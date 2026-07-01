//! Pratt parser with recursive descent for statements (§6).
//!
//! Expression assembly uses precedence climbing.  Statements, declarations, blocks, and top-level forms use ordinary
//! recursive descent that calls `parse_expr` where expressions are needed.

use crate::ast::*;
use crate::error::ParseError;
use crate::keyword::{self, Keyword};
use crate::lexer::{FormatState, FrameRole, LexContext};
use crate::pragma::{Features, Pragmas, resolve_feature_name};
use crate::source::LexerLine;
use crate::span::Span;
use crate::symbol::{ProtoSlot, SubPrototype, SymbolTable};
use crate::token::*;
use bytes::Bytes;
use std::collections::VecDeque;

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
    // ── Lexer source layer (line delivery, heredoc/subst interleaving, CRLF; methods in source.rs) ──
    /// The complete source buffer.
    pub(crate) src: Bytes,
    /// Name of the source — used for `__FILE__` resolution and diagnostic messages.  Defaults to `"(script)"`.
    pub(crate) filename: String,
    /// Current byte position for reading the next line.
    pub(crate) cursor: usize,
    /// The line currently being scanned, if any.  `None` forces a fresh `next_line` read on the next `peek_byte`.
    pub(crate) line: Option<LexerLine>,
    /// Next line number to assign (1-based).
    pub(crate) line_number: u32,
    /// Lines queued for delivery by future `next_line()` calls (heredoc remainders, push_back, subst bodies).
    pub(crate) queued_lines: VecDeque<LexerLine>,
    /// Displaced lines captured during an active lookahead scan, in displacement order.
    pub(crate) lookahead: VecDeque<LexerLine>,
    /// Source offset of the line a lookahead scan ended on, identifying it for `consume_lookahead`.
    pub(crate) lookahead_offset: Option<u32>,
    /// Cursor position the lookahead scan ended at, within the end line.
    pub(crate) lookahead_pos: Option<u32>,
    /// True while a lookahead guard is alive.
    pub(crate) lookahead_mode: bool,

    // ── Lexer tokenization layer (methods in lexer.rs) ──
    pub(crate) context_stack: Vec<LexContext>,
    /// Deferred error from auto-loading in `peek_byte`.  Surfaced on the next call to `lex_token`.
    pub(crate) pending_error: Option<ParseError>,
    /// Active format sublex state, if we're inside a format body.
    pub(crate) format_state: Option<FormatState>,
    /// Whether `use utf8` is active.  Kept in step with `pragmas` at block boundaries; read by the lexer layer for
    /// error diagnostics on high bytes outside strings.
    pub(crate) utf8_mode: bool,
    /// Fast-path composite: `utf8_mode && !line.ascii_only`.  When false, the current line is pure ASCII.
    pub(crate) effective_utf8: bool,
    /// Feature flags, kept in step with `pragmas.features`.  A cached copy the lexer layer reads on hot paths.
    pub(crate) features: Features,
    /// Stacked cumulative case-modification flags (`\L`/`\U`/`\F`/`\Q`/`\E`).
    pub(crate) case_mod_stack: Vec<CaseMod>,
    /// `\l` pending — lowercase the very next character only.
    pub(crate) case_mod_lcfirst: bool,
    /// `\u` pending — titlecase the very next character only.
    pub(crate) case_mod_ucfirst: bool,
    /// Set when `__DATA__`/`__END__` or `^D`/`^Z` triggers logical end-of-source.
    pub(crate) logical_eof: bool,
    /// Set when `__DATA__`/`__END__` triggers logical EOF: the keyword and the byte offset where trailing data begins.
    pub(crate) data_end_info: Option<(Keyword, u32)>,

    /// Current token.  Read directly by parsing methods; advanced by `lex_token`.
    pub(crate) tok: Spanned,

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
        Self::build(Bytes::copy_from_slice(src), "(script)".into())
    }

    /// Construct a parser that reports `filename` for `__FILE__` resolution and in diagnostic messages.  Prefer this
    /// over [`Self::new`] when the source comes from a named file.
    pub fn with_filename(src: &[u8], filename: impl Into<String>) -> Result<Self, ParseError> {
        Self::build(Bytes::copy_from_slice(src), filename.into())
    }

    /// Shared core: detect/transcode any BOM or UTF-16 encoding, then initialize the lexer-layer and parser-layer
    /// state together.  The source bytes are copied into a `Bytes` buffer once; all subsequent line slicing is
    /// zero-copy.
    fn build(src: Bytes, filename: String) -> Result<Self, ParseError> {
        let (src, bom_utf8) = Self::detect_and_transcode(src);
        Ok(Parser {
            // Lexer source layer.
            src,
            filename,
            cursor: 0,
            line: None,
            line_number: 1,
            queued_lines: VecDeque::new(),
            lookahead: VecDeque::new(),
            lookahead_offset: None,
            lookahead_pos: None,
            lookahead_mode: false,

            // Lexer tokenization layer.
            context_stack: Vec::new(),
            pending_error: None,
            format_state: None,
            utf8_mode: bom_utf8,
            effective_utf8: false,
            features: Features::DEFAULT,
            case_mod_stack: Vec::new(),
            case_mod_lcfirst: false,
            case_mod_ucfirst: false,
            logical_eof: false,
            data_end_info: None,
            tok: Spanned { token: Token::Eof, span: Span::new(0, 0) },

            // Parser layer.
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
        self.features = features;
    }

    // ── Token access ──────────────────────────────────────────
    /// Verify that the current token matches `expected` and advance past it.  Produces a parse error if the token does
    /// not match.  Callers that need the consumed token's span save it before calling.
    fn expect(&mut self, expected: &Token) -> Result<(), ParseError> {
        if &self.tok.token != expected {
            return Err(ParseError::new(format!("expected {expected}, got {}", self.tok.token), self.tok.span));
        }
        self.tok = self.lex_token()?;
        Ok(())
    }

    // ── Flag validation ───────────────────────────────────────
    /// Validate regex modifier flags.  Returns an error for any unrecognized modifier character.
    pub(crate) fn validate_regex_flags(flags: &str, span: Span) -> Result<(), ParseError> {
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
        self.tok = self.lex_token()?;
        let start = self.tok.span;
        let mut statements = Vec::new();

        while self.tok.token != Token::Eof {
            let stmt = self.parse_statement()?;
            statements.push(stmt);
        }

        // If __DATA__/__END__ triggered logical EOF, the lexer stored the keyword and data offset.  Emit a DataEnd
        // node so the compiler knows where the DATA filehandle content begins.
        if let Some((kw, offset)) = self.data_end_info.take() {
            statements.push(Statement { kind: StmtKind::DataEnd(kw, offset), span: self.tok.span, terminated: false });
        }

        let end = self.tok.span;
        let mut program = Program { statements, span: start.merge(end) };
        // Stamp evaluation context across the whole tree (§6.2.5).  The program is a body evaluated in the caller's
        // runtime context, so its final statement is Runtime (load-determined, resolved at runtime) and every earlier
        // statement is Void.
        program.save_context(Context::Runtime);

        Ok(program)
    }

    // ── Statement parsing ─────────────────────────────────────
    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        let start = self.tok.span;

        // Empty statement
        if self.tok.token == Token::Semi {
            self.tok = self.lex_token()?;
            return Ok(Statement { kind: StmtKind::Empty, span: start, terminated: true });
        }

        let kind = match &self.tok.token {
            // Statement-level keywords: consume first, then dispatch to handler.  Fat-comma autoquoting
            // (e.g. `if => 1`) is handled by the lexer, which returns StrLit instead of Keyword.
            Token::Keyword(kw) if keyword::is_statement_keyword(*kw) => {
                let kw = *kw;
                let kw_span = self.tok.span;
                self.tok = self.lex_token()?;
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
                        if self.tok.token == Token::Keyword(Keyword::Sub) {
                            self.tok = self.lex_token()?;
                            if !matches!(self.tok.token, Token::Ident(_) | Token::Keyword(_)) {
                                return Err(ParseError::new("expected sub name after my/our/state sub", self.tok.span));
                            }
                            let kind = self.parse_sub_decl_body(kw_span)?;
                            // Patch the scope onto the SubDecl.
                            match kind {
                                StmtKind::SubDecl(mut sd) => {
                                    sd.scope = Some(scope);
                                    StmtKind::SubDecl(sd)
                                }
                                other => other,
                            }
                        // `my method foo { }`, `state method foo { }`
                        } else if self.tok.token == Token::Keyword(Keyword::Method) {
                            self.tok = self.lex_token()?;
                            let kind = self.parse_method(kw_span)?;
                            match kind {
                                StmtKind::MethodDecl(mut sd) => {
                                    sd.scope = Some(scope);
                                    StmtKind::MethodDecl(sd)
                                }
                                other => other,
                            }
                        } else {
                            let initial = self.parse_decl_expr(scope, kw_span)?;
                            // `my (...)` yields a transient `Paren(Decl)`; after the continuation has had its chance
                            // to consume it (e.g. `my ($x) = ...`, where `=` reads-then-unwraps), unwrap any grouping
                            // `Paren` that survived a bare `my ($a, $b);` so it does not persist in the statement.
                            let expr = self.parse_expr_continuation(initial, PREC_LOW)?;
                            return self.finish_expr_stmt(Self::unwrap_paren(expr), start);
                        }
                    }
                    Keyword::Sub => {
                        if matches!(self.tok.token, Token::Ident(_) | Token::Keyword(_)) {
                            self.parse_sub_decl_body(kw_span)?
                        } else {
                            let expr = self.parse_anon_sub(kw_span)?;
                            return self.finish_expr_stmt(expr, start);
                        }
                    }
                    Keyword::If => self.parse_if_stmt()?,
                    Keyword::Unless => self.parse_unless_stmt()?,
                    Keyword::While => self.parse_while_stmt()?,
                    Keyword::Until => self.parse_until_stmt()?,
                    Keyword::For | Keyword::Foreach => self.parse_for_stmt()?,
                    Keyword::Package => self.parse_package_decl(kw_span)?,
                    Keyword::Use | Keyword::No => self.parse_use_decl(kw_span, kw == Keyword::No)?,

                    // Phaser blocks
                    Keyword::BEGIN => self.parse_phaser(PhaserKind::Begin)?,
                    Keyword::END => self.parse_phaser(PhaserKind::End)?,
                    Keyword::INIT => self.parse_phaser(PhaserKind::Init)?,
                    Keyword::CHECK => self.parse_phaser(PhaserKind::Check)?,
                    Keyword::UNITCHECK => self.parse_phaser(PhaserKind::Unitcheck)?,
                    Keyword::ADJUST => self.parse_phaser(PhaserKind::Adjust)?,

                    // AUTOLOAD/DESTROY — implicit sub declarations.  `AUTOLOAD { ... }` is `sub AUTOLOAD { ... }`.
                    // `AUTOLOAD;` is `sub AUTOLOAD;` (forward decl).  They are NEVER function calls — Perl always
                    // treats them as implicit sub declarations.
                    Keyword::AUTOLOAD | Keyword::DESTROY => {
                        let name: &str = kw.into();
                        self.parse_sub_decl_with_name(name.to_string(), kw_span)?
                    }

                    // given/when/default
                    Keyword::Given => self.parse_given()?,
                    Keyword::When => self.parse_when()?,
                    Keyword::Default => {
                        let block = self.parse_block(true)?;
                        StmtKind::When(Expr::new(ExprKind::IntLit(1), kw_span), block)
                    }

                    // try/catch/finally/defer
                    Keyword::Try => self.parse_try()?,
                    Keyword::Defer => {
                        let block = self.parse_block(true)?;
                        StmtKind::Defer(block)
                    }

                    // format NAME = ... .
                    Keyword::Format => self.parse_format(kw_span)?,
                    // class Name :attrs { ... }
                    Keyword::Class => self.parse_class(kw_span)?,
                    // field $var :attrs = default;
                    Keyword::Field => self.parse_field(kw_span)?,

                    // method name(params) { ... } or method { ... }
                    Keyword::Method => {
                        if matches!(self.tok.token, Token::Ident(_)) {
                            self.parse_method(kw_span)?
                        } else {
                            let expr = self.parse_anon_method(kw_span)?;
                            return self.finish_expr_stmt(expr, start);
                        }
                    }

                    // Any other statement keyword not handled above.
                    _ => unreachable!("unhandled statement keyword: {kw:?}"),
                }
            }

            // '{' at statement level — parse as block, then check if it should be reclassified as a hash constructor.
            Token::LeftBrace => {
                // Consume '{' and parse block without expecting opener.
                self.tok = self.lex_token()?;
                let block = self.parse_block(false)?;
                match Self::try_reclassify_as_hash(block) {
                    // Reclassified as hash constructor.  Continue as an expression statement: check for postfix
                    // control flow and optional semicolon.
                    Ok(hash_expr) => return self.finish_expr_stmt(hash_expr, start),
                    Err(block) => {
                        let cont = if self.tok.token == Token::Keyword(Keyword::Continue) {
                            self.tok = self.lex_token()?;
                            Some(self.parse_block(true)?)
                        } else {
                            None
                        };
                        StmtKind::Block(block, cont)
                    }
                }
            }

            // Identifier: could be a label (IDENT:) or start of expression.
            Token::Ident(_) => {
                // Consume via next_token() for zero-clone ownership of the name String.
                let ident_span = self.tok.span;
                let Token::Ident(name) = self.next_token()?.token else { unreachable!() };
                if self.tok.token == Token::Colon {
                    // Label: consume ':' and parse the labeled statement.
                    self.tok = self.lex_token()?;
                    let stmt = self.parse_statement()?;
                    StmtKind::Labeled(name, Box::new(stmt))
                } else {
                    // Expression starting with an identifier.
                    let initial = self.parse_ident_term(name, ident_span)?;
                    let expr = self.parse_expr_continuation(initial, PREC_LOW)?;
                    return self.finish_expr_stmt(expr, start);
                }
            }

            // Expression keywords (local, return, etc.) and non-keywords go through parse_expr.
            _ => {
                let expr = self.parse_expr(PREC_LOW)?;
                return self.finish_expr_stmt(expr, start);
            }
        };

        Ok(Statement { kind, span: start.merge(self.tok.span), terminated: false })
    }

    /// Build a terminated expression statement: apply postfix control flow, observe and consume optional semicolon.
    fn finish_expr_stmt(&mut self, expr: Expr, start: Span) -> Result<Statement, ParseError> {
        let kind = self.maybe_postfix_control(expr)?;
        let terminated = self.tok.token == Token::Semi;
        if terminated {
            self.tok = self.lex_token()?;
        }
        Ok(Statement { kind, span: start.merge(self.tok.span), terminated })
    }

    fn maybe_postfix_control(&mut self, expr: Expr) -> Result<StmtKind, ParseError> {
        match &self.tok.token {
            Token::Keyword(Keyword::If) => {
                self.tok = self.lex_token()?;
                let cond = self.parse_expr(PREC_LOW)?;
                let span = expr.span.merge(cond.span);
                Ok(StmtKind::Expr(Expr::postfix_control(PostfixKind::If, expr, cond, span)))
            }
            Token::Keyword(Keyword::Unless) => {
                self.tok = self.lex_token()?;
                let cond = self.parse_expr(PREC_LOW)?;
                let span = expr.span.merge(cond.span);
                Ok(StmtKind::Expr(Expr::postfix_control(PostfixKind::Unless, expr, cond, span)))
            }
            Token::Keyword(Keyword::While) => {
                self.tok = self.lex_token()?;
                let cond = self.parse_expr(PREC_LOW)?;
                let span = expr.span.merge(cond.span);
                Ok(StmtKind::Expr(Expr::postfix_control(PostfixKind::While, expr, cond, span)))
            }
            Token::Keyword(Keyword::Until) => {
                self.tok = self.lex_token()?;
                let cond = self.parse_expr(PREC_LOW)?;
                let span = expr.span.merge(cond.span);
                Ok(StmtKind::Expr(Expr::postfix_control(PostfixKind::Until, expr, cond, span)))
            }
            Token::Keyword(Keyword::For) | Token::Keyword(Keyword::Foreach) => {
                self.tok = self.lex_token()?;
                let list = self.parse_expr(PREC_LOW)?;
                let span = expr.span.merge(list.span);
                Ok(StmtKind::Expr(Expr::postfix_control(PostfixKind::For, expr, list, span)))
            }
            Token::Keyword(Keyword::When) => {
                self.tok = self.lex_token()?;
                let cond = self.parse_expr(PREC_LOW)?;
                let span = expr.span.merge(cond.span);
                Ok(StmtKind::Expr(Expr::postfix_control(PostfixKind::When, expr, cond, span)))
            }
            _ => Ok(StmtKind::Expr(expr)),
        }
    }

    // ── Variable declarations ─────────────────────────────────
    fn parse_single_var_decl(&mut self) -> Result<VarDecl, ParseError> {
        let span = self.tok.span;

        // `my \$x` / `my \@a` / `my \%h` — reference declaration (declared_refs, 5.26+).  Only honored when the
        // feature is active; otherwise `\` would be an unexpected token here.
        let is_ref = if self.pragmas.features.contains(Features::DECLARED_REFS) && self.tok.token == Token::Backslash {
            self.tok = self.lex_token()?;
            true
        } else {
            false
        };

        match &self.tok.token {
            Token::ScalarVar(_) => {
                let Token::ScalarVar(name) = self.next_token()?.token else { unreachable!() };
                Ok(VarDecl { sigil: Sigil::Scalar, name, span, attributes: vec![], is_ref })
            }
            Token::ArrayVar(_) => {
                let Token::ArrayVar(name) = self.next_token()?.token else { unreachable!() };
                Ok(VarDecl { sigil: Sigil::Array, name, span, attributes: vec![], is_ref })
            }
            Token::HashVar(_) => {
                let Token::HashVar(name) = self.next_token()?.token else { unreachable!() };
                Ok(VarDecl { sigil: Sigil::Hash, name, span, attributes: vec![], is_ref })
            }
            Token::Percent => {
                // lex_hash_var_after_percent reads raw bytes right after % — must be called before lex_token.
                match self.lex_hash_var_after_percent()? {
                    Some(Token::HashVar(name)) => {
                        self.tok = self.lex_token()?;
                        Ok(VarDecl { sigil: Sigil::Hash, name, span, attributes: vec![], is_ref })
                    }
                    Some(Token::SpecialHashVar(name)) => {
                        self.tok = self.lex_token()?;
                        Ok(VarDecl { sigil: Sigil::Hash, name, span, attributes: vec![], is_ref })
                    }
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
        let name = match &self.tok.token {
            Token::Ident(_) => {
                let Token::Ident(name) = self.next_token()?.token else { unreachable!() };
                name
            }
            // Keywords are valid sub names: `sub send { }`, `sub print { }`.
            Token::Keyword(kw) => {
                let name = <&str>::from(*kw).to_string();
                self.tok = self.lex_token()?;
                name
            }
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
        if self.tok.token == Token::Semi {
            let attr_names: Vec<String> = attributes.iter().map(|a| a.name.clone()).collect();
            self.symbols.entry(&self.current_package.clone()).declare_sub(&name, prototype_parsed, attr_names, true);
            // Represent as a SubDecl with an empty body for now; an optional `body: None` variant would be cleaner,
            // but that's a separate AST change.
            let span = start.merge(self.tok.span);
            self.tok = self.lex_token()?;
            let body = Block { statements: Vec::new(), span };
            return Ok(StmtKind::SubDecl(SubDecl { name, scope: None, prototype: prototype_raw, attributes, signature, body, span }));
        }

        let body = self.parse_block(true)?;
        // Register the full definition, replacing any prior forward declaration of the same name.
        let attr_names: Vec<String> = attributes.iter().map(|a| a.name.clone()).collect();
        self.symbols.entry(&self.current_package.clone()).declare_sub(&name, prototype_parsed, attr_names, false);

        Ok(StmtKind::SubDecl(SubDecl { name, scope: None, prototype: prototype_raw, attributes, signature, body, span: start.merge(self.tok.span) }))
    }

    /// Parse an optional prototype: `($$)`, `(\@\%)`, etc.  If `(` follows, consume it and scan the body as raw bytes
    /// until `)`, matching toke.c's `scan_str()` call in `yyl_sub()`.
    fn parse_prototype(&mut self) -> Result<Option<String>, ParseError> {
        if self.tok.token == Token::LeftParen {
            let proto = self.lex_body_str('(', FrameRole::Prototype)?;
            {
                self.tok = self.lex_token()?;
                Ok(Some(proto))
            }
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
        if self.tok.token != Token::LeftParen {
            return Ok(None);
        }
        let start_span = self.tok.span;
        self.tok = self.lex_token()?;
        let mut params = Vec::new();

        // Track the span of the first slurpy parameter (if any) so we can reject anything that follows it.
        let mut slurpy_span: Option<Span> = None;

        loop {
            if self.tok.token == Token::RightParen {
                break;
            }
            let param = self.parse_sig_param()?;

            // Reject params after a slurpy.
            if let Some(sp) = slurpy_span {
                let offending = match &param {
                    SigParam::Scalar { span, .. }
                    | SigParam::SlurpyArray { span, .. }
                    | SigParam::SlurpyHash { span, .. }
                    | SigParam::AnonScalar { span, .. }
                    | SigParam::AnonArray { span }
                    | SigParam::AnonHash { span } => *span,
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
            if self.tok.token == Token::Comma {
                self.tok = self.lex_token()?;
            } else {
                break;
            }
            // Trailing comma is allowed.
        }
        let close_span = self.tok.span;
        self.expect(&Token::RightParen)?;
        Ok(Some(Signature { params, span: start_span.merge(close_span) }))
    }

    /// Parse one signature parameter.  Handles the parser/lexer interplay for sigils:
    ///
    /// * `Token::ScalarVar(name)` / `Token::ArrayVar(name)` arrive pre-combined because the lexer greedily consumes
    ///   `$ident` / `@ident`.
    /// * `Token::HashVar` does NOT arrive pre-combined — the lexer always emits `Token::Percent` and the parser opts
    ///   in via `lex_hash_var_after_percent()` when in term position.  We do that here.
    /// * Bare `$`/`@`/`%` (followed by a non-identifier) arrive as `Token::Dollar` / `Token::At` / `Token::Percent`
    ///   respectively, and mean anonymous placeholders.
    /// * `$,`/`$)`/`$;` and similar get eagerly lexed as `Token::SpecialVar(c)` because those are real punctuation
    ///   variables.  In a signature, `$` followed by a separator is an anonymous scalar; we split the SpecialVar back
    ///   into a `Dollar` + synthetic delimiter.
    fn parse_sig_param(&mut self) -> Result<SigParam, ParseError> {
        // Intercept `SpecialVar(c)` where `c` is a signature separator or `=` — splits into anon scalar + delimiter.
        if let Token::SpecialVar(ref name) = self.tok.token
            && name.len() == 1
            && matches!(name.as_bytes()[0], b',' | b')' | b'=')
        {
            let span = self.tok.span;
            let delim_byte = name.as_bytes()[0];
            if delim_byte == b'=' {
                // `$=` — nameless optional.  Check for default expr.
                self.tok = self.lex_token()?;
                if matches!(self.tok.token, Token::RightParen | Token::Comma) {
                    // `$=)` or `$=,` — no default expression.
                    return Ok(SigParam::AnonScalar { default: Some((SigDefaultKind::Eq, Expr::new(ExprKind::Undef, span))), span });
                }
                // `$ = expr` — has default expression.
                let default_expr = self.parse_expr(PREC_COMMA + 1)?;
                return Ok(SigParam::AnonScalar { default: Some((SigDefaultKind::Eq, default_expr)), span: span.merge(self.tok.span) });
            }
            let delim_tok = match delim_byte {
                b',' => Token::Comma,
                b')' => Token::RightParen,
                _ => unreachable!(),
            };
            self.tok = Spanned { token: delim_tok, span: Span::new(span.end - 1, span.end) };
            return Ok(SigParam::AnonScalar { default: None, span });
        }

        let span = self.tok.span;
        match &self.tok.token {
            Token::ScalarVar(_) => {
                let Token::ScalarVar(name) = self.next_token()?.token else { unreachable!() };
                let default = self.parse_sig_default()?;
                Ok(SigParam::Scalar { name, default, span: span.merge(self.tok.span) })
            }
            Token::ArrayVar(_) => {
                let Token::ArrayVar(name) = self.next_token()?.token else { unreachable!() };
                Ok(SigParam::SlurpyArray { name, span })
            }
            Token::Dollar => {
                // Bare `$` — anonymous scalar.  May have a default.
                self.tok = self.lex_token()?;
                let default = self.parse_sig_default()?;
                Ok(SigParam::AnonScalar { default, span: span.merge(self.tok.span) })
            }
            Token::At => {
                self.tok = self.lex_token()?;
                Ok(SigParam::AnonArray { span })
            }
            Token::Percent => {
                // Either anon hash placeholder or named slurpy hash; ask the lexer to probe for a hash name.
                match self.lex_hash_var_after_percent()? {
                    Some(Token::HashVar(name)) => {
                        self.tok = self.lex_token()?;
                        Ok(SigParam::SlurpyHash { name, span })
                    }
                    Some(Token::SpecialHashVar(_)) | Some(_) => {
                        // `%^FOO` etc. — not valid in a signature.
                        Err(ParseError::new("special hash variable not allowed in signature", span))
                    }
                    None => {
                        self.tok = self.lex_token()?;
                        Ok(SigParam::AnonHash { span })
                    }
                }
            }
            other => Err(ParseError::new(format!("expected signature parameter, got {other:?}"), span)),
        }
    }

    /// Parse an optional default value in a signature: `= expr`, `//= expr`, `||= expr`.
    fn parse_sig_default(&mut self) -> Result<Option<(SigDefaultKind, Expr)>, ParseError> {
        let kind = match &self.tok.token {
            Token::Assign(AssignOp::Eq) => SigDefaultKind::Eq,
            Token::Assign(AssignOp::DefinedOrEq) => SigDefaultKind::DefinedOr,
            Token::Assign(AssignOp::OrEq) => SigDefaultKind::LogicalOr,
            _ => return Ok(None),
        };
        self.tok = self.lex_token()?;
        let expr = self.parse_expr(PREC_COMMA + 1)?;
        Ok(Some((kind, expr)))
    }

    fn parse_attributes(&mut self) -> Result<Vec<Attribute>, ParseError> {
        let mut attrs = Vec::new();
        while self.tok.token == Token::Colon {
            let attr_start = self.tok.span;
            self.tok = self.lex_token()?;
            // Attribute names can be identifiers or keywords (e.g. :method, :lvalue)
            let name = match &self.tok.token {
                Token::Ident(s) => Some(s.clone()),
                Token::Keyword(kw) => Some(<&str>::from(*kw).to_string()),
                _ => None,
            };
            if let Some(name) = name {
                let name_span = self.tok.span;
                self.tok = self.lex_token()?;
                // Optional parenthesized args.  For `:prototype(...)` specifically, the body is Perl prototype syntax
                // (containing `$`, `@`, `%`, `\`, etc.) which must be read as raw bytes — token-by-token
                // reconstruction via Display impls loses fidelity.  Other attributes use the general
                // token-reconstruction path.
                let value = if self.tok.token == Token::LeftParen {
                    if name == "prototype" {
                        let proto = self.lex_body_str('(', FrameRole::Prototype)?;
                        self.tok = self.lex_token()?;
                        Some(proto)
                    } else {
                        self.tok = self.lex_token()?;
                        let mut args = String::new();
                        let mut depth = 1u32;
                        loop {
                            match &self.tok.token {
                                Token::LeftParen => {
                                    depth += 1;
                                    args.push('(');
                                    self.tok = self.lex_token()?;
                                }
                                Token::RightParen => {
                                    depth -= 1;
                                    if depth == 0 {
                                        self.tok = self.lex_token()?;
                                        break;
                                    }
                                    args.push(')');
                                    self.tok = self.lex_token()?;
                                }
                                Token::Eof => break,
                                _ => {
                                    args.push_str(&format!("{}", self.tok.token));
                                    self.tok = self.lex_token()?;
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

        if self.tok.token == Token::LeftParen {
            // List form: my ($x, @y, %z)
            self.tok = self.lex_token()?;
            while self.tok.token != Token::RightParen && self.tok.token != Token::Eof {
                let var = self.parse_single_var_decl()?;
                vars.push(var);
                if self.tok.token == Token::Comma {
                    self.tok = self.lex_token()?;
                } else {
                    break;
                }
            }
            let end = self.tok.span;
            self.expect(&Token::RightParen)?;
            // Wrap the list form in a transient grouping `Paren` so the parens-fact rides the same machinery as any
            // other parenthesized lvalue: the `=` arm reads it (`my ($x) = ...` is a list assignment) and unwraps,
            // and a standalone `my ($a, $b);` is unwrapped to a bare `Decl` by the caller.  `Paren` never persists.
            let full = span.merge(end);
            let decl = Expr::new(ExprKind::Decl(scope, vars), full);
            Ok(Expr::new(ExprKind::Paren(Box::new(decl)), full))
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

    /// Parse an anonymous sub expression: `sub { ... }`, `sub ($x) { ... }`, `sub :lvalue { ... }`, etc.
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

        let body = self.parse_block(true)?;
        let span = span.merge(body.span);
        Ok(Expr::anon_sub(prototype, attributes, signature, body, span))
    }

    fn parse_anon_method(&mut self, span: Span) -> Result<Expr, ParseError> {
        // Methods always act as if signatures are in effect.
        let attrs = self.parse_attributes()?;
        let sig = self.parse_signature()?;
        let body = self.parse_block(true)?;
        let span = span.merge(body.span);
        Ok(Expr::anon_method(attrs, sig, body, span))
    }

    // ── Control flow statements ───────────────────────────────
    fn parse_if_stmt(&mut self) -> Result<StmtKind, ParseError> {
        let condition = self.parse_paren_expr()?;
        let then_block = self.parse_block(true)?;

        let mut elsif_clauses = Vec::new();
        while self.tok.token == Token::Keyword(Keyword::Elsif) {
            self.tok = self.lex_token()?;
            let cond = self.parse_paren_expr()?;
            let block = self.parse_block(true)?;
            elsif_clauses.push((cond, block));
        }

        // Catch common mistake: `elseif` instead of `elsif`.
        if self.tok.token == Token::Keyword(Keyword::Elseif) {
            return Err(ParseError::new("elseif should be elsif", self.tok.span));
        }

        let else_block = if self.tok.token == Token::Keyword(Keyword::Else) {
            self.tok = self.lex_token()?;
            let block = self.parse_block(true)?;
            Some(block)
        } else {
            None
        };

        Ok(StmtKind::If(IfStmt { condition, then_block, elsif_clauses, else_block }))
    }

    fn parse_unless_stmt(&mut self) -> Result<StmtKind, ParseError> {
        let condition = self.parse_paren_expr()?;
        let then_block = self.parse_block(true)?;

        let mut elsif_clauses = Vec::new();
        while self.tok.token == Token::Keyword(Keyword::Elsif) {
            self.tok = self.lex_token()?;
            let cond = self.parse_paren_expr()?;
            let block = self.parse_block(true)?;
            elsif_clauses.push((cond, block));
        }

        // Catch common mistake: `elseif` instead of `elsif`.
        if self.tok.token == Token::Keyword(Keyword::Elseif) {
            return Err(ParseError::new("elseif should be elsif", self.tok.span));
        }

        let else_block = if self.tok.token == Token::Keyword(Keyword::Else) {
            self.tok = self.lex_token()?;
            let block = self.parse_block(true)?;
            Some(block)
        } else {
            None
        };
        Ok(StmtKind::Unless(UnlessStmt { condition, then_block, elsif_clauses, else_block }))
    }

    fn parse_while_stmt(&mut self) -> Result<StmtKind, ParseError> {
        let condition = self.parse_paren_expr()?;
        let body = self.parse_block(true)?;
        let continue_block = if self.tok.token == Token::Keyword(Keyword::Continue) {
            self.tok = self.lex_token()?;
            Some(self.parse_block(true)?)
        } else {
            None
        };
        Ok(StmtKind::While(WhileStmt { condition, body, continue_block }))
    }

    fn parse_until_stmt(&mut self) -> Result<StmtKind, ParseError> {
        let condition = self.parse_paren_expr()?;
        let body = self.parse_block(true)?;
        let continue_block = if self.tok.token == Token::Keyword(Keyword::Continue) {
            self.tok = self.lex_token()?;
            Some(self.parse_block(true)?)
        } else {
            None
        };
        Ok(StmtKind::Until(UntilStmt { condition, body, continue_block }))
    }

    fn parse_for_stmt(&mut self) -> Result<StmtKind, ParseError> {
        // If next is a variable, 'my', or '\' (refaliasing), it's foreach-style.
        if matches!(self.tok.token, Token::Keyword(Keyword::My) | Token::ScalarVar(_) | Token::Backslash) {
            return self.parse_foreach_body();
        }

        // Consume '(' then decide: C-style or foreach based on whether a `;` appears after the first expression.
        self.expect(&Token::LeftParen)?;

        // Empty init (`;` immediately) → definitely C-style.
        if self.tok.token == Token::Semi {
            return self.parse_c_style_for_body(None);
        }

        // Parse the first expression.
        let first = self.parse_expr(PREC_LOW)?;

        // Semicolon after first expression → C-style, first was init.
        if self.tok.token == Token::Semi {
            return self.parse_c_style_for_body(Some(first));
        }

        // No semicolon → foreach-style, first was the list.
        self.expect(&Token::RightParen)?;
        let body = self.parse_block(true)?;
        let continue_block = if self.tok.token == Token::Keyword(Keyword::Continue) {
            self.tok = self.lex_token()?;
            Some(self.parse_block(true)?)
        } else {
            None
        };

        Ok(StmtKind::ForEach(ForEachStmt { vars: vec![], list: first, body, continue_block }))
    }

    /// Parse the rest of a C-style for loop after `(` and the optional init expression have been consumed.  Next token
    /// should be `;`.
    fn parse_c_style_for_body(&mut self, init: Option<Expr>) -> Result<StmtKind, ParseError> {
        self.expect(&Token::Semi)?;

        // condition (may be empty)
        let condition = if self.tok.token == Token::Semi {
            None
        } else {
            let expr = self.parse_expr(PREC_LOW)?;
            Some(expr)
        };
        self.expect(&Token::Semi)?;

        // step (may be empty)
        let step = if self.tok.token == Token::RightParen {
            None
        } else {
            let expr = self.parse_expr(PREC_LOW)?;
            Some(expr)
        };
        self.expect(&Token::RightParen)?;

        let body = self.parse_block(true)?;

        Ok(StmtKind::For(ForStmt { init, condition, step, body }))
    }

    fn parse_foreach_body(&mut self) -> Result<StmtKind, ParseError> {
        let vars = if self.tok.token == Token::Keyword(Keyword::My) {
            self.tok = self.lex_token()?;
            if self.tok.token == Token::LeftParen {
                // `for my ($x, $y, $z) (LIST)` — multi-variable (5.36+).
                self.tok = self.lex_token()?;
                let mut vars = Vec::new();
                loop {
                    let vd = self.parse_single_var_decl()?;
                    vars.push(vd);
                    if self.tok.token == Token::Comma {
                        self.tok = self.lex_token()?;
                    } else {
                        break;
                    }
                }
                self.expect(&Token::RightParen)?;
                vars
            } else {
                // `for my $x (LIST)` — single variable.
                let vd = self.parse_single_var_decl()?;
                vec![vd]
            }
        } else if self.tok.token == Token::Backslash {
            // `for \my $x (LIST)` — refaliasing (experimental).
            self.tok = self.lex_token()?;
            self.expect(&Token::Keyword(Keyword::My))?;
            let mut vd = self.parse_single_var_decl()?;
            vd.is_ref = true;
            vec![vd]
        } else if matches!(self.tok.token, Token::ScalarVar(_)) {
            let span = self.tok.span;
            let Token::ScalarVar(name) = self.next_token()?.token else { unreachable!() };
            vec![VarDecl { sigil: Sigil::Scalar, name, span, attributes: vec![], is_ref: false }]
        } else {
            vec![]
        };

        let list = self.parse_paren_expr()?;
        let body = self.parse_block(true)?;
        let continue_block = if self.tok.token == Token::Keyword(Keyword::Continue) {
            self.tok = self.lex_token()?;
            Some(self.parse_block(true)?)
        } else {
            None
        };

        Ok(StmtKind::ForEach(ForEachStmt { vars, list, body, continue_block }))
    }

    // ── Package and use ───────────────────────────────────────
    fn parse_package_decl(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        let name = match &self.tok.token {
            Token::Ident(_) => {
                let Token::Ident(n) = self.next_token()?.token else { unreachable!() };
                n
            }
            Token::Keyword(kw) => {
                let n = <&str>::from(*kw).to_string();
                self.tok = self.lex_token()?;
                n
            }
            other => return Err(ParseError::new(format!("expected package name, got {other:?}"), start)),
        };

        // Optional version
        let version = if matches!(self.tok.token, Token::IntLit(_) | Token::FloatLit(_) | Token::VersionLit(_)) {
            let v = format!("{}", self.tok.token);
            self.tok = self.lex_token()?;
            Some(v)
        } else {
            None
        };

        // Ensure the package exists in the symbol table, even if empty — so later references resolve correctly.
        let _ = self.symbols.entry(&name);

        let block = if self.tok.token == Token::LeftBrace {
            // Block form: `package Name { ... }` — switch packages for the duration of the block, then restore.
            let saved = std::mem::replace(&mut self.current_package, std::sync::Arc::from(name.as_str()));
            let block = self.parse_block(true)?;
            self.current_package = saved;
            Some(block)
        } else {
            // Statement form: `package Name;` — switch packages for everything that follows.
            if self.tok.token == Token::Semi {
                self.tok = self.lex_token()?;
            }
            self.current_package = std::sync::Arc::from(name.as_str());
            None
        };

        Ok(StmtKind::PackageDecl(PackageDecl { name, version, block, span: start.merge(self.tok.span) }))
    }

    fn parse_use_decl(&mut self, start: Span, is_no: bool) -> Result<StmtKind, ParseError> {
        // First argument: either a version (use 5.020) or a module name.
        let module = match &self.tok.token {
            Token::Ident(n) => n.clone(),

            // Bare version: `use 5.020;` / `use v5.36;` — module slot gets the version; no further version or imports.
            Token::IntLit(n) => {
                let n = *n;
                // Apply the matching bundle to pragma state.  `use 5.036` / `use 5036` → major=5, minor=36.
                if !is_no && let Some((maj, min)) = parse_int_version(n) {
                    self.pragmas.features.apply_version_bundle(maj, min);
                }
                self.set_utf8_mode(self.pragmas.utf8);
                self.features = self.pragmas.features;
                self.tok = self.lex_token()?;
                if self.tok.token == Token::Semi {
                    self.tok = self.lex_token()?;
                }
                return Ok(StmtKind::UseDecl(UseDecl { is_no, module: format!("{n}"), version: None, imports: None, span: start.merge(self.tok.span) }));
            }
            Token::FloatLit(n) => {
                let n = *n;
                // `use 5.036` can also lex as FloatLit (5.036).
                if !is_no && let Some((maj, min)) = parse_float_version(n) {
                    self.pragmas.features.apply_version_bundle(maj, min);
                }
                self.set_utf8_mode(self.pragmas.utf8);
                self.features = self.pragmas.features;
                self.tok = self.lex_token()?;
                if self.tok.token == Token::Semi {
                    self.tok = self.lex_token()?;
                }
                return Ok(StmtKind::UseDecl(UseDecl { is_no, module: format!("{n}"), version: None, imports: None, span: start.merge(self.tok.span) }));
            }
            Token::VersionLit(n) => {
                // v-string: `use v5.36;` — arrives as VersionLit.
                if !is_no && let Some((maj, min)) = parse_v_string_version(n) {
                    self.pragmas.features.apply_version_bundle(maj, min);
                }
                n.clone()
            }
            Token::StrLit(n) => n.clone(),

            // Keywords can be module names: `use if`, `use open`, etc.
            Token::Keyword(kw) => <&str>::from(*kw).to_string(),
            other => return Err(ParseError::new(format!("expected module name or version, got {other:?}"), start)),
        };

        self.tok = self.lex_token()?;
        // Optional version after the module name: `use Module 1.23;` or `use Module v5.26;`.
        let version = match &self.tok.token {
            Token::IntLit(n) => {
                let v = format!("{n}");
                self.tok = self.lex_token()?;
                Some(v)
            }
            Token::FloatLit(n) => {
                let v = format!("{n}");
                self.tok = self.lex_token()?;
                Some(v)
            }
            // v-string literal like v5.26.0.
            Token::VersionLit(s) => {
                let v = s.clone();
                self.tok = self.lex_token()?;
                Some(v)
            }
            _ => None,
        };

        // Optional import list: anything until the semicolon.
        let imports = if matches!(self.tok.token, Token::Semi | Token::Eof) {
            None
        } else {
            let mut items = Vec::new();
            loop {
                if matches!(self.tok.token, Token::Semi | Token::Eof) {
                    break;
                }
                let expr = self.parse_expr(PREC_COMMA + 1)?;
                items.push(expr);
                if self.tok.token == Token::Comma || self.tok.token == Token::FatComma {
                    self.tok = self.lex_token()?;
                } else {
                    break;
                }
            }
            Some(items)
        };

        // Pragma dispatch: apply any side effects to parser state before returning.  Unknown modules and non-pragma
        // imports are silently ignored here; they'd require runtime module loading to take effect.
        apply_pragma(&mut self.pragmas, &module, is_no, imports.as_ref());

        // Sync shared UTF-8 flag — the lexer reads this to decide whether to accept multi-byte identifiers.
        self.set_utf8_mode(self.pragmas.utf8);
        self.features = self.pragmas.features;

        // Consume the trailing semicolon AFTER syncing features — the next token may be a feature-gated keyword (e.g.
        // `say` after `use feature 'say';`) and must be lexed with the updated feature set.
        if self.tok.token == Token::Semi {
            self.tok = self.lex_token()?;
        }

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

        Ok(StmtKind::UseDecl(UseDecl { is_no, module, version, imports, span: start.merge(self.tok.span) }))
    }

    // ── Phaser blocks ─────────────────────────────────────────
    fn parse_phaser(&mut self, kind: PhaserKind) -> Result<StmtKind, ParseError> {
        let block = self.parse_block(true)?;
        Ok(StmtKind::Phaser(kind, block))
    }

    // ── given/when ────────────────────────────────────────────
    fn parse_given(&mut self) -> Result<StmtKind, ParseError> {
        let expr = self.parse_paren_expr()?;
        let block = self.parse_block(true)?;
        Ok(StmtKind::Given(expr, block))
    }

    fn parse_when(&mut self) -> Result<StmtKind, ParseError> {
        let expr = self.parse_paren_expr()?;
        let block = self.parse_block(true)?;
        Ok(StmtKind::When(expr, block))
    }

    // ── try/catch/finally ─────────────────────────────────────
    fn parse_try(&mut self) -> Result<StmtKind, ParseError> {
        let body = self.parse_block(true)?;

        let (catch_var, catch_block) = if self.tok.token == Token::Keyword(Keyword::Catch) {
            self.tok = self.lex_token()?;
            let var = if self.tok.token == Token::LeftParen {
                self.tok = self.lex_token()?;
                let decl = self.parse_single_var_decl()?;
                self.expect(&Token::RightParen)?;
                Some(decl)
            } else {
                None
            };
            let block = self.parse_block(true)?;
            (var, Some(block))
        } else {
            (None, None)
        };

        let finally_block = if self.tok.token == Token::Keyword(Keyword::Finally) {
            self.tok = self.lex_token()?;
            Some(self.parse_block(true)?)
        } else {
            None
        };

        Ok(StmtKind::Try(TryStmt { body, catch_var, catch_block, finally_block }))
    }

    // ── format ────────────────────────────────────────────────
    fn parse_format(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        // Optional name (defaults to STDOUT).
        let name = match &self.tok.token {
            Token::Ident(_) => {
                let Token::Ident(n) = self.next_token()?.token else { unreachable!() };
                n
            }
            // Keywords are valid format names: `format send = ...`.
            Token::Keyword(kw) => {
                let n = <&str>::from(*kw).to_string();
                self.tok = self.lex_token()?;
                n
            }
            _ => "STDOUT".to_string(),
        };

        // Verify `=` without lexing past it — start_format changes the lexer mode, so the token after `=` must be
        // lexed in format mode, not regular mode.
        if self.tok.token != Token::Assign(AssignOp::Eq) {
            return Err(ParseError::new(format!("expected =, got {}", self.tok.token), self.tok.span));
        }

        // Hand off to the lexer's format sublex mode.  The next token will be FormatSublexBegin; the body ends at
        // SublexEnd (emitted for the `.` terminator).
        //
        // Careful: do NOT call lex_token here for the begin_span — that would invoke the lexer and potentially tokenize
        // into the first body line, which `start_format` would then discard.  Build the begin span from `start` and the
        // current (pre-body) lexer position instead.
        let here = self.pos() as u32;
        let begin_span = start.merge(Span::new(here, here));
        self.start_format(name.clone(), begin_span);

        // Consume the FormatSublexBegin.
        let begin = self.lex_token()?;
        if !matches!(begin.token, Token::FormatSublexBegin(_)) {
            return Err(ParseError::new(format!("expected FormatSublexBegin, got {:?}", begin.token), begin.span));
        }

        // Read format lines until SublexEnd.
        let mut lines = Vec::new();
        loop {
            self.tok = self.lex_token()?;
            match &self.tok.token {
                Token::SublexEnd => break,
                Token::FormatComment(text) => lines.push(FormatLine::Comment { text: text.clone(), span: self.tok.span }),
                Token::FormatBlankLine => lines.push(FormatLine::Blank { span: self.tok.span }),
                Token::FormatLiteralLine(repeat, text) => lines.push(FormatLine::Literal { repeat: *repeat, text: text.clone(), span: self.tok.span }),
                Token::FormatPictureBegin(repeat) => lines.push(self.parse_format_picture(*repeat, self.tok.span)?),
                other => return Err(ParseError::new(format!("unexpected token in format body: {other:?}"), self.tok.span)),
            }
        }

        self.tok = self.lex_token()?;
        Ok(StmtKind::FormatDecl(FormatDecl { name, lines, span: start.merge(self.tok.span) }))
    }

    /// Parse a picture line after `FormatPictureBegin(repeat)` has been consumed.  Consumes tokens until
    /// `FormatPictureEnd`, then the following `FormatArgsBegin` / expressions / `FormatArgsEnd` group, and assembles a
    /// `FormatLine::Picture`.
    fn parse_format_picture(&mut self, repeat: RepeatKind, begin_span: Span) -> Result<FormatLine, ParseError> {
        let mut parts = Vec::new();
        loop {
            self.tok = self.lex_token()?;
            match &self.tok.token {
                Token::FormatPictureEnd => break,
                Token::FormatLiteral(text) => parts.push(FormatPart::Literal(text.clone())),
                Token::FormatField(kind) => parts.push(FormatPart::Field(FormatField { kind: *kind, span: self.tok.span })),
                other => return Err(ParseError::new(format!("unexpected token in picture line: {other:?}"), self.tok.span)),
            }
        }

        // Expect FormatArgsBegin.
        let args_begin = self.lex_token()?;
        if !matches!(args_begin.token, Token::FormatArgsBegin) {
            return Err(ParseError::new(format!("expected FormatArgsBegin, got {:?}", args_begin.token), args_begin.span));
        }

        // Peek: if '{' is the first args token, consume it and switch the lexer to braced mode.
        self.tok = self.lex_token()?;
        let braced = self.tok.token == Token::LeftBrace;
        if braced {
            self.format_args_enter_braced();
            self.tok = self.lex_token()?;
        }

        // Parse comma-separated expressions until FormatArgsEnd.
        let mut args = Vec::new();
        while self.tok.token != Token::FormatArgsEnd {
            // Defensive: surface EOF / unexpected termination.
            if self.tok.token == Token::Eof {
                return Err(ParseError::new("unexpected EOF inside format arguments", self.tok.span));
            }
            let expr = self.parse_expr(PREC_COMMA + 1)?;
            args.push(expr);
            if self.tok.token == Token::Comma {
                self.tok = self.lex_token()?;
            } else {
                break;
            }
        }

        // Consume FormatArgsEnd.
        if !matches!(self.tok.token, Token::FormatArgsEnd) {
            return Err(ParseError::new(format!("expected FormatArgsEnd, got {:?}", self.tok.token), self.tok.span));
        }

        Ok(FormatLine::Picture { repeat, parts, args, span: begin_span.merge(self.tok.span) })
    }

    // ── class / field / method (5.38+ Corinna) ────────────────
    fn parse_class(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        let name = match &self.tok.token {
            Token::Ident(_) => {
                let Token::Ident(n) = self.next_token()?.token else { unreachable!() };
                n
            }
            other => return Err(ParseError::new(format!("expected class name, got {other:?}"), start)),
        };

        // Optional version (like package).
        let version = if matches!(self.tok.token, Token::IntLit(_) | Token::FloatLit(_) | Token::VersionLit(_)) {
            let v = format!("{}", self.tok.token);
            self.tok = self.lex_token()?;
            Some(v)
        } else {
            None
        };

        let attributes = self.parse_attributes()?;

        // Block form or statement form (like package).
        let body = if self.tok.token == Token::LeftBrace {
            let b = self.parse_block(true)?;
            Some(b)
        } else {
            if self.tok.token == Token::Semi {
                self.tok = self.lex_token()?;
            }
            None
        };

        Ok(StmtKind::ClassDecl(ClassDecl { name: name.clone(), version, attributes, body, span: start.merge(self.tok.span) }))
    }

    fn parse_field(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        let var = self.parse_single_var_decl()?;
        let attributes = self.parse_attributes()?;

        let default = match &self.tok.token {
            Token::Assign(AssignOp::Eq) => {
                self.tok = self.lex_token()?;
                let expr = self.parse_expr(PREC_COMMA)?;
                Some((SigDefaultKind::Eq, expr))
            }
            Token::Assign(AssignOp::DefinedOrEq) => {
                self.tok = self.lex_token()?;
                let expr = self.parse_expr(PREC_COMMA)?;
                Some((SigDefaultKind::DefinedOr, expr))
            }
            Token::Assign(AssignOp::OrEq) => {
                self.tok = self.lex_token()?;
                let expr = self.parse_expr(PREC_COMMA)?;
                Some((SigDefaultKind::LogicalOr, expr))
            }
            _ => None,
        };

        if self.tok.token == Token::Semi {
            self.tok = self.lex_token()?;
        }
        Ok(StmtKind::FieldDecl(FieldDecl { var, attributes, default, span: start.merge(self.tok.span) }))
    }

    fn parse_method(&mut self, start: Span) -> Result<StmtKind, ParseError> {
        let name = match &self.tok.token {
            Token::Ident(_) => {
                let Token::Ident(n) = self.next_token()?.token else { unreachable!() };
                n
            }
            // Keywords are valid method names: `method send { }`.
            Token::Keyword(kw) => {
                let n = <&str>::from(*kw).to_string();
                self.tok = self.lex_token()?;
                n
            }
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

        let body = self.parse_block(true)?;
        Ok(StmtKind::MethodDecl(SubDecl { name, scope: None, prototype, attributes, signature, body, span: start.merge(self.tok.span) }))
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
    /// Parse a brace-delimited block with lexical pragma scoping.  If `expect_open` is true, consumes a '{' first; if
    /// false, the caller has already consumed the opening brace.  The block always ends at '}'; `Eof` is an error.
    fn parse_block(&mut self, expect_open: bool) -> Result<Block, ParseError> {
        self.parse_scoped_block(expect_open, true)
    }

    /// Parse the brace-less statement sequence a bounded-code frame delivers — the `/e` replacement body — up to the
    /// `SublexEnd` that marks the frame's virtual EOF.  Pragmas are lexically scoped as in `parse_block` (a `use`
    /// inside the replacement does not leak out).  `SublexEnd` and `Eof` are interchangeable terminators: only one can
    /// occur, and for a sublex context the delimiters were already matched before setting up the frame.
    fn parse_eval_block(&mut self) -> Result<Block, ParseError> {
        self.parse_scoped_block(false, false)
    }

    /// Shared pragma-scoping wrapper for `parse_block` and `parse_eval_block`.
    fn parse_scoped_block(&mut self, expect_open: bool, expect_close_brace: bool) -> Result<Block, ParseError> {
        // Pragmas and current_package are lexically scoped: any `use feature`, `use utf8`, or `package Name;` inside
        // this block doesn't leak out.  Save state before parsing, restore after.
        let saved_pragmas = self.pragmas;
        let saved_package = self.current_package.clone();

        let result = self.parse_block_inner(expect_open, expect_close_brace);

        self.pragmas = saved_pragmas;
        self.current_package = saved_package;
        // Sync shared state with the restored lexical scope.
        self.set_utf8_mode(self.pragmas.utf8);
        self.features = self.pragmas.features;
        result
    }

    fn parse_block_inner(&mut self, expect_open: bool, expect_close_brace: bool) -> Result<Block, ParseError> {
        let start = self.tok.span;
        if expect_open {
            self.expect(&Token::LeftBrace)?;
        }
        let mut statements = Vec::new();
        if expect_close_brace {
            // Brace-delimited block: '}' terminates, `Eof` is an error.
            while !matches!(self.tok.token, Token::RightBrace | Token::Eof) {
                statements.push(self.parse_statement()?);
            }
            let end = self.tok.span;
            if self.tok.token == Token::RightBrace {
                self.tok = self.lex_token()?;
            } else {
                return Err(ParseError::new("unterminated block", start));
            }
            Ok(Block { statements, span: start.merge(end) })
        } else {
            // Sublex block (`/e` body): `SublexEnd` or `Eof` terminates.  No unterminated check — the delimiters
            // were already matched before the sublex frame was set up.
            while !matches!(self.tok.token, Token::SublexEnd | Token::Eof) {
                statements.push(self.parse_statement()?);
            }
            let end = self.tok.span;
            if self.tok.token == Token::SublexEnd {
                self.tok = self.lex_token()?;
            }
            Ok(Block { statements, span: start.merge(end) })
        }
    }

    /// Finish an `s///` once its pattern is collected: set up the replacement frame, then parse the replacement either
    /// as an interpolated template or — for `/e` — as a code block valued at its last statement.  The `e`-count becomes
    /// `SubstReplacement::Eval`'s `evals` and is stripped from the stored flags so the eval depth lives in one place.
    fn finish_subst(&mut self, delim: char, pattern: Interpolated, span: Span) -> Result<Expr, ParseError> {
        let flags = self.start_subst_replacement(delim)?;
        if let Some(ref f) = flags {
            Self::validate_subst_flags(f, span)?;
        }
        let evals = flags.as_ref().map_or(0, |f| f.bytes().filter(|&b| b == b'e').count() as u32);
        let replacement = if evals > 0 {
            self.tok = self.lex_token()?;
            let block = self.parse_eval_block()?;
            SubstReplacement::Eval { block, evals }
        } else {
            // Lex the first replacement body token, then collect the interpolated body.
            self.tok = self.lex_token()?;
            let interp = self.parse_interpolated()?;
            self.tok = self.lex_token()?;
            SubstReplacement::Interp(interp)
        };
        let flags = flags.map(|f| f.chars().filter(|&c| c != 'e').collect::<String>()).filter(|f| !f.is_empty());
        Ok(Expr::new(ExprKind::Subst(pattern, replacement, flags), span.merge(self.tok.span)))
    }

    fn parse_paren_expr(&mut self) -> Result<Expr, ParseError> {
        self.expect(&Token::LeftParen)?;
        let expr = self.parse_expr(PREC_LOW)?;
        self.expect(&Token::RightParen)?;
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
            let expr = self.parse_expr_continuation(left, current_prec)?;
            left = expr;
            match stack.pop() {
                // Stack empty: this is the finished top-level expression.  A standalone `($x)` still carries a
                // transient `Paren` here (no operator consumed it), so unwrap it — grouping parens never persist.
                None => return Ok(Self::unwrap_paren(left)),
                Some(frame) => match self.apply_expr_frame(frame, left)? {
                    FrameResult::Done(expr, prec) => {
                        left = expr;
                        current_prec = prec;
                    }
                    FrameResult::Continue(frame, inner_prec) => {
                        // Accumulator frame needs more elements — re-push and re-enter forward phase.
                        stack.push(frame);
                        current_prec = inner_prec;
                        let expr = self.parse_expr_forward(&mut stack, &mut current_prec)?;
                        left = expr;
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

    /// Try to consume a prefix operator or opening paren.  Returns `(Some(Frame/Leaf), next_tok)` if consumed, or
    /// `(None)` if the token is not a prefix — returning it unconsumed for parse_term.
    fn try_prefix(&mut self, outer_prec: Precedence) -> Result<Option<PrefixResult>, ParseError> {
        let span = self.tok.span;
        match &self.tok.token {
            Token::LeftParen => {
                self.tok = self.lex_token()?;
                if self.tok.token == Token::RightParen {
                    self.tok = self.lex_token()?;
                    let expr = Expr::new(ExprKind::EmptyList, span);
                    let expr = self.maybe_postfix_subscript(expr)?;
                    return Ok(Some(PrefixResult::Leaf(expr)));
                }
                Ok(Some(PrefixResult::Frame(ExprFrame::Paren { span, min_prec: outer_prec }, PREC_LOW)))
            }
            Token::Minus => {
                // Filetest: -f, -d, etc.  In fat-comma context, lex_filetest_after_minus returns StrLit.
                match self.lex_filetest_after_minus() {
                    Some(Token::Filetest(b)) => {
                        let end_pos = self.pos() as u32;
                        self.tok = self.lex_token()?;
                        let expr = self.parse_filetest(b, Span::new(span.start, end_pos))?;
                        return Ok(Some(PrefixResult::Leaf(expr)));
                    }
                    Some(Token::StrLit(s)) => {
                        return {
                            self.tok = self.lex_token()?;
                            Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::StringLit(s), span))))
                        };
                    }
                    _ => {}
                }

                // Not a filetest.  Lex the next token.
                self.tok = self.lex_token()?;

                // -bareword (not followed by parens) → StringLit("-bareword")
                if matches!(self.tok.token, Token::Ident(_)) {
                    let ident_span = self.tok.span;
                    let Token::Ident(name) = self.next_token()?.token else { unreachable!() };
                    if self.tok.token == Token::LeftParen {
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
                self.tok = self.lex_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::Unary { op: UnaryOp::NumPositive, span, min_prec: outer_prec }, PREC_UNARY)))
            }
            Token::Bang => {
                self.tok = self.lex_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::Unary { op: UnaryOp::LogNot, span, min_prec: outer_prec }, PREC_UNARY)))
            }
            Token::Tilde => {
                self.tok = self.lex_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::Unary { op: UnaryOp::BitNot, span, min_prec: outer_prec }, PREC_UNARY)))
            }
            Token::StringBitNot => {
                self.tok = self.lex_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::Unary { op: UnaryOp::StringBitNot, span, min_prec: outer_prec }, PREC_UNARY)))
            }
            Token::Backslash => {
                self.tok = self.lex_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::Ref { span, min_prec: outer_prec }, PREC_UNARY)))
            }
            Token::Keyword(Keyword::Not) => {
                self.tok = self.lex_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::Unary { op: UnaryOp::Not, span, min_prec: outer_prec }, PREC_NOT_LOW)))
            }
            Token::PlusPlus => {
                self.tok = self.lex_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::PreIncDec { op: UnaryOp::PreInc, span, min_prec: outer_prec }, PREC_INC)))
            }
            Token::MinusMinus => {
                self.tok = self.lex_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::PreIncDec { op: UnaryOp::PreDec, span, min_prec: outer_prec }, PREC_INC)))
            }
            Token::Keyword(Keyword::Local) => {
                self.tok = self.lex_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::Local { span, min_prec: outer_prec }, PREC_UNARY)))
            }
            Token::LeftBracket => {
                self.tok = self.lex_token()?;
                if self.tok.token == Token::RightBracket {
                    return {
                        self.tok = self.lex_token()?;
                        Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::AnonArray(vec![]), span))))
                    };
                }
                Ok(Some(PrefixResult::Frame(ExprFrame::ArrayRef { elems: vec![], span, min_prec: outer_prec }, PREC_COMMA + 1)))
            }
            Token::LeftBrace => {
                self.tok = self.lex_token()?;
                if self.tok.token == Token::RightBrace {
                    return {
                        self.tok = self.lex_token()?;
                        Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::AnonHash(vec![]), span))))
                    };
                }
                Ok(Some(PrefixResult::Frame(ExprFrame::HashRef { elems: vec![], span, min_prec: outer_prec }, PREC_COMMA + 1)))
            }

            // ── Sigil-prefix dereference ──
            // '${expr}', '$$ref' → Token::Dollar; '@{expr}', '@$ref' → Token::At; etc.
            // The {expr} path pushes a DerefBlock frame; everything else is a complete Leaf.
            Token::Dollar => {
                self.tok = self.lex_token()?;
                if self.tok.token == Token::LeftBrace {
                    {
                        self.tok = self.lex_token()?;
                        Ok(Some(PrefixResult::Frame(ExprFrame::DerefBlock { sigil: Sigil::Scalar, span, min_prec: outer_prec }, PREC_LOW)))
                    }
                } else {
                    let operand = self.parse_deref_operand()?;
                    let span = span.merge(operand.span);
                    let expr = Expr::deref(Sigil::Scalar, operand, span);
                    let expr = self.maybe_postfix_subscript(expr)?;
                    Ok(Some(PrefixResult::Leaf(expr)))
                }
            }
            Token::At => {
                self.tok = self.lex_token()?;
                if self.tok.token == Token::LeftBrace {
                    {
                        self.tok = self.lex_token()?;
                        Ok(Some(PrefixResult::Frame(ExprFrame::DerefBlock { sigil: Sigil::Array, span, min_prec: outer_prec }, PREC_LOW)))
                    }
                } else {
                    let operand = self.parse_deref_operand()?;
                    let span = span.merge(operand.span);
                    Ok(Some(PrefixResult::Leaf(Expr::deref(Sigil::Array, operand, span))))
                }
            }
            Token::Percent => {
                // Position is past `%`.  Try hash var before lexing the next token.
                match self.lex_hash_var_after_percent()? {
                    Some(Token::HashVar(name)) => {
                        let recv = Expr::new(ExprKind::HashVar(name), span);
                        self.tok = self.lex_token()?;
                        let expr = self.maybe_kv_slice(recv, span)?;
                        Ok(Some(PrefixResult::Leaf(expr)))
                    }
                    Some(Token::SpecialHashVar(name)) => {
                        self.tok = self.lex_token()?;
                        Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::SpecialHashVar(name), span))))
                    }
                    Some(other) => unreachable!("unexpected hash token: {other:?}"),
                    None => {
                        self.tok = self.lex_token()?;
                        if self.tok.token == Token::LeftBrace {
                            self.tok = self.lex_token()?;
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
                self.tok = self.lex_token()?;
                if self.tok.token == Token::LeftBrace {
                    self.tok = self.lex_token()?;
                    Ok(Some(PrefixResult::Frame(ExprFrame::DerefBlock { sigil: Sigil::Code, span, min_prec: outer_prec }, PREC_LOW)))
                } else if matches!(self.tok.token, Token::Ident(_)) {
                    let name_span = self.tok.span;
                    let Token::Ident(name) = self.next_token()?.token else { unreachable!() };
                    if self.tok.token == Token::LeftParen {
                        self.tok = self.lex_token()?;
                        let mut args = Vec::new();
                        while self.tok.token != Token::RightParen && self.tok.token != Token::Eof {
                            let expr = self.parse_expr(PREC_COMMA + 1)?;
                            args.push(expr);
                            if self.tok.token == Token::Comma {
                                self.tok = self.lex_token()?;
                            } else {
                                break;
                            }
                        }
                        let end = self.tok.span;
                        self.expect(&Token::RightParen)?;
                        Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::FuncCall(self.qualify_sub_name(&name), args), span.merge(end)))))
                    } else {
                        Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::FuncCall(self.qualify_sub_name(&name), vec![]), span.merge(name_span)))))
                    }
                } else {
                    let operand = self.parse_deref_operand()?;
                    let span = span.merge(operand.span);
                    let deref = Expr::deref(Sigil::Code, operand, span);
                    let expr = self.maybe_call_args(deref)?;
                    Ok(Some(PrefixResult::Leaf(expr)))
                }
            }
            Token::Star => {
                self.tok = self.lex_token()?;
                if self.tok.token == Token::LeftBrace {
                    self.tok = self.lex_token()?;
                    Ok(Some(PrefixResult::Frame(ExprFrame::DerefBlock { sigil: Sigil::Glob, span, min_prec: outer_prec }, PREC_LOW)))
                } else if matches!(self.tok.token, Token::Ident(_)) {
                    let name_span = self.tok.span;
                    let Token::Ident(name) = self.next_token()?.token else { unreachable!() };
                    let expr = Expr::new(ExprKind::GlobVar(name), span.merge(name_span));
                    if self.tok.token == Token::LeftBrace {
                        let key = self.parse_hash_subscript_key()?;
                        let end = self.tok.span;
                        self.expect(&Token::RightBrace)?;
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
                self.tok = self.lex_token()?;
                if self.tok.token == Token::LeftBrace {
                    let block = self.parse_block(true)?;
                    let span = span.merge(block.span);
                    Ok(Some(PrefixResult::Leaf(Expr::eval_block(block, span))))
                } else {
                    Ok(Some(PrefixResult::Frame(ExprFrame::EvalExpr { span, min_prec: outer_prec }, PREC_COMMA)))
                }
            }
            Token::Keyword(Keyword::Do) => {
                self.tok = self.lex_token()?;
                if self.tok.token == Token::LeftBrace {
                    let block = self.parse_block(true)?;
                    let span = span.merge(block.span);
                    Ok(Some(PrefixResult::Leaf(Expr::do_block(block, span))))
                } else {
                    Ok(Some(PrefixResult::Frame(ExprFrame::DoExpr { span, min_prec: outer_prec }, PREC_UNARY)))
                }
            }
            Token::Keyword(Keyword::Return) => {
                self.tok = self.lex_token()?;
                if matches!(self.tok.token, Token::Semi | Token::RightBrace | Token::Eof) {
                    Ok(Some(PrefixResult::Leaf(Expr::new(ExprKind::Return(None), span))))
                } else {
                    Ok(Some(PrefixResult::Frame(ExprFrame::ReturnExpr { span, min_prec: outer_prec }, PREC_COMMA)))
                }
            }
            Token::Keyword(Keyword::Goto) => {
                self.tok = self.lex_token()?;
                Ok(Some(PrefixResult::Frame(ExprFrame::GotoExpr { span, min_prec: outer_prec }, PREC_COMMA)))
            }
            Token::Keyword(Keyword::Dump) => {
                self.tok = self.lex_token()?;
                if matches!(self.tok.token, Token::Eof | Token::Semi) {
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
        // Grouping parens are transparent to a combined operand, so unwrap a transient `Paren` before applying the
        // frame: this collapses `(($x))` to depth one and keeps grouping out of the finished tree (`-(x)` → `-x`,
        // `[($x)]` → `[$x]`).  `Ref` is the exception — refgen reads the parens-fact off its operand, so it must see
        // the `Paren` (the read-then-unwrap lives in the `Ref` arm).
        let operand = if matches!(frame, ExprFrame::Ref { .. }) { operand } else { Self::unwrap_paren(operand) };
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
                // Refgen distinguishes `\@a` (reference the container whole) from `\(@a)` (flatten and reference each
                // element) by the parens-fact alone — the operand node is identical otherwise.  This frame is carved
                // out of the top-level operand unwrap, so the transient `Paren` is still here to read: stamp the
                // operand's context from it, then unwrap.  No parens → `Scalar`; parens → `List`.  The rule is parens-
                // only (unlike the `=` arm's parens-or-aggregate), since `\@a` is a whole-array reference despite @a
                // being an aggregate.  `save_context` leaves the `Ref` arm deferred, so this construction-time stamp is
                // the sole writer of the operand's context and survives the descent; lowering later combines this tag
                // with the operand's container-ness (§6.2.5).
                let parenthesized = matches!(operand.kind, ExprKind::Paren(_));
                let mut operand = Self::unwrap_paren(operand);
                operand.save_context(if parenthesized { Context::List } else { Context::Scalar });
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
                // Wrap the operand in a *transient* `Paren` carrying the parens-fact that `=`, refgen, and the
                // list slice need.  Nesting is capped at depth one by the `unwrap_paren` above.  `Paren` never
                // persists in a finished tree: every consumer unwraps it at the point of combination.
                let end = self.tok.span;
                self.expect(&Token::RightParen)?;
                let span = span.merge(end);
                let paren = Expr::new(ExprKind::Paren(Box::new(operand)), span);
                let expr = self.maybe_postfix_subscript(paren)?;
                Ok(FrameResult::Done(expr, min_prec))
            }
            ExprFrame::ArrayRef { mut elems, span, min_prec } => {
                elems.push(operand);
                if self.tok.token == Token::Comma {
                    self.tok = self.lex_token()?;
                    if self.tok.token == Token::RightBracket {
                        // Trailing comma: `[1, 2, 3,]`
                        let end = self.tok.span;
                        return {
                            self.tok = self.lex_token()?;
                            Ok(FrameResult::Done(Expr::new(ExprKind::AnonArray(elems), span.merge(end)), min_prec))
                        };
                    }
                    // More elements — re-enter forward phase.
                    return Ok(FrameResult::Continue(ExprFrame::ArrayRef { elems, span, min_prec }, PREC_COMMA + 1));
                }
                let end = self.tok.span;
                self.expect(&Token::RightBracket)?;
                Ok(FrameResult::Done(Expr::new(ExprKind::AnonArray(elems), span.merge(end)), min_prec))
            }
            ExprFrame::HashRef { mut elems, span, min_prec } => {
                elems.push(operand);
                if self.tok.token == Token::Comma || self.tok.token == Token::FatComma {
                    self.tok = self.lex_token()?;
                    if self.tok.token == Token::RightBrace {
                        // Trailing comma: `{a => 1, b => 2,}`
                        let end = self.tok.span;
                        return {
                            self.tok = self.lex_token()?;
                            Ok(FrameResult::Done(Expr::new(ExprKind::AnonHash(elems), span.merge(end)), min_prec))
                        };
                    }
                    // More elements — re-enter forward phase.
                    return Ok(FrameResult::Continue(ExprFrame::HashRef { elems, span, min_prec }, PREC_COMMA + 1));
                }
                let end = self.tok.span;
                self.expect(&Token::RightBrace)?;
                Ok(FrameResult::Done(Expr::new(ExprKind::AnonHash(elems), span.merge(end)), min_prec))
            }
            ExprFrame::DerefBlock { sigil, span, min_prec } => {
                let end = self.tok.span;
                self.expect(&Token::RightBrace)?;
                let span = span.merge(end);
                let expr = Expr::deref(sigil, operand, span);
                let expr = match sigil {
                    Sigil::Scalar => self.maybe_postfix_subscript(expr)?,
                    Sigil::Code => self.maybe_call_args(expr)?,
                    _ => expr,
                };
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
                Ok(FrameResult::Done(Expr::new(ExprKind::Return(Some(Box::new(operand))), end), min_prec))
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

    /// Continue parsing an expression from a pre-built left-hand side.  Runs the Pratt operator loop: lex_operator
    /// transforms the token for operator context (x-repeat split), op_info_for_token maps it to precedence/associativity,
    /// and the loop continues while the operator binds tightly enough.  Returns the expression and the first unconsumed
    /// token.
    fn parse_expr_continuation(&mut self, mut left: Expr, min_prec: Precedence) -> Result<Expr, ParseError> {
        let sm = self.features.contains(Features::SMARTMATCH);
        loop {
            self.lex_operator()?;
            let Some(info) = Self::op_info_for_token(&self.tok.token, sm) else { break };
            if info.left_prec() < min_prec {
                break;
            }
            let expr = self.parse_operator(left, info)?;
            left = expr;
        }
        Ok(left)
    }

    // ── Term parsing ──────────────────────────────────────────
    fn parse_term(&mut self) -> Result<Expr, ParseError> {
        self.lex_term()?;
        let span = self.tok.span;

        // Weak keyword override: if this keyword has been declared as a sub (via `use subs`, `sub name;`, etc.), treat
        // it as an identifier in term position so the user sub takes precedence.  Infix position is unaffected — `"ab"
        // x 3` always works as repeat.
        if let Token::Keyword(kw) = &self.tok.token
            && keyword::is_weak(*kw)
        {
            let name: &str = (*kw).into();
            if self.symbols.lookup(name, &self.current_package).is_some() {
                let name = name.to_string();
                self.next_token()?;
                return self.parse_ident_term(name, span);
            }
        }

        // Consume the current token.  All arms below work with owned values from the consumed token; self.tok already
        // holds the following token.
        let consumed = self.next_token()?;
        match consumed.token {
            Token::IntLit(n) => Ok(Expr::new(ExprKind::IntLit(n), span)),
            Token::FloatLit(n) => Ok(Expr::new(ExprKind::FloatLit(n), span)),
            Token::StrLit(s) => Ok(Expr::new(ExprKind::StringLit(s), span)),
            Token::VersionLit(s) => Ok(Expr::new(ExprKind::VersionLit(s), span)),

            // Interpolating string: collect sub-tokens into AST.
            Token::QuoteSublexBegin(_, _) => self.parse_interpolated_string(span),

            Token::ScalarVar(name) => {
                let expr = Expr::new(ExprKind::ScalarVar(name), span);
                self.maybe_postfix_subscript(expr)
            }
            Token::ArrayVar(name) => {
                // @array[0,1] → array slice; @array{qw(a b)} → hash slice
                if self.tok.token == Token::LeftBracket {
                    self.tok = self.lex_token()?;
                    let mut indices = Vec::new();
                    while self.tok.token != Token::RightBracket && self.tok.token != Token::Eof {
                        let expr = self.parse_expr(PREC_COMMA + 1)?;
                        indices.push(expr);
                        if self.tok.token == Token::Comma {
                            self.tok = self.lex_token()?;
                        } else {
                            break;
                        }
                    }
                    let end = self.tok.span;
                    self.expect(&Token::RightBracket)?;
                    Ok(Expr::new(ExprKind::ArraySlice(Box::new(Expr::new(ExprKind::ArrayVar(name), span)), indices), span.merge(end)))
                } else if self.tok.token == Token::LeftBrace {
                    self.tok = self.lex_token()?;
                    let mut keys = Vec::new();
                    while self.tok.token != Token::RightBrace && self.tok.token != Token::Eof {
                        let expr = self.parse_expr(PREC_COMMA + 1)?;
                        keys.push(expr);
                        if self.tok.token == Token::Comma {
                            self.tok = self.lex_token()?;
                        } else {
                            break;
                        }
                    }
                    let end = self.tok.span;
                    self.expect(&Token::RightBrace)?;
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
                if self.tok.token == Token::LeftBrace {
                    let key = self.parse_hash_subscript_key()?;
                    let end = self.tok.span;
                    self.expect(&Token::RightBrace)?;
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

            // Compile-time special literals.  SourceFile/SourceLine carry lex-time values; __PACKAGE__ is resolved
            // from the parser's state.  __SUB__ and __CLASS__ are feature-gated.
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
            Token::Keyword(kw @ (Keyword::My | Keyword::Our | Keyword::State)) => {
                let scope = match kw {
                    Keyword::My => DeclScope::My,
                    Keyword::Our => DeclScope::Our,
                    Keyword::State => DeclScope::State,
                    _ => unreachable!(),
                };
                self.parse_decl_expr(scope, span)
            }

            // Anonymous sub: sub { ... } or sub ($x) { ... }
            Token::Keyword(Keyword::Sub) => self.parse_anon_sub(span),
            // Anonymous method: method { ... } or method ($x) { ... }
            Token::Keyword(Keyword::Method) => self.parse_anon_method(span),

            // last/next/redo with optional label
            Token::Keyword(kw @ (Keyword::Last | Keyword::Next | Keyword::Redo)) => {
                let name = match kw {
                    Keyword::Last => "CORE::last",
                    Keyword::Next => "CORE::next",
                    Keyword::Redo => "CORE::redo",
                    _ => unreachable!(),
                };
                // Optional label argument
                if matches!(self.tok.token, Token::Ident(_)) {
                    let label_span = self.tok.span;
                    let Token::Ident(label) = self.next_token()?.token else { unreachable!() };
                    let end = span.merge(label_span);
                    Ok(Expr::new(ExprKind::FuncCall(name.into(), vec![Expr::new(ExprKind::StringLit(label), label_span)]), end))
                } else {
                    Ok(Expr::new(ExprKind::FuncCall(name.into(), vec![]), span))
                }
            }

            // break — exits a given/when block.  No label argument.
            Token::Keyword(Keyword::Break) => Ok(Expr::new(ExprKind::FuncCall("CORE::break".into(), vec![]), span)),
            // continue — falls through to the next when in a given block.
            Token::Keyword(Keyword::Continue) => Ok(Expr::new(ExprKind::FuncCall("CORE::continue".into(), vec![]), span)),

            // `x` is a weak keyword: in prefix position it acts as an identifier (function call / bareword).
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

            // `//` in term position is an empty regex, not defined-or.
            Token::EmptyRegex(flags) => Ok(Expr::new(ExprKind::Regex(RegexKind::Match, None, flags), span)),

            // Regex, substitution, transliteration
            Token::RegexSublexBegin(kind, _delim) => {
                let pattern = self.parse_interpolated()?;
                let flags = self.scan_adjacent_word_chars();
                if let Some(ref f) = flags {
                    Self::validate_regex_flags(f, span)?;
                }
                self.tok = self.lex_token()?;
                Ok(Expr::new(ExprKind::Regex(kind, Some(pattern), flags), span))
            }

            Token::SubstSublexBegin(delim) => {
                // Collect pattern body tokens until SublexEnd, then finish the replacement.
                let pattern = self.parse_interpolated()?;
                self.finish_subst(delim, pattern, span)
            }
            Token::TranslitLit(from, to, flags) => {
                if let Some(ref f) = flags {
                    Self::validate_tr_flags(f, span)?;
                }
                Ok(Expr::new(ExprKind::Translit(from, to, flags), span))
            }

            // sort/map/grep with optional block
            Token::Keyword(kw) if keyword::is_block_list_op(kw) => self.parse_block_list_op(kw, span),
            // print/say with optional filehandle
            Token::Keyword(kw) if keyword::is_print_op(kw) => self.parse_print_op(kw, span),
            // Filetest operators: -e, -f, -d, etc. (lexed as single token)
            Token::Filetest(test_byte) => self.parse_filetest(test_byte, span),
            // Yada yada yada (...)
            Token::ThreeDots => Ok(Expr::new(ExprKind::YadaYada, span)),
            // Readline / diamond: <STDIN>, <>, <$fh>, <*.txt>
            Token::Readline(content, safe) => {
                let expr = Self::readline_expr(content, safe, span)?;
                Ok(expr)
            }

            other => Err(ParseError::new(format!("expected expression, got {other:?}"), span)),
        }
    }

    fn parse_ident_term(&mut self, name: String, span: Span) -> Result<Expr, ParseError> {
        // Look up in the symbol table to see if this is a known sub.  Clone the prototype (small: raw string + a Vec
        // of slot enums) and the "is known" flag so we can release the borrow on self before parsing args.
        let (is_known_sub, proto) = match self.symbols.lookup(&name, &self.current_package) {
            Some(info) => (true, info.prototype.clone()),
            None => (false, None),
        };

        // Check if followed by `(` — function call
        if self.tok.token == Token::LeftParen {
            self.tok = self.lex_token()?;
            let mut args = Vec::new();
            while self.tok.token != Token::RightParen && self.tok.token != Token::Eof {
                let expr = self.parse_expr(PREC_COMMA + 1)?;
                args.push(expr);
                if self.tok.token == Token::Comma {
                    self.tok = self.lex_token()?;
                } else {
                    break;
                }
            }
            let end = self.tok.span;
            self.expect(&Token::RightParen)?;
            return Ok(Expr::new(ExprKind::FuncCall(self.qualify_sub_name(&name), args), span.merge(end)));
        }

        // No parens — if we know this sub has a prototype, use it to drive argument parsing.
        if let Some(proto) = proto {
            return self.parse_prototyped_call(self.qualify_sub_name(&name), span, &proto);
        }

        // No parens, no prototype, but the sub is known: parse as a list operator call (greedy args until
        // end-of-statement).
        if is_known_sub {
            return self.parse_known_sub_call(self.qualify_sub_name(&name), span);
        }

        // Indirect object syntax: METHOD CLASS ARGS
        // e.g. new Foo(args), new Foo args
        // Heuristic: bareword followed by a capitalized bareword or $var.
        // Requires `use feature 'indirect'` (in :default, dropped from :5.36+).
        if self.pragmas.features.contains(Features::INDIRECT) {
            match &self.tok.token {
                Token::Ident(class_name) if class_name.starts_with(|c: char| c.is_ascii_uppercase()) => {
                    let class_name = class_name.clone();
                    let class_span = self.tok.span;
                    let class_expr = Expr::new(ExprKind::Bareword(class_name), class_span);

                    self.tok = self.lex_token()?;
                    let mut args = Vec::new();
                    if self.tok.token == Token::LeftParen {
                        self.tok = self.lex_token()?;
                        while self.tok.token != Token::RightParen && self.tok.token != Token::Eof {
                            let expr = self.parse_expr(PREC_COMMA + 1)?;
                            args.push(expr);
                            if self.tok.token == Token::Comma {
                                self.tok = self.lex_token()?;
                            } else {
                                break;
                            }
                        }
                        let end = self.tok.span;
                        self.expect(&Token::RightParen)?;
                        return Ok(Expr::new(ExprKind::IndirectMethodCall(Box::new(class_expr), name, args), span.merge(end)));
                    }
                    return Ok(Expr::new(ExprKind::IndirectMethodCall(Box::new(class_expr), name, args), span.merge(class_span)));
                }
                Token::ScalarVar(_) => {
                    let var_span = self.tok.span;
                    let Token::ScalarVar(var) = self.next_token()?.token else { unreachable!() };
                    let invocant = Expr::new(ExprKind::ScalarVar(var), var_span);

                    let mut args = Vec::new();
                    if self.tok.token == Token::LeftParen {
                        self.tok = self.lex_token()?;
                        while self.tok.token != Token::RightParen && self.tok.token != Token::Eof {
                            let expr = self.parse_expr(PREC_COMMA + 1)?;
                            args.push(expr);
                            if self.tok.token == Token::Comma {
                                self.tok = self.lex_token()?;
                            } else {
                                break;
                            }
                        }
                        let end = self.tok.span;
                        self.expect(&Token::RightParen)?;
                        return Ok(Expr::new(ExprKind::IndirectMethodCall(Box::new(invocant), name, args), span.merge(end)));
                    }
                    return Ok(Expr::new(ExprKind::IndirectMethodCall(Box::new(invocant), name, args), span.merge(var_span)));
                }
                _ => {}
            }
        }

        // Bare identifier — not followed by ( or indirect object context.
        Ok(Expr::new(ExprKind::Bareword(name), span))
    }

    /// True if the current token marks the end of a list-op / prototyped argument list: statement terminator, closing
    /// bracket/brace/paren, EOF, or a postfix-control keyword.
    fn at_args_end(&self) -> bool {
        matches!(
            self.tok.token,
            Token::Semi
                | Token::RightParen
                | Token::RightBracket
                | Token::RightBrace
                | Token::Eof
                | Token::Keyword(Keyword::If)
                | Token::Keyword(Keyword::Unless)
                | Token::Keyword(Keyword::While)
                | Token::Keyword(Keyword::Until)
                | Token::Keyword(Keyword::For)
                | Token::Keyword(Keyword::Foreach)
        )
    }

    /// Parse a call to a known sub (no prototype) in list-operator style: greedy comma-separated args until end of
    /// statement.  Produces `FuncCall` (not `ListOp`, which is reserved for built-in list operators like `push`,
    /// `join`).
    fn parse_known_sub_call(&mut self, name: String, start: Span) -> Result<Expr, ParseError> {
        let mut args = Vec::new();
        while !self.at_args_end() {
            let expr = self.parse_expr(PREC_COMMA + 1)?;
            args.push(expr);
            if self.tok.token == Token::Comma {
                self.tok = self.lex_token()?;
            } else {
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
            if self.at_args_end() {
                // No more input.  The `_` slot is special: when omitted, it defaults to the global default
                // variable ($_), regardless of required/optional status.  All other slots simply stop.
                if matches!(slot, ProtoSlot::DefaultedScalar) {
                    args.push(Expr::new(ExprKind::DefaultVar, self.tok.span));
                }
                let _ = is_optional;
                break;
            }
            match slot {
                ProtoSlot::Block => {
                    // `&` slot accepts either a literal block (only in initial position — the map/grep/sort
                    // pattern) or a code reference expression.
                    let arg = if i == 0 && self.tok.token == Token::LeftBrace {
                        let block = self.parse_block(true)?;
                        let span = block.span;
                        Expr::anon_sub(None, vec![], None, block, span)
                    } else {
                        self.parse_expr(PREC_NAMED_UNARY)?
                    };
                    args.push(arg);
                    // Optional comma between slots.
                    if i + 1 < proto.slots.len() && self.tok.token == Token::Comma {
                        self.tok = self.lex_token()?;
                    }
                }
                ProtoSlot::SlurpyList | ProtoSlot::SlurpyHash => {
                    // Consume all remaining tokens as comma-separated expressions.  Slurpy is always last.
                    while !self.at_args_end() {
                        let expr = self.parse_expr(PREC_COMMA + 1)?;
                        args.push(expr);
                        if self.tok.token == Token::Comma {
                            self.tok = self.lex_token()?;
                        } else {
                            break;
                        }
                    }
                    break;
                }
                ProtoSlot::Glob => {
                    // `*` slot: a bare identifier is auto-promoted to a typeglob reference (e.g., `foo STDIN`
                    // becomes `foo(*STDIN)`).  Any other expression is parsed at named-unary precedence.
                    let arg = if matches!(self.tok.token, Token::Ident(_)) {
                        let gspan = self.tok.span;
                        let Token::Ident(name) = self.next_token()?.token else { unreachable!() };
                        Expr::new(ExprKind::GlobVar(name), gspan)
                    } else {
                        self.parse_expr(PREC_NAMED_UNARY)?
                    };
                    args.push(arg);
                    if i + 1 < proto.slots.len() && self.tok.token == Token::Comma {
                        self.tok = self.lex_token()?;
                    }
                }
                ProtoSlot::AutoRef(_) | ProtoSlot::AutoRefOneOf(_) | ProtoSlot::ArrayOrHash => {
                    // Auto-reference slots: `\$`, `\@`, `\%`, `\&`, `\*`, `\[...]`, and `+`.  The argument is
                    // parsed at named-unary precedence and then wrapped in a Ref expression.
                    let arg = self.parse_expr(PREC_NAMED_UNARY)?;
                    let span = arg.span;
                    args.push(Expr::reference(arg, span));
                    if i + 1 < proto.slots.len() && self.tok.token == Token::Comma {
                        self.tok = self.lex_token()?;
                    }
                }
                _ => {
                    // Scalar-ish slot (`$`, `_`).  One expression at named-unary precedence.
                    let arg = self.parse_expr(PREC_NAMED_UNARY)?;
                    args.push(arg);
                    if i + 1 < proto.slots.len() && self.tok.token == Token::Comma {
                        self.tok = self.lex_token()?;
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
        if self.tok.token == Token::LeftParen {
            self.tok = self.lex_token()?;
            let end = self.tok.span;
            self.expect(&Token::RightParen)?;
            return Ok(Expr::new(ExprKind::FuncCall(name, vec![]), span.merge(end)));
        }

        // No parens — emit as a zero-arg call; the next token is an operator, not an argument.
        Ok(Expr::new(ExprKind::FuncCall(name, vec![]), span))
    }

    fn parse_named_unary(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let name = Self::core_name(&kw);

        // Named unary with optional arg — check for tokens that indicate "no argument follows."
        if matches!(
            self.tok.token,
            Token::Semi
                | Token::Eof
                | Token::RightBrace
                | Token::RightParen
                | Token::Comma
                | Token::RightBracket
                | Token::Keyword(
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
        ) {
            return Ok(Expr::new(ExprKind::FuncCall(name, vec![]), span));
        }

        // Operators that prefer defined-or: // after shift/pop/undef/etc. is defined-or, not an empty regex
        // argument.  Matches toke.c's XTERMORDORDOR.
        if keyword::prefers_defined_or(kw) && matches!(self.tok.token, Token::DefinedOr) {
            return Ok(Expr::new(ExprKind::FuncCall(name, vec![]), span));
        }

        if self.tok.token == Token::LeftParen {
            self.tok = self.lex_token()?;
            let arg = self.parse_expr(PREC_LOW)?;
            let end = self.tok.span;
            self.expect(&Token::RightParen)?;
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
    /// - No operand (`;`, '}', `)`, EOF) → `StatTarget::Default`
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
        if self.tok.token == Token::LeftParen {
            self.tok = self.lex_token()?;
            let (target, _) = self.parse_stat_target_inner(start)?;
            let end = self.tok.span;
            self.expect(&Token::RightParen)?;
            return Ok((target, end));
        }

        self.parse_stat_target_inner(start)
    }

    /// Inner helper: parse the stat target without handling parens.
    fn parse_stat_target_inner(&mut self, start: Span) -> Result<(StatTarget, Span), ParseError> {
        // No argument: ;, '}', ), EOF, or // (defined-or, not empty regex — matches toke.c's FTST macro).
        if matches!(self.tok.token, Token::Semi | Token::RightBrace | Token::RightParen | Token::Eof | Token::DefinedOr) {
            Ok((StatTarget::Default, start))
        } else if matches!(&self.tok.token, Token::Ident(name) if name == "_") {
            let end = self.tok.span;
            {
                self.tok = self.lex_token()?;
                Ok((StatTarget::StatCache, end))
            }
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
        if self.tok.token == Token::LeftParen {
            self.tok = self.lex_token()?;
            let mut args = Vec::new();
            while self.tok.token != Token::RightParen && self.tok.token != Token::Eof {
                let expr = self.parse_expr(PREC_COMMA + 1)?;
                args.push(expr);
                if self.tok.token == Token::Comma {
                    self.tok = self.lex_token()?;
                } else {
                    break;
                }
            }
            let end = self.tok.span;
            self.expect(&Token::RightParen)?;
            return Ok(Expr::new(ExprKind::ListOp(name, args), span.merge(end)));
        }

        // No parens — parse everything up to end of statement as args
        let mut args = Vec::new();
        while !self.at_args_end() {
            let expr = self.parse_expr(PREC_COMMA + 1)?;
            args.push(expr);
            if self.tok.token == Token::Comma {
                self.tok = self.lex_token()?;
            } else {
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
        if self.tok.token == Token::LeftParen {
            self.tok = self.lex_token()?;
            let mut args = Vec::new();
            // Check for block as first arg inside parens
            if self.tok.token == Token::LeftBrace {
                let block = self.parse_block(true)?;
                let bspan = block.span;
                args.push(Expr::anon_sub(None, vec![], None, block, bspan));
                if self.tok.token == Token::Comma {
                    self.tok = self.lex_token()?;
                }
            }
            while self.tok.token != Token::RightParen && self.tok.token != Token::Eof {
                let expr = self.parse_expr(PREC_COMMA + 1)?;
                args.push(expr);
                if self.tok.token == Token::Comma {
                    self.tok = self.lex_token()?;
                } else {
                    break;
                }
            }
            let end = self.tok.span;
            self.expect(&Token::RightParen)?;
            return Ok(Expr::new(ExprKind::ListOp(name, args), span.merge(end)));
        }

        let mut args = Vec::new();
        // Check for block or sub name as first arg
        if self.tok.token == Token::LeftBrace {
            let block = self.parse_block(true)?;
            let bspan = block.span;
            args.push(Expr::anon_sub(None, vec![], None, block, bspan));
        } else if kw == Keyword::Sort && matches!(self.tok.token, Token::Ident(_)) {
            // sort can also take a sub name: sort subname @list
            let ident_span = self.tok.span;
            let Token::Ident(ident) = self.next_token()?.token else { unreachable!() };
            args.push(Expr::new(ExprKind::Bareword(ident), ident_span));
        }

        while !self.at_args_end() {
            let expr = self.parse_expr(PREC_COMMA + 1)?;
            args.push(expr);
            if self.tok.token == Token::Comma {
                self.tok = self.lex_token()?;
            } else {
                break;
            }
        }

        let end_span = args.last().map(|a| a.span).unwrap_or(span);
        Ok(Expr::new(ExprKind::ListOp(name, args), span.merge(end_span)))
    }
    /// Parse print/say with optional filehandle as first argument.  `print STDERR "error"`, `print "hello"`, `say $fh
    /// "data"`.
    fn parse_print_op(&mut self, kw: Keyword, span: Span) -> Result<Expr, ParseError> {
        let name = Self::core_name(&kw);

        // Handle optional parens — print(...) form
        let in_parens = self.tok.token == Token::LeftParen;
        if in_parens {
            self.tok = self.lex_token()?;
        }

        // Try to detect filehandle before argument list.  Consume-then-decide: take the candidate token, peek at
        // what follows to determine if it's a filehandle or the first argument.
        let mut filehandle: Option<Box<Expr>> = None;
        let mut first_arg: Option<Expr> = None;

        let is_bareword = matches!(self.tok.token, Token::Ident(_)) && self.pragmas.features.contains(Features::BAREWORD_FILEHANDLES);
        let is_scalar = matches!(self.tok.token, Token::ScalarVar(_));

        if is_bareword {
            let fh_span = self.tok.span;
            let Token::Ident(fh_name) = self.next_token()?.token else { unreachable!() };
            if self.tok.token == Token::Comma {
                // Bareword followed by comma → first argument, not filehandle.  `print CONSTANT, "hello"`.
                let initial = self.parse_ident_term(fh_name, fh_span)?;
                let expr = self.parse_expr_continuation(initial, PREC_COMMA + 1)?;
                first_arg = Some(expr);
            } else {
                // Bareword not followed by comma → filehandle.  `print STDERR "hello"`.
                filehandle = Some(Box::new(Expr::new(ExprKind::Bareword(fh_name), fh_span)));
            }
        } else if is_scalar {
            let var_span = self.tok.span;
            let Token::ScalarVar(var_name) = self.next_token()?.token else { unreachable!() };
            let next_is_term = matches!(
                self.tok.token,
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
            if self.tok.token == Token::Comma {
                self.tok = self.lex_token()?;
            } else {
                // No comma — this was the only argument.
                if in_parens {
                    self.expect(&Token::RightParen)?;
                }
                let end_span = args.last().map(|a| a.span).unwrap_or(span);
                return Ok(Expr::new(ExprKind::PrintOp(name, filehandle, args), span.merge(end_span)));
            }
        }
        while !self.at_print_end(in_parens) {
            let expr = self.parse_expr(PREC_COMMA + 1)?;
            args.push(expr);
            if self.tok.token == Token::Comma {
                self.tok = self.lex_token()?;
            } else {
                break;
            }
        }

        if in_parens {
            self.expect(&Token::RightParen)?;
        }

        let end_span = args.last().map(|a| a.span).unwrap_or(span);
        Ok(Expr::new(ExprKind::PrintOp(name, filehandle, args), span.merge(end_span)))
    }

    /// Check whether we're at the end of a print argument list.
    fn at_print_end(&self, in_parens: bool) -> bool {
        if matches!(self.tok.token, Token::Semi | Token::Eof | Token::RightBrace) {
            return true;
        }
        if in_parens && self.tok.token == Token::RightParen {
            return true;
        }
        matches!(
            self.tok.token,
            Token::Keyword(Keyword::If)
                | Token::Keyword(Keyword::Unless)
                | Token::Keyword(Keyword::While)
                | Token::Keyword(Keyword::Until)
                | Token::Keyword(Keyword::For)
                | Token::Keyword(Keyword::Foreach)
        )
    }

    /// Parse the operand of a prefix dereference ($$ref, @$ref, etc.).  Consumes just the variable — subscripts are NOT
    /// included.  This ensures $$ref[0] parses as ($$ref)[0], not $(${ref}[0]).
    fn parse_deref_operand(&mut self) -> Result<Expr, ParseError> {
        let span = self.tok.span;
        let consumed = self.next_token()?;
        match consumed.token {
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
        if self.tok.token == Token::LeftParen {
            self.tok = self.lex_token()?;
            let mut args = Vec::new();
            while self.tok.token != Token::RightParen && self.tok.token != Token::Eof {
                let expr = self.parse_expr(PREC_COMMA + 1)?;
                args.push(expr);
                if self.tok.token == Token::Comma {
                    self.tok = self.lex_token()?;
                } else {
                    break;
                }
            }
            let end = self.tok.span;
            self.expect(&Token::RightParen)?;
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
        // try_autoquoted_subscript_key reads raw bytes right after '{' — no token may be lexed before this call.
        if let Some((name, span)) = self.try_autoquoted_subscript_key() {
            return {
                self.tok = self.lex_token()?;
                Ok(Expr::new(ExprKind::StringLit(name), span))
            };
        }
        self.tok = self.lex_token()?;
        let key = self.parse_expr(PREC_LOW)?;

        // Multidimensional hash emulation: `$h{1,2,3}` → `$h{join($;, 1, 2, 3)}`.  When the feature is off, the
        // comma-list is left as-is for the compiler to diagnose ("Multidimensional hash lookup is disabled").
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
        if self.tok.token == Token::LeftBracket {
            self.tok = self.lex_token()?;
            let mut indices = Vec::new();
            while self.tok.token != Token::RightBracket && self.tok.token != Token::Eof {
                let expr = self.parse_expr(PREC_COMMA + 1)?;
                indices.push(expr);
                if self.tok.token == Token::Comma {
                    self.tok = self.lex_token()?;
                } else {
                    break;
                }
            }
            let end = self.tok.span;
            self.expect(&Token::RightBracket)?;
            Ok(Expr::new(ExprKind::KvArraySlice(Box::new(recv), indices), span.merge(end)))
        } else if self.tok.token == Token::LeftBrace {
            self.tok = self.lex_token()?;
            let mut keys = Vec::new();
            while self.tok.token != Token::RightBrace && self.tok.token != Token::Eof {
                let expr = self.parse_expr(PREC_COMMA + 1)?;
                keys.push(expr);
                if self.tok.token == Token::Comma {
                    self.tok = self.lex_token()?;
                } else {
                    break;
                }
            }
            let end = self.tok.span;
            self.expect(&Token::RightBrace)?;
            Ok(Expr::new(ExprKind::KvHashSlice(Box::new(recv), keys), span.merge(end)))
        } else {
            Ok(recv)
        }
    }

    fn maybe_postfix_subscript(&mut self, mut expr: Expr) -> Result<Expr, ParseError> {
        // Subscript chains: `$x[0][1]`, `$x{a}{b}`, `$x[0]{key}`, and list slices `(LIST)[i]` / `(LIST)[i][j]`.
        //
        // A *list-literal* operand — `(LIST)` (a transient `Paren`), `qw[...]`, or `()` — followed by `[...]` is a
        // list slice, not array-element access on a container.  A list slice's result is itself a list literal, so a
        // chained `[...]` slices again.  Everything else (`$x`, a deref, an element) subscripts as array/hash element.
        loop {
            if self.tok.token == Token::LeftBracket && Self::is_list_literal_operand(&expr) {
                // Unwrap a transient `Paren` to its inner list expression; `qw`/`()`/a prior `ListSlice` are
                // used as-is.
                let operand = match expr.kind {
                    ExprKind::Paren(inner) => *inner,
                    _ => expr,
                };
                self.tok = self.lex_token()?;
                let idx = self.parse_expr(PREC_LOW)?;
                let end = self.tok.span;
                self.expect(&Token::RightBracket)?;
                let span = operand.span.merge(end);
                // The subscript is a list (`(1,2,3)[0,2]`): flatten a `Comma` into the index vector.
                let indices = match idx.kind {
                    ExprKind::Comma(items) => items,
                    _ => vec![idx],
                };
                expr = Expr::list_slice(operand, indices, span);
            } else if self.tok.token == Token::LeftBracket {
                self.tok = self.lex_token()?;
                let idx = self.parse_expr(PREC_LOW)?;
                let end = self.tok.span;
                self.expect(&Token::RightBracket)?;
                let span = expr.span.merge(end);
                expr = Expr::array_elem(expr, idx, span);
            } else if self.tok.token == Token::LeftBrace {
                let key = self.parse_hash_subscript_key()?;
                let end = self.tok.span;
                self.expect(&Token::RightBrace)?;
                let span = expr.span.merge(end);
                expr = Expr::hash_elem(expr, key, span);
            } else {
                break;
            }
        }
        // A `Paren` not consumed as a list slice above is left intact: it carries the parens-fact that the
        // enclosing consumer needs (`=` classifies list-vs-scalar, refgen reads flatten-ness) and is unwrapped
        // there.  Unwrapping here would erase that fact before anyone could read it.
        Ok(expr)
    }

    /// Whether `expr` is a *list literal* that a following `[...]` slices (rather than indexes as a container element).
    /// The forms are a parenthesized expression (transient `Paren`), a `qw[...]`, an empty list `()`, and a list slice
    /// (whose result is itself a list literal, so it can be sliced again).
    fn is_list_literal_operand(expr: &Expr) -> bool {
        matches!(expr.kind, ExprKind::Paren(_) | ExprKind::QwList(_) | ExprKind::EmptyList | ExprKind::ListSlice(_, _))
    }

    // ── Interpolated string assembly ──────────────────────────
    /// Collect sub-tokens from the active sublex frame into an `Interpolated`.  Returns the body and a virtual
    /// `Token::Eof` — the position is right after the closing delimiter, so the caller can scan flags or set up a
    /// replacement frame before lexing the next real token.
    fn parse_interpolated(&mut self) -> Result<Interpolated, ParseError> {
        let mut parts: Vec<InterpPart> = Vec::new();

        loop {
            match &self.tok.token {
                Token::SublexEnd => {
                    let merged = merge_interp_parts(parts);
                    // Virtual EOF — don't lex past the sublex boundary.  The caller handles the post-sublex transition
                    // (flag scanning, replacement setup, etc.) before lexing the next real token.
                    return Ok(Interpolated(merged));
                }
                Token::ConstSegment(_) => {
                    let Token::ConstSegment(s) = self.next_token()?.token else { unreachable!() };
                    parts.push(InterpPart::Const(s));
                }
                Token::NamedChar { .. } => {
                    let Token::NamedChar { name, codepoint } = self.next_token()?.token else { unreachable!() };
                    parts.push(InterpPart::NamedChar { name, codepoint });
                }
                Token::InterpScalar(_) => {
                    let cm = self.take_interp_case_mod();
                    let consumed = self.next_token()?;
                    let Token::InterpScalar(name) = consumed.token else { unreachable!() };
                    let expr = apply_case_mod_wrap(Expr::new(ExprKind::ScalarVar(name), consumed.span), cm);
                    parts.push(InterpPart::ScalarInterp(Box::new(expr)));
                }
                Token::InterpArray(_) => {
                    let cm = self.take_interp_case_mod();
                    let consumed = self.next_token()?;
                    let Token::InterpArray(name) = consumed.token else { unreachable!() };
                    let expr = apply_case_mod_wrap(Expr::new(ExprKind::ArrayVar(name), consumed.span), cm);
                    parts.push(InterpPart::ArrayInterp(Box::new(expr)));
                }
                Token::InterpScalarChainStart(_) => {
                    let cm = self.take_interp_case_mod();
                    let consumed = self.next_token()?;
                    let Token::InterpScalarChainStart(name) = consumed.token else { unreachable!() };
                    let initial = Expr::new(ExprKind::ScalarVar(name), consumed.span);
                    let after_subscripts = self.maybe_postfix_subscript(initial)?;
                    let expr = self.parse_expr_continuation(after_subscripts, PREC_LOW)?;
                    self.expect(&Token::InterpChainEnd)?;
                    let expr = apply_case_mod_wrap(expr, cm);
                    parts.push(InterpPart::ScalarInterp(Box::new(expr)));
                    // tok is the token after InterpChainEnd — loop continues with it.
                }
                Token::InterpArrayChainStart(_) => {
                    let cm = self.take_interp_case_mod();
                    let consumed = self.next_token()?;
                    let Token::InterpArrayChainStart(name) = consumed.token else { unreachable!() };
                    let recv = Expr::new(ExprKind::ArrayVar(name), consumed.span);
                    let expr = if self.tok.token == Token::LeftBracket {
                        self.tok = self.lex_token()?;
                        let mut indices = Vec::new();
                        while self.tok.token != Token::RightBracket && self.tok.token != Token::Eof {
                            let expr = self.parse_expr(PREC_COMMA + 1)?;
                            indices.push(expr);

                            if self.tok.token == Token::Comma {
                                self.tok = self.lex_token()?;
                            } else {
                                break;
                            }
                        }
                        let end = self.tok.span;
                        self.expect(&Token::RightBracket)?;
                        Expr::new(ExprKind::ArraySlice(Box::new(recv), indices), consumed.span.merge(end))
                    } else if self.tok.token == Token::LeftBrace {
                        self.tok = self.lex_token()?;
                        let mut keys = Vec::new();
                        while self.tok.token != Token::RightBrace && self.tok.token != Token::Eof {
                            let expr = self.parse_expr(PREC_COMMA + 1)?;
                            keys.push(expr);

                            if self.tok.token == Token::Comma {
                                self.tok = self.lex_token()?;
                            } else {
                                break;
                            }
                        }
                        let end = self.tok.span;
                        self.expect(&Token::RightBrace)?;
                        Expr::new(ExprKind::HashSlice(Box::new(recv), keys), consumed.span.merge(end))
                    } else {
                        return Err(ParseError::new("expected [ or { after @name in string", self.tok.span));
                    };
                    self.expect(&Token::InterpChainEnd)?;
                    let expr = apply_case_mod_wrap(expr, cm);
                    parts.push(InterpPart::ArrayInterp(Box::new(expr)));
                    // tok is the token after InterpChainEnd — loop continues with it.
                }
                Token::InterpScalarExprStart | Token::InterpArrayExprStart => {
                    let cm = self.take_interp_case_mod();
                    self.tok = self.lex_token()?;
                    let expr = self.parse_expr(PREC_LOW)?;
                    self.expect(&Token::RightBrace)?;
                    let expr = apply_case_mod_wrap(expr, cm);
                    parts.push(InterpPart::ExprInterp(Box::new(expr)));
                    // tok is the token after } — loop continues with it.
                }
                tok_val @ (Token::RegexCodeStart | Token::RegexCondCodeStart) => {
                    let is_cond = matches!(tok_val, Token::RegexCondCodeStart);
                    let code_start = self.tok.span.end as usize;
                    self.tok = self.lex_token()?;
                    let expr = self.parse_expr(PREC_LOW)?;
                    let code_end = self.tok.span.start as usize;
                    let raw = String::from_utf8_lossy(self.slice(code_start, code_end)).into_owned();
                    self.expect(&Token::RightBrace)?;
                    if is_cond {
                        parts.push(InterpPart::RegexCondCode(raw, Box::new(expr)));
                    } else {
                        parts.push(InterpPart::RegexCode(raw, Box::new(expr)));
                    }
                    // tok is the token after } — loop continues with it.
                }
                Token::Eof => {
                    return Err(ParseError::new("unterminated interpolated string", self.tok.span));
                }
                other => {
                    return Err(ParseError::new(format!("unexpected token in string: {other:?}"), self.tok.span));
                }
            }
        }
    }

    /// Parse an interpolated string body into an Expr.  Lexes the next real token after the sublex ends.
    fn parse_interpolated_string(&mut self, span: Span) -> Result<Expr, ParseError> {
        let interp = self.parse_interpolated()?;
        {
            self.tok = self.lex_token()?;
            Ok(interp_to_expr(interp, span))
        }
    }

    // ── Operator parsing ──────────────────────────────────────
    /// The operator table: map a token to its precedence and associativity, or `None` if it does not begin an infix or
    /// postfix operator.  The single source of truth for operator binding, consulted by `lex_operator`.
    /// `smartmatch_active` gates the feature-gated `~~`; the caller snapshots it from `self.features` before borrowing
    /// the cached token.
    fn op_info_for_token(tok: &Token, smartmatch_active: bool) -> Option<OpInfo> {
        match tok {
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
            Token::TwoDots | Token::ThreeDots => Some(OpInfo { prec: PREC_RANGE, assoc: Assoc::Non }),
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
            ExprKind::Undef => true,     // (undef, $x) = (1, 2)
            ExprKind::EmptyList => true, // () = (1, 2, 3); my $n = () = LIST
            _ => false,
        }
    }

    /// Strip a single transient grouping `Paren`, returning the inner expression; any non-`Paren` is returned
    /// unchanged.  Grouping parens are transparent at every point of combination (infix operators, prefix-frame
    /// operands, the top-level return), and each such site calls this so the `Paren` never reaches the finished tree.
    /// Nesting is already capped at depth one by the wrap site, so a single strip suffices.
    fn unwrap_paren(expr: Expr) -> Expr {
        match expr.kind {
            ExprKind::Paren(inner) => *inner,
            other => Expr { kind: other, ..expr },
        }
    }

    /// Does this assignment LHS denote an aggregate (list-context) target?  Used by the `=` arm: an aggregate LHS — or
    /// a parenthesized LHS, which the caller tests separately via the parens-fact — makes the whole assignment a list
    /// assignment.  Arrays, hashes, their slices and derefs, a comma list, and the empty list are aggregates; `local`
    /// inherits from its inner lvalue; a `Decl` is aggregate when it declares any array/hash (the parenthesized list
    /// form is a grouping `Paren`, caught by the parens-fact, so it is not the concern here).  Everything else —
    /// scalars, elements, scalar derefs — is scalar.
    fn is_aggregate_lvalue(expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::ArrayVar(_)
            | ExprKind::HashVar(_)
            | ExprKind::ArraySlice(_, _)
            | ExprKind::HashSlice(_, _)
            | ExprKind::Deref(Sigil::Array, _)
            | ExprKind::Deref(Sigil::Hash, _)
            | ExprKind::Comma(_)
            | ExprKind::EmptyList => true,
            ExprKind::Local(inner) => Self::is_aggregate_lvalue(inner),
            ExprKind::Decl(_, vars) => vars.iter().any(|v| matches!(v.sigil, Sigil::Array | Sigil::Hash)),
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
        let right_prec = info.right_prec();

        // Grouping parens are transparent to every infix operator.  Record whether the left operand was parenthesized
        // before unwrapping it — the `=` arm needs that fact to classify a single-scalar LHS as a list assignment
        // (`($x) = ...`) — then unwrap so the operator combines with the bare operand.
        let left_parenthesized = matches!(left.kind, ExprKind::Paren(_));
        let mut left = Self::unwrap_paren(left);

        match &self.tok.token {
            // Postfix increment/decrement
            Token::PlusPlus => {
                if !Self::is_valid_lvalue(&left) {
                    return Err(ParseError::new("invalid operand for postfix ++", left.span));
                }
                let span = left.span.merge(self.tok.span);
                self.tok = self.lex_token()?;
                Ok(Expr::postfix(PostfixOp::Inc, left, span))
            }
            Token::MinusMinus => {
                if !Self::is_valid_lvalue(&left) {
                    return Err(ParseError::new("invalid operand for postfix --", left.span));
                }
                let span = left.span.merge(self.tok.span);
                self.tok = self.lex_token()?;
                Ok(Expr::postfix(PostfixOp::Dec, left, span))
            }

            // Ternary
            Token::Question => {
                self.tok = self.lex_token()?;
                let then_expr = self.parse_expr(PREC_LOW)?;
                self.expect(&Token::Colon)?;
                let else_expr = self.parse_expr(right_prec)?;
                let span = left.span.merge(else_expr.span);
                Ok(Expr::ternary(left, then_expr, else_expr, span))
            }

            // Arrow
            Token::Arrow => self.parse_arrow_rhs(left),

            // Assignment
            Token::Assign(op) => {
                let op = *op;
                // `refaliasing` (5.22+) extends lvalue-ness to include `\$x`, `\@a`, `\%h`, and lists of those.
                let refalias_ok = self.pragmas.features.contains(Features::REFALIASING) && Self::is_ref_alias_target(&left);
                if !Self::is_valid_lvalue(&left) && !refalias_ok {
                    return Err(ParseError::new("invalid assignment target", left.span));
                }
                self.tok = self.lex_token()?;
                let mut right = self.parse_expr(right_prec)?;

                // List assignment iff the LHS is a parenthesized list or an inherent aggregate target; both sides
                // are then evaluated in list context.  A bare scalar LHS is a scalar assignment.
                let ctx = if left_parenthesized || Self::is_aggregate_lvalue(&left) { Context::List } else { Context::Scalar };
                left.save_context(ctx);
                right.save_context(ctx);

                let span = left.span.merge(right.span);
                Ok(Expr::assign(op, left, right, span))
            }

            // Comma / fat comma — build a list
            Token::Comma | Token::FatComma => {
                self.tok = self.lex_token()?;
                if matches!(self.tok.token, Token::Semi | Token::RightParen | Token::RightBracket | Token::RightBrace | Token::Eof) {
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

            // Range / flip-flop — non-associative, reject chaining.  `..` and `...` are the same operator;
            // range-vs-flip-flop is determined by context at lowering.
            Token::TwoDots => {
                self.tok = self.lex_token()?;
                let right = self.parse_expr(right_prec)?;
                self.reject_non_assoc_chaining(info, &right)?;
                let span = left.span.merge(right.span);
                Ok(Expr::range(left, right, RangeKind::TwoDots, span))
            }
            Token::ThreeDots => {
                self.tok = self.lex_token()?;
                let right = self.parse_expr(right_prec)?;
                self.reject_non_assoc_chaining(info, &right)?;
                let span = left.span.merge(right.span);
                Ok(Expr::range(left, right, RangeKind::ThreeDots, span))
            }

            // Binary operators
            token => {
                let binop = token_to_binop(token)?;
                self.tok = self.lex_token()?;
                let right = self.parse_expr(right_prec)?;
                let sm = self.features.contains(Features::SMARTMATCH);

                match info.assoc {
                    Assoc::Chain => {
                        // After parse_expr(right_prec) consumed everything above info.prec, lex_operator
                        // can only match operators at exactly that level.
                        self.lex_operator()?;
                        if let Some(next_info) = Self::op_info_for_token(&self.tok.token, sm)
                            && next_info.prec == info.prec
                        {
                            if next_info.assoc != Assoc::Chain {
                                // Non-chainable operator at same precedence (e.g. `$a == $b <=> $c`).
                                return Err(ParseError::new("non-associative operator cannot be chained", right.span));
                            }
                            let mut ops = vec![binop, token_to_binop(&self.tok.token)?];
                            let start_span = left.span;
                            self.tok = self.lex_token()?;
                            let expr = self.parse_expr(right_prec)?;
                            let mut operands = vec![left, right, expr];
                            self.lex_operator()?;
                            while let Some(next_info) = Self::op_info_for_token(&self.tok.token, sm) {
                                if next_info.prec != info.prec {
                                    break;
                                }
                                if next_info.assoc != Assoc::Chain {
                                    // e.g. `$a == $b != $c <=> $d` — non-chainable trailing a chain.
                                    return Err(ParseError::new("non-associative operator cannot be chained", operands.last().map_or(start_span, |e| e.span)));
                                }
                                ops.push(token_to_binop(&self.tok.token)?);
                                self.tok = self.lex_token()?;
                                let expr = self.parse_expr(right_prec)?;
                                operands.push(expr);
                                self.lex_operator()?;
                            }
                            let end_span = operands.last().map_or(start_span, |e| e.span);
                            return Ok(Expr::new(ExprKind::ChainedCmp(ops, operands), start_span.merge(end_span)));
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
    /// follows.  Runs lex_operator for x-repeat fixup, then checks op_info_for_token.  Returns the (possibly
    /// transformed) token for the caller to continue with.
    fn reject_non_assoc_chaining(&mut self, info: OpInfo, right: &Expr) -> Result<(), ParseError> {
        self.lex_operator()?;
        let sm = self.features.contains(Features::SMARTMATCH);
        if let Some(next_info) = Self::op_info_for_token(&self.tok.token, sm)
            && next_info.prec == info.prec
        {
            return Err(ParseError::new("non-associative operator cannot be chained", right.span));
        }
        Ok(())
    }

    fn parse_arrow_rhs(&mut self, left: Expr) -> Result<Expr, ParseError> {
        self.tok = self.lex_token()?;

        // After ->, identifiers (including what would otherwise be keywords) are method names.
        let method_name: Option<String> = match &self.tok.token {
            Token::Ident(name) => Some(name.clone()),
            Token::Keyword(kw) => Some((<&str>::from(*kw)).to_string()),
            _ => None,
        };
        if let Some(name) = method_name {
            self.tok = self.lex_token()?;
            // Method call: ->method(...)
            if self.tok.token == Token::LeftParen {
                self.tok = self.lex_token()?;
                let mut args = Vec::new();
                while self.tok.token != Token::RightParen && self.tok.token != Token::Eof {
                    let expr = self.parse_expr(PREC_COMMA + 1)?;
                    args.push(expr);
                    if self.tok.token == Token::Comma {
                        self.tok = self.lex_token()?;
                    } else {
                        break;
                    }
                }
                let end = self.tok.span;
                self.expect(&Token::RightParen)?;
                let span = left.span.merge(end);
                return Ok(Expr::method_call(left, name, args, span));
            } else {
                // Bare method call with no parens
                let span = left.span.merge(self.tok.span);
                return Ok(Expr::method_call(left, name, vec![], span));
            }
        }
        match &self.tok.token {
            Token::LeftBracket => {
                self.tok = self.lex_token()?;
                let idx = self.parse_expr(PREC_LOW)?;
                let end = self.tok.span;
                self.expect(&Token::RightBracket)?;
                let span = left.span.merge(end);
                let expr = Expr::arrow_deref(left, ArrowTarget::array_elem(idx), span);
                // Handle chained subscripts: $ref->[0][1], $ref->[0]{key}
                self.maybe_postfix_subscript(expr)
            }
            Token::LeftBrace => {
                let key = self.parse_hash_subscript_key()?;
                let end = self.tok.span;
                self.expect(&Token::RightBrace)?;
                let span = left.span.merge(end);
                let expr = Expr::arrow_deref(left, ArrowTarget::hash_elem(key), span);
                // Handle chained subscripts: $ref->{a}{b}, $ref->{a}[0]
                self.maybe_postfix_subscript(expr)
            }
            Token::LeftParen => {
                // ->(...) — coderef call
                self.tok = self.lex_token()?;
                let mut args = Vec::new();
                while self.tok.token != Token::RightParen && self.tok.token != Token::Eof {
                    let expr = self.parse_expr(PREC_COMMA + 1)?;
                    args.push(expr);
                    if self.tok.token == Token::Comma {
                        self.tok = self.lex_token()?;
                    } else {
                        break;
                    }
                }
                let end = self.tok.span;
                self.expect(&Token::RightParen)?;
                let span = left.span.merge(end);
                Ok(Expr::method_call(left, String::new(), args, span))
            }

            // Dynamic method dispatch: ->$method or ->$method(args)
            Token::ScalarVar(_) => {
                let var_span = self.tok.span;
                let Token::ScalarVar(var_name) = self.next_token()?.token else { unreachable!() };
                let method_expr = Expr::new(ExprKind::ScalarVar(var_name), var_span);
                if self.tok.token == Token::LeftParen {
                    self.tok = self.lex_token()?;
                    let mut args = Vec::new();
                    while self.tok.token != Token::RightParen && self.tok.token != Token::Eof {
                        let expr = self.parse_expr(PREC_COMMA + 1)?;
                        args.push(expr);
                        if self.tok.token == Token::Comma {
                            self.tok = self.lex_token()?;
                        } else {
                            break;
                        }
                    }
                    let end = self.tok.span;
                    self.expect(&Token::RightParen)?;
                    let span = left.span.merge(end);
                    Ok(Expr::arrow_deref(left, ArrowTarget::dyn_method(method_expr, args), span))
                } else {
                    let span = left.span.merge(var_span);
                    Ok(Expr::arrow_deref(left, ArrowTarget::dyn_method(method_expr, vec![]), span))
                }
            }

            // Postfix dereference: ->@*, ->%*, ->$*, ->&*, ->**, plus slice forms ->@[...], ->@{...},
            // ->%[...], ->%{...}.
            //
            // The trailing `*` forms are whole-container derefs.  The `[...]` and `{...}` forms after `@` or `%`
            // produce slices (array of values or kv list).
            Token::At => {
                self.tok = self.lex_token()?;
                if self.tok.token == Token::Star {
                    self.tok = self.lex_token()?;
                    let span = left.span.merge(self.tok.span);
                    Ok(Expr::arrow_deref(left, ArrowTarget::DerefArray, span))
                } else if self.tok.token == Token::LeftBracket {
                    self.tok = self.lex_token()?;
                    let idx = self.parse_expr(PREC_LOW)?;
                    let end = self.tok.span;
                    self.expect(&Token::RightBracket)?;
                    let span = left.span.merge(end);
                    Ok(Expr::arrow_deref(left, ArrowTarget::array_slice_indices(idx), span))
                } else if self.tok.token == Token::LeftBrace {
                    self.tok = self.lex_token()?;
                    let key = self.parse_expr(PREC_LOW)?;
                    let end = self.tok.span;
                    self.expect(&Token::RightBrace)?;
                    let span = left.span.merge(end);
                    Ok(Expr::arrow_deref(left, ArrowTarget::array_slice_keys(key), span))
                } else {
                    Err(ParseError::new("expected *, [indices], or {keys} after ->@", self.tok.span))
                }
            }
            Token::Dollar => {
                // `->$#*` — postderef last-index.  The lexer would otherwise tokenize the `#` as a comment start,
                // so we peek+consume the two raw bytes here before the next token is lexed.
                if self.try_consume_hash_star() {
                    self.tok = self.lex_token()?;
                    let span = left.span.merge(self.tok.span);
                    Ok(Expr::arrow_deref(left, ArrowTarget::LastIndex, span))
                } else {
                    self.tok = self.lex_token()?;
                    if self.tok.token == Token::Star {
                        self.tok = self.lex_token()?;
                        let span = left.span.merge(self.tok.span);
                        Ok(Expr::arrow_deref(left, ArrowTarget::DerefScalar, span))
                    } else {
                        Err(ParseError::new("expected * or #* after ->$", self.tok.span))
                    }
                }
            }
            Token::Percent => {
                self.tok = self.lex_token()?;
                if self.tok.token == Token::Star {
                    self.tok = self.lex_token()?;
                    let span = left.span.merge(self.tok.span);
                    Ok(Expr::arrow_deref(left, ArrowTarget::DerefHash, span))
                } else if self.tok.token == Token::LeftBracket {
                    self.tok = self.lex_token()?;
                    let idx = self.parse_expr(PREC_LOW)?;
                    let end = self.tok.span;
                    self.expect(&Token::RightBracket)?;
                    let span = left.span.merge(end);
                    Ok(Expr::arrow_deref(left, ArrowTarget::kv_slice_indices(idx), span))
                } else if self.tok.token == Token::LeftBrace {
                    self.tok = self.lex_token()?;
                    let key = self.parse_expr(PREC_LOW)?;
                    let end = self.tok.span;
                    self.expect(&Token::RightBrace)?;
                    let span = left.span.merge(end);
                    Ok(Expr::arrow_deref(left, ArrowTarget::kv_slice_keys(key), span))
                } else {
                    Err(ParseError::new("expected *, [indices], or {keys} after ->%", self.tok.span))
                }
            }

            // `->&*` — code-ref postfix deref.  `->&method` or `->&method(args)` — lexical method invocation
            // (resolved at compile time, not via package inheritance).
            Token::BitAnd => {
                self.tok = self.lex_token()?;
                if self.tok.token == Token::Star {
                    self.tok = self.lex_token()?;
                    let span = left.span.merge(self.tok.span);
                    Ok(Expr::arrow_deref(left, ArrowTarget::DerefCode, span))
                } else {
                    // Lexical method: ->&name or ->&name(args)
                    let method_name = match &self.tok.token {
                        Token::Ident(_) => {
                            let Token::Ident(n) = self.next_token()?.token else { unreachable!() };
                            n
                        }
                        Token::Keyword(kw) => {
                            let n = <&str>::from(*kw).to_string();
                            self.tok = self.lex_token()?;
                            n
                        }
                        other => return Err(ParseError::new(format!("expected * or method name after ->&, got {other:?}"), self.tok.span)),
                    };
                    let name = format!("&{method_name}");
                    if self.tok.token == Token::LeftParen {
                        self.tok = self.lex_token()?;
                        let mut args = Vec::new();
                        while self.tok.token != Token::RightParen && self.tok.token != Token::Eof {
                            let expr = self.parse_expr(PREC_COMMA + 1)?;
                            args.push(expr);
                            if self.tok.token == Token::Comma {
                                self.tok = self.lex_token()?;
                            } else {
                                break;
                            }
                        }
                        let end = self.tok.span;
                        self.expect(&Token::RightParen)?;
                        let span = left.span.merge(end);
                        Ok(Expr::method_call(left, name, args, span))
                    } else {
                        let span = left.span.merge(self.tok.span);
                        Ok(Expr::method_call(left, name, vec![], span))
                    }
                }
            }

            // `->**` — glob deref.  Two consecutive `*`s; the lexer emits `Power` (`**`) for that pair.
            Token::Power => {
                self.tok = self.lex_token()?;
                let span = left.span.merge(self.tok.span);
                Ok(Expr::arrow_deref(left, ArrowTarget::DerefGlob, span))
            }
            other => Err(ParseError::new(format!("expected method name or subscript after ->, got {other:?}"), self.tok.span)),
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
