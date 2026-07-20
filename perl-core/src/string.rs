//! `PerlString` — a Perl string: octet sequence + per-string state (§2.2.3).
//!
//! Two storage kinds and three per-value state dimensions fold into the enum discriminant:
//!
//! - **Storage**: `Inline` (≤ 22 bytes, no heap allocation) or `Heap` (a [`CowBuffer`]).
//! - **The Perl utf8 flag** (`u`): a per-SV *semantic claim* ("interpret these bytes as characters"), not a validity
//!   fact.  It can be set on bytes Rust rejects (perl-extended UTF-8; verified `chr(0x110000)`); no code path may
//!   derive `from_utf8_unchecked` from it.  Rust-level validity comes from the scan cache only.
//! - **Warned** (`w`): the numification-warning once-bit (§2.3.4).  Monotone: set, never cleared.
//! - **Tainted** (`t`): the per-value taint bit (§2.6.1).  Cleared only through the laundering capability (§2.6.2).
//!
//! Inline strings additionally fold their **scan state** into the tag — and only the three *terminal* states
//! (`A` = pure ASCII, `N` = valid UTF-8 non-ASCII, `X` = invalid UTF-8), because inline strings are scanned eagerly
//! and completely at construction: a full validity scan of at most 22 bytes is nearly free.  Heap strings keep the
//! full five-state lazy lattice in the buffer header (§2.2.4–§2.2.6).
//!
//! Variant names are full words (no legend required): scan word first (`Ascii`, `Utf8` — Rust-valid non-ASCII —
//! `Extended`, `Malformed`), then flag words in fixed order: `Flagged` (the *Perl* utf8 flag — a different thing
//! from the scan's validity facts), `Warned`, `Tainted`.  E.g. `InlineUtf8FlaggedTainted`, `HeapWarned`.
//!
//! Equality and hashing are **character-sequence** semantics (§2.3.5): the utf8 flag changes the byte→character
//! mapping, so same-bytes/different-flags can be different strings and different-bytes can be the same string.  Warned
//! and tainted are ignored by `Eq`/`Hash`.

use crate::cow_buffer::{AllocError, CowBuffer};
use std::hash::{Hash, Hasher};

/// Maximum inline payload: chosen so every numeric stringification stays allocation-free (§2.2.3).
pub const INLINE_MAX: usize = 22;

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
    }

    thread_local! {
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
/// values cap at IV_MAX.  Rust additionally rejects surrogates, values above U+10FFFF, and any sequence longer than
/// 4 bytes — decidable per-sequence during the same decode.
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
        match scalar_decode_span(bytes, pos, soft_end, &mut facts) {
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
fn scalar_decode_span(bytes: &[u8], start: usize, soft_end: usize, facts: &mut ScanFacts) -> Option<usize> {
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

    /// Heap: a shared [`CowBuffer`].
    Heap,
}

/// Generates the folded-tag variant set and the accessors over it.  Variant names are written out explicitly (not
/// synthesized by identifier concatenation) so a grep for any variant finds this defining invocation.
macro_rules! define_perl_string {
    (
        inline: [ $( $iv:ident = ($iscan:ident, $iu:literal, $iw:literal, $it:literal) ),* $(,)? ],
        heap:   [ $( $hv:ident = ($hu:literal, $hw:literal, $ht:literal) ),* $(,)? ]
    ) => {
        /// A Perl string.  See the module documentation; the variant set is the folded tag (§2.2.3) and is an
        /// implementation detail — construct and inspect through the methods, never by matching variants directly.
        pub enum PerlString {
            $( #[doc(hidden)] $iv { len: u8, buf: [u8; INLINE_MAX] }, )*
            $( #[doc(hidden)] $hv(CowBuffer), )*
        }

        impl PerlString {
            /// The storage kind.
            pub fn storage_kind(&self) -> StorageKind {
                match self {
                    $( PerlString::$iv { .. } => StorageKind::Inline, )*
                    $( PerlString::$hv(_) => StorageKind::Heap, )*
                }
            }

            /// The Perl utf8 flag (semantic claim, not validity — see module docs).
            pub fn is_utf8(&self) -> bool {
                match self {
                    $( PerlString::$iv { .. } => $iu, )*
                    $( PerlString::$hv(_) => $hu, )*
                }
            }

            /// Whether the numification warning has fired for this value (§2.3.4).
            pub fn is_warned(&self) -> bool {
                match self {
                    $( PerlString::$iv { .. } => $iw, )*
                    $( PerlString::$hv(_) => $hw, )*
                }
            }

            /// Whether this value is tainted (§2.6).
            pub fn is_tainted(&self) -> bool {
                match self {
                    $( PerlString::$iv { .. } => $it, )*
                    $( PerlString::$hv(_) => $ht, )*
                }
            }

            /// Inline terminal scan state, or `None` for heap storage.
            pub fn inline_scan(&self) -> Option<InlineScan> {
                match self {
                    $( PerlString::$iv { .. } => Some(InlineScan::$iscan), )*
                    $( PerlString::$hv(_) => None, )*
                }
            }

            /// Rebuild an inline value with the given tag dimensions (payload preserved).  Internal: tag transitions go
            /// through the public monotone/setter methods.
            fn build_inline(scan: InlineScan, utf8: bool, warned: bool, tainted: bool, len: u8, buf: [u8; INLINE_MAX]) -> PerlString {
                match (scan, utf8, warned, tainted) {
                    $( (InlineScan::$iscan, $iu, $iw, $it) => PerlString::$iv { len, buf }, )*
                }
            }

            /// Rebuild a heap value with the given tag dimensions (buffer preserved).
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
const _: () = assert!(size_of::<PerlString>() == 24);
const _: () = assert!(size_of::<Option<PerlString>>() == 24);

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

fn inline_payload(bytes: &[u8]) -> (u8, [u8; INLINE_MAX]) {
    debug_assert!(bytes.len() <= INLINE_MAX);
    let mut buf = [0u8; INLINE_MAX];
    buf[..bytes.len()].copy_from_slice(bytes);
    (bytes.len() as u8, buf)
}

impl PerlString {
    /// Construct from a Rust `&str`.  ASCII content is stored unflagged (the canonical downgraded form, §2.3.5);
    /// non-ASCII content is stored with the utf8 flag (validity known from the type).  Public surface: the `FromStr`
    /// impl below.
    fn from_str_impl(s: &str) -> Result<PerlString, AllocError> {
        let bytes = s.as_bytes();
        if bytes.len() <= INLINE_MAX {
            let state = eager_scan(bytes); // Ascii or Utf8NonAscii; Malformed/Extended impossible from &str
            let (len, buf) = inline_payload(bytes);
            let utf8 = state != InlineScan::Ascii;
            Ok(PerlString::build_inline(state, utf8, false, false, len, buf))
        } else {
            let cb = CowBuffer::from_slice(bytes)?;
            let ascii = bytes.iter().all(|b| b.is_ascii());
            cb.narrow_scan(if ascii { scan::ASCII } else { scan::UTF8_UNKNOWN_RANGE });
            Ok(PerlString::build_heap(!ascii, false, false, cb))
        }
    }

    /// Construct from raw bytes (I/O, `Encode`, lexer literals).  Unflagged; inline content gets its eager terminal
    /// scan, heap content defers all scanning (`UNKNOWN`), per §2.2.7.
    pub fn from_bytes(bytes: &[u8]) -> Result<PerlString, AllocError> {
        if bytes.len() <= INLINE_MAX {
            let (len, buf) = inline_payload(bytes);
            Ok(PerlString::build_inline(eager_scan(bytes), false, false, false, len, buf))
        } else {
            let cb = CowBuffer::from_slice(bytes)?; // scan byte born UNKNOWN
            Ok(PerlString::build_heap(false, false, false, cb))
        }
    }

    /// The empty string (inline, unflagged, trivially ASCII).
    pub fn empty() -> PerlString {
        PerlString::build_inline(InlineScan::Ascii, false, false, false, 0, [0u8; INLINE_MAX])
    }

    // ── Accessors ─────────────────────────────────────────────────
    /// Length in bytes.  No dereference for inline; handle mirror for heap.
    pub fn len(&self) -> usize {
        match self.raw_parts() {
            RawParts::Inline { len, .. } => len as usize,
            RawParts::Heap(cb) => cb.len(),
        }
    }

    /// Whether the string is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        match self.raw_parts() {
            RawParts::Inline { len, buf } => &buf[..len as usize],
            RawParts::Heap(cb) => cb.as_slice(),
        }
    }

    /// View as a Rust `&str` if the bytes are valid UTF-8 (a fact question, independent of the Perl flag).  Narrows the
    /// heap scan lattice as a side effect (§2.2.5); sound through `&self`.
    pub fn as_str(&self) -> Option<&str> {
        match self.raw_parts() {
            RawParts::Inline { len, buf } => {
                let bytes = &buf[..len as usize];
                match self.inline_scan() {
                    // SAFETY: terminal scan states were established by a full validity scan at construction and inline
                    // mutation re-scans; Ascii, Latin1, and NonLatin1 all certify Rust-valid UTF-8.
                    Some(InlineScan::Ascii) | Some(InlineScan::Latin1) | Some(InlineScan::NonLatin1) => Some(unsafe { std::str::from_utf8_unchecked(bytes) }),
                    _ => None,
                }
            }
            RawParts::Heap(cb) => {
                let bytes = cb.as_slice();
                match cb.scan() {
                    // SAFETY: these lattice states certify prior successful validation of these exact bytes (states
                    // only narrow; mutation resets to UNKNOWN).
                    st if scan::is_rust_valid(st) => Some(unsafe { std::str::from_utf8_unchecked(bytes) }),
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
                            Some(unsafe { std::str::from_utf8_unchecked(bytes) })
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
            RawParts::Heap(cb) => cb.scan(),
        }
    }

    /// Whether the bytes are valid under perl's *extended* UTF-8 rules (§2.2.4) — the predicate character-level
    /// operations on flagged strings use.  Narrows the heap lattice.
    pub fn is_perl_utf8_valid(&self) -> bool {
        match self.raw_parts() {
            RawParts::Inline { .. } => !matches!(self.inline_scan(), Some(InlineScan::Malformed)),
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

    /// Resolve range knowledge to a terminal where a full pass is warranted (§2.2.5): the consumers are downgrade
    /// hashing and the equality fast path.  No-op for inline (terminal at birth) and for heap states that are already
    /// terminal or not Rust-valid.
    fn resolve_range(&self) {
        if let RawParts::Heap(cb) = self.raw_parts() {
            match cb.scan() {
                scan::UTF8_UNKNOWN_RANGE | scan::UTF8_NON_ASCII => {
                    let (st, chars) = classify_known_valid(cb.as_slice());
                    cb.narrow_scan(st);
                    if chars > 0 {
                        cb.set_char_count(chars);
                    }
                }
                scan::UNKNOWN | scan::NON_ASCII => {
                    let _ = self.is_perl_utf8_valid(); // full validation classifies fully
                }
                _ => {}
            }
        }
    }

    /// Whether the value is *known* (from existing scan knowledge; no scan is performed) to contain a character
    /// ≥ U+0100 — the equality fast-negative (§2.2.4): such a string can equal no unflagged string.
    fn known_beyond_latin1(&self) -> bool {
        match self.raw_parts() {
            RawParts::Inline { .. } => {
                matches!(self.inline_scan(), Some(InlineScan::NonLatin1) | Some(InlineScan::Extended))
            }
            RawParts::Heap(cb) => scan::is_known_beyond_latin1(cb.scan()),
        }
    }

    /// Character length under perl's flagged semantics (§2.2.4): the character count of the decoded content.  `None`
    /// iff the content is malformed under perl's extended rules (the ops layer owns perl's malformed-length warning
    /// behavior).  For unflagged strings perl's `length()` is byte length — callers pick the primitive by flag; this
    /// one is the flagged-side answer.  O(1) after first classification; cached per-buffer, shared across COW sharers.
    pub fn char_len(&self) -> Option<usize> {
        match self.raw_parts() {
            RawParts::Inline { len, buf } => {
                let bytes = &buf[..len as usize];
                match self.inline_scan() {
                    Some(InlineScan::Ascii) => Some(len as usize),
                    Some(InlineScan::Malformed) | None => None,
                    _ => {
                        let (_, chars) = classify_full(bytes); // ≤ 22 bytes: recount is trivial
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
    #[cfg_attr(not(test), expect(dead_code, reason = "consumers are the §21.1 capture and hash-key steps; API is design-mandated"))]
    pub(crate) fn untaint_for_sanctioned_path(&mut self) {
        self.rebuild_tag(|u, w, _t| (u, w, false));
    }

    fn rebuild_tag(&mut self, f: impl FnOnce(bool, bool, bool) -> (bool, bool, bool)) {
        let (u, w, t) = (self.is_utf8(), self.is_warned(), self.is_tainted());
        let (u2, w2, t2) = f(u, w, t);

        if (u, w, t) == (u2, w2, t2) {
            return;
        }

        let old = std::mem::replace(self, PerlString::empty());

        *self = match old.into_raw() {
            RawOwned::Inline { scan, len, buf } => PerlString::build_inline(scan, u2, w2, t2, len, buf),
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
        let old = std::mem::replace(self, PerlString::empty());

        *self = match old.into_raw() {
            RawOwned::Inline { scan, len, buf } => {
                let old_bytes = &buf[..len as usize];
                let new_len = len as usize + bytes.len();
                if new_len <= INLINE_MAX {
                    let mut nbuf = buf;
                    nbuf[len as usize..new_len].copy_from_slice(bytes);
                    let nscan = append_transition_inline(scan, kind, &nbuf[..new_len]);
                    PerlString::build_inline(nscan, u, w, t, new_len as u8, nbuf)
                } else {
                    // Promote to heap (one-way).  Fold the append into the promoting allocation.
                    let mut cb = CowBuffer::with_capacity(new_len + (new_len >> 2))?;
                    cb.extend_from_slice(old_bytes)?;
                    cb.extend_from_slice(bytes)?;
                    cb.narrow_scan(append_transition_heap(inline_scan_to_heap(scan), kind));
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

    // ── Internal raw views ────────────────────────────────────────
    fn raw_parts(&self) -> RawParts<'_> {
        // The macro cannot express payload access generically across two shapes without another dimension of rules;
        // this single exhaustive match is the one hand-written traversal, kept private.
        macro_rules! arms {
            () => {};
        }

        arms!();

        #[allow(clippy::enum_glob_use)]
        {
            use PerlString::*;
            match self {
                InlineAscii { len, buf }
                | InlineAsciiFlagged { len, buf }
                | InlineAsciiWarned { len, buf }
                | InlineAsciiFlaggedWarned { len, buf }
                | InlineAsciiTainted { len, buf }
                | InlineAsciiFlaggedTainted { len, buf }
                | InlineAsciiWarnedTainted { len, buf }
                | InlineAsciiFlaggedWarnedTainted { len, buf }
                | InlineLatin1 { len, buf }
                | InlineLatin1Flagged { len, buf }
                | InlineLatin1Warned { len, buf }
                | InlineLatin1FlaggedWarned { len, buf }
                | InlineLatin1Tainted { len, buf }
                | InlineLatin1FlaggedTainted { len, buf }
                | InlineLatin1WarnedTainted { len, buf }
                | InlineLatin1FlaggedWarnedTainted { len, buf }
                | InlineNonLatin1 { len, buf }
                | InlineNonLatin1Flagged { len, buf }
                | InlineNonLatin1Warned { len, buf }
                | InlineNonLatin1FlaggedWarned { len, buf }
                | InlineNonLatin1Tainted { len, buf }
                | InlineNonLatin1FlaggedTainted { len, buf }
                | InlineNonLatin1WarnedTainted { len, buf }
                | InlineNonLatin1FlaggedWarnedTainted { len, buf }
                | InlineExtended { len, buf }
                | InlineExtendedFlagged { len, buf }
                | InlineExtendedWarned { len, buf }
                | InlineExtendedFlaggedWarned { len, buf }
                | InlineExtendedTainted { len, buf }
                | InlineExtendedFlaggedTainted { len, buf }
                | InlineExtendedWarnedTainted { len, buf }
                | InlineExtendedFlaggedWarnedTainted { len, buf }
                | InlineMalformed { len, buf }
                | InlineMalformedFlagged { len, buf }
                | InlineMalformedWarned { len, buf }
                | InlineMalformedFlaggedWarned { len, buf }
                | InlineMalformedTainted { len, buf }
                | InlineMalformedFlaggedTainted { len, buf }
                | InlineMalformedWarnedTainted { len, buf }
                | InlineMalformedFlaggedWarnedTainted { len, buf } => RawParts::Inline { len: *len, buf },

                Heap(cb)
                | HeapFlagged(cb)
                | HeapWarned(cb)
                | HeapFlaggedWarned(cb)
                | HeapTainted(cb)
                | HeapFlaggedTainted(cb)
                | HeapWarnedTainted(cb)
                | HeapFlaggedWarnedTainted(cb) => RawParts::Heap(cb),
            }
        }
    }

    fn into_raw(self) -> RawOwned {
        let scan = self.inline_scan();

        #[allow(clippy::enum_glob_use)]
        {
            use PerlString::*;
            match self {
                InlineAscii { len, buf }
                | InlineAsciiFlagged { len, buf }
                | InlineAsciiWarned { len, buf }
                | InlineAsciiFlaggedWarned { len, buf }
                | InlineAsciiTainted { len, buf }
                | InlineAsciiFlaggedTainted { len, buf }
                | InlineAsciiWarnedTainted { len, buf }
                | InlineAsciiFlaggedWarnedTainted { len, buf }
                | InlineLatin1 { len, buf }
                | InlineLatin1Flagged { len, buf }
                | InlineLatin1Warned { len, buf }
                | InlineLatin1FlaggedWarned { len, buf }
                | InlineLatin1Tainted { len, buf }
                | InlineLatin1FlaggedTainted { len, buf }
                | InlineLatin1WarnedTainted { len, buf }
                | InlineLatin1FlaggedWarnedTainted { len, buf }
                | InlineNonLatin1 { len, buf }
                | InlineNonLatin1Flagged { len, buf }
                | InlineNonLatin1Warned { len, buf }
                | InlineNonLatin1FlaggedWarned { len, buf }
                | InlineNonLatin1Tainted { len, buf }
                | InlineNonLatin1FlaggedTainted { len, buf }
                | InlineNonLatin1WarnedTainted { len, buf }
                | InlineNonLatin1FlaggedWarnedTainted { len, buf }
                | InlineExtended { len, buf }
                | InlineExtendedFlagged { len, buf }
                | InlineExtendedWarned { len, buf }
                | InlineExtendedFlaggedWarned { len, buf }
                | InlineExtendedTainted { len, buf }
                | InlineExtendedFlaggedTainted { len, buf }
                | InlineExtendedWarnedTainted { len, buf }
                | InlineExtendedFlaggedWarnedTainted { len, buf }
                | InlineMalformed { len, buf }
                | InlineMalformedFlagged { len, buf }
                | InlineMalformedWarned { len, buf }
                | InlineMalformedFlaggedWarned { len, buf }
                | InlineMalformedTainted { len, buf }
                | InlineMalformedFlaggedTainted { len, buf }
                | InlineMalformedWarnedTainted { len, buf }
                | InlineMalformedFlaggedWarnedTainted { len, buf } => {
                    // inline_scan() is Some for every inline variant by construction of the macro table.
                    let scan = scan.unwrap_or(InlineScan::Malformed);
                    RawOwned::Inline { scan, len, buf }
                }

                Heap(cb)
                | HeapFlagged(cb)
                | HeapWarned(cb)
                | HeapFlaggedWarned(cb)
                | HeapTainted(cb)
                | HeapFlaggedTainted(cb)
                | HeapWarnedTainted(cb)
                | HeapFlaggedWarnedTainted(cb) => RawOwned::Heap(cb),
            }
        }
    }
}

enum RawParts<'a> {
    Inline { len: u8, buf: &'a [u8; INLINE_MAX] },
    Heap(&'a CowBuffer),
}

enum RawOwned {
    Inline { scan: InlineScan, len: u8, buf: [u8; INLINE_MAX] },
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

            match std::str::from_utf8(&self.rest[..self.rest.len().min(4)]) {
                Ok(s) => {
                    let c = s.chars().next()?;
                    self.rest = &self.rest[c.len_utf8()..];
                    Some(c as u32)
                }
                Err(e) if e.valid_up_to() > 0 => {
                    // SAFETY: valid_up_to bytes are certified valid UTF-8.
                    let s = unsafe { std::str::from_utf8_unchecked(&self.rest[..e.valid_up_to()]) };
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

impl std::str::FromStr for PerlString {
    type Err = AllocError;

    /// Construct from a Rust `&str` (the same rules as documented on the private constructor: ASCII stores in the
    /// canonical downgraded unflagged form; non-ASCII stores flagged).  `"..." .parse::<PerlString>()` therefore works,
    /// with allocation failure as the error.
    fn from_str(s: &str) -> Result<PerlString, AllocError> {
        PerlString::from_str_impl(s)
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
            return self.as_bytes() == other.as_bytes();
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

        // Undecided: the single streaming dual-direction compare, short-circuiting at first mismatch, narrowing
        // opportunistically on a completed (equal) walk.
        #[cfg(test)]
        eq_probe::WALK_ENTRIES.with(|c| c.set(c.get() + 1));

        let fb = flagged.as_bytes();
        let pb = plain.as_bytes();
        let mut saw_non_ascii = false;
        let mut plain_iter = pb.iter();

        for c in flagged_chars(fb) {
            #[cfg(test)]
            eq_probe::WALK_CHARS.with(|w| w.set(w.get() + 1));

            let Some(&b) = plain_iter.next() else { return false };

            if c != b as u32 {
                return false;
            }

            saw_non_ascii |= b >= 0x80;
        }

        if plain_iter.next().is_some() {
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
    /// Canonical downgraded-when-possible form (§2.3.5): unflagged strings hash their bytes; flagged strings whose
    /// characters all fit 0–255 hash the downgraded bytes (colliding with their unflagged equals, as required); flagged
    /// strings with characters above 255 (which can equal only byte-identical flagged strings) hash their raw bytes.
    /// Warned and tainted bits are ignored throughout (they are not part of string identity).
    fn hash<H: Hasher>(&self, state: &mut H) {
        let bytes = self.as_bytes();

        if !self.is_utf8() || self.is_ascii() {
            state.write(bytes);
            state.write_u8(0xFF); // length delimiter, Hash-contract hygiene
            return;
        }

        // Flagged, non-ASCII: downgradability is range knowledge (§2.2.4) — resolve it via the scan cache rather than a
        // trial decode.
        self.resolve_range();
        if self.known_beyond_latin1() || !self.is_perl_utf8_valid() {
            // Contains a character ≥ U+0100 (can equal only byte-identical flagged strings), or malformed (characters
            // undefined): hash the raw bytes.
            state.write(bytes);
        } else {
            // Known Latin-1 range: emit the canonical downgraded bytes, single pass.
            for c in flagged_chars(bytes) {
                state.write_u8(c as u8);
            }
        }

        state.write_u8(0xFF);
    }
}

impl Clone for PerlString {
    fn clone(&self) -> PerlString {
        let (u, w, t) = (self.is_utf8(), self.is_warned(), self.is_tainted());
        match self.raw_parts() {
            RawParts::Inline { len, buf } => {
                let scan = match self.inline_scan() {
                    Some(s) => s,
                    None => InlineScan::Malformed, // unreachable by construction; safe fallback
                };
                PerlString::build_inline(scan, u, w, t, len, *buf)
            }
            RawParts::Heap(cb) => PerlString::build_heap(u, w, t, cb.clone()),
        }
    }
}

impl std::fmt::Debug for PerlString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PerlString")
            .field("storage", &self.storage_kind())
            .field("len", &self.len())
            .field("utf8", &self.is_utf8())
            .field("warned", &self.is_warned())
            .field("tainted", &self.is_tainted())
            .field("bytes", &self.as_bytes())
            .finish()
    }
}

// ── Tests ─────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn hash_of(s: &PerlString) -> u64 {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut h = DefaultHasher::new();
        s.hash(&mut h);
        h.finish()
    }

    // ── Construction and boundaries ───────────────────────────────
    #[test]
    fn boundary_22_inline_23_heap() {
        let s22 = PerlString::from_str_impl(&"a".repeat(22)).unwrap();
        assert_eq!(s22.storage_kind(), StorageKind::Inline);
        let s23 = PerlString::from_str_impl(&"a".repeat(23)).unwrap();
        assert_eq!(s23.storage_kind(), StorageKind::Heap);
        assert_eq!(s23.len(), 23);
    }

    #[test]
    fn ascii_from_str_is_unflagged_canonical() {
        let s = PerlString::from_str_impl("hello").unwrap();
        assert!(!s.is_utf8(), "ASCII stores in canonical downgraded form");
        assert_eq!(s.inline_scan(), Some(InlineScan::Ascii));
        assert_eq!(s.as_str(), Some("hello"));
    }

    #[test]
    fn non_ascii_from_str_is_flagged() {
        let s = PerlString::from_str_impl("héllo").unwrap();
        assert!(s.is_utf8());
        assert_eq!(s.inline_scan(), Some(InlineScan::Latin1)); // é is U+00E9: Latin-1 range
        assert_eq!(s.as_str(), Some("héllo"));
    }

    #[test]
    fn invalid_bytes_inline_scan_terminal() {
        let s = PerlString::from_bytes(&[0xFF, 0xFE]).unwrap();
        assert_eq!(s.inline_scan(), Some(InlineScan::Malformed));
        assert_eq!(s.as_str(), None);
        assert!(!s.is_ascii());
        assert_eq!(s.as_bytes(), &[0xFF, 0xFE]);
    }

    #[test]
    fn heap_from_bytes_defers_scanning() {
        let bytes = vec![b'x'; 40];
        let s = PerlString::from_bytes(&bytes).unwrap();
        assert_eq!(s.storage_kind(), StorageKind::Heap);
        // as_str triggers the lazy scan and narrows.
        assert_eq!(s.as_str(), Some("x".repeat(40).as_str()));
        assert!(s.is_ascii());
    }

    // ── Character-sequence equality (container-verified cases) ────
    #[test]
    fn eq_same_flags_is_byte_equality() {
        let a = PerlString::from_str_impl("hello").unwrap();
        let b = PerlString::from_bytes(b"hello").unwrap();
        assert_eq!(a, b); // both unflagged ASCII
    }

    #[test]
    fn eq_cross_flag_same_bytes_can_differ() {
        // Verified perl 5.38: unflagged C3 A9 is the two characters "\xc3\xa9"; flagged it is "é" — not eq.
        let mut flagged = PerlString::from_bytes(&[0xC3, 0xA9]).unwrap();
        flagged.set_utf8_for_test();
        let unflagged = PerlString::from_bytes(&[0xC3, 0xA9]).unwrap();
        assert_ne!(flagged, unflagged);
    }

    #[test]
    fn eq_cross_flag_different_bytes_can_match() {
        // Verified perl 5.38: unflagged E9 (latin-1 é) eq flagged C3 A9 (UTF-8 é).
        let mut flagged = PerlString::from_bytes(&[0xC3, 0xA9]).unwrap();
        flagged.set_utf8_for_test();
        let latin1 = PerlString::from_bytes(&[0xE9]).unwrap();
        assert_eq!(flagged, latin1);
        assert_eq!(latin1, flagged);
    }

    #[test]
    fn eq_ignores_warned_and_tainted() {
        let a = PerlString::from_str_impl("same").unwrap();
        let mut b = PerlString::from_str_impl("same").unwrap();
        b.mark_warned();
        b.taint();
        assert_eq!(a, b);
        assert_eq!(hash_of(&a), hash_of(&b));
    }

    // ── Canonical hashing (container-verified hash-key semantics) ─
    #[test]
    fn hash_key_flag_insensitive() {
        // Verified perl 5.38: utf8::upgrade/downgrade variants of a key are ONE key.
        let mut flagged = PerlString::from_bytes(&[0xC3, 0xA9]).unwrap();
        flagged.set_utf8_for_test();
        let latin1 = PerlString::from_bytes(&[0xE9]).unwrap();
        assert_eq!(hash_of(&flagged), hash_of(&latin1), "equal strings must hash equal");
        let mut h: HashMap<PerlString, i32> = HashMap::new();
        h.insert(flagged, 1);
        h.insert(latin1, 2);
        assert_eq!(h.len(), 1, "Perl hash keys are flag-insensitive");
    }

    // ── Tag transitions ───────────────────────────────────────────
    #[test]
    fn warned_is_monotone_and_payload_preserving() {
        let mut s = PerlString::from_str_impl("12abc").unwrap();
        assert!(!s.is_warned());
        s.mark_warned();
        assert!(s.is_warned());
        assert_eq!(s.as_bytes(), b"12abc");
        assert_eq!(s.inline_scan(), Some(InlineScan::Ascii));
        s.mark_warned(); // idempotent
        assert!(s.is_warned());
    }

    #[test]
    fn taint_round_trip_via_sanctioned_path() {
        let mut s = PerlString::from_str_impl("data").unwrap();
        s.taint();
        assert!(s.is_tainted());
        s.untaint_for_sanctioned_path();
        assert!(!s.is_tainted());
    }

    #[test]
    fn warned_copies_with_the_value() {
        // Verified perl 5.38 (§2.3.4): the warn state is copied on assignment.
        let mut s = PerlString::from_str_impl("abc").unwrap();
        s.mark_warned();
        let copy = s.clone();
        assert!(copy.is_warned());
    }

    // ── Append transitions (§2.2.5) ───────────────────────────────
    #[test]
    fn ascii_append_preserves_state() {
        let mut s = PerlString::from_str_impl("abc").unwrap();
        s.push_str("def").unwrap();
        assert_eq!(s.inline_scan(), Some(InlineScan::Ascii));
        assert_eq!(s.as_bytes(), b"abcdef");
    }

    #[test]
    fn valid_utf8_append_to_ascii_goes_non_ascii() {
        let mut s = PerlString::from_str_impl("abc").unwrap();
        s.push_str("é").unwrap();
        assert_eq!(s.inline_scan(), Some(InlineScan::Latin1)); // ASCII + é joins to Latin-1 range
        assert_eq!(s.as_str(), Some("abcé"));
    }

    #[test]
    fn inline_overflow_promotes_to_heap_one_way() {
        let mut s = PerlString::from_str_impl(&"a".repeat(20)).unwrap();
        s.push_str("bcdef").unwrap(); // 25 bytes
        assert_eq!(s.storage_kind(), StorageKind::Heap);
        assert_eq!(s.len(), 25);
        assert!(s.is_ascii(), "promotion carried the scan knowledge");
        // Shrinking (future truncate) must not demote — pinned when truncate lands.
    }

    #[test]
    fn heap_append_transitions() {
        let mut s = PerlString::from_str_impl(&"a".repeat(30)).unwrap(); // Heap, ASCII known
        s.push_str("é").unwrap();
        assert_eq!(s.as_str().map(|v| v.len()), Some(32));
        // ASCII + valid-non-ascii → UTF8_NON_ASCII, without rescanning.
        assert!(!s.is_ascii());
        let mut raw = PerlString::from_bytes(&[0x80u8; 30]).unwrap(); // Heap, UNKNOWN
        raw.push_bytes(&[0x81]).unwrap();
        assert_eq!(raw.as_str(), None); // lazy scan resolves to invalid
    }

    #[test]
    fn flag_and_bits_survive_promotion() {
        let mut s = PerlString::from_str_impl(&"é".repeat(11)).unwrap(); // 22 bytes inline, flagged
        s.taint();
        s.push_str("x").unwrap(); // promotes
        assert_eq!(s.storage_kind(), StorageKind::Heap);
        assert!(s.is_utf8());
        assert!(s.is_tainted());
        assert_eq!(s.as_str(), Some(format!("{}x", "é".repeat(11)).as_str()));
    }

    // ── Extended-UTF-8 taxonomy (container-verified, §2.2.4) ──────
    #[test]
    fn extended_taxonomy_inline() {
        // Perl-decodable, Rust-invalid: surrogate, supra-Unicode, minimal FE form.
        for bytes in [&[0xED, 0xA0, 0x80][..], &[0xF4, 0x90, 0x80, 0x80], &[0xFE, 0x82, 0x80, 0x80, 0x80, 0x80, 0x80]] {
            let s = PerlString::from_bytes(bytes).unwrap();
            assert_eq!(s.inline_scan(), Some(InlineScan::Extended), "{bytes:02X?}");
            assert_eq!(s.as_str(), None, "Rust view must reject extended");
            assert!(s.is_perl_utf8_valid(), "perl view must accept extended");
            assert!(!s.is_ascii());
        }

        // Malformed for perl too: overlong, bare continuation, truncated, overlong FF form.
        let overlong_ff: Vec<u8> = std::iter::once(0xFFu8).chain(std::iter::repeat_n(0x80u8, 12)).collect();
        for bytes in [&[0xC0, 0x80][..], &[0x80], &[0xC3], &overlong_ff] {
            let s = PerlString::from_bytes(bytes).unwrap();
            assert_eq!(s.inline_scan(), Some(InlineScan::Malformed), "{bytes:02X?}");
            assert!(!s.is_perl_utf8_valid());
        }
    }

    #[test]
    fn extended_taxonomy_heap_lazy() {
        // Heap string ending in an extended sequence: lazy classification narrows to EXTENDED_UTF8.
        let mut bytes = vec![b'a'; 30];
        bytes.extend_from_slice(&[0xF4, 0x90, 0x80, 0x80]);
        let s = PerlString::from_bytes(&bytes).unwrap();
        assert_eq!(s.as_str(), None);
        assert!(s.is_perl_utf8_valid());

        // And a malformed heap string classifies INVALID.
        let mut bad = vec![b'a'; 30];
        bad.push(0xC0);
        bad.push(0x80);
        let t = PerlString::from_bytes(&bad).unwrap();
        assert!(!t.is_perl_utf8_valid());
        assert_eq!(t.as_str(), None);
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
        assert_eq!(s.inline_scan(), Some(InlineScan::Extended), "minimal FF form is perl-valid");

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
        assert_eq!(t.inline_scan(), Some(InlineScan::Malformed), "FF encoding a FE-range value is overlong");
    }

    #[test]
    fn extended_append_transitions() {
        let mut s = PerlString::from_bytes(&[0xF4, 0x90, 0x80, 0x80]).unwrap();
        s.push_str("abc").unwrap();
        assert_eq!(s.inline_scan(), Some(InlineScan::Extended), "valid append preserves extended");
        assert!(s.is_perl_utf8_valid());
    }

    #[test]
    fn extended_eq_and_hash_behavior() {
        // A flagged extended string equals no unflagged string (chars above 0xFF) and byte-identical flagged self.
        let mut a = PerlString::from_bytes(&[0xF4, 0x90, 0x80, 0x80]).unwrap();
        a.set_utf8_for_test();
        let mut b = PerlString::from_bytes(&[0xF4, 0x90, 0x80, 0x80]).unwrap();
        b.set_utf8_for_test();
        assert_eq!(a, b);
        assert_eq!(hash_of(&a), hash_of(&b));
        let plain = PerlString::from_bytes(&[0xF4, 0x90, 0x80, 0x80]).unwrap();
        assert_ne!(a, plain, "flag changes the character sequence");
    }

    // ── Range-tuned lattice (§2.2.4) ──────────────────────────────
    #[test]
    fn latin1_vs_non_latin1_terminals() {
        let e = PerlString::from_str_impl("é").unwrap(); // U+00E9
        assert_eq!(e.inline_scan(), Some(InlineScan::Latin1));
        let cjk = PerlString::from_str_impl("字").unwrap(); // U+5B57
        assert_eq!(cjk.inline_scan(), Some(InlineScan::NonLatin1));
        let mixed = PerlString::from_str_impl("é字").unwrap();
        assert_eq!(mixed.inline_scan(), Some(InlineScan::NonLatin1), "range joins upward");
    }

    #[test]
    fn unknown_range_classifies_on_ascii_probe() {
        let s = PerlString::from_str_impl(&"é".repeat(20)).unwrap(); // 40 bytes: heap, UTF8_UNKNOWN_RANGE
        assert_eq!(s.storage_kind(), StorageKind::Heap);
        assert!(!s.is_ascii(), "probe performs the range classification, not just an ASCII scan");

        // The classification left terminal Latin-1 knowledge behind: cross-flag equality against the downgraded form
        // succeeds (and would fast-negative if the state had wrongly become NON_LATIN1).
        let plain = PerlString::from_bytes(&[0xE9u8; 20]).unwrap();
        assert_eq!(s, plain);
    }

    #[test]
    fn eq_grid_same_flag_length_mismatch() {
        // Same flags + different byte lengths ⇒ ne, at both flag settings.
        let a = PerlString::from_bytes(b"abc").unwrap();
        let b = PerlString::from_bytes(b"abcd").unwrap();
        assert_ne!(a, b);
        let mut fa = PerlString::from_bytes(&[0xC3, 0xA9]).unwrap();
        fa.set_utf8_for_test();
        let mut fb = PerlString::from_bytes(&[0xC3, 0xA9, 0x41]).unwrap();
        fb.set_utf8_for_test();
        assert_ne!(fa, fb);
    }

    #[test]
    fn eq_cross_flag_flagged_longer_positive_and_negative() {
        // Flagged longer CAN match (char count < byte count): é as C3 A9 vs E9 — the positive case.
        let mut f = PerlString::from_bytes(&[0xC3, 0xA9]).unwrap();
        f.set_utf8_for_test();
        assert_eq!(f, PerlString::from_bytes(&[0xE9]).unwrap());

        // Flagged longer, mismatch mid-walk.
        assert_ne!(f, PerlString::from_bytes(&[0xEA]).unwrap());

        // Flagged longer, plain exhausted with flagged characters remaining: "é" + "a" vs just é.
        let mut f2 = PerlString::from_bytes(&[0xC3, 0xA9, b'a']).unwrap();
        f2.set_utf8_for_test();
        assert_ne!(f2, PerlString::from_bytes(&[0xE9]).unwrap());

        // And the fully-matching longer-flagged multi-char case.
        assert_eq!(f2, PerlString::from_bytes(&[0xE9, b'a']).unwrap());
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
        let mut latin1 = PerlString::from_bytes(&[0xC3, 0xA9]).unwrap();
        latin1.set_utf8_for_test();
        let mut mal = PerlString::from_bytes(&[0xC0, 0x80]).unwrap();
        mal.set_utf8_for_test();
        assert_ne!(latin1, mal);
    }

    #[test]
    fn eq_grid_valid_vs_invalid_same_flag() {
        // Flagged terminal Rust-invalid vs flagged known-Rust-valid nonterminal (heap UTF8_UNKNOWN_RANGE): valid bytes
        // never equal invalid bytes.
        let flagged_valid = PerlString::from_str_impl(&"é".repeat(20)).unwrap(); // heap, flagged, UNKNOWN_RANGE
        let mut ext = PerlString::from_bytes(&[0xF4, 0x90, 0x80, 0x80]).unwrap();
        ext.set_utf8_for_test();
        assert_ne!(flagged_valid, ext);
        assert_ne!(ext, flagged_valid);
    }

    #[test]
    fn eq_grid_ascii_vs_non_ascii_both_orientations() {
        // Flagged-ASCII vs unflagged known-non-ASCII.
        let mut fa = PerlString::from_bytes(b"abc").unwrap();
        fa.set_utf8_for_test();
        assert_ne!(fa, PerlString::from_bytes(&[0x80, 0x81, 0x82]).unwrap());

        // Unflagged-ASCII vs flagged known-non-ASCII (Latin-1).
        let mut fl = PerlString::from_bytes(&[0xC3, 0xA9]).unwrap();
        fl.set_utf8_for_test();
        assert_ne!(PerlString::from_bytes(b"ab").unwrap(), fl);
    }

    #[test]
    fn eq_grid_same_flag_terminal_mismatch() {
        // Differing terminals, both unflagged: decided without memcmp (exclusivity law).
        let latin1 = PerlString::from_bytes(&[0xC3, 0xA9]).unwrap(); // valid, Latin-1-range... as bytes: classified
        let malformed = PerlString::from_bytes(&[0xC0, 0x80]).unwrap();
        assert_ne!(latin1, malformed);
        let ascii = PerlString::from_bytes(b"ab").unwrap();
        assert_ne!(ascii, latin1);
    }

    #[test]
    fn eq_grid_flagged_malformed_vs_unflagged_is_false() {
        let mut mal = PerlString::from_bytes(&[0x80]).unwrap();
        mal.set_utf8_for_test(); // flagged malformed
        let plain = PerlString::from_bytes(&[0x80]).unwrap();
        assert_ne!(mal, plain, "upgrade of unflagged is valid; never matches malformed bytes");
    }

    #[test]
    fn eq_reverse_malformed_orientation_can_match() {
        // Unflagged MALFORMED-classified bytes are just bytes: \x80 as a character equals flagged C2 80.
        let plain = PerlString::from_bytes(&[0x80]).unwrap();
        assert_eq!(plain.inline_scan(), Some(InlineScan::Malformed));
        let mut flagged = PerlString::from_bytes(&[0xC2, 0x80]).unwrap();
        flagged.set_utf8_for_test();
        assert_eq!(flagged, plain, "the grid must not shortcut this orientation");
    }

    #[test]
    fn eq_grid_length_rows() {
        // plain longer than flagged: impossible.
        let mut flagged = PerlString::from_bytes(&[0xC3, 0xA9]).unwrap();
        flagged.set_utf8_for_test();
        let plain3 = PerlString::from_bytes(&[0xE9, 0xE9, 0xE9]).unwrap();
        assert_ne!(flagged, plain3);

        // flagged known Latin-1 (has a 2-byte char) with equal byte lengths: impossible.
        let plain2 = PerlString::from_bytes(&[0xC3, 0xA9]).unwrap();
        assert_ne!(flagged, plain2);
    }

    #[test]
    fn streaming_compare_narrows_on_completed_walk() {
        // Heap flagged UTF8_UNKNOWN_RANGE vs matching latin1 bytes: undecided by the grid, resolved by the single walk,
        // which narrows both sides.
        let flagged = PerlString::from_str_impl(&"é".repeat(20)).unwrap(); // heap, flagged, UNKNOWN_RANGE
        let plain = PerlString::from_bytes(&[0xE9u8; 20]).unwrap();
        assert_eq!(flagged, plain);

        // The completed walk narrowed the flagged side to UTF8_LATIN1: is_ascii is now a state read.
        assert!(!flagged.is_ascii());
        assert!(!plain.is_ascii());
    }

    #[test]
    fn cheap_probe_defers_range() {
        let s = PerlString::from_str_impl(&"é".repeat(20)).unwrap(); // heap, UTF8_UNKNOWN_RANGE
        assert!(!s.is_ascii()); // cheap probe: narrows to UTF8_NON_ASCII, range still deferred

        // Equality resolves range on demand and still matches the downgraded form.
        let plain = PerlString::from_bytes(&[0xE9u8; 20]).unwrap();
        assert_eq!(s, plain);

        // And a wide heap string resolved through the same path fast-negatives.
        let wide = PerlString::from_str_impl(&"字".repeat(14)).unwrap(); // 42 bytes heap
        assert!(!wide.is_ascii());
        let wide_plain = PerlString::from_bytes(wide.as_bytes()).unwrap();
        assert_ne!(wide, wide_plain);
    }

    #[test]
    fn eq_fast_negative_for_beyond_latin1() {
        // A flagged string containing U+0100+ equals no unflagged string, regardless of bytes.
        let wide = PerlString::from_str_impl("abc字").unwrap();
        assert!(wide.is_utf8());
        let plain = PerlString::from_bytes(wide.as_bytes()).unwrap();
        assert_ne!(wide, plain);

        // And the é (Latin-1) case still compares by character as before.
        let e_flagged = PerlString::from_str_impl("é").unwrap();
        let e_latin1 = PerlString::from_bytes(&[0xE9]).unwrap();
        assert_eq!(e_flagged, e_latin1);
    }

    #[test]
    fn append_range_join_semantics() {
        let mut s = PerlString::from_str_impl("abc").unwrap(); // Ascii
        s.push_str("é").unwrap();
        assert_eq!(s.inline_scan(), Some(InlineScan::Latin1));
        s.push_str("字").unwrap();
        assert_eq!(s.inline_scan(), Some(InlineScan::NonLatin1));
        s.push_str("more ascii").unwrap();
        assert_eq!(s.inline_scan(), Some(InlineScan::NonLatin1), "range cannot go back down on append");
    }

    #[test]
    fn heap_append_range_join() {
        let mut s = PerlString::from_bytes(&b"a".repeat(30)).unwrap();
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
            if s.is_utf8() { flagged_chars(s.as_bytes()).collect() } else { s.as_bytes().iter().map(|&b| b as u32).collect() }
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

    /// Build every reachable (state, storage) witness configuration, with several byte contents behind the
    /// indeterminate states.  Each witness's state is asserted at construction.
    fn grid_witnesses() -> Vec<(String, PerlString)> {
        let mut out: Vec<(String, PerlString)> = Vec::new();

        let mut push = |name: &str, s: PerlString, want: u8| {
            assert_eq!(s.scan_state(), want, "witness {name} state");
            out.push((name.to_string(), s));
        };

        // Inline terminals.
        push("inl-ascii", PerlString::from_bytes(b"ab").unwrap(), scan::ASCII);
        push("inl-latin1", PerlString::from_bytes(&[0xC3, 0xA9]).unwrap(), scan::UTF8_LATIN1);
        push("inl-nonlatin1", PerlString::from_str_impl("字").unwrap(), scan::UTF8_NON_LATIN1);
        push("inl-extended", PerlString::from_bytes(&[0xF4, 0x90, 0x80, 0x80]).unwrap(), scan::EXTENDED_UTF8);
        push("inl-malformed", PerlString::from_bytes(&[0x80]).unwrap(), scan::MALFORMED_UTF8);

        // Heap terminals (narrowed via probes).
        let h_ascii = PerlString::from_bytes(&b"a".repeat(24)).unwrap();
        assert!(h_ascii.is_ascii());
        push("heap-ascii", h_ascii, scan::ASCII);
        let h_l1 = PerlString::from_str_impl(&"é".repeat(12)).unwrap();
        let _ = h_l1.char_len(); // classifies via the fused pass
        push("heap-latin1", h_l1, scan::UTF8_LATIN1);
        let h_nl1 = PerlString::from_str_impl(&"字".repeat(8)).unwrap();
        let _ = h_nl1.char_len();
        push("heap-nonlatin1", h_nl1, scan::UTF8_NON_LATIN1);
        let h_ext = PerlString::from_bytes(&[0xF4, 0x90, 0x80, 0x80].repeat(6)).unwrap();
        assert!(h_ext.is_perl_utf8_valid());
        push("heap-extended", h_ext, scan::EXTENDED_UTF8);
        let h_mal = PerlString::from_bytes(&[0x80; 24]).unwrap();
        assert!(!h_mal.is_perl_utf8_valid());
        push("heap-malformed", h_mal, scan::MALFORMED_UTF8);

        // Indeterminate states, several contents each.
        push("heap-unknown-ascii", PerlString::from_bytes(&b"x".repeat(24)).unwrap(), scan::UNKNOWN);
        push("heap-unknown-latin1", PerlString::from_bytes(&[0xC3, 0xA9].repeat(12)).unwrap(), scan::UNKNOWN);
        push("heap-unknown-malformed", PerlString::from_bytes(&[0x81; 23]).unwrap(), scan::UNKNOWN);
        push("heap-ur-latin1", PerlString::from_str_impl(&"é".repeat(12)).unwrap(), scan::UTF8_UNKNOWN_RANGE);
        push("heap-ur-wide", PerlString::from_str_impl(&"字".repeat(8)).unwrap(), scan::UTF8_UNKNOWN_RANGE);
        let na8_l1 = PerlString::from_str_impl(&"é".repeat(12)).unwrap();
        assert!(!na8_l1.is_ascii());
        push("heap-na8-latin1", na8_l1, scan::UTF8_NON_ASCII);
        let na8_wide = PerlString::from_str_impl(&"字".repeat(8)).unwrap();
        assert!(!na8_wide.is_ascii());
        push("heap-na8-wide", na8_wide, scan::UTF8_NON_ASCII);
        let na_raw = PerlString::from_bytes(&[0x82; 24]).unwrap();
        assert!(!na_raw.is_ascii());
        push("heap-nonascii-raw", na_raw, scan::NON_ASCII);
        let na_raw_valid = PerlString::from_bytes(&[0xC3, 0xA9].repeat(12)).unwrap();
        assert!(!na_raw_valid.is_ascii());
        push("heap-nonascii-valid-bytes", na_raw_valid, scan::NON_ASCII);
        out
    }

    #[test]
    fn full_scan_runs_once_then_state_answers() {
        // A heap string's first as_str pays one validation pass (+ one classification); afterwards every question is a
        // state read — the never-scan-twice law, mechanically.
        let s = PerlString::from_bytes(&[0xC3, 0xA9].repeat(12)).unwrap(); // heap UNKNOWN
        eq_probe::reset();
        assert!(s.as_str().is_some());
        let (scans_first, _) = eq_probe::scans();
        assert_eq!(scans_first, 1, "first as_str must pay exactly ONE fused pass — more is double-scanning");
        eq_probe::reset();
        assert!(s.as_str().is_some());
        assert!(s.is_perl_utf8_valid());
        assert!(!s.is_ascii());
        assert_eq!(s.char_len(), Some(12));
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
        let f = PerlString::from_str_impl(&format!("é{}", "a".repeat(5000))).unwrap(); // heap UNKNOWN_RANGE
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
        let flagged = PerlString::from_str_impl(&"é".repeat(big)).unwrap();
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
        let flagged2 = PerlString::from_str_impl(&"é".repeat(big)).unwrap();
        eq_probe::reset();
        assert_ne!(flagged2, plain2);
        let (_, _, chars2) = eq_probe::snapshot();
        assert!((100..=102).contains(&chars2), "mismatch at 100 must consume ~101 characters, consumed {chars2}");

        // Full equality consumes everything exactly once.
        let flagged3 = PerlString::from_str_impl(&"é".repeat(big)).unwrap();
        let plain3 = PerlString::from_bytes(&vec![0xE9u8; big]).unwrap();
        eq_probe::reset();
        assert_eq!(flagged3, plain3);
        let (_, _, chars3) = eq_probe::snapshot();
        assert_eq!(chars3, big, "completed walk consumes each character exactly once");
    }

    #[test]
    fn eq_grid_decided_pairs_perform_no_scan() {
        // Observable-state companion: a grid-decided comparison must leave an indeterminate operand's state untouched
        // (no scan happened on it).
        let wide = PerlString::from_str_impl("字").unwrap(); // inline NL1, flagged
        assert!(wide.is_utf8()); // from_str of non-ASCII is flagged already
        let unknown = PerlString::from_bytes(&[0x90u8; 24]).unwrap(); // heap UNKNOWN
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

                            // The mechanism assertion: a decided pair must be decided BY THE GRID — same-flag decided
                            // pairs may resolve in the pre-memcmp rows or memcmp's length check; cross-flag decided
                            // pairs must hit a grid row and must never enter the streaming walk.
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

    // ── Blocked hybrid classifier boundaries (§2.2.5) ─────────────

    /// Test-only reference: the scalar single-byte-scan classifier, transcribed as the oracle for the blocked
    /// hybrid (same decode rules, no blocking).
    fn reference_classify(bytes: &[u8]) -> (u8, usize) {
        let mut facts = ScanFacts::default();
        match scalar_decode_span(bytes, 0, bytes.len(), &mut facts) {
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
        // Sequences straddling TWO consecutive fixed grid boundaries: correctness here requires the second
        // block to end at the absolute grid multiple, not at a drifted offset.
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

        // Several compositions, each ~3 blocks long; the last snippet index drawn caps which classes appear so
        // the corpus covers pure-ASCII, valid-only, extended, and malformed mixes.
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
        let a = PerlString::from_bytes(&b"ab".repeat(15)).unwrap();
        assert!(a.is_ascii());
        eq_probe::reset();
        assert_eq!(a.char_len(), Some(30));
        assert_eq!(eq_probe::scans().0, 0, "ASCII char_len is a length read");

        // Latin-1 heap: first call pays ONE fused pass; second call is a cache read.
        let l = PerlString::from_bytes(&[0xC3, 0xA9].repeat(12)).unwrap();
        eq_probe::reset();
        assert_eq!(l.char_len(), Some(12));
        assert_eq!(eq_probe::scans().0, 1, "exactly one fused pass classifies and counts");
        eq_probe::reset();
        assert_eq!(l.char_len(), Some(12));
        assert!(l.as_str().is_some());
        assert_eq!(eq_probe::scans().0, 0, "count and state both cached from the one pass");

        // Extended: counted (a 4-byte and a 13-byte character are one character each).
        let e = PerlString::from_bytes(&[0xF4, 0x90, 0x80, 0x80].repeat(6)).unwrap();
        assert_eq!(e.char_len(), Some(6));

        // Surrogates count one character per encoded sequence; perl never merges pairs.  Container-verified:
        // length(chr 0xD800) == 1; a CESU-style pair decodes to TWO characters (D800, DC00), length 2, distinct from
        // the one-character astral U+10000.
        let lone = PerlString::from_bytes(&[0xED, 0xA0, 0x80]).unwrap();
        assert_eq!(lone.inline_scan(), Some(InlineScan::Extended));
        assert_eq!(lone.char_len(), Some(1));
        let cesu_pair = PerlString::from_bytes(&[0xED, 0xA0, 0x80, 0xED, 0xB0, 0x80]).unwrap();
        assert_eq!(cesu_pair.char_len(), Some(2), "pairs are two characters, never merged");
        let astral = PerlString::from_str_impl("\u{10000}").unwrap();
        assert_eq!(astral.char_len(), Some(1));

        // Malformed: None (ops layer owns perl's warning behavior).
        let m = PerlString::from_bytes(&[0x80; 24]).unwrap();
        assert_eq!(m.char_len(), None);

        // Inline recount, all classes.
        assert_eq!(PerlString::from_str_impl("héllo").unwrap().char_len(), Some(5));
        assert_eq!(PerlString::from_str_impl("字").unwrap().char_len(), Some(1));
        assert_eq!(PerlString::from_bytes(&[0x80]).unwrap().char_len(), None);
    }

    #[test]
    fn char_len_maintained_through_append() {
        let mut s = PerlString::from_bytes(&[0xC3, 0xA9].repeat(12)).unwrap(); // heap
        assert_eq!(s.char_len(), Some(12)); // classify + count: one pass
        eq_probe::reset();
        s.push_str("abc").unwrap(); // classification of the ADDED bytes only
        assert_eq!(s.char_len(), Some(15), "count maintained incrementally");
        let (full, _) = eq_probe::scans();
        assert_eq!(full, 1, "only the appended content was scanned (its own classification pass)");
    }

    #[test]
    fn char_count_shared_across_cow_sharers() {
        let a = PerlString::from_bytes(&[0xC3, 0xA9].repeat(12)).unwrap();
        let b = a.clone(); // shares the buffer
        assert_eq!(a.char_len(), Some(12)); // pays the pass
        eq_probe::reset();
        assert_eq!(b.char_len(), Some(12));
        assert_eq!(eq_probe::scans().0, 0, "sharer reads the cached count");
    }

    // ── COW behavior through the string layer ─────────────────────
    #[test]
    fn clone_shares_heap_buffer_and_append_cow_breaks() {
        let a = PerlString::from_str_impl(&"base".repeat(10)).unwrap(); // heap
        let mut b = a.clone();
        b.push_str("+more").unwrap();
        assert_eq!(a.len(), 40);
        assert_eq!(b.len(), 45);
        assert!(a.as_str().is_some());
    }

    impl PerlString {
        /// Test-only: force the utf8 flag on (simulating `Encode::_utf8_on` / upgrade provenance).
        pub(super) fn set_utf8_for_test(&mut self) {
            self.rebuild_tag(|_u, w, t| (true, w, t));
        }
    }
}
