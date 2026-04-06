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

    // ── Truthiness ────────────────────────────────────────────

    /// Perl truthiness.
    ///
    /// False values: `undef`, `0`, `0.0`, `""`, `"0"`, empty arrays,
    /// empty hashes.
    /// True values: nonzero numbers, non-false strings, references,
    /// non-empty arrays, non-empty hashes, code refs, regexes.
    pub fn is_true(&self) -> bool {
        match self {
            Value::Undef => false,
            Value::Int(n) => *n != 0,
            Value::Float(n) => *n != 0.0,
            Value::SmallStr(ss) => !bytes_are_false(ss.as_bytes()),
            Value::Str(ps) => !bytes_are_false(ps.as_bytes()),
            Value::Ref(_) => true,
            Value::Scalar(sv) => sv.read().map(|s| s.is_true()).unwrap_or(false),
            Value::Array(av) => av.read().map(|a| !a.is_empty()).unwrap_or(false),
            Value::Hash(hv) => hv.read().map(|h| !h.is_empty()).unwrap_or(false),
            Value::Code(_) | Value::Regex(_) => true,
        }
    }

    /// The logical inverse of `is_true`.
    pub fn is_false(&self) -> bool {
        !self.is_true()
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
                    Value::Int(n) => Scalar::from_int(n),
                    Value::Float(n) => Scalar::from_num(n),
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
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            Value::Float(n) => Some(*n as i64),
            _ => None,
        }
    }

    /// Try to read as f64 without upgrading.
    pub fn as_num(&self) -> Option<f64> {
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

    // ── Stringification ───────────────────────────────────────

    /// Convert this value to its Perl string representation.
    ///
    /// This is the operation that happens when a value is used in
    /// string context: `print`, `.` concatenation, `"$x"` interpolation,
    /// `eq`/`ne` comparison.
    ///
    /// Rules:
    /// - `Undef` → `""` (empty string; in practice Perl warns)
    /// - `Int(n)` → decimal representation
    /// - `Float(n)` → Perl-style float formatting
    /// - `SmallStr`/`Str` → the string itself (clone)
    /// - `Ref(sv)` → `"SCALAR(0xADDR)"`
    /// - `Scalar(sv)` → delegates through lock, coerces if needed
    /// - `Array(av)` → element count (Perl's `@arr` in string context)
    /// - `Hash(hv)` → `"N/M"` buckets string (Perl 5 behavior)
    /// - `Code` → `"CODE(0xADDR)"`
    /// - `Regex` → `"Regex(0xADDR)"` (placeholder)
    pub fn stringify(&self) -> PerlString {
        match self {
            Value::Undef => PerlString::new(),
            Value::Int(n) => PerlString::from_str(&n.to_string()),
            Value::Float(n) => PerlString::from_str(&crate::scalar::format_nv(*n)),
            Value::SmallStr(ss) => ss.to_perl_string(),
            Value::Str(ps) => ps.clone(),
            Value::Ref(sv) => {
                let addr = Arc::as_ptr(sv) as usize;
                PerlString::from_str(&format!("SCALAR(0x{:x})", addr))
            }
            Value::Scalar(sv) => {
                let mut guard = sv.write().expect("Scalar lock poisoned");
                guard.stringify()
            }
            Value::Array(av) => {
                let len = av.read().map(|a| a.len()).unwrap_or(0);
                PerlString::from_str(&len.to_string())
            }
            Value::Hash(hv) => {
                // Perl 5 stringifies a hash as "N/M" where N = used buckets,
                // M = total buckets.  We approximate with "N/N" (key count).
                let len = hv.read().map(|h| h.len()).unwrap_or(0);
                if len == 0 { PerlString::from_str("0") } else { PerlString::from_str(&format!("{}/{}", len, len)) }
            }
            Value::Code(c) => {
                let addr = Arc::as_ptr(c) as usize;
                PerlString::from_str(&format!("CODE(0x{:x})", addr))
            }
            Value::Regex(r) => {
                let addr = Arc::as_ptr(r) as usize;
                PerlString::from_str(&format!("Regex(0x{:x})", addr))
            }
        }
    }

    /// Write the string representation to a byte buffer.
    ///
    /// More efficient than `stringify()` when you just need to
    /// append to output — avoids allocating a PerlString for types
    /// that are already byte slices.
    pub fn write_bytes_to(&self, buf: &mut Vec<u8>) {
        match self {
            Value::Undef => {} // empty string — no bytes
            Value::Int(n) => {
                use std::io::Write;
                let _ = write!(buf, "{}", n);
            }
            Value::Float(n) => {
                buf.extend_from_slice(crate::scalar::format_nv(*n).as_bytes());
            }
            Value::SmallStr(ss) => buf.extend_from_slice(ss.as_bytes()),
            Value::Str(ps) => buf.extend_from_slice(ps.as_bytes()),
            _ => {
                // For ref/scalar/array/hash/code/regex, fall back to
                // stringify() — these are less common in hot output paths.
                let ps = self.stringify();
                buf.extend_from_slice(ps.as_bytes());
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────

/// Perl string falseness check on raw bytes.
/// Empty (`b""`) and the string `"0"` (`b"0"`) are false.
#[inline]
fn bytes_are_false(bytes: &[u8]) -> bool {
    bytes.is_empty() || bytes == b"0"
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

impl fmt::Display for Value {
    /// Perl stringification — what `print $value` produces.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Undef => Ok(()), // empty string
            Value::Int(n) => write!(f, "{}", n),
            Value::Float(n) => write!(f, "{}", crate::scalar::format_nv(*n)),
            Value::SmallStr(ss) => fmt::Display::fmt(ss, f),
            Value::Str(ps) => fmt::Display::fmt(ps, f),
            _ => {
                // References, scalars, containers, code, regex —
                // convert via stringify to avoid duplicating logic.
                let ps = self.stringify();
                fmt::Display::fmt(&ps, f)
            }
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
        assert_eq!(v.as_int(), Some(42));
        assert!((v.as_num().unwrap() - 42.0).abs() < 1e-10);
    }

    #[test]
    fn float_value() {
        let v = Value::from(3.14f64);
        assert!(v.is_defined());
        assert!((v.as_num().unwrap() - 3.14).abs() < 1e-10);
        assert_eq!(v.as_int(), Some(3)); // truncation
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
        assert_eq!(Value::from(true).as_int(), Some(1));
        assert_eq!(Value::from(false).as_int(), Some(0));
    }

    #[test]
    fn upgrade_int_to_scalar() {
        let mut v = Value::from(42i64);
        assert!(matches!(v, Value::Int(42)));

        let sv = v.upgrade_to_scalar();
        assert!(matches!(v, Value::Scalar(_)));

        // The Scalar should have int=42 with INT_VALID set
        let guard = sv.read().unwrap();
        assert!(guard.flags().contains(crate::flags::SvFlags::INT_VALID));
    }

    #[test]
    fn upgrade_string_to_scalar() {
        let mut v = Value::from("hello");
        let sv = v.upgrade_to_scalar();
        assert!(matches!(v, Value::Scalar(_)));

        let guard = sv.read().unwrap();
        assert!(guard.flags().contains(crate::flags::SvFlags::STR_VALID));
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

    // ── Truthiness tests ──────────────────────────────────────

    #[test]
    fn undef_is_false() {
        assert!(Value::Undef.is_false());
        assert!(!Value::Undef.is_true());
    }

    #[test]
    fn int_truthiness() {
        assert!(Value::Int(0).is_false());
        assert!(Value::Int(1).is_true());
        assert!(Value::Int(-1).is_true());
        assert!(Value::Int(42).is_true());
    }

    #[test]
    fn float_truthiness() {
        assert!(Value::Float(0.0).is_false());
        assert!(Value::Float(1.0).is_true());
        assert!(Value::Float(-0.5).is_true());
        assert!(Value::Float(f64::NAN).is_true());
        assert!(Value::Float(f64::INFINITY).is_true());
    }

    #[test]
    fn string_truthiness() {
        // False strings
        assert!(Value::from("").is_false());
        assert!(Value::from("0").is_false());

        // True strings
        assert!(Value::from("1").is_true());
        assert!(Value::from("hello").is_true());
        assert!(Value::from("00").is_true());
        assert!(Value::from("0.0").is_true());
        assert!(Value::from(" ").is_true());
        assert!(Value::from("0E0").is_true());
    }

    #[test]
    fn long_string_truthiness() {
        // Same rules apply to heap-allocated strings
        let long_true = "a".repeat(30);
        assert!(Value::from(long_true.as_str()).is_true());

        // PerlString empty
        assert!(Value::Str(PerlString::from_str("")).is_false());
        assert!(Value::Str(PerlString::from_str("0")).is_false());
        assert!(Value::Str(PerlString::from_str("hello")).is_true());
    }

    #[test]
    fn reference_always_true() {
        // A reference is always true, even to a false value.
        let sv = Arc::new(RwLock::new(Scalar::from_int(0)));
        assert!(Value::Ref(sv).is_true());
    }

    #[test]
    fn empty_containers_are_false() {
        let av = Arc::new(RwLock::new(Vec::new()));
        assert!(Value::Array(av).is_false());

        let hv = Arc::new(RwLock::new(HashMap::new()));
        assert!(Value::Hash(hv).is_false());
    }

    #[test]
    fn nonempty_containers_are_true() {
        let av = Arc::new(RwLock::new(vec![Value::Int(1)]));
        assert!(Value::Array(av).is_true());

        let mut map = HashMap::new();
        map.insert(PerlString::from_str("key"), Value::Int(1));
        let hv = Arc::new(RwLock::new(map));
        assert!(Value::Hash(hv).is_true());
    }

    #[test]
    fn upgraded_scalar_truthiness() {
        // Upgrade an int to Scalar, truthiness should still work.
        let mut v = Value::from(0i64);
        v.upgrade_to_scalar();
        assert!(v.is_false());

        let mut v = Value::from(42i64);
        v.upgrade_to_scalar();
        assert!(v.is_true());

        let mut v = Value::from("");
        v.upgrade_to_scalar();
        assert!(v.is_false());

        let mut v = Value::from("hello");
        v.upgrade_to_scalar();
        assert!(v.is_true());
    }

    #[test]
    fn from_bool_truthiness() {
        assert!(Value::from(true).is_true());
        assert!(Value::from(false).is_false());
    }

    // ── Stringification tests ─────────────────────────────────

    #[test]
    fn stringify_undef() {
        let v = Value::Undef;
        assert_eq!(format!("{}", v), "");
        assert!(v.stringify().is_empty());
    }

    #[test]
    fn stringify_int() {
        assert_eq!(format!("{}", Value::Int(42)), "42");
        assert_eq!(format!("{}", Value::Int(0)), "0");
        assert_eq!(format!("{}", Value::Int(-7)), "-7");

        let ps = Value::Int(42).stringify();
        assert_eq!(ps.as_str(), Some("42"));
        assert!(ps.is_utf8());
    }

    #[test]
    fn stringify_float() {
        assert_eq!(format!("{}", Value::Float(3.14)), "3.14");
        assert_eq!(format!("{}", Value::Float(0.0)), "0");
        assert_eq!(format!("{}", Value::Float(-2.5)), "-2.5");
        assert_eq!(format!("{}", Value::Float(1000.0)), "1000");
    }

    #[test]
    fn stringify_small_str() {
        let v = Value::from("hello");
        assert_eq!(format!("{}", v), "hello");
        assert_eq!(v.stringify().as_str(), Some("hello"));
    }

    #[test]
    fn stringify_long_str() {
        let long = "a".repeat(30);
        let v = Value::from(long.as_str());
        assert_eq!(format!("{}", v), long);
    }

    #[test]
    fn stringify_reference() {
        let sv = Arc::new(RwLock::new(Scalar::from_int(42)));
        let v = Value::Ref(sv);
        let s = format!("{}", v);
        assert!(s.starts_with("SCALAR(0x"));
        assert!(s.ends_with(')'));
    }

    #[test]
    fn stringify_upgraded_scalar() {
        let mut v = Value::from(42i64);
        v.upgrade_to_scalar();
        assert_eq!(format!("{}", v), "42");

        let mut v = Value::from("hello");
        v.upgrade_to_scalar();
        assert_eq!(format!("{}", v), "hello");
    }

    #[test]
    fn stringify_array() {
        // Empty array → "0"
        let av = Arc::new(RwLock::new(Vec::new()));
        assert_eq!(format!("{}", Value::Array(av)), "0");

        // 3-element array → "3"
        let av = Arc::new(RwLock::new(vec![Value::Int(1), Value::Int(2), Value::Int(3)]));
        assert_eq!(format!("{}", Value::Array(av)), "3");
    }

    #[test]
    fn stringify_hash() {
        // Empty hash → "0"
        let hv = Arc::new(RwLock::new(HashMap::new()));
        assert_eq!(format!("{}", Value::Hash(hv)), "0");

        // Non-empty hash → "N/N"
        let mut map = HashMap::new();
        map.insert(PerlString::from_str("a"), Value::Int(1));
        map.insert(PerlString::from_str("b"), Value::Int(2));
        let hv = Arc::new(RwLock::new(map));
        assert_eq!(format!("{}", Value::Hash(hv)), "2/2");
    }

    #[test]
    fn write_bytes_to_buffer() {
        let mut buf = Vec::new();
        Value::Int(42).write_bytes_to(&mut buf);
        assert_eq!(&buf, b"42");

        buf.clear();
        Value::from("hello").write_bytes_to(&mut buf);
        assert_eq!(&buf, b"hello");

        buf.clear();
        Value::Undef.write_bytes_to(&mut buf);
        assert!(buf.is_empty());

        buf.clear();
        Value::Float(3.14).write_bytes_to(&mut buf);
        assert_eq!(&buf, b"3.14");
    }
}
