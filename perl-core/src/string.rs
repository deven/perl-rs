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
//! Variant names are full words: scan word first (`Ascii`, `Latin1`, `NonLatin1`, `Extended`, `Bytes`), then flag words
//! in fixed order: `Flagged` (the *Perl* utf8 flag — a different thing from the scan's validity facts), `Warned`,
//! `Tainted`.  E.g. `InlineLatin1FlaggedTainted`, `HeapWarned`.
//!
//! Equality and hashing are **character-sequence** semantics (§2.3.5): the utf8 flag changes the byte→character
//! mapping, so same-bytes/different-flags can be different strings and different-bytes can be the same string.  Warned
//! and tainted are ignored by `Eq`/`Hash`.

use crate::cow_buffer::{AllocError, CowBuffer};
use crate::value::{Numeric, classify_numeric, parse_float, parse_int_i64_visible, string_would_warn};
use std::cmp::Ordering;
use std::fmt;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::mem;
use std::str::{self, FromStr};

/// Maximum inline payload: chosen so every numeric stringification stays allocation-free (§2.2.3).
pub const INLINE_MAX: usize = 15;

/// The widest byte sequence any non-heap form decodes to, and so the size of the scratch buffer the borrowed-view
/// accessors take.
///
/// It is `INLINE_MAX * 2`, and not by coincidence: **every non-heap form is a 2:1 compression of its decoded bytes**.
/// The packed forms reach that ratio by storing two symbols per byte, four bits each; the Latin-1 form reaches it by
/// declining to spend two bytes on what Latin-1 writes in one, since `U+0080`-`U+00FF` sits inside UTF-8's two-byte
/// range.  The same factor, arrived at from opposite directions — one packing two units into a byte, the other refusing
/// to let one unit take two.  Raw forms compress not at all and are trivially under the bound.
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

/// The inline content class (§2.2.9), eagerly established at construction.  One vocabulary with the §2.2.4 heap
/// lattice: the same classification, eager and in the tag here, lazy and in the buffer header there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InlineClass {
    /// Entirely U+0000–U+007F.
    Ascii,

    /// Rust-valid, entirely U+0000–U+00FF, non-ASCII.
    Latin1,

    /// Rust-valid, contains a character ≥ U+0100.
    NonLatin1,

    /// Perl-decodable, Rust-invalid (§2.2.4): contains a Rust-rejected code point, hence ≥ U+0100.
    Extended,

    /// Bytes neither reader decodes — malformed as UTF-8 to both perl and Rust, and perfectly well-formed Latin-1 to
    /// perl when the flag is off: every octet a character.
    Bytes,
}

/// The storage type: the seventeen-value normative vocabulary (§2.2.9), one value per base variant of the folded tag —
/// the discriminant is this type times the three flag bits.  Coarse questions are the projection methods.  Declaration
/// order is the deterministic tier-selection priority (§2.2.9), which is what the derived `Ord` means.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum StorageType {
    /// Inline, ≤ [`INLINE_MAX`] payload bytes, no allocation: the five content classes, each beside its full-capacity
    /// family twin.  ASCII content, entirely U+0000-U+007F.
    InlineAscii,

    /// ASCII at full capacity, the stored length implied.
    InlineAsciiFull,

    /// Latin-1-range content: valid UTF-8, every code point in U+0000-U+00FF, at least one at or above U+0080 — stored
    /// as its Latin-1 transcoding, one byte per one- or two-byte UTF-8 sequence.
    InlineLatin1,

    /// Latin-1-range content at full capacity.
    InlineLatin1Full,

    /// Rust-valid UTF-8 containing a character at or above U+0100.
    InlineNonLatin1,

    /// Non-Latin-1 content at full capacity.
    InlineNonLatin1Full,

    /// Perl-decodable, Rust-invalid (§2.2.4).
    InlineExtended,

    /// Extended content at full capacity.
    InlineExtendedFull,

    /// Bytes neither UTF-8 reader accepts — well-formed Latin-1 to perl when the flag is off.
    InlineBytes,

    /// Bytes-class content at full capacity.
    InlineBytesFull,

    /// Nibble-packed, 16-30 characters of the numeric alphabet, no allocation (§2.2.9).  The bytes do not exist in that
    /// form, so a borrowed view of them must be decoded into a caller-held buffer.
    PackedNumeric,

    /// Numeric-alphabet content at the full thirty characters.
    PackedNumericFull,

    /// Nibble-packed datetime content whose sixteenth symbol is `T`.
    PackedDateTimePlus,

    /// DateTimePlus content at the full thirty characters.
    PackedDateTimePlusFull,

    /// Nibble-packed datetime content whose alphabet carries `Z`.
    PackedDateTimeZulu,

    /// DateTimeZulu content at the full thirty characters.
    PackedDateTimeZuluFull,

    /// Heap: a shared [`CowBuffer`].
    Heap,
}

impl StorageType {
    /// Any of the ten inline types.
    pub fn is_inline(self) -> bool {
        !self.is_packed() && !self.is_heap()
    }

    /// Any of the six packed types.
    pub fn is_packed(self) -> bool {
        use StorageType::*;
        matches!(self, PackedNumeric | PackedNumericFull | PackedDateTimePlus | PackedDateTimePlusFull | PackedDateTimeZulu | PackedDateTimeZuluFull)
    }

    /// The heap type.
    pub fn is_heap(self) -> bool {
        matches!(self, StorageType::Heap)
    }
}

/// Generates the folded-tag variant set and the accessors over it.  Variant names are written out explicitly (not
/// synthesized by identifier concatenation) so a grep for any variant finds this defining invocation.
macro_rules! define_perl_string {
    (
        inline: [ $( $iv:ident = ($iscan:ident, $istype:ident, $ifull:literal, $iu:literal, $iw:literal, $it:literal) ),* $(,)? ],
        packed: [ $( $pv:ident = ($palpha:ident, $pstype:ident, $pfull:literal, $pu:literal, $pw:literal, $pt:literal) ),* $(,)? ],
        heap:   [ $( $hv:ident = ($hu:literal, $hw:literal, $ht:literal) ),* $(,)? ]
    ) => {
        /// A Perl string.  See the module documentation.  The representation — the folded tag (§2.2.3) — is sealed
        /// behind this newtype: no variant is nameable outside the crate, so no payload can be forged or mutated around
        /// the invariants the unchecked readers rely on (probed: `#[non_exhaustive]` blocks construction but not
        /// mutation through `&mut` pattern binding, so the seal must be privacy).  Construct through the constructors;
        /// inspect through the methods and [`StorageType`].  `Clone` is derived: inline and packed payloads are plain
        /// arrays, and `CowBuffer`'s own `Clone` is the refcount bump the heap forms need.  A hand-written impl that
        /// destructured the tag and rebuilt it cost thirteen nanoseconds to copy sixteen bytes (measured).
        #[derive(Clone)]
        pub struct PerlString(Repr);

        /// The sealed representation: the folded tag itself.  `repr(align(8))` sits here, on the inner enum, where it
        /// costs nothing — on a wrapper of the enclosing fusion it would defeat niche-filling (§2.3.6) — and the
        /// wrapper inherits the alignment.
        #[derive(Clone)]
        #[repr(align(8))]
        enum Repr {
            $( $iv { buf: [u8; INLINE_MAX] }, )*
            $( $pv { nibbles: [u8; PACKED_BYTES] }, )*
            $( $hv(CowBuffer), )*
        }

        impl PerlString {
            /// The storage type (§2.2.9's normative vocabulary).
            pub fn storage_type(&self) -> StorageType {
                match &self.0 {
                    $( Repr::$iv { .. } => StorageType::$istype, )*
                    $( Repr::$pv { .. } => StorageType::$pstype, )*
                    $( Repr::$hv(_) => StorageType::Heap, )*
                }
            }

            /// The Perl utf8 flag (semantic claim, not validity — see module docs).
            pub fn is_utf8(&self) -> bool {
                match &self.0 {
                    $( Repr::$iv { .. } => $iu, )*
                    $( Repr::$pv { .. } => $pu, )*
                    $( Repr::$hv(_) => $hu, )*
                }
            }

            /// Whether the numification warning has fired for this value (§2.3.4).
            pub fn is_warned(&self) -> bool {
                match &self.0 {
                    $( Repr::$iv { .. } => $iw, )*
                    $( Repr::$pv { .. } => $pw, )*
                    $( Repr::$hv(_) => $hw, )*
                }
            }

            /// Whether this value is tainted (§2.6).
            pub fn is_tainted(&self) -> bool {
                match &self.0 {
                    $( Repr::$iv { .. } => $it, )*
                    $( Repr::$pv { .. } => $pt, )*
                    $( Repr::$hv(_) => $ht, )*
                }
            }

            /// The inline content class, or `None` for heap storage.  Internal: the public vocabulary is
            /// [`StorageType`], of which this is the class projection.
            fn inline_class(&self) -> Option<InlineClass> {
                match &self.0 {
                    $( Repr::$iv { .. } => Some(InlineClass::$iscan), )*
                    // Packed alphabets are ASCII by construction, so the scan state is fixed.
                    $( Repr::$pv { .. } => Some(InlineClass::Ascii), )*
                    $( Repr::$hv(_) => None, )*
                }
            }

            /// Rebuild an inline value with the given tag dimensions and payload.  `s` is the payload byte count and
            /// selects the length family; `aux` is the class's second nibble (§2.2.9) — the high-bit count for the
            /// compressed classes, the decoded character count for the verbatim valid classes, zero for Ascii and
            /// Bytes — stored beside `s` in the short family, implied and derived at full capacity.  Internal: tag
            /// transitions go through the public monotone/setter methods.
            fn build_inline(class: InlineClass, utf8: bool, warned: bool, tainted: bool, s: usize, aux: usize, buf: [u8; INLINE_MAX]) -> PerlString {
                debug_assert!(s <= INLINE_MAX);
                debug_assert!(
                    match class {
                        InlineClass::Ascii | InlineClass::Bytes => aux == 0,
                        InlineClass::Latin1 => (1..=s).contains(&aux),
                        InlineClass::NonLatin1 | InlineClass::Extended => (1..s).contains(&aux),
                    },
                    "the aux nibble is per-class (§2.2.9): {class:?} with s {s}, aux {aux}"
                );
                let mut buf = buf;
                if s < INLINE_MAX {
                    // Everything past the content is zeroed, not just the length byte: equal content must have equal
                    // bytes, or representation stops standing in for content.
                    buf[s..].fill(0);
                    buf[LENGTH_BYTE] = ((aux as u8) << 4) | s as u8;
                }

                match (class, s == INLINE_MAX, utf8, warned, tainted) {
                    $( (InlineClass::$iscan, $ifull, $iu, $iw, $it) => PerlString(Repr::$iv { buf }), )*
                }
            }

            /// Rebuild a heap value with the given tag dimensions (buffer preserved).  Build a packed value with the
            /// given alphabet, length family, and tag dimensions.
            fn build_packed(packed: Packed, utf8: bool, warned: bool, tainted: bool) -> PerlString {
                match (packed.alphabet, packed.full, utf8, warned, tainted) {
                    $( (PackedAlphabet::$palpha, $pfull, $pu, $pw, $pt) => PerlString(Repr::$pv { nibbles: packed.nibbles }), )*
                }
            }

            /// The payload behind the tag, borrowed.  Generated rather than hand-written: with three storage kinds the
            /// explicit variant lists ran past a hundred names, and the per-section repetition expresses it exactly.
            fn raw_parts(&self) -> RawParts<'_> {
                match &self.0 {
                    $( Repr::$iv { buf } => RawParts::Inline { class: InlineClass::$iscan, full: $ifull, buf }, )*
                    $( Repr::$pv { nibbles } => RawParts::Packed(Packed {
                        alphabet: PackedAlphabet::$palpha,
                        full: $pfull,
                        nibbles: *nibbles,
                    }), )*
                    $( Repr::$hv(cb) => RawParts::Heap(cb), )*
                }
            }

            /// The inline payload, mutably, for appends that leave the tag alone.  `None` for the other storage kinds,
            /// whose payloads cannot be extended in place.
            fn inline_buf_mut(&mut self) -> Option<(bool, &mut [u8; INLINE_MAX])> {
                match &mut self.0 {
                    $( Repr::$iv { buf } => Some(($ifull, buf)), )*
                    $( Repr::$pv { .. } => None, )*
                    $( Repr::$hv(_) => None, )*
                }
            }

            /// The payload behind the tag, owned — the shape mutation needs, since it rebuilds the tag afterward.
            fn into_raw(self) -> RawOwned {
                match self.0 {
                    $( Repr::$iv { buf } => RawOwned::Inline { class: InlineClass::$iscan, full: $ifull, buf }, )*
                    $( Repr::$pv { nibbles } => RawOwned::Packed(Packed {
                        alphabet: PackedAlphabet::$palpha,
                        full: $pfull,
                        nibbles,
                    }), )*
                    $( Repr::$hv(cb) => RawOwned::Heap(cb), )*
                }
            }

            fn build_heap(utf8: bool, warned: bool, tainted: bool, cb: CowBuffer) -> PerlString {
                match (utf8, warned, tainted) {
                    $( ($hu, $hw, $ht) => PerlString(Repr::$hv(cb)), )*
                }
            }
        }
    };
}

define_perl_string! {
    inline: [
        InlineAscii                                  = (Ascii, InlineAscii, false, false, false, false),
        InlineAsciiFlagged                           = (Ascii, InlineAscii, false, true , false, false),
        InlineAsciiWarned                            = (Ascii, InlineAscii, false, false, true , false),
        InlineAsciiFlaggedWarned                     = (Ascii, InlineAscii, false, true , true , false),
        InlineAsciiTainted                           = (Ascii, InlineAscii, false, false, false, true),
        InlineAsciiFlaggedTainted                    = (Ascii, InlineAscii, false, true , false, true),
        InlineAsciiWarnedTainted                     = (Ascii, InlineAscii, false, false, true , true),
        InlineAsciiFlaggedWarnedTainted              = (Ascii, InlineAscii, false, true , true , true),
        InlineAsciiFull                              = (Ascii, InlineAsciiFull, true , false, false, false),
        InlineAsciiFullFlagged                       = (Ascii, InlineAsciiFull, true , true , false, false),
        InlineAsciiFullWarned                        = (Ascii, InlineAsciiFull, true , false, true , false),
        InlineAsciiFullFlaggedWarned                 = (Ascii, InlineAsciiFull, true , true , true , false),
        InlineAsciiFullTainted                       = (Ascii, InlineAsciiFull, true , false, false, true),
        InlineAsciiFullFlaggedTainted                = (Ascii, InlineAsciiFull, true , true , false, true),
        InlineAsciiFullWarnedTainted                 = (Ascii, InlineAsciiFull, true , false, true , true),
        InlineAsciiFullFlaggedWarnedTainted          = (Ascii, InlineAsciiFull, true , true , true , true),
        InlineLatin1                                 = (Latin1, InlineLatin1, false, false, false, false),
        InlineLatin1Flagged                          = (Latin1, InlineLatin1, false, true , false, false),
        InlineLatin1Warned                           = (Latin1, InlineLatin1, false, false, true , false),
        InlineLatin1FlaggedWarned                    = (Latin1, InlineLatin1, false, true , true , false),
        InlineLatin1Tainted                          = (Latin1, InlineLatin1, false, false, false, true),
        InlineLatin1FlaggedTainted                   = (Latin1, InlineLatin1, false, true , false, true),
        InlineLatin1WarnedTainted                    = (Latin1, InlineLatin1, false, false, true , true),
        InlineLatin1FlaggedWarnedTainted             = (Latin1, InlineLatin1, false, true , true , true),
        InlineLatin1Full                             = (Latin1, InlineLatin1Full, true , false, false, false),
        InlineLatin1FullFlagged                      = (Latin1, InlineLatin1Full, true , true , false, false),
        InlineLatin1FullWarned                       = (Latin1, InlineLatin1Full, true , false, true , false),
        InlineLatin1FullFlaggedWarned                = (Latin1, InlineLatin1Full, true , true , true , false),
        InlineLatin1FullTainted                      = (Latin1, InlineLatin1Full, true , false, false, true),
        InlineLatin1FullFlaggedTainted               = (Latin1, InlineLatin1Full, true , true , false, true),
        InlineLatin1FullWarnedTainted                = (Latin1, InlineLatin1Full, true , false, true , true),
        InlineLatin1FullFlaggedWarnedTainted         = (Latin1, InlineLatin1Full, true , true , true , true),
        InlineNonLatin1                              = (NonLatin1, InlineNonLatin1, false, false, false, false),
        InlineNonLatin1Flagged                       = (NonLatin1, InlineNonLatin1, false, true , false, false),
        InlineNonLatin1Warned                        = (NonLatin1, InlineNonLatin1, false, false, true , false),
        InlineNonLatin1FlaggedWarned                 = (NonLatin1, InlineNonLatin1, false, true , true , false),
        InlineNonLatin1Tainted                       = (NonLatin1, InlineNonLatin1, false, false, false, true),
        InlineNonLatin1FlaggedTainted                = (NonLatin1, InlineNonLatin1, false, true , false, true),
        InlineNonLatin1WarnedTainted                 = (NonLatin1, InlineNonLatin1, false, false, true , true),
        InlineNonLatin1FlaggedWarnedTainted          = (NonLatin1, InlineNonLatin1, false, true , true , true),
        InlineNonLatin1Full                          = (NonLatin1, InlineNonLatin1Full, true , false, false, false),
        InlineNonLatin1FullFlagged                   = (NonLatin1, InlineNonLatin1Full, true , true , false, false),
        InlineNonLatin1FullWarned                    = (NonLatin1, InlineNonLatin1Full, true , false, true , false),
        InlineNonLatin1FullFlaggedWarned             = (NonLatin1, InlineNonLatin1Full, true , true , true , false),
        InlineNonLatin1FullTainted                   = (NonLatin1, InlineNonLatin1Full, true , false, false, true),
        InlineNonLatin1FullFlaggedTainted            = (NonLatin1, InlineNonLatin1Full, true , true , false, true),
        InlineNonLatin1FullWarnedTainted             = (NonLatin1, InlineNonLatin1Full, true , false, true , true),
        InlineNonLatin1FullFlaggedWarnedTainted      = (NonLatin1, InlineNonLatin1Full, true , true , true , true),
        InlineExtended                               = (Extended, InlineExtended, false, false, false, false),
        InlineExtendedFlagged                        = (Extended, InlineExtended, false, true , false, false),
        InlineExtendedWarned                         = (Extended, InlineExtended, false, false, true , false),
        InlineExtendedFlaggedWarned                  = (Extended, InlineExtended, false, true , true , false),
        InlineExtendedTainted                        = (Extended, InlineExtended, false, false, false, true),
        InlineExtendedFlaggedTainted                 = (Extended, InlineExtended, false, true , false, true),
        InlineExtendedWarnedTainted                  = (Extended, InlineExtended, false, false, true , true),
        InlineExtendedFlaggedWarnedTainted           = (Extended, InlineExtended, false, true , true , true),
        InlineExtendedFull                           = (Extended, InlineExtendedFull, true , false, false, false),
        InlineExtendedFullFlagged                    = (Extended, InlineExtendedFull, true , true , false, false),
        InlineExtendedFullWarned                     = (Extended, InlineExtendedFull, true , false, true , false),
        InlineExtendedFullFlaggedWarned              = (Extended, InlineExtendedFull, true , true , true , false),
        InlineExtendedFullTainted                    = (Extended, InlineExtendedFull, true , false, false, true),
        InlineExtendedFullFlaggedTainted             = (Extended, InlineExtendedFull, true , true , false, true),
        InlineExtendedFullWarnedTainted              = (Extended, InlineExtendedFull, true , false, true , true),
        InlineExtendedFullFlaggedWarnedTainted       = (Extended, InlineExtendedFull, true , true , true , true),
        InlineBytes                              = (Bytes, InlineBytes, false, false, false, false),
        InlineBytesFlagged                       = (Bytes, InlineBytes, false, true , false, false),
        InlineBytesWarned                        = (Bytes, InlineBytes, false, false, true , false),
        InlineBytesFlaggedWarned                 = (Bytes, InlineBytes, false, true , true , false),
        InlineBytesTainted                       = (Bytes, InlineBytes, false, false, false, true),
        InlineBytesFlaggedTainted                = (Bytes, InlineBytes, false, true , false, true),
        InlineBytesWarnedTainted                 = (Bytes, InlineBytes, false, false, true , true),
        InlineBytesFlaggedWarnedTainted          = (Bytes, InlineBytes, false, true , true , true),
        InlineBytesFull                          = (Bytes, InlineBytesFull, true , false, false, false),
        InlineBytesFullFlagged                   = (Bytes, InlineBytesFull, true , true , false, false),
        InlineBytesFullWarned                    = (Bytes, InlineBytesFull, true , false, true , false),
        InlineBytesFullFlaggedWarned             = (Bytes, InlineBytesFull, true , true , true , false),
        InlineBytesFullTainted                   = (Bytes, InlineBytesFull, true , false, false, true),
        InlineBytesFullFlaggedTainted            = (Bytes, InlineBytesFull, true , true , false, true),
        InlineBytesFullWarnedTainted             = (Bytes, InlineBytesFull, true , false, true , true),
        InlineBytesFullFlaggedWarnedTainted      = (Bytes, InlineBytesFull, true , true , true , true),
    ],
    packed: [
        PackedNum                                    = (Numeric, PackedNumeric, false, false, false, false),
        PackedNumFlagged                             = (Numeric, PackedNumeric, false, true , false, false),
        PackedNumWarned                              = (Numeric, PackedNumeric, false, false, true , false),
        PackedNumFlaggedWarned                       = (Numeric, PackedNumeric, false, true , true , false),
        PackedNumTainted                             = (Numeric, PackedNumeric, false, false, false, true),
        PackedNumFlaggedTainted                      = (Numeric, PackedNumeric, false, true , false, true),
        PackedNumWarnedTainted                       = (Numeric, PackedNumeric, false, false, true , true),
        PackedNumFlaggedWarnedTainted                = (Numeric, PackedNumeric, false, true , true , true),
        PackedNumFull                                = (Numeric, PackedNumericFull, true , false, false, false),
        PackedNumFullFlagged                         = (Numeric, PackedNumericFull, true , true , false, false),
        PackedNumFullWarned                          = (Numeric, PackedNumericFull, true , false, true , false),
        PackedNumFullFlaggedWarned                   = (Numeric, PackedNumericFull, true , true , true , false),
        PackedNumFullTainted                         = (Numeric, PackedNumericFull, true , false, false, true),
        PackedNumFullFlaggedTainted                  = (Numeric, PackedNumericFull, true , true , false, true),
        PackedNumFullWarnedTainted                   = (Numeric, PackedNumericFull, true , false, true , true),
        PackedNumFullFlaggedWarnedTainted            = (Numeric, PackedNumericFull, true , true , true , true),
        PackedPlus                                   = (DateTimePlus, PackedDateTimePlus, false, false, false, false),
        PackedPlusFlagged                            = (DateTimePlus, PackedDateTimePlus, false, true , false, false),
        PackedPlusWarned                             = (DateTimePlus, PackedDateTimePlus, false, false, true , false),
        PackedPlusFlaggedWarned                      = (DateTimePlus, PackedDateTimePlus, false, true , true , false),
        PackedPlusTainted                            = (DateTimePlus, PackedDateTimePlus, false, false, false, true),
        PackedPlusFlaggedTainted                     = (DateTimePlus, PackedDateTimePlus, false, true , false, true),
        PackedPlusWarnedTainted                      = (DateTimePlus, PackedDateTimePlus, false, false, true , true),
        PackedPlusFlaggedWarnedTainted               = (DateTimePlus, PackedDateTimePlus, false, true , true , true),
        PackedPlusFull                               = (DateTimePlus, PackedDateTimePlusFull, true , false, false, false),
        PackedPlusFullFlagged                        = (DateTimePlus, PackedDateTimePlusFull, true , true , false, false),
        PackedPlusFullWarned                         = (DateTimePlus, PackedDateTimePlusFull, true , false, true , false),
        PackedPlusFullFlaggedWarned                  = (DateTimePlus, PackedDateTimePlusFull, true , true , true , false),
        PackedPlusFullTainted                        = (DateTimePlus, PackedDateTimePlusFull, true , false, false, true),
        PackedPlusFullFlaggedTainted                 = (DateTimePlus, PackedDateTimePlusFull, true , true , false, true),
        PackedPlusFullWarnedTainted                  = (DateTimePlus, PackedDateTimePlusFull, true , false, true , true),
        PackedPlusFullFlaggedWarnedTainted           = (DateTimePlus, PackedDateTimePlusFull, true , true , true , true),
        PackedZulu                                   = (DateTimeZulu, PackedDateTimeZulu, false, false, false, false),
        PackedZuluFlagged                            = (DateTimeZulu, PackedDateTimeZulu, false, true , false, false),
        PackedZuluWarned                             = (DateTimeZulu, PackedDateTimeZulu, false, false, true , false),
        PackedZuluFlaggedWarned                      = (DateTimeZulu, PackedDateTimeZulu, false, true , true , false),
        PackedZuluTainted                            = (DateTimeZulu, PackedDateTimeZulu, false, false, false, true),
        PackedZuluFlaggedTainted                     = (DateTimeZulu, PackedDateTimeZulu, false, true , false, true),
        PackedZuluWarnedTainted                      = (DateTimeZulu, PackedDateTimeZulu, false, false, true , true),
        PackedZuluFlaggedWarnedTainted               = (DateTimeZulu, PackedDateTimeZulu, false, true , true , true),
        PackedZuluFull                               = (DateTimeZulu, PackedDateTimeZuluFull, true , false, false, false),
        PackedZuluFullFlagged                        = (DateTimeZulu, PackedDateTimeZuluFull, true , true , false, false),
        PackedZuluFullWarned                         = (DateTimeZulu, PackedDateTimeZuluFull, true , false, true , false),
        PackedZuluFullFlaggedWarned                  = (DateTimeZulu, PackedDateTimeZuluFull, true , true , true , false),
        PackedZuluFullTainted                        = (DateTimeZulu, PackedDateTimeZuluFull, true , false, false, true),
        PackedZuluFullFlaggedTainted                 = (DateTimeZulu, PackedDateTimeZuluFull, true , true , false, true),
        PackedZuluFullWarnedTainted                  = (DateTimeZulu, PackedDateTimeZuluFull, true , false, true , true),
        PackedZuluFullFlaggedWarnedTainted           = (DateTimeZulu, PackedDateTimeZuluFull, true , true , true , true),
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

// ── Classification and construction ───────────────────────────────
/// Where a short inline string keeps its length byte: the byte a fifteenth character would have occupied.  Two nibbles
/// (§2.2.9): the low nibble is `s`, the payload bytes stored; the high nibble is the class's aux count — the high-bit
/// count `h` for the compressed classes, the decoded character count for the verbatim valid classes, canonically zero
/// for Ascii (whose `h` is zero by tag) and Bytes.  Full capacity implies `s = 15` and derives the aux instead.
const LENGTH_BYTE: usize = INLINE_MAX - 1;

/// The stored payload byte count `s` of an inline value: implied at full capacity, the length byte's low nibble
/// otherwise.  An explicit length costs nothing at full capacity and buys a nibble read instead of a scan everywhere
/// else, and it lets NUL-bearing content live inline, which a terminator cannot.
#[inline]
fn inline_stored(full: bool, buf: &[u8; INLINE_MAX]) -> usize {
    if full { INLINE_MAX } else { (buf[LENGTH_BYTE] & 0x0F) as usize }
}

/// The stored aux nibble of a short-family inline value; the full family derives its aux instead (§2.2.9).
#[inline]
fn inline_aux(buf: &[u8; INLINE_MAX]) -> usize {
    (buf[LENGTH_BYTE] >> 4) as usize
}

/// The count of stored bytes with the high bit set in a compressed payload — each the transcoding of a two-byte UTF-8
/// sequence — the full family's derived `h` (§2.2.9).
fn high_count(cps: &[u8]) -> usize {
    cps.iter().filter(|&&c| c >= 0x80).count()
}

/// The internal byte length of an inline value: `s + h` for the Latin-1 class, whose stored bytes each expand back to
/// one or two internal bytes, and `s` for everything else — Ascii's `h` is zero by tag, and the verbatim classes store
/// the internal bytes themselves (§2.2.9).
fn inline_internal_len(class: InlineClass, full: bool, buf: &[u8; INLINE_MAX]) -> usize {
    let s = inline_stored(full, buf);
    match class {
        InlineClass::Latin1 => s + if full { high_count(buf) } else { inline_aux(buf) },
        _ => s,
    }
}

/// The aux nibble, stored or derived: the short family reads it; the full family recomputes it (§2.2.9).
fn inline_derived_aux(class: InlineClass, full: bool, buf: &[u8; INLINE_MAX]) -> usize {
    if !full {
        return inline_aux(buf);
    }
    match class {
        InlineClass::Ascii | InlineClass::Bytes => 0,
        InlineClass::Latin1 => high_count(buf),
        InlineClass::NonLatin1 | InlineClass::Extended => classify_full(buf).1,
    }
}

/// Strict decode of Latin-1-range UTF-8 into its Latin-1 transcoding: every code point in U+0000-U+00FF, canonical
/// encodings only, each one- or two-byte sequence stored as its single-byte Latin-1 equivalent — flag-blind, since the
/// class is a fact about the bytes.  Overlong forms (`C0`/`C1` leads — including `C0 80`, the overlong NUL) and every
/// lead at or above `C4` fail, since noncanonical content must never compress; so does a sixteenth stored byte, which
/// is why up to thirty input bytes can land inline (§2.2.9).  Returns the transcoded bytes beside the two nibbles: the
/// stored count `s` and the high-bit count `h`.
fn decode_latin1_range(bytes: &[u8]) -> Option<([u8; INLINE_MAX], usize, usize)> {
    if bytes.len() > DECODE_MAX {
        return None; // More than thirty bytes cannot transcode to fifteen.
    }

    let mut cp = [0u8; INLINE_MAX];
    let (mut s, mut h, mut i) = (0usize, 0usize, 0usize);
    while i < bytes.len() {
        let decoded = match bytes[i] {
            b @ 0x00..=0x7F => {
                i += 1;
                b
            }
            lead @ (0xC2 | 0xC3) => {
                let &cont = bytes.get(i + 1)?;
                if cont & 0xC0 != 0x80 {
                    return None;
                }
                i += 2;
                h += 1;
                ((lead & 0x03) << 6) | (cont & 0x3F)
            }
            _ => return None,
        };

        if s == INLINE_MAX {
            return None; // A sixteenth stored byte: past the payload.
        }
        cp[s] = decoded;
        s += 1;
    }

    Some((cp, s, h))
}

/// Expand one stored byte back to the UTF-8 sequence it transcoded, returning the width — the compressed classes'
/// expansion primitive.  Callers guarantee room: every consumer writes into a `DECODE_MAX` scratch sized for the widest
/// expansion.
fn expand_latin1(b: u8, out: &mut [u8]) -> usize {
    if b < 0x80 {
        out[0] = b;
        1
    } else {
        out[0] = 0xC0 | (b >> 6);
        out[1] = 0x80 | (b & 0x3F);
        2
    }
}

/// Classify content into its canonical inline form: the class, the two nibbles, and the payload — `None` when no inline
/// form holds it, packed and heap lying past (§2.2.9's ladder, inline rungs).  Determinism is disjointness: valid
/// Latin-1-range UTF-8 always compresses — up to thirty input bytes — and the verbatim classes hold exactly the
/// fifteen-byte-or-shorter content failing that test, the Bytes class by default when the tag rules out every other.
/// The flag is never consulted: the class is a fact about the bytes.
fn classify_inline(bytes: &[u8]) -> Option<(InlineClass, usize, usize, [u8; INLINE_MAX])> {
    if let Some((cp, s, h)) = decode_latin1_range(bytes) {
        let class = if h == 0 { InlineClass::Ascii } else { InlineClass::Latin1 };
        return Some((class, s, h, cp));
    }

    if bytes.len() > INLINE_MAX {
        return None;
    }

    let (class, aux) = match classify_full(bytes) {
        (scan::UTF8_NON_LATIN1, chars) => (InlineClass::NonLatin1, chars),
        (scan::EXTENDED_UTF8, chars) => (InlineClass::Extended, chars),
        // ASCII and Latin-1-range content took the compressed branch above; what remains is the Bytes residual.
        _ => (InlineClass::Bytes, 0),
    };

    Some((class, bytes.len(), aux, inline_payload(bytes)))
}

/// Whether content can be stored inline.  Length alone: an explicit length admits NUL-bearing content that a terminator
/// would have to reject.
fn inline_eligible(bytes: &[u8]) -> bool {
    bytes.len() <= INLINE_MAX
}

fn inline_payload(bytes: &[u8]) -> [u8; INLINE_MAX] {
    debug_assert!(inline_eligible(bytes));
    let mut buf = [0u8; INLINE_MAX];
    buf[..bytes.len()].copy_from_slice(bytes);

    buf
}

impl PerlString {
    /// Construct from a Rust `&str`.  ASCII content is stored unflagged (the canonical downgraded form, §2.3.5);
    /// non-ASCII content is stored with the utf8 flag, its validity known from the type.  Allocation failure is the
    /// only error.
    ///
    /// Generic at the boundary so that embedders holding a `String`, a `Cow`, or one of the compact string types from
    /// the ecosystem need no conversion; the ladder beneath is monomorphic and instantiated once.
    pub fn new(s: impl AsRef<str>) -> Result<PerlString, AllocError> {
        let s = s.as_ref();

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

    /// Construct from raw bytes (I/O, `Encode`, lexer literals).  Unflagged; inline content gets its eager terminal
    /// scan, heap content defers all scanning (`UNKNOWN`), per §2.2.7.
    pub fn from_bytes(bytes: impl AsRef<[u8]>) -> Result<PerlString, AllocError> {
        let bytes = bytes.as_ref();
        match PerlString::inline_bytes(bytes) {
            Some(inline) => Ok(inline),
            None => {
                let cb = CowBuffer::from_slice(bytes)?; // scan byte born UNKNOWN
                Ok(PerlString::build_heap(false, false, false, cb))
            }
        }
    }

    /// The full tier ladder with every tag dimension supplied — the transforms' constructor, and `from_bytes` in
    /// spirit: compressed inline, verbatim inline, packed, heap, in the ruled order (§2.2.9).  Internal: public
    /// construction fixes the flags.
    fn tiered(bytes: &[u8], utf8: bool, warned: bool, tainted: bool) -> Result<PerlString, AllocError> {
        if let Some((class, stored, aux, buf)) = classify_inline(bytes) {
            return Ok(PerlString::build_inline(class, utf8, warned, tainted, stored, aux, buf));
        }
        if let Some(p) = pack(bytes) {
            return Ok(PerlString::build_packed(p, utf8, warned, tainted));
        }
        let cb = CowBuffer::from_slice(bytes)?;
        Ok(PerlString::build_heap(utf8, warned, tainted, cb))
    }

    /// The empty string: inline, unflagged, trivially ASCII.  Infallible, unlike the other constructors — an empty
    /// payload needs no allocation — which is also what lets `Default` exist.
    pub fn empty() -> PerlString {
        PerlString::build_inline(InlineClass::Ascii, false, false, false, 0, 0, [0u8; INLINE_MAX])
    }

    /// Construct from a Rust `&str` **without allocating**, or `None` if the content cannot be stored in the value
    /// itself.  Flagging follows [`FromStr`]: ASCII stores unflagged, non-ASCII flagged.
    ///
    /// The contract is the guarantee, not a byte count: `Some` means no heap allocation occurred, so the set of
    /// accepted content widens whenever the non-allocating storage forms do.  Callers who merely prefer inline storage
    /// can write `PerlString::inline(s).unwrap_or_default()`; callers who need the content stored either way should use
    /// the fallible constructors instead.
    pub fn inline(s: impl AsRef<str>) -> Option<PerlString> {
        let bytes = s.as_ref().as_bytes();

        // The inline rungs come first (§2.2.9's ladder), and they now reach 16-30-byte Latin-1-compressible content:
        // the accepted set has widened, exactly as the guarantee-not-a-count contract promises.
        if let Some((class, stored, aux, buf)) = classify_inline(bytes) {
            // Ascii stores unflagged, non-ASCII flagged, following `FromStr`.  Bytes/Extended are impossible from a
            // `&str`, whose bytes are Rust-valid.
            return Some(PerlString::build_inline(class, class != InlineClass::Ascii, false, false, stored, aux, buf));
        }

        // The packed band holds 16-30-character alphabet content and allocates nothing either.  Past it, `None` starts
        // meaning "the heap".
        if !(MIN_PACKED_LEN..=MAX_PACKED_LEN).contains(&bytes.len()) {
            return None;
        }

        pack(bytes).map(|p| PerlString::build_packed(p, false, false, false))
    }

    /// Construct from raw bytes **without allocating**, or `None` if the content cannot be stored in the value itself.
    /// Unflagged, like [`PerlString::from_bytes`]; the same guarantee-not-a-count contract as [`PerlString::inline`].
    pub fn inline_bytes(bytes: impl AsRef<[u8]>) -> Option<PerlString> {
        let bytes = bytes.as_ref();

        if let Some((class, stored, aux, buf)) = classify_inline(bytes) {
            return Some(PerlString::build_inline(class, false, false, false, stored, aux, buf));
        }

        if !(MIN_PACKED_LEN..=MAX_PACKED_LEN).contains(&bytes.len()) {
            return None;
        }

        pack(bytes).map(|p| PerlString::build_packed(p, false, false, false))
    }

    // ── Accessors ─────────────────────────────────────────────────
    /// Length in bytes.  No dereference for inline; handle mirror for heap.
    pub fn len(&self) -> usize {
        match self.raw_parts() {
            RawParts::Inline { class, full, buf } => inline_internal_len(class, full, buf),
            RawParts::Packed(p) => p.len(),
            RawParts::Heap(cb) => cb.len(),
        }
    }

    /// Whether the string is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The string's bytes — perl's buffer contents, whatever form this value stores them in.
    ///
    /// Borrowed from the string where the bytes exist in that form, and from `scratch` where they do not — packed
    /// content is nibbles, so its bytes have to be decoded somewhere, and a buffer built inside this call could not
    /// outlive it.  The caller supplies one stack array and never learns which case it got, which is what lets the
    /// storage forms multiply without every consumer following along.
    ///
    /// **Every compressing storage form must expand here.**  This is the one place the expansion happens, and it is why
    /// length, comparison, and hashing are correct without knowing which form they were handed: they read the value's
    /// bytes, never a payload.  An arm returning a compressed payload would silently give every consumer the wrong
    /// string — most damagingly the Latin-1 inline form (§2.2.9), whose stored bytes are *half* the internal bytes at
    /// the widest, so returning them unexpanded would make a thirty-byte string look like fifteen and compare equal to
    /// a string it differs from.  `DECODE_MAX` is `INLINE_MAX * 2` for this reason: it is sized for the widest
    /// expansion any form may need.
    pub fn as_bytes<'a>(&'a self, scratch: &'a mut [u8; DECODE_MAX]) -> &'a [u8] {
        // `len` derives the byte count per storage form, independently of this decode, so the two disagreeing means an
        // arm forgot to expand.  Its own scratch, the caller's being borrowed for the return.
        #[cfg(debug_assertions)]
        {
            let mut probe = [0u8; DECODE_MAX];
            debug_assert_eq!(self.as_bytes_inner(&mut probe).len(), self.len(), "as_bytes must yield the value's bytes, not a compressed payload");
        }

        self.as_bytes_inner(scratch)
    }

    fn as_bytes_inner<'a>(&'a self, scratch: &'a mut [u8; DECODE_MAX]) -> &'a [u8] {
        match self.raw_parts() {
            RawParts::Inline { class, full, buf } => {
                let stored = inline_stored(full, buf);
                if class == InlineClass::Latin1 {
                    // The compressed payload is the Latin-1 transcoding; the value's bytes are its expansion (§2.2.9).
                    let mut n = 0;
                    for &c in &buf[..stored] {
                        n += expand_latin1(c, &mut scratch[n..]);
                    }

                    return &scratch[..n];
                }
                &buf[..stored]
            }
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
            RawParts::Inline { class, full, buf } => {
                let stored = inline_stored(full, buf);
                match class {
                    InlineClass::Latin1 => {
                        let mut n = 0;
                        for &c in &buf[..stored] {
                            n += expand_latin1(c, &mut scratch[n..]);
                        }

                        // SAFETY: the expansion emits canonical one- and two-byte encodings only — valid by
                        // construction.
                        Some(unsafe { str::from_utf8_unchecked(&scratch[..n]) })
                    }

                    // SAFETY: the Ascii class certifies seven-bit content and NonLatin1 certifies Rust-valid UTF-8,
                    // both established by a full scan at construction; inline mutation reclassifies.
                    InlineClass::Ascii | InlineClass::NonLatin1 => Some(unsafe { str::from_utf8_unchecked(&buf[..stored]) }),
                    InlineClass::Extended | InlineClass::Bytes => None,
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
            RawParts::Inline { .. } => self.inline_class() == Some(InlineClass::Ascii),

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
            RawParts::Inline { .. } => match self.inline_class() {
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
            RawParts::Inline { .. } => !matches!(self.inline_class(), Some(InlineClass::Bytes)),
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
            RawParts::Inline { class, full, buf } => {
                let stored = inline_stored(full, buf);
                match class {
                    // Under perl's flagged semantics — this method's question — the transcoded units are the
                    // characters, so the count is the stored count: O(1) for all Latin-1-range content, where the
                    // raw-byte tier could only shortcut ASCII.
                    InlineClass::Ascii | InlineClass::Latin1 => Some(stored),
                    InlineClass::Bytes => None,
                    // The verbatim valid classes carry their count in the aux nibble; the full family derives it.
                    InlineClass::NonLatin1 | InlineClass::Extended => Some(if full { classify_full(&buf[..stored]).1 } else { inline_aux(buf) }),
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

    // ── Representation transforms (§2.2.9) ────────────────────────
    // Private until the ops layer calls them — no public surface without a caller — with the tests pinning the
    // container-verified facts the design records.

    /// `Encode::_utf8_on`/`_utf8_off`: reinterpretation is a pure flag flip.  The class is a fact about the bytes, so
    /// the representation is already right, verbatim and compressed classes alike (§2.2.9): an upgraded `é` becomes the
    /// flag-off two-character `C3.A9` with the payload untouched.
    fn reinterpret_utf8(&mut self, utf8: bool) {
        self.rebuild_tag(|_, w, t| (utf8, w, t));
    }

    /// `utf8::upgrade`: the same characters, flagged.  Flagged content is untouched.  An unflagged string's characters
    /// are its internal bytes, so the result stores exactly those bytes as its compressed payload — zero byte work for
    /// the Ascii and verbatim classes, whose payload already is the internal bytes (the monster: flag-off `E9` re-tags
    /// Bytes to flagged Latin-1, payload identical), and one expansion copy for the Latin-1 class, whose stored bytes
    /// are not (upgrading flag-off `C3 A9` yields the two-character `Ã©`, not `é` — that reinterpretation is
    /// `_utf8_on`'s).  Past fifteen characters the result is heap: sixteen to thirty characters have no flagged
    /// non-heap form unless ASCII, and ASCII took the flip.
    #[cfg_attr(not(test), expect(dead_code))] // The ops layer is the caller-to-be; the tests keep it honest.
    fn upgraded(&self) -> Result<PerlString, AllocError> {
        if self.is_utf8() {
            return Ok(self.clone());
        }

        if self.is_ascii() {
            // Characters, bytes, and encoding all coincide: the representation is already right in every tier.
            let mut s = self.clone();
            s.reinterpret_utf8(true);
            return Ok(s);
        }

        let (w, t) = (self.is_warned(), self.is_tainted());
        let mut scratch = [0u8; DECODE_MAX];
        let internal = self.as_bytes(&mut scratch);

        if internal.len() <= INLINE_MAX {
            let mut buf = [0u8; INLINE_MAX];
            buf[..internal.len()].copy_from_slice(internal);
            let h = high_count(internal);
            debug_assert!(h > 0, "all-ASCII content took the flip above");
            return Ok(PerlString::build_inline(InlineClass::Latin1, true, w, t, internal.len(), h, buf));
        }

        // Sixteen or more non-ASCII characters: heap, each byte expanding to its encoding.
        let mut cb = CowBuffer::with_capacity(internal.len() + high_count(internal))?;
        let mut pair = [0u8; 2];
        for &b in internal {
            let n = expand_latin1(b, &mut pair);
            cb.extend_from_slice(&pair[..n])?;
        }
        cb.narrow_scan(scan::UTF8_LATIN1);

        Ok(PerlString::build_heap(true, w, t, cb))
    }

    /// `utf8::downgrade`: the same characters, unflagged — `None` where a character exceeds U+00FF, which is where
    /// perl's downgrade dies.  Unflagged content is untouched.  The compressed classes' characters are their stored
    /// bytes, so the result is those bytes reclassified as octets — zero byte work for the monster (`é` re-tags Latin-1
    /// to Bytes, payload `E9` identical), re-compression where the octets happen to be valid Latin-1-range UTF-8.  The
    /// flagged verbatim classes cannot downgrade: NonLatin1 and Extended hold a character at or above U+0100 by their
    /// class, and the Bytes class flagged has no characters at all.
    #[cfg_attr(not(test), expect(dead_code))] // The ops layer is the caller-to-be; the tests keep it honest.
    fn downgraded(&self) -> Result<Option<PerlString>, AllocError> {
        if !self.is_utf8() {
            return Ok(Some(self.clone()));
        }

        if self.is_ascii() {
            let mut s = self.clone();
            s.reinterpret_utf8(false);
            return Ok(Some(s));
        }

        let (w, t) = (self.is_warned(), self.is_tainted());
        match self.raw_parts() {
            RawParts::Inline { class: InlineClass::Latin1, full, buf } => {
                let stored = inline_stored(full, buf);

                Ok(Some(PerlString::tiered(&buf[..stored], false, w, t)?))
            }
            RawParts::Inline { .. } => Ok(None),
            RawParts::Packed(_) => {
                // Packed content is ASCII by construction, so the flip above took it; answering the same way keeps the
                // arm total without a panic.
                let mut s = self.clone();
                s.reinterpret_utf8(false);

                Ok(Some(s))
            }
            RawParts::Heap(cb) => {
                // Walk the encoding: every character must sit in U+0000-U+00FF, emitted as its single byte.  The result
                // re-runs the ladder — sixteen to thirty emitted octets can compress right back inline.
                let bytes = cb.as_slice();
                let mut out = CowBuffer::with_capacity(bytes.len())?;
                let mut i = 0;
                while i < bytes.len() {
                    match bytes[i] {
                        b @ 0x00..=0x7F => {
                            out.extend_from_slice(&[b])?;
                            i += 1;
                        }
                        lead @ (0xC2 | 0xC3) if i + 1 < bytes.len() && bytes[i + 1] & 0xC0 == 0x80 => {
                            out.extend_from_slice(&[((lead & 0x03) << 6) | (bytes[i + 1] & 0x3F)])?;
                            i += 2;
                        }
                        _ => return Ok(None), // A character past U+00FF, or no character at all.
                    }
                }

                if out.len() <= MAX_PACKED_LEN {
                    return Ok(Some(PerlString::tiered(out.as_slice(), false, w, t)?));
                }

                Ok(Some(PerlString::build_heap(false, w, t, out)))
            }
        }
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
            RawOwned::Inline { class, full, buf } => {
                let (s, aux) = (inline_stored(full, &buf), inline_derived_aux(class, full, &buf));
                PerlString::build_inline(class, u2, w2, t2, s, aux, buf)
            }
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

        // Inline content never needs `mem::take`: the payload is a fifteen-byte `Copy` array, so it can be read out,
        // extended, and written back.  Taking exists to move a `CowBuffer` out of `&mut self`, which only the heap arm
        // below actually requires.  Cheapest case first, and touching nothing it does not have to: only the Ascii class
        // writes through — its payload is the raw bytes with a zero aux nibble, so appending ASCII content that still
        // fits short extends the existing variant bit-identically to the raw-byte tier, the length byte updated in
        // place.  Every other class, and any append reaching full capacity (which changes the family), rebuilds through
        // canonical selection below — append is byte mutation (§2.2.9).
        if self.inline_class() == Some(InlineClass::Ascii)
            && matches!(kind, AppendKind::Valid { class: scan::ASCII, .. })
            && let Some((full, dst)) = self.inline_buf_mut()
            && !full
        {
            let len = (dst[LENGTH_BYTE] & 0x0F) as usize;
            let new_len = len + bytes.len();
            if new_len < INLINE_MAX {
                dst[len..new_len].copy_from_slice(bytes);
                dst[LENGTH_BYTE] = new_len as u8; // Aux stays zero: the class is Ascii on both sides.
                return Ok(());
            }
        }

        // Otherwise the payload has to move.  For inline content that is still only a materialisation of at most thirty
        // bytes and one rebuild: append is byte mutation, so the result re-runs canonical selection (§2.2.9) over the
        // value's internal bytes — the compressed classes expand first, exactly as `as_bytes` would.
        let inline = match self.raw_parts() {
            RawParts::Inline { class, full, buf } => Some((class, inline_stored(full, buf), *buf)),
            _ => None,
        };

        if let Some((class, stored, buf)) = inline {
            let (u, w, t) = (self.is_utf8(), self.is_warned(), self.is_tainted());

            let mut internal = [0u8; DECODE_MAX];
            let ilen = if class == InlineClass::Latin1 {
                let mut n = 0;
                for &c in &buf[..stored] {
                    n += expand_latin1(c, &mut internal[n..]);
                }
                n
            } else {
                internal[..stored].copy_from_slice(&buf[..stored]);
                stored
            };
            let total = ilen + bytes.len();

            if total <= MAX_PACKED_LEN {
                let mut combined = [0u8; MAX_PACKED_LEN];
                combined[..ilen].copy_from_slice(&internal[..ilen]);
                combined[ilen..total].copy_from_slice(bytes);

                if let Some((nc, ns, naux, nbuf)) = classify_inline(&combined[..total]) {
                    *self = PerlString::build_inline(nc, u, w, t, ns, naux, nbuf);
                    return Ok(());
                }

                if let Some(packed) = pack(&combined[..total]) {
                    *self = PerlString::build_packed(packed, u, w, t);
                    return Ok(());
                }

                // Sixteen to thirty bytes fitting neither a compressed payload nor an alphabet: the heap, below.
            }

            let mut cb = CowBuffer::with_capacity(total + (total >> 2))?;
            cb.extend_from_slice(&internal[..ilen])?;
            cb.extend_from_slice(bytes)?;
            cb.narrow_scan(append_transition_heap(inline_scan_to_heap(class), kind));
            *self = PerlString::build_heap(u, w, t, cb);

            return Ok(());
        }

        let (u, w, t) = (self.is_utf8(), self.is_warned(), self.is_tainted());
        let old = mem::take(self);

        *self = match old.into_raw() {
            RawOwned::Inline { .. } => return Ok(()), // Handled above; unreachable.
            RawOwned::Packed(p) => {
                if let Some(packed) = p.push(bytes) {
                    // In place: the existing nibbles are kept, rather than the whole result being decoded and
                    // re-encoded on every append.
                    PerlString::build_packed(packed, u, w, t)
                } else {
                    // Past the band, or no longer alphabet-conformant: decode once, on the way out of the tier.  Packed
                    // content is ASCII, so the heap state starts from there.
                    let (decoded, len) = p.unpack();
                    let old_bytes = &decoded[..len];
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
    Inline { class: InlineClass, full: bool, buf: &'a [u8; INLINE_MAX] },
    Packed(Packed),
    Heap(&'a CowBuffer),
}

enum RawOwned {
    Inline { class: InlineClass, full: bool, buf: [u8; INLINE_MAX] },
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

fn inline_scan_to_heap(s: InlineClass) -> u8 {
    match s {
        InlineClass::Ascii => scan::ASCII,
        InlineClass::Latin1 => scan::UTF8_LATIN1,
        InlineClass::NonLatin1 => scan::UTF8_NON_LATIN1,
        InlineClass::Extended => scan::EXTENDED_UTF8,
        InlineClass::Bytes => scan::MALFORMED_UTF8,
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
        parse_int_i64_visible(self.as_bytes(&mut scratch))
    }

    /// Perl's float numification: leading-numeric prefix, `Inf`/`NaN` forms, zero for a non-numeric string.
    pub fn to_float(&self) -> f64 {
        let mut scratch = [0u8; DECODE_MAX];
        parse_float(self.as_bytes(&mut scratch))
    }

    /// How this string numifies: integer, unsigned, or float, per §2.2.2's classification.
    pub fn numify(&self) -> Numeric {
        let mut scratch = [0u8; DECODE_MAX];
        classify_numeric(self.as_bytes(&mut scratch))
    }

    /// Whether numifying this string would emit perl's `Argument isn't numeric` warning (§2.3.4).  A question about the
    /// content; whether the warning has *already* fired is [`PerlString::is_warned`].
    pub fn would_warn(&self) -> bool {
        let mut scratch = [0u8; DECODE_MAX];
        string_would_warn(self.as_bytes(&mut scratch))
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

impl FromStr for PerlString {
    type Err = AllocError;

    /// The same construction as [`PerlString::new`], for generic contexts and `"...".parse()`.
    fn from_str(s: &str) -> Result<PerlString, AllocError> {
        PerlString::new(s)
    }
}

macro_rules! grid_hit {
    () => {
        #[cfg(test)]
        eq_probe::GRID_HITS.with(|c| c.set(c.get() + 1));
    };
}

impl PerlString {
    /// Perl's `cmp`: **code-point ordering**, which is what the utf8 flag selects between.
    ///
    /// The flag says how to read the bytes, so it decides the comparison shape.  When both sides agree, byte order *is*
    /// code-point order — unflagged octets are their own code points, and UTF-8 is order-preserving, so a straight byte
    /// comparison answers.  When they disagree, the flagged side's bytes decode to code points while the plain side's
    /// are code points, and the two are walked together.
    ///
    /// Container-verified: an unflagged `0xE9` sorts before a flagged `U+0100`, and `"\xC3\xA9"` sorts before `U+00E9`
    /// because its first octet reads as `U+00C3`.  Equal content compares equal across flags, agreeing with
    /// [`PartialEq`].
    ///
    /// `use bytes` selects no second comparison.  The utf8 flag is part of a string's value rather than an annotation
    /// on it, so ignoring the flag is the identity on an unflagged string and yields a *different value* for a flagged
    /// one — the same octets, read as Latin-1 characters.  The operands are projected and then compared by this
    /// ordering, like against like.
    pub fn cmp_perl(&self, other: &PerlString) -> Ordering {
        if self.is_utf8() == other.is_utf8() {
            return self.cmp_raw_bytes(other);
        }

        let (flagged, plain) = if self.is_utf8() { (self, other) } else { (other, self) };
        let ordering = cmp_cross_flag(flagged, plain);

        if self.is_utf8() { ordering } else { ordering.reverse() }
    }

    /// Compare the raw bytes, which is what [`PerlString::cmp_perl`] reduces to when both sides read theirs the same
    /// way — unflagged octets being their own code points, and UTF-8 being order-preserving.
    ///
    /// Two packed strings of one alphabet compare as their nibbles do, the values being assigned in ASCII order, so
    /// neither side decodes.
    ///
    /// Private, and an optimization rather than a second ordering: there is one ordering on these values, and this is
    /// how it computes when neither side needs decoding.  It is *not* the `use bytes` comparison — that pragma changes
    /// which value is being compared, not how — and applied to operands whose flags differ it would report unlike
    /// things equal, which is why it cannot be the `Ord` impl.
    fn cmp_raw_bytes(&self, other: &PerlString) -> Ordering {
        match (self.raw_parts(), other.raw_parts()) {
            (RawParts::Packed(a), RawParts::Packed(b)) if a.alphabet == b.alphabet => a.cmp_same_alphabet(&b),
            (RawParts::Packed(a), _) => a.cmp_bytes(other.as_bytes(&mut [0u8; DECODE_MAX])),
            (_, RawParts::Packed(b)) => b.cmp_bytes(self.as_bytes(&mut [0u8; DECODE_MAX])).reverse(),
            _ => {
                let (mut ls, mut rs) = ([0u8; DECODE_MAX], [0u8; DECODE_MAX]);
                self.as_bytes(&mut ls).cmp(other.as_bytes(&mut rs))
            }
        }
    }
}

/// Compare a flagged string against an unflagged one: perl upgrades the unflagged side and compares the encodings byte
/// for byte (`sv_cmp`), so the flagged side's bytes pass verbatim — malformed content included, where reading the bytes
/// as code points would invent an ordering perl does not use.  The unflagged side upgrades virtually: each octet below
/// 0x80 presents itself; each octet above presents its lead and its continuation in turn, no buffer needed.
///
/// Container-pinned: flag-off `FF` against flagged `E9` is `C3.BF` against `E9` — less — and flag-off `E9` against
/// flagged `E9` is `C3.A9` against `E9`: unequal, the monster's cousin, identical payload bytes under different flags
/// being different strings.
fn cmp_cross_flag(flagged: &PerlString, plain: &PerlString) -> Ordering {
    let (mut fs, mut ps) = ([0u8; DECODE_MAX], [0u8; DECODE_MAX]);
    let fb = flagged.as_bytes(&mut fs);
    let pb = plain.as_bytes(&mut ps);

    let mut i = 0;
    for &b in pb {
        let (lead, cont) = if b < 0x80 { (b, None) } else { (0xC0 | (b >> 6), Some(0x80 | (b & 0x3F))) };
        for expected in std::iter::once(lead).chain(cont) {
            let Some(&f) = fb.get(i) else {
                return Ordering::Less; // The flagged side is a strict prefix of the upgrade: the lesser.
            };
            match f.cmp(&expected) {
                Ordering::Equal => i += 1,
                ordering => return ordering,
            }
        }
    }

    if i < fb.len() { Ordering::Greater } else { Ordering::Equal }
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

            // Same flag, both inline: representation equality is exact — canonical selection guarantees equal content
            // takes equal class, family, and payload bytes (§2.2.9), and the padding is canonical.  One discriminant
            // compare and one fifteen-byte memcmp, where expanding both sides costs decode work.
            if let (RawParts::Inline { class: ca, full: fa, buf: ba }, RawParts::Inline { class: cb, full: fb, buf: bb }) =
                (self.raw_parts(), other.raw_parts())
            {
                grid_hit!();
                return ca == cb && fa == fb && ba == bb;
            }

            // Same interpretation: byte equality is character equality (length check is memcmp's first move).
            //
            // Two packed strings of one alphabet compare as their nibbles do, with no decoding at all: the encoding is
            // injective and the padding canonical, so equal content is equal bytes.  Different alphabets cannot hold
            // equal content — classification is deterministic, so content picks exactly one — which makes the mismatch
            // a decisive answer rather than a reason to decode.
            if let (RawParts::Packed(a), RawParts::Packed(b)) = (self.raw_parts(), other.raw_parts()) {
                return a.alphabet == b.alphabet && a.full == b.full && a.nibbles == b.nibbles;
            }

            // One side packed: compare against the other's bytes without materialising this side's.
            if let RawParts::Packed(p) = self.raw_parts() {
                let mut rs = [0u8; DECODE_MAX];
                return p.eq_bytes(other.as_bytes(&mut rs));
            }

            if let RawParts::Packed(p) = other.raw_parts() {
                let mut ls = [0u8; DECODE_MAX];
                return p.eq_bytes(self.as_bytes(&mut ls));
            }

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

impl Ord for PerlString {
    /// Perl's `cmp`, which is the only ordering consistent with [`PartialEq`] and so the only one this trait can carry.
    ///
    /// A raw byte comparison is deliberately *not* this: two strings can share their internal bytes and still differ —
    /// an unflagged `"\xC3\xA9"` is two Latin-1 characters where a flagged one is `U+00E9` — so it would report them
    /// equal where equality reports them unequal, and `Ord` requires the two to agree.
    fn cmp(&self, other: &PerlString) -> Ordering {
        self.cmp_perl(other)
    }
}

impl PartialOrd for PerlString {
    fn partial_cmp(&self, other: &PerlString) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

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
                            // Bytes: characters are undefined; the raw digest is the value's.  Finish the fetch
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
            .field("storage", &self.storage_type())
            .field("len", &self.len())
            .field("utf8", &self.is_utf8())
            .field("warned", &self.is_warned())
            .field("tainted", &self.is_tainted())
            .field("bytes", &ByteLiteral(self.as_bytes(&mut [0u8; DECODE_MAX])))
            .finish()
    }
}

// ═══ The packed nibble tier ═════════════════════════════════════════════════════════════════════════════════

// Nibble-packed digit-dense strings (§2.2.9): two characters per byte over 16-symbol alphabets.
//
// Strings drawn from a 16-symbol alphabet pack two characters per byte, raising the inline capacity for the digit-dense
// class — timestamps, IPs, numeric IDs, and every default numeric stringification — to `MAX_PACKED_LEN` (30) characters
// inside the 16-byte envelope.  Three alphabets are defined, selected by the enclosing discriminant:
//
// - **Numeric**: space, `+`, `-`, `.`, `0`-`9`, `E`, `e` — every `%.15g` output and every `i64` stringification, in
//   either exponent spelling.
// - **DateTimePlus**: space, `+`, `-`, `.`, `0`-`9`, `:`, `T` — ISO timestamps in every form but Zulu.
// - **DateTimeZulu**: space, `-`, `.`, `0`-`9`, `:`, `T`, `Z` — Zulu-form ISO timestamps.
//
// A valid timestamp never needs `Z` and `+` together — Zulu *is* the zero offset — so splitting the two spellings
// covers the whole ISO grammar without a seventeenth symbol.  The union of all three is nineteen symbols against
// sixteen nibble values, so three alphabets are forced, and content that migrates between them transcodes
// ([`Packed::transcode`]).
//
// The order above is the classification priority, and it is chosen for the append path.  `Numeric` is a subset of
// `DateTimePlus` on nibbles 0-13, so a string that starts numeric and meets a `:` or `T` is *reclassified* with no
// rewriting at all — and lands on the canonical alphabet, because `DateTimePlus` is where timestamps belong unless a
// `Z` forces otherwise.  All three alphabets hold sixteen symbols, so none is wider than another; they differ in which
// sixteen.  `Z` is the one symbol no other alphabet holds, so `DateTimeZulu` is reached only through it, which makes
// the variant itself a proof that the timestamp's offset is `+00:00`.
//
// # The length lives in the last nibble
//
// Each alphabet has **two length families**, again carried by the discriminant.  Content of exactly `MAX_PACKED_LEN`
// characters fills all thirty nibbles and needs no stored length — the family says so.  Content of `MIN_PACKED_LEN`-29
// characters stores the low four bits of its length in nibble 29, the one a thirtieth character would have used, and
// recovers it as `0x10 | nibble` because the band's floor is sixteen.  Reading a length is one byte load, an `AND`, and
// an `OR` — no scan, and no dependence on content.
//
// Storing the length explicitly is what makes **trailing spaces representable**.  With the length implied by the last
// nonzero nibble, a string ending in a space could not be told from one padded with zeros, so such strings were
// unpackable — a restriction that looked harmless for whole strings but blocks incremental building, where a string
// passes through a trailing space on its way to something longer.
//
// Nibble values are assigned in ASCII order, so for two packed strings **of the same alphabet and the same length
// **family**, comparing the nibble arrays as plain bytes gives exactly the raw strings' byte order: content differences
// decide before nibble 29 is reached, and where one string ends the other has a symbol above the zero padding.
// Comparing across length families compares the twenty-nine shared nibbles and then the lengths, since the last nibble
// means different things on the two sides.  Comparing across alphabets decodes.
//
// # Invariants
//
// - **Padding is zero.**  Nibbles from the content end through nibble 28 are zero.  Nothing reads them to derive a
//   length any more, so a violation no longer announces itself — it silently corrupts ordering, equality, and hashing,
//   all of which read the whole payload.  Every construction path zeroes by building from a zeroed array; any future
//   mutation that shortens content must re-zero what it vacates.  [`Packed::padding_is_canonical`] states the property
//   and the debug assertions check it.
// - **Packing is an encoding, never a canonicalization.**  `unpack(pack(s)) == s` exactly, for every accepted input.
// - **Classification is deterministic**: the alphabets are tried in the fixed priority order Numeric, DateTimePlus,
//   DateTimeZulu, so equal byte contents always take equal representations — the prerequisite for representation-level
//   equality.

/// The packed-tier capacity in characters: 15 nibble bytes, two characters each.
const MAX_PACKED_LEN: usize = 30;

/// The shortest content this tier holds.  Content of 15 characters or fewer takes an inline form instead (§2.2.9), so
/// the packed forms hold exactly 16-30 characters.  The band is established by the tier selector, the only path that
/// constructs strings; `pack` states it as a precondition rather than checking it.  It is also what lets the stored
/// length occupy four bits: only the low nibble varies across 16-29.
const MIN_PACKED_LEN: usize = 16;

/// The nibble-array width in bytes.
const PACKED_BYTES: usize = MAX_PACKED_LEN / 2;

/// The nibble index holding the stored length, for content shorter than the capacity.
const LENGTH_NIBBLE: usize = MAX_PACKED_LEN - 1;

/// Which 16-symbol alphabet a packed string uses.  In `PerlString` this is not stored: it is folded into the tag, so
/// each alphabet has its own variants and the payload is fifteen nibble bytes with nothing else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PackedAlphabet {
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
struct Packed {
    alphabet: PackedAlphabet,

    /// The `MAX_PACKED_LEN`-character family: every nibble is content and the length is implied.
    full: bool,
    nibbles: [u8; PACKED_BYTES],
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
fn pack(bytes: &[u8]) -> Option<Packed> {
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
fn pack_in(bytes: &[u8], alphabet: PackedAlphabet) -> Option<Packed> {
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
    fn len(&self) -> usize {
        if self.full { MAX_PACKED_LEN } else { MIN_PACKED_LEN | nibble_at(&self.nibbles, LENGTH_NIBBLE) as usize }
    }

    /// Whether the nibbles between the content end and the length field are zero.  Nothing derives a length from them
    /// any more, so a violation would not announce itself: ordering, equality, and hashing all read the whole payload,
    /// and equal content must have equal representation.
    fn padding_is_canonical(&self) -> bool {
        if self.full {
            return true; // Every nibble is content.
        }

        (self.len()..LENGTH_NIBBLE).all(|i| nibble_at(&self.nibbles, i) == 0)
    }

    /// Decode to raw bytes: the exact original, by the round-trip invariant.
    fn unpack(&self) -> ([u8; MAX_PACKED_LEN], usize) {
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
    fn transcode(&self, to: PackedAlphabet) -> Option<Packed> {
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
    fn push(&self, tail: &[u8]) -> Option<Packed> {
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
    fn cmp_same_alphabet(&self, other: &Packed) -> Ordering {
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
    fn eq_bytes(&self, other: &[u8]) -> bool {
        if self.len() != other.len() {
            return false;
        }

        let table = self.alphabet.decode_table();

        other.iter().enumerate().all(|(i, &o)| table[nibble_at(&self.nibbles, i) as usize] == o)
    }

    /// Ordering against a raw byte string: decoded characters decide, then length breaks a prefix tie.
    fn cmp_bytes(&self, other: &[u8]) -> Ordering {
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
#[path = "tests/string_tests.rs"]
mod tests;
