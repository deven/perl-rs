//! `PString` — a Perl string: octet sequence + per-string state (§2.2.3).
//!
//! Two storage kinds and three per-value state dimensions fold into the enum discriminant:
//!
//! - **Storage**: an envelope-resident form (inline, packed — no heap allocation) or a pointer-backed one (the tiered
//!   refcounted buffers, the immortal images, and the §2.2.15 views).
//! - **The Perl utf8 flag**: a per-SV *semantic claim* ("interpret these bytes as characters"), not a validity fact.
//!   It can be set on bytes Rust rejects (perl-extended UTF-8; verified `chr(0x110000)`); no code path may derive
//!   `from_utf8_unchecked` from it.  Rust-level validity comes from the scan cache only.
//! - **Warned**: not a flag here — warn-once suppression rides the cached numeric face (§2.3.4), and the tag is storage
//!   times utf8 times tainted only (§2.2.3).
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

use crate::cow_buffer;
use crate::cow_buffer::{AllocError, HeapParts, HeapView, Owned, Tier};
use crate::value::{Numeric, classify_numeric, classify_numeric_noting_warning, parse_float, parse_int_i64_visible};
use std::cmp::Ordering;
use std::fmt;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::mem;
use std::ops::ControlFlow;
use std::str::{self, FromStr};

/// Maximum inline payload: chosen so every numeric stringification stays allocation-free (§2.2.3).
pub const INLINE_MAX: usize = 15;

/// The u24 whole-object sentinel (§2.2.15): both fields at this value mark an adopted whole-object handle, whose length
/// reads from the `Adopted` struct's cache line instead — so adoption is never capped by the 24-bit fields, which
/// describe only genuine sub-views.
pub(crate) const SPAN: u32 = 0xFF_FFFF;

/// The 24-bit view fields (§2.2.15): little-endian in three bytes, the immortal envelope's geometry.
#[inline]
const fn u24(bytes: [u8; 3]) -> usize {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]) as usize
}

/// The inverse, for values already proven under the bound.
#[inline]
#[cfg_attr(not(test), allow(dead_code))]
const fn to_u24(value: usize) -> [u8; 3] {
    debug_assert!(value <= SPAN as usize);
    let b = (value as u32).to_le_bytes();
    [b[0], b[1], b[2]]
}

/// The 16-bit view fields of the small-tier form (§2.2.15): little-endian in two bytes.
#[inline]
const fn u16v(bytes: [u8; 2]) -> usize {
    u16::from_le_bytes(bytes) as usize
}

/// The inverse, for values already proven under the bound.
#[inline]
const fn to_u16(value: usize) -> [u8; 2] {
    debug_assert!(value <= u16::MAX as usize);
    (value as u16).to_le_bytes()
}

/// The 32-bit offset of the far forms (§2.2.15): little-endian in four bytes.
#[inline]
const fn u32v(bytes: [u8; 4]) -> usize {
    u32::from_le_bytes(bytes) as usize
}

/// The inverse, for values already proven under the bound.
#[inline]
const fn to_u32(value: usize) -> [u8; 4] {
    debug_assert!(value <= u32::MAX as usize);
    (value as u32).to_le_bytes()
}

/// Which backing a view in flight owes its release to (§2.2.15): the small form carries the capacity its tiers' release
/// demands, and the capacity is also the dispatch — the allocation ladder is strict, so Heap8 capacities sit at or
/// below 255 and Heap16's above, with no overlap.
#[derive(Clone, Copy)]
enum ViewBacking {
    Heap32Medium,
    Heap32Far,
    Small { cap: usize },
    Adopted,
    AdoptedFar,
}

/// # Safety
/// `ptr` must own a live small-tier allocation of the tier the strict ladder assigns `cap`.
#[inline]
unsafe fn small_backing_retain(ptr: std::ptr::NonNull<u8>, cap: usize) {
    debug_assert!(cap <= cow_buffer::heap16::MAX_CAPACITY, "the small form serves the small tiers only");

    // SAFETY: the caller vouches for a live allocation; the ladder's strictness makes cap the tier.
    unsafe {
        if cap <= cow_buffer::heap8::MAX_CAPACITY {
            cow_buffer::heap8::retain(ptr);
        } else {
            cow_buffer::heap16::retain(ptr);
        }
    }
}

/// # Safety
/// As `small_backing_retain`, and the caller surrenders one reference.
#[inline]
unsafe fn small_backing_release(ptr: std::ptr::NonNull<u8>, cap: usize) {
    // SAFETY: the caller vouches; the ladder's strictness makes cap the tier, and each release takes the capacity at
    // its own width.
    unsafe {
        if cap <= cow_buffer::heap8::MAX_CAPACITY {
            cow_buffer::heap8::release(ptr, cap as u8);
        } else {
            cow_buffer::heap16::release(ptr, cap as u16);
        }
    }
}

/// The widest byte sequence any non-heap form decodes to, and so the size of the scratch buffer the borrowed-view
/// accessors take.
///
/// It is `INLINE_MAX * 2`, and not by coincidence: **every non-heap form is a 2:1 compression of its decoded bytes**.
/// The packed forms reach that ratio by storing two symbols per byte, four bits each; the Latin-1 form reaches it by
/// declining to spend two bytes on what Latin-1 writes in one, since `U+0080`-`U+00FF` sits inside UTF-8's two-byte
/// range.  The same factor, arrived at from opposite directions — one packing two units into a byte, the other refusing
/// to let one unit take two.  Raw forms compress not at all and are trivially under the bound.
///
/// So today's value is correct by construction rather than by measuring the cases.  The definition below is the
/// **maximum over the envelope families' decode ceilings** rather than a hardcoded factor, deliberately: a future
/// family may compress beyond 2:1 — a format whose fixed punctuation is implied by position decodes more characters
/// than it stores nibbles — and defining the maximum as a maximum means such a family raises this constant and every
/// scratch sized by it in the same edit that adds its ceiling to the list, with no silent shortfall possible.
///
/// Read from the producer side, the same number is the **envelope representability ceiling**: the maximum logical byte
/// length any envelope-resident form can possibly represent, and so the bound past which the envelope ladder is not
/// worth attempting.  Representability inside the ceiling stays conditional — only lengths up to `INLINE_MAX` are
/// unconditional, and longer content needs a compressed form to admit it — so the constant bounds where the ladder may
/// succeed, never what it yields.
pub const DECODE_MAX: usize = {
    let nibble = if 2 * INLINE_MAX > MAX_PACKED_LEN { 2 * INLINE_MAX } else { MAX_PACKED_LEN };
    let identifiers = if UUID_LEN > HEX_MAX_LEN { UUID_LEN } else { HEX_MAX_LEN };
    if identifiers > nibble { identifiers } else { nibble }
};

/// The heap scan lattice (§2.2.4): terminal states live typed in the small tiers' envelopes, and the large tiers keep
/// an atomic byte in the allocation header.  Zero is `UNKNOWN`, the lattice top — the natural zero-initialized state
/// can never assert a validity claim (§2.2.6).
pub mod scan {
    /// The scan lattice as a closed type, and the numbering's single home: the variants carry the discriminants, the
    /// related enums source theirs symbolically from the variant names, and a private projection module derives
    /// pattern-position constants — no numeric fact is stated twice (§2.2.4).  Every value a scan byte can legally hold
    /// is a variant, so "the byte is a valid state" stops being a convention the writers maintain and becomes a fact
    /// the loader establishes once: [`ScanState::from_u8`] is the single place a raw byte re-enters the type, and it is
    /// of the bomb's family.  The variants are re-exported, so `scan::Unknown` is the working vocabulary — true variant
    /// paths, not aliases.
    ///
    /// Each state is an assertion set (§2.2.4).  The two `Maybe` states — the strong assertion minus the witness a
    /// subrange may exclude — and `PerlValidNonAscii` are currently unreachable, seated by ruling: slicing is certain
    /// to come, their births arrive with it, and the meet is total only with all twelve seated.
    #[repr(u8)]
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum ScanState {
        /// Asserts nothing.  Zero-pinned (§2.2.6): fresh headers can never assert a claim.
        Unknown = 0,

        /// All bytes `0x00`–`0x7F`.  Terminal.
        Ascii = 1,

        /// Rust-valid, all code points ≤ U+00FF, at least one ≥ U+0080.  Terminal.  Can equal an unflagged string.
        Utf8Latin1 = 2,

        /// Rust-valid, at least one code point ≥ U+0100.  Terminal.  Cannot equal an unflagged string.
        Utf8NonLatin1 = 3,

        /// Rust-valid; nothing further known.  Narrows to the range terminals, or to `Utf8NonAscii` via the cheap
        /// high-bit probe.
        ValidUtf8 = 4,

        /// Rust-valid, at least one code point ≥ U+0080; Latin-1-range unresolved.  The cheap `is_ascii` probe lands
        /// here from `ValidUtf8` without paying the full-range lead-byte pass (§2.2.4).
        Utf8NonAscii = 5,

        /// Perl-decodable, Rust-invalid: contains a code point Rust rejects (a surrogate or ≥ U+110000, each ≥ U+0100 —
        /// so the beyond-Latin-1 range fact is derivable, and the range predicates lean on it).  Terminal.  Cannot
        /// equal an unflagged string.
        ExtendedUtf8 = 6,

        /// Violates the encoding patterns; invalid for Rust and perl both (§2.2.4).  Terminal.  Cannot equal an
        /// unflagged string.
        MalformedUtf8 = 7,

        /// A high bit is present; validity and range unknown.
        NonAscii = 8,

        /// Rust-valid, all code points ≤ U+00FF; the non-Ascii witness is not asserted.  Narrows to `Ascii` or
        /// `Utf8Latin1` — and a failed Ascii probe alone completes it to `Utf8Latin1`, both witnesses then in hand.
        MaybeUtf8Latin1 = 9,

        /// Perl-decodable; Rust validity and range unasserted.  `is_perl_decodable` answers with no scan; a full
        /// classification of content honestly in this state can never land on `MalformedUtf8` — that outcome would
        /// falsify the assertion, and debug builds treat the contradiction as the bomb family does.
        MaybeExtendedUtf8 = 10,

        /// Perl-decodable with a high-bit byte present — under perl validity that byte's sequence decodes at or above
        /// U+0080, so this is `Utf8NonAscii`'s perl-layer analog, with Rust validity and range unasserted.  The meet's
        /// home for a probe's byte fact landing on `MaybeExtendedUtf8` content, seated so that no union of true
        /// certifications forfeits anything; a full classification can land on any perl-valid non-`Ascii` terminal,
        /// with `MalformedUtf8` and `Ascii` both falsifying and bomb-family in debug.
        PerlValidNonAscii = 11,
    }

    pub use ScanState::*;

    /// Pattern-position projections of the variants: a cast is not a pattern, and a `u8` scrutinee cannot match an enum
    /// path, so these one-line derivations exist solely for `match` arms over raw bytes.  The variants remain the
    /// numbering's only home — nothing here states a number.
    mod raw {
        use super::ScanState;

        pub const UNKNOWN: u8 = ScanState::Unknown as u8;
        pub const ASCII: u8 = ScanState::Ascii as u8;
        pub const UTF8_LATIN1: u8 = ScanState::Utf8Latin1 as u8;
        pub const UTF8_NON_LATIN1: u8 = ScanState::Utf8NonLatin1 as u8;
        pub const VALID_UTF8: u8 = ScanState::ValidUtf8 as u8;
        pub const UTF8_NON_ASCII: u8 = ScanState::Utf8NonAscii as u8;
        pub const EXTENDED_UTF8: u8 = ScanState::ExtendedUtf8 as u8;
        pub const MALFORMED_UTF8: u8 = ScanState::MalformedUtf8 as u8;
        pub const NON_ASCII: u8 = ScanState::NonAscii as u8;
        pub const MAYBE_UTF8_LATIN1: u8 = ScanState::MaybeUtf8Latin1 as u8;
        pub const MAYBE_EXTENDED_UTF8: u8 = ScanState::MaybeExtendedUtf8 as u8;
        pub const PERL_VALID_NON_ASCII: u8 = ScanState::PerlValidNonAscii as u8;
    }

    impl ScanState {
        /// Project to the storage byte, for the atomic scan slots the large tiers keep in their allocations.
        #[inline]
        pub const fn as_u8(self) -> u8 {
            self as u8
        }

        /// The single seam where a storage byte re-enters the type.  Only this crate writes scan bytes and only through
        /// [`ScanState::as_u8`], so anything else is corruption; of the bomb's family, this reports at the site rather
        /// than laundering a garbage byte into a legal-looking state.
        pub fn from_u8(byte: u8) -> ScanState {
            match byte {
                raw::UNKNOWN => Unknown,
                raw::ASCII => Ascii,
                raw::UTF8_LATIN1 => Utf8Latin1,
                raw::UTF8_NON_LATIN1 => Utf8NonLatin1,
                raw::VALID_UTF8 => ValidUtf8,
                raw::UTF8_NON_ASCII => Utf8NonAscii,
                raw::EXTENDED_UTF8 => ExtendedUtf8,
                raw::MALFORMED_UTF8 => MalformedUtf8,
                raw::NON_ASCII => NonAscii,
                raw::MAYBE_UTF8_LATIN1 => MaybeUtf8Latin1,
                raw::MAYBE_EXTENDED_UTF8 => MaybeExtendedUtf8,
                raw::PERL_VALID_NON_ASCII => PerlValidNonAscii,
                other => panic!("scan byte {other} is not a state: header corruption or a write that bypassed the type"),
            }
        }

        /// The terminal subset, where this state is in it.  The non-terminal arms are named rather than wildcarded: a
        /// state added to the lattice must land here by decision, not by omission.
        #[inline]
        pub const fn terminal(self) -> Option<Terminal> {
            match self {
                Ascii => Some(Terminal::Ascii),
                Utf8Latin1 => Some(Terminal::Utf8Latin1),
                Utf8NonLatin1 => Some(Terminal::Utf8NonLatin1),
                ExtendedUtf8 => Some(Terminal::ExtendedUtf8),
                MalformedUtf8 => Some(Terminal::MalformedUtf8),
                Unknown | ValidUtf8 | Utf8NonAscii | NonAscii | MaybeUtf8Latin1 | MaybeExtendedUtf8 | PerlValidNonAscii => None,
            }
        }
    }

    /// The valid-range chain as a closed type: the three classes known-valid content can classify to, which are the
    /// only classes `classify_known_valid` can produce and the only ones an `AppendKind::Valid` may carry.  The chain
    /// is totally ordered (Ascii < Latin-1 < non-Latin-1) with discriminants sourced from the lattice, which is what
    /// makes the append range join a max.
    #[repr(u8)]
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum ValidRange {
        Ascii = ScanState::Ascii as u8,
        Latin1 = ScanState::Utf8Latin1 as u8,
        NonLatin1 = ScanState::Utf8NonLatin1 as u8,
    }

    impl ValidRange {
        /// Into the full lattice.
        #[inline]
        pub const fn widen(self) -> ScanState {
            match self {
                ValidRange::Ascii => Ascii,
                ValidRange::Latin1 => Utf8Latin1,
                ValidRange::NonLatin1 => Utf8NonLatin1,
            }
        }

        /// The range join (§2.2.5): the result of concatenating content of these two classes.
        #[inline]
        pub const fn join(self, other: ValidRange) -> ValidRange {
            if (self as u8) >= (other as u8) { self } else { other }
        }
    }

    /// The five states a full classification can produce — the *terminal* subset of the lattice, as a closed type.
    ///
    /// The small tiers' envelopes hold this type rather than the raw `u8`, and that is the enforcement of §2.2.3's
    /// eager rule: with no allocation slot to record a later discovery, a small tier holding an indeterminate state
    /// would re-derive on every read forever, so the states that need narrowing are not merely rejected below 64 KiB —
    /// they are unrepresentable there.  Writing `Unknown` into a small envelope is a type error, which is how the
    /// defect this type answers was written twice (the in-place downgrade, and the raw-byte append transition) before
    /// the compiler was given the means to refuse it.
    ///
    /// Discriminants are sourced from the lattice's variants, so projection is a cast.
    #[repr(u8)]
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Terminal {
        Ascii = ScanState::Ascii as u8,
        Utf8Latin1 = ScanState::Utf8Latin1 as u8,
        Utf8NonLatin1 = ScanState::Utf8NonLatin1 as u8,
        ExtendedUtf8 = ScanState::ExtendedUtf8 as u8,
        MalformedUtf8 = ScanState::MalformedUtf8 as u8,
    }

    impl Terminal {
        /// Into the full lattice, for the readers, the transitions and the large tiers.
        #[inline]
        pub const fn widen(self) -> ScanState {
            match self {
                Terminal::Ascii => Ascii,
                Terminal::Utf8Latin1 => Utf8Latin1,
                Terminal::Utf8NonLatin1 => Utf8NonLatin1,
                Terminal::ExtendedUtf8 => ExtendedUtf8,
                Terminal::MalformedUtf8 => MalformedUtf8,
            }
        }

        /// The reverse projection, for the one seam where a state re-enters an envelope from tier-agnostic transport
        /// (`HeapParts`).  Every path feeding a small tier establishes a terminal state first, so the panic is of the
        /// bomb's family: unreachable in correct code, and reporting rather than misbehaving if a future path is not.
        pub fn from_scan(state: ScanState) -> Terminal {
            match state.terminal() {
                Some(terminal) => terminal,
                None => panic!("non-terminal scan state {state:?} reached a small tier"),
            }
        }
    }

    /// Rust-valid: the states whose assertion includes it (§2.2.4).
    #[inline]
    pub const fn is_rust_valid(state: ScanState) -> bool {
        matches!(state, Ascii | Utf8Latin1 | MaybeUtf8Latin1 | Utf8NonLatin1 | ValidUtf8 | Utf8NonAscii)
    }

    /// Perl-decodable: every Rust-valid state plus the extended forms perl accepts, asserted or possible-with-proof.
    #[inline]
    pub const fn is_perl_decodable(state: ScanState) -> bool {
        matches!(state, Ascii | Utf8Latin1 | MaybeUtf8Latin1 | Utf8NonLatin1 | ValidUtf8 | Utf8NonAscii | ExtendedUtf8 | MaybeExtendedUtf8 | PerlValidNonAscii)
    }

    /// Known entirely ≤ U+00FF (downgradable).  `MaybeUtf8Latin1` qualifies: the range bound is asserted even where the
    /// non-Ascii witness is not.
    #[inline]
    pub const fn is_known_latin1_range(state: ScanState) -> bool {
        matches!(state, Ascii | Utf8Latin1 | MaybeUtf8Latin1)
    }

    /// Fully-scanned terminal classification (§2.2.4): mutually exclusive byte-content classes.
    #[inline]
    pub const fn is_terminal(state: ScanState) -> bool {
        state.terminal().is_some()
    }

    /// Known non-ASCII (a high bit is known used).  The `Maybe` states do not assert their witnesses.
    #[inline]
    pub const fn is_known_non_ascii(state: ScanState) -> bool {
        !matches!(state, Unknown | Ascii | ValidUtf8 | MaybeUtf8Latin1 | MaybeExtendedUtf8)
    }

    /// Known to contain a character ≥ U+0100.
    #[inline]
    pub const fn is_known_beyond_latin1(state: ScanState) -> bool {
        matches!(state, Utf8NonLatin1 | ExtendedUtf8)
    }

    // ── The narrowing meet (§2.2.4) ────────────────────────────────
    // Each state is a set of certified facts about the bytes; two states stored by racing readers are both true of the
    // same immutable content, so their conjunction is true, and the meet is that union canonicalized to the most
    // precise representable state.  Every union of true certifications lands exactly: the lattice was extended by one
    // state (`PerlValidNonAscii`) precisely so that no combination forfeits a fact.

    /// Rust-valid (which implies perl-decodable).
    const RUST_VALID: u16 = 1 << 0;

    /// Perl-decodable.
    const PERL_VALID: u16 = 1 << 1;

    /// Every code point at most U+00FF.
    const ALL_LE_00FF: u16 = 1 << 2;

    /// A code point at or above U+0080 exists.
    const CONTAINS_GE_0080: u16 = 1 << 3;

    /// A code point at or above U+0100 exists.
    const CONTAINS_GE_0100: u16 = 1 << 4;

    /// A Rust-invalid form exists: a surrogate, or a code point at or above U+110000.
    const RUST_INVALID: u16 = 1 << 5;

    /// Invalid to perl (and so to Rust).
    const MALFORMED: u16 = 1 << 6;

    /// A byte with the high bit set exists (the byte-level fact a probe learns).
    const HIGH_BIT: u16 = 1 << 7;

    /// Every byte is ASCII.
    const ALL_ASCII: u16 = 1 << 8;

    /// The assertion set of a state — exactly the §2.2.4 definitions.
    fn facts(state: ScanState) -> u16 {
        match state {
            Unknown => 0,
            PerlValidNonAscii => PERL_VALID | HIGH_BIT,
            Ascii => RUST_VALID | PERL_VALID | ALL_LE_00FF | ALL_ASCII,
            Utf8Latin1 => RUST_VALID | PERL_VALID | ALL_LE_00FF | CONTAINS_GE_0080,
            MaybeUtf8Latin1 => RUST_VALID | PERL_VALID | ALL_LE_00FF,
            Utf8NonLatin1 => RUST_VALID | PERL_VALID | CONTAINS_GE_0100,
            ValidUtf8 => RUST_VALID | PERL_VALID,
            Utf8NonAscii => RUST_VALID | PERL_VALID | CONTAINS_GE_0080,
            ExtendedUtf8 => PERL_VALID | RUST_INVALID,
            MaybeExtendedUtf8 => PERL_VALID,
            MalformedUtf8 => MALFORMED,
            NonAscii => HIGH_BIT,
        }
    }

    /// The meet of two true certifications of the same bytes: the union of their facts, closed under derivation,
    /// canonicalized.  Monotonic — the result's facts contain each input's representable facts — commutative, and
    /// idempotent, with `Unknown` the identity.  A union asserting a contradiction (an all-ASCII certificate beside a
    /// high-bit witness, a validity claim beside malformedness) cannot arise from two truths; debug builds treat it as
    /// the bomb family does, and release canonicalization proceeds malformed-first.
    pub(crate) fn meet(a: ScanState, b: ScanState) -> ScanState {
        let mut f = facts(a) | facts(b);

        // Derivations: under Rust validity a high-bit byte is a code point at or above U+0080, and the stronger witness
        // implies the weaker.
        if f & RUST_VALID != 0 && f & HIGH_BIT != 0 {
            f |= CONTAINS_GE_0080;
        }
        if f & (CONTAINS_GE_0100 | RUST_INVALID) != 0 {
            f |= CONTAINS_GE_0080;
        }

        // Contradiction census, one clause per impossible pairing; a single expression would defeat the reading.
        let contradiction = (f & MALFORMED != 0 && f & PERL_VALID != 0)
            || (f & RUST_VALID != 0 && f & RUST_INVALID != 0)
            || (f & ALL_ASCII != 0 && (f & HIGH_BIT != 0 || f & CONTAINS_GE_0080 != 0))
            || (f & ALL_LE_00FF != 0 && f & CONTAINS_GE_0100 != 0);
        debug_assert!(!contradiction, "the meet of two true certifications asserted a contradiction: {a:?} with {b:?}");

        if f & MALFORMED != 0 {
            MalformedUtf8
        } else if f & RUST_INVALID != 0 {
            ExtendedUtf8
        } else if f & RUST_VALID != 0 {
            if f & ALL_ASCII != 0 {
                Ascii
            } else if f & CONTAINS_GE_0100 != 0 {
                Utf8NonLatin1
            } else if f & ALL_LE_00FF != 0 {
                if f & CONTAINS_GE_0080 != 0 { Utf8Latin1 } else { MaybeUtf8Latin1 }
            } else if f & CONTAINS_GE_0080 != 0 {
                Utf8NonAscii
            } else {
                ValidUtf8
            }
        } else if f & PERL_VALID != 0 {
            if f & HIGH_BIT != 0 { PerlValidNonAscii } else { MaybeExtendedUtf8 }
        } else if f & HIGH_BIT != 0 {
            NonAscii
        } else {
            Unknown
        }
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
fn classify_full(bytes: &[u8]) -> (scan::Terminal, usize) {
    classify_walk(bytes, &mut NullSink)
}

/// Classify `src` while copying it to `dst`: one traversal determines the terminal state and character count and emits
/// every byte, including on the malformed path — a perl string legitimately holds malformed content, so the copy always
/// completes even where classification stops.  The pair with [`classify_full`] is one generic walker monomorphized
/// twice; the null sink's stores are deleted by the compiler, verified against the pre-split assembly.
///
/// # Safety
/// `dst` must be valid for `src.len()` writes and must not overlap `src` — the in-place transforms, which do overlap,
/// have their facts known a priori and never come here.
unsafe fn classify_into(dst: *mut u8, src: &[u8]) -> (scan::Terminal, usize) {
    debug_assert!({
        let (d, s, n) = (dst as usize, src.as_ptr() as usize, src.len());
        d + n <= s || s + n <= d
    });
    classify_walk(src, &mut CopySink { dst })
}

/// Where a classification pass sends the bytes it has just read: nowhere, or to a destination buffer.  A zero-sized
/// null sink monomorphizes the walker back to the pure classifier — the emit calls vanish — so the non-copying path
/// pays nothing for the copying path's existence.
trait ScanSink {
    /// Emit `src[range]`, which the walker has just classified.  Ranges arrive in order and cover `src` exactly.
    fn emit(&mut self, src: &[u8], start: usize, end: usize);
}

struct NullSink;

impl ScanSink for NullSink {
    #[inline(always)]
    fn emit(&mut self, _src: &[u8], _start: usize, _end: usize) {}
}

struct CopySink {
    dst: *mut u8,
}

impl ScanSink for CopySink {
    #[inline(always)]
    fn emit(&mut self, src: &[u8], start: usize, end: usize) {
        // SAFETY: `classify_into`'s caller vouches for a non-overlapping destination valid for the whole source, and
        // the walker's ranges stay inside it.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr().add(start), self.dst.add(start), end - start) };
    }
}

fn classify_walk(bytes: &[u8], sink: &mut impl ScanSink) -> (scan::Terminal, usize) {
    count_full_scan();

    let mut facts = ScanFacts::default();
    let mut pos = 0usize;

    while pos < bytes.len() {
        let soft_end = block_end(pos, bytes.len());

        // Exitless SIMD gate over the block (a fold, not an early-exit scan — folds vectorize).
        let hi = bytes[pos..soft_end].iter().fold(0u8, |a, &b| a | b) & 0x80 != 0;
        if !hi {
            facts.chars += soft_end - pos; // ASCII block: characters are bytes; no further passes
            sink.emit(bytes, pos, soft_end);
            pos = soft_end;
            continue;
        }

        // Non-ASCII block: scalar fused decode over the cached bytes, running to at least soft_end and completing any
        // sequence that straddles it.
        match scalar_decode_span(bytes, pos, soft_end, &mut facts, |_| {}) {
            Some(next) => {
                sink.emit(bytes, pos, next);
                pos = next;
            }
            None => {
                // The copy completes even though classification stops: malformed content is still content.
                sink.emit(bytes, pos, bytes.len());
                return (scan::Terminal::MalformedUtf8, 0);
            }
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
    fn state(&self) -> scan::Terminal {
        if self.saw_rust_rejected {
            scan::Terminal::ExtendedUtf8
        } else if self.saw_beyond_latin1 {
            scan::Terminal::Utf8NonLatin1
        } else if self.saw_multibyte {
            scan::Terminal::Utf8Latin1
        } else {
            scan::Terminal::Ascii
        }
    }
}

/// The scalar fused extended decoder over `bytes[start..]`, decoding whole sequences until the position reaches
/// `soft_end` (a sequence beginning before `soft_end` completes past it; truncation is judged against the full slice).
/// Returns the position where decoding stopped, or `None` on malformed content.
fn scalar_decode_span(bytes: &[u8], start: usize, soft_end: usize, facts: &mut ScanFacts, emit: impl FnMut(u64)) -> Option<usize> {
    scalar_decode_span_reporting(bytes, start, soft_end, facts, emit, |_| ControlFlow::Break(()))
}

/// Decode one sequence at `bytes[at]`, returning its length and code point.
///
/// `None` covers every way a sequence can be rejected — a bare continuation byte, truncation against the full slice, a
/// non-continuation where a continuation belongs, an accumulator that would overflow, and a value either overlong for
/// its form or beyond `IV_MAX` — because the caller treats them alike.  Which one it was changes nothing downstream:
/// the byte at `at` is not the start of a decodable character either way.
#[inline(always)]
fn decode_one(bytes: &[u8], at: usize) -> Option<(usize, u64)> {
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

    let lead = bytes[at];

    let (len, mut value): (usize, u64) = match lead {
        0x00..=0x7F => return Some((1, lead as u64)),
        0xC0..=0xDF => (2, (lead & 0x1F) as u64),
        0xE0..=0xEF => (3, (lead & 0x0F) as u64),
        0xF0..=0xF7 => (4, (lead & 0x07) as u64),
        0xF8..=0xFB => (5, (lead & 0x03) as u64),
        0xFC..=0xFD => (6, (lead & 0x01) as u64),
        0xFE => (7, 0),
        0xFF => (13, 0),
        _ => return None, // bare continuation byte
    };

    if at + len > bytes.len() {
        return None; // truncated (judged against the full slice, not the block)
    }

    for &b in &bytes[at + 1..at + len] {
        if !is_continuation(b) {
            return None; // malformed continuation
        }

        // 12 continuations x 6 bits = 72 bits could overflow u64, but any value needing the high bits exceeds IV_MAX
        // and is rejected; checked arithmetic keeps the reasoning airtight.
        value = value.checked_mul(64)? | (b & 0x3F) as u64;
    }

    if value < min_for_len(len) || value > 0x7FFF_FFFF_FFFF_FFFF {
        return None; // overlong for its form, or beyond IV_MAX
    }

    Some((len, value))
}

/// A continuation byte carries `10` in its top two bits, and can therefore never begin a sequence.
const fn is_continuation(byte: u8) -> bool {
    byte & 0xC0 == 0x80
}

/// The span of bytes a rejected sequence covers: the byte at `at` together with every continuation byte that follows
/// it, running to the end of the slice rather than to a block boundary because truncation is judged the same way.
///
/// One rule serves both shapes the rejection can take.  Where `at` holds a lead byte the span is that lead and the
/// continuations belonging to it; where `at` holds a stray continuation the span is the maximal run of them.  Nothing
/// decodable is ever swallowed, since a continuation byte cannot begin a sequence.
fn malformed_run(bytes: &[u8], at: usize) -> usize {
    let mut end = at + 1;
    while end < bytes.len() && is_continuation(bytes[end]) {
        end += 1;
    }

    end - at
}

/// The decoder above, with malformed spans reported rather than merely fatal.
///
/// `on_malformed` receives each rejected span (see [`malformed_run`]) and decides whether the walk goes on.  Breaking
/// reproduces the classifying behavior exactly — the first rejection ends the walk and the span position is discarded —
/// while continuing lets a caller render or count the whole string, malformed regions included.
///
/// `facts` is untouched across a rejected span.  It accumulates classification, and content holding such a span is
/// already `MalformedUtf8` regardless of what surrounds it; a caller that walks on is rendering or counting, not
/// classifying, and reads the span from the closure instead.
fn scalar_decode_span_reporting(
    bytes: &[u8],
    start: usize,
    soft_end: usize,
    facts: &mut ScanFacts,
    mut emit: impl FnMut(u64),
    mut on_malformed: impl FnMut(&[u8]) -> ControlFlow<()>,
) -> Option<usize> {
    let mut i = start;
    while i < soft_end {
        let Some((len, value)) = decode_one(bytes, i) else {
            let run = malformed_run(bytes, i);
            if on_malformed(&bytes[i..i + run]).is_break() {
                return None;
            }

            i += run;
            continue;
        };

        // A single byte is ASCII by construction, so none of the multibyte facts can move.
        if len > 1 {
            facts.saw_multibyte = true;
            facts.saw_beyond_latin1 |= value > 0xFF;
            facts.saw_rust_rejected |= len > 4 || value > 0x10_FFFF || (0xD800..=0xDFFF).contains(&value);
        }

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
fn classify_known_valid(bytes: &[u8]) -> (scan::ValidRange, usize) {
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
            return (scan::ValidRange::NonLatin1, 0); // answer determined; the block-granular bail forfeits the count
        }

        saw_high = true;
        chars += block.iter().map(|&b| usize::from(b & 0xC0 != 0x80)).sum::<usize>();
    }

    (if saw_high { scan::ValidRange::Latin1 } else { scan::ValidRange::Ascii }, chars)
}

/// The copying twin of [`classify_known_valid`]: one traversal ranges, counts, and emits every byte.  Standalone rather
/// than sink-generic because the control flow genuinely diverges — the pure walker bails once `NonLatin1` is
/// determined, forfeiting the count, but a copy cannot stop, and since the remaining blocks stream through anyway,
/// counting them is free: this twin returns a real count where the pure one forfeits.
///
/// # Safety
/// `dst` must be valid for `src.len()` writes and must not overlap `src`.
unsafe fn classify_known_valid_into(dst: *mut u8, src: &[u8]) -> (scan::ValidRange, usize) {
    count_full_scan();
    debug_assert!({
        let (d, s, n) = (dst as usize, src.as_ptr() as usize, src.len());
        d + n <= s || s + n <= d
    });

    let mut range = scan::ValidRange::Ascii;
    let mut chars = 0usize;
    let mut pos = 0usize;

    while pos < src.len() {
        let end = block_end(pos, src.len());
        let block = &src[pos..end];

        // SAFETY: the caller vouches for a non-overlapping destination valid for the whole source.
        unsafe { std::ptr::copy_nonoverlapping(block.as_ptr(), dst.add(pos), block.len()) };
        pos = end;

        let hi = block.iter().fold(0u8, |a, &b| a | b) & 0x80 != 0;
        if !hi {
            chars += block.len();
            continue;
        }
        if range != scan::ValidRange::NonLatin1 && block.iter().fold(0u8, |a, &b| a | u8::from(b >= 0xC4)) != 0 {
            range = scan::ValidRange::NonLatin1;
        }
        if range != scan::ValidRange::NonLatin1 {
            range = scan::ValidRange::Latin1;
        }
        chars += block.iter().map(|&b| usize::from(b & 0xC0 != 0x80)).sum::<usize>();
    }

    (range, chars)
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

/// The storage type: the forty-seven-value normative vocabulary (§2.2.9), one value per base variant of the folded tag
/// — the discriminant is this type times the two flag bits, utf8 and tainted, which is the whole tag ledger:
/// forty-seven times four is the 188 the §2.2.3 arithmetic states, pinned by `REPR_VARIANT_COUNT`.  Coarse questions
/// are the projection methods.  Declaration order is itself the selection (§2.2.9): canonical selection takes the first
/// type, in this order, able to represent the content — first-fit is the ladder — which is what the derived `Ord`
/// means.
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

    /// Heap, content of 255 bytes or fewer, ASCII: the twin stands before its tier — more specific first, per the
    /// ladder — with count and scan omitted from the envelope, both derivable from the variant itself (§2.2.3).
    Heap8Ascii,

    /// Heap, content of 255 bytes or fewer: a two-byte allocation header, everything else in the envelope
    /// (§2.2.3).
    Heap8,

    /// Heap, content through 64 KiB, ASCII: the twin at `u16` widths.
    Heap16Ascii,

    /// Heap, content through 64 KiB: the same shape at `u16` widths.
    Heap16,

    /// Heap, content through 4 GiB: metadata in the allocation, since at this size it is filled lazily and shared.
    Heap32,

    /// Heap, content beyond 4 GiB.  Unreachable where pointers are 32 bits, and compiled there anyway so that
    /// discriminants match across targets.
    Heap,

    /// An immortal image (§2.2.3): interpreter- or arena-lifetime bytes, envelope facts settled at construction, never
    /// freed by teardown.  Constructed explicitly, never canonically selected.
    Immortal,

    /// A `'static` image (§2.2.3): the program's bytes, alive forever.  Constructed explicitly, never canonically
    /// selected.
    Static,

    /// An immortal image past the compact ceiling (§2.2.3): the envelope points at a shared, leaked side header.
    LargeImmortal,

    /// A `'static` image past the compact ceiling (§2.2.3).
    LargeStatic,

    /// A native view (§2.2.15): a sub-range of a Heap32 buffer, sharing its header refcount.
    MediumSlice,

    /// A small-tier view (§2.2.15): a sub-range of a Heap8 or Heap16 buffer, the envelope carrying the capacity their
    /// release demands, which is also the release dispatch under the strict ladder.
    SmallSlice,

    /// A far native view (§2.2.15): u32 offset with u16 length — short views at any offset of a full-size Heap32
    /// backing, where the u24 pair reaches only the first 16 MiB.
    FarSlice,

    /// A far adopted view (§2.2.15): the far geometry over an `Adopted` object.
    FarAdopted,

    /// An adopted view (§2.2.15): a range of an `Adopted` object — a foreign buffer, or an oversized view's child.
    Adopted,

    /// The packed UUID v1 form (§2.2.16): Gregorian timestamp low-first, top two timestamp bits implied zero.
    PackedUuidV1,

    /// A packed UUID v3 shard (§2.2.16): namespace-MD5, the suffix carrying the variant nibble's two data bits.
    PackedUuidV3S0,
    /// A packed UUID v3 shard (§2.2.16).
    PackedUuidV3S1,
    /// A packed UUID v3 shard (§2.2.16).
    PackedUuidV3S2,
    /// A packed UUID v3 shard (§2.2.16).
    PackedUuidV3S3,

    /// A packed UUID v4 shard (§2.2.16): random, the suffix carrying the variant nibble's two data bits.
    PackedUuidV4S0,
    /// A packed UUID v4 shard (§2.2.16).
    PackedUuidV4S1,
    /// A packed UUID v4 shard (§2.2.16).
    PackedUuidV4S2,
    /// A packed UUID v4 shard (§2.2.16).
    PackedUuidV4S3,

    /// A packed UUID v5 shard (§2.2.16): namespace-SHA-1, the suffix carrying the variant nibble's two data bits.
    PackedUuidV5S0,
    /// A packed UUID v5 shard (§2.2.16).
    PackedUuidV5S1,
    /// A packed UUID v5 shard (§2.2.16).
    PackedUuidV5S2,
    /// A packed UUID v5 shard (§2.2.16).
    PackedUuidV5S3,

    /// The packed UUID v6 form (§2.2.16): Gregorian timestamp most-significant-first, top two bits implied zero.
    PackedUuidV6,

    /// The packed UUID v7 form (§2.2.16): Unix-millisecond timestamp, top two bits implied zero.
    PackedUuidV7,

    /// A packed hex-byte string (§2.2.16): the digits in the payload, the spelling's format and case in its fifteenth
    /// byte beside the length.
    PackedHexBytes,
}

impl StorageType {
    /// Any of the ten inline types: content the payload carries verbatim or Latin-1-compressed.  A positive match, not
    /// the complement of the others — the immortal images and the §2.2.15 views are none of the three, so a complement
    /// would claim them.
    pub fn is_inline(self) -> bool {
        use StorageType::*;
        matches!(
            self,
            InlineAscii
                | InlineAsciiFull
                | InlineLatin1
                | InlineLatin1Full
                | InlineNonLatin1
                | InlineNonLatin1Full
                | InlineExtended
                | InlineExtendedFull
                | InlineBytes
                | InlineBytesFull
        )
    }

    /// Any of the twenty-two packed types: the six nibble alphabets and the §2.2.16 identifier families, whose content
    /// is encoded into the payload rather than stored there.
    pub fn is_packed(self) -> bool {
        use StorageType::*;
        matches!(
            self,
            PackedNumeric
                | PackedNumericFull
                | PackedDateTimePlus
                | PackedDateTimePlusFull
                | PackedDateTimeZulu
                | PackedDateTimeZuluFull
                | PackedUuidV1
                | PackedUuidV3S0
                | PackedUuidV3S1
                | PackedUuidV3S2
                | PackedUuidV3S3
                | PackedUuidV4S0
                | PackedUuidV4S1
                | PackedUuidV4S2
                | PackedUuidV4S3
                | PackedUuidV5S0
                | PackedUuidV5S1
                | PackedUuidV5S2
                | PackedUuidV5S3
                | PackedUuidV6
                | PackedUuidV7
                | PackedHexBytes
        )
    }

    /// The heap tiers that classify eagerly, keeping their scan state in the envelope or the variant (§2.2.3).
    pub fn is_small_heap_tier(self) -> bool {
        use StorageType::*;
        matches!(self, Heap8 | Heap8Ascii | Heap16 | Heap16Ascii)
    }

    /// Any of the four heap tiers, Ascii twins included.
    pub fn is_heap(self) -> bool {
        use StorageType::*;
        matches!(self, Heap8 | Heap8Ascii | Heap16 | Heap16Ascii | Heap32 | Heap)
    }
}

/// Generates the folded-tag variant set and the accessors over it.  Variant names are written out explicitly (not
/// synthesized by identifier concatenation) so a grep for any variant finds this defining invocation.
macro_rules! define_perl_string {
    (
        inline: [ $( $inline:ident = ($inline_class:ident, $inline_type:ident, $inline_full:literal, $inline_utf8:literal, $inline_tainted:literal) ),* $(,)? ],
        packed: [ $( $packed:ident = ($packed_alphabet:ident, $packed_type:ident, $packed_full:literal, $packed_utf8:literal, $packed_tainted:literal) ),* $(,)? ],
        uuids: [ $( $uuid:ident = ($uuid_form:ident, $uuid_type:ident, $uuid_utf8:literal, $uuid_tainted:literal) ),* $(,)? ],
        hexes: [ $( $hex:ident = ($hex_type:ident, $hex_utf8:literal, $hex_tainted:literal) ),* $(,)? ],
        heap8:  [ $( $heap8:ident  = ($heap8_utf8:literal,  $heap8_tainted:literal)  ),* $(,)? ],
        heap8_ascii:  [ $( $heap8a:ident  = ($heap8a_utf8:literal,  $heap8a_tainted:literal)  ),* $(,)? ],
        heap16: [ $( $heap16:ident = ($heap16_utf8:literal, $heap16_tainted:literal) ),* $(,)? ],
        heap16_ascii: [ $( $heap16a:ident = ($heap16a_utf8:literal, $heap16a_tainted:literal) ),* $(,)? ],
        heap32: [ $( $heap32:ident = ($heap32_utf8:literal, $heap32_tainted:literal) ),* $(,)? ],
        heap:   [ $( $heap:ident  = ($heap_utf8:literal,  $heap_tainted:literal)  ),* $(,)? ],
        immortal: [ $( $immortal:ident = ($immortal_utf8:literal, $immortal_tainted:literal) ),* $(,)? ],
        statics:  [ $( $static:ident  = ($static_utf8:literal,  $static_tainted:literal)  ),* $(,)? ],
        large_immortal: [ $( $large_immortal:ident = ($large_immortal_utf8:literal, $large_immortal_tainted:literal) ),* $(,)? ],
        large_statics:  [ $( $large_static:ident  = ($large_static_utf8:literal,  $large_static_tainted:literal)  ),* $(,)? ],
        slices:   [ $( $slice:ident   = ($slice_utf8:literal,   $slice_tainted:literal)   ),* $(,)? ],
        small_slices: [ $( $small_slice:ident = ($small_slice_utf8:literal, $small_slice_tainted:literal) ),* $(,)? ],
        far_slices:   [ $( $far_slice:ident   = ($far_slice_utf8:literal,   $far_slice_tainted:literal)   ),* $(,)? ],
        far_adopteds: [ $( $far_adopted:ident = ($far_adopted_utf8:literal, $far_adopted_tainted:literal) ),* $(,)? ],
        adopteds: [ $( $adopted:ident = ($adopted_utf8:literal, $adopted_tainted:literal) ),* $(,)? ]
    ) => {
        /// A Perl string.  See the module documentation.  The representation — the folded tag (§2.2.3) — is sealed
        /// behind this newtype: no variant is nameable outside the crate, so no payload can be forged or mutated around
        /// the invariants the unchecked readers rely on (probed: `#[non_exhaustive]` blocks construction but not
        /// mutation through `&mut` pattern binding, so the seal must be privacy).  Construct through the constructors;
        /// inspect through the methods and [`StorageType`].  `Clone` is derived: inline and packed payloads are plain
        /// arrays, and the heap variants' hand-written `Clone` is the refcount bump they need.  A hand-written impl
        /// that destructured the tag and rebuilt it cost thirteen nanoseconds to copy sixteen bytes (measured).
        #[derive(Clone)]
        #[repr(transparent)]
        pub struct PString(Repr);

        /// The sealed representation: the folded tag itself.  `repr(align(8))` sits here, on the inner enum, where it
        /// costs nothing — on a wrapper of the enclosing fusion it would defeat niche-filling (§2.3.6) — and the
        /// wrapper inherits the alignment.
        ///
        /// `Clone` and `Drop` are hand-written rather than derived, and must be: a heap variant holds an [`Owned`]
        /// pointer whose duplication requires the tier's `retain`, and whose release requires — for the small tiers —
        /// the capacity stored beside it in this very variant.  Only code that sees both can do either, which is why
        /// the obligation lives here rather than in a handle type (§2.2.3).
        #[repr(align(8))]
        enum Repr {
            $( $inline { buf: [u8; INLINE_MAX] }, )*
            $( $packed { nibbles: [u8; PACKED_BYTES] }, )*
            $( $uuid { payload: [u8; PACKED_BYTES] }, )*
            $( $hex { payload: [u8; PACKED_BYTES] }, )*
            $( $heap8  { ptr: Owned, len: u8,  cap: u8,  count: u8,  scan: scan::Terminal }, )*
            $( $heap8a { ptr: Owned, len: u8,  cap: u8 }, )*
            $( $heap16 { ptr: Owned, len: u16, cap: u16, count: u16, scan: scan::Terminal }, )*
            $( $heap16a { ptr: Owned, len: u16, cap: u16 }, )*
            $( $heap32 { ptr: Owned, len: u32 }, )*
            $( $heap  { ptr: Owned }, )*
            $( $immortal { ptr: Image, len: [u8; 3], count: [u8; 3], scan: scan::Terminal }, )*
            $( $static { ptr: Image, len: [u8; 3], count: [u8; 3], scan: scan::Terminal }, )*
            $( $large_immortal { head: &'static ImmortalHead }, )*
            $( $large_static { head: &'static ImmortalHead }, )*
            $( $slice { ptr: Owned, offset: [u8; 3], len: [u8; 3], scan: scan::ScanState }, )*
            $( $small_slice { ptr: Owned, offset: [u8; 2], len: [u8; 2], cap: [u8; 2], scan: scan::ScanState }, )*
            $( $far_slice { ptr: Owned, offset: [u8; 4], len: [u8; 2], scan: scan::ScanState }, )*
            $( $far_adopted { ptr: Owned, offset: [u8; 4], len: [u8; 2], scan: scan::ScanState }, )*
            $( $adopted { ptr: Owned, offset: [u8; 3], len: [u8; 3], scan: scan::ScanState }, )*
        }

        /// The number of `Repr` variants this invocation declares: checked arithmetic for the tag ledger, which must
        /// state this same number (§2.2.3, §2.2.9) — the assert beside the invocation fails any drift.
        pub(crate) const REPR_VARIANT_COUNT: usize = 0
            $( + { let _ = stringify!($inline); 1 } )*
            $( + { let _ = stringify!($packed); 1 } )*
            $( + { let _ = stringify!($uuid); 1 } )*
            $( + { let _ = stringify!($hex); 1 } )*
            $( + { let _ = stringify!($heap8a); 1 } )*
            $( + { let _ = stringify!($heap16a); 1 } )*
            $( + { let _ = stringify!($heap8); 1 } )*
            $( + { let _ = stringify!($heap16); 1 } )*
            $( + { let _ = stringify!($heap32); 1 } )*
            $( + { let _ = stringify!($heap); 1 } )*
            $( + { let _ = stringify!($immortal); 1 } )*
            $( + { let _ = stringify!($static); 1 } )*
            $( + { let _ = stringify!($large_immortal); 1 } )*
            $( + { let _ = stringify!($large_static); 1 } )*
            $( + { let _ = stringify!($slice); 1 } )*
            $( + { let _ = stringify!($small_slice); 1 } )*
            $( + { let _ = stringify!($far_slice); 1 } )*
            $( + { let _ = stringify!($far_adopted); 1 } )*
            $( + { let _ = stringify!($adopted); 1 } )*;

        impl Clone for Repr {
            fn clone(&self) -> Repr {
                match self {
                    $( Repr::$inline { buf } => Repr::$inline { buf: *buf }, )*
                    $( Repr::$packed { nibbles } => Repr::$packed { nibbles: *nibbles }, )*
                    $( Repr::$uuid { payload } => Repr::$uuid { payload: *payload }, )*
                    $( Repr::$hex { payload } => Repr::$hex { payload: *payload }, )*

                    // SAFETY (each heap arm): the variant owns a live allocation of its tier, and the new handle takes
                    // the reference this `retain` adds.
                    $( Repr::$heap8 { ptr, len, cap, count, scan } => Repr::$heap8 {
                        ptr: unsafe { cow_buffer::heap8::retain(ptr.as_ptr()); Owned::from_raw(ptr.as_ptr()) },
                        len: *len, cap: *cap, count: *count, scan: *scan,
                    }, )*
                    $( Repr::$heap16 { ptr, len, cap, count, scan } => Repr::$heap16 {
                        ptr: unsafe { cow_buffer::heap16::retain(ptr.as_ptr()); Owned::from_raw(ptr.as_ptr()) },
                        len: *len, cap: *cap, count: *count, scan: *scan,
                    }, )*
                    $( Repr::$heap8a { ptr, len, cap } => Repr::$heap8a {
                        ptr: unsafe { cow_buffer::heap8::retain(ptr.as_ptr()); Owned::from_raw(ptr.as_ptr()) },
                        len: *len, cap: *cap,
                    }, )*
                    $( Repr::$heap16a { ptr, len, cap } => Repr::$heap16a {
                        ptr: unsafe { cow_buffer::heap16::retain(ptr.as_ptr()); Owned::from_raw(ptr.as_ptr()) },
                        len: *len, cap: *cap,
                    }, )*
                    $( Repr::$heap32 { ptr, len } => Repr::$heap32 {
                        ptr: unsafe { cow_buffer::heap32::retain(ptr.as_ptr()); Owned::from_raw(ptr.as_ptr()) },
                        len: *len,
                    }, )*
                    $( Repr::$heap { ptr } => Repr::$heap {
                        ptr: unsafe { cow_buffer::heap::retain(ptr.as_ptr()); Owned::from_raw(ptr.as_ptr()) },
                    }, )*

                    // The immortal forms carry no refcount — the image outlives every handle by contract (§2.2.3) — so
                    // a clone is the envelope, bitwise.
                    $( Repr::$immortal { ptr, len, count, scan } =>
                        Repr::$immortal { ptr: *ptr, len: *len, count: *count, scan: *scan }, )*
                    $( Repr::$static { ptr, len, count, scan } =>
                        Repr::$static { ptr: *ptr, len: *len, count: *count, scan: *scan }, )*
                    $( Repr::$large_immortal { head } => Repr::$large_immortal { head }, )*
                    $( Repr::$large_static { head } => Repr::$large_static { head }, )*

                    // SAFETY (both view arms): the variant holds one reference on its live backing — the Heap32 buffer
                    // or the Adopted struct — and the new handle takes the reference this retain adds.
                    $( Repr::$slice { ptr, offset, len, scan } => Repr::$slice {
                        ptr: unsafe { cow_buffer::heap32::retain(ptr.as_ptr()); Owned::from_raw(ptr.as_ptr()) },
                        offset: *offset, len: *len, scan: *scan,
                    }, )*
                    $( Repr::$small_slice { ptr, offset, len, cap, scan } => Repr::$small_slice {
                        ptr: unsafe { small_backing_retain(ptr.as_ptr(), u16v(*cap)); Owned::from_raw(ptr.as_ptr()) },
                        offset: *offset, len: *len, cap: *cap, scan: *scan,
                    }, )*
                    $( Repr::$far_slice { ptr, offset, len, scan } => Repr::$far_slice {
                        ptr: unsafe { cow_buffer::heap32::retain(ptr.as_ptr()); Owned::from_raw(ptr.as_ptr()) },
                        offset: *offset, len: *len, scan: *scan,
                    }, )*
                    $( Repr::$far_adopted { ptr, offset, len, scan } => Repr::$far_adopted {
                        ptr: unsafe { cow_buffer::Adopted::retain(ptr.as_ptr().cast()); Owned::from_raw(ptr.as_ptr()) },
                        offset: *offset, len: *len, scan: *scan,
                    }, )*
                    $( Repr::$adopted { ptr, offset, len, scan } => Repr::$adopted {
                        ptr: unsafe { cow_buffer::Adopted::retain(ptr.as_ptr().cast()); Owned::from_raw(ptr.as_ptr()) },
                        offset: *offset, len: *len, scan: *scan,
                    }, )*
                }
            }
        }

        impl Drop for Repr {
            fn drop(&mut self) {
                // SAFETY (each heap arm): the variant owns exactly one reference on a live allocation of its tier,
                // consumed here; the small tiers pass the capacity their allocation does not record.
                match self {
                    $( Repr::$inline { .. } => {}, )*
                    $( Repr::$packed { .. } => {}, )*
                    $( Repr::$uuid { .. } => {}, )*
                    $( Repr::$hex { .. } => {}, )*
                    $( Repr::$heap8 { ptr, cap, .. } => unsafe { cow_buffer::heap8::release(ptr.claim(), *cap) }, )*
                    $( Repr::$heap16 { ptr, cap, .. } => unsafe { cow_buffer::heap16::release(ptr.claim(), *cap) }, )*
                    $( Repr::$heap8a { ptr, cap, .. } => unsafe { cow_buffer::heap8::release(ptr.claim(), *cap) }, )*
                    $( Repr::$heap16a { ptr, cap, .. } => unsafe { cow_buffer::heap16::release(ptr.claim(), *cap) }, )*
                    $( Repr::$heap32 { ptr, .. } => unsafe { cow_buffer::heap32::release(ptr.claim()) }, )*
                    $( Repr::$heap { ptr } => unsafe { cow_buffer::heap::release(ptr.claim()) }, )*

                    // Teardown never touches an immortal image (§2.2.3): the slab's owner frees it, statics are the
                    // program's.
                    $( Repr::$immortal { .. } => {}, )*
                    $( Repr::$static { .. } => {}, )*
                    $( Repr::$large_immortal { .. } => {}, )*
                    $( Repr::$large_static { .. } => {}, )*

                    // SAFETY (both view arms): the variant owns exactly one reference on its live backing, consumed
                    // here; the adopted pointer round-trips through the untyped Owned it rode in.
                    $( Repr::$slice { ptr, .. } => unsafe { cow_buffer::heap32::release(ptr.claim()) }, )*
                    $( Repr::$small_slice { ptr, cap, .. } => unsafe { small_backing_release(ptr.claim(), u16v(*cap)) }, )*
                    $( Repr::$far_slice { ptr, .. } => unsafe { cow_buffer::heap32::release(ptr.claim()) }, )*
                    $( Repr::$far_adopted { ptr, .. } => unsafe { cow_buffer::Adopted::release(ptr.claim().cast()) }, )*
                    $( Repr::$adopted { ptr, .. } => unsafe { cow_buffer::Adopted::release(ptr.claim().cast()) }, )*
                }
            }
        }

        impl PString {
            /// The storage type (§2.2.9's normative vocabulary).
            pub fn storage_type(&self) -> StorageType {
                match &self.0 {
                    $( Repr::$inline { .. } => StorageType::$inline_type, )*
                    $( Repr::$packed { .. } => StorageType::$packed_type, )*
                    $( Repr::$uuid { .. } => StorageType::$uuid_type, )*
                    $( Repr::$hex { .. } => StorageType::$hex_type, )*
                    $( Repr::$heap8 { .. } => StorageType::Heap8, )*
                    $( Repr::$heap16 { .. } => StorageType::Heap16, )*
                    $( Repr::$heap8a { .. } => StorageType::Heap8Ascii, )*
                    $( Repr::$heap16a { .. } => StorageType::Heap16Ascii, )*
                    $( Repr::$heap32 { .. } => StorageType::Heap32, )*
                    $( Repr::$heap { .. } => StorageType::Heap, )*
                    $( Repr::$immortal { .. } => StorageType::Immortal, )*
                    $( Repr::$static { .. } => StorageType::Static, )*
                    $( Repr::$large_immortal { .. } => StorageType::LargeImmortal, )*
                    $( Repr::$large_static { .. } => StorageType::LargeStatic, )*
                    $( Repr::$slice { .. } => StorageType::MediumSlice, )*
                    $( Repr::$small_slice { .. } => StorageType::SmallSlice, )*
                    $( Repr::$far_slice { .. } => StorageType::FarSlice, )*
                    $( Repr::$far_adopted { .. } => StorageType::FarAdopted, )*
                    $( Repr::$adopted { .. } => StorageType::Adopted, )*
                }
            }

            /// The Perl utf8 flag (semantic claim, not validity — see module docs).
            pub fn is_utf8(&self) -> bool {
                match &self.0 {
                    $( Repr::$inline { .. } => $inline_utf8, )*
                    $( Repr::$packed { .. } => $packed_utf8, )*
                    $( Repr::$uuid { .. } => $uuid_utf8, )*
                    $( Repr::$hex { .. } => $hex_utf8, )*
                    $( Repr::$heap8 { .. } => $heap8_utf8, )*
                    $( Repr::$heap16 { .. } => $heap16_utf8, )*
                    $( Repr::$heap8a { .. } => $heap8a_utf8, )*
                    $( Repr::$heap16a { .. } => $heap16a_utf8, )*
                    $( Repr::$heap32 { .. } => $heap32_utf8, )*
                    $( Repr::$heap { .. } => $heap_utf8, )*
                    $( Repr::$immortal { .. } => $immortal_utf8, )*
                    $( Repr::$static { .. } => $static_utf8, )*
                    $( Repr::$large_immortal { .. } => $large_immortal_utf8, )*
                    $( Repr::$large_static { .. } => $large_static_utf8, )*
                    $( Repr::$slice { .. } => $slice_utf8, )*
                    $( Repr::$small_slice { .. } => $small_slice_utf8, )*
                    $( Repr::$far_slice { .. } => $far_slice_utf8, )*
                    $( Repr::$far_adopted { .. } => $far_adopted_utf8, )*
                    $( Repr::$adopted { .. } => $adopted_utf8, )*
                }
            }

            /// Whether this value is tainted (§2.6).
            pub fn is_tainted(&self) -> bool {
                match &self.0 {
                    $( Repr::$inline { .. } => $inline_tainted, )*
                    $( Repr::$packed { .. } => $packed_tainted, )*
                    $( Repr::$uuid { .. } => $uuid_tainted, )*
                    $( Repr::$hex { .. } => $hex_tainted, )*
                    $( Repr::$heap8 { .. } => $heap8_tainted, )*
                    $( Repr::$heap16 { .. } => $heap16_tainted, )*
                    $( Repr::$heap8a { .. } => $heap8a_tainted, )*
                    $( Repr::$heap16a { .. } => $heap16a_tainted, )*
                    $( Repr::$heap32 { .. } => $heap32_tainted, )*
                    $( Repr::$heap { .. } => $heap_tainted, )*
                    $( Repr::$immortal { .. } => $immortal_tainted, )*
                    $( Repr::$static { .. } => $static_tainted, )*
                    $( Repr::$large_immortal { .. } => $large_immortal_tainted, )*
                    $( Repr::$large_static { .. } => $large_static_tainted, )*
                    $( Repr::$slice { .. } => $slice_tainted, )*
                    $( Repr::$small_slice { .. } => $small_slice_tainted, )*
                    $( Repr::$far_slice { .. } => $far_slice_tainted, )*
                    $( Repr::$far_adopted { .. } => $far_adopted_tainted, )*
                    $( Repr::$adopted { .. } => $adopted_tainted, )*
                }
            }

            /// The inline content class, or `None` for heap storage.  Internal: the public vocabulary is
            /// [`StorageType`], of which this is the class projection.
            fn inline_class(&self) -> Option<InlineClass> {
                match &self.0 {
                    $( Repr::$inline { .. } => Some(InlineClass::$inline_class), )*
                    $( Repr::$immortal { .. } => None, )*
                    $( Repr::$static { .. } => None, )*
                    $( Repr::$large_immortal { .. } => None, )*
                    $( Repr::$large_static { .. } => None, )*
                    $( Repr::$heap8a { .. } => None, )*
                    $( Repr::$slice { .. } => None, )*
                    $( Repr::$small_slice { .. } => None, )*
                    $( Repr::$far_slice { .. } => None, )*
                    $( Repr::$far_adopted { .. } => None, )*
                    $( Repr::$adopted { .. } => None, )*
                    $( Repr::$heap16a { .. } => None, )*

                    // Packed alphabets are ASCII by construction, so the scan state is fixed.
                    $( Repr::$packed { .. } => Some(InlineClass::Ascii), )*
                    $( Repr::$uuid { .. } => Some(InlineClass::Ascii), )*
                    $( Repr::$hex { .. } => Some(InlineClass::Ascii), )*
                    $( Repr::$heap8 { .. } => None, )*
                    $( Repr::$heap16 { .. } => None, )*
                    $( Repr::$heap32 { .. } => None, )*
                    $( Repr::$heap { .. } => None, )*
                }
            }

            /// Rebuild an inline value with the given tag dimensions and payload.  `s` is the payload byte count and
            /// selects the length family; `aux` is the class's second nibble (§2.2.9) — the high-bit count for the
            /// compressed classes, the decoded character count for the verbatim valid classes, zero for Ascii and
            /// Bytes — stored beside `s` in the short family, implied and derived at full capacity.  Internal: tag
            /// transitions go through the public monotonic/setter methods.
            fn build_inline(class: InlineClass, utf8: bool, tainted: bool, s: usize, aux: usize, buf: [u8; INLINE_MAX]) -> PString {
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

                match (class, s == INLINE_MAX, utf8, tainted) {
                    $( (InlineClass::$inline_class, $inline_full, $inline_utf8, $inline_tainted) => PString(Repr::$inline { buf }), )*
                }
            }

            /// Build a packed value with the given alphabet, length family, and tag dimensions.
            fn build_packed(packed: Packed, utf8: bool, tainted: bool) -> PString {
                match (packed.alphabet, packed.full, utf8, tainted) {
                    $( (PackedAlphabet::$packed_alphabet, $packed_full, $packed_utf8, $packed_tainted) => PString(Repr::$packed { nibbles: packed.nibbles }), )*
                }
            }

            /// Mint a packed UUID (§2.2.16): the form and both flags select the variant, exhaustively — fifteen forms
            /// times four flag combinations is the sixty arms below, no fallback.
            fn build_uuid(form: UuidForm, payload: [u8; PACKED_BYTES], utf8: bool, tainted: bool) -> PString {
                match (form, utf8, tainted) {
                    $( (UuidForm::$uuid_form, $uuid_utf8, $uuid_tainted) => PString(Repr::$uuid { payload }), )*
                }
            }

            /// Mint a packed hex-byte string (§2.2.16): both flags select the variant, exhaustively — the format and
            /// case ride the payload, so the tag carries nothing else.
            fn build_hex(payload: [u8; PACKED_BYTES], utf8: bool, tainted: bool) -> PString {
                match (utf8, tainted) {
                    $( ($hex_utf8, $hex_tainted) => PString(Repr::$hex { payload }), )*
                }
            }

            /// The payload behind the tag, borrowed.  Generated rather than hand-written: with three storage kinds the
            /// explicit variant lists ran past a hundred names, and the per-section repetition expresses it exactly.
            fn raw_parts(&self) -> RawParts<'_> {
                match &self.0 {
                    $( Repr::$inline { buf } => RawParts::Inline { class: InlineClass::$inline_class, full: $inline_full, buf }, )*
                    $( Repr::$packed { nibbles } => RawParts::Packed(Packed {
                        alphabet: PackedAlphabet::$packed_alphabet,
                        full: $packed_full,
                        nibbles: *nibbles,
                    }), )*
                    $( Repr::$uuid { payload } => RawParts::Uuid { form: UuidForm::$uuid_form, payload }, )*
                    $( Repr::$hex { payload } => RawParts::Hex { payload }, )*
                    $( Repr::$heap8 { ptr, len, cap, count, scan } =>
                        RawParts::Heap(HeapView::small(ptr, *len as usize, *cap as usize, *count as usize, scan.widen(), Tier::Heap8)), )*
                    $( Repr::$heap16 { ptr, len, cap, count, scan } =>
                        RawParts::Heap(HeapView::small(ptr, *len as usize, *cap as usize, *count as usize, scan.widen(), Tier::Heap16)), )*

                    // The Ascii twins' omitted fields are derivable (§2.2.3): every byte is a character, and the class
                    // is the variant.
                    $( Repr::$heap8a { ptr, len, cap } =>
                        RawParts::Heap(HeapView::small(ptr, *len as usize, *cap as usize, *len as usize, scan::Ascii, Tier::Heap8)), )*
                    $( Repr::$heap16a { ptr, len, cap } =>
                        RawParts::Heap(HeapView::small(ptr, *len as usize, *cap as usize, *len as usize, scan::Ascii, Tier::Heap16)), )*

                    // SAFETY: a live allocation of this tier, whose header carries the metadata.
                    $( Repr::$heap32 { ptr, len } => RawParts::Heap(unsafe { HeapView::heap32(ptr, *len as usize) }), )*
                    $( Repr::$heap { ptr } => RawParts::Heap(unsafe { HeapView::large(ptr, Tier::Heap) }), )*

                    // SAFETY (each immortal arm): the image outlives every handle by the forms' contract (§2.2.3), so a
                    // borrow bounded by `&self` is always inside its life.
                    $( Repr::$immortal { ptr, len, count, scan } => RawParts::Borrowed {
                        bytes: unsafe { std::slice::from_raw_parts(ptr.0.as_ptr(), u24_get(len)) },
                        count: u24_get(count),
                        scan: *scan,
                    }, )*
                    $( Repr::$static { ptr, len, count, scan } => RawParts::Borrowed {
                        bytes: unsafe { std::slice::from_raw_parts(ptr.0.as_ptr(), u24_get(len)) },
                        count: u24_get(count),
                        scan: *scan,
                    }, )*
                    $( Repr::$large_immortal { head } => RawParts::Borrowed {
                        bytes: head.bytes(),
                        count: head.count,
                        scan: head.scan,
                    }, )*
                    $( Repr::$large_static { head } => RawParts::Borrowed {
                        bytes: head.bytes(),
                        count: head.count,
                        scan: head.scan,
                    }, )*

                    // SAFETY (both view arms): the variant holds a reference on its live backing, which pins the bytes;
                    // the range was bounds-checked at birth.
                    $( Repr::$slice { ptr, offset, len, scan } => RawParts::View {
                        bytes: unsafe { std::slice::from_raw_parts(ptr.as_ptr().as_ptr().add(u24(*offset)), u24(*len)) },
                        scan: *scan,
                        backing: None,
                    }, )*
                    $( Repr::$small_slice { ptr, offset, len, scan, .. } => RawParts::View {
                        bytes: unsafe { std::slice::from_raw_parts(ptr.as_ptr().as_ptr().add(u16v(*offset)), u16v(*len)) },
                        scan: *scan,
                        backing: None,
                    }, )*
                    $( Repr::$far_slice { ptr, offset, len, scan } => RawParts::View {
                        bytes: unsafe { std::slice::from_raw_parts(ptr.as_ptr().as_ptr().add(u32v(*offset)), u16v(*len)) },
                        scan: *scan,
                        backing: None,
                    }, )*
                    $( Repr::$far_adopted { ptr, offset, len, scan } => RawParts::View {
                        bytes: unsafe {
                            let a: &cow_buffer::Adopted = ptr.as_ptr().cast::<cow_buffer::Adopted>().as_ref();
                            &a.as_slice()[u32v(*offset)..u32v(*offset) + u16v(*len)]
                        },
                        scan: *scan,
                        backing: None,
                    }, )*
                    $( Repr::$adopted { ptr, offset, len, scan } => {
                        // SAFETY: the handle holds a reference on the live struct, which pins struct and bytes both.
                        let a: &cow_buffer::Adopted = unsafe { ptr.as_ptr().cast::<cow_buffer::Adopted>().as_ref() };
                        let span = u24(*offset) == SPAN as usize && u24(*len) == SPAN as usize;
                        let (off, n) = if span { (0, a.total_len()) } else { (u24(*offset), u24(*len)) };
                        RawParts::View {
                            // SAFETY: within the object, bounds-checked at birth; SPAN is the whole object.
                            bytes: unsafe { &a.as_slice()[off..off + n] },
                            scan: *scan,
                            backing: if span { Some(a) } else { None },
                        }
                    }, )*
                }
            }

            /// The inline payload, mutably, for appends that leave the tag alone.  `None` for the other storage kinds,
            /// whose payloads cannot be extended in place.
            fn inline_buf_mut(&mut self) -> Option<(bool, &mut [u8; INLINE_MAX])> {
                match &mut self.0 {
                    $( Repr::$inline { buf } => Some(($inline_full, buf)), )*
                    $( Repr::$packed { .. } => None, )*
                    $( Repr::$uuid { .. } => None, )*
                    $( Repr::$hex { .. } => None, )*
                    $( Repr::$immortal { .. } => None, )*
                    $( Repr::$static { .. } => None, )*
                    $( Repr::$large_immortal { .. } => None, )*
                    $( Repr::$large_static { .. } => None, )*
                    $( Repr::$heap8 { .. } => None, )*
                    $( Repr::$heap8a { .. } => None, )*
                    $( Repr::$heap16 { .. } => None, )*
                    $( Repr::$heap16a { .. } => None, )*
                    $( Repr::$heap32 { .. } => None, )*
                    $( Repr::$heap { .. } => None, )*
                    $( Repr::$slice { .. } => None, )*
                    $( Repr::$small_slice { .. } => None, )*
                    $( Repr::$far_slice { .. } => None, )*
                    $( Repr::$far_adopted { .. } => None, )*
                    $( Repr::$adopted { .. } => None, )*
                }
            }

            /// Rewrite a heap buffer as the UTF-8 upgrade of its Latin-1 content, where that is possible without
            /// leaving the allocation.  `None` means the caller must take the copying form.
            ///
            /// Two conditions, and both are refusals rather than problems to solve: a shared buffer cannot be written
            /// at all, and one whose expansion exceeds its spare capacity would have to reallocate — at which point
            /// copying *is* the operation, and it picks the right tier on the way (§2.2.3).  So there is no growth here
            /// and no tier change: either it fits where it stands, or it moves.
            fn upgrade_heap_in_place(&mut self) -> Option<usize> {
                let (first, expansion, old_len) = match self.raw_parts() {
                    RawParts::Heap(view) => {
                        let bytes = view.as_slice();
                        let first = cow_buffer::first_variant(bytes)?;
                        if !view.is_unique() {
                            return None;
                        }
                        let expansion = cow_buffer::variant_count(&bytes[first..]);
                        if bytes.len() + expansion > view.capacity() {
                            return None;
                        }
                        (first, expansion, bytes.len())
                    }
                    _ => return None,
                };
                let new_len = old_len + expansion;

                // SAFETY (each arm): unique and within capacity, both checked above; the loop rewrites only bytes this
                // handle owns.  Every old byte becomes exactly one character, so the count is the old length, and the
                // result is Latin-1 range by construction.
                match &mut self.0 {
                    $( Repr::$heap8 { ptr, len, count, scan, .. } => {
                        unsafe { cow_buffer::expand_latin1_in_place(ptr.as_ptr(), first, old_len, new_len) };
                        *len = new_len as u8;
                        *count = old_len as u8;
                        *scan = scan::Terminal::Utf8Latin1;
                    }, )*
                    $( Repr::$heap16 { ptr, len, count, scan, .. } => {
                        unsafe { cow_buffer::expand_latin1_in_place(ptr.as_ptr(), first, old_len, new_len) };
                        *len = new_len as u16;
                        *count = old_len as u16;
                        *scan = scan::Terminal::Utf8Latin1;
                    }, )*
                    $( Repr::$heap32 { ptr, len } => {
                        unsafe {
                            cow_buffer::expand_latin1_in_place(ptr.as_ptr(), first, old_len, new_len);
                            cow_buffer::heap32::set_scan(ptr.as_ptr(), scan::Utf8Latin1.as_u8());
                            cow_buffer::heap32::set_char_count(ptr.as_ptr(), old_len as u32);
                        }
                        *len = new_len as u32;
                    }, )*
                    $( Repr::$heap { ptr } => unsafe {
                        cow_buffer::expand_latin1_in_place(ptr.as_ptr(), first, old_len, new_len);
                        cow_buffer::heap::set_len(ptr.as_ptr(), new_len);
                        cow_buffer::heap::set_scan(ptr.as_ptr(), scan::Utf8Latin1.as_u8());
                        cow_buffer::heap::set_char_count(ptr.as_ptr(), old_len);
                    }, )*
                    _ => return None,
                }
                Some(new_len)
            }

            /// Rewrite a heap buffer as the Latin-1 contraction of its UTF-8 content.  `Some(false)` means a character
            /// past U+00FF made the contraction impossible; `None` means the buffer is shared and the caller must copy.
            ///
            /// Contraction only shrinks, so unlike the upgrade it can never want room it lacks, never reallocate and
            /// never change tier.  It leaves spare capacity behind; whether that should be trimmed is a question about
            /// trimming in general, deliberately left open.
            fn downgrade_heap_in_place(&mut self) -> Option<bool> {
                let (first, contractions, old_len) = match self.raw_parts() {
                    RawParts::Heap(view) => {
                        let bytes = view.as_slice();
                        let Some(first) = cow_buffer::first_variant(bytes) else {
                            return Some(true); // Already its own downgrade.
                        };
                        if !view.is_unique() {
                            return None;
                        }
                        let Some(contractions) = cow_buffer::latin1_contractions(&bytes[first..]) else {
                            return Some(false);
                        };
                        (first, contractions, bytes.len())
                    }
                    _ => return None,
                };
                let new_len = old_len - contractions;

                // SAFETY (each arm): unique and shrinking, both established above.  The contracted octets can
                // themselves be valid UTF-8, so the class is not derivable without a scan — and here the tiers part
                // ways.  A large tier records UNKNOWN and lets the next reader both derive and store.  A small tier has
                // nowhere to store a later discovery, so UNKNOWN there means re-deriving on every read forever; it must
                // classify now, exactly as construction does, and at these sizes the pass costs what the construction
                // pass cost.  Writing UNKNOWN here would cost a scan on every subsequent validity read — (1, 1) across
                // two reads where (1, 0) is the invariant.
                match &mut self.0 {
                    $( Repr::$heap8 { ptr, len, count, scan, .. } => {
                        unsafe { cow_buffer::contract_latin1_in_place(ptr.as_ptr(), first, old_len, new_len) };
                        *len = new_len as u8;

                        // SAFETY: the first `new_len` bytes were just written by the contraction.
                        let (state, chars) = classify_full(unsafe { std::slice::from_raw_parts(ptr.as_ptr().as_ptr(), new_len) });
                        *count = chars as u8;
                        *scan = state;
                    }, )*
                    $( Repr::$heap16 { ptr, len, count, scan, .. } => {
                        unsafe { cow_buffer::contract_latin1_in_place(ptr.as_ptr(), first, old_len, new_len) };
                        *len = new_len as u16;

                        // SAFETY: the first `new_len` bytes were just written by the contraction.
                        let (state, chars) = classify_full(unsafe { std::slice::from_raw_parts(ptr.as_ptr().as_ptr(), new_len) });
                        *count = chars as u16;
                        *scan = state;
                    }, )*
                    $( Repr::$heap32 { ptr, len } => {
                        unsafe {
                            cow_buffer::contract_latin1_in_place(ptr.as_ptr(), first, old_len, new_len);
                            cow_buffer::heap32::set_scan(ptr.as_ptr(), scan::Unknown.as_u8());
                            cow_buffer::heap32::set_char_count(ptr.as_ptr(), 0);
                        }
                        *len = new_len as u32;
                    }, )*
                    $( Repr::$heap { ptr } => unsafe {
                        cow_buffer::contract_latin1_in_place(ptr.as_ptr(), first, old_len, new_len);
                        cow_buffer::heap::set_len(ptr.as_ptr(), new_len);
                        cow_buffer::heap::set_scan(ptr.as_ptr(), scan::Unknown.as_u8());
                        cow_buffer::heap::set_char_count(ptr.as_ptr(), 0usize);
                    }, )*
                    _ => return None,
                }
                Some(true)
            }

            /// Extend a unique heap buffer in place when the appended result fits its spare capacity (§2.2.3): the
            /// class headroom buffers are born with exists for exactly this, so the common append pays one suffix copy
            /// and no allocation.  `false` means the fast path does not apply — not heap, shared, or over capacity —
            /// and the caller rebuilds through the tier-choosing constructor, which is also the only road across a tier
            /// ceiling.
            fn append_heap_in_place(&mut self, bytes: &[u8], kind: AppendKind) -> bool {
                let (old_len, new_len, prior, prior_chars) = match self.raw_parts() {
                    RawParts::Heap(view) => {
                        let old_len = view.as_slice().len();
                        let Some(new_len) = old_len.checked_add(bytes.len()) else { return false };
                        if !view.is_unique() || new_len > view.capacity() {
                            return false;
                        }
                        (old_len, new_len, view.scan(), view.char_count())
                    }
                    _ => return false,
                };

                let state = append_transition_heap(prior, kind);

                // Maintain the character count incrementally when both sides know theirs (§2.2.5), exactly as the
                // rebuild does.
                let chars = match kind {
                    AppendKind::Valid { chars: added, .. } if prior_chars > 0 && added > 0 && scan::is_perl_decodable(state) => prior_chars + added,
                    _ => 0,
                };

                // SAFETY (each arm): unique and within capacity, both established above, so the suffix copy stays
                // inside the allocation this handle solely owns, and `bytes` cannot alias it while `&mut self` is held.
                // The small tiers must end settled — below 64 KiB the state is now or never (§2.2.3) — so where the
                // transition or the count came out indeterminate they classify the joined content: one pass, still no
                // allocation.  The large tiers record what is known and let the next reader derive the rest.
                match &mut self.0 {
                    // A twin's implied class survives only an append that stays Ascii; anything else bails to the
                    // rebuild, which re-dispatches the variant by the joined content's class.
                    $( Repr::$heap8a { ptr, len, .. } => {
                        if state != scan::Ascii {
                            return false;
                        }
                        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.as_ptr().as_ptr().add(old_len), bytes.len()) };
                        *len = new_len as u8;
                    }, )*
                    $( Repr::$heap16a { ptr, len, .. } => {
                        if state != scan::Ascii {
                            return false;
                        }
                        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.as_ptr().as_ptr().add(old_len), bytes.len()) };
                        *len = new_len as u16;
                    }, )*
                    $( Repr::$heap8 { ptr, len, count, scan, .. } => {
                        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.as_ptr().as_ptr().add(old_len), bytes.len()) };
                        *len = new_len as u8;
                        if scan::is_terminal(state) && chars > 0 {
                            *scan = scan::Terminal::from_scan(state);
                            *count = chars as u8;
                        } else {
                            // SAFETY: the first `new_len` bytes are the old content plus the suffix just copied.
                            let (settled, counted) = classify_full(unsafe { std::slice::from_raw_parts(ptr.as_ptr().as_ptr(), new_len) });
                            *scan = settled;
                            *count = counted as u8;
                        }
                    }, )*
                    $( Repr::$heap16 { ptr, len, count, scan, .. } => {
                        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.as_ptr().as_ptr().add(old_len), bytes.len()) };
                        *len = new_len as u16;
                        if scan::is_terminal(state) && chars > 0 {
                            *scan = scan::Terminal::from_scan(state);
                            *count = chars as u16;
                        } else {
                            // SAFETY: the first `new_len` bytes are the old content plus the suffix just copied.
                            let (settled, counted) = classify_full(unsafe { std::slice::from_raw_parts(ptr.as_ptr().as_ptr(), new_len) });
                            *scan = settled;
                            *count = counted as u16;
                        }
                    }, )*
                    $( Repr::$heap32 { ptr, len } => {
                        unsafe {
                            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.as_ptr().as_ptr().add(old_len), bytes.len());
                            cow_buffer::heap32::set_scan(ptr.as_ptr(), state.as_u8());
                            cow_buffer::heap32::set_char_count(ptr.as_ptr(), chars as u32);
                        }
                        *len = new_len as u32;
                    }, )*
                    $( Repr::$heap { ptr } => unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.as_ptr().as_ptr().add(old_len), bytes.len());
                        cow_buffer::heap::set_len(ptr.as_ptr(), new_len);
                        cow_buffer::heap::set_scan(ptr.as_ptr(), state.as_u8());
                        cow_buffer::heap::set_char_count(ptr.as_ptr(), chars);
                    }, )*
                    _ => return false,
                }
                true
            }

            /// Whether this value is held on the heap, and in which tier.  In-place transforms need the variant itself
            /// rather than a handle, because a small tier's length lives in the envelope beside the pointer.
            #[allow(dead_code)]
            fn heap_tier(&self) -> Option<Tier> {
                match &self.0 {
                    $( Repr::$inline { .. } => None, )*
                    $( Repr::$packed { .. } => None, )*
                    $( Repr::$uuid { .. } => None, )*
                    $( Repr::$hex { .. } => None, )*
                    $( Repr::$immortal { .. } => None, )*
                    $( Repr::$static { .. } => None, )*
                    $( Repr::$large_immortal { .. } => None, )*
                    $( Repr::$large_static { .. } => None, )*
                    $( Repr::$heap8 { .. } => Some(Tier::Heap8), )*
                    $( Repr::$heap8a { .. } => Some(Tier::Heap8), )*
                    $( Repr::$heap16a { .. } => Some(Tier::Heap16), )*
                    $( Repr::$heap16 { .. } => Some(Tier::Heap16), )*
                    $( Repr::$heap32 { .. } => Some(Tier::Heap32), )*
                    $( Repr::$heap { .. } => Some(Tier::Heap), )*
                    $( Repr::$slice { .. } => None, )*
                    $( Repr::$small_slice { .. } => None, )*
                    $( Repr::$far_slice { .. } => None, )*
                    $( Repr::$far_adopted { .. } => None, )*
                    $( Repr::$adopted { .. } => None, )*
                }
            }

            /// The payload behind the tag, owned — the shape mutation needs, since it rebuilds the tag afterward.
            fn into_raw(self) -> RawOwned {
                // `Owned` is not `Copy`, so taking a heap variant's pointer out is a genuine move, and `Repr`'s `Drop`
                // forbids that (E0509).  The refusal is the point: a `Copy` pointer would be read out here while `self`
                // still dropped, releasing an allocation the caller believes it owns.  Matching on a reference and
                // reading the one non-`Copy` field makes the transfer explicit, and suppressing the source's drop is
                // what makes it sound.
                let this = std::mem::ManuallyDrop::new(self);

                // SAFETY (each heap arm): `this` is never dropped, so the pointer is read out exactly once and the
                // reference it carries transfers to the returned `RawOwned`.
                match &this.0 {
                    $( Repr::$inline { buf } => RawOwned::Inline { class: InlineClass::$inline_class, full: $inline_full, buf: *buf }, )*
                    $( Repr::$packed { nibbles } => RawOwned::Packed(Packed {
                        alphabet: PackedAlphabet::$packed_alphabet,
                        full: $packed_full,
                        nibbles: *nibbles,
                    }), )*
                    $( Repr::$uuid { payload } => RawOwned::Uuid { form: UuidForm::$uuid_form, payload: *payload }, )*
                    $( Repr::$hex { payload } => RawOwned::Hex { payload: *payload }, )*
                    $( Repr::$heap8 { ptr, len, cap, count, scan } => RawOwned::Heap {
                        ptr: unsafe { std::ptr::read(ptr) },
                        len: *len as usize,
                        cap: *cap as usize,
                        count: *count as usize,
                        scan: scan.widen(),
                        tier: Tier::Heap8,
                    }, )*
                    $( Repr::$heap16 { ptr, len, cap, count, scan } => RawOwned::Heap {
                        ptr: unsafe { std::ptr::read(ptr) },
                        len: *len as usize,
                        cap: *cap as usize,
                        count: *count as usize,
                        scan: scan.widen(),
                        tier: Tier::Heap16,
                    }, )*
                    $( Repr::$heap8a { ptr, len, cap } => RawOwned::Heap {
                        ptr: unsafe { std::ptr::read(ptr) },
                        len: *len as usize,
                        cap: *cap as usize,
                        count: *len as usize,
                        scan: scan::Ascii,
                        tier: Tier::Heap8,
                    }, )*
                    $( Repr::$heap16a { ptr, len, cap } => RawOwned::Heap {
                        ptr: unsafe { std::ptr::read(ptr) },
                        len: *len as usize,
                        cap: *cap as usize,
                        count: *len as usize,
                        scan: scan::Ascii,
                        tier: Tier::Heap16,
                    }, )*

                    // Large tiers keep their metadata in the allocation, so only the pointer travels.
                    // SAFETY (both large arms): a live allocation still owned here; reading its header preserves the
                    // facts it knew across the take instead of discarding them.
                    $( Repr::$heap32 { ptr, len } => RawOwned::Heap {
                        len: *len as usize,
                        cap: unsafe { cow_buffer::heap32::capacity(ptr.as_ptr()) },
                        count: unsafe { cow_buffer::heap32::char_count(ptr.as_ptr()) } as usize,
                        scan: scan::ScanState::from_u8(unsafe { cow_buffer::heap32::scan(ptr.as_ptr()) }),
                        ptr: unsafe { std::ptr::read(ptr) },
                        tier: Tier::Heap32,
                    }, )*
                    $( Repr::$heap { ptr } => RawOwned::Heap {
                        len: unsafe { cow_buffer::heap::len(ptr.as_ptr()) },
                        cap: unsafe { cow_buffer::heap::capacity(ptr.as_ptr()) },
                        count: unsafe { cow_buffer::heap::char_count(ptr.as_ptr()) },
                        scan: scan::ScanState::from_u8(unsafe { cow_buffer::heap::scan(ptr.as_ptr()) }),
                        ptr: unsafe { std::ptr::read(ptr) },
                        tier: Tier::Heap,
                    }, )*

                    // The immortal forms own nothing to transfer: the envelope is `Copy` in all but name.
                    $( Repr::$immortal { ptr, len, count, scan } => RawOwned::Borrowed {
                        form: BorrowedForm::Immortal, ptr: ptr.0, len: u24_get(len), count: u24_get(count), scan: *scan,
                    }, )*
                    $( Repr::$static { ptr, len, count, scan } => RawOwned::Borrowed {
                        form: BorrowedForm::Static, ptr: ptr.0, len: u24_get(len), count: u24_get(count), scan: *scan,
                    }, )*
                    $( Repr::$large_immortal { head } => RawOwned::BorrowedLarge { form: BorrowedForm::Immortal, head }, )*
                    $( Repr::$large_static { head } => RawOwned::BorrowedLarge { form: BorrowedForm::Static, head }, )*

                    // SAFETY (both view arms): `this` is never dropped, so the backing reference transfers to the
                    // returned transport, which carries which release it owes through `native`.
                    $( Repr::$slice { ptr, offset, len, scan } => RawOwned::View {
                        ptr: unsafe { std::ptr::read(ptr) },
                        backing: ViewBacking::Heap32Medium, offset: u24(*offset), len: u24(*len), scan: *scan,
                    }, )*
                    $( Repr::$small_slice { ptr, offset, len, cap, scan } => RawOwned::View {
                        ptr: unsafe { std::ptr::read(ptr) },
                        backing: ViewBacking::Small { cap: u16v(*cap) }, offset: u16v(*offset), len: u16v(*len), scan: *scan,
                    }, )*
                    $( Repr::$far_slice { ptr, offset, len, scan } => RawOwned::View {
                        ptr: unsafe { std::ptr::read(ptr) },
                        backing: ViewBacking::Heap32Far, offset: u32v(*offset), len: u16v(*len), scan: *scan,
                    }, )*
                    $( Repr::$far_adopted { ptr, offset, len, scan } => RawOwned::View {
                        ptr: unsafe { std::ptr::read(ptr) },
                        backing: ViewBacking::AdoptedFar, offset: u32v(*offset), len: u16v(*len), scan: *scan,
                    }, )*
                    $( Repr::$adopted { ptr, offset, len, scan } => RawOwned::View {
                        ptr: unsafe { std::ptr::read(ptr) },
                        backing: ViewBacking::Adopted, offset: u24(*offset), len: u24(*len), scan: *scan,
                    }, )*
                }
            }

            /// A view's backing and absolute coordinates, borrowed for re-slice composition: no reference is
            /// taken, and the adopted whole-object sentinel resolves to real coordinates here.
            fn view_parts(&self) -> Option<(ViewBacking, std::ptr::NonNull<u8>, usize, usize)> {
                match &self.0 {
                    $( Repr::$slice { ptr, offset, len, .. } => Some((ViewBacking::Heap32Medium, ptr.as_ptr(), u24(*offset), u24(*len))), )*
                    $( Repr::$far_slice { ptr, offset, len, .. } => Some((ViewBacking::Heap32Far, ptr.as_ptr(), u32v(*offset), u16v(*len))), )*
                    $( Repr::$small_slice { ptr, offset, len, cap, .. } => Some((ViewBacking::Small { cap: u16v(*cap) }, ptr.as_ptr(), u16v(*offset), u16v(*len))), )*
                    $( Repr::$adopted { ptr, offset, len, .. } => Some({
                        // SAFETY: the handle holds a reference on the live struct.
                        let (off, n) = if u24(*offset) == SPAN as usize && u24(*len) == SPAN as usize {
                            (0, unsafe { ptr.as_ptr().cast::<cow_buffer::Adopted>().as_ref() }.total_len())
                        } else {
                            (u24(*offset), u24(*len))
                        };
                        (ViewBacking::Adopted, ptr.as_ptr(), off, n)
                    }), )*
                    $( Repr::$far_adopted { ptr, offset, len, .. } => Some((ViewBacking::AdoptedFar, ptr.as_ptr(), u32v(*offset), u16v(*len))), )*
                    _ => None,
                }
            }

            /// Rebuild a view with the given tag dimensions (backing reference preserved): the backing selects the
            /// family, and each family stores the fields at its own width, already proven under its bound.
            fn build_view(backing: ViewBacking, utf8: bool, tainted: bool, ptr: Owned, offset: usize, len: usize, scan: scan::ScanState) -> PString {
                match (backing, utf8, tainted) {
                    $( (ViewBacking::Heap32Medium, $slice_utf8, $slice_tainted) =>
                        PString(Repr::$slice { ptr, offset: to_u24(offset), len: to_u24(len), scan }), )*
                    $( (ViewBacking::Small { cap }, $small_slice_utf8, $small_slice_tainted) =>
                        PString(Repr::$small_slice { ptr, offset: to_u16(offset), len: to_u16(len), cap: to_u16(cap), scan }), )*
                    $( (ViewBacking::Adopted, $adopted_utf8, $adopted_tainted) =>
                        PString(Repr::$adopted { ptr, offset: to_u24(offset), len: to_u24(len), scan }), )*
                    $( (ViewBacking::Heap32Far, $far_slice_utf8, $far_slice_tainted) =>
                        PString(Repr::$far_slice { ptr, offset: to_u32(offset), len: to_u16(len), scan }), )*
                    $( (ViewBacking::AdoptedFar, $far_adopted_utf8, $far_adopted_tainted) =>
                        PString(Repr::$far_adopted { ptr, offset: to_u32(offset), len: to_u16(len), scan }), )*
                }
            }

            /// Rebuild a heap value with the given tag dimensions (buffer preserved).
            fn build_heap(utf8: bool, tainted: bool, parts: HeapParts) -> PString {
                // The one place the obligation leaves a `HeapParts` without releasing: the variant built below is its
                // next owner.  `E0509` forbids the plain destructure now that `HeapParts` owns a `Drop`, which is the
                // compiler confirming this transfer must be spelled out.
                let mut parts = std::mem::ManuallyDrop::new(parts);
                let ptr = parts.ptr.claim();
                let (len, cap, count, scan, tier) = (parts.len, parts.cap, parts.count, parts.scan, parts.tier);

                // SAFETY: `ptr` was claimed above and every field is `Copy`; the `ManuallyDrop` shell holds only a
                // disarmed `Owned` and is never dropped.
                let ptr = unsafe { Owned::from_raw(ptr) };
                match tier {
                    // The Ascii twins are the class-specific selection (§2.2.3): same tier, count and scan derivable,
                    // chosen whenever the settled state is Ascii.
                    Tier::Heap8 if scan == scan::Ascii => match (utf8, tainted) {
                        $( ($heap8a_utf8, $heap8a_tainted) => PString(Repr::$heap8a {
                            ptr, len: len as u8, cap: cap as u8,
                        }), )*
                    },
                    Tier::Heap16 if scan == scan::Ascii => match (utf8, tainted) {
                        $( ($heap16a_utf8, $heap16a_tainted) => PString(Repr::$heap16a {
                            ptr, len: len as u16, cap: cap as u16,
                        }), )*
                    },
                    Tier::Heap8 => match (utf8, tainted) {
                        $( ($heap8_utf8, $heap8_tainted) => PString(Repr::$heap8 {
                            ptr, len: len as u8, cap: cap as u8, count: count as u8,
                            scan: scan::Terminal::from_scan(scan),
                        }), )*
                    },
                    Tier::Heap16 => match (utf8, tainted) {
                        $( ($heap16_utf8, $heap16_tainted) => PString(Repr::$heap16 {
                            ptr, len: len as u16, cap: cap as u16, count: count as u16,
                            scan: scan::Terminal::from_scan(scan),
                        }), )*
                    },
                    Tier::Heap32 => match (utf8, tainted) {
                        $( ($heap32_utf8, $heap32_tainted) => PString(Repr::$heap32 { ptr, len: len as u32 }), )*
                    },
                    Tier::Heap => match (utf8, tainted) {
                        $( ($heap_utf8, $heap_tainted) => PString(Repr::$heap { ptr }), )*
                    },
                }
            }

            /// Rebuild an immortal value with the given tag dimensions (image untouched).
            fn build_immortal(
                utf8: bool, tainted: bool,
                ptr: std::ptr::NonNull<u8>, len: usize, count: usize, scan: scan::Terminal,
            ) -> PString {
                match (utf8, tainted) {
                    $( ($immortal_utf8, $immortal_tainted) =>
                        PString(Repr::$immortal { ptr: Image(ptr), len: u24_new(len), count: u24_new(count), scan }), )*
                }
            }

            /// Rebuild a large immortal value with the given tag dimensions (header shared, image untouched).
            fn build_large_immortal(utf8: bool, tainted: bool, head: &'static ImmortalHead) -> PString {
                match (utf8, tainted) {
                    $( ($large_immortal_utf8, $large_immortal_tainted) => PString(Repr::$large_immortal { head }), )*
                }
            }

            /// Rebuild a large static value with the given tag dimensions (header shared, image untouched).
            fn build_large_static(utf8: bool, tainted: bool, head: &'static ImmortalHead) -> PString {
                match (utf8, tainted) {
                    $( ($large_static_utf8, $large_static_tainted) => PString(Repr::$large_static { head }), )*
                }
            }

            /// Rebuild a static value with the given tag dimensions (image untouched).
            fn build_static(
                utf8: bool, tainted: bool,
                ptr: std::ptr::NonNull<u8>, len: usize, count: usize, scan: scan::Terminal,
            ) -> PString {
                match (utf8, tainted) {
                    $( ($static_utf8, $static_tainted) =>
                        PString(Repr::$static { ptr: Image(ptr), len: u24_new(len), count: u24_new(count), scan }), )*
                }
            }
        }
    };
}

define_perl_string! {
    inline: [
        InlineAscii                       = (Ascii,        InlineAscii,            false, false, false),
        InlineAsciiFlagged                = (Ascii,        InlineAscii,            false, true,  false),
        InlineAsciiTainted                = (Ascii,        InlineAscii,            false, false, true),
        InlineAsciiFlaggedTainted         = (Ascii,        InlineAscii,            false, true,  true),
        InlineAsciiFull                   = (Ascii,        InlineAsciiFull,        true,  false, false),
        InlineAsciiFullFlagged            = (Ascii,        InlineAsciiFull,        true,  true,  false),
        InlineAsciiFullTainted            = (Ascii,        InlineAsciiFull,        true,  false, true),
        InlineAsciiFullFlaggedTainted     = (Ascii,        InlineAsciiFull,        true,  true,  true),
        InlineLatin1                      = (Latin1,       InlineLatin1,           false, false, false),
        InlineLatin1Flagged               = (Latin1,       InlineLatin1,           false, true,  false),
        InlineLatin1Tainted               = (Latin1,       InlineLatin1,           false, false, true),
        InlineLatin1FlaggedTainted        = (Latin1,       InlineLatin1,           false, true,  true),
        InlineLatin1Full                  = (Latin1,       InlineLatin1Full,       true,  false, false),
        InlineLatin1FullFlagged           = (Latin1,       InlineLatin1Full,       true,  true,  false),
        InlineLatin1FullTainted           = (Latin1,       InlineLatin1Full,       true,  false, true),
        InlineLatin1FullFlaggedTainted    = (Latin1,       InlineLatin1Full,       true,  true,  true),
        InlineNonLatin1                   = (NonLatin1,    InlineNonLatin1,        false, false, false),
        InlineNonLatin1Flagged            = (NonLatin1,    InlineNonLatin1,        false, true,  false),
        InlineNonLatin1Tainted            = (NonLatin1,    InlineNonLatin1,        false, false, true),
        InlineNonLatin1FlaggedTainted     = (NonLatin1,    InlineNonLatin1,        false, true,  true),
        InlineNonLatin1Full               = (NonLatin1,    InlineNonLatin1Full,    true,  false, false),
        InlineNonLatin1FullFlagged        = (NonLatin1,    InlineNonLatin1Full,    true,  true,  false),
        InlineNonLatin1FullTainted        = (NonLatin1,    InlineNonLatin1Full,    true,  false, true),
        InlineNonLatin1FullFlaggedTainted = (NonLatin1,    InlineNonLatin1Full,    true,  true,  true),
        InlineExtended                    = (Extended,     InlineExtended,         false, false, false),
        InlineExtendedFlagged             = (Extended,     InlineExtended,         false, true,  false),
        InlineExtendedTainted             = (Extended,     InlineExtended,         false, false, true),
        InlineExtendedFlaggedTainted      = (Extended,     InlineExtended,         false, true,  true),
        InlineExtendedFull                = (Extended,     InlineExtendedFull,     true,  false, false),
        InlineExtendedFullFlagged         = (Extended,     InlineExtendedFull,     true,  true,  false),
        InlineExtendedFullTainted         = (Extended,     InlineExtendedFull,     true,  false, true),
        InlineExtendedFullFlaggedTainted  = (Extended,     InlineExtendedFull,     true,  true,  true),
        InlineBytes                       = (Bytes,        InlineBytes,            false, false, false),
        InlineBytesFlagged                = (Bytes,        InlineBytes,            false, true,  false),
        InlineBytesTainted                = (Bytes,        InlineBytes,            false, false, true),
        InlineBytesFlaggedTainted         = (Bytes,        InlineBytes,            false, true,  true),
        InlineBytesFull                   = (Bytes,        InlineBytesFull,        true,  false, false),
        InlineBytesFullFlagged            = (Bytes,        InlineBytesFull,        true,  true,  false),
        InlineBytesFullTainted            = (Bytes,        InlineBytesFull,        true,  false, true),
        InlineBytesFullFlaggedTainted     = (Bytes,        InlineBytesFull,        true,  true,  true),
    ],
    packed: [
        PackedNum                         = (Numeric,      PackedNumeric,          false, false, false),
        PackedNumFlagged                  = (Numeric,      PackedNumeric,          false, true,  false),
        PackedNumTainted                  = (Numeric,      PackedNumeric,          false, false, true),
        PackedNumFlaggedTainted           = (Numeric,      PackedNumeric,          false, true,  true),
        PackedNumFull                     = (Numeric,      PackedNumericFull,      true,  false, false),
        PackedNumFullFlagged              = (Numeric,      PackedNumericFull,      true,  true,  false),
        PackedNumFullTainted              = (Numeric,      PackedNumericFull,      true,  false, true),
        PackedNumFullFlaggedTainted       = (Numeric,      PackedNumericFull,      true,  true,  true),
        PackedPlus                        = (DateTimePlus, PackedDateTimePlus,     false, false, false),
        PackedPlusFlagged                 = (DateTimePlus, PackedDateTimePlus,     false, true,  false),
        PackedPlusTainted                 = (DateTimePlus, PackedDateTimePlus,     false, false, true),
        PackedPlusFlaggedTainted          = (DateTimePlus, PackedDateTimePlus,     false, true,  true),
        PackedPlusFull                    = (DateTimePlus, PackedDateTimePlusFull, true,  false, false),
        PackedPlusFullFlagged             = (DateTimePlus, PackedDateTimePlusFull, true,  true,  false),
        PackedPlusFullTainted             = (DateTimePlus, PackedDateTimePlusFull, true,  false, true),
        PackedPlusFullFlaggedTainted      = (DateTimePlus, PackedDateTimePlusFull, true,  true,  true),
        PackedZulu                        = (DateTimeZulu, PackedDateTimeZulu,     false, false, false),
        PackedZuluFlagged                 = (DateTimeZulu, PackedDateTimeZulu,     false, true,  false),
        PackedZuluTainted                 = (DateTimeZulu, PackedDateTimeZulu,     false, false, true),
        PackedZuluFlaggedTainted          = (DateTimeZulu, PackedDateTimeZulu,     false, true,  true),
        PackedZuluFull                    = (DateTimeZulu, PackedDateTimeZuluFull, true,  false, false),
        PackedZuluFullFlagged             = (DateTimeZulu, PackedDateTimeZuluFull, true,  true,  false),
        PackedZuluFullTainted             = (DateTimeZulu, PackedDateTimeZuluFull, true,  false, true),
        PackedZuluFullFlaggedTainted      = (DateTimeZulu, PackedDateTimeZuluFull, true,  true,  true),
    ],
    uuids: [
        PackedUuidV1                      = (V1   , PackedUuidV1   , false, false),
        PackedUuidV1Flagged               = (V1   , PackedUuidV1   , true,  false),
        PackedUuidV1Tainted               = (V1   , PackedUuidV1   , false, true),
        PackedUuidV1FlaggedTainted        = (V1   , PackedUuidV1   , true,  true),
        PackedUuidV3S0                    = (V3S0 , PackedUuidV3S0 , false, false),
        PackedUuidV3S0Flagged             = (V3S0 , PackedUuidV3S0 , true,  false),
        PackedUuidV3S0Tainted             = (V3S0 , PackedUuidV3S0 , false, true),
        PackedUuidV3S0FlaggedTainted      = (V3S0 , PackedUuidV3S0 , true,  true),
        PackedUuidV3S1                    = (V3S1 , PackedUuidV3S1 , false, false),
        PackedUuidV3S1Flagged             = (V3S1 , PackedUuidV3S1 , true,  false),
        PackedUuidV3S1Tainted             = (V3S1 , PackedUuidV3S1 , false, true),
        PackedUuidV3S1FlaggedTainted      = (V3S1 , PackedUuidV3S1 , true,  true),
        PackedUuidV3S2                    = (V3S2 , PackedUuidV3S2 , false, false),
        PackedUuidV3S2Flagged             = (V3S2 , PackedUuidV3S2 , true,  false),
        PackedUuidV3S2Tainted             = (V3S2 , PackedUuidV3S2 , false, true),
        PackedUuidV3S2FlaggedTainted      = (V3S2 , PackedUuidV3S2 , true,  true),
        PackedUuidV3S3                    = (V3S3 , PackedUuidV3S3 , false, false),
        PackedUuidV3S3Flagged             = (V3S3 , PackedUuidV3S3 , true,  false),
        PackedUuidV3S3Tainted             = (V3S3 , PackedUuidV3S3 , false, true),
        PackedUuidV3S3FlaggedTainted      = (V3S3 , PackedUuidV3S3 , true,  true),
        PackedUuidV4S0                    = (V4S0 , PackedUuidV4S0 , false, false),
        PackedUuidV4S0Flagged             = (V4S0 , PackedUuidV4S0 , true,  false),
        PackedUuidV4S0Tainted             = (V4S0 , PackedUuidV4S0 , false, true),
        PackedUuidV4S0FlaggedTainted      = (V4S0 , PackedUuidV4S0 , true,  true),
        PackedUuidV4S1                    = (V4S1 , PackedUuidV4S1 , false, false),
        PackedUuidV4S1Flagged             = (V4S1 , PackedUuidV4S1 , true,  false),
        PackedUuidV4S1Tainted             = (V4S1 , PackedUuidV4S1 , false, true),
        PackedUuidV4S1FlaggedTainted      = (V4S1 , PackedUuidV4S1 , true,  true),
        PackedUuidV4S2                    = (V4S2 , PackedUuidV4S2 , false, false),
        PackedUuidV4S2Flagged             = (V4S2 , PackedUuidV4S2 , true,  false),
        PackedUuidV4S2Tainted             = (V4S2 , PackedUuidV4S2 , false, true),
        PackedUuidV4S2FlaggedTainted      = (V4S2 , PackedUuidV4S2 , true,  true),
        PackedUuidV4S3                    = (V4S3 , PackedUuidV4S3 , false, false),
        PackedUuidV4S3Flagged             = (V4S3 , PackedUuidV4S3 , true,  false),
        PackedUuidV4S3Tainted             = (V4S3 , PackedUuidV4S3 , false, true),
        PackedUuidV4S3FlaggedTainted      = (V4S3 , PackedUuidV4S3 , true,  true),
        PackedUuidV5S0                    = (V5S0 , PackedUuidV5S0 , false, false),
        PackedUuidV5S0Flagged             = (V5S0 , PackedUuidV5S0 , true,  false),
        PackedUuidV5S0Tainted             = (V5S0 , PackedUuidV5S0 , false, true),
        PackedUuidV5S0FlaggedTainted      = (V5S0 , PackedUuidV5S0 , true,  true),
        PackedUuidV5S1                    = (V5S1 , PackedUuidV5S1 , false, false),
        PackedUuidV5S1Flagged             = (V5S1 , PackedUuidV5S1 , true,  false),
        PackedUuidV5S1Tainted             = (V5S1 , PackedUuidV5S1 , false, true),
        PackedUuidV5S1FlaggedTainted      = (V5S1 , PackedUuidV5S1 , true,  true),
        PackedUuidV5S2                    = (V5S2 , PackedUuidV5S2 , false, false),
        PackedUuidV5S2Flagged             = (V5S2 , PackedUuidV5S2 , true,  false),
        PackedUuidV5S2Tainted             = (V5S2 , PackedUuidV5S2 , false, true),
        PackedUuidV5S2FlaggedTainted      = (V5S2 , PackedUuidV5S2 , true,  true),
        PackedUuidV5S3                    = (V5S3 , PackedUuidV5S3 , false, false),
        PackedUuidV5S3Flagged             = (V5S3 , PackedUuidV5S3 , true,  false),
        PackedUuidV5S3Tainted             = (V5S3 , PackedUuidV5S3 , false, true),
        PackedUuidV5S3FlaggedTainted      = (V5S3 , PackedUuidV5S3 , true,  true),
        PackedUuidV6                      = (V6   , PackedUuidV6   , false, false),
        PackedUuidV6Flagged               = (V6   , PackedUuidV6   , true,  false),
        PackedUuidV6Tainted               = (V6   , PackedUuidV6   , false, true),
        PackedUuidV6FlaggedTainted        = (V6   , PackedUuidV6   , true,  true),
        PackedUuidV7                      = (V7   , PackedUuidV7   , false, false),
        PackedUuidV7Flagged               = (V7   , PackedUuidV7   , true,  false),
        PackedUuidV7Tainted               = (V7   , PackedUuidV7   , false, true),
        PackedUuidV7FlaggedTainted        = (V7   , PackedUuidV7   , true,  true),
    ],
    hexes: [
        PackedHexBytes                    = (PackedHexBytes, false, false),
        PackedHexBytesFlagged             = (PackedHexBytes, true,  false),
        PackedHexBytesTainted             = (PackedHexBytes, false, true),
        PackedHexBytesFlaggedTainted      = (PackedHexBytes, true,  true),
    ],
    heap8: [
        Heap8                             = (false, false),
        Heap8Flagged                      = (true,  false),
        Heap8Tainted                      = (false, true),
        Heap8FlaggedTainted               = (true,  true),
    ],
    heap8_ascii: [
        Heap8Ascii                        = (false, false),
        Heap8AsciiFlagged                 = (true,  false),
        Heap8AsciiTainted                 = (false, true),
        Heap8AsciiFlaggedTainted          = (true,  true),
    ],
    heap16: [
        Heap16                            = (false, false),
        Heap16Flagged                     = (true,  false),
        Heap16Tainted                     = (false, true),
        Heap16FlaggedTainted              = (true,  true),
    ],
    heap16_ascii: [
        Heap16Ascii                       = (false, false),
        Heap16AsciiFlagged                = (true,  false),
        Heap16AsciiTainted                = (false, true),
        Heap16AsciiFlaggedTainted         = (true,  true),
    ],
    heap32: [
        Heap32                            = (false, false),
        Heap32Flagged                     = (true,  false),
        Heap32Tainted                     = (false, true),
        Heap32FlaggedTainted              = (true,  true),
    ],
    heap: [
        Heap                              = (false, false),
        HeapFlagged                       = (true,  false),
        HeapTainted                       = (false, true),
        HeapFlaggedTainted                = (true,  true),
    ],
    immortal: [
        Immortal                          = (false, false),
        ImmortalFlagged                   = (true,  false),
        ImmortalTainted                   = (false, true),
        ImmortalFlaggedTainted            = (true,  true),
    ],
    statics: [
        Static                            = (false, false),
        StaticFlagged                     = (true,  false),
        StaticTainted                     = (false, true),
        StaticFlaggedTainted              = (true,  true),
    ],
    large_immortal: [
        LargeImmortal                     = (false, false),
        LargeImmortalFlagged              = (true,  false),
        LargeImmortalTainted              = (false, true),
        LargeImmortalFlaggedTainted       = (true,  true),
    ],
    large_statics: [
        LargeStatic                       = (false, false),
        LargeStaticFlagged                = (true,  false),
        LargeStaticTainted                = (false, true),
        LargeStaticFlaggedTainted         = (true,  true),
    ],
    slices: [
        MediumSlice                       = (false, false),
        MediumSliceFlagged                = (true,  false),
        MediumSliceTainted                = (false, true),
        MediumSliceFlaggedTainted         = (true,  true),
    ],
    small_slices: [
        SmallSlice                        = (false, false),
        SmallSliceFlagged                 = (true,  false),
        SmallSliceTainted                 = (false, true),
        SmallSliceFlaggedTainted          = (true,  true),
    ],
    far_slices: [
        FarSlice                          = (false, false),
        FarSliceFlagged                   = (true,  false),
        FarSliceTainted                   = (false, true),
        FarSliceFlaggedTainted            = (true,  true),
    ],
    far_adopteds: [
        FarAdopted                        = (false, false),
        FarAdoptedFlagged                 = (true,  false),
        FarAdoptedTainted                 = (false, true),
        FarAdoptedFlaggedTainted          = (true,  true),
    ],
    adopteds: [
        AdoptedView                       = (false, false),
        AdoptedViewFlagged                = (true,  false),
        AdoptedViewTainted                = (false, true),
        AdoptedViewFlaggedTainted         = (true,  true),
    ]
}

// The ledger, checked: forty-seven storage types times the two flag bits (§2.2.3, §2.2.9).  Growing the list moves this
// number and the design's ledgers in the same commit.
const _: () = assert!(REPR_VARIANT_COUNT == 188);

// ── Layout law (§2.3.6) ───────────────────────────────────────────
const _: () = assert!(size_of::<PString>() == 16);
const _: () = assert!(size_of::<Option<PString>>() == 16);

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

/// Allocate heap parts for appended content, classified by the transition where that suffices.
///
/// The append lattice (§2.2.5) can answer `UNKNOWN` — a raw-byte append, or growth onto content whose validity was
/// never established — and a large tier records that and moves on.  A small tier cannot: its envelope holds only the
/// terminal type, so an indeterminate transition means paying the construction-grade pass over the joined content.  The
/// same eager rule construction follows, for the same reason: below 64 KiB the state is settled now or never (§2.2.3).
fn heap_parts_transitioned(bytes: &[u8], state: scan::ScanState, chars: usize) -> Result<HeapParts, AllocError> {
    if !scan::is_terminal(state) {
        return heap_parts_classified(bytes);
    }
    HeapParts::from_slice(bytes, state, chars)
}

/// Allocate heap parts for `bytes`, classifying eagerly where the tier keeps its scan state in the envelope.
///
/// §2.2.3 pairs the small tiers with eager classification, and the two go together necessarily: with no scan byte in
/// the allocation there is nowhere to record a later discovery, so a small tier that were born unknown would rescan on
/// every read.  The pass costs at most ~117 ns at `Heap8` and, for ASCII content of any size, less than the copy that
/// just happened (§2.2.11).  The large tiers are left unknown deliberately — at those sizes the pass is what they
/// cannot afford, and their allocation has the room to record what a reader discovers.
fn heap_parts_classified(bytes: &[u8]) -> Result<HeapParts, AllocError> {
    // Classification rides the copy at every size (§2.2.3): one traversal classifies and emits, so the buffer is born
    // settled without a pass the copy was not already paying for.
    HeapParts::from_slice_classifying(bytes, |dst, src| {
        // SAFETY: `dst` is a fresh allocation with room for `src.len()`, disjoint from `src` by construction.
        let (terminal, chars) = unsafe { classify_into(dst, src) };
        (terminal.widen(), chars)
    })
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
        (scan::Terminal::Utf8NonLatin1, chars) => (InlineClass::NonLatin1, chars),
        (scan::Terminal::ExtendedUtf8, chars) => (InlineClass::Extended, chars),

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

impl PString {
    /// Construct from a Rust `&str`.  ASCII content is stored unflagged (the canonical downgraded form, §2.3.5);
    /// non-ASCII content is stored with the utf8 flag, its validity known from the type.  Allocation failure is the
    /// only error.
    ///
    /// Generic at the boundary so that embedders holding a `String`, a `Cow`, or one of the compact string types from
    /// the ecosystem need no conversion; the ladder beneath is monomorphic and instantiated once.
    pub fn new(s: impl AsRef<str>) -> Result<PString, AllocError> {
        let s = s.as_ref();

        if let Some(inline) = PString::inline(s) {
            Ok(inline)
        } else {
            let bytes = s.as_bytes();

            // Known-valid input classifies through the cheaper ranging walker, fused with the copy (§2.2.3) at every
            // size, so a `&str` birth is settled at its exact range with its character count in the one traversal the
            // copy already pays — and the ASCII question is answered by the settled state for free, where probing first
            // would scan the same bytes twice for one fact.
            let parts = HeapParts::from_slice_classifying(bytes, |dst, src| {
                // SAFETY: `dst` is a fresh allocation with room for `src.len()`, disjoint from `src` by construction.
                let (class, chars) = unsafe { classify_known_valid_into(dst, src) };
                (class.widen(), chars)
            })?;
            let ascii = parts.scan == scan::Ascii;

            Ok(PString::build_heap(!ascii, false, parts))
        }
    }

    /// Construct from raw bytes (I/O, `Encode`, lexer literals).  Unflagged; inline content gets its eager terminal
    /// scan, heap content defers all scanning (`UNKNOWN`), per §2.2.7.
    pub fn from_bytes(bytes: impl AsRef<[u8]>) -> Result<PString, AllocError> {
        let bytes = bytes.as_ref();
        match PString::inline_bytes(bytes) {
            Some(inline) => Ok(inline),
            None => Ok(PString::build_heap(false, false, heap_parts_classified(bytes)?)),
        }
    }

    /// A string over `'static` UTF-8: zero-copy, never freed (§2.2.3).  Classification is eager and terminal at any
    /// size — the full walk rather than the cheaper ranging one, because facts settled once for content that lives
    /// forever should include the character count the ranging walker forfeits.  Below the compact ceiling this
    /// allocates nothing; past it, the one allocation is the shared, deliberately leaked side header.
    pub fn from_static_str(s: &'static str) -> Result<PString, AllocError> {
        PString::from_static_bytes(s.as_bytes())
    }

    /// A string over `'static` bytes: zero-copy, never freed (§2.2.3).  Classification is eager and terminal at any
    /// size, malformed content included — a static image can hold any bytes the program does.  Below the compact
    /// ceiling this allocates nothing; past it, the one allocation is the shared, deliberately leaked side header.
    pub fn from_static_bytes(bytes: &'static [u8]) -> Result<PString, AllocError> {
        let (scan, count) = classify_full(bytes);

        // A slice pointer is never null, so the fallback arm is unreachable; it exists to keep this path panic-free,
        // and a dangling pointer with length zero reads soundly anyway.
        let ptr = std::ptr::NonNull::new(bytes.as_ptr().cast_mut()).unwrap_or(std::ptr::NonNull::dangling());

        if bytes.len() > U24_MAX {
            let head = ImmortalHead::leaked(Image(ptr), bytes.len(), count, scan)?;
            return Ok(PString::build_large_static(false, false, head));
        }

        Ok(PString::build_static(false, false, ptr, bytes.len(), count, scan))
    }

    /// A string over an immortal image: bytes some owner keeps alive longer than every handle (§2.2.3) — the
    /// interpreter's slab canonically; any arena or interner legitimately.  Zero-copy; classification is eager and
    /// terminal.  Past the compact ceiling the one allocation is the shared, deliberately leaked side header.
    ///
    /// # Safety
    /// The caller warrants that the image outlives every handle, clones included; that it is never written while any
    /// handle lives; and that its owner frees it only after the last handle is gone.
    #[cfg_attr(not(test), expect(dead_code, reason = "the §2.4 slab is the production caller; until it lands, tests are"))]
    pub(crate) unsafe fn from_immortal_bytes(bytes: &[u8]) -> Result<PString, AllocError> {
        let (scan, count) = classify_full(bytes);

        // See from_static_bytes: never null, never panics, sound at zero length.
        let ptr = std::ptr::NonNull::new(bytes.as_ptr().cast_mut()).unwrap_or(std::ptr::NonNull::dangling());

        if bytes.len() > U24_MAX {
            let head = ImmortalHead::leaked(Image(ptr), bytes.len(), count, scan)?;
            return Ok(PString::build_large_immortal(false, false, head));
        }

        Ok(PString::build_immortal(false, false, ptr, bytes.len(), count, scan))
    }

    /// The full tier ladder with every tag dimension supplied — the transforms' constructor, and `from_bytes` in
    /// spirit: compressed inline, verbatim inline, packed, heap, in the ruled order (§2.2.9).  Internal: public
    /// construction fixes the flags.
    fn tiered(bytes: &[u8], utf8: bool, tainted: bool) -> Result<PString, AllocError> {
        // The envelope ladder is only worth attempting within the ceiling; longer content takes the classified
        // heap constructor directly, and `pack`'s own band precondition backstops this in debug builds.
        debug_assert!(bytes.len() <= DECODE_MAX, "the envelope ladder serves lengths the envelope can possibly represent");
        if let Some((class, stored, aux, buf)) = classify_inline(bytes) {
            return Ok(PString::build_inline(class, utf8, tainted, stored, aux, buf));
        }
        if (MIN_PACKED_LEN..=MAX_PACKED_LEN).contains(&bytes.len())
            && let Some(p) = pack(bytes)
        {
            return Ok(PString::build_packed(p, utf8, tainted));
        }

        if let Some((form, payload)) = classify_uuid(bytes) {
            return Ok(PString::build_uuid(form, payload, utf8, tainted));
        }

        if let Some(payload) = classify_hex_bytes(bytes) {
            return Ok(PString::build_hex(payload, utf8, tainted));
        }
        Ok(PString::build_heap(utf8, tainted, heap_parts_classified(bytes)?))
    }

    /// The empty string: inline, unflagged, trivially ASCII.  Infallible, unlike the other constructors — an empty
    /// payload needs no allocation — which is also what lets `Default` exist.
    pub fn empty() -> PString {
        PString::build_inline(InlineClass::Ascii, false, false, 0, 0, [0u8; INLINE_MAX])
    }

    /// Construct from a Rust `&str` **without allocating**, or `None` if the content cannot be stored in the value
    /// itself.  Flagging follows [`FromStr`]: ASCII stores unflagged, non-ASCII flagged.
    ///
    /// The contract is the guarantee, not a byte count: `Some` means no heap allocation occurred, so the set of
    /// accepted content widens whenever the non-allocating storage forms do.  Callers who merely prefer inline storage
    /// can write `PString::inline(s).unwrap_or_default()`; callers who need the content stored either way should use
    /// the fallible constructors instead.
    pub fn inline(s: impl AsRef<str>) -> Option<PString> {
        let bytes = s.as_ref().as_bytes();

        // The inline rungs come first (§2.2.9's ladder), and they now reach 16-30-byte Latin-1-compressible content:
        // the accepted set has widened, exactly as the guarantee-not-a-count contract promises.
        if let Some((class, stored, aux, buf)) = classify_inline(bytes) {
            // Ascii stores unflagged, non-ASCII flagged, following `FromStr`.  Bytes/Extended are impossible from a
            // `&str`, whose bytes are Rust-valid.
            return Some(PString::build_inline(class, class != InlineClass::Ascii, false, stored, aux, buf));
        }

        // The packed band holds 16-30-character alphabet content and allocates nothing either.
        if (MIN_PACKED_LEN..=MAX_PACKED_LEN).contains(&bytes.len())
            && let Some(p) = pack(bytes)
        {
            return Some(PString::build_packed(p, false, false));
        }

        // The §2.2.16 identifier forms allocate nothing either.  Past every envelope reach, `None` starts meaning
        // "the heap".
        if let Some((form, payload)) = classify_uuid(bytes) {
            return Some(PString::build_uuid(form, payload, false, false));
        }

        classify_hex_bytes(bytes).map(|payload| PString::build_hex(payload, false, false))
    }

    /// Construct from raw bytes **without allocating**, or `None` if the content cannot be stored in the value itself.
    /// Unflagged, like [`PString::from_bytes`]; the same guarantee-not-a-count contract as [`PString::inline`].
    pub fn inline_bytes(bytes: impl AsRef<[u8]>) -> Option<PString> {
        let bytes = bytes.as_ref();

        if let Some((class, stored, aux, buf)) = classify_inline(bytes) {
            return Some(PString::build_inline(class, false, false, stored, aux, buf));
        }

        if (MIN_PACKED_LEN..=MAX_PACKED_LEN).contains(&bytes.len())
            && let Some(p) = pack(bytes)
        {
            return Some(PString::build_packed(p, false, false));
        }

        if let Some((form, payload)) = classify_uuid(bytes) {
            return Some(PString::build_uuid(form, payload, false, false));
        }

        classify_hex_bytes(bytes).map(|payload| PString::build_hex(payload, false, false))
    }

    // ── Accessors ─────────────────────────────────────────────────
    /// Length in bytes.  No dereference for inline; handle mirror for heap.
    pub fn len(&self) -> usize {
        match self.raw_parts() {
            RawParts::Inline { class, full, buf } => inline_internal_len(class, full, buf),
            RawParts::Packed(p) => p.len(),
            RawParts::Uuid { .. } => UUID_LEN,
            RawParts::Hex { payload } => hex_rendered_len(payload),
            RawParts::Heap(cb) => cb.len(),
            RawParts::Borrowed { bytes, .. } | RawParts::View { bytes, .. } => bytes.len(),
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
            RawParts::Uuid { form, payload } => {
                let n = decode_uuid(form, payload, scratch);
                &scratch[..n]
            }
            RawParts::Hex { payload } => {
                let n = decode_hex_bytes(payload, scratch);
                &scratch[..n]
            }
            RawParts::Heap(cb) => cb.as_slice(),
            RawParts::Borrowed { bytes, .. } | RawParts::View { bytes, .. } => bytes,
        }
    }

    /// View as a Rust `&str` if the bytes are valid UTF-8 (a fact question, independent of the Perl flag).  Narrows the
    /// heap scan lattice as a side effect (§2.2.5); sound through `&self`.
    pub fn as_str<'a>(&'a self, scratch: &'a mut [u8; DECODE_MAX]) -> Option<&'a str> {
        match self.raw_parts() {
            RawParts::Uuid { form, payload } => {
                // The canonical spelling is ASCII, so the decoded bytes are always valid.
                let n = decode_uuid(form, payload, scratch);

                // SAFETY: ASCII by construction.
                Some(unsafe { str::from_utf8_unchecked(&scratch[..n]) })
            }
            RawParts::Hex { payload } => {
                // Hex digits, separators, and the prefix are ASCII, so the decoded bytes are always valid.
                let n = decode_hex_bytes(payload, scratch);

                // SAFETY: ASCII by construction.
                Some(unsafe { str::from_utf8_unchecked(&scratch[..n]) })
            }
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
            RawParts::Borrowed { bytes, scan, .. } => match scan.widen() {
                // SAFETY: immortal facts are settled at construction and the image is readonly (§2.2.3), so the
                // certification can never go stale.
                st if scan::is_rust_valid(st) => Some(unsafe { str::from_utf8_unchecked(bytes) }),
                _ => None, // ExtendedUtf8 or MalformedUtf8: the only terminal states outside the Rust-valid set.
            },
            RawParts::View { bytes, scan, backing } => match backing.map_or(scan, |a| scan::meet(scan, a.scan())) {
                // SAFETY: the envelope byte certifies the view's own bytes (born from the slice-birth table, only ever
                // narrowed per handle), met with the struct's slot for a whole-object adopted view, whose facts are the
                // object's facts.
                st if scan::is_rust_valid(st) => Some(unsafe { str::from_utf8_unchecked(bytes) }),
                scan::MalformedUtf8 | scan::ExtendedUtf8 | scan::PerlValidNonAscii => None,
                _ => {
                    // Undecided: classify, answer, and record the certification in the shared slot where one exists —
                    // the envelope byte itself cannot move through `&self`.
                    let (st, _) = classify_full(bytes);
                    if let Some(a) = backing {
                        a.narrow_scan(st.widen());
                    }

                    // SAFETY: the classification just certified these exact bytes.
                    if scan::is_rust_valid(st.widen()) { Some(unsafe { str::from_utf8_unchecked(bytes) }) } else { None }
                }
            },
            RawParts::Heap(cb) => {
                let bytes = cb.as_slice();
                match cb.scan() {
                    // SAFETY: these lattice states certify prior successful validation of these exact bytes (states
                    // only narrow; mutation resets to UNKNOWN).
                    st if scan::is_rust_valid(st) => Some(unsafe { str::from_utf8_unchecked(bytes) }),
                    scan::MalformedUtf8 | scan::ExtendedUtf8 => None,
                    _ => {
                        let (st, chars) = classify_full(bytes); // one pass: validity (both tiers) + range + count
                        let st = st.widen();
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
            RawParts::Uuid { .. } => true,
            RawParts::Hex { .. } => true,
            RawParts::View { bytes, scan, backing } => match backing.map_or(scan, |a| scan::meet(scan, a.scan())) {
                scan::Ascii => true,
                st if scan::is_known_non_ascii(st) => false,

                // Undecided: probe, and record the answer in the shared slot where one exists — the envelope byte
                // itself cannot move through `&self`.
                _ => {
                    count_probe_byte();
                    let ascii = bytes.iter().all(u8::is_ascii);
                    if let Some(a) = backing
                        && ascii
                    {
                        a.narrow_scan(scan::Ascii);
                    }

                    ascii
                }
            },
            RawParts::Heap(cb) => match cb.scan() {
                scan::Ascii => true,
                scan::Utf8Latin1 | scan::Utf8NonLatin1 | scan::Utf8NonAscii | scan::MalformedUtf8 | scan::NonAscii | scan::ExtendedUtf8 => false,
                scan::ValidUtf8 => {
                    // Cheap probe: bail at the first high bit; range stays deferred (§2.2.4/§2.2.5).
                    let ascii = cb.as_slice().iter().all(|b| {
                        count_probe_byte();
                        b.is_ascii()
                    });

                    cb.narrow_scan(if ascii { scan::Ascii } else { scan::Utf8NonAscii });

                    ascii
                }
                _ => {
                    let ascii = cb.as_slice().iter().all(|b| {
                        count_probe_byte();
                        b.is_ascii()
                    });

                    cb.narrow_scan(if ascii { scan::Ascii } else { scan::NonAscii });

                    ascii
                }
            },

            // Settled at construction: the terminal state answers, no probe and no narrowing.
            RawParts::Borrowed { scan, .. } => scan == scan::Terminal::Ascii,
        }
    }

    /// The current scan state in the heap encoding (§2.2.4), inline terminals mapped through.  Reads existing knowledge
    /// only; performs no scan.
    fn scan_state(&self) -> scan::ScanState {
        match self.raw_parts() {
            RawParts::Inline { .. } => match self.inline_class() {
                Some(st) => inline_scan_to_heap(st),
                None => scan::Unknown, // unreachable by construction
            },
            RawParts::Packed(_) => scan::Ascii,
            RawParts::Uuid { .. } => scan::Ascii,
            RawParts::Hex { .. } => scan::Ascii,
            RawParts::Heap(cb) => cb.scan(),
            RawParts::Borrowed { scan, .. } => scan.widen(),
            RawParts::View { scan, .. } => scan,
        }
    }

    /// Whether the bytes are shared with another handle — a heap tier holding more than one reference.  Inline and
    /// packed values own their bytes in the envelope and are never shared.
    pub fn is_shared(&self) -> bool {
        match self.raw_parts() {
            RawParts::Heap(view) => !view.is_unique(),
            RawParts::Inline { .. } | RawParts::Packed(_) | RawParts::Uuid { .. } | RawParts::Hex { .. } => false,

            // Bitwise clones do share the image, but with the owner that outlives them all (§2.2.3), not with each
            // other in any sense `unshare` could dissolve: copying frees nothing that would otherwise be freed.
            RawParts::Borrowed { .. } => false,

            // Sharing the backing is a view's purpose: even a sole handle pins bytes it does not own, which is exactly
            // what `unshare` exists to dissolve.
            RawParts::View { .. } => true,
        }
    }

    /// A whole-object view of an adopted backing (§2.2.15): `SPAN` in both fields, the struct's shared slot as the
    /// birth state.  Takes ownership of one reference the caller holds on `adopted` — the one `mint` returns with.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn adopted_whole(adopted: std::ptr::NonNull<cow_buffer::Adopted>, utf8: bool, tainted: bool) -> PString {
        // SAFETY: the caller's reference keeps the struct live for this read and transfers into the envelope below.
        let (scan, ptr) = unsafe { (adopted.as_ref().scan(), Owned::from_raw(adopted.cast())) };
        PString::build_view(ViewBacking::Adopted, utf8, tainted, ptr, SPAN as usize, SPAN as usize, scan)
    }

    /// A zero-copy sub-view of this value where the representation admits one (§2.2.15): the Heap32 tier birthing the
    /// native form, the small tiers birthing the capacity-carrying small form.  `None` where the storage has no compact
    /// view form, the range escapes the value, or a field would overflow its width: every fallback (the word tier
    /// through `Adopted`, re-slicing views, the copy forms and floors) is the verbs stage's, which owns the policy-free
    /// `slice`/`substr` surface.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn view_range(&self, offset: usize, len: usize) -> Option<PString> {
        let (utf8, tainted) = (self.is_utf8(), self.is_tainted());
        match self.raw_parts() {
            RawParts::Heap(cb) if cb.tier() == Tier::Heap32 => {
                let bytes = cb.as_slice();
                if offset.checked_add(len)? > bytes.len() {
                    return None;
                }

                // The ruled selection (§2.2.15): the far form whenever the length fits u16 — its fields are native
                // widths, one load each, where u24 has none — and the medium form for the band only it serves, lengths
                // past 64 KiB within u24 reach.  The large forms, the verbs stage's, lie past both.
                let backing = if len <= u16::MAX as usize {
                    ViewBacking::Heap32Far
                } else if offset < SPAN as usize && len < SPAN as usize {
                    ViewBacking::Heap32Medium
                } else {
                    return None;
                };

                let scan = view_birth_state(cb.scan(), bytes, offset, len);

                // SAFETY: `cb` proves a live Heap32 allocation; the envelope built below owns the reference this retain
                // adds and releases it under the same tier in Drop.
                let ptr = unsafe {
                    cow_buffer::heap32::retain(cb.raw());
                    Owned::from_raw(cb.raw())
                };

                Some(PString::build_view(backing, utf8, tainted, ptr, offset, len, scan))
            }
            RawParts::Heap(cb) if matches!(cb.tier(), Tier::Heap8 | Tier::Heap16) => {
                let bytes = cb.as_slice();
                if offset.checked_add(len)? > bytes.len() {
                    return None;
                }

                // The tier's ceilings prove the u16 fields; the strict ladder makes the capacity the release dispatch
                // (§2.2.15).
                let cap = cb.capacity();
                debug_assert!(bytes.len() <= cow_buffer::heap16::MAX_CAPACITY && cap <= cow_buffer::heap16::MAX_CAPACITY);
                debug_assert!((cap <= cow_buffer::heap8::MAX_CAPACITY) == (cb.tier() == Tier::Heap8), "the ladder's strictness is the dispatch");

                let scan = view_birth_state(cb.scan(), bytes, offset, len);

                // SAFETY: `cb` proves a live small-tier allocation whose tier the capacity names; the envelope owns the
                // reference this retain adds and releases it under the same dispatch in Drop.
                let ptr = unsafe {
                    small_backing_retain(cb.raw(), cap);
                    Owned::from_raw(cb.raw())
                };

                Some(PString::build_view(ViewBacking::Small { cap }, utf8, tainted, ptr, offset, len, scan))
            }
            _ => None,
        }
    }

    /// The sharing verb (§2.2.15): a zero-copy view of `offset..offset + len`, except content representable in an
    /// inline or packed form returns that form instead — a copy costing zero allocation, zero refcount traffic, and
    /// zero pin dominates a view on every axis, which is form selection, not policy.  Sources with no shareable
    /// backing answer in kind: envelope-resident content materializes owned, and an immortal image yields another
    /// immortal envelope over the same image.  The range clamps as perl's own rvalue `substr` clamps — an offset
    /// past the end is the empty string, a length past the end truncates — with warnings the ops layer's business.
    /// Core carries no thresholds: the copy-versus-pin judgment belongs to callers who can see what this crate
    /// cannot (§2.7.7), with `substr` and `unshare` as the other two verbs.
    pub fn slice(&self, offset: usize, len: usize) -> Result<PString, AllocError> {
        let (utf8, tainted) = (self.is_utf8(), self.is_tainted());
        let total = self.len();
        let offset = offset.min(total);
        let len = len.min(total - offset);

        // Representability first: the dominant copy (§2.2.15).
        {
            let mut scratch = [0u8; DECODE_MAX];
            if len <= DECODE_MAX {
                let bytes = &self.as_bytes(&mut scratch)[offset..offset + len];

                // Representability, never a byte count (§2.2.15): `classify_inline` is the authority, and it holds
                // compressible content well past the payload width — a length pre-filter here would send a twenty-byte
                // Latin-1 cut to a view, pinning a whole buffer where a free envelope copy existed.
                if let Some((class, s, aux, buf)) = classify_inline(bytes) {
                    return Ok(PString::build_inline(class, utf8, tainted, s, aux, buf));
                }

                if (MIN_PACKED_LEN..=MAX_PACKED_LEN).contains(&len)
                    && let Some(packed) = pack(bytes)
                {
                    return Ok(PString::build_packed(packed, utf8, tainted));
                }

                if let Some((form, payload)) = classify_uuid(bytes) {
                    return Ok(PString::build_uuid(form, payload, utf8, tainted));
                }

                if let Some(payload) = classify_hex_bytes(bytes) {
                    return Ok(PString::build_hex(payload, utf8, tainted));
                }
            }
        }

        let mut scratch = [0u8; DECODE_MAX];
        match self.raw_parts() {
            // Re-slice composition: the same backing, absolute coordinates, the birth table against the view's own
            // bytes and envelope state.
            RawParts::View { bytes, .. } => {
                let Some((backing, raw, base, _)) = self.view_parts() else {
                    // Unreachable by construction — the View arm and view_parts cover the same families — but a
                    // defensive copy is total where a panic is forbidden.
                    return Ok(PString::build_heap(utf8, tainted, heap_parts_classified(&bytes[offset..offset + len])?));
                };

                let scan = view_birth_state(self.scan_state(), bytes, offset, len);

                // SAFETY (each arm): the handle's reference proves the backing live, and the coordinates were
                // clamped within the view, which lies within the backing.
                match backing {
                    ViewBacking::Heap32Medium | ViewBacking::Heap32Far => unsafe { heap32_view(raw, base + offset, len, scan, utf8, tainted) },
                    ViewBacking::Small { cap } => {
                        let ptr = unsafe {
                            small_backing_retain(raw, cap);
                            Owned::from_raw(raw)
                        };

                        Ok(PString::build_view(ViewBacking::Small { cap }, utf8, tainted, ptr, base + offset, len, scan))
                    }
                    ViewBacking::Adopted | ViewBacking::AdoptedFar => unsafe { adopted_view(raw.cast(), base + offset, len, scan, utf8, tainted) },
                }
            }
            RawParts::Heap(cb) => {
                let scan = view_birth_state(cb.scan(), cb.as_slice(), offset, len);

                // SAFETY (each arm): `cb` proves a live allocation of its tier with the clamped range initialized.
                match cb.tier() {
                    Tier::Heap8 | Tier::Heap16 => {
                        let cap = cb.capacity();
                        let ptr = unsafe {
                            small_backing_retain(cb.raw(), cap);
                            Owned::from_raw(cb.raw())
                        };

                        Ok(PString::build_view(ViewBacking::Small { cap }, utf8, tainted, ptr, offset, len, scan))
                    }
                    Tier::Heap32 => unsafe { heap32_view(cb.raw(), offset, len, scan, utf8, tainted) },

                    // The word tier lies past every compact reach: always the LargeSlice case.
                    Tier::Heap => {
                        let child = unsafe { cow_buffer::Adopted::adopt_heap_buf(cb.raw(), Tier::Heap, 0, offset, len, scan) }?;
                        Ok(PString::adopted_whole(child, utf8, tainted))
                    }
                }
            }
            RawParts::Borrowed { bytes, .. } => {
                let sub = &bytes[offset..offset + len];
                if len <= SPAN as usize {
                    let (st, chars) = classify_full(sub);
                    if let Some(term) = st.widen().terminal() {
                        // SAFETY: a subslice of a live image is nonnull.
                        let ptr = unsafe { std::ptr::NonNull::new_unchecked(sub.as_ptr().cast_mut()) };
                        let immortal = matches!(self.storage_type(), StorageType::Immortal | StorageType::LargeImmortal);

                        return Ok(if immortal {
                            PString::build_immortal(utf8, tainted, ptr, len, chars, term)
                        } else {
                            PString::build_static(utf8, tainted, ptr, len, chars, term)
                        });
                    }
                }

                // Past the compact image reach (a large image's sub-range over 16 MiB), or a defensively
                // non-terminal classification: the owned copy is total.
                Ok(PString::build_heap(utf8, tainted, heap_parts_classified(sub)?))
            }

            // Envelope-resident sources past representability: no buffer to share (§2.2.15).
            RawParts::Inline { .. } | RawParts::Packed(_) | RawParts::Uuid { .. } | RawParts::Hex { .. } => {
                let bytes = &self.as_bytes(&mut scratch)[offset..offset + len];
                Ok(PString::build_heap(utf8, tainted, heap_parts_classified(bytes)?))
            }
        }
    }

    /// The copying verb (§2.2.15): an unshared, uniquely-owned copy of `offset..offset + len` — content within the
    /// envelope ceiling runs the envelope ladder, representability staying conditional inside it, and everything
    /// past the ceiling takes the classified heap constructor — perl's own rvalue `substr` semantics under perl's
    /// own name, clamping as `slice` clamps.  Lvalue `substr` remains the reserved borrowed-view type `Str`.
    pub fn substr(&self, offset: usize, len: usize) -> Result<PString, AllocError> {
        let (utf8, tainted) = (self.is_utf8(), self.is_tainted());
        let total = self.len();
        let offset = offset.min(total);
        let len = len.min(total - offset);

        let mut scratch = [0u8; DECODE_MAX];
        let bytes = &self.as_bytes(&mut scratch)[offset..offset + len];
        if len <= DECODE_MAX {
            return PString::tiered(bytes, utf8, tainted);
        }

        Ok(PString::build_heap(utf8, tainted, heap_parts_classified(bytes)?))
    }

    /// Break any sharing: after this call, the value's bytes live in storage this handle exclusively owns, retaining
    /// nothing external.  This exists for lifetime control — a handle that must not pin an allocation other holders
    /// keep alive — not for mutation, which makes its own arrangements.  Unique and envelope-owned storage is
    /// untouched.  Shared storage is rebuilt by one copy, and classification rides the copy (§2.2.3), so a shared
    /// buffer with an indeterminate scan state comes out settled: unsharing only improves knowledge.  The Perl utf8
    /// flag and taint are preserved — this changes where the bytes live, never what the value means.
    pub fn unshare(&mut self) -> Result<(), AllocError> {
        let parts = match self.raw_parts() {
            RawParts::Heap(view) if !view.is_unique() => heap_parts_transitioned(view.as_slice(), view.scan(), view.char_count())?,

            // A view carries no count of its own (§2.2.15): zero is the unfilled sentinel, and classification rides the
            // copy as it does for the shared-heap arm above.
            RawParts::View { bytes, scan, .. } => heap_parts_transitioned(bytes, scan, 0)?,
            RawParts::Heap(_) | RawParts::Inline { .. } | RawParts::Packed(_) | RawParts::Uuid { .. } | RawParts::Hex { .. } | RawParts::Borrowed { .. } => {
                return Ok(());
            }
        };
        *self = PString::build_heap(self.is_utf8(), self.is_tainted(), parts);
        Ok(())
    }

    /// Whether the bytes are valid under perl's *extended* UTF-8 rules (§2.2.4) — the predicate character-level
    /// operations on flagged strings use.  Narrows the heap lattice.
    pub fn is_perl_utf8_valid(&self) -> bool {
        match self.raw_parts() {
            RawParts::Inline { .. } => !matches!(self.inline_class(), Some(InlineClass::Bytes)),
            RawParts::Packed(_) => true,   // ASCII is valid under every reading.
            RawParts::Uuid { .. } => true, // Likewise.
            RawParts::Hex { .. } => true,  // Likewise.
            RawParts::Borrowed { scan, .. } => scan::is_perl_decodable(scan.widen()),
            RawParts::View { bytes, scan, backing } => match backing.map_or(scan, |a| scan::meet(scan, a.scan())) {
                st if scan::is_perl_decodable(st) => true,
                scan::MalformedUtf8 => false,

                // Undecided: classify and answer, recording the certification in the shared slot where one exists.
                _ => {
                    let st = classify_full(bytes).0.widen();
                    if let Some(a) = backing {
                        a.narrow_scan(st);
                    }

                    scan::is_perl_decodable(st)
                }
            },
            RawParts::Heap(cb) => match cb.scan() {
                st if scan::is_perl_decodable(st) => true,
                scan::MalformedUtf8 => false,
                _ => {
                    let (st, chars) = classify_full(cb.as_slice()); // the single pass
                    let st = st.widen();
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
            RawParts::Uuid { .. } => Some(UUID_LEN),
            RawParts::Hex { payload } => Some(hex_rendered_len(payload)),
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

            // A view carries no count of its own (§2.2.15): a whole-object adopted view reads and fills the struct's
            // cache — zero the unfilled sentinel — and sub-views derive on demand.
            RawParts::View { bytes, scan, backing } => match backing.map_or(scan, |a| scan::meet(scan, a.scan())) {
                _ if bytes.is_empty() => Some(0),
                scan::Ascii => Some(bytes.len()),
                scan::MalformedUtf8 => None,
                _ => {
                    if let Some(a) = backing {
                        let cached = a.char_count();
                        if cached != 0 {
                            return Some(cached);
                        }
                    }

                    let (st, chars) = classify_full(bytes);
                    if let Some(a) = backing {
                        a.narrow_scan(st.widen());
                    }

                    if st.widen() == scan::MalformedUtf8 {
                        None
                    } else {
                        if let Some(a) = backing {
                            a.set_char_count(chars);
                        }

                        Some(chars)
                    }
                }
            },

            // Settled at construction: the count is always true, and only the malformed terminal has none.
            RawParts::Borrowed { count, scan, .. } => {
                if scan == scan::Terminal::MalformedUtf8 {
                    None
                } else {
                    Some(count)
                }
            }
            RawParts::Heap(cb) => match cb.scan() {
                _ if cb.is_empty() => Some(0), // Zero bytes hold zero characters: the count field is never consulted.
                scan::Ascii => Some(cb.len()),
                scan::MalformedUtf8 => None,
                _ => {
                    let cached = cb.char_count();
                    if cached > 0 {
                        return Some(cached);
                    }

                    let (st, chars) = classify_full(cb.as_slice()); // one pass classifies AND counts
                    let st = st.widen();
                    cb.narrow_scan(st);

                    if st == scan::MalformedUtf8 {
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
    // ── Representation transforms (§2.2.9) ────────────────────────
    // Private until the ops layer calls them — no public surface without a caller — with the tests pinning the
    // container-verified facts the design records.

    /// `Encode::_utf8_on`/`_utf8_off`: reinterpretation is a pure flag flip.  The class is a fact about the bytes, so
    /// the representation is already right, verbatim and compressed classes alike (§2.2.9): an upgraded `é` becomes the
    /// flag-off two-character `C3.A9` with the payload untouched.
    fn reinterpret_utf8(&mut self, utf8: bool) {
        self.rebuild_tag(|_, t| (utf8, t));
    }

    /// `utf8::upgrade`: the same characters, flagged.  Flagged content is untouched.  An unflagged string's characters
    /// are its internal bytes, so the result stores exactly those bytes as its compressed payload — zero byte work for
    /// the Ascii and verbatim classes, whose payload already is the internal bytes (the monster: flag-off `E9` re-tags
    /// Bytes to flagged Latin-1, payload identical), and one expansion copy for the Latin-1 class, whose stored bytes
    /// are not (upgrading flag-off `C3 A9` yields the two-character `Ã©`, not `é` — that reinterpretation is
    /// `_utf8_on`'s).  Past fifteen characters the result is heap: sixteen to thirty characters have no flagged
    /// non-heap form unless ASCII, and ASCII took the flip.
    fn upgraded(&self) -> Result<PString, AllocError> {
        if self.is_utf8() {
            return Ok(self.clone());
        }

        if self.is_ascii() {
            // Characters, bytes, and encoding all coincide: the representation is already right in every tier.
            let mut s = self.clone();
            s.reinterpret_utf8(true);
            return Ok(s);
        }

        let t = self.is_tainted();
        let mut scratch = [0u8; DECODE_MAX];
        let internal = self.as_bytes(&mut scratch);

        if internal.len() <= INLINE_MAX {
            let mut buf = [0u8; INLINE_MAX];
            buf[..internal.len()].copy_from_slice(internal);
            let h = high_count(internal);
            debug_assert!(h > 0, "all-ASCII content took the flip above");
            return Ok(PString::build_inline(InlineClass::Latin1, true, t, internal.len(), h, buf));
        }

        // Sixteen or more non-ASCII characters: heap.  The buffer owns the expansion — appending byte by byte would pay
        // a capacity check and two cache-invalidating atomic stores per input byte, and would rewrite an invariant
        // prefix the buffer can copy wholesale.
        let upgraded = cow_buffer::upgraded_bytes(internal)?;
        let parts = HeapParts::from_slice(&upgraded, scan::Utf8Latin1, internal.len())?;

        Ok(PString::build_heap(true, t, parts))
    }

    /// The in-place form of [`PString::upgraded`]: the same characters, flagged, rewriting a unique heap buffer rather
    /// than producing a fresh value — perl's `utf8::upgrade` shape, and the only form that can leave the invariant
    /// prefix untouched.  The non-heap forms hold at most thirty bytes, so they rebuild through the copying form; a
    /// shared heap buffer does too, its other holders keeping the unexpanded content.
    // `allow` rather than `expect`: gating the entry point marks it a live root, so its callees stop being reported and
    // an `expect` here would itself go unfulfilled.
    #[cfg_attr(not(test), allow(dead_code))] // The ops layer is the caller-to-be; the tests keep it honest.
    fn upgrade_in_place(&mut self) -> Result<(), AllocError> {
        if self.is_utf8() {
            return Ok(());
        }
        if self.is_ascii() {
            // Characters, bytes, and encoding all coincide: the representation is already right in every tier.
            self.reinterpret_utf8(true);
            return Ok(());
        }

        // In place where the expansion fits the spare capacity and the buffer is unique; otherwise the copying form,
        // which is what reallocating means and which picks the right tier on the way.
        if self.upgrade_heap_in_place().is_none() {
            *self = self.upgraded()?;
            return Ok(());
        }

        self.reinterpret_utf8(true); // The bytes are already the encoding now; only the tag moves.

        Ok(())
    }

    /// `utf8::downgrade`: the same characters, unflagged — `None` where a character exceeds U+00FF, which is where
    /// perl's downgrade dies.  Unflagged content is untouched.  The compressed classes' characters are their stored
    /// bytes, so the result is those bytes reclassified as octets — zero byte work for the monster (`é` re-tags Latin-1
    /// to Bytes, payload `E9` identical), re-compression where the octets happen to be valid Latin-1-range UTF-8.  The
    /// flagged verbatim classes cannot downgrade: NonLatin1 and Extended hold a character at or above U+0100 by their
    /// class, and the Bytes class flagged has no characters at all.
    fn downgraded(&self) -> Result<Option<PString>, AllocError> {
        if !self.is_utf8() {
            return Ok(Some(self.clone()));
        }

        if self.is_ascii() {
            let mut s = self.clone();
            s.reinterpret_utf8(false);
            return Ok(Some(s));
        }

        let t = self.is_tainted();
        match self.raw_parts() {
            RawParts::Inline { class: InlineClass::Latin1, full, buf } => {
                let stored = inline_stored(full, buf);

                Ok(Some(PString::tiered(&buf[..stored], false, t)?))
            }
            RawParts::Inline { .. } => Ok(None),
            RawParts::Packed(_) => {
                // Packed content is ASCII by construction, so the flip above took it; answering the same way keeps the
                // arm total without a panic.
                let mut s = self.clone();
                s.reinterpret_utf8(false);

                Ok(Some(s))
            }
            RawParts::Uuid { .. } => {
                // Likewise ASCII by construction.
                let mut s = self.clone();
                s.reinterpret_utf8(false);

                Ok(Some(s))
            }
            RawParts::Hex { .. } => {
                // Likewise ASCII by construction.
                let mut s = self.clone();
                s.reinterpret_utf8(false);

                Ok(Some(s))
            }
            RawParts::Borrowed { bytes, .. } | RawParts::View { bytes, .. } => {
                // The image is readonly, so the downgrade is a copy-out by nature: walk it like heap content.
                let Some(out) = cow_buffer::downgraded_bytes(bytes)? else {
                    return Ok(None); // A character past U+00FF, or no character at all.
                };

                if out.len() <= DECODE_MAX {
                    return Ok(Some(PString::tiered(&out, false, t)?));
                }

                Ok(Some(PString::build_heap(false, t, heap_parts_classified(&out)?)))
            }
            RawParts::Heap(cb) => {
                // Walk the encoding: every character must sit in U+0000-U+00FF, emitted as its single byte.  The result
                // re-runs the ladder — sixteen to thirty emitted octets can compress right back inline.
                let Some(out) = cow_buffer::downgraded_bytes(cb.as_slice())? else {
                    return Ok(None); // A character past U+00FF, or no character at all.
                };

                if out.len() <= DECODE_MAX {
                    return Ok(Some(PString::tiered(&out, false, t)?));
                }

                Ok(Some(PString::build_heap(false, t, heap_parts_classified(&out)?)))
            }
        }
    }

    /// The in-place form of [`PString::downgraded`]: the same characters, unflagged, contracting a unique heap buffer
    /// rather than producing a fresh value.  `false` means the content refuses to downgrade — a character above
    /// `U+00FF`, where perl's dies — and leaves this value untouched.  Contraction never grows, so unlike the upgrade
    /// it needs no reallocation; the invariant prefix is still left exactly where it is.
    #[cfg_attr(not(test), allow(dead_code))] // The ops layer is the caller-to-be; the tests keep it honest.
    fn downgrade_in_place(&mut self) -> Result<bool, AllocError> {
        if !self.is_utf8() {
            return Ok(true);
        }

        if self.is_ascii() {
            self.reinterpret_utf8(false);
            return Ok(true);
        }

        // Contraction only shrinks, so it never needs room, never reallocates and never leaves its tier: the only
        // reason to copy is a shared buffer.
        match self.downgrade_heap_in_place() {
            Some(false) => return Ok(false), // A character past U+00FF.
            Some(true) => {}
            None => match self.downgraded()? {
                Some(contracted) => {
                    *self = contracted;
                    return Ok(true);
                }
                None => return Ok(false),
            },
        }

        // Contracted octets can themselves be valid UTF-8, so the class is not derivable here; the caches were cleared
        // and the next reader re-derives them.  Only the flag moves.
        self.reinterpret_utf8(false);

        Ok(true)
    }

    /// Set or propagate the taint bit.  Monotonic raise; clearing is the laundering capability's alone (§2.6.2).
    pub fn taint(&mut self) {
        self.rebuild_tag(|u, _t| (u, true));
    }

    /// Clear the taint bit.  Non-public: reachable only through the two sanctioned laundering paths (§2.6.2) — capture
    /// materialization and hash-key canonicalization, both inside perl-core.
    pub(crate) fn untaint_for_sanctioned_path(&mut self) {
        self.rebuild_tag(|u, _t| (u, false));
    }

    fn rebuild_tag(&mut self, f: impl FnOnce(bool, bool) -> (bool, bool)) {
        let (u, t) = (self.is_utf8(), self.is_tainted());
        let (u2, t2) = f(u, t);

        if (u, t) == (u2, t2) {
            return;
        }

        let old = mem::take(self);

        *self = match old.into_raw() {
            RawOwned::Inline { class, full, buf } => {
                let (s, aux) = (inline_stored(full, &buf), inline_derived_aux(class, full, &buf));
                PString::build_inline(class, u2, t2, s, aux, buf)
            }
            RawOwned::Packed(p) => PString::build_packed(p, u2, t2),
            RawOwned::Uuid { form, payload } => PString::build_uuid(form, payload, u2, t2),
            RawOwned::Hex { payload } => PString::build_hex(payload, u2, t2),
            RawOwned::Heap { ptr, len, cap, count, scan, tier } => PString::build_heap(u2, t2, HeapParts { ptr, len, cap, count, scan, tier }),
            RawOwned::Borrowed { form: BorrowedForm::Immortal, ptr, len, count, scan } => PString::build_immortal(u2, t2, ptr, len, count, scan),
            RawOwned::Borrowed { form: BorrowedForm::Static, ptr, len, count, scan } => PString::build_static(u2, t2, ptr, len, count, scan),
            RawOwned::BorrowedLarge { form: BorrowedForm::Immortal, head } => PString::build_large_immortal(u2, t2, head),
            RawOwned::BorrowedLarge { form: BorrowedForm::Static, head } => PString::build_large_static(u2, t2, head),
            RawOwned::View { ptr, backing, offset, len, scan } => PString::build_view(backing, u2, t2, ptr, offset, len, scan),
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
            AppendKind::Valid { class: scan::ValidRange::Ascii, chars: bytes.len() }
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
        // extended, and written back.  Taking exists to move an owned heap pointer out of `&mut self`, which only the
        // heap arm below actually requires.  Cheapest case first, and touching nothing it does not have to: only the
        // Ascii class writes through — its payload is the raw bytes with a zero aux nibble, so appending ASCII content
        // that still fits short extends the existing variant bit-identically to the raw-byte tier, the length byte
        // updated in place.  Every other class, and any append reaching full capacity (which changes the family),
        // rebuilds through canonical selection below — append is byte mutation (§2.2.9).
        if self.inline_class() == Some(InlineClass::Ascii)
            && matches!(kind, AppendKind::Valid { class: scan::ValidRange::Ascii, .. })
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

        // Otherwise the payload has to move.  For inline content that is still only a materialization of at most thirty
        // bytes and one rebuild: append is byte mutation, so the result re-runs canonical selection (§2.2.9) over the
        // value's internal bytes — the compressed classes expand first, exactly as `as_bytes` would.
        let inline = match self.raw_parts() {
            RawParts::Inline { class, full, buf } => Some((class, inline_stored(full, buf), *buf)),
            _ => None,
        };

        if let Some((class, stored, buf)) = inline {
            let (u, t) = (self.is_utf8(), self.is_tainted());

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

            if total <= DECODE_MAX {
                let mut combined = [0u8; DECODE_MAX];
                combined[..ilen].copy_from_slice(&internal[..ilen]);
                combined[ilen..total].copy_from_slice(bytes);

                if let Some((nc, ns, naux, nbuf)) = classify_inline(&combined[..total]) {
                    *self = PString::build_inline(nc, u, t, ns, naux, nbuf);
                    return Ok(());
                }

                if (MIN_PACKED_LEN..=MAX_PACKED_LEN).contains(&total)
                    && let Some(packed) = pack(&combined[..total])
                {
                    *self = PString::build_packed(packed, u, t);
                    return Ok(());
                }

                if let Some((form, payload)) = classify_uuid(&combined[..total]) {
                    *self = PString::build_uuid(form, payload, u, t);
                    return Ok(());
                }

                if let Some(payload) = classify_hex_bytes(&combined[..total]) {
                    *self = PString::build_hex(payload, u, t);
                    return Ok(());
                }

                // Sixteen to thirty bytes fitting neither a compressed payload nor an alphabet: the heap, below.
            }

            let mut joined = Vec::new();
            joined.try_reserve_exact(total).map_err(|_| AllocError { requested: total })?;
            joined.extend_from_slice(&internal[..ilen]);
            joined.extend_from_slice(bytes);
            let state = append_transition_heap(inline_scan_to_heap(class), kind);
            *self = PString::build_heap(u, t, heap_parts_transitioned(&joined, state, 0)?);

            return Ok(());
        }

        // The fast path the class headroom exists for (§2.2.3): a unique buffer whose spare capacity holds the result
        // extends in place — one suffix copy, no allocation.  Everything else rebuilds below.
        if self.append_heap_in_place(bytes, kind) {
            return Ok(());
        }

        let (u, t) = (self.is_utf8(), self.is_tainted());
        let old = mem::take(self);

        *self = match old.into_raw() {
            RawOwned::Inline { .. } => return Ok(()), // Handled above; unreachable.
            RawOwned::Packed(p) => {
                if let Some(packed) = p.push(bytes) {
                    // In place: the existing nibbles are kept, rather than the whole result being decoded and
                    // re-encoded on every append.
                    PString::build_packed(packed, u, t)
                } else {
                    // Past the band, or no longer alphabet-conformant: decode once, on the way out of the tier.  Packed
                    // content is ASCII, so the heap state starts from there.
                    let (decoded, len) = p.unpack();
                    let old_bytes = &decoded[..len];
                    let new_len = len + bytes.len();
                    let mut joined = Vec::new();
                    joined.try_reserve_exact(new_len).map_err(|_| AllocError { requested: new_len })?;
                    joined.extend_from_slice(old_bytes);
                    joined.extend_from_slice(bytes);
                    let state = append_transition_heap(scan::Ascii, kind);
                    PString::build_heap(u, t, heap_parts_transitioned(&joined, state, 0)?)
                }
            }
            RawOwned::Uuid { form, payload } => {
                // An append leaves the canonical spelling, so the value always exits the family: decode once, on the
                // way out, ASCII seeding the heap state as the packed tier's exit does.
                let mut decoded = [0u8; DECODE_MAX];
                let len = decode_uuid(form, &payload, &mut decoded);
                let new_len = len + bytes.len();
                let mut joined = Vec::new();
                joined.try_reserve_exact(new_len).map_err(|_| AllocError { requested: new_len })?;
                joined.extend_from_slice(&decoded[..len]);
                joined.extend_from_slice(bytes);
                let state = append_transition_heap(scan::Ascii, kind);
                PString::build_heap(u, t, heap_parts_transitioned(&joined, state, 0)?)
            }
            RawOwned::Hex { payload } => {
                // An append may or may not leave a hex spelling; the combined attempt above already tried the ladder,
                // so reaching here means it did.  Same exit as the packed tier's.
                let mut decoded = [0u8; DECODE_MAX];
                let len = decode_hex_bytes(&payload, &mut decoded);
                let new_len = len + bytes.len();
                let mut joined = Vec::new();
                joined.try_reserve_exact(new_len).map_err(|_| AllocError { requested: new_len })?;
                joined.extend_from_slice(&decoded[..len]);
                joined.extend_from_slice(bytes);
                let state = append_transition_heap(scan::Ascii, kind);
                PString::build_heap(u, t, heap_parts_transitioned(&joined, state, 0)?)
            }
            RawOwned::Borrowed { form: _, ptr, len, count: _, scan } => {
                // Copy-out on write (§2.2.3): the image is readonly, so an append is a rebuild seeded by the settled
                // state — same shape as the packed tier's exit.
                // SAFETY: the image outlives every handle by the forms' contract.
                let old_bytes = unsafe { std::slice::from_raw_parts(ptr.as_ptr(), len) };
                let new_len = len + bytes.len();
                let mut joined = Vec::new();
                joined.try_reserve_exact(new_len).map_err(|_| AllocError { requested: new_len })?;
                joined.extend_from_slice(old_bytes);
                joined.extend_from_slice(bytes);
                let state = append_transition_heap(scan.widen(), kind);
                PString::build_heap(u, t, heap_parts_transitioned(&joined, state, 0)?)
            }
            RawOwned::BorrowedLarge { form: _, head } => {
                // Copy-out on write, at large size: the image is readonly, so the append is a rebuild seeded by the
                // settled state, exactly the compact forms' path.
                let old_bytes = head.bytes();
                let new_len = old_bytes.len() + bytes.len();
                let mut joined = Vec::new();
                joined.try_reserve_exact(new_len).map_err(|_| AllocError { requested: new_len })?;
                joined.extend_from_slice(old_bytes);
                joined.extend_from_slice(bytes);
                let state = append_transition_heap(head.scan.widen(), kind);
                PString::build_heap(u, t, heap_parts_transitioned(&joined, state, 0)?)
            }
            RawOwned::View { mut ptr, backing, offset, len, scan } => {
                // Copy-out on write (§2.2.15): a view is a read-only carrier, so an append is a rebuild seeded by the
                // envelope's state — the images' path, plus a reference to surrender.
                // SAFETY: the transport owns one reference on the live backing, released below after the copy.
                let old_bytes = unsafe {
                    match backing {
                        ViewBacking::Heap32Medium | ViewBacking::Heap32Far | ViewBacking::Small { .. } => {
                            std::slice::from_raw_parts(ptr.as_ptr().as_ptr().add(offset), len)
                        }
                        ViewBacking::Adopted => {
                            let a: &cow_buffer::Adopted = ptr.as_ptr().cast::<cow_buffer::Adopted>().as_ref();
                            let (off, n) = if offset == SPAN as usize && len == SPAN as usize { (0, a.total_len()) } else { (offset, len) };
                            &a.as_slice()[off..off + n]
                        }
                        ViewBacking::AdoptedFar => {
                            let a: &cow_buffer::Adopted = ptr.as_ptr().cast::<cow_buffer::Adopted>().as_ref();
                            &a.as_slice()[offset..offset + len]
                        }
                    }
                };

                let new_len = old_bytes.len() + bytes.len();
                let mut joined = Vec::new();
                let reserved = joined.try_reserve_exact(new_len);
                if reserved.is_ok() {
                    joined.extend_from_slice(old_bytes);
                    joined.extend_from_slice(bytes);
                }

                // The reference is surrendered on every path: the copy above is complete or abandoned.
                // SAFETY: the transport's one reference, consumed exactly once, under the backing's own release.
                unsafe {
                    match backing {
                        ViewBacking::Heap32Medium | ViewBacking::Heap32Far => cow_buffer::heap32::release(ptr.claim()),
                        ViewBacking::Small { cap } => small_backing_release(ptr.claim(), cap),
                        ViewBacking::Adopted | ViewBacking::AdoptedFar => cow_buffer::Adopted::release(ptr.claim().cast()),
                    }
                }

                if reserved.is_err() {
                    return Err(AllocError { requested: new_len });
                }

                let state = append_transition_heap(scan, kind);
                PString::build_heap(u, t, heap_parts_transitioned(&joined, state, 0)?)
            }
            RawOwned::Heap { ptr, len, cap, count, scan: prior, tier } => {
                // Reached only past the in-place fast path — shared, or over capacity — so the buffer is rebuilt:
                // growth crosses tiers at the ceilings (§2.2.3), and choosing the tier is what `HeapParts::from_slice`
                // does.  Reassembling the parts first makes the old allocation owned for the whole arm: dropping `old`
                // — at the end or through either `?` — is the release.  The first run of the leak bomb caught this arm
                // abandoning the pointer instead.
                let old = HeapParts { ptr, len, cap, count, scan: prior, tier };
                let view = match tier {
                    Tier::Heap8 | Tier::Heap16 => HeapView::small(&old.ptr, len, cap, count, prior, tier),

                    // SAFETY (both large arms): a live allocation of the matching tier, owned by `old`.  The compact
                    // tier's length is envelope-authoritative, so its view takes the length this arm already holds
                    // (§2.2.3).
                    Tier::Heap32 => unsafe { HeapView::heap32(&old.ptr, len) },
                    Tier::Heap => unsafe { HeapView::large(&old.ptr, tier) },
                };
                let (prior, prior_chars) = (view.scan(), view.char_count());

                let old_bytes = view.as_slice();
                let total = old_bytes.len() + bytes.len();
                let mut joined = Vec::new();
                joined.try_reserve_exact(total).map_err(|_| AllocError { requested: total })?;
                joined.extend_from_slice(old_bytes);
                joined.extend_from_slice(bytes);

                let state = append_transition_heap(prior, kind);

                // Maintain the character count incrementally when both sides know theirs (§2.2.5): the appended
                // content's own classification counted its characters in its own pass.
                let chars = match kind {
                    AppendKind::Valid { chars: added, .. } if prior_chars > 0 && added > 0 && scan::is_perl_decodable(state) => prior_chars + added,
                    _ => 0,
                };

                PString::build_heap(u, t, heap_parts_transitioned(&joined, state, chars)?)
            }
        };

        Ok(())
    }
}

/// Which immortal form a borrowed payload came from, so a tag rebuild can return to it.  Nothing else dispatches on
/// this: the forms differ in who guarantees the image's life (§2.2.3), not in how it reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BorrowedForm {
    Immortal,
    Static,
}

/// An immortal image pointer (§2.2.3): a thin wrapper whose only job is the thread-safety assertion the raw pointer
/// cannot make for itself.
#[derive(Clone, Copy)]
struct Image(std::ptr::NonNull<u8>);

// SAFETY: the image is readonly for its entire life and outlives every handle by the forms' contract (§2.2.3), so
// sharing references to it across threads is sharing immutable bytes; no operation writes through this pointer.
unsafe impl Send for Image {}
unsafe impl Sync for Image {}

/// The large immortal forms' side header (§2.2.3): word-width facts for images past the compact ceiling, plus the image
/// pointer neither form can prepend a header to.  Shared by every bitwise clone and deliberately leaked per the ruling
/// — handles cannot free what they share without the refcount these forms decline, so ownership belongs above them: the
/// §2.4 slab when it lands, the process's life in standalone use.  Allocated raw rather than through the tier backend,
/// so the live counters keep meaning tier balance and a ruled leak indicts no balance test.
struct ImmortalHead {
    image: Image,
    len: usize,
    count: usize,
    scan: scan::Terminal,
}

impl ImmortalHead {
    /// Allocate and leak a header.  Fallible by hand — `Box::new` aborts on exhaustion, and this crate reports.
    fn leaked(image: Image, len: usize, count: usize, scan: scan::Terminal) -> Result<&'static ImmortalHead, AllocError> {
        let layout = std::alloc::Layout::new::<ImmortalHead>();

        // SAFETY: the layout is non-zero-sized; the write initializes the allocation before any read.
        let ptr = unsafe { std::alloc::alloc(layout) }.cast::<ImmortalHead>();
        let Some(ptr) = std::ptr::NonNull::new(ptr) else {
            return Err(AllocError { requested: layout.size() });
        };
        unsafe { ptr.as_ptr().write(ImmortalHead { image, len, count, scan }) };

        // SAFETY: just initialized, never freed — 'static by the leak this type's contract rules.
        Ok(unsafe { &*ptr.as_ptr() })
    }

    /// The image bytes.
    fn bytes(&self) -> &'static [u8] {
        // SAFETY: the image outlives every handle by the forms' contract, and the header itself is leaked.
        unsafe { std::slice::from_raw_parts(self.image.0.as_ptr(), self.len) }
    }
}

/// The immortal envelopes' 24-bit fields (§2.2.3): compact forms hold lengths and counts below 16 MiB.
const U24_MAX: usize = 0xFF_FFFF;

fn u24_new(value: usize) -> [u8; 3] {
    debug_assert!(value <= U24_MAX, "a compact immortal field holds at most 16 MiB - 1");
    let b = (value as u32).to_le_bytes();
    [b[0], b[1], b[2]]
}

fn u24_get(bytes: &[u8; 3]) -> usize {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]) as usize
}

enum RawParts<'a> {
    /// A packed hex-byte string (§2.2.16): the payload carries digits, length, format, and case, decoded on demand
    /// into a caller scratch — the content is ASCII and terminal, whatever the spelling.
    Hex {
        payload: &'a [u8; PACKED_BYTES],
    },

    /// A packed UUID (§2.2.16): the form and the 15-byte payload, decoded on demand into a caller scratch — the content
    /// is always the 36-character canonical lowercase spelling, ASCII and terminal.
    Uuid {
        form: UuidForm,
        payload: &'a [u8; PACKED_BYTES],
    },

    /// A view's bytes (§2.2.15), resolved by `raw_parts` itself — offset applied, `SPAN` decoded — so every consumer
    /// reads one shape.  The scan is the envelope's per-handle byte, born from the slice-birth table and possibly
    /// non-terminal.  `backing` is the read-through handle for whole-object adopted views only (§2.2.15): their facts
    /// are the whole object's facts, so they read the struct's shared slot and character-count cache — riding the cache
    /// line `base` already occupies — and record what on-demand classification learns; sub-views carry `None`, since a
    /// sub-range's facts certify nothing about the whole and view envelopes cannot propagate narrowing.
    View {
        bytes: &'a [u8],
        scan: scan::ScanState,
        backing: Option<&'a cow_buffer::Adopted>,
    },

    Inline {
        class: InlineClass,
        full: bool,
        buf: &'a [u8; INLINE_MAX],
    },
    Packed(Packed),

    /// Whatever the tier, read through one view: the metadata is gathered at construction from wherever that tier keeps
    /// it, so nothing below dispatches on it (§2.2.3).
    Heap(HeapView<'a>),

    /// An immortal image (§2.2.3): readonly bytes someone else keeps alive, with facts settled at construction.  There
    /// is no refcount and no headroom, so every writer copies out and every clone is bitwise.  Which form is
    /// [`RawOwned`]'s business — rebuilds need it, reads never dispatch on it.
    Borrowed {
        bytes: &'a [u8],
        count: usize,
        scan: scan::Terminal,
    },
}

enum RawOwned {
    Inline {
        class: InlineClass,
        full: bool,
        buf: [u8; INLINE_MAX],
    },
    Packed(Packed),
    Uuid {
        form: UuidForm,
        payload: [u8; PACKED_BYTES],
    },
    Hex {
        payload: [u8; PACKED_BYTES],
    },

    /// The owned pointer with its tier's full metadata, wherever the tier keeps it: the small tiers' fields come from
    /// the envelope, the large tiers' from the allocation header, read at the take.  Every field is true — nothing
    /// rides at zero on the strength of nobody reading it, and the append transition starts from the state the buffer
    /// actually knew rather than discarding it to `UNKNOWN`.
    Heap {
        ptr: Owned,
        len: usize,
        cap: usize,
        count: usize,
        scan: scan::ScanState,
        tier: Tier,
    },

    /// A view's owned backing reference in flight (§2.2.15): the backing says which release it owes, the fields ride at
    /// full width, and a tag transition preserves the view while a content transition must materialize away from it
    /// first.
    View {
        ptr: Owned,
        backing: ViewBacking,
        offset: usize,
        len: usize,
        scan: scan::ScanState,
    },

    /// An immortal image's envelope fields: nothing is owned, so nothing transfers but the facts.
    Borrowed {
        form: BorrowedForm,
        ptr: std::ptr::NonNull<u8>,
        len: usize,
        count: usize,
        scan: scan::Terminal,
    },

    /// A large immortal form's shared header: rebuilds point a fresh envelope at the same header.
    BorrowedLarge {
        form: BorrowedForm,
        head: &'static ImmortalHead,
    },
}

/// What is known about appended content, for the §2.2.5 transition rules.  For Rust-valid content the range is carried
/// (join semantics: the result range is the max of the operand ranges, §2.2.5).
#[derive(Clone, Copy, PartialEq)]
enum AppendKind {
    /// Known valid UTF-8, with its range class and character count (0 when the classification bailed early — count
    /// forfeited, class still exact).  The class is [`scan::ValidRange`], so carrying anything outside the chain is
    /// unrepresentable rather than a documented convention.
    Valid { class: scan::ValidRange, chars: usize },

    /// Nothing known.
    Unknown,
}

fn inline_scan_to_heap(s: InlineClass) -> scan::ScanState {
    match s {
        InlineClass::Ascii => scan::Ascii,
        InlineClass::Latin1 => scan::Utf8Latin1,
        InlineClass::NonLatin1 => scan::Utf8NonLatin1,
        InlineClass::Extended => scan::ExtendedUtf8,
        InlineClass::Bytes => scan::MalformedUtf8,
    }
}

/// §2.2.5 append transitions for a heap result, from the buffer's prior state and the appended content's kind.
fn append_transition_heap(prior: scan::ScanState, kind: AppendKind) -> scan::ScanState {
    use scan::{ScanState, ValidRange};
    match kind {
        // Appending pure ASCII: no state change (cannot raise the range or affect validity).
        AppendKind::Valid { class: ValidRange::Ascii, .. } => prior,
        AppendKind::Valid { class, .. } => match prior {
            // Valid + valid: the range join (§2.2.5), total on the chain and typed there.
            ScanState::Ascii => ValidRange::Ascii.join(class).widen(),
            ScanState::Utf8Latin1 => ValidRange::Latin1.join(class).widen(),
            ScanState::Utf8NonLatin1 => ValidRange::NonLatin1.join(class).widen(),

            // Range-unresolved priors: the addition can prove non-ASCII or beyond-Latin-1, never below.
            ScanState::ValidUtf8 if class == ValidRange::NonLatin1 => ScanState::Utf8NonLatin1,
            ScanState::ValidUtf8 => ScanState::Utf8NonAscii,
            ScanState::Utf8NonAscii if class == ValidRange::NonLatin1 => ScanState::Utf8NonLatin1,
            ScanState::Utf8NonAscii => ScanState::Utf8NonAscii,

            // Perl-decodable onto extended: the Rust-rejected code point is still there.
            ScanState::ExtendedUtf8 => ScanState::ExtendedUtf8,

            // The ambiguous twins (§2.2.4): appended Latin-1-class content supplies exactly the witness
            // `MaybeUtf8Latin1` lacks, completing it; NonLatin1-class content proves the stronger terminal outright.
            // `MaybeExtendedUtf8` stays — appended Rust-valid content resolves nothing about the extended code points
            // that may already be present.
            ScanState::MaybeUtf8Latin1 if class == ValidRange::NonLatin1 => ScanState::Utf8NonLatin1,
            ScanState::MaybeUtf8Latin1 => ScanState::Utf8Latin1,
            ScanState::MaybeExtendedUtf8 => ScanState::MaybeExtendedUtf8,
            ScanState::PerlValidNonAscii => ScanState::PerlValidNonAscii,

            // Prior validity unknown or invalid: fallback, lazily recoverable above 64 KiB and reclassified below
            // (§2.2.3's funnel).  Named rather than wildcarded: an indeterminate-state defect once hid in a `_` arm
            // here, and a tenth state added to the lattice must land here by decision, not by omission.
            ScanState::Unknown | ScanState::MalformedUtf8 | ScanState::NonAscii => ScanState::Unknown,
        },
        AppendKind::Unknown => ScanState::Unknown,
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

impl PString {
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

    /// [`PString::numify`] and [`PString::would_warn`] from the one walk the numification already pays (§2.3.4): the
    /// parse surfaces its own consumption, and warn-worthiness is that consumption measured against the trimmed token.
    /// The sites that need both answers — payload numification, frozen-cell materialization — come here rather than
    /// walking twice.
    pub fn numify_noting_warning(&self) -> (Numeric, bool) {
        let mut scratch = [0u8; DECODE_MAX];
        classify_numeric_noting_warning(self.as_bytes(&mut scratch))
    }

    /// The bounded prefix a warning message quotes (§2.3.4): up to `max_chars` characters — bytes when unflagged,
    /// perl-decoded characters when flagged, the cut always sequence-clean — plus whether the face extends beyond it.
    /// Perl's renderer consumes source greedily while the rendered width is under its cap, so a bound of cap + 1
    /// characters is sufficient for any conforming renderer; carrying more would pin content the message never uses.
    /// When the whole face fits the bound, the clone is a refcount bump and nothing is copied.
    pub(crate) fn message_prefix(&self, max_chars: usize) -> Result<(PString, bool), AllocError> {
        let mut scratch = [0u8; DECODE_MAX];
        let bytes = self.as_bytes(&mut scratch);

        // Character-count the prefix: bytes are characters unless flagged, where a character is a perl-extended
        // sequence — lead byte plus continuations — and the cut lands before a lead.
        let cut = if !self.is_utf8() {
            if bytes.len() <= max_chars { bytes.len() } else { max_chars }
        } else {
            let mut chars = 0usize;
            let mut at = 0usize;
            while at < bytes.len() && chars < max_chars {
                at += 1;
                while at < bytes.len() && bytes[at] & 0xC0 == 0x80 {
                    at += 1;
                }
                chars += 1;
            }
            at
        };

        if cut == bytes.len() {
            return Ok((self.clone(), false));
        }

        let mut snippet = PString::from_bytes(&bytes[..cut])?;
        snippet.reinterpret_utf8(self.is_utf8());

        Ok((snippet, true))
    }

    /// Whether numifying this string would emit perl's `Argument isn't numeric` warning (§2.3.4).  A question about the
    /// content.  Whether the warning has *already* fired is not a property of the string: it is whether the value
    /// carries a cached numeric face, which lives on the payload (§2.3.4).
    pub fn would_warn(&self) -> bool {
        self.numify_noting_warning().1
    }
}

impl fmt::Write for PString {
    /// Append formatted text.  The only failure this can encounter is allocation, which `fmt::Error` cannot carry — use
    /// [`PString::push_fmt`] where the distinction matters; this impl exists so that `write!` works.
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push_str(s).map_err(|_| fmt::Error)
    }
}

impl PString {
    /// Append formatted text, reporting allocation failure precisely: `write!(s, ...)` through the [`fmt::Write`] impl
    /// flattens that into `fmt::Error`, which carries nothing.
    ///
    /// Formatting straight into the string is the point — rendering into a scratch buffer and copying the result in
    /// would allocate a second time for content the string can usually hold itself.
    pub fn push_fmt(&mut self, args: fmt::Arguments<'_>) -> Result<(), AllocError> {
        // `fmt::Error` carries nothing, so the real error is captured on the way past.
        struct Sink<'a> {
            target: &'a mut PString,
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

impl Default for PString {
    /// The empty string, per [`PString::empty`].
    fn default() -> PString {
        PString::empty()
    }
}

impl FromStr for PString {
    type Err = AllocError;

    /// The same construction as [`PString::new`], for generic contexts and `"...".parse()`.
    fn from_str(s: &str) -> Result<PString, AllocError> {
        PString::new(s)
    }
}

macro_rules! grid_hit {
    () => {
        #[cfg(test)]
        eq_probe::GRID_HITS.with(|c| c.set(c.get() + 1));
    };
}

impl PString {
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
    pub fn cmp_perl(&self, other: &PString) -> Ordering {
        if self.is_utf8() == other.is_utf8() {
            return self.cmp_raw_bytes(other);
        }

        let (flagged, plain) = if self.is_utf8() { (self, other) } else { (other, self) };
        let ordering = cmp_cross_flag(flagged, plain);

        if self.is_utf8() { ordering } else { ordering.reverse() }
    }

    /// Compare the raw bytes, which is what [`PString::cmp_perl`] reduces to when both sides read theirs the same way —
    /// unflagged octets being their own code points, and UTF-8 being order-preserving.
    ///
    /// Two packed strings of one alphabet compare as their nibbles do, the values being assigned in ASCII order, so
    /// neither side decodes.
    ///
    /// Private, and an optimization rather than a second ordering: there is one ordering on these values, and this is
    /// how it computes when neither side needs decoding.  It is *not* the `use bytes` comparison — that pragma changes
    /// which value is being compared, not how — and applied to operands whose flags differ it would report unlike
    /// things equal, which is why it cannot be the `Ord` impl.
    fn cmp_raw_bytes(&self, other: &PString) -> Ordering {
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
fn cmp_cross_flag(flagged: &PString, plain: &PString) -> Ordering {
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

impl PartialEq for PString {
    /// The §2.3.5 equality inference grid, then the single streaming dual-direction compare.  Consults existing scan
    /// knowledge only — never scans twice, never pre-scans.
    fn eq(&self, other: &PString) -> bool {
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

            // One side packed: compare against the other's bytes without materializing this side's.
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

        if (sf == scan::Utf8Latin1 || sf == scan::Utf8NonAscii) && plain.len() == flagged.len() {
            grid_hit!();
            return false; // a multi-byte sequence forces char count < byte count
        }

        // Grid row 3: ASCII vs known-non-ASCII, either orientation.
        if (sf == scan::Ascii && scan::is_known_non_ascii(sp)) || (sp == scan::Ascii && scan::is_known_non_ascii(sf)) {
            grid_hit!();
            return false;
        }

        // Grid row 4: cross-flag range disjointness and the malformed rule.
        if scan::is_known_beyond_latin1(sf) || sf == scan::MalformedUtf8 {
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
            cb.narrow_scan(if saw_non_ascii { scan::Utf8Latin1 } else { scan::Ascii });
        }

        if let RawParts::Heap(cb) = plain.raw_parts() {
            cb.narrow_scan(if saw_non_ascii { scan::NonAscii } else { scan::Ascii });
        }

        true
    }
}
impl Eq for PString {}

impl Ord for PString {
    /// Perl's `cmp`, which is the only ordering consistent with [`PartialEq`] and so the only one this trait can carry.
    ///
    /// A raw byte comparison is deliberately *not* this: two strings can share their internal bytes and still differ —
    /// an unflagged `"\xC3\xA9"` is two Latin-1 characters where a flagged one is `U+00E9` — so it would report them
    /// equal where equality reports them unequal, and `Ord` requires the two to agree.
    fn cmp(&self, other: &PString) -> Ordering {
        self.cmp_perl(other)
    }
}

impl PartialOrd for PString {
    fn partial_cmp(&self, other: &PString) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Hash for PString {
    /// Canonical downgraded-when-possible form (§2.3.5), routed through an internal 64-bit content digest: the `Hasher`
    /// API cannot fork mid-stream, and the single-fetch dual calculation (below) must run two candidate hashers and
    /// pick the winner at the end, so every string writes its digest — one `write_u64` — for cross-provenance
    /// consistency.  Warned and tainted bits are ignored (not part of string identity).
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.content_digest());
    }
}

impl PString {
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

        /// The per-process digest key (§2.3.5): the analog of perl's `PL_hash_seed`.  An unkeyed digest would let
        /// attackers precompute colliding keys offline (the pre-5.8.1 HashDoS posture); collisions in this inner digest
        /// collapse hash-map buckets regardless of the outer map's own seed, so the hardening must live here.  One
        /// state per process: digests must agree within a process and need not (must not, for hardening) agree across
        /// processes.
        static DIGEST_KEY: OnceLock<RandomState> = OnceLock::new();
        fn hasher() -> impl Hasher {
            DIGEST_KEY.get_or_init(RandomState::new).build_hasher()
        }

        /// Feeds a candidate's canonical byte stream to its hasher in provenance-independent chunks: identical streams
        /// issue identical `write` calls whether the bytes arrive as one slice, as block spans, or one character at a
        /// time.  The `Hasher` contract does not promise that mixed call shapes (`write` vs `write_u8`) hash alike, so
        /// the shape is owned here rather than borrowed from SipHasher's current behavior.
        struct ChunkFeed<H: Hasher> {
            h: H,
            buf: [u8; 64],
            n: usize,
        }

        impl<H: Hasher> ChunkFeed<H> {
            fn new(h: H) -> Self {
                ChunkFeed { h, buf: [0u8; 64], n: 0 }
            }

            fn push(&mut self, b: u8) {
                self.buf[self.n] = b;
                self.n += 1;
                if self.n == self.buf.len() {
                    self.h.write(&self.buf);
                    self.n = 0;
                }
            }

            fn extend(&mut self, bytes: &[u8]) {
                let mut rest = bytes;
                while !rest.is_empty() {
                    let take = (self.buf.len() - self.n).min(rest.len());
                    self.buf[self.n..self.n + take].copy_from_slice(&rest[..take]);
                    self.n += take;
                    rest = &rest[take..];
                    if self.n == self.buf.len() {
                        self.h.write(&self.buf);
                        self.n = 0;
                    }
                }
            }

            fn finish(mut self) -> u64 {
                if self.n > 0 {
                    self.h.write(&self.buf[..self.n]);
                }
                self.h.finish()
            }
        }

        let mut scratch = [0u8; DECODE_MAX];
        let bytes = self.as_bytes(&mut scratch);

        // Unflagged, or flagged with known-ASCII content: the raw bytes ARE the canonical downgraded form.
        if !self.is_utf8() || self.scan_state() == scan::Ascii {
            let mut feed = ChunkFeed::new(hasher());
            feed.extend(bytes);
            return feed.finish();
        }

        match self.scan_state() {
            // Known Latin-1 range: single decode-emit pass over the downgraded characters.
            scan::Utf8Latin1 => {
                count_full_scan();
                let mut feed = ChunkFeed::new(hasher());
                let mut facts = ScanFacts::default();
                let _ = scalar_decode_span(bytes, 0, bytes.len(), &mut facts, |v| feed.push(v as u8));
                feed.finish()
            }

            // Known beyond Latin-1 or invalid: the raw bytes are the canonical form.
            st if scan::is_known_beyond_latin1(st) || st == scan::MalformedUtf8 => {
                let mut feed = ChunkFeed::new(hasher());
                feed.extend(bytes);
                feed.finish()
            }

            // Unresolved: the blocked dual calculation.
            _ => {
                count_full_scan();
                let mut raw = ChunkFeed::new(hasher());
                let mut down = ChunkFeed::new(hasher());
                let mut downgradable = true;
                let mut facts = ScanFacts::default();
                let mut pos = 0usize;
                let mut malformed = false;

                while pos < bytes.len() {
                    let soft_end = block_end(pos, bytes.len());

                    // Exitless gate: a pure-ASCII block advances both candidates with the same bytes.
                    let hi = bytes[pos..soft_end].iter().fold(0u8, |a, &b| a | b) & 0x80 != 0;
                    if !hi {
                        raw.extend(&bytes[pos..soft_end]);
                        if downgradable {
                            down.extend(&bytes[pos..soft_end]);
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
                            down.push(v as u8);
                        }
                    });

                    match stop {
                        Some(next) => {
                            raw.extend(&bytes[pos..next]);
                            pos = next;
                        }
                        None => {
                            // Bytes: characters are undefined; the raw digest is the value's.  Finish the fetch
                            // raw-only.
                            raw.extend(&bytes[pos..]);
                            malformed = true;
                            downgradable = false;
                            pos = bytes.len();
                        }
                    }
                }

                // The pass classified the content — keep the knowledge (heap only; inline is terminal at birth).
                if let RawParts::Heap(cb) = self.raw_parts() {
                    if malformed {
                        cb.narrow_scan(scan::MalformedUtf8);
                    } else {
                        cb.narrow_scan(facts.state().widen());
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

/// The `string:` field of `PString`'s `Debug`: the lossless content rendering (§2.7.8).  The `b"…"` byte-string form
/// appears only for an unflagged string holding at least one high byte; pure-ASCII content renders as `"…"` whatever
/// the flag, and flagged strings render as `"…"` with UTF-8 assumed.  Printables verbatim, `\n`/`\t`/`\r` short forms,
/// self-escaping backslash and quote.  Escapes divide by width: a seven-bit character takes exactly two lowercase hex
/// digits in every string kind (one byte being one code point below `U+0080`), a code point at `U+0080` or above takes
/// at least four zero-padded to four, each rejected byte takes exactly two, and three are never emitted — so inside
/// `"…"` a two-digit escape at `0x80` or above can only be a rejected byte.  Nothing here allocates.
struct ContentDebug<'a>(&'a PString);

impl fmt::Debug for ContentDebug<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut scratch = [0u8; DECODE_MAX];
        let bytes = self.0.as_bytes(&mut scratch);
        if !self.0.is_utf8() {
            // The byte-string prefix only where it says something: pure-ASCII content renders as "…" whatever the flag,
            // ASCII being the subset over which the flag is value-invisible.  `is_ascii` consults the scan byte and
            // probes only when the lattice is genuinely undecided, narrowing it with the answer.
            if !self.0.is_ascii() {
                f.write_str("b")?;
            }

            f.write_str("\"")?;
            for &b in bytes {
                match b {
                    b'"' => f.write_str("\\\"")?,
                    b'\\' => f.write_str("\\\\")?,
                    b'\n' => f.write_str("\\n")?,
                    b'\r' => f.write_str("\\r")?,
                    b'\t' => f.write_str("\\t")?,
                    0x20..=0x7E => f.write_char(b as char)?,
                    _ => write!(f, "\\x{{{b:02x}}}")?,
                }
            }
        } else {
            f.write_str("\"")?;
            let mut i = 0;
            while i < bytes.len() {
                match decode_one(bytes, i) {
                    Some((len, v)) => {
                        match u32::try_from(v).ok().and_then(char::from_u32) {
                            Some('"') => f.write_str("\\\"")?,
                            Some('\\') => f.write_str("\\\\")?,
                            Some('\n') => f.write_str("\\n")?,
                            Some('\r') => f.write_str("\\r")?,
                            Some('\t') => f.write_str("\\t")?,
                            Some(c) if c.is_control() && v < 0x80 => write!(f, "\\x{{{v:02x}}}")?,
                            Some(c) if c.is_control() => write!(f, "\\x{{{:04x}}}", v)?,
                            Some(c) => f.write_char(c)?,

                            // A code point Rust cannot hold — supra-Unicode or a surrogate — well formed under perl.
                            None => write!(f, "\\x{{{:04x}}}", v)?,
                        }

                        i += len;
                    }
                    None => {
                        // Every byte of the rejected sequence, spelled as the raw byte it is.
                        for &b in &bytes[i..i + malformed_run(bytes, i)] {
                            write!(f, "\\x{{{b:02x}}}")?;
                            i += 1;
                        }
                    }
                }
            }
        }

        f.write_str("\"")
    }
}

/// The `bytes:` field of `PString`'s `Debug` for the envelope-resident tiers: the exact stored array in bare lowercase
/// hex, padding and auxiliary nibbles visible, because for those tiers the array *is* the representation and cleared
/// padding is an invariant worth seeing.
struct EnvelopeHex<'a>(&'a [u8]);

impl fmt::Debug for EnvelopeHex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (n, b) in self.0.iter().enumerate() {
            if n > 0 {
                f.write_str(" ")?;
            }

            write!(f, "{b:02x}")?;
        }

        Ok(())
    }
}

impl fmt::Debug for PString {
    /// The representation and the value, one struct (§2.7.8): the tier, the length, the per-value tag bits, the
    /// lossless content rendering under `string:`, and — for the envelope-resident tiers only — the exact envelope
    /// array under `bytes:`.  Pointer-backed tiers omit `bytes:`; their content is behind the pointer and `string:`
    /// already carries it losslessly.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("PString");
        d.field("storage", &self.storage_type())
            .field("len", &self.len())
            .field("utf8", &self.is_utf8())
            .field("tainted", &self.is_tainted())
            .field("string", &ContentDebug(self));

        match self.raw_parts() {
            RawParts::Inline { buf, .. } => {
                d.field("bytes", &EnvelopeHex(buf));
            }
            RawParts::Packed(ref p) => {
                d.field("bytes", &EnvelopeHex(&p.nibbles));
            }
            _ => {}
        }

        d.finish()
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

/// The nibble-array width in bytes: the envelope payload itself, the same fifteen bytes every inline form spans.
const PACKED_BYTES: usize = INLINE_MAX;

/// The packed-tier capacity in characters: two characters per nibble byte over the whole payload.
const MAX_PACKED_LEN: usize = 2 * PACKED_BYTES;

/// The shortest content this tier holds.  Content the inline forms can carry verbatim takes them instead (§2.2.9),
/// so the packed forms hold exactly 16-30 characters.  The band is established by the tier selector, the only path
/// that constructs strings; `pack` states it as a precondition rather than checking it.  It is also what lets the
/// stored length occupy four bits: only the low nibble varies across 16-29.
const MIN_PACKED_LEN: usize = INLINE_MAX + 1;

/// The nibble index holding the stored length, for content shorter than the capacity.
const LENGTH_NIBBLE: usize = MAX_PACKED_LEN - 1;

/// Which 16-symbol alphabet a packed string uses.  In `PString` this is not stored: it is folded into the tag, so each
/// alphabet has its own variants and the payload is fifteen nibble bytes with nothing else.
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
/// This is the working form, used while encoding and decoding.  In `PString` the first two fields do not exist — they
/// are folded into the tag, one variant per alphabet and length family — so a stored packed string is fifteen bytes of
/// nibbles and nothing else.  The fields here stand in for that tag while the value is in hand.
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

// ASCII-ordered symbol lists, space first.  Order is load-bearing: monotonic nibble assignment is what makes
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

// ─── The packed-UUID codec (§2.2.16) ────────────────────────────────────────────────────────────────────────────────
//
// Classification into a version form and a 15-byte payload, and reconstruction of the canonical 36-character spelling.
// A canonical hyphenated UUID is 36 characters: 32 hex digits with hyphens at positions 8, 13, 18, and 23.  Of its 128
// bits, the version nibble (digit 12) and the variant nibble's high two bits (digit 16 is `10xx`) are implied by the
// form, and each version family fixes or shards two further bits (§2.2.16), leaving exactly 120 — the payload.  The
// nibble layout is the spelling's own order with the implied nibbles removed:
//
// - v4, v3, v5 (shard forms): the 30 data nibbles in digit order, skipping digit 12 (version) and digit 16 (variant)
//   entirely; the variant's two data bits ride the tag shard.
// - v1, v6 (Gregorian): the top four timestamp bits sit at digit 13 for v1 — its timestamp runs low-first, the high
//   twelve bits landing after the version — and at digit 0 for v6, which reorders the same timestamp
//   most-significant-first.  That digit's top two bits are required zero (§2.2.16: through roughly 2496), and its two
//   live bits fuse with the variant's two data bits into one nibble — variant bits high, timestamp bits low — standing
//   where the digit stood; digits 12 and 16 are skipped.
// - v7 (Unix-millisecond): digit 0 carries the top four timestamp bits, top two required zero (§2.2.16: through roughly
//   4199), fused the same way and standing where digit 0 stood; digits 12 and 16 are skipped.
//
// Every form therefore stores exactly 30 nibbles in 15 bytes through the packed tier's own nibble helpers, and decoding
// reverses the same walk.  Classification is total over candidate bytes: anything that is not a canonical lowercase
// spelling of a recognized form is simply not a packed UUID, and the value takes ordinary storage (§2.2.16: the forms
// are capacity, never semantics).

/// The canonical spelling's length: the §2.2.16 UUID decode ceiling.
pub(crate) const UUID_LEN: usize = 36;

// The ladder's floor applies here too, though the length is fixed rather than gated: content the payload can carry
// verbatim takes an inline form, and no packed family may claim it (§2.2.9).
const _: () = assert!(UUID_LEN >= MIN_PACKED_LEN);

/// The hyphen positions in the canonical spelling.
const HYPHENS: [usize; 4] = [8, 13, 18, 23];

/// A recognized packed-UUID form: the version family, the hash forms carrying the variant nibble's two data bits as
/// their shard suffix (§2.2.16: shards for the hash forms, fixed bits for the time forms — the time forms carry those
/// two bits inside the payload instead).  Fifteen unit variants, one per storage type, so that minting can match
/// `(UuidForm, bool, bool)` exhaustively — sixty arms, no fallback.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum UuidForm {
    V1,
    V3S0,
    V3S1,
    V3S2,
    V3S3,
    V4S0,
    V4S1,
    V4S2,
    V4S3,
    V5S0,
    V5S1,
    V5S2,
    V5S3,
    V6,
    V7,
}

/// The shard variant for two data bits, per hash version.
fn uuid_shard(version: u8, bits: u8) -> UuidForm {
    match (version, bits & 0b11) {
        (3, 0) => UuidForm::V3S0,
        (3, 1) => UuidForm::V3S1,
        (3, 2) => UuidForm::V3S2,
        (3, 3) => UuidForm::V3S3,
        (4, 0) => UuidForm::V4S0,
        (4, 1) => UuidForm::V4S1,
        (4, 2) => UuidForm::V4S2,
        (4, 3) => UuidForm::V4S3,
        (5, 0) => UuidForm::V5S0,
        (5, 1) => UuidForm::V5S1,
        (5, 2) => UuidForm::V5S2,
        _ => UuidForm::V5S3,
    }
}

/// Classify a candidate as a canonical lowercase UUID of a recognized form, yielding the form and the 15-byte payload,
/// or `None` for anything else — which is not a failure, merely a value the family does not serve.
pub(crate) fn classify_uuid(bytes: &[u8]) -> Option<(UuidForm, [u8; PACKED_BYTES])> {
    if bytes.len() != UUID_LEN {
        return None;
    }

    // The 32 data nibbles in digit order, hyphens checked by position.  Uppercase fails here: the initial scope is
    // lowercase (§2.2.16), and mixed case must fail regardless.
    let mut nibbles = [0u8; 32];
    let mut d = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if HYPHENS.contains(&i) {
            if b != b'-' {
                return None;
            }

            continue;
        }

        nibbles[d] = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            _ => return None,
        };
        d += 1;
    }

    // The variant nibble must be the RFC `10xx` shape; its two data bits are what the forms carry.
    let variant = nibbles[16];
    if variant & 0b1100 != 0b1000 {
        return None;
    }
    let variant_bits = variant & 0b0011;

    match nibbles[12] {
        // Shard forms: drop digits 12 and 16; the variant bits become the shard.
        v @ 3..=5 => Some((uuid_shard(v, variant_bits), pack_skipping(&nibbles, None))),

        // Time forms: the fused nibble stands where the range-checked digit stood.
        1 => pack_time(&nibbles, 13, variant_bits).map(|p| (UuidForm::V1, p)),
        6 => pack_time(&nibbles, 0, variant_bits).map(|p| (UuidForm::V6, p)),
        7 => pack_time(&nibbles, 0, variant_bits).map(|p| (UuidForm::V7, p)),
        _ => None,
    }
}

/// Pack the 30 stored nibbles for a time form: the digit at `top` must have its top two bits zero (the §2.2.16 range
/// requirement), and the fused nibble — variant bits high, the digit's live bits low — stands in its place.
fn pack_time(nibbles: &[u8; 32], top: usize, variant_bits: u8) -> Option<[u8; PACKED_BYTES]> {
    if nibbles[top] & 0b1100 != 0 {
        return None;
    }

    let mut fused = *nibbles;
    fused[top] = (variant_bits << 2) | nibbles[top];
    Some(pack_skipping(&fused, Some(top)))
}

/// Pack digit order into 15 bytes, high nibble first, always skipping digits 12 and 16.  For the shard forms the
/// substituted position is `None` and the walk is the plain skip; the time forms pass their fused digit through
/// unchanged by position.
fn pack_skipping(nibbles: &[u8; 32], _fused_at: Option<usize>) -> [u8; PACKED_BYTES] {
    let mut payload = [0u8; PACKED_BYTES];
    let mut out = 0;
    for (i, &n) in nibbles.iter().enumerate() {
        if i == 12 || i == 16 {
            continue;
        }

        set_nibble(&mut payload, out, n);
        out += 1;
    }

    payload
}

/// Reconstruct the canonical lowercase spelling into `out`, returning the written length (always [`UUID_LEN`]).  The
/// inverse of [`classify_uuid`] by construction, which the round-trip tests pin.
pub(crate) fn decode_uuid(form: UuidForm, payload: &[u8; PACKED_BYTES], out: &mut [u8]) -> usize {
    // Unpack the 30 stored nibbles.
    let mut stored = [0u8; 30];
    for (i, slot) in stored.iter_mut().enumerate() {
        *slot = nibble_at(payload, i);
    }

    // Rebuild the 32 digit nibbles: reinsert the version at 12 and the variant at 16, and split any fused digit.
    let (version, variant_bits, fused_at) = match form {
        UuidForm::V1 => (1, None, Some(13)),
        UuidForm::V3S0 => (3, Some(0), None),
        UuidForm::V3S1 => (3, Some(1), None),
        UuidForm::V3S2 => (3, Some(2), None),
        UuidForm::V3S3 => (3, Some(3), None),
        UuidForm::V4S0 => (4, Some(0), None),
        UuidForm::V4S1 => (4, Some(1), None),
        UuidForm::V4S2 => (4, Some(2), None),
        UuidForm::V4S3 => (4, Some(3), None),
        UuidForm::V5S0 => (5, Some(0), None),
        UuidForm::V5S1 => (5, Some(1), None),
        UuidForm::V5S2 => (5, Some(2), None),
        UuidForm::V5S3 => (5, Some(3), None),
        UuidForm::V6 => (6, None, Some(0)),
        UuidForm::V7 => (7, None, Some(0)),
    };

    let mut nibbles = [0u8; 32];
    let mut src = 0;
    for (i, slot) in nibbles.iter_mut().enumerate() {
        if i == 12 || i == 16 {
            continue;
        }

        *slot = stored[src];
        src += 1;
    }

    nibbles[12] = version;
    let variant_bits = match (variant_bits, fused_at) {
        (Some(v), _) => v,
        (None, Some(at)) => {
            let fused = nibbles[at];
            nibbles[at] = fused & 0b0011;
            fused >> 2
        }

        // Unreachable by the form table above; zero keeps the function total.
        (None, None) => 0,
    };
    nibbles[16] = 0b1000 | variant_bits;

    // Render: 32 hex digits with the four hyphens by position.
    let mut d = 0;
    for (i, o) in out.iter_mut().take(UUID_LEN).enumerate() {
        if HYPHENS.contains(&i) {
            *o = b'-';
            continue;
        }

        let n = nibbles[d];
        *o = if n < 10 { b'0' + n } else { b'a' + (n - 10) };
        d += 1;
    }

    UUID_LEN
}

// ─── The packed hex-byte codec (§2.2.16) ────────────────────────────────────────────────────────────────────────────
//
// A hex string of D digits, byte-separated, plain, or `0x`-prefixed.  Fourteen payload bytes hold the digits two per
// byte, high nibble first, and the fifteenth byte holds the metadata: nibble 28 the length code, nibble 29 the
// variation.  The length code carries every count by the formula *zero means 12, otherwise 13 plus the code*, which
// covers 12 and 14 through 28 with a single hole at 13 — placed where it costs least, since plain and prefixed
// thirteens are inline and only the separated thirteen-digit spellings lose it.  The variation is a three-bit format
// code and a case bit; five codes are assigned and three are unassigned, reachable only by corruption.
//
// Every format admits every count its rendering can express: digits pair left to right, and a separated spelling
// carries a trailing lone digit for an odd count, whose unused nibble stays zero by construction.  Case is a
// whole-string property of the digits alone: the `0x` prefix is always lowercase, so `0xABCD` packs and `0XABCD`
// spills, and all-digit content is canonically lowercase because the two spellings render identically.
//
// The family sits after the nibble alphabets in the selection order (§2.2.16), so digit strings they can represent stay
// theirs; what reaches here is content bearing `a`-`f` or `A`-`F`, and the all-digit separated spellings of 31 through
// 41 characters, which lie past the alphabets' thirty-character ceiling.  Classification is total over candidate bytes:
// anything else is simply not a hex-byte string, and the value takes ordinary storage.

/// The longest rendering any format produces: twenty-eight digits separated is forty-one characters, the widest decode
/// in the crate and so `DECODE_MAX`'s tallest entry.
const HEX_MAX_LEN: usize = 41;

/// The digit ceiling: fourteen payload bytes at two digits each.
const HEX_MAX_DIGITS: usize = 28;

/// The metadata nibbles in the fifteenth payload byte.
const HEX_LENGTH_NIBBLE: usize = 28;
const HEX_VARIATION_NIBBLE: usize = 29;

/// The case bit within the variation nibble; the low three bits are the format code.
const HEX_UPPER_BIT: u8 = 0b1000;

/// A recognized hex-byte spelling (§2.2.16).  The discriminants are the assigned format codes, and the plain spelling
/// takes zero on the house polarity: the undecorated reading is what a zeroed nibble should mean, not some decoration
/// nothing asked for.  Codes five through seven are unassigned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HexFormat {
    Plain = 0,
    Colon = 1,
    Hyphen = 2,
    Space = 3,
    PrefixOnce = 4,
}

// The polarity, pinned: moving the plain spelling off zero would make a zeroed variation nibble decode as a separated
// one.
const _: () = assert!(HexFormat::Plain as u8 == 0);

impl HexFormat {
    /// The separator standing between digit pairs, or `None` where the spelling has none.
    fn separator(self) -> Option<u8> {
        match self {
            HexFormat::Colon => Some(b':'),
            HexFormat::Hyphen => Some(b'-'),
            HexFormat::Space => Some(b' '),
            HexFormat::Plain | HexFormat::PrefixOnce => None,
        }
    }
}

/// The digit count a length code carries: zero means twelve, and every other code means thirteen plus itself.
fn hex_digits_of(code: u8) -> usize {
    if code == 0 { 12 } else { 13 + code as usize }
}

/// The length code for a digit count, or `None` for the counts no code carries — below twelve, above twenty-eight, and
/// the hole at thirteen.
fn hex_length_code(digits: usize) -> Option<u8> {
    match digits {
        12 => Some(0),
        14..=28 => Some((digits - 13) as u8),
        _ => None,
    }
}

/// Classify a candidate as a hex-byte string of a recognized spelling, yielding the payload, or `None` for anything
/// else — which is not a failure, merely a value the family does not serve.
fn classify_hex_bytes(bytes: &[u8]) -> Option<[u8; PACKED_BYTES]> {
    // The floor is the ladder's, shared with the alphabets: one past what the payload carries verbatim, since hex
    // digits are ASCII and never compress.  No per-format floor is needed on top of it — the spellings shorter than
    // this are exactly the ones the inline forms already hold (§2.2.16).
    if !(MIN_PACKED_LEN..=HEX_MAX_LEN).contains(&bytes.len()) {
        return None;
    }

    // The format is one lookahead, never a trial of each spelling: a `0x` front is the prefixed form, a separator
    // standing where the first one would is that separator, and anything else is plain — and is rejected below if it is
    // not a hex digit after all.
    let format = if bytes.starts_with(b"0x") {
        HexFormat::PrefixOnce
    } else {
        match bytes[2] {
            b':' => HexFormat::Colon,
            b'-' => HexFormat::Hyphen,
            b' ' => HexFormat::Space,
            _ => HexFormat::Plain,
        }
    };

    let tail = if format == HexFormat::PrefixOnce { &bytes[2..] } else { bytes };
    let separator = format.separator();

    let mut payload = [0u8; PACKED_BYTES];
    let mut digits = 0usize;
    let mut saw_lower = false;
    let mut saw_upper = false;
    let mut i = 0usize;
    while i < tail.len() {
        // Between groups stands exactly one separator, where the spelling has one.
        if digits > 0
            && let Some(sep) = separator
        {
            if tail[i] != sep {
                return None;
            }

            i += 1;
        }

        // A group is two digits, or one where an odd count ends the string.
        for _ in 0..2 {
            if i >= tail.len() {
                break;
            }

            let value = match tail[i] {
                b @ b'0'..=b'9' => b - b'0',
                b @ b'a'..=b'f' => {
                    saw_lower = true;
                    b - b'a' + 10
                }
                b @ b'A'..=b'F' => {
                    saw_upper = true;
                    b - b'A' + 10
                }
                _ => return None,
            };

            if digits >= HEX_MAX_DIGITS {
                return None;
            }

            set_nibble(&mut payload, digits, value);
            digits += 1;
            i += 1;
        }
    }

    // Mixed case is no spelling of ours, and all-digit content is canonically lowercase: the two render alike.
    if saw_lower && saw_upper {
        return None;
    }

    let code = hex_length_code(digits)?;
    set_nibble(&mut payload, HEX_LENGTH_NIBBLE, code);
    let variation = format as u8 | if saw_upper { HEX_UPPER_BIT } else { 0 };
    set_nibble(&mut payload, HEX_VARIATION_NIBBLE, variation);
    Some(payload)
}

/// The format and case a payload's variation nibble carries.  Total over the three-bit field by construction, so no
/// branch here can fail: the plain spelling answers for zero, which is its own code, and for the three unassigned ones,
/// which classification never emits.  Assigning a code gives it an arm above, and the round-trip tests fail at once if
/// a spelling is added on the classifying side alone.
fn hex_variation(payload: &[u8; PACKED_BYTES]) -> (HexFormat, bool) {
    let variation = nibble_at(payload, HEX_VARIATION_NIBBLE);
    let format = match variation & 0b0111 {
        1 => HexFormat::Colon,
        2 => HexFormat::Hyphen,
        3 => HexFormat::Space,
        4 => HexFormat::PrefixOnce,

        // Zero is the plain spelling; the three unassigned codes read as it too.
        _ => HexFormat::Plain,
    };

    (format, variation & HEX_UPPER_BIT != 0)
}

/// The rendered length of a payload, without decoding it: the length answer every consumer asks for.
fn hex_rendered_len(payload: &[u8; PACKED_BYTES]) -> usize {
    let digits = hex_digits_of(nibble_at(payload, HEX_LENGTH_NIBBLE));
    let (format, _) = hex_variation(payload);
    match format {
        HexFormat::Plain => digits,
        HexFormat::PrefixOnce => 2 + digits,

        // One separator between groups: the groups are the digit pairs, the last possibly lone.
        _ => digits + digits.div_ceil(2) - 1,
    }
}

/// Reconstruct the spelling into `out`, returning the written length.  The inverse of [`classify_hex_bytes`] by
/// construction, which the round-trip tests pin.
fn decode_hex_bytes(payload: &[u8; PACKED_BYTES], out: &mut [u8]) -> usize {
    // Every spelling fits the scratch every caller supplies: the widest is the separated one at `HEX_MAX_LEN`, which
    // `DECODE_MAX` is defined to cover, so the writes below need no bound of their own.
    let digits = hex_digits_of(nibble_at(payload, HEX_LENGTH_NIBBLE));
    let (format, upper) = hex_variation(payload);
    let separator = format.separator();
    let alpha = if upper { b'A' } else { b'a' };

    let mut n = 0;
    if format == HexFormat::PrefixOnce {
        out[0] = b'0';
        out[1] = b'x';
        n = 2;
    }

    for d in 0..digits {
        if d > 0
            && d.is_multiple_of(2)
            && let Some(sep) = separator
        {
            out[n] = sep;
            n += 1;
        }

        let value = nibble_at(payload, d);
        out[n] = if value < 10 { b'0' + value } else { alpha + (value - 10) };
        n += 1;
    }

    n
}

// ─── The slicing mints (§2.2.15) ─────────────────────────────────────────────────────────────────────────────────────

/// Mint a view of a Heap32 buffer under the ruled selection: far while the length fits u16, medium for the band only it
/// serves, and past both the LargeSlice case — an `Adopted` child holding the buffer, worn as a whole-object envelope.
///
/// # Safety
/// `raw` must be a live Heap32 allocation with at least `offset + len` initialized bytes.
unsafe fn heap32_view(raw: std::ptr::NonNull<u8>, offset: usize, len: usize, scan: scan::ScanState, utf8: bool, tainted: bool) -> Result<PString, AllocError> {
    if len <= u16::MAX as usize && offset <= u32::MAX as usize {
        // SAFETY: live per the contract; the envelope owns the reference this retain adds.
        let ptr = unsafe {
            cow_buffer::heap32::retain(raw);
            Owned::from_raw(raw)
        };

        return Ok(PString::build_view(ViewBacking::Heap32Far, utf8, tainted, ptr, offset, len, scan));
    }

    if offset < SPAN as usize && len < SPAN as usize {
        // SAFETY: as above.
        let ptr = unsafe {
            cow_buffer::heap32::retain(raw);
            Owned::from_raw(raw)
        };

        return Ok(PString::build_view(ViewBacking::Heap32Medium, utf8, tainted, ptr, offset, len, scan));
    }

    // SAFETY: live per the contract; the child takes its own retain and the envelope owns the child.
    let child = unsafe { cow_buffer::Adopted::adopt_heap_buf(raw, Tier::Heap32, 0, offset, len, scan) }?;

    Ok(PString::adopted_whole(child, utf8, tainted))
}

/// Mint a view of an adopted object under the same selection, the LargeAdopted case being a `Parent` child.
///
/// # Safety
/// `backing` must be a live `Adopted` with `offset + len` within its object.
unsafe fn adopted_view(
    backing: std::ptr::NonNull<cow_buffer::Adopted>,
    offset: usize,
    len: usize,
    scan: scan::ScanState,
    utf8: bool,
    tainted: bool,
) -> Result<PString, AllocError> {
    let far = len <= u16::MAX as usize && offset <= u32::MAX as usize;
    if far || (offset < SPAN as usize && len < SPAN as usize) {
        // SAFETY: live per the contract; the envelope owns the reference this retain adds.
        let ptr = unsafe {
            cow_buffer::Adopted::retain(backing);
            Owned::from_raw(backing.cast())
        };

        let kind = if far { ViewBacking::AdoptedFar } else { ViewBacking::Adopted };
        return Ok(PString::build_view(kind, utf8, tainted, ptr, offset, len, scan));
    }

    // SAFETY: live per the contract; the child retains the parent and the envelope owns the child.
    let child = unsafe { cow_buffer::Adopted::adopt_span_of(backing, offset, len, scan) }?;

    Ok(PString::adopted_whole(child, utf8, tainted))
}

// ─── View births (§2.2.15) ───────────────────────────────────────────────────────────────────────────────────────────

/// Whether the cut at `offset..offset + len` of `parent` splits no sequence: two O(1) continuation-byte tests, valid
/// for the perl-extended forms too, since every continuation byte is `0x80..=0xBF` under both readings (§2.2.3).  The
/// empty view and the whole-object cut are clean by construction.
#[cfg_attr(not(test), allow(dead_code))]
fn cut_is_clean(parent: &[u8], offset: usize, len: usize) -> bool {
    let start_clean = offset == 0 || offset >= parent.len() || (parent[offset] & 0xC0) != 0x80;
    let end = offset + len;
    let end_clean = end >= parent.len() || (parent[end] & 0xC0) != 0x80;

    start_clean && end_clean
}

/// The slice-birth state (§2.2.3): nothing is scanned, so the birth state is what clean cuts provably preserve —
/// witnesses may be excluded by the cut, range and validity bounds survive — and a dirty cut of any validity-asserting
/// source is *proven* malformed by the cut, terminal and free.
#[cfg_attr(not(test), allow(dead_code))]
fn slice_birth(parent: scan::ScanState, clean: bool) -> scan::ScanState {
    use scan::ScanState::*;

    if !clean {
        return match parent {
            // A dirty cut through content these states certify decodable severs a sequence: the view provably holds an
            // incomplete form.
            Utf8Latin1 | MaybeUtf8Latin1 | Utf8NonLatin1 | Utf8NonAscii | ValidUtf8 | ExtendedUtf8 | MaybeExtendedUtf8 | PerlValidNonAscii => MalformedUtf8,

            // Ascii cannot cut dirty (no continuation bytes exist to split); the rest asserted no validity for the cut
            // to disprove.
            Ascii => Ascii,
            Unknown | NonAscii | MalformedUtf8 => Unknown,
        };
    }

    match parent {
        Unknown => Unknown,
        Ascii => Ascii,
        Utf8Latin1 | MaybeUtf8Latin1 => MaybeUtf8Latin1,
        Utf8NonLatin1 | Utf8NonAscii | ValidUtf8 => ValidUtf8,
        ExtendedUtf8 | MaybeExtendedUtf8 | PerlValidNonAscii => MaybeExtendedUtf8,

        // The witness may be excluded; nothing else was asserted.
        MalformedUtf8 | NonAscii => Unknown,
    }
}

/// The slice-eager floor (§2.2.3, 4 KiB [DECISION]): below it a view classifies at birth — one cheap pass beats a scan
/// per descendant, since view envelopes cannot propagate narrowing.
#[cfg_attr(not(test), allow(dead_code))]
const SLICE_EAGER_FLOOR: usize = 4 * 1024;

/// The birth state a view of `parent_bytes[offset..offset + len]` carries in its envelope: the table's answer, improved
/// to a full classification below the eager floor.
#[cfg_attr(not(test), allow(dead_code))]
fn view_birth_state(parent: scan::ScanState, parent_bytes: &[u8], offset: usize, len: usize) -> scan::ScanState {
    let table = slice_birth(parent, cut_is_clean(parent_bytes, offset, len));
    if len < SLICE_EAGER_FLOOR && !matches!(table, scan::Ascii | scan::MalformedUtf8) {
        return classify_full(&parent_bytes[offset..offset + len]).0.widen();
    }

    table
}

// ─── Display (§2.7.8) ────────────────────────────────────────────────────────────────────────────────────────────────

/// What `Formatter::pad` would compute from the format spec, reproduced because `pad` takes a `&str` this content
/// cannot always be: the character count after precision truncation, and the fill split per alignment — default left
/// for strings, center putting its odd column on the right, matching std.
fn pad_plan(f: &fmt::Formatter<'_>, count: usize) -> (usize, usize, usize) {
    let effective = match f.precision() {
        Some(p) if p < count => p,
        _ => count,
    };

    let (mut left, mut right) = (0, 0);
    if let Some(width) = f.width()
        && width > effective
    {
        let pad = width - effective;
        match f.align() {
            Some(fmt::Alignment::Right) => left = pad,
            Some(fmt::Alignment::Center) => {
                left = pad / 2;
                right = pad - pad / 2;
            }
            _ => right = pad,
        }
    }

    (effective, left, right)
}

fn write_fill(f: &mut fmt::Formatter<'_>, n: usize) -> fmt::Result {
    let fill = f.fill();
    for _ in 0..n {
        f.write_char(fill)?;
    }

    Ok(())
}

/// The flagged lossy body (§2.7.8): one glyph per decode step — the character where Rust can hold it, `U+FFFD` where it
/// cannot or where the decoder rejected a sequence — stopping after `limit` steps.  The loop is the reporting
/// decoder's, restated here because the formatter must thread a write error out of the walk, and stops early both on
/// the limit and on the sink's first refusal.
fn fmt_flagged_lossy(bytes: &[u8], limit: usize, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut i = 0;
    let mut left = limit;
    while i < bytes.len() && left > 0 {
        match decode_one(bytes, i) {
            Some((len, v)) => {
                let c = u32::try_from(v).ok().and_then(char::from_u32).unwrap_or('\u{FFFD}');
                f.write_char(c)?;
                i += len;
            }
            None => {
                f.write_char('\u{FFFD}')?;
                i += malformed_run(bytes, i);
            }
        }

        left -= 1;
    }

    Ok(())
}

/// The rendered glyph count of flagged content with no cached character count — the malformed terminal, whose rendering
/// is one glyph per decode step, replacements included.  Allocation-free by construction: the counting walk is the
/// render walk with the writes removed.
fn lossy_steps(bytes: &[u8]) -> usize {
    let mut i = 0;
    let mut steps = 0;
    while i < bytes.len() {
        i += match decode_one(bytes, i) {
            Some((len, _)) => len,
            None => malformed_run(bytes, i),
        };
        steps += 1;
    }

    steps
}

impl fmt::Display for PString {
    /// Lossy rendering per §2.7.8.  An unflagged string is one code point per byte, widened on the way out, and never
    /// produces a replacement; flagged content Rust can represent is written through unchanged; an unrepresentable code
    /// point or a rejected sequence is one `U+FFFD` each.  Width, precision, and fill are honored with `pad`'s
    /// semantics — the cached character count supplies the length, and only the malformed terminal pays a counting walk
    /// first.
    ///
    /// Total per §2.7.8: allocates nothing, consults no magic, runs no user code, so there is no `try_` twin — the only
    /// `Err` out of here is the sink's own (§2.7.1's bargain is not needed).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut scratch = [0u8; DECODE_MAX];
        if !self.is_utf8() {
            // One code point per byte: the character count is the byte length, and truncation is a byte slice.
            let bytes = self.as_bytes(&mut scratch);
            let (effective, left, right) = pad_plan(f, bytes.len());
            write_fill(f, left)?;

            cow_buffer::widen_latin1::<fmt::Error>(&bytes[..effective], |chunk| {
                // SAFETY: every chunk is a maximal ASCII run of the source or whole two-byte encodings the widen just
                // built — valid UTF-8 on its own, per `widen_latin1`'s contract.
                f.write_str(unsafe { str::from_utf8_unchecked(chunk) })
            })?;

            write_fill(f, right)
        } else {
            // `char_len` classifies and narrows on demand, so the state read below is post-narrowing knowledge; `None`
            // is the malformed terminal, whose rendered count is the counting walk's.
            let counted = self.char_len();
            let bytes = self.as_bytes(&mut scratch);
            let count = match counted {
                Some(n) => n,
                None => lossy_steps(bytes),
            };

            let (effective, left, right) = pad_plan(f, count);
            write_fill(f, left)?;

            if scan::is_rust_valid(self.scan_state()) {
                // SAFETY: the state asserts the bytes are valid UTF-8 (§2.2.4).
                let s = unsafe { str::from_utf8_unchecked(bytes) };
                if effective == count {
                    f.write_str(s)?;
                } else {
                    match s.char_indices().nth(effective) {
                        Some((cut, _)) => f.write_str(&s[..cut])?,
                        None => f.write_str(s)?, // unreachable: effective < count means a cut index exists
                    }
                }
            } else {
                fmt_flagged_lossy(bytes, effective, f)?;
            }

            write_fill(f, right)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/string_tests.rs"]
mod tests;
