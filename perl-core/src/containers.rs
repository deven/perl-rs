//! `PerlArray` and `PerlHash` — the containers (§2.2.1) — with their Arc-backed shared identities `ArrayRef` and
//! `HashRef`.  The module name is temporary in the same sense as `payload.rs`.
//!
//! Container-verified semantics encoded here:
//!
//! - Arrays have holes below their length (`$a[5] = "x"` on empty: length 6, indices 0–4 nonexistent); hashes have no
//!   slot wrapper — nonexistence is absence of the map entry.
//! - Lvalue access vivifies the undef element (`\$a[3]` on empty: length 4, existing undef element); read access never
//!   creates.  This `get`/`ensure` split is the autovivification-option mechanism (§2.2.1).
//! - Hash keys are laundered at storage (§2.6.2): a tainted key stores clean — `keys` returns clean strings.
//! - `each`: safe to delete the current item (all remaining keys are still visited); other concurrent mutation may skip
//!   entries (perl documents this as unspecified); `keys`/`values` reset the iterator; an exhausted iterator returns
//!   `None` once, then restarts.  Order is stable without mutation and `keys`/`values` correspond.
//!
//! The map is an `IndexMap` (ruled §21.1): the `each` cursor is a plain index — deletes use `swap_remove` (O(1), order
//! perturbation being within perl's unspecified-order contract) with the cursor adjustment `if idx < cursor { cursor -=
//! 1 }`, which makes delete-current *exact*: the moved tail entry lands at the decremented cursor and is yielded next,
//! so every remaining key is visited.

use indexmap::IndexMap;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::fmt;

use crate::heap::{HeapArc, release_value};
use crate::scalar::ScalarError;
use crate::string::PerlString;
use crate::value::{ArraySlot, Value};

// ── PerlArray (§2.2.1) ────────────────────────────────────────────
/// `Vec<ArraySlot>` plus array-level state.  `None` = a hole (nonexistent element); `Some(Undef)` = an existing element
/// holding undef.
#[derive(Default)]
pub struct PerlArray {
    slots: Vec<ArraySlot>,

    /// The dynamic readonly flag (`Internals::SvREADONLY` on the container), checked per mutation.
    readonly: bool,
}

impl Drop for PerlArray {
    /// Iterative teardown (§2.4.9): drain elements through the release worklist rather than recursing through the
    /// `Vec`'s drop glue.  Destruction is not perl-visible mutation, so the readonly flag is deliberately not consulted.
    fn drop(&mut self) {
        for v in self.slots.drain(..).flatten() {
            release_value(v);
        }
    }
}

impl PerlArray {
    pub fn new() -> PerlArray {
        PerlArray::default()
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    fn check_writable(&self) -> Result<(), ScalarError> {
        if self.readonly { Err(ScalarError::ReadOnly) } else { Ok(()) }
    }

    /// Read access: never creates.  `None` for holes and out-of-range indices alike (the exists/defined distinction
    /// goes through [`PerlArray::exists`]).
    pub fn get(&self, index: usize) -> Option<&Value> {
        self.slots.get(index).and_then(Option::as_ref)
    }

    /// `exists $a[$i]`: present and occupied.
    pub fn exists(&self, index: usize) -> bool {
        self.slots.get(index).is_some_and(Option::is_some)
    }

    /// `$a[$i] = $v`: extends with holes below (container-verified: `$a[5] = "x"` on empty gives length 6 with indices
    /// 0–4 nonexistent).
    pub fn set(&mut self, index: usize, value: Value) -> Result<(), ScalarError> {
        self.check_writable()?;
        if index >= self.slots.len() {
            self.slots.resize_with(index + 1, || None);
        }

        self.slots[index] = Some(value);

        Ok(())
    }

    /// Lvalue access: vivify the undef element and hand back the slot's value (container-verified: `\$a[3]` on empty
    /// yields length 4 with an existing undef element).  The `get`/`ensure` split is the autovivification-option
    /// mechanism (§2.2.1).
    pub fn ensure_element(&mut self, index: usize) -> Result<&mut Value, ScalarError> {
        self.check_writable()?;
        if index >= self.slots.len() {
            self.slots.resize_with(index + 1, || None);
        }

        Ok(self.slots[index].get_or_insert_with(Value::default))
    }

    /// `delete $a[$i]` (§2.2.1, container-verified): returns the deleted value (undef for holes and out-of-range
    /// indices, which are left untouched); deleting the last element truncates through trailing holes.
    pub fn delete(&mut self, index: usize) -> Result<Value, ScalarError> {
        self.check_writable()?;
        if index >= self.slots.len() {
            return Ok(Value::default());
        }

        let deleted = self.slots[index].take().unwrap_or_default();

        if index == self.slots.len() - 1 {
            while matches!(self.slots.last(), Some(None)) {
                self.slots.pop();
            }
        }

        Ok(deleted)
    }

    /// `push @a, $v` (single element; list forms loop at the ops layer).
    pub fn push_value(&mut self, value: Value) -> Result<(), ScalarError> {
        self.check_writable()?;
        self.slots.push(Some(value));

        Ok(())
    }

    /// `pop @a`: undef for an empty array or a trailing hole (indistinguishable in perl); shortens by one.
    pub fn pop_value(&mut self) -> Result<Value, ScalarError> {
        self.check_writable()?;

        Ok(self.slots.pop().flatten().unwrap_or_default())
    }

    /// `shift @a`.
    pub fn shift_value(&mut self) -> Result<Value, ScalarError> {
        self.check_writable()?;
        if self.slots.is_empty() {
            return Ok(Value::default());
        }

        Ok(self.slots.remove(0).unwrap_or_default())
    }

    /// `unshift @a, $v` (single element).
    pub fn unshift_value(&mut self, value: Value) -> Result<(), ScalarError> {
        self.check_writable()?;
        self.slots.insert(0, Some(value));

        Ok(())
    }

    /// `@a = ()`.
    pub fn clear(&mut self) -> Result<(), ScalarError> {
        self.check_writable()?;
        self.slots.clear();

        Ok(())
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }

    pub fn is_readonly(&self) -> bool {
        self.readonly
    }

    /// The graph traversal hook (§2.4.6 demolition, §2.4.11 cycle detection): existing elements only.
    #[cfg_attr(not(test), expect(dead_code, reason = "consumers are §2.4.6 demolition and the on-demand cycle detector"))]
    pub(crate) fn values_iter(&self) -> impl Iterator<Item = &Value> {
        self.slots.iter().filter_map(Option::as_ref)
    }
}

// ── PerlHash (§2.2.1) ─────────────────────────────────────────────
/// `IndexMap<PerlString, Value>` plus iterator state.  Keys are laundered at storage (§2.6.2); the stored key is kept
/// on re-store (equal keys: the first-stored spelling wins, matching map semantics).
#[derive(Default)]
pub struct PerlHash {
    map: IndexMap<PerlString, Value>,

    /// The `each` cursor: the next index to yield.
    cursor: usize,
    readonly: bool,
}

impl Drop for PerlHash {
    /// Iterative teardown (§2.4.9): values route through the release worklist; keys are strings and cannot recurse.
    fn drop(&mut self) {
        for (_key, v) in self.map.drain(..) {
            release_value(v);
        }
    }
}

impl PerlHash {
    pub fn new() -> PerlHash {
        PerlHash::default()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn check_writable(&self) -> Result<(), ScalarError> {
        if self.readonly { Err(ScalarError::ReadOnly) } else { Ok(()) }
    }

    fn launder(mut key: PerlString) -> PerlString {
        if key.is_tainted() {
            key.untaint_for_sanctioned_path();
        }

        key
    }

    /// `$h{$k} = $v`, laundering the key (§2.6.2: hash-key canonicalization is a sanctioned untaint path —
    /// container-verified: a tainted key stores clean).
    pub fn store(&mut self, key: PerlString, value: Value) -> Result<(), ScalarError> {
        self.check_writable()?;
        self.map.insert(PerlHash::launder(key), value);

        Ok(())
    }

    /// Read access: never creates.
    pub fn get(&self, key: &PerlString) -> Option<&Value> {
        self.map.get(key)
    }

    /// `exists $h{$k}`: absence of the entry is nonexistence (§2.2.1 — no slot wrapper).
    pub fn exists(&self, key: &PerlString) -> bool {
        self.map.contains_key(key)
    }

    /// Lvalue access: vivify the undef entry (container-verified: `\$h{k}` creates an existing undef entry).  The
    /// `get`/`ensure` split is the autovivification-option mechanism (§2.2.1).
    pub fn entry_or_undef(&mut self, key: PerlString) -> Result<&mut Value, ScalarError> {
        self.check_writable()?;

        Ok(self.map.entry(PerlHash::launder(key)).or_default())
    }

    /// `delete $h{$k}`, returning the value (undef for absent keys).  `swap_remove` keeps delete O(1); the cursor
    /// adjustment makes delete-current exact (module header).
    pub fn delete(&mut self, key: &PerlString) -> Result<Value, ScalarError> {
        self.check_writable()?;
        let Some(index) = self.map.get_index_of(key) else {
            return Ok(Value::default());
        };
        let (_, value) = self.map.swap_remove_index(index).unwrap_or_else(|| (PerlString::empty(), Value::default()));

        if index < self.cursor {
            self.cursor -= 1;
        }

        Ok(value)
    }

    /// `each %h`: yield the next pair, or `None` once at exhaustion (then restart — container-verified).
    pub fn each(&mut self) -> Option<(PerlString, Value)> {
        match self.map.get_index(self.cursor) {
            Some((k, v)) => {
                self.cursor += 1;
                Some((k.clone(), v.clone()))
            }
            None => {
                self.cursor = 0;
                None
            }
        }
    }

    /// `keys %h`: resets the iterator (container-verified).
    pub fn keys(&mut self) -> Vec<PerlString> {
        self.cursor = 0;
        self.map.keys().cloned().collect()
    }

    /// `values %h`: resets the iterator; corresponds to `keys` order (container-verified).
    pub fn values(&mut self) -> Vec<Value> {
        self.cursor = 0;
        self.map.values().cloned().collect()
    }

    /// `%h = ()`.
    pub fn clear(&mut self) -> Result<(), ScalarError> {
        self.check_writable()?;
        self.map.clear();
        self.cursor = 0;

        Ok(())
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }

    pub fn is_readonly(&self) -> bool {
        self.readonly
    }

    /// The graph traversal hook (§2.4.6 demolition, §2.4.11 cycle detection).
    #[cfg_attr(not(test), expect(dead_code, reason = "consumers are §2.4.6 demolition and the on-demand cycle detector"))]
    pub(crate) fn values_iter(&self) -> impl Iterator<Item = &Value> {
        self.map.values()
    }
}

// ── The shared identities (§2.2.1: Arc-backed) ────────────────────
macro_rules! container_handle {
    ($handle:ident, $container:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone)]
        pub struct $handle(HeapArc<RwLock<$container>>);

        impl $handle {
            pub fn new(container: $container) -> $handle {
                $handle(HeapArc::new(RwLock::new(container)))
            }

            /// Reference identity: what `==` on Perl references compares.
            pub fn ptr_eq(a: &$handle, b: &$handle) -> bool {
                HeapArc::ptr_eq(&a.0, &b.0)
            }

            /// The address perl exposes when the reference is numified or stringified.
            pub fn addr(&self) -> usize {
                HeapArc::as_ptr(&self.0) as usize
            }

            pub fn read(&self) -> RwLockReadGuard<'_, $container> {
                self.0.read()
            }

            /// Container mutation goes through the lock; the dynamic readonly flag is checked per operation inside the
            /// container (matching the cell model: acquiring the guard stays legal).
            pub fn write(&self) -> RwLockWriteGuard<'_, $container> {
                self.0.write()
            }
        }

        impl fmt::Debug for $handle {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($handle), "(0x{:x})"), self.addr())
            }
        }
    };
}

container_handle!(ArrayRef, PerlArray, "The Arc-backed shared array identity (§2.2.1).");
container_handle!(HashRef, PerlHash, "The Arc-backed shared hash identity (§2.2.1).");

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/containers_tests.rs"]
mod tests;
