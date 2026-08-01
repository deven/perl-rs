//! Numeric payloads and the space their digit caches occupy (§2.2.9).
//!
//! A numeric value is eight bytes of datum and the envelope has fifteen payload bytes, so seven are spare.  They hold
//! the digits of the value's default stringification, which is otherwise recomputed on every print, hash-key use, and
//! interpolation.
//!
//! # Why these are `repr(packed)`, and why `PerlString` is `repr(align(8))`
//!
//! The discriminant is not a byte the layout sets aside: it lives in the niche of [`PerlString`](crate::string)'s own
//! tag, which uses 96 of its 256 values.  Niche-filling requires every other variant's data to avoid that byte, and
//! Rust lays a variant out as a self-contained struct *before* placing it — so a field wanting eight-byte alignment
//! lands at offset 8 and fills the payload, leaving nowhere for a cache.  Adding even one cache byte beside a bare
//! `i64` costs eight, for the same reason a taint byte did before taint moved into the discriminant.
//!
//! What does work, measured against what does not:
//!
//! | arrangement                                                  |  size  |
//! |--------------------------------------------------------------|--------|
//! | `Integer(i64)` — the datum alone                             |   16   |
//! | `Integer(i64, [u8; 1])` — one cache byte beside it           |   24   |
//! | `Integer([u8; 7], i64)` — cache first                        |   24   |
//! | `Integer(SomeReprCStruct)` — the pair as a struct            |   24   |
//! | `#[repr(align(8))]` on the *enclosing* enum                  |   24   |
//! | **`repr(packed)` payload, `repr(align(8))` on `PerlString`** | **16** |
//!
//! `repr(packed)` gives these structs alignment 1, so the whole struct sits at offset 1 and the datum lands at envelope
//! offset 8 — with seven bytes ahead of it for the cache.  The eight-byte alignment that makes offset 8 a real boundary
//! comes from `PerlString` carrying `repr(align(8))`: applied *there* it is free, that type being sixteen bytes
//! already, and the enclosing enums inherit alignment from their largest variant.  Applied to `Value` directly it
//! defeats niche-filling and costs eight bytes, which is the arrangement that does not work.
//!
//! So every datum access is an ordinary aligned load.  `repr(packed)` buys the layout here, not unaligned reads.
//!
//! # What the cache will hold
//!
//! The digits of perl's `%.15g` output, never shortest-round-trip, so a cache cannot leak a formatting divergence.  It
//! is all-or-nothing: a rendering that does not fit is not cached at all, because completing a partial digit sequence
//! correctly needs the high-precision remainder state the digits came from.  The sign is not stored, being recoverable
//! from the datum, which buys a digit of capacity.
//!
//! The digits themselves, and the protocol that fills them, are the next step: filling through a shared reference needs
//! atomic cache bytes, and filling only where a caller holds the value mutably misses shared-container reads.  §2.2.9
//! records that as an open sub-decision.  This module establishes the space and proves it fits.

use std::fmt;

use crate::cow_buffer::AllocError;
use crate::string::PerlString;
use crate::value::{float_digits, format_float_into, format_int_into, present_float};

/// Cache bytes available beside an eight-byte datum: the envelope's fifteen, less the datum's eight.
pub(crate) const CACHE_BYTES: usize = 7;

/// The digits of a value's default rendering, or nothing.
///
/// Byte 0 is the digit count, and zero means empty — free as a marker, since every finite rendering has at least one
/// digit.  The remaining bytes hold the decimal exponent and the digits themselves, two per byte.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
///
/// `DIGITS_AT` is where the digits begin, which differs by kind: floats spend byte 1 on the decimal exponent and
/// integers do not, so integers hold two more digits in the same seven bytes.
pub(crate) struct DigitCache<const DIGITS_AT: usize>([u8; CACHE_BYTES]);

impl<const DIGITS_AT: usize> DigitCache<DIGITS_AT> {
    /// No digits: never rendered, or rendered too long to hold.
    pub(crate) const EMPTY: DigitCache<DIGITS_AT> = DigitCache([0; CACHE_BYTES]);

    /// Digits this layout holds: the bytes past the count and any exponent, two digits each.
    pub(crate) const CAPACITY: usize = (CACHE_BYTES - DIGITS_AT) * 2;

    /// The cached digit count, or `None` when empty.
    pub(crate) fn count(self) -> Option<usize> {
        match self.0[0] {
            0 => None,
            n => Some(n as usize),
        }
    }

    /// The decimal exponent stored beside the digits.  Meaningless for integers, which never consult it.
    fn exponent(self) -> i32 {
        self.0[1] as i8 as i32
    }

    /// One digit, high nibble first so that byte order follows digit order.
    fn digit(self, index: usize) -> u8 {
        let byte = self.0[DIGITS_AT + index / 2];
        if index.is_multiple_of(2) { byte >> 4 } else { byte & 0x0F }
    }

    /// The digits, copied out for rendering.
    fn digits(self, count: usize) -> [u8; MAX_DIGITS] {
        let mut out = [0u8; MAX_DIGITS];
        for (i, slot) in out.iter_mut().enumerate().take(count) {
            *slot = self.digit(i);
        }

        out
    }

    /// Build from decimal digits and an exponent, or stay empty when they do not fit.
    ///
    /// All or nothing: a rendering too long to hold is not cached in part, because completing a partial digit sequence
    /// correctly needs the high-precision remainder state the digits came from.
    fn build(digits: &[u8], exponent: i32) -> DigitCache<DIGITS_AT> {
        if digits.is_empty() || digits.len() > Self::CAPACITY || !(-128..=127).contains(&exponent) {
            return DigitCache::EMPTY;
        }

        let mut out = [0u8; CACHE_BYTES];
        out[0] = digits.len() as u8;
        out[1] = exponent as i8 as u8;

        for (i, &d) in digits.iter().enumerate() {
            debug_assert!(d < 10, "cache digits are decimal");
            let slot = &mut out[DIGITS_AT + i / 2];
            if i.is_multiple_of(2) {
                *slot = (*slot & 0x0F) | (d << 4);
            } else {
                *slot = (*slot & 0xF0) | d;
            }
        }

        DigitCache(out)
    }
}

/// The widest digit run any layout holds, for buffers that must fit either.
///
/// Neither capacity covers `%.15g`'s fifteen digits, deliberately.  The measured distribution is bimodal: parse-born
/// and money-shaped values at one to four digits, arithmetic artifacts at fourteen to fifteen, almost nothing between.
/// Both capacities cover the short mode completely, and the long mode takes the empty cache.
pub(crate) const MAX_DIGITS: usize = (CACHE_BYTES - 1) * 2;

/// Integers store digits from byte 1: no exponent to make room for.
type IntegerCache = DigitCache<1>;

/// Floats spend byte 1 on the decimal exponent.
type FloatCache = DigitCache<2>;

impl<const DIGITS_AT: usize> fmt::Debug for DigitCache<DIGITS_AT> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.count() {
            Some(n) => write!(f, "{n} digits"),
            None => f.write_str("empty"),
        }
    }
}

macro_rules! numeric_payload {
    ($ty:ident, $inner:ty, $cache:ty, $doc:literal) => {
        #[doc = $doc]
        ///
        /// `repr(packed)` for layout, not for unaligned access: see the module documentation.
        #[repr(Rust, packed)]
        #[derive(Clone, Copy, Default)]
        pub struct $ty {
            cache: $cache,
            value: $inner,
        }

        impl $ty {
            /// A payload with no cached digits.
            pub fn new(value: $inner) -> $ty {
                $ty { cache: <$cache>::EMPTY, value }
            }

            /// The datum.  An ordinary aligned load — the packed layout places it at envelope offset 8.
            pub fn value(self) -> $inner {
                self.value
            }

            /// Whether this value's rendering is cached.
            pub fn is_cached(self) -> bool {
                self.cache.count().is_some()
            }
        }

        impl PartialEq for $ty {
            /// The datum alone: a cache is derived, so two payloads holding the same value are equal whether or not
            /// either has been rendered.
            fn eq(&self, other: &$ty) -> bool {
                self.value() == other.value()
            }
        }

        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // The datum is what a reader wants; the cache is derived, and worth a word only when present.
                let value = self.value();
                if self.is_cached() { write!(f, "{value:?} (cached)") } else { write!(f, "{value:?}") }
            }
        }
    };
}

numeric_payload!(IntegerPayload, i64, IntegerCache, "A signed integer and the digits of its rendering.");
numeric_payload!(UnsignedPayload, u64, IntegerCache, "An integer in `[2^63, 2^64)` and the digits of its rendering.");
numeric_payload!(FloatPayload, f64, FloatCache, "A float and the digits of its `%.15g` rendering.");

impl IntegerPayload {
    /// Render, from the cached digits when they are there.
    pub(crate) fn render(self, out: &mut PerlString) -> Result<(), AllocError> {
        match self.cache.count() {
            Some(count) => emit_digits(self.cache.digits(count), count, self.value() < 0, out),
            None => format_int_into(self.value(), out),
        }
    }

    /// A copy carrying the digits of its own rendering, when they fit.
    pub(crate) fn filled(self) -> IntegerPayload {
        let value = self.value();
        let magnitude = value.unsigned_abs();
        IntegerPayload { cache: decimal_cache(magnitude), value: self.value }
    }
}

impl UnsignedPayload {
    /// Render, from the cached digits when they are there.
    pub(crate) fn render(self, out: &mut PerlString) -> Result<(), AllocError> {
        match self.cache.count() {
            Some(count) => emit_digits(self.cache.digits(count), count, false, out),
            None => out.push_fmt(format_args!("{}", self.value())),
        }
    }

    /// A copy carrying the digits of its own rendering, when they fit.
    pub(crate) fn filled(self) -> UnsignedPayload {
        UnsignedPayload { cache: decimal_cache(self.value()), value: self.value }
    }
}

impl FloatPayload {
    /// Render, from the cached digits when they are there.
    ///
    /// The cache holds what the expensive half of float formatting produces — the significant digits and the decimal
    /// exponent — so a cached render is only `%g`'s presentation step.  Specials never cache: they have no digits.
    pub(crate) fn render(self, out: &mut PerlString) -> Result<(), AllocError> {
        match self.cache.count() {
            Some(count) => {
                let digits = self.cache.digits(count);
                present_float(&digits[..count], self.cache.exponent(), self.value().is_sign_negative(), out)
            }
            None => format_float_into(self.value(), out),
        }
    }

    /// A copy carrying the digits of its own rendering, when they fit.
    pub(crate) fn filled(self) -> FloatPayload {
        let cache = match float_digits(self.value()) {
            Some((digits, count, exp)) => DigitCache::build(&digits[..count], exp),
            None => DigitCache::EMPTY, // Specials and zero: no digits to hold.
        };

        FloatPayload { cache, value: self.value }
    }
}

/// The decimal digits of a magnitude, as a cache.  The sign is not stored — the datum carries it.
fn decimal_cache(magnitude: u64) -> IntegerCache {
    let mut digits = [0u8; 20];
    let mut count = 0;
    let mut rest = magnitude;
    loop {
        digits[count] = (rest % 10) as u8;
        count += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }

    digits[..count].reverse();
    DigitCache::build(&digits[..count], 0)
}

/// Write a sign and a run of decimal digits.
fn emit_digits(digits: [u8; MAX_DIGITS], count: usize, negative: bool, out: &mut PerlString) -> Result<(), AllocError> {
    let mut buf = [0u8; MAX_DIGITS + 1];
    let mut len = 0;

    if negative {
        buf[0] = b'-';
        len = 1;
    }

    for &d in digits.iter().take(count) {
        buf[len] = b'0' + d;
        len += 1;
    }

    out.push_bytes(&buf[..len])
}

impl Eq for IntegerPayload {}
impl Eq for UnsignedPayload {}

// ── Layout law (§2.3.6) ───────────────────────────────────────────
//
// Alignment 1 is the point: it is what lets the struct sit at envelope offset 1, clear of the discriminant's niche.
const _: () = assert!(size_of::<IntegerPayload>() == 15);
const _: () = assert!(align_of::<IntegerPayload>() == 1);
const _: () = assert!(size_of::<UnsignedPayload>() == 15);
const _: () = assert!(size_of::<FloatPayload>() == 15);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/numeric_tests.rs"]
mod tests;
