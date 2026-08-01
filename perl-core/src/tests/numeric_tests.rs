use super::*;

#[test]
fn the_payloads_are_one_aligned_so_they_clear_the_niche() {
    // Alignment 1 is what the packed representation is for: it lets the struct sit at envelope offset 1, past the byte
    // the discriminant occupies, with the datum landing on an eight-byte boundary at offset 8.
    assert_eq!(align_of::<IntegerPayload>(), 1);
    assert_eq!(align_of::<UnsignedPayload>(), 1);
    assert_eq!(align_of::<FloatPayload>(), 1);
    assert_eq!(size_of::<IntegerPayload>(), 8 + CACHE_BYTES);
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
    assert_eq!(DigitCache::EMPTY.count(), None);
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
