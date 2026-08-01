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

/// Cache bytes available beside an eight-byte datum: the envelope's fifteen, less the datum's eight.
pub(crate) const CACHE_BYTES: usize = 7;

/// The digits of a value's default rendering, or nothing.
///
/// Byte 0 is the digit count, and zero means empty — free as a marker, since every finite rendering has at least one
/// digit.  The remaining bytes hold the decimal exponent and the digits themselves, two per byte.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct DigitCache([u8; CACHE_BYTES]);

impl DigitCache {
    /// No digits: never rendered, or rendered too long to hold.
    pub(crate) const EMPTY: DigitCache = DigitCache([0; CACHE_BYTES]);

    /// The cached digit count, or `None` when empty.
    pub(crate) fn count(self) -> Option<usize> {
        match self.0[0] {
            0 => None,
            n => Some(n as usize),
        }
    }
}

impl fmt::Debug for DigitCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.count() {
            Some(n) => write!(f, "{n} digits"),
            None => f.write_str("empty"),
        }
    }
}

macro_rules! numeric_payload {
    ($ty:ident, $inner:ty, $doc:literal) => {
        #[doc = $doc]
        ///
        /// `repr(packed)` for layout, not for unaligned access: see the module documentation.
        #[repr(Rust, packed)]
        #[derive(Clone, Copy, Default)]
        pub struct $ty {
            cache: DigitCache,
            value: $inner,
        }

        impl $ty {
            /// A payload with no cached digits.
            pub fn new(value: $inner) -> $ty {
                $ty { cache: DigitCache::EMPTY, value }
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

numeric_payload!(IntegerPayload, i64, "A signed integer and the digits of its rendering.");
numeric_payload!(UnsignedPayload, u64, "An integer in `[2^63, 2^64)` and the digits of its rendering.");
numeric_payload!(FloatPayload, f64, "A float and the digits of its `%.15g` rendering.");

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
