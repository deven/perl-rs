//! String cache slot for the `Scalar` struct.
//!
//! This enum provides small-string optimization within the `bytes` field
//! of a full `Scalar`.  Even after a value has been upgraded from a
//! compact `Value` variant to a full `Scalar` (behind `Arc<RwLock<>>`),
//! short strings in the cache avoid an additional heap allocation.

use std::fmt;

use crate::PerlString;

/// Maximum bytes for an inline string within a `PerlStringSlot`.
/// Sized to match `PerlString` so the enum doesn't grow.
pub const SLOT_INLINE_MAX: usize = 24;

/// The string cache inside a `Scalar`.
///
/// - `None` — no string representation cached (STR_VALID is clear).
/// - `Inline` — short string stored directly in the Scalar struct.
/// - `Heap` — longer string on the heap via `PerlString`.
///
/// Note that `None` means "no string cache", not "empty string".
/// An empty string is `Inline { len: 0, .. }` with STR_VALID set.
#[derive(Clone, PartialEq, Eq)]
pub enum PerlStringSlot {
    /// No string representation cached.
    None,

    /// Inline short string (up to [`SLOT_INLINE_MAX`] bytes).
    Inline { buf: [u8; SLOT_INLINE_MAX], len: u8, is_utf8: bool },

    /// Heap-allocated string.
    Heap(PerlString),
}

impl PerlStringSlot {
    // ── Constructors ──────────────────────────────────────────────

    /// Set the cache from a `&str`.  Uses `Inline` if it fits.
    pub fn set_str(&mut self, s: &str) {
        if s.len() <= SLOT_INLINE_MAX {
            let mut buf = [0u8; SLOT_INLINE_MAX];
            buf[..s.len()].copy_from_slice(s.as_bytes());
            *self = PerlStringSlot::Inline { buf, len: s.len() as u8, is_utf8: true };
        } else {
            *self = PerlStringSlot::Heap(PerlString::from_str(s));
        }
    }

    /// Set the cache from raw bytes.  Uses `Inline` if it fits.
    pub fn set_bytes(&mut self, bytes: &[u8]) {
        if bytes.len() <= SLOT_INLINE_MAX {
            let mut buf = [0u8; SLOT_INLINE_MAX];
            buf[..bytes.len()].copy_from_slice(bytes);
            *self = PerlStringSlot::Inline { buf, len: bytes.len() as u8, is_utf8: false };
        } else {
            *self = PerlStringSlot::Heap(PerlString::from_bytes(bytes.to_vec()));
        }
    }

    /// Set the cache from a `PerlString`.  Does NOT demote to Inline.
    pub fn set_perl_string(&mut self, ps: PerlString) {
        *self = PerlStringSlot::Heap(ps);
    }

    /// Clear the cache (set to None).
    pub fn clear(&mut self) {
        *self = PerlStringSlot::None;
    }

    // ── Accessors ─────────────────────────────────────────────────

    /// Whether a string is cached.
    pub fn is_some(&self) -> bool {
        !matches!(self, PerlStringSlot::None)
    }

    /// Whether the cache is empty.
    pub fn is_none(&self) -> bool {
        matches!(self, PerlStringSlot::None)
    }

    /// Byte slice view of the cached string.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            PerlStringSlot::None => None,
            PerlStringSlot::Inline { buf, len, .. } => Some(&buf[..*len as usize]),
            PerlStringSlot::Heap(ps) => Some(ps.as_bytes()),
        }
    }

    /// `&str` view if the cached string is flagged as UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            PerlStringSlot::None => None,
            PerlStringSlot::Inline { buf, len, is_utf8: true } => {
                // SAFETY: is_utf8 flag guarantees valid UTF-8.
                Some(unsafe { std::str::from_utf8_unchecked(&buf[..*len as usize]) })
            }
            PerlStringSlot::Inline { is_utf8: false, .. } => None,
            PerlStringSlot::Heap(ps) => ps.as_str(),
        }
    }

    /// Whether the cached string (if any) has the UTF-8 flag set.
    pub fn is_utf8(&self) -> bool {
        match self {
            PerlStringSlot::None => false,
            PerlStringSlot::Inline { is_utf8, .. } => *is_utf8,
            PerlStringSlot::Heap(ps) => ps.is_utf8(),
        }
    }

    /// Length in bytes of the cached string, or 0 if no cache.
    pub fn len(&self) -> usize {
        match self {
            PerlStringSlot::None => 0,
            PerlStringSlot::Inline { len, .. } => *len as usize,
            PerlStringSlot::Heap(ps) => ps.len(),
        }
    }

    /// Convert the cached value to a `PerlString`.
    /// Returns `None` if no string is cached.
    pub fn to_perl_string(&self) -> Option<PerlString> {
        match self {
            PerlStringSlot::None => None,
            PerlStringSlot::Inline { buf, len, is_utf8 } => {
                let bytes = buf[..*len as usize].to_vec();
                // SAFETY: if is_utf8, the bytes are valid UTF-8.
                Some(unsafe { PerlString::from_bytes_utf8_unchecked(bytes, *is_utf8) })
            }
            PerlStringSlot::Heap(ps) => Some(ps.clone()),
        }
    }
}

// ── Trait impls ───────────────────────────────────────────────────

impl Default for PerlStringSlot {
    fn default() -> Self {
        PerlStringSlot::None
    }
}

impl fmt::Debug for PerlStringSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PerlStringSlot::None => write!(f, "PerlStringSlot::None"),
            PerlStringSlot::Inline { buf, len, is_utf8 } => {
                let bytes = &buf[..*len as usize];
                if *is_utf8 {
                    write!(f, "PerlStringSlot::Inline({:?}, utf8)", std::str::from_utf8(bytes).unwrap_or("<invalid>"))
                } else {
                    write!(f, "PerlStringSlot::Inline({:?}, bytes)", bytes)
                }
            }
            PerlStringSlot::Heap(ps) => write!(f, "PerlStringSlot::Heap({:?})", ps),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_none() {
        let slot = PerlStringSlot::default();
        assert!(slot.is_none());
        assert!(!slot.is_some());
        assert_eq!(slot.as_bytes(), None);
        assert_eq!(slot.as_str(), None);
    }

    #[test]
    fn set_short_str_uses_inline() {
        let mut slot = PerlStringSlot::None;
        slot.set_str("hello");
        assert!(matches!(slot, PerlStringSlot::Inline { .. }));
        assert_eq!(slot.as_str(), Some("hello"));
        assert_eq!(slot.as_bytes(), Some(b"hello".as_slice()));
        assert!(slot.is_utf8());
    }

    #[test]
    fn set_long_str_uses_heap() {
        let mut slot = PerlStringSlot::None;
        let long = "a".repeat(SLOT_INLINE_MAX + 1);
        slot.set_str(&long);
        assert!(matches!(slot, PerlStringSlot::Heap(_)));
        assert_eq!(slot.as_str(), Some(long.as_str()));
    }

    #[test]
    fn set_max_inline() {
        let mut slot = PerlStringSlot::None;
        let s = "a".repeat(SLOT_INLINE_MAX);
        slot.set_str(&s);
        assert!(matches!(slot, PerlStringSlot::Inline { .. }));
        assert_eq!(slot.len(), SLOT_INLINE_MAX);
    }

    #[test]
    fn set_bytes_no_utf8() {
        let mut slot = PerlStringSlot::None;
        slot.set_bytes(&[0xff, 0xfe]);
        assert!(slot.is_some());
        assert!(!slot.is_utf8());
        assert_eq!(slot.as_str(), None);
        assert_eq!(slot.as_bytes(), Some(&[0xff, 0xfe][..]));
    }

    #[test]
    fn clear_resets_to_none() {
        let mut slot = PerlStringSlot::None;
        slot.set_str("hello");
        assert!(slot.is_some());
        slot.clear();
        assert!(slot.is_none());
    }

    #[test]
    fn to_perl_string_inline() {
        let mut slot = PerlStringSlot::None;
        slot.set_str("hello");
        let ps = slot.to_perl_string().unwrap();
        assert!(ps.is_utf8());
        assert_eq!(ps.as_str(), Some("hello"));
    }

    #[test]
    fn to_perl_string_heap() {
        let mut slot = PerlStringSlot::None;
        let long = "a".repeat(SLOT_INLINE_MAX + 1);
        slot.set_str(&long);
        let ps = slot.to_perl_string().unwrap();
        assert_eq!(ps.as_str(), Some(long.as_str()));
    }

    #[test]
    fn to_perl_string_none() {
        let slot = PerlStringSlot::None;
        assert!(slot.to_perl_string().is_none());
    }
}
