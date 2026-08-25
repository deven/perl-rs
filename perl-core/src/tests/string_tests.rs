use super::*;
use std::collections::HashMap;
use std::str::FromStr;

fn hash_of(s: &PString) -> u64 {
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
    let inline = PString::from_str(&"a".repeat(15)).unwrap();
    assert!(inline.storage_type().is_inline());

    // Letters belong to no packed alphabet, so past the inline payload they go to the heap.
    let lettered = PString::from_str(&"a".repeat(16)).unwrap();
    assert!(lettered.storage_type().is_heap());
    assert_eq!(lettered.len(), 16);

    // Digit-dense content of the same length does not.
    for text in ["1234567890123456", "2.2250738585072e-308", "2026-07-28T14:33:07Z", "192.168.100.200 1.2"] {
        let packed = PString::from_str(text).unwrap();
        assert!(packed.storage_type().is_packed(), "{text} should pack");
        assert_eq!(packed.len(), text.len());
        assert_eq!(packed.as_bytes(&mut [0u8; DECODE_MAX]), text.as_bytes());
    }

    // Past the packed capacity there is no non-allocating form left.
    let long = PString::from_str(&"1".repeat(31)).unwrap();
    assert!(long.storage_type().is_heap());
    assert_eq!(long.len(), 31);
}

#[test]
fn ascii_from_str_is_unflagged_canonical() {
    let s = PString::from_str("hello").unwrap();
    assert!(!s.is_utf8(), "ASCII stores in canonical downgraded form");
    assert_eq!(s.inline_class(), Some(InlineClass::Ascii));
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), Some("hello"));
}

#[test]
fn non_ascii_from_str_is_flagged() {
    let s = PString::from_str("héllo").unwrap();
    assert!(s.is_utf8());
    assert_eq!(s.inline_class(), Some(InlineClass::Latin1)); // é is U+00E9: Latin-1 range
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), Some("héllo"));
}

#[test]
fn invalid_bytes_inline_scan_terminal() {
    let s = PString::from_bytes([0xFF, 0xFE]).unwrap();
    assert_eq!(s.inline_class(), Some(InlineClass::Bytes));
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), None);
    assert!(!s.is_ascii());
    assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), &[0xFF, 0xFE]);
}

#[test]
fn heap_from_bytes_defers_scanning() {
    let bytes = vec![b'x'; 40];
    let s = PString::from_bytes(&bytes).unwrap();
    assert!(s.storage_type().is_heap());

    // as_str triggers the lazy scan and narrows.
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), Some("x".repeat(40).as_str()));
    assert!(s.is_ascii());
}

// ── Character-sequence equality (container-verified cases) ────
#[test]
fn eq_same_flags_is_byte_equality() {
    let a = PString::from_str("hello").unwrap();
    let b = PString::from_bytes(b"hello").unwrap();
    assert_eq!(a, b); // both unflagged ASCII
}

#[test]
fn eq_cross_flag_same_bytes_can_differ() {
    // Verified perl 5.38: unflagged C3 A9 is the two characters "\xc3\xa9"; flagged it is "é" — not eq.
    let mut flagged = PString::from_bytes([0xC3, 0xA9]).unwrap();
    flagged.set_utf8_for_test();
    let unflagged = PString::from_bytes([0xC3, 0xA9]).unwrap();
    assert_ne!(flagged, unflagged);
}

#[test]
fn eq_cross_flag_different_bytes_can_match() {
    // Verified perl 5.38: unflagged E9 (latin-1 é) eq flagged C3 A9 (UTF-8 é).
    let mut flagged = PString::from_bytes([0xC3, 0xA9]).unwrap();
    flagged.set_utf8_for_test();
    let latin1 = PString::from_bytes([0xE9]).unwrap();
    assert_eq!(flagged, latin1);
    assert_eq!(latin1, flagged);
}

#[test]
fn eq_ignores_tainted() {
    let a = PString::from_str("same").unwrap();
    let mut b = PString::from_str("same").unwrap();
    b.taint();
    assert_eq!(a, b);
    assert_eq!(hash_of(&a), hash_of(&b));
}

// ── Canonical hashing (container-verified hash-key semantics) ─
#[test]
fn hash_key_flag_insensitive() {
    // Verified perl 5.38: utf8::upgrade/downgrade variants of a key are ONE key.
    let mut flagged = PString::from_bytes([0xC3, 0xA9]).unwrap();
    flagged.set_utf8_for_test();
    let latin1 = PString::from_bytes([0xE9]).unwrap();
    assert_eq!(hash_of(&flagged), hash_of(&latin1), "equal strings must hash equal");
    let mut h: HashMap<PString, i32> = HashMap::new();
    h.insert(flagged, 1);
    h.insert(latin1, 2);
    assert_eq!(h.len(), 1, "Perl hash keys are flag-insensitive");
}

// ── Tag transitions ───────────────────────────────────────────
#[test]
fn taint_round_trip_via_sanctioned_path() {
    let mut s = PString::from_str("data").unwrap();
    s.taint();
    assert!(s.is_tainted());
    s.untaint_for_sanctioned_path();
    assert!(!s.is_tainted());
}

// ── Append transitions (§2.2.5) ───────────────────────────────
#[test]
fn ascii_append_preserves_state() {
    let mut s = PString::from_str("abc").unwrap();
    s.push_str("def").unwrap();
    assert_eq!(s.inline_class(), Some(InlineClass::Ascii));
    assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), b"abcdef");
}

#[test]
fn valid_utf8_append_to_ascii_goes_non_ascii() {
    let mut s = PString::from_str("abc").unwrap();
    s.push_str("é").unwrap();
    assert_eq!(s.inline_class(), Some(InlineClass::Latin1)); // ASCII + é joins to Latin-1 range
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), Some("abcé"));
}

#[test]
fn inline_overflow_promotes_to_heap_one_way() {
    let mut s = PString::from_str(&"a".repeat(20)).unwrap();
    s.push_str("bcdef").unwrap(); // 25 bytes
    assert!(s.storage_type().is_heap());
    assert_eq!(s.len(), 25);
    assert!(s.is_ascii(), "promotion carried the scan knowledge");

    // Shrinking (future truncate) must not demote — pinned when truncate lands.
}

#[test]
fn heap_append_transitions() {
    let mut s = PString::from_str(&"a".repeat(30)).unwrap(); // Heap, ASCII known
    s.push_str("é").unwrap();
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]).map(|v| v.len()), Some(32));

    // The transition raised the range without a rescan; below 64 KiB the answer is terminal by type.
    assert!(!s.is_ascii());
    let mut raw = PString::from_bytes([0x80u8; 30]).unwrap(); // small tier: terminal (malformed) at construction
    raw.push_bytes(&[0x81]).unwrap(); // indeterminate transition → the funnel reclassifies (§2.2.3)
    assert_eq!(raw.as_str(&mut [0u8; DECODE_MAX]), None); // answered from the recorded state
}

#[test]
fn flag_and_bits_survive_promotion() {
    let mut s = PString::from_str(&"é".repeat(15)).unwrap(); // Fifteen stored bytes: full-capacity inline, flagged.
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
        let s = PString::from_bytes(bytes).unwrap();
        assert_eq!(s.inline_class(), Some(InlineClass::Extended), "{bytes:02X?}");
        assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), None, "Rust view must reject extended");
        assert!(s.is_perl_utf8_valid(), "perl view must accept extended");
        assert!(!s.is_ascii());
    }

    // Malformed for perl too: overlong, bare continuation, truncated, overlong FF form.
    let overlong_ff: Vec<u8> = std::iter::once(0xFFu8).chain(std::iter::repeat_n(0x80u8, 12)).collect();
    for bytes in [&[0xC0, 0x80][..], &[0x80], &[0xC3], &overlong_ff] {
        let s = PString::from_bytes(bytes).unwrap();
        assert_eq!(s.inline_class(), Some(InlineClass::Bytes), "{bytes:02X?}");
        assert!(!s.is_perl_utf8_valid());
    }
}

#[test]
fn extended_taxonomy_heap_lazy() {
    // Heap string ending in an extended sequence: lazy classification narrows to EXTENDED_UTF8.
    let mut bytes = vec![b'a'; 30];
    bytes.extend_from_slice(&[0xF4, 0x90, 0x80, 0x80]);
    let s = PString::from_bytes(&bytes).unwrap();
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), None);
    assert!(s.is_perl_utf8_valid());

    // And a malformed heap string classifies INVALID.
    let mut bad = vec![b'a'; 30];
    bad.push(0xC0);
    bad.push(0x80);
    let t = PString::from_bytes(&bad).unwrap();
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
    let s = PString::from_bytes(&seq).unwrap();
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
    let t = PString::from_bytes(&seq2).unwrap();
    assert_eq!(t.inline_class(), Some(InlineClass::Bytes), "FF encoding a FE-range value is overlong");
}

#[test]
fn extended_append_transitions() {
    let mut s = PString::from_bytes([0xF4, 0x90, 0x80, 0x80]).unwrap();
    s.push_str("abc").unwrap();
    assert_eq!(s.inline_class(), Some(InlineClass::Extended), "valid append preserves extended");
    assert!(s.is_perl_utf8_valid());
}

#[test]
fn extended_eq_and_hash_behavior() {
    // A flagged extended string equals no unflagged string (chars above 0xFF) and byte-identical flagged self.
    let mut a = PString::from_bytes([0xF4, 0x90, 0x80, 0x80]).unwrap();
    a.set_utf8_for_test();
    let mut b = PString::from_bytes([0xF4, 0x90, 0x80, 0x80]).unwrap();
    b.set_utf8_for_test();
    assert_eq!(a, b);
    assert_eq!(hash_of(&a), hash_of(&b));
    let plain = PString::from_bytes([0xF4, 0x90, 0x80, 0x80]).unwrap();
    assert_ne!(a, plain, "flag changes the character sequence");
}

// ── Range-tuned lattice (§2.2.4) ──────────────────────────────
#[test]
fn latin1_vs_non_latin1_terminals() {
    let e = PString::from_str("é").unwrap(); // U+00E9
    assert_eq!(e.inline_class(), Some(InlineClass::Latin1));
    let cjk = PString::from_str("字").unwrap(); // U+5B57
    assert_eq!(cjk.inline_class(), Some(InlineClass::NonLatin1));
    let mixed = PString::from_str("é字").unwrap();
    assert_eq!(mixed.inline_class(), Some(InlineClass::NonLatin1), "range joins upward");
}

#[test]
fn unknown_range_classifies_on_ascii_probe() {
    let s = PString::from_str(&"é".repeat(20)).unwrap(); // 40 bytes: small tier, settled Utf8Latin1 at birth
    assert!(s.storage_type().is_heap());
    assert!(!s.is_ascii(), "the ASCII question is a state read on a settled birth");

    // The classification left terminal Latin-1 knowledge behind: cross-flag equality against the downgraded form
    // succeeds (and would fast-negative if the state had wrongly become NON_LATIN1).
    let plain = PString::from_bytes([0xE9u8; 20]).unwrap();
    assert_eq!(s, plain);
}

#[test]
fn eq_grid_same_flag_length_mismatch() {
    // Same flags + different byte lengths ⇒ ne, at both flag settings.
    let a = PString::from_bytes(b"abc").unwrap();
    let b = PString::from_bytes(b"abcd").unwrap();
    assert_ne!(a, b);
    let mut fa = PString::from_bytes([0xC3, 0xA9]).unwrap();
    fa.set_utf8_for_test();
    let mut fb = PString::from_bytes([0xC3, 0xA9, 0x41]).unwrap();
    fb.set_utf8_for_test();
    assert_ne!(fa, fb);
}

#[test]
fn eq_cross_flag_flagged_longer_positive_and_negative() {
    // Flagged longer CAN match (char count < byte count): é as C3 A9 vs E9 — the positive case.
    let mut f = PString::from_bytes([0xC3, 0xA9]).unwrap();
    f.set_utf8_for_test();
    assert_eq!(f, PString::from_bytes([0xE9]).unwrap());

    // Flagged longer, mismatch mid-walk.
    assert_ne!(f, PString::from_bytes([0xEA]).unwrap());

    // Flagged longer, plain exhausted with flagged characters remaining: "é" + "a" vs just é.
    let mut f2 = PString::from_bytes([0xC3, 0xA9, b'a']).unwrap();
    f2.set_utf8_for_test();
    assert_ne!(f2, PString::from_bytes([0xE9]).unwrap());

    // And the fully-matching longer-flagged multi-char case.
    assert_eq!(f2, PString::from_bytes([0xE9, b'a']).unwrap());
}

#[test]
fn eq_cross_flag_equal_length_ascii_can_match() {
    // Equal byte lengths must NOT be decided-false when the flagged side has no multi-byte sequence.
    let mut f = PString::from_bytes(b"ab").unwrap();
    f.set_utf8_for_test();
    assert_eq!(f, PString::from_bytes(b"ab").unwrap());
    assert_ne!(f, PString::from_bytes(b"ba").unwrap());
}

#[test]
fn eq_grid_both_flagged_terminal_mismatch() {
    // The flagged twin of the exclusivity row.
    let mut latin1 = PString::from_bytes([0xC3, 0xA9]).unwrap();
    latin1.set_utf8_for_test();
    let mut mal = PString::from_bytes([0xC0, 0x80]).unwrap();
    mal.set_utf8_for_test();
    assert_ne!(latin1, mal);
}

#[test]
fn eq_grid_valid_vs_invalid_same_flag() {
    // Flagged terminal Rust-invalid vs flagged known-Rust-valid (small tier, settled Utf8Latin1 at birth): valid bytes
    // never equal invalid bytes.
    let flagged_valid = PString::from_str(&"é".repeat(20)).unwrap();
    let mut ext = PString::from_bytes([0xF4, 0x90, 0x80, 0x80]).unwrap();
    ext.set_utf8_for_test();
    assert_ne!(flagged_valid, ext);
    assert_ne!(ext, flagged_valid);
}

#[test]
fn eq_grid_ascii_vs_non_ascii_both_orientations() {
    // Flagged-ASCII vs unflagged known-non-ASCII.
    let mut fa = PString::from_bytes(b"abc").unwrap();
    fa.set_utf8_for_test();
    assert_ne!(fa, PString::from_bytes([0x80, 0x81, 0x82]).unwrap());

    // Unflagged-ASCII vs flagged known-non-ASCII (Latin-1).
    let mut fl = PString::from_bytes([0xC3, 0xA9]).unwrap();
    fl.set_utf8_for_test();
    assert_ne!(PString::from_bytes(b"ab").unwrap(), fl);
}

#[test]
fn eq_grid_same_flag_terminal_mismatch() {
    // Differing terminals, both unflagged: decided without memcmp (exclusivity law).
    let latin1 = PString::from_bytes([0xC3, 0xA9]).unwrap(); // valid, Latin-1-range... as bytes: classified
    let malformed = PString::from_bytes([0xC0, 0x80]).unwrap();
    assert_ne!(latin1, malformed);
    let ascii = PString::from_bytes(b"ab").unwrap();
    assert_ne!(ascii, latin1);
}

#[test]
fn eq_grid_flagged_malformed_vs_unflagged_is_false() {
    let mut mal = PString::from_bytes([0x80]).unwrap();
    mal.set_utf8_for_test(); // flagged malformed
    let plain = PString::from_bytes([0x80]).unwrap();
    assert_ne!(mal, plain, "upgrade of unflagged is valid; never matches malformed bytes");
}

#[test]
fn eq_reverse_malformed_orientation_can_match() {
    // Unflagged MALFORMED-classified bytes are just bytes: \x80 as a character equals flagged C2 80.
    let plain = PString::from_bytes([0x80]).unwrap();
    assert_eq!(plain.inline_class(), Some(InlineClass::Bytes));
    let mut flagged = PString::from_bytes([0xC2, 0x80]).unwrap();
    flagged.set_utf8_for_test();
    assert_eq!(flagged, plain, "the grid must not shortcut this orientation");
}

#[test]
fn eq_grid_length_rows() {
    // plain longer than flagged: impossible.
    let mut flagged = PString::from_bytes([0xC3, 0xA9]).unwrap();
    flagged.set_utf8_for_test();
    let plain3 = PString::from_bytes([0xE9, 0xE9, 0xE9]).unwrap();
    assert_ne!(flagged, plain3);

    // flagged known Latin-1 (has a 2-byte char) with equal byte lengths: impossible.
    let plain2 = PString::from_bytes([0xC3, 0xA9]).unwrap();
    assert_ne!(flagged, plain2);
}

#[test]
fn streaming_compare_narrows_on_completed_walk() {
    // Heap flagged Utf8Latin1 (settled at birth) vs matching latin1 bytes: cross-flag content equality resolved by the
    // single walk.
    let flagged = PString::from_str(&"é".repeat(20)).unwrap();
    let plain = PString::from_bytes([0xE9u8; 20]).unwrap();
    assert_eq!(flagged, plain);

    // The completed walk narrowed the flagged side to Utf8Latin1: is_ascii is now a state read.
    assert!(!flagged.is_ascii());
    assert!(!plain.is_ascii());
}

#[test]
fn cheap_probe_defers_range() {
    let s = PString::from_str(&"é".repeat(20)).unwrap(); // small tier, settled Utf8Latin1 at birth
    assert!(!s.is_ascii()); // a state read: nothing left to defer on a settled birth

    // Equality resolves range on demand and still matches the downgraded form.
    let plain = PString::from_bytes([0xE9u8; 20]).unwrap();
    assert_eq!(s, plain);

    // And a wide heap string resolved through the same path fast-negatives.
    let wide = PString::from_str(&"字".repeat(14)).unwrap(); // 42 bytes heap
    assert!(!wide.is_ascii());
    let wide_plain = PString::from_bytes(wide.as_bytes(&mut [0u8; DECODE_MAX])).unwrap();
    assert_ne!(wide, wide_plain);
}

#[test]
fn eq_fast_negative_for_beyond_latin1() {
    // A flagged string containing U+0100+ equals no unflagged string, regardless of bytes.
    let wide = PString::from_str("abc字").unwrap();
    assert!(wide.is_utf8());
    let plain = PString::from_bytes(wide.as_bytes(&mut [0u8; DECODE_MAX])).unwrap();
    assert_ne!(wide, plain);

    // And the é (Latin-1) case still compares by character as before.
    let e_flagged = PString::from_str("é").unwrap();
    let e_latin1 = PString::from_bytes([0xE9]).unwrap();
    assert_eq!(e_flagged, e_latin1);
}

#[test]
fn append_range_join_semantics() {
    let mut s = PString::from_str("abc").unwrap(); // Ascii
    s.push_str("é").unwrap();
    assert_eq!(s.inline_class(), Some(InlineClass::Latin1));
    s.push_str("字").unwrap();
    assert_eq!(s.inline_class(), Some(InlineClass::NonLatin1));

    // This append carries the content past the inline payload, and non-ASCII bytes belong to no packed alphabet, so the
    // string lands on the heap — where the same join rule holds, read through the heap lattice.
    s.push_str("more ascii").unwrap();
    assert!(s.storage_type().is_heap());
    assert_eq!(s.scan_state(), scan::Utf8NonLatin1, "range cannot go back down on append");
}

#[test]
fn heap_append_range_join() {
    let mut s = PString::from_bytes(b"a".repeat(30)).unwrap();
    assert!(s.is_ascii()); // narrows heap state to ASCII
    s.push_str("é").unwrap(); // ASCII join Latin-1 = Latin-1, no rescan
    let latin1_equiv: Vec<u8> = b"a".repeat(30).iter().copied().chain([0xE9u8]).collect();
    let plain = PString::from_bytes(&latin1_equiv).unwrap();
    let mut flagged = s;
    flagged.set_utf8_for_test();
    assert_eq!(flagged, plain, "Latin-1-range heap string equals its downgraded form");
}

// ── Exhaustive grid verification (§2.3.5) ─────────────────────
/// Ground truth: pure character-sequence comparison with no grid and no state consultation.
fn reference_eq(a: &PString, b: &PString) -> bool {
    fn chars_of(s: &PString) -> Vec<u32> {
        if s.is_utf8() {
            flagged_chars(s.as_bytes(&mut [0u8; DECODE_MAX])).collect()
        } else {
            s.as_bytes(&mut [0u8; DECODE_MAX]).iter().map(|&b| b as u32).collect()
        }
    }

    chars_of(a) == chars_of(b)
}

/// The design's decided-false table (§2.3.5 rows 1–4), transcribed independently of the implementation.
fn design_decides_false(a: &PString, sa: scan::ScanState, b: &PString, sb: scan::ScanState) -> bool {
    if a.is_utf8() == b.is_utf8() {
        return a.len() != b.len()
            || (scan::is_terminal(sa) && scan::is_terminal(sb) && sa != sb)
            || (scan::is_terminal(sa) && !scan::is_rust_valid(sa) && scan::is_rust_valid(sb))
            || (scan::is_terminal(sb) && !scan::is_rust_valid(sb) && scan::is_rust_valid(sa))
            || (sa == scan::Ascii && scan::is_known_non_ascii(sb))
            || (sb == scan::Ascii && scan::is_known_non_ascii(sa));
    }

    let (f, p, sf, sp) = if a.is_utf8() { (a, b, sa, sb) } else { (b, a, sb, sa) };

    p.len() > f.len()
        || ((sf == scan::Utf8Latin1 || sf == scan::Utf8NonAscii) && p.len() == f.len())
        || (sf == scan::Ascii && scan::is_known_non_ascii(sp))
        || (sp == scan::Ascii && scan::is_known_non_ascii(sf))
        || scan::is_known_beyond_latin1(sf)
        || sf == scan::MalformedUtf8
}

/// A heap string in the `UNKNOWN` scan state.  Classification rides every copy (§2.2.3), so copying births are settled
/// at every size and `UNKNOWN` arises only from an in-place raw append on a large tier — which is exactly how this
/// helper manufactures it: the content is built with the final `pattern` repetition withheld and appended raw,
/// resetting the state per the blanket rule (§2.2.5) while leaving the bytes identical to a one-shot build.  Requires a
/// pattern with a non-ASCII byte, since a pure-ASCII suffix carries knowledge and would not reset.
fn lazy_heap(pattern: &[u8]) -> PString {
    assert!(pattern.iter().any(|b| !b.is_ascii()), "an ASCII suffix cannot reset the state");

    let mut bytes = Vec::with_capacity(LAZY_MIN + pattern.len());
    while bytes.len() < LAZY_MIN {
        bytes.extend_from_slice(pattern);
    }

    let split = bytes.len() - pattern.len();
    let mut s = PString::from_bytes(&bytes[..split]).unwrap();
    s.push_bytes(&bytes[split..]).unwrap();
    assert!(!s.storage_type().is_small_heap_tier(), "the lazy tiers begin above 64 KiB");
    assert_eq!(s.scan_state(), scan::Unknown, "the raw append is what makes the state indeterminate");

    s
}

/// One 16-byte quantum past `Heap16`'s ceiling, 16-aligned: the append-reset manufacture needs its base — the content
/// minus a withheld one- or two-byte pattern — on a large tier AND inside the birth headroom, so the reset append
/// extends in place instead of rebuilding (a rebuild classifies, defeating the manufacture).  The 16-quantum system
/// backend leaves exactly (16 - (header + len) % 16) % 16 bytes of headroom, which the alignment makes cover both
/// pattern widths; jemalloc's coarser classes leave more.  Kept minimal because several witnesses allocate one each and
/// the suite pays for every byte.
const LAZY_MIN: usize = 65_552;

/// Build every reachable (state, storage) witness configuration, with several byte contents behind the indeterminate
/// states.  Each witness's state is asserted at construction.  How many witnesses the grid has, building none of them.
fn grid_witness_count() -> usize {
    grid_witnesses_range(1, 0).1
}

/// The one witness at `index`, built fresh.
///
/// The grid needs a fresh witness per comparison — `eq` narrows scan states as a side effect, so a reused witness would
/// silently degrade indeterminate-state coverage into terminal-state coverage — but building all of them to keep one
/// made the cost quadratic in witness size, and the indeterminate states now live above 64 KiB where only the lazy
/// tiers still hold them (§2.2.3).
fn grid_witness(index: usize) -> (String, PString) {
    grid_witnesses_range(index, index).0.pop().expect("index within the witness set")
}

/// Build the witnesses whose indices fall in `lo..=hi`, and report how many exist.  A range that selects none still
/// walks the list, so the count is available without paying for a single allocation.
fn grid_witnesses_range(lo: usize, hi: usize) -> (Vec<(String, PString)>, usize) {
    let mut out: Vec<(String, PString)> = Vec::new();
    let mut seen = 0usize;

    // The witness is passed as a thunk, not a value, so asking for one does not build the other eighteen.  That
    // mattered little when every witness was a couple of dozen bytes; the indeterminate states now live above 64 KiB,
    // because only the lazy tiers still hold them (§2.2.3), and the grid asks for a fresh witness per comparison.
    let mut push = |name: &str, build: &dyn Fn() -> PString, want: scan::ScanState| {
        let index = seen;
        seen += 1;
        if index < lo || index > hi {
            return;
        }
        let s = build();
        assert_eq!(s.scan_state(), want, "witness {name} state");
        out.push((name.to_string(), s));
    };

    // Inline terminals.
    push("inl-ascii", &|| PString::from_bytes(b"ab").unwrap(), scan::Ascii);
    push("inl-latin1", &|| PString::from_bytes([0xC3, 0xA9]).unwrap(), scan::Utf8Latin1);
    push("inl-nonlatin1", &|| PString::from_str("字").unwrap(), scan::Utf8NonLatin1);
    push("inl-extended", &|| PString::from_bytes([0xF4, 0x90, 0x80, 0x80]).unwrap(), scan::ExtendedUtf8);
    push("inl-malformed", &|| PString::from_bytes([0x80]).unwrap(), scan::MalformedUtf8);

    // Heap terminals.  Below 64 KiB these are classified at construction, so no probe is needed to reach the state.
    push("heap-ascii", &|| PString::from_bytes(b"a".repeat(24)).unwrap(), scan::Ascii);
    push("heap-latin1", &|| PString::from_str(&"é".repeat(16)).unwrap(), scan::Utf8Latin1);
    push("heap-nonlatin1", &|| PString::from_str(&"字".repeat(8)).unwrap(), scan::Utf8NonLatin1);
    push(
        "heap-extended",
        &|| {
            let s = PString::from_bytes([0xF4, 0x90, 0x80, 0x80].repeat(6)).unwrap();
            assert!(s.is_perl_utf8_valid());
            s
        },
        scan::ExtendedUtf8,
    );
    push(
        "heap-malformed",
        &|| {
            let s = PString::from_bytes([0x80; 24]).unwrap();
            assert!(!s.is_perl_utf8_valid());
            s
        },
        scan::MalformedUtf8,
    );

    // Indeterminate states, which exist only above 64 KiB.  UNKNOWN over all-ASCII content is unreachable: copying
    // births classify (§2.2.3), and the one remaining door to UNKNOWN — a raw append on a large tier — requires a
    // non-ASCII byte in the suffix, since push_bytes classifies a pure-ASCII suffix as Valid and Valid preserves the
    // state.  The witness is retired, not skipped.
    push("heap-unknown-latin1", &|| lazy_heap(&[0xC3, 0xA9]), scan::Unknown);
    push("heap-unknown-malformed", &|| lazy_heap(&[0x81]), scan::Unknown);

    // ValidUtf8-family indeterminates (the valid-side probe memo included) are unreachable from today's constructor
    // inventory: every initializer copies and classification rides the copy (§2.2.3), so a known-valid birth is settled
    // at its exact range.  The states stay in the lattice as the zero-copy adoption forms' vocabulary — witnesses
    // return when a constructor without a copy exists.

    // The probe-narrowed states need a probe with something left to narrow, which again is the lazy tiers alone.
    let narrowed = |s: PString| {
        assert!(!s.is_ascii());
        s
    };

    push("heap-nonascii-raw", &|| narrowed(lazy_heap(&[0x82])), scan::NonAscii);
    push("heap-nonascii-valid-bytes", &|| narrowed(lazy_heap(&[0xC3, 0xA9])), scan::NonAscii);

    (out, seen)
}

#[test]
fn full_scan_runs_once_then_state_answers() {
    // A heap string's first as_str pays one validation pass (+ one classification); afterwards every question is a
    // state read — the never-scan-twice law, mechanically.
    let s = lazy_heap(&[0xC3, 0xA9]); // above 64 KiB, where the pass is still deferred
    eq_probe::reset();
    assert!(s.as_str(&mut [0u8; DECODE_MAX]).is_some());
    let (scans_first, _) = eq_probe::scans();
    assert_eq!(scans_first, 1, "first as_str must pay exactly ONE fused pass — more is double-scanning");
    eq_probe::reset();
    assert!(s.as_str(&mut [0u8; DECODE_MAX]).is_some());
    assert!(s.is_perl_utf8_valid());
    assert!(!s.is_ascii());
    assert_eq!(s.char_len(), Some(LAZY_MIN / 2), "two bytes per character across the whole buffer");
    assert_eq!(eq_probe::scans(), (0, 0), "cached state must answer every subsequent question");
}

#[test]
fn cheap_probe_bails_at_first_high_bit() {
    // The ninth state's raison d'être (§2.2.4): the ASCII probe examines O(first-high-bit) bytes.
    let mut bytes = vec![0x80u8];
    bytes.extend_from_slice(&b"a".repeat(LAZY_MIN));
    let mut s = PString::from_bytes(&bytes).unwrap(); // born settled: copying births classify (§2.2.3)
    s.push_bytes(&[0x80]).unwrap(); // the raw append is what resets to UNKNOWN
    eq_probe::reset();
    assert!(!s.is_ascii());
    let (_, probe_bytes) = eq_probe::scans();
    assert_eq!(probe_bytes, 1, "first byte is high: the probe must bail immediately");
    assert_eq!(s.scan_state(), scan::NonAscii);

    // The validity-known side has no probe to bail: a `&str` birth is settled at its exact range during the copy
    // (§2.2.3), so the ASCII question is a state read and the probe never runs at all.
    let f = PString::from_str(&format!("é{}", "a".repeat(LAZY_MIN))).unwrap();
    eq_probe::reset();
    assert!(!f.is_ascii());
    let (_, pb2) = eq_probe::scans();
    assert_eq!(pb2, 0, "born settled: no probe at all");
    assert_eq!(f.scan_state(), scan::Utf8Latin1);
}

#[test]
fn eq_short_circuits_at_first_mismatch_depth() {
    // The asymptotic property "short-circuit" names: characters consumed is O(mismatch position), not O(n).
    let big = 10_000;

    // Mismatch at position 0: flagged é-string vs plain starting with a different byte.
    let flagged = PString::from_str(&"é".repeat(big)).unwrap();
    let mut plain_bytes = vec![0xE9u8; big];
    plain_bytes[0] = 0xAA;
    let plain = PString::from_bytes(&plain_bytes).unwrap();
    eq_probe::reset();
    assert_ne!(flagged, plain);
    let (_, entries, chars) = eq_probe::snapshot();
    assert_eq!(entries, 1, "undecided pair must go to the walk");
    assert!(chars <= 2, "mismatch at position 0 must be found within the first characters, consumed {chars}");

    // Mismatch at position 100.
    let mut plain_bytes2 = vec![0xE9u8; big];
    plain_bytes2[100] = 0xAA;
    let plain2 = PString::from_bytes(&plain_bytes2).unwrap();
    let flagged2 = PString::from_str(&"é".repeat(big)).unwrap();
    eq_probe::reset();
    assert_ne!(flagged2, plain2);
    let (_, _, chars2) = eq_probe::snapshot();
    assert!((100..=102).contains(&chars2), "mismatch at 100 must consume ~101 characters, consumed {chars2}");

    // Full equality consumes everything exactly once.
    let flagged3 = PString::from_str(&"é".repeat(big)).unwrap();
    let plain3 = PString::from_bytes(vec![0xE9u8; big]).unwrap();
    eq_probe::reset();
    assert_eq!(flagged3, plain3);
    let (_, _, chars3) = eq_probe::snapshot();
    assert_eq!(chars3, big, "completed walk consumes each character exactly once");
}

#[test]
fn eq_grid_decided_pairs_perform_no_scan() {
    // Observable-state companion: a grid-decided comparison must leave an indeterminate operand's state untouched (no
    // scan happened on it).
    let wide = PString::from_str("字").unwrap(); // inline NL1, flagged
    assert!(wide.is_utf8()); // from_str of non-ASCII is flagged already
    let unknown = lazy_heap(&[0x90u8]); // only the lazy tiers still hold UNKNOWN
    assert_eq!(unknown.scan_state(), scan::Unknown);
    eq_probe::reset();
    assert_ne!(wide, unknown); // cross-flag, flagged NL1: grid row 4
    let (hits, entries, _) = eq_probe::snapshot();
    assert_eq!((hits, entries), (1, 0));
    assert_eq!(unknown.scan_state(), scan::Unknown, "decided comparison must not scan the other operand");
}

#[test]
fn eq_grid_exhaustive_over_all_state_flag_combinations() {
    // Every (witness × flag) against every (witness × flag).  Witnesses are constructed FRESH for every pair: eq
    // narrows scan states as a side effect and heap clones share buffer state, so reused witnesses would silently
    // degrade indeterminate-state coverage into terminal-state coverage.
    let n = grid_witness_count();
    let fresh = |i: usize, flagged: bool| -> (String, PString) {
        let (name, mut w) = grid_witness(i);
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
                    assert_eq!(got, want, "eq vs oracle for {na} vs {nb} (states {sa:?}/{sb:?})");

                    if design_decides_false(&a, sa, &b, sb) {
                        decided += 1;
                        assert!(!want, "design table unsound for {na} vs {nb} (states {sa:?}/{sb:?})");

                        // The mechanism assertion: a decided pair must be decided BY THE GRID — same-flag decided pairs
                        // may resolve in the pre-memcmp rows or memcmp's length check; cross-flag decided pairs must
                        // hit a grid row and must never enter the streaming walk.
                        if a.is_utf8() != b.is_utf8() {
                            assert!(grid_hits >= 1, "grid row failed to fire for {na} vs {nb} (states {sa:?}/{sb:?})");
                            assert_eq!(walk_entries, 0, "walk entered on decided pair {na} vs {nb} (states {sa:?}/{sb:?})");
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
    let f = PString::from_str(&src).unwrap();
    let mut twin = vec![b'a'; 63];
    twin.push(0xE9);
    twin.extend_from_slice(&b"b".repeat(200));
    let p = PString::from_bytes(&twin).unwrap();
    assert_eq!(f, p);

    // Long pure-ASCII cross-flag pair: decided entirely by gated memcmp spans, late mismatch caught.
    let mut fa = PString::from_bytes(b"a".repeat(9000)).unwrap();
    fa.set_utf8_for_test();
    let _ = fa.is_ascii(); // ASCII state on the flagged side would grid-decide vs known-non-ASCII only
    let mut good = b"a".repeat(9000);
    let eq_twin = PString::from_bytes(&good).unwrap();
    assert_eq!(fa, eq_twin);
    good[8999] = b'b';
    let ne_twin = PString::from_bytes(&good).unwrap();
    eq_probe::reset();
    assert_ne!(fa, ne_twin);
    let (_, _, consumed) = eq_probe::snapshot();
    assert!(consumed >= 8192, "the walk must have streamed the long equal prefix, consumed {consumed}");

    // Mismatch inside the FIRST ladder block of a long string: consumption bounded by one cache line.
    let flagged_long = PString::from_str(&"é".repeat(10_000)).unwrap();
    let bad = vec![0xAAu8; 10_000];
    let plain_bad = PString::from_bytes(&bad).unwrap();
    eq_probe::reset();
    assert_ne!(flagged_long, plain_bad);
    let (_, _, chars0) = eq_probe::snapshot();
    assert!(chars0 <= WALK_FIRST_BLOCK, "first-block mismatch must stay within the first walk block, consumed {chars0}");
}

// ── Dual-calculation hashing (§2.3.5) ─────────────────────────
fn digest_of(s: &PString) -> u64 {
    s.content_digest()
}

#[test]
fn hash_dual_calculation_is_single_fetch_and_keeps_knowledge() {
    // Unresolved flagged heap string: ONE fused pass computes both candidates, decides, and classifies.
    let s = PString::from_str(&"é".repeat(20)).unwrap(); // small tier, flagged, settled at birth
    eq_probe::reset();
    let d = digest_of(&s);
    assert_eq!(eq_probe::scans(), (1, 0), "dual calculation is one fetch, no probes");
    assert_eq!(s.scan_state(), scan::Utf8Latin1, "the pass's classification is kept");
    eq_probe::reset();
    assert_eq!(s.char_len(), Some(20), "and so is the character count");
    assert_eq!(eq_probe::scans(), (0, 0));

    // The downgraded digest matches the unflagged equal (the HashMap-key requirement).
    let plain = PString::from_bytes([0xE9u8; 20]).unwrap();
    assert_eq!(d, digest_of(&plain));

    // A repeat hash uses the known-Latin-1 single-emission path: still exactly one fetch.
    eq_probe::reset();
    assert_eq!(digest_of(&s), d);
    assert_eq!(eq_probe::scans().0, 1, "known-range emission is one pass, never two");
}

#[test]
fn hash_dual_calculation_wide_and_malformed_outcomes() {
    // Wide content: the raw candidate wins; byte-identical flagged strings agree.
    let a = PString::from_str(&"字".repeat(14)).unwrap(); // a small heap tier: classified at construction
    let b = PString::from_str(&"字".repeat(14)).unwrap();
    assert_eq!(a.scan_state(), scan::Utf8NonLatin1, "the construction pass settled the wide outcome");
    eq_probe::reset();
    let da = digest_of(&a);
    assert_eq!(eq_probe::scans().0, 0, "no pass left to pay");
    assert_eq!(da, digest_of(&b));

    // Malformed discovered mid-pass: raw digest, MALFORMED_UTF8 recorded, agrees with the known-malformed path on
    // byte-identical content.  Copying births are settled (§2.2.3), so the indeterminate starting point is manufactured
    // the one way it still arises: a raw append resetting a large tier to UNKNOWN — after which the malformation really
    // is discovered by the digest's own pass.
    let mut bad_bytes = vec![b'a'; LAZY_MIN];
    bad_bytes.push(0xC0);
    bad_bytes.push(0x80);

    let mut m1 = PString::from_bytes(&bad_bytes[..LAZY_MIN + 1]).unwrap();
    m1.push_bytes(&bad_bytes[LAZY_MIN + 1..]).unwrap(); // the raw 0x80 suffix resets to UNKNOWN
    m1.set_utf8_for_test();
    assert_eq!(m1.scan_state(), scan::Unknown);

    eq_probe::reset();
    let dm = digest_of(&m1);
    assert_eq!(eq_probe::scans().0, 1);
    assert_eq!(m1.scan_state(), scan::MalformedUtf8);

    let mut m2 = PString::from_bytes(&bad_bytes).unwrap();
    m2.set_utf8_for_test();
    assert!(!m2.is_perl_utf8_valid()); // known-malformed from its settled birth: takes the known-malformed digest path
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
    let f = PString::from_str(&flagged_src).unwrap(); // flagged, settled at birth

    let mut twin = vec![b'a'; CLASSIFY_BLOCK - 1];
    twin.push(0xE9);
    twin.extend_from_slice(b"tail");
    let p = PString::from_bytes(&twin).unwrap();

    assert_eq!(digest_of(&f), digest_of(&p));
    assert_eq!(f.scan_state(), scan::Utf8Latin1);
    assert_eq!(f.char_len(), Some(CLASSIFY_BLOCK - 1 + 1 + 4));
}

// ── Blocked hybrid classifier boundaries (§2.2.5) ─────────────
/// Test-only reference: the scalar single-byte-scan classifier, transcribed as the oracle for the blocked hybrid (same
/// decode rules, no blocking).
fn reference_classify(bytes: &[u8]) -> (scan::Terminal, usize) {
    let mut facts = ScanFacts::default();
    match scalar_decode_span(bytes, 0, bytes.len(), &mut facts, |_| {}) {
        Some(_) => (facts.state(), facts.chars),
        None => (scan::Terminal::MalformedUtf8, 0),
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

    let cases: [(&[u8], scan::ScanState); 5] = [
        ("é".as_bytes(), scan::Utf8Latin1),
        ("字".as_bytes(), scan::Utf8NonLatin1),
        ("\u{10000}".as_bytes(), scan::Utf8NonLatin1),
        (&fe_min, scan::ExtendedUtf8),
        (&ff_min, scan::ExtendedUtf8),
    ];

    for (seq, want_state) in cases {
        for cut in 1..seq.len() {
            // The sequence begins `cut` bytes before the boundary, so the boundary falls inside it.
            let lead_len = CLASSIFY_BLOCK - cut;
            let mut bytes = vec![b'a'; lead_len];
            bytes.extend_from_slice(seq);
            bytes.extend_from_slice(b"tail");
            let (st, chars) = classify_full(&bytes);
            assert_eq!(st.widen(), want_state, "state for seq len {} cut {}", seq.len(), cut);
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
    assert_eq!(st, scan::Terminal::Utf8NonLatin1);

    // chars: (BLOCK-1) a's + 字 + b-fill + é + 4 tail.
    let b_fill = (2 * CLASSIFY_BLOCK - 1) - (CLASSIFY_BLOCK - 1 + 3);
    assert_eq!(chars, (CLASSIFY_BLOCK - 1) + 1 + b_fill + 1 + 4);
}

#[test]
fn block_boundary_truncation_and_malformation() {
    // Lead byte as the final byte of the slice, exactly at the boundary: truncated.
    let mut t = vec![b'a'; CLASSIFY_BLOCK - 1];
    t.push(0xC3);
    assert_eq!(classify_full(&t), (scan::Terminal::MalformedUtf8, 0));

    // Bad continuation lands in the next block: malformed.
    let mut m = vec![b'a'; CLASSIFY_BLOCK - 1];
    m.extend_from_slice(&[0xC3, 0x28]);
    assert_eq!(classify_full(&m), (scan::Terminal::MalformedUtf8, 0));
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
    assert_eq!(st, scan::ValidRange::Latin1);
    assert_eq!(chars, CLASSIFY_BLOCK - 1 + 1 + 4);

    // A wide character first appearing blocks later still bails (block-granular, count forfeited).
    let mut w = String::with_capacity(2 * CLASSIFY_BLOCK + 8);
    for _ in 0..2 * CLASSIFY_BLOCK {
        w.push('a');
    }

    w.push('字');
    assert_eq!(classify_known_valid(w.as_bytes()), (scan::ValidRange::NonLatin1, 0));

    // Multi-block pure Latin-1: exact count.
    let l = "é".repeat(CLASSIFY_BLOCK); // 2 bytes each: two blocks
    assert_eq!(classify_known_valid(l.as_bytes()), (scan::ValidRange::Latin1, CLASSIFY_BLOCK));
}

// ── Character-length cache (§2.2.4) ───────────────────────────
#[test]
fn char_len_semantics_and_caching() {
    // ASCII: chars == bytes, no scan at all when state is known.
    let a = PString::from_bytes(b"ab".repeat(15)).unwrap();
    assert!(a.is_ascii());
    eq_probe::reset();
    assert_eq!(a.char_len(), Some(30));
    assert_eq!(eq_probe::scans().0, 0, "ASCII char_len is a length read");

    // Latin-1 inline: the transcoded units are the flagged-side characters, so the count is the stored nibble — no scan
    // at all, where the raw-byte tier paid a recount.
    let li = PString::from_bytes([0xC3, 0xA9].repeat(12)).unwrap();
    assert!(li.storage_type().is_inline(), "24 compressible bytes live inline now (§2.2.9)");
    eq_probe::reset();
    assert_eq!(li.char_len(), Some(12));
    assert_eq!(eq_probe::scans().0, 0, "the count is the stored nibble: no pass at all");

    // Latin-1 in a small heap tier: classified at construction (§2.2.3), so every read is a state read — the stronger
    // property the eager tiers buy, where the lazy tiers pay one fused pass at first read instead.
    let l = PString::from_bytes([0xC3, 0xA9].repeat(16)).unwrap();
    assert!(l.storage_type().is_small_heap_tier());
    eq_probe::reset();
    assert_eq!(l.char_len(), Some(16));
    assert!(l.as_str(&mut [0u8; DECODE_MAX]).is_some());
    assert_eq!(eq_probe::scans().0, 0, "the construction pass already answered both questions");

    // Above 64 KiB the pass is deferred, and then paid exactly once.
    let big = lazy_heap(&[0xC3, 0xA9]);
    eq_probe::reset();
    assert_eq!(big.char_len(), Some(LAZY_MIN / 2));
    assert_eq!(eq_probe::scans().0, 1, "exactly one fused pass classifies and counts");
    eq_probe::reset();
    assert_eq!(big.char_len(), Some(LAZY_MIN / 2));
    assert_eq!(eq_probe::scans().0, 0, "count and state both cached from the one pass");

    // Extended: counted (a 4-byte and a 13-byte character are one character each).
    let e = PString::from_bytes([0xF4, 0x90, 0x80, 0x80].repeat(6)).unwrap();
    assert_eq!(e.char_len(), Some(6));

    // Surrogates count one character per encoded sequence; perl never merges pairs.  Container-verified:
    // length(chr 0xD800) == 1; a CESU-style pair decodes to TWO characters (D800, DC00), length 2, distinct from the
    // one-character astral U+10000.
    let lone = PString::from_bytes([0xED, 0xA0, 0x80]).unwrap();
    assert_eq!(lone.inline_class(), Some(InlineClass::Extended));
    assert_eq!(lone.char_len(), Some(1));
    let cesu_pair = PString::from_bytes([0xED, 0xA0, 0x80, 0xED, 0xB0, 0x80]).unwrap();
    assert_eq!(cesu_pair.char_len(), Some(2), "pairs are two characters, never merged");
    let astral = PString::from_str("\u{10000}").unwrap();
    assert_eq!(astral.char_len(), Some(1));

    // Malformed: None (ops layer owns perl's warning behavior).
    let m = PString::from_bytes([0x80; 24]).unwrap();
    assert_eq!(m.char_len(), None);

    // Inline recount, all classes.
    assert_eq!(PString::from_str("héllo").unwrap().char_len(), Some(5));
    assert_eq!(PString::from_str("字").unwrap().char_len(), Some(1));
    assert_eq!(PString::from_bytes([0x80]).unwrap().char_len(), None);
}

#[test]
fn char_len_maintained_through_append() {
    let mut s = PString::from_bytes([0xC3, 0xA9].repeat(12)).unwrap(); // heap
    assert_eq!(s.char_len(), Some(12)); // classify + count: one pass
    eq_probe::reset();
    s.push_str("abc").unwrap(); // classification of the ADDED bytes only
    assert_eq!(s.char_len(), Some(15), "count maintained incrementally");
    let (full, _) = eq_probe::scans();
    assert_eq!(full, 1, "only the appended content was scanned (its own classification pass)");
}

#[test]
fn char_count_shared_across_cow_sharers() {
    let a = PString::from_bytes([0xC3, 0xA9].repeat(12)).unwrap();
    let b = a.clone(); // shares the buffer
    assert_eq!(a.char_len(), Some(12)); // pays the pass
    eq_probe::reset();
    assert_eq!(b.char_len(), Some(12));
    assert_eq!(eq_probe::scans().0, 0, "sharer reads the cached count");
}

// ── COW behavior through the string layer ─────────────────────
#[test]
fn large_immortal_forms_share_a_leaked_header_and_read_zero_copy() {
    // Past the compact ceiling: one leaked header, zero tier allocations, facts settled at birth.  The image here is
    // itself leaked — the test standing in for a program's large embedded literal.
    let big: &'static [u8] = Box::leak(b"z".repeat(U24_TEST_CEILING + 3).into_boxed_slice());
    let live = cow_buffer::live::count();
    let s = PString::from_static_bytes(big).unwrap();
    assert_eq!(s.storage_type(), StorageType::LargeStatic);
    assert_eq!(s.scan_state(), scan::Ascii);
    assert_eq!(s.len(), big.len());
    assert_eq!(s.char_len(), Some(big.len()));
    assert!(s.is_ascii() && !s.is_shared());

    // Bitwise clones share the header; teardown touches neither header nor image, and the tier counter never moves.
    let c = s.clone();
    assert_eq!(c.len(), s.len());

    // A flag flip points a fresh envelope at the same shared header: still large, still zero-copy.
    let mut f = c.clone();
    f.set_utf8_for_test();
    assert_eq!(f.storage_type(), StorageType::LargeStatic);
    assert!(f.is_utf8());
    drop(f);
    drop(c);
    drop(s);
    assert_eq!(cow_buffer::live::count(), live, "no tier allocation anywhere in the large form's life");

    // The unsafe immortal door at large size, and copy-out on write: the append lands in a tier, the image and its
    // other handles untouched.
    // SAFETY: leaked memory outlives every handle and nothing writes it.
    let im = unsafe { PString::from_immortal_bytes(big) }.unwrap();
    assert_eq!(im.storage_type(), StorageType::LargeImmortal);
    let mut w = im.clone();
    w.push_str("!").unwrap();
    assert_eq!(w.len(), big.len() + 1);
    assert!(w.storage_type().is_heap(), "the write landed in a tier");
    assert_eq!(im.len(), big.len(), "the image and its other handles are untouched");
}

/// One past the compact ceiling, kept as a named constant so the test reads as what it is.
const U24_TEST_CEILING: usize = 0xFF_FFFF;

#[test]
fn ascii_twins_are_the_class_specific_selection_with_derived_facts() {
    // Both tiers route settled-Ascii births to their twin, whose omitted count and scan are derived: every byte a
    // character, the class in the variant.
    let small = PString::from_bytes(b"x".repeat(40)).unwrap();
    assert_eq!(small.storage_type(), StorageType::Heap8Ascii);
    assert_eq!(small.scan_state(), scan::Ascii);
    assert!(small.is_ascii());
    assert_eq!(small.char_len(), Some(40));

    let mid = PString::from_bytes(b"y".repeat(300)).unwrap();
    assert_eq!(mid.storage_type(), StorageType::Heap16Ascii);
    assert_eq!(mid.char_len(), Some(300));

    // Non-Ascii small heap content stays on the plain variant.
    let plain = PString::from_bytes([0xE9u8; 40]).unwrap();
    assert_eq!(plain.storage_type(), StorageType::Heap8);

    // A twin and a plain tier holding the same bytes are equal: the twin is a selection, not a meaning.
    let as_heap16 = PString::from_bytes(b"x".repeat(40)).unwrap();
    assert_eq!(small, as_heap16);
}

#[test]
fn ascii_twin_appends_stay_in_place_only_while_ascii_holds() {
    // An Ascii append extends the twin in place: same variant, no reclassification, count still derived.
    let mut s = PString::from_bytes(b"a".repeat(40)).unwrap();
    assert_eq!(s.storage_type(), StorageType::Heap8Ascii);
    s.push_str("bcd").unwrap();
    assert_eq!(s.storage_type(), StorageType::Heap8Ascii);
    assert_eq!(s.len(), 43);
    assert_eq!(s.char_len(), Some(43));

    // A non-Ascii append cannot ride the implied class: the fast path bails and the rebuild re-dispatches the
    // variant by the joined content, settled per classify-on-copy.
    let mut t = PString::from_bytes(b"a".repeat(40)).unwrap();
    t.push_str("é").unwrap();
    assert_eq!(t.storage_type(), StorageType::Heap8);
    assert_eq!(t.scan_state(), scan::Utf8Latin1);
    assert_eq!(t.char_len(), Some(41));

    // COW discipline holds on the twins: a shared twin's append copies, the sharer untouched.
    let a = PString::from_bytes(b"z".repeat(40)).unwrap();
    let mut b = a.clone();
    assert!(a.is_shared());
    b.push_str("!").unwrap();
    assert_eq!(a.len(), 40);
    assert_eq!(b.len(), 41);
    assert!(!a.is_shared());
}

#[test]
fn the_meet_is_the_fact_union_canonicalized() {
    use scan::meet;
    let all = || (0..=11u8).map(scan::ScanState::from_u8);

    // Idempotent everywhere; Unknown the identity everywhere; commutative over the compatible pairs below.
    for s in all() {
        assert_eq!(meet(s, s), s, "{s:?} idempotent");
        assert_eq!(meet(s, scan::Unknown), s, "{s:?} vs Unknown");
        assert_eq!(meet(scan::Unknown, s), s, "Unknown vs {s:?}");
    }

    // The ruled example, and its family: a probe's byte witness completes a Maybe state whose validity is already
    // certified — the derivation RUST_VALID and HIGH_BIT yields the U+0080 witness.
    assert_eq!(meet(scan::NonAscii, scan::MaybeUtf8Latin1), scan::Utf8Latin1);
    assert_eq!(meet(scan::MaybeUtf8Latin1, scan::NonAscii), scan::Utf8Latin1);
    assert_eq!(meet(scan::NonAscii, scan::ValidUtf8), scan::Utf8NonAscii);
    assert_eq!(meet(scan::NonAscii, scan::Utf8NonAscii), scan::Utf8NonAscii);
    assert_eq!(meet(scan::NonAscii, scan::Utf8NonLatin1), scan::Utf8NonLatin1);
    assert_eq!(meet(scan::NonAscii, scan::ExtendedUtf8), scan::ExtendedUtf8);

    // The union that once forfeited: perl-decodability beside a byte witness now has its own state, so the meet is
    // total — no combination of true certifications loses a fact.
    assert_eq!(meet(scan::NonAscii, scan::MaybeExtendedUtf8), scan::PerlValidNonAscii);
    assert_eq!(meet(scan::PerlValidNonAscii, scan::MaybeExtendedUtf8), scan::PerlValidNonAscii);
    assert_eq!(meet(scan::PerlValidNonAscii, scan::ValidUtf8), scan::Utf8NonAscii);
    assert_eq!(meet(scan::PerlValidNonAscii, scan::ExtendedUtf8), scan::ExtendedUtf8);

    // Terminals absorb their compatible weaker facts; witness states refine the bare-validity states.
    assert_eq!(meet(scan::ValidUtf8, scan::MaybeUtf8Latin1), scan::MaybeUtf8Latin1);
    assert_eq!(meet(scan::Utf8NonAscii, scan::MaybeUtf8Latin1), scan::Utf8Latin1);
    assert_eq!(meet(scan::ValidUtf8, scan::MaybeExtendedUtf8), scan::ValidUtf8);
    assert_eq!(meet(scan::Utf8NonLatin1, scan::ValidUtf8), scan::Utf8NonLatin1);
    assert_eq!(meet(scan::Ascii, scan::MaybeUtf8Latin1), scan::Ascii);

    // Monotonic and *total*: over every compatible pair, meeting the result with either input is the result again
    // (absorption).  Absorption is the totality proof — had canonicalization dropped a fact, re-meeting with the input
    // carrying it would re-derive it and land elsewhere — so this list passing is the mechanical demonstration that the
    // twelve-state lattice is meet-closed and needs no thirteenth state.
    let compatible: &[(scan::ScanState, scan::ScanState)] = &[
        (scan::NonAscii, scan::MaybeUtf8Latin1),
        (scan::NonAscii, scan::MaybeExtendedUtf8),
        (scan::PerlValidNonAscii, scan::MaybeExtendedUtf8),
        (scan::PerlValidNonAscii, scan::ValidUtf8),
        (scan::PerlValidNonAscii, scan::ExtendedUtf8),
        (scan::NonAscii, scan::ValidUtf8),
        (scan::NonAscii, scan::Utf8NonAscii),
        (scan::NonAscii, scan::Utf8NonLatin1),
        (scan::NonAscii, scan::ExtendedUtf8),
        (scan::ValidUtf8, scan::MaybeUtf8Latin1),
        (scan::Utf8NonAscii, scan::MaybeUtf8Latin1),
        (scan::ValidUtf8, scan::MaybeExtendedUtf8),
        (scan::Utf8NonLatin1, scan::ValidUtf8),
        (scan::Ascii, scan::MaybeUtf8Latin1),
        (scan::Utf8Latin1, scan::MaybeUtf8Latin1),
        (scan::ExtendedUtf8, scan::MaybeExtendedUtf8),
        (scan::PerlValidNonAscii, scan::NonAscii),
        (scan::PerlValidNonAscii, scan::MaybeUtf8Latin1),
        (scan::PerlValidNonAscii, scan::Utf8Latin1),
        (scan::PerlValidNonAscii, scan::Utf8NonLatin1),
        (scan::PerlValidNonAscii, scan::Utf8NonAscii),
        (scan::MaybeExtendedUtf8, scan::MaybeUtf8Latin1),
        (scan::MaybeExtendedUtf8, scan::Utf8NonLatin1),
        (scan::NonAscii, scan::Utf8Latin1),
    ];
    for &(a, b) in compatible {
        let m = meet(a, b);
        assert_eq!(meet(a, b), meet(b, a), "{a:?}/{b:?} commutative");
        assert_eq!(meet(m, a), m, "{a:?}/{b:?} absorbs a");
        assert_eq!(meet(m, b), m, "{a:?}/{b:?} absorbs b");
    }
}

#[test]
fn racing_narrows_lose_no_information() {
    use std::sync::Arc;

    // A shared large-tier string of Latin-1 content: one honest probe truth is NonAscii, one honest classification
    // truth is Utf8Latin1, and under the CAS meet the finer fact must survive any interleaving.
    let mut content = b"pr\xC3\xA9cis ".repeat(10_000);
    content.truncate(70_000);
    while content.last().is_some_and(|b| b & 0xC0 == 0x80) || content.last() == Some(&0xC3) {
        content.pop();
    }
    let s = Arc::new(PString::from_bytes(&content).unwrap());
    assert_eq!(s.storage_type(), StorageType::Heap32, "large tier: the lazy, shared-header regime");

    let narrow = |s: &PString, st: scan::ScanState| match s.raw_parts() {
        RawParts::Heap(view) => view.narrow_scan(st),
        _ => panic!("large tier expected"),
    };

    // Reset to Unknown through the exclusive door each round, then race mixed-precision narrows.
    for _ in 0..200 {
        match s.raw_parts() {
            RawParts::Heap(view) => view.set_scan_for_test(scan::Unknown),
            _ => unreachable!(),
        }
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let s = Arc::clone(&s);
                std::thread::spawn(move || {
                    if i % 2 == 0 {
                        narrow(&s, scan::NonAscii);
                    } else {
                        narrow(&s, scan::Utf8Latin1);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(s.scan_state(), scan::Utf8Latin1, "the finer certification survived the race");
    }
}

#[test]
fn static_strings_are_zero_allocation_with_facts_settled_at_birth() {
    const TEXT: &str = "héllo, wörld: a static string past the inline ceiling";
    let live = cow_buffer::live::count();
    let s = PString::from_static_str(TEXT).unwrap();
    assert_eq!(s.storage_type(), StorageType::Static);
    assert_eq!(s.scan_state(), scan::Utf8Latin1, "terminal at construction");
    assert_eq!(s.char_len(), Some(TEXT.chars().count()), "the count is settled, not deferred");
    assert!(s.as_str(&mut [0u8; DECODE_MAX]).is_some());
    assert!(!s.is_ascii() && !s.is_shared());

    // Clones are bitwise and teardown touches nothing: across construction, clone, and drop, not one tier allocation
    // moves.
    let c = s.clone();
    assert_eq!(c, s);
    drop(c);
    drop(s);
    assert_eq!(cow_buffer::live::count(), live, "zero allocations, zero releases");

    // Malformed static bytes are a legitimate image: terminal, count-free, honestly invalid to both readers.
    let m = PString::from_static_bytes(b"a static image with a broken tail \xC0\x80").unwrap();
    assert_eq!(m.scan_state(), scan::MalformedUtf8);
    assert_eq!(m.char_len(), None);
    assert!(m.as_str(&mut [0u8; DECODE_MAX]).is_none());
    assert!(!m.is_perl_utf8_valid());
}

#[test]
fn immortal_and_static_writes_copy_out_and_flags_ride_the_image() {
    // Writing to an immortal form copies out to a mutable tier: the image is untouched and other handles still read it.
    let s = PString::from_static_str("the quick brown fox jumps over the lazy dog").unwrap();
    let mut w = s.clone();
    w.push_str(" again").unwrap();
    assert!(matches!(w.storage_type(), StorageType::Heap8Ascii), "the write landed in a tier, in the Ascii twin");
    assert_eq!(w.len(), s.len() + 6);
    assert_eq!(s.storage_type(), StorageType::Static, "the image and its other handles are untouched");
    assert_eq!(s.char_len(), Some(43));

    // A flag flip rebuilds the envelope and stays in the form: the image never moves for a tag change.
    let mut f = PString::from_static_str("étale cohomology, statically").unwrap();
    f.set_utf8_for_test();
    assert_eq!(f.storage_type(), StorageType::Static);
    assert!(f.is_utf8());

    // The unsafe immortal door, exercised through a leaked buffer standing in for the slab (§2.2.3).
    let live = cow_buffer::live::count();
    let image: &'static [u8] = Box::leak(b"immortal by leak: the slab arrives later".to_vec().into_boxed_slice());

    // SAFETY: leaked memory outlives every handle and nothing writes it.
    let im = unsafe { PString::from_immortal_bytes(image) }.unwrap();
    assert_eq!(im.storage_type(), StorageType::Immortal);
    assert_eq!(im.scan_state(), scan::Ascii);
    assert!(im.is_ascii());

    let c = im.clone();
    drop(im);
    drop(c);
    assert_eq!(cow_buffer::live::count(), live, "immortal handles never touch the tier allocator");

    // Content equality is representation-blind: a static, an immortal, and a tier holding the same bytes are equal.
    let st = PString::from_static_str("the same forty-two bytes in every form!!!!").unwrap();
    let heap = PString::from_bytes(b"the same forty-two bytes in every form!!!!").unwrap();
    assert_eq!(st, heap);
}

#[test]
fn unshare_is_a_no_op_on_unique_and_envelope_storage() {
    // Inline: the envelope owns the bytes; nothing is shared, nothing moves.
    let mut inline = PString::new("hi").unwrap();
    assert!(!inline.is_shared());
    inline.unshare().unwrap();
    assert_eq!(inline.as_bytes(&mut [0u8; DECODE_MAX]), b"hi");

    // A uniquely-held heap buffer stays where it is: no allocation churn at all.
    let mut unique = PString::from_bytes([0xE9u8; 40]).unwrap();
    assert!(!unique.is_shared());
    let live = cow_buffer::live::count();
    unique.unshare().unwrap();
    assert_eq!(cow_buffer::live::count(), live, "unique storage is untouched");
    assert_eq!(unique.as_bytes(&mut [0u8; DECODE_MAX]), &[0xE9u8; 40][..]);
}

#[test]
fn unshare_copies_out_of_shared_storage_and_preserves_meaning() {
    let a = PString::from_bytes([0xE9u8; 40]).unwrap();
    let mut b = a.clone();
    assert!(a.is_shared() && b.is_shared());

    let live = cow_buffer::live::count();
    b.unshare().unwrap();
    assert_eq!(cow_buffer::live::count(), live + 1, "the copy is a fresh allocation; the sharer keeps the old one");
    assert!(!a.is_shared() && !b.is_shared());
    assert_eq!(a, b, "content is meaning, and it did not change");

    // The flags ride along: unsharing changes where the bytes live, never what the value means.
    let mut f = PString::from_str(&"é".repeat(20)).unwrap(); // heap-tier, so the clone shares
    f.set_utf8_for_test();
    let mut f2 = f.clone();
    f2.unshare().unwrap();
    assert!(f2.is_utf8(), "the Perl utf8 flag survives the move");
    assert!(!f2.is_tainted());
}

#[test]
fn unshare_settles_an_indeterminate_shared_buffer() {
    // Classification rides the copy, so unsharing an UNKNOWN buffer is also the moment it becomes settled — while
    // the sharer, untouched, still holds the indeterminate state in the old allocation.
    let mut u = lazy_heap(&[0xC3, 0xA9]);
    let keep = u.clone();
    u.unshare().unwrap();
    assert_eq!(u.scan_state(), scan::Utf8Latin1, "the copy was already walking every byte");
    assert_eq!(keep.scan_state(), scan::Unknown, "the sharer is untouched");
    assert_eq!(u, keep);
}

#[test]
fn clone_shares_heap_buffer_and_append_cow_breaks() {
    let a = PString::from_str(&"base".repeat(10)).unwrap(); // heap
    let mut b = a.clone();
    b.push_str("+more").unwrap();
    assert_eq!(a.len(), 40);
    assert_eq!(b.len(), 45);
    assert!(a.as_str(&mut [0u8; DECODE_MAX]).is_some());
}

impl PString {
    /// Test-only: force the utf8 flag on (simulating `Encode::_utf8_on` / upgrade provenance).
    pub(crate) fn set_utf8_for_test(&mut self) {
        self.rebuild_tag(|_u, t| (true, t));
    }
}

// ── The non-allocating constructors ───────────────────────────────

#[test]
fn inline_accepts_up_to_the_capacity_and_refuses_beyond() {
    assert!(PString::inline("a".repeat(INLINE_MAX)).is_some());
    assert_eq!(PString::inline("a".repeat(INLINE_MAX + 1)), None);
    assert!(PString::inline_bytes(vec![0xFFu8; INLINE_MAX]).is_some());
    assert_eq!(PString::inline_bytes(vec![0xFFu8; INLINE_MAX + 1]), None);
}

#[test]
fn inline_agrees_with_the_fallible_constructors() {
    // Same content, same result: the fallible paths delegate here, so the representations must match exactly.
    for text in ["", "hello", "héllo", "0", "a longer ascii string"] {
        if let Some(inline) = PString::inline(text) {
            assert_eq!(inline, text.parse::<PString>().unwrap(), "{text:?}");
        }
    }

    for bytes in [&b""[..], b"hello", b"\xFF\xFE", b"\xC3\xA9"] {
        if let Some(inline) = PString::inline_bytes(bytes) {
            assert_eq!(inline, PString::from_bytes(bytes).unwrap(), "{bytes:?}");
        }
    }
}

#[test]
fn inline_flags_follow_the_source_type() {
    // From &str: ASCII unflagged (canonical downgraded form), non-ASCII flagged.
    assert!(!PString::inline("hello").unwrap().is_utf8());
    assert!(PString::inline("héllo").unwrap().is_utf8());

    // From bytes: never flagged, even when the content happens to be valid UTF-8.
    assert!(!PString::inline_bytes(b"h\xC3\xA9llo").unwrap().is_utf8());
}

#[test]
fn inline_composes_with_unwrap_or_default() {
    // The discard-the-detail path: callers who merely prefer inline storage need one combinator.
    assert_eq!(PString::inline("hi").unwrap_or_default().as_bytes(&mut [0u8; DECODE_MAX]), b"hi");
    assert_eq!(PString::inline("a".repeat(INLINE_MAX + 1)).unwrap_or_default(), PString::empty());
}

#[test]
fn inline_accepts_every_asref_shape() {
    let owned = String::from("owned");
    assert!(PString::inline(&owned).is_some());
    assert!(PString::inline(owned.clone()).is_some());
    assert!(PString::inline(owned.as_str()).is_some());

    let bytes = vec![1u8, 2, 3];
    assert!(PString::inline_bytes(&bytes).is_some());
    assert!(PString::inline_bytes(bytes.clone()).is_some());
    assert!(PString::inline_bytes(&bytes[..]).is_some());
}

// ── Formatting into the string ────────────────────────────────────

#[test]
fn write_macro_appends_through_fmt_write() {
    use std::fmt::Write;
    let mut s = PString::empty();
    write!(s, "{}-tail", 42).unwrap();
    write!(s, " {:.2}", 1.5).unwrap();
    assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), b"42-tail 1.50");
}

#[test]
fn push_fmt_reports_allocation_precisely() {
    // The trait impl flattens failure into fmt::Error, which carries nothing; push_fmt keeps the real error.
    let mut s = PString::empty();
    s.push_fmt(format_args!("{}", 12345)).unwrap();
    s.push_fmt(format_args!("{:>8}", "x")).unwrap();
    assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), b"12345       x");
}

#[test]
fn formatting_into_a_string_grows_it_across_tiers() {
    use std::fmt::Write;

    // Crossing the inline capacity mid-format must promote and keep every byte.
    let mut s = PString::empty();
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
    let s: PString = "42abc".parse().unwrap();
    assert_eq!(s.to_int(), 42, "leading numeric prefix");
    assert!(s.to_bool());
    assert!(s.would_warn(), "a trailing non-numeric tail warns");

    let f: PString = "3.75".parse().unwrap();
    assert_eq!(f.to_float(), 3.75);
    assert_eq!(f.to_int(), 3, "truncating toward zero");
    assert!(!f.would_warn());

    // Perl truthiness: only "" and "0" are false, so "0.0" and "00" are true.
    for (text, truth) in [("", false), ("0", false), ("0.0", true), ("00", true), (" ", true), ("0E0", true)] {
        let v: PString = text.parse().unwrap();
        assert_eq!(v.to_bool(), truth, "truthiness of {text:?}");
    }
}

#[test]
fn interpretation_agrees_across_storage_forms() {
    // The same content held inline and on the heap must answer identically — the property that lets storage forms
    // multiply without consumers noticing.
    let short: PString = "17".parse().unwrap();
    let padded: PString = "17                                        ".parse().unwrap();
    assert_ne!(short.storage_type(), padded.storage_type(), "the two must actually differ in storage");
    assert_eq!(short.to_int(), 17);
    assert_eq!(padded.to_int(), 17, "trailing space does not change the numeric prefix");
    assert!(short.to_bool() && padded.to_bool());
}

#[test]
fn debug_shows_the_representation_with_readable_content() {
    let packed: PString = "2026-07-28T14:33:07Z".parse().unwrap();
    let shown = format!("{packed:?}");
    assert!(shown.contains("storage: Packed"), "the tier is the first thing a developer wants: {shown}");
    assert!(shown.contains(r#"string: "2026-07-28T14:33:07Z""#), "the content, losslessly rendered: {shown}");

    // Bytes that are not text render escaped rather than lossily, since a perl string's content need not be UTF-8.
    let raw = PString::from_bytes([0xFF, 0xFE, b'h', b'i']).unwrap();
    assert!(format!("{raw:?}").contains(r#"string: b"\x{ff}\x{fe}hi""#));

    // The usual escapes, so a newline does not break the line.
    let escaped: PString = "a\tb\nc".parse().unwrap();
    assert!(format!("{escaped:?}").contains(r#"string: "a\tb\nc""#));
}

#[test]
fn the_constructors_accept_every_asref_shape() {
    // Generic at the boundary: an embedder holding a String, a Cow, or a compact string type from the ecosystem needs
    // no conversion, and the ladder beneath is monomorphic.
    let owned = String::from("owned content");
    assert_eq!(PString::new(&owned).unwrap().len(), 13);
    assert_eq!(PString::new(owned.clone()).unwrap().len(), 13);
    assert_eq!(PString::new(owned.as_str()).unwrap().len(), 13);
    assert_eq!(PString::new(std::borrow::Cow::Borrowed("borrowed")).unwrap().len(), 8);

    let bytes = vec![1u8, 2, 3];
    assert_eq!(PString::from_bytes(&bytes).unwrap().len(), 3);
    assert_eq!(PString::from_bytes(bytes.clone()).unwrap().len(), 3);
    assert_eq!(PString::from_bytes(&bytes[..]).unwrap().len(), 3);
    assert_eq!(PString::from_bytes([7u8; 4]).unwrap().len(), 4);

    // FromStr forwards to new, so parse() and new() agree exactly.
    assert_eq!(PString::new("2026-07-28T14:33:07Z").unwrap(), "2026-07-28T14:33:07Z".parse().unwrap());
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
        let mut built: PString = head.parse().unwrap();
        assert!(built.storage_type().is_packed(), "{head} should start packed");
        built.push_str(tail).unwrap();

        let whole: PString = format!("{head}{tail}").parse().unwrap();
        assert_eq!(built.storage_type(), whole.storage_type(), "{head}+{tail}: same tier");
        assert_eq!(built.as_bytes(&mut [0u8; DECODE_MAX]), whole.as_bytes(&mut [0u8; DECODE_MAX]), "{head}+{tail}: same content");
        assert_eq!(built, whole, "{head}+{tail}: equal strings");
    }
}

#[test]
fn appending_leaves_the_tier_when_it_must() {
    // A character in no alphabet, and content past the capacity: both go to the heap, carrying their bytes intact.
    let mut lettered: PString = "2026-07-28T14:33".parse().unwrap();
    lettered.push_str("x").unwrap();
    assert!(lettered.storage_type().is_heap());
    assert_eq!(lettered.as_bytes(&mut [0u8; DECODE_MAX]), b"2026-07-28T14:33x");

    let mut overflowing: PString = "123456789012345678901234567890".parse().unwrap();
    assert!(overflowing.storage_type().is_packed(), "thirty characters is the capacity");
    overflowing.push_str("1").unwrap();
    assert!(overflowing.storage_type().is_heap());
    assert_eq!(overflowing.len(), 31);

    // A '+' offset meeting a 'Z': the two spellings are mutually exclusive, so this leaves the tier too.
    let mut offset: PString = "14:33+01:00 14:33".parse().unwrap();
    offset.push_str("Z").unwrap();
    assert!(offset.storage_type().is_heap());
}

#[test]
fn incremental_building_reaches_the_packed_tier() {
    // The case the length families exist for: a string that passes through a trailing space on its way to something
    // longer, built one piece at a time through fmt::Write.
    use std::fmt::Write;
    let mut s = PString::empty();
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
        let s = PString::from_bytes(&content).unwrap();
        assert_eq!(s.len(), len, "length of {len} bytes of content");
        assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), &content[..], "content of {len} bytes");
    }

    // High bytes must not be mistaken for terminators: 0x80 and 0xFF are where the naive bit trick goes wrong.
    for filler in [0x80u8, 0xFF, 0x01, 0x7F] {
        for len in 1..=INLINE_MAX {
            let content = vec![filler; len];
            let s = PString::from_bytes(&content).unwrap();
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
        let s = PString::from_bytes(&content).unwrap();
        assert_eq!(s.len(), len, "length {len}");
        assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), &content[..], "content at length {len}");
    }

    // A byte with the high bit set must not be mistaken for the terminator: the trick discards borrows that came from
    // 0x80-or-above bytes, which is the half of it that is easy to get wrong.
    for len in 1..=INLINE_MAX {
        let mut content = vec![0xFFu8; len];
        content[len - 1] = 0x80;
        let s = PString::from_bytes(&content).unwrap();
        assert_eq!(s.len(), len, "high-bit content at length {len}");
    }
}

#[test]
fn nul_bearing_content_lives_inline_now() {
    // An explicit length admits what a terminator could not: a NUL is content like any other byte, and needs no special
    // case in construction, in appending, or in the tier ladder.
    for content in [&b"\0"[..], b"a\0b", b"\0\0\0", b"ab\0", b"\0abcdefghijklm", b"abcdefghijklmn\0"] {
        let s = PString::from_bytes(content).unwrap();
        assert!(s.storage_type().is_inline(), "{content:?} should be inline");
        assert_eq!(s.len(), content.len());
        assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), content);
    }

    // And appending one keeps the string inline.
    let mut s = PString::from_bytes(b"ab").unwrap();
    s.push_bytes(b"\0cd").unwrap();
    assert!(s.storage_type().is_inline());
    assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), b"ab\0cd");
}

#[test]
fn the_length_families_split_at_capacity() {
    // Content of exactly fifteen bytes fills the payload and implies its length; anything shorter stores it in the byte
    // a fifteenth character would have used.
    for len in 0..=INLINE_MAX {
        let content = vec![b'x'; len];
        let s = PString::from_bytes(&content).unwrap();
        assert_eq!(s.len(), len, "length {len}");
        assert!(s.storage_type().is_inline());
        assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), &content[..]);
    }

    // Growing across the boundary by appending, one byte at a time.
    let mut s = PString::empty();
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
    let whole = PString::from_bytes(b"abcde").unwrap();
    let mut built = PString::from_bytes(b"abc").unwrap();
    built.push_bytes(b"de").unwrap();
    assert_eq!(whole, built);

    // The same content reached through the full-capacity family and back down.
    let mut long = PString::from_bytes(b"abc").unwrap();
    long.push_bytes(b"de").unwrap();
    assert_eq!(whole, long);
}

#[test]
fn compressed_payloads_and_the_nibble_scheme() {
    // The Latin-1 class stores the Latin-1 transcoding of the internal bytes — each one- or two-byte UTF-8 sequence as
    // its single-byte equivalent — with the length byte split into the two nibbles (§2.2.9): low `s`, high `h`.  The E9
    // monster's two strings at the representation level.
    let two_char = PString::from_bytes([0xC3, 0xA9]).unwrap(); // The octet string C3.A9.
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

    let one_char = PString::from_bytes([0xE9]).unwrap(); // The one-octet string é: the Bytes residual.
    assert_eq!(one_char.storage_type(), StorageType::InlineBytes);
    assert_eq!(one_char.len(), 1);
    assert_ne!(two_char, one_char, "different strings, distinguished by the class axis alone");

    // Sixteen to thirty compressible bytes are the new inline intake: fifteen stored high bytes span thirty internal
    // bytes, report length 30 (container-verified: ord returns the lead C3), and fill the payload — the full family.
    let wide = PString::from_bytes([0xC3, 0xA9].repeat(15)).unwrap();
    assert_eq!(wide.storage_type(), StorageType::InlineLatin1Full);
    assert_eq!(wide.len(), 30, "length is the expansion sum, never the payload count");
    assert_eq!(wide.char_len(), Some(15));
    assert_eq!(wide.as_bytes(&mut [0u8; DECODE_MAX]), [0xC3, 0xA9].repeat(15));

    // The verbatim valid classes carry their character count in the aux nibble: O(1), no decode.
    let euro = PString::from_str("€€").unwrap(); // Six bytes, two characters, beyond Latin-1.
    assert_eq!(euro.storage_type(), StorageType::InlineNonLatin1);
    match euro.raw_parts() {
        RawParts::Inline { buf, .. } => assert_eq!(buf[LENGTH_BYTE], 0x26, "six stored, two characters: nibbles 2/6"),
        _ => panic!("expected inline storage"),
    }
    assert_eq!(euro.char_len(), Some(2));

    // Ascii and Bytes keep a zero aux nibble, making their short-family payloads bit-identical to the raw-byte tier.
    let plain = PString::from_bytes(b"abcd").unwrap();
    match plain.raw_parts() {
        RawParts::Inline { buf, .. } => assert_eq!(buf[LENGTH_BYTE], 0x04),
        _ => panic!("expected inline storage"),
    }

    // Overlong NUL never compresses; canonical NUL does — in every spelling (§2.2.9).
    assert_eq!(PString::from_bytes([0xC0, 0x80]).unwrap().storage_type(), StorageType::InlineBytes);
    assert_eq!(PString::from_bytes(b"a\0b").unwrap().storage_type(), StorageType::InlineAscii);

    // Deterministic ladder: 16-30-byte ASCII goes packed where an alphabet fits and heap otherwise — never compressed,
    // sixteen characters not fitting fifteen payload bytes.
    assert_eq!(PString::from_bytes(b"1234567890123456").unwrap().storage_type(), StorageType::PackedNumeric);
    assert_eq!(PString::from_bytes(b"abcdefghabcdefgh").unwrap().storage_type(), StorageType::Heap8Ascii);
}

#[test]
fn rebuilding_zeroes_everything_past_the_content() {
    // The canonical-padding obligation, checked at the representation rather than through content: a payload carrying
    // stale bytes past its length must come back with them cleared, or two equal strings could differ in their bytes
    // and representation would stop standing in for content.
    let mut dirty = [0xEEu8; INLINE_MAX];
    dirty[..4].copy_from_slice(b"abcd");
    let s = PString::build_inline(InlineClass::Ascii, false, false, 4, 0, dirty);

    match s.raw_parts() {
        RawParts::Inline { full, buf, .. } => {
            assert!(!full, "four bytes is the stored-length family");
            assert_eq!(&buf[..4], b"abcd");
            assert!(buf[4..LENGTH_BYTE].iter().all(|&b| b == 0), "padding must be cleared, got {:?}", &buf[4..LENGTH_BYTE]);
            assert_eq!(buf[LENGTH_BYTE], 4, "the length byte: aux nibble zero, stored nibble four");
        }
        _ => panic!("expected inline storage"),
    }

    assert_eq!(s, PString::from_bytes(b"abcd").unwrap());
}

#[test]
fn packed_equality_compares_nibbles_directly() {
    // Equal content in one alphabet has equal nibbles, so no decoding is needed — the encoding is injective and the
    // padding canonical.  These pin the answers rather than the mechanism, but a wrong fast path would break them.
    let a: PString = "2026-07-28T14:33:07Z".parse().unwrap();
    let b: PString = "2026-07-28T14:33:07Z".parse().unwrap();
    assert_eq!(a, b);
    assert!(a.storage_type().is_packed());

    // Differing in the last character, and in the first.
    assert_ne!(a, "2026-07-28T14:33:08Z".parse().unwrap());
    assert_ne!(a, "3026-07-28T14:33:07Z".parse().unwrap());

    // Different lengths within the same alphabet, including the two length families.
    assert_ne!(a, "2026-07-28T14:33:07.5Z".parse::<PString>().unwrap());
    let full: PString = "2026-07-29T17:23:45.123456789Z".parse().unwrap();
    assert_eq!(full, "2026-07-29T17:23:45.123456789Z".parse().unwrap());
    assert_ne!(full, "2026-07-29T17:23:45.12345678Z".parse().unwrap());

    // Different alphabets cannot hold equal content, so the mismatch is decisive.
    let numeric: PString = "192.168.100.200 1.2".parse().unwrap();
    assert_ne!(a, numeric);

    // Packed against the other tiers, both directions.
    let heaped: PString = "2026-07-28T14:33:07Z and then some more".parse().unwrap();
    assert_ne!(a, heaped);
    assert_ne!(heaped, a);

    let short: PString = "2026-07-28".parse().unwrap();
    assert_ne!(a, short);
    assert_ne!(short, a);

    // A packed string equals the same content held on the heap, which is the case the one-sided path serves.
    let long_numeric: PString = "1234567890123456789012345".parse().unwrap();
    assert!(long_numeric.storage_type().is_packed());

    let same_on_heap = {
        let mut s: PString = "1234567890123456789012345 tail".parse().unwrap();
        assert!(s.storage_type().is_heap());
        s = PString::from_bytes(&s.as_bytes(&mut [0u8; DECODE_MAX])[..25]).unwrap();
        s
    };
    assert_eq!(long_numeric, same_on_heap, "same content, different tiers");
}

// ── Ordering (§2.3.5), every case container-verified ──────────────

/// Rebuild a string from the internal bytes and flag the probe recorded, forcing the flag rather than letting the tier
/// ladder choose it — flagged ASCII is representable but no constructor produces it, ASCII being canonically unflagged.
fn from_hex(hex: &str, flagged: bool) -> PString {
    let bytes: Vec<u8> = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap()).collect();

    if bytes.len() <= INLINE_MAX {
        let (class, stored, aux, buf) = classify_inline(&bytes).expect("fifteen bytes always classify");
        return PString::build_inline(class, flagged, false, stored, aux, buf);
    }

    if (MIN_PACKED_LEN..=MAX_PACKED_LEN).contains(&bytes.len())
        && let Some(p) = pack(&bytes)
    {
        return PString::build_packed(p, flagged, false);
    }

    let parts = heap_parts_classified(&bytes).unwrap();
    PString::build_heap(flagged, false, parts)
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
/// tables and the intended behavior ever diverge.  `None` means the content leaves the packed tier for the heap.
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
fn the_two_upgrade_forms_agree() {
    // Both ways, same value: the in-place form rewrites a unique heap buffer where it can (skipping the invariant
    // prefix entirely, perl's shape), the copying form always produces a fresh one, and they must be indistinguishable
    // from outside.
    for bytes in [
        b"plain ascii long enough to reach the heap tier easily".to_vec(),
        [b"an invariant prefix that never moves ".to_vec(), vec![0xE9; 40]].concat(),
        vec![0xE9; 64],
        [0xC3u8, 0xA9].repeat(30),
        b"short".to_vec(),
        vec![0xE9],
        Vec::new(),
    ] {
        let src = PString::from_bytes(&bytes).unwrap();
        let copied = src.upgraded().unwrap();
        let mut in_place = src.clone();
        in_place.upgrade_in_place().unwrap();

        assert_eq!(copied, in_place, "the two forms must produce the same value for {bytes:?}");
        assert!(in_place.is_utf8());
        assert_eq!(in_place.char_len(), Some(bytes.len()), "every original byte becomes one character");
        assert_eq!(in_place.len(), bytes.len() + bytes.iter().filter(|&&b| b >= 0x80).count(), "the encoded length");
        assert_eq!(in_place.downgraded().unwrap().unwrap(), src, "and the round trip returns the original");
    }
}

#[test]
fn upgrading_in_place_leaves_a_sharer_untouched() {
    // A shared buffer cannot be rewritten under its other holders, so the in-place form falls back to copying.
    let bytes = [b"an invariant prefix ".to_vec(), vec![0xE9; 40]].concat();
    let sharer = PString::from_bytes(&bytes).unwrap();
    let mut upgraded = sharer.clone();
    upgraded.upgrade_in_place().unwrap();

    assert!(upgraded.is_utf8());
    assert_eq!(upgraded.char_len(), Some(bytes.len()));
    assert!(!sharer.is_utf8(), "the sharer keeps its flag");
    assert_eq!(sharer.len(), bytes.len(), "and its unexpanded bytes");
    assert_eq!(sharer.as_bytes(&mut [0u8; DECODE_MAX]), &bytes[..]);

    // The unique path rewrites the buffer it already owns and must reach the same answer.
    let mut unique = PString::from_bytes(&bytes).unwrap();
    unique.upgrade_in_place().unwrap();
    assert_eq!(unique, upgraded);
}

#[test]
fn the_two_downgrade_forms_agree() {
    // Contraction never grows, so the in-place form needs no reallocation — but it must validate before it moves, since
    // a refusal discovered halfway would leave the buffer holding neither string.
    for bytes in [
        b"plain ascii long enough to reach the heap tier easily".to_vec(),
        [b"an invariant prefix that never moves ".to_vec(), [0xC3u8, 0xA9].repeat(30)].concat(),
        [0xC3u8, 0xA9].repeat(40),
        b"short".to_vec(),
        Vec::new(),
    ] {
        let src = PString::from_bytes(&bytes).unwrap().upgraded().unwrap();
        let copied = src.downgraded().unwrap().expect("Latin-1-range content downgrades");
        let mut in_place = src.clone();
        assert!(in_place.downgrade_in_place().unwrap(), "and so does the in-place form");
        assert_eq!(copied, in_place, "the two forms must agree for {bytes:?}");
        assert!(!in_place.is_utf8());
        assert_eq!(in_place.as_bytes(&mut [0u8; DECODE_MAX]), &bytes[..], "the original octets return");
    }

    // Beyond U+00FF refuses, and the refusal leaves the value untouched rather than half-contracted.
    let mut wide = PString::from_str(&"€".repeat(40)).unwrap();
    let before = wide.clone();
    assert!(!wide.downgrade_in_place().unwrap(), "a character past U+00FF must refuse");
    assert_eq!(wide, before, "a refused downgrade changes nothing");
    assert_eq!(wide.downgraded().unwrap(), None, "and the copying form refuses alike");

    // A shared buffer takes the copying route; the sharer keeps its encoding.
    let shared = PString::from_bytes([0xC3u8, 0xA9].repeat(40)).unwrap().upgraded().unwrap();
    let sharer = shared.clone();
    let mut contracted = shared.clone();
    assert!(contracted.downgrade_in_place().unwrap());
    assert!(sharer.is_utf8(), "the sharer keeps its flag");
    assert_eq!(sharer.char_len(), Some(80), "and its characters");
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
    let mut s = PString::from_bytes(b"heap content that is long enough to stay heap").unwrap();
    s.push_bytes(b"").unwrap();
    assert_eq!(s.char_len(), Some(45));

    // Truncation to empty through the mutable escape: the count answer is 0 without any recount possible or needed.
    let empty = PString::from_bytes(vec![]).unwrap();
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

// ── In-place transforms (§2.2.3) ──────────────────────────────

/// The address of a heap string's data, for proving a rewrite stayed where it was.
fn heap_addr(s: &PString) -> Option<usize> {
    match s.raw_parts() {
        RawParts::Heap(view) => Some(view.as_slice().as_ptr() as usize),
        _ => None,
    }
}

#[test]
fn upgrade_moves_when_the_expansion_does_not_fit() {
    // A buffer born sized exactly to its content has no room for an upgrade that doubles the length, so this one must
    // move.  Nothing in the public API reveals whether a rewrite reallocated, so the allocation's address is the
    // witness.
    let mut s = PString::from_bytes([0xE9u8; 40]).unwrap();
    assert!(s.storage_type().is_small_heap_tier());
    let before = heap_addr(&s).expect("heap-resident");
    let cap_before = match s.raw_parts() {
        RawParts::Heap(v) => v.capacity(),
        _ => unreachable!(),
    };

    // Born sized to its content, so 40 Latin-1 bytes expanding to 80 cannot fit and must move.
    assert!(cap_before < 80, "the premise: this one has to reallocate");

    s.upgrade_in_place().unwrap();
    assert_eq!(s.len(), 80, "each Latin-1 byte becomes two UTF-8 bytes");
    assert!(s.is_utf8());
    assert_ne!(heap_addr(&s), Some(before), "no room, so the copying form ran");
}

#[test]
fn upgrade_rewrites_in_place_when_the_expansion_fits() {
    // A contraction leaves its spare capacity behind — §2.2.3 defers trimming deliberately — so a downgraded buffer
    // holds exactly the headroom its own re-upgrade needs.  The round trip is therefore both the natural test and the
    // case where in-place rewriting is reachable at all, birth allocation being exact.
    let mut s = PString::from_str(&"é".repeat(40)).unwrap();
    assert!(s.downgrade_in_place().unwrap());
    assert_eq!(s.len(), 40);

    let after_downgrade = heap_addr(&s).expect("heap-resident");
    let spare = match s.raw_parts() {
        RawParts::Heap(v) => v.capacity(),
        _ => unreachable!(),
    };
    assert!(spare >= 80, "the contraction kept room for its own reversal");

    s.upgrade_in_place().unwrap();
    assert_eq!(s.len(), 80, "back to the UTF-8 encoding");
    assert!(s.is_utf8());
    assert_eq!(heap_addr(&s), Some(after_downgrade), "the rewrite stayed in the allocation it found");
    assert_eq!(s.as_str(&mut [0u8; DECODE_MAX]), Some("é".repeat(40).as_str()), "and round-trips exactly");
    assert_eq!(s.char_len(), Some(40), "the count the rewrite recorded, not a rescan");
}

#[test]
fn downgrade_rewrites_in_place_and_never_reallocates() {
    // Contraction only shrinks, so it always fits: the buffer must be the same one afterwards, whatever its tier.
    let mut s = PString::from_str(&"é".repeat(40)).unwrap();
    assert!(s.is_utf8());
    let before = heap_addr(&s).expect("heap-resident");

    assert!(s.downgrade_in_place().unwrap(), "Latin-1 range contracts");
    assert_eq!(s.len(), 40, "two UTF-8 bytes become one Latin-1 byte");
    assert!(!s.is_utf8());
    assert_eq!(heap_addr(&s), Some(before), "contraction never reallocates (§2.2.3)");
    assert_eq!(s.as_bytes(&mut [0u8; DECODE_MAX]), &[0xE9u8; 40]);
}

#[test]
fn a_shared_buffer_forces_the_copying_form() {
    // Neither rewrite may touch bytes another handle can see, so sharing sends both down the copying path.
    let original = PString::from_str(&"é".repeat(40)).unwrap();
    let mut copy = original.clone();
    let shared_addr = heap_addr(&original).expect("heap-resident");
    assert_eq!(heap_addr(&copy), Some(shared_addr), "the clone shares the allocation");

    assert!(copy.downgrade_in_place().unwrap());
    assert_ne!(heap_addr(&copy), Some(shared_addr), "shared: contracted into a buffer of its own");
    assert_eq!(original.len(), 80, "the other handle is untouched");
    assert!(original.is_utf8());
}

#[test]
fn downgrade_refuses_content_past_the_latin1_range() {
    let mut wide = PString::from_str(&"字".repeat(20)).unwrap();
    let before = heap_addr(&wide).expect("heap-resident");
    assert!(!wide.downgrade_in_place().unwrap(), "a character past U+00FF cannot contract");
    assert_eq!(heap_addr(&wide), Some(before), "and the refusal leaves the buffer alone");
    assert!(wide.is_utf8(), "still flagged");
}

#[test]
fn raw_append_onto_a_small_tier_is_classified_not_left_unknown() {
    // A raw-byte append transitions to UNKNOWN in the lattice, and a small tier cannot hold that — the transition
    // funnel must pay the construction-grade pass instead of recording an indeterminate state (§2.2.3).
    let mut s = PString::from_bytes(b"a".repeat(24)).unwrap();
    assert!(s.storage_type().is_small_heap_tier());
    s.push_bytes(&[0x81, 0x82]).unwrap(); // AppendKind::Unknown: nothing known about these bytes

    assert_ne!(s.scan_state(), scan::Unknown, "indeterminate states are unrepresentable below 64 KiB");
    eq_probe::reset();
    let _ = s.is_perl_utf8_valid();
    let _ = s.is_perl_utf8_valid();
    assert_eq!(eq_probe::scans().0, 0, "both reads answered from the state the append recorded");

    // Oracle: the append's classification must be exactly construction's on the same bytes.
    let mut buf = [0u8; DECODE_MAX];
    let bytes = s.as_bytes(&mut buf).to_vec();
    let fresh = PString::from_bytes(&bytes).unwrap();
    assert_eq!(s.scan_state(), fresh.scan_state(), "the transition funnel agrees with construction");
    assert_eq!(s.char_len(), fresh.char_len(), "and on the cached count");
}

#[test]
fn downgraded_small_tier_is_classified_not_left_unknown() {
    // An in-place downgrade that left a small tier's envelope UNKNOWN would re-derive on every subsequent validity
    // question, since narrow_scan is deliberately a no-op there — (1, 1) scans across two reads where (1, 0) is the
    // invariant.  The rule is the same one construction follows: below 64 KiB the state is settled now or never.
    let mut s = PString::from_str(&"é".repeat(20)).unwrap();
    assert!(s.storage_type().is_small_heap_tier());
    assert!(s.downgrade_in_place().unwrap());

    // The downgrade itself paid the classifying pass, so the state is terminal and every read is a state read.
    assert_ne!(s.scan_state(), scan::Unknown, "a small tier is never left indeterminate (§2.2.3)");
    eq_probe::reset();
    let _ = s.is_perl_utf8_valid();
    let _ = s.is_perl_utf8_valid();
    assert_eq!(eq_probe::scans().0, 0, "both reads answered from the state the downgrade recorded");

    // Oracle: the recorded state and count must be exactly what constructing from the same bytes records.
    let mut buf = [0u8; DECODE_MAX];
    let bytes = s.as_bytes(&mut buf).to_vec();
    let fresh = PString::from_bytes(&bytes).unwrap();
    assert_eq!(s.scan_state(), fresh.scan_state(), "in-place downgrade agrees with construction");
    assert_eq!(s.char_len(), fresh.char_len(), "and on the cached count");
}

#[test]
fn small_tier_str_construction_is_one_pass() {
    // Probing for ASCII and then classifying would scan the same bytes twice for one fact: the classifying pass answers
    // the ASCII question through the terminal state, so a small tier pays one pass and no probe bytes.
    eq_probe::reset();

    let s = PString::from_str(&"a".repeat(40)).unwrap();
    assert!(s.storage_type().is_small_heap_tier());

    let (full, probe) = eq_probe::scans();
    assert_eq!((full, probe), (1, 0), "one classifying pass, no separate ASCII probe");
    assert!(s.is_ascii(), "and the state answers the flag question");
}

#[test]
fn large_tier_append_preserves_known_scan_state() {
    // The take that feeds an append reads the large tier's header rather than discarding its facts: an append onto a
    // buffer whose state a reader already narrowed must transition from that state, not from UNKNOWN — otherwise every
    // rebuild forgets what a scan already paid to learn.
    let mut s = PString::from_bytes(b"a".repeat(100_000)).unwrap();
    assert!(!s.storage_type().is_small_heap_tier());
    assert!(s.is_ascii(), "the probe narrows the shared scan slot");
    let narrowed = s.scan_state();
    assert_ne!(narrowed, scan::Unknown);

    s.push_str("bcd").unwrap();
    assert_eq!(s.scan_state(), narrowed, "ASCII onto a narrowed buffer keeps the narrowed state across the rebuild");
}

#[test]
fn append_within_class_headroom_extends_in_place() {
    // The fast path the class headroom exists for (§2.2.3): a unique buffer whose spare capacity holds the result
    // extends in place — no allocation, same buffer, facts maintained.
    let mut s = PString::from_str(&"x".repeat(40)).unwrap();
    let mut buf = [0u8; DECODE_MAX];
    let live = cow_buffer::live::count();
    let addr = s.as_bytes(&mut buf).as_ptr() as usize;

    s.push_str("yz").unwrap();
    assert_eq!(cow_buffer::live::count(), live, "the fast path allocates nothing");
    assert_eq!(s.as_bytes(&mut buf).as_ptr() as usize, addr, "the buffer is the same one");
    assert_eq!(s.as_bytes(&mut buf), format!("{}yz", "x".repeat(40)).as_bytes());
    assert!(s.is_ascii(), "ASCII onto ASCII stays established");
}

#[test]
fn append_past_capacity_rebuilds_through_the_constructor() {
    // Over capacity there is nothing to extend into: the rebuild releases the old buffer (net zero live) and the
    // tier-choosing constructor establishes the result's facts.
    let mut s = PString::from_str(&"x".repeat(40)).unwrap();
    let mut buf = [0u8; DECODE_MAX];
    let live = cow_buffer::live::count();
    let addr = s.as_bytes(&mut buf).as_ptr() as usize;

    s.push_str(&"y".repeat(100)).unwrap();
    assert_eq!(cow_buffer::live::count(), live, "one allocated, one released");
    assert_ne!(s.as_bytes(&mut buf).as_ptr() as usize, addr, "a rebuilt buffer is a different one");
    assert_eq!(s.as_bytes(&mut buf).len(), 140);
    assert!(s.is_ascii());
}

#[test]
fn append_onto_a_shared_buffer_rebuilds_and_leaves_the_sharer() {
    // Shared means no in-place anything: the appender rebuilds into its own allocation and the sharer keeps the
    // original untouched — append is where COW pays its copy.
    let a = PString::from_str(&"x".repeat(40)).unwrap();
    let mut b = a.clone();
    let live = cow_buffer::live::count();

    b.push_str("z").unwrap();
    let (mut ba, mut bb) = ([0u8; DECODE_MAX], [0u8; DECODE_MAX]);
    assert_eq!(cow_buffer::live::count(), live + 1, "the appender got its own allocation; the sharer keeps the old");
    assert_eq!(a.as_bytes(&mut ba), "x".repeat(40).as_bytes(), "the sharer is untouched");
    assert_eq!(b.as_bytes(&mut bb), format!("{}z", "x".repeat(40)).as_bytes());
    assert_ne!(a.as_bytes(&mut ba).as_ptr(), b.as_bytes(&mut bb).as_ptr());
}

#[test]
fn raw_append_in_place_settles_a_small_tier() {
    // A raw append transitions to UNKNOWN, which a small tier cannot hold: the in-place path classifies the joined
    // content — one pass, still no allocation — so the envelope ends settled exactly as construction leaves it.
    let mut s = PString::from_str(&"x".repeat(40)).unwrap();
    let live = cow_buffer::live::count();

    s.push_bytes(&[0xC3, 0xA9]).unwrap();
    let mut buf = [0u8; DECODE_MAX];
    assert_eq!(cow_buffer::live::count(), live, "settling in place allocates nothing");
    assert_eq!(s.as_bytes(&mut buf).len(), 42);
    assert!(!s.is_ascii());
    assert!(scan::is_terminal(s.scan_state()), "a small tier ends settled, never UNKNOWN");
}

// ── Leak accounting (the bomb's second layer) ──────────────────────────────

#[test]
fn heap_append_releases_the_buffer_it_replaces() {
    // The heap-to-heap append rebuilds into a fresh allocation, and the old one must be released, not abandoned.
    // The bomb catches an abandonment at the drop site; this counts the balance, which also covers a leak that never
    // touches an `Owned`.
    let before = crate::cow_buffer::live::count();
    {
        let mut s = PString::from_bytes(b"a".repeat(24)).unwrap();
        assert!(s.storage_type().is_small_heap_tier());

        for _ in 0..8 {
            s.push_str("bcdefghij").unwrap();
        }

        assert!(crate::cow_buffer::live::count() > before, "the appends allocated");
    }
    assert_eq!(crate::cow_buffer::live::count(), before, "every allocation the appends made was released");
}

#[test]
fn construction_and_transform_round_trips_balance_allocations() {
    let before = crate::cow_buffer::live::count();
    {
        let mut s = PString::from_str(&"é".repeat(40)).unwrap();
        assert!(s.downgrade_in_place().unwrap());
        s.upgrade_in_place().unwrap();
        let _clone = s.clone();
        let _big = lazy_heap(&[0xC3, 0xA9]);
    }
    assert_eq!(crate::cow_buffer::live::count(), before, "constructions, transforms and clones all balance");
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "Owned dropped while still armed")]
fn the_bomb_detonates_on_an_abandoned_obligation() {
    // The mechanism's own test: an armed `Owned` dropped without release is the defect, reported at its site.  Balance
    //  is restored via a manual release inside the panic path being impossible — so this test intentionally leaks one
    // allocation on its own thread; the counter is thread-local and this thread ends here.
    let ptr = crate::cow_buffer::heap16::allocate(32).unwrap();

    // SAFETY: freshly allocated with one reference, which this `Owned` takes on — and then abandons.
    let _armed = unsafe { crate::cow_buffer::Owned::from_raw(ptr) };
}

// ── Malformed-span reporting (§2.7.8) ─────────────────────────
/// Walk to the end of `bytes`, collecting decoded code points and rejected spans in the order they occur.
fn lossy_walk(bytes: &[u8]) -> (Vec<u64>, Vec<Vec<u8>>, Option<usize>) {
    let mut facts = ScanFacts::default();
    let (mut points, mut spans) = (Vec::new(), Vec::new());
    let stop = scalar_decode_span_reporting(
        bytes,
        0,
        bytes.len(),
        &mut facts,
        |v| points.push(v),
        |span| {
            spans.push(span.to_vec());
            ControlFlow::Continue(())
        },
    );

    (points, spans, stop)
}

#[test]
fn breaking_on_the_first_rejection_reproduces_the_classifying_walk() {
    // The plain decoder is the reporting one with a breaking closure, so the two must agree byte for byte on inputs
    // that are malformed as well as on inputs that are not.
    let cases: [&[u8]; 7] = [
        b"plain ascii",
        &[0xC3, 0xA9],             // well-formed two-byte
        &[0xF4, 0x90, 0x80, 0x80], // supra-Unicode, well formed under perl
        &[0x80],                   // stray continuation
        &[0xE4, 0xB8],             // truncated
        &[0xC0, 0xAF],             // overlong
        &[b'a', 0x80, b'b'],       // rejection with content on both sides
    ];

    for bytes in cases {
        let (mut plain_facts, mut break_facts) = (ScanFacts::default(), ScanFacts::default());
        let plain = scalar_decode_span(bytes, 0, bytes.len(), &mut plain_facts, |_| {});
        let broken = scalar_decode_span_reporting(bytes, 0, bytes.len(), &mut break_facts, |_| {}, |_| ControlFlow::Break(()));

        assert_eq!(plain, broken, "stop position differs for {bytes:02X?}");
        assert_eq!(plain_facts.state(), break_facts.state(), "state differs for {bytes:02X?}");
        assert_eq!(plain_facts.chars, break_facts.chars, "count differs for {bytes:02X?}");
    }
}

#[test]
fn a_rejected_span_covers_its_lead_and_the_continuations_that_follow() {
    // One replacement per rejected sequence, not per byte: a lead byte claims the continuation bytes after it, whatever
    // made the sequence unacceptable.
    for bytes in [[0xE4u8, 0xB8].as_slice(), &[0xC0, 0xAF], &[0xF0, 0x80, 0x80, 0x80]] {
        let (points, spans, stop) = lossy_walk(bytes);

        assert_eq!(points, Vec::<u64>::new());
        assert_eq!(spans, vec![bytes.to_vec()], "span differs for {bytes:02X?}");
        assert_eq!(stop, Some(bytes.len()));
    }
}

#[test]
fn stray_continuations_group_into_one_maximal_run() {
    let (points, spans, stop) = lossy_walk(&[0x80, 0x80, 0x80]);

    assert_eq!(points, Vec::<u64>::new());
    assert_eq!(spans, vec![vec![0x80, 0x80, 0x80]]);
    assert_eq!(stop, Some(3));
}

#[test]
fn grouping_covers_only_the_bytes_the_decoder_rejected() {
    // The span is the run the decode failed on, not a greedy sweep of every continuation in sight: the first two bytes
    // decode cleanly and only the third is stray.
    let (points, spans, stop) = lossy_walk(&[0xC2, 0x80, 0x80]);

    assert_eq!(points, vec![0x80]);
    assert_eq!(spans, vec![vec![0x80]]);
    assert_eq!(stop, Some(3));
}

#[test]
fn a_non_continuation_after_a_lead_ends_the_span_and_decodes_on_its_own() {
    // `C2` claims nothing, `A` being no continuation, so the letter survives as itself rather than joining the span.
    let (points, spans, stop) = lossy_walk(&[0xC2, b'A']);

    assert_eq!(points, vec![u64::from(b'A')]);
    assert_eq!(spans, vec![vec![0xC2]]);
    assert_eq!(stop, Some(2));
}

#[test]
fn walking_on_resumes_decoding_after_each_rejected_span() {
    let (points, spans, stop) = lossy_walk(&[b'a', 0x80, 0xC3, 0xA9, 0xE4, 0xB8]);

    assert_eq!(points, vec![u64::from(b'a'), 0xE9]);
    assert_eq!(spans, vec![vec![0x80], vec![0xE4, 0xB8]]);
    assert_eq!(stop, Some(6));
}

#[test]
fn classification_facts_do_not_move_across_a_rejected_span() {
    // Content holding such a span is malformed whatever surrounds it, so a walk that continues is rendering or counting
    // rather than classifying, and reads the spans from the closure instead.
    let mut facts = ScanFacts::default();
    let stop = scalar_decode_span_reporting(&[0x80, 0x80], 0, 2, &mut facts, |_| {}, |_| ControlFlow::Continue(()));

    assert_eq!(stop, Some(2));
    assert_eq!(facts.chars, 0);
    assert_eq!(facts.state(), scan::Terminal::Ascii);
}

#[test]
fn well_formed_content_reports_nothing_and_still_carries_its_facts() {
    // The supra-Unicode case is the one that must not be mistaken for malformed: perl decodes it to a single code
    // point, and only Rust cannot hold it.
    let (points, spans, stop) = lossy_walk(&[0xF4, 0x90, 0x80, 0x80]);

    assert_eq!(points, vec![0x11_0000]);
    assert!(spans.is_empty());
    assert_eq!(stop, Some(4));

    let mut facts = ScanFacts::default();
    scalar_decode_span(&[0xF4, 0x90, 0x80, 0x80], 0, 4, &mut facts, |_| {});

    assert_eq!(facts.state(), scan::Terminal::ExtendedUtf8);
    assert_eq!(facts.chars, 1);
}

/// Deterministic splitmix64, so a failure names its input and reproduces exactly.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Encode `v` in its minimal perl-extended form — the exact inverse of `decode_one`, which rejects non-minimal forms,
/// so every decoded value re-encodes to the bytes it came from.
fn encode_extended(v: u64, out: &mut Vec<u8>) {
    let len: usize = match v {
        0..=0x7F => 1,
        0x80..=0x7FF => 2,
        0x800..=0xFFFF => 3,
        0x1_0000..=0x1F_FFFF => 4,
        0x20_0000..=0x3FF_FFFF => 5,
        0x400_0000..=0x7FFF_FFFF => 6,
        0x8000_0000..=0xF_FFFF_FFFF => 7,
        _ => 13,
    };
    if len == 1 {
        out.push(v as u8);
        return;
    }

    let cont = len - 1;
    out.push(match len {
        2 => 0xC0 | (v >> 6) as u8,
        3 => 0xE0 | (v >> 12) as u8,
        4 => 0xF0 | (v >> 18) as u8,
        5 => 0xF8 | (v >> 24) as u8,
        6 => 0xFC | (v >> 30) as u8,
        7 => 0xFE,
        _ => 0xFF,
    });
    for k in (0..cont).rev() {
        // The FF form's twelve continuations span 72 bits; every accepted value fits u64, so the groups above bit 63
        // are zero and must not be reached by a real shift.
        let group = if 6 * k < 64 { (v >> (6 * k)) & 0x3F } else { 0 };
        out.push(0x80 | group as u8);
    }
}

#[test]
fn random_content_round_trips_through_the_reporting_decoder() {
    use std::cell::RefCell;

    enum Ev {
        Point(u64),
        Span(Vec<u8>),
    }

    let mut seed = 0x0DDB_1A5E_5BAD_5EEDu64;
    for iteration in 0..2000u32 {
        // Concatenate pieces that interleave well-formed and rejected material densely: ASCII runs, minimal encodings
        // from every form length (surrogates and supra-Unicode included — well formed under perl), raw bytes, stray
        // continuations, truncated sequences, overlong leads, and an FF form past IV_MAX.
        let mut bytes = Vec::new();
        for _ in 0..=(splitmix(&mut seed) % 8) {
            match splitmix(&mut seed) % 8 {
                0 => {
                    for _ in 0..(splitmix(&mut seed) % 6) {
                        bytes.push((splitmix(&mut seed) % 0x80) as u8);
                    }
                }
                1 | 2 => {
                    let v = match splitmix(&mut seed) % 8 {
                        0 => splitmix(&mut seed) % 0x80,
                        1 => 0x80 + splitmix(&mut seed) % 0x780,
                        2 => 0x800 + splitmix(&mut seed) % 0xF800,
                        3 => 0x1_0000 + splitmix(&mut seed) % 0x1F_0000,
                        4 => 0xD800 + splitmix(&mut seed) % 0x800,
                        5 => 0x20_0000 + splitmix(&mut seed) % 0x100_0000,
                        6 => 0x8000_0000 + splitmix(&mut seed) % 0x1_0000_0000,
                        _ => 0x10_0000_0000 + splitmix(&mut seed) % 0x1000_0000_0000,
                    };
                    encode_extended(v, &mut bytes);
                }
                3 => {
                    for _ in 0..=(splitmix(&mut seed) % 4) {
                        bytes.push((splitmix(&mut seed) & 0xFF) as u8);
                    }
                }
                4 => {
                    for _ in 0..=(splitmix(&mut seed) % 4) {
                        bytes.push(0x80 | (splitmix(&mut seed) % 0x40) as u8);
                    }
                }
                5 => {
                    let mut t = Vec::new();
                    encode_extended(0x800 + splitmix(&mut seed) % 0xF800, &mut t);
                    t.truncate(1 + (splitmix(&mut seed) as usize % (t.len() - 1)));
                    bytes.extend_from_slice(&t);
                }
                6 => {
                    bytes.push(if splitmix(&mut seed).is_multiple_of(2) { 0xC0 } else { 0xC1 });
                    bytes.push(0x80 | (splitmix(&mut seed) % 0x40) as u8);
                }
                _ => {
                    bytes.push(0xFF);
                    bytes.extend_from_slice(&[0xBF; 12]);
                }
            }
        }

        // The plain decoder and the breaking closure must agree exactly, malformed input or not.
        let mut plain_facts = ScanFacts::default();
        let mut plain_emits = 0usize;
        let plain = scalar_decode_span(&bytes, 0, bytes.len(), &mut plain_facts, |_| plain_emits += 1);

        let mut break_facts = ScanFacts::default();
        let mut break_emits = 0usize;
        let broke = scalar_decode_span_reporting(&bytes, 0, bytes.len(), &mut break_facts, |_| break_emits += 1, |_| ControlFlow::Break(()));

        assert_eq!(plain, broke, "[{iteration}] stop position differs for {bytes:02X?}");
        assert_eq!(
            (plain_facts.state(), plain_facts.chars, plain_emits),
            (break_facts.state(), break_facts.chars, break_emits),
            "[{iteration}] classification differs for {bytes:02X?}"
        );

        // Walking on must cover every byte exactly once: re-encoding the emitted code points and splicing the reported
        // spans back in, in order, must reproduce the input bit for bit.
        let events = RefCell::new(Vec::<Ev>::new());
        let mut walk_facts = ScanFacts::default();
        let stop = scalar_decode_span_reporting(
            &bytes,
            0,
            bytes.len(),
            &mut walk_facts,
            |v| events.borrow_mut().push(Ev::Point(v)),
            |span| {
                events.borrow_mut().push(Ev::Span(span.to_vec()));
                ControlFlow::Continue(())
            },
        );

        assert_eq!(stop, Some(bytes.len()), "[{iteration}] the continuing walk fell short for {bytes:02X?}");

        let mut rebuilt = Vec::new();
        let mut saw_span = false;
        for ev in events.into_inner() {
            match ev {
                Ev::Point(v) => encode_extended(v, &mut rebuilt),
                Ev::Span(span) => {
                    saw_span = true;
                    rebuilt.extend_from_slice(&span);
                }
            }
        }

        assert_eq!(rebuilt, bytes, "[{iteration}] round trip differs");
        assert_eq!(plain.is_some(), !saw_span, "[{iteration}] the plain decoder and the spans disagree on malformedness");
    }
}

// ── Display (§2.7.8) ──────────────────────────────────────────
/// A flagged string with the given payload bytes, however the classifier judges them.
fn flagged(bytes: &[u8]) -> PString {
    let mut s = PString::from_bytes(bytes).unwrap();
    s.set_utf8_for_test();
    s
}

#[test]
fn display_matches_std_formatting_wherever_std_can_render_the_content() {
    // Where the content is representable as a `&str`, our padding machinery must be indistinguishable from
    // `Formatter::pad` across width, precision, fill, and every alignment — including the sign-aware-zero quirk,
    // whatever std does with it, since both sides read the same spec accessors.
    macro_rules! check {
        ($spec:literal, $s:expr) => {{
            let oracle: &str = $s;
            let ours = flagged(oracle.as_bytes());
            assert_eq!(format!($spec, ours), format!($spec, oracle), "spec {} on flagged", $spec);

            if oracle.is_ascii() {
                let plain = PString::from_bytes(oracle.as_bytes()).unwrap();
                assert_eq!(format!($spec, plain), format!($spec, oracle), "spec {} on unflagged", $spec);
            }
        }};
    }

    for s in ["", "a", "hello", "héllo wörld", "aé中\u{1F600}tail"] {
        check!("{}", s);
        check!("{:10}", s);
        check!("{:<10}", s);
        check!("{:>10}", s);
        check!("{:^10}", s);
        check!("{:^11}", s);
        check!("{:*^11}", s);
        check!("{:.3}", s);
        check!("{:.0}", s);
        check!("{:>10.3}", s);
        check!("{:*<7.2}", s);
        check!("{:08}", s);
        check!("{:2}", s);
    }
}

#[test]
fn unflagged_high_bytes_render_as_latin_1_characters_and_never_replace() {
    // Unflagged `C3 A9` is two Latin-1 characters, not `é` — the flag decides the character model, and the byte
    // string's rendering must say so.
    assert_eq!(format!("{}", PString::from_bytes([0xC3, 0xA9]).unwrap()), "Ã©");
    assert_eq!(format!("{}", PString::from_bytes([0xE9]).unwrap()), "é");
    assert_eq!(format!("{}", PString::from_bytes([0x00, 0xFF]).unwrap()), "\u{0}ÿ");

    // Padding counts characters, not bytes: one high byte is one column.
    assert_eq!(format!("{:>4}", PString::from_bytes([0xE9]).unwrap()), "   é");
    assert_eq!(format!("{:.2}", PString::from_bytes([0xE9, 0xE8, 0xE7]).unwrap()), "éè");
}

#[test]
fn unrepresentable_code_points_render_as_one_replacement_each() {
    // Supra-Unicode and surrogates are well formed under perl and single characters; Rust merely cannot hold them, so
    // each is exactly one U+FFFD — where `from_utf8_lossy` would emit four and three.
    assert_eq!(format!("{}", flagged(&[0xF4, 0x90, 0x80, 0x80])), "\u{FFFD}");
    assert_eq!(format!("{}", flagged(&[0xED, 0xA0, 0x80])), "\u{FFFD}");
    assert_eq!(format!("{:>3}", flagged(&[0xF4, 0x90, 0x80, 0x80])), "  \u{FFFD}");
    assert_eq!(format!("{}", flagged(b"a\xF4\x90\x80\x80b")), "a\u{FFFD}b");
}

#[test]
fn malformed_content_renders_one_replacement_per_rejected_sequence() {
    // The §2.7.8 grouping table, rendered: a lead claims its continuations, stray continuations run maximally, and the
    // span is what the decoder rejected rather than a greedy sweep.
    let table: [(&[u8], &str); 6] = [
        (&[0x80], "\u{FFFD}"),
        (&[0x80, 0x80, 0x80], "\u{FFFD}"),
        (&[0xE4, 0xB8], "\u{FFFD}"),
        (&[0xC0, 0xAF], "\u{FFFD}"),
        (&[0xC2, 0x80, 0x80], "\u{80}\u{FFFD}"),
        (&[0xC2, b'A'], "\u{FFFD}A"),
    ];
    for (bytes, expected) in table {
        assert_eq!(format!("{}", flagged(bytes)), expected, "for {bytes:02X?}");
    }
}

#[test]
fn malformed_content_pads_and_truncates_by_rendered_glyphs() {
    // The malformed terminal has no cached count, so the counting walk supplies one — and it must agree with what the
    // render emits, or the fill arithmetic would drift.
    let s = flagged(b"a\x80b");

    assert_eq!(format!("{}", s), "a\u{FFFD}b");
    assert_eq!(format!("{:^5}", s), " a\u{FFFD}b ");
    assert_eq!(format!("{:.1}", s), "a");
    assert_eq!(format!("{:.2}", s), "a\u{FFFD}");
    assert_eq!(format!("{:>4.2}", s), "  a\u{FFFD}");
}

#[test]
fn precision_cuts_flagged_valid_content_on_character_boundaries() {
    let s = flagged("aé中tail".as_bytes());

    assert_eq!(format!("{:.1}", s), "a");
    assert_eq!(format!("{:.2}", s), "aé");
    assert_eq!(format!("{:.3}", s), "aé中");
}

// ── Debug (§2.7.8) ────────────────────────────────────────────
#[test]
fn content_debug_renders_the_ruled_escape_format() {
    // Two digits is always a raw byte, four or more is always a code point, and the string kind is the quote prefix:
    // bare quotes are UTF-8 assumed, b-prefixed is a byte string.
    let table: [(&[u8], bool, &str); 17] = [
        (b"h\xC3\xA9llo", false, r#"b"h\x{c3}\x{a9}llo""#),
        (b"h\xC3\xA9llo", true, "\"h\u{e9}llo\""),
        (&[0xF4, 0x90, 0x80, 0x80], true, r#""\x{110000}""#),
        (&[0xED, 0xA0, 0x80], true, r#""\x{d800}""#),
        (&[0xC2, 0x80], true, r#""\x{0080}""#),
        (&[0x80], true, r#""\x{80}""#),
        (&[0xE4, 0xB8], true, r#""\x{e4}\x{b8}""#),
        (&[0xC2, 0x80, 0x80], true, r#""\x{0080}\x{80}""#),
        (&[0x07], true, r#""\x{07}""#),
        (&[0x07], false, r#""\x{07}""#),
        (b"a\"b\\c", true, r#""a\"b\\c""#),
        (b"a\nb\tc\r", false, r#""a\nb\tc\r""#),
        (b"hi", true, r#""hi""#),
        (&[0xC2, 0x85], true, r#""\x{0085}""#),
        (&[0x85], true, r#""\x{85}""#),
        (b"\xF0\x9F\x98\x80", true, "\"\u{1F600}\""),
        (&[0xFF, 0x80, 0x87, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF], true, r#""\x{7fffffffffffffff}""#),
    ];
    for (bytes, utf8, expected) in table {
        let mut s = PString::from_bytes(bytes).unwrap();
        if utf8 {
            s.set_utf8_for_test();
        }

        assert_eq!(format!("{:?}", ContentDebug(&s)), expected, "for {bytes:02X?} utf8={utf8}");
    }
}

/// Invert the content rendering back to (flag evidence, bytes) — the mechanical parser the format promises.  The
/// evidence is `None` exactly for pure-ASCII `"…"` content, where the rendering deliberately serves both flags and the
/// struct's `utf8:` field completes the identity.
fn parse_content_debug(text: &str) -> (Option<bool>, Vec<u8>) {
    let (mut utf8, body) = match text.strip_prefix("b\"") {
        Some(rest) => (Some(false), rest),
        None => (None, text.strip_prefix('\"').unwrap()),
    };
    let body = body.strip_suffix('\"').unwrap();

    let mut out = Vec::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            if u32::from(c) >= 0x80 {
                utf8 = Some(true);
            }

            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }

        match chars.next().unwrap() {
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            '\"' => out.push(b'\"'),
            '\\' => out.push(b'\\'),
            'x' => {
                assert_eq!(chars.next(), Some('{'));
                let digits: String = chars.by_ref().take_while(|&d| d != '}').collect();
                match digits.len() {
                    2 => {
                        let b = u8::from_str_radix(&digits, 16).unwrap();
                        if b >= 0x80 && utf8.is_none() {
                            // Inside "…" a two-digit escape at 0x80 or above can only be a rejected byte.
                            utf8 = Some(true);
                        }

                        out.push(b);
                    }
                    n if n >= 4 => {
                        utf8 = Some(true);
                        encode_extended(u64::from_str_radix(&digits, 16).unwrap(), &mut out);
                    }
                    n => panic!("a {n}-digit escape is never emitted: {digits}"),
                }
            }
            other => panic!("unknown escape \\{other}"),
        }
    }

    (utf8, out)
}

#[test]
fn content_debug_round_trips_arbitrary_flag_and_bytes() {
    // Losslessness, mechanically: parse the rendering back and require the exact (flag, bytes) identity.  Malformed
    // content round-trips because rejected bytes are spelled as bytes, and decoded code points invert uniquely because
    // the decoder admits only minimal forms.
    let mut seed = 0xDEB0_65C4_9E5Fu64;
    for iteration in 0..1500u32 {
        let n = (splitmix(&mut seed) % 24) as usize;
        let bytes: Vec<u8> = (0..n).map(|_| (splitmix(&mut seed) & 0xFF) as u8).collect();
        let utf8 = splitmix(&mut seed).is_multiple_of(2);

        let mut s = PString::from_bytes(&bytes).unwrap();
        if utf8 {
            s.set_utf8_for_test();
        }

        let rendered = format!("{:?}", ContentDebug(&s));
        let (evidence, back) = parse_content_debug(&rendered);

        assert_eq!(back, bytes, "[{iteration}] via {rendered}");
        match evidence {
            Some(flag) => assert_eq!(flag, utf8, "[{iteration}] via {rendered}"),
            None => assert!(bytes.iter().all(u8::is_ascii), "[{iteration}] ambiguity is reserved for pure ASCII: {rendered}"),
        }
    }
}

#[test]
fn debug_shows_the_envelope_for_resident_tiers_and_omits_it_for_pointers() {
    let inline = format!("{:?}", PString::from_bytes(b"h\xC3\xA9llo").unwrap());
    assert!(inline.contains("string: b\"h\\x{c3}\\x{a9}llo\""), "{inline}");
    assert!(inline.contains("bytes: 68 e9 6c 6c 6f 00"), "the envelope hex shows physical storage: {inline}");

    let packed = format!("{:?}", PString::from_bytes(b"2026-08-22T17:49:00").unwrap());
    assert!(packed.contains("storage: Packed"), "{packed}");
    assert!(packed.contains("bytes: "), "{packed}");

    let heap = format!("{:?}", PString::from_bytes([b'x'; 40]).unwrap());
    assert!(heap.contains("string: \"xxx"), "{heap}");
    assert!(!heap.contains("bytes:"), "pointer tiers omit the envelope field: {heap}");
}

// ── Views (§2.2.15, Stage 2) ──────────────────────────────────
/// A Heap32 parent: past the Heap16 band, mixed content unless stated.
fn heap32_parent(len: usize) -> PString {
    let mut v = vec![b'a'; len];
    v[len / 2] = 0xC3;
    v[len / 2 + 1] = 0xA9;
    let s = PString::from_bytes(v).unwrap();
    assert_eq!(s.storage_type(), StorageType::Heap32, "the fixture must land in the native view tier");
    s
}

#[test]
fn a_native_view_reads_the_subrange_without_copying() {
    let parent = heap32_parent(100_000);
    let mut scratch_a = [0u8; DECODE_MAX];
    let mut scratch_b = [0u8; DECODE_MAX];

    let view = parent.view_range(10, 50_000).unwrap();
    assert_eq!(view.storage_type(), StorageType::FarSlice, "under 64 KiB the far form is preferred");
    assert_eq!(view.len(), 50_000);
    assert_eq!(view.as_bytes(&mut scratch_a), &parent.as_bytes(&mut scratch_b)[10..50_010]);

    // The view is the same value as the copy, under the same flag.
    let copy = PString::from_bytes(&parent.as_bytes(&mut scratch_b)[10..50_010]).unwrap();
    assert_eq!(view, copy);
}

#[test]
fn a_native_view_keeps_the_buffer_alive_past_the_parent() {
    let view = {
        let parent = heap32_parent(70_000);
        parent.view_range(0, 65_000).unwrap()
    };

    // The parent handle is gone; the view's retain holds the allocation.
    let mut scratch = [0u8; DECODE_MAX];
    assert_eq!(view.as_bytes(&mut scratch)[0], b'a');
    assert_eq!(view.len(), 65_000);
}

#[test]
fn view_clones_retain_and_release_in_balance() {
    let parent = heap32_parent(70_000);
    let view = parent.view_range(5, 60_000).unwrap();
    let (a, b) = (view.clone(), view.clone());
    assert_eq!(a, b);
    drop(view);
    drop(a);

    let mut scratch = [0u8; DECODE_MAX];
    assert_eq!(b.as_bytes(&mut scratch).len(), 60_000);
}

#[test]
fn an_adopted_whole_view_reads_through_span() {
    let arc: std::sync::Arc<[u8]> = std::sync::Arc::from(&b"the adopted content"[..]);
    let a = cow_buffer::Adopted::adopt_arc_bytes(arc.clone(), scan::Ascii).unwrap();
    let view = PString::adopted_whole(a, false, false);

    assert_eq!(view.storage_type(), StorageType::Adopted);
    assert_eq!(view.len(), 19);

    let mut scratch = [0u8; DECODE_MAX];
    assert_eq!(view.as_bytes(&mut scratch), b"the adopted content");
    assert!(view.is_ascii());

    let clone = view.clone();

    drop(view);
    assert_eq!(std::sync::Arc::strong_count(&arc), 2, "the Adopted struct still shares the Arc");

    drop(clone);
    assert_eq!(std::sync::Arc::strong_count(&arc), 1, "the last view released the struct, which released the Arc");
}

#[test]
fn tag_transitions_preserve_the_view() {
    let parent = heap32_parent(70_000);
    let mut view = parent.view_range(0, 60_000).unwrap();

    view.taint();
    assert_eq!(view.storage_type(), StorageType::FarSlice, "taint is a tag transition, not a copy");
    assert!(view.is_tainted());

    view.untaint_for_sanctioned_path();
    assert_eq!(view.storage_type(), StorageType::FarSlice);
    assert!(!view.is_tainted());
}

#[test]
fn appending_to_a_view_materializes_away_from_it() {
    let parent = heap32_parent(70_000);
    let mut view = parent.view_range(0, 100).unwrap();

    view.push_bytes(b"!tail").unwrap();
    assert_ne!(view.storage_type(), StorageType::MediumSlice, "a view is a read-only carrier");
    assert_eq!(view.len(), 105);

    let mut scratch = [0u8; DECODE_MAX];
    assert_eq!(&view.as_bytes(&mut scratch)[100..], b"!tail");
}

#[test]
fn unshare_dissolves_the_pin() {
    let parent = heap32_parent(70_000);
    let mut view = parent.view_range(3, 200).unwrap();
    assert!(view.is_shared(), "a view shares its backing by construction");

    view.unshare().unwrap();
    assert!(!matches!(
        view.storage_type(),
        StorageType::SmallSlice | StorageType::MediumSlice | StorageType::FarSlice | StorageType::Adopted | StorageType::FarAdopted
    ));
    assert_eq!(view.len(), 200);
}

#[test]
fn the_birth_table_is_the_ruled_mapping() {
    use scan::ScanState::*;
    let clean: [(scan::ScanState, scan::ScanState); 11] = [
        (Unknown, Unknown),
        (Ascii, Ascii),
        (Utf8Latin1, MaybeUtf8Latin1),
        (MaybeUtf8Latin1, MaybeUtf8Latin1),
        (Utf8NonLatin1, ValidUtf8),
        (Utf8NonAscii, ValidUtf8),
        (ValidUtf8, ValidUtf8),
        (ExtendedUtf8, MaybeExtendedUtf8),
        (MaybeExtendedUtf8, MaybeExtendedUtf8),
        (PerlValidNonAscii, MaybeExtendedUtf8),
        (MalformedUtf8, Unknown),
    ];
    for (parent, born) in clean {
        assert_eq!(slice_birth(parent, true), born, "clean cut of {parent:?}");
    }

    assert_eq!(slice_birth(NonAscii, true), Unknown);

    // A dirty cut of any validity-asserting source is proven malformed by the cut, terminal and free.
    for parent in [Utf8Latin1, MaybeUtf8Latin1, Utf8NonLatin1, Utf8NonAscii, ValidUtf8, ExtendedUtf8, MaybeExtendedUtf8, PerlValidNonAscii] {
        assert_eq!(slice_birth(parent, false), MalformedUtf8, "dirty cut of {parent:?}");
    }

    for parent in [Unknown, NonAscii, MalformedUtf8] {
        assert_eq!(slice_birth(parent, false), Unknown, "dirty cut of {parent:?} asserted nothing to disprove");
    }
}

#[test]
fn cut_cleanliness_is_the_two_boundary_tests() {
    let bytes = b"ab\xC3\xA9cd";
    assert!(cut_is_clean(bytes, 0, 2));
    assert!(cut_is_clean(bytes, 2, 2), "the whole sequence is clean on both edges");
    assert!(!cut_is_clean(bytes, 3, 2), "starting on a continuation byte is dirty");
    assert!(!cut_is_clean(bytes, 0, 3), "ending mid-sequence is dirty");
    assert!(cut_is_clean(bytes, 0, 6), "the whole object is clean");
    assert!(cut_is_clean(bytes, 6, 0), "the empty tail is clean");
}

#[test]
fn a_dirty_cut_births_proven_malformed_and_a_small_clean_cut_classifies_eagerly() {
    // Above the eager floor, the table's word stands: a dirty cut of a valid parent is malformed with no scan.
    let parent = heap32_parent(70_000);
    let dirty = parent.view_range(35_001, 5_000).unwrap();
    assert_eq!(dirty.scan_state(), scan::MalformedUtf8);
    assert_eq!(dirty.char_len(), None);

    // Below the floor, birth classifies: a pure-ASCII subrange of a non-ASCII parent is born Ascii, tighter than the
    // table's answer.
    let small = parent.view_range(0, 100).unwrap();
    assert_eq!(small.scan_state(), scan::Ascii);
    assert!(small.is_ascii());
}

// ── SmallSlice (§2.2.15) ──────────────────────────────────────
#[test]
fn small_tier_views_read_share_and_release_under_the_capacity_dispatch() {
    // One backing per small tier: ~100 bytes lands in Heap8, ~40 KiB in Heap16.
    for (len, tier) in [(100usize, StorageType::Heap8), (40_000, StorageType::Heap16)] {
        let mut v = vec![b'x'; len];
        v[len / 2] = 0xC3;
        v[len / 2 + 1] = 0xA9;
        let parent = PString::from_bytes(v).unwrap();
        assert_eq!(parent.storage_type(), tier, "fixture for {tier:?}");

        let view = parent.view_range(1, len - 2).unwrap();
        assert_eq!(view.storage_type(), StorageType::SmallSlice);
        assert_eq!(view.len(), len - 2);

        let mut sa = [0u8; DECODE_MAX];
        let mut sb = [0u8; DECODE_MAX];
        assert_eq!(view.as_bytes(&mut sa), &parent.as_bytes(&mut sb)[1..len - 1]);

        // The buffer outlives the parent handle through the view's retain, and clones balance.
        let clone = view.clone();
        drop(parent);
        drop(view);
        assert_eq!(clone.as_bytes(&mut sa).len(), len - 2);
    }
}

#[test]
fn small_views_take_tag_transitions_in_place_and_materialize_on_write() {
    let parent = PString::from_bytes(vec![b'q'; 5_000]).unwrap();
    assert_eq!(parent.storage_type(), StorageType::Heap16Ascii, "pure ASCII takes the specialized family");
    let mut view = parent.view_range(10, 3_000).unwrap();

    view.taint();
    assert_eq!(view.storage_type(), StorageType::SmallSlice, "taint is a tag transition, not a copy");
    assert!(view.is_tainted());

    view.push_bytes(b"...").unwrap();
    assert_ne!(view.storage_type(), StorageType::SmallSlice);
    assert_eq!(view.len(), 3_003);
    assert!(view.is_tainted(), "materialization keeps the tag");
}

#[test]
fn a_small_view_of_ascii_content_is_born_ascii_and_answers_without_probing() {
    let parent = PString::from_bytes(vec![b'z'; 300]).unwrap();
    let view = parent.view_range(50, 200).unwrap();
    assert_eq!(view.scan_state(), scan::Ascii, "Ascii cuts clean by nature and survives exactly");
    assert!(view.is_ascii());
    assert_eq!(view.char_len(), Some(200));
}

// ── FarSlice and FarAdopted (§2.2.15) ─────────────────────────
#[test]
fn selection_prefers_far_and_keeps_medium_for_its_band() {
    // A parent past the u24 offset reach: 17 MiB.
    let parent = heap32_parent(17 * 1024 * 1024);

    // Short view, small offset: far wins on width even where medium fits.
    let short = parent.view_range(10, 1_000).unwrap();
    assert_eq!(short.storage_type(), StorageType::FarSlice);

    // Short view past the u24 reach: only far can carry it.
    let far = parent.view_range(16_900_000, 60_000).unwrap();
    assert_eq!(far.storage_type(), StorageType::FarSlice);
    let mut sa = [0u8; DECODE_MAX];
    let mut sb = [0u8; DECODE_MAX];
    assert_eq!(far.as_bytes(&mut sa), &parent.as_bytes(&mut sb)[16_900_000..16_960_000]);

    // Long view within u24 reach: the band only medium serves.
    let medium = parent.view_range(100, 200_000).unwrap();
    assert_eq!(medium.storage_type(), StorageType::MediumSlice);

    // Long view past u24 reach: the large forms' territory, not yet built.
    assert!(parent.view_range(16_900_000, 200_000).is_none());
}

#[test]
fn far_views_share_read_transition_and_materialize_like_their_kin() {
    let parent = heap32_parent(17 * 1024 * 1024);
    let mut view = parent.view_range(16_900_000, 500).unwrap();
    assert_eq!(view.storage_type(), StorageType::FarSlice);

    let clone = view.clone();
    view.taint();
    assert_eq!(view.storage_type(), StorageType::FarSlice, "taint is a tag transition");
    assert!(view.is_tainted() && !clone.is_tainted());

    view.push_bytes(b"++").unwrap();
    assert_ne!(view.storage_type(), StorageType::FarSlice);
    assert_eq!(view.len(), 502);

    drop(parent);
    let mut scratch = [0u8; DECODE_MAX];
    assert_eq!(clone.as_bytes(&mut scratch).len(), 500, "the clone's retain outlives the parent handle");
}

#[test]
fn far_adopted_carries_offsets_past_u24_reach() {
    let big: std::sync::Arc<[u8]> = {
        let mut v = vec![b'.'; 17 * 1024 * 1024];
        v[16_900_000] = b'X';
        std::sync::Arc::from(&v[..])
    };
    let a = cow_buffer::Adopted::adopt_arc_bytes(big.clone(), scan::Ascii).unwrap();

    // A far sub-view of the adoptee, built directly: the constructor surface is the verbs stage's.
    let view = PString::build_view(ViewBacking::AdoptedFar, false, false, unsafe { Owned::from_raw(a.cast()) }, 16_900_000, 100, scan::Ascii);
    assert_eq!(view.storage_type(), StorageType::FarAdopted);
    let mut scratch = [0u8; DECODE_MAX];
    assert_eq!(view.as_bytes(&mut scratch)[0], b'X');

    let mut clone = view.clone();
    clone.taint();
    assert_eq!(clone.storage_type(), StorageType::FarAdopted);

    drop(view);
    drop(clone);
    assert_eq!(std::sync::Arc::strong_count(&big), 1, "the last far view released the struct, which released the Arc");
}

// ── The verbs (§2.2.15, Stage 3) ──────────────────────────────
#[test]
fn slice_returns_representable_content_in_the_envelope_forms() {
    let parent = heap32_parent(100_000);
    let mut sa = [0u8; DECODE_MAX];
    let mut sb = [0u8; DECODE_MAX];

    // At or under fifteen bytes: always inline, whatever the source.
    let tiny = parent.slice(3, 10).unwrap();
    assert!(matches!(tiny.storage_type(), StorageType::InlineAscii | StorageType::InlineAsciiFull));
    assert_eq!(tiny.as_bytes(&mut sa), &parent.as_bytes(&mut sb)[3..13]);

    // Packable content past fifteen: the packed form.
    let stamp = PString::from_bytes(*b"xx2026-08-23T09:41:07Zyy").unwrap();
    let packed = stamp.slice(2, 20).unwrap();
    assert!(format!("{:?}", packed.storage_type()).starts_with("Packed"), "{:?}", packed.storage_type());

    // Unrepresentable twenty bytes of a heap source: shared, not copied.
    let mixed = parent.slice(49_990, 25).unwrap();
    assert_eq!(mixed.storage_type(), StorageType::FarSlice, "an unrepresentable sub-range of a shareable backing is a view");

    // The one reachable envelope-resident leftover: a dirty byte-cut of the Latin-1 inline class — seventeen
    // bytes splitting a sequence are the Bytes class past the inline ceiling and no alphabet — owned, there being
    // no buffer to share.
    let inline_src = PString::from_bytes([0xC3, 0xA9].repeat(15)).unwrap();
    assert!(format!("{:?}", inline_src.storage_type()).starts_with("InlineLatin1"), "{:?}", inline_src.storage_type());
    let owned = inline_src.slice(0, 17).unwrap();
    assert_eq!(owned.len(), 17);
    assert!(!matches!(
        owned.storage_type(),
        StorageType::SmallSlice | StorageType::MediumSlice | StorageType::FarSlice | StorageType::Adopted | StorageType::FarAdopted
    ));
}

#[test]
fn slice_clamps_as_perl_clamps() {
    let parent = heap32_parent(70_000);
    assert!(parent.slice(1_000_000, 10).unwrap().is_empty(), "an offset past the end is the empty string");
    assert_eq!(parent.slice(69_990, 1_000).unwrap().len(), 10, "a length past the end truncates");
    assert_eq!(parent.substr(69_990, 1_000).unwrap().len(), 10);
}

#[test]
fn oversized_native_views_take_the_large_slice_case() {
    let parent = heap32_parent(21 * 1024 * 1024);
    let big = parent.slice(100, 20 * 1024 * 1024).unwrap();
    assert_eq!(big.storage_type(), StorageType::Adopted, "the LargeSlice case wears a whole-object adopted envelope");
    assert_eq!(big.len(), 20 * 1024 * 1024);

    let mut sa = [0u8; DECODE_MAX];
    let mut sb = [0u8; DECODE_MAX];
    assert_eq!(big.as_bytes(&mut sa)[..64], parent.as_bytes(&mut sb)[100..164]);

    // The child's retain holds the buffer past the parent handle.
    drop(parent);
    assert_eq!(big.as_bytes(&mut sa).len(), 20 * 1024 * 1024);
}

#[test]
fn oversized_adopted_views_take_the_parent_child() {
    let arc: std::sync::Arc<[u8]> = std::sync::Arc::from(&vec![b'.'; 20 * 1024 * 1024][..]);
    let a = cow_buffer::Adopted::adopt_arc_bytes(arc.clone(), scan::Ascii).unwrap();
    let whole = PString::adopted_whole(a, false, false);

    let big = whole.slice(100, 18 * 1024 * 1024).unwrap();
    assert_eq!(big.storage_type(), StorageType::Adopted, "the LargeAdopted case is a Parent child worn whole");
    assert_eq!(big.len(), 18 * 1024 * 1024);

    drop(whole);
    assert_eq!(std::sync::Arc::strong_count(&arc), 2, "the chain still holds the Arc");
    drop(big);
    assert_eq!(std::sync::Arc::strong_count(&arc), 1, "releasing the child released the parent released the Arc");
}

#[test]
fn reslicing_composes_absolute_offsets() {
    let parent = heap32_parent(200_000);
    let view = parent.slice(1_000, 100_000).unwrap();
    let sub = view.slice(500, 40_000).unwrap();
    assert_eq!(sub.storage_type(), StorageType::FarSlice);

    let direct = parent.substr(1_500, 40_000).unwrap();
    assert_eq!(sub, direct, "composition reads the same bytes the direct copy does");

    // Small-tier composition, past representability.
    let small_parent = PString::from_bytes(vec![b's'; 5_000]).unwrap();
    let small_view = small_parent.slice(10, 3_000).unwrap();
    assert_eq!(small_view.storage_type(), StorageType::SmallSlice);
    let small_sub = small_view.slice(500, 2_000).unwrap();
    assert_eq!(small_sub.storage_type(), StorageType::SmallSlice);
    assert_eq!(small_sub, small_parent.substr(510, 2_000).unwrap());

    // Adopted composition selects far and medium by the same rule.
    let arc: std::sync::Arc<[u8]> = std::sync::Arc::from(&vec![b'a'; 18 * 1024 * 1024][..]);
    let whole = PString::adopted_whole(cow_buffer::Adopted::adopt_arc_bytes(arc, scan::Ascii).unwrap(), false, false);
    assert_eq!(whole.slice(16_900_000, 100).unwrap().storage_type(), StorageType::FarAdopted);
    assert_eq!(whole.slice(100, 200_000).unwrap().storage_type(), StorageType::Adopted);
}

#[test]
fn slicing_a_static_image_yields_another_static_envelope() {
    let s = PString::from_static_bytes(b"a static image with some length to it, well past the packed band for certain").unwrap();
    assert_eq!(s.storage_type(), StorageType::Static);

    let sub = s.slice(2, 40).unwrap();
    assert_eq!(sub.storage_type(), StorageType::Static, "the image outlives every handle; the sub-envelope is free");
    assert_eq!(sub, s.substr(2, 40).unwrap());
}

#[test]
fn substr_is_always_uniquely_owned_and_flags_ride_both_verbs() {
    let mut parent = heap32_parent(70_000);
    parent.taint();

    let copy = parent.substr(10, 40_000).unwrap();
    assert!(!matches!(
        copy.storage_type(),
        StorageType::SmallSlice | StorageType::MediumSlice | StorageType::FarSlice | StorageType::Adopted | StorageType::FarAdopted
    ));
    assert!(!copy.is_shared());
    assert!(copy.is_tainted(), "taint rides the copy");

    let view = parent.slice(10, 40_000).unwrap();
    assert!(view.is_tainted(), "taint rides the view");
    assert_eq!(view, copy);
}

// ── Stage 4: the view-equals-copy oracle (§2.2.15) ────────────
/// Every consumer surface must be unable to tell a view from its copy: same value, same answers, same renderings.
fn view_copy_oracle(parent: &PString, offset: usize, len: usize, tag: &str) {
    let view = parent.slice(offset, len).unwrap();
    let copy = parent.substr(offset, len).unwrap();

    assert_eq!(view, copy, "{tag}: eq");
    assert_eq!(copy, view, "{tag}: eq is symmetric");
    assert_eq!(view.cmp(&copy), std::cmp::Ordering::Equal, "{tag}: cmp");
    assert_eq!(hash_of(&view), hash_of(&copy), "{tag}: hash");
    assert_eq!(view.len(), copy.len(), "{tag}: len");
    assert_eq!(view.char_len(), copy.char_len(), "{tag}: char_len");
    assert_eq!(view.is_ascii(), copy.is_ascii(), "{tag}: is_ascii");
    assert_eq!(view.is_perl_utf8_valid(), copy.is_perl_utf8_valid(), "{tag}: is_perl_utf8_valid");

    let mut sa = [0u8; DECODE_MAX];
    let mut sb = [0u8; DECODE_MAX];
    assert_eq!(view.as_bytes(&mut sa), copy.as_bytes(&mut sb), "{tag}: as_bytes");
    assert_eq!(view.as_str(&mut sa).is_some(), copy.as_str(&mut sb).is_some(), "{tag}: as_str presence");
    assert_eq!(view.as_str(&mut sa), copy.as_str(&mut sb), "{tag}: as_str");
    assert_eq!(format!("{view}"), format!("{copy}"), "{tag}: Display");
    assert_eq!(format!("{:?}", ContentDebug(&view)), format!("{:?}", ContentDebug(&copy)), "{tag}: Debug content");

    let (dv, dc) = (view.downgraded().unwrap(), copy.downgraded().unwrap());
    assert_eq!(dv.is_some(), dc.is_some(), "{tag}: downgrade presence");
    if let (Some(a), Some(b)) = (dv, dc) {
        assert_eq!(a, b, "{tag}: downgrade value");
    }
}

/// Random content mixing ASCII runs, well-formed multibyte, and raw high bytes, so cuts land clean and dirty alike.
fn oracle_content(seed: &mut u64, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len + 4);
    while v.len() < len {
        match splitmix(seed) % 5 {
            0 => v.extend_from_slice(&[0xC3, 0xA9]),
            1 => v.push((splitmix(seed) % 0x80) as u8),
            2 => v.extend_from_slice("汉".as_bytes()),
            3 => v.push(0x80 | (splitmix(seed) & 0x3F) as u8),
            _ => v.extend_from_slice(b"plain"),
        }
    }

    v.truncate(len);
    v
}

#[test]
fn the_oracle_holds_across_the_native_families() {
    let mut seed = 0x04AC_1E01_u64;
    for (iterations, min, spread, label) in [(150u32, 40usize, 200usize, "heap8"), (100, 300, 50_000, "heap16"), (60, 70_000, 30_000, "heap32")] {
        for i in 0..iterations {
            let total = min + (splitmix(&mut seed) as usize % spread);
            let mut parent = PString::from_bytes(oracle_content(&mut seed, total)).unwrap();
            if splitmix(&mut seed).is_multiple_of(2) {
                parent.set_utf8_for_test();
            }

            let offset = splitmix(&mut seed) as usize % total;
            let len = splitmix(&mut seed) as usize % (total - offset + 1);
            view_copy_oracle(&parent, offset, len, &format!("{label}[{i}] {offset}+{len}/{total}"));
        }
    }
}

#[test]
fn the_oracle_holds_across_the_adopted_families_and_composition() {
    let mut seed = 0xADD_04AC_1E02_u64;
    let total = 200_000usize;
    let whole = PString::adopted_whole(cow_buffer::Adopted::adopt_vec(oracle_content(&mut seed, total), scan::Unknown).unwrap(), false, false);

    for i in 0..100u32 {
        let offset = splitmix(&mut seed) as usize % total;
        let len = splitmix(&mut seed) as usize % (total - offset + 1);
        view_copy_oracle(&whole, offset, len, &format!("adopted[{i}] {offset}+{len}"));
    }

    // Composition: a random view re-sliced, oracled against the whole at composed coordinates.
    for i in 0..40u32 {
        let offset = splitmix(&mut seed) as usize % (total / 2);
        let len = total / 4 + (splitmix(&mut seed) as usize % (total / 4));
        let view = whole.slice(offset, len).unwrap();
        let sub_off = splitmix(&mut seed) as usize % len;
        let sub_len = splitmix(&mut seed) as usize % (len - sub_off + 1);
        assert_eq!(view.slice(sub_off, sub_len).unwrap(), whole.substr(offset + sub_off, sub_len).unwrap(), "composed[{i}]");
    }
}

#[test]
fn the_oracle_holds_at_far_offsets_and_the_large_case() {
    let mut seed = 0xFA20_u64;
    let total = 17 * 1024 * 1024;
    let parent = PString::from_bytes(oracle_content(&mut seed, total)).unwrap();
    assert_eq!(parent.storage_type(), StorageType::Heap32);

    for i in 0..12u32 {
        let offset = 16_800_000 + (splitmix(&mut seed) as usize % 100_000);
        let len = splitmix(&mut seed) as usize % 60_000;
        view_copy_oracle(&parent, offset, len, &format!("far[{i}]"));
    }

    view_copy_oracle(&parent, 100, 16_900_000, "large slice");
}

#[test]
fn whole_object_views_read_and_fill_the_struct_cache() {
    let a = cow_buffer::Adopted::adopt_vec("héllo wörld, plainly mixed".into(), scan::Unknown).unwrap();
    let raw = a;
    let whole = PString::adopted_whole(a, true, false);

    // SAFETY: `whole` holds a reference on the struct for these reads.
    unsafe {
        assert_eq!(raw.as_ref().char_count(), 0, "unfilled at birth");
        assert_eq!(whole.char_len(), Some(26));
        assert_eq!(raw.as_ref().char_count(), 26, "the derivation filled the whole-object cache");
        assert_ne!(raw.as_ref().scan(), scan::Unknown, "the classification narrowed the shared slot");
        assert_eq!(whole.char_len(), Some(26), "the second ask is the cache read");
    }
}

// ── The packed-UUID codec (§2.2.16) ──────────────────────────
fn round_trip(s: &str) -> (UuidForm, [u8; 15]) {
    let (form, payload) = classify_uuid(s.as_bytes()).unwrap();
    let mut out = [0u8; UUID_LEN];
    assert_eq!(decode_uuid(form, &payload, &mut out), UUID_LEN);
    assert_eq!(std::str::from_utf8(&out).unwrap(), s, "round trip must be exact");

    (form, payload)
}

#[test]
fn every_recognized_form_round_trips() {
    let (f, _) = round_trip("f47ac10b-58cc-4372-a567-0e02b2c3d479");
    assert_eq!(f, UuidForm::V4S2, "variant digit a is data bits 10");

    let (f, _) = round_trip("017f22e2-79b0-7cc3-98c4-dc0c0c07398f");
    assert_eq!(f, UuidForm::V7);

    let (f, _) = round_trip("2f1a0e9c-5d1b-11ee-8c99-0242ac120002");
    assert_eq!(f, UuidForm::V1);

    let (f, _) = round_trip("1ee5d1b2-f1a0-6e9c-8c99-0242ac120002");
    assert_eq!(f, UuidForm::V6);

    let (f, _) = round_trip("a3bb189e-8bf9-3888-9912-ace4e6543002");
    assert_eq!(f, UuidForm::V3S1);

    let (f, _) = round_trip("74738ff5-5367-5958-9aee-98fffdcd1876");
    assert_eq!(f, UuidForm::V5S1);
}

#[test]
fn every_shard_and_every_variant_digit_survives() {
    for (digit, shard) in [(b'8', UuidForm::V4S0), (b'9', UuidForm::V4S1), (b'a', UuidForm::V4S2), (b'b', UuidForm::V4S3)] {
        let s = format!("f47ac10b-58cc-4372-{}567-0e02b2c3d479", digit as char);
        let (form, payload) = classify_uuid(s.as_bytes()).unwrap();
        assert_eq!(form, shard);

        let mut out = [0u8; UUID_LEN];
        decode_uuid(form, &payload, &mut out);
        assert_eq!(out[19], digit, "the variant digit is the shard's to restore");
    }
}

#[test]
fn the_time_ranges_gate_exactly() {
    // v7: the top two bits of the first digit are the range.  3 = 0b0011 passes; 4 = 0b0100 spills.
    assert!(classify_uuid(b"3fffffff-ffff-7fff-bfff-ffffffffffff").is_some());
    assert!(classify_uuid(b"4fffffff-ffff-7fff-bfff-ffffffffffff").is_none(), "past roughly 4199: spills");

    // v1: digit 13 carries the range, the timestamp running low-first.
    assert!(classify_uuid(b"2f1a0e9c-5d1b-13ee-8c99-0242ac120002").is_some());
    assert!(classify_uuid(b"2f1a0e9c-5d1b-14ee-8c99-0242ac120002").is_none(), "past roughly 2496: spills");

    // v6 reorders most-significant-first: digit 0 carries the same range.
    assert!(classify_uuid(b"3ee5d1b2-f1a0-6e9c-8c99-0242ac120002").is_some());
    assert!(classify_uuid(b"4ee5d1b2-f1a0-6e9c-8c99-0242ac120002").is_none(), "the same 2496 gate, at v6's digit");
}

#[test]
fn everything_noncanonical_spills() {
    assert!(classify_uuid(b"F47AC10B-58CC-4372-A567-0E02B2C3D479").is_none(), "uppercase: initial scope is lowercase");
    assert!(classify_uuid(b"f47ac10b-58cc-4372-A567-0e02b2c3d479").is_none(), "mixed case");
    assert!(classify_uuid(b"f47ac10b_58cc_4372_a567_0e02b2c3d479").is_none(), "wrong separators");
    assert!(classify_uuid(b"f47ac10b58cc4372a5670e02b2c3d479").is_none(), "unhyphenated: recorded candidate, not this form");
    assert!(classify_uuid(b"{f47ac10b-58cc-4372-a567-0e02b2c3d479").is_none(), "braces");
    assert!(classify_uuid(b"f47ac10b-58cc-2372-a567-0e02b2c3d479").is_none(), "unrecognized version");
    assert!(classify_uuid(b"f47ac10b-58cc-4372-c567-0e02b2c3d479").is_none(), "non-RFC variant: legacy layouts spill");
    assert!(classify_uuid(b"f47ac10b-58cc-4372-a567-0e02b2c3d47").is_none(), "wrong length");
}

#[test]
fn payloads_of_distinct_uuids_differ_and_zero_nothing_silently() {
    let (_, a) = classify_uuid(b"f47ac10b-58cc-4372-a567-0e02b2c3d479").unwrap();
    let (_, b) = classify_uuid(b"f47ac10b-58cc-4372-a567-0e02b2c3d478").unwrap();
    assert_ne!(a, b);
}

// ── The packed-UUID family (§2.2.16) ──────────────────────────
#[test]
fn uuid_spellings_pack_through_the_selector() {
    for (s, ty) in [
        ("f47ac10b-58cc-4372-a567-0e02b2c3d479", StorageType::PackedUuidV4S2),
        ("017f22e2-79b0-7cc3-98c4-dc0c0c07398f", StorageType::PackedUuidV7),
        ("2f1a0e9c-5d1b-11ee-8c99-0242ac120002", StorageType::PackedUuidV1),
        ("3ee5d1b2-f1a0-6e9c-8c99-0242ac120002", StorageType::PackedUuidV6),
        ("a3bb189e-8bf9-3888-9912-ace4e6543002", StorageType::PackedUuidV3S1),
        ("74738ff5-5367-5958-9aee-98fffdcd1876", StorageType::PackedUuidV5S1),
    ] {
        let p = PString::from_bytes(s.as_bytes()).unwrap();
        assert_eq!(p.storage_type(), ty, "{s}");
        assert_eq!(p.len(), 36);
        assert_eq!(p.char_len(), Some(36));
        assert!(p.is_ascii());
        assert!(p.is_perl_utf8_valid());
        assert!(!p.is_shared());

        let mut sc = [0u8; DECODE_MAX];
        assert_eq!(p.as_bytes(&mut sc), s.as_bytes());
        assert_eq!(p.as_str(&mut sc), Some(s));
        assert_eq!(format!("{p}"), s);
    }

    // A spelling outside every pattern takes ordinary storage: capacity, never semantics.
    let spilled = PString::from_bytes(*b"F47AC10B-58CC-4372-A567-0E02B2C3D479").unwrap();
    assert_eq!(spilled.storage_type(), StorageType::Heap8Ascii, "uppercase spills until evidence rules it in");
}

#[test]
fn packed_uuids_equal_their_heap_spelling_on_every_surface() {
    let s = b"f47ac10b-58cc-4372-a567-0e02b2c3d479";
    let packed = PString::from_bytes(*s).unwrap();
    assert_eq!(packed.storage_type(), StorageType::PackedUuidV4S2);

    // A heap twin of the same spelling: build past the band, append the tail — heap never demotes.
    let mut heap = PString::from_bytes(&s[..35]).unwrap();
    assert_eq!(heap.storage_type(), StorageType::Heap8Ascii);
    heap.push_bytes(&s[35..]).unwrap();
    assert!(matches!(heap.storage_type(), StorageType::Heap8 | StorageType::Heap8Ascii), "no demotion");

    assert_eq!(packed, heap);
    assert_eq!(heap, packed);
    assert_eq!(packed.cmp(&heap), std::cmp::Ordering::Equal);
    assert_eq!(hash_of(&packed), hash_of(&heap));
    assert_eq!(format!("{packed}"), format!("{heap}"));
    assert_eq!(format!("{:?}", ContentDebug(&packed)), format!("{:?}", ContentDebug(&heap)));
}

#[test]
fn uuid_flags_ride_the_twins_and_the_payload_stays() {
    let s = b"017f22e2-79b0-7cc3-98c4-dc0c0c07398f";
    let mut p = PString::from_bytes(*s).unwrap();
    p.taint();
    assert!(p.is_tainted());
    assert_eq!(p.storage_type(), StorageType::PackedUuidV7, "the twin switch leaves the storage type");

    p.set_utf8_for_test();
    assert!(p.is_utf8());
    assert_eq!(p.storage_type(), StorageType::PackedUuidV7);

    let mut sc = [0u8; DECODE_MAX];
    assert_eq!(p.as_bytes(&mut sc), s, "flags never touch the payload");

    let down = p.downgraded().unwrap().unwrap();
    assert!(!down.is_utf8());
    assert!(down.is_tainted(), "the downgrade drops the flag and keeps the taint");
    assert_eq!(down, p);
}

#[test]
fn uuid_slices_copy_and_appends_exit_or_complete() {
    let s = b"f47ac10b-58cc-4372-a567-0e02b2c3d479";
    let p = PString::from_bytes(*s).unwrap();

    let head = p.slice(0, 8).unwrap();
    assert_eq!(head, p.substr(0, 8).unwrap());
    let mid = p.slice(9, 20).unwrap();
    assert!(
        !matches!(mid.storage_type(), StorageType::SmallSlice | StorageType::MediumSlice | StorageType::FarSlice),
        "an envelope-resident source has no buffer to share"
    );
    assert_eq!(mid, p.substr(9, 20).unwrap());

    // An append leaves the spelling: the value exits to the heap with the joined content.
    let mut q = p.clone();
    q.push_bytes(b"!").unwrap();
    assert_eq!(q.len(), 37);
    let mut sa = [0u8; DECODE_MAX];
    assert_eq!(&q.as_bytes(&mut sa)[..36], s);

    // And an append that completes a spelling packs it: the combined attempt runs the same ladder.
    let mut r = PString::from_bytes(&s[..12]).unwrap();
    assert!(format!("{:?}", r.storage_type()).starts_with("Inline"));
    r.push_bytes(&s[12..]).unwrap();
    assert_eq!(r.storage_type(), StorageType::PackedUuidV4S2, "the ladder recognizes the completed spelling");
    assert_eq!(r, p);
}
