//! `PerlString` — a Perl string: octet sequence + per-string state (§2.2.3).
//!
//! Two storage kinds and three per-value state dimensions fold into the enum discriminant:
//!
//! - **Storage**: `Inline` (≤ 22 bytes, no heap allocation) or `Heap` (a [`CowBuffer`]).
//! - **The Perl utf8 flag**: a per-SV *semantic claim* ("interpret these bytes as characters"), not a validity fact.
//!   It can be set on bytes Rust rejects (perl-extended UTF-8; verified `chr(0x110000)`); no code path may derive
//!   `from_utf8_unchecked` from it.  Rust-level validity comes from the scan cache only.
//! - **Warned**: the numification-warning once-bit (§2.3.4).  Monotone: set, never cleared.
//! - **Tainted**: the per-value taint bit (§2.6.1).  Cleared only through the laundering capability (§2.6.2).
//!
//! Inline strings additionally fold their **scan state** into the tag — and only the five mutually exclusive *terminal*
//! states of the §2.2.4 lattice, because inline strings are scanned eagerly and completely at construction: a full
//! classification of at most 22 bytes is nearly free.  Heap strings keep the full nine-state lazy lattice in the buffer
//! header (§2.2.4–§2.2.6).
//!
//! Variant names are full words: scan word first (`Ascii`, `Latin1`, `NonLatin1`, `Extended`, `Malformed`), then flag
//! words in fixed order: `Flagged` (the *Perl* utf8 flag — a different thing from the scan's validity facts), `Warned`,
//! `Tainted`.  E.g. `InlineLatin1FlaggedTainted`, `HeapWarned`.
//!
//! Equality and hashing are **character-sequence** semantics (§2.3.5): the utf8 flag changes the byte→character
//! mapping, so same-bytes/different-flags can be different strings and different-bytes can be the same string.  Warned
//! and tainted are ignored by `Eq`/`Hash`.

use crate::cow_buffer::{AllocError, CowBuffer};
use crate::packed::{MAX_PACKED_LEN, MIN_PACKED_LEN, PACKED_BYTES, Packed, PackedAlphabet, pack};
use std::fmt;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::mem;
use std::str;

/// Maximum inline payload: chosen so every numeric stringification stays allocation-free (§2.2.3).
pub const INLINE_MAX: usize = 15;

/// The widest byte sequence any non-heap form decodes to, and so the size of the scratch buffer the borrowed-view
/// accessors take.
///
/// It is `INLINE_MAX * 2`, and not by coincidence: **every non-heap form is a 2:1 compression of its decoded bytes**.
/// The packed forms reach that ratio by storing two symbols per byte, four bits each; the Latin-1 form reaches it by
/// declining to spend two bytes on a code point, since `U+0080`-`U+00FF` sits inside UTF-8's two-byte range.  The same
/// factor, arrived at from opposite directions — one packing two units into a byte, the other refusing to let one unit
/// take two.  Raw forms compress not at all and are trivially under the bound.
///
/// So this is correct by construction rather than by measuring the cases, and it is a constraint on what may be added
/// later: a non-heap encoding compressing more than 2:1 would overflow every scratch buffer in the crate.
pub const DECODE_MAX: usize = INLINE_MAX * 2;

/// Heap scan-cache states, stored in the `CowBuffer` header byte (§2.2.4).  Zero is `UNKNOWN`, the lattice top — the
/// natural zero-initialized state can never assert a validity claim (§2.2.6).
pub mod scan {
    /// Completely unknown.  Zero-pinned (§2.2.6): fresh headers can never assert a claim.
    pub const UNKNOWN: u8 = 0;

    /// Entirely U+0000–U+007F.
    pub const ASCII: u8 = 1;

    /// Rust-valid, entirely U+0000–U+00FF, non-ASCII.  Can equal an unflagged string.
    pub const UTF8_LATIN1: u8 = 2;

    /// Rust-valid, contains a character ≥ U+0100.  Cannot equal an unflagged string.
    pub const UTF8_NON_LATIN1: u8 = 3;

    /// Rust-valid; nothing further known (narrows to ASCII / UTF8_LATIN1 / UTF8_NON_LATIN1, or to UTF8_NON_ASCII via
    /// the cheap high-bit probe).
    pub const UTF8_UNKNOWN_RANGE: u8 = 4;

    /// Rust-valid, known non-ASCII; Latin-1-range unresolved.  The cheap `is_ascii` probe lands here from
    /// UTF8_UNKNOWN_RANGE without paying the full-range lead-byte pass (§2.2.4).
    pub const UTF8_NON_ASCII: u8 = 5;

    /// Perl-decodable, Rust-invalid: contains a code point Rust rejects (a surrogate or ≥ U+110000), hence ≥ U+0100.
    /// Cannot equal an unflagged string.
    pub const EXTENDED_UTF8: u8 = 6;

    /// Violates the encoding patterns; invalid for Rust and perl both (§2.2.4).  Cannot equal an unflagged string.
    pub const MALFORMED_UTF8: u8 = 7;

    /// A high bit is present; validity and range unknown.
    pub const NON_ASCII: u8 = 8;

    /// Rust-valid ⟺ 1..=5 under the numbering (§2.2.4).
    #[inline]
    pub const fn is_rust_valid(state: u8) -> bool {
        state >= ASCII && state <= UTF8_NON_ASCII
    }

    /// Perl-decodable ⟺ 1..=6.
    #[inline]
    pub const fn is_perl_decodable(state: u8) -> bool {
        state >= ASCII && state <= EXTENDED_UTF8
    }

    /// Known entirely ≤ U+00FF (downgradable) ⟺ 1..=2.
    #[inline]
    pub const fn is_known_latin1_range(state: u8) -> bool {
        state == ASCII || state == UTF8_LATIN1
    }

    /// Fully-scanned terminal classification (§2.2.4): mutually exclusive byte-content classes.
    #[inline]
    pub const fn is_terminal(state: u8) -> bool {
        matches!(state, ASCII | UTF8_LATIN1 | UTF8_NON_LATIN1 | EXTENDED_UTF8 | MALFORMED_UTF8)
    }

    /// Known non-ASCII (a high bit is known used) ⟺ any state but UNKNOWN, ASCII, UTF8_UNKNOWN_RANGE.
    #[inline]
    pub const fn is_known_non_ascii(state: u8) -> bool {
        !matches!(state, UNKNOWN | ASCII | UTF8_UNKNOWN_RANGE)
    }

    /// Known to contain a character ≥ U+0100 ⟺ 3 or 5.
    #[inline]
    pub const fn is_known_beyond_latin1(state: u8) -> bool {
        state == UTF8_NON_LATIN1 || state == EXTENDED_UTF8
    }
}

/// Test-only instrumentation proving the §2.3.5 short-circuits actually fire (compiled out of non-test builds).
#[cfg(test)]
pub(crate) mod eq_probe {
    use std::cell::Cell;

    thread_local! {
        /// Count of grid early-returns taken.
        pub static GRID_HITS: Cell<usize> = const { Cell::new(0) };

        /// Count of streaming-walk entries.
        pub static WALK_ENTRIES: Cell<usize> = const { Cell::new(0) };

        /// Characters consumed by the streaming walk.
        pub static WALK_CHARS: Cell<usize> = const { Cell::new(0) };

        /// Full-content passes performed (classification or validation — must visit every byte).
        pub static FULL_SCANS: Cell<usize> = const { Cell::new(0) };

        /// Bytes examined by cheap probes (may bail at the first high bit).
        pub static PROBE_BYTES: Cell<usize> = const { Cell::new(0) };
    }

    pub fn reset() {
        GRID_HITS.with(|c| c.set(0));
        WALK_ENTRIES.with(|c| c.set(0));
        WALK_CHARS.with(|c| c.set(0));
        FULL_SCANS.with(|c| c.set(0));
        PROBE_BYTES.with(|c| c.set(0));
    }

    pub fn snapshot() -> (usize, usize, usize) {
        (GRID_HITS.with(Cell::get), WALK_ENTRIES.with(Cell::get), WALK_CHARS.with(Cell::get))
    }

    pub fn scans() -> (usize, usize) {
        (FULL_SCANS.with(Cell::get), PROBE_BYTES.with(Cell::get))
    }
}

/// Test-only scan accounting; no-ops compiled out of non-test builds.
#[inline]
fn count_full_scan() {
    #[cfg(test)]
    eq_probe::FULL_SCANS.with(|c| c.set(c.get() + 1));
}

#[inline]
fn count_probe_byte() {
    #[cfg(test)]
    eq_probe::PROBE_BYTES.with(|c| c.set(c.get() + 1));
}

/// Classification block size (§2.2.5): the blocked hybrid passes fetch each block from main memory once and may make
/// multiple passes while it is cache-resident.  Variance-controlled container measurement (9 trials, min/median/max)
/// put the vector pass's plateau at 16 KiB: ≥16 KiB runs a tight 26–27 GB/s, 512 B–2 KiB ~23 GB/s, and 4–8 KiB was
/// bimodal on the container VM (12–27 GB/s; unexplained — workspace re-benchmark is a listed chore).  Larger blocks do
/// lengthen the scalar-fallback span when non-ASCII appears mid-block; the 16 KiB choice optimizes the vector pass.
/// A tunable.
const CLASSIFY_BLOCK: usize = 16384;

/// First walk block: one cache line (§2.3.5).  Small early blocks only pay for operations that can *exit* early —
/// full-read passes (classification, the digest) gate uniform grid blocks, measured 4× faster on short strings than a
/// geometric ladder and free of the ladder's per-block overhead on long ones; the walk alone prepends this one small
/// block, bounding a first-bytes mismatch at ~9 ns instead of ~131 ns.  The block is a win by being small, not by being
/// scalar: at one cache line, vector and scalar folds cost the same.
const WALK_FIRST_BLOCK: usize = 64;

/// Fixed grid block boundaries (§2.2.5): the next multiple of CLASSIFY_BLOCK strictly after `pos` (which may sit a few
/// bytes past a boundary after a sequence straddle; the grid itself never moves).
fn block_end(pos: usize, len: usize) -> usize {
    ((pos / CLASSIFY_BLOCK + 1) * CLASSIFY_BLOCK).min(len)
}

/// Blocked hybrid full classification (§2.2.4/§2.2.5), implementing the single-fetch fusion law: each byte is fetched
/// from main memory once, and per cache-resident block one exitless SIMD high-bit pass gates the block — pure-ASCII
/// blocks contribute `chars += len` and are done; non-ASCII blocks fall to the scalar fused extended decoder over the
/// cached bytes.  Exitless inner loops are what auto-vectorize; early-exit semantics live at block granularity.  Blocks
/// end at fixed multiples of CLASSIFY_BLOCK: sequences straddling a boundary are handled without copying — the scalar
/// decoder's soft end is the grid boundary, but sequence reads bound against the full slice, so a straddling sequence
/// completes past the boundary and the next block runs from there to the *next grid multiple* (boundaries never drift;
/// a post-straddle block is merely a few bytes short).
///
/// One traversal (in the fetch sense) determines perl-validity, Rust-validity, both range facts, and the character
/// count.  Perl's extended validity, container-verified: surrogates, supra-Unicode, and the FE (7-byte) / FF (13-byte)
/// forms decode; overlongs (minimal-length rule at every width), bare continuations, and truncations are malformed;
/// values cap at perl's `IV_MAX`, 2^63-1.  Rust additionally rejects surrogates, values above U+10FFFF, and any
/// sequence longer than 4 bytes — decidable per-sequence during the same decode.
fn classify_full(bytes: &[u8]) -> (u8, usize) {
    count_full_scan();

    let mut facts = ScanFacts::default();
    let mut pos = 0usize;

    while pos < bytes.len() {
        let soft_end = block_end(pos, bytes.len());

        // Exitless SIMD gate over the block (a fold, not an early-exit scan — folds vectorize).
        let hi = bytes[pos..soft_end].iter().fold(0u8, |a, &b| a | b) & 0x80 != 0;
        if !hi {
            facts.chars += soft_end - pos; // ASCII block: characters are bytes; no further passes
            pos = soft_end;
            continue;
        }

        // Non-ASCII block: scalar fused decode over the cached bytes, running to at least soft_end and completing any
        // sequence that straddles it.
        match scalar_decode_span(bytes, pos, soft_end, &mut facts, |_| {}) {
            Some(next) => pos = next,
            None => return (scan::MALFORMED_UTF8, 0),
        }
    }

    (facts.state(), facts.chars)
}

/// Accumulated classification facts across blocks.
#[derive(Default)]
struct ScanFacts {
    saw_multibyte: bool,
    saw_beyond_latin1: bool,
    saw_rust_rejected: bool,
    chars: usize,
}

impl ScanFacts {
    fn state(&self) -> u8 {
        if self.saw_rust_rejected {
            scan::EXTENDED_UTF8
        } else if self.saw_beyond_latin1 {
            scan::UTF8_NON_LATIN1
        } else if self.saw_multibyte {
            scan::UTF8_LATIN1
        } else {
            scan::ASCII
        }
    }
}

/// The scalar fused extended decoder over `bytes[start..]`, decoding whole sequences until the position reaches
/// `soft_end` (a sequence beginning before `soft_end` completes past it; truncation is judged against the full slice).
/// Returns the position where decoding stopped, or `None` on malformed content.
fn scalar_decode_span(bytes: &[u8], start: usize, soft_end: usize, facts: &mut ScanFacts, mut emit: impl FnMut(u64)) -> Option<usize> {
    /// Minimum code-point value for each sequence length (minimal-length / anti-overlong rule).
    fn min_for_len(len: usize) -> u64 {
        match len {
            1 => 0,
            2 => 0x80,
            3 => 0x800,
            4 => 0x1_0000,
            5 => 0x20_0000,
            6 => 0x400_0000,
            7 => 0x8000_0000,     // FE form starts where 6-byte forms end (verified: chr(2**31) is FE)
            13 => 0x10_0000_0000, // FF form starts at 2**36 (verified: chr(2**36) is FF)
            _ => u64::MAX,
        }
    }

    let mut i = start;
    while i < soft_end {
        let lead = bytes[i];

        let (len, mut value): (usize, u64) = match lead {
            0x00..=0x7F => {
                facts.chars += 1;
                emit(lead as u64);
                i += 1;
                continue;
            }
            0xC0..=0xDF => (2, (lead & 0x1F) as u64),
            0xE0..=0xEF => (3, (lead & 0x0F) as u64),
            0xF0..=0xF7 => (4, (lead & 0x07) as u64),
            0xF8..=0xFB => (5, (lead & 0x03) as u64),
            0xFC..=0xFD => (6, (lead & 0x01) as u64),
            0xFE => (7, 0),
            0xFF => (13, 0),
            _ => return None, // bare continuation byte
        };

        if i + len > bytes.len() {
            return None; // truncated (judged against the full slice, not the block)
        }

        for &b in &bytes[i + 1..i + len] {
            if b & 0xC0 != 0x80 {
                return None; // malformed continuation
            }

            // 12 continuations x 6 bits = 72 bits could overflow u64, but any value needing the high bits exceeds
            // IV_MAX and is rejected; checked arithmetic keeps the reasoning airtight.
            value = match value.checked_mul(64) {
                Some(v) => v | (b & 0x3F) as u64,
                None => return None,
            };
        }

        if value < min_for_len(len) || value > 0x7FFF_FFFF_FFFF_FFFF {
            return None; // overlong for its form, or beyond IV_MAX
        }

        facts.saw_multibyte = true;
        facts.saw_beyond_latin1 |= value > 0xFF;
        facts.saw_rust_rejected |= len > 4 || value > 0x10_FFFF || (0xD800..=0xDFFF).contains(&value);
        facts.chars += 1;
        emit(value);
        i += len;
    }

    Some(i)
}

/// Blocked range classification of *already Rust-valid* bytes (§2.2.4): per cache-resident block, an exitless high-bit
/// gate (ASCII block: characters are bytes), then an exitless `≥ C4` fold — the first block containing such a lead
/// determines the answer (U+0100 begins at `C4 80`), a block-granular bail that legitimately forfeits the count.
/// Rust-validity of the input means no sequence straddles awkwardly: continuation bytes are never counted as characters
/// regardless of which block sees them.
fn classify_known_valid(bytes: &[u8]) -> (u8, usize) {
    count_full_scan();

    let mut saw_high = false;
    let mut chars = 0usize;
    let mut pos = 0usize;

    while pos < bytes.len() {
        let end = block_end(pos, bytes.len());
        let block = &bytes[pos..end];
        pos = end;

        let hi = block.iter().fold(0u8, |a, &b| a | b) & 0x80 != 0;
        if !hi {
            chars += block.len();
            continue;
        }

        if block.iter().fold(0u8, |a, &b| a | u8::from(b >= 0xC4)) != 0 {
            return (scan::UTF8_NON_LATIN1, 0); // answer determined; the block-granular bail forfeits the count
        }

        saw_high = true;
        chars += block.iter().map(|&b| usize::from(b & 0xC0 != 0x80)).sum::<usize>();
    }

    (if saw_high { scan::UTF8_LATIN1 } else { scan::ASCII }, chars)
}

/// Terminal scan state of an inline string (eagerly established at construction).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InlineScan {
    /// Entirely U+0000–U+007F.
    Ascii,

    /// Rust-valid, entirely U+0000–U+00FF, non-ASCII.
    Latin1,

    /// Rust-valid, contains a character ≥ U+0100.
    NonLatin1,

    /// Perl-decodable, Rust-invalid (§2.2.4): contains a Rust-rejected code point, hence ≥ U+0100.
    Extended,

    /// Malformed under perl's extended rules too.
    Malformed,
}

/// Storage kind of a `PerlString`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StorageKind {
    /// Inline: ≤ [`INLINE_MAX`] bytes in the enum payload, no allocation.
    Inline,

    /// Nibble-packed: 16-30 characters of digit-dense text in the enum payload, no allocation (§2.2.9).  The bytes do
    /// not exist in that form, so a borrowed view of them must be decoded into a caller-held buffer.
    Packed,

    /// Heap: a shared [`CowBuffer`].
    Heap,
}

/// Generates the folded-tag variant set and the accessors over it.  Variant names are written out explicitly (not
/// synthesized by identifier concatenation) so a grep for any variant finds this defining invocation.
macro_rules! define_perl_string {
    (
        inline: [ $( $iv:ident = ($iscan:ident, $iu:literal, $iw:literal, $it:literal) ),* $(,)? ],
        packed: [ $( $pv:ident = ($palpha:ident, $pfull:literal, $pu:literal, $pw:literal, $pt:literal) ),* $(,)? ],
        heap:   [ $( $hv:ident = ($hu:literal, $hw:literal, $ht:literal) ),* $(,)? ]
    ) => {
        /// A Perl string.  See the module documentation; the variant set is the folded tag (§2.2.3) and is an
        /// implementation detail — construct and inspect through the methods, never by matching variants directly.
        pub enum PerlString {
            $( #[doc(hidden)] $iv { buf: [u8; INLINE_MAX] }, )*
            $( #[doc(hidden)] $pv { nibbles: [u8; PACKED_BYTES] }, )*
            $( #[doc(hidden)] $hv(CowBuffer), )*
        }

        impl PerlString {
            /// The storage kind.
            pub fn storage_kind(&self) -> StorageKind {
                match self {
                    $( PerlString::$iv { .. } => StorageKind::Inline, )*
                    $( PerlString::$pv { .. } => StorageKind::Packed, )*
                    $( PerlString::$hv(_) => StorageKind::Heap, )*
                }
            }

            /// The Perl utf8 flag (semantic claim, not validity — see module docs).
            pub fn is_utf8(&self) -> bool {
                match self {
                    $( PerlString::$iv { .. } => $iu, )*
                    $( PerlString::$pv { .. } => $pu, )*
                    $( PerlString::$hv(_) => $hu, )*
                }
            }

            /// Whether the numification warning has fired for this value (§2.3.4).
            pub fn is_warned(&self) -> bool {
                match self {
                    $( PerlString::$iv { .. } => $iw, )*
                    $( PerlString::$pv { .. } => $pw, )*
                    $( PerlString::$hv(_) => $hw, )*
                }
            }

            /// Whether this value is tainted (§2.6).
            pub fn is_tainted(&self) -> bool {
                match self {
                    $( PerlString::$iv { .. } => $it, )*
                    $( PerlString::$pv { .. } => $pt, )*
                    $( PerlString::$hv(_) => $ht, )*
                }
            }

            /// Inline terminal scan state, or `None` for heap storage.
            pub fn inline_scan(&self) -> Option<InlineScan> {
                match self {
                    $( PerlString::$iv { .. } => Some(InlineScan::$iscan), )*
                    // Packed alphabets are ASCII by construction, so the scan state is fixed.
                    $( PerlString::$pv { .. } => Some(InlineScan::Ascii), )*
                    $( PerlString::$hv(_) => None, )*
                }
            }

            /// Rebuild an inline value with the given tag dimensions (payload preserved).  Internal: tag transitions go
            /// through the public monotone/setter methods.
            fn build_inline(scan: InlineScan, utf8: bool, warned: bool, tainted: bool, buf: [u8; INLINE_MAX]) -> PerlString {
                match (scan, utf8, warned, tainted) {
                    $( (InlineScan::$iscan, $iu, $iw, $it) => PerlString::$iv { buf }, )*
                }
            }

            /// Rebuild a heap value with the given tag dimensions (buffer preserved).  Build a packed value with the
            /// given alphabet, length family, and tag dimensions.
            fn build_packed(packed: Packed, utf8: bool, warned: bool, tainted: bool) -> PerlString {
                match (packed.alphabet, packed.full, utf8, warned, tainted) {
                    $( (PackedAlphabet::$palpha, $pfull, $pu, $pw, $pt) => PerlString::$pv { nibbles: packed.nibbles }, )*
                }
            }


            /// The payload behind the tag, borrowed.  Generated rather than hand-written: with three storage kinds the
            /// explicit variant lists ran past a hundred names, and the per-section repetition expresses it exactly.
            fn raw_parts(&self) -> RawParts<'_> {
                match self {
                    $( PerlString::$iv { buf } => RawParts::Inline { buf }, )*
                    $( PerlString::$pv { nibbles } => RawParts::Packed(Packed {
                        alphabet: PackedAlphabet::$palpha,
                        full: $pfull,
                        nibbles: *nibbles,
                    }), )*
                    $( PerlString::$hv(cb) => RawParts::Heap(cb), )*
                }
            }

            /// The payload behind the tag, owned — the shape mutation needs, since it rebuilds the tag afterward.
            fn into_raw(self) -> RawOwned {
                match self {
                    $( PerlString::$iv { buf } => RawOwned::Inline { scan: InlineScan::$iscan, buf }, )*
                    $( PerlString::$pv { nibbles } => RawOwned::Packed(Packed {
                        alphabet: PackedAlphabet::$palpha,
                        full: $pfull,
                        nibbles,
                    }), )*
                    $( PerlString::$hv(cb) => RawOwned::Heap(cb), )*
                }
            }

            fn build_heap(utf8: bool, warned: bool, tainted: bool, cb: CowBuffer) -> PerlString {
                match (utf8, warned, tainted) {
                    $( ($hu, $hw, $ht) => PerlString::$hv(cb), )*
                }
            }
        }
    };
}

define_perl_string! {
    inline: [
        InlineAscii                         = (Ascii,     false, false, false),
        InlineAsciiFlagged                  = (Ascii,     true,  false, false),
        InlineAsciiWarned                   = (Ascii,     false, true,  false),
        InlineAsciiFlaggedWarned            = (Ascii,     true,  true,  false),
        InlineAsciiTainted                  = (Ascii,     false, false, true),
        InlineAsciiFlaggedTainted           = (Ascii,     true,  false, true),
        InlineAsciiWarnedTainted            = (Ascii,     false, true,  true),
        InlineAsciiFlaggedWarnedTainted     = (Ascii,     true,  true,  true),
        InlineLatin1                        = (Latin1,    false, false, false),
        InlineLatin1Flagged                 = (Latin1,    true,  false, false),
        InlineLatin1Warned                  = (Latin1,    false, true,  false),
        InlineLatin1FlaggedWarned           = (Latin1,    true,  true,  false),
        InlineLatin1Tainted                 = (Latin1,    false, false, true),
        InlineLatin1FlaggedTainted          = (Latin1,    true,  false, true),
        InlineLatin1WarnedTainted           = (Latin1,    false, true,  true),
        InlineLatin1FlaggedWarnedTainted    = (Latin1,    true,  true,  true),
        InlineNonLatin1                     = (NonLatin1, false, false, false),
        InlineNonLatin1Flagged              = (NonLatin1, true,  false, false),
        InlineNonLatin1Warned               = (NonLatin1, false, true,  false),
        InlineNonLatin1FlaggedWarned        = (NonLatin1, true,  true,  false),
        InlineNonLatin1Tainted              = (NonLatin1, false, false, true),
        InlineNonLatin1FlaggedTainted       = (NonLatin1, true,  false, true),
        InlineNonLatin1WarnedTainted        = (NonLatin1, false, true,  true),
        InlineNonLatin1FlaggedWarnedTainted = (NonLatin1, true,  true,  true),
        InlineExtended                      = (Extended,  false, false, false),
        InlineExtendedFlagged               = (Extended,  true,  false, false),
        InlineExtendedWarned                = (Extended,  false, true,  false),
        InlineExtendedFlaggedWarned         = (Extended,  true,  true,  false),
        InlineExtendedTainted               = (Extended,  false, false, true),
        InlineExtendedFlaggedTainted        = (Extended,  true,  false, true),
        InlineExtendedWarnedTainted         = (Extended,  false, true,  true),
        InlineExtendedFlaggedWarnedTainted  = (Extended,  true,  true,  true),
        InlineMalformed                     = (Malformed, false, false, false),
        InlineMalformedFlagged              = (Malformed, true,  false, false),
        InlineMalformedWarned               = (Malformed, false, true,  false),
        InlineMalformedFlaggedWarned        = (Malformed, true,  true,  false),
        InlineMalformedTainted              = (Malformed, false, false, true),
        InlineMalformedFlaggedTainted       = (Malformed, true,  false, true),
        InlineMalformedWarnedTainted        = (Malformed, false, true,  true),
        InlineMalformedFlaggedWarnedTainted = (Malformed, true,  true,  true),
    ],
    packed: [
        PackedNum                                    = (Numeric     , false, false, false, false),
        PackedNumFlagged                             = (Numeric     , false, true , false, false),
        PackedNumWarned                              = (Numeric     , false, false, true , false),
        PackedNumFlaggedWarned                       = (Numeric     , false, true , true , false),
        PackedNumTainted                             = (Numeric     , false, false, false, true),
        PackedNumFlaggedTainted                      = (Numeric     , false, true , false, true),
        PackedNumWarnedTainted                       = (Numeric     , false, false, true , true),
        PackedNumFlaggedWarnedTainted                = (Numeric     , false, true , true , true),
        PackedNumFull                                = (Numeric     , true , false, false, false),
        PackedNumFullFlagged                         = (Numeric     , true , true , false, false),
        PackedNumFullWarned                          = (Numeric     , true , false, true , false),
        PackedNumFullFlaggedWarned                   = (Numeric     , true , true , true , false),
        PackedNumFullTainted                         = (Numeric     , true , false, false, true),
        PackedNumFullFlaggedTainted                  = (Numeric     , true , true , false, true),
        PackedNumFullWarnedTainted                   = (Numeric     , true , false, true , true),
        PackedNumFullFlaggedWarnedTainted            = (Numeric     , true , true , true , true),
        PackedPlus                                   = (DateTimePlus, false, false, false, false),
        PackedPlusFlagged                            = (DateTimePlus, false, true , false, false),
        PackedPlusWarned                             = (DateTimePlus, false, false, true , false),
        PackedPlusFlaggedWarned                      = (DateTimePlus, false, true , true , false),
        PackedPlusTainted                            = (DateTimePlus, false, false, false, true),
        PackedPlusFlaggedTainted                     = (DateTimePlus, false, true , false, true),
        PackedPlusWarnedTainted                      = (DateTimePlus, false, false, true , true),
        PackedPlusFlaggedWarnedTainted               = (DateTimePlus, false, true , true , true),
        PackedPlusFull                               = (DateTimePlus, true , false, false, false),
        PackedPlusFullFlagged                        = (DateTimePlus, true , true , false, false),
        PackedPlusFullWarned                         = (DateTimePlus, true , false, true , false),
        PackedPlusFullFlaggedWarned                  = (DateTimePlus, true , true , true , false),
        PackedPlusFullTainted                        = (DateTimePlus, true , false, false, true),
        PackedPlusFullFlaggedTainted                 = (DateTimePlus, true , true , false, true),
        PackedPlusFullWarnedTainted                  = (DateTimePlus, true , false, true , true),
        PackedPlusFullFlaggedWarnedTainted           = (DateTimePlus, true , true , true , true),
        PackedZulu                                   = (DateTimeZulu, false, false, false, false),
        PackedZuluFlagged                            = (DateTimeZulu, false, true , false, false),
        PackedZuluWarned                             = (DateTimeZulu, false, false, true , false),
        PackedZuluFlaggedWarned                      = (DateTimeZulu, false, true , true , false),
        PackedZuluTainted                            = (DateTimeZulu, false, false, false, true),
        PackedZuluFlaggedTainted                     = (DateTimeZulu, false, true , false, true),
        PackedZuluWarnedTainted                      = (DateTimeZulu, false, false, true , true),
        PackedZuluFlaggedWarnedTainted               = (DateTimeZulu, false, true , true , true),
        PackedZuluFull                               = (DateTimeZulu, true , false, false, false),
        PackedZuluFullFlagged                        = (DateTimeZulu, true , true , false, false),
        PackedZuluFullWarned                         = (DateTimeZulu, true , false, true , false),
        PackedZuluFullFlaggedWarned                  = (DateTimeZulu, true , true , true , false),
        PackedZuluFullTainted                        = (DateTimeZulu, true , false, false, true),
        PackedZuluFullFlaggedTainted                 = (DateTimeZulu, true , true , false, true),
        PackedZuluFullWarnedTainted                  = (DateTimeZulu, true , false, true , true),
        PackedZuluFullFlaggedWarnedTainted           = (DateTimeZulu, true , true , true , true),
    ],
    heap: [
        Heap                     = (false, false, false),
        HeapFlagged              = (true,  false, false),
        HeapWarned               = (false, true,  false),
        HeapFlaggedWarned        = (true,  true,  false),
        HeapTainted              = (false, false, true),
        HeapFlaggedTainted       = (true,  false, true),
        HeapWarnedTainted        = (false, true,  true),
        HeapFlaggedWarnedTainted = (true,  true,  true),
    ]
}

// ── Layout law (§2.3.6) ───────────────────────────────────────────
const _: () = assert!(size_of::<PerlString>() == 16);
const _: () = assert!(size_of::<Option<PerlString>>() == 16);

// ── Construction ──────────────────────────────────────────────────
/// Eager full scan of a short byte slice: terminal state (§2.2.3).
fn eager_scan(bytes: &[u8]) -> InlineScan {
    match classify_full(bytes).0 {
        scan::ASCII => InlineScan::Ascii,
        scan::UTF8_LATIN1 => InlineScan::Latin1,
        scan::UTF8_NON_LATIN1 => InlineScan::NonLatin1,
        scan::EXTENDED_UTF8 => InlineScan::Extended,
        _ => InlineScan::Malformed,
    }
}

/// The stored length of a NUL-terminated payload: the first NUL, or the whole payload when there is none.
///
/// The terminator is what buys the fifteenth byte — a length field would have spent it — and it is why content
/// containing NUL cannot be stored inline at all (§2.2.9): its own bytes would end it early.  Such content takes the
/// heap, which is the ruled placement.
fn inline_len(buf: &[u8; INLINE_MAX]) -> usize {
    buf.iter().position(|&b| b == 0).unwrap_or(INLINE_MAX)
}

/// Whether content can be stored inline: short enough, and free of the terminator.
fn inline_eligible(bytes: &[u8]) -> bool {
    bytes.len() <= INLINE_MAX && !bytes.contains(&0)
}

/// Concatenate and re-pack, or `None` when the result leaves the packed tier — too long, or not encodable in any
/// alphabet.
///
/// Re-classifying the whole result rather than widening the existing nibbles in place costs a decode the incremental
/// path would avoid, and buys canonicity for free: `pack` picks the alphabet by the priority order, so a string built
/// by appending is byte-identical to the same content constructed whole.
fn pack_grown(head: &[u8], tail: &[u8]) -> Option<Packed> {
    let new_len = head.len() + tail.len();
    if !(MIN_PACKED_LEN..=MAX_PACKED_LEN).contains(&new_len) {
        return None;
    }

    let mut combined = [0u8; MAX_PACKED_LEN];
    combined[..head.len()].copy_from_slice(head);
    combined[head.len()..new_len].copy_from_slice(tail);

    pack(&combined[..new_len])
}

fn inline_payload(bytes: &[u8]) -> [u8; INLINE_MAX] {
    debug_assert!(inline_eligible(bytes));
    let mut buf = [0u8; INLINE_MAX];
    buf[..bytes.len()].copy_from_slice(bytes);

    buf
}

impl PerlString {
    /// Construct from raw bytes (I/O, `Encode`, lexer literals).  Unflagged; inline content gets its eager terminal
    /// scan, heap content defers all scanning (`UNKNOWN`), per §2.2.7.
    pub fn from_bytes(bytes: &[u8]) -> Result<PerlString, AllocError> {
        match PerlString::inline_bytes(bytes) {
            Some(inline) => Ok(inline),
            None => {
                let cb = CowBuffer::from_slice(bytes)?; // scan byte born UNKNOWN
                Ok(PerlString::build_heap(false, false, false, cb))
            }
        }
    }

    /// The empty string: inline, unflagged, trivially ASCII.  Infallible, unlike the other constructors — an empty
    /// payload needs no allocation — which is also what lets `Default` exist.
    pub fn empty() -> PerlString {
        PerlString::build_inline(InlineScan::Ascii, false, false, false, [0u8; INLINE_MAX])
    }

    /// Construct from a Rust `&str` **without allocating**, or `None` if the content cannot be stored in the value
    /// itself.  Flagging follows [`FromStr`](std::str::FromStr): ASCII stores unflagged, non-ASCII flagged.
    ///
    /// The contract is the guarantee, not a byte count: `Some` means no heap allocation occurred, so the set of
    /// accepted content widens whenever the non-allocating storage forms do.  Callers who merely prefer inline storage
    /// can write `PerlString::inline(s).unwrap_or_default()`; callers who need the content stored either way should use
    /// the fallible constructors instead.
    pub fn inline(s: impl AsRef<str>) -> Option<PerlString> {
        let s = s.as_ref();
        let bytes = s.as_bytes();
        if bytes.len() > INLINE_MAX || bytes.contains(&0) {
            // The packed tier's band begins exactly where the inline payload ends, and it allocates nothing either.
            // Past the band there is no non-allocating form, so this is where `None` starts meaning "the heap".
            if !(MIN_PACKED_LEN..=MAX_PACKED_LEN).contains(&bytes.len()) {
                return None;
            }

            return pack(bytes).map(|p| PerlString::build_packed(p, false, false, false));
        }

        let state = eager_scan(bytes); // Ascii or Utf8NonAscii; Malformed/Extended impossible from &str.

        Some(PerlString::build_inline(state, state != InlineScan::Ascii, false, false, inline_payload(bytes)))
    }

    /// Construct from raw bytes **without allocating**, or `None` if the content cannot be stored in the value itself.
    /// Unflagged, like [`PerlString::from_bytes`]; the same guarantee-not-a-count contract as [`PerlString::inline`].
    pub fn inline_bytes(bytes: impl AsRef<[u8]>) -> Option<PerlString> {
        let bytes = bytes.as_ref();
        if bytes.len() > INLINE_MAX || bytes.contains(&0) {
            if !(MIN_PACKED_LEN..=MAX_PACKED_LEN).contains(&bytes.len()) {
                return None;
            }

            return pack(bytes).map(|p| PerlString::build_packed(p, false, false, false));
        }

        Some(PerlString::build_inline(eager_scan(bytes), false, false, false, inline_payload(bytes)))
    }

    // ── Accessors ─────────────────────────────────────────────────
    /// Length in bytes.  No dereference for inline; handle mirror for heap.
    pub fn len(&self) -> usize {
        match self.raw_parts() {
            RawParts::Inline { buf } => inline_len(buf),
            RawParts::Packed(p) => p.len(),
            RawParts::Heap(cb) => cb.len(),
        }
    }

    /// Whether the string is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The raw bytes.
    ///
    /// Borrowed from the string where the bytes exist in that form, and from `scratch` where they do not — packed
    /// content is nibbles, so its bytes have to be decoded somewhere, and a buffer built inside this call could not
    /// outlive it.  The caller supplies one stack array and never learns which case it got, which is what lets the
    /// storage forms multiply without every consumer following along.
    pub fn as_bytes<'a>(&'a self, scratch: &'a mut [u8; DECODE_MAX]) -> &'a [u8] {
        match self.raw_parts() {
            RawParts::Inline { buf } => &buf[..inline_len(buf)],
            RawParts::Packed(p) => {
                let (decoded, len) = p.unpack();
                scratch[..len].copy_from_slice(&decoded[..len]);
                &scratch[..len]
            }
            RawParts::Heap(cb) => cb.as_slice(),
        }
    }

    /// View as a Rust `&str` if the bytes are valid UTF-8 (a fact question, independent of the Perl flag).  Narrows the
    /// heap scan lattice as a side effect (§2.2.5); sound through `&self`.
    pub fn as_str<'a>(&'a self, scratch: &'a mut [u8; DECODE_MAX]) -> Option<&'a str> {
        match self.raw_parts() {
            RawParts::Packed(p) => {
                // Every packed alphabet is ASCII, so the decoded bytes are always valid.
                let (decoded, len) = p.unpack();
                scratch[..len].copy_from_slice(&decoded[..len]);
                str::from_utf8(&scratch[..len]).ok()
            }
            RawParts::Inline { buf } => {
                let bytes = &buf[..inline_len(buf)];
                match self.inline_scan() {
                    // SAFETY: terminal scan states were established by a full validity scan at construction and inline
                    // mutation re-scans; Ascii, Latin1, and NonLatin1 all certify Rust-valid UTF-8.
                    Some(InlineScan::Ascii) | Some(InlineScan::Latin1) | Some(InlineScan::NonLatin1) => Some(unsafe { str::from_utf8_unchecked(bytes) }),
                    _ => None,
                }
            }
            RawParts::Heap(cb) => {
                let bytes = cb.as_slice();
                match cb.scan() {
                    // SAFETY: these lattice states certify prior successful validation of these exact bytes (states
                    // only narrow; mutation resets to UNKNOWN).
                    st if scan::is_rust_valid(st) => Some(unsafe { str::from_utf8_unchecked(bytes) }),
                    scan::MALFORMED_UTF8 | scan::EXTENDED_UTF8 => None,
                    _ => {
                        let (st, chars) = classify_full(bytes); // one pass: validity (both tiers) + range + count
                        cb.narrow_scan(st);

                        if chars > 0 {
                            cb.set_char_count(chars);
                        }

                        if scan::is_rust_valid(st) {
                            // SAFETY: classify_full certifies Rust-valid states only for byte content that decoded
                            // cleanly within Rust's accepted range.
                            Some(unsafe { str::from_utf8_unchecked(bytes) })
                        } else {
                            None
                        }
                    }
                }
            }
        }
    }

    /// Whether the content is pure 7-bit ASCII.  Narrows the heap lattice (§2.2.5).
    pub fn is_ascii(&self) -> bool {
        match self.raw_parts() {
            RawParts::Inline { .. } => self.inline_scan() == Some(InlineScan::Ascii),
            // Every symbol of every packed alphabet is ASCII, so this is a constant rather than a question about
            // content — unlike the inline forms, whose bytes are whatever they are.
            RawParts::Packed(_) => true,
            RawParts::Heap(cb) => match cb.scan() {
                scan::ASCII => true,
                scan::UTF8_LATIN1 | scan::UTF8_NON_LATIN1 | scan::UTF8_NON_ASCII | scan::MALFORMED_UTF8 | scan::NON_ASCII | scan::EXTENDED_UTF8 => false,
                scan::UTF8_UNKNOWN_RANGE => {
                    // Cheap probe: bail at the first high bit; range stays deferred (§2.2.4/§2.2.5).
                    let ascii = cb.as_slice().iter().all(|b| {
                        count_probe_byte();
                        b.is_ascii()
                    });

                    cb.narrow_scan(if ascii { scan::ASCII } else { scan::UTF8_NON_ASCII });

                    ascii
                }
                _ => {
                    let ascii = cb.as_slice().iter().all(|b| {
                        count_probe_byte();
                        b.is_ascii()
                    });

                    cb.narrow_scan(if ascii { scan::ASCII } else { scan::NON_ASCII });

                    ascii
                }
            },
        }
    }

    /// The current scan state in the heap encoding (§2.2.4), inline terminals mapped through.  Reads existing knowledge
    /// only; performs no scan.
    fn scan_state(&self) -> u8 {
        match self.raw_parts() {
            RawParts::Inline { .. } => match self.inline_scan() {
                Some(st) => inline_scan_to_heap(st),
                None => scan::UNKNOWN, // unreachable by construction
            },
            RawParts::Packed(_) => scan::ASCII,
            RawParts::Heap(cb) => cb.scan(),
        }
    }

    /// Whether the bytes are valid under perl's *extended* UTF-8 rules (§2.2.4) — the predicate character-level
    /// operations on flagged strings use.  Narrows the heap lattice.
    pub fn is_perl_utf8_valid(&self) -> bool {
        match self.raw_parts() {
            RawParts::Inline { .. } => !matches!(self.inline_scan(), Some(InlineScan::Malformed)),
            RawParts::Packed(_) => true, // ASCII is valid under every reading.
            RawParts::Heap(cb) => match cb.scan() {
                st if scan::is_perl_decodable(st) => true,
                scan::MALFORMED_UTF8 => false,
                _ => {
                    let (st, chars) = classify_full(cb.as_slice()); // the single pass
                    cb.narrow_scan(st);

                    if chars > 0 {
                        cb.set_char_count(chars);
                    }

                    scan::is_perl_decodable(st)
                }
            },
        }
    }

    /// Character length under perl's flagged semantics (§2.2.4): the character count of the decoded content.  `None`
    /// iff the content is malformed under perl's extended rules (the ops layer owns perl's malformed-length warning
    /// behavior).  For unflagged strings perl's `length()` is byte length — callers pick the primitive by flag; this
    /// one is the flagged-side answer.  O(1) after first classification; cached per-buffer, shared across COW sharers.
    pub fn char_len(&self) -> Option<usize> {
        match self.raw_parts() {
            // Packed alphabets are ASCII, so every character is one byte.
            RawParts::Packed(p) => Some(p.len()),
            RawParts::Inline { buf } => {
                let len = inline_len(buf);
                let bytes = &buf[..len];
                match self.inline_scan() {
                    Some(InlineScan::Ascii) => Some(len),
                    Some(InlineScan::Malformed) | None => None,
                    _ => {
                        let (_, chars) = classify_full(bytes); // at most fifteen bytes: recount is trivial
                        Some(chars)
                    }
                }
            }
            RawParts::Heap(cb) => match cb.scan() {
                scan::ASCII => Some(cb.len()),
                scan::MALFORMED_UTF8 => None,
                _ => {
                    let cached = cb.char_count();
                    if cached > 0 {
                        return Some(cached);
                    }

                    let (st, chars) = classify_full(cb.as_slice()); // one pass classifies AND counts
                    cb.narrow_scan(st);

                    if st == scan::MALFORMED_UTF8 {
                        None
                    } else {
                        cb.set_char_count(chars);
                        Some(chars)
                    }
                }
            },
        }
    }

    // ── Tag transitions ───────────────────────────────────────────
    /// Mark the numification warning as fired.  Monotone: there is no clearing method (§2.3.4).
    pub fn mark_warned(&mut self) {
        self.rebuild_tag(|_u, _w, _t| (_u, true, _t));
    }

    /// Set or propagate the taint bit.  Monotone raise; clearing is the laundering capability's alone (§2.6.2).
    pub fn taint(&mut self) {
        self.rebuild_tag(|u, w, _t| (u, w, true));
    }

    /// Clear the taint bit.  Non-public: reachable only through the two sanctioned laundering paths (§2.6.2) — capture
    /// materialization and hash-key canonicalization, both inside perl-core.
    pub(crate) fn untaint_for_sanctioned_path(&mut self) {
        self.rebuild_tag(|u, w, _t| (u, w, false));
    }

    fn rebuild_tag(&mut self, f: impl FnOnce(bool, bool, bool) -> (bool, bool, bool)) {
        let (u, w, t) = (self.is_utf8(), self.is_warned(), self.is_tainted());
        let (u2, w2, t2) = f(u, w, t);

        if (u, w, t) == (u2, w2, t2) {
            return;
        }

        let old = mem::take(self);

        *self = match old.into_raw() {
            RawOwned::Inline { scan, buf } => PerlString::build_inline(scan, u2, w2, t2, buf),
            RawOwned::Packed(p) => PerlString::build_packed(p, u2, w2, t2),
            RawOwned::Heap(cb) => PerlString::build_heap(u2, w2, t2, cb),
        };
    }

    // ── Mutation ──────────────────────────────────────────────────
    /// Append the bytes of a Rust `&str`, applying the §2.2.5 transition rules (valid-UTF-8 append preserves validity;
    /// ASCII append cannot change anything; inline overflow promotes to heap, one-way).
    pub fn push_str(&mut self, s: &str) -> Result<(), AllocError> {
        let (class, chars) = classify_known_valid(s.as_bytes());
        self.push_raw(s.as_bytes(), AppendKind::Valid { class, chars })
    }

    /// Append raw bytes.  Content knowledge resets per the blanket rule (§2.2.5) except where the appended bytes' own
    /// scan preserves it.
    pub fn push_bytes(&mut self, bytes: &[u8]) -> Result<(), AllocError> {
        let kind = if bytes.iter().all(|b| b.is_ascii()) {
            // Pure ASCII bytes: strongest knowledge, cheap to establish; characters == bytes.
            AppendKind::Valid { class: scan::ASCII, chars: bytes.len() }
        } else {
            AppendKind::Unknown
        };

        self.push_raw(bytes, kind)
    }

    fn push_raw(&mut self, bytes: &[u8], kind: AppendKind) -> Result<(), AllocError> {
        if bytes.is_empty() {
            return Ok(());
        }

        let (u, w, t) = (self.is_utf8(), self.is_warned(), self.is_tainted());
        let old = mem::take(self);

        *self = match old.into_raw() {
            RawOwned::Inline { scan, buf } => {
                let len = inline_len(&buf);
                let old_bytes = &buf[..len];
                let new_len = len + bytes.len();

                // A NUL among the appended bytes would terminate the payload early, so such content leaves the inline
                // forms even when it would otherwise fit (§2.2.9).
                if new_len <= INLINE_MAX && !bytes.contains(&0) {
                    let mut nbuf = buf;
                    nbuf[len..new_len].copy_from_slice(bytes);
                    let nscan = append_transition_inline(scan, kind, &nbuf[..new_len]);
                    PerlString::build_inline(nscan, u, w, t, nbuf)
                } else if let Some(packed) = pack_grown(old_bytes, bytes) {
                    // Outgrowing the inline payload does not mean the heap: the packed tier's band starts exactly where
                    // the inline one ends.
                    PerlString::build_packed(packed, u, w, t)
                } else {
                    // Promote to heap (one-way).  Fold the append into the promoting allocation.
                    let mut cb = CowBuffer::with_capacity(new_len + (new_len >> 2))?;
                    cb.extend_from_slice(old_bytes)?;
                    cb.extend_from_slice(bytes)?;
                    cb.narrow_scan(append_transition_heap(inline_scan_to_heap(scan), kind));
                    PerlString::build_heap(u, w, t, cb)
                }
            }
            RawOwned::Packed(p) => {
                let (decoded, len) = p.unpack();
                let old_bytes = &decoded[..len];

                if let Some(packed) = pack_grown(old_bytes, bytes) {
                    PerlString::build_packed(packed, u, w, t)
                } else {
                    // Past the band, or no longer alphabet-conformant.  Packed content is ASCII, so the heap state
                    // starts from there.
                    let new_len = len + bytes.len();
                    let mut cb = CowBuffer::with_capacity(new_len + (new_len >> 2))?;
                    cb.extend_from_slice(old_bytes)?;
                    cb.extend_from_slice(bytes)?;
                    cb.narrow_scan(append_transition_heap(scan::ASCII, kind));
                    PerlString::build_heap(u, w, t, cb)
                }
            }
            RawOwned::Heap(mut cb) => {
                let prior = cb.scan();
                let prior_chars = cb.char_count();
                cb.extend_from_slice(bytes)?; // resets buffer scan and count to unknown
                cb.narrow_scan(append_transition_heap(prior, kind));

                // Maintain the character count incrementally when both sides know theirs (§2.2.5): the appended
                // content's own classification counted its characters in its own pass.
                if let AppendKind::Valid { chars: added, .. } = kind
                    && prior_chars > 0
                    && added > 0
                    && scan::is_perl_decodable(cb.scan())
                {
                    cb.set_char_count(prior_chars + added);
                }
                PerlString::build_heap(u, w, t, cb)
            }
        };

        Ok(())
    }
}

enum RawParts<'a> {
    Inline { buf: &'a [u8; INLINE_MAX] },
    Packed(Packed),
    Heap(&'a CowBuffer),
}

enum RawOwned {
    Inline { scan: InlineScan, buf: [u8; INLINE_MAX] },
    Packed(Packed),
    Heap(CowBuffer),
}

/// What is known about appended content, for the §2.2.5 transition rules.  For Rust-valid content the range is carried
/// (join semantics: the result range is the max of the operand ranges, §2.2.5).
#[derive(Clone, Copy, PartialEq)]
enum AppendKind {
    /// Known valid UTF-8, with its terminal classification (scan::ASCII / UTF8_LATIN1 / UTF8_NON_LATIN1) and character
    /// count (0 when the classification bailed early — count forfeited, class still exact).
    Valid { class: u8, chars: usize },

    /// Nothing known.
    Unknown,
}

fn inline_scan_to_heap(s: InlineScan) -> u8 {
    match s {
        InlineScan::Ascii => scan::ASCII,
        InlineScan::Latin1 => scan::UTF8_LATIN1,
        InlineScan::NonLatin1 => scan::UTF8_NON_LATIN1,
        InlineScan::Extended => scan::EXTENDED_UTF8,
        InlineScan::Malformed => scan::MALFORMED_UTF8,
    }
}

/// §2.2.5 append transitions for an inline result.  Inline states are terminal, and the appended region is small, so
/// degraded knowledge is recovered by an eager re-scan of the (≤ 22-byte) result rather than tracked lazily.
fn append_transition_inline(prior: InlineScan, kind: AppendKind, result: &[u8]) -> InlineScan {
    match (prior, kind) {
        // Valid + valid: the range join (§2.2.5).
        (InlineScan::Ascii, AppendKind::Valid { class: scan::ASCII, .. }) => InlineScan::Ascii,
        (InlineScan::Ascii | InlineScan::Latin1, AppendKind::Valid { class: scan::ASCII | scan::UTF8_LATIN1, .. }) => InlineScan::Latin1,
        (
            InlineScan::Ascii | InlineScan::Latin1 | InlineScan::NonLatin1,
            AppendKind::Valid { class: scan::ASCII | scan::UTF8_LATIN1 | scan::UTF8_NON_LATIN1, .. },
        ) => InlineScan::NonLatin1,

        // Perl-decodable content of any kind appended to extended: the Rust-rejected code point is still there.
        (InlineScan::Extended, AppendKind::Valid { .. }) => InlineScan::Extended,

        // Anything else: inline is small — rescue full knowledge with an eager re-scan.
        _ => eager_scan(result),
    }
}

/// §2.2.5 append transitions for a heap result, from the buffer's prior state and the appended content's kind.
fn append_transition_heap(prior: u8, kind: AppendKind) -> u8 {
    match kind {
        // Appending pure ASCII: no state change (cannot raise the range or affect validity).
        AppendKind::Valid { class: scan::ASCII, .. } => prior,
        AppendKind::Valid { class, .. } => match prior {
            // Valid + valid: the range join — result range is the max of the two (§2.2.5).
            scan::ASCII | scan::UTF8_LATIN1 | scan::UTF8_NON_LATIN1 => prior.max(class),

            // Range-unresolved priors: the addition can prove non-ASCII or beyond-Latin-1, never below.
            scan::UTF8_UNKNOWN_RANGE if class == scan::UTF8_NON_LATIN1 => scan::UTF8_NON_LATIN1,
            scan::UTF8_UNKNOWN_RANGE if class == scan::UTF8_LATIN1 => scan::UTF8_NON_ASCII,
            scan::UTF8_UNKNOWN_RANGE => scan::UTF8_UNKNOWN_RANGE,
            scan::UTF8_NON_ASCII if class == scan::UTF8_NON_LATIN1 => scan::UTF8_NON_LATIN1,
            scan::UTF8_NON_ASCII => scan::UTF8_NON_ASCII,

            // Perl-decodable onto extended: the Rust-rejected code point is still there.
            scan::EXTENDED_UTF8 => scan::EXTENDED_UTF8,

            // Prior validity unknown or invalid: blanket fallback, lazily recoverable (always correct).
            _ => scan::UNKNOWN,
        },
        AppendKind::Unknown => scan::UNKNOWN,
    }
}

// ── Character-sequence equality and hashing (§2.3.5) ──────────────
/// Iterate the character sequence of a *flagged* string as far as standard UTF-8 decoding reaches.
///
/// Extended and malformed regions are *tokenized* (offset past the character space) rather than decoded: for equality
/// and hashing this is exact, because every such token corresponds to a code point above 0xFF or a malformed byte,
/// neither of which can equal any Latin-1 character from the unflagged side (§2.2.4).  The full extended decoder
/// arrives with the character-operations design.
#[cfg(test)]
fn flagged_chars(bytes: &[u8]) -> impl Iterator<Item = u32> + '_ {
    struct Chars<'a> {
        rest: &'a [u8],
        raw_fallback: bool,
    }

    impl<'a> Iterator for Chars<'a> {
        type Item = u32;
        fn next(&mut self) -> Option<u32> {
            if self.rest.is_empty() {
                return None;
            }

            if self.raw_fallback {
                let b = self.rest[0];
                self.rest = &self.rest[1..];

                // Offset raw bytes past char space so they can never equal a genuine character from the other side
                // (prevents false equality during the interim fallback).
                return Some(0x8000_0000 | b as u32);
            }

            match str::from_utf8(&self.rest[..self.rest.len().min(4)]) {
                Ok(s) => {
                    let c = s.chars().next()?;
                    self.rest = &self.rest[c.len_utf8()..];
                    Some(c as u32)
                }
                Err(e) if e.valid_up_to() > 0 => {
                    // SAFETY: valid_up_to bytes are certified valid UTF-8.
                    let s = unsafe { str::from_utf8_unchecked(&self.rest[..e.valid_up_to()]) };
                    let c = s.chars().next()?;
                    self.rest = &self.rest[c.len_utf8()..];
                    Some(c as u32)
                }
                Err(_) => {
                    self.raw_fallback = true;
                    self.next()
                }
            }
        }
    }

    Chars { rest: bytes, raw_fallback: false }
}

impl PerlString {
    // ── Numeric and boolean interpretation (§2.2.2, §2.3.4) ───────
    // These live here rather than at the call site because they are questions about a string's *content*, and the
    // representation that holds that content is this type's business.  A caller asking `s.to_int()` needs no view of
    // the bytes, so no scratch buffer and no decision about which storage form it is looking at — which is what lets
    // the storage forms multiply without every consumer learning about them.

    /// Perl truthiness: every string is true but `""` and `"0"` (§2.3.3).
    pub fn to_bool(&self) -> bool {
        let mut scratch = [0u8; DECODE_MAX];
        !matches!(self.as_bytes(&mut scratch), b"" | b"0")
    }

    /// Perl's integer numification, as `int` and integer context see it — the visible i64, wrapping past the range
    /// exactly as perl's cast does (§2.2.2).
    pub fn to_int(&self) -> i64 {
        let mut scratch = [0u8; DECODE_MAX];
        crate::value::parse_int_i64_visible(self.as_bytes(&mut scratch))
    }

    /// Perl's float numification: leading-numeric prefix, `Inf`/`NaN` forms, zero for a non-numeric string.
    pub fn to_float(&self) -> f64 {
        let mut scratch = [0u8; DECODE_MAX];
        crate::value::parse_float(self.as_bytes(&mut scratch))
    }

    /// How this string numifies — integer or float — under the deferred-UV rule (§2.2.2).
    pub fn numify(&self) -> crate::value::Numeric {
        let mut scratch = [0u8; DECODE_MAX];
        crate::value::classify_numeric(self.as_bytes(&mut scratch))
    }

    /// Whether numifying this string would emit perl's `Argument isn't numeric` warning (§2.3.4).  A question about the
    /// content; whether the warning has *already* fired is [`PerlString::is_warned`].
    pub fn would_warn(&self) -> bool {
        let mut scratch = [0u8; DECODE_MAX];
        crate::value::string_would_warn(self.as_bytes(&mut scratch))
    }
}

impl fmt::Write for PerlString {
    /// Append formatted text.  The only failure this can encounter is allocation, which `fmt::Error` cannot carry — use
    /// [`PerlString::push_fmt`] where the distinction matters; this impl exists so that `write!` works.
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push_str(s).map_err(|_| fmt::Error)
    }
}

impl PerlString {
    /// Append formatted text, reporting allocation failure precisely: `write!(s, ...)` through the [`fmt::Write`] impl
    /// flattens that into `fmt::Error`, which carries nothing.
    ///
    /// Formatting straight into the string is the point — rendering into a scratch buffer and copying the result in
    /// would allocate a second time for content the string can usually hold itself.
    pub fn push_fmt(&mut self, args: fmt::Arguments<'_>) -> Result<(), AllocError> {
        // `fmt::Error` carries nothing, so the real error is captured on the way past.
        struct Sink<'a> {
            target: &'a mut PerlString,
            failure: Option<AllocError>,
        }

        impl fmt::Write for Sink<'_> {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                self.target.push_str(s).map_err(|e| {
                    self.failure = Some(e);
                    fmt::Error
                })
            }
        }

        let mut sink = Sink { target: self, failure: None };
        match fmt::write(&mut sink, args) {
            Ok(()) => Ok(()),

            // A failure with nothing captured means a `Display` impl among the arguments failed on its own account —
            // exotic, and reported here as a zero-size allocation failure rather than growing a second error type.
            Err(_) => Err(sink.failure.unwrap_or(AllocError { requested: 0 })),
        }
    }
}

impl Default for PerlString {
    /// The empty string, per [`PerlString::empty`].
    fn default() -> PerlString {
        PerlString::empty()
    }
}

impl std::str::FromStr for PerlString {
    type Err = AllocError;

    /// Construct from a Rust `&str`.  ASCII content is stored unflagged (the canonical downgraded form, §2.3.5);
    /// non-ASCII content is stored with the utf8 flag, its validity known from the type.  Allocation failure is the
    /// only error.
    fn from_str(s: &str) -> Result<PerlString, AllocError> {
        if let Some(inline) = PerlString::inline(s) {
            Ok(inline)
        } else {
            let bytes = s.as_bytes();
            let cb = CowBuffer::from_slice(bytes)?;
            let ascii = bytes.iter().all(|b| b.is_ascii());
            cb.narrow_scan(if ascii { scan::ASCII } else { scan::UTF8_UNKNOWN_RANGE });
            Ok(PerlString::build_heap(!ascii, false, false, cb))
        }
    }
}

macro_rules! grid_hit {
    () => {
        #[cfg(test)]
        eq_probe::GRID_HITS.with(|c| c.set(c.get() + 1));
    };
}

impl PartialEq for PerlString {
    /// The §2.3.5 equality inference grid, then the single streaming dual-direction compare.  Consults existing scan
    /// knowledge only — never scans twice, never pre-scans.
    fn eq(&self, other: &PerlString) -> bool {
        let (sa, sb) = (self.scan_state(), other.scan_state());

        if self.is_utf8() == other.is_utf8() {
            // Grid row 2: same flags, both terminal, states differ ⇒ byte contents differ (exclusivity law).
            if scan::is_terminal(sa) && scan::is_terminal(sb) && sa != sb {
                grid_hit!();
                return false;
            }

            // Flagged Rust-invalid terminal vs known Rust-valid: valid bytes never equal invalid bytes.
            if (scan::is_terminal(sa) && !scan::is_rust_valid(sa) && scan::is_rust_valid(sb))
                || (scan::is_terminal(sb) && !scan::is_rust_valid(sb) && scan::is_rust_valid(sa))
            {
                grid_hit!();
                return false;
            }

            // Same interpretation: byte equality is character equality (length check is memcmp's first move).
            let (mut ls, mut rs) = ([0u8; DECODE_MAX], [0u8; DECODE_MAX]);
            return self.as_bytes(&mut ls) == other.as_bytes(&mut rs);
        }

        let (flagged, plain) = if self.is_utf8() { (self, other) } else { (other, self) };
        let (sf, sp) = if self.is_utf8() { (sa, sb) } else { (sb, sa) };

        // Grid row 1: length rows (O(1) — lengths live in handles).
        if plain.len() > flagged.len() {
            grid_hit!();
            return false; // character count never exceeds byte count
        }

        if (sf == scan::UTF8_LATIN1 || sf == scan::UTF8_NON_ASCII) && plain.len() == flagged.len() {
            grid_hit!();
            return false; // a multi-byte sequence forces char count < byte count
        }

        // Grid row 3: ASCII vs known-non-ASCII, either orientation.
        if (sf == scan::ASCII && scan::is_known_non_ascii(sp)) || (sp == scan::ASCII && scan::is_known_non_ascii(sf)) {
            grid_hit!();
            return false;
        }

        // Grid row 4: cross-flag range disjointness and the malformed rule.
        if scan::is_known_beyond_latin1(sf) || sf == scan::MALFORMED_UTF8 {
            grid_hit!();
            return false;
        }

        // Undecided: the blocked streaming dual-direction compare (§2.3.5) — the walk under the single-fetch law's
        // block architecture.  Per ladder block of the flagged side, an exitless high-bit gate: a pure-ASCII block
        // means characters are bytes there, so the whole span compares against the plain side's slice as one memcmp
        // (hand-SIMD with internal early exits); a non-ASCII block falls to the scalar dual-cursor over the cached
        // bytes, sequences completing past the soft end (the straddle rule).  The ladder bounds early-mismatch waste
        // at one cache line.  An undecodable flagged sequence (extended or malformed) returns false directly: its
        // tokenized characters sit above the character space and can never equal a plain byte.
        #[cfg(test)]
        eq_probe::WALK_ENTRIES.with(|c| c.set(c.get() + 1));

        let (mut fs, mut ps) = ([0u8; DECODE_MAX], [0u8; DECODE_MAX]);
        let fb = flagged.as_bytes(&mut fs);
        let pb = plain.as_bytes(&mut ps);
        let mut saw_non_ascii = false;
        let (mut i, mut j) = (0usize, 0usize);

        while i < fb.len() {
            // The walk's two-step schedule (§2.3.5): a single cache-line first block bounds early-mismatch cost; every
            // later boundary is the uniform grid.
            let end = if i == 0 { WALK_FIRST_BLOCK.min(fb.len()) } else { block_end(i, fb.len()) };

            let hi = fb[i..end].iter().fold(0u8, |a, &b| a | b) & 0x80 != 0;
            if !hi {
                let n = end - i;
                #[cfg(test)]
                eq_probe::WALK_CHARS.with(|w| w.set(w.get() + n));

                if j + n > pb.len() || fb[i..end] != pb[j..j + n] {
                    return false;
                }

                i = end;
                j += n;
                continue;
            }

            // Non-ASCII block: scalar dual-cursor over the cached bytes.
            while i < end {
                let win_end = (i + 4).min(fb.len());

                let (c, len) = match str::from_utf8(&fb[i..win_end]) {
                    Ok(w) => match w.chars().next() {
                        Some(ch) => (ch as u32, ch.len_utf8()),
                        None => return false,
                    },
                    Err(e) if e.valid_up_to() > 0 => {
                        // SAFETY: the error reports a valid prefix of this exact window.
                        let w = unsafe { str::from_utf8_unchecked(&fb[i..i + e.valid_up_to()]) };
                        match w.chars().next() {
                            Some(ch) => (ch as u32, ch.len_utf8()),
                            None => return false,
                        }
                    }

                    // Extended or malformed: tokenized characters can never equal a plain byte.
                    Err(_) => return false,
                };

                #[cfg(test)]
                eq_probe::WALK_CHARS.with(|w| w.set(w.get() + 1));

                if j >= pb.len() || c != pb[j] as u32 {
                    return false;
                }

                saw_non_ascii |= pb[j] >= 0x80;
                i += len;
                j += 1;
            }
        }

        if j != pb.len() {
            return false;
        }

        // Completed walk: equality proven, and with it both sides' range (all characters ≤ U+00FF).
        if let RawParts::Heap(cb) = flagged.raw_parts() {
            cb.narrow_scan(if saw_non_ascii { scan::UTF8_LATIN1 } else { scan::ASCII });
        }

        if let RawParts::Heap(cb) = plain.raw_parts() {
            cb.narrow_scan(if saw_non_ascii { scan::NON_ASCII } else { scan::ASCII });
        }

        true
    }
}
impl Eq for PerlString {}

impl Hash for PerlString {
    /// Canonical downgraded-when-possible form (§2.3.5), routed through an internal 64-bit content digest: the `Hasher`
    /// API cannot fork mid-stream, and the single-fetch dual calculation (below) must run two candidate hashers and
    /// pick the winner at the end, so every string writes its digest — one `write_u64` — for cross-provenance
    /// consistency.  Warned and tainted bits are ignored (not part of string identity).
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.content_digest());
    }
}

impl PerlString {
    /// The 64-bit content digest (§2.3.5): unflagged strings digest their bytes; flagged strings whose characters all
    /// fit 0–255 digest the downgraded bytes (colliding with their unflagged equals, as required); flagged strings with
    /// characters above 255 or with malformed content digest their raw bytes.
    ///
    /// When the range is unresolved, deciding it first would fetch the bytes twice.  Instead, the single-fetch dual
    /// calculation (§2.2.5): per cache-resident block, BOTH candidate digests advance — raw over the bytes, downgraded
    /// over the decoded characters (until a character > 0xFF kills that candidate) — and the end of the data decides
    /// which digest is the value's.  The pass is a classification, so its knowledge is kept: the scan state narrows and
    /// the character count caches, like any other fused pass.
    fn content_digest(&self) -> u64 {
        use std::collections::hash_map::RandomState;
        use std::hash::BuildHasher;
        use std::sync::OnceLock;

        /// The per-process digest key (§2.3.5): the analogue of perl's `PL_hash_seed`.  An unkeyed digest would let
        /// attackers precompute colliding keys offline (the pre-5.8.1 HashDoS posture); collisions in this inner digest
        /// collapse hash-map buckets regardless of the outer map's own seed, so the hardening must live here.  One
        /// state per process: digests must agree within a process and need not (must not, for hardening) agree across
        /// processes.
        static DIGEST_KEY: OnceLock<RandomState> = OnceLock::new();
        fn hasher() -> impl Hasher {
            DIGEST_KEY.get_or_init(RandomState::new).build_hasher()
        }

        let mut scratch = [0u8; DECODE_MAX];
        let bytes = self.as_bytes(&mut scratch);

        // Unflagged, or flagged with known-ASCII content: the raw bytes ARE the canonical downgraded form.
        if !self.is_utf8() || self.scan_state() == scan::ASCII {
            let mut h = hasher();
            h.write(bytes);
            return h.finish();
        }

        match self.scan_state() {
            // Known Latin-1 range: single decode-emit pass over the downgraded characters.
            scan::UTF8_LATIN1 => {
                count_full_scan();
                let mut h = hasher();
                let mut facts = ScanFacts::default();
                let _ = scalar_decode_span(bytes, 0, bytes.len(), &mut facts, |v| h.write_u8(v as u8));
                h.finish()
            }

            // Known beyond Latin-1 or invalid: the raw bytes are the canonical form.
            st if scan::is_known_beyond_latin1(st) || st == scan::MALFORMED_UTF8 => {
                let mut h = hasher();
                h.write(bytes);
                h.finish()
            }

            // Unresolved: the blocked dual calculation.
            _ => {
                count_full_scan();
                let mut raw = hasher();
                let mut down = hasher();
                let mut downgradable = true;
                let mut facts = ScanFacts::default();
                let mut pos = 0usize;
                let mut malformed = false;

                while pos < bytes.len() {
                    let soft_end = block_end(pos, bytes.len());

                    // Exitless gate: a pure-ASCII block advances both candidates with the same bytes.
                    let hi = bytes[pos..soft_end].iter().fold(0u8, |a, &b| a | b) & 0x80 != 0;
                    if !hi {
                        raw.write(&bytes[pos..soft_end]);
                        if downgradable {
                            down.write(&bytes[pos..soft_end]);
                        }
                        facts.chars += soft_end - pos;
                        pos = soft_end;
                        continue;
                    }

                    // Non-ASCII block: one cached decode advances the downgraded candidate per character (until a
                    // character > 0xFF kills it) while the raw candidate takes the same byte span.
                    let stop = scalar_decode_span(bytes, pos, soft_end, &mut facts, |v| {
                        if v > 0xFF {
                            downgradable = false;
                        } else if downgradable {
                            down.write_u8(v as u8);
                        }
                    });

                    match stop {
                        Some(next) => {
                            raw.write(&bytes[pos..next]);
                            pos = next;
                        }
                        None => {
                            // Malformed: characters are undefined; the raw digest is the value's.  Finish the fetch
                            // raw-only.
                            raw.write(&bytes[pos..]);
                            malformed = true;
                            downgradable = false;
                            pos = bytes.len();
                        }
                    }
                }

                // The pass classified the content — keep the knowledge (heap only; inline is terminal at birth).
                if let RawParts::Heap(cb) = self.raw_parts() {
                    if malformed {
                        cb.narrow_scan(scan::MALFORMED_UTF8);
                    } else {
                        cb.narrow_scan(facts.state());
                        if facts.chars > 0 {
                            cb.set_char_count(facts.chars);
                        }
                    }
                }

                if downgradable { down.finish() } else { raw.finish() }
            }
        }
    }
}

impl Clone for PerlString {
    fn clone(&self) -> PerlString {
        let (u, w, t) = (self.is_utf8(), self.is_warned(), self.is_tainted());
        match self.raw_parts() {
            RawParts::Packed(p) => PerlString::build_packed(p, u, w, t),
            RawParts::Inline { buf } => {
                let scan = match self.inline_scan() {
                    Some(s) => s,
                    None => InlineScan::Malformed, // unreachable by construction; safe fallback
                };
                PerlString::build_inline(scan, u, w, t, *buf)
            }
            RawParts::Heap(cb) => PerlString::build_heap(u, w, t, cb.clone()),
        }
    }
}

/// Byte-string syntax for the content: printable ASCII as itself, everything else escaped.  `Debug` for a byte slice
/// renders integers, which makes a timestamp thirty numbers; a perl string's bytes are frequently not UTF-8, so lossy
/// text would misrepresent them instead.
struct ByteLiteral<'a>(&'a [u8]);

impl fmt::Debug for ByteLiteral<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("b\"")?;
        for &b in self.0 {
            match b {
                b'"' => f.write_str("\\\"")?,
                b'\\' => f.write_str("\\\\")?,
                b'\n' => f.write_str("\\n")?,
                b'\r' => f.write_str("\\r")?,
                b'\t' => f.write_str("\\t")?,
                0x20..=0x7E => f.write_char(b as char)?,
                _ => write!(f, "\\x{b:02X}")?,
            }
        }

        f.write_str("\"")
    }
}

impl fmt::Debug for PerlString {
    /// The representation, not the value: which tier holds the content, its length, the three per-value tag bits, and
    /// the bytes.  A developer printing one of these is nearly always asking where it landed, and this type's identity
    /// *is* its representation — how the content should render as text is a question for whatever layer knows the
    /// output encoding.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PerlString")
            .field("storage", &self.storage_kind())
            .field("len", &self.len())
            .field("utf8", &self.is_utf8())
            .field("warned", &self.is_warned())
            .field("tainted", &self.is_tainted())
            .field("bytes", &ByteLiteral(self.as_bytes(&mut [0u8; DECODE_MAX])))
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/string_tests.rs"]
mod tests;
