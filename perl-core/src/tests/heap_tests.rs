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

// ── The release worklist (§2.4.9) ─────────────────────────────────
use crate::containers::{ArrayRef, HashRef, PerlArray, PerlHash};
use crate::value::{ScalarPayload, Tainted, Value};

// Depths chosen well past the measured failure points: the scalar-ref chain overflowed at 20k in debug builds and the
// array chain at 200k in release builds before §2.4.9.

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
