//! Abstract Syntax Tree — syntax-oriented, not execution-oriented (§7).
//!
//! The AST preserves syntactic distinctions that matter for diagnostics,
//! lowering, and tooling.  It is the public output of `perl-parser`.

use crate::span::Span;
use crate::token::AssignOp;

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
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    /// Expression statement (expression followed by `;`).
    Expr(Expr),

    /// `my $x`, `my ($x, $y)`, with optional assignment.
    My(Vec<VarDecl>, Option<Expr>),
    /// `our $x`.
    Our(Vec<VarDecl>, Option<Expr>),
    /// `local $x`.
    Local(Vec<VarDecl>, Option<Expr>),
    /// `state $x`.
    State(Vec<VarDecl>, Option<Expr>),

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

    /// `__END__` or `__DATA__`.
    DataEnd,
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
    InterpolatedString(Vec<StringPart>),
    /// `qw/.../`.
    QwList(Vec<String>),
    Undef,
    /// Regex literal: `m/.../flags` or `/.../flags`.
    Regex(String, String),

    // ── Variables ─────────────────────────────────────────────
    ScalarVar(String),
    ArrayVar(String),
    HashVar(String),
    GlobVar(String),
    ArrayLen(String),
    SpecialVar(String),

    /// `my $x`, `our ($a, $b)`, `local @arr` etc. in expression context.
    /// The Pratt parser handles `= expr` as normal assignment wrapping this.
    Decl(DeclScope, Vec<VarDecl>),

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
    /// `sub { ... }` — anonymous sub.
    AnonSub(Option<String>, Option<Vec<Param>>, Block),

    // ── Calls ─────────────────────────────────────────────────
    /// Named function call: `foo(...)` or `foo ...`.
    FuncCall(String, Vec<Expr>),
    /// Method call: `$obj->method(...)`.
    MethodCall(Box<Expr>, String, Vec<Expr>),
    /// Indirect method call: `new Foo(...)`.
    IndirectMethodCall(String, Box<Expr>, Vec<Expr>),

    // ── List operators ────────────────────────────────────────
    /// `print EXPR, EXPR` — list operator with args.
    ListOp(String, Vec<Expr>),

    // ── Regex operations ──────────────────────────────────────
    /// `s/pattern/replacement/flags`.
    Subst(String, SubstReplacement, String),
    /// `tr/from/to/flags` or `y/from/to/flags`.
    Translit(String, String, String),

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

    // ── Placeholder for incremental development ───────────────
    Todo(String),
}

/// Part of an interpolated string (§7.3).
#[derive(Clone, Debug)]
pub enum StringPart {
    Const(String),
    ScalarInterp(String),
    ArrayInterp(String),
    ExprInterp(Box<Expr>),
}

/// Substitution replacement — can be a string or interpolated parts.
#[derive(Clone, Debug)]
pub enum SubstReplacement {
    Literal(String),
    Interpolated(Vec<StringPart>),
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
    // Logical
    And,
    Or,
    Dor,
    LowAnd,
    LowOr,
    // Bitwise
    BitAnd,
    BitOr,
    BitXor,
    ShiftL,
    ShiftR,
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
    /// `$ref->@*` postfix deref.
    DerefArray,
    /// `$ref->%*` postfix deref.
    DerefHash,
    /// `$ref->$*` postfix deref.
    DerefScalar,
}

/// Sigil for dereference operations.
/// Scope of a variable declaration in expression context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclScope {
    My,
    Our,
    Local,
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
}

/// Subroutine declaration.
#[derive(Clone, Debug)]
pub struct SubDecl {
    pub name: String,
    pub prototype: Option<String>,
    pub attributes: Vec<Attribute>,
    pub params: Option<Vec<Param>>,
    pub body: Block,
    pub span: Span,
}

/// Subroutine parameter (signatures).
#[derive(Clone, Debug)]
pub struct Param {
    pub sigil: Sigil,
    pub name: String,
    pub default: Option<Expr>,
    pub span: Span,
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
