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

use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use indexmap::IndexMap;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::BuildHasher;

#[cfg(feature = "imbl")]
use imbl;

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

// ── PerlHash (§2.2.1, §2.2.13) ────────────────────────────────────
/// The dual-engine hash (§2.2.13): a bucket engine on `hashbrown::HashTable` by default, and the insertion-ordered
/// `IndexMap` engine on explicit request, fixed at construction.  Keys are laundered at storage (§2.6.2); the stored
/// key is kept on re-store (equal keys: the first-stored spelling wins) under either engine.
pub struct PerlHash {
    engine: HashEngine,
    readonly: bool,
}

/// The engine, chosen at construction and never morphed (§2.2.13).
enum HashEngine {
    /// The default: SwissTable buckets, per-hash SipHash keys, and the `each` cursor as a bucket index.
    Buckets { table: HashTable<(PerlString, Value)>, hasher: RandomState, cursor: usize },

    /// The explicitly requested insertion-ordered mode (§2.2.10): the `each` cursor is an entry index.
    Ordered { map: IndexMap<PerlString, Value>, cursor: usize },

    /// The feature-gated immutable mode (§2.2.13): a persistent HAMT; the `each` cursor is an owning iterator over an
    /// O(1) snapshot, yielding with live revalidation.
    #[cfg(feature = "imbl")]
    Immutable { map: ImblMap, iter: Option<ImblIter> },
}

#[cfg(feature = "imbl")]
type ImblMap = imbl::HashMap<PerlString, Value>;

#[cfg(feature = "imbl")]
type ImblIter = <ImblMap as IntoIterator>::IntoIter;

/// Retire a parked snapshot iterator (§2.2.13): its remaining values route through the release worklist — a co-owner's
/// release nets zero on shared values; the final owner's moves them out.
#[cfg(feature = "imbl")]
fn retire_iter(iter: &mut Option<ImblIter>) {
    if let Some(rest) = iter.take() {
        for (_key, v) in rest {
            release_value(v);
        }
    }
}

impl Default for PerlHash {
    fn default() -> PerlHash {
        PerlHash::new()
    }
}

impl Drop for PerlHash {
    /// Iterative teardown (§2.4.9): values route through the release worklist; keys are strings and cannot recurse.
    fn drop(&mut self) {
        match &mut self.engine {
            HashEngine::Buckets { table, .. } => {
                for (_key, v) in table.drain() {
                    release_value(v);
                }
            }
            HashEngine::Ordered { map, .. } => {
                for (_key, v) in map.drain(..) {
                    release_value(v);
                }
            }

            #[cfg(feature = "imbl")]
            HashEngine::Immutable { map, iter } => {
                retire_iter(iter);
                for (_key, v) in std::mem::take(map) {
                    release_value(v);
                }
            }
        }
    }
}

impl PerlHash {
    /// The default engine (§2.2.13): buckets, per-hash random iteration order.
    pub fn new() -> PerlHash {
        PerlHash { engine: HashEngine::Buckets { table: HashTable::new(), hasher: RandomState::new(), cursor: 0 }, readonly: false }
    }

    /// The insertion-ordered mode (§2.2.13), on explicit request only; the perl-visible request surface is the runtime
    /// design's.
    pub fn insertion_ordered() -> PerlHash {
        PerlHash { engine: HashEngine::Ordered { map: IndexMap::new(), cursor: 0 }, readonly: false }
    }

    /// The immutable mode (§2.2.13), on explicit request only: a persistent HAMT with O(1) [`PerlHash::snapshot`].
    #[cfg(feature = "imbl")]
    pub fn immutable() -> PerlHash {
        PerlHash { engine: HashEngine::Immutable { map: ImblMap::new(), iter: None }, readonly: false }
    }

    /// An O(1) detached, diverging copy of an immutable-engine hash (§2.2.13), with a fresh cursor and the readonly
    /// flag cleared; the other engines answer [`ScalarError::SnapshotUnsupported`], their copies being O(n).
    pub fn snapshot(&self) -> Result<PerlHash, ScalarError> {
        match &self.engine {
            #[cfg(feature = "imbl")]
            HashEngine::Immutable { map, .. } => Ok(PerlHash { engine: HashEngine::Immutable { map: map.clone(), iter: None }, readonly: false }),
            _ => Err(ScalarError::SnapshotUnsupported),
        }
    }

    pub fn len(&self) -> usize {
        match &self.engine {
            HashEngine::Buckets { table, .. } => table.len(),
            HashEngine::Ordered { map, .. } => map.len(),

            #[cfg(feature = "imbl")]
            HashEngine::Immutable { map, .. } => map.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
    ///
    /// The cursor discipline (§2.2.13): an existing key updates in place and leaves the cursor alone — value updates
    /// during iteration are contract-specified safe — while a new key resets it, answering any rehash with a restart.
    pub fn store(&mut self, key: PerlString, value: Value) -> Result<(), ScalarError> {
        self.check_writable()?;
        let key = PerlHash::launder(key);
        match &mut self.engine {
            HashEngine::Buckets { table, hasher, cursor } => {
                let hash = hasher.hash_one(&key);
                if let Some(pair) = table.find_mut(hash, |(stored, _)| stored == &key) {
                    pair.1 = value;
                } else {
                    *cursor = 0;
                    table.insert_unique(hash, (key, value), |(stored, _)| hasher.hash_one(stored));
                }
            }
            HashEngine::Ordered { map, .. } => {
                map.insert(key, value);
            }

            // No reset in any case (§2.2.13): the snapshot walk is rehash-immune, and the stored key spelling of an
            // existing entry is preserved by the update-in-place branch.
            #[cfg(feature = "imbl")]
            HashEngine::Immutable { map, .. } => {
                if let Some(slot) = map.get_mut(&key) {
                    *slot = value;
                } else {
                    map.insert(key, value);
                }
            }
        }
        Ok(())
    }

    /// Read access: never creates.
    pub fn get(&self, key: &PerlString) -> Option<&Value> {
        match &self.engine {
            HashEngine::Buckets { table, hasher, .. } => table.find(hasher.hash_one(key), |(stored, _)| stored == key).map(|(_, value)| value),
            HashEngine::Ordered { map, .. } => map.get(key),

            #[cfg(feature = "imbl")]
            HashEngine::Immutable { map, .. } => map.get(key),
        }
    }

    /// `exists $h{$k}`: absence of the entry is nonexistence (§2.2.1 — no slot wrapper).
    pub fn exists(&self, key: &PerlString) -> bool {
        self.get(key).is_some()
    }

    /// Lvalue access: vivify the undef entry (container-verified: `\$h{k}` creates an existing undef entry).  The
    /// `get`/`ensure` split is the autovivification-option mechanism (§2.2.1).  Vivification of an absent key is a
    /// new-key insertion and resets the cursor (§2.2.13); an existing key leaves it alone.
    pub fn entry_or_undef(&mut self, key: PerlString) -> Result<&mut Value, ScalarError> {
        self.check_writable()?;
        let key = PerlHash::launder(key);
        match &mut self.engine {
            HashEngine::Buckets { table, hasher, cursor } => {
                let hash = hasher.hash_one(&key);
                let entry = match table.entry(hash, |(stored, _)| stored == &key, |(stored, _)| hasher.hash_one(stored)) {
                    Entry::Occupied(occupied) => occupied,
                    Entry::Vacant(vacant) => {
                        *cursor = 0;
                        vacant.insert((key, Value::default()))
                    }
                };
                Ok(&mut entry.into_mut().1)
            }
            HashEngine::Ordered { map, .. } => Ok(map.entry(key).or_default()),

            #[cfg(feature = "imbl")]
            HashEngine::Immutable { map, .. } => Ok(map.entry(key).or_insert_with(Value::default)),
        }
    }

    /// `delete $h{$k}`, returning the value (undef for absent keys).  The cursor is never touched (§2.2.13): erasure
    /// moves nothing, so every deletion is exact — delete-current is behind the cursor already, and a deleted unvisited
    /// entry is a slot the walk will skip.  (Ordered engine: `swap_remove` keeps delete O(1); the cursor adjustment
    /// makes delete-current exact, module header.)
    pub fn delete(&mut self, key: &PerlString) -> Result<Value, ScalarError> {
        self.check_writable()?;
        match &mut self.engine {
            HashEngine::Buckets { table, hasher, .. } => match table.find_entry(hasher.hash_one(key), |(stored, _)| stored == key) {
                Ok(occupied) => {
                    let ((_key, value), _) = occupied.remove();
                    Ok(value)
                }
                Err(_) => Ok(Value::default()),
            },
            HashEngine::Ordered { map, cursor } => {
                let Some(index) = map.get_index_of(key) else {
                    return Ok(Value::default());
                };
                let (_, value) = map.swap_remove_index(index).unwrap_or_else(|| (PerlString::empty(), Value::default()));
                if index < *cursor {
                    *cursor -= 1;
                }
                Ok(value)
            }

            // The parked walk revalidates live, so the sharing-safe removal needs no cursor bookkeeping either.
            #[cfg(feature = "imbl")]
            HashEngine::Immutable { map, .. } => Ok(map.remove(key).unwrap_or_default()),
        }
    }

    /// `each %h`: yield the next pair, or `None` once at exhaustion (then restart — container-verified).  The bucket
    /// engine walks `get_bucket` from the cursor to the next occupied slot (§2.2.13).
    pub fn each(&mut self) -> Option<(PerlString, Value)> {
        match &mut self.engine {
            HashEngine::Buckets { table, cursor, .. } => {
                let end = table.num_buckets();
                while *cursor < end {
                    let bucket = *cursor;
                    *cursor += 1;
                    if let Some((key, value)) = table.get_bucket(bucket) {
                        return Some((key.clone(), value.clone()));
                    }
                }
                *cursor = 0;
                None
            }
            HashEngine::Ordered { map, cursor } => match map.get_index(*cursor) {
                Some((k, v)) => {
                    *cursor += 1;
                    Some((k.clone(), v.clone()))
                }
                None => {
                    *cursor = 0;
                    None
                }
            },

            // §2.2.13: walk an O(1) snapshot through an owning iterator, revalidating live — deleted keys are skipped,
            // values are read live (specified-visible updates), inserted keys wait for the restart.
            #[cfg(feature = "imbl")]
            HashEngine::Immutable { map, iter } => {
                let walk = iter.get_or_insert_with(|| map.clone().into_iter());
                for (key, _snapshot_value) in walk {
                    if let Some(live) = map.get(&key) {
                        return Some((key, live.clone()));
                    }
                }
                *iter = None;
                None
            }
        }
    }

    /// `keys %h`: resets the iterator (container-verified); shares `each`'s scan order (§2.2.13).
    pub fn keys(&mut self) -> Vec<PerlString> {
        match &mut self.engine {
            HashEngine::Buckets { table, cursor, .. } => {
                *cursor = 0;
                (0..table.num_buckets()).filter_map(|bucket| table.get_bucket(bucket)).map(|(k, _)| k.clone()).collect()
            }
            HashEngine::Ordered { map, cursor } => {
                *cursor = 0;
                map.keys().cloned().collect()
            }

            #[cfg(feature = "imbl")]
            HashEngine::Immutable { map, iter } => {
                retire_iter(iter);
                map.keys().cloned().collect()
            }
        }
    }

    /// `values %h`: resets the iterator; corresponds to `keys` order (container-verified).
    pub fn values(&mut self) -> Vec<Value> {
        match &mut self.engine {
            HashEngine::Buckets { table, cursor, .. } => {
                *cursor = 0;
                (0..table.num_buckets()).filter_map(|bucket| table.get_bucket(bucket)).map(|(_, v)| v.clone()).collect()
            }
            HashEngine::Ordered { map, cursor } => {
                *cursor = 0;
                map.values().cloned().collect()
            }

            #[cfg(feature = "imbl")]
            HashEngine::Immutable { map, iter } => {
                retire_iter(iter);
                map.values().cloned().collect()
            }
        }
    }

    /// `%h = ()`.
    pub fn clear(&mut self) -> Result<(), ScalarError> {
        self.check_writable()?;
        match &mut self.engine {
            HashEngine::Buckets { table, cursor, .. } => {
                for (_key, v) in table.drain() {
                    release_value(v);
                }
                *cursor = 0;
            }
            HashEngine::Ordered { map, cursor } => {
                for (_key, v) in map.drain(..) {
                    release_value(v);
                }
                *cursor = 0;
            }

            #[cfg(feature = "imbl")]
            HashEngine::Immutable { map, iter } => {
                retire_iter(iter);
                for (_key, v) in std::mem::take(map) {
                    release_value(v);
                }
            }
        }
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
    pub(crate) fn values_iter(&self) -> HashValuesIter<'_> {
        match &self.engine {
            HashEngine::Buckets { table, .. } => HashValuesIter::Buckets { table, next: 0 },
            HashEngine::Ordered { map, .. } => HashValuesIter::Ordered(map.values()),

            #[cfg(feature = "imbl")]
            HashEngine::Immutable { map, .. } => HashValuesIter::Immutable(map.values()),
        }
    }
}

/// The engine-dispatched values walk behind [`PerlHash::values_iter`]: a hand-rolled two-arm iterator, keeping the cold
/// traversal hook vtable-free.
pub(crate) enum HashValuesIter<'a> {
    Buckets {
        table: &'a HashTable<(PerlString, Value)>,
        next: usize,
    },
    Ordered(indexmap::map::Values<'a, PerlString, Value>),

    #[cfg(feature = "imbl")]
    Immutable(imbl::hashmap::Values<'a, PerlString, Value, imbl::shared_ptr::DefaultSharedPtr>),
}

impl<'a> Iterator for HashValuesIter<'a> {
    type Item = &'a Value;

    fn next(&mut self) -> Option<&'a Value> {
        match self {
            HashValuesIter::Buckets { table, next } => {
                let end = table.num_buckets();
                while *next < end {
                    let bucket = *next;
                    *next += 1;
                    if let Some((_, value)) = table.get_bucket(bucket) {
                        return Some(value);
                    }
                }
                None
            }
            HashValuesIter::Ordered(values) => values.next(),

            #[cfg(feature = "imbl")]
            HashValuesIter::Immutable(values) => values.next(),
        }
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
