use super::*;

fn roundtrip(s: &[u8]) -> Packed {
    let p = pack(s).unwrap();
    let (out, len) = p.unpack();
    assert_eq!(&out[..len], s, "round-trip must be exact: {:?}", String::from_utf8_lossy(s));
    assert_eq!(p.len(), s.len(), "derived length must match: {:?}", String::from_utf8_lossy(s));
    p
}

#[test]
fn exact_round_trip_across_the_class() {
    // The tier's real citizens: %.15g output at full width, i64 extremes, timestamps, dotted addresses.
    for s in [
        &b"0.333333333333333"[..],
        b"-2.22507385850720e-308",
        b"1.7976931348623157e+308",
        b"9223372036854775807",
        b"-9223372036854775808",
        b"1.000000E+00 1E+100",
        b"2026-07-28T14:33:07Z",
        b"2026-07-28T14:33:07.123Z",
        b"2026-07-28 14:33:07",
        b"192.168.100.200 1.2.3",
        b"12:34:56 12:34:57",
    ] {
        roundtrip(s);
    }
}

#[test]
fn interior_spaces_pack_and_trailing_spaces_do_not() {
    // Interior spaces are nibble 0 and decode back to spaces.
    for s in [&b"555 1234 555 1234"[..], b" 1 234 567 890 12", b"2026-07-28 14:33:07Z", b"2026-07-28 14:33:07+05:00", b"1 2 3 4 5 6 7 8 9"] {
        roundtrip(s);
    }

    // Trailing spaces are unpackable: the final space cannot be told from padding.
    assert_eq!(pack(b"2026-07-28T14:33:07 "), None);
    assert_eq!(pack(b"555 1234 555 1234 "), None);
    assert_eq!(pack(b"1 2 3 4 5 6 7 8   "), None);
}

#[test]
fn iso_timestamp_grammar_is_covered() {
    // Both offset spellings, both date-time separators, all three alphabets.
    for s in [
        &b"2026-07-28T14:33:07Z"[..],
        b"2026-07-28T14:33:07+05:00",
        b"2026-07-28T14:33:07-05:00",
        b"2026-07-28 14:33:07+00:00",
        b"2026-07-28T14:33:07.123456Z",
        b"2026-07-28T14:33:07.12+05:00",
        b"20260728T143307Z 1234",
        b"2026-07-28 000000",
    ] {
        roundtrip(s);
    }

    // The capacity boundary, exactly: Zulu leaves room for nine fractional digits — full nanoseconds — and a
    // numeric offset for three; millisecond-plus-offset (29) and nanosecond-Zulu (30) both fit.
    assert_eq!(b"2026-07-29T17:23:45.123456789Z".len(), MAX_PACKED_LEN);
    roundtrip(b"2026-07-29T17:23:45.123456789Z");
    roundtrip(b"2026-07-29 17:23:45.123-04:00"); // 29: millisecond precision with a numeric offset.

    // A 31-character timestamp is the selector's business, not this tier's: see the precondition tests.
    assert_eq!(b"2026-07-29T17:23:45.1234567891Z".len(), MAX_PACKED_LEN + 1);
}

#[test]
fn alphabet_selection_is_deterministic() {
    // Numeric wins every tie — including strings that also fit both date-time alphabets.
    assert_eq!(roundtrip(b"2026-07-28 2026-07-29").alphabet, PackedAlphabet::Numeric);
    assert_eq!(roundtrip(b"3.14159265358979").alphabet, PackedAlphabet::Numeric);
    assert_eq!(roundtrip(b"192.168.100.200 1.2").alphabet, PackedAlphabet::Numeric);

    // DateTimeZ takes strings needing ':' or 'T' but not '+', including '-' offsets and the Zulu spelling.
    assert_eq!(roundtrip(b"12:34:56 12:34:57").alphabet, PackedAlphabet::DateTimeZ);
    assert_eq!(roundtrip(b"2026-07-28T14:33:07").alphabet, PackedAlphabet::DateTimeZ);
    assert_eq!(roundtrip(b"2026-07-28T14:33:07Z").alphabet, PackedAlphabet::DateTimeZ);
    assert_eq!(roundtrip(b"2026-07-28T14:33:07-05:00").alphabet, PackedAlphabet::DateTimeZ);

    // DateTimePlus is reached exactly when '+' appears alongside a date-time-only symbol.
    assert_eq!(roundtrip(b"2026-07-28T14:33:07+05:00").alphabet, PackedAlphabet::DateTimePlus);
    assert_eq!(roundtrip(b"14:33+01:00 14:33+02").alphabet, PackedAlphabet::DateTimePlus);

    // Exponent spellings are Numeric-only, as 'Z' is DateTimeZ-only: together they fit nothing.
    assert_eq!(roundtrip(b"1.000000E+00 1e+100").alphabet, PackedAlphabet::Numeric);
    assert_eq!(pack(b"1e+9T 2026-07-28T14:33"), None);
    assert_eq!(pack(b"1E9Z 2026-07-28T14:33"), None);
}

#[test]
fn the_band_is_accepted_end_to_end() {
    for len in MIN_PACKED_LEN..=MAX_PACKED_LEN {
        roundtrip(&vec![b'1'; len]);
    }
}

// The band is the tier selector's contract, asserted rather than checked at runtime.  Release builds disable debug
// assertions, so these tests exist only where the assertion does.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "16-30 characters")]
fn content_below_the_band_violates_the_precondition() {
    let _ = pack(&[b'1'; MIN_PACKED_LEN - 1]);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "16-30 characters")]
fn content_above_the_band_violates_the_precondition() {
    let _ = pack(&[b'1'; MAX_PACKED_LEN + 1]);
}

#[test]
fn nul_is_unpackable_in_every_alphabet() {
    // NUL is in no symbol list, so the encode tables hold INVALID at index 0 by construction: in-band NUL-bearing
    // content is a certain `pack` failure and needs no pre-check anywhere.  It reaches the heap tier, per the
    // §2.2.9 NUL ruling, by way of this rejection.
    for table in [&NUMERIC_ENCODE, &DATETIME_Z_ENCODE, &DATETIME_PLUS_ENCODE] {
        assert_eq!(table[0], INVALID, "NUL must be outside every alphabet");
    }

    assert_eq!(pack(b"2026-07-28T14:33\x00"), None);
    assert_eq!(pack(b"\x002026-07-28T14:33"), None);
    assert_eq!(pack(b"1234567\x0090123456"), None);
}

#[test]
fn boundaries_and_rejections() {
    assert_eq!(pack(b"abcdefghijklmnopq"), None);
    assert_eq!(pack(b"1,234,567,890,123"), None, "the comma is in no alphabet");
    assert_eq!(pack(b"1\t2\t3\t4\t5\t6\t7\t8\t9"), None, "only the space is whitespace-encodable");
}

#[test]
fn packed_order_equals_raw_order() {
    // Same-alphabet corpora with prefix pairs, interior spaces, and equals, all inside the band.
    let numeric: Vec<&[u8]> = vec![
        b"1234567890123456",
        b"12345678901234567",
        b"1234567890123456 ", // Trailing space: unpackable, filtered below.
        b"1234567890123456.7",
        b"1234567890123456 7",
        b"2026-07-28 2026-07",
        b"2026-07-28 2026-07-29",
        b"-2.22507385850720e-308",
        b"-2.22507385850720e-30",
        b"1.7976931348623157e+308",
        b"9223372036854775807",
        b"-9223372036854775808",
        b"192.168.100.200 1.2",
        b"192.168.100.200 1.20",
    ];
    let datetimes: Vec<&[u8]> = vec![
        b"2026-07-28T14:33:07Z",
        b"2026-07-28T14:33:07.123Z",
        b"2026-07-28T14:33:08Z",
        b"2026-07-28 14:33:07Z",
        b"2025-12-31T23:59:59Z",
        b"12:34:56 12:34:57",
        b"12:34:56 12:34:57Z",
        b"2026-07-28T14:33:07+05:00",
        b"14:33+01:00 14:33+02",
        b"14:33+01:01 14:33+02",
    ];

    for corpus in [&numeric, &datetimes] {
        for a in corpus.iter() {
            for b in corpus.iter() {
                let (pa, pb) = match (pack(a), pack(b)) {
                    (Some(pa), Some(pb)) => (pa, pb),
                    _ => continue, // Unpackable entries are covered by the rejection tests.
                };
                if pa.alphabet != pb.alphabet {
                    continue; // Cross-alphabet ordering decodes; the same-alphabet law is what this checks.
                }
                assert_eq!(
                    pa.cmp_same_alphabet(&pb),
                    a.cmp(b),
                    "order property violated for {:?} vs {:?}",
                    String::from_utf8_lossy(a),
                    String::from_utf8_lossy(b)
                );
            }
        }
    }
}

#[test]
fn prefix_ordering_survives_the_shared_zero_nibble() {
    // The case the shared space/pad nibble makes delicate: the longer string continues with a space.
    for (short, long) in [
        (&b"1234567890123456"[..], &b"1234567890123456 7"[..]),
        (b"1234567890123456", b"1234567890123456  7"),
        (b"2026-07-28 14:33", b"2026-07-28 14:33 07"),
        (b"12:34:56 12:34:57", b"12:34:56 12:34:57 5"),
    ] {
        let (ps, pl) = (pack(short).unwrap(), pack(long).unwrap());
        assert_eq!(ps.alphabet, pl.alphabet, "corpus must stay in one alphabet");
        assert_eq!(ps.cmp_same_alphabet(&pl), std::cmp::Ordering::Less);
        assert_eq!(short.cmp(long), std::cmp::Ordering::Less, "premise: the prefix sorts first");
    }
}

#[test]
fn every_symbol_at_every_position() {
    for &sym in NUMERIC_SYMBOLS.iter().chain(DATETIME_Z_SYMBOLS).chain(DATETIME_PLUS_SYMBOLS) {
        for pos in 0..MAX_PACKED_LEN {
            let mut s = vec![b'0'; MAX_PACKED_LEN];
            s[pos] = sym;

            // A symbol landing last would be a trailing space in one case; that string is unpackable by rule.
            if s.last() == Some(&b' ') {
                assert_eq!(pack(&s), None);
            } else {
                roundtrip(&s);
            }
        }
    }
}

#[test]
fn nibble_assignment_is_ascii_monotone() {
    // The order property's foundation, checked directly so a table edit cannot silently break it: nibbles are
    // exactly 0, 1, 2, ... in ASCII order, with the space — the least symbol — at 0.
    for (symbols, table) in [(NUMERIC_SYMBOLS, &NUMERIC_ENCODE), (DATETIME_Z_SYMBOLS, &DATETIME_Z_ENCODE), (DATETIME_PLUS_SYMBOLS, &DATETIME_PLUS_ENCODE)] {
        assert_eq!(symbols[0], b' ', "the space must be the first symbol");
        let mut expected = 0u8;
        for b in 0..=255u8 {
            let n = table[b as usize];
            if n != INVALID {
                assert_eq!(n, expected, "nibble values must ascend with ASCII, starting at 0");
                expected += 1;
            }
        }
        assert_eq!(expected as usize, symbols.len(), "every symbol must be reachable");
    }
}

/// The straightforward reverse scan the nibble count replaces: retained as the equivalence oracle.
fn len_reference(nibbles: &[u8; PACKED_BYTES]) -> usize {
    for i in (0..PACKED_BYTES).rev() {
        let byte = nibbles[i];
        if byte != 0 {
            return 2 * i + if byte & 0x0F != 0 { 2 } else { 1 };
        }
    }

    0
}

#[test]
fn derived_length_matches_the_reference_exhaustively() {
    // Every terminating position in the band and both parities, crossed with interior-zero (space) patterns — the
    // nibble-count arithmetic is exactly where an off-by-one would hide.
    for last in (MIN_PACKED_LEN - 1)..MAX_PACKED_LEN {
        for pattern in 0..512u32 {
            let mut nibbles = [0u8; PACKED_BYTES];
            for i in 0..last {
                let v = ((pattern >> (i % 9)) & 0x0F) as u8;
                if i % 2 == 0 { nibbles[i / 2] |= v << 4 } else { nibbles[i / 2] |= v }
            }

            let v = 1 + (pattern % 15) as u8; // The final nibble is nonzero: no trailing spaces.
            if last % 2 == 0 {
                nibbles[last / 2] |= v << 4
            } else {
                nibbles[last / 2] |= v
            }

            let packed = Packed { alphabet: PackedAlphabet::Numeric, nibbles };
            assert_eq!(packed.len(), len_reference(&nibbles), "last={last} pattern={pattern}");
            assert_eq!(packed.len(), last + 1);
        }
    }
}

#[test]
fn cross_representation_comparison_is_length_first_correct() {
    // The pinned counterexample: a naive space-decoding of the zero nibble answers Greater; the truth is Less.
    let p = pack(b"2026-07-28 14:33").unwrap();
    assert_eq!(p.cmp_bytes(b"2026-07-28 14:33\n"), std::cmp::Ordering::Less);
    assert_eq!(p.cmp_bytes(b"2026-07-28 14:33 "), std::cmp::Ordering::Less);
    assert_eq!(p.cmp_bytes(b"2026-07-28 14:33"), std::cmp::Ordering::Equal);
    assert!(!p.eq_bytes(b"2026-07-28 14:33 "), "trailing space on the raw side is a length mismatch");
    assert!(!p.eq_bytes(b"2026-07-28 14:33\n"));
    assert!(p.eq_bytes(b"2026-07-28 14:33"));

    // Interior spaces decode and compare as real characters.
    let spaced = pack(b"2026-07-28 14:33:07").unwrap();
    assert!(spaced.eq_bytes(b"2026-07-28 14:33:07"));
    assert!(!spaced.eq_bytes(b"2026-07-28 14:33"));
    assert_eq!(spaced.cmp_bytes(b"2026-07-28\n14:33:07"), std::cmp::Ordering::Greater); // Space > newline.

    // The general property against arbitrary raw strings, packable or not, longer and shorter.
    let corpus: Vec<&[u8]> =
        vec![b"2026-07-28 14:33", b"2026-07-28 14:33:07", b"192.168.100.200 1.2", b"2026-07-29T17:23:45.123456789Z", b"9223372036854775807"];
    let others: Vec<&[u8]> = vec![
        b"",
        b"1",
        b"2026-07-28 14:33",
        b"2026-07-28 14:33\n",
        b"2026-07-28 14:33 ",
        b"2026-07-28 14:33:07",
        b"2026-07-28\x0014:33",
        b"abcdefghijklmnopq",
        b"192.168.100.200 1.2",
        b"zzz",
        b"\x00",
        b"2026-07-29T17:23:45.123456789Z",
        b"2026-07-29T17:23:45.123456789Z0",
    ];

    for a in &corpus {
        let pa = pack(a).unwrap();
        for b in &others {
            assert_eq!(pa.cmp_bytes(b), a.cmp(b), "{:?} vs {:?}", String::from_utf8_lossy(a), String::from_utf8_lossy(b));
            assert_eq!(pa.eq_bytes(b), a == b, "{:?} vs {:?}", String::from_utf8_lossy(a), String::from_utf8_lossy(b));
        }
    }
}
