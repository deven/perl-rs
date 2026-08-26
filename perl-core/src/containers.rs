//! `Array` and `Hash` — the containers (§2.2.1) — each the public enum of its per-engine shared identities (§2.2.13):
//! `Array` over `PerlArray` and `ImmutableArray`, `Hash` over its three.
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
use parking_lot::RwLock;
use std::alloc::Layout;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::BuildHasher;
use std::ptr::NonNull;

use crate::alloc_backend;
use crate::cow_buffer::AllocError;
use crate::heap::{HeapArc, release_value};
use crate::scalar::ScalarError;
use crate::string::PString;
use crate::value::{ArraySlot, Value};

#[cfg(feature = "imbl")]
use imbl;

#[cfg(feature = "indexmap")]
use indexmap::IndexMap;

// ── Array (§2.2.1, §2.2.12, §2.2.13) ─────────────────────────────
/// The front-gap slot engine (§2.2.12), shaped on perl's AV: an allocation whose live window floats behind a gap, so
/// `shift` is an O(1) window slide and `unshift` reclaims the gap before any element moves.  `None` = a hole
/// (nonexistent element); `Some(Undef)` = an existing element holding undef.
///
/// The header is the ruled 24 bytes: a manual two-arm tag in the flags byte, because a Rust enum would spend a
/// discriminant byte the §2.4.3 budget does not have.  Small arrays keep `ptr` as the buffer base with `u32` geometry;
/// past `u32` the geometry spills to a boxed wide header and `ptr` holds that box (`FLAG_LARGE`), the `Heap32`/`Heap`
/// philosophy applied to arrays.  Bits 8..32 of `stash_flags` are the reserved bless stash (u24).
pub struct PerlArray {
    /// Small: the buffer base (dangling when unallocated).  Large: the boxed [`Geometry`].
    ptr: NonNull<ArraySlot>,
    start: u32,
    len: u32,
    cap: u32,
    stash_flags: u32,
}

const _: () = assert!(size_of::<PerlArray>() == 24);

/// The dynamic readonly flag's bit.
const FLAG_READONLY: u32 = 1;

/// The arm tag: set when `ptr` holds the boxed wide geometry.
const FLAG_LARGE: u32 = 2;

// SAFETY: the raw pointer is exclusively owned storage of `Send + Sync` slots; sharing is external (§2.2.1: the
// handle's lock).
unsafe impl Send for PerlArray {}

// SAFETY: as above — `&PerlArray` exposes no interior mutability.
unsafe impl Sync for PerlArray {}

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

        // A class the allocator declines to name is one it cannot serve: fall back to the request itself and let the
        // allocation below report the failure, rather than harvesting slack from a refusal.
        let granted = alloc_backend::size_class(layout).map_or(at_least, |class| class / size_of::<ArraySlot>());
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

impl Default for PerlArray {
    fn default() -> PerlArray {
        PerlArray::new()
    }
}

impl fmt::Debug for PerlArray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PerlArray").field("len", &self.len()).finish_non_exhaustive()
    }
}

#[cfg(feature = "imbl")]
impl fmt::Debug for ImmutableArray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImmutableArray").field("len", &self.vec.len()).finish_non_exhaustive()
    }
}

impl Drop for PerlArray {
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

impl PerlArray {
    pub fn new() -> PerlArray {
        PerlArray { ptr: NonNull::dangling(), start: 0, len: 0, cap: 0, stash_flags: 0 }
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
            // SAFETY: as [`PerlArray::parts`]; the box is reclaimed and the flag dropped, so ownership moves out.
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
            // SAFETY: as [`PerlArray::parts`]; the box stays the owner, its contents replaced.
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
    /// goes through [`PerlArray::exists`]).
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
            PerlArray::extend_to(parts, index)?;
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
        PerlArray::extend_to(&mut parts, index)?;
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
            PerlArray::extend_to(parts, index)?;
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

    /// The graph traversal walk over the live window, consumed by [`Array::for_each_value`].
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

// ── Array: the per-engine identities (§2.2.12, §2.2.13) ──────────
/// The immutable array engine (§2.2.12 amendment): an RRB tree over the same slots — O(1) clones and O(1)
/// amortized operations at both ends, O(log32) indexing as the recorded tradeoff.  No cursor state exists at this
/// layer, so the lock guards only the root swap and the readonly flag.
#[cfg(feature = "imbl")]
pub struct ImmutableArray {
    vec: imbl::Vector<ArraySlot>,
    readonly: bool,
}

#[cfg(feature = "imbl")]
impl ImmutableArray {
    fn extend_to(&mut self, index: usize) {
        while self.vec.len() <= index {
            self.vec.push_back(None);
        }
    }
}

#[cfg(feature = "imbl")]
impl Drop for ImmutableArray {
    /// Iterative teardown (§2.4.9): values route through the release worklist; a co-owner's drain nets zero on
    /// shared values, the final owner's moves them out.
    fn drop(&mut self) {
        for v in std::mem::take(&mut self.vec).into_iter().flatten() {
            release_value(v);
        }
    }
}

/// The per-engine array (§2.2.12, §2.2.13): the public enum of shared identities, itself the cheap-clone handle,
/// fixed at construction (construction-final, §2.2.13).  Locks are internal: reads clone out, lvalue access is
/// closure-shaped.
#[derive(Clone)]
pub enum Array {
    /// The default: the front-gap engine (§2.2.12), perl's own AV shape.
    Perl(HeapArc<RwLock<PerlArray>>),

    /// The explicitly requested immutable mode (§2.2.12 amendment): O(1) snapshots, O(1) both-end operations.
    #[cfg(feature = "imbl")]
    Immutable(HeapArc<RwLock<ImmutableArray>>),
}

impl Default for Array {
    fn default() -> Array {
        Array::new()
    }
}

impl Array {
    /// The default engine (§2.2.12): the front-gap header in its own exactly-sized allocation.
    pub fn new() -> Array {
        Array::Perl(HeapArc::new(RwLock::new(PerlArray::new())))
    }

    /// The immutable mode (§2.2.12 amendment), on explicit request only.
    #[cfg(feature = "imbl")]
    pub fn immutable() -> Array {
        Array::Immutable(HeapArc::new(RwLock::new(ImmutableArray { vec: imbl::Vector::new(), readonly: false })))
    }

    /// Identity comparison: the same shared allocation.
    pub fn ptr_eq(a: &Array, b: &Array) -> bool {
        match (a, b) {
            (Array::Perl(x), Array::Perl(y)) => HeapArc::ptr_eq(x, y),

            #[cfg(feature = "imbl")]
            (Array::Immutable(x), Array::Immutable(y)) => HeapArc::ptr_eq(x, y),

            #[cfg_attr(not(feature = "imbl"), expect(unreachable_patterns, reason = "with one engine the gap arm is the whole match"))]
            _ => false,
        }
    }

    /// The address perl exposes when the reference is numified or stringified.
    pub fn addr(&self) -> usize {
        match self {
            Array::Perl(a) => HeapArc::as_ptr(a) as usize,

            #[cfg(feature = "imbl")]
            Array::Immutable(a) => HeapArc::as_ptr(a) as usize,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Array::Perl(a) => a.read().len(),

            #[cfg(feature = "imbl")]
            Array::Immutable(a) => a.read().vec.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read access, cloned out (§2.2.13: no borrow escapes the internal lock): `None` for holes and out-of-range
    /// indices alike.
    pub fn get(&self, index: usize) -> Option<Value> {
        match self {
            Array::Perl(a) => a.read().get(index).cloned(),

            #[cfg(feature = "imbl")]
            Array::Immutable(a) => a.read().vec.get(index).and_then(|slot| slot.clone()),
        }
    }

    /// `exists $a[$i]`: present and occupied.
    pub fn exists(&self, index: usize) -> bool {
        self.get(index).is_some()
    }

    /// `$a[$i] = $v`: extends with holes below (container-verified).
    pub fn set(&self, index: usize, value: Value) -> Result<(), ScalarError> {
        match self {
            Array::Perl(a) => a.write().set(index, value),

            #[cfg(feature = "imbl")]
            Array::Immutable(a) => {
                let mut e = a.write();
                check_writable(e.readonly)?;
                e.extend_to(index);
                e.vec.set(index, Some(value));
                Ok(())
            }
        }
    }

    /// Lvalue access, closure-shaped (§2.2.13): vivify the undef element and run `f` on the slot's value
    /// (container-verified: `\$a[3]` on empty yields length 4 with an existing undef element).
    pub fn ensure_element<R>(&self, index: usize, f: impl FnOnce(&mut Value) -> R) -> Result<R, ScalarError> {
        match self {
            Array::Perl(a) => Ok(f(a.write().ensure_element(index)?)),

            #[cfg(feature = "imbl")]
            Array::Immutable(a) => {
                let mut e = a.write();
                check_writable(e.readonly)?;
                e.extend_to(index);
                let Some(slot) = e.vec.get_mut(index) else {
                    return Err(ScalarError::ReadOnly);
                };
                Ok(f(slot.get_or_insert_with(Value::default)))
            }
        }
    }

    /// `delete $a[$i]` (§2.2.1, container-verified): returns the deleted value; deleting the last element
    /// truncates through trailing holes.
    pub fn delete(&self, index: usize) -> Result<Value, ScalarError> {
        match self {
            Array::Perl(a) => a.write().delete(index),

            #[cfg(feature = "imbl")]
            Array::Immutable(a) => {
                let mut e = a.write();
                check_writable(e.readonly)?;
                if index >= e.vec.len() {
                    return Ok(Value::default());
                }
                let deleted = e.vec.get_mut(index).and_then(Option::take).unwrap_or_default();
                while e.vec.last().is_some_and(Option::is_none) {
                    e.vec.pop_back();
                }
                Ok(deleted)
            }
        }
    }

    /// `push @a, $v` (single element; list forms loop at the ops layer).
    pub fn push_value(&self, value: Value) -> Result<(), ScalarError> {
        match self {
            Array::Perl(a) => a.write().push_value(value),

            #[cfg(feature = "imbl")]
            Array::Immutable(a) => {
                let mut e = a.write();
                check_writable(e.readonly)?;
                e.vec.push_back(Some(value));
                Ok(())
            }
        }
    }

    /// `pop @a`: undef for an empty array or a trailing hole; shortens by one.
    pub fn pop_value(&self) -> Result<Value, ScalarError> {
        match self {
            Array::Perl(a) => a.write().pop_value(),

            #[cfg(feature = "imbl")]
            Array::Immutable(a) => {
                let mut e = a.write();
                check_writable(e.readonly)?;
                Ok(e.vec.pop_back().flatten().unwrap_or_default())
            }
        }
    }

    /// `shift @a`: O(1) under either engine — the gap engine's window slide (§2.2.12), the RRB tree's
    /// `pop_front`.
    pub fn shift_value(&self) -> Result<Value, ScalarError> {
        match self {
            Array::Perl(a) => a.write().shift_value(),

            #[cfg(feature = "imbl")]
            Array::Immutable(a) => {
                let mut e = a.write();
                check_writable(e.readonly)?;
                Ok(e.vec.pop_front().flatten().unwrap_or_default())
            }
        }
    }

    /// `unshift @a, $v`: the gap engine's two-phase strategy (§2.2.12), the RRB tree's `push_front`.
    pub fn unshift_value(&self, value: Value) -> Result<(), ScalarError> {
        match self {
            Array::Perl(a) => a.write().unshift_value(value),

            #[cfg(feature = "imbl")]
            Array::Immutable(a) => {
                let mut e = a.write();
                check_writable(e.readonly)?;
                e.vec.push_front(Some(value));
                Ok(())
            }
        }
    }

    /// `@a = ()`.
    pub fn clear(&self) -> Result<(), ScalarError> {
        match self {
            Array::Perl(a) => a.write().clear(),

            #[cfg(feature = "imbl")]
            Array::Immutable(a) => {
                let mut e = a.write();
                check_writable(e.readonly)?;
                for v in std::mem::take(&mut e.vec).into_iter().flatten() {
                    release_value(v);
                }
                Ok(())
            }
        }
    }

    pub fn set_readonly(&self, readonly: bool) {
        match self {
            Array::Perl(a) => a.write().set_readonly(readonly),

            #[cfg(feature = "imbl")]
            Array::Immutable(a) => a.write().readonly = readonly,
        }
    }

    pub fn is_readonly(&self) -> bool {
        match self {
            Array::Perl(a) => a.read().is_readonly(),

            #[cfg(feature = "imbl")]
            Array::Immutable(a) => a.read().readonly,
        }
    }

    /// An O(1) detached, diverging copy of an immutable-engine array (§2.2.12 amendment), readonly cleared; the
    /// gap engine answers [`ScalarError::SnapshotUnsupported`], its copy being O(n).
    pub fn snapshot(&self) -> Result<Array, ScalarError> {
        match self {
            #[cfg(feature = "imbl")]
            Array::Immutable(a) => {
                let vec = a.read().vec.clone();
                Ok(Array::Immutable(HeapArc::new(RwLock::new(ImmutableArray { vec, readonly: false }))))
            }
            _ => Err(ScalarError::SnapshotUnsupported),
        }
    }

    /// The graph traversal hook (§2.4.6 demolition, §2.4.11 cycle detection): existing elements only, visited
    /// under the read lock.
    #[cfg_attr(not(test), expect(dead_code, reason = "consumers are §2.4.6 demolition and the on-demand cycle detector"))]
    pub(crate) fn for_each_value(&self, mut f: impl FnMut(&Value)) {
        match self {
            Array::Perl(a) => {
                for v in a.read().values_iter() {
                    f(v);
                }
            }

            #[cfg(feature = "imbl")]
            Array::Immutable(a) => {
                for v in a.read().vec.iter().flatten() {
                    f(v);
                }
            }
        }
    }

    /// Test doors (§2.2.12 batteries), gap-engine geometry only.
    #[cfg(test)]
    pub(crate) fn probe_geometry(&self) -> (usize, usize, usize, bool) {
        match self {
            Array::Perl(a) => a.read().probe_geometry(),

            #[cfg(feature = "imbl")]
            Array::Immutable(_) => (0, self.len(), 0, false),
        }
    }

    #[cfg(test)]
    pub(crate) fn probe_base(&self) -> *const ArraySlot {
        match self {
            Array::Perl(a) => a.read().probe_base(),

            #[cfg(feature = "imbl")]
            Array::Immutable(_) => std::ptr::null(),
        }
    }

    #[cfg(test)]
    pub(crate) fn force_large_for_test(&self) {
        #[cfg_attr(not(feature = "imbl"), expect(irrefutable_let_patterns, reason = "one engine without imbl"))]
        if let Array::Perl(a) = self {
            a.write().force_large_for_test();
        }
    }
}

// ── Hash (§2.2.1, §2.2.13) ────────────────────────────────────
/// The per-engine hash (§2.2.13): `Hash` is the public enum of per-engine shared identities — each arm an
/// `HeapArc<RwLock<…>>` over its own exactly-sized engine — and is itself the cheap-clone handle.  The engine is fixed
/// at construction (construction-final, §2.2.13: no in-place morph, matching perl, enforced by the representation).
/// Keys are laundered at storage (§2.6.2); the stored key is kept on re-store (equal keys: the first-stored spelling
/// wins) under every engine.  Locks are internal: reads clone out, lvalue access is closure-shaped.
#[derive(Clone)]
pub enum Hash {
    /// The default: SwissTable buckets, per-hash SipHash keys, and the `each` cursor as a bucket index — stock
    /// perl hash semantics.
    Perl(HeapArc<RwLock<PerlHash>>),

    /// The explicitly requested insertion-ordered mode (§2.2.10): the `each` cursor is an entry index.
    #[cfg(feature = "indexmap")]
    Ordered(HeapArc<RwLock<OrderedHash>>),

    /// The explicitly requested immutable mode (§2.2.13): a persistent HAMT with O(1) [`Hash::snapshot`].
    #[cfg(feature = "imbl")]
    Immutable(HeapArc<RwLock<ImmutableHash>>),
}

/// The default bucket engine (§2.2.13).
pub struct PerlHash {
    table: HashTable<(PString, Value)>,
    hasher: RandomState,
    cursor: usize,
    readonly: bool,
}

/// The insertion-ordered engine (§2.2.10, §2.2.13).
#[cfg(feature = "indexmap")]
pub struct OrderedHash {
    map: IndexMap<PString, Value>,
    cursor: usize,
    readonly: bool,
}

/// The immutable engine (§2.2.13): the `each` cursor is an owning iterator over an O(1) snapshot, yielding with live
/// revalidation, boxed so an idle hash does not carry the cursor's hundred-byte chunk stack.
#[cfg(feature = "imbl")]
pub struct ImmutableHash {
    map: ImblMap,
    iter: Option<Box<ImblIter>>,
    readonly: bool,
}

#[cfg(feature = "imbl")]
type ImblMap = imbl::HashMap<PString, Value>;

#[cfg(feature = "imbl")]
pub(crate) type ImblIter = <ImblMap as IntoIterator>::IntoIter;

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

fn launder(mut key: PString) -> PString {
    if key.is_tainted() {
        key.untaint_for_sanctioned_path();
    }
    key
}

fn check_writable(readonly: bool) -> Result<(), ScalarError> {
    if readonly { Err(ScalarError::ReadOnly) } else { Ok(()) }
}

impl Default for Hash {
    fn default() -> Hash {
        Hash::new()
    }
}

impl Hash {
    /// The default engine (§2.2.13): buckets, per-hash random iteration order.
    pub fn new() -> Hash {
        Hash::Perl(HeapArc::new(RwLock::new(PerlHash { table: HashTable::new(), hasher: RandomState::new(), cursor: 0, readonly: false })))
    }

    /// The insertion-ordered mode (§2.2.13), on explicit request only; the perl-visible request surface is the runtime
    /// design's.
    #[cfg(feature = "indexmap")]
    pub fn ordered() -> Hash {
        Hash::Ordered(HeapArc::new(RwLock::new(OrderedHash { map: IndexMap::new(), cursor: 0, readonly: false })))
    }

    /// The immutable mode (§2.2.13), on explicit request only: a persistent HAMT with O(1) [`Hash::snapshot`].
    #[cfg(feature = "imbl")]
    pub fn immutable() -> Hash {
        Hash::Immutable(HeapArc::new(RwLock::new(ImmutableHash { map: ImblMap::new(), iter: None, readonly: false })))
    }

    /// Identity comparison: the same shared allocation (engines never compare equal across kinds).
    pub fn ptr_eq(a: &Hash, b: &Hash) -> bool {
        match (a, b) {
            (Hash::Perl(x), Hash::Perl(y)) => HeapArc::ptr_eq(x, y),

            #[cfg(feature = "indexmap")]
            (Hash::Ordered(x), Hash::Ordered(y)) => HeapArc::ptr_eq(x, y),

            #[cfg(feature = "imbl")]
            (Hash::Immutable(x), Hash::Immutable(y)) => HeapArc::ptr_eq(x, y),

            #[cfg_attr(
                not(any(feature = "indexmap", feature = "imbl")),
                expect(unreachable_patterns, reason = "with one engine the bucket arm is the whole match")
            )]
            _ => false,
        }
    }

    /// The address perl exposes when the reference is numified or stringified.
    pub fn addr(&self) -> usize {
        match self {
            Hash::Perl(a) => HeapArc::as_ptr(a) as usize,

            #[cfg(feature = "indexmap")]
            Hash::Ordered(a) => HeapArc::as_ptr(a) as usize,

            #[cfg(feature = "imbl")]
            Hash::Immutable(a) => HeapArc::as_ptr(a) as usize,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Hash::Perl(a) => a.read().table.len(),

            #[cfg(feature = "indexmap")]
            Hash::Ordered(a) => a.read().map.len(),

            #[cfg(feature = "imbl")]
            Hash::Immutable(a) => a.read().map.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `$h{$k} = $v`, laundering the key (§2.6.2: hash-key canonicalization is a sanctioned untaint path —
    /// container-verified: a tainted key stores clean).
    ///
    /// The cursor discipline (§2.2.13): an existing key updates in place and leaves the cursor alone — value updates
    /// during iteration are contract-specified safe — while a new key resets the bucket cursor, answering any rehash
    /// with a restart (the snapshot walk is rehash-immune and needs no reset).
    pub fn store(&self, key: PString, value: Value) -> Result<(), ScalarError> {
        let key = launder(key);
        match self {
            Hash::Perl(arc) => {
                let mut e = arc.write();
                check_writable(e.readonly)?;
                let hash = e.hasher.hash_one(&key);
                let e = &mut *e;
                if let Some(pair) = e.table.find_mut(hash, |(stored, _)| stored == &key) {
                    pair.1 = value;
                } else {
                    e.cursor = 0;
                    e.table.insert_unique(hash, (key, value), |(stored, _)| e.hasher.hash_one(stored));
                }
            }

            #[cfg(feature = "indexmap")]
            Hash::Ordered(arc) => {
                let mut e = arc.write();
                check_writable(e.readonly)?;
                e.map.insert(key, value);
            }

            #[cfg(feature = "imbl")]
            Hash::Immutable(arc) => {
                let mut e = arc.write();
                check_writable(e.readonly)?;
                if let Some(slot) = e.map.get_mut(&key) {
                    *slot = value;
                } else {
                    e.map.insert(key, value);
                }
            }
        }
        Ok(())
    }

    /// Read access: never creates.  Clones out (§2.2.13: no borrow escapes the internal lock) — a refcount bump for
    /// shared values.
    pub fn get(&self, key: &PString) -> Option<Value> {
        match self {
            Hash::Perl(arc) => {
                let e = arc.read();
                e.table.find(e.hasher.hash_one(key), |(stored, _)| stored == key).map(|(_, value)| value.clone())
            }

            #[cfg(feature = "indexmap")]
            Hash::Ordered(arc) => arc.read().map.get(key).cloned(),

            #[cfg(feature = "imbl")]
            Hash::Immutable(arc) => arc.read().map.get(key).cloned(),
        }
    }

    /// `exists $h{$k}`: absence of the entry is nonexistence (§2.2.1 — no slot wrapper).
    pub fn exists(&self, key: &PString) -> bool {
        match self {
            Hash::Perl(arc) => {
                let e = arc.read();
                e.table.find(e.hasher.hash_one(key), |(stored, _)| stored == key).is_some()
            }

            #[cfg(feature = "indexmap")]
            Hash::Ordered(arc) => arc.read().map.contains_key(key),

            #[cfg(feature = "imbl")]
            Hash::Immutable(arc) => arc.read().map.contains_key(key),
        }
    }

    /// Lvalue access, closure-shaped (§2.2.13: the guard cannot escape): vivify the undef entry and run `f` on the
    /// slot's value (container-verified: `\$h{k}` creates an existing undef entry).  Vivification of an absent key is a
    /// new-key insertion and resets the bucket cursor; an existing key leaves it alone.
    pub fn entry_or_undef<R>(&self, key: PString, f: impl FnOnce(&mut Value) -> R) -> Result<R, ScalarError> {
        let key = launder(key);
        match self {
            Hash::Perl(arc) => {
                let mut e = arc.write();
                check_writable(e.readonly)?;
                let hash = e.hasher.hash_one(&key);
                let e = &mut *e;
                let entry = match e.table.entry(hash, |(stored, _)| stored == &key, |(stored, _)| e.hasher.hash_one(stored)) {
                    Entry::Occupied(occupied) => occupied,
                    Entry::Vacant(vacant) => {
                        e.cursor = 0;
                        vacant.insert((key, Value::default()))
                    }
                };
                Ok(f(&mut entry.into_mut().1))
            }

            #[cfg(feature = "indexmap")]
            Hash::Ordered(arc) => {
                let mut e = arc.write();
                check_writable(e.readonly)?;
                Ok(f(e.map.entry(key).or_default()))
            }

            #[cfg(feature = "imbl")]
            Hash::Immutable(arc) => {
                let mut e = arc.write();
                check_writable(e.readonly)?;
                Ok(f(e.map.entry(key).or_insert_with(Value::default)))
            }
        }
    }

    /// `delete $h{$k}`, returning the value (undef for absent keys).  The cursor is never touched (§2.2.13): erasure
    /// moves nothing, so every deletion is exact — delete-current is behind the cursor already, and a deleted unvisited
    /// entry is a slot the walk will skip.  (Ordered engine: `swap_remove` keeps delete O(1); the cursor adjustment
    /// makes delete-current exact.  Immutable engine: the parked walk revalidates live.)
    pub fn delete(&self, key: &PString) -> Result<Value, ScalarError> {
        match self {
            Hash::Perl(arc) => {
                let mut e = arc.write();
                check_writable(e.readonly)?;
                let hash = e.hasher.hash_one(key);
                match e.table.find_entry(hash, |(stored, _)| stored == key) {
                    Ok(occupied) => {
                        let ((_key, value), _) = occupied.remove();
                        Ok(value)
                    }
                    Err(_) => Ok(Value::default()),
                }
            }

            #[cfg(feature = "indexmap")]
            Hash::Ordered(arc) => {
                let mut e = arc.write();
                check_writable(e.readonly)?;
                let Some(index) = e.map.get_index_of(key) else {
                    return Ok(Value::default());
                };
                let (_, value) = e.map.swap_remove_index(index).unwrap_or_else(|| (PString::empty(), Value::default()));
                if index < e.cursor {
                    e.cursor -= 1;
                }
                Ok(value)
            }

            #[cfg(feature = "imbl")]
            Hash::Immutable(arc) => {
                let mut e = arc.write();
                check_writable(e.readonly)?;
                Ok(e.map.remove(key).unwrap_or_default())
            }
        }
    }

    /// `each %h`: yield the next pair, or `None` once at exhaustion (then restart — container-verified).
    pub fn each(&self) -> Option<(PString, Value)> {
        match self {
            Hash::Perl(arc) => {
                let mut e = arc.write();
                let e = &mut *e;
                let end = e.table.num_buckets();
                while e.cursor < end {
                    let bucket = e.cursor;
                    e.cursor += 1;
                    if let Some((key, value)) = e.table.get_bucket(bucket) {
                        return Some((key.clone(), value.clone()));
                    }
                }
                e.cursor = 0;
                None
            }

            #[cfg(feature = "indexmap")]
            Hash::Ordered(arc) => {
                let mut e = arc.write();
                match e.map.get_index(e.cursor) {
                    Some((k, v)) => {
                        let pair = (k.clone(), v.clone());
                        e.cursor += 1;
                        Some(pair)
                    }
                    None => {
                        e.cursor = 0;
                        None
                    }
                }
            }

            #[cfg(feature = "imbl")]
            Hash::Immutable(arc) => {
                // §2.2.13: walk an O(1) snapshot through an owning iterator, revalidating live — deleted keys are
                // skipped, values are read live (specified-visible updates), inserted keys wait for restart.
                let mut e = arc.write();
                let e = &mut *e;
                let walk = e.iter.get_or_insert_with(|| Box::new(e.map.clone().into_iter()));
                for (key, _snapshot_value) in walk.by_ref() {
                    if let Some(live) = e.map.get(&key) {
                        return Some((key, live.clone()));
                    }
                }
                e.iter = None;
                None
            }
        }
    }

    /// `keys %h`: resets the iterator (container-verified); shares `each`'s scan order (§2.2.13).
    pub fn keys(&self) -> Vec<PString> {
        match self {
            Hash::Perl(arc) => {
                let mut e = arc.write();
                e.cursor = 0;
                (0..e.table.num_buckets()).filter_map(|b| e.table.get_bucket(b)).map(|(k, _)| k.clone()).collect()
            }

            #[cfg(feature = "indexmap")]
            Hash::Ordered(arc) => {
                let mut e = arc.write();
                e.cursor = 0;
                e.map.keys().cloned().collect()
            }

            #[cfg(feature = "imbl")]
            Hash::Immutable(arc) => {
                let mut e = arc.write();
                retire_iter(&mut e.iter);
                e.map.keys().cloned().collect()
            }
        }
    }

    /// `values %h`: resets the iterator; corresponds to `keys` order (container-verified).
    pub fn values(&self) -> Vec<Value> {
        match self {
            Hash::Perl(arc) => {
                let mut e = arc.write();
                e.cursor = 0;
                (0..e.table.num_buckets()).filter_map(|b| e.table.get_bucket(b)).map(|(_, v)| v.clone()).collect()
            }

            #[cfg(feature = "indexmap")]
            Hash::Ordered(arc) => {
                let mut e = arc.write();
                e.cursor = 0;
                e.map.values().cloned().collect()
            }

            #[cfg(feature = "imbl")]
            Hash::Immutable(arc) => {
                let mut e = arc.write();
                retire_iter(&mut e.iter);
                e.map.values().cloned().collect()
            }
        }
    }

    /// `%h = ()`.
    pub fn clear(&self) -> Result<(), ScalarError> {
        match self {
            Hash::Perl(arc) => {
                let mut e = arc.write();
                check_writable(e.readonly)?;
                e.cursor = 0;
                for (_key, v) in e.table.drain() {
                    release_value(v);
                }
            }

            #[cfg(feature = "indexmap")]
            Hash::Ordered(arc) => {
                let mut e = arc.write();
                check_writable(e.readonly)?;
                e.cursor = 0;
                for (_key, v) in e.map.drain(..) {
                    release_value(v);
                }
            }

            #[cfg(feature = "imbl")]
            Hash::Immutable(arc) => {
                let mut e = arc.write();
                check_writable(e.readonly)?;
                retire_iter(&mut e.iter);
                for (_key, v) in std::mem::take(&mut e.map) {
                    release_value(v);
                }
            }
        }
        Ok(())
    }

    pub fn set_readonly(&self, readonly: bool) {
        match self {
            Hash::Perl(arc) => arc.write().readonly = readonly,

            #[cfg(feature = "indexmap")]
            Hash::Ordered(arc) => arc.write().readonly = readonly,

            #[cfg(feature = "imbl")]
            Hash::Immutable(arc) => arc.write().readonly = readonly,
        }
    }

    pub fn is_readonly(&self) -> bool {
        match self {
            Hash::Perl(arc) => arc.read().readonly,

            #[cfg(feature = "indexmap")]
            Hash::Ordered(arc) => arc.read().readonly,

            #[cfg(feature = "imbl")]
            Hash::Immutable(arc) => arc.read().readonly,
        }
    }

    /// An O(1) detached, diverging copy of an immutable-engine hash (§2.2.13), with a fresh cursor and the readonly
    /// flag cleared; the other engines answer [`ScalarError::SnapshotUnsupported`], their copies being O(n).
    pub fn snapshot(&self) -> Result<Hash, ScalarError> {
        match self {
            #[cfg(feature = "imbl")]
            Hash::Immutable(arc) => {
                let map = arc.read().map.clone();
                Ok(Hash::Immutable(HeapArc::new(RwLock::new(ImmutableHash { map, iter: None, readonly: false }))))
            }
            _ => Err(ScalarError::SnapshotUnsupported),
        }
    }

    /// The graph traversal hook (§2.4.6 demolition, §2.4.11 cycle detection): existing values only, visited under the
    /// read lock (§2.2.13: no borrow escapes it).
    #[cfg_attr(not(test), expect(dead_code, reason = "consumers are §2.4.6 demolition and the on-demand cycle detector"))]
    pub(crate) fn for_each_value(&self, mut f: impl FnMut(&Value)) {
        match self {
            Hash::Perl(arc) => {
                let e = arc.read();
                for b in 0..e.table.num_buckets() {
                    if let Some((_, v)) = e.table.get_bucket(b) {
                        f(v);
                    }
                }
            }

            #[cfg(feature = "indexmap")]
            Hash::Ordered(arc) => {
                for v in arc.read().map.values() {
                    f(v);
                }
            }

            #[cfg(feature = "imbl")]
            Hash::Immutable(arc) => {
                for v in arc.read().map.values() {
                    f(v);
                }
            }
        }
    }
}

impl fmt::Debug for PerlHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PerlHash").field("len", &self.table.len()).finish_non_exhaustive()
    }
}

#[cfg(feature = "indexmap")]
impl fmt::Debug for OrderedHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrderedHash").field("len", &self.map.len()).finish_non_exhaustive()
    }
}

#[cfg(feature = "imbl")]
impl fmt::Debug for ImmutableHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImmutableHash").field("len", &self.map.len()).finish_non_exhaustive()
    }
}

impl Drop for PerlHash {
    /// Iterative teardown (§2.4.9): values route through the release worklist; keys are strings and cannot recurse.
    fn drop(&mut self) {
        for (_key, v) in self.table.drain() {
            release_value(v);
        }
    }
}

#[cfg(feature = "indexmap")]
impl Drop for OrderedHash {
    /// Iterative teardown (§2.4.9), as [`PerlHash`]'s.
    fn drop(&mut self) {
        for (_key, v) in self.map.drain(..) {
            release_value(v);
        }
    }
}

#[cfg(feature = "imbl")]
impl Drop for ImmutableHash {
    /// Iterative teardown (§2.4.9): the parked iterator retires first, then the map drains — co-owner drains net zero
    /// on shared values; the final owner's moves them out.
    fn drop(&mut self) {
        retire_iter(&mut self.iter);
        for (_key, v) in std::mem::take(&mut self.map) {
            release_value(v);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/containers_tests.rs"]
mod tests;
