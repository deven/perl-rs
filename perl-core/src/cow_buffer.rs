//! The tiered copy-on-write heap storage backing heap strings (§2.2.3).
//!
//! Four tiers by content length — `heap8`, `heap16`, `heap32`, `heap` — each a module owning one allocation shape: the
//! small tiers a bare refcount header with every other fact in the string's envelope, `heap32` a compact header whose
//! length is envelope-authoritative, and `heap` a full word-width header.  Around them, the ownership and viewing
//! machinery: `Owned` (the release obligation as a linear token, bomb-armed), `HeapParts` (the owning transport between
//! representations), `HeapView` (the borrowed tier-agnostic read), and the byte-level transform functions the tiers
//! share.
//!
//! This is the analog of perl's `SvPV_COW`/`CowREFCNT` mechanism (the COW refcount stored with the string buffer), done
//! with a real atomic.  "Owned" is the refcount == 1 *state*, checked before in-place mutation.  Clone is a refcount
//! bump; mutation of a shared allocation copies out into a fresh unique one (the COW break), leaving other sharers
//! undisturbed.
//!
//! The large tiers' `scan` header byte is the per-buffer byte-content scan cache (§2.2.4), an `AtomicU8` because
//! narrowing records a fact about immutable-at-that-moment bytes and may happen through a shared reference (§2.2.5);
//! zero is `UNKNOWN`, the lattice top (§2.2.6), which is also the natural zero-initialized state.  The storage byte is
//! [`ScanState`]'s projection, converted back at exactly one seam per direction.
//!
//! # Safety architecture
//!
//! This module is the only owner of the allocation layout invariants:
//!
//! 1. A tier's data pointer is non-null and points at the data region of a live allocation laid out as `[Head][data]`,
//!    with that tier's `Head` immediately below and at least `capacity` addressable data bytes.
//! 2. Envelope and header lengths never exceed the recorded capacity outside a mutation in progress.
//! 3. The refcount counts live owners; the allocation is freed exactly when the count falls from 1 to 0
//!    (release/acquire protocol, as `Arc`).
//! 4. Data bytes are never written unless the refcount is exactly 1 (checked with acquire ordering).  The large tiers'
//!    `scan` byte is the sole exception (atomic, monotonic-narrowing only).
//!
//! Verified by the test suite at every size-class and COW-transition boundary; the refcount protocol has targeted
//! concurrency tests.  (Miri is unavailable under the container's apt toolchain — noted as an outstanding verification
//! obligation for an environment that has it.)

use crate::string::scan::ScanState;
use std::marker::PhantomData;
use std::ptr::{self, NonNull};

/// Allocation failure (or capacity arithmetic overflow, which is the same condition seen earlier).  Surfaces as a
/// `Result` so the runtime can eventually map it to perl's trappable `Out of memory!` die rather than aborting the
/// process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocError {
    /// The data capacity that could not be provided.
    pub requested: usize,
}

// ── Tiered allocations (§2.2.3) ───────────────────────────────────────────────────────────────────────────────────
//
// Four tiers, two shapes.  The *small* tiers (`Heap8`, `Heap16`) hold length, capacity, character count and scan state
// in the envelope — eager facts established at construction, so the allocation carries a refcount and nothing else.
// The *large* tiers (`Heap32`, `Heap`) discover those facts lazily and share them, so they live in the allocation where
// every holder sees one copy.
//
// That difference is not a width: it decides which operations can be told a length and which must read one.  The macro
// therefore has two arms rather than one generic path, and the resulting signatures enforce the placement rule — a
// small tier's `release` *takes* the capacity it must free, because nothing in its allocation can supply it, while a
// large tier's takes only a pointer.  Getting that wrong is a compile error rather than a corruption.
//
// Every tier counts in 32 bits and aborts on overflow, following `Arc`.  The check is a compare on the value
// `fetch_add` already returned; the alternative is a wrap to zero and a use-after-free.

/// The data pointer of a live tiered allocation, and the obligation to release it exactly once.
///
/// Deliberately **not** `Copy`.  Duplicating a data pointer without a matching `retain` is the bug this type exists to
/// make impossible: because `PerlString`'s representation owns a `Drop`, a `Copy` pointer would let a `match` read the
/// pointer out of a heap variant while the source still dropped — a double release the compiler would not diagnose,
/// since `E0509` only fires for non-`Copy` fields.  With this newtype, every such site is a compile error naming the
/// field.
///
/// It carries no state.  Length, capacity, character count and scan state live in the envelope for the small tiers and
/// in the allocation for the large ones (§2.2.3), so this is not a second authority for anything — only a capability.
/// That is the distinction from the `CowBuffer` it replaces, which held both.
#[repr(transparent)]
pub(crate) struct Owned(Option<NonNull<u8>>);

/// Live-allocation accounting, test builds only.
///
/// Second detection layer beneath the bomb: the bomb reports an abandoned `Owned` at its drop site, while these
/// counters catch imbalance that never touches an `Owned` at all — a defect inside a tier's own machinery, or a path
/// that claims a pointer and then loses it raw.  Thread-local, so parallel tests cannot skew one another's balance; the
/// unit tests neither allocate nor release across threads.
#[cfg(test)]
pub(crate) mod live {
    use std::cell::Cell;

    thread_local! {
        static LIVE: Cell<isize> = const { Cell::new(0) };
    }

    /// Record one tier allocation on this thread.
    pub(crate) fn allocated() {
        LIVE.with(|c| c.set(c.get() + 1));
    }

    /// Record one tier release on this thread.
    pub(crate) fn released() {
        LIVE.with(|c| c.set(c.get() - 1));
    }

    /// The number of tier allocations currently live on this thread.
    pub(crate) fn count() -> isize {
        LIVE.with(Cell::get)
    }
}

/// The bomb: an `Owned` dropped while still armed is a leak, reported at its site.
///
/// Rust's ownership is affine — at most once — which is what makes the double release a compile error (`E0509`).  Using
/// the obligation *at least* once is not expressible in the type system, so it is enforced here instead, at the moment
/// of violation: every legitimate consumption goes through [`Owned::claim`], which disarms, and control reaching this
/// `Drop` at all is the defect.  Debug builds only; a leak in a release build stays a leak rather than an abort.
impl Drop for Owned {
    fn drop(&mut self) {
        if cfg!(debug_assertions) && self.0.is_some() && !std::thread::panicking() {
            panic!("Owned dropped while still armed: an allocation leaked without release");
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl Owned {
    /// The raw data pointer, for reads that do not transfer ownership.
    #[inline]
    pub(crate) fn as_ptr(&self) -> NonNull<u8> {
        match self.0 {
            Some(p) => p,

            // Unreachable in correct code: the only disarm sites are the consumption paths, after which the `Owned` is
            // gone or about to be.  Part of the bomb machinery, so it reports rather than misbehaves.
            None => panic!("read of a consumed Owned"),
        }
    }

    /// Claim ownership of a pointer returned by a tier's `allocate`, or one whose refcount this caller has just
    /// incremented.  This is the arming point: the immortal forms never call it, because nothing is ever owed on them —
    /// their variants hold plain `Copy` pointers, which is correct exactly where `E0509`'s protection has nothing to
    /// protect.
    ///
    /// # Safety
    /// The caller must hold a reference count that this `Owned` will consume when released.
    #[inline]
    pub(crate) unsafe fn from_raw(ptr: NonNull<u8>) -> Owned {
        Owned(Some(ptr))
    }

    /// Consume the obligation through `&mut`, disarming the bomb: the caller is now the one who must release (or hand
    /// onward).  Exists for `Drop` implementations, where fields cannot be moved out but are dropped by glue after
    /// `drop` returns — taking the pointer leaves a disarmed shell for the glue to find.
    #[inline]
    pub(crate) fn claim(&mut self) -> NonNull<u8> {
        match self.0.take() {
            Some(p) => p,
            None => panic!("Owned consumed twice"),
        }
    }

    /// Give up the obligation without releasing: the caller now owes exactly one release on the returned pointer, and
    /// this is where the tracking ends — past here, only the test-build allocation counters still see it.
    ///
    /// Safe, deliberately, and by the standard library's own precedent (`Box::into_raw`, `Arc::into_raw`,
    /// `mem::forget`): `unsafe` marks preconditions whose violation causes undefined behavior, and there is nothing
    /// safe code can do with the returned pointer that does — dereferencing, releasing and reconstituting are all
    /// behind their own `unsafe` gates.  Misuse of this function alone is a leak, which is the bomb's and the counters'
    /// jurisdiction, not the keyword's.  Contrast [`Owned::from_raw`], which is genuinely unsafe: it mints a value
    /// whose entirely safe destruction performs a release predicated on the refcount the caller claimed to hold.  UB
    /// enters at reconstitution, never at discharge.
    #[inline]
    pub(crate) fn into_raw(mut self) -> NonNull<u8> {
        self.claim()
    }
}

// SAFETY: the pointee is an allocation whose only shared mutable state is atomic (the refcount, and for the large tiers
// the lazily-filled caches).  Ownership transfer across threads is therefore sound, as it was for `CowBuffer`.
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}

/// Which tier an allocation belongs to.  The tag already encodes it, so this exists for the paths that hold a pointer
/// without its variant — release, growth, and the borrowed view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tier {
    Heap8,
    Heap16,
    Heap32,
    Heap,
}

impl Tier {
    /// The largest content this tier can address.  The growth path is its caller-to-be: growth crosses tiers at the
    /// ceilings (§2.2.3), which is the comparison this answers.
    #[allow(dead_code)]
    pub(crate) const fn max_capacity(self) -> usize {
        match self {
            Tier::Heap8 => heap8::MAX_CAPACITY,
            Tier::Heap16 => heap16::MAX_CAPACITY,
            Tier::Heap32 => heap32::MAX_CAPACITY,
            Tier::Heap => heap::MAX_CAPACITY,
        }
    }

    /// The smallest tier that can hold `len` bytes.  First fit, as the storage ladder is throughout (§2.2.9).
    pub(crate) const fn for_length(len: usize) -> Tier {
        if len <= heap8::MAX_CAPACITY {
            Tier::Heap8
        } else if len <= heap16::MAX_CAPACITY {
            Tier::Heap16
        } else if len <= heap32::MAX_CAPACITY {
            Tier::Heap32
        } else {
            Tier::Heap
        }
    }

    /// Whether this tier keeps its metadata in the envelope rather than the allocation.
    pub(crate) const fn is_small(self) -> bool {
        matches!(self, Tier::Heap8 | Tier::Heap16)
    }
}

/// An owned pointer with the metadata its tier keeps in the envelope, for the paths that move a heap payload between
/// representations.  The large tiers hold their own, so their metadata fields are ignored.
///
/// Owns the release obligation: dropping a `HeapParts` releases the allocation, which is what makes every `?` on a path
/// holding one leak-free without ceremony.  It holds exactly what release needs — the pointer, the capacity, the tier —
/// so it is the natural owner, and [`PerlString`]'s `build_heap` is the one place that takes the obligation onward
/// instead, under `ManuallyDrop`.
pub(crate) struct HeapParts {
    pub(crate) ptr: Owned,
    pub(crate) len: usize,
    pub(crate) cap: usize,
    pub(crate) count: usize,
    pub(crate) scan: ScanState,
    pub(crate) tier: Tier,
}

impl HeapParts {
    /// Allocate the smallest tier that can hold `bytes`, copy them in, and hand back the pointer with whatever metadata
    /// that tier keeps in the envelope.  First fit, as the storage ladder is throughout (§2.2.9).
    ///
    /// The scan state and character count are the caller's facts, passed in and written exactly once at birth: the
    /// small tiers carry them in the envelope, the large tiers in the allocation header, and a caller that has no
    /// classification passes `UNKNOWN` and zero, which are the lazily-filled caches' genuine unfilled states (§2.2.3),
    /// not placeholders to patch.
    pub(crate) fn from_slice(bytes: &[u8], scan: ScanState, count: usize) -> Result<HeapParts, AllocError> {
        let len = bytes.len();
        let tier = Tier::for_length(len);

        // Birth capacity is the allocator's size class for the whole allocation (§2.2.3): requesting the class the
        // request would occupy anyway makes the headroom free, and growth doubles from there.  The tier is chosen by
        // content length; class headroom never promotes a value across a tier (the clamp inside `class_capacity`).
        //
        // SAFETY (each arm): the pointer comes from this tier's `allocate` with room for at least `len`, so the copy
        // stays inside the allocation, and the single reference it carries is handed to the returned `HeapParts`.
        let (ptr, cap) = unsafe {
            match tier {
                Tier::Heap8 => {
                    let cap = heap8::class_capacity(len);
                    let p = heap8::allocate(cap as u8)?;
                    ptr::copy_nonoverlapping(bytes.as_ptr(), p.as_ptr(), len);
                    (p, cap)
                }
                Tier::Heap16 => {
                    let cap = heap16::class_capacity(len);
                    let p = heap16::allocate(cap as u16)?;
                    ptr::copy_nonoverlapping(bytes.as_ptr(), p.as_ptr(), len);
                    (p, cap)
                }
                Tier::Heap32 => {
                    let cap = heap32::class_capacity(len);
                    let p = heap32::allocate(cap as u32, scan.as_u8(), count as u32)?;
                    ptr::copy_nonoverlapping(bytes.as_ptr(), p.as_ptr(), len);
                    (p, cap)
                }
                Tier::Heap => {
                    let cap = heap::class_capacity(len);
                    let p = heap::allocate(cap, len, scan.as_u8(), count)?;
                    ptr::copy_nonoverlapping(bytes.as_ptr(), p.as_ptr(), len);
                    (p, cap)
                }
            }
        };

        // SAFETY: the allocation above carries exactly one reference, which this `Owned` now owes.
        Ok(HeapParts { ptr: unsafe { Owned::from_raw(ptr) }, len, cap, count, scan, tier })
    }
}

/// Rewrite `bytes[first..old_len]` in place as the UTF-8 upgrade of its Latin-1 content, returning the new length.
///
/// Tier-independent: this is byte manipulation inside one allocation, and every tier-specific question — is there room,
/// is the buffer unique, where does the new length get recorded — belongs to the caller, which is the only code that
/// can see the envelope (§2.2.3).
///
/// # Safety
/// `base` must own a live allocation, unique to the caller, with at least `old_len + expansion` bytes of capacity,
/// where `expansion` is [`variant_count`] of the region from `first`.  The first `old_len` bytes must be
/// initialized.
pub(crate) unsafe fn expand_latin1_in_place(base: NonNull<u8>, first: usize, old_len: usize, new_len: usize) {
    // SAFETY: both cursors descend from the ends of their regions, `dst` leading `src` by the expansions still owed and
    // meeting it exactly at `first`, so no write lands on a byte not yet read.  The caller vouches for the room.
    unsafe {
        let base = base.as_ptr();
        let (mut src, mut dst) = (old_len, new_len);
        while src > first {
            src -= 1;
            let byte = *base.add(src);
            if byte < 0x80 {
                dst -= 1;
                *base.add(dst) = byte;
            } else {
                dst -= 2;
                *base.add(dst) = 0xC0 | (byte >> 6);
                *base.add(dst + 1) = 0x80 | (byte & 0x3F);
            }
        }
        debug_assert_eq!(dst, first, "the cursors must meet at the first variant byte");
    }
}

/// Rewrite `bytes[first..old_len]` in place as the Latin-1 contraction of its UTF-8 content.
///
/// Contraction only ever shrinks, so it needs no room and can never leave its tier — which is why, unlike the upgrade,
/// it has no fallback to the copying form on capacity grounds.
///
/// # Safety
/// `base` must own a live allocation, unique to the caller, whose first `old_len` bytes are initialized and whose
/// region from `first` contracts to exactly `new_len` (per [`latin1_contractions`]).
pub(crate) unsafe fn contract_latin1_in_place(base: NonNull<u8>, first: usize, old_len: usize, new_len: usize) {
    // SAFETY: shrinking, so every write lands inside the existing region; `dst` trails `src` by the sequences already
    // collapsed, reaching `new_len` exactly as `src` reaches `old_len`.
    unsafe {
        let base = base.as_ptr();
        let (mut src, mut dst) = (first, first);
        while src < old_len {
            let byte = *base.add(src);
            if byte < 0x80 {
                *base.add(dst) = byte;
                src += 1;
            } else {
                *base.add(dst) = ((byte & 0x03) << 6) | (*base.add(src + 1) & 0x3F);
                src += 2;
            }
            dst += 1;
        }
        debug_assert_eq!(dst, new_len, "the counted contraction must land exactly");
    }
}

impl Drop for HeapParts {
    fn drop(&mut self) {
        let ptr = self.ptr.claim();

        // SAFETY: the parts own exactly one reference on a live allocation of `tier`; the small tiers release by the
        // capacity carried beside the pointer.
        unsafe {
            match self.tier {
                Tier::Heap8 => heap8::release(ptr, self.cap as u8),
                Tier::Heap16 => heap16::release(ptr, self.cap as u16),
                Tier::Heap32 => heap32::release(ptr),
                Tier::Heap => heap::release(ptr),
            }
        }
    }
}

/// A borrowed look at a heap buffer, whatever its tier.
///
/// The four-way dispatch lives here and nowhere else: the metadata is read at construction from whichever place that
/// tier keeps it — the envelope for the small tiers, the allocation for the large — so every reader below is
/// tier-agnostic.  Lazy filling is the exception and is offered only where it applies (§2.2.3): a small tier is scanned
/// eagerly at construction, so it has nothing to fill in later.
pub(crate) struct HeapView<'a> {
    ptr: NonNull<u8>,
    len: usize,
    cap: usize,
    count: usize,
    scan: ScanState,
    tier: Tier,
    _life: PhantomData<&'a Owned>,
}

impl<'a> HeapView<'a> {
    /// A small tier's view: the caller supplies the metadata, because its allocation holds none.
    pub(crate) fn small(ptr: &'a Owned, len: usize, cap: usize, count: usize, scan: ScanState, tier: Tier) -> HeapView<'a> {
        HeapView { ptr: ptr.as_ptr(), len, cap, count, scan, tier, _life: PhantomData }
    }

    /// A large tier's view, read from the allocation.
    ///
    /// # Safety
    /// `ptr` must own a live allocation of `tier`, which must be `Heap32` or `Heap`.
    pub(crate) unsafe fn large(ptr: &'a Owned, tier: Tier) -> HeapView<'a> {
        debug_assert!(matches!(tier, Tier::Heap), "Heap32 views take their envelope length via `heap32`");
        let raw = ptr.as_ptr();

        // SAFETY: the caller vouches for a live allocation of the word tier.
        let (len, cap, count, scan) = unsafe { (heap::len(raw), heap::capacity(raw), heap::char_count(raw), heap::scan(raw)) };

        // The one seam where a storage byte re-enters the type; corruption reports here rather than flowing on.
        HeapView { ptr: raw, len, cap, count, scan: ScanState::from_u8(scan), tier, _life: PhantomData }
    }

    /// A `Heap32` view: the length is envelope-authoritative (§2.2.3), so the caller supplies it and everything else
    /// comes from the compact header.
    ///
    /// # Safety
    /// `ptr` must own a live `Heap32` allocation whose first `len` bytes are initialized.
    pub(crate) unsafe fn heap32(ptr: &'a Owned, len: usize) -> HeapView<'a> {
        let raw = ptr.as_ptr();

        // SAFETY: the caller vouches for a live Heap32 allocation.
        let (cap, count, scan) = unsafe { (heap32::capacity(raw), heap32::char_count(raw), heap32::scan(raw)) };
        HeapView { ptr: raw, len, cap, count: count as usize, scan: ScanState::from_u8(scan), tier: Tier::Heap32, _life: PhantomData }
    }

    /// Content length in bytes.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Whether the content is zero bytes.
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Room in the allocation.  Read by the growth path, which decides between extending in place and moving to the
    /// next tier.
    #[allow(dead_code)]
    pub(crate) fn capacity(&self) -> usize {
        self.cap
    }

    /// Which tier owns the allocation this view borrows.
    #[allow(dead_code)]
    pub(crate) fn tier(&self) -> Tier {
        self.tier
    }

    /// The content bytes.
    pub(crate) fn as_slice(&self) -> &'a [u8] {
        // SAFETY: the view borrows a live allocation whose first `len` bytes are initialized, and `Owned`'s lifetime is
        // threaded through `_life` so the slice cannot outlive it.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// The cached scan state (§2.2.4); zero is `UNKNOWN`, which a small tier never reports since it is classified at
    /// construction.
    pub(crate) fn scan(&self) -> ScanState {
        self.scan
    }

    /// The cached character count; zero means none, dual-purposed by the byte length (§2.2.4).
    pub(crate) fn char_count(&self) -> usize {
        self.count
    }

    /// Whether this is the only handle on the allocation, which is what licenses writing through it.  Unused while
    /// every mutation rebuilds: restoring in-place growth is what brings the COW break back (§2.2.3).
    #[allow(dead_code)]
    pub(crate) fn is_unique(&self) -> bool {
        // SAFETY: the view borrows a live allocation of its tier.
        unsafe {
            match self.tier {
                Tier::Heap8 => heap8::is_unique(self.ptr),
                Tier::Heap16 => heap16::is_unique(self.ptr),
                Tier::Heap32 => heap32::is_unique(self.ptr),
                Tier::Heap => heap::is_unique(self.ptr),
            }
        }
    }

    /// Record a discovered scan state, for the tiers that discover one.  A small tier's is settled at construction and
    /// lives in the envelope, so this is a no-op there rather than an error: the caller learns nothing the value does
    /// not already know.
    pub(crate) fn narrow_scan(&self, state: ScanState) {
        // SAFETY: the view borrows a live allocation of its tier.  The storage byte is the type's projection, so a scan
        // slot can only ever hold a value `ScanState::from_u8` will accept back.
        unsafe {
            match self.tier {
                Tier::Heap32 => heap32::set_scan(self.ptr, state.as_u8()),
                Tier::Heap => heap::set_scan(self.ptr, state.as_u8()),
                Tier::Heap8 | Tier::Heap16 => {}
            }
        }
    }

    /// Record a computed character count, for the tiers that compute one lazily.
    pub(crate) fn set_char_count(&self, count: usize) {
        // SAFETY: the view borrows a live allocation of its tier.
        unsafe {
            match self.tier {
                Tier::Heap32 => heap32::set_char_count(self.ptr, count as u32),
                Tier::Heap => heap::set_char_count(self.ptr, count),
                Tier::Heap8 | Tier::Heap16 => {}
            }
        }
    }
}

/// The refcount ceiling.  Reaching it needs 4,294,967,295 live handles and therefore at least 64 GiB of envelopes; the
/// test exists because unchecked overflow wraps to zero, not because the ceiling is approachable.
#[cfg_attr(not(test), allow(dead_code))]
const REFCOUNT_CEILING: u32 = u32::MAX;

/// Overflowing the refcount aborts rather than panicking, as `Arc` does.  Unwinding would run `Drop` impls that touch
/// refcounts on a structure whose invariant has already failed, and `catch_unwind` could resume on top of a corrupt
/// count.  An abort is not a panic: nothing catches it, so §1's no-panic rule is untouched.
#[cold]
#[inline(never)]
#[cfg_attr(not(test), allow(dead_code))]
fn refcount_overflow() -> ! {
    // `Arc` uses the same escape hatch for the same reason.
    std::process::abort()
}

macro_rules! heap_tier {
    // ── Small tiers: the envelope is authoritative, so the allocation is a bare refcount ──
    ($tier:ident, width = $w:ty, meta = envelope) => {
        // Staged migration: the tiers are exercised by tests until `PerlString`'s variants move over to them.
        #[cfg_attr(not(test), allow(dead_code))]
        pub(crate) mod $tier {
            use super::{AllocError, REFCOUNT_CEILING, refcount_overflow};
            use std::alloc::Layout;

            use crate::alloc_backend;
            use std::ptr::NonNull;
            use std::sync::atomic::{AtomicU32, Ordering, fence};

            /// The whole allocation header: a refcount.  Everything else this tier knows is in the envelope.
            #[repr(C)]
            struct Head {
                refcount: AtomicU32,
            }

            /// Data begins this far into the allocation.
            pub(crate) const HEADER: usize = size_of::<Head>();

            /// The largest content this tier can address, which is what its envelope width can express.
            pub(crate) const MAX_CAPACITY: usize = <$w>::MAX as usize;

            /// The header sits immediately before the data.
            ///

            /// The capacity a buffer of `len` content bytes is born with: the allocator's size class for the whole
            /// allocation, minus this tier's header, clamped to the tier ceiling.  Allocating this capacity requests
            /// exactly the class, so the headroom costs nothing (§2.2.3) — the class is asked, not guessed.
            pub(crate) fn class_capacity(len: usize) -> usize {
                let total = HEADER.saturating_add(len);
                let Ok(layout) = Layout::from_size_align(total, align_of::<Head>()) else {
                    return len;
                };
                (crate::alloc_backend::size_class(layout) - HEADER).min(MAX_CAPACITY)
            }

            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            unsafe fn head<'a>(ptr: NonNull<u8>) -> &'a Head {
                // SAFETY: the caller vouches for a live allocation, whose header precedes the data by `HEADER`.
                unsafe { &*(ptr.as_ptr().sub(HEADER).cast::<Head>()) }
            }

            /// The allocation layout for `capacity` content bytes below this tier's header.
            fn layout(capacity: usize) -> Result<Layout, AllocError> {
                let size = HEADER.checked_add(capacity).ok_or(AllocError { requested: capacity })?;
                Layout::from_size_align(size, align_of::<Head>()).map_err(|_| AllocError { requested: capacity })
            }

            /// Allocate room for `capacity` bytes, refcount one.  The data is uninitialized: the caller writes it and
            /// records the length in the envelope, which is the only place this tier keeps one.
            pub(crate) fn allocate(capacity: $w) -> Result<NonNull<u8>, AllocError> {
                let capacity = capacity as usize;
                let layout = layout(capacity)?;

                let Some(base) = alloc_backend::allocate(layout) else {
                    return Err(AllocError { requested: capacity });
                };
                #[cfg(test)]
                super::live::allocated();

                // SAFETY: `base` is a fresh allocation of `layout`, aligned for `Head`.
                unsafe { base.cast::<Head>().write(Head { refcount: AtomicU32::new(1) }) };

                // SAFETY: `HEADER` is within the allocation by construction.
                Ok(unsafe { NonNull::new_unchecked(base.as_ptr().add(HEADER)) })
            }

            /// One more handle.
            ///
            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            pub(crate) unsafe fn retain(ptr: NonNull<u8>) {
                // SAFETY: the caller vouches for a live allocation.
                let old = unsafe { head(ptr) }.refcount.fetch_add(1, Ordering::Relaxed);
                if old == REFCOUNT_CEILING {
                    refcount_overflow();
                }
            }

            /// One fewer handle, freeing at the last.  Takes the capacity because the allocation does not record it —
            /// that is the envelope's job in this tier, and the layout cannot be reconstructed without it.
            ///
            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`] with exactly `capacity`, and
            /// the caller must not use `ptr` afterwards through this handle.
            #[inline]
            pub(crate) unsafe fn release(ptr: NonNull<u8>, capacity: $w) {
                // SAFETY: the caller vouches for a live allocation.
                if unsafe { head(ptr) }.refcount.fetch_sub(1, Ordering::Release) != 1 {
                    return;
                }

                #[cfg(test)]
                super::live::released();

                // Everything done through other handles happens-before the free.  The `Arc` protocol.
                fence(Ordering::Acquire);

                // A live allocation's layout was computable when it was made, so this cannot fail; leaking would be the
                // only no-panic recourse if it somehow did, and is better than a mismatched deallocation.
                if let Ok(layout) = layout(capacity as usize) {
                    // SAFETY: last handle, and the allocation was made with exactly this layout — capacity never
                    // changes for a given allocation, since growth allocates afresh.
                    unsafe { alloc_backend::release(NonNull::new_unchecked(ptr.as_ptr().sub(HEADER)), layout) };
                }
            }

            /// Whether this is the only handle, which is what licenses mutation in place.
            ///
            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            pub(crate) unsafe fn is_unique(ptr: NonNull<u8>) -> bool {
                // Acquire pairs with the release in `release`, so a buffer seen unique here really is.
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.refcount.load(Ordering::Acquire) == 1
            }

            /// The live handle count, for tests and diagnostics.
            ///
            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            pub(crate) unsafe fn refcount(ptr: NonNull<u8>) -> u32 {
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.refcount.load(Ordering::Relaxed)
            }
        }
    };

    // ── Large tiers: the allocation is authoritative, because the facts are discovered lazily and shared ──
    ($tier:ident, width = $w:ty, counter = $c:ty, meta = header) => {
        #[cfg_attr(not(test), allow(dead_code))]
        pub(crate) mod $tier {
            use super::{AllocError, REFCOUNT_CEILING, refcount_overflow};
            use std::alloc::Layout;

            use crate::alloc_backend;
            use std::ptr::NonNull;

            #[allow(unused_imports)] // Each arm names the counter type it needs; the others go unused.
            use std::sync::atomic::{AtomicU8, AtomicU32, AtomicUsize, Ordering, fence};

            /// The allocation header.  Unlike the small tiers, this one carries the metadata: at these sizes the
            /// classifying pass is too expensive to run eagerly (§2.2.3), so scan state and character count are filled
            /// lazily and must be visible to every holder.
            #[repr(C)]
            struct Head {
                refcount: AtomicU32,
                len: $w,
                capacity: $w,

                /// Cached flag-on character count (§2.2.4) at the tier's own width — a count can reach the byte
                /// length, so a narrower field would cache wrong answers past its ceiling.  Zero means none, dual-
                /// purposed by the byte length.
                char_count: $c,
                scan: AtomicU8,
            }

            pub(crate) const HEADER: usize = size_of::<Head>();
            pub(crate) const MAX_CAPACITY: usize = <$w>::MAX as usize;

            /// The capacity a buffer of `len` content bytes is born with: the allocator's size class for the whole
            /// allocation, minus this tier's header, clamped to the tier ceiling.  Allocating this capacity requests
            /// exactly the class, so the headroom costs nothing (§2.2.3) — the class is asked, not guessed.
            pub(crate) fn class_capacity(len: usize) -> usize {
                let total = HEADER.saturating_add(len);
                let Ok(layout) = Layout::from_size_align(total, align_of::<Head>()) else {
                    return len;
                };
                (crate::alloc_backend::size_class(layout) - HEADER).min(MAX_CAPACITY)
            }

            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            unsafe fn head<'a>(ptr: NonNull<u8>) -> &'a Head {
                // SAFETY: the caller vouches for a live allocation, whose header precedes the data by `HEADER`.
                unsafe { &*(ptr.as_ptr().sub(HEADER).cast::<Head>()) }
            }

            /// # Safety
            /// As [`head`], and the caller must hold the only handle.
            #[inline]
            unsafe fn head_mut<'a>(ptr: NonNull<u8>) -> &'a mut Head {
                // SAFETY: the caller vouches for a live allocation and for uniqueness.
                unsafe { &mut *(ptr.as_ptr().sub(HEADER).cast::<Head>()) }
            }

            /// The allocation layout for `capacity` content bytes below this tier's header.
            fn layout(capacity: usize) -> Result<Layout, AllocError> {
                let size = HEADER.checked_add(capacity).ok_or(AllocError { requested: capacity })?;
                Layout::from_size_align(size, align_of::<Head>()).map_err(|_| AllocError { requested: capacity })
            }

            /// Allocate room for `capacity` bytes holding `len` of content, refcount one, carrying the caller's scan
            /// state and character count — every header field written exactly once at birth.  `set_len` and the cache
            /// setters exist for later change: the in-place transforms and lazy narrowing operate on an existing
            /// allocation, while a fact known at birth arrives here.  A caller without classification passes `UNKNOWN`
            /// and zero, the caches' genuine unfilled states, not placeholders.
            pub(crate) fn allocate(capacity: $w, len: $w, scan: u8, count: $w) -> Result<NonNull<u8>, AllocError> {
                let layout = layout(capacity as usize)?;

                let Some(base) = alloc_backend::allocate(layout) else {
                    return Err(AllocError { requested: capacity as usize });
                };

                #[cfg(test)]
                super::live::allocated();

                // SAFETY: `base` is a fresh allocation of `layout`, aligned for `Head`.
                unsafe {
                    base.cast::<Head>().write(Head { refcount: AtomicU32::new(1), len, capacity, char_count: <$c>::new(count), scan: AtomicU8::new(scan) });
                }

                // SAFETY: `HEADER` is within the allocation by construction.
                Ok(unsafe { NonNull::new_unchecked(base.as_ptr().add(HEADER)) })
            }

            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            pub(crate) unsafe fn retain(ptr: NonNull<u8>) {
                // SAFETY: the caller vouches for a live allocation.
                let old = unsafe { head(ptr) }.refcount.fetch_add(1, Ordering::Relaxed);
                if old == REFCOUNT_CEILING {
                    refcount_overflow();
                }
            }

            /// One fewer handle, freeing at the last.  Takes no capacity: this tier records its own.
            ///
            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`], unused afterwards through this
            /// handle.
            #[inline]
            pub(crate) unsafe fn release(ptr: NonNull<u8>) {
                // SAFETY: the caller vouches for a live allocation.
                let header = unsafe { head(ptr) };
                if header.refcount.fetch_sub(1, Ordering::Release) != 1 {
                    return;
                }

                #[cfg(test)]
                super::live::released();

                fence(Ordering::Acquire);
                let capacity = header.capacity as usize;

                if let Ok(layout) = layout(capacity) {
                    // SAFETY: last handle, and the allocation was made with exactly this layout.
                    unsafe { alloc_backend::release(NonNull::new_unchecked(ptr.as_ptr().sub(HEADER)), layout) };
                }
            }

            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            pub(crate) unsafe fn is_unique(ptr: NonNull<u8>) -> bool {
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.refcount.load(Ordering::Acquire) == 1
            }

            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            pub(crate) unsafe fn refcount(ptr: NonNull<u8>) -> u32 {
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.refcount.load(Ordering::Relaxed)
            }

            /// The authoritative length, which for this tier lives in the allocation.
            ///
            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            pub(crate) unsafe fn len(ptr: NonNull<u8>) -> usize {
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.len as usize
            }

            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            pub(crate) unsafe fn capacity(ptr: NonNull<u8>) -> usize {
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.capacity as usize
            }

            /// Record a new length.
            ///
            /// # Safety
            /// As [`head_mut`]: a live allocation, and the caller must hold the only handle.  `len` must not exceed the
            /// capacity, and the bytes below it must be initialized.
            #[inline]
            pub(crate) unsafe fn set_len(ptr: NonNull<u8>, len: $w) {
                // SAFETY: the caller vouches for a live allocation and for uniqueness.
                unsafe { head_mut(ptr) }.len = len;
            }

            /// The cached scan state (§2.2.4); zero is `UNKNOWN`.
            ///
            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            pub(crate) unsafe fn scan(ptr: NonNull<u8>) -> u8 {
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.scan.load(Ordering::Relaxed)
            }

            /// Record a scan state.  Relaxed suffices: the value is a deterministic fact about bytes that are immutable
            /// while shared, so a racing writer can only store the same answer.
            ///
            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            pub(crate) unsafe fn set_scan(ptr: NonNull<u8>, state: u8) {
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.scan.store(state, Ordering::Relaxed);
            }

            /// The cached character count (§2.2.4); zero means none is cached.
            ///
            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            pub(crate) unsafe fn char_count(ptr: NonNull<u8>) -> $w {
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.char_count.load(Ordering::Relaxed)
            }

            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            pub(crate) unsafe fn set_char_count(ptr: NonNull<u8>, count: $w) {
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.char_count.store(count, Ordering::Relaxed);
            }
        }
    };

    // The compact large tier: metadata in the allocation except the length, which is envelope-authoritative (§2.2.3).
    // A `u32` length rides beside the pointer in the variant at no size cost, and eliminating it here spares both the
    // header bytes and the dereference on every length question — the mirror design's goal, without maintaining two
    // copies of one fact.
    ($tier:ident, width = u32, meta = compact) => {
        pub(crate) mod $tier {
            use super::{AllocError, REFCOUNT_CEILING, refcount_overflow};
            use std::alloc::Layout;

            use crate::alloc_backend;
            use std::ptr::NonNull;

            #[allow(unused_imports)] // Each arm names the counter type it needs; the others go unused.
            use std::sync::atomic::{AtomicU8, AtomicU32, AtomicUsize, Ordering, fence};

            /// The allocation header: refcount, capacity and the two lazily-filled caches.  No length — the envelope
            /// owns it, so shared handles each carry their own, which copy-on-write makes correct: content is immutable
            /// while shared, hence so is length.
            #[repr(C)]
            struct Head {
                refcount: AtomicU32,
                capacity: u32,

                /// Cached flag-on character count (§2.2.4); zero means none, dual-purposed by the byte length.
                char_count: AtomicU32,
                scan: AtomicU8,
            }

            pub(crate) const HEADER: usize = size_of::<Head>();
            pub(crate) const MAX_CAPACITY: usize = u32::MAX as usize;

            /// The capacity a buffer of `len` content bytes is born with: the allocator's size class for the whole
            /// allocation, minus this tier's header, clamped to the tier ceiling.  Allocating this capacity requests
            /// exactly the class, so the headroom costs nothing (§2.2.3) — the class is asked, not guessed.
            pub(crate) fn class_capacity(len: usize) -> usize {
                let total = HEADER.saturating_add(len);
                let Ok(layout) = Layout::from_size_align(total, align_of::<Head>()) else {
                    return len;
                };
                (crate::alloc_backend::size_class(layout) - HEADER).min(MAX_CAPACITY)
            }

            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            unsafe fn head<'a>(ptr: NonNull<u8>) -> &'a Head {
                // SAFETY: the caller vouches for a live allocation, whose header precedes the data by `HEADER`.
                unsafe { &*(ptr.as_ptr().sub(HEADER).cast::<Head>()) }
            }

            /// The allocation layout for `capacity` content bytes below this tier's header.
            fn layout(capacity: usize) -> Result<Layout, AllocError> {
                let size = HEADER.checked_add(capacity).ok_or(AllocError { requested: capacity })?;
                Layout::from_size_align(size, align_of::<Head>()).map_err(|_| AllocError { requested: capacity })
            }

            /// Allocate room for `capacity` bytes, refcount one, carrying the caller's scan state and character count —
            /// every header field written exactly once at birth.  A caller without classification passes `UNKNOWN` and
            /// zero, the lazily-filled caches' genuine unfilled states, later narrowed through
            /// `set_scan`/`set_char_count`.  Length is the caller's to keep: this tier's allocations do not record one.
            pub(crate) fn allocate(capacity: u32, scan: u8, count: u32) -> Result<NonNull<u8>, AllocError> {
                let layout = layout(capacity as usize)?;

                let Some(base) = alloc_backend::allocate(layout) else {
                    return Err(AllocError { requested: capacity as usize });
                };

                #[cfg(test)]
                super::live::allocated();

                // SAFETY: a fresh allocation of at least `HEADER` bytes, aligned for `Head`.
                unsafe {
                    base.cast::<Head>().write(Head { refcount: AtomicU32::new(1), capacity, char_count: AtomicU32::new(count), scan: AtomicU8::new(scan) });
                    Ok(base.add(HEADER))
                }
            }

            /// # Safety
            /// `ptr` must be the data pointer of a live allocation of this tier.
            #[inline]
            pub(crate) unsafe fn retain(ptr: NonNull<u8>) {
                // SAFETY: the caller vouches for a live allocation.
                let prior = unsafe { head(ptr) }.refcount.fetch_add(1, Ordering::Relaxed);
                if prior >= REFCOUNT_CEILING {
                    refcount_overflow();
                }
            }

            /// # Safety
            /// As [`retain`]; consumes one reference, freeing on the last.
            pub(crate) unsafe fn release(ptr: NonNull<u8>) {
                // SAFETY: the caller vouches for a live allocation.
                let header = unsafe { head(ptr) };
                if header.refcount.fetch_sub(1, Ordering::Release) != 1 {
                    return;
                }

                #[cfg(test)]
                super::live::released();

                // Everything done through other handles happens-before the free.  The `Arc` protocol.
                fence(Ordering::Acquire);

                let capacity = header.capacity;

                // A live allocation's layout was computable when it was made, so this cannot fail; leaking would be the
                // only no-panic recourse if it somehow did, and is better than a mismatched deallocation.
                if let Ok(layout) = layout(capacity as usize) {
                    // SAFETY: last handle, and the allocation was made with exactly this layout.
                    unsafe { alloc_backend::release(NonNull::new_unchecked(ptr.as_ptr().sub(HEADER)), layout) };
                }
            }

            /// # Safety
            /// As [`retain`].
            #[inline]
            pub(crate) unsafe fn is_unique(ptr: NonNull<u8>) -> bool {
                // SAFETY: the caller vouches for a live allocation.  Acquire pairs with the Release decrements, so a
                // count of one proves every other handle's writes are visible.
                unsafe { head(ptr) }.refcount.load(Ordering::Acquire) == 1
            }

            /// # Safety
            /// As [`retain`].
            #[cfg_attr(not(test), allow(dead_code))] // The refcount protocol tests are its callers.
            #[inline]
            pub(crate) unsafe fn refcount(ptr: NonNull<u8>) -> u32 {
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.refcount.load(Ordering::Relaxed)
            }

            /// # Safety
            /// As [`retain`].
            #[inline]
            pub(crate) unsafe fn capacity(ptr: NonNull<u8>) -> usize {
                // SAFETY: the caller vouches for a live allocation; capacity is immutable after birth.
                unsafe { head(ptr) }.capacity as usize
            }

            /// # Safety
            /// As [`retain`].
            #[inline]
            pub(crate) unsafe fn scan(ptr: NonNull<u8>) -> u8 {
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.scan.load(Ordering::Relaxed)
            }

            /// Record a discovered scan state.  Relaxed store: every stored value is a true fact about content that is
            /// immutable while shared, so a race can only replace a precise truth with a coarser one (§2.2.4).
            ///
            /// # Safety
            /// As [`retain`].
            #[inline]
            pub(crate) unsafe fn set_scan(ptr: NonNull<u8>, state: u8) {
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.scan.store(state, Ordering::Relaxed);
            }

            /// # Safety
            /// As [`retain`].
            #[inline]
            pub(crate) unsafe fn char_count(ptr: NonNull<u8>) -> u32 {
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.char_count.load(Ordering::Relaxed)
            }

            /// # Safety
            /// As [`retain`].
            #[inline]
            pub(crate) unsafe fn set_char_count(ptr: NonNull<u8>, count: u32) {
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.char_count.store(count, Ordering::Relaxed);
            }
        }
    };
}

heap_tier!(heap8, width = u8, meta = envelope);
heap_tier!(heap16, width = u16, meta = envelope);
heap_tier!(heap32, width = u32, meta = compact);
heap_tier!(heap, width = usize, counter = AtomicUsize, meta = header);

// The placement rule, checked rather than described: a small tier's allocation is a refcount and nothing else.
const _: () = assert!(heap8::HEADER == 4);
const _: () = assert!(heap16::HEADER == 4);
const _: () = assert!(heap8::MAX_CAPACITY == 255);
const _: () = assert!(heap16::MAX_CAPACITY == 65535);
const _: () = assert!(heap32::MAX_CAPACITY == 4_294_967_295);

// ── Byte-level transforms shared by the tiers ─────────────────────
//
// Free functions since the CowBuffer they grew up in dissolved into the tiers (§2.2.3): pure byte analysis and
// rewriting, no allocation protocol.

/// Count the bytes at or above `0x80` — the ones that take two bytes under UTF-8.  Word-at-a-time, matching perl's
/// `variant_under_utf8_count`; the per-byte form costs three times as much over a long buffer.
pub(crate) fn variant_count(bytes: &[u8]) -> usize {
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let mut count = 0u32;
    let mut chunks = bytes.chunks_exact(8);

    for c in &mut chunks {
        count += (u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]) & HIGH).count_ones();
    }

    count as usize + chunks.remainder().iter().filter(|&&b| b >= 0x80).count()
}

/// The offset of the first byte at or above `0x80`, or `None` when every byte is invariant.  Word-at-a-time with an
/// early exit: the invariant prefix needs no rewriting, so finding it cheaply is what lets the upgrade skip it.
pub(crate) fn first_variant(bytes: &[u8]) -> Option<usize> {
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let mut offset = 0;
    let mut chunks = bytes.chunks_exact(8);

    for c in &mut chunks {
        if u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]) & HIGH != 0 {
            break;
        }
        offset += 8;
    }

    bytes[offset..].iter().position(|&b| b >= 0x80).map(|k| offset + k)
}

/// A fresh buffer holding the upgraded encoding of `bytes` — the copying transform.  What a shared buffer requires, its
/// other holders keeping the unexpanded content, and what an inline payload spilling to the heap uses.
pub(crate) fn upgraded_bytes(bytes: &[u8]) -> Result<Vec<u8>, AllocError> {
    let first = first_variant(bytes).unwrap_or(bytes.len());
    let total = bytes.len() + variant_count(&bytes[first..]);

    let mut out = Vec::new();
    out.try_reserve_exact(total).map_err(|_| AllocError { requested: total })?;
    out.extend_from_slice(&bytes[..first]);

    for &byte in &bytes[first..] {
        if byte < 0x80 {
            out.push(byte);
        } else {
            out.push(0xC0 | (byte >> 6));
            out.push(0x80 | (byte & 0x3F));
        }
    }

    debug_assert_eq!(out.len(), total, "the counted expansion must fill the buffer exactly");

    Ok(out)
}

/// Count the two-byte sequences in `bytes`, or `None` when any byte is not part of a Latin-1-range encoding — exactly
/// the content perl's downgrade refuses.  The count is what the contraction removes.
pub(crate) fn latin1_contractions(bytes: &[u8]) -> Option<usize> {
    let (mut count, mut i) = (0usize, 0usize);
    while i < bytes.len() {
        let byte = bytes[i];
        if byte < 0x80 {
            i += 1;
        } else if (byte == 0xC2 || byte == 0xC3) && bytes.get(i + 1).is_some_and(|&c| c & 0xC0 == 0x80) {
            count += 1;
            i += 2;
        } else {
            return None;
        }
    }

    Some(count)
}

/// A fresh buffer holding the contracted bytes, or `None` when the content refuses to downgrade.  What a shared buffer
/// requires, its other holders keeping the encoding.
pub(crate) fn downgraded_bytes(bytes: &[u8]) -> Result<Option<Vec<u8>>, AllocError> {
    let first = first_variant(bytes).unwrap_or(bytes.len());

    let Some(contractions) = latin1_contractions(&bytes[first..]) else {
        return Ok(None);
    };

    let total = bytes.len() - contractions;

    let mut out = Vec::new();
    out.try_reserve_exact(total).map_err(|_| AllocError { requested: total })?;
    out.extend_from_slice(&bytes[..first]);

    let mut src = first;
    while src < bytes.len() {
        let byte = bytes[src];
        if byte < 0x80 {
            out.push(byte);
            src += 1;
        } else {
            out.push(((byte & 0x03) << 6) | (bytes[src + 1] & 0x3F));
            src += 2;
        }
    }

    debug_assert_eq!(out.len(), total, "the counted contraction must fill the buffer exactly");

    Ok(Some(out))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/cow_buffer_tests.rs"]
mod tests;
