use super::*;

fn latin1(cp: &[u8], utf8_flag: bool) -> InlineStr {
    let mut payload = [0u8; INLINE_BYTES];
    payload[..cp.len()].copy_from_slice(cp);
    InlineStr::Latin1 { cp: payload, utf8_flag }
}

#[test]
fn the_e9_monster_is_distinguished_by_format_alone() {
    // Same payload byte, same flag state — different strings, exactly the container-verified facts: flag-off they
    // are unequal at lengths one and two; decoded they are equal.
    let raw = classify(b"\xE9", false).unwrap();
    let utf8_data = classify(b"\xC3\xA9", false).unwrap();
    assert_eq!(raw, InlineStr::Bytes(*b"\xE9\0\0\0\0\0\0\0\0\0\0\0\0\0\0"));
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
    // Valid Latin-1-range UTF-8 octets always compress, even when Bytes would also fit — equal perl strings must
    // take equal representations.
    assert!(matches!(classify(b"\xC3\xA9", false).unwrap(), InlineStr::Latin1 { .. }));
    assert!(matches!(classify(b"ascii", false).unwrap(), InlineStr::Latin1 { .. }));

    // Invalid-as-UTF-8 octets take Bytes: a lone continuation, a dangling lead, a high lead.
    assert!(matches!(classify(b"\xA9", false).unwrap(), InlineStr::Bytes(_)));
    assert!(matches!(classify(b"\xE9", false).unwrap(), InlineStr::Bytes(_)));
    assert!(matches!(classify(b"abc\xC3", false).unwrap(), InlineStr::Bytes(_)));

    // Overlong encodings are invalid and never compress — noncanonical content stays encoded.
    assert!(matches!(classify(b"\xC0\xA9", false).unwrap(), InlineStr::Bytes(_)));
    assert!(matches!(classify(b"\xC1\xBF", false).unwrap(), InlineStr::Bytes(_)));

    // Flag-on: Latin-1-range decodes; beyond-range and malformed take the Utf8 form.
    assert!(matches!(classify(b"\xC3\xA9", true).unwrap(), InlineStr::Latin1 { utf8_flag: true, .. }));
    assert!(matches!(classify(b"\xE2\x82\xAC", true).unwrap(), InlineStr::Utf8(_))); // U+20AC.
    assert!(matches!(classify(b"\xE9", true).unwrap(), InlineStr::Utf8(_))); // Malformed.
}

#[test]
fn upgrade_and_downgrade_preserve_characters() {
    // Bytes E9 upgrades with zero byte work: the payload is verbatim, only the form changes.
    let raw = classify(b"\xE9", false).unwrap();
    assert_eq!(raw.upgrade().unwrap(), latin1(b"\xE9", true));
    assert!(raw.upgrade().unwrap().eq_perl(&raw), "upgrade preserves the characters");

    // Downgrading flag-on \u{e9} lands in Bytes: E9 alone is not valid UTF-8.
    let e_acute = latin1(b"\xE9", true);
    assert_eq!(e_acute.downgrade().unwrap(), classify(b"\xE9", false).unwrap());

    // Downgrading flag-on "\u{c3}\u{a9}" re-compresses: its octets are valid UTF-8, so the canonical rule lands it
    // in flag-off Latin1 automatically.
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
    // _utf8_off on compressed flag-on content is the pure flag flip: an upgraded e-acute becomes the flag-off
    // two-octet C3 A9 (probed: flag=0 chars=2 str=C3.A9).
    let upgraded = latin1(b"\xE9", true);
    let off = upgraded.utf8_off_reinterpret().unwrap();
    assert_eq!(off, latin1(b"\xE9", false), "payload untouched; only the flag");
    assert_eq!(off.len(), 2);

    // _utf8_on over the raw octet E9 reclassifies to flagged malformed content (probed: flag=1).
    let raw = classify(b"\xE9", false).unwrap();
    assert!(matches!(raw.utf8_on_reinterpret().unwrap(), InlineStr::Utf8(_)));

    // _utf8_on over flag-off Latin1 is the flag flip in the other direction.
    let data = classify(b"\xC3\xA9", false).unwrap();
    assert_eq!(data.utf8_on_reinterpret().unwrap(), latin1(b"\xE9", true));

    // _utf8_off on the Utf8 form reclassifies its bytes as octets — valid UTF-8 bytes compress.
    let euro = classify(b"\xE2\x82\xAC", true).unwrap();
    assert!(matches!(euro.utf8_off_reinterpret().unwrap(), InlineStr::Bytes(_)), "E2 82 AC decodes to U+20AC, beyond Latin-1 range, so it cannot compress");
}

#[test]
fn byte_mutation_reruns_canonical_selection() {
    // Chop splitting a trailing pair lands in Bytes (probed: a dangling C3 lead remains).
    let s = classify(b"A\xC3\xA9", false).unwrap();
    let chopped = s.remove_last_octet().unwrap();
    assert_eq!(chopped, classify(b"A\xC3", false).unwrap());
    assert!(matches!(chopped, InlineStr::Bytes(_)), "the split result is no longer valid UTF-8");

    // Chop removing a whole trailing ASCII character stays compressed.
    let s = classify(b"\xC3\xA9A", false).unwrap();
    assert_eq!(s.remove_last_octet().unwrap(), latin1(b"\xE9", false));

    // Chop on a 30-octet flag-off Latin1 leaves 29 raw octets: nothing inline can hold them.
    let octets: Vec<u8> = std::iter::repeat_n(*b"\xC3\xA9", 15).flatten().collect();
    let s = classify(&octets, false).unwrap();
    assert_eq!(s.remove_last_octet(), None, "29 non-UTF-8 octets exceed every inline form: heap");
}

#[test]
fn nul_bearing_strings_are_heap_only() {
    // All three spellings of the ruled NUL policy: octet, encoded byte, and U+0000 character.
    assert_eq!(classify(b"\0", false), None);
    assert_eq!(classify(b"ab\0cd", false), None);
    assert_eq!(classify(b"\0", true), None);
    assert_eq!(classify(b"\xC3\xA9\0", true), None);
}

#[test]
fn storage_boundaries() {
    assert_eq!(classify(b"", false).unwrap().len(), 0);
    // Fifteen octets of non-UTF-8 content fill Bytes exactly; sixteen exceed it.
    assert!(classify(&[0xE9; 15], false).is_some());
    assert_eq!(classify(&[0xE9; 16], false), None);

    // The unterminated form: a full payload has no NUL and length 15.
    assert_eq!(classify(&[0xE9; 15], false).unwrap().len(), 15);

    // Fifteen characters flag-on fill Latin1 exactly; sixteen exceed it.
    let cps16: Vec<u8> = std::iter::repeat_n(*b"\xC3\xA9", 16).flatten().collect();
    assert_eq!(classify(&cps16, true), None);
}
