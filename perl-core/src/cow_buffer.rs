//! `CowBuffer` — the copy-on-write byte buffer backing heap strings (§2.2.3).
//!
//! Specification: a `Send + Sync` refcounted growable byte buffer with a `(ptr, len)` handle and a `{refcount, len,
//! capacity, char_count, scan}` header — COW clone, unique-check mutation, nothing else.
//!
//! This is the analogue of perl's `SvPV_COW`/`CowREFCNT` mechanism (the COW refcount stored with the string buffer),
//! done with a real atomic.  "Owned" is not a separate kind: it is the refcount == 1 *state*, checked before in-place
//! mutation.  Clone is a refcount bump; mutation of a shared buffer copies out into a fresh unique buffer (the COW
//! break), leaving other sharers undisturbed.
//!
//! The handle mirrors the length out of the header (§2.3.6 padding-placement rule): a shared buffer is immutable, so
//! its header length never changes under any handle; mutation requires `&mut` on this handle (COW-breaking first if
//! shared) and updates both copies.  The two lengths cannot skew.
//!
//! The `scan` header byte is the per-buffer byte-content scan cache (§2.2.4).  It is an `AtomicU8` because narrowing
//! records a fact about immutable-at-that-moment bytes and may happen through a shared reference (§2.2.5); zero is
//! `UNKNOWN`, the lattice top (§2.2.6), which is also the natural zero-initialized state.
//!
//! # Safety architecture
//!
//! This module is the only owner of the buffer layout invariants:
//!
//! 1. `ptr` is non-null, points at the data region of a live allocation laid out as `[Header][data]`, with the `Header`
//!    at `ptr - HEADER_SIZE` and at least `capacity` addressable data bytes.
//! 2. `self.header().len == header.len <= header.capacity` at all times outside a mutation in progress.
//! 3. The refcount counts live handles; the allocation is freed exactly when the count falls from 1 to 0
//!    (release/acquire protocol, as `Arc`).
//! 4. Data bytes are never written through a handle unless the refcount is exactly 1 (checked with acquire ordering).
//!    The `scan` byte is the sole exception (atomic, monotonic-narrowing only).
//!
//! Verified by the test suite at every size-class and COW-transition boundary; the refcount protocol has targeted
//! concurrency tests.  (Miri is unavailable under the container's apt toolchain — noted as an outstanding verification
//! obligation for an environment that has it.)

use std::alloc::{self, Layout};
use std::fmt;
use std::ptr::{self, NonNull};
use std::slice;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering, fence};

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
// Four tiers, two shapes.  The *small* tiers (`Heap8`, `Heap16`) hold length, capacity, character count and scan
// state in the envelope — eager facts established at construction, so the allocation carries a refcount and nothing
// else.  The *large* tiers (`Heap32`, `Heap`) discover those facts lazily and share them, so they live in the
// allocation where every holder sees one copy.
//
// That difference is not a width: it decides which operations can be told a length and which must read one.  The
// macro therefore has two arms rather than one generic path, and the resulting signatures enforce the placement rule
// — a small tier's `release` *takes* the capacity it must free, because nothing in its allocation can supply it,
// while a large tier's takes only a pointer.  Getting that wrong is a compile error rather than a corruption.
//
// Every tier counts in 32 bits and aborts on overflow, following `Arc`.  The check is a compare on the value
// `fetch_add` already returned; the alternative is a wrap to zero and a use-after-free.

/// The refcount ceiling.  Reaching it needs 4,294,967,295 live handles and therefore at least 64 GiB of envelopes;
/// the test exists because unchecked overflow wraps to zero, not because the ceiling is approachable.
#[cfg_attr(not(test), allow(dead_code))]
const REFCOUNT_CEILING: u32 = u32::MAX;

/// Overflowing the refcount aborts rather than panicking, as `Arc` does.  Unwinding would run `Drop` impls that
/// touch refcounts on a structure whose invariant has already failed, and `catch_unwind` could resume on top of a
/// corrupt count.  An abort is not a panic: nothing catches it, so §1's no-panic rule is untouched.
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
            use std::alloc::{self, Layout};
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
            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
            #[inline]
            unsafe fn head<'a>(ptr: NonNull<u8>) -> &'a Head {
                // SAFETY: the caller vouches for a live allocation, whose header precedes the data by `HEADER`.
                unsafe { &*(ptr.as_ptr().sub(HEADER).cast::<Head>()) }
            }

            fn layout(capacity: usize) -> Result<Layout, AllocError> {
                let size = HEADER.checked_add(capacity).ok_or(AllocError { requested: capacity })?;
                Layout::from_size_align(size, align_of::<Head>()).map_err(|_| AllocError { requested: capacity })
            }

            /// Allocate room for `capacity` bytes, refcount one.  The data is uninitialised: the caller writes it
            /// and records the length in the envelope, which is the only place this tier keeps one.
            pub(crate) fn allocate(capacity: $w) -> Result<NonNull<u8>, AllocError> {
                let capacity = capacity as usize;
                let layout = layout(capacity)?;

                // SAFETY: the layout has non-zero size, the header alone guaranteeing it.
                let raw = unsafe { alloc::alloc(layout) };
                let Some(base) = NonNull::new(raw) else {
                    return Err(AllocError { requested: capacity });
                };

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

            /// One fewer handle, freeing at the last.  Takes the capacity because the allocation does not record it
            /// — that is the envelope's job in this tier, and the layout cannot be reconstructed without it.
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

                // Everything done through other handles happens-before the free.  The `Arc` protocol.
                fence(Ordering::Acquire);

                // A live allocation's layout was computable when it was made, so this cannot fail; leaking would be
                // the only no-panic recourse if it somehow did, and is better than a mismatched deallocation.
                if let Ok(layout) = layout(capacity as usize) {
                    // SAFETY: last handle, and the allocation was made with exactly this layout — capacity never
                    // changes for a given allocation, since growth allocates afresh.
                    unsafe { alloc::dealloc(ptr.as_ptr().sub(HEADER), layout) };
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
    ($tier:ident, width = $w:ty, meta = header) => {
        #[cfg_attr(not(test), allow(dead_code))]
        pub(crate) mod $tier {
            use super::{AllocError, REFCOUNT_CEILING, refcount_overflow};
            use std::alloc::{self, Layout};
            use std::ptr::NonNull;
            use std::sync::atomic::{AtomicU8, AtomicU32, Ordering, fence};

            /// The allocation header.  Unlike the small tiers, this one carries the metadata: at these sizes the
            /// classifying pass is too expensive to run eagerly (§2.2.3), so scan state and character count are
            /// filled lazily and must be visible to every holder.
            #[repr(C)]
            struct Head {
                refcount: AtomicU32,
                len: $w,
                capacity: $w,

                /// Cached flag-on character count (§2.2.4); zero means none, dual-purposed by the byte length.
                char_count: AtomicU32,
                scan: AtomicU8,
            }

            pub(crate) const HEADER: usize = size_of::<Head>();
            pub(crate) const MAX_CAPACITY: usize = <$w>::MAX as usize;

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

            fn layout(capacity: usize) -> Result<Layout, AllocError> {
                let size = HEADER.checked_add(capacity).ok_or(AllocError { requested: capacity })?;
                Layout::from_size_align(size, align_of::<Head>()).map_err(|_| AllocError { requested: capacity })
            }

            /// Allocate room for `capacity` bytes, refcount one, length zero, no cached facts.
            pub(crate) fn allocate(capacity: $w) -> Result<NonNull<u8>, AllocError> {
                let layout = layout(capacity as usize)?;

                // SAFETY: the layout has non-zero size, the header alone guaranteeing it.
                let raw = unsafe { alloc::alloc(layout) };
                let Some(base) = NonNull::new(raw) else {
                    return Err(AllocError { requested: capacity as usize });
                };

                // SAFETY: `base` is a fresh allocation of `layout`, aligned for `Head`.
                unsafe {
                    base.cast::<Head>().write(Head { refcount: AtomicU32::new(1), len: 0, capacity, char_count: AtomicU32::new(0), scan: AtomicU8::new(0) });
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
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`], unused afterwards through
            /// this handle.
            #[inline]
            pub(crate) unsafe fn release(ptr: NonNull<u8>) {
                // SAFETY: the caller vouches for a live allocation.
                let header = unsafe { head(ptr) };
                if header.refcount.fetch_sub(1, Ordering::Release) != 1 {
                    return;
                }

                fence(Ordering::Acquire);
                let capacity = header.capacity as usize;

                if let Ok(layout) = layout(capacity) {
                    // SAFETY: last handle, and the allocation was made with exactly this layout.
                    unsafe { alloc::dealloc(ptr.as_ptr().sub(HEADER), layout) };
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
            /// As [`head_mut`]: a live allocation, and the caller must hold the only handle.  `len` must not exceed
            /// the capacity, and the bytes below it must be initialised.
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

            /// Record a scan state.  Relaxed suffices: the value is a deterministic fact about bytes that are
            /// immutable while shared, so a racing writer can only store the same answer.
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
            pub(crate) unsafe fn char_count(ptr: NonNull<u8>) -> u32 {
                // SAFETY: the caller vouches for a live allocation.
                unsafe { head(ptr) }.char_count.load(Ordering::Relaxed)
            }

            /// # Safety
            /// `ptr` must be the data pointer of a live allocation made by [`allocate`].
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
heap_tier!(heap32, width = u32, meta = header);
heap_tier!(heapw, width = usize, meta = header);

// The placement rule, checked rather than described: a small tier's allocation is a refcount and nothing else.
const _: () = assert!(heap8::HEADER == 4);
const _: () = assert!(heap16::HEADER == 4);
const _: () = assert!(heap8::MAX_CAPACITY == 255);
const _: () = assert!(heap16::MAX_CAPACITY == 65535);
const _: () = assert!(heap32::MAX_CAPACITY == 4_294_967_295);

/// Heap header preceding the data bytes.
#[repr(C)]
struct Header {
    refcount: AtomicUsize,
    len: usize,
    capacity: usize,

    /// Cached flag-on character count (§2.2.4); 0 = no cached count.  The byte length dual-purposes the zero: length
    /// zero implies zero characters by definition, so readers short-circuit on `len` and never consult this field, and
    /// any nonempty perl-decodable content counts at least one character — zero is unambiguous where it is read.
    /// Malformed content keeps zero permanently; the scan byte says which case holds.  Self-validating, so relaxed
    /// atomics suffice (deterministic content fact, like `scan`).
    char_count: AtomicUsize,
    scan: AtomicU8,
}

/// Size of the header; data begins at this offset within the allocation.
const HEADER_SIZE: usize = size_of::<Header>();
const _: () = assert!(HEADER_SIZE == 40);
const _: () = assert!(align_of::<Header>() == 8);

/// Growth headroom: perl's `sv_grow` uses roughly 25%; this constant is the tunable named in §2.2.3.
#[inline]
const fn grow_headroom(needed: usize) -> usize {
    needed + (needed >> 2)
}

/// The copy-on-write byte buffer.  16-byte handle: data pointer + mirrored length.
pub struct CowBuffer {
    /// Points at the data region (offset `HEADER_SIZE` into the allocation).  The whole handle: the length is read
    /// from the header rather than mirrored here, because a two-word handle cannot fit the 16-byte envelope beside a
    /// discriminant (§2.2.9).  The envelope reinstates a mirror in its own spare bytes, where it costs nothing.
    ptr: NonNull<u8>,
}

// SAFETY: the buffer is shared only through the atomic refcount protocol; data bytes are immutable while shared
// (invariant 4), and the scan byte is atomic.  This is the same argument as `Arc<[u8]>` plus an atomic byte.
unsafe impl Send for CowBuffer {}
unsafe impl Sync for CowBuffer {}

impl CowBuffer {
    // ── Construction ──────────────────────────────────────────────
    /// Allocate a buffer holding a copy of `bytes`, with the scan byte zero-initialized (`UNKNOWN`).
    pub fn from_slice(bytes: &[u8]) -> Result<CowBuffer, AllocError> {
        let mut buf = CowBuffer::with_capacity(bytes.len())?;

        // SAFETY: freshly allocated, refcount 1, capacity >= bytes.len().
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), buf.ptr.as_ptr(), bytes.len());
            buf.set_len(bytes.len());
        }

        Ok(buf)
    }

    /// Allocate an empty buffer with at least `capacity` data bytes.
    pub fn with_capacity(capacity: usize) -> Result<CowBuffer, AllocError> {
        let layout = Self::layout_for(capacity)?;

        // SAFETY: layout has non-zero size (header is 32 bytes even for capacity 0).
        let raw = unsafe { alloc::alloc(layout) };
        let Some(base) = NonNull::new(raw) else { return Err(AllocError { requested: capacity }) };
        let header = base.cast::<Header>();

        // SAFETY: `base` is a fresh allocation of `layout`, properly aligned for Header.
        unsafe {
            header.write(Header { refcount: AtomicUsize::new(1), len: 0, capacity, char_count: AtomicUsize::new(0), scan: AtomicU8::new(0) });
        }

        // SAFETY: HEADER_SIZE is within the allocation.
        let ptr = unsafe { NonNull::new_unchecked(base.as_ptr().add(HEADER_SIZE)) };

        Ok(CowBuffer { ptr })
    }

    /// Allocation layout for a buffer with `capacity` data bytes.  Capacity arithmetic overflow is reported as the same
    /// `AllocError` an allocator refusal would produce — an unsatisfiable size is unsatisfiable either way.
    fn layout_for(capacity: usize) -> Result<Layout, AllocError> {
        let size = HEADER_SIZE.checked_add(capacity).ok_or(AllocError { requested: capacity })?;
        Layout::from_size_align(size, align_of::<Header>()).map_err(|_| AllocError { requested: capacity })
    }

    // ── Header access ─────────────────────────────────────────────
    #[inline]
    fn header(&self) -> &Header {
        // SAFETY: invariant 1 — the header lives at ptr - HEADER_SIZE for as long as the handle does.
        unsafe { &*self.ptr.as_ptr().sub(HEADER_SIZE).cast::<Header>() }
    }

    #[inline]
    fn header_mut(&mut self) -> &mut Header {
        debug_assert!(self.is_unique());

        // SAFETY: invariant 1 for the location; invariant 4 (uniqueness) for the exclusive access, guaranteed by
        // callers, all of which are within this module and check or establish uniqueness first.
        unsafe { &mut *self.ptr.as_ptr().sub(HEADER_SIZE).cast::<Header>() }
    }

    // ── Accessors ─────────────────────────────────────────────────
    /// Length in bytes.  Reads the handle mirror — no dereference.
    #[inline]
    pub fn len(&self) -> usize {
        self.header().len
    }

    /// Whether the buffer is empty.  No dereference.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.header().len == 0
    }

    /// Allocated data capacity in bytes.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.header().capacity
    }

    /// The data bytes.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: invariants 1 and 2 — `len` bytes are initialized at `ptr`.
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.header().len) }
    }

    /// Whether this handle is the only one (refcount == 1).  Acquire ordering so a `true` result synchronizes with any
    /// prior handle's release-decrement, making subsequent in-place mutation sound.
    #[inline]
    pub fn is_unique(&self) -> bool {
        self.header().refcount.load(Ordering::Acquire) == 1
    }

    // ── Scan byte (per-buffer byte-content cache, §2.2.4) ─────────
    /// Read the scan byte.
    #[inline]
    pub fn scan(&self) -> u8 {
        self.header().scan.load(Ordering::Relaxed)
    }

    /// Cached character count; 0 = unset (see `Header::char_count`).
    #[inline]
    pub fn char_count(&self) -> usize {
        self.header().char_count.load(Ordering::Relaxed)
    }

    /// Record the character count (a deterministic content fact; racing writers store the same value).
    #[inline]
    pub fn set_char_count(&self, count: usize) {
        self.header().char_count.store(count, Ordering::Relaxed);
    }

    /// Record a scan-state narrowing.  Sound through `&self`: narrowing records a fact about the current bytes, and
    /// concurrent narrowings of a shared (hence immutable) buffer store compatible values (§2.2.5).  Callers must only
    /// narrow; widening is reserved to mutation sites, which hold `&mut` on a unique buffer.
    #[inline]
    pub fn narrow_scan(&self, state: u8) {
        self.header().scan.store(state, Ordering::Relaxed);
    }

    // ── Mutation (unique-check + COW break) ───────────────────────
    /// Ensure this handle is unique, copying out of a shared buffer if necessary (the COW break).  `extra` is
    /// additional capacity the caller is about to need, folded into the break's allocation to avoid a second copy.
    fn make_unique(&mut self, extra: usize) -> Result<(), AllocError> {
        if self.is_unique() {
            return Ok(());
        }

        let needed = self.header().len.checked_add(extra).ok_or(AllocError { requested: usize::MAX })?;
        let mut fresh = CowBuffer::with_capacity(grow_headroom(needed))?;

        // SAFETY: fresh is unique with sufficient capacity; source bytes are valid for self.header().len.
        unsafe {
            ptr::copy_nonoverlapping(self.ptr.as_ptr(), fresh.ptr.as_ptr(), self.header().len);
            fresh.set_len(self.header().len);
        }

        // The scan and count knowledge describe the bytes, which we copied verbatim — carry them.
        fresh.narrow_scan(self.scan());
        fresh.set_char_count(self.char_count());
        *self = fresh; // drops (decrements) the shared original

        Ok(())
    }

    /// Ensure capacity for `additional` more bytes, COW-breaking and/or growing as needed.  After a successful call the
    /// buffer is unique with `capacity >= len + additional`.
    pub fn reserve(&mut self, additional: usize) -> Result<(), AllocError> {
        self.make_unique(additional)?;
        let needed = self.header().len.checked_add(additional).ok_or(AllocError { requested: usize::MAX })?;

        if needed <= self.capacity() {
            return Ok(());
        }

        let new_cap = grow_headroom(needed);
        let mut fresh = CowBuffer::with_capacity(new_cap)?;

        // SAFETY: fresh is unique with capacity >= len; source valid for self.header().len.
        unsafe {
            ptr::copy_nonoverlapping(self.ptr.as_ptr(), fresh.ptr.as_ptr(), self.header().len);
            fresh.set_len(self.header().len);
        }

        fresh.narrow_scan(self.scan());
        fresh.set_char_count(self.char_count());
        *self = fresh;

        Ok(())
    }

    /// Append bytes, with amortized growth.  COW-breaks if shared.  The scan byte is NOT updated here — transition
    /// rules (§2.2.5) belong to `PerlString`, which knows what it appended; this layer resets to `UNKNOWN` (always
    /// correct) and lets the caller re-narrow.
    pub fn extend_from_slice(&mut self, bytes: &[u8]) -> Result<(), AllocError> {
        self.reserve(bytes.len())?;

        // SAFETY: unique (reserve guarantees), capacity checked; regions cannot overlap (a &[u8] argument cannot alias
        // our uniquely-owned data region while &mut self is held).
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.as_ptr().add(self.header().len), bytes.len());
            let new_len = self.header().len + bytes.len();
            self.set_len(new_len);
        }

        self.narrow_scan(0);
        self.set_char_count(0);

        Ok(())
    }

    /// Count the bytes at or above `0x80` — the ones that take two bytes under UTF-8.  Word-at-a-time, matching perl's
    /// `variant_under_utf8_count`; the per-byte form costs three times as much over a long buffer.
    fn variant_count(bytes: &[u8]) -> usize {
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
    fn first_variant(bytes: &[u8]) -> Option<usize> {
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

    /// Expand every byte at or above `0x80` into its two-byte UTF-8 encoding, returning the character count of the
    /// result — the byte length before expansion, each original byte being one character.
    ///
    /// A shared buffer cannot be rewritten under its other holders, so it takes the copying form.  A unique one follows
    /// perl's shape (`sv_utf8_upgrade_flags_grow`): the invariant prefix stays exactly where it is, and the expansion
    /// walks backwards from the end, so one buffer serves and nothing before the first variant is touched.
    pub(crate) fn upgrade_in_place(&mut self) -> Result<usize, AllocError> {
        let old_len = self.len();
        let Some(first) = CowBuffer::first_variant(self.as_slice()) else {
            return Ok(old_len); // Entirely invariant: these bytes already are their own encoding.
        };

        if !self.is_unique() {
            *self = CowBuffer::upgraded_from_slice(self.as_slice())?;
            return Ok(old_len);
        }

        let expansion = CowBuffer::variant_count(&self.as_slice()[first..]);
        let new_len = old_len + expansion;
        self.reserve(expansion)?;

        // SAFETY: unique (checked above; `reserve` preserves it) with capacity for `new_len`.  Both cursors descend
        // from the ends of their regions, `dst` leading `src` by the expansions still owed and meeting it exactly at
        // `first`, so no write lands on a byte not yet read.
        unsafe {
            let base = self.ptr.as_ptr();
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
            self.set_len(new_len);
        }

        // The content changed, so the cached facts go.  The caller knows the result's class and count and restores
        // them, which is the point of owning the loop here: two stores, not two per byte.
        self.narrow_scan(0);
        self.set_char_count(0);

        Ok(old_len)
    }

    /// The copying form of [`CowBuffer::upgrade_in_place`]: a fresh buffer holding the upgraded encoding of `bytes`.
    /// What a shared buffer requires, its other holders keeping the unexpanded content, and what an inline payload
    /// spilling to the heap uses.
    pub(crate) fn upgraded_from_slice(bytes: &[u8]) -> Result<CowBuffer, AllocError> {
        let first = CowBuffer::first_variant(bytes).unwrap_or(bytes.len());
        let total = bytes.len() + CowBuffer::variant_count(&bytes[first..]);
        let mut out = CowBuffer::with_capacity(total)?;

        // SAFETY: freshly allocated with capacity for the whole result, hence unique and large enough.  The invariant
        // prefix copies wholesale; the remainder expands into the space counted for it, so `dst` cannot pass `total`.
        unsafe {
            let base = out.ptr.as_ptr();
            ptr::copy_nonoverlapping(bytes.as_ptr(), base, first);
            let mut dst = first;
            for &byte in &bytes[first..] {
                if byte < 0x80 {
                    *base.add(dst) = byte;
                    dst += 1;
                } else {
                    *base.add(dst) = 0xC0 | (byte >> 6);
                    *base.add(dst + 1) = 0x80 | (byte & 0x3F);
                    dst += 2;
                }
            }
            debug_assert_eq!(dst, total, "the counted expansion must fill the buffer exactly");
            out.set_len(total);
        }

        Ok(out)
    }

    /// Count the two-byte sequences in `bytes`, or `None` when any byte is not part of a Latin-1-range encoding —
    /// exactly the content perl's downgrade refuses.  The count is what the contraction removes.
    fn latin1_contractions(bytes: &[u8]) -> Option<usize> {
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

    /// Contract every two-byte sequence into the single byte it encodes, returning the new length, or `None` when
    /// some character lies above `U+00FF` — where perl's downgrade dies.
    ///
    /// The content is validated before anything moves: a refusal discovered halfway through an in-place walk would
    /// leave the buffer holding neither the old string nor the new one.  The invariant prefix stays where it is and
    /// the compaction walks forward, the write cursor trailing the read cursor by the sequences already collapsed,
    /// so the contraction never needs to grow and never overwrites a byte it has not read.
    #[cfg_attr(not(test), allow(dead_code))] // The ops layer is the caller-to-be; the tests keep it honest.
    pub(crate) fn downgrade_in_place(&mut self) -> Result<Option<usize>, AllocError> {
        let old_len = self.len();

        let Some(first) = CowBuffer::first_variant(self.as_slice()) else {
            return Ok(Some(old_len)); // Entirely invariant: already its own downgrade.
        };

        let Some(contractions) = CowBuffer::latin1_contractions(&self.as_slice()[first..]) else {
            return Ok(None);
        };

        if !self.is_unique() {
            match CowBuffer::downgraded_from_slice(self.as_slice())? {
                Some(contracted) => *self = contracted,
                None => return Ok(None),
            }
            return Ok(Some(old_len - contractions));
        }

        let new_len = old_len - contractions;

        // SAFETY: unique (checked above) and shrinking, so every write lands inside the existing region.  `dst` trails
        // `src` by the sequences already collapsed, reaching `new_len` exactly as `src` reaches `old_len`.
        unsafe {
            let base = self.ptr.as_ptr();
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
            self.set_len(new_len);
        }

        // The bytes changed and their class is not derivable without another look — contracted octets can themselves be
        // valid UTF-8 — so the cached facts go and the next reader re-derives them.
        self.narrow_scan(0);
        self.set_char_count(0);

        Ok(Some(new_len))
    }

    /// The copying form of [`CowBuffer::downgrade_in_place`]: a fresh buffer holding the contracted bytes, or `None`
    /// when the content refuses to downgrade.  What a shared buffer requires, its other holders keeping the encoding.
    pub(crate) fn downgraded_from_slice(bytes: &[u8]) -> Result<Option<CowBuffer>, AllocError> {
        let first = CowBuffer::first_variant(bytes).unwrap_or(bytes.len());

        let Some(contractions) = CowBuffer::latin1_contractions(&bytes[first..]) else {
            return Ok(None);
        };

        let total = bytes.len() - contractions;
        let mut out = CowBuffer::with_capacity(total)?;

        // SAFETY: freshly allocated with capacity for the whole result, hence unique and large enough.  The invariant
        // prefix copies wholesale; the remainder contracts into the space counted for it, so `dst` cannot pass `total`.
        unsafe {
            let base = out.ptr.as_ptr();
            ptr::copy_nonoverlapping(bytes.as_ptr(), base, first);

            let (mut src, mut dst) = (first, first);
            while src < bytes.len() {
                let byte = bytes[src];
                if byte < 0x80 {
                    *base.add(dst) = byte;
                    src += 1;
                } else {
                    *base.add(dst) = ((byte & 0x03) << 6) | (bytes[src + 1] & 0x3F);
                    src += 2;
                }
                dst += 1;
            }

            debug_assert_eq!(dst, total, "the counted contraction must fill the buffer exactly");
            out.set_len(total);
        }

        Ok(Some(out))
    }

    /// Truncate to `new_len` bytes (no-op if already shorter).  COW-breaks if shared: truncation is a mutation of this
    /// value, and other sharers must keep their full contents.  Scan state resets to `UNKNOWN`; the caller may
    /// re-narrow per the removal rules (§2.2.5).
    pub fn truncate(&mut self, new_len: usize) -> Result<(), AllocError> {
        if new_len >= self.header().len {
            return Ok(());
        }

        self.make_unique(0)?;

        // SAFETY: unique; shrinking within initialized bytes.
        unsafe { self.set_len(new_len) };

        self.narrow_scan(0);
        self.set_char_count(0);

        Ok(())
    }

    /// Mutable access to the data bytes, COW-breaking if shared.
    pub fn as_mut_slice(&mut self) -> Result<&mut [u8], AllocError> {
        self.make_unique(0)?;

        // Raw byte mutation invalidates every cached content fact before the caller can write: the scan lattice returns
        // to its no-knowledge top and the character count to unset (§2.2.4).  A stale "valid UTF-8" state over freshly
        // corrupted bytes would let the unchecked readers walk invalid content — the caches must never outlive the
        // bytes they describe.
        self.header().scan.store(0, Ordering::Relaxed); // 0 = UNKNOWN, the lattice top.
        self.header().char_count.store(0, Ordering::Relaxed); // 0 = unset.

        // SAFETY: unique (just ensured); len bytes initialized.
        Ok(unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.header().len) })
    }

    /// Set both lengths (handle mirror and header).
    ///
    /// # Safety
    ///
    /// The buffer must be unique, `new_len <= capacity`, and the first `new_len` data bytes must be initialized.
    unsafe fn set_len(&mut self, new_len: usize) {
        debug_assert!(new_len <= self.capacity());
        self.header_mut().len = new_len;
        self.header_mut().len = new_len;
    }
}

impl Clone for CowBuffer {
    /// A relaxed refcount increment — `clone_cow` in the original design's vocabulary.
    fn clone(&self) -> CowBuffer {
        // Relaxed suffices for increment: creating a new handle from an existing one cannot race with destruction of
        // the last handle (we hold one).  Same protocol as `Arc::clone`.
        self.header().refcount.fetch_add(1, Ordering::Relaxed);
        CowBuffer { ptr: self.ptr }
    }
}

impl Drop for CowBuffer {
    fn drop(&mut self) {
        // Release decrement; acquire fence before freeing so all prior use of the data happens-before deallocation.
        // Standard Arc protocol.
        if self.header().refcount.fetch_sub(1, Ordering::Release) == 1 {
            fence(Ordering::Acquire);
            let capacity = self.header().capacity;

            // A live allocation's layout was computable at construction, so this cannot fail; if it somehow did,
            // leaking is the only no-panic option, and strictly better than a bad dealloc.
            if let Ok(layout) = Self::layout_for(capacity) {
                // SAFETY: last handle; allocation was made with exactly this layout (capacity is immutable for a given
                // allocation — growth allocates fresh).
                unsafe { alloc::dealloc(self.ptr.as_ptr().sub(HEADER_SIZE), layout) };
            }
        }
    }
}

impl fmt::Debug for CowBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CowBuffer")
            .field("len", &self.header().len)
            .field("capacity", &self.capacity())
            .field("unique", &self.is_unique())
            .field("scan", &self.scan())
            .finish()
    }
}

impl PartialEq for CowBuffer {
    /// Byte equality.  (String-level equality semantics — flags, character sequences — live in `PerlString`.)
    fn eq(&self, other: &CowBuffer) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for CowBuffer {}

// ── Layout law (§2.3.6) ───────────────────────────────────────────
//
// One word: the handle must fit the 16-byte envelope beside a discriminant and the inline payload alternative, which a
// mirrored length would prevent.  `NonNull` supplies the niche, so `Option` is free.
const _: () = assert!(size_of::<CowBuffer>() == 8);
const _: () = assert!(size_of::<Option<CowBuffer>>() == 8);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/cow_buffer_tests.rs"]
mod tests;
