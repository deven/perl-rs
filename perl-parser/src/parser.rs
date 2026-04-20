//! Pratt parser with recursive descent for statements (§6).
//!
//! Expression assembly uses precedence climbing.  Statements, declarations,
//! blocks, and top-level forms use ordinary recursive descent that calls
//! `parse_expr` where expressions are needed.

use crate::ast::*;
use crate::error::ParseError;
use crate::keyword;
use crate::lexer::Lexer;
use crate::pragma::{Features, Pragmas, resolve_feature_name};
use crate::span::Span;
use crate::symbol::{ProtoSlot, SubPrototype, SymbolTable};
use crate::token::Keyword;
use crate::token::*;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/parser_tests.rs"]
mod tests;

/// Precedence levels (u8 with gaps for plugin insertion).
/// Maps to Perl 5's precedence table from perly.y.
pub type Precedence = u8;

/// Parse depth counter for nesting limit.
pub type ParseDepth = u16;

const MAX_DEPTH: ParseDepth = 10_000;

// ── Precedence constants ──────────────────────────────────────
// Gaps of 2 to allow plugin operators at intermediate levels.

const PREC_LOW: Precedence = 0; // statement boundary
const PREC_OR_LOW: Precedence = 2; // or
const PREC_AND_LOW: Precedence = 4; // and
const PREC_NOT_LOW: Precedence = 6; // not (prefix)
#[allow(dead_code)]
const PREC_LIST: Precedence = 8; // list operators
const PREC_COMMA: Precedence = 10; // , =>
const PREC_ASSIGN: Precedence = 12; // = += -= etc.
const PREC_TERNARY: Precedence = 14; // ?:
const PREC_RANGE: Precedence = 16; // .. ...
const PREC_OR: Precedence = 18; // || //
const PREC_AND: Precedence = 20; // &&
const PREC_BIT_OR: Precedence = 22; // |
const PREC_BIT_AND: Precedence = 24; // &
const PREC_EQ: Precedence = 26; // == != eq ne <=> cmp
const PREC_REL: Precedence = 28; // < > <= >= lt gt le ge
/// `isa` — class-instance infix operator (5.32+).  Non-associative.
/// Tighter than relational, looser than named unary: `$x isa Foo < 1`
/// parses as `($x isa Foo) < 1`, while `foo $x isa Bar` parses as
/// `foo($x isa Bar)`.
const PREC_ISA: Precedence = 29;
/// Named unary operators and prototyped subs with a scalar-ish
/// slot (`$`, `_`, `+`, `\X`, `\[...]`, etc.).  Sits between
/// isa and shift: `foo $a < 1` parses as `foo($a) < 1`,
/// while `foo $a << 1` parses as `foo($a << 1)`.  Non-associative.
const PREC_NAMED_UNARY: Precedence = 30;
const PREC_SHIFT: Precedence = 32; // << >>
const PREC_ADD: Precedence = 34; // + - .
const PREC_MUL: Precedence = 36; // * / % x
const PREC_BINDING: Precedence = 38; // =~ !~
const PREC_UNARY: Precedence = 40; // ! ~ \ - + (prefix)
const PREC_POW: Precedence = 42; // **
const PREC_INC: Precedence = 44; // ++ -- (postfix)
const PREC_ARROW: Precedence = 46; // ->

#[derive(Clone, Copy, Debug, PartialEq)]
enum Assoc {
    Left,
    Right,
    Non,
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
            Assoc::Left | Assoc::Non => self.prec + 1,
            Assoc::Right => self.prec,
        }
    }
}

/// The combined parser/lexer.
pub struct Parser {
    lexer: Lexer,
    /// Cached current token.  `None` means no token is cached —
    /// the next peek/next will lex one.
    current: Option<Spanned>,
    /// Stored lexer error — surfaced by next_token().
    lexer_error: Option<ParseError>,
    depth: ParseDepth,
    /// Symbol table of all packages, subs, and imports seen so far.
    /// Populated as sub declarations and (eventually) `use`
    /// statements are parsed; consulted at call sites for
    /// prototype-aware argument parsing.
    symbols: SymbolTable,
    /// Name of the package currently being parsed.  Updated by
    /// `package Name;` and the block form `package Name { ... }`.
    current_package: std::sync::Arc<str>,
    /// Lexically-scoped pragma state (`use feature`, `use utf8`,
    /// version bundles).  Saved/restored across block boundaries
    /// by `parse_block`.
    pragmas: Pragmas,
}

impl Parser {
    // ── Construction ──────────────────────────────────────────

    pub fn new(src: &[u8]) -> Result<Self, ParseError> {
        Self::from_lexer(Lexer::new(src))
    }

    /// Construct a parser that reports `filename` for `__FILE__`
    /// resolution and in diagnostic messages.  Prefer this over
    /// [`Self::new`] when the source comes from a named file.
    pub fn with_filename(src: &[u8], filename: impl Into<String>) -> Result<Self, ParseError> {
        Self::from_lexer(Lexer::with_filename(src, filename))
    }

    /// Shared core: all constructors funnel through here so
    /// field initialization stays in one place.
    fn from_lexer(lexer: Lexer) -> Result<Self, ParseError> {
        Ok(Parser {
            lexer,
            current: None,
            lexer_error: None,
            depth: 0,
            symbols: SymbolTable::new(),
            current_package: std::sync::Arc::from("main"),
            pragmas: Pragmas::new(),
        })
    }

    /// Read-only access to the accumulated symbol table.
    /// Primarily for tests and future cross-pass consumers.
    pub fn symbols(&self) -> &SymbolTable {
        &self.symbols
    }

    /// Read-only access to the current lexical pragma state.
    /// Primarily for tests and future parsing-behavior dispatch
    /// (signatures vs. prototypes, postderef enablement, etc.).
    pub fn pragmas(&self) -> &Pragmas {
        &self.pragmas
    }

    // ── Token access ──────────────────────────────────────────

    /// Peek at the current token without consuming it.
    /// Lexes on demand if no token is cached.
    fn peek_token(&mut self) -> &Token {
        if self.current.is_none() {
            self.current = Some(match self.lexer.lex_token() {
                Ok(s) => s,
                Err(e) => {
                    self.lexer_error = Some(e);
                    Spanned { token: Token::Eof, span: Span::new(self.lexer.pos() as u32, self.lexer.pos() as u32) }
                }
            });
            // Downgrade feature-gated keywords to plain idents
            // when the governing feature is not active in the
            // current lexical scope.  This lets user code that
            // predates the feature (or explicitly `no feature`s
            // it) keep using these words as function names,
            // method names, hash keys, etc.  Same in spirit as
            // the parser resolving `/` to regex-vs-division:
            // the lexer emits a context-free classification and
            // the parser refines it with scope-level information.
            self.maybe_demote_keyword();
        }
        // self.current is Some by construction above.
        match &self.current {
            Some(s) => &s.token,
            None => unreachable!("peek_token: current is Some"),
        }
    }

    /// Rewrite a feature-gated `Keyword` in the lookahead cache
    /// as a plain `Ident(name)` when the governing feature is
    /// off.  No-op if the token isn't such a keyword or if the
    /// feature is active.
    fn maybe_demote_keyword(&mut self) {
        let Some(sp) = &self.current else {
            return;
        };
        let kw = match sp.token {
            Token::Keyword(kw) => kw,
            _ => return,
        };
        // Map keyword → governing feature.  Only feature-gated
        // keywords appear here; unconditional keywords (my, sub,
        // if, etc.) are omitted and always stay as Keyword.
        let needed = match kw {
            Keyword::Try | Keyword::Catch | Keyword::Finally => Features::TRY,
            Keyword::Defer => Features::DEFER,
            Keyword::Given | Keyword::When | Keyword::Default => Features::SWITCH,
            Keyword::Class | Keyword::Field | Keyword::Method | Keyword::ADJUST => Features::CLASS,
            Keyword::Any => Features::KEYWORD_ANY,
            Keyword::All => Features::KEYWORD_ALL,
            _ => return,
        };
        if self.pragmas.features.contains(needed) {
            return;
        }
        let name: &str = kw.into();
        let span = sp.span;
        self.current = Some(Spanned { token: Token::Ident(name.to_string()), span });
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

    // ── Depth control ─────────────────────────────────────────

    /// Increment the recursion-depth counter, invoke `f`, then
    /// decrement unconditionally — even if `f` returns an error.
    /// This is the closure-based alternative to RAII descent guards
    /// (which would conflict with `&mut self` re-borrows inside `f`).
    ///
    /// If entering would exceed `MAX_DEPTH`, returns an error without
    /// calling `f`.
    fn with_descent<T, F>(&mut self, f: F) -> Result<T, ParseError>
    where
        F: FnOnce(&mut Self) -> Result<T, ParseError>,
    {
        if self.depth + 1 >= MAX_DEPTH {
            return Err(ParseError::new("nesting too deep", self.peek_span()));
        }
        self.depth += 1;
        let result = f(self);
        self.depth -= 1;
        result
    }

    // ── Flag validation ───────────────────────────────────────

    /// Validate regex modifier flags.  Returns an error for any
    /// unrecognized modifier character.
    fn validate_regex_flags(flags: &str, span: Span) -> Result<(), ParseError> {
        for ch in flags.chars() {
            if !"msixpogcadlun".contains(ch) {
                return Err(ParseError::new(format!("Unknown regexp modifier \"/{ch}\""), span));
            }
        }
        Ok(())
    }

    /// Validate substitution modifier flags.  Includes regex flags
    /// plus `e` (eval replacement) and `r` (non-destructive).
    fn validate_subst_flags(flags: &str, span: Span) -> Result<(), ParseError> {
        for ch in flags.chars() {
            if !"msixpogcadluner".contains(ch) {
                return Err(ParseError::new(format!("Unknown regexp modifier \"/{ch}\""), span));
            }
        }
        Ok(())
    }

    /// Validate transliteration modifier flags.  Returns an error for
    /// any unrecognized modifier character.
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
            let is_data_end = matches!(stmt.kind, StmtKind::DataEnd(_, _));
            statements.push(stmt);
            if is_data_end {
                break;
            }
        }

        // A lexer error produces Eof, which exits the loop above.
        // If advance() was never called to surface it, catch it here.
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

        // __END__ / __DATA__ / ^D / ^Z — stop parsing immediately
        if let Token::DataEnd(marker) = self.peek_token() {
            let marker = *marker;
            self.next_token()?;
            // Compute the byte offset where trailing data begins.
            let data_offset = match marker {
                // ^D / ^Z: data starts immediately after the control char.
                DataEndMarker::CtrlD | DataEndMarker::CtrlZ => self.lexer.current_pos() as u32,
                // __END__ / __DATA__: data starts after the current line.
                DataEndMarker::End | DataEndMarker::Data => {
                    // Skip rest of current line.  Data starts on the next line.
                    let remaining_len = self.lexer.remaining().len();
                    let mut offset = self.lexer.current_pos() + remaining_len;
                    // Account for the line terminator (\n) which is not in remaining().
                    if self.lexer.line_is_terminated() {
                        offset += 1;
                    }
                    offset as u32
                }
            };
            // Skip all remaining source — everything after is not code.
            self.lexer.skip_to_end();
            return Ok(Statement { kind: StmtKind::DataEnd(marker, data_offset), span: start.merge(self.peek_span()), terminated: false });
        }

        let (kind, terminated) = match self.peek_token().clone() {
            // Statement-level keywords: consume first, check for fat comma
            // autoquoting (e.g. `if => 1`), then dispatch to handler.
            Token::Keyword(kw) if keyword::is_statement_keyword(kw) => {
                let kw_span = self.peek_span();
                self.next_token()?; // consume the keyword
                if matches!(self.peek_token(), Token::FatComma) {
                    // Autoquote: keyword is used as a hash key.
                    let expr = self.with_descent(|this| {
                        let name: &str = kw.into();
                        let initial = Expr { kind: ExprKind::StringLit(name.to_string()), span: kw_span };
                        this.parse_expr_continuation(initial, PREC_LOW)
                    })?;
                    let kind = self.maybe_postfix_control(expr)?;
                    let terminated = self.eat(&Token::Semi)?;
                    (kind, terminated)
                } else {
                    match kw {
                        // my/our/state are expressions, not statements.
                        // The keyword has already been consumed; construct
                        // the Decl expression and run the Pratt loop to pick
                        // up optional `= expr` assignment and trailing
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
                                if matches!(self.peek_token(), Token::Ident(_)) {
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
                                let expr = self.with_descent(|this| {
                                    let initial = this.parse_decl_expr(scope, kw_span)?;
                                    this.parse_expr_continuation(initial, PREC_LOW)
                                })?;
                                let kind = self.maybe_postfix_control(expr)?;
                                let terminated = self.eat(&Token::Semi)?;
                                (kind, terminated)
                            }
                        }
                        Keyword::Sub => {
                            if matches!(self.peek_token(), Token::Ident(_)) {
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

                        // given/when/default
                        Keyword::Given => (self.parse_given()?, false),
                        Keyword::When => (self.parse_when()?, false),
                        Keyword::Default => {
                            let block = self.parse_block()?;
                            (StmtKind::When(Expr { kind: ExprKind::IntLit(1), span: kw_span }, block), false)
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
            }

            // Expression keywords (local, return, etc.) and non-keywords
            // go through parse_expr_statement.  parse_term handles fat
            // comma autoquoting for these.
            Token::Keyword(Keyword::Local) => self.parse_expr_statement()?,

            // `{` at statement level — parse as block, then check if it
            // should be reclassified as a hash constructor.
            Token::LeftBrace => {
                let block = self.parse_block()?;
                match Self::try_reclassify_as_hash(block) {
                    Ok(hash_expr) => {
                        // Reclassified as hash constructor.  Continue as
                        // an expression statement: check for postfix
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
                    let expr = self.with_descent(|this| {
                        let initial = this.parse_ident_term(name, ident_span)?;
                        this.parse_expr_continuation(initial, PREC_LOW)
                    })?;
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
                Ok(StmtKind::Expr(Expr { span: expr.span.merge(cond.span), kind: ExprKind::PostfixControl(PostfixKind::If, Box::new(expr), Box::new(cond)) }))
            }
            Token::Keyword(Keyword::Unless) => {
                self.next_token()?;
                let cond = self.parse_expr(PREC_LOW)?;
                Ok(StmtKind::Expr(Expr {
                    span: expr.span.merge(cond.span),
                    kind: ExprKind::PostfixControl(PostfixKind::Unless, Box::new(expr), Box::new(cond)),
                }))
            }
            Token::Keyword(Keyword::While) => {
                self.next_token()?;
                let cond = self.parse_expr(PREC_LOW)?;
                Ok(StmtKind::Expr(Expr {
                    span: expr.span.merge(cond.span),
                    kind: ExprKind::PostfixControl(PostfixKind::While, Box::new(expr), Box::new(cond)),
                }))
            }
            Token::Keyword(Keyword::Until) => {
                self.next_token()?;
                let cond = self.parse_expr(PREC_LOW)?;
                Ok(StmtKind::Expr(Expr {
                    span: expr.span.merge(cond.span),
                    kind: ExprKind::PostfixControl(PostfixKind::Until, Box::new(expr), Box::new(cond)),
                }))
            }
            Token::Keyword(Keyword::For) | Token::Keyword(Keyword::Foreach) => {
                self.next_token()?;
                let list = self.parse_expr(PREC_LOW)?;
                Ok(StmtKind::Expr(Expr { span: expr.span.merge(list.span), kind: ExprKind::PostfixControl(PostfixKind::For, Box::new(expr), Box::new(list)) }))
            }
            Token::Keyword(Keyword::When) => {
                self.next_token()?;
                let cond = self.parse_expr(PREC_LOW)?;
                Ok(StmtKind::Expr(Expr { span: expr.span.merge(cond.span), kind: ExprKind::PostfixControl(PostfixKind::When, Box::new(expr), Box::new(cond)) }))
            }
            _ => Ok(StmtKind::Expr(expr)),
        }
    }

    // ── Variable declarations ─────────────────────────────────

    fn parse_single_var_decl(&mut self) -> Result<VarDecl, ParseError> {
        let span = self.peek_span();

        // `my \$x` / `my \@a` / `my \%h` — reference declaration
        // (declared_refs, 5.26+).  Only honored when the feature
        // is active; otherwise `\` would be an unexpected token
        // here.
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

    /// Parse the body of a named sub declaration after `sub` has
    /// already been consumed.  `start` is the span of the `sub` keyword.
    ///
    /// Registers the sub (with its prototype, if any) in the symbol
    /// table before returning, so subsequent call sites can consult
    /// it for prototype-driven argument parsing.
    ///
    /// Prototypes may be declared in two syntactic forms:
    /// * Paren-form after the name: `sub foo ($$) { ... }`.
    /// * Attribute form: `sub foo :prototype($$) { ... }` (Perl 5.20+).
    ///
    /// Both are supported; the attribute form takes precedence if
    /// both appear (matching the behavior needed once signatures are
    /// enabled, where the paren form is a signature instead).
    fn parse_sub_decl_body(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        let name = match self.next_token()?.token {
            Token::Ident(name) => name,
            other => return Err(ParseError::new(format!("expected sub name, got {other:?}"), start)),
        };

        // Dispatch on the `signatures` feature: when active, the
        // grammar is `sub NAME [ATTRS] [SIGNATURE] BLOCK` — attrs
        // come before the paren-form, which is a signature.
        // When inactive, the grammar is `sub NAME [PROTO] [ATTRS]
        // BLOCK` — the paren-form is a prototype.
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

        // The effective prototype (for symbol-table purposes) may
        // come from either the paren form or a `:prototype(...)`
        // attribute.  With signatures active the paren-form is a
        // signature, so only the attribute contributes.
        let effective_proto_raw = attributes.iter().find(|a| a.name == "prototype").and_then(|a| a.value.clone()).or_else(|| prototype_raw.clone());

        let prototype_parsed = match &effective_proto_raw {
            Some(raw) => Some(SubPrototype::parse(raw).map_err(|e| ParseError::new(format!("invalid prototype: {}", e.message), start))?),
            None => None,
        };

        // Forward declaration: `sub name PROTO ATTRS;` with no body.
        if self.eat(&Token::Semi)? {
            let attr_names: Vec<String> = attributes.iter().map(|a| a.name.clone()).collect();
            self.symbols.entry(&self.current_package.clone()).declare_sub(&name, prototype_parsed, attr_names, true);
            // Represent as a SubDecl with an empty body for now; an
            // optional `body: None` variant would be cleaner, but
            // that's a separate AST change.
            let span = start.merge(self.peek_span());
            let body = Block { statements: Vec::new(), span };
            return Ok(StmtKind::SubDecl(SubDecl { name, scope: None, prototype: prototype_raw, attributes, signature, body, span }));
        }

        let body = self.parse_block()?;

        // Register the full definition, replacing any prior forward
        // declaration of the same name.
        let attr_names: Vec<String> = attributes.iter().map(|a| a.name.clone()).collect();
        self.symbols.entry(&self.current_package.clone()).declare_sub(&name, prototype_parsed, attr_names, false);

        Ok(StmtKind::SubDecl(SubDecl { name, scope: None, prototype: prototype_raw, attributes, signature, body, span: start.merge(self.peek_span()) }))
    }

    /// Parse an optional prototype: `($$)`, `(\@\%)`, etc.
    /// If `(` follows, consume it and scan the body as raw bytes
    /// until `)`, matching toke.c's `scan_str()` call in `yyl_sub()`.
    fn parse_prototype(&mut self) -> Result<Option<String>, ParseError> {
        if self.at(&Token::LeftParen)? {
            self.next_token()?; // consume (
            let proto = self.lexer.lex_body_str(b'(', true)?;
            Ok(Some(proto))
        } else {
            Ok(None)
        }
    }

    /// Parse an optional signature: `($x, $y, $z = default)`,
    /// `(@rest)`, etc.  Called instead of [`Self::parse_prototype`]
    /// when the `signatures` feature is active at the declaration
    /// site.  Returns `None` when no paren-form is present.
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
    /// This parser is permissive about slurpy placement — it
    /// accepts slurpy params anywhere and trusts a later semantic
    /// pass to diagnose "slurpy must be last".  Same for duplicate
    /// parameter names.
    fn parse_signature(&mut self) -> Result<Option<Signature>, ParseError> {
        if !self.at(&Token::LeftParen)? {
            return Ok(None);
        }
        let open = self.next_token()?; // consume (
        let start_span = open.span;

        let mut params = Vec::new();
        // Track the span of the first slurpy parameter (if any)
        // so we can reject anything that follows it.
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

    /// Parse one signature parameter.  Handles the parser/lexer
    /// interplay for sigils:
    ///
    /// * `Token::ScalarVar(name)` / `Token::ArrayVar(name)` arrive
    ///   pre-combined because the lexer greedily consumes `$ident`
    ///   / `@ident`.
    /// * `Token::HashVar` does NOT arrive pre-combined — the lexer
    ///   always emits `Token::Percent` and the parser opts in via
    ///   `lex_hash_var_after_percent()` when in term position.
    ///   We do that here.
    /// * Bare `$`/`@`/`%` (followed by a non-identifier) arrive as
    ///   `Token::Dollar` / `Token::At` / `Token::Percent`
    ///   respectively, and mean anonymous placeholders.
    /// * `$,`/`$)`/`$;` and similar get eagerly lexed as
    ///   `Token::SpecialVar(c)` because those are real punctuation
    ///   variables.  In a signature, `$` followed by a separator
    ///   is an anonymous scalar; we split the SpecialVar back into
    ///   a `Dollar` + synthetic delimiter.
    fn parse_sig_param(&mut self) -> Result<SigParam, ParseError> {
        // Intercept `SpecialVar(c)` where `c` is a signature
        // separator or `=` — splits into anon scalar + delimiter.
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
                    return Ok(SigParam::AnonScalar { default: Some((SigDefaultKind::Eq, Expr { kind: ExprKind::Undef, span })), span });
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
            // Push the delimiter into the lookahead cache so the
            // outer loop in parse_signature sees it next.
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
                // Either anon hash placeholder or named slurpy
                // hash; ask the lexer to probe for a hash name.
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
                Token::Keyword(kw) => Some((<&str>::from(kw)).to_string()),
                _ => None,
            };
            if let Some(name) = name {
                let name_span = self.peek_span();
                self.next_token()?; // eat the name
                // Optional parenthesized args.  For `:prototype(...)`
                // specifically, the body is Perl prototype syntax
                // (containing `$`, `@`, `%`, `\`, etc.) which must be
                // read as raw bytes — token-by-token reconstruction
                // via Display impls loses fidelity.  Other attributes
                // use the general token-reconstruction path.
                let value = if self.at(&Token::LeftParen)? {
                    self.next_token()?; // consume (
                    if name == "prototype" {
                        Some(self.lexer.lex_body_str(b'(', true)?)
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

    /// Parse a declaration in expression context: `my $x`, `our ($a, @b)`, etc.
    /// Returns a Decl expression; the Pratt parser handles `= expr` as assignment.
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
            Ok(Expr { kind: ExprKind::Decl(scope, vars), span: span.merge(end) })
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
            Ok(Expr { kind: ExprKind::Decl(scope, vars), span: span.merge(end) })
        }
    }

    /// Parse an anonymous sub expression: `sub { ... }`, `sub ($x) { ... }`,
    /// `sub :lvalue { ... }`, `sub ($) :lvalue { ... }`, etc.
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

        Ok(Expr { span: span.merge(body.span), kind: ExprKind::AnonSub(prototype, attributes, signature, body) })
    }

    fn parse_anon_method(&mut self, span: Span) -> Result<Expr, ParseError> {
        // Methods always act as if signatures are in effect.
        let attrs = self.parse_attributes()?;
        let sig = self.parse_signature()?;
        let body = self.parse_block()?;

        Ok(Expr { span: span.merge(body.span), kind: ExprKind::AnonMethod(attrs, sig, body) })
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

        // Consume '(' then decide: C-style or foreach based on
        // whether a `;` appears after the first expression.
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

    /// Parse the rest of a C-style for loop after `(` and the optional
    /// init expression have been consumed.  Next token should be `;`.
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
            other => return Err(ParseError::new(format!("expected package name, got {other:?}"), start)),
        };

        // Optional version
        let version = if matches!(self.peek_token(), Token::IntLit(_) | Token::FloatLit(_) | Token::VersionLit(_)) {
            Some(format!("{}", self.next_token()?.token))
        } else {
            None
        };

        // Ensure the package exists in the symbol table, even if
        // empty — so later references to it resolve correctly.
        let _ = self.symbols.entry(&name);

        let block = if self.at(&Token::LeftBrace)? {
            // Block form: `package Name { ... }` — switch packages
            // for the duration of the block, then restore.
            let saved = std::mem::replace(&mut self.current_package, std::sync::Arc::from(name.as_str()));
            let block = self.parse_block()?;
            self.current_package = saved;
            Some(block)
        } else {
            // Statement form: `package Name;` — switch packages for
            // everything that follows in this compilation unit.
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
            // Bare version: `use 5.020;` / `use v5.36;` — module slot
            // gets the version; no further version or imports.
            Token::IntLit(n) => {
                // Apply the matching bundle to pragma state.
                // `use 5.036` / `use 5036` → major=5, minor=36.
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
            Token::Keyword(kw) => (<&str>::from(kw)).to_string(),
            other => return Err(ParseError::new(format!("expected module name or version, got {other:?}"), start)),
        };

        // Optional version after the module name: `use Module 1.23;`
        // or `use Module v5.26;`.  Versions are either numeric literals or
        // v-string VersionLit tokens; anything else starts the import list.
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

        // Pragma dispatch: apply any side effects to parser state
        // before returning.  Unknown modules and non-pragma
        // imports are silently ignored here; they'd require
        // runtime module loading to take effect.
        apply_pragma(&mut self.pragmas, &module, is_no, imports.as_ref());

        // Sync shared UTF-8 flag — the lexer reads this to
        // decide whether to accept multi-byte identifiers.
        self.lexer.utf8_mode = self.pragmas.utf8;

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
        let name = if let Token::Ident(_) = self.peek_token() {
            match self.next_token()?.token {
                Token::Ident(s) => s,
                _ => unreachable!(),
            }
        } else {
            "STDOUT".to_string()
        };

        // Expect '='
        self.expect_token(&Token::Assign(AssignOp::Eq))?;

        // Hand off to the lexer's format sublex mode.  The next
        // token will be FormatSublexBegin; the body ends at
        // SublexEnd (emitted for the `.` terminator).
        //
        // Careful: do NOT call `peek_span` here — that would invoke
        // the lexer and potentially tokenize into the first body
        // line, which `start_format` would then discard when it
        // drops `current_line`.  Build the begin span from `start`
        // and the current (pre-body) lexer position instead.
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

    /// Parse a picture line after `FormatPictureBegin(repeat)` has
    /// been consumed.  Consumes tokens until `FormatPictureEnd`,
    /// then the following `FormatArgsBegin` / expressions /
    /// `FormatArgsEnd` group, and assembles a `FormatLine::Picture`.
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

        // Peek: if `{` is the first args token, consume it and
        // switch the lexer to braced mode.
        let braced = matches!(self.peek_token(), Token::LeftBrace);
        if braced {
            self.next_token()?; // consume `{`
            self.lexer.format_args_enter_braced();
            // Clear any cached token — the mode switch may affect
            // how the next token is produced.
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
            other => return Err(ParseError::new(format!("expected method name, got {other:?}"), start)),
        };

        // Methods get the same signature-vs-prototype dispatch as
        // regular subs.  (Inside `class { ... }` with signatures
        // active, methods bind `$self` implicitly — captured at
        // runtime, not in the parser AST.)
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

    /// Inspect a block parsed at statement level and determine if it
    /// should be reclassified as an anonymous hash constructor.
    ///
    /// Returns `Ok(expr)` with an `AnonHash` expression if the block
    /// looks like a hash constructor, or `Err(block)` to keep it as
    /// a block.
    ///
    /// A block is reclassified as a hash constructor when:
    /// - It contains exactly one statement.
    /// - That statement is a plain expression (not `my`, `if`, etc.).
    /// - The expression was NOT terminated by a semicolon.
    /// - The expression contains a top-level fat comma (`=>`), OR
    ///   the expression is a comma-list whose first element is a
    ///   string literal, non-lowercase bareword, or other non-function
    ///   term.
    ///
    /// This matches Perl's behavior for common cases while being
    /// strictly more accurate than the byte-level heuristic it
    /// replaces, because it operates on parsed AST nodes rather
    /// than raw bytes.
    fn try_reclassify_as_hash(block: Block) -> Result<Expr, Block> {
        // Empty block → empty hash (matching Perl's toke.c line 6368).
        if block.statements.is_empty() {
            return Ok(Expr { kind: ExprKind::AnonHash(Vec::new()), span: block.span });
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
            // Flatten List into individual AnonHash elements to
            // match the structure produced by parse_term for hash
            // constructors in term position.
            let elems = match expr.kind {
                ExprKind::List(items) => items,
                _ => vec![expr],
            };
            Ok(Expr { kind: ExprKind::AnonHash(elems), span })
        } else {
            Err(block)
        }
    }

    /// Check whether an expression looks like hash constructor
    /// content rather than a block body.
    ///
    /// Matches Perl's byte-level heuristic at the AST level:
    /// - A comma-list whose first element is a string literal,
    ///   numeric literal, variable, or non-lowercase bareword is
    ///   hash-like (Perl: non-lowercase first byte + comma → hash).
    /// - A comma-list whose first element is a lowercase bareword
    ///   or function call is block-like (`func arg, arg`).
    /// - A non-list expression (no commas) is block-like.
    ///
    /// Fat comma (`=>`) autoquotes barewords to StringLit before we
    /// see them, so `key => val` appears as `List([StringLit, val])`.
    fn looks_like_hash_expr(expr: &Expr) -> bool {
        match &expr.kind {
            // A comma-list: check the first element.
            ExprKind::List(items) => {
                match items.first().map(|e| &e.kind) {
                    // String literal — covers autoquoted barewords from
                    // fat comma, explicit strings, q//.
                    Some(ExprKind::StringLit(_)) => true,
                    // Numeric literals.
                    Some(ExprKind::IntLit(_)) => true,
                    Some(ExprKind::FloatLit(_)) => true,
                    // Variables — `$x => 1` or `$x, 1`.  Perl's
                    // heuristic: `$` is not lowercase → hash.
                    Some(ExprKind::ScalarVar(_)) => true,
                    Some(ExprKind::ArrayVar(_)) => true,
                    Some(ExprKind::HashVar(_)) => true,
                    // Unary prefix on a variable — `-$x => 1`.
                    Some(ExprKind::UnaryOp(_, _)) => true,
                    // Non-lowercase bareword — `Foo, 1`.
                    Some(ExprKind::Bareword(name)) => name.starts_with(|c: char| c.is_ascii_uppercase() || c == '_'),
                    // Lowercase bareword looks like `func arg, arg`.
                    // Anything else (function calls, complex exprs) → block.
                    _ => false,
                }
            }
            // No commas at all — not hash-like.
            _ => false,
        }
    }

    // ── Block parsing ─────────────────────────────────────────

    fn parse_block(&mut self) -> Result<Block, ParseError> {
        self.with_descent(|this| {
            // Pragmas and current_package are lexically scoped: any
            // `use feature`, `use utf8`, or `package Name;` inside
            // this block doesn't leak out.  Save state before parsing,
            // restore after.
            let saved_pragmas = this.pragmas;
            let saved_package = this.current_package.clone();

            let start = this.peek_span();
            let result = (|this: &mut Parser| -> Result<Block, ParseError> {
                this.expect_token(&Token::LeftBrace)?;

                let mut statements = Vec::new();
                while !this.at(&Token::RightBrace)? && !this.at_eof()? {
                    statements.push(this.parse_statement()?);
                }

                let end = this.peek_span();
                this.expect_token(&Token::RightBrace)?;

                Ok(Block { statements, span: start.merge(end) })
            })(this);

            this.pragmas = saved_pragmas;
            this.current_package = saved_package;
            // Sync shared UTF-8 flag with the restored state.
            this.lexer.utf8_mode = this.pragmas.utf8;
            result
        })
    }

    fn parse_paren_expr(&mut self) -> Result<Expr, ParseError> {
        self.expect_token(&Token::LeftParen)?;
        let expr = self.parse_expr(PREC_LOW)?;
        self.expect_token(&Token::RightParen)?;
        Ok(expr)
    }

    // ── Expression parsing (Pratt) ────────────────────────────

    fn parse_expr(&mut self, min_prec: Precedence) -> Result<Expr, ParseError> {
        self.with_descent(|this| {
            let left = this.parse_term()?;
            this.parse_expr_continuation(left, min_prec)
        })
    }

    /// Continue parsing an expression from a pre-built left-hand side.
    /// Runs the Pratt operator loop without calling parse_term first.
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

        // Fat comma autoquotes keywords: `if => 1` produces StringLit("if").
        // RightBrace also autoquotes: $hash{if} produces StringLit("if").
        if let Token::Keyword(kw) = &spanned.token
            && matches!(self.peek_token(), Token::FatComma | Token::RightBrace)
        {
            let name: &str = (*kw).into();
            return Ok(Expr { kind: ExprKind::StringLit(name.to_string()), span });
        }

        match spanned.token {
            Token::IntLit(n) => Ok(Expr { kind: ExprKind::IntLit(n), span }),
            Token::FloatLit(n) => Ok(Expr { kind: ExprKind::FloatLit(n), span }),
            Token::StrLit(s) => Ok(Expr { kind: ExprKind::StringLit(s), span }),
            Token::VersionLit(s) => Ok(Expr { kind: ExprKind::VersionLit(s), span }),

            // Interpolating string: collect sub-tokens into AST.
            Token::QuoteSublexBegin(_, _) => self.parse_interpolated_string(span),

            // << in term position: try heredoc.  The lexer emitted
            // ShiftLeft; we ask it to attempt heredoc detection.
            // If it can't find a valid tag, that's a parse error
            // (shift-left is not a valid term).
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
                    Some(Token::HeredocLit(_kind, _tag, body)) => Ok(Expr { kind: ExprKind::StringLit(body), span }),
                    // <<>> double diamond — safe version of <>.
                    Some(Token::Readline(content, safe)) => Self::readline_expr(content, safe, span),
                    Some(other) => unreachable!("unexpected heredoc token: {other:?}"),
                    None => Err(ParseError::new("expected heredoc tag after <<", span)),
                }
            }

            Token::ScalarVar(name) => {
                let expr = Expr { kind: ExprKind::ScalarVar(name), span };
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
                    Ok(Expr { span: span.merge(end), kind: ExprKind::ArraySlice(Box::new(Expr { kind: ExprKind::ArrayVar(name), span }), indices) })
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
                    Ok(Expr { span: span.merge(end), kind: ExprKind::HashSlice(Box::new(Expr { kind: ExprKind::ArrayVar(name), span }), keys) })
                } else {
                    Ok(Expr { kind: ExprKind::ArrayVar(name), span })
                }
            }
            Token::HashVar(name) => {
                // %hash{keys} → kv hash slice; %hash[indices] → kv array slice (5.20+)
                let recv = Expr { kind: ExprKind::HashVar(name), span };
                self.maybe_kv_slice(recv, span)
            }
            Token::GlobVar(name) => {
                let expr = Expr { kind: ExprKind::GlobVar(name), span };
                // *foo{THING} — typeglob slot access
                if self.at(&Token::LeftBrace)? {
                    self.next_token()?;
                    let key = self.parse_hash_subscript_key()?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBrace)?;
                    Ok(Expr { span: span.merge(end), kind: ExprKind::ArrowDeref(Box::new(expr), ArrowTarget::HashElem(Box::new(key))) })
                } else {
                    Ok(expr)
                }
            }
            Token::ArrayLen(name) => Ok(Expr { kind: ExprKind::ArrayLen(name), span }),
            Token::SpecialVar(name) => {
                let expr = Expr { kind: ExprKind::SpecialVar(name), span };
                self.maybe_postfix_subscript(expr)
            }
            Token::SpecialArrayVar(name) => Ok(Expr { kind: ExprKind::SpecialArrayVar(name), span }),
            Token::SpecialHashVar(name) => Ok(Expr { kind: ExprKind::SpecialHashVar(name), span }),

            // % in term position: try hash variable first, then
            // fall through to hash-deref (%$ref, %{expr}).
            Token::Percent => {
                match self.lexer.lex_hash_var_after_percent()? {
                    Some(Token::HashVar(name)) => {
                        let recv = Expr { kind: ExprKind::HashVar(name), span };
                        self.maybe_kv_slice(recv, span)
                    }
                    Some(Token::SpecialHashVar(name)) => Ok(Expr { kind: ExprKind::SpecialHashVar(name), span }),
                    Some(other) => unreachable!("unexpected hash token: {other:?}"),
                    None => {
                        // No hash name — must be a deref (%$ref, %{expr}).
                        if self.at(&Token::LeftBrace)? {
                            self.next_token()?;
                            let inner = self.parse_expr(PREC_LOW)?;
                            let end = self.peek_span();
                            self.expect_token(&Token::RightBrace)?;
                            Ok(Expr { span: span.merge(end), kind: ExprKind::Deref(Sigil::Hash, Box::new(inner)) })
                        } else {
                            let operand = self.parse_deref_operand()?;
                            Ok(Expr { span: span.merge(operand.span), kind: ExprKind::Deref(Sigil::Hash, Box::new(operand)) })
                        }
                    }
                }
            }

            // Prefix dereference: $$ref, @$ref, %$ref, ${expr}, @{expr}
            Token::Dollar => {
                if self.at(&Token::LeftBrace)? {
                    // ${expr} — dereference block
                    self.next_token()?;
                    let inner = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBrace)?;
                    let expr = Expr { span: span.merge(end), kind: ExprKind::Deref(Sigil::Scalar, Box::new(inner)) };
                    self.maybe_postfix_subscript(expr)
                } else {
                    // $$ref — consume just the variable, subscripts apply to deref result.
                    // Recursive: $$$ref → Deref(Scalar, Deref(Scalar, ScalarVar("ref")))
                    let operand = self.parse_deref_operand()?;
                    let expr = Expr { span: span.merge(operand.span), kind: ExprKind::Deref(Sigil::Scalar, Box::new(operand)) };
                    self.maybe_postfix_subscript(expr)
                }
            }
            Token::At => {
                if self.at(&Token::LeftBrace)? {
                    // @{expr} — array dereference block
                    self.next_token()?;
                    let inner = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBrace)?;
                    Ok(Expr { span: span.merge(end), kind: ExprKind::Deref(Sigil::Array, Box::new(inner)) })
                } else {
                    let operand = self.parse_deref_operand()?;
                    Ok(Expr { span: span.merge(operand.span), kind: ExprKind::Deref(Sigil::Array, Box::new(operand)) })
                }
            }

            // Ampersand prefix: &foo, &foo(args), &$coderef(args), &{expr}(args)
            Token::BitAnd => {
                if self.at(&Token::LeftBrace)? {
                    // &{expr}
                    self.next_token()?;
                    let inner = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBrace)?;
                    let deref = Expr { span: span.merge(end), kind: ExprKind::Deref(Sigil::Code, Box::new(inner)) };
                    self.maybe_call_args(deref)
                } else if let Token::Ident(_) = self.peek_token() {
                    // &foo or &foo(args)
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
                        Ok(Expr { kind: ExprKind::FuncCall(name, args), span: span.merge(end) })
                    } else {
                        // &foo with no parens — call with current @_
                        Ok(Expr { kind: ExprKind::FuncCall(name, vec![]), span: span.merge(name_span) })
                    }
                } else {
                    // &$coderef or &$coderef(args)
                    let operand = self.parse_deref_operand()?;
                    let deref = Expr { span: span.merge(operand.span), kind: ExprKind::Deref(Sigil::Code, Box::new(operand)) };
                    self.maybe_call_args(deref)
                }
            }

            // Typeglob: *foo, *$ref, *{expr}
            Token::Star => {
                if self.at(&Token::LeftBrace)? {
                    self.next_token()?;
                    let inner = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBrace)?;
                    Ok(Expr { span: span.merge(end), kind: ExprKind::Deref(Sigil::Glob, Box::new(inner)) })
                } else if let Token::Ident(_) = self.peek_token() {
                    let name_span = self.peek_span();
                    let name = match self.next_token()?.token {
                        Token::Ident(s) => s,
                        _ => unreachable!(),
                    };
                    let expr = Expr { kind: ExprKind::GlobVar(name), span: span.merge(name_span) };
                    // *foo{THING} — typeglob slot access
                    if self.at(&Token::LeftBrace)? {
                        self.next_token()?;
                        let key = self.parse_hash_subscript_key()?;
                        let end = self.peek_span();
                        self.expect_token(&Token::RightBrace)?;
                        Ok(Expr { span: span.merge(end), kind: ExprKind::ArrowDeref(Box::new(expr), ArrowTarget::HashElem(Box::new(key))) })
                    } else {
                        Ok(expr)
                    }
                } else {
                    let operand = self.parse_deref_operand()?;
                    Ok(Expr { span: span.merge(operand.span), kind: ExprKind::Deref(Sigil::Glob, Box::new(operand)) })
                }
            }

            Token::Ident(name) => self.parse_ident_term(name, span),

            // Compile-time constants.  SourceFile/SourceLine carry
            // their values in the token itself (captured at lex
            // time); CurrentPackage is a marker filled with the
            // parser's current package; CurrentSub is a marker
            // that evaluates at runtime.
            Token::SourceFile(path) => Ok(Expr { kind: ExprKind::SourceFile(path), span }),
            Token::SourceLine(n) => Ok(Expr { kind: ExprKind::SourceLine(n), span }),
            Token::CurrentPackage => {
                let pkg = self.current_package.to_string();
                Ok(Expr { kind: ExprKind::CurrentPackage(pkg), span })
            }
            Token::CurrentSub => {
                // Gated on the `current_sub` feature.  Without it,
                // `__SUB__` falls back to a bareword — matching
                // Perl's behavior where unknown-at-compile-time
                // barewords become string literals.
                if self.pragmas.features.contains(Features::CURRENT_SUB) {
                    Ok(Expr { kind: ExprKind::CurrentSub, span })
                } else {
                    Ok(Expr { kind: ExprKind::Bareword("__SUB__".to_string()), span })
                }
            }
            Token::CurrentClass => Ok(Expr { kind: ExprKind::CurrentClass, span }),

            Token::Keyword(Keyword::Undef) => Ok(Expr { kind: ExprKind::Undef, span }),
            Token::Keyword(Keyword::Wantarray) => Ok(Expr { kind: ExprKind::Wantarray, span }),

            // Prefix unary operators
            Token::Minus => {
                // -f, -d, -r, etc. → filetest operator (single letter
                // not followed by word-continuation char).
                if let Some(Token::Filetest(b)) = self.lexer.lex_filetest_after_minus() {
                    let end = self.peek_span();
                    return self.parse_filetest(b, span.merge(end));
                }
                // -bareword (not followed by parens) → StringLit("-bareword")
                // Perl: unary minus on an identifier always returns "-identifier".
                if let Token::Ident(name) = self.peek_token().clone() {
                    let ident_span = self.peek_span();
                    self.next_token()?; // consume the ident
                    if matches!(self.peek_token(), Token::LeftParen) {
                        // -func(...) → unary minus on function call.
                        let func = self.parse_ident_term(name, ident_span)?;
                        return Ok(Expr { span: span.merge(func.span), kind: ExprKind::UnaryOp(UnaryOp::Negate, Box::new(func)) });
                    }
                    // -bareword → StringLit("-bareword")
                    return Ok(Expr { kind: ExprKind::StringLit(format!("-{name}")), span: span.merge(ident_span) });
                }
                let operand = self.parse_expr(PREC_UNARY)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::UnaryOp(UnaryOp::Negate, Box::new(operand)) })
            }
            Token::Plus => {
                let operand = self.parse_expr(PREC_UNARY)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::UnaryOp(UnaryOp::NumPositive, Box::new(operand)) })
            }
            Token::Bang => {
                let operand = self.parse_expr(PREC_UNARY)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::UnaryOp(UnaryOp::LogNot, Box::new(operand)) })
            }
            Token::Tilde => {
                let operand = self.parse_expr(PREC_UNARY)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::UnaryOp(UnaryOp::BitNot, Box::new(operand)) })
            }
            Token::StringBitNot => {
                let operand = self.parse_expr(PREC_UNARY)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::UnaryOp(UnaryOp::StringBitNot, Box::new(operand)) })
            }
            Token::Backslash => {
                let operand = self.parse_expr(PREC_UNARY)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::Ref(Box::new(operand)) })
            }
            Token::Keyword(Keyword::Not) => {
                let operand = self.parse_expr(PREC_NOT_LOW)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::UnaryOp(UnaryOp::Not, Box::new(operand)) })
            }
            Token::PlusPlus => {
                let operand = self.parse_expr(PREC_INC)?;
                if !Self::is_valid_lvalue(&operand) {
                    return Err(ParseError::new("invalid operand for prefix ++", operand.span));
                }
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::UnaryOp(UnaryOp::PreInc, Box::new(operand)) })
            }
            Token::MinusMinus => {
                let operand = self.parse_expr(PREC_INC)?;
                if !Self::is_valid_lvalue(&operand) {
                    return Err(ParseError::new("invalid operand for prefix --", operand.span));
                }
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::UnaryOp(UnaryOp::PreDec, Box::new(operand)) })
            }

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

            // local is different — it dynamically scopes any lvalue,
            // not just bare variables: local $hash{key}, local $/, local *GLOB
            Token::Keyword(Keyword::Local) => {
                let operand = self.parse_expr(PREC_UNARY)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::Local(Box::new(operand)) })
            }

            // Anonymous sub: sub { ... } or sub ($x) { ... }
            Token::Keyword(Keyword::Sub) => self.parse_anon_sub(span),

            // Anonymous method: method { ... } or method ($x) { ... }
            Token::Keyword(Keyword::Method) => self.parse_anon_method(span),

            // eval BLOCK vs eval EXPR
            Token::Keyword(Keyword::Eval) => {
                if self.at(&Token::LeftBrace)? {
                    let block = self.parse_block()?;
                    Ok(Expr { span: span.merge(block.span), kind: ExprKind::EvalBlock(block) })
                } else {
                    let arg = self.parse_expr(PREC_COMMA)?;
                    let end = span.merge(arg.span);
                    Ok(Expr { kind: ExprKind::EvalExpr(Box::new(arg)), span: end })
                }
            }

            // return with optional value
            Token::Keyword(Keyword::Return) => {
                if self.at(&Token::Semi)? || self.at(&Token::RightBrace)? || self.at_eof()? {
                    Ok(Expr { kind: ExprKind::FuncCall("return".into(), vec![]), span })
                } else {
                    let val = self.parse_expr(PREC_COMMA)?;
                    let end = span.merge(val.span);
                    Ok(Expr { kind: ExprKind::FuncCall("return".into(), vec![val]), span: end })
                }
            }

            // last/next/redo with optional label
            Token::Keyword(Keyword::Last) | Token::Keyword(Keyword::Next) | Token::Keyword(Keyword::Redo) => {
                let name = match spanned.token {
                    Token::Keyword(Keyword::Last) => "last",
                    Token::Keyword(Keyword::Next) => "next",
                    Token::Keyword(Keyword::Redo) => "redo",
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
                    Ok(Expr { kind: ExprKind::FuncCall(name.into(), vec![Expr { kind: ExprKind::StringLit(label), span: label_span }]), span: end })
                } else {
                    Ok(Expr { kind: ExprKind::FuncCall(name.into(), vec![]), span })
                }
            }

            // stat / lstat — dedicated AST nodes with StatTarget
            Token::Keyword(Keyword::Stat) => self.parse_stat_op(false, span),
            Token::Keyword(Keyword::Lstat) => self.parse_stat_op(true, span),

            // Named unary keywords
            Token::Keyword(kw) if keyword::is_named_unary(kw) => self.parse_named_unary(kw, span),

            // List operators
            Token::Keyword(kw) if keyword::is_list_op(kw) => self.parse_list_op(kw, span),

            // Parenthesized expression or list
            Token::LeftParen => {
                if self.at(&Token::RightParen)? {
                    self.next_token()?;
                    let expr = Expr { kind: ExprKind::List(vec![]), span };
                    self.maybe_postfix_subscript(expr)
                } else {
                    let inner = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightParen)?;
                    let expr = Expr { kind: ExprKind::Paren(Box::new(inner)), span: span.merge(end) };
                    // `(LIST)[idx]` and `(LIST){key}` are list/hash
                    // slices — valid postfix subscripts on parens.
                    self.maybe_postfix_subscript(expr)
                }
            }

            // Anonymous array ref [...]
            Token::LeftBracket => {
                let mut elems = Vec::new();
                while !self.at(&Token::RightBracket)? && !self.at_eof()? {
                    elems.push(self.parse_expr(PREC_COMMA + 1)?);
                    if !self.eat(&Token::Comma)? {
                        break;
                    }
                }
                let end = self.peek_span();
                self.expect_token(&Token::RightBracket)?;
                Ok(Expr { kind: ExprKind::AnonArray(elems), span: span.merge(end) })
            }

            // Anonymous hash constructor: {key => val, ...}
            // In term position, `{` is always a hash constructor.
            Token::LeftBrace => {
                let mut elems = Vec::new();
                while !self.at(&Token::RightBrace)? && !self.at_eof()? {
                    elems.push(self.parse_expr(PREC_COMMA + 1)?);
                    if !self.eat(&Token::Comma)? && !self.eat(&Token::FatComma)? {
                        break;
                    }
                }
                let end = self.peek_span();
                self.expect_token(&Token::RightBrace)?;
                Ok(Expr { kind: ExprKind::AnonHash(elems), span: span.merge(end) })
            }

            Token::QwList(words) => Ok(Expr { kind: ExprKind::QwList(words), span }),

            // Regex, substitution, transliteration
            Token::RegexSublexBegin(kind, _delim) => {
                let pattern = self.parse_interpolated()?;
                let flags = self.lexer.scan_adjacent_word_chars();
                if let Some(ref f) = flags {
                    Self::validate_regex_flags(f, span)?;
                }
                Ok(Expr { kind: ExprKind::Regex(kind, pattern, flags), span })
            }
            // // in term position is an empty regex, not defined-or.
            Token::DefinedOr => {
                let flags = self.lexer.scan_adjacent_word_chars();
                if let Some(ref f) = flags {
                    Self::validate_regex_flags(f, span)?;
                }
                Ok(Expr { kind: ExprKind::Regex(RegexKind::Match, Interpolated(vec![]), flags), span })
            }
            // / in term position is a regex, not division.
            Token::Slash => {
                let pattern = self.lexer.lex_body_str(b'/', true)?;
                let flags = self.lexer.scan_adjacent_word_chars();
                if let Some(ref f) = flags {
                    Self::validate_regex_flags(f, span)?;
                }
                Ok(Expr { kind: ExprKind::Regex(RegexKind::Match, Interpolated(vec![InterpPart::Const(pattern)]), flags), span })
            }
            // /= in term position: = is the first character of the
            // regex pattern, not a division-assignment operator.
            Token::Assign(AssignOp::DivEq) => {
                self.lexer.rewind(1);
                let pattern = self.lexer.lex_body_str(b'/', true)?;
                let flags = self.lexer.scan_adjacent_word_chars();
                if let Some(ref f) = flags {
                    Self::validate_regex_flags(f, span)?;
                }
                Ok(Expr { kind: ExprKind::Regex(RegexKind::Match, Interpolated(vec![InterpPart::Const(pattern)]), flags), span })
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
                    // With /e: body is raw bytes in a single ConstSegment.
                    // Reparse as code.
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
                        _ => Expr { kind: ExprKind::StringLit(raw), span },
                    };
                    Interpolated(vec![InterpPart::ExprInterp(Box::new(expr))])
                } else {
                    // Without /e: body is an interpolated string.
                    self.parse_interpolated()?
                };
                let end = self.peek_span();
                Ok(Expr { kind: ExprKind::Subst(pattern, replacement, flags), span: span.merge(end) })
            }
            Token::TranslitLit(from, to, flags) => {
                if let Some(ref f) = flags {
                    Self::validate_tr_flags(f, span)?;
                }
                Ok(Expr { kind: ExprKind::Translit(from, to, flags), span })
            }

            // Heredoc (body already collected by lexer).
            // Literal heredocs (body collected by lexer as raw string).
            // Interpolating heredocs come through QuoteSublexBegin → tokens → SublexEnd.
            Token::HeredocLit(_kind, _tag, body) => Ok(Expr { kind: ExprKind::StringLit(body), span }),

            // sort/map/grep with optional block
            Token::Keyword(kw) if keyword::is_block_list_op(kw) => self.parse_block_list_op(kw, span),

            // print/say with optional filehandle
            Token::Keyword(kw) if keyword::is_print_op(kw) => self.parse_print_op(kw, span),

            // goto LABEL, goto &sub, goto EXPR
            Token::Keyword(Keyword::Goto) => {
                let arg = self.parse_expr(PREC_COMMA)?;
                let end = span.merge(arg.span);
                Ok(Expr { kind: ExprKind::FuncCall("goto".into(), vec![arg]), span: end })
            }
            Token::Keyword(Keyword::Dump) => {
                // `dump LABEL` — same precedence as goto.
                // Deprecated; mostly used to create core dumps.
                if self.at_eof()? || self.at(&Token::Semi)? {
                    Ok(Expr { kind: ExprKind::FuncCall("dump".into(), vec![]), span })
                } else {
                    let arg = self.parse_expr(PREC_COMMA)?;
                    let end = span.merge(arg.span);
                    Ok(Expr { kind: ExprKind::FuncCall("dump".into(), vec![arg]), span: end })
                }
            }

            // Filetest operators: -e, -f, -d, etc. (lexed as single token)
            Token::Filetest(test_byte) => self.parse_filetest(test_byte, span),

            // Yada yada yada (...)
            Token::DotDotDot => Ok(Expr { kind: ExprKind::YadaYada, span }),

            // Readline / diamond: <STDIN>, <>, <$fh>, <*.txt>
            Token::Readline(content, safe) => Self::readline_expr(content, safe, span),

            // < in term position: try readline.  The lexer emitted NumLt;
            // we ask it to attempt readline scanning.  If not a readline,
            // that's a parse error (less-than is not a valid term).
            Token::NumLt => {
                if let Some(Token::Readline(content, safe)) = self.lexer.lex_readline_after_lt() {
                    let end = self.peek_span();
                    Self::readline_expr(content, safe, span.merge(end))
                } else {
                    Err(ParseError::new("expected readline or glob after <", span))
                }
            }

            Token::Keyword(Keyword::Do) => {
                if self.at(&Token::LeftBrace)? {
                    let block = self.parse_block()?;
                    Ok(Expr { span: span.merge(block.span), kind: ExprKind::DoBlock(block) })
                } else {
                    let arg = self.parse_expr(PREC_UNARY)?;
                    Ok(Expr { span: span.merge(arg.span), kind: ExprKind::DoExpr(Box::new(arg)) })
                }
            }

            other => Err(ParseError::new(format!("expected expression, got {other:?}"), span)),
        }
    }

    fn parse_ident_term(&mut self, name: String, span: Span) -> Result<Expr, ParseError> {
        // Autoquote: bareword followed by `=>` (fat comma) or `}` (hash subscript)
        if matches!(self.peek_token(), Token::FatComma | Token::RightBrace) {
            return Ok(Expr { kind: ExprKind::StringLit(name), span });
        }

        // Feature-gated named-unary builtins.  `fc` and
        // `evalbytes` become operators only when their feature is
        // active; without the feature they're ordinary
        // identifiers, which fall through to the general
        // function-call/bareword path below.
        let feature_gated_unary = match name.as_str() {
            "fc" if self.pragmas.features.contains(Features::FC) => true,
            "evalbytes" if self.pragmas.features.contains(Features::EVALBYTES) => true,
            _ => false,
        };
        if feature_gated_unary {
            return self.parse_feature_named_unary(name, span);
        }

        // Look up in the symbol table to see if this is a known sub.
        // Clone the prototype (small: raw string + a Vec of slot enums)
        // and the "is known" flag so we can release the borrow on self
        // before parsing args.
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
            return Ok(Expr { kind: ExprKind::FuncCall(name, args), span: span.merge(end) });
        }

        // No parens — if we know this sub has a prototype, use it to
        // drive argument parsing.
        if let Some(proto) = proto {
            return self.parse_prototyped_call(name, span, &proto);
        }

        // No parens, no prototype, but the sub is known: parse as a
        // list operator call (greedy args until end-of-statement).
        if is_known_sub {
            return self.parse_known_sub_call(name, span);
        }

        // Indirect object syntax: METHOD CLASS ARGS
        // e.g. new Foo(args), new Foo args
        // Heuristic: bareword followed by a capitalized bareword or $var.
        match self.peek_token() {
            Token::Ident(class_name) if class_name.starts_with(|c: char| c.is_ascii_uppercase()) => {
                let class_name = class_name.clone();
                let class_span = self.peek_span();
                self.next_token()?; // eat class name
                let class_expr = Expr { kind: ExprKind::Bareword(class_name), span: class_span };

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
                    return Ok(Expr { kind: ExprKind::IndirectMethodCall(Box::new(class_expr), name, args), span: span.merge(end) });
                }

                return Ok(Expr { kind: ExprKind::IndirectMethodCall(Box::new(class_expr), name, args), span: span.merge(class_span) });
            }
            Token::ScalarVar(_) => {
                let var_span = self.peek_span();
                let var = match self.next_token()?.token {
                    Token::ScalarVar(n) => n,
                    _ => unreachable!(),
                };
                let invocant = Expr { kind: ExprKind::ScalarVar(var), span: var_span };

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
                    return Ok(Expr { kind: ExprKind::IndirectMethodCall(Box::new(invocant), name, args), span: span.merge(end) });
                }

                return Ok(Expr { kind: ExprKind::IndirectMethodCall(Box::new(invocant), name, args), span: span.merge(var_span) });
            }
            _ => {}
        }

        // Bare identifier — not followed by ( or indirect object context.
        Ok(Expr { kind: ExprKind::Bareword(name), span })
    }

    /// True if the current token marks the end of a list-op / prototyped
    /// argument list: statement terminator, closing bracket/brace/paren,
    /// EOF, or a postfix-control keyword.
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

    /// Parse a call to a known sub (no prototype) in list-operator
    /// style: greedy comma-separated args until end of statement.
    /// Produces `FuncCall` (not `ListOp`, which is reserved for
    /// built-in list operators like `push`, `join`).
    fn parse_known_sub_call(&mut self, name: String, start: Span) -> Result<Expr, ParseError> {
        let mut args = Vec::new();
        while !self.at_args_end()? {
            args.push(self.parse_expr(PREC_COMMA + 1)?);
            if !self.eat(&Token::Comma)? {
                break;
            }
        }
        let end_span = args.last().map(|a| a.span).unwrap_or(start);
        Ok(Expr { kind: ExprKind::FuncCall(name, args), span: start.merge(end_span) })
    }

    /// Parse a call whose target sub has a known prototype.  Arguments
    /// are consumed according to the prototype slots; trailing content
    /// is left for the outer parser to deal with.
    ///
    /// * `$`, `_`, `*`, `+`, `\X`, `\[...]` — one scalar-ish expression
    ///   per slot, stopping at comma precedence.  Optional comma
    ///   consumed between slots.
    /// * `&` — expect `{ ... }`, parsed as an anonymous sub body.
    /// * `@`, `%` — slurpy, consumes all remaining comma-separated
    ///   arguments.  Always last.
    ///
    /// Missing required arguments are silently tolerated (Perl would
    /// error at compile time).  A later semantic pass can validate.
    fn parse_prototyped_call(&mut self, name: String, start: Span, proto: &SubPrototype) -> Result<Expr, ParseError> {
        let mut args = Vec::new();

        for (i, slot) in proto.slots.iter().enumerate() {
            let is_optional = i >= proto.required;

            if self.at_args_end()? {
                // No more input.  The `_` slot is special: when
                // omitted, it defaults to the global default variable
                // ($_), regardless of required/optional status.  All
                // other slots simply stop; a later semantic pass can
                // validate required-arg counts.
                if matches!(slot, ProtoSlot::DefaultedScalar) {
                    args.push(Expr { kind: ExprKind::DefaultVar, span: self.peek_span() });
                }
                let _ = is_optional;
                break;
            }

            match slot {
                ProtoSlot::Block => {
                    // `&` slot accepts either:
                    //   - A literal block `{ ... }`, but ONLY when
                    //     this is the initial slot.  That's the
                    //     map/grep/sort pattern: `foo { ... } @list`.
                    //     In non-initial positions, `{` at a call
                    //     site is an ordinary hash-ref constructor;
                    //     code references must be explicit.
                    //   - A code reference expression: `\&name`,
                    //     `$coderef`, `sub { ... }`, etc.  Parsed at
                    //     named-unary precedence.
                    let arg = if i == 0 && self.at(&Token::LeftBrace)? {
                        let block = self.parse_block()?;
                        let span = block.span;
                        Expr { kind: ExprKind::AnonSub(None, vec![], None, block), span }
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
                    // Consume all remaining tokens as comma-separated
                    // expressions.  Slurpy is always last.
                    while !self.at_args_end()? {
                        args.push(self.parse_expr(PREC_COMMA + 1)?);
                        if !self.eat(&Token::Comma)? {
                            break;
                        }
                    }
                    break;
                }
                ProtoSlot::Glob => {
                    // `*` slot: a bare identifier is auto-promoted to
                    // a typeglob reference (e.g., `foo STDIN` becomes
                    // `foo(*STDIN)`).  Any other expression — a glob
                    // literal `*NAME`, a scalar holding a glob ref,
                    // etc. — is parsed normally at named-unary
                    // precedence.
                    let arg = if let Token::Ident(_) = self.peek_token() {
                        let glob_span = self.peek_span();
                        let name = match self.next_token()?.token {
                            Token::Ident(n) => n,
                            _ => unreachable!(),
                        };
                        Expr { kind: ExprKind::GlobVar(name), span: glob_span }
                    } else {
                        self.parse_expr(PREC_NAMED_UNARY)?
                    };
                    args.push(arg);
                    if i + 1 < proto.slots.len() {
                        self.eat(&Token::Comma)?;
                    }
                }
                ProtoSlot::AutoRef(_) | ProtoSlot::AutoRefOneOf(_) | ProtoSlot::ArrayOrHash => {
                    // Auto-reference slots: `\$`, `\@`, `\%`, `\&`,
                    // `\*`, `\[...]`, and `+` (which is effectively
                    // `\[@%]`).  The argument is parsed at named-
                    // unary precedence and then wrapped in a Ref
                    // expression — the call site receives a reference
                    // to the variable rather than its value.  Whether
                    // the argument is actually of the expected kind
                    // (array for `\@`, etc.) is a semantic-pass
                    // concern, not a parsing one.
                    let arg = self.parse_expr(PREC_NAMED_UNARY)?;
                    let span = arg.span;
                    args.push(Expr { kind: ExprKind::Ref(Box::new(arg)), span });
                    if i + 1 < proto.slots.len() {
                        self.eat(&Token::Comma)?;
                    }
                }
                _ => {
                    // Scalar-ish slot (`$`, `_`).  One expression at
                    // named-unary precedence: operators tighter than
                    // named unary (+ - * / << >>, etc.) are consumed;
                    // operators looser (< == , ?:, etc.) terminate
                    // the arg.  This matches Perl's semantics for
                    // prototyped subs whose slot is a single scalar.
                    let arg = self.parse_expr(PREC_NAMED_UNARY)?;
                    args.push(arg);
                    if i + 1 < proto.slots.len() {
                        self.eat(&Token::Comma)?;
                    }
                }
            }
        }

        let end_span = args.last().map(|a| a.span).unwrap_or(start);
        Ok(Expr { kind: ExprKind::FuncCall(name, args), span: start.merge(end_span) })
    }

    fn parse_named_unary(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let name = (<&str>::from(kw)).to_string();

        // Named unary with optional arg
        if self.at(&Token::Semi)? || self.at_eof()? || self.at(&Token::RightBrace)? || self.at(&Token::RightParen)? {
            // No argument
            return Ok(Expr { kind: ExprKind::FuncCall(name, vec![]), span });
        }

        // Operators that prefer defined-or: // after shift/pop/undef/etc.
        // is defined-or, not an empty regex argument.  Matches toke.c's
        // XTERMORDORDOR.
        if keyword::prefers_defined_or(kw) && matches!(self.peek_token(), Token::DefinedOr) {
            return Ok(Expr { kind: ExprKind::FuncCall(name, vec![]), span });
        }

        if self.at(&Token::LeftParen)? {
            self.next_token()?;
            let arg = self.parse_expr(PREC_LOW)?;
            let end = self.peek_span();
            self.expect_token(&Token::RightParen)?;
            return Ok(Expr { kind: ExprKind::FuncCall(name, vec![arg]), span: span.merge(end) });
        }

        // Parse one term as the argument
        let arg = self.parse_expr(PREC_COMMA)?;
        let end = span.merge(arg.span);
        Ok(Expr { kind: ExprKind::FuncCall(name, vec![arg]), span: end })
    }

    /// Parse a feature-gated named-unary builtin called by its
    /// string name (rather than a `Keyword` enum variant).
    ///
    /// Used for `fc` and `evalbytes`, which are feature-gated and
    /// therefore not in the keyword table: adding them there would
    /// make them keywords even when the feature is off, shadowing
    /// any user-defined sub of the same name.  This helper is
    /// essentially the string-name version of `parse_named_unary`
    /// minus the `prefers_defined_or` check (neither `fc` nor
    /// `evalbytes` needs it).
    fn parse_feature_named_unary(&mut self, name: String, span: Span) -> Result<Expr, ParseError> {
        // No argument: `fc;` or `fc)` or at EOF.
        if self.at(&Token::Semi)? || self.at_eof()? || self.at(&Token::RightBrace)? || self.at(&Token::RightParen)? {
            return Ok(Expr { kind: ExprKind::FuncCall(name, vec![]), span });
        }

        // Parenthesized: `fc($x)`.
        if self.at(&Token::LeftParen)? {
            self.next_token()?;
            let arg = self.parse_expr(PREC_LOW)?;
            let end = self.peek_span();
            self.expect_token(&Token::RightParen)?;
            return Ok(Expr { kind: ExprKind::FuncCall(name, vec![arg]), span: span.merge(end) });
        }

        // Unparenthesized: one argument at named-unary precedence.
        let arg = self.parse_expr(PREC_NAMED_UNARY)?;
        let end = span.merge(arg.span);
        Ok(Expr { kind: ExprKind::FuncCall(name, vec![arg]), span: end })
    }

    /// Parse the target of a stat-family operation (filetest, stat, lstat).
    ///
    /// Handles three cases:
    /// - Bare `_` → `StatTarget::StatCache`
    /// - No operand (`;`, `}`, `)`, EOF) → `StatTarget::Default`
    /// - Expression → `StatTarget::Expr(Box::new(expr))`
    ///
    /// Also handles the parenthesized form: `stat(_)`, `stat($file)`.
    /// Returns `(target, end_span)`.
    /// Parse a filetest expression given the test byte and the span
    /// of the leading `-X` tokens.  Shared between the Minus-triggered
    /// path and the explicit Filetest token arm.
    /// Build an Expr from readline content: `<>` is readline/ARGV,
    /// `<*.txt>` (with wildcards) is glob, otherwise `<FH>` is readline.
    fn readline_expr(content: String, safe: bool, span: Span) -> Result<Expr, ParseError> {
        if content.is_empty() {
            // `<>` (safe=false) or `<<>>` (safe=true).
            let name = if safe { "readline_safe" } else { "readline" };
            Ok(Expr { kind: ExprKind::FuncCall(name.into(), vec![]), span })
        } else if content.contains('*') || content.contains('?') {
            Ok(Expr { kind: ExprKind::FuncCall("glob".into(), vec![Expr { kind: ExprKind::StringLit(content), span }]), span })
        } else {
            Ok(Expr { kind: ExprKind::FuncCall("readline".into(), vec![Expr { kind: ExprKind::StringLit(content), span }]), span })
        }
    }

    fn parse_filetest(&mut self, test_byte: u8, span: Span) -> Result<Expr, ParseError> {
        let test_char = test_byte as char;
        // In autoquoting contexts (=> or }), treat as StringLit("-x")
        if matches!(self.peek_token(), Token::FatComma | Token::RightBrace) {
            return Ok(Expr { kind: ExprKind::StringLit(format!("-{test_char}")), span });
        }
        let (target, end) = self.parse_stat_target(span)?;
        Ok(Expr { span: span.merge(end), kind: ExprKind::Filetest(test_char, target) })
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
        // No argument: ;, }, ), EOF, or // (defined-or, not empty regex —
        // matches toke.c's FTST macro setting XTERMORDORDOR).
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
        Ok(Expr { span: span.merge(end), kind })
    }

    fn parse_list_op(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let name = (<&str>::from(kw)).to_string();

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
            return Ok(Expr { kind: ExprKind::ListOp(name, args), span: span.merge(end) });
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
        Ok(Expr { kind: ExprKind::ListOp(name, args), span: span.merge(end_span) })
    }

    /// Parse sort/map/grep with optional block as first argument.
    /// `sort { $a <=> $b } @list`, `map { ... } @list`, `grep { ... } @list`
    fn parse_block_list_op(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let name = (<&str>::from(kw)).to_string();

        // Check for parens: sort(...), map(...), grep(...)
        if self.at(&Token::LeftParen)? {
            self.next_token()?;
            let mut args = Vec::new();
            // Check for block as first arg inside parens
            if self.at(&Token::LeftBrace)? {
                let block = self.parse_block()?;
                args.push(Expr { span: block.span, kind: ExprKind::AnonSub(None, vec![], None, block) });
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
            return Ok(Expr { kind: ExprKind::ListOp(name, args), span: span.merge(end) });
        }

        let mut args = Vec::new();

        // Check for block or sub name as first arg
        if self.at(&Token::LeftBrace)? {
            let block = self.parse_block()?;
            args.push(Expr { span: block.span, kind: ExprKind::AnonSub(None, vec![], None, block) });
        } else if kw == Keyword::Sort {
            // sort can also take a sub name: sort subname @list
            if let Token::Ident(_) = self.peek_token() {
                let ident_span = self.peek_span();
                let ident = match self.next_token()?.token {
                    Token::Ident(s) => s,
                    _ => unreachable!(),
                };
                args.push(Expr { kind: ExprKind::Bareword(ident), span: ident_span });
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
        Ok(Expr { kind: ExprKind::ListOp(name, args), span: span.merge(end_span) })
    }

    /// Parse print/say with optional filehandle as first argument.
    /// `print STDERR "error"`, `print "hello"`, `say $fh "data"`
    fn parse_print_op(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let name = (<&str>::from(kw)).to_string();

        // Handle optional parens — print(...) form
        let in_parens = self.eat(&Token::LeftParen)?;

        // Try to detect filehandle before argument list.
        // Consume-then-decide: take the candidate token, peek at
        // what follows to determine if it's a filehandle or the
        // first argument.
        let mut filehandle: Option<Box<Expr>> = None;
        let mut first_arg: Option<Expr> = None;

        let is_bareword = matches!(self.peek_token(), Token::Ident(_));
        let is_scalar = matches!(self.peek_token(), Token::ScalarVar(_));

        if is_bareword {
            let fh_span = self.peek_span();
            let fh_name = match self.next_token()?.token {
                Token::Ident(n) => n,
                _ => unreachable!(),
            };
            if matches!(self.peek_token(), Token::Comma) {
                // Bareword followed by comma → first argument, not filehandle.
                // `print CONSTANT, "hello"`.
                let expr = self.with_descent(|this| {
                    let initial = this.parse_ident_term(fh_name, fh_span)?;
                    this.parse_expr_continuation(initial, PREC_COMMA + 1)
                })?;
                first_arg = Some(expr);
            } else {
                // Bareword not followed by comma → filehandle.
                // `print STDERR "hello"`.
                filehandle = Some(Box::new(Expr { kind: ExprKind::Bareword(fh_name), span: fh_span }));
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
                filehandle = Some(Box::new(Expr { kind: ExprKind::ScalarVar(var_name), span: var_span }));
            } else {
                // `print $x + 1` → not filehandle, first argument.
                let expr = self.with_descent(|this| {
                    let var_expr = Expr { kind: ExprKind::ScalarVar(var_name), span: var_span };
                    let initial = this.maybe_postfix_subscript(var_expr)?;
                    this.parse_expr_continuation(initial, PREC_COMMA + 1)
                })?;
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
                return Ok(Expr { kind: ExprKind::PrintOp(name, filehandle, args), span: span.merge(end_span) });
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
        Ok(Expr { kind: ExprKind::PrintOp(name, filehandle, args), span: span.merge(end_span) })
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

    /// Parse the operand of a prefix dereference ($$ref, @$ref, etc.).
    /// Consumes just the variable — subscripts are NOT included.
    /// This ensures $$ref[0] parses as ($$ref)[0], not $(${ref}[0]).
    fn parse_deref_operand(&mut self) -> Result<Expr, ParseError> {
        let spanned = self.next_token()?;
        let span = spanned.span;
        match spanned.token {
            Token::ScalarVar(name) => Ok(Expr { kind: ExprKind::ScalarVar(name), span }),
            Token::ArrayVar(name) => Ok(Expr { kind: ExprKind::ArrayVar(name), span }),
            Token::HashVar(name) => Ok(Expr { kind: ExprKind::HashVar(name), span }),
            Token::SpecialVar(name) => Ok(Expr { kind: ExprKind::SpecialVar(name), span }),
            Token::SpecialArrayVar(name) => Ok(Expr { kind: ExprKind::SpecialArrayVar(name), span }),
            Token::SpecialHashVar(name) => Ok(Expr { kind: ExprKind::SpecialHashVar(name), span }),
            // Recursive: $$$ref
            Token::Dollar => {
                let inner = self.parse_deref_operand()?;
                Ok(Expr { span: span.merge(inner.span), kind: ExprKind::Deref(Sigil::Scalar, Box::new(inner)) })
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
            Ok(Expr { span: callee.span.merge(end), kind: ExprKind::MethodCall(Box::new(callee), String::new(), args) })
        } else {
            Ok(callee)
        }
    }

    /// Parse the key expression inside `{ }` hash subscripts.
    /// Handles bareword autoquoting: `$hash{key}` → StringLit("key"),
    /// `$hash{-key}` → StringLit("-key").
    fn parse_hash_subscript_key(&mut self) -> Result<Expr, ParseError> {
        // Parser-driven bareword autoquoting for `$h{ident}`.
        //
        // The parser knows it's inside a hash-subscript body;
        // the lexer doesn't.  Per Perl, `q}foo}` at expression
        // position is a valid q-string, but `$h{q}` autoquotes
        // `q` to a string literal.  To make that context
        // distinction, call the lexer's try_autoquoted_bareword
        // API *before* any peek_token in this function — once
        // peek_token commits the lexer, it may have already
        // consumed `q}...}` as a q-string.
        //
        // Callers (maybe_postfix_subscript, the ArrowDeref
        // hash-elem branch) consume `{` via next_token before
        // calling here, so the parser's one-token cache is
        // empty at entry.  This is the only moment where the
        // raw source bytes past `{` haven't yet been touched
        // by the lexer.
        debug_assert!(self.current.is_none(), "parse_hash_subscript_key: one-token cache must be empty to try bareword lookahead");
        if let Some((name, span)) = self.lexer.try_autoquoted_bareword_subscript() {
            return Ok(Expr { kind: ExprKind::StringLit(name), span });
        }
        // Other autoquoting rules — bareword followed by `=>` or
        // `}` (when not a quote op), `-bareword` — are handled
        // inside parse_expr via parse_ident_term and the
        // Minus-prefix bareword path.
        self.parse_expr(PREC_LOW)
    }

    /// Check for `%hash{keys}` (kv hash slice) or `%array[indices]`
    /// (kv array slice) subscripts on a hash-sigil variable (5.20+).
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
            Ok(Expr { span: span.merge(end), kind: ExprKind::KvArraySlice(Box::new(recv), indices) })
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
            Ok(Expr { span: span.merge(end), kind: ExprKind::KvHashSlice(Box::new(recv), keys) })
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
                expr = Expr { span: expr.span.merge(end), kind: ExprKind::ArrayElem(Box::new(expr), Box::new(idx)) };
            } else if self.at(&Token::LeftBrace)? {
                self.next_token()?;
                let key = self.parse_hash_subscript_key()?;
                let end = self.peek_span();
                self.expect_token(&Token::RightBrace)?;
                expr = Expr { span: expr.span.merge(end), kind: ExprKind::HashElem(Box::new(expr), Box::new(key)) };
            } else {
                break;
            }
        }
        Ok(expr)
    }

    // ── Interpolated string assembly ──────────────────────────

    /// Collect sub-tokens after a `QuoteSublexBegin`/`RegexSublexBegin`/`SubstSublexBegin`
    /// into an `Interpolated`.  The caller decides how to wrap it
    /// in an AST node.
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
                Token::InterpScalar(name) => {
                    let span = self.peek_span();
                    self.next_token()?;
                    let cm = self.lexer.take_interp_case_mod();
                    let expr = apply_case_mod_wrap(Expr { kind: ExprKind::ScalarVar(name), span }, cm);
                    parts.push(InterpPart::ScalarInterp(Box::new(expr)));
                }
                Token::InterpArray(name) => {
                    let span = self.peek_span();
                    self.next_token()?;
                    let cm = self.lexer.take_interp_case_mod();
                    let expr = apply_case_mod_wrap(Expr { kind: ExprKind::ArrayVar(name), span }, cm);
                    parts.push(InterpPart::ArrayInterp(Box::new(expr)));
                }
                Token::InterpScalarChainStart(name) => {
                    let span = self.peek_span();
                    self.next_token()?;
                    let cm = self.lexer.take_interp_case_mod();
                    let initial = Expr { kind: ExprKind::ScalarVar(name), span };
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
                    let recv = Expr { kind: ExprKind::ArrayVar(name), span };
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
                        Expr { span: span.merge(end), kind: ExprKind::ArraySlice(Box::new(recv), indices) }
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
                        Expr { span: span.merge(end), kind: ExprKind::HashSlice(Box::new(recv), keys) }
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

    /// Parse an interpolated string body into an Expr.
    /// Returns `StringLit` for plain strings, `InterpolatedString` for
    /// strings with interpolation.
    fn parse_interpolated_string(&mut self, span: Span) -> Result<Expr, ParseError> {
        let interp = self.parse_interpolated()?;
        Ok(interp_to_expr(interp, span))
    }

    // ── Operator parsing ──────────────────────────────────────

    fn peek_op_info(&mut self) -> Option<OpInfo> {
        // Snapshot feature bits we may consult before the match
        // on self.peek_token() — that call takes a mutable borrow
        // of self, which we can't hold across further field access.
        let isa_active = self.pragmas.features.contains(Features::ISA);
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
            Token::NumEq | Token::NumNe | Token::Spaceship => Some(OpInfo { prec: PREC_EQ, assoc: Assoc::Non }),
            Token::SmartMatch if smartmatch_active => Some(OpInfo { prec: PREC_EQ, assoc: Assoc::Non }),
            Token::NumLt | Token::NumGt | Token::NumLe | Token::NumGe => Some(OpInfo { prec: PREC_REL, assoc: Assoc::Non }),
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
            // Word operators (always emitted as Keyword tokens):
            // eq/ne/cmp at == precedence, lt/gt/le/ge at relational,
            // x at multiplicative, xor at low-logical.
            Token::Keyword(Keyword::Eq) | Token::Keyword(Keyword::Ne) | Token::Keyword(Keyword::Cmp) => Some(OpInfo { prec: PREC_EQ, assoc: Assoc::Non }),
            Token::Keyword(Keyword::Lt) | Token::Keyword(Keyword::Gt) | Token::Keyword(Keyword::Le) | Token::Keyword(Keyword::Ge) => {
                Some(OpInfo { prec: PREC_REL, assoc: Assoc::Non })
            }
            Token::Ident(name) => match name.as_str() {
                "x" => Some(OpInfo { prec: PREC_MUL, assoc: Assoc::Left }),
                "xor" => Some(OpInfo { prec: PREC_OR_LOW, assoc: Assoc::Left }),
                // `isa` is feature-gated: treated as an infix
                // operator only when the `isa` feature is active.
                // Without it, it's an ordinary bareword.
                "isa" if isa_active => Some(OpInfo { prec: PREC_ISA, assoc: Assoc::Non }),
                _ => None,
            },
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
            ExprKind::List(items) => items.iter().all(Self::is_valid_lvalue),
            ExprKind::Undef => true, // (undef, $x) = (1, 2)
            _ => false,
        }
    }

    /// Is `expr` a valid left-hand side of an aliasing assignment,
    /// per the `refaliasing` feature?
    ///
    /// Accepts `\$x`, `\@a`, `\%h`, `\&f`, `\*g`, parenthesized
    /// forms, and lists of those (including `my`-declarations that
    /// themselves contain ref-wrapped variables).  The base
    /// lvalues (ScalarVar, ArrayVar, etc.) are NOT accepted here —
    /// plain `$x = 1` goes through `is_valid_lvalue` alone.
    fn is_ref_alias_target(expr: &Expr) -> bool {
        match &expr.kind {
            // The canonical aliasing form: `\<variable>`.
            ExprKind::Ref(inner) => Self::is_valid_lvalue(inner),
            // Parenthesized aliasing: `(\$x) = ...`.
            ExprKind::Paren(inner) => Self::is_ref_alias_target(inner),
            // List: `(\$x, \@y) = ...`.  Mixed lists (some ref,
            // some not) are syntactically valid — semantics is
            // a later concern.
            ExprKind::List(items) => items.iter().any(Self::is_ref_alias_target),
            // `my \$x = ...` — the Decl already carries the
            // `is_ref` flag on each VarDecl.
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
                Ok(Expr { span: left.span.merge(op_spanned.span), kind: ExprKind::PostfixOp(PostfixOp::Inc, Box::new(left)) })
            }
            Token::MinusMinus => {
                if !Self::is_valid_lvalue(&left) {
                    return Err(ParseError::new("invalid operand for postfix --", left.span));
                }
                Ok(Expr { span: left.span.merge(op_spanned.span), kind: ExprKind::PostfixOp(PostfixOp::Dec, Box::new(left)) })
            }

            // Ternary
            Token::Question => {
                let then_expr = self.parse_expr(PREC_LOW)?;
                self.expect_token(&Token::Colon)?;
                let else_expr = self.parse_expr(right_prec)?;
                Ok(Expr { span: left.span.merge(else_expr.span), kind: ExprKind::Ternary(Box::new(left), Box::new(then_expr), Box::new(else_expr)) })
            }

            // Arrow
            Token::Arrow => self.parse_arrow_rhs(left),

            // Assignment
            Token::Assign(op) => {
                // `refaliasing` (5.22+) extends lvalue-ness to
                // include `\$x`, `\@a`, `\%h`, and lists of those.
                // We accept `Ref(...)` as an assignment target
                // only when the feature is active; the
                // ++/-- lvalue checks below are unaffected.
                let refalias_ok = self.pragmas.features.contains(Features::REFALIASING) && Self::is_ref_alias_target(&left);
                if !Self::is_valid_lvalue(&left) && !refalias_ok {
                    return Err(ParseError::new("invalid assignment target", left.span));
                }
                let right = self.parse_expr(right_prec)?;
                Ok(Expr { span: left.span.merge(right.span), kind: ExprKind::Assign(op, Box::new(left), Box::new(right)) })
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
                    ExprKind::List(items) => items,
                    _ => vec![left],
                };
                match right.kind {
                    ExprKind::List(more) => items.extend(more),
                    _ => items.push(right),
                };
                let span = match (items.first(), items.last()) {
                    (Some(f), Some(l)) => f.span.merge(l.span),
                    _ => Span::new(0, 0),
                };
                Ok(Expr { kind: ExprKind::List(items), span })
            }

            // Range
            Token::DotDot => {
                let right = self.parse_expr(right_prec)?;
                Ok(Expr { span: left.span.merge(right.span), kind: ExprKind::Range(Box::new(left), Box::new(right)) })
            }
            Token::DotDotDot => {
                let right = self.parse_expr(right_prec)?;
                Ok(Expr { span: left.span.merge(right.span), kind: ExprKind::FlipFlop(Box::new(left), Box::new(right)) })
            }

            // Binary operators
            token => {
                let binop = token_to_binop(&token)?;
                let right = self.parse_expr(right_prec)?;

                // Chained comparisons: `$x < $y <= $z` produces
                // ChainedCmp([<, <=], [x, y, z]).  Only operators
                // in the same chain group can chain together.
                let group = chain_group(&binop);
                if group.is_some() && self.peek_chain_continues(group) {
                    let mut ops = vec![binop];
                    let start_span = left.span;
                    let mut operands = vec![left, right];
                    while self.peek_chain_continues(group) {
                        let next_tok = self.next_token()?;
                        let next_op = token_to_binop(&next_tok.token)?;
                        ops.push(next_op);
                        operands.push(self.parse_expr(right_prec)?);
                    }
                    let end_span = operands.last().map_or(start_span, |e| e.span);
                    let span = start_span.merge(end_span);
                    Ok(Expr { span, kind: ExprKind::ChainedCmp(ops, operands) })
                } else {
                    Ok(Expr { span: left.span.merge(right.span), kind: ExprKind::BinOp(binop, Box::new(left), Box::new(right)) })
                }
            }
        }
    }

    /// Check whether the next token continues a chained comparison
    /// in the given chain group.
    fn peek_chain_continues(&mut self, group: Option<u8>) -> bool {
        group.is_some() && token_chain_group(self.peek_token()) == group
    }

    fn parse_arrow_rhs(&mut self, left: Expr) -> Result<Expr, ParseError> {
        // After ->, identifiers (including what would otherwise be
        // keywords like 'keys', 'values', 'print') are method names.
        // Convert Keyword tokens to their name string.
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
                return Ok(Expr { span: left.span.merge(end), kind: ExprKind::MethodCall(Box::new(left), name, args) });
            } else {
                // Bare method call with no parens
                return Ok(Expr { span: left.span.merge(self.peek_span()), kind: ExprKind::MethodCall(Box::new(left), name, vec![]) });
            }
        }
        match self.peek_token().clone() {
            Token::LeftBracket => {
                self.next_token()?;
                let idx = self.parse_expr(PREC_LOW)?;
                let end = self.peek_span();
                self.expect_token(&Token::RightBracket)?;
                let expr = Expr { span: left.span.merge(end), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::ArrayElem(Box::new(idx))) };
                // Handle chained subscripts: $ref->[0][1], $ref->[0]{key}
                self.maybe_postfix_subscript(expr)
            }
            Token::LeftBrace => {
                self.next_token()?;
                let key = self.parse_hash_subscript_key()?;
                let end = self.peek_span();
                self.expect_token(&Token::RightBrace)?;
                let expr = Expr { span: left.span.merge(end), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::HashElem(Box::new(key))) };
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
                Ok(Expr { span: left.span.merge(end), kind: ExprKind::MethodCall(Box::new(left), String::new(), args) })
            }
            // Dynamic method dispatch: ->$method or ->$method(args)
            Token::ScalarVar(var_name) => {
                let var_span = self.peek_span();
                self.next_token()?;
                let method_expr = Expr { kind: ExprKind::ScalarVar(var_name), span: var_span };
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
                    Ok(Expr { span: left.span.merge(end), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::DynMethod(Box::new(method_expr), args)) })
                } else {
                    Ok(Expr {
                        span: left.span.merge(var_span),
                        kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::DynMethod(Box::new(method_expr), vec![])),
                    })
                }
            }
            // Postfix dereference: ->@*, ->%*, ->$*, ->&*, ->**,
            // plus slice forms ->@[...], ->@{...}, ->%[...], ->%{...}.
            //
            // The trailing `*` forms are whole-container derefs.
            // The `[...]` and `{...}` forms after `@` or `%`
            // produce slices (array of values or kv list).
            Token::At => {
                self.next_token()?;
                if self.eat(&Token::Star)? {
                    Ok(Expr { span: left.span.merge(self.peek_span()), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::DerefArray) })
                } else if self.at(&Token::LeftBracket)? {
                    self.next_token()?;
                    let idx = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBracket)?;
                    Ok(Expr { span: left.span.merge(end), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::ArraySliceIndices(Box::new(idx))) })
                } else if self.at(&Token::LeftBrace)? {
                    self.next_token()?;
                    let key = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBrace)?;
                    Ok(Expr { span: left.span.merge(end), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::ArraySliceKeys(Box::new(key))) })
                } else {
                    Err(ParseError::new("expected *, [indices], or {keys} after ->@", self.peek_span()))
                }
            }
            Token::Dollar => {
                self.next_token()?;
                // `->$#*` — postderef last-index.  The lexer
                // would otherwise tokenize the `#` as a comment
                // start, so we peek+consume the two raw bytes
                // here before the next token is lexed.
                if self.lexer.try_consume_hash_star() {
                    Ok(Expr { span: left.span.merge(self.peek_span()), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::LastIndex) })
                } else if self.eat(&Token::Star)? {
                    Ok(Expr { span: left.span.merge(self.peek_span()), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::DerefScalar) })
                } else {
                    Err(ParseError::new("expected * or #* after ->$", self.peek_span()))
                }
            }
            Token::Percent => {
                self.next_token()?;
                if self.eat(&Token::Star)? {
                    Ok(Expr { span: left.span.merge(self.peek_span()), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::DerefHash) })
                } else if self.at(&Token::LeftBracket)? {
                    self.next_token()?;
                    let idx = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBracket)?;
                    Ok(Expr { span: left.span.merge(end), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::KvSliceIndices(Box::new(idx))) })
                } else if self.at(&Token::LeftBrace)? {
                    self.next_token()?;
                    let key = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightBrace)?;
                    Ok(Expr { span: left.span.merge(end), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::KvSliceKeys(Box::new(key))) })
                } else {
                    Err(ParseError::new("expected *, [indices], or {keys} after ->%", self.peek_span()))
                }
            }
            // `->&*` — code-ref postfix deref.
            // `->&method` or `->&method(args)` — lexical method
            // invocation (resolved at compile time, not via package
            // inheritance).  The `&` prefix in the name signals
            // lexical resolution to the compiler.
            Token::BitAnd => {
                self.next_token()?;
                if self.eat(&Token::Star)? {
                    Ok(Expr { span: left.span.merge(self.peek_span()), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::DerefCode) })
                } else {
                    // Lexical method: ->&name or ->&name(args)
                    let method_name = match self.peek_token().clone() {
                        Token::Ident(name) => name,
                        Token::Keyword(kw) => (<&str>::from(kw)).to_string(),
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
                        Ok(Expr { span: left.span.merge(end), kind: ExprKind::MethodCall(Box::new(left), name, args) })
                    } else {
                        Ok(Expr { span: left.span.merge(self.peek_span()), kind: ExprKind::MethodCall(Box::new(left), name, vec![]) })
                    }
                }
            }
            // `->**` — glob deref.  Two consecutive `*`s; the
            // lexer emits `Power` (`**`) for that pair.
            Token::Power => {
                self.next_token()?;
                Ok(Expr { span: left.span.merge(self.peek_span()), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::DerefGlob) })
            }
            other => Err(ParseError::new(format!("expected method name or subscript after ->, got {other:?}"), self.peek_span())),
        }
    }
}

// ── Pragma application helpers ────────────────────────────────

/// Apply the side effects of a `use` or `no` statement whose
/// module name is a known pragma.  Unknown modules are ignored.
fn apply_pragma(pragmas: &mut Pragmas, module: &str, is_no: bool, imports: Option<&Vec<Expr>>) {
    match module {
        "feature" => {
            // Arguments are feature or bundle names as string
            // literals, barewords, or a qw(...) list.  Bundle
            // aliases (`:all`, `:default`, `:5.36`) are handled
            // by resolve_feature_name.
            match imports {
                Some(items) if !items.is_empty() => {
                    for item in items {
                        for name in expr_to_pragma_strings(item) {
                            if let Some(feats) = resolve_feature_name(&name) {
                                // A bundle name evaluates to a set
                                // of features; individual names
                                // evaluate to single-bit sets.
                                // Either way, OR/AND-NOT works.
                                if is_no {
                                    pragmas.features.remove(feats);
                                } else {
                                    // `use feature ':5.36'` resets
                                    // to the bundle rather than
                                    // ORing with prior state.  We
                                    // detect that by checking for
                                    // a leading colon in the name
                                    // (same test the resolver uses).
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
                    // Per perlfeature: "no feature" with no args
                    // resets to :default; "use feature" with no
                    // args is a no-op.
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
            // Any other pragma (strict, warnings, integer, ...) or
            // module import is not yet parser-relevant.
        }
    }
}

/// Extract one or more pragma argument names from an AST
/// expression.  `use feature 'say'` yields a StringLit (one name);
/// `use feature qw(say state)` yields a QwList (multiple names);
/// barewords are historically allowed too.  Returns an empty vec
/// for anything else.
fn expr_to_pragma_strings(expr: &Expr) -> Vec<String> {
    match &expr.kind {
        ExprKind::StringLit(s) => vec![s.clone()],
        ExprKind::Bareword(s) => vec![s.clone()],
        ExprKind::QwList(words) => words.clone(),
        _ => Vec::new(),
    }
}

/// Parse a `use 5036` / `use 5.036` integer as a (major, minor) pair.
/// The integer form is interpreted as `5_036` = major 5, minor 36
/// (last three digits = minor).
fn parse_int_version(n: i64) -> Option<(u32, u32)> {
    if n <= 0 {
        return None;
    }
    let n = n as u64;
    // Historically, `use 5008001;` → 5.008.001 (major.minor.patch).
    // For phase 1 we handle the common pattern `5NNN` where NNN is
    // the minor version, which matches `use 5036`.
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

/// Parse a v-string literal like `"v5.36"` or `"v5.36.0"` as a
/// (major, minor) pair.  Only the first two components are used.
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
        Token::Keyword(Keyword::Eq) => Ok(BinOp::StrEq),
        Token::Keyword(Keyword::Ne) => Ok(BinOp::StrNe),
        Token::Keyword(Keyword::Lt) => Ok(BinOp::StrLt),
        Token::Keyword(Keyword::Gt) => Ok(BinOp::StrGt),
        Token::Keyword(Keyword::Le) => Ok(BinOp::StrLe),
        Token::Keyword(Keyword::Ge) => Ok(BinOp::StrGe),
        Token::Keyword(Keyword::Cmp) => Ok(BinOp::StrCmp),
        Token::Ident(name) => match name.as_str() {
            "x" => Ok(BinOp::Repeat),
            "xor" => Ok(BinOp::LowXor),
            // Only reaches here when op_info already gated on the
            // `isa` feature, so no feature check needed.
            "isa" => Ok(BinOp::Isa),
            _ => Err(ParseError::new(format!("not a binary operator: {token:?}"), Span::DUMMY)),
        },
        other => Err(ParseError::new(format!("not a binary operator: {other:?}"), Span::DUMMY)),
    }
}

/// Returns a chain-group identifier for operators that participate
/// in chained comparisons, or `None` for non-chainable ops.
///
/// Group 1: relational (`< > <= >= lt gt le ge`) — chain with each other.
/// Group 2: equality (`== != eq ne`) — chain with each other.
/// `<=>`, `cmp`, and `~~` are non-associative and do NOT chain.
fn chain_group(op: &BinOp) -> Option<u8> {
    match op {
        BinOp::NumLt | BinOp::NumGt | BinOp::NumLe | BinOp::NumGe | BinOp::StrLt | BinOp::StrGt | BinOp::StrLe | BinOp::StrGe => Some(1),
        BinOp::NumEq | BinOp::NumNe | BinOp::StrEq | BinOp::StrNe => Some(2),
        _ => None,
    }
}

/// Check whether a token would produce a chainable BinOp in the
/// given chain group.
fn token_chain_group(token: &Token) -> Option<u8> {
    match token {
        Token::NumLt | Token::NumGt | Token::NumLe | Token::NumGe => Some(1),
        Token::Keyword(Keyword::Lt) | Token::Keyword(Keyword::Gt) | Token::Keyword(Keyword::Le) | Token::Keyword(Keyword::Ge) => Some(1),
        Token::NumEq | Token::NumNe => Some(2),
        Token::Keyword(Keyword::Eq) | Token::Keyword(Keyword::Ne) => Some(2),
        _ => None,
    }
}

/// Wrap an interpolated expression in case-modification function
/// calls based on the active `CaseMod` flags.
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
        expr = Expr { kind: ExprKind::FuncCall("lcfirst".into(), vec![expr]), span };
    } else if flags.contains(CaseMod::UCFIRST) {
        expr = Expr { kind: ExprKind::FuncCall("ucfirst".into(), vec![expr]), span };
    } else if flags.contains(CaseMod::UPPER) {
        expr = Expr { kind: ExprKind::FuncCall("uc".into(), vec![expr]), span };
    } else if flags.contains(CaseMod::LOWER) || flags.contains(CaseMod::FOLD) {
        expr = Expr { kind: ExprKind::FuncCall("lc".into(), vec![expr]), span };
    }

    // Quotemeta wraps outermost (applied last, after case).
    if flags.contains(CaseMod::QUOTEMETA) {
        expr = Expr { kind: ExprKind::FuncCall("quotemeta".into(), vec![expr]), span };
    }

    expr
}

/// Merge adjacent `Const` segments in an interpolated value.
fn merge_interp_parts(parts: Vec<InterpPart>) -> Vec<InterpPart> {
    let mut merged: Vec<InterpPart> = Vec::new();
    for part in parts {
        // Skip empty constant segments (produced when a case-mod
        // escape like `\l` immediately precedes an interpolation
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

/// Convert an `Interpolated` into an `Expr`.
/// Returns `StringLit` for plain strings, `InterpolatedString` otherwise.
fn interp_to_expr(interp: Interpolated, span: Span) -> Expr {
    if let Some(s) = interp.as_plain_string() { Expr { kind: ExprKind::StringLit(s), span } } else { Expr { kind: ExprKind::InterpolatedString(interp), span } }
}
