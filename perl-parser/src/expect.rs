//! Lexer expectation state.
//!
//! The `Expect` enum tells the lexer how to resolve context-sensitive
//! tokens: `/` (regex vs division), `{` (block vs hash), and `(`
//! (prototype vs grouping).  The parser sets this before each peek
//! to communicate syntactic context to the lexer.
//!
//! Maps to Perl 5's `PL_expect` states.  `XTERMORDORDOR` is folded
//! into `Term` (we always lex `//` as defined-or), and the five block
//! variants are collapsed into `Block(ExpectNext)`, where `ExpectNext`
//! carries the state to restore after `}` — the equivalent of Perl's
//! bracket stack.  `Prototype` has no `PL_expect` equivalent.

/// Lexer expectation state.
///
/// Set by the parser before each peek to tell the lexer how to resolve
/// context-sensitive tokens (`/`, `{`, `(`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Expect {
    /// Expecting a term (value, prefix operator, or keyword).
    /// `/` starts a regex; `{` is a hash constructor.
    #[default]
    Term, // XTERM | XTERMORDORDOR

    /// Expecting an infix or postfix operator.
    /// `/` is division; `{` is a block brace.
    Operator, // XOPERATOR

    /// Start of a statement.  Like Term, but labels are allowed
    /// and statement-level declarations (format, sub) are valid.
    /// `/` starts a regex; `{` uses the byte-level heuristic.
    Statement, // XSTATE

    /// Expecting `{` to open a block.
    /// After the matching `}`, the parser restores the given state.
    Block(ExpectNext), // XBLOCK | XATTRBLOCK | XATTRTERM | XTERMBLOCK | XBLOCKTERM

    /// After a sigil (`$`, `@`, `%`, `&`) for dereference.
    /// `{` is a deref block, not a hash constructor.
    Deref, // XREF

    /// After `->` for postfix dereference (`->@*`, `->$*`, `->%*`).
    Postderef, // XPOSTDEREF

    /// After `sub name` (without `use feature 'signatures'`).
    /// `(` scans a raw prototype string instead of tokenizing.
    Prototype,
}

/// What to expect after a block's closing `}`.
///
/// Equivalent to the value Perl pushes onto `PL_lex_brackstack` when
/// `{` is encountered, and restores when the matching `}` is reached.
/// In our recursive-descent parser, this is carried in the `Block`
/// variant rather than an explicit stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpectNext {
    /// Block is a statement body (`if`/`while`/`for`/`sub`).
    /// After `}`, expect a new statement.
    Statement,

    /// Block produces a value (`eval`/`do`/anonymous sub).
    /// After `}`, expect an operator.
    Operator,

    /// Block is a leading argument (`sort`/`map`/`grep`).
    /// After `}`, expect a term (the list to operate on).
    Term,
}

impl Expect {
    /// Are we in a position where `/` should be a regex?
    pub fn slash_is_regex(&self) -> bool {
        matches!(self, Expect::Term | Expect::Statement | Expect::Deref)
    }

    /// Are we expecting a term (value, prefix, keyword)?
    pub fn expecting_term(&self) -> bool {
        matches!(self, Expect::Term | Expect::Statement | Expect::Deref)
    }

    /// Whether two expects produce identical lexer behavior.
    /// Statement and Term are equivalent for the lexer: both expect
    /// a term, both treat `/` as regex, both treat `<<` as heredoc.
    /// The only difference (`{` heuristic vs always-hash) is handled
    /// at the parser level before any expect transition.
    pub fn lexer_equivalent(&self, other: &Expect) -> bool {
        if self == other {
            return true;
        }
        matches!((self, other), (Expect::Statement, Expect::Term) | (Expect::Term, Expect::Statement))
    }
}
