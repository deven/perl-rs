//! The top-level `Value` enum — compact variants for common cases,
//! `Arc`-wrapped variants for shared values.
//!
//! Most Perl values are simple: just a number, just a string, just a
//! reference.  The compact `Value` variants handle these with no heap
//! allocation and no locking overhead.  Only values that need full Perl
//! SV semantics (multi-representation caching, magic, blessing) are
//! upgraded to `Value::Scalar(Sv)`.

use std::fmt;
use std::sync::{Arc, RwLock};

use perl_string::{PerlString, SmallString};

use crate::scalar::Scalar;

// ── Type aliases ─────────────────────────────────────────────────

/// A shared scalar — the full Perl SV behind Arc<RwLock<>>.
pub type Sv = Arc<RwLock<Scalar>>;

/// A shared array.
pub type Av = Arc<RwLock<Vec<Value>>>;

/// A shared hash.
pub type Hv = Arc<RwLock<HashMap<PerlString, Value>>>;

// Placeholder for Code — will hold compiled IR + captures.
/// A compiled code reference.
pub struct Code {
    // TODO: IR function ID, captured values, prototype, attributes
    _private: (),
}

// Placeholder for CompiledRegex — will be the perl-regex crate's type.
/// A compiled regular expression.
pub struct CompiledRegex {
    // TODO: compiled pattern, flags, named captures
    _private: (),
}

use std::collections::HashMap;

// ── The Value enum ───────────────────────────────────────────────

/// A Perl value.
///
/// The compact variants (`Int`, `Float`, `SmallStr`, `Str`) are inline —
/// no heap allocation, no `Arc`, no locking.  These cover the vast
/// majority of values in a typical Perl program.
///
/// `Scalar(Sv)` is the full Perl SV with multi-representation caching,
/// magic, and blessing.  Values are upgraded to this form when needed
/// (see [`Value::upgrade_to_scalar`]).
///
/// Container types (`Array`, `Hash`) and code/regex are always `Arc`-wrapped
/// because they have shared identity from creation.
#[derive(Clone)]
pub enum Value {
    // ── Compact scalar variants (no heap allocation) ──────────
    /// Undefined value.
    Undef,

    /// Integer — just an i64, no SV overhead.
    Int(i64),

    /// Float — just an f64, no SV overhead.
    Float(f64),

    /// Short string (≤22 bytes) — inline, no heap allocation.
    SmallStr(SmallString),

    /// Longer string — heap-allocated PerlString.
    Str(PerlString),

    /// Reference — points to a full Scalar (which holds the target).
    Ref(Sv),

    // ── Full scalar ───────────────────────────────────────────
    /// Full Perl SV — multi-rep caching, magic, blessing, etc.
    /// Upgrade target for compact variants when they need SV features.
    Scalar(Sv),

    // ── Container types ───────────────────────────────────────
    /// Array.
    Array(Av),

    /// Hash.
    Hash(Hv),

    // ── Code and regex ────────────────────────────────────────
    /// Code reference (compiled IR + captures).
    Code(Arc<Code>),

    /// Compiled regex.
    Regex(Arc<CompiledRegex>),
}

impl Value {
    // ── Type testing ──────────────────────────────────────────

    /// Whether this value is undef.
    pub fn is_undef(&self) -> bool {
        matches!(self, Value::Undef)
    }

    /// Whether this value is defined (not undef).
    pub fn is_defined(&self) -> bool {
        !self.is_undef()
    }

    /// Whether this value is a reference.
    pub fn is_ref(&self) -> bool {
        matches!(self, Value::Ref(_))
    }

    /// Whether this value is an array.
    pub fn is_array(&self) -> bool {
        matches!(self, Value::Array(_))
    }

    /// Whether this value is a hash.
    pub fn is_hash(&self) -> bool {
        matches!(self, Value::Hash(_))
    }

    /// Whether this value is a code reference.
    pub fn is_code(&self) -> bool {
        matches!(self, Value::Code(_))
    }

    // ── Upgrade to full Scalar ────────────────────────────────

    /// Upgrade this value to a full `Scalar` behind `Arc<RwLock<>>`.
    ///
    /// If already a `Scalar`, returns a clone of the `Sv` (refcount bump).
    /// Otherwise, creates a new `Scalar` from the compact representation,
    /// replaces `self` with `Value::Scalar(sv)`, and returns the `Sv`.
    ///
    /// **Once upgraded, never downgrade.**  Identity via `Arc` address
    /// must be preserved.
    pub fn upgrade_to_scalar(&mut self) -> Sv {
        match self {
            Value::Scalar(sv) => sv.clone(),
            _ => {
                let scalar = match std::mem::replace(self, Value::Undef) {
                    Value::Undef => Scalar::new_undef(),
                    Value::Int(n) => Scalar::from_iv(n),
                    Value::Float(n) => Scalar::from_nv(n),
                    Value::SmallStr(ss) => Scalar::from_perl_string(ss.to_perl_string()),
                    Value::Str(ps) => Scalar::from_perl_string(ps),
                    Value::Ref(target_sv) => Scalar::from_ref(Value::Scalar(target_sv)),
                    // Already handled above
                    Value::Scalar(_) => unreachable!(),
                    // Containers/code/regex shouldn't upgrade to Scalar
                    other => {
                        // Put it back and panic — this is a logic error
                        *self = other;
                        panic!("Cannot upgrade Array/Hash/Code/Regex to Scalar");
                    }
                };
                let sv = Arc::new(RwLock::new(scalar));
                *self = Value::Scalar(sv.clone());
                sv
            }
        }
    }

    // ── Convenience accessors ─────────────────────────────────
    // These provide quick access to the underlying value without
    // upgrading.  For coercing access (e.g., reading a string as
    // an integer), upgrade to Scalar first and use its coercion
    // methods.

    /// Try to read as i64 without upgrading.
    /// Returns `None` for non-numeric compact types.
    pub fn as_iv(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            Value::Float(n) => Some(*n as i64),
            _ => None,
        }
    }

    /// Try to read as f64 without upgrading.
    pub fn as_nv(&self) -> Option<f64> {
        match self {
            Value::Int(n) => Some(*n as f64),
            Value::Float(n) => Some(*n),
            _ => None,
        }
    }

    /// Try to read as `&str` without upgrading.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::SmallStr(ss) => ss.as_str(),
            Value::Str(ps) => ps.as_str(),
            _ => None,
        }
    }

    /// Try to read as byte slice without upgrading.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::SmallStr(ss) => Some(ss.as_bytes()),
            Value::Str(ps) => Some(ps.as_bytes()),
            _ => None,
        }
    }
}

// ── Trait impls ──────────────────────────────────────────────────

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Undef => write!(f, "Undef"),
            Value::Int(n) => write!(f, "Int({})", n),
            Value::Float(n) => write!(f, "Float({})", n),
            Value::SmallStr(ss) => write!(f, "SmallStr({:?})", ss),
            Value::Str(ps) => write!(f, "Str({:?})", ps),
            Value::Ref(sv) => write!(f, "Ref({:?})", sv),
            Value::Scalar(sv) => write!(f, "Scalar({:?})", sv),
            Value::Array(av) => write!(f, "Array(<{} elems>)", { av.read().map(|a| a.len()).unwrap_or(0) }),
            Value::Hash(hv) => write!(f, "Hash(<{} keys>)", { hv.read().map(|h| h.len()).unwrap_or(0) }),
            Value::Code(_) => write!(f, "Code(...)"),
            Value::Regex(_) => write!(f, "Regex(...)"),
        }
    }
}

impl Default for Value {
    fn default() -> Self {
        Value::Undef
    }
}

// Convenience From impls for creating Values from Rust types.

impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Value::Int(n)
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Value::Float(n)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        match SmallString::from_str(s) {
            Some(ss) => Value::SmallStr(ss),
            None => Value::Str(PerlString::from_str(s)),
        }
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        if s.len() <= perl_string::SMALL_STRING_MAX {
            // Safe because String is always valid UTF-8
            Value::SmallStr(SmallString::from_str(&s).unwrap())
        } else {
            Value::Str(PerlString::from(s))
        }
    }
}

impl From<PerlString> for Value {
    fn from(ps: PerlString) -> Self {
        Value::Str(ps)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Int(if b { 1 } else { 0 })
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undef() {
        let v = Value::Undef;
        assert!(v.is_undef());
        assert!(!v.is_defined());
    }

    #[test]
    fn int_value() {
        let v = Value::from(42i64);
        assert!(v.is_defined());
        assert_eq!(v.as_iv(), Some(42));
        assert!((v.as_nv().unwrap() - 42.0).abs() < 1e-10);
    }

    #[test]
    fn float_value() {
        let v = Value::from(3.14f64);
        assert!(v.is_defined());
        assert!((v.as_nv().unwrap() - 3.14).abs() < 1e-10);
        assert_eq!(v.as_iv(), Some(3)); // truncation
    }

    #[test]
    fn short_string_uses_small_str() {
        let v = Value::from("hello");
        assert!(matches!(v, Value::SmallStr(_)));
        assert_eq!(v.as_str(), Some("hello"));
    }

    #[test]
    fn long_string_uses_str() {
        let long = "a".repeat(30);
        let v = Value::from(long.as_str());
        assert!(matches!(v, Value::Str(_)));
        assert_eq!(v.as_str(), Some(long.as_str()));
    }

    #[test]
    fn from_bool() {
        assert_eq!(Value::from(true).as_iv(), Some(1));
        assert_eq!(Value::from(false).as_iv(), Some(0));
    }

    #[test]
    fn upgrade_int_to_scalar() {
        let mut v = Value::from(42i64);
        assert!(matches!(v, Value::Int(42)));

        let sv = v.upgrade_to_scalar();
        assert!(matches!(v, Value::Scalar(_)));

        // The Scalar should have iv=42 with IOK set
        let guard = sv.read().unwrap();
        assert!(guard.flags().contains(crate::flags::SvFlags::IOK));
    }

    #[test]
    fn upgrade_string_to_scalar() {
        let mut v = Value::from("hello");
        let sv = v.upgrade_to_scalar();
        assert!(matches!(v, Value::Scalar(_)));

        let guard = sv.read().unwrap();
        assert!(guard.flags().contains(crate::flags::SvFlags::POK));
    }

    #[test]
    fn upgrade_is_idempotent() {
        let mut v = Value::from(42i64);
        let sv1 = v.upgrade_to_scalar();
        let sv2 = v.upgrade_to_scalar();
        // Both should be the same Arc (same allocation)
        assert!(Arc::ptr_eq(&sv1, &sv2));
    }

    #[test]
    fn upgrade_undef() {
        let mut v = Value::Undef;
        let sv = v.upgrade_to_scalar();
        let guard = sv.read().unwrap();
        assert!(!guard.is_defined());
    }

    #[test]
    fn value_default_is_undef() {
        let v: Value = Default::default();
        assert!(v.is_undef());
    }

    #[test]
    fn debug_formatting() {
        assert_eq!(format!("{:?}", Value::Undef), "Undef");
        assert_eq!(format!("{:?}", Value::from(42i64)), "Int(42)");
    }
}
