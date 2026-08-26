//! The buffer allocation backend: a jemalloc instance called through its C API (§2.2.3).
//!
//! Behind this seam, the allocator's own contract governs, which is what makes the class harvest legal: `nallocx` names
//! the size class a request would occupy, the allocation then requests exactly that class, and the capacity is ours
//! under either the C or the Rust reading — perl's `malloc_usable_size` harvest, recovered by owning the allocator.
//! Never `#[global_allocator]`: the host application keeps its malloc (§2.7), and this module's jemalloc serves buffer
//! allocations alone.  A future pool-based backend (§2.4) or a C-free system-allocator fallback replaces the internals
//! of these four functions without a caller changing.

use std::alloc::Layout;
use std::ptr::NonNull;

/// The jemalloc backend: the measured default (§2.2.3), where the class is asked through `nallocx`.
#[cfg(feature = "jemalloc")]
mod backend {
    use super::*;
    use tikv_jemalloc_sys as je;

    /// `mallocx` flags for a layout: jemalloc's default alignment covers ≤ 16, and larger alignments are requested
    /// explicitly.  `MALLOCX_ALIGN(a)` is `log2(a)` in the low bits, valid for any power of two, so it is simply always
    /// passed; alignment zero cannot occur (`Layout` forbids it).
    #[inline]
    fn flags(layout: Layout) -> std::ffi::c_int {
        layout.align().trailing_zeros() as std::ffi::c_int
    }

    /// The size class a request of `layout` would occupy: allocating this many bytes costs exactly what allocating
    /// `layout.size()` costs, so the difference is free capacity.  Zero-sized requests are the caller's to avoid, as
    /// with the raw allocator APIs.
    ///
    /// `None` where the allocator declines to name a class, which it reports as zero for a request it cannot serve.
    /// That is not a capacity of nothing — it is the absence of an answer, and a caller that subtracted a header from
    /// it would wrap into an enormous bogus capacity.
    #[inline]
    pub(crate) fn size_class(layout: Layout) -> Option<usize> {
        // SAFETY: `nallocx` computes without allocating; a `Layout` guarantees a power-of-two alignment.
        let class = unsafe { je::nallocx(layout.size(), flags(layout)) };

        (class != 0).then_some(class)
    }

    /// Allocate `layout` from the buffer instance.  Returns `None` on exhaustion; the caller maps that to its own
    /// error.
    #[inline]
    pub(crate) fn allocate(layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: size is nonzero at every call site (headers alone guarantee it), and the alignment is a power of
        // two.
        NonNull::new(unsafe { je::mallocx(layout.size(), flags(layout)) }.cast::<u8>())
    }

    /// Release an allocation made by [`allocate`] with this exact `layout`.
    ///
    /// # Safety
    /// `ptr` must come from [`allocate`] with the same `layout`, not yet released.
    #[inline]
    pub(crate) unsafe fn release(ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller vouches for provenance and layout; `sdallocx` is the sized free.
        unsafe { je::sdallocx(ptr.as_ptr().cast(), layout.size(), flags(layout)) }
    }
}

/// The C-free backend when the `jemalloc` feature is absent: the system allocator, with the class computed rather than
/// asked — the 16-byte quantum every allocator family shares (§2.2.3), which the empirical probes showed is also
/// glibc's true granularity at every heap-served size.
#[cfg(not(feature = "jemalloc"))]
mod backend {
    use super::*;

    /// The size class for `layout`: its size rounded up to the 16-byte quantum.  Conservative by design — a family with
    /// coarser classes wastes nothing on a 16-shaped request, and no family is finer.
    ///
    /// `None` where the rounding itself would overflow, matching the other backend's shape so callers need only one
    /// road for a class that cannot be named.
    #[inline]
    pub(crate) fn size_class(layout: Layout) -> Option<usize> {
        layout.size().checked_add(15).map(|rounded| rounded & !15)
    }

    /// Allocate `layout` from the system allocator.
    #[inline]
    pub(crate) fn allocate(layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: size is nonzero at every call site (headers alone guarantee it).
        NonNull::new(unsafe { std::alloc::alloc(layout) })
    }

    /// Release an allocation made by [`allocate`] with this exact `layout`.
    ///
    /// # Safety
    /// `ptr` must come from [`allocate`] with the same `layout`, not yet released.
    #[inline]
    pub(crate) unsafe fn release(ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller vouches for provenance and layout.
        unsafe { std::alloc::dealloc(ptr.as_ptr(), layout) }
    }
}

pub(crate) use backend::{allocate, release, size_class};
