use super::*;

fn roundtrip(s: &[u8]) -> Packed {
    let p = pack(s).unwrap();
    let (out, len) = p.unpack();
    assert_eq!(&out[..len], s, "round-trip must be exact: {:?}", String::from_utf8_lossy(s));
    assert_eq!(p.len(), s.len(), "derived length must match: {:?}", String::from_utf8_lossy(s));
    assert!(p.padding_is_canonical(), "padding must be zero: {:?}", String::from_utf8_lossy(s));

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
fn trailing_spaces_are_representable() {
    // The restriction the explicit length removes.  Incremental building passes through these on its way to longer
    // content, so they must round-trip like anything else.
    for s in [
        &b"2026-07-28T14:33:07 "[..],
        b"555 1234 555 1234 ",
        b"1 2 3 4 5 6 7 8   ",
        b"2026-07-28 14:33:0 ",
        b"12345678901234567890123456789 ", // 30 characters, the full family, ending in a space.
    ] {
        roundtrip(s);
    }
}

#[test]
fn interior_spaces_pack_too() {
    for s in [&b"555 1234 555 1234"[..], b" 1 234 567 890 12", b"2026-07-28 14:33:07Z", b"2026-07-28 14:33:07+05:00"] {
        roundtrip(s);
    }
}

#[test]
fn iso_timestamp_grammar_is_covered() {
    for s in [
        &b"2026-07-28T14:33:07Z"[..],
        b"2026-07-28T14:33:07+05:00",
        b"2026-07-28T14:33:07-05:00",
        b"2026-07-28 14:33:07+00:00",
        b"2026-07-28T14:33:07.123456Z",
        b"2026-07-28T14:33:07.12+05:00",
        b"20260728T143307Z 1234",
    ] {
        roundtrip(s);
    }

    // The capacity boundary: Zulu leaves room for nine fractional digits and a numeric offset for three, so
    // millisecond-plus-offset (29) and nanosecond-Zulu (30) both fit.
    assert_eq!(b"2026-07-29T17:23:45.123456789Z".len(), MAX_PACKED_LEN);
    roundtrip(b"2026-07-29T17:23:45.123456789Z");
    roundtrip(b"2026-07-29 17:23:45.123-04:00");
}

#[test]
fn the_length_families_split_at_the_capacity() {
    for len in MIN_PACKED_LEN..MAX_PACKED_LEN {
        let p = roundtrip(&vec![b'1'; len]);
        assert!(!p.full, "length {len} belongs to the stored-length family");
        assert_eq!(nibble_at(&p.nibbles, MAX_PACKED_LEN - 1), (len & 0x0F) as u8, "stored length nibble");
    }

    let p = roundtrip(&[b'1'; MAX_PACKED_LEN]);
    assert!(p.full, "the capacity belongs to the implied-length family");
}

#[test]
fn alphabet_selection_is_deterministic() {
    // Numeric wins every tie — including strings that also fit both date-time alphabets.
    assert_eq!(roundtrip(b"2026-07-28 2026-07-29").alphabet, PackedAlphabet::Numeric);
    assert_eq!(roundtrip(b"3.14159265358979").alphabet, PackedAlphabet::Numeric);
    assert_eq!(roundtrip(b"1.000000E+00 1e+100").alphabet, PackedAlphabet::Numeric);

    // DateTimePlus is where timestamps belong: everything needing ':' or 'T', in any offset form.
    assert_eq!(roundtrip(b"12:34:56 12:34:57").alphabet, PackedAlphabet::DateTimePlus);
    assert_eq!(roundtrip(b"2026-07-28T14:33:07").alphabet, PackedAlphabet::DateTimePlus);
    assert_eq!(roundtrip(b"2026-07-28T14:33:07-05:00").alphabet, PackedAlphabet::DateTimePlus);
    assert_eq!(roundtrip(b"2026-07-28T14:33:07+05:00").alphabet, PackedAlphabet::DateTimePlus);
    assert_eq!(roundtrip(b"14:33+01:00 14:33+02").alphabet, PackedAlphabet::DateTimePlus);

    // DateTimeZulu is reached only through 'Z' — which no other alphabet holds — so the variant proves the offset.
    assert_eq!(roundtrip(b"2026-07-28T14:33:07Z").alphabet, PackedAlphabet::DateTimeZulu);
    assert_eq!(roundtrip(b"2026-07-28T14:33:07.123Z").alphabet, PackedAlphabet::DateTimeZulu);

    // Exponent spellings are Numeric-only, as 'Z' is DateTimeZulu-only: together they fit nothing.
    assert_eq!(pack(b"1e+9T 2026-07-28T14:33"), None);
    assert_eq!(pack(b"1E9Z 2026-07-28T14:33"), None);
}

#[test]
fn nul_is_unpackable_in_every_alphabet() {
    // NUL is in no symbol list, so the encode tables hold INVALID at index 0 by construction: in-band NUL-bearing
    // content is a certain `pack` failure and needs no pre-check anywhere.
    for table in [&NUMERIC_ENCODE, &DATETIME_PLUS_ENCODE, &DATETIME_ZULU_ENCODE] {
        assert_eq!(table[0], INVALID, "NUL must be outside every alphabet");
    }

    assert_eq!(pack(b"2026-07-28T14:33\x00"), None);
    assert_eq!(pack(b"\x002026-07-28T14:33"), None);
}

#[test]
fn boundaries_and_rejections() {
    assert_eq!(pack(b"abcdefghijklmnopq"), None);
    assert_eq!(pack(b"1,234,567,890,123"), None, "the comma is in no alphabet");
    assert_eq!(pack(b"1\t2\t3\t4\t5\t6\t7\t8\t9"), None, "only the space is whitespace-encodable");
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

// ── Ordering ──────────────────────────────────────────────────────

/// Every packable string in one alphabet, crossed with itself: same-family pairs must agree with plain byte comparison,
/// and cross-family pairs with the shared-nibbles-then-length path.
fn assert_order_law(corpus: &[&[u8]]) {
    for a in corpus {
        for b in corpus {
            let (Some(pa), Some(pb)) = (pack(a), pack(b)) else { continue };
            if pa.alphabet != pb.alphabet {
                continue; // Cross-alphabet ordering decodes; this checks the same-alphabet law.
            }
            assert_eq!(pa.cmp_same_alphabet(&pb), a.cmp(b), "order violated for {:?} vs {:?}", String::from_utf8_lossy(a), String::from_utf8_lossy(b));
        }
    }
}

#[test]
fn packed_order_equals_raw_order() {
    assert_order_law(&[
        b"1234567890123456",
        b"12345678901234567",
        b"1234567890123456 ",
        b"1234567890123456  ",
        b"1234567890123456.7",
        b"1234567890123456 7",
        b"123456789012345678901234567890", // Full family.
        b"12345678901234567890123456789",  // One shorter: cross-family prefix pair.
        b"12345678901234567890123456789 ",
        b"-2.22507385850720e-308",
        b"-2.22507385850720e-30",
        b"9223372036854775807",
        b"-9223372036854775808",
        b"192.168.100.200 1.2",
        b"192.168.100.200 1.20",
    ]);
    assert_order_law(&[
        b"2026-07-28T14:33:07Z",
        b"2026-07-28T14:33:07.123Z",
        b"2026-07-28T14:33:08Z",
        b"2026-07-28 14:33:07Z",
        b"2025-12-31T23:59:59Z",
        b"12:34:56 12:34:57",
        b"12:34:56 12:34:57 ",
        b"2026-07-29T17:23:45.123456789Z", // Full family.
        b"2026-07-29T17:23:45.12345678Z",
    ]);
}

#[test]
fn cross_family_prefix_ordering() {
    // The case the two families make delicate: a 29-character string against the 30-character extension of itself,
    // where the last nibble is a length on one side and a character on the other — including when that character is a
    // space, whose nibble is zero and would otherwise compare below the stored length.
    for (short, long) in [
        (&b"12345678901234567890123456789"[..], &b"123456789012345678901234567890"[..]),
        (b"12345678901234567890123456789", b"12345678901234567890123456789 "),
        (b"2026-07-29T17:23:45.12345678Z", b"2026-07-29T17:23:45.12345678Z0"),
    ] {
        let (ps, pl) = (pack(short).unwrap(), pack(long).unwrap());
        assert_eq!(ps.alphabet, pl.alphabet, "corpus must stay in one alphabet");
        assert!(!ps.full && pl.full, "this pair must straddle the families");
        assert_eq!(ps.cmp_same_alphabet(&pl), Ordering::Less, "{:?} vs {:?}", String::from_utf8_lossy(short), String::from_utf8_lossy(long));
        assert_eq!(pl.cmp_same_alphabet(&ps), Ordering::Greater);
        assert_eq!(short.cmp(long), Ordering::Less, "premise: the prefix sorts first");
    }
}

#[test]
fn every_symbol_at_every_position() {
    for &sym in NUMERIC_SYMBOLS.iter().chain(DATETIME_PLUS_SYMBOLS).chain(DATETIME_ZULU_SYMBOLS) {
        for pos in 0..MAX_PACKED_LEN {
            let mut s = vec![b'0'; MAX_PACKED_LEN];
            s[pos] = sym;
            roundtrip(&s); // Trailing spaces included now.
        }
    }
}

#[test]
fn nibble_assignment_is_ascii_monotone() {
    // The order property's foundation, checked directly so a table edit cannot silently break it: nibbles are exactly
    // 0, 1, 2, ... in ASCII order, with the space — the least symbol — at 0.
    for (symbols, table) in [(NUMERIC_SYMBOLS, &NUMERIC_ENCODE), (DATETIME_PLUS_SYMBOLS, &DATETIME_PLUS_ENCODE), (DATETIME_ZULU_SYMBOLS, &DATETIME_ZULU_ENCODE)]
    {
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

// ── Transcoding between alphabets ─────────────────────────────────

#[test]
fn numeric_widens_into_datetime_plus_without_rewriting() {
    // The two lists agree on nibbles 0-13, so widening is a pure reclassification whenever no exponent symbol is
    // present — the nibble array comes out identical.
    for s in [&b"2026-07-28 2026-07-29"[..], b"192.168.100.200 1.2", b"1234567890123456", b"1234567890123456 "] {
        let numeric = pack(s).unwrap();
        assert_eq!(numeric.alphabet, PackedAlphabet::Numeric);
        let widened = numeric.transcode(PackedAlphabet::DateTimePlus).unwrap();
        assert_eq!(widened.nibbles, numeric.nibbles, "no nibble should change for {:?}", String::from_utf8_lossy(s));
        assert_eq!(widened.len(), numeric.len());
        assert_eq!(&widened.unpack().0[..widened.len()], s);
    }

    // Exponent symbols have no counterpart there.
    let with_exponent = pack(b"1.000000E+00 1e+100").unwrap();
    assert_eq!(with_exponent.transcode(PackedAlphabet::DateTimePlus), None);
}

#[test]
fn timestamps_transcode_into_zulu_by_decrement() {
    // The append path's one transcoding step: a timestamp built as DateTimePlus meets a 'Z'.  DateTimeZulu is the same
    // symbol list shifted down past the absent '+', so every nonzero nibble decrements.
    let plus = pack(b"2026-07-28T14:33:0").unwrap();
    assert_eq!(plus.alphabet, PackedAlphabet::DateTimePlus, "timestamps are canonically DateTimePlus");
    let zulu = plus.transcode(PackedAlphabet::DateTimeZulu).unwrap();
    for i in 0..plus.len() {
        let before = nibble_at(&plus.nibbles, i);
        let after = nibble_at(&zulu.nibbles, i);
        assert_eq!(after, if before == 0 { 0 } else { before - 1 }, "nibble {i} should decrement");
    }
    assert_eq!(&zulu.unpack().0[..zulu.len()], b"2026-07-28T14:33:0");

    // A '+' offset cannot become Zulu — the two spellings are mutually exclusive, which is why they fit in two
    // alphabets at all.  Such content goes to the heap instead.
    let offset = pack(b"14:33+01:00 14:33+02").unwrap();
    assert_eq!(offset.alphabet, PackedAlphabet::DateTimePlus);
    assert_eq!(offset.transcode(PackedAlphabet::DateTimeZulu), None, "'+' has no counterpart in DateTimeZulu");

    // Widening from Numeric is free: the two agree on nibbles 0-13, so nothing is rewritten.
    let numeric = pack(b"2026-07-28 2026-07-29").unwrap();
    assert_eq!(numeric.alphabet, PackedAlphabet::Numeric);
    let widened = numeric.transcode(PackedAlphabet::DateTimePlus).unwrap();
    assert_eq!(widened.nibbles, numeric.nibbles, "widening rewrites no nibble");
}

/// The transition specification, written out rather than re-derived from the symbol lists, so the test fails if the
/// tables and the intended behaviour ever diverge.  `None` means the content leaves the packed tier for the heap.
fn expected_mapping(from: PackedAlphabet, to: PackedAlphabet, nibble: u8) -> Option<u8> {
    match (from, to) {
        (PackedAlphabet::Numeric, PackedAlphabet::DateTimePlus) => match nibble {
            0x00..=0x0D => Some(nibble),
            _ => None, // 'E' and 'e' exist in no other alphabet.
        },
        (PackedAlphabet::Numeric, PackedAlphabet::DateTimeZulu) => match nibble {
            0x00 => Some(0x00),
            0x02..=0x0D => Some(nibble - 1),
            _ => None, // '+' at 0x01, and 'E'/'e' at 0x0E-0x0F.
        },
        (PackedAlphabet::DateTimePlus, PackedAlphabet::DateTimeZulu) => match nibble {
            0x00 => Some(0x00),
            0x02..=0x0F => Some(nibble - 1),
            _ => None, // '+' at 0x01.
        },
        _ => unreachable!("the append path only ever widens along these three transitions"),
    }
}

#[test]
fn transition_table_matches_the_specification() {
    let transitions = [
        (PackedAlphabet::Numeric, PackedAlphabet::DateTimePlus, NUMERIC_SYMBOLS),
        (PackedAlphabet::Numeric, PackedAlphabet::DateTimeZulu, NUMERIC_SYMBOLS),
        (PackedAlphabet::DateTimePlus, PackedAlphabet::DateTimeZulu, DATETIME_PLUS_SYMBOLS),
    ];

    for (from, to, symbols) in transitions {
        for (nibble, &symbol) in symbols.iter().enumerate() {
            // A run of one symbol, so every content nibble exercises the same mapping.
            let content = vec![symbol; MIN_PACKED_LEN];
            let packed = pack_in(&content, from).expect("the symbol belongs to its own alphabet");
            assert_eq!(nibble_at(&packed.nibbles, 0), nibble as u8, "symbol {symbol:?} should encode to {nibble:#04x}");

            let expected = expected_mapping(from, to, nibble as u8);
            match (packed.transcode(to), expected) {
                (Some(moved), Some(want)) => {
                    for i in 0..moved.len() {
                        assert_eq!(nibble_at(&moved.nibbles, i), want, "{from:?} to {to:?} on {nibble:#04x}");
                    }
                    assert_eq!(moved.len(), MIN_PACKED_LEN, "the stored length must survive");
                    assert!(moved.padding_is_canonical());
                }
                (None, None) => {} // Falls out of the packed tier, as specified.
                (got, want) => panic!("{from:?} to {to:?} on {nibble:#04x}: got {got:?}, expected {want:?}"),
            }
        }
    }
}

#[test]
fn transcoding_preserves_content_and_leaves_the_length_alone() {
    for s in [&b"2026-07-28 14:33:07"[..], b"2026-07-28 14:33:0 ", b"192.168.100.200 1.2"] {
        let original = pack(s).unwrap();
        for target in [PackedAlphabet::Numeric, PackedAlphabet::DateTimeZulu, PackedAlphabet::DateTimePlus] {
            let Some(moved) = original.transcode(target) else { continue };
            assert_eq!(moved.len(), original.len(), "the length nibble is not a symbol and must not be remapped");
            assert_eq!(moved.full, original.full);
            assert!(moved.padding_is_canonical());
            let (bytes, len) = moved.unpack();
            assert_eq!(&bytes[..len], s, "content must survive the move to {target:?}");
        }
    }
}

#[test]
fn transcoding_to_the_same_alphabet_is_identity() {
    let p = pack(b"2026-07-28T14:33:07Z").unwrap();
    assert_eq!(p.transcode(p.alphabet), Some(p));
}

// ── Comparison against unpacked representations ───────────────────

#[test]
fn cross_representation_comparison_is_correct() {
    let p = pack(b"2026-07-28 14:33").unwrap();
    assert!(p.eq_bytes(b"2026-07-28 14:33"));
    assert!(!p.eq_bytes(b"2026-07-28 14:33 "), "a longer raw string is not equal");
    assert!(!p.eq_bytes(b"2026-07-28 14:33\n"));
    assert_eq!(p.cmp_bytes(b"2026-07-28 14:33\n"), Ordering::Less, "the packed string ended first");
    assert_eq!(p.cmp_bytes(b"2026-07-28 14:33 "), Ordering::Less);
    assert_eq!(p.cmp_bytes(b"2026-07-28 14:33"), Ordering::Equal);

    // A packed string that really does end in a space now exists, and compares as its bytes do.
    let spaced = pack(b"2026-07-28 14:33 ").unwrap();
    assert!(spaced.eq_bytes(b"2026-07-28 14:33 "));
    assert_eq!(spaced.cmp_bytes(b"2026-07-28 14:33"), Ordering::Greater, "the space extends the prefix");

    let corpus: Vec<&[u8]> = vec![
        b"2026-07-28 14:33",
        b"2026-07-28 14:33 ",
        b"2026-07-28 14:33:07",
        b"192.168.100.200 1.2",
        b"2026-07-29T17:23:45.123456789Z",
        b"9223372036854775807",
    ];
    let others: Vec<&[u8]> = vec![
        b"",
        b"1",
        b"2026-07-28 14:33",
        b"2026-07-28 14:33\n",
        b"2026-07-28 14:33 ",
        b"2026-07-28 14:33:07",
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

// ── The padding invariant ─────────────────────────────────────────

#[test]
fn nonzero_padding_is_detected() {
    // Nothing derives a length from the padding any more, so a violation is silent corruption rather than a wrong
    // answer.  The predicate exists to be asserted at every write; this pins that it actually detects the case.
    let mut p = pack(b"1234567890123456").unwrap();
    assert!(p.padding_is_canonical());
    set_nibble(&mut p.nibbles, 20, 7); // Garbage past the content end.
    assert!(!p.padding_is_canonical(), "a nonzero padding nibble must be detected");
    assert_eq!(p.len(), 16, "the length is unaffected, which is exactly why this is dangerous");
}
