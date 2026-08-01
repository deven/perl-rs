//! Shared ownership and teardown: the `HeapArc`/`HeapWeak` façade (§2.4.10, §21.1 step 8) and the mechanical release
//! worklist (§2.4.9).
//!
//! Today these are `repr(transparent)` wrappers over `std::sync::{Arc, Weak}`; at §21.1 step 11 the backend becomes the
//! typed-slab custom implementation (§2.4.2-§2.4.5) and the swap is local to this module, because every graph-bearing
//! strong edge in the crate uses these types and raw `Arc` construction for graph nodes happens nowhere else.
//! `repr(transparent)` preserves every niche, so the §2.3.6 envelope assertions keep verifying the layout rather than
//! trusting it.
//!
//! These types are runtime-internal, not embedder API (§2.4.2): the public contract is the checked handle types
//! (`ScalarRef`, `ArrayRef`, `HashRef`, ...).  The API surface is deliberately the restricted set the eventual custom
//! implementation will provide — no `get_mut`, no `try_unwrap`, no DSTs (§2.4.2's amended contracts).
//!
//! # The release worklist
//!
//! Rust drop glue recurses through deep ownership chains — a linked structure a few tens of thousands of nodes deep
//! dying at once overflows the native stack (measured: SIGABRT, uncatchable).  Perl frees million-deep chains because
//! `sv_clear` defers nested frees to an explicit list.  So do we: when a graph-bearing node dies, its child `Value`s
//! are extracted and drained through this worklist, iteratively, never executing Perl.
//!
//! This worklist is deliberately *not* the §2.4.5 intrusive finalization list: its items are envelope-sized values,
//! not slots (they have no link word to chain through), it needs no interpreter context, and its exception and
//! reentrancy rules differ.
//!
//! The hard discipline (§2.4.9): the worklist is never held locked while a value is being dropped, because that drop
//! may recursively enqueue more work.  Append and pop acquire; the drop itself runs unheld.  The thread-local
//! `parking_lot::Mutex` is uncontended and avoids `RefCell`'s panic-on-reentrant-borrow hazard; the no-lock-during-drop
//! rule is what keeps it deadlock-free.

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

    /// Real strong ownership.  Under the step-11 backend this excludes the hidden finalizer hold (§2.4.4); the std
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

/// A weak reference to a heap-domain node.  Upgrade semantics become state-sensitive (§2.4.4) under the step-11
/// backend; the std façade upgrades on a nonzero strong count.
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

// ── The release worklist (§2.4.9) ─────────────────────────────────
use parking_lot::Mutex;

use crate::value::{ScalarPayload, Value};

struct ReleaseState {
    /// True while this thread's drain loop is running: nested releases append and return.
    draining: bool,
    pending: Vec<Value>,
}

thread_local! {
    static RELEASE: Mutex<ReleaseState> = const { Mutex::new(ReleaseState { draining: false, pending: Vec::new() }) };
}

/// Release a value without unbounded native-stack recursion.
///
/// Non-graph-bearing values drop inline (they cannot recurse).  Graph-bearing values are appended to the per-thread
/// worklist; the outermost call on the thread drains it to empty, so nested node deaths cost constant stack.  Perl
/// observes outermost-first destruction order, which the worklist produces naturally.
pub(crate) fn release_value(value: Value) {
    if !value.carries_strong_edge() {
        return; // Dropped here: no strong edges, no recursion.
    }

    // If the thread-local is gone (Rust TLS teardown), the closure — and the value it captured — is dropped unrun:
    // plain recursive drop glue, the ledgered §2.4.11 fallback posture.
    let _ = RELEASE.try_with(|state| {
        let start_draining = {
            let mut st = state.lock();
            st.pending.push(value);
            if st.draining {
                false
            } else {
                st.draining = true;
                true
            }
        };

        if start_draining {
            loop {
                let next = state.lock().pending.pop();
                match next {
                    // Unheld during the drop: this may re-enter release_value and append.
                    Some(v) => drop(v),
                    None => {
                        state.lock().draining = false;
                        break;
                    }
                }
            }
        }
    });
}

/// Release a payload extracted from a dying cell: the payload→slot-value mapping, then the worklist.
pub(crate) fn release_payload(payload: ScalarPayload) {
    release_value(Value::from_payload(payload));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/heap_tests.rs"]
mod tests;
