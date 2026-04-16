//! Pratt parser with recursive descent for statements (§6).
//!
//! Expression assembly uses precedence climbing.  Statements, declarations,
//! blocks, and top-level forms use ordinary recursive descent that calls
//! `parse_expr` where expressions are needed.

use crate::ast::*;
use crate::error::ParseError;
use crate::keyword;
use crate::lexer::Lexer;
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
const PREC_SHIFT: Precedence = 30; // << >>
const PREC_ADD: Precedence = 32; // + - .
const PREC_MUL: Precedence = 34; // * / % x
const PREC_BINDING: Precedence = 36; // =~ !~
const PREC_UNARY: Precedence = 38; // ! ~ \ - + (prefix)
const PREC_POW: Precedence = 40; // **
const PREC_INC: Precedence = 42; // ++ -- (postfix)
const PREC_ARROW: Precedence = 44; // ->

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
}

impl Parser {
    // ── Construction ──────────────────────────────────────────

    pub fn new(src: &[u8]) -> Result<Self, ParseError> {
        let lexer = Lexer::new(src);
        Ok(Parser { lexer, current: None, lexer_error: None, depth: 0, symbols: SymbolTable::new(), current_package: std::sync::Arc::from("main") })
    }

    /// Read-only access to the accumulated symbol table.
    /// Primarily for tests and future cross-pass consumers.
    pub fn symbols(&self) -> &SymbolTable {
        &self.symbols
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
            if !"msixpgcadlun".contains(ch) {
                return Err(ParseError::new(format!("Unknown regexp modifier \"/{ch}\""), span));
            }
        }
        Ok(())
    }

    /// Validate substitution modifier flags.  Includes regex flags
    /// plus `e` (eval replacement) and `r` (non-destructive).
    fn validate_subst_flags(flags: &str, span: Span) -> Result<(), ParseError> {
        for ch in flags.chars() {
            if !"msixpgcadluner".contains(ch) {
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
                            let expr = self.with_descent(|this| {
                                let initial = this.parse_decl_expr(scope, kw_span)?;
                                this.parse_expr_continuation(initial, PREC_LOW)
                            })?;
                            let kind = self.maybe_postfix_control(expr)?;
                            let terminated = self.eat(&Token::Semi)?;
                            (kind, terminated)
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

                        // method name(params) { ... }
                        Keyword::Method => (self.parse_method(kw_span)?, false),

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
                    Err(block) => (StmtKind::Block(block), false),
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
            _ => Ok(StmtKind::Expr(expr)),
        }
    }

    // ── Variable declarations ─────────────────────────────────

    fn parse_single_var_decl(&mut self) -> Result<VarDecl, ParseError> {
        let span = self.peek_span();
        match self.next_token()?.token {
            Token::ScalarVar(name) => Ok(VarDecl { sigil: Sigil::Scalar, name, span }),
            Token::ArrayVar(name) => Ok(VarDecl { sigil: Sigil::Array, name, span }),
            Token::HashVar(name) => Ok(VarDecl { sigil: Sigil::Hash, name, span }),
            Token::Percent => {
                // my %hash — lexer emitted Percent; read the hash name.
                match self.lexer.lex_hash_var_after_percent()? {
                    Some(Token::HashVar(name)) => Ok(VarDecl { sigil: Sigil::Hash, name, span }),
                    Some(Token::SpecialHashVar(name)) => Ok(VarDecl { sigil: Sigil::Hash, name, span }),
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
    fn parse_sub_decl_body(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        let name = match self.next_token()?.token {
            Token::Ident(name) => name,
            other => return Err(ParseError::new(format!("expected sub name, got {other:?}"), start)),
        };

        let prototype_raw = self.parse_prototype()?;

        // Parse prototype string into structured form.  Invalid
        // prototypes become a parse error with the reported position
        // relative to the prototype body.
        let prototype_parsed = match &prototype_raw {
            Some(raw) => Some(SubPrototype::parse(raw).map_err(|e| ParseError::new(format!("invalid prototype: {}", e.message), start))?),
            None => None,
        };

        let attributes = self.parse_attributes()?;

        // Forward declaration: `sub name PROTO ATTRS;` with no body.
        if self.eat(&Token::Semi)? {
            let attr_names: Vec<String> = attributes.iter().map(|a| a.name.clone()).collect();
            self.symbols.entry(&self.current_package.clone()).declare_sub(&name, prototype_parsed, attr_names, true);
            // Represent as a SubDecl with an empty body for now; an
            // optional `body: None` variant would be cleaner, but
            // that's a separate AST change.
            let span = start.merge(self.peek_span());
            let body = Block { statements: Vec::new(), span };
            return Ok(StmtKind::SubDecl(SubDecl { name, prototype: prototype_raw, attributes, params: None, body, span }));
        }

        let body = self.parse_block()?;

        // Register the full definition, replacing any prior forward
        // declaration of the same name.
        let attr_names: Vec<String> = attributes.iter().map(|a| a.name.clone()).collect();
        self.symbols.entry(&self.current_package.clone()).declare_sub(&name, prototype_parsed, attr_names, false);

        Ok(StmtKind::SubDecl(SubDecl { name, prototype: prototype_raw, attributes, params: None, body, span: start.merge(self.peek_span()) }))
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
                // Optional parenthesized args
                let value = if self.at(&Token::LeftParen)? {
                    self.next_token()?;
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
            let var = self.parse_single_var_decl()?;
            let end = var.span;
            vars.push(var);
            Ok(Expr { kind: ExprKind::Decl(scope, vars), span: span.merge(end) })
        }
    }

    /// Parse an anonymous sub expression: `sub { ... }` or `sub ($x) { ... }`.
    fn parse_anon_sub(&mut self, span: Span) -> Result<Expr, ParseError> {
        let prototype = self.parse_prototype()?;

        let body = self.parse_block()?;

        Ok(Expr { span: span.merge(body.span), kind: ExprKind::AnonSub(prototype, None, body) })
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
        // If next is a variable or 'my', it's foreach-style
        if matches!(self.peek_token(), Token::Keyword(Keyword::My) | Token::ScalarVar(_)) {
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

        Ok(StmtKind::ForEach(ForEachStmt { var: None, list: first, body, continue_block }))
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
        let var = if self.eat(&Token::Keyword(Keyword::My))? {
            Some(self.parse_single_var_decl()?)
        } else if matches!(self.peek_token(), Token::ScalarVar(_)) {
            let span = self.peek_span();
            let name = match self.next_token()?.token {
                Token::ScalarVar(n) => n,
                _ => unreachable!(),
            };
            Some(VarDecl { sigil: Sigil::Scalar, name, span })
        } else {
            None
        };

        let list = self.parse_paren_expr()?;
        let body = self.parse_block()?;
        let continue_block = if self.eat(&Token::Keyword(Keyword::Continue))? { Some(self.parse_block()?) } else { None };

        Ok(StmtKind::ForEach(ForEachStmt { var, list, body, continue_block }))
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
                self.eat(&Token::Semi)?;
                return Ok(StmtKind::UseDecl(UseDecl { is_no, module: format!("{n}"), version: None, imports: None, span: start.merge(self.peek_span()) }));
            }
            Token::FloatLit(n) => {
                self.eat(&Token::Semi)?;
                return Ok(StmtKind::UseDecl(UseDecl { is_no, module: format!("{n}"), version: None, imports: None, span: start.merge(self.peek_span()) }));
            }
            Token::StrLit(n) => n, // v-strings come through as StrLit
            other => return Err(ParseError::new(format!("expected module name or version, got {other:?}"), start)),
        };

        // Optional version after the module name: `use Module 1.23;`
        // or `use Module v5.26;`.  Versions are either numeric literals or
        // v-string StrLit tokens; anything else starts the import list.
        let version = match self.peek_token() {
            Token::IntLit(_) | Token::FloatLit(_) => {
                let tok = self.next_token()?;
                Some(match tok.token {
                    Token::IntLit(n) => format!("{n}"),
                    Token::FloatLit(n) => format!("{n}"),
                    _ => unreachable!(),
                })
            }
            // v-string literal like v5.26.0 — lexed as StrLit.
            Token::StrLit(s) if s.starts_with('v') && s.len() > 1 && s.as_bytes()[1].is_ascii_digit() => {
                let tok = self.next_token()?;
                Some(match tok.token {
                    Token::StrLit(s) => s,
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

        // Consume body lines until a line containing just '.'
        // We scan raw bytes since format bodies are not normal Perl code.
        let body_start = self.lexer.current_pos();
        self.lexer.skip_format_body();
        let body_end = self.lexer.current_pos();
        let body = String::from_utf8_lossy(self.lexer.slice(body_start, body_end)).into_owned();

        Ok(StmtKind::FormatDecl(FormatDecl { name, body, span: start.merge(self.peek_span()) }))
    }

    // ── class / field / method (5.38+ Corinna) ────────────────

    fn parse_class(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        let name = match self.next_token()?.token {
            Token::Ident(n) => n,
            other => return Err(ParseError::new(format!("expected class name, got {other:?}"), start)),
        };

        let attributes = self.parse_attributes()?;
        let body = self.parse_block()?;

        Ok(StmtKind::ClassDecl(ClassDecl { name, attributes, body, span: start.merge(self.peek_span()) }))
    }

    fn parse_field(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        let var = self.parse_single_var_decl()?;
        let attributes = self.parse_attributes()?;

        let default = if self.eat(&Token::Assign(AssignOp::Eq))? { Some(self.parse_expr(PREC_COMMA)?) } else { None };

        self.eat(&Token::Semi)?;

        Ok(StmtKind::FieldDecl(FieldDecl { var, attributes, default, span: start.merge(self.peek_span()) }))
    }

    fn parse_method(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        let name = match self.next_token()?.token {
            Token::Ident(n) => n,
            other => return Err(ParseError::new(format!("expected method name, got {other:?}"), start)),
        };

        let prototype = self.parse_prototype()?;

        let attributes = self.parse_attributes()?;
        let body = self.parse_block()?;

        Ok(StmtKind::MethodDecl(SubDecl { name, prototype, attributes, params: None, body, span: start.merge(self.peek_span()) }))
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
            let start = this.peek_span();
            this.expect_token(&Token::LeftBrace)?;

            let mut statements = Vec::new();
            while !this.at(&Token::RightBrace)? && !this.at_eof()? {
                statements.push(this.parse_statement()?);
            }

            let end = this.peek_span();
            this.expect_token(&Token::RightBrace)?;

            Ok(Block { statements, span: start.merge(end) })
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
                            // Preserve the << span as the start.
                            e.span = span.merge(e.span);
                            // kind/delim unused — the Heredoc QuoteKind is implicit.
                            let _ = (kind, delim);
                            e
                        })
                    }
                    Some(Token::HeredocLit(_kind, _tag, body)) => Ok(Expr { kind: ExprKind::StringLit(body), span }),
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
            Token::HashVar(name) => Ok(Expr { kind: ExprKind::HashVar(name), span }),
            Token::GlobVar(name) => Ok(Expr { kind: ExprKind::GlobVar(name), span }),
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
                    Some(Token::HashVar(name)) => Ok(Expr { kind: ExprKind::HashVar(name), span }),
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
                    Ok(Expr { kind: ExprKind::GlobVar(name), span: span.merge(name_span) })
                } else {
                    let operand = self.parse_deref_operand()?;
                    Ok(Expr { span: span.merge(operand.span), kind: ExprKind::Deref(Sigil::Glob, Box::new(operand)) })
                }
            }

            Token::Ident(name) => self.parse_ident_term(name, span),

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
                    Ok(Expr { kind: ExprKind::List(vec![]), span })
                } else {
                    let inner = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RightParen)?;
                    Ok(Expr { kind: ExprKind::Paren(Box::new(inner)), span: span.merge(end) })
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

            // Filetest operators: -e, -f, -d, etc. (lexed as single token)
            Token::Filetest(test_byte) => self.parse_filetest(test_byte, span),

            // Yada yada yada (...)
            Token::DotDotDot => Ok(Expr { kind: ExprKind::YadaYada, span }),

            // Readline / diamond: <STDIN>, <>, <$fh>, <*.txt>
            Token::Readline(content) => Self::readline_expr(content, span),

            // < in term position: try readline.  The lexer emitted NumLt;
            // we ask it to attempt readline scanning.  If not a readline,
            // that's a parse error (less-than is not a valid term).
            Token::NumLt => {
                if let Some(Token::Readline(content)) = self.lexer.lex_readline_after_lt() {
                    let end = self.peek_span();
                    Self::readline_expr(content, span.merge(end))
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
                // No more input — break unless the slot is required
                // (in which case we also break, silently; a later
                // semantic pass could error).
                let _ = is_optional;
                break;
            }

            match slot {
                ProtoSlot::Block => {
                    if !self.at(&Token::LeftBrace)? {
                        // Block slot expected a `{`.  For now, stop
                        // parsing args here; a stricter implementation
                        // would error if required.
                        break;
                    }
                    let block = self.parse_block()?;
                    let span = block.span;
                    args.push(Expr { kind: ExprKind::AnonSub(None, None, block), span });
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
                _ => {
                    // Scalar-ish slot: one expression at comma-plus-1
                    // precedence so a comma terminates this arg.
                    let arg = self.parse_expr(PREC_COMMA + 1)?;
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
    fn readline_expr(content: String, span: Span) -> Result<Expr, ParseError> {
        if content.is_empty() {
            Ok(Expr { kind: ExprKind::FuncCall("readline".into(), vec![]), span })
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
                args.push(Expr { span: block.span, kind: ExprKind::AnonSub(None, None, block) });
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
            args.push(Expr { span: block.span, kind: ExprKind::AnonSub(None, None, block) });
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
        // Bareword autoquoting ($hash{key}) handled by parse_ident_term (RightBrace check).
        // -bareword autoquoting ($hash{-key}) handled by parse_term Minus handler.
        self.parse_expr(PREC_LOW)
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
                    self.next_token()?;
                    parts.push(InterpPart::ScalarInterp(name));
                }
                Token::InterpArray(name) => {
                    self.next_token()?;
                    parts.push(InterpPart::ArrayInterp(name));
                }
                Token::InterpScalarExprStart | Token::InterpArrayExprStart => {
                    self.next_token()?;
                    let expr = self.parse_expr(PREC_LOW)?;
                    self.expect_token(&Token::RightBrace)?;
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
        match self.peek_token() {
            Token::OrOr => Some(OpInfo { prec: PREC_OR, assoc: Assoc::Left }),
            Token::DefinedOr => Some(OpInfo { prec: PREC_OR, assoc: Assoc::Left }),
            Token::AndAnd => Some(OpInfo { prec: PREC_AND, assoc: Assoc::Left }),
            Token::BitOr => Some(OpInfo { prec: PREC_BIT_OR, assoc: Assoc::Left }),
            Token::BitXor => Some(OpInfo { prec: PREC_BIT_OR, assoc: Assoc::Left }),
            Token::BitAnd => Some(OpInfo { prec: PREC_BIT_AND, assoc: Assoc::Left }),
            Token::NumEq | Token::NumNe | Token::Spaceship => Some(OpInfo { prec: PREC_EQ, assoc: Assoc::Non }),
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
                if !Self::is_valid_lvalue(&left) {
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
                Ok(Expr { span: left.span.merge(right.span), kind: ExprKind::BinOp(binop, Box::new(left), Box::new(right)) })
            }
        }
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
            // Postfix dereference: ->@*, ->%*, ->$*, ->@[...], ->@{...}
            Token::At => {
                self.next_token()?;
                if self.eat(&Token::Star)? {
                    Ok(Expr { span: left.span.merge(self.peek_span()), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::DerefArray) })
                } else {
                    Err(ParseError::new("expected * after ->@", self.peek_span()))
                }
            }
            Token::Dollar => {
                self.next_token()?;
                if self.eat(&Token::Star)? {
                    Ok(Expr { span: left.span.merge(self.peek_span()), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::DerefScalar) })
                } else {
                    Err(ParseError::new("expected * after ->$", self.peek_span()))
                }
            }
            Token::Percent => {
                self.next_token()?;
                if self.eat(&Token::Star)? {
                    Ok(Expr { span: left.span.merge(self.peek_span()), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::DerefHash) })
                } else {
                    Err(ParseError::new("expected * after ->%", self.peek_span()))
                }
            }
            other => Err(ParseError::new(format!("expected method name or subscript after ->, got {other:?}"), self.peek_span())),
        }
    }
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
        Token::AndAnd => Ok(BinOp::And),
        Token::OrOr => Ok(BinOp::Or),
        Token::DefinedOr => Ok(BinOp::DefinedOr),
        Token::BitAnd => Ok(BinOp::BitAnd),
        Token::BitOr => Ok(BinOp::BitOr),
        Token::BitXor => Ok(BinOp::BitXor),
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
            _ => Err(ParseError::new(format!("not a binary operator: {token:?}"), Span::DUMMY)),
        },
        other => Err(ParseError::new(format!("not a binary operator: {other:?}"), Span::DUMMY)),
    }
}

/// Merge adjacent `Const` segments in an interpolated value.
fn merge_interp_parts(parts: Vec<InterpPart>) -> Vec<InterpPart> {
    let mut merged: Vec<InterpPart> = Vec::new();
    for part in parts {
        if let InterpPart::Const(s) = &part
            && let Some(InterpPart::Const(prev)) = merged.last_mut()
        {
            prev.push_str(s);
            continue;
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
                let var = f.var.as_ref().expect("expected loop variable");
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
                assert!(matches!(&parts[1], InterpPart::ScalarInterp(s) if s == "name"));
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
                assert!(matches!(&parts[0], InterpPart::ScalarInterp(s) if s == "x"));
                assert!(matches!(&parts[1], InterpPart::Const(s) if s == " and "));
                assert!(matches!(&parts[2], InterpPart::ScalarInterp(s) if s == "y"));
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
                assert!(matches!(&parts[1], InterpPart::ArrayInterp(s) if s == "list"));
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
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
            ExprKind::AnonSub(proto, _, body) => {
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
            ExprKind::AnonSub(proto, _, _) => {
                assert!(proto.is_some());
            }
            other => panic!("expected AnonSub, got {other:?}"),
        }
    }

    #[test]
    fn parse_anon_sub_as_arg() {
        let prog = parse("my $f = sub { 1; };");
        let init = decl_init(&prog.statements[0]);
        assert!(matches!(init.kind, ExprKind::AnonSub(_, _, _)));
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
                assert!(matches!(args[0].kind, ExprKind::AnonSub(_, _, _)));
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
                assert!(matches!(args[0].kind, ExprKind::AnonSub(_, _, _)));
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
                assert!(matches!(args[0].kind, ExprKind::AnonSub(_, _, _)));
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
        let prog = parse("given ($x) { when (1) { 1; } default { 0; } }");
        match &prog.statements[0].kind {
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
        let prog = parse("try { die; } catch ($e) { warn $e; }");
        match &prog.statements[0].kind {
            StmtKind::Try(t) => {
                assert!(t.catch_block.is_some());
                assert!(t.catch_var.is_some());
            }
            other => panic!("expected Try, got {other:?}"),
        }
    }

    #[test]
    fn parse_try_catch_finally() {
        let prog = parse("try { 1; } catch ($e) { 2; } finally { 3; }");
        match &prog.statements[0].kind {
            StmtKind::Try(t) => {
                assert!(t.catch_block.is_some());
                assert!(t.finally_block.is_some());
            }
            other => panic!("expected Try, got {other:?}"),
        }
    }

    #[test]
    fn parse_defer() {
        let prog = parse("defer { cleanup(); }");
        match &prog.statements[0].kind {
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
            ExprKind::StringLit(s) => assert_eq!(s, "v5.26"),
            other => panic!("expected StringLit(\"v5.26\"), got {other:?}"),
        }
    }

    #[test]
    fn parse_vstring_no_dots() {
        let e = parse_expr_str("v5;");
        match &e.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "v5"),
            other => panic!("expected StringLit(\"v5\"), got {other:?}"),
        }
    }

    // ── format tests ──────────────────────────────────────────

    #[test]
    fn parse_format_decl() {
        let prog = parse("format STDOUT =\n@<<<< @>>>>\n$name, $value\n.\n");
        match &prog.statements[0].kind {
            StmtKind::FormatDecl(f) => {
                assert_eq!(f.name, "STDOUT");
                assert!(f.body.contains("@<<<<"));
            }
            other => panic!("expected FormatDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_format_default_name() {
        let prog = parse("format =\ntest\n.\n");
        match &prog.statements[0].kind {
            StmtKind::FormatDecl(f) => {
                assert_eq!(f.name, "STDOUT");
            }
            other => panic!("expected FormatDecl, got {other:?}"),
        }
    }

    // ── class/field/method tests ──────────────────────────────

    #[test]
    fn parse_class_decl() {
        let prog = parse("class Foo { field $x; method greet { 1; } }");
        match &prog.statements[0].kind {
            StmtKind::ClassDecl(c) => {
                assert_eq!(c.name, "Foo");
                assert!(c.body.statements.len() >= 2);
            }
            other => panic!("expected ClassDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_class_with_isa() {
        let prog = parse("class Bar :isa(Foo) { }");
        match &prog.statements[0].kind {
            StmtKind::ClassDecl(c) => {
                assert_eq!(c.name, "Bar");
                assert_eq!(c.attributes.len(), 1);
                assert_eq!(c.attributes[0].name, "isa");
            }
            other => panic!("expected ClassDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_field_decl() {
        let prog = parse("class Foo { field $x = 42; }");
        match &prog.statements[0].kind {
            StmtKind::ClassDecl(c) => match &c.body.statements[0].kind {
                StmtKind::FieldDecl(f) => {
                    assert_eq!(f.var.name, "x");
                    assert!(f.default.is_some());
                }
                other => panic!("expected FieldDecl, got {other:?}"),
            },
            other => panic!("expected ClassDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_field_with_param() {
        let prog = parse("class Foo { field $name :param; }");
        match &prog.statements[0].kind {
            StmtKind::ClassDecl(c) => match &c.body.statements[0].kind {
                StmtKind::FieldDecl(f) => {
                    assert_eq!(f.attributes.len(), 1);
                    assert_eq!(f.attributes[0].name, "param");
                }
                other => panic!("expected FieldDecl, got {other:?}"),
            },
            other => panic!("expected ClassDecl, got {other:?}"),
        }
    }

    #[test]
    fn parse_method_decl() {
        let prog = parse("class Foo { method greet() { 1; } }");
        match &prog.statements[0].kind {
            StmtKind::ClassDecl(c) => match &c.body.statements[0].kind {
                StmtKind::MethodDecl(m) => {
                    assert_eq!(m.name, "greet");
                }
                other => panic!("expected MethodDecl, got {other:?}"),
            },
            other => panic!("expected ClassDecl, got {other:?}"),
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

    // ── Dynamic method dispatch tests ─────────────────────────

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
            StmtKind::Block(_) => {}
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn parse_block_at_stmt_level() {
        // {my $x = 1; $x} — clearly a block (no comma/=> after first term).
        let prog = parse("{my $x = 1; $x};");
        match &prog.statements[0].kind {
            StmtKind::Block(block) => {
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
        let prog = parse("try { 1; } finally { 2; }");
        match &prog.statements[0].kind {
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
                assert!(matches!(&pat.0[1], InterpPart::ScalarInterp(s) if s == "bar"));
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
                assert!(matches!(&parts[0], InterpPart::ScalarInterp(s) if s == "name"), "expected ScalarInterp(name), got {:?}", parts[0]);
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
        // $a < $b < $c — chained comparison, valid since 5.32.
        let e = parse_expr_str("$a < $b < $c;");
        // Parses as ($a < $b) < $c (left-assoc); desugaring to
        // $a < $b && $b < $c is a compiler concern.
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::NumLt, _, _)));
    }

    #[test]
    fn allow_chained_eq() {
        let e = parse_expr_str("$a == $b == $c;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::NumEq, _, _)));
    }

    #[test]
    fn allow_chained_str_cmp() {
        let e = parse_expr_str("$a eq $b eq $c;");
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::StrEq, _, _)));
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
                assert!(matches!(args[0].kind, ExprKind::AnonSub(_, _, _)), "expected AnonSub block, got {:?}", args[0].kind);
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
                    ExprKind::AnonSub(_, _, block) => block,
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
            ExprKind::AnonSub(_, _, block) => {
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
                    ExprKind::AnonSub(_, _, b) => b,
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
                assert!(matches!(args[0].kind, ExprKind::AnonSub(_, _, _)), "arg 1 should be AnonSub (block), got {:?}", args[0].kind);
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
}
