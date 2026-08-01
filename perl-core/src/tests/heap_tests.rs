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
