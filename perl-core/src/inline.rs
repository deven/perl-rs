//! The three inline string storage formats (§2.2.9): 15 payload bytes in two length families each, with the storage
//! format an axis independent of perl's semantic `SvUTF8` flag.
//!
//! - **Bytes**: perl's internal octets verbatim; the flag is off by definition (a flag-on string's internal bytes are
//!   UTF-8, which is the `Utf8` form's job).  Characters are the octets.
//! - **Utf8**: encoded bytes verbatim, flag on by definition — content that cannot decode into the Latin-1 range: a
//!   code point at or above U+0100, perl-extended, or malformed.
//! - **Latin1**: content that is valid UTF-8 with every code point in U+0000–U+00FF, stored one code point per byte
//!   **regardless of the flag**, which is carried alongside.  Flag on: the payload is the string's characters.  Flag
//!   off: the string *is* the UTF-8 octet sequence — up to thirty semantic octets in fifteen payload bytes — and every
//!   observable answers over that virtual expansion: `len` is the expansion sum, never the payload count.
//!
//! The load-bearing monster (container-verified): payload `E9` flag-off is the one-octet string `é` under Bytes and the
//! two-octet string `C3 A9` under Latin1 — different strings, distinguished by the format alone.  Canonical selection
//! is the determinism obligation making representation-level equality sound: flag-off octets that are valid
//! Latin-1-range UTF-8 *always* take Latin1, so equal perl strings take equal representations.  Noncanonical (overlong)
//! encodings are invalid UTF-8 and therefore never compress — they stay in their encoded form, which is what makes the
//! reinterpretation transforms total.
//!
//! Storage length is explicit: content of fourteen bytes or fewer keeps its length in the byte a fifteenth would have
//! used, and content of exactly fifteen implies it (§2.2.9).  Here the family is the `full` field beside each payload;
//! in `PerlString` it is a discriminant dimension, and the payload arrays here are byte-identical to what the fused
//! variants store, so adoption is a pure discriminant fold.  NUL is ordinary content in all three spellings — the
//! octet, the encoded byte, and the character U+0000, which is why the Latin-1 range starts at U+0000 — where it was
//! heap-only while the inline forms were NUL-terminated, the terminator being what bought the fifteenth byte.
//!
//! # The obligation this format carries into comparison
//!
//! `Latin1` stores code points where perl's buffer holds their UTF-8 encoding.  That is a compression of the buffer,
//! not a claim about the value.  With the utf8 flag *off* those bytes **are** the string: fifteen stored code points in
//! `U+0080`-`U+00FF` are a thirty-character value, each byte its own character, and it is only a coincidence of
//! encoding that they fit in fifteen.
//!
//! Against an **all-ASCII** operand the stored payload nevertheless compares directly, either flag — a property of the
//! other side being ASCII rather than of any storage form.  A stored byte below `0x80` compares as the character it is;
//! a stored byte at or above `0x80` exceeds every ASCII byte and decides the comparison there, being a high code point
//! flagged or its encoding's lead byte unflagged, and which one does not change the answer.  A payload with no high
//! byte expands to itself, so the prefix rule applies unchanged.  Verified over 400,000 pairs against the expanded
//! comparison.  The packed forms are ASCII by construction and so always qualify; for the inline and heap forms the
//! scan cache already records it.
//!
//! The reason it is correct rather than merely convenient: a set high bit means a code point above `U+007F`, which
//! outranks every ASCII character, so byte order and code-point order agree at that position and the naive comparison
//! yields the answer the expanded one would have.
//!
//! That agreement is UTF-8's design, not a fact about encodings.  UTF-8 sets the high bit on every non-ASCII byte and
//! preserves code-point order under byte comparison — verified across every code point to `U+10FFFF`.  UTF-16 does
//! neither: an ASCII character contains a `0x00` byte, and the surrogate range inverts (`U+E000` encodes as `E0 00`,
//! the larger `U+10000` as `D8 00 DC 00`).  None of this survives a change of encoding.
//!
//! Against the **`Utf8`** form it does not: both sides hold high bytes, one storing code points and the other their
//! encoding, so `E9` and `C3 A9` are one character compared unequal, and `U+00E9` against `U+0100` *inverts* — `E9`
//! exceeding `C4` while the code point is smaller.  An inverted order corrupts a sort silently, so that pairing
//! compares character by character — and what this payload presents as characters is decided by the flag.
//!
//! Flag on, each stored byte *is* a code point, compared against the other side's decoded one.  Flag off, the value is
//! the UTF-8 encoding and its bytes are themselves the characters, so a stored byte below `0x80` presents one character
//! and a byte at or above presents two: the lead `0xC0 | b >> 6`, then the continuation `0x80 | b & 0x3F`, each
//! compared in its own right.  Neither reading needs a buffer — the characters are computed as the comparison walks,
//! one stored byte yielding one or two.  Verified over 300,000 pairs against the materialised expansion.
//!
//! **Gate the direct path on a positive fact.**  The condition is *the other operand is known all-ASCII*, never *the
//! other operand is not `Utf8`*.  An exclusion fails open: a storage form added later, or content whose classification
//! is simply unknown, would slip through and compare wrongly.  Stated positively those cases fall through to decoding,
//! which is always correct and only sometimes slower.
//!
//! So every length, comparison, and hash answers over the virtual expansion rather than the stored payload — §2.2.9 and
//! §2.3.5.  `PerlString`'s comparison paths are written against raw inline bytes today and will need revisiting here,
//! not merely extending.

// The production consumers arrive when `PerlString` adopts these formats; the expect self-reports the moment they land.
#![cfg_attr(not(test), expect(dead_code))]

/// The inline payload width in bytes.
pub(crate) const INLINE_BYTES: usize = 15;

/// The maximum semantic octet length an inline string can represent: a flag-off Latin1 payload of fifteen two-byte code
/// points expands to thirty octets.
pub(crate) const INLINE_MAX_OCTETS: usize = 30;

/// An inline string with its full semantic identity.  Illegal flag/format combinations are unrepresentable: `Bytes` is
/// flag-off by construction, `Utf8` flag-on, and only `Latin1` carries the flag.  In `PerlString` neither the flag nor
/// the family are fields at all — each format × family × flag combination is its own variant, folded into the tag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum InlineStr {
    /// Internal octets verbatim; semantic flag off.
    Bytes { buf: [u8; INLINE_BYTES], full: bool },

    /// Encoded bytes verbatim (beyond Latin-1, extended, or malformed); semantic flag on.
    Utf8 { buf: [u8; INLINE_BYTES], full: bool },

    /// Code points U+0000–U+00FF, one per byte; the semantic flag rides alongside.
    Latin1 { cp: [u8; INLINE_BYTES], full: bool, utf8_flag: bool },
}

/// Where a short inline payload keeps its length: the byte a fifteenth character would have occupied.
const LENGTH_BYTE: usize = INLINE_BYTES - 1;

/// The stored length of an inline payload: implied at full capacity, read from the length byte otherwise — the same
/// arrangement the live inline tier and the packed band use (§2.2.9).
fn payload_len(full: bool, payload: &[u8; INLINE_BYTES]) -> usize {
    debug_assert!(full || payload[LENGTH_BYTE] as usize <= LENGTH_BYTE, "a short payload's length byte must be <= 14");
    if full { INLINE_BYTES } else { payload[LENGTH_BYTE] as usize }
}

/// Build a canonical payload from content: padding zeroed, the length byte written for the short family.  Returns the
/// payload beside its family — equal content must take equal bytes, or representation stops standing in for content.
fn build_payload(content: &[u8]) -> ([u8; INLINE_BYTES], bool) {
    debug_assert!(content.len() <= INLINE_BYTES);
    let mut payload = [0u8; INLINE_BYTES];
    payload[..content.len()].copy_from_slice(content);
    let full = content.len() == INLINE_BYTES;
    if !full {
        payload[LENGTH_BYTE] = content.len() as u8;
    }
    (payload, full)
}

/// Strict decode of Latin-1-range UTF-8: every code point in U+0000–U+00FF, canonical encodings only.  Overlong forms
/// (`C0`/`C1` leads — including `C0 80`, the overlong NUL) and every lead at or above `C4` fail — by design, since
/// noncanonical content must never compress.  Returns the code points and their count.
fn decode_latin1_range(bytes: &[u8]) -> Option<([u8; INLINE_BYTES], usize)> {
    let mut cp = [0u8; INLINE_BYTES];
    let mut n = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let decoded = match b {
            0x00..=0x7F => {
                i += 1;
                b
            }
            0xC2 | 0xC3 => {
                let &next = bytes.get(i + 1)?;
                if !(0x80..=0xBF).contains(&next) {
                    return None;
                }
                i += 2;
                ((b & 0x03) << 6) | (next & 0x3F)
            }
            _ => return None, // Continuation, overlong lead, or beyond-Latin-1 lead.
        };
        if n == INLINE_BYTES {
            return None; // Sixteenth code point: exceeds the payload.
        }
        cp[n] = decoded;
        n += 1;
    }

    Some((cp, n))
}

/// Encode one Latin-1-range code point into the expansion buffer; returns the bytes written.
fn encode_cp(cp: u8, out: &mut [u8]) -> usize {
    if cp <= 0x7F {
        out[0] = cp;
        1
    } else {
        out[0] = 0xC2 | (cp >> 6);
        out[1] = 0x80 | (cp & 0x3F);
        2
    }
}

/// Classify perl string content (internal bytes plus semantic flag) into its canonical inline form, or `None` when the
/// content fits none of them.
///
/// Determinism is the point: flag-off octets that are valid Latin-1-range UTF-8 always take `Latin1`, never `Bytes`, so
/// equal perl strings take equal representations.
pub(crate) fn classify(internal: &[u8], utf8_flag: bool) -> Option<InlineStr> {
    if let Some((cp, n)) = decode_latin1_range(internal) {
        // Valid Latin-1-range UTF-8 compresses regardless of the flag — the canonical rule.
        let (cp, full) = build_payload(&cp[..n]);
        return Some(InlineStr::Latin1 { cp, full, utf8_flag });
    }
    if internal.len() > INLINE_BYTES {
        return None;
    }
    let (buf, full) = build_payload(internal);

    Some(if utf8_flag { InlineStr::Utf8 { buf, full } } else { InlineStr::Bytes { buf, full } })
}

impl InlineStr {
    /// The semantic flag: definitional for `Bytes` and `Utf8`, carried for `Latin1`.
    pub(crate) fn utf8_flag(&self) -> bool {
        match self {
            InlineStr::Bytes { .. } => false,
            InlineStr::Utf8 { .. } => true,
            InlineStr::Latin1 { utf8_flag, .. } => *utf8_flag,
        }
    }

    /// The internal (perl-visible under `use bytes`) byte sequence.  For `Latin1` this is the virtual expansion — the
    /// dual-view discipline shared with the packed tier.
    pub(crate) fn internal_bytes(&self) -> ([u8; INLINE_MAX_OCTETS], usize) {
        let mut out = [0u8; INLINE_MAX_OCTETS];
        match self {
            InlineStr::Bytes { buf, full } | InlineStr::Utf8 { buf, full } => {
                let n = payload_len(*full, buf);
                out[..n].copy_from_slice(&buf[..n]);
                (out, n)
            }
            InlineStr::Latin1 { cp, full, .. } => {
                let mut n = 0;
                for &c in &cp[..payload_len(*full, cp)] {
                    n += encode_cp(c, &mut out[n..]);
                }
                (out, n)
            }
        }
    }

    /// Perl's `length`.  Flag on: characters.  Flag off: octets — for `Latin1` that is the expansion sum, one or two
    /// per payload byte, never the payload count: fifteen stored high-Latin-1 code points report thirty
    /// (container-verified).
    pub(crate) fn len(&self) -> usize {
        match self {
            InlineStr::Bytes { buf, full } => payload_len(*full, buf),

            // Character count of encoded content is the scan machinery's concern (and ill-defined for malformed
            // payloads); the storage length is what this layer answers.
            InlineStr::Utf8 { buf, full } => payload_len(*full, buf),
            InlineStr::Latin1 { cp, full, utf8_flag } => {
                let stored = payload_len(*full, cp);
                if *utf8_flag { stored } else { cp[..stored].iter().map(|&c| if c <= 0x7F { 1 } else { 2 }).sum() }
            }
        }
    }

    /// Perl `eq` (`sv_eq`): equal flags compare internal bytes; differing flags upgrade the byte side's octets to their
    /// UTF-8 encoding first.
    pub(crate) fn eq_perl(&self, other: &InlineStr) -> bool {
        let (a, alen) = self.eq_view(other.utf8_flag());
        let (b, blen) = other.eq_view(self.utf8_flag());

        a[..alen] == b[..blen]
    }

    /// This string's byte sequence for comparison against a string with the given flag: the internal bytes, unless this
    /// side is flag-off and the other flagged — then the upgraded encoding.  Upgrading a flag-off string encodes its
    /// octets, and a flag-off `Latin1`'s octets are its expansion, so the upgrade is the double expansion.
    fn eq_view(&self, other_flag: bool) -> ([u8; 2 * INLINE_MAX_OCTETS], usize) {
        let mut out = [0u8; 2 * INLINE_MAX_OCTETS];
        let (bytes, n) = self.internal_bytes();

        if self.utf8_flag() || !other_flag {
            out[..n].copy_from_slice(&bytes[..n]);
            (out, n)
        } else {
            let mut m = 0;
            for &b in &bytes[..n] {
                m += encode_cp(b, &mut out[m..]);
            }
            (out, m)
        }
    }

    /// `utf8::upgrade`: the characters are preserved and the flag turns on.  A flag-off string's characters are its
    /// octets, at most 30, each within Latin-1 range — so the result always fits: this is total.  For `Bytes` the
    /// payload is preserved verbatim (zero byte work — only the discriminant changes); for flag-off `Latin1` the octets
    /// are the expansion, so the payload re-derives — and may not fit (16-30 octets become 16-30 characters).
    pub(crate) fn upgrade(&self) -> Option<InlineStr> {
        match self {
            InlineStr::Bytes { buf, full } => Some(InlineStr::Latin1 { cp: *buf, full: *full, utf8_flag: true }),
            InlineStr::Utf8 { .. } => Some(*self),
            InlineStr::Latin1 { utf8_flag: true, .. } => Some(*self),
            InlineStr::Latin1 { .. } => {
                let (bytes, n) = self.internal_bytes();
                if n > INLINE_BYTES {
                    return None; // 16-30 octets upgrade to 16-30 characters: heap territory.
                }
                let (cp, full) = build_payload(&bytes[..n]);
                Some(InlineStr::Latin1 { cp, full, utf8_flag: true })
            }
        }
    }

    /// `utf8::downgrade`: the characters are preserved and the flag turns off.  Fails (perl croaks without `fail_ok`)
    /// beyond Latin-1 — `Utf8` content by definition.  For `Latin1` the characters become octets and canonical
    /// selection re-runs: `é` lands in `Bytes` (`E9` alone is not valid UTF-8), while `Ã©` re-compresses to flag-off
    /// `Latin1` — the canonical rule keeps downgrade's output deterministic without special cases.
    pub(crate) fn downgrade(&self) -> Option<InlineStr> {
        match self {
            InlineStr::Bytes { .. } => Some(*self),
            InlineStr::Utf8 { .. } => None,
            InlineStr::Latin1 { cp, full, utf8_flag: true } => {
                let n = payload_len(*full, cp);
                classify(&cp[..n], false)
            }
            InlineStr::Latin1 { .. } => Some(*self),
        }
    }

    /// `Encode::_utf8_off`: reinterpret the internal bytes as octets.  On compressed flag-on content this is the pure
    /// flag flip — the payload is untouched, and the string's octets become the expansion (container-verified: an
    /// upgraded `é` becomes the flag-off two-character `C3 A9`).
    pub(crate) fn utf8_off_reinterpret(&self) -> Option<InlineStr> {
        match self {
            InlineStr::Bytes { .. } => Some(*self),
            InlineStr::Latin1 { cp, full, .. } => Some(InlineStr::Latin1 { cp: *cp, full: *full, utf8_flag: false }),
            InlineStr::Utf8 { buf, full } => classify(&buf[..payload_len(*full, buf)], false),
        }
    }

    /// `Encode::_utf8_on`: reinterpret the internal bytes as encoded content.  On flag-off `Latin1` this is the pure
    /// flag flip; on `Bytes` it reclassifies — a lone `E9` becomes flagged malformed content in the `Utf8` form
    /// (container-verified).
    pub(crate) fn utf8_on_reinterpret(&self) -> Option<InlineStr> {
        match self {
            InlineStr::Utf8 { .. } => Some(*self),
            InlineStr::Latin1 { cp, full, .. } => Some(InlineStr::Latin1 { cp: *cp, full: *full, utf8_flag: true }),
            InlineStr::Bytes { buf, full } => classify(&buf[..payload_len(*full, buf)], true),
        }
    }

    /// Remove the last octet — perl's `chop` on a flag-off string.  Byte-level mutation can split an encoded character,
    /// so the result re-runs canonical selection: a split lands in `Bytes`, and a 16-30-octet flag-off `Latin1` result
    /// exceeds inline entirely (`None` — heap).  Decoded storage is a read optimization a string can fall out of, never
    /// a constraint on its bytes.
    pub(crate) fn remove_last_octet(&self) -> Option<InlineStr> {
        debug_assert!(!self.utf8_flag(), "chop on a flagged string removes a character, not an octet");
        let (bytes, n) = self.internal_bytes();
        if n == 0 {
            return Some(*self);
        }

        classify(&bytes[..n - 1], false)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/inline_tests.rs"]
mod tests;
