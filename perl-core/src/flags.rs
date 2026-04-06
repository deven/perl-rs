//! Scalar value flags — cache validity and metadata bits.
//!
//! These flags follow Perl 5's SV flag model:
//! - **Validity flags** (INT_VALID, NUM_VALID, STR_VALID, REF_VALID): which
//!   cached representations are current.
//! - **Metadata flags** (READONLY, UTF8, TAINT, MAGICAL, WEAK): orthogonal
//!   properties of the value.

/// Flags for a `Scalar` value.
///
/// Validity flags indicate which representation slots contain current data.
/// The coercion engine reads these to determine the fast path (e.g., INT_VALID
/// set means the integer representation is valid — return it directly) and
/// sets them when caching a new representation (e.g., parsing a string as an
/// integer sets INT_VALID).
///
/// Metadata flags describe orthogonal properties that don't affect which
/// representation is current.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SvFlags(u16);

impl SvFlags {
    // ── Validity flags ────────────────────────────────────────────

    /// Integer representation is valid.
    pub const INT_VALID: SvFlags = SvFlags(1 << 0);

    /// Numeric representation is valid.
    pub const NUM_VALID: SvFlags = SvFlags(1 << 1);

    /// String representation is valid.
    pub const STR_VALID: SvFlags = SvFlags(1 << 2);

    /// Reference is valid — this scalar IS a reference.
    pub const REF_VALID: SvFlags = SvFlags(1 << 3);

    // ── Metadata flags ────────────────────────────────────────────

    /// Value is read-only (Internals::SvREADONLY).
    pub const READONLY: SvFlags = SvFlags(1 << 4);

    /// String value is valid UTF-8 (redundant with PerlStringSlot's
    /// own flag, but kept for fast checking without unpacking the string slot).
    pub const UTF8: SvFlags = SvFlags(1 << 5);

    /// Value is tainted (taint mode).
    pub const TAINT: SvFlags = SvFlags(1 << 6);

    /// Magic chain is attached to this scalar.
    pub const MAGICAL: SvFlags = SvFlags(1 << 7);

    /// This is a weak reference.
    pub const WEAK: SvFlags = SvFlags(1 << 8);

    // ── Compound masks ────────────────────────────────────────────

    /// Any numeric representation is valid.
    pub const ANY_NUM: SvFlags = SvFlags(Self::INT_VALID.0 | Self::NUM_VALID.0);

    /// Any value representation is valid.
    pub const ANY_VAL: SvFlags = SvFlags(Self::INT_VALID.0 | Self::NUM_VALID.0 | Self::STR_VALID.0);

    /// All validity flags.
    pub const ALL_VALID: SvFlags = SvFlags(Self::INT_VALID.0 | Self::NUM_VALID.0 | Self::STR_VALID.0 | Self::REF_VALID.0);

    // ── Empty ─────────────────────────────────────────────────────

    /// No flags set.
    pub const EMPTY: SvFlags = SvFlags(0);

    // ── Operations ────────────────────────────────────────────────

    /// Test whether all bits in `other` are set in `self`.
    #[inline]
    pub const fn contains(self, other: SvFlags) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Test whether any bits in `other` are set in `self`.
    #[inline]
    pub const fn intersects(self, other: SvFlags) -> bool {
        (self.0 & other.0) != 0
    }

    /// Set all bits in `other`.
    #[inline]
    pub fn insert(&mut self, other: SvFlags) {
        self.0 |= other.0;
    }

    /// Clear all bits in `other`.
    #[inline]
    pub fn remove(&mut self, other: SvFlags) {
        self.0 &= !other.0;
    }

    /// Return `self` with all bits in `other` set.
    #[inline]
    pub const fn union(self, other: SvFlags) -> SvFlags {
        SvFlags(self.0 | other.0)
    }

    /// Return `self` with all bits in `other` cleared.
    #[inline]
    pub const fn difference(self, other: SvFlags) -> SvFlags {
        SvFlags(self.0 & !other.0)
    }

    /// Whether no flags are set.
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The raw bits.
    #[inline]
    pub const fn bits(self) -> u16 {
        self.0
    }
}

// Bitwise operators for ergonomic flag combining.

impl std::ops::BitOr for SvFlags {
    type Output = SvFlags;
    #[inline]
    fn bitor(self, rhs: SvFlags) -> SvFlags {
        SvFlags(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for SvFlags {
    #[inline]
    fn bitor_assign(&mut self, rhs: SvFlags) {
        self.0 |= rhs.0;
    }
}

impl std::ops::BitAnd for SvFlags {
    type Output = SvFlags;
    #[inline]
    fn bitand(self, rhs: SvFlags) -> SvFlags {
        SvFlags(self.0 & rhs.0)
    }
}

impl std::ops::Not for SvFlags {
    type Output = SvFlags;
    #[inline]
    fn not(self) -> SvFlags {
        SvFlags(!self.0)
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty() {
        let f = SvFlags::default();
        assert!(f.is_empty());
        assert!(!f.contains(SvFlags::INT_VALID));
    }

    #[test]
    fn set_and_check() {
        let mut f = SvFlags::EMPTY;
        f.insert(SvFlags::INT_VALID);
        assert!(f.contains(SvFlags::INT_VALID));
        assert!(!f.contains(SvFlags::NUM_VALID));
    }

    #[test]
    fn insert_multiple() {
        let mut f = SvFlags::EMPTY;
        f.insert(SvFlags::INT_VALID);
        f.insert(SvFlags::STR_VALID);
        assert!(f.contains(SvFlags::INT_VALID));
        assert!(f.contains(SvFlags::STR_VALID));
        assert!(!f.contains(SvFlags::NUM_VALID));
    }

    #[test]
    fn remove() {
        let mut f = SvFlags::INT_VALID | SvFlags::STR_VALID;
        f.remove(SvFlags::INT_VALID);
        assert!(!f.contains(SvFlags::INT_VALID));
        assert!(f.contains(SvFlags::STR_VALID));
    }

    #[test]
    fn intersects() {
        let f = SvFlags::INT_VALID | SvFlags::STR_VALID;
        assert!(f.intersects(SvFlags::INT_VALID));
        assert!(f.intersects(SvFlags::ANY_NUM));
        assert!(!f.intersects(SvFlags::NUM_VALID));
    }

    #[test]
    fn contains_compound() {
        let f = SvFlags::INT_VALID | SvFlags::NUM_VALID;
        assert!(f.contains(SvFlags::ANY_NUM));

        let g = SvFlags::INT_VALID;
        assert!(!g.contains(SvFlags::ANY_NUM)); // missing NUM_VALID
    }

    #[test]
    fn clear_validity() {
        let mut f = SvFlags::INT_VALID | SvFlags::STR_VALID | SvFlags::READONLY;
        f.remove(SvFlags::ALL_VALID);
        assert!(!f.contains(SvFlags::INT_VALID));
        assert!(!f.contains(SvFlags::STR_VALID));
        assert!(f.contains(SvFlags::READONLY)); // metadata preserved
    }

    #[test]
    fn bitor_syntax() {
        let f = SvFlags::INT_VALID | SvFlags::NUM_VALID | SvFlags::READONLY;
        assert!(f.contains(SvFlags::INT_VALID));
        assert!(f.contains(SvFlags::READONLY));
        assert!(!f.contains(SvFlags::STR_VALID));
    }
}
