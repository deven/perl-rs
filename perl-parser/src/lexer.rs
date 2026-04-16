//! Lexer — tokenizer.
//!
//! The lexer returns unambiguous tokens; where Perl syntax is
//! context-sensitive (e.g. `<<` meaning heredoc vs shift-left,
//! `%` meaning hash-sigil vs modulo), the parser invokes specific
//! hook methods (e.g. `lex_heredoc_after_shift_left`,
//! `lex_hash_var_after_percent`) to drive the disambiguation.
//!
//! This module implements the core tokenization loop.  Quote-like
//! sublexing, heredocs, and regex scanning are handled by helper
//! methods.

use bytes::Bytes;

use crate::error::ParseError;
use crate::keyword;
use crate::source::{LexerLine, LexerSource};
use crate::span::Span;
use crate::token::*;

/// Sublexing context — tracks what mode the lexer is in.
///
/// When `expr_depth > 0`, the lexer is in expression-parsing mode
/// inside `${expr}` or `@{expr}`.  When `expr_depth == 0`, the
/// lexer is in body-scanning mode (string/regex content).
#[derive(Clone, Debug)]
struct LexContext {
    /// Opening delimiter byte.  `None` for heredocs (end signaled
    /// by LexerSource).
    delim: Option<u8>,
    /// Delimiter nesting depth (for paired delimiters like `{}`).
    depth: u32,
    /// Brace depth inside `${expr}` or `@{expr}`.  When > 0,
    /// the lexer produces normal code tokens.  When 0, it
    /// produces string body tokens via `lex_body`.
    expr_depth: u32,
    /// Whether `$`/`@` trigger interpolation.
    interpolating: bool,
    /// Whether escapes pass through raw (for regex, tr, prototypes).
    raw: bool,
    /// Whether to detect `(?{...})` code blocks (regex mode).
    regex: bool,
}

/// Lexer state, owned by the `Parser`.
///
/// The lexer operates on lines delivered by `LexerSource`.  The
/// context stack tracks sublexing modes (interpolating strings,
/// regex patterns, heredocs).  Context-sensitive disambiguation
/// (e.g. heredoc vs shift-left for `<<`) is driven by the parser
/// via explicit hook methods.
///
/// CRLF normalization is handled by `LexerSource` at the line level.
pub(crate) struct Lexer {
    source: LexerSource,
    current_line: Option<LexerLine>,
    context_stack: Vec<LexContext>,
    /// Deferred error from auto-loading in `peek_byte`.
    /// Surfaced on the next call to `lex_token`.
    pending_error: Option<ParseError>,
}

impl Lexer {
    pub fn new(src: &[u8]) -> Self {
        Lexer { source: LexerSource::new(src), current_line: None, context_stack: Vec::new(), pending_error: None }
    }

    /// Global byte position in the original source.
    pub fn pos(&self) -> usize {
        match &self.current_line {
            Some(line) => line.offset + line.pos,
            None => self.source.cursor(),
        }
    }

    // ── Byte access (auto-loading) ──────────────────────────

    /// Peek at the current byte without advancing.
    /// Auto-loads the next line when the current one is exhausted.
    /// Returns `b'\n'` for a terminated line ending.
    /// Returns `None` only at true EOF (or heredoc end in peek mode).
    ///
    /// `peek_heredoc`: when true, a heredoc end-of-body signal from
    /// `next_line` is preserved (not consumed).  Use `true` inside
    /// body scanning loops, `false` at entry points.
    fn peek_byte(&mut self, peek_heredoc: bool) -> Option<u8> {
        // Check current line for available bytes.
        if let Some(line) = &self.current_line
            && let Some(b) = line.peek_byte()
        {
            return Some(b);
        }
        // No line or line exhausted.  Try to load a new one.
        // On success, replace the old line.  On failure, keep
        // the old line so callers can still use line_slice etc.
        match self.source.next_line(peek_heredoc) {
            Ok(Some(new_line)) => {
                let b = new_line.peek_byte();
                self.current_line = Some(new_line);
                b
            }
            Ok(None) => None,
            Err(e) => {
                self.pending_error = Some(e);
                None
            }
        }
    }

    /// Peek at a byte at an offset from the current position.
    /// Does NOT auto-load — only valid within the current line.
    fn peek_byte_at(&self, offset: usize) -> Option<u8> {
        self.current_line.as_ref()?.peek_byte_at(offset)
    }

    /// Consume the current byte and advance.
    /// Does NOT auto-load — the next `peek_byte` call will handle
    /// loading if the line is exhausted.
    fn advance_byte(&mut self) -> Option<u8> {
        self.current_line.as_mut()?.advance_byte()
    }

    /// Remaining bytes in the current line (not including synthetic \n).
    pub fn remaining(&self) -> &[u8] {
        match &self.current_line {
            Some(line) => line.remaining(),
            None => &[],
        }
    }

    // ── Position and span helpers ─────────────────────────────

    /// Current position within the current line (line-local).
    fn line_pos(&self) -> usize {
        self.current_line.as_ref().map_or(0, |l| l.pos)
    }

    /// Global position as u32 for span construction.
    fn span_pos(&self) -> u32 {
        match &self.current_line {
            Some(line) => line.global_pos(),
            None => self.source.cursor() as u32,
        }
    }

    /// Build a `Span` from a line-local start position to the current
    /// cursor position.  Both positions are on the current line.
    fn span_from(&self, local_start: usize) -> Span {
        match &self.current_line {
            Some(line) => Span::new((line.offset + local_start) as u32, line.global_pos()),
            None => {
                let pos = self.source.cursor() as u32;
                Span::new(pos, pos)
            }
        }
    }

    /// Advance the cursor by `n` bytes within the current line.
    fn skip(&mut self, n: usize) {
        if let Some(line) = self.current_line.as_mut() {
            line.pos += n;
        }
    }

    /// Rewind the cursor by `n` bytes within the current line.
    /// The caller must ensure `n` does not exceed the current position.
    pub fn rewind(&mut self, n: usize) {
        if let Some(line) = self.current_line.as_mut() {
            line.pos -= n;
        }
    }

    /// Byte slice from line-local `start` to current cursor position.
    fn line_slice(&self, start: usize) -> &[u8] {
        match &self.current_line {
            Some(line) => &line.line[start..line.pos],
            None => &[],
        }
    }

    /// Like `line_slice` but returns `&str`.  Returns an error for
    /// non-UTF-8 source bytes (identifiers and numbers are always
    /// ASCII, so this only fails for truly malformed input).
    fn line_slice_str(&self, start: usize) -> Result<&str, ParseError> {
        let bytes = self.line_slice(start);
        std::str::from_utf8(bytes).map_err(|_| ParseError::new("invalid UTF-8 in source", self.span_from(start)))
    }

    /// Whether the current line was terminated by a newline in the source.
    pub fn line_is_terminated(&self) -> bool {
        self.current_line.as_ref().is_some_and(|l| l.terminated)
    }

    /// Skip to end of source — used after __END__/__DATA__.
    pub fn skip_to_end(&mut self) {
        self.current_line = None;
        // Drain the source.
        while let Ok(Some(_)) = self.source.next_line(false) {}
    }

    /// Current byte position in source (global).
    pub fn current_pos(&self) -> usize {
        self.pos()
    }

    /// Raw slice of the source buffer.  For rare operations that
    /// need global byte access (e.g. format body extraction).
    pub fn slice(&self, start: usize, end: usize) -> &[u8] {
        self.source.src_slice(start, end)
    }

    /// Skip a format body: everything until a line containing just `.`
    /// (optionally followed by whitespace).
    pub fn skip_format_body(&mut self) {
        // Skip the rest of the current line (after `=`).
        self.current_line = None;
        loop {
            match self.source.next_line(false) {
                Ok(Some(line)) => {
                    // Terminator: '.' at start of line, optionally followed by ws.
                    if line.line.first() == Some(&b'.') && line.line[1..].iter().all(|&b| b == b' ' || b == b'\t' || b == b'\r') {
                        return;
                    }
                }
                _ => return, // EOF
            }
        }
    }

    // ── Skip whitespace and comments ──────────────────────────

    fn skip_ws_and_comments(&mut self) -> Result<(), ParseError> {
        loop {
            // peek_byte auto-loads lines. \n is a byte, skipped as whitespace.
            match self.peek_byte(false) {
                Some(b' ') | Some(b'\t') | Some(b'\n') => self.skip(1),
                Some(b'#') => {
                    // Comment — drop entire line.
                    self.current_line = None;
                }
                Some(b'=') if self.peek_byte_at(1).is_some_and(|b| b.is_ascii_alphabetic()) && self.line_pos() == 0 => {
                    self.skip_pod()?;
                }
                _ => break, // Non-whitespace byte or EOF
            }
        }
        Ok(())
    }

    /// Skip a pod block: everything from `=word` to `=cut\n`.
    /// Matches Perl 5's behavior: `=cut` must be at start of line,
    /// followed by a non-alphabetic character (or EOF).
    fn skip_pod(&mut self) -> Result<(), ParseError> {
        // Skip the current =word line.
        self.current_line = None;
        // Read lines until =cut at start of line.
        loop {
            // peek_byte auto-loads the next line.
            if self.peek_byte(false).is_none() {
                break; // EOF inside pod — not an error per Perl
            }
            let is_cut =
                self.current_line.as_ref().is_some_and(|line| line.line.starts_with(b"=cut") && !line.line.get(4).is_some_and(|b| b.is_ascii_alphabetic()));
            self.current_line = None; // skip this line
            if is_cut {
                break;
            }
        }
        Ok(())
    }

    // ── Main tokenization entry point ─────────────────────────

    /// Lex the next token.  When inside a sublexing context
    /// (interpolating string, etc.), dispatches to the appropriate
    /// sub-lexer instead.
    pub fn lex_token(&mut self) -> Result<Spanned, ParseError> {
        // Surface any deferred error from auto-loading in peek_byte.
        if let Some(e) = self.pending_error.take() {
            return Err(e);
        }

        // If inside a sublexing context, dispatch there.
        match self.context_stack.last() {
            Some(ctx) if ctx.expr_depth > 0 => {
                // Normal code lexing inside ${expr} or @{expr}.
                let result = self.lex_normal_token()?;
                // Track brace depth to find the closing }.
                match &result.token {
                    Token::LeftBrace => {
                        if let Some(ctx) = self.context_stack.last_mut() {
                            ctx.expr_depth += 1;
                        }
                    }
                    Token::RightBrace => {
                        if let Some(ctx) = self.context_stack.last_mut() {
                            ctx.expr_depth -= 1;
                        }
                    }
                    _ => {}
                }
                return Ok(result);
            }
            Some(ctx) => {
                let (delim, depth, interpolating, raw, regex) = (ctx.delim, ctx.depth, ctx.interpolating, ctx.raw, ctx.regex);
                return self.lex_body(delim, depth, interpolating, regex, raw);
            }
            None => {}
        }

        self.lex_normal_token()
    }

    /// Lex a token in normal (code) mode.
    fn lex_normal_token(&mut self) -> Result<Spanned, ParseError> {
        self.skip_ws_and_comments()?;

        let start = self.span_pos();

        let b = match self.peek_byte(false) {
            Some(b) => b,
            None => {
                return Ok(Spanned { token: Token::Eof, span: Span::new(start, start) });
            }
        };

        let token = match b {
            // ── Digits → numeric literal ──────────────────────
            b'0'..=b'9' => self.lex_number()?,

            // ── Sigils → variables ────────────────────────────
            b'$' => self.lex_dollar()?,
            b'@' => self.lex_at()?,
            b'%' => self.lex_percent()?,

            // ── Identifiers and keywords ──────────────────────
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.lex_word()?,

            // ── Strings ───────────────────────────────────────
            b'\'' => self.lex_single_quoted_string()?,
            b'"' => {
                self.skip(1); // skip opening "
                self.context_stack.push(LexContext { delim: Some(b'"'), depth: 0, expr_depth: 0, interpolating: true, raw: false, regex: false });
                Token::QuoteBegin(QuoteKind::Double, b'"')
            }
            b'`' => {
                self.skip(1); // skip opening `
                self.context_stack.push(LexContext { delim: Some(b'`'), depth: 0, expr_depth: 0, interpolating: true, raw: false, regex: false });
                Token::QuoteBegin(QuoteKind::Backtick, b'`')
            }

            // ── Operators and punctuation ─────────────────────
            b'+' => self.lex_plus(),
            b'-' => self.lex_minus(),
            b'*' => self.lex_star(),
            b'/' => self.lex_slash(),
            b'.' => self.lex_dot(),
            b'<' => self.lex_less_than()?,
            b'>' => self.lex_greater_than(),
            b'=' => self.lex_equals(),
            b'!' => self.lex_bang(),
            b'&' => self.lex_ampersand(),
            b'|' => self.lex_pipe(),
            b'^' => {
                self.skip(1);
                if self.peek_byte(false) == Some(b'=') {
                    self.skip(1);
                    Token::Assign(AssignOp::BitXorEq)
                } else {
                    Token::BitXor
                }
            }
            b'~' => {
                self.skip(1);
                Token::Tilde
            }
            b'\\' => {
                self.skip(1);
                Token::Backslash
            }
            b'?' => {
                self.skip(1);
                Token::Question
            }
            b':' => {
                self.skip(1);
                Token::Colon
            }
            b',' => {
                self.skip(1);
                Token::Comma
            }
            b';' => {
                self.skip(1);
                Token::Semi
            }
            b'(' => {
                self.skip(1);
                Token::LeftParen
            }
            b')' => {
                self.skip(1);
                Token::RightParen
            }
            b'[' => {
                self.skip(1);
                Token::LeftBracket
            }
            b']' => {
                self.skip(1);
                Token::RightBracket
            }
            b'{' => {
                self.skip(1);
                Token::LeftBrace
            }
            b'}' => {
                self.skip(1);
                Token::RightBrace
            }

            // ^D (0x04) and ^Z (0x1a) — logical end of script.
            b'\x04' => {
                self.skip(1);
                Token::DataEnd(DataEndMarker::CtrlD)
            }
            b'\x1a' => {
                self.skip(1);
                Token::DataEnd(DataEndMarker::CtrlZ)
            }

            other => {
                self.skip(1);
                return Err(ParseError::new(format!("unexpected byte 0x{:02x} ('{}')", other, other as char), Span::new(start, self.span_pos())));
            }
        };

        let end = self.span_pos();
        Ok(Spanned { token, span: Span::new(start, end) })
    }

    // ── Number literals ───────────────────────────────────────

    fn lex_number(&mut self) -> Result<Token, ParseError> {
        let start = self.line_pos();

        // Check for 0x, 0b, 0o prefixes
        if self.peek_byte(false) == Some(b'0') {
            match self.peek_byte_at(1) {
                Some(b'x') | Some(b'X') => return self.lex_hex(),
                Some(b'b') | Some(b'B') => return self.lex_binary(),
                Some(b'o') | Some(b'O') => return self.lex_octal_explicit(),
                _ => {}
            }
        }

        // Decimal integer or float
        self.scan_digits();

        if self.peek_byte(false) == Some(b'.') && self.peek_byte_at(1).is_some_and(|b| b.is_ascii_digit()) {
            // Float
            self.skip(1); // skip '.'
            self.scan_digits();
            self.scan_exponent();
            let s = self.line_slice_str(start)?;
            let s = s.replace('_', "");
            let n: f64 = s.parse().map_err(|_| ParseError::new("invalid float literal", self.span_from(start)))?;
            Ok(Token::FloatLit(n))
        } else if self.peek_byte(false) == Some(b'e') || self.peek_byte(false) == Some(b'E') {
            // Float with exponent
            self.scan_exponent();
            let s = self.line_slice_str(start)?;
            let s = s.replace('_', "");
            let n: f64 = s.parse().map_err(|_| ParseError::new("invalid float literal", self.span_from(start)))?;
            Ok(Token::FloatLit(n))
        } else {
            // Integer
            let s = self.line_slice_str(start)?;
            let s = s.replace('_', "");
            // Leading zero means octal in Perl 5.
            if s.len() > 1 && s.starts_with('0') {
                // Check for illegal octal digits (8, 9).
                if let Some(bad) = s.bytes().skip(1).find(|b| *b == b'8' || *b == b'9') {
                    return Err(ParseError::new(format!("Illegal octal digit '{}'", bad as char), self.span_from(start)));
                }
                let n = i64::from_str_radix(&s[1..], 8).map_err(|_| ParseError::new("invalid octal literal", self.span_from(start)))?;
                Ok(Token::IntLit(n))
            } else {
                let n: i64 = s.parse().map_err(|_| ParseError::new("invalid integer literal", self.span_from(start)))?;
                Ok(Token::IntLit(n))
            }
        }
    }

    fn scan_digits(&mut self) {
        while let Some(b) = self.peek_byte(false) {
            if b.is_ascii_digit() || b == b'_' {
                self.skip(1);
            } else {
                break;
            }
        }
    }

    fn scan_exponent(&mut self) {
        if self.peek_byte(false) == Some(b'e') || self.peek_byte(false) == Some(b'E') {
            self.skip(1);
            if self.peek_byte(false) == Some(b'+') || self.peek_byte(false) == Some(b'-') {
                self.skip(1);
            }
            self.scan_digits();
        }
    }

    fn lex_hex(&mut self) -> Result<Token, ParseError> {
        let start = self.line_pos();
        self.skip(2); // skip 0x
        let hex_start = self.line_pos();
        while let Some(b) = self.peek_byte(false) {
            if b.is_ascii_hexdigit() || b == b'_' {
                self.skip(1);
            } else {
                break;
            }
        }
        let s = self.line_slice_str(hex_start)?.replace('_', "");
        let n = i64::from_str_radix(&s, 16).map_err(|_| ParseError::new("invalid hex literal", self.span_from(start)))?;
        Ok(Token::IntLit(n))
    }

    fn lex_binary(&mut self) -> Result<Token, ParseError> {
        let start = self.line_pos();
        self.skip(2); // skip 0b
        let bin_start = self.line_pos();
        while let Some(b) = self.peek_byte(false) {
            if b == b'0' || b == b'1' || b == b'_' {
                self.skip(1);
            } else {
                break;
            }
        }
        // Check for illegal binary digits (2-9)
        if let Some(b) = self.peek_byte(false)
            && b.is_ascii_digit()
        {
            return Err(ParseError::new(format!("Illegal binary digit '{}'", b as char), self.span_from(start)));
        }
        let s = self.line_slice_str(bin_start)?.replace('_', "");
        let n = i64::from_str_radix(&s, 2).map_err(|_| ParseError::new("invalid binary literal", self.span_from(start)))?;
        Ok(Token::IntLit(n))
    }

    fn lex_octal_explicit(&mut self) -> Result<Token, ParseError> {
        let start = self.line_pos();
        self.skip(2); // skip 0o
        let oct_start = self.line_pos();
        while let Some(b) = self.peek_byte(false) {
            if (b'0'..=b'7').contains(&b) || b == b'_' {
                self.skip(1);
            } else {
                break;
            }
        }
        // Check for illegal octal digits (8, 9)
        if let Some(b) = self.peek_byte(false)
            && (b == b'8' || b == b'9')
        {
            return Err(ParseError::new(format!("Illegal octal digit '{}'", b as char), self.span_from(start)));
        }
        let s = self.line_slice_str(oct_start)?.replace('_', "");
        let n = i64::from_str_radix(&s, 8).map_err(|_| ParseError::new("invalid octal literal", self.span_from(start)))?;
        Ok(Token::IntLit(n))
    }

    // ── Variables ($, @, %) ───────────────────────────────────

    fn lex_dollar(&mut self) -> Result<Token, ParseError> {
        self.skip(1); // skip $

        // $# — array length
        if self.peek_byte(false) == Some(b'#') && self.peek_byte_at(1).is_some_and(|b| b == b'_' || b.is_ascii_alphabetic()) {
            self.skip(1); // skip #
            let name = self.scan_ident();
            return Ok(Token::ArrayLen(name));
        }

        // Special variables: $$, $!, $@, $_, $0-$9, $/, $\, etc.
        match self.peek_byte(false) {
            Some(b'_') => {
                // Could be $_ or $_[...] or $__ident
                if self.peek_byte_at(1).is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_') {
                    let name = self.scan_ident();
                    return Ok(Token::ScalarVar(name));
                }
                self.skip(1);
                return Ok(Token::ScalarVar("_".into()));
            }
            Some(b) if b.is_ascii_alphabetic() => {
                let name = self.scan_ident();
                return Ok(Token::ScalarVar(name));
            }
            Some(b'{') => {
                // ${^Foo} — demarcated caret variable
                if self.peek_byte_at(1) == Some(b'^') {
                    self.skip(2); // skip { and ^
                    let ident_start = self.line_pos();
                    while let Some(b) = self.peek_byte(false) {
                        if b.is_ascii_alphanumeric() || b == b'_' {
                            self.skip(1);
                        } else {
                            break;
                        }
                    }
                    let ident = self.line_slice_str(ident_start)?;
                    let name = format!("^{ident}");
                    if self.peek_byte(false) == Some(b'}') {
                        self.skip(1);
                    }
                    return Ok(Token::SpecialVar(name));
                }
                // ${name} — variable with brace disambiguation
                // ${$ref} or ${expr} — dereference block (return Dollar, let parser handle {})
                if self.peek_byte_at(1).is_some_and(|b| b == b'_' || b.is_ascii_alphabetic()) {
                    self.skip(1); // skip {
                    let name = self.scan_ident();
                    if self.peek_byte(false) == Some(b'}') {
                        self.skip(1);
                    }
                    return Ok(Token::ScalarVar(name));
                }
                // Not a simple identifier — deref block
                return Ok(Token::Dollar);
            }
            Some(b'$') => {
                // $$name is scalar dereference; $$ alone is PID.
                // Return Dollar (deref prefix) if the byte after the second $
                // could start any variable expression ($name, ${expr}, $0,
                // $$nested, $!, etc.).  Only return SpecialVar("$") (PID)
                // when nothing variable-like follows.
                if let Some(b) = self.peek_byte_at(1)
                    && (b == b'_'
                        || b.is_ascii_alphabetic()
                        || b == b'{'
                        || b == b'$'
                        || b.is_ascii_digit()
                        || b == b'!'
                        || b == b'@'
                        || b == b'/'
                        || b == b'\\'
                        || b == b';'
                        || b == b','
                        || b == b'^'
                        || b == b'+'
                        || b == b'-'
                        || b == b'#'
                        || b == b'&'
                        || b == b'"'
                        || b == b'.'
                        || b == b'|'
                        || b == b'?'
                        || b == b'`'
                        || b == b'\''
                        || b == b'('
                        || b == b')'
                        || b == b'<'
                        || b == b'>'
                        || b == b']'
                        || b == b'%'
                        || b == b':'
                        || b == b'='
                        || b == b'~')
                {
                    return Ok(Token::Dollar);
                }
                self.skip(1);
                return Ok(Token::SpecialVar("$".into()));
            }
            Some(b'^') => {
                // $^X — caret variable (single character after ^)
                if let Some(next) = self.peek_byte_at(1)
                    && (next.is_ascii_alphabetic() || next == b'[' || next == b']')
                {
                    self.skip(2); // skip ^ and the character
                    let name = format!("^{}", next as char);
                    return Ok(Token::SpecialVar(name));
                }
                // Bare $^ — not a caret variable
                return Ok(Token::Dollar);
            }
            Some(b'!') => {
                self.skip(1);
                return Ok(Token::SpecialVar("!".into()));
            }
            Some(b'@') => {
                self.skip(1);
                return Ok(Token::SpecialVar("@".into()));
            }
            Some(b'/') => {
                self.skip(1);
                return Ok(Token::SpecialVar("/".into()));
            }
            Some(b'\\') => {
                self.skip(1);
                return Ok(Token::SpecialVar("\\".into()));
            }
            Some(b';') => {
                self.skip(1);
                return Ok(Token::SpecialVar(";".into()));
            }
            Some(b',') => {
                self.skip(1);
                return Ok(Token::SpecialVar(",".into()));
            }
            Some(b'+') => {
                self.skip(1);
                return Ok(Token::SpecialVar("+".into()));
            }
            Some(b'-') => {
                self.skip(1);
                return Ok(Token::SpecialVar("-".into()));
            }
            Some(b'&') => {
                self.skip(1);
                return Ok(Token::SpecialVar("&".into()));
            }
            // ── perlvar: remaining punctuation special variables ──
            Some(b'"') => {
                // $" — list separator for array interpolation
                self.skip(1);
                return Ok(Token::SpecialVar("\"".into()));
            }
            Some(b'.') => {
                // $. — current line number
                self.skip(1);
                return Ok(Token::SpecialVar(".".into()));
            }
            Some(b'|') => {
                // $| — output autoflush
                self.skip(1);
                return Ok(Token::SpecialVar("|".into()));
            }
            Some(b'?') => {
                // $? — child process status
                self.skip(1);
                return Ok(Token::SpecialVar("?".into()));
            }
            Some(b'`') => {
                // $` — prematch string
                self.skip(1);
                return Ok(Token::SpecialVar("`".into()));
            }
            Some(b'\'') => {
                // $' — postmatch string
                self.skip(1);
                return Ok(Token::SpecialVar("'".into()));
            }
            Some(b'(') => {
                // $( — real GID
                self.skip(1);
                return Ok(Token::SpecialVar("(".into()));
            }
            Some(b')') => {
                // $) — effective GID
                self.skip(1);
                return Ok(Token::SpecialVar(")".into()));
            }
            Some(b'<') => {
                // $< — real UID
                self.skip(1);
                return Ok(Token::SpecialVar("<".into()));
            }
            Some(b'>') => {
                // $> — effective UID
                self.skip(1);
                return Ok(Token::SpecialVar(">".into()));
            }
            Some(b']') => {
                // $] — Perl version
                self.skip(1);
                return Ok(Token::SpecialVar("]".into()));
            }
            Some(b'%') => {
                // $% — page number (format)
                self.skip(1);
                return Ok(Token::SpecialVar("%".into()));
            }
            Some(b':') => {
                // $: — format break characters
                self.skip(1);
                return Ok(Token::SpecialVar(":".into()));
            }
            Some(b'=') => {
                // $= — page length (format)
                self.skip(1);
                return Ok(Token::SpecialVar("=".into()));
            }
            Some(b'~') => {
                // $~ — format name
                self.skip(1);
                return Ok(Token::SpecialVar("~".into()));
            }
            Some(b) if b.is_ascii_digit() => {
                let start = self.line_pos();
                while self.peek_byte(false).is_some_and(|b| b.is_ascii_digit()) {
                    self.skip(1);
                }
                let name = self.line_slice_str(start)?;
                return Ok(Token::SpecialVar(name.into()));
            }
            _ => {}
        }

        Ok(Token::Dollar)
    }

    fn lex_at(&mut self) -> Result<Token, ParseError> {
        self.skip(1); // skip @
        match self.peek_byte(false) {
            Some(b'{') if self.peek_byte_at(1) == Some(b'^') => {
                // @{^CAPTURE} etc.
                self.skip(2); // skip { and ^
                let ident_start = self.line_pos();
                while let Some(b) = self.peek_byte(false) {
                    if b.is_ascii_alphanumeric() || b == b'_' {
                        self.skip(1);
                    } else {
                        break;
                    }
                }
                let ident = self.line_slice_str(ident_start)?;
                let name = format!("^{ident}");
                if self.peek_byte(false) == Some(b'}') {
                    self.skip(1);
                }
                Ok(Token::SpecialArrayVar(name))
            }
            Some(b'+') => {
                self.skip(1);
                Ok(Token::SpecialArrayVar("+".into()))
            }
            Some(b'-') => {
                self.skip(1);
                Ok(Token::SpecialArrayVar("-".into()))
            }
            Some(b) if b == b'_' || b.is_ascii_alphabetic() => {
                let name = self.scan_ident();
                Ok(Token::ArrayVar(name))
            }
            _ => Ok(Token::At),
        }
    }

    fn lex_percent(&mut self) -> Result<Token, ParseError> {
        // Always return Percent or ModEq.  The parser, in term
        // position, calls lex_hash_var_after_percent to attempt
        // hash-variable detection.
        self.skip(1);
        if self.peek_byte(false) == Some(b'=') {
            self.skip(1);
            Ok(Token::Assign(AssignOp::ModEq))
        } else {
            Ok(Token::Percent)
        }
    }

    /// Called by the parser after consuming a `Percent` token in
    /// term position.  Attempts to read a hash variable name and
    /// returns the appropriate token, or `None` if `%` is not
    /// followed by a valid hash name (in which case `%` is an
    /// invalid standalone term).
    pub fn lex_hash_var_after_percent(&mut self) -> Result<Option<Token>, ParseError> {
        match self.peek_byte(false) {
            Some(b'{') if self.peek_byte_at(1) == Some(b'^') => {
                // %{^CAPTURE} etc.
                self.skip(2); // skip { and ^
                let ident_start = self.line_pos();
                while let Some(b) = self.peek_byte(false) {
                    if b.is_ascii_alphanumeric() || b == b'_' {
                        self.skip(1);
                    } else {
                        break;
                    }
                }
                let ident = self.line_slice_str(ident_start)?;
                let name = format!("^{ident}");
                if self.peek_byte(false) == Some(b'}') {
                    self.skip(1);
                }
                Ok(Some(Token::SpecialHashVar(name)))
            }
            Some(b'!') => {
                self.skip(1);
                Ok(Some(Token::SpecialHashVar("!".into())))
            }
            Some(b'+') => {
                self.skip(1);
                Ok(Some(Token::SpecialHashVar("+".into())))
            }
            Some(b'-') => {
                self.skip(1);
                Ok(Some(Token::SpecialHashVar("-".into())))
            }
            Some(b) if b == b'_' || b.is_ascii_alphabetic() => {
                let name = self.scan_ident();
                Ok(Some(Token::HashVar(name)))
            }
            _ => Ok(None),
        }
    }

    // ── Identifiers ───────────────────────────────────────────

    fn scan_ident(&mut self) -> String {
        let start = self.line_pos();
        while let Some(b) = self.peek_byte(false) {
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.skip(1);
            } else if b == b':' && self.peek_byte_at(1) == Some(b':') {
                // Package separator Foo::Bar
                self.skip(2);
            } else {
                break;
            }
        }
        String::from_utf8_lossy(self.line_slice(start)).into_owned()
    }

    fn lex_word(&mut self) -> Result<Token, ParseError> {
        let name = self.scan_ident();

        // Word operators (eq, ne, lt, gt, le, ge, cmp, x, and, or,
        // xor, not) are always emitted as Ident(name).  The parser
        // recognizes them as operators in operator context via
        // peek_op_info and token_to_binop.

        // q// qq// qw// qr// m// s/// tr/// y///
        match name.as_str() {
            "q" if self.at_quote_delimiter() => return self.lex_q_string(),
            "qq" if self.at_quote_delimiter() => return self.lex_qq_string(),
            "qw" if self.at_quote_delimiter() => return self.lex_qw(),
            "qr" if self.at_quote_delimiter() => return self.lex_qr(),
            "m" if self.at_quote_delimiter() => return self.lex_m(),
            "s" if self.at_quote_delimiter() => return self.lex_s(),
            "tr" if self.at_quote_delimiter() => return self.lex_tr(),
            "y" if self.at_quote_delimiter() => return self.lex_tr(),
            "qx" if self.at_quote_delimiter() => return self.lex_qx(),
            _ => {}
        }

        // Special tokens
        match name.as_str() {
            "__FILE__" | "__LINE__" | "__PACKAGE__" | "__SUB__" => {
                return Ok(Token::Ident(name));
            }
            "__END__" | "__DATA__" => {
                let marker = if name == "__END__" { DataEndMarker::End } else { DataEndMarker::Data };
                return Ok(Token::DataEnd(marker));
            }
            _ => {}
        }

        // v-strings: v5, v5.26, v5.26.0 etc.
        if name.starts_with('v') && name.len() > 1 && name[1..].bytes().all(|b| b.is_ascii_digit()) {
            let mut vstr = name.clone();
            while self.peek_byte(false) == Some(b'.') {
                // Check that a digit follows the dot
                if self.peek_byte_at(1).is_some_and(|b| b.is_ascii_digit()) {
                    vstr.push('.');
                    self.skip(1); // skip '.'
                    let start = self.line_pos();
                    while self.peek_byte(false).is_some_and(|b| b.is_ascii_digit()) {
                        self.skip(1);
                    }
                    vstr.push_str(self.line_slice_str(start)?);
                } else {
                    break;
                }
            }
            return Ok(Token::StrLit(vstr));
        }

        // Keywords
        if let Some(kw) = keyword::lookup_keyword(&name) {
            return Ok(Token::Keyword(kw));
        }

        // Regular identifier / bareword
        Ok(Token::Ident(name))
    }

    fn at_quote_delimiter(&self) -> bool {
        match self.peek_byte_at(0) {
            Some(b) => b != b'\n' && !b.is_ascii_alphanumeric() && b != b'_',
            None => false,
        }
    }

    // ── Strings ───────────────────────────────────────────────

    fn lex_single_quoted_string(&mut self) -> Result<Token, ParseError> {
        self.skip(1); // skip opening '
        let s = self.lex_body_str(b'\'', false)?;
        Ok(Token::StrLit(s))
    }

    // ── Unified string/regex body scanner (§5.4) ──────────────────

    /// Scan one token from a string/regex body.
    ///
    /// In interpolating mode (called repeatedly via context stack):
    /// returns one sub-token per call — `ConstSegment`, `InterpScalar`,
    /// `InterpScalarExprStart`, etc.  Returns `QuoteEnd` when the
    /// closing delimiter is reached.
    ///
    /// In non-interpolating mode (called once by q//, '...', etc.):
    /// scans the entire body and returns a single `ConstSegment`.
    /// The closing delimiter is consumed.
    ///
    /// Escape handling is controlled by the flags:
    /// - `!raw && !interpolating`: literal escapes (`\\`→`\`,
    ///   `\delim`→delim).  For `q//`, `'...'`.
    /// - `!raw && interpolating`: double-quote escapes (`\n`,
    ///   `\t`, etc.) via `process_escape`.  For `qq//`, `"..."`.
    /// - `raw`: passthrough (backslash prevents delimiter
    ///   matching but both bytes are kept).  For `m//`, `tr//`.
    /// - `regex`: detect `(?{...})` code blocks (future).
    fn lex_body(&mut self, delim: Option<u8>, depth: u32, interpolating: bool, regex: bool, raw: bool) -> Result<Spanned, ParseError> {
        // Compute open/close from the delimiter.
        // None means heredoc (no delimiter — end signaled by LexerSource).
        let (open, close) = match delim {
            Some(d) => {
                let (o, c) = matching_delimiter(d);
                (o, Some(c))
            }
            None => (None, None),
        };

        // peek_byte(false) auto-loads the next line, consuming
        // the virtual EOF signal if pending (heredoc or subst body).
        let b = match self.peek_byte(false) {
            Some(b) => b,
            None => {
                let pos = self.span_pos();
                if close.is_none() {
                    // Heredoc or subst body finished (virtual EOF).
                    self.context_stack.pop();
                    return Ok(Spanned { token: Token::QuoteEnd, span: Span::new(pos, pos) });
                }
                return Err(ParseError::new("unterminated string", Span::new(pos, pos)));
            }
        };

        // Compute start AFTER peek_byte, so the position reflects
        // the loaded line (not a stale source cursor).
        let start = self.span_pos();

        // Fast dispatch for closing delimiter (incremental mode:
        // context on the stack → pop and return QuoteEnd).
        if let Some(c) = close
            && b == c
            && depth == 0
            && !self.context_stack.is_empty()
        {
            self.skip(1);
            self.context_stack.pop();
            return Ok(Spanned { token: Token::QuoteEnd, span: Span::new(start, self.span_pos()) });
        }
        if interpolating {
            if b == b'$' {
                return self.lex_interp_scalar(start);
            }
            if b == b'@' {
                return self.lex_interp_array(start);
            }
        }

        // Regex code blocks: (?{...}) and (??{...}).
        if regex && b == b'(' && self.peek_byte_at(1) == Some(b'?') {
            if self.peek_byte_at(2) == Some(b'{') {
                // (?{ — consume 3 bytes, enter code mode.
                self.skip(3);
                if let Some(ctx) = self.context_stack.last_mut() {
                    ctx.expr_depth = 1;
                }
                return Ok(Spanned { token: Token::RegexCodeStart, span: Span::new(start, self.span_pos()) });
            }
            if self.peek_byte_at(2) == Some(b'?') && self.peek_byte_at(3) == Some(b'{') {
                // (??{ — consume 4 bytes, enter code mode.
                self.skip(4);
                if let Some(ctx) = self.context_stack.last_mut() {
                    ctx.expr_depth = 1;
                }
                return Ok(Spanned { token: Token::RegexCondCodeStart, span: Span::new(start, self.span_pos()) });
            }
        }

        // Scan a ConstSegment: everything until we hit the closing
        // delimiter (or $/@/end-of-line in interpolating mode).
        let mut s = String::new();
        let mut current_depth = depth;

        loop {
            match self.peek_byte(true) {
                None => {
                    // EOF or virtual EOF (peeked).  For delimited
                    // strings (close is Some), this means the closing
                    // delimiter was not found — that's an error.
                    if close.is_some() {
                        return Err(ParseError::new("unterminated string", Span::new(start, self.span_pos())));
                    }
                    break;
                }
                Some(b) if Some(b) == close && current_depth == 0 => {
                    if self.context_stack.is_empty() {
                        // lex_body_str mode: consume the closing delimiter.
                        self.skip(1);
                    }
                    // Incremental mode: leave the delimiter for
                    // the QuoteEnd fast dispatch on the next call.
                    break;
                }
                Some(b'$') | Some(b'@') if interpolating => break,
                // Regex code block lookahead: break so fast dispatch
                // handles it on the next call.
                Some(b'(')
                    if regex
                        && self.peek_byte_at(1) == Some(b'?')
                        && (self.peek_byte_at(2) == Some(b'{') || (self.peek_byte_at(2) == Some(b'?') && self.peek_byte_at(3) == Some(b'{'))) =>
                {
                    break;
                }
                Some(b'\\') => {
                    self.skip(1);
                    if raw {
                        // Raw: backslash prevents delimiter matching.
                        // For \delim, consume the delimiter (backslash
                        // dropped).  For everything else, keep both.
                        if let Some(next) = self.peek_byte(false) {
                            if Some(next) == close || Some(next) == open {
                                self.skip(1);
                                s.push(next as char);
                            } else {
                                s.push('\\');
                            }
                        } else {
                            s.push('\\');
                        }
                    } else if interpolating {
                        // Double-quote escapes.
                        self.process_escape(&mut s, close);
                    } else {
                        // Literal (single-quote) escapes.
                        match self.peek_byte(false) {
                            Some(b'\\') => {
                                self.skip(1);
                                s.push('\\');
                            }
                            Some(b) if Some(b) == close => {
                                self.skip(1);
                                s.push(b as char);
                            }
                            Some(b) if Some(b) == open => {
                                self.skip(1);
                                s.push(b as char);
                            }
                            _ => s.push('\\'),
                        }
                    }
                }
                Some(b) if Some(b) == open => {
                    current_depth += 1;
                    self.skip(1);
                    s.push(b as char);
                }
                Some(b) if Some(b) == close && current_depth > 0 => {
                    current_depth -= 1;
                    self.skip(1);
                    s.push(b as char);
                }
                Some(b) => {
                    self.skip(1);
                    s.push(b as char);
                }
            }
        }

        // Update depth in context stack (only relevant for
        // interpolating mode with paired delimiters).
        if interpolating && let Some(ctx) = self.context_stack.last_mut() {
            ctx.depth = current_depth;
        }

        Ok(Spanned { token: Token::ConstSegment(s), span: Span::new(start, self.span_pos()) })
    }

    /// Non-interpolating convenience: scan the entire body and return
    /// the content as a String.  The closing delimiter is consumed.
    ///
    /// `raw` selects escape handling:
    /// - `false`: single-quote escapes (`\\`→`\`, `\delim`→delim).
    /// - `true`: raw passthrough (`\delim`→delim, else pass through).
    pub fn lex_body_str(&mut self, delim: u8, raw: bool) -> Result<String, ParseError> {
        let spanned = self.lex_body(Some(delim), 0, false, false, raw)?;
        match spanned.token {
            Token::ConstSegment(s) => Ok(s),
            _ => unreachable!("lex_body in non-interpolating mode should return ConstSegment"),
        }
    }

    /// Process a backslash escape inside a double-quoted string.
    /// The backslash has already been consumed.
    fn process_escape(&mut self, s: &mut String, close: Option<u8>) {
        match self.peek_byte(false) {
            Some(b'n') => {
                self.skip(1);
                s.push('\n');
            }
            Some(b't') => {
                self.skip(1);
                s.push('\t');
            }
            Some(b'r') => {
                self.skip(1);
                s.push('\r');
            }
            Some(b'\\') => {
                self.skip(1);
                s.push('\\');
            }
            Some(b'$') => {
                self.skip(1);
                s.push('$');
            }
            Some(b'@') => {
                self.skip(1);
                s.push('@');
            }
            Some(b'0') => {
                self.skip(1);
                s.push('\0');
            }
            Some(b'a') => {
                self.skip(1);
                s.push('\x07');
            }
            Some(b'b') => {
                self.skip(1);
                s.push('\x08');
            }
            Some(b'f') => {
                self.skip(1);
                s.push('\x0C');
            }
            Some(b'e') => {
                self.skip(1);
                s.push('\x1B');
            }
            Some(b) if Some(b) == close => {
                self.skip(1);
                s.push(b as char);
            }
            Some(b'x') => {
                self.skip(1);
                let mut val = 0u8;
                if self.peek_byte(false) == Some(b'{') {
                    // \x{HH...} — Unicode escape
                    self.skip(1);
                    let mut n = 0u32;
                    while let Some(b) = self.peek_byte(false) {
                        if b == b'}' {
                            self.skip(1);
                            break;
                        }
                        if b.is_ascii_hexdigit() {
                            self.skip(1);
                            n = n * 16 + hex_digit(b) as u32;
                        } else {
                            break;
                        }
                    }
                    if let Some(c) = char::from_u32(n) {
                        s.push(c);
                    }
                } else {
                    // \xHH
                    for _ in 0..2 {
                        if let Some(b) = self.peek_byte(false) {
                            if b.is_ascii_hexdigit() {
                                self.skip(1);
                                val = val * 16 + hex_digit(b);
                            } else {
                                break;
                            }
                        }
                    }
                    s.push(val as char);
                }
            }
            _ => s.push('\\'),
        }
    }

    /// Lex `$name`, `${name}`, or `${expr}` interpolation inside a string.
    fn lex_interp_scalar(&mut self, start: u32) -> Result<Spanned, ParseError> {
        self.skip(1); // skip $

        // ${...} form
        if self.peek_byte(false) == Some(b'{') {
            self.skip(1); // skip {
            // Simple identifier: ${name}
            if self.peek_byte(false).is_some_and(|b| b == b'_' || b.is_ascii_alphabetic()) {
                let saved_pos = self.line_pos();
                let name = self.scan_ident();
                if self.peek_byte(false) == Some(b'}') {
                    self.skip(1);
                    return Ok(Spanned { token: Token::InterpScalar(name), span: Span::new(start, self.span_pos()) });
                }
                // Not a simple ${name} — backtrack and scan as expression
                if let Some(line) = self.current_line.as_mut() {
                    line.pos = saved_pos;
                }
            }
            // Expression interpolation: ${\ expr}, ${$ref}, etc.
            // Enter expression-parsing mode — normal code until }.
            if let Some(ctx) = self.context_stack.last_mut() {
                ctx.expr_depth = 1;
            }
            return Ok(Spanned { token: Token::InterpScalarExprStart, span: Span::new(start, self.span_pos()) });
        }

        // $name form — must start with alpha or _
        if self.peek_byte(false).is_some_and(|b| b == b'_' || b.is_ascii_alphabetic()) {
            let name = self.scan_ident();
            return Ok(Spanned { token: Token::InterpScalar(name), span: Span::new(start, self.span_pos()) });
        }

        // Bare $ not followed by a name — treat as literal
        Ok(Spanned { token: Token::ConstSegment("$".into()), span: Span::new(start, self.span_pos()) })
    }

    /// Lex `@name` or `@{expr}` interpolation inside a string.
    fn lex_interp_array(&mut self, start: u32) -> Result<Spanned, ParseError> {
        self.skip(1); // skip @

        // @{...} form — expression interpolation: @{[ expr ]}
        if self.peek_byte(false) == Some(b'{') {
            self.skip(1); // skip {
            if let Some(ctx) = self.context_stack.last_mut() {
                ctx.expr_depth = 1;
            }
            return Ok(Spanned { token: Token::InterpArrayExprStart, span: Span::new(start, self.span_pos()) });
        }

        // @name form
        if self.peek_byte(false).is_some_and(|b| b == b'_' || b.is_ascii_alphabetic()) {
            let name = self.scan_ident();
            return Ok(Spanned { token: Token::InterpArray(name), span: Span::new(start, self.span_pos()) });
        }

        // Bare @ not followed by a name — treat as literal
        Ok(Spanned { token: Token::ConstSegment("@".into()), span: Span::new(start, self.span_pos()) })
    }

    // ── q// qq// qw// ─────────────────────────────────────────

    fn lex_q_string(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;
        let s = self.lex_body_str(delim, false)?;
        Ok(Token::StrLit(s))
    }

    fn lex_qq_string(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;
        self.context_stack.push(LexContext { delim: Some(delim), depth: 0, expr_depth: 0, interpolating: true, raw: false, regex: false });
        Ok(Token::QuoteBegin(QuoteKind::Double, delim))
    }

    fn lex_qx(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;
        self.context_stack.push(LexContext { delim: Some(delim), depth: 0, expr_depth: 0, interpolating: true, raw: false, regex: false });
        Ok(Token::QuoteBegin(QuoteKind::Backtick, delim))
    }

    fn lex_qw(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;
        // qw is documented as equivalent to split(' ', q/.../).
        // Use literal (q//) escape mode: \\ → \, \delim → delim.
        let body = self.lex_body_str(delim, false)?;
        let words: Vec<String> = body.split_whitespace().map(String::from).collect();
        Ok(Token::QwList(words))
    }

    // ── Regex and friends ─────────────────────────────────────

    /// `m/pattern/flags` or `m{pattern}flags`
    fn lex_m(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;
        self.context_stack.push(LexContext { delim: Some(delim), depth: 0, expr_depth: 0, interpolating: delim != b'\'', raw: true, regex: true });
        Ok(Token::RegexBegin(RegexKind::Match, delim))
    }

    /// `qr/pattern/flags` or `qr{pattern}flags`
    fn lex_qr(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;
        self.context_stack.push(LexContext { delim: Some(delim), depth: 0, expr_depth: 0, interpolating: delim != b'\'', raw: true, regex: true });
        Ok(Token::RegexBegin(RegexKind::Qr, delim))
    }

    /// `s/pattern/replacement/flags` or `s{pattern}{replacement}flags`
    fn lex_s(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;

        // Push context for the pattern body (raw, regex mode).
        // Single-quote delimiter disables interpolation.
        // The parser will collect body tokens until QuoteEnd,
        // then call start_subst_replacement to set up the
        // replacement body.
        self.context_stack.push(LexContext { delim: Some(delim), depth: 0, expr_depth: 0, interpolating: delim != b'\'', raw: true, regex: true });

        Ok(Token::SubstBegin(delim))
    }

    /// Set up the replacement body of a substitution after the
    /// pattern has been consumed.  Called by the parser after
    /// collecting the pattern's QuoteEnd.
    ///
    /// For paired delimiters, reads the replacement delimiter.
    /// Scans ahead for flags via `start_subst_body`, then pushes
    /// the appropriate LexContext for the replacement body.
    /// Returns the captured flags.
    pub fn start_subst_replacement(&mut self, pattern_delim: u8) -> Result<Option<String>, ParseError> {
        let repl_delim = if is_paired(pattern_delim) { self.read_quote_delimiter()? } else { pattern_delim };

        let flags = self.source.start_subst_body(repl_delim, &mut self.current_line)?;
        let has_eval = flags.as_ref().is_some_and(|f| f.contains('e'));

        // Push context for the replacement body.
        // delim is None — the body ends at the virtual EOF set up
        // by start_subst_body, not at a delimiter byte.
        // With /e: raw scan (code, parser will reparse).
        // Without /e: interpolating string.
        self.context_stack.push(LexContext { delim: None, depth: 0, expr_depth: 0, interpolating: !has_eval, raw: has_eval, regex: false });

        Ok(flags)
    }

    /// `tr/from/to/flags` or `y/from/to/flags`
    fn lex_tr(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;
        let from = self.lex_body_str(delim, true)?;
        let to = if is_paired(delim) {
            let delim2 = self.read_quote_delimiter()?;
            self.lex_body_str(delim2, true)?
        } else {
            self.lex_body_str(delim, true)?
        };
        let flags = self.scan_adjacent_word_chars();
        Ok(Token::TranslitLit(from, to, flags))
    }

    /// Read the delimiter byte for a quote-like construct.
    /// Skips whitespace first if the current byte is whitespace.
    fn read_quote_delimiter(&mut self) -> Result<u8, ParseError> {
        // Match toke.c's scan_str: skip whitespace before the delimiter
        // only if the current byte IS whitespace (or the line is exhausted).
        // `m#foo#` uses `#` as the delimiter — it's not a comment.
        // `m /foo/` skips the space and uses `/`.
        match self.peek_byte(false) {
            Some(b) if b == b' ' || b == b'\t' => {
                self.skip_ws_and_comments()?;
            }
            None => {
                // End of line — need to cross to next line.
                self.skip_ws_and_comments()?;
            }
            _ => {}
        }
        self.advance_byte().ok_or_else(|| ParseError::new("expected delimiter", Span::new(self.span_pos(), self.span_pos())))
    }

    // ── Operators ─────────────────────────────────────────────

    fn lex_plus(&mut self) -> Token {
        self.skip(1);
        match self.peek_byte(false) {
            Some(b'+') => {
                self.skip(1);
                Token::PlusPlus
            }
            Some(b'=') => {
                self.skip(1);
                Token::Assign(AssignOp::AddEq)
            }
            _ => Token::Plus,
        }
    }

    fn lex_minus(&mut self) -> Token {
        self.skip(1);
        match self.peek_byte(false) {
            Some(b'-') => {
                self.skip(1);
                Token::MinusMinus
            }
            Some(b'=') => {
                self.skip(1);
                Token::Assign(AssignOp::SubEq)
            }
            Some(b'>') => {
                self.skip(1);
                Token::Arrow
            }
            _ => Token::Minus,
        }
    }

    /// Called by the parser after consuming a `Minus` token in term
    /// position.  Returns `Some(Filetest(b))` if the next byte is
    /// a single letter not followed by a word-continuation char
    /// (e.g. `-f $file`, `-d "/tmp"`).  Returns `None` otherwise.
    pub fn lex_filetest_after_minus(&mut self) -> Option<Token> {
        let b = self.peek_byte(false)?;
        if !b.is_ascii_alphabetic() {
            return None;
        }
        // Must not be followed by a word-continuation char.
        if self.peek_byte_at(1).is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') {
            return None;
        }
        self.skip(1);
        Some(Token::Filetest(b))
    }

    fn lex_star(&mut self) -> Token {
        self.skip(1);
        match self.peek_byte(false) {
            Some(b'*') => {
                self.skip(1);
                if self.peek_byte(false) == Some(b'=') {
                    self.skip(1);
                    Token::Assign(AssignOp::PowEq)
                } else {
                    Token::Power
                }
            }
            Some(b'=') => {
                self.skip(1);
                Token::Assign(AssignOp::MulEq)
            }
            _ => Token::Star,
        }
    }

    fn lex_slash(&mut self) -> Token {
        if self.peek_byte_at(1) == Some(b'/') {
            self.skip(2);
            if self.peek_byte(false) == Some(b'=') {
                self.skip(1);
                Token::Assign(AssignOp::DefinedOrEq)
            } else {
                Token::DefinedOr
            }
        } else {
            self.skip(1);
            if self.peek_byte(false) == Some(b'=') {
                self.skip(1);
                Token::Assign(AssignOp::DivEq)
            } else {
                Token::Slash
            }
        }
    }

    /// Consume adjacent ASCII word characters (letters, digits,
    /// underscore) without skipping whitespace first.  Returns `None`
    /// if the next byte is not a word character.  Used by the parser
    /// to collect regex and transliteration flags immediately after a
    /// closing delimiter.  Perl's flag scanner (`S_pmflag`) consumes
    /// all word characters and reports errors for invalid ones.
    pub fn scan_adjacent_word_chars(&mut self) -> Option<String> {
        let start = self.line_pos();
        while let Some(b) = self.peek_byte(false) {
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.skip(1);
            } else {
                break;
            }
        }
        let slice = self.line_slice(start);
        if slice.is_empty() {
            return None;
        }
        Some(String::from_utf8_lossy(slice).into_owned())
    }

    fn lex_dot(&mut self) -> Token {
        self.skip(1);
        match self.peek_byte(false) {
            Some(b'.') => {
                self.skip(1);
                if self.peek_byte(false) == Some(b'.') {
                    self.skip(1);
                    Token::DotDotDot
                } else {
                    Token::DotDot
                }
            }
            Some(b'=') => {
                self.skip(1);
                Token::Assign(AssignOp::ConcatEq)
            }
            _ => Token::Dot,
        }
    }

    fn lex_less_than(&mut self) -> Result<Token, ParseError> {
        self.skip(1); // consume first <
        match self.peek_byte(false) {
            Some(b'<') => {
                // Always return ShiftLeft / ShiftLeftEq.  The parser
                // handles heredoc detection in term position by
                // calling lex_heredoc_after_shift_left.
                self.skip(1);
                if self.peek_byte(false) == Some(b'=') {
                    self.skip(1);
                    Ok(Token::Assign(AssignOp::ShiftLeftEq))
                } else {
                    Ok(Token::ShiftLeft)
                }
            }
            Some(b'=') => {
                self.skip(1);
                if self.peek_byte(false) == Some(b'>') {
                    self.skip(1);
                    Ok(Token::Spaceship)
                } else {
                    Ok(Token::NumLe)
                }
            }
            _ => Ok(Token::NumLt),
        }
    }

    /// Called by the parser after consuming a `NumLt` token in term
    /// position.  Attempts to scan a readline/glob construct: `<...>`
    /// where the content ends at `>` on the same line.  Returns the
    /// `Readline(content)` token if successful, or `None` if no `>`
    /// terminates the content (the parser should then treat `<` as
    /// less-than).
    pub fn lex_readline_after_lt(&mut self) -> Option<Token> {
        let start_pos = self.line_pos();
        let mut content = String::new();
        let mut found_close = false;
        while let Some(b) = self.peek_byte(false) {
            if b == b'>' {
                self.skip(1);
                found_close = true;
                break;
            }
            if b == b'\n' {
                break;
            }
            self.skip(1);
            content.push(b as char);
        }
        if found_close {
            Some(Token::Readline(content))
        } else {
            // Not a readline — rewind.
            if let Some(line) = self.current_line.as_mut() {
                line.pos = start_pos;
            }
            None
        }
    }

    /// Called by the parser after consuming a `ShiftLeft` token in
    /// term position.  Attempts to read a heredoc tag (with optional
    /// `~` prefix) and start the heredoc body.  Returns the token
    /// produced (`QuoteBegin` for interpolating, `HeredocLit` for
    /// literal), or rewinds and returns `None` if no valid tag follows
    /// — the parser should then treat the `ShiftLeft` as a shift.
    pub fn lex_heredoc_after_shift_left(&mut self) -> Result<Option<Token>, ParseError> {
        let saved = self.line_pos();

        // <<~ for indented heredocs
        let indented = self.peek_byte(false) == Some(b'~');
        if indented {
            self.skip(1);
        }

        // Skip optional whitespace between << and tag.
        while self.peek_byte(false) == Some(b' ') || self.peek_byte(false) == Some(b'\t') {
            self.skip(1);
        }

        match self.peek_byte(false) {
            Some(b'"') | Some(b'\'') => Ok(Some(self.lex_heredoc(indented)?)),
            Some(b) if b == b'_' || b.is_ascii_alphabetic() => Ok(Some(self.lex_heredoc(indented)?)),
            _ => {
                // No valid tag — rewind to just after << so the
                // parser can proceed with a normal shift-left.
                if let Some(line) = self.current_line.as_mut() {
                    line.pos = saved;
                }
                Ok(None)
            }
        }
    }

    /// Lex a heredoc tag and start body processing via LexerSource.
    /// Position is after `<<` (and optional `~`), at the tag start.
    fn lex_heredoc(&mut self, indented: bool) -> Result<Token, ParseError> {
        let start = self.line_pos();

        // Determine quoting style and extract tag.
        let (kind, tag) = match self.peek_byte(false) {
            Some(b'\'') => {
                // <<'TAG' — literal
                self.skip(1);
                let tag = self.scan_heredoc_tag(b'\'')?;
                let k = if indented { HeredocKind::IndentedLiteral } else { HeredocKind::Literal };
                (k, tag)
            }
            Some(b'"') => {
                // <<"TAG" — interpolating (explicit)
                self.skip(1);
                let tag = self.scan_heredoc_tag(b'"')?;
                let k = if indented { HeredocKind::Indented } else { HeredocKind::Interpolating };
                (k, tag)
            }
            _ => {
                // Bare identifier — interpolating
                let tag_start = self.line_pos();
                while self.peek_byte(false).is_some_and(|b| b == b'_' || b.is_ascii_alphanumeric()) {
                    self.skip(1);
                }
                let tag = String::from_utf8_lossy(self.line_slice(tag_start)).into_owned();
                let k = if indented { HeredocKind::Indented } else { HeredocKind::Interpolating };
                (k, tag)
            }
        };

        if tag.is_empty() {
            return Err(ParseError::new("empty heredoc tag", self.span_from(start)));
        }

        let tag_bytes = Bytes::from(tag.as_bytes().to_vec());

        // Tell LexerSource to start the heredoc.  This takes the current
        // line (with cursor at the rest-of-line position) and begins
        // serving heredoc body lines on subsequent next_line() calls.
        match kind {
            HeredocKind::Interpolating => {
                self.source.start_heredoc(tag_bytes, &mut self.current_line)?;
                self.context_stack.push(LexContext { delim: None, depth: 0, expr_depth: 0, interpolating: true, raw: false, regex: false });
                Ok(Token::QuoteBegin(QuoteKind::Heredoc, 0))
            }
            HeredocKind::Indented => {
                self.source.start_indented_heredoc(tag_bytes, &mut self.current_line)?;
                self.context_stack.push(LexContext { delim: None, depth: 0, expr_depth: 0, interpolating: true, raw: false, regex: false });
                Ok(Token::QuoteBegin(QuoteKind::Heredoc, 0))
            }
            HeredocKind::Literal => {
                self.source.start_heredoc(tag_bytes, &mut self.current_line)?;
                self.collect_heredoc_literal(&tag, false)
            }
            HeredocKind::IndentedLiteral => {
                self.source.start_indented_heredoc(tag_bytes, &mut self.current_line)?;
                self.collect_heredoc_literal(&tag, true)
            }
        }
    }

    /// Collect a literal heredoc body as a raw string.
    /// LexerSource handles terminator detection and indent stripping.
    fn collect_heredoc_literal(&mut self, tag: &str, indented: bool) -> Result<Token, ParseError> {
        let mut body = String::new();
        while let Some(line) = self.source.next_line(false)? {
            body.push_str(&String::from_utf8_lossy(&line.line));
            body.push('\n');
        }
        let kind = if indented { HeredocKind::IndentedLiteral } else { HeredocKind::Literal };
        Ok(Token::HeredocLit(kind, tag.to_string(), body))
    }

    /// Scan a quoted heredoc tag (between matching quotes).
    fn scan_heredoc_tag(&mut self, close: u8) -> Result<String, ParseError> {
        let start = self.line_pos();
        while self.peek_byte(false).is_some_and(|b| b != close) {
            self.skip(1);
        }
        let tag = String::from_utf8_lossy(self.line_slice(start)).into_owned();
        if self.peek_byte(false) == Some(close) {
            self.skip(1); // skip closing quote
        }
        Ok(tag)
    }

    fn lex_greater_than(&mut self) -> Token {
        self.skip(1);
        match self.peek_byte(false) {
            Some(b'>') => {
                self.skip(1);
                if self.peek_byte(false) == Some(b'=') {
                    self.skip(1);
                    Token::Assign(AssignOp::ShiftRightEq)
                } else {
                    Token::ShiftRight
                }
            }
            Some(b'=') => {
                self.skip(1);
                Token::NumGe
            }
            _ => Token::NumGt,
        }
    }

    fn lex_equals(&mut self) -> Token {
        self.skip(1);
        match self.peek_byte(false) {
            Some(b'=') => {
                self.skip(1);
                Token::NumEq
            }
            Some(b'~') => {
                self.skip(1);
                Token::Binding
            }
            Some(b'>') => {
                self.skip(1);
                Token::FatComma
            }
            _ => Token::Assign(AssignOp::Eq),
        }
    }

    fn lex_bang(&mut self) -> Token {
        self.skip(1);
        match self.peek_byte(false) {
            Some(b'=') => {
                self.skip(1);
                Token::NumNe
            }
            Some(b'~') => {
                self.skip(1);
                Token::NotBinding
            }
            _ => Token::Bang,
        }
    }

    fn lex_ampersand(&mut self) -> Token {
        self.skip(1);
        match self.peek_byte(false) {
            Some(b'&') => {
                self.skip(1);
                if self.peek_byte(false) == Some(b'=') {
                    self.skip(1);
                    Token::Assign(AssignOp::AndEq)
                } else {
                    Token::AndAnd
                }
            }
            Some(b'=') => {
                self.skip(1);
                Token::Assign(AssignOp::BitAndEq)
            }
            _ => Token::BitAnd,
        }
    }

    fn lex_pipe(&mut self) -> Token {
        self.skip(1);
        match self.peek_byte(false) {
            Some(b'|') => {
                self.skip(1);
                if self.peek_byte(false) == Some(b'=') {
                    self.skip(1);
                    Token::Assign(AssignOp::OrEq)
                } else {
                    Token::OrOr
                }
            }
            Some(b'=') => {
                self.skip(1);
                Token::Assign(AssignOp::BitOrEq)
            }
            _ => Token::BitOr,
        }
    }
}

/// Return the (open, close) delimiter pair for a given delimiter.
/// For paired brackets, open is `Some(delim)` and close is the
/// matching bracket.  For same-char delimiters, open is `None`.
pub fn matching_delimiter(delim: u8) -> (Option<u8>, u8) {
    match delim {
        b'(' => (Some(b'('), b')'),
        b'[' => (Some(b'['), b']'),
        b'{' => (Some(b'{'), b'}'),
        b'<' => (Some(b'<'), b'>'),
        _ => (None, delim),
    }
}

/// Whether a delimiter is a paired bracket.
pub fn is_paired(delim: u8) -> bool {
    matching_delimiter(delim).0.is_some()
}

fn hex_digit(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn lex_all(src: &str) -> Vec<Token> {
        let mut lexer = Lexer::new(src.as_bytes());
        let mut term_context = true; // start of statement is term context
        let mut tokens = Vec::new();
        loop {
            let mut spanned = lexer.lex_token().unwrap();
            if matches!(spanned.token, Token::Eof) {
                break;
            }
            // In term context, ShiftLeft may introduce a heredoc —
            // mimic what the parser does by calling the heredoc hook.
            if matches!(spanned.token, Token::ShiftLeft)
                && term_context
                && let Some(tok) = lexer.lex_heredoc_after_shift_left().unwrap()
            {
                spanned.token = tok;
            }
            // In term context, Percent may introduce a hash variable.
            if matches!(spanned.token, Token::Percent)
                && term_context
                && let Some(tok) = lexer.lex_hash_var_after_percent().unwrap()
            {
                spanned.token = tok;
            }
            // In term context, Minus may introduce a filetest operator.
            if matches!(spanned.token, Token::Minus)
                && term_context
                && let Some(tok) = lexer.lex_filetest_after_minus()
            {
                spanned.token = tok;
            }
            // In term context, NumLt may introduce a readline/glob.
            if matches!(spanned.token, Token::NumLt)
                && term_context
                && let Some(tok) = lexer.lex_readline_after_lt()
            {
                spanned.token = tok;
            }
            // Update term/operator state based on the token.
            match &spanned.token {
                Token::IntLit(_)
                | Token::FloatLit(_)
                | Token::StrLit(_)
                | Token::ScalarVar(_)
                | Token::ArrayVar(_)
                | Token::HashVar(_)
                | Token::Ident(_)
                | Token::RightParen
                | Token::RightBracket
                | Token::RightBrace
                | Token::PlusPlus
                | Token::MinusMinus
                | Token::SpecialVar(_)
                | Token::ArrayLen(_)
                | Token::QuoteEnd
                | Token::TranslitLit(_, _, _)
                | Token::HeredocLit(_, _, _)
                | Token::Readline(_)
                | Token::GlobVar(_)
                | Token::QwList(_)
                | Token::SpecialArrayVar(_)
                | Token::SpecialHashVar(_)
                | Token::Arrow => {
                    term_context = false;
                }
                // Sub-tokens inside strings/regex don't change context.
                Token::QuoteBegin(_, _)
                | Token::RegexBegin(_, _)
                | Token::SubstBegin(_)
                | Token::ConstSegment(_)
                | Token::InterpScalar(_)
                | Token::InterpArray(_)
                | Token::InterpScalarExprStart
                | Token::InterpArrayExprStart
                | Token::RegexCodeStart
                | Token::RegexCondCodeStart => {}
                _ => {
                    term_context = true;
                }
            }
            tokens.push(spanned.token);
        }
        tokens
    }

    // ── CR normalization ────────────────────────────────────────
    // Note: normalize_crlf is now handled by LexerSource.
    // Low-level tests are in source.rs; these test high-level behavior.

    #[test]
    fn lex_crlf_same_as_lf() {
        // CRLF source should produce identical tokens to LF source.
        let lf_tokens = lex_all("my $x = 1;\nmy $y = 2;\n");
        let crlf_tokens = lex_all("my $x = 1;\r\nmy $y = 2;\r\n");
        assert_eq!(lf_tokens, crlf_tokens);
    }

    #[test]
    fn lex_crlf_heredoc() {
        // Heredoc with CRLF line endings should work identically to LF.
        let lf_tokens = lex_all("<<END;\nhello\nEND\n");
        let crlf_tokens = lex_all("<<END;\r\nhello\r\nEND\r\n");
        assert_eq!(lf_tokens, crlf_tokens);
    }

    #[test]
    fn lex_cr_only_not_treated_as_newline() {
        // Standalone \r is NOT a line ending.  This source has \r (not \r\n)
        // before the terminator, so "END" is not at line start and the
        // heredoc is unterminated — matching Perl's behavior.
        let src = b"<<END;\nhello\rEND\n";
        let mut lexer = Lexer::new(src);
        // Consume ShiftLeft, then call the heredoc hook.
        let tok = lexer.lex_token().unwrap();
        assert_eq!(tok.token, Token::ShiftLeft);
        let heredoc_tok = lexer.lex_heredoc_after_shift_left().unwrap().expect("expected heredoc");
        assert_eq!(heredoc_tok, Token::QuoteBegin(QuoteKind::Heredoc, 0));
        // Body line "hello\rEND\n" is not a terminator — returned as content.
        let tok = lexer.lex_token().unwrap();
        assert!(matches!(tok.token, Token::ConstSegment(_)));
        // Next call surfaces the deferred unterminated heredoc error.
        let result = lexer.lex_token();
        assert!(result.is_err(), "expected unterminated heredoc error");
    }

    // ── Indented heredoc indentation mismatch errors ──────────

    #[test]
    fn lex_indented_heredoc_mismatch_croaks() {
        // Body line with wrong indentation should error.
        let src = "<<~END;\n    hello\n  bad indent\n    END\n";
        let mut lexer = Lexer::new(src.as_bytes());
        // Consume ShiftLeft, then call the heredoc hook.
        lexer.lex_token().unwrap();
        lexer.lex_heredoc_after_shift_left().unwrap().expect("expected heredoc");
        // Consume tokens until we hit the error.
        let mut got_error = false;
        for _ in 0..20 {
            match lexer.lex_token() {
                Err(e) => {
                    assert!(e.message.contains("indent"), "expected indentation error, got: {}", e.message);
                    got_error = true;
                    break;
                }
                Ok(tok) if matches!(tok.token, Token::Eof) => break,
                Ok(_) => continue,
            }
        }
        assert!(got_error, "expected indentation mismatch error");
    }

    #[test]
    fn lex_indented_heredoc_tabs_vs_spaces_croaks() {
        // Terminator uses tab+spaces, body line uses only spaces — mismatch.
        let src = "<<~END;\n\t  hello\n    wrong\n\t  END\n";
        let mut lexer = Lexer::new(src.as_bytes());
        lexer.lex_token().unwrap();
        lexer.lex_heredoc_after_shift_left().unwrap().expect("expected heredoc");
        let mut got_error = false;
        for _ in 0..20 {
            match lexer.lex_token() {
                Err(e) => {
                    assert!(e.message.contains("indent"), "expected indentation error, got: {}", e.message);
                    got_error = true;
                    break;
                }
                Ok(tok) if matches!(tok.token, Token::Eof) => break,
                Ok(_) => continue,
            }
        }
        assert!(got_error, "expected indentation mismatch error");
    }

    #[test]
    fn lex_indented_heredoc_empty_line_ok() {
        // Empty lines (just \n) are allowed without indentation.
        let src = "<<~END;\n    hello\n\n    world\n    END\n";
        let tokens = lex_all(src);
        assert_eq!(tokens[0], Token::QuoteBegin(QuoteKind::Heredoc, 0));
        let body: String = tokens.iter().filter_map(|t| if let Token::ConstSegment(s) = t { Some(s.as_str()) } else { None }).collect();
        assert_eq!(body, "hello\n\nworld\n");
    }

    // ── Basic token tests ─────────────────────────────────────

    #[test]
    fn lex_simple_assignment() {
        let tokens = lex_all("my $x = 42;");
        assert_eq!(tokens, vec![Token::Keyword(Keyword::My), Token::ScalarVar("x".into()), Token::Assign(AssignOp::Eq), Token::IntLit(42), Token::Semi,]);
    }

    #[test]
    fn lex_arithmetic() {
        let tokens = lex_all("$a + $b * 3");
        assert_eq!(tokens, vec![Token::ScalarVar("a".into()), Token::Plus, Token::ScalarVar("b".into()), Token::Star, Token::IntLit(3),]);
    }

    #[test]
    fn lex_string_literals() {
        // Single-quoted: still emits StrLit (no interpolation).
        let tokens = lex_all("'hello'");
        assert_eq!(tokens, vec![Token::StrLit("hello".into())]);

        // Double-quoted: emits sub-token stream.
        let tokens = lex_all(r#""world\n""#);
        assert_eq!(tokens, vec![Token::QuoteBegin(QuoteKind::Double, b'"'), Token::ConstSegment("world\n".into()), Token::QuoteEnd,]);
    }

    #[test]
    fn lex_comparison_ops() {
        let tokens = lex_all("$a == $b != $c <= $d >= $e <=> $f");
        assert_eq!(
            tokens,
            vec![
                Token::ScalarVar("a".into()),
                Token::NumEq,
                Token::ScalarVar("b".into()),
                Token::NumNe,
                Token::ScalarVar("c".into()),
                Token::NumLe,
                Token::ScalarVar("d".into()),
                Token::NumGe,
                Token::ScalarVar("e".into()),
                Token::Spaceship,
                Token::ScalarVar("f".into()),
            ]
        );
    }

    #[test]
    fn lex_string_cmp_ops() {
        let tokens = lex_all("$a eq $b ne $c lt $d");
        assert_eq!(
            tokens,
            vec![
                Token::ScalarVar("a".into()),
                Token::Keyword(Keyword::Eq),
                Token::ScalarVar("b".into()),
                Token::Keyword(Keyword::Ne),
                Token::ScalarVar("c".into()),
                Token::Keyword(Keyword::Lt),
                Token::ScalarVar("d".into()),
            ]
        );
    }

    #[test]
    fn lex_arrow_and_deref() {
        let tokens = lex_all("$ref->{key}");
        assert_eq!(tokens, vec![Token::ScalarVar("ref".into()), Token::Arrow, Token::LeftBrace, Token::Ident("key".into()), Token::RightBrace,]);
    }

    #[test]
    fn lex_hex_literal() {
        let tokens = lex_all("0xFF");
        assert_eq!(tokens, vec![Token::IntLit(255)]);
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn lex_float() {
        let tokens = lex_all("3.14 1e10 2.5e-3");
        assert_eq!(tokens.len(), 3);
        assert!(matches!(tokens[0], Token::FloatLit(f) if (f - 3.14).abs() < 1e-10));
        assert!(matches!(tokens[1], Token::FloatLit(f) if (f - 1e10).abs() < 1.0));
        assert!(matches!(tokens[2], Token::FloatLit(f) if (f - 2.5e-3).abs() < 1e-10));
    }

    #[test]
    fn lex_qw() {
        let tokens = lex_all("qw(foo bar baz)");
        assert_eq!(tokens, vec![Token::QwList(vec!["foo".into(), "bar".into(), "baz".into()]),]);
    }

    #[test]
    fn lex_q_string() {
        let tokens = lex_all("q{hello world}");
        assert_eq!(tokens, vec![Token::StrLit("hello world".into())]);
    }

    #[test]
    fn lex_underscore_in_number() {
        let tokens = lex_all("1_000_000");
        assert_eq!(tokens, vec![Token::IntLit(1_000_000)]);
    }

    #[test]
    fn lex_power_op() {
        let tokens = lex_all("$x ** 2");
        assert_eq!(tokens, vec![Token::ScalarVar("x".into()), Token::Power, Token::IntLit(2),]);
    }

    #[test]
    fn lex_logical_ops() {
        let tokens = lex_all("$a && $b || $c // $d");
        assert_eq!(
            tokens,
            vec![
                Token::ScalarVar("a".into()),
                Token::AndAnd,
                Token::ScalarVar("b".into()),
                Token::OrOr,
                Token::ScalarVar("c".into()),
                Token::DefinedOr,
                Token::ScalarVar("d".into()),
            ]
        );
    }

    #[test]
    fn lex_print_hello() {
        let tokens = lex_all(r#"print "Hello, world!\n";"#);
        assert_eq!(
            tokens,
            vec![
                Token::Keyword(Keyword::Print),
                Token::QuoteBegin(QuoteKind::Double, b'"'),
                Token::ConstSegment("Hello, world!\n".into()),
                Token::QuoteEnd,
                Token::Semi,
            ]
        );
    }

    #[test]
    fn lex_comment() {
        let tokens = lex_all("42 # answer\n+ 1");
        assert_eq!(tokens, vec![Token::IntLit(42), Token::Plus, Token::IntLit(1)]);
    }

    #[test]
    fn lex_fat_comma() {
        let tokens = lex_all("foo => 42");
        // "foo" is an ident, "=>" is fat comma
        assert_eq!(tokens, vec![Token::Ident("foo".into()), Token::FatComma, Token::IntLit(42),]);
    }

    #[test]
    fn lex_package_qualified() {
        let tokens = lex_all("Foo::Bar::baz");
        assert_eq!(tokens, vec![Token::Ident("Foo::Bar::baz".into())]);
    }

    #[test]
    fn lex_special_vars() {
        let tokens = lex_all("$_ $0 $! $@");
        assert_eq!(tokens.len(), 4);
        assert_eq!(tokens[0], Token::ScalarVar("_".into()));
        assert_eq!(tokens[1], Token::SpecialVar("0".into()));
        assert_eq!(tokens[2], Token::SpecialVar("!".into()));
        assert_eq!(tokens[3], Token::SpecialVar("@".into()));
    }

    // ── Interpolation tests ───────────────────────────────────

    #[test]
    fn lex_interp_scalar() {
        let tokens = lex_all(r#""Hello, $name!""#);
        assert_eq!(
            tokens,
            vec![
                Token::QuoteBegin(QuoteKind::Double, b'"'),
                Token::ConstSegment("Hello, ".into()),
                Token::InterpScalar("name".into()),
                Token::ConstSegment("!".into()),
                Token::QuoteEnd,
            ]
        );
    }

    #[test]
    fn lex_interp_braced() {
        let tokens = lex_all(r#""${name}bar""#);
        assert_eq!(
            tokens,
            vec![Token::QuoteBegin(QuoteKind::Double, b'"'), Token::InterpScalar("name".into()), Token::ConstSegment("bar".into()), Token::QuoteEnd,]
        );
    }

    #[test]
    fn lex_interp_array() {
        let tokens = lex_all(r#""items: @list.""#);
        assert_eq!(
            tokens,
            vec![
                Token::QuoteBegin(QuoteKind::Double, b'"'),
                Token::ConstSegment("items: ".into()),
                Token::InterpArray("list".into()),
                Token::ConstSegment(".".into()),
                Token::QuoteEnd,
            ]
        );
    }

    #[test]
    fn lex_interp_escaped_sigil() {
        let tokens = lex_all(r#""price: \$100""#);
        assert_eq!(tokens, vec![Token::QuoteBegin(QuoteKind::Double, b'"'), Token::ConstSegment("price: $100".into()), Token::QuoteEnd,]);
    }

    #[test]
    fn lex_interp_no_interpolation() {
        // A double-quoted string with no variables is still sub-tokens.
        let tokens = lex_all(r#""plain text""#);
        assert_eq!(tokens, vec![Token::QuoteBegin(QuoteKind::Double, b'"'), Token::ConstSegment("plain text".into()), Token::QuoteEnd,]);
    }

    #[test]
    fn lex_interp_multiple_vars() {
        let tokens = lex_all(r#""$x + $y""#);
        assert_eq!(
            tokens,
            vec![
                Token::QuoteBegin(QuoteKind::Double, b'"'),
                Token::InterpScalar("x".into()),
                Token::ConstSegment(" + ".into()),
                Token::InterpScalar("y".into()),
                Token::QuoteEnd,
            ]
        );
    }

    #[test]
    fn lex_qq_interp() {
        let tokens = lex_all(r#"qq{Hello, $name!}"#);
        assert_eq!(
            tokens,
            vec![
                Token::QuoteBegin(QuoteKind::Double, b'{'),
                Token::ConstSegment("Hello, ".into()),
                Token::InterpScalar("name".into()),
                Token::ConstSegment("!".into()),
                Token::QuoteEnd,
            ]
        );
    }

    #[test]
    fn lex_empty_string() {
        let tokens = lex_all(r#""""#);
        assert_eq!(tokens, vec![Token::QuoteBegin(QuoteKind::Double, b'"'), Token::QuoteEnd,]);
    }

    #[test]
    fn lex_interp_after_string() {
        // Verify . (concat) is lexed as operator after a string.
        let tokens = lex_all(r#""hello" . "world""#);
        assert!(tokens.contains(&Token::Dot));
    }

    // ── Regex / substitution / transliteration tests ──────────

    #[test]
    fn lex_bare_regex() {
        // The lexer always returns Slash for /; the parser
        // interprets it as a regex in term position.
        let tokens = lex_all("/foo/i");
        assert_eq!(tokens, vec![Token::Slash, Token::Ident("foo".into()), Token::Slash, Token::Ident("i".into())]);
    }

    #[test]
    fn lex_bare_regex_no_flags() {
        let tokens = lex_all("/hello world/");
        assert_eq!(tokens, vec![Token::Slash, Token::Ident("hello".into()), Token::Ident("world".into()), Token::Slash]);
    }

    #[test]
    fn lex_m_regex() {
        let tokens = lex_all("m{foo}i");
        assert_eq!(tokens, vec![Token::RegexBegin(RegexKind::Match, b'{'), Token::ConstSegment("foo".into()), Token::QuoteEnd, Token::Ident("i".into()),]);
    }

    #[test]
    fn lex_m_regex_slash() {
        let tokens = lex_all("m/bar/gx");
        assert_eq!(tokens, vec![Token::RegexBegin(RegexKind::Match, b'/'), Token::ConstSegment("bar".into()), Token::QuoteEnd, Token::Ident("gx".into()),]);
    }

    #[test]
    fn lex_qr_regex() {
        let tokens = lex_all("qr/\\d+/");
        assert_eq!(tokens, vec![Token::RegexBegin(RegexKind::Qr, b'/'), Token::ConstSegment("\\d+".into()), Token::QuoteEnd,]);
    }

    #[test]
    fn lex_substitution() {
        // lex_all only tests the pattern side; the full pipeline
        // (replacement + flags) is tested by parser tests.
        let tokens = lex_all("s/foo/bar/g");
        assert_eq!(tokens[0], Token::SubstBegin(b'/'));
        assert_eq!(tokens[1], Token::ConstSegment("foo".into()));
        assert_eq!(tokens[2], Token::QuoteEnd);
    }

    #[test]
    fn lex_substitution_braces() {
        let tokens = lex_all("s{foo}{bar}g");
        assert_eq!(tokens[0], Token::SubstBegin(b'{'));
        assert_eq!(tokens[1], Token::ConstSegment("foo".into()));
        assert_eq!(tokens[2], Token::QuoteEnd);
    }

    #[test]
    fn lex_transliteration() {
        let tokens = lex_all("tr/a-z/A-Z/");
        assert_eq!(tokens, vec![Token::TranslitLit("a-z".into(), "A-Z".into(), None),]);
    }

    #[test]
    fn lex_y_transliteration() {
        let tokens = lex_all("y/abc/def/");
        assert_eq!(tokens, vec![Token::TranslitLit("abc".into(), "def".into(), None),]);
    }

    #[test]
    fn lex_regex_in_expression() {
        // After $x =~ the / is still just Slash from the lexer;
        // the parser handles regex interpretation.
        let tokens = lex_all("$x =~ /foo/");
        assert_eq!(tokens, vec![Token::ScalarVar("x".into()), Token::Binding, Token::Slash, Token::Ident("foo".into()), Token::Slash]);
    }

    #[test]
    fn lex_division_not_regex() {
        // After a variable, / is division.
        let tokens = lex_all("$x / $y");
        assert_eq!(tokens, vec![Token::ScalarVar("x".into()), Token::Slash, Token::ScalarVar("y".into()),]);
    }

    // ── Heredoc tests ─────────────────────────────────────────

    #[test]
    fn lex_heredoc_bare_tag() {
        let src = "<<END;\nHello, world!\nEND\n";
        let tokens = lex_all(src);
        assert_eq!(tokens, vec![Token::QuoteBegin(QuoteKind::Heredoc, 0), Token::ConstSegment("Hello, world!\n".into()), Token::QuoteEnd, Token::Semi,]);
    }

    #[test]
    fn lex_heredoc_double_quoted() {
        let src = "<<\"END\";\nHello!\nEND\n";
        let tokens = lex_all(src);
        assert_eq!(tokens, vec![Token::QuoteBegin(QuoteKind::Heredoc, 0), Token::ConstSegment("Hello!\n".into()), Token::QuoteEnd, Token::Semi,]);
    }

    #[test]
    fn lex_heredoc_single_quoted() {
        let src = "<<'END';\nNo $interpolation here.\nEND\n";
        let tokens = lex_all(src);
        assert_eq!(tokens, vec![Token::HeredocLit(HeredocKind::Literal, "END".into(), "No $interpolation here.\n".into()), Token::Semi,]);
    }

    #[test]
    fn lex_heredoc_multiline_body() {
        let src = "<<END;\nline 1\nline 2\nline 3\nEND\n";
        let tokens = lex_all(src);
        // Heredoc body is returned as a single ConstSegment
        // covering all lines, same as regular strings.
        assert_eq!(
            tokens,
            vec![Token::QuoteBegin(QuoteKind::Heredoc, 0), Token::ConstSegment("line 1\nline 2\nline 3\n".into()), Token::QuoteEnd, Token::Semi,]
        );
    }

    #[test]
    fn lex_heredoc_with_rest_of_line() {
        // The `. " suffix"` should be tokenized from the current line.
        let src = "<<END . \" suffix\";\nbody\nEND\n";
        let tokens = lex_all(src);
        assert_eq!(
            tokens,
            vec![
                Token::QuoteBegin(QuoteKind::Heredoc, 0),
                Token::ConstSegment("body\n".into()),
                Token::QuoteEnd,
                Token::Dot,
                Token::QuoteBegin(QuoteKind::Double, b'"'),
                Token::ConstSegment(" suffix".into()),
                Token::QuoteEnd,
                Token::Semi,
            ]
        );
    }

    #[test]
    fn lex_heredoc_indented() {
        let src = "<<~END;\n    hello\n    world\n    END\n";
        let tokens = lex_all(src);
        assert_eq!(tokens[0], Token::QuoteBegin(QuoteKind::Heredoc, 0));
        // Indent (4 spaces) should be stripped from each line.
        let body: String = tokens.iter().filter_map(|t| if let Token::ConstSegment(s) = t { Some(s.as_str()) } else { None }).collect();
        assert_eq!(body, "hello\nworld\n");
    }

    #[test]
    fn lex_heredoc_then_code() {
        // Code after the heredoc terminator should be lexed normally.
        let src = "my $x = <<END;\nhello\nEND\nmy $y = 1;\n";
        let tokens = lex_all(src);
        // Should contain: my $x = <<END ; my $y = 1 ;
        assert!(tokens.contains(&Token::Keyword(Keyword::My)));
        assert_eq!(tokens.iter().filter(|t| matches!(t, Token::Keyword(Keyword::My))).count(), 2);
    }

    #[test]
    fn lex_heredoc_spliced_inside_q_string() {
        // A q{} string that spans across a heredoc body.
        // The heredoc body is invisible to the q{} scanner.
        // Perl: <<EOF returns "body\n", q{before\nafter\n} returns "before\nafter\n"
        let src = "<<EOF, q{before\nbody\nEOF\nafter\n};\n";
        let tokens = lex_all(src);

        // First tokens: heredoc body
        assert_eq!(tokens[0], Token::QuoteBegin(QuoteKind::Heredoc, 0));
        // Collect heredoc body content.
        let mut i = 1;
        let mut body = String::new();
        while i < tokens.len() && tokens[i] != Token::QuoteEnd {
            if let Token::ConstSegment(s) = &tokens[i] {
                body.push_str(s);
            }
            i += 1;
        }
        assert_eq!(body, "body\n");
        assert_eq!(tokens[i], Token::QuoteEnd);
        i += 1;

        // Then comma
        assert_eq!(tokens[i], Token::Comma);
        i += 1;

        // Then q{} string: "before\nafter\n" — the heredoc body is skipped
        match &tokens[i] {
            Token::StrLit(s) => {
                assert_eq!(s, "before\nafter\n");
            }
            other => panic!("expected StrLit for q{{}}, got {other:?}"),
        }
    }

    // ── Octal literal tests ───────────────────────────────────

    #[test]
    fn lex_legacy_octal() {
        let tokens = lex_all("0777");
        assert_eq!(tokens, vec![Token::IntLit(0o777)]);
    }

    #[test]
    fn lex_zero_alone() {
        let tokens = lex_all("0");
        assert_eq!(tokens, vec![Token::IntLit(0)]);
    }

    #[test]
    #[should_panic(expected = "Illegal octal digit '8'")]
    fn lex_illegal_octal_digit() {
        lex_all("08");
    }

    #[test]
    #[should_panic(expected = "Illegal octal digit '9'")]
    fn lex_illegal_octal_digit_9() {
        lex_all("09");
    }

    #[test]
    #[should_panic(expected = "Illegal octal digit '8'")]
    fn lex_illegal_explicit_octal_digit() {
        lex_all("0o78");
    }

    #[test]
    #[should_panic(expected = "Illegal binary digit '2'")]
    fn lex_illegal_binary_digit() {
        lex_all("0b12");
    }

    #[test]
    fn lex_valid_binary() {
        let tokens = lex_all("0b1010");
        assert_eq!(tokens, vec![Token::IntLit(0b1010)]);
    }

    #[test]
    fn lex_valid_explicit_octal() {
        let tokens = lex_all("0o77");
        assert_eq!(tokens, vec![Token::IntLit(0o77)]);
    }

    // ═══════════════════════════════════════════════════════════
    // NEW TESTS — numeric edge cases
    // ═══════════════════════════════════════════════════════════

    #[test]
    fn lex_hex_underscored() {
        let tokens = lex_all("0xFF_FF");
        assert_eq!(tokens, vec![Token::IntLit(0xFFFF)]);
    }

    #[test]
    fn lex_binary_underscored() {
        let tokens = lex_all("0b1010_0101");
        assert_eq!(tokens, vec![Token::IntLit(0b1010_0101)]);
    }

    #[test]
    fn lex_scientific_neg_exponent() {
        let tokens = lex_all("1.5e-3");
        assert_eq!(tokens.len(), 1);
        assert!(matches!(tokens[0], Token::FloatLit(f) if (f - 1.5e-3).abs() < 1e-15));
    }

    #[test]
    fn lex_scientific_pos_exponent() {
        let tokens = lex_all("2e+5");
        assert_eq!(tokens.len(), 1);
        assert!(matches!(tokens[0], Token::FloatLit(f) if (f - 2e5).abs() < 1.0));
    }

    // ── String / quote edge cases ─────────────────────────────

    #[test]
    fn lex_single_quoted_escape_backslash() {
        let tokens = lex_all("'a\\\\b'");
        assert_eq!(tokens, vec![Token::StrLit("a\\b".into())]);
    }

    #[test]
    fn lex_single_quoted_escape_quote() {
        let tokens = lex_all("'it\\'s'");
        assert_eq!(tokens, vec![Token::StrLit("it's".into())]);
    }

    #[test]
    fn lex_double_quoted_tab_escape() {
        let tokens = lex_all(r#""\t""#);
        assert_eq!(tokens, vec![Token::QuoteBegin(QuoteKind::Double, b'"'), Token::ConstSegment("\t".into()), Token::QuoteEnd]);
    }

    #[test]
    fn lex_q_bang_delimiter() {
        let tokens = lex_all("q!hello!");
        assert_eq!(tokens, vec![Token::StrLit("hello".into())]);
    }

    #[test]
    fn lex_qq_pipe_delimiter() {
        let tokens = lex_all("qq|hello $name|");
        assert_eq!(
            tokens,
            vec![Token::QuoteBegin(QuoteKind::Double, b'|'), Token::ConstSegment("hello ".into()), Token::InterpScalar("name".into()), Token::QuoteEnd,]
        );
    }

    #[test]
    fn lex_qw_braces_delimiter() {
        let tokens = lex_all("qw{foo bar baz}");
        assert_eq!(tokens, vec![Token::QwList(vec!["foo".into(), "bar".into(), "baz".into()])]);
    }

    #[test]
    fn lex_qw_slash_delimiter() {
        let tokens = lex_all("qw/foo bar baz/");
        assert_eq!(tokens, vec![Token::QwList(vec!["foo".into(), "bar".into(), "baz".into()])]);
    }

    #[test]
    fn lex_qw_escaped_delimiter() {
        // \] inside qw[] escapes the delimiter.
        let tokens = lex_all("qw[a\\] b c]");
        assert_eq!(tokens, vec![Token::QwList(vec!["a]".into(), "b".into(), "c".into()])]);
    }

    #[test]
    fn lex_qw_escaped_backslash() {
        // \\ inside qw produces a single backslash (q// escaping).
        let tokens = lex_all("qw(a\\\\b c)");
        assert_eq!(tokens, vec![Token::QwList(vec!["a\\b".into(), "c".into()])]);
    }

    #[test]
    fn lex_qw_literal_backslash_n() {
        // \n inside qw is literal (not a newline).
        let tokens = lex_all("qw(a\\nb c)");
        assert_eq!(tokens, vec![Token::QwList(vec!["a\\nb".into(), "c".into()])]);
    }

    #[test]
    fn lex_backtick_string() {
        let tokens = lex_all("`ls -la`");
        assert_eq!(tokens, vec![Token::QuoteBegin(QuoteKind::Backtick, b'`'), Token::ConstSegment("ls -la".into()), Token::QuoteEnd]);
    }

    #[test]
    fn lex_heredoc_empty_body() {
        let src = "<<END;\nEND\n";
        let tokens = lex_all(src);
        assert_eq!(tokens, vec![Token::QuoteBegin(QuoteKind::Heredoc, 0), Token::QuoteEnd, Token::Semi,]);
    }

    #[test]
    fn lex_heredoc_indented_literal() {
        let src = "<<~'END';\n    hello\n    END\n";
        let tokens = lex_all(src);
        match &tokens[0] {
            Token::HeredocLit(HeredocKind::IndentedLiteral, tag, body) => {
                assert_eq!(tag, "END");
                assert_eq!(body, "hello\n");
            }
            other => panic!("expected IndentedLiteral HeredocLit, got {other:?}"),
        }
    }

    // ── Multiline string handling ────────────────────────────────

    #[test]
    fn lex_single_quoted_multiline_one_segment() {
        // A multiline single-quoted string should produce one StrLit
        // covering all lines, not one per line.
        let tokens = lex_all("'line 1\nline 2\nline 3'");
        assert_eq!(tokens, vec![Token::StrLit("line 1\nline 2\nline 3".into())]);
    }

    #[test]
    fn lex_q_multiline_one_segment() {
        // q// across lines should also produce one StrLit.
        let tokens = lex_all("q/line 1\nline 2\nline 3/");
        assert_eq!(tokens, vec![Token::StrLit("line 1\nline 2\nline 3".into())]);
    }

    #[test]
    fn lex_double_quoted_multiline_one_segment() {
        // A multiline double-quoted string without interpolation
        // should produce one ConstSegment covering all lines.
        let tokens = lex_all("\"line 1\nline 2\nline 3\"");
        assert_eq!(tokens, vec![Token::QuoteBegin(QuoteKind::Double, b'"'), Token::ConstSegment("line 1\nline 2\nline 3".into()), Token::QuoteEnd,]);
    }

    #[test]
    fn lex_double_quoted_multiline_breaks_at_interp() {
        // A multiline double-quoted string breaks ConstSegment at
        // interpolation points, but lines without interpolation
        // are merged into one segment.
        let tokens = lex_all("\"line 1\nline 2\n$x\nline 4\"");
        assert_eq!(
            tokens,
            vec![
                Token::QuoteBegin(QuoteKind::Double, b'"'),
                Token::ConstSegment("line 1\nline 2\n".into()),
                Token::InterpScalar("x".into()),
                Token::ConstSegment("\nline 4".into()),
                Token::QuoteEnd,
            ]
        );
    }

    #[test]
    fn lex_heredoc_multiline_single_segment() {
        // Heredocs return all body lines in one ConstSegment,
        // same as regular strings.  The peek_heredoc mechanism
        // in next_line prevents the one-shot signal from being
        // consumed prematurely.
        let src = "<<END;\nline 1\nline 2\nEND\n";
        let tokens = lex_all(src);
        assert_eq!(tokens, vec![Token::QuoteBegin(QuoteKind::Heredoc, 0), Token::ConstSegment("line 1\nline 2\n".into()), Token::QuoteEnd, Token::Semi,]);
    }

    // ── Assignment operator tokens ────────────────────────────

    #[test]
    fn lex_all_assignment_ops() {
        let tokens = lex_all("$a += $b -= $c *= $d /= $e");
        assert!(tokens.contains(&Token::Assign(AssignOp::AddEq)));
        assert!(tokens.contains(&Token::Assign(AssignOp::SubEq)));
        assert!(tokens.contains(&Token::Assign(AssignOp::MulEq)));
        assert!(tokens.contains(&Token::Assign(AssignOp::DivEq)));
    }

    #[test]
    fn lex_mod_eq() {
        let tokens = lex_all("$x %= 3");
        assert!(tokens.contains(&Token::Assign(AssignOp::ModEq)));
    }

    #[test]
    fn lex_pow_eq() {
        let tokens = lex_all("$x **= 2");
        assert!(tokens.contains(&Token::Assign(AssignOp::PowEq)));
    }

    #[test]
    fn lex_concat_eq() {
        let tokens = lex_all("$x .= 'a'");
        assert!(tokens.contains(&Token::Assign(AssignOp::ConcatEq)));
    }

    #[test]
    fn lex_and_eq() {
        let tokens = lex_all("$x &&= 1");
        assert!(tokens.contains(&Token::Assign(AssignOp::AndEq)));
    }

    #[test]
    fn lex_or_eq() {
        let tokens = lex_all("$x ||= 1");
        assert!(tokens.contains(&Token::Assign(AssignOp::OrEq)));
    }

    #[test]
    fn lex_defined_or_eq() {
        let tokens = lex_all("$x //= 1");
        assert!(tokens.contains(&Token::Assign(AssignOp::DefinedOrEq)));
    }

    #[test]
    fn lex_bit_and_eq() {
        let tokens = lex_all("$x &= 0xFF");
        assert!(tokens.contains(&Token::Assign(AssignOp::BitAndEq)));
    }

    #[test]
    fn lex_bit_or_eq() {
        let tokens = lex_all("$x |= 0xFF");
        assert!(tokens.contains(&Token::Assign(AssignOp::BitOrEq)));
    }

    #[test]
    fn lex_bit_xor_eq() {
        let tokens = lex_all("$x ^= 0xFF");
        assert!(tokens.contains(&Token::Assign(AssignOp::BitXorEq)));
    }

    #[test]
    fn lex_shift_l_eq() {
        let tokens = lex_all("$x <<= 2");
        assert!(tokens.contains(&Token::Assign(AssignOp::ShiftLeftEq)));
    }

    #[test]
    fn lex_shift_r_eq() {
        let tokens = lex_all("$x >>= 2");
        assert!(tokens.contains(&Token::Assign(AssignOp::ShiftRightEq)));
    }

    // ── Operator edge cases ───────────────────────────────────

    #[test]
    fn lex_not_binding() {
        let tokens = lex_all("$x !~ /foo/");
        assert!(tokens.contains(&Token::NotBinding));
    }

    #[test]
    fn lex_dotdot() {
        let tokens = lex_all("1..10");
        assert!(tokens.contains(&Token::DotDot));
    }

    #[test]
    fn lex_dotdotdot_as_yada() {
        let tokens = lex_all("...");
        assert_eq!(tokens, vec![Token::DotDotDot]);
    }

    // ── Variable edge cases ───────────────────────────────────

    #[test]
    fn lex_dollar_slash() {
        let tokens = lex_all("$/");
        assert_eq!(tokens, vec![Token::SpecialVar("/".into())]);
    }

    #[test]
    fn lex_dollar_backslash() {
        let tokens = lex_all("$\\");
        assert_eq!(tokens, vec![Token::SpecialVar("\\".into())]);
    }

    #[test]
    fn lex_dollar_comma() {
        let tokens = lex_all("$,");
        assert_eq!(tokens, vec![Token::SpecialVar(",".into())]);
    }

    // ── perlvar punctuation special variables ─────────────────

    #[test]
    fn lex_dollar_ampersand() {
        let tokens = lex_all("$&");
        assert_eq!(tokens, vec![Token::SpecialVar("&".into())]);
    }

    #[test]
    fn lex_dollar_double_quote() {
        // $" — list separator
        let tokens = lex_all("$\"");
        assert_eq!(tokens, vec![Token::SpecialVar("\"".into())]);
    }

    #[test]
    fn lex_dollar_dot() {
        // $. — line number
        let tokens = lex_all("$.");
        assert_eq!(tokens, vec![Token::SpecialVar(".".into())]);
    }

    #[test]
    fn lex_dollar_pipe() {
        // $| — autoflush
        let tokens = lex_all("$|");
        assert_eq!(tokens, vec![Token::SpecialVar("|".into())]);
    }

    #[test]
    fn lex_dollar_question() {
        // $? — child status
        let tokens = lex_all("$?");
        assert_eq!(tokens, vec![Token::SpecialVar("?".into())]);
    }

    #[test]
    fn lex_dollar_backtick() {
        // $` — prematch
        let tokens = lex_all("$`");
        assert_eq!(tokens, vec![Token::SpecialVar("`".into())]);
    }

    #[test]
    fn lex_dollar_single_quote() {
        // $' — postmatch
        let tokens = lex_all("$'");
        assert_eq!(tokens, vec![Token::SpecialVar("'".into())]);
    }

    #[test]
    fn lex_dollar_open_paren() {
        // $( — real GID
        let tokens = lex_all("$(");
        assert_eq!(tokens, vec![Token::SpecialVar("(".into())]);
    }

    #[test]
    fn lex_dollar_close_paren() {
        // $) — effective GID
        let tokens = lex_all("$)");
        assert_eq!(tokens, vec![Token::SpecialVar(")".into())]);
    }

    #[test]
    fn lex_dollar_less_than() {
        // $< — real UID
        let tokens = lex_all("$<");
        assert_eq!(tokens, vec![Token::SpecialVar("<".into())]);
    }

    #[test]
    fn lex_dollar_greater_than() {
        // $> — effective UID
        let tokens = lex_all("$>");
        assert_eq!(tokens, vec![Token::SpecialVar(">".into())]);
    }

    #[test]
    fn lex_dollar_close_bracket() {
        // $] — Perl version
        let tokens = lex_all("$]");
        assert_eq!(tokens, vec![Token::SpecialVar("]".into())]);
    }

    #[test]
    fn lex_dollar_percent() {
        // $% — page number
        let tokens = lex_all("$%");
        assert_eq!(tokens, vec![Token::SpecialVar("%".into())]);
    }

    #[test]
    fn lex_dollar_colon() {
        // $: — format break chars
        let tokens = lex_all("$:");
        assert_eq!(tokens, vec![Token::SpecialVar(":".into())]);
    }

    #[test]
    fn lex_dollar_equals() {
        // $= — page length
        let tokens = lex_all("$=");
        assert_eq!(tokens, vec![Token::SpecialVar("=".into())]);
    }

    #[test]
    fn lex_dollar_tilde() {
        // $~ — format name
        let tokens = lex_all("$~");
        assert_eq!(tokens, vec![Token::SpecialVar("~".into())]);
    }

    #[test]
    fn lex_array_len() {
        let tokens = lex_all("$#arr");
        assert_eq!(tokens, vec![Token::ArrayLen("arr".into())]);
    }

    #[test]
    fn lex_glob_var() {
        // * lexes as Star; the parser combines Star + Ident into GlobVar
        let tokens = lex_all("*foo");
        assert_eq!(tokens, vec![Token::Star, Token::Ident("foo".into())]);
    }

    #[test]
    fn lex_multi_digit_capture() {
        let tokens = lex_all("$12");
        assert_eq!(tokens, vec![Token::SpecialVar("12".into())]);
    }

    // ── Caret variable tests ──────────────────────────────────

    #[test]
    fn lex_caret_w() {
        let tokens = lex_all("$^W");
        assert_eq!(tokens, vec![Token::SpecialVar("^W".into())]);
    }

    #[test]
    fn lex_caret_o() {
        let tokens = lex_all("$^O");
        assert_eq!(tokens, vec![Token::SpecialVar("^O".into())]);
    }

    #[test]
    fn lex_caret_x() {
        let tokens = lex_all("$^X");
        assert_eq!(tokens, vec![Token::SpecialVar("^X".into())]);
    }

    #[test]
    fn lex_caret_bracket() {
        // $^[ is the $COMPILING variable.
        let tokens = lex_all("$^[");
        assert_eq!(tokens, vec![Token::SpecialVar("^[".into())]);
    }

    #[test]
    fn lex_demarcated_caret_match() {
        let tokens = lex_all("${^MATCH}");
        assert_eq!(tokens, vec![Token::SpecialVar("^MATCH".into())]);
    }

    #[test]
    fn lex_demarcated_caret_postmatch() {
        let tokens = lex_all("${^POSTMATCH}");
        assert_eq!(tokens, vec![Token::SpecialVar("^POSTMATCH".into())]);
    }

    #[test]
    fn lex_demarcated_caret_utf8locale() {
        let tokens = lex_all("${^UTF8LOCALE}");
        assert_eq!(tokens, vec![Token::SpecialVar("^UTF8LOCALE".into())]);
    }

    #[test]
    fn lex_demarcated_caret_warning_bits() {
        let tokens = lex_all("${^WARNING_BITS}");
        assert_eq!(tokens, vec![Token::SpecialVar("^WARNING_BITS".into())]);
    }

    // ── Special array variable tests ──────────────────────────

    #[test]
    fn lex_special_array_plus() {
        let tokens = lex_all("@+");
        assert_eq!(tokens, vec![Token::SpecialArrayVar("+".into())]);
    }

    #[test]
    fn lex_special_array_minus() {
        let tokens = lex_all("@-");
        assert_eq!(tokens, vec![Token::SpecialArrayVar("-".into())]);
    }

    #[test]
    fn lex_special_array_caret_capture() {
        let tokens = lex_all("@{^CAPTURE}");
        assert_eq!(tokens, vec![Token::SpecialArrayVar("^CAPTURE".into())]);
    }

    #[test]
    fn lex_regular_array_not_special() {
        // @foo is a regular array, not special.
        let tokens = lex_all("@foo");
        assert_eq!(tokens, vec![Token::ArrayVar("foo".into())]);
    }

    // ── Special hash variable tests ───────────────────────────

    #[test]
    fn lex_special_hash_bang() {
        let tokens = lex_all("%!");
        assert_eq!(tokens, vec![Token::SpecialHashVar("!".into())]);
    }

    #[test]
    fn lex_special_hash_plus() {
        let tokens = lex_all("%+");
        assert_eq!(tokens, vec![Token::SpecialHashVar("+".into())]);
    }

    #[test]
    fn lex_special_hash_minus() {
        let tokens = lex_all("%-");
        assert_eq!(tokens, vec![Token::SpecialHashVar("-".into())]);
    }

    #[test]
    fn lex_special_hash_caret_capture() {
        let tokens = lex_all("%{^CAPTURE}");
        assert_eq!(tokens, vec![Token::SpecialHashVar("^CAPTURE".into())]);
    }

    #[test]
    fn lex_special_hash_caret_capture_all() {
        let tokens = lex_all("%{^CAPTURE_ALL}");
        assert_eq!(tokens, vec![Token::SpecialHashVar("^CAPTURE_ALL".into())]);
    }

    #[test]
    fn lex_regular_hash_not_special() {
        // %foo is a regular hash, not special.
        let tokens = lex_all("%foo");
        assert_eq!(tokens, vec![Token::HashVar("foo".into())]);
    }

    // ── Regex edge cases ──────────────────────────────────────

    #[test]
    fn lex_regex_many_flags() {
        // Bare / is always Slash from the lexer.
        let tokens = lex_all("/foo/imsxg");
        assert_eq!(tokens, vec![Token::Slash, Token::Ident("foo".into()), Token::Slash, Token::Ident("imsxg".into())]);
    }

    #[test]
    fn lex_regex_code_block() {
        let tokens = lex_all("m/foo(?{ $x })bar/");
        assert_eq!(
            tokens,
            vec![
                Token::RegexBegin(RegexKind::Match, b'/'),
                Token::ConstSegment("foo".into()),
                Token::RegexCodeStart,
                Token::ScalarVar("x".into()),
                Token::RightBrace,
                Token::ConstSegment(")bar".into()),
                Token::QuoteEnd,
            ]
        );
    }

    #[test]
    fn lex_regex_cond_code_block() {
        let tokens = lex_all("m/foo(??{ $x })bar/");
        assert_eq!(
            tokens,
            vec![
                Token::RegexBegin(RegexKind::Match, b'/'),
                Token::ConstSegment("foo".into()),
                Token::RegexCondCodeStart,
                Token::ScalarVar("x".into()),
                Token::RightBrace,
                Token::ConstSegment(")bar".into()),
                Token::QuoteEnd,
            ]
        );
    }

    #[test]
    fn lex_regex_code_block_paired_delim() {
        // The whole point: (?{...}) with paired braces.
        // Without code mode, the } inside the string would
        // close the m{} prematurely.
        let tokens = lex_all("m{foo(?{ 1 })bar}");
        assert_eq!(
            tokens,
            vec![
                Token::RegexBegin(RegexKind::Match, b'{'),
                Token::ConstSegment("foo".into()),
                Token::RegexCodeStart,
                Token::IntLit(1),
                Token::RightBrace,
                Token::ConstSegment(")bar".into()),
                Token::QuoteEnd,
            ]
        );
    }

    #[test]
    fn lex_code_block_not_in_string() {
        // (?{ in a double-quoted string is literal, not a code block.
        let tokens = lex_all(r#""foo(?{bar})""#);
        assert_eq!(tokens, vec![Token::QuoteBegin(QuoteKind::Double, b'"'), Token::ConstSegment("foo(?{bar})".into()), Token::QuoteEnd,]);
    }

    #[test]
    fn lex_code_block_not_in_single_quoted() {
        // (?{ in a single-quoted string is literal.
        let tokens = lex_all("q/foo(?{bar})/");
        assert_eq!(tokens, vec![Token::StrLit("foo(?{bar})".into())]);
    }

    #[test]
    fn lex_cond_code_block_not_in_string() {
        // (??{ in a double-quoted string is literal.
        let tokens = lex_all(r#""foo(??{bar})""#);
        assert_eq!(tokens, vec![Token::QuoteBegin(QuoteKind::Double, b'"'), Token::ConstSegment("foo(??{bar})".into()), Token::QuoteEnd,]);
    }

    // ── Regex interpolation ───────────────────────────────────

    #[test]
    fn lex_regex_scalar_interp() {
        // $var in a regex pattern triggers interpolation.
        let tokens = lex_all("m/foo$bar/");
        assert_eq!(
            tokens,
            vec![Token::RegexBegin(RegexKind::Match, b'/'), Token::ConstSegment("foo".into()), Token::InterpScalar("bar".into()), Token::QuoteEnd,]
        );
    }

    #[test]
    fn lex_regex_array_interp() {
        // @arr in a regex pattern triggers interpolation.
        let tokens = lex_all("m/foo@arr/");
        assert_eq!(
            tokens,
            vec![Token::RegexBegin(RegexKind::Match, b'/'), Token::ConstSegment("foo".into()), Token::InterpArray("arr".into()), Token::QuoteEnd,]
        );
    }

    #[test]
    fn lex_regex_interp_and_code_block() {
        // Both $var and (?{...}) in the same pattern.
        let tokens = lex_all("m/foo$bar(?{ $x })baz/");
        assert_eq!(
            tokens,
            vec![
                Token::RegexBegin(RegexKind::Match, b'/'),
                Token::ConstSegment("foo".into()),
                Token::InterpScalar("bar".into()),
                Token::RegexCodeStart,
                Token::ScalarVar("x".into()),
                Token::RightBrace,
                Token::ConstSegment(")baz".into()),
                Token::QuoteEnd,
            ]
        );
    }

    #[test]
    fn lex_regex_literal_no_interp() {
        // m'...' with single-quote delimiter: no $var interpolation.
        let tokens = lex_all("m'foo$bar'");
        assert_eq!(tokens, vec![Token::RegexBegin(RegexKind::Match, b'\''), Token::ConstSegment("foo$bar".into()), Token::QuoteEnd,]);
    }

    #[test]
    fn lex_regex_literal_with_code_block() {
        // m'...' still recognizes (?{...}) code blocks.
        let tokens = lex_all("m'foo(?{ $x })bar'");
        assert_eq!(
            tokens,
            vec![
                Token::RegexBegin(RegexKind::Match, b'\''),
                Token::ConstSegment("foo".into()),
                Token::RegexCodeStart,
                Token::ScalarVar("x".into()),
                Token::RightBrace,
                Token::ConstSegment(")bar".into()),
                Token::QuoteEnd,
            ]
        );
    }

    #[test]
    fn lex_subst_pattern_interp() {
        // $var in s/// pattern triggers interpolation.
        let tokens = lex_all("s/foo$bar/baz/");
        assert_eq!(tokens[0], Token::SubstBegin(b'/'));
        assert_eq!(tokens[1], Token::ConstSegment("foo".into()));
        assert_eq!(tokens[2], Token::InterpScalar("bar".into()));
        // ConstSegment("") before QuoteEnd for the empty segment
        // after the interpolation.
        assert!(tokens.contains(&Token::QuoteEnd));
    }

    #[test]
    fn lex_substitution_global() {
        let tokens = lex_all("s/old/new/g");
        assert_eq!(tokens[0], Token::SubstBegin(b'/'));
        assert_eq!(tokens[1], Token::ConstSegment("old".into()));
        assert_eq!(tokens[2], Token::QuoteEnd);
    }

    #[test]
    fn lex_transliteration_flags() {
        let tokens = lex_all("tr/a-z/A-Z/cs");
        assert_eq!(tokens, vec![Token::TranslitLit("a-z".into(), "A-Z".into(), Some("cs".into()))]);
    }

    #[test]
    fn lex_regex_after_keyword_term() {
        // Lexer returns Slash; parser interprets as regex after print.
        let tokens = lex_all("print /foo/");
        assert_eq!(tokens, vec![Token::Keyword(Keyword::Print), Token::Slash, Token::Ident("foo".into()), Token::Slash]);
    }

    // ── Filetest tokens ───────────────────────────────────────

    #[test]
    fn lex_filetest_f() {
        let tokens = lex_all("-f $file");
        assert_eq!(tokens, vec![Token::Filetest(b'f'), Token::ScalarVar("file".into())]);
    }

    #[test]
    fn lex_filetest_d() {
        let tokens = lex_all("-d $dir");
        assert_eq!(tokens, vec![Token::Filetest(b'd'), Token::ScalarVar("dir".into())]);
    }

    // ── Readline / glob tokens ────────────────────────────────

    #[test]
    fn lex_readline_stdin() {
        let tokens = lex_all("<STDIN>");
        assert_eq!(tokens, vec![Token::Readline("STDIN".into())]);
    }

    #[test]
    fn lex_readline_diamond() {
        let tokens = lex_all("<>");
        assert_eq!(tokens, vec![Token::Readline("".into())]);
    }

    #[test]
    fn lex_glob_wildcard() {
        let tokens = lex_all("<*.txt>");
        assert_eq!(tokens, vec![Token::Readline("*.txt".into())]);
    }

    // ── DataEnd tokens ────────────────────────────────────────

    #[test]
    fn lex_end_token() {
        let tokens = lex_all("1;\n__END__\nstuff");
        assert!(tokens.contains(&Token::DataEnd(DataEndMarker::End)));
    }

    #[test]
    fn lex_data_token() {
        let tokens = lex_all("1;\n__DATA__\nstuff");
        assert!(tokens.contains(&Token::DataEnd(DataEndMarker::Data)));
    }

    #[test]
    fn lex_ctrl_d_eof() {
        let tokens = lex_all("1;\x04more stuff");
        assert!(tokens.contains(&Token::DataEnd(DataEndMarker::CtrlD)));
    }

    #[test]
    fn lex_ctrl_z_eof() {
        let tokens = lex_all("1;\x1amore stuff");
        assert!(tokens.contains(&Token::DataEnd(DataEndMarker::CtrlZ)));
    }
}
