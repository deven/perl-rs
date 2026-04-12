//! Lexer expectation state.
//!
//! The `Expect` enum tells the lexer how to resolve context-sensitive
//! tokens: `/` (regex vs division) and `%` (hash sigil vs modulo).
//! The parser sets this before each peek to communicate syntactic
//! context to the lexer.
//!
//! Maps to Perl 5's `PL_expect` states.  `XTERMORDORDOR` is folded
//! into `Term` (we always lex `//` as defined-or), and the five block
//! variants are collapsed into `Block(ExpectNext)`, where `ExpectNext`
//! carries the state to restore after `}`.

/// Lexer expectation state.
///
/// Set by the parser before each peek to tell the lexer how to resolve
/// context-sensitive tokens (`/`, `%`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Expect {
    /// Expecting a term (value, prefix operator, or keyword).
    /// `/` starts a regex; `%` is a hash sigil.
    #[default]
    Term, // XTERM | XTERMORDORDOR

    /// Expecting an infix or postfix operator.
    /// `/` is division; `%` is modulo.
    Operator, // XOPERATOR

    /// Start of a statement.  Like Term, but labels are allowed
    /// and statement-level declarations (format, sub) are valid.
    Statement, // XSTATE

    /// Expecting `{` to open a block.
    /// After the matching `}`, the parser restores the given state.
    Block(ExpectNext), // XBLOCK | XATTRBLOCK | XATTRTERM | XTERMBLOCK | XBLOCKTERM

    /// After a sigil (`$`, `@`, `%`, `&`) for dereference.
    Deref, // XREF

    /// After `->` for postfix dereference (`->@*`, `->$*`, `->%*`).
    Postderef, // XPOSTDEREF
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
        matches!(self, Expect::Term | Expect::Statement | Expect::Block(_) | Expect::Deref)
    }

    /// Are we expecting a term (value, prefix, keyword)?
    pub fn expecting_term(&self) -> bool {
        matches!(self, Expect::Term | Expect::Statement | Expect::Block(_) | Expect::Deref)
    }

    /// Whether two expects produce identical lexer behavior.
    /// All term-like states (Term, Statement, Block, Deref) are
    /// equivalent for the lexer: `/` is regex, `%` is hash sigil.
    /// Operator and Postderef are equivalent: `/` is divide, `%`
    /// is modulo.
    pub fn lexer_equivalent(&self, other: &Expect) -> bool {
        if self == other {
            return true;
        }
        self.expecting_term() == other.expecting_term()
    }
}
