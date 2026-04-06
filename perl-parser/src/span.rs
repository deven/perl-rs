//! Source location tracking.
//!
//! Every token and AST node carries a `Span` recording its byte range
//! in the source.  This supports diagnostics, source maps, and tooling.

/// A half-open byte range `[start, end)` in the source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    /// Byte offset of the first character.
    pub start: u32,
    /// Byte offset one past the last character.
    pub end: u32,
}

impl Span {
    pub const DUMMY: Span = Span { start: 0, end: 0 };

    pub fn new(start: u32, end: u32) -> Self {
        debug_assert!(start <= end);
        Span { start, end }
    }

    /// Length in bytes.
    pub fn len(&self) -> u32 {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Merge two spans into one covering both.
    pub fn merge(self, other: Span) -> Span {
        Span { start: self.start.min(other.start), end: self.end.max(other.end) }
    }
}
