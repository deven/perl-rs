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
fn mutable_escape_invalidates_content_caches() {
    // The caches must never outlive the bytes they describe: handing out &mut resets the lattice to its no-knowledge
    // top and the character count to unset, before the caller can write anything.
    let mut cb = CowBuffer::from_slice(b"ascii content here").unwrap();
    cb.narrow_scan(3); // Some terminal knowledge.
    cb.set_char_count(18); // A filled count cache.
    cb.as_mut_slice().unwrap()[0] = 0xFF;
    assert_eq!(cb.scan(), 0, "the lattice must return to UNKNOWN on mutable escape");
    assert_eq!(cb.char_count(), 0, "the count cache must return to unset, never outliving the bytes it counted");
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

// ── Tiered allocations (§2.2.3) ───────────────────────────────

/// The placement rule is meant to be enforced by signatures, not by discipline.  These exercise both shapes through
/// the allocate/retain/release protocol and confirm the small tiers really do carry a refcount and nothing else.
#[test]
fn small_tier_allocations_carry_only_a_refcount() {
    // SAFETY: every pointer below comes from this tier's `allocate` and is released exactly once per handle.
    unsafe {
        let cap: u8 = 200;
        let ptr = heap8::allocate(cap).unwrap();
        assert_eq!(heap8::refcount(ptr), 1);
        assert!(heap8::is_unique(ptr));

        // Writing through the data pointer is the caller's business; the tier only vouches for the room.
        std::ptr::write_bytes(ptr.as_ptr(), b'x', cap as usize);
        assert_eq!(std::slice::from_raw_parts(ptr.as_ptr(), 8), b"xxxxxxxx");

        heap8::retain(ptr);
        assert_eq!(heap8::refcount(ptr), 2);
        assert!(!heap8::is_unique(ptr), "two handles is not unique");

        heap8::release(ptr, cap);
        assert!(heap8::is_unique(ptr), "back to one");
        heap8::release(ptr, cap); // frees
    }
}

#[test]
fn large_tier_allocations_carry_their_own_metadata() {
    // SAFETY: as above; `set_len` and `set_scan` are called while the single handle is held.
    unsafe {
        let cap: u32 = 100_000;
        let ptr = heap32::allocate(cap).unwrap();
        assert_eq!(heap32::capacity(ptr), cap as usize, "capacity is recorded, not passed back in");
        assert_eq!(heap32::len(ptr), 0);
        assert_eq!(heap32::scan(ptr), 0, "UNKNOWN is the zero-initialized state");
        assert_eq!(heap32::char_count(ptr), 0, "no cached count");

        heap32::set_len(ptr, 12_345);
        assert_eq!(heap32::len(ptr), 12_345);
        heap32::set_scan(ptr, 3);
        assert_eq!(heap32::scan(ptr), 3);
        heap32::set_char_count(ptr, 999);
        assert_eq!(heap32::char_count(ptr), 999);

        heap32::retain(ptr);
        heap32::release(ptr); // takes no capacity: this tier knows its own
        assert!(heap32::is_unique(ptr));
        heap32::release(ptr);
    }
}

/// The macro generates four modules, so all four are exercised through the whole protocol — otherwise a tier can be
/// generated wrong and no test notices, which is precisely the failure a table-driven macro invites.
macro_rules! small_tier_protocol {
    ($tier:ident, $cap:expr) => {{
        // SAFETY: the pointer comes from this tier's `allocate` and is released once per handle held.
        unsafe {
            let cap = $cap;
            let ptr = $tier::allocate(cap).unwrap();
            assert_eq!($tier::refcount(ptr), 1, concat!(stringify!($tier), ": born with one handle"));
            assert!($tier::is_unique(ptr));
            $tier::retain(ptr);
            assert_eq!($tier::refcount(ptr), 2);
            assert!(!$tier::is_unique(ptr));
            $tier::release(ptr, cap);
            assert!($tier::is_unique(ptr), concat!(stringify!($tier), ": back to one"));
            $tier::release(ptr, cap);
        }
    }};
}

macro_rules! large_tier_protocol {
    ($tier:ident, $cap:expr) => {{
        // SAFETY: as above; the metadata setters run while the single handle is held.
        unsafe {
            let cap = $cap;
            let ptr = $tier::allocate(cap).unwrap();
            assert_eq!($tier::refcount(ptr), 1);
            assert!($tier::is_unique(ptr));
            assert_eq!($tier::capacity(ptr), cap as usize);
            assert_eq!($tier::len(ptr), 0);
            $tier::set_len(ptr, 7);
            assert_eq!($tier::len(ptr), 7);
            $tier::set_scan(ptr, 2);
            assert_eq!($tier::scan(ptr), 2);
            $tier::set_char_count(ptr, 5);
            assert_eq!($tier::char_count(ptr), 5);
            $tier::retain(ptr);
            assert!(!$tier::is_unique(ptr));
            $tier::release(ptr);
            assert!($tier::is_unique(ptr));
            $tier::release(ptr);
        }
    }};
}

#[test]
fn all_four_tiers_honor_the_whole_protocol() {
    small_tier_protocol!(heap8, 255u8);
    small_tier_protocol!(heap16, 65_535u16);
    large_tier_protocol!(heap32, 4096u32);
    large_tier_protocol!(heapw, 4096usize);
    assert_eq!(heap8::MAX_CAPACITY, 255);
    assert_eq!(heap16::MAX_CAPACITY, 65_535);
    assert_eq!(heap32::MAX_CAPACITY, u32::MAX as usize);
    assert_eq!(heapw::MAX_CAPACITY, usize::MAX);
}

#[test]
fn every_tier_allocates_at_its_ceiling_and_at_zero() {
    // SAFETY: each pointer is from the matching tier's `allocate` and released once.
    unsafe {
        let p = heap8::allocate(0).unwrap();
        assert!(heap8::is_unique(p));
        heap8::release(p, 0);

        let p = heap8::allocate(u8::MAX).unwrap();
        heap8::release(p, u8::MAX);

        let p = heap16::allocate(u16::MAX).unwrap();
        assert_eq!(heap16::refcount(p), 1);
        heap16::release(p, u16::MAX);

        let p = heapw::allocate(4096).unwrap();
        assert_eq!(heapw::capacity(p), 4096);
        heapw::release(p);
    }
}

#[test]
fn an_unsatisfiable_tier_capacity_is_an_error_not_a_panic() {
    // The word tier is the only one whose width can express a request the allocator cannot meet.
    assert!(heapw::allocate(usize::MAX).is_err(), "capacity arithmetic overflow reports as AllocError");
}

#[test]
fn tier_headers_match_the_placement_rule() {
    // Small tiers: a refcount and nothing else.  Large tiers: refcount plus the shared lazily-filled facts.
    assert_eq!(heap8::HEADER, 4);
    assert_eq!(heap16::HEADER, 4);
    // The large tiers pay for what they cache: a refcount plus length, capacity, character count and scan state.
    assert_eq!(heap32::HEADER, 20, "u32 fields, padded to alignment 4");
    assert_eq!(heapw::HEADER, 32, "usize lengths, padded to alignment 8");
    assert_eq!(heap8::MAX_CAPACITY, 255);
    assert_eq!(heap16::MAX_CAPACITY, 65_535);
}

#[test]
fn owned_carries_the_release_obligation_and_nothing_else() {
    // `Owned` exists so that reading a pointer out of a heap variant is a compile error rather than a silent
    // double release — `E0509` fires only for non-`Copy` fields, and a bare `NonNull` would be copied out while
    // the source still dropped.  That property is checked by the compiler, not here; what this pins is that the
    // marker is a capability and not a second authority: it is pointer-sized and knows nothing about the buffer.
    assert_eq!(size_of::<Owned>(), size_of::<std::ptr::NonNull<u8>>(), "no state, no cost");

    // SAFETY: the pointer comes from this tier's `allocate`, is wrapped once, and is released exactly once.
    unsafe {
        let cap = 64u16;
        let raw = heap16::allocate(cap).unwrap();
        let owned = Owned::from_raw(raw);
        assert_eq!(owned.as_ptr(), raw, "reads do not transfer the obligation");
        assert_eq!(heap16::refcount(owned.as_ptr()), 1);

        // Handing the obligation onward must not release: `into_raw` forgets rather than drops.
        let handed_on = owned.into_raw();
        assert_eq!(heap16::refcount(handed_on), 1, "still exactly one outstanding release");
        heap16::release(handed_on, cap);
    }
}
