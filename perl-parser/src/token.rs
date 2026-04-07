//! Token types emitted by the lexer.
//!
//! The lexer does context-sensitive disambiguation (§5.2), so the parser
//! receives tokens that already reflect whether `/` is division or regex,
//! whether `{` is a block or hash, etc.
//!
//! Quote-like constructs emit a stream of sub-tokens (§5.4) rather than
//! a single string token, enabling the parser to build interpolation AST
//! nodes directly.

use crate::span::Span;

/// A token with its source location.
#[derive(Clone, Debug)]
pub struct Spanned {
    pub token: Token,
    pub span: Span,
}

/// Perl keyword.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Keyword {
    // ── Control flow ──────────────────────────────────────────
    If,
    Elsif,
    Else,
    Unless,
    While,
    Until,
    For,
    Foreach,
    Given,
    When,
    Default,

    // ── Exception handling ────────────────────────────────────
    Try,
    Catch,
    Finally,
    Defer,

    // ── Declarations ──────────────────────────────────────────
    My,
    Our,
    Local,
    State,
    Sub,
    Format,
    Package,
    Class,
    Field,
    Method,

    // ── Module ────────────────────────────────────────────────
    Use,
    No,
    Require,
    Do,

    // ── Operators / special ───────────────────────────────────
    And, // low-precedence `and`
    Or,  // low-precedence `or`
    Not, // low-precedence `not`
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Cmp, // string comparison

    // ── Loop control ──────────────────────────────────────────
    Last,
    Next,
    Redo,
    Goto,

    // ── Special values ────────────────────────────────────────
    Undef,
    Return,

    // ── Eval / execution ──────────────────────────────────────
    Eval,
    Die,
    Warn,

    // ── I/O and builtins emitted as distinct token classes ────
    Print,
    Say,
    Chomp,
    Chop,
    Defined,
    Ref,
    Exists,
    Delete,
    Push,
    Pop,
    Shift,
    Unshift,
    Splice,
    Keys,
    Values,
    Each,
    Reverse,
    Sort,
    Map,
    Grep,
    Join,
    Split,
    Sprintf,
    Printf,
    Chr,
    Ord,
    Hex,
    Oct,
    Lc,
    Uc,
    Lcfirst,
    Ucfirst,
    Length,
    Substr,
    Index,
    Rindex,
    Abs,
    Int,
    Sqrt,
    Rand,
    Srand,
    Wantarray,
    Scalar,
    Caller,
    Die_,
    Exit,
    Chdir,
    Mkdir,
    Rmdir,
    Unlink,
    Rename,
    Open,
    Close,
    Read,
    Write,
    Seek,
    Tell,
    Eof,
    Getc,
    Readline,
    Readlink,
    Binmode,
    Stat,
    Lstat,
    Chmod,
    Chown,
    Umask,
    Glob,
    Opendir,
    Readdir,
    Closedir,
    Pos,
    System,
    Exec,
    Qw,

    // ── Phaser blocks ─────────────────────────────────────────
    BEGIN,
    END,
    INIT,
    CHECK,
    UNITCHECK,

    // ── Miscellaneous ─────────────────────────────────────────
    Tie,
    Untie,
    Tied,
    Bless,
    Blessed, // from Scalar::Util but common
    Continue,

    // ── Typed layer (§14, our extensions) ─────────────────────
    Let,
    Fn,
    Struct,
    Enum,
    Impl,
    Trait,
    Match,
}

/// Assignment operator variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Eq,          // =
    AddEq,       // +=
    SubEq,       // -=
    MulEq,       // *=
    DivEq,       // /=
    ModEq,       // %=
    PowEq,       // **=
    ConcatEq,    // .=
    AndEq,       // &&=
    OrEq,        // ||=
    DefinedOrEq, // //=
    BitAndEq,    // &=
    BitOrEq,     // |=
    BitXorEq,    // ^=
    ShiftLEq,    // <<=
    ShiftREq,    // >>=
    RepeatEq,    // x=
    BandEq,      // &.=   (string bitand)
    BorEq,       // |.=   (string bitor)
    BxorEq,      // ^.=   (string bitxor)
}

/// Tokens emitted by the lexer.
///
/// Named to match perly.y token names where practical, but reorganized
/// by function rather than by how toke.c happens to emit them.
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    // ── End of input ──────────────────────────────────────────
    Eof,

    // ── Literals ──────────────────────────────────────────────
    /// Integer literal (decimal, hex, octal, binary).
    IntLit(i64),
    /// Float literal.
    FloatLit(f64),
    /// Single-quoted string (no interpolation, fully resolved).
    StrLit(String),
    /// Version string (v5.42.2 or 5.042_002).
    VersionLit(String),

    // ── Identifiers ───────────────────────────────────────────
    /// Bare identifier (may be package-qualified: `Foo::Bar::baz`).
    Ident(String),
    /// Label (`LOOP:`, `OUTER:`).  Name without the colon.
    Label(String),

    // ── Variables ─────────────────────────────────────────────
    /// `$name` — scalar variable.
    ScalarVar(String),
    /// `@name` — array variable.
    ArrayVar(String),
    /// `%name` — hash variable.
    HashVar(String),
    /// `*name` — glob.
    GlobVar(String),
    /// `$#name` — array last index.
    ArrayLen(String),
    /// `$$`, `$!`, `$@`, `$_`, `$0`, `$/`, `$\`, `$^W`, `${^MATCH}`, etc.
    SpecialVar(String),
    /// `@+`, `@-`, `@{^CAPTURE}`, etc.
    SpecialArrayVar(String),
    /// `%!`, `%+`, `%-`, `%{^CAPTURE}`, etc.
    SpecialHashVar(String),

    // ── Keywords ──────────────────────────────────────────────
    Keyword(Keyword),

    // ── Operators — arithmetic ────────────────────────────────
    Plus,
    Minus,
    Star,    // * (multiply or glob)
    Slash,   // / (division; regex handled separately)
    Percent, // %
    Power,   // **

    // ── Operators — string ────────────────────────────────────
    Dot, // . (concatenation)
    X,   // x (string repetition)

    // ── Operators — comparison ────────────────────────────────
    NumEq,     // ==
    NumNe,     // !=
    NumLt,     // <
    NumGt,     // >
    NumLe,     // <=
    NumGe,     // >=
    Spaceship, // <=>
    StrEq,     // eq
    StrNe,     // ne
    StrLt,     // lt
    StrGt,     // gt
    StrLe,     // le
    StrGe,     // ge
    StrCmp,    // cmp

    // ── Operators — logical ───────────────────────────────────
    AndAnd,    // &&
    OrOr,      // ||
    DefinedOr, // //  (defined-or)
    Bang,      // !
    Not,       // not (low precedence, also keyword)

    // ── Operators — bitwise ───────────────────────────────────
    BitAnd, // &
    BitOr,  // |
    BitXor, // ^
    Tilde,  // ~ (complement)
    ShiftL, // <<
    ShiftR, // >>

    // ── Operators — binding ───────────────────────────────────
    Binding,    // =~
    NotBinding, // !~

    // ── Operators — range ─────────────────────────────────────
    DotDot,    // ..
    DotDotDot, // ...

    // ── Operators — increment/decrement ───────────────────────
    PlusPlus,   // ++
    MinusMinus, // --

    // ── Operators — assignment ────────────────────────────────
    Assign(AssignOp),

    // ── Operators — arrow and deref ───────────────────────────
    Arrow,     // ->
    Backslash, // \ (reference constructor)

    // ── Operators — ternary ───────────────────────────────────
    Question, // ?
    Colon,    // :

    // ── Operators — string special ────────────────────────────
    Comma,    // ,
    FatComma, // =>

    // ── Operators — filetest ──────────────────────────────────
    /// `-f`, `-d`, `-r`, etc.  Contains the test character.
    Filetest(u8),

    // ── Delimiters ────────────────────────────────────────────
    LParen,   // (
    RParen,   // )
    LBracket, // [
    RBracket, // ]
    LBrace,   // { (block, hash subscript)
    /// `{` that opens an anonymous hash constructor.
    /// Distinct from `LBrace` (which opens blocks and subscripts).
    /// The lexer decides which to emit, matching toke.c's
    /// `PERLY_BRACE_OPEN` vs `HASHBRACK` distinction.
    HashBrace,
    RBrace, // }

    // ── Punctuation ───────────────────────────────────────────
    Semi,     // ;
    At,       // @ (when not part of a variable)
    Dollar,   // $ (when not part of a variable)
    HashSign, // # (should not normally reach parser)

    /// Subroutine prototype scanned as a raw string: `($$)` → `"$$"`.
    /// The lexer produces this instead of `LParen` when `base = Proto`.
    /// Matches toke.c's `scan_str` + `force_next(THING)` for prototypes.
    Prototype(String),

    // ── Quote/interpolation sub-tokens (§5.4) ─────────────────
    /// Start of a quote-like construct.  Contains quote type and delimiter.
    QuoteBegin(QuoteKind, u8),
    /// End of a quote-like construct.
    QuoteEnd,
    /// Literal segment inside a quote.
    ConstSegment(String),
    /// `$name` or `${name}` interpolation inside a quote.
    InterpScalar(String),
    /// `@name` interpolation inside a quote (array in string).
    InterpArray(String),
    /// Start of `${expr}` expression interpolation.
    InterpExprBegin,
    /// End of `${expr}` expression interpolation.
    InterpExprEnd,

    // ── Regex sub-tokens ──────────────────────────────────────
    /// Start of regex: `m/`, `qr/`, bare `//`, or `s/`.
    RegexBegin(RegexKind, u8),
    /// Regex body (pattern text, pre-interpolation).
    RegexBody(String),
    /// Regex flags (imsx etc.).
    RegexFlags(String),
    /// End of regex.
    RegexEnd,
    /// Substitution replacement (between second and third delimiters).
    SubstReplacement(String),

    // ── Compound regex tokens (bootstrap, pre-interpolation) ──
    /// Complete regex: `/pattern/flags`, `m/pattern/flags`, `qr/pattern/flags`.
    RegexLit(RegexKind, String, String),
    /// Complete substitution: `s/pattern/replacement/flags`.
    SubstLit(String, String, String),
    /// Complete transliteration: `tr/from/to/flags` or `y/from/to/flags`.
    TranslitLit(String, String, String),

    // ── Heredoc ───────────────────────────────────────────────
    /// `<<TAG`, `<<"TAG"`, `<<'TAG'`.
    HeredocBegin(HeredocKind, String),
    /// Body of a heredoc (sub-tokens if interpolating).
    HeredocEnd,
    /// Complete heredoc with body already collected (bootstrap).
    /// Fields: kind, tag, body.
    HeredocLit(HeredocKind, String, String),

    // ── Special ───────────────────────────────────────────────
    /// `qw/.../` — list of words.
    QwList(Vec<String>),
    /// `__END__`, `__DATA__`, `^D` (0x04), or `^Z` (0x1a) — logical end of script.
    DataEnd(DataEndMarker),
    /// Yada yada yada (`...` as a statement).
    YadaYada,
    /// `<STDIN>`, `<>`, `<$fh>`, `<*.txt>` — readline or glob.
    Readline(String),
}

/// Which marker triggered logical end-of-script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataEndMarker {
    /// `__END__` — trailing data readable via `<main::DATA>`.
    End,
    /// `__DATA__` — trailing data readable via `<DATA>` in current package.
    Data,
    /// ^D (0x04) — logical EOF, no DATA filehandle.
    CtrlD,
    /// ^Z (0x1a) — logical EOF, no DATA filehandle.
    CtrlZ,
}

/// Kind of quote-like construct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuoteKind {
    /// `'...'` or `q//`
    Single,
    /// `"..."` or `qq//`
    Double,
    /// Backtick or `qx//`
    Backtick,
}

/// Kind of regex construct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegexKind {
    /// `m//` or bare `//`
    Match,
    /// `qr//`
    Qr,
    /// `s///`
    Subst,
    /// `tr///` or `y///`
    Translit,
}

/// Kind of heredoc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeredocKind {
    /// `<<TAG` or `<<"TAG"` — interpolating.
    Interpolating,
    /// `<<'TAG'` — literal.
    Literal,
    /// `<<~TAG` — indented (5.26+).
    Indented,
    /// `<<~'TAG'` — indented literal.
    IndentedLiteral,
}

impl Token {
    /// Is this token something that can start an expression (a term)?
    pub fn is_term_start(&self) -> bool {
        matches!(
            self,
            Token::IntLit(_)
                | Token::FloatLit(_)
                | Token::StrLit(_)
                | Token::Ident(_)
                | Token::ScalarVar(_)
                | Token::ArrayVar(_)
                | Token::HashVar(_)
                | Token::GlobVar(_)
                | Token::SpecialVar(_)
                | Token::SpecialArrayVar(_)
                | Token::SpecialHashVar(_)
                | Token::LParen
                | Token::LBracket
                | Token::LBrace
                | Token::HashBrace
                | Token::Minus
                | Token::Plus
                | Token::Bang
                | Token::Tilde
                | Token::Backslash
                | Token::PlusPlus
                | Token::MinusMinus
                | Token::Keyword(_)
                | Token::QuoteBegin(_, _)
                | Token::RegexBegin(_, _)
                | Token::HeredocBegin(_, _)
                | Token::RegexLit(_, _, _)
                | Token::SubstLit(_, _, _)
                | Token::TranslitLit(_, _, _)
                | Token::HeredocLit(_, _, _)
                | Token::Readline(_)
                | Token::QwList(_)
                | Token::Dollar
                | Token::At
        )
    }
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Token::Eof => write!(f, "EOF"),
            Token::IntLit(n) => write!(f, "{n}"),
            Token::FloatLit(n) => write!(f, "{n}"),
            Token::StrLit(s) => write!(f, "'{s}'"),
            Token::Ident(s) => write!(f, "{s}"),
            Token::ScalarVar(s) => write!(f, "${s}"),
            Token::ArrayVar(s) => write!(f, "@{s}"),
            Token::HashVar(s) => write!(f, "%{s}"),
            Token::SpecialVar(s) => write!(f, "${s}"),
            Token::SpecialArrayVar(s) => write!(f, "@{s}"),
            Token::SpecialHashVar(s) => write!(f, "%{s}"),
            Token::Semi => write!(f, ";"),
            Token::Plus => write!(f, "+"),
            Token::Minus => write!(f, "-"),
            Token::Star => write!(f, "*"),
            Token::Slash => write!(f, "/"),
            Token::Assign(AssignOp::Eq) => write!(f, "="),
            Token::LParen => write!(f, "("),
            Token::RParen => write!(f, ")"),
            Token::LBrace => write!(f, "{{"),
            Token::HashBrace => write!(f, "{{"),
            Token::RBrace => write!(f, "}}"),
            Token::Prototype(s) => write!(f, "({})", s),
            Token::LBracket => write!(f, "["),
            Token::RBracket => write!(f, "]"),
            Token::Comma => write!(f, ","),
            Token::Arrow => write!(f, "->"),
            Token::Keyword(kw) => write!(f, "{kw:?}"),
            _ => write!(f, "{self:?}"),
        }
    }
}
