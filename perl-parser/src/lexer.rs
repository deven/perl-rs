//! Lexer — context-sensitive tokenizer.
//!
//! The lexer and parser are inseparable: the lexer reads `self.expect`
//! (set by the parser) to resolve ambiguities like `/` (regex vs division)
//! and `{` (block vs hash).
//!
//! This module implements the core tokenization loop.  Quote-like sublexing,
//! heredocs, and regex scanning are handled by helper methods.

use crate::error::ParseError;
use crate::expect::{BaseExpect, Expect};
use crate::keyword;
use crate::span::Span;
use crate::token::*;

/// Sublexing context — tracks what mode the lexer is in.
#[derive(Clone, Debug)]
enum LexContext {
    /// Inside an interpolating string ("...", qq//, `...`).
    Interpolating {
        close: u8,
        /// For paired delimiters like qq{...}, the open delimiter
        /// for nesting depth tracking.  None for non-paired (qq//).
        open: Option<u8>,
        depth: u32,
    },
}

/// Saved lexer state for checkpoint/restore (used by the parser's
/// re-lex mechanism to undo a speculatively-lexed token).
#[derive(Clone, Debug)]
pub(crate) struct LexerCheckpoint {
    pub pos: usize,
    pub context_depth: usize,
    pub redirect_idx: usize,
    pub redirect_len: usize,
}

/// Lexer state, embedded in the `Parser` struct (not standalone).
///
/// The lexer operates on a byte slice and maintains a position cursor.
/// It reads the `expect` field to resolve context-sensitive ambiguities.
/// The context stack tracks sublexing modes (interpolating strings,
/// regex patterns, heredocs).
pub(crate) struct Lexer<'src> {
    src: &'src [u8],
    pos: usize,
    context_stack: Vec<LexContext>,
    /// When a heredoc tag is encountered, the body is collected eagerly
    /// from subsequent lines.  The lexer then rewinds to continue scanning
    /// the current line.  When skip_ws_and_comments later hits the newline
    /// at the end of that line, it jumps to the position after the heredoc
    /// terminator instead of the next source line.
    heredoc_line_redirects: Vec<usize>,
    /// Index into heredoc_line_redirects: how many have been consumed.
    /// Using an index instead of removing from the vec allows checkpoint/
    /// restore to undo redirect consumption by resetting the index.
    heredoc_redirect_idx: usize,
}

impl<'src> Lexer<'src> {
    pub fn new(src: &'src [u8]) -> Self {
        Lexer { src, pos: 0, context_stack: Vec::new(), heredoc_line_redirects: Vec::new(), heredoc_redirect_idx: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
    }

    /// Save a checkpoint for the parser's re-lex mechanism.
    pub fn checkpoint(&self) -> LexerCheckpoint {
        LexerCheckpoint {
            pos: self.pos,
            context_depth: self.context_stack.len(),
            redirect_idx: self.heredoc_redirect_idx,
            redirect_len: self.heredoc_line_redirects.len(),
        }
    }

    /// Restore to a saved checkpoint, undoing any state changes
    /// (context pushes, redirect consumption/addition) since the checkpoint.
    pub fn restore(&mut self, cp: LexerCheckpoint) {
        self.pos = cp.pos;
        self.context_stack.truncate(cp.context_depth);
        self.heredoc_redirect_idx = cp.redirect_idx;
        self.heredoc_line_redirects.truncate(cp.redirect_len);
    }

    // ── Character access ──────────────────────────────────────

    fn peek_byte(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek_byte_at(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    fn advance_byte(&mut self) -> Option<u8> {
        let b = self.src.get(self.pos).copied()?;
        self.pos += 1;
        // When crossing a newline, fire any pending heredoc redirect.
        // This makes heredoc body skipping transparent to all byte-level
        // scanners (scan_balanced_string, lex_single_quoted_string, etc.),
        // so constructs that span across a heredoc splice work correctly.
        if b == b'\n' && self.heredoc_redirect_idx < self.heredoc_line_redirects.len() {
            self.pos = self.heredoc_line_redirects[self.heredoc_redirect_idx];
            self.heredoc_redirect_idx += 1;
        }
        Some(b)
    }

    fn remaining(&self) -> &'src [u8] {
        &self.src[self.pos..]
    }

    fn at_end(&self) -> bool {
        self.pos >= self.src.len()
    }

    /// Skip to end of source — used after __END__/__DATA__.
    pub fn skip_to_end(&mut self) {
        self.pos = self.src.len();
    }

    // ── Skip whitespace and comments ──────────────────────────

    fn skip_ws_and_comments(&mut self) {
        loop {
            // Skip whitespace (advance_byte handles heredoc redirects on newlines)
            while let Some(b) = self.peek_byte() {
                if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                    self.advance_byte();
                } else {
                    break;
                }
            }
            // Skip line comments
            if self.peek_byte() == Some(b'#') {
                while let Some(b) = self.advance_byte() {
                    if b == b'\n' {
                        break;
                    }
                }
                continue;
            }
            // Skip pod: =word ... =cut at start of line
            if self.peek_byte() == Some(b'=')
                && self.peek_byte_at(1).is_some_and(|b| b.is_ascii_alphabetic())
                && (self.pos == 0 || self.src.get(self.pos - 1) == Some(&b'\n'))
            {
                self.skip_pod();
                continue;
            }
            break;
        }
    }

    /// Skip a pod block: everything from `=word` to `=cut\n`.
    /// Matches Perl 5's behavior: `=cut` must be at start of line,
    /// followed by a non-alphabetic character (or EOF).
    fn skip_pod(&mut self) {
        loop {
            // Advance to next line
            while let Some(b) = self.advance_byte() {
                if b == b'\n' {
                    break;
                }
            }
            if self.at_end() {
                break;
            }
            // Check for =cut at start of line, followed by non-alpha or EOF.
            // Perl uses !isALPHA(s[4]): =cut\n, =cut , =cut123 all match;
            // =cutting does not.
            if self.remaining().starts_with(b"=cut") && !self.remaining().get(4).is_some_and(|b| b.is_ascii_alphabetic()) {
                // Skip the =cut line
                while let Some(b) = self.advance_byte() {
                    if b == b'\n' {
                        break;
                    }
                }
                break;
            }
        }
    }

    // ── Main tokenization entry point ─────────────────────────

    /// Lex the next token.  Uses `expect` to resolve ambiguities.
    /// When inside a sublexing context (interpolating string, etc.),
    /// dispatches to the appropriate sub-lexer instead.
    pub fn next_token(&mut self, expect: &Expect) -> Result<Spanned, ParseError> {
        // If inside a sublexing context, dispatch there.
        if let Some(ctx) = self.context_stack.last().cloned() {
            return match ctx {
                LexContext::Interpolating { close, open, depth } => self.lex_interp_token(close, open, depth),
            };
        }

        self.lex_normal_token(expect)
    }

    /// Lex a token in normal (code) mode.
    fn lex_normal_token(&mut self, expect: &Expect) -> Result<Spanned, ParseError> {
        self.skip_ws_and_comments();

        let start = self.pos as u32;

        if self.at_end() {
            return Ok(Spanned { token: Token::Eof, span: Span::new(start, start) });
        }

        let b = self.peek_byte().unwrap();

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
                self.pos += 1; // skip opening "
                self.context_stack.push(LexContext::Interpolating { close: b'"', open: None, depth: 0 });
                Token::QuoteBegin(QuoteKind::Double, b'"')
            }
            b'`' => {
                self.pos += 1; // skip opening `
                self.context_stack.push(LexContext::Interpolating { close: b'`', open: None, depth: 0 });
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
                self.pos += 1;
                Token::BitXor
            }
            b'~' => {
                self.pos += 1;
                Token::Tilde
            }
            b'\\' => {
                self.pos += 1;
                Token::Backslash
            }
            b'?' => {
                self.pos += 1;
                Token::Question
            }
            b':' => {
                self.pos += 1;
                Token::Colon
            }
            b',' => {
                self.pos += 1;
                Token::Comma
            }
            b';' => {
                self.pos += 1;
                Token::Semi
            }
            b'(' => {
                self.pos += 1;
                Token::LParen
            }
            b')' => {
                self.pos += 1;
                Token::RParen
            }
            b'[' => {
                self.pos += 1;
                Token::LBracket
            }
            b']' => {
                self.pos += 1;
                Token::RBracket
            }
            b'{' => {
                self.pos += 1;
                Token::LBrace
            }
            b'}' => {
                self.pos += 1;
                Token::RBrace
            }

            other => {
                self.pos += 1;
                return Err(ParseError::new(format!("unexpected byte 0x{:02x} ('{}')", other, other as char), Span::new(start, self.pos as u32)));
            }
        };

        let end = self.pos as u32;
        Ok(Spanned { token, span: Span::new(start, end) })
    }

    // ── Number literals ───────────────────────────────────────

    fn lex_number(&mut self) -> Result<Token, ParseError> {
        let start = self.pos;

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
            self.pos += 1; // skip '.'
            self.scan_digits();
            self.scan_exponent();
            let s = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
            let s = s.replace('_', "");
            let n: f64 = s.parse().map_err(|_| ParseError::new("invalid float literal", Span::new(start as u32, self.pos as u32)))?;
            Ok(Token::FloatLit(n))
        } else if self.peek_byte() == Some(b'e') || self.peek_byte() == Some(b'E') {
            // Float with exponent
            self.scan_exponent();
            let s = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
            let s = s.replace('_', "");
            let n: f64 = s.parse().map_err(|_| ParseError::new("invalid float literal", Span::new(start as u32, self.pos as u32)))?;
            Ok(Token::FloatLit(n))
        } else {
            // Integer
            let s = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
            let s = s.replace('_', "");
            // Legacy octal: 0NNN where all digits are 0-7.
            // Perl 5 treats leading-zero integers as octal.
            if s.len() > 1 && s.starts_with('0') && s.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
                let n = i64::from_str_radix(&s[1..], 8).map_err(|_| ParseError::new("invalid octal literal", Span::new(start as u32, self.pos as u32)))?;
                Ok(Token::IntLit(n))
            } else {
                let n: i64 = s.parse().map_err(|_| ParseError::new("invalid integer literal", Span::new(start as u32, self.pos as u32)))?;
                Ok(Token::IntLit(n))
            }
        }
    }

    fn scan_digits(&mut self) {
        while let Some(b) = self.peek_byte() {
            if b.is_ascii_digit() || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn scan_exponent(&mut self) {
        if self.peek_byte() == Some(b'e') || self.peek_byte() == Some(b'E') {
            self.pos += 1;
            if self.peek_byte() == Some(b'+') || self.peek_byte() == Some(b'-') {
                self.pos += 1;
            }
            self.scan_digits();
        }
    }

    fn lex_hex(&mut self) -> Result<Token, ParseError> {
        let start = self.pos;
        self.pos += 2; // skip 0x
        let hex_start = self.pos;
        while let Some(b) = self.peek_byte() {
            if b.is_ascii_hexdigit() || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.src[hex_start..self.pos]).unwrap().replace('_', "");
        let n = i64::from_str_radix(&s, 16).map_err(|_| ParseError::new("invalid hex literal", Span::new(start as u32, self.pos as u32)))?;
        Ok(Token::IntLit(n))
    }

    fn lex_binary(&mut self) -> Result<Token, ParseError> {
        let start = self.pos;
        self.pos += 2; // skip 0b
        let bin_start = self.pos;
        while let Some(b) = self.peek_byte() {
            if b == b'0' || b == b'1' || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.src[bin_start..self.pos]).unwrap().replace('_', "");
        let n = i64::from_str_radix(&s, 2).map_err(|_| ParseError::new("invalid binary literal", Span::new(start as u32, self.pos as u32)))?;
        Ok(Token::IntLit(n))
    }

    fn lex_octal_explicit(&mut self) -> Result<Token, ParseError> {
        let start = self.pos;
        self.pos += 2; // skip 0o
        let oct_start = self.pos;
        while let Some(b) = self.peek_byte() {
            if (b'0'..=b'7').contains(&b) || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.src[oct_start..self.pos]).unwrap().replace('_', "");
        let n = i64::from_str_radix(&s, 8).map_err(|_| ParseError::new("invalid octal literal", Span::new(start as u32, self.pos as u32)))?;
        Ok(Token::IntLit(n))
    }

    // ── Variables ($, @, %) ───────────────────────────────────

    fn lex_dollar(&mut self, _expect: &Expect) -> Result<Token, ParseError> {
        self.pos += 1; // skip $

        // $# — array length
        if self.peek_byte() == Some(b'#') {
            if self.peek_byte_at(1).is_some_and(|b| b == b'_' || b.is_ascii_alphabetic()) {
                self.pos += 1; // skip #
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
                self.pos += 1;
                return Ok(Token::ScalarVar("_".into()));
            }
            Some(b) if b.is_ascii_alphabetic() => {
                let name = self.scan_ident();
                return Ok(Token::ScalarVar(name));
            }
            Some(b'{') => {
                // ${name} — variable with brace disambiguation
                // ${$ref} or ${expr} — dereference block (return Dollar, let parser handle {})
                if self.peek_byte_at(1).is_some_and(|b| b == b'_' || b.is_ascii_alphabetic()) {
                    self.pos += 1; // skip {
                    let name = self.scan_ident();
                    if self.peek_byte() == Some(b'}') {
                        self.pos += 1;
                    }
                    return Ok(Token::ScalarVar(name));
                }
                // Not a simple identifier — deref block
                return Ok(Token::Dollar);
            }
            Some(b'$') => {
                // $$name is scalar dereference; $$ alone is PID
                if self.peek_byte_at(1).is_some_and(|b| b == b'_' || b.is_ascii_alphabetic() || b == b'{') {
                    return Ok(Token::Dollar);
                }
                self.pos += 1;
                return Ok(Token::SpecialVar("$".into()));
            }
            Some(b'!') => {
                self.pos += 1;
                return Ok(Token::SpecialVar("!".into()));
            }
            Some(b'@') => {
                self.pos += 1;
                return Ok(Token::SpecialVar("@".into()));
            }
            Some(b'/') => {
                self.pos += 1;
                return Ok(Token::SpecialVar("/".into()));
            }
            Some(b'\\') => {
                self.pos += 1;
                return Ok(Token::SpecialVar("\\".into()));
            }
            Some(b';') => {
                self.pos += 1;
                return Ok(Token::SpecialVar(";".into()));
            }
            Some(b',') => {
                self.pos += 1;
                return Ok(Token::SpecialVar(",".into()));
            }
            Some(b) if b.is_ascii_digit() => {
                let start = self.pos;
                while self.peek_byte().is_some_and(|b| b.is_ascii_digit()) {
                    self.pos += 1;
                }
                let name = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
                return Ok(Token::SpecialVar(name.into()));
            }
            _ => {}
        }

        Ok(Token::Dollar)
    }

    fn lex_at(&mut self) -> Result<Token, ParseError> {
        self.pos += 1; // skip @
        match self.peek_byte() {
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
            self.pos += 1;
            match self.peek_byte() {
                Some(b) if b == b'_' || b.is_ascii_alphabetic() => {
                    let name = self.scan_ident();
                    Ok(Token::HashVar(name))
                }
                _ => Ok(Token::Percent),
            }
        } else {
            self.pos += 1;
            Ok(Token::Percent)
        }
    }

    // ── Identifiers ───────────────────────────────────────────

    fn scan_ident(&mut self) -> String {
        let start = self.pos;
        while let Some(b) = self.peek_byte() {
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.pos += 1;
            } else if b == b':' && self.peek_byte_at(1) == Some(b':') {
                // Package separator Foo::Bar
                self.pos += 2;
            } else {
                break;
            }
        }
        String::from_utf8_lossy(&self.src[start..self.pos]).into_owned()
    }

    fn lex_word(&mut self, expect: &Expect) -> Result<Token, ParseError> {
        let name = self.scan_ident();

        // After -> (Ref position), all words are identifiers — no keyword
        // lookup.  `$obj->method`, `$obj->keys`, `$obj->print` are all
        // method calls, not keywords.
        if expect.base == BaseExpect::Ref {
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
                return Ok(Token::DataEnd);
            }
            _ => {}
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
        self.pos += 1; // skip opening '
        let mut s = String::new();
        loop {
            match self.advance_byte() {
                None => return Err(ParseError::new("unterminated string", Span::new(self.pos as u32, self.pos as u32))),
                Some(b'\\') => match self.peek_byte() {
                    Some(b'\\') => {
                        self.pos += 1;
                        s.push('\\');
                    }
                    Some(b'\'') => {
                        self.pos += 1;
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
    fn lex_interp_token(&mut self, close: u8, open: Option<u8>, depth: u32) -> Result<Spanned, ParseError> {
        let start = self.pos as u32;

        if self.at_end() {
            return Err(ParseError::new("unterminated string", Span::new(start, start)));
        }

        let b = self.peek_byte().unwrap();

        // Check for closing delimiter.
        if b == close && depth == 0 {
            self.pos += 1;
            self.context_stack.pop();
            return Ok(Spanned { token: Token::QuoteEnd, span: Span::new(start, self.pos as u32) });
        }

        // Check for interpolation.
        if b == b'$' {
            return self.lex_interp_scalar(start);
        }
        if b == b'@' {
            return self.lex_interp_array(start);
        }

        // Otherwise, scan a ConstSegment: everything until we hit
        // $, @, the closing delimiter, or end of input.
        let mut s = String::new();
        let mut current_depth = depth;

        loop {
            match self.peek_byte() {
                None => break,
                Some(b) if b == close && current_depth == 0 => break,
                Some(b'$') | Some(b'@') => break,
                Some(b'\\') => {
                    self.pos += 1;
                    self.process_escape(&mut s, close);
                }
                Some(b) if Some(b) == open => {
                    current_depth += 1;
                    self.pos += 1;
                    s.push(b as char);
                }
                Some(b) if b == close && current_depth > 0 => {
                    current_depth -= 1;
                    self.pos += 1;
                    s.push(b as char);
                }
                Some(b) => {
                    self.pos += 1;
                    s.push(b as char);
                }
            }
        }

        // Update depth in context stack.
        if let Some(LexContext::Interpolating { depth: d, .. }) = self.context_stack.last_mut() {
            *d = current_depth;
        }

        Ok(Spanned { token: Token::ConstSegment(s), span: Span::new(start, self.pos as u32) })
    }

    /// Process a backslash escape inside a double-quoted string.
    /// The backslash has already been consumed.
    fn process_escape(&mut self, s: &mut String, close: u8) {
        match self.peek_byte() {
            Some(b'n') => {
                self.pos += 1;
                s.push('\n');
            }
            Some(b't') => {
                self.pos += 1;
                s.push('\t');
            }
            Some(b'r') => {
                self.pos += 1;
                s.push('\r');
            }
            Some(b'\\') => {
                self.pos += 1;
                s.push('\\');
            }
            Some(b'$') => {
                self.pos += 1;
                s.push('$');
            }
            Some(b'@') => {
                self.pos += 1;
                s.push('@');
            }
            Some(b'0') => {
                self.pos += 1;
                s.push('\0');
            }
            Some(b'a') => {
                self.pos += 1;
                s.push('\x07');
            }
            Some(b'b') => {
                self.pos += 1;
                s.push('\x08');
            }
            Some(b'f') => {
                self.pos += 1;
                s.push('\x0C');
            }
            Some(b'e') => {
                self.pos += 1;
                s.push('\x1B');
            }
            Some(b) if b == close => {
                self.pos += 1;
                s.push(b as char);
            }
            Some(b'x') => {
                self.pos += 1;
                let mut val = 0u8;
                if self.peek_byte() == Some(b'{') {
                    // \x{HH...} — Unicode escape
                    self.pos += 1;
                    let mut n = 0u32;
                    while let Some(b) = self.peek_byte() {
                        if b == b'}' {
                            self.pos += 1;
                            break;
                        }
                        if b.is_ascii_hexdigit() {
                            self.pos += 1;
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
                                self.pos += 1;
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

    /// Lex `$name` or `${name}` interpolation inside a string.
    fn lex_interp_scalar(&mut self, start: u32) -> Result<Spanned, ParseError> {
        self.pos += 1; // skip $

        // ${name} form
        if self.peek_byte() == Some(b'{') {
            self.pos += 1;
            let name = self.scan_ident();
            if self.peek_byte() == Some(b'}') {
                self.pos += 1;
            }
            return Ok(Spanned { token: Token::InterpScalar(name), span: Span::new(start, self.pos as u32) });
        }

        // $name form — must start with alpha or _
        if self.peek_byte().is_some_and(|b| b == b'_' || b.is_ascii_alphabetic()) {
            let name = self.scan_ident();
            return Ok(Spanned { token: Token::InterpScalar(name), span: Span::new(start, self.pos as u32) });
        }

        // Bare $ not followed by a name — treat as literal
        // (Will become a ConstSegment "$")
        Ok(Spanned { token: Token::ConstSegment("$".into()), span: Span::new(start, self.pos as u32) })
    }

    /// Lex `@name` interpolation inside a string.
    fn lex_interp_array(&mut self, start: u32) -> Result<Spanned, ParseError> {
        self.pos += 1; // skip @

        if self.peek_byte().is_some_and(|b| b == b'_' || b.is_ascii_alphabetic()) {
            let name = self.scan_ident();
            return Ok(Spanned { token: Token::InterpArray(name), span: Span::new(start, self.pos as u32) });
        }

        // Bare @ not followed by a name — treat as literal
        Ok(Spanned { token: Token::ConstSegment("@".into()), span: Span::new(start, self.pos as u32) })
    }

    fn scan_to_delimiter(&mut self, delim: u8) -> Result<String, ParseError> {
        let mut s = String::new();
        loop {
            match self.advance_byte() {
                None => return Err(ParseError::new("unterminated string", Span::new(self.pos as u32, self.pos as u32))),
                Some(b'\\') if self.peek_byte() == Some(delim) => {
                    self.pos += 1;
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
        self.context_stack.push(LexContext::Interpolating { close, open: paired_open, depth: 0 });
        Ok(Token::QuoteBegin(QuoteKind::Double, open))
    }

    fn lex_qx(&mut self) -> Result<Token, ParseError> {
        let (open, close) = self.read_quote_delimiters()?;
        let paired_open = if open != close { Some(open) } else { None };
        self.context_stack.push(LexContext::Interpolating { close, open: paired_open, depth: 0 });
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
        let flags = self.scan_regex_flags();
        Ok(Token::RegexLit(RegexKind::Match, pattern, flags))
    }

    /// `qr/pattern/flags` or `qr{pattern}flags`
    fn lex_qr(&mut self) -> Result<Token, ParseError> {
        let (open, close) = self.read_quote_delimiters()?;
        let pattern = self.scan_balanced_string(open, close)?;
        let flags = self.scan_regex_flags();
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
        let flags = self.scan_regex_flags();
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
        let flags = self.scan_regex_flags();
        Ok(Token::TranslitLit(from, to, flags))
    }

    fn read_quote_delimiters(&mut self) -> Result<(u8, u8), ParseError> {
        let open = self.advance_byte().ok_or_else(|| ParseError::new("expected delimiter", Span::new(self.pos as u32, self.pos as u32)))?;
        let close = matching_delimiter(open);
        Ok((open, close))
    }

    fn scan_balanced_string(&mut self, open: u8, close: u8) -> Result<String, ParseError> {
        let mut s = String::new();
        let mut depth = 1u32;
        let paired = open != close; // e.g. {}, [], (), <>

        loop {
            match self.advance_byte() {
                None => return Err(ParseError::new("unterminated string", Span::new(self.pos as u32, self.pos as u32))),
                Some(b'\\') => {
                    if let Some(next) = self.peek_byte() {
                        if next == close || next == open {
                            self.pos += 1;
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
        self.pos += 1;
        match self.peek_byte() {
            Some(b'+') => {
                self.pos += 1;
                Token::PlusPlus
            }
            Some(b'=') => {
                self.pos += 1;
                Token::Assign(AssignOp::AddEq)
            }
            _ => Token::Plus,
        }
    }

    fn lex_minus(&mut self, expect: &Expect) -> Result<Token, ParseError> {
        self.pos += 1;
        match self.peek_byte() {
            Some(b'-') => {
                self.pos += 1;
                Ok(Token::MinusMinus)
            }
            Some(b'=') => {
                self.pos += 1;
                Ok(Token::Assign(AssignOp::SubEq))
            }
            Some(b'>') => {
                self.pos += 1;
                Ok(Token::Arrow)
            }
            Some(b) if expect.expecting_term() && b.is_ascii_alphabetic() && !self.peek_byte_at(1).is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') => {
                // Filetest: -f, -d, -r, etc.
                self.pos += 1;
                Ok(Token::Filetest(b))
            }
            _ => Ok(Token::Minus),
        }
    }

    fn lex_star(&mut self) -> Token {
        self.pos += 1;
        match self.peek_byte() {
            Some(b'*') => {
                self.pos += 1;
                if self.peek_byte() == Some(b'=') {
                    self.pos += 1;
                    Token::Assign(AssignOp::PowEq)
                } else {
                    Token::Power
                }
            }
            Some(b'=') => {
                self.pos += 1;
                Token::Assign(AssignOp::MulEq)
            }
            _ => Token::Star,
        }
    }

    fn lex_slash(&mut self, expect: &Expect) -> Result<Token, ParseError> {
        if expect.slash_is_regex() {
            // Regex: /pattern/flags
            self.pos += 1; // skip opening /
            let pattern = self.scan_to_delimiter(b'/')?;
            let flags = self.scan_regex_flags();
            Ok(Token::RegexLit(RegexKind::Match, pattern, flags))
        } else {
            self.pos += 1;
            match self.peek_byte() {
                Some(b'/') => {
                    self.pos += 1;
                    if self.peek_byte() == Some(b'=') {
                        self.pos += 1;
                        Ok(Token::Assign(AssignOp::DorEq))
                    } else {
                        Ok(Token::DorDor)
                    }
                }
                Some(b'=') => {
                    self.pos += 1;
                    Ok(Token::Assign(AssignOp::DivEq))
                }
                _ => Ok(Token::Slash),
            }
        }
    }

    fn scan_regex_flags(&mut self) -> String {
        let start = self.pos;
        while let Some(b) = self.peek_byte() {
            if b.is_ascii_alphabetic() {
                self.pos += 1;
            } else {
                break;
            }
        }
        String::from_utf8_lossy(&self.src[start..self.pos]).into_owned()
    }

    fn lex_dot(&mut self) -> Token {
        self.pos += 1;
        match self.peek_byte() {
            Some(b'.') => {
                self.pos += 1;
                if self.peek_byte() == Some(b'.') {
                    self.pos += 1;
                    Token::DotDotDot
                } else {
                    Token::DotDot
                }
            }
            Some(b'=') => {
                self.pos += 1;
                Token::Assign(AssignOp::ConcatEq)
            }
            _ => Token::Dot,
        }
    }

    fn lex_less_than(&mut self, expect: &Expect) -> Result<Token, ParseError> {
        self.pos += 1; // consume first <
        match self.peek_byte() {
            Some(b'<') => {
                // Could be heredoc (in term position) or left shift.
                if expect.expecting_term() {
                    // Check for heredoc tag after <<
                    let saved = self.pos; // position of second <
                    self.pos += 1; // skip second <

                    // <<~ for indented heredocs
                    let indented = self.peek_byte() == Some(b'~');
                    if indented {
                        self.pos += 1;
                    }

                    // Skip optional whitespace between << and tag
                    while self.peek_byte() == Some(b' ') || self.peek_byte() == Some(b'\t') {
                        self.pos += 1;
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
                            self.pos = saved + 1;
                            if self.peek_byte() == Some(b'=') {
                                self.pos += 1;
                                return Ok(Token::Assign(AssignOp::ShiftLEq));
                            }
                            return Ok(Token::ShiftL);
                        }
                    }
                } else {
                    // Operator position: left shift
                    self.pos += 1;
                    if self.peek_byte() == Some(b'=') {
                        self.pos += 1;
                        Ok(Token::Assign(AssignOp::ShiftLEq))
                    } else {
                        Ok(Token::ShiftL)
                    }
                }
            }
            Some(b'=') => {
                self.pos += 1;
                if self.peek_byte() == Some(b'>') {
                    self.pos += 1;
                    Ok(Token::Spaceship)
                } else {
                    Ok(Token::NumLe)
                }
            }
            _ => {
                // In term position, < could be readline/glob: <STDIN>, <>, <$fh>, <*.txt>
                if expect.expecting_term() {
                    // Try to scan a readline: <...> where ... is the content
                    let start_pos = self.pos; // just after <
                    let mut content = String::new();
                    let mut found_close = false;
                    while let Some(b) = self.peek_byte() {
                        if b == b'>' {
                            self.pos += 1;
                            found_close = true;
                            break;
                        }
                        if b == b'\n' {
                            break;
                        } // no multiline
                        self.pos += 1;
                        content.push(b as char);
                    }
                    if found_close {
                        return Ok(Token::Readline(content));
                    }
                    // Not a readline — rewind
                    self.pos = start_pos;
                }
                Ok(Token::NumLt)
            }
        }
    }

    /// Lex a heredoc tag and eagerly collect the body.
    /// Position is after `<<` (and optional `~`), at the tag start.
    fn lex_heredoc(&mut self, indented: bool) -> Result<Token, ParseError> {
        let start = self.pos;

        // Determine quoting style and extract tag.
        let (kind, tag) = match self.peek_byte() {
            Some(b'\'') => {
                // <<'TAG' — literal
                self.pos += 1;
                let tag = self.scan_heredoc_tag(b'\'')?;
                let k = if indented { HeredocKind::IndentedLiteral } else { HeredocKind::Literal };
                (k, tag)
            }
            Some(b'"') => {
                // <<"TAG" — interpolating (explicit)
                self.pos += 1;
                let tag = self.scan_heredoc_tag(b'"')?;
                let k = if indented { HeredocKind::Indented } else { HeredocKind::Interpolating };
                (k, tag)
            }
            _ => {
                // Bare identifier — interpolating
                let tag_start = self.pos;
                while self.peek_byte().is_some_and(|b| b == b'_' || b.is_ascii_alphanumeric()) {
                    self.pos += 1;
                }
                let tag = String::from_utf8_lossy(&self.src[tag_start..self.pos]).into_owned();
                let k = if indented { HeredocKind::Indented } else { HeredocKind::Interpolating };
                (k, tag)
            }
        };

        if tag.is_empty() {
            return Err(ParseError::new("empty heredoc tag", Span::new(start as u32, self.pos as u32)));
        }

        // Save position — the rest of the current line continues from here.
        let rest_of_line_pos = self.pos;

        // Find the end of the current line.
        while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
            self.pos += 1;
        }
        if self.pos < self.src.len() {
            self.pos += 1; // skip the \n
        }

        // Now collect body lines until the terminator.
        let body_start = self.pos;
        let mut body = String::new();
        let mut found_terminator = false;

        while self.pos < self.src.len() {
            let line_start = self.pos;
            // Read to end of line
            while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
                self.pos += 1;
            }
            let line_end = self.pos;
            if self.pos < self.src.len() {
                self.pos += 1; // skip \n
            }

            let line = &self.src[line_start..line_end];

            // Check if this line is the terminator.
            let trimmed = if indented {
                // For <<~, strip leading whitespace before comparing.
                let mut i = 0;
                while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
                    i += 1;
                }
                &line[i..]
            } else {
                line
            };

            if trimmed == tag.as_bytes() {
                found_terminator = true;
                break;
            }

            // Add line to body (including the newline).
            body.push_str(&String::from_utf8_lossy(line));
            body.push('\n');
        }

        if !found_terminator {
            return Err(ParseError::new(format!("can't find heredoc terminator '{tag}'"), Span::new(body_start as u32, self.pos as u32)));
        }

        // For indented heredocs, strip common leading whitespace from body.
        if indented && !body.is_empty() {
            body = strip_heredoc_indent(&body);
        }

        // Save the position after the terminator — this is where scanning
        // resumes after the current line is finished.
        let after_terminator = self.pos;

        // Rewind to the rest of the current line.
        self.pos = rest_of_line_pos;

        // Register the redirect: when skip_ws_and_comments hits the
        // newline at the end of the current line, jump to after_terminator.
        self.heredoc_line_redirects.push(after_terminator);

        Ok(Token::HeredocLit(kind, tag, body))
    }

    /// Scan a quoted heredoc tag (between matching quotes).
    fn scan_heredoc_tag(&mut self, close: u8) -> Result<String, ParseError> {
        let start = self.pos;
        while self.pos < self.src.len() && self.src[self.pos] != close {
            self.pos += 1;
        }
        let tag = String::from_utf8_lossy(&self.src[start..self.pos]).into_owned();
        if self.pos < self.src.len() {
            self.pos += 1; // skip closing quote
        }
        Ok(tag)
    }

    fn lex_greater_than(&mut self) -> Token {
        self.pos += 1;
        match self.peek_byte() {
            Some(b'>') => {
                self.pos += 1;
                if self.peek_byte() == Some(b'=') {
                    self.pos += 1;
                    Token::Assign(AssignOp::ShiftREq)
                } else {
                    Token::ShiftR
                }
            }
            Some(b'=') => {
                self.pos += 1;
                Token::NumGe
            }
            _ => Token::NumGt,
        }
    }

    fn lex_equals(&mut self) -> Token {
        self.pos += 1;
        match self.peek_byte() {
            Some(b'=') => {
                self.pos += 1;
                Token::NumEq
            }
            Some(b'~') => {
                self.pos += 1;
                Token::Binding
            }
            Some(b'>') => {
                self.pos += 1;
                Token::FatComma
            }
            _ => Token::Assign(AssignOp::Eq),
        }
    }

    fn lex_bang(&mut self) -> Token {
        self.pos += 1;
        match self.peek_byte() {
            Some(b'=') => {
                self.pos += 1;
                Token::NumNe
            }
            Some(b'~') => {
                self.pos += 1;
                Token::NotBinding
            }
            _ => Token::Bang,
        }
    }

    fn lex_ampersand(&mut self) -> Token {
        self.pos += 1;
        match self.peek_byte() {
            Some(b'&') => {
                self.pos += 1;
                if self.peek_byte() == Some(b'=') {
                    self.pos += 1;
                    Token::Assign(AssignOp::AndEq)
                } else {
                    Token::AndAnd
                }
            }
            Some(b'=') => {
                self.pos += 1;
                Token::Assign(AssignOp::BitAndEq)
            }
            _ => Token::BitAnd,
        }
    }

    fn lex_pipe(&mut self) -> Token {
        self.pos += 1;
        match self.peek_byte() {
            Some(b'|') => {
                self.pos += 1;
                if self.peek_byte() == Some(b'=') {
                    self.pos += 1;
                    Token::Assign(AssignOp::OrEq)
                } else {
                    Token::OrOr
                }
            }
            Some(b'=') => {
                self.pos += 1;
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

/// Strip the common leading whitespace from an indented heredoc body (<<~).
fn strip_heredoc_indent(body: &str) -> String {
    // Find the minimum indentation (ignoring empty lines).
    let min_indent = body.lines().filter(|line| !line.trim().is_empty()).map(|line| line.len() - line.trim_start().len()).min().unwrap_or(0);

    if min_indent == 0 {
        return body.to_string();
    }

    body.lines().map(|line| if line.len() >= min_indent { &line[min_indent..] } else { line }).collect::<Vec<_>>().join("\n")
        + if body.ends_with('\n') { "\n" } else { "" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex_all(src: &str) -> Vec<Token> {
        let mut lexer = Lexer::new(src.as_bytes());
        let mut expect = Expect::XSTATE;
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
                | Token::RParen
                | Token::RBracket
                | Token::RBrace
                | Token::PlusPlus
                | Token::MinusMinus
                | Token::SpecialVar(_)
                | Token::ArrayLen(_)
                | Token::QuoteEnd
                | Token::RegexLit(_, _, _)
                | Token::SubstLit(_, _, _)
                | Token::TranslitLit(_, _, _)
                | Token::HeredocLit(_, _, _)
                | Token::Readline(_) => {
                    expect.base = BaseExpect::Operator;
                }
                Token::Semi | Token::LBrace => {
                    expect = Expect::XSTATE;
                }
                // Sub-tokens inside strings don't affect expect.
                Token::QuoteBegin(_, _) | Token::ConstSegment(_) | Token::InterpScalar(_) | Token::InterpArray(_) => {}
                _ => {
                    expect.base = BaseExpect::Term;
                }
            }
            tokens.push(spanned.token);
        }
        tokens
    }

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
        assert!(tokens.contains(&Token::NumEq));
        assert!(tokens.contains(&Token::NumNe));
        assert!(tokens.contains(&Token::NumLe));
        assert!(tokens.contains(&Token::NumGe));
        assert!(tokens.contains(&Token::Spaceship));
    }

    #[test]
    fn lex_string_cmp_ops() {
        let tokens = lex_all("$a eq $b ne $c lt $d");
        assert!(tokens.contains(&Token::StrEq));
        assert!(tokens.contains(&Token::StrNe));
        assert!(tokens.contains(&Token::StrLt));
    }

    #[test]
    fn lex_arrow_and_deref() {
        let tokens = lex_all("$ref->{key}");
        assert_eq!(tokens, vec![Token::ScalarVar("ref".into()), Token::Arrow, Token::LBrace, Token::Ident("key".into()), Token::RBrace,]);
    }

    #[test]
    fn lex_hex_literal() {
        let tokens = lex_all("0xFF");
        assert_eq!(tokens, vec![Token::IntLit(255)]);
    }

    #[test]
    fn lex_binary_literal() {
        let tokens = lex_all("0b1010");
        assert_eq!(tokens, vec![Token::IntLit(10)]);
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
        assert!(tokens.contains(&Token::AndAnd));
        assert!(tokens.contains(&Token::OrOr));
        assert!(tokens.contains(&Token::DorDor));
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
        assert_eq!(tokens, vec![Token::RegexLit(RegexKind::Match, "foo".into(), "i".into()),]);
    }

    #[test]
    fn lex_bare_regex_no_flags() {
        let tokens = lex_all("/hello world/");
        assert_eq!(tokens, vec![Token::RegexLit(RegexKind::Match, "hello world".into(), "".into()),]);
    }

    #[test]
    fn lex_m_regex() {
        let tokens = lex_all("m{foo}i");
        assert_eq!(tokens, vec![Token::RegexLit(RegexKind::Match, "foo".into(), "i".into()),]);
    }

    #[test]
    fn lex_m_regex_slash() {
        let tokens = lex_all("m/bar/gx");
        assert_eq!(tokens, vec![Token::RegexLit(RegexKind::Match, "bar".into(), "gx".into()),]);
    }

    #[test]
    fn lex_qr_regex() {
        let tokens = lex_all("qr/\\d+/");
        assert_eq!(tokens, vec![Token::RegexLit(RegexKind::Qr, "\\d+".into(), "".into()),]);
    }

    #[test]
    fn lex_substitution() {
        let tokens = lex_all("s/foo/bar/g");
        assert_eq!(tokens, vec![Token::SubstLit("foo".into(), "bar".into(), "g".into()),]);
    }

    #[test]
    fn lex_substitution_braces() {
        let tokens = lex_all("s{foo}{bar}g");
        assert_eq!(tokens, vec![Token::SubstLit("foo".into(), "bar".into(), "g".into()),]);
    }

    #[test]
    fn lex_transliteration() {
        let tokens = lex_all("tr/a-z/A-Z/");
        assert_eq!(tokens, vec![Token::TranslitLit("a-z".into(), "A-Z".into(), "".into()),]);
    }

    #[test]
    fn lex_y_transliteration() {
        let tokens = lex_all("y/abc/def/");
        assert_eq!(tokens, vec![Token::TranslitLit("abc".into(), "def".into(), "".into()),]);
    }

    #[test]
    fn lex_regex_in_expression() {
        // After $x =~ the / should be a regex, not division.
        let tokens = lex_all("$x =~ /foo/");
        assert_eq!(tokens, vec![Token::ScalarVar("x".into()), Token::Binding, Token::RegexLit(RegexKind::Match, "foo".into(), "".into()),]);
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
        assert_eq!(tokens, vec![Token::HeredocLit(HeredocKind::Interpolating, "END".into(), "Hello, world!\n".into()), Token::Semi,]);
    }

    #[test]
    fn lex_heredoc_double_quoted() {
        let src = "<<\"END\";\nHello!\nEND\n";
        let tokens = lex_all(src);
        assert_eq!(tokens, vec![Token::HeredocLit(HeredocKind::Interpolating, "END".into(), "Hello!\n".into()), Token::Semi,]);
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
        match &tokens[0] {
            Token::HeredocLit(_, _, body) => {
                assert_eq!(body, "line 1\nline 2\nline 3\n");
            }
            other => panic!("expected HeredocLit, got {other:?}"),
        }
    }

    #[test]
    fn lex_heredoc_with_rest_of_line() {
        // The `. " suffix"` should be tokenized from the current line.
        let src = "<<END . \" suffix\";\nbody\nEND\n";
        let tokens = lex_all(src);
        assert_eq!(
            tokens,
            vec![
                Token::HeredocLit(HeredocKind::Interpolating, "END".into(), "body\n".into()),
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
        match &tokens[0] {
            Token::HeredocLit(HeredocKind::Indented, _, body) => {
                assert_eq!(body, "hello\nworld\n");
            }
            other => panic!("expected indented HeredocLit, got {other:?}"),
        }
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

        // First token: heredoc with body "body\n"
        match &tokens[0] {
            Token::HeredocLit(HeredocKind::Interpolating, tag, body) => {
                assert_eq!(tag, "EOF");
                assert_eq!(body, "body\n");
            }
            other => panic!("expected HeredocLit, got {other:?}"),
        }

        // Then comma
        assert_eq!(tokens[1], Token::Comma);

        // Then q{} string: "before\nafter\n" — the heredoc body is skipped
        match &tokens[2] {
            Token::StrLit(s) => {
                assert_eq!(s, "before\nafter\n");
            }
            other => panic!("expected StrLit for q{{}}, got {other:?}"),
        }
    }
}
