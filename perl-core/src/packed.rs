//! Nibble-packed digit-dense strings (§2.2.9): two characters per byte over 16-symbol alphabets.
//!
//! Strings drawn from a 16-symbol alphabet pack two characters per byte, raising the inline capacity for the
//! digit-dense class — timestamps, IPs, numeric IDs, and every numeric stringification the interpreter can produce — to
//! `MAX_PACKED_LEN` (30) characters inside the 16-byte envelope, with no stored length: the logical length is one past
//! the last nonzero nibble, unique because trailing spaces are unpackable.  Three alphabets are defined, selected by
//! format bits in the enclosing discriminant:
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

/// The packed-tier capacity in characters: 15 nibble bytes, two characters each.  No length is stored — the byte a
/// length would occupy is two characters of capacity, and 29-30 characters is exactly where millisecond-offset and
/// nanosecond-Zulu timestamps live.  Because trailing spaces are unpackable, the logical length is uniquely the
/// position one past the last nonzero nibble.
pub(crate) const MAX_PACKED_LEN: usize = 30;

/// The shortest content this tier holds.  Content of 15 characters or fewer takes an inline form instead (§2.2.9), so
/// the packed forms hold exactly 16-30 characters — which is what lets `len` read a single word: the terminating nibble
/// is always at nibble index 15-29, inside the last eight payload bytes.  The band is established by the tier selector,
/// the only path that constructs strings; `pack` states it as a precondition rather than checking it.
pub(crate) const MIN_PACKED_LEN: usize = 16;

/// The nibble-array width in bytes.
pub(crate) const PACKED_BYTES: usize = MAX_PACKED_LEN / 2;

/// Which 16-symbol alphabet a packed string uses.  In the fused forms this is carried entirely by the enclosing enum's
/// discriminant — the packed payload is 15 nibble bytes with no metadata byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PackedAlphabet {
    /// space `+` `-` `.` `0`-`9` `E` `e` — every numeric stringification fits here, in either exponent spelling.
    Numeric = 0,

    /// space `-` `.` `0`-`9` `:` `T` `Z` — Zulu-form and `-hh:mm`-offset ISO timestamps.
    DateTimeZ = 1,

    /// space `+` `-` `.` `0`-`9` `:` `T` — `+hh:mm`-offset ISO timestamps.
    DateTimePlus = 2,
}

/// A packed string: the alphabet and the pad-filled nibble array — no stored length.  In the fused `PerlString`/`Value`
/// forms the alphabet lives in the enclosing discriminant and the payload is the 15 nibble bytes alone; the field here
/// stands in for that discriminant.
///
/// Unused nibbles are canonically zero at construction: equality, ordering, and length recovery all read the full
/// array, so stale bits would be semantic corruption.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Packed {
    pub(crate) alphabet: PackedAlphabet,
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

// Each alphabet is exactly full, with 16 symbols defined.
const _: () = assert!(NUMERIC_SYMBOLS.len() == 16);
const _: () = assert!(DATETIME_Z_SYMBOLS.len() == 16);
const _: () = assert!(DATETIME_PLUS_SYMBOLS.len() == 16);

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

/// Classify and pack, or report that the bytes are not packable: ending in a space, or containing a byte outside every
/// alphabet.  Deterministic — the alphabets are tried in a fixed priority order.
///
/// **Precondition: the input is 16-30 bytes** (`MIN_PACKED_LEN..=MAX_PACKED_LEN`).  The tier selector is the only
/// constructor of strings and dispatches on length before reaching here, so out-of-band content cannot arrive; the
/// bound is asserted in debug builds and not checked in release.  `None` therefore means exactly one thing — the
/// content is not encodable in any alphabet — which is the classification result the selector needs, not a bounds
/// rejection it already ruled out.
pub(crate) fn pack(bytes: &[u8]) -> Option<Packed> {
    debug_assert!((MIN_PACKED_LEN..=MAX_PACKED_LEN).contains(&bytes.len()), "the tier selector must route content outside 16-30 characters elsewhere");
    if bytes.last() == Some(&b' ') {
        return None; // A trailing space cannot be distinguished from padding.
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
    let encode = |b: &u8| {
        let n = table[*b as usize];
        debug_assert_ne!(n, INVALID, "feasibility pass admitted an out-of-alphabet byte");
        n
    };

    // Walking the destination rather than the source bounds the write by the array itself, so nothing stands between
    // the precondition and memory safety.  High nibble first: byte order mirrors character order.
    let mut nibbles = [0u8; PACKED_BYTES];
    for (slot, pair) in nibbles.iter_mut().zip(bytes.chunks(2)) {
        *slot = (pair.first().map_or(0, &encode) << 4) | pair.get(1).map_or(0, &encode);
    }

    Some(Packed { alphabet, nibbles })
}

impl Packed {
    /// The logical character count: one past the last nonzero nibble.  Unique because trailing spaces are unpackable —
    /// the final character's nibble is always nonzero.
    ///
    /// The derivation counts trailing zero *nibbles*: a big-endian load of the word makes string position map
    /// monotonically onto bit position (nibble k occupies bits 4*(29-k)), so the trailing zeros of the word are the
    /// trailing pad nibbles of the string and `trailing_zeros() >> 2` is their count.  The length is the capacity minus
    /// that count — five instructions, one load, no branches.  The big-endian load is load-bearing, not incidental:
    /// little-endian with `leading_zeros` inverts nibble order within each byte and breaks the mapping.
    ///
    /// The word to load is chosen by the tier guarantee: content of 16-30 characters has its last nonzero nibble at
    /// nibble index 15-29, which is byte index 7-14 — exactly the last eight bytes.  Both words are fetched whole; an
    /// eight-byte copy compiles to one load plus a byte swap, where a byte-wise array literal compiles to eight loads
    /// and a shift chain and measured 1.7x slower.  Neither load has an alignment precondition — the copy is a `memcpy`
    /// into a value, where a raw-pointer cast would be the unsound formulation — but the hot one is aligned anyway: in
    /// the fused envelope the discriminant occupies byte 0, placing the payload at offset 1, so this word's first byte
    /// sits at struct offset 8 and inherits the envelope's 8-byte alignment (measured).  An 8-aligned eight-byte word
    /// cannot cross a cache line either, so the hot path is free of both costs.  The fallback word, at offset 1, is the
    /// unaligned one — and it is the branch that never runs in population.
    ///
    /// Measured against a byte-at-a-time reverse scan, medians of 15 interleaved runs over 4096-entry corpora: this
    /// form is data-independent at ~1.01 ns, while the scan ranges from ~2.09 ns at 16 characters to ~0.82 at 30,
    /// exiting sooner the longer the string is.  It therefore wins across the whole tier band, and by 1.85x on mixed
    /// content where the scan additionally pays branch mispredictions.  Splitting the packed variants by length band to
    /// dispatch between the two algorithms was measured and rejected: it costs 1.2-1.3x on every distribution tested,
    /// because duplicating the 65-instruction unrolled scan into a second arm inflates the dispatch function from 79 to
    /// 144 instructions, and that cost is paid on every call — including calls on values holding no string at all.
    pub(crate) fn len(&self) -> usize {
        // The last eight payload bytes, loaded as one word.  `pack` guarantees 16-30 characters, so the terminating
        // nibble is always inside this word and it is never zero.
        let mut word = [0u8; 8];
        word.copy_from_slice(&self.nibbles[7..15]);
        let terminator = u64::from_be_bytes(word);
        debug_assert_ne!(terminator, 0, "a packed string always holds at least MIN_PACKED_LEN characters");
        MAX_PACKED_LEN - (terminator.trailing_zeros() as usize >> 2)
    }

    /// Decode to raw bytes: the exact original, by the round-trip invariant.  Interior zero nibbles decode to spaces;
    /// the padding is never reached because the walk is bounded by the length.
    pub(crate) fn unpack(&self) -> ([u8; MAX_PACKED_LEN], usize) {
        let table = self.alphabet.decode_table();
        let mut out = [0u8; MAX_PACKED_LEN];
        let len = self.len();
        for (i, slot) in out.iter_mut().enumerate().take(len) {
            let byte = self.nibbles[i / 2];
            let n = if i % 2 == 0 { byte >> 4 } else { byte & 0x0F };
            *slot = table[n as usize];
        }

        (out, len)
    }

    /// Equality against a raw byte string, length-first: derive the length, reject on mismatch, then compare decoded
    /// characters.  Length-first is normative because a zero nibble is ambiguous against a raw space or a raw
    /// end-of-string (interior space versus padding), and the length resolves every such case up front with predictable
    /// control flow.  A first-bytes decode-pair precheck and the speculative dual-interpretation comparator are
    /// recorded measured options.
    pub(crate) fn eq_bytes(&self, other: &[u8]) -> bool {
        if other.len() > MAX_PACKED_LEN {
            return false;
        }

        let len = self.len();
        if len != other.len() {
            return false;
        }

        let table = self.alphabet.decode_table();
        for (i, &o) in other.iter().enumerate() {
            let byte = self.nibbles[i / 2];
            let n = if i % 2 == 0 { byte >> 4 } else { byte & 0x0F };
            if table[n as usize] != o {
                return false;
            }
        }

        true
    }

    /// Ordering against a raw byte string, length-first for the same ambiguity reason — the pinned counterexample:
    /// packed "2026" against raw "2026\n" must be Less (the packed string ended), but a naive decoder reading the zero
    /// nibble as a space would answer Greater (space > newline).  Deriving the length first makes every zero's meaning
    /// known before it is compared.
    pub(crate) fn cmp_bytes(&self, other: &[u8]) -> std::cmp::Ordering {
        let len = self.len();
        let table = self.alphabet.decode_table();
        for (i, &o) in other.iter().enumerate().take(len) {
            let byte = self.nibbles[i / 2];
            let n = if i % 2 == 0 { byte >> 4 } else { byte & 0x0F };
            match table[n as usize].cmp(&o) {
                std::cmp::Ordering::Equal => {}
                ordering => return ordering,
            }
        }

        len.cmp(&other.len())
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/packed_tests.rs"]
mod tests;
