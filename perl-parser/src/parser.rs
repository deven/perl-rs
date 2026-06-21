//! Pratt parser with recursive descent for statements (§6).
//!
//! Expression assembly uses precedence climbing.  Statements, declarations, blocks, and top-level forms use ordinary
//! recursive descent that calls `parse_expr` where expressions are needed.

use crate::ast::*;
use crate::error::ParseError;
use crate::keyword::{self, Keyword};
use crate::lexer::Lexer;
use crate::pragma::{Features, Pragmas, resolve_feature_name};
use crate::span::Span;
use crate::symbol::{ProtoSlot, SubPrototype, SymbolTable};
use crate::token::*;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/parser_tests.rs"]
mod tests;

/// Precedence levels (u8 with gaps for plugin insertion).  Maps to Perl 5's precedence table from perly.y.
pub type Precedence = u16;

/// Continuation frame for the iterative expression parser.  Each frame captures what to do with a sub-expression after
/// the forward phase (which consumed a prefix operator or opening delimiter) completes.
enum ExprFrame {
    /// Simple prefix unary: wrap result in UnaryOp.
    Unary { op: UnaryOp, span: Span, min_prec: Precedence },

    /// Negate prefix: `-expr`, with string negation collapse.
    Negate { span: Span, min_prec: Precedence },

    /// Reference: `\expr`.
    Ref { span: Span, min_prec: Precedence },

    /// `local expr` — dynamically scopes any lvalue.
    Local { span: Span, min_prec: Precedence },

    /// Prefix `++`/`--` with lvalue validation.
    PreIncDec { op: UnaryOp, span: Span, min_prec: Precedence },

    /// Parenthesized expression: expect `)`, wrap in Paren, check for postfix subscript.
    Paren { span: Span, min_prec: Precedence },

    /// Anonymous array ref `[elem, ...]`: accumulating elements.
    ArrayRef { elems: Vec<Expr>, span: Span, min_prec: Precedence },

    /// Anonymous hash constructor `{key => val, ...}`: accumulating elements.
    HashRef { elems: Vec<Expr>, span: Span, min_prec: Precedence },

    /// Dereference block: `${expr}`, `@{expr}`, `%{expr}`, `&{expr}`, `*{expr}`.
    DerefBlock { sigil: Sigil, span: Span, min_prec: Precedence },

    /// `eval EXPR` (not eval BLOCK).
    EvalExpr { span: Span, min_prec: Precedence },

    /// `do EXPR` (not do BLOCK).
    DoExpr { span: Span, min_prec: Precedence },

    /// `return EXPR`.
    ReturnExpr { span: Span, min_prec: Precedence },

    /// `goto EXPR`.
    GotoExpr { span: Span, min_prec: Precedence },

    /// `dump EXPR`.
    DumpExpr { span: Span, min_prec: Precedence },
}

/// Result of `try_prefix`: either a frame to push (prefix op consumed, continue looking for more), a complete leaf
/// expression (no further prefix processing needed), or None (not a prefix — proceed to `parse_term`).
enum PrefixResult {
    /// Consumed a prefix op; push this frame and continue.  The second element is the inner precedence for the
    /// sub-expression.
    Frame(ExprFrame, Precedence),

    /// Consumed a complete leaf expression (e.g. filetest, -bareword).
    Leaf(Expr),
}

/// Result of applying a continuation frame.
enum FrameResult {
    /// Frame fully applied — here's the result expression and the outer precedence to restore.
    Done(Expr, Precedence),

    /// Frame needs more elements (array/hash accumulation) — re-push and re-enter the forward phase.
    Continue(ExprFrame, Precedence),
}

// ── Precedence constants ──────────────────────────────────────
// Multiples of 100 to allow plugin operators at intermediate levels (99 slots between each pair).

const PREC_LOW: Precedence = 0; // statement boundary
const PREC_OR_LOW: Precedence = 100; // or xor
const PREC_AND_LOW: Precedence = 200; // and
const PREC_NOT_LOW: Precedence = 300; // not (prefix)
#[allow(dead_code)]
const PREC_LIST: Precedence = 400; // list operators
const PREC_COMMA: Precedence = 500; // , =>
const PREC_ASSIGN: Precedence = 600; // = += -= etc.
const PREC_TERNARY: Precedence = 700; // ?:
const PREC_RANGE: Precedence = 800; // .. ...
const PREC_OR: Precedence = 900; // || // ^^
const PREC_AND: Precedence = 1000; // &&
const PREC_BIT_OR: Precedence = 1100; // | ^
const PREC_BIT_AND: Precedence = 1200; // &
const PREC_EQ: Precedence = 1300; // == != eq ne <=> cmp
const PREC_REL: Precedence = 1400; // < > <= >= lt gt le ge

/// `isa` — class-instance infix operator (5.32+).  Non-associative.  Tighter than relational, looser than named unary:
/// `$x isa Foo < 1` parses as `($x isa Foo) < 1`, while `foo $x isa Bar` parses as `foo($x isa Bar)`.
const PREC_ISA: Precedence = 1500;

/// Named unary operators and prototyped subs with a scalar-ish slot (`$`, `_`, `+`, `\X`, `\[...]`, etc.).  Sits
/// between isa and shift: `foo $a < 1` parses as `foo($a) < 1`, while `foo $a << 1` parses as `foo($a << 1)`.
/// Non-associative.
const PREC_NAMED_UNARY: Precedence = 1600;
const PREC_SHIFT: Precedence = 1700; // << >>
const PREC_ADD: Precedence = 1800; // + - .
const PREC_MUL: Precedence = 1900; // * / % x
const PREC_BINDING: Precedence = 2000; // =~ !~
const PREC_UNARY: Precedence = 2100; // ! ~ \ - + (prefix)
const PREC_POW: Precedence = 2200; // **
const PREC_INC: Precedence = 2300; // ++ -- (postfix)
const PREC_ARROW: Precedence = 2400; // ->

#[derive(Clone, Copy, Debug, PartialEq)]
enum Assoc {
    Left,
    Right,
    Non,
    Chain,
}

#[derive(Clone, Copy, Debug)]
struct OpInfo {
    prec: Precedence,
    assoc: Assoc,
}

impl OpInfo {
    fn left_prec(self) -> Precedence {
        self.prec
    }

    fn right_prec(self) -> Precedence {
        match self.assoc {
            Assoc::Left | Assoc::Non | Assoc::Chain => self.prec + 1,
            Assoc::Right => self.prec,
        }
    }
}

/// The combined parser/lexer.
pub struct Parser {
    lexer: Lexer,

    /// Cached current token.  `None` means no token is cached — the next peek/next will lex one.
    current: Option<Spanned>,

    /// Stored lexer error — surfaced by next_token().
    lexer_error: Option<ParseError>,

    /// Symbol table of all packages, subs, and imports seen so far.  Populated as sub declarations and (eventually)
    /// `use` statements are parsed; consulted at call sites for prototype-aware argument parsing.
    symbols: SymbolTable,

    /// Name of the package currently being parsed.  Updated by `package Name;` and the block form `package Name
    /// { ... }`.
    current_package: std::sync::Arc<str>,

    /// Lexically-scoped pragma state (`use feature`, `use utf8`, version bundles).  Saved/restored across block
    /// boundaries by `parse_block`.
    pragmas: Pragmas,
}

impl Parser {
    // ── Construction ──────────────────────────────────────────
    pub fn new(src: &[u8]) -> Result<Self, ParseError> {
        Self::from_lexer(Lexer::new(src))
    }

    /// Construct a parser that reports `filename` for `__FILE__` resolution and in diagnostic messages.  Prefer this
    /// over [`Self::new`] when the source comes from a named file.
    pub fn with_filename(src: &[u8], filename: impl Into<String>) -> Result<Self, ParseError> {
        Self::from_lexer(Lexer::with_filename(src, filename))
    }

    /// Shared core: all constructors funnel through here so field initialization stays in one place.
    fn from_lexer(lexer: Lexer) -> Result<Self, ParseError> {
        Ok(Parser {
            lexer,
            current: None,
            lexer_error: None,
            symbols: SymbolTable::new(),
            current_package: std::sync::Arc::from("main"),
            pragmas: Pragmas::new(),
        })
    }

    /// Read-only access to the accumulated symbol table.  Primarily for tests and future cross-pass consumers.
    pub fn symbols(&self) -> &SymbolTable {
        &self.symbols
    }

    /// Read-only access to the current lexical pragma state.  Primarily for tests and future parsing-behavior dispatch
    /// (signatures vs. prototypes, postderef enablement, etc.).
    pub fn pragmas(&self) -> &Pragmas {
        &self.pragmas
    }

    /// Override the active feature set.  Updates both the parser's pragma state and the lexer's cached copy.  Primarily
    /// for tests that need to parse under a specific feature bundle without a `use feature` declaration in the source.
    pub fn set_features(&mut self, features: Features) {
        self.pragmas.features = features;
        self.lexer.features = features;
    }

    // ── Token access ──────────────────────────────────────────
    /// Peek at the current token without consuming it.  Lexes on demand if no token is cached.
    fn peek_token(&mut self) -> &Token {
        if self.current.is_none() {
            self.current = Some(match self.lexer.lex_token() {
                Ok(s) => s,
                Err(e) => {
                    self.lexer_error = Some(e);
                    Spanned { token: Token::Eof, span: Span::new(self.lexer.pos() as u32, self.lexer.pos() as u32) }
                }
            });
        }

        // self.current is Some by construction above.
        match &self.current {
            Some(s) => &s.token,
            None => unreachable!("peek_token: current is Some"),
        }
    }

    /// Peek at the span of the current token.
    fn peek_span(&mut self) -> Span {
        self.peek_token();
        match &self.current {
            Some(s) => s.span,
            None => unreachable!("peek_span: peek_token ensures current is Some"),
        }
    }

    /// Consume and return the current token.
    fn next_token(&mut self) -> Result<Spanned, ParseError> {
        self.peek_token();
        if let Some(e) = self.lexer_error.take() {
            return Err(e);
        }
        match self.current.take() {
            Some(s) => Ok(s),
            None => unreachable!("next_token: peek_token ensures current is Some"),
        }
    }

    fn expect_token(&mut self, expected: &Token) -> Result<Spanned, ParseError> {
        if let Some(e) = self.lexer_error.take() {
            return Err(e);
        }
        if self.peek_token() == expected {
            self.next_token()
        } else {
            let msg = format!("expected {expected}, got {}", self.peek_token());
            let span = self.peek_span();
            Err(ParseError::new(msg, span))
        }
    }

    fn eat(&mut self, token: &Token) -> Result<bool, ParseError> {
        if self.at(token)? {
            self.next_token()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn at(&mut self, token: &Token) -> Result<bool, ParseError> {
        if let Some(e) = self.lexer_error.take() {
            return Err(e);
        }
        Ok(self.peek_token() == token)
    }

    fn at_eof(&mut self) -> Result<bool, ParseError> {
        if let Some(e) = self.lexer_error.take() {
            return Err(e);
        }
        Ok(matches!(self.peek_token(), Token::Eof))
    }

    // ── Flag validation ───────────────────────────────────────
    /// Validate regex modifier flags.  Returns an error for any unrecognized modifier character.
    fn validate_regex_flags(flags: &str, span: Span) -> Result<(), ParseError> {
        for ch in flags.chars() {
            if !"msixpogcadlun".contains(ch) {
                return Err(ParseError::new(format!("Unknown regexp modifier \"/{ch}\""), span));
            }
        }
        Ok(())
    }

    /// Validate substitution modifier flags.  Includes regex flags plus `e` (eval replacement) and `r` (non-
    /// destructive).
    fn validate_subst_flags(flags: &str, span: Span) -> Result<(), ParseError> {
        for ch in flags.chars() {
            if !"msixpogcadluner".contains(ch) {
                return Err(ParseError::new(format!("Unknown regexp modifier \"/{ch}\""), span));
            }
        }
        Ok(())
    }

    /// Validate transliteration modifier flags.  Returns an error for any unrecognized modifier character.
    fn validate_tr_flags(flags: &str, span: Span) -> Result<(), ParseError> {
        for ch in flags.chars() {
            if !"cdsr".contains(ch) {
                return Err(ParseError::new(format!("Unknown transliteration modifier \"/{ch}\""), span));
            }
        }
        Ok(())
    }

    // ── Public entry point ────────────────────────────────────
    /// Parse a complete program.
    pub fn parse_program(&mut self) -> Result<Program, ParseError> {
        let start = self.peek_span();
        let mut statements = Vec::new();

        while !self.at_eof()? {
            let stmt = self.parse_statement()?;
            statements.push(stmt);
        }

        // If __DATA__/__END__ triggered logical EOF, the lexer stored the keyword and data offset.  Emit a DataEnd
        // node so the compiler knows where the DATA filehandle content begins.
        if let Some((kw, offset)) = self.lexer.data_end_info.take() {
            let end_span = self.peek_span();
            statements.push(Statement { kind: StmtKind::DataEnd(kw, offset), span: end_span, terminated: false });
        }

        // A lexer error produces Eof, which exits the loop above.  If advance() was never called to surface it, catch
        // it here.
        if let Some(e) = self.lexer_error.take() {
            return Err(e);
        }

        let end = self.peek_span();
        Ok(Program { statements, span: start.merge(end) })
    }

    // ── Statement parsing ─────────────────────────────────────
    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        let start = self.peek_span();

        // Empty statement
        if self.eat(&Token::Semi)? {
            return Ok(Statement { kind: StmtKind::Empty, span: start, terminated: true });
        }

        let (kind, terminated) = match self.peek_token().clone() {
            // Statement-level keywords: consume first, then dispatch to handler.  Fat-comma autoquoting
            // (e.g. `if => 1`) is handled by the lexer, which returns StrLit instead of Keyword.
            Token::Keyword(kw) if keyword::is_statement_keyword(kw) => {
                let kw_span = self.peek_span();
                self.next_token()?; // consume the keyword
                match kw {
                    // my/our/state are expressions, not statements.  The keyword has already been consumed; construct
                    // the Decl expression and run the Pratt loop to pick up optional `= expr` assignment and trailing
                    // `, $other` list members.
                    Keyword::My | Keyword::Our | Keyword::State => {
                        let scope = match kw {
                            Keyword::My => DeclScope::My,
                            Keyword::Our => DeclScope::Our,
                            Keyword::State => DeclScope::State,
                            _ => unreachable!(),
                        };

                        // `my sub foo { }`, `state sub foo { }`, `our sub foo { }`
                        if self.eat(&Token::Keyword(Keyword::Sub))? {
                            if matches!(self.peek_token(), Token::Ident(_) | Token::Keyword(_)) {
                                let (kind, _) = (self.parse_sub_decl_body(kw_span)?, false);
                                // Patch the scope onto the SubDecl.
                                let kind = match kind {
                                    StmtKind::SubDecl(mut sd) => {
                                        sd.scope = Some(scope);
                                        StmtKind::SubDecl(sd)
                                    }
                                    other => other,
                                };
                                (kind, false)
                            } else {
                                return Err(ParseError::new("expected sub name after my/our/state sub", self.peek_span()));
                            }

                        // `my method foo { }`, `state method foo { }`
                        } else if self.eat(&Token::Keyword(Keyword::Method))? {
                            let kind = self.parse_method(kw_span)?;
                            let kind = match kind {
                                StmtKind::MethodDecl(mut sd) => {
                                    sd.scope = Some(scope);
                                    StmtKind::MethodDecl(sd)
                                }
                                other => other,
                            };
                            (kind, false)
                        } else {
                            let initial = self.parse_decl_expr(scope, kw_span)?;
                            let expr = self.parse_expr_continuation(initial, PREC_LOW)?;
                            let kind = self.maybe_postfix_control(expr)?;
                            let terminated = self.eat(&Token::Semi)?;
                            (kind, terminated)
                        }
                    }
                    Keyword::Sub => {
                        if matches!(self.peek_token(), Token::Ident(_) | Token::Keyword(_)) {
                            (self.parse_sub_decl_body(kw_span)?, false)
                        } else {
                            let expr = self.parse_anon_sub(kw_span)?;
                            let kind = self.maybe_postfix_control(expr)?;
                            let terminated = self.eat(&Token::Semi)?;
                            (kind, terminated)
                        }
                    }
                    Keyword::If => (self.parse_if_stmt()?, false),
                    Keyword::Unless => (self.parse_unless_stmt()?, false),
                    Keyword::While => (self.parse_while_stmt()?, false),
                    Keyword::Until => (self.parse_until_stmt()?, false),
                    Keyword::For | Keyword::Foreach => (self.parse_for_stmt()?, false),
                    Keyword::Package => (self.parse_package_decl(kw_span)?, false),
                    Keyword::Use | Keyword::No => (self.parse_use_decl(kw_span, kw == Keyword::No)?, false),

                    // Phaser blocks
                    Keyword::BEGIN => (self.parse_phaser(PhaserKind::Begin)?, false),
                    Keyword::END => (self.parse_phaser(PhaserKind::End)?, false),
                    Keyword::INIT => (self.parse_phaser(PhaserKind::Init)?, false),
                    Keyword::CHECK => (self.parse_phaser(PhaserKind::Check)?, false),
                    Keyword::UNITCHECK => (self.parse_phaser(PhaserKind::Unitcheck)?, false),
                    Keyword::ADJUST => (self.parse_phaser(PhaserKind::Adjust)?, false),

                    // AUTOLOAD/DESTROY — implicit sub declarations.  `AUTOLOAD { ... }` is `sub AUTOLOAD { ... }`.
                    // `AUTOLOAD;` is `sub AUTOLOAD;` (forward decl).  `AUTOLOAD()` is `sub AUTOLOAD ();` (prototype).
                    // They are NEVER function calls — Perl always treats them as implicit sub declarations.
                    Keyword::AUTOLOAD | Keyword::DESTROY => {
                        let name: &str = kw.into();
                        (self.parse_sub_decl_with_name(name.to_string(), kw_span)?, false)
                    }

                    // given/when/default
                    Keyword::Given => (self.parse_given()?, false),
                    Keyword::When => (self.parse_when()?, false),
                    Keyword::Default => {
                        let block = self.parse_block()?;
                        (StmtKind::When(Expr::new(ExprKind::IntLit(1), kw_span), block), false)
                    }

                    // try/catch/finally/defer
                    Keyword::Try => (self.parse_try()?, false),
                    Keyword::Defer => {
                        let block = self.parse_block()?;
                        (StmtKind::Defer(block), false)
                    }

                    // format NAME = ... .
                    Keyword::Format => (self.parse_format(kw_span)?, false),

                    // class Name :attrs { ... }
                    Keyword::Class => (self.parse_class(kw_span)?, false),

                    // field $var :attrs = default;
                    Keyword::Field => (self.parse_field(kw_span)?, false),

                    // method name(params) { ... } or method { ... }
                    Keyword::Method => {
                        if matches!(self.peek_token(), Token::Ident(_)) {
                            (self.parse_method(kw_span)?, false)
                        } else {
                            let expr = self.parse_anon_method(kw_span)?;
                            let kind = self.maybe_postfix_control(expr)?;
                            let terminated = self.eat(&Token::Semi)?;
                            (kind, terminated)
                        }
                    }

                    // Any other statement keyword not handled above.
                    _ => unreachable!("unhandled statement keyword: {kw:?}"),
                }
            }

            // Expression keywords (local, return, etc.) and non-keywords go through parse_expr_statement.
            Token::Keyword(Keyword::Local) => self.parse_expr_statement()?,

            // `{` at statement level — parse as block, then check if it should be reclassified as a hash constructor.
            Token::LeftBrace => {
                let block = self.parse_block()?;
                match Self::try_reclassify_as_hash(block) {
                    Ok(hash_expr) => {
                        // Reclassified as hash constructor.  Continue as an expression statement: check for postfix
                        // control flow and optional semicolon.
                        let kind = self.maybe_postfix_control(hash_expr)?;
                        let terminated = self.eat(&Token::Semi)?;
                        (kind, terminated)
                    }
                    Err(block) => {
                        let cont = if self.eat(&Token::Keyword(Keyword::Continue))? { Some(self.parse_block()?) } else { None };
                        (StmtKind::Block(block, cont), false)
                    }
                }
            }

            // Identifier: could be a label (IDENT:) or start of expression.
            Token::Ident(_) => {
                let ident_span = self.peek_span();
                let name = match self.next_token()?.token {
                    Token::Ident(n) => n,
                    _ => unreachable!(),
                };
                if matches!(self.peek_token(), Token::Colon) {
                    // Label: consume ':' and parse the labeled statement.
                    self.next_token()?;
                    let stmt = self.parse_statement()?;
                    (StmtKind::Labeled(name, Box::new(stmt)), false)
                } else {
                    // Expression starting with an identifier.
                    let initial = self.parse_ident_term(name, ident_span)?;
                    let expr = self.parse_expr_continuation(initial, PREC_LOW)?;
                    let kind = self.maybe_postfix_control(expr)?;
                    let terminated = self.eat(&Token::Semi)?;
                    (kind, terminated)
                }
            }

            _ => self.parse_expr_statement()?,
        };

        let end = self.peek_span();
        Ok(Statement { kind, span: start.merge(end), terminated })
    }

    fn maybe_postfix_control(&mut self, expr: Expr) -> Result<StmtKind, ParseError> {
        match self.peek_token() {
            Token::Keyword(Keyword::If) => {
                self.next_token()?;
                let cond = self.parse_expr(PREC_LOW)?;
                let span = expr.span.merge(cond.span);
                Ok(StmtKind::Expr(Expr::postfix_control(PostfixKind::If, expr, cond, span)))
            }
            Token::Keyword(Keyword::Unless) => {
                self.next_token()?;
                let cond = self.parse_expr(PREC_LOW)?;
                let span = expr.span.merge(cond.span);
                Ok(StmtKind::Expr(Expr::postfix_control(PostfixKind::Unless, expr, cond, span)))
            }
            Token::Keyword(Keyword::While) => {
                self.next_token()?;
                let cond = self.parse_expr(PREC_LOW)?;
                let span = expr.span.merge(cond.span);
                Ok(StmtKind::Expr(Expr::postfix_control(PostfixKind::While, expr, cond, span)))
            }
            Token::Keyword(Keyword::Until) => {
                self.next_token()?;
                let cond = self.parse_expr(PREC_LOW)?;
                let span = expr.span.merge(cond.span);
                Ok(StmtKind::Expr(Expr::postfix_control(PostfixKind::Until, expr, cond, span)))
            }
            Token::Keyword(Keyword::For) | Token::Keyword(Keyword::Foreach) => {
                self.next_token()?;
                let list = self.parse_expr(PREC_LOW)?;
                let span = expr.span.merge(list.span);
                Ok(StmtKind::Expr(Expr::postfix_control(PostfixKind::For, expr, list, span)))
            }
            Token::Keyword(Keyword::When) => {
                self.next_token()?;
                let cond = self.parse_expr(PREC_LOW)?;
                let span = expr.span.merge(cond.span);
                Ok(StmtKind::Expr(Expr::postfix_control(PostfixKind::When, expr, cond, span)))
            }
            _ => Ok(StmtKind::Expr(expr)),
        }
    }

    // ── Variable declarations ─────────────────────────────────
    fn parse_single_var_decl(&mut self) -> Result<VarDecl, ParseError> {
        let span = self.peek_span();

        // `my \$x` / `my \@a` / `my \%h` — reference declaration (declared_refs, 5.26+).  Only honored when the feature
        // is active; otherwise `\` would be an unexpected token here.
        let is_ref = if self.pragmas.features.contains(Features::DECLARED_REFS) && matches!(self.peek_token(), Token::Backslash) {
            self.next_token()?;
            true
        } else {
            false
        };

        match self.next_token()?.token {
            Token::ScalarVar(name) => Ok(VarDecl { sigil: Sigil::Scalar, name, span, attributes: vec![], is_ref }),
            Token::ArrayVar(name) => Ok(VarDecl { sigil: Sigil::Array, name, span, attributes: vec![], is_ref }),
            Token::HashVar(name) => Ok(VarDecl { sigil: Sigil::Hash, name, span, attributes: vec![], is_ref }),
            Token::Percent => {
                // my %hash — lexer emitted Percent; read the hash name.
                match self.lexer.lex_hash_var_after_percent()? {
                    Some(Token::HashVar(name)) => Ok(VarDecl { sigil: Sigil::Hash, name, span, attributes: vec![], is_ref }),
                    Some(Token::SpecialHashVar(name)) => Ok(VarDecl { sigil: Sigil::Hash, name, span, attributes: vec![], is_ref }),
                    _ => Err(ParseError::new("expected hash variable name after %", span)),
                }
            }
            other => Err(ParseError::new(format!("expected variable, got {other:?}"), span)),
        }
    }

    // ── Sub declaration ───────────────────────────────────────
    /// Parse the body of a named sub declaration after `sub` has already been consumed.  `start` is the span of the
    /// `sub` keyword.
    ///
    /// Registers the sub (with its prototype, if any) in the symbol table before returning, so subsequent call sites
    /// can consult it for prototype-driven argument parsing.
    ///
    /// Prototypes may be declared in two syntactic forms:
    /// * Paren-form after the name: `sub foo ($$) { ... }`.
    /// * Attribute form: `sub foo :prototype($$) { ... }` (Perl 5.20+).
    ///
    /// Both are supported; the attribute form takes precedence if both appear (matching the behavior needed once
    /// signatures are enabled, where the paren form is a signature instead).
    fn parse_sub_decl_body(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        let name = match self.next_token()?.token {
            Token::Ident(name) => name,

            // Keywords are valid sub names: `sub send { }`, `sub print { }`.
            Token::Keyword(kw) => <&str>::from(kw).to_string(),
            other => return Err(ParseError::new(format!("expected sub name, got {other:?}"), start)),
        };
        self.parse_sub_decl_with_name(name, start)
    }

    /// Parse a sub declaration body when the name is already known.  Shared between `sub NAME ...` and implicit sub
    /// forms like `AUTOLOAD { ... }` / `DESTROY { ... }`.
    fn parse_sub_decl_with_name(&mut self, name: String, start: Span) -> Result<StmtKind, ParseError> {
        // Dispatch on the `signatures` feature: when active, the grammar is `sub NAME [ATTRS] [SIGNATURE] BLOCK` —
        // attrs come before the paren-form, which is a signature.  When inactive, the grammar is `sub NAME [PROTO]
        // [ATTRS] BLOCK` — the paren-form is a prototype.
        let signatures_active = self.pragmas.features.contains(Features::SIGNATURES);

        let (prototype_raw, attributes, signature) = if signatures_active {
            let attrs = self.parse_attributes()?;
            let sig = self.parse_signature()?;
            (None, attrs, sig)
        } else {
            let proto = self.parse_prototype()?;
            let attrs = self.parse_attributes()?;
            (proto, attrs, None)
        };

        // The effective prototype (for symbol-table purposes) may come from either the paren form or a
        // `:prototype(...)` attribute.  With signatures active the paren-form is a signature, so only the attribute
        // contributes.
        let effective_proto_raw = attributes.iter().find(|a| a.name == "prototype").and_then(|a| a.value.clone()).or_else(|| prototype_raw.clone());

        let prototype_parsed = match &effective_proto_raw {
            Some(raw) => Some(SubPrototype::parse(raw).map_err(|e| ParseError::new(format!("invalid prototype: {}", e.message), start))?),
            None => None,
        };

        // Forward declaration: `sub name PROTO ATTRS;` with no body.
        if self.eat(&Token::Semi)? {
            let attr_names: Vec<String> = attributes.iter().map(|a| a.name.clone()).collect();
            self.symbols.entry(&self.current_package.clone()).declare_sub(&name, prototype_parsed, attr_names, true);

            // Represent as a SubDecl with an empty body for now; an optional `body: None` variant would be cleaner, but
            // that's a separate AST change.
            let span = start.merge(self.peek_span());
            let body = Block { statements: Vec::new(), span };
            return Ok(StmtKind::SubDecl(SubDecl { name, scope: None, prototype: prototype_raw, attributes, signature, body, span }));
        }

        let body = self.parse_block()?;

        // Register the full definition, replacing any prior forward declaration of the same name.
        let attr_names: Vec<String> = attributes.iter().map(|a| a.name.clone()).collect();
        self.symbols.entry(&self.current_package.clone()).declare_sub(&name, prototype_parsed, attr_names, false);

        Ok(StmtKind::SubDecl(SubDecl { name, scope: None, prototype: prototype_raw, attributes, signature, body, span: start.merge(self.peek_span()) }))
    }

    /// Parse an optional prototype: `($$)`, `(\@\%)`, etc.  If `(` follows, consume it and scan the body as raw bytes
    /// until `)`, matching toke.c's `scan_str()` call in `yyl_sub()`.
    fn parse_prototype(&mut self) -> Result<Option<String>, ParseError> {
        if self.at(&Token::LeftParen)? {
            self.next_token()?; // consume (
            let proto = self.lexer.lex_body_str('(', true)?;
            Ok(Some(proto))
        } else {
            Ok(None)
        }
    }

    /// Parse an optional signature: `($x, $y, $z = default)`, `(@rest)`, etc.  Called instead of
    /// [`Self::parse_prototype`] when the `signatures` feature is active at the declaration site.  Returns `None` when
    /// no paren-form is present.
    ///
    /// Grammar (non-normative):
    ///
    /// ```text
    /// signature     := '(' params? ','? ')'
    /// params        := param (',' param)*
    /// param         := scalar_param | slurpy_param | anon_param
    /// scalar_param  := '$' IDENT ( '=' expr )?
    /// slurpy_param  := ('@'|'%') IDENT
    /// anon_param    := '$' | '@' | '%'
    /// ```
    ///
    /// This parser is permissive about slurpy placement — it accepts slurpy params anywhere and trusts a later semantic
    /// pass to diagnose "slurpy must be last".  Same for duplicate parameter names.
    fn parse_signature(&mut self) -> Result<Option<Signature>, ParseError> {
        if !self.at(&Token::LeftParen)? {
            return Ok(None);
        }
        let open = self.next_token()?; // consume (
        let start_span = open.span;

        let mut params = Vec::new();

        // Track the span of the first slurpy parameter (if any) so we can reject anything that follows it.
        let mut slurpy_span: Option<Span> = None;

        loop {
            if self.at(&Token::RightParen)? {
                break;
            }
            let param = self.parse_sig_param()?;

            // Reject params after a slurpy.
            if let Some(sp) = slurpy_span {
                let offending = match &param {
                    SigParam::Scalar { span, .. } => *span,
                    SigParam::SlurpyArray { span, .. } => *span,
                    SigParam::SlurpyHash { span, .. } => *span,
                    SigParam::AnonScalar { span, .. } => *span,
                    SigParam::AnonArray { span } => *span,
                    SigParam::AnonHash { span } => *span,
                };
                return Err(ParseError::new(format!("parameter after slurpy (slurpy at byte {})", sp.start), offending));
            }

            // Record if this param is a slurpy.
            match &param {
                SigParam::SlurpyArray { span, .. } | SigParam::SlurpyHash { span, .. } | SigParam::AnonArray { span } | SigParam::AnonHash { span } => {
                    slurpy_span = Some(*span);
                }
                _ => {}
            }

            params.push(param);
            if !self.eat(&Token::Comma)? {
                break;
            }

            // Trailing comma is allowed.
        }
        let close = self.expect_token(&Token::RightParen)?;

        Ok(Some(Signature { params, span: start_span.merge(close.span) }))
    }

    /// Parse one signature parameter.  Handles the parser/lexer interplay for sigils:
    ///
    /// * `Token::ScalarVar(name)` / `Token::ArrayVar(name)` arrive pre-combined because the lexer greedily consumes
    ///   `$ident` / `@ident`.
    /// * `Token::HashVar` does NOT arrive pre-combined — the lexer always emits `Token::Percent` and the parser opts in
    ///   via `lex_hash_var_after_percent()` when in term position.  We do that here.
    /// * Bare `$`/`@`/`%` (followed by a non-identifier) arrive as `Token::Dollar` / `Token::At` / `Token::Percent`
    ///   respectively, and mean anonymous placeholders.
    /// * `$,`/`$)`/`$;` and similar get eagerly lexed as `Token::SpecialVar(c)` because those are real punctuation
    ///   variables.  In a signature, `$` followed by a separator is an anonymous scalar; we split the SpecialVar back
    ///   into a `Dollar` + synthetic delimiter.
    fn parse_sig_param(&mut self) -> Result<SigParam, ParseError> {
        // Intercept `SpecialVar(c)` where `c` is a signature separator or `=` — splits into anon scalar + delimiter.
        if let Token::SpecialVar(name) = self.peek_token()
            && name.len() == 1
            && matches!(name.as_bytes()[0], b',' | b')' | b'=')
        {
            let tok = self.next_token()?;
            let span = tok.span;
            let delim_byte = match tok.token {
                Token::SpecialVar(n) => n.as_bytes()[0],
                _ => unreachable!(),
            };
            if delim_byte == b'=' {
                // `$=` — nameless optional.  Check for default expr.
                if self.at(&Token::RightParen)? || self.at(&Token::Comma)? {
                    // `$=)` or `$=,` — no default expression.
                    return Ok(SigParam::AnonScalar { default: Some((SigDefaultKind::Eq, Expr::new(ExprKind::Undef, span))), span });
                }

                // `$ = expr` — has default expression.
                let default_expr = self.parse_expr(PREC_COMMA + 1)?;
                return Ok(SigParam::AnonScalar { default: Some((SigDefaultKind::Eq, default_expr)), span: span.merge(self.peek_span()) });
            }
            let delim_tok = match delim_byte {
                b',' => Token::Comma,
                b')' => Token::RightParen,
                _ => unreachable!(),
            };

            // Push the delimiter into the lookahead cache so the outer loop in parse_signature sees it next.
            self.current = Some(Spanned { token: delim_tok, span });
            return Ok(SigParam::AnonScalar { default: None, span });
        }

        let tok = self.next_token()?;
        let span = tok.span;
        match tok.token {
            Token::ScalarVar(name) => {
                let default = self.parse_sig_default()?;
                Ok(SigParam::Scalar { name, default, span: span.merge(self.peek_span()) })
            }
            Token::ArrayVar(name) => Ok(SigParam::SlurpyArray { name, span }),
            Token::Dollar => {
                // Bare `$` — anonymous scalar.  May have a default.
                let default = self.parse_sig_default()?;
                Ok(SigParam::AnonScalar { default, span: span.merge(self.peek_span()) })
            }
            Token::At => Ok(SigParam::AnonArray { span }),
            Token::Percent => {
                // Either anon hash placeholder or named slurpy hash; ask the lexer to probe for a hash name.
                match self.lexer.lex_hash_var_after_percent()? {
                    Some(Token::HashVar(name)) => Ok(SigParam::SlurpyHash { name, span }),
                    Some(Token::SpecialHashVar(_)) | Some(_) => {
                        // `%^FOO` etc. — not valid in a signature.
                        Err(ParseError::new("special hash variable not allowed in signature".to_string(), span))
                    }
                    None => Ok(SigParam::AnonHash { span }),
                }
            }
            other => Err(ParseError::new(format!("expected signature parameter, got {other:?}"), span)),
        }
    }

    /// Parse an optional default value in a signature: `= expr`, `//= expr`, `||= expr`.
    fn parse_sig_default(&mut self) -> Result<Option<(SigDefaultKind, Expr)>, ParseError> {
        let kind = if self.eat(&Token::Assign(AssignOp::Eq))? {
            SigDefaultKind::Eq
        } else if self.eat(&Token::Assign(AssignOp::DefinedOrEq))? {
            SigDefaultKind::DefinedOr
        } else if self.eat(&Token::Assign(AssignOp::OrEq))? {
            SigDefaultKind::LogicalOr
        } else {
            return Ok(None);
        };
        let expr = self.parse_expr(PREC_COMMA + 1)?;
        Ok(Some((kind, expr)))
    }

    fn parse_attributes(&mut self) -> Result<Vec<Attribute>, ParseError> {
        let mut attrs = Vec::new();
        while self.at(&Token::Colon)? {
            let attr_start = self.peek_span();
            self.next_token()?; // eat ':'

            // Attribute names can be identifiers or keywords (e.g. :method, :lvalue)
            let name = match self.peek_token().clone() {
                Token::Ident(s) => Some(s),
                Token::Keyword(kw) => Some(<&str>::from(kw).to_string()),
                _ => None,
            };
            if let Some(name) = name {
                let name_span = self.peek_span();
                self.next_token()?; // eat the name

                // Optional parenthesized args.  For `:prototype(...)` specifically, the body is Perl prototype syntax
                // (containing `$`, `@`, `%`, `\`, etc.) which must be read as raw bytes — token-by-token reconstruction
                // via Display impls loses fidelity.  Other attributes use the general token-reconstruction path.
                let value = if self.at(&Token::LeftParen)? {
                    self.next_token()?; // consume (
                    if name == "prototype" {
                        Some(self.lexer.lex_body_str('(', true)?)
                    } else {
                        let mut args = String::new();
                        let mut depth = 1u32;
                        loop {
                            match self.peek_token().clone() {
                                Token::LeftParen => {
                                    depth += 1;
                                    args.push('(');
                                    self.next_token()?;
                                }
                                Token::RightParen => {
                                    depth -= 1;
                                    if depth == 0 {
                                        self.next_token()?;
                                        break;
                                    }
                                    args.push(')');
                                    self.next_token()?;
                                }
                                Token::Eof => break,
                                _ => {
                                    args.push_str(&format!("{}", self.next_token()?.token));
                                }
                            }
                        }
                        Some(args)
                    }
                } else {
                    None
                };
                attrs.push(Attribute { name, value, span: attr_start.merge(name_span) });
            } else {
                break;
            }
        }
        Ok(attrs)
    }

    /// Parse a declaration in expression context: `my $x`, `our ($a, @b)`, etc.  Returns a Decl expression; the Pratt
    /// parser handles `= expr` as assignment.
    fn parse_decl_expr(&mut self, scope: DeclScope, span: Span) -> Result<Expr, ParseError> {
        let mut vars = Vec::new();

        if self.at(&Token::LeftParen)? {
            // List form: my ($x, @y, %z)
            self.next_token()?;
            while !self.at(&Token::RightParen)? && !self.at_eof()? {
                vars.push(self.parse_single_var_decl()?);
                if !self.eat(&Token::Comma)? {
                    break;
                }
            }
            let end = self.peek_span();
            self.expect_token(&Token::RightParen)?;
            Ok(Expr::new(ExprKind::Decl(scope, vars), span.merge(end)))
        } else {
            // Single variable: my $x, my @arr, my %hash
            // Optional attributes: my $x : Foo
            let mut var = self.parse_single_var_decl()?;
            let attrs = self.parse_attributes()?;
            if !attrs.is_empty() {
                var.attributes = attrs;
            }
            let end = var.span;
            vars.push(var);
            Ok(Expr::new(ExprKind::Decl(scope, vars), span.merge(end)))
        }
    }

    /// Parse an anonymous sub expression: `sub { ... }`, `sub ($x) { ... }`, `sub :lvalue { ... }`, `sub ($) :lvalue
    /// { ... }`, etc.
    fn parse_anon_sub(&mut self, span: Span) -> Result<Expr, ParseError> {
        let signatures_active = self.pragmas.features.contains(Features::SIGNATURES);
        let (prototype, attributes, signature) = if signatures_active {
            // With signatures: attributes before signature.
            let attrs = self.parse_attributes()?;
            let sig = self.parse_signature()?;
            (None, attrs, sig)
        } else {
            // Without signatures: prototype before attributes.
            let proto = self.parse_prototype()?;
            let attrs = self.parse_attributes()?;
            (proto, attrs, None)
        };

        let body = self.parse_block()?;

        let span = span.merge(body.span);
        Ok(Expr::anon_sub(prototype, attributes, signature, body, span))
    }

    fn parse_anon_method(&mut self, span: Span) -> Result<Expr, ParseError> {
        // Methods always act as if signatures are in effect.
        let attrs = self.parse_attributes()?;
        let sig = self.parse_signature()?;
        let body = self.parse_block()?;

        let span = span.merge(body.span);
        Ok(Expr::anon_method(attrs, sig, body, span))
    }

    // ── Control flow statements ───────────────────────────────
    fn parse_if_stmt(&mut self) -> Result<StmtKind, ParseError> {
        let condition = self.parse_paren_expr()?;
        let then_block = self.parse_block()?;

        let mut elsif_clauses = Vec::new();
        while self.eat(&Token::Keyword(Keyword::Elsif))? {
            let cond = self.parse_paren_expr()?;
            let block = self.parse_block()?;
            elsif_clauses.push((cond, block));
        }

        // Catch common mistake: `elseif` instead of `elsif`.
        if self.at(&Token::Keyword(Keyword::Elseif))? {
            return Err(ParseError::new("elseif should be elsif", self.peek_span()));
        }

        let else_block = if self.eat(&Token::Keyword(Keyword::Else))? { Some(self.parse_block()?) } else { None };

        Ok(StmtKind::If(IfStmt { condition, then_block, elsif_clauses, else_block }))
    }

    fn parse_unless_stmt(&mut self) -> Result<StmtKind, ParseError> {
        let condition = self.parse_paren_expr()?;
        let then_block = self.parse_block()?;

        let mut elsif_clauses = Vec::new();
        while self.eat(&Token::Keyword(Keyword::Elsif))? {
            let cond = self.parse_paren_expr()?;
            let block = self.parse_block()?;
            elsif_clauses.push((cond, block));
        }

        // Catch common mistake: `elseif` instead of `elsif`.
        if self.at(&Token::Keyword(Keyword::Elseif))? {
            return Err(ParseError::new("elseif should be elsif", self.peek_span()));
        }

        let else_block = if self.eat(&Token::Keyword(Keyword::Else))? { Some(self.parse_block()?) } else { None };
        Ok(StmtKind::Unless(UnlessStmt { condition, then_block, elsif_clauses, else_block }))
    }

    fn parse_while_stmt(&mut self) -> Result<StmtKind, ParseError> {
        let condition = self.parse_paren_expr()?;
        let body = self.parse_block()?;
        let continue_block = if self.eat(&Token::Keyword(Keyword::Continue))? { Some(self.parse_block()?) } else { None };
        Ok(StmtKind::While(WhileStmt { condition, body, continue_block }))
    }

    fn parse_until_stmt(&mut self) -> Result<StmtKind, ParseError> {
        let condition = self.parse_paren_expr()?;
        let body = self.parse_block()?;
        let continue_block = if self.eat(&Token::Keyword(Keyword::Continue))? { Some(self.parse_block()?) } else { None };
        Ok(StmtKind::Until(UntilStmt { condition, body, continue_block }))
    }

    fn parse_for_stmt(&mut self) -> Result<StmtKind, ParseError> {
        // If next is a variable, 'my', or '\' (refaliasing), it's foreach-style.
        if matches!(self.peek_token(), Token::Keyword(Keyword::My) | Token::ScalarVar(_) | Token::Backslash) {
            return self.parse_foreach_body();
        }

        // Consume '(' then decide: C-style or foreach based on whether a `;` appears after the first expression.
        self.expect_token(&Token::LeftParen)?;

        // Empty init (`;` immediately) → definitely C-style.
        if self.at(&Token::Semi)? {
            return self.parse_c_style_for_body(None);
        }

        // Parse the first expression.
        let first = self.parse_expr(PREC_LOW)?;

        // Semicolon after first expression → C-style, first was init.
        if self.at(&Token::Semi)? {
            return self.parse_c_style_for_body(Some(first));
        }

        // No semicolon → foreach-style, first was the list.
        self.expect_token(&Token::RightParen)?;
        let body = self.parse_block()?;
        let continue_block = if self.eat(&Token::Keyword(Keyword::Continue))? { Some(self.parse_block()?) } else { None };

        Ok(StmtKind::ForEach(ForEachStmt { vars: vec![], list: first, body, continue_block }))
    }

    /// Parse the rest of a C-style for loop after `(` and the optional init expression have been consumed.  Next token
    /// should be `;`.
    fn parse_c_style_for_body(&mut self, init: Option<Expr>) -> Result<StmtKind, ParseError> {
        self.expect_token(&Token::Semi)?;

        // condition (may be empty)
        let condition = if self.at(&Token::Semi)? { None } else { Some(self.parse_expr(PREC_LOW)?) };
        self.expect_token(&Token::Semi)?;

        // step (may be empty)
        let step = if self.at(&Token::RightParen)? { None } else { Some(self.parse_expr(PREC_LOW)?) };
        self.expect_token(&Token::RightParen)?;

        let body = self.parse_block()?;

        Ok(StmtKind::For(ForStmt { init, condition, step, body }))
    }

    fn parse_foreach_body(&mut self) -> Result<StmtKind, ParseError> {
        let vars = if self.eat(&Token::Keyword(Keyword::My))? {
            if self.at(&Token::LeftParen)? {
                // `for my ($x, $y, $z) (LIST)` — multi-variable (5.36+).
                self.next_token()?; // consume (
                let mut vars = Vec::new();
                loop {
                    vars.push(self.parse_single_var_decl()?);
                    if !self.eat(&Token::Comma)? {
                        break;
                    }
                }
                self.expect_token(&Token::RightParen)?;
                vars
            } else {
                // `for my $x (LIST)` — single variable.
                vec![self.parse_single_var_decl()?]
            }
        } else if self.at(&Token::Backslash)? {
            // `for \my $x (LIST)` — refaliasing (experimental).
            self.next_token()?; // consume backslash
            self.expect_token(&Token::Keyword(Keyword::My))?;
            let mut vd = self.parse_single_var_decl()?;
            vd.is_ref = true;
            vec![vd]
        } else if matches!(self.peek_token(), Token::ScalarVar(_)) {
            let span = self.peek_span();
            let name = match self.next_token()?.token {
                Token::ScalarVar(n) => n,
                _ => unreachable!(),
            };
            vec![VarDecl { sigil: Sigil::Scalar, name, span, attributes: vec![], is_ref: false }]
        } else {
            vec![]
        };

        let list = self.parse_paren_expr()?;
        let body = self.parse_block()?;
        let continue_block = if self.eat(&Token::Keyword(Keyword::Continue))? { Some(self.parse_block()?) } else { None };

        Ok(StmtKind::ForEach(ForEachStmt { vars, list, body, continue_block }))
    }

    // ── Package and use ───────────────────────────────────────
    fn parse_package_decl(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        let name = match self.next_token()?.token {
            Token::Ident(n) => n,

            // Keywords are valid package names: `package send;`
            Token::Keyword(kw) => <&str>::from(kw).to_string(),
            other => return Err(ParseError::new(format!("expected package name, got {other:?}"), start)),
        };

        // Optional version
        let version = if matches!(self.peek_token(), Token::IntLit(_) | Token::FloatLit(_) | Token::VersionLit(_)) {
            Some(format!("{}", self.next_token()?.token))
        } else {
            None
        };

        // Ensure the package exists in the symbol table, even if empty — so later references to it resolve correctly.
        let _ = self.symbols.entry(&name);

        let block = if self.at(&Token::LeftBrace)? {
            // Block form: `package Name { ... }` — switch packages for the duration of the block, then restore.
            let saved = std::mem::replace(&mut self.current_package, std::sync::Arc::from(name.as_str()));
            let block = self.parse_block()?;
            self.current_package = saved;
            Some(block)
        } else {
            // Statement form: `package Name;` — switch packages for everything that follows in this compilation unit.
            self.eat(&Token::Semi)?;
            self.current_package = std::sync::Arc::from(name.as_str());
            None
        };

        Ok(StmtKind::PackageDecl(PackageDecl { name, version, block, span: start.merge(self.peek_span()) }))
    }

    fn parse_use_decl(&mut self, start: Span, is_no: bool) -> Result<StmtKind, ParseError> {
        // First argument: either a version (use 5.020) or a module name.
        let first = self.next_token()?;
        let module = match first.token {
            Token::Ident(n) => n,

            // Bare version: `use 5.020;` / `use v5.36;` — module slot gets the version; no further version or imports.
            Token::IntLit(n) => {
                // Apply the matching bundle to pragma state.  `use 5.036` / `use 5036` → major=5, minor=36.
                if !is_no && let Some((maj, min)) = parse_int_version(n) {
                    self.pragmas.features.apply_version_bundle(maj, min);
                }
                self.eat(&Token::Semi)?;
                return Ok(StmtKind::UseDecl(UseDecl { is_no, module: format!("{n}"), version: None, imports: None, span: start.merge(self.peek_span()) }));
            }
            Token::FloatLit(n) => {
                // `use 5.036` can also lex as FloatLit (5.036).
                if !is_no && let Some((maj, min)) = parse_float_version(n) {
                    self.pragmas.features.apply_version_bundle(maj, min);
                }
                self.eat(&Token::Semi)?;
                return Ok(StmtKind::UseDecl(UseDecl { is_no, module: format!("{n}"), version: None, imports: None, span: start.merge(self.peek_span()) }));
            }
            Token::VersionLit(n) => {
                // v-string: `use v5.36;` — arrives as VersionLit.
                if !is_no && let Some((maj, min)) = parse_v_string_version(&n) {
                    self.pragmas.features.apply_version_bundle(maj, min);
                }
                n
            }
            Token::StrLit(n) => n,

            // Keywords can be module names: `use if`, `use open`, etc.
            Token::Keyword(kw) => <&str>::from(kw).to_string(),
            other => return Err(ParseError::new(format!("expected module name or version, got {other:?}"), start)),
        };

        // Optional version after the module name: `use Module 1.23;` or `use Module v5.26;`.  Versions are either
        // numeric literals or v-string VersionLit tokens; anything else starts the import list.
        let version = match self.peek_token() {
            Token::IntLit(_) | Token::FloatLit(_) => {
                let tok = self.next_token()?;
                Some(match tok.token {
                    Token::IntLit(n) => format!("{n}"),
                    Token::FloatLit(n) => format!("{n}"),
                    _ => unreachable!(),
                })
            }

            // v-string literal like v5.26.0.
            Token::VersionLit(_) => {
                let tok = self.next_token()?;
                Some(match tok.token {
                    Token::VersionLit(s) => s,
                    _ => unreachable!(),
                })
            }
            _ => None,
        };

        // Optional import list: anything until the semicolon.
        let imports = if self.at(&Token::Semi)? || self.at_eof()? {
            None
        } else {
            let mut items = Vec::new();
            loop {
                if self.at(&Token::Semi)? || self.at_eof()? {
                    break;
                }
                items.push(self.parse_expr(PREC_COMMA + 1)?);
                if !self.eat(&Token::Comma)? && !self.eat(&Token::FatComma)? {
                    break;
                }
            }
            Some(items)
        };

        self.eat(&Token::Semi)?;

        // Pragma dispatch: apply any side effects to parser state before returning.  Unknown modules and non-pragma
        // imports are silently ignored here; they'd require runtime module loading to take effect.
        apply_pragma(&mut self.pragmas, &module, is_no, imports.as_ref());

        // Sync shared UTF-8 flag — the lexer reads this to decide whether to accept multi-byte identifiers.
        self.lexer.set_utf8_mode(self.pragmas.utf8);
        self.lexer.features = self.pragmas.features;

        // `use subs qw(name ...)` — forward-declare names in the symbol table so the parser recognizes them as known
        // subs.  This causes weak keywords to be overridden in term position.
        if !is_no
            && module == "subs"
            && let Some(ref items) = imports
        {
            for item in items {
                match &item.kind {
                    ExprKind::StringLit(name) => {
                        self.symbols.entry(&self.current_package.clone()).declare_sub(name, None, Vec::new(), true);
                    }
                    ExprKind::QwList(names) => {
                        for name in names {
                            self.symbols.entry(&self.current_package.clone()).declare_sub(name, None, Vec::new(), true);
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(StmtKind::UseDecl(UseDecl { is_no, module, version, imports, span: start.merge(self.peek_span()) }))
    }

    // ── Phaser blocks ─────────────────────────────────────────
    fn parse_phaser(&mut self, kind: PhaserKind) -> Result<StmtKind, ParseError> {
        let block = self.parse_block()?;
        Ok(StmtKind::Phaser(kind, block))
    }

    // ── given/when ────────────────────────────────────────────
    fn parse_given(&mut self) -> Result<StmtKind, ParseError> {
        let expr = self.parse_paren_expr()?;
        let block = self.parse_block()?;
        Ok(StmtKind::Given(expr, block))
    }

    fn parse_when(&mut self) -> Result<StmtKind, ParseError> {
        let expr = self.parse_paren_expr()?;
        let block = self.parse_block()?;
        Ok(StmtKind::When(expr, block))
    }

    // ── try/catch/finally ─────────────────────────────────────
    fn parse_try(&mut self) -> Result<StmtKind, ParseError> {
        let body = self.parse_block()?;

        let (catch_var, catch_block) = if self.eat(&Token::Keyword(Keyword::Catch))? {
            let var = if self.at(&Token::LeftParen)? {
                self.next_token()?;
                let decl = self.parse_single_var_decl()?;
                self.expect_token(&Token::RightParen)?;
                Some(decl)
            } else {
                None
            };
            let block = self.parse_block()?;
            (var, Some(block))
        } else {
            (None, None)
        };

        let finally_block = if self.eat(&Token::Keyword(Keyword::Finally))? { Some(self.parse_block()?) } else { None };

        Ok(StmtKind::Try(TryStmt { body, catch_var, catch_block, finally_block }))
    }

    // ── format ────────────────────────────────────────────────
    fn parse_format(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        // Optional name (defaults to STDOUT)
        let name = match self.peek_token() {
            Token::Ident(_) => match self.next_token()?.token {
                Token::Ident(s) => s,
                _ => unreachable!(),
            },

            // Keywords are valid format names: `format send = ...`.
            Token::Keyword(_) => match self.next_token()?.token {
                Token::Keyword(kw) => <&str>::from(kw).to_string(),
                _ => unreachable!(),
            },
            _ => "STDOUT".to_string(),
        };

        // Expect '='
        self.expect_token(&Token::Assign(AssignOp::Eq))?;

        // Hand off to the lexer's format sublex mode.  The next token will be FormatSublexBegin; the body ends at
        // SublexEnd (emitted for the `.` terminator).
        //
        // Careful: do NOT call `peek_span` here — that would invoke the lexer and potentially tokenize into the first
        // body line, which `start_format` would then discard when it drops `current_line`.  Build the begin span from
        // `start` and the current (pre-body) lexer position instead.
        let here = self.lexer.pos() as u32;
        let begin_span = start.merge(Span::new(here, here));
        self.lexer.start_format(name.clone(), begin_span);

        // Clear any cached current-token so the Parser re-fetches.
        self.current = None;

        // Consume the FormatSublexBegin.
        let begin = self.next_token()?;
        if !matches!(begin.token, Token::FormatSublexBegin(_)) {
            return Err(ParseError::new(format!("expected FormatSublexBegin, got {:?}", begin.token), begin.span));
        }

        // Read format lines until SublexEnd.
        let mut lines = Vec::new();
        loop {
            let tok = self.next_token()?;
            match tok.token {
                Token::SublexEnd => break,
                Token::FormatComment(text) => {
                    lines.push(FormatLine::Comment { text, span: tok.span });
                }
                Token::FormatBlankLine => {
                    lines.push(FormatLine::Blank { span: tok.span });
                }
                Token::FormatLiteralLine(repeat, text) => {
                    lines.push(FormatLine::Literal { repeat, text, span: tok.span });
                }
                Token::FormatPictureBegin(repeat) => {
                    lines.push(self.parse_format_picture(repeat, tok.span)?);
                }
                other => {
                    return Err(ParseError::new(format!("unexpected token in format body: {other:?}"), tok.span));
                }
            }
        }

        Ok(StmtKind::FormatDecl(FormatDecl { name, lines, span: start.merge(self.peek_span()) }))
    }

    /// Parse a picture line after `FormatPictureBegin(repeat)` has been consumed.  Consumes tokens until
    /// `FormatPictureEnd`, then the following `FormatArgsBegin` / expressions / `FormatArgsEnd` group, and assembles a
    /// `FormatLine::Picture`.
    fn parse_format_picture(&mut self, repeat: RepeatKind, begin_span: Span) -> Result<FormatLine, ParseError> {
        let mut parts = Vec::new();
        loop {
            let tok = self.next_token()?;
            match tok.token {
                Token::FormatPictureEnd => break,
                Token::FormatLiteral(text) => {
                    parts.push(FormatPart::Literal(text));
                }
                Token::FormatField(kind) => {
                    parts.push(FormatPart::Field(FormatField { kind, span: tok.span }));
                }
                other => {
                    return Err(ParseError::new(format!("unexpected token in picture line: {other:?}"), tok.span));
                }
            }
        }

        // Expect FormatArgsBegin.
        let args_begin = self.next_token()?;
        if !matches!(args_begin.token, Token::FormatArgsBegin) {
            return Err(ParseError::new(format!("expected FormatArgsBegin, got {:?}", args_begin.token), args_begin.span));
        }

        // Peek: if `{` is the first args token, consume it and switch the lexer to braced mode.
        let braced = matches!(self.peek_token(), Token::LeftBrace);
        if braced {
            self.next_token()?; // consume `{`
            self.lexer.format_args_enter_braced();

            // Clear any cached token — the mode switch may affect how the next token is produced.
            self.current = None;
        }

        // Parse comma-separated expressions until FormatArgsEnd.
        let mut args = Vec::new();
        while !matches!(self.peek_token(), Token::FormatArgsEnd) {
            // Defensive: surface EOF / unexpected termination.
            if matches!(self.peek_token(), Token::Eof) {
                return Err(ParseError::new("unexpected EOF inside format arguments".to_string(), self.peek_span()));
            }
            let expr = self.parse_expr(PREC_COMMA + 1)?;
            args.push(expr);
            if !self.eat(&Token::Comma)? {
                break;
            }
        }

        // Consume FormatArgsEnd.
        let end = self.next_token()?;
        if !matches!(end.token, Token::FormatArgsEnd) {
            return Err(ParseError::new(format!("expected FormatArgsEnd, got {:?}", end.token), end.span));
        }

        Ok(FormatLine::Picture { repeat, parts, args, span: begin_span.merge(end.span) })
    }

    // ── class / field / method (5.38+ Corinna) ────────────────
    fn parse_class(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        let name = match self.next_token()?.token {
            Token::Ident(n) => n,
            other => return Err(ParseError::new(format!("expected class name, got {other:?}"), start)),
        };

        // Optional version (like package).
        let version = if matches!(self.peek_token(), Token::IntLit(_) | Token::FloatLit(_) | Token::VersionLit(_)) {
            Some(format!("{}", self.next_token()?.token))
        } else {
            None
        };

        let attributes = self.parse_attributes()?;

        // Block form or statement form (like package).
        let body = if self.at(&Token::LeftBrace)? {
            Some(self.parse_block()?)
        } else {
            self.eat(&Token::Semi)?;
            None
        };

        Ok(StmtKind::ClassDecl(ClassDecl { name, version, attributes, body, span: start.merge(self.peek_span()) }))
    }

    fn parse_field(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        let var = self.parse_single_var_decl()?;
        let attributes = self.parse_attributes()?;

        let default = if self.eat(&Token::Assign(AssignOp::Eq))? {
            Some((SigDefaultKind::Eq, self.parse_expr(PREC_COMMA)?))
        } else if self.eat(&Token::Assign(AssignOp::DefinedOrEq))? {
            Some((SigDefaultKind::DefinedOr, self.parse_expr(PREC_COMMA)?))
        } else if self.eat(&Token::Assign(AssignOp::OrEq))? {
            Some((SigDefaultKind::LogicalOr, self.parse_expr(PREC_COMMA)?))
        } else {
            None
        };

        self.eat(&Token::Semi)?;

        Ok(StmtKind::FieldDecl(FieldDecl { var, attributes, default, span: start.merge(self.peek_span()) }))
    }

    fn parse_method(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        let name = match self.next_token()?.token {
            Token::Ident(n) => n,

            // Keywords are valid method names: `method send { }`.
            Token::Keyword(kw) => <&str>::from(kw).to_string(),
            other => return Err(ParseError::new(format!("expected method name, got {other:?}"), start)),
        };

        // Methods get the same signature-vs-prototype dispatch as regular subs.  (Inside `class { ... }` with
        // signatures active, methods bind `$self` implicitly — captured at runtime, not in the parser AST.)
        let signatures_active = self.pragmas.features.contains(Features::SIGNATURES);
        let (prototype, attributes, signature) = if signatures_active {
            let attrs = self.parse_attributes()?;
            let sig = self.parse_signature()?;
            (None, attrs, sig)
        } else {
            let proto = self.parse_prototype()?;
            let attrs = self.parse_attributes()?;
            (proto, attrs, None)
        };

        let body = self.parse_block()?;

        Ok(StmtKind::MethodDecl(SubDecl { name, scope: None, prototype, attributes, signature, body, span: start.merge(self.peek_span()) }))
    }

    // ── Expression statements ─────────────────────────────────
    fn parse_expr_statement(&mut self) -> Result<(StmtKind, bool), ParseError> {
        let expr = self.parse_expr(PREC_LOW)?;

        // Check for postfix control flow
        let kind = self.maybe_postfix_control(expr)?;

        let terminated = self.eat(&Token::Semi)?;
        Ok((kind, terminated))
    }

    // ── Block-to-hash reclassification ─────────────────────────
    /// Inspect a block parsed at statement level and determine if it should be reclassified as an anonymous hash
    /// constructor.
    ///
    /// Returns `Ok(expr)` with an `AnonHash` expression if the block looks like a hash constructor, or `Err(block)` to
    /// keep it as a block.
    ///
    /// A block is reclassified as a hash constructor when:
    /// - It contains exactly one statement.
    /// - That statement is a plain expression (not `my`, `if`, etc.).
    /// - The expression was NOT terminated by a semicolon.
    /// - The expression contains a top-level fat comma (`=>`), OR the expression is a comma-list whose first element is
    ///   a string literal, non-lowercase bareword, or other non-function term.
    ///
    /// This matches Perl's behavior for common cases while being strictly more accurate than the byte-level heuristic
    /// it replaces, because it operates on parsed AST nodes rather than raw bytes.
    fn try_reclassify_as_hash(block: Block) -> Result<Expr, Block> {
        // Empty block → empty hash (matching Perl's toke.c line 6368).
        if block.statements.is_empty() {
            return Ok(Expr::new(ExprKind::AnonHash(Vec::new()), block.span));
        }

        // Multiple statements → definitely a block.
        if block.statements.len() != 1 {
            return Err(block);
        }

        let stmt = &block.statements[0];

        // Terminated by semicolon → block.
        if stmt.terminated {
            return Err(block);
        }

        // Not an expression statement → block.
        let expr = match &stmt.kind {
            StmtKind::Expr(e) => e,
            _ => return Err(block),
        };

        // Check if the expression looks like hash content.
        if Self::looks_like_hash_expr(expr) {
            // Destructure to take ownership.
            let span = block.span;
            let mut stmts = block.statements;
            let stmt = stmts.remove(0);
            let expr = match stmt.kind {
                StmtKind::Expr(e) => e,
                _ => unreachable!(),
            };

            // Flatten Comma into individual AnonHash elements to match the structure produced by parse_term for hash
            // constructors in term position.
            let elems = match expr.kind {
                ExprKind::Comma(items) => items,
                _ => vec![expr],
            };
            Ok(Expr::new(ExprKind::AnonHash(elems), span))
        } else {
            Err(block)
        }
    }

    /// Check whether an expression looks like hash constructor content rather than a block body.
    ///
    /// Matches Perl's byte-level heuristic at the AST level:
    /// - A comma-list whose first element is a string literal, numeric literal, variable, or non-lowercase bareword is
    ///   hash-like (Perl: non-lowercase first byte + comma → hash).  - A comma-list whose first element is a lowercase
    ///   bareword or function call is block-like (`func arg, arg`).
    /// - A non-list expression (no commas) is block-like.
    ///
    /// Fat comma (`=>`) autoquotes barewords to StringLit before we see them, so `key => val` appears as
    /// `Comma([StringLit, val])`.
    fn looks_like_hash_expr(expr: &Expr) -> bool {
        match &expr.kind {
            // A comma-list: check the first element.
            ExprKind::Comma(items) => {
                match items.first().map(|e| &e.kind) {
                    // String literal — covers autoquoted barewords from fat comma, explicit strings, q//.
                    Some(ExprKind::StringLit(_)) => true,

                    // Numeric literals.
                    Some(ExprKind::IntLit(_)) => true,
                    Some(ExprKind::FloatLit(_)) => true,

                    // Variables — `$x => 1` or `$x, 1`.  Perl's heuristic: `$` is not lowercase → hash.
                    Some(ExprKind::ScalarVar(_)) => true,
                    Some(ExprKind::ArrayVar(_)) => true,
                    Some(ExprKind::HashVar(_)) => true,

                    // Unary prefix on a variable — `-$x => 1`.
                    Some(ExprKind::UnaryOp(_, _)) => true,

                    // Non-lowercase bareword — `Foo, 1`.
                    Some(ExprKind::Bareword(name)) => name.starts_with(|c: char| c.is_ascii_uppercase() || c == '_'),

                    // Lowercase bareword looks like `func arg, arg`.  Anything else (function calls, complex exprs) →
                    // block.
                    _ => false,
                }
            }

            // No commas at all — not hash-like.
            _ => false,
        }
    }

    // ── Block parsing ─────────────────────────────────────────
    fn parse_block(&mut self) -> Result<Block, ParseError> {
        // Pragmas and current_package are lexically scoped: any `use feature`, `use utf8`, or `package Name;` inside
        // this block doesn't leak out.  Save state before parsing, restore after.
        let saved_pragmas = self.pragmas;
        let saved_package = self.current_package.clone();

        let start = self.peek_span();
        let result = (|this: &mut Parser| -> Result<Block, ParseError> {
            this.expect_token(&Token::LeftBrace)?;

            let mut statements = Vec::new();
            while !this.at(&Token::RightBrace)? && !this.at_eof()? {
                statements.push(this.parse_statement()?);
            }

            let end = this.peek_span();
            this.expect_token(&Token::RightBrace)?;

            Ok(Block { statements, span: start.merge(end) })
        })(self);

        self.pragmas = saved_pragmas;
        self.current_package = saved_package;

        // Sync shared state with the restored lexical scope.
        self.lexer.set_utf8_mode(self.pragmas.utf8);
        self.lexer.features = self.pragmas.features;
        result
    }

    fn parse_paren_expr(&mut self) -> Result<Expr, ParseError> {
        self.expect_token(&Token::LeftParen)?;
        let expr = self.parse_expr(PREC_LOW)?;
        self.expect_token(&Token::RightParen)?;
        Ok(expr)
    }

    // ── Expression parsing (Pratt) ────────────────────────────
    /// Parse an expression at the given minimum precedence.  The forward phase consumes prefix operators and opening
    /// parens iteratively (no recursion) until a leaf term is reached.  The backward phase applies saved frames,
    /// running the infix continuation loop at each precedence level.  This eliminates the recursive `parse_expr` →
    /// `parse_term` → `parse_expr` chain that previously caused stack overflow on deeply nested input.
    fn parse_expr(&mut self, min_prec: Precedence) -> Result<Expr, ParseError> {
        let mut stack: Vec<ExprFrame> = Vec::new();
        let mut current_prec = min_prec;

        // Forward phase: consume prefix operators, parens, and opening delimiters iteratively.
        let mut left = self.parse_expr_forward(&mut stack, &mut current_prec)?;

        // Backward phase: run the infix loop at each level, then apply the saved frame.
        loop {
            left = self.parse_expr_continuation(left, current_prec)?;
            match stack.pop() {
                None => return Ok(left),
                Some(frame) => match self.apply_expr_frame(frame, left)? {
                    FrameResult::Done(expr, prec) => {
                        left = expr;
                        current_prec = prec;
                    }
                    FrameResult::Continue(frame, inner_prec) => {
                        // Accumulator frame needs more elements — re-push and re-enter forward phase.
                        stack.push(frame);
                        current_prec = inner_prec;
                        left = self.parse_expr_forward(&mut stack, &mut current_prec)?;
                    }
                },
            }
        }
    }

    /// Forward phase: consume prefix operators and opening delimiters, pushing continuation frames.
    fn parse_expr_forward(&mut self, stack: &mut Vec<ExprFrame>, current_prec: &mut Precedence) -> Result<Expr, ParseError> {
        loop {
            match self.try_prefix(*current_prec)? {
                Some(PrefixResult::Frame(frame, inner_prec)) => {
                    stack.push(frame);
                    *current_prec = inner_prec;
                }
                Some(PrefixResult::Leaf(expr)) => return Ok(expr),
                None => return self.parse_term(),
            }
        }
    }

    /// Try to consume a prefix operator or opening paren.  Returns `Frame` to push and continue, `Leaf` for a complete
    /// term that doesn't recurse, or `None` if the current token is not a prefix.
    fn try_prefix(&mut self, outer_prec: Precedence) -> Result<Option<PrefixResult>, ParseError> {
        let span = self.peek_span();
        match self.peek_token().clone() {
            Token::LeftParen => {
                self.next_token()?;
                if self.at(&Token::RightParen)? {
                    self.next_token()?;
                    let expr = Expr::new(ExprKind::Comma(vec![]), span);
                    return Ok(Some(PrefixResult::Leaf(self.maybe_postfix_subscript(expr)?)));
                }
                Ok(Some(PrefixResult::Frame(ExprFrame::Paren { span, min_prec: outer_prec }, PREC_LOW)))
            }
            Token::Minus => {
                self.next_token()?;

                // Filetest: -f, -d, etc.  In fat-comma context, lex_filetest_after_minus returns StrLit.
                match self.lexer.lex_filetest_after_minus() {
                    Some(Token::Filetest(b)) => {
                        let end = self.peek_span();
                        return Ok(Some(PrefixResult::Leaf(self.parse_filetest(b, span.merge(end))?)));
                    }
                    Some(Token::StrLit(s)) => {
                        return Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::StringLit(s), span))));
                    }
                    _ => {}
                }

                // -bareword (not followed by parens) → StringLit("-bareword")
                if let Token::Ident(name) = self.peek_token().clone() {
                    let ident_span = self.peek_span();
                    self.next_token()?;
                    if matches!(self.peek_token(), Token::LeftParen) {
                        // -func(...) → unary minus on function call
                        let func = self.parse_ident_term(name, ident_span)?;
                        let span = span.merge(func.span);
                        return Ok(Some(PrefixResult::Leaf(Expr::unary(UnaryOp::Negate, func, span))));
                    }
                    return Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::StringLit(format!("-{name}")), span.merge(ident_span)))));
                }

                // General negate: -expr
                Ok(Some(PrefixResult::Frame(ExprFrame::Negate { span, min_prec: outer_prec }, PREC_UNARY)))
            }
            Token::Plus => {
                self.next_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::Unary { op: UnaryOp::NumPositive, span, min_prec: outer_prec }, PREC_UNARY)))
            }
            Token::Bang => {
                self.next_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::Unary { op: UnaryOp::LogNot, span, min_prec: outer_prec }, PREC_UNARY)))
            }
            Token::Tilde => {
                self.next_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::Unary { op: UnaryOp::BitNot, span, min_prec: outer_prec }, PREC_UNARY)))
            }
            Token::StringBitNot => {
                self.next_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::Unary { op: UnaryOp::StringBitNot, span, min_prec: outer_prec }, PREC_UNARY)))
            }
            Token::Backslash => {
                self.next_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::Ref { span, min_prec: outer_prec }, PREC_UNARY)))
            }
            Token::Keyword(Keyword::Not) => {
                self.next_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::Unary { op: UnaryOp::Not, span, min_prec: outer_prec }, PREC_NOT_LOW)))
            }
            Token::PlusPlus => {
                self.next_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::PreIncDec { op: UnaryOp::PreInc, span, min_prec: outer_prec }, PREC_INC)))
            }
            Token::MinusMinus => {
                self.next_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::PreIncDec { op: UnaryOp::PreDec, span, min_prec: outer_prec }, PREC_INC)))
            }
            Token::Keyword(Keyword::Local) => {
                self.next_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::Local { span, min_prec: outer_prec }, PREC_UNARY)))
            }
            Token::LeftBracket => {
                self.next_token()?;
                if self.at(&Token::RightBracket)? {
                    self.next_token()?;
                    return Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::AnonArray(vec![]), span))));
                }
                Ok(Some(PrefixResult::Frame(ExprFrame::ArrayRef { elems: vec![], span, min_prec: outer_prec }, PREC_COMMA + 1)))
            }
            Token::LeftBrace => {
                self.next_token()?;
                if self.at(&Token::RightBrace)? {
                    self.next_token()?;
                    return Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::AnonHash(vec![]), span))));
                }
                Ok(Some(PrefixResult::Frame(ExprFrame::HashRef { elems: vec![], span, min_prec: outer_prec }, PREC_COMMA + 1)))
            }

            // ── Sigil-prefix dereference ──
            // ${expr}, $$ref → Token::Dollar; @{expr}, @$ref → Token::At; etc.
            // The {expr} path pushes a DerefBlock frame; everything else is a complete Leaf.
            Token::Dollar => {
                self.next_token()?;
                if self.at(&Token::LeftBrace)? {
                    self.next_token()?;
                    Ok(Some(PrefixResult::Frame(ExprFrame::DerefBlock { sigil: Sigil::Scalar, span, min_prec: outer_prec }, PREC_LOW)))
                } else {
                    let operand = self.parse_deref_operand()?;
                    let span = span.merge(operand.span);
                    let expr = Expr::deref(Sigil::Scalar, operand, span);
                    Ok(Some(PrefixResult::Leaf(self.maybe_postfix_subscript(expr)?)))
                }
            }
            Token::At => {
                self.next_token()?;
                if self.at(&Token::LeftBrace)? {
                    self.next_token()?;
                    Ok(Some(PrefixResult::Frame(ExprFrame::DerefBlock { sigil: Sigil::Array, span, min_prec: outer_prec }, PREC_LOW)))
                } else {
                    let operand = self.parse_deref_operand()?;
                    let span = span.merge(operand.span);
                    Ok(Some(PrefixResult::Leaf(Expr::deref(Sigil::Array, operand, span))))
                }
            }
            Token::Percent => {
                self.next_token()?;
                match self.lexer.lex_hash_var_after_percent()? {
                    Some(Token::HashVar(name)) => {
                        let recv = Expr::new(ExprKind::HashVar(name), span);
                        Ok(Some(PrefixResult::Leaf(self.maybe_kv_slice(recv, span)?)))
                    }
                    Some(Token::SpecialHashVar(name)) => Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::SpecialHashVar(name), span)))),
                    Some(other) => unreachable!("unexpected hash token: {other:?}"),
                    None => {
                        if self.at(&Token::LeftBrace)? {
                            self.next_token()?;
                            Ok(Some(PrefixResult::Frame(ExprFrame::DerefBlock { sigil: Sigil::Hash, span, min_prec: outer_prec }, PREC_LOW)))
                        } else {
                            let operand = self.parse_deref_operand()?;
                            let span = span.merge(operand.span);
                            Ok(Some(PrefixResult::Leaf(Expr::deref(Sigil::Hash, operand, span))))
                        }
                    }
                }
            }
            Token::BitAnd => {
                self.next_token()?;
                if self.at(&Token::LeftBrace)? {
                    self.next_token()?;
                    Ok(Some(PrefixResult::Frame(ExprFrame::DerefBlock { sigil: Sigil::Code, span, min_prec: outer_prec }, PREC_LOW)))
                } else if let Token::Ident(_) = self.peek_token() {
                    let name_span = self.peek_span();
                    let name = match self.next_token()?.token {
                        Token::Ident(s) => s,
                        _ => unreachable!(),
                    };
                    if self.at(&Token::LeftParen)? {
                        self.next_token()?;
                        let mut args = Vec::new();
                        while !self.at(&Token::RightParen)? && !self.at_eof()? {
                            args.push(self.parse_expr(PREC_COMMA + 1)?);
                            if !self.eat(&Token::Comma)? {
                                break;
                            }
                        }
                        let end = self.peek_span();
                        self.expect_token(&Token::RightParen)?;
                        Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::FuncCall(self.qualify_sub_name(&name), args), span.merge(end)))))
                    } else {
                        Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::FuncCall(self.qualify_sub_name(&name), vec![]), span.merge(name_span)))))
                    }
                } else {
                    let operand = self.parse_deref_operand()?;
                    let span = span.merge(operand.span);
                    let deref = Expr::deref(Sigil::Code, operand, span);
                    Ok(Some(PrefixResult::Leaf(self.maybe_call_args(deref)?)))
                }
            }
            Token::Star => {
                self.next_token()?;
                if self.at(&Token::LeftBrace)? {
                    self.next_token()?;
                    Ok(Some(PrefixResult::Frame(ExprFrame::DerefBlock { sigil: Sigil::Glob, span, min_prec: outer_prec }, PREC_LOW)))
                } else if let Token::Ident(_) = self.peek_token() {
                    let name_span = self.peek_span();
                    let name = match self.next_token()?.token {
                        Token::Ident(s) => s,
                        _ => unreachable!(),
                    };
                    let expr = Expr::new(ExprKind::GlobVar(name), span.merge(name_span));
                    if self.at(&Token::LeftBrace)? {
                        self.next_token()?;
                        let key = self.parse_hash_subscript_key()?;
                        let end = self.peek_span();
                        self.expect_token(&Token::RightBrace)?;
                        Ok(Some(PrefixResult::Leaf(Expr::arrow_deref(expr, ArrowTarget::hash_elem(key), span.merge(end)))))
                    } else {
                        Ok(Some(PrefixResult::Leaf(expr)))
                    }
                } else {
                    let operand = self.parse_deref_operand()?;
                    let span = span.merge(operand.span);
                    Ok(Some(PrefixResult::Leaf(Expr::deref(Sigil::Glob, operand, span))))
                }
            }

            // ── Keyword expressions that recurse into parse_expr ──
            Token::Keyword(Keyword::Eval) => {
                self.next_token()?;
                if self.at(&Token::LeftBrace)? {
                    let block = self.parse_block()?;
                    let span = span.merge(block.span);
                    Ok(Some(PrefixResult::Leaf(Expr::eval_block(block, span))))
                } else {
                    Ok(Some(PrefixResult::Frame(ExprFrame::EvalExpr { span, min_prec: outer_prec }, PREC_COMMA)))
                }
            }
            Token::Keyword(Keyword::Do) => {
                self.next_token()?;
                if self.at(&Token::LeftBrace)? {
                    let block = self.parse_block()?;
                    let span = span.merge(block.span);
                    Ok(Some(PrefixResult::Leaf(Expr::do_block(block, span))))
                } else {
                    Ok(Some(PrefixResult::Frame(ExprFrame::DoExpr { span, min_prec: outer_prec }, PREC_UNARY)))
                }
            }
            Token::Keyword(Keyword::Return) => {
                self.next_token()?;
                if self.at(&Token::Semi)? || self.at(&Token::RightBrace)? || self.at_eof()? {
                    Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::FuncCall("CORE::return".into(), vec![]), span))))
                } else {
                    Ok(Some(PrefixResult::Frame(ExprFrame::ReturnExpr { span, min_prec: outer_prec }, PREC_COMMA)))
                }
            }
            Token::Keyword(Keyword::Goto) => {
                self.next_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::GotoExpr { span, min_prec: outer_prec }, PREC_COMMA)))
            }
            Token::Keyword(Keyword::Dump) => {
                self.next_token()?;
                if self.at_eof()? || self.at(&Token::Semi)? {
                    Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::FuncCall("CORE::dump".into(), vec![]), span))))
                } else {
                    Ok(Some(PrefixResult::Frame(ExprFrame::DumpExpr { span, min_prec: outer_prec }, PREC_COMMA)))
                }
            }
            _ => Ok(None),
        }
    }

    /// Apply a saved continuation frame to the completed sub-expression.
    fn apply_expr_frame(&mut self, frame: ExprFrame, operand: Expr) -> Result<FrameResult, ParseError> {
        match frame {
            ExprFrame::Unary { op, span, min_prec } => {
                let span = span.merge(operand.span);
                Ok(FrameResult::Done(Expr::unary(op, operand, span), min_prec))
            }
            ExprFrame::Negate { span, min_prec } => {
                // String negation collapse: -"foo" → "-foo", -"-foo" → "+foo", -"+foo" → "-foo".
                if let ExprKind::StringLit(ref s) = operand.kind {
                    let negated = if let Some(rest) = s.strip_prefix('-') {
                        format!("+{rest}")
                    } else if let Some(rest) = s.strip_prefix('+') {
                        format!("-{rest}")
                    } else {
                        format!("-{s}")
                    };
                    Ok(FrameResult::Done(Expr::new(ExprKind::StringLit(negated), span.merge(operand.span)), min_prec))
                } else {
                    let span = span.merge(operand.span);
                    Ok(FrameResult::Done(Expr::unary(UnaryOp::Negate, operand, span), min_prec))
                }
            }
            ExprFrame::Ref { span, min_prec } => {
                let span = span.merge(operand.span);
                Ok(FrameResult::Done(Expr::reference(operand, span), min_prec))
            }
            ExprFrame::Local { span, min_prec } => {
                let span = span.merge(operand.span);
                Ok(FrameResult::Done(Expr::local(operand, span), min_prec))
            }
            ExprFrame::PreIncDec { op, span, min_prec } => {
                if !Self::is_valid_lvalue(&operand) {
                    let op_name = if op == UnaryOp::PreInc { "++" } else { "--" };
                    return Err(ParseError::new(format!("invalid operand for prefix {op_name}"), operand.span));
                }
                let span = span.merge(operand.span);
                Ok(FrameResult::Done(Expr::unary(op, operand, span), min_prec))
            }
            ExprFrame::Paren { span, min_prec } => {
                let end = self.peek_span();
                self.expect_token(&Token::RightParen)?;

                // Don't wrap in ExprKind::Paren — parentheses are purely syntactic grouping, and the tree structure
                // already captures precedence.  Avoiding the wrapper prevents recursive-Drop overflow on deeply nested
                // parens.  Update the span to include the parens.
                let expr = Expr { span: span.merge(end), ..operand };
                let expr = self.maybe_postfix_subscript(expr)?;
                Ok(FrameResult::Done(expr, min_prec))
            }
            ExprFrame::ArrayRef { mut elems, span, min_prec } => {
                elems.push(operand);
                if self.eat(&Token::Comma)? {
                    if self.at(&Token::RightBracket)? {
                        // Trailing comma: `[1, 2, 3,]`
                        let end = self.peek_span();
                        self.next_token()?;
                        return Ok(FrameResult::Done(Expr::new(ExprKind::AnonArray(elems), span.merge(end)), min_prec));
                    }

                    // More elements — re-enter forward phase.
                    return Ok(FrameResult::Continue(ExprFrame::ArrayRef { elems, span, min_prec }, PREC_COMMA + 1));
                }
                let end = self.peek_span();
                self.expect_token(&Token::RightBracket)?;
                Ok(FrameResult::Done(Expr::new(ExprKind::AnonArray(elems), span.merge(end)), min_prec))
            }
            ExprFrame::HashRef { mut elems, span, min_prec } => {
                elems.push(operand);
                if self.eat(&Token::Comma)? || self.eat(&Token::FatComma)? {
                    if self.at(&Token::RightBrace)? {
                        // Trailing comma: `{a => 1, b => 2,}`
                        let end = self.peek_span();
                        self.next_token()?;
                        return Ok(FrameResult::Done(Expr::new(ExprKind::AnonHash(elems), span.merge(end)), min_prec));
                    }

                    // More elements — re-enter forward phase.
                    return Ok(FrameResult::Continue(ExprFrame::HashRef { elems, span, min_prec }, PREC_COMMA + 1));
                }
                let end = self.peek_span();
                self.expect_token(&Token::RightBrace)?;
                Ok(FrameResult::Done(Expr::new(ExprKind::AnonHash(elems), span.merge(end)), min_prec))
            }
            ExprFrame::DerefBlock { sigil, span, min_prec } => {
                let end = self.peek_span();
                self.expect_token(&Token::RightBrace)?;
                let span = span.merge(end);
                let mut expr = Expr::deref(sigil, operand, span);
                match sigil {
                    Sigil::Scalar => expr = self.maybe_postfix_subscript(expr)?,
                    Sigil::Code => expr = self.maybe_call_args(expr)?,
                    _ => {}
                }
                Ok(FrameResult::Done(expr, min_prec))
            }
            ExprFrame::EvalExpr { span, min_prec } => {
                let end = span.merge(operand.span);
                Ok(FrameResult::Done(Expr::new(ExprKind::EvalExpr(Box::new(operand)), end), min_prec))
            }
            ExprFrame::DoExpr { span, min_prec } => {
                let end = span.merge(operand.span);
                Ok(FrameResult::Done(Expr::new(ExprKind::DoExpr(Box::new(operand)), end), min_prec))
            }
            ExprFrame::ReturnExpr { span, min_prec } => {
                let end = span.merge(operand.span);
                Ok(FrameResult::Done(Expr::new(ExprKind::FuncCall("CORE::return".into(), vec![operand]), end), min_prec))
            }
            ExprFrame::GotoExpr { span, min_prec } => {
                let end = span.merge(operand.span);
                Ok(FrameResult::Done(Expr::new(ExprKind::FuncCall("CORE::goto".into(), vec![operand]), end), min_prec))
            }
            ExprFrame::DumpExpr { span, min_prec } => {
                let end = span.merge(operand.span);
                Ok(FrameResult::Done(Expr::new(ExprKind::FuncCall("CORE::dump".into(), vec![operand]), end), min_prec))
            }
        }
    }

    /// Continue parsing an expression from a pre-built left-hand side.  Runs the Pratt operator loop without calling
    /// parse_term first.
    fn parse_expr_continuation(&mut self, mut left: Expr, min_prec: Precedence) -> Result<Expr, ParseError> {
        while let Some(info) = self.peek_op_info() {
            if info.left_prec() < min_prec {
                break;
            }
            left = self.parse_operator(left, info)?;
        }

        Ok(left)
    }

    // ── Term parsing ──────────────────────────────────────────
    fn parse_term(&mut self) -> Result<Expr, ParseError> {
        let spanned = self.next_token()?;
        let span = spanned.span;

        // Quote keywords must be handled before any peek_token() call — tokenizing the delimiter byte would be
        // destructive (e.g. `q/foo/` would lex `/` as Slash, losing the quote-op context).
        if let Token::Keyword(kw) = &spanned.token
            && keyword::is_quote_keyword(*kw)
        {
            return self.parse_quote_keyword(*kw, span);
        }

        // Weak keyword override: if this keyword has been declared as a sub (via `use subs`, `sub name;`, etc.), treat
        // it as an identifier in term position so the user sub takes precedence.  Infix position is unaffected — `"ab"
        // x 3` always works as repeat.
        if let Token::Keyword(kw) = &spanned.token
            && keyword::is_weak(*kw)
        {
            let name: &str = (*kw).into();
            if self.symbols.lookup(name, &self.current_package).is_some() {
                return self.parse_ident_term(name.to_string(), span);
            }
        }

        match spanned.token {
            Token::IntLit(n) => Ok(Expr::new(ExprKind::IntLit(n), span)),
            Token::FloatLit(n) => Ok(Expr::new(ExprKind::FloatLit(n), span)),
            Token::StrLit(s) => Ok(Expr::new(ExprKind::StringLit(s), span)),
            Token::VersionLit(s) => Ok(Expr::new(ExprKind::VersionLit(s), span)),

            // Interpolating string: collect sub-tokens into AST.
            Token::QuoteSublexBegin(_, _) => self.parse_interpolated_string(span),

            // << in term position: try heredoc.  The lexer emitted ShiftLeft; we ask it to attempt heredoc detection.
            // If it can't find a valid tag, that's a parse error (shift-left is not a valid term).
            Token::ShiftLeft => {
                match self.lexer.lex_heredoc_after_shift_left()? {
                    Some(Token::QuoteSublexBegin(kind, delim)) => {
                        let body_span = self.peek_span();
                        self.parse_interpolated_string(body_span.merge(span)).map(|mut e| {
                            e.span = span.merge(e.span);
                            let _ = (kind, delim);
                            e
                        })
                    }
                    Some(Token::HeredocLit(_kind, _tag, body)) => Ok(Expr::new(ExprKind::StringLit(body), span)),

                    // <<>> double diamond — safe version of <>.
                    Some(Token::Readline(content, safe)) => Self::readline_expr(content, safe, span),
                    Some(other) => unreachable!("unexpected heredoc token: {other:?}"),
                    None => Err(ParseError::new("expected heredoc tag after <<", span)),
                }
            }

            Token::ScalarVar(name) => {
                let expr = Expr::new(ExprKind::ScalarVar(name), span);
                self.maybe_postfix_subscript(expr)
            }
            Token::ArrayVar(name) => {
                // @array[0,1] → array slice; @array{qw(a b)} → hash slice
                if self.at(&Token::LeftBracket)? {
                    self.next_token()?;
                    let mut indices = Vec::new();
                    while !self.at(&Token::RightBracket)? && !self.at_eof()? {
                        indices.push(self.parse_expr(PREC_COMMA + 1)?);
                        if !self.eat(&Token::Comma)? {
                            break;
                        }
                    }
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBracket)?;
                    Ok(Expr::new(ExprKind::ArraySlice(Box::new(Expr::new(ExprKind::ArrayVar(name), span)), indices), span.merge(end)))
                } else if self.at(&Token::LeftBrace)? {
                    self.next_token()?;
                    let mut keys = Vec::new();
                    while !self.at(&Token::RightBrace)? && !self.at_eof()? {
                        keys.push(self.parse_expr(PREC_COMMA + 1)?);
                        if !self.eat(&Token::Comma)? {
                            break;
                        }
                    }
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBrace)?;
                    Ok(Expr::new(ExprKind::HashSlice(Box::new(Expr::new(ExprKind::ArrayVar(name), span)), keys), span.merge(end)))
                } else {
                    Ok(Expr::new(ExprKind::ArrayVar(name), span))
                }
            }
            Token::HashVar(name) => {
                // %hash{keys} → kv hash slice; %hash[indices] → kv array slice (5.20+)
                let recv = Expr::new(ExprKind::HashVar(name), span);
                self.maybe_kv_slice(recv, span)
            }
            Token::GlobVar(name) => {
                let expr = Expr::new(ExprKind::GlobVar(name), span);

                // *foo{THING} — typeglob slot access
                if self.at(&Token::LeftBrace)? {
                    self.next_token()?;
                    let key = self.parse_hash_subscript_key()?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBrace)?;
                    let span = span.merge(end);
                    Ok(Expr::arrow_deref(expr, ArrowTarget::hash_elem(key), span))
                } else {
                    Ok(expr)
                }
            }
            Token::ArrayLen(name) => Ok(Expr::new(ExprKind::ArrayLen(name), span)),
            Token::SpecialVar(name) => {
                let expr = Expr::new(ExprKind::SpecialVar(name), span);
                self.maybe_postfix_subscript(expr)
            }
            Token::SpecialArrayVar(name) => Ok(Expr::new(ExprKind::SpecialArrayVar(name), span)),
            Token::SpecialHashVar(name) => Ok(Expr::new(ExprKind::SpecialHashVar(name), span)),

            Token::Ident(name) => self.parse_ident_term(name, span),

            // Compile-time special literals.  SourceFile/SourceLine carry lex-time values; __PACKAGE__ is resolved from
            // the parser's state.  __SUB__ and __CLASS__ are feature-gated — the lexer only emits them as keywords when
            // the feature is active (otherwise they become Ident, falling through to the arm above).
            Token::SourceFile(path) => Ok(Expr::new(ExprKind::SourceFile(path), span)),
            Token::SourceLine(n) => Ok(Expr::new(ExprKind::SourceLine(n), span)),
            Token::Keyword(Keyword::__PACKAGE__) => {
                let pkg = self.current_package.to_string();
                Ok(Expr::new(ExprKind::CurrentPackage(pkg), span))
            }
            Token::Keyword(Keyword::__SUB__) => Ok(Expr::new(ExprKind::CurrentSub, span)),
            Token::Keyword(Keyword::__CLASS__) => Ok(Expr::new(ExprKind::CurrentClass, span)),

            Token::Keyword(Keyword::Undef) => Ok(Expr::new(ExprKind::Undef, span)),
            Token::Keyword(Keyword::Wantarray) => Ok(Expr::new(ExprKind::Wantarray, span)),

            // Declaration in expression context: my $x, our ($a, $b), state $x
            Token::Keyword(Keyword::My) | Token::Keyword(Keyword::Our) | Token::Keyword(Keyword::State) => {
                let scope = match spanned.token {
                    Token::Keyword(Keyword::My) => DeclScope::My,
                    Token::Keyword(Keyword::Our) => DeclScope::Our,
                    Token::Keyword(Keyword::State) => DeclScope::State,
                    _ => unreachable!(),
                };
                self.parse_decl_expr(scope, span)
            }

            // Anonymous sub: sub { ... } or sub ($x) { ... }
            Token::Keyword(Keyword::Sub) => self.parse_anon_sub(span),

            // Anonymous method: method { ... } or method ($x) { ... }
            Token::Keyword(Keyword::Method) => self.parse_anon_method(span),

            // last/next/redo with optional label
            Token::Keyword(Keyword::Last) | Token::Keyword(Keyword::Next) | Token::Keyword(Keyword::Redo) => {
                let name = match spanned.token {
                    Token::Keyword(Keyword::Last) => "CORE::last",
                    Token::Keyword(Keyword::Next) => "CORE::next",
                    Token::Keyword(Keyword::Redo) => "CORE::redo",
                    _ => unreachable!(),
                };

                // Optional label argument
                if let Token::Ident(_) = self.peek_token() {
                    let label_span = self.peek_span();
                    let label = match self.next_token()?.token {
                        Token::Ident(s) => s,
                        _ => unreachable!(),
                    };
                    let end = span.merge(label_span);
                    Ok(Expr::new(ExprKind::FuncCall(name.into(), vec![Expr::new(ExprKind::StringLit(label), label_span)]), end))
                } else {
                    Ok(Expr::new(ExprKind::FuncCall(name.into(), vec![]), span))
                }
            }

            // break — exits a given/when block.  No label argument.
            Token::Keyword(Keyword::Break) => Ok(Expr::new(ExprKind::FuncCall("CORE::break".into(), vec![]), span)),

            // continue — falls through to the next when in a given block.  Different from `continue BLOCK` after loops,
            // which is handled at statement level.
            Token::Keyword(Keyword::Continue) => Ok(Expr::new(ExprKind::FuncCall("CORE::continue".into(), vec![]), span)),

            // `x` is a weak keyword: in prefix position it acts as an identifier (function call / bareword).  In infix
            // position the Pratt parser handles it as the repeat operator.
            Token::Keyword(Keyword::X) => self.parse_ident_term("x".to_string(), span),

            // stat / lstat — dedicated AST nodes with StatTarget
            Token::Keyword(Keyword::Stat) => self.parse_stat_op(false, span),
            Token::Keyword(Keyword::Lstat) => self.parse_stat_op(true, span),

            // Nullary keywords — never consume arguments
            Token::Keyword(kw) if keyword::is_nullary(kw) => self.parse_nullary(kw, span),

            // Named unary keywords
            Token::Keyword(kw) if keyword::is_named_unary(kw) => self.parse_named_unary(kw, span),

            // List operators
            Token::Keyword(kw) if keyword::is_list_op(kw) => self.parse_list_op(kw, span),

            Token::QwList(words) => Ok(Expr::new(ExprKind::QwList(words), span)),

            // Regex, substitution, transliteration
            Token::RegexSublexBegin(kind, _delim) => {
                let pattern = self.parse_interpolated()?;
                let flags = self.lexer.scan_adjacent_word_chars();
                if let Some(ref f) = flags {
                    Self::validate_regex_flags(f, span)?;
                }
                Ok(Expr::new(ExprKind::Regex(kind, pattern, flags), span))
            }

            // // in term position is an empty regex, not defined-or.
            Token::DefinedOr => {
                let flags = self.lexer.scan_adjacent_word_chars();
                if let Some(ref f) = flags {
                    Self::validate_regex_flags(f, span)?;
                }
                Ok(Expr::new(ExprKind::Regex(RegexKind::Match, Interpolated(vec![]), flags), span))
            }

            // / in term position is a regex, not division.
            Token::Slash => {
                let pattern = self.lexer.lex_body_str('/', true)?;
                let flags = self.lexer.scan_adjacent_word_chars();
                if let Some(ref f) = flags {
                    Self::validate_regex_flags(f, span)?;
                }
                Ok(Expr::new(ExprKind::Regex(RegexKind::Match, Interpolated(vec![InterpPart::Const(pattern)]), flags), span))
            }

            // /= in term position: = is the first character of the regex pattern, not a division-assignment operator.
            Token::Assign(AssignOp::DivEq) => {
                self.lexer.rewind(1);
                let pattern = self.lexer.lex_body_str('/', true)?;
                let flags = self.lexer.scan_adjacent_word_chars();
                if let Some(ref f) = flags {
                    Self::validate_regex_flags(f, span)?;
                }
                Ok(Expr::new(ExprKind::Regex(RegexKind::Match, Interpolated(vec![InterpPart::Const(pattern)]), flags), span))
            }
            Token::SubstSublexBegin(delim) => {
                // Collect pattern body tokens until SublexEnd.
                let pattern = self.parse_interpolated()?;

                // Set up the replacement body (virtual EOF, flags).
                let flags = self.lexer.start_subst_replacement(delim)?;
                if let Some(ref f) = flags {
                    Self::validate_subst_flags(f, span)?;
                }
                let has_eval = flags.as_ref().is_some_and(|f| f.contains('e'));

                let replacement = if has_eval {
                    // With /e: body is raw bytes in a single ConstSegment.  Reparse as code.
                    let raw = match self.peek_token().clone() {
                        Token::ConstSegment(s) => {
                            self.next_token()?;
                            s
                        }
                        Token::SublexEnd => String::new(),
                        other => return Err(ParseError::new(format!("unexpected token in s///e: {other:?}"), self.peek_span())),
                    };
                    self.expect_token(&Token::SublexEnd)?;
                    let repl_src = format!("{};", raw);
                    let prog = crate::parse(repl_src.as_bytes()).map_err(|e| ParseError::new(format!("in s///e replacement: {}", e.message), span))?;
                    let expr = match prog.statements.into_iter().next() {
                        Some(Statement { kind: StmtKind::Expr(expr), .. }) => expr,
                        _ => Expr::new(ExprKind::StringLit(raw), span),
                    };
                    Interpolated(vec![InterpPart::ExprInterp(Box::new(expr))])
                } else {
                    // Without /e: body is an interpolated string.
                    self.parse_interpolated()?
                };
                let end = self.peek_span();
                Ok(Expr::new(ExprKind::Subst(pattern, replacement, flags), span.merge(end)))
            }
            Token::TranslitLit(from, to, flags) => {
                if let Some(ref f) = flags {
                    Self::validate_tr_flags(f, span)?;
                }
                Ok(Expr::new(ExprKind::Translit(from, to, flags), span))
            }

            // Heredoc (body already collected by lexer).  Literal heredocs (body collected by lexer as raw string).
            // Interpolating heredocs come through QuoteSublexBegin → tokens → SublexEnd.
            Token::HeredocLit(_kind, _tag, body) => Ok(Expr::new(ExprKind::StringLit(body), span)),

            // sort/map/grep with optional block
            Token::Keyword(kw) if keyword::is_block_list_op(kw) => self.parse_block_list_op(kw, span),

            // print/say with optional filehandle
            Token::Keyword(kw) if keyword::is_print_op(kw) => self.parse_print_op(kw, span),

            // Filetest operators: -e, -f, -d, etc. (lexed as single token)
            Token::Filetest(test_byte) => self.parse_filetest(test_byte, span),

            // Yada yada yada (...)
            Token::DotDotDot => Ok(Expr::new(ExprKind::YadaYada, span)),

            // Readline / diamond: <STDIN>, <>, <$fh>, <*.txt>
            Token::Readline(content, safe) => Self::readline_expr(content, safe, span),

            // < in term position: try readline.  The lexer emitted NumLt; we ask it to attempt readline scanning.  If
            // not a readline, that's a parse error (less-than is not a valid term).
            Token::NumLt => {
                if let Some(Token::Readline(content, safe)) = self.lexer.lex_readline_after_lt() {
                    let end = self.peek_span();
                    Self::readline_expr(content, safe, span.merge(end))
                } else {
                    Err(ParseError::new("expected readline or glob after <", span))
                }
            }

            // `.` in term position: try leading-dot float (`.5`, `.5e2`) or v-string (`.5.6`).  In operator position,
            // `.` is concat (handled by peek_op_info) — same disambiguation pattern as `/` (regex vs division).
            Token::Dot => match self.lexer.lex_leading_dot_float()? {
                Some(Token::FloatLit(n)) => {
                    let end = self.peek_span().start;
                    Ok(Expr::new(ExprKind::FloatLit(n), Span::new(span.start, end)))
                }
                Some(Token::VersionLit(v)) => {
                    let end = self.peek_span().start;
                    Ok(Expr::new(ExprKind::VersionLit(v), Span::new(span.start, end)))
                }
                _ => Err(ParseError::new("expected expression, got Dot", span)),
            },

            other => Err(ParseError::new(format!("expected expression, got {other:?}"), span)),
        }
    }

    /// Handle a quote keyword (`q`, `qq`, `qw`, `qr`, `qx`, `m`, `s`, `tr`, `y`) received in `parse_term`.  Skips
    /// whitespace and peeks at the raw delimiter byte to decide whether to start sublexing.  Fat-comma autoquoting
    /// (e.g. `q => 1`) is handled by the lexer, which returns StrLit instead of the keyword.  Must be called BEFORE any
    /// `peek_token()` — tokenizing the delimiter byte would be destructive.
    fn parse_quote_keyword(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let raw = self.lexer.skip_ws_and_peek_byte();

        // No delimiter byte (EOF) — treat as bareword.
        if raw.is_none() {
            let name: &str = kw.into();
            return Ok(Expr::new(ExprKind::Bareword(name.to_string()), span));
        }

        // Start sublexing — the lexer reads the delimiter and begins scanning the body.
        self.dispatch_quote_result(kw, span)
    }

    /// Enter sublexing for a quote keyword and dispatch the resulting token.  The lexer must be positioned at (or
    /// before whitespace preceding) the delimiter byte.
    fn dispatch_quote_result(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let token = self.lexer.begin_quote_sublex(kw)?;
        match token {
            Token::StrLit(s) => Ok(Expr::new(ExprKind::StringLit(s), span)),
            Token::QwList(words) => Ok(Expr::new(ExprKind::QwList(words), span)),
            Token::QuoteSublexBegin(_, _) => self.parse_interpolated_string(span),
            Token::RegexSublexBegin(kind, _delim) => {
                let pattern = self.parse_interpolated()?;
                let flags = self.lexer.scan_adjacent_word_chars();
                if let Some(ref f) = flags {
                    Self::validate_regex_flags(f, span)?;
                }
                Ok(Expr::new(ExprKind::Regex(kind, pattern, flags), span))
            }
            Token::SubstSublexBegin(delim) => {
                let pattern = self.parse_interpolated()?;
                let flags = self.lexer.start_subst_replacement(delim)?;
                if let Some(ref f) = flags {
                    Self::validate_subst_flags(f, span)?;
                }
                let has_eval = flags.as_ref().is_some_and(|f| f.contains('e'));
                let replacement = if has_eval {
                    let raw = match self.peek_token().clone() {
                        Token::ConstSegment(s) => {
                            self.next_token()?;
                            s
                        }
                        Token::SublexEnd => String::new(),
                        other => return Err(ParseError::new(format!("unexpected token in s///e: {other:?}"), self.peek_span())),
                    };
                    self.expect_token(&Token::SublexEnd)?;
                    let repl_src = format!("{};", raw);
                    let prog = crate::parse(repl_src.as_bytes()).map_err(|e| ParseError::new(format!("in s///e replacement: {}", e.message), span))?;
                    let expr = match prog.statements.into_iter().next() {
                        Some(Statement { kind: StmtKind::Expr(expr), .. }) => expr,
                        _ => Expr::new(ExprKind::StringLit(raw), span),
                    };
                    Interpolated(vec![InterpPart::ExprInterp(Box::new(expr))])
                } else {
                    self.parse_interpolated()?
                };
                let end = self.peek_span();
                Ok(Expr::new(ExprKind::Subst(pattern, replacement, flags), span.merge(end)))
            }
            Token::TranslitLit(from, to, flags) => {
                if let Some(ref f) = flags {
                    Self::validate_tr_flags(f, span)?;
                }
                Ok(Expr::new(ExprKind::Translit(from, to, flags), span))
            }
            other => Err(ParseError::new(format!("unexpected token from quote sublexer: {other:?}"), span)),
        }
    }

    fn parse_ident_term(&mut self, name: String, span: Span) -> Result<Expr, ParseError> {
        // Look up in the symbol table to see if this is a known sub.  Clone the prototype (small: raw string + a Vec of
        // slot enums) and the "is known" flag so we can release the borrow on self before parsing args.
        let (is_known_sub, proto) = match self.symbols.lookup(&name, &self.current_package) {
            Some(info) => (true, info.prototype.clone()),
            None => (false, None),
        };

        // Check if followed by `(` — function call
        if self.at(&Token::LeftParen)? {
            self.next_token()?;
            let mut args = Vec::new();
            while !self.at(&Token::RightParen)? && !self.at_eof()? {
                args.push(self.parse_expr(PREC_COMMA + 1)?);
                if !self.eat(&Token::Comma)? {
                    break;
                }
            }
            let end = self.peek_span();
            self.expect_token(&Token::RightParen)?;
            return Ok(Expr::new(ExprKind::FuncCall(self.qualify_sub_name(&name), args), span.merge(end)));
        }

        // No parens — if we know this sub has a prototype, use it to drive argument parsing.
        if let Some(proto) = proto {
            return self.parse_prototyped_call(self.qualify_sub_name(&name), span, &proto);
        }

        // No parens, no prototype, but the sub is known: parse as a list operator call (greedy args until end-of-
        // statement).
        if is_known_sub {
            return self.parse_known_sub_call(self.qualify_sub_name(&name), span);
        }

        // Indirect object syntax: METHOD CLASS ARGS
        // e.g. new Foo(args), new Foo args
        // Heuristic: bareword followed by a capitalized bareword or $var.
        // Requires `use feature 'indirect'` (in :default, dropped from :5.36+).
        if self.pragmas.features.contains(Features::INDIRECT) {
            match self.peek_token() {
                Token::Ident(class_name) if class_name.starts_with(|c: char| c.is_ascii_uppercase()) => {
                    let class_name = class_name.clone();
                    let class_span = self.peek_span();
                    self.next_token()?; // eat class name
                    let class_expr = Expr::new(ExprKind::Bareword(class_name), class_span);

                    // Optional args
                    let mut args = Vec::new();
                    if self.at(&Token::LeftParen)? {
                        self.next_token()?;
                        while !self.at(&Token::RightParen)? && !self.at_eof()? {
                            args.push(self.parse_expr(PREC_COMMA + 1)?);
                            if !self.eat(&Token::Comma)? {
                                break;
                            }
                        }
                        let end = self.peek_span();
                        self.expect_token(&Token::RightParen)?;
                        return Ok(Expr::new(ExprKind::IndirectMethodCall(Box::new(class_expr), name, args), span.merge(end)));
                    }

                    return Ok(Expr::new(ExprKind::IndirectMethodCall(Box::new(class_expr), name, args), span.merge(class_span)));
                }
                Token::ScalarVar(_) => {
                    let var_span = self.peek_span();
                    let var = match self.next_token()?.token {
                        Token::ScalarVar(n) => n,
                        _ => unreachable!(),
                    };
                    let invocant = Expr::new(ExprKind::ScalarVar(var), var_span);

                    let mut args = Vec::new();
                    if self.at(&Token::LeftParen)? {
                        self.next_token()?;
                        while !self.at(&Token::RightParen)? && !self.at_eof()? {
                            args.push(self.parse_expr(PREC_COMMA + 1)?);
                            if !self.eat(&Token::Comma)? {
                                break;
                            }
                        }
                        let end = self.peek_span();
                        self.expect_token(&Token::RightParen)?;
                        return Ok(Expr::new(ExprKind::IndirectMethodCall(Box::new(invocant), name, args), span.merge(end)));
                    }

                    return Ok(Expr::new(ExprKind::IndirectMethodCall(Box::new(invocant), name, args), span.merge(var_span)));
                }
                _ => {}
            }
        } // INDIRECT feature gate

        // Bare identifier — not followed by ( or indirect object context.
        Ok(Expr::new(ExprKind::Bareword(name), span))
    }

    /// True if the current token marks the end of a list-op / prototyped argument list: statement terminator, closing
    /// bracket/brace/paren, EOF, or a postfix-control keyword.
    fn at_args_end(&mut self) -> Result<bool, ParseError> {
        Ok(self.at(&Token::Semi)?
            || self.at(&Token::RightParen)?
            || self.at(&Token::RightBracket)?
            || self.at(&Token::RightBrace)?
            || self.at_eof()?
            || matches!(
                self.peek_token(),
                Token::Keyword(Keyword::If)
                    | Token::Keyword(Keyword::Unless)
                    | Token::Keyword(Keyword::While)
                    | Token::Keyword(Keyword::Until)
                    | Token::Keyword(Keyword::For)
                    | Token::Keyword(Keyword::Foreach)
            ))
    }

    /// Parse a call to a known sub (no prototype) in list-operator style: greedy comma-separated args until end of
    /// statement.  Produces `FuncCall` (not `ListOp`, which is reserved for built-in list operators like `push`,
    /// `join`).
    fn parse_known_sub_call(&mut self, name: String, start: Span) -> Result<Expr, ParseError> {
        let mut args = Vec::new();
        while !self.at_args_end()? {
            args.push(self.parse_expr(PREC_COMMA + 1)?);
            if !self.eat(&Token::Comma)? {
                break;
            }
        }
        let end_span = args.last().map(|a| a.span).unwrap_or(start);
        Ok(Expr::new(ExprKind::FuncCall(name, args), start.merge(end_span)))
    }

    /// Parse a call whose target sub has a known prototype.  Arguments are consumed according to the prototype slots;
    /// trailing content is left for the outer parser to deal with.
    ///
    /// * `$`, `_`, `*`, `+`, `\X`, `\[...]` — one scalar-ish expression per slot, stopping at comma precedence.
    ///   Optional comma consumed between slots.
    /// * `&` — expect `{ ... }`, parsed as an anonymous sub body.
    /// * `@`, `%` — slurpy, consumes all remaining comma-separated arguments.  Always last.
    ///
    /// Missing required arguments are silently tolerated (Perl would error at compile time).  A later semantic pass can
    /// validate.
    fn parse_prototyped_call(&mut self, name: String, start: Span, proto: &SubPrototype) -> Result<Expr, ParseError> {
        let mut args = Vec::new();

        for (i, slot) in proto.slots.iter().enumerate() {
            let is_optional = i >= proto.required;

            if self.at_args_end()? {
                // No more input.  The `_` slot is special: when omitted, it defaults to the global default variable
                // ($_), regardless of required/optional status.  All other slots simply stop; a later semantic pass can
                // validate required-arg counts.
                if matches!(slot, ProtoSlot::DefaultedScalar) {
                    args.push(Expr::new(ExprKind::DefaultVar, self.peek_span()));
                }
                let _ = is_optional;
                break;
            }

            match slot {
                ProtoSlot::Block => {
                    // `&` slot accepts either:
                    //   - A literal block `{ ... }`, but ONLY when this is the initial slot.  That's the map/grep/sort
                    //     pattern: `foo { ... } @list`.  In non-initial positions, `{` at a call site is an ordinary
                    //     hash-ref constructor; code references must be explicit.
                    //   - A code reference expression: `\&name`, `$coderef`, `sub { ... }`, etc.  Parsed at named-unary
                    //     precedence.
                    let arg = if i == 0 && self.at(&Token::LeftBrace)? {
                        let block = self.parse_block()?;
                        let span = block.span;
                        Expr::anon_sub(None, vec![], None, block, span)
                    } else {
                        self.parse_expr(PREC_NAMED_UNARY)?
                    };
                    args.push(arg);

                    // Optional comma between slots.
                    if i + 1 < proto.slots.len() {
                        self.eat(&Token::Comma)?;
                    }
                }
                ProtoSlot::SlurpyList | ProtoSlot::SlurpyHash => {
                    // Consume all remaining tokens as comma-separated expressions.  Slurpy is always last.
                    while !self.at_args_end()? {
                        args.push(self.parse_expr(PREC_COMMA + 1)?);
                        if !self.eat(&Token::Comma)? {
                            break;
                        }
                    }
                    break;
                }
                ProtoSlot::Glob => {
                    // `*` slot: a bare identifier is auto-promoted to a typeglob reference (e.g., `foo STDIN` becomes
                    // `foo(*STDIN)`).  Any other expression — a glob literal `*NAME`, a scalar holding a glob ref, etc.
                    // — is parsed normally at named-unary precedence.
                    let arg = if let Token::Ident(_) = self.peek_token() {
                        let glob_span = self.peek_span();
                        let name = match self.next_token()?.token {
                            Token::Ident(n) => n,
                            _ => unreachable!(),
                        };
                        Expr::new(ExprKind::GlobVar(name), glob_span)
                    } else {
                        self.parse_expr(PREC_NAMED_UNARY)?
                    };
                    args.push(arg);
                    if i + 1 < proto.slots.len() {
                        self.eat(&Token::Comma)?;
                    }
                }
                ProtoSlot::AutoRef(_) | ProtoSlot::AutoRefOneOf(_) | ProtoSlot::ArrayOrHash => {
                    // Auto-reference slots: `\$`, `\@`, `\%`, `\&`, `\*`, `\[...]`, and `+` (which is effectively
                    // `\[@%]`).  The argument is parsed at named-unary precedence and then wrapped in a Ref expression
                    // — the call site receives a reference to the variable rather than its value.  Whether the argument
                    // is actually of the expected kind (array for `\@`, etc.) is a semantic-pass concern, not a parsing
                    // one.
                    let arg = self.parse_expr(PREC_NAMED_UNARY)?;
                    let span = arg.span;
                    args.push(Expr::reference(arg, span));
                    if i + 1 < proto.slots.len() {
                        self.eat(&Token::Comma)?;
                    }
                }
                _ => {
                    // Scalar-ish slot (`$`, `_`).  One expression at named-unary precedence: operators tighter than
                    // named unary (+ - * / << >>, etc.) are consumed; operators looser (< == , ?:, etc.) terminate the
                    // arg.  This matches Perl's semantics for prototyped subs whose slot is a single scalar.
                    let arg = self.parse_expr(PREC_NAMED_UNARY)?;
                    args.push(arg);
                    if i + 1 < proto.slots.len() {
                        self.eat(&Token::Comma)?;
                    }
                }
            }
        }

        let end_span = args.last().map(|a| a.span).unwrap_or(start);
        Ok(Expr::new(ExprKind::FuncCall(name, args), start.merge(end_span)))
    }

    /// Convert a keyword to its `CORE::name` form for the AST.  Keyword-dispatched function calls use this so the
    /// compiler can distinguish builtins (`CORE::abs`) from user subs (`abs`).
    fn core_name(kw: &Keyword) -> String {
        format!("CORE::{}", <&str>::from(*kw))
    }

    /// Package-qualify a bare sub name for the AST.  `foo` in package `Bar` → `Bar::foo`.  Already-qualified names like
    /// `Foo::bar` are left unchanged.
    fn qualify_sub_name(&self, name: &str) -> String {
        if name.contains("::") { name.to_string() } else { format!("{}::{}", self.current_package, name) }
    }

    /// Parse a nullary builtin.  These never consume arguments, so `time+86_400` is always `time() + 86_400`.  Explicit
    /// empty parens (`time()`) are accepted.
    fn parse_nullary(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let name = Self::core_name(&kw);

        // Accept optional empty parens: `time()`
        if self.at(&Token::LeftParen)? {
            self.next_token()?; // consume (
            let end = self.peek_span();
            self.expect_token(&Token::RightParen)?;
            return Ok(Expr::new(ExprKind::FuncCall(name, vec![]), span.merge(end)));
        }

        // No parens — emit as a zero-arg call; the next token is an operator, not an argument.
        Ok(Expr::new(ExprKind::FuncCall(name, vec![]), span))
    }

    fn parse_named_unary(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let name = Self::core_name(&kw);

        // Named unary with optional arg — check for tokens that indicate "no argument follows."
        if self.at(&Token::Semi)?
            || self.at_eof()?
            || self.at(&Token::RightBrace)?
            || self.at(&Token::RightParen)?
            || self.at(&Token::Comma)?
            || self.at(&Token::RightBracket)?
            || matches!(
                self.peek_token(),
                Token::Keyword(
                    Keyword::If
                        | Keyword::Unless
                        | Keyword::While
                        | Keyword::Until
                        | Keyword::For
                        | Keyword::Foreach
                        | Keyword::Or
                        | Keyword::And
                        | Keyword::Xor
                )
            )
        {
            // No argument
            return Ok(Expr::new(ExprKind::FuncCall(name, vec![]), span));
        }

        // Operators that prefer defined-or: // after shift/pop/undef/etc.  is defined-or, not an empty regex argument.
        // Matches toke.c's XTERMORDORDOR.
        if keyword::prefers_defined_or(kw) && matches!(self.peek_token(), Token::DefinedOr) {
            return Ok(Expr::new(ExprKind::FuncCall(name, vec![]), span));
        }

        if self.at(&Token::LeftParen)? {
            self.next_token()?;
            let arg = self.parse_expr(PREC_LOW)?;
            let end = self.peek_span();
            self.expect_token(&Token::RightParen)?;
            return Ok(Expr::new(ExprKind::FuncCall(name, vec![arg]), span.merge(end)));
        }

        // Parse one term as the argument at named-unary precedence.  Named unary binds tighter than ternary,
        // comparison, logical operators: `defined $x ? 1 : 0` is `defined($x) ? 1 : 0`, `defined $x || $y` is
        // `defined($x) || $y`.  But it binds looser than arithmetic: `lc $x . $y` is `lc($x . $y)`.
        let arg = self.parse_expr(PREC_NAMED_UNARY)?;
        let end = span.merge(arg.span);
        Ok(Expr::new(ExprKind::FuncCall(name, vec![arg]), end))
    }

    /// Parse the target of a stat-family operation (filetest, stat, lstat).
    ///
    /// Handles three cases:
    /// - Bare `_` → `StatTarget::StatCache`
    /// - No operand (`;`, `}`, `)`, EOF) → `StatTarget::Default`
    /// - Expression → `StatTarget::Expr(Box::new(expr))`
    ///
    /// Also handles the parenthesized form: `stat(_)`, `stat($file)`.  Returns `(target, end_span)`.  Parse a filetest
    /// expression given the test byte and the span of the leading `-X` tokens.  Shared between the Minus-triggered path
    /// and the explicit Filetest token arm.  Build an Expr from readline content: `<>` is readline/ARGV, `<*.txt>`
    /// (with wildcards) is glob, otherwise `<FH>` is readline.
    fn readline_expr(content: String, safe: bool, span: Span) -> Result<Expr, ParseError> {
        if content.is_empty() {
            // `<>` (safe=false) or `<<>>` (safe=true).
            let name = if safe { "CORE::readline_safe" } else { "CORE::readline" };
            Ok(Expr::new(ExprKind::FuncCall(name.into(), vec![]), span))
        } else if content.contains('*') || content.contains('?') {
            Ok(Expr::new(ExprKind::FuncCall("CORE::glob".into(), vec![Expr::new(ExprKind::StringLit(content), span)]), span))
        } else {
            Ok(Expr::new(ExprKind::FuncCall("CORE::readline".into(), vec![Expr::new(ExprKind::StringLit(content), span)]), span))
        }
    }

    fn parse_filetest(&mut self, test_byte: u8, span: Span) -> Result<Expr, ParseError> {
        let test_char = test_byte as char;
        let (target, end) = self.parse_stat_target(span)?;
        Ok(Expr::new(ExprKind::Filetest(test_char, target), span.merge(end)))
    }

    fn parse_stat_target(&mut self, start: Span) -> Result<(StatTarget, Span), ParseError> {
        // Parenthesized form: stat($file), stat(_)
        if self.at(&Token::LeftParen)? {
            self.next_token()?;
            let (target, _) = self.parse_stat_target_inner(start)?;
            let end = self.peek_span();
            self.expect_token(&Token::RightParen)?;
            return Ok((target, end));
        }

        self.parse_stat_target_inner(start)
    }

    /// Inner helper: parse the stat target without handling parens.
    fn parse_stat_target_inner(&mut self, start: Span) -> Result<(StatTarget, Span), ParseError> {
        // No argument: ;, }, ), EOF, or // (defined-or, not empty regex — matches toke.c's FTST macro setting
        // XTERMORDORDOR).
        if self.at(&Token::Semi)?
            || self.at(&Token::RightBrace)?
            || self.at(&Token::RightParen)?
            || self.at_eof()?
            || matches!(self.peek_token(), Token::DefinedOr)
        {
            Ok((StatTarget::Default, start))
        } else if matches!(self.peek_token(), Token::Ident(name) if name == "_") {
            let end = self.peek_span();
            self.next_token()?;
            Ok((StatTarget::StatCache, end))
        } else {
            let expr = self.parse_expr(PREC_UNARY)?;
            let end = expr.span;
            Ok((StatTarget::Expr(Box::new(expr)), end))
        }
    }

    /// Parse `stat TARGET` or `lstat TARGET`.
    fn parse_stat_op(&mut self, is_lstat: bool, span: Span) -> Result<Expr, ParseError> {
        let (target, end) = self.parse_stat_target(span)?;
        let kind = if is_lstat { ExprKind::Lstat(target) } else { ExprKind::Stat(target) };
        Ok(Expr::new(kind, span.merge(end)))
    }

    fn parse_list_op(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let name = Self::core_name(&kw);

        // Check for parens
        if self.at(&Token::LeftParen)? {
            self.next_token()?;
            let mut args = Vec::new();
            while !self.at(&Token::RightParen)? && !self.at_eof()? {
                args.push(self.parse_expr(PREC_COMMA + 1)?);
                if !self.eat(&Token::Comma)? {
                    break;
                }
            }
            let end = self.peek_span();
            self.expect_token(&Token::RightParen)?;
            return Ok(Expr::new(ExprKind::ListOp(name, args), span.merge(end)));
        }

        // No parens — parse everything up to end of statement as args
        let mut args = Vec::new();
        while !self.at(&Token::Semi)? && !self.at_eof()? && !self.at(&Token::RightBrace)? {
            // Check for postfix control keywords
            if matches!(
                self.peek_token(),
                Token::Keyword(Keyword::If)
                    | Token::Keyword(Keyword::Unless)
                    | Token::Keyword(Keyword::While)
                    | Token::Keyword(Keyword::Until)
                    | Token::Keyword(Keyword::For)
                    | Token::Keyword(Keyword::Foreach)
            ) {
                break;
            }
            args.push(self.parse_expr(PREC_COMMA + 1)?);
            if !self.eat(&Token::Comma)? {
                break;
            }
        }

        let end_span = args.last().map(|a| a.span).unwrap_or(span);
        Ok(Expr::new(ExprKind::ListOp(name, args), span.merge(end_span)))
    }

    /// Parse sort/map/grep with optional block as first argument.  `sort { $a <=> $b } @list`, `map { ... } @list`,
    /// `grep { ... } @list`
    fn parse_block_list_op(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let name = Self::core_name(&kw);

        // Check for parens: sort(...), map(...), grep(...)
        if self.at(&Token::LeftParen)? {
            self.next_token()?;
            let mut args = Vec::new();

            // Check for block as first arg inside parens
            if self.at(&Token::LeftBrace)? {
                let block = self.parse_block()?;
                let span = block.span;
                args.push(Expr::anon_sub(None, vec![], None, block, span));
                self.eat(&Token::Comma)?;
            }
            while !self.at(&Token::RightParen)? && !self.at_eof()? {
                args.push(self.parse_expr(PREC_COMMA + 1)?);
                if !self.eat(&Token::Comma)? {
                    break;
                }
            }
            let end = self.peek_span();
            self.expect_token(&Token::RightParen)?;
            return Ok(Expr::new(ExprKind::ListOp(name, args), span.merge(end)));
        }

        let mut args = Vec::new();

        // Check for block or sub name as first arg
        if self.at(&Token::LeftBrace)? {
            let block = self.parse_block()?;
            let span = block.span;
            args.push(Expr::anon_sub(None, vec![], None, block, span));
        } else if kw == Keyword::Sort {
            // sort can also take a sub name: sort subname @list
            if let Token::Ident(_) = self.peek_token() {
                let ident_span = self.peek_span();
                let ident = match self.next_token()?.token {
                    Token::Ident(s) => s,
                    _ => unreachable!(),
                };
                args.push(Expr::new(ExprKind::Bareword(ident), ident_span));
            }
        }

        // Rest of arguments
        while !self.at(&Token::Semi)? && !self.at_eof()? && !self.at(&Token::RightBrace)? && !self.at(&Token::RightParen)? {
            if matches!(
                self.peek_token(),
                Token::Keyword(Keyword::If)
                    | Token::Keyword(Keyword::Unless)
                    | Token::Keyword(Keyword::While)
                    | Token::Keyword(Keyword::Until)
                    | Token::Keyword(Keyword::For)
                    | Token::Keyword(Keyword::Foreach)
            ) {
                break;
            }
            args.push(self.parse_expr(PREC_COMMA + 1)?);
            if !self.eat(&Token::Comma)? {
                break;
            }
        }

        let end_span = args.last().map(|a| a.span).unwrap_or(span);
        Ok(Expr::new(ExprKind::ListOp(name, args), span.merge(end_span)))
    }

    /// Parse print/say with optional filehandle as first argument.  `print STDERR "error"`, `print "hello"`, `say $fh
    /// "data"`
    fn parse_print_op(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let name = Self::core_name(&kw);

        // Handle optional parens — print(...) form
        let in_parens = self.eat(&Token::LeftParen)?;

        // Try to detect filehandle before argument list.  Consume-then-decide: take the candidate token, peek at what
        // follows to determine if it's a filehandle or the first argument.
        let mut filehandle: Option<Box<Expr>> = None;
        let mut first_arg: Option<Expr> = None;

        let is_bareword = matches!(self.peek_token(), Token::Ident(_)) && self.pragmas.features.contains(Features::BAREWORD_FILEHANDLES);
        let is_scalar = matches!(self.peek_token(), Token::ScalarVar(_));

        if is_bareword {
            let fh_span = self.peek_span();
            let fh_name = match self.next_token()?.token {
                Token::Ident(n) => n,
                _ => unreachable!(),
            };
            if matches!(self.peek_token(), Token::Comma) {
                // Bareword followed by comma → first argument, not filehandle.  `print CONSTANT, "hello"`.
                let initial = self.parse_ident_term(fh_name, fh_span)?;
                let expr = self.parse_expr_continuation(initial, PREC_COMMA + 1)?;
                first_arg = Some(expr);
            } else {
                // Bareword not followed by comma → filehandle.  `print STDERR "hello"`.
                filehandle = Some(Box::new(Expr::new(ExprKind::Bareword(fh_name), fh_span)));
            }
        } else if is_scalar {
            let var_span = self.peek_span();
            let var_name = match self.next_token()?.token {
                Token::ScalarVar(n) => n,
                _ => unreachable!(),
            };
            let next_is_term = matches!(
                self.peek_token(),
                Token::QuoteSublexBegin(_, _)
                    | Token::StrLit(_)
                    | Token::IntLit(_)
                    | Token::FloatLit(_)
                    | Token::ScalarVar(_)
                    | Token::ArrayVar(_)
                    | Token::HashVar(_)
                    | Token::SpecialVar(_)
                    | Token::SpecialArrayVar(_)
                    | Token::SpecialHashVar(_)
                    | Token::Ident(_)
                    | Token::LeftParen
                    | Token::LeftBracket
                    | Token::RegexSublexBegin(_, _)
                    | Token::SubstSublexBegin(_)
                    | Token::HeredocLit(_, _, _)
                    | Token::QwList(_)
                    | Token::Backslash
            );
            if next_is_term {
                // `print $fh "hello"` → filehandle.
                filehandle = Some(Box::new(Expr::new(ExprKind::ScalarVar(var_name), var_span)));
            } else {
                // `print $x + 1` → not filehandle, first argument.
                let var_expr = Expr::new(ExprKind::ScalarVar(var_name), var_span);
                let initial = self.maybe_postfix_subscript(var_expr)?;
                let expr = self.parse_expr_continuation(initial, PREC_COMMA + 1)?;
                first_arg = Some(expr);
            }
        }

        // Collect args as comma-separated list.
        let mut args = Vec::new();
        if let Some(arg) = first_arg {
            args.push(arg);

            // Consume comma after the first arg if present.
            if !self.eat(&Token::Comma)? {
                // No comma — this was the only argument.
                if in_parens {
                    self.expect_token(&Token::RightParen)?;
                }
                let end_span = args.last().map(|a| a.span).unwrap_or(span);
                return Ok(Expr::new(ExprKind::PrintOp(name, filehandle, args), span.merge(end_span)));
            }
        }
        while !self.at_print_end(in_parens)? {
            args.push(self.parse_expr(PREC_COMMA + 1)?);
            if !self.eat(&Token::Comma)? {
                break;
            }
        }

        if in_parens {
            self.expect_token(&Token::RightParen)?;
        }

        let end_span = args.last().map(|a| a.span).unwrap_or(span);
        Ok(Expr::new(ExprKind::PrintOp(name, filehandle, args), span.merge(end_span)))
    }

    /// Check whether we're at the end of a print argument list.
    fn at_print_end(&mut self, in_parens: bool) -> Result<bool, ParseError> {
        if self.at(&Token::Semi)? || self.at_eof()? || self.at(&Token::RightBrace)? {
            return Ok(true);
        }
        if in_parens && self.at(&Token::RightParen)? {
            return Ok(true);
        }
        Ok(matches!(
            self.peek_token(),
            Token::Keyword(Keyword::If)
                | Token::Keyword(Keyword::Unless)
                | Token::Keyword(Keyword::While)
                | Token::Keyword(Keyword::Until)
                | Token::Keyword(Keyword::For)
                | Token::Keyword(Keyword::Foreach)
        ))
    }

    /// Parse the operand of a prefix dereference ($$ref, @$ref, etc.).  Consumes just the variable — subscripts are NOT
    /// included.  This ensures $$ref[0] parses as ($$ref)[0], not $(${ref}[0]).
    fn parse_deref_operand(&mut self) -> Result<Expr, ParseError> {
        let spanned = self.next_token()?;
        let span = spanned.span;
        match spanned.token {
            Token::ScalarVar(name) => Ok(Expr::new(ExprKind::ScalarVar(name), span)),
            Token::ArrayVar(name) => Ok(Expr::new(ExprKind::ArrayVar(name), span)),
            Token::HashVar(name) => Ok(Expr::new(ExprKind::HashVar(name), span)),
            Token::SpecialVar(name) => Ok(Expr::new(ExprKind::SpecialVar(name), span)),
            Token::SpecialArrayVar(name) => Ok(Expr::new(ExprKind::SpecialArrayVar(name), span)),
            Token::SpecialHashVar(name) => Ok(Expr::new(ExprKind::SpecialHashVar(name), span)),

            // Recursive: $$$ref
            Token::Dollar => {
                let inner = self.parse_deref_operand()?;
                let span = span.merge(inner.span);
                Ok(Expr::deref(Sigil::Scalar, inner, span))
            }
            other => Err(ParseError::new(format!("expected variable after dereference sigil, got {other:?}"), span)),
        }
    }

    /// If `(` follows, parse arguments for a coderef call: `&$ref(args)`.
    fn maybe_call_args(&mut self, callee: Expr) -> Result<Expr, ParseError> {
        if self.at(&Token::LeftParen)? {
            self.next_token()?;
            let mut args = Vec::new();
            while !self.at(&Token::RightParen)? && !self.at_eof()? {
                args.push(self.parse_expr(PREC_COMMA + 1)?);
                if !self.eat(&Token::Comma)? {
                    break;
                }
            }
            let end = self.peek_span();
            self.expect_token(&Token::RightParen)?;
            let span = callee.span.merge(end);
            Ok(Expr::method_call(callee, String::new(), args, span))
        } else {
            Ok(callee)
        }
    }

    /// Parse the key expression inside `{ }` hash subscripts.  Handles bareword autoquoting: `$hash{key}` →
    /// StringLit("key"), `$hash{-key}` → StringLit("-key"), `$hash{if}` → StringLit("if"), etc.
    ///
    /// Autoquoting is handled by a single byte-level scan (`try_autoquoted_subscript_key`) that matches the pattern
    /// `[ \t]* [-]? [ \t]* IDENT [ \t]* }` on a SINGLE LINE.  This catches all autoquoting cases uniformly: keywords,
    /// special tokens (`__FILE__`, `__END__`), and plain barewords are all just identifiers at the byte level.
    ///
    /// If the scan doesn't match, everything goes through normal `parse_expr` — quote keywords, data-EOF keywords, and
    /// filetests are all handled by their existing mechanisms in `parse_term`.
    fn parse_hash_subscript_key(&mut self) -> Result<Expr, ParseError> {
        // The one-token cache must be empty so the byte-level scanner sees the raw source bytes immediately after `{`.
        debug_assert!(self.current.is_none(), "parse_hash_subscript_key: one-token cache must be empty");
        if let Some((name, span)) = self.lexer.try_autoquoted_subscript_key() {
            return Ok(Expr::new(ExprKind::StringLit(name), span));
        }
        let key = self.parse_expr(PREC_LOW)?;

        // Multidimensional hash emulation: `$h{1,2,3}` → `$h{join($;, 1, 2, 3)}`.  When the feature is off, the comma-
        // list is left as-is for the compiler to diagnose ("Multidimensional hash lookup is disabled").
        if let ExprKind::Comma(items) = &key.kind
            && self.pragmas.features.contains(Features::MULTIDIMENSIONAL)
        {
            let span = key.span;
            let mut args = vec![Expr::new(ExprKind::SpecialVar(";".to_string()), span)];
            args.extend(items.iter().cloned());
            return Ok(Expr::new(ExprKind::FuncCall("CORE::join".to_string(), args), span));
        }
        Ok(key)
    }

    /// Check for `%hash{keys}` (kv hash slice) or `%array[indices]` (kv array slice) subscripts on a hash-sigil
    /// variable (5.20+).
    fn maybe_kv_slice(&mut self, recv: Expr, span: Span) -> Result<Expr, ParseError> {
        if self.at(&Token::LeftBracket)? {
            self.next_token()?;
            let mut indices = Vec::new();
            while !self.at(&Token::RightBracket)? && !self.at_eof()? {
                indices.push(self.parse_expr(PREC_COMMA + 1)?);
                if !self.eat(&Token::Comma)? {
                    break;
                }
            }
            let end = self.peek_span();
            self.expect_token(&Token::RightBracket)?;
            Ok(Expr::new(ExprKind::KvArraySlice(Box::new(recv), indices), span.merge(end)))
        } else if self.at(&Token::LeftBrace)? {
            self.next_token()?;
            let mut keys = Vec::new();
            while !self.at(&Token::RightBrace)? && !self.at_eof()? {
                keys.push(self.parse_expr(PREC_COMMA + 1)?);
                if !self.eat(&Token::Comma)? {
                    break;
                }
            }
            let end = self.peek_span();
            self.expect_token(&Token::RightBrace)?;
            Ok(Expr::new(ExprKind::KvHashSlice(Box::new(recv), keys), span.merge(end)))
        } else {
            Ok(recv)
        }
    }

    fn maybe_postfix_subscript(&mut self, mut expr: Expr) -> Result<Expr, ParseError> {
        // Handle chained subscripts: $x[0][1], $x{a}{b}, $x[0]{key}
        loop {
            // After a term, we're in operator position.
            if self.at(&Token::LeftBracket)? {
                self.next_token()?;
                let idx = self.parse_expr(PREC_LOW)?;
                let end = self.peek_span();
                self.expect_token(&Token::RightBracket)?;
                let span = expr.span.merge(end);
                expr = Expr::array_elem(expr, idx, span);
            } else if self.at(&Token::LeftBrace)? {
                self.next_token()?;
                let key = self.parse_hash_subscript_key()?;
                let end = self.peek_span();
                self.expect_token(&Token::RightBrace)?;
                let span = expr.span.merge(end);
                expr = Expr::hash_elem(expr, key, span);
            } else {
                break;
            }
        }
        Ok(expr)
    }

    // ── Interpolated string assembly ──────────────────────────
    /// Collect sub-tokens after a `QuoteSublexBegin`/`RegexSublexBegin`/`SubstSublexBegin` into an `Interpolated`.  The
    /// caller decides how to wrap it in an AST node.
    fn parse_interpolated(&mut self) -> Result<Interpolated, ParseError> {
        let mut parts: Vec<InterpPart> = Vec::new();

        loop {
            match self.peek_token().clone() {
                Token::SublexEnd => {
                    self.next_token()?;
                    let merged = merge_interp_parts(parts);
                    return Ok(Interpolated(merged));
                }
                Token::ConstSegment(s) => {
                    self.next_token()?;
                    parts.push(InterpPart::Const(s));
                }
                Token::NamedChar { name, codepoint } => {
                    self.next_token()?;
                    parts.push(InterpPart::NamedChar { name, codepoint });
                }
                Token::InterpScalar(name) => {
                    let span = self.peek_span();
                    self.next_token()?;
                    let cm = self.lexer.take_interp_case_mod();
                    let expr = apply_case_mod_wrap(Expr::new(ExprKind::ScalarVar(name), span), cm);
                    parts.push(InterpPart::ScalarInterp(Box::new(expr)));
                }
                Token::InterpArray(name) => {
                    let span = self.peek_span();
                    self.next_token()?;
                    let cm = self.lexer.take_interp_case_mod();
                    let expr = apply_case_mod_wrap(Expr::new(ExprKind::ArrayVar(name), span), cm);
                    parts.push(InterpPart::ArrayInterp(Box::new(expr)));
                }
                Token::InterpScalarChainStart(name) => {
                    let span = self.peek_span();
                    self.next_token()?;
                    let cm = self.lexer.take_interp_case_mod();
                    let initial = Expr::new(ExprKind::ScalarVar(name), span);
                    let after_subscripts = self.maybe_postfix_subscript(initial)?;
                    let expr = self.parse_expr_continuation(after_subscripts, PREC_LOW)?;
                    self.expect_token(&Token::InterpChainEnd)?;
                    let expr = apply_case_mod_wrap(expr, cm);
                    parts.push(InterpPart::ScalarInterp(Box::new(expr)));
                }
                Token::InterpArrayChainStart(name) => {
                    let span = self.peek_span();
                    self.next_token()?;
                    let cm = self.lexer.take_interp_case_mod();
                    let recv = Expr::new(ExprKind::ArrayVar(name), span);
                    let expr = if self.eat(&Token::LeftBracket)? {
                        let mut indices = Vec::new();
                        while !self.at(&Token::RightBracket)? && !self.at_eof()? {
                            indices.push(self.parse_expr(PREC_COMMA + 1)?);
                            if !self.eat(&Token::Comma)? {
                                break;
                            }
                        }
                        let end = self.peek_span();
                        self.expect_token(&Token::RightBracket)?;
                        Expr::new(ExprKind::ArraySlice(Box::new(recv), indices), span.merge(end))
                    } else if self.eat(&Token::LeftBrace)? {
                        let mut keys = Vec::new();
                        while !self.at(&Token::RightBrace)? && !self.at_eof()? {
                            keys.push(self.parse_expr(PREC_COMMA + 1)?);
                            if !self.eat(&Token::Comma)? {
                                break;
                            }
                        }
                        let end = self.peek_span();
                        self.expect_token(&Token::RightBrace)?;
                        Expr::new(ExprKind::HashSlice(Box::new(recv), keys), span.merge(end))
                    } else {
                        return Err(ParseError::new("expected [ or { after @name in string", self.peek_span()));
                    };
                    self.expect_token(&Token::InterpChainEnd)?;
                    let expr = apply_case_mod_wrap(expr, cm);
                    parts.push(InterpPart::ArrayInterp(Box::new(expr)));
                }
                Token::InterpScalarExprStart | Token::InterpArrayExprStart => {
                    self.next_token()?;
                    let cm = self.lexer.take_interp_case_mod();
                    let expr = self.parse_expr(PREC_LOW)?;
                    self.expect_token(&Token::RightBrace)?;
                    let expr = apply_case_mod_wrap(expr, cm);
                    parts.push(InterpPart::ExprInterp(Box::new(expr)));
                }
                tok @ (Token::RegexCodeStart | Token::RegexCondCodeStart) => {
                    let is_cond = matches!(tok, Token::RegexCondCodeStart);
                    let code_start = self.peek_span().end as usize;
                    self.next_token()?;
                    let expr = self.parse_expr(PREC_LOW)?;
                    let code_end = self.peek_span().start as usize;
                    let raw = String::from_utf8_lossy(self.lexer.slice(code_start, code_end)).into_owned();
                    self.expect_token(&Token::RightBrace)?;
                    if is_cond {
                        parts.push(InterpPart::RegexCondCode(raw, Box::new(expr)));
                    } else {
                        parts.push(InterpPart::RegexCode(raw, Box::new(expr)));
                    }
                }
                Token::Eof => {
                    return Err(ParseError::new("unterminated interpolated string", self.peek_span()));
                }
                other => {
                    return Err(ParseError::new(format!("unexpected token in string: {other:?}"), self.peek_span()));
                }
            }
        }
    }

    /// Parse an interpolated string body into an Expr.  Returns `StringLit` for plain strings, `InterpolatedString` for
    /// strings with interpolation.
    fn parse_interpolated_string(&mut self, span: Span) -> Result<Expr, ParseError> {
        let interp = self.parse_interpolated()?;
        Ok(interp_to_expr(interp, span))
    }

    // ── Operator parsing ──────────────────────────────────────
    fn peek_op_info(&mut self) -> Option<OpInfo> {
        // Snapshot feature bits we may consult before the match on self.peek_token() — that call takes a mutable borrow
        // of self, which we can't hold across further field access.
        let smartmatch_active = self.pragmas.features.contains(Features::SMARTMATCH);

        match self.peek_token() {
            Token::OrOr => Some(OpInfo { prec: PREC_OR, assoc: Assoc::Left }),
            Token::DefinedOr => Some(OpInfo { prec: PREC_OR, assoc: Assoc::Left }),
            Token::LogicalXor => Some(OpInfo { prec: PREC_OR, assoc: Assoc::Left }),
            Token::AndAnd => Some(OpInfo { prec: PREC_AND, assoc: Assoc::Left }),
            Token::BitOr => Some(OpInfo { prec: PREC_BIT_OR, assoc: Assoc::Left }),
            Token::BitXor => Some(OpInfo { prec: PREC_BIT_OR, assoc: Assoc::Left }),
            Token::StringBitOr => Some(OpInfo { prec: PREC_BIT_OR, assoc: Assoc::Left }),
            Token::StringBitXor => Some(OpInfo { prec: PREC_BIT_OR, assoc: Assoc::Left }),
            Token::BitAnd => Some(OpInfo { prec: PREC_BIT_AND, assoc: Assoc::Left }),
            Token::StringBitAnd => Some(OpInfo { prec: PREC_BIT_AND, assoc: Assoc::Left }),
            Token::NumEq | Token::NumNe => Some(OpInfo { prec: PREC_EQ, assoc: Assoc::Chain }),
            Token::Spaceship => Some(OpInfo { prec: PREC_EQ, assoc: Assoc::Non }),
            Token::SmartMatch if smartmatch_active => Some(OpInfo { prec: PREC_EQ, assoc: Assoc::Non }),
            Token::NumLt | Token::NumGt | Token::NumLe | Token::NumGe => Some(OpInfo { prec: PREC_REL, assoc: Assoc::Chain }),
            Token::ShiftLeft | Token::ShiftRight => Some(OpInfo { prec: PREC_SHIFT, assoc: Assoc::Left }),
            Token::Plus => Some(OpInfo { prec: PREC_ADD, assoc: Assoc::Left }),
            Token::Minus => Some(OpInfo { prec: PREC_ADD, assoc: Assoc::Left }),
            Token::Dot => Some(OpInfo { prec: PREC_ADD, assoc: Assoc::Left }),
            Token::Star => Some(OpInfo { prec: PREC_MUL, assoc: Assoc::Left }),
            Token::Slash => Some(OpInfo { prec: PREC_MUL, assoc: Assoc::Left }),
            Token::Percent => Some(OpInfo { prec: PREC_MUL, assoc: Assoc::Left }),
            Token::Binding | Token::NotBinding => Some(OpInfo { prec: PREC_BINDING, assoc: Assoc::Left }),
            Token::Power => Some(OpInfo { prec: PREC_POW, assoc: Assoc::Right }),
            Token::Arrow => Some(OpInfo { prec: PREC_ARROW, assoc: Assoc::Left }),
            Token::DotDot | Token::DotDotDot => Some(OpInfo { prec: PREC_RANGE, assoc: Assoc::Non }),
            Token::Question => Some(OpInfo { prec: PREC_TERNARY, assoc: Assoc::Right }),
            Token::Assign(_) => Some(OpInfo { prec: PREC_ASSIGN, assoc: Assoc::Right }),
            Token::Comma | Token::FatComma => Some(OpInfo { prec: PREC_COMMA, assoc: Assoc::Left }),
            Token::Keyword(Keyword::And) => Some(OpInfo { prec: PREC_AND_LOW, assoc: Assoc::Left }),
            Token::Keyword(Keyword::Or) => Some(OpInfo { prec: PREC_OR_LOW, assoc: Assoc::Left }),
            Token::PlusPlus => Some(OpInfo { prec: PREC_INC, assoc: Assoc::Non }),
            Token::MinusMinus => Some(OpInfo { prec: PREC_INC, assoc: Assoc::Non }),

            // Word operators (always emitted as Keyword tokens): eq/ne/cmp at == precedence, lt/gt/le/ge at relational,
            // x at multiplicative, xor at low-logical.
            Token::Keyword(Keyword::Eq) | Token::Keyword(Keyword::Ne) => Some(OpInfo { prec: PREC_EQ, assoc: Assoc::Chain }),
            Token::Keyword(Keyword::Cmp) => Some(OpInfo { prec: PREC_EQ, assoc: Assoc::Non }),
            Token::Keyword(Keyword::Lt) | Token::Keyword(Keyword::Gt) | Token::Keyword(Keyword::Le) | Token::Keyword(Keyword::Ge) => {
                Some(OpInfo { prec: PREC_REL, assoc: Assoc::Chain })
            }
            Token::Keyword(Keyword::X) => Some(OpInfo { prec: PREC_MUL, assoc: Assoc::Left }),
            Token::Keyword(Keyword::Xor) => Some(OpInfo { prec: PREC_OR_LOW, assoc: Assoc::Left }),
            Token::Keyword(Keyword::Isa) => Some(OpInfo { prec: PREC_ISA, assoc: Assoc::Non }),
            _ => None,
        }
    }

    // ── Semantic validation helpers ──────────────────────────────
    /// Check whether an expression is a valid assignment target (lvalue).
    fn is_valid_lvalue(expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::ScalarVar(_) | ExprKind::ArrayVar(_) | ExprKind::HashVar(_) => true,
            ExprKind::SpecialVar(_) | ExprKind::GlobVar(_) | ExprKind::ArrayLen(_) => true,
            ExprKind::ArrayElem(_, _) | ExprKind::HashElem(_, _) => true,
            ExprKind::ArraySlice(_, _) | ExprKind::HashSlice(_, _) => true,
            ExprKind::Deref(_, _) | ExprKind::ArrowDeref(_, _) => true,
            ExprKind::Local(_) => true,
            ExprKind::Decl(_, _) => true,
            ExprKind::Paren(inner) => Self::is_valid_lvalue(inner),
            ExprKind::Comma(items) => items.iter().all(Self::is_valid_lvalue),
            ExprKind::Undef => true, // (undef, $x) = (1, 2)
            _ => false,
        }
    }

    /// Is `expr` a valid left-hand side of an aliasing assignment, per the `refaliasing` feature?
    ///
    /// Accepts `\$x`, `\@a`, `\%h`, `\&f`, `\*g`, parenthesized forms, and lists of those (including `my`-declarations
    /// that themselves contain ref-wrapped variables).  The base lvalues (ScalarVar, ArrayVar, etc.) are NOT accepted
    /// here — plain `$x = 1` goes through `is_valid_lvalue` alone.
    fn is_ref_alias_target(expr: &Expr) -> bool {
        match &expr.kind {
            // The canonical aliasing form: `\<variable>`.
            ExprKind::Ref(inner) => Self::is_valid_lvalue(inner),

            // Parenthesized aliasing: `(\$x) = ...`.
            ExprKind::Paren(inner) => Self::is_ref_alias_target(inner),

            // Comma: `(\$x, \@y) = ...`.  Mixed lists (some ref, some not) are syntactically valid — semantics is a
            // later concern.
            ExprKind::Comma(items) => items.iter().any(Self::is_ref_alias_target),

            // `my \$x = ...` — the Decl already carries the `is_ref` flag on each VarDecl.
            ExprKind::Decl(_, vars) => vars.iter().any(|v| v.is_ref),
            _ => false,
        }
    }

    fn parse_operator(&mut self, left: Expr, info: OpInfo) -> Result<Expr, ParseError> {
        let op_spanned = self.next_token()?;
        let right_prec = info.right_prec();

        match op_spanned.token {
            // Postfix increment/decrement
            Token::PlusPlus => {
                if !Self::is_valid_lvalue(&left) {
                    return Err(ParseError::new("invalid operand for postfix ++", left.span));
                }
                let span = left.span.merge(op_spanned.span);
                Ok(Expr::postfix(PostfixOp::Inc, left, span))
            }
            Token::MinusMinus => {
                if !Self::is_valid_lvalue(&left) {
                    return Err(ParseError::new("invalid operand for postfix --", left.span));
                }
                let span = left.span.merge(op_spanned.span);
                Ok(Expr::postfix(PostfixOp::Dec, left, span))
            }

            // Ternary
            Token::Question => {
                let then_expr = self.parse_expr(PREC_LOW)?;
                self.expect_token(&Token::Colon)?;
                let else_expr = self.parse_expr(right_prec)?;
                let span = left.span.merge(else_expr.span);
                Ok(Expr::ternary(left, then_expr, else_expr, span))
            }

            // Arrow
            Token::Arrow => self.parse_arrow_rhs(left),

            // Assignment
            Token::Assign(op) => {
                // `refaliasing` (5.22+) extends lvalue-ness to include `\$x`, `\@a`, `\%h`, and lists of those.  We
                // accept `Ref(...)` as an assignment target only when the feature is active; the ++/-- lvalue checks
                // below are unaffected.
                let refalias_ok = self.pragmas.features.contains(Features::REFALIASING) && Self::is_ref_alias_target(&left);
                if !Self::is_valid_lvalue(&left) && !refalias_ok {
                    return Err(ParseError::new("invalid assignment target", left.span));
                }
                let right = self.parse_expr(right_prec)?;
                let span = left.span.merge(right.span);
                Ok(Expr::assign(op, left, right, span))
            }

            // Comma / fat comma — build a list
            Token::Comma | Token::FatComma => {
                if self.at(&Token::Semi)? || self.at(&Token::RightParen)? || self.at(&Token::RightBracket)? || self.at(&Token::RightBrace)? || self.at_eof()? {
                    // Trailing comma
                    return Ok(left);
                }
                let right = self.parse_expr(right_prec)?;

                // Flatten comma lists
                let mut items = match left.kind {
                    ExprKind::Comma(items) => items,
                    _ => vec![left],
                };
                match right.kind {
                    ExprKind::Comma(more) => items.extend(more),
                    _ => items.push(right),
                };
                let span = match (items.first(), items.last()) {
                    (Some(f), Some(l)) => f.span.merge(l.span),
                    _ => Span::new(0, 0),
                };
                Ok(Expr::new(ExprKind::Comma(items), span))
            }

            // Range — non-associative, reject chaining.
            Token::DotDot => {
                let right = self.parse_expr(right_prec)?;
                self.reject_non_assoc_chaining(info, &right)?;
                let span = left.span.merge(right.span);
                Ok(Expr::range(left, right, span))
            }
            Token::DotDotDot => {
                let right = self.parse_expr(right_prec)?;
                self.reject_non_assoc_chaining(info, &right)?;
                let span = left.span.merge(right.span);
                Ok(Expr::flipflop(left, right, span))
            }

            // Binary operators
            token => {
                let binop = token_to_binop(&token)?;
                let right = self.parse_expr(right_prec)?;

                match info.assoc {
                    Assoc::Chain => {
                        // Check if more operators follow at the same precedence.
                        if let Some(next_info) = self.peek_op_info()
                            && next_info.prec == info.prec
                        {
                            if next_info.assoc == Assoc::Chain {
                                // More chainable operators — build ChainedCmp.
                                let mut ops = vec![binop];
                                let start_span = left.span;
                                let mut operands = vec![left, right];
                                while let Some(next_info) = self.peek_op_info()
                                    && next_info.prec == info.prec
                                    && next_info.assoc == Assoc::Chain
                                {
                                    let next_tok = self.next_token()?;
                                    ops.push(token_to_binop(&next_tok.token)?);
                                    operands.push(self.parse_expr(right_prec)?);
                                }

                                // After the chain, reject a trailing Non at the same level
                                // (e.g. `$a == $b != $c <=> $d`).
                                if let Some(trail) = self.peek_op_info()
                                    && trail.prec == info.prec
                                {
                                    return Err(ParseError::new("non-associative operator cannot be chained", operands.last().map_or(start_span, |e| e.span)));
                                }
                                let end_span = operands.last().map_or(start_span, |e| e.span);
                                return Ok(Expr::new(ExprKind::ChainedCmp(ops, operands), start_span.merge(end_span)));
                            } else {
                                // Non-chainable operator at same precedence (e.g. `$a == $b <=> $c`).
                                return Err(ParseError::new("non-associative operator cannot be chained", right.span));
                            }
                        }
                        let span = left.span.merge(right.span);
                        Ok(Expr::binop(binop, left, right, span))
                    }
                    Assoc::Non => {
                        self.reject_non_assoc_chaining(info, &right)?;
                        let span = left.span.merge(right.span);
                        Ok(Expr::binop(binop, left, right, span))
                    }
                    _ => {
                        let span = left.span.merge(right.span);
                        Ok(Expr::binop(binop, left, right, span))
                    }
                }
            }
        }
    }

    /// After parsing the RHS of a non-associative operator, check if another operator at the same precedence level
    /// follows.  If so, it's a chaining error like `$x .. $y .. $z` or `$x <=> $y <=> $z`.
    fn reject_non_assoc_chaining(&mut self, info: OpInfo, right: &Expr) -> Result<(), ParseError> {
        if let Some(next_info) = self.peek_op_info()
            && next_info.prec == info.prec
        {
            return Err(ParseError::new("non-associative operator cannot be chained", right.span));
        }
        Ok(())
    }

    fn parse_arrow_rhs(&mut self, left: Expr) -> Result<Expr, ParseError> {
        // After ->, identifiers (including what would otherwise be keywords like 'keys', 'values', 'print') are method
        // names.  Convert Keyword tokens to their name string.
        let method_name: Option<String> = match self.peek_token() {
            Token::Ident(name) => Some(name.clone()),
            Token::Keyword(kw) => Some((<&str>::from(*kw)).to_string()),
            _ => None,
        };
        if let Some(name) = method_name {
            self.next_token()?;

            // Method call: ->method(...)
            if self.at(&Token::LeftParen)? {
                self.next_token()?;
                let mut args = Vec::new();
                while !self.at(&Token::RightParen)? && !self.at_eof()? {
                    args.push(self.parse_expr(PREC_COMMA + 1)?);
                    if !self.eat(&Token::Comma)? {
                        break;
                    }
                }
                let end = self.peek_span();
                self.expect_token(&Token::RightParen)?;
                let span = left.span.merge(end);
                return Ok(Expr::method_call(left, name, args, span));
            } else {
                // Bare method call with no parens
                let span = left.span.merge(self.peek_span());
                return Ok(Expr::method_call(left, name, vec![], span));
            }
        }
        match self.peek_token().clone() {
            Token::LeftBracket => {
                self.next_token()?;
                let idx = self.parse_expr(PREC_LOW)?;
                let end = self.peek_span();
                self.expect_token(&Token::RightBracket)?;
                let span = left.span.merge(end);
                let expr = Expr::arrow_deref(left, ArrowTarget::array_elem(idx), span);

                // Handle chained subscripts: $ref->[0][1], $ref->[0]{key}
                self.maybe_postfix_subscript(expr)
            }
            Token::LeftBrace => {
                self.next_token()?;
                let key = self.parse_hash_subscript_key()?;
                let end = self.peek_span();
                self.expect_token(&Token::RightBrace)?;
                let span = left.span.merge(end);
                let expr = Expr::arrow_deref(left, ArrowTarget::hash_elem(key), span);

                // Handle chained subscripts: $ref->{a}{b}, $ref->{a}[0]
                self.maybe_postfix_subscript(expr)
            }
            Token::LeftParen => {
                // ->(...) — coderef call
                self.next_token()?;
                let mut args = Vec::new();
                while !self.at(&Token::RightParen)? && !self.at_eof()? {
                    args.push(self.parse_expr(PREC_COMMA + 1)?);
                    if !self.eat(&Token::Comma)? {
                        break;
                    }
                }
                let end = self.peek_span();
                self.expect_token(&Token::RightParen)?;
                let span = left.span.merge(end);
                Ok(Expr::method_call(left, String::new(), args, span))
            }

            // Dynamic method dispatch: ->$method or ->$method(args)
            Token::ScalarVar(var_name) => {
                let var_span = self.peek_span();
                self.next_token()?;
                let method_expr = Expr::new(ExprKind::ScalarVar(var_name), var_span);
                if self.at(&Token::LeftParen)? {
                    self.next_token()?;
                    let mut args = Vec::new();
                    while !self.at(&Token::RightParen)? && !self.at_eof()? {
                        args.push(self.parse_expr(PREC_COMMA + 1)?);
                        if !self.eat(&Token::Comma)? {
                            break;
                        }
                    }
                    let end = self.peek_span();
                    self.expect_token(&Token::RightParen)?;
                    let span = left.span.merge(end);
                    Ok(Expr::arrow_deref(left, ArrowTarget::dyn_method(method_expr, args), span))
                } else {
                    let span = left.span.merge(var_span);
                    Ok(Expr::arrow_deref(left, ArrowTarget::dyn_method(method_expr, vec![]), span))
                }
            }

            // Postfix dereference: ->@*, ->%*, ->$*, ->&*, ->**, plus slice forms ->@[...], ->@{...}, ->%[...],
            // ->%{...}.
            //
            // The trailing `*` forms are whole-container derefs.  The `[...]` and `{...}` forms after `@` or `%`
            // produce slices (array of values or kv list).
            Token::At => {
                self.next_token()?;
                if self.eat(&Token::Star)? {
                    let span = left.span.merge(self.peek_span());
                    Ok(Expr::arrow_deref(left, ArrowTarget::DerefArray, span))
                } else if self.at(&Token::LeftBracket)? {
                    self.next_token()?;
                    let idx = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBracket)?;
                    let span = left.span.merge(end);
                    Ok(Expr::arrow_deref(left, ArrowTarget::array_slice_indices(idx), span))
                } else if self.at(&Token::LeftBrace)? {
                    self.next_token()?;
                    let key = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBrace)?;
                    let span = left.span.merge(end);
                    Ok(Expr::arrow_deref(left, ArrowTarget::array_slice_keys(key), span))
                } else {
                    Err(ParseError::new("expected *, [indices], or {keys} after ->@", self.peek_span()))
                }
            }
            Token::Dollar => {
                self.next_token()?;

                // `->$#*` — postderef last-index.  The lexer would otherwise tokenize the `#` as a comment start, so we
                // peek+consume the two raw bytes here before the next token is lexed.
                if self.lexer.try_consume_hash_star() {
                    let span = left.span.merge(self.peek_span());
                    Ok(Expr::arrow_deref(left, ArrowTarget::LastIndex, span))
                } else if self.eat(&Token::Star)? {
                    let span = left.span.merge(self.peek_span());
                    Ok(Expr::arrow_deref(left, ArrowTarget::DerefScalar, span))
                } else {
                    Err(ParseError::new("expected * or #* after ->$", self.peek_span()))
                }
            }
            Token::Percent => {
                self.next_token()?;
                if self.eat(&Token::Star)? {
                    let span = left.span.merge(self.peek_span());
                    Ok(Expr::arrow_deref(left, ArrowTarget::DerefHash, span))
                } else if self.at(&Token::LeftBracket)? {
                    self.next_token()?;
                    let idx = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBracket)?;
                    let span = left.span.merge(end);
                    Ok(Expr::arrow_deref(left, ArrowTarget::kv_slice_indices(idx), span))
                } else if self.at(&Token::LeftBrace)? {
                    self.next_token()?;
                    let key = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBrace)?;
                    let span = left.span.merge(end);
                    Ok(Expr::arrow_deref(left, ArrowTarget::kv_slice_keys(key), span))
                } else {
                    Err(ParseError::new("expected *, [indices], or {keys} after ->%", self.peek_span()))
                }
            }

            // `->&*` — code-ref postfix deref.  `->&method` or `->&method(args)` — lexical method invocation (resolved
            // at compile time, not via package inheritance).  The `&` prefix in the name signals lexical resolution to
            // the compiler.
            Token::BitAnd => {
                self.next_token()?;
                if self.eat(&Token::Star)? {
                    let span = left.span.merge(self.peek_span());
                    Ok(Expr::arrow_deref(left, ArrowTarget::DerefCode, span))
                } else {
                    // Lexical method: ->&name or ->&name(args)
                    let method_name = match self.peek_token().clone() {
                        Token::Ident(name) => name,
                        Token::Keyword(kw) => <&str>::from(kw).to_string(),
                        other => return Err(ParseError::new(format!("expected * or method name after ->&, got {other:?}"), self.peek_span())),
                    };
                    self.next_token()?;
                    let name = format!("&{method_name}");
                    if self.at(&Token::LeftParen)? {
                        self.next_token()?;
                        let mut args = Vec::new();
                        while !self.at(&Token::RightParen)? && !self.at_eof()? {
                            args.push(self.parse_expr(PREC_COMMA + 1)?);
                            if !self.eat(&Token::Comma)? {
                                break;
                            }
                        }
                        let end = self.peek_span();
                        self.expect_token(&Token::RightParen)?;
                        let span = left.span.merge(end);
                        Ok(Expr::method_call(left, name, args, span))
                    } else {
                        let span = left.span.merge(self.peek_span());
                        Ok(Expr::method_call(left, name, vec![], span))
                    }
                }
            }

            // `->**` — glob deref.  Two consecutive `*`s; the lexer emits `Power` (`**`) for that pair.
            Token::Power => {
                self.next_token()?;
                let span = left.span.merge(self.peek_span());
                Ok(Expr::arrow_deref(left, ArrowTarget::DerefGlob, span))
            }
            other => Err(ParseError::new(format!("expected method name or subscript after ->, got {other:?}"), self.peek_span())),
        }
    }
}

// ── Pragma application helpers ────────────────────────────────
/// Apply the side effects of a `use` or `no` statement whose module name is a known pragma.  Unknown modules are
/// ignored.
fn apply_pragma(pragmas: &mut Pragmas, module: &str, is_no: bool, imports: Option<&Vec<Expr>>) {
    match module {
        "feature" => {
            // Arguments are feature or bundle names as string literals, barewords, or a qw(...) list.  Bundle aliases
            // (`:all`, `:default`, `:5.36`) are handled by resolve_feature_name.
            match imports {
                Some(items) if !items.is_empty() => {
                    for item in items {
                        for name in expr_to_pragma_strings(item) {
                            if let Some(feats) = resolve_feature_name(&name) {
                                // A bundle name evaluates to a set of features; individual names evaluate to single-bit
                                // sets.  Either way, OR/AND-NOT works.
                                if is_no {
                                    pragmas.features.remove(feats);
                                } else {
                                    // `use feature ':5.36'` resets to the bundle rather than ORing with prior state.
                                    // We detect that by checking for a leading colon in the name (same test the
                                    // resolver uses).
                                    if name.starts_with(':') {
                                        pragmas.features = feats;
                                    } else {
                                        pragmas.features.insert(feats);
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {
                    // Per perlfeature: "no feature" with no args resets to :default; "use feature" with no args is a
                    // no-op.
                    if is_no {
                        pragmas.features = Features::DEFAULT;
                    }
                }
            }
        }
        "utf8" => {
            pragmas.utf8 = !is_no;
        }
        _ => {
            // Any other pragma (strict, warnings, integer, ...) or module import is not yet parser-relevant.
        }
    }
}

/// Extract one or more pragma argument names from an AST expression.  `use feature 'say'` yields a StringLit (one
/// name); `use feature qw(say state)` yields a QwList (multiple names); barewords are historically allowed too.
/// Returns an empty vec for anything else.
fn expr_to_pragma_strings(expr: &Expr) -> Vec<String> {
    match &expr.kind {
        ExprKind::StringLit(s) => vec![s.clone()],
        ExprKind::Bareword(s) => vec![s.clone()],
        ExprKind::QwList(words) => words.clone(),
        _ => Vec::new(),
    }
}

/// Parse a `use 5036` / `use 5.036` integer as a (major, minor) pair.  The integer form is interpreted as `5_036` =
/// major 5, minor 36 (last three digits = minor).
fn parse_int_version(n: i64) -> Option<(u32, u32)> {
    if n <= 0 {
        return None;
    }
    let n = n as u64;

    // Historically, `use 5008001;` → 5.008.001 (major.minor.patch).  For phase 1 we handle the common pattern `5NNN`
    // where NNN is the minor version, which matches `use 5036`.
    if n >= 1000 {
        let major = (n / 1000) as u32;
        let minor = (n % 1000) as u32;
        Some((major, minor))
    } else {
        // Plain major number.
        Some((n as u32, 0))
    }
}

/// Parse a `use 5.036` float as a (major, minor) pair.
fn parse_float_version(n: f64) -> Option<(u32, u32)> {
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    let major = n.trunc() as u32;

    // Extract three decimal digits as the minor.  `5.036` → 36.
    let frac = n - n.trunc();
    let minor = (frac * 1000.0).round() as u32;
    Some((major, minor))
}

/// Parse a v-string literal like `"v5.36"` or `"v5.36.0"` as a (major, minor) pair.  Only the first two components are
/// used.
fn parse_v_string_version(s: &str) -> Option<(u32, u32)> {
    let rest = s.strip_prefix('v')?;
    let mut it = rest.split('.');
    let major: u32 = it.next()?.parse().ok()?;
    let minor: u32 = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

fn token_to_binop(token: &Token) -> Result<BinOp, ParseError> {
    match token {
        Token::Plus => Ok(BinOp::Add),
        Token::Minus => Ok(BinOp::Sub),
        Token::Star => Ok(BinOp::Mul),
        Token::Slash => Ok(BinOp::Div),
        Token::Percent => Ok(BinOp::Mod),
        Token::Power => Ok(BinOp::Pow),
        Token::Dot => Ok(BinOp::Concat),
        Token::NumEq => Ok(BinOp::NumEq),
        Token::NumNe => Ok(BinOp::NumNe),
        Token::NumLt => Ok(BinOp::NumLt),
        Token::NumGt => Ok(BinOp::NumGt),
        Token::NumLe => Ok(BinOp::NumLe),
        Token::NumGe => Ok(BinOp::NumGe),
        Token::Spaceship => Ok(BinOp::Spaceship),
        Token::SmartMatch => Ok(BinOp::SmartMatch),
        Token::AndAnd => Ok(BinOp::And),
        Token::OrOr => Ok(BinOp::Or),
        Token::DefinedOr => Ok(BinOp::DefinedOr),
        Token::LogicalXor => Ok(BinOp::LogicalXor),
        Token::BitAnd => Ok(BinOp::BitAnd),
        Token::BitOr => Ok(BinOp::BitOr),
        Token::BitXor => Ok(BinOp::BitXor),
        Token::StringBitAnd => Ok(BinOp::StringBitAnd),
        Token::StringBitOr => Ok(BinOp::StringBitOr),
        Token::StringBitXor => Ok(BinOp::StringBitXor),
        Token::ShiftLeft => Ok(BinOp::ShiftLeft),
        Token::ShiftRight => Ok(BinOp::ShiftRight),
        Token::Binding => Ok(BinOp::Binding),
        Token::NotBinding => Ok(BinOp::NotBinding),
        Token::Keyword(Keyword::And) => Ok(BinOp::LowAnd),
        Token::Keyword(Keyword::Or) => Ok(BinOp::LowOr),
        Token::Keyword(Keyword::X) => Ok(BinOp::Repeat),
        Token::Keyword(Keyword::Xor) => Ok(BinOp::LowXor),
        Token::Keyword(Keyword::Isa) => Ok(BinOp::Isa),
        Token::Keyword(Keyword::Eq) => Ok(BinOp::StrEq),
        Token::Keyword(Keyword::Ne) => Ok(BinOp::StrNe),
        Token::Keyword(Keyword::Lt) => Ok(BinOp::StrLt),
        Token::Keyword(Keyword::Gt) => Ok(BinOp::StrGt),
        Token::Keyword(Keyword::Le) => Ok(BinOp::StrLe),
        Token::Keyword(Keyword::Ge) => Ok(BinOp::StrGe),
        Token::Keyword(Keyword::Cmp) => Ok(BinOp::StrCmp),
        other => Err(ParseError::new(format!("not a binary operator: {other:?}"), Span::DUMMY)),
    }
}

/// Wrap an interpolated expression in case-modification function calls based on the active `CaseMod` flags.
///
/// `\U$x` → `uc($x)`, `\l$x` → `lcfirst($x)`,
/// `\Q\U$x\E\E` → `quotemeta(uc($x))`.
fn apply_case_mod_wrap(mut expr: Expr, flags: CaseMod) -> Expr {
    if flags.is_empty() {
        return expr;
    }
    let span = expr.span;

    // One-shot overrides persistent case mode.
    if flags.contains(CaseMod::LCFIRST) {
        expr = Expr::new(ExprKind::FuncCall("CORE::lcfirst".into(), vec![expr]), span);
    } else if flags.contains(CaseMod::UCFIRST) {
        expr = Expr::new(ExprKind::FuncCall("CORE::ucfirst".into(), vec![expr]), span);
    } else if flags.contains(CaseMod::UPPER) {
        expr = Expr::new(ExprKind::FuncCall("CORE::uc".into(), vec![expr]), span);
    } else if flags.contains(CaseMod::LOWER) || flags.contains(CaseMod::FOLD) {
        expr = Expr::new(ExprKind::FuncCall("CORE::lc".into(), vec![expr]), span);
    }

    // Quotemeta wraps outermost (applied last, after case).
    if flags.contains(CaseMod::QUOTEMETA) {
        expr = Expr::new(ExprKind::FuncCall("CORE::quotemeta".into(), vec![expr]), span);
    }

    expr
}

/// Merge adjacent `Const` segments in an interpolated value.
fn merge_interp_parts(parts: Vec<InterpPart>) -> Vec<InterpPart> {
    let mut merged: Vec<InterpPart> = Vec::new();
    for part in parts {
        // Skip empty constant segments (produced when a case-mod escape like `\l` immediately precedes an interpolation
        // with no intervening literal characters).
        if let InterpPart::Const(s) = &part {
            if s.is_empty() {
                continue;
            }
            if let Some(InterpPart::Const(prev)) = merged.last_mut() {
                prev.push_str(s);
                continue;
            }
        }
        merged.push(part);
    }
    merged
}

/// Convert an `Interpolated` into an `Expr`.  Returns `StringLit` for plain strings, `InterpolatedString` otherwise.
fn interp_to_expr(interp: Interpolated, span: Span) -> Expr {
    if let Some(s) = interp.as_plain_string() { Expr::new(ExprKind::StringLit(s), span) } else { Expr::new(ExprKind::InterpolatedString(interp), span) }
}
