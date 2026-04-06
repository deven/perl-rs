//! Scalar value flags — cache validity and metadata bits.
//!
//! These flags follow Perl 5's SV flag model:
//! - **Validity flags** (IOK, NOK, POK, ROK): which cached representations
//!   are current.
//! - **Metadata flags** (READONLY, UTF8, TAINT, MAGICAL, WEAK): orthogonal
//!   properties of the value.

/// Flags for a `Scalar` value.
///
/// Validity flags indicate which representation slots contain current data.
/// The coercion engine reads these to determine the fast path (e.g., IOK
/// set means `int` is valid — return it directly) and sets them when caching
/// a new representation (e.g., parsing a string as an integer sets IOK).
///
/// Metadata flags describe orthogonal properties that don't affect which
/// representation is current.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SvFlags(u16);

impl SvFlags {
    // ── Validity flags ────────────────────────────────────────────

    /// Integer value (`int`) is valid.
    pub const IOK: SvFlags = SvFlags(1 << 0);

    /// Numeric value (`num`) is valid.
    pub const NOK: SvFlags = SvFlags(1 << 1);

    /// String value (`pv`) is valid.
    pub const POK: SvFlags = SvFlags(1 << 2);

    /// Reference value (`rv`) is valid — this scalar IS a reference.
    pub const ROK: SvFlags = SvFlags(1 << 3);

    // ── Metadata flags ────────────────────────────────────────────

    /// Value is read-only (Internals::SvREADONLY).
    pub const READONLY: SvFlags = SvFlags(1 << 4);

    /// String value is valid UTF-8 (redundant with PerlStringSlot's
    /// own flag, but kept for fast checking without unpacking pv).
    pub const UTF8: SvFlags = SvFlags(1 << 5);

    /// Value is tainted (taint mode).
    pub const TAINT: SvFlags = SvFlags(1 << 6);

    /// Magic chain is attached to this scalar.
    pub const MAGICAL: SvFlags = SvFlags(1 << 7);

    /// This is a weak reference.
    pub const WEAK: SvFlags = SvFlags(1 << 8);

    // ── Compound masks ────────────────────────────────────────────

    /// Any numeric representation is valid.
    pub const ANY_NUM: SvFlags = SvFlags(Self::IOK.0 | Self::NOK.0);

    /// Any value representation is valid.
    pub const ANY_VAL: SvFlags = SvFlags(Self::IOK.0 | Self::NOK.0 | Self::POK.0);

    /// All validity flags.
    pub const ALL_VALIDITY: SvFlags = SvFlags(Self::IOK.0 | Self::NOK.0 | Self::POK.0 | Self::ROK.0);

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
        assert!(!f.contains(SvFlags::IOK));
    }

    #[test]
    fn set_and_check() {
        let mut f = SvFlags::EMPTY;
        f.insert(SvFlags::IOK);
        assert!(f.contains(SvFlags::IOK));
        assert!(!f.contains(SvFlags::NOK));
    }

    #[test]
    fn insert_multiple() {
        let mut f = SvFlags::EMPTY;
        f.insert(SvFlags::IOK);
        f.insert(SvFlags::POK);
        assert!(f.contains(SvFlags::IOK));
        assert!(f.contains(SvFlags::POK));
        assert!(!f.contains(SvFlags::NOK));
    }

    #[test]
    fn remove() {
        let mut f = SvFlags::IOK | SvFlags::POK;
        f.remove(SvFlags::IOK);
        assert!(!f.contains(SvFlags::IOK));
        assert!(f.contains(SvFlags::POK));
    }

    #[test]
    fn intersects() {
        let f = SvFlags::IOK | SvFlags::POK;
        assert!(f.intersects(SvFlags::IOK));
        assert!(f.intersects(SvFlags::ANY_NUM));
        assert!(!f.intersects(SvFlags::NOK));
    }

    #[test]
    fn contains_compound() {
        let f = SvFlags::IOK | SvFlags::NOK;
        assert!(f.contains(SvFlags::ANY_NUM));

        let g = SvFlags::IOK;
        assert!(!g.contains(SvFlags::ANY_NUM)); // missing NOK
    }

    #[test]
    fn clear_validity() {
        let mut f = SvFlags::IOK | SvFlags::POK | SvFlags::READONLY;
        f.remove(SvFlags::ALL_VALIDITY);
        assert!(!f.contains(SvFlags::IOK));
        assert!(!f.contains(SvFlags::POK));
        assert!(f.contains(SvFlags::READONLY)); // metadata preserved
    }

    #[test]
    fn bitor_syntax() {
        let f = SvFlags::IOK | SvFlags::NOK | SvFlags::READONLY;
        assert!(f.contains(SvFlags::IOK));
        assert!(f.contains(SvFlags::READONLY));
        assert!(!f.contains(SvFlags::POK));
    }
}
