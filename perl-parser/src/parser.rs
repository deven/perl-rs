//! Pratt parser with recursive descent for statements (§6).
//!
//! Expression assembly uses precedence climbing.  Statements, declarations,
//! blocks, and top-level forms use ordinary recursive descent that calls
//! `parse_expr` where expressions are needed.

use crate::ast::*;
use crate::error::ParseError;
use crate::expect::{BaseExpect, BraceDisposition, Expect};
use crate::keyword;
use crate::lexer::Lexer;
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
    /// Cached current token with the lexer state that produced it.
    /// `None` means no token is cached — the next peek/advance will lex.
    /// Fields: (token, pre_lex_pos, pre_lex_context_depth, expect_used).
    /// On rewind, both position and context stack are restored.
    current: Option<(Spanned, usize, usize, Expect)>,
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
    /// expect, rewind the lexer (position + context stack) and re-lex.
    fn ensure_current(&mut self) {
        match &self.current {
            Some((_, _, _, cached_expect)) if *cached_expect == self.expect => {
                return;
            }
            Some((_, pre_lex_pos, pre_lex_ctx_depth, _)) => {
                let pos = *pre_lex_pos;
                let ctx_depth = *pre_lex_ctx_depth;
                self.lexer.set_pos(pos);
                self.lexer.restore_context_depth(ctx_depth);
                self.current = None;
            }
            None => {}
        }

        let pre_lex_pos = self.lexer.pos();
        let pre_lex_ctx_depth = self.lexer.context_depth();
        let spanned =
            self.lexer.next_token(&self.expect).unwrap_or(Spanned { token: Token::Eof, span: Span::new(self.lexer.pos() as u32, self.lexer.pos() as u32) });
        self.current = Some((spanned, pre_lex_pos, pre_lex_ctx_depth, self.expect));
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
            statements.push(stmt);
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

        let kind = match self.peek().clone() {
            Token::Keyword(Keyword::My) => self.parse_my_decl()?,
            Token::Keyword(Keyword::Our) => self.parse_our_decl()?,
            Token::Keyword(Keyword::Local) => self.parse_local_decl()?,
            Token::Keyword(Keyword::State) => self.parse_state_decl()?,
            Token::Keyword(Keyword::Sub) => self.parse_sub_decl()?,
            Token::Keyword(Keyword::If) => self.parse_if_stmt()?,
            Token::Keyword(Keyword::Unless) => self.parse_unless_stmt()?,
            Token::Keyword(Keyword::While) => self.parse_while_stmt()?,
            Token::Keyword(Keyword::Until) => self.parse_until_stmt()?,
            Token::Keyword(Keyword::For) | Token::Keyword(Keyword::Foreach) => self.parse_for_stmt()?,
            Token::Keyword(Keyword::Package) => self.parse_package_decl()?,
            Token::Keyword(Keyword::Use) | Token::Keyword(Keyword::No) => self.parse_use_decl()?,
            Token::LBrace => {
                let block = self.parse_block()?;
                StmtKind::Block(block)
            }
            _ => {
                // Expression statement
                self.expect.base = BaseExpect::Term;
                let expr = self.parse_expr(PREC_LOW)?;

                // Check for postfix control flow
                let kind = self.maybe_postfix_control(expr)?;

                self.eat(&Token::Semi);
                kind
            }
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
        let else_block = if self.eat(&Token::Keyword(Keyword::Else)) { Some(self.parse_block()?) } else { None };
        Ok(StmtKind::Unless(UnlessStmt { condition, then_block, else_block }))
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

        // Determine: C-style for or foreach-style
        // foreach $var (LIST) { }
        // for (init; cond; step) { }
        // for $var (LIST) { }

        // If next token is a variable or 'my', it's foreach-style
        if matches!(self.peek(), Token::Keyword(Keyword::My) | Token::ScalarVar(_)) {
            return self.parse_foreach_body();
        }

        // If next is '(' we need to peek further to distinguish
        // for ($i=0; ...) vs for (@list)
        // For now, assume foreach-style if '(' follows
        let list = self.parse_paren_expr()?;
        self.expect.brace = BraceDisposition::Block;
        let body = self.parse_block()?;

        Ok(StmtKind::ForEach(ForEachStmt { var: None, list, body, continue_block: None }))
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

        Ok(StmtKind::ForEach(ForEachStmt { var, list, body, continue_block: None }))
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
            Token::ArrayVar(name) => Ok(Expr { kind: ExprKind::ArrayVar(name), span }),
            Token::HashVar(name) => Ok(Expr { kind: ExprKind::HashVar(name), span }),
            Token::GlobVar(name) => Ok(Expr { kind: ExprKind::GlobVar(name), span }),
            Token::ArrayLen(name) => Ok(Expr { kind: ExprKind::ArrayLen(name), span }),
            Token::SpecialVar(name) => Ok(Expr { kind: ExprKind::SpecialVar(name), span }),

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
            Token::Not => {
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

    fn maybe_postfix_subscript(&mut self, expr: Expr) -> Result<Expr, ParseError> {
        // Check for $x[idx] or $x{key} immediately after a scalar var
        if self.at(&Token::LBracket) {
            self.advance();
            self.expect.base = BaseExpect::Term;
            let idx = self.parse_expr(PREC_LOW)?;
            let end = self.peek_span();
            self.expect_token(&Token::RBracket)?;
            return Ok(Expr { span: expr.span.merge(end), kind: ExprKind::ArrayElem(Box::new(expr), Box::new(idx)) });
        }
        if self.at(&Token::LBrace) {
            // Could be hash subscript — we need better heuristics here
            // For now, in operator position after a scalar, { starts a hash subscript
            self.advance();
            self.expect.base = BaseExpect::Term;
            let key = self.parse_expr(PREC_LOW)?;
            let end = self.peek_span();
            self.expect_token(&Token::RBrace)?;
            return Ok(Expr { span: expr.span.merge(end), kind: ExprKind::HashElem(Box::new(expr), Box::new(key)) });
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
                Ok(Expr { span: left.span.merge(end), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::ArrayElem(Box::new(idx))) })
            }
            Token::LBrace => {
                self.advance();
                self.expect.base = BaseExpect::Term;
                let key = self.parse_expr(PREC_LOW)?;
                let end = self.peek_span();
                self.expect_token(&Token::RBrace)?;
                Ok(Expr { span: left.span.merge(end), kind: ExprKind::ArrowDeref(Box::new(left), ArrowTarget::HashElem(Box::new(key))) })
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
}
