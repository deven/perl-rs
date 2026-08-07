use super::*;
use std::collections::HashMap;
use std::str::FromStr;

fn hash_of(s: &PerlString) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);

    h.finish()
}

// ── Construction and boundaries ───────────────────────────────
#[test]
fn the_tier_ladder_places_content_by_length_and_alphabet() {
    // Fifteen payload bytes inline; sixteen to thirty packed when the content is alphabet-conformant; the heap for
    // everything else.  The bands are contiguous, so the packed tier begins exactly where the inline payload ends.
    let inline = PerlString::from_str(&"a".repeat(15)).unwrap();
    assert!(inline.storage_type().is_inline());

    // Letters belong to no packed alphabet, so past the inline payload they go to the heap.
    let lettered = PerlString::from_str(&"a".repeat(16)).unwrap();
    assert!(lettered.storage_type().is_heap());
    assert_eq!(lettered.len(), 16);

    // Digit-dense content of the same length does not.
    for text in ["1234567890123456", "2.2250738585072e-308", "2026-07-28T14:33:07Z", "192.168.100.200 1.2"] {
        let packed = PerlString::from_str(text).unwrap();
        assert!(packed.storage_type().is_packed(), "{text} should pack");
        assert_eq!(packed.len(), text.len());
        assert_eq!(packed.as_bytes(&mut [0u8; DECODE_MAX]), text.as_bytes());
    }

    // Past the packed capacity there is no non-allocating form left.
    let long = PerlString::from_str(&"1".repeat(31)).unwrap();
    assert!(long.storage_type().is_heap());
    assert_eq!(long.len(), 31);
}

#[test]
fn ascii_from_str_is_unflagged_canonical() {
    let s = PerlString::from_str("hello").unwrap();
    assert!(!s.is_utf8(), "ASCII stores in canonical downgraded form");
    assert_eq!(s.inline_class(), Some(InlineClass::Ascii));
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), Some("hello"));
}

#[test]
fn non_ascii_from_str_is_flagged() {
    let s = PerlString::from_str("héllo").unwrap();
    assert!(s.is_utf8());
    assert_eq!(s.inline_class(), Some(InlineClass::Latin1)); // é is U+00E9: Latin-1 range
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), Some("héllo"));
}

#[test]
fn invalid_bytes_inline_scan_terminal() {
    let s = PerlString::from_bytes([0xFF, 0xFE]).unwrap();
    assert_eq!(s.inline_class(), Some(InlineClass::Bytes));
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), None);
    assert!(!s.is_ascii());
    assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), &[0xFF, 0xFE]);
}

#[test]
fn heap_from_bytes_defers_scanning() {
    let bytes = vec![b'x'; 40];
    let s = PerlString::from_bytes(&bytes).unwrap();
    assert!(s.storage_type().is_heap());

    // as_str triggers the lazy scan and narrows.
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), Some("x".repeat(40).as_str()));
    assert!(s.is_ascii());
}

// ── Character-sequence equality (container-verified cases) ────
#[test]
fn eq_same_flags_is_byte_equality() {
    let a = PerlString::from_str("hello").unwrap();
    let b = PerlString::from_bytes(b"hello").unwrap();
    assert_eq!(a, b); // both unflagged ASCII
}

#[test]
fn eq_cross_flag_same_bytes_can_differ() {
    // Verified perl 5.38: unflagged C3 A9 is the two characters "\xc3\xa9"; flagged it is "é" — not eq.
    let mut flagged = PerlString::from_bytes([0xC3, 0xA9]).unwrap();
    flagged.set_utf8_for_test();
    let unflagged = PerlString::from_bytes([0xC3, 0xA9]).unwrap();
    assert_ne!(flagged, unflagged);
}

#[test]
fn eq_cross_flag_different_bytes_can_match() {
    // Verified perl 5.38: unflagged E9 (latin-1 é) eq flagged C3 A9 (UTF-8 é).
    let mut flagged = PerlString::from_bytes([0xC3, 0xA9]).unwrap();
    flagged.set_utf8_for_test();
    let latin1 = PerlString::from_bytes([0xE9]).unwrap();
    assert_eq!(flagged, latin1);
    assert_eq!(latin1, flagged);
}

#[test]
fn eq_ignores_warned_and_tainted() {
    let a = PerlString::from_str("same").unwrap();
    let mut b = PerlString::from_str("same").unwrap();
    b.mark_warned();
    b.taint();
    assert_eq!(a, b);
    assert_eq!(hash_of(&a), hash_of(&b));
}

// ── Canonical hashing (container-verified hash-key semantics) ─
#[test]
fn hash_key_flag_insensitive() {
    // Verified perl 5.38: utf8::upgrade/downgrade variants of a key are ONE key.
    let mut flagged = PerlString::from_bytes([0xC3, 0xA9]).unwrap();
    flagged.set_utf8_for_test();
    let latin1 = PerlString::from_bytes([0xE9]).unwrap();
    assert_eq!(hash_of(&flagged), hash_of(&latin1), "equal strings must hash equal");
    let mut h: HashMap<PerlString, i32> = HashMap::new();
    h.insert(flagged, 1);
    h.insert(latin1, 2);
    assert_eq!(h.len(), 1, "Perl hash keys are flag-insensitive");
}

// ── Tag transitions ───────────────────────────────────────────
#[test]
fn warned_is_monotonic_and_payload_preserving() {
    let mut s = PerlString::from_str("12abc").unwrap();
    assert!(!s.is_warned());
    s.mark_warned();
    assert!(s.is_warned());
    assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), b"12abc");
    assert_eq!(s.inline_class(), Some(InlineClass::Ascii));
    s.mark_warned(); // idempotent
    assert!(s.is_warned());
}

#[test]
fn taint_round_trip_via_sanctioned_path() {
    let mut s = PerlString::from_str("data").unwrap();
    s.taint();
    assert!(s.is_tainted());
    s.untaint_for_sanctioned_path();
    assert!(!s.is_tainted());
}

#[test]
fn warned_copies_with_the_value() {
    // Verified perl 5.38 (§2.3.4): the warn state is copied on assignment.
    let mut s = PerlString::from_str("abc").unwrap();
    s.mark_warned();
    let copy = s.clone();
    assert!(copy.is_warned());
}

// ── Append transitions (§2.2.5) ───────────────────────────────
#[test]
fn ascii_append_preserves_state() {
    let mut s = PerlString::from_str("abc").unwrap();
    s.push_str("def").unwrap();
    assert_eq!(s.inline_class(), Some(InlineClass::Ascii));
    assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), b"abcdef");
}

#[test]
fn valid_utf8_append_to_ascii_goes_non_ascii() {
    let mut s = PerlString::from_str("abc").unwrap();
    s.push_str("é").unwrap();
    assert_eq!(s.inline_class(), Some(InlineClass::Latin1)); // ASCII + é joins to Latin-1 range
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), Some("abcé"));
}

#[test]
fn inline_overflow_promotes_to_heap_one_way() {
    let mut s = PerlString::from_str(&"a".repeat(20)).unwrap();
    s.push_str("bcdef").unwrap(); // 25 bytes
    assert!(s.storage_type().is_heap());
    assert_eq!(s.len(), 25);
    assert!(s.is_ascii(), "promotion carried the scan knowledge");

    // Shrinking (future truncate) must not demote — pinned when truncate lands.
}

#[test]
fn heap_append_transitions() {
    let mut s = PerlString::from_str(&"a".repeat(30)).unwrap(); // Heap, ASCII known
    s.push_str("é").unwrap();
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]).map(|v| v.len()), Some(32));

    // ASCII + valid-non-ascii → UTF8_NON_ASCII, without rescanning.
    assert!(!s.is_ascii());
    let mut raw = PerlString::from_bytes([0x80u8; 30]).unwrap(); // Heap, UNKNOWN
    raw.push_bytes(&[0x81]).unwrap();
    assert_eq!(raw.as_str(&mut [0u8; DECODE_MAX]), None); // lazy scan resolves to invalid
}

#[test]
fn flag_and_bits_survive_promotion() {
    let mut s = PerlString::from_str(&"é".repeat(15)).unwrap(); // Fifteen stored bytes: full-capacity inline, flagged.
    s.taint();
    assert!(s.storage_type().is_inline(), "thirty encoded bytes compress to a full payload (§2.2.9)");
    s.push_str("é").unwrap(); // A sixteenth character: past every non-heap form — non-ASCII cannot pack.
    assert!(s.storage_type().is_heap());
    assert!(s.is_utf8());
    assert!(s.is_tainted());
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), Some("é".repeat(16).as_str()));
}

// ── Extended-UTF-8 taxonomy (container-verified, §2.2.4) ──────
#[test]
fn extended_taxonomy_inline() {
    // Perl-decodable, Rust-invalid: surrogate, supra-Unicode, minimal FE form.
    for bytes in [&[0xED, 0xA0, 0x80][..], &[0xF4, 0x90, 0x80, 0x80], &[0xFE, 0x82, 0x80, 0x80, 0x80, 0x80, 0x80]] {
        let s = PerlString::from_bytes(bytes).unwrap();
        assert_eq!(s.inline_class(), Some(InlineClass::Extended), "{bytes:02X?}");
        assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), None, "Rust view must reject extended");
        assert!(s.is_perl_utf8_valid(), "perl view must accept extended");
        assert!(!s.is_ascii());
    }

    // Malformed for perl too: overlong, bare continuation, truncated, overlong FF form.
    let overlong_ff: Vec<u8> = std::iter::once(0xFFu8).chain(std::iter::repeat_n(0x80u8, 12)).collect();
    for bytes in [&[0xC0, 0x80][..], &[0x80], &[0xC3], &overlong_ff] {
        let s = PerlString::from_bytes(bytes).unwrap();
        assert_eq!(s.inline_class(), Some(InlineClass::Bytes), "{bytes:02X?}");
        assert!(!s.is_perl_utf8_valid());
    }
}

#[test]
fn extended_taxonomy_heap_lazy() {
    // Heap string ending in an extended sequence: lazy classification narrows to EXTENDED_UTF8.
    let mut bytes = vec![b'a'; 30];
    bytes.extend_from_slice(&[0xF4, 0x90, 0x80, 0x80]);
    let s = PerlString::from_bytes(&bytes).unwrap();
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), None);
    assert!(s.is_perl_utf8_valid());

    // And a malformed heap string classifies INVALID.
    let mut bad = vec![b'a'; 30];
    bad.push(0xC0);
    bad.push(0x80);
    let t = PerlString::from_bytes(&bad).unwrap();
    assert!(!t.is_perl_utf8_valid());
    assert_eq!(t.as_str(&mut [0u8; DECODE_MAX]), None);
}

#[test]
fn ff_form_boundary() {
    // chr(2**36) is the minimal FF form (container-verified); its encoding must validate.  2**36 in extended form:
    // FF + 12 continuations encoding the value.
    let mut v: u64 = 1 << 36;
    let mut conts = [0u8; 12];
    for slot in conts.iter_mut().rev() {
        *slot = 0x80 | (v & 0x3F) as u8;
        v >>= 6;
    }

    let mut seq = vec![0xFFu8];
    seq.extend_from_slice(&conts);
    let s = PerlString::from_bytes(&seq).unwrap();
    assert_eq!(s.inline_class(), Some(InlineClass::Extended), "minimal FF form is perl-valid");

    // One less than the boundary is overlong for FF.
    let mut v2: u64 = (1 << 36) - 1;
    let mut c2 = [0u8; 12];
    for slot in c2.iter_mut().rev() {
        *slot = 0x80 | (v2 & 0x3F) as u8;
        v2 >>= 6;
    }

    let mut seq2 = vec![0xFFu8];
    seq2.extend_from_slice(&c2);
    let t = PerlString::from_bytes(&seq2).unwrap();
    assert_eq!(t.inline_class(), Some(InlineClass::Bytes), "FF encoding a FE-range value is overlong");
}

#[test]
fn extended_append_transitions() {
    let mut s = PerlString::from_bytes([0xF4, 0x90, 0x80, 0x80]).unwrap();
    s.push_str("abc").unwrap();
    assert_eq!(s.inline_class(), Some(InlineClass::Extended), "valid append preserves extended");
    assert!(s.is_perl_utf8_valid());
}

#[test]
fn extended_eq_and_hash_behavior() {
    // A flagged extended string equals no unflagged string (chars above 0xFF) and byte-identical flagged self.
    let mut a = PerlString::from_bytes([0xF4, 0x90, 0x80, 0x80]).unwrap();
    a.set_utf8_for_test();
    let mut b = PerlString::from_bytes([0xF4, 0x90, 0x80, 0x80]).unwrap();
    b.set_utf8_for_test();
    assert_eq!(a, b);
    assert_eq!(hash_of(&a), hash_of(&b));
    let plain = PerlString::from_bytes([0xF4, 0x90, 0x80, 0x80]).unwrap();
    assert_ne!(a, plain, "flag changes the character sequence");
}

// ── Range-tuned lattice (§2.2.4) ──────────────────────────────
#[test]
fn latin1_vs_non_latin1_terminals() {
    let e = PerlString::from_str("é").unwrap(); // U+00E9
    assert_eq!(e.inline_class(), Some(InlineClass::Latin1));
    let cjk = PerlString::from_str("字").unwrap(); // U+5B57
    assert_eq!(cjk.inline_class(), Some(InlineClass::NonLatin1));
    let mixed = PerlString::from_str("é字").unwrap();
    assert_eq!(mixed.inline_class(), Some(InlineClass::NonLatin1), "range joins upward");
}

#[test]
fn unknown_range_classifies_on_ascii_probe() {
    let s = PerlString::from_str(&"é".repeat(20)).unwrap(); // 40 bytes: heap, UTF8_UNKNOWN_RANGE
    assert!(s.storage_type().is_heap());
    assert!(!s.is_ascii(), "probe performs the range classification, not just an ASCII scan");

    // The classification left terminal Latin-1 knowledge behind: cross-flag equality against the downgraded form
    // succeeds (and would fast-negative if the state had wrongly become NON_LATIN1).
    let plain = PerlString::from_bytes([0xE9u8; 20]).unwrap();
    assert_eq!(s, plain);
}

#[test]
fn eq_grid_same_flag_length_mismatch() {
    // Same flags + different byte lengths ⇒ ne, at both flag settings.
    let a = PerlString::from_bytes(b"abc").unwrap();
    let b = PerlString::from_bytes(b"abcd").unwrap();
    assert_ne!(a, b);
    let mut fa = PerlString::from_bytes([0xC3, 0xA9]).unwrap();
    fa.set_utf8_for_test();
    let mut fb = PerlString::from_bytes([0xC3, 0xA9, 0x41]).unwrap();
    fb.set_utf8_for_test();
    assert_ne!(fa, fb);
}

#[test]
fn eq_cross_flag_flagged_longer_positive_and_negative() {
    // Flagged longer CAN match (char count < byte count): é as C3 A9 vs E9 — the positive case.
    let mut f = PerlString::from_bytes([0xC3, 0xA9]).unwrap();
    f.set_utf8_for_test();
    assert_eq!(f, PerlString::from_bytes([0xE9]).unwrap());

    // Flagged longer, mismatch mid-walk.
    assert_ne!(f, PerlString::from_bytes([0xEA]).unwrap());

    // Flagged longer, plain exhausted with flagged characters remaining: "é" + "a" vs just é.
    let mut f2 = PerlString::from_bytes([0xC3, 0xA9, b'a']).unwrap();
    f2.set_utf8_for_test();
    assert_ne!(f2, PerlString::from_bytes([0xE9]).unwrap());

    // And the fully-matching longer-flagged multi-char case.
    assert_eq!(f2, PerlString::from_bytes([0xE9, b'a']).unwrap());
}

#[test]
fn eq_cross_flag_equal_length_ascii_can_match() {
    // Equal byte lengths must NOT be decided-false when the flagged side has no multi-byte sequence.
    let mut f = PerlString::from_bytes(b"ab").unwrap();
    f.set_utf8_for_test();
    assert_eq!(f, PerlString::from_bytes(b"ab").unwrap());
    assert_ne!(f, PerlString::from_bytes(b"ba").unwrap());
}

#[test]
fn eq_grid_both_flagged_terminal_mismatch() {
    // The flagged twin of the exclusivity row.
    let mut latin1 = PerlString::from_bytes([0xC3, 0xA9]).unwrap();
    latin1.set_utf8_for_test();
    let mut mal = PerlString::from_bytes([0xC0, 0x80]).unwrap();
    mal.set_utf8_for_test();
    assert_ne!(latin1, mal);
}

#[test]
fn eq_grid_valid_vs_invalid_same_flag() {
    // Flagged terminal Rust-invalid vs flagged known-Rust-valid nonterminal (heap UTF8_UNKNOWN_RANGE): valid bytes
    // never equal invalid bytes.
    let flagged_valid = PerlString::from_str(&"é".repeat(20)).unwrap(); // heap, flagged, UNKNOWN_RANGE
    let mut ext = PerlString::from_bytes([0xF4, 0x90, 0x80, 0x80]).unwrap();
    ext.set_utf8_for_test();
    assert_ne!(flagged_valid, ext);
    assert_ne!(ext, flagged_valid);
}

#[test]
fn eq_grid_ascii_vs_non_ascii_both_orientations() {
    // Flagged-ASCII vs unflagged known-non-ASCII.
    let mut fa = PerlString::from_bytes(b"abc").unwrap();
    fa.set_utf8_for_test();
    assert_ne!(fa, PerlString::from_bytes([0x80, 0x81, 0x82]).unwrap());

    // Unflagged-ASCII vs flagged known-non-ASCII (Latin-1).
    let mut fl = PerlString::from_bytes([0xC3, 0xA9]).unwrap();
    fl.set_utf8_for_test();
    assert_ne!(PerlString::from_bytes(b"ab").unwrap(), fl);
}

#[test]
fn eq_grid_same_flag_terminal_mismatch() {
    // Differing terminals, both unflagged: decided without memcmp (exclusivity law).
    let latin1 = PerlString::from_bytes([0xC3, 0xA9]).unwrap(); // valid, Latin-1-range... as bytes: classified
    let malformed = PerlString::from_bytes([0xC0, 0x80]).unwrap();
    assert_ne!(latin1, malformed);
    let ascii = PerlString::from_bytes(b"ab").unwrap();
    assert_ne!(ascii, latin1);
}

#[test]
fn eq_grid_flagged_malformed_vs_unflagged_is_false() {
    let mut mal = PerlString::from_bytes([0x80]).unwrap();
    mal.set_utf8_for_test(); // flagged malformed
    let plain = PerlString::from_bytes([0x80]).unwrap();
    assert_ne!(mal, plain, "upgrade of unflagged is valid; never matches malformed bytes");
}

#[test]
fn eq_reverse_malformed_orientation_can_match() {
    // Unflagged MALFORMED-classified bytes are just bytes: \x80 as a character equals flagged C2 80.
    let plain = PerlString::from_bytes([0x80]).unwrap();
    assert_eq!(plain.inline_class(), Some(InlineClass::Bytes));
    let mut flagged = PerlString::from_bytes([0xC2, 0x80]).unwrap();
    flagged.set_utf8_for_test();
    assert_eq!(flagged, plain, "the grid must not shortcut this orientation");
}

#[test]
fn eq_grid_length_rows() {
    // plain longer than flagged: impossible.
    let mut flagged = PerlString::from_bytes([0xC3, 0xA9]).unwrap();
    flagged.set_utf8_for_test();
    let plain3 = PerlString::from_bytes([0xE9, 0xE9, 0xE9]).unwrap();
    assert_ne!(flagged, plain3);

    // flagged known Latin-1 (has a 2-byte char) with equal byte lengths: impossible.
    let plain2 = PerlString::from_bytes([0xC3, 0xA9]).unwrap();
    assert_ne!(flagged, plain2);
}

#[test]
fn streaming_compare_narrows_on_completed_walk() {
    // Heap flagged UTF8_UNKNOWN_RANGE vs matching latin1 bytes: undecided by the grid, resolved by the single walk,
    // which narrows both sides.
    let flagged = PerlString::from_str(&"é".repeat(20)).unwrap(); // heap, flagged, UNKNOWN_RANGE
    let plain = PerlString::from_bytes([0xE9u8; 20]).unwrap();
    assert_eq!(flagged, plain);

    // The completed walk narrowed the flagged side to UTF8_LATIN1: is_ascii is now a state read.
    assert!(!flagged.is_ascii());
    assert!(!plain.is_ascii());
}

#[test]
fn cheap_probe_defers_range() {
    let s = PerlString::from_str(&"é".repeat(20)).unwrap(); // heap, UTF8_UNKNOWN_RANGE
    assert!(!s.is_ascii()); // cheap probe: narrows to UTF8_NON_ASCII, range still deferred

    // Equality resolves range on demand and still matches the downgraded form.
    let plain = PerlString::from_bytes([0xE9u8; 20]).unwrap();
    assert_eq!(s, plain);

    // And a wide heap string resolved through the same path fast-negatives.
    let wide = PerlString::from_str(&"字".repeat(14)).unwrap(); // 42 bytes heap
    assert!(!wide.is_ascii());
    let wide_plain = PerlString::from_bytes(wide.as_bytes(&mut [0u8; DECODE_MAX])).unwrap();
    assert_ne!(wide, wide_plain);
}

#[test]
fn eq_fast_negative_for_beyond_latin1() {
    // A flagged string containing U+0100+ equals no unflagged string, regardless of bytes.
    let wide = PerlString::from_str("abc字").unwrap();
    assert!(wide.is_utf8());
    let plain = PerlString::from_bytes(wide.as_bytes(&mut [0u8; DECODE_MAX])).unwrap();
    assert_ne!(wide, plain);

    // And the é (Latin-1) case still compares by character as before.
    let e_flagged = PerlString::from_str("é").unwrap();
    let e_latin1 = PerlString::from_bytes([0xE9]).unwrap();
    assert_eq!(e_flagged, e_latin1);
}

#[test]
fn append_range_join_semantics() {
    let mut s = PerlString::from_str("abc").unwrap(); // Ascii
    s.push_str("é").unwrap();
    assert_eq!(s.inline_class(), Some(InlineClass::Latin1));
    s.push_str("字").unwrap();
    assert_eq!(s.inline_class(), Some(InlineClass::NonLatin1));

    // This append carries the content past the inline payload, and non-ASCII bytes belong to no packed alphabet, so the
    // string lands on the heap — where the same join rule holds, read through the heap lattice.
    s.push_str("more ascii").unwrap();
    assert!(s.storage_type().is_heap());
    assert_eq!(s.scan_state(), scan::UTF8_NON_LATIN1, "range cannot go back down on append");
}

#[test]
fn heap_append_range_join() {
    let mut s = PerlString::from_bytes(b"a".repeat(30)).unwrap();
    assert!(s.is_ascii()); // narrows heap state to ASCII
    s.push_str("é").unwrap(); // ASCII join Latin-1 = Latin-1, no rescan
    let latin1_equiv: Vec<u8> = b"a".repeat(30).iter().copied().chain([0xE9u8]).collect();
    let plain = PerlString::from_bytes(&latin1_equiv).unwrap();
    let mut flagged = s;
    flagged.set_utf8_for_test();
    assert_eq!(flagged, plain, "Latin-1-range heap string equals its downgraded form");
}

// ── Exhaustive grid verification (§2.3.5) ─────────────────────
/// Ground truth: pure character-sequence comparison with no grid and no state consultation.
fn reference_eq(a: &PerlString, b: &PerlString) -> bool {
    fn chars_of(s: &PerlString) -> Vec<u32> {
        if s.is_utf8() {
            flagged_chars(s.as_bytes(&mut [0u8; DECODE_MAX])).collect()
        } else {
            s.as_bytes(&mut [0u8; DECODE_MAX]).iter().map(|&b| b as u32).collect()
        }
    }

    chars_of(a) == chars_of(b)
}

/// The design's decided-false table (§2.3.5 rows 1–4), transcribed independently of the implementation.
fn design_decides_false(a: &PerlString, sa: u8, b: &PerlString, sb: u8) -> bool {
    if a.is_utf8() == b.is_utf8() {
        return a.len() != b.len()
            || (scan::is_terminal(sa) && scan::is_terminal(sb) && sa != sb)
            || (scan::is_terminal(sa) && !scan::is_rust_valid(sa) && scan::is_rust_valid(sb))
            || (scan::is_terminal(sb) && !scan::is_rust_valid(sb) && scan::is_rust_valid(sa))
            || (sa == scan::ASCII && scan::is_known_non_ascii(sb))
            || (sb == scan::ASCII && scan::is_known_non_ascii(sa));
    }

    let (f, p, sf, sp) = if a.is_utf8() { (a, b, sa, sb) } else { (b, a, sb, sa) };

    p.len() > f.len()
        || ((sf == scan::UTF8_LATIN1 || sf == scan::UTF8_NON_ASCII) && p.len() == f.len())
        || (sf == scan::ASCII && scan::is_known_non_ascii(sp))
        || (sp == scan::ASCII && scan::is_known_non_ascii(sf))
        || scan::is_known_beyond_latin1(sf)
        || sf == scan::MALFORMED_UTF8
}

/// Build every reachable (state, storage) witness configuration, with several byte contents behind the indeterminate
/// states.  Each witness's state is asserted at construction.
fn grid_witnesses() -> Vec<(String, PerlString)> {
    let mut out: Vec<(String, PerlString)> = Vec::new();

    let mut push = |name: &str, s: PerlString, want: u8| {
        assert_eq!(s.scan_state(), want, "witness {name} state");
        out.push((name.to_string(), s));
    };

    // Inline terminals.
    push("inl-ascii", PerlString::from_bytes(b"ab").unwrap(), scan::ASCII);
    push("inl-latin1", PerlString::from_bytes([0xC3, 0xA9]).unwrap(), scan::UTF8_LATIN1);
    push("inl-nonlatin1", PerlString::from_str("字").unwrap(), scan::UTF8_NON_LATIN1);
    push("inl-extended", PerlString::from_bytes([0xF4, 0x90, 0x80, 0x80]).unwrap(), scan::EXTENDED_UTF8);
    push("inl-malformed", PerlString::from_bytes([0x80]).unwrap(), scan::MALFORMED_UTF8);

    // Heap terminals (narrowed via probes).
    let h_ascii = PerlString::from_bytes(b"a".repeat(24)).unwrap();
    assert!(h_ascii.is_ascii());
    push("heap-ascii", h_ascii, scan::ASCII);
    let h_l1 = PerlString::from_str(&"é".repeat(16)).unwrap();
    let _ = h_l1.char_len(); // classifies via the fused pass
    push("heap-latin1", h_l1, scan::UTF8_LATIN1);
    let h_nl1 = PerlString::from_str(&"字".repeat(8)).unwrap();
    let _ = h_nl1.char_len();
    push("heap-nonlatin1", h_nl1, scan::UTF8_NON_LATIN1);
    let h_ext = PerlString::from_bytes([0xF4, 0x90, 0x80, 0x80].repeat(6)).unwrap();
    assert!(h_ext.is_perl_utf8_valid());
    push("heap-extended", h_ext, scan::EXTENDED_UTF8);
    let h_mal = PerlString::from_bytes([0x80; 24]).unwrap();
    assert!(!h_mal.is_perl_utf8_valid());
    push("heap-malformed", h_mal, scan::MALFORMED_UTF8);

    // Indeterminate states, several contents each.
    push("heap-unknown-ascii", PerlString::from_bytes(b"x".repeat(24)).unwrap(), scan::UNKNOWN);
    push("heap-unknown-latin1", PerlString::from_bytes([0xC3, 0xA9].repeat(16)).unwrap(), scan::UNKNOWN);
    push("heap-unknown-malformed", PerlString::from_bytes([0x81; 23]).unwrap(), scan::UNKNOWN);
    push("heap-ur-latin1", PerlString::from_str(&"é".repeat(16)).unwrap(), scan::UTF8_UNKNOWN_RANGE);
    push("heap-ur-wide", PerlString::from_str(&"字".repeat(8)).unwrap(), scan::UTF8_UNKNOWN_RANGE);
    let na8_l1 = PerlString::from_str(&"é".repeat(16)).unwrap();
    assert!(!na8_l1.is_ascii());
    push("heap-na8-latin1", na8_l1, scan::UTF8_NON_ASCII);
    let na8_wide = PerlString::from_str(&"字".repeat(8)).unwrap();
    assert!(!na8_wide.is_ascii());
    push("heap-na8-wide", na8_wide, scan::UTF8_NON_ASCII);
    let na_raw = PerlString::from_bytes([0x82; 24]).unwrap();
    assert!(!na_raw.is_ascii());
    push("heap-nonascii-raw", na_raw, scan::NON_ASCII);
    let na_raw_valid = PerlString::from_bytes([0xC3, 0xA9].repeat(16)).unwrap();
    assert!(!na_raw_valid.is_ascii());
    push("heap-nonascii-valid-bytes", na_raw_valid, scan::NON_ASCII);

    out
}

#[test]
fn full_scan_runs_once_then_state_answers() {
    // A heap string's first as_str pays one validation pass (+ one classification); afterwards every question is a
    // state read — the never-scan-twice law, mechanically.
    let s = PerlString::from_bytes([0xC3, 0xA9].repeat(16)).unwrap(); // heap UNKNOWN: 32 bytes exceed the intake
    eq_probe::reset();
    assert!(s.as_str(&mut [0u8; DECODE_MAX]).is_some());
    let (scans_first, _) = eq_probe::scans();
    assert_eq!(scans_first, 1, "first as_str must pay exactly ONE fused pass — more is double-scanning");
    eq_probe::reset();
    assert!(s.as_str(&mut [0u8; DECODE_MAX]).is_some());
    assert!(s.is_perl_utf8_valid());
    assert!(!s.is_ascii());
    assert_eq!(s.char_len(), Some(16));
    assert_eq!(eq_probe::scans(), (0, 0), "cached state must answer every subsequent question");
}

#[test]
fn cheap_probe_bails_at_first_high_bit() {
    // The ninth state's raison d'être (§2.2.4): the ASCII probe examines O(first-high-bit) bytes.
    let mut bytes = vec![0x80u8];
    bytes.extend_from_slice(&b"a".repeat(5000));
    let s = PerlString::from_bytes(&bytes).unwrap(); // heap UNKNOWN
    eq_probe::reset();
    assert!(!s.is_ascii());
    let (_, probe_bytes) = eq_probe::scans();
    assert_eq!(probe_bytes, 1, "first byte is high: the probe must bail immediately");
    assert_eq!(s.scan_state(), scan::NON_ASCII);

    // Same bail on the validity-known tier.
    let f = PerlString::from_str(&format!("é{}", "a".repeat(5000))).unwrap(); // heap UNKNOWN_RANGE
    eq_probe::reset();
    assert!(!f.is_ascii());
    let (_, pb2) = eq_probe::scans();
    assert!(pb2 <= 2, "high bit at byte 0: probe examined {pb2} bytes");
    assert_eq!(f.scan_state(), scan::UTF8_NON_ASCII);
}

#[test]
fn eq_short_circuits_at_first_mismatch_depth() {
    // The asymptotic property "short-circuit" names: characters consumed is O(mismatch position), not O(n).
    let big = 10_000;

    // Mismatch at position 0: flagged é-string vs plain starting with a different byte.
    let flagged = PerlString::from_str(&"é".repeat(big)).unwrap();
    let mut plain_bytes = vec![0xE9u8; big];
    plain_bytes[0] = 0xAA;
    let plain = PerlString::from_bytes(&plain_bytes).unwrap();
    eq_probe::reset();
    assert_ne!(flagged, plain);
    let (_, entries, chars) = eq_probe::snapshot();
    assert_eq!(entries, 1, "undecided pair must go to the walk");
    assert!(chars <= 2, "mismatch at position 0 must be found within the first characters, consumed {chars}");

    // Mismatch at position 100.
    let mut plain_bytes2 = vec![0xE9u8; big];
    plain_bytes2[100] = 0xAA;
    let plain2 = PerlString::from_bytes(&plain_bytes2).unwrap();
    let flagged2 = PerlString::from_str(&"é".repeat(big)).unwrap();
    eq_probe::reset();
    assert_ne!(flagged2, plain2);
    let (_, _, chars2) = eq_probe::snapshot();
    assert!((100..=102).contains(&chars2), "mismatch at 100 must consume ~101 characters, consumed {chars2}");

    // Full equality consumes everything exactly once.
    let flagged3 = PerlString::from_str(&"é".repeat(big)).unwrap();
    let plain3 = PerlString::from_bytes(vec![0xE9u8; big]).unwrap();
    eq_probe::reset();
    assert_eq!(flagged3, plain3);
    let (_, _, chars3) = eq_probe::snapshot();
    assert_eq!(chars3, big, "completed walk consumes each character exactly once");
}

#[test]
fn eq_grid_decided_pairs_perform_no_scan() {
    // Observable-state companion: a grid-decided comparison must leave an indeterminate operand's state untouched (no
    // scan happened on it).
    let wide = PerlString::from_str("字").unwrap(); // inline NL1, flagged
    assert!(wide.is_utf8()); // from_str of non-ASCII is flagged already
    let unknown = PerlString::from_bytes([0x90u8; 24]).unwrap(); // heap UNKNOWN
    assert_eq!(unknown.scan_state(), scan::UNKNOWN);
    eq_probe::reset();
    assert_ne!(wide, unknown); // cross-flag, flagged NL1: grid row 4
    let (hits, entries, _) = eq_probe::snapshot();
    assert_eq!((hits, entries), (1, 0));
    assert_eq!(unknown.scan_state(), scan::UNKNOWN, "decided comparison must not scan the other operand");
}

#[test]
fn eq_grid_exhaustive_over_all_state_flag_combinations() {
    // Every (witness × flag) against every (witness × flag).  Witnesses are constructed FRESH for every pair: eq
    // narrows scan states as a side effect and heap clones share buffer state, so reused witnesses would silently
    // degrade indeterminate-state coverage into terminal-state coverage.
    let n = grid_witnesses().len();
    let fresh = |i: usize, flagged: bool| -> (String, PerlString) {
        let (name, mut w) = grid_witnesses().swap_remove(i);
        if flagged {
            let st = w.scan_state();
            w.set_utf8_for_test();
            assert_eq!(w.scan_state(), st, "flagging must not disturb scan state ({name})");
            (format!("{name}+flag"), w)
        } else {
            (name, w)
        }
    };

    let mut pairs = 0usize;
    let mut decided = 0usize;
    for ia in 0..n {
        for fa in [false, true] {
            for ib in 0..n {
                for fb in [false, true] {
                    let (na, a) = fresh(ia, fa);
                    let (nb, b) = fresh(ib, fb);
                    let (sa, sb) = (a.scan_state(), b.scan_state());
                    super::eq_probe::reset();
                    let got = a == b;
                    let (grid_hits, walk_entries, _) = super::eq_probe::snapshot();
                    let (full_scans, _) = super::eq_probe::scans();
                    assert_eq!(full_scans, 0, "eq performed a full scan on {na} vs {nb} — the walk is its only byte access");
                    let want = reference_eq(&a, &b);
                    assert_eq!(got, want, "eq vs oracle for {na} vs {nb} (states {sa}/{sb})");

                    if design_decides_false(&a, sa, &b, sb) {
                        decided += 1;
                        assert!(!want, "design table unsound for {na} vs {nb} (states {sa}/{sb})");

                        // The mechanism assertion: a decided pair must be decided BY THE GRID — same-flag decided pairs
                        // may resolve in the pre-memcmp rows or memcmp's length check; cross-flag decided pairs must
                        // hit a grid row and must never enter the streaming walk.
                        if a.is_utf8() != b.is_utf8() {
                            assert!(grid_hits >= 1, "grid row failed to fire for {na} vs {nb} (states {sa}/{sb})");
                            assert_eq!(walk_entries, 0, "walk entered on decided pair {na} vs {nb} (states {sa}/{sb})");
                        }
                    }

                    pairs += 1;
                }
            }
        }
    }

    assert_eq!(pairs, n * n * 4);
    assert!(decided > pairs / 4, "sanity: a healthy fraction of pairs should be grid-decided ({decided}/{pairs})");
}

// ── Blocked walk (§2.3.5) ─────────────────────────────────────

#[test]
fn blocked_walk_gated_spans_and_ladder_straddle() {
    // A Latin-1 character straddling the first ladder boundary (64): gated span, dirty block, gated tail.
    let mut src = String::new();
    for _ in 0..63 {
        src.push('a');
    }

    src.push('é'); // bytes 63..65: straddles the 64 boundary
    src.push_str(&"b".repeat(200));
    let f = PerlString::from_str(&src).unwrap();
    let mut twin = vec![b'a'; 63];
    twin.push(0xE9);
    twin.extend_from_slice(&b"b".repeat(200));
    let p = PerlString::from_bytes(&twin).unwrap();
    assert_eq!(f, p);

    // Long pure-ASCII cross-flag pair: decided entirely by gated memcmp spans, late mismatch caught.
    let mut fa = PerlString::from_bytes(b"a".repeat(9000)).unwrap();
    fa.set_utf8_for_test();
    let _ = fa.is_ascii(); // ASCII state on the flagged side would grid-decide vs known-non-ASCII only
    let mut good = b"a".repeat(9000);
    let eq_twin = PerlString::from_bytes(&good).unwrap();
    assert_eq!(fa, eq_twin);
    good[8999] = b'b';
    let ne_twin = PerlString::from_bytes(&good).unwrap();
    eq_probe::reset();
    assert_ne!(fa, ne_twin);
    let (_, _, consumed) = eq_probe::snapshot();
    assert!(consumed >= 8192, "the walk must have streamed the long equal prefix, consumed {consumed}");

    // Mismatch inside the FIRST ladder block of a long string: consumption bounded by one cache line.
    let flagged_long = PerlString::from_str(&"é".repeat(10_000)).unwrap();
    let bad = vec![0xAAu8; 10_000];
    let plain_bad = PerlString::from_bytes(&bad).unwrap();
    eq_probe::reset();
    assert_ne!(flagged_long, plain_bad);
    let (_, _, chars0) = eq_probe::snapshot();
    assert!(chars0 <= WALK_FIRST_BLOCK, "first-block mismatch must stay within the first walk block, consumed {chars0}");
}

// ── Dual-calculation hashing (§2.3.5) ─────────────────────────
fn digest_of(s: &PerlString) -> u64 {
    s.content_digest()
}

#[test]
fn hash_dual_calculation_is_single_fetch_and_keeps_knowledge() {
    // Unresolved flagged heap string: ONE fused pass computes both candidates, decides, and classifies.
    let s = PerlString::from_str(&"é".repeat(20)).unwrap(); // heap, flagged, UNKNOWN_RANGE
    eq_probe::reset();
    let d = digest_of(&s);
    assert_eq!(eq_probe::scans(), (1, 0), "dual calculation is one fetch, no probes");
    assert_eq!(s.scan_state(), scan::UTF8_LATIN1, "the pass's classification is kept");
    eq_probe::reset();
    assert_eq!(s.char_len(), Some(20), "and so is the character count");
    assert_eq!(eq_probe::scans(), (0, 0));

    // The downgraded digest matches the unflagged equal (the HashMap-key requirement).
    let plain = PerlString::from_bytes([0xE9u8; 20]).unwrap();
    assert_eq!(d, digest_of(&plain));

    // A repeat hash uses the known-Latin-1 single-emission path: still exactly one fetch.
    eq_probe::reset();
    assert_eq!(digest_of(&s), d);
    assert_eq!(eq_probe::scans().0, 1, "known-range emission is one pass, never two");
}

#[test]
fn hash_dual_calculation_wide_and_malformed_outcomes() {
    // Wide content: the raw candidate wins; byte-identical flagged strings agree.
    let a = PerlString::from_str(&"字".repeat(14)).unwrap(); // heap, flagged, UNKNOWN_RANGE
    let b = PerlString::from_str(&"字".repeat(14)).unwrap();
    eq_probe::reset();
    let da = digest_of(&a);
    assert_eq!(eq_probe::scans().0, 1);
    assert_eq!(a.scan_state(), scan::UTF8_NON_LATIN1, "classification kept on the wide outcome too");
    assert_eq!(da, digest_of(&b));

    // Malformed discovered mid-pass: raw digest, MALFORMED_UTF8 recorded, agrees with the known-malformed path on
    // byte-identical content.
    let mut bad_bytes = vec![b'a'; 30];
    bad_bytes.push(0xC0);
    bad_bytes.push(0x80);
    let mut m1 = PerlString::from_bytes(&bad_bytes).unwrap();
    m1.set_utf8_for_test(); // flagged, heap UNKNOWN
    eq_probe::reset();
    let dm = digest_of(&m1);
    assert_eq!(eq_probe::scans().0, 1);
    assert_eq!(m1.scan_state(), scan::MALFORMED_UTF8);
    let mut m2 = PerlString::from_bytes(&bad_bytes).unwrap();
    m2.set_utf8_for_test();
    assert!(!m2.is_perl_utf8_valid()); // pre-classify: takes the known-malformed digest path
    assert_eq!(dm, digest_of(&m2), "dual-discovered and pre-known malformed digests agree");
}

#[test]
fn hash_dual_calculation_across_block_boundary() {
    // A Latin-1 character straddling the grid boundary during the dual pass: the downgraded digest must still match the
    // unflagged twin byte-for-byte.
    let mut flagged_src = String::with_capacity(CLASSIFY_BLOCK + 8);
    for _ in 0..CLASSIFY_BLOCK - 1 {
        flagged_src.push('a');
    }

    flagged_src.push('é');
    flagged_src.push_str("tail");
    let f = PerlString::from_str(&flagged_src).unwrap(); // flagged, UNKNOWN_RANGE

    let mut twin = vec![b'a'; CLASSIFY_BLOCK - 1];
    twin.push(0xE9);
    twin.extend_from_slice(b"tail");
    let p = PerlString::from_bytes(&twin).unwrap();

    assert_eq!(digest_of(&f), digest_of(&p));
    assert_eq!(f.scan_state(), scan::UTF8_LATIN1);
    assert_eq!(f.char_len(), Some(CLASSIFY_BLOCK - 1 + 1 + 4));
}

// ── Blocked hybrid classifier boundaries (§2.2.5) ─────────────
/// Test-only reference: the scalar single-byte-scan classifier, transcribed as the oracle for the blocked hybrid (same
/// decode rules, no blocking).
fn reference_classify(bytes: &[u8]) -> (u8, usize) {
    let mut facts = ScanFacts::default();
    match scalar_decode_span(bytes, 0, bytes.len(), &mut facts, |_| {}) {
        Some(_) => (facts.state(), facts.chars),
        None => (scan::MALFORMED_UTF8, 0),
    }
}

#[test]
fn block_boundary_straddles_every_sequence_length() {
    // Sequences of every length, split at every interior offset across the block boundary.
    let mut ff_min = vec![0xFFu8]; // minimal FF form: 2^36
    let mut v: u64 = 1 << 36;
    let mut conts = [0u8; 12];
    for slot in conts.iter_mut().rev() {
        *slot = 0x80 | (v & 0x3F) as u8;
        v >>= 6;
    }

    ff_min.extend_from_slice(&conts);

    let mut fe_min = vec![0xFEu8]; // minimal FE form: 2^31
    let mut v2: u64 = 1 << 31;
    let mut c2 = [0u8; 6];
    for slot in c2.iter_mut().rev() {
        *slot = 0x80 | (v2 & 0x3F) as u8;
        v2 >>= 6;
    }

    fe_min.extend_from_slice(&c2);

    let cases: [(&[u8], u8); 5] = [
        ("é".as_bytes(), scan::UTF8_LATIN1),
        ("字".as_bytes(), scan::UTF8_NON_LATIN1),
        ("\u{10000}".as_bytes(), scan::UTF8_NON_LATIN1),
        (&fe_min, scan::EXTENDED_UTF8),
        (&ff_min, scan::EXTENDED_UTF8),
    ];

    for (seq, want_state) in cases {
        for cut in 1..seq.len() {
            // The sequence begins `cut` bytes before the boundary, so the boundary falls inside it.
            let lead_len = CLASSIFY_BLOCK - cut;
            let mut bytes = vec![b'a'; lead_len];
            bytes.extend_from_slice(seq);
            bytes.extend_from_slice(b"tail");
            let (st, chars) = classify_full(&bytes);
            assert_eq!(st, want_state, "state for seq len {} cut {}", seq.len(), cut);
            assert_eq!(chars, lead_len + 1 + 4, "chars for seq len {} cut {}", seq.len(), cut);
        }
    }
}

#[test]
fn block_boundaries_realign_to_the_grid_after_straddles() {
    // Sequences straddling TWO consecutive fixed grid boundaries: correctness here requires the second block to end at
    // the absolute grid multiple, not at a drifted offset.
    let mut bytes = vec![b'a'; CLASSIFY_BLOCK - 1];
    bytes.extend_from_slice("字".as_bytes()); // straddles boundary 1 (cut after 1 of 3 bytes)
    while bytes.len() < 2 * CLASSIFY_BLOCK - 1 {
        bytes.push(b'b');
    }

    bytes.extend_from_slice("é".as_bytes()); // straddles boundary 2 exactly
    bytes.extend_from_slice(b"tail");

    let (st, chars) = classify_full(&bytes);
    assert_eq!(st, scan::UTF8_NON_LATIN1);

    // chars: (BLOCK-1) a's + 字 + b-fill + é + 4 tail.
    let b_fill = (2 * CLASSIFY_BLOCK - 1) - (CLASSIFY_BLOCK - 1 + 3);
    assert_eq!(chars, (CLASSIFY_BLOCK - 1) + 1 + b_fill + 1 + 4);
}

#[test]
fn block_boundary_truncation_and_malformation() {
    // Lead byte as the final byte of the slice, exactly at the boundary: truncated.
    let mut t = vec![b'a'; CLASSIFY_BLOCK - 1];
    t.push(0xC3);
    assert_eq!(classify_full(&t), (scan::MALFORMED_UTF8, 0));

    // Bad continuation lands in the next block: malformed.
    let mut m = vec![b'a'; CLASSIFY_BLOCK - 1];
    m.extend_from_slice(&[0xC3, 0x28]);
    assert_eq!(classify_full(&m), (scan::MALFORMED_UTF8, 0));
}

#[test]
fn blocked_hybrid_matches_reference_on_corpus() {
    // Deterministic pseudo-random corpus mixing every content class, sized to span multiple blocks.
    let snippets: [&[u8]; 7] = [
        b"plain ascii run ",
        "éàçñ".as_bytes(),
        "字典漢".as_bytes(),
        "\u{10000}\u{10FFFF}".as_bytes(),
        &[0xED, 0xA0, 0x80],       // surrogate: extended
        &[0xF4, 0x90, 0x80, 0x80], // supra-Unicode: extended
        &[0xC0, 0x80],             // overlong: malformed
    ];

    let mut rng: u64 = 0x243F_6A88_85A3_08D3;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    // Several compositions, each ~3 blocks long; the last snippet index drawn caps which classes appear so the corpus
    // covers pure-ASCII, valid-only, extended, and malformed mixes.
    for cap in [1usize, 3, 4, 6, 7] {
        let mut bytes = Vec::with_capacity(3 * CLASSIFY_BLOCK + 64);
        while bytes.len() < 3 * CLASSIFY_BLOCK {
            let pick = (next() as usize) % cap;
            bytes.extend_from_slice(snippets[pick]);
        }
        assert_eq!(classify_full(&bytes), reference_classify(&bytes), "corpus cap {cap}");
    }
}

#[test]
fn blocked_known_valid_boundaries() {
    // A Latin-1 sequence straddling the boundary: continuation byte in the next block is not a character.
    let mut s = String::with_capacity(CLASSIFY_BLOCK + 8);
    for _ in 0..CLASSIFY_BLOCK - 1 {
        s.push('a');
    }

    s.push('é');
    s.push_str("tail");
    let (st, chars) = classify_known_valid(s.as_bytes());
    assert_eq!(st, scan::UTF8_LATIN1);
    assert_eq!(chars, CLASSIFY_BLOCK - 1 + 1 + 4);

    // A wide character first appearing blocks later still bails (block-granular, count forfeited).
    let mut w = String::with_capacity(2 * CLASSIFY_BLOCK + 8);
    for _ in 0..2 * CLASSIFY_BLOCK {
        w.push('a');
    }

    w.push('字');
    assert_eq!(classify_known_valid(w.as_bytes()), (scan::UTF8_NON_LATIN1, 0));

    // Multi-block pure Latin-1: exact count.
    let l = "é".repeat(CLASSIFY_BLOCK); // 2 bytes each: two blocks
    assert_eq!(classify_known_valid(l.as_bytes()), (scan::UTF8_LATIN1, CLASSIFY_BLOCK));
}

// ── Character-length cache (§2.2.4) ───────────────────────────
#[test]
fn char_len_semantics_and_caching() {
    // ASCII: chars == bytes, no scan at all when state is known.
    let a = PerlString::from_bytes(b"ab".repeat(15)).unwrap();
    assert!(a.is_ascii());
    eq_probe::reset();
    assert_eq!(a.char_len(), Some(30));
    assert_eq!(eq_probe::scans().0, 0, "ASCII char_len is a length read");

    // Latin-1 inline: the transcoded units are the flagged-side characters, so the count is the stored nibble — no scan
    // at all, where the raw-byte tier paid a recount.
    let li = PerlString::from_bytes([0xC3, 0xA9].repeat(12)).unwrap();
    assert!(li.storage_type().is_inline(), "24 compressible bytes live inline now (§2.2.9)");
    eq_probe::reset();
    assert_eq!(li.char_len(), Some(12));
    assert_eq!(eq_probe::scans().0, 0, "the count is the stored nibble: no pass at all");

    // Latin-1 heap: first call pays ONE fused pass; second call is a cache read.
    let l = PerlString::from_bytes([0xC3, 0xA9].repeat(16)).unwrap();
    eq_probe::reset();
    assert_eq!(l.char_len(), Some(16));
    assert_eq!(eq_probe::scans().0, 1, "exactly one fused pass classifies and counts");
    eq_probe::reset();
    assert_eq!(l.char_len(), Some(16));
    assert!(l.as_str(&mut [0u8; DECODE_MAX]).is_some());
    assert_eq!(eq_probe::scans().0, 0, "count and state both cached from the one pass");

    // Extended: counted (a 4-byte and a 13-byte character are one character each).
    let e = PerlString::from_bytes([0xF4, 0x90, 0x80, 0x80].repeat(6)).unwrap();
    assert_eq!(e.char_len(), Some(6));

    // Surrogates count one character per encoded sequence; perl never merges pairs.  Container-verified:
    // length(chr 0xD800) == 1; a CESU-style pair decodes to TWO characters (D800, DC00), length 2, distinct from the
    // one-character astral U+10000.
    let lone = PerlString::from_bytes([0xED, 0xA0, 0x80]).unwrap();
    assert_eq!(lone.inline_class(), Some(InlineClass::Extended));
    assert_eq!(lone.char_len(), Some(1));
    let cesu_pair = PerlString::from_bytes([0xED, 0xA0, 0x80, 0xED, 0xB0, 0x80]).unwrap();
    assert_eq!(cesu_pair.char_len(), Some(2), "pairs are two characters, never merged");
    let astral = PerlString::from_str("\u{10000}").unwrap();
    assert_eq!(astral.char_len(), Some(1));

    // Malformed: None (ops layer owns perl's warning behavior).
    let m = PerlString::from_bytes([0x80; 24]).unwrap();
    assert_eq!(m.char_len(), None);

    // Inline recount, all classes.
    assert_eq!(PerlString::from_str("héllo").unwrap().char_len(), Some(5));
    assert_eq!(PerlString::from_str("字").unwrap().char_len(), Some(1));
    assert_eq!(PerlString::from_bytes([0x80]).unwrap().char_len(), None);
}

#[test]
fn char_len_maintained_through_append() {
    let mut s = PerlString::from_bytes([0xC3, 0xA9].repeat(12)).unwrap(); // heap
    assert_eq!(s.char_len(), Some(12)); // classify + count: one pass
    eq_probe::reset();
    s.push_str("abc").unwrap(); // classification of the ADDED bytes only
    assert_eq!(s.char_len(), Some(15), "count maintained incrementally");
    let (full, _) = eq_probe::scans();
    assert_eq!(full, 1, "only the appended content was scanned (its own classification pass)");
}

#[test]
fn char_count_shared_across_cow_sharers() {
    let a = PerlString::from_bytes([0xC3, 0xA9].repeat(12)).unwrap();
    let b = a.clone(); // shares the buffer
    assert_eq!(a.char_len(), Some(12)); // pays the pass
    eq_probe::reset();
    assert_eq!(b.char_len(), Some(12));
    assert_eq!(eq_probe::scans().0, 0, "sharer reads the cached count");
}

// ── COW behavior through the string layer ─────────────────────
#[test]
fn clone_shares_heap_buffer_and_append_cow_breaks() {
    let a = PerlString::from_str(&"base".repeat(10)).unwrap(); // heap
    let mut b = a.clone();
    b.push_str("+more").unwrap();
    assert_eq!(a.len(), 40);
    assert_eq!(b.len(), 45);
    assert!(a.as_str(&mut [0u8; DECODE_MAX]).is_some());
}

impl PerlString {
    /// Test-only: force the utf8 flag on (simulating `Encode::_utf8_on` / upgrade provenance).
    pub(super) fn set_utf8_for_test(&mut self) {
        self.rebuild_tag(|_u, w, t| (true, w, t));
    }
}

// ── The non-allocating constructors ───────────────────────────────

#[test]
fn inline_accepts_up_to_the_capacity_and_refuses_beyond() {
    assert!(PerlString::inline("a".repeat(INLINE_MAX)).is_some());
    assert_eq!(PerlString::inline("a".repeat(INLINE_MAX + 1)), None);
    assert!(PerlString::inline_bytes(vec![0xFFu8; INLINE_MAX]).is_some());
    assert_eq!(PerlString::inline_bytes(vec![0xFFu8; INLINE_MAX + 1]), None);
}

#[test]
fn inline_agrees_with_the_fallible_constructors() {
    // Same content, same result: the fallible paths delegate here, so the representations must match exactly.
    for text in ["", "hello", "héllo", "0", "a longer ascii string"] {
        if let Some(inline) = PerlString::inline(text) {
            assert_eq!(inline, text.parse::<PerlString>().unwrap(), "{text:?}");
        }
    }

    for bytes in [&b""[..], b"hello", b"\xFF\xFE", b"\xC3\xA9"] {
        if let Some(inline) = PerlString::inline_bytes(bytes) {
            assert_eq!(inline, PerlString::from_bytes(bytes).unwrap(), "{bytes:?}");
        }
    }
}

#[test]
fn inline_flags_follow_the_source_type() {
    // From &str: ASCII unflagged (canonical downgraded form), non-ASCII flagged.
    assert!(!PerlString::inline("hello").unwrap().is_utf8());
    assert!(PerlString::inline("héllo").unwrap().is_utf8());

    // From bytes: never flagged, even when the content happens to be valid UTF-8.
    assert!(!PerlString::inline_bytes(b"h\xC3\xA9llo").unwrap().is_utf8());
}

#[test]
fn inline_composes_with_unwrap_or_default() {
    // The discard-the-detail path: callers who merely prefer inline storage need one combinator.
    assert_eq!(PerlString::inline("hi").unwrap_or_default().as_bytes(&mut [0u8; DECODE_MAX]), b"hi");
    assert_eq!(PerlString::inline("a".repeat(INLINE_MAX + 1)).unwrap_or_default(), PerlString::empty());
}

#[test]
fn inline_accepts_every_asref_shape() {
    let owned = String::from("owned");
    assert!(PerlString::inline(&owned).is_some());
    assert!(PerlString::inline(owned.clone()).is_some());
    assert!(PerlString::inline(owned.as_str()).is_some());

    let bytes = vec![1u8, 2, 3];
    assert!(PerlString::inline_bytes(&bytes).is_some());
    assert!(PerlString::inline_bytes(bytes.clone()).is_some());
    assert!(PerlString::inline_bytes(&bytes[..]).is_some());
}

// ── Formatting into the string ────────────────────────────────────

#[test]
fn write_macro_appends_through_fmt_write() {
    use std::fmt::Write;
    let mut s = PerlString::empty();
    write!(s, "{}-tail", 42).unwrap();
    write!(s, " {:.2}", 1.5).unwrap();
    assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), b"42-tail 1.50");
}

#[test]
fn push_fmt_reports_allocation_precisely() {
    // The trait impl flattens failure into fmt::Error, which carries nothing; push_fmt keeps the real error.
    let mut s = PerlString::empty();
    s.push_fmt(format_args!("{}", 12345)).unwrap();
    s.push_fmt(format_args!("{:>8}", "x")).unwrap();
    assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), b"12345       x");
}

#[test]
fn formatting_into_a_string_grows_it_across_tiers() {
    use std::fmt::Write;

    // Crossing the inline capacity mid-format must promote and keep every byte.
    let mut s = PerlString::empty();
    for i in 0..10 {
        write!(s, "{i:04}").unwrap();
    }

    assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), b"0000000100020003000400050006000700080009");
    assert_eq!(s.len(), 40);
}

// ── Interpreting the content (§2.2.2, §2.3.3, §2.3.4) ─────────────

#[test]
fn interpretation_methods_answer_from_the_string() {
    // The operations that used to reach for the bytes at the call site.  Asking the string means the caller neither
    // sees nor decides which storage form holds the content.
    let s: PerlString = "42abc".parse().unwrap();
    assert_eq!(s.to_int(), 42, "leading numeric prefix");
    assert!(s.to_bool());
    assert!(s.would_warn(), "a trailing non-numeric tail warns");

    let f: PerlString = "3.75".parse().unwrap();
    assert_eq!(f.to_float(), 3.75);
    assert_eq!(f.to_int(), 3, "truncating toward zero");
    assert!(!f.would_warn());

    // Perl truthiness: only "" and "0" are false, so "0.0" and "00" are true.
    for (text, truth) in [("", false), ("0", false), ("0.0", true), ("00", true), (" ", true), ("0E0", true)] {
        let v: PerlString = text.parse().unwrap();
        assert_eq!(v.to_bool(), truth, "truthiness of {text:?}");
    }
}

#[test]
fn interpretation_agrees_across_storage_forms() {
    // The same content held inline and on the heap must answer identically — the property that lets storage forms
    // multiply without consumers noticing.
    let short: PerlString = "17".parse().unwrap();
    let padded: PerlString = "17                                        ".parse().unwrap();
    assert_ne!(short.storage_type(), padded.storage_type(), "the two must actually differ in storage");
    assert_eq!(short.to_int(), 17);
    assert_eq!(padded.to_int(), 17, "trailing space does not change the numeric prefix");
    assert!(short.to_bool() && padded.to_bool());
}

#[test]
fn debug_shows_the_representation_with_readable_bytes() {
    let packed: PerlString = "2026-07-28T14:33:07Z".parse().unwrap();
    let shown = format!("{packed:?}");
    assert!(shown.contains("storage: Packed"), "the tier is the first thing a developer wants: {shown}");
    assert!(shown.contains(r#"bytes: b"2026-07-28T14:33:07Z""#), "byte-string syntax, not integers: {shown}");

    // Bytes that are not text render escaped rather than lossily, since a perl string's content need not be UTF-8.
    let raw = PerlString::from_bytes([0xFF, 0xFE, b'h', b'i']).unwrap();
    assert!(format!("{raw:?}").contains(r#"b"\xFF\xFEhi""#));

    // The usual escapes, so a newline does not break the line.
    let escaped: PerlString = "a\tb\nc".parse().unwrap();
    assert!(format!("{escaped:?}").contains(r#"b"a\tb\nc""#));
}

#[test]
fn the_constructors_accept_every_asref_shape() {
    // Generic at the boundary: an embedder holding a String, a Cow, or a compact string type from the ecosystem needs
    // no conversion, and the ladder beneath is monomorphic.
    let owned = String::from("owned content");
    assert_eq!(PerlString::new(&owned).unwrap().len(), 13);
    assert_eq!(PerlString::new(owned.clone()).unwrap().len(), 13);
    assert_eq!(PerlString::new(owned.as_str()).unwrap().len(), 13);
    assert_eq!(PerlString::new(std::borrow::Cow::Borrowed("borrowed")).unwrap().len(), 8);

    let bytes = vec![1u8, 2, 3];
    assert_eq!(PerlString::from_bytes(&bytes).unwrap().len(), 3);
    assert_eq!(PerlString::from_bytes(bytes.clone()).unwrap().len(), 3);
    assert_eq!(PerlString::from_bytes(&bytes[..]).unwrap().len(), 3);
    assert_eq!(PerlString::from_bytes([7u8; 4]).unwrap().len(), 4);

    // FromStr forwards to new, so parse() and new() agree exactly.
    assert_eq!(PerlString::new("2026-07-28T14:33:07Z").unwrap(), "2026-07-28T14:33:07Z".parse().unwrap());
}

#[test]
fn appending_yields_what_constructing_whole_would() {
    // The canonicity obligation for the incremental path: appending into the nibbles must land on the same
    // representation `pack` would have chosen for the finished content, or equal strings would differ by how they were
    // built.
    let cases: &[(&str, &str)] = &[
        ("2026-07-28T14:33", ":07"),            // stays DateTimePlus
        ("2026-07-28T14:33:07", "Z"),           // DateTimePlus transcodes into Zulu
        ("1234567890123456", "7890"),           // stays Numeric
        ("2026-07-28 202607", "28"),            // Numeric throughout
        ("192.168.100.200 1", ".2.3"),          // Numeric
        ("14:33+01:00 14:33", "+02"),           // '+' keeps it out of Zulu
        ("2026-07-29T17:23:45.1234567", "89Z"), // reaches the full family
    ];

    for (head, tail) in cases {
        let mut built: PerlString = head.parse().unwrap();
        assert!(built.storage_type().is_packed(), "{head} should start packed");
        built.push_str(tail).unwrap();

        let whole: PerlString = format!("{head}{tail}").parse().unwrap();
        assert_eq!(built.storage_type(), whole.storage_type(), "{head}+{tail}: same tier");
        assert_eq!(built.as_bytes(&mut [0u8; DECODE_MAX]), whole.as_bytes(&mut [0u8; DECODE_MAX]), "{head}+{tail}: same content");
        assert_eq!(built, whole, "{head}+{tail}: equal strings");
    }
}

#[test]
fn appending_leaves_the_tier_when_it_must() {
    // A character in no alphabet, and content past the capacity: both go to the heap, carrying their bytes intact.
    let mut lettered: PerlString = "2026-07-28T14:33".parse().unwrap();
    lettered.push_str("x").unwrap();
    assert!(lettered.storage_type().is_heap());
    assert_eq!(lettered.as_bytes(&mut [0u8; DECODE_MAX]), b"2026-07-28T14:33x");

    let mut overflowing: PerlString = "123456789012345678901234567890".parse().unwrap();
    assert!(overflowing.storage_type().is_packed(), "thirty characters is the capacity");
    overflowing.push_str("1").unwrap();
    assert!(overflowing.storage_type().is_heap());
    assert_eq!(overflowing.len(), 31);

    // A '+' offset meeting a 'Z': the two spellings are mutually exclusive, so this leaves the tier too.
    let mut offset: PerlString = "14:33+01:00 14:33".parse().unwrap();
    offset.push_str("Z").unwrap();
    assert!(offset.storage_type().is_heap());
}

#[test]
fn incremental_building_reaches_the_packed_tier() {
    // The case the length families exist for: a string that passes through a trailing space on its way to something
    // longer, built one piece at a time through fmt::Write.
    use std::fmt::Write;
    let mut s = PerlString::empty();
    write!(s, "2026-07-28").unwrap();
    write!(s, " ").unwrap();
    assert_eq!(s.len(), 11, "a trailing space mid-build");
    write!(s, "14:33:07").unwrap();
    assert!(s.storage_type().is_packed());
    assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), b"2026-07-28 14:33:07");
    assert_eq!(s, "2026-07-28 14:33:07".parse().unwrap());
}

#[test]
fn the_terminator_is_found_at_every_position() {
    // inline_len reads two words rather than scanning bytes, so every boundary deserves checking — especially 7/8,
    // where the first word ends, and 15, where a full payload has no terminator at all.
    for len in 0..=INLINE_MAX {
        let content: Vec<u8> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
        let s = PerlString::from_bytes(&content).unwrap();
        assert_eq!(s.len(), len, "length of {len} bytes of content");
        assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), &content[..], "content of {len} bytes");
    }

    // High bytes must not be mistaken for terminators: 0x80 and 0xFF are where the naive bit trick goes wrong.
    for filler in [0x80u8, 0xFF, 0x01, 0x7F] {
        for len in 1..=INLINE_MAX {
            let content = vec![filler; len];
            let s = PerlString::from_bytes(&content).unwrap();
            assert_eq!(s.len(), len, "{len} bytes of {filler:#04x}");
        }
    }
}

#[test]
fn the_terminator_is_found_at_every_length() {
    // inline_len reads two words rather than scanning bytes, so every boundary within and across the two — and the full
    // payload, which has no terminator at all — needs pinning.
    for len in 0..=INLINE_MAX {
        let content = vec![b'x'; len];
        let s = PerlString::from_bytes(&content).unwrap();
        assert_eq!(s.len(), len, "length {len}");
        assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), &content[..], "content at length {len}");
    }

    // A byte with the high bit set must not be mistaken for the terminator: the trick discards borrows that came from
    // 0x80-or-above bytes, which is the half of it that is easy to get wrong.
    for len in 1..=INLINE_MAX {
        let mut content = vec![0xFFu8; len];
        content[len - 1] = 0x80;
        let s = PerlString::from_bytes(&content).unwrap();
        assert_eq!(s.len(), len, "high-bit content at length {len}");
    }
}

#[test]
fn nul_bearing_content_lives_inline_now() {
    // An explicit length admits what a terminator could not: a NUL is content like any other byte, and needs no special
    // case in construction, in appending, or in the tier ladder.
    for content in [&b"\0"[..], b"a\0b", b"\0\0\0", b"ab\0", b"\0abcdefghijklm", b"abcdefghijklmn\0"] {
        let s = PerlString::from_bytes(content).unwrap();
        assert!(s.storage_type().is_inline(), "{content:?} should be inline");
        assert_eq!(s.len(), content.len());
        assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), content);
    }

    // And appending one keeps the string inline.
    let mut s = PerlString::from_bytes(b"ab").unwrap();
    s.push_bytes(b"\0cd").unwrap();
    assert!(s.storage_type().is_inline());
    assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), b"ab\0cd");
}

#[test]
fn the_length_families_split_at_capacity() {
    // Content of exactly fifteen bytes fills the payload and implies its length; anything shorter stores it in the byte'
    // a fifteenth character would have used.
    for len in 0..=INLINE_MAX {
        let content = vec![b'x'; len];
        let s = PerlString::from_bytes(&content).unwrap();
        assert_eq!(s.len(), len, "length {len}");
        assert!(s.storage_type().is_inline());
        assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), &content[..]);
    }

    // Growing across the boundary by appending, one byte at a time.
    let mut s = PerlString::empty();
    for i in 0..INLINE_MAX {
        s.push_bytes(b"y").unwrap();
        assert_eq!(s.len(), i + 1, "after {} appends", i + 1);
        assert!(s.storage_type().is_inline());
    }

    // One more leaves the tier: sixteen characters is where the packed band begins.
    s.push_bytes(b"y").unwrap();
    assert_eq!(s.len(), 16);
    assert!(!s.storage_type().is_inline());
}

#[test]
fn equal_content_has_equal_bytes_whatever_its_history() {
    // Padding past the length is canonically zero, so a string built by appending is byte-identical to the same content
    // constructed whole — which is what lets representation stand in for content.
    let whole = PerlString::from_bytes(b"abcde").unwrap();
    let mut built = PerlString::from_bytes(b"abc").unwrap();
    built.push_bytes(b"de").unwrap();
    assert_eq!(whole, built);

    // The same content reached through the full-capacity family and back down.
    let mut long = PerlString::from_bytes(b"abc").unwrap();
    long.push_bytes(b"de").unwrap();
    assert_eq!(whole, long);
}

#[test]
fn compressed_payloads_and_the_nibble_scheme() {
    // The Latin-1 class stores the Latin-1 transcoding of the internal bytes — each one- or two-byte UTF-8 sequence as
    // its single-byte equivalent — with the length byte split into the two nibbles (§2.2.9): low `s`, high `h`.  The E9
    // monster's two strings at the representation level.
    let two_char = PerlString::from_bytes([0xC3, 0xA9]).unwrap(); // The octet string C3.A9.
    assert_eq!(two_char.storage_type(), StorageType::InlineLatin1);
    match two_char.raw_parts() {
        RawParts::Inline { buf, .. } => {
            assert_eq!(buf[0], 0xE9, "the payload is the Latin-1 equivalent, not the encoding");
            assert_eq!(buf[LENGTH_BYTE], 0x11, "one stored, one high: nibbles 1/1");
        }
        _ => panic!("expected inline storage"),
    }
    assert_eq!(two_char.len(), 2, "the internal length is s + h");
    assert_eq!(two_char.char_len(), Some(1));
    assert_eq!(two_char.as_bytes(&mut [0u8; DECODE_MAX]), [0xC3, 0xA9], "as_bytes expands the compression");

    let one_char = PerlString::from_bytes([0xE9]).unwrap(); // The one-octet string é: the Bytes residual.
    assert_eq!(one_char.storage_type(), StorageType::InlineBytes);
    assert_eq!(one_char.len(), 1);
    assert_ne!(two_char, one_char, "different strings, distinguished by the class axis alone");

    // Sixteen to thirty compressible bytes are the new inline intake: fifteen stored high bytes span thirty internal
    // bytes, report length 30 (container-verified: ord returns the lead C3), and fill the payload — the full family.
    let wide = PerlString::from_bytes([0xC3, 0xA9].repeat(15)).unwrap();
    assert_eq!(wide.storage_type(), StorageType::InlineLatin1Full);
    assert_eq!(wide.len(), 30, "length is the expansion sum, never the payload count");
    assert_eq!(wide.char_len(), Some(15));
    assert_eq!(wide.as_bytes(&mut [0u8; DECODE_MAX]), [0xC3, 0xA9].repeat(15));

    // The verbatim valid classes carry their character count in the aux nibble: O(1), no decode.
    let euro = PerlString::from_str("€€").unwrap(); // Six bytes, two characters, beyond Latin-1.
    assert_eq!(euro.storage_type(), StorageType::InlineNonLatin1);
    match euro.raw_parts() {
        RawParts::Inline { buf, .. } => assert_eq!(buf[LENGTH_BYTE], 0x26, "six stored, two characters: nibbles 2/6"),
        _ => panic!("expected inline storage"),
    }
    assert_eq!(euro.char_len(), Some(2));

    // Ascii and Bytes keep a zero aux nibble, making their short-family payloads bit-identical to the raw-byte tier.
    let plain = PerlString::from_bytes(b"abcd").unwrap();
    match plain.raw_parts() {
        RawParts::Inline { buf, .. } => assert_eq!(buf[LENGTH_BYTE], 0x04),
        _ => panic!("expected inline storage"),
    }

    // Overlong NUL never compresses; canonical NUL does — in every spelling (§2.2.9).
    assert_eq!(PerlString::from_bytes([0xC0, 0x80]).unwrap().storage_type(), StorageType::InlineBytes);
    assert_eq!(PerlString::from_bytes(b"a\0b").unwrap().storage_type(), StorageType::InlineAscii);

    // Deterministic ladder: 16-30-byte ASCII goes packed where an alphabet fits and heap otherwise — never compressed,
    // sixteen characters not fitting fifteen payload bytes.
    assert_eq!(PerlString::from_bytes(b"1234567890123456").unwrap().storage_type(), StorageType::PackedNumeric);
    assert_eq!(PerlString::from_bytes(b"abcdefghabcdefgh").unwrap().storage_type(), StorageType::Heap);
}

#[test]
fn rebuilding_zeroes_everything_past_the_content() {
    // The canonical-padding obligation, checked at the representation rather than through content: a payload carrying
    // stale bytes past its length must come back with them cleared, or two equal strings could differ in their bytes
    // and representation would stop standing in for content.
    let mut dirty = [0xEEu8; INLINE_MAX];
    dirty[..4].copy_from_slice(b"abcd");
    let s = PerlString::build_inline(InlineClass::Ascii, false, false, false, 4, 0, dirty);

    match s.raw_parts() {
        RawParts::Inline { full, buf, .. } => {
            assert!(!full, "four bytes is the stored-length family");
            assert_eq!(&buf[..4], b"abcd");
            assert!(buf[4..LENGTH_BYTE].iter().all(|&b| b == 0), "padding must be cleared, got {:?}", &buf[4..LENGTH_BYTE]);
            assert_eq!(buf[LENGTH_BYTE], 4, "the length byte: aux nibble zero, stored nibble four");
        }
        _ => panic!("expected inline storage"),
    }

    assert_eq!(s, PerlString::from_bytes(b"abcd").unwrap());
}

#[test]
fn packed_equality_compares_nibbles_directly() {
    // Equal content in one alphabet has equal nibbles, so no decoding is needed — the encoding is injective and the
    // padding canonical.  These pin the answers rather than the mechanism, but a wrong fast path would break them.
    let a: PerlString = "2026-07-28T14:33:07Z".parse().unwrap();
    let b: PerlString = "2026-07-28T14:33:07Z".parse().unwrap();
    assert_eq!(a, b);
    assert!(a.storage_type().is_packed());

    // Differing in the last character, and in the first.
    assert_ne!(a, "2026-07-28T14:33:08Z".parse().unwrap());
    assert_ne!(a, "3026-07-28T14:33:07Z".parse().unwrap());

    // Different lengths within the same alphabet, including the two length families.
    assert_ne!(a, "2026-07-28T14:33:07.5Z".parse::<PerlString>().unwrap());
    let full: PerlString = "2026-07-29T17:23:45.123456789Z".parse().unwrap();
    assert_eq!(full, "2026-07-29T17:23:45.123456789Z".parse().unwrap());
    assert_ne!(full, "2026-07-29T17:23:45.12345678Z".parse().unwrap());

    // Different alphabets cannot hold equal content, so the mismatch is decisive.
    let numeric: PerlString = "192.168.100.200 1.2".parse().unwrap();
    assert_ne!(a, numeric);

    // Packed against the other tiers, both directions.
    let heaped: PerlString = "2026-07-28T14:33:07Z and then some more".parse().unwrap();
    assert_ne!(a, heaped);
    assert_ne!(heaped, a);

    let short: PerlString = "2026-07-28".parse().unwrap();
    assert_ne!(a, short);
    assert_ne!(short, a);

    // A packed string equals the same content held on the heap, which is the case the one-sided path serves.
    let long_numeric: PerlString = "1234567890123456789012345".parse().unwrap();
    assert!(long_numeric.storage_type().is_packed());

    let same_on_heap = {
        let mut s: PerlString = "1234567890123456789012345 tail".parse().unwrap();
        assert!(s.storage_type().is_heap());
        s = PerlString::from_bytes(&s.as_bytes(&mut [0u8; DECODE_MAX])[..25]).unwrap();
        s
    };
    assert_eq!(long_numeric, same_on_heap, "same content, different tiers");
}

// ── Ordering (§2.3.5), every case container-verified ──────────────

/// Rebuild a string from the internal bytes and flag the probe recorded, forcing the flag rather than letting the tier
/// ladder choose it — flagged ASCII is representable but no constructor produces it, ASCII being canonically unflagged.
fn from_hex(hex: &str, flagged: bool) -> PerlString {
    let bytes: Vec<u8> = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap()).collect();

    if bytes.len() <= INLINE_MAX {
        let (class, stored, aux, buf) = classify_inline(&bytes).expect("fifteen bytes always classify");
        return PerlString::build_inline(class, flagged, false, false, stored, aux, buf);
    }

    if (MIN_PACKED_LEN..=MAX_PACKED_LEN).contains(&bytes.len())
        && let Some(p) = pack(&bytes)
    {
        return PerlString::build_packed(p, flagged, false, false);
    }

    let cb = CowBuffer::from_slice(&bytes).unwrap();
    PerlString::build_heap(flagged, false, false, cb)
}

#[test]
fn ordering_matches_perl_for_every_flag_combination() {
    // Pairs from `cmp` in container perl 5.44 across every storage pairing — inline, packed, and heap operands in every
    // content class, both length families, the four flag combinations, ASCII and high octets, multi-byte sequences,
    // embedded NULs, empty strings — including pairs whose bytes are identical but whose flags make them mean different
    // things.  39 operands, all 1521 ordered pairs — two of them the compressed tier's 16-30-byte intake, under both
    // flags, regenerated whole whenever the operand list changes.
    let cases: &[(&str, bool, &str, bool, i32)] = &[
        ("616263", false, "616263", false, 0),
        ("616263", false, "616264", false, -1),
        ("616263", false, "6162", false, 1),
        ("616263", false, "", false, 1),
        ("616263", false, "e9", false, -1),
        ("616263", false, "ff", false, -1),
        ("616263", false, "c3a9", false, -1),
        ("616263", false, "00", false, 1),
        ("616263", false, "610062", false, 1),
        ("616263", false, "616263", true, 0),
        ("616263", false, "616264", true, -1),
        ("616263", false, "6162", true, 1),
        ("616263", false, "", true, 1),
        ("616263", false, "c3a9", true, -1),
        ("616263", false, "c3bf", true, -1),
        ("616263", false, "c480", true, -1),
        ("616263", false, "e282ac", true, -1),
        ("616263", false, "61c3a9", true, -1),
        ("616263", false, "c3a961", true, -1),
        ("616263", false, "00", true, 1),
        ("616263", false, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("616263", false, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("616263", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("616263", false, "31323334353637383930313233343536373839", false, 1),
        ("616263", false, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("616263", false, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("616263", false, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("616263", false, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("616263", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("616263", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("616263", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("616263", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("616263", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("616263", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("616263", false, "616161616161616161616161616161", false, 1),
        ("616263", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("616263", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("616263", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("616263", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("616264", false, "616263", false, 1),
        ("616264", false, "616264", false, 0),
        ("616264", false, "6162", false, 1),
        ("616264", false, "", false, 1),
        ("616264", false, "e9", false, -1),
        ("616264", false, "ff", false, -1),
        ("616264", false, "c3a9", false, -1),
        ("616264", false, "00", false, 1),
        ("616264", false, "610062", false, 1),
        ("616264", false, "616263", true, 1),
        ("616264", false, "616264", true, 0),
        ("616264", false, "6162", true, 1),
        ("616264", false, "", true, 1),
        ("616264", false, "c3a9", true, -1),
        ("616264", false, "c3bf", true, -1),
        ("616264", false, "c480", true, -1),
        ("616264", false, "e282ac", true, -1),
        ("616264", false, "61c3a9", true, -1),
        ("616264", false, "c3a961", true, -1),
        ("616264", false, "00", true, 1),
        ("616264", false, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("616264", false, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("616264", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("616264", false, "31323334353637383930313233343536373839", false, 1),
        ("616264", false, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("616264", false, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("616264", false, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("616264", false, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("616264", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("616264", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("616264", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("616264", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("616264", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("616264", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("616264", false, "616161616161616161616161616161", false, 1),
        ("616264", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("616264", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("616264", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("616264", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("6162", false, "616263", false, -1),
        ("6162", false, "616264", false, -1),
        ("6162", false, "6162", false, 0),
        ("6162", false, "", false, 1),
        ("6162", false, "e9", false, -1),
        ("6162", false, "ff", false, -1),
        ("6162", false, "c3a9", false, -1),
        ("6162", false, "00", false, 1),
        ("6162", false, "610062", false, 1),
        ("6162", false, "616263", true, -1),
        ("6162", false, "616264", true, -1),
        ("6162", false, "6162", true, 0),
        ("6162", false, "", true, 1),
        ("6162", false, "c3a9", true, -1),
        ("6162", false, "c3bf", true, -1),
        ("6162", false, "c480", true, -1),
        ("6162", false, "e282ac", true, -1),
        ("6162", false, "61c3a9", true, -1),
        ("6162", false, "c3a961", true, -1),
        ("6162", false, "00", true, 1),
        ("6162", false, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("6162", false, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("6162", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("6162", false, "31323334353637383930313233343536373839", false, 1),
        ("6162", false, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("6162", false, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("6162", false, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("6162", false, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("6162", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("6162", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("6162", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("6162", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("6162", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("6162", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("6162", false, "616161616161616161616161616161", false, 1),
        ("6162", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("6162", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("6162", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("6162", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("", false, "616263", false, -1),
        ("", false, "616264", false, -1),
        ("", false, "6162", false, -1),
        ("", false, "", false, 0),
        ("", false, "e9", false, -1),
        ("", false, "ff", false, -1),
        ("", false, "c3a9", false, -1),
        ("", false, "00", false, -1),
        ("", false, "610062", false, -1),
        ("", false, "616263", true, -1),
        ("", false, "616264", true, -1),
        ("", false, "6162", true, -1),
        ("", false, "", true, 0),
        ("", false, "c3a9", true, -1),
        ("", false, "c3bf", true, -1),
        ("", false, "c480", true, -1),
        ("", false, "e282ac", true, -1),
        ("", false, "61c3a9", true, -1),
        ("", false, "c3a961", true, -1),
        ("", false, "00", true, -1),
        ("", false, "323032362d30372d32385431343a33333a30375a", false, -1),
        ("", false, "323032362d30372d32385431343a33333a30385a", false, -1),
        ("", false, "3139322e3136382e3130302e32303020312e32", false, -1),
        ("", false, "31323334353637383930313233343536373839", false, -1),
        ("", false, "323032362d30372d32385431343a33333a30375a", true, -1),
        ("", false, "61616161616161616161616161616161616161616161616161616161616161", false, -1),
        ("", false, "61616161616161616161616161616161616161616161616161616161616161", true, -1),
        ("", false, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("", false, "31313131313131313131313131313131313131313131313131313131313131", false, -1),
        ("", false, "616161616161616161616161616161", false, -1),
        ("", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("", false, "313131313131313131313131313131313131313131313131313131313131", false, -1),
        ("", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("e9", false, "616263", false, 1),
        ("e9", false, "616264", false, 1),
        ("e9", false, "6162", false, 1),
        ("e9", false, "", false, 1),
        ("e9", false, "e9", false, 0),
        ("e9", false, "ff", false, -1),
        ("e9", false, "c3a9", false, 1),
        ("e9", false, "00", false, 1),
        ("e9", false, "610062", false, 1),
        ("e9", false, "616263", true, 1),
        ("e9", false, "616264", true, 1),
        ("e9", false, "6162", true, 1),
        ("e9", false, "", true, 1),
        ("e9", false, "c3a9", true, 0),
        ("e9", false, "c3bf", true, -1),
        ("e9", false, "c480", true, -1),
        ("e9", false, "e282ac", true, -1),
        ("e9", false, "61c3a9", true, 1),
        ("e9", false, "c3a961", true, -1),
        ("e9", false, "00", true, 1),
        ("e9", false, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("e9", false, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("e9", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("e9", false, "31323334353637383930313233343536373839", false, 1),
        ("e9", false, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("e9", false, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("e9", false, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("e9", false, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("e9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("e9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("e9", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("e9", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("e9", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("e9", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("e9", false, "616161616161616161616161616161", false, 1),
        ("e9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, 1),
        ("e9", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("e9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("e9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("ff", false, "616263", false, 1),
        ("ff", false, "616264", false, 1),
        ("ff", false, "6162", false, 1),
        ("ff", false, "", false, 1),
        ("ff", false, "e9", false, 1),
        ("ff", false, "ff", false, 0),
        ("ff", false, "c3a9", false, 1),
        ("ff", false, "00", false, 1),
        ("ff", false, "610062", false, 1),
        ("ff", false, "616263", true, 1),
        ("ff", false, "616264", true, 1),
        ("ff", false, "6162", true, 1),
        ("ff", false, "", true, 1),
        ("ff", false, "c3a9", true, 1),
        ("ff", false, "c3bf", true, 0),
        ("ff", false, "c480", true, -1),
        ("ff", false, "e282ac", true, -1),
        ("ff", false, "61c3a9", true, 1),
        ("ff", false, "c3a961", true, 1),
        ("ff", false, "00", true, 1),
        ("ff", false, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("ff", false, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("ff", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("ff", false, "31323334353637383930313233343536373839", false, 1),
        ("ff", false, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("ff", false, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("ff", false, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("ff", false, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("ff", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("ff", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 1),
        ("ff", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, 1),
        ("ff", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("ff", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("ff", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("ff", false, "616161616161616161616161616161", false, 1),
        ("ff", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, 1),
        ("ff", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("ff", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("ff", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 1),
        ("c3a9", false, "616263", false, 1),
        ("c3a9", false, "616264", false, 1),
        ("c3a9", false, "6162", false, 1),
        ("c3a9", false, "", false, 1),
        ("c3a9", false, "e9", false, -1),
        ("c3a9", false, "ff", false, -1),
        ("c3a9", false, "c3a9", false, 0),
        ("c3a9", false, "00", false, 1),
        ("c3a9", false, "610062", false, 1),
        ("c3a9", false, "616263", true, 1),
        ("c3a9", false, "616264", true, 1),
        ("c3a9", false, "6162", true, 1),
        ("c3a9", false, "", true, 1),
        ("c3a9", false, "c3a9", true, -1),
        ("c3a9", false, "c3bf", true, -1),
        ("c3a9", false, "c480", true, -1),
        ("c3a9", false, "e282ac", true, -1),
        ("c3a9", false, "61c3a9", true, 1),
        ("c3a9", false, "c3a961", true, -1),
        ("c3a9", false, "00", true, 1),
        ("c3a9", false, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("c3a9", false, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("c3a9", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("c3a9", false, "31323334353637383930313233343536373839", false, 1),
        ("c3a9", false, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("c3a9", false, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("c3a9", false, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("c3a9", false, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("c3a9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("c3a9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("c3a9", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("c3a9", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("c3a9", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("c3a9", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a9", false, "616161616161616161616161616161", false, 1),
        ("c3a9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("c3a9", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("c3a9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("00", false, "616263", false, -1),
        ("00", false, "616264", false, -1),
        ("00", false, "6162", false, -1),
        ("00", false, "", false, 1),
        ("00", false, "e9", false, -1),
        ("00", false, "ff", false, -1),
        ("00", false, "c3a9", false, -1),
        ("00", false, "00", false, 0),
        ("00", false, "610062", false, -1),
        ("00", false, "616263", true, -1),
        ("00", false, "616264", true, -1),
        ("00", false, "6162", true, -1),
        ("00", false, "", true, 1),
        ("00", false, "c3a9", true, -1),
        ("00", false, "c3bf", true, -1),
        ("00", false, "c480", true, -1),
        ("00", false, "e282ac", true, -1),
        ("00", false, "61c3a9", true, -1),
        ("00", false, "c3a961", true, -1),
        ("00", false, "00", true, 0),
        ("00", false, "323032362d30372d32385431343a33333a30375a", false, -1),
        ("00", false, "323032362d30372d32385431343a33333a30385a", false, -1),
        ("00", false, "3139322e3136382e3130302e32303020312e32", false, -1),
        ("00", false, "31323334353637383930313233343536373839", false, -1),
        ("00", false, "323032362d30372d32385431343a33333a30375a", true, -1),
        ("00", false, "61616161616161616161616161616161616161616161616161616161616161", false, -1),
        ("00", false, "61616161616161616161616161616161616161616161616161616161616161", true, -1),
        ("00", false, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("00", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("00", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("00", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("00", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("00", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("00", false, "31313131313131313131313131313131313131313131313131313131313131", false, -1),
        ("00", false, "616161616161616161616161616161", false, -1),
        ("00", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("00", false, "313131313131313131313131313131313131313131313131313131313131", false, -1),
        ("00", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("00", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("610062", false, "616263", false, -1),
        ("610062", false, "616264", false, -1),
        ("610062", false, "6162", false, -1),
        ("610062", false, "", false, 1),
        ("610062", false, "e9", false, -1),
        ("610062", false, "ff", false, -1),
        ("610062", false, "c3a9", false, -1),
        ("610062", false, "00", false, 1),
        ("610062", false, "610062", false, 0),
        ("610062", false, "616263", true, -1),
        ("610062", false, "616264", true, -1),
        ("610062", false, "6162", true, -1),
        ("610062", false, "", true, 1),
        ("610062", false, "c3a9", true, -1),
        ("610062", false, "c3bf", true, -1),
        ("610062", false, "c480", true, -1),
        ("610062", false, "e282ac", true, -1),
        ("610062", false, "61c3a9", true, -1),
        ("610062", false, "c3a961", true, -1),
        ("610062", false, "00", true, 1),
        ("610062", false, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("610062", false, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("610062", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("610062", false, "31323334353637383930313233343536373839", false, 1),
        ("610062", false, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("610062", false, "61616161616161616161616161616161616161616161616161616161616161", false, -1),
        ("610062", false, "61616161616161616161616161616161616161616161616161616161616161", true, -1),
        ("610062", false, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("610062", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("610062", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("610062", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("610062", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("610062", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("610062", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("610062", false, "616161616161616161616161616161", false, -1),
        ("610062", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("610062", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("610062", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("610062", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("616263", true, "616263", false, 0),
        ("616263", true, "616264", false, -1),
        ("616263", true, "6162", false, 1),
        ("616263", true, "", false, 1),
        ("616263", true, "e9", false, -1),
        ("616263", true, "ff", false, -1),
        ("616263", true, "c3a9", false, -1),
        ("616263", true, "00", false, 1),
        ("616263", true, "610062", false, 1),
        ("616263", true, "616263", true, 0),
        ("616263", true, "616264", true, -1),
        ("616263", true, "6162", true, 1),
        ("616263", true, "", true, 1),
        ("616263", true, "c3a9", true, -1),
        ("616263", true, "c3bf", true, -1),
        ("616263", true, "c480", true, -1),
        ("616263", true, "e282ac", true, -1),
        ("616263", true, "61c3a9", true, -1),
        ("616263", true, "c3a961", true, -1),
        ("616263", true, "00", true, 1),
        ("616263", true, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("616263", true, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("616263", true, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("616263", true, "31323334353637383930313233343536373839", false, 1),
        ("616263", true, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("616263", true, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("616263", true, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("616263", true, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("616263", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("616263", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("616263", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("616263", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("616263", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("616263", true, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("616263", true, "616161616161616161616161616161", false, 1),
        ("616263", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("616263", true, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("616263", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("616263", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("616264", true, "616263", false, 1),
        ("616264", true, "616264", false, 0),
        ("616264", true, "6162", false, 1),
        ("616264", true, "", false, 1),
        ("616264", true, "e9", false, -1),
        ("616264", true, "ff", false, -1),
        ("616264", true, "c3a9", false, -1),
        ("616264", true, "00", false, 1),
        ("616264", true, "610062", false, 1),
        ("616264", true, "616263", true, 1),
        ("616264", true, "616264", true, 0),
        ("616264", true, "6162", true, 1),
        ("616264", true, "", true, 1),
        ("616264", true, "c3a9", true, -1),
        ("616264", true, "c3bf", true, -1),
        ("616264", true, "c480", true, -1),
        ("616264", true, "e282ac", true, -1),
        ("616264", true, "61c3a9", true, -1),
        ("616264", true, "c3a961", true, -1),
        ("616264", true, "00", true, 1),
        ("616264", true, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("616264", true, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("616264", true, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("616264", true, "31323334353637383930313233343536373839", false, 1),
        ("616264", true, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("616264", true, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("616264", true, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("616264", true, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("616264", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("616264", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("616264", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("616264", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("616264", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("616264", true, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("616264", true, "616161616161616161616161616161", false, 1),
        ("616264", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("616264", true, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("616264", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("616264", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("6162", true, "616263", false, -1),
        ("6162", true, "616264", false, -1),
        ("6162", true, "6162", false, 0),
        ("6162", true, "", false, 1),
        ("6162", true, "e9", false, -1),
        ("6162", true, "ff", false, -1),
        ("6162", true, "c3a9", false, -1),
        ("6162", true, "00", false, 1),
        ("6162", true, "610062", false, 1),
        ("6162", true, "616263", true, -1),
        ("6162", true, "616264", true, -1),
        ("6162", true, "6162", true, 0),
        ("6162", true, "", true, 1),
        ("6162", true, "c3a9", true, -1),
        ("6162", true, "c3bf", true, -1),
        ("6162", true, "c480", true, -1),
        ("6162", true, "e282ac", true, -1),
        ("6162", true, "61c3a9", true, -1),
        ("6162", true, "c3a961", true, -1),
        ("6162", true, "00", true, 1),
        ("6162", true, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("6162", true, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("6162", true, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("6162", true, "31323334353637383930313233343536373839", false, 1),
        ("6162", true, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("6162", true, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("6162", true, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("6162", true, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("6162", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("6162", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("6162", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("6162", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("6162", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("6162", true, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("6162", true, "616161616161616161616161616161", false, 1),
        ("6162", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("6162", true, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("6162", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("6162", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("", true, "616263", false, -1),
        ("", true, "616264", false, -1),
        ("", true, "6162", false, -1),
        ("", true, "", false, 0),
        ("", true, "e9", false, -1),
        ("", true, "ff", false, -1),
        ("", true, "c3a9", false, -1),
        ("", true, "00", false, -1),
        ("", true, "610062", false, -1),
        ("", true, "616263", true, -1),
        ("", true, "616264", true, -1),
        ("", true, "6162", true, -1),
        ("", true, "", true, 0),
        ("", true, "c3a9", true, -1),
        ("", true, "c3bf", true, -1),
        ("", true, "c480", true, -1),
        ("", true, "e282ac", true, -1),
        ("", true, "61c3a9", true, -1),
        ("", true, "c3a961", true, -1),
        ("", true, "00", true, -1),
        ("", true, "323032362d30372d32385431343a33333a30375a", false, -1),
        ("", true, "323032362d30372d32385431343a33333a30385a", false, -1),
        ("", true, "3139322e3136382e3130302e32303020312e32", false, -1),
        ("", true, "31323334353637383930313233343536373839", false, -1),
        ("", true, "323032362d30372d32385431343a33333a30375a", true, -1),
        ("", true, "61616161616161616161616161616161616161616161616161616161616161", false, -1),
        ("", true, "61616161616161616161616161616161616161616161616161616161616161", true, -1),
        ("", true, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("", true, "31313131313131313131313131313131313131313131313131313131313131", false, -1),
        ("", true, "616161616161616161616161616161", false, -1),
        ("", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("", true, "313131313131313131313131313131313131313131313131313131313131", false, -1),
        ("", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("c3a9", true, "616263", false, 1),
        ("c3a9", true, "616264", false, 1),
        ("c3a9", true, "6162", false, 1),
        ("c3a9", true, "", false, 1),
        ("c3a9", true, "e9", false, 0),
        ("c3a9", true, "ff", false, -1),
        ("c3a9", true, "c3a9", false, 1),
        ("c3a9", true, "00", false, 1),
        ("c3a9", true, "610062", false, 1),
        ("c3a9", true, "616263", true, 1),
        ("c3a9", true, "616264", true, 1),
        ("c3a9", true, "6162", true, 1),
        ("c3a9", true, "", true, 1),
        ("c3a9", true, "c3a9", true, 0),
        ("c3a9", true, "c3bf", true, -1),
        ("c3a9", true, "c480", true, -1),
        ("c3a9", true, "e282ac", true, -1),
        ("c3a9", true, "61c3a9", true, 1),
        ("c3a9", true, "c3a961", true, -1),
        ("c3a9", true, "00", true, 1),
        ("c3a9", true, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("c3a9", true, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("c3a9", true, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("c3a9", true, "31323334353637383930313233343536373839", false, 1),
        ("c3a9", true, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("c3a9", true, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("c3a9", true, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("c3a9", true, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("c3a9", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("c3a9", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("c3a9", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("c3a9", true, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a9", true, "616161616161616161616161616161", false, 1),
        ("c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, 1),
        ("c3a9", true, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("c3bf", true, "616263", false, 1),
        ("c3bf", true, "616264", false, 1),
        ("c3bf", true, "6162", false, 1),
        ("c3bf", true, "", false, 1),
        ("c3bf", true, "e9", false, 1),
        ("c3bf", true, "ff", false, 0),
        ("c3bf", true, "c3a9", false, 1),
        ("c3bf", true, "00", false, 1),
        ("c3bf", true, "610062", false, 1),
        ("c3bf", true, "616263", true, 1),
        ("c3bf", true, "616264", true, 1),
        ("c3bf", true, "6162", true, 1),
        ("c3bf", true, "", true, 1),
        ("c3bf", true, "c3a9", true, 1),
        ("c3bf", true, "c3bf", true, 0),
        ("c3bf", true, "c480", true, -1),
        ("c3bf", true, "e282ac", true, -1),
        ("c3bf", true, "61c3a9", true, 1),
        ("c3bf", true, "c3a961", true, 1),
        ("c3bf", true, "00", true, 1),
        ("c3bf", true, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("c3bf", true, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("c3bf", true, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("c3bf", true, "31323334353637383930313233343536373839", false, 1),
        ("c3bf", true, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("c3bf", true, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("c3bf", true, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("c3bf", true, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("c3bf", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("c3bf", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 1),
        ("c3bf", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, 1),
        ("c3bf", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("c3bf", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("c3bf", true, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3bf", true, "616161616161616161616161616161", false, 1),
        ("c3bf", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, 1),
        ("c3bf", true, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3bf", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("c3bf", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 1),
        ("c480", true, "616263", false, 1),
        ("c480", true, "616264", false, 1),
        ("c480", true, "6162", false, 1),
        ("c480", true, "", false, 1),
        ("c480", true, "e9", false, 1),
        ("c480", true, "ff", false, 1),
        ("c480", true, "c3a9", false, 1),
        ("c480", true, "00", false, 1),
        ("c480", true, "610062", false, 1),
        ("c480", true, "616263", true, 1),
        ("c480", true, "616264", true, 1),
        ("c480", true, "6162", true, 1),
        ("c480", true, "", true, 1),
        ("c480", true, "c3a9", true, 1),
        ("c480", true, "c3bf", true, 1),
        ("c480", true, "c480", true, 0),
        ("c480", true, "e282ac", true, -1),
        ("c480", true, "61c3a9", true, 1),
        ("c480", true, "c3a961", true, 1),
        ("c480", true, "00", true, 1),
        ("c480", true, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("c480", true, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("c480", true, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("c480", true, "31323334353637383930313233343536373839", false, 1),
        ("c480", true, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("c480", true, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("c480", true, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("c480", true, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("c480", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("c480", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 1),
        ("c480", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, 1),
        ("c480", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("c480", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("c480", true, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c480", true, "616161616161616161616161616161", false, 1),
        ("c480", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, 1),
        ("c480", true, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c480", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("c480", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 1),
        ("e282ac", true, "616263", false, 1),
        ("e282ac", true, "616264", false, 1),
        ("e282ac", true, "6162", false, 1),
        ("e282ac", true, "", false, 1),
        ("e282ac", true, "e9", false, 1),
        ("e282ac", true, "ff", false, 1),
        ("e282ac", true, "c3a9", false, 1),
        ("e282ac", true, "00", false, 1),
        ("e282ac", true, "610062", false, 1),
        ("e282ac", true, "616263", true, 1),
        ("e282ac", true, "616264", true, 1),
        ("e282ac", true, "6162", true, 1),
        ("e282ac", true, "", true, 1),
        ("e282ac", true, "c3a9", true, 1),
        ("e282ac", true, "c3bf", true, 1),
        ("e282ac", true, "c480", true, 1),
        ("e282ac", true, "e282ac", true, 0),
        ("e282ac", true, "61c3a9", true, 1),
        ("e282ac", true, "c3a961", true, 1),
        ("e282ac", true, "00", true, 1),
        ("e282ac", true, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("e282ac", true, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("e282ac", true, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("e282ac", true, "31323334353637383930313233343536373839", false, 1),
        ("e282ac", true, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("e282ac", true, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("e282ac", true, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("e282ac", true, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("e282ac", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("e282ac", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 1),
        ("e282ac", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, 1),
        ("e282ac", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("e282ac", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("e282ac", true, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("e282ac", true, "616161616161616161616161616161", false, 1),
        ("e282ac", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, 1),
        ("e282ac", true, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("e282ac", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("e282ac", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 1),
        ("61c3a9", true, "616263", false, 1),
        ("61c3a9", true, "616264", false, 1),
        ("61c3a9", true, "6162", false, 1),
        ("61c3a9", true, "", false, 1),
        ("61c3a9", true, "e9", false, -1),
        ("61c3a9", true, "ff", false, -1),
        ("61c3a9", true, "c3a9", false, -1),
        ("61c3a9", true, "00", false, 1),
        ("61c3a9", true, "610062", false, 1),
        ("61c3a9", true, "616263", true, 1),
        ("61c3a9", true, "616264", true, 1),
        ("61c3a9", true, "6162", true, 1),
        ("61c3a9", true, "", true, 1),
        ("61c3a9", true, "c3a9", true, -1),
        ("61c3a9", true, "c3bf", true, -1),
        ("61c3a9", true, "c480", true, -1),
        ("61c3a9", true, "e282ac", true, -1),
        ("61c3a9", true, "61c3a9", true, 0),
        ("61c3a9", true, "c3a961", true, -1),
        ("61c3a9", true, "00", true, 1),
        ("61c3a9", true, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("61c3a9", true, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("61c3a9", true, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("61c3a9", true, "31323334353637383930313233343536373839", false, 1),
        ("61c3a9", true, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("61c3a9", true, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("61c3a9", true, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("61c3a9", true, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("61c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("61c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("61c3a9", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("61c3a9", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("61c3a9", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("61c3a9", true, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("61c3a9", true, "616161616161616161616161616161", false, 1),
        ("61c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("61c3a9", true, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("61c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("61c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("c3a961", true, "616263", false, 1),
        ("c3a961", true, "616264", false, 1),
        ("c3a961", true, "6162", false, 1),
        ("c3a961", true, "", false, 1),
        ("c3a961", true, "e9", false, 1),
        ("c3a961", true, "ff", false, -1),
        ("c3a961", true, "c3a9", false, 1),
        ("c3a961", true, "00", false, 1),
        ("c3a961", true, "610062", false, 1),
        ("c3a961", true, "616263", true, 1),
        ("c3a961", true, "616264", true, 1),
        ("c3a961", true, "6162", true, 1),
        ("c3a961", true, "", true, 1),
        ("c3a961", true, "c3a9", true, 1),
        ("c3a961", true, "c3bf", true, -1),
        ("c3a961", true, "c480", true, -1),
        ("c3a961", true, "e282ac", true, -1),
        ("c3a961", true, "61c3a9", true, 1),
        ("c3a961", true, "c3a961", true, 0),
        ("c3a961", true, "00", true, 1),
        ("c3a961", true, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("c3a961", true, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("c3a961", true, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("c3a961", true, "31323334353637383930313233343536373839", false, 1),
        ("c3a961", true, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("c3a961", true, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("c3a961", true, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("c3a961", true, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("c3a961", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("c3a961", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("c3a961", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("c3a961", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("c3a961", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("c3a961", true, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a961", true, "616161616161616161616161616161", false, 1),
        ("c3a961", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, 1),
        ("c3a961", true, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a961", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("c3a961", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("00", true, "616263", false, -1),
        ("00", true, "616264", false, -1),
        ("00", true, "6162", false, -1),
        ("00", true, "", false, 1),
        ("00", true, "e9", false, -1),
        ("00", true, "ff", false, -1),
        ("00", true, "c3a9", false, -1),
        ("00", true, "00", false, 0),
        ("00", true, "610062", false, -1),
        ("00", true, "616263", true, -1),
        ("00", true, "616264", true, -1),
        ("00", true, "6162", true, -1),
        ("00", true, "", true, 1),
        ("00", true, "c3a9", true, -1),
        ("00", true, "c3bf", true, -1),
        ("00", true, "c480", true, -1),
        ("00", true, "e282ac", true, -1),
        ("00", true, "61c3a9", true, -1),
        ("00", true, "c3a961", true, -1),
        ("00", true, "00", true, 0),
        ("00", true, "323032362d30372d32385431343a33333a30375a", false, -1),
        ("00", true, "323032362d30372d32385431343a33333a30385a", false, -1),
        ("00", true, "3139322e3136382e3130302e32303020312e32", false, -1),
        ("00", true, "31323334353637383930313233343536373839", false, -1),
        ("00", true, "323032362d30372d32385431343a33333a30375a", true, -1),
        ("00", true, "61616161616161616161616161616161616161616161616161616161616161", false, -1),
        ("00", true, "61616161616161616161616161616161616161616161616161616161616161", true, -1),
        ("00", true, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("00", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("00", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("00", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("00", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("00", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("00", true, "31313131313131313131313131313131313131313131313131313131313131", false, -1),
        ("00", true, "616161616161616161616161616161", false, -1),
        ("00", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("00", true, "313131313131313131313131313131313131313131313131313131313131", false, -1),
        ("00", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("00", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "616263", false, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "616264", false, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "6162", false, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "", false, 1),
        ("323032362d30372d32385431343a33333a30375a", false, "e9", false, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "ff", false, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "c3a9", false, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "00", false, 1),
        ("323032362d30372d32385431343a33333a30375a", false, "610062", false, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "616263", true, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "616264", true, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "6162", true, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "", true, 1),
        ("323032362d30372d32385431343a33333a30375a", false, "c3a9", true, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "c3bf", true, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "c480", true, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "e282ac", true, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "61c3a9", true, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "c3a961", true, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "00", true, 1),
        ("323032362d30372d32385431343a33333a30375a", false, "323032362d30372d32385431343a33333a30375a", false, 0),
        ("323032362d30372d32385431343a33333a30375a", false, "323032362d30372d32385431343a33333a30385a", false, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("323032362d30372d32385431343a33333a30375a", false, "31323334353637383930313233343536373839", false, 1),
        ("323032362d30372d32385431343a33333a30375a", false, "323032362d30372d32385431343a33333a30375a", true, 0),
        ("323032362d30372d32385431343a33333a30375a", false, "61616161616161616161616161616161616161616161616161616161616161", false, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "61616161616161616161616161616161616161616161616161616161616161", true, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("323032362d30372d32385431343a33333a30375a", false, "616161616161616161616161616161", false, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("323032362d30372d32385431343a33333a30375a", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("323032362d30372d32385431343a33333a30375a", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "616263", false, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "616264", false, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "6162", false, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "", false, 1),
        ("323032362d30372d32385431343a33333a30385a", false, "e9", false, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "ff", false, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "c3a9", false, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "00", false, 1),
        ("323032362d30372d32385431343a33333a30385a", false, "610062", false, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "616263", true, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "616264", true, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "6162", true, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "", true, 1),
        ("323032362d30372d32385431343a33333a30385a", false, "c3a9", true, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "c3bf", true, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "c480", true, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "e282ac", true, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "61c3a9", true, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "c3a961", true, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "00", true, 1),
        ("323032362d30372d32385431343a33333a30385a", false, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("323032362d30372d32385431343a33333a30385a", false, "323032362d30372d32385431343a33333a30385a", false, 0),
        ("323032362d30372d32385431343a33333a30385a", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("323032362d30372d32385431343a33333a30385a", false, "31323334353637383930313233343536373839", false, 1),
        ("323032362d30372d32385431343a33333a30385a", false, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("323032362d30372d32385431343a33333a30385a", false, "61616161616161616161616161616161616161616161616161616161616161", false, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "61616161616161616161616161616161616161616161616161616161616161", true, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("323032362d30372d32385431343a33333a30385a", false, "616161616161616161616161616161", false, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("323032362d30372d32385431343a33333a30385a", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("323032362d30372d32385431343a33333a30385a", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "616263", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "616264", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "6162", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "", false, 1),
        ("3139322e3136382e3130302e32303020312e32", false, "e9", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "ff", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "c3a9", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "00", false, 1),
        ("3139322e3136382e3130302e32303020312e32", false, "610062", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "616263", true, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "616264", true, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "6162", true, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "", true, 1),
        ("3139322e3136382e3130302e32303020312e32", false, "c3a9", true, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "c3bf", true, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "c480", true, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "e282ac", true, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "61c3a9", true, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "c3a961", true, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "00", true, 1),
        ("3139322e3136382e3130302e32303020312e32", false, "323032362d30372d32385431343a33333a30375a", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "323032362d30372d32385431343a33333a30385a", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "3139322e3136382e3130302e32303020312e32", false, 0),
        ("3139322e3136382e3130302e32303020312e32", false, "31323334353637383930313233343536373839", false, 1),
        ("3139322e3136382e3130302e32303020312e32", false, "323032362d30372d32385431343a33333a30375a", true, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "61616161616161616161616161616161616161616161616161616161616161", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "61616161616161616161616161616161616161616161616161616161616161", true, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("3139322e3136382e3130302e32303020312e32", false, "616161616161616161616161616161", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("3139322e3136382e3130302e32303020312e32", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("3139322e3136382e3130302e32303020312e32", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("31323334353637383930313233343536373839", false, "616263", false, -1),
        ("31323334353637383930313233343536373839", false, "616264", false, -1),
        ("31323334353637383930313233343536373839", false, "6162", false, -1),
        ("31323334353637383930313233343536373839", false, "", false, 1),
        ("31323334353637383930313233343536373839", false, "e9", false, -1),
        ("31323334353637383930313233343536373839", false, "ff", false, -1),
        ("31323334353637383930313233343536373839", false, "c3a9", false, -1),
        ("31323334353637383930313233343536373839", false, "00", false, 1),
        ("31323334353637383930313233343536373839", false, "610062", false, -1),
        ("31323334353637383930313233343536373839", false, "616263", true, -1),
        ("31323334353637383930313233343536373839", false, "616264", true, -1),
        ("31323334353637383930313233343536373839", false, "6162", true, -1),
        ("31323334353637383930313233343536373839", false, "", true, 1),
        ("31323334353637383930313233343536373839", false, "c3a9", true, -1),
        ("31323334353637383930313233343536373839", false, "c3bf", true, -1),
        ("31323334353637383930313233343536373839", false, "c480", true, -1),
        ("31323334353637383930313233343536373839", false, "e282ac", true, -1),
        ("31323334353637383930313233343536373839", false, "61c3a9", true, -1),
        ("31323334353637383930313233343536373839", false, "c3a961", true, -1),
        ("31323334353637383930313233343536373839", false, "00", true, 1),
        ("31323334353637383930313233343536373839", false, "323032362d30372d32385431343a33333a30375a", false, -1),
        ("31323334353637383930313233343536373839", false, "323032362d30372d32385431343a33333a30385a", false, -1),
        ("31323334353637383930313233343536373839", false, "3139322e3136382e3130302e32303020312e32", false, -1),
        ("31323334353637383930313233343536373839", false, "31323334353637383930313233343536373839", false, 0),
        ("31323334353637383930313233343536373839", false, "323032362d30372d32385431343a33333a30375a", true, -1),
        ("31323334353637383930313233343536373839", false, "61616161616161616161616161616161616161616161616161616161616161", false, -1),
        ("31323334353637383930313233343536373839", false, "61616161616161616161616161616161616161616161616161616161616161", true, -1),
        ("31323334353637383930313233343536373839", false, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("31323334353637383930313233343536373839", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("31323334353637383930313233343536373839", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("31323334353637383930313233343536373839", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("31323334353637383930313233343536373839", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("31323334353637383930313233343536373839", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("31323334353637383930313233343536373839", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("31323334353637383930313233343536373839", false, "616161616161616161616161616161", false, -1),
        ("31323334353637383930313233343536373839", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("31323334353637383930313233343536373839", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("31323334353637383930313233343536373839", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("31323334353637383930313233343536373839", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "616263", false, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "616264", false, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "6162", false, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "", false, 1),
        ("323032362d30372d32385431343a33333a30375a", true, "e9", false, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "ff", false, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "c3a9", false, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "00", false, 1),
        ("323032362d30372d32385431343a33333a30375a", true, "610062", false, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "616263", true, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "616264", true, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "6162", true, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "", true, 1),
        ("323032362d30372d32385431343a33333a30375a", true, "c3a9", true, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "c3bf", true, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "c480", true, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "e282ac", true, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "61c3a9", true, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "c3a961", true, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "00", true, 1),
        ("323032362d30372d32385431343a33333a30375a", true, "323032362d30372d32385431343a33333a30375a", false, 0),
        ("323032362d30372d32385431343a33333a30375a", true, "323032362d30372d32385431343a33333a30385a", false, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("323032362d30372d32385431343a33333a30375a", true, "31323334353637383930313233343536373839", false, 1),
        ("323032362d30372d32385431343a33333a30375a", true, "323032362d30372d32385431343a33333a30375a", true, 0),
        ("323032362d30372d32385431343a33333a30375a", true, "61616161616161616161616161616161616161616161616161616161616161", false, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "61616161616161616161616161616161616161616161616161616161616161", true, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("323032362d30372d32385431343a33333a30375a", true, "616161616161616161616161616161", false, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("323032362d30372d32385431343a33333a30375a", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("323032362d30372d32385431343a33333a30375a", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "616263", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "616264", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "6162", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "e9", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "ff", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "c3a9", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "00", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "610062", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "616263", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "616264", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "6162", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "", true, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "c3a9", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "c3bf", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "c480", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "e282ac", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "61c3a9", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "c3a961", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "00", true, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "31323334353637383930313233343536373839", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "61616161616161616161616161616161616161616161616161616161616161", false, 0),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "61616161616161616161616161616161616161616161616161616161616161", true, 0),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        (
            "61616161616161616161616161616161616161616161616161616161616161",
            false,
            "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9",
            false,
            -1,
        ),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "616161616161616161616161616161", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "616263", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "616264", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "6162", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "e9", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "ff", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "c3a9", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "00", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "610062", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "616263", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "616264", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "6162", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "", true, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "c3a9", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "c3bf", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "c480", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "e282ac", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "61c3a9", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "c3a961", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "00", true, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "31323334353637383930313233343536373839", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "61616161616161616161616161616161616161616161616161616161616161", false, 0),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "61616161616161616161616161616161616161616161616161616161616161", true, 0),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "616161616161616161616161616161", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("61616161616161616161616161616161616161616161616161616161616161", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "616263", false, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "616264", false, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "6162", false, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "", false, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "e9", false, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "ff", false, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "c3a9", false, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "00", false, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "610062", false, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "616263", true, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "616264", true, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "6162", true, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "", true, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "c3a9", true, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "c3bf", true, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "c480", true, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "e282ac", true, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "61c3a9", true, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "c3a961", true, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "00", true, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "31323334353637383930313233343536373839", false, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "6162636465666768696a206b6c6d6e6f70717273", false, 0),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "616161616161616161616161616161", false, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("6162636465666768696a206b6c6d6e6f70717273", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "616263", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "616264", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "6162", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "e9", false, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "ff", false, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3a9", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "00", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "610062", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "616263", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "616264", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "6162", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3a9", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3bf", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c480", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "e282ac", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "61c3a9", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3a961", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "00", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "31323334353637383930313233343536373839", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        (
            "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9",
            false,
            "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9",
            false,
            0,
        ),
        (
            "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9",
            false,
            "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9",
            true,
            -1,
        ),
        (
            "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9",
            false,
            "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9",
            false,
            -1,
        ),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        (
            "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9",
            false,
            "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080",
            true,
            -1,
        ),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "616161616161616161616161616161", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "616263", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "616264", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "6162", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "e9", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "ff", false, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3a9", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "00", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "610062", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "616263", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "616264", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "6162", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3a9", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3bf", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c480", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "e282ac", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "61c3a9", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3a961", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "00", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "31323334353637383930313233343536373839", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        (
            "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9",
            true,
            "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9",
            false,
            1,
        ),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 0),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        (
            "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9",
            true,
            "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080",
            true,
            -1,
        ),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "616161616161616161616161616161", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "616263", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "616264", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "6162", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "e9", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "ff", false, -1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "c3a9", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "00", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "610062", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "616263", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "616264", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "6162", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "c3a9", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "c3bf", true, -1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "c480", true, -1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "e282ac", true, -1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "61c3a9", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "c3a961", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "00", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "31323334353637383930313233343536373839", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, 0),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "616161616161616161616161616161", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "616263", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "616264", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "6162", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "e9", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "ff", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "c3a9", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "00", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "610062", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "616263", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "616264", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "6162", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "c3a9", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "c3bf", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "c480", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "e282ac", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "61c3a9", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "c3a961", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "00", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "31323334353637383930313233343536373839", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, 0),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "616161616161616161616161616161", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "616263", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "616264", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "6162", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "e9", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "ff", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "c3a9", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "00", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "610062", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "616263", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "616264", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "6162", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "c3a9", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "c3bf", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "c480", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "e282ac", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "61c3a9", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "c3a961", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "00", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "31323334353637383930313233343536373839", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        (
            "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080",
            true,
            "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9",
            false,
            1,
        ),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, 0),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "616161616161616161616161616161", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "616263", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "616264", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "6162", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "", false, 1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "e9", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "ff", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "c3a9", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "00", false, 1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "610062", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "616263", true, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "616264", true, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "6162", true, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "", true, 1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "c3a9", true, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "c3bf", true, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "c480", true, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "e282ac", true, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "61c3a9", true, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "c3a961", true, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "00", true, 1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "323032362d30372d32385431343a33333a30375a", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "323032362d30372d32385431343a33333a30385a", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "3139322e3136382e3130302e32303020312e32", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "31323334353637383930313233343536373839", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "323032362d30372d32385431343a33333a30375a", true, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "61616161616161616161616161616161616161616161616161616161616161", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "61616161616161616161616161616161616161616161616161616161616161", true, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        (
            "31313131313131313131313131313131313131313131313131313131313131",
            false,
            "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9",
            false,
            -1,
        ),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "31313131313131313131313131313131313131313131313131313131313131", false, 0),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "616161616161616161616161616161", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("31313131313131313131313131313131313131313131313131313131313131", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("616161616161616161616161616161", false, "616263", false, -1),
        ("616161616161616161616161616161", false, "616264", false, -1),
        ("616161616161616161616161616161", false, "6162", false, -1),
        ("616161616161616161616161616161", false, "", false, 1),
        ("616161616161616161616161616161", false, "e9", false, -1),
        ("616161616161616161616161616161", false, "ff", false, -1),
        ("616161616161616161616161616161", false, "c3a9", false, -1),
        ("616161616161616161616161616161", false, "00", false, 1),
        ("616161616161616161616161616161", false, "610062", false, 1),
        ("616161616161616161616161616161", false, "616263", true, -1),
        ("616161616161616161616161616161", false, "616264", true, -1),
        ("616161616161616161616161616161", false, "6162", true, -1),
        ("616161616161616161616161616161", false, "", true, 1),
        ("616161616161616161616161616161", false, "c3a9", true, -1),
        ("616161616161616161616161616161", false, "c3bf", true, -1),
        ("616161616161616161616161616161", false, "c480", true, -1),
        ("616161616161616161616161616161", false, "e282ac", true, -1),
        ("616161616161616161616161616161", false, "61c3a9", true, -1),
        ("616161616161616161616161616161", false, "c3a961", true, -1),
        ("616161616161616161616161616161", false, "00", true, 1),
        ("616161616161616161616161616161", false, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("616161616161616161616161616161", false, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("616161616161616161616161616161", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("616161616161616161616161616161", false, "31323334353637383930313233343536373839", false, 1),
        ("616161616161616161616161616161", false, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("616161616161616161616161616161", false, "61616161616161616161616161616161616161616161616161616161616161", false, -1),
        ("616161616161616161616161616161", false, "61616161616161616161616161616161616161616161616161616161616161", true, -1),
        ("616161616161616161616161616161", false, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("616161616161616161616161616161", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("616161616161616161616161616161", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("616161616161616161616161616161", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("616161616161616161616161616161", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("616161616161616161616161616161", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("616161616161616161616161616161", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("616161616161616161616161616161", false, "616161616161616161616161616161", false, 0),
        ("616161616161616161616161616161", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("616161616161616161616161616161", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("616161616161616161616161616161", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("616161616161616161616161616161", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "616263", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "616264", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "6162", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "e9", false, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "ff", false, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "c3a9", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "00", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "610062", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "616263", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "616264", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "6162", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "c3a9", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "c3bf", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "c480", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "e282ac", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "61c3a9", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "c3a961", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "00", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "31323334353637383930313233343536373839", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "616161616161616161616161616161", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, 0),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "616263", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "616264", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "6162", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "", false, 1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "e9", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "ff", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "c3a9", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "00", false, 1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "610062", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "616263", true, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "616264", true, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "6162", true, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "", true, 1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "c3a9", true, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "c3bf", true, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "c480", true, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "e282ac", true, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "61c3a9", true, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "c3a961", true, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "00", true, 1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "323032362d30372d32385431343a33333a30375a", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "323032362d30372d32385431343a33333a30385a", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "3139322e3136382e3130302e32303020312e32", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "31323334353637383930313233343536373839", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "323032362d30372d32385431343a33333a30375a", true, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "61616161616161616161616161616161616161616161616161616161616161", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "61616161616161616161616161616161616161616161616161616161616161", true, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "6162636465666768696a206b6c6d6e6f70717273", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "31313131313131313131313131313131313131313131313131313131313131", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "616161616161616161616161616161", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "313131313131313131313131313131313131313131313131313131313131", false, 0),
        ("313131313131313131313131313131313131313131313131313131313131", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("313131313131313131313131313131313131313131313131313131313131", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "616263", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "616264", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "6162", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "e9", false, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "ff", false, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3a9", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "00", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "610062", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "616263", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "616264", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "6162", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3a9", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3bf", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c480", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "e282ac", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "61c3a9", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3a961", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "00", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "31323334353637383930313233343536373839", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "616161616161616161616161616161", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 0),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "616263", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "616264", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "6162", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "e9", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "ff", false, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3a9", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "00", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "610062", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "616263", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "616264", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "6162", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3a9", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3bf", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c480", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "e282ac", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "61c3a9", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3a961", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "00", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "323032362d30372d32385431343a33333a30375a", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "323032362d30372d32385431343a33333a30385a", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "3139322e3136382e3130302e32303020312e32", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "31323334353637383930313233343536373839", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "323032362d30372d32385431343a33333a30375a", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "61616161616161616161616161616161616161616161616161616161616161", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "61616161616161616161616161616161616161616161616161616161616161", true, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "6162636465666768696a206b6c6d6e6f70717273", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", false, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9e9", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "f4908080f4908080f4908080f4908080f4908080f4908080f4908080f4908080", true, -1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "31313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "616161616161616161616161616161", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a961", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "313131313131313131313131313131313131313131313131313131313131", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", false, 1),
        ("c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, "c3a9c3a9c3a9c3a9c3a9c3a9c3a9c3a9", true, 0),
    ];

    for (ah, af, bh, bf, want) in cases {
        let (a, b) = (from_hex(ah, *af), from_hex(bh, *bf));
        let got = match a.cmp_perl(&b) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        };
        assert_eq!(got, *want, "cmp of {ah:?}/{af} against {bh:?}/{bf}");

        // Ordering and equality must agree, or a sort and a lookup can disagree about the same pair.
        assert_eq!(a == b, *want == 0, "eq disagrees with cmp for {ah:?}/{af} against {bh:?}/{bf}");
    }
}

#[test]
fn ord_is_the_ordering_that_agrees_with_equality() {
    // `Ord` carries perl's cmp, so sorting and lookups agree.  Byte-mode ordering cannot be the trait: these two share
    // their internal bytes — c3 a9 — but one is two Latin-1 characters and the other is U+00E9, so raw octets would
    // call them equal where equality calls them unequal.
    let plain = from_hex("c3a9", false);
    let flagged = from_hex("c3a9", true);
    assert_ne!(plain, flagged);
    assert_eq!(plain.cmp(&flagged), Ordering::Less, "Ord agrees with perl and with equality");
    assert_eq!(plain.cmp_raw_bytes(&flagged), Ordering::Equal, "byte mode sees identical octets");

    // Sorting works, and matches perl's order for a mixed corpus.
    let mut v = [
        from_hex("ff", false),  // one octet, U+00FF
        from_hex("c480", true), // U+0100
        from_hex("61", false),  // "a"
        from_hex("e9", false),  // U+00E9
    ];

    v.sort();
    let order: Vec<usize> = v.iter().map(|s| s.len()).collect();
    assert_eq!(v[0], from_hex("61", false), "\"a\" first");
    assert_eq!(v[3], from_hex("c480", true), "U+0100 last");
    assert!(order.len() == 4);

    // And the trait's own consistency obligation, over the container-verified corpus.
    for (a, b) in [("616263", "616264"), ("e9", "c480"), ("", "61")] {
        let (x, y) = (from_hex(a, false), from_hex(b, false));
        assert_eq!(x < y, x.cmp(&y) == Ordering::Less);
        assert_eq!(x == y, x.cmp(&y) == Ordering::Equal);
    }
}

#[test]
fn the_flag_is_semantically_null_for_ascii() {
    // Seven-bit content encodes identically as octets and as UTF-8, so the flag changes nothing about the value: it
    // takes a code point of U+0080 or above for the flag to mean anything.  Equality, ordering, and hashing must
    // therefore ignore it here — while `is_utf8` still reports it, perl exposing the flag through `utf8::is_utf8`
    // even where it is semantically null.
    //
    // Perl does not canonicalize the flag for ASCII — it may be set or clear arbitrarily — so all four combinations
    // over identical seven-bit bytes must agree.  Our scan state records ASCII as its own value, which perl has no
    // equivalent of, and that extra distinction must not leak into comparison.
    for hex in ["", "61", "616263", "7f", "004161", "30313233343536373839616263"] {
        for (lf, rf) in [(false, false), (false, true), (true, false), (true, true)] {
            let (a, b) = (from_hex(hex, lf), from_hex(hex, rf));
            assert_eq!(a, b, "{hex:?} with flags {lf}/{rf}");
            assert_eq!(a.cmp_perl(&b), Ordering::Equal, "{hex:?} with flags {lf}/{rf}");
            assert_eq!(hash_of(&a), hash_of(&b), "{hex:?} with flags {lf}/{rf}: equal values must hash alike");
        }
    }

    // And ASCII strings that differ in content still differ, whatever the flags.
    for (lf, rf) in [(false, false), (false, true), (true, false), (true, true)] {
        assert_ne!(from_hex("616263", lf), from_hex("616264", rf), "flags {lf}/{rf}");
        assert_eq!(from_hex("616263", lf).cmp_perl(&from_hex("616264", rf)), Ordering::Less);
    }

    let plain = from_hex("616263", false);
    let flagged = from_hex("616263", true);
    assert_ne!(plain.is_utf8(), flagged.is_utf8(), "the flag is still observable");

    // At U+0080 and above the flag becomes load-bearing: the same octets are two Latin-1 characters unflagged and
    // one character flagged.
    let two_octets = from_hex("c3a9", false);
    let one_char = from_hex("c3a9", true);
    assert_ne!(two_octets, one_char, "here the flag decides the value");
    assert_eq!(two_octets.len(), 2);
    assert_eq!(one_char.char_len(), Some(1));
}

#[test]
fn identical_ascii_bytes_are_one_value_across_every_flag_and_tier() {
    // The property, exhaustively rather than by sample: if the bytes are seven-bit and identical, the strings are
    // the same value whatever the flags say.  Swept across all three storage tiers, because each has its own
    // comparison path — inline runs the scan-state grid, packed has a nibble fast path, heap compares buffers — and
    // a shortcut in any of them could let the flag leak into the answer.
    //
    // Packed content is always ASCII by construction, so the flagged packed forms exist precisely to be equal to
    // their unflagged twins.
    let mut contents: Vec<Vec<u8>> = Vec::new();
    for len in 0..=40 {
        contents.push((0..len).map(|i| b'a' + (i % 26) as u8).collect()); // inline, then heap
        contents.push((0..len).map(|i| b'0' + (i % 10) as u8).collect()); // packs, in the numeric alphabet
    }
    contents.push(b"2026-07-28T14:33:07Z".to_vec()); // packs, date-time alphabet
    contents.push(b"2026-07-29T17:23:45.123456789Z".to_vec()); // packs, full family
    contents.push(b"\x00\x01\x7f".to_vec()); // the seven-bit extremes, including NUL
    contents.push(vec![0u8; 20]); // all NUL, packed band length

    let mut tiers = std::collections::BTreeSet::new();
    for bytes in &contents {
        assert!(bytes.iter().all(|&b| b < 0x80), "fixture must be seven-bit");
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

        for (lf, rf) in [(false, false), (false, true), (true, false), (true, true)] {
            let (a, b) = (from_hex(&hex, lf), from_hex(&hex, rf));
            tiers.insert(a.storage_type());

            assert_eq!(a, b, "len {} flags {lf}/{rf}", bytes.len());
            assert_eq!(a.cmp_perl(&b), Ordering::Equal, "len {} flags {lf}/{rf}", bytes.len());
            assert_eq!(b.cmp_perl(&a), Ordering::Equal, "len {} flags {rf}/{lf}", bytes.len());
            assert_eq!(hash_of(&a), hash_of(&b), "len {} flags {lf}/{rf}: equal values must hash alike", bytes.len());
        }
    }

    assert!(
        tiers.iter().any(|t| t.is_inline()) && tiers.iter().any(|t| t.is_packed()) && tiers.iter().any(|t| t.is_heap()),
        "the sweep must reach every tier, saw {tiers:?}"
    );
    assert!(tiers.len() >= 5, "the storage types seen should span families and alphabets too, saw {tiers:?}");
}

// ═══ The packed nibble tier ══════════════════════════════════════════════════════════════════════════════════════════

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
fn every_alphabet_symbol_round_trips() {
    // Built from the tables themselves so a table edit cannot outrun it.  Each alphabet's full sixteen-symbol sweep
    // packs, selects its own alphabet — every sweep contains a symbol the other two lack — and decodes back exactly,
    // which covers all sixteen nibble values in both directions where the representative citizens only cover the
    // symbols they happen to use.  Symbol order must equal nibble value: that is the ASCII-order property the no-decode
    // comparisons rest on, and the ascending check pins it against table edits.
    for (symbols, alphabet) in [
        (NUMERIC_SYMBOLS, PackedAlphabet::Numeric),
        (DATETIME_PLUS_SYMBOLS, PackedAlphabet::DateTimePlus),
        (DATETIME_ZULU_SYMBOLS, PackedAlphabet::DateTimeZulu),
    ] {
        assert!(symbols.windows(2).all(|w| w[0] < w[1]), "symbols must ascend in ASCII order");
        let p = roundtrip(symbols);
        assert_eq!(p.alphabet, alphabet, "each sweep must select its own alphabet");
        for (i, &sym) in symbols.iter().enumerate() {
            assert_eq!(alphabet.encode_table()[sym as usize] as usize, i, "symbol order must equal nibble value");
        }
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
fn nibble_assignment_is_ascii_monotonic() {
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
fn numeric_reclassifies_as_datetime_plus_without_rewriting() {
    // The two lists agree on nibbles 0-13, so this is a pure reclassification whenever no exponent symbol is present —
    // the nibble array comes out identical.
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

    // Moving from Numeric is free: the two agree on nibbles 0-13, so nothing is rewritten.
    let numeric = pack(b"2026-07-28 2026-07-29").unwrap();
    assert_eq!(numeric.alphabet, PackedAlphabet::Numeric);
    let widened = numeric.transcode(PackedAlphabet::DateTimePlus).unwrap();
    assert_eq!(widened.nibbles, numeric.nibbles, "reclassification rewrites no nibble");
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
        _ => unreachable!("the append path only ever moves along these three transitions"),
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
        for target in [PackedAlphabet::Numeric, PackedAlphabet::DateTimePlus, PackedAlphabet::DateTimeZulu] {
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

// ── Representation transforms and canonical selection (§2.2.9) ───────────────────────────────────────────────────────

#[test]
fn equal_values_hash_alike_across_provenance_and_scan_knowledge() {
    // One canonical stream, three provenances: a raw slice, the per-character downgrade emit, and the dual walk's
    // block-wise spans.  The chunk feed makes the hasher call shapes identical; the Hasher contract does not.
    let raw = from_hex(&"e9".repeat(80), false); // Heap Bytes class: the raw stream arrives as one slice.
    let flagged = from_hex(&"c3a9".repeat(80), true); // Heap flagged Latin-1: the same stream, emitted per character.
    assert_eq!(raw, flagged, "sv_eq upgrades the byte side: equal");
    assert_eq!(hash_of(&raw), hash_of(&flagged), "equal values, different provenances, one digest");

    // The same value hashed cold (unknown scan: the dual walk) and again after the knowledge narrows (the known Latin-1
    // arm) must agree with itself across the arms.
    let cold = from_hex(&"c3a9".repeat(80), true);
    let under_unknown = hash_of(&cold);
    let _ = cold.char_len(); // Narrows the heap lattice to the terminal state.
    assert_eq!(hash_of(&cold), under_unknown, "the dual walk and the known arm must digest alike");
}

#[test]
fn upgrade_and_downgrade_preserve_characters() {
    // The monster's transforms: flag-off E9 upgrades with zero byte work — Bytes re-tags to flagged Latin-1, the
    // payload identical — and flagged é downgrades back the same way.
    let raw = from_hex("e9", false);
    let up = raw.upgraded().unwrap();
    assert_eq!(up, from_hex("c3a9", true), "the character survives: é stays é");
    assert_eq!(up.storage_type(), StorageType::InlineLatin1);
    assert_eq!(up, raw, "sv_eq upgrades the byte side: still equal");
    assert_eq!(up.downgraded().unwrap().unwrap(), raw);

    // Upgrading flag-off C3 A9 yields the two-character Ã©, not é — upgrade is not reinterpretation.
    let octets = from_hex("c3a9", false);
    let up2 = octets.upgraded().unwrap();
    assert_ne!(up2, from_hex("c3a9", true), "Ã© is not é");
    assert_eq!(up2.char_len(), Some(2));
    assert_eq!(up2.len(), 4, "two characters, C3 and A9, each encoding to two bytes");
    assert_eq!(up2.downgraded().unwrap().unwrap(), octets, "downgrade returns exactly the octets");

    // Beyond Latin-1 cannot downgrade, and flagged Bytes-class content has no characters to downgrade.
    assert_eq!(from_hex("e282ac", true).downgraded().unwrap(), None);
    assert_eq!(from_hex("e9", true).downgraded().unwrap(), None);

    // ASCII upgrades and downgrades as pure flips in every tier: inline, packed, and heap.
    for hex in ["616263", &"31".repeat(20), &"61".repeat(40)] {
        let s = from_hex(hex, false);
        let up = s.upgraded().unwrap();
        assert!(up.is_utf8());
        assert_eq!(up.storage_type(), s.storage_type(), "the representation is already right");
        assert_eq!(up.downgraded().unwrap().unwrap(), s);
    }

    // Sixteen non-ASCII characters exceed every flagged non-heap form: the upgrade heaps, stays character-exact, and
    // downgrades back through the ladder to exactly where the content fits.
    let wide = from_hex(&"e9".repeat(16), false);
    let up = wide.upgraded().unwrap();
    assert!(up.storage_type().is_heap());
    assert_eq!(up.char_len(), Some(16));
    assert_eq!(up.downgraded().unwrap().unwrap(), wide);
}

#[test]
fn reinterpretation_is_a_pure_flag_flip() {
    // Container-probed: _utf8_off on an upgraded é yields the flag-off two-character C3.A9 with the payload untouched —
    // and the class axis never moves, being a fact about the bytes (§2.2.9).
    let wide = "c3a9".repeat(10);
    for (hex, flagged) in [("c3a9", true), ("e9", false), ("e282ac", true), (wide.as_str(), false)] {
        let s = from_hex(hex, flagged);
        let mut flipped = s.clone();
        flipped.reinterpret_utf8(!flagged);
        assert_eq!(flipped, from_hex(hex, !flagged), "the value is the same bytes under the other flag");
        assert_eq!(flipped.storage_type(), s.storage_type(), "the class is a fact about the bytes");

        let mut back = flipped.clone();
        back.reinterpret_utf8(flagged);
        assert_eq!(back, s);
    }

    let e_acute = from_hex("c3a9", true);
    let mut off = e_acute.clone();
    off.reinterpret_utf8(false);
    assert_eq!(off.len(), 2, "flag-off C3.A9 is the two-octet string");
    assert_ne!(off, e_acute, "the octet string is not the character é");
}

#[test]
fn byte_mutation_reruns_canonical_selection() {
    // chop's split, as a constructor fact (container-verified): removing the trailing octet of A.C3.A9 leaves a
    // dangling lead that no longer reads as UTF-8 — the Bytes residual.
    assert_eq!(from_hex("41c3", false).storage_type(), StorageType::InlineBytes);

    // And live mutation through append — the pass-through hazard: a Bytes-class dangling lead completed by its
    // continuation becomes valid Latin-1-range UTF-8 again, and canonical selection re-compresses it.
    let mut s = from_hex("616263c3", false);
    assert_eq!(s.storage_type(), StorageType::InlineBytes);

    s.push_bytes(b"\xA9").unwrap();
    assert_eq!(s.storage_type(), StorageType::InlineLatin1, "abc + é: valid again, so it compresses");
    assert_eq!(s, from_hex("616263c3a9", false));
    assert_eq!(s.char_len(), Some(4));
}

#[test]
fn nul_compresses_in_every_spelling() {
    // The revised ruling (§2.2.9): the octet, the encoded byte, and the character U+0000 are ordinary content — the
    // explicit length is what admits them, a terminator having no way to.
    assert_eq!(from_hex("00", false).storage_type(), StorageType::InlineAscii);
    assert_eq!(from_hex("00", false).len(), 1);

    let s = from_hex("c3a900", true); // é then U+0000, flagged: two characters, one of them NUL.
    assert_eq!(s.storage_type(), StorageType::InlineLatin1);
    assert_eq!(s.char_len(), Some(2));
    assert_eq!(s.len(), 3);

    assert_eq!(from_hex("e900", false).storage_type(), StorageType::InlineBytes, "beside an invalid octet: verbatim");
    assert_ne!(from_hex("610062", false), from_hex("610063", false), "equality sees past a NUL");
}

#[test]
fn equal_content_takes_equal_representations_across_routes() {
    // Canonical selection means routes group by which string they produce — downgrade preserves characters where
    // reinterpretation preserves bytes, the E9 monster in transform clothing.
    let direct = from_hex("e9e9", false);
    let via_downgrade = from_hex("c3a9c3a9", true).downgraded().unwrap().unwrap();
    let via_round_trip = direct.upgraded().unwrap().downgraded().unwrap().unwrap();
    assert_eq!(direct, via_downgrade);
    assert_eq!(direct.storage_type(), via_downgrade.storage_type());
    assert_eq!(direct, via_round_trip);

    let data = from_hex("c3a9c3a9", false);
    let mut via_flip = from_hex("c3a9c3a9", true);
    via_flip.reinterpret_utf8(false);
    assert_eq!(data, via_flip);
    assert_eq!(data.storage_type(), via_flip.storage_type());
    assert_ne!(direct, data, "and the two groups are different strings");
}

#[test]
fn char_count_zero_is_dual_purposed_by_the_byte_length() {
    // The §2.2.4 ruling: zero means "no cached count", and the byte length disambiguates — zero bytes hold zero
    // characters by definition, so the field is never consulted there; nonempty decodable content counts at least one;
    // malformed content keeps zero permanently, the scan byte saying which case holds.
    let mut s = PerlString::from_bytes(b"heap content that is long enough to stay heap").unwrap();
    s.push_bytes(b"").unwrap();
    assert_eq!(s.char_len(), Some(45));

    // Truncation to empty through the mutable escape: the count answer is 0 without any recount possible or needed.
    let empty = PerlString::from_bytes(vec![]).unwrap();
    assert_eq!(empty.char_len(), Some(0));

    // Malformed heap content: no clean answer, and the count field stays at zero behind the scan byte's verdict.
    let mal = from_hex(&"e9".repeat(40), false);
    assert!(mal.storage_type().is_heap());
    assert_eq!(mal.char_len(), None);

    // The cache holds the flag-on answer regardless of the handle's flag: the flag-off octets and their flagged
    // reading are one buffer fact apart from the tag.
    let off = from_hex(&"c3a9".repeat(40), false);
    let on = from_hex(&"c3a9".repeat(40), true);
    assert_eq!(on.char_len(), Some(40), "the flag-on count: forty characters");
    assert_eq!(off.char_len(), Some(40), "char_len answers the flag-on question for either handle");
    assert_eq!(off.len(), 80, "the flag-off length answer is the byte length the envelope already knows");
}
