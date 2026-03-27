//! Inline small string — up to 22 bytes with no heap allocation.
//!
//! Used for the compact `Value::SmallStr` variant.  Most hash keys,
//! identifiers, short literals, and numeric stringifications fit
//! within 22 bytes.

use std::fmt;

use crate::PerlString;

/// Maximum number of bytes in a `SmallString`.
pub const SMALL_STRING_MAX: usize = 22;

/// An inline Perl string that fits in 24 bytes total (22 data + len + flag).
///
/// Like [`PerlString`], this is an octet sequence with an optional UTF-8 flag,
/// but stored entirely on the stack with no heap allocation.
///
/// # Invariant
///
/// If `is_utf8` is `true`, then `buf[..len]` MUST contain valid UTF-8.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SmallString {
    buf: [u8; SMALL_STRING_MAX],
    len: u8,
    is_utf8: bool,
}

impl SmallString {
    // ── Constructors ──────────────────────────────────────────────

    /// Create an empty `SmallString`.
    pub fn new() -> Self {
        SmallString { buf: [0; SMALL_STRING_MAX], len: 0, is_utf8: false }
    }

    /// Create a `SmallString` from a `&str`.
    ///
    /// Returns `None` if the string exceeds [`SMALL_STRING_MAX`] bytes.
    pub fn from_str(s: &str) -> Option<Self> {
        Self::from_bytes_with_flag(s.as_bytes(), true)
    }

    /// Create a `SmallString` from raw bytes (UTF-8 flag NOT set).
    ///
    /// Returns `None` if the slice exceeds [`SMALL_STRING_MAX`] bytes.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        Self::from_bytes_with_flag(bytes, false)
    }

    /// Create a `SmallString` from bytes with an explicit UTF-8 flag.
    ///
    /// Returns `None` if the slice exceeds [`SMALL_STRING_MAX`] bytes.
    ///
    /// # Safety (logical)
    ///
    /// If `is_utf8` is `true`, the caller must ensure `bytes` is valid UTF-8.
    pub fn from_bytes_with_flag(bytes: &[u8], is_utf8: bool) -> Option<Self> {
        if bytes.len() > SMALL_STRING_MAX {
            return None;
        }
        let mut buf = [0u8; SMALL_STRING_MAX];
        buf[..bytes.len()].copy_from_slice(bytes);
        Some(SmallString { buf, len: bytes.len() as u8, is_utf8 })
    }

    // ── Accessors ─────────────────────────────────────────────────

    /// The string content as a byte slice.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    /// Zero-cost `&str` view when the UTF-8 flag is set.
    pub fn as_str(&self) -> Option<&str> {
        if self.is_utf8 {
            // SAFETY: invariant guarantees valid UTF-8 when flag is set.
            Some(unsafe { std::str::from_utf8_unchecked(self.as_bytes()) })
        } else {
            None
        }
    }

    /// Whether the UTF-8 flag is set.
    pub fn is_utf8(&self) -> bool {
        self.is_utf8
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether the string is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    // ── Conversion to heap-allocated PerlString ───────────────────

    /// Promote to a heap-allocated `PerlString`.
    /// This is a one-way operation — used when the string needs to
    /// grow beyond [`SMALL_STRING_MAX`] bytes or when a `PerlString`
    /// is required.
    pub fn to_perl_string(&self) -> PerlString {
        // SAFETY: if is_utf8, then as_bytes() is valid UTF-8.
        unsafe { PerlString::from_bytes_utf8_unchecked(self.as_bytes().to_vec(), self.is_utf8) }
    }

    // ── Numeric parsing (delegates to PerlString logic) ───────────

    /// Parse as i64 with Perl's numeric conversion rules.
    pub fn parse_iv(&self) -> i64 {
        // Re-use PerlString's parsing by creating a temporary.
        // This is a cold path — numeric conversion of a SmallString
        // will typically upgrade it to a full Scalar anyway.
        self.to_perl_string().parse_iv()
    }

    /// Parse as f64 with Perl's numeric conversion rules.
    pub fn parse_nv(&self) -> f64 {
        self.to_perl_string().parse_nv()
    }
}

// ── Trait impls ───────────────────────────────────────────────────

impl Default for SmallString {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SmallString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(s) = self.as_str() { write!(f, "SmallString({:?}, utf8)", s) } else { write!(f, "SmallString({:?}, bytes)", self.as_bytes()) }
    }
}

impl fmt::Display for SmallString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(s) = self.as_str() { f.write_str(s) } else { f.write_str(&String::from_utf8_lossy(self.as_bytes())) }
    }
}

/// Try to convert a `&str` into a `SmallString`.
/// Fails if the string is longer than [`SMALL_STRING_MAX`] bytes.
impl TryFrom<&str> for SmallString {
    type Error = ();

    fn try_from(s: &str) -> Result<Self, ()> {
        SmallString::from_str(s).ok_or(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_short_str() {
        let s = SmallString::from_str("hello").unwrap();
        assert!(s.is_utf8());
        assert_eq!(s.as_str(), Some("hello"));
        assert_eq!(s.len(), 5);
    }

    #[test]
    fn from_max_length_str() {
        let s = "abcdefghijklmnopqrstuv"; // exactly 22 bytes
        assert_eq!(s.len(), SMALL_STRING_MAX);
        let ss = SmallString::from_str(s).unwrap();
        assert_eq!(ss.as_str(), Some(s));
        assert_eq!(ss.len(), SMALL_STRING_MAX);
    }

    #[test]
    fn from_too_long_str() {
        let s = "abcdefghijklmnopqrstuvw"; // 23 bytes
        assert!(SmallString::from_str(s).is_none());
    }

    #[test]
    fn from_bytes() {
        let s = SmallString::from_bytes(&[0xff, 0xfe]).unwrap();
        assert!(!s.is_utf8());
        assert_eq!(s.as_bytes(), &[0xff, 0xfe]);
        assert_eq!(s.as_str(), None);
    }

    #[test]
    fn empty() {
        let s = SmallString::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.as_bytes(), &[]);
    }

    #[test]
    fn to_perl_string_preserves_flag() {
        let ss = SmallString::from_str("hello").unwrap();
        let ps = ss.to_perl_string();
        assert!(ps.is_utf8());
        assert_eq!(ps.as_str(), Some("hello"));

        let ss = SmallString::from_bytes(&[0xff]).unwrap();
        let ps = ss.to_perl_string();
        assert!(!ps.is_utf8());
    }

    #[test]
    fn copy_semantics() {
        let a = SmallString::from_str("hello").unwrap();
        let b = a; // Copy, not move
        assert_eq!(a.as_str(), Some("hello"));
        assert_eq!(b.as_str(), Some("hello"));
    }

    #[test]
    fn numeric_parsing() {
        let s = SmallString::from_str("42").unwrap();
        assert_eq!(s.parse_iv(), 42);
        assert!((s.parse_nv() - 42.0).abs() < 1e-10);
    }

    #[test]
    fn display() {
        let s = SmallString::from_str("hello").unwrap();
        assert_eq!(format!("{}", s), "hello");
    }
}
