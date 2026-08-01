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
