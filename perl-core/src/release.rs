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

// ── Tests ─────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use crate::containers::{ArrayRef, HashRef, PerlArray, PerlHash};
    use crate::value::{ScalarPayload, Tainted, Value};

    // Depths chosen well past the measured failure points: the scalar-ref chain overflowed at 20k in debug builds and
    // the array chain at 200k in release builds before §2.4.9.

    #[test]
    fn deep_scalar_ref_chain_releases_iteratively() {
        let mut slot = Value::Undef(Tainted::CLEAN);
        for _ in 0..200_000 {
            slot = Value::take_ref(&mut slot);
        }

        drop(slot);
    }

    #[test]
    fn deep_array_chain_releases_iteratively() {
        let mut inner = ArrayRef::new(PerlArray::new());
        for _ in 0..200_000 {
            let outer = ArrayRef::new(PerlArray::new());
            outer.write().push_value(Value::ArrayRef(inner, Tainted::CLEAN)).unwrap();
            inner = outer;
        }

        drop(inner);
    }

    #[test]
    fn deep_hash_chain_releases_iteratively() {
        let mut inner = HashRef::new(PerlHash::new());
        for _ in 0..100_000 {
            let outer = HashRef::new(PerlHash::new());
            outer.write().store("next".parse().unwrap(), Value::HashRef(inner, Tainted::CLEAN)).unwrap();
            inner = outer;
        }

        drop(inner);
    }

    #[test]
    fn deep_mixed_chain_releases_iteratively() {
        // Alternating array → hash → promoted-scalar links: every Drop interception point in one chain.
        let mut link = Value::Int(0, Tainted::CLEAN);
        for i in 0..50_000 {
            link = match i % 3 {
                0 => {
                    let a = ArrayRef::new(PerlArray::new());
                    a.write().push_value(link).unwrap();
                    Value::ArrayRef(a, Tainted::CLEAN)
                }
                1 => {
                    let h = HashRef::new(PerlHash::new());
                    h.write().store("k".parse().unwrap(), link).unwrap();
                    Value::HashRef(h, Tainted::CLEAN)
                }
                _ => {
                    let mut slot = link;
                    Value::take_ref(&mut slot)
                    // The promoted slot (the aliased cell) dies here; the ref value carries the chain.
                }
            };
        }

        drop(link);
    }

    #[test]
    fn assignment_over_a_deep_chain_releases_iteratively() {
        // The assign choke point: the old payload dies inside ScalarCell::assign, not a plain drop.
        let mut slot = Value::Undef(Tainted::CLEAN);
        for _ in 0..100_000 {
            slot = Value::take_ref(&mut slot);
        }

        let r = Value::take_ref(&mut slot);
        let view = r.deref_scalar().unwrap();
        view.write().unwrap().assign(ScalarPayload::Int(1, Tainted::CLEAN)).unwrap();
        assert_eq!(slot.to_int(), 1, "the chain died; the slot lives on with the new payload");
    }

    #[test]
    fn container_clear_releases_iteratively() {
        let a = ArrayRef::new(PerlArray::new());
        let mut chain = Value::Undef(Tainted::CLEAN);
        for _ in 0..100_000 {
            chain = Value::take_ref(&mut chain);
        }

        a.write().push_value(chain).unwrap();
        a.write().clear().unwrap();
        assert!(a.read().is_empty());
    }
}
