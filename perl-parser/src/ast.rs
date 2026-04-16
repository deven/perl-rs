//! Abstract Syntax Tree — syntax-oriented, not execution-oriented (§7).
//!
//! The AST preserves syntactic distinctions that matter for diagnostics,
//! lowering, and tooling.  It is the public output of `perl-parser`.

use crate::span::Span;
use crate::token::{AssignOp, DataEndMarker, FieldKind, RegexKind, RepeatKind};

/// A complete Perl program.
#[derive(Clone, Debug)]
pub struct Program {
    pub statements: Vec<Statement>,
    pub span: Span,
}

/// A statement.
#[derive(Clone, Debug)]
pub struct Statement {
    pub kind: StmtKind,
    pub span: Span,
    /// Whether the statement was followed by a semicolon.
    /// Used to distinguish `{ expr }` (hash constructor candidate)
    /// from `{ expr; }` (block) at statement level.
    pub terminated: bool,
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    /// Expression statement (expression followed by `;`).
    ///
    /// In Perl, declarations (`my`, `our`, `state`, `local`) are
    /// expressions, not statements — `my $x = 5, $y` parses as
    /// `(my $x = 5), $y`.  They therefore appear here wrapped as
    /// `Expr(...)`, with `ExprKind::Decl` / `ExprKind::Local`
    /// (often inside `ExprKind::Assign` when an initializer is
    /// present).
    Expr(Expr),

    /// `sub name { ... }` or `sub name (proto) { ... }`.
    SubDecl(SubDecl),

    /// `package Name;` or `package Name { ... }`.
    PackageDecl(PackageDecl),

    /// `use Module ...` or `no Module ...`.
    UseDecl(UseDecl),

    /// `if (...) { ... } elsif ... else { ... }`.
    If(IfStmt),
    /// `unless (...) { ... }`.
    Unless(UnlessStmt),
    /// `while (...) { ... }`.
    While(WhileStmt),
    /// `until (...) { ... }`.
    Until(UntilStmt),
    /// C-style `for (init; cond; step) { ... }`.
    For(ForStmt),
    /// `for/foreach VAR (LIST) { ... }`.
    ForEach(ForEachStmt),

    /// `LABEL: stmt`.
    Labeled(String, Box<Statement>),

    /// A bare block `{ ... }`.
    Block(Block),

    /// `BEGIN { ... }`, `END { ... }`, etc.
    Phaser(PhaserKind, Block),

    /// `given (EXPR) { when ... }`.
    Given(Expr, Block),
    /// `when (EXPR) { ... }` (inside given).
    When(Expr, Block),

    /// `try { ... } catch ($e) { ... } finally { ... }`.
    Try(TryStmt),
    /// `defer { ... }`.
    Defer(Block),

    /// Empty statement (bare `;`).
    Empty,

    /// Logical end of script: `__END__`, `__DATA__`, `^D`, or `^Z`.
    /// The `u32` is the byte offset where trailing data begins
    /// (after the marker line's newline).
    DataEnd(DataEndMarker, u32),

    /// `format NAME = ... .`
    FormatDecl(FormatDecl),

    /// `class Name :attrs { ... }` (5.38+ Corinna).
    ClassDecl(ClassDecl),
    /// `field $var :attrs = default;` (inside class).
    FieldDecl(FieldDecl),
    /// `method name(params) { ... }` (inside class).
    MethodDecl(SubDecl),
}

/// An expression.
#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    // ── Literals ──────────────────────────────────────────────
    IntLit(i64),
    FloatLit(f64),
    StringLit(String),
    /// Interpolated string: sequence of constant and interpolated parts.
    InterpolatedString(Interpolated),
    /// `qw/.../`.
    QwList(Vec<String>),
    Undef,
    /// Regex literal: `m/.../flags`, `/.../flags`, or `qr/.../flags`.
    Regex(RegexKind, Interpolated, Option<String>),

    // ── Variables ─────────────────────────────────────────────
    ScalarVar(String),
    ArrayVar(String),
    HashVar(String),
    GlobVar(String),
    ArrayLen(String),
    /// `$!`, `$^W`, `${^MATCH}`, `$/`, etc.
    SpecialVar(String),
    /// `@+`, `@-`, `@{^CAPTURE}`, etc.
    SpecialArrayVar(String),
    /// `%!`, `%+`, `%-`, `%{^CAPTURE}`, etc.
    SpecialHashVar(String),

    /// The default variable (`$_`) inserted implicitly by the
    /// parser — e.g., when a prototype's `_` slot is omitted from
    /// a call.  Distinct from `ScalarVar("_")`, which represents
    /// the scalar *variable* named `_` as written in the source
    /// (and which may be a lexical `my $_` rather than the global
    /// default).  At runtime, `DefaultVar` always refers to the
    /// global default; `ScalarVar("_")` follows normal scope rules.
    DefaultVar,

    /// `my $x`, `our ($a, $b)`, `state $x` in expression context.
    /// The Pratt parser handles `= expr` as normal assignment wrapping this.
    Decl(DeclScope, Vec<VarDecl>),
    /// `local LVALUE` — localize any lvalue (hash elem, glob, etc.).
    Local(Box<Expr>),

    // ── Binary operations ─────────────────────────────────────
    BinOp(BinOp, Box<Expr>, Box<Expr>),

    // ── Unary operations ──────────────────────────────────────
    UnaryOp(UnaryOp, Box<Expr>),
    PostfixOp(PostfixOp, Box<Expr>),

    // ── Assignment ────────────────────────────────────────────
    Assign(AssignOp, Box<Expr>, Box<Expr>),

    // ── Ternary ───────────────────────────────────────────────
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),

    // ── Range ─────────────────────────────────────────────────
    Range(Box<Expr>, Box<Expr>),
    FlipFlop(Box<Expr>, Box<Expr>),

    // ── Subscripting ──────────────────────────────────────────
    /// `$array[$idx]` — array element.
    ArrayElem(Box<Expr>, Box<Expr>),
    /// `$hash{$key}` — hash element.
    HashElem(Box<Expr>, Box<Expr>),
    /// `@array[$idx1, $idx2]` — array slice.
    ArraySlice(Box<Expr>, Vec<Expr>),
    /// `@hash{$k1, $k2}` — hash slice.
    HashSlice(Box<Expr>, Vec<Expr>),

    // ── Dereference ───────────────────────────────────────────
    /// `$$ref`, `@$ref`, `%$ref`.
    Deref(Sigil, Box<Expr>),
    /// `$ref->[idx]`, `$ref->{key}`.
    ArrowDeref(Box<Expr>, ArrowTarget),

    // ── References ────────────────────────────────────────────
    /// `\$x`, `\@a`, `\%h`, `\&sub`.
    Ref(Box<Expr>),
    /// `[...]` — anonymous array ref.
    AnonArray(Vec<Expr>),
    /// `{...}` — anonymous hash ref (when disambiguated from block).
    AnonHash(Vec<Expr>),
    /// `sub { ... }` — anonymous sub.  Fields: prototype (raw
    /// bytes), signature (parsed 5.20+ signatures syntax), body.
    /// Prototype and signature are mutually exclusive per-call-site;
    /// the `signatures` feature in scope at parse time picks which
    /// form the parentheses were parsed as.
    AnonSub(Option<String>, Option<Signature>, Block),

    // ── Calls ─────────────────────────────────────────────────
    /// Named function call: `foo(...)` or `foo ...`.
    FuncCall(String, Vec<Expr>),
    /// Method call: `$obj->method(...)`.
    MethodCall(Box<Expr>, String, Vec<Expr>),
    /// Indirect method call: `new Foo(...)` → invocant, method, args.
    IndirectMethodCall(Box<Expr>, String, Vec<Expr>),

    // ── Bareword ──────────────────────────────────────────────
    /// A bare identifier not followed by `(` — class name, constant,
    /// or bareword.  The parser doesn't resolve which; that's the
    /// compiler's job.
    Bareword(String),

    // ── List operators ────────────────────────────────────────
    /// List operator with args: `push @arr, 1`, `join ',', @arr`, etc.
    ListOp(String, Vec<Expr>),

    // ── Print operators ───────────────────────────────────────
    /// `print`, `say`, `printf` — with optional filehandle.
    /// `print STDERR "hello"` → filehandle = Some(Bareword("STDERR")).
    /// `print "hello"` → filehandle = None.
    PrintOp(String, Option<Box<Expr>>, Vec<Expr>),

    // ── Regex operations ──────────────────────────────────────
    /// `s/pattern/replacement/flags`.
    Subst(Interpolated, Interpolated, Option<String>),
    /// `tr/from/to/flags` or `y/from/to/flags`.
    Translit(String, String, Option<String>),

    // ── Control flow expressions ──────────────────────────────
    /// Postfix `if`/`unless`/`while`/`until`/`for`/`foreach`.
    PostfixControl(PostfixKind, Box<Expr>, Box<Expr>),

    /// `do BLOCK`.
    DoBlock(Block),
    /// `do EXPR` (do file).
    DoExpr(Box<Expr>),
    /// `eval BLOCK`.
    EvalBlock(Block),
    /// `eval EXPR`.
    EvalExpr(Box<Expr>),

    // ── Comma / list ──────────────────────────────────────────
    /// Comma-separated list of expressions.
    List(Vec<Expr>),

    // ── Parenthesized ─────────────────────────────────────────
    Paren(Box<Expr>),

    // ── Wantarray ─────────────────────────────────────────────
    Wantarray,

    // ── Compile-time constants ────────────────────────────────
    /// `__FILE__` — source filename at parse time.
    SourceFile(String),
    /// `__LINE__` — source line number at parse time (1-based).
    SourceLine(u32),
    /// `__PACKAGE__` — name of the package in effect when this
    /// expression was parsed.  Filled by the parser from its
    /// `current_package` state.
    CurrentPackage(String),
    /// `__SUB__` — reference to the current subroutine, or
    /// `undef` if outside any sub.  Resolved at runtime; no
    /// compile-time data.  Emitted only when the `current_sub`
    /// feature is active; otherwise the token falls through as
    /// a bareword.
    CurrentSub,

    // ── Placeholder for incremental development ───────────────
    /// `...` — yada yada yada (unimplemented placeholder).
    YadaYada,
    /// `-e $file`, `-d "/tmp"`, `-f _` — filetest operator.
    Filetest(char, StatTarget),
    /// `stat $file`, `stat _`, `stat` — stat call.
    Stat(StatTarget),
    /// `lstat $file`, `lstat _`, `lstat` — lstat call.
    Lstat(StatTarget),
}

/// The operand of a stat-family operation: filetest operators (`-e`, `-f`,
/// `-d`, etc.), `stat`, and `lstat`.
///
/// All three share the Perl convention that a bare `_` means "reuse the
/// cached stat buffer from the most recent `stat`, `lstat`, or filetest."
#[derive(Clone, Debug)]
pub enum StatTarget {
    /// An expression: `-f $file`, `-d "/tmp"`, `stat $fh`, or stacked
    /// filetests like `-f -r $file`.
    Expr(Box<Expr>),
    /// The bare `_` filehandle — reuse the cached stat buffer from the
    /// most recent `stat`, `lstat`, or filetest call.
    StatCache,
    /// Implicit `$_` — when no operand is given (`-e;`).
    Default,
}

/// A sequence of interpolated parts — used for strings, regex
/// patterns, and substitution replacements.
#[derive(Clone, Debug)]
pub struct Interpolated(pub Vec<InterpPart>);

impl Interpolated {
    /// If this is a single constant with no interpolation, return
    /// the plain string.  Returns `Some("")` for empty.
    pub fn as_plain_string(&self) -> Option<String> {
        if self.0.is_empty() {
            return Some(String::new());
        }
        if self.0.len() == 1
            && let InterpPart::Const(s) = &self.0[0]
        {
            return Some(s.clone());
        }
        None
    }
}

/// Part of an interpolated value (§7.3).
#[derive(Clone, Debug)]
pub enum InterpPart {
    Const(String),
    ScalarInterp(String),
    ArrayInterp(String),
    ExprInterp(Box<Expr>),
    /// `(?{code})` — raw text for stringification + parsed code.
    RegexCode(String, Box<Expr>),
    /// `(??{code})` — postponed regex code block.
    RegexCondCode(String, Box<Expr>),
}

/// Binary operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    // String
    Concat,
    Repeat,
    // Numeric comparison
    NumEq,
    NumNe,
    NumLt,
    NumGt,
    NumLe,
    NumGe,
    Spaceship,
    // String comparison
    StrEq,
    StrNe,
    StrLt,
    StrGt,
    StrLe,
    StrGe,
    StrCmp,
    /// `isa` — class-instance test (feature-gated).
    Isa,
    // Logical
    And,
    Or,
    DefinedOr,
    LowAnd,
    LowOr,
    LowXor,
    // Bitwise
    BitAnd,
    BitOr,
    BitXor,
    ShiftLeft,
    ShiftRight,
    // Binding
    Binding,
    NotBinding,
}

/// Unary prefix operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Negate,       // -
    NumPositive,  // +  (forces numeric context)
    LogNot,       // !
    BitNot,       // ~
    Ref,          // \
    Not,          // not (low precedence)
    Defined,      // defined
    PreInc,       // ++$x
    PreDec,       // --$x
    Filetest(u8), // -f, -d, etc.
}

/// Postfix operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostfixOp {
    Inc, // $x++
    Dec, // $x--
}

/// Arrow dereference target.
#[derive(Clone, Debug)]
pub enum ArrowTarget {
    ArrayElem(Box<Expr>),
    HashElem(Box<Expr>),
    MethodCall(String, Vec<Expr>),
    /// `$ref->@*` — whole-array postfix deref.
    DerefArray,
    /// `$ref->%*` — whole-hash postfix deref.
    DerefHash,
    /// `$ref->$*` — scalar postfix deref.
    DerefScalar,
    /// `$ref->&*` — code postfix deref.
    DerefCode,
    /// `$ref->**` — glob postfix deref.
    DerefGlob,
    /// `$ref->@[indices]` — array slice by positions.
    ArraySliceIndices(Box<Expr>),
    /// `$ref->@{keys}` — slice of hash returning values as array.
    ArraySliceKeys(Box<Expr>),
    /// `$ref->%[indices]` — key/value pairs from an array
    /// (indices paired with values).
    KvSliceIndices(Box<Expr>),
    /// `$ref->%{keys}` — key/value pairs from a hash.
    KvSliceKeys(Box<Expr>),
    /// `$obj->$method(args)` dynamic method dispatch.
    DynMethod(Box<Expr>, Vec<Expr>),
    // Note: `$ref->$#*` (postfix last-index) is not yet supported;
    // it requires coordinated lexer/parser work because `$#*`
    // doesn't tokenize cleanly under the current lexer rules.
}

/// Sigil for dereference operations.
/// Scope of a variable declaration in expression context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclScope {
    My,
    Our,
    State,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sigil {
    Scalar, // $
    Array,  // @
    Hash,   // %
    Glob,   // *
    Code,   // &
}

/// Postfix control flow kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostfixKind {
    If,
    Unless,
    While,
    Until,
    For,
    Foreach,
}

/// Phaser block kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhaserKind {
    Begin,
    End,
    Init,
    Check,
    Unitcheck,
}

/// A block of statements.
#[derive(Clone, Debug)]
pub struct Block {
    pub statements: Vec<Statement>,
    pub span: Span,
}

/// Variable declaration (the variable part of `my $x`, `my @a`, etc.).
#[derive(Clone, Debug)]
pub struct VarDecl {
    pub sigil: Sigil,
    pub name: String,
    pub span: Span,
    /// Reference-declaration form: `my \$x` binds `$x` as an alias
    /// (via the `declared_refs` feature, 5.26+).  The RHS of the
    /// enclosing assignment must be a matching reference.  When
    /// `false`, this is a normal copy-initialized variable.
    pub is_ref: bool,
}

/// Subroutine declaration.
#[derive(Clone, Debug)]
pub struct SubDecl {
    pub name: String,
    /// Paren-form prototype from pre-signatures Perl (e.g. `($$)`,
    /// `(\@\%)`).  Stored as raw bytes.  Mutually exclusive with
    /// `signature` (the `signatures` feature controls which path
    /// parses the paren-form).  A `:prototype(...)` attribute
    /// shows up in `attributes` and coexists with either.
    pub prototype: Option<String>,
    pub attributes: Vec<Attribute>,
    /// Parsed parameter signature from 5.20+ signatures syntax.
    /// Present when the `signatures` feature is active at the
    /// declaration site.
    pub signature: Option<Signature>,
    pub body: Block,
    pub span: Span,
}

/// Parsed subroutine signature (the `signatures` feature).
///
/// Each parameter is one of several `SigParam` variants: named
/// scalar (optionally with a default), slurpy array, slurpy hash,
/// or an anonymous placeholder that accepts and discards a value.
#[derive(Clone, Debug)]
pub struct Signature {
    pub params: Vec<SigParam>,
    pub span: Span,
}

/// One parameter in a signature.
#[derive(Clone, Debug)]
pub enum SigParam {
    /// `$name`, or `$name = DEFAULT`.  Positional.  When
    /// `default` is `None`, the parameter is required; otherwise
    /// it's optional and the expression evaluates at call time if
    /// the caller didn't supply a value.
    Scalar { name: String, default: Option<Expr>, span: Span },
    /// `@name` — slurpy, captures all remaining positional
    /// arguments.  Must appear last if at all.
    SlurpyArray { name: String, span: Span },
    /// `%name` — slurpy, captures remaining name/value pairs.
    /// Must appear last if at all.
    SlurpyHash { name: String, span: Span },
    /// `$` — anonymous scalar placeholder; accepts a value without
    /// binding it.
    AnonScalar { span: Span },
    /// `@` — anonymous slurpy array (consumes remaining positional
    /// args without binding).
    AnonArray { span: Span },
    /// `%` — anonymous slurpy hash.
    AnonHash { span: Span },
}

/// Attribute on a sub or variable.
#[derive(Clone, Debug)]
pub struct Attribute {
    pub name: String,
    pub value: Option<String>,
    pub span: Span,
}

/// Package declaration.
#[derive(Clone, Debug)]
pub struct PackageDecl {
    pub name: String,
    pub version: Option<String>,
    pub block: Option<Block>,
    pub span: Span,
}

/// `use` or `no` declaration.
#[derive(Clone, Debug)]
pub struct UseDecl {
    pub is_no: bool,
    pub module: String,
    pub version: Option<String>,
    pub imports: Option<Vec<Expr>>,
    pub span: Span,
}

/// `if`/`elsif`/`else` chain.
#[derive(Clone, Debug)]
pub struct IfStmt {
    pub condition: Expr,
    pub then_block: Block,
    pub elsif_clauses: Vec<(Expr, Block)>,
    pub else_block: Option<Block>,
}

/// `unless (...) { ... } else { ... }`.
#[derive(Clone, Debug)]
pub struct UnlessStmt {
    pub condition: Expr,
    pub then_block: Block,
    pub elsif_clauses: Vec<(Expr, Block)>,
    pub else_block: Option<Block>,
}

/// `while (...) { ... } continue { ... }`.
#[derive(Clone, Debug)]
pub struct WhileStmt {
    pub condition: Expr,
    pub body: Block,
    pub continue_block: Option<Block>,
}

/// `until (...) { ... } continue { ... }`.
#[derive(Clone, Debug)]
pub struct UntilStmt {
    pub condition: Expr,
    pub body: Block,
    pub continue_block: Option<Block>,
}

/// C-style `for (init; cond; step) { ... }`.
#[derive(Clone, Debug)]
pub struct ForStmt {
    pub init: Option<Expr>,
    pub condition: Option<Expr>,
    pub step: Option<Expr>,
    pub body: Block,
}

/// `for/foreach VAR (LIST) { ... }`.
#[derive(Clone, Debug)]
pub struct ForEachStmt {
    pub var: Option<VarDecl>,
    pub list: Expr,
    pub body: Block,
    pub continue_block: Option<Block>,
}

/// `try { ... } catch ($e) { ... } finally { ... }`.
#[derive(Clone, Debug)]
pub struct TryStmt {
    pub body: Block,
    pub catch_var: Option<VarDecl>,
    pub catch_block: Option<Block>,
    pub finally_block: Option<Block>,
}

/// `format NAME = ... .`
///
/// `lines` captures every source line of the body in order,
/// classified into one of the four `FormatLine` variants.  Picture
/// lines are already paired with their argument expressions.
#[derive(Clone, Debug)]
pub struct FormatDecl {
    pub name: String,
    pub lines: Vec<FormatLine>,
    pub span: Span,
}

/// One line of a format body.
#[derive(Clone, Debug)]
pub enum FormatLine {
    /// `# ...` — comment, not rendered.  Stored without the leading
    /// `#` or surrounding whitespace; the full source is available
    /// via `span`.
    Comment { text: String, span: Span },

    /// Empty or whitespace-only line; renders as a blank line of
    /// output.
    Blank { span: Span },

    /// A picture line containing no field specifiers.  The text is
    /// stored with any `~`/`~~` characters already replaced with
    /// spaces (so the output width matches the source layout); the
    /// `repeat` field records the original repeat behavior.
    Literal { repeat: RepeatKind, text: String, span: Span },

    /// A picture line containing at least one field.  Arguments come
    /// from the source line immediately following the picture: one
    /// expression per field in order.  When the argument line begins
    /// with `{`, expressions may span multiple source lines until
    /// the matching `}`.
    Picture { repeat: RepeatKind, parts: Vec<FormatPart>, args: Vec<Expr>, span: Span },
}

/// One piece of a picture line.  Literals and fields interleave in
/// source order.
#[derive(Clone, Debug)]
pub enum FormatPart {
    /// Run of literal text (tildes already normalized to spaces).
    Literal(String),
    /// Field specifier.
    Field(FormatField),
}

/// A single picture-line field specifier.
#[derive(Clone, Copy, Debug)]
pub struct FormatField {
    pub kind: FieldKind,
    pub span: Span,
}

/// `class Name :attrs { ... }` (5.38+ Corinna).
#[derive(Clone, Debug)]
pub struct ClassDecl {
    pub name: String,
    pub attributes: Vec<Attribute>,
    pub body: Block,
    pub span: Span,
}

/// `field $var :attrs = default;` (inside class).
#[derive(Clone, Debug)]
pub struct FieldDecl {
    pub var: VarDecl,
    pub attributes: Vec<Attribute>,
    pub default: Option<Expr>,
    pub span: Span,
}
