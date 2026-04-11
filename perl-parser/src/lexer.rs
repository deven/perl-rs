//! Lexer — context-sensitive tokenizer.
//!
//! The lexer and parser are inseparable: the lexer reads `self.expect`
//! (set by the parser) to resolve ambiguities like `/` (regex vs division)
//! and `{` (block vs hash).
//!
//! This module implements the core tokenization loop.  Quote-like sublexing,
//! heredocs, and regex scanning are handled by helper methods.

use bytes::Bytes;

use crate::error::ParseError;
use crate::expect::Expect;
use crate::keyword;
use crate::source::{LexerLine, LexerSource};
use crate::span::Span;
use crate::token::*;

/// Sublexing context — tracks what mode the lexer is in.
#[derive(Clone, Debug)]
enum LexContext {
    /// Inside an interpolating string ("...", qq//, `...`, heredoc body).
    /// `close` is `None` for heredocs (end signaled by LexerSource).
    Interpolating {
        close: Option<u8>,
        /// For paired delimiters like qq{...}, the open delimiter
        /// for nesting depth tracking.  None for non-paired (qq//).
        open: Option<u8>,
        depth: u32,
    },
    /// Inside `${expr}` or `@{expr}` — normal code lexing, but
    /// when `}` is reached at depth 0, pop back to Interpolating.
    ExprInString { depth: u32 },
}

/// Saved lexer state for checkpoint/restore (used by the parser's
/// re-lex mechanism to undo a speculatively-lexed token).
#[derive(Clone, Debug)]
pub(crate) struct LexerCheckpoint {
    pub line: Option<LexerLine>,
    pub context_depth: usize,
    pub source_cursor: usize,
    pub source_line_number: usize,
    pub heredoc_depth: usize,
}

/// Lexer state, embedded in the `Parser` struct (not standalone).
///
/// The lexer operates on lines delivered by `LexerSource`.  It reads
/// the `expect` field to resolve context-sensitive ambiguities.
/// The context stack tracks sublexing modes (interpolating strings,
/// regex patterns, heredocs).
///
/// CRLF normalization is handled by `LexerSource` at the line level.
pub(crate) struct Lexer {
    source: LexerSource,
    current_line: Option<LexerLine>,
    context_stack: Vec<LexContext>,
}

impl Lexer {
    pub fn new(src: &[u8]) -> Self {
        Lexer { source: LexerSource::new(src), current_line: None, context_stack: Vec::new() }
    }

    /// Global byte position in the original source.
    pub fn pos(&self) -> usize {
        match &self.current_line {
            Some(line) => line.offset + line.pos,
            None => self.source.cursor(),
        }
    }

    /// Save a checkpoint for the parser's re-lex mechanism.
    pub fn checkpoint(&self) -> LexerCheckpoint {
        LexerCheckpoint {
            line: self.current_line.clone(),
            context_depth: self.context_stack.len(),
            source_cursor: self.source.cursor(),
            source_line_number: self.source.line_number(),
            heredoc_depth: self.source.heredoc_depth(),
        }
    }

    /// Restore to a saved checkpoint, undoing any state changes
    /// (context pushes, line transitions, heredoc starts) since the checkpoint.
    pub fn restore(&mut self, cp: LexerCheckpoint) {
        self.current_line = cp.line;
        self.context_stack.truncate(cp.context_depth);
        self.source.set_cursor(cp.source_cursor, cp.source_line_number, cp.heredoc_depth);
    }

    // ── Line management ───────────────────────────────────────

    /// Ensure a line is loaded.  Returns true if a line is available,
    /// false if EOF or heredoc finished (current_line stays None).
    fn ensure_line(&mut self) -> Result<bool, ParseError> {
        if let Some(line) = &self.current_line {
            if line.pos < line.line.len() {
                return Ok(true); // has content
            }
            // Exhausted — drop it.
            self.current_line = None;
        }
        match self.source.next_line()? {
            Some(line) => {
                self.current_line = Some(line);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    // ── Character access ──────────────────────────────────────

    /// Peek at the current byte without advancing.
    /// Returns `None` at end of line or when no line is loaded.
    fn peek_byte(&self) -> Option<u8> {
        self.current_line.as_ref()?.peek_byte()
    }

    /// Peek at a byte at an offset from the current position.
    /// Only valid within the current line content.
    fn peek_byte_at(&self, offset: usize) -> Option<u8> {
        self.current_line.as_ref()?.peek_byte_at(offset)
    }

    /// Consume the current byte and advance.
    /// Returns `None` at end of line content (line is NOT dropped;
    /// caller can check `line.terminated` before dropping).
    fn advance_byte(&mut self) -> Option<u8> {
        self.current_line.as_mut()?.advance_byte()
    }

    /// Advance to the next content byte, crossing line boundaries.
    /// When the current line is exhausted and terminated, pushes
    /// `'\n'` to `content`, drops the line, and fetches the next.
    /// Returns `None` only at true EOF (unterminated line or source
    /// exhausted).
    fn advance_byte_in_string(&mut self, content: &mut String) -> Result<Option<u8>, ParseError> {
        loop {
            if let Some(b) = self.advance_byte() {
                return Ok(Some(b));
            }
            // End of line content.  If terminated, the line boundary
            // is a newline in the source — add it and fetch next line.
            if self.current_line.as_ref().is_some_and(|l| l.terminated) {
                content.push('\n');
                self.current_line = None;
                if !self.ensure_line()? {
                    return Ok(None); // EOF after newline
                }
            } else {
                return Ok(None); // unterminated — true EOF
            }
        }
    }

    /// Remaining bytes in the current line (not including synthetic \n).
    pub fn remaining(&self) -> &[u8] {
        match &self.current_line {
            Some(line) => line.remaining(),
            None => &[],
        }
    }

    /// Whether the source is fully exhausted (no current line, no
    /// more lines in source, not inside a heredoc).
    fn at_end(&self) -> bool {
        self.current_line.is_none() && self.source.cursor() >= self.source.src_len()
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
        while let Ok(Some(_)) = self.source.next_line() {}
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

    /// Byte-level lookahead to determine if content after `{` looks like
    /// an anonymous hash rather than a block.  Faithfully reproduces the
    /// heuristic from toke.c `yyl_leftcurly()` default case (lines 6400–6471).
    ///
    /// Called when the lexer position is right after `{`.
    /// Does NOT advance the lexer — purely read-only scan on source bytes.
    pub fn looks_like_hash_content(&self) -> bool {
        // Use raw source from current position — the heuristic needs to
        // see past comments and line boundaries to determine block vs hash.
        let pos = self.pos();
        let len = self.source.src_len();
        if pos >= len {
            return true;
        }
        let src = self.source.src_slice(pos, len);

        // Skip whitespace and comments (toke.c: s = skipspace(s))
        let i = skip_ws_bytes(src, 0);

        // Empty {} → hash (toke.c line 6368)
        if i >= src.len() || src[i] == b'}' {
            return true;
        }

        let s = i; // start of first term (toke.c's `s`)
        let mut t = i; // scanner position  (toke.c's `t`)

        // ── Scan past the first term ────────────────────────────
        // toke.c lines 6401–6463

        if src[s] == b'\'' || src[s] == b'"' || src[s] == b'`' {
            // String literal — scan to matching quote, handling escapes.
            // toke.c lines 6401–6406
            let quote = src[s];
            t += 1;
            while t < src.len() && src[t] != quote {
                if src[t] == b'\\' {
                    t += 1;
                }
                t += 1;
            }
            if t < src.len() {
                t += 1;
            } // past closing quote
        } else if src[s] == b'q' {
            // q//, qq//, qx// — or a plain word starting with 'q'.
            // toke.c lines 6408–6455.
            //
            // The C code uses `++t` with side effects inside boolean
            // short-circuit evaluation.  We replicate the same
            // advancement sequence explicitly.
            t += 1; // past 'q'

            let is_q_quote = if t < src.len() {
                if !is_word_char(src[t]) {
                    // Non-word char right after 'q' → q// (e.g. q/, q{)
                    true
                } else if src[t] == b'q' || src[t] == b'x' {
                    // Could be qq// or qx// — advance past second char
                    t += 1;
                    t < src.len() && !is_word_char(src[t])
                } else {
                    false
                }
            } else {
                false
            };

            if is_q_quote {
                // Skip whitespace before delimiter (toke.c line 6419)
                while t < src.len() && is_space(src[t]) {
                    t += 1;
                }

                // Check for `q =>` — bare 'q' as hash key (toke.c line 6422)
                if t + 1 < src.len() && src[t] == b'=' && src[t + 1] == b'>' {
                    return true;
                }

                // Scan past the q-quote's delimiters (toke.c lines 6425–6447)
                if t < src.len() {
                    let open = src[t];
                    let close = match open {
                        b'(' => b')',
                        b'[' => b']',
                        b'{' => b'}',
                        b'<' => b'>',
                        _ => open,
                    };
                    if open == close {
                        // Same-char delimiter (q/.../)
                        t += 1;
                        while t < src.len() {
                            if src[t] == b'\\' && t + 1 < src.len() && open != b'\\' {
                                t += 1;
                            } else if src[t] == open {
                                break;
                            }
                            t += 1;
                        }
                    } else {
                        // Paired delimiters (q{...}, q<...>)
                        let mut brackets: i32 = 1;
                        t += 1;
                        while t < src.len() {
                            if src[t] == b'\\' && t + 1 < src.len() {
                                t += 1;
                            } else if src[t] == close {
                                brackets -= 1;
                                if brackets <= 0 {
                                    break;
                                }
                            } else if src[t] == open {
                                brackets += 1;
                            }
                            t += 1;
                        }
                    }
                    if t < src.len() {
                        t += 1;
                    } // past closing delimiter
                }
            } else {
                // Plain word starting with 'q' (e.g. "query", "qw", "qr").
                // t was already advanced past 'q' (and possibly 'qq'/'qx'
                // whose second char turned out to be a word char); continue
                // scanning the rest of the identifier.
                // toke.c lines 6449–6455
                while t < src.len() && is_word_char(src[t]) {
                    t += 1;
                }
            }
        } else if is_word_char(src[s]) {
            // Bareword — scan past it.
            // toke.c lines 6457–6463
            t += 1;
            while t < src.len() && is_word_char(src[t]) {
                t += 1;
            }
        }

        // Skip whitespace after first term (toke.c line 6465)
        t = skip_ws_bytes(src, t);

        // ── Key decision ────────────────────────────────────────
        // "if comma follows first term, call it an anon hash"
        // toke.c lines 6467–6471
        if t < src.len() {
            // => after first term → definitely hash
            if src[t] == b'=' && t + 1 < src.len() && src[t + 1] == b'>' {
                return true;
            }
            // , after first term → hash if first char is 'q' or non-lowercase
            // (lowercase bareword + comma could be a function call in a block)
            if src[t] == b',' && (src[s] == b'q' || !src[s].is_ascii_lowercase()) {
                return true;
            }
        }

        // Default: block
        false
    }

    /// Skip a format body: everything until a line containing just `.`
    /// (optionally followed by whitespace).
    pub fn skip_format_body(&mut self) {
        // Skip the rest of the current line (after `=`).
        self.current_line = None;
        loop {
            match self.source.next_line() {
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
            if !self.ensure_line()? {
                break; // EOF
            }
            // Skip spaces and tabs within the line.
            while self.peek_byte().is_some_and(|b| b == b' ' || b == b'\t') {
                self.skip(1);
            }
            // End of line content — ensure_line will drop and fetch next.
            if self.peek_byte().is_none() {
                continue;
            }
            // Comment — rest of line is comment, drop it.
            if self.peek_byte() == Some(b'#') {
                self.current_line = None;
                continue;
            }
            // Skip pod: =word ... =cut at start of line
            if self.peek_byte() == Some(b'=') && self.peek_byte_at(1).is_some_and(|b| b.is_ascii_alphabetic()) && self.line_pos() == 0 {
                self.skip_pod()?;
                continue;
            }
            break;
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
            if !self.ensure_line()? {
                break; // EOF inside pod — not an error per Perl
            }
            let is_cut = {
                if let Some(line) = &self.current_line {
                    line.line.starts_with(b"=cut") && !line.line.get(4).is_some_and(|b| b.is_ascii_alphabetic())
                } else {
                    false
                }
            };
            self.current_line = None; // skip this line
            if is_cut {
                break;
            }
        }
        Ok(())
    }

    // ── Main tokenization entry point ─────────────────────────

    /// Lex the next token.  Uses `expect` to resolve ambiguities.
    /// When inside a sublexing context (interpolating string, etc.),
    /// dispatches to the appropriate sub-lexer instead.
    pub fn next_token(&mut self, expect: &Expect) -> Result<Spanned, ParseError> {
        // If inside a sublexing context, dispatch there.
        match self.context_stack.last() {
            Some(LexContext::Interpolating { close, open, depth }) => {
                let (close, open, depth) = (*close, *open, *depth);
                return self.lex_interp_token(close, open, depth);
            }
            Some(LexContext::ExprInString { .. }) => {
                // Normal code lexing inside ${expr} or @{expr}.
                let result = self.lex_normal_token(expect)?;
                // Track brace depth to find the closing }.
                match &result.token {
                    Token::LeftBrace | Token::HashBrace => {
                        if let Some(LexContext::ExprInString { depth: d }) = self.context_stack.last_mut() {
                            *d += 1;
                        }
                    }
                    Token::RightBrace => {
                        if let Some(LexContext::ExprInString { depth: d }) = self.context_stack.last_mut() {
                            if *d == 0 {
                                self.context_stack.pop();
                            } else {
                                *d -= 1;
                            }
                        }
                    }
                    _ => {}
                }
                return Ok(result);
            }
            None => {}
        }

        self.lex_normal_token(expect)
    }

    /// Lex a token in normal (code) mode.
    fn lex_normal_token(&mut self, expect: &Expect) -> Result<Spanned, ParseError> {
        self.skip_ws_and_comments()?;

        // Ensure a line is available after skipping whitespace.
        if !self.ensure_line()? {
            let start = self.span_pos();
            return Ok(Spanned { token: Token::Eof, span: Span::new(start, start) });
        }

        let start = self.span_pos();

        let b = match self.peek_byte() {
            Some(b) => b,
            None => {
                let start = self.span_pos();
                return Ok(Spanned { token: Token::Eof, span: Span::new(start, start) });
            }
        };

        let token = match b {
            // ── Digits → numeric literal ──────────────────────
            b'0'..=b'9' => self.lex_number()?,

            // ── Sigils → variables ────────────────────────────
            b'$' => self.lex_dollar(expect)?,
            b'@' => self.lex_at()?,
            b'%' => self.lex_percent(expect)?,

            // ── Identifiers and keywords ──────────────────────
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.lex_word(expect)?,

            // ── Strings ───────────────────────────────────────
            b'\'' => self.lex_single_quoted_string()?,
            b'"' => {
                self.skip(1); // skip opening "
                self.context_stack.push(LexContext::Interpolating { close: Some(b'"'), open: None, depth: 0 });
                Token::QuoteBegin(QuoteKind::Double, b'"')
            }
            b'`' => {
                self.skip(1); // skip opening `
                self.context_stack.push(LexContext::Interpolating { close: Some(b'`'), open: None, depth: 0 });
                Token::QuoteBegin(QuoteKind::Backtick, b'`')
            }

            // ── Operators and punctuation ─────────────────────
            b'+' => self.lex_plus(),
            b'-' => self.lex_minus(expect)?,
            b'*' => self.lex_star(),
            b'/' => self.lex_slash(expect)?,
            b'.' => self.lex_dot(),
            b'<' => self.lex_less_than(expect)?,
            b'>' => self.lex_greater_than(),
            b'=' => self.lex_equals(),
            b'!' => self.lex_bang(),
            b'&' => self.lex_ampersand(),
            b'|' => self.lex_pipe(),
            b'^' => {
                self.skip(1);
                if self.peek_byte() == Some(b'=') {
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
                if *expect == Expect::Prototype {
                    // Prototype scanning: read raw bytes until matching ).
                    // Matches toke.c's scan_str() call in yyl_sub().
                    let content = self.scan_balanced_string(b'(', b')')?;
                    Token::Prototype(content)
                } else {
                    Token::LeftParen
                }
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
                // Brace disambiguation matching toke.c yyl_leftcurly().
                match expect {
                    // XTERM → always hash (toke.c lines 6313–6317).
                    Expect::Term => Token::HashBrace,
                    // XSTATE → heuristic (toke.c lines 6360–6501).
                    Expect::Statement => {
                        if self.looks_like_hash_content() {
                            Token::HashBrace
                        } else {
                            Token::LeftBrace
                        }
                    }
                    // Everything else → block brace.
                    _ => Token::LeftBrace,
                }
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
        if self.peek_byte() == Some(b'0') {
            match self.peek_byte_at(1) {
                Some(b'x') | Some(b'X') => return self.lex_hex(),
                Some(b'b') | Some(b'B') => return self.lex_binary(),
                Some(b'o') | Some(b'O') => return self.lex_octal_explicit(),
                _ => {}
            }
        }

        // Decimal integer or float
        self.scan_digits();

        if self.peek_byte() == Some(b'.') && self.peek_byte_at(1).is_some_and(|b| b.is_ascii_digit()) {
            // Float
            self.skip(1); // skip '.'
            self.scan_digits();
            self.scan_exponent();
            let s = self.line_slice_str(start)?;
            let s = s.replace('_', "");
            let n: f64 = s.parse().map_err(|_| ParseError::new("invalid float literal", self.span_from(start)))?;
            Ok(Token::FloatLit(n))
        } else if self.peek_byte() == Some(b'e') || self.peek_byte() == Some(b'E') {
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
        while let Some(b) = self.peek_byte() {
            if b.is_ascii_digit() || b == b'_' {
                self.skip(1);
            } else {
                break;
            }
        }
    }

    fn scan_exponent(&mut self) {
        if self.peek_byte() == Some(b'e') || self.peek_byte() == Some(b'E') {
            self.skip(1);
            if self.peek_byte() == Some(b'+') || self.peek_byte() == Some(b'-') {
                self.skip(1);
            }
            self.scan_digits();
        }
    }

    fn lex_hex(&mut self) -> Result<Token, ParseError> {
        let start = self.line_pos();
        self.skip(2); // skip 0x
        let hex_start = self.line_pos();
        while let Some(b) = self.peek_byte() {
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
        while let Some(b) = self.peek_byte() {
            if b == b'0' || b == b'1' || b == b'_' {
                self.skip(1);
            } else {
                break;
            }
        }
        // Check for illegal binary digits (2-9)
        if let Some(b) = self.peek_byte() {
            if b.is_ascii_digit() {
                return Err(ParseError::new(format!("Illegal binary digit '{}'", b as char), self.span_from(start)));
            }
        }
        let s = self.line_slice_str(bin_start)?.replace('_', "");
        let n = i64::from_str_radix(&s, 2).map_err(|_| ParseError::new("invalid binary literal", self.span_from(start)))?;
        Ok(Token::IntLit(n))
    }

    fn lex_octal_explicit(&mut self) -> Result<Token, ParseError> {
        let start = self.line_pos();
        self.skip(2); // skip 0o
        let oct_start = self.line_pos();
        while let Some(b) = self.peek_byte() {
            if (b'0'..=b'7').contains(&b) || b == b'_' {
                self.skip(1);
            } else {
                break;
            }
        }
        // Check for illegal octal digits (8, 9)
        if let Some(b) = self.peek_byte() {
            if b == b'8' || b == b'9' {
                return Err(ParseError::new(format!("Illegal octal digit '{}'", b as char), self.span_from(start)));
            }
        }
        let s = self.line_slice_str(oct_start)?.replace('_', "");
        let n = i64::from_str_radix(&s, 8).map_err(|_| ParseError::new("invalid octal literal", self.span_from(start)))?;
        Ok(Token::IntLit(n))
    }

    // ── Variables ($, @, %) ───────────────────────────────────

    fn lex_dollar(&mut self, _expect: &Expect) -> Result<Token, ParseError> {
        self.skip(1); // skip $

        // $# — array length
        if self.peek_byte() == Some(b'#') {
            if self.peek_byte_at(1).is_some_and(|b| b == b'_' || b.is_ascii_alphabetic()) {
                self.skip(1); // skip #
                let name = self.scan_ident();
                return Ok(Token::ArrayLen(name));
            }
        }

        // Special variables: $$, $!, $@, $_, $0-$9, $/, $\, etc.
        match self.peek_byte() {
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
                    while let Some(b) = self.peek_byte() {
                        if b.is_ascii_alphanumeric() || b == b'_' {
                            self.skip(1);
                        } else {
                            break;
                        }
                    }
                    let ident = self.line_slice_str(ident_start)?;
                    let name = format!("^{ident}");
                    if self.peek_byte() == Some(b'}') {
                        self.skip(1);
                    }
                    return Ok(Token::SpecialVar(name));
                }
                // ${name} — variable with brace disambiguation
                // ${$ref} or ${expr} — dereference block (return Dollar, let parser handle {})
                if self.peek_byte_at(1).is_some_and(|b| b == b'_' || b.is_ascii_alphabetic()) {
                    self.skip(1); // skip {
                    let name = self.scan_ident();
                    if self.peek_byte() == Some(b'}') {
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
                if let Some(b) = self.peek_byte_at(1) {
                    if b == b'_'
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
                        || b == b'~'
                    {
                        return Ok(Token::Dollar);
                    }
                }
                self.skip(1);
                return Ok(Token::SpecialVar("$".into()));
            }
            Some(b'^') => {
                // $^X — caret variable (single character after ^)
                if let Some(next) = self.peek_byte_at(1) {
                    if next.is_ascii_alphabetic() || next == b'[' || next == b']' {
                        self.skip(2); // skip ^ and the character
                        let name = format!("^{}", next as char);
                        return Ok(Token::SpecialVar(name));
                    }
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
                while self.peek_byte().is_some_and(|b| b.is_ascii_digit()) {
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
        match self.peek_byte() {
            Some(b'{') if self.peek_byte_at(1) == Some(b'^') => {
                // @{^CAPTURE} etc.
                self.skip(2); // skip { and ^
                let ident_start = self.line_pos();
                while let Some(b) = self.peek_byte() {
                    if b.is_ascii_alphanumeric() || b == b'_' {
                        self.skip(1);
                    } else {
                        break;
                    }
                }
                let ident = self.line_slice_str(ident_start)?;
                let name = format!("^{ident}");
                if self.peek_byte() == Some(b'}') {
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

    fn lex_percent(&mut self, expect: &Expect) -> Result<Token, ParseError> {
        // % can be modulo or hash sigil
        if expect.expecting_term() {
            self.skip(1);
            match self.peek_byte() {
                Some(b'{') if self.peek_byte_at(1) == Some(b'^') => {
                    // %{^CAPTURE} etc.
                    self.skip(2); // skip { and ^
                    let ident_start = self.line_pos();
                    while let Some(b) = self.peek_byte() {
                        if b.is_ascii_alphanumeric() || b == b'_' {
                            self.skip(1);
                        } else {
                            break;
                        }
                    }
                    let ident = self.line_slice_str(ident_start)?;
                    let name = format!("^{ident}");
                    if self.peek_byte() == Some(b'}') {
                        self.skip(1);
                    }
                    Ok(Token::SpecialHashVar(name))
                }
                Some(b'!') => {
                    self.skip(1);
                    Ok(Token::SpecialHashVar("!".into()))
                }
                Some(b'+') => {
                    self.skip(1);
                    Ok(Token::SpecialHashVar("+".into()))
                }
                Some(b'-') => {
                    self.skip(1);
                    Ok(Token::SpecialHashVar("-".into()))
                }
                Some(b) if b == b'_' || b.is_ascii_alphabetic() => {
                    let name = self.scan_ident();
                    Ok(Token::HashVar(name))
                }
                _ => Ok(Token::Percent),
            }
        } else {
            self.skip(1);
            if self.peek_byte() == Some(b'=') {
                self.skip(1);
                Ok(Token::Assign(AssignOp::ModEq))
            } else {
                Ok(Token::Percent)
            }
        }
    }

    // ── Identifiers ───────────────────────────────────────────

    fn scan_ident(&mut self) -> String {
        let start = self.line_pos();
        while let Some(b) = self.peek_byte() {
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

    fn lex_word(&mut self, expect: &Expect) -> Result<Token, ParseError> {
        let name = self.scan_ident();

        // After -> (Ref position), all words are identifiers — no keyword
        // lookup.  `$obj->method`, `$obj->keys`, `$obj->print` are all
        // method calls, not keywords.
        if *expect == Expect::Deref {
            return Ok(Token::Ident(name));
        }

        // Check for `=>` after bareword (fat comma autoquotes)
        // We don't consume the `=>` — just recognize the word as a string
        // when `=>` follows.  Actually the parser should handle this.

        // Check for string comparison keyword operators
        // These are infix operators when in operator position.
        if !expect.expecting_term() {
            match name.as_str() {
                "eq" => return Ok(Token::StrEq),
                "ne" => return Ok(Token::StrNe),
                "lt" => return Ok(Token::StrLt),
                "gt" => return Ok(Token::StrGt),
                "le" => return Ok(Token::StrLe),
                "ge" => return Ok(Token::StrGe),
                "cmp" => return Ok(Token::StrCmp),
                "x" => return Ok(Token::X),
                "and" => return Ok(Token::Keyword(Keyword::And)),
                "or" => return Ok(Token::Keyword(Keyword::Or)),
                "not" => return Ok(Token::Not),
                _ => {}
            }
        }

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
            while self.peek_byte() == Some(b'.') {
                // Check that a digit follows the dot
                if self.peek_byte_at(1).is_some_and(|b| b.is_ascii_digit()) {
                    vstr.push('.');
                    self.skip(1); // skip '.'
                    let start = self.line_pos();
                    while self.peek_byte().is_some_and(|b| b.is_ascii_digit()) {
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
        match self.peek_byte() {
            Some(b) => !b.is_ascii_alphanumeric() && b != b'_',
            None => false,
        }
    }

    // ── Strings ───────────────────────────────────────────────

    fn lex_single_quoted_string(&mut self) -> Result<Token, ParseError> {
        self.skip(1); // skip opening '
        let mut s = String::new();
        loop {
            match self.advance_byte_in_string(&mut s)? {
                None => return Err(ParseError::new("unterminated string", Span::new(self.span_pos(), self.span_pos()))),
                Some(b'\\') => match self.peek_byte() {
                    Some(b'\\') => {
                        self.skip(1);
                        s.push('\\');
                    }
                    Some(b'\'') => {
                        self.skip(1);
                        s.push('\'');
                    }
                    _ => s.push('\\'),
                },
                Some(b'\'') => break,
                Some(b) => s.push(b as char),
            }
        }
        Ok(Token::StrLit(s))
    }

    // ── Interpolating string sublexer (§5.4) ────────────────────

    /// Lex one sub-token from inside an interpolating string.
    /// Called when the context stack top is `Interpolating`.
    /// `close` is `None` for heredocs (end signaled by LexerSource).
    fn lex_interp_token(&mut self, close: Option<u8>, open: Option<u8>, depth: u32) -> Result<Spanned, ParseError> {
        let start = self.span_pos();

        // For heredocs, ensure we have a line loaded.
        if close.is_none() {
            if !self.ensure_line()? {
                // LexerSource returned None — heredoc finished.
                self.context_stack.pop();
                return Ok(Spanned { token: Token::QuoteEnd, span: Span::new(start, start) });
            }
        }

        let b = match self.peek_byte() {
            Some(b) => b,
            None if close.is_none() => {
                // Empty heredoc line — return newline as ConstSegment.
                if self.current_line.as_ref().is_some_and(|l| l.terminated) {
                    self.current_line = None;
                    return Ok(Spanned { token: Token::ConstSegment("\n".into()), span: Span::new(start, self.span_pos()) });
                }
                return Err(ParseError::new("unterminated string", Span::new(start, start)));
            }
            None => return Err(ParseError::new("unterminated string", Span::new(start, start))),
        };

        // Check for closing delimiter (not for heredocs — close is None).
        if let Some(c) = close {
            if b == c && depth == 0 {
                self.skip(1);
                self.context_stack.pop();
                return Ok(Spanned { token: Token::QuoteEnd, span: Span::new(start, self.span_pos()) });
            }
        }

        // Check for interpolation.
        if b == b'$' {
            return self.lex_interp_scalar(start);
        }
        if b == b'@' {
            return self.lex_interp_array(start);
        }

        // Otherwise, scan a ConstSegment: everything until we hit
        // $, @, the closing delimiter, or end of line.
        let mut s = String::new();
        let mut current_depth = depth;

        loop {
            match self.peek_byte() {
                None => {
                    // End of line content.
                    let terminated = self.current_line.as_ref().is_some_and(|l| l.terminated);
                    if close.is_none() {
                        // Heredoc: push newline, drop line, break.
                        // Next call to lex_interp_token will ensure_line,
                        // which either gets the next body line or detects
                        // heredoc end (QuoteEnd).
                        if terminated {
                            s.push('\n');
                        }
                        self.current_line = None;
                        break;
                    } else {
                        // Regular string: cross line boundary.
                        if terminated {
                            s.push('\n');
                            self.current_line = None;
                            if !self.ensure_line()? {
                                break; // EOF inside string
                            }
                            continue;
                        }
                        break; // unterminated line — EOF
                    }
                }
                Some(b) if Some(b) == close && current_depth == 0 => break,
                Some(b'$') | Some(b'@') => break,
                Some(b'\\') => {
                    self.skip(1);
                    self.process_escape(&mut s, close);
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

        // Update depth in context stack.
        if let Some(LexContext::Interpolating { depth: d, .. }) = self.context_stack.last_mut() {
            *d = current_depth;
        }

        Ok(Spanned { token: Token::ConstSegment(s), span: Span::new(start, self.span_pos()) })
    }

    /// Process a backslash escape inside a double-quoted string.
    /// The backslash has already been consumed.
    fn process_escape(&mut self, s: &mut String, close: Option<u8>) {
        match self.peek_byte() {
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
                if self.peek_byte() == Some(b'{') {
                    // \x{HH...} — Unicode escape
                    self.skip(1);
                    let mut n = 0u32;
                    while let Some(b) = self.peek_byte() {
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
                        if let Some(b) = self.peek_byte() {
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
        if self.peek_byte() == Some(b'{') {
            self.skip(1); // skip {
            // Simple identifier: ${name}
            if self.peek_byte().is_some_and(|b| b == b'_' || b.is_ascii_alphabetic()) {
                let saved_pos = self.line_pos();
                let name = self.scan_ident();
                if self.peek_byte() == Some(b'}') {
                    self.skip(1);
                    return Ok(Spanned { token: Token::InterpScalar(name), span: Span::new(start, self.span_pos()) });
                }
                // Not a simple ${name} — backtrack and scan as expression
                if let Some(line) = self.current_line.as_mut() {
                    line.pos = saved_pos;
                }
            }
            // Expression interpolation: ${\ expr}, ${$ref}, etc.
            // Push ExprInString — next tokens are normal code until }.
            self.context_stack.push(LexContext::ExprInString { depth: 0 });
            return Ok(Spanned { token: Token::InterpScalarExprStart, span: Span::new(start, self.span_pos()) });
        }

        // $name form — must start with alpha or _
        if self.peek_byte().is_some_and(|b| b == b'_' || b.is_ascii_alphabetic()) {
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
        if self.peek_byte() == Some(b'{') {
            self.skip(1); // skip {
            self.context_stack.push(LexContext::ExprInString { depth: 0 });
            return Ok(Spanned { token: Token::InterpArrayExprStart, span: Span::new(start, self.span_pos()) });
        }

        // @name form
        if self.peek_byte().is_some_and(|b| b == b'_' || b.is_ascii_alphabetic()) {
            let name = self.scan_ident();
            return Ok(Spanned { token: Token::InterpArray(name), span: Span::new(start, self.span_pos()) });
        }

        // Bare @ not followed by a name — treat as literal
        Ok(Spanned { token: Token::ConstSegment("@".into()), span: Span::new(start, self.span_pos()) })
    }

    fn scan_to_delimiter(&mut self, delim: u8) -> Result<String, ParseError> {
        let mut s = String::new();
        loop {
            match self.advance_byte_in_string(&mut s)? {
                None => return Err(ParseError::new("unterminated string", Span::new(self.span_pos(), self.span_pos()))),
                Some(b'\\') if self.peek_byte() == Some(delim) => {
                    self.skip(1);
                    s.push(delim as char);
                }
                Some(b) if b == delim => break,
                Some(b) => s.push(b as char),
            }
        }
        Ok(s)
    }

    // ── q// qq// qw// ─────────────────────────────────────────

    fn lex_q_string(&mut self) -> Result<Token, ParseError> {
        let (open, close) = self.read_quote_delimiters()?;
        let s = self.scan_balanced_string(open, close)?;
        Ok(Token::StrLit(s))
    }

    fn lex_qq_string(&mut self) -> Result<Token, ParseError> {
        let (open, close) = self.read_quote_delimiters()?;
        let paired_open = if open != close { Some(open) } else { None };
        self.context_stack.push(LexContext::Interpolating { close: Some(close), open: paired_open, depth: 0 });
        Ok(Token::QuoteBegin(QuoteKind::Double, open))
    }

    fn lex_qx(&mut self) -> Result<Token, ParseError> {
        let (open, close) = self.read_quote_delimiters()?;
        let paired_open = if open != close { Some(open) } else { None };
        self.context_stack.push(LexContext::Interpolating { close: Some(close), open: paired_open, depth: 0 });
        Ok(Token::QuoteBegin(QuoteKind::Backtick, open))
    }

    fn lex_qw(&mut self) -> Result<Token, ParseError> {
        let (open, close) = self.read_quote_delimiters()?;
        let body = self.scan_balanced_string(open, close)?;
        let words: Vec<String> = body.split_whitespace().map(String::from).collect();
        Ok(Token::QwList(words))
    }

    // ── Regex and friends ─────────────────────────────────────

    /// `m/pattern/flags` or `m{pattern}flags`
    fn lex_m(&mut self) -> Result<Token, ParseError> {
        let (open, close) = self.read_quote_delimiters()?;
        let pattern = self.scan_balanced_string(open, close)?;
        let flags = self.scan_adjacent_word_chars();
        Ok(Token::RegexLit(RegexKind::Match, pattern, flags))
    }

    /// `qr/pattern/flags` or `qr{pattern}flags`
    fn lex_qr(&mut self) -> Result<Token, ParseError> {
        let (open, close) = self.read_quote_delimiters()?;
        let pattern = self.scan_balanced_string(open, close)?;
        let flags = self.scan_adjacent_word_chars();
        Ok(Token::RegexLit(RegexKind::Qr, pattern, flags))
    }

    /// `s/pattern/replacement/flags` or `s{pattern}{replacement}flags`
    fn lex_s(&mut self) -> Result<Token, ParseError> {
        let (open, close) = self.read_quote_delimiters()?;
        let pattern = self.scan_balanced_string(open, close)?;
        // For paired delimiters like s{pat}{repl}, read a new pair.
        // For same-char delimiters like s/pat/repl/, reuse the same delimiter.
        let replacement = if open != close {
            let (_open2, close2) = self.read_quote_delimiters()?;
            self.scan_balanced_string(_open2, close2)?
        } else {
            self.scan_balanced_string(open, close)?
        };
        let flags = self.scan_adjacent_word_chars();
        Ok(Token::SubstLit(pattern, replacement, flags))
    }

    /// `tr/from/to/flags` or `y/from/to/flags`
    fn lex_tr(&mut self) -> Result<Token, ParseError> {
        let (open, close) = self.read_quote_delimiters()?;
        let from = self.scan_balanced_string(open, close)?;
        let to = if open != close {
            let (_open2, close2) = self.read_quote_delimiters()?;
            self.scan_balanced_string(_open2, close2)?
        } else {
            self.scan_balanced_string(open, close)?
        };
        let flags = self.scan_adjacent_word_chars();
        Ok(Token::TranslitLit(from, to, flags))
    }

    fn read_quote_delimiters(&mut self) -> Result<(u8, u8), ParseError> {
        // Match toke.c's scan_str: skip whitespace before the delimiter
        // only if the current byte IS whitespace (or the line is exhausted).
        // `m#foo#` uses `#` as the delimiter — it's not a comment.
        // `m /foo/` skips the space and uses `/`.
        match self.peek_byte() {
            Some(b) if b == b' ' || b == b'\t' => {
                self.skip_ws_and_comments()?;
            }
            None => {
                // End of line — need to cross to next line.
                self.skip_ws_and_comments()?;
            }
            _ => {}
        }
        let open = self.advance_byte().ok_or_else(|| ParseError::new("expected delimiter", Span::new(self.span_pos(), self.span_pos())))?;
        let close = matching_delimiter(open);
        Ok((open, close))
    }

    fn scan_balanced_string(&mut self, open: u8, close: u8) -> Result<String, ParseError> {
        let mut s = String::new();
        let mut depth = 1u32;
        let paired = open != close; // e.g. {}, [], (), <>

        loop {
            match self.advance_byte_in_string(&mut s)? {
                None => return Err(ParseError::new("unterminated string", Span::new(self.span_pos(), self.span_pos()))),
                Some(b'\\') => {
                    if let Some(next) = self.peek_byte() {
                        if next == close || next == open {
                            self.skip(1);
                            s.push(next as char);
                            continue;
                        }
                    }
                    s.push('\\');
                }
                Some(b) if b == close => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    s.push(b as char);
                }
                Some(b) if paired && b == open => {
                    depth += 1;
                    s.push(b as char);
                }
                Some(b) => s.push(b as char),
            }
        }
        Ok(s)
    }

    // ── Operators ─────────────────────────────────────────────

    fn lex_plus(&mut self) -> Token {
        self.skip(1);
        match self.peek_byte() {
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

    fn lex_minus(&mut self, expect: &Expect) -> Result<Token, ParseError> {
        self.skip(1);
        match self.peek_byte() {
            Some(b'-') => {
                self.skip(1);
                Ok(Token::MinusMinus)
            }
            Some(b'=') => {
                self.skip(1);
                Ok(Token::Assign(AssignOp::SubEq))
            }
            Some(b'>') => {
                self.skip(1);
                Ok(Token::Arrow)
            }
            Some(b) if expect.expecting_term() && b.is_ascii_alphabetic() && !self.peek_byte_at(1).is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') => {
                // Filetest: -f, -d, -r, etc.
                self.skip(1);
                Ok(Token::Filetest(b))
            }
            _ => Ok(Token::Minus),
        }
    }

    fn lex_star(&mut self) -> Token {
        self.skip(1);
        match self.peek_byte() {
            Some(b'*') => {
                self.skip(1);
                if self.peek_byte() == Some(b'=') {
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

    fn lex_slash(&mut self, expect: &Expect) -> Result<Token, ParseError> {
        // The lexer always returns Token::DefinedOr for //.  The parser
        // converts it to an empty regex in term position (and consumes
        // any trailing flags like //gi → DefinedOr + Ident("gi")).
        // Only m// produces Token::RegexLit with an empty pattern directly
        // from the lexer.  This eliminates the need for XTERMORDORDOR.
        if self.peek_byte_at(1) == Some(b'/') {
            self.skip(2);
            if self.peek_byte() == Some(b'=') {
                self.skip(1);
                Ok(Token::Assign(AssignOp::DefinedOrEq))
            } else {
                Ok(Token::DefinedOr)
            }
        } else if expect.slash_is_regex() {
            // Single / in term context: regex.
            self.skip(1); // skip opening /
            let pattern = self.scan_to_delimiter(b'/')?;
            let flags = self.scan_adjacent_word_chars();
            Ok(Token::RegexLit(RegexKind::Match, pattern, flags))
        } else {
            self.skip(1);
            if self.peek_byte() == Some(b'=') {
                self.skip(1);
                Ok(Token::Assign(AssignOp::DivEq))
            } else {
                Ok(Token::Slash)
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
        while let Some(b) = self.peek_byte() {
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
        match self.peek_byte() {
            Some(b'.') => {
                self.skip(1);
                if self.peek_byte() == Some(b'.') {
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

    fn lex_less_than(&mut self, expect: &Expect) -> Result<Token, ParseError> {
        self.skip(1); // consume first <
        match self.peek_byte() {
            Some(b'<') => {
                // Could be heredoc (in term position) or left shift.
                if expect.expecting_term() {
                    // Check for heredoc tag after <<
                    let saved = self.line_pos(); // position of second <
                    self.skip(1); // skip second <

                    // <<~ for indented heredocs
                    let indented = self.peek_byte() == Some(b'~');
                    if indented {
                        self.skip(1);
                    }

                    // Skip optional whitespace between << and tag
                    while self.peek_byte() == Some(b' ') || self.peek_byte() == Some(b'\t') {
                        self.skip(1);
                    }

                    match self.peek_byte() {
                        // Quoted tags: <<"TAG" or <<'TAG'
                        Some(b'"') | Some(b'\'') => {
                            return self.lex_heredoc(indented);
                        }
                        // Bare tag: <<IDENT or <<~IDENT
                        Some(b) if b == b'_' || b.is_ascii_alphabetic() => {
                            return self.lex_heredoc(indented);
                        }
                        _ => {
                            // Not a heredoc — rewind to after first <, re-parse as <<
                            if let Some(line) = self.current_line.as_mut() {
                                line.pos = saved + 1;
                            }
                            if self.peek_byte() == Some(b'=') {
                                self.skip(1);
                                return Ok(Token::Assign(AssignOp::ShiftLeftEq));
                            }
                            return Ok(Token::ShiftL);
                        }
                    }
                } else {
                    // Operator position: left shift
                    self.skip(1);
                    if self.peek_byte() == Some(b'=') {
                        self.skip(1);
                        Ok(Token::Assign(AssignOp::ShiftLeftEq))
                    } else {
                        Ok(Token::ShiftL)
                    }
                }
            }
            Some(b'=') => {
                self.skip(1);
                if self.peek_byte() == Some(b'>') {
                    self.skip(1);
                    Ok(Token::Spaceship)
                } else {
                    Ok(Token::NumLe)
                }
            }
            _ => {
                // In term position, < could be readline/glob: <STDIN>, <>, <$fh>, <*.txt>
                if expect.expecting_term() {
                    // Try to scan a readline: <...> where ... is the content
                    let start_pos = self.line_pos(); // just after <
                    let mut content = String::new();
                    let mut found_close = false;
                    while let Some(b) = self.peek_byte() {
                        if b == b'>' {
                            self.skip(1);
                            found_close = true;
                            break;
                        }
                        if b == b'\n' {
                            break;
                        } // no multiline
                        self.skip(1);
                        content.push(b as char);
                    }
                    if found_close {
                        return Ok(Token::Readline(content));
                    }
                    // Not a readline — rewind
                    if let Some(line) = self.current_line.as_mut() {
                        line.pos = start_pos;
                    }
                }
                Ok(Token::NumLt)
            }
        }
    }

    /// Lex a heredoc tag and start body processing via LexerSource.
    /// Position is after `<<` (and optional `~`), at the tag start.
    fn lex_heredoc(&mut self, indented: bool) -> Result<Token, ParseError> {
        let start = self.line_pos();

        // Determine quoting style and extract tag.
        let (kind, tag) = match self.peek_byte() {
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
                while self.peek_byte().is_some_and(|b| b == b'_' || b.is_ascii_alphanumeric()) {
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
                self.source.start_heredoc(tag_bytes, &mut self.current_line);
                self.context_stack.push(LexContext::Interpolating { close: None, open: None, depth: 0 });
                Ok(Token::QuoteBegin(QuoteKind::Heredoc, 0))
            }
            HeredocKind::Indented => {
                self.source.start_indented_heredoc(tag_bytes, &mut self.current_line)?;
                self.context_stack.push(LexContext::Interpolating { close: None, open: None, depth: 0 });
                Ok(Token::QuoteBegin(QuoteKind::Heredoc, 0))
            }
            HeredocKind::Literal => {
                self.source.start_heredoc(tag_bytes, &mut self.current_line);
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
        loop {
            match self.source.next_line()? {
                Some(line) => {
                    body.push_str(&String::from_utf8_lossy(&line.line));
                    body.push('\n');
                }
                None => break, // terminator found by LexerSource
            }
        }
        let kind = if indented { HeredocKind::IndentedLiteral } else { HeredocKind::Literal };
        Ok(Token::HeredocLit(kind, tag.to_string(), body))
    }

    /// Scan a quoted heredoc tag (between matching quotes).
    fn scan_heredoc_tag(&mut self, close: u8) -> Result<String, ParseError> {
        let start = self.line_pos();
        while self.peek_byte().is_some_and(|b| b != close) {
            self.skip(1);
        }
        let tag = String::from_utf8_lossy(self.line_slice(start)).into_owned();
        if self.peek_byte() == Some(close) {
            self.skip(1); // skip closing quote
        }
        Ok(tag)
    }

    fn lex_greater_than(&mut self) -> Token {
        self.skip(1);
        match self.peek_byte() {
            Some(b'>') => {
                self.skip(1);
                if self.peek_byte() == Some(b'=') {
                    self.skip(1);
                    Token::Assign(AssignOp::ShiftRightEq)
                } else {
                    Token::ShiftR
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
        match self.peek_byte() {
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
        match self.peek_byte() {
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
        match self.peek_byte() {
            Some(b'&') => {
                self.skip(1);
                if self.peek_byte() == Some(b'=') {
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
        match self.peek_byte() {
            Some(b'|') => {
                self.skip(1);
                if self.peek_byte() == Some(b'=') {
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

fn matching_delimiter(open: u8) -> u8 {
    match open {
        b'(' => b')',
        b'[' => b']',
        b'{' => b'}',
        b'<' => b'>',
        other => other, // same char for non-paired delimiters like / | ! etc.
    }
}

fn hex_digit(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

// ── Byte-level helpers for brace disambiguation ─────────────

/// Equivalent to Perl's `isWORDCHAR`: `[a-zA-Z0-9_]`.
fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Equivalent to Perl's `isSPACE`.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0C)
}

/// Skip whitespace and `#`-comments in raw source bytes.
/// Equivalent to toke.c's `skipspace()`.
fn skip_ws_bytes(src: &[u8], mut i: usize) -> usize {
    loop {
        while i < src.len() && is_space(src[i]) {
            i += 1;
        }
        if i < src.len() && src[i] == b'#' {
            while i < src.len() && src[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        break;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expect::ExpectNext;

    fn lex_all(src: &str) -> Vec<Token> {
        let mut lexer = Lexer::new(src.as_bytes());
        let mut expect = Expect::Statement;
        let mut tokens = Vec::new();
        loop {
            let spanned = lexer.next_token(&expect).unwrap();
            if matches!(spanned.token, Token::Eof) {
                break;
            }
            // Simple expectation update: after a term, expect operator.
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
                | Token::RegexLit(_, _, _)
                | Token::SubstLit(_, _, _)
                | Token::TranslitLit(_, _, _)
                | Token::HeredocLit(_, _, _)
                | Token::Readline(_)
                | Token::GlobVar(_)
                | Token::QwList(_)
                | Token::SpecialArrayVar(_)
                | Token::SpecialHashVar(_)
                | Token::Arrow => {
                    // Arrow: toke.c's TOKEN(ARROW) doesn't change PL_expect,
                    // so it inherits XOPERATOR from the preceding term.
                    expect = Expect::Operator;
                }
                Token::Semi | Token::LeftBrace => {
                    expect = Expect::Statement;
                }
                // HASHBRACK in toke.c is returned via OPERATOR() which
                // sets PL_expect = XTERM — the first thing in a hash
                // literal is a term (key expression).
                Token::HashBrace => {
                    expect = Expect::Term;
                }
                // Sub-tokens inside strings don't affect expect.
                Token::QuoteBegin(_, _)
                | Token::ConstSegment(_)
                | Token::InterpScalar(_)
                | Token::InterpArray(_)
                | Token::InterpScalarExprStart
                | Token::InterpArrayExprStart => {}
                _ => {
                    expect = Expect::Term;
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
        let expect = Expect::Statement;
        // Consume QuoteBegin.
        let tok = lexer.next_token(&expect).unwrap();
        assert_eq!(tok.token, Token::QuoteBegin(QuoteKind::Heredoc, 0));
        // Consume body content — "hello\rEND\n" is a single non-terminator line.
        let tok = lexer.next_token(&expect).unwrap();
        assert!(matches!(tok.token, Token::ConstSegment(_)));
        // Now at EOF with no terminator found — should error.
        let result = lexer.next_token(&expect);
        assert!(result.is_err(), "expected unterminated heredoc error");
    }

    // ── Indented heredoc indentation mismatch errors ──────────

    #[test]
    fn lex_indented_heredoc_mismatch_croaks() {
        // Body line with wrong indentation should error.
        let src = "<<~END;\n    hello\n  bad indent\n    END\n";
        let mut lexer = Lexer::new(src.as_bytes());
        let expect = Expect::Statement;
        // Consume QuoteBegin.
        lexer.next_token(&expect).unwrap();
        // Consume tokens until we hit the error.
        let mut got_error = false;
        for _ in 0..20 {
            match lexer.next_token(&expect) {
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
        let expect = Expect::Statement;
        lexer.next_token(&expect).unwrap();
        let mut got_error = false;
        for _ in 0..20 {
            match lexer.next_token(&expect) {
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
                Token::StrEq,
                Token::ScalarVar("b".into()),
                Token::StrNe,
                Token::ScalarVar("c".into()),
                Token::StrLt,
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
        // Verify expect state is correct after a string (operator position).
        let tokens = lex_all(r#""hello" . "world""#);
        assert!(tokens.contains(&Token::Dot));
    }

    // ── Regex / substitution / transliteration tests ──────────

    #[test]
    fn lex_bare_regex() {
        let tokens = lex_all("/foo/i");
        assert_eq!(tokens, vec![Token::RegexLit(RegexKind::Match, "foo".into(), Some("i".into())),]);
    }

    #[test]
    fn lex_bare_regex_no_flags() {
        let tokens = lex_all("/hello world/");
        assert_eq!(tokens, vec![Token::RegexLit(RegexKind::Match, "hello world".into(), None),]);
    }

    #[test]
    fn lex_m_regex() {
        let tokens = lex_all("m{foo}i");
        assert_eq!(tokens, vec![Token::RegexLit(RegexKind::Match, "foo".into(), Some("i".into())),]);
    }

    #[test]
    fn lex_m_regex_slash() {
        let tokens = lex_all("m/bar/gx");
        assert_eq!(tokens, vec![Token::RegexLit(RegexKind::Match, "bar".into(), Some("gx".into())),]);
    }

    #[test]
    fn lex_qr_regex() {
        let tokens = lex_all("qr/\\d+/");
        assert_eq!(tokens, vec![Token::RegexLit(RegexKind::Qr, "\\d+".into(), None),]);
    }

    #[test]
    fn lex_substitution() {
        let tokens = lex_all("s/foo/bar/g");
        assert_eq!(tokens, vec![Token::SubstLit("foo".into(), "bar".into(), Some("g".into())),]);
    }

    #[test]
    fn lex_substitution_braces() {
        let tokens = lex_all("s{foo}{bar}g");
        assert_eq!(tokens, vec![Token::SubstLit("foo".into(), "bar".into(), Some("g".into())),]);
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
        // After $x =~ the / should be a regex, not division.
        let tokens = lex_all("$x =~ /foo/");
        assert_eq!(tokens, vec![Token::ScalarVar("x".into()), Token::Binding, Token::RegexLit(RegexKind::Match, "foo".into(), None),]);
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
        // Heredoc body is split into ConstSegments at line boundaries.
        assert_eq!(tokens[0], Token::QuoteBegin(QuoteKind::Heredoc, 0));
        // Collect all ConstSegment content.
        let body: String = tokens.iter().filter_map(|t| if let Token::ConstSegment(s) = t { Some(s.as_str()) } else { None }).collect();
        assert_eq!(body, "line 1\nline 2\nline 3\n");
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
        let tokens = lex_all("/foo/imsxg");
        assert_eq!(tokens, vec![Token::RegexLit(RegexKind::Match, "foo".into(), Some("imsxg".into()))]);
    }

    #[test]
    fn lex_substitution_global() {
        let tokens = lex_all("s/old/new/g");
        assert_eq!(tokens, vec![Token::SubstLit("old".into(), "new".into(), Some("g".into()))]);
    }

    #[test]
    fn lex_transliteration_flags() {
        let tokens = lex_all("tr/a-z/A-Z/cs");
        assert_eq!(tokens, vec![Token::TranslitLit("a-z".into(), "A-Z".into(), Some("cs".into()))]);
    }

    #[test]
    fn lex_regex_after_keyword_term() {
        let tokens = lex_all("print /foo/");
        assert!(tokens.contains(&Token::RegexLit(RegexKind::Match, "foo".into(), None)));
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

    // ═══════════════════════════════════════════════════════════
    // Brace disambiguation: Token::LeftBrace vs Token::HashBrace
    //
    // These tests verify that the lexer faithfully reproduces
    // toke.c's yyl_leftcurly() decision for every code path.
    //
    // Each named Expect constant maps to a toke.c PL_expect state.
    // The expected token matches what toke.c would return:
    //   LeftBrace = PERLY_BRACE_OPEN  (block / subscript)
    //   HashBrace = HASHBRACK         (anonymous hash)
    // ═══════════════════════════════════════════════════════════

    /// Lex `{` (and whatever follows) under an explicit expect state.
    fn lex_brace(src: &str, expect: Expect) -> Token {
        let mut lexer = Lexer::new(src.as_bytes());
        lexer.next_token(&expect).unwrap().token
    }

    // ── Named expect states from toke.c ───────────────────────
    // Each test verifies brace disambiguation for a specific Expect variant.

    #[test]
    fn brace_term_always_hash() {
        // Term: { is always a hash constructor.
        assert_eq!(lex_brace("{}", Expect::Term), Token::HashBrace);
    }

    #[test]
    fn brace_term_hash_even_with_block_content() {
        assert_eq!(lex_brace("{my $x = 1; $x}", Expect::Term), Token::HashBrace);
    }

    #[test]
    fn brace_operator_always_block() {
        // Operator: { is always a block brace (subscript).
        assert_eq!(lex_brace("{key}", Expect::Operator), Token::LeftBrace);
    }

    #[test]
    fn brace_operator_block_even_with_hash_content() {
        assert_eq!(lex_brace("{key => val}", Expect::Operator), Token::LeftBrace);
    }

    #[test]
    fn brace_block_statement_always_block() {
        // Block(Statement): { after if/while/sub — always block.
        assert_eq!(lex_brace("{1}", Expect::Block(ExpectNext::Statement)), Token::LeftBrace);
    }

    #[test]
    fn brace_block_statement_block_even_with_hash_content() {
        assert_eq!(lex_brace("{key => val}", Expect::Block(ExpectNext::Statement)), Token::LeftBrace);
    }

    #[test]
    fn brace_block_operator_always_block() {
        // Block(Operator): { for eval/do/anon sub — always block.
        assert_eq!(lex_brace("{1}", Expect::Block(ExpectNext::Operator)), Token::LeftBrace);
    }

    #[test]
    fn brace_block_term_always_block() {
        // Block(Term): { for sort/map/grep block arg — always block.
        assert_eq!(lex_brace("{1}", Expect::Block(ExpectNext::Term)), Token::LeftBrace);
    }

    #[test]
    fn brace_deref_always_block() {
        // Deref: ${...}, @{...} — always block.
        assert_eq!(lex_brace("{expr}", Expect::Deref), Token::LeftBrace);
    }

    #[test]
    fn brace_deref_block_even_with_hash_content() {
        assert_eq!(lex_brace("{key => val}", Expect::Deref), Token::LeftBrace);
    }

    #[test]
    fn brace_postderef_always_block() {
        // Postderef: ->@*, ->$* context — always block.
        assert_eq!(lex_brace("{key}", Expect::Postderef), Token::LeftBrace);
    }

    // ── Heuristic (XSTATE / Statement) — toke.c default case ─
    //
    // When PL_expect is XSTATE (statement level), toke.c scans the
    // content after { to decide block vs hash.  Lines 6400–6471.

    #[test]
    fn brace_heuristic_empty_is_hash() {
        // {} → hash (toke.c line 6377)
        assert_eq!(lex_brace("{}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_empty_with_space() {
        assert_eq!(lex_brace("{  }", Expect::Statement), Token::HashBrace);
    }

    // ── Heuristic: bareword first term ────────────────────────

    #[test]
    fn brace_heuristic_bareword_fat_comma() {
        // {key => ...} → hash (line 6470: '=' and '>')
        assert_eq!(lex_brace("{key => 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_uppercase_fat_comma() {
        assert_eq!(lex_brace("{Foo => 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_uppercase_comma_is_hash() {
        // {Foo, 1} → hash: !isLOWER('F') (line 6469)
        assert_eq!(lex_brace("{Foo, 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_lowercase_comma_is_block() {
        // {foo, 1} → block: isLOWER('f'), could be func call (line 6469)
        assert_eq!(lex_brace("{foo, 1}", Expect::Statement), Token::LeftBrace);
    }

    #[test]
    fn brace_heuristic_lowercase_fat_comma_is_hash() {
        // {foo => 1} → hash: => always wins regardless of case
        assert_eq!(lex_brace("{foo => 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_no_comma_is_block() {
        // {my $x = 1} → block: no comma/=> after first term
        assert_eq!(lex_brace("{my $x = 1}", Expect::Statement), Token::LeftBrace);
    }

    // ── Heuristic: string first term (lines 6401–6406) ───────

    #[test]
    fn brace_heuristic_single_quoted_comma() {
        // {'key', 1} → hash
        assert_eq!(lex_brace("{'key', 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_single_quoted_fat_comma() {
        assert_eq!(lex_brace("{'key' => 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_double_quoted_comma() {
        assert_eq!(lex_brace("{\"key\", 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_double_quoted_fat_comma() {
        assert_eq!(lex_brace("{\"key\" => 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_backtick_comma() {
        assert_eq!(lex_brace("{`cmd`, 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_string_no_comma_is_block() {
        // {"key"; 1} → block: string but no comma/=> after
        assert_eq!(lex_brace("{\"key\"; 1}", Expect::Statement), Token::LeftBrace);
    }

    #[test]
    fn brace_heuristic_string_with_escapes() {
        // {"he\"llo", 1} → hash: escaped quote inside string
        assert_eq!(lex_brace("{\"he\\\"llo\", 1}", Expect::Statement), Token::HashBrace);
    }

    // ── Heuristic: non-alpha first char (line 6469: !isLOWER) ─

    #[test]
    fn brace_heuristic_number_comma() {
        // {1, 2} → hash: '1' is not isLOWER
        assert_eq!(lex_brace("{1, 2}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_underscore_comma() {
        // {_foo, 1} → hash: '_' is not isLOWER
        assert_eq!(lex_brace("{_foo, 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_dollar_is_block() {
        // {$x + 1} → block: '$' doesn't start a word/quote
        assert_eq!(lex_brace("{$x + 1}", Expect::Statement), Token::LeftBrace);
    }

    #[test]
    fn brace_heuristic_at_is_block() {
        // {@array} → block: '@' doesn't start a word/quote
        assert_eq!(lex_brace("{@array}", Expect::Statement), Token::LeftBrace);
    }

    // ── Heuristic: q-quote constructs (lines 6408–6455) ──────

    #[test]
    fn brace_heuristic_q_slash_comma() {
        // {q/hello/, 1} → hash
        assert_eq!(lex_brace("{q/hello/, 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_qq_slash_comma() {
        assert_eq!(lex_brace("{qq/hello/, 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_qx_slash_comma() {
        assert_eq!(lex_brace("{qx/cmd/, 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_q_braces_comma() {
        // {q{hello}, 1} → hash: q{} with paired delimiters
        assert_eq!(lex_brace("{q{hello}, 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_q_nested_braces_comma() {
        // {q{{nested}}, 1} → hash: q{} with nested braces
        assert_eq!(lex_brace("{q{{nested}}, 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_q_fat_comma() {
        // {q/hello/ => 1} → hash
        assert_eq!(lex_brace("{q/hello/ => 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_q_with_escapes_comma() {
        assert_eq!(lex_brace("{q/he\\'llo/, 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_bare_q_fat_comma() {
        // {q => 1} → hash: bare 'q' as key (toke.c line 6422)
        assert_eq!(lex_brace("{q => 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_bare_qq_fat_comma() {
        // {qq => 1} → hash: 'qq' followed by space (non-word-char),
        // enters q-quote branch, skips whitespace, finds =>
        assert_eq!(lex_brace("{qq => 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_qw_word_comma() {
        // {qw, 1} → hash: 'qw' is a word starting with 'q',
        // *s == 'q' satisfies the check (toke.c line 6469)
        assert_eq!(lex_brace("{qw, 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_qr_word_no_comma_is_block() {
        // {qr; 1} → block: 'qr' is a word, no comma/=> after
        assert_eq!(lex_brace("{qr; 1}", Expect::Statement), Token::LeftBrace);
    }

    #[test]
    fn brace_heuristic_query_word_no_comma_is_block() {
        assert_eq!(lex_brace("{query; 1}", Expect::Statement), Token::LeftBrace);
    }

    // ── Heuristic: comments (skipspace) ───────────────────────

    #[test]
    fn brace_heuristic_comment_then_hash() {
        // { # comment\n key => 1} → hash
        assert_eq!(lex_brace("{ # comment\nkey => 1}", Expect::Statement), Token::HashBrace);
    }

    #[test]
    fn brace_heuristic_comment_then_block() {
        assert_eq!(lex_brace("{ # comment\nmy $x = 1}", Expect::Statement), Token::LeftBrace);
    }

    // ── Integration: lex_all with natural context ─────────────

    #[test]
    fn brace_in_assignment_is_hash() {
        // $x = {...} — after =, lex_all sets Term → HashBrace
        let tokens = lex_all("$x = {key => 1}");
        assert!(tokens.contains(&Token::HashBrace));
        assert!(!tokens.iter().any(|t| *t == Token::LeftBrace));
    }

    #[test]
    fn brace_after_semicolon_hash() {
        // ; {key => 1} — after ;, XSTATE → heuristic → hash
        let tokens = lex_all("1; {key => 1}");
        assert!(tokens.contains(&Token::HashBrace));
    }

    #[test]
    fn brace_after_semicolon_block() {
        // ; {my $x} — after ;, XSTATE → heuristic → block
        let tokens = lex_all("1; {my $x}");
        assert!(tokens.contains(&Token::LeftBrace));
    }

    #[test]
    fn brace_after_arrow_is_block() {
        // $ref->{key} — after ->, XOPERATOR → LeftBrace (subscript)
        let tokens = lex_all("$ref->{key}");
        assert!(tokens.contains(&Token::LeftBrace));
        assert!(!tokens.contains(&Token::HashBrace));
    }
}
