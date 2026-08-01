//! Mechanical iterative teardown (§2.4.9): the per-thread release context.
//!
//! Rust drop glue recurses through deep ownership chains — a linked structure a few tens of thousands of nodes deep
//! dying at once overflows the native stack (measured: SIGABRT, uncatchable).  Perl frees million-deep chains because
//! `sv_clear` defers nested frees to an explicit list.  So do we: when a graph-bearing node dies, its child `Value`s
//! are extracted and drained through this worklist, iteratively, never executing Perl.
//!
//! This worklist is deliberately *not* the §2.4.5 intrusive finalization list: its items are 24-byte values, not slots
//! (they have no link word to chain through), it needs no interpreter context, and its exception and reentrancy rules
//! differ.
//!
//! The hard discipline (§2.4.9): the worklist is never held locked while a value is being dropped, because that drop
//! may recursively enqueue more work.  Append and pop acquire; the drop itself runs unheld.  The thread-local
//! `parking_lot::Mutex` is uncontended and avoids `RefCell`'s panic-on-reentrant-borrow hazard; the no-lock-during-drop
//! rule is what keeps it deadlock-free.

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
#[path = "tests/release_tests.rs"]
mod tests;
