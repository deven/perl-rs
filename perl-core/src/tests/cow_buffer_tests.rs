use super::*;

// ── Tiered allocations (§2.2.3) ───────────────────────────────

/// The placement rule is meant to be enforced by signatures, not by discipline.  These exercise both shapes through the
/// allocate/retain/release protocol and confirm the small tiers really do carry a refcount and nothing else.
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
    // SAFETY: as above; the setters are called while the single handle is held.
    unsafe {
        let cap: u32 = 100_000;
        let ptr = heap32::allocate(cap, 0, 0).unwrap();
        assert_eq!(heap32::capacity(ptr), cap as usize, "capacity is recorded, not passed back in");
        assert_eq!(heap32::scan(ptr), 0, "UNKNOWN is the zero-initialized state");
        assert_eq!(heap32::char_count(ptr), 0, "no cached count");

        // No length: the envelope owns it (§2.2.3), so the compact header holds only the shared caches.
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
    // The word tier keeps a header length and its allocate takes the birth value — every field written exactly once —
    // where the compact tier records no length at all (§2.2.3).  The signatures differ, so the arms do too.
    ($tier:ident, $cap:expr, len = header) => {{
        // SAFETY: the setters run while the single handle is held.
        unsafe {
            let cap = $cap;
            let ptr = $tier::allocate(cap, 7, 0, 0).unwrap();
            assert_eq!($tier::len(ptr), 7, "the birth write records the length allocate was given");
            $tier::set_len(ptr, 9);
            assert_eq!($tier::len(ptr), 9, "set_len serves the in-place transforms");
            assert_eq!($tier::refcount(ptr), 1);
            assert!($tier::is_unique(ptr));
            assert_eq!($tier::capacity(ptr), cap as usize);
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
    ($tier:ident, $cap:expr) => {{
        // SAFETY: as above; the metadata setters run while the single handle is held.
        unsafe {
            let cap = $cap;
            let ptr = $tier::allocate(cap, 0, 0).unwrap();
            assert_eq!($tier::refcount(ptr), 1);
            assert!($tier::is_unique(ptr));
            assert_eq!($tier::capacity(ptr), cap as usize);
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
    large_tier_protocol!(heap, 4096usize, len = header);
    assert_eq!(heap8::MAX_CAPACITY, 255);
    assert_eq!(heap16::MAX_CAPACITY, 65_535);
    assert_eq!(heap32::MAX_CAPACITY, u32::MAX as usize);
    assert_eq!(heap::MAX_CAPACITY, usize::MAX);
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

        let p = heap::allocate(4096, 0, 0, 0).unwrap();
        assert_eq!(heap::capacity(p), 4096);
        heap::release(p);
    }
}

#[test]
fn an_unsatisfiable_tier_capacity_is_an_error_not_a_panic() {
    // The word tier is the only one whose width can express a request the allocator cannot meet.
    assert!(heap::allocate(usize::MAX, 0, 0, 0).is_err(), "capacity arithmetic overflow reports as AllocError");
}

#[test]
fn tier_headers_match_the_placement_rule() {
    // Small tiers: a refcount and nothing else.  Large tiers: refcount plus the shared lazily-filled facts.
    assert_eq!(heap8::HEADER, 4);
    assert_eq!(heap16::HEADER, 4);

    // The large tiers pay for what they cache.  Heap32 is compact: no length (the envelope owns it, §2.2.3), so a
    // refcount, capacity, count and scan pad to sixteen.  The word tier keeps its length and a word-width count.
    assert_eq!(heap32::HEADER, 16, "u32 fields without a length, padded to alignment 4");
    assert_eq!(heap::HEADER, 40, "usize lengths and a word-width count, padded to alignment 8");
    assert_eq!(heap8::MAX_CAPACITY, 255);
    assert_eq!(heap16::MAX_CAPACITY, 65_535);
}

#[test]
fn owned_carries_the_release_obligation_and_nothing_else() {
    // `Owned` exists so that reading a pointer out of a heap variant is a compile error rather than a silent double
    // release — `E0509` fires only for non-`Copy` fields, and a bare `NonNull` would be copied out while the source
    // still dropped.  That property is checked by the compiler, not here; what this pins is that the marker is a
    // capability and not a second authority: it is pointer-sized and knows nothing about the buffer.
    assert_eq!(size_of::<Owned>(), size_of::<std::ptr::NonNull<u8>>(), "no state, no cost");

    let cap = 64u16;
    let raw = heap16::allocate(cap).unwrap();

    // SAFETY: freshly allocated with one reference, which this `Owned` takes on.
    let owned = unsafe { Owned::from_raw(raw) };
    assert_eq!(owned.as_ptr(), raw, "reads do not transfer the obligation");

    // SAFETY: a live allocation of this tier.
    assert_eq!(unsafe { heap16::refcount(owned.as_ptr()) }, 1);

    // Handing the obligation onward must not release — and it is a safe call, per `Box::into_raw`'s precedent: the only
    // misuse it permits is a leak, which is the bomb's jurisdiction.  Only reconstitution and release need `unsafe`,
    // and the blocks above and below now mark exactly the places UB is possible.
    let handed_on = owned.into_raw();

    // SAFETY: a live allocation, whose one outstanding reference this release consumes.
    unsafe {
        assert_eq!(heap16::refcount(handed_on), 1, "still exactly one outstanding release");
        heap16::release(handed_on, cap);
    }
}

#[test]
fn word_tier_char_count_is_word_width() {
    // Finding 3's regression: a character count can reach the byte length, so the counter must carry the tier's full
    // word width — a narrower field would cache wrong answers past its ceiling.  Probing at the word's own ceiling
    // proves no narrower storage hides inside, on any architecture: on 64-bit targets this exercises far past the u32
    // boundary the defect sat at, and on 32-bit targets the word and u32 coincide, which is the width the tier is then
    // entitled to.  (`u32::MAX as usize + 5` here would itself overflow on 32-bit — the probe must not presume the
    // width it is probing.)  A raw header write suffices: the property is about storage, not about allocating four
    // gigabytes in a test.
    let big = usize::MAX - 3;

    // SAFETY: a live allocation; the setter runs while the single handle is held.
    unsafe {
        let ptr = heap::allocate(64, 0, 0, 0).unwrap();
        heap::set_char_count(ptr, big);
        assert_eq!(heap::char_count(ptr), big, "no truncation below the word's ceiling");
        heap::release(ptr);
    }
}

#[test]
fn concurrent_retain_release_refcount_protocol() {
    // The Arc protocol at the tier level, re-targeted from the dissolved CowBuffer's handle test: racing retains and
    // releases across threads must balance to exactly one owner, and the final release must free (the counter returning
    // to baseline is the freeing's witness).
    let before = live::count();

    // SAFETY: a live allocation; every thread holds a reference across its retain/release pair.
    unsafe {
        let ptr = heap::allocate(64, 0, 0, 0).unwrap();
        let addr = ptr.as_ptr() as usize;
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(move || {
                    let p = std::ptr::NonNull::new(addr as *mut u8).unwrap();
                    for _ in 0..1000 {
                        heap::retain(p);
                        heap::release(p);
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        assert_eq!(heap::refcount(ptr), 1, "every thread's retains and releases balanced");
        heap::release(ptr);
    }

    // The allocation counter is thread-local, so cross-thread retain/release pairs do not disturb this thread's
    // balance: allocate and the final release both happened here.
    assert_eq!(live::count(), before, "the last release freed");
}

#[test]
fn concurrent_scan_narrowing_races_are_benign() {
    // Scan narrowing under races, re-targeted from the dissolved CowBuffer: every stored value is a true fact about
    // immutable-while-shared content, so racing writers can only replace one truth with another — the slot must end
    // holding one of the written values, never a torn or invented byte.
    // SAFETY: a live allocation; set_scan is the atomic store the protocol allows from any handle.
    unsafe {
        let ptr = heap::allocate(16, 0, 0, 0).unwrap();
        let addr = ptr.as_ptr() as usize;
        let threads: Vec<_> = [2u8, 3, 5]
            .into_iter()
            .map(|state| {
                std::thread::spawn(move || {
                    let p = std::ptr::NonNull::new(addr as *mut u8).unwrap();
                    for _ in 0..1000 {
                        heap::set_scan(p, state);
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        assert!(matches!(heap::scan(ptr), 2 | 3 | 5), "the slot holds one of the written states");
        heap::release(ptr);
    }
}

#[test]
fn birth_capacity_is_the_allocator_size_class() {
    // The class is asked, not guessed (§2.2.3): a buffer's birth capacity plus its tier header equals exactly the size
    // class jemalloc names for the birth request, so the headroom is memory the allocation occupied anyway.
    let parts = HeapParts::from_slice(&[0u8; 40], crate::string::scan::Ascii, 40).unwrap();
    assert_eq!(parts.tier, Tier::Heap8);
    assert!(parts.cap >= 40, "capacity covers the content");
    assert_eq!(heap8::HEADER + parts.cap, heap8::HEADER + heap8::class_capacity(40), "born at the class");
    assert!(parts.cap > 40, "the class for header + 40 leaves headroom on every allocator family at this size");

    // The clamp: class headroom never promotes across a tier ceiling.
    let parts = HeapParts::from_slice(&[0u8; 250], crate::string::scan::Ascii, 250).unwrap();
    assert_eq!(parts.tier, Tier::Heap8);
    assert!(parts.cap <= heap8::MAX_CAPACITY, "headroom clamps at the tier ceiling");
}

#[test]
#[cfg(feature = "jemalloc")]
fn buffers_allocate_inside_the_jemalloc_instance() {
    // The -ctl crate reads the same jemalloc our seam allocates from, so a large buffer's birth must move this thread's
    // allocation counter by at least its size — the sanity that the seam really routes there.  The *thread-local*
    // monotonic counter, not the process-global live-bytes statistic: the global number is racy under the parallel test
    // harness, where other threads' frees between two reads can offset any allocation.
    use tikv_jemalloc_ctl::thread;

    let counter = thread::allocatedp::read().unwrap();
    let before = counter.get();
    let parts = HeapParts::from_slice(&[7u8; 100_000], crate::string::scan::Unknown, 0).unwrap();
    assert!(counter.get() >= before + 100_000, "the buffer lives in the instance the counter reads");
    drop(parts);
}

// ── widen_latin1 (§2.7.8) ─────────────────────────────────────
#[test]
fn widen_latin1_streams_the_same_bytes_upgraded_bytes_builds() {
    // The streaming form and the copying form are one implementation now, but the equivalence stays pinned against the
    // reference expansion in case they ever part ways again.
    let cases: [&[u8]; 6] = [b"", b"plain ascii", &[0xE9], &[0xFF, 0x00, 0x80], b"mid\xE9dle then a long ascii tail after the variant", &[0xE9; 200]];
    for bytes in cases {
        let mut reference = Vec::new();
        for &b in bytes {
            if b < 0x80 {
                reference.push(b);
            } else {
                reference.extend_from_slice(&[0xC0 | (b >> 6), 0x80 | (b & 0x3F)]);
            }
        }

        assert_eq!(upgraded_bytes(bytes).unwrap(), reference, "for {bytes:02X?}");

        let mut streamed = Vec::new();
        let Ok(()) = widen_latin1::<std::convert::Infallible>(bytes, |chunk| {
            // Every chunk must be valid UTF-8 on its own: that is the contract the Display sink asserts.
            assert!(std::str::from_utf8(chunk).is_ok(), "chunk {chunk:02X?} is not self-contained UTF-8");
            streamed.extend_from_slice(chunk);
            Ok(())
        });

        assert_eq!(streamed, reference, "for {bytes:02X?}");
    }
}

#[test]
fn widen_latin1_borrows_ascii_runs_from_the_source() {
    // The zero-copy claim, pinned: chunks covering pure-ASCII spans are subslices of the input, not staged copies.
    let bytes = b"a long ascii prefix\xE9and a long ascii suffix after it";
    let range = bytes.as_ptr() as usize..bytes.as_ptr() as usize + bytes.len();
    let mut borrowed = 0;
    let Ok(()) = widen_latin1::<std::convert::Infallible>(bytes, |chunk| {
        if range.contains(&(chunk.as_ptr() as usize)) {
            borrowed += chunk.len();
        }

        Ok(())
    });

    assert_eq!(borrowed, bytes.len() - 1, "every byte but the variant should pass through borrowed");
}

// ── Adopted (§2.2.15, Stage 1) ────────────────────────────────
#[test]
fn adoption_joins_and_release_leaves_the_holders_sharing() {
    let before = live::count();
    let arc: std::sync::Arc<[u8]> = std::sync::Arc::from(&b"shared content"[..]);
    let a = Adopted::adopt_arc_bytes(arc.clone(), ScanState::Ascii).unwrap();

    // SAFETY: `a` is live until the release below, and this handle pins it.
    unsafe {
        assert_eq!(a.as_ref().as_slice(), b"shared content");
        assert_eq!(a.as_ref().total_len(), 14);
        assert_eq!(std::sync::Arc::strong_count(&arc), 2, "adoption joins the Arc's sharing");
        assert_eq!(a.as_ref().refcount(), 1);

        Adopted::release(a);
    }

    assert_eq!(std::sync::Arc::strong_count(&arc), 1, "the last release surrenders the holder");
    assert_eq!(live::count(), before, "the struct's allocation is balanced");
}

#[test]
fn a_span_child_retains_its_parent_and_releases_it_last() {
    let before = live::count();
    let v: Vec<u8> = (0u8..100).collect();
    let parent = Adopted::adopt_vec(v, ScanState::Unknown).unwrap();

    // SAFETY: parent is live; the child's span lies within it; handles pin what they read.
    unsafe {
        let child = Adopted::adopt_span_of(parent, 40, 10, ScanState::Unknown).unwrap();
        assert_eq!(parent.as_ref().refcount(), 2, "the child retains the parent");
        assert_eq!(child.as_ref().as_slice(), &(40u8..50).collect::<Vec<u8>>()[..], "base is pre-resolved to the large offset");

        Adopted::release(parent);
        assert_eq!(child.as_ref().as_slice()[0], 40, "the parent lives while the child holds it");

        Adopted::release(child);
    }

    assert_eq!(live::count(), before, "both structs and the Vec are balanced");
}

#[test]
fn the_shared_slot_narrows_and_never_widens() {
    let a = Adopted::adopt_vec(b"abc".to_vec(), ScanState::Unknown).unwrap();

    // SAFETY: live until the release below.
    unsafe {
        a.as_ref().narrow_scan(ScanState::Ascii);
        assert_eq!(a.as_ref().scan(), ScanState::Ascii);

        a.as_ref().narrow_scan(ScanState::Unknown);
        assert_eq!(a.as_ref().scan(), ScanState::Ascii, "the meet keeps the finer certification");

        assert_eq!(a.as_ref().char_count(), 0, "zero is the unfilled sentinel");
        a.as_ref().set_char_count(3);
        assert_eq!(a.as_ref().char_count(), 3);

        Adopted::release(a);
    }
}

#[test]
fn the_adopted_struct_lands_in_the_recorded_size_class() {
    // The §2.2.15 accounting: 64 bare and 72 with the bytes arm land in jemalloc's 64 and 80 classes.
    #[cfg(not(feature = "bytes"))]
    assert_eq!(alloc_backend::size_class(Layout::new::<Adopted>()), 64);
    #[cfg(feature = "bytes")]
    assert_eq!(alloc_backend::size_class(Layout::new::<Adopted>()), 80);
}

#[cfg(feature = "bytes")]
#[test]
fn bytes_adoption_joins_the_view_and_balances() {
    let before = live::count();
    let b = bytes::Bytes::from_static(b"a static image");
    let a = Adopted::adopt_bytes(b.slice(2..8), ScanState::Ascii).unwrap();

    // SAFETY: live until the release below.
    unsafe {
        assert_eq!(a.as_ref().as_slice(), b"static");
        Adopted::release(a);
    }

    assert_eq!(live::count(), before);
}

#[test]
fn string_and_arc_str_adoption_carry_their_holders_to_the_last_release() {
    let before = live::count();
    let s = Adopted::adopt_string(String::from("owned héllo"), ScanState::ValidUtf8).unwrap();
    let arc: std::sync::Arc<str> = std::sync::Arc::from("shared text");
    let a = Adopted::adopt_arc_str(arc.clone(), ScanState::Ascii).unwrap();

    // SAFETY: both are live until the releases below.
    unsafe {
        assert_eq!(s.as_ref().as_slice(), "owned héllo".as_bytes());
        assert_eq!(a.as_ref().as_slice(), b"shared text");
        assert_eq!(std::sync::Arc::strong_count(&arc), 2);

        Adopted::release(s);
        Adopted::release(a);
    }

    assert_eq!(std::sync::Arc::strong_count(&arc), 1);
    assert_eq!(live::count(), before);
}
