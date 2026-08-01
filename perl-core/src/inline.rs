//! The three inline string storage formats (§2.2.9): NUL-terminated, 15 payload bytes, with the storage format an axis
//! independent of perl's semantic `SvUTF8` flag.
//!
//! - **Bytes**: perl's internal octets verbatim; the flag is off by definition (a flag-on string's internal bytes are
//!   UTF-8, which is the `Utf8` form's job).  Characters are the octets.
//! - **Utf8**: encoded bytes verbatim, flag on by definition — content that cannot decode into the Latin-1 range: a
//!   code point at or above U+0100, perl-extended, or malformed.
//! - **Latin1**: content that is valid UTF-8 with every code point in U+0001–U+00FF, stored one code point per byte
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
//! Storage length is the position of the first NUL, or 15 when none is present (the unterminated form).  Content
//! containing NUL in any spelling — octet `0x00`, encoded byte `0x00`, character U+0000 — is **heap-only, ruled**
//! (§2.2.9): the terminator byte is reserved, NUL-bearing strings are rare and skew long, and inline storage is an
//! optimization a string simply doesn't get.

// The production consumers arrive with the PerlString fusion; the expect self-reports for removal the moment they land.
#![cfg_attr(not(test), expect(dead_code))]

/// The inline payload width in bytes.
pub(crate) const INLINE_BYTES: usize = 15;

/// The maximum semantic octet length an inline string can represent: a flag-off Latin1 payload of fifteen two-byte code
/// points expands to thirty octets.
pub(crate) const INLINE_MAX_OCTETS: usize = 30;

/// An inline string with its full semantic identity.  Illegal flag/format combinations are unrepresentable: `Bytes` is
/// flag-off by construction, `Utf8` flag-on, and only `Latin1` carries the flag — in the fused enums these become
/// discriminant dimensions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum InlineStr {
    /// Internal octets verbatim; semantic flag off.
    Bytes([u8; INLINE_BYTES]),

    /// Encoded bytes verbatim (beyond Latin-1, extended, or malformed); semantic flag on.
    Utf8([u8; INLINE_BYTES]),

    /// Code points U+0001–U+00FF, one per byte; the semantic flag rides alongside.
    Latin1 { cp: [u8; INLINE_BYTES], utf8_flag: bool },
}

/// Position of the first NUL, or 15: the storage length of a NUL-terminated payload.
fn payload_len(payload: &[u8; INLINE_BYTES]) -> usize {
    payload.iter().position(|&b| b == 0).unwrap_or(INLINE_BYTES)
}

/// Strict decode of Latin-1-range UTF-8: every code point in U+0001–U+00FF, canonical encodings only.  Overlong forms
/// (`C0`/`C1` leads) and every lead at or above `C4` fail — by design, since noncanonical content must never compress.
/// Returns the code points and their count.
fn decode_latin1_range(bytes: &[u8]) -> Option<([u8; INLINE_BYTES], usize)> {
    let mut cp = [0u8; INLINE_BYTES];
    let mut n = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let decoded = match b {
            0x00 => return None, // U+0000: the NUL ruling's third spelling.
            0x01..=0x7F => {
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
/// content is not inline-eligible (too long, NUL-bearing pending the ruling, or — with the flag on — content the caller
/// should route to heap alongside everything else oversized).
///
/// Determinism is the point: flag-off octets that are valid Latin-1-range UTF-8 always take `Latin1`, never `Bytes`, so
/// equal perl strings take equal representations.
pub(crate) fn classify(internal: &[u8], utf8_flag: bool) -> Option<InlineStr> {
    if internal.contains(&0) {
        return None; // NUL-bearing strings are heap-only, ruled (§2.2.9): the terminator is reserved.
    }
    if let Some((cp, n)) = decode_latin1_range(internal) {
        // Valid Latin-1-range UTF-8 compresses regardless of the flag — the canonical rule.
        let mut payload = [0u8; INLINE_BYTES];
        payload[..n].copy_from_slice(&cp[..n]);
        return Some(InlineStr::Latin1 { cp: payload, utf8_flag });
    }
    if internal.len() > INLINE_BYTES {
        return None;
    }
    let mut payload = [0u8; INLINE_BYTES];
    payload[..internal.len()].copy_from_slice(internal);

    Some(if utf8_flag { InlineStr::Utf8(payload) } else { InlineStr::Bytes(payload) })
}

impl InlineStr {
    /// The semantic flag: definitional for `Bytes` and `Utf8`, carried for `Latin1`.
    pub(crate) fn utf8_flag(&self) -> bool {
        match self {
            InlineStr::Bytes(_) => false,
            InlineStr::Utf8(_) => true,
            InlineStr::Latin1 { utf8_flag, .. } => *utf8_flag,
        }
    }

    /// The internal (perl-visible under `use bytes`) byte sequence.  For `Latin1` this is the virtual expansion — the
    /// dual-view discipline shared with the packed tier.
    pub(crate) fn internal_bytes(&self) -> ([u8; INLINE_MAX_OCTETS], usize) {
        let mut out = [0u8; INLINE_MAX_OCTETS];
        match self {
            InlineStr::Bytes(p) | InlineStr::Utf8(p) => {
                let n = payload_len(p);
                out[..n].copy_from_slice(&p[..n]);
                (out, n)
            }
            InlineStr::Latin1 { cp, .. } => {
                let mut n = 0;
                for &c in &cp[..payload_len(cp)] {
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
            InlineStr::Bytes(p) => payload_len(p),

            // Character count of encoded content is the scan machinery's concern (and ill-defined
            // for malformed payloads); the storage length is what this layer answers.
            InlineStr::Utf8(p) => payload_len(p),
            InlineStr::Latin1 { cp, utf8_flag } => {
                let stored = payload_len(cp);
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
            InlineStr::Bytes(p) => Some(InlineStr::Latin1 { cp: *p, utf8_flag: true }),
            InlineStr::Utf8(_) => Some(*self),
            InlineStr::Latin1 { utf8_flag: true, .. } => Some(*self),
            InlineStr::Latin1 { .. } => {
                let (bytes, n) = self.internal_bytes();
                if n > INLINE_BYTES {
                    return None; // 16-30 octets upgrade to 16-30 characters: heap territory.
                }
                let mut cp = [0u8; INLINE_BYTES];
                cp[..n].copy_from_slice(&bytes[..n]);
                Some(InlineStr::Latin1 { cp, utf8_flag: true })
            }
        }
    }

    /// `utf8::downgrade`: the characters are preserved and the flag turns off.  Fails (perl croaks without `fail_ok`)
    /// beyond Latin-1 — `Utf8` content by definition.  For `Latin1` the characters become octets and canonical
    /// selection re-runs: `é` lands in `Bytes` (`E9` alone is not valid UTF-8), while `Ã©` re-compresses to flag-off
    /// `Latin1` — the canonical rule keeps downgrade's output deterministic without special cases.
    pub(crate) fn downgrade(&self) -> Option<InlineStr> {
        match self {
            InlineStr::Bytes(_) => Some(*self),
            InlineStr::Utf8(_) => None,
            InlineStr::Latin1 { cp, utf8_flag: true } => {
                let n = payload_len(cp);
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
            InlineStr::Bytes(_) => Some(*self),
            InlineStr::Latin1 { cp, .. } => Some(InlineStr::Latin1 { cp: *cp, utf8_flag: false }),
            InlineStr::Utf8(p) => classify(&p[..payload_len(p)], false),
        }
    }

    /// `Encode::_utf8_on`: reinterpret the internal bytes as encoded content.  On flag-off `Latin1` this is the pure
    /// flag flip; on `Bytes` it reclassifies — a lone `E9` becomes flagged malformed content in the `Utf8` form
    /// (container-verified).
    pub(crate) fn utf8_on_reinterpret(&self) -> Option<InlineStr> {
        match self {
            InlineStr::Utf8(_) => Some(*self),
            InlineStr::Latin1 { cp, .. } => Some(InlineStr::Latin1 { cp: *cp, utf8_flag: true }),
            InlineStr::Bytes(p) => classify(&p[..payload_len(p)], true),
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
