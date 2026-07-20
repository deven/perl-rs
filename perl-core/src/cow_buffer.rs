//! `CowBuffer` — the copy-on-write byte buffer backing heap strings (§2.2.3).
//!
//! Specification: a `Send + Sync` refcounted growable byte buffer with a `(ptr, len)` handle and a
//! `{refcount, len, capacity, char_count, scan}` header — COW clone, unique-check mutation, nothing else.
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
//! 2. `self.len == header.len <= header.capacity` at all times outside a mutation in progress.
//! 3. The refcount counts live handles; the allocation is freed exactly when the count falls from 1 to 0
//!    (release/acquire protocol, as `Arc`).
//! 4. Data bytes are never written through a handle unless the refcount is exactly 1 (checked with acquire ordering).
//!    The `scan` byte is the sole exception (atomic, monotone-narrowing only).
//!
//! Verified by the test suite at every size-class and COW-transition boundary; the refcount protocol has targeted
//! concurrency tests.  (Miri is unavailable under the container's apt toolchain — noted as an outstanding
//! verification obligation for an environment that has it.)

use std::alloc::{self, Layout};
use std::ptr::NonNull;
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
    /// Points at the data region (offset `HEADER_SIZE` into the allocation).
    ptr: NonNull<u8>,
    /// Mirrored from the header (coherent by COW; see module docs).
    len: usize,
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
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.ptr.as_ptr(), bytes.len());
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

        Ok(CowBuffer { ptr, len: 0 })
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
        self.len
    }

    /// Whether the buffer is empty.  No dereference.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
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
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
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

        let needed = self.len.checked_add(extra).ok_or(AllocError { requested: usize::MAX })?;
        let mut fresh = CowBuffer::with_capacity(grow_headroom(needed))?;

        // SAFETY: fresh is unique with sufficient capacity; source bytes are valid for self.len.
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr.as_ptr(), fresh.ptr.as_ptr(), self.len);
            fresh.set_len(self.len);
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
        let needed = self.len.checked_add(additional).ok_or(AllocError { requested: usize::MAX })?;

        if needed <= self.capacity() {
            return Ok(());
        }

        let new_cap = grow_headroom(needed);
        let mut fresh = CowBuffer::with_capacity(new_cap)?;

        // SAFETY: fresh is unique with capacity >= len; source valid for self.len.
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr.as_ptr(), fresh.ptr.as_ptr(), self.len);
            fresh.set_len(self.len);
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
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.as_ptr().add(self.len), bytes.len());
            let new_len = self.len + bytes.len();
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
        if new_len >= self.len {
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

        // SAFETY: unique (just ensured); len bytes initialized.
        Ok(unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) })
    }

    /// Set both lengths (handle mirror and header).
    ///
    /// # Safety
    ///
    /// The buffer must be unique, `new_len <= capacity`, and the first `new_len` data bytes must be initialized.
    unsafe fn set_len(&mut self, new_len: usize) {
        debug_assert!(new_len <= self.capacity());
        self.header_mut().len = new_len;
        self.len = new_len;
    }
}

impl Clone for CowBuffer {
    /// A relaxed refcount increment — `clone_cow` in the original design's vocabulary.
    fn clone(&self) -> CowBuffer {
        // Relaxed suffices for increment: creating a new handle from an existing one cannot race with destruction of
        // the last handle (we hold one).  Same protocol as `Arc::clone`.
        self.header().refcount.fetch_add(1, Ordering::Relaxed);
        CowBuffer { ptr: self.ptr, len: self.len }
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

impl std::fmt::Debug for CowBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CowBuffer")
            .field("len", &self.len)
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
const _: () = assert!(size_of::<CowBuffer>() == 16);
const _: () = assert!(size_of::<Option<CowBuffer>>() == 16);

// ── Tests ─────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_slice_round_trip() {
        let b = CowBuffer::from_slice(b"hello").unwrap();
        assert_eq!(b.as_slice(), b"hello");
        assert_eq!(b.len(), 5);
        assert!(b.is_unique());
        assert_eq!(b.scan(), 0); // UNKNOWN at birth
    }

    #[test]
    fn empty_buffer() {
        let b = CowBuffer::from_slice(b"").unwrap();
        assert!(b.is_empty());
        assert_eq!(b.as_slice(), b"");

        // Header-only allocation is legal and freeable (exercised by drop).
    }

    #[test]
    fn clone_shares_and_drop_releases() {
        let a = CowBuffer::from_slice(b"shared").unwrap();
        let b = a.clone();
        assert!(!a.is_unique());
        assert!(!b.is_unique());
        assert_eq!(a.as_slice(), b.as_slice());
        drop(b);
        assert!(a.is_unique());
    }

    #[test]
    fn handle_len_mirror_matches_header() {
        let mut a = CowBuffer::from_slice(b"abc").unwrap();
        assert_eq!(a.len(), a.header().len);
        a.extend_from_slice(b"def").unwrap();
        assert_eq!(a.len(), 6);
        assert_eq!(a.len(), a.header().len);
        let b = a.clone();
        assert_eq!(b.len(), b.header().len);
    }

    #[test]
    fn unique_append_is_in_place_within_capacity() {
        let mut a = CowBuffer::with_capacity(16).unwrap();
        a.extend_from_slice(b"1234").unwrap();
        let p = a.as_slice().as_ptr();
        a.extend_from_slice(b"5678").unwrap();
        assert_eq!(a.as_slice(), b"12345678");
        assert_eq!(a.as_slice().as_ptr(), p, "in-place append must not reallocate within capacity");
    }

    #[test]
    fn growth_reallocates_with_headroom() {
        let mut a = CowBuffer::with_capacity(4).unwrap();
        a.extend_from_slice(b"1234").unwrap();
        a.extend_from_slice(b"5").unwrap(); // exceeds capacity 4
        assert_eq!(a.as_slice(), b"12345");
        assert!(a.capacity() >= grow_headroom(5), "growth must include headroom");
    }

    #[test]
    fn cow_break_on_shared_append_leaves_sharer_intact() {
        let mut a = CowBuffer::from_slice(b"base").unwrap();
        let b = a.clone();
        a.extend_from_slice(b"+more").unwrap();
        assert_eq!(a.as_slice(), b"base+more");
        assert_eq!(b.as_slice(), b"base", "COW break must not disturb other sharers");
        assert!(a.is_unique());
        assert!(b.is_unique());
    }

    #[test]
    fn cow_break_on_shared_truncate_leaves_sharer_intact() {
        let mut a = CowBuffer::from_slice(b"abcdef").unwrap();
        let b = a.clone();
        a.truncate(3).unwrap();
        assert_eq!(a.as_slice(), b"abc");
        assert_eq!(b.as_slice(), b"abcdef");
    }

    #[test]
    fn truncate_syncs_both_lengths() {
        let mut a = CowBuffer::from_slice(b"abcdef").unwrap();
        a.truncate(2).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a.len(), a.header().len);
        a.truncate(5).unwrap(); // no-op: already shorter
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn as_mut_slice_cow_breaks() {
        let mut a = CowBuffer::from_slice(b"xyz").unwrap();
        let b = a.clone();
        a.as_mut_slice().unwrap()[0] = b'X';
        assert_eq!(a.as_slice(), b"Xyz");
        assert_eq!(b.as_slice(), b"xyz");
    }

    #[test]
    fn scan_narrowing_is_visible_to_sharers() {
        let a = CowBuffer::from_slice(b"ascii").unwrap();
        let b = a.clone();
        a.narrow_scan(3); // some terminal state
        assert_eq!(b.scan(), 3, "per-buffer scan knowledge must be shared");
    }

    #[test]
    fn cow_break_carries_scan_knowledge() {
        let mut a = CowBuffer::from_slice(b"data").unwrap();
        a.narrow_scan(3);
        let b = a.clone();
        a.extend_from_slice(b"!").unwrap(); // COW break + mutation resets a's scan
        assert_eq!(a.scan(), 0, "mutation resets to UNKNOWN");
        assert_eq!(b.scan(), 3, "sharer's buffer keeps its knowledge");
    }

    #[test]
    fn mutation_resets_scan_to_unknown() {
        let mut a = CowBuffer::from_slice(b"abc").unwrap();
        a.narrow_scan(3);
        a.extend_from_slice(b"d").unwrap();
        assert_eq!(a.scan(), 0);
        a.narrow_scan(3);
        a.truncate(1).unwrap();
        assert_eq!(a.scan(), 0);
    }

    #[test]
    fn size_class_boundaries() {
        // Exercise construction/append/drop across a spread of sizes including the header-only case, small sizes, and
        // around typical allocator size classes.
        for n in [0usize, 1, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 4095, 4096, 4097] {
            let payload = vec![0xABu8; n];
            let mut b = CowBuffer::from_slice(&payload).unwrap();
            assert_eq!(b.len(), n);
            assert_eq!(b.as_slice(), &payload[..]);
            b.extend_from_slice(b"tail").unwrap();
            assert_eq!(b.len(), n + 4);
            assert_eq!(&b.as_slice()[n..], b"tail");
        }
    }

    #[test]
    fn unsatisfiable_capacity_is_an_error_not_a_panic() {
        let e = CowBuffer::with_capacity(usize::MAX);
        assert!(matches!(e, Err(AllocError { requested: usize::MAX })));
        let e2 = CowBuffer::with_capacity(usize::MAX - HEADER_SIZE + 1);
        assert!(e2.is_err());
    }

    #[test]
    fn concurrent_clone_drop_refcount_protocol() {
        use std::sync::Arc as StdArc;
        let base = CowBuffer::from_slice(b"contended").unwrap();
        let shared = StdArc::new(base);
        let mut handles = Vec::new();
        for _ in 0..8 {
            let s = StdArc::clone(&shared);
            handles.push(std::thread::spawn(move || {
                for _ in 0..10_000 {
                    let c = (*s).clone();
                    assert_eq!(c.as_slice(), b"contended");
                    drop(c);
                }
            }));
        }

        for h in handles {
            assert!(h.join().is_ok());
        }

        drop(shared);

        // If the refcount protocol is wrong, this test aborts, double-frees, or leaks under sanitizers; under plain
        // execution it at minimum exercises the contended increment/decrement paths.
    }

    #[test]
    fn concurrent_scan_narrowing_races_are_benign() {
        use std::sync::Arc as StdArc;
        let b = StdArc::new(CowBuffer::from_slice(b"immutable while shared").unwrap());
        let mut handles = Vec::new();
        for _ in 0..4 {
            let s = StdArc::clone(&b);
            handles.push(std::thread::spawn(move || {
                for _ in 0..10_000 {
                    s.narrow_scan(3); // all racers narrow to the same terminal state
                    assert_eq!(s.scan(), 3);
                }
            }));
        }

        for h in handles {
            assert!(h.join().is_ok());
        }
    }
}
