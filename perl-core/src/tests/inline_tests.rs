use super::*;

fn bytes(content: &[u8]) -> InlineStr {
    let (buf, full) = build_payload(content);
    InlineStr::Bytes { buf, full }
}

fn utf8(content: &[u8]) -> InlineStr {
    let (buf, full) = build_payload(content);
    InlineStr::Utf8 { buf, full }
}

fn latin1(cp: &[u8], utf8_flag: bool) -> InlineStr {
    let (cp, full) = build_payload(cp);
    InlineStr::Latin1 { cp, full, utf8_flag }
}

#[test]
fn the_e9_monster_is_distinguished_by_format_alone() {
    // Same payload byte, same flag state — different strings, exactly the container-verified facts: flag-off they are
    // unequal at lengths one and two; decoded they are equal.
    let raw = classify(b"\xE9", false).unwrap();
    let utf8_data = classify(b"\xC3\xA9", false).unwrap();
    assert_eq!(raw, bytes(b"\xE9"));
    assert_eq!(utf8_data, latin1(b"\xE9", false), "canonical: valid UTF-8 octets compress");
    assert_eq!(raw.len(), 1);
    assert_eq!(utf8_data.len(), 2, "flag-off Latin1 length is the expansion sum");
    assert!(!raw.eq_perl(&utf8_data), "flag-off: E9 ne C3.A9");

    let decoded = classify(b"\xC3\xA9", true).unwrap();
    assert_eq!(decoded, latin1(b"\xE9", true));
    assert!(raw.eq_perl(&decoded), "flag-off E9 eq flag-on \u{e9}: sv_eq upgrades the byte side");
    assert!(decoded.eq_perl(&raw), "and symmetrically");
    assert!(!utf8_data.eq_perl(&decoded), "the octet string C3.A9 ne the character \u{e9}");
}

#[test]
fn flag_off_latin1_length_is_thirty_at_the_extreme() {
    // Fifteen high-Latin-1 code points, originally UTF-8 encoded, flag off: length is 30.
    let octets: Vec<u8> = std::iter::repeat_n(*b"\xC3\xA9", 15).flatten().collect();
    assert_eq!(octets.len(), 30);

    let s = classify(&octets, false).unwrap();
    assert_eq!(s, latin1(&[0xE9; 15], false));
    assert_eq!(s.len(), 30, "length is the expansion sum, never the payload count");

    let (bytes, n) = s.internal_bytes();
    assert_eq!(&bytes[..n], &octets[..], "the octet view is the exact original");

    // The same payload with the flag on is fifteen characters.
    assert_eq!(latin1(&[0xE9; 15], true).len(), 15);

    // Mixed content sums per code point; all-ASCII expands to itself.
    assert_eq!(classify(b"a\xC3\xA9b", false).unwrap().len(), 4);
    assert_eq!(classify(b"hello", false).unwrap().len(), 5);

    // Thirty-one octets exceed the tier.
    let over: Vec<u8> = std::iter::repeat_n(*b"\xC3\xA9", 15).flatten().chain([b'a']).collect();
    assert_eq!(classify(&over, false), None);
}

#[test]
fn canonical_selection_is_deterministic() {
    // Valid Latin-1-range UTF-8 octets always compress, even when Bytes would also fit — equal perl strings must take
    // equal representations.
    assert!(matches!(classify(b"\xC3\xA9", false).unwrap(), InlineStr::Latin1 { .. }));
    assert!(matches!(classify(b"ascii", false).unwrap(), InlineStr::Latin1 { .. }));

    // Invalid-as-UTF-8 octets take Bytes: a lone continuation, a dangling lead, a high lead.
    assert!(matches!(classify(b"\xA9", false).unwrap(), InlineStr::Bytes { .. }));
    assert!(matches!(classify(b"\xE9", false).unwrap(), InlineStr::Bytes { .. }));
    assert!(matches!(classify(b"abc\xC3", false).unwrap(), InlineStr::Bytes { .. }));

    // Overlong encodings are invalid and never compress — noncanonical content stays encoded.  C0 80, the overlong NUL,
    // is the case the length-byte design newly reaches: canonical U+0000 is the single byte 00.
    assert!(matches!(classify(b"\xC0\xA9", false).unwrap(), InlineStr::Bytes { .. }));
    assert!(matches!(classify(b"\xC1\xBF", false).unwrap(), InlineStr::Bytes { .. }));
    assert!(matches!(classify(b"\xC0\x80", false).unwrap(), InlineStr::Bytes { .. }));

    // Flag-on: Latin-1-range decodes; beyond-range and malformed take the Utf8 form.
    assert!(matches!(classify(b"\xC3\xA9", true).unwrap(), InlineStr::Latin1 { utf8_flag: true, .. }));
    assert_eq!(classify(b"\xE2\x82\xAC", true).unwrap(), utf8(b"\xE2\x82\xAC")); // U+20AC.
    assert!(matches!(classify(b"\xE9", true).unwrap(), InlineStr::Utf8 { .. })); // Malformed.
}

#[test]
fn upgrade_and_downgrade_preserve_characters() {
    // Bytes E9 upgrades with zero byte work: the payload is verbatim — length byte included — only the form changes.
    let raw = classify(b"\xE9", false).unwrap();
    assert_eq!(raw.upgrade().unwrap(), latin1(b"\xE9", true));
    assert!(raw.upgrade().unwrap().eq_perl(&raw), "upgrade preserves the characters");

    // Downgrading flag-on \u{e9} lands in Bytes: E9 alone is not valid UTF-8.
    let e_acute = latin1(b"\xE9", true);
    assert_eq!(e_acute.downgrade().unwrap(), classify(b"\xE9", false).unwrap());

    // Downgrading flag-on "\u{c3}\u{a9}" re-compresses: its octets are valid UTF-8, so the canonical rule lands it in
    // flag-off Latin1 automatically.
    let a_tilde_pair = latin1(b"\xC3\xA9", true);
    assert_eq!(a_tilde_pair.downgrade().unwrap(), latin1(b"\xE9", false));
    assert!(a_tilde_pair.downgrade().unwrap().eq_perl(&a_tilde_pair));

    // Beyond Latin-1 cannot downgrade.
    assert_eq!(classify(b"\xE2\x82\xAC", true).unwrap().downgrade(), None);

    // Upgrading a 16-30-octet flag-off Latin1 exceeds inline: 16-30 characters.
    let wide: Vec<u8> = std::iter::repeat_n(*b"\xC3\xA9", 10).flatten().collect();
    let s = classify(&wide, false).unwrap(); // 20 octets in 10 payload bytes.
    assert_eq!(s.upgrade(), None, "20 characters do not fit 15 payload bytes");
}

#[test]
fn reinterpretation_transforms_match_the_probes() {
    // _utf8_off on compressed flag-on content is the pure flag flip: an upgraded e-acute becomes the flag-off two-octet
    // C3 A9 (probed: flag=0 chars=2 str=C3.A9).
    let upgraded = latin1(b"\xE9", true);
    let off = upgraded.utf8_off_reinterpret().unwrap();
    assert_eq!(off, latin1(b"\xE9", false), "payload untouched; only the flag");
    assert_eq!(off.len(), 2);

    // _utf8_on over the raw octet E9 reclassifies to flagged malformed content (probed: flag=1).
    let raw = classify(b"\xE9", false).unwrap();
    assert!(matches!(raw.utf8_on_reinterpret().unwrap(), InlineStr::Utf8 { .. }));

    // _utf8_on over flag-off Latin1 is the flag flip in the other direction.
    let data = classify(b"\xC3\xA9", false).unwrap();
    assert_eq!(data.utf8_on_reinterpret().unwrap(), latin1(b"\xE9", true));

    // _utf8_off on the Utf8 form reclassifies its bytes as octets — valid UTF-8 bytes compress.
    let euro = classify(b"\xE2\x82\xAC", true).unwrap();
    assert!(matches!(euro.utf8_off_reinterpret().unwrap(), InlineStr::Bytes { .. }), "E2 82 AC decodes to U+20AC, beyond Latin-1 range, so it cannot compress");
}

#[test]
fn byte_mutation_reruns_canonical_selection() {
    // Chop splitting a trailing pair lands in Bytes (probed: a dangling C3 lead remains).
    let s = classify(b"A\xC3\xA9", false).unwrap();
    let chopped = s.remove_last_octet().unwrap();
    assert_eq!(chopped, classify(b"A\xC3", false).unwrap());
    assert!(matches!(chopped, InlineStr::Bytes { .. }), "the split result is no longer valid UTF-8");

    // Chop removing a whole trailing ASCII character stays compressed.
    let s = classify(b"\xC3\xA9A", false).unwrap();
    assert_eq!(s.remove_last_octet().unwrap(), latin1(b"\xE9", false));

    // Chop on a 30-octet flag-off Latin1 leaves 29 raw octets: nothing inline can hold them.
    let octets: Vec<u8> = std::iter::repeat_n(*b"\xC3\xA9", 15).flatten().collect();
    let s = classify(&octets, false).unwrap();
    assert_eq!(s.remove_last_octet(), None, "29 non-UTF-8 octets exceed every inline form: heap");
}

#[test]
fn nul_is_ordinary_content_in_every_spelling() {
    // The revised ruling (§2.2.9): the octet, the encoded byte, and the character U+0000 are stored like anything else
    // — the explicit length is what admits them, a terminator having no way to.
    let s = classify(b"\0", false).unwrap();
    assert_eq!(s, latin1(b"\0", false), "a lone NUL octet is valid UTF-8: canonical selection compresses it");
    assert_eq!(s.len(), 1);

    let s = classify(b"ab\0cd", false).unwrap();
    assert_eq!(s.len(), 5);

    let (view, n) = s.internal_bytes();
    assert_eq!(&view[..n], b"ab\0cd", "the NUL is content, not an end marker");

    // Beside an invalid-UTF-8 octet, NUL-bearing content takes Bytes like any other such content.
    let s = classify(b"\xE9\0", false).unwrap();
    assert_eq!(s, bytes(b"\xE9\0"));
    assert_eq!(s.len(), 2);

    // Flag-on: the encoded byte 00 is U+0000, compressing with its neighbours.
    let s = classify(b"\xC3\xA9\0", true).unwrap();
    assert_eq!(s, latin1(b"\xE9\0", true));
    assert_eq!(s.len(), 2, "flagged: two characters, one of them U+0000");

    // And equality distinguishes content past a NUL, which a terminator never could.
    assert!(!classify(b"a\0b", false).unwrap().eq_perl(&classify(b"a\0c", false).unwrap()));
}

#[test]
fn the_length_families_split_at_capacity() {
    // Fourteen bytes store their length in the byte a fifteenth would have used; fifteen imply it.
    match classify(&[0x41; 14], false).unwrap() {
        InlineStr::Latin1 { cp, full, .. } => {
            assert!(!full, "fourteen is the stored-length family");
            assert_eq!(cp[LENGTH_BYTE] as usize, 14, "the length byte");
        }
        other => panic!("ASCII should compress, got {other:?}"),
    }

    match classify(&[0xE9; 15], false).unwrap() {
        InlineStr::Bytes { full, .. } => assert!(full, "fifteen is the full-capacity family"),
        other => panic!("fifteen invalid-UTF-8 octets should take Bytes, got {other:?}"),
    }
    assert_eq!(classify(&[0xE9; 15], false).unwrap().len(), 15);
    assert_eq!(classify(&[0xE9; 16], false), None, "sixteen octets exceed every inline form");

    // Fifteen characters flag-on fill Latin1 exactly; sixteen exceed it.
    let cps16: Vec<u8> = std::iter::repeat_n(*b"\xC3\xA9", 16).flatten().collect();
    assert_eq!(classify(&cps16, true), None);

    assert_eq!(classify(b"", false).unwrap().len(), 0);
}

#[test]
fn equal_content_takes_equal_payload_bytes() {
    // Canonical padding plus the length byte: content arriving by different routes must land byte-identical, or
    // representation-level equality stops being sound.  The routes group by which string they produce — downgrade
    // preserves *characters* where _utf8_off reinterprets *bytes*, so those two land on different strings, which is the
    // E9 monster wearing its transform clothes.

    // The two-octet string E9 E9, three ways.
    let direct = classify(b"\xE9\xE9", false).unwrap();
    let via_downgrade = latin1(b"\xE9\xE9", true).downgrade().unwrap();
    let via_chop = match classify(b"\xE9\xE9A", false).unwrap().remove_last_octet() {
        Some(s) => s,
        None => panic!("two octets fit inline"),
    };
    assert_eq!(direct, via_downgrade, "downgrade lands on the octet string");
    assert_eq!(direct, via_chop);

    // The four-octet string C3 A9 C3 A9, two ways: direct classification, and reinterpreting the flagged pair's bytes —
    // the pure flag flip.
    let data = classify(b"\xC3\xA9\xC3\xA9", false).unwrap();
    let via_reinterpret = latin1(b"\xE9\xE9", true).utf8_off_reinterpret().unwrap();
    assert_eq!(data, via_reinterpret);
    assert!(!direct.eq_perl(&data), "and the two groups are different strings");
}
