//! `Array` and `Hash` — the containers (§2.2.1) — with their Arc-backed shared identities `ArrayRef` and `HashRef`.
//! The module name is temporary in the same sense as `payload.rs`.
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
use std::alloc::Layout;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::BuildHasher;
use std::ptr::NonNull;

#[cfg(feature = "imbl")]
use imbl;

use crate::alloc_backend;
use crate::cow_buffer::AllocError;
use crate::heap::{HeapArc, release_value};
use crate::scalar::ScalarError;
use crate::string::PString;
use crate::value::{ArraySlot, Value};

// ── Array (§2.2.1, §2.2.12) ───────────────────────────────────
/// The front-gap slot engine (§2.2.12), shaped on perl's AV: an allocation whose live window floats behind a gap, so
/// `shift` is an O(1) window slide and `unshift` reclaims the gap before any element moves.  `None` = a hole
/// (nonexistent element); `Some(Undef)` = an existing element holding undef.
///
/// The header is the ruled 24 bytes: a manual two-arm tag in the flags byte, because a Rust enum would spend a
/// discriminant byte the §2.4.3 budget does not have.  Small arrays keep `ptr` as the buffer base with `u32` geometry;
/// past `u32` the geometry spills to a boxed wide header and `ptr` holds that box (`FLAG_LARGE`), the `Heap32`/`Heap`
/// philosophy applied to arrays.  Bits 8..32 of `stash_flags` are the reserved bless stash (u24).
pub struct Array {
    /// Small: the buffer base (dangling when unallocated).  Large: the boxed [`Geometry`].
    ptr: NonNull<ArraySlot>,
    start: u32,
    len: u32,
    cap: u32,
    stash_flags: u32,
}

const _: () = assert!(size_of::<Array>() == 24);

/// The dynamic readonly flag's bit.
const FLAG_READONLY: u32 = 1;

/// The arm tag: set when `ptr` holds the boxed wide geometry.
const FLAG_LARGE: u32 = 2;

// SAFETY: the raw pointer is exclusively owned storage of `Send + Sync` slots; sharing is external (§2.2.1: the
// handle's lock).
unsafe impl Send for Array {}

// SAFETY: as above — `&Array` exposes no interior mutability.
unsafe impl Sync for Array {}

/// The width-agnostic geometry the engine operates on: buffer base, gap size, live count, and usable slots from the
/// gap's end.  Total allocation is `start + cap` slots, every one an initialized `ArraySlot` (gap and tail spare hold
/// `None`), the live window `[start, start + len)`.  Small arrays pack this into `u32` fields; large ones box it whole.
struct Geometry {
    ptr: NonNull<ArraySlot>,
    start: usize,
    len: usize,
    cap: usize,
}

impl Geometry {
    const EMPTY: Geometry = Geometry { ptr: NonNull::dangling(), start: 0, len: 0, cap: 0 };

    fn total(&self) -> usize {
        self.start + self.cap
    }

    /// The live window as a slice: the safe face most operations go through.
    fn live(&self) -> &[ArraySlot] {
        if self.len == 0 {
            return &[];
        }

        // SAFETY: the window invariant — `[start, start + len)` are initialized slots of a live allocation.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(self.start), self.len) }
    }

    fn live_mut(&mut self) -> &mut [ArraySlot] {
        if self.len == 0 {
            return &mut [];
        }

        // SAFETY: as [`Geometry::live`], with exclusive access through `&mut self`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().add(self.start), self.len) }
    }

    /// Allocate `at_least` slots, harvesting the allocator's whole size class (§2.2.12: the slack probe applied at
    /// allocation, so later growth finds the class already claimed) and initializing every slot to `None`.
    fn allocate_slots(at_least: usize) -> Result<(NonNull<ArraySlot>, usize), AllocError> {
        let fail = || AllocError { requested: at_least * size_of::<ArraySlot>() };
        let layout = Layout::array::<ArraySlot>(at_least).map_err(|_| fail())?;
        let granted = alloc_backend::size_class(layout) / size_of::<ArraySlot>();
        let full = Layout::array::<ArraySlot>(granted).map_err(|_| fail())?;
        let raw = alloc_backend::allocate(full).ok_or_else(fail)?.cast::<ArraySlot>();

        for i in 0..granted {
            // SAFETY: `raw` addresses `granted` uninitialized slots; writing initializes them.
            unsafe { raw.as_ptr().add(i).write(None) };
        }

        Ok((raw, granted))
    }

    /// Release the buffer (values must already be drained or dropped by the caller through the live window).
    fn release_buffer(&mut self) {
        let total = self.total();

        if total == 0 {
            return;
        }

        let Ok(layout) = Layout::array::<ArraySlot>(total) else {
            return;
        };

        // SAFETY: `ptr` is the base of a live allocation of exactly `total` slots (the window invariant), made by
        // [`Geometry::allocate_slots`] through the backend.
        unsafe { alloc_backend::release(self.ptr.cast(), layout) };
        *self = Geometry::EMPTY;
    }

    /// Perl's escalating growth (§2.2.12): slide the live window back over the gap, then — the size class having been
    /// harvested at allocation — allocate anew on perl's curve, `requested + cap/5`, never doubling.
    fn grow(&mut self, min_cap: usize) -> Result<(), AllocError> {
        if self.cap >= min_cap {
            return Ok(());
        }

        // Move one: reclaim the gap.  The live window slides to the base; vacated slots return to `None`.
        if self.start > 0 {
            // SAFETY: source and destination lie within the allocation; `copy` handles the overlap.
            unsafe {
                let base = self.ptr.as_ptr();
                std::ptr::copy(base.add(self.start), base, self.len);
                for i in self.len..self.start + self.len {
                    *base.add(i) = None;
                }
            }

            self.cap = self.total();
            self.start = 0;

            if self.cap >= min_cap {
                return Ok(());
            }
        }

        // Move two: a new allocation on the ruled curve, class-harvested; live slots move, the old buffer goes.
        let requested = min_cap.checked_add(self.cap / 5).ok_or(AllocError { requested: min_cap * size_of::<ArraySlot>() })?;
        let (new_ptr, granted) = Geometry::allocate_slots(requested)?;

        // SAFETY: distinct allocations; the source window is initialized; the destination was just initialized and the
        // overwritten `None`s need no drop.
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr.as_ptr().add(self.start), new_ptr.as_ptr(), self.len);

            // The moved-from slots must not drop their values with the old buffer; forget them by overwrite-free
            // release: the buffer is released without reading slots, so nothing more is needed here.
        }

        let len = self.len;
        self.release_buffer();
        *self = Geometry { ptr: new_ptr, start: 0, len, cap: granted };

        Ok(())
    }
}

impl Default for Array {
    fn default() -> Array {
        Array::new()
    }
}

impl Drop for Array {
    /// Iterative teardown (§2.4.9): drain elements through the release worklist rather than recursing through drop
    /// glue.  Destruction is not perl-visible mutation, so the readonly flag is deliberately not consulted.
    fn drop(&mut self) {
        let mut parts = self.take_parts();
        for slot in parts.live_mut() {
            if let Some(v) = slot.take() {
                release_value(v);
            }
        }

        parts.release_buffer();
    }
}

impl Array {
    pub fn new() -> Array {
        Array { ptr: NonNull::dangling(), start: 0, len: 0, cap: 0, stash_flags: 0 }
    }

    fn is_large(&self) -> bool {
        self.stash_flags & FLAG_LARGE != 0
    }

    /// The geometry, whichever arm holds it.
    fn parts(&self) -> Geometry {
        if self.is_large() {
            // SAFETY: `FLAG_LARGE` certifies `ptr` is the boxed wide geometry (`store_parts` is the only writer).
            let wide = unsafe { &*self.ptr.cast::<Geometry>().as_ptr() };
            Geometry { ptr: wide.ptr, start: wide.start, len: wide.len, cap: wide.cap }
        } else {
            Geometry { ptr: self.ptr, start: self.start as usize, len: self.len as usize, cap: self.cap as usize }
        }
    }

    /// Take the geometry out, leaving the header empty (the teardown door).
    fn take_parts(&mut self) -> Geometry {
        if self.is_large() {
            // SAFETY: as [`Array::parts`]; the box is reclaimed and the flag dropped, so ownership moves out.
            let wide = unsafe { Box::from_raw(self.ptr.cast::<Geometry>().as_ptr()) };
            self.stash_flags &= !FLAG_LARGE;
            self.ptr = NonNull::dangling();
            self.start = 0;
            self.len = 0;
            self.cap = 0;
            *wide
        } else {
            let parts = self.parts();
            self.ptr = NonNull::dangling();
            self.start = 0;
            self.len = 0;
            self.cap = 0;
            parts
        }
    }

    /// Store the geometry, spilling to the wide arm when any field passes `u32` (§2.2.12) — a one-way door.
    fn store_parts(&mut self, parts: Geometry) {
        let fits = parts.start <= u32::MAX as usize && parts.len <= u32::MAX as usize && parts.cap <= u32::MAX as usize;
        if self.is_large() {
            // SAFETY: as [`Array::parts`]; the box stays the owner, its contents replaced.
            unsafe { *self.ptr.cast::<Geometry>().as_ptr() = parts };
        } else if fits {
            self.ptr = parts.ptr;
            self.start = parts.start as u32;
            self.len = parts.len as u32;
            self.cap = parts.cap as u32;
        } else {
            let boxed = Box::new(parts);
            self.ptr = NonNull::from(Box::leak(boxed)).cast();
            self.stash_flags |= FLAG_LARGE;
        }
    }

    /// Run an operation over the geometry and store it back.
    fn with_parts<R>(&mut self, op: impl FnOnce(&mut Geometry) -> R) -> R {
        let mut parts = self.parts();
        let result = op(&mut parts);
        self.store_parts(parts);

        result
    }

    pub fn len(&self) -> usize {
        self.parts().len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn check_writable(&self) -> Result<(), ScalarError> {
        if self.stash_flags & FLAG_READONLY != 0 { Err(ScalarError::ReadOnly) } else { Ok(()) }
    }

    /// Read access: never creates.  `None` for holes and out-of-range indices alike (the exists/defined distinction
    /// goes through [`Array::exists`]).
    pub fn get(&self, index: usize) -> Option<&Value> {
        let parts = self.parts();
        if index >= parts.len {
            return None;
        }

        // SAFETY: in-window per the check; the borrow is tied to `&self`, which owns the allocation.
        unsafe { (*parts.ptr.as_ptr().add(parts.start + index)).as_ref() }
    }

    /// `exists $a[$i]`: present and occupied.
    pub fn exists(&self, index: usize) -> bool {
        self.get(index).is_some()
    }

    /// Ensure the live window reaches `index + 1`, growing on the ruled curve; intervening slots are already `None`
    /// holes by the window invariant.
    fn extend_to(parts: &mut Geometry, index: usize) -> Result<(), AllocError> {
        let needed = index.checked_add(1).ok_or(AllocError { requested: usize::MAX })?;

        if needed > parts.cap {
            parts.grow(needed)?;
        }

        if needed > parts.len {
            parts.len = needed;
        }

        Ok(())
    }

    /// `$a[$i] = $v`: extends with holes below (container-verified: `$a[5] = "x"` on empty gives length 6 with indices
    /// 0–4 nonexistent).
    pub fn set(&mut self, index: usize, value: Value) -> Result<(), ScalarError> {
        self.check_writable()?;

        self.with_parts(|parts| {
            Array::extend_to(parts, index)?;
            parts.live_mut()[index] = Some(value);

            Ok(())
        })
    }

    /// Lvalue access: vivify the undef element and hand back the slot's value (container-verified: `\$a[3]` on empty
    /// yields length 4 with an existing undef element).  The `get`/`ensure` split is the autovivification-option
    /// mechanism (§2.2.1).
    pub fn ensure_element(&mut self, index: usize) -> Result<&mut Value, ScalarError> {
        self.check_writable()?;

        let mut parts = self.parts();
        Array::extend_to(&mut parts, index)?;
        self.store_parts(parts);
        let parts = self.parts();

        // SAFETY: in-window (just extended); exclusive through `&mut self`, which owns the allocation.
        let slot = unsafe { &mut *parts.ptr.as_ptr().add(parts.start + index) };

        Ok(slot.get_or_insert_with(Value::default))
    }

    /// `delete $a[$i]` (§2.2.1, container-verified): returns the deleted value (undef for holes and out-of-range
    /// indices, which are left untouched); deleting the last element truncates through trailing holes.
    pub fn delete(&mut self, index: usize) -> Result<Value, ScalarError> {
        self.check_writable()?;

        Ok(self.with_parts(|parts| {
            if index >= parts.len {
                return Value::default();
            }
            let deleted = parts.live_mut()[index].take().unwrap_or_default();
            if index == parts.len - 1 {
                while parts.len > 0 && parts.live()[parts.len - 1].is_none() {
                    parts.len -= 1;
                }
            }

            deleted
        }))
    }

    /// `push @a, $v` (single element; list forms loop at the ops layer).
    pub fn push_value(&mut self, value: Value) -> Result<(), ScalarError> {
        self.check_writable()?;

        self.with_parts(|parts| {
            let index = parts.len;
            Array::extend_to(parts, index)?;
            parts.live_mut()[index] = Some(value);

            Ok(())
        })
    }

    /// `pop @a`: undef for an empty array or a trailing hole (indistinguishable in perl); shortens by one.
    pub fn pop_value(&mut self) -> Result<Value, ScalarError> {
        self.check_writable()?;

        Ok(self.with_parts(|parts| {
            if parts.len == 0 {
                return Value::default();
            }

            let last = parts.len - 1;
            let value = parts.live_mut()[last].take();
            parts.len = last;

            value.unwrap_or_default()
        }))
    }

    /// `shift @a`: the O(1) window slide (§2.2.12, matching `av_shift`) — take the first slot, leave `None` behind as
    /// gap, and move the window right.  Empty shift returns undef (pinned).
    pub fn shift_value(&mut self) -> Result<Value, ScalarError> {
        self.check_writable()?;

        Ok(self.with_parts(|parts| {
            if parts.len == 0 {
                return Value::default();
            }

            let value = parts.live_mut()[0].take();
            parts.start += 1;
            parts.cap -= 1;
            parts.len -= 1;

            value.unwrap_or_default()
        }))
    }

    /// `unshift @a, $v` (single element): perl's exact two-phase strategy (§2.2.12).  Phase one reclaims from the gap
    /// only what is needed, preserving any surplus, with no element movement.  Phase two, only on a shortfall, slides
    /// the live elements right by the need plus the pre-slide fill and carves the fill's worth back off as fresh gap —
    /// the prepaid buffer equals the live count, front-gap geometric growth.
    pub fn unshift_value(&mut self, value: Value) -> Result<(), ScalarError> {
        self.check_writable()?;

        self.with_parts(|parts| {
            let mut need = 1usize;

            // Phase one: `take = min(start, need)`.
            let take = parts.start.min(need);
            if take > 0 {
                parts.start -= take;
                parts.cap += take;
                parts.len += take;
                need -= take;
            }

            if need > 0 {
                // Phase two: slide by `need + fill`, carving `fill` back off as gap (`fill` zero below two live — the
                // pre-phase-one live count is `len - take`, and the slide measures the pre-slide fill).
                let live = parts.len - take;
                let slide = if live >= 2 { live - 1 } else { 0 };
                let moved = need + slide;
                parts.grow(parts.len - take + moved)?;

                // SAFETY: the grow guarantees room; source and destination lie within the allocation and `copy` handles
                // the overlap; vacated slots return to `None`.
                unsafe {
                    let base = parts.ptr.as_ptr().add(parts.start);
                    std::ptr::copy(base, base.add(moved), live);
                    for i in 0..moved.min(live) {
                        *base.add(i) = None;
                    }
                }

                parts.start += slide;
                parts.cap -= slide;
                parts.len = live + need + take;
            }

            parts.live_mut()[0] = Some(value);

            Ok(())
        })
    }

    /// `@a = ()`.
    pub fn clear(&mut self) -> Result<(), ScalarError> {
        self.check_writable()?;

        self.with_parts(|parts| {
            for slot in parts.live_mut() {
                *slot = None;
            }

            parts.len = 0;
        });

        Ok(())
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        if readonly {
            self.stash_flags |= FLAG_READONLY;
        } else {
            self.stash_flags &= !FLAG_READONLY;
        }
    }

    pub fn is_readonly(&self) -> bool {
        self.stash_flags & FLAG_READONLY != 0
    }

    /// The graph traversal hook (§2.4.6 demolition, §2.4.11 cycle detection): existing elements only.
    #[cfg_attr(not(test), expect(dead_code, reason = "consumers are §2.4.6 demolition and the on-demand cycle detector"))]
    pub(crate) fn values_iter(&self) -> impl Iterator<Item = &Value> {
        let parts = self.parts();

        // SAFETY: the live-window slice, tied to `&self` (as [`Geometry::live`], through the shared borrow).
        let live: &[ArraySlot] = if parts.len == 0 { &[] } else { unsafe { std::slice::from_raw_parts(parts.ptr.as_ptr().add(parts.start), parts.len) } };

        live.iter().filter_map(Option::as_ref)
    }

    /// Test doors (§2.2.12 batteries): the gap, the capacity, the buffer identity, and the forced wide arm.
    #[cfg(test)]
    pub(crate) fn probe_geometry(&self) -> (usize, usize, usize, bool) {
        let parts = self.parts();

        (parts.start, parts.len, parts.cap, self.is_large())
    }

    #[cfg(test)]
    pub(crate) fn probe_base(&self) -> *const ArraySlot {
        self.parts().ptr.as_ptr()
    }

    #[cfg(test)]
    pub(crate) fn force_large_for_test(&mut self) {
        if !self.is_large() {
            let parts = self.take_parts();
            let boxed = Box::new(parts);
            self.ptr = NonNull::from(Box::leak(boxed)).cast();
            self.stash_flags |= FLAG_LARGE;
        }
    }
}

// ── Hash (§2.2.1, §2.2.13) ────────────────────────────────────
/// The dual-engine hash (§2.2.13): a bucket engine on `hashbrown::HashTable` by default, and the insertion-ordered
/// `IndexMap` engine on explicit request, fixed at construction.  Keys are laundered at storage (§2.6.2); the stored
/// key is kept on re-store (equal keys: the first-stored spelling wins) under either engine.
pub struct Hash {
    engine: HashEngine,
    readonly: bool,
}

/// The engine, chosen at construction and never morphed (§2.2.13).
enum HashEngine {
    /// The default: SwissTable buckets, per-hash SipHash keys, and the `each` cursor as a bucket index.
    Buckets { table: HashTable<(PString, Value)>, hasher: RandomState, cursor: usize },

    /// The explicitly requested insertion-ordered mode (§2.2.10): the `each` cursor is an entry index.
    Ordered { map: IndexMap<PString, Value>, cursor: usize },

    /// The feature-gated immutable mode (§2.2.13): a persistent HAMT; the `each` cursor is an owning iterator over an
    /// O(1) snapshot, yielding with live revalidation.
    #[cfg(feature = "imbl")]
    Immutable { map: ImblMap, iter: Option<Box<ImblIter>> },
}

#[cfg(feature = "imbl")]
type ImblMap = imbl::HashMap<PString, Value>;

#[cfg(feature = "imbl")]
type ImblIter = <ImblMap as IntoIterator>::IntoIter;

/// Retire a parked snapshot iterator (§2.2.13): its remaining values route through the release worklist — a co-owner's
/// release nets zero on shared values; the final owner's moves them out.
#[cfg(feature = "imbl")]
fn retire_iter(iter: &mut Option<Box<ImblIter>>) {
    if let Some(rest) = iter.take() {
        for (_key, v) in *rest {
            release_value(v);
        }
    }
}

impl Default for Hash {
    fn default() -> Hash {
        Hash::new()
    }
}

impl Drop for Hash {
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

impl Hash {
    /// The default engine (§2.2.13): buckets, per-hash random iteration order.
    pub fn new() -> Hash {
        Hash { engine: HashEngine::Buckets { table: HashTable::new(), hasher: RandomState::new(), cursor: 0 }, readonly: false }
    }

    /// The insertion-ordered mode (§2.2.13), on explicit request only; the perl-visible request surface is the runtime
    /// design's.
    pub fn insertion_ordered() -> Hash {
        Hash { engine: HashEngine::Ordered { map: IndexMap::new(), cursor: 0 }, readonly: false }
    }

    /// The immutable mode (§2.2.13), on explicit request only: a persistent HAMT with O(1) [`Hash::snapshot`].
    #[cfg(feature = "imbl")]
    pub fn immutable() -> Hash {
        Hash { engine: HashEngine::Immutable { map: ImblMap::new(), iter: None }, readonly: false }
    }

    /// An O(1) detached, diverging copy of an immutable-engine hash (§2.2.13), with a fresh cursor and the readonly
    /// flag cleared; the other engines answer [`ScalarError::SnapshotUnsupported`], their copies being O(n).
    pub fn snapshot(&self) -> Result<Hash, ScalarError> {
        match &self.engine {
            #[cfg(feature = "imbl")]
            HashEngine::Immutable { map, .. } => Ok(Hash { engine: HashEngine::Immutable { map: map.clone(), iter: None }, readonly: false }),
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

    fn launder(mut key: PString) -> PString {
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
    pub fn store(&mut self, key: PString, value: Value) -> Result<(), ScalarError> {
        self.check_writable()?;
        let key = Hash::launder(key);
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
    pub fn get(&self, key: &PString) -> Option<&Value> {
        match &self.engine {
            HashEngine::Buckets { table, hasher, .. } => table.find(hasher.hash_one(key), |(stored, _)| stored == key).map(|(_, value)| value),
            HashEngine::Ordered { map, .. } => map.get(key),

            #[cfg(feature = "imbl")]
            HashEngine::Immutable { map, .. } => map.get(key),
        }
    }

    /// `exists $h{$k}`: absence of the entry is nonexistence (§2.2.1 — no slot wrapper).
    pub fn exists(&self, key: &PString) -> bool {
        self.get(key).is_some()
    }

    /// Lvalue access: vivify the undef entry (container-verified: `\$h{k}` creates an existing undef entry).  The
    /// `get`/`ensure` split is the autovivification-option mechanism (§2.2.1).  Vivification of an absent key is a
    /// new-key insertion and resets the cursor (§2.2.13); an existing key leaves it alone.
    pub fn entry_or_undef(&mut self, key: PString) -> Result<&mut Value, ScalarError> {
        self.check_writable()?;
        let key = Hash::launder(key);
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
    pub fn delete(&mut self, key: &PString) -> Result<Value, ScalarError> {
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
                let (_, value) = map.swap_remove_index(index).unwrap_or_else(|| (PString::empty(), Value::default()));
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
    pub fn each(&mut self) -> Option<(PString, Value)> {
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
                let walk = iter.get_or_insert_with(|| Box::new(map.clone().into_iter()));
                for (key, _snapshot_value) in walk.by_ref() {
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
    pub fn keys(&mut self) -> Vec<PString> {
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

/// The engine-dispatched values walk behind [`Hash::values_iter`]: a hand-rolled two-arm iterator, keeping the cold
/// traversal hook vtable-free.
pub(crate) enum HashValuesIter<'a> {
    Buckets {
        table: &'a HashTable<(PString, Value)>,
        next: usize,
    },
    Ordered(indexmap::map::Values<'a, PString, Value>),

    #[cfg(feature = "imbl")]
    Immutable(imbl::hashmap::Values<'a, PString, Value, imbl::shared_ptr::DefaultSharedPtr>),
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

container_handle!(ArrayRef, Array, "The Arc-backed shared array identity (§2.2.1).");
container_handle!(HashRef, Hash, "The Arc-backed shared hash identity (§2.2.1).");

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/containers_tests.rs"]
mod tests;
