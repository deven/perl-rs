use super::*;

#[test]
fn the_payloads_are_one_aligned_so_they_clear_the_niche() {
    // Alignment 1 is what the packed representation is for: it lets the struct sit at envelope offset 1, past the byte
    // the discriminant occupies, with the datum landing on an eight-byte boundary at offset 8.
    assert_eq!(align_of::<IntegerPayload>(), 1);
    assert_eq!(align_of::<UnsignedPayload>(), 1);
    assert_eq!(align_of::<FloatPayload>(), 1);
    assert_eq!(size_of::<IntegerPayload>(), 8 + CACHE_BYTES);

    // Integers spend no byte on an exponent, so the same seven bytes hold two more of their digits.
    assert_eq!(IntegerCache::CAPACITY, 12);
    assert_eq!(FloatCache::CAPACITY, 10);
}

#[test]
fn the_datum_round_trips() {
    for n in [0i64, 1, -1, i64::MAX, i64::MIN, 4_294_967_296] {
        assert_eq!(IntegerPayload::new(n).value(), n);
    }

    for u in [0u64, 1, u64::MAX, 9_223_372_036_854_775_808] {
        assert_eq!(UnsignedPayload::new(u).value(), u);
    }

    for f in [0.0f64, -0.0, 3.7, -2.5, f64::MAX, f64::MIN_POSITIVE] {
        assert_eq!(FloatPayload::new(f).value(), f);
    }

    assert!(FloatPayload::new(f64::NAN).value().is_nan());
}

#[test]
fn a_fresh_payload_caches_nothing() {
    assert!(!IntegerPayload::new(42).is_cached());
    assert!(!FloatPayload::new(3.7).is_cached());
    assert_eq!(IntegerCache::EMPTY.count(), None);
    assert_eq!(FloatCache::EMPTY.count(), None);
}

#[test]
fn equality_ignores_the_cache() {
    // The cache is derived, so it cannot make two equal values unequal — the property that lets it be filled lazily and
    // by whichever holder gets there first.
    assert_eq!(IntegerPayload::new(7), IntegerPayload::new(7));
    assert_ne!(IntegerPayload::new(7), IntegerPayload::new(8));
    assert_eq!(UnsignedPayload::new(u64::MAX), UnsignedPayload::new(u64::MAX));
}

#[test]
fn debug_shows_the_datum() {
    assert_eq!(format!("{:?}", IntegerPayload::new(-42)), "-42");
    assert_eq!(format!("{:?}", FloatPayload::new(3.75)), "3.75");
}

// ── Filling and rendering ─────────────────────────────────────────

#[test]
fn a_filled_payload_renders_identically() {
    // The cache's whole obligation: it must be invisible.  A value that has rendered once and kept its digits must
    // produce exactly what recomputing produces, or the cache is a formatting divergence waiting to happen.
    for n in [0i64, 1, -1, 42, -42, 999_999_999_999, -999_999_999_999, 7_000_000] {
        let (bare, filled) = (IntegerPayload::new(n), IntegerPayload::new(n).filled());
        let (mut a, mut b) = (PerlString::empty(), PerlString::empty());
        bare.render(&mut a).unwrap();
        filled.render(&mut b).unwrap();
        assert_eq!(a, b, "rendering of {n} must not depend on the cache");
        assert!(filled.is_cached(), "{n} fits the integer capacity of {}", IntegerCache::CAPACITY);
    }
}

#[test]
fn floats_render_identically_from_the_cache() {
    for f in [0.1f64, 3.7, -2.5, 100.0, 0.0001, 1e15, 1e-5, 1.5e15, 0.3333333333, -0.5] {
        let (bare, filled) = (FloatPayload::new(f), FloatPayload::new(f).filled());
        let (mut a, mut b) = (PerlString::empty(), PerlString::empty());
        bare.render(&mut a).unwrap();
        filled.render(&mut b).unwrap();
        assert_eq!(a, b, "rendering of {f} must not depend on the cache");
    }
}

#[test]
fn long_renderings_are_not_cached_at_all() {
    // All or nothing: a rendering past the capacity keeps no digits, because a prefix cannot be completed correctly.
    // These are the arithmetic artifacts the measured distribution puts at fourteen to fifteen digits.
    for f in [1.0 / 3.0, 2.0f64.sqrt(), 1.0 / 7.0] {
        let filled = FloatPayload::new(f).filled();
        assert!(!filled.is_cached(), "{f} renders past the capacity and must cache nothing");

        // And it still renders correctly, by recomputing.
        let (mut a, mut b) = (PerlString::empty(), PerlString::empty());
        FloatPayload::new(f).render(&mut a).unwrap();
        filled.render(&mut b).unwrap();
        assert_eq!(a, b);
    }

    // Integers past twelve digits likewise.
    let big = IntegerPayload::new(i64::MAX).filled();
    assert!(!big.is_cached(), "nineteen digits exceed the capacity");
}

#[test]
fn specials_and_zero_cache_nothing() {
    // They have no digits to hold, and rendering them is a string push either way.
    for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -0.0] {
        let filled = FloatPayload::new(f).filled();
        assert!(!filled.is_cached());
        let (mut a, mut b) = (PerlString::empty(), PerlString::empty());
        FloatPayload::new(f).render(&mut a).unwrap();
        filled.render(&mut b).unwrap();
        assert_eq!(a, b, "{f} must render the same either way");
    }
}

#[test]
fn unsigned_fills_across_its_range() {
    for u in [0u64, 1, 999_999_999_999, u64::MAX] {
        let (bare, filled) = (UnsignedPayload::new(u), UnsignedPayload::new(u).filled());
        let (mut a, mut b) = (PerlString::empty(), PerlString::empty());
        bare.render(&mut a).unwrap();
        filled.render(&mut b).unwrap();
        assert_eq!(a, b, "rendering of {u} must not depend on the cache");
    }

    assert!(!UnsignedPayload::new(u64::MAX).filled().is_cached(), "twenty digits exceed the capacity");
}
