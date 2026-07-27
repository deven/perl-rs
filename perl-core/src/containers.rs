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

use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::cell::ScalarError;
use crate::payload::{ArraySlot, Value};
use crate::string::PerlString;

// ── PerlArray (§2.2.1) ────────────────────────────────────────────
/// `Vec<ArraySlot>` plus array-level state.  `None` = a hole (nonexistent element); `Some(Undef)` = an existing element
/// holding undef.
#[derive(Default)]
pub struct PerlArray {
    slots: Vec<ArraySlot>,

    /// The dynamic readonly flag (`Internals::SvREADONLY` on the container), checked per mutation.
    readonly: bool,
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
        pub struct $handle(Arc<RwLock<$container>>);

        impl $handle {
            pub fn new(container: $container) -> $handle {
                $handle(Arc::new(RwLock::new(container)))
            }

            /// Reference identity: what `==` on Perl references compares.
            pub fn ptr_eq(a: &$handle, b: &$handle) -> bool {
                Arc::ptr_eq(&a.0, &b.0)
            }

            /// The address perl exposes when the reference is numified or stringified.
            pub fn addr(&self) -> usize {
                Arc::as_ptr(&self.0) as usize
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

        impl std::fmt::Debug for $handle {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!(stringify!($handle), "(0x{:x})"), self.addr())
            }
        }
    };
}

container_handle!(ArrayRef, PerlArray, "The Arc-backed shared array identity (§2.2.1).");
container_handle!(HashRef, PerlHash, "The Arc-backed shared hash identity (§2.2.1).");

// ── Tests ─────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::{ScalarPayload, Tainted};

    fn int(n: i64) -> Value {
        Value::Int(n, Tainted::CLEAN)
    }

    fn key(text: &str) -> PerlString {
        text.parse().unwrap()
    }

    // ── Arrays ────────────────────────────────────────────────────
    #[test]
    fn array_holes_below_length() {
        // Container-verified: $a[5] = "x" on empty — length 6, 0–4 nonexistent, 5 exists.
        let mut a = PerlArray::new();
        a.set(5, int(1)).unwrap();
        assert_eq!(a.len(), 6);
        assert!(!a.exists(0));
        assert!(a.exists(5));
        assert!(a.get(0).is_none());
        assert_eq!(a.get(5).unwrap().to_int(), 1);
        assert!(a.get(99).is_none());
    }

    #[test]
    fn array_ensure_element_vivifies_undef() {
        // Container-verified: \$a[3] on empty — length 4, element exists, undef.
        let mut a = PerlArray::new();
        let slot = a.ensure_element(3).unwrap();
        assert!(matches!(slot, Value::Undef(_)));
        assert_eq!(a.len(), 4);
        assert!(a.exists(3));
        assert!(!a.exists(0), "the get/ensure split: indices below stay holes");

        // Write-through: take a ref of the vivified slot, assign, observe (the \$a[3] round trip).
        let r = Value::take_ref(a.ensure_element(3).unwrap());
        r.deref_scalar().unwrap().write().unwrap().assign(ScalarPayload::Int(5, Tainted::CLEAN)).unwrap();
        assert_eq!(a.get(3).unwrap().to_int(), 5, "$$r = 5 lands in the array");
    }

    #[test]
    fn array_delete_rules() {
        // Migrated §2.2.1 pins: delete-mid holes, delete-last truncates through trailing holes.
        let mut a = PerlArray::new();
        for i in 0..3 {
            a.set(i, int(i as i64 + 1)).unwrap();
        }

        assert_eq!(a.delete(1).unwrap().to_int(), 2);
        assert_eq!(a.len(), 3, "delete-mid leaves a hole, length unchanged");
        assert!(!a.exists(1));
        assert_eq!(a.delete(2).unwrap().to_int(), 3);
        assert_eq!(a.len(), 1, "delete-last truncates through trailing holes");
        assert!(matches!(a.delete(9).unwrap(), Value::Undef(_)));
        assert_eq!(a.len(), 1, "delete beyond the end touches nothing");
    }

    #[test]
    fn array_push_pop_shift_unshift() {
        let mut a = PerlArray::new();
        a.push_value(int(1)).unwrap();
        a.push_value(int(2)).unwrap();
        a.unshift_value(int(0)).unwrap();
        assert_eq!(a.len(), 3);
        assert_eq!(a.shift_value().unwrap().to_int(), 0);
        assert_eq!(a.pop_value().unwrap().to_int(), 2);
        assert_eq!(a.pop_value().unwrap().to_int(), 1);
        assert!(matches!(a.pop_value().unwrap(), Value::Undef(_)), "pop on empty is undef");

        // Pop after a sparse set: the value comes off; the holes remain (length 5, all holes).
        let mut sparse = PerlArray::new();
        sparse.set(5, int(9)).unwrap();
        assert_eq!(sparse.pop_value().unwrap().to_int(), 9);
        assert_eq!(sparse.len(), 5);
        assert!(!sparse.exists(0));
        assert!(matches!(sparse.pop_value().unwrap(), Value::Undef(_)), "popping a hole is undef");
        assert_eq!(sparse.len(), 4);
    }

    #[test]
    fn array_readonly() {
        let mut a = PerlArray::new();
        a.set(0, int(1)).unwrap();
        a.set_readonly(true);
        assert_eq!(a.set(1, int(2)), Err(ScalarError::ReadOnly));
        assert_eq!(a.delete(0).map(|_| ()), Err(ScalarError::ReadOnly));
        assert_eq!(a.push_value(int(2)), Err(ScalarError::ReadOnly));
        assert_eq!(a.ensure_element(3).map(|_| ()), Err(ScalarError::ReadOnly));
        assert_eq!(a.clear(), Err(ScalarError::ReadOnly));
        assert_eq!(a.get(0).unwrap().to_int(), 1, "reads stay legal");
        a.set_readonly(false);
        a.set(1, int(2)).unwrap();
    }

    // ── Hashes ────────────────────────────────────────────────────
    #[test]
    fn hash_store_get_exists_delete() {
        let mut h = PerlHash::new();
        h.store(key("a"), int(1)).unwrap();
        h.store(key("b"), int(2)).unwrap();
        assert_eq!(h.len(), 2);
        assert!(h.exists(&key("a")));
        assert!(!h.exists(&key("z")));
        assert_eq!(h.get(&key("b")).unwrap().to_int(), 2);
        assert_eq!(h.delete(&key("b")).unwrap().to_int(), 2, "delete returns the value (verified)");
        assert!(!h.exists(&key("b")));
        assert!(matches!(h.delete(&key("z")).unwrap(), Value::Undef(_)));
        h.store(key("a"), int(9)).unwrap();
        assert_eq!(h.get(&key("a")).unwrap().to_int(), 9, "re-store replaces the value");
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn hash_keys_are_laundered_at_storage() {
        // Container-verified under -T: a tainted key stores clean; keys returns clean strings.
        let mut tainted_key = key("secret");
        tainted_key.taint();
        assert!(tainted_key.is_tainted());

        let mut h = PerlHash::new();
        h.store(tainted_key.clone(), int(1)).unwrap();
        let stored = h.keys();
        assert_eq!(stored.len(), 1);
        assert!(!stored[0].is_tainted(), "the §2.6.2 sanctioned laundering path");

        // Same through the lvalue path.
        let mut h2 = PerlHash::new();
        let _ = h2.entry_or_undef(tainted_key).unwrap();
        assert!(!h2.keys()[0].is_tainted());
    }

    #[test]
    fn hash_entry_or_undef_vivifies() {
        // Container-verified: \$h{k} — the entry exists, undef.
        let mut h = PerlHash::new();
        let slot = h.entry_or_undef(key("k")).unwrap();
        assert!(matches!(slot, Value::Undef(_)));
        assert!(h.exists(&key("k")));

        let r = Value::take_ref(h.entry_or_undef(key("k")).unwrap());
        r.deref_scalar().unwrap().write().unwrap().assign(ScalarPayload::Int(7, Tainted::CLEAN)).unwrap();
        assert_eq!(h.get(&key("k")).unwrap().to_int(), 7);
    }

    #[test]
    fn each_visits_all_when_deleting_current() {
        // Container-verified: deleting the current item mid-each still visits all 4 keys.
        let mut h = PerlHash::new();
        for k in ["a", "b", "c", "d"] {
            h.store(key(k), int(1)).unwrap();
        }

        let mut visited = Vec::new();
        while let Some((k, _)) = h.each() {
            let is_b = k.as_bytes() == b"b";
            visited.push(k.clone());
            if is_b {
                h.delete(&k).unwrap();
            }
        }

        assert_eq!(visited.len(), 4, "all keys visited despite delete-current (verified)");
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn each_exhausts_restarts_and_keys_resets() {
        let mut h = PerlHash::new();
        h.store(key("x"), int(1)).unwrap();
        h.store(key("y"), int(2)).unwrap();

        // Exhaust: two yields, one None, then a restart (container-verified).
        assert!(h.each().is_some());
        assert!(h.each().is_some());
        assert!(h.each().is_none());
        assert!(h.each().is_some(), "the iterator restarts after exhaustion");

        // keys() resets mid-iteration (container-verified).
        let mut g = PerlHash::new();
        g.store(key("x"), int(1)).unwrap();
        g.store(key("y"), int(2)).unwrap();
        let _ = g.each();
        let _ = g.keys();
        let mut count = 0;
        while g.each().is_some() {
            count += 1;
        }

        assert_eq!(count, 2, "full pass after the reset");
    }

    #[test]
    fn keys_values_stable_and_corresponding() {
        // Container-verified: stable without mutation; keys/values correspond.
        let mut h = PerlHash::new();
        for (i, k) in ["a", "b", "c"].iter().enumerate() {
            h.store(key(k), int(i as i64)).unwrap();
        }

        let k1 = h.keys();
        let k2 = h.keys();
        assert_eq!(k1.iter().map(|k| k.as_bytes().to_vec()).collect::<Vec<_>>(), k2.iter().map(|k| k.as_bytes().to_vec()).collect::<Vec<_>>());
        let vals = h.values();
        for (k, v) in k1.iter().zip(vals.iter()) {
            assert_eq!(h.get(k).unwrap().to_int(), v.to_int());
        }
    }

    #[test]
    fn hash_readonly() {
        let mut h = PerlHash::new();
        h.store(key("a"), int(1)).unwrap();
        h.set_readonly(true);
        assert_eq!(h.store(key("b"), int(2)), Err(ScalarError::ReadOnly));
        assert_eq!(h.delete(&key("a")).map(|_| ()), Err(ScalarError::ReadOnly));
        assert_eq!(h.entry_or_undef(key("c")).map(|_| ()), Err(ScalarError::ReadOnly));
        assert_eq!(h.clear(), Err(ScalarError::ReadOnly));
        assert_eq!(h.get(&key("a")).unwrap().to_int(), 1);
        assert_eq!(h.keys().len(), 1, "reads and iteration stay legal");
        h.set_readonly(false);
        h.store(key("b"), int(2)).unwrap();
    }

    // ── Handles ───────────────────────────────────────────────────
    #[test]
    fn handle_identity_and_traversal() {
        let a = ArrayRef::new(PerlArray::new());
        let a2 = a.clone();
        assert!(ArrayRef::ptr_eq(&a, &a2));
        let b = ArrayRef::new(PerlArray::new());
        assert!(!ArrayRef::ptr_eq(&a, &b));
        assert_ne!(a.addr(), 0);

        a.write().push_value(int(1)).unwrap();
        a.write().push_value(int(2)).unwrap();
        assert_eq!(a2.read().len(), 2, "writes visible through the clone: shared identity");
        assert_eq!(a.read().values_iter().map(Value::to_int).sum::<i64>(), 3, "collector hook");

        let h = HashRef::new(PerlHash::new());
        h.write().store(key("k"), int(5)).unwrap();
        assert_eq!(h.read().values_iter().count(), 1);
        assert!(format!("{h:?}").starts_with("HashRef(0x"));
    }
}
