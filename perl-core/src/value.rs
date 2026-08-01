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
//! the 24-byte envelope (§2.3.6).  The module name is temporary in the same sense as `string.rs`: the final names
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
use crate::scalar::{ConstScalar, ScalarCell, ScalarRef};
use crate::string::PerlString;

// ── Tainted (§2.6.1, §2.6.3) ──────────────────────────────────────
/// The per-value taint bit: a monotone bool newtype.  Constructors are explicit (`CLEAN` / `TAINTED` — sources that
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

    /// The monotone combinator: propagation ORs, never lowers.
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
    Undef(Tainted),
    Integer(i64, Tainted),
    Float(f64, Tainted),
    String(PerlString),
    True,
    False,

    /// A reference to a mutable scalar (§2.2.1, flattened per mutability — measured: the nested identity enum defeats
    /// niche-folding).  The referent carries its own taint; this is the reference value's.
    ScalarRefMut(HeapArc<RwLock<ScalarCell>>, Tainted),

    /// A reference to a frozen scalar (§2.3.1 `Const`: immortals, `use constant`, folded literals).
    ScalarRefConst(HeapArc<ConstScalar>, Tainted),

    /// A reference to an array (§2.2.1).  The handle is a tagless newtype, so nesting preserves the niche.
    ArrayRef(ArrayRef, Tainted),

    /// A reference to a hash (§2.2.1).
    HashRef(HashRef, Tainted),
}

/// The universal slot value (§2.2.1): the compact scalar payloads, plus (in later §21.1 steps) the reference variants,
/// the promoted-scalar aliasing variant, and `Typed`.
#[derive(Clone, Debug)]
pub enum Value {
    Undef(Tainted),
    Integer(i64, Tainted),
    Float(f64, Tainted),
    String(PerlString),
    True,
    False,
    ScalarRefMut(HeapArc<RwLock<ScalarCell>>, Tainted),
    ScalarRefConst(HeapArc<ConstScalar>, Tainted),
    ArrayRef(ArrayRef, Tainted),
    HashRef(HashRef, Tainted),

    /// A promoted mutable scalar occupying this slot — the slot aliases it (§2.2.1).  Coercions read through the cell:
    /// aliasing transparency.
    ScalarMut(HeapArc<RwLock<ScalarCell>>),

    /// A promoted frozen scalar occupying this slot (e.g. `foreach` aliasing over literal list elements).
    ScalarConst(HeapArc<ConstScalar>),
}

/// A fielded variant cannot be a derived default (§2.6.1): the manual impl names the clean undef.
impl Default for Value {
    fn default() -> Value {
        Value::Undef(Tainted::CLEAN)
    }
}

// ── Layout law (§2.3.6) ───────────────────────────────────────────
const _: () = assert!(size_of::<Tainted>() == 1);
const _: () = assert!(size_of::<ScalarPayload>() == 24);
const _: () = assert!(size_of::<Value>() == 24);
const _: () = assert!(size_of::<Option<Value>>() == 24);

// ── Coercions: one match each, written once (§2.2.2) ──────────────
/// The result of numification: perl's numeric context yields an integer or a float per the value's nature.  i64-visible
/// only (§2.2.2): integer strings exact as unsigned 64-bit values but beyond `i64::MAX` classify as `Float` here, with
/// `to_int` supplying the pinned wrapped value through the exact-digits path independently.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Numeric {
    Integer(i64),
    Float(f64),
}

macro_rules! impl_coercions {
    ($ty:ident $(, $smut:ident, $sconst:ident)?) => {
        impl $ty {
            /// Perl truthiness, one match on the payload.  Container-verified: NaN is true, `-0.0` is false, `""` and
            /// `"0"` are the only false strings.
            pub fn to_bool(&self) -> bool {
                match self {
                    $ty::Undef(_) => false,
                    $ty::Integer(n, _) => *n != 0,
                    $ty::Float(f, _) => *f != 0.0, // NaN != 0.0 is true; -0.0 == 0.0 — both perl-correct
                    $ty::String(s) => s.to_bool(),
                    $ty::True => true,
                    $ty::False => false,
                    $ty::ScalarRefMut(..) | $ty::ScalarRefConst(..) | $ty::ArrayRef(..) | $ty::HashRef(..) => true, // refs are always true (verified)
                    $($ty::$smut(c) => c.read().to_bool(),)?
                    $($ty::$sconst(c) => c.to_bool(),)?
                }
            }

            /// The i64-visible integer coercion, one match on the payload (contracts in the module header).
            pub fn to_int(&self) -> i64 {
                match self {
                    $ty::Undef(_) => 0,
                    $ty::Integer(n, _) => *n,
                    $ty::Float(f, _) => float_to_int_i64_visible(*f),
                    $ty::String(s) => s.to_int(),
                    $ty::True => 1,
                    $ty::False => 0,
                    $ty::ScalarRefMut(c, _) => HeapArc::as_ptr(c) as usize as i64, // the address (verified)
                    $ty::ScalarRefConst(c, _) => HeapArc::as_ptr(c) as usize as i64,
                    $ty::ArrayRef(r, _) => r.addr() as i64,
                    $ty::HashRef(r, _) => r.addr() as i64,
                    $($ty::$smut(c) => c.read().to_int(),)?
                    $($ty::$sconst(c) => c.to_int(),)?
                }
            }

            /// The float coercion, one match on the payload.
            pub fn to_float(&self) -> f64 {
                match self {
                    $ty::Undef(_) => 0.0,
                    $ty::Integer(n, _) => *n as f64,
                    $ty::Float(f, _) => *f,
                    $ty::String(s) => s.to_float(),
                    $ty::True => 1.0,
                    $ty::False => 0.0,
                    $ty::ScalarRefMut(c, _) => HeapArc::as_ptr(c) as usize as f64,
                    $ty::ScalarRefConst(c, _) => HeapArc::as_ptr(c) as usize as f64,
                    $ty::ArrayRef(r, _) => r.addr() as f64,
                    $ty::HashRef(r, _) => r.addr() as f64,
                    $($ty::$smut(c) => c.read().to_float(),)?
                    $($ty::$sconst(c) => c.to_float(),)?
                }
            }

            /// Numification with perl's int-vs-float classification: integer payloads and exactly-integral string
            /// tokens in i64 range numify as integers; everything else as floats.
            pub fn numify(&self) -> Numeric {
                match self {
                    $ty::Undef(_) => Numeric::Integer(0),
                    $ty::Integer(n, _) => Numeric::Integer(*n),
                    $ty::Float(f, _) => Numeric::Float(*f),
                    $ty::String(s) => s.numify(),
                    $ty::True => Numeric::Integer(1),
                    $ty::False => Numeric::Integer(0),
                    $ty::ScalarRefMut(c, _) => Numeric::Integer(HeapArc::as_ptr(c) as usize as i64),
                    $ty::ScalarRefConst(c, _) => Numeric::Integer(HeapArc::as_ptr(c) as usize as i64),
                    $ty::ArrayRef(r, _) => Numeric::Integer(r.addr() as i64),
                    $ty::HashRef(r, _) => Numeric::Integer(r.addr() as i64),
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
                    $ty::Undef(t) => (PerlString::empty(), *t),
                    $ty::Integer(n, t) => {
                        let mut rendered = PerlString::empty();
                        format_int_into(*n, &mut rendered)?;
                        (rendered, *t)
                    }
                    $ty::Float(f, t) => {
                        let mut rendered = PerlString::empty();
                        format_float_into(*f, &mut rendered)?;
                        (rendered, *t)
                    }
                    $ty::String(s) => return Ok(s.clone()),
                    $ty::True => (PerlString::from_bytes(b"1")?, Tainted::CLEAN),
                    $ty::False => (PerlString::empty(), Tainted::CLEAN),

                    // Container-verified form: SCALAR(0x...) with lowercase hex.
                    $ty::ScalarRefMut(c, t) => (ref_repr("SCALAR", HeapArc::as_ptr(c) as usize)?, *t),
                    $ty::ScalarRefConst(c, t) => (ref_repr("SCALAR", HeapArc::as_ptr(c) as usize)?, *t),
                    $ty::ArrayRef(r, t) => (ref_repr("ARRAY", r.addr())?, *t),
                    $ty::HashRef(r, t) => (ref_repr("HASH", r.addr())?, *t),
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
                match self {
                    $ty::Undef(t)
                    | $ty::Integer(_, t)
                    | $ty::Float(_, t)
                    | $ty::ScalarRefMut(_, t)
                    | $ty::ScalarRefConst(_, t)
                    | $ty::ArrayRef(_, t)
                    | $ty::HashRef(_, t) => t.is_tainted(),
                    $ty::String(s) => s.is_tainted(),
                    $ty::True | $ty::False => false,
                    $($ty::$smut(c) => c.read().is_tainted(),)?
                    $($ty::$sconst(c) => c.is_tainted(),)?
                }
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
            Value::True => Some(crate::scalar::TRUE_SCALAR.clone()),
            Value::False => Some(crate::scalar::FALSE_SCALAR.clone()),
            _ => None,
        }
    }

    /// `\$x` — the taking-a-reference upgrade trigger (§2.2.8): promote the slot in place through the `Scalar` variant
    /// (a stable identity the slot now aliases) and return the reference value.  Idempotent on identity: taking twice
    /// yields `ptr_eq` references.  The reference value itself is clean — taint belongs to the referent.
    pub fn take_ref(slot: &mut Value) -> Value {
        match slot {
            Value::ScalarMut(c) => return Value::ScalarRefMut(c.clone(), Tainted::CLEAN),
            Value::ScalarConst(c) => return Value::ScalarRefConst(c.clone(), Tainted::CLEAN),
            _ => {}
        }

        let payload = match mem::take(slot) {
            Value::Undef(t) => ScalarPayload::Undef(t),
            Value::Integer(n, t) => ScalarPayload::Integer(n, t),
            Value::Float(f, t) => ScalarPayload::Float(f, t),
            Value::String(s) => ScalarPayload::String(s),
            Value::True => ScalarPayload::True,
            Value::False => ScalarPayload::False,
            Value::ScalarRefMut(c, t) => ScalarPayload::ScalarRefMut(c, t),
            Value::ScalarRefConst(c, t) => ScalarPayload::ScalarRefConst(c, t),
            Value::ArrayRef(r, t) => ScalarPayload::ArrayRef(r, t),
            Value::HashRef(r, t) => ScalarPayload::HashRef(r, t),
            Value::ScalarMut(c) => {
                // Unreachable (handled above); restore and share rather than panic.
                *slot = Value::ScalarMut(c.clone());
                return Value::ScalarRefMut(c, Tainted::CLEAN);
            }
            Value::ScalarConst(c) => {
                *slot = Value::ScalarConst(c.clone());
                return Value::ScalarRefConst(c, Tainted::CLEAN);
            }
        };

        let cell = HeapArc::new(RwLock::new(ScalarCell::Plain(payload)));
        *slot = Value::ScalarMut(cell.clone());

        Value::ScalarRefMut(cell, Tainted::CLEAN)
    }

    /// Whether this value holds a strong graph edge (§2.4.9): the reference and aliasing variants.  Non-edge values
    /// cannot recurse when dropped and skip the release worklist.
    pub(crate) fn carries_strong_edge(&self) -> bool {
        matches!(
            self,
            Value::ScalarRefMut(..) | Value::ScalarRefConst(..) | Value::ArrayRef(..) | Value::HashRef(..) | Value::ScalarMut(_) | Value::ScalarConst(_)
        )
    }

    /// Rehydrate a payload as a slot value.  Consumers: the §2.4.9 release path (a dying cell's payload enters the
    /// worklist as a value) and, eventually, the ops layer's slot writes.
    pub(crate) fn from_payload(p: ScalarPayload) -> Value {
        match p {
            ScalarPayload::Undef(t) => Value::Undef(t),
            ScalarPayload::Integer(n, t) => Value::Integer(n, t),
            ScalarPayload::Float(f, t) => Value::Float(f, t),
            ScalarPayload::String(s) => Value::String(s),
            ScalarPayload::True => Value::True,
            ScalarPayload::False => Value::False,
            ScalarPayload::ScalarRefMut(c, t) => Value::ScalarRefMut(c, t),
            ScalarPayload::ScalarRefConst(c, t) => Value::ScalarRefConst(c, t),
            ScalarPayload::ArrayRef(r, t) => Value::ArrayRef(r, t),
            ScalarPayload::HashRef(r, t) => Value::HashRef(r, t),
        }
    }

    /// `$$r` — scalar dereference: the identity behind a reference value (through the aliasing variant if the slot is
    /// promoted).  `None` for non-references; the "Not a SCALAR reference" error is ops-layer.  `@$r` — array
    /// dereference: the shared identity behind an array-reference value (through the aliasing variant if the slot is
    /// promoted).  "Not an ARRAY reference" is ops-layer.
    pub fn deref_array(&self) -> Option<ArrayRef> {
        fn from_payload(p: &ScalarPayload) -> Option<ArrayRef> {
            match p {
                ScalarPayload::ArrayRef(r, _) => Some(r.clone()),
                _ => None,
            }
        }

        match self {
            Value::ArrayRef(r, _) => Some(r.clone()),
            Value::ScalarMut(cell) => from_payload(cell.read().payload()),
            Value::ScalarConst(cs) => from_payload(cs.payload()),
            _ => None,
        }
    }

    /// `%$r` — hash dereference.
    pub fn deref_hash(&self) -> Option<HashRef> {
        fn from_payload(p: &ScalarPayload) -> Option<HashRef> {
            match p {
                ScalarPayload::HashRef(r, _) => Some(r.clone()),
                _ => None,
            }
        }

        match self {
            Value::HashRef(r, _) => Some(r.clone()),
            Value::ScalarMut(cell) => from_payload(cell.read().payload()),
            Value::ScalarConst(cs) => from_payload(cs.payload()),
            _ => None,
        }
    }

    pub fn deref_scalar(&self) -> Option<ScalarRef> {
        fn from_payload(p: &ScalarPayload) -> Option<ScalarRef> {
            match p {
                ScalarPayload::ScalarRefMut(c, _) => Some(ScalarRef::Mut(c.clone())),
                ScalarPayload::ScalarRefConst(c, _) => Some(ScalarRef::Const(c.clone())),
                _ => None,
            }
        }

        match self {
            Value::ScalarRefMut(c, _) => Some(ScalarRef::Mut(c.clone())),
            Value::ScalarRefConst(c, _) => Some(ScalarRef::Const(c.clone())),
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
/// only the implicit conversion.
fn format_float_into(n: f64, out: &mut PerlString) -> Result<(), AllocError> {
    if n.is_nan() {
        return out.push_str("NaN");
    }

    if n.is_infinite() {
        return out.push_str(if n < 0.0 { "-Inf" } else { "Inf" });
    }

    if n == 0.0 {
        return out.push_str("0"); // Covers -0.0, which perl also prints as "0".
    }

    // "{:.14e}" is the normalized d.dddddddddddddd form: 15 significant digits, correctly rounded.
    let mut scientific = ScientificBuf::new();
    let _ = write!(scientific, "{n:.14e}");
    let rendered = scientific.as_bytes();

    let Some(e) = rendered.iter().position(|&b| b == b'e') else {
        return out.push_bytes(rendered); // Unreachable: exponent form always contains 'e'.
    };
    let (mantissa, exponent) = (&rendered[..e], &rendered[e + 1..]);
    let Ok(exp) = str::from_utf8(exponent).unwrap_or("").parse::<i32>() else {
        return out.push_bytes(rendered); // Unreachable likewise.
    };

    let negative = mantissa.first() == Some(&b'-');

    // The significant digits, trailing zeros trimmed — %g drops them.
    let mut digits = [0u8; 16];
    let mut count = 0;
    for &b in mantissa {
        if b.is_ascii_digit() && count < digits.len() {
            digits[count] = b;
            count += 1;
        }
    }

    while count > 1 && digits[count - 1] == b'0' {
        count -= 1;
    }

    let digits = &digits[..count];

    if negative {
        out.push_str("-")?;
    }

    // %g takes exponent form when the decimal exponent is below -4 or at/above the precision (15).
    if !(-4..15).contains(&exp) {
        out.push_bytes(&digits[..1])?;

        if count > 1 {
            out.push_str(".")?;
            out.push_bytes(&digits[1..])?;
        }

        let magnitude = exp.unsigned_abs();
        let sign = if exp < 0 { '-' } else { '+' };

        // Perl pads the exponent to two digits: 1e-05, not 1e-5.
        return out.push_fmt(format_args!("e{sign}{magnitude:02}"));
    }

    if exp >= 0 {
        let int_len = exp as usize + 1;

        if count <= int_len {
            out.push_bytes(digits)?;
            push_zeros(out, int_len - count)?;
        } else {
            out.push_bytes(&digits[..int_len])?;
            out.push_str(".")?;
            out.push_bytes(&digits[int_len..])?;
        }
    } else {
        out.push_str("0.")?;
        push_zeros(out, (-exp - 1) as usize)?;
        out.push_bytes(digits)?;
    }

    Ok(())
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
        Ok(()) => String::from_utf8_lossy(out.as_bytes()).into_owned(),
        Err(_) => String::new(),
    }
}

/// Perl's integer stringification, rendered without allocating.
fn format_int_into(n: i64, out: &mut PerlString) -> Result<(), AllocError> {
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

        // Exact as an unsigned 64-bit value but beyond i64, and larger: Float under the deferred-UV rule (§2.2.2).
    }

    Numeric::Float(parse_float(bytes))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/value_tests.rs"]
mod tests;
