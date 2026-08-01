//! Nibble-packed digit-dense strings (§2.2.9): two characters per byte over 16-symbol alphabets.
//!
//! Strings drawn from a 16-symbol alphabet pack two characters per byte, raising the inline capacity for the
//! digit-dense class — timestamps, IPs, numeric IDs, and every default numeric stringification — to `MAX_PACKED_LEN`
//! (30) characters inside the 16-byte envelope.  Three alphabets are defined, selected by the enclosing discriminant:
//!
//! - **Numeric**: space, `+`, `-`, `.`, `0`-`9`, `E`, `e` — every `%.15g` output and every `i64` stringification, in
//!   either exponent spelling.
//! - **DateTimePlus**: space, `+`, `-`, `.`, `0`-`9`, `:`, `T` — ISO timestamps in every form but Zulu.
//! - **DateTimeZulu**: space, `-`, `.`, `0`-`9`, `:`, `T`, `Z` — Zulu-form ISO timestamps.
//!
//! A valid timestamp never needs `Z` and `+` together — Zulu *is* the zero offset — so splitting the two spellings
//! covers the whole ISO grammar without a seventeenth symbol.  The union of all three is nineteen symbols against
//! sixteen nibble values, so three alphabets are forced, and content that migrates between them transcodes
//! ([`Packed::transcode`]).
//!
//! The order above is the classification priority, and it is chosen for the append path.  `Numeric` is a subset of
//! `DateTimePlus` on nibbles 0-13, so a string that starts numeric and meets a `:` or `T` is *reclassified* with no
//! rewriting at all — and lands on the canonical alphabet, because `DateTimePlus` is where timestamps belong unless a
//! `Z` forces otherwise.  All three alphabets hold sixteen symbols, so none is wider than another; they differ in which
//! sixteen.  `Z` is the one symbol no other alphabet holds, so `DateTimeZulu` is reached only through it, which makes
//! the variant itself a proof that the timestamp's offset is `+00:00`.
//!
//! # The length lives in the last nibble
//!
//! Each alphabet has **two length families**, again carried by the discriminant.  Content of exactly `MAX_PACKED_LEN`
//! characters fills all thirty nibbles and needs no stored length — the family says so.  Content of `MIN_PACKED_LEN`-29
//! characters stores the low four bits of its length in nibble 29, the one a thirtieth character would have used, and
//! recovers it as `0x10 | nibble` because the band's floor is sixteen.  Reading a length is one byte load, an `AND`,
//! and an `OR` — no scan, and no dependence on content.
//!
//! Storing the length explicitly is what makes **trailing spaces representable**.  With the length implied by the last
//! nonzero nibble, a string ending in a space could not be told from one padded with zeros, so such strings were
//! unpackable — a restriction that looked harmless for whole strings but blocks incremental building, where a string
//! passes through a trailing space on its way to something longer.
//!
//! Nibble values are assigned in ASCII order, so for two packed strings **of the same alphabet and the same length
//! **family**, comparing the nibble arrays as plain bytes gives exactly the raw strings' byte order: content
//! differences decide before nibble 29 is reached, and where one string ends the other has a symbol above the zero
//! padding.  Comparing across length families compares the twenty-nine shared nibbles and then the lengths, since the
//! last nibble means different things on the two sides.  Comparing across alphabets decodes.
//!
//! # Invariants
//!
//! - **Padding is zero.**  Nibbles from the content end through nibble 28 are zero.  Nothing reads them to derive a
//!   length any more, so a violation no longer announces itself — it silently corrupts ordering, equality, and hashing,
//!   all of which read the whole payload.  Every construction path zeroes by building from a zeroed array; any future
//!   mutation that shortens content must re-zero what it vacates.  [`Packed::padding_is_canonical`] states the property
//!   and the debug assertions check it.
//! - **Packing is an encoding, never a canonicalization.**  `unpack(pack(s)) == s` exactly, for every accepted input.
//! - **Classification is deterministic**: the alphabets are tried in the fixed priority order Numeric, DateTimePlus,
//!   DateTimeZulu, so equal byte contents always take equal representations — the prerequisite for representation-level
//!   equality.

// Three comparison fast paths still await their consumer: `cmp_same_alphabet`, `eq_bytes`, and `cmp_bytes` are what
// `PerlString` will route equality and ordering through, where it decodes and compares bytes for now.  The expect
// self-reports when that arrives.
#![cfg_attr(not(test), expect(dead_code))]

use std::cmp::Ordering;

/// The packed-tier capacity in characters: 15 nibble bytes, two characters each.
pub(crate) const MAX_PACKED_LEN: usize = 30;

/// The shortest content this tier holds.  Content of 15 characters or fewer takes an inline form instead (§2.2.9), so
/// the packed forms hold exactly 16-30 characters.  The band is established by the tier selector, the only path that
/// constructs strings; `pack` states it as a precondition rather than checking it.  It is also what lets the stored
/// length occupy four bits: only the low nibble varies across 16-29.
pub(crate) const MIN_PACKED_LEN: usize = 16;

/// The nibble-array width in bytes.
pub(crate) const PACKED_BYTES: usize = MAX_PACKED_LEN / 2;

/// The nibble index holding the stored length, for content shorter than the capacity.
const LENGTH_NIBBLE: usize = MAX_PACKED_LEN - 1;

/// Which 16-symbol alphabet a packed string uses.  In `PerlString` this is not stored: it is folded into the tag, so
/// each alphabet has its own variants and the payload is fifteen nibble bytes with nothing else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PackedAlphabet {
    /// space `+` `-` `.` `0`-`9` `E` `e` — every numeric stringification, in either exponent spelling.
    Numeric,

    /// space `+` `-` `.` `0`-`9` `:` `T` — ISO timestamps in every form but Zulu.  The canonical alphabet for
    /// timestamps, and it agrees with `Numeric` on nibbles 0-13, so moving into it rewrites nothing.
    DateTimePlus,

    /// space `-` `.` `0`-`9` `:` `T` `Z` — Zulu-form ISO timestamps.  Reached only by a `Z`, since that is the one
    /// symbol the other alphabets lack: **this variant proves the offset is `+00:00`**.
    DateTimeZulu,
}

/// A packed string: the alphabet, the length family, and the nibble array.
///
/// This is the working form, used while encoding and decoding.  In `PerlString` the first two fields do not exist —
/// they are folded into the tag, one variant per alphabet and length family — so a stored packed string is fifteen
/// bytes of nibbles and nothing else.  The fields here stand in for that tag while the value is in hand.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Packed {
    pub(crate) alphabet: PackedAlphabet,

    /// The `MAX_PACKED_LEN`-character family: every nibble is content and the length is implied.
    pub(crate) full: bool,
    pub(crate) nibbles: [u8; PACKED_BYTES],
}

/// Sentinel in the byte-to-nibble tables: this byte is outside the alphabet.
const INVALID: u8 = 0xFF;

/// Build a byte-to-nibble table from an ASCII-ordered symbol list, space first.
const fn encode_table(symbols: &[u8]) -> [u8; 256] {
    let mut table = [INVALID; 256];
    let mut i = 0;
    while i < symbols.len() {
        table[symbols[i] as usize] = i as u8;
        i += 1;
    }

    table
}

/// Build the nibble-to-byte table.
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
const DATETIME_PLUS_SYMBOLS: &[u8] = b" +-.0123456789:T";
const DATETIME_ZULU_SYMBOLS: &[u8] = b" -.0123456789:TZ";

const NUMERIC_ENCODE: [u8; 256] = encode_table(NUMERIC_SYMBOLS);
const NUMERIC_DECODE: [u8; 16] = decode_table(NUMERIC_SYMBOLS);
const DATETIME_PLUS_ENCODE: [u8; 256] = encode_table(DATETIME_PLUS_SYMBOLS);
const DATETIME_PLUS_DECODE: [u8; 16] = decode_table(DATETIME_PLUS_SYMBOLS);
const DATETIME_ZULU_ENCODE: [u8; 256] = encode_table(DATETIME_ZULU_SYMBOLS);
const DATETIME_ZULU_DECODE: [u8; 16] = decode_table(DATETIME_ZULU_SYMBOLS);

const _: () = assert!(NUMERIC_SYMBOLS.len() == 16);
const _: () = assert!(DATETIME_PLUS_SYMBOLS.len() == 16);
const _: () = assert!(DATETIME_ZULU_SYMBOLS.len() == 16);

impl PackedAlphabet {
    fn encode_table(self) -> &'static [u8; 256] {
        match self {
            PackedAlphabet::Numeric => &NUMERIC_ENCODE,
            PackedAlphabet::DateTimePlus => &DATETIME_PLUS_ENCODE,
            PackedAlphabet::DateTimeZulu => &DATETIME_ZULU_ENCODE,
        }
    }

    fn decode_table(self) -> &'static [u8; 16] {
        match self {
            PackedAlphabet::Numeric => &NUMERIC_DECODE,
            PackedAlphabet::DateTimePlus => &DATETIME_PLUS_DECODE,
            PackedAlphabet::DateTimeZulu => &DATETIME_ZULU_DECODE,
        }
    }
}

/// Read one nibble, high nibble first so that byte order over the array mirrors character order.
fn nibble_at(nibbles: &[u8; PACKED_BYTES], index: usize) -> u8 {
    let byte = nibbles[index / 2];

    if index.is_multiple_of(2) { byte >> 4 } else { byte & 0x0F }
}

/// Write one nibble, leaving the other half of the byte untouched.
fn set_nibble(nibbles: &mut [u8; PACKED_BYTES], index: usize, value: u8) {
    let byte = &mut nibbles[index / 2];

    if index.is_multiple_of(2) {
        *byte = (*byte & 0x0F) | (value << 4);
    } else {
        *byte = (*byte & 0xF0) | value;
    }
}

/// Classify and pack, or report that the content is not encodable in any alphabet.  Deterministic — the alphabets are
/// tried in a fixed priority order.
///
/// **Precondition: the input is 16-30 bytes** (`MIN_PACKED_LEN..=MAX_PACKED_LEN`).  The tier selector is the only
/// constructor of strings and dispatches on length before reaching here, so out-of-band content cannot arrive; the
/// bound is asserted in debug builds and not checked in release.
pub(crate) fn pack(bytes: &[u8]) -> Option<Packed> {
    debug_assert!((MIN_PACKED_LEN..=MAX_PACKED_LEN).contains(&bytes.len()), "the tier selector must route content outside 16-30 characters elsewhere");

    // One pass tracking feasibility in every alphabet; fail fast when none survives.  All must be tracked for every
    // byte: 'e' passes only Numeric and 'Z' only DateTimeZulu, so a later byte must not select an alphabet an earlier
    // byte already ruled out.
    let mut numeric = true;
    let mut datetime_plus = true;
    let mut datetime_zulu = true;
    for &b in bytes {
        numeric &= NUMERIC_ENCODE[b as usize] != INVALID;
        datetime_plus &= DATETIME_PLUS_ENCODE[b as usize] != INVALID;
        datetime_zulu &= DATETIME_ZULU_ENCODE[b as usize] != INVALID;
        if !numeric && !datetime_plus && !datetime_zulu {
            return None;
        }
    }

    // The priority order is the determinism rule: equal byte contents must always take equal representations.
    let alphabet = if numeric {
        PackedAlphabet::Numeric
    } else if datetime_plus {
        PackedAlphabet::DateTimePlus
    } else {
        PackedAlphabet::DateTimeZulu
    };

    pack_in(bytes, alphabet)
}

/// Encode into a **named** alphabet, or `None` if a byte has no symbol there.
///
/// [`pack`] is this under the canonical priority order.  Incremental building needs the forced form instead, because it
/// must choose an alphabet before seeing the whole string: it starts in `Numeric` and moves to `DateTimePlus` on the
/// first `:` or `T`, which rewrites no nibble at all, those two agreeing on 0-13.
///
/// The eager choice *is* the canonical one, which is why the priority order runs Numeric, DateTimePlus, DateTimeZulu:
/// timestamps belong to `DateTimePlus` unless a `Z` forces otherwise, so a string reclassified on its first `:` or `T`
/// needs no correction at the end.  Only a `Z` arriving later moves it again, through [`Packed::transcode`].
pub(crate) fn pack_in(bytes: &[u8], alphabet: PackedAlphabet) -> Option<Packed> {
    debug_assert!((MIN_PACKED_LEN..=MAX_PACKED_LEN).contains(&bytes.len()), "the tier selector must route content outside 16-30 characters elsewhere");

    let table = alphabet.encode_table();
    let mut nibbles = [0u8; PACKED_BYTES]; // Padding is zero by construction.
    for (i, &b) in bytes.iter().enumerate() {
        let n = table[b as usize];
        if n == INVALID {
            return None;
        }
        set_nibble(&mut nibbles, i, n);
    }

    let full = bytes.len() == MAX_PACKED_LEN;
    if !full {
        set_nibble(&mut nibbles, LENGTH_NIBBLE, (bytes.len() & 0x0F) as u8);
    }

    let packed = Packed { alphabet, full, nibbles };
    debug_assert!(packed.padding_is_canonical(), "packing must leave unused nibbles zero");
    Some(packed)
}

impl Packed {
    /// The character count.  The full family implies it; otherwise nibble 29 carries its low four bits and the band's
    /// floor supplies the high one.  One byte load, an `AND`, an `OR` — no scan, no dependence on content.
    pub(crate) fn len(&self) -> usize {
        if self.full { MAX_PACKED_LEN } else { MIN_PACKED_LEN | nibble_at(&self.nibbles, LENGTH_NIBBLE) as usize }
    }

    /// Whether the nibbles between the content end and the length field are zero.  Nothing derives a length from them
    /// any more, so a violation would not announce itself: ordering, equality, and hashing all read the whole payload,
    /// and equal content must have equal representation.
    pub(crate) fn padding_is_canonical(&self) -> bool {
        if self.full {
            return true; // Every nibble is content.
        }

        (self.len()..LENGTH_NIBBLE).all(|i| nibble_at(&self.nibbles, i) == 0)
    }

    /// Decode to raw bytes: the exact original, by the round-trip invariant.
    pub(crate) fn unpack(&self) -> ([u8; MAX_PACKED_LEN], usize) {
        let table = self.alphabet.decode_table();
        let mut out = [0u8; MAX_PACKED_LEN];
        let len = self.len();

        for (i, slot) in out.iter_mut().enumerate().take(len) {
            *slot = table[nibble_at(&self.nibbles, i) as usize];
        }

        (out, len)
    }

    /// Re-encode into another alphabet, or `None` when a symbol has no counterpart there — the operation incremental
    /// building needs when a character arrives that the current alphabet cannot hold.
    ///
    /// Only content nibbles are remapped: nibble 29 holds a length, not a symbol, and must survive untouched.
    ///
    /// The table lookups make this correct by construction — a symbol absent from the target has no encoding, so the
    /// conversion fails — and the resulting transitions, which the append path uses, are:
    ///
    /// |            transition            |  `0x00`   |  `0x01`   | `0x02`-`0x0D` | `0x0E`-`0x0F` |
    /// |----------------------------------|-----------|-----------|---------------|---------------|
    /// |      `Numeric` to `DateTimePlus` | unchanged | unchanged |   unchanged   |   **fail**    |
    /// |      `Numeric` to `DateTimeZulu` | unchanged | **fail**  |   decrement   |   **fail**    |
    /// | `DateTimePlus` to `DateTimeZulu` | unchanged | **fail**  |   decrement   |   decrement   |
    ///
    /// Widening into `DateTimePlus` rewrites nothing, since it and `Numeric` agree on nibbles 0-13 — only `E` and `e`
    /// have no counterpart, and they exist in no other alphabet.  Converting into `DateTimeZulu` is the same decrement
    /// from either source, `DateTimeZulu` being the same list shifted down past the absent `+`; `0x01` is that `+` and
    /// always fails, and the two sources differ only in that `0x0E`-`0x0F` are `E`/`e` under `Numeric` and `:`/`T`
    /// under `DateTimePlus`.  A failure means the content leaves the packed tier for the heap.
    pub(crate) fn transcode(&self, to: PackedAlphabet) -> Option<Packed> {
        if to == self.alphabet {
            return Some(*self);
        }

        let (from_table, to_table) = (self.alphabet.decode_table(), to.encode_table());
        let mut nibbles = self.nibbles;
        for i in 0..self.len() {
            let symbol = from_table[nibble_at(&self.nibbles, i) as usize];
            let mapped = to_table[symbol as usize];
            if mapped == INVALID {
                return None;
            }
            set_nibble(&mut nibbles, i, mapped);
        }

        let packed = Packed { alphabet: to, full: self.full, nibbles };
        debug_assert!(packed.padding_is_canonical(), "transcode must preserve zero padding");

        Some(packed)
    }

    /// Append bytes without leaving the nibbles, or `None` when the result leaves the tier — past the capacity, or
    /// encodable in no alphabet that also holds the existing content.
    ///
    /// This is the incremental path: the existing nibbles are kept and the new characters written past them.  Moving
    /// between `Numeric` and `DateTimePlus` rewrites nothing, those two agreeing on nibbles 0-13; only a move into
    /// `DateTimeZulu` rewrites, and then by a single decrement pass, that alphabet being the same list shifted down
    /// past the absent `+`.  Re-classifying the whole result instead would decode and re-encode everything on every
    /// append, which turns building a string into quadratic work.
    ///
    /// The alphabet is chosen by the same priority order `pack` uses, so the result is the representation the content
    /// would have taken had it been packed whole — appending cannot produce a non-canonical string.
    pub(crate) fn push(&self, tail: &[u8]) -> Option<Packed> {
        let len = self.len();
        let new_len = len + tail.len();
        if new_len > MAX_PACKED_LEN {
            return None;
        }

        // The first alphabet that both holds the new bytes and accepts the existing content.  Priority order is what
        // makes this the canonical choice.
        let target = [PackedAlphabet::Numeric, PackedAlphabet::DateTimePlus, PackedAlphabet::DateTimeZulu]
            .into_iter()
            .find(|&a| tail.iter().all(|&b| a.encode_table()[b as usize] != INVALID) && self.transcode(a).is_some())?;

        let mut moved = self.transcode(target)?;
        let table = target.encode_table();
        for (i, &b) in tail.iter().enumerate() {
            set_nibble(&mut moved.nibbles, len + i, table[b as usize]);
        }

        moved.full = new_len == MAX_PACKED_LEN;
        if !moved.full {
            set_nibble(&mut moved.nibbles, LENGTH_NIBBLE, (new_len & 0x0F) as u8);
        }

        debug_assert_eq!(moved.len(), new_len, "the stored length must follow the content");
        debug_assert!(moved.padding_is_canonical(), "append must leave unused nibbles zero");
        Some(moved)
    }

    /// Ordering against another packed string of the **same alphabet**.
    ///
    /// Within one length family this is plain byte comparison: a content difference decides before the length field is
    /// reached, and where one string ends the other holds a symbol above the zero padding.  Across families the last
    /// nibble means different things on the two sides, so the twenty-nine shared nibbles decide first and the lengths
    /// break the tie — which is prefix ordering, since the full family is the longer one.
    pub(crate) fn cmp_same_alphabet(&self, other: &Packed) -> Ordering {
        debug_assert_eq!(self.alphabet, other.alphabet, "cross-alphabet packed ordering must decode");

        if self.full == other.full {
            return self.nibbles.cmp(&other.nibbles);
        }

        let shared = self.nibbles[..PACKED_BYTES - 1].cmp(&other.nibbles[..PACKED_BYTES - 1]);
        let last_shared = MAX_PACKED_LEN - 2;

        shared.then_with(|| nibble_at(&self.nibbles, last_shared).cmp(&nibble_at(&other.nibbles, last_shared))).then_with(|| self.len().cmp(&other.len()))
    }

    /// Equality against a raw byte string, length-first: the stored length is free, so a mismatch rejects before any
    /// decoding.
    pub(crate) fn eq_bytes(&self, other: &[u8]) -> bool {
        if self.len() != other.len() {
            return false;
        }

        let table = self.alphabet.decode_table();

        other.iter().enumerate().all(|(i, &o)| table[nibble_at(&self.nibbles, i) as usize] == o)
    }

    /// Ordering against a raw byte string: decoded characters decide, then length breaks a prefix tie.
    pub(crate) fn cmp_bytes(&self, other: &[u8]) -> Ordering {
        let len = self.len();
        let table = self.alphabet.decode_table();
        for (i, &o) in other.iter().enumerate().take(len) {
            match table[nibble_at(&self.nibbles, i) as usize].cmp(&o) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
        }

        len.cmp(&other.len())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/packed_tests.rs"]
mod tests;
