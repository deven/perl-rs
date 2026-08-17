//! `Value` and `Value` — the authoritative-payload value model (§2.2.1–§2.2.2), with `Tainted` (§2.6.1/§2.6.3),
//! `ArraySlot` hole semantics (§2.2.1), and the numeric coercion primitives.
//!
//! **The payload principle (§2.2.2)**: a scalar has exactly one authoritative payload; everything else is derived, and
//! derived state can never be consulted for anything the payload answers.  Truthiness, stringification, and
//! numification are each one `match` on the payload, written once.  The stale-cache bug class of the flag-matrix model
//! is unrepresentable here.
//!
//! This module carries the §21.1 step-3 subset plus the landed reference and aliasing variants; `CodeRef`, `RegexRef`,
//! and `Typed` land with their own steps (§21.1 steps 4–6), which introduce the remaining referent types; the enum is
//! laid out so those additions preserve the 16-byte envelope (§2.3.6).  The module name is temporary in the same sense
//! as `string.rs`: the final names arrive when the superseded flag-matrix modules are deleted.
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
use crate::scalar::{ConstScalar, FALSE_SCALAR, Referent, Scalar, TRUE_SCALAR};
use crate::string::{DECODE_MAX, PString};

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
    ScalarRef(HeapArc<RwLock<Scalar>>),

    /// Tainted (§2.6): a reference to a mutable scalar (§2.2.1, flattened per mutability).  The taint dimension is a
    /// discriminant twin rather than a field, because a taint byte beside an eight-byte datum cannot fit the envelope's
    /// niche-supplied tag (measured).
    ScalarRefTainted(HeapArc<RwLock<Scalar>>),

    /// Clean: a reference to a frozen scalar (§2.3.1 `Const`).
    ConstScalarRef(HeapArc<ConstScalar>),

    /// Tainted (§2.6): a reference to a frozen scalar (§2.3.1 `Const`).  The taint dimension is a discriminant twin
    /// rather than a field, because a taint byte beside an eight-byte datum cannot fit the envelope's niche-supplied
    /// tag (measured).
    ConstScalarRefTainted(HeapArc<ConstScalar>),

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
    String(PString),

    /// Both faces real (§2.3.4): a dualvar, `$!`, or a string that has numified once.  Carrying a numeric face is what
    /// suppresses the repeat warning, so no tag bit records it.
    Dual(HeapArc<DualPayload>),
    DualTainted(HeapArc<DualPayload>),

    /// The immortal booleans, always clean.
    True,
    False,

    /// An aliasing slot naming a promoted mutable scalar; taint belongs to the referent, not the alias.
    AliasMut(HeapArc<RwLock<Scalar>>),

    /// An aliasing slot naming a frozen scalar.
    AliasConst(HeapArc<ConstScalar>),
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

    /// The canonical value for a `u64`, clean or tainted as `taint` says (ruled): any value is accepted, and values
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

    /// A `ScalarRef`, clean or tainted as `taint` says.
    pub fn scalar_ref(value: HeapArc<RwLock<Scalar>>, taint: Tainted) -> Value {
        if taint.is_tainted() { Value::ScalarRefTainted(value) } else { Value::ScalarRef(value) }
    }

    /// A `ConstScalarRef`, clean or tainted as `taint` says.
    pub fn const_scalar_ref(value: HeapArc<ConstScalar>, taint: Tainted) -> Value {
        if taint.is_tainted() { Value::ConstScalarRefTainted(value) } else { Value::ConstScalarRef(value) }
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

/// A fielded variant cannot be a derived default (§2.6.1): the manual impl names the clean undef.
impl Default for Value {
    fn default() -> Value {
        Value::undef(Tainted::CLEAN)
    }
}

// ── Layout law (§2.3.6) ───────────────────────────────────────────
const _: () = assert!(size_of::<Tainted>() == 1);
const _: () = assert!(size_of::<Value>() == 16);
const _: () = assert!(size_of::<Option<Value>>() == 16);

// ── Coercions: one match each, written once (§2.2.2) ──────────────
impl Numeric {
    /// The i64-visible integer view, matching the payload coercion arm for arm: an unsigned value is the same
    /// sixty-four bits read signed, and a float takes the pinned out-of-range path (§2.2.2).
    pub fn to_int(self) -> i64 {
        match self {
            Numeric::Integer(n) => n,
            Numeric::Unsigned(u) => u as i64,
            Numeric::Float(f) => float_to_int_i64_visible(f),
        }
    }

    /// The float view, matching the payload coercion.
    pub fn to_float(self) -> f64 {
        match self {
            Numeric::Integer(n) => n as f64,
            Numeric::Unsigned(u) => u as f64,
            Numeric::Float(f) => f,
        }
    }
}

/// A value whose string and numeric faces are both real and may disagree: `Scalar::Util::dualvar`, `$!`, and — the
/// common case — a string that has been numified once and cached the result (§2.3.4).
///
/// Immutable once built, so sharing through [`HeapArc`] is free and unobservable, and copying is a refcount bump rather
/// than the deep copy a `Box` would force on every assignment.  It holds no [`Value`], so it can neither chain nor
/// cycle: the §2.4.9 teardown worklist treats it as a leaf.
#[derive(Debug)]
pub struct DualPayload {
    /// What stringification yields, and what decides truth — perl tests the string face, so `dualvar(5, "0")` is false
    /// and `dualvar(0, "00")` is true (container-verified).
    pub string: PString,

    /// What numification yields, without re-parsing and therefore without warning again.
    pub numeric: Numeric,
}

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

impl Value {
    /// Perl truthiness, one match on the payload.  Container-verified: NaN is true, `-0.0` is false, `""` and `"0"` are
    /// the only false strings.
    pub fn to_bool(&self) -> bool {
        match self {
            Value::Undef | Value::UndefTainted => false,
            Value::Integer(n) | Value::IntegerTainted(n) => n.value() != 0,
            Value::Unsigned(u) | Value::UnsignedTainted(u) => u.value() != 0,
            Value::Float(f) | Value::FloatTainted(f) => f.value() != 0.0, // NaN != 0.0 is true; -0.0 == 0.0 — both perl-correct
            Value::String(s) => s.to_bool(),

            // Truth reads the string face: `dualvar(5, "0")` is false and `dualvar(0, "00")` is true.
            Value::Dual(d) | Value::DualTainted(d) => d.string.to_bool(),
            Value::True => true,
            Value::False => false,
            Value::ScalarRef(..)
            | Value::ScalarRefTainted(..)
            | Value::ConstScalarRef(..)
            | Value::ConstScalarRefTainted(..)
            | Value::ArrayRef(..)
            | Value::ArrayRefTainted(..)
            | Value::HashRef(..)
            | Value::HashRefTainted(..) => true, // References are always true (verified).
            Value::AliasMut(c) => c.read().to_bool(),
            Value::AliasConst(c) => c.to_bool(),
        }
    }

    /// The u64-visible integer coercion: the same 64 bits `to_int` yields, read unsigned — which is what perl's `%u`
    /// renders (container-verified across the range, including the wrapping and clamping cases).  Exact arithmetic on
    /// `Unsigned` values needs this reading; nothing else about it differs.
    pub fn to_unsigned(&self) -> u64 {
        self.to_int() as u64
    }

    /// The i64-visible integer coercion, one match on the payload (contracts in the module header).
    pub fn to_int(&self) -> i64 {
        match self {
            Value::Undef | Value::UndefTainted => 0,
            Value::Integer(n) | Value::IntegerTainted(n) => n.value(),
            Value::Unsigned(u) | Value::UnsignedTainted(u) => u.value() as i64, // The same 64 bits read signed — perl's IV view of a UV.
            Value::Float(f) | Value::FloatTainted(f) => float_to_int_i64_visible(f.value()),
            Value::String(s) => s.to_int(),
            Value::Dual(d) | Value::DualTainted(d) => d.numeric.to_int(),
            Value::True => 1,
            Value::False => 0,
            Value::ScalarRef(c) | Value::ScalarRefTainted(c) => HeapArc::as_ptr(c) as usize as i64, // the address (verified)
            Value::ConstScalarRef(c) | Value::ConstScalarRefTainted(c) => HeapArc::as_ptr(c) as usize as i64,
            Value::ArrayRef(r) | Value::ArrayRefTainted(r) => r.addr() as i64,
            Value::HashRef(r) | Value::HashRefTainted(r) => r.addr() as i64,
            Value::AliasMut(c) => c.read().to_int(),
            Value::AliasConst(c) => c.to_int(),
        }
    }

    /// The float coercion, one match on the payload.
    pub fn to_float(&self) -> f64 {
        match self {
            Value::Undef | Value::UndefTainted => 0.0,
            Value::Integer(n) | Value::IntegerTainted(n) => n.value() as f64,
            Value::Unsigned(u) | Value::UnsignedTainted(u) => u.value() as f64,
            Value::Float(f) | Value::FloatTainted(f) => f.value(),
            Value::String(s) => s.to_float(),
            Value::Dual(d) | Value::DualTainted(d) => d.numeric.to_float(),
            Value::True => 1.0,
            Value::False => 0.0,
            Value::ScalarRef(c) | Value::ScalarRefTainted(c) => HeapArc::as_ptr(c) as usize as f64,
            Value::ConstScalarRef(c) | Value::ConstScalarRefTainted(c) => HeapArc::as_ptr(c) as usize as f64,
            Value::ArrayRef(r) | Value::ArrayRefTainted(r) => r.addr() as f64,
            Value::HashRef(r) | Value::HashRefTainted(r) => r.addr() as f64,
            Value::AliasMut(c) => c.read().to_float(),
            Value::AliasConst(c) => c.to_float(),
        }
    }

    /// Numification with perl's int-vs-float classification: integer payloads and exactly-integral string tokens in i64
    /// range numify as integers; everything else as floats.
    pub fn numify(&self) -> Numeric {
        match self {
            Value::Undef | Value::UndefTainted => Numeric::Integer(0),
            Value::Integer(n) | Value::IntegerTainted(n) => Numeric::Integer(n.value()),
            Value::Unsigned(u) | Value::UnsignedTainted(u) => Numeric::Unsigned(u.value()),
            Value::Float(f) | Value::FloatTainted(f) => Numeric::Float(f.value()),
            Value::String(s) => s.numify(),
            Value::Dual(d) | Value::DualTainted(d) => d.numeric,
            Value::True => Numeric::Integer(1),
            Value::False => Numeric::Integer(0),
            Value::ScalarRef(c) | Value::ScalarRefTainted(c) => Numeric::Integer(HeapArc::as_ptr(c) as usize as i64),
            Value::ConstScalarRef(c) | Value::ConstScalarRefTainted(c) => Numeric::Integer(HeapArc::as_ptr(c) as usize as i64),
            Value::ArrayRef(r) | Value::ArrayRefTainted(r) => Numeric::Integer(r.addr() as i64),
            Value::HashRef(r) | Value::HashRefTainted(r) => Numeric::Integer(r.addr() as i64),
            Value::AliasMut(c) => c.read().payload().numify(),
            Value::AliasConst(c) => c.payload().numify(),
        }
    }

    /// Stringification, one match on the payload, producing a `PString` with the operand's taint propagated (string
    /// payloads carry theirs in the tag already; `True` is `"1"`, `False` is `""`, both clean — the immortal-boolean
    /// rule).  Numeric renderings are at most 24 ASCII bytes, hence inline; the `Result` is the honest allocation
    /// contract, not an expected path.
    pub fn stringify(&self) -> Result<PString, AllocError> {
        // Each arm renders into the `PString` itself: a scratch buffer would only be copied from and dropped, and the
        // value can usually hold the result without allocating at all.
        let (out, taint): (PString, Tainted) = match self {
            Value::Undef | Value::UndefTainted => (PString::empty(), self.taint()),
            Value::Integer(n) | Value::IntegerTainted(n) => {
                // Through the payload, so cached digits are used when present.
                let mut rendered = PString::empty();
                n.render(&mut rendered)?;
                (rendered, self.taint())
            }
            Value::Unsigned(u) | Value::UnsignedTainted(u) => {
                // Exact digits: at most twenty characters, so the packed numeric alphabet holds them.
                let mut rendered = PString::empty();
                u.render(&mut rendered)?;
                (rendered, self.taint())
            }
            Value::Float(f) | Value::FloatTainted(f) => {
                let mut rendered = PString::empty();
                f.render(&mut rendered)?;
                (rendered, self.taint())
            }
            Value::String(s) => return Ok(s.clone()),
            Value::Dual(d) | Value::DualTainted(d) => return Ok(d.string.clone()),
            Value::True => (PString::from_bytes(b"1")?, Tainted::CLEAN),
            Value::False => (PString::empty(), Tainted::CLEAN),

            // Container-verified form: SCALAR(0x...) with lowercase hex.
            Value::ScalarRef(c) | Value::ScalarRefTainted(c) => (ref_repr("SCALAR", HeapArc::as_ptr(c) as usize)?, self.taint()),
            Value::ConstScalarRef(c) | Value::ConstScalarRefTainted(c) => (ref_repr("SCALAR", HeapArc::as_ptr(c) as usize)?, self.taint()),
            Value::ArrayRef(r) | Value::ArrayRefTainted(r) => (ref_repr("ARRAY", r.addr())?, self.taint()),
            Value::HashRef(r) | Value::HashRefTainted(r) => (ref_repr("HASH", r.addr())?, self.taint()),
            Value::AliasMut(c) => return c.read().stringify(),
            Value::AliasConst(c) => return Ok(c.stringify().clone()),
        };

        let mut out = out;
        if taint.is_tainted() {
            out.taint();
        }

        Ok(out)
    }

    /// Whether the value is tainted, read through the payload (string payloads carry it in the tag).  Named parallel to
    /// `PString::is_tainted`; `PString::taint` is the tag *setter*.
    pub fn is_tainted(&self) -> bool {
        // Exhaustive rather than a catch-all: the aliasing slots answer through their referent, and a wildcard would
        // silently claim any future variant is clean.
        match self {
            Value::UndefTainted
            | Value::IntegerTainted(_)
            | Value::UnsignedTainted(_)
            | Value::FloatTainted(_)
            | Value::ScalarRefTainted(_)
            | Value::ConstScalarRefTainted(_)
            | Value::ArrayRefTainted(_)
            | Value::HashRefTainted(_) => true,

            Value::Undef
            | Value::Integer(_)
            | Value::Unsigned(_)
            | Value::Float(_)
            | Value::ScalarRef(_)
            | Value::ConstScalarRef(_)
            | Value::ArrayRef(_)
            | Value::HashRef(_)
            | Value::True
            | Value::False => false,

            Value::String(s) => s.is_tainted(),
            Value::Dual(_) => false,
            Value::DualTainted(_) => true,
            Value::AliasMut(c) => c.read().is_tainted(),
            Value::AliasConst(c) => c.is_tainted(),
        }
    }

    /// A copy whose numeric rendering is cached, when its digits fit the seven bytes beside the datum.
    ///
    /// Rendering a number is the expensive part of stringifying one, and the digits are the same every time, so a value
    /// that will be printed, interpolated, or used as a hash key more than once should carry them.  Non-numeric values
    /// are returned unchanged: they have no digits.
    ///
    /// Who calls this is not yet settled (§2.2.9): filling through a shared reference needs atomic cache bytes, while
    /// filling only where a caller holds the value mutably — as here — misses values read through shared containers.
    /// This is the mutable path; the shared one awaits that ruling.
    pub fn with_cached_digits(self) -> Value {
        match self {
            Value::Integer(n) => Value::Integer(n.filled()),
            Value::IntegerTainted(n) => Value::IntegerTainted(n.filled()),
            Value::Unsigned(u) => Value::Unsigned(u.filled()),
            Value::UnsignedTainted(u) => Value::UnsignedTainted(u.filled()),
            Value::Float(f) => Value::Float(f.filled()),
            Value::FloatTainted(f) => Value::FloatTainted(f.filled()),

            // Nothing else renders from digits.  A later numeric kind would want an arm here; missing one costs the
            // optimization, never correctness.
            other => other,
        }
    }

    /// Whether this value's rendering is already cached.
    pub fn has_cached_digits(&self) -> bool {
        match self {
            Value::Integer(n) | Value::IntegerTainted(n) => n.is_cached(),
            Value::Unsigned(u) | Value::UnsignedTainted(u) => u.is_cached(),
            Value::Float(f) | Value::FloatTainted(f) => f.is_cached(),

            // A dual's rendering is its string face, present by construction — no digits ever need formatting, which is
            // exactly what this predicate promises to its caller.
            Value::Dual(_) | Value::DualTainted(_) => true,

            // Everything else renders from no digit cache; a later numeric kind falling here costs the optimization,
            // never correctness (as with `filled` above).
            _ => false,
        }
    }

    /// The taint dimension as a value, for handing to a constructor.
    pub fn taint(&self) -> Tainted {
        if self.is_tainted() { Tainted::TAINTED } else { Tainted::CLEAN }
    }
}

impl Value {
    /// `builtin::is_bool`, answered from the variant (§2.3.3).
    pub fn is_bool(&self) -> bool {
        matches!(self, Value::True | Value::False)
    }

    /// Promote a *temporary* to a shared scalar identity.  The booleans return clones of the immortal singletons
    /// (§2.3.3: `\(1==1)` twice yields the same address — but a boolean held in a *variable* promotes to its own cell
    /// via [`Value::take_ref`]; container-verified distinct).  Other temporaries answer `None`: non-slot temporaries
    /// reach references through the ops layer's temp materialization.
    pub fn upgrade_to_scalar(&self) -> Option<Referent> {
        match self {
            Value::True => Some(TRUE_SCALAR.clone()),
            Value::False => Some(FALSE_SCALAR.clone()),
            _ => None,
        }
    }

    /// `\$x` — the taking-a-reference upgrade trigger (§2.2.8): promote the slot in place through the `AliasMut`
    /// variant (a stable identity the slot now aliases) and return the reference value.  Idempotent on identity: taking
    /// twice yields `ptr_eq` references.  The reference value itself is clean — taint belongs to the referent.
    pub fn take_ref(slot: &mut Value) -> Value {
        match slot {
            Value::AliasMut(c) => return Value::scalar_ref(c.clone(), Tainted::CLEAN),
            Value::AliasConst(c) => return Value::const_scalar_ref(c.clone(), Tainted::CLEAN),
            _ => {}
        }

        let payload = match mem::take(slot) {
            Value::Undef => Value::Undef,
            Value::UndefTainted => Value::UndefTainted,
            Value::Integer(n) => Value::Integer(n),
            Value::IntegerTainted(n) => Value::IntegerTainted(n),
            Value::Unsigned(u) => Value::Unsigned(u),
            Value::UnsignedTainted(u) => Value::UnsignedTainted(u),
            Value::Float(f) => Value::Float(f),
            Value::FloatTainted(f) => Value::FloatTainted(f),
            Value::ScalarRef(c) => Value::ScalarRef(c),
            Value::ScalarRefTainted(c) => Value::ScalarRefTainted(c),
            Value::ConstScalarRef(c) => Value::ConstScalarRef(c),
            Value::ConstScalarRefTainted(c) => Value::ConstScalarRefTainted(c),
            Value::ArrayRef(r) => Value::ArrayRef(r),
            Value::ArrayRefTainted(r) => Value::ArrayRefTainted(r),
            Value::HashRef(r) => Value::HashRef(r),
            Value::HashRefTainted(r) => Value::HashRefTainted(r),
            Value::String(s) => Value::String(s),
            Value::Dual(d) => Value::Dual(d),
            Value::DualTainted(d) => Value::DualTainted(d),
            Value::True => Value::True,
            Value::False => Value::False,
            Value::AliasMut(c) => {
                // Unreachable (handled above); restore and share rather than panic.
                *slot = Value::AliasMut(c.clone());
                return Value::scalar_ref(c, Tainted::CLEAN);
            }
            Value::AliasConst(c) => {
                *slot = Value::AliasConst(c.clone());
                return Value::const_scalar_ref(c, Tainted::CLEAN);
            }
        };

        let cell = HeapArc::new(RwLock::new(Scalar::Plain(payload)));
        *slot = Value::AliasMut(cell.clone());

        Value::scalar_ref(cell, Tainted::CLEAN)
    }

    /// Whether this value holds a strong graph edge (§2.4.9): the reference and aliasing variants.  Non-edge values
    /// cannot recurse when dropped and skip the release worklist.
    pub(crate) fn carries_strong_edge(&self) -> bool {
        // Exhaustive on purpose — no wildcard, no bare list.  A wildcard here is how the tainted twins went missing
        // from the teardown classification: adding a variant must break this match, never silently classify as leaf.
        match self {
            Value::ScalarRef(..)
            | Value::ScalarRefTainted(..)
            | Value::ConstScalarRef(..)
            | Value::ConstScalarRefTainted(..)
            | Value::ArrayRef(..)
            | Value::ArrayRefTainted(..)
            | Value::HashRef(..)
            | Value::HashRefTainted(..)
            | Value::AliasMut(_)
            | Value::AliasConst(_) => true,
            Value::Undef
            | Value::UndefTainted
            | Value::Integer(..)
            | Value::IntegerTainted(..)
            | Value::Unsigned(..)
            | Value::UnsignedTainted(..)
            | Value::Float(..)
            | Value::FloatTainted(..)
            | Value::String(_)
            | Value::Dual(_)
            | Value::DualTainted(_)
            | Value::True
            | Value::False => false,
        }
    }

    /// `@$r` — array dereference: the shared identity behind an array-reference value (through the aliasing variant if
    /// the slot is promoted).  "Not an ARRAY reference" is ops-layer.
    pub fn deref_array(&self) -> Option<ArrayRef> {
        fn from_payload(p: &Value) -> Option<ArrayRef> {
            match p {
                Value::ArrayRef(r) | Value::ArrayRefTainted(r) => Some(r.clone()),
                _ => None,
            }
        }

        match self {
            Value::ArrayRef(r) | Value::ArrayRefTainted(r) => Some(r.clone()),
            Value::AliasMut(cell) => from_payload(cell.read().payload()),
            Value::AliasConst(cs) => from_payload(cs.payload()),
            _ => None,
        }
    }

    /// `%$r` — hash dereference.
    pub fn deref_hash(&self) -> Option<HashRef> {
        fn from_payload(p: &Value) -> Option<HashRef> {
            match p {
                Value::HashRef(r) | Value::HashRefTainted(r) => Some(r.clone()),
                _ => None,
            }
        }

        match self {
            Value::HashRef(r) | Value::HashRefTainted(r) => Some(r.clone()),
            Value::AliasMut(cell) => from_payload(cell.read().payload()),
            Value::AliasConst(cs) => from_payload(cs.payload()),
            _ => None,
        }
    }

    /// `$$r` — scalar dereference: the identity behind a reference value (through the aliasing variant if the slot is
    /// promoted).  `None` for non-references; the "Not a SCALAR reference" error is ops-layer.
    pub fn deref_scalar(&self) -> Option<Referent> {
        fn from_payload(p: &Value) -> Option<Referent> {
            match p {
                Value::ScalarRef(c) | Value::ScalarRefTainted(c) => Some(Referent::Mut(c.clone())),
                Value::ConstScalarRef(c) | Value::ConstScalarRefTainted(c) => Some(Referent::Const(c.clone())),
                _ => None,
            }
        }

        match self {
            Value::ScalarRef(c) | Value::ScalarRefTainted(c) => Some(Referent::Mut(c.clone())),
            Value::ConstScalarRef(c) | Value::ConstScalarRefTainted(c) => Some(Referent::Const(c.clone())),
            Value::AliasMut(cell) => from_payload(cell.read().payload()),
            Value::AliasConst(cs) => from_payload(cs.payload()),
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
pub(crate) fn present_float(digits: &[u8], exp: i32, negative: bool, out: &mut PString) -> Result<(), AllocError> {
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
pub(crate) fn format_float_into(n: f64, out: &mut PString) -> Result<(), AllocError> {
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
fn push_zeros(out: &mut PString, count: usize) -> Result<(), AllocError> {
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
/// `PString` render into the stack buffer instead and never allocate.
pub fn format_float(n: f64) -> String {
    let mut out = PString::empty();
    match format_float_into(n, &mut out) {
        // Every rendering fits without allocating (§2.2.3), so the error arm is unreachable in practice.
        Ok(()) => String::from_utf8_lossy(out.as_bytes(&mut [0u8; DECODE_MAX])).into_owned(),
        Err(_) => String::new(),
    }
}

/// Perl's integer stringification, rendered without allocating.
pub(crate) fn format_int_into(n: i64, out: &mut PString) -> Result<(), AllocError> {
    out.push_fmt(format_args!("{n}"))
}

/// A reference's stringification: `PREFIX(0xADDR)` with lowercase hex, perl's container-verified form.  Rendered
/// through the stack buffer like the numeric forms; the result exceeds the inline capacity, so this one does allocate.
fn ref_repr(prefix: &str, addr: usize) -> Result<PString, AllocError> {
    let mut out = ScientificBuf::new();
    let _ = write!(out, "{prefix}(0x{addr:x})");
    PString::from_bytes(out.as_bytes())
}

/// All eight bytes are ASCII digits.  Masking to nibbles first keeps the comparison free of cross-byte carries: the
/// high nibble must be `3` and the low nibble at most `9`, which adding six turns into a bit-four test.
#[inline]
fn word_all_digits(word: u64) -> bool {
    const LOW: u64 = 0x0F0F_0F0F_0F0F_0F0F;
    const HIGH: u64 = 0xF0F0_F0F0_F0F0_F0F0;
    (word & HIGH) == 0x3030_3030_3030_3030 && ((word & LOW) + 0x0606_0606_0606_0606) & HIGH == 0
}

/// Vectorized leading-digit-run scan: subtracting `'0'` wraps every non-digit above nine, so a saturating subtract
/// leaves a nonzero lane exactly where the run ends.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn digit_run_avx2(bytes: &[u8]) -> usize {
    use std::arch::x86_64::*;
    let mut i = 0;

    unsafe {
        let zero = _mm256_set1_epi8(b'0' as i8);
        let nine = _mm256_set1_epi8(9);
        let null = _mm256_setzero_si256();
        while i + 32 <= bytes.len() {
            let block = _mm256_loadu_si256(bytes.as_ptr().add(i) as *const __m256i);
            let over = _mm256_subs_epu8(_mm256_sub_epi8(block, zero), nine);

            // `movemask` locates the boundary exactly where `testz` only detects one, so a mismatching block is
            // answered here instead of being handed whole to the word scan and read a second time.
            let digits = _mm256_movemask_epi8(_mm256_cmpeq_epi8(over, null)) as u32;
            if digits != u32::MAX {
                return i + (!digits).trailing_zeros() as usize;
            }

            i += 32;
        }
    }

    i + digit_run_words(&bytes[i..])
}

/// AVX-512VL at 256-bit width.  The mask register locates the first non-digit exactly, so a partial block needs no
/// scalar tail, and the narrower vector avoids the downclocking that 512-bit work provokes on some parts.  This is also
/// the AVX10 baseline shape — 256-bit vectors with mask registers, 512-bit optional — so it is the form most likely to
/// keep running unchanged on later hardware.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512bw,avx512vl")]
unsafe fn digit_run_avx512vl(bytes: &[u8]) -> usize {
    use std::arch::x86_64::*;
    let mut i = 0;

    unsafe {
        let zero = _mm256_set1_epi8(b'0' as i8);
        let nine = _mm256_set1_epi8(9);
        while i + 32 <= bytes.len() {
            let block = _mm256_loadu_si256(bytes.as_ptr().add(i) as *const __m256i);
            let digits = _mm256_cmple_epu8_mask(_mm256_sub_epi8(block, zero), nine);
            if digits != u32::MAX {
                return i + (!digits).trailing_zeros() as usize;
            }
            i += 32;
        }
    }

    i + digit_run_words(&bytes[i..])
}

/// NEON, which is baseline on aarch64 and so needs no feature check.  The comparison is the same as the x86 paths —
/// subtracting `'0'` wraps every non-digit above nine — but NEON has no mask register, so the position comes from
/// shift-and-narrow: reinterpreting the sixteen `0x00`/`0xFF` lanes as eight `u16`s and narrowing them with a four-bit
/// shift leaves four bits per original byte in a general register, whence `trailing_zeros() / 4` is the lane index.
/// Measured on an M1 Pro at 34.1 B/ns against 12.8 for the word loop below.
#[cfg(target_arch = "aarch64")]
unsafe fn digit_run_neon(bytes: &[u8]) -> usize {
    use std::arch::aarch64::*;
    let mut i = 0;

    unsafe {
        let zero = vdupq_n_u8(b'0');
        let nine = vdupq_n_u8(9);
        while i + 16 <= bytes.len() {
            let block = vld1q_u8(bytes.as_ptr().add(i));
            let ends_run = vcgtq_u8(vsubq_u8(block, zero), nine);
            let nibbles = vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(ends_run))));
            if nibbles != 0 {
                return i + nibbles.trailing_zeros() as usize / 4;
            }
            i += 16;
        }
    }

    i + digit_run_words(&bytes[i..])
}

/// The vector block size each architecture's path consumes, and so the length below which dispatching to it loses: a
/// run shorter than one block falls straight through to the scalar tail, having paid for the vector setup and, on x86,
/// the feature checks as well.  Measured on an M1 Pro, a twelve-character number — the commonest input a numeric parse
/// ever sees — runs at 0.7x the word loop through the NEON path, which is what this guard prevents.
#[cfg(target_arch = "x86_64")]
const VECTOR_BLOCK: usize = 32;

#[cfg(target_arch = "aarch64")]
const VECTOR_BLOCK: usize = 16;

/// The portable word scan: the fallback where no vector path exists, and every vector kernel's tail.  A partial block
/// is still up to sixty-three bytes, and finishing those a byte at a time costs two to three times what the word loop
/// costs — measured at 2.65x for a thirty-two byte remainder — so the kernels fall through to here rather than straight
/// to a byte loop.  Below eight bytes there is no whole word left and the byte loop is the whole job.
fn digit_run_words(bytes: &[u8]) -> usize {
    let mut i = 0;
    while i + 8 <= bytes.len() {
        let chunk = &bytes[i..i + 8];
        let word = u64::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7]]);
        if !word_all_digits(word) {
            break;
        }
        i += 8;
    }

    i + bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count()
}

/// The length of the leading run of ASCII digits.  This is the only part of numification that is O(the string) — past
/// nineteen significant digits the remainder can only shift an exponent — so it is the part worth vectorizing, and the
/// block structure exits at the first non-digit rather than reading to the end.
pub(crate) fn digit_run(bytes: &[u8]) -> usize {
    // A run shorter than one vector block — which is nearly every number a program actually numifies — skips dispatch
    // entirely: the feature checks would cost more than the word loop below answers in.
    #[cfg(target_arch = "aarch64")]
    if bytes.len() >= VECTOR_BLOCK {
        // SAFETY: NEON is baseline on aarch64, so no feature check is needed; the scan reads only within `bytes`.
        return unsafe { digit_run_neon(bytes) };
    }

    #[cfg(target_arch = "x86_64")]
    if bytes.len() >= VECTOR_BLOCK {
        // SAFETY (both arms): guarded by the runtime feature check; the scans only read within `bytes`.
        if is_x86_feature_detected!("avx512bw") && is_x86_feature_detected!("avx512vl") {
            return unsafe { digit_run_avx512vl(bytes) };
        }

        if is_x86_feature_detected!("avx2") {
            return unsafe { digit_run_avx2(bytes) };
        }
    }

    digit_run_words(bytes)
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

/// Significant integer digits beyond which no `f64` is finite: `f64::MAX` is about `1.8e308`, so 310 digits is at least
/// `10^309` and cannot be represented.
const MAX_FINITE_DIGITS: usize = 309;

/// The string→float coercion: perl's partial-parse rules plus the Inf/NaN prefix forms (module header).
pub fn parse_float(bytes: &[u8]) -> f64 {
    parse_float_spanned(bytes).0
}

/// [`parse_float`] with its consumption surfaced: the second field is how many bytes of the sign-stripped rest the
/// numeric token occupied — the fact the would-warn byproduct is made of (§2.3.4), free because the parse walks exactly
/// that far anyway.  For the `inf`/`nan` forms the span is the recognized word (`infinity` counts all eight), so
/// completeness against the trimmed token falls out of one comparison.
fn parse_float_spanned(bytes: &[u8]) -> (f64, usize) {
    let (negative, rest) = split_sign(bytes);

    // Case-insensitive inf/nan *prefixes* after the sign ("infx" is Inf, "in" is not).
    if rest.len() >= 3 {
        let p = [rest[0].to_ascii_lowercase(), rest[1].to_ascii_lowercase(), rest[2].to_ascii_lowercase()];

        if p == *b"inf" {
            let consumed = if rest.len() >= 8 && rest[..8].eq_ignore_ascii_case(b"infinity") { 8 } else { 3 };
            return (if negative { f64::NEG_INFINITY } else { f64::INFINITY }, consumed);
        }

        if p == *b"nan" {
            return (f64::NAN, 3);
        }
    }

    // Decimal scan: digits, optional fraction, exponent committed only when digits follow the marker ("1e" and "1e+"
    // numify as 1 — a dangling exponent marker is not part of the number).  The dot and the exponent extend only a span
    // that has mantissa digits: a bare "." or "e5" is no numeric token at all, and consuming them would change no value
    // (the magnitude parse of either fails to zero) while falsely claiming span for the would-warn completeness
    // measure.
    let integer_digits = digit_run(rest);
    let mut end = integer_digits;

    if end < rest.len() && rest[end] == b'.' {
        let fraction_digits = digit_run(&rest[end + 1..]);
        if integer_digits > 0 || fraction_digits > 0 {
            end += 1 + fraction_digits;
        }
    }

    let mut has_exponent = false;

    if end > 0 && end < rest.len() && (rest[end] == b'e' || rest[end] == b'E') {
        let mut exp_end = end + 1;

        if exp_end < rest.len() && (rest[exp_end] == b'+' || rest[exp_end] == b'-') {
            exp_end += 1;
        }

        let exp_digits_start = exp_end;
        exp_end += digit_run(&rest[exp_end..]);

        if exp_end > exp_digits_start {
            end = exp_end;
            has_exponent = true;
        }
    }

    if end == 0 {
        return (0.0, 0);
    }

    // A 310-digit integer part is at least 10^309, past `f64::MAX`, so with no exponent to pull it back the magnitude
    // is infinite whatever follows the decimal point.  The general parser reaches the same answer by reading every
    // digit; this reaches it from the count, which is what makes a pathological digit run O(1).
    if !has_exponent && integer_digits > MAX_FINITE_DIGITS {
        let leading_zeros = rest[..integer_digits].iter().take_while(|&&b| b == b'0').count();
        if integer_digits - leading_zeros > MAX_FINITE_DIGITS {
            return (if negative { f64::NEG_INFINITY } else { f64::INFINITY }, end);
        }
    }

    // The scanned span is ASCII digits/'.'/'e'/sign by construction.
    let magnitude = str::from_utf8(&rest[..end]).ok().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);

    (if negative { -magnitude } else { magnitude }, end)
}

/// The §2.3.4 would-warn predicate over the container-mapped boundary table, as an independent grammar: a string is
/// silent iff it is exactly `"0 but true"` (case-sensitive, no surrounding whitespace) or, after trimming ASCII
/// whitespace from both ends, the entire remainder is one complete numeric token — `[sign] (digits [. digits?] | .
/// digits) [e/E [sign] digits+]` with at least one mantissa digit, or case-insensitive signed `inf`/`infinity`/`nan`
/// whole.  Independent of what the parse salvages: `"1e"` numifies as 1 yet warns.
///
/// Production answers this question as a byproduct of the numification walk ([`classify_numeric_noting_warning`]); this
/// predicate survives as the test oracle the byproduct is checked against, valuable precisely because it is a second,
/// independent statement of the same law.
#[cfg(test)]
pub(crate) fn string_would_warn(bytes: &[u8]) -> bool {
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

    // Signed case-insensitive inf/infinity/nan, entire.  `eq_ignore_ascii_case` compares lengths first, where
    // lowercasing into a fresh buffer copied the whole string to test three words of at most eight characters.
    if body.eq_ignore_ascii_case(b"inf") || body.eq_ignore_ascii_case(b"infinity") || body.eq_ignore_ascii_case(b"nan") {
        return false;
    }

    // The complete numeric token grammar.
    let mut i = digit_run(body);
    let mut mantissa_digits = i;

    if i < body.len() && body[i] == b'.' {
        i += 1;
        let fraction = digit_run(&body[i..]);
        i += fraction;
        mantissa_digits += fraction;
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
        j += digit_run(&body[j..]);

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
    classify_numeric_noting_warning(bytes).0
}

/// The §2.3.4 fusion: how the string numifies, and whether numifying it warns, from one walk.  The parse surfaces its
/// own consumption, and warn-worthiness is that consumption measured against the whitespace-trimmed token — nothing is
/// scanned that the numification was not already scanning.  `"0 but true"` is the one lexical exception, silent by
/// perl's law.  The independent grammar predicate survives as a test oracle only.
pub(crate) fn classify_numeric_noting_warning(bytes: &[u8]) -> (Numeric, bool) {
    if bytes == b"0 but true" {
        return (Numeric::Integer(0), false);
    }

    // Silence is a fact about the whole trimmed token: leading whitespace the sign split forgives, trailing whitespace
    // perl forgives, and everything between must be one complete numeric token.
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let token = &bytes[..end];

    let (negative, rest) = split_sign(token);

    let digit_end = digit_run(rest);

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

        let complete = digit_end == rest.len();
        let in_range = if negative { value <= i64::MAX as u128 + 1 } else { value <= i64::MAX as u128 };
        if in_range {
            let n = if negative { if value == i64::MAX as u128 + 1 { i64::MIN } else { -(value as i64) } } else { value as i64 };
            return (Numeric::Integer(n), !complete);
        }

        // Beyond i64 but within u64: exact as an unsigned value, which is where perl reaches for its unsigned slot.
        if !negative && value <= u128::from(u64::MAX) {
            return (Numeric::Unsigned(value as u64), !complete);
        }

        // Larger still (or negative past i64::MIN): only a float can hold it, inexactly.
    }

    let (value, consumed) = parse_float_spanned(token);
    (Numeric::Float(value), consumed != rest.len() || rest.is_empty())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/value_tests.rs"]
mod tests;
