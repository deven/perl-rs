//! Pratt parser with recursive descent for statements (§6).
//!
//! Expression assembly uses precedence climbing.  Statements, declarations,
//! blocks, and top-level forms use ordinary recursive descent that calls
//! `parse_expr` where expressions are needed.

use crate::ast::*;
use crate::error::ParseError;
use crate::expect::{BaseExpect, BraceDisposition, Expect};
use crate::keyword;
use crate::lexer::{Lexer, LexerCheckpoint};
use crate::span::Span;
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

#[derive(Clone, Copy, Debug)]
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
pub struct Parser<'src> {
    lexer: Lexer<'src>,
    /// Cached current token with the lexer checkpoint that produced it.
    /// `None` means no token is cached — the next peek/advance will lex.
    /// On rewind, the full lexer state (position, context stack,
    /// heredoc redirects) is restored via the checkpoint.
    current: Option<(Spanned, LexerCheckpoint, Expect)>,
    expect: Expect,
    errors: Vec<ParseError>,
    depth: ParseDepth,
}

impl<'src> Parser<'src> {
    // ── Construction ──────────────────────────────────────────

    pub fn new(src: &'src [u8]) -> Result<Self, ParseError> {
        let lexer = Lexer::new(src);
        Ok(Parser { lexer, current: None, expect: Expect::XSTATE, errors: Vec::new(), depth: 0 })
    }

    // ── Token access ──────────────────────────────────────────

    /// Ensure `self.current` holds a token lexed under the current
    /// expect state.  If the cached token was lexed under a different
    /// expect, restore the full lexer checkpoint and re-lex.
    fn ensure_current(&mut self) {
        match &self.current {
            Some((_, _, cached_expect)) if *cached_expect == self.expect => {
                return;
            }
            Some((_, checkpoint, _)) => {
                let cp = checkpoint.clone();
                self.lexer.restore(cp);
                self.current = None;
            }
            None => {}
        }

        let checkpoint = self.lexer.checkpoint();
        let spanned =
            self.lexer.next_token(&self.expect).unwrap_or(Spanned { token: Token::Eof, span: Span::new(self.lexer.pos() as u32, self.lexer.pos() as u32) });
        self.current = Some((spanned, checkpoint, self.expect));
    }

    fn peek(&mut self) -> &Token {
        self.ensure_current();
        &self.current.as_ref().unwrap().0.token
    }

    fn peek_span(&mut self) -> Span {
        self.ensure_current();
        self.current.as_ref().unwrap().0.span
    }

    fn advance(&mut self) -> Spanned {
        self.ensure_current();
        self.current.take().unwrap().0
    }

    fn expect_token(&mut self, expected: &Token) -> Result<Spanned, ParseError> {
        if self.peek() == expected {
            Ok(self.advance())
        } else {
            let msg = format!("expected {expected}, got {}", self.peek());
            let span = self.peek_span();
            Err(ParseError::new(msg, span))
        }
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == token {
            self.advance();
            true
        } else {
            false
        }
    }

    fn at(&mut self, token: &Token) -> bool {
        self.peek() == token
    }

    fn at_eof(&mut self) -> bool {
        matches!(self.peek(), Token::Eof)
    }

    // ── Depth control ─────────────────────────────────────────

    fn descend(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth >= MAX_DEPTH { Err(ParseError::new("nesting too deep", self.peek_span())) } else { Ok(()) }
    }

    fn ascend(&mut self) {
        self.depth -= 1;
    }

    // ── Public entry point ────────────────────────────────────

    /// Parse a complete program.
    pub fn parse_program(&mut self) -> Result<Program, ParseError> {
        let start = self.peek_span();
        let mut statements = Vec::new();
        self.expect = Expect::XSTATE;

        while !self.at_eof() {
            let stmt = self.parse_statement()?;
            let is_data_end = matches!(stmt.kind, StmtKind::DataEnd);
            statements.push(stmt);
            if is_data_end {
                break; // __END__ / __DATA__ — everything after is not code
            }
        }

        let end = self.peek_span();
        Ok(Program { statements, span: start.merge(end) })
    }

    // ── Statement parsing ─────────────────────────────────────

    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        self.expect = Expect::XSTATE;
        let start = self.peek_span();

        // Empty statement
        if self.eat(&Token::Semi) {
            return Ok(Statement { kind: StmtKind::Empty, span: start });
        }

        // __END__ / __DATA__ — stop parsing immediately
        if matches!(self.peek(), Token::DataEnd) {
            self.advance();
            // Skip all remaining source — everything after is not code.
            self.lexer.skip_to_end();
            return Ok(Statement { kind: StmtKind::DataEnd, span: start.merge(self.peek_span()) });
        }

        let kind = match self.peek().clone() {
            Token::Keyword(Keyword::My) => self.parse_my_decl()?,
            Token::Keyword(Keyword::Our) => self.parse_our_decl()?,
            Token::Keyword(Keyword::Local) => self.parse_local_decl()?,
            Token::Keyword(Keyword::State) => self.parse_state_decl()?,
            Token::Keyword(Keyword::Sub) => {
                // Named sub declaration if an identifier follows.
                // Otherwise fall through to expression (anonymous sub).
                if self.is_named_sub_ahead() { self.parse_sub_decl()? } else { self.parse_expr_statement()? }
            }
            Token::Keyword(Keyword::If) => self.parse_if_stmt()?,
            Token::Keyword(Keyword::Unless) => self.parse_unless_stmt()?,
            Token::Keyword(Keyword::While) => self.parse_while_stmt()?,
            Token::Keyword(Keyword::Until) => self.parse_until_stmt()?,
            Token::Keyword(Keyword::For) | Token::Keyword(Keyword::Foreach) => self.parse_for_stmt()?,
            Token::Keyword(Keyword::Package) => self.parse_package_decl()?,
            Token::Keyword(Keyword::Use) | Token::Keyword(Keyword::No) => self.parse_use_decl()?,

            // Phaser blocks
            Token::Keyword(Keyword::BEGIN) => self.parse_phaser(PhaserKind::Begin)?,
            Token::Keyword(Keyword::END) => self.parse_phaser(PhaserKind::End)?,
            Token::Keyword(Keyword::INIT) => self.parse_phaser(PhaserKind::Init)?,
            Token::Keyword(Keyword::CHECK) => self.parse_phaser(PhaserKind::Check)?,
            Token::Keyword(Keyword::UNITCHECK) => self.parse_phaser(PhaserKind::Unitcheck)?,

            // given/when/default
            Token::Keyword(Keyword::Given) => self.parse_given()?,
            Token::Keyword(Keyword::When) => self.parse_when()?,
            Token::Keyword(Keyword::Default) => {
                self.advance();
                self.expect.brace = BraceDisposition::Block;
                let block = self.parse_block()?;
                StmtKind::When(Expr { kind: ExprKind::IntLit(1), span: start }, block)
            }

            // try/catch/finally/defer
            Token::Keyword(Keyword::Try) => self.parse_try()?,
            Token::Keyword(Keyword::Defer) => {
                self.advance();
                self.expect.brace = BraceDisposition::Block;
                let block = self.parse_block()?;
                StmtKind::Defer(block)
            }

            Token::LBrace => {
                let block = self.parse_block()?;
                StmtKind::Block(block)
            }

            // Check for label: IDENT followed by ':'
            Token::Ident(_) => {
                if self.is_label_ahead() {
                    self.parse_labeled_stmt()?
                } else {
                    self.parse_expr_statement()?
                }
            }

            _ => self.parse_expr_statement()?,
        };

        let end = self.peek_span();
        Ok(Statement { kind, span: start.merge(end) })
    }

    fn maybe_postfix_control(&mut self, expr: Expr) -> Result<StmtKind, ParseError> {
        match self.peek() {
            Token::Keyword(Keyword::If) => {
                self.advance();
                self.expect.base = BaseExpect::Term;
                let cond = self.parse_expr(PREC_LOW)?;
                Ok(StmtKind::Expr(Expr { span: expr.span.merge(cond.span), kind: ExprKind::PostfixControl(PostfixKind::If, Box::new(expr), Box::new(cond)) }))
            }
            Token::Keyword(Keyword::Unless) => {
                self.advance();
                self.expect.base = BaseExpect::Term;
                let cond = self.parse_expr(PREC_LOW)?;
                Ok(StmtKind::Expr(Expr {
                    span: expr.span.merge(cond.span),
                    kind: ExprKind::PostfixControl(PostfixKind::Unless, Box::new(expr), Box::new(cond)),
                }))
            }
            Token::Keyword(Keyword::While) => {
                self.advance();
                self.expect.base = BaseExpect::Term;
                let cond = self.parse_expr(PREC_LOW)?;
                Ok(StmtKind::Expr(Expr {
                    span: expr.span.merge(cond.span),
                    kind: ExprKind::PostfixControl(PostfixKind::While, Box::new(expr), Box::new(cond)),
                }))
            }
            Token::Keyword(Keyword::Until) => {
                self.advance();
                self.expect.base = BaseExpect::Term;
                let cond = self.parse_expr(PREC_LOW)?;
                Ok(StmtKind::Expr(Expr {
                    span: expr.span.merge(cond.span),
                    kind: ExprKind::PostfixControl(PostfixKind::Until, Box::new(expr), Box::new(cond)),
                }))
            }
            Token::Keyword(Keyword::For) | Token::Keyword(Keyword::Foreach) => {
                self.advance();
                self.expect.base = BaseExpect::Term;
                let list = self.parse_expr(PREC_LOW)?;
                Ok(StmtKind::Expr(Expr { span: expr.span.merge(list.span), kind: ExprKind::PostfixControl(PostfixKind::For, Box::new(expr), Box::new(list)) }))
            }
            _ => Ok(StmtKind::Expr(expr)),
        }
    }

    // ── Variable declarations ─────────────────────────────────

    fn parse_var_list(&mut self) -> Result<(Vec<VarDecl>, Option<Expr>), ParseError> {
        let mut vars = Vec::new();

        if self.eat(&Token::LParen) {
            // my ($x, @y, %z)
            loop {
                let decl = self.parse_single_var_decl()?;
                vars.push(decl);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect_token(&Token::RParen)?;
        } else {
            vars.push(self.parse_single_var_decl()?);
        }

        let init = if self.eat(&Token::Assign(AssignOp::Eq)) {
            self.expect.base = BaseExpect::Term;
            Some(self.parse_expr(PREC_ASSIGN)?)
        } else {
            None
        };

        self.eat(&Token::Semi);
        Ok((vars, init))
    }

    fn parse_single_var_decl(&mut self) -> Result<VarDecl, ParseError> {
        let span = self.peek_span();
        match self.advance().token {
            Token::ScalarVar(name) => Ok(VarDecl { sigil: Sigil::Scalar, name, span }),
            Token::ArrayVar(name) => Ok(VarDecl { sigil: Sigil::Array, name, span }),
            Token::HashVar(name) => Ok(VarDecl { sigil: Sigil::Hash, name, span }),
            other => Err(ParseError::new(format!("expected variable, got {other:?}"), span)),
        }
    }

    fn parse_my_decl(&mut self) -> Result<StmtKind, ParseError> {
        self.advance(); // eat 'my'
        let (vars, init) = self.parse_var_list()?;
        Ok(StmtKind::My(vars, init))
    }

    fn parse_our_decl(&mut self) -> Result<StmtKind, ParseError> {
        self.advance();
        let (vars, init) = self.parse_var_list()?;
        Ok(StmtKind::Our(vars, init))
    }

    fn parse_local_decl(&mut self) -> Result<StmtKind, ParseError> {
        self.advance();
        let (vars, init) = self.parse_var_list()?;
        Ok(StmtKind::Local(vars, init))
    }

    fn parse_state_decl(&mut self) -> Result<StmtKind, ParseError> {
        self.advance();
        let (vars, init) = self.parse_var_list()?;
        Ok(StmtKind::State(vars, init))
    }

    // ── Sub declaration ───────────────────────────────────────

    fn parse_sub_decl(&mut self) -> Result<StmtKind, ParseError> {
        let start = self.peek_span();
        self.advance(); // eat 'sub'
        let name = match self.advance().token {
            Token::Ident(name) => name,
            other => return Err(ParseError::new(format!("expected sub name, got {other:?}"), start)),
        };

        // Optional prototype
        let prototype = if self.at(&Token::LParen) {
            self.advance();
            let mut proto = String::new();
            while !self.at(&Token::RParen) && !self.at_eof() {
                let t = self.advance();
                proto.push_str(&format!("{}", t.token));
            }
            self.expect_token(&Token::RParen)?;
            Some(proto)
        } else {
            None
        };

        let body = self.parse_block()?;

        Ok(StmtKind::SubDecl(SubDecl { name, prototype, attributes: Vec::new(), params: None, body, span: start.merge(self.peek_span()) }))
    }

    /// Parse a declaration in expression context: `my $x`, `our ($a, @b)`, etc.
    /// Returns a Decl expression; the Pratt parser handles `= expr` as assignment.
    fn parse_decl_expr(&mut self, scope: DeclScope, span: Span) -> Result<Expr, ParseError> {
        let mut vars = Vec::new();

        if self.at(&Token::LParen) {
            // List form: my ($x, @y, %z)
            self.advance();
            while !self.at(&Token::RParen) && !self.at_eof() {
                vars.push(self.parse_single_var_decl()?);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            let end = self.peek_span();
            self.expect_token(&Token::RParen)?;
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
        // Optional prototype
        let prototype = if self.at(&Token::LParen) {
            self.advance();
            let mut proto = String::new();
            while !self.at(&Token::RParen) && !self.at_eof() {
                let t = self.advance();
                proto.push_str(&format!("{}", t.token));
            }
            self.expect_token(&Token::RParen)?;
            Some(proto)
        } else {
            None
        };

        self.expect.brace = BraceDisposition::BlockExpr;
        let body = self.parse_block()?;

        Ok(Expr { span: span.merge(body.span), kind: ExprKind::AnonSub(prototype, None, body) })
    }

    // ── Control flow statements ───────────────────────────────

    fn parse_if_stmt(&mut self) -> Result<StmtKind, ParseError> {
        self.advance(); // eat 'if'
        let condition = self.parse_paren_expr()?;
        self.expect.brace = BraceDisposition::Block;
        let then_block = self.parse_block()?;

        let mut elsif_clauses = Vec::new();
        while self.eat(&Token::Keyword(Keyword::Elsif)) {
            let cond = self.parse_paren_expr()?;
            self.expect.brace = BraceDisposition::Block;
            let block = self.parse_block()?;
            elsif_clauses.push((cond, block));
        }

        let else_block = if self.eat(&Token::Keyword(Keyword::Else)) {
            self.expect.brace = BraceDisposition::Block;
            Some(self.parse_block()?)
        } else {
            None
        };

        Ok(StmtKind::If(IfStmt { condition, then_block, elsif_clauses, else_block }))
    }

    fn parse_unless_stmt(&mut self) -> Result<StmtKind, ParseError> {
        self.advance();
        let condition = self.parse_paren_expr()?;
        self.expect.brace = BraceDisposition::Block;
        let then_block = self.parse_block()?;

        let mut elsif_clauses = Vec::new();
        while self.eat(&Token::Keyword(Keyword::Elsif)) {
            let cond = self.parse_paren_expr()?;
            self.expect.brace = BraceDisposition::Block;
            let block = self.parse_block()?;
            elsif_clauses.push((cond, block));
        }

        let else_block = if self.eat(&Token::Keyword(Keyword::Else)) {
            self.expect.brace = BraceDisposition::Block;
            Some(self.parse_block()?)
        } else {
            None
        };
        Ok(StmtKind::Unless(UnlessStmt { condition, then_block, elsif_clauses, else_block }))
    }

    fn parse_while_stmt(&mut self) -> Result<StmtKind, ParseError> {
        self.advance();
        let condition = self.parse_paren_expr()?;
        self.expect.brace = BraceDisposition::Block;
        let body = self.parse_block()?;
        let continue_block = if self.eat(&Token::Keyword(Keyword::Continue)) { Some(self.parse_block()?) } else { None };
        Ok(StmtKind::While(WhileStmt { condition, body, continue_block }))
    }

    fn parse_until_stmt(&mut self) -> Result<StmtKind, ParseError> {
        self.advance();
        let condition = self.parse_paren_expr()?;
        self.expect.brace = BraceDisposition::Block;
        let body = self.parse_block()?;
        let continue_block = if self.eat(&Token::Keyword(Keyword::Continue)) { Some(self.parse_block()?) } else { None };
        Ok(StmtKind::Until(UntilStmt { condition, body, continue_block }))
    }

    fn parse_for_stmt(&mut self) -> Result<StmtKind, ParseError> {
        self.advance(); // eat for/foreach

        // If next is a variable or 'my', it's foreach-style
        if matches!(self.peek(), Token::Keyword(Keyword::My) | Token::ScalarVar(_)) {
            return self.parse_foreach_body();
        }

        // If next is '(' we need to distinguish C-style from foreach.
        // C-style: for (init; cond; step) { ... }
        // Foreach: for (LIST) { ... }
        // Heuristic: scan inside parens for a semicolon at depth 0.
        if self.at(&Token::LParen) {
            if self.is_c_style_for() {
                return self.parse_c_style_for();
            }
        }

        // Foreach-style with bare list
        let list = self.parse_paren_expr()?;
        self.expect.brace = BraceDisposition::Block;
        let body = self.parse_block()?;
        let continue_block = if self.eat(&Token::Keyword(Keyword::Continue)) {
            self.expect.brace = BraceDisposition::Block;
            Some(self.parse_block()?)
        } else {
            None
        };

        Ok(StmtKind::ForEach(ForEachStmt { var: None, list, body, continue_block }))
    }

    /// Lookahead: is this `for (init; cond; step)` (C-style)?
    /// Scans inside parens looking for a semicolon at paren depth 0.
    fn is_c_style_for(&mut self) -> bool {
        let cp = self.lexer.checkpoint();
        let saved_expect = self.expect;
        let saved_current = self.current.take();

        // We're looking at '('. Skip it.
        self.ensure_current();
        let _ = self.current.take();

        let mut depth = 1u32;
        let mut found_semi = false;
        loop {
            self.ensure_current();
            match self.peek() {
                Token::LParen => {
                    depth += 1;
                }
                Token::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Token::Semi => {
                    if depth == 1 {
                        found_semi = true;
                        break;
                    }
                }
                Token::Eof => break,
                _ => {}
            }
            let _ = self.current.take();
        }

        self.current = saved_current;
        self.expect = saved_expect;
        self.lexer.restore(cp);
        found_semi
    }

    /// Parse C-style: `for (init; cond; step) { ... }`
    fn parse_c_style_for(&mut self) -> Result<StmtKind, ParseError> {
        self.expect_token(&Token::LParen)?;
        self.expect.base = BaseExpect::Term;

        // init (may be empty)
        let init = if self.at(&Token::Semi) {
            None
        } else {
            self.expect.base = BaseExpect::Term;
            Some(self.parse_expr(PREC_LOW)?)
        };
        self.expect_token(&Token::Semi)?;

        // condition (may be empty)
        self.expect.base = BaseExpect::Term;
        let condition = if self.at(&Token::Semi) { None } else { Some(self.parse_expr(PREC_LOW)?) };
        self.expect_token(&Token::Semi)?;

        // step (may be empty)
        self.expect.base = BaseExpect::Term;
        let step = if self.at(&Token::RParen) { None } else { Some(self.parse_expr(PREC_LOW)?) };
        self.expect_token(&Token::RParen)?;

        self.expect.brace = BraceDisposition::Block;
        let body = self.parse_block()?;

        Ok(StmtKind::For(ForStmt { init, condition, step, body }))
    }

    fn parse_foreach_body(&mut self) -> Result<StmtKind, ParseError> {
        let var = if self.eat(&Token::Keyword(Keyword::My)) {
            Some(self.parse_single_var_decl()?)
        } else if matches!(self.peek(), Token::ScalarVar(_)) {
            let span = self.peek_span();
            let name = match self.advance().token {
                Token::ScalarVar(n) => n,
                _ => unreachable!(),
            };
            Some(VarDecl { sigil: Sigil::Scalar, name, span })
        } else {
            None
        };

        let list = self.parse_paren_expr()?;
        self.expect.brace = BraceDisposition::Block;
        let body = self.parse_block()?;
        let continue_block = if self.eat(&Token::Keyword(Keyword::Continue)) {
            self.expect.brace = BraceDisposition::Block;
            Some(self.parse_block()?)
        } else {
            None
        };

        Ok(StmtKind::ForEach(ForEachStmt { var, list, body, continue_block }))
    }

    // ── Package and use ───────────────────────────────────────

    fn parse_package_decl(&mut self) -> Result<StmtKind, ParseError> {
        let start = self.peek_span();
        self.advance(); // eat 'package'
        let name = match self.advance().token {
            Token::Ident(n) => n,
            other => return Err(ParseError::new(format!("expected package name, got {other:?}"), start)),
        };

        // Optional version
        let version =
            if matches!(self.peek(), Token::IntLit(_) | Token::FloatLit(_) | Token::VersionLit(_)) { Some(format!("{}", self.advance().token)) } else { None };

        let block = if self.at(&Token::LBrace) {
            Some(self.parse_block()?)
        } else {
            self.eat(&Token::Semi);
            None
        };

        Ok(StmtKind::PackageDecl(PackageDecl { name, version, block, span: start.merge(self.peek_span()) }))
    }

    fn parse_use_decl(&mut self) -> Result<StmtKind, ParseError> {
        let start = self.peek_span();
        let is_no = matches!(self.peek(), Token::Keyword(Keyword::No));
        self.advance(); // eat 'use'/'no'

        let module = match self.advance().token {
            Token::Ident(n) => n,
            Token::IntLit(n) => format!("{n}"),
            Token::FloatLit(n) => format!("{n}"),
            other => return Err(ParseError::new(format!("expected module name, got {other:?}"), start)),
        };

        // Optional version and import list
        let version = None; // simplified for bootstrap
        let imports = None;

        self.eat(&Token::Semi);

        Ok(StmtKind::UseDecl(UseDecl { is_no, module, version, imports, span: start.merge(self.peek_span()) }))
    }

    // ── Phaser blocks ─────────────────────────────────────────

    fn parse_phaser(&mut self, kind: PhaserKind) -> Result<StmtKind, ParseError> {
        self.advance(); // eat the phaser keyword
        self.expect.brace = BraceDisposition::Block;
        let block = self.parse_block()?;
        Ok(StmtKind::Phaser(kind, block))
    }

    // ── given/when ────────────────────────────────────────────

    fn parse_given(&mut self) -> Result<StmtKind, ParseError> {
        self.advance(); // eat 'given'
        let expr = self.parse_paren_expr()?;
        self.expect.brace = BraceDisposition::Block;
        let block = self.parse_block()?;
        Ok(StmtKind::Given(expr, block))
    }

    fn parse_when(&mut self) -> Result<StmtKind, ParseError> {
        self.advance(); // eat 'when'
        let expr = self.parse_paren_expr()?;
        self.expect.brace = BraceDisposition::Block;
        let block = self.parse_block()?;
        Ok(StmtKind::When(expr, block))
    }

    // ── try/catch/finally ─────────────────────────────────────

    fn parse_try(&mut self) -> Result<StmtKind, ParseError> {
        self.advance(); // eat 'try'
        self.expect.brace = BraceDisposition::Block;
        let body = self.parse_block()?;

        let (catch_var, catch_block) = if self.eat(&Token::Keyword(Keyword::Catch)) {
            let var = if self.at(&Token::LParen) {
                self.advance();
                let decl = self.parse_single_var_decl()?;
                self.expect_token(&Token::RParen)?;
                Some(decl)
            } else {
                None
            };
            self.expect.brace = BraceDisposition::Block;
            let block = self.parse_block()?;
            (var, Some(block))
        } else {
            (None, None)
        };

        let finally_block = if self.eat(&Token::Keyword(Keyword::Finally)) {
            self.expect.brace = BraceDisposition::Block;
            Some(self.parse_block()?)
        } else {
            None
        };

        Ok(StmtKind::Try(TryStmt { body, catch_var, catch_block, finally_block }))
    }

    // ── Labels ────────────────────────────────────────────────

    /// Check if we're looking at `IDENT :` (a label).
    fn is_label_ahead(&mut self) -> bool {
        let cp = self.lexer.checkpoint();
        let saved_expect = self.expect;
        let saved_current = self.current.take();

        // Lexer is already past the Ident (it was cached).
        // Lex the next token and check if it's a colon.
        self.ensure_current();
        let is_colon = matches!(self.peek(), Token::Colon);

        self.current = saved_current;
        self.expect = saved_expect;
        self.lexer.restore(cp);
        is_colon
    }

    /// Check if `sub` is followed by an identifier (named sub decl).
    fn is_named_sub_ahead(&mut self) -> bool {
        let cp = self.lexer.checkpoint();
        let saved_expect = self.expect;
        let saved_current = self.current.take();

        // Lexer is already past `sub` (it was cached).
        // Lex the next token and check if it's an identifier.
        self.ensure_current();
        let is_ident = matches!(self.peek(), Token::Ident(_));

        self.current = saved_current;
        self.expect = saved_expect;
        self.lexer.restore(cp);
        is_ident
    }

    fn parse_labeled_stmt(&mut self) -> Result<StmtKind, ParseError> {
        let label = match self.advance().token {
            Token::Ident(name) => name,
            _ => unreachable!(),
        };
        self.expect_token(&Token::Colon)?; // eat ':'
        let stmt = self.parse_statement()?;
        Ok(StmtKind::Labeled(label, Box::new(stmt)))
    }

    // ── Expression statements ─────────────────────────────────

    fn parse_expr_statement(&mut self) -> Result<StmtKind, ParseError> {
        self.expect.base = BaseExpect::Term;
        let expr = self.parse_expr(PREC_LOW)?;

        // Check for postfix control flow
        let kind = self.maybe_postfix_control(expr)?;

        self.eat(&Token::Semi);
        Ok(kind)
    }

    // ── Block parsing ─────────────────────────────────────────

    fn parse_block(&mut self) -> Result<Block, ParseError> {
        self.descend()?;
        let start = self.peek_span();
        self.expect_token(&Token::LBrace)?;
        self.expect = Expect::XSTATE;

        let mut statements = Vec::new();
        while !self.at(&Token::RBrace) && !self.at_eof() {
            statements.push(self.parse_statement()?);
        }

        let end = self.peek_span();
        self.expect_token(&Token::RBrace)?;
        self.expect = Expect::XSTATE;
        self.ascend();

        Ok(Block { statements, span: start.merge(end) })
    }

    fn parse_paren_expr(&mut self) -> Result<Expr, ParseError> {
        self.expect_token(&Token::LParen)?;
        self.expect.base = BaseExpect::Term;
        let expr = self.parse_expr(PREC_LOW)?;
        self.expect_token(&Token::RParen)?;
        Ok(expr)
    }

    // ── Expression parsing (Pratt) ────────────────────────────

    fn parse_expr(&mut self, min_prec: Precedence) -> Result<Expr, ParseError> {
        self.descend()?;
        self.expect.base = BaseExpect::Term;

        let mut left = self.parse_term()?;

        // After a term, expect an operator
        self.expect.base = BaseExpect::Operator;

        while let Some(info) = self.peek_op_info() {
            if info.left_prec() < min_prec {
                break;
            }
            left = self.parse_operator(left, info)?;
            self.expect.base = BaseExpect::Operator;
        }

        self.ascend();
        Ok(left)
    }

    // ── Term parsing ──────────────────────────────────────────

    fn parse_term(&mut self) -> Result<Expr, ParseError> {
        let spanned = self.advance();
        let span = spanned.span;

        match spanned.token {
            Token::IntLit(n) => Ok(Expr { kind: ExprKind::IntLit(n), span }),
            Token::FloatLit(n) => Ok(Expr { kind: ExprKind::FloatLit(n), span }),
            Token::StrLit(s) => Ok(Expr { kind: ExprKind::StringLit(s), span }),

            // Interpolating string: collect sub-tokens into AST.
            Token::QuoteBegin(_, _) => self.parse_interpolated_string(span),

            Token::ScalarVar(name) => {
                let expr = Expr { kind: ExprKind::ScalarVar(name), span };
                self.maybe_postfix_subscript(expr)
            }
            Token::ArrayVar(name) => {
                // @array[0,1] → array slice; @array{qw(a b)} → hash slice
                if self.at(&Token::LBracket) {
                    self.advance();
                    self.expect.base = BaseExpect::Term;
                    let mut indices = Vec::new();
                    while !self.at(&Token::RBracket) && !self.at_eof() {
                        indices.push(self.parse_expr(PREC_COMMA + 1)?);
                        if !self.eat(&Token::Comma) {
                            break;
                        }
                    }
                    let end = self.peek_span();
                    self.expect_token(&Token::RBracket)?;
                    Ok(Expr { span: span.merge(end), kind: ExprKind::ArraySlice(Box::new(Expr { kind: ExprKind::ArrayVar(name), span }), indices) })
                } else if self.at(&Token::LBrace) {
                    self.advance();
                    self.expect.base = BaseExpect::Term;
                    let mut keys = Vec::new();
                    while !self.at(&Token::RBrace) && !self.at_eof() {
                        keys.push(self.parse_expr(PREC_COMMA + 1)?);
                        if !self.eat(&Token::Comma) {
                            break;
                        }
                    }
                    let end = self.peek_span();
                    self.expect_token(&Token::RBrace)?;
                    Ok(Expr { span: span.merge(end), kind: ExprKind::HashSlice(Box::new(Expr { kind: ExprKind::ArrayVar(name), span }), keys) })
                } else {
                    Ok(Expr { kind: ExprKind::ArrayVar(name), span })
                }
            }
            Token::HashVar(name) => Ok(Expr { kind: ExprKind::HashVar(name), span }),
            Token::GlobVar(name) => Ok(Expr { kind: ExprKind::GlobVar(name), span }),
            Token::ArrayLen(name) => Ok(Expr { kind: ExprKind::ArrayLen(name), span }),
            Token::SpecialVar(name) => Ok(Expr { kind: ExprKind::SpecialVar(name), span }),

            // Prefix dereference: $$ref, @$ref, %$ref, ${expr}, @{expr}
            Token::Dollar => {
                self.expect.base = BaseExpect::Term;
                if self.at(&Token::LBrace) {
                    // ${expr} — dereference block
                    self.advance();
                    let inner = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RBrace)?;
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
                self.expect.base = BaseExpect::Term;
                if self.at(&Token::LBrace) {
                    // @{expr} — array dereference block
                    self.advance();
                    let inner = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RBrace)?;
                    Ok(Expr { span: span.merge(end), kind: ExprKind::Deref(Sigil::Array, Box::new(inner)) })
                } else {
                    let operand = self.parse_deref_operand()?;
                    Ok(Expr { span: span.merge(operand.span), kind: ExprKind::Deref(Sigil::Array, Box::new(operand)) })
                }
            }

            // Prefix hash dereference: %$ref, %{expr}
            Token::Percent => {
                self.expect.base = BaseExpect::Term;
                if self.at(&Token::LBrace) {
                    self.advance();
                    let inner = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RBrace)?;
                    Ok(Expr { span: span.merge(end), kind: ExprKind::Deref(Sigil::Hash, Box::new(inner)) })
                } else {
                    let operand = self.parse_deref_operand()?;
                    Ok(Expr { span: span.merge(operand.span), kind: ExprKind::Deref(Sigil::Hash, Box::new(operand)) })
                }
            }

            // Ampersand prefix: &foo, &foo(args), &$coderef(args), &{expr}(args)
            Token::BitAnd => {
                self.expect.base = BaseExpect::Term;
                if self.at(&Token::LBrace) {
                    // &{expr}
                    self.advance();
                    let inner = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RBrace)?;
                    let deref = Expr { span: span.merge(end), kind: ExprKind::Deref(Sigil::Code, Box::new(inner)) };
                    self.maybe_call_args(deref)
                } else if let Token::Ident(_) = self.peek() {
                    // &foo or &foo(args)
                    let name_span = self.peek_span();
                    let name = match self.advance().token {
                        Token::Ident(s) => s,
                        _ => unreachable!(),
                    };
                    if self.at(&Token::LParen) {
                        self.advance();
                        self.expect.base = BaseExpect::Term;
                        let mut args = Vec::new();
                        while !self.at(&Token::RParen) && !self.at_eof() {
                            args.push(self.parse_expr(PREC_COMMA + 1)?);
                            if !self.eat(&Token::Comma) {
                                break;
                            }
                        }
                        let end = self.peek_span();
                        self.expect_token(&Token::RParen)?;
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
                self.expect.base = BaseExpect::Term;
                if self.at(&Token::LBrace) {
                    self.advance();
                    let inner = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RBrace)?;
                    Ok(Expr { span: span.merge(end), kind: ExprKind::Deref(Sigil::Glob, Box::new(inner)) })
                } else if let Token::Ident(_) = self.peek() {
                    let name_span = self.peek_span();
                    let name = match self.advance().token {
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
                self.expect.base = BaseExpect::Term;
                let operand = self.parse_expr(PREC_UNARY)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::UnaryOp(UnaryOp::Negate, Box::new(operand)) })
            }
            Token::Plus => {
                self.expect.base = BaseExpect::Term;
                let operand = self.parse_expr(PREC_UNARY)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::UnaryOp(UnaryOp::NumPositive, Box::new(operand)) })
            }
            Token::Bang => {
                self.expect.base = BaseExpect::Term;
                let operand = self.parse_expr(PREC_UNARY)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::UnaryOp(UnaryOp::LogNot, Box::new(operand)) })
            }
            Token::Tilde => {
                self.expect.base = BaseExpect::Term;
                let operand = self.parse_expr(PREC_UNARY)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::UnaryOp(UnaryOp::BitNot, Box::new(operand)) })
            }
            Token::Backslash => {
                self.expect.base = BaseExpect::Term;
                let operand = self.parse_expr(PREC_UNARY)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::Ref(Box::new(operand)) })
            }
            Token::Not | Token::Keyword(Keyword::Not) => {
                self.expect.base = BaseExpect::Term;
                let operand = self.parse_expr(PREC_NOT_LOW)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::UnaryOp(UnaryOp::Not, Box::new(operand)) })
            }
            Token::PlusPlus => {
                self.expect.base = BaseExpect::Term;
                let operand = self.parse_expr(PREC_INC)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::UnaryOp(UnaryOp::PreInc, Box::new(operand)) })
            }
            Token::MinusMinus => {
                self.expect.base = BaseExpect::Term;
                let operand = self.parse_expr(PREC_INC)?;
                Ok(Expr { span: span.merge(operand.span), kind: ExprKind::UnaryOp(UnaryOp::PreDec, Box::new(operand)) })
            }

            // Declaration in expression context: my $x, our ($a, $b), etc.
            Token::Keyword(Keyword::My) | Token::Keyword(Keyword::Our) | Token::Keyword(Keyword::Local) | Token::Keyword(Keyword::State) => {
                let scope = match spanned.token {
                    Token::Keyword(Keyword::My) => DeclScope::My,
                    Token::Keyword(Keyword::Our) => DeclScope::Our,
                    Token::Keyword(Keyword::Local) => DeclScope::Local,
                    Token::Keyword(Keyword::State) => DeclScope::State,
                    _ => unreachable!(),
                };
                self.parse_decl_expr(scope, span)
            }

            // Anonymous sub: sub { ... } or sub ($x) { ... }
            Token::Keyword(Keyword::Sub) => self.parse_anon_sub(span),

            // eval BLOCK vs eval EXPR
            Token::Keyword(Keyword::Eval) => {
                self.expect.base = BaseExpect::Term;
                if self.at(&Token::LBrace) {
                    self.expect.brace = BraceDisposition::BlockExpr;
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
                self.expect.base = BaseExpect::Term;
                if self.at(&Token::Semi) || self.at(&Token::RBrace) || self.at_eof() {
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
                self.expect.base = BaseExpect::Term;
                // Optional label argument
                if let Token::Ident(_) = self.peek() {
                    let label_span = self.peek_span();
                    let label = match self.advance().token {
                        Token::Ident(s) => s,
                        _ => unreachable!(),
                    };
                    let end = span.merge(label_span);
                    Ok(Expr { kind: ExprKind::FuncCall(name.into(), vec![Expr { kind: ExprKind::StringLit(label), span: label_span }]), span: end })
                } else {
                    Ok(Expr { kind: ExprKind::FuncCall(name.into(), vec![]), span })
                }
            }

            // Named unary keywords
            Token::Keyword(kw) if keyword::is_named_unary(kw) => self.parse_named_unary(kw, span),

            // List operators
            Token::Keyword(kw) if keyword::is_list_op(kw) => self.parse_list_op(kw, span),

            // Parenthesized expression or list
            Token::LParen => {
                self.expect.base = BaseExpect::Term;
                if self.at(&Token::RParen) {
                    self.advance();
                    Ok(Expr { kind: ExprKind::List(vec![]), span })
                } else {
                    let inner = self.parse_expr(PREC_LOW)?;
                    let end = self.peek_span();
                    self.expect_token(&Token::RParen)?;
                    Ok(Expr { kind: ExprKind::Paren(Box::new(inner)), span: span.merge(end) })
                }
            }

            // Anonymous array ref [...]
            Token::LBracket => {
                self.expect.base = BaseExpect::Term;
                let mut elems = Vec::new();
                while !self.at(&Token::RBracket) && !self.at_eof() {
                    elems.push(self.parse_expr(PREC_COMMA + 1)?);
                    if !self.eat(&Token::Comma) {
                        break;
                    }
                }
                let end = self.peek_span();
                self.expect_token(&Token::RBracket)?;
                Ok(Expr { kind: ExprKind::AnonArray(elems), span: span.merge(end) })
            }

            // Anonymous hash ref or block — depends on context
            // For now, treat as hash in expression context
            Token::LBrace => {
                self.expect.base = BaseExpect::Term;
                let mut elems = Vec::new();
                while !self.at(&Token::RBrace) && !self.at_eof() {
                    elems.push(self.parse_expr(PREC_COMMA + 1)?);
                    if !self.eat(&Token::Comma) && !self.eat(&Token::FatComma) {
                        break;
                    }
                }
                let end = self.peek_span();
                self.expect_token(&Token::RBrace)?;
                Ok(Expr { kind: ExprKind::AnonHash(elems), span: span.merge(end) })
            }

            Token::QwList(words) => Ok(Expr { kind: ExprKind::QwList(words), span }),

            // Regex, substitution, transliteration
            Token::RegexLit(_kind, pattern, flags) => Ok(Expr { kind: ExprKind::Regex(pattern, flags), span }),
            Token::SubstLit(pattern, replacement, flags) => Ok(Expr { kind: ExprKind::Subst(pattern, SubstReplacement::Literal(replacement), flags), span }),
            Token::TranslitLit(from, to, flags) => Ok(Expr { kind: ExprKind::Translit(from, to, flags), span }),

            // Heredoc (body already collected by lexer).
            Token::HeredocLit(kind, _tag, body) => {
                match kind {
                    HeredocKind::Literal | HeredocKind::IndentedLiteral => Ok(Expr { kind: ExprKind::StringLit(body), span }),
                    HeredocKind::Interpolating | HeredocKind::Indented => {
                        // TODO: process interpolation in body.
                        // For now, treat as plain string.
                        Ok(Expr { kind: ExprKind::StringLit(body), span })
                    }
                }
            }

            // sort/map/grep with optional block
            Token::Keyword(kw) if keyword::is_block_list_op(kw) => self.parse_block_list_op(kw, span),

            // print/say with optional filehandle
            Token::Keyword(kw) if keyword::is_print_op(kw) => self.parse_print_op(kw, span),

            // goto LABEL, goto &sub, goto EXPR
            Token::Keyword(Keyword::Goto) => {
                self.expect.base = BaseExpect::Term;
                let arg = self.parse_expr(PREC_COMMA)?;
                let end = span.merge(arg.span);
                Ok(Expr { kind: ExprKind::FuncCall("goto".into(), vec![arg]), span: end })
            }

            // Yada yada yada (...)
            Token::DotDotDot => Ok(Expr { kind: ExprKind::Todo("...".into()), span }),

            // __END__ / __DATA__
            Token::DataEnd => Ok(Expr { kind: ExprKind::Todo("__END__".into()), span }),

            // Readline / diamond: <STDIN>, <>, <$fh>, <*.txt>
            Token::Readline(content) => {
                if content.is_empty() {
                    // <> — diamond operator, reads from ARGV
                    Ok(Expr { kind: ExprKind::FuncCall("readline".into(), vec![]), span })
                } else if content.contains('*') || content.contains('?') {
                    // <*.txt> — glob
                    Ok(Expr { kind: ExprKind::FuncCall("glob".into(), vec![Expr { kind: ExprKind::StringLit(content), span }]), span })
                } else {
                    // <STDIN>, <$fh> — readline
                    Ok(Expr { kind: ExprKind::FuncCall("readline".into(), vec![Expr { kind: ExprKind::StringLit(content), span }]), span })
                }
            }

            Token::Keyword(Keyword::Do) => {
                if self.at(&Token::LBrace) {
                    let block = self.parse_block()?;
                    Ok(Expr { span: span.merge(block.span), kind: ExprKind::DoBlock(block) })
                } else {
                    self.expect.base = BaseExpect::Term;
                    let arg = self.parse_expr(PREC_UNARY)?;
                    Ok(Expr { span: span.merge(arg.span), kind: ExprKind::DoExpr(Box::new(arg)) })
                }
            }

            other => Err(ParseError::new(format!("expected expression, got {other:?}"), span)),
        }
    }

    fn parse_ident_term(&mut self, name: String, span: Span) -> Result<Expr, ParseError> {
        // Check if followed by `=>` — fat comma autoquoting
        if matches!(self.peek(), Token::FatComma) {
            return Ok(Expr { kind: ExprKind::StringLit(name), span });
        }

        // Check if followed by `(` — function call
        if self.at(&Token::LParen) {
            self.advance();
            self.expect.base = BaseExpect::Term;
            let mut args = Vec::new();
            while !self.at(&Token::RParen) && !self.at_eof() {
                args.push(self.parse_expr(PREC_COMMA + 1)?);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            let end = self.peek_span();
            self.expect_token(&Token::RParen)?;
            return Ok(Expr { kind: ExprKind::FuncCall(name, args), span: span.merge(end) });
        }

        // Bare identifier
        Ok(Expr { kind: ExprKind::FuncCall(name, vec![]), span })
    }

    fn parse_named_unary(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let name = format!("{kw:?}").to_lowercase();
        self.expect.base = BaseExpect::Term;

        // Named unary with optional arg
        if self.at(&Token::Semi) || self.at_eof() || self.at(&Token::RBrace) || self.at(&Token::RParen) {
            // No argument
            return Ok(Expr { kind: ExprKind::FuncCall(name, vec![]), span });
        }

        if self.at(&Token::LParen) {
            self.advance();
            let arg = self.parse_expr(PREC_LOW)?;
            let end = self.peek_span();
            self.expect_token(&Token::RParen)?;
            return Ok(Expr { kind: ExprKind::FuncCall(name, vec![arg]), span: span.merge(end) });
        }

        // Parse one term as the argument
        let arg = self.parse_expr(PREC_COMMA)?;
        let end = span.merge(arg.span);
        Ok(Expr { kind: ExprKind::FuncCall(name, vec![arg]), span: end })
    }

    fn parse_list_op(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let name = format!("{kw:?}").to_lowercase();
        self.expect.base = BaseExpect::Term;

        // Check for parens
        if self.at(&Token::LParen) {
            self.advance();
            let mut args = Vec::new();
            while !self.at(&Token::RParen) && !self.at_eof() {
                args.push(self.parse_expr(PREC_COMMA + 1)?);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            let end = self.peek_span();
            self.expect_token(&Token::RParen)?;
            return Ok(Expr { kind: ExprKind::ListOp(name, args), span: span.merge(end) });
        }

        // No parens — parse everything up to end of statement as args
        let mut args = Vec::new();
        while !self.at(&Token::Semi) && !self.at_eof() && !self.at(&Token::RBrace) {
            // Check for postfix control keywords
            if matches!(
                self.peek(),
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
            if !self.eat(&Token::Comma) {
                break;
            }
        }

        let end_span = args.last().map(|a| a.span).unwrap_or(span);
        Ok(Expr { kind: ExprKind::ListOp(name, args), span: span.merge(end_span) })
    }

    /// Parse sort/map/grep with optional block as first argument.
    /// `sort { $a <=> $b } @list`, `map { ... } @list`, `grep { ... } @list`
    fn parse_block_list_op(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let name = format!("{kw:?}").to_lowercase();
        self.expect.base = BaseExpect::Term;

        // Check for parens: sort(...), map(...), grep(...)
        if self.at(&Token::LParen) {
            self.advance();
            let mut args = Vec::new();
            // Check for block as first arg inside parens
            if self.at(&Token::LBrace) {
                self.expect.brace = BraceDisposition::BlockExpr;
                let block = self.parse_block()?;
                args.push(Expr { span: block.span, kind: ExprKind::AnonSub(None, None, block) });
                self.eat(&Token::Comma);
            }
            while !self.at(&Token::RParen) && !self.at_eof() {
                args.push(self.parse_expr(PREC_COMMA + 1)?);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            let end = self.peek_span();
            self.expect_token(&Token::RParen)?;
            return Ok(Expr { kind: ExprKind::ListOp(name, args), span: span.merge(end) });
        }

        let mut args = Vec::new();

        // Check for block or sub name as first arg
        if self.at(&Token::LBrace) {
            self.expect.brace = BraceDisposition::BlockExpr;
            let block = self.parse_block()?;
            args.push(Expr { span: block.span, kind: ExprKind::AnonSub(None, None, block) });
        } else if kw == Keyword::Sort {
            // sort can also take a sub name: sort subname @list
            if let Token::Ident(_) = self.peek() {
                let ident_span = self.peek_span();
                let ident = match self.advance().token {
                    Token::Ident(s) => s,
                    _ => unreachable!(),
                };
                args.push(Expr { kind: ExprKind::FuncCall(ident, vec![]), span: ident_span });
            }
        }

        // Rest of arguments
        while !self.at(&Token::Semi) && !self.at_eof() && !self.at(&Token::RBrace) && !self.at(&Token::RParen) {
            if matches!(
                self.peek(),
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
            if !self.eat(&Token::Comma) {
                break;
            }
        }

        let end_span = args.last().map(|a| a.span).unwrap_or(span);
        Ok(Expr { kind: ExprKind::ListOp(name, args), span: span.merge(end_span) })
    }

    /// Parse print/say with optional filehandle as first argument.
    /// `print STDERR "error"`, `print "hello"`, `say $fh "data"`
    fn parse_print_op(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let name = format!("{kw:?}").to_lowercase();
        self.expect.base = BaseExpect::Term;

        // Check for parens
        if self.at(&Token::LParen) {
            return self.parse_list_op(kw, span);
        }

        // No parens — check for bare filehandle (uppercase identifier not followed by comma)
        // This is a heuristic: `print STDERR "hello"` vs `print $x, $y`
        // Perl uses the rule: bareword followed by non-comma term is a filehandle.
        let mut args = Vec::new();

        // Collect args as list
        while !self.at(&Token::Semi) && !self.at_eof() && !self.at(&Token::RBrace) {
            if matches!(
                self.peek(),
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
            if !self.eat(&Token::Comma) {
                break;
            }
        }

        let end_span = args.last().map(|a| a.span).unwrap_or(span);
        Ok(Expr { kind: ExprKind::ListOp(name, args), span: span.merge(end_span) })
    }

    /// Parse the operand of a prefix dereference ($$ref, @$ref, etc.).
    /// Consumes just the variable — subscripts are NOT included.
    /// This ensures $$ref[0] parses as ($$ref)[0], not $(${ref}[0]).
    fn parse_deref_operand(&mut self) -> Result<Expr, ParseError> {
        let spanned = self.advance();
        let span = spanned.span;
        match spanned.token {
            Token::ScalarVar(name) => Ok(Expr { kind: ExprKind::ScalarVar(name), span }),
            Token::ArrayVar(name) => Ok(Expr { kind: ExprKind::ArrayVar(name), span }),
            Token::HashVar(name) => Ok(Expr { kind: ExprKind::HashVar(name), span }),
            Token::SpecialVar(name) => Ok(Expr { kind: ExprKind::SpecialVar(name), span }),
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
        if self.at(&Token::LParen) {
            self.advance();
            self.expect.base = BaseExpect::Term;
            let mut args = Vec::new();
            while !self.at(&Token::RParen) && !self.at_eof() {
                args.push(self.parse_expr(PREC_COMMA + 1)?);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            let end = self.peek_span();
            self.expect_token(&Token::RParen)?;
            Ok(Expr { span: callee.span.merge(end), kind: ExprKind::MethodCall(Box::new(callee), String::new(), args) })
        } else {
            Ok(callee)
        }
    }

    fn maybe_postfix_subscript(&mut self, mut expr: Expr) -> Result<Expr, ParseError> {
        // Handle chained subscripts: $x[0][1], $x{a}{b}, $x[0]{key}
        loop {
            if self.at(&Token::LBracket) {
                self.advance();
                self.expect.base = BaseExpect::Term;
                let idx = self.parse_expr(PREC_LOW)?;
                let end = self.peek_span();
                self.expect_token(&Token::RBracket)?;
                expr = Expr { span: expr.span.merge(end), kind: ExprKind::ArrayElem(Box::new(expr), Box::new(idx)) };
            } else if self.at(&Token::LBrace) {
                self.advance();
                self.expect.base = BaseExpect::Term;
                let key = self.parse_expr(PREC_LOW)?;
                let end = self.peek_span();
                self.expect_token(&Token::RBrace)?;
                expr = Expr { span: expr.span.merge(end), kind: ExprKind::HashElem(Box::new(expr), Box::new(key)) };
            } else {
                break;
            }
        }
        Ok(expr)
    }

    // ── Interpolated string assembly ──────────────────────────

    /// Collect sub-tokens after a `QuoteBegin` into an AST node.
    /// Produces `StringLit` if no interpolation, `InterpolatedString` otherwise.
    fn parse_interpolated_string(&mut self, start_span: Span) -> Result<Expr, ParseError> {
        let mut parts: Vec<StringPart> = Vec::new();
        let mut has_interp = false;

        loop {
            match self.peek().clone() {
                Token::QuoteEnd => {
                    let end = self.peek_span();
                    self.advance();
                    let span = start_span.merge(end);

                    // Optimize: if no interpolation, collapse to a plain string.
                    if !has_interp {
                        let s: String = parts
                            .into_iter()
                            .map(|p| match p {
                                StringPart::Const(s) => s,
                                _ => unreachable!(),
                            })
                            .collect();
                        return Ok(Expr { kind: ExprKind::StringLit(s), span });
                    }

                    // Merge adjacent Const segments.
                    let merged = merge_string_parts(parts);
                    return Ok(Expr { kind: ExprKind::InterpolatedString(merged), span });
                }
                Token::ConstSegment(s) => {
                    self.advance();
                    parts.push(StringPart::Const(s));
                }
                Token::InterpScalar(name) => {
                    self.advance();
                    has_interp = true;
                    parts.push(StringPart::ScalarInterp(name));
                }
                Token::InterpArray(name) => {
                    self.advance();
                    has_interp = true;
                    parts.push(StringPart::ArrayInterp(name));
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

    // ── Operator parsing ──────────────────────────────────────

    fn peek_op_info(&mut self) -> Option<OpInfo> {
        match self.peek() {
            Token::OrOr => Some(OpInfo { prec: PREC_OR, assoc: Assoc::Left }),
            Token::DorDor => Some(OpInfo { prec: PREC_OR, assoc: Assoc::Left }),
            Token::AndAnd => Some(OpInfo { prec: PREC_AND, assoc: Assoc::Left }),
            Token::BitOr => Some(OpInfo { prec: PREC_BIT_OR, assoc: Assoc::Left }),
            Token::BitXor => Some(OpInfo { prec: PREC_BIT_OR, assoc: Assoc::Left }),
            Token::BitAnd => Some(OpInfo { prec: PREC_BIT_AND, assoc: Assoc::Left }),
            Token::NumEq | Token::NumNe | Token::Spaceship => Some(OpInfo { prec: PREC_EQ, assoc: Assoc::Non }),
            Token::StrEq | Token::StrNe | Token::StrCmp => Some(OpInfo { prec: PREC_EQ, assoc: Assoc::Non }),
            Token::NumLt | Token::NumGt | Token::NumLe | Token::NumGe => Some(OpInfo { prec: PREC_REL, assoc: Assoc::Non }),
            Token::StrLt | Token::StrGt | Token::StrLe | Token::StrGe => Some(OpInfo { prec: PREC_REL, assoc: Assoc::Non }),
            Token::ShiftL | Token::ShiftR => Some(OpInfo { prec: PREC_SHIFT, assoc: Assoc::Left }),
            Token::Plus => Some(OpInfo { prec: PREC_ADD, assoc: Assoc::Left }),
            Token::Minus => Some(OpInfo { prec: PREC_ADD, assoc: Assoc::Left }),
            Token::Dot => Some(OpInfo { prec: PREC_ADD, assoc: Assoc::Left }),
            Token::Star => Some(OpInfo { prec: PREC_MUL, assoc: Assoc::Left }),
            Token::Slash => Some(OpInfo { prec: PREC_MUL, assoc: Assoc::Left }),
            Token::Percent => Some(OpInfo { prec: PREC_MUL, assoc: Assoc::Left }),
            Token::X => Some(OpInfo { prec: PREC_MUL, assoc: Assoc::Left }),
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
            _ => None,
        }
    }

    fn parse_operator(&mut self, left: Expr, info: OpInfo) -> Result<Expr, ParseError> {
        let op_spanned = self.advance();
        let right_prec = info.right_prec();

        match op_spanned.token {
            // Postfix increment/decrement
            Token::PlusPlus => Ok(Expr { span: left.span.merge(op_spanned.span), kind: ExprKind::PostfixOp(PostfixOp::Inc, Box::new(left)) }),
            Token::MinusMinus => Ok(Expr { span: left.span.merge(op_spanned.span), kind: ExprKind::PostfixOp(PostfixOp::Dec, Box::new(left)) }),

            // Ternary
            Token::Question => {
                self.expect.base = BaseExpect::Term;
                let then_expr = self.parse_expr(PREC_LOW)?;
                self.expect_token(&Token::Colon)?;
                self.expect.base = BaseExpect::Term;
                let else_expr = self.parse_expr(right_prec)?;
                Ok(Expr { span: left.span.merge(else_expr.span), kind: ExprKind::Ternary(Box::new(left), Box::new(then_expr), Box::new(else_expr)) })
            }

            // Arrow
            Token::Arrow => {
                self.expect = Expect::XREF;
                self.parse_arrow_rhs(left)
            }

            // Assignment
            Token::Assign(op) => {
                self.expect.base = BaseExpect::Term;
                let right = self.parse_expr(right_prec)?;
                Ok(Expr { span: left.span.merge(right.span), kind: ExprKind::Assign(op, Box::new(left), Box::new(right)) })
            }

            // Comma / fat comma — build a list
            Token::Comma | Token::FatComma => {
                self.expect.base = BaseExpect::Term;
                if self.at(&Token::Semi) || self.at(&Token::RParen) || self.at(&Token::RBracket) || self.at(&Token::RBrace) || self.at_eof() {
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
                let span = items.first().unwrap().span.merge(items.last().unwrap().span);
                Ok(Expr { kind: ExprKind::List(items), span })
            }

            // Range
            Token::DotDot => {
                self.expect.base = BaseExpect::Term;
                let right = self.parse_expr(right_prec)?;
                Ok(Expr { span: left.span.merge(right.span), kind: ExprKind::Range(Box::new(left), Box::new(right)) })
            }
            Token::DotDotDot => {
                self.expect.base = BaseExpect::Term;
                let right = self.parse_expr(right_prec)?;
                Ok(Expr { span: left.span.merge(right.span), kind: ExprKind::FlipFlop(Box::new(left), Box::new(right)) })
            }

            // Binary operators
            token => {
                let binop = token_to_binop(&token)?;
                self.expect.base = BaseExpect::Term;
                let right = self.parse_expr(right_prec)?;
                Ok(Expr { span: left.span.merge(right.span), kind: ExprKind::BinOp(binop, Box::new(left), Box::new(right)) })
            }
        }
    }

    fn parse_arrow_rhs(&mut self, left: Expr) -> Result<Expr, ParseError> {
        match self.peek().clone() {
            Token::Ident(name) => {
                self.advance();
                // Method call: ->method(...)
                if self.at(&Token::LParen) {
                    self.advance();
                    self.expect.base = BaseExpect::Term;
                    let mut args = Vec::new();
                    while !self.at(&Token::RParen) && !self.at_eof() {
                        args.push(self.parse_expr(PREC_COMMA + 1)?);
                        if !self.eat(&Token::Comma) {
                            break;
                        }
                    }
                    let end = self.peek_span();
                    self.expect_token(&Token::RParen)?;
                    Ok(Expr { span: left.span.merge(end), kind: ExprKind::MethodCall(Box::new(left), name, args) })
                } else {
                    // Bare method call with no parens
                    Ok(Expr { span: left.span.merge(self.peek_span()), kind: ExprKind::MethodCall(Box::new(left), name, vec![]) })
                }
            }
            Token::LBracket => {
                self.advance();
                self.expect.base = BaseExpect::Term;
                let idx = self.parse_expr(PREC_LOW)?;
                let end = self.peek_span();
                self.expect_token(&Token::RBracket)?;
                let expr = Expr { span: left.span.merge(end), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::ArrayElem(Box::new(idx))) };
                // Handle chained subscripts: $ref->[0][1], $ref->[0]{key}
                self.maybe_postfix_subscript(expr)
            }
            Token::LBrace => {
                self.advance();
                self.expect.base = BaseExpect::Term;
                let key = self.parse_expr(PREC_LOW)?;
                let end = self.peek_span();
                self.expect_token(&Token::RBrace)?;
                let expr = Expr { span: left.span.merge(end), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::HashElem(Box::new(key))) };
                // Handle chained subscripts: $ref->{a}{b}, $ref->{a}[0]
                self.maybe_postfix_subscript(expr)
            }
            Token::LParen => {
                // ->(...) — coderef call
                self.advance();
                self.expect.base = BaseExpect::Term;
                let mut args = Vec::new();
                while !self.at(&Token::RParen) && !self.at_eof() {
                    args.push(self.parse_expr(PREC_COMMA + 1)?);
                    if !self.eat(&Token::Comma) {
                        break;
                    }
                }
                let end = self.peek_span();
                self.expect_token(&Token::RParen)?;
                Ok(Expr { span: left.span.merge(end), kind: ExprKind::MethodCall(Box::new(left), String::new(), args) })
            }
            // Postfix dereference: ->@*, ->%*, ->$*, ->@[...], ->@{...}
            Token::At => {
                self.advance();
                if self.eat(&Token::Star) {
                    Ok(Expr { span: left.span.merge(self.peek_span()), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::DerefArray) })
                } else {
                    Err(ParseError::new("expected * after ->@", self.peek_span()))
                }
            }
            Token::Dollar => {
                self.advance();
                if self.eat(&Token::Star) {
                    Ok(Expr { span: left.span.merge(self.peek_span()), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::DerefScalar) })
                } else {
                    Err(ParseError::new("expected * after ->$", self.peek_span()))
                }
            }
            Token::Percent => {
                self.advance();
                if self.eat(&Token::Star) {
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
        Token::X => Ok(BinOp::Repeat),
        Token::NumEq => Ok(BinOp::NumEq),
        Token::NumNe => Ok(BinOp::NumNe),
        Token::NumLt => Ok(BinOp::NumLt),
        Token::NumGt => Ok(BinOp::NumGt),
        Token::NumLe => Ok(BinOp::NumLe),
        Token::NumGe => Ok(BinOp::NumGe),
        Token::Spaceship => Ok(BinOp::Spaceship),
        Token::StrEq => Ok(BinOp::StrEq),
        Token::StrNe => Ok(BinOp::StrNe),
        Token::StrLt => Ok(BinOp::StrLt),
        Token::StrGt => Ok(BinOp::StrGt),
        Token::StrLe => Ok(BinOp::StrLe),
        Token::StrGe => Ok(BinOp::StrGe),
        Token::StrCmp => Ok(BinOp::StrCmp),
        Token::AndAnd => Ok(BinOp::And),
        Token::OrOr => Ok(BinOp::Or),
        Token::DorDor => Ok(BinOp::Dor),
        Token::BitAnd => Ok(BinOp::BitAnd),
        Token::BitOr => Ok(BinOp::BitOr),
        Token::BitXor => Ok(BinOp::BitXor),
        Token::ShiftL => Ok(BinOp::ShiftL),
        Token::ShiftR => Ok(BinOp::ShiftR),
        Token::Binding => Ok(BinOp::Binding),
        Token::NotBinding => Ok(BinOp::NotBinding),
        Token::Keyword(Keyword::And) => Ok(BinOp::LowAnd),
        Token::Keyword(Keyword::Or) => Ok(BinOp::LowOr),
        other => Err(ParseError::new(format!("not a binary operator: {other:?}"), Span::DUMMY)),
    }
}

/// Merge adjacent `Const` segments in an interpolated string.
fn merge_string_parts(parts: Vec<StringPart>) -> Vec<StringPart> {
    let mut merged: Vec<StringPart> = Vec::new();
    for part in parts {
        if let StringPart::Const(s) = &part {
            if let Some(StringPart::Const(prev)) = merged.last_mut() {
                prev.push_str(s);
                continue;
            }
        }
        merged.push(part);
    }
    merged
}

#[cfg(test)]
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
            StmtKind::My(_, Some(e)) => e.clone(),
            other => panic!("expected expression, got {other:?}"),
        }
    }

    #[test]
    fn parse_simple_assignment() {
        let prog = parse("my $x = 42;");
        assert_eq!(prog.statements.len(), 1);
        match &prog.statements[0].kind {
            StmtKind::My(vars, Some(init)) => {
                assert_eq!(vars.len(), 1);
                assert_eq!(vars[0].name, "x");
                assert!(matches!(init.kind, ExprKind::IntLit(42)));
            }
            other => panic!("expected My, got {other:?}"),
        }
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
        assert!(matches!(prog.statements[0].kind, StmtKind::If(_)));
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
        assert!(matches!(prog.statements[0].kind, StmtKind::While(_)));
    }

    #[test]
    fn parse_foreach_loop() {
        let prog = parse("for my $item (@list) { print $item; }");
        assert!(matches!(prog.statements[0].kind, StmtKind::ForEach(_)));
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
        assert!(matches!(e.kind, ExprKind::MethodCall(_, _, _)));
    }

    #[test]
    fn parse_arrow_deref() {
        let e = parse_expr_str("$ref->{key};");
        assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))));
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
            StmtKind::Expr(Expr { kind: ExprKind::ListOp(name, args), .. }) => {
                assert_eq!(name, "print");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected ListOp, got {other:?}"),
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
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Dor, _, _)));
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
            ExprKind::InterpolatedString(parts) => {
                assert_eq!(parts.len(), 3);
                assert!(matches!(&parts[0], StringPart::Const(s) if s == "Hello, "));
                assert!(matches!(&parts[1], StringPart::ScalarInterp(s) if s == "name"));
                assert!(matches!(&parts[2], StringPart::Const(s) if s == "!"));
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn parse_interp_multiple_vars() {
        let e = parse_expr_str(r#""$x and $y";"#);
        match &e.kind {
            ExprKind::InterpolatedString(parts) => {
                assert_eq!(parts.len(), 3);
                assert!(matches!(&parts[0], StringPart::ScalarInterp(s) if s == "x"));
                assert!(matches!(&parts[1], StringPart::Const(s) if s == " and "));
                assert!(matches!(&parts[2], StringPart::ScalarInterp(s) if s == "y"));
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn parse_interp_array() {
        let e = parse_expr_str(r#""items: @list""#);
        match &e.kind {
            ExprKind::InterpolatedString(parts) => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(&parts[0], StringPart::Const(s) if s == "items: "));
                assert!(matches!(&parts[1], StringPart::ArrayInterp(s) if s == "list"));
            }
            other => panic!("expected InterpolatedString, got {other:?}"),
        }
    }

    #[test]
    fn parse_string_concat_interp() {
        // Interpolated string in a concat expression.
        let e = parse_expr_str(r#""Hello, $name!" . " Bye!""#);
        assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Concat, _, _)));
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
    }

    // ── Regex / substitution / transliteration tests ──────────

    #[test]
    fn parse_bare_regex() {
        let e = parse_expr_str("/foo/i;");
        match &e.kind {
            ExprKind::Regex(pat, flags) => {
                assert_eq!(pat, "foo");
                assert_eq!(flags, "i");
            }
            other => panic!("expected Regex, got {other:?}"),
        }
    }

    #[test]
    fn parse_regex_binding() {
        let e = parse_expr_str("$x =~ /foo/;");
        match &e.kind {
            ExprKind::BinOp(BinOp::Binding, _, right) => {
                assert!(matches!(&right.kind, ExprKind::Regex(_, _)));
            }
            other => panic!("expected Binding, got {other:?}"),
        }
    }

    #[test]
    fn parse_substitution() {
        let e = parse_expr_str("s/foo/bar/g;");
        match &e.kind {
            ExprKind::Subst(pat, _, flags) => {
                assert_eq!(pat, "foo");
                assert_eq!(flags, "g");
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
        match &prog.statements[0].kind {
            StmtKind::My(vars, Some(init)) => {
                assert_eq!(vars[0].name, "x");
                match &init.kind {
                    ExprKind::StringLit(s) => assert_eq!(s, "hello world\n"),
                    other => panic!("expected StringLit, got {other:?}"),
                }
            }
            other => panic!("expected My, got {other:?}"),
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
    }

    // ── Anonymous sub tests ───────────────────────────────────

    #[test]
    fn parse_anon_sub() {
        let e = parse_expr_str("sub { 42; };");
        assert!(matches!(e.kind, ExprKind::AnonSub(_, _, _)));
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
        match &prog.statements[0].kind {
            StmtKind::My(_, Some(init)) => {
                assert!(matches!(init.kind, ExprKind::AnonSub(_, _, _)));
            }
            other => panic!("expected My with AnonSub, got {other:?}"),
        }
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
            StmtKind::Expr(Expr { kind: ExprKind::ListOp(name, _), .. }) => {
                assert_eq!(name, "print");
            }
            other => panic!("expected print ListOp, got {other:?}"),
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
        assert!(matches!(e.kind, ExprKind::Deref(Sigil::Scalar, _)));
    }

    #[test]
    fn parse_array_deref_block() {
        let e = parse_expr_str("@{$ref};");
        assert!(matches!(e.kind, ExprKind::Deref(Sigil::Array, _)));
    }

    #[test]
    fn parse_deref_subscript() {
        // $$ref[0] — deref then subscript
        let e = parse_expr_str("$$ref[0];");
        assert!(matches!(e.kind, ExprKind::ArrayElem(_, _)));
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
        assert!(matches!(prog.statements[0].kind, StmtKind::Given(_, _)));
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
        assert!(matches!(prog.statements[0].kind, StmtKind::Defer(_)));
    }

    // ── __END__ test ──────────────────────────────────────────

    #[test]
    fn parse_end_stops_parsing() {
        let prog = parse("my $x = 1;\n__END__\nThis is not code.\n");
        // Should have 2 statements: my decl and DataEnd
        assert_eq!(prog.statements.len(), 2);
        assert!(matches!(prog.statements[1].kind, StmtKind::DataEnd));
    }

    #[test]
    fn parse_data_stops_parsing() {
        let prog = parse("my $x = 1;\n__DATA__\nraw data here\n");
        assert_eq!(prog.statements.len(), 2);
        assert!(matches!(prog.statements[1].kind, StmtKind::DataEnd));
    }

    // ── Pod skipping test ─────────────────────────────────────

    #[test]
    fn parse_pod_skipped() {
        let prog = parse("my $x = 1;\n\n=pod\n\nThis is pod.\n\n=cut\n\nmy $y = 2;\n");
        // Should see both my declarations, pod is invisible
        let my_count = prog.statements.iter().filter(|s| matches!(s.kind, StmtKind::My(_, _))).count();
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
        // my $x = 5 in statement context still works
        let prog = parse("my $x = 5;");
        assert!(matches!(prog.statements[0].kind, StmtKind::My(_, _)));
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
                    assert!(matches!(lhs.kind, ExprKind::Decl(DeclScope::Local, _)));
                }
                other => panic!("expected Assign(Decl(Local), ...), got {other:?}"),
            },
            other => panic!("expected For, got {other:?}"),
        }
    }

    // ── my with array/hash ────────────────────────────────────

    #[test]
    fn parse_my_array_decl() {
        let prog = parse("my @arr = (1, 2, 3);");
        assert!(matches!(prog.statements[0].kind, StmtKind::My(_, _)));
    }

    #[test]
    fn parse_my_hash_decl() {
        let prog = parse("my %hash = (a => 1);");
        assert!(matches!(prog.statements[0].kind, StmtKind::My(_, _)));
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
        assert!(matches!(e.kind, ExprKind::MethodCall(_, _, _)));
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
        assert!(matches!(e.kind, ExprKind::Deref(Sigil::Hash, _)));
    }

    #[test]
    fn parse_hash_deref_block() {
        let e = parse_expr_str("%{$ref};");
        assert!(matches!(e.kind, ExprKind::Deref(Sigil::Hash, _)));
    }

    // ── Glob / typeglob tests ─────────────────────────────────

    #[test]
    fn parse_glob_var() {
        let e = parse_expr_str("*foo;");
        assert!(matches!(e.kind, ExprKind::GlobVar(_)));
    }

    #[test]
    fn parse_glob_deref() {
        let e = parse_expr_str("*$ref;");
        assert!(matches!(e.kind, ExprKind::Deref(Sigil::Glob, _)));
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
        match &prog.statements[0].kind {
            StmtKind::My(_, Some(init)) => {
                match &init.kind {
                    ExprKind::IntLit(n) => assert_eq!(*n, 0o777), // 511 decimal
                    other => panic!("expected IntLit, got {other:?}"),
                }
            }
            other => panic!("expected My, got {other:?}"),
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
}
