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
//!    The `scan` byte is the sole exception (atomic, monotone-narrowing only).
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

/// Heap header preceding the data bytes.
#[repr(C)]
struct Header {
    refcount: AtomicUsize,
    len: usize,
    capacity: usize,

    /// Cached character count under perl semantics (§2.2.4); 0 = unset.  Sound as a sentinel for heap buffers
    /// specifically: heap strings exceed the inline maximum, hence contain at least two characters.  Self-validating,
    /// so relaxed atomics suffice (deterministic content fact, like `scan`).
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
