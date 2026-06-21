//! Abstract Syntax Tree — syntax-oriented, not execution-oriented (§7).
//!
//! The AST preserves syntactic distinctions that matter for diagnostics, lowering, and tooling.  It is the public
//! output of `perl-parser`.

use crate::keyword::Keyword;
use crate::span::Span;
use crate::token::{AssignOp, FieldKind, RegexKind, RepeatKind};

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

    /// Whether the statement was followed by a semicolon.  Used to distinguish `{ expr }` (hash constructor candidate)
    /// from `{ expr; }` (block) at statement level.
    pub terminated: bool,
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    /// Expression statement (expression followed by `;`).
    ///
    /// In Perl, declarations (`my`, `our`, `state`, `local`) are expressions, not statements — `my $x = 5, $y` parses
    /// as `(my $x = 5), $y`.  They therefore appear here wrapped as `Expr(...)`, with `ExprKind::Decl` /
    /// `ExprKind::Local` (often inside `ExprKind::Assign` when an initializer is present).
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
    /// A bare block `{ ... }` with optional `continue` block.
    Block(Block, Option<Block>),

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

    /// Logical end of script: `__END__`, `__DATA__`, `^D`, or `^Z`.  The `u32` is the byte offset where trailing data
    /// begins (after the marker line's newline).
    DataEnd(Keyword, u32),

    /// `format NAME = ... .`
    FormatDecl(FormatDecl),

    /// `class Name :attrs { ... }` (5.38+ Corinna).
    ClassDecl(ClassDecl),

    /// `field $var :attrs = default;` (inside class).
    FieldDecl(FieldDecl),

    /// `method name(params) { ... }` (inside class).
    MethodDecl(SubDecl),
}

/// Evaluation context — scalar, list, or void (§6.1.5).
///
/// This is the *evaluation* context an expression is in, distinct from its grammatical category.  It is stamped onto
/// AST nodes after parsing via [`Expr::save_context`]; see the module docs and design §6.1.5 for the full model.
///
/// - `Scalar` — the expression produces a single value (the ambient default for most positions).
/// - `List`   — the expression is flattened into a list (a genuine stack operation; marked by an interposed list build).
/// - `Void`   — the expression's value is discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    Scalar,
    List,
    Void,
}

/// An expression.
#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,

    /// Evaluation context this expression is in, or `None` until stamped.
    ///
    /// Born `None`.  Resolved by [`save_context`](Expr::save_context), called by whoever first knows the context: a
    /// node's own constructor (for context-independent children), a positional parsing routine (for grammar positions
    /// that fix the context — statement/clause parsers), or the parent node's own `save_context` (for deferred,
    /// context-dependent children).  Context is never threaded through the parser as a parameter.
    pub ctx: Option<Context>,
}

impl Expr {
    /// Construct an expression with no context yet stamped (`ctx: None`).
    pub fn new(kind: ExprKind, span: Span) -> Self {
        Expr { kind, span, ctx: None }
    }

    /// Construct an expression with its context already known (context-independent nodes set this at construction).
    pub fn with_context(kind: ExprKind, span: Span, ctx: Context) -> Self {
        Expr { kind, span, ctx: Some(ctx) }
    }

    // ── Operand-bearing constructors ──────────────────────────────────────────────────────────────────────────────
    //
    // These box their `Expr` operands internally so call sites never write `Box::new`.  The node's `span` is computed
    // by the caller and passed last (uniformly across all constructors — no constructor computes a span merge itself).
    // Passing operands by value also makes these the seam where the context-stamping ownership contract (§6.1.5) is
    // applied to context-independent children.

    /// `left OP right` — binary operator.
    pub fn binop(op: BinOp, left: Expr, right: Expr, span: Span) -> Expr {
        Expr::new(ExprKind::BinOp(op, Box::new(left), Box::new(right)), span)
    }

    /// `left = right`, `left += right`, etc. — assignment.
    pub fn assign(op: AssignOp, left: Expr, right: Expr, span: Span) -> Expr {
        Expr::new(ExprKind::Assign(op, Box::new(left), Box::new(right)), span)
    }

    /// `cond ? then_expr : else_expr` — ternary conditional.
    pub fn ternary(cond: Expr, then_expr: Expr, else_expr: Expr, span: Span) -> Expr {
        Expr::new(ExprKind::Ternary(Box::new(cond), Box::new(then_expr), Box::new(else_expr)), span)
    }

    /// `left .. right` — range.
    pub fn range(left: Expr, right: Expr, span: Span) -> Expr {
        Expr::new(ExprKind::Range(Box::new(left), Box::new(right)), span)
    }

    /// `left ... right` — flip-flop.
    pub fn flipflop(left: Expr, right: Expr, span: Span) -> Expr {
        Expr::new(ExprKind::FlipFlop(Box::new(left), Box::new(right)), span)
    }

    /// `$base[index]` — array element.
    pub fn array_elem(base: Expr, index: Expr, span: Span) -> Expr {
        Expr::new(ExprKind::ArrayElem(Box::new(base), Box::new(index)), span)
    }

    /// `$base{key}` — hash element.
    pub fn hash_elem(base: Expr, key: Expr, span: Span) -> Expr {
        Expr::new(ExprKind::HashElem(Box::new(base), Box::new(key)), span)
    }

    /// `OP operand` — prefix unary operator.
    pub fn unary(op: UnaryOp, operand: Expr, span: Span) -> Expr {
        Expr::new(ExprKind::UnaryOp(op, Box::new(operand)), span)
    }

    /// `operand++`, `operand--` — postfix operator.
    pub fn postfix(op: PostfixOp, operand: Expr, span: Span) -> Expr {
        Expr::new(ExprKind::PostfixOp(op, Box::new(operand)), span)
    }

    /// `$$ref`, `@$ref`, `%$ref`, etc. — dereference.
    pub fn deref(sigil: Sigil, operand: Expr, span: Span) -> Expr {
        Expr::new(ExprKind::Deref(sigil, Box::new(operand)), span)
    }

    /// `\operand` — reference.
    pub fn reference(operand: Expr, span: Span) -> Expr {
        Expr::new(ExprKind::Ref(Box::new(operand)), span)
    }

    /// `local operand` — localize an lvalue.
    pub fn local(operand: Expr, span: Span) -> Expr {
        Expr::new(ExprKind::Local(Box::new(operand)), span)
    }

    /// `$invocant->name(args)` — method call.
    pub fn method_call(invocant: Expr, name: String, args: Vec<Expr>, span: Span) -> Expr {
        Expr::new(ExprKind::MethodCall(Box::new(invocant), name, args), span)
    }

    /// `$base->TARGET` — arrow dereference/postfix.
    pub fn arrow_deref(base: Expr, target: ArrowTarget, span: Span) -> Expr {
        Expr::new(ExprKind::ArrowDeref(Box::new(base), target), span)
    }

    /// Postfix `EXPR if/unless/while/until/for COND` — statement modifier in expression form.
    pub fn postfix_control(kind: PostfixKind, expr: Expr, cond: Expr, span: Span) -> Expr {
        Expr::new(ExprKind::PostfixControl(kind, Box::new(expr), Box::new(cond)), span)
    }

    // ── Block-bearing constructors ────────────────────────────────────────────────────────────────────────────────
    //
    // Take a `Block`; the caller computes and passes the `span` last, as with the operand-bearing constructors.

    /// `do { ... }`.
    pub fn do_block(block: Block, span: Span) -> Expr {
        Expr::new(ExprKind::DoBlock(block), span)
    }

    /// `eval { ... }`.
    pub fn eval_block(block: Block, span: Span) -> Expr {
        Expr::new(ExprKind::EvalBlock(block), span)
    }

    /// `sub { ... }` — anonymous sub.
    pub fn anon_sub(proto: Option<String>, attrs: Vec<Attribute>, sig: Option<Signature>, block: Block, span: Span) -> Expr {
        Expr::new(ExprKind::AnonSub(proto, attrs, sig, block), span)
    }

    /// `method { ... }` — anonymous method (5.38+).
    pub fn anon_method(attrs: Vec<Attribute>, sig: Option<Signature>, block: Block, span: Span) -> Expr {
        Expr::new(ExprKind::AnonMethod(attrs, sig, block), span)
    }
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    // ── Literals ──────────────────────────────────────────────
    IntLit(i64),
    FloatLit(f64),
    StringLit(String),

    /// Version object literal: `v5.36.0`, `v5.26`.
    VersionLit(String),

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

    /// The default variable (`$_`) inserted implicitly by the parser — e.g., when a prototype's `_` slot is omitted
    /// from a call.  Distinct from `ScalarVar("_")`, which represents the scalar *variable* named `_` as written in the
    /// source (and which may be a lexical `my $_` rather than the global default).  At runtime, `DefaultVar` always
    /// refers to the global default; `ScalarVar("_")` follows normal scope rules.
    DefaultVar,

    /// `my $x`, `our ($a, $b)`, `state $x` in expression context.  The Pratt parser handles `= expr` as normal
    /// assignment wrapping this.
    Decl(DeclScope, Vec<VarDecl>),

    /// `local LVALUE` — localize any lvalue (hash elem, glob, etc.).
    Local(Box<Expr>),

    // ── Binary operations ─────────────────────────────────────
    BinOp(BinOp, Box<Expr>, Box<Expr>),

    /// Chained comparison: `$x < $y <= $z` → ops [<, <=], operands [x, y, z].  Semantics: compare each adjacent pair,
    /// results implicitly ANDed, interior operands evaluated at most once.  `operands.len() == ops.len() + 1`.
    ChainedCmp(Vec<BinOp>, Vec<Expr>),

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

    /// `%array[$idx1, $idx2]` — index/value array slice (5.20+).
    KvArraySlice(Box<Expr>, Vec<Expr>),

    /// `%hash{$k1, $k2}` — key/value hash slice (5.20+).
    KvHashSlice(Box<Expr>, Vec<Expr>),

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

    /// `sub { ... }` — anonymous sub.  Fields: prototype (raw bytes), signature (parsed 5.20+ signatures syntax), body.
    /// Prototype, attributes, and signature are parsed per the `signatures` feature: with signatures, attrs come before
    /// the paren-form; without, prototype comes before attrs.
    AnonSub(Option<String>, Vec<Attribute>, Option<Signature>, Block),

    /// `method { ... }` — anonymous method (5.38+ Corinna).
    AnonMethod(Vec<Attribute>, Option<Signature>, Block),

    // ── Calls ─────────────────────────────────────────────────
    /// Named function call: `foo(...)` or `foo ...`.
    FuncCall(String, Vec<Expr>),

    /// Method call: `$obj->method(...)`.
    MethodCall(Box<Expr>, String, Vec<Expr>),

    /// Indirect method call: `new Foo(...)` → invocant, method, args.
    IndirectMethodCall(Box<Expr>, String, Vec<Expr>),

    // ── Bareword ──────────────────────────────────────────────
    /// A bare identifier not followed by `(` — class name, constant, or bareword.  The parser doesn't resolve which;
    /// that's the compiler's job.
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

    /// `__PACKAGE__` — name of the package in effect when this expression was parsed.  Filled by the parser from its
    /// `current_package` state.
    CurrentPackage(String),

    /// `__SUB__` — reference to the current subroutine, or `undef` if outside any sub.  Resolved at runtime; no
    /// compile-time data.  Emitted only when the `current_sub` feature is active; otherwise the token falls through as
    /// a bareword.
    CurrentSub,

    /// `__CLASS__` — name of the class being constructed during field initializers and ADJUST blocks (5.38+, Corinna).
    /// Resolved at runtime (may differ from the compile-time class if a subclass inherits the field).
    CurrentClass,

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

/// The operand of a stat-family operation: filetest operators (`-e`, `-f`, `-d`, etc.), `stat`, and `lstat`.
///
/// All three share the Perl convention that a bare `_` means "reuse the cached stat buffer from the most recent `stat`,
/// `lstat`, or filetest."
#[derive(Clone, Debug)]
pub enum StatTarget {
    /// An expression: `-f $file`, `-d "/tmp"`, `stat $fh`, or stacked filetests like `-f -r $file`.
    Expr(Box<Expr>),

    /// The bare `_` filehandle — reuse the cached stat buffer from the most recent `stat`, `lstat`, or filetest call.
    StatCache,

    /// Implicit `$_` — when no operand is given (`-e;`).
    Default,
}

/// A sequence of interpolated parts — used for strings, regex patterns, and substitution replacements.
#[derive(Clone, Debug)]
pub struct Interpolated(pub Vec<InterpPart>);

impl Interpolated {
    /// If this contains only constant parts (no runtime interpolation), return the plain string.  `Const` and
    /// `NamedChar` segments are both constant — `NamedChar` is resolved at lex time.  Returns `Some("")` for empty.
    pub fn as_plain_string(&self) -> Option<String> {
        if self.0.is_empty() {
            return Some(String::new());
        }
        let mut result = String::new();
        for part in &self.0 {
            match part {
                InterpPart::Const(s) => result.push_str(s),
                InterpPart::NamedChar { codepoint, .. } => {
                    if let Some(c) = char::from_u32(*codepoint) {
                        result.push(c);
                    } else {
                        return None; // extended UTF-8 above U+10FFFF
                    }
                }
                _ => return None, // has runtime interpolation
            }
        }
        Some(result)
    }
}

/// Part of an interpolated value (§7.3).
#[derive(Clone, Debug)]
pub enum InterpPart {
    Const(String),

    /// `$var`, `$var[0]`, `$var->{k}`, `$var->[0]{k}`, etc.  Wraps the full subscripted expression — not just a name —
    /// so chained subscripts (`$h->{k}[0]->{x}`) are parsed into real AST rather than stringified literally.
    ScalarInterp(Box<Expr>),

    /// `@var`, `@var[1..3]`, `@var{'a','b'}` — whole-array or slice interpolation.  Like `ScalarInterp`, holds the full
    /// expression.
    ArrayInterp(Box<Expr>),

    ExprInterp(Box<Expr>),

    /// `(?{code})` — raw text for stringification + parsed code.
    RegexCode(String, Box<Expr>),

    /// `(??{code})` — postponed regex code block.
    RegexCondCode(String, Box<Expr>),

    /// `\N{CHARNAME}` or `\N{U+XXXX}` — named Unicode character.  Preserves the original name for tooling (formatters,
    /// linters) while storing the resolved code point.  The string content already contains the resolved character in
    /// the surrounding `Const` segments; this variant exists for round-trip fidelity and will be emitted when the body
    /// scanner is reworked to produce separate segments for named character escapes.
    NamedChar {
        name: String,
        codepoint: u32,
    },
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

    /// `~~` — smartmatch (experimental, feature-gated).
    SmartMatch,

    // Logical
    And,
    Or,
    DefinedOr,

    /// `^^` — logical exclusive or.
    LogicalXor,
    LowAnd,
    LowOr,
    LowXor,

    // Bitwise
    BitAnd,
    BitOr,
    BitXor,

    /// `&.` — string-bitwise AND (feature 'bitwise').
    StringBitAnd,

    /// `|.` — string-bitwise OR.
    StringBitOr,

    /// `^.` — string-bitwise XOR.
    StringBitXor,

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
    StringBitNot, // ~. (feature 'bitwise')
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

    /// `$ref->%[indices]` — key/value pairs from an array (indices paired with values).
    KvSliceIndices(Box<Expr>),

    /// `$ref->%{keys}` — key/value pairs from a hash.
    KvSliceKeys(Box<Expr>),

    /// `$obj->$method(args)` dynamic method dispatch.
    DynMethod(Box<Expr>, Vec<Expr>),

    /// `$ref->$#*` — postfix last-index of an array reference, equivalent to `$#{$ref}`.  Requires coordinated lexer
    /// handling: the sequence `$#*` after `->` is consumed as a unit because the lexer otherwise treats the `#` as the
    /// start of a comment.
    LastIndex,
}

impl ArrowTarget {
    // Constructors for the boxed-payload variants: each boxes its `Expr` payload internally so call sites never write
    // `Box::new`.  Nullary variants (`DerefArray`, `DerefHash`, etc.) carry no payload and are used directly.

    /// `$base->[index]` — arrow array element.
    pub fn array_elem(index: Expr) -> ArrowTarget {
        ArrowTarget::ArrayElem(Box::new(index))
    }

    /// `$base->{key}` — arrow hash element.
    pub fn hash_elem(key: Expr) -> ArrowTarget {
        ArrowTarget::HashElem(Box::new(key))
    }

    /// `$base->@[indices]` — arrow array slice by positions.
    pub fn array_slice_indices(indices: Expr) -> ArrowTarget {
        ArrowTarget::ArraySliceIndices(Box::new(indices))
    }

    /// `$base->@{keys}` — arrow slice of a hash returning values as an array.
    pub fn array_slice_keys(keys: Expr) -> ArrowTarget {
        ArrowTarget::ArraySliceKeys(Box::new(keys))
    }

    /// `$base->%[indices]` — arrow key/value pairs from an array.
    pub fn kv_slice_indices(indices: Expr) -> ArrowTarget {
        ArrowTarget::KvSliceIndices(Box::new(indices))
    }

    /// `$base->%{keys}` — arrow key/value pairs from a hash.
    pub fn kv_slice_keys(keys: Expr) -> ArrowTarget {
        ArrowTarget::KvSliceKeys(Box::new(keys))
    }

    /// `$obj->$method(args)` — dynamic method dispatch.
    pub fn dyn_method(method: Expr, args: Vec<Expr>) -> ArrowTarget {
        ArrowTarget::DynMethod(Box::new(method), args)
    }
}

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
    When,
}

/// Phaser block kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhaserKind {
    Begin,
    End,
    Init,
    Check,
    Unitcheck,

    /// `ADJUST { ... }` — runs during object construction (5.38+, Corinna).
    Adjust,
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

    /// Attributes on the variable declaration: `my $x : Foo`.
    pub attributes: Vec<Attribute>,

    /// Reference-declaration form: `my \$x` binds `$x` as an alias (via the `declared_refs` feature, 5.26+).  The RHS
    /// of the enclosing assignment must be a matching reference.  When `false`, this is a normal copy-initialized
    /// variable.
    pub is_ref: bool,
}

/// Subroutine declaration.
#[derive(Clone, Debug)]
pub struct SubDecl {
    pub name: String,

    /// Lexical scope for `my sub`, `state sub`, `our sub`.  `None` for regular package subs.
    pub scope: Option<DeclScope>,

    /// Paren-form prototype from pre-signatures Perl (e.g. `($$)`, `(\@\%)`).  Stored as raw bytes.  Mutually exclusive
    /// with `signature` (the `signatures` feature controls which path parses the paren-form).  A `:prototype(...)`
    /// attribute shows up in `attributes` and coexists with either.
    pub prototype: Option<String>,

    pub attributes: Vec<Attribute>,

    /// Parsed parameter signature from 5.20+ signatures syntax.  Present when the `signatures` feature is active at the
    /// declaration site.
    pub signature: Option<Signature>,

    pub body: Block,
    pub span: Span,
}

/// Parsed subroutine signature (the `signatures` feature).
///
/// Each parameter is one of several `SigParam` variants: named scalar (optionally with a default), slurpy array, slurpy
/// hash, or an anonymous placeholder that accepts and discards a value.
#[derive(Clone, Debug)]
pub struct Signature {
    pub params: Vec<SigParam>,
    pub span: Span,
}

/// One parameter in a signature.
#[derive(Clone, Debug)]
pub enum SigParam {
    /// `$name`, `$name = DEFAULT`, `$name //= DEFAULT`, `$name ||= DEFAULT`.  Positional.  When `default` is `None`,
    /// the parameter is required.
    Scalar { name: String, default: Option<(SigDefaultKind, Expr)>, span: Span },

    /// `@name` — slurpy, captures all remaining positional arguments.  Must appear last if at all.
    SlurpyArray { name: String, span: Span },

    /// `%name` — slurpy, captures remaining name/value pairs.  Must appear last if at all.
    SlurpyHash { name: String, span: Span },

    /// `$` — anonymous scalar placeholder; accepts a value without binding it.  Optional `default` for `$ = expr` or
    /// `$=` forms.
    AnonScalar { default: Option<(SigDefaultKind, Expr)>, span: Span },

    /// `@` — anonymous slurpy array (consumes remaining positional args without binding).
    AnonArray { span: Span },

    /// `%` — anonymous slurpy hash.
    AnonHash { span: Span },
}

/// The kind of default-value operator in a signature parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigDefaultKind {
    /// `= expr` — use default when argument is not provided.
    Eq,

    /// `//= expr` — use default when argument is missing or undef (5.38+).
    DefinedOr,

    /// `||= expr` — use default when argument is missing or false (5.38+).
    LogicalOr,
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
/// When `vars` is empty, the loop uses implicit `$_`.
/// When `vars` has one element, it's `for my $x (LIST)`.
/// When `vars` has multiple elements, it's `for my ($x, $y) (LIST)` (5.36+).
#[derive(Clone, Debug)]
pub struct ForEachStmt {
    pub vars: Vec<VarDecl>,
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
/// `lines` captures every source line of the body in order, classified into one of the four `FormatLine` variants.
/// Picture lines are already paired with their argument expressions.
#[derive(Clone, Debug)]
pub struct FormatDecl {
    pub name: String,
    pub lines: Vec<FormatLine>,
    pub span: Span,
}

/// One line of a format body.
#[derive(Clone, Debug)]
pub enum FormatLine {
    /// `# ...` — comment, not rendered.  Stored without the leading `#` or surrounding whitespace; the full source is
    /// available via `span`.
    Comment { text: String, span: Span },

    /// Empty or whitespace-only line; renders as a blank line of output.
    Blank { span: Span },

    /// A picture line containing no field specifiers.  The text is stored with any `~`/`~~` characters already replaced
    /// with spaces (so the output width matches the source layout); the `repeat` field records the original repeat
    /// behavior.
    Literal { repeat: RepeatKind, text: String, span: Span },

    /// A picture line containing at least one field.  Arguments come from the source line immediately following the
    /// picture: one expression per field in order.  When the argument line begins with `{`, expressions may span
    /// multiple source lines until the matching `}`.
    Picture { repeat: RepeatKind, parts: Vec<FormatPart>, args: Vec<Expr>, span: Span },
}

/// One piece of a picture line.  Literals and fields interleave in source order.
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

/// `class Name VERSION :attrs { ... }` (5.38+ Corinna).
#[derive(Clone, Debug)]
pub struct ClassDecl {
    pub name: String,
    pub version: Option<String>,
    pub attributes: Vec<Attribute>,

    /// `None` for statement form (`class Foo;`).
    pub body: Option<Block>,

    pub span: Span,
}

/// `field $var :attrs = default;` (inside class).
#[derive(Clone, Debug)]
pub struct FieldDecl {
    pub var: VarDecl,
    pub attributes: Vec<Attribute>,

    /// Default expression with operator kind (`=`, `//=`, `||=`).
    pub default: Option<(SigDefaultKind, Expr)>,

    pub span: Span,
}
