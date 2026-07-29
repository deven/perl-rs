//! Nibble-packed digit-dense strings (§2.2.9): two characters per byte over 16-symbol alphabets.
//!
//! Strings drawn from a 16-symbol alphabet pack two characters per byte, raising the inline capacity for the
//! digit-dense class — timestamps, IPs, numeric IDs, and every numeric stringification the interpreter can produce — to
//! `MAX_PACKED_LEN` (28) characters inside the 16-byte envelope.  Three alphabets are defined, selected by format bits
//! in the enclosing discriminant:
//!
//! - **Numeric**: space, `+`, `-`, `.`, `0`-`9`, `E`, `e` — covers every `%.15g` float output and every `i64`
//!   stringification (§2.2.3's 22-character bound), and both exponent spellings: perl emits lowercase by default but
//!   uppercase through `%E` and `%G`, and accepts either on numification.
//! - **DateTimeZ**: space, `-`, `.`, `0`-`9`, `:`, `T`, `Z` — ISO timestamps in Zulu form, and those carrying a
//!   `-hh:mm` offset (the minus sign is shared with the date separators).
//! - **DateTimePlus**: space, `+`, `-`, `.`, `0`-`9`, `:`, `T` — ISO timestamps carrying a `+hh:mm` offset.  A valid
//!   timestamp never needs `Z` and `+` together (Zulu *is* the zero offset), so splitting the two spellings across two
//!   alphabets covers the whole grammar without a 17th symbol.
//!
//! All three alphabets are exactly full at sixteen symbols: the `T`/space date-time separator, both offset spellings,
//! and both exponent spellings are therefore all encodable, with no nibble left over.
//!
//! **The space is nibble 0, shared with the padding, disambiguated by position**: trailing zero nibbles are padding,
//! interior zero nibbles are spaces.  A string with a *trailing* space is consequently not packable at all — its final
//! space would be indistinguishable from padding — and `pack` rejects it.  This costs nothing that was previously
//! available (such strings were unpackable when the space was outside the alphabets), and it buys the space for free
//! because nibble 0 was already reserved.  ASCII space is 0x20, below every other symbol in every alphabet, so nibble 0
//! remains the least code and comparison is unaffected.
//!
//! Nibble values are assigned in ASCII order, packed high-nibble first, and the tail pad-filled — so for two packed
//! strings of the *same* alphabet, comparing the nibble arrays as plain bytes gives exactly the raw strings' byte order
//! (verified exhaustively by the order property test).  Prefix ordering survives the shared zero: where the shorter
//! string has padding and the longer has a space, both nibbles are 0 and the comparison simply continues, and because
//! trailing spaces are excluded the longer string must reach a non-space — hence a nonzero nibble — before it ends.
//! Cross-alphabet *ordering* decodes; cross-alphabet *equality* is decided by the alphabets alone, since deterministic
//! classification maps each byte string to exactly one alphabet.
//!
//! Packing is an **encoding, never a canonicalization**: `unpack(pack(s)) == s` exactly, byte for byte, for every
//! accepted input — the §2.2.9 observational-identity obligation starts here.  Classification is deterministic so that
//! equal byte contents always take equal representations (a prerequisite for packed `eq` via `memcmp`): the alphabets
//! are tried in the fixed priority order Numeric, DateTimeZ, DateTimePlus, and the first feasible one wins.

// The production consumers arrive with the step-9 PerlString and Value rework; the expect self-reports for removal the
// moment they land.
#![cfg_attr(not(test), expect(dead_code))]

/// The packed-tier capacity in characters: 14 nibble bytes, two characters each.
pub(crate) const MAX_PACKED_LEN: usize = 28;

/// The nibble-array width in bytes.
pub(crate) const PACKED_BYTES: usize = MAX_PACKED_LEN / 2;

/// Which 16-symbol alphabet a packed string uses.  The discriminant value is the format bits stored in the enclosing
/// enum's encoding — two bits, which with the 5-bit length still fits one byte beside the 14 nibble bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PackedAlphabet {
    /// space `+` `-` `.` `0`-`9` `E` `e` — every numeric stringification fits here, in either exponent spelling.
    Numeric = 0,

    /// space `-` `.` `0`-`9` `:` `T` `Z` — Zulu-form and `-hh:mm`-offset ISO timestamps.
    DateTimeZ = 1,

    /// space `+` `-` `.` `0`-`9` `:` `T` — `+hh:mm`-offset ISO timestamps.
    DateTimePlus = 2,
}

/// A packed string: the alphabet, the character count, and the pad-filled nibble array.
///
/// `len` is stored for O(1) length, not because the information is otherwise lost: since trailing spaces are rejected,
/// the last character's nibble is nonzero, so the length is recoverable as one past the last nonzero nibble.  (The
/// forward scan that served before the space joined the alphabets no longer works — an interior space is a zero nibble.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Packed {
    pub(crate) alphabet: PackedAlphabet,
    pub(crate) len: u8,
    pub(crate) nibbles: [u8; PACKED_BYTES],
}

/// Sentinel in the byte→nibble tables: this byte is outside the alphabet.
const INVALID: u8 = 0xFF;

/// Build a byte→nibble table from an ASCII-ordered symbol list.  The list *starts* with the space, so the space takes
/// nibble 0 — the same value the unwritten tail carries as padding.
const fn encode_table(symbols: &[u8]) -> [u8; 256] {
    let mut table = [INVALID; 256];
    let mut i = 0;
    while i < symbols.len() {
        table[symbols[i] as usize] = i as u8;
        i += 1;
    }

    table
}

/// Build the nibble→byte table.  Index 0 decodes to the space; positions past the alphabet are never reached, because
/// decoding is bounded by the stored length and packing only ever emits in-alphabet nibbles.
const fn decode_table(symbols: &[u8]) -> [u8; 16] {
    let mut table = [0u8; 16];
    let mut i = 0;
    while i < symbols.len() {
        table[i] = symbols[i];
        i += 1;
    }

    table
}

// ASCII-ordered symbol lists, space first.  Order is load-bearing: monotone nibble assignment is what makes
// same-alphabet packed comparison agree with raw byte comparison.
const NUMERIC_SYMBOLS: &[u8] = b" +-.0123456789Ee";
const DATETIME_Z_SYMBOLS: &[u8] = b" -.0123456789:TZ";
const DATETIME_PLUS_SYMBOLS: &[u8] = b" +-.0123456789:T";

const NUMERIC_ENCODE: [u8; 256] = encode_table(NUMERIC_SYMBOLS);
const NUMERIC_DECODE: [u8; 16] = decode_table(NUMERIC_SYMBOLS);
const DATETIME_Z_ENCODE: [u8; 256] = encode_table(DATETIME_Z_SYMBOLS);
const DATETIME_Z_DECODE: [u8; 16] = decode_table(DATETIME_Z_SYMBOLS);
const DATETIME_PLUS_ENCODE: [u8; 256] = encode_table(DATETIME_PLUS_SYMBOLS);
const DATETIME_PLUS_DECODE: [u8; 16] = decode_table(DATETIME_PLUS_SYMBOLS);

const _: () = assert!(NUMERIC_SYMBOLS.len() == 16); // Exactly full.
const _: () = assert!(DATETIME_Z_SYMBOLS.len() == 16); // Exactly full.
const _: () = assert!(DATETIME_PLUS_SYMBOLS.len() == 16); // Exactly full.

impl PackedAlphabet {
    fn encode_table(self) -> &'static [u8; 256] {
        match self {
            PackedAlphabet::Numeric => &NUMERIC_ENCODE,
            PackedAlphabet::DateTimeZ => &DATETIME_Z_ENCODE,
            PackedAlphabet::DateTimePlus => &DATETIME_PLUS_ENCODE,
        }
    }

    fn decode_table(self) -> &'static [u8; 16] {
        match self {
            PackedAlphabet::Numeric => &NUMERIC_DECODE,
            PackedAlphabet::DateTimeZ => &DATETIME_Z_DECODE,
            PackedAlphabet::DateTimePlus => &DATETIME_PLUS_DECODE,
        }
    }
}

/// Classify and pack, or report that the bytes are not packable: too long, ending in a space, or containing a byte that
/// falls outside every alphabet.  Deterministic — the alphabets are tried in a fixed priority order.
pub(crate) fn pack(bytes: &[u8]) -> Option<Packed> {
    if bytes.len() > MAX_PACKED_LEN || bytes.last() == Some(&b' ') {
        return None; // Trailing spaces cannot be distinguished from padding.
    }

    // One pass tracking feasibility in every alphabet; fail fast when none survives.  All must be tracked for every
    // byte: 'e' passes only Numeric and 'Z' only DateTimeZ, so a later byte must not select an alphabet an earlier byte
    // already ruled out.
    let mut numeric = true;
    let mut datetime_z = true;
    let mut datetime_plus = true;
    for &b in bytes {
        numeric &= NUMERIC_ENCODE[b as usize] != INVALID;
        datetime_z &= DATETIME_Z_ENCODE[b as usize] != INVALID;
        datetime_plus &= DATETIME_PLUS_ENCODE[b as usize] != INVALID;
        if !numeric && !datetime_z && !datetime_plus {
            return None;
        }
    }

    // The priority order is the determinism rule: equal byte contents must always take equal representations.
    let alphabet = if numeric {
        PackedAlphabet::Numeric
    } else if datetime_z {
        PackedAlphabet::DateTimeZ
    } else {
        PackedAlphabet::DateTimePlus
    };

    let table = alphabet.encode_table();
    let mut nibbles = [0u8; PACKED_BYTES];
    for (i, &b) in bytes.iter().enumerate() {
        let n = table[b as usize];
        debug_assert_ne!(n, INVALID, "feasibility pass admitted an out-of-alphabet byte");
        // High nibble first: byte order over the packed array mirrors character order.
        if i % 2 == 0 {
            nibbles[i / 2] = n << 4;
        } else {
            nibbles[i / 2] |= n;
        }
    }

    // The sole silent truncation risk is the length; checked above, so the cast is exact.
    Some(Packed { alphabet, len: bytes.len() as u8, nibbles })
}

impl Packed {
    /// Decode to raw bytes: the exact original, by the round-trip invariant.  Interior zero nibbles decode to spaces;
    /// the padding is never reached because the walk is bounded by the stored length.
    pub(crate) fn unpack(&self) -> ([u8; MAX_PACKED_LEN], usize) {
        let table = self.alphabet.decode_table();
        let mut out = [0u8; MAX_PACKED_LEN];
        let len = self.len as usize;
        for (i, slot) in out.iter_mut().enumerate().take(len) {
            let byte = self.nibbles[i / 2];
            let n = if i % 2 == 0 { byte >> 4 } else { byte & 0x0F };
            *slot = table[n as usize];
        }

        (out, len)
    }

    /// Same-alphabet comparison on the packed representation: equals raw byte order.  The ASCII-order nibble assignment
    /// handles symbol-versus-symbol, and the shared zero handles length: a padded slot ties against an interior space
    /// and the comparison continues, while against any other symbol the pad is lower — prefix-first, exactly as byte
    /// comparison orders a prefix before its extension.  Callers must decode for cross-alphabet ordering; asserting
    /// here keeps that contract loud.
    pub(crate) fn cmp_same_alphabet(&self, other: &Packed) -> std::cmp::Ordering {
        debug_assert_eq!(self.alphabet, other.alphabet, "cross-alphabet packed ordering must decode");
        self.nibbles.cmp(&other.nibbles)
    }
}

// ── Tests ─────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(s: &[u8]) -> Packed {
        let p = pack(s).unwrap();
        let (out, len) = p.unpack();
        assert_eq!(&out[..len], s, "round-trip must be exact: {:?}", String::from_utf8_lossy(s));

        p
    }

    #[test]
    fn exact_round_trip_across_the_class() {
        // Every %.15g shape, i64 extremes, timestamps, IPs, versions — the §2.2.9 flagship citizens.
        for s in [
            &b""[..],
            b"0",
            b"0.3",
            b"0.333333333333333",
            b"-2.22507385850720e-308",
            b"1e+100",
            b"1E5",
            b"1.5E+10",
            b"1.000000E+00", // perl's %E.
            b"1E+100",       // perl's %G.
            b"9223372036854775807",
            b"-9223372036854775808",
            b"2026-07-28T14:33:07Z",
            b"2026-07-28T14:33:07.123Z",
            b"192.168.100.200",
            b"1.2.3",
            b"12:34:56",
        ] {
            roundtrip(s);
        }
    }

    #[test]
    fn interior_spaces_pack_and_trailing_spaces_do_not() {
        // Interior spaces are nibble 0 and decode back to spaces.
        for s in [
            &b" "[..], // leading-only: a lone space *is* trailing, so it must be rejected below.
            b"1 2",
            b" 12",
            b"12 34",
            b"1 234 567",
            b"555 1234",
            b"2026-07-28 14:33:07Z",
            b"2026-07-28 14:33:07+05:00",
            b"2026-07-28 14:33:07",
        ] {
            if s.last() == Some(&b' ') {
                assert_eq!(pack(s), None);
            } else {
                roundtrip(s);
            }
        }

        // Trailing spaces are unpackable: the final space cannot be told from padding.
        assert_eq!(pack(b" "), None);
        assert_eq!(pack(b"1 "), None);
        assert_eq!(pack(b"   "), None);
        assert_eq!(pack(b"2026-07-28 "), None);
        assert_eq!(pack(b"1 2 "), None);
    }

    #[test]
    fn iso_timestamp_grammar_is_covered() {
        // Both offset spellings, both date-time separators, both alphabets.
        for s in [
            &b"2026-07-28T14:33:07Z"[..],
            b"2026-07-28T14:33:07+05:00",
            b"2026-07-28T14:33:07-05:00",
            b"2026-07-28 14:33:07+00:00",
            b"2026-07-28T14:33:07.123456Z", // Microseconds, Zulu.
            b"2026-07-28T14:33:07.12+05:00",
            b"20260728T143307Z",
            b"2026-07-28",
            b"14:33:07.123",
        ] {
            roundtrip(s);
        }

        // The capacity boundary, exactly: Zulu leaves room for seven fractional digits, a numeric offset for two.
        assert_eq!(b"2026-07-28T14:33:07.1234567Z".len(), MAX_PACKED_LEN);
        roundtrip(b"2026-07-28T14:33:07.1234567Z");
        assert_eq!(pack(b"2026-07-28T14:33:07.12345678Z"), None); // 29 characters.
        assert_eq!(b"2026-07-28T14:33:07.12+05:00".len(), MAX_PACKED_LEN);
        roundtrip(b"2026-07-28T14:33:07.12+05:00");
        assert_eq!(pack(b"2026-07-28T14:33:07.123+05:00"), None); // 29 characters.
    }

    #[test]
    fn alphabet_selection_is_deterministic() {
        // Numeric wins every tie — including strings that also fit both date-time alphabets.
        assert_eq!(roundtrip(b"2026-07-28").alphabet, PackedAlphabet::Numeric);
        assert_eq!(roundtrip(b"3.14").alphabet, PackedAlphabet::Numeric);
        assert_eq!(roundtrip(b"12 34").alphabet, PackedAlphabet::Numeric);
        assert_eq!(roundtrip(b"").alphabet, PackedAlphabet::Numeric);

        // DateTimeZ takes the strings needing ':' or 'T' but not '+', including '-' offsets and the Zulu spelling.
        assert_eq!(roundtrip(b"12:34").alphabet, PackedAlphabet::DateTimeZ);
        assert_eq!(roundtrip(b"2026-07-28T14:33:07").alphabet, PackedAlphabet::DateTimeZ);
        assert_eq!(roundtrip(b"2026-07-28T14:33:07Z").alphabet, PackedAlphabet::DateTimeZ);
        assert_eq!(roundtrip(b"2026-07-28T14:33:07-05:00").alphabet, PackedAlphabet::DateTimeZ);

        // DateTimePlus is reached exactly when '+' appears alongside a date-time-only symbol.
        assert_eq!(roundtrip(b"2026-07-28T14:33:07+05:00").alphabet, PackedAlphabet::DateTimePlus);
        assert_eq!(roundtrip(b"14:33+01:00").alphabet, PackedAlphabet::DateTimePlus);

        // Both exponent spellings are Numeric-only, as 'Z' is DateTimeZ-only: together they fit nothing.
        assert_eq!(roundtrip(b"1E9").alphabet, PackedAlphabet::Numeric);
        assert_eq!(pack(b"1e+9T"), None);
        assert_eq!(pack(b"1E+9T"), None);
        assert_eq!(pack(b"1e9Z"), None);
        assert_eq!(pack(b"1E9Z"), None);
        assert_eq!(pack(b"Z+"), None);
    }

    #[test]
    fn boundaries_and_rejections() {
        assert!(pack(&[b'1'; 27]).is_some());
        assert!(pack(&[b'1'; 28]).is_some());
        assert_eq!(pack(&[b'1'; 29]), None, "over capacity");
        assert_eq!(pack(b"abc"), None);
        assert_eq!(pack(b"1,234"), None, "the comma is in no alphabet");
        assert_eq!(pack(b"1\t2"), None, "only the space is whitespace-encodable");
    }

    #[test]
    fn packed_order_equals_raw_order() {
        // Exhaustive over generated same-alphabet corpora, prefix pairs, interior spaces, and equals included.
        let numeric: Vec<&[u8]> = vec![
            b"",
            b" 1",
            b"+",
            b"-",
            b".",
            b"0",
            b"1",
            b"12",
            b"120",
            b"123",
            b"12.",
            b"1 2",
            b"1 20",
            b"1 3",
            b"1e9",
            b"2",
            b"9",
            b"-1",
            b"-9",
            b"0.1",
            b"0.10",
            b"1e+9",
            b"1e-9",
            b"99999999999999999999999999",
            b"1E9",
            b"1E+9",
            b"1E-9",
            b"E",
            b"e",
        ];

        for a in &numeric {
            for b in &numeric {
                let (pa, pb) = (pack(a).unwrap(), pack(b).unwrap());
                assert_eq!(
                    pa.cmp_same_alphabet(&pb),
                    a.cmp(b),
                    "order property violated for {:?} vs {:?}",
                    String::from_utf8_lossy(a),
                    String::from_utf8_lossy(b)
                );
            }
        }

        let datetimes: Vec<&[u8]> = vec![
            b"",
            b"2026-07-28T14:33:07Z",
            b"2026-07-28T14:33:07.123Z",
            b"2026-07-28T14:33:08Z",
            b"2026-07-28 14:33:07Z",
            b"2025-12-31T23:59:59Z",
            b"12:34",
            b"12:34:56",
            b"T",
            b"Z",
            b"::",
            b"1 2:3",
            b"2026-07-28T14:33:07+05:00",
            b"14:33+01:00",
            b"14:33+01:01",
        ];

        for a in &datetimes {
            for b in &datetimes {
                let (pa, pb) = (pack(a).unwrap(), pack(b).unwrap());
                if pa.alphabet == pb.alphabet {
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
        for (short, long) in [(&b"12"[..], &b"12 3"[..]), (b"12", b"12  3"), (b"1", b"1 2"), (b"", b" 1"), (b"2026-07-28", b"2026-07-28 1")] {
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

    #[test]
    fn length_is_recoverable_from_the_nibbles() {
        // Not relied on by the code (len is stored for O(1) access), but the property that makes storage redundant:
        // trailing spaces are rejected, so the last character's nibble is nonzero.
        for s in [&b""[..], b"1", b"1 2", b" 12", b"2026-07-28 14:33:07Z", &[b'1'; 28][..]] {
            let p = pack(s).unwrap();
            let derived = (0..MAX_PACKED_LEN)
                .rev()
                .find(|&i| {
                    let byte = p.nibbles[i / 2];
                    let n = if i % 2 == 0 { byte >> 4 } else { byte & 0x0F };
                    n != 0
                })
                .map_or(0, |i| i + 1);
            assert_eq!(derived, s.len(), "derived length must match for {:?}", String::from_utf8_lossy(s));
        }
    }
}
