//! The `HeapArc`/`HeapWeak` façade (§2.4.10, §21.1 step 8).
//!
//! Today these are `repr(transparent)` wrappers over `std::sync::{Arc, Weak}`; at §21.1 step 10 the backend becomes the
//! typed-slab custom implementation (§2.4.2-§2.4.5) and the swap is local to this module, because every graph-bearing
//! strong edge in the crate uses these types and raw `Arc` construction for graph nodes happens nowhere else.
//! `repr(transparent)` preserves every niche, so the §2.3.6 24-byte assertions keep verifying the layout rather than
//! trusting it.
//!
//! These types are runtime-internal, not embedder API (§2.4.2): the public contract is the checked handle types
//! (`ScalarRef`, `ArrayRef`, `HashRef`, ...).  The API surface is deliberately the restricted set the eventual custom
//! implementation will provide — no `get_mut`, no `try_unwrap`, no DSTs (§2.4.2's amended contracts).

use std::sync::{Arc, Weak};

/// A strong reference to a heap-domain node.
#[repr(transparent)]
pub struct HeapArc<T>(Arc<T>);

impl<T> HeapArc<T> {
    pub fn new(value: T) -> HeapArc<T> {
        HeapArc(Arc::new(value))
    }

    /// Reference identity: what `==` on Perl references compares.
    pub fn ptr_eq(a: &HeapArc<T>, b: &HeapArc<T>) -> bool {
        Arc::ptr_eq(&a.0, &b.0)
    }

    /// The node address — the value perl exposes when a reference is numified or stringified.
    pub fn as_ptr(this: &HeapArc<T>) -> *const T {
        Arc::as_ptr(&this.0)
    }

    pub fn downgrade(this: &HeapArc<T>) -> HeapWeak<T> {
        HeapWeak(Arc::downgrade(&this.0))
    }

    /// Real strong ownership.  Under the step-10 backend this excludes the hidden finalizer hold (§2.4.4); the std
    /// façade has no hold, so the counts coincide.
    pub fn strong_count(this: &HeapArc<T>) -> usize {
        Arc::strong_count(&this.0)
    }
}

impl<T> Clone for HeapArc<T> {
    fn clone(&self) -> HeapArc<T> {
        HeapArc(Arc::clone(&self.0))
    }
}

impl<T> std::ops::Deref for HeapArc<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for HeapArc<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A weak reference to a heap-domain node.  Upgrade semantics become state-sensitive (§2.4.4) under the step-10 backend; the std façade upgrades on a nonzero strong count.
#[repr(transparent)]
pub struct HeapWeak<T>(Weak<T>);

impl<T> HeapWeak<T> {
    /// A dangling weak reference that never upgrades (the `Weak::new` analog).
    pub fn dangling() -> HeapWeak<T> {
        HeapWeak(Weak::new())
    }

    pub fn upgrade(&self) -> Option<HeapArc<T>> {
        self.0.upgrade().map(HeapArc)
    }
}

impl<T> Clone for HeapWeak<T> {
    fn clone(&self) -> HeapWeak<T> {
        HeapWeak(Weak::clone(&self.0))
    }
}

// ── Tests ─────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_weak_basics() {
        let a = HeapArc::new(5);
        let b = a.clone();
        assert!(HeapArc::ptr_eq(&a, &b));
        assert!(!HeapArc::ptr_eq(&a, &HeapArc::new(5)));
        assert_eq!(*a, 5, "Deref");
        assert_eq!(HeapArc::strong_count(&a), 2);

        let w = HeapArc::downgrade(&a);
        assert!(HeapArc::ptr_eq(&w.upgrade().unwrap(), &a));
        drop(a);
        drop(b);
        assert!(w.upgrade().is_none(), "dead after the last strong drop");
        assert!(HeapWeak::<i32>::dangling().upgrade().is_none());
    }

    #[test]
    fn transparent_layout() {
        assert_eq!(size_of::<HeapArc<u64>>(), size_of::<usize>());
        assert_eq!(size_of::<Option<HeapArc<u64>>>(), size_of::<usize>(), "niche preserved");
        assert_eq!(size_of::<HeapWeak<u64>>(), size_of::<usize>());
    }
}
