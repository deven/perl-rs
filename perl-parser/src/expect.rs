//! Lexer–parser expectation state (§5.2).
//!
//! The `Expect` struct tells the lexer how to resolve context-sensitive
//! tokens like `/` (regex vs division) and `{` (block vs hash).  The
//! parser updates this after consuming each token or construct.
//!
//! This decomposes Perl 5's 11 `PL_expect` states into orthogonal fields.

/// What the lexer should expect next: a term or an operator?
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BaseExpect {
    /// Expecting a value, prefix operator, or keyword.
    /// `/` starts a regex.
    #[default]
    Term,

    /// Expecting an infix/postfix operator.
    /// `/` is division.
    Operator,

    /// Start of a statement.  Like Term, but labels are allowed
    /// and statement-level declarations (format, sub) are valid.
    Statement,

    /// After `->`.  The next token is a method name, subscript
    /// key, or sigil for postfix deref.
    Ref,

    /// After `->` for postfix dereference syntax (`$ref->@*`).
    Postderef,

    /// After `sub name` — if `(` follows, scan it as a raw prototype
    /// string (not tokenized).  Matches toke.c's `scan_str()` call
    /// inside `yyl_sub()`.
    Proto,
}

/// How to interpret `{` when encountered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BraceDisposition {
    /// Use parser position and local token context to determine
    /// whether `{` is a block or hash.
    #[default]
    Infer,

    /// `{` is a statement block.  After `}`, expect a statement.
    /// Used after `if (expr)`, `while (expr)`, etc.
    Block,

    /// `{` is a block-expression.  After `}`, expect an operator.
    /// Used for `do { }`, anonymous subs, etc.
    BlockExpr,

    /// `{` is a block argument.  After `}`, expect a term.
    /// Used after `->method` where the block is an argument.
    BlockArg,

    /// `{` is a hash constructor.
    /// Used after filetest operators (the XTERMORDORDOR case).
    Hash,
}

/// Combined expectation state.
///
/// Maps to Perl 5's states:
///
/// | Perl 5 state   | base      | brace     | allow_attributes |
/// |----------------|-----------|-----------|------------------|
/// | XOPERATOR      | Operator  | Infer     | false            |
/// | XTERM          | Term      | Infer     | false            |
/// | XSTATE         | Statement | Infer     | false            |
/// | XBLOCK         | Term      | Block     | false            |
/// | XREF           | Ref       | Infer     | false            |
/// | XPOSTDEREF     | Postderef | Infer     | false            |
/// | XATTRBLOCK     | Term      | Block     | true             |
/// | XATTRTERM      | Term      | BlockExpr | true             |
/// | XTERMBLOCK     | Term      | BlockExpr | false            |
/// | XBLOCKTERM     | Term      | BlockArg  | false            |
/// | XTERMORDORDOR  | Operator  | Hash      | false            |
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Expect {
    pub base: BaseExpect,
    pub brace: BraceDisposition,
    pub allow_attributes: bool,
}

impl Expect {
    // ── Named presets matching Perl 5's states ────────────────

    pub const XOPERATOR: Expect = Expect { base: BaseExpect::Operator, brace: BraceDisposition::Infer, allow_attributes: false };
    pub const XTERM: Expect = Expect { base: BaseExpect::Term, brace: BraceDisposition::Infer, allow_attributes: false };
    pub const XSTATE: Expect = Expect { base: BaseExpect::Statement, brace: BraceDisposition::Infer, allow_attributes: false };
    pub const XBLOCK: Expect = Expect { base: BaseExpect::Term, brace: BraceDisposition::Block, allow_attributes: false };
    pub const XREF: Expect = Expect { base: BaseExpect::Ref, brace: BraceDisposition::Infer, allow_attributes: false };
    pub const XPOSTDEREF: Expect = Expect { base: BaseExpect::Postderef, brace: BraceDisposition::Infer, allow_attributes: false };
    pub const XATTRBLOCK: Expect = Expect { base: BaseExpect::Term, brace: BraceDisposition::Block, allow_attributes: true };
    pub const XATTRTERM: Expect = Expect { base: BaseExpect::Term, brace: BraceDisposition::BlockExpr, allow_attributes: true };
    pub const XTERMBLOCK: Expect = Expect { base: BaseExpect::Term, brace: BraceDisposition::BlockExpr, allow_attributes: false };
    pub const XBLOCKTERM: Expect = Expect { base: BaseExpect::Term, brace: BraceDisposition::BlockArg, allow_attributes: false };
    pub const XTERMORDORDOR: Expect = Expect { base: BaseExpect::Operator, brace: BraceDisposition::Hash, allow_attributes: false };

    /// Are we in a position where `/` should be a regex?
    pub fn slash_is_regex(&self) -> bool {
        matches!(self.base, BaseExpect::Term | BaseExpect::Statement)
    }

    /// Are we expecting a term (value, prefix, keyword)?
    pub fn expecting_term(&self) -> bool {
        matches!(self.base, BaseExpect::Term | BaseExpect::Statement)
    }
}
