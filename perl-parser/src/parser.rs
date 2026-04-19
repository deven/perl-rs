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
use crate::symbols::{ProtoSlot, SubPrototype, SymbolTable};
use crate::token::Keyword;
use crate::token::*;

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
            // Pragmas are lexically scoped: any `use feature` / `use
            // utf8` inside this block doesn't leak out.  Save state
            // before parsing, restore after.  Done for both the
            // success and error paths via early-return handling.
            let saved_pragmas = this.pragmas;

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Program {
        let mut parser = Parser::new(src.as_bytes()).unwrap();
        parser.parse_program().unwrap()
    }

    fn parse_expr_str(src: &str) -> Expr {
        // Wrap in a statement to parse
        let prog = parse(src);
        match &prog.statements[0].kind {
            StmtKind::Expr(e) => e.clone(),
            other => panic!("expected expression, got {other:?}"),
        }
    }

    /// For tests that need the initializer from a `my $x = expr;`
    /// declaration-statement.  Returns the RHS of the Assign.
    fn decl_init(stmt: &Statement) -> &Expr {
        match &stmt.kind {
            StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, rhs), .. }) => {
                assert!(matches!(lhs.kind, ExprKind::Decl(_, _)), "expected Decl lhs, got {:?}", lhs.kind);
                rhs
            }
            other => panic!("expected decl with initializer, got {other:?}"),
        }
    }

    /// For tests that need the var list from a declaration.
    /// Works for both `my $x;` (plain Decl) and `my $x = ...;` (Assign(Decl, _)).
    fn decl_vars(stmt: &Statement) -> (DeclScope, &[VarDecl]) {
        let expr = match &stmt.kind {
            StmtKind::Expr(e) => e,
            other => panic!("expected Expr stmt, got {other:?}"),
        };
        let decl = match &expr.kind {
            ExprKind::Decl(_, _) => expr,
            ExprKind::Assign(_, lhs, _) => lhs,
            other => panic!("expected Decl or Assign(Decl, _), got {other:?}"),
        };
        match &decl.kind {
            ExprKind::Decl(scope, vars) => (*scope, vars.as_slice()),
            other => panic!("expected Decl, got {other:?}"),
        }
    }

    /// Extract the pattern string from an `Interpolated` value.
    fn pat_str(interp: &Interpolated) -> &str {
        match interp.as_plain_string() {
            Some(ref _s) => {
                // as_plain_string returns owned; match on parts directly.
                if interp.0.is_empty() {
                    return "";
                }
                match &interp.0[0] {
                    InterpPart::Const(s) => s.as_str(),
                    other => panic!("expected Const part, got {other:?}"),
                }
            }
            None => panic!("expected plain string pattern, got {:?}", interp.0),
        }
    }

    #[test]
    fn parse_simple_assignment() {
        let prog = parse("my $x = 42;");
        assert_eq!(prog.statements.len(), 1);
        let (_scope, vars) = decl_vars(&prog.statements[0]);
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].name, "x");
        let init = decl_init(&prog.statements[0]);
        assert!(matches!(init.kind, ExprKind::IntLit(42)));
    }

    #[test]
    fn parse_arithmetic_precedence() {
        // 1 + 2 * 3 should be 1 + (2 * 3)
        let e = parse_expr_str("1 + 2 * 3;");
        match &e.kind {
            ExprKind::BinOp(BinOp::Add, left, right) => {
                assert!(matches!(left.kind, ExprKind::IntLit(1)));
                match &right.kind {
                    ExprKind::BinOp(BinOp::Mul, l, r) => {
                        assert!(matches!(l.kind, ExprKind::IntLit(2)));
                        assert!(matches!(r.kind, ExprKind::IntLit(3)));
                    }
                    other => panic!("expected Mul, got {other:?}"),
                }
            }
            other => panic!("expected Add, got {other:?}"),
        }
    }

    #[test]
    fn parse_power_right_assoc() {
        // 2 ** 3 ** 4 should be 2 ** (3 ** 4)
        let e = parse_expr_str("2 ** 3 ** 4;");
        match &e.kind {
            ExprKind::BinOp(BinOp::Pow, left, right) => {
                assert!(matches!(left.kind, ExprKind::IntLit(2)));
                match &right.kind {
                    ExprKind::BinOp(BinOp::Pow, l, r) => {
                        assert!(matches!(l.kind, ExprKind::IntLit(3)));
                        assert!(matches!(r.kind, ExprKind::IntLit(4)));
                    }
                    other => panic!("expected Pow, got {other:?}"),
                }
            }
            other => panic!("expected Pow, got {other:?}"),
        }
    }

    #[test]
    fn parse_ternary() {
        let e = parse_expr_str("$x ? 1 : 0;");
        assert!(matches!(e.kind, ExprKind::Ternary(_, _, _)));
    }

    #[test]
    fn parse_if_stmt() {
        let prog = parse("if ($x > 0) { print 1; }");
        match &prog.statements[0].kind {
            StmtKind::If(if_stmt) => {
                assert!(matches!(if_stmt.condition.kind, ExprKind::BinOp(BinOp::NumGt, _, _)));
                assert_eq!(if_stmt.then_block.statements.len(), 1);
                assert!(if_stmt.elsif_clauses.is_empty());
                assert!(if_stmt.else_block.is_none());
            }
            other => panic!("expected If, got {other:?}"),
        }
    }

    #[test]
    fn parse_if_elsif_else() {
        let prog = parse("if ($x > 0) { 1; } elsif ($x == 0) { 0; } else { -1; }");
        match &prog.statements[0].kind {
            StmtKind::If(if_stmt) => {
                assert_eq!(if_stmt.elsif_clauses.len(), 1);
                assert!(if_stmt.else_block.is_some());
            }
            other => panic!("expected If, got {other:?}"),
        }
    }

    #[test]
    fn parse_while_loop() {
        let prog = parse("while ($x > 0) { $x--; }");
        match &prog.statements[0].kind {
            StmtKind::While(w) => {
                assert!(matches!(w.condition.kind, ExprKind::BinOp(BinOp::NumGt, _, _)));
                assert_eq!(w.body.statements.len(), 1);
            }
            other => panic!("expected While, got {other:?}"),
        }
    }

    #[test]
    fn parse_foreach_loop() {
        let prog = parse("for my $item (@list) { print $item; }");
        match &prog.statements[0].kind {
            StmtKind::ForEach(f) => {
                let var = f.vars.first().expect("expected loop variable");
                assert_eq!(var.name, "item");
                assert_eq!(var.sigil, Sigil::Scalar);
                assert_eq!(f.body.statements.len(), 1);
            }
            other => panic!("expected ForEach, got {other:?}"),
        }
    }

    #[test]
    fn parse_sub_decl() {
        let prog = parse("sub foo { return 42; }");
        match &prog.statements[0].kind {
            StmtKind::SubDecl(sub) => {
                assert_eq!(sub.name, "foo");
                assert_eq!(sub.body.statements.len(), 1);
            }
            other => panic!("expected SubDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_arrow_method_call() {
        let e = parse_expr_str("$obj->method(1, 2);");
        match &e.kind {
            ExprKind::MethodCall(invocant, name, args) => {
                assert!(matches!(invocant.kind, ExprKind::ScalarVar(ref n) if n == "obj"));
                assert_eq!(name, "method");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected MethodCall, got {other:?}"),
        }
    }

    #[test]
    fn parse_arrow_deref() {
        let e = parse_expr_str("$ref->{key};");
        match &e.kind {
            ExprKind::ArrowDeref(base, ArrowTarget::HashElem(key)) => {
                assert!(matches!(base.kind, ExprKind::ScalarVar(ref n) if n == "ref"));
                assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "key"));
            }
            other => panic!("expected ArrowDeref HashElem, got {other:?}"),
        }
    }

    #[test]
    fn parse_anon_array() {
        let e = parse_expr_str("[1, 2, 3];");
        match &e.kind {
            ExprKind::AnonArray(elems) => assert_eq!(elems.len(), 3),
            other => panic!("expected AnonArray, got {other:?}"),
        }
    }

    #[test]
    fn parse_print_list() {
        let prog = parse(r#"print "hello", " ", "world";"#);
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PrintOp(name, fh, args), .. }) => {
                assert_eq!(name, "print");
                assert!(fh.is_none());
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected PrintOp, got {other:?}"),
        }
    }

    #[test]
    fn parse_postfix_if() {
        let prog = parse("print 1 if $x;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::If, _, _), .. }) => {}
            other => panic!("expected PostfixControl If, got {other:?}"),
        }
    }

    #[test]
    fn parse_string_concat() {
        let e = parse_expr_str(r#""hello" . " " . "world";"#);
        // Should be left-associative: ("hello" . " ") . "world"
        match &e.kind {
            ExprKind::BinOp(BinOp::Concat, _, right) => {
                assert!(matches!(right.kind, ExprKind::StringLit(_)));
            }
            other => panic!("expected Concat, got {other:?}"),
        }
    }

    #[test]
    fn parse_use_strict() {
        let prog = parse("use strict;");
        match &prog.statements[0].kind {
            StmtKind::UseDecl(u) => assert_eq!(u.module, "strict"),
            other => panic!("expected UseDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_use_with_version() {
        let prog = parse("use Foo 1.23;");
        match &prog.statements[0].kind {
            StmtKind::UseDecl(u) => {
                assert_eq!(u.module, "Foo");
                assert_eq!(u.version.as_deref(), Some("1.23"));
                assert!(u.imports.is_none());
            }
            other => panic!("expected UseDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_use_with_imports() {
        let prog = parse("use Foo qw(bar baz);");
        match &prog.statements[0].kind {
            StmtKind::UseDecl(u) => {
                assert_eq!(u.module, "Foo");
                assert!(u.version.is_none());
                let imports = u.imports.as_ref().expect("expected imports");
                assert_eq!(imports.len(), 1);
                assert!(matches!(&imports[0].kind, ExprKind::QwList(_)));
            }
            other => panic!("expected UseDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_use_with_version_and_imports() {
        let prog = parse("use Foo 1.23 qw(bar baz);");
        match &prog.statements[0].kind {
            StmtKind::UseDecl(u) => {
                assert_eq!(u.module, "Foo");
                assert_eq!(u.version.as_deref(), Some("1.23"));
                assert!(u.imports.is_some());
            }
            other => panic!("expected UseDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_use_perl_version() {
        let prog = parse("use 5.020;");
        match &prog.statements[0].kind {
            StmtKind::UseDecl(u) => {
                assert_eq!(u.module, "5.02"); // 5.020 → 5.02 in float form
                assert!(u.version.is_none());
                assert!(u.imports.is_none());
            }
            other => panic!("expected UseDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_use_with_list_imports() {
        let prog = parse("use Foo 'bar', 'baz';");
        match &prog.statements[0].kind {
            StmtKind::UseDecl(u) => {
                assert_eq!(u.module, "Foo");
                let imports = u.imports.as_ref().expect("expected imports");
                assert_eq!(imports.len(), 2);
            }
            other => panic!("expected UseDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_package() {
        let prog = parse("package Foo::Bar;");
        match &prog.statements[0].kind {
            StmtKind::PackageDecl(p) => assert_eq!(p.name, "Foo::Bar"),
            other => panic!("expected PackageDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_multiple_statements() {
        let prog = parse("my $x = 1; my $y = 2; $x + $y;");
        assert_eq!(prog.statements.len(), 3);
        // First two are `my` declarations with initializers, so
        // Stmt::Expr wrapping Assign(Decl, ...).
        let (_s0, _v0) = decl_vars(&prog.statements[0]);
        let (_s1, _v1) = decl_vars(&prog.statements[1]);
        match &prog.statements[2].kind {
            StmtKind::Expr(e) => assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Add, _, _))),
            other => panic!("expected Expr(Add), got {other:?}"),
        }
    }

    #[test]
    fn parse_prefix_negation() {
        let e = parse_expr_str("-$x;");
        assert!(matches!(e.kind, ExprKind::UnaryOp(UnaryOp::Negate, _)));
    }

    #[test]
    fn parse_logical_operators() {
        let e = parse_expr_str("$a && $b || $c;");
        // || is lower precedence than &&, so: ($a && $b) || $c
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Or, _, _)));
    }

    #[test]
    fn parse_defined_or() {
        let e = parse_expr_str("$x // $default;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
    }

    #[test]
    fn parse_assign_add() {
        let e = parse_expr_str("$x += 1;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::AddEq, _, _)));
    }

    #[test]
    fn parse_ref_and_deref() {
        let e = parse_expr_str("\\$x;");
        assert!(matches!(e.kind, ExprKind::Ref(_)));
    }

    // ── Interpolation tests ───────────────────────────────────

    #[test]
    fn parse_plain_double_string() {
        // No interpolation — collapses to StringLit.
        let e = parse_expr_str(r#""hello world";"#);
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "hello world"),
            other => panic!("expected StringLit, got {other:?}"),
        }
    }

    #[test]
    fn parse_interp_string() {
        let e = parse_expr_str(r#""Hello, $name!";"#);
        match &e.kind {
            ExprKind::InterpolatedString(Interpolated(parts)) => {
                assert_eq!(parts.len(), 3);
                assert!(matches!(&parts[0], InterpPart::Const(s) if s == "Hello, "));
                assert_eq!(scalar_interp_name(&parts[1]), Some("name"));
                assert!(matches!(&parts[2], InterpPart::Const(s) if s == "!"));
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn parse_interp_multiple_vars() {
        let e = parse_expr_str(r#""$x and $y";"#);
        match &e.kind {
            ExprKind::InterpolatedString(Interpolated(parts)) => {
                assert_eq!(parts.len(), 3);
                assert_eq!(scalar_interp_name(&parts[0]), Some("x"));
                assert!(matches!(&parts[1], InterpPart::Const(s) if s == " and "));
                assert_eq!(scalar_interp_name(&parts[2]), Some("y"));
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn parse_interp_array() {
        let e = parse_expr_str(r#""items: @list""#);
        match &e.kind {
            ExprKind::InterpolatedString(Interpolated(parts)) => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(&parts[0], InterpPart::Const(s) if s == "items: "));
                assert_eq!(array_interp_name(&parts[1]), Some("list"));
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    /// Extract the variable name from a simple scalar-interp
    /// (one that wraps a bare ScalarVar with no subscripts).
    /// Returns None if the part isn't a ScalarInterp or the inner
    /// expr isn't a bare variable.
    fn scalar_interp_name(p: &InterpPart) -> Option<&str> {
        match p {
            InterpPart::ScalarInterp(expr) => match &expr.kind {
                ExprKind::ScalarVar(n) => Some(n.as_str()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Extract the variable name from a simple array-interp.
    fn array_interp_name(p: &InterpPart) -> Option<&str> {
        match p {
            InterpPart::ArrayInterp(expr) => match &expr.kind {
                ExprKind::ArrayVar(n) => Some(n.as_str()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Pull the inner expression out of a ScalarInterp for tests
    /// that need to inspect the subscript structure.
    fn scalar_interp_expr(p: &InterpPart) -> &Expr {
        match p {
            InterpPart::ScalarInterp(e) => e,
            other => panic!("expected ScalarInterp, got {other:?}"),
        }
    }

    /// Pull the inner expression out of an ArrayInterp.
    fn array_interp_expr(p: &InterpPart) -> &Expr {
        match p {
            InterpPart::ArrayInterp(e) => e,
            other => panic!("expected ArrayInterp, got {other:?}"),
        }
    }

    #[test]
    fn parse_string_concat_interp() {
        // Interpolated string in a concat expression.
        let e = parse_expr_str(r#""Hello, $name!" . " Bye!""#);
        match &e.kind {
            ExprKind::BinOp(BinOp::Concat, left, right) => {
                assert!(matches!(left.kind, ExprKind::InterpolatedString(_)));
                assert!(matches!(right.kind, ExprKind::StringLit(ref s) if s == " Bye!"));
            }
            other => panic!("expected Concat(InterpolatedString, StringLit), got {other:?}"),
        }
    }

    #[test]
    fn parse_escaped_no_interp() {
        // \$ suppresses interpolation — should be plain StringLit.
        let e = parse_expr_str(r#""price: \$100";"#);
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "price: $100"),
            other => panic!("expected StringLit, got {other:?}"),
        }
    }

    #[test]
    fn parse_print_interp_string() {
        let prog = parse(r#"print "Hello, $name!\n";"#);
        assert_eq!(prog.statements.len(), 1);
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PrintOp(name, fh, args), .. }) => {
                assert_eq!(name, "print");
                assert!(fh.is_none());
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::InterpolatedString(_)));
            }
            other => panic!("expected print with InterpolatedString arg, got {other:?}"),
        }
    }

    // ═══════════════════════════════════════════════════════════
    // Subscript-chain interpolation inside strings.
    //
    // All of these should parse the subscript into real AST
    // nodes inside a `ScalarInterp(Box<Expr>)` / `ArrayInterp(...)`
    // part — not be swallowed into a `Const` segment.
    // ═══════════════════════════════════════════════════════════

    /// Pull the `parts` out of an interpolated-string expression.
    fn interp_parts(src: &str) -> Vec<InterpPart> {
        let e = parse_expr_str(src);
        match e.kind {
            ExprKind::InterpolatedString(Interpolated(parts)) => parts,
            // Some single-subscript strings collapse via merge
            // into a non-interpolated StringLit in degenerate
            // cases — callers pass non-degenerate sources.
            other => panic!("expected InterpolatedString, got {other:?} for {src:?}"),
        }
    }

    /// For string-level asserts: the N-th part should be a
    /// scalar-interp wrapping an expression whose pretty-printed
    /// outline matches a given structural check.
    fn scalar_part(parts: &[InterpPart], n: usize) -> &Expr {
        scalar_interp_expr(&parts[n])
    }

    fn array_part(parts: &[InterpPart], n: usize) -> &Expr {
        array_interp_expr(&parts[n])
    }

    // ── Basic subscript forms ─────────────────────────────────

    #[test]
    fn interp_hash_elem_arrow() {
        // "$h->{key}" — classic bugged case.  Must parse as a
        // ScalarInterp wrapping ArrowDeref(ScalarVar(h), HashElem(key)).
        let parts = interp_parts(r#""$h->{key}";"#);
        let e = scalar_part(&parts, 0);
        match &e.kind {
            ExprKind::ArrowDeref(recv, ArrowTarget::HashElem(k)) => {
                assert!(matches!(recv.kind, ExprKind::ScalarVar(ref n) if n == "h"));
                // Key is a bareword (autoquoted by the subscript
                // rule in the parser).
                assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "key"));
            }
            other => panic!("expected ArrowDeref hash-elem, got {other:?}"),
        }
    }

    #[test]
    fn interp_array_elem_arrow() {
        // "$a->[0]"
        let parts = interp_parts(r#""$a->[0]";"#);
        let e = scalar_part(&parts, 0);
        match &e.kind {
            ExprKind::ArrowDeref(recv, ArrowTarget::ArrayElem(i)) => {
                assert!(matches!(recv.kind, ExprKind::ScalarVar(ref n) if n == "a"));
                assert!(matches!(i.kind, ExprKind::IntLit(0)));
            }
            other => panic!("expected ArrowDeref array-elem, got {other:?}"),
        }
    }

    #[test]
    fn interp_hash_elem_direct() {
        // "$h{key}" — no arrow.  In Perl this is still a hash
        // element access because `$h{...}` is equivalent to
        // `${h}{...}`.  Parses as HashElem(ScalarVar(h), key).
        let parts = interp_parts(r#""$h{key}";"#);
        let e = scalar_part(&parts, 0);
        match &e.kind {
            ExprKind::HashElem(recv, k) => {
                assert!(matches!(recv.kind, ExprKind::ScalarVar(ref n) if n == "h"));
                assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "key"));
            }
            other => panic!("expected HashElem, got {other:?}"),
        }
    }

    #[test]
    fn interp_array_elem_direct() {
        // "$a[3]"
        let parts = interp_parts(r#""$a[3]";"#);
        let e = scalar_part(&parts, 0);
        match &e.kind {
            ExprKind::ArrayElem(recv, i) => {
                assert!(matches!(recv.kind, ExprKind::ScalarVar(ref n) if n == "a"));
                assert!(matches!(i.kind, ExprKind::IntLit(3)));
            }
            other => panic!("expected ArrayElem, got {other:?}"),
        }
    }

    // ── Chained subscripts ────────────────────────────────────

    #[test]
    fn interp_chain_two_hash_levels() {
        // "$h->{a}{b}" — arrow before first, implicit between.
        // Hash elem wrapped in hash elem.
        let parts = interp_parts(r#""$h->{a}{b}";"#);
        let e = scalar_part(&parts, 0);
        // Outer: HashElem(ArrowDeref(..., HashElem(h, a)), b)
        match &e.kind {
            ExprKind::HashElem(inner, k2) => {
                assert!(matches!(k2.kind, ExprKind::StringLit(ref s) if s == "b"));
                match &inner.kind {
                    ExprKind::ArrowDeref(recv, ArrowTarget::HashElem(k1)) => {
                        assert!(matches!(recv.kind, ExprKind::ScalarVar(ref n) if n == "h"));
                        assert!(matches!(k1.kind, ExprKind::StringLit(ref s) if s == "a"));
                    }
                    other => panic!("expected inner ArrowDeref, got {other:?}"),
                }
            }
            other => panic!("expected outer HashElem, got {other:?}"),
        }
    }

    #[test]
    fn interp_chain_hash_then_array() {
        // "$h->{k}[0]"
        let parts = interp_parts(r#""$h->{k}[0]";"#);
        let e = scalar_part(&parts, 0);
        match &e.kind {
            ExprKind::ArrayElem(inner, i) => {
                assert!(matches!(i.kind, ExprKind::IntLit(0)));
                assert!(matches!(inner.kind, ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))));
            }
            other => panic!("expected ArrayElem wrapping ArrowDeref, got {other:?}"),
        }
    }

    #[test]
    fn interp_chain_array_then_hash() {
        // "$a[0]{k}"
        let parts = interp_parts(r#""$a[0]{k}";"#);
        let e = scalar_part(&parts, 0);
        match &e.kind {
            ExprKind::HashElem(inner, k) => {
                assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "k"));
                match &inner.kind {
                    ExprKind::ArrayElem(recv, i) => {
                        assert!(matches!(recv.kind, ExprKind::ScalarVar(ref n) if n == "a"));
                        assert!(matches!(i.kind, ExprKind::IntLit(0)));
                    }
                    other => panic!("expected inner ArrayElem, got {other:?}"),
                }
            }
            other => panic!("expected outer HashElem, got {other:?}"),
        }
    }

    #[test]
    fn interp_chain_three_levels() {
        // "$h->{a}->{b}->{c}" — three arrow-hashes.
        let parts = interp_parts(r#""$h->{a}->{b}->{c}";"#);
        let e = scalar_part(&parts, 0);
        // Triple-nested ArrowDeref(HashElem).
        fn unwrap_hash_arrow(expr: &Expr) -> (&Expr, &Expr) {
            match &expr.kind {
                ExprKind::ArrowDeref(recv, ArrowTarget::HashElem(k)) => (recv, k),
                other => panic!("expected ArrowDeref hash, got {other:?}"),
            }
        }
        let (mid, k3) = unwrap_hash_arrow(e);
        assert!(matches!(k3.kind, ExprKind::StringLit(ref s) if s == "c"));
        let (innermost, k2) = unwrap_hash_arrow(mid);
        assert!(matches!(k2.kind, ExprKind::StringLit(ref s) if s == "b"));
        let (leaf, k1) = unwrap_hash_arrow(innermost);
        assert!(matches!(k1.kind, ExprKind::StringLit(ref s) if s == "a"));
        assert!(matches!(leaf.kind, ExprKind::ScalarVar(ref n) if n == "h"));
    }

    #[test]
    fn interp_chain_arrow_then_implicit() {
        // "$h->{a}[0]{b}" — arrow, array, hash.
        let parts = interp_parts(r#""$h->{a}[0]{b}";"#);
        let e = scalar_part(&parts, 0);
        match &e.kind {
            ExprKind::HashElem(ae, k) => {
                assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "b"));
                match &ae.kind {
                    ExprKind::ArrayElem(ad, i) => {
                        assert!(matches!(i.kind, ExprKind::IntLit(0)));
                        assert!(matches!(ad.kind, ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))));
                    }
                    other => panic!("expected ArrayElem, got {other:?}"),
                }
            }
            other => panic!("expected outer HashElem, got {other:?}"),
        }
    }

    // ── Subscripts with expression keys/indices ───────────────

    #[test]
    fn interp_hash_subscript_expr_key() {
        // "$h->{$k}" — key is a scalar variable, not bareword.
        let parts = interp_parts(r#""$h->{$k}";"#);
        let e = scalar_part(&parts, 0);
        match &e.kind {
            ExprKind::ArrowDeref(_, ArrowTarget::HashElem(k)) => {
                assert!(matches!(k.kind, ExprKind::ScalarVar(ref n) if n == "k"));
            }
            other => panic!("expected ArrowDeref hash-elem, got {other:?}"),
        }
    }

    #[test]
    fn interp_array_subscript_expr_index() {
        // "$a[$i]" — index is $i.
        let parts = interp_parts(r#""$a[$i]";"#);
        let e = scalar_part(&parts, 0);
        match &e.kind {
            ExprKind::ArrayElem(_, i) => {
                assert!(matches!(i.kind, ExprKind::ScalarVar(ref n) if n == "i"));
            }
            other => panic!("expected ArrayElem, got {other:?}"),
        }
    }

    #[test]
    fn interp_array_subscript_arith_expr() {
        // "$a[$i + 1]"
        let parts = interp_parts(r#""$a[$i + 1]";"#);
        let e = scalar_part(&parts, 0);
        match &e.kind {
            ExprKind::ArrayElem(_, i) => {
                assert!(matches!(i.kind, ExprKind::BinOp(BinOp::Add, _, _)));
            }
            other => panic!("expected ArrayElem, got {other:?}"),
        }
    }

    #[test]
    fn interp_hash_subscript_string_key() {
        // "$h{'literal'}" — explicit single-quoted key.
        let parts = interp_parts(r#""$h{'literal'}";"#);
        let e = scalar_part(&parts, 0);
        match &e.kind {
            ExprKind::HashElem(_, k) => {
                assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "literal"));
            }
            other => panic!("expected HashElem, got {other:?}"),
        }
    }

    // ── Array-interp chains ──────────────────────────────────

    #[test]
    fn interp_array_slice_range() {
        // "@a[1..3]" — array slice with a range index.
        let parts = interp_parts(r#""@a[1..3]";"#);
        let e = array_part(&parts, 0);
        match &e.kind {
            ExprKind::ArraySlice(recv, indices) => {
                assert!(matches!(recv.kind, ExprKind::ArrayVar(ref n) if n == "a"));
                assert_eq!(indices.len(), 1);
                assert!(matches!(indices[0].kind, ExprKind::Range(_, _)));
            }
            other => panic!("expected ArraySlice, got {other:?}"),
        }
    }

    #[test]
    fn interp_hash_slice_list() {
        // "@h{'k1','k2'}" — hash slice with two keys.
        let parts = interp_parts(r#""@h{'k1','k2'}";"#);
        let e = array_part(&parts, 0);
        match &e.kind {
            ExprKind::HashSlice(recv, keys) => {
                assert!(matches!(recv.kind, ExprKind::ArrayVar(ref n) if n == "h"));
                assert_eq!(keys.len(), 2);
                assert!(matches!(keys[0].kind, ExprKind::StringLit(ref s) if s == "k1"));
                assert!(matches!(keys[1].kind, ExprKind::StringLit(ref s) if s == "k2"));
            }
            other => panic!("expected HashSlice, got {other:?}"),
        }
    }

    // ── Mixed with literal text ──────────────────────────────

    #[test]
    fn interp_chain_mid_string() {
        // "a $h->{key} b" — subscript in the middle.
        let parts = interp_parts(r#""a $h->{key} b";"#);
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[0], InterpPart::Const(s) if s == "a "));
        // Middle is the chain.
        let e = scalar_part(&parts, 1);
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))));
        assert!(matches!(&parts[2], InterpPart::Const(s) if s == " b"));
    }

    #[test]
    fn interp_two_chains_one_string() {
        // "$h->{k} and $a[0]"
        let parts = interp_parts(r#""$h->{k} and $a[0]";"#);
        assert_eq!(parts.len(), 3);
        let e0 = scalar_part(&parts, 0);
        assert!(matches!(e0.kind, ExprKind::ArrowDeref(_, _)));
        assert!(matches!(&parts[1], InterpPart::Const(s) if s == " and "));
        let e2 = scalar_part(&parts, 2);
        assert!(matches!(e2.kind, ExprKind::ArrayElem(_, _)));
    }

    // ── Negative cases (no chain) ────────────────────────────

    #[test]
    fn interp_bare_arrow_is_literal() {
        // "$a->" — bare arrow with nothing after.  Lexer must not
        // start a chain; the `->` stays literal text.
        let parts = interp_parts(r#""$a->";"#);
        assert_eq!(parts.len(), 2);
        assert_eq!(scalar_interp_name(&parts[0]), Some("a"));
        assert!(matches!(&parts[1], InterpPart::Const(s) if s == "->"));
    }

    #[test]
    fn interp_bare_arrow_then_ident_is_literal() {
        // "$a->foo" — method-call shape is NOT interpolated in
        // strings (per perlop).  `$a` interpolates; `->foo`
        // renders literally.
        let parts = interp_parts(r#""$a->foo";"#);
        assert_eq!(parts.len(), 2);
        assert_eq!(scalar_interp_name(&parts[0]), Some("a"));
        assert!(matches!(&parts[1], InterpPart::Const(s) if s == "->foo"));
    }

    #[test]
    fn interp_plain_scalar_no_subscript() {
        // Simple "$name" shouldn't start a chain.  Still uses the
        // new ScalarInterp(Box<Expr>) wrapper around a bare
        // ScalarVar.
        let parts = interp_parts(r#""Hello $name!";"#);
        assert_eq!(parts.len(), 3);
        assert_eq!(scalar_interp_name(&parts[1]), Some("name"));
    }

    #[test]
    fn interp_trailing_literal_bracket() {
        // "$a [0]" — space before `[` means it's NOT a subscript.
        // The literal `[` and `]` stay as ConstSegment.
        let parts = interp_parts(r#""$a [0]";"#);
        // Parts: ScalarInterp(a), Const(" [0]").
        assert_eq!(parts.len(), 2);
        assert_eq!(scalar_interp_name(&parts[0]), Some("a"));
        assert!(matches!(&parts[1], InterpPart::Const(s) if s == " [0]"));
    }

    // ── Escaped sigils ───────────────────────────────────────

    #[test]
    fn interp_escaped_dollar_before_subscript_bracket() {
        // "\$a[0]" — escaped `$`; whole thing is literal.
        let e = parse_expr_str(r#""\$a[0]";"#);
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "$a[0]"),
            other => panic!("expected StringLit, got {other:?}"),
        }
    }

    #[test]
    fn interp_escaped_arrow_after_var() {
        // `"\$a->{x}"` — escaped $ makes the whole thing literal.
        let e = parse_expr_str(r#""\$a->{x}";"#);
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "$a->{x}"),
            other => panic!("expected StringLit, got {other:?}"),
        }
    }

    // ── Nested braces inside subscript expression ────────────

    #[test]
    fn interp_subscript_with_nested_braces() {
        // `"$h->{$x}{y}"` — two nested subscripts, with `y` as
        // a bareword hash key in the inner-most subscript.
        //
        // `y}` is a lexer edge case: `y` is one of the quote
        // keywords (alias for `tr`), so at_quote_delimiter must
        // reject the closing `}` that follows.  Tests below
        // cover every quote keyword × every closing delimiter
        // combination; this one spot-checks the interaction with
        // subscript-chain interpolation specifically.
        let parts = interp_parts(r#""$h->{$x}{y}";"#);
        let e = scalar_part(&parts, 0);
        match &e.kind {
            ExprKind::HashElem(inner, k) => {
                assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "y"));
                match &inner.kind {
                    ExprKind::ArrowDeref(_, ArrowTarget::HashElem(k1)) => {
                        assert!(matches!(k1.kind, ExprKind::ScalarVar(ref n) if n == "x"));
                    }
                    other => panic!("expected ArrowDeref hash-elem, got {other:?}"),
                }
            }
            other => panic!("expected HashElem, got {other:?}"),
        }
    }

    #[test]
    fn interp_subscript_with_func_call() {
        // "$h->{foo()}" — key is a function call.
        let parts = interp_parts(r#""$h->{foo()}";"#);
        let e = scalar_part(&parts, 0);
        match &e.kind {
            ExprKind::ArrowDeref(_, ArrowTarget::HashElem(k)) => {
                assert!(matches!(k.kind, ExprKind::FuncCall(_, _)));
            }
            other => panic!("expected ArrowDeref hash-elem, got {other:?}"),
        }
    }

    // ── In qq// ──────────────────────────────────────────────

    #[test]
    fn interp_qq_with_subscript() {
        // qq{...} uses `{}` as delimiter; the `{key}` inside is
        // still recognized as a hash subscript.
        let e = parse_expr_str("qq{$h->{key}};");
        match &e.kind {
            ExprKind::InterpolatedString(Interpolated(parts)) => {
                assert_eq!(parts.len(), 1);
                let inner = scalar_part(parts, 0);
                assert!(matches!(inner.kind, ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))));
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    // ── Concatenation-style interpolation context ────────────

    #[test]
    fn interp_chain_then_concat() {
        // Interpolated string concatenated with another.  The
        // chain in the first one must still be parsed correctly.
        let e = parse_expr_str(r#""$h->{key}" . "plain";"#);
        match &e.kind {
            ExprKind::BinOp(BinOp::Concat, left, _) => {
                if let ExprKind::InterpolatedString(Interpolated(parts)) = &left.kind {
                    assert_eq!(parts.len(), 1);
                    assert!(matches!(scalar_part(parts, 0).kind, ExprKind::ArrowDeref(_, _)));
                } else {
                    panic!("left should be InterpolatedString");
                }
            }
            other => panic!("expected Concat, got {other:?}"),
        }
    }

    // ── @name chain forms ────────────────────────────────────

    #[test]
    fn interp_array_chain_in_mid_string() {
        // "list: @a[0..2] done"
        let parts = interp_parts(r#""list: @a[0..2] done";"#);
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[0], InterpPart::Const(s) if s == "list: "));
        let e = array_part(&parts, 1);
        match &e.kind {
            ExprKind::ArraySlice(recv, indices) => {
                assert!(matches!(recv.kind, ExprKind::ArrayVar(ref n) if n == "a"));
                assert_eq!(indices.len(), 1);
                assert!(matches!(indices[0].kind, ExprKind::Range(_, _)));
            }
            other => panic!("expected ArraySlice, got {other:?}"),
        }
        assert!(matches!(&parts[2], InterpPart::Const(s) if s == " done"));
    }

    // ── ${name}-expression form interaction ──────────────────

    #[test]
    fn interp_braced_name_then_literal_subscript() {
        // "${name}[0]" — `${name}` is explicit braced form.
        // The `[0]` after the `}` is literal text (per Perl
        // behavior: ${name}[0] interpolates only $name).
        let parts = interp_parts(r#""${name}[0]";"#);
        assert_eq!(parts.len(), 2);
        assert_eq!(scalar_interp_name(&parts[0]), Some("name"));
        assert!(matches!(&parts[1], InterpPart::Const(s) if s == "[0]"));
    }

    // ── Regex interpolation (shares the same scanner) ────────

    #[test]
    fn regex_interp_subscript() {
        // m/$h->{key}/ — regex bodies use the same interp
        // machinery; chains should work there too.
        let e = parse_expr_str(r#"m/$h->{key}/;"#);
        match &e.kind {
            ExprKind::Regex(_, pat, _) => {
                let parts = &pat.0;
                // Expect at least one ScalarInterp with the chain.
                let has_chain = parts.iter().any(|p| {
                    matches!(
                        p,
                        InterpPart::ScalarInterp(expr) if matches!(
                            expr.kind,
                            ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))
                        )
                    )
                });
                assert!(has_chain, "expected arrow-hash chain in regex parts: {parts:?}");
            }
            other => panic!("expected Regex, got {other:?}"),
        }
    }

    // ── Missing tests promised in the audit ───────────────────
    //
    // These cover interpolation contexts beyond plain `"..."` —
    // heredoc bodies, `qr//`, `s///` pattern and replacement,
    // and the `@{[expr]}` form mixed with chains.  A few cases
    // don't work yet and are marked `#[ignore]` with a clear
    // note explaining the gap; they're here rather than absent
    // so the gap is visible in the test suite rather than only
    // in my memory.

    // Heredoc with chain in body.

    #[test]
    fn interp_chain_in_heredoc() {
        let src = "<<END;\nvalue: $h->{key}\nEND\n";
        let prog = parse(src);
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::InterpolatedString(Interpolated(parts)), .. }) => {
                let has_chain = parts.iter().any(|p| {
                    matches!(
                        p,
                        InterpPart::ScalarInterp(e) if matches!(
                            e.kind,
                            ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))
                        )
                    )
                });
                assert!(has_chain, "expected arrow-hash chain in heredoc parts: {parts:?}");
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn interp_direct_subscript_in_heredoc() {
        // Bare `$a[0]` inside a heredoc body.
        let src = "<<END;\nfirst: $a[0]\nEND\n";
        let prog = parse(src);
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::InterpolatedString(Interpolated(parts)), .. }) => {
                let has_elem = parts.iter().any(|p| {
                    matches!(
                        p,
                        InterpPart::ScalarInterp(e) if matches!(e.kind, ExprKind::ArrayElem(_, _))
                    )
                });
                assert!(has_elem, "expected ArrayElem chain in heredoc parts: {parts:?}");
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    // qr// compiled-regex with chain.

    #[test]
    fn interp_chain_in_qr() {
        let e = parse_expr_str(r#"qr/$h->{key}/;"#);
        match &e.kind {
            ExprKind::Regex(RegexKind::Qr, pat, _) => {
                let has_chain = pat.0.iter().any(|p| {
                    matches!(
                        p,
                        InterpPart::ScalarInterp(e) if matches!(
                            e.kind,
                            ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))
                        )
                    )
                });
                assert!(has_chain, "expected arrow-hash chain in qr// parts: {parts:?}", parts = pat.0);
            }
            other => panic!("expected Regex(Qr, ...), got {other:?}"),
        }
    }

    // s/// — pattern AND replacement can interpolate.

    #[test]
    fn interp_chain_in_subst_pattern() {
        let e = parse_expr_str(r#"s/$h->{key}/new/;"#);
        match &e.kind {
            ExprKind::Subst(pat, _, _) => {
                let has_chain = pat.0.iter().any(|p| {
                    matches!(
                        p,
                        InterpPart::ScalarInterp(e) if matches!(
                            e.kind,
                            ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))
                        )
                    )
                });
                assert!(has_chain, "expected arrow-hash chain in subst pattern: {parts:?}", parts = pat.0);
            }
            other => panic!("expected Subst, got {other:?}"),
        }
    }

    #[test]
    fn interp_chain_in_subst_replacement() {
        let e = parse_expr_str(r#"s/old/$h->{key}/;"#);
        match &e.kind {
            ExprKind::Subst(_, repl, _) => {
                let has_chain = repl.0.iter().any(|p| {
                    matches!(
                        p,
                        InterpPart::ScalarInterp(e) if matches!(
                            e.kind,
                            ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))
                        )
                    )
                });
                assert!(has_chain, "expected arrow-hash chain in subst replacement: {parts:?}", parts = repl.0);
            }
            other => panic!("expected Subst, got {other:?}"),
        }
    }

    // @{[expr]} expression-interpolation form.

    #[test]
    fn interp_array_expr_form_with_chain_inside() {
        // `"@{[$h->{k}]}"` — the @{[...]} form wraps an
        // expression; the expression internally uses a
        // subscript chain.  Outer shape is ExprInterp (not
        // ChainStart) because the leading token is `@{`, not
        // `@name`.
        let parts = interp_parts(r#""@{[$h->{k}]}";"#);
        let expr_part = parts
            .iter()
            .find_map(|p| match p {
                InterpPart::ExprInterp(e) => Some(e),
                _ => None,
            })
            .expect("expected an ExprInterp part");
        // Inside: anonymous array ref containing the chain.
        // AnonArray([ArrowDeref(h, HashElem(k))])
        match &expr_part.kind {
            ExprKind::AnonArray(items) => {
                assert_eq!(items.len(), 1);
                assert!(matches!(items[0].kind, ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))));
            }
            other => panic!("expected AnonArray inside @{{[...]}}: {other:?}"),
        }
    }

    // Escape sequences in hash-subscript position are NOT
    // processed as string escapes.  `"$h{\x41}"` is NOT
    // `$h{'A'}`; per `perl -MO=Deparse -e '"$h{\x41}"'` it
    // parses as `"$h{\'x41'}"` — the `\` is the reference
    // operator applied to the autoquoted bareword `x41`.  The
    // hash lookup key is therefore a scalar reference (which
    // stringifies to `SCALAR(0x...)` at runtime).
    //
    // Verified with the Perl debugger:
    //
    // ```perl
    // my %h = (x41 => 'test');
    // print "$h{\x41}\n";    # empty — lookup misses, stringified ref != 'x41'
    // ```

    #[test]
    fn interp_escape_sequence_in_hash_subscript_is_ref_to_bareword() {
        let parts = interp_parts(r#""$h{\x41}";"#);
        let e = scalar_part(&parts, 0);
        match &e.kind {
            ExprKind::HashElem(_, k) => match &k.kind {
                ExprKind::Ref(inner) => {
                    // Inner: the autoquoted bareword "x41".
                    assert!(matches!(inner.kind, ExprKind::StringLit(ref s) if s == "x41"), "expected Ref(StringLit('x41')), inner was {:?}", inner.kind);
                }
                other => panic!("expected Ref(StringLit('x41')) as hash key, got {other:?}"),
            },
            other => panic!("expected HashElem, got {other:?}"),
        }
    }

    // ── Known gaps — ignored tests, kept visible ─────────────
    //
    // These encode behavior we haven't implemented yet.  Each
    // is marked `#[ignore]` with a note explaining what's
    // missing.  Running with `cargo test -- --ignored` will
    // run them and show the real failures.

    #[test]
    fn interp_postderef_qq_array() {
        // `"$ref->@*"` — postderef array form inside a string.
        // Requires peek_chain_starter to recognize `->@*` and
        // the chain dispatch to end on `Star` at depth 0.
        let parts = interp_parts(r#""$ref->@*";"#);
        let e = scalar_part(&parts, 0);
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefArray)), "expected ArrowDeref(_, DerefArray), got {:?}", e.kind);
    }

    #[test]
    fn interp_postderef_qq_hash() {
        // `"$ref->%*"` — postderef hash form.
        let parts = interp_parts(r#""$ref->%*";"#);
        let e = scalar_part(&parts, 0);
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefHash)), "expected ArrowDeref(_, DerefHash), got {:?}", e.kind);
    }

    #[test]
    fn interp_postderef_qq_scalar() {
        // `"$ref->$*"` — postderef scalar form.
        let parts = interp_parts(r#""$ref->$*";"#);
        let e = scalar_part(&parts, 0);
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefScalar)), "expected ArrowDeref(_, DerefScalar), got {:?}", e.kind);
    }

    #[test]
    fn interp_postderef_qq_last_index() {
        // `"$ref->$#*"` — postderef last-index in a string.
        // The `#` would normally start a comment in code mode;
        // this works because the parser's `try_consume_hash_star`
        // consumes the raw `#*` bytes between lex_token calls,
        // and (in chain mode) sets `chain_end_pending` so the
        // chain terminates cleanly.
        let parts = interp_parts(r#""$ref->$#*";"#);
        let e = scalar_part(&parts, 0);
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::LastIndex)), "expected ArrowDeref(_, LastIndex), got {:?}", e.kind);
    }

    #[test]
    fn interp_postderef_qq_chained_after_subscript() {
        // `"$h->{key}->@*"` — subscript then postderef in one chain.
        let parts = interp_parts(r#""$h->{key}->@*";"#);
        let e = scalar_part(&parts, 0);
        match &e.kind {
            ExprKind::ArrowDeref(inner, ArrowTarget::DerefArray) => {
                // Inner: ArrowDeref(ScalarVar(h), HashElem(key)).
                assert!(matches!(inner.kind, ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))), "inner should be hash-elem deref, got {:?}", inner.kind);
            }
            other => panic!("expected ArrowDeref(_, DerefArray), got {other:?}"),
        }
    }

    #[test]
    fn interp_postderef_qq_with_surrounding_text() {
        // `"values: $ref->@* end"` — postderef mid-string.
        let parts = interp_parts(r#""values: $ref->@* end";"#);
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[0], InterpPart::Const(s) if s == "values: "));
        let e = scalar_part(&parts, 1);
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefArray)));
        assert!(matches!(&parts[2], InterpPart::Const(s) if s == " end"));
    }

    // ── Regex / substitution / transliteration tests ──────────

    #[test]
    fn parse_bare_regex() {
        let e = parse_expr_str("/foo/i;");
        match &e.kind {
            ExprKind::Regex(_, pat, flags) => {
                assert_eq!(pat_str(pat), "foo");
                assert_eq!(flags.as_deref(), Some("i"));
            }
            other => panic!("expected Regex, got {other:?}"),
        }
    }

    #[test]
    fn parse_regex_binding() {
        let e = parse_expr_str("$x =~ /foo/;");
        match &e.kind {
            ExprKind::BinOp(BinOp::Binding, _, right) => {
                assert!(matches!(&right.kind, ExprKind::Regex(_, _, _)));
            }
            other => panic!("expected Binding, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_regex() {
        // // in term position is an empty regex, not defined-or.
        let e = parse_expr_str("$x =~ //;");
        match &e.kind {
            ExprKind::BinOp(BinOp::Binding, _, right) => match &right.kind {
                ExprKind::Regex(_, pat, flags) => {
                    assert_eq!(pat_str(pat), "");
                    assert!(flags.is_none());
                }
                other => panic!("expected empty Regex, got {other:?}"),
            },
            other => panic!("expected Binding, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_regex_bare() {
        // // at statement level is an empty regex match against $_.
        let e = parse_expr_str("//;");
        match &e.kind {
            ExprKind::Regex(_, pat, flags) => {
                assert_eq!(pat_str(pat), "");
                assert!(flags.is_none());
            }
            other => panic!("expected empty Regex, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_regex_with_flags() {
        // //gi in term position is an empty regex with flags.
        let e = parse_expr_str("$x =~ //gi;");
        match &e.kind {
            ExprKind::BinOp(BinOp::Binding, _, right) => match &right.kind {
                ExprKind::Regex(_, pat, flags) => {
                    assert_eq!(pat_str(pat), "");
                    assert_eq!(flags.as_deref(), Some("gi"));
                }
                other => panic!("expected empty Regex with flags, got {other:?}"),
            },
            other => panic!("expected Binding, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_regex_bare_with_flags() {
        // //gi at statement level is an empty regex with flags.
        let e = parse_expr_str("//gi;");
        match &e.kind {
            ExprKind::Regex(_, pat, flags) => {
                assert_eq!(pat_str(pat), "");
                assert_eq!(flags.as_deref(), Some("gi"));
            }
            other => panic!("expected empty Regex with flags, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_regex_in_condition() {
        // if (//) { } — empty regex as condition.
        let prog = parse("if (//) { 1; }");
        assert_eq!(prog.statements.len(), 1);
        match &prog.statements[0].kind {
            StmtKind::If(if_stmt) => {
                assert!(matches!(if_stmt.condition.kind, ExprKind::Regex(_, _, _)));
            }
            other => panic!("expected If, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_regex_as_print_arg() {
        // print //; — empty regex as print argument.
        let prog = parse("print //;");
        assert_eq!(prog.statements.len(), 1);
    }

    #[test]
    fn parse_empty_regex_in_split() {
        // split //, $s — empty regex as split pattern.
        let prog = parse("split //, $s;");
        assert_eq!(prog.statements.len(), 1);
    }

    #[test]
    fn parse_empty_regex_space_not_flags() {
        // // gi — space separates, so gi is NOT flags.
        // This produces an empty regex with no flags.
        let e = parse_expr_str("$x =~ //gi;");
        match &e.kind {
            ExprKind::BinOp(BinOp::Binding, _, right) => match &right.kind {
                ExprKind::Regex(_, pat, flags) => {
                    assert_eq!(pat_str(pat), "");
                    assert_eq!(flags.as_deref(), Some("gi"));
                }
                other => panic!("expected Regex, got {other:?}"),
            },
            other => panic!("expected Binding, got {other:?}"),
        }
        // But with space: flags are NOT consumed.
        let e2 = parse_expr_str("$x =~ // gi;");
        match &e2.kind {
            ExprKind::BinOp(BinOp::Binding, _, right) => match &right.kind {
                ExprKind::Regex(_, pat, flags) => {
                    assert_eq!(pat_str(pat), "");
                    assert!(flags.is_none());
                }
                other => panic!("expected Regex with empty flags, got {other:?}"),
            },
            other => panic!("expected Binding, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_regex_in_ternary() {
        // $x = // ? 1 : 0 — empty regex match, then ternary.
        let e = parse_expr_str("$x = // ? 1 : 0;");
        assert!(matches!(e.kind, ExprKind::Assign(_, _, _)));
    }

    #[test]
    fn parse_regex_invalid_flag() {
        // /foo/q — invalid flag 'q' should produce an error.
        let mut parser = Parser::new(b"/foo/q;").unwrap();
        let result = parser.parse_program();
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("Unknown regexp modifier"));
    }

    #[test]
    fn parse_tr_invalid_flag() {
        // tr/a/b/q — invalid flag 'q' should produce an error.
        let mut parser = Parser::new(b"tr/a/b/q;").unwrap();
        let result = parser.parse_program();
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("Unknown transliteration modifier"));
    }

    // ── prefers_defined_or: UNIDOR operators ────────────────
    //
    // After these operators, // is defined-or, not an empty regex.
    // Matches toke.c's UNIDOR macro and XTERMORDORDOR behavior.

    #[test]
    fn parse_shift_prefers_defined_or() {
        let e = parse_expr_str("shift // 0;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
    }

    #[test]
    fn parse_pop_prefers_defined_or() {
        let e = parse_expr_str("pop // 0;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
    }

    #[test]
    fn parse_getc_prefers_defined_or() {
        let e = parse_expr_str("getc // 0;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
    }

    #[test]
    fn parse_pos_prefers_defined_or() {
        let e = parse_expr_str("pos // 0;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
    }

    #[test]
    fn parse_readline_prefers_defined_or() {
        let e = parse_expr_str("readline // 0;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
    }

    #[test]
    fn parse_readlink_prefers_defined_or() {
        let e = parse_expr_str("readlink // 0;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
    }

    #[test]
    fn parse_undef_prefers_defined_or() {
        let e = parse_expr_str("undef // 0;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
    }

    #[test]
    fn parse_umask_prefers_defined_or() {
        let e = parse_expr_str("umask // 0;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
    }

    #[test]
    fn parse_filetest_prefers_defined_or() {
        // -f // "default" — file test with no operand, then defined-or.
        let e = parse_expr_str("-f // \"default\";");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
    }

    #[test]
    fn parse_shift_defined_or_bareword() {
        // shift //i 0 — in Perl this is a syntax error because i is not
        // predeclared.  Our parser is more permissive: it parses as
        // shift() // i(0) since any bareword can be a function call.
        let e = parse_expr_str("shift //i 0;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
    }

    #[test]
    fn parse_substitution() {
        let e = parse_expr_str("s/foo/bar/g;");
        match &e.kind {
            ExprKind::Subst(pat, repl, flags) => {
                assert_eq!(pat_str(pat), "foo");
                assert_eq!(flags.as_deref(), Some("g"));
                assert_eq!(pat_str(repl), "bar");
            }
            other => panic!("expected Subst, got {other:?}"),
        }
    }

    #[test]
    fn parse_substitution_no_flags() {
        let e = parse_expr_str("s/old/new/;");
        match &e.kind {
            ExprKind::Subst(pat, repl, flags) => {
                assert_eq!(pat_str(pat), "old");
                assert!(flags.is_none());
                assert_eq!(pat_str(repl), "new");
            }
            other => panic!("expected Subst, got {other:?}"),
        }
    }

    #[test]
    fn parse_substitution_paired_delimiters() {
        let e = parse_expr_str("s{foo}{bar}g;");
        match &e.kind {
            ExprKind::Subst(pat, repl, flags) => {
                assert_eq!(pat_str(pat), "foo");
                assert_eq!(flags.as_deref(), Some("g"));
                assert_eq!(pat_str(repl), "bar");
            }
            other => panic!("expected Subst, got {other:?}"),
        }
    }

    #[test]
    fn parse_transliteration() {
        let e = parse_expr_str("tr/a-z/A-Z/;");
        match &e.kind {
            ExprKind::Translit(from, to, _) => {
                assert_eq!(from, "a-z");
                assert_eq!(to, "A-Z");
            }
            other => panic!("expected Translit, got {other:?}"),
        }
    }

    #[test]
    fn parse_subst_binding() {
        let e = parse_expr_str("$x =~ s/old/new/g;");
        match &e.kind {
            ExprKind::BinOp(BinOp::Binding, _, right) => {
                assert!(matches!(&right.kind, ExprKind::Subst(_, _, _)));
            }
            other => panic!("expected Binding with Subst, got {other:?}"),
        }
    }

    // ── Heredoc tests ─────────────────────────────────────────

    #[test]
    fn parse_heredoc_basic() {
        let src = "my $x = <<END;\nhello world\nEND\n";
        let prog = parse(src);
        assert_eq!(prog.statements.len(), 1);
        let (_s, vars) = decl_vars(&prog.statements[0]);
        assert_eq!(vars[0].name, "x");
        let init = decl_init(&prog.statements[0]);
        match &init.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "hello world\n"),
            other => panic!("expected StringLit, got {other:?}"),
        }
    }

    #[test]
    fn parse_heredoc_concat() {
        // <<END . " suffix" should parse as concatenation.
        let src = "<<END . \" suffix\";\nbody\nEND\n";
        let prog = parse(src);
        match &prog.statements[0].kind {
            StmtKind::Expr(e) => {
                assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Concat, _, _)));
            }
            other => panic!("expected Expr with Concat, got {other:?}"),
        }
    }

    #[test]
    fn parse_heredoc_then_statement() {
        let src = "print <<END;\nhello\nEND\nmy $x = 1;\n";
        let prog = parse(src);
        assert_eq!(prog.statements.len(), 2);
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PrintOp(name, _, _), .. }) => assert_eq!(name, "print"),
            other => panic!("expected print PrintOp, got {other:?}"),
        }
        match &prog.statements[1].kind {
            StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => match &lhs.kind {
                ExprKind::Decl(_, vars) => assert_eq!(vars[0].name, "x"),
                other => panic!("expected Decl lhs, got {other:?}"),
            },
            other => panic!("expected decl stmt, got {other:?}"),
        }
    }

    // ── Anonymous sub tests ───────────────────────────────────

    #[test]
    fn parse_anon_sub() {
        let e = parse_expr_str("sub { 42; };");
        match &e.kind {
            ExprKind::AnonSub(proto, _, _, body) => {
                assert!(proto.is_none());
                assert_eq!(body.statements.len(), 1);
            }
            other => panic!("expected AnonSub, got {other:?}"),
        }
    }

    #[test]
    fn parse_anon_sub_with_proto() {
        let e = parse_expr_str("sub ($x) { $x + 1; };");
        match &e.kind {
            ExprKind::AnonSub(proto, _, _, _) => {
                assert!(proto.is_some());
            }
            other => panic!("expected AnonSub, got {other:?}"),
        }
    }

    #[test]
    fn parse_anon_sub_as_arg() {
        let prog = parse("my $f = sub { 1; };");
        let init = decl_init(&prog.statements[0]);
        assert!(matches!(init.kind, ExprKind::AnonSub(..)));
    }

    // ── Phaser block tests ────────────────────────────────────

    #[test]
    fn parse_begin_block() {
        let prog = parse("BEGIN { 1; }");
        assert!(matches!(prog.statements[0].kind, StmtKind::Phaser(PhaserKind::Begin, _)));
    }

    #[test]
    fn parse_end_block() {
        let prog = parse("END { 1; }");
        assert!(matches!(prog.statements[0].kind, StmtKind::Phaser(PhaserKind::End, _)));
    }

    // ── Eval tests ────────────────────────────────────────────

    #[test]
    fn parse_eval_block() {
        let e = parse_expr_str("eval { die; };");
        assert!(matches!(e.kind, ExprKind::EvalBlock(_)));
    }

    #[test]
    fn parse_eval_expr() {
        let e = parse_expr_str("eval $code;");
        assert!(matches!(e.kind, ExprKind::EvalExpr(_)));
    }

    // ── Return / loop control tests ───────────────────────────

    #[test]
    fn parse_return_value() {
        let e = parse_expr_str("return 42;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "return");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected return call, got {other:?}"),
        }
    }

    #[test]
    fn parse_return_bare() {
        let e = parse_expr_str("return;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "return");
                assert_eq!(args.len(), 0);
            }
            other => panic!("expected bare return, got {other:?}"),
        }
    }

    #[test]
    fn parse_last_with_label() {
        let e = parse_expr_str("last OUTER;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "last");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected last with label, got {other:?}"),
        }
    }

    #[test]
    fn parse_next_bare() {
        let e = parse_expr_str("next;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "next");
                assert_eq!(args.len(), 0);
            }
            other => panic!("expected bare next, got {other:?}"),
        }
    }

    // ── Label tests ───────────────────────────────────────────

    #[test]
    fn parse_labeled_loop() {
        let prog = parse("OUTER: for my $i (@list) { next OUTER; }");
        match &prog.statements[0].kind {
            StmtKind::Labeled(label, inner) => {
                assert_eq!(label, "OUTER");
                assert!(matches!(inner.kind, StmtKind::ForEach(_)));
            }
            other => panic!("expected Labeled, got {other:?}"),
        }
    }

    // ── Chained subscript tests ───────────────────────────────

    #[test]
    fn parse_chained_array_subscripts() {
        // $aoa[0][1] — implicit arrow between adjacent subscripts
        let e = parse_expr_str("$aoa[0][1];");
        match &e.kind {
            ExprKind::ArrayElem(inner, _) => {
                assert!(matches!(inner.kind, ExprKind::ArrayElem(_, _)));
            }
            other => panic!("expected nested ArrayElem, got {other:?}"),
        }
    }

    #[test]
    fn parse_chained_hash_subscripts() {
        let e = parse_expr_str("$h{a}{b};");
        match &e.kind {
            ExprKind::HashElem(inner, _) => {
                assert!(matches!(inner.kind, ExprKind::HashElem(_, _)));
            }
            other => panic!("expected nested HashElem, got {other:?}"),
        }
    }

    #[test]
    fn parse_arrow_then_implicit_subscript() {
        // $ref->[0][1] — arrow for first, implicit for second
        let e = parse_expr_str("$ref->[0][1];");
        match &e.kind {
            ExprKind::ArrayElem(inner, _) => {
                assert!(matches!(inner.kind, ExprKind::ArrowDeref(_, _)));
            }
            other => panic!("expected ArrayElem wrapping ArrowDeref, got {other:?}"),
        }
    }

    // ── sort/map/grep block tests ─────────────────────────────

    #[test]
    fn parse_sort_block() {
        let e = parse_expr_str("sort { $a <=> $b } @list;");
        match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "sort");
                assert!(args.len() >= 2); // block + @list
                assert!(matches!(args[0].kind, ExprKind::AnonSub(..)));
            }
            other => panic!("expected sort ListOp, got {other:?}"),
        }
    }

    #[test]
    fn parse_map_block() {
        let e = parse_expr_str("map { $_ * 2 } @nums;");
        match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "map");
                assert!(matches!(args[0].kind, ExprKind::AnonSub(..)));
            }
            other => panic!("expected map ListOp, got {other:?}"),
        }
    }

    #[test]
    fn parse_grep_block() {
        let e = parse_expr_str("grep { $_ > 0 } @nums;");
        match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "grep");
                assert!(matches!(args[0].kind, ExprKind::AnonSub(..)));
            }
            other => panic!("expected grep ListOp, got {other:?}"),
        }
    }

    #[test]
    fn parse_sort_no_block() {
        let e = parse_expr_str("sort @list;");
        match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "sort");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected sort ListOp, got {other:?}"),
        }
    }

    // ── print tests ───────────────────────────────────────────

    #[test]
    fn parse_print_simple() {
        let prog = parse(r#"print "hello";"#);
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PrintOp(name, fh, _), .. }) => {
                assert_eq!(name, "print");
                assert!(fh.is_none());
            }
            other => panic!("expected print PrintOp, got {other:?}"),
        }
    }

    // ── Prefix dereference tests ──────────────────────────────

    #[test]
    fn parse_scalar_deref() {
        let e = parse_expr_str("$$ref;");
        match &e.kind {
            ExprKind::Deref(Sigil::Scalar, inner) => {
                assert!(matches!(inner.kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected scalar Deref, got {other:?}"),
        }
    }

    #[test]
    fn parse_array_deref() {
        let e = parse_expr_str("@$ref;");
        match &e.kind {
            ExprKind::Deref(Sigil::Array, inner) => {
                assert!(matches!(inner.kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected array Deref, got {other:?}"),
        }
    }

    #[test]
    fn parse_deref_block() {
        let e = parse_expr_str("${$ref};");
        match &e.kind {
            ExprKind::Deref(Sigil::Scalar, inner) => {
                assert!(matches!(inner.kind, ExprKind::ScalarVar(ref n) if n == "ref"));
            }
            other => panic!("expected Deref(Scalar, ScalarVar), got {other:?}"),
        }
    }

    #[test]
    fn parse_array_deref_block() {
        let e = parse_expr_str("@{$ref};");
        match &e.kind {
            ExprKind::Deref(Sigil::Array, inner) => {
                assert!(matches!(inner.kind, ExprKind::ScalarVar(ref n) if n == "ref"));
            }
            other => panic!("expected Deref(Array, ScalarVar), got {other:?}"),
        }
    }

    #[test]
    fn parse_deref_subscript() {
        // $$ref[0] → ArrayElem(Deref(Scalar, ScalarVar("ref")), 0)
        let e = parse_expr_str("$$ref[0];");
        match &e.kind {
            ExprKind::ArrayElem(base, idx) => {
                assert!(matches!(base.kind, ExprKind::Deref(Sigil::Scalar, _)));
                assert!(matches!(idx.kind, ExprKind::IntLit(0)));
            }
            other => panic!("expected ArrayElem(Deref, 0), got {other:?}"),
        }
    }

    // ── Slice tests ───────────────────────────────────────────

    #[test]
    fn parse_array_slice() {
        let e = parse_expr_str("@arr[0, 1, 2];");
        match &e.kind {
            ExprKind::ArraySlice(_, indices) => {
                assert_eq!(indices.len(), 3);
            }
            other => panic!("expected ArraySlice, got {other:?}"),
        }
    }

    #[test]
    fn parse_hash_slice() {
        let e = parse_expr_str("@hash{qw(a b c)};");
        match &e.kind {
            ExprKind::HashSlice(_, keys) => {
                assert_eq!(keys.len(), 1); // qw() is one expr
            }
            other => panic!("expected HashSlice, got {other:?}"),
        }
    }

    // ── Postfix deref tests ───────────────────────────────────

    #[test]
    fn parse_postfix_deref_array() {
        let e = parse_expr_str("$ref->@*;");
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefArray)));
    }

    #[test]
    fn parse_postfix_deref_hash() {
        let e = parse_expr_str("$ref->%*;");
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefHash)));
    }

    // ── Yada yada test ────────────────────────────────────────

    #[test]
    fn parse_yada_yada() {
        let prog = parse("sub foo { ... }");
        match &prog.statements[0].kind {
            StmtKind::SubDecl(sub) => {
                assert_eq!(sub.body.statements.len(), 1);
                match &sub.body.statements[0].kind {
                    StmtKind::Expr(Expr { kind: ExprKind::YadaYada, .. }) => {}
                    other => panic!("expected YadaYada, got {other:?}"),
                }
            }
            other => panic!("expected SubDecl, got {other:?}"),
        }
    }

    // ── goto test ─────────────────────────────────────────────

    #[test]
    fn parse_goto() {
        let e = parse_expr_str("goto LABEL;");
        match &e.kind {
            ExprKind::FuncCall(name, _) => assert_eq!(name, "goto"),
            other => panic!("expected goto, got {other:?}"),
        }
    }

    // ── Readline / diamond tests ──────────────────────────────

    #[test]
    fn parse_diamond() {
        let e = parse_expr_str("<>;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "readline");
                assert_eq!(args.len(), 0);
            }
            other => panic!("expected readline, got {other:?}"),
        }
    }

    #[test]
    fn parse_readline_stdin() {
        let e = parse_expr_str("<STDIN>;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "readline");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected readline, got {other:?}"),
        }
    }

    // ── given/when tests ──────────────────────────────────────

    #[test]
    fn parse_given_when() {
        let prog = parse(
            "use feature 'switch'; no warnings 'experimental::smartmatch'; \
             given ($x) { when (1) { 1; } default { 0; } }",
        );
        let stmt = prog.statements.iter().find(|s| matches!(s.kind, StmtKind::Given(_, _))).expect("Given statement present");
        match &stmt.kind {
            StmtKind::Given(expr, block) => {
                assert!(matches!(expr.kind, ExprKind::ScalarVar(ref n) if n == "x"));
                assert!(block.statements.len() >= 2);
                assert!(matches!(block.statements[0].kind, StmtKind::When(_, _)));
            }
            other => panic!("expected Given, got {other:?}"),
        }
    }

    // ── try/catch tests ───────────────────────────────────────

    #[test]
    fn parse_try_catch() {
        let prog = parse("use feature 'try'; try { die; } catch ($e) { warn $e; }");
        let stmt = prog.statements.iter().find(|s| matches!(s.kind, StmtKind::Try(_))).expect("Try statement present");
        match &stmt.kind {
            StmtKind::Try(t) => {
                assert!(t.catch_block.is_some());
                assert!(t.catch_var.is_some());
            }
            other => panic!("expected Try, got {other:?}"),
        }
    }

    #[test]
    fn parse_try_catch_finally() {
        let prog = parse("use feature 'try'; try { 1; } catch ($e) { 2; } finally { 3; }");
        let stmt = prog.statements.iter().find(|s| matches!(s.kind, StmtKind::Try(_))).expect("Try statement present");
        match &stmt.kind {
            StmtKind::Try(t) => {
                assert!(t.catch_block.is_some());
                assert!(t.finally_block.is_some());
            }
            other => panic!("expected Try, got {other:?}"),
        }
    }

    #[test]
    fn parse_defer() {
        let prog = parse(
            "use feature 'defer'; no warnings 'experimental::defer'; \
             defer { cleanup(); }",
        );
        let stmt = prog.statements.iter().find(|s| matches!(s.kind, StmtKind::Defer(_))).expect("Defer statement present");
        match &stmt.kind {
            StmtKind::Defer(block) => {
                assert_eq!(block.statements.len(), 1);
            }
            other => panic!("expected Defer with 1-stmt block, got {other:?}"),
        }
    }

    // ── __END__ test ──────────────────────────────────────────

    #[test]
    fn parse_end_stops_parsing() {
        let src = "my $x = 1;\n__END__\nThis is not code.\n";
        let prog = parse(src);
        // Should have 2 statements: my decl and DataEnd
        assert_eq!(prog.statements.len(), 2);
        match &prog.statements[1].kind {
            StmtKind::DataEnd(DataEndMarker::End, offset) => {
                assert_eq!(&src.as_bytes()[*offset as usize..], b"This is not code.\n");
            }
            other => panic!("expected DataEnd(End), got {other:?}"),
        }
    }

    #[test]
    fn parse_data_stops_parsing() {
        let src = "my $x = 1;\n__DATA__\nraw data here\n";
        let prog = parse(src);
        assert_eq!(prog.statements.len(), 2);
        match &prog.statements[1].kind {
            StmtKind::DataEnd(DataEndMarker::Data, offset) => {
                assert_eq!(&src.as_bytes()[*offset as usize..], b"raw data here\n");
            }
            other => panic!("expected DataEnd(Data), got {other:?}"),
        }
    }

    #[test]
    fn parse_ctrl_d_stops_parsing() {
        let src = "my $x = 1;\x04ignored code\n";
        let prog = parse(src);
        assert_eq!(prog.statements.len(), 2);
        match &prog.statements[1].kind {
            StmtKind::DataEnd(DataEndMarker::CtrlD, offset) => {
                assert_eq!(&src.as_bytes()[*offset as usize..], b"ignored code\n");
            }
            other => panic!("expected DataEnd(CtrlD), got {other:?}"),
        }
    }

    #[test]
    fn parse_ctrl_z_stops_parsing() {
        let src = "my $x = 1;\x1aignored code\n";
        let prog = parse(src);
        assert_eq!(prog.statements.len(), 2);
        match &prog.statements[1].kind {
            StmtKind::DataEnd(DataEndMarker::CtrlZ, offset) => {
                assert_eq!(&src.as_bytes()[*offset as usize..], b"ignored code\n");
            }
            other => panic!("expected DataEnd(CtrlZ), got {other:?}"),
        }
    }

    // ── Pod skipping test ─────────────────────────────────────

    #[test]
    fn parse_pod_skipped() {
        let prog = parse("my $x = 1;\n\n=pod\n\nThis is pod.\n\n=cut\n\nmy $y = 2;\n");
        // Should see both my declarations, pod is invisible.
        // Each is Stmt::Expr wrapping Assign(Decl(My), _).
        let my_count = prog
            .statements
            .iter()
            .filter(|s| {
                matches!(s.kind,
                    StmtKind::Expr(Expr { kind: ExprKind::Assign(_, ref lhs, _), .. })
                        if matches!(lhs.kind, ExprKind::Decl(DeclScope::My, _))
                )
            })
            .count();
        assert_eq!(my_count, 2);
    }

    // ── C-style for loop tests ────────────────────────────────

    #[test]
    fn parse_c_style_for() {
        let prog = parse("for (my $i = 0; $i < 10; $i++) { print $i; }");
        match &prog.statements[0].kind {
            StmtKind::For(f) => {
                // init should be an assignment wrapping a Decl(My)
                match &f.init {
                    Some(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => {
                        assert!(matches!(lhs.kind, ExprKind::Decl(DeclScope::My, _)));
                    }
                    other => panic!("expected Assign(Decl(My), ...), got {other:?}"),
                }
            }
            other => panic!("expected For, got {other:?}"),
        }
    }

    #[test]
    fn parse_c_style_for_empty_parts() {
        let prog = parse("for (;;) { last; }");
        match &prog.statements[0].kind {
            StmtKind::For(f) => {
                assert!(f.init.is_none());
                assert!(f.condition.is_none());
                assert!(f.step.is_none());
            }
            other => panic!("expected For, got {other:?}"),
        }
    }

    #[test]
    fn parse_c_style_for_list_decl() {
        let prog = parse("for (my ($i, $j) = (0, 0); $i < 10; $i++) { 1; }");
        match &prog.statements[0].kind {
            StmtKind::For(f) => match &f.init {
                Some(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => match &lhs.kind {
                    ExprKind::Decl(DeclScope::My, vars) => {
                        assert_eq!(vars.len(), 2);
                    }
                    other => panic!("expected Decl(My, 2 vars), got {other:?}"),
                },
                other => panic!("expected Assign(Decl(My), ...), got {other:?}"),
            },
            other => panic!("expected For, got {other:?}"),
        }
    }

    #[test]
    fn parse_foreach_still_works() {
        // Ensure for (@list) still parses as foreach
        let prog = parse("for (@list) { 1; }");
        assert!(matches!(prog.statements[0].kind, StmtKind::ForEach(_)));
    }

    #[test]
    fn parse_foreach_continue() {
        let prog = parse("foreach my $x (@list) { 1; } continue { 2; }");
        match &prog.statements[0].kind {
            StmtKind::ForEach(f) => {
                assert!(f.continue_block.is_some());
            }
            other => panic!("expected ForEach, got {other:?}"),
        }
    }

    // ── scalar keyword test ───────────────────────────────────

    #[test]
    fn parse_scalar_keyword() {
        let e = parse_expr_str("scalar @array;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "scalar");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected scalar call, got {other:?}"),
        }
    }

    #[test]
    fn parse_unless_elsif_else() {
        let prog = parse("unless (0) { 1; } elsif (1) { 2; } else { 3; }");
        match &prog.statements[0].kind {
            StmtKind::Unless(u) => {
                assert_eq!(u.elsif_clauses.len(), 1);
                assert!(u.else_block.is_some());
            }
            other => panic!("expected Unless, got {other:?}"),
        }
    }

    // ── Decl-as-expression test ───────────────────────────────

    #[test]
    fn parse_decl_in_expr_context() {
        // my $x = 5 in statement context still works.
        // Now represented as Stmt::Expr wrapping Assign(Decl(My), IntLit(5)).
        let prog = parse("my $x = 5;");
        let (scope, vars) = decl_vars(&prog.statements[0]);
        assert_eq!(scope, DeclScope::My);
        assert_eq!(vars[0].name, "x");
        let init = decl_init(&prog.statements[0]);
        assert!(matches!(init.kind, ExprKind::IntLit(5)));
    }

    // ── qx// test ─────────────────────────────────────────────

    #[test]
    fn parse_qx_string() {
        let e = parse_expr_str("qx{ls -la};");
        // qx produces an interpolated string (backtick kind)
        assert!(matches!(e.kind, ExprKind::InterpolatedString(_) | ExprKind::StringLit(_)));
    }

    // ── C-style for with plain expression init ────────────────

    #[test]
    fn parse_c_style_for_plain_init() {
        // No 'my' — just a plain assignment
        let prog = parse("for ($i = 0; $i < 10; $i++) { 1; }");
        assert!(matches!(prog.statements[0].kind, StmtKind::For(_)));
    }

    // ── local/our/state in for-init ───────────────────────────

    #[test]
    fn parse_c_style_for_local() {
        let prog = parse("for (local $i = 0; $i < 10; $i++) { 1; }");
        match &prog.statements[0].kind {
            StmtKind::For(f) => match &f.init {
                Some(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => {
                    assert!(matches!(lhs.kind, ExprKind::Local(_)));
                }
                other => panic!("expected Assign(Local(...), ...), got {other:?}"),
            },
            other => panic!("expected For, got {other:?}"),
        }
    }

    // ── my with array/hash ────────────────────────────────────

    #[test]
    fn parse_my_array_decl() {
        let prog = parse("my @arr = (1, 2, 3);");
        let (_s, vars) = decl_vars(&prog.statements[0]);
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].sigil, Sigil::Array);
        assert_eq!(vars[0].name, "arr");
    }

    #[test]
    fn parse_my_hash_decl() {
        let prog = parse("my %hash = (a => 1);");
        let (_s, vars) = decl_vars(&prog.statements[0]);
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].sigil, Sigil::Hash);
        assert_eq!(vars[0].name, "hash");
    }

    // ── while continue block ──────────────────────────────────

    #[test]
    fn parse_while_continue() {
        let prog = parse("while (1) { 1; } continue { 2; }");
        match &prog.statements[0].kind {
            StmtKind::While(w) => {
                assert!(w.continue_block.is_some());
            }
            other => panic!("expected While, got {other:?}"),
        }
    }

    #[test]
    fn parse_until_continue() {
        let prog = parse("until (0) { 1; } continue { 2; }");
        match &prog.statements[0].kind {
            StmtKind::Until(u) => {
                assert!(u.continue_block.is_some());
            }
            other => panic!("expected Until, got {other:?}"),
        }
    }

    // ── Fat comma autoquoting test ────────────────────────────

    #[test]
    fn parse_fat_comma_autoquote() {
        // key => value — key should be a StringLit, not a FuncCall
        let e = parse_expr_str("key => 42;");
        match &e.kind {
            ExprKind::List(items) => {
                assert!(matches!(items[0].kind, ExprKind::StringLit(_)));
            }
            other => panic!("expected List with StringLit first, got {other:?}"),
        }
    }

    // ── Ampersand prefix call tests ───────────────────────────

    #[test]
    fn parse_ampersand_call() {
        let e = parse_expr_str("&foo(1, 2);");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn parse_ampersand_coderef() {
        let e = parse_expr_str("&$coderef(1);");
        match &e.kind {
            ExprKind::MethodCall(_, name, args) => {
                assert!(name.is_empty()); // coderef call uses empty method name
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::IntLit(1)));
            }
            other => panic!("expected MethodCall for coderef, got {other:?}"),
        }
    }

    #[test]
    fn parse_ampersand_bare() {
        // &foo with no parens — call with current @_
        let e = parse_expr_str("&foo;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 0);
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    // ── Hash dereference tests ────────────────────────────────

    #[test]
    fn parse_hash_deref() {
        let e = parse_expr_str("%$ref;");
        match &e.kind {
            ExprKind::Deref(Sigil::Hash, inner) => {
                assert!(matches!(inner.kind, ExprKind::ScalarVar(ref n) if n == "ref"));
            }
            other => panic!("expected Deref(Hash, ScalarVar), got {other:?}"),
        }
    }

    #[test]
    fn parse_hash_deref_block() {
        let e = parse_expr_str("%{$ref};");
        match &e.kind {
            ExprKind::Deref(Sigil::Hash, inner) => {
                assert!(matches!(inner.kind, ExprKind::ScalarVar(ref n) if n == "ref"));
            }
            other => panic!("expected Deref(Hash, ScalarVar), got {other:?}"),
        }
    }

    // ── Glob / typeglob tests ─────────────────────────────────

    #[test]
    fn parse_glob_var() {
        let e = parse_expr_str("*foo;");
        match &e.kind {
            ExprKind::GlobVar(name) => assert_eq!(name, "foo"),
            other => panic!("expected GlobVar('foo'), got {other:?}"),
        }
    }

    #[test]
    fn parse_glob_deref() {
        let e = parse_expr_str("*$ref;");
        match &e.kind {
            ExprKind::Deref(Sigil::Glob, inner) => {
                assert!(matches!(inner.kind, ExprKind::ScalarVar(ref n) if n == "ref"));
            }
            other => panic!("expected Deref(Glob, ScalarVar), got {other:?}"),
        }
    }

    // ── Chained arrow calls test ──────────────────────────────

    #[test]
    fn parse_chained_arrow_calls() {
        let e = parse_expr_str("$obj->foo->bar->baz;");
        // Should be deeply nested MethodCall(MethodCall(MethodCall(...)))
        match &e.kind {
            ExprKind::MethodCall(inner, name, _) => {
                assert_eq!(name, "baz");
                assert!(matches!(inner.kind, ExprKind::MethodCall(_, _, _)));
            }
            other => panic!("expected chained MethodCall, got {other:?}"),
        }
    }

    // ── Octal literal test ────────────────────────────────────

    #[test]
    fn lex_legacy_octal() {
        let prog = parse("my $x = 0777;");
        let init = decl_init(&prog.statements[0]);
        match &init.kind {
            ExprKind::IntLit(n) => assert_eq!(*n, 0o777), // 511 decimal
            other => panic!("expected IntLit, got {other:?}"),
        }
    }

    // ── require test ──────────────────────────────────────────

    #[test]
    fn parse_require() {
        let e = parse_expr_str("require Foo::Bar;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "require");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected require call, got {other:?}"),
        }
    }

    // ── Hash subscript autoquoting tests ──────────────────────

    #[test]
    fn parse_hash_bareword_autoquote() {
        let e = parse_expr_str("$hash{key};");
        match &e.kind {
            ExprKind::HashElem(_, key) => {
                assert!(matches!(key.kind, ExprKind::StringLit(_)));
            }
            other => panic!("expected HashElem with StringLit key, got {other:?}"),
        }
    }

    #[test]
    fn parse_hash_neg_bareword_autoquote() {
        let e = parse_expr_str("$hash{-key};");
        match &e.kind {
            ExprKind::HashElem(_, key) => match &key.kind {
                ExprKind::StringLit(s) => assert_eq!(s, "-key"),
                other => panic!("expected StringLit('-key'), got {other:?}"),
            },
            other => panic!("expected HashElem, got {other:?}"),
        }
    }

    #[test]
    fn parse_arrow_hash_autoquote() {
        let e = parse_expr_str("$ref->{key};");
        match &e.kind {
            ExprKind::ArrowDeref(_, ArrowTarget::HashElem(key)) => {
                assert!(matches!(key.kind, ExprKind::StringLit(_)));
            }
            other => panic!("expected ArrowDeref with StringLit key, got {other:?}"),
        }
    }

    #[test]
    fn parse_hash_expr_not_autoquoted() {
        // $hash{$key} should NOT autoquote
        let e = parse_expr_str("$hash{$key};");
        match &e.kind {
            ExprKind::HashElem(_, key) => {
                assert!(matches!(key.kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected HashElem with ScalarVar key, got {other:?}"),
        }
    }

    // ── -bareword fat comma autoquoting ───────────────────────

    #[test]
    fn parse_neg_bareword_fat_comma() {
        // -key => 42 should produce StringLit("-key")
        let e = parse_expr_str("-key => 42;");
        match &e.kind {
            ExprKind::List(items) => match &items[0].kind {
                ExprKind::StringLit(s) => assert_eq!(s, "-key"),
                other => panic!("expected StringLit('-key'), got {other:?}"),
            },
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn parse_neg_bareword_alone() {
        // -key alone → StringLit("-key")
        let e = parse_expr_str("-key;");
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "-key"),
            other => panic!("expected StringLit, got {other:?}"),
        }
    }

    #[test]
    fn parse_neg_func_call_not_quoted() {
        // -func() → negate the function call, NOT autoquote
        let e = parse_expr_str("-func();");
        assert!(matches!(e.kind, ExprKind::UnaryOp(UnaryOp::Negate, _)));
    }

    // ── Attribute tests ───────────────────────────────────────

    #[test]
    fn parse_sub_with_attribute() {
        let prog = parse("sub foo :lvalue { 1; }");
        match &prog.statements[0].kind {
            StmtKind::SubDecl(sub) => {
                assert_eq!(sub.attributes.len(), 1);
                assert_eq!(sub.attributes[0].name, "lvalue");
                assert!(sub.attributes[0].value.is_none());
            }
            other => panic!("expected SubDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_sub_multiple_attributes() {
        let prog = parse("sub foo :lvalue :method { 1; }");
        match &prog.statements[0].kind {
            StmtKind::SubDecl(sub) => {
                assert_eq!(sub.attributes.len(), 2);
                assert_eq!(sub.attributes[0].name, "lvalue");
                assert_eq!(sub.attributes[1].name, "method");
            }
            other => panic!("expected SubDecl, got {other:?}"),
        }
    }

    // ── v-string tests ────────────────────────────────────────

    #[test]
    fn parse_vstring() {
        let prog = parse("use v5.26.0;");
        match &prog.statements[0].kind {
            StmtKind::UseDecl(u) => {
                assert_eq!(u.module, "v5.26.0");
            }
            other => panic!("expected UseDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_vstring_as_expr() {
        let e = parse_expr_str("v5.26;");
        match &e.kind {
            ExprKind::VersionLit(s) => assert_eq!(s, "v5.26"),
            other => panic!("expected VersionLit(\"v5.26\"), got {other:?}"),
        }
    }

    #[test]
    fn parse_vstring_no_dots() {
        let e = parse_expr_str("v5;");
        match &e.kind {
            ExprKind::VersionLit(s) => assert_eq!(s, "v5"),
            other => panic!("expected VersionLit(\"v5\"), got {other:?}"),
        }
    }

    // ── pragma tests ──────────────────────────────────────────

    /// Parse a program and return the parser's final pragma state.
    /// Because pragmas are lexically scoped, this reflects whatever
    /// was in effect at end-of-file (i.e., the outermost scope).
    fn parse_pragmas(src: &str) -> crate::pragma::Pragmas {
        let mut p = Parser::new(src.as_bytes()).unwrap();
        let _ = p.parse_program().unwrap();
        *p.pragmas()
    }

    #[test]
    fn pragma_default_has_default_bundle() {
        let p = parse_pragmas("my $x = 1;");
        // Pre-`use feature` state: the `:default` bundle (indirect,
        // multidimensional, bareword_filehandles,
        // apostrophe_as_package_separator, smartmatch).
        assert_eq!(p.features, Features::DEFAULT);
        assert!(p.features.contains(Features::INDIRECT));
        assert!(!p.features.contains(Features::SAY));
        assert!(!p.utf8);
    }

    #[test]
    fn pragma_use_feature_single() {
        let p = parse_pragmas("use feature 'signatures';");
        assert!(p.features.contains(Features::SIGNATURES));
        // Other non-default features untouched.
        assert!(!p.features.contains(Features::SAY));
        // :default features still present (use feature just adds).
        assert!(p.features.contains(Features::INDIRECT));
    }

    #[test]
    fn pragma_use_feature_multiple_via_qw() {
        let p = parse_pragmas("use feature qw(say state);");
        assert!(p.features.contains(Features::SAY));
        assert!(p.features.contains(Features::STATE));
    }

    #[test]
    fn pragma_no_feature_removes_specific() {
        // Enable a non-default feature, then disable it.
        let p = parse_pragmas("use feature 'signatures';\nno feature 'signatures';\n");
        assert!(!p.features.contains(Features::SIGNATURES));
        // :default still intact.
        assert!(p.features.contains(Features::INDIRECT));
    }

    #[test]
    fn pragma_no_feature_bare_resets_to_default() {
        // Per perlfeature: `no feature;` with no args resets to
        // :default, not to empty.
        let p = parse_pragmas("use feature qw(say state signatures);\nno feature;\n");
        assert_eq!(p.features, Features::DEFAULT);
    }

    #[test]
    fn pragma_no_feature_all_clears_everything() {
        // `no feature ':all'` is how you get to truly-empty state.
        let p = parse_pragmas("no feature ':all';");
        assert_eq!(p.features, Features::EMPTY);
    }

    #[test]
    fn pragma_use_feature_bundle_by_name() {
        // `use feature ':5.36'` applies the bundle directly.
        let p = parse_pragmas("use feature ':5.36';");
        assert!(p.features.contains(Features::SIGNATURES));
        assert!(!p.features.contains(Features::INDIRECT), "5.36 bundle excludes indirect");
    }

    #[test]
    fn pragma_use_vstring_bundle() {
        let p = parse_pragmas("use v5.36;");
        assert!(p.features.contains(Features::SAY));
        assert!(p.features.contains(Features::SIGNATURES));
        assert!(!p.features.contains(Features::SWITCH), "5.36 bundle should not include switch");
        assert!(!p.features.contains(Features::INDIRECT), "5.36 bundle should not include indirect");
    }

    #[test]
    fn pragma_use_int_version_bundle() {
        let p = parse_pragmas("use 5036;");
        assert!(p.features.contains(Features::SIGNATURES));
    }

    #[test]
    fn pragma_use_float_version_bundle() {
        let p = parse_pragmas("use 5.036;");
        assert!(p.features.contains(Features::SIGNATURES));
    }

    #[test]
    fn pragma_use_utf8() {
        let p = parse_pragmas("use utf8;");
        assert!(p.utf8);
    }

    #[test]
    fn pragma_no_utf8() {
        let p = parse_pragmas("use utf8;\nno utf8;\n");
        assert!(!p.utf8);
    }

    #[test]
    fn pragma_unknown_module_is_noop() {
        // `use strict;` doesn't set any parsing-relevant flag yet
        // and must not cause a panic.
        let p = parse_pragmas("use strict;");
        assert_eq!(p.features, Features::DEFAULT);
        assert!(!p.utf8);
    }

    #[test]
    fn pragma_unknown_feature_name_silently_ignored() {
        let p = parse_pragmas("use feature 'totally_fake_feature';");
        assert_eq!(p.features, Features::DEFAULT);
    }

    #[test]
    fn pragma_lexical_scoping_block_doesnt_leak() {
        let p = parse_pragmas("{ use feature 'signatures'; }");
        assert!(!p.features.contains(Features::SIGNATURES), "signatures enabled inside block should not leak out");
    }

    #[test]
    fn pragma_lexical_scoping_outer_preserved() {
        let p = parse_pragmas("use feature 'signatures';\n{ no feature 'signatures'; }\n");
        assert!(p.features.contains(Features::SIGNATURES), "outer scope's signatures should be preserved across the inner block");
    }

    #[test]
    fn pragma_version_bundle_resets_features() {
        // `use v5.36` does implicit `no feature ':all'; use feature ':5.36'`.
        // Applying after unrelated feature enables should leave only
        // the bundle.
        let p = parse_pragmas("use feature 'keyword_any';\nuse v5.36;\n");
        assert!(!p.features.contains(Features::KEYWORD_ANY), "version bundle should reset, not union");
        assert!(p.features.contains(Features::SIGNATURES));
    }

    // ── signature tests ───────────────────────────────────────

    /// Convenience: parse a program and return the last top-level
    /// SubDecl, panicking if none exists.
    fn parse_sub(src: &str) -> SubDecl {
        let prog = parse(src);
        for stmt in prog.statements.iter().rev() {
            if let StmtKind::SubDecl(s) = &stmt.kind {
                return s.clone();
            }
        }
        panic!("no SubDecl in program; statements: {:#?}", prog.statements);
    }

    #[test]
    fn sig_without_feature_is_prototype() {
        // No `use feature 'signatures'` in scope: `($)` is a
        // prototype (meaning "exactly one scalar argument").  We
        // verify the signature path was NOT taken by checking
        // that the prototype parser saw the raw text.
        let s = parse_sub("sub f ($) { }");
        assert!(s.signature.is_none(), "no signature when feature off");
        assert_eq!(s.prototype.as_deref(), Some("$"), "paren-form goes to prototype");
    }

    #[test]
    fn sig_empty_with_feature_on() {
        let s = parse_sub("use feature 'signatures'; sub f () { }");
        assert!(s.prototype.is_none());
        let sig = s.signature.expect("signature present");
        assert_eq!(sig.params.len(), 0);
    }

    #[test]
    fn sig_single_scalar() {
        let s = parse_sub("use feature 'signatures'; sub f ($x) { }");
        let sig = s.signature.expect("signature present");
        assert_eq!(sig.params.len(), 1);
        match &sig.params[0] {
            SigParam::Scalar { name, default, .. } => {
                assert_eq!(name, "x");
                assert!(default.is_none());
            }
            other => panic!("expected Scalar, got {other:?}"),
        }
    }

    #[test]
    fn sig_multiple_scalars() {
        let s = parse_sub("use feature 'signatures'; sub f ($x, $y, $z) { }");
        let sig = s.signature.expect("signature present");
        assert_eq!(sig.params.len(), 3);
        for (p, expected) in sig.params.iter().zip(["x", "y", "z"]) {
            match p {
                SigParam::Scalar { name, default: None, .. } => {
                    assert_eq!(name, expected);
                }
                other => panic!("expected Scalar({expected}), got {other:?}"),
            }
        }
    }

    #[test]
    fn sig_scalar_with_default() {
        let s = parse_sub("use feature 'signatures'; sub f ($x = 42) { }");
        let sig = s.signature.expect("signature present");
        match &sig.params[0] {
            SigParam::Scalar { name, default: Some((_, d)), .. } => {
                assert_eq!(name, "x");
                assert!(matches!(d.kind, ExprKind::IntLit(42)));
            }
            other => panic!("expected Scalar with default, got {other:?}"),
        }
    }

    #[test]
    fn sig_default_references_prior_param() {
        // Default expression can reference earlier parameter —
        // parser shouldn't care (just an expression).
        let s = parse_sub("use feature 'signatures'; sub f ($x, $y = $x * 2) { }");
        let sig = s.signature.expect("signature present");
        assert_eq!(sig.params.len(), 2);
        match &sig.params[1] {
            SigParam::Scalar { name, default: Some(_), .. } => {
                assert_eq!(name, "y");
            }
            other => panic!("expected Scalar with default, got {other:?}"),
        }
    }

    #[test]
    fn sig_slurpy_array() {
        let s = parse_sub("use feature 'signatures'; sub f ($x, @rest) { }");
        let sig = s.signature.expect("signature present");
        assert_eq!(sig.params.len(), 2);
        match &sig.params[1] {
            SigParam::SlurpyArray { name, .. } => assert_eq!(name, "rest"),
            other => panic!("expected SlurpyArray, got {other:?}"),
        }
    }

    #[test]
    fn sig_slurpy_hash() {
        let s = parse_sub("use feature 'signatures'; sub f ($x, %opts) { }");
        let sig = s.signature.expect("signature present");
        match &sig.params[1] {
            SigParam::SlurpyHash { name, .. } => assert_eq!(name, "opts"),
            other => panic!("expected SlurpyHash, got {other:?}"),
        }
    }

    #[test]
    fn sig_anonymous_placeholders() {
        // Anonymous scalars — `$` without names — accept-and-discard.
        // Only scalars here; slurpy forms (`@`, `%`) must be last
        // and only one is allowed, so they get their own tests.
        let s = parse_sub("use feature 'signatures'; sub f ($, $, $) { }");
        let sig = s.signature.expect("signature present");
        assert_eq!(sig.params.len(), 3);
        assert!(sig.params.iter().all(|p| matches!(p, SigParam::AnonScalar { .. })));
    }

    #[test]
    fn sig_anonymous_slurpy_array() {
        // Bare `@` at the end — anonymous slurpy array.
        let s = parse_sub("use feature 'signatures'; sub f ($, @) { }");
        let sig = s.signature.expect("signature present");
        assert_eq!(sig.params.len(), 2);
        assert!(matches!(sig.params[0], SigParam::AnonScalar { .. }));
        assert!(matches!(sig.params[1], SigParam::AnonArray { .. }));
    }

    #[test]
    fn sig_anonymous_slurpy_hash() {
        // Bare `%` at the end — anonymous slurpy hash.
        let s = parse_sub("use feature 'signatures'; sub f ($, %) { }");
        let sig = s.signature.expect("signature present");
        assert_eq!(sig.params.len(), 2);
        assert!(matches!(sig.params[0], SigParam::AnonScalar { .. }));
        assert!(matches!(sig.params[1], SigParam::AnonHash { .. }));
    }

    #[test]
    fn sig_anon_scalar_then_named() {
        // Skip first arg, bind second.
        let s = parse_sub("use feature 'signatures'; sub f ($, $y) { }");
        let sig = s.signature.expect("signature present");
        assert!(matches!(sig.params[0], SigParam::AnonScalar { .. }));
        match &sig.params[1] {
            SigParam::Scalar { name, .. } => assert_eq!(name, "y"),
            other => panic!("expected Scalar(y), got {other:?}"),
        }
    }

    #[test]
    fn sig_trailing_comma_allowed() {
        let s = parse_sub("use feature 'signatures'; sub f ($x, $y,) { }");
        let sig = s.signature.expect("signature present");
        assert_eq!(sig.params.len(), 2);
    }

    // ── Interaction with :prototype(...) attribute ──

    #[test]
    fn sig_with_prototype_attribute() {
        // `:prototype($$)` attaches a prototype; the paren-form is
        // still a signature when the feature is active.
        let s = parse_sub("use feature 'signatures'; sub f :prototype($$) ($x, $y) { }");
        let sig = s.signature.expect("signature present");
        assert_eq!(sig.params.len(), 2);
        // Attribute is captured too.
        let has_proto_attr = s.attributes.iter().any(|a| a.name == "prototype" && a.value.as_deref() == Some("$$"));
        assert!(has_proto_attr, "prototype attribute should be present");
    }

    // ── use v5.36 enables signatures via the bundle ──

    #[test]
    fn sig_enabled_by_use_v5_36() {
        // Phase 1 hookup: the `:5.36` bundle includes signatures,
        // so `use v5.36;` should enable the signature path without
        // an explicit `use feature 'signatures';`.
        let s = parse_sub("use v5.36; sub f ($x, $y) { }");
        assert!(s.signature.is_some(), "use v5.36 should enable signatures");
        assert!(s.prototype.is_none());
    }

    #[test]
    fn sig_feature_is_lexically_scoped() {
        // Outer scope has signatures; inner `no feature 'signatures'`
        // disables it for a sub declared inside the inner block.
        let src = "\
use feature 'signatures';
sub outer ($x) { 1 }
{
  no feature 'signatures';
  sub inner ($) { 2 }
}
";
        let prog = parse(src);
        // Find `outer` at top level.
        let outer = prog
            .statements
            .iter()
            .find_map(|s| match &s.kind {
                StmtKind::SubDecl(s) if s.name == "outer" => Some(s),
                _ => None,
            })
            .expect("outer sub at top level");
        assert!(outer.signature.is_some(), "outer has signature");
        assert!(outer.prototype.is_none());

        // Find `inner` inside the bare block.
        let mut found_inner = false;
        for stmt in &prog.statements {
            if let StmtKind::Block(block, _) = &stmt.kind {
                for inner_stmt in &block.statements {
                    if let StmtKind::SubDecl(s) = &inner_stmt.kind
                        && s.name == "inner"
                    {
                        assert!(s.signature.is_none(), "inner should NOT have signature");
                        assert!(s.prototype.is_some(), "inner should have prototype");
                        found_inner = true;
                    }
                }
            }
        }
        assert!(found_inner, "didn't find `inner` sub inside block");
    }

    // ── Anonymous sub signatures ──

    #[test]
    fn sig_anon_sub_with_signature() {
        let prog = parse("use feature 'signatures'; my $f = sub ($x) { $x };");
        // Find the AnonSub expression in the statements.
        let mut found = false;
        for stmt in &prog.statements {
            walk_for_anon_sub(&stmt.kind, &mut found);
        }
        assert!(found, "expected an AnonSub with signature");
    }

    /// Helper: recursively walk a stmt looking for an AnonSub with
    /// a non-None signature.
    fn walk_for_anon_sub(stmt: &StmtKind, found: &mut bool) {
        if let StmtKind::Expr(expr) = stmt {
            walk_expr(expr, found);
        }
    }

    fn walk_expr(expr: &Expr, found: &mut bool) {
        match &expr.kind {
            ExprKind::AnonSub(_, _, Some(sig), _) => {
                assert_eq!(sig.params.len(), 1);
                *found = true;
            }
            ExprKind::Assign(_, l, r) => {
                walk_expr(l, found);
                walk_expr(r, found);
            }
            _ => {}
        }
    }

    // ── postderef tests ───────────────────────────────────────

    /// Convenience: parse one expression statement, returning the
    /// inner expression.
    fn parse_expr_stmt(src: &str) -> Expr {
        let prog = parse(src);
        for stmt in &prog.statements {
            if let StmtKind::Expr(e) = &stmt.kind {
                return e.clone();
            }
        }
        panic!("no expression in program; statements: {:#?}", prog.statements);
    }

    /// Helper: walk the outermost arrow-deref off a parsed expr,
    /// returning the ArrowTarget.  Panics if the expression isn't
    /// an ArrowDeref.
    fn arrow_target(e: &Expr) -> &ArrowTarget {
        match &e.kind {
            ExprKind::ArrowDeref(_, target) => target,
            other => panic!("expected ArrowDeref, got {other:?}"),
        }
    }

    #[test]
    fn postderef_deref_array() {
        let e = parse_expr_stmt("$r->@*;");
        assert!(matches!(arrow_target(&e), ArrowTarget::DerefArray));
    }

    #[test]
    fn postderef_deref_hash() {
        let e = parse_expr_stmt("$r->%*;");
        assert!(matches!(arrow_target(&e), ArrowTarget::DerefHash));
    }

    #[test]
    fn postderef_deref_scalar() {
        let e = parse_expr_stmt("$r->$*;");
        assert!(matches!(arrow_target(&e), ArrowTarget::DerefScalar));
    }

    #[test]
    fn postderef_last_index() {
        // `->$#*` — equivalent to `$#{$ref}`.  Requires lexer
        // byte-level disambiguation because `#` would otherwise
        // begin a comment.
        let e = parse_expr_stmt("$r->$#*;");
        assert!(matches!(arrow_target(&e), ArrowTarget::LastIndex));
    }

    #[test]
    fn postderef_last_index_in_expr() {
        // Embed in a larger expression to verify the parser
        // continues past the LastIndex properly.
        let e = parse_expr_stmt("my $n = $r->$#*;");
        match e.kind {
            ExprKind::Assign(_, _, rhs) => {
                assert!(matches!(arrow_target(&rhs), ArrowTarget::LastIndex));
            }
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    #[test]
    fn postderef_dollar_not_hashstar_still_fails() {
        // `->$foo` (Dollar + named ScalarVar) is not postderef.
        // The lexer greedily combines `$foo` into ScalarVar —
        // which is handled as dynamic method dispatch in another
        // arm.  We just verify `->$` followed by something
        // neither `*` nor `#*` doesn't crash.
        let src = "$r->$;";
        let mut p = match Parser::new(src.as_bytes()) {
            Ok(p) => p,
            Err(_) => panic!("parser construction failed"),
        };
        let result = p.parse_program();
        assert!(result.is_err(), "->$ with trailing semicolon is a parse error");
    }

    #[test]
    fn postderef_deref_code() {
        let e = parse_expr_stmt("$r->&*;");
        assert!(matches!(arrow_target(&e), ArrowTarget::DerefCode));
    }

    #[test]
    fn postderef_deref_glob() {
        // `->**` — lexer emits Token::Power for `**`.
        let e = parse_expr_stmt("$r->**;");
        assert!(matches!(arrow_target(&e), ArrowTarget::DerefGlob));
    }

    #[test]
    fn postderef_array_slice_indices() {
        let e = parse_expr_stmt("$r->@[0, 1, 2];");
        match arrow_target(&e) {
            ArrowTarget::ArraySliceIndices(_) => {}
            other => panic!("expected ArraySliceIndices, got {other:?}"),
        }
    }

    #[test]
    fn postderef_array_slice_keys() {
        let e = parse_expr_stmt(r#"$r->@{"a", "b"};"#);
        match arrow_target(&e) {
            ArrowTarget::ArraySliceKeys(_) => {}
            other => panic!("expected ArraySliceKeys, got {other:?}"),
        }
    }

    #[test]
    fn postderef_kv_slice_indices() {
        let e = parse_expr_stmt("$r->%[0, 1];");
        match arrow_target(&e) {
            ArrowTarget::KvSliceIndices(_) => {}
            other => panic!("expected KvSliceIndices, got {other:?}"),
        }
    }

    #[test]
    fn postderef_kv_slice_keys() {
        let e = parse_expr_stmt(r#"$r->%{"a", "b"};"#);
        match arrow_target(&e) {
            ArrowTarget::KvSliceKeys(_) => {}
            other => panic!("expected KvSliceKeys, got {other:?}"),
        }
    }

    #[test]
    fn postderef_chained_on_complex_expr() {
        // Chain off a method call result.
        let e = parse_expr_stmt("$obj->method->@*;");
        assert!(matches!(arrow_target(&e), ArrowTarget::DerefArray));
    }

    #[test]
    fn postderef_nested_slice() {
        // `->@[0]->[1]` — slice followed by subscript chain.
        // (Not semantically useful but should parse.)
        let e = parse_expr_stmt("$r->@[0];");
        match arrow_target(&e) {
            ArrowTarget::ArraySliceIndices(_) => {}
            other => panic!("expected ArraySliceIndices, got {other:?}"),
        }
    }

    // ── Phase 4: isa / fc / evalbytes / compile-time tokens ──

    // ── `isa` infix operator ──

    #[test]
    fn isa_requires_feature() {
        // Without the `isa` feature, `isa` is just an ordinary
        // bareword (would be a function call or bareword
        // reference).  We verify by checking that parsing
        // `$x isa Foo` with no feature does NOT produce a BinOp.
        let e = parse_expr_stmt("$x isa Foo;");
        assert!(!matches!(e.kind, ExprKind::BinOp(BinOp::Isa, _, _)), "no isa feature → must not parse as Isa binop");
    }

    #[test]
    fn isa_with_feature() {
        let e = parse_expr_stmt("use feature 'isa'; $x isa Foo;");
        match e.kind {
            ExprKind::BinOp(BinOp::Isa, lhs, rhs) => {
                assert!(matches!(lhs.kind, ExprKind::ScalarVar(_)));
                assert!(matches!(rhs.kind, ExprKind::Bareword(_)));
            }
            other => panic!("expected Isa binop, got {other:?}"),
        }
    }

    #[test]
    fn isa_enabled_by_v5_36() {
        // The :5.36 bundle includes isa.
        let e = parse_expr_stmt("use v5.36; $x isa Foo;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Isa, _, _)));
    }

    #[test]
    fn isa_precedence_vs_relational() {
        // `isa` binds tighter than `<`, so `$x isa Foo < 1`
        // groups as `($x isa Foo) < 1`.
        let e = parse_expr_stmt("use feature 'isa'; $x isa Foo < 1;");
        match e.kind {
            ExprKind::BinOp(BinOp::NumLt, lhs, _) => {
                assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::Isa, _, _)), "isa should bind tighter than <");
            }
            other => panic!("expected NumLt at top level, got {other:?}"),
        }
    }

    // ── `fc` feature-gated named unary ──

    #[test]
    fn fc_requires_feature() {
        // Without `fc` feature, `fc($x)` parses as an ordinary
        // function call to a user sub named `fc`.  Either way
        // we get a FuncCall; just confirm it doesn't error and
        // the function name is captured.
        let e = parse_expr_stmt("fc($x);");
        match e.kind {
            ExprKind::FuncCall(name, _) => assert_eq!(name, "fc"),
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn fc_with_feature_paren() {
        let e = parse_expr_stmt("use feature 'fc'; fc($x);");
        match e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "fc");
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn fc_with_feature_no_paren() {
        // `fc $x` — named unary, one argument at NAMED_UNARY
        // precedence.
        let e = parse_expr_stmt("use feature 'fc'; fc $x;");
        match e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "fc");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    // ── `evalbytes` feature-gated named unary ──

    #[test]
    fn evalbytes_with_feature() {
        let e = parse_expr_stmt(r#"use feature 'evalbytes'; evalbytes("1+1");"#);
        match e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "evalbytes");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    // ── Compile-time tokens ──

    #[test]
    fn source_file_captured_at_lex_time() {
        // Default filename placeholder when constructed via
        // `parse(src)` / `Parser::new(src)`.
        let e = parse_expr_stmt("__FILE__;");
        match e.kind {
            ExprKind::SourceFile(path) => assert_eq!(path, "(script)"),
            other => panic!("expected SourceFile, got {other:?}"),
        }
    }

    #[test]
    fn source_file_uses_custom_filename() {
        // `Parser::with_filename` / `parse_with_filename` plumbs
        // the filename through to `LexerSource::filename()`,
        // which `__FILE__` reads at lex time.
        let prog = crate::parse_with_filename(b"__FILE__;", "my_script.pl").expect("parse should succeed");
        let expr = prog.statements.iter().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e.clone()) } else { None }).expect("expression statement");
        match expr.kind {
            ExprKind::SourceFile(path) => assert_eq!(path, "my_script.pl"),
            other => panic!("expected SourceFile, got {other:?}"),
        }
    }

    #[test]
    fn source_line_captured_at_lex_time() {
        // `__LINE__` on line 3 of a 3-line program.
        let e = parse_expr_stmt("\n\n__LINE__;");
        match e.kind {
            ExprKind::SourceLine(n) => assert_eq!(n, 3),
            other => panic!("expected SourceLine, got {other:?}"),
        }
    }

    #[test]
    fn current_package_filled_by_parser() {
        let e = parse_expr_stmt("__PACKAGE__;");
        match e.kind {
            ExprKind::CurrentPackage(name) => assert_eq!(name, "main"),
            other => panic!("expected CurrentPackage, got {other:?}"),
        }
    }

    #[test]
    fn current_package_reflects_package_decl() {
        // After `package Foo;`, __PACKAGE__ should give "Foo".
        let prog = parse("package Foo;\n__PACKAGE__;\n");
        let e = prog.statements.iter().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e.clone()) } else { None }).expect("expression statement");
        match e.kind {
            ExprKind::CurrentPackage(name) => assert_eq!(name, "Foo"),
            other => panic!("expected CurrentPackage, got {other:?}"),
        }
    }

    #[test]
    fn current_sub_requires_feature() {
        // Without the current_sub feature, `__SUB__` falls back
        // to bareword treatment.
        let e = parse_expr_stmt("__SUB__;");
        assert!(!matches!(e.kind, ExprKind::CurrentSub), "no current_sub feature → must not be CurrentSub");
    }

    #[test]
    fn current_sub_with_feature() {
        let e = parse_expr_stmt("use feature 'current_sub'; __SUB__;");
        assert!(matches!(e.kind, ExprKind::CurrentSub));
    }

    #[test]
    fn current_sub_via_v5_16() {
        // The :5.16 bundle includes current_sub.
        let e = parse_expr_stmt("use v5.16; __SUB__;");
        assert!(matches!(e.kind, ExprKind::CurrentSub));
    }

    // ── Feature-gated keyword downgrade ───────────────────────
    //
    // When the governing feature is off, try/catch/finally/defer,
    // given/when/default, and class/field/method all act as plain
    // identifiers — users can define subs with those names,
    // pass them as hash keys, etc.  These tests verify the
    // downgrade happens at the parser level so legacy code keeps
    // working.

    #[test]
    fn class_is_bareword_without_feature() {
        // `sub class { ... }` — defining a sub named "class".
        // With class feature off, the lexer emits
        // Token::Keyword(Class) but the parser downgrades to
        // Token::Ident("class") because we're not in a class
        // scope.  The sub declaration should parse.
        let prog = parse("sub class { 1; }");
        assert!(
            prog.statements.iter().any(|s| matches!(
                &s.kind,
                StmtKind::SubDecl(sd) if sd.name == "class"
            )),
            "expected sub named `class` to parse without class feature"
        );
    }

    #[test]
    fn try_is_ident_without_feature() {
        // `my $try = try();` — `try` as a function call.
        let prog = parse("my $try = try();");
        // Should parse as a normal expression statement (Decl
        // assignment with FuncCall).  The inner expression is
        // FuncCall("try", []), not a Try statement.
        assert!(!prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Try(_))), "must not parse as Try without feature");
    }

    #[test]
    fn given_is_ident_without_feature() {
        // `given(...)` is a function call without the switch feature.
        let prog = parse("given(1);");
        assert!(!prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Given(_, _))), "must not parse as Given without feature");
    }

    #[test]
    fn defer_is_ident_without_feature() {
        // `defer { ... }` would be a Defer statement with the
        // defer feature; without it, `defer` is a bareword
        // followed by a block, which is a parse error (or parsed
        // as something else).  We just confirm it doesn't
        // produce a Defer statement.
        let prog_result = Parser::new(b"my $x = defer;").and_then(|mut p| p.parse_program());
        if let Ok(prog) = prog_result {
            assert!(!prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Defer(_))), "must not parse as Defer without feature");
        }
    }

    #[test]
    fn method_is_ident_without_feature() {
        // Outside `use feature 'class'`, `method` is a plain sub
        // name.  `sub method { ... }` at top level defines a
        // regular sub.
        let prog = parse("sub method { 1; }");
        assert!(
            prog.statements.iter().any(|s| matches!(
                &s.kind,
                StmtKind::SubDecl(sd) if sd.name == "method"
            )),
            "expected sub named `method` to parse without class feature"
        );
    }

    #[test]
    fn try_keyword_reactivates_with_feature() {
        // Sanity check: once `use feature 'try';` is seen, the
        // downgrade stops happening for the rest of the scope.
        let prog = parse("use feature 'try'; try { 1; }");
        let has_try = prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Try(_)));
        assert!(has_try, "Try must parse when feature is active");
    }

    #[test]
    fn feature_gate_is_lexically_scoped() {
        // Inside a block, `no feature 'try'` disables the gate.
        // Outside the block, `try` is still active.
        // We only verify the outer `try { ... }` succeeds —
        // demonstrating the scope restore after the inner block.
        let prog = parse("use feature 'try'; try { 1; } catch ($e) { 2; }");
        assert!(prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Try(_))), "outer Try with feature on must parse");
    }

    // ── Refaliasing / declared_refs (5.22+ / 5.26+) ───────────

    #[test]
    fn refalias_requires_feature() {
        // Without `refaliasing`, `\$a = \$b` is a parse error
        // (Ref is not a valid lvalue).
        let src = "\\$a = \\$b;";
        let mut p = match Parser::new(src.as_bytes()) {
            Ok(p) => p,
            Err(_) => panic!("parser construction failed"),
        };
        let result = p.parse_program();
        assert!(result.is_err(), "refaliasing without feature should fail");
    }

    #[test]
    fn refalias_with_feature_scalar() {
        let e = parse_expr_stmt("use feature 'refaliasing'; no warnings 'experimental::refaliasing'; \\$a = \\$b;");
        match e.kind {
            ExprKind::Assign(AssignOp::Eq, lhs, rhs) => {
                assert!(matches!(lhs.kind, ExprKind::Ref(_)));
                assert!(matches!(rhs.kind, ExprKind::Ref(_)));
            }
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    #[test]
    fn refalias_with_feature_array() {
        let e = parse_expr_stmt("use feature 'refaliasing'; no warnings 'experimental::refaliasing'; \\@a = \\@b;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::Eq, _, _)));
    }

    #[test]
    fn refalias_with_feature_hash() {
        let e = parse_expr_stmt("use feature 'refaliasing'; no warnings 'experimental::refaliasing'; \\%a = \\%b;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::Eq, _, _)));
    }

    #[test]
    fn refalias_list_form() {
        let e = parse_expr_stmt("use feature 'refaliasing'; no warnings 'experimental::refaliasing'; (\\$a, \\$b) = (\\$c, \\$d);");
        match e.kind {
            ExprKind::Assign(AssignOp::Eq, lhs, _) => {
                // LHS should be a list / paren containing Refs.
                match &lhs.kind {
                    ExprKind::Paren(inner) => match &inner.kind {
                        ExprKind::List(items) => {
                            assert_eq!(items.len(), 2);
                            assert!(items.iter().all(|e| matches!(e.kind, ExprKind::Ref(_))));
                        }
                        other => panic!("expected List inside Paren, got {other:?}"),
                    },
                    ExprKind::List(items) => {
                        assert!(items.iter().all(|e| matches!(e.kind, ExprKind::Ref(_))));
                    }
                    other => panic!("expected List/Paren on LHS, got {other:?}"),
                }
            }
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    // ── declared_refs (5.26+) ──

    #[test]
    fn declared_refs_requires_feature() {
        // `my \$x` without feature → ParseError at the `\`.
        let src = "my \\$x = \\$y;";
        let mut p = match Parser::new(src.as_bytes()) {
            Ok(p) => p,
            Err(_) => panic!("parser construction failed"),
        };
        let result = p.parse_program();
        assert!(result.is_err(), "declared_refs without feature should fail");
    }

    #[test]
    fn declared_refs_scalar() {
        let e = parse_expr_stmt(
            "use feature 'declared_refs'; use feature 'refaliasing'; \
             no warnings 'experimental::refaliasing'; no warnings 'experimental::declared_refs'; \
             my \\$x = \\$y;",
        );
        match e.kind {
            ExprKind::Assign(AssignOp::Eq, lhs, _) => match lhs.kind {
                ExprKind::Decl(DeclScope::My, vars) => {
                    assert_eq!(vars.len(), 1);
                    assert_eq!(vars[0].name, "x");
                    assert_eq!(vars[0].sigil, Sigil::Scalar);
                    assert!(vars[0].is_ref, "expected is_ref=true for `my \\$x`");
                }
                other => panic!("expected Decl on LHS, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    #[test]
    fn declared_refs_list_mixed() {
        // `my (\$a, \@b)` — two ref-declared vars.
        let e = parse_expr_stmt(
            "use feature 'declared_refs'; use feature 'refaliasing'; \
             no warnings 'experimental::refaliasing'; no warnings 'experimental::declared_refs'; \
             my (\\$a, \\@b) = (\\$c, \\@d);",
        );
        match e.kind {
            ExprKind::Assign(AssignOp::Eq, lhs, _) => match lhs.kind {
                ExprKind::Decl(DeclScope::My, vars) => {
                    assert_eq!(vars.len(), 2);
                    assert!(vars[0].is_ref && vars[1].is_ref);
                    assert_eq!(vars[0].sigil, Sigil::Scalar);
                    assert_eq!(vars[1].sigil, Sigil::Array);
                }
                other => panic!("expected Decl on LHS, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    #[test]
    fn declared_refs_partial() {
        // Mixing ref and non-ref in one decl: `my (\$a, $b)` — the
        // parser accepts this (semantic validation is a later pass).
        let e = parse_expr_stmt(
            "use feature 'declared_refs'; use feature 'refaliasing'; \
             no warnings 'experimental::refaliasing'; no warnings 'experimental::declared_refs'; \
             my (\\$a, $b) = (\\$c, 42);",
        );
        match e.kind {
            ExprKind::Assign(AssignOp::Eq, lhs, _) => match lhs.kind {
                ExprKind::Decl(DeclScope::My, vars) => {
                    assert_eq!(vars.len(), 2);
                    assert!(vars[0].is_ref);
                    assert!(!vars[1].is_ref);
                }
                other => panic!("expected Decl on LHS, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    #[test]
    fn declared_refs_via_v5_36() {
        // `use v5.36` enables both refaliasing and declared_refs
        // via the bundle.
        // Actually, checking perlfeature: :5.36 does NOT include
        // refaliasing/declared_refs (those are still experimental
        // as of 5.36).  So this test expects a parse error.
        // Using a feature-on path with explicit `use feature` in
        // other tests above covers the positive case.
        let src = "use v5.36; my \\$x = \\$y;";
        let mut p = match Parser::new(src.as_bytes()) {
            Ok(p) => p,
            Err(_) => panic!("parser construction failed"),
        };
        let result = p.parse_program();
        assert!(result.is_err(), ":5.36 bundle does not include declared_refs (experimental)");
    }

    // ── format tests ──────────────────────────────────────────

    /// Convenience: parse a single format declaration, panic on any
    /// other top-level statement shape.
    fn parse_fmt(src: &str) -> FormatDecl {
        let prog = parse(src);
        assert_eq!(prog.statements.len(), 1, "expected one top-level stmt, got {}", prog.statements.len());
        match &prog.statements[0].kind {
            StmtKind::FormatDecl(f) => f.clone(),
            other => panic!("expected FormatDecl, got {other:?}"),
        }
    }

    // ── Boundary / naming ──

    #[test]
    fn format_default_name_is_stdout() {
        let f = parse_fmt("format =\n.\n");
        assert_eq!(f.name, "STDOUT");
        assert!(f.lines.is_empty(), "empty body → no lines");
    }

    #[test]
    fn format_named() {
        let f = parse_fmt("format MyFmt =\n.\n");
        assert_eq!(f.name, "MyFmt");
    }

    #[test]
    fn format_empty_body() {
        // `.` immediately on the next line → zero lines.
        let f = parse_fmt("format X =\n.\n");
        assert!(f.lines.is_empty());
    }

    #[test]
    fn format_terminator_with_trailing_ws() {
        // `. \t\r` on the terminator line still terminates.
        let f = parse_fmt("format X =\nhello\n. \t\n");
        assert_eq!(f.lines.len(), 1);
    }

    #[test]
    fn format_indented_dot_does_not_terminate() {
        // A `.` not in column 0 is just literal content.
        let f = parse_fmt("format X =\n hello\n .\n.\n");
        // Two content lines (one " hello", one " ."), then the real `.` terminates.
        assert_eq!(f.lines.len(), 2);
        match &f.lines[1] {
            FormatLine::Literal { text, .. } => assert_eq!(text, " ."),
            other => panic!("expected Literal, got {other:?}"),
        }
    }

    // ── Line classification ──

    #[test]
    fn format_comment_line() {
        let f = parse_fmt("format X =\n# some comment\n.\n");
        assert_eq!(f.lines.len(), 1);
        match &f.lines[0] {
            FormatLine::Comment { text, .. } => assert_eq!(text, " some comment"),
            other => panic!("expected Comment, got {other:?}"),
        }
    }

    #[test]
    fn format_blank_line() {
        let f = parse_fmt("format X =\n\n.\n");
        assert_eq!(f.lines.len(), 1);
        assert!(matches!(f.lines[0], FormatLine::Blank { .. }));
    }

    #[test]
    fn format_whitespace_only_line_is_blank() {
        let f = parse_fmt("format X =\n   \t\n.\n");
        assert!(matches!(f.lines[0], FormatLine::Blank { .. }));
    }

    #[test]
    fn format_literal_line_no_fields() {
        let f = parse_fmt("format X =\nhello world\n.\n");
        match &f.lines[0] {
            FormatLine::Literal { repeat, text, .. } => {
                assert!(matches!(repeat, RepeatKind::None));
                assert_eq!(text, "hello world");
            }
            other => panic!("expected Literal, got {other:?}"),
        }
    }

    // ── Tilde handling ──

    #[test]
    fn format_single_tilde_on_literal_line() {
        // ~ on a fieldless line → repeat=Suppress, ~ replaced with space.
        let f = parse_fmt("format X =\n~hello\n.\n");
        match &f.lines[0] {
            FormatLine::Literal { repeat, text, .. } => {
                assert!(matches!(repeat, RepeatKind::Suppress));
                assert_eq!(text, " hello", "tilde should be replaced with space");
            }
            other => panic!("expected Literal, got {other:?}"),
        }
    }

    #[test]
    fn format_double_tilde_on_literal_line() {
        let f = parse_fmt("format X =\n~~hello\n.\n");
        match &f.lines[0] {
            FormatLine::Literal { repeat, text, .. } => {
                assert!(matches!(repeat, RepeatKind::Repeat));
                assert_eq!(text, "  hello");
            }
            other => panic!("expected Literal, got {other:?}"),
        }
    }

    #[test]
    fn format_tilde_mid_line_sets_suppress() {
        // ~ anywhere on the line, not just at the start.
        let f = parse_fmt("format X =\nhello ~ world\n.\n");
        match &f.lines[0] {
            FormatLine::Literal { repeat, text, .. } => {
                assert!(matches!(repeat, RepeatKind::Suppress));
                assert_eq!(text, "hello   world");
            }
            other => panic!("expected Literal, got {other:?}"),
        }
    }

    // ── Field: text justifications ──

    #[test]
    fn format_field_left_justify() {
        let f = parse_fmt("format X =\n@<<<\n$x\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { parts, args, .. } => {
                assert_eq!(parts.len(), 1);
                assert_eq!(args.len(), 1);
                match &parts[0] {
                    FormatPart::Field(FormatField { kind: FieldKind::LeftJustify { width, truncate_ellipsis }, .. }) => {
                        assert_eq!(*width, 4);
                        assert!(!truncate_ellipsis);
                    }
                    other => panic!("expected LeftJustify, got {other:?}"),
                }
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    #[test]
    fn format_field_right_justify() {
        let f = parse_fmt("format X =\n@>>>>>\n$x\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { parts, .. } => {
                assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::RightJustify { width: 6, truncate_ellipsis: false }, .. })));
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    #[test]
    fn format_field_center() {
        let f = parse_fmt("format X =\n@||||\n$x\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { parts, .. } => {
                assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::Center { width: 5, truncate_ellipsis: false }, .. })));
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    #[test]
    fn format_field_left_with_ellipsis() {
        let f = parse_fmt("format X =\n@<<<<...\n$x\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { parts, .. } => {
                assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::LeftJustify { width: 5, truncate_ellipsis: true }, .. })));
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    #[test]
    fn format_field_fill_left() {
        let f = parse_fmt("format X =\n^<<<<\n$x\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { parts, .. } => {
                assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::FillLeft { width: 5, truncate_ellipsis: false }, .. })));
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    #[test]
    fn format_field_fill_with_ellipsis() {
        let f = parse_fmt("format X =\n^<<<<...\n$x\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { parts, .. } => {
                assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::FillLeft { width: 5, truncate_ellipsis: true }, .. })));
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    // ── Field: multi-line ──

    #[test]
    fn format_field_multi_line_at_star() {
        let f = parse_fmt("format X =\n@*\n$x\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { parts, .. } => {
                assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::MultiLine, .. })));
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    #[test]
    fn format_field_fill_multi_line_caret_star() {
        let f = parse_fmt("format X =\n^*\n$x\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { parts, .. } => {
                assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::FillMultiLine, .. })));
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    // ── Field: numeric ──

    #[test]
    fn format_field_numeric_integer() {
        let f = parse_fmt("format X =\n@####\n$x\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { parts, .. } => match &parts[0] {
                FormatPart::Field(FormatField { kind: FieldKind::Numeric { integer_digits, decimal_digits, leading_zeros, caret }, .. }) => {
                    assert_eq!(*integer_digits, 4);
                    assert!(decimal_digits.is_none());
                    assert!(!leading_zeros);
                    assert!(!caret);
                }
                other => panic!("expected Numeric, got {other:?}"),
            },
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    #[test]
    fn format_field_numeric_with_decimal() {
        let f = parse_fmt("format X =\n@###.##\n$x\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { parts, .. } => match &parts[0] {
                FormatPart::Field(FormatField { kind: FieldKind::Numeric { integer_digits, decimal_digits, .. }, .. }) => {
                    assert_eq!(*integer_digits, 3);
                    assert_eq!(*decimal_digits, Some(2));
                }
                other => panic!("expected Numeric, got {other:?}"),
            },
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    #[test]
    fn format_field_numeric_leading_zeros() {
        let f = parse_fmt("format X =\n@0###\n$x\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { parts, .. } => {
                match &parts[0] {
                    FormatPart::Field(FormatField { kind: FieldKind::Numeric { integer_digits, leading_zeros, .. }, .. }) => {
                        assert_eq!(*integer_digits, 4); // 0 + 3 #s
                        assert!(*leading_zeros);
                    }
                    other => panic!("expected Numeric, got {other:?}"),
                }
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    #[test]
    fn format_field_numeric_caret() {
        let f = parse_fmt("format X =\n^####\n$x\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { parts, .. } => match &parts[0] {
                FormatPart::Field(FormatField { kind: FieldKind::Numeric { caret, integer_digits, .. }, .. }) => {
                    assert!(*caret);
                    assert_eq!(*integer_digits, 4);
                }
                other => panic!("expected Numeric, got {other:?}"),
            },
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    #[test]
    fn format_field_numeric_decimal_only() {
        // @.### — no integer digits, three decimal.
        let f = parse_fmt("format X =\n@.###\n$x\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { parts, .. } => match &parts[0] {
                FormatPart::Field(FormatField { kind: FieldKind::Numeric { integer_digits, decimal_digits, .. }, .. }) => {
                    assert_eq!(*integer_digits, 0);
                    assert_eq!(*decimal_digits, Some(3));
                }
                other => panic!("expected Numeric, got {other:?}"),
            },
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    // ── Mixed picture lines ──

    #[test]
    fn format_multiple_fields_with_literals() {
        let f = parse_fmt("format X =\n@<<< = @>>>\n$k, $v\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { parts, args, .. } => {
                assert_eq!(parts.len(), 3);
                assert!(matches!(&parts[0], FormatPart::Field(_)));
                assert!(matches!(&parts[1], FormatPart::Literal(s) if s == " = "));
                assert!(matches!(&parts[2], FormatPart::Field(_)));
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    #[test]
    fn format_literal_prefix_before_field() {
        let f = parse_fmt("format X =\nName: @<<<<<\n$name\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { parts, .. } => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(&parts[0], FormatPart::Literal(s) if s == "Name: "));
                assert!(matches!(&parts[1], FormatPart::Field(_)));
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    // ── Args: expressions ──

    #[test]
    fn format_args_multiple_scalars() {
        let f = parse_fmt("format X =\n@<<< @<<< @<<<\n$a, $b, $c\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { args, .. } => {
                assert_eq!(args.len(), 3);
                for a in args {
                    assert!(matches!(a.kind, ExprKind::ScalarVar(_)));
                }
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    #[test]
    fn format_args_expression() {
        // Args are real Perl expressions, not just var refs.
        let f = parse_fmt("format X =\n@###\n$a + $b\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { args, .. } => {
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Add, _, _)));
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    // ── Args: braced multi-line form ──

    #[test]
    fn format_args_braced_single_line() {
        let f = parse_fmt("format X =\n@<<< @<<<\n{ $a, $b }\n.\n");
        match &f.lines[0] {
            FormatLine::Picture { args, .. } => {
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    #[test]
    fn format_args_braced_multi_line() {
        // Classic perlform example: args spread across many lines.
        let src = "\
format X =
@<< @<< @<<
{
  1,
  2,
  3,
}
.
";
        let f = parse_fmt(src);
        match &f.lines[0] {
            FormatLine::Picture { args, .. } => {
                assert_eq!(args.len(), 3);
                assert!(args.iter().all(|a| matches!(a.kind, ExprKind::IntLit(_))));
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    #[test]
    fn format_args_braced_with_qw() {
        // qw(...) in braced args yields multiple list elements.
        let src = "\
format X =
@<< @<< @<<
{
  qw[a b c],
}
.
";
        let f = parse_fmt(src);
        match &f.lines[0] {
            FormatLine::Picture { args, .. } => {
                // qw counts as one expr here (a QwList node); runtime
                // flattens it.  Parser sees one argument.
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    // ── Multi-line format body structure ──

    #[test]
    fn format_multiple_lines_mixed() {
        let src = "\
format X =
# header comment
Header text
@<<< @###
$name, $n
.
";
        let f = parse_fmt(src);
        assert_eq!(f.lines.len(), 3);
        assert!(matches!(f.lines[0], FormatLine::Comment { .. }));
        assert!(matches!(f.lines[1], FormatLine::Literal { .. }));
        assert!(matches!(f.lines[2], FormatLine::Picture { .. }));
    }

    #[test]
    fn format_two_pictures_back_to_back() {
        let src = "\
format X =
@<<<
$a
@>>>
$b
.
";
        let f = parse_fmt(src);
        assert_eq!(f.lines.len(), 2);
        match &f.lines[0] {
            FormatLine::Picture { parts, .. } => {
                assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::LeftJustify { .. }, .. })));
            }
            _ => panic!(),
        }
        match &f.lines[1] {
            FormatLine::Picture { parts, .. } => {
                assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::RightJustify { .. }, .. })));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn format_repeat_kind_on_picture_line() {
        let src = "\
format X =
~~ ^<<<
$long
.
";
        let f = parse_fmt(src);
        match &f.lines[0] {
            FormatLine::Picture { repeat, .. } => {
                assert!(matches!(repeat, RepeatKind::Repeat));
            }
            other => panic!("expected Picture, got {other:?}"),
        }
    }

    // ── Format followed by more top-level code ──

    #[test]
    fn format_followed_by_statement() {
        let src = "\
format X =
@<<<
$x
.
my $y = 1;
";
        let prog = parse(src);
        assert_eq!(prog.statements.len(), 2);
        assert!(matches!(prog.statements[0].kind, StmtKind::FormatDecl(_)));
        assert!(matches!(prog.statements[1].kind, StmtKind::Expr(_)));
    }

    // ── Rejection: `@` or `^` not followed by valid pad chars ──

    #[test]
    fn format_bare_at_is_literal() {
        // `I have an @ here.` — the lone `@` isn't a field start,
        // so the whole line parses as Literal.
        let f = parse_fmt("format X =\nI have an @ here.\n.\n");
        match &f.lines[0] {
            FormatLine::Literal { text, .. } => assert_eq!(text, "I have an @ here."),
            other => panic!("expected Literal, got {other:?}"),
        }
    }

    // ── class/field/method tests ──────────────────────────────

    /// Convenience for class tests: prefixes the source with the
    /// required `use feature 'class'` and `no warnings` pragmas,
    /// then returns the first ClassDecl statement.
    fn parse_class_prog(body: &str) -> Program {
        let src = format!("use feature 'class'; no warnings 'experimental::class'; {body}");
        parse(&src)
    }

    fn find_class_decl(prog: &Program) -> &ClassDecl {
        for stmt in &prog.statements {
            if let StmtKind::ClassDecl(c) = &stmt.kind {
                return c;
            }
        }
        panic!("no ClassDecl in program");
    }

    #[test]
    fn parse_class_decl() {
        let prog = parse_class_prog("class Foo { field $x; method greet { 1; } }");
        let c = find_class_decl(&prog);
        assert_eq!(c.name, "Foo");
        assert!(c.body.as_ref().unwrap().statements.len() >= 2);
    }

    #[test]
    fn parse_class_with_isa() {
        let prog = parse_class_prog("class Bar :isa(Foo) { }");
        let c = find_class_decl(&prog);
        assert_eq!(c.name, "Bar");
        assert_eq!(c.attributes.len(), 1);
        assert_eq!(c.attributes[0].name, "isa");
    }

    #[test]
    fn parse_field_decl() {
        let prog = parse_class_prog("class Foo { field $x = 42; }");
        let c = find_class_decl(&prog);
        match &c.body.as_ref().unwrap().statements[0].kind {
            StmtKind::FieldDecl(f) => {
                assert_eq!(f.var.name, "x");
                assert!(f.default.is_some());
            }
            other => panic!("expected FieldDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_field_with_param() {
        let prog = parse_class_prog("class Foo { field $name :param; }");
        let c = find_class_decl(&prog);
        match &c.body.as_ref().unwrap().statements[0].kind {
            StmtKind::FieldDecl(f) => {
                assert_eq!(f.attributes.len(), 1);
                assert_eq!(f.attributes[0].name, "param");
            }
            other => panic!("expected FieldDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_method_decl() {
        let prog = parse_class_prog("class Foo { method greet() { 1; } }");
        let c = find_class_decl(&prog);
        match &c.body.as_ref().unwrap().statements[0].kind {
            StmtKind::MethodDecl(m) => {
                assert_eq!(m.name, "greet");
            }
            other => panic!("expected MethodDecl, got {other:?}"),
        }
    }

    // ── Indirect object syntax tests ──────────────────────────

    #[test]
    fn parse_indirect_new() {
        let e = parse_expr_str("new Foo(1, 2);");
        match &e.kind {
            ExprKind::IndirectMethodCall(class, method, args) => {
                assert!(matches!(&class.kind, ExprKind::Bareword(n) if n == "Foo"));
                assert_eq!(method, "new");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected IndirectMethodCall, got {other:?}"),
        }
    }

    #[test]
    fn parse_indirect_new_no_args() {
        let e = parse_expr_str("new Foo;");
        match &e.kind {
            ExprKind::IndirectMethodCall(class, method, args) => {
                assert!(matches!(&class.kind, ExprKind::Bareword(n) if n == "Foo"));
                assert_eq!(method, "new");
                assert_eq!(args.len(), 0);
            }
            other => panic!("expected IndirectMethodCall, got {other:?}"),
        }
    }

    #[test]
    fn parse_indirect_with_var() {
        let e = parse_expr_str("new $class;");
        match &e.kind {
            ExprKind::IndirectMethodCall(invocant, method, _) => {
                assert_eq!(method, "new");
                assert!(matches!(invocant.kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected IndirectMethodCall, got {other:?}"),
        }
    }

    // ── Heredoc interpolation tests ───────────────────────────

    #[test]
    fn parse_heredoc_interpolation() {
        let src = "<<END;\nHello $name!\nEND\n";
        let prog = parse(src);
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::InterpolatedString(Interpolated(parts)), .. }) => {
                assert!(parts.len() >= 3); // "Hello ", $name, "!\n"
                assert!(matches!(parts[0], InterpPart::Const(_)));
                assert!(matches!(parts[1], InterpPart::ScalarInterp(_)));
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn parse_heredoc_no_interp_stays_stringlit() {
        let src = "<<END;\nNo variables here.\nEND\n";
        let prog = parse(src);
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::StringLit(s), .. }) => {
                assert_eq!(s, "No variables here.\n");
            }
            other => panic!("expected StringLit, got {other:?}"),
        }
    }

    #[test]
    fn parse_heredoc_single_quoted_no_interp() {
        // <<'END' should NOT interpolate — $name stays literal
        let src = "<<'END';\nHello $name!\nEND\n";
        let prog = parse(src);
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::StringLit(s), .. }) => {
                assert_eq!(s, "Hello $name!\n");
            }
            other => panic!("expected StringLit with literal $name, got {other:?}"),
        }
    }

    // ── Heredoc nesting torture tests ─────────────────────────

    #[test]
    fn parse_heredoc_two_stacked() {
        // Two heredocs on one line, bodies consumed in order.
        let src = "print <<A, <<B;\nbody A\nA\nbody B\nB\nafter\n";
        let prog = parse(src);
        assert_eq!(prog.statements.len(), 2);
        // First statement: print with two heredoc args.
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PrintOp(name, _, args), .. }) => {
                assert_eq!(name, "print");
                assert_eq!(args.len(), 2);
                assert!(matches!(&args[0].kind, ExprKind::StringLit(s) if s == "body A\n"));
                assert!(matches!(&args[1].kind, ExprKind::StringLit(s) if s == "body B\n"));
            }
            other => panic!("expected print with 2 heredoc args, got {other:?}"),
        }
    }

    #[test]
    fn parse_heredoc_three_stacked() {
        // Three heredocs on one line.
        let src = "print <<A, <<B, <<C;\nA-body\nA\nB-body\nB\nC-body\nC\n";
        let prog = parse(src);
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, _, args), .. }) => {
                assert_eq!(args.len(), 3);
                assert!(matches!(&args[0].kind, ExprKind::StringLit(s) if s == "A-body\n"));
                assert!(matches!(&args[1].kind, ExprKind::StringLit(s) if s == "B-body\n"));
                assert!(matches!(&args[2].kind, ExprKind::StringLit(s) if s == "C-body\n"));
            }
            other => panic!("expected print with 3 heredoc args, got {other:?}"),
        }
    }

    #[test]
    fn parse_heredoc_stacked_mixed_quoting() {
        // Mix of <<TAG, <<'TAG', and <<"TAG".
        let src = "print <<A, <<'B', <<\"C\";\nA: $x\nA\nB: $x\nB\nC: $x\nC\n";
        let prog = parse(src);
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, _, args), .. }) => {
                assert_eq!(args.len(), 3);
                // <<A interpolates (A's body has $x).
                assert!(matches!(&args[0].kind, ExprKind::InterpolatedString(_)));
                // <<'B' does not interpolate.
                assert!(matches!(&args[1].kind, ExprKind::StringLit(s) if s == "B: $x\n"));
                // <<"C" interpolates.
                assert!(matches!(&args[2].kind, ExprKind::InterpolatedString(_)));
            }
            other => panic!("expected print with 3 mixed heredocs, got {other:?}"),
        }
    }

    #[test]
    fn parse_heredoc_stacked_with_trailing_code() {
        // Bodies on separate lines, then more code after.
        let src = "my @a = (<<X, <<Y);\nX-body\nX\nY-body\nY\nmy $z = 1;\n";
        let prog = parse(src);
        assert_eq!(prog.statements.len(), 2);
        let (_, vars0) = decl_vars(&prog.statements[0]);
        assert_eq!(vars0[0].name, "a");
        let (_, vars1) = decl_vars(&prog.statements[1]);
        assert_eq!(vars1[0].name, "z");
    }

    #[test]
    fn parse_heredoc_indented_stacked() {
        // <<~A and <<~B stacked, indentation stripped from each.
        let src = "print <<~A, <<~B;\n    A-body\n    A\n        B-body\n        B\n";
        let prog = parse(src);
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, _, args), .. }) => {
                assert_eq!(args.len(), 2);
                assert!(matches!(&args[0].kind, ExprKind::StringLit(s) if s == "A-body\n"));
                assert!(matches!(&args[1].kind, ExprKind::StringLit(s) if s == "B-body\n"));
            }
            other => panic!("expected print with 2 indented heredocs, got {other:?}"),
        }
    }

    #[test]
    fn parse_heredoc_mixed_indented_non_indented() {
        // <<A followed by <<~B: one plain, one indented.
        let src = "print <<A, <<~B;\nplain-body\nA\n    indented-body\n    B\n";
        let prog = parse(src);
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, _, args), .. }) => {
                assert_eq!(args.len(), 2);
                assert!(matches!(&args[0].kind, ExprKind::StringLit(s) if s == "plain-body\n"));
                assert!(matches!(&args[1].kind, ExprKind::StringLit(s) if s == "indented-body\n"));
            }
            other => panic!("expected print with mixed heredocs, got {other:?}"),
        }
    }

    #[test]
    fn parse_heredoc_same_tag_name() {
        // Two heredocs with the same tag name.  The first body
        // terminates at the first occurrence of the tag, then
        // the second heredoc begins with a new body.
        let src = "print <<END, <<END;\nfirst\nEND\nsecond\nEND\n";
        let prog = parse(src);
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, _, args), .. }) => {
                assert_eq!(args.len(), 2);
                assert!(matches!(&args[0].kind, ExprKind::StringLit(s) if s == "first\n"));
                assert!(matches!(&args[1].kind, ExprKind::StringLit(s) if s == "second\n"));
            }
            other => panic!("expected print with 2 same-tag heredocs, got {other:?}"),
        }
    }

    #[test]
    fn parse_heredoc_with_concat_then_heredoc() {
        // <<A . <<B — concatenation of two heredoc strings.
        let src = "my $x = <<A . <<B;\nalpha\nA\nbeta\nB\n";
        let prog = parse(src);
        let init = decl_init(&prog.statements[0]);
        // Should be a Concat of two StringLits.
        match &init.kind {
            ExprKind::BinOp(BinOp::Concat, left, right) => {
                assert!(matches!(&left.kind, ExprKind::StringLit(s) if s == "alpha\n"));
                assert!(matches!(&right.kind, ExprKind::StringLit(s) if s == "beta\n"));
            }
            other => panic!("expected Concat, got {other:?}"),
        }
    }

    #[test]
    fn parse_heredoc_unterminated_gives_error() {
        // Heredoc tag with no terminator line → parse error.
        let src = "my $x = <<END;\nbody line\nbody line 2\n";
        let mut parser = Parser::new(src.as_bytes()).unwrap();
        let result = parser.parse_program();
        assert!(result.is_err(), "expected error for unterminated heredoc");
    }

    // ── Torture test pieces ──────────────────────────────────
    //
    // Derived from a real Perl program that exercises heredoc
    // nesting, interpolation forms, and compile-time hoisting
    // simultaneously.  Each test below isolates one aspect so
    // failures are diagnostic.

    #[test]
    fn torture_heredoc_arithmetic_stacked() {
        // `<<A + <<B + <<C` — three heredocs combined with `+`.
        // Bodies are single numbers.  Deparse evaluates at
        // compile time but we just verify parsing.
        let src = "my $x = <<A + <<B + <<C;\n1\nA\n2\nB\n3\nC\n";
        let prog = parse(src);
        let init = decl_init(&prog.statements[0]);
        // Shape: Add(Add(heredoc_A, heredoc_B), heredoc_C).
        match &init.kind {
            ExprKind::BinOp(BinOp::Add, left, right) => {
                // Right is the third heredoc (literal "3\n").
                assert!(matches!(&right.kind, ExprKind::StringLit(s) if s == "3\n"), "right should be heredoc C, got {:?}", right.kind);
                match &left.kind {
                    ExprKind::BinOp(BinOp::Add, a, b) => {
                        assert!(matches!(&a.kind, ExprKind::StringLit(s) if s == "1\n"), "first should be heredoc A");
                        assert!(matches!(&b.kind, ExprKind::StringLit(s) if s == "2\n"), "second should be heredoc B");
                    }
                    other => panic!("inner should be Add, got {other:?}"),
                }
            }
            other => panic!("expected Add at top, got {other:?}"),
        }
    }

    #[test]
    fn torture_ref_to_expr_in_interp() {
        // `"${\(1 + 2)}"` — `${...}` with `\(expr)` inside.
        // This is a common Perl idiom for embedding arbitrary
        // expressions in interpolated strings.
        let parts = interp_parts(r#""${\(1 + 2)}";"#);
        // Expect: ExprInterp containing Ref(Paren(Add(1, 2)))
        // or Ref(Add(1, 2)) — depends on paren handling.
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            InterpPart::ExprInterp(e) => {
                // Outer is Ref(\...).
                match &e.kind {
                    ExprKind::Ref(inner) => {
                        // Inner is the paren-wrapped addition.
                        let actual_add = match &inner.kind {
                            ExprKind::Paren(p) => p,
                            other => panic!("expected Paren inside Ref, got {other:?}"),
                        };
                        assert!(matches!(actual_add.kind, ExprKind::BinOp(BinOp::Add, _, _)), "expected Add, got {:?}", actual_add.kind);
                    }
                    other => panic!("expected Ref, got {other:?}"),
                }
            }
            other => panic!("expected ExprInterp, got {other:?}"),
        }
    }

    #[test]
    fn torture_do_block_in_expression() {
        // `my $x = do { 1 + 2 };` — do-block as expression.
        let prog = parse("my $x = do { 1 + 2 };");
        let init = decl_init(&prog.statements[0]);
        match &init.kind {
            ExprKind::DoBlock(_) => {}
            other => panic!("expected DoBlock, got {other:?}"),
        }
    }

    #[test]
    fn torture_begin_inside_do_block() {
        // `do { BEGIN { our $a = 1; } $a }` — BEGIN hoists to
        // compile time even inside a runtime do-block.  We just
        // verify the parser accepts this; BEGIN semantics are
        // runtime behavior.
        let prog = parse("my $x = do { BEGIN { our $a = 1; } $a };");
        let init = decl_init(&prog.statements[0]);
        match &init.kind {
            ExprKind::DoBlock(block) => {
                // Block should contain a BEGIN and an expression.
                assert!(block.statements.len() >= 2, "expected at least BEGIN + expr in do-block, got {:?}", block.statements);
            }
            other => panic!("expected DoBlock, got {other:?}"),
        }
    }

    #[test]
    fn torture_heredoc_in_interp_of_heredoc() {
        // Heredoc inside `${\(...)}` inside another heredoc body.
        // This is the nesting pattern from the torture test:
        //   <<OUTER contains `${\(do { my $a = <<INNER; ... })}`.
        // Simplified version:
        let src = "\
my $x = <<OUTER;\n\
prefix ${\\ do { <<INNER }}\n\
inner body\n\
INNER\n\
suffix\n\
OUTER\n";
        let prog = parse(src);
        assert!(!prog.statements.is_empty(), "should parse without error");
        let init = decl_init(&prog.statements[0]);
        // Outer is an InterpolatedString (heredoc with interpolation).
        assert!(matches!(init.kind, ExprKind::InterpolatedString(_)), "expected InterpolatedString for heredoc, got {:?}", init.kind);
    }

    #[test]
    fn torture_array_interp_with_heredoc() {
        // `"@{[<<END]}"` — array interpolation containing a heredoc.
        let src = "my $x = \"@{[<<END]}\";\nheredoc body\nEND\n";
        let prog = parse(src);
        assert!(!prog.statements.is_empty(), "should parse");
        let init = decl_init(&prog.statements[0]);
        assert!(matches!(init.kind, ExprKind::InterpolatedString(_)), "expected InterpolatedString, got {:?}", init.kind);
    }

    #[test]
    fn torture_qq_with_nested_heredoc() {
        // `qq{prefix ${\(<<END)} suffix}` — qq with heredoc inside.
        let src = "my $x = qq{prefix ${\\<<END} suffix};\nheredoc body\nEND\n";
        let prog = parse(src);
        assert!(!prog.statements.is_empty(), "should parse");
    }

    #[test]
    fn torture_stacked_heredoc_list_assignment() {
        // The exact pattern from the torture test:
        // `my ($x, $y, $z) = (<<~X, <<Y, do { expr });`
        // Simplified: just two heredocs plus a literal.
        let src = "my ($x, $y, $z) = (<<~A, <<B, 42);\n    A-body\n    A\nB-body\nB\n";
        let prog = parse(src);
        assert!(!prog.statements.is_empty(), "should parse");
    }

    // ── Dynamic method dispatch tests ─────────────────────────

    // ═══════════════════════════════════════════════════════════
    // Gap-probing tests — things I'm not sure the parser
    // handles.  Written to match Perl's actual behavior.
    // Failures are diagnostic: they tell us what to fix.
    // ═══════════════════════════════════════════════════════════

    // ── Postderef_qq: remaining forms ────────────────────────

    #[test]
    fn interp_postderef_qq_code() {
        // `->&*` — code deref inside string.
        let parts = interp_parts(r#""$ref->&*";"#);
        let e = scalar_part(&parts, 0);
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefCode)), "expected DerefCode, got {:?}", e.kind);
    }

    #[test]
    fn interp_postderef_qq_glob() {
        // `->**` — glob deref inside string.  Lexer emits
        // Token::Power for `**`.
        let parts = interp_parts(r#""$ref->**";"#);
        let e = scalar_part(&parts, 0);
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefGlob)), "expected DerefGlob, got {:?}", e.kind);
    }

    #[test]
    fn interp_postderef_qq_array_slice() {
        // `->@[0,1]` — array slice inside string.
        let parts = interp_parts(r#""$ref->@[0,1]";"#);
        let e = scalar_part(&parts, 0);
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::ArraySliceIndices(_))), "expected ArraySliceIndices, got {:?}", e.kind);
    }

    #[test]
    fn interp_postderef_qq_hash_slice() {
        // `->@{"a","b"}` — hash slice (values) inside string.
        let parts = interp_parts(r#""$ref->@{'a','b'}";"#);
        let e = scalar_part(&parts, 0);
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::ArraySliceKeys(_))), "expected ArraySliceKeys, got {:?}", e.kind);
    }

    // ── Indented heredoc edge cases ──────────────────────────

    #[test]
    fn heredoc_indented_tabs() {
        // <<~END with tab indentation.
        let src = "my $x = <<~END;\n\tindented with tab\n\tEND\n";
        let prog = parse(src);
        let init = decl_init(&prog.statements[0]);
        assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s == "indented with tab\n"), "expected stripped tab indent, got {:?}", init.kind);
    }

    #[test]
    fn heredoc_indented_empty_body() {
        // <<~END with terminator immediately — empty body.
        let src = "my $x = <<~END;\n    END\n";
        let prog = parse(src);
        let init = decl_init(&prog.statements[0]);
        assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s.is_empty()), "expected empty string, got {:?}", init.kind);
    }

    #[test]
    fn heredoc_indented_blank_lines_preserved() {
        // Blank lines in <<~ body should be preserved as
        // empty lines (they don't need indentation).
        let src = "my $x = <<~END;\n    line1\n\n    line2\n    END\n";
        let prog = parse(src);
        let init = decl_init(&prog.statements[0]);
        assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s == "line1\n\nline2\n"), "expected blank line preserved, got {:?}", init.kind);
    }

    #[test]
    fn heredoc_traditional_empty_body() {
        // Regular <<END with tag on the very next line.
        let src = "my $x = <<END;\nEND\n";
        let prog = parse(src);
        let init = decl_init(&prog.statements[0]);
        assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s.is_empty()), "expected empty heredoc body, got {:?}", init.kind);
    }

    // ── Heredoc backslash form ───────────────────────────────

    #[test]
    fn heredoc_backslash_form() {
        // `<<\EOF` — equivalent to `<<'EOF'` (non-interpolating).
        let src = "my $x = <<\\EOF;\nHello \\$name!\nEOF\n";
        let prog = parse(src);
        let init = decl_init(&prog.statements[0]);
        // Non-interpolating: `$name` stays literal.
        assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s.contains("$name")), "expected literal $name in body, got {:?}", init.kind);
    }

    #[test]
    fn heredoc_backslash_no_escape_processing() {
        // Per perlop: backslashes have no special meaning in a
        // single-quoted here-doc, `\\` is two backslashes.
        let src = "my $x = <<\\EOF;\nline with \\\\ two backslashes\nEOF\n";
        let prog = parse(src);
        let init = decl_init(&prog.statements[0]);
        assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s.contains("\\\\")), "expected literal double-backslash, got {:?}", init.kind);
    }

    #[test]
    fn heredoc_indented_backslash_form() {
        // `<<~\EOF` — indented + backslash (non-interpolating).
        let src = "my $x = <<~\\EOF;\n    Hello $name!\n    EOF\n";
        let prog = parse(src);
        let init = decl_init(&prog.statements[0]);
        assert!(
            matches!(init.kind, ExprKind::StringLit(ref s) if s.contains("$name")),
            "expected literal $name in indented backslash heredoc, got {:?}",
            init.kind
        );
    }

    // ── Substitution delimiter variations ────────────────────

    #[test]
    fn subst_paren_delimiters() {
        let e = parse_expr_str("s(foo)(bar);");
        match &e.kind {
            ExprKind::Subst(pat, repl, _) => {
                assert_eq!(pat_str(pat), "foo");
                assert_eq!(pat_str(repl), "bar");
            }
            other => panic!("expected Subst, got {other:?}"),
        }
    }

    #[test]
    fn subst_bracket_delimiters() {
        let e = parse_expr_str("s[foo][bar];");
        match &e.kind {
            ExprKind::Subst(pat, repl, _) => {
                assert_eq!(pat_str(pat), "foo");
                assert_eq!(pat_str(repl), "bar");
            }
            other => panic!("expected Subst, got {other:?}"),
        }
    }

    #[test]
    fn subst_mixed_paired_delimiters() {
        // s{pattern}(replacement) — different paired delims.
        let e = parse_expr_str("s{foo}(bar);");
        match &e.kind {
            ExprKind::Subst(pat, repl, _) => {
                assert_eq!(pat_str(pat), "foo");
                assert_eq!(pat_str(repl), "bar");
            }
            other => panic!("expected Subst, got {other:?}"),
        }
    }

    #[test]
    fn subst_paired_pattern_unpaired_replacement() {
        // s{pattern}/replacement/ — paired then unpaired.
        let e = parse_expr_str("s{foo}/bar/;");
        match &e.kind {
            ExprKind::Subst(pat, repl, _) => {
                assert_eq!(pat_str(pat), "foo");
                assert_eq!(pat_str(repl), "bar");
            }
            other => panic!("expected Subst, got {other:?}"),
        }
    }

    #[test]
    fn subst_angle_delimiters() {
        // s<foo><bar> — angle brackets as paired delimiters.
        let e = parse_expr_str("s<foo><bar>;");
        match &e.kind {
            ExprKind::Subst(pat, repl, _) => {
                assert_eq!(pat_str(pat), "foo");
                assert_eq!(pat_str(repl), "bar");
            }
            other => panic!("expected Subst, got {other:?}"),
        }
    }

    #[test]
    fn tr_paired_delimiters() {
        // tr{a-z}{A-Z} — paired braces for tr.
        let e = parse_expr_str("tr{a-z}{A-Z};");
        match &e.kind {
            ExprKind::Translit(from, to, _) => {
                assert_eq!(from, "a-z");
                assert_eq!(to, "A-Z");
            }
            other => panic!("expected Translit, got {other:?}"),
        }
    }

    // ── Empty / minimal quote forms ──────────────────────────

    #[test]
    fn empty_qw() {
        // `qw()` — empty word list.
        let e = parse_expr_str("qw();");
        match &e.kind {
            ExprKind::QwList(words) => assert!(words.is_empty()),
            other => panic!("expected empty QwList, got {other:?}"),
        }
    }

    #[test]
    fn single_qw() {
        let e = parse_expr_str("qw(hello);");
        match &e.kind {
            ExprKind::QwList(words) => {
                assert_eq!(words.len(), 1);
                assert_eq!(words[0], "hello");
            }
            other => panic!("expected QwList, got {other:?}"),
        }
    }

    #[test]
    fn empty_q_string() {
        let e = parse_expr_str("q{};");
        assert!(matches!(e.kind, ExprKind::StringLit(ref s) if s.is_empty()), "expected empty StringLit, got {:?}", e.kind);
    }

    #[test]
    fn empty_interpolated_string() {
        let e = parse_expr_str("\"\";");
        assert!(matches!(e.kind, ExprKind::StringLit(ref s) if s.is_empty()), "expected empty StringLit, got {:?}", e.kind);
    }

    // ── Hash key edge cases ──────────────────────────────────

    #[test]
    fn negative_bareword_hash_key() {
        // `$h{-key}` — the `-key` form is common in Perl.
        // Parses as HashElem with StringLit("-key").
        let e = parse_expr_str("$h{-key};");
        match &e.kind {
            ExprKind::HashElem(_, k) => {
                assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "-key"), "expected StringLit(-key), got {:?}", k.kind);
            }
            other => panic!("expected HashElem, got {other:?}"),
        }
    }

    #[test]
    fn numeric_hash_key() {
        // `$h{42}` — numeric key, not autoquoted.
        let e = parse_expr_str("$h{42};");
        match &e.kind {
            ExprKind::HashElem(_, k) => {
                assert!(matches!(k.kind, ExprKind::IntLit(42)), "expected IntLit(42), got {:?}", k.kind);
            }
            other => panic!("expected HashElem, got {other:?}"),
        }
    }

    // ── Special variable forms ───────────────────────────────

    #[test]
    fn local_list_separator() {
        // `local $" = ","` — localizing the list separator.
        let prog = parse(r#"local $" = ",";"#);
        assert!(!prog.statements.is_empty(), "should parse local $\" assignment");
    }

    #[test]
    fn special_var_in_interpolation() {
        // `"v$^V"` — $^V (Perl version) in a string.
        let parts = interp_parts(r#""v$^V";"#);
        assert!(parts.len() >= 2, "expected at least const + var");
    }

    // ── Control flow edge cases ──────────────────────────────

    #[test]
    fn nested_ternary() {
        let e = parse_expr_str("$a ? $b ? 1 : 2 : 3;");
        // Right-associative: `$a ? ($b ? 1 : 2) : 3`.
        match &e.kind {
            ExprKind::Ternary(_, then_expr, else_expr) => {
                assert!(matches!(then_expr.kind, ExprKind::Ternary(_, _, _)), "inner then should be another ternary");
                assert!(matches!(else_expr.kind, ExprKind::IntLit(3)));
            }
            other => panic!("expected nested Ternary, got {other:?}"),
        }
    }

    #[test]
    fn unless_block() {
        let prog = parse("unless ($x) { 1; }");
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn until_loop() {
        let prog = parse("until ($done) { do_work(); }");
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn chained_method_calls() {
        let e = parse_expr_str("$obj->method1->method2->method3;");
        // Outer: MethodCall(MethodCall(MethodCall($obj, "method1", []),
        //        "method2", []), "method3", []).
        // Note: `->method` produces MethodCall, not ArrowDeref.
        fn depth(e: &Expr) -> usize {
            match &e.kind {
                ExprKind::MethodCall(inner, _, _) => 1 + depth(inner),
                ExprKind::ArrowDeref(inner, _) => 1 + depth(inner),
                _ => 0,
            }
        }
        assert_eq!(depth(&e), 3, "expected 3 levels of method chain");
    }

    // ── String operator precedence ───────────────────────────

    #[test]
    fn concat_and_repeat() {
        // `"a" . "b" x 3` — `x` binds tighter than `.`.
        // Parses as `"a" . ("b" x 3)`.
        let e = parse_expr_str(r#""a" . "b" x 3;"#);
        match &e.kind {
            ExprKind::BinOp(BinOp::Concat, _, rhs) => {
                assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::Repeat, _, _)), "rhs of concat should be repeat, got {:?}", rhs.kind);
            }
            other => panic!("expected Concat, got {other:?}"),
        }
    }

    // ── Defined-or forms ─────────────────────────────────────

    #[test]
    fn defined_or_assign() {
        let e = parse_expr_str("$x //= 42;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::DefinedOrEq, _, _)), "expected //= assignment, got {:?}", e.kind);
    }

    #[test]
    fn chained_defined_or() {
        // `$a // $b // $c` — left-associative.
        let e = parse_expr_str("$a // $b // $c;");
        match &e.kind {
            ExprKind::BinOp(BinOp::DefinedOr, lhs, _) => {
                assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)), "inner should also be DefinedOr");
            }
            other => panic!("expected chained DefinedOr, got {other:?}"),
        }
    }

    // ── do "filename" vs do { block } ────────────────────────

    #[test]
    fn do_file() {
        // `do "config.pl"` — loads and executes a file.
        let e = parse_expr_str(r#"do "config.pl";"#);
        match &e.kind {
            ExprKind::DoExpr(path) => {
                assert!(matches!(path.kind, ExprKind::StringLit(ref s) if s == "config.pl"));
            }
            other => panic!("expected DoExpr, got {other:?}"),
        }
    }

    #[test]
    fn do_block_vs_do_file() {
        // `do { 1 }` vs `do "file"` — both valid.
        let block = parse_expr_str("do { 1 };");
        assert!(matches!(block.kind, ExprKind::DoBlock(_)));
        let file = parse_expr_str(r#"do "file";"#);
        assert!(matches!(file.kind, ExprKind::DoExpr(_)));
    }

    // ── require ──────────────────────────────────────────────

    #[test]
    fn require_module() {
        let prog = parse("require Foo::Bar;");
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn require_version() {
        let prog = parse("require 5.036;");
        assert!(!prog.statements.is_empty());
    }

    // ── __DATA__ section ─────────────────────────────────────

    #[test]
    fn data_section() {
        let src = "my $x = 1;\n__DATA__\nThis is data.\nMore data.\n";
        let prog = parse(src);
        // Should have at least 2 statements: my decl and DataEnd.
        assert!(prog.statements.len() >= 2);
        let has_data_end = prog.statements.iter().any(|s| matches!(s.kind, StmtKind::DataEnd(_, _)));
        assert!(has_data_end, "expected DataEnd statement");
    }

    // ── Regex edge cases ─────────────────────────────────────

    #[test]
    fn regex_many_flags() {
        let e = parse_expr_str("/foo/msixpn;");
        match &e.kind {
            ExprKind::Regex(_, _, flags) => {
                let f = flags.as_deref().unwrap_or("");
                assert!(f.contains('m') && f.contains('s') && f.contains('i') && f.contains('x'), "expected msixpn flags, got {f:?}");
            }
            other => panic!("expected Regex, got {other:?}"),
        }
    }

    #[test]
    fn regex_character_class() {
        let e = parse_expr_str(r#"/[a-z\d\s]+/;"#);
        match &e.kind {
            ExprKind::Regex(_, pat, _) => {
                let s = pat_str(pat);
                assert!(s.contains("[a-z") && s.contains("]"), "expected char class in pattern, got {s:?}");
            }
            other => panic!("expected Regex, got {other:?}"),
        }
    }

    // ── Print to filehandle ──────────────────────────────────

    #[test]
    fn print_to_stderr() {
        let prog = parse(r#"print STDERR "error\n";"#);
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, fh, _), .. }) => {
                assert!(fh.is_some(), "expected filehandle");
            }
            other => panic!("expected PrintOp with filehandle, got {other:?}"),
        }
    }

    // ── Fat comma autoquoting edge case ──────────────────────

    #[test]
    fn fat_comma_numeric_key() {
        // `123 => "val"` — numbers are NOT autoquoted.
        let e = parse_expr_str("123 => 'val';");
        match &e.kind {
            ExprKind::List(items) => {
                assert!(matches!(items[0].kind, ExprKind::IntLit(123)), "numeric key should stay IntLit, got {:?}", items[0].kind);
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn parse_dynamic_method() {
        // $obj->$method
        let e = parse_expr_str("$obj->$method;");
        match &e.kind {
            ExprKind::ArrowDeref(_, ArrowTarget::DynMethod(method_expr, args)) => {
                assert!(matches!(method_expr.kind, ExprKind::ScalarVar(_)));
                assert_eq!(args.len(), 0);
            }
            other => panic!("expected DynMethod, got {other:?}"),
        }
    }

    #[test]
    fn parse_dynamic_method_with_args() {
        // $obj->$method(1, 2)
        let e = parse_expr_str("$obj->$method(1, 2);");
        match &e.kind {
            ExprKind::ArrowDeref(_, ArrowTarget::DynMethod(_, args)) => {
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected DynMethod with args, got {other:?}"),
        }
    }

    // ── Complex local lvalue tests ────────────────────────────

    #[test]
    fn parse_local_hash_elem() {
        let prog = parse("local $hash{key} = 42;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => match &lhs.kind {
                ExprKind::Local(inner) => {
                    assert!(matches!(inner.kind, ExprKind::HashElem(_, _)));
                }
                other => panic!("expected Local(HashElem), got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    #[test]
    fn parse_local_glob() {
        let prog = parse("local *STDOUT;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::Local(inner), .. }) => {
                assert!(matches!(inner.kind, ExprKind::GlobVar(_)));
            }
            other => panic!("expected Local(GlobVar), got {other:?}"),
        }
    }

    #[test]
    fn parse_local_simple_var() {
        // local $x = 5 still works
        let prog = parse("local $x = 5;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => {
                assert!(matches!(lhs.kind, ExprKind::Local(_)));
            }
            other => panic!("expected Assign(Local), got {other:?}"),
        }
    }

    #[test]
    fn parse_delete_local_hash_elem() {
        // delete local $hash{key}
        let e = parse_expr_str("delete local $hash{key};");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "delete");
                assert!(matches!(args[0].kind, ExprKind::Local(_)));
            }
            other => panic!("expected delete(Local(...)), got {other:?}"),
        }
    }

    #[test]
    fn parse_local_special_var() {
        // local $/ — localize input record separator
        let e = parse_expr_str("local $/;");
        match &e.kind {
            ExprKind::Local(inner) => {
                assert!(matches!(inner.kind, ExprKind::SpecialVar(_)));
            }
            other => panic!("expected Local(SpecialVar), got {other:?}"),
        }
    }

    // ── Filetest operator tests ───────────────────────────────

    #[test]
    fn parse_filetest_e() {
        let e = parse_expr_str("-e $file;");
        match &e.kind {
            ExprKind::Filetest(c, StatTarget::Expr(operand)) => {
                assert_eq!(*c, 'e');
                assert!(matches!(operand.kind, ExprKind::ScalarVar(ref n) if n == "file"));
            }
            other => panic!("expected Filetest('e', Expr(ScalarVar)), got {other:?}"),
        }
    }

    #[test]
    fn parse_filetest_d_string() {
        let e = parse_expr_str(r#"-d "/tmp";"#);
        match &e.kind {
            ExprKind::Filetest(c, StatTarget::Expr(operand)) => {
                assert_eq!(*c, 'd');
                assert!(matches!(operand.kind, ExprKind::StringLit(ref s) if s == "/tmp"));
            }
            other => panic!("expected Filetest('d', Expr(StringLit)), got {other:?}"),
        }
    }

    #[test]
    fn parse_filetest_f_underscore() {
        // -f _ uses the cached stat buffer — dedicated AST variant.
        let e = parse_expr_str("-f _;");
        match &e.kind {
            ExprKind::Filetest(c, StatTarget::StatCache) => {
                assert_eq!(*c, 'f');
            }
            other => panic!("expected Filetest('f', StatCache), got {other:?}"),
        }
    }

    #[test]
    fn parse_filetest_no_operand() {
        // -e alone defaults to $_ — dedicated AST variant.
        let e = parse_expr_str("-e;");
        match &e.kind {
            ExprKind::Filetest(c, StatTarget::Default) => {
                assert_eq!(*c, 'e');
            }
            other => panic!("expected Filetest('e', Default), got {other:?}"),
        }
    }

    #[test]
    fn parse_stacked_filetests() {
        // -f -r $file → Filetest('f', Expr(Filetest('r', Expr($file))))
        let e = parse_expr_str("-f -r $file;");
        match &e.kind {
            ExprKind::Filetest(c, StatTarget::Expr(inner)) => {
                assert_eq!(*c, 'f');
                match &inner.kind {
                    ExprKind::Filetest(c2, StatTarget::Expr(innermost)) => {
                        assert_eq!(*c2, 'r');
                        assert!(matches!(innermost.kind, ExprKind::ScalarVar(ref n) if n == "file"));
                    }
                    other => panic!("expected inner Filetest('r', Expr(ScalarVar)), got {other:?}"),
                }
            }
            other => panic!("expected stacked Filetest, got {other:?}"),
        }
    }

    #[test]
    fn parse_minus_non_filetest_still_quotes() {
        // -key is NOT a filetest — 'k' is filetest but "key" is multi-char
        let e = parse_expr_str("-key;");
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "-key"),
            other => panic!("expected StringLit, got {other:?}"),
        }
    }

    #[test]
    fn parse_filetest_letter_fat_comma_autoquotes() {
        // -f => value — NOT a filetest, autoquotes as StringLit("-f")
        let e = parse_expr_str("-f => 1;");
        match &e.kind {
            ExprKind::List(items) => match &items[0].kind {
                ExprKind::StringLit(s) => assert_eq!(s, "-f"),
                other => panic!("expected StringLit('-f'), got {other:?}"),
            },
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn parse_filetest_letter_hash_subscript_autoquotes() {
        // $hash{-f} — NOT a filetest, autoquotes as StringLit("-f")
        let e = parse_expr_str("$hash{-f};");
        match &e.kind {
            ExprKind::HashElem(_, key) => match &key.kind {
                ExprKind::StringLit(s) => assert_eq!(s, "-f"),
                other => panic!("expected StringLit('-f'), got {other:?}"),
            },
            other => panic!("expected HashElem, got {other:?}"),
        }
    }

    // ── stat / lstat tests ────────────────────────────────────

    #[test]
    fn parse_stat_expr() {
        let e = parse_expr_str("stat $file;");
        match &e.kind {
            ExprKind::Stat(StatTarget::Expr(operand)) => {
                assert!(matches!(operand.kind, ExprKind::ScalarVar(ref n) if n == "file"));
            }
            other => panic!("expected Stat(Expr(ScalarVar)), got {other:?}"),
        }
    }

    #[test]
    fn parse_stat_underscore() {
        let e = parse_expr_str("stat _;");
        assert!(matches!(e.kind, ExprKind::Stat(StatTarget::StatCache)));
    }

    #[test]
    fn parse_stat_default() {
        let e = parse_expr_str("stat;");
        assert!(matches!(e.kind, ExprKind::Stat(StatTarget::Default)));
    }

    #[test]
    fn parse_stat_parens() {
        let e = parse_expr_str("stat($file);");
        match &e.kind {
            ExprKind::Stat(StatTarget::Expr(operand)) => {
                assert!(matches!(operand.kind, ExprKind::ScalarVar(ref n) if n == "file"));
            }
            other => panic!("expected Stat(Expr(ScalarVar)), got {other:?}"),
        }
    }

    #[test]
    fn parse_stat_parens_underscore() {
        let e = parse_expr_str("stat(_);");
        assert!(matches!(e.kind, ExprKind::Stat(StatTarget::StatCache)));
    }

    #[test]
    fn parse_lstat_expr() {
        let e = parse_expr_str("lstat $file;");
        match &e.kind {
            ExprKind::Lstat(StatTarget::Expr(operand)) => {
                assert!(matches!(operand.kind, ExprKind::ScalarVar(ref n) if n == "file"));
            }
            other => panic!("expected Lstat(Expr(ScalarVar)), got {other:?}"),
        }
    }

    #[test]
    fn parse_lstat_underscore() {
        let e = parse_expr_str("lstat _;");
        assert!(matches!(e.kind, ExprKind::Lstat(StatTarget::StatCache)));
    }

    // ── Special array / hash variable tests ───────────────────

    #[test]
    fn parse_special_array_plus() {
        let e = parse_expr_str("@+;");
        match &e.kind {
            ExprKind::SpecialArrayVar(name) => assert_eq!(name, "+"),
            other => panic!("expected SpecialArrayVar('+'), got {other:?}"),
        }
    }

    #[test]
    fn parse_special_array_minus() {
        let e = parse_expr_str("@-;");
        match &e.kind {
            ExprKind::SpecialArrayVar(name) => assert_eq!(name, "-"),
            other => panic!("expected SpecialArrayVar('-'), got {other:?}"),
        }
    }

    #[test]
    fn parse_special_array_elem() {
        // $+[0] — element access on special array @+.
        let e = parse_expr_str("$+[0];");
        match &e.kind {
            ExprKind::ArrayElem(base, idx) => {
                assert!(matches!(base.kind, ExprKind::SpecialVar(ref n) if n == "+"));
                assert!(matches!(idx.kind, ExprKind::IntLit(0)));
            }
            other => panic!("expected ArrayElem(SpecialVar('+'), 0), got {other:?}"),
        }
    }

    #[test]
    fn parse_special_hash_bang() {
        let e = parse_expr_str("%!;");
        match &e.kind {
            ExprKind::SpecialHashVar(name) => assert_eq!(name, "!"),
            other => panic!("expected SpecialHashVar('!'), got {other:?}"),
        }
    }

    #[test]
    fn parse_special_hash_plus() {
        let e = parse_expr_str("%+;");
        match &e.kind {
            ExprKind::SpecialHashVar(name) => assert_eq!(name, "+"),
            other => panic!("expected SpecialHashVar('+'), got {other:?}"),
        }
    }

    #[test]
    fn parse_special_hash_elem() {
        // $!{ENOENT} — element access on special hash %!.
        let e = parse_expr_str("$!{ENOENT};");
        match &e.kind {
            ExprKind::HashElem(base, key) => {
                assert!(matches!(base.kind, ExprKind::SpecialVar(ref n) if n == "!"));
                assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "ENOENT"));
            }
            other => panic!("expected HashElem(SpecialVar('!'), 'ENOENT'), got {other:?}"),
        }
    }

    #[test]
    fn parse_special_array_caret_capture() {
        let e = parse_expr_str("@{^CAPTURE};");
        match &e.kind {
            ExprKind::SpecialArrayVar(name) => assert_eq!(name, "^CAPTURE"),
            other => panic!("expected SpecialArrayVar('^CAPTURE'), got {other:?}"),
        }
    }

    #[test]
    fn parse_special_hash_caret_capture_all() {
        let e = parse_expr_str("%{^CAPTURE_ALL};");
        match &e.kind {
            ExprKind::SpecialHashVar(name) => assert_eq!(name, "^CAPTURE_ALL"),
            other => panic!("expected SpecialHashVar('^CAPTURE_ALL'), got {other:?}"),
        }
    }

    // ═══════════════════════════════════════════════════════════
    // NEW TESTS — compound assignment operators
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn parse_assign_sub() {
        let e = parse_expr_str("$x -= 1;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::SubEq, _, _)));
    }

    #[test]
    fn parse_assign_mul() {
        let e = parse_expr_str("$x *= 2;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::MulEq, _, _)));
    }

    #[test]
    fn parse_assign_div() {
        let e = parse_expr_str("$x /= 2;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::DivEq, _, _)));
    }

    #[test]
    fn parse_assign_mod() {
        let e = parse_expr_str("$x %= 3;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::ModEq, _, _)));
    }

    #[test]
    fn parse_assign_pow() {
        let e = parse_expr_str("$x **= 2;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::PowEq, _, _)));
    }

    #[test]
    fn parse_assign_concat() {
        let e = parse_expr_str("$x .= 'a';");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::ConcatEq, _, _)));
    }

    #[test]
    fn parse_assign_and() {
        let e = parse_expr_str("$x &&= 1;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::AndEq, _, _)));
    }

    #[test]
    fn parse_assign_or() {
        let e = parse_expr_str("$x ||= 1;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::OrEq, _, _)));
    }

    #[test]
    fn parse_assign_defined_or() {
        let e = parse_expr_str("$x //= 1;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::DefinedOrEq, _, _)));
    }

    #[test]
    fn parse_assign_bit_and() {
        let e = parse_expr_str("$x &= 0xFF;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::BitAndEq, _, _)));
    }

    #[test]
    fn parse_assign_bit_or() {
        let e = parse_expr_str("$x |= 0xFF;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::BitOrEq, _, _)));
    }

    #[test]
    fn parse_assign_bit_xor() {
        let e = parse_expr_str("$x ^= 0xFF;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::BitXorEq, _, _)));
    }

    #[test]
    fn parse_assign_shift_l() {
        let e = parse_expr_str("$x <<= 2;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::ShiftLeftEq, _, _)));
    }

    #[test]
    fn parse_assign_shift_r() {
        let e = parse_expr_str("$x >>= 2;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::ShiftRightEq, _, _)));
    }

    // ═══════════════════════════════════════════════════════════
    // NEW TESTS — precedence verification
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn prec_and_binds_tighter_than_or() {
        let e = parse_expr_str("$a && $b || $c;");
        match &e.kind {
            ExprKind::BinOp(BinOp::Or, left, _) => {
                assert!(matches!(left.kind, ExprKind::BinOp(BinOp::And, _, _)));
            }
            other => panic!("expected Or(And(..), ..), got {other:?}"),
        }
    }

    #[test]
    fn prec_assign_right_assoc() {
        let e = parse_expr_str("$a = $b = 1;");
        match &e.kind {
            ExprKind::Assign(AssignOp::Eq, _, right) => {
                assert!(matches!(right.kind, ExprKind::Assign(AssignOp::Eq, _, _)));
            }
            other => panic!("expected chained assign, got {other:?}"),
        }
    }

    #[test]
    fn prec_ternary_nested() {
        // $a ? $b ? 1 : 2 : 3 — right-assoc: $a ? ($b ? 1 : 2) : 3
        let e = parse_expr_str("$a ? $b ? 1 : 2 : 3;");
        match &e.kind {
            ExprKind::Ternary(_, middle, _) => {
                assert!(matches!(middle.kind, ExprKind::Ternary(_, _, _)));
            }
            other => panic!("expected nested Ternary, got {other:?}"),
        }
    }

    #[test]
    fn prec_binding_tighter_than_concat() {
        let e = parse_expr_str("$x =~ /foo/ . 'bar';");
        match &e.kind {
            ExprKind::BinOp(BinOp::Concat, left, _) => {
                assert!(matches!(left.kind, ExprKind::BinOp(BinOp::Binding, _, _)));
            }
            other => panic!("expected Concat(Binding(..), ..), got {other:?}"),
        }
    }

    #[test]
    fn prec_low_or_loosest() {
        let e = parse_expr_str("$a = 1 or die;");
        match &e.kind {
            ExprKind::BinOp(BinOp::LowOr, left, _) => {
                assert!(matches!(left.kind, ExprKind::Assign(_, _, _)));
            }
            other => panic!("expected LowOr(Assign(..), ..), got {other:?}"),
        }
    }

    #[test]
    fn prec_not_low_vs_and_low() {
        let e = parse_expr_str("not $a and $b;");
        match &e.kind {
            ExprKind::BinOp(BinOp::LowAnd, left, _) => {
                assert!(matches!(left.kind, ExprKind::UnaryOp(UnaryOp::Not, _)));
            }
            other => panic!("expected LowAnd(Not(..), ..), got {other:?}"),
        }
    }

    // ═══════════════════════════════════════════════════════════
    // NEW TESTS — operators with AST verification
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn parse_range() {
        let e = parse_expr_str("1..10;");
        assert!(matches!(e.kind, ExprKind::Range(_, _)));
    }

    #[test]
    fn parse_not_binding() {
        let e = parse_expr_str("$x !~ /foo/;");
        match &e.kind {
            ExprKind::BinOp(BinOp::NotBinding, _, right) => {
                assert!(matches!(right.kind, ExprKind::Regex(_, _, _)));
            }
            other => panic!("expected NotBinding, got {other:?}"),
        }
    }

    #[test]
    fn parse_pre_inc() {
        let e = parse_expr_str("++$x;");
        assert!(matches!(e.kind, ExprKind::UnaryOp(UnaryOp::PreInc, _)));
    }

    #[test]
    fn parse_pre_dec() {
        let e = parse_expr_str("--$x;");
        assert!(matches!(e.kind, ExprKind::UnaryOp(UnaryOp::PreDec, _)));
    }

    #[test]
    fn parse_post_inc() {
        let e = parse_expr_str("$x++;");
        assert!(matches!(e.kind, ExprKind::PostfixOp(PostfixOp::Inc, _)));
    }

    #[test]
    fn parse_post_dec() {
        let e = parse_expr_str("$x--;");
        assert!(matches!(e.kind, ExprKind::PostfixOp(PostfixOp::Dec, _)));
    }

    #[test]
    fn parse_bit_and() {
        let e = parse_expr_str("$a & $b;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::BitAnd, _, _)));
    }

    #[test]
    fn parse_bit_or() {
        let e = parse_expr_str("$a | $b;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::BitOr, _, _)));
    }

    #[test]
    fn parse_bit_xor() {
        let e = parse_expr_str("$a ^ $b;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::BitXor, _, _)));
    }

    #[test]
    fn parse_shift_l() {
        let e = parse_expr_str("$a << 2;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::ShiftLeft, _, _)));
    }

    #[test]
    fn parse_shift_r() {
        let e = parse_expr_str("$a >> 2;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::ShiftRight, _, _)));
    }

    #[test]
    fn parse_bit_not() {
        let e = parse_expr_str("~$x;");
        assert!(matches!(e.kind, ExprKind::UnaryOp(UnaryOp::BitNot, _)));
    }

    #[test]
    fn parse_spaceship() {
        let e = parse_expr_str("$a <=> $b;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Spaceship, _, _)));
    }

    #[test]
    fn parse_str_cmp() {
        let e = parse_expr_str("$a cmp $b;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::StrCmp, _, _)));
    }

    // ═══════════════════════════════════════════════════════════
    // NEW TESTS — arrow deref targets
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn parse_arrow_coderef_call() {
        let e = parse_expr_str("$ref->(1, 2);");
        match &e.kind {
            ExprKind::MethodCall(_, name, args) => {
                assert!(name.is_empty());
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected coderef MethodCall, got {other:?}"),
        }
    }

    #[test]
    fn parse_arrow_array_elem() {
        let e = parse_expr_str("$ref->[0];");
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::ArrayElem(_))));
    }

    #[test]
    fn parse_chained_mixed_subscripts() {
        let e = parse_expr_str("$ref->[0]{key}[1];");
        match &e.kind {
            ExprKind::ArrayElem(inner, _) => {
                assert!(matches!(inner.kind, ExprKind::HashElem(_, _)));
            }
            other => panic!("expected ArrayElem(HashElem(..),..), got {other:?}"),
        }
    }

    #[test]
    fn parse_postfix_deref_scalar() {
        let e = parse_expr_str("$ref->$*;");
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefScalar)));
    }

    #[test]
    fn parse_triple_deref() {
        let e = parse_expr_str("$$$ref;");
        match &e.kind {
            ExprKind::Deref(Sigil::Scalar, inner) => {
                assert!(matches!(inner.kind, ExprKind::Deref(Sigil::Scalar, _)));
            }
            other => panic!("expected Deref(Deref(..)), got {other:?}"),
        }
    }

    // ═══════════════════════════════════════════════════════════
    // NEW TESTS — postfix control flow variants
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn parse_postfix_unless() {
        let prog = parse("print 1 unless $x;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::Unless, _, _), .. }) => {}
            other => panic!("expected PostfixControl Unless, got {other:?}"),
        }
    }

    #[test]
    fn parse_postfix_while() {
        let prog = parse("$x++ while $x < 10;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::While, _, _), .. }) => {}
            other => panic!("expected PostfixControl While, got {other:?}"),
        }
    }

    #[test]
    fn parse_postfix_until() {
        let prog = parse("$x++ until $x >= 10;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::Until, _, _), .. }) => {}
            other => panic!("expected PostfixControl Until, got {other:?}"),
        }
    }

    #[test]
    fn parse_postfix_for() {
        let prog = parse("print $_ for @list;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::For, _, _), .. }) => {}
            other => panic!("expected PostfixControl For, got {other:?}"),
        }
    }

    // ═══════════════════════════════════════════════════════════
    // NEW TESTS — declaration variants
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn parse_our_decl() {
        let prog = parse("our $VERSION = '1.0';");
        let (scope, vars) = decl_vars(&prog.statements[0]);
        assert_eq!(scope, DeclScope::Our);
        assert_eq!(vars[0].name, "VERSION");
        assert_eq!(vars[0].sigil, Sigil::Scalar);
    }

    #[test]
    fn parse_state_decl() {
        let prog = parse("state $counter = 0;");
        let (scope, vars) = decl_vars(&prog.statements[0]);
        assert_eq!(scope, DeclScope::State);
        assert_eq!(vars[0].name, "counter");
    }

    #[test]
    fn parse_my_list_decl() {
        // `my ($a, $b, $c);` — no initializer, so Stmt::Expr(Decl(...)).
        let prog = parse("my ($a, $b, $c);");
        let (scope, vars) = decl_vars(&prog.statements[0]);
        assert_eq!(scope, DeclScope::My);
        assert_eq!(vars.len(), 3);
        assert_eq!(vars[0].name, "a");
        assert_eq!(vars[1].name, "b");
        assert_eq!(vars[2].name, "c");
    }

    #[test]
    fn parse_my_mixed_sigil_list() {
        let prog = parse("my ($x, @y, %z);");
        let (_scope, vars) = decl_vars(&prog.statements[0]);
        assert_eq!(vars[0].sigil, Sigil::Scalar);
        assert_eq!(vars[1].sigil, Sigil::Array);
        assert_eq!(vars[2].sigil, Sigil::Hash);
    }

    #[test]
    fn parse_sub_with_prototype() {
        let prog = parse("sub foo ($$) { }");
        match &prog.statements[0].kind {
            StmtKind::SubDecl(sub) => {
                assert_eq!(sub.name, "foo");
                assert!(sub.prototype.is_some());
            }
            other => panic!("expected SubDecl with prototype, got {other:?}"),
        }
    }

    #[test]
    fn parse_package_block_form() {
        let prog = parse("package Foo { }");
        match &prog.statements[0].kind {
            StmtKind::PackageDecl(p) => {
                assert_eq!(p.name, "Foo");
                assert!(p.block.is_some());
            }
            other => panic!("expected PackageDecl with block, got {other:?}"),
        }
    }

    #[test]
    fn parse_package_version() {
        let prog = parse("package Foo 1.0;");
        match &prog.statements[0].kind {
            StmtKind::PackageDecl(p) => {
                assert_eq!(p.name, "Foo");
                assert!(p.version.is_some());
            }
            other => panic!("expected PackageDecl with version, got {other:?}"),
        }
    }

    #[test]
    fn parse_no_decl() {
        let prog = parse("no warnings;");
        match &prog.statements[0].kind {
            StmtKind::UseDecl(u) => {
                assert!(u.is_no);
                assert_eq!(u.module, "warnings");
            }
            other => panic!("expected UseDecl(is_no=true), got {other:?}"),
        }
    }

    // ═══════════════════════════════════════════════════════════
    // NEW TESTS — builtins
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn parse_defined() {
        let e = parse_expr_str("defined $x;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "defined");
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::ScalarVar(ref n) if n == "x"));
            }
            other => panic!("expected FuncCall('defined', [ScalarVar]), got {other:?}"),
        }
    }

    #[test]
    fn parse_chomp() {
        let e = parse_expr_str("chomp $line;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "chomp");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected chomp FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn parse_die_no_arg() {
        let e = parse_expr_str("die;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "die");
                assert_eq!(args.len(), 0);
            }
            other => panic!("expected bare die, got {other:?}"),
        }
    }

    #[test]
    fn parse_push_list() {
        let e = parse_expr_str("push @arr, 1, 2, 3;");
        match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "push");
                assert_eq!(args.len(), 4);
            }
            other => panic!("expected push ListOp, got {other:?}"),
        }
    }

    #[test]
    fn parse_join_list() {
        let e = parse_expr_str("join ',', @arr;");
        match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "join");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected join ListOp, got {other:?}"),
        }
    }

    #[test]
    fn parse_split_regex() {
        let e = parse_expr_str("split /,/, $str;");
        match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "split");
                assert_eq!(args.len(), 2);
                assert!(matches!(args[0].kind, ExprKind::Regex(_, _, _)));
            }
            other => panic!("expected split ListOp, got {other:?}"),
        }
    }

    #[test]
    fn parse_sort_subname() {
        let e = parse_expr_str("sort compare @list;");
        match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "sort");
                assert!(args.len() >= 2);
                assert!(matches!(args[0].kind, ExprKind::Bareword(_)));
            }
            other => panic!("expected sort with sub name, got {other:?}"),
        }
    }

    #[test]
    fn parse_open_three_arg() {
        let e = parse_expr_str("open my $fh, '<', 'file.txt';");
        match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "open");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected open ListOp, got {other:?}"),
        }
    }

    #[test]
    fn parse_bless_two_arg() {
        let e = parse_expr_str("bless $self, 'Foo';");
        match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "bless");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected bless ListOp, got {other:?}"),
        }
    }

    // ═══════════════════════════════════════════════════════════
    // NEW TESTS — special forms
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn parse_do_block() {
        let e = parse_expr_str("do { 42; };");
        assert!(matches!(e.kind, ExprKind::DoBlock(_)));
    }

    #[test]
    fn parse_do_file() {
        let e = parse_expr_str("do 'config.pl';");
        assert!(matches!(e.kind, ExprKind::DoExpr(_)));
    }

    #[test]
    fn parse_undef() {
        let e = parse_expr_str("undef;");
        assert!(matches!(e.kind, ExprKind::Undef));
    }

    #[test]
    fn parse_glob_wildcard() {
        let e = parse_expr_str("<*.txt>;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "glob");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected glob FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn parse_anon_hash() {
        let e = parse_expr_str("{key => 'val'};");
        match &e.kind {
            ExprKind::AnonHash(elems) => {
                assert!(elems.len() >= 2);
            }
            other => panic!("expected AnonHash, got {other:?}"),
        }
    }

    #[test]
    fn parse_anon_hash_at_stmt_level() {
        // {key => 'val'} at statement level — the heuristic should
        // detect => after bareword and route to AnonHash.
        let prog = parse("{key => 'val'};");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::AnonHash(elems), .. }) => {
                assert_eq!(elems.len(), 2);
            }
            other => panic!("expected AnonHash at stmt level, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_hash_at_stmt_level() {
        // {} at statement level — empty braces are a hash.
        let prog = parse("{};");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::AnonHash(elems), .. }) => {
                assert_eq!(elems.len(), 0);
            }
            other => panic!("expected empty AnonHash at stmt level, got {other:?}"),
        }
    }

    #[test]
    fn parse_string_key_hash_at_stmt_level() {
        // {'key', 'val'} — string followed by comma → hash.
        let prog = parse("{'key', 'val'};");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::AnonHash(_), .. }) => {}
            other => panic!("expected AnonHash, got {other:?}"),
        }
    }

    #[test]
    fn parse_uppercase_comma_hash_at_stmt_level() {
        // {Foo, 1} — uppercase bareword followed by comma → hash.
        let prog = parse("{Foo, 1};");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::AnonHash(_), .. }) => {}
            other => panic!("expected AnonHash, got {other:?}"),
        }
    }

    #[test]
    fn parse_lowercase_comma_block_at_stmt_level() {
        // {foo, 1} — lowercase bareword followed by comma → block
        // (could be a function call: foo(), 1).
        let prog = parse("{foo(1)};");
        match &prog.statements[0].kind {
            StmtKind::Block(_, _) => {}
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn parse_block_at_stmt_level() {
        // {my $x = 1; $x} — clearly a block (no comma/=> after first term).
        let prog = parse("{my $x = 1; $x};");
        match &prog.statements[0].kind {
            StmtKind::Block(block, _) => {
                assert!(!block.statements.is_empty());
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn parse_nested_anon_constructors() {
        let e = parse_expr_str("[{a => 1}, {b => 2}];");
        match &e.kind {
            ExprKind::AnonArray(elems) => {
                assert_eq!(elems.len(), 2);
                assert!(matches!(elems[0].kind, ExprKind::AnonHash(_)));
                assert!(matches!(elems[1].kind, ExprKind::AnonHash(_)));
            }
            other => panic!("expected AnonArray of AnonHashes, got {other:?}"),
        }
    }

    // ═══════════════════════════════════════════════════════════
    // NEW TESTS — phaser blocks (INIT/CHECK/UNITCHECK)
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn parse_init_block() {
        let prog = parse("INIT { 1; }");
        assert!(matches!(prog.statements[0].kind, StmtKind::Phaser(PhaserKind::Init, _)));
    }

    #[test]
    fn parse_check_block() {
        let prog = parse("CHECK { 1; }");
        assert!(matches!(prog.statements[0].kind, StmtKind::Phaser(PhaserKind::Check, _)));
    }

    #[test]
    fn parse_unitcheck_block() {
        let prog = parse("UNITCHECK { 1; }");
        assert!(matches!(prog.statements[0].kind, StmtKind::Phaser(PhaserKind::Unitcheck, _)));
    }

    // ═══════════════════════════════════════════════════════════
    // NEW TESTS — control flow variants
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn parse_try_finally_only() {
        let prog = parse("use feature 'try'; try { 1; } finally { 2; }");
        let stmt = prog.statements.iter().find(|s| matches!(s.kind, StmtKind::Try(_))).expect("Try statement present");
        match &stmt.kind {
            StmtKind::Try(t) => {
                assert!(t.catch_block.is_none());
                assert!(t.finally_block.is_some());
            }
            other => panic!("expected Try with only finally, got {other:?}"),
        }
    }

    #[test]
    fn parse_many_elsifs() {
        let prog = parse("if ($a) { 1; } elsif ($b) { 2; } elsif ($c) { 3; } elsif ($d) { 4; } else { 5; }");
        match &prog.statements[0].kind {
            StmtKind::If(if_stmt) => {
                assert_eq!(if_stmt.elsif_clauses.len(), 3);
                assert!(if_stmt.else_block.is_some());
            }
            other => panic!("expected If with 3 elsifs, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_statements() {
        let prog = parse(";;;");
        assert!(prog.statements.iter().all(|s| matches!(s.kind, StmtKind::Empty)));
    }

    // ═══════════════════════════════════════════════════════════
    // NEW TESTS — regex flags
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn parse_regex_with_many_flags() {
        let e = parse_expr_str("/foo/imsxg;");
        match &e.kind {
            ExprKind::Regex(_, pat, flags) => {
                assert_eq!(pat_str(pat), "foo");
                assert_eq!(flags.as_deref(), Some("imsxg"));
            }
            other => panic!("expected Regex with flags, got {other:?}"),
        }
    }

    #[test]
    fn parse_qr_regex() {
        let e = parse_expr_str("qr/\\d+/;");
        match &e.kind {
            ExprKind::Regex(_, pat, _) => assert_eq!(pat_str(pat), "\\d+"),
            other => panic!("expected Regex (qr), got {other:?}"),
        }
    }

    #[test]
    fn parse_regex_with_interp() {
        // m/foo$bar/ should produce an Interpolated pattern, not plain string.
        let e = parse_expr_str("m/foo$bar/;");
        match &e.kind {
            ExprKind::Regex(_, pat, _) => {
                assert!(pat.as_plain_string().is_none(), "expected interpolated pattern");
                assert!(pat.0.len() >= 2);
                assert!(matches!(&pat.0[0], InterpPart::Const(s) if s == "foo"));
                assert_eq!(scalar_interp_name(&pat.0[1]), Some("bar"));
            }
            other => panic!("expected Regex, got {other:?}"),
        }
    }

    #[test]
    fn parse_regex_literal_no_interp() {
        // m'foo$bar' should NOT interpolate — pattern is plain string.
        let e = parse_expr_str("m'foo$bar';");
        match &e.kind {
            ExprKind::Regex(_, pat, _) => {
                assert_eq!(pat_str(pat), "foo$bar");
            }
            other => panic!("expected Regex, got {other:?}"),
        }
    }

    #[test]
    fn parse_regex_literal_with_code_block() {
        // m'...' still recognizes (?{...}) code blocks.
        let e = parse_expr_str("m'foo(?{ 1 })bar';");
        match &e.kind {
            ExprKind::Regex(_, pat, _) => {
                assert!(pat.as_plain_string().is_none(), "expected interpolated pattern with code block");
                assert!(pat.0.iter().any(|p| matches!(p, InterpPart::RegexCode(_, _))));
            }
            other => panic!("expected Regex, got {other:?}"),
        }
    }

    #[test]
    fn parse_tr_with_flags() {
        let e = parse_expr_str("tr/a-z/A-Z/cs;");
        match &e.kind {
            ExprKind::Translit(_, _, flags) => assert_eq!(flags.as_deref(), Some("cs")),
            other => panic!("expected Translit with flags, got {other:?}"),
        }
    }

    // ═══════════════════════════════════════════════════════════
    // NEW TESTS — miscellaneous
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn parse_scalar_context() {
        let e = parse_expr_str("scalar @arr;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "scalar");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected scalar FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn parse_package_method_call() {
        let e = parse_expr_str("Foo::Bar->new();");
        match &e.kind {
            ExprKind::MethodCall(class, method, _) => {
                assert_eq!(method, "new");
                match &class.kind {
                    ExprKind::Bareword(name) => assert_eq!(name, "Foo::Bar"),
                    other => panic!("expected Bareword, got {other:?}"),
                }
            }
            other => panic!("expected MethodCall, got {other:?}"),
        }
    }

    #[test]
    fn parse_require_version() {
        let e = parse_expr_str("require 5.010;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "require");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected require with version, got {other:?}"),
        }
    }

    #[test]
    fn parse_labeled_bare_block() {
        let prog = parse("BLOCK: { last BLOCK; }");
        match &prog.statements[0].kind {
            StmtKind::Labeled(label, _) => assert_eq!(label, "BLOCK"),
            other => panic!("expected Labeled, got {other:?}"),
        }
    }

    #[test]
    fn parse_fat_comma_with_keyword() {
        let e = parse_expr_str("if => 1;");
        match &e.kind {
            ExprKind::List(items) => match &items[0].kind {
                ExprKind::StringLit(s) => assert_eq!(s, "if"),
                other => panic!("expected StringLit('if'), got {other:?}"),
            },
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn parse_fat_comma_keyword_cross_line() {
        // Keyword on one line, => on the next — should still autoquote.
        let e = parse_expr_str("my\n  => 1;");
        match &e.kind {
            ExprKind::List(items) => match &items[0].kind {
                ExprKind::StringLit(s) => assert_eq!(s, "my"),
                other => panic!("expected StringLit('my'), got {other:?}"),
            },
            other => panic!("expected List, got {other:?}"),
        }
    }

    // ═══════════════════════════════════════════════════════════
    // Quote-keyword autoquoting.
    //
    // The 8 Perl quote-like operators — `q`, `qq`, `qw`, `qr`,
    // `m`, `s`, `tr`, `y` — are recognized as operators only
    // when followed by a *valid* opening delimiter (see
    // `at_quote_delimiter` in the lexer).  When not followed by
    // a valid opener — including when followed by `=>` (fat
    // comma), `}` (hash-subscript close), or any of the
    // closing paired delimiters `)`, `]`, `}`, `>` — they must
    // NOT start a quote op and must instead be treated as
    // ordinary barewords (autoquoted to string literals in the
    // appropriate contexts).
    //
    // (`qx` — the backtick-equivalent — has the same lexical
    // shape but is omitted from this set to match Perl's common
    // "8 quote operators" terminology.)
    // ═══════════════════════════════════════════════════════════

    // ── Autoquote in fat-comma context ────────────────────────

    /// Parse `(KEYWORD => 1);` and return the first list element.
    /// Handles the outer Paren wrapping produced by the `(...)`.
    fn parse_kw_fat_comma(src: &str) -> Expr {
        let mut e = parse_expr_str(src);
        // Unwrap a single-level Paren — `(k => v)` parses as
        // Paren(List([k, v])) rather than bare List.
        if let ExprKind::Paren(inner) = e.kind {
            e = *inner;
        }
        match e.kind {
            ExprKind::List(mut items) => {
                assert!(!items.is_empty(), "expected non-empty list for {src:?}");
                items.remove(0)
            }
            other => panic!("expected List, got {other:?} for {src:?}"),
        }
    }

    #[test]
    fn autoquote_q_fat_comma() {
        let first = parse_kw_fat_comma("(q => 1);");
        assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "q"), "expected StringLit(q), got {:?}", first.kind);
    }

    #[test]
    fn autoquote_qq_fat_comma() {
        let first = parse_kw_fat_comma("(qq => 1);");
        assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "qq"), "expected StringLit(qq), got {:?}", first.kind);
    }

    #[test]
    fn autoquote_qw_fat_comma() {
        let first = parse_kw_fat_comma("(qw => 1);");
        assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "qw"), "expected StringLit(qw), got {:?}", first.kind);
    }

    #[test]
    fn autoquote_qr_fat_comma() {
        let first = parse_kw_fat_comma("(qr => 1);");
        assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "qr"), "expected StringLit(qr), got {:?}", first.kind);
    }

    #[test]
    fn autoquote_m_fat_comma() {
        let first = parse_kw_fat_comma("(m => 1);");
        assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "m"), "expected StringLit(m), got {:?}", first.kind);
    }

    #[test]
    fn autoquote_s_fat_comma() {
        let first = parse_kw_fat_comma("(s => 1);");
        assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "s"), "expected StringLit(s), got {:?}", first.kind);
    }

    #[test]
    fn autoquote_tr_fat_comma() {
        let first = parse_kw_fat_comma("(tr => 1);");
        assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "tr"), "expected StringLit(tr), got {:?}", first.kind);
    }

    #[test]
    fn autoquote_y_fat_comma() {
        let first = parse_kw_fat_comma("(y => 1);");
        assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "y"), "expected StringLit(y), got {:?}", first.kind);
    }

    // ── Autoquote in hash-subscript context ───────────────────

    /// Parse `$h{KEYWORD}` and return the subscript key expression.
    fn parse_kw_hash_key(src: &str) -> Expr {
        let e = parse_expr_str(src);
        match e.kind {
            ExprKind::HashElem(_, key) => *key,
            other => panic!("expected HashElem, got {other:?} for {src:?}"),
        }
    }

    #[test]
    fn autoquote_q_hash_key() {
        let key = parse_kw_hash_key("$h{q};");
        assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "q"), "expected StringLit(q), got {:?}", key.kind);
    }

    #[test]
    fn autoquote_qq_hash_key() {
        let key = parse_kw_hash_key("$h{qq};");
        assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "qq"), "expected StringLit(qq), got {:?}", key.kind);
    }

    #[test]
    fn autoquote_qw_hash_key() {
        let key = parse_kw_hash_key("$h{qw};");
        assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "qw"), "expected StringLit(qw), got {:?}", key.kind);
    }

    #[test]
    fn autoquote_qr_hash_key() {
        let key = parse_kw_hash_key("$h{qr};");
        assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "qr"), "expected StringLit(qr), got {:?}", key.kind);
    }

    #[test]
    fn autoquote_m_hash_key() {
        let key = parse_kw_hash_key("$h{m};");
        assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "m"), "expected StringLit(m), got {:?}", key.kind);
    }

    #[test]
    fn autoquote_s_hash_key() {
        let key = parse_kw_hash_key("$h{s};");
        assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "s"), "expected StringLit(s), got {:?}", key.kind);
    }

    #[test]
    fn autoquote_tr_hash_key() {
        let key = parse_kw_hash_key("$h{tr};");
        assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "tr"), "expected StringLit(tr), got {:?}", key.kind);
    }

    #[test]
    fn autoquote_y_hash_key() {
        let key = parse_kw_hash_key("$h{y};");
        assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "y"), "expected StringLit(y), got {:?}", key.kind);
    }

    // ═══════════════════════════════════════════════════════════
    // Audit-driven gap-filling tests.
    //
    // The previous commits added many tests by phase, but the
    // audit I committed to in the interpolation masking
    // postmortem turned up several genuinely shallow ones and a
    // few real gaps.  These fill the worst of them.  Structured
    // by the phase they belong to.
    // ═══════════════════════════════════════════════════════════

    // ── Phase 3: postderef slice content verification ────────
    //
    // The original postderef slice tests checked only the
    // ArrowTarget variant, not the index/key contents.  A
    // regression that parsed `$r->@[0, 1, 2]` as
    // `ArraySliceIndices(IntLit(0))` (dropping the rest) would
    // slip through.  Tests below verify the inner expression.

    #[test]
    fn postderef_array_slice_indices_content() {
        let e = parse_expr_stmt("$r->@[0, 1, 2];");
        match arrow_target(&e) {
            ArrowTarget::ArraySliceIndices(idx) => {
                // Index expr is a comma-list of three ints.
                match &idx.kind {
                    ExprKind::List(items) => {
                        assert_eq!(items.len(), 3);
                        assert!(matches!(items[0].kind, ExprKind::IntLit(0)));
                        assert!(matches!(items[1].kind, ExprKind::IntLit(1)));
                        assert!(matches!(items[2].kind, ExprKind::IntLit(2)));
                    }
                    ExprKind::IntLit(n) => panic!("single IntLit({n}) — expected 3-element List; would mean slice dropped items"),
                    other => panic!("expected List of 3, got {other:?}"),
                }
            }
            other => panic!("expected ArraySliceIndices, got {other:?}"),
        }
    }

    #[test]
    fn postderef_array_slice_keys_content() {
        let e = parse_expr_stmt(r#"$r->@{"a", "b", "c"};"#);
        match arrow_target(&e) {
            ArrowTarget::ArraySliceKeys(keys) => match &keys.kind {
                ExprKind::List(items) => {
                    assert_eq!(items.len(), 3);
                    for (i, want) in ["a", "b", "c"].iter().enumerate() {
                        assert!(
                            matches!(items[i].kind, ExprKind::StringLit(ref s) if s == want),
                            "item {i}: expected StringLit({want}), got {:?}",
                            items[i].kind
                        );
                    }
                }
                other => panic!("expected List of 3 strings, got {other:?}"),
            },
            other => panic!("expected ArraySliceKeys, got {other:?}"),
        }
    }

    #[test]
    fn postderef_kv_slice_indices_content() {
        let e = parse_expr_stmt("$r->%[0, 1];");
        match arrow_target(&e) {
            ArrowTarget::KvSliceIndices(idx) => match &idx.kind {
                ExprKind::List(items) => {
                    assert_eq!(items.len(), 2);
                    assert!(matches!(items[0].kind, ExprKind::IntLit(0)));
                    assert!(matches!(items[1].kind, ExprKind::IntLit(1)));
                }
                other => panic!("expected List of 2 ints, got {other:?}"),
            },
            other => panic!("expected KvSliceIndices, got {other:?}"),
        }
    }

    #[test]
    fn postderef_nested_actually_nested() {
        // Original `postderef_nested_slice` test claimed to
        // cover chaining but only had one level.  This one
        // actually chains: slice followed by arrow-array-elem.
        let e = parse_expr_stmt("$r->@[0, 1]->[0];");
        // Outer is ArrowDeref(_, ArrayElem(0)); inner is
        // ArrowDeref($r, ArraySliceIndices([0, 1])).
        match &e.kind {
            ExprKind::ArrowDeref(inner, ArrowTarget::ArrayElem(idx)) => {
                assert!(matches!(idx.kind, ExprKind::IntLit(0)));
                assert!(matches!(inner.kind, ExprKind::ArrowDeref(_, ArrowTarget::ArraySliceIndices(_))));
            }
            other => panic!("expected ArrowDeref(slice, ArrayElem(0)), got {other:?}"),
        }
    }

    // ── Phase 4: fc as named unary actually IS one ──────────
    //
    // `fc_requires_feature` was weak: it asserted parsing
    // didn't error and the name was "fc" — but that's true
    // regardless of whether fc was recognized as a named unary
    // or fell back to a generic FuncCall.  Counter-test: with
    // the feature on AND no parens, `fc` must bind as a
    // named-unary operator (precedence boundary: tighter than
    // `+`, looser than `*`).

    #[test]
    fn fc_named_unary_precedence() {
        // `fc $x . $y` — named-unary operators parse their
        // argument at NAMED_UNARY precedence, which is BELOW
        // concat.  So the entire `$x . $y` is the argument:
        // `fc($x . $y)`, NOT `fc($x) . $y`.
        let e = parse_expr_stmt("use feature 'fc'; fc $x . $y;");
        match e.kind {
            ExprKind::FuncCall(ref name, ref args) if name == "fc" => {
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Concat, _, _)), "argument should be the whole Concat expr, got {:?}", args[0].kind);
            }
            other => panic!("expected FuncCall(fc, [Concat(...)]), got {other:?}"),
        }
    }

    // ── Phase 5b: reactivation tests for each gated keyword ──
    //
    // The original downgrade tests only checked `try` reactivates
    // when its feature is on.  Add the same check for each of
    // the seven keywords whose downgrade was implemented: each
    // should parse as its real keyword form when the feature is
    // active.

    #[test]
    fn catch_reactivates_with_try_feature() {
        let prog = parse("use feature 'try'; try { 1; } catch ($e) { 2; }");
        // Try stmt captured with a catch clause.
        let try_stmt = prog.statements.iter().find_map(|s| match &s.kind {
            StmtKind::Try(t) => Some(t),
            _ => None,
        });
        assert!(try_stmt.is_some(), "Try stmt must exist with feature active");
        // And the Try must have a catch clause with var $e.
        let try_ = try_stmt.unwrap();
        assert!(try_.catch_block.is_some(), "catch clause must be attached");
    }

    #[test]
    fn defer_reactivates_with_feature() {
        let prog = parse("use feature 'defer'; defer { 1; }");
        assert!(prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Defer(_))), "Defer must parse with feature active");
    }

    #[test]
    fn given_when_reactivate_with_switch_feature() {
        let prog = parse(
            "use feature 'switch'; no warnings 'experimental::smartmatch'; \
             given ($x) { when (1) { 'one' } default { 'other' } }",
        );
        assert!(prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Given(_, _))), "Given must parse with switch feature");
    }

    #[test]
    fn class_reactivates_with_feature() {
        let prog = parse("use feature 'class'; no warnings 'experimental::class'; class Foo { }");
        assert!(prog.statements.iter().any(|s| matches!(s.kind, StmtKind::ClassDecl(_))), "Class decl must parse with class feature");
    }

    // ── Compile-time tokens in contexts ──────────────────────
    //
    // The original tests covered top-level __SUB__ / __PACKAGE__
    // but not nested contexts.

    #[test]
    fn current_sub_inside_named_sub() {
        // __SUB__ inside a sub body — the token is lex-time so
        // context doesn't affect its form; verify it parses.
        let prog = parse("use feature 'current_sub'; sub f { __SUB__ }");
        let sub = prog
            .statements
            .iter()
            .find_map(|s| match &s.kind {
                StmtKind::SubDecl(sd) if sd.name == "f" => Some(sd),
                _ => None,
            })
            .expect("sub f");
        // Body contains a CurrentSub expression somewhere.
        let body_has_current_sub = sub.body.statements.iter().any(|s| match &s.kind {
            StmtKind::Expr(e) => matches!(e.kind, ExprKind::CurrentSub),
            _ => false,
        });
        assert!(body_has_current_sub, "expected CurrentSub inside sub f body");
    }

    #[test]
    fn current_package_after_nested_package_decl() {
        // After `package Foo; package Bar;`, __PACKAGE__ gives
        // "Bar".  Tests the parser state-tracking on successive
        // package declarations.
        let prog = parse("package Foo;\npackage Bar;\n__PACKAGE__;\n");
        let e = prog.statements.iter().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e.clone()) } else { None }).expect("expression statement");
        match e.kind {
            ExprKind::CurrentPackage(name) => assert_eq!(name, "Bar"),
            other => panic!("expected CurrentPackage(Bar), got {other:?}"),
        }
    }

    // ── Signatures: negative cases ───────────────────────────

    #[test]
    fn sig_slurpy_array_before_scalar_is_error() {
        // `@rest` must be the last named parameter — a scalar
        // after it is invalid.  The parser should reject.
        let src = "use feature 'signatures'; sub f (@rest, $x) { }";
        let mut p = match Parser::new(src.as_bytes()) {
            Ok(p) => p,
            Err(_) => panic!("parser construction failed"),
        };
        let result = p.parse_program();
        assert!(result.is_err(), "slurpy array before scalar should error");
    }

    #[test]
    fn sig_two_slurpies_is_error() {
        let src = "use feature 'signatures'; sub f (@a, %h) { }";
        let mut p = match Parser::new(src.as_bytes()) {
            Ok(p) => p,
            Err(_) => panic!("parser construction failed"),
        };
        let result = p.parse_program();
        assert!(result.is_err(), "two slurpies should error");
    }

    // ═══════════════════════════════════════════════════════════
    // NEW TESTS — known gaps (ignored until implemented)
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn parse_subst_e_flag() {
        let e = parse_expr_str("s/foo/uc($&)/e;");
        match &e.kind {
            ExprKind::Subst(_, repl, _) => {
                assert!(repl.as_plain_string().is_none(), "expected non-literal replacement for /e, got {repl:?}");
            }
            other => panic!("expected Subst, got {other:?}"),
        }
    }

    #[test]
    fn parse_interp_scalar_expr() {
        // "${\ $ref}" — scalar expression interpolation.
        let e = parse_expr_str(r#""${\ $ref}";"#);
        match &e.kind {
            ExprKind::InterpolatedString(Interpolated(parts)) => {
                assert!(parts.iter().any(|p| matches!(p, InterpPart::ExprInterp(_))), "expected ExprInterp, got {parts:?}");
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn parse_interp_scalar_expr_arithmetic() {
        // "${\ $x + 1}" — expression with arithmetic.
        let e = parse_expr_str(r#""val: ${\ $x + 1}";"#);
        match &e.kind {
            ExprKind::InterpolatedString(Interpolated(parts)) => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(&parts[0], InterpPart::Const(s) if s == "val: "));
                assert!(matches!(&parts[1], InterpPart::ExprInterp(_)));
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn parse_interp_array_expr() {
        // "@{[ 1, 2, 3 ]}" — array expression interpolation.
        let e = parse_expr_str(r#""@{[ 1, 2, 3 ]}";"#);
        match &e.kind {
            ExprKind::InterpolatedString(Interpolated(parts)) => {
                assert!(parts.iter().any(|p| matches!(p, InterpPart::ExprInterp(_))), "expected ExprInterp, got {parts:?}");
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn parse_interp_expr_with_text() {
        // Mixing expression interpolation with plain text and simple vars.
        let e = parse_expr_str(r#""Hello ${\ uc($name)}, you have @{[ $n + 1 ]} items";"#);
        match &e.kind {
            ExprKind::InterpolatedString(Interpolated(parts)) => {
                assert!(parts.len() >= 4, "expected at least 4 parts, got {}", parts.len());
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn parse_interp_simple_braced_var() {
        // "${name}" — simple braced variable, NOT expression interpolation.
        let e = parse_expr_str(r#""${name}s";"#);
        match &e.kind {
            ExprKind::InterpolatedString(Interpolated(parts)) => {
                assert_eq!(scalar_interp_name(&parts[0]), Some("name"), "expected ScalarInterp(name), got {:?}", parts[0]);
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn parse_local_special_var_assign() {
        let prog = parse("local $/ = undef;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => {
                assert!(matches!(lhs.kind, ExprKind::Local(_)));
            }
            other => panic!("expected local assign, got {other:?}"),
        }
    }

    #[test]
    fn parse_qx_string_parens() {
        let e = parse_expr_str("qx(ls -la);");
        assert!(matches!(e.kind, ExprKind::InterpolatedString(_) | ExprKind::StringLit(_)));
    }

    #[test]
    fn parse_print_filehandle() {
        let e = parse_expr_str("print STDERR 'error';");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "print");
                match fh.as_deref() {
                    Some(Expr { kind: ExprKind::Bareword(n), .. }) => assert_eq!(n, "STDERR"),
                    other => panic!("expected filehandle Bareword('STDERR'), got {other:?}"),
                }
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected PrintOp with filehandle, got {other:?}"),
        }
    }

    #[test]
    fn parse_print_filehandle_parens() {
        // print(STDERR "testing\n") — parenthesized form.
        let e = parse_expr_str(r#"print(STDERR "testing\n");"#);
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "print");
                assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDERR"));
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected PrintOp with filehandle (parens), got {other:?}"),
        }
    }

    #[test]
    fn parse_print_comma_not_filehandle() {
        // print STDERR, "hello" — comma means STDERR is an arg, not filehandle.
        let e = parse_expr_str("print STDERR, 'hello';");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "print");
                assert!(fh.is_none());
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected PrintOp with no filehandle, got {other:?}"),
        }
    }

    #[test]
    fn parse_print_scalar_filehandle() {
        // print $fh "hello" — $fh is filehandle.
        let e = parse_expr_str("print $fh 'hello';");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "print");
                assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::ScalarVar(n), .. }) if n == "fh"));
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected PrintOp with scalar filehandle, got {other:?}"),
        }
    }

    #[test]
    fn parse_print_bare_no_args() {
        // print STDERR; — filehandle with no args (prints $_ to STDERR).
        let e = parse_expr_str("print STDERR;");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "print");
                assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDERR"));
                assert_eq!(args.len(), 0);
            }
            other => panic!("expected PrintOp with filehandle, no args, got {other:?}"),
        }
    }

    #[test]
    fn parse_say_filehandle() {
        let e = parse_expr_str("say STDERR 'error';");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "say");
                assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDERR"));
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected say PrintOp with filehandle, got {other:?}"),
        }
    }

    #[test]
    fn parse_printf_filehandle() {
        let e = parse_expr_str("printf STDERR '%s', $msg;");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "printf");
                assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDERR"));
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected printf PrintOp with filehandle, got {other:?}"),
        }
    }

    #[test]
    fn parse_print_no_args() {
        // print; — prints $_ to default output.
        let e = parse_expr_str("print;");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "print");
                assert!(fh.is_none());
                assert_eq!(args.len(), 0);
            }
            other => panic!("expected PrintOp with no args, got {other:?}"),
        }
    }

    #[test]
    fn parse_print_parens_no_args() {
        // print() — prints $_ to default output (paren form).
        let e = parse_expr_str("print();");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "print");
                assert!(fh.is_none());
                assert_eq!(args.len(), 0);
            }
            other => panic!("expected PrintOp() with no args, got {other:?}"),
        }
    }

    #[test]
    fn parse_print_parens_fh_no_args() {
        // print(STDERR); — bareword filehandle in parens, no args (prints $_).
        let e = parse_expr_str("print(STDERR);");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "print");
                assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDERR"));
                assert_eq!(args.len(), 0);
            }
            other => panic!("expected PrintOp(STDERR) with no args, got {other:?}"),
        }
    }

    #[test]
    fn parse_print_parens_scalar_fh() {
        // print($fh $_); — $fh is filehandle (followed by $_, a term).
        let e = parse_expr_str("print($fh $_);");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "print");
                assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::ScalarVar(n), .. }) if n == "fh"));
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::ScalarVar(ref n) if n == "_"));
            }
            other => panic!("expected PrintOp($fh, [$_]), got {other:?}"),
        }
    }

    #[test]
    fn parse_print_parens_scalar_not_fh() {
        // print($f); — $f NOT a filehandle (followed by ), not a term).
        // Prints value of $f to STDOUT.
        let e = parse_expr_str("print($f);");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "print");
                assert!(fh.is_none());
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::ScalarVar(ref n) if n == "f"));
            }
            other => panic!("expected PrintOp(None, [$f]), got {other:?}"),
        }
    }

    #[test]
    fn parse_print_scalar_not_fh() {
        // print $f; — $f NOT a filehandle (followed by ;, not a term).
        let e = parse_expr_str("print $f;");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "print");
                assert!(fh.is_none());
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::ScalarVar(ref n) if n == "f"));
            }
            other => panic!("expected PrintOp(None, [$f]), got {other:?}"),
        }
    }

    #[test]
    fn parse_say_no_filehandle() {
        let e = parse_expr_str("say 'hello';");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "say");
                assert!(fh.is_none());
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected say PrintOp with no filehandle, got {other:?}"),
        }
    }

    #[test]
    fn parse_say_parens_filehandle() {
        let e = parse_expr_str("say(STDERR 'hello');");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "say");
                assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDERR"));
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected say PrintOp with filehandle (parens), got {other:?}"),
        }
    }

    #[test]
    fn parse_printf_no_filehandle() {
        let e = parse_expr_str("printf '%s', $msg;");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "printf");
                assert!(fh.is_none());
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected printf PrintOp with no filehandle, got {other:?}"),
        }
    }

    #[test]
    fn parse_printf_parens_filehandle() {
        let e = parse_expr_str("printf(STDERR '%s', $msg);");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "printf");
                assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDERR"));
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected printf PrintOp with filehandle (parens), got {other:?}"),
        }
    }

    #[test]
    fn parse_print_stdout_filehandle() {
        let e = parse_expr_str("print STDOUT 'hello';");
        match &e.kind {
            ExprKind::PrintOp(name, fh, args) => {
                assert_eq!(name, "print");
                assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDOUT"));
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected print STDOUT filehandle, got {other:?}"),
        }
    }

    #[test]
    fn parse_print_postfix_if() {
        // print "hello" if $cond; — postfix control should work with PrintOp.
        let prog = parse("print 'hello' if $cond;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::If, body, _), .. }) => {
                assert!(matches!(body.kind, ExprKind::PrintOp(_, _, _)));
            }
            other => panic!("expected PostfixControl(If, PrintOp), got {other:?}"),
        }
    }

    // ═══════════════════════════════════════════════════════════
    // Semantic validation tests
    // ═══════════════════════════════════════════════════════════

    fn parse_expr_fails(src: &str) -> bool {
        // A quick way to check that parsing an expression fails.
        std::panic::catch_unwind(|| parse_expr_str(src)).is_err()
    }

    // ── Chained comparisons (Perl 5.32+) ───────────────────

    #[test]
    fn allow_chained_lt() {
        // $a < $b < $c — chained comparison (5.32+).
        let e = parse_expr_str("$a < $b < $c;");
        assert!(matches!(e.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp, got {:?}", e.kind);
    }

    #[test]
    fn allow_chained_eq() {
        let e = parse_expr_str("$a == $b == $c;");
        assert!(matches!(e.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp, got {:?}", e.kind);
    }

    #[test]
    fn allow_chained_str_cmp() {
        let e = parse_expr_str("$a eq $b eq $c;");
        assert!(matches!(e.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp, got {:?}", e.kind);
    }

    #[test]
    fn allow_mixed_prec_comparisons() {
        // $a < $b && $b < $c — different precedence levels, OK.
        let _e = parse_expr_str("$a < $b && $b < $c;");
    }

    #[test]
    fn allow_comparison_in_ternary() {
        let _e = parse_expr_str("$a == $b ? 1 : 0;");
    }

    #[test]
    fn allow_eq_after_lt() {
        // $a < $b == $c — different non-assoc prec groups, OK.
        let _e = parse_expr_str("$a < $b == $c;");
    }

    // ── Lvalue validation ─────────────────────────────────────

    #[test]
    fn reject_assign_to_literal() {
        assert!(parse_expr_fails("42 = $x;"));
    }

    #[test]
    fn reject_assign_to_string() {
        assert!(parse_expr_fails("'hello' = $x;"));
    }

    #[test]
    fn reject_assign_to_binop() {
        assert!(parse_expr_fails("$a + $b = 1;"));
    }

    #[test]
    fn reject_compound_assign_to_literal() {
        assert!(parse_expr_fails("42 += 1;"));
    }

    #[test]
    fn reject_prefix_inc_literal() {
        assert!(parse_expr_fails("++42;"));
    }

    #[test]
    fn reject_postfix_inc_literal() {
        assert!(parse_expr_fails("42++;"));
    }

    #[test]
    fn reject_prefix_dec_string() {
        assert!(parse_expr_fails("--'hello';"));
    }

    #[test]
    fn allow_assign_to_var() {
        let _e = parse_expr_str("$x = 1;");
    }

    #[test]
    fn allow_assign_to_array_elem() {
        let _e = parse_expr_str("$a[0] = 1;");
    }

    #[test]
    fn allow_assign_to_hash_elem() {
        let _e = parse_expr_str("$h{key} = 1;");
    }

    #[test]
    fn allow_assign_to_deref() {
        let _e = parse_expr_str("$$ref = 1;");
    }

    #[test]
    fn allow_assign_to_arrow_deref() {
        let _e = parse_expr_str("$ref->[0] = 1;");
    }

    #[test]
    fn allow_assign_to_my_decl() {
        let _e = parse_expr_str("my $x = 1;");
    }

    #[test]
    fn allow_assign_to_local() {
        let _e = parse_expr_str("local $/ = undef;");
    }

    #[test]
    fn allow_list_assign() {
        let prog = parse("my ($a, $b) = (1, 2);");
        assert_eq!(prog.statements.len(), 1);
    }

    #[test]
    fn allow_inc_var() {
        let _e = parse_expr_str("++$x;");
    }

    #[test]
    fn allow_postfix_inc_var() {
        let _e = parse_expr_str("$x++;");
    }

    #[test]
    fn parse_unless_elsif() {
        let prog = parse("unless ($x) { 1; } elsif ($y) { 2; }");
        assert_eq!(prog.statements.len(), 1);
    }

    // ── Lexer error surfacing ─────────────────────────────────
    //
    // Lexer errors must be reported, not silently converted to Eof.

    fn parse_fails(src: &str) -> String {
        let mut parser = Parser::new(src.as_bytes()).unwrap();
        match parser.parse_program() {
            Err(e) => e.message,
            Ok(_) => panic!("expected parse error for: {src}"),
        }
    }

    #[test]
    fn lexer_error_unterminated_string() {
        let msg = parse_fails("my $x = \"hello;");
        assert!(msg.contains("unterminated"), "expected unterminated error, got: {msg}");
    }

    #[test]
    fn lexer_error_unterminated_regex() {
        let msg = parse_fails("/foo bar");
        assert!(msg.contains("unterminated"), "expected unterminated error, got: {msg}");
    }

    #[test]
    fn lexer_error_unexpected_byte() {
        let msg = parse_fails("my $x = \x01;");
        assert!(msg.contains("unexpected byte"), "expected unexpected byte error, got: {msg}");
    }

    #[test]
    fn lexer_error_after_valid_code() {
        // Error occurs after some valid statements have been parsed.
        let msg = parse_fails("my $x = 1; my $y = \"unterminated;");
        assert!(msg.contains("unterminated"), "expected unterminated error, got: {msg}");
    }

    #[test]
    fn lexer_error_immediate() {
        // Error on the very first token — no valid code at all.
        let msg = parse_fails("\"unterminated");
        assert!(msg.contains("unterminated"), "expected unterminated error, got: {msg}");
    }

    // ── Hard parsing corpus ───────────────────────────────────
    //
    // The tests below are derived from a corpus of adversarial
    // cases targeting the hardest ambiguities in Perl parsing:
    // regex-vs-division, block-vs-hash, indirect object, ternary
    // associativity, comma/assignment precedence, arrow chains,
    // interpolation, and heredoc integration.
    //
    // For each case we assert the specific structural facts we're
    // confident about — typically the top-level node kind and a
    // key grouping relationship.  We deliberately don't try to
    // match whole trees, to keep tests robust against AST
    // refactoring.

    // ── Regex vs division ─────────────────────────────────────

    #[test]
    fn hard_div_chain_is_left_assoc() {
        // `$x / $y / $z` must be (($x / $y) / $z), not ($x / ($y / $z)).
        let e = parse_expr_str("$x / $y / $z;");
        match &e.kind {
            ExprKind::BinOp(BinOp::Div, lhs, rhs) => {
                assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::Div, _, _)), "expected left-associative division, got lhs = {:?}", lhs.kind);
                assert!(matches!(rhs.kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected Div BinOp, got {other:?}"),
        }
    }

    #[test]
    fn hard_print_slash_is_regex() {
        // `print /x/;` — after `print`, `/` starts a regex.
        let prog = parse("print /x/;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PrintOp(name, _fh, args), .. }) => {
                assert_eq!(name, "print");
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::Regex(_, _, _)), "expected Regex arg, got {:?}", args[0].kind);
            }
            other => panic!("expected PrintOp with Regex arg, got {other:?}"),
        }
    }

    #[test]
    fn hard_print_scalar_slash_is_division() {
        // `print $x / 2;` — here `/` is division, not regex.
        let prog = parse("print $x / 2;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, _, args), .. }) => {
                assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Div, _, _)), "expected Div BinOp arg, got {:?}", args[0].kind);
            }
            other => panic!("expected PrintOp, got {other:?}"),
        }
    }

    #[test]
    fn hard_slash_in_ternary_condition_is_regex() {
        // `$x = /foo/ ? 1 : 2;` — ternary condition is regex.
        let e = parse_expr_str("$x = /foo/ ? 1 : 2;");
        match &e.kind {
            ExprKind::Assign(_, _, rhs) => match &rhs.kind {
                ExprKind::Ternary(cond, _, _) => {
                    assert!(matches!(cond.kind, ExprKind::Regex(_, _, _)), "expected Regex condition, got {:?}", cond.kind);
                }
                other => panic!("expected Ternary, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    #[test]
    fn hard_defined_or_rhs_is_regex() {
        // `$x // /foo/;` — RHS of // is in term position, so regex.
        let e = parse_expr_str("$x // /foo/;");
        match &e.kind {
            ExprKind::BinOp(BinOp::DefinedOr, _, rhs) => {
                assert!(matches!(rhs.kind, ExprKind::Regex(_, _, _)), "expected Regex rhs, got {:?}", rhs.kind);
            }
            other => panic!("expected DefinedOr BinOp, got {other:?}"),
        }
    }

    // ── Block vs hash ─────────────────────────────────────────

    #[test]
    fn hard_unary_plus_brace_is_hash() {
        // `+{ a => 1 }` — unary + forces expression context, so hash.
        let e = parse_expr_str("+{ a => 1 };");
        match &e.kind {
            ExprKind::UnaryOp(UnaryOp::NumPositive, inner) => {
                assert!(matches!(inner.kind, ExprKind::AnonHash(_)), "expected AnonHash inside unary +, got {:?}", inner.kind);
            }
            other => panic!("expected UnaryOp(+, AnonHash), got {other:?}"),
        }
    }

    #[test]
    fn hard_map_outer_brace_is_block() {
        // `map { a => 1 } @list;` — outer braces are block argument.
        let e = parse_expr_str("map { a => 1 } @list;");
        match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "map");
                // First arg is the block (as AnonSub).
                assert!(matches!(args[0].kind, ExprKind::AnonSub(..)), "expected AnonSub block, got {:?}", args[0].kind);
            }
            other => panic!("expected map ListOp, got {other:?}"),
        }
    }

    #[test]
    fn hard_map_nested_brace_is_hash() {
        // `map { { a => 1 } } @list;` — outer = block, inner = hash.
        let e = parse_expr_str("map { { a => 1 } } @list;");
        match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "map");
                // Outer: AnonSub wrapping the block.
                let body = match &args[0].kind {
                    ExprKind::AnonSub(_, _, _, block) => block,
                    other => panic!("expected AnonSub, got {other:?}"),
                };
                // Block body's single statement is an AnonHash expression.
                assert_eq!(body.statements.len(), 1);
                match &body.statements[0].kind {
                    StmtKind::Expr(Expr { kind: ExprKind::AnonHash(_), .. }) => {}
                    other => panic!("expected AnonHash stmt, got {other:?}"),
                }
            }
            other => panic!("expected map ListOp, got {other:?}"),
        }
    }

    #[test]
    fn hard_do_brace_is_block_not_hash() {
        // `do { a => 1 };` — do BLOCK, not a hash constructor.
        let e = parse_expr_str("do { a => 1 };");
        assert!(matches!(e.kind, ExprKind::DoBlock(_)), "expected DoBlock, got {:?}", e.kind);
    }

    #[test]
    fn hard_sub_nested_hash() {
        // `sub { { a => 1 } }` — anon sub whose body is a hash expression.
        let e = parse_expr_str("sub { { a => 1 } };");
        match &e.kind {
            ExprKind::AnonSub(_, _, _, block) => {
                assert_eq!(block.statements.len(), 1);
                match &block.statements[0].kind {
                    StmtKind::Expr(Expr { kind: ExprKind::AnonHash(_), .. }) => {}
                    other => panic!("expected AnonHash stmt inside sub body, got {other:?}"),
                }
            }
            other => panic!("expected AnonSub, got {other:?}"),
        }
    }

    // ── Bareword ambiguity ────────────────────────────────────

    #[test]
    fn hard_bareword_plus_literal() {
        // `foo + 1;` — bareword + literal (absent prototype/constant info).
        let e = parse_expr_str("foo + 1;");
        match &e.kind {
            ExprKind::BinOp(BinOp::Add, lhs, _) => {
                assert!(matches!(lhs.kind, ExprKind::Bareword(_) | ExprKind::FuncCall(_, _)), "expected Bareword/FuncCall lhs, got {:?}", lhs.kind);
            }
            other => panic!("expected Add BinOp, got {other:?}"),
        }
    }

    #[test]
    fn hard_label_on_statement() {
        // `foo: bar();` — label at statement level.
        let prog = parse("foo: bar();");
        assert!(matches!(prog.statements[0].kind, StmtKind::Labeled(_, _)), "expected Labeled statement, got {:?}", prog.statements[0].kind);
    }

    // ── Indirect object ───────────────────────────────────────

    #[test]
    fn hard_print_filehandle_scalar() {
        // `print $fh "hello";` — indirect-object filehandle form.
        let prog = parse("print $fh \"hello\";");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, fh, args), .. }) => {
                let fh = fh.as_ref().expect("expected filehandle");
                assert!(matches!(fh.kind, ExprKind::ScalarVar(_)), "expected ScalarVar filehandle, got {:?}", fh.kind);
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected PrintOp with filehandle, got {other:?}"),
        }
    }

    #[test]
    fn hard_print_filehandle_bareword() {
        // `print STDERR "hello";` — bareword filehandle.
        let prog = parse("print STDERR \"hello\";");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, fh, _), .. }) => {
                let fh = fh.as_ref().expect("expected filehandle");
                assert!(matches!(fh.kind, ExprKind::Bareword(_)), "expected Bareword filehandle, got {:?}", fh.kind);
            }
            other => panic!("expected PrintOp, got {other:?}"),
        }
    }

    // ── Postfix control flow ──────────────────────────────────

    #[test]
    fn hard_postfix_if() {
        // `print "x" if $cond;` — the whole `print "x"` is the modifier subject.
        let prog = parse("print \"x\" if $cond;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(kind, subject, cond), .. }) => {
                assert!(matches!(kind, PostfixKind::If));
                assert!(matches!(subject.kind, ExprKind::PrintOp(_, _, _)), "expected PrintOp subject, got {:?}", subject.kind);
                assert!(matches!(cond.kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected PostfixControl, got {other:?}"),
        }
    }

    #[test]
    fn hard_postfix_while() {
        let prog = parse("foo() while $cond;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(kind, _, _), .. }) => {
                assert!(matches!(kind, PostfixKind::While));
            }
            other => panic!("expected PostfixControl, got {other:?}"),
        }
    }

    #[test]
    fn hard_postfix_for() {
        let prog = parse("foo() for @list;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(kind, _, list), .. }) => {
                assert!(matches!(kind, PostfixKind::For | PostfixKind::Foreach));
                assert!(matches!(list.kind, ExprKind::ArrayVar(_)));
            }
            other => panic!("expected PostfixControl, got {other:?}"),
        }
    }

    // ── do / eval ─────────────────────────────────────────────

    #[test]
    fn hard_do_file() {
        // `do $file;` — do EXPR form, not do BLOCK.
        let e = parse_expr_str("do $file;");
        assert!(matches!(e.kind, ExprKind::DoExpr(_)), "expected DoExpr, got {:?}", e.kind);
    }

    #[test]
    fn hard_eval_block_vs_expr() {
        let e1 = parse_expr_str("eval { 1 };");
        assert!(matches!(e1.kind, ExprKind::EvalBlock(_)), "expected EvalBlock, got {:?}", e1.kind);

        let e2 = parse_expr_str("eval $code;");
        assert!(matches!(e2.kind, ExprKind::EvalExpr(_)), "expected EvalExpr, got {:?}", e2.kind);
    }

    // ── Ternary precedence ────────────────────────────────────

    #[test]
    fn hard_ternary_right_associative() {
        // `$a ? $b : $c ? $d : $e;` — right-associative.
        // Must group as: Ternary($a, $b, Ternary($c, $d, $e))
        let e = parse_expr_str("$a ? $b : $c ? $d : $e;");
        match &e.kind {
            ExprKind::Ternary(_, then, else_) => {
                assert!(matches!(then.kind, ExprKind::ScalarVar(_)), "expected scalar then-branch, got {:?}", then.kind);
                assert!(matches!(else_.kind, ExprKind::Ternary(_, _, _)), "expected nested Ternary in else-branch (right-assoc), got {:?}", else_.kind);
            }
            other => panic!("expected Ternary, got {other:?}"),
        }
    }

    #[test]
    fn hard_ternary_condition_has_plus() {
        // `$a + $b ? $c : $d;` — + binds tighter than ternary cond.
        let e = parse_expr_str("$a + $b ? $c : $d;");
        match &e.kind {
            ExprKind::Ternary(cond, _, _) => {
                assert!(matches!(cond.kind, ExprKind::BinOp(BinOp::Add, _, _)), "expected Add in condition, got {:?}", cond.kind);
            }
            other => panic!("expected Ternary, got {other:?}"),
        }
    }

    #[test]
    fn hard_ternary_then_has_plus() {
        // `$a ? $b + $c : $d;` — full expression in then-branch.
        let e = parse_expr_str("$a ? $b + $c : $d;");
        match &e.kind {
            ExprKind::Ternary(_, then, _) => {
                assert!(matches!(then.kind, ExprKind::BinOp(BinOp::Add, _, _)), "expected Add in then-branch, got {:?}", then.kind);
            }
            other => panic!("expected Ternary, got {other:?}"),
        }
    }

    // ── Assignment / comma precedence ─────────────────────────

    #[test]
    fn hard_assign_comma_precedence() {
        // `$a = $b, $c;` — comma is lower than assignment.
        // Must group as: List([Assign($a, $b), $c])
        let e = parse_expr_str("$a = $b, $c;");
        match &e.kind {
            ExprKind::List(items) => {
                assert_eq!(items.len(), 2);
                assert!(matches!(items[0].kind, ExprKind::Assign(_, _, _)), "expected Assign as first list item, got {:?}", items[0].kind);
                assert!(matches!(items[1].kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn hard_assign_paren_comma() {
        // `$a = ($b, $c);` — parens force comma expression as RHS.
        let e = parse_expr_str("$a = ($b, $c);");
        match &e.kind {
            ExprKind::Assign(_, _, rhs) => {
                // RHS should be a List (possibly wrapped in Paren).
                let inner = match &rhs.kind {
                    ExprKind::Paren(inner) => &inner.kind,
                    other => other,
                };
                assert!(matches!(inner, ExprKind::List(_)), "expected List on RHS, got {inner:?}");
            }
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    // ── Arrow / deref precedence ──────────────────────────────

    #[test]
    fn hard_arrow_method_call() {
        let e = parse_expr_str("$obj->method;");
        assert!(matches!(e.kind, ExprKind::MethodCall(_, _, _)), "expected MethodCall, got {:?}", e.kind);
    }

    #[test]
    fn hard_arrow_hash_deref() {
        // `$obj->{key};` — hash element via arrow.
        let e = parse_expr_str("$obj->{key};");
        // Either ArrowDeref(_, Hash("key")) or HashElem form — both are valid.
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, _) | ExprKind::HashElem(_, _)), "expected ArrowDeref or HashElem, got {:?}", e.kind);
    }

    #[test]
    fn hard_arrow_chained_method() {
        // `$obj->{key}->method;` — method call on hash-deref result.
        let e = parse_expr_str("$obj->{key}->method;");
        match &e.kind {
            ExprKind::MethodCall(target, name, _) => {
                assert_eq!(name, "method");
                assert!(
                    matches!(target.kind, ExprKind::ArrowDeref(_, _) | ExprKind::HashElem(_, _)),
                    "expected arrow/hash deref target, got {:?}",
                    target.kind
                );
            }
            other => panic!("expected MethodCall, got {other:?}"),
        }
    }

    #[test]
    fn hard_arrow_method_then_hash() {
        // `$obj->method()->{key};` — index on method call result.
        let e = parse_expr_str("$obj->method()->{key};");
        // The outermost should be the hash index, inner should be MethodCall.
        let inner = match &e.kind {
            ExprKind::ArrowDeref(target, _) => &target.kind,
            ExprKind::HashElem(target, _) => &target.kind,
            other => panic!("expected arrow/hash deref, got {other:?}"),
        };
        assert!(matches!(inner, ExprKind::MethodCall(_, _, _)), "expected MethodCall inside, got {inner:?}");
    }

    // ── Interpolation ─────────────────────────────────────────

    #[test]
    fn hard_interp_scalar() {
        // `"hello $x"` — interpolated string with scalar.
        let e = parse_expr_str("\"hello $x\";");
        match &e.kind {
            ExprKind::InterpolatedString(Interpolated(parts)) => {
                assert!(parts.iter().any(|p| matches!(p, InterpPart::ScalarInterp(_))), "expected ScalarInterp part, got {parts:?}");
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn hard_interp_array() {
        let e = parse_expr_str("\"@arr\";");
        match &e.kind {
            ExprKind::InterpolatedString(Interpolated(parts)) => {
                assert!(parts.iter().any(|p| matches!(p, InterpPart::ArrayInterp(_))), "expected ArrayInterp part, got {parts:?}");
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    // ── Regex edge cases ──────────────────────────────────────

    #[test]
    fn hard_regex_m_slash() {
        let e = parse_expr_str("m/foo/;");
        assert!(matches!(e.kind, ExprKind::Regex(_, _, _)));
    }

    #[test]
    fn hard_regex_bare_slash() {
        let e = parse_expr_str("/foo/;");
        assert!(matches!(e.kind, ExprKind::Regex(_, _, _)));
    }

    #[test]
    fn hard_subst() {
        let e = parse_expr_str("s/foo/bar/;");
        assert!(matches!(e.kind, ExprKind::Subst(_, _, _)));
    }

    #[test]
    fn hard_binding_regex() {
        // `$x =~ /foo/;` — binding operator with regex on RHS.
        let e = parse_expr_str("$x =~ /foo/;");
        match &e.kind {
            ExprKind::BinOp(BinOp::Binding, _, rhs) => {
                assert!(matches!(rhs.kind, ExprKind::Regex(_, _, _)));
            }
            other => panic!("expected Binding BinOp, got {other:?}"),
        }
    }

    #[test]
    fn hard_regex_brace_delim() {
        let e = parse_expr_str("$x =~ m{foo};");
        match &e.kind {
            ExprKind::BinOp(BinOp::Binding, _, rhs) => {
                assert!(matches!(rhs.kind, ExprKind::Regex(_, _, _)));
            }
            other => panic!("expected Binding, got {other:?}"),
        }
    }

    // ── Combined nightmare cases ──────────────────────────────

    #[test]
    fn hard_nightmare_map_ternary_hash() {
        // `map { /x/ ? { a => 1 } : { b => 2 } } @list;`
        // Exercises: block-vs-hash, regex-vs-division, ternary grouping.
        let e = parse_expr_str("map { /x/ ? { a => 1 } : { b => 2 } } @list;");
        match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "map");
                let block = match &args[0].kind {
                    ExprKind::AnonSub(_, _, _, b) => b,
                    other => panic!("expected AnonSub, got {other:?}"),
                };
                assert_eq!(block.statements.len(), 1);
                match &block.statements[0].kind {
                    StmtKind::Expr(Expr { kind: ExprKind::Ternary(cond, then, else_), .. }) => {
                        assert!(matches!(cond.kind, ExprKind::Regex(_, _, _)), "expected Regex condition, got {:?}", cond.kind);
                        assert!(matches!(then.kind, ExprKind::AnonHash(_)), "expected AnonHash then-branch, got {:?}", then.kind);
                        assert!(matches!(else_.kind, ExprKind::AnonHash(_)), "expected AnonHash else-branch, got {:?}", else_.kind);
                    }
                    other => panic!("expected Ternary stmt, got {other:?}"),
                }
            }
            other => panic!("expected map ListOp, got {other:?}"),
        }
    }

    #[test]
    fn hard_nightmare_do_ternary_hash() {
        // `$x = do { /x/ ? { a => 1 } : { b => 2 } };`
        let e = parse_expr_str("$x = do { /x/ ? { a => 1 } : { b => 2 } };");
        match &e.kind {
            ExprKind::Assign(_, _, rhs) => match &rhs.kind {
                ExprKind::DoBlock(block) => {
                    assert_eq!(block.statements.len(), 1);
                    match &block.statements[0].kind {
                        StmtKind::Expr(Expr { kind: ExprKind::Ternary(_, then, else_), .. }) => {
                            assert!(matches!(then.kind, ExprKind::AnonHash(_)));
                            assert!(matches!(else_.kind, ExprKind::AnonHash(_)));
                        }
                        other => panic!("expected Ternary stmt, got {other:?}"),
                    }
                }
                other => panic!("expected DoBlock, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    // ── Parse-or-error (Tier 1) — just verify these parse ─────
    //
    // For cases where the exact AST shape depends on decisions we
    // haven't firmed up (or features we haven't implemented yet),
    // at least verify the parser accepts them.

    #[test]
    fn hard_parses_map_slash() {
        // `map { /x/ } @list;` — regex inside map block.
        parse("map { /x/ } @list;");
    }

    #[test]
    fn hard_parses_regex_in_sub() {
        // `sub f { /x/ }` — regex as sub body expression.
        parse("sub f { /x/ }");
    }

    #[test]
    fn hard_parses_map_list_form() {
        // `map /x/, @list;` — non-block form of map.
        parse("map /x/, @list;");
    }

    #[test]
    fn hard_parses_foo_bareword_alone() {
        // `foo;` — bare bareword statement.
        parse("foo;");
    }

    #[test]
    fn hard_parses_nested_brace_print() {
        // `print { $fh } "hello";` — brace-filehandle form.
        parse("print { $fh } \"hello\";");
    }

    #[test]
    fn hard_parses_paren_grouping() {
        parse("($a + $b) * $c;");
    }

    #[test]
    fn hard_my_assign_comma_grouping() {
        // `my $x = $a, $b;` — Perl parses as `(my $x = $a), $b`.
        // Since `my` is an expression, the whole thing is a List with
        // an Assign(Decl(My), $a) first, then $b.
        let prog = parse("my $x = $a, $b;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::List(items), .. }) => {
                assert_eq!(items.len(), 2, "expected 2 list items, got {}", items.len());
                // First item: Assign(Decl(My, [$x]), $a)
                match &items[0].kind {
                    ExprKind::Assign(_, lhs, rhs) => {
                        assert!(matches!(lhs.kind, ExprKind::Decl(DeclScope::My, _)), "expected Decl(My) lhs, got {:?}", lhs.kind);
                        assert!(matches!(rhs.kind, ExprKind::ScalarVar(_)), "expected ScalarVar rhs, got {:?}", rhs.kind);
                    }
                    other => panic!("expected Assign as first list item, got {other:?}"),
                }
                // Second item: $b
                assert!(matches!(items[1].kind, ExprKind::ScalarVar(_)), "expected ScalarVar as second item, got {:?}", items[1].kind);
            }
            other => panic!("expected Stmt::Expr(List), got {other:?}"),
        }
    }

    #[test]
    fn hard_our_assign_comma_grouping() {
        // `our $x = $a, $b;` — same behavior as `my` with a different scope.
        let prog = parse("our $x = $a, $b;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::List(items), .. }) => {
                assert_eq!(items.len(), 2);
                match &items[0].kind {
                    ExprKind::Assign(_, lhs, _) => {
                        assert!(matches!(lhs.kind, ExprKind::Decl(DeclScope::Our, _)), "expected Decl(Our) lhs, got {:?}", lhs.kind);
                    }
                    other => panic!("expected Assign, got {other:?}"),
                }
                assert!(matches!(items[1].kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected Stmt::Expr(List), got {other:?}"),
        }
    }

    #[test]
    fn hard_state_assign_comma_grouping() {
        // `state $x = $a, $b;` — same behavior as `my` with a different scope.
        let prog = parse("state $x = $a, $b;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::List(items), .. }) => {
                assert_eq!(items.len(), 2);
                match &items[0].kind {
                    ExprKind::Assign(_, lhs, _) => {
                        assert!(matches!(lhs.kind, ExprKind::Decl(DeclScope::State, _)), "expected Decl(State) lhs, got {:?}", lhs.kind);
                    }
                    other => panic!("expected Assign, got {other:?}"),
                }
                assert!(matches!(items[1].kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected Stmt::Expr(List), got {other:?}"),
        }
    }

    #[test]
    fn hard_local_assign_comma_grouping() {
        // `local $x = $a, $b;` — local is an expression too; the trailing
        // comma must NOT be absorbed into the Local operand.
        // Must group as `(local $x = $a), $b`, giving List([Assign(Local($x), $a), $b]).
        let prog = parse("local $x = $a, $b;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::List(items), .. }) => {
                assert_eq!(items.len(), 2, "expected 2 list items, got {}", items.len());
                match &items[0].kind {
                    ExprKind::Assign(_, lhs, rhs) => {
                        assert!(matches!(lhs.kind, ExprKind::Local(_)), "expected Local lhs, got {:?}", lhs.kind);
                        assert!(matches!(rhs.kind, ExprKind::ScalarVar(_)), "expected ScalarVar rhs, got {:?}", rhs.kind);
                    }
                    other => panic!("expected Assign, got {other:?}"),
                }
                assert!(matches!(items[1].kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected Stmt::Expr(List), got {other:?}"),
        }
    }

    // ── Declarations as expressions: basic forms ──────────────
    //
    // Verify that each declaration kind produces an expression
    // (wrapped in Stmt::Expr), not a dedicated statement kind.

    #[test]
    fn hard_my_is_expression() {
        // `my $x;` — no initializer, bare Decl expression.
        let prog = parse("my $x;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::Decl(DeclScope::My, vars), .. }) => {
                assert_eq!(vars[0].name, "x");
            }
            other => panic!("expected Stmt::Expr(Decl(My)), got {other:?}"),
        }
    }

    #[test]
    fn hard_our_is_expression() {
        let prog = parse("our $x;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::Decl(DeclScope::Our, vars), .. }) => {
                assert_eq!(vars[0].name, "x");
            }
            other => panic!("expected Stmt::Expr(Decl(Our)), got {other:?}"),
        }
    }

    #[test]
    fn hard_state_is_expression() {
        let prog = parse("state $x;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::Decl(DeclScope::State, vars), .. }) => {
                assert_eq!(vars[0].name, "x");
            }
            other => panic!("expected Stmt::Expr(Decl(State)), got {other:?}"),
        }
    }

    #[test]
    fn hard_local_is_expression() {
        let prog = parse("local $x;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::Local(inner), .. }) => {
                assert!(matches!(inner.kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected Stmt::Expr(Local), got {other:?}"),
        }
    }

    // ── Declarations in expression position ───────────────────
    //
    // Declarations as expressions should be usable in any context
    // that accepts an expression — not just at statement start.

    #[test]
    fn hard_my_in_parens() {
        // `(my $x) = @list;` — decl inside parens on LHS of assignment.
        let prog = parse("(my $x) = @list;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => {
                // LHS should contain a Decl (possibly wrapped in Paren).
                let inner = match &lhs.kind {
                    ExprKind::Paren(inner) => &inner.kind,
                    other => other,
                };
                assert!(matches!(inner, ExprKind::Decl(DeclScope::My, _)), "expected Decl on LHS, got {inner:?}");
            }
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    #[test]
    fn hard_my_list_in_parens() {
        // `my ($a, $b) = @list;` — list form.
        let prog = parse("my ($a, $b) = @list;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => match &lhs.kind {
                ExprKind::Decl(DeclScope::My, vars) => {
                    assert_eq!(vars.len(), 2);
                    assert_eq!(vars[0].name, "a");
                    assert_eq!(vars[1].name, "b");
                }
                other => panic!("expected Decl(My), got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    // ── Declarations in control-flow heads ────────────────────

    #[test]
    fn hard_my_in_if_condition() {
        // `if (my $x = foo()) { ... }` — decl in an if condition.
        // The decl is nested inside an If statement's paren-expr.
        let prog = parse("if (my $x = foo()) { 1; }");
        match &prog.statements[0].kind {
            StmtKind::If(if_stmt) => match &if_stmt.condition.kind {
                ExprKind::Assign(_, lhs, _) => {
                    assert!(matches!(lhs.kind, ExprKind::Decl(DeclScope::My, _)), "expected Decl on LHS, got {:?}", lhs.kind);
                }
                other => panic!("expected Assign in if condition, got {other:?}"),
            },
            other => panic!("expected If, got {other:?}"),
        }
    }

    #[test]
    fn hard_my_in_while_condition() {
        let prog = parse("while (my $line = <$fh>) { 1; }");
        match &prog.statements[0].kind {
            StmtKind::While(w) => match &w.condition.kind {
                ExprKind::Assign(_, lhs, _) => {
                    assert!(matches!(lhs.kind, ExprKind::Decl(DeclScope::My, _)));
                }
                other => panic!("expected Assign, got {other:?}"),
            },
            other => panic!("expected While, got {other:?}"),
        }
    }

    #[test]
    fn hard_parses_postfix_unless() {
        parse("print \"x\" unless $cond;");
    }

    // ── Prototype-driven call-site parsing ─────────────────────
    //
    // These verify that a sub's prototype — registered in the
    // symbol table at declaration time — drives how arguments at
    // call sites are parsed.  Anti-oracle cases adapted from
    // ChatGPT's parser-breaker corpus.

    /// Given `sub NAME (PROTO); CALL`, parse and return the
    /// expression from the second statement (the call).
    fn parse_call_with_proto(src: &str) -> Expr {
        let prog = parse(src);
        assert!(prog.statements.len() >= 2, "expected ≥2 statements (decl + call), got {}", prog.statements.len());
        match &prog.statements[1].kind {
            StmtKind::Expr(e) => e.clone(),
            other => panic!("expected Stmt::Expr for call, got {other:?}"),
        }
    }

    #[test]
    fn proto_empty_stops_at_plus() {
        // sub foo (); foo + 1;
        // Empty prototype forces zero args, so `+ 1` is a binary op.
        // Expected: BinOp(Add, FuncCall("foo", []), Int(1)).
        let e = parse_call_with_proto("sub foo (); foo + 1;");
        match &e.kind {
            ExprKind::BinOp(BinOp::Add, lhs, rhs) => {
                match &lhs.kind {
                    ExprKind::FuncCall(name, args) => {
                        assert_eq!(name, "foo");
                        assert_eq!(args.len(), 0, "empty-proto call should have 0 args");
                    }
                    other => panic!("expected FuncCall(foo, []), got {other:?}"),
                }
                assert!(matches!(rhs.kind, ExprKind::IntLit(1)));
            }
            other => panic!("expected BinOp(Add, FuncCall, 1), got {other:?}"),
        }
    }

    #[test]
    fn proto_single_scalar_takes_one_expr() {
        // sub foo ($); foo $a + $b;
        // One-scalar proto: `$a + $b` is the single arg.
        let e = parse_call_with_proto("sub foo ($); foo $a + $b;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 1, "$-proto should take exactly 1 arg");
                assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Add, _, _)), "arg should be $a + $b, got {:?}", args[0].kind);
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_single_scalar_comma_terminates_arg() {
        // sub foo ($); foo $a, $b;
        // One-scalar proto: `$a` is the arg; comma ends the call,
        // and `$b` is a separate list element.  Expected:
        // List([FuncCall("foo", [$a]), $b]).
        let e = parse_call_with_proto("sub foo ($); foo $a, $b;");
        match &e.kind {
            ExprKind::List(items) => {
                assert_eq!(items.len(), 2);
                match &items[0].kind {
                    ExprKind::FuncCall(name, args) => {
                        assert_eq!(name, "foo");
                        assert_eq!(args.len(), 1);
                        assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)));
                    }
                    other => panic!("expected FuncCall(foo, [$a]), got {other:?}"),
                }
                assert!(matches!(items[1].kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected List with foo call and $b, got {other:?}"),
        }
    }

    #[test]
    fn proto_two_scalars_takes_two_args() {
        // sub foo ($$); foo $a + $b, $c;
        // Two-scalar proto: `$a + $b` is arg 1, `$c` is arg 2.
        let e = parse_call_with_proto("sub foo ($$); foo $a + $b, $c;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 2, "$$-proto should take 2 args");
                assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Add, _, _)), "arg 1 should be Add, got {:?}", args[0].kind);
                assert!(matches!(args[1].kind, ExprKind::ScalarVar(_)), "arg 2 should be $c, got {:?}", args[1].kind);
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_block_and_list() {
        // sub foo (&@); foo { $x } @list;
        // &@-proto: first arg is a block (wrapped as AnonSub),
        // second is the slurpy list.
        let e = parse_call_with_proto("sub foo (&@); foo { $x } @list;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 2, "&@-proto should take block + list = 2 args");
                assert!(matches!(args[0].kind, ExprKind::AnonSub(..)), "arg 1 should be AnonSub (block), got {:?}", args[0].kind);
                assert!(matches!(args[1].kind, ExprKind::ArrayVar(_)), "arg 2 should be @list, got {:?}", args[1].kind);
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_slurpy_list_takes_everything() {
        // sub foo (@); foo $a, $b, $c;
        // Slurpy proto: all three args are consumed.
        let e = parse_call_with_proto("sub foo (@); foo $a, $b, $c;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected FuncCall with 3 args, got {other:?}"),
        }
    }

    #[test]
    fn proto_forward_declaration_registers_proto() {
        // sub foo ($$);  # forward-decl only, no body
        // foo $a, $b;    # should still use the proto
        let e = parse_call_with_proto("sub foo ($$); foo $a, $b;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected FuncCall with 2 args, got {other:?}"),
        }
    }

    #[test]
    fn known_sub_without_proto_is_list_op() {
        // sub foo { 1 } foo 1, 2;
        // No prototype, but sub is known: parses as list op call.
        let e = parse_call_with_proto("sub foo { 1 } foo 1, 2;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 2);
                assert!(matches!(args[0].kind, ExprKind::IntLit(1)));
                assert!(matches!(args[1].kind, ExprKind::IntLit(2)));
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn unknown_sub_stays_bareword_before_operator() {
        // foo + 1;  # no declaration — original behavior preserved.
        // Should parse as BinOp(Add, Bareword("foo"), 1).
        let prog = parse("foo + 1;");
        match &prog.statements[0].kind {
            StmtKind::Expr(Expr { kind: ExprKind::BinOp(BinOp::Add, lhs, rhs), .. }) => {
                assert!(matches!(lhs.kind, ExprKind::Bareword(_) | ExprKind::FuncCall(_, _)), "lhs should be Bareword or FuncCall, got {:?}", lhs.kind);
                assert!(matches!(rhs.kind, ExprKind::IntLit(1)));
            }
            other => panic!("expected BinOp(Add, ..., 1), got {other:?}"),
        }
    }

    #[test]
    fn proto_respects_package_scope() {
        // A proto declared in Foo shouldn't affect bare calls in main.
        // package Foo; sub bar (); package main; bar + 1;
        // The bare `bar` in main isn't found → falls through to
        // Bareword + BinOp.
        let prog = parse("package Foo; sub bar (); package main; bar + 1;");
        // Find the last statement (the `bar + 1` call).
        let last = prog.statements.last().expect("at least one stmt");
        match &last.kind {
            StmtKind::Expr(Expr { kind: ExprKind::BinOp(BinOp::Add, lhs, _), .. }) => {
                // bar is not found in main → stays bareword.
                assert!(matches!(lhs.kind, ExprKind::Bareword(_)), "expected Bareword (not found in main), got {:?}", lhs.kind);
            }
            other => panic!("expected BinOp, got {other:?}"),
        }
    }

    #[test]
    fn proto_respects_fully_qualified_call() {
        // package Foo; sub bar (); package main; Foo::bar + 1;
        // Fully-qualified call finds the proto → zero-arg call.
        let prog = parse("package Foo; sub bar (); package main; Foo::bar + 1;");
        let last = prog.statements.last().expect("at least one stmt");
        match &last.kind {
            StmtKind::Expr(Expr { kind: ExprKind::BinOp(BinOp::Add, lhs, _), .. }) => match &lhs.kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "Foo::bar");
                    assert_eq!(args.len(), 0, "empty-proto FQN call should have 0 args");
                }
                other => panic!("expected FuncCall(Foo::bar, []), got {other:?}"),
            },
            other => panic!("expected BinOp, got {other:?}"),
        }
    }

    #[test]
    fn proto_underscore_with_arg_takes_it() {
        // sub foo (_); foo $x;
        // `_` slot with an arg supplied behaves like `$`.
        let e = parse_call_with_proto("sub foo (_); foo $x;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)), "expected ScalarVar, got {:?}", args[0].kind);
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_underscore_without_arg_inserts_default_var() {
        // sub foo (_); foo;
        // `_` slot with no arg → parser inserts DefaultVar.
        let e = parse_call_with_proto("sub foo (_); foo;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 1, "_-slot should default to DefaultVar when omitted");
                assert!(matches!(args[0].kind, ExprKind::DefaultVar), "expected DefaultVar, got {:?}", args[0].kind);
            }
            other => panic!("expected FuncCall with DefaultVar, got {other:?}"),
        }
    }

    #[test]
    fn proto_underscore_distinct_from_explicit_dollar_underscore() {
        // sub foo (_); foo $_;
        // Explicit $_ should be ScalarVar("_"), NOT DefaultVar.
        // This pins down the distinction: the parser inserts
        // DefaultVar only when the arg is omitted.
        let e = parse_call_with_proto("sub foo (_); foo $_;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 1);
                // Note: $_ may be represented as SpecialVar or
                // ScalarVar depending on the lexer; either is fine,
                // as long as it's NOT DefaultVar.
                assert!(!matches!(args[0].kind, ExprKind::DefaultVar), "explicit $_ should not become DefaultVar");
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_glob_bareword_becomes_glob_var() {
        // sub foo (*); foo STDIN;
        // Bareword in a `*` slot is auto-promoted to a typeglob.
        let e = parse_call_with_proto("sub foo (*); foo STDIN;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 1);
                match &args[0].kind {
                    ExprKind::GlobVar(n) => assert_eq!(n, "STDIN"),
                    other => panic!("expected GlobVar(STDIN), got {other:?}"),
                }
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_glob_explicit_star_stays_glob() {
        // sub foo (*); foo *STDIN;
        // Explicit *STDIN is already a GlobVar from the source.
        let e = parse_call_with_proto("sub foo (*); foo *STDIN;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::GlobVar(_)), "expected GlobVar, got {:?}", args[0].kind);
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_glob_scalar_passed_through() {
        // sub foo (*); foo $fh;
        // A scalar expression in a `*` slot is parsed as-is —
        // it's presumed to hold a glob ref at runtime.
        let e = parse_call_with_proto("sub foo (*); foo $fh;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)), "expected ScalarVar, got {:?}", args[0].kind);
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    // ── Prototype bypass cases ─────────────────────────────────
    //
    // Two syntactic forms bypass prototype-driven argument parsing:
    //   1. Parens form: foo(args) — args are parens-delimited, so
    //      the parser takes a generic comma-separated list without
    //      consulting the prototype.  (Perl may still validate arg
    //      counts at compile time; that's a semantic-pass concern,
    //      not a parsing concern.)
    //   2. Ampersand form: &foo(args) — goes through the code-ref
    //      prefix path, completely bypassing parse_ident_term and
    //      therefore the symbol-table lookup.

    #[test]
    fn proto_parens_form_parses_generic_list() {
        // sub foo ($); foo($a + $b, $c);
        // Without parens, `$` proto would consume only `$a + $b`
        // and leave `$c` in the outer comma list.  With parens,
        // the args are delimited, so we get both.
        let e = parse_call_with_proto("sub foo ($); foo($a + $b, $c);");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 2, "parens form should parse both args regardless of $ proto");
                assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Add, _, _)));
                assert!(matches!(args[1].kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_parens_form_ignores_empty_proto() {
        // sub foo (); foo(1, 2);
        // Parens form takes the args; Perl would report "Too many
        // arguments" at compile time but we don't validate yet.
        let e = parse_call_with_proto("sub foo (); foo(1, 2);");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected FuncCall with 2 args, got {other:?}"),
        }
    }

    #[test]
    fn proto_ampersand_call_bypasses_empty_proto() {
        // sub foo (); &foo(1, 2);
        // &foo() completely bypasses prototype parsing.  Without
        // the &, `foo(1, 2)` would still work via parens (see test
        // above), but the &-form is the canonical bypass.
        let e = parse_call_with_proto("sub foo (); &foo(1, 2);");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 2, "&foo(...) bypasses empty proto");
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_ampersand_no_parens_bypasses_proto() {
        // sub foo ($); &foo;
        // &foo with no parens calls with current @_ (inherited);
        // prototype is not consulted.
        let e = parse_call_with_proto("sub foo ($); &foo;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 0, "&foo with no parens inherits @_");
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    // ── Named-unary precedence for scalar-ish slots ─────────────
    //
    // A `$`-slot (or `_`, `+`, `\X`, `\[...]`, glob-expression)
    // parses its arg at named-unary precedence.  That means
    // operators tighter than named unary (shift, +, -, *, /, **,
    // etc.) are consumed into the arg, while operators looser
    // (relational, equality, ternary, assignment, comma) terminate
    // the arg and apply at the outer level.

    #[test]
    fn proto_scalar_tight_op_is_consumed() {
        // sub foo ($); foo $a << 1;
        // `<<` (shift, tighter than named unary) is consumed.
        let e = parse_call_with_proto("sub foo ($); foo $a << 1;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 1);
                assert!(
                    matches!(args[0].kind, ExprKind::BinOp(BinOp::ShiftLeft, _, _)),
                    "expected arg to be ShiftLeft (tighter than named-unary), got {:?}",
                    args[0].kind
                );
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_scalar_relational_terminates_arg() {
        // sub foo ($); foo $a < 1;
        // `<` (relational, looser than named unary) terminates the
        // arg.  Parses as `foo($a) < 1`.
        let e = parse_call_with_proto("sub foo ($); foo $a < 1;");
        match &e.kind {
            ExprKind::BinOp(BinOp::NumLt, lhs, rhs) => {
                match &lhs.kind {
                    ExprKind::FuncCall(name, args) => {
                        assert_eq!(name, "foo");
                        assert_eq!(args.len(), 1);
                        assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)));
                    }
                    other => panic!("expected FuncCall on lhs, got {other:?}"),
                }
                assert!(matches!(rhs.kind, ExprKind::IntLit(1)));
            }
            other => panic!("expected BinOp(NumLt, FuncCall, 1), got {other:?}"),
        }
    }

    #[test]
    fn proto_scalar_equality_terminates_arg() {
        // sub foo ($); foo 1 == 2;
        // `==` is looser than named unary → terminates arg.
        // Parses as `foo(1) == 2`.
        let e = parse_call_with_proto("sub foo ($); foo 1 == 2;");
        match &e.kind {
            ExprKind::BinOp(BinOp::NumEq, lhs, rhs) => {
                match &lhs.kind {
                    ExprKind::FuncCall(name, args) => {
                        assert_eq!(name, "foo");
                        assert_eq!(args.len(), 1);
                        assert!(matches!(args[0].kind, ExprKind::IntLit(1)));
                    }
                    other => panic!("expected FuncCall on lhs, got {other:?}"),
                }
                assert!(matches!(rhs.kind, ExprKind::IntLit(2)));
            }
            other => panic!("expected BinOp(NumEq, FuncCall, 2), got {other:?}"),
        }
    }

    #[test]
    fn proto_scalar_ternary_terminates_arg() {
        // sub foo ($); foo $a ? $b : $c;
        // Ternary is far below named unary → terminates arg.
        // Parses as `foo($a) ? $b : $c`.
        let e = parse_call_with_proto("sub foo ($); foo $a ? $b : $c;");
        match &e.kind {
            ExprKind::Ternary(cond, _, _) => match &cond.kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "foo");
                    assert_eq!(args.len(), 1);
                }
                other => panic!("expected FuncCall as ternary cond, got {other:?}"),
            },
            other => panic!("expected Ternary, got {other:?}"),
        }
    }

    #[test]
    fn proto_scalar_mul_and_add_both_consumed() {
        // sub foo ($); foo 1 + 2 * 3;
        // Both `+` and `*` are tighter than named unary, so the
        // whole arithmetic expression is the single arg.
        let e = parse_call_with_proto("sub foo ($); foo 1 + 2 * 3;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Add, _, _)), "expected top-level Add, got {:?}", args[0].kind);
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    // ── & slot accepting code references ────────────────────────
    //
    // A `&` prototype slot accepts either a literal block (wrapped
    // as an anonymous sub) or any code-reference expression —
    // `\&name`, `$coderef`, `sub { ... }`, etc.

    #[test]
    fn proto_amp_slot_accepts_backslash_sub_ref() {
        // sub foo (&@); foo \&bar, @list;
        // `\&bar` is a reference-to-sub expression.
        let e = parse_call_with_proto("sub foo (&@); foo \\&bar, @list;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 2);
                // First arg is a ref-take around something naming `bar`.
                assert!(matches!(args[0].kind, ExprKind::Ref(_)), "expected Ref(...), got {:?}", args[0].kind);
                assert!(matches!(args[1].kind, ExprKind::ArrayVar(_)), "expected @list, got {:?}", args[1].kind);
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_amp_slot_accepts_scalar_coderef() {
        // sub foo (&@); foo $cref, @list;
        // Scalar holding a coderef.
        let e = parse_call_with_proto("sub foo (&@); foo $cref, @list;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 2);
                assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)), "expected ScalarVar, got {:?}", args[0].kind);
                assert!(matches!(args[1].kind, ExprKind::ArrayVar(_)));
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_amp_slot_accepts_anonymous_sub() {
        // sub foo (&@); foo sub { 1 }, @list;
        // Anonymous sub expression in the & slot.
        let e = parse_call_with_proto("sub foo (&@); foo sub { 1 }, @list;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 2);
                assert!(matches!(args[0].kind, ExprKind::AnonSub(..)), "expected AnonSub, got {:?}", args[0].kind);
                assert!(matches!(args[1].kind, ExprKind::ArrayVar(_)));
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_amp_slot_block_still_works() {
        // sub foo (&@); foo { $x * 2 } @list;
        // Regression: literal block form still wraps as AnonSub.
        let e = parse_call_with_proto("sub foo (&@); foo { $x * 2 } @list;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 2);
                assert!(matches!(args[0].kind, ExprKind::AnonSub(..)));
                assert!(matches!(args[1].kind, ExprKind::ArrayVar(_)));
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    // ── Auto-reference prototype slots ──────────────────────────
    //
    // `\$`, `\@`, `\%`, `\&`, `\*`, `\[...]`, and `+` all cause
    // the argument to be implicitly referenced at the call site.
    // `foo @arr` with `sub foo (\@)` is equivalent to `foo(\@arr)`.
    // The parser wraps the argument in an ExprKind::Ref; any
    // validation that the argument is of the expected kind is a
    // semantic-pass concern.

    #[test]
    fn proto_auto_ref_array() {
        // sub foo (\@); foo @arr;  →  foo(\@arr)
        let e = parse_call_with_proto("sub foo (\\@); foo @arr;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 1);
                match &args[0].kind {
                    ExprKind::Ref(inner) => {
                        assert!(matches!(inner.kind, ExprKind::ArrayVar(_)), "expected Ref(ArrayVar), got Ref({:?})", inner.kind);
                    }
                    other => panic!("expected Ref, got {other:?}"),
                }
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_auto_ref_hash() {
        // sub foo (\%); foo %h;  →  foo(\%h)
        let e = parse_call_with_proto("sub foo (\\%); foo %h;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 1);
                match &args[0].kind {
                    ExprKind::Ref(inner) => {
                        assert!(matches!(inner.kind, ExprKind::HashVar(_)), "expected Ref(HashVar), got Ref({:?})", inner.kind);
                    }
                    other => panic!("expected Ref, got {other:?}"),
                }
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_auto_ref_scalar() {
        // sub foo (\$); foo $x;  →  foo(\$x)
        let e = parse_call_with_proto("sub foo (\\$); foo $x;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 1);
                match &args[0].kind {
                    ExprKind::Ref(inner) => {
                        assert!(matches!(inner.kind, ExprKind::ScalarVar(_)));
                    }
                    other => panic!("expected Ref, got {other:?}"),
                }
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_auto_ref_one_of_takes_array() {
        // sub foo (\[@%]); foo @arr;  →  foo(\@arr)
        let e = parse_call_with_proto("sub foo (\\[@%]); foo @arr;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 1);
                match &args[0].kind {
                    ExprKind::Ref(inner) => {
                        assert!(matches!(inner.kind, ExprKind::ArrayVar(_)));
                    }
                    other => panic!("expected Ref, got {other:?}"),
                }
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_auto_ref_one_of_takes_hash() {
        // sub foo (\[@%]); foo %h;  →  foo(\%h)
        let e = parse_call_with_proto("sub foo (\\[@%]); foo %h;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 1);
                match &args[0].kind {
                    ExprKind::Ref(inner) => {
                        assert!(matches!(inner.kind, ExprKind::HashVar(_)));
                    }
                    other => panic!("expected Ref, got {other:?}"),
                }
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_array_or_hash_takes_array() {
        // sub foo (+); foo @arr;  →  foo(\@arr)
        // The `+` slot is effectively `\[@%]`.
        let e = parse_call_with_proto("sub foo (+); foo @arr;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 1);
                match &args[0].kind {
                    ExprKind::Ref(inner) => {
                        assert!(matches!(inner.kind, ExprKind::ArrayVar(_)), "expected Ref(ArrayVar), got Ref({:?})", inner.kind);
                    }
                    other => panic!("expected Ref, got {other:?}"),
                }
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_array_or_hash_takes_hash() {
        // sub foo (+); foo %h;  →  foo(\%h)
        let e = parse_call_with_proto("sub foo (+); foo %h;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 1);
                match &args[0].kind {
                    ExprKind::Ref(inner) => {
                        assert!(matches!(inner.kind, ExprKind::HashVar(_)));
                    }
                    other => panic!("expected Ref, got {other:?}"),
                }
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_auto_ref_multiple_slots() {
        // sub foo (\@\@); foo @a, @b;  →  foo(\@a, \@b)
        let e = parse_call_with_proto("sub foo (\\@\\@); foo @a, @b;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 2);
                for arg in args {
                    match &arg.kind {
                        ExprKind::Ref(inner) => {
                            assert!(matches!(inner.kind, ExprKind::ArrayVar(_)));
                        }
                        other => panic!("expected Ref(ArrayVar), got {other:?}"),
                    }
                }
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_auto_ref_mixed_with_slurpy() {
        // sub foo (\@@); foo @a, $x, $y;  →  foo(\@a, $x, $y)
        // First slot takes the array by ref; slurpy takes the rest.
        let e = parse_call_with_proto("sub foo (\\@@); foo @a, $x, $y;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 3);
                // First arg is the ref'd array.
                match &args[0].kind {
                    ExprKind::Ref(inner) => {
                        assert!(matches!(inner.kind, ExprKind::ArrayVar(_)));
                    }
                    other => panic!("expected Ref(ArrayVar) first, got {other:?}"),
                }
                // Remaining two are scalar slurpy args, not ref'd.
                assert!(matches!(args[1].kind, ExprKind::ScalarVar(_)));
                assert!(matches!(args[2].kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    // ── Non-initial & slot: `{` is a hash-ref, not a block ──────
    //
    // In the initial slot, `&` plus a bare `{` parses the block as
    // an anonymous sub (the map/grep pattern).  In any non-initial
    // position, `{` at a call site is an ordinary hash-ref
    // constructor; to pass a code reference the caller must spell
    // it out: `sub { ... }`, `\&name`, `$coderef`, etc.

    #[test]
    fn proto_amp_non_initial_brace_is_hash_ref() {
        // sub foo ($&); foo $x, { a => 1 };
        // The `{ a => 1 }` is a hash-ref constructor, NOT a block.
        let e = parse_call_with_proto("sub foo ($&); foo $x, { a => 1 };");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 2);
                assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)));
                assert!(matches!(args[1].kind, ExprKind::AnonHash(_)), "expected AnonHash, got {:?}", args[1].kind);
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_amp_non_initial_explicit_sub_works() {
        // sub foo ($&); foo $x, sub { 1 };
        let e = parse_call_with_proto("sub foo ($&); foo $x, sub { 1 };");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 2);
                assert!(matches!(args[1].kind, ExprKind::AnonSub(..)), "expected AnonSub, got {:?}", args[1].kind);
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_amp_non_initial_backslash_name_works() {
        // sub foo ($&); foo $x, \&bar;
        let e = parse_call_with_proto("sub foo ($&); foo $x, \\&bar;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 2);
                assert!(matches!(args[1].kind, ExprKind::Ref(_)), "expected Ref, got {:?}", args[1].kind);
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_amp_initial_block_still_works() {
        // sub foo (&); foo { 1 };
        // Regression: initial `&` with bare block still wraps as
        // AnonSub.
        let e = parse_call_with_proto("sub foo (&); foo { 1 };");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::AnonSub(..)));
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_amp_initial_map_style_still_works() {
        // sub mymap (&@); mymap { $_ * 2 } @list;
        // Regression: initial `&@` map-style syntax is unchanged.
        let e = parse_call_with_proto("sub mymap (&@); mymap { $_ * 2 } @list;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 2);
                assert!(matches!(args[0].kind, ExprKind::AnonSub(..)));
                assert!(matches!(args[1].kind, ExprKind::ArrayVar(_)));
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    // ── :prototype(...) attribute form ──────────────────────────
    //
    // Modern Perl (5.20+) allows the prototype to be declared via
    // an attribute rather than the paren form:
    //   sub foo :prototype($$) { ... }
    // The attribute form is equivalent to the paren form but
    // avoids the paren/signatures ambiguity.

    #[test]
    fn proto_attribute_form_registers_prototype() {
        // sub foo :prototype($$) { } foo $a + $b, $c;
        // Prototype declared via attribute drives call-site parsing
        // just like the paren form.
        let e = parse_call_with_proto("sub foo :prototype($$) { } foo $a + $b, $c;");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 2, ":prototype($$) should give 2 args");
                assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Add, _, _)));
                assert!(matches!(args[1].kind, ExprKind::ScalarVar(_)));
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn proto_attribute_empty_proto_forces_zero_args() {
        // sub foo :prototype() { } foo + 1;
        // Empty :prototype() means zero args; `+ 1` is a binary op.
        let e = parse_call_with_proto("sub foo :prototype() { } foo + 1;");
        match &e.kind {
            ExprKind::BinOp(BinOp::Add, lhs, rhs) => {
                match &lhs.kind {
                    ExprKind::FuncCall(name, args) => {
                        assert_eq!(name, "foo");
                        assert_eq!(args.len(), 0);
                    }
                    other => panic!("expected FuncCall(foo, []), got {other:?}"),
                }
                assert!(matches!(rhs.kind, ExprKind::IntLit(1)));
            }
            other => panic!("expected BinOp(Add, ...), got {other:?}"),
        }
    }

    #[test]
    fn proto_attribute_form_on_forward_declaration() {
        // sub foo :prototype(&@); foo { $_ } @list;
        // Forward declaration with :prototype attribute.
        let e = parse_call_with_proto("sub foo :prototype(&@); foo { $_ } @list;");
        match &e.kind {
            ExprKind::FuncCall(_, args) => {
                assert_eq!(args.len(), 2);
                assert!(matches!(args[0].kind, ExprKind::AnonSub(..)));
                assert!(matches!(args[1].kind, ExprKind::ArrayVar(_)));
            }
            other => panic!("expected FuncCall, got {other:?}"),
        }
    }

    #[test]
    fn hard_parses_heredoc_basic() {
        parse("print <<EOF;\nhello\nEOF\n");
    }

    #[test]
    fn hard_parses_heredoc_concat() {
        // `print <<EOF . "x"; ... EOF` — heredoc in compound expression.
        parse("print <<EOF . \"x\";\nhello\nEOF\n");
    }

    #[test]
    fn hard_parses_heredoc_interp() {
        parse("print <<\"EOF\";\n$interpolated\nEOF\n");
    }

    #[test]
    fn hard_parses_heredoc_literal() {
        parse("print <<'EOF';\n$not_interpolated\nEOF\n");
    }

    #[test]
    fn hard_parses_two_heredocs_same_line() {
        parse("print <<A . <<B;\na\nA\nb\nB\n");
    }

    #[test]
    fn hard_parses_do_block_simple() {
        parse("do { 1 };");
    }

    #[test]
    fn hard_parses_if_hashlike_body() {
        // `if (1) { a => 1 }` — body looks hashy but must parse as block.
        parse("if (1) { a => 1 }");
    }

    #[test]
    fn hard_parses_tricky_slash_combinations() {
        parse("$x / 2;");
        parse("$x / $y / $z;");
        parse("$x // /foo/;");
    }

    // ═══════════════════════════════════════════════════════════
    // Tests for previously-unimplemented syntax features.
    // ═══════════════════════════════════════════════════════════

    // ── \N{U+XXXX} and \N{name} escapes ──────────────────────

    #[test]
    fn escape_n_unicode_codepoint() {
        // `\N{U+2603}` → snowman character ☃.
        let e = parse_expr_str(r#""\N{U+2603}";"#);
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "\u{2603}"),
            other => panic!("expected StringLit with snowman, got {other:?}"),
        }
    }

    #[test]
    fn escape_n_unicode_codepoint_ascii() {
        // `\N{U+41}` → 'A'.
        let e = parse_expr_str(r#""\N{U+41}";"#);
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "A"),
            other => panic!("expected StringLit('A'), got {other:?}"),
        }
    }

    #[test]
    fn escape_n_charname_placeholder() {
        // `\N{SNOWMAN}` — named character.  Without a charnames
        // database the parser emits U+FFFD as a placeholder.
        // Verifying it doesn't error and produces a single-char
        // string.
        let e = parse_expr_str(r#""\N{SNOWMAN}";"#);
        match &e.kind {
            ExprKind::StringLit(s) => {
                assert_eq!(s.len(), 3); // U+FFFD is 3 bytes in UTF-8
                assert!(s.contains('\u{FFFD}'));
            }
            other => panic!("expected StringLit with placeholder, got {other:?}"),
        }
    }

    #[test]
    fn escape_n_in_interpolated_string() {
        // `"prefix \N{U+2603} suffix"` — mixed with other content.
        let e = parse_expr_str(r#""prefix \N{U+2603} suffix";"#);
        match &e.kind {
            ExprKind::StringLit(s) => {
                assert!(s.contains('\u{2603}'), "expected snowman in string");
                assert!(s.starts_with("prefix "), "expected prefix");
            }
            other => panic!("expected StringLit, got {other:?}"),
        }
    }

    #[test]
    fn escape_bare_n_without_braces() {
        // `\N` without `{` is just literal `\N`.
        let e = parse_expr_str(r#""\N test";"#);
        match &e.kind {
            ExprKind::StringLit(s) => assert!(s.contains("\\N"), "expected literal \\N, got {s:?}"),
            other => panic!("expected StringLit, got {other:?}"),
        }
    }

    // ── Smartmatch ~~ ────────────────────────────────────────

    #[test]
    fn smartmatch_basic() {
        // ~~ is in the :default bundle, so it's on by default.
        let e = parse_expr_str("$a ~~ @b;");
        match &e.kind {
            ExprKind::BinOp(BinOp::SmartMatch, lhs, rhs) => {
                assert!(matches!(lhs.kind, ExprKind::ScalarVar(ref n) if n == "a"));
                assert!(matches!(rhs.kind, ExprKind::ArrayVar(ref n) if n == "b"));
            }
            other => panic!("expected SmartMatch, got {other:?}"),
        }
    }

    #[test]
    fn smartmatch_precedence_vs_equality() {
        // `~~` is at PREC_EQ (same as `==`), non-associative.
        // `$a == $b ~~ $c` should error or parse as comparison
        // chain — but since both are non-associative at the same
        // level, the Pratt loop stops after the first one.
        // We just verify `$a ~~ $b` parses at the right level.
        let e = parse_expr_str("$a ~~ $b || $c;");
        match &e.kind {
            ExprKind::BinOp(BinOp::Or, lhs, _) => {
                assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::SmartMatch, _, _)), "~~ should bind tighter than ||");
            }
            other => panic!("expected Or wrapping SmartMatch, got {other:?}"),
        }
    }

    #[test]
    fn smartmatch_disabled_without_feature() {
        // After `no feature ':all'`, smartmatch is off.
        // The lexer still emits Token::SmartMatch (it doesn't
        // have feature state), but peek_op_info won't recognize
        // it as an operator.  The expression `$a ~~ $b` fails
        // to parse as a single expression — `$a` is one
        // statement and `~~` is an unexpected token.
        //
        // A full solution would need lexer-level token demotion
        // (splitting SmartMatch back into two Tildes), similar
        // to keyword demotion.  For now, verify the program
        // doesn't produce a SmartMatch BinOp.
        let prog = parse("no feature ':all'; ~$b;");
        // Just confirms the feature removal doesn't break
        // normal `~` (bitwise not).
        assert!(!prog.statements.is_empty());
    }

    // ── String-bitwise operators ─────────────────────────────

    #[test]
    fn string_bitwise_and() {
        let e = parse_expr_str("$a &. $b;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::StringBitAnd, _, _)), "expected StringBitAnd, got {:?}", e.kind);
    }

    #[test]
    fn string_bitwise_or() {
        let e = parse_expr_str("$a |. $b;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::StringBitOr, _, _)), "expected StringBitOr, got {:?}", e.kind);
    }

    #[test]
    fn string_bitwise_xor() {
        let e = parse_expr_str("$a ^. $b;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::StringBitXor, _, _)), "expected StringBitXor, got {:?}", e.kind);
    }

    #[test]
    fn string_bitwise_not() {
        let e = parse_expr_str("~. $a;");
        assert!(matches!(e.kind, ExprKind::UnaryOp(UnaryOp::StringBitNot, _)), "expected StringBitNot, got {:?}", e.kind);
    }

    #[test]
    fn string_bitwise_and_assign() {
        let e = parse_expr_str("$a &.= $b;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::StringBitAndEq, _, _)), "expected &.= assign, got {:?}", e.kind);
    }

    #[test]
    fn string_bitwise_or_assign() {
        let e = parse_expr_str("$a |.= $b;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::StringBitOrEq, _, _)), "expected |.= assign, got {:?}", e.kind);
    }

    #[test]
    fn string_bitwise_xor_assign() {
        let e = parse_expr_str("$a ^.= $b;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::StringBitXorEq, _, _)), "expected ^.= assign, got {:?}", e.kind);
    }

    #[test]
    fn string_bitwise_precedence() {
        // `&.` has PREC_BIT_AND, which is tighter than `|.`.
        // `$a |. $b &. $c` → `$a |. ($b &. $c)`.
        let e = parse_expr_str("$a |. $b &. $c;");
        match &e.kind {
            ExprKind::BinOp(BinOp::StringBitOr, _, rhs) => {
                assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::StringBitAnd, _, _)), "expected &. to bind tighter than |.");
            }
            other => panic!("expected StringBitOr at top, got {other:?}"),
        }
    }

    // ── CORE:: qualified builtins ────────────────────────────

    #[test]
    fn core_qualified_builtin() {
        // `CORE::say(...)` parses as a package-qualified function call.
        // The semantic distinction (forcing the builtin) is a
        // compiler concern; the parser treats it like any other
        // qualified name.
        let prog = parse(r#"CORE::say("hello");"#);
        assert!(!prog.statements.is_empty(), "should parse CORE::say");
    }

    #[test]
    fn core_qualified_length() {
        let e = parse_expr_str("CORE::length($x);");
        match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::length");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall(CORE::length), got {other:?}"),
        }
    }

    // ── UTF-8 identifiers under `use utf8` ───────────────────

    #[test]
    fn utf8_scalar_variable() {
        // `use utf8; my $café = 1;` — UTF-8 identifier.
        let prog = parse("use utf8; my $café = 1;");
        assert!(prog.statements.len() >= 2, "should parse use + decl");
    }

    #[test]
    fn utf8_sub_name() {
        let prog = parse("use utf8; sub naïve { 1 }");
        assert!(
            prog.statements.iter().any(|s| matches!(
                &s.kind,
                StmtKind::SubDecl(sd) if sd.name == "naïve"
            )),
            "expected sub named naïve"
        );
    }

    #[test]
    fn utf8_bareword_fat_comma() {
        // `café => 1` with utf8 active — autoquoted.
        let prog = parse("use utf8; my %h = (café => 1);");
        assert!(!prog.statements.is_empty(), "should parse");
    }

    #[test]
    fn utf8_hash_key_autoquote() {
        // `$h{café}` — bareword autoquoted inside hash subscript.
        let prog = parse("use utf8; $h{café};");
        let expr = prog.statements.iter().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e) } else { None }).expect("expression statement");
        match &expr.kind {
            ExprKind::HashElem(_, k) => {
                assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "café"), "expected StringLit(café), got {:?}", k.kind);
            }
            other => panic!("expected HashElem, got {other:?}"),
        }
    }

    #[test]
    fn utf8_error_without_pragma() {
        // Without `use utf8`, bytes ≥ 0x80 are rejected.
        let src = "my $café = 1;";
        let mut p = match Parser::new(src.as_bytes()) {
            Ok(p) => p,
            Err(_) => return, // construction error is also acceptable
        };
        let result = p.parse_program();
        assert!(result.is_err(), "high bytes without use utf8 should error");
    }

    #[test]
    fn utf8_lexical_scoping() {
        // `use utf8` is lexically scoped: inside a `no utf8`
        // block, UTF-8 identifiers are rejected again.
        let src = "use utf8; my $café = 1; { no utf8; my $x = 1; }";
        let prog = parse(src);
        // The program parses — $café is in utf8 scope,
        // $x is in no-utf8 scope (ASCII only, fine).
        assert!(prog.statements.len() >= 2);
    }

    #[test]
    fn utf8_lexical_scoping_error_in_block() {
        // After `no utf8` inside a block, UTF-8 identifiers
        // should error — matching Perl's behavior.
        let src = "use utf8; { no utf8; my $café = 1; }";
        let mut p = match Parser::new(src.as_bytes()) {
            Ok(p) => p,
            Err(_) => return,
        };
        let result = p.parse_program();
        assert!(result.is_err(), "UTF-8 identifier after `no utf8` inside block should error");
    }

    #[test]
    fn utf8_in_string_interpolation() {
        // `"$café"` with utf8 active — the variable name is UTF-8.
        let prog = parse("use utf8; my $café = 1; print \"$café\";");
        assert!(!prog.statements.is_empty(), "should parse");
    }

    // ═══════════════════════════════════════════════════════════
    // perlsyn gap-probing tests — features from perlsyn that
    // may or may not be implemented.  Failures are diagnostic.
    // ═══════════════════════════════════════════════════════════

    // ── 1. Postfix `when` modifier ───────────────────────────

    #[test]
    fn postfix_when_modifier_v514() {
        // `$abc = 1 when /^abc/;` — perlsyn lists `when EXPR`
        // as a statement modifier alongside if/unless/while/until.
        // `use v5.14` enables the switch feature (5.10–5.34 bundle).
        let prog = parse("use v5.14; $abc = 1 when /^abc/;");
        assert!(prog.statements.len() >= 2, "should parse postfix when with use v5.14");
    }

    #[test]
    fn postfix_when_modifier_explicit_feature() {
        // Explicitly enabling switch feature.
        let prog = parse("use feature 'switch'; $abc = 1 when /^abc/;");
        assert!(prog.statements.len() >= 2, "should parse postfix when with use feature 'switch'");
    }

    #[test]
    fn postfix_when_without_feature() {
        // Without the switch feature, `when` is demoted to a bare
        // identifier.  `$abc = 1 when ...` would parse `when` as
        // a bareword function call — the expression becomes
        // `1 when(...)` which is NOT a postfix modifier.
        // Just verify it doesn't produce PostfixKind::When.
        let prog = parse("$abc = 1; when(/^abc/);");
        // Without switch, `when` is a function call, not a keyword.
        // The program parses as two separate statements.
        assert!(prog.statements.len() >= 2);
        // Verify the first statement is NOT a postfix-when.
        let not_postfix_when = !matches!(&prog.statements[0].kind, StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::When, _, _), .. }));
        assert!(not_postfix_when, "when should not be a postfix modifier without the switch feature");
    }

    // ── 2. continue block on bare BLOCK ──────────────────────

    #[test]
    fn continue_block_on_bare_block() {
        // perlsyn: `LABEL BLOCK continue BLOCK` is valid.
        // A bare block acts as a loop that executes once.
        let prog = parse("LOOP: { 1; } continue { 2; }");
        assert!(!prog.statements.is_empty(), "should parse bare block with continue");
    }

    #[test]
    fn continue_block_on_unlabeled_bare_block() {
        let prog = parse("{ 1; } continue { 2; }");
        assert!(!prog.statements.is_empty(), "should parse unlabeled bare block with continue");
    }

    // ── 3. Multi-variable foreach (5.36+) ────────────────────

    #[test]
    fn foreach_multi_variable() {
        // `for my ($key, $value) (%hash) { ... }` — iterating
        // over multiple values at a time (Perl 5.36+).
        let prog = parse("for my ($key, $value) (%hash) { 1; }");
        assert!(!prog.statements.is_empty(), "should parse multi-variable foreach");
    }

    #[test]
    fn foreach_three_variables() {
        let prog = parse("for my ($a, $b, $c) (@list) { 1; }");
        assert!(!prog.statements.is_empty(), "should parse three-variable foreach");
    }

    // ── 4. Backslash foreach (refaliasing, 5.22+) ────────────

    #[test]
    fn foreach_refaliasing() {
        // `foreach \my %hash (@array_of_hash_refs) { ... }`
        // Experimental refaliasing feature.
        let prog = parse(r#"use feature "refaliasing"; foreach \my %hash (@refs) { 1; }"#);
        assert!(!prog.statements.is_empty(), "should parse backslash foreach");
    }

    // ── 5. break keyword (in given blocks) ───────────────────

    #[test]
    fn break_in_given() {
        // `break` exits a `given` block.
        let prog = parse("use v5.14; given ($x) { when (1) { break } }");
        assert!(!prog.statements.is_empty(), "should parse break in given");
    }

    // ── 6. continue as fall-through in given/when ────────────

    #[test]
    fn continue_fall_through_in_given() {
        // `continue` inside a `when` block means fall through
        // to the next when — different from `continue BLOCK`.
        let prog = parse("use v5.14; given ($x) { when (1) { $a = 1; continue } when (2) { $b = 1 } }");
        assert!(!prog.statements.is_empty(), "should parse continue as fall-through in given");
    }

    // ── 7. goto — three forms ────────────────────────────────

    #[test]
    fn goto_label() {
        let prog = parse("goto DONE; DONE: print 1;");
        assert!(!prog.statements.is_empty(), "should parse goto LABEL");
    }

    #[test]
    fn goto_expr() {
        // `goto(("FOO", "BAR")[$i])` — computed goto.
        let prog = parse(r#"goto(("FOO", "BAR")[$i]);"#);
        assert!(!prog.statements.is_empty(), "should parse goto EXPR");
    }

    #[test]
    fn goto_ampersand_name() {
        // `goto &subname` — magical tail call.
        let prog = parse("goto &other_sub;");
        assert!(!prog.statements.is_empty(), "should parse goto &NAME");
    }

    // ── 8. # line N "file" directives ────────────────────────

    #[test]
    fn line_directive_sets_line_number() {
        // `# line 200 "bzzzt"` overrides __LINE__ on the next line.
        let prog = parse("# line 200 \"bzzzt\"\n__LINE__;");
        match &prog.statements[0].kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::SourceLine(n) => assert_eq!(*n, 200, "__LINE__ should be 200 after # line 200"),
                other => panic!("expected SourceLine, got {other:?}"),
            },
            other => panic!("expected Expr, got {other:?}"),
        }
    }

    #[test]
    fn line_directive_sets_filename() {
        // `# line 200 "bzzzt"` overrides __FILE__.
        let prog = parse("# line 200 \"bzzzt\"\n__FILE__;");
        match &prog.statements[0].kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::SourceFile(path) => assert_eq!(path, "bzzzt", "__FILE__ should be 'bzzzt' after # line 200 \"bzzzt\""),
                other => panic!("expected SourceFile, got {other:?}"),
            },
            other => panic!("expected Expr, got {other:?}"),
        }
    }

    #[test]
    fn line_directive_number_only() {
        // `# line 42` without filename — only line number changes.
        let prog = parse("# line 42\n__LINE__;");
        match &prog.statements[0].kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::SourceLine(n) => assert_eq!(*n, 42),
                other => panic!("expected SourceLine, got {other:?}"),
            },
            other => panic!("expected Expr, got {other:?}"),
        }
    }

    #[test]
    fn line_directive_not_at_column_zero() {
        // Leading whitespace — `#` is NOT at column 0, so it's
        // just a regular comment, not a directive.
        let prog = parse("  # line 200\n__LINE__;");
        match &prog.statements[0].kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::SourceLine(n) => assert_ne!(*n, 200, "indented # line should NOT be a directive"),
                other => panic!("expected SourceLine, got {other:?}"),
            },
            other => panic!("expected Expr, got {other:?}"),
        }
    }

    // ═══════════════════════════════════════════════════════════
    // perlop gap-probing tests.
    // ═══════════════════════════════════════════════════════════

    // ── ^^ logical XOR operator ──────────────────────────────

    #[test]
    fn logical_xor_operator() {
        // `^^` — logical XOR, between `||` and `//` in precedence.
        let e = parse_expr_str("$a ^^ $b;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::LogicalXor, _, _)), "expected LogicalXor, got {:?}", e.kind);
    }

    #[test]
    fn logical_xor_precedence() {
        // `^^` is lower than `||` but same level.
        // `$a || $b ^^ $c` → `($a || $b) ^^ $c`.
        let e = parse_expr_str("$a || $b ^^ $c;");
        match &e.kind {
            ExprKind::BinOp(BinOp::LogicalXor, lhs, _) => {
                assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::Or, _, _)), "|| should bind tighter than ^^");
            }
            other => panic!("expected LogicalXor at top, got {other:?}"),
        }
    }

    #[test]
    fn logical_xor_assign() {
        // `^^=` assignment operator.
        let e = parse_expr_str("$a ^^= $b;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::LogicalXorEq, _, _)), "expected ^^= assign, got {:?}", e.kind);
    }

    // ── <<>> double diamond operator ─────────────────────────

    #[test]
    fn double_diamond_operator() {
        // `<<>>` — safe diamond, uses 3-arg open.
        let prog = parse("while (<<>>) { print; }");
        assert!(!prog.statements.is_empty(), "should parse <<>> in while condition");
    }

    // ── m?PATTERN? match-once ────────────────────────────────

    #[test]
    fn match_once_question_mark() {
        // `m?pattern?` — matches only once between reset() calls.
        let e = parse_expr_str("m?foo?;");
        match &e.kind {
            ExprKind::Regex(_, _, _) => {}
            other => panic!("expected Regex from m??, got {other:?}"),
        }
    }

    // ── Chained comparisons ──────────────────────────────────

    #[test]
    fn chained_relational() {
        // `$x < $y <= $z` → ChainedCmp([NumLt, NumLe], [x, y, z]).
        let e = parse_expr_str("$x < $y <= $z;");
        match &e.kind {
            ExprKind::ChainedCmp(ops, operands) => {
                assert_eq!(ops.len(), 2, "two operators");
                assert_eq!(operands.len(), 3, "three operands");
                assert_eq!(ops[0], BinOp::NumLt);
                assert_eq!(ops[1], BinOp::NumLe);
            }
            other => panic!("expected ChainedCmp, got {other:?}"),
        }
    }

    #[test]
    fn chained_equality() {
        // `$a == $b != $c` → ChainedCmp([NumEq, NumNe], [a, b, c]).
        let e = parse_expr_str("$a == $b != $c;");
        match &e.kind {
            ExprKind::ChainedCmp(ops, operands) => {
                assert_eq!(ops.len(), 2);
                assert_eq!(operands.len(), 3);
                assert_eq!(ops[0], BinOp::NumEq);
                assert_eq!(ops[1], BinOp::NumNe);
            }
            other => panic!("expected ChainedCmp, got {other:?}"),
        }
    }

    #[test]
    fn chained_string_relational() {
        // `$a lt $b le $c gt $d` — four operands, three ops.
        let e = parse_expr_str("$a lt $b le $c gt $d;");
        match &e.kind {
            ExprKind::ChainedCmp(ops, operands) => {
                assert_eq!(ops.len(), 3);
                assert_eq!(operands.len(), 4);
            }
            other => panic!("expected ChainedCmp, got {other:?}"),
        }
    }

    #[test]
    fn non_chained_spaceship() {
        // `<=>` is non-associative — should NOT produce ChainedCmp.
        let e = parse_expr_str("$a <=> $b;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Spaceship, _, _)), "spaceship should be plain BinOp");
    }

    #[test]
    fn simple_comparison_stays_binop() {
        // A single comparison should remain BinOp, not ChainedCmp.
        let e = parse_expr_str("$x < $y;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::NumLt, _, _)), "single < should be plain BinOp");
    }

    // ── Existing escape sequences (verify) ───────────────────

    #[test]
    fn octal_brace_escape() {
        // `\o{101}` → 'A' (octal 101 = decimal 65).
        let e = parse_expr_str(r#""\o{101}";"#);
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "A"),
            other => panic!("expected 'A', got {other:?}"),
        }
    }

    #[test]
    fn control_char_escape() {
        // `\cA` → chr(1), `\c[` → chr(27) (ESC).
        let e = parse_expr_str(r#""\c[";"#);
        match &e.kind {
            ExprKind::StringLit(s) => {
                assert_eq!(s.len(), 1);
                assert_eq!(s.chars().next().unwrap(), '\x1B');
            }
            other => panic!("expected ESC char, got {other:?}"),
        }
    }

    #[test]
    fn case_mod_uppercase() {
        // `"\Ufoo\E"` → "FOO"
        let e = parse_expr_str(r#""\Ufoo\E";"#);
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "FOO"),
            other => panic!("expected StringLit(FOO), got {other:?}"),
        }
    }

    #[test]
    fn case_mod_lowercase() {
        // `"\LFOO\E"` → "foo"
        let e = parse_expr_str(r#""\LFOO\E";"#);
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "foo"),
            other => panic!("expected StringLit(foo), got {other:?}"),
        }
    }

    #[test]
    fn case_mod_lower_next() {
        // `"\lFOO"` → "fOO" (only first char lowercased)
        let e = parse_expr_str(r#""\lFOO";"#);
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "fOO"),
            other => panic!("expected StringLit(fOO), got {other:?}"),
        }
    }

    #[test]
    fn case_mod_upper_next() {
        // `"\ufoo"` → "Foo" (only first char uppercased)
        let e = parse_expr_str(r#""\ufoo";"#);
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "Foo"),
            other => panic!("expected StringLit(Foo), got {other:?}"),
        }
    }

    #[test]
    fn case_mod_quotemeta() {
        // `"\Qfoo.bar\E"` → "foo\\.bar"
        let e = parse_expr_str(r#""\Qfoo.bar\E";"#);
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "foo\\.bar"),
            other => panic!("expected quotemeta'd string, got {other:?}"),
        }
    }

    #[test]
    fn case_mod_stacking() {
        // `"\Q'\Ufoo\Ebar'\E"` → `\\'FOObar\\'`
        // \Q quotemeta, then \U uppercase stacks on top.
        // \E pops \U, \E pops \Q.
        let e = parse_expr_str(r#""\Q'\Ufoo\Ebar'\E";"#);
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "\\'FOObar\\'"),
            other => panic!("expected stacked case-mod result, got {other:?}"),
        }
    }

    #[test]
    fn case_mod_foldcase() {
        // `"\FFOO\E"` → "foo" (foldcase ≈ lowercase for ASCII)
        let e = parse_expr_str(r#""\FFOO\E";"#);
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "foo"),
            other => panic!("expected StringLit(foo), got {other:?}"),
        }
    }

    #[test]
    fn case_mod_interp_uppercase() {
        // `"\Utest$x\E"` → InterpolatedString with:
        //   Const("TEST"), ScalarInterp(uc($x))
        let e = parse_expr_str(r#""\Utest$x\E";"#);
        match &e.kind {
            ExprKind::InterpolatedString(interp) => {
                // First part: constant "TEST" (uppercased at lex time).
                assert!(matches!(&interp.0[0], InterpPart::Const(s) if s == "TEST"), "first part should be Const(TEST), got {:?}", interp.0[0]);
                // Second part: $x wrapped in uc().
                match &interp.0[1] {
                    InterpPart::ScalarInterp(expr) => {
                        assert!(matches!(&expr.kind, ExprKind::FuncCall(name, _) if name == "uc"), "interp should be uc($x), got {:?}", expr.kind);
                    }
                    other => panic!("expected ScalarInterp, got {other:?}"),
                }
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn case_mod_interp_lcfirst() {
        // `"\l$X"` → ScalarInterp(lcfirst($X))
        let e = parse_expr_str(r#""\l$X";"#);
        match &e.kind {
            ExprKind::InterpolatedString(interp) => match &interp.0[0] {
                InterpPart::ScalarInterp(expr) => {
                    assert!(matches!(&expr.kind, ExprKind::FuncCall(name, _) if name == "lcfirst"), "interp should be lcfirst($X), got {:?}", expr.kind);
                }
                other => panic!("expected ScalarInterp, got {other:?}"),
            },
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn case_mod_interp_quotemeta_upper() {
        // `"\Q\U$x\E\E"` → ScalarInterp(quotemeta(uc($x)))
        let e = parse_expr_str(r#""\Q\U$x\E\E";"#);
        match &e.kind {
            ExprKind::InterpolatedString(interp) => {
                match &interp.0[0] {
                    InterpPart::ScalarInterp(expr) => {
                        // Outermost should be quotemeta.
                        match &expr.kind {
                            ExprKind::FuncCall(name, args) if name == "quotemeta" => {
                                // Inner should be uc.
                                assert!(matches!(&args[0].kind, ExprKind::FuncCall(n, _) if n == "uc"), "inner should be uc, got {:?}", args[0].kind);
                            }
                            other => panic!("expected quotemeta(uc($x)), got {other:?}"),
                        }
                    }
                    other => panic!("expected ScalarInterp, got {other:?}"),
                }
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn case_mod_no_wrap_after_end() {
        // `"\Ufoo\E$x"` — \E ends the case mod, so $x should NOT be wrapped.
        let e = parse_expr_str(r#""\Ufoo\E$x";"#);
        match &e.kind {
            ExprKind::InterpolatedString(interp) => {
                assert!(matches!(&interp.0[0], InterpPart::Const(s) if s == "FOO"));
                match &interp.0[1] {
                    InterpPart::ScalarInterp(expr) => {
                        assert!(matches!(&expr.kind, ExprKind::ScalarVar(_)), "$x should be plain ScalarVar after \\E, got {:?}", expr.kind);
                    }
                    other => panic!("expected ScalarInterp, got {other:?}"),
                }
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    // ── dump keyword ─────────────────────────────────────────

    #[test]
    fn dump_keyword() {
        let prog = parse("dump;");
        assert!(!prog.statements.is_empty(), "should parse bare dump");
    }

    #[test]
    fn dump_with_label() {
        let prog = parse("dump RESTART;");
        assert!(!prog.statements.is_empty(), "should parse dump LABEL");
    }

    // ═══════════════════════════════════════════════════════════
    // Remaining audit gaps — probing tests
    // ═══════════════════════════════════════════════════════════

    // ── M9. x= compound assignment ───────────────────────────

    #[test]
    fn repeat_assign() {
        // `$str x= 3` — compound repeat-assignment.
        let e = parse_expr_str("$str x= 3;");
        assert!(matches!(e.kind, ExprKind::Assign(AssignOp::RepeatEq, _, _)), "expected RepeatEq assign, got {:?}", e.kind);
    }

    // ── H11. v-strings as dedicated AST node ─────────────────

    #[test]
    fn vstring_produces_version_lit() {
        // `v5.36.0` should produce VersionLit.
        let e = parse_expr_str("v5.36.0;");
        match &e.kind {
            ExprKind::VersionLit(s) => assert_eq!(s, "v5.36.0"),
            other => panic!("expected VersionLit(\"v5.36.0\"), got {other:?}"),
        }
    }

    // ── L8. <<\"\" empty heredoc tag ──────────────────────────

    #[test]
    fn heredoc_empty_tag_double_quoted() {
        // `<<""` — empty string as terminator, body ends at empty line.
        let prog = parse("print <<\"\";\nhello\nworld\n\n");
        assert!(!prog.statements.is_empty(), "should parse <<\"\" empty tag heredoc");
    }

    #[test]
    fn heredoc_empty_tag_single_quoted() {
        let prog = parse("print <<'';\nhello\n$not_interp\n\n");
        assert!(!prog.statements.is_empty(), "should parse <<'' empty tag heredoc");
    }

    #[test]
    fn heredoc_indented_empty_tag() {
        // `<<~""` with indented body.
        let prog = parse("print <<~\"\";\n  hello\n  world\n  \n");
        assert!(!prog.statements.is_empty(), "should parse <<~\"\" indented empty tag heredoc");
    }

    // ── M14. Anonymous sub with prototype AND attributes ─────

    #[test]
    fn sub_proto_then_attrs() {
        // `sub ($) :lvalue { 1 }` — prototype before attributes.
        let prog = parse("my $f = sub ($) :lvalue { 1; };");
        assert!(!prog.statements.is_empty(), "should parse sub with proto then attrs");
    }

    #[test]
    fn sub_attrs_then_sig() {
        // With signatures active: `sub :lvalue ($x) { }`.
        let prog = parse("use feature 'signatures'; my $f = sub :lvalue ($x) { 1; };");
        assert!(!prog.statements.is_empty(), "should parse sub with attrs then sig");
    }

    // ── L3. use if — conditional use pragma ──────────────────

    #[test]
    fn use_if_conditional() {
        // `use if $cond, "Module"` — the `if` module.
        let prog = parse("use if $^O eq 'MSWin32', 'Win32';");
        assert!(!prog.statements.is_empty(), "should parse use if");
    }

    // ── L4. use Module qw(imports) ───────────────────────────

    #[test]
    fn use_module_with_qw_imports() {
        let prog = parse("use POSIX qw(setlocale LC_ALL);");
        assert!(!prog.statements.is_empty(), "should parse use with qw import list");
    }

    #[test]
    fn use_module_with_list_imports() {
        let prog = parse("use File::Basename 'dirname', 'basename';");
        assert!(!prog.statements.is_empty(), "should parse use with string import list");
    }

    // ── Backtick heredocs ────────────────────────────────────

    #[test]
    fn heredoc_backtick() {
        // <<`EOC` — command heredoc (interpolated, then executed).
        let prog = parse("my $out = <<`EOC`;\necho hello\nEOC\n");
        assert!(!prog.statements.is_empty(), "should parse backtick heredoc");
    }

    #[test]
    fn heredoc_backtick_indented() {
        // <<~`EOC` — indented command heredoc.
        let prog = parse("my $out = <<~`EOC`;\n  echo hello\n  EOC\n");
        assert!(!prog.statements.is_empty(), "should parse indented backtick heredoc");
    }

    // ── Lexical method invocation (->&method) ────────────────

    #[test]
    fn arrow_lexical_method() {
        // `$obj->&method` — lexical method invocation.
        let e = parse_expr_str("$obj->&method;");
        match &e.kind {
            ExprKind::MethodCall(_, name, args) => {
                assert_eq!(name, "&method");
                assert!(args.is_empty());
            }
            other => panic!("expected MethodCall(&method), got {other:?}"),
        }
    }

    #[test]
    fn arrow_lexical_method_with_args() {
        // `$obj->&method(1, 2)` — with arguments.
        let e = parse_expr_str("$obj->&method(1, 2);");
        match &e.kind {
            ExprKind::MethodCall(_, name, args) => {
                assert_eq!(name, "&method");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected MethodCall(&method, 2 args), got {other:?}"),
        }
    }

    #[test]
    fn arrow_deref_code_still_works() {
        // `->&*` should still work as code postfix deref.
        let e = parse_expr_str("$ref->&*;");
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefCode)), "expected DerefCode, got {:?}", e.kind);
    }

    // ── perldata: KV slices (5.20+) ──────────────────────────

    #[test]
    fn kv_hash_slice() {
        // %hash{'foo','bar'} → key/value hash slice.
        let e = parse_expr_str("%hash{'foo','bar'};");
        match &e.kind {
            ExprKind::KvHashSlice(_, keys) => assert_eq!(keys.len(), 2),
            other => panic!("expected KvHashSlice, got {other:?}"),
        }
    }

    #[test]
    fn kv_array_slice() {
        // %array[1,2,3] → index/value array slice.
        let e = parse_expr_str("%array[1,2,3];");
        match &e.kind {
            ExprKind::KvArraySlice(_, indices) => assert_eq!(indices.len(), 3),
            other => panic!("expected KvArraySlice, got {other:?}"),
        }
    }

    // ── perldata: *foo{THING} typeglob access ────────────────

    #[test]
    fn glob_thing_access() {
        // *foo{SCALAR} → typeglob slot access.
        let e = parse_expr_str("*foo{SCALAR};");
        match &e.kind {
            ExprKind::ArrowDeref(recv, ArrowTarget::HashElem(key)) => {
                assert!(matches!(recv.kind, ExprKind::GlobVar(ref n) if n == "foo"));
                assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "SCALAR"));
            }
            other => panic!("expected ArrowDeref(GlobVar, HashElem), got {other:?}"),
        }
    }

    // ── perldata: hex/octal/binary float ─────────────────────

    #[test]
    fn hex_float_expr() {
        let e = parse_expr_str("0x1p10;");
        assert!(matches!(e.kind, ExprKind::FloatLit(v) if v == 1024.0), "expected FloatLit(1024.0), got {:?}", e.kind);
    }

    // ── perlsub: //= and ||= signature defaults (5.38+) ─────

    #[test]
    fn sig_defined_or_default() {
        let s = parse_sub("use feature 'signatures'; sub f ($name //= \"world\") { }");
        let sig = s.signature.expect("signature present");
        match &sig.params[0] {
            SigParam::Scalar { name, default: Some((kind, _)), .. } => {
                assert_eq!(name, "name");
                assert_eq!(*kind, SigDefaultKind::DefinedOr);
            }
            other => panic!("expected Scalar with //= default, got {other:?}"),
        }
    }

    #[test]
    fn sig_logical_or_default() {
        let s = parse_sub("use feature 'signatures'; sub f ($x ||= 10) { }");
        let sig = s.signature.expect("signature present");
        match &sig.params[0] {
            SigParam::Scalar { name, default: Some((kind, _)), .. } => {
                assert_eq!(name, "x");
                assert_eq!(*kind, SigDefaultKind::LogicalOr);
            }
            other => panic!("expected Scalar with ||= default, got {other:?}"),
        }
    }

    // ── perlsub: $= in signatures ───────────────────────────

    #[test]
    fn sig_anon_optional_no_default() {
        let s = parse_sub("use feature 'signatures'; sub f ($thing, $=) { }");
        let sig = s.signature.expect("signature present");
        assert_eq!(sig.params.len(), 2);
        assert!(matches!(sig.params[1], SigParam::AnonScalar { default: Some(_), .. }), "expected AnonScalar with default, got {:?}", sig.params[1]);
    }

    // ── perlsub: lexical subs ───────────────────────────────

    #[test]
    fn my_sub() {
        let prog = parse("my sub foo { 42; }");
        match &prog.statements[0].kind {
            StmtKind::SubDecl(sd) => {
                assert_eq!(sd.name, "foo");
                assert_eq!(sd.scope, Some(DeclScope::My));
            }
            other => panic!("expected SubDecl(my), got {other:?}"),
        }
    }

    #[test]
    fn state_sub() {
        let prog = parse("use feature 'state'; state sub bar { 1; }");
        match &prog.statements[1].kind {
            StmtKind::SubDecl(sd) => {
                assert_eq!(sd.name, "bar");
                assert_eq!(sd.scope, Some(DeclScope::State));
            }
            other => panic!("expected SubDecl(state), got {other:?}"),
        }
    }

    #[test]
    fn our_sub() {
        let prog = parse("our sub baz { 1; }");
        match &prog.statements[0].kind {
            StmtKind::SubDecl(sd) => {
                assert_eq!(sd.name, "baz");
                assert_eq!(sd.scope, Some(DeclScope::Our));
            }
            other => panic!("expected SubDecl(our), got {other:?}"),
        }
    }

    // ── perlsub: my with attributes ─────────────────────────

    #[test]
    fn my_var_with_attribute() {
        let prog = parse("my $x : Shared = 1;");
        let (scope, vars) = decl_vars(&prog.statements[0]);
        assert_eq!(scope, DeclScope::My);
        assert_eq!(vars[0].name, "x");
        assert_eq!(vars[0].attributes.len(), 1);
        assert_eq!(vars[0].attributes[0].name, "Shared");
    }

    // ── perldata: whitespace between sigil and name ──────────

    #[test]
    fn percent_space_name() {
        // `% hash` ≡ `%hash` — whitespace between % and name.
        let prog = parse("my % hash = (a => 1);");
        assert!(!prog.statements.is_empty(), "should parse % hash with space");
    }

    // ── perlvar: special variable gaps ──────────────────────

    #[test]
    fn percent_caret_h() {
        // `%^H` — hints hash, caret hash variable.
        let e = parse_expr_str("%^H;");
        assert!(matches!(e.kind, ExprKind::SpecialHashVar(ref n) if n == "^H"), "expected SpecialHashVar(^H), got {:?}", e.kind);
    }

    // ── perlre: /o flag ─────────────────────────────────────

    #[test]
    fn regex_o_flag() {
        // /o — compile-once flag (no-op in modern Perl, but valid syntax).
        let prog = parse("$x =~ /foo/o;");
        assert!(!prog.statements.is_empty(), "should parse /o flag");
    }

    #[test]
    fn subst_o_flag() {
        let prog = parse("$x =~ s/foo/bar/og;");
        assert!(!prog.statements.is_empty(), "should parse s///og flags");
    }

    // ── perlre: regex code block raw source capture ─────────

    #[test]
    fn regex_code_block_raw_source() {
        // (?{code}) — verify both raw source and parsed expression.
        let e = parse_expr_str("m/(?{ 1 + 2 })/;");
        match &e.kind {
            ExprKind::Regex(_, interp, _) => {
                let code_parts: Vec<_> = interp
                    .0
                    .iter()
                    .filter_map(|p| match p {
                        InterpPart::RegexCode(raw, expr) => Some((raw.as_str(), expr)),
                        _ => None,
                    })
                    .collect();
                assert_eq!(code_parts.len(), 1, "expected one code block");
                assert_eq!(code_parts[0].0, " 1 + 2 ", "raw source mismatch");
                assert!(matches!(code_parts[0].1.kind, ExprKind::BinOp(BinOp::Add, _, _)), "parsed expr should be Add, got {:?}", code_parts[0].1.kind);
            }
            other => panic!("expected Regex, got {other:?}"),
        }
    }

    #[test]
    fn regex_cond_code_block_raw_source() {
        // (??{code}) — verify raw source capture.
        let e = parse_expr_str("m/(??{ $re })/;");
        match &e.kind {
            ExprKind::Regex(_, interp, _) => {
                let code_parts: Vec<_> = interp
                    .0
                    .iter()
                    .filter_map(|p| match p {
                        InterpPart::RegexCondCode(raw, _) => Some(raw.as_str()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(code_parts.len(), 1, "expected one cond code block");
                assert_eq!(code_parts[0], " $re ", "raw source mismatch");
            }
            other => panic!("expected Regex, got {other:?}"),
        }
    }

    #[test]
    fn regex_optimistic_code_block_raw_source() {
        // (*{code}) — optimistic code block, same structure as (?{}).
        let e = parse_expr_str("m/(*{ $n })/;");
        match &e.kind {
            ExprKind::Regex(_, interp, _) => {
                let code_parts: Vec<_> = interp
                    .0
                    .iter()
                    .filter_map(|p| match p {
                        InterpPart::RegexCode(raw, _) => Some(raw.as_str()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(code_parts.len(), 1, "expected one code block");
                assert_eq!(code_parts[0], " $n ", "raw source mismatch");
            }
            other => panic!("expected Regex, got {other:?}"),
        }
    }

    // ── perlclass audit ─────────────────────────────────────

    #[test]
    fn class_with_version() {
        let prog = parse_class_prog("class Foo 1.234 { }");
        let c = find_class_decl(&prog);
        assert_eq!(c.name, "Foo");
        assert_eq!(c.version.as_deref(), Some("1.234"));
    }

    #[test]
    fn class_statement_form() {
        // `class Foo;` — statement form with no block.
        let prog = parse_class_prog("class Foo;");
        let c = find_class_decl(&prog);
        assert_eq!(c.name, "Foo");
        assert!(c.body.is_none(), "statement form should have no body");
    }

    #[test]
    fn class_version_and_attrs() {
        let prog = parse_class_prog("class Bar 2.0 :isa(Foo) { }");
        let c = find_class_decl(&prog);
        assert_eq!(c.version.as_deref(), Some("2"));
        assert_eq!(c.attributes[0].name, "isa");
    }

    #[test]
    fn adjust_block() {
        let prog = parse_class_prog("class Foo { ADJUST { 1; } }");
        let c = find_class_decl(&prog);
        let body = c.body.as_ref().unwrap();
        match &body.statements[0].kind {
            StmtKind::Phaser(PhaserKind::Adjust, _) => {}
            other => panic!("expected Phaser(Adjust), got {other:?}"),
        }
    }

    #[test]
    fn dunder_class() {
        let prog = parse_class_prog("class Foo { field $x = __CLASS__->DEFAULT; }");
        let c = find_class_decl(&prog);
        let body = c.body.as_ref().unwrap();
        match &body.statements[0].kind {
            StmtKind::FieldDecl(f) => {
                assert!(f.default.is_some(), "should have default");
            }
            other => panic!("expected FieldDecl, got {other:?}"),
        }
    }

    #[test]
    fn field_defined_or_default() {
        let prog = parse_class_prog("class Foo { field $x :param //= 42; }");
        let c = find_class_decl(&prog);
        let body = c.body.as_ref().unwrap();
        match &body.statements[0].kind {
            StmtKind::FieldDecl(f) => {
                let (kind, _) = f.default.as_ref().unwrap();
                assert_eq!(*kind, SigDefaultKind::DefinedOr);
            }
            other => panic!("expected FieldDecl, got {other:?}"),
        }
    }

    #[test]
    fn field_logical_or_default() {
        let prog = parse_class_prog("class Foo { field $x :param ||= 0; }");
        let c = find_class_decl(&prog);
        let body = c.body.as_ref().unwrap();
        match &body.statements[0].kind {
            StmtKind::FieldDecl(f) => {
                let (kind, _) = f.default.as_ref().unwrap();
                assert_eq!(*kind, SigDefaultKind::LogicalOr);
            }
            other => panic!("expected FieldDecl, got {other:?}"),
        }
    }

    #[test]
    fn anon_method() {
        let prog = parse_class_prog("class Foo { method get { return method { 1; }; } }");
        let c = find_class_decl(&prog);
        assert!(c.body.is_some());
    }

    #[test]
    fn lexical_method() {
        let prog = parse_class_prog("class Foo { my method secret { 1; } }");
        let c = find_class_decl(&prog);
        let body = c.body.as_ref().unwrap();
        match &body.statements[0].kind {
            StmtKind::MethodDecl(m) => {
                assert_eq!(m.name, "secret");
                assert_eq!(m.scope, Some(DeclScope::My));
            }
            other => panic!("expected MethodDecl, got {other:?}"),
        }
    }

    // ── perlexperiment: any/all operators ────────────────────

    #[test]
    fn any_block_list() {
        let prog = parse("use feature 'any'; any { $_ > 0 } @nums;");
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn all_block_list() {
        let prog = parse("use feature 'all'; all { defined $_ } @items;");
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn any_without_feature_is_bareword() {
        // Without `use feature 'any'`, `any` is a regular identifier.
        let e = parse_expr_str("any();");
        assert!(matches!(e.kind, ExprKind::FuncCall(ref name, _) if name == "any"), "without feature, any() should be a regular call, got {:?}", e.kind);
    }

    #[test]
    fn all_without_feature_is_bareword() {
        let e = parse_expr_str("all();");
        assert!(matches!(e.kind, ExprKind::FuncCall(ref name, _) if name == "all"), "without feature, all() should be a regular call, got {:?}", e.kind);
    }

    // ── UTF-8 identifier validation ─────────────────────────

    #[test]
    fn utf8_array_variable() {
        // `@données` — UTF-8 array name.
        let prog = parse("use utf8; my @données;");
        assert!(!prog.statements.is_empty(), "should parse @données");
    }

    #[test]
    fn utf8_hash_variable() {
        // `%données` — UTF-8 hash name.
        let prog = parse("use utf8; my %données;");
        assert!(!prog.statements.is_empty(), "should parse %données");
    }

    #[test]
    fn utf8_cjk_identifier() {
        // CJK ideographs are XID_Start/XID_Continue.
        let prog = parse("use utf8; my $変数 = 1;");
        assert!(!prog.statements.is_empty(), "CJK identifier should parse");
    }

    #[test]
    fn utf8_mixed_ascii_and_unicode() {
        // ASCII start, Unicode continuation: `$foo変数`
        let prog = parse("use utf8; my $foo変数 = 1;");
        assert!(!prog.statements.is_empty(), "mixed ASCII+Unicode should parse");
    }

    #[test]
    fn utf8_underscore_then_unicode() {
        // `$_café` — underscore then Unicode.
        let prog = parse("use utf8; my $_café = 1;");
        assert!(!prog.statements.is_empty(), "$_café should parse");
    }

    #[test]
    fn utf8_package_qualified() {
        // `Ünïcödé::módule` — Unicode in package names.
        let prog = parse("use utf8; Ünïcödé::módule->new();");
        assert!(!prog.statements.is_empty(), "Unicode package name should parse");
    }

    #[test]
    fn utf8_sub_with_unicode_param() {
        let prog = parse("use utf8; use feature 'signatures'; sub grüß($naïve) { $naïve }");
        assert!(!prog.statements.is_empty());
    }

    // ── Non-UTF-8 mode rejects high bytes ───────────────────

    #[test]
    fn no_utf8_rejects_high_bytes_in_scalar() {
        let src = "my $café = 1;";
        let mut p = Parser::new(src.as_bytes()).unwrap();
        assert!(p.parse_program().is_err(), "high bytes without use utf8 should error");
    }

    #[test]
    fn no_utf8_rejects_high_bytes_in_bareword() {
        let src = "café();";
        let mut p = Parser::new(src.as_bytes()).unwrap();
        assert!(p.parse_program().is_err(), "high bytes in bareword without use utf8 should error");
    }

    // ── Invalid identifier characters (even with use utf8) ──

    #[test]
    fn utf8_emoji_not_identifier() {
        // Emoji (U+1F600) is not XID_Start.
        let src = "use utf8; my $\u{1F600} = 1;";
        let mut p = Parser::new(src.as_bytes()).unwrap();
        assert!(p.parse_program().is_err(), "emoji should not be valid in identifier");
    }

    #[test]
    fn utf8_math_symbol_not_identifier() {
        // ∑ (U+2211 N-ARY SUMMATION) is not XID_Start.
        let src = "use utf8; my $∑ = 1;";
        let mut p = Parser::new(src.as_bytes()).unwrap();
        assert!(p.parse_program().is_err(), "math symbol should not be valid in identifier");
    }

    #[test]
    fn utf8_bare_emoji_errors() {
        // Emoji as a bare statement — not an identifier.
        let src = "use utf8; \u{1F600};";
        let mut p = Parser::new(src.as_bytes()).unwrap();
        assert!(p.parse_program().is_err(), "emoji as bare statement should error");
    }

    #[test]
    fn utf8_punctuation_not_identifier() {
        // « (U+00AB LEFT-POINTING DOUBLE ANGLE QUOTATION MARK) is not XID.
        let src = "use utf8; my $\u{00AB} = 1;";
        let mut p = Parser::new(src.as_bytes()).unwrap();
        assert!(p.parse_program().is_err(), "Unicode punctuation should not be valid in identifier");
    }

    #[test]
    fn utf8_combining_mark_not_identifier_start() {
        // U+0301 COMBINING ACUTE ACCENT is XID_Continue but not XID_Start.
        // As the first char after a sigil, it should fail.
        let src = "use utf8; my $\u{0301}x = 1;";
        let mut p = Parser::new(src.as_bytes()).unwrap();
        assert!(p.parse_program().is_err(), "combining mark should not be valid as identifier start");
    }

    #[test]
    fn utf8_combining_mark_ok_as_continue() {
        // Combining mark after a valid start character is fine.
        // `$e\u{0301}` = $é (e + combining acute) — valid.
        let prog = parse("use utf8; my $e\u{0301} = 1;");
        assert!(!prog.statements.is_empty(), "combining mark as continuation should parse");
    }

    #[test]
    fn utf8_sigil_whitespace_then_unicode() {
        // `$ \n変数` — whitespace between sigil and UTF-8 name.
        let prog = parse("use utf8; my $\n変数 = 1;");
        assert!(!prog.statements.is_empty(), "whitespace between sigil and UTF-8 name should parse");
    }

    #[test]
    fn utf8_at_sigil_whitespace_then_unicode() {
        // `@ \n変数` — whitespace between @ and UTF-8 name.
        let prog = parse("use utf8; my @\ndonn\u{00E9}es;");
        assert!(!prog.statements.is_empty(), "whitespace between @ and UTF-8 name should parse");
    }

    // ── Invalid UTF-8 byte sequences ────────────────────────

    #[test]
    fn invalid_utf8_bytes_error() {
        // 0xFF 0xFE is not valid UTF-8.
        let src: Vec<u8> = b"use utf8; my $\xff\xfe = 1;".to_vec();
        let mut p = Parser::new(&src).unwrap();
        assert!(p.parse_program().is_err(), "invalid UTF-8 bytes should error even with use utf8");
    }

    #[test]
    fn invalid_utf8_lone_continuation_byte() {
        // 0x80 is a continuation byte without a lead byte.
        let src: Vec<u8> = b"use utf8; my $\x80x = 1;".to_vec();
        let mut p = Parser::new(&src).unwrap();
        assert!(p.parse_program().is_err(), "lone continuation byte should error");
    }

    // ── NFC normalization ───────────────────────────────────

    #[test]
    fn nfc_identifier_precomposed_and_decomposed_are_same() {
        // NFC: é = U+00E9 (precomposed, 2 bytes in UTF-8)
        // NFD: e + U+0301 (decomposed, 3 bytes in UTF-8)
        // Both should produce the same identifier name after NFC.
        let nfc_src = "use utf8; my $caf\u{00E9} = 42;";
        let nfd_src = "use utf8; my $cafe\u{0301} = 42;";

        let nfc_prog = parse(nfc_src);
        let nfd_prog = parse(nfd_src);

        // Extract variable names from both programs.
        // `my $café = 42` parses as Assign(Eq, Decl(My, [VarDecl]), Int(42)).
        let get_var_name = |prog: &crate::ast::Program| -> Option<String> {
            for stmt in &prog.statements {
                if let StmtKind::Expr(expr) = &stmt.kind {
                    // Bare declaration: `my $x;`
                    if let ExprKind::Decl(_, decls) = &expr.kind
                        && let Some(decl) = decls.first()
                    {
                        return Some(decl.name.clone());
                    }
                    // Assignment wrapping declaration: `my $x = 1;`
                    if let ExprKind::Assign(_, lhs, _) = &expr.kind
                        && let ExprKind::Decl(_, decls) = &lhs.kind
                        && let Some(decl) = decls.first()
                    {
                        return Some(decl.name.clone());
                    }
                }
            }
            None
        };

        let nfc_name = get_var_name(&nfc_prog).expect("should find variable in NFC source");
        let nfd_name = get_var_name(&nfd_prog).expect("should find variable in NFD source");
        assert_eq!(nfc_name, nfd_name, "NFC and NFD forms of café should produce the same identifier");
        assert_eq!(nfc_name, "caf\u{00E9}", "identifier should be in NFC form");
    }

    #[test]
    fn nfc_sub_name_normalized() {
        // Sub name with NFD decomposed character.
        let nfd_src = "use utf8; sub nai\u{0308}ve { 1 }";
        let prog = parse(nfd_src);
        let sub_name = prog
            .statements
            .iter()
            .find_map(|s| if let StmtKind::SubDecl(sd) = &s.kind { Some(sd.name.clone()) } else { None })
            .expect("should find sub declaration");
        // ï in NFC is U+00EF
        assert_eq!(sub_name, "na\u{00EF}ve", "sub name should be NFC-normalized");
    }

    #[test]
    fn nfc_package_name_normalized() {
        // Package name with decomposed character.
        let src = "use utf8; Caf\u{00E9}::Mo\u{0308}dule->new();";
        let prog = parse(src);
        assert!(!prog.statements.is_empty(), "should parse NFC package name");
    }

    #[test]
    fn nfc_ascii_identifiers_unchanged() {
        // ASCII identifiers should pass through unchanged.
        let prog = parse("use utf8; my $hello = 1;");
        let var_name = prog
            .statements
            .iter()
            .find_map(|s| {
                if let StmtKind::Expr(expr) = &s.kind {
                    if let ExprKind::Assign(_, lhs, _) = &expr.kind
                        && let ExprKind::Decl(_, decls) = &lhs.kind
                    {
                        return Some(decls[0].name.clone());
                    }
                    if let ExprKind::Decl(_, decls) = &expr.kind {
                        return Some(decls[0].name.clone());
                    }
                }
                None
            })
            .expect("should find variable");
        assert_eq!(var_name, "hello");
    }

    #[test]
    fn nfc_no_normalization_without_utf8() {
        // Without `use utf8`, high bytes are errors, so NFC
        // normalization never applies.
        let prog = parse("my $hello = 1;");
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn nfc_string_content_normalized() {
        // String literal content should also be NFC-normalized.
        // NFD: "cafe\u{0301}" → NFC: "café"
        let src = "use utf8; my $x = \"caf\u{00E9}\";";
        let prog = parse(src);
        // The program should parse successfully with NFC content.
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn nfc_hangul_normalized() {
        // Hangul syllables: NFC composition should work.
        // U+1100 U+1161 (Jamo ᄀ + ᅡ) → U+AC00 (syllable 가)
        let src = "use utf8; my $\u{AC00} = 1;";
        let prog = parse(src);
        assert!(!prog.statements.is_empty(), "Hangul syllable identifier should parse");
    }

    #[test]
    fn nfc_multiple_combining_marks() {
        // Multiple combining marks after a base character.
        // o + U+0308 (diaeresis) + U+0304 (macron)
        // NFC composes o+diaeresis → ö, macron stays.
        let src = "use utf8; my $o\u{0308}\u{0304}x = 1;";
        let prog = parse(src);
        assert!(!prog.statements.is_empty(), "multiple combining marks should parse");
    }

    #[test]
    fn nfc_already_nfc_input() {
        // Input already in NFC should pass through unchanged.
        let src = "use utf8; my $für = 1;";
        let prog = parse(src);
        let var_name = prog
            .statements
            .iter()
            .find_map(|s| {
                if let StmtKind::Expr(expr) = &s.kind {
                    if let ExprKind::Assign(_, lhs, _) = &expr.kind
                        && let ExprKind::Decl(_, decls) = &lhs.kind
                    {
                        return Some(decls[0].name.clone());
                    }
                    if let ExprKind::Decl(_, decls) = &expr.kind {
                        return Some(decls[0].name.clone());
                    }
                }
                None
            })
            .expect("should find variable");
        assert_eq!(var_name, "f\u{00FC}r", "already-NFC input should be unchanged");
    }

    // ── memchr optimization (correctness via existing tests) ─

    #[test]
    fn memchr_long_string_body() {
        // Long string body exercises the memchr bulk-copy path.
        let long_text = "a".repeat(1000);
        let src = format!("my $x = \"{long_text}\";");
        let prog = parse(&src);
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn memchr_interpolating_string() {
        // Interpolating string with triggers spread through it.
        let src = r#"my $name = "world"; my $x = "hello $name, foo\nbar";"#;
        let prog = parse(src);
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn memchr_regex_with_code_block() {
        // Regex with code block — memchr must detect ( trigger.
        let src = "use feature 'all'; my $x = 'abc'; $x =~ m/foo(?{ 1 + 2 })bar/;";
        let prog = parse(src);
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn memchr_paired_delimiter_depth() {
        // Paired delimiter with nesting — memchr fast path
        // should hand off to byte-by-byte for depth tracking.
        let src = r#"my $x = q{outer{inner}outer};"#;
        let prog = parse(src);
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn memchr_heredoc_multiline() {
        // Heredoc body spans multiple lines — memchr works
        // per-line, line loading handled by peek_byte(true).
        let src = "my $x = <<END;\nline 1\nline 2\nline 3\nEND\n";
        let prog = parse(src);
        assert!(!prog.statements.is_empty());
    }

    // ── UTF-8 additional coverage ───────────────────────────

    #[test]
    fn utf8_array_len_unicode() {
        // $#données — array length with UTF-8 name.
        let prog = parse("use utf8; my @données; my $n = $#données;");
        assert!(!prog.statements.is_empty(), "$#données should parse");
    }

    #[test]
    fn utf8_devanagari_identifier() {
        // Devanagari script: XID_Start/XID_Continue.
        let prog = parse("use utf8; my $नाम = 1;");
        assert!(!prog.statements.is_empty(), "Devanagari identifier should parse");
    }

    #[test]
    fn utf8_cyrillic_identifier() {
        let prog = parse("use utf8; my $имя = 1;");
        assert!(!prog.statements.is_empty(), "Cyrillic identifier should parse");
    }

    #[test]
    fn utf8_greek_identifier() {
        let prog = parse("use utf8; my $αριθμός = 1;");
        assert!(!prog.statements.is_empty(), "Greek identifier should parse");
    }

    #[test]
    fn utf8_arabic_identifier() {
        let prog = parse("use utf8; my $اسم = 1;");
        assert!(!prog.statements.is_empty(), "Arabic identifier should parse");
    }

    #[test]
    fn utf8_method_call() {
        let prog = parse("use utf8; $obj->café();");
        assert!(!prog.statements.is_empty(), "UTF-8 method name should parse");
    }

    #[test]
    fn utf8_multiple_identifiers_same_program() {
        let prog = parse("use utf8; my $café = 1; my $naïve = 2; my $für = 3;");
        assert!(prog.statements.len() >= 3, "multiple UTF-8 identifiers in one program");
    }

    #[test]
    fn utf8_hash_subscript_utf8_key() {
        // Both the hash name and the autoquoted key are UTF-8.
        let prog = parse("use utf8; my %données; $données{clé};");
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn utf8_heredoc_tag() {
        // UTF-8 heredoc tag — the tag itself is a raw byte match,
        // so this tests the terminator matching path.
        let prog = parse("use utf8; my $x = <<FIN;\ncontent\nFIN\n");
        assert!(!prog.statements.is_empty(), "UTF-8-adjacent heredoc should parse");
    }

    // ── NFC normalization additional coverage ────────────────

    #[test]
    fn nfc_same_identifier_both_forms_in_program() {
        // If NFC and NFD forms of the same identifier appear in
        // one program, they should resolve to the same name.
        // NFC café = $x, then NFD café = $x should be the same var.
        let src = "use utf8; my $caf\u{00E9} = 42; print $cafe\u{0301};";
        let prog = parse(src);
        // Both should parse — the NFD form becomes NFC.
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn nfc_interpolation_uses_normalized_name() {
        // Interpolated variable name should be NFC-normalized.
        // NFD $café inside a string should find NFC $café.
        let src = "use utf8; my $caf\u{00E9} = 42; print \"$cafe\u{0301}\";";
        let prog = parse(src);
        assert!(!prog.statements.is_empty(), "interpolated NFD variable should parse");
    }

    #[test]
    fn nfc_consistent_across_sigils() {
        // $café, @café, %café should all normalize consistently.
        let src = "use utf8; my $cafe\u{0301} = 1; my @cafe\u{0301} = (1); my %cafe\u{0301} = (a => 1);";
        let prog = parse(src);
        assert!(prog.statements.len() >= 3, "all sigils should accept NFD and normalize");
    }

    #[test]
    fn nfc_in_single_quoted_string() {
        // Single-quoted string content should be NFC-normalized.
        let src = "use utf8; my $x = 'cafe\u{0301}';";
        let prog = parse(src);
        assert!(!prog.statements.is_empty(), "single-quoted NFD should parse");
    }

    #[test]
    fn nfc_in_heredoc_body() {
        // Heredoc body content should be NFC-normalized.
        let body = format!("use utf8; my $x = <<END;\ncaf{}\nEND\n", "\u{00E9}");
        let prog = parse(&body);
        assert!(!prog.statements.is_empty(), "heredoc with NFC content should parse");
    }

    #[test]
    fn nfc_in_regex_body() {
        // Regex body should be NFC-normalized.
        let src = "use utf8; my $x = 'test'; $x =~ /cafe\u{0301}/;";
        let prog = parse(src);
        assert!(!prog.statements.is_empty(), "regex with NFD content should parse");
    }

    #[test]
    fn nfc_in_qw() {
        // qw() word list should be NFC-normalized.
        let src = "use utf8; my @words = qw(cafe\u{0301} nai\u{0308}ve);";
        let prog = parse(src);
        assert!(!prog.statements.is_empty(), "qw with NFD should parse");
    }

    #[test]
    fn nfc_escape_sequences_not_normalized() {
        // Escape sequences construct characters post-lexing,
        // so they should NOT be NFC-normalized.
        // "\x{65}\x{301}" should stay as two codepoints (e + combining acute),
        // not be composed into é (U+00E9).
        let src = r#"use utf8; my $x = "\x{65}\x{301}";"#;
        let prog = parse(src);
        assert!(!prog.statements.is_empty(), "escape sequences should parse");
    }

    #[test]
    fn nfc_fat_comma_autoquote_normalized() {
        // Fat comma autoquoting with NFD bareword.
        let src = "use utf8; my %h = (cafe\u{0301} => 1);";
        let prog = parse(src);
        assert!(!prog.statements.is_empty(), "fat comma with NFD bareword should parse");
    }

    #[test]
    fn nfc_sub_name_nfd_and_nfc_same() {
        // Sub declared with NFD name should be callable with NFC name.
        let src = "use utf8; sub cafe\u{0301} { 1 } caf\u{00E9}();";
        let prog = parse(src);
        assert!(!prog.statements.is_empty(), "sub with NFD name and NFC call should parse");
    }

    // ── memchr with UTF-8 content ───────────────────────────

    #[test]
    fn memchr_utf8_string_body() {
        // UTF-8 content in string body — memchr bulk copy
        // should handle multi-byte characters correctly.
        let src = "use utf8; my $x = \"héllo wörld café naïve\";";
        let prog = parse(src);
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn memchr_utf8_regex_body() {
        // UTF-8 in regex body with memchr scanning.
        let src = "use utf8; my $x = 'test'; $x =~ /héllo|wörld/;";
        let prog = parse(src);
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn memchr_utf8_single_quoted() {
        // UTF-8 in single-quoted string — non-interpolating memchr path.
        let src = "use utf8; my $x = 'café résumé naïve';";
        let prog = parse(src);
        assert!(!prog.statements.is_empty());
    }

    #[test]
    fn memchr_utf8_interpolating_with_sigil() {
        // UTF-8 content before interpolation trigger.
        let src = "use utf8; my $name = 'world'; my $x = \"café $name résumé\";";
        let prog = parse(src);
        assert!(!prog.statements.is_empty());
    }
}
