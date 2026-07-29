//! Nibble-packed digit-dense strings (§2.2.9): two characters per byte over 16-symbol alphabets.
//!
//! Strings drawn from a 16-symbol alphabet pack two characters per byte, raising the inline capacity for the
//! digit-dense class — timestamps, IPs, numeric IDs, and every numeric stringification the interpreter can produce — to
//! `MAX_PACKED_LEN` (28) characters inside the 16-byte envelope.  Two alphabets are defined, selected by format bits in
//! the enclosing discriminant:
//!
//! - **Numeric**: pad, `+`, `-`, `.`, `0`-`9`, `e` — covers every `%.15g` float output and every `i64` stringification
//!   (§2.2.3's 22-character bound).  Fifteen symbols; nibble 15 is unused.
//! - **Datetime**: pad, `-`, `.`, `0`-`9`, `:`, `T`, `Z` — T-form ISO through millisecond precision.  Exactly sixteen
//!   symbols; space-separated timestamps take the raw or heap tier (the recorded 17th-symbol trade).
//!
//! Nibble values are assigned in ASCII order with the pad below every symbol, packed high-nibble first, and the tail
//! pad-filled — so for two packed strings of the *same* alphabet, comparing the nibble arrays as plain bytes gives
//! exactly the raw strings' byte order (verified exhaustively by the order property test).  Cross-alphabet comparison
//! decodes.
//!
//! Packing is an **encoding, never a canonicalization**: `unpack(pack(s)) == s` exactly, byte for byte, for every
//! accepted input — the §2.2.9 observational-identity obligation starts here.  Classification is deterministic so that
//! equal byte contents always take equal representations (a prerequisite for packed `eq` via `memcmp`): strings
//! feasible in both alphabets — pure digits, or digits with `-`/`.` — always classify as Numeric; Datetime is chosen
//! exactly when a byte outside the numeric alphabet appears (`:`, `T`, `Z`).

// The production consumers arrive with the step-9 PerlString and Value rework; the expect self-reports for removal the
// moment they land.
#![cfg_attr(not(test), expect(dead_code))]

/// The packed-tier capacity in characters: 14 nibble bytes, two characters each.
pub(crate) const MAX_PACKED_LEN: usize = 28;

/// The nibble-array width in bytes.
pub(crate) const PACKED_BYTES: usize = MAX_PACKED_LEN / 2;

/// Which 16-symbol alphabet a packed string uses.  The discriminant value is the format bit stored in the enclosing
/// enum's encoding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PackedAlphabet {
    /// pad `+` `-` `.` `0`-`9` `e` — every numeric stringification fits here.
    Numeric = 0,

    /// pad `-` `.` `0`-`9` `:` `T` `Z` — T-form ISO datetimes.
    DateTime = 1,
}

/// A packed string: the alphabet, the character count, and the pad-filled nibble array.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Packed {
    pub(crate) alphabet: PackedAlphabet,
    pub(crate) len: u8,
    pub(crate) nibbles: [u8; PACKED_BYTES],
}

/// Sentinel in the byte→nibble tables: this byte is outside the alphabet.
const INVALID: u8 = 0xFF;

/// Build a byte→nibble table from an ASCII-ordered symbol list (pad excluded; nibble 0 is the pad).
const fn to_nibble_table(symbols: &[u8]) -> [u8; 256] {
    let mut table = [INVALID; 256];
    let mut i = 0;
    while i < symbols.len() {
        table[symbols[i] as usize] = (i + 1) as u8; // Nibble 0 is the pad; symbols start at 1.
        i += 1;
    }

    table
}

/// Build the nibble→byte table (index 0 = pad, mapped to 0 and never emitted for in-range lengths).
const fn from_nibble_table(symbols: &[u8]) -> [u8; 16] {
    let mut table = [0u8; 16];
    let mut i = 0;
    while i < symbols.len() {
        table[i + 1] = symbols[i];
        i += 1;
    }

    table
}

// ASCII-ordered symbol lists.  Order is load-bearing: monotone nibble assignment is what makes same-alphabet packed
// comparison agree with raw byte comparison.
const NUMERIC_SYMBOLS: &[u8] = b"+-.0123456789e";
const DATETIME_SYMBOLS: &[u8] = b"-.0123456789:TZ";

const NUMERIC_TO: [u8; 256] = to_nibble_table(NUMERIC_SYMBOLS);
const NUMERIC_FROM: [u8; 16] = from_nibble_table(NUMERIC_SYMBOLS);
const DATETIME_TO: [u8; 256] = to_nibble_table(DATETIME_SYMBOLS);
const DATETIME_FROM: [u8; 16] = from_nibble_table(DATETIME_SYMBOLS);

const _: () = assert!(NUMERIC_SYMBOLS.len() == 14); // Nibble 15 deliberately unused.
const _: () = assert!(DATETIME_SYMBOLS.len() == 15); // Exactly full: 15 symbols + pad.

impl PackedAlphabet {
    fn encode_table(self) -> &'static [u8; 256] {
        match self {
            PackedAlphabet::Numeric => &NUMERIC_TO,
            PackedAlphabet::DateTime => &DATETIME_TO,
        }
    }

    fn decode_table(self) -> &'static [u8; 16] {
        match self {
            PackedAlphabet::Numeric => &NUMERIC_FROM,
            PackedAlphabet::DateTime => &DATETIME_FROM,
        }
    }
}

/// Classify and pack, or report that the bytes are not packable (too long, or a byte falls outside both alphabets).
/// Deterministic: both-feasible inputs always classify Numeric.
pub(crate) fn pack(bytes: &[u8]) -> Option<Packed> {
    if bytes.len() > MAX_PACKED_LEN {
        return None;
    }

    // One pass tracking feasibility in both alphabets; fail fast when neither survives.  Both must be tracked for every
    // byte: 'e' and '+' pass numeric but not datetime, so a later ':'/'T'/'Z' must not select an alphabet an earlier
    // byte already ruled out.
    let mut numeric_ok = true;
    let mut datetime_ok = true;
    for &b in bytes {
        numeric_ok &= NUMERIC_TO[b as usize] != INVALID;
        datetime_ok &= DATETIME_TO[b as usize] != INVALID;
        if !numeric_ok && !datetime_ok {
            return None;
        }
    }

    let alphabet = if numeric_ok { PackedAlphabet::Numeric } else { PackedAlphabet::DateTime };
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

    // Sole silent truncation risk is the length; checked above, so the cast is exact.
    Some(Packed { alphabet, len: bytes.len() as u8, nibbles })
}

impl Packed {
    /// Decode to raw bytes: the exact original, by the round-trip invariant.
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

    /// Same-alphabet comparison on the packed representation: equals raw byte order (the ASCII-order nibble assignment
    /// plus below-everything pad make plain array comparison correct).  Callers must decode for cross-alphabet
    /// comparison; asserting here keeps that contract loud.
    pub(crate) fn cmp_same_alphabet(&self, other: &Packed) -> std::cmp::Ordering {
        debug_assert_eq!(self.alphabet, other.alphabet, "cross-alphabet packed comparison must decode");
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
    fn alphabet_selection_is_deterministic() {
        // Both-feasible inputs (digits, '-', '.') classify Numeric — always.
        assert_eq!(roundtrip(b"2026-07-28").alphabet, PackedAlphabet::Numeric);
        assert_eq!(roundtrip(b"3.14").alphabet, PackedAlphabet::Numeric);
        assert_eq!(roundtrip(b"").alphabet, PackedAlphabet::Numeric);

        // A datetime-only byte forces the datetime alphabet.
        assert_eq!(roundtrip(b"2026-07-28T00:00:00Z").alphabet, PackedAlphabet::DateTime);
        assert_eq!(roundtrip(b"12:34").alphabet, PackedAlphabet::DateTime);

        // 'e' and '+' are numeric-only: mixing them with datetime-only bytes is unpackable.
        assert_eq!(pack(b"1e+9T"), None);
    }

    #[test]
    fn boundaries_and_rejections() {
        assert!(pack(&[b'1'; 27]).is_some());
        assert!(pack(&[b'1'; 28]).is_some());
        assert_eq!(pack(&[b'1'; 29]), None, "over capacity");
        assert_eq!(pack(b"3.14 "), None, "space is in neither alphabet");
        assert_eq!(pack(b"abc"), None);
        assert_eq!(pack(b"2026-07-28 14:33"), None, "space-separated timestamps take another tier");
    }

    #[test]
    fn packed_order_equals_raw_order() {
        // Exhaustive over a generated same-alphabet corpus, prefix pairs and equals included.
        let numeric: Vec<&[u8]> = vec![
            b"",
            b"+",
            b"-",
            b".",
            b"0",
            b"1",
            b"12",
            b"120",
            b"123",
            b"12.",
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
        let datetime: Vec<&[u8]> = vec![
            b"",
            b"2026-07-28T14:33:07Z",
            b"2026-07-28T14:33:07.123Z",
            b"2026-07-28T14:33:08Z",
            b"2025-12-31T23:59:59Z",
            b"12:34",
            b"12:34:56",
            b"T",
            b"Z",
            b"::",
        ];
        for a in &datetime {
            for b in &datetime {
                let (pa, pb) = (pack(a).unwrap(), pack(b).unwrap());
                if pa.alphabet == pb.alphabet {
                    assert_eq!(pa.cmp_same_alphabet(&pb), a.cmp(b));
                }
            }
        }
    }

    #[test]
    fn every_symbol_at_every_position() {
        for &sym in NUMERIC_SYMBOLS.iter().chain(DATETIME_SYMBOLS) {
            for pos in 0..MAX_PACKED_LEN {
                let mut s = vec![b'0'; MAX_PACKED_LEN];
                s[pos] = sym;
                roundtrip(&s);
            }
        }
    }

    #[test]
    fn nibble_assignment_is_ascii_monotone() {
        // The order property's foundation, checked directly so a table edit cannot silently break it.
        for table in [&NUMERIC_TO, &DATETIME_TO] {
            let mut last = 0u8; // The pad sits below every symbol.
            for b in 0..=255u8 {
                let n = table[b as usize];
                if n != INVALID {
                    assert!(n > last, "nibble values must ascend with ASCII");
                    last = n;
                }
            }
        }
    }
}
