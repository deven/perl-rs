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
    Eq,           // =
    AddEq,        // +=
    SubEq,        // -=
    MulEq,        // *=
    DivEq,        // /=
    ModEq,        // %=
    PowEq,        // **=
    ConcatEq,     // .=
    AndEq,        // &&=
    OrEq,         // ||=
    DefinedOrEq,  // //=
    BitAndEq,     // &=
    BitOrEq,      // |=
    BitXorEq,     // ^=
    ShiftLeftEq,  // <<=
    ShiftRightEq, // >>=
    RepeatEq,     // x=
    BandEq,       // &.=   (string bitand)
    BorEq,        // |.=   (string bitor)
    BxorEq,       // ^.=   (string bitxor)
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

    // ── Operators — comparison ────────────────────────────────
    NumEq,     // ==
    NumNe,     // !=
    NumLt,     // <
    NumGt,     // >
    NumLe,     // <=
    NumGe,     // >=
    Spaceship, // <=>

    // ── Operators — logical ───────────────────────────────────
    AndAnd,    // &&
    OrOr,      // ||
    DefinedOr, // //  (defined-or)
    Bang,      // !

    // ── Operators — bitwise ───────────────────────────────────
    BitAnd,     // &
    BitOr,      // |
    BitXor,     // ^
    Tilde,      // ~ (complement)
    ShiftLeft,  // <<
    ShiftRight, // >>

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
    LeftParen,    // (
    RightParen,   // )
    LeftBracket,  // [
    RightBracket, // ]
    LeftBrace,    // { (block, hash subscript)
    RightBrace,   // }

    // ── Punctuation ───────────────────────────────────────────
    Semi,     // ;
    At,       // @ (when not part of a variable)
    Dollar,   // $ (when not part of a variable)
    HashSign, // # (should not normally reach parser)

    // ── Quote/interpolation sub-tokens (§5.4) ─────────────────
    /// Start of a quote-like construct.  Contains quote type and delimiter.
    QuoteSublexBegin(QuoteKind, u8),
    /// End of a quote-like construct.
    SublexEnd,
    /// Literal segment inside a quote.
    ConstSegment(String),
    /// `$name` or `${name}` interpolation inside a quote.
    InterpScalar(String),
    /// `@name` interpolation inside a quote (array in string).
    InterpArray(String),
    /// `${expr}` expression interpolation — lexer switches to normal code mode.
    /// Parser calls parse_expr(), then expect_token(RightBrace).
    InterpScalarExprStart,
    /// `@{expr}` expression interpolation — lexer switches to normal code mode.
    InterpArrayExprStart,

    // ── Regex sub-tokens ──────────────────────────────────────
    /// Start of regex: `m/`, `qr/`, bare `//`, or `s/`.
    RegexSublexBegin(RegexKind, u8),
    /// `(?{` — embedded code block in a regex pattern.
    /// Lexer switches to normal code mode until `}`.
    RegexCodeStart,
    /// `(??{` — postponed regex code block.
    /// Lexer switches to normal code mode until `}`.
    RegexCondCodeStart,

    // ── Substitution / transliteration ──────────────────────────
    /// Start of a substitution.  The delimiter byte is carried so
    /// the parser can pass it back to `start_subst_replacement`.
    /// Pattern body tokens follow until SublexEnd.
    SubstSublexBegin(u8),
    /// Complete transliteration: `tr/from/to/flags` or `y/from/to/flags`.
    TranslitLit(String, String, Option<String>),

    // ── Heredoc ───────────────────────────────────────────────
    /// `<<TAG`, `<<"TAG"`, `<<'TAG'`.
    HeredocBegin(HeredocKind, String),
    /// Body of a heredoc (sub-tokens if interpolating).
    HeredocEnd,
    /// Complete heredoc with body already collected (bootstrap).
    /// Fields: kind, tag, body.
    HeredocLit(HeredocKind, String, String),

    // ── Format sub-tokens ─────────────────────────────────────
    /// Opens a `format NAME = ... .` body.  `name` is the format
    /// name (defaults to `STDOUT` when omitted at the call site).
    /// The sublex context ends with `SublexEnd` at the `.` terminator.
    FormatSublexBegin(String),
    /// `# ...` — comment line inside a format (column-0 `#`).
    FormatComment(String),
    /// Whitespace-only line inside a format.
    FormatBlankLine,
    /// A picture line containing no field specifiers.  Emitted
    /// instead of the `FormatPictureBegin` / … / `FormatPictureEnd`
    /// stream when the line has only literal text (tildes
    /// normalized to spaces).
    FormatLiteralLine(RepeatKind, String),
    /// Start of a picture line that contains one or more fields.
    /// Followed by alternating `FormatLiteral` and `FormatField`
    /// tokens, then `FormatPictureEnd`, then `FormatArgsBegin`.
    FormatPictureBegin(RepeatKind),
    /// Literal run of text between or around fields inside a
    /// picture line.  Tildes have already been replaced with
    /// spaces; the `RepeatKind` is on the enclosing
    /// `FormatPictureBegin`.
    FormatLiteral(String),
    /// One field specifier in a picture line.
    FormatField(FieldKind),
    /// Closes a picture line.  Always followed by `FormatArgsBegin`.
    FormatPictureEnd,
    /// Start of the argument line following a picture.  The lexer
    /// emits normal code tokens until `FormatArgsEnd`.  Two modes:
    ///   * Line mode (default): newline terminates the args.
    ///   * Braced mode: if the parser sees a `{` as the first
    ///     token and calls `lexer.format_args_enter_braced()`,
    ///     matching `}` terminates instead (expressions may span
    ///     multiple lines).
    FormatArgsBegin,
    /// Closes the argument line.  Next token resumes format body
    /// scanning.
    FormatArgsEnd,

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
    /// Interpolating heredoc body (`<<TAG`, `<<"TAG"`)
    Heredoc,
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

/// Repeat behavior on a format picture line, controlled by `~` or
/// `~~` characters anywhere in the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepeatKind {
    /// No `~` on the line.
    None,
    /// At least one `~` on the line: suppress the line if all
    /// fields produce empty output.
    Suppress,
    /// `~~` anywhere on the line: repeat the line until all fields
    /// are exhausted (become undef).
    Repeat,
}

/// One field specifier in a format picture line.
///
/// See `perlform` for details.  Widths are in source columns and
/// include the leading `@` or `^` character.  `u32` matches Perl's
/// internal representation (C `int`), which has been empirically
/// verified to support fields wider than 65535.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    /// `@<<<<` — text, left-justified.  `truncate_ellipsis` is set
    /// when the field ends with `...` (shown in output when the
    /// value was truncated).
    LeftJustify { width: u32, truncate_ellipsis: bool },
    /// `@>>>>` — text, right-justified.
    RightJustify { width: u32, truncate_ellipsis: bool },
    /// `@||||` — text, centered.
    Center { width: u32, truncate_ellipsis: bool },

    /// `^<<<<` — fill mode, left-justified (word-wraps, chops the
    /// source scalar).
    FillLeft { width: u32, truncate_ellipsis: bool },
    /// `^>>>>` — fill mode, right-justified.
    FillRight { width: u32, truncate_ellipsis: bool },
    /// `^||||` — fill mode, centered.
    FillCenter { width: u32, truncate_ellipsis: bool },

    /// `@*` — variable-width multi-line field.
    MultiLine,
    /// `^*` — variable-width, one line at a time; chops the source
    /// scalar.
    FillMultiLine,

    /// `@####` (integer) or `@####.##` (with fractional part).
    /// `leading_zeros` is set when the first `#` was written as `0`
    /// (pad with zeros instead of spaces).  `caret` is set for
    /// `^###` — blanks the field when the value is undef instead
    /// of rendering as 0.
    Numeric { integer_digits: u32, decimal_digits: Option<u32>, leading_zeros: bool, caret: bool },
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
                | Token::LeftParen
                | Token::LeftBracket
                | Token::LeftBrace
                | Token::Minus
                | Token::Plus
                | Token::Bang
                | Token::Tilde
                | Token::Backslash
                | Token::PlusPlus
                | Token::MinusMinus
                | Token::Keyword(_)
                | Token::QuoteSublexBegin(_, _)
                | Token::RegexSublexBegin(_, _)
                | Token::HeredocBegin(_, _)
                | Token::SubstSublexBegin(_)
                | Token::TranslitLit(_, _, _)
                | Token::HeredocLit(_, _, _)
                | Token::Readline(_)
                | Token::QwList(_)
                | Token::Dollar
                | Token::At
                | Token::Slash
                | Token::DefinedOr
                | Token::Assign(AssignOp::DivEq)
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
            Token::LeftParen => write!(f, "("),
            Token::RightParen => write!(f, ")"),
            Token::LeftBrace => write!(f, "{{"),
            Token::RightBrace => write!(f, "}}"),
            Token::LeftBracket => write!(f, "["),
            Token::RightBracket => write!(f, "]"),
            Token::Comma => write!(f, ","),
            Token::Arrow => write!(f, "->"),
            Token::Keyword(kw) => write!(f, "{kw:?}"),
            _ => write!(f, "{self:?}"),
        }
    }
}
