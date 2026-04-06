//! Heap-allocated Perl string — `Vec<u8>` + UTF-8 flag.

use std::fmt;

/// A Perl string: an octet sequence with an optional UTF-8 flag.
///
/// Unlike Rust's `String`, a `PerlString` can hold arbitrary bytes.
/// The `is_utf8` flag indicates whether the bytes are valid UTF-8,
/// enabling zero-cost conversion to `&str` when set.
///
/// # Invariant
///
/// If `is_utf8` is `true`, then `buf` MUST contain valid UTF-8.
/// All mutating methods are responsible for maintaining this invariant
/// (typically by clearing the flag when the result might not be UTF-8).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PerlString {
    buf: Vec<u8>,
    is_utf8: bool,
}

impl PerlString {
    // ── Constructors ──────────────────────────────────────────────

    /// Create an empty Perl string (no UTF-8 flag).
    pub fn new() -> Self {
        PerlString { buf: Vec::new(), is_utf8: false }
    }

    /// Create a Perl string from a Rust `&str`.  The UTF-8 flag is set.
    pub fn from_str(s: &str) -> Self {
        PerlString { buf: s.as_bytes().to_vec(), is_utf8: true }
    }

    /// Create a Perl string from raw bytes.  The UTF-8 flag is NOT set.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        PerlString { buf: bytes, is_utf8: false }
    }

    /// Create a Perl string from raw bytes with an explicit UTF-8 flag.
    ///
    /// # Safety
    ///
    /// If `is_utf8` is `true`, the caller MUST ensure `bytes` is valid UTF-8.
    pub unsafe fn from_bytes_utf8_unchecked(bytes: Vec<u8>, is_utf8: bool) -> Self {
        PerlString { buf: bytes, is_utf8 }
    }

    /// Create a Perl string from raw bytes, checking UTF-8 validity.
    /// Sets the flag if the bytes are valid UTF-8.
    pub fn from_bytes_detect_utf8(bytes: Vec<u8>) -> Self {
        let is_utf8 = std::str::from_utf8(&bytes).is_ok();
        PerlString { buf: bytes, is_utf8 }
    }

    // ── Accessors ─────────────────────────────────────────────────

    /// Zero-cost `&str` view when the UTF-8 flag is set.
    /// Returns `None` if the string is not flagged as UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        if self.is_utf8 {
            // SAFETY: we maintain the invariant that is_utf8 == true
            // means self.buf contains valid UTF-8.
            Some(unsafe { std::str::from_utf8_unchecked(&self.buf) })
        } else {
            None
        }
    }

    /// Byte slice view — always available regardless of UTF-8 flag.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Whether the UTF-8 flag is set.
    pub fn is_utf8(&self) -> bool {
        self.is_utf8
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the string is empty (zero bytes).
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    // ── Mutation ──────────────────────────────────────────────────

    /// Append bytes from a `&str`.  Preserves UTF-8 flag if already set
    /// (appending valid UTF-8 to valid UTF-8 is valid UTF-8).
    pub fn push_str(&mut self, s: &str) {
        self.buf.extend_from_slice(s.as_bytes());
        // If we were already UTF-8, appending a &str keeps us UTF-8.
        // If we weren't, appending UTF-8 doesn't make us UTF-8.
    }

    /// Append raw bytes.  Clears the UTF-8 flag (we can't guarantee
    /// the result is valid UTF-8).
    pub fn push_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        self.is_utf8 = false;
    }

    /// Append another PerlString.  UTF-8 flag is set only if both are UTF-8.
    pub fn push_perl_string(&mut self, other: &PerlString) {
        self.buf.extend_from_slice(&other.buf);
        self.is_utf8 = self.is_utf8 && other.is_utf8;
    }

    /// Clear the string contents but keep the allocated buffer.
    pub fn clear(&mut self) {
        self.buf.clear();
        // An empty string is trivially valid UTF-8, but we preserve
        // the flag state for consistency with Perl 5 behavior.
    }

    /// Truncate to `len` bytes.  Clears UTF-8 flag if truncation might
    /// split a multi-byte character.
    pub fn truncate(&mut self, len: usize) {
        if len < self.buf.len() {
            self.buf.truncate(len);
            if self.is_utf8 {
                // Check if we split a multi-byte sequence.
                if std::str::from_utf8(&self.buf).is_err() {
                    self.is_utf8 = false;
                }
            }
        }
    }

    /// Set the UTF-8 flag, validating the contents first.
    /// Returns `true` if the contents are valid UTF-8 (flag set),
    /// `false` if not (flag unchanged).
    pub fn upgrade_to_utf8(&mut self) -> bool {
        if self.is_utf8 {
            return true;
        }
        if std::str::from_utf8(&self.buf).is_ok() {
            self.is_utf8 = true;
            true
        } else {
            false
        }
    }

    /// Clear the UTF-8 flag (downgrade to raw bytes).
    pub fn downgrade_from_utf8(&mut self) {
        self.is_utf8 = false;
    }

    // ── Conversion ────────────────────────────────────────────────

    /// Consume the PerlString and return the underlying byte vector.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// Consume the PerlString and attempt to convert to a Rust `String`.
    /// Returns `Err(self)` if the contents are not valid UTF-8.
    pub fn into_string(self) -> Result<String, Self> {
        if self.is_utf8 {
            // SAFETY: is_utf8 flag guarantees valid UTF-8.
            Ok(unsafe { String::from_utf8_unchecked(self.buf) })
        } else {
            match String::from_utf8(self.buf) {
                Ok(s) => Ok(s),
                Err(e) => Err(PerlString { buf: e.into_bytes(), is_utf8: false }),
            }
        }
    }

    // ── Numeric parsing (for SV coercion) ─────────────────────────

    /// Attempt to parse the string as an i64.
    /// Follows Perl's numeric conversion rules: leading whitespace is
    /// skipped, trailing non-numeric characters are ignored, and an
    /// empty or non-numeric string yields 0.
    pub fn parse_iv(&self) -> i64 {
        let s = self.trimmed_bytes();
        if s.is_empty() {
            return 0;
        }
        // Fast path: try the whole trimmed string
        if let Some(s) = std::str::from_utf8(s).ok() {
            if let Ok(n) = s.parse::<i64>() {
                return n;
            }
            // Perl-style: parse as much as possible from the front
            return perl_atoi(s);
        }
        0
    }

    /// Attempt to parse the string as an f64.
    /// Same leading-whitespace / trailing-garbage rules as `parse_iv`.
    pub fn parse_nv(&self) -> f64 {
        let s = self.trimmed_bytes();
        if s.is_empty() {
            return 0.0;
        }
        if let Some(s) = std::str::from_utf8(s).ok() {
            if let Ok(n) = s.parse::<f64>() {
                return n;
            }
            return perl_atof(s);
        }
        0.0
    }

    /// Return a byte slice with leading ASCII whitespace removed.
    fn trimmed_bytes(&self) -> &[u8] {
        let mut start = 0;
        while start < self.buf.len() && self.buf[start].is_ascii_whitespace() {
            start += 1;
        }
        &self.buf[start..]
    }
}

// ── Perl-style numeric parsing helpers ────────────────────────────

/// Parse as much of the leading portion of `s` as a valid integer.
/// Returns 0 if no leading digits.
fn perl_atoi(s: &str) -> i64 {
    let s = s.trim_start();
    if s.is_empty() {
        return 0;
    }

    let (negative, s) = if s.starts_with('-') {
        (true, &s[1..])
    } else if s.starts_with('+') {
        (false, &s[1..])
    } else {
        (false, s)
    };

    // Hex
    if s.starts_with("0x") || s.starts_with("0X") {
        let hex = &s[2..];
        let end = hex.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(hex.len());
        if end == 0 {
            return 0;
        }
        let val = i64::from_str_radix(&hex[..end], 16).unwrap_or(0);
        return if negative { -val } else { val };
    }

    // Octal
    if s.starts_with("0b") || s.starts_with("0B") {
        let bin = &s[2..];
        let end = bin.find(|c: char| c != '0' && c != '1').unwrap_or(bin.len());
        if end == 0 {
            return 0;
        }
        let val = i64::from_str_radix(&bin[..end], 2).unwrap_or(0);
        return if negative { -val } else { val };
    }

    // Decimal (with leading 0 still decimal in numeric context, unlike Perl's
    // oct() function — Perl's implicit numeric conversion treats "010" as 10)
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        return 0;
    }
    let val = s[..end].parse::<i64>().unwrap_or(0);
    if negative { -val } else { val }
}

/// Parse as much of the leading portion of `s` as a valid float.
/// Returns 0.0 if no leading numeric content.
fn perl_atof(s: &str) -> f64 {
    let s = s.trim_start();
    if s.is_empty() {
        return 0.0;
    }

    // Find the longest prefix that looks like a float
    let mut end = 0;
    let bytes = s.as_bytes();

    // Optional sign
    if end < bytes.len() && (bytes[end] == b'+' || bytes[end] == b'-') {
        end += 1;
    }

    // Digits before decimal point
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }

    // Decimal point + digits after
    if end < bytes.len() && bytes[end] == b'.' {
        end += 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
    }

    // Exponent
    if end < bytes.len() && (bytes[end] == b'e' || bytes[end] == b'E') {
        end += 1;
        if end < bytes.len() && (bytes[end] == b'+' || bytes[end] == b'-') {
            end += 1;
        }
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
    }

    if end == 0 {
        return 0.0;
    }

    s[..end].parse::<f64>().unwrap_or(0.0)
}

// ── Trait impls ───────────────────────────────────────────────────

impl Default for PerlString {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for PerlString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(s) = self.as_str() { write!(f, "PerlString({:?}, utf8)", s) } else { write!(f, "PerlString({:?}, bytes)", self.buf) }
    }
}

impl fmt::Display for PerlString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(s) = self.as_str() {
            f.write_str(s)
        } else {
            // Best-effort display for non-UTF-8 strings
            f.write_str(&String::from_utf8_lossy(&self.buf))
        }
    }
}

impl From<&str> for PerlString {
    fn from(s: &str) -> Self {
        PerlString::from_str(s)
    }
}

impl From<String> for PerlString {
    fn from(s: String) -> Self {
        PerlString { buf: s.into_bytes(), is_utf8: true }
    }
}

impl From<Vec<u8>> for PerlString {
    fn from(bytes: Vec<u8>) -> Self {
        PerlString::from_bytes(bytes)
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_sets_utf8_flag() {
        let s = PerlString::from_str("hello");
        assert!(s.is_utf8());
        assert_eq!(s.as_str(), Some("hello"));
        assert_eq!(s.as_bytes(), b"hello");
        assert_eq!(s.len(), 5);
    }

    #[test]
    fn from_bytes_clears_utf8_flag() {
        let s = PerlString::from_bytes(vec![0xff, 0xfe]);
        assert!(!s.is_utf8());
        assert_eq!(s.as_str(), None);
        assert_eq!(s.as_bytes(), &[0xff, 0xfe]);
    }

    #[test]
    fn from_bytes_detect_utf8() {
        let valid = PerlString::from_bytes_detect_utf8(b"hello".to_vec());
        assert!(valid.is_utf8());

        let invalid = PerlString::from_bytes_detect_utf8(vec![0xff, 0xfe]);
        assert!(!invalid.is_utf8());
    }

    #[test]
    fn push_str_preserves_utf8() {
        let mut s = PerlString::from_str("hello");
        s.push_str(", world");
        assert!(s.is_utf8());
        assert_eq!(s.as_str(), Some("hello, world"));
    }

    #[test]
    fn push_bytes_clears_utf8() {
        let mut s = PerlString::from_str("hello");
        s.push_bytes(&[0xff]);
        assert!(!s.is_utf8());
        assert_eq!(s.as_str(), None);
    }

    #[test]
    fn push_perl_string_utf8_both() {
        let mut a = PerlString::from_str("hello ");
        let b = PerlString::from_str("world");
        a.push_perl_string(&b);
        assert!(a.is_utf8());
        assert_eq!(a.as_str(), Some("hello world"));
    }

    #[test]
    fn push_perl_string_mixed_clears_utf8() {
        let mut a = PerlString::from_str("hello ");
        let b = PerlString::from_bytes(vec![0xff]);
        a.push_perl_string(&b);
        assert!(!a.is_utf8());
    }

    #[test]
    fn truncate_safe() {
        let mut s = PerlString::from_str("hello");
        s.truncate(3);
        assert_eq!(s.as_str(), Some("hel"));
        assert!(s.is_utf8());
    }

    #[test]
    fn truncate_splits_multibyte() {
        let mut s = PerlString::from_str("héllo"); // é is 2 bytes
        // "héllo" = [104, 195, 169, 108, 108, 111]
        s.truncate(2); // splits the é
        assert!(!s.is_utf8()); // flag cleared
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn upgrade_to_utf8() {
        let mut s = PerlString::from_bytes(b"hello".to_vec());
        assert!(!s.is_utf8());
        assert!(s.upgrade_to_utf8());
        assert!(s.is_utf8());
        assert_eq!(s.as_str(), Some("hello"));
    }

    #[test]
    fn upgrade_to_utf8_fails_for_invalid() {
        let mut s = PerlString::from_bytes(vec![0xff, 0xfe]);
        assert!(!s.upgrade_to_utf8());
        assert!(!s.is_utf8());
    }

    #[test]
    fn parse_iv_basic() {
        assert_eq!(PerlString::from_str("42").parse_iv(), 42);
        assert_eq!(PerlString::from_str("-7").parse_iv(), -7);
        assert_eq!(PerlString::from_str("  123  ").parse_iv(), 123);
        assert_eq!(PerlString::from_str("42abc").parse_iv(), 42);
        assert_eq!(PerlString::from_str("abc").parse_iv(), 0);
        assert_eq!(PerlString::from_str("").parse_iv(), 0);
    }

    #[test]
    fn parse_iv_hex_and_binary() {
        assert_eq!(PerlString::from_str("0xff").parse_iv(), 255);
        assert_eq!(PerlString::from_str("0b1010").parse_iv(), 10);
        assert_eq!(PerlString::from_str("-0xff").parse_iv(), -255);
    }

    #[test]
    fn parse_nv_basic() {
        assert!((PerlString::from_str("3.14").parse_nv() - 3.14).abs() < 1e-10);
        assert!((PerlString::from_str("-2.5").parse_nv() - (-2.5)).abs() < 1e-10);
        assert!((PerlString::from_str("1e3").parse_nv() - 1000.0).abs() < 1e-10);
        assert!((PerlString::from_str("  42.5abc").parse_nv() - 42.5).abs() < 1e-10);
        assert_eq!(PerlString::from_str("abc").parse_nv(), 0.0);
        assert_eq!(PerlString::from_str("").parse_nv(), 0.0);
    }

    #[test]
    fn into_string_utf8() {
        let s = PerlString::from_str("hello");
        assert_eq!(s.into_string(), Ok(String::from("hello")));
    }

    #[test]
    fn into_string_non_utf8_valid() {
        let s = PerlString::from_bytes(b"hello".to_vec());
        // Not flagged as UTF-8, but contents happen to be valid
        assert_eq!(s.into_string(), Ok(String::from("hello")));
    }

    #[test]
    fn into_string_non_utf8_invalid() {
        let s = PerlString::from_bytes(vec![0xff, 0xfe]);
        assert!(s.into_string().is_err());
    }

    #[test]
    fn display_utf8() {
        let s = PerlString::from_str("hello");
        assert_eq!(format!("{}", s), "hello");
    }

    #[test]
    fn display_non_utf8() {
        let s = PerlString::from_bytes(vec![0xff, 0xfe]);
        // Should not panic — uses lossy conversion
        let _ = format!("{}", s);
    }

    #[test]
    fn empty_string() {
        let s = PerlString::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.parse_iv(), 0);
        assert_eq!(s.parse_nv(), 0.0);
    }

    #[test]
    fn equality() {
        let a = PerlString::from_str("hello");
        let b = PerlString::from_str("hello");
        let c = PerlString::from_bytes(b"hello".to_vec());
        assert_eq!(a, b);
        // Different UTF-8 flag, same bytes — should they be equal?
        // In Perl, "hello" eq "hello" regardless of internal flags.
        // Our PartialEq derives from the struct, so flag matters.
        // This is intentional — internal representation equality,
        // not Perl-level equality (which is handled by the runtime).
        assert_ne!(a, c);
    }
}
