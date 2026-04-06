//! Full Perl scalar — multi-representation caching with flag-driven validity.
//!
//! A `Scalar` is the "heavy" representation used when a value needs:
//! - Multiple cached representations (string + integer + float)
//! - Magic (tied variables, overloaded objects)
//! - Blessing into a package (objects)
//! - Readonly / taint flags
//!
//! Most values start as compact `Value` variants and are upgraded to
//! `Scalar` only when needed (see `Value::upgrade_to_scalar`).

use std::fmt;
use std::sync::Arc;

use perl_string::{PerlString, PerlStringSlot};

use crate::flags::SvFlags;
use crate::value::Value;

// Placeholder types — will be fleshed out as the runtime develops.
// Using empty structs for now so the code compiles and the shape is right.

/// A chain of magic (tie, overload, etc.) attached to a scalar.
/// Will be a linked list of trait objects implementing the Magic trait.
pub struct MagicChain {
    // TODO: Vec<Box<dyn Magic>> or linked list
    _private: (),
}

/// A package stash — the symbol table for a package.
/// Will be a HashMap<PerlString, Glob> or similar.
pub struct Stash {
    // TODO: name, symbol table, ISA, method cache
    _private: (),
}

/// The full Perl scalar.
///
/// Parallel representation slots with flag-driven validity, following
/// Perl 5's SV model.  `$x = "42"` sets POK; `$x + 0` sets IOK and
/// caches 42 in `int` without clearing the string.
///
/// # Flag Discipline
///
/// - `int` is meaningful only when `flags.contains(SvFlags::IOK)`.
/// - `num` is meaningful only when `flags.contains(SvFlags::NOK)`.
/// - `pv` content is meaningful only when `flags.contains(SvFlags::POK)`.
/// - `rv` is meaningful only when `flags.contains(SvFlags::ROK)`.
///
/// Writing a new representation typically invalidates the others:
/// - Setting a string: set POK, clear IOK and NOK.
/// - Setting an integer: set IOK, clear POK and NOK (unless caching).
///
/// Reading a missing representation triggers lazy coercion:
/// - Reading iv when only POK is set: parse the string, cache in int, set IOK.
pub struct Scalar {
    /// Which representations are valid + metadata.
    pub(crate) flags: SvFlags,

    /// Integer representation.  Valid when IOK is set.
    pub(crate) int: i64,

    /// Float representation.  Valid when NOK is set.
    pub(crate) num: f64,

    /// String representation.  Valid when POK is set.
    /// Uses small-string optimization internally.
    pub(crate) pv: PerlStringSlot,

    /// Reference target.  Valid when ROK is set.
    /// When set, this scalar IS a reference (like Perl's RV).
    pub(crate) rv: Option<Value>,

    /// Magic chain (tie, overload, special variable magic).
    pub(crate) magic: Option<Box<MagicChain>>,

    /// Blessed package stash (for objects).
    pub(crate) stash: Option<Arc<Stash>>,
}

impl Scalar {
    // ── Constructors ──────────────────────────────────────────────

    /// Create an undef scalar (no flags set, no valid representations).
    pub fn new_undef() -> Self {
        Scalar { flags: SvFlags::EMPTY, int: 0, num: 0.0, pv: PerlStringSlot::None, rv: None, magic: None, stash: None }
    }

    /// Create a scalar from an integer.  IOK is set.
    pub fn from_int(n: i64) -> Self {
        Scalar { flags: SvFlags::IOK, int: n, num: 0.0, pv: PerlStringSlot::None, rv: None, magic: None, stash: None }
    }

    /// Create a scalar from a float.  NOK is set.
    pub fn from_num(n: f64) -> Self {
        Scalar { flags: SvFlags::NOK, int: 0, num: n, pv: PerlStringSlot::None, rv: None, magic: None, stash: None }
    }

    /// Create a scalar from a string.  POK is set.
    pub fn from_str(s: &str) -> Self {
        let mut pv = PerlStringSlot::None;
        pv.set_str(s);
        Scalar { flags: SvFlags::POK | SvFlags::UTF8, int: 0, num: 0.0, pv, rv: None, magic: None, stash: None }
    }

    /// Create a scalar from a `PerlString`.  POK is set.
    pub fn from_perl_string(ps: PerlString) -> Self {
        let flags = if ps.is_utf8() { SvFlags::POK | SvFlags::UTF8 } else { SvFlags::POK };
        Scalar { flags, int: 0, num: 0.0, pv: PerlStringSlot::Heap(ps), rv: None, magic: None, stash: None }
    }

    /// Create a reference scalar.  ROK is set.
    pub fn from_ref(target: Value) -> Self {
        Scalar { flags: SvFlags::ROK, int: 0, num: 0.0, pv: PerlStringSlot::None, rv: Some(target), magic: None, stash: None }
    }

    // ── Flag accessors ────────────────────────────────────────────

    /// The current flags.
    pub fn flags(&self) -> SvFlags {
        self.flags
    }

    /// Whether this scalar is a reference.
    pub fn is_ref(&self) -> bool {
        self.flags.contains(SvFlags::ROK)
    }

    /// Whether this scalar is read-only.
    pub fn is_readonly(&self) -> bool {
        self.flags.contains(SvFlags::READONLY)
    }

    /// Whether magic is attached.
    pub fn is_magical(&self) -> bool {
        self.flags.contains(SvFlags::MAGICAL)
    }

    /// Whether this scalar is blessed into a package.
    pub fn is_blessed(&self) -> bool {
        self.stash.is_some()
    }

    /// Whether any value representation is valid (not undef).
    pub fn is_defined(&self) -> bool {
        self.flags.intersects(SvFlags::ANY_VAL | SvFlags::ROK)
    }

    /// Perl truthiness.
    ///
    /// A scalar is false if it is:
    /// - undef (no valid representations)
    /// - integer zero (`iv == 0` when IOK)
    /// - float zero (`nv == 0.0` when NOK)
    /// - empty string (`""` when POK)
    /// - the string `"0"` (when POK)
    ///
    /// References are always true.  Everything else is true.
    ///
    /// When multiple representations are valid, numeric is checked
    /// first (faster than string comparison).
    pub fn is_true(&self) -> bool {
        // References are always true.
        if self.flags.contains(SvFlags::ROK) {
            return true;
        }

        // Integer representation — check for zero.
        if self.flags.contains(SvFlags::IOK) {
            return self.int != 0;
        }

        // Float representation — check for zero.
        if self.flags.contains(SvFlags::NOK) {
            return self.num != 0.0;
        }

        // String representation — check for "" and "0".
        if self.flags.contains(SvFlags::POK) {
            return !string_is_false(&self.pv);
        }

        // No valid representation → undef → false.
        false
    }

    // ── Integer access (with lazy coercion) ───────────────────────

    /// Get the integer value, coercing from other representations if needed.
    /// Caches the result by setting IOK.
    pub fn get_int(&mut self) -> i64 {
        if self.flags.contains(SvFlags::IOK) {
            return self.int;
        }

        // Try to coerce from float
        if self.flags.contains(SvFlags::NOK) {
            self.int = self.num as i64;
            self.flags.insert(SvFlags::IOK);
            return self.int;
        }

        // Try to coerce from string
        if self.flags.contains(SvFlags::POK) {
            if let Some(ps) = self.pv.to_perl_string() {
                self.int = ps.parse_iv();
            } else {
                self.int = 0;
            }
            self.flags.insert(SvFlags::IOK);
            return self.int;
        }

        // Undef → 0
        0
    }

    /// Set the integer value.  Sets IOK, clears NOK and POK.
    pub fn set_int(&mut self, n: i64) {
        self.int = n;
        self.flags.insert(SvFlags::IOK);
        self.flags.remove(SvFlags::NOK | SvFlags::POK | SvFlags::UTF8);
        self.pv.clear();
    }

    // ── Float access (with lazy coercion) ─────────────────────────

    /// Get the float value, coercing from other representations if needed.
    /// Caches the result by setting NOK.
    pub fn get_num(&mut self) -> f64 {
        if self.flags.contains(SvFlags::NOK) {
            return self.num;
        }

        if self.flags.contains(SvFlags::IOK) {
            self.num = self.int as f64;
            self.flags.insert(SvFlags::NOK);
            return self.num;
        }

        if self.flags.contains(SvFlags::POK) {
            if let Some(ps) = self.pv.to_perl_string() {
                self.num = ps.parse_nv();
            } else {
                self.num = 0.0;
            }
            self.flags.insert(SvFlags::NOK);
            return self.num;
        }

        0.0
    }

    /// Set the float value.  Sets NOK, clears IOK and POK.
    pub fn set_num(&mut self, n: f64) {
        self.num = n;
        self.flags.insert(SvFlags::NOK);
        self.flags.remove(SvFlags::IOK | SvFlags::POK | SvFlags::UTF8);
        self.pv.clear();
    }

    // ── String access (with lazy coercion) ────────────────────────

    /// Get a string view, coercing from other representations if needed.
    /// Caches the result by setting POK.
    /// Returns `None` only for undef.
    pub fn get_bytes(&mut self) -> Option<&[u8]> {
        if self.flags.contains(SvFlags::POK) {
            return self.pv.as_bytes();
        }

        if self.flags.contains(SvFlags::IOK) {
            let s = self.int.to_string();
            self.pv.set_str(&s);
            self.flags.insert(SvFlags::POK | SvFlags::UTF8);
            return self.pv.as_bytes();
        }

        if self.flags.contains(SvFlags::NOK) {
            let s = format_nv(self.num);
            self.pv.set_str(&s);
            self.flags.insert(SvFlags::POK | SvFlags::UTF8);
            return self.pv.as_bytes();
        }

        // Undef
        None
    }

    /// Get a `&str` view if the string representation is UTF-8.
    /// Coerces if needed (numeric → string is always UTF-8).
    pub fn get_str(&mut self) -> Option<&str> {
        // Ensure pv is populated
        self.get_bytes()?;
        self.pv.as_str()
    }

    /// Set the string value from a `&str`.  Sets POK + UTF8, clears IOK and NOK.
    pub fn set_str(&mut self, s: &str) {
        self.pv.set_str(s);
        self.flags.insert(SvFlags::POK | SvFlags::UTF8);
        self.flags.remove(SvFlags::IOK | SvFlags::NOK);
    }

    /// Set the string value from raw bytes.  Sets POK, clears UTF8 + IOK + NOK.
    pub fn set_bytes(&mut self, bytes: &[u8]) {
        self.pv.set_bytes(bytes);
        self.flags.insert(SvFlags::POK);
        self.flags.remove(SvFlags::IOK | SvFlags::NOK | SvFlags::UTF8);
    }

    // ── Reference access ──────────────────────────────────────────

    /// Get the reference target, if this is a reference.
    pub fn get_rv(&self) -> Option<&Value> {
        if self.flags.contains(SvFlags::ROK) { self.rv.as_ref() } else { None }
    }

    /// Set this scalar to be a reference to the given value.
    /// Clears all other representations.
    pub fn set_rv(&mut self, target: Value) {
        self.rv = Some(target);
        self.flags = SvFlags::ROK;
        self.int = 0;
        self.num = 0.0;
        self.pv.clear();
    }

    // ── Magic ─────────────────────────────────────────────────────

    /// Attach a magic chain.  Sets MAGICAL flag.
    pub fn set_magic(&mut self, magic: MagicChain) {
        self.magic = Some(Box::new(magic));
        self.flags.insert(SvFlags::MAGICAL);
    }

    // ── Blessing ──────────────────────────────────────────────────

    /// Bless this scalar into a package.
    pub fn bless(&mut self, stash: Arc<Stash>) {
        self.stash = Some(stash);
    }

    /// The blessed stash, if any.
    pub fn blessed_stash(&self) -> Option<&Arc<Stash>> {
        self.stash.as_ref()
    }

    // ── Read-only ─────────────────────────────────────────────────

    /// Mark this scalar as read-only.
    pub fn set_readonly(&mut self) {
        self.flags.insert(SvFlags::READONLY);
    }
}

// ── Helpers ──────────────────────────────────────────────────────

/// Format a float the way Perl does.  Perl uses Gconvert which is
/// essentially `sprintf("%.15g", n)` — shortest representation that
/// round-trips.  Rust doesn't have %g, so we approximate:
/// use Display (which gives shortest round-trip representation),
/// falling back to LowerExp for very large/small values.
fn format_nv(n: f64) -> String {
    if n == 0.0 {
        return "0".to_string();
    }
    // Rust's Display for f64 gives shortest round-trip representation
    // which is close to %g behavior.
    format!("{}", n)
}

/// Perl string falseness: `""` (empty) and `"0"` are false.
/// Everything else is true.
fn string_is_false(pv: &PerlStringSlot) -> bool {
    match pv.as_bytes() {
        None => true,       // no string → false (shouldn't happen if POK is set)
        Some(b"") => true,  // empty string
        Some(b"0") => true, // the string "0"
        Some(_) => false,   // anything else
    }
}

// ── Trait impls ──────────────────────────────────────────────────

impl fmt::Debug for Scalar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Scalar");
        d.field("flags", &self.flags);
        if self.flags.contains(SvFlags::IOK) {
            d.field("int", &self.int);
        }
        if self.flags.contains(SvFlags::NOK) {
            d.field("num", &self.num);
        }
        if self.flags.contains(SvFlags::POK) {
            d.field("pv", &self.pv);
        }
        if self.flags.contains(SvFlags::ROK) {
            d.field("rv", &self.rv);
        }
        if self.magic.is_some() {
            d.field("magic", &"<attached>");
        }
        if self.stash.is_some() {
            d.field("stash", &"<blessed>");
        }
        d.finish()
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undef_scalar() {
        let sv = Scalar::new_undef();
        assert!(!sv.is_defined());
        assert!(sv.flags().is_empty());
    }

    #[test]
    fn from_int() {
        let mut sv = Scalar::from_int(42);
        assert!(sv.flags().contains(SvFlags::IOK));
        assert_eq!(sv.get_int(), 42);
        assert!(sv.is_defined());
    }

    #[test]
    fn from_num() {
        let mut sv = Scalar::from_num(3.14);
        assert!(sv.flags().contains(SvFlags::NOK));
        assert!((sv.get_num() - 3.14).abs() < 1e-10);
    }

    #[test]
    fn from_str() {
        let mut sv = Scalar::from_str("hello");
        assert!(sv.flags().contains(SvFlags::POK));
        assert!(sv.flags().contains(SvFlags::UTF8));
        assert_eq!(sv.get_str(), Some("hello"));
    }

    #[test]
    fn iv_to_nv_coercion() {
        let mut sv = Scalar::from_int(42);
        assert!(!sv.flags().contains(SvFlags::NOK));
        let n = sv.get_num();
        assert!((n - 42.0).abs() < 1e-10);
        assert!(sv.flags().contains(SvFlags::NOK)); // now cached
    }

    #[test]
    fn iv_to_str_coercion() {
        let mut sv = Scalar::from_int(42);
        assert!(!sv.flags().contains(SvFlags::POK));
        let s = sv.get_str();
        assert_eq!(s, Some("42"));
        assert!(sv.flags().contains(SvFlags::POK)); // now cached
        assert!(sv.flags().contains(SvFlags::IOK)); // still valid
    }

    #[test]
    fn str_to_iv_coercion() {
        let mut sv = Scalar::from_str("42abc");
        assert!(!sv.flags().contains(SvFlags::IOK));
        let n = sv.get_int();
        assert_eq!(n, 42); // Perl-style: parse leading digits
        assert!(sv.flags().contains(SvFlags::IOK)); // now cached
        assert!(sv.flags().contains(SvFlags::POK)); // still valid
    }

    #[test]
    fn set_int_clears_string() {
        let mut sv = Scalar::from_str("hello");
        assert!(sv.flags().contains(SvFlags::POK));
        sv.set_int(99);
        assert!(sv.flags().contains(SvFlags::IOK));
        assert!(!sv.flags().contains(SvFlags::POK));
    }

    #[test]
    fn set_str_clears_numeric() {
        let mut sv = Scalar::from_int(42);
        assert!(sv.flags().contains(SvFlags::IOK));
        sv.set_str("hello");
        assert!(sv.flags().contains(SvFlags::POK));
        assert!(!sv.flags().contains(SvFlags::IOK));
        assert!(!sv.flags().contains(SvFlags::NOK));
    }

    #[test]
    fn multi_rep_caching() {
        // Simulate: $x = "42"; $x + 0;
        // After the addition, both POK and IOK should be set.
        let mut sv = Scalar::from_str("42");
        assert!(sv.flags().contains(SvFlags::POK));
        assert!(!sv.flags().contains(SvFlags::IOK));

        // Reading as integer triggers coercion and caching
        let n = sv.get_int();
        assert_eq!(n, 42);
        assert!(sv.flags().contains(SvFlags::IOK)); // cached
        assert!(sv.flags().contains(SvFlags::POK)); // still valid
    }

    #[test]
    fn reference_scalar() {
        let target = Value::Int(42);
        let sv = Scalar::from_ref(target);
        assert!(sv.is_ref());
        assert!(sv.is_defined());
        match sv.get_rv() {
            Some(Value::Int(42)) => {}
            other => panic!("Expected Value::Int(42), got {:?}", other),
        }
    }

    #[test]
    fn readonly() {
        let mut sv = Scalar::from_int(42);
        assert!(!sv.is_readonly());
        sv.set_readonly();
        assert!(sv.is_readonly());
    }

    #[test]
    fn undef_coerces_to_zero() {
        let mut sv = Scalar::new_undef();
        assert_eq!(sv.get_int(), 0);
        assert!((sv.get_num()).abs() < 1e-10);
    }

    #[test]
    fn undef_coerces_to_no_string() {
        let mut sv = Scalar::new_undef();
        assert_eq!(sv.get_bytes(), None);
        assert_eq!(sv.get_str(), None);
    }

    // ── Truthiness tests ──────────────────────────────────────

    #[test]
    fn undef_is_false() {
        let sv = Scalar::new_undef();
        assert!(!sv.is_true());
    }

    #[test]
    fn zero_int_is_false() {
        let sv = Scalar::from_int(0);
        assert!(!sv.is_true());
    }

    #[test]
    fn nonzero_int_is_true() {
        let sv = Scalar::from_int(42);
        assert!(sv.is_true());

        let sv = Scalar::from_int(-1);
        assert!(sv.is_true());
    }

    #[test]
    fn zero_float_is_false() {
        let sv = Scalar::from_num(0.0);
        assert!(!sv.is_true());
    }

    #[test]
    fn nonzero_float_is_true() {
        let sv = Scalar::from_num(3.14);
        assert!(sv.is_true());

        let sv = Scalar::from_num(-0.001);
        assert!(sv.is_true());
    }

    #[test]
    fn nan_is_true() {
        // In Perl, NaN is true (it's not zero).
        let sv = Scalar::from_num(f64::NAN);
        assert!(sv.is_true());
    }

    #[test]
    fn empty_string_is_false() {
        let sv = Scalar::from_str("");
        assert!(!sv.is_true());
    }

    #[test]
    fn string_zero_is_false() {
        let sv = Scalar::from_str("0");
        assert!(!sv.is_true());
    }

    #[test]
    fn nonempty_string_is_true() {
        let sv = Scalar::from_str("hello");
        assert!(sv.is_true());

        // "00" is true — only exactly "0" is false
        let sv = Scalar::from_str("00");
        assert!(sv.is_true());

        // "0.0" is true — only exactly "0" is false
        let sv = Scalar::from_str("0.0");
        assert!(sv.is_true());

        // " " (space) is true
        let sv = Scalar::from_str(" ");
        assert!(sv.is_true());

        // "0E0" is true — Perl's "zero but true"
        let sv = Scalar::from_str("0E0");
        assert!(sv.is_true());
    }

    #[test]
    fn reference_is_true() {
        let sv = Scalar::from_ref(Value::Int(0));
        assert!(sv.is_true());

        // Even a reference to undef is true
        let sv = Scalar::from_ref(Value::Undef);
        assert!(sv.is_true());
    }
}
