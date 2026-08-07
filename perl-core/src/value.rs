//! `ScalarPayload` and `Value` — the authoritative-payload value model (§2.2.1–§2.2.2), with `Tainted` (§2.6.1/§2.6.3),
//! `ArraySlot` hole semantics (§2.2.1), and the numeric coercion primitives.
//!
//! **The payload principle (§2.2.2)**: a scalar has exactly one authoritative payload; everything else is derived, and
//! derived state can never be consulted for anything the payload answers.  Truthiness, stringification, and
//! numification are each one `match` on the payload, written once.  The stale-cache bug class of the flag-matrix model
//! is unrepresentable here.
//!
//! This module carries the §21.1 step-3 subset: the scalar payload variants.  The reference variants
//! (`ScalarRef`/`ArrayRef`/`HashRef`/`CodeRef`/`RegexRef`), the `Scalar` aliasing variant, and `Typed` land with their
//! own steps (§21.1 steps 4–6), which introduce the referent types; the enums are laid out so those additions preserve
//! the 16-byte envelope (§2.3.6).  The module name is temporary in the same sense as `string.rs`: the final names
//! arrive when the superseded flag-matrix modules are deleted.
//!
//! Numeric contracts are container-verified against perl 5.38 and pin the **i64-visible** behavior only — the value
//! this crate exposes as an `i64`, which is what perl's own integer context yields for everything in range.  Unsigned
//! semantics are a deferred design section (§2.2.2).  Verified facts encoded below:
//!
//! - String numification: leading ASCII whitespace skipped; optional sign; decimal digits (radix prefixes are never
//!   interpreted: `"0xff"` is 0-and-stop); a dangling exponent marker is not part of the number (`"1e"` is 1).
//!   Case-insensitive `inf`/`nan` *prefixes* are recognized after the sign (`"infx"` is Inf, `"nanx"` is NaN, `"in"`
//!   is 0).
//! - Integer strings beyond `i64::MAX` are exact as unsigned 64-bit values in perl; the i64-visible value is the
//!   wrapping cast (`"9223372036854775808"` is `i64::MIN`); beyond `u64::MAX` the value reads as `-1` (perl saturates
//!   its cached unsigned integer at `UV_MAX`); negative overflow clamps to `i64::MIN`.
//! - Float→int truncates toward zero; NaN gives 0; values in `[2^63, 2^64)` wrap through the u64 cast (9.3e18 is
//!   -9146744073709551616); at or above `2^64` (including `+Inf`) the value reads as `-1`; below `-2^63` (including
//!   `-Inf`) it clamps to `i64::MIN`.  (`printf %d` renders non-finite NVs as `Inf`/`NaN` without consulting the cached
//!   integer — a formatting rule for the ops layer, separate from these coercion values.)
//! - Truthiness: NaN is true; `-0.0` is false; the strings `""` and `"0"` are false, everything else (including
//!   `"0.0"`, `"00"`, `" "`) is true.

use parking_lot::RwLock;
use std::fmt;
use std::fmt::Write as _;
use std::mem;
use std::str;

use crate::containers::{ArrayRef, HashRef};
use crate::cow_buffer::AllocError;
use crate::heap::HeapArc;
use crate::numeric::{FloatPayload, IntegerPayload, UnsignedPayload};
use crate::scalar::{ConstScalar, FALSE_SCALAR, ScalarCell, ScalarRef, TRUE_SCALAR};
use crate::string::{DECODE_MAX, PerlString};

// ── Tainted (§2.6.1, §2.6.3) ──────────────────────────────────────
/// The per-value taint bit: a monotonic bool newtype.  Constructors are explicit (`CLEAN` / `TAINTED` — sources that
/// produce tainted values name it), the only public combinator is OR (`tainted_by` raises, never lowers), there is no
/// `Default`, and the clean-from-tainted constructor is crate-private: the untaint capability is confined to the two
/// documented laundering paths (§2.6.2).  Laundering elsewhere is uncompilable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tainted(bool);

impl Tainted {
    /// The clean state: what every constructor of untainted values names explicitly.
    pub const CLEAN: Tainted = Tainted(false);

    /// The tainted state: named by taint *sources* (readline, `%ENV`, locale-dependent results, ...).
    pub const TAINTED: Tainted = Tainted(true);

    #[inline]
    pub fn is_tainted(self) -> bool {
        self.0
    }

    /// The monotonic combinator: propagation ORs, never lowers.
    #[inline]
    #[must_use]
    pub fn tainted_by(self, other: Tainted) -> Tainted {
        Tainted(self.0 | other.0)
    }

    /// The laundered (clean) state, reachable only in-crate: the §2.6.2 capability for capture materialization and
    /// hash-key canonicalization.
    #[cfg_attr(not(test), expect(dead_code, reason = "consumers are the §21.1 capture and hash-key steps; capability is design-mandated"))]
    pub(crate) fn laundered() -> Tainted {
        Tainted(false)
    }
}

// ── The payload and slot-value enums (§2.2.1–§2.2.2) ──────────────
/// The authoritative datum of one scalar (§2.2.2).  Taint rides envelope padding for the sub-maximal variants and the
/// `PerlString` tag for strings; `True`/`False` alone carry no taint state — perl's comparison results are the
/// never-tainted immortal booleans (§2.6.1).
#[derive(Clone, Debug)]
pub enum ScalarPayload {
    /// Clean: the absence of a value.
    Undef,

    /// Tainted (§2.6): the absence of a value.  The taint dimension is a discriminant twin rather than a field, because
    /// a taint byte beside an eight-byte datum cannot fit the envelope's niche-supplied tag (measured).
    UndefTainted,

    /// Clean: a signed integer.
    Integer(IntegerPayload),

    /// Tainted (§2.6): a signed integer.  The taint dimension is a discriminant twin rather than a field, because a
    /// taint byte beside an eight-byte datum cannot fit the envelope's niche-supplied tag (measured).
    IntegerTainted(IntegerPayload),

    /// Clean: an integer in `[2^63, 2^64)`, which `Integer` cannot hold exactly (§2.2.2).
    Unsigned(UnsignedPayload),

    /// Tainted (§2.6): an integer in `[2^63, 2^64)`, which `Integer` cannot hold exactly (§2.2.2).  The taint dimension
    /// is a discriminant twin rather than a field, because a taint byte beside an eight-byte datum cannot fit the
    /// envelope's niche-supplied tag (measured).
    UnsignedTainted(UnsignedPayload),

    /// Clean: a float.
    Float(FloatPayload),

    /// Tainted (§2.6): a float.  The taint dimension is a discriminant twin rather than a field, because a taint byte
    /// beside an eight-byte datum cannot fit the envelope's niche-supplied tag (measured).
    FloatTainted(FloatPayload),

    /// Clean: a reference to a mutable scalar (§2.2.1, flattened per mutability).
    ScalarRefMut(HeapArc<RwLock<ScalarCell>>),

    /// Tainted (§2.6): a reference to a mutable scalar (§2.2.1, flattened per mutability).  The taint dimension is a
    /// discriminant twin rather than a field, because a taint byte beside an eight-byte datum cannot fit the envelope's
    /// niche-supplied tag (measured).
    ScalarRefMutTainted(HeapArc<RwLock<ScalarCell>>),

    /// Clean: a reference to a frozen scalar (§2.3.1 `Const`).
    ScalarRefConst(HeapArc<ConstScalar>),

    /// Tainted (§2.6): a reference to a frozen scalar (§2.3.1 `Const`).  The taint dimension is a discriminant twin
    /// rather than a field, because a taint byte beside an eight-byte datum cannot fit the envelope's niche-supplied
    /// tag (measured).
    ScalarRefConstTainted(HeapArc<ConstScalar>),

    /// Clean: a reference to an array.
    ArrayRef(ArrayRef),

    /// Tainted (§2.6): a reference to an array.  The taint dimension is a discriminant twin rather than a field,
    /// because a taint byte beside an eight-byte datum cannot fit the envelope's niche-supplied tag (measured).
    ArrayRefTainted(ArrayRef),

    /// Clean: a reference to a hash.
    HashRef(HashRef),

    /// Tainted (§2.6): a reference to a hash.  The taint dimension is a discriminant twin rather than a field, because
    /// a taint byte beside an eight-byte datum cannot fit the envelope's niche-supplied tag (measured).
    HashRefTainted(HashRef),

    /// A string, whose taint rides its own tag (§2.2.3).
    String(PerlString),

    /// The immortal booleans, always clean.
    True,
    False,
}

/// The universal slot value (§2.2.1): the compact scalar payloads, plus (in later §21.1 steps) the reference variants,
/// the promoted-scalar aliasing variant, and `Typed`.
#[derive(Clone, Debug)]
pub enum Value {
    /// Clean: the absence of a value.
    Undef,

    /// Tainted (§2.6): the absence of a value.  The taint dimension is a discriminant twin rather than a field, because
    /// a taint byte beside an eight-byte datum cannot fit the envelope's niche-supplied tag (measured).
    UndefTainted,

    /// Clean: a signed integer.
    Integer(IntegerPayload),

    /// Tainted (§2.6): a signed integer.  The taint dimension is a discriminant twin rather than a field, because a
    /// taint byte beside an eight-byte datum cannot fit the envelope's niche-supplied tag (measured).
    IntegerTainted(IntegerPayload),

    /// Clean: an integer in `[2^63, 2^64)`, which `Integer` cannot hold exactly (§2.2.2).
    Unsigned(UnsignedPayload),

    /// Tainted (§2.6): an integer in `[2^63, 2^64)`, which `Integer` cannot hold exactly (§2.2.2).  The taint dimension
    /// is a discriminant twin rather than a field, because a taint byte beside an eight-byte datum cannot fit the
    /// envelope's niche-supplied tag (measured).
    UnsignedTainted(UnsignedPayload),

    /// Clean: a float.
    Float(FloatPayload),

    /// Tainted (§2.6): a float.  The taint dimension is a discriminant twin rather than a field, because a taint byte
    /// beside an eight-byte datum cannot fit the envelope's niche-supplied tag (measured).
    FloatTainted(FloatPayload),

    /// Clean: a reference to a mutable scalar (§2.2.1, flattened per mutability).
    ScalarRefMut(HeapArc<RwLock<ScalarCell>>),

    /// Tainted (§2.6): a reference to a mutable scalar (§2.2.1, flattened per mutability).  The taint dimension is a
    /// discriminant twin rather than a field, because a taint byte beside an eight-byte datum cannot fit the envelope's
    /// niche-supplied tag (measured).
    ScalarRefMutTainted(HeapArc<RwLock<ScalarCell>>),

    /// Clean: a reference to a frozen scalar (§2.3.1 `Const`).
    ScalarRefConst(HeapArc<ConstScalar>),

    /// Tainted (§2.6): a reference to a frozen scalar (§2.3.1 `Const`).  The taint dimension is a discriminant twin
    /// rather than a field, because a taint byte beside an eight-byte datum cannot fit the envelope's niche-supplied
    /// tag (measured).
    ScalarRefConstTainted(HeapArc<ConstScalar>),

    /// Clean: a reference to an array.
    ArrayRef(ArrayRef),

    /// Tainted (§2.6): a reference to an array.  The taint dimension is a discriminant twin rather than a field,
    /// because a taint byte beside an eight-byte datum cannot fit the envelope's niche-supplied tag (measured).
    ArrayRefTainted(ArrayRef),

    /// Clean: a reference to a hash.
    HashRef(HashRef),

    /// Tainted (§2.6): a reference to a hash.  The taint dimension is a discriminant twin rather than a field, because
    /// a taint byte beside an eight-byte datum cannot fit the envelope's niche-supplied tag (measured).
    HashRefTainted(HashRef),

    /// A string, whose taint rides its own tag (§2.2.3).
    String(PerlString),

    /// The immortal booleans, always clean.
    True,
    False,

    /// An aliasing slot naming a promoted mutable scalar; taint belongs to the referent, not the alias.
    ScalarMut(HeapArc<RwLock<ScalarCell>>),

    /// An aliasing slot naming a frozen scalar.
    ScalarConst(HeapArc<ConstScalar>),
}

/// A fielded variant cannot be a derived default (§2.6.1): the manual impl names the clean undef.
impl ScalarPayload {
    // ── Constructors (§2.6: taint is a discriminant twin) ─────────
    //
    // The taint dimension is carried by the variant rather than a field, so these exist to keep callers writing
    // `(datum, taint)` instead of choosing a variant by hand — the pairing that would otherwise be restated at every
    // construction site.

    /// A `Undef`, clean or tainted as `taint` says.
    pub fn undef(taint: Tainted) -> ScalarPayload {
        if taint.is_tainted() { ScalarPayload::UndefTainted } else { ScalarPayload::Undef }
    }

    /// A `Integer`, clean or tainted as `taint` says.
    pub fn integer(value: i64, taint: Tainted) -> ScalarPayload {
        let p = IntegerPayload::new(value);
        if taint.is_tainted() { ScalarPayload::IntegerTainted(p) } else { ScalarPayload::Integer(p) }
    }

    /// The canonical payload for a `u64`, clean or tainted as `taint` says [DECISION]: any value is accepted, and
    /// values `Integer` can hold exactly route there, so `Unsigned` is only ever `[2^63, 2^64)` — its documented range,
    /// enforced at the door rather than assumed of callers.
    pub fn unsigned(value: u64, taint: Tainted) -> ScalarPayload {
        if let Ok(small) = i64::try_from(value) {
            return ScalarPayload::integer(small, taint);
        }
        let p = UnsignedPayload::new(value);
        if taint.is_tainted() { ScalarPayload::UnsignedTainted(p) } else { ScalarPayload::Unsigned(p) }
    }

    /// A `Float`, clean or tainted as `taint` says.
    pub fn float(value: f64, taint: Tainted) -> ScalarPayload {
        let p = FloatPayload::new(value);
        if taint.is_tainted() { ScalarPayload::FloatTainted(p) } else { ScalarPayload::Float(p) }
    }

    /// A `ScalarRefMut`, clean or tainted as `taint` says.
    pub fn scalar_ref_mut(value: HeapArc<RwLock<ScalarCell>>, taint: Tainted) -> ScalarPayload {
        if taint.is_tainted() { ScalarPayload::ScalarRefMutTainted(value) } else { ScalarPayload::ScalarRefMut(value) }
    }

    /// A `ScalarRefConst`, clean or tainted as `taint` says.
    pub fn scalar_ref_const(value: HeapArc<ConstScalar>, taint: Tainted) -> ScalarPayload {
        if taint.is_tainted() { ScalarPayload::ScalarRefConstTainted(value) } else { ScalarPayload::ScalarRefConst(value) }
    }

    /// A `ArrayRef`, clean or tainted as `taint` says.
    pub fn array_ref(value: ArrayRef, taint: Tainted) -> ScalarPayload {
        if taint.is_tainted() { ScalarPayload::ArrayRefTainted(value) } else { ScalarPayload::ArrayRef(value) }
    }

    /// A `HashRef`, clean or tainted as `taint` says.
    pub fn hash_ref(value: HashRef, taint: Tainted) -> ScalarPayload {
        if taint.is_tainted() { ScalarPayload::HashRefTainted(value) } else { ScalarPayload::HashRef(value) }
    }
}

impl Value {
    // ── Constructors (§2.6: taint is a discriminant twin) ─────────
    //
    // The taint dimension is carried by the variant rather than a field, so these exist to keep callers writing
    // `(datum, taint)` instead of choosing a variant by hand — the pairing that would otherwise be restated at every
    // construction site.

    /// A `Undef`, clean or tainted as `taint` says.
    pub fn undef(taint: Tainted) -> Value {
        if taint.is_tainted() { Value::UndefTainted } else { Value::Undef }
    }

    /// A `Integer`, clean or tainted as `taint` says.
    pub fn integer(value: i64, taint: Tainted) -> Value {
        let p = IntegerPayload::new(value);
        if taint.is_tainted() { Value::IntegerTainted(p) } else { Value::Integer(p) }
    }

    /// The canonical value for a `u64`, clean or tainted as `taint` says [DECISION]: any value is accepted, and values
    /// `Integer` can hold exactly route there, so `Unsigned` is only ever `[2^63, 2^64)` — its documented range,
    /// enforced at the door rather than assumed of callers.
    pub fn unsigned(value: u64, taint: Tainted) -> Value {
        if let Ok(small) = i64::try_from(value) {
            return Value::integer(small, taint);
        }
        let p = UnsignedPayload::new(value);
        if taint.is_tainted() { Value::UnsignedTainted(p) } else { Value::Unsigned(p) }
    }

    /// A `Float`, clean or tainted as `taint` says.
    pub fn float(value: f64, taint: Tainted) -> Value {
        let p = FloatPayload::new(value);
        if taint.is_tainted() { Value::FloatTainted(p) } else { Value::Float(p) }
    }

    /// A `ScalarRefMut`, clean or tainted as `taint` says.
    pub fn scalar_ref_mut(value: HeapArc<RwLock<ScalarCell>>, taint: Tainted) -> Value {
        if taint.is_tainted() { Value::ScalarRefMutTainted(value) } else { Value::ScalarRefMut(value) }
    }

    /// A `ScalarRefConst`, clean or tainted as `taint` says.
    pub fn scalar_ref_const(value: HeapArc<ConstScalar>, taint: Tainted) -> Value {
        if taint.is_tainted() { Value::ScalarRefConstTainted(value) } else { Value::ScalarRefConst(value) }
    }

    /// A `ArrayRef`, clean or tainted as `taint` says.
    pub fn array_ref(value: ArrayRef, taint: Tainted) -> Value {
        if taint.is_tainted() { Value::ArrayRefTainted(value) } else { Value::ArrayRef(value) }
    }

    /// A `HashRef`, clean or tainted as `taint` says.
    pub fn hash_ref(value: HashRef, taint: Tainted) -> Value {
        if taint.is_tainted() { Value::HashRefTainted(value) } else { Value::HashRef(value) }
    }
}

impl Default for Value {
    fn default() -> Value {
        Value::undef(Tainted::CLEAN)
    }
}

// ── Layout law (§2.3.6) ───────────────────────────────────────────
const _: () = assert!(size_of::<Tainted>() == 1);
const _: () = assert!(size_of::<ScalarPayload>() == 16);
const _: () = assert!(size_of::<Value>() == 16);
const _: () = assert!(size_of::<Option<Value>>() == 16);

// ── Coercions: one match each, written once (§2.2.2) ──────────────
/// The result of numification: perl's numeric context yields an integer or a float per the value's nature.  i64-visible
/// only (§2.2.2): integer strings exact as unsigned 64-bit values but beyond `i64::MAX` classify as `Float` here, with
/// `to_int` supplying the pinned wrapped value through the exact-digits path independently.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Numeric {
    Integer(i64),

    /// Values in `[2^63, 2^64)`, which perl holds exactly and an `i64` cannot.  Canonical only in that range: perl uses
    /// its unsigned slot strictly when the signed one will not fit (container-verified — subtracting two unsigned
    /// values down to 5 comes back signed), so a value has one representation, not two.
    Unsigned(u64),
    Float(f64),
}

macro_rules! impl_coercions {
    ($ty:ident $(, $smut:ident, $sconst:ident)?) => {
        impl $ty {
            /// Perl truthiness, one match on the payload.  Container-verified: NaN is true, `-0.0` is false, `""` and
            /// `"0"` are the only false strings.
            pub fn to_bool(&self) -> bool {
                match self {
                    $ty::Undef | $ty::UndefTainted => false,
                    $ty::Integer(n) | $ty::IntegerTainted(n) => n.value() != 0,
                    $ty::Unsigned(u) | $ty::UnsignedTainted(u) => u.value() != 0,
                    $ty::Float(f) | $ty::FloatTainted(f) => f.value() != 0.0, // NaN != 0.0 is true; -0.0 == 0.0 — both perl-correct
                    $ty::String(s) => s.to_bool(),
                    $ty::True => true,
                    $ty::False => false,
                    $ty::ScalarRefMut(..)
                    | $ty::ScalarRefMutTainted(..)
                    | $ty::ScalarRefConst(..)
                    | $ty::ScalarRefConstTainted(..)
                    | $ty::ArrayRef(..)
                    | $ty::ArrayRefTainted(..)
                    | $ty::HashRef(..)
                    | $ty::HashRefTainted(..) => true, // References are always true (verified).
                    $($ty::$smut(c) => c.read().to_bool(),)?
                    $($ty::$sconst(c) => c.to_bool(),)?
                }
            }

            /// The u64-visible integer coercion: the same 64 bits `to_int` yields, read unsigned — which is what perl's
            /// `%u` renders (container-verified across the range, including the wrapping and clamping cases).  Exact
            /// arithmetic on `Unsigned` values needs this reading; nothing else about it differs.
            pub fn to_unsigned(&self) -> u64 {
                self.to_int() as u64
            }

            /// The i64-visible integer coercion, one match on the payload (contracts in the module header).
            pub fn to_int(&self) -> i64 {
                match self {
                    $ty::Undef | $ty::UndefTainted => 0,
                    $ty::Integer(n) | $ty::IntegerTainted(n) => n.value(),
                    $ty::Unsigned(u) | $ty::UnsignedTainted(u) => u.value() as i64, // The same 64 bits read signed — perl's IV view of a UV.
                    $ty::Float(f) | $ty::FloatTainted(f) => float_to_int_i64_visible(f.value()),
                    $ty::String(s) => s.to_int(),
                    $ty::True => 1,
                    $ty::False => 0,
                    $ty::ScalarRefMut(c) | $ty::ScalarRefMutTainted(c) => HeapArc::as_ptr(c) as usize as i64, // the address (verified)
                    $ty::ScalarRefConst(c) | $ty::ScalarRefConstTainted(c) => HeapArc::as_ptr(c) as usize as i64,
                    $ty::ArrayRef(r) | $ty::ArrayRefTainted(r) => r.addr() as i64,
                    $ty::HashRef(r) | $ty::HashRefTainted(r) => r.addr() as i64,
                    $($ty::$smut(c) => c.read().to_int(),)?
                    $($ty::$sconst(c) => c.to_int(),)?
                }
            }

            /// The float coercion, one match on the payload.
            pub fn to_float(&self) -> f64 {
                match self {
                    $ty::Undef | $ty::UndefTainted => 0.0,
                    $ty::Integer(n) | $ty::IntegerTainted(n) => n.value() as f64,
                    $ty::Unsigned(u) | $ty::UnsignedTainted(u) => u.value() as f64,
                    $ty::Float(f) | $ty::FloatTainted(f) => f.value(),
                    $ty::String(s) => s.to_float(),
                    $ty::True => 1.0,
                    $ty::False => 0.0,
                    $ty::ScalarRefMut(c) | $ty::ScalarRefMutTainted(c) => HeapArc::as_ptr(c) as usize as f64,
                    $ty::ScalarRefConst(c) | $ty::ScalarRefConstTainted(c) => HeapArc::as_ptr(c) as usize as f64,
                    $ty::ArrayRef(r) | $ty::ArrayRefTainted(r) => r.addr() as f64,
                    $ty::HashRef(r) | $ty::HashRefTainted(r) => r.addr() as f64,
                    $($ty::$smut(c) => c.read().to_float(),)?
                    $($ty::$sconst(c) => c.to_float(),)?
                }
            }

            /// Numification with perl's int-vs-float classification: integer payloads and exactly-integral string
            /// tokens in i64 range numify as integers; everything else as floats.
            pub fn numify(&self) -> Numeric {
                match self {
                    $ty::Undef | $ty::UndefTainted => Numeric::Integer(0),
                    $ty::Integer(n) | $ty::IntegerTainted(n) => Numeric::Integer(n.value()),
                    $ty::Unsigned(u) | $ty::UnsignedTainted(u) => Numeric::Unsigned(u.value()),
                    $ty::Float(f) | $ty::FloatTainted(f) => Numeric::Float(f.value()),
                    $ty::String(s) => s.numify(),
                    $ty::True => Numeric::Integer(1),
                    $ty::False => Numeric::Integer(0),
                    $ty::ScalarRefMut(c) | $ty::ScalarRefMutTainted(c) => Numeric::Integer(HeapArc::as_ptr(c) as usize as i64),
                    $ty::ScalarRefConst(c) | $ty::ScalarRefConstTainted(c) => Numeric::Integer(HeapArc::as_ptr(c) as usize as i64),
                    $ty::ArrayRef(r) | $ty::ArrayRefTainted(r) => Numeric::Integer(r.addr() as i64),
                    $ty::HashRef(r) | $ty::HashRefTainted(r) => Numeric::Integer(r.addr() as i64),
                    $($ty::$smut(c) => c.read().payload().numify(),)?
                    $($ty::$sconst(c) => c.payload().numify(),)?
                }
            }

            /// Stringification, one match on the payload, producing a `PerlString` with the operand's taint propagated
            /// (string payloads carry theirs in the tag already; `True` is `"1"`, `False` is `""`, both clean — the
            /// immortal-boolean rule).  Numeric renderings are at most 24 ASCII bytes, hence inline; the `Result` is
            /// the honest allocation contract, not an expected path.
            pub fn stringify(&self) -> Result<PerlString, AllocError> {
                // Each arm renders into the `PerlString` itself: a scratch buffer would only be copied from and
                // dropped, and the value can usually hold the result without allocating at all.
                let (out, taint): (PerlString, Tainted) = match self {
                    $ty::Undef | $ty::UndefTainted => (PerlString::empty(), self.taint()),
                    $ty::Integer(n) | $ty::IntegerTainted(n) => {
                        // Through the payload, so cached digits are used when present.
                        let mut rendered = PerlString::empty();
                        n.render(&mut rendered)?;
                        (rendered, self.taint())
                    }
                    $ty::Unsigned(u) | $ty::UnsignedTainted(u) => {
                        // Exact digits: at most twenty characters, so the packed numeric alphabet holds them.
                        let mut rendered = PerlString::empty();
                        u.render(&mut rendered)?;
                        (rendered, self.taint())
                    }
                    $ty::Float(f) | $ty::FloatTainted(f) => {
                        let mut rendered = PerlString::empty();
                        f.render(&mut rendered)?;
                        (rendered, self.taint())
                    }
                    $ty::String(s) => return Ok(s.clone()),
                    $ty::True => (PerlString::from_bytes(b"1")?, Tainted::CLEAN),
                    $ty::False => (PerlString::empty(), Tainted::CLEAN),

                    // Container-verified form: SCALAR(0x...) with lowercase hex.
                    $ty::ScalarRefMut(c) | $ty::ScalarRefMutTainted(c) => (ref_repr("SCALAR", HeapArc::as_ptr(c) as usize)?, self.taint()),
                    $ty::ScalarRefConst(c) | $ty::ScalarRefConstTainted(c) => (ref_repr("SCALAR", HeapArc::as_ptr(c) as usize)?, self.taint()),
                    $ty::ArrayRef(r) | $ty::ArrayRefTainted(r) => (ref_repr("ARRAY", r.addr())?, self.taint()),
                    $ty::HashRef(r) | $ty::HashRefTainted(r) => (ref_repr("HASH", r.addr())?, self.taint()),
                    $($ty::$smut(c) => return c.read().stringify(),)?
                    $($ty::$sconst(c) => return Ok(c.stringify().clone()),)?
                };

                let mut out = out;
                if taint.is_tainted() {
                    out.taint();
                }

                Ok(out)
            }

            /// Whether the value is tainted, read through the payload (string payloads carry it in the tag).  Named
            /// parallel to `PerlString::is_tainted`; `PerlString::taint` is the tag *setter*.
            pub fn is_tainted(&self) -> bool {
                // Exhaustive rather than a catch-all: the aliasing slots answer through their referent, and a wildcard
                // would silently claim any future variant is clean.
                match self {
                    $ty::UndefTainted
                    | $ty::IntegerTainted(_)
                    | $ty::UnsignedTainted(_)
                    | $ty::FloatTainted(_)
                    | $ty::ScalarRefMutTainted(_)
                    | $ty::ScalarRefConstTainted(_)
                    | $ty::ArrayRefTainted(_)
                    | $ty::HashRefTainted(_) => true,

                    $ty::Undef
                    | $ty::Integer(_)
                    | $ty::Unsigned(_)
                    | $ty::Float(_)
                    | $ty::ScalarRefMut(_)
                    | $ty::ScalarRefConst(_)
                    | $ty::ArrayRef(_)
                    | $ty::HashRef(_)
                    | $ty::True
                    | $ty::False => false,

                    $ty::String(s) => s.is_tainted(),
                    $($ty::$smut(c) => c.read().is_tainted(),)?
                    $($ty::$sconst(c) => c.is_tainted(),)?
                }
            }

            /// A copy whose numeric rendering is cached, when its digits fit the seven bytes beside the datum.
            ///
            /// Rendering a number is the expensive part of stringifying one, and the digits are the same every time,
            /// so a value that will be printed, interpolated, or used as a hash key more than once should carry them.
            /// Non-numeric values are returned unchanged: they have no digits.
            ///
            /// Who calls this is not yet settled (§2.2.9): filling through a shared reference needs atomic cache bytes,
            /// while filling only where a caller holds the value mutably — as here — misses values read through shared
            /// containers.  This is the mutable path; the shared one awaits that ruling.
            pub fn with_cached_digits(self) -> $ty {
                match self {
                    $ty::Integer(n) => $ty::Integer(n.filled()),
                    $ty::IntegerTainted(n) => $ty::IntegerTainted(n.filled()),
                    $ty::Unsigned(u) => $ty::Unsigned(u.filled()),
                    $ty::UnsignedTainted(u) => $ty::UnsignedTainted(u.filled()),
                    $ty::Float(f) => $ty::Float(f.filled()),
                    $ty::FloatTainted(f) => $ty::FloatTainted(f.filled()),

                    // Nothing else renders from digits.  A later numeric kind would want an arm here; missing one
                    // costs the optimization, never correctness.
                    other => other,
                }
            }

            /// Whether this value's rendering is already cached.
            pub fn has_cached_digits(&self) -> bool {
                match self {
                    $ty::Integer(n) | $ty::IntegerTainted(n) => n.is_cached(),
                    $ty::Unsigned(u) | $ty::UnsignedTainted(u) => u.is_cached(),
                    $ty::Float(f) | $ty::FloatTainted(f) => f.is_cached(),
                    _ => false,
                }
            }

            /// The taint dimension as a value, for handing to a constructor.
            pub fn taint(&self) -> Tainted {
                if self.is_tainted() { Tainted::TAINTED } else { Tainted::CLEAN }
            }
        }
    };
}

impl_coercions!(ScalarPayload);
impl_coercions!(Value, ScalarMut, ScalarConst);

impl Value {
    /// `builtin::is_bool`, answered from the variant (§2.3.3).
    pub fn is_bool(&self) -> bool {
        matches!(self, Value::True | Value::False)
    }

    /// Promote a *temporary* to a shared scalar identity.  The booleans return clones of the immortal singletons
    /// (§2.3.3: `\(1==1)` twice yields the same address — but a boolean held in a *variable* promotes to its own cell
    /// via [`Value::take_ref`]; container-verified distinct).  Other temporaries answer `None`: non-slot temporaries
    /// reach references through the ops layer's temp materialization.
    pub fn upgrade_to_scalar(&self) -> Option<ScalarRef> {
        match self {
            Value::True => Some(TRUE_SCALAR.clone()),
            Value::False => Some(FALSE_SCALAR.clone()),
            _ => None,
        }
    }

    /// `\$x` — the taking-a-reference upgrade trigger (§2.2.8): promote the slot in place through the `Scalar` variant
    /// (a stable identity the slot now aliases) and return the reference value.  Idempotent on identity: taking twice
    /// yields `ptr_eq` references.  The reference value itself is clean — taint belongs to the referent.
    pub fn take_ref(slot: &mut Value) -> Value {
        match slot {
            Value::ScalarMut(c) => return Value::scalar_ref_mut(c.clone(), Tainted::CLEAN),
            Value::ScalarConst(c) => return Value::scalar_ref_const(c.clone(), Tainted::CLEAN),
            _ => {}
        }

        let payload = match mem::take(slot) {
            Value::Undef => ScalarPayload::Undef,
            Value::UndefTainted => ScalarPayload::UndefTainted,
            Value::Integer(n) => ScalarPayload::Integer(n),
            Value::IntegerTainted(n) => ScalarPayload::IntegerTainted(n),
            Value::Unsigned(u) => ScalarPayload::Unsigned(u),
            Value::UnsignedTainted(u) => ScalarPayload::UnsignedTainted(u),
            Value::Float(f) => ScalarPayload::Float(f),
            Value::FloatTainted(f) => ScalarPayload::FloatTainted(f),
            Value::ScalarRefMut(c) => ScalarPayload::ScalarRefMut(c),
            Value::ScalarRefMutTainted(c) => ScalarPayload::ScalarRefMutTainted(c),
            Value::ScalarRefConst(c) => ScalarPayload::ScalarRefConst(c),
            Value::ScalarRefConstTainted(c) => ScalarPayload::ScalarRefConstTainted(c),
            Value::ArrayRef(r) => ScalarPayload::ArrayRef(r),
            Value::ArrayRefTainted(r) => ScalarPayload::ArrayRefTainted(r),
            Value::HashRef(r) => ScalarPayload::HashRef(r),
            Value::HashRefTainted(r) => ScalarPayload::HashRefTainted(r),
            Value::String(s) => ScalarPayload::String(s),
            Value::True => ScalarPayload::True,
            Value::False => ScalarPayload::False,
            Value::ScalarMut(c) => {
                // Unreachable (handled above); restore and share rather than panic.
                *slot = Value::ScalarMut(c.clone());
                return Value::scalar_ref_mut(c, Tainted::CLEAN);
            }
            Value::ScalarConst(c) => {
                *slot = Value::ScalarConst(c.clone());
                return Value::scalar_ref_const(c, Tainted::CLEAN);
            }
        };

        let cell = HeapArc::new(RwLock::new(ScalarCell::Plain(payload)));
        *slot = Value::ScalarMut(cell.clone());

        Value::scalar_ref_mut(cell, Tainted::CLEAN)
    }

    /// Whether this value holds a strong graph edge (§2.4.9): the reference and aliasing variants.  Non-edge values
    /// cannot recurse when dropped and skip the release worklist.
    pub(crate) fn carries_strong_edge(&self) -> bool {
        // Exhaustive on purpose — no wildcard, no bare list.  A wildcard here is how the tainted twins went missing
        // from the teardown classification: adding a variant must break this match, never silently classify as leaf.
        match self {
            Value::ScalarRefMut(..)
            | Value::ScalarRefMutTainted(..)
            | Value::ScalarRefConst(..)
            | Value::ScalarRefConstTainted(..)
            | Value::ArrayRef(..)
            | Value::ArrayRefTainted(..)
            | Value::HashRef(..)
            | Value::HashRefTainted(..)
            | Value::ScalarMut(_)
            | Value::ScalarConst(_) => true,
            Value::Undef
            | Value::UndefTainted
            | Value::Integer(..)
            | Value::IntegerTainted(..)
            | Value::Unsigned(..)
            | Value::UnsignedTainted(..)
            | Value::Float(..)
            | Value::FloatTainted(..)
            | Value::String(_)
            | Value::True
            | Value::False => false,
        }
    }

    /// Whether this payload holds a strong owning edge into the heap graph — the teardown worklist's classification,
    /// shared by every teardown path so the answer cannot drift between them.  Exhaustive on purpose, like
    /// [`Value::carries_strong_edge`]: adding a variant must break this match.
    pub(crate) fn payload_carries_strong_edge(p: &ScalarPayload) -> bool {
        match p {
            ScalarPayload::ScalarRefMut(..)
            | ScalarPayload::ScalarRefMutTainted(..)
            | ScalarPayload::ScalarRefConst(..)
            | ScalarPayload::ScalarRefConstTainted(..)
            | ScalarPayload::ArrayRef(..)
            | ScalarPayload::ArrayRefTainted(..)
            | ScalarPayload::HashRef(..)
            | ScalarPayload::HashRefTainted(..) => true,
            ScalarPayload::Undef
            | ScalarPayload::UndefTainted
            | ScalarPayload::Integer(..)
            | ScalarPayload::IntegerTainted(..)
            | ScalarPayload::Unsigned(..)
            | ScalarPayload::UnsignedTainted(..)
            | ScalarPayload::Float(..)
            | ScalarPayload::FloatTainted(..)
            | ScalarPayload::String(_)
            | ScalarPayload::True
            | ScalarPayload::False => false,
        }
    }

    /// Rehydrate a payload as a slot value.  Consumers: the §2.4.9 release path (a dying cell's payload enters the
    /// worklist as a value) and, eventually, the ops layer's slot writes.
    pub(crate) fn from_payload(p: ScalarPayload) -> Value {
        match p {
            ScalarPayload::Undef => Value::Undef,
            ScalarPayload::UndefTainted => Value::UndefTainted,
            ScalarPayload::Integer(n) => Value::Integer(n),
            ScalarPayload::IntegerTainted(n) => Value::IntegerTainted(n),
            ScalarPayload::Unsigned(u) => Value::Unsigned(u),
            ScalarPayload::UnsignedTainted(u) => Value::UnsignedTainted(u),
            ScalarPayload::Float(f) => Value::Float(f),
            ScalarPayload::FloatTainted(f) => Value::FloatTainted(f),
            ScalarPayload::ScalarRefMut(c) => Value::ScalarRefMut(c),
            ScalarPayload::ScalarRefMutTainted(c) => Value::ScalarRefMutTainted(c),
            ScalarPayload::ScalarRefConst(c) => Value::ScalarRefConst(c),
            ScalarPayload::ScalarRefConstTainted(c) => Value::ScalarRefConstTainted(c),
            ScalarPayload::ArrayRef(r) => Value::ArrayRef(r),
            ScalarPayload::ArrayRefTainted(r) => Value::ArrayRefTainted(r),
            ScalarPayload::HashRef(r) => Value::HashRef(r),
            ScalarPayload::HashRefTainted(r) => Value::HashRefTainted(r),
            ScalarPayload::String(s) => Value::String(s),
            ScalarPayload::True => Value::True,
            ScalarPayload::False => Value::False,
        }
    }

    /// `$$r` — scalar dereference: the identity behind a reference value (through the aliasing variant if the slot is
    /// promoted).  `None` for non-references; the "Not a SCALAR reference" error is ops-layer.  `@$r` — array
    /// dereference: the shared identity behind an array-reference value (through the aliasing variant if the slot is
    /// promoted).  "Not an ARRAY reference" is ops-layer.
    pub fn deref_array(&self) -> Option<ArrayRef> {
        fn from_payload(p: &ScalarPayload) -> Option<ArrayRef> {
            match p {
                ScalarPayload::ArrayRef(r) | ScalarPayload::ArrayRefTainted(r) => Some(r.clone()),
                _ => None,
            }
        }

        match self {
            Value::ArrayRef(r) | Value::ArrayRefTainted(r) => Some(r.clone()),
            Value::ScalarMut(cell) => from_payload(cell.read().payload()),
            Value::ScalarConst(cs) => from_payload(cs.payload()),
            _ => None,
        }
    }

    /// `%$r` — hash dereference.
    pub fn deref_hash(&self) -> Option<HashRef> {
        fn from_payload(p: &ScalarPayload) -> Option<HashRef> {
            match p {
                ScalarPayload::HashRef(r) | ScalarPayload::HashRefTainted(r) => Some(r.clone()),
                _ => None,
            }
        }

        match self {
            Value::HashRef(r) | Value::HashRefTainted(r) => Some(r.clone()),
            Value::ScalarMut(cell) => from_payload(cell.read().payload()),
            Value::ScalarConst(cs) => from_payload(cs.payload()),
            _ => None,
        }
    }

    pub fn deref_scalar(&self) -> Option<ScalarRef> {
        fn from_payload(p: &ScalarPayload) -> Option<ScalarRef> {
            match p {
                ScalarPayload::ScalarRefMut(c) | ScalarPayload::ScalarRefMutTainted(c) => Some(ScalarRef::Mut(c.clone())),
                ScalarPayload::ScalarRefConst(c) | ScalarPayload::ScalarRefConstTainted(c) => Some(ScalarRef::Const(c.clone())),
                _ => None,
            }
        }

        match self {
            Value::ScalarRefMut(c) | Value::ScalarRefMutTainted(c) => Some(ScalarRef::Mut(c.clone())),
            Value::ScalarRefConst(c) | Value::ScalarRefConstTainted(c) => Some(ScalarRef::Const(c.clone())),
            Value::ScalarMut(cell) => from_payload(cell.read().payload()),
            Value::ScalarConst(cs) => from_payload(cs.payload()),
            _ => None,
        }
    }
}

// ── Array slots (§2.2.1) ──────────────────────────────────────────
/// `None` = nonexistent element (a hole); `Some(Value::Undef)` = an existing element holding undef.
pub type ArraySlot = Option<Value>;

/// `exists $a[$i]`: the slot is present and occupied.
pub fn array_exists(slots: &[ArraySlot], index: usize) -> bool {
    slots.get(index).is_some_and(Option::is_some)
}

/// `delete $a[$i]`, returning the deleted value (undef for holes and out-of-range indices, which are left untouched).
/// Container-verified (§2.2.1): deleting mid-array leaves a hole with the length unchanged; deleting the *last* element
/// truncates through any trailing holes (deleting index 2 of a 3-element array whose index 1 is already a hole yields
/// length 1, not 2).
pub fn array_delete(slots: &mut Vec<ArraySlot>, index: usize) -> Value {
    if index >= slots.len() {
        return Value::default();
    }

    let deleted = slots[index].take().unwrap_or_default();

    if index == slots.len() - 1 {
        while matches!(slots.last(), Some(None)) {
            slots.pop();
        }
    }

    deleted
}

// ── Numeric primitives (container-verified; contracts in the module header) ──
/// Perl's `%g`-at-15-digits float stringification.  Rust has no `%g` formatter, so build it: render at 15 significant
/// digits in exponent form, then choose fixed or exponent presentation by the `%g` rule and strip trailing fraction
/// zeros.  All shapes verified against perl 5.38.2 print output: `0.1+0.2` is `"0.3"`, `1e15` is `"1e+15"`, `1e-5` is
/// `"1e-05"`.  The widest rendering the float formatter's intermediate step produces: `{:.14e}` of an `f64` is at most
/// 22 characters, so 32 bytes is comfortable headroom.
const SCIENTIFIC_MAX: usize = 32;

/// A fixed-capacity buffer for the float formatter's intermediate scientific form, which must be produced before it can
/// be parsed into `%g`'s presentation.  Genuine scratch: the *result* goes straight into the destination string, since
/// numeric rendering is constant traffic and has no business allocating a buffer to copy from.  Writes past the
/// capacity are dropped rather than panicking; the bound above proves that cannot happen, and a debug assertion catches
/// any future format that outgrows it.
struct ScientificBuf {
    buf: [u8; SCIENTIFIC_MAX],
    len: usize,
}

impl ScientificBuf {
    fn new() -> ScientificBuf {
        ScientificBuf { buf: [0; SCIENTIFIC_MAX], len: 0 }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    fn push(&mut self, byte: u8) {
        if self.len < SCIENTIFIC_MAX {
            self.buf[self.len] = byte;
            self.len += 1;
        }
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.push(b);
        }
    }
}

impl fmt::Write for ScientificBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let before = self.len;
        self.push_bytes(s.as_bytes());
        debug_assert_eq!(self.len - before, s.len(), "SCIENTIFIC_MAX is too small for this format");
        Ok(())
    }
}

/// Perl's default float stringification: `sprintf("%.15g")`, the `SvPV` path — a fixed significant-digit count rather
/// than shortest-round-trip, which is why perl prints `0.1 + 0.2` as `0.3`.  Perl's own capitalizations for the
/// specials.  Renders into `out`, allocating nothing.
///
/// Explicit `sprintf`/`printf` with a precision is a different operation with unbounded output (§2.2.3); this covers
/// only the implicit conversion.  The significant digits and decimal exponent of a float's `%.15g` rendering, or `None`
/// when the value renders as a special (`NaN`, `Inf`, `-Inf`) or as plain `0`, which have no digits to extract.
///
/// This is the expensive half — a formatted render followed by a parse — and the half a digit cache exists to skip.
/// Its counterpart [`present_float`] turns the result back into text.
pub(crate) fn float_digits(n: f64) -> Option<([u8; FLOAT_DIGIT_MAX], usize, i32)> {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return None;
    }

    // "{:.14e}" is the normalized d.dddddddddddddd form: 15 significant digits, correctly rounded.
    let mut scientific = ScientificBuf::new();
    let _ = write!(scientific, "{n:.14e}");
    let rendered = scientific.as_bytes();

    let e = rendered.iter().position(|&b| b == b'e')?;
    let (mantissa, exponent) = (&rendered[..e], &rendered[e + 1..]);
    let exp = str::from_utf8(exponent).ok()?.parse::<i32>().ok()?;

    // The significant digits, trailing zeros trimmed — %g drops them.
    let mut digits = [0u8; FLOAT_DIGIT_MAX];
    let mut count = 0;
    for &b in mantissa {
        if b.is_ascii_digit() && count < digits.len() {
            digits[count] = b - b'0';
            count += 1;
        }
    }

    while count > 1 && digits[count - 1] == 0 {
        count -= 1;
    }

    Some((digits, count, exp))
}

/// The widest digit sequence `%.15g` produces, plus room for the rounding position.
pub(crate) const FLOAT_DIGIT_MAX: usize = 16;

/// Render digits and a decimal exponent as `%g` presents them: fixed notation within its range, exponent notation
/// outside it.  The cheap half, and the one a cached rendering reuses.
pub(crate) fn present_float(digits: &[u8], exp: i32, negative: bool, out: &mut PerlString) -> Result<(), AllocError> {
    if negative {
        out.push_str("-")?;
    }

    let count = digits.len();
    let mut buf = [0u8; FLOAT_DIGIT_MAX];

    for (i, &d) in digits.iter().enumerate() {
        buf[i] = b'0' + d;
    }

    let ascii = &buf[..count];

    // %g takes exponent form when the decimal exponent is below -4 or at/above the precision (15).
    if !(-4..15).contains(&exp) {
        out.push_bytes(&ascii[..1])?;

        if count > 1 {
            out.push_str(".")?;
            out.push_bytes(&ascii[1..])?;
        }

        let magnitude = exp.unsigned_abs();
        let sign = if exp < 0 { '-' } else { '+' };

        // Perl pads the exponent to two digits: 1e-05, not 1e-5.
        return out.push_fmt(format_args!("e{sign}{magnitude:02}"));
    }

    if exp >= 0 {
        let int_len = exp as usize + 1;

        if count <= int_len {
            out.push_bytes(ascii)?;
            push_zeros(out, int_len - count)?;
        } else {
            out.push_bytes(&ascii[..int_len])?;
            out.push_str(".")?;
            out.push_bytes(&ascii[int_len..])?;
        }
    } else {
        out.push_str("0.")?;
        push_zeros(out, (-exp - 1) as usize)?;
        out.push_bytes(ascii)?;
    }

    Ok(())
}

/// Perl's default float stringification: `sprintf("%.15g")`, the `SvPV` path — a fixed significant-digit count rather
/// than shortest-round-trip, which is why perl prints `0.1 + 0.2` as `0.3`.  Perl's own capitalizations for the
/// specials.  Renders into `out`, allocating nothing.
///
/// Explicit `sprintf`/`printf` with a precision is a different operation with unbounded output (§2.2.3); this covers
/// only the implicit conversion.
pub(crate) fn format_float_into(n: f64, out: &mut PerlString) -> Result<(), AllocError> {
    if n.is_nan() {
        return out.push_str("NaN");
    }

    if n.is_infinite() {
        return out.push_str(if n < 0.0 { "-Inf" } else { "Inf" });
    }

    if n == 0.0 {
        return out.push_str("0"); // Covers -0.0, which perl also prints as "0".
    }

    match float_digits(n) {
        Some((digits, count, exp)) => present_float(&digits[..count], exp, n.is_sign_negative(), out),
        None => out.push_str("0"), // Unreachable: the specials returned above.
    }
}

/// Append a run of `'0'`, the one repetition `%g`'s presentation needs.
fn push_zeros(out: &mut PerlString, count: usize) -> Result<(), AllocError> {
    const ZEROS: &[u8; 24] = b"000000000000000000000000";

    let mut left = count;
    while left > 0 {
        let take = left.min(ZEROS.len());
        out.push_bytes(&ZEROS[..take])?;
        left -= take;
    }

    Ok(())
}

/// Perl's default float stringification as an owned `String`, for callers that want Rust text.  Paths that build a
/// `PerlString` render into the stack buffer instead and never allocate.
pub fn format_float(n: f64) -> String {
    let mut out = PerlString::empty();
    match format_float_into(n, &mut out) {
        // Every rendering fits without allocating (§2.2.3), so the error arm is unreachable in practice.
        Ok(()) => String::from_utf8_lossy(out.as_bytes(&mut [0u8; DECODE_MAX])).into_owned(),
        Err(_) => String::new(),
    }
}

/// Perl's integer stringification, rendered without allocating.
pub(crate) fn format_int_into(n: i64, out: &mut PerlString) -> Result<(), AllocError> {
    out.push_fmt(format_args!("{n}"))
}

/// A reference's stringification: `PREFIX(0xADDR)` with lowercase hex, perl's container-verified form.  Rendered
/// through the stack buffer like the numeric forms; the result exceeds the inline capacity, so this one does allocate.
fn ref_repr(prefix: &str, addr: usize) -> Result<PerlString, AllocError> {
    let mut out = ScientificBuf::new();
    let _ = write!(out, "{prefix}(0x{addr:x})");
    PerlString::from_bytes(out.as_bytes())
}

/// Leading ASCII whitespace and optional sign; returns (negative, rest).
fn split_sign(bytes: &[u8]) -> (bool, &[u8]) {
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }

    match bytes.get(i) {
        Some(b'-') => (true, &bytes[i + 1..]),
        Some(b'+') => (false, &bytes[i + 1..]),
        _ => (false, &bytes[i..]),
    }
}

/// The i64-visible string→integer coercion (contracts in the module header).
pub fn parse_int_i64_visible(bytes: &[u8]) -> i64 {
    let (negative, rest) = split_sign(bytes);

    // Accumulate the leading decimal digits exactly; beyond u64 range only the overflow class matters.
    let mut value: u128 = 0;
    let mut digits = 0usize;
    for &b in rest {
        if !b.is_ascii_digit() {
            break;
        }

        digits += 1;

        if value <= u128::from(u64::MAX) {
            value = value * 10 + u128::from(b - b'0');
        }
    }

    if digits == 0 {
        return 0;
    }

    if negative {
        if value <= i64::MAX as u128 {
            -(value as i64)
        } else {
            i64::MIN // -(2^63) exactly, and every larger magnitude clamps here (container-verified)
        }
    } else if value <= u128::from(u64::MAX) {
        value as u64 as i64 // Exact within i64, the wrapping cast above it — perl holds these exactly, unsigned.
    } else {
        -1 // Reads as -1: perl saturates its cached unsigned integer at UV_MAX.
    }
}

/// The float→integer coercion (contracts in the module header).
pub fn float_to_int_i64_visible(f: f64) -> i64 {
    const TWO_63: f64 = 9_223_372_036_854_775_808.0;
    const TWO_64: f64 = 18_446_744_073_709_551_616.0;

    if f.is_nan() {
        return 0;
    }

    if f >= TWO_64 {
        return -1; // Reads as -1, +Inf included: perl saturates at UV_MAX.
    }

    if f >= TWO_63 {
        return f as u64 as i64; // the UV range: wrap through the unsigned cast (9.3e18 verified)
    }

    if f <= -TWO_63 {
        return i64::MIN; // includes -Inf
    }

    f as i64 // truncation toward zero
}

/// The string→float coercion: perl's partial-parse rules plus the Inf/NaN prefix forms (module header).
pub fn parse_float(bytes: &[u8]) -> f64 {
    let (negative, rest) = split_sign(bytes);

    // Case-insensitive inf/nan *prefixes* after the sign ("infx" is Inf, "in" is not).
    if rest.len() >= 3 {
        let p = [rest[0].to_ascii_lowercase(), rest[1].to_ascii_lowercase(), rest[2].to_ascii_lowercase()];

        if p == *b"inf" {
            return if negative { f64::NEG_INFINITY } else { f64::INFINITY };
        }

        if p == *b"nan" {
            return f64::NAN;
        }
    }

    // Decimal scan: digits, optional fraction, exponent committed only when digits follow the marker ("1e" and "1e+"
    // numify as 1 — a dangling exponent marker is not part of the number).
    let mut end = 0;
    while end < rest.len() && rest[end].is_ascii_digit() {
        end += 1;
    }

    if end < rest.len() && rest[end] == b'.' {
        end += 1;

        while end < rest.len() && rest[end].is_ascii_digit() {
            end += 1;
        }
    }

    if end < rest.len() && (rest[end] == b'e' || rest[end] == b'E') {
        let mut exp_end = end + 1;

        if exp_end < rest.len() && (rest[exp_end] == b'+' || rest[exp_end] == b'-') {
            exp_end += 1;
        }

        let exp_digits_start = exp_end;

        while exp_end < rest.len() && rest[exp_end].is_ascii_digit() {
            exp_end += 1;
        }

        if exp_end > exp_digits_start {
            end = exp_end;
        }
    }

    if end == 0 {
        return 0.0;
    }

    // The scanned span is ASCII digits/'.'/'e'/sign by construction.
    let magnitude = str::from_utf8(&rest[..end]).ok().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);

    if negative { -magnitude } else { magnitude }
}

/// The §2.3.4 would-warn predicate over the container-mapped boundary table: a string is silent iff it is exactly
/// `"0 but true"` (case-sensitive, no surrounding whitespace) or, after trimming ASCII whitespace from both ends, the
/// entire remainder is one complete numeric token — `[sign] (digits [. digits?] | . digits) [e/E [sign] digits+]` with
/// at least one mantissa digit, or case-insensitive signed `inf`/`infinity`/`nan` whole.  Independent of what the parse
/// salvages: `"1e"` numifies as 1 yet warns.
pub fn string_would_warn(bytes: &[u8]) -> bool {
    if bytes == b"0 but true" {
        return false;
    }

    let mut start = 0;
    while start < bytes.len() && bytes[start].is_ascii_whitespace() {
        start += 1;
    }

    let mut end = bytes.len();
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }

    let token = &bytes[start..end];
    if token.is_empty() {
        return true; // empty and whitespace-only strings warn
    }

    let body = match token[0] {
        b'+' | b'-' => &token[1..],
        _ => token,
    };

    // Signed case-insensitive inf/infinity/nan, entire.
    let lower: Vec<u8> = body.iter().map(u8::to_ascii_lowercase).collect();
    if lower == b"inf" || lower == b"infinity" || lower == b"nan" {
        return false;
    }

    // The complete numeric token grammar.
    let mut i = 0;
    let mut mantissa_digits = 0usize;
    while i < body.len() && body[i].is_ascii_digit() {
        i += 1;
        mantissa_digits += 1;
    }

    if i < body.len() && body[i] == b'.' {
        i += 1;

        while i < body.len() && body[i].is_ascii_digit() {
            i += 1;
            mantissa_digits += 1;
        }
    }

    if mantissa_digits == 0 {
        return true;
    }

    if i < body.len() && (body[i] == b'e' || body[i] == b'E') {
        let mut j = i + 1;
        if j < body.len() && (body[j] == b'+' || body[j] == b'-') {
            j += 1;
        }

        let digits_start = j;
        while j < body.len() && body[j].is_ascii_digit() {
            j += 1;
        }

        if j == digits_start {
            return true; // dangling exponent marker: "1e", "1e+"
        }

        i = j;
    }

    i != body.len()
}

/// String numification classification: an exactly-integral token within i64 range numifies as an integer; everything
/// else (fractions, exponents, overflow, Inf/NaN forms, garbage) as a float.
pub(crate) fn classify_numeric(bytes: &[u8]) -> Numeric {
    let (negative, rest) = split_sign(bytes);

    let mut digit_end = 0;
    while digit_end < rest.len() && rest[digit_end].is_ascii_digit() {
        digit_end += 1;
    }

    // Integral iff there are digits and the token ends there (nothing numeric continues it).
    let integral_token = digit_end > 0 && !matches!(rest.get(digit_end), Some(b'.') | Some(b'e') | Some(b'E'));

    if integral_token {
        let mut value: u128 = 0;
        for &b in &rest[..digit_end] {
            value = value * 10 + u128::from(b - b'0');
            if value > u128::from(u64::MAX) {
                break;
            }
        }

        let in_range = if negative { value <= i64::MAX as u128 + 1 } else { value <= i64::MAX as u128 };
        if in_range {
            let n = if negative { if value == i64::MAX as u128 + 1 { i64::MIN } else { -(value as i64) } } else { value as i64 };
            return Numeric::Integer(n);
        }

        // Beyond i64 but within u64: exact as an unsigned value, which is where perl reaches for its unsigned slot.
        if !negative && value <= u128::from(u64::MAX) {
            return Numeric::Unsigned(value as u64);
        }

        // Larger still (or negative past i64::MIN): only a float can hold it, inexactly.
    }

    Numeric::Float(parse_float(bytes))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/value_tests.rs"]
mod tests;
