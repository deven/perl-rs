//! Lexer — tokenizer.
//!
//! The lexer returns unambiguous tokens; where Perl syntax is context-sensitive (e.g. `<<` meaning heredoc vs shift-
//! left, `%` meaning hash-sigil vs modulo), the parser invokes specific hook methods (e.g.
//! `lex_heredoc_after_shift_left`, `lex_hash_var_after_percent`) to drive the disambiguation.
//!
//! This module implements the core tokenization loop.  Quote-like sublexing, heredocs, and regex scanning are handled
//! by helper methods.

use crate::error::ParseError;
use crate::keyword::{self, Keyword};
use crate::pragma::Features;
use crate::source::LexerSource;
use crate::span::Span;
use crate::token::*;
use bytes::Bytes;
use memchr::{memchr, memchr2, memchr3};
use std::collections::VecDeque;
use unicode_normalization::UnicodeNormalization;
use unicode_xid::UnicodeXID;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/lexer_tests.rs"]
mod tests;

/// Sublexing context — tracks what mode the lexer is in.
///
/// When `expr_depth > 0`, the lexer is in expression-parsing mode inside `${expr}` or `@{expr}`.  When `expr_depth ==
/// 0`, the lexer is in body-scanning mode (string/regex content).
///
/// When `chain_active` is set, the lexer is producing normal code tokens for a subscript chain that follows `$name` or
/// `@name` inside a string (e.g. `"$h->{k}[0]"`).  `chain_depth` tracks `[`/`{` nesting within the chain; the chain
/// ends when a closing bracket returns depth to 0 and no continuation (`[`, `{`, `->[`, `->{`) follows.
/// `chain_end_pending` is set between tokens when the probe has detected end-of-chain — the next `lex_token` call emits
/// `InterpChainEnd` and clears the chain state.
#[derive(Clone, Debug, Default)]
struct LexContext {
    /// Opening delimiter character.  `None` for heredocs (end signaled by LexerSource).
    delim: Option<char>,
    /// Delimiter nesting depth (for paired delimiters like `{}`).
    depth: u32,
    /// Brace depth inside `${expr}` or `@{expr}`.  When > 0, the lexer produces normal code tokens.  When 0, it
    /// produces string body tokens via `lex_body`.
    expr_depth: u32,
    /// Whether `$`/`@` trigger interpolation.
    interpolating: bool,
    /// Whether escapes pass through raw (for regex, tr, prototypes).
    raw: bool,
    /// Whether to detect `(?{...})` code blocks (regex mode).
    regex: bool,
    /// Inside a subscript chain (see type-level doc).
    chain_active: bool,
    /// Bracket/brace nesting inside the chain.
    chain_depth: u32,
    /// Chain end detected; emit `InterpChainEnd` on the next call.
    chain_end_pending: bool,
}

impl LexContext {
    /// Convenience for the common string/regex push pattern: opening delimiter plus the three behavior flags.  Chain
    /// fields default to false/0.
    fn new(delim: Option<char>, interpolating: bool, raw: bool, regex: bool) -> Self {
        LexContext { delim, interpolating, raw, regex, ..Default::default() }
    }
}

/// Format sublexing state.  Orthogonal to `context_stack` because format mode is line-oriented rather than delimiter-
/// oriented.
///
/// A picture line is tokenized in one pass (tildes normalized to spaces, then fields and literals extracted) and the
/// resulting tokens are queued for the lexer to drain.  Argument lines run in one of two sub-modes: line-terminated
/// (the default) or brace-matched (entered via `format_args_enter_braced`).
struct FormatState {
    /// Pre-tokenized spans queued for emission.  Drained before reading more lines.
    queue: VecDeque<Spanned>,
    mode: FormatMode,
}

#[derive(Clone, Copy, Debug)]
enum FormatMode {
    /// Read and classify the next line.  Default mode.
    Body,
    /// Emit normal code tokens until a newline at depth 0.  Entered after emitting `FormatArgsBegin` when no `{` is
    /// consumed.
    ArgsLine,
    /// Emit normal code tokens until `}` brings `depth` to 0.  Entered after the parser consumes the opening `{` and
    /// calls `format_args_enter_braced`.  `depth` starts at 1.
    ArgsBraced { depth: u32 },
    /// Pending FormatArgsBegin — next `lex_token` call emits it and transitions to `ArgsLine` (or the parser may call
    /// `format_args_enter_braced` first).
    PendingArgsBegin,
    /// Format body has been terminated by `.`; the SublexEnd has been queued.  After it's drained, the format state is
    /// torn down.
    Finished,
}

/// Lexer state, owned by the `Parser`.
///
/// The lexer operates on lines delivered by `LexerSource`.  The context stack tracks sublexing modes (interpolating
/// strings, regex patterns, heredocs).  Context-sensitive disambiguation (e.g. heredoc vs shift-left for `<<`) is
/// driven by the parser via explicit hook methods.
///
/// CRLF normalization is handled by `LexerSource` at the line level.
pub(crate) struct Lexer {
    source: LexerSource,
    context_stack: Vec<LexContext>,
    /// Deferred error from auto-loading in `peek_byte`.  Surfaced on the next call to `lex_token`.
    pending_error: Option<ParseError>,
    /// Active format sublex state, if we're inside a format body.  `Some` between `start_format` and the `.`
    /// terminator's `SublexEnd`.
    format_state: Option<FormatState>,
    /// Whether `use utf8` is active.  Written by the parser when processing `use utf8` / `no utf8` and when restoring
    /// pragma state at block boundaries.  Read by the lexer for error diagnostics on high bytes outside strings.
    pub(crate) utf8_mode: bool,
    /// Fast-path composite: `utf8_mode && !source.line.ascii_only`.  When false, the current line is pure ASCII and
    /// all UTF-8 decoding, XID checks, and NFC normalization can be skipped.  Updated whenever `utf8_mode` changes or a
    /// new line is loaded.
    pub(crate) effective_utf8: bool,
    /// Feature flags synced from the parser's `Pragmas::features`.  Written by the parser when features change or when
    /// restoring pragma state at block boundaries.
    pub(crate) features: Features,
    /// Stacked cumulative case-modification flags.  Each `\L`/`\U`/`\F`/`\Q` pushes the current flags ORed with the new
    /// mode; `\E` pops, reverting to the enclosing flags.
    case_mod_stack: Vec<CaseMod>,
    /// `\l` pending — lowercase the very next character only.
    case_mod_lcfirst: bool,
    /// `\u` pending — titlecase the very next character only.
    case_mod_ucfirst: bool,
    /// Set by `lex_word` when `__DATA__` or `__END__` triggers logical end-of-source, or by the `^D`/`^Z` handler.
    /// When true, `lex_normal_token` returns `Eof` immediately.
    logical_eof: bool,
    /// Set when `__DATA__` or `__END__` triggers logical EOF.  Stores the keyword and the byte offset where trailing
    /// data begins (for the `<DATA>` filehandle).  The parser reads this after the statement loop exits.
    pub(crate) data_end_info: Option<(Keyword, u32)>,
}

impl Lexer {
    pub fn new(src: &[u8]) -> Self {
        let source = LexerSource::new(src);
        let bom = source.bom_utf8;
        Lexer {
            source,
            context_stack: Vec::new(),
            pending_error: None,
            format_state: None,
            utf8_mode: bom,
            effective_utf8: false,
            features: Features::DEFAULT,
            case_mod_stack: Vec::new(),
            case_mod_lcfirst: false,
            case_mod_ucfirst: false,
            logical_eof: false,
            data_end_info: None,
        }
    }

    /// Construct with an explicit filename (used for `__FILE__` and diagnostic messages).  Equivalent to `Lexer::new`
    /// when the caller doesn't care about filename reporting.
    pub fn with_filename(src: &[u8], filename: impl Into<String>) -> Self {
        let source = LexerSource::with_filename(src, filename);
        let bom = source.bom_utf8;
        Lexer {
            source,
            context_stack: Vec::new(),
            pending_error: None,
            format_state: None,
            utf8_mode: bom,
            effective_utf8: false,
            features: Features::DEFAULT,
            case_mod_stack: Vec::new(),
            case_mod_lcfirst: false,
            case_mod_ucfirst: false,
            logical_eof: false,
            data_end_info: None,
        }
    }

    /// Global byte position in the original source.
    pub fn pos(&self) -> usize {
        match &self.source.line {
            Some(line) => line.offset + line.pos,
            None => self.source.cursor(),
        }
    }

    /// Is byte `b` a valid identifier-start character?  In ASCII mode: `[a-zA-Z_]`.  In UTF-8 mode: also accepts lead
    /// bytes ≥ 0x80 (the full multi-byte decode and Unicode letter check happens inside `scan_ident`).
    fn is_ident_start(&self, b: u8) -> bool {
        b == b'_' || b.is_ascii_alphabetic() || (self.effective_utf8 && b >= 0x80)
    }

    /// Check whether the given bytes form an active keyword.  Returns true for unconditional keywords (print, grep,
    /// etc.) and for feature-gated keywords whose feature is currently enabled.  Returns false for feature-gated
    /// keywords whose feature is off (they are just barewords in that case).  Also returns true for quote-like
    /// operators (q, qq, qr, qx, m, s, tr, y) which are not in the keyword table but must still prevent apostrophe
    /// consumption in `scan_ident`.
    fn is_active_keyword(&self, name: &[u8]) -> bool {
        keyword::lookup_keyword(name, self.features).is_some()
    }

    /// Decode the UTF-8 character starting at the current position.  Returns `(char, byte_length)` on success, `None`
    /// for invalid UTF-8 or empty remaining input.
    fn peek_utf8_char(&self) -> Option<(char, usize)> {
        let r = self.remaining();
        if r.is_empty() || r[0] < 0x80 {
            // ASCII or empty — caller should handle directly.
            return None;
        }
        // Determine how many bytes this UTF-8 lead byte claims.
        let len = match r[0] {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => return None, // invalid lead byte
        };
        if r.len() < len {
            return None;
        }
        std::str::from_utf8(&r[..len]).ok().and_then(|s| s.chars().next()).map(|c| (c, len))
    }

    /// NFC-normalize a string if UTF-8 mode is active and the string contains non-ASCII bytes.  Returns the input
    /// unchanged for ASCII-only strings or when UTF-8 mode is off.
    #[inline]
    fn nfc_normalize(&self, s: String) -> String {
        if self.effective_utf8 && s.bytes().any(|b| b >= 0x80) { s.nfc().collect() } else { s }
    }

    /// Push a character to `s` with the active case modification applied.  One-shot modes (`\l`/`\u`) override
    /// persistent case modes for one character, then clear.
    fn push_case_mod(&mut self, s: &mut String, c: char) {
        let flags = self.case_mod_stack.last().copied().unwrap_or(CaseMod::EMPTY);
        if flags.is_empty() && !self.case_mod_lcfirst && !self.case_mod_ucfirst {
            s.push(c);
            return;
        }

        // Step 1: case transformation.  One-shot overrides persistent mode for this character.
        if self.case_mod_lcfirst {
            self.case_mod_lcfirst = false;
            let lc: String = c.to_lowercase().collect();
            if flags.contains(CaseMod::QUOTEMETA) {
                Self::push_quotemeta(s, &lc);
            } else {
                s.push_str(&lc);
            }
            return;
        }
        if self.case_mod_ucfirst {
            self.case_mod_ucfirst = false;
            let uc: String = c.to_uppercase().collect();
            if flags.contains(CaseMod::QUOTEMETA) {
                Self::push_quotemeta(s, &uc);
            } else {
                s.push_str(&uc);
            }
            return;
        }

        // Persistent case mode.
        if flags.contains(CaseMod::UPPER) {
            let uc: String = c.to_uppercase().collect();
            if flags.contains(CaseMod::QUOTEMETA) {
                Self::push_quotemeta(s, &uc);
            } else {
                s.push_str(&uc);
            }
            return;
        }
        if flags.contains(CaseMod::LOWER) || flags.contains(CaseMod::FOLD) {
            let lc: String = c.to_lowercase().collect();
            if flags.contains(CaseMod::QUOTEMETA) {
                Self::push_quotemeta(s, &lc);
            } else {
                s.push_str(&lc);
            }
            return;
        }

        // Quotemeta only (no case change).
        if flags.contains(CaseMod::QUOTEMETA) {
            if c.is_ascii_alphanumeric() || c == '_' {
                s.push(c);
            } else {
                s.push('\\');
                s.push(c);
            }
            return;
        }

        s.push(c);
    }

    /// Apply quotemeta escaping to a (possibly multi-char) string.
    fn push_quotemeta(s: &mut String, text: &str) {
        for ch in text.chars() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                s.push(ch);
            } else {
                s.push('\\');
                s.push(ch);
            }
        }
    }

    /// Snapshot the active case-modification state for an interpolated expression.  Returns the cumulative flags
    /// including any pending one-shot.  Clears the one-shot flags (they apply to this interpolation only).
    pub(crate) fn take_interp_case_mod(&mut self) -> CaseMod {
        let mut flags = self.case_mod_stack.last().copied().unwrap_or(CaseMod::EMPTY);
        if self.case_mod_lcfirst {
            flags |= CaseMod::LCFIRST;
            self.case_mod_lcfirst = false;
        }
        if self.case_mod_ucfirst {
            flags |= CaseMod::UCFIRST;
            self.case_mod_ucfirst = false;
        }
        flags
    }

    // ── Byte access (auto-loading) ──────────────────────────

    /// Peek at the current byte without advancing.  Auto-loads the next line when the current one is exhausted.
    /// Returns `b'\n'` for a terminated line ending.  Returns `None` only at true EOF (or heredoc end in peek mode).
    ///
    /// `peek_heredoc`: when true, a heredoc end-of-body signal from `next_line` is preserved (not consumed).  Use
    /// `true` inside body scanning loops, `false` at entry points.
    fn peek_byte(&mut self, peek_heredoc: bool) -> Option<u8> {
        // Check current line for available bytes.
        if let Some(line) = &self.source.line
            && let Some(b) = line.peek_byte()
        {
            return Some(b);
        }
        // No line or line exhausted.  Try to load a new one.  On success, replace the old line.  On failure, keep the
        // old line so callers can still use line_slice etc.
        match self.source.next_line(peek_heredoc) {
            Ok(Some(new_line)) => {
                let b = new_line.peek_byte();
                let ascii = new_line.ascii_only;
                self.source.line = Some(new_line);
                self.effective_utf8 = self.utf8_mode && !ascii;
                b
            }
            Ok(None) => None,
            Err(e) => {
                self.pending_error = Some(e);
                None
            }
        }
    }

    /// Peek at a byte at an offset from the current position.  Does NOT auto-load — only valid within the current line.
    pub(crate) fn peek_byte_at(&self, offset: usize) -> Option<u8> {
        self.source.line.as_ref()?.peek_byte_at(offset)
    }

    /// Set the UTF-8 pragma mode and recompute `effective_utf8` based on the current line's `ascii_only` flag.  Called
    /// by the parser when processing `use utf8` / `no utf8` and when restoring pragma state at block boundaries.
    pub(crate) fn set_utf8_mode(&mut self, on: bool) {
        self.utf8_mode = on;
        self.effective_utf8 = on && self.source.line.as_ref().is_some_and(|l| !l.ascii_only);
    }

    /// Remaining bytes in the current line (not including synthetic \n).
    pub fn remaining(&self) -> &[u8] {
        match &self.source.line {
            Some(line) => line.remaining(),
            None => &[],
        }
    }

    // ── Position and span helpers ─────────────────────────────

    /// Current position within the current line (line-local).
    fn line_pos(&self) -> usize {
        self.source.line.as_ref().map_or(0, |l| l.pos)
    }

    /// Global position as u32 for span construction.
    fn span_pos(&self) -> u32 {
        match &self.source.line {
            Some(line) => line.global_pos(),
            None => self.source.cursor() as u32,
        }
    }

    /// Build a `Span` from a line-local start position to the current cursor position.  Both positions are on the
    /// current line.
    fn span_from(&self, local_start: usize) -> Span {
        match &self.source.line {
            Some(line) => Span::new((line.offset + local_start) as u32, line.global_pos()),
            None => {
                let pos = self.source.cursor() as u32;
                Span::new(pos, pos)
            }
        }
    }

    /// Advance the cursor by `n` bytes within the current line.
    fn skip(&mut self, n: usize) {
        if let Some(line) = self.source.line.as_mut() {
            line.pos += n;
        }
    }

    /// Rewind the cursor by `n` bytes within the current line.  The caller must ensure `n` does not exceed the current
    /// position.
    pub fn rewind(&mut self, n: usize) {
        if let Some(line) = self.source.line.as_mut() {
            line.pos -= n;
        }
    }

    /// Check for a `# line N "file"` directive and apply it.  Called when `#` is at column 0.  Updates the source's
    /// line number (and optionally filename) so that `__LINE__` and `__FILE__` reflect the override on subsequent
    /// lines.
    fn try_line_directive(&mut self) {
        // Clone the line's bytes (a cheap `Bytes` refcount bump) so the parse below borrows the clone, not
        // `self.source.line`.  This frees `self.source` for the `set_line_number` / `set_filename` mutations at the end
        // — otherwise the immutable borrow of `self.source.line` would still be live across them (the `rest` slices are
        // derived from it).
        let bytes = match &self.source.line {
            Some(line) => line.line.clone(),
            None => return,
        };
        let bytes = &bytes[..];

        // Pattern: `#` optional-spaces `line` spaces DIGITS optional-spaces optional("FILENAME")
        let rest = &bytes[1..]; // skip #
        let rest = Self::trim_ascii_start(rest);
        if !rest.starts_with(b"line") {
            return;
        }
        let rest = &rest[4..];
        // Must have whitespace after "line"
        if rest.is_empty() || !rest[0].is_ascii_whitespace() {
            return;
        }
        let rest = Self::trim_ascii_start(rest);

        // Parse digits
        let digit_end = rest.iter().position(|b| !b.is_ascii_digit()).unwrap_or(rest.len());
        if digit_end == 0 {
            return;
        }
        let line_num: usize = match std::str::from_utf8(&rest[..digit_end]) {
            Ok(s) => match s.parse() {
                Ok(n) => n,
                Err(_) => return,
            },
            Err(_) => return,
        };

        // Apply line number — the NEXT line will be this number.
        self.source.set_line_number(line_num);

        // Optional filename
        let rest = Self::trim_ascii_start(&rest[digit_end..]);
        if rest.starts_with(b"\"") {
            let rest = &rest[1..];
            if let Some(end_quote) = rest.iter().position(|&b| b == b'"')
                && let Ok(filename) = std::str::from_utf8(&rest[..end_quote])
            {
                self.source.set_filename(filename.to_string());
            }
        }
    }

    fn trim_ascii_start(bytes: &[u8]) -> &[u8] {
        let start = bytes.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(bytes.len());
        &bytes[start..]
    }

    /// Byte slice from line-local `start` to current cursor position.
    fn line_slice(&self, start: usize) -> &[u8] {
        match &self.source.line {
            Some(line) => &line.line[start..line.pos],
            None => &[],
        }
    }

    /// Like `line_slice` but returns `&str`.  Returns an error for non-UTF-8 source bytes (identifiers and numbers are
    /// always ASCII, so this only fails for truly malformed input).
    fn line_slice_str(&self, start: usize) -> Result<&str, ParseError> {
        let bytes = self.line_slice(start);
        std::str::from_utf8(bytes).map_err(|_| ParseError::new("invalid UTF-8 in source", self.span_from(start)))
    }

    /// Raw slice of the source buffer.  For rare operations that need global byte access (e.g. format body extraction).
    pub fn slice(&self, start: usize, end: usize) -> &[u8] {
        self.source.src_slice(start, end)
    }

    /// Byte-level scan for hash subscript autoquoting.  Called by the parser immediately after consuming `{` in a hash
    /// subscript context, before any tokenization of the subscript body.
    ///
    /// Matches the pattern: `[ \t]* [-]? [ \t]* IDENT [ \t]* }` on a SINGLE LINE (no newlines or comments).  If
    /// matched, consumes everything up to (but not including) `}` and returns `Some((name, span))` where `name` is the
    /// autoquoted string (e.g. `"foo"` or `"-foo"`).  If the pattern doesn't match, the lexer position is unchanged and
    /// `None` is returned, signaling the parser to fall through to normal expression parsing.
    ///
    /// This handles all hash subscript autoquoting in one place: `$h{foo}`, `$h{-foo}`, `$h{if}`, `$h{-if}`,
    /// `$h{__FILE__}`, `$h{-__END__}`, `$h{q}`, etc.  Keywords and special tokens are treated as plain identifiers at
    /// the byte level — the parser doesn't need to intercept them individually.
    pub fn try_autoquoted_subscript_key(&mut self) -> Option<(String, Span)> {
        let line = self.source.line.as_ref()?;
        let r = line.remaining();
        let mut i = 0;

        // Skip leading horizontal whitespace.
        while i < r.len() && matches!(r[i], b' ' | b'\t') {
            i += 1;
        }

        // Optional `-` prefix.
        let has_dash = r.get(i).copied() == Some(b'-');
        if has_dash {
            i += 1;
            // Skip whitespace between `-` and identifier.
            while i < r.len() && matches!(r[i], b' ' | b'\t') {
                i += 1;
            }
        }

        // Identifier: must start with [a-zA-Z_] (or UTF-8 lead byte in UTF-8 mode).
        let ident_start = i;
        let &first = r.get(i)?;
        if first == b'_' || first.is_ascii_alphabetic() {
            i += 1;
        } else if self.effective_utf8 && first >= 0x80 {
            let tail = &r[i..];
            let s = std::str::from_utf8(tail).ok()?;
            let ch = s.chars().next()?;
            if !ch.is_alphabetic() {
                return None;
            }
            i += ch.len_utf8();
        } else {
            return None;
        }

        // Identifier body: [a-zA-Z0-9_] (or UTF-8 continuation).
        while i < r.len() {
            let b = r[i];
            if b == b'_' || b.is_ascii_alphanumeric() {
                i += 1;
            } else if self.effective_utf8 && b >= 0x80 {
                let tail = &r[i..];
                if let Ok(s) = std::str::from_utf8(tail)
                    && let Some(ch) = s.chars().next()
                    && ch.is_alphanumeric()
                {
                    i += ch.len_utf8();
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        let ident_end = i;

        // Must have scanned at least one identifier character.
        if ident_end == ident_start {
            return None;
        }

        // Skip trailing horizontal whitespace.
        while i < r.len() && matches!(r[i], b' ' | b'\t') {
            i += 1;
        }

        // Must be followed by `}` on the same line.
        if r.get(i).copied() != Some(b'}') {
            return None;
        }

        // Build the autoquoted string.
        let ident_bytes = &r[ident_start..ident_end];
        let ident = std::str::from_utf8(ident_bytes).ok()?;
        let name = if has_dash { format!("-{ident}") } else { ident.to_string() };

        // Compute span and consume.  The span covers from the first meaningful byte (dash or ident start) through the
        // end of the identifier.  Leave `}` unconsumed for the parser to match.
        let offset = line.offset;
        let pos = line.pos;
        let span_start = if has_dash { pos } else { pos + ident_start };
        let start_global = (offset + span_start) as u32;
        let end_global = (offset + pos + ident_end) as u32;

        // Advance the lexer past the identifier and trailing whitespace (up to but not including `}`).
        let mline = self.source.line.as_mut()?;
        mline.pos += i;

        Some((name, Span::new(start_global, end_global)))
    }

    // ── Format sublexing ──────────────────────────────────────
    //
    // Format bodies are line-oriented: each line is either a comment (`#` in column 0), blank, a literal line (no field
    // specifiers), or a picture line (one or more `@`/`^` fields) followed by an argument line (expressions to fill the
    // fields).  The body is terminated by a line containing only `.` (optionally followed by whitespace or `\r`).
    //
    // Tokens are pre-tokenized by line when the line is read, and drained from a queue on subsequent `lex_token` calls.
    // This lets us classify a line once and emit a clean stream.

    /// Enter format-body sublexing.  Called by the parser after it has consumed `format [NAME] =`.  The first token
    /// returned by the next `lex_token` call will be `FormatSublexBegin`.
    ///
    /// `name` is the format name (empty string defaults to STDOUT at the parser level before this is called).
    /// `begin_span` is the span of the `format` keyword through the `=`.
    pub fn start_format(&mut self, name: String, begin_span: Span) {
        // Drop the rest of the `=` line — the format body starts
        // on the next source line.
        self.source.line = None;
        let mut queue = VecDeque::new();
        queue.push_back(Spanned { token: Token::FormatSublexBegin(name), span: begin_span });
        self.format_state = Some(FormatState { queue, mode: FormatMode::Body });
    }

    /// Called by the parser when it consumes `{` as the first token of an argument line, to switch from line-terminated
    /// to brace-matched argument mode.  Must be called while the lexer is in `ArgsLine` mode.
    pub fn format_args_enter_braced(&mut self) {
        if let Some(state) = &mut self.format_state {
            state.mode = FormatMode::ArgsBraced { depth: 1 };
        }
    }

    /// Lex the next token while in format sublex mode.
    fn lex_format_token(&mut self) -> Result<Spanned, ParseError> {
        // Drain queue first.
        if let Some(state) = &mut self.format_state
            && let Some(tok) = state.queue.pop_front()
        {
            // If the drained token was SublexEnd (Finished mode), tear down format state so subsequent calls are
            // normal.
            if matches!(tok.token, Token::SublexEnd) && matches!(state.mode, FormatMode::Finished) {
                self.format_state = None;
            }
            return Ok(tok);
        }

        // Queue is empty — decide what to do based on mode.
        let mode = match &self.format_state {
            Some(s) => s.mode,
            None => unreachable!("lex_format_token called with no format state"),
        };
        match mode {
            FormatMode::Body => self.format_read_line(),
            FormatMode::PendingArgsBegin => {
                let pos = self.span_pos();
                let span = Span::new(pos, pos);
                if let Some(state) = &mut self.format_state {
                    state.mode = FormatMode::ArgsLine;
                }
                Ok(Spanned { token: Token::FormatArgsBegin, span })
            }
            FormatMode::ArgsLine => self.format_lex_args_line(),
            FormatMode::ArgsBraced { .. } => self.format_lex_args_braced(),
            FormatMode::Finished => unreachable!("Finished mode with empty queue"),
        }
    }

    /// Read the next source line and classify it, enqueuing the appropriate tokens.
    fn format_read_line(&mut self) -> Result<Spanned, ParseError> {
        // Ensure any in-progress line is dropped; we read raw lines.
        self.source.line = None;
        let line = match self.source.next_line(false) {
            Ok(Some(l)) => l,
            Ok(None) | Err(_) => {
                // EOF inside a format — emit SublexEnd and finish.
                let pos = self.span_pos();
                let span = Span::new(pos, pos);
                if let Some(state) = &mut self.format_state {
                    state.mode = FormatMode::Finished;
                }
                self.format_state = None; // tear down immediately
                return Ok(Spanned { token: Token::SublexEnd, span });
            }
        };
        let offset = line.offset;
        let bytes = line.line.clone();
        // Drop the consumed line from source-tracking state.
        self.source.line = None;

        let offset_u32 = offset as u32;
        let line_end_u32 = (offset + bytes.len()) as u32;

        // Classify: terminator, comment, blank, or picture.
        if is_format_terminator(&bytes) {
            let span = Span::new(offset_u32, line_end_u32);
            if let Some(state) = &mut self.format_state {
                state.mode = FormatMode::Finished;
            }
            self.format_state = None;
            return Ok(Spanned { token: Token::SublexEnd, span });
        }

        if bytes.first() == Some(&b'#') {
            // Comment line — strip leading `#` and trailing newline/CR.
            let text_bytes = strip_line_ending(&bytes[1..]);
            let text = String::from_utf8_lossy(text_bytes).into_owned();
            let span = Span::new(offset_u32, line_end_u32);
            return Ok(Spanned { token: Token::FormatComment(text), span });
        }

        let stripped = strip_line_ending(&bytes);
        if stripped.iter().all(|&b| b == b' ' || b == b'\t') {
            let span = Span::new(offset_u32, line_end_u32);
            return Ok(Spanned { token: Token::FormatBlankLine, span });
        }

        // Tokenize as a picture/literal line.
        self.format_tokenize_picture_line(offset, &bytes, stripped)
    }

    /// Tokenize one non-comment non-blank non-terminator line.  `offset` is the byte offset of the start of the line in
    /// the source; `raw_bytes` is the full line including line ending; `content` is the same with the line ending
    /// stripped.
    fn format_tokenize_picture_line(&mut self, offset: usize, raw_bytes: &[u8], content: &[u8]) -> Result<Spanned, ParseError> {
        // Determine RepeatKind by counting tildes, then replace all `~` with spaces (they don't belong to fields).
        let repeat = classify_repeat(content);
        let normalized: Vec<u8> = content.iter().map(|&b| if b == b'~' { b' ' } else { b }).collect();

        let offset_u32 = offset as u32;
        let raw_end_u32 = (offset + raw_bytes.len()) as u32;

        // Scan for fields.  We walk byte-by-byte, collecting literal runs interspersed with fields.
        let mut parts: Vec<(Token, Span)> = Vec::new();
        let mut i = 0;
        let mut literal_start = 0;
        let mut has_fields = false;
        while i < normalized.len() {
            let b = normalized[i];
            if b == b'@' || b == b'^' {
                // Try to parse a field starting here.
                if let Some((kind, consumed)) = parse_field(&normalized, i) {
                    // Flush any pending literal.
                    if literal_start < i {
                        let lit: String = String::from_utf8_lossy(&normalized[literal_start..i]).into_owned();
                        let span = Span::new((offset + literal_start) as u32, (offset + i) as u32);
                        parts.push((Token::FormatLiteral(lit), span));
                    }
                    let field_span = Span::new((offset + i) as u32, (offset + i + consumed) as u32);
                    parts.push((Token::FormatField(kind), field_span));
                    has_fields = true;
                    i += consumed;
                    literal_start = i;
                    continue;
                }
                // `@` or `^` not followed by valid pad chars: pass through as literal text.
            }
            i += 1;
        }
        // Trailing literal run.
        if literal_start < normalized.len() {
            let lit: String = String::from_utf8_lossy(&normalized[literal_start..]).into_owned();
            let span = Span::new((offset + literal_start) as u32, (offset + normalized.len()) as u32);
            parts.push((Token::FormatLiteral(lit), span));
        }

        let line_span = Span::new(offset_u32, raw_end_u32);

        if !has_fields {
            // No fields — emit a single FormatLiteralLine.
            let text: String = String::from_utf8_lossy(&normalized).into_owned();
            return Ok(Spanned { token: Token::FormatLiteralLine(repeat, text), span: line_span });
        }

        // Has fields.  Emit PictureBegin, all parts, PictureEnd; then set mode so PendingArgsBegin fires next.
        let first = Spanned { token: Token::FormatPictureBegin(repeat), span: line_span };
        if let Some(state) = &mut self.format_state {
            for (tok, span) in parts {
                state.queue.push_back(Spanned { token: tok, span });
            }
            let end_pos = (offset + content.len()) as u32;
            let end_span = Span::new(end_pos, end_pos);
            state.queue.push_back(Spanned { token: Token::FormatPictureEnd, span: end_span });
            state.mode = FormatMode::PendingArgsBegin;
        }
        Ok(first)
    }

    /// Lex a token in argument-line mode.  Returns `FormatArgsEnd` when the current line ends; otherwise delegates to
    /// normal tokenization.
    fn format_lex_args_line(&mut self) -> Result<Spanned, ParseError> {
        // Skip in-line whitespace but NOT newlines — a newline terminates this args line.
        self.format_skip_inline_ws();
        // If at end of current source line, or at EOF, end args.
        if self.peek_byte(false).is_none_or(|b| b == b'\n') {
            let pos_start = self.span_pos();
            // Consume the newline (if any) so we're positioned on the next line for further format scanning.
            if self.peek_byte(false) == Some(b'\n') {
                self.skip(1);
            }
            if let Some(state) = &mut self.format_state {
                state.mode = FormatMode::Body;
            }
            let pos_end = self.span_pos();
            return Ok(Spanned { token: Token::FormatArgsEnd, span: Span::new(pos_start, pos_end) });
        }
        // Normal code token.
        self.lex_normal_token()
    }

    /// Lex a token in braced argument mode.  Tracks `{`/`}` depth and emits `FormatArgsEnd` when a `}` brings depth to
    /// 0 (swallowing that `}`).
    fn format_lex_args_braced(&mut self) -> Result<Spanned, ParseError> {
        // In braced mode, newlines inside the expression are just whitespace, so ordinary tokenization (which skips all
        // ws) is correct.  We just need to intercept the `}` that closes the args.
        let tok = self.lex_normal_token()?;
        match &tok.token {
            Token::LeftBrace => {
                if let Some(FormatState { mode: FormatMode::ArgsBraced { depth }, .. }) = &mut self.format_state {
                    *depth += 1;
                }
                Ok(tok)
            }
            Token::RightBrace => {
                if let Some(state) = &mut self.format_state
                    && let FormatMode::ArgsBraced { depth } = &mut state.mode
                {
                    *depth -= 1;
                    if *depth == 0 {
                        // Closing `}` — swallow and emit FormatArgsEnd.
                        state.mode = FormatMode::Body;
                        return Ok(Spanned { token: Token::FormatArgsEnd, span: tok.span });
                    }
                }
                Ok(tok)
            }
            _ => Ok(tok),
        }
    }

    /// Skip space/tab characters but not newlines.
    fn format_skip_inline_ws(&mut self) {
        while let Some(b) = self.peek_byte(false) {
            if b == b' ' || b == b'\t' {
                self.skip(1);
            } else {
                break;
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
                    // Check for `# line N "file"` directive at column 0.
                    if self.line_pos() == 0 {
                        self.try_line_directive();
                    }
                    // Comment — drop entire line.
                    self.source.line = None;
                }
                Some(b'=') if self.peek_byte_at(1).is_some_and(|b| b.is_ascii_alphabetic()) && self.line_pos() == 0 => {
                    self.skip_pod()?;
                }
                _ => break, // Non-whitespace byte or EOF
            }
        }
        Ok(())
    }

    /// Skip whitespace and `#` comments only — **not** POD.  For use when the lexer is inside a quote-operator's
    /// delimiter-finding scan: per Perl, POD is suspended until the delimiter is found, so
    ///
    /// ```perl
    /// $_ = qq
    ///
    /// =pod
    ///
    /// testing
    ///
    /// =;
    /// ```
    ///
    /// is a qq-string with body `"pod\n\ntesting\n\n"`, not a pod block.  `=pod` at column 0 would start a pod block
    /// in normal code context, but once we've committed to a quote op waiting for its delimiter, the `=` is just a
    /// candidate delimiter byte.
    fn skip_ws_and_comments_no_pod(&mut self) -> Result<(), ParseError> {
        loop {
            match self.peek_byte(false) {
                Some(b' ') | Some(b'\t') | Some(b'\n') => self.skip(1),
                Some(b'#') => {
                    // Check for `# line N "file"` directive at column 0, same as skip_ws_and_comments.
                    if self.line_pos() == 0 {
                        self.try_line_directive();
                    }
                    self.source.line = None;
                }
                _ => break,
            }
        }
        Ok(())
    }

    /// Skip a pod block: everything from `=word` to `=cut\n`.  Matches Perl 5's behavior: `=cut` must be at start of
    /// line, followed by a non-alphabetic character (or EOF).
    fn skip_pod(&mut self) -> Result<(), ParseError> {
        // Skip the current =word line.
        self.source.line = None;
        // Read lines until =cut at start of line.
        loop {
            // peek_byte auto-loads the next line.
            if self.peek_byte(false).is_none() {
                break; // EOF inside pod — not an error per Perl
            }
            let is_cut =
                self.source.line.as_ref().is_some_and(|line| line.line.starts_with(b"=cut") && !line.line.get(4).is_some_and(|b| b.is_ascii_alphabetic()));
            self.source.line = None; // skip this line
            if is_cut {
                break;
            }
        }
        Ok(())
    }

    // ── Main tokenization entry point ─────────────────────────

    /// Lex the next token.  When inside a sublexing context (interpolating string, etc.), dispatches to the appropriate
    /// sub-lexer instead.
    pub fn lex_token(&mut self) -> Result<Spanned, ParseError> {
        // Surface any deferred error from auto-loading in peek_byte.
        if let Some(e) = self.pending_error.take() {
            return Err(e);
        }

        // Format sublex takes priority over the LexContext stack — format state is orthogonal (line-oriented, not
        // delimiter-oriented) and `context_stack` is unused during format mode.
        if self.format_state.is_some() {
            return self.lex_format_token();
        }

        // If inside a sublexing context, dispatch there.
        match self.context_stack.last() {
            // Chain-end bookkeeping runs BEFORE chain_active / expr_depth dispatch: when the previous call finished the
            // last subscript, we leave a marker and emit `InterpChainEnd` here before switching back to body-scanning
            // mode.
            Some(ctx) if ctx.chain_end_pending => {
                let span_pos = self.span_pos();
                if let Some(ctx) = self.context_stack.last_mut() {
                    ctx.chain_end_pending = false;
                    ctx.chain_active = false;
                    ctx.chain_depth = 0;
                }
                return Ok(Spanned { token: Token::InterpChainEnd, span: Span::new(span_pos, span_pos) });
            }
            Some(ctx) if ctx.chain_active => {
                // Subscript chain — code-mode lexing with bracket/brace tracking.  When a closing bracket drops
                // chain_depth to 0, probe for a continuer; if none, mark chain_end_pending so the next call emits
                // InterpChainEnd.
                let result = self.lex_normal_token()?;
                let closed_to_zero = match &result.token {
                    Token::LeftBrace | Token::LeftBracket => {
                        if let Some(ctx) = self.context_stack.last_mut() {
                            ctx.chain_depth += 1;
                        }
                        false
                    }
                    Token::RightBrace | Token::RightBracket => {
                        if let Some(ctx) = self.context_stack.last_mut() {
                            // Saturating in case of malformed input — we'd rather bail to body mode than underflow.
                            ctx.chain_depth = ctx.chain_depth.saturating_sub(1);
                            ctx.chain_depth == 0
                        } else {
                            false
                        }
                    }
                    // Postderef forms `->@*`, `->%*`, `->$*`, `->&*` end with `Star`; `->**` ends with `Power`.  At
                    // depth 0 these complete the postderef — probe for continuation just like a closing bracket would.
                    Token::Star | Token::Power => self.context_stack.last().is_some_and(|ctx| ctx.chain_depth == 0),
                    _ => false,
                };
                if closed_to_zero {
                    let cont = self.peek_chain_starter();
                    if let Some(ctx) = self.context_stack.last_mut() {
                        ctx.chain_end_pending = !cont;
                    }
                }
                return Ok(result);
            }
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
        if self.logical_eof {
            let start = self.span_pos();
            return Ok(Spanned { token: Token::Eof, span: Span::new(start, start) });
        }

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
                self.context_stack.push(LexContext::new(Some('"'), true, false, false));
                Token::QuoteSublexBegin(QuoteKind::Double, '"')
            }
            b'`' => {
                self.skip(1); // skip opening `
                self.context_stack.push(LexContext::new(Some('`'), true, false, false));
                Token::QuoteSublexBegin(QuoteKind::Backtick, '`')
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
                if self.peek_byte(false) == Some(b'^') {
                    self.skip(1);
                    if self.peek_byte(false) == Some(b'=') {
                        self.skip(1);
                        Token::Assign(AssignOp::LogicalXorEq)
                    } else {
                        Token::LogicalXor
                    }
                } else if self.peek_byte(false) == Some(b'.') && self.features.contains(Features::BITWISE) {
                    self.skip(1);
                    if self.peek_byte(false) == Some(b'=') {
                        self.skip(1);
                        Token::Assign(AssignOp::StringBitXorEq)
                    } else {
                        Token::StringBitXor
                    }
                } else if self.peek_byte(false) == Some(b'=') {
                    self.skip(1);
                    Token::Assign(AssignOp::BitXorEq)
                } else {
                    Token::BitXor
                }
            }
            b'~' => {
                self.skip(1);
                if self.peek_byte(false) == Some(b'~') {
                    self.skip(1);
                    Token::SmartMatch
                } else if self.peek_byte(false) == Some(b'.') && self.features.contains(Features::BITWISE) {
                    self.skip(1);
                    Token::StringBitNot
                } else {
                    Token::Tilde
                }
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

            // ^D (0x04) and ^Z (0x1a) — logical end of script.  Unlike __DATA__/__END__, these don't set up a DATA
            // filehandle; they just trigger EOF.
            b'\x04' | b'\x1a' => {
                self.skip(1);
                self.logical_eof = true;
                Token::Eof
            }

            other => {
                if other >= 0x80 {
                    if self.utf8_mode {
                        // UTF-8 lead byte — check if it decodes to an XID_Start character before routing to the
                        // identifier path.
                        if let Some((ch, _)) = self.peek_utf8_char() {
                            if UnicodeXID::is_xid_start(ch) {
                                self.lex_word()?
                            } else {
                                let len = ch.len_utf8();
                                self.skip(len);
                                return Err(ParseError::new(format!("Unrecognized character U+{:04X}", ch as u32), Span::new(start, self.span_pos())));
                            }
                        } else {
                            // Invalid UTF-8 sequence.
                            self.skip(1);
                            return Err(ParseError::new(format!("Invalid UTF-8 byte \\x{other:02X}"), Span::new(start, self.span_pos())));
                        }
                    } else {
                        self.skip(1);
                        return Err(ParseError::new(format!("Unrecognized character \\x{other:02X}"), Span::new(start, self.span_pos())));
                    }
                } else {
                    self.skip(1);
                    return Err(ParseError::new(format!("unexpected byte 0x{other:02x} ('{}')", other as char), Span::new(start, self.span_pos())));
                }
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

        if self.peek_byte(false) == Some(b'.') && self.peek_byte_at(1).is_some_and(|b| b.is_ascii_digit() || b == b'_') {
            // Before committing to float: check if this is a v-string without the `v` prefix.  If there are 2+ dots
            // (e.g. 102.111.111), it's a v-string per perldata.
            let saved_pos = self.line_pos();
            self.skip(1); // skip first '.'
            self.scan_digits();
            if self.peek_byte(false) == Some(b'.') && self.peek_byte_at(1).is_some_and(|b| b.is_ascii_digit()) {
                // Two dots — v-string without v prefix.  Collect the rest: .digits(.digits)*
                let mut vstr = self.line_slice_str(start)?.to_string();
                while self.peek_byte(false) == Some(b'.') && self.peek_byte_at(1).is_some_and(|b| b.is_ascii_digit()) {
                    vstr.push('.');
                    self.skip(1);
                    let seg_start = self.line_pos();
                    while self.peek_byte(false).is_some_and(|b| b.is_ascii_digit()) {
                        self.skip(1);
                    }
                    vstr.push_str(self.line_slice_str(seg_start)?);
                }
                return Ok(Token::VersionLit(vstr));
            }
            // Not a v-string — rewind and parse as float.
            if let Some(line) = self.source.line.as_mut() {
                line.pos = saved_pos;
            }

            // Float
            self.skip(1); // skip '.'
            let frac_start = self.line_pos();
            self.scan_digits();

            // Legacy octal float: 07.65p2 — starts with 0, has p exponent.
            if (self.peek_byte(false) == Some(b'p') || self.peek_byte(false) == Some(b'P')) && self.line_slice_str(start)?.starts_with('0') {
                let int_s = self.line_slice_str(start)?.split('.').next().unwrap_or("0").replace('_', "");
                let frac_s = self.line_slice_str(frac_start)?.replace('_', "");
                let exp = self.scan_p_exponent();
                let int_val = u64::from_str_radix(&int_s, 8).unwrap_or(0) as f64;
                let frac_val = if frac_s.is_empty() { 0.0 } else { u64::from_str_radix(&frac_s, 8).unwrap_or(0) as f64 / 8f64.powi(frac_s.len() as i32) };
                return Ok(Token::FloatLit((int_val + frac_val) * 2f64.powi(exp)));
            }

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

    /// Scan a `p`/`P` power-of-2 exponent for hex/octal/binary floats.  Assumes the cursor is on `p` or `P`.  Returns
    /// the signed exponent.
    fn scan_p_exponent(&mut self) -> i32 {
        self.skip(1); // skip p/P
        let neg = if self.peek_byte(false) == Some(b'-') {
            self.skip(1);
            true
        } else {
            if self.peek_byte(false) == Some(b'+') {
                self.skip(1);
            }
            false
        };
        let exp_start = self.line_pos();
        while self.peek_byte(false).is_some_and(|b| b.is_ascii_digit()) {
            self.skip(1);
        }
        let exp_s = std::str::from_utf8(self.line_slice(exp_start)).unwrap_or("0");
        let exp: i32 = exp_s.parse().unwrap_or(0);
        if neg { -exp } else { exp }
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
        let int_str = self.line_slice_str(hex_start)?.replace('_', "");

        // Check for hex float: 0xHH.HHpEE
        if self.peek_byte(false) == Some(b'.') && self.peek_byte_at(1).is_some_and(|b| b.is_ascii_hexdigit()) {
            self.skip(1); // skip '.'
            let frac_start = self.line_pos();
            while let Some(b) = self.peek_byte(false) {
                if b.is_ascii_hexdigit() || b == b'_' {
                    self.skip(1);
                } else {
                    break;
                }
            }
            let frac_str = self.line_slice_str(frac_start)?.replace('_', "");

            // 'p' or 'P' exponent is required for hex float
            if self.peek_byte(false) != Some(b'p') && self.peek_byte(false) != Some(b'P') {
                return Err(ParseError::new("hex float requires 'p' exponent", self.span_from(start)));
            }
            let exp = self.scan_p_exponent();

            let int_val = u64::from_str_radix(&int_str, 16).unwrap_or(0) as f64;
            let frac_val = if frac_str.is_empty() { 0.0 } else { u64::from_str_radix(&frac_str, 16).unwrap_or(0) as f64 / 16f64.powi(frac_str.len() as i32) };
            let val = (int_val + frac_val) * 2f64.powi(exp);
            return Ok(Token::FloatLit(val));
        }

        // Check for hex float without fraction: 0xHHpEE
        if self.peek_byte(false) == Some(b'p') || self.peek_byte(false) == Some(b'P') {
            let exp = self.scan_p_exponent();
            let int_val = u64::from_str_radix(&int_str, 16).unwrap_or(0) as f64;
            let val = int_val * 2f64.powi(exp);
            return Ok(Token::FloatLit(val));
        }

        let n = i64::from_str_radix(&int_str, 16).map_err(|_| ParseError::new("invalid hex literal", self.span_from(start)))?;
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
        let bin_str = self.line_slice_str(bin_start)?.replace('_', "");

        // Check for binary float: 0b101.01p-1
        if self.peek_byte(false) == Some(b'.') && self.peek_byte_at(1).is_some_and(|b| b == b'0' || b == b'1') {
            self.skip(1);
            let frac_start = self.line_pos();
            while self.peek_byte(false).is_some_and(|b| b == b'0' || b == b'1' || b == b'_') {
                self.skip(1);
            }
            let frac_str = self.line_slice_str(frac_start)?.replace('_', "");
            if self.peek_byte(false) != Some(b'p') && self.peek_byte(false) != Some(b'P') {
                return Err(ParseError::new("binary float requires 'p' exponent", self.span_from(start)));
            }
            let exp = self.scan_p_exponent();
            let int_val = u64::from_str_radix(&bin_str, 2).unwrap_or(0) as f64;
            let frac_val = if frac_str.is_empty() { 0.0 } else { u64::from_str_radix(&frac_str, 2).unwrap_or(0) as f64 / 2f64.powi(frac_str.len() as i32) };
            return Ok(Token::FloatLit((int_val + frac_val) * 2f64.powi(exp)));
        }
        if self.peek_byte(false) == Some(b'p') || self.peek_byte(false) == Some(b'P') {
            let exp = self.scan_p_exponent();
            let int_val = u64::from_str_radix(&bin_str, 2).unwrap_or(0) as f64;
            return Ok(Token::FloatLit(int_val * 2f64.powi(exp)));
        }

        let n = i64::from_str_radix(&bin_str, 2).map_err(|_| ParseError::new("invalid binary literal", self.span_from(start)))?;
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
        let oct_str = self.line_slice_str(oct_start)?.replace('_', "");

        // Check for octal float: 0o7.65p2
        if self.peek_byte(false) == Some(b'.') && self.peek_byte_at(1).is_some_and(|b| (b'0'..=b'7').contains(&b)) {
            self.skip(1);
            let frac_start = self.line_pos();
            while self.peek_byte(false).is_some_and(|b| (b'0'..=b'7').contains(&b) || b == b'_') {
                self.skip(1);
            }
            let frac_str = self.line_slice_str(frac_start)?.replace('_', "");
            if self.peek_byte(false) != Some(b'p') && self.peek_byte(false) != Some(b'P') {
                return Err(ParseError::new("octal float requires 'p' exponent", self.span_from(start)));
            }
            let exp = self.scan_p_exponent();
            let int_val = u64::from_str_radix(&oct_str, 8).unwrap_or(0) as f64;
            let frac_val = if frac_str.is_empty() { 0.0 } else { u64::from_str_radix(&frac_str, 8).unwrap_or(0) as f64 / 8f64.powi(frac_str.len() as i32) };
            return Ok(Token::FloatLit((int_val + frac_val) * 2f64.powi(exp)));
        }
        if self.peek_byte(false) == Some(b'p') || self.peek_byte(false) == Some(b'P') {
            let exp = self.scan_p_exponent();
            let int_val = u64::from_str_radix(&oct_str, 8).unwrap_or(0) as f64;
            return Ok(Token::FloatLit(int_val * 2f64.powi(exp)));
        }

        let n = i64::from_str_radix(&oct_str, 8).map_err(|_| ParseError::new("invalid octal literal", self.span_from(start)))?;
        Ok(Token::IntLit(n))
    }

    // ── Variables ($, @, %) ───────────────────────────────────

    fn lex_dollar(&mut self) -> Result<Token, ParseError> {
        self.skip(1); // skip $

        // Per perldata: "It is legal, but not recommended, to separate a variable's sigil from its name by space and/or
        // tab characters."  Only whitespace triggers this — `$#` is ArrayLen, not a comment.  Once inside the skip,
        // skip_ws_and_comments_no_pod handles any comments encountered along the way (e.g. `$  # comment\n x`).
        if matches!(self.peek_byte(false), Some(b' ' | b'\t' | b'\n')) {
            self.skip_ws_and_comments_no_pod()?;
            if self.peek_byte(false).is_some_and(|b| b == b'_' || b.is_ascii_alphabetic() || (self.effective_utf8 && b >= 0x80)) {
                let name = self.scan_ident();
                if !name.is_empty() {
                    return Ok(Token::ScalarVar(name));
                }
            }
            return Ok(Token::Dollar);
        }

        // $# — array length
        let after_hash = self.peek_byte_at(1);
        if self.peek_byte(false) == Some(b'#') && after_hash.is_some_and(|b| b == b'_' || b.is_ascii_alphabetic() || (self.effective_utf8 && b >= 0x80)) {
            self.skip(1); // skip #
            let name = self.scan_ident();
            if !name.is_empty() {
                return Ok(Token::ArrayLen(name));
            }
            // Not a valid identifier — rewind past the #.
            self.rewind(1);
        }

        // Special variables: $$, $!, $@, $_, $0-$9, $/, $\, etc.
        match self.peek_byte(false) {
            Some(b'_') => {
                // Could be $_ or $_[...] or $_ident or $_ünïcödé
                if self.peek_byte_at(1).is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || (self.effective_utf8 && b >= 0x80)) {
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
            Some(b) if self.effective_utf8 && b >= 0x80 => {
                // UTF-8 lead byte — attempt to scan a Unicode identifier.
                let name = self.scan_ident();
                if !name.is_empty() {
                    return Ok(Token::ScalarVar(name));
                }
                // Not a valid identifier — fall through to Dollar.
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
                // $$name is scalar dereference; $$ alone is PID.  Return Dollar (deref prefix) if the byte after the
                // second $ could start any variable expression ($name, ${expr}, $0, $$nested, $!, etc.).  Only return
                // SpecialVar("$") (PID) when nothing variable-like follows.
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
                // $^X — caret variable.  Per perlvar, the character after ^ can be any of [][A-Z^_?\a-z].
                if let Some(next) = self.peek_byte_at(1)
                    && (next.is_ascii_alphabetic() || next == b'[' || next == b']' || next == b'^' || next == b'_' || next == b'?' || next == b'\\')
                {
                    self.skip(2); // skip ^ and the character
                    let name = format!("^{}", next as char);
                    return Ok(Token::SpecialVar(name));
                }
                // Bare $^ — format_top_name.
                self.skip(1);
                return Ok(Token::SpecialVar("^".into()));
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
                // $'ident → $::ident when apostrophe_as_package_separator is enabled and an identifier-start follows.
                if self.features.contains(Features::APOSTROPHE_AS_PACKAGE_SEPARATOR)
                    && self.peek_byte_at(1).is_some_and(|next| next == b'_' || next.is_ascii_alphabetic() || (self.effective_utf8 && next >= 0x80))
                {
                    self.skip(1); // consume the apostrophe
                    let rest = self.scan_ident();
                    return Ok(Token::ScalarVar(format!("::{rest}")));
                }
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
            Some(b'[') => {
                // $[ — array base (deprecated, always 0 in modern Perl)
                self.skip(1);
                return Ok(Token::SpecialVar("[".into()));
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

        // Whitespace between sigil and name.
        if matches!(self.peek_byte(false), Some(b' ' | b'\t' | b'\n')) {
            self.skip_ws_and_comments_no_pod()?;
            if self.peek_byte(false).is_some_and(|b| b == b'_' || b.is_ascii_alphabetic() || (self.effective_utf8 && b >= 0x80)) {
                let name = self.scan_ident();
                if !name.is_empty() {
                    return Ok(Token::ArrayVar(name));
                }
            }
            return Ok(Token::At);
        }

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
            Some(b) if self.effective_utf8 && b >= 0x80 => {
                let name = self.scan_ident();
                if !name.is_empty() { Ok(Token::ArrayVar(name)) } else { Ok(Token::At) }
            }
            // @'ident — apostrophe as package separator.
            Some(b'\'')
                if self.features.contains(Features::APOSTROPHE_AS_PACKAGE_SEPARATOR)
                    && self.peek_byte_at(1).is_some_and(|next| next == b'_' || next.is_ascii_alphabetic() || (self.effective_utf8 && next >= 0x80)) =>
            {
                self.skip(1); // consume apostrophe
                let rest = self.scan_ident();
                Ok(Token::ArrayVar(format!("::{rest}")))
            }
            _ => Ok(Token::At),
        }
    }

    fn lex_percent(&mut self) -> Result<Token, ParseError> {
        // Always return Percent or ModEq.  The parser, in term position, calls lex_hash_var_after_percent to attempt
        // hash-variable detection.
        self.skip(1);
        if self.peek_byte(false) == Some(b'=') {
            self.skip(1);
            Ok(Token::Assign(AssignOp::ModEq))
        } else {
            Ok(Token::Percent)
        }
    }

    /// Called by the parser after consuming a `Percent` token in term position.  Attempts to read a hash variable name
    /// and returns the appropriate token, or `None` if `%` is not followed by a valid hash name (in which case `%` is
    /// an invalid standalone term).
    pub fn lex_hash_var_after_percent(&mut self) -> Result<Option<Token>, ParseError> {
        // Whitespace between sigil and name.
        if matches!(self.peek_byte(false), Some(b' ' | b'\t' | b'\n')) {
            self.skip_ws_and_comments_no_pod()?;
            if self.peek_byte(false).is_some_and(|b| b == b'_' || b.is_ascii_alphabetic() || (self.effective_utf8 && b >= 0x80)) {
                let name = self.scan_ident();
                if !name.is_empty() {
                    return Ok(Some(Token::HashVar(name)));
                }
            }
            return Ok(None);
        }

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
            Some(b'^') => {
                // %^H — caret hash variable (single char after ^).
                if let Some(next) = self.peek_byte_at(1)
                    && (next.is_ascii_alphabetic() || next == b'[' || next == b']' || next == b'^' || next == b'_' || next == b'?' || next == b'\\')
                {
                    self.skip(2);
                    let name = format!("^{}", next as char);
                    Ok(Some(Token::SpecialHashVar(name)))
                } else {
                    Ok(None)
                }
            }
            Some(b) if b == b'_' || b.is_ascii_alphabetic() => {
                let name = self.scan_ident();
                Ok(Some(Token::HashVar(name)))
            }
            Some(b) if self.effective_utf8 && b >= 0x80 => {
                let name = self.scan_ident();
                if !name.is_empty() { Ok(Some(Token::HashVar(name))) } else { Ok(None) }
            }
            // %'ident — apostrophe as package separator.
            Some(b'\'')
                if self.features.contains(Features::APOSTROPHE_AS_PACKAGE_SEPARATOR)
                    && self.peek_byte_at(1).is_some_and(|next| next == b'_' || next.is_ascii_alphabetic() || (self.effective_utf8 && next >= 0x80)) =>
            {
                self.skip(1); // consume apostrophe
                let rest = self.scan_ident();
                Ok(Some(Token::HashVar(format!("::{rest}"))))
            }
            _ => Ok(None),
        }
    }

    /// Called by the parser after consuming a `->` then `$` when probing for the postderef last-index form `->$#*`.
    ///
    /// Checks whether the next two raw bytes are `#*`.  If so, consumes both and returns `true`; otherwise returns
    /// `false` and leaves the cursor unchanged.
    ///
    /// Needed because the lexer would otherwise tokenize `#` as the start of a comment, eating the rest of the line and
    /// losing the trailing `*`.  The parser calls this before asking for the next token so the byte-level
    /// disambiguation happens outside the normal tokenization path.
    pub fn try_consume_hash_star(&mut self) -> bool {
        let r = self.remaining();
        if r.len() >= 2 && r[0] == b'#' && r[1] == b'*' {
            self.skip(2);
            // When inside a subscript-chain in a string body, the `#*` completes a `->$#*` postderef last-index.  Probe
            // for continuation and mark chain_end_pending so the next lex_token emits InterpChainEnd — just like a
            // closing bracket at depth 0 would.  Without this, the next lex_token call in chain mode would route to
            // lex_normal_token and misinterpret the string-body bytes (e.g. `"`) as code tokens.
            let in_chain = self.context_stack.last().is_some_and(|ctx| ctx.chain_active && ctx.chain_depth == 0);
            if in_chain {
                let cont = self.peek_chain_starter();
                if let Some(ctx) = self.context_stack.last_mut() {
                    ctx.chain_end_pending = !cont;
                }
            }
            true
        } else {
            false
        }
    }

    // ── Identifiers ───────────────────────────────────────────

    fn scan_ident(&mut self) -> String {
        let start = self.line_pos();
        let mut first = true;
        while let Some(b) = self.peek_byte(false) {
            if b == b'_' || b.is_ascii_alphabetic() || (!first && b.is_ascii_digit()) {
                self.skip(1);
                first = false;
            } else if b == b':' && self.peek_byte_at(1) == Some(b':') {
                // Package separator Foo::Bar
                self.skip(2);
                first = true; // next char starts a new segment
            } else if b == b'\''
                && !first
                && self.features.contains(Features::APOSTROPHE_AS_PACKAGE_SEPARATOR)
                // Don't consume ' when it's the close delimiter of the current string context (e.g. qq'$Foo'bar — the
                // ' after Foo closes the string, not a package separator).
                && self.context_stack.last().and_then(|ctx| ctx.delim).is_none_or(|d| d != '\'')
                && self.peek_byte_at(1).is_some_and(|next|
                    next == b'_' || next.is_ascii_alphabetic()
                    || (self.effective_utf8 && next >= 0x80))
            {
                // Don't consume ' as a package separator if the identifier scanned so far is an active keyword.  Quote-
                // like operators use ' as a delimiter; unconditional keywords (print, grep, die, etc.) always stop;
                // feature-gated keywords (given, any, try, etc.) stop only when their feature is active.
                if let Some(line) = &self.source.line {
                    let so_far = &line.line[start..line.pos];
                    if self.is_active_keyword(so_far) {
                        break;
                    }
                }
                // Apostrophe as package separator: Foo'Bar → Foo::Bar
                self.skip(1);
                first = true; // next char starts a new segment
            } else if self.effective_utf8 && b >= 0x80 {
                // UTF-8 multi-byte character: decode and check XID_Start for the first character, XID_Continue for
                // subsequent characters.
                if let Some((ch, len)) = self.peek_utf8_char() {
                    let ok = if first { UnicodeXID::is_xid_start(ch) } else { UnicodeXID::is_xid_continue(ch) };
                    if ok {
                        self.skip(len);
                        first = false;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        let name = String::from_utf8_lossy(self.line_slice(start)).into_owned();
        // Normalize apostrophe separators to :: so the AST always uses :: regardless of which separator the source
        // used.  ASCII 0x27 cannot appear inside UTF-8 multi-byte sequences, so a simple replace is safe.
        let name = if name.contains('\'') { name.replace('\'', "::") } else { name };
        self.nfc_normalize(name)
    }

    fn lex_word(&mut self) -> Result<Token, ParseError> {
        let name = self.scan_ident();

        // Word operators (eq, ne, lt, gt, le, ge, cmp, x, and, or, xor, not) are always emitted as Ident(name).  The
        // parser recognizes them as operators in operator context via peek_op_info and token_to_binop.

        // Special double-underscore tokens.  __DATA__/__END__ have same-line-only autoquoting and trigger EOF.
        // __FILE__/__LINE__ must save their lex-time values before at_fat_comma (which may advance past newlines and
        // `# line` directives).  Other __ tokens (__PACKAGE__, __SUB__, __CLASS__) go through normal keyword lookup.
        match name.as_str() {
            "__DATA__" | "__END__" => {
                let kw = if name == "__DATA__" { Keyword::__DATA__ } else { Keyword::__END__ };
                while matches!(self.peek_byte(false), Some(b' ' | b'\t')) {
                    self.skip(1);
                }
                if self.peek_byte(false) == Some(b'=') && self.peek_byte_at(1) == Some(b'>') {
                    return Ok(Token::StrLit(name));
                }
                self.source.line = None;
                let offset = match self.source.next_line(false) {
                    Ok(Some(line)) => line.offset as u32,
                    _ => self.source.cursor() as u32,
                };
                self.data_end_info = Some((kw, offset));
                self.logical_eof = true;
                return Ok(Token::Eof);
            }
            "__FILE__" => {
                let saved_filename = self.source.filename().to_string();
                if self.at_fat_comma() {
                    return Ok(Token::StrLit(name));
                }
                return Ok(Token::SourceFile(saved_filename));
            }
            "__LINE__" => {
                let saved_line_no = self.source.line.as_ref().map(|l| l.number).unwrap_or(0) as u32;
                if self.at_fat_comma() {
                    return Ok(Token::StrLit(name));
                }
                return Ok(Token::SourceLine(saved_line_no));
            }
            _ => {}
        }

        // `x=` compound assignment: when the identifier is exactly "x" and the immediately next byte (no whitespace —
        // scan_ident stopped at a non-ident char) is `=` not followed by `>` (which would be `x =>`, a fat comma), emit
        // RepeatEq.  Must be before at_fat_comma which consumes whitespace and would break the adjacency check.
        if name == "x" && self.peek_byte(false) == Some(b'=') && self.peek_byte_at(1) != Some(b'>') {
            self.skip(1); // consume =
            return Ok(Token::Assign(AssignOp::RepeatEq));
        }

        // Fat-comma autoquoting: `if => 1`, `foo => 1`, `v5 => 1`, `__PACKAGE__ => 1`, etc.  The four tokens above
        // (__DATA__, __END__, __FILE__, __LINE__) already handled their own fat-comma checks; everything else goes
        // through here.  Must be before the v-string check so that `v5 => 1` autoquotes as "v5" instead of producing
        // VersionLit("v5").
        if self.at_fat_comma() {
            return Ok(Token::StrLit(name));
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
            return Ok(Token::VersionLit(vstr));
        }

        // Keywords — lookup_keyword handles feature gating internally.
        if let Some(kw) = keyword::lookup_keyword(name.as_bytes(), self.features) {
            return Ok(Token::Keyword(kw));
        }

        // Regular identifier / bareword
        Ok(Token::Ident(name))
    }

    /// Start sublexing for a quote-like operator.  The cursor must be positioned at (or before whitespace preceding) the
    /// delimiter byte.  Called by the parser after it decides a quote keyword should enter sublexing rather than
    /// autoquoting.
    pub fn begin_quote_sublex(&mut self, kw: Keyword) -> Result<Token, ParseError> {
        match kw {
            Keyword::Q => self.lex_q_string(),
            Keyword::Qq => self.lex_qq_string(),
            Keyword::Qw => self.lex_qw(),
            Keyword::Qr => self.lex_qr(),
            Keyword::Qx => self.lex_qx(),
            Keyword::M => self.lex_m(),
            Keyword::S => self.lex_s(),
            Keyword::Tr | Keyword::Y => self.lex_tr(),
            _ => unreachable!("begin_quote_sublex called with non-quote keyword: {kw:?}"),
        }
    }

    /// Skip whitespace and `#` comments (not POD), then return the next raw byte without consuming it.  Used by the
    /// parser to inspect the delimiter byte after a quote keyword without triggering tokenization.  Returns `None` at
    /// EOF.
    pub fn skip_ws_and_peek_byte(&mut self) -> Option<u8> {
        let _ = self.skip_ws_and_comments_no_pod();
        self.peek_byte(false)
    }

    /// Skip whitespace/newlines/comments (not POD) and check if `=>` (fat comma) follows.  Used by `lex_word` to
    /// autoquote keywords and identifiers at lex time.  Consumes the whitespace regardless of the result — the next
    /// `lex_normal_token` call would have skipped it anyway.
    fn at_fat_comma(&mut self) -> bool {
        let _ = self.skip_ws_and_comments_no_pod();
        self.peek_byte(false) == Some(b'=') && self.peek_byte_at(1) == Some(b'>')
    }

    // ── Strings ───────────────────────────────────────────────

    fn lex_single_quoted_string(&mut self) -> Result<Token, ParseError> {
        self.skip(1); // skip opening '
        let s = self.lex_body_str('\'', false)?;
        Ok(Token::StrLit(s))
    }

    // ── Unified string/regex body scanner (§5.4) ──────────────────

    /// Scan one token from a string/regex body.
    ///
    /// In interpolating mode (called repeatedly via context stack): returns one sub-token per call — `ConstSegment`,
    /// `InterpScalar`, `InterpScalarExprStart`, etc.  Returns `SublexEnd` when the closing delimiter is reached.
    ///
    /// In non-interpolating mode (called once by q//, '...', etc.): scans the entire body and returns a single
    /// `ConstSegment`.  The closing delimiter is consumed.
    ///
    /// Escape handling is controlled by the flags:
    /// - `!raw && !interpolating`: literal escapes (`\\`→`\`, `\delim`→delim).  For `q//`, `'...'`.
    /// - `!raw && interpolating`: double-quote escapes (`\n`, `\t`, etc.) via `process_escape`.  For `qq//`, `"..."`.
    /// - `raw`: passthrough (backslash prevents delimiter matching but both bytes are kept).  For `m//`, `tr//`.
    /// - `regex`: detect `(?{...})` code blocks (future).
    fn lex_body(&mut self, delim: Option<char>, depth: u32, interpolating: bool, regex: bool, raw: bool) -> Result<Spanned, ParseError> {
        // Compute open/close from the delimiter.  None means heredoc (no delimiter — end signaled by LexerSource).
        let extra = self.features.contains(Features::EXTRA_PAIRED_DELIMITERS);
        let (open, close) = match delim {
            Some(d) => {
                let (o, c) = matching_delimiter(d, extra);
                (o, Some(c))
            }
            None => (None, None),
        };

        // peek_byte(false) auto-loads the next line, consuming the virtual EOF signal if pending (heredoc or subst
        // body).
        let b = match self.peek_byte(false) {
            Some(b) => b,
            None => {
                let pos = self.span_pos();
                if close.is_none() {
                    // Heredoc or subst body finished (virtual EOF).
                    self.context_stack.pop();
                    self.case_mod_stack.clear();
                    self.case_mod_lcfirst = false;
                    self.case_mod_ucfirst = false;
                    return Ok(Spanned { token: Token::SublexEnd, span: Span::new(pos, pos) });
                }
                return Err(ParseError::new("unterminated string", Span::new(pos, pos)));
            }
        };

        // Compute start AFTER peek_byte, so the position reflects the loaded line (not a stale source cursor).
        let start = self.span_pos();

        // Fast dispatch for closing delimiter (incremental mode: context on the stack → pop and return SublexEnd).
        if let Some(close_ch) = close
            && depth == 0
            && !self.context_stack.is_empty()
        {
            let mut cbuf = [0u8; 4];
            let close_bytes = close_ch.encode_utf8(&mut cbuf).as_bytes();
            if self.remaining().starts_with(close_bytes) {
                self.skip(close_ch.len_utf8());
                self.context_stack.pop();
                self.case_mod_stack.clear();
                self.case_mod_lcfirst = false;
                self.case_mod_ucfirst = false;
                return Ok(Spanned { token: Token::SublexEnd, span: Span::new(start, self.span_pos()) });
            }
        }
        if interpolating {
            if b == b'$' {
                return self.lex_interp_scalar(start);
            }
            if b == b'@' {
                return self.lex_interp_array(start);
            }
            // \N{CHARNAME} — named Unicode character.  Handled here (not in process_escape) so it emits a separate
            // NamedChar token, like interpolation.  The \N{U+XXXX} hex form stays in process_escape since it's just a
            // hex escape with no name to preserve.
            if !raw
                && b == b'\\'
                && self.peek_byte_at(1) == Some(b'N')
                && self.peek_byte_at(2) == Some(b'{')
                && !(self.peek_byte_at(3) == Some(b'U') && self.peek_byte_at(4) == Some(b'+'))
            {
                let escape_start = self.span_pos();
                self.skip(3); // consume \N{
                let mut name = String::new();
                while let Some(b) = self.peek_byte(false) {
                    if b == b'}' {
                        self.skip(1);
                        break;
                    }
                    self.skip(1);
                    name.push(b as char);
                }
                let codepoint = unicode_names2::character(&name)
                    .map(|c| c as u32)
                    .ok_or_else(|| ParseError::new(format!("Unknown Unicode character name \"{name}\""), Span::new(escape_start, self.span_pos())))?;
                return Ok(Spanned { token: Token::NamedChar { name, codepoint }, span: Span::new(escape_start, self.span_pos()) });
            }
        }

        // Regex code blocks: (?{...}), (??{...}), and (*{...}).
        if regex && b == b'(' {
            if self.peek_byte_at(1) == Some(b'?') {
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
            if self.peek_byte_at(1) == Some(b'*') && self.peek_byte_at(2) == Some(b'{') {
                // (*{ — optimistic code block (5.37.7+).  Same as (?{...}) but doesn't disable optimizations.
                self.skip(3);
                if let Some(ctx) = self.context_stack.last_mut() {
                    ctx.expr_depth = 1;
                }
                return Ok(Spanned { token: Token::RegexCodeStart, span: Span::new(start, self.span_pos()) });
            }
        }

        // Scan a ConstSegment: everything until we hit the closing delimiter (or $/@/end-of-line in interpolating
        // mode).
        let mut s = String::new();
        let mut current_depth = depth;

        loop {
            // ── memchr fast path ──────────────────────────────────
            // When no case mods are active and we have remaining bytes in the current line, use SIMD-optimized search
            // to skip past safe bytes in bulk.
            let no_case_mods = !self.case_mod_lcfirst && !self.case_mod_ucfirst && self.case_mod_stack.last().is_none_or(|f| f.is_empty());
            if no_case_mods && current_depth == 0 {
                let r = self.remaining();
                if !r.is_empty() {
                    // First UTF-8 byte of each delimiter char, for memchr triggers.  For multi-byte delimiters this may
                    // produce false positives (other chars sharing the same lead byte), which is safe — the byte-by-
                    // byte fallback verifies the full sequence.
                    let close_byte = close.map(|c| {
                        let mut b = [0u8; 4];
                        c.encode_utf8(&mut b);
                        b[0]
                    });
                    let open_byte = open.map(|c| {
                        let mut b = [0u8; 4];
                        c.encode_utf8(&mut b);
                        b[0]
                    });

                    // Find the next trigger byte using memchr.
                    let trigger_pos = if regex {
                        // Regex: $, @, \, close, (
                        let sig = memchr3(b'$', b'@', b'\\', r);
                        let delim = if let Some(c) = close_byte { memchr2(c, b'(', r) } else { memchr(b'(', r) };
                        match (sig, delim) {
                            (Some(a), Some(b)) => Some(a.min(b)),
                            (a, b) => a.or(b),
                        }
                    } else if interpolating {
                        // Interpolating: $, @, \, close
                        let sig = memchr3(b'$', b'@', b'\\', r);
                        let delim = close_byte.and_then(|c| memchr(c, r));
                        match (sig, delim) {
                            (Some(a), Some(b)) => Some(a.min(b)),
                            (a, b) => a.or(b),
                        }
                    } else {
                        // Non-interpolating: \, close
                        if let Some(c) = close_byte { memchr2(b'\\', c, r) } else { memchr(b'\\', r) }
                    };

                    // Also search for open delimiter (depth tracking).
                    let trigger_pos = if let Some(ob) = open_byte
                        && ob != close_byte.unwrap_or(0)
                    {
                        match (trigger_pos, memchr(ob, r)) {
                            (Some(a), Some(b)) => Some(a.min(b)),
                            (a, b) => a.or(b),
                        }
                    } else {
                        trigger_pos
                    };

                    let safe_len = trigger_pos.unwrap_or(r.len());
                    if safe_len > 0 {
                        // Bulk-copy safe bytes, NFC-normalizing raw source content immediately.  This ensures escape-
                        // produced chars (pushed later) are never mixed with raw bytes before NFC.
                        let safe = &r[..safe_len];
                        match std::str::from_utf8(safe) {
                            Ok(text) => {
                                if self.effective_utf8 && text.bytes().any(|b| b >= 0x80) {
                                    let normalized: String = text.nfc().collect();
                                    s.push_str(&normalized);
                                } else {
                                    s.push_str(text);
                                }
                            }
                            Err(_) => s.push_str(&String::from_utf8_lossy(safe)),
                        }
                        self.skip(safe_len);
                        continue;
                    }
                }
            }

            // ── Byte-by-byte fallback ─────────────────────────────
            let Some(b) = self.peek_byte(true) else {
                // EOF or virtual EOF (peeked).  For delimited strings (close is Some), this means the closing delimiter
                // was not found — that's an error.
                if close.is_some() {
                    return Err(ParseError::new("unterminated string", Span::new(start, self.span_pos())));
                }
                break;
            };

            // Check close delimiter (handles both ASCII and multi-byte).
            if let Some(close_ch) = close {
                let close_len = close_ch.len_utf8();
                let mut cbuf = [0u8; 4];
                let close_bytes = close_ch.encode_utf8(&mut cbuf).as_bytes();
                if self.remaining().starts_with(close_bytes) {
                    if current_depth == 0 {
                        if self.context_stack.is_empty() {
                            // lex_body_str mode: consume the closing delimiter.
                            self.skip(close_len);
                        }
                        // Incremental mode: leave the delimiter for the SublexEnd fast dispatch on the next call.
                        break;
                    } else {
                        current_depth -= 1;
                        self.skip(close_len);
                        self.push_case_mod(&mut s, close_ch);
                        continue;
                    }
                }
            }

            // Check open delimiter for nesting depth.
            if let Some(open_ch) = open {
                let open_len = open_ch.len_utf8();
                let mut obuf = [0u8; 4];
                let open_bytes = open_ch.encode_utf8(&mut obuf).as_bytes();
                if self.remaining().starts_with(open_bytes) {
                    current_depth += 1;
                    self.skip(open_len);
                    self.push_case_mod(&mut s, open_ch);
                    continue;
                }
            }

            match b {
                b'$' | b'@' if interpolating => break,
                // Regex code block lookahead: break so fast dispatch handles it on the next call.
                b'(' if regex
                    && (self.peek_byte_at(1) == Some(b'?')
                        && (self.peek_byte_at(2) == Some(b'{') || (self.peek_byte_at(2) == Some(b'?') && self.peek_byte_at(3) == Some(b'{')))
                        || self.peek_byte_at(1) == Some(b'*') && self.peek_byte_at(2) == Some(b'{')) =>
                {
                    break;
                }
                b'\\' => {
                    // Before consuming the backslash, check for \N{CHARNAME} (not \N{U+XXXX}).  Break to flush
                    // accumulated text as ConstSegment; the next lex_body call's fast dispatch handles the escape.
                    if interpolating
                        && !raw
                        && self.peek_byte_at(1) == Some(b'N')
                        && self.peek_byte_at(2) == Some(b'{')
                        && !(self.peek_byte_at(3) == Some(b'U') && self.peek_byte_at(4) == Some(b'+'))
                    {
                        break;
                    }
                    self.skip(1);
                    if raw {
                        // Raw: backslash prevents delimiter matching.  For \delim, consume the delimiter (backslash
                        // dropped).  For everything else, keep both.
                        if let Some(close_ch) = close {
                            let mut cbuf = [0u8; 4];
                            let close_bytes = close_ch.encode_utf8(&mut cbuf).as_bytes();
                            if self.remaining().starts_with(close_bytes) {
                                self.skip(close_ch.len_utf8());
                                s.push(close_ch);
                                continue;
                            }
                        }
                        if let Some(open_ch) = open {
                            let mut obuf = [0u8; 4];
                            let open_bytes = open_ch.encode_utf8(&mut obuf).as_bytes();
                            if self.remaining().starts_with(open_bytes) {
                                self.skip(open_ch.len_utf8());
                                s.push(open_ch);
                                continue;
                            }
                        }
                        s.push('\\');
                    } else if interpolating {
                        // Double-quote escapes.
                        self.process_escape(&mut s, close)?;
                    } else {
                        // Literal (single-quote) escapes: \\ → \, \close → close, \open → open.
                        let mut matched = false;
                        if self.peek_byte(false) == Some(b'\\') {
                            self.skip(1);
                            s.push('\\');
                            matched = true;
                        }
                        if !matched && let Some(close_ch) = close {
                            let mut cbuf = [0u8; 4];
                            if self.remaining().starts_with(close_ch.encode_utf8(&mut cbuf).as_bytes()) {
                                self.skip(close_ch.len_utf8());
                                s.push(close_ch);
                                matched = true;
                            }
                        }
                        if !matched && let Some(open_ch) = open {
                            let mut obuf = [0u8; 4];
                            if self.remaining().starts_with(open_ch.encode_utf8(&mut obuf).as_bytes()) {
                                self.skip(open_ch.len_utf8());
                                s.push(open_ch);
                                matched = true;
                            }
                        }
                        if !matched {
                            s.push('\\');
                        }
                    }
                }
                _ => {
                    if b >= 0x80 {
                        if let Some((ch, len)) = self.peek_utf8_char() {
                            self.skip(len);
                            self.push_case_mod(&mut s, ch);
                        } else {
                            self.skip(1);
                            self.push_case_mod(&mut s, b as char);
                        }
                    } else {
                        self.skip(1);
                        self.push_case_mod(&mut s, b as char);
                    }
                }
            }
        }

        // Update depth in context stack (only relevant for interpolating mode with paired delimiters).
        if interpolating && let Some(ctx) = self.context_stack.last_mut() {
            ctx.depth = current_depth;
        }

        Ok(Spanned { token: Token::ConstSegment(s), span: Span::new(start, self.span_pos()) })
    }

    /// Non-interpolating convenience: scan the entire body and return the content as a String.  The closing delimiter
    /// is consumed.
    ///
    /// `raw` selects escape handling:
    /// - `false`: single-quote escapes (`\\`→`\`, `\delim`→delim).
    /// - `true`: raw passthrough (`\delim`→delim, else pass through).
    pub fn lex_body_str(&mut self, delim: char, raw: bool) -> Result<String, ParseError> {
        let spanned = self.lex_body(Some(delim), 0, false, false, raw)?;
        let s = match spanned.token {
            Token::ConstSegment(s) => s,
            _ => unreachable!("lex_body in non-interpolating mode should return ConstSegment"),
        };
        // `lex_body` only auto-consumes the closing delimiter when the context stack is empty — its incremental-sublex
        // protocol leaves the delim in place so the next `lex_token` call can emit `SublexEnd`.  When we're called from
        // inside a sublex context (e.g. a single-quoted subscript key inside a `"..."` interpolation, or the second
        // half of `tr{from}{to}`), that leaves the delim un-consumed.  Consume it here so `lex_body_str` always leaves
        // the cursor past the closer.
        //
        // Use `matching_delimiter` to get the CLOSING char — for paired delimiters like `{`, the close is `}`, not `{`
        // itself.
        let extra = self.features.contains(Features::EXTRA_PAIRED_DELIMITERS);
        let (_, close) = matching_delimiter(delim, extra);
        let mut cbuf = [0u8; 4];
        let close_bytes = close.encode_utf8(&mut cbuf).as_bytes();
        if self.remaining().starts_with(close_bytes) {
            self.skip(close.len_utf8());
        }
        Ok(s)
    }

    /// Process a backslash escape inside a double-quoted string.  The backslash has already been consumed.
    fn process_escape(&mut self, s: &mut String, close: Option<char>) -> Result<(), ParseError> {
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
            Some(b)
                if close.is_some_and(|c| {
                    let mut buf = [0u8; 4];
                    self.remaining().starts_with(c.encode_utf8(&mut buf).as_bytes())
                }) =>
            {
                let close_ch = close.unwrap_or(b as char);
                self.skip(close_ch.len_utf8());
                s.push(close_ch);
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
            Some(b'N') => {
                // \N{U+XXXX} — hex codepoint escape.  \N{CHARNAME} is handled inline in lex_body before process_escape
                // is called, so only the U+ form and bare \N reach here.
                self.skip(1);
                if self.peek_byte(false) == Some(b'{') {
                    let escape_start = self.span_pos();
                    self.skip(1);
                    let mut name = String::new();
                    while let Some(b) = self.peek_byte(false) {
                        if b == b'}' {
                            self.skip(1);
                            break;
                        }
                        self.skip(1);
                        name.push(b as char);
                    }
                    if let Some(hex) = name.strip_prefix("U+") {
                        let n = u32::from_str_radix(hex, 16)
                            .map_err(|_| ParseError::new(format!("Invalid hex in \\N{{U+{hex}}}"), Span::new(escape_start, self.span_pos())))?;
                        s.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
                    } else {
                        // Should not reach here — lex_body intercepts \N{CHARNAME} before calling process_escape.
                        // Defensive fallback: push replacement char.
                        s.push('\u{FFFD}');
                    }
                } else {
                    // Bare \N without braces — literal.
                    s.push('\\');
                    s.push('N');
                }
            }
            Some(b'o') => {
                // \o{NNN} — octal escape with braces.
                self.skip(1);
                if self.peek_byte(false) == Some(b'{') {
                    self.skip(1);
                    let mut n = 0u32;
                    while let Some(b) = self.peek_byte(false) {
                        if b == b'}' {
                            self.skip(1);
                            break;
                        }
                        if (b'0'..=b'7').contains(&b) {
                            self.skip(1);
                            n = n * 8 + (b - b'0') as u32;
                        } else {
                            break;
                        }
                    }
                    if let Some(c) = char::from_u32(n) {
                        s.push(c);
                    }
                } else {
                    // Bare \o without braces — literal.
                    s.push('\\');
                    s.push('o');
                }
            }
            Some(b'c') => {
                // \cX — control character.  The character following \c is XORed with 0x40 to produce the control char.
                self.skip(1);
                if let Some(next) = self.peek_byte(false) {
                    self.skip(1);
                    let ctrl = (next.to_ascii_uppercase()) ^ 0x40;
                    s.push(ctrl as char);
                } else {
                    s.push('\\');
                    s.push('c');
                }
            }
            Some(b'1'..=b'7') => {
                // \NNN — octal escape (1–3 digits, no braces).  Note: \0 is handled separately above.
                let mut n = 0u32;
                for _ in 0..3 {
                    if let Some(b) = self.peek_byte(false) {
                        if (b'0'..=b'7').contains(&b) {
                            self.skip(1);
                            n = n * 8 + (b - b'0') as u32;
                        } else {
                            break;
                        }
                    }
                }
                if let Some(c) = char::from_u32(n) {
                    s.push(c);
                }
            }
            // Case-modification escapes.  These affect subsequent characters until \E.  For now we consume the markers
            // and apply the transformations inline.
            Some(b'l') => {
                // \l — lowercase next character only.
                self.skip(1);
                self.case_mod_lcfirst = true;
                self.case_mod_ucfirst = false; // \l overrides pending \u
            }
            Some(b'u') => {
                // \u — titlecase next character only.
                self.skip(1);
                self.case_mod_ucfirst = true;
                self.case_mod_lcfirst = false; // \u overrides pending \l
            }
            Some(b'L') => {
                // \L — lowercase until \E.  Cumulative with enclosing flags.
                self.skip(1);
                let cur = self.case_mod_stack.last().copied().unwrap_or(CaseMod::EMPTY);
                self.case_mod_stack.push(cur | CaseMod::LOWER);
            }
            Some(b'U') => {
                // \U — uppercase until \E.  Cumulative with enclosing flags.
                self.skip(1);
                let cur = self.case_mod_stack.last().copied().unwrap_or(CaseMod::EMPTY);
                self.case_mod_stack.push(cur | CaseMod::UPPER);
            }
            Some(b'F') => {
                // \F — foldcase until \E.  Cumulative with enclosing flags.
                self.skip(1);
                let cur = self.case_mod_stack.last().copied().unwrap_or(CaseMod::EMPTY);
                self.case_mod_stack.push(cur | CaseMod::FOLD);
            }
            Some(b'Q') => {
                // \Q — quotemeta until \E.  Cumulative with enclosing flags.
                self.skip(1);
                let cur = self.case_mod_stack.last().copied().unwrap_or(CaseMod::EMPTY);
                self.case_mod_stack.push(cur | CaseMod::QUOTEMETA);
            }
            Some(b'E') => {
                // \E — pop, reverting to enclosing flags.
                self.skip(1);
                self.case_mod_stack.pop();
            }
            _ => s.push('\\'),
        }
        Ok(())
    }

    /// Lex `$name`, `${name}`, or `${expr}` interpolation inside a string.
    fn lex_interp_scalar(&mut self, start: u32) -> Result<Spanned, ParseError> {
        self.skip(1); // skip $

        // ${...} form
        if self.peek_byte(false) == Some(b'{') {
            self.skip(1); // skip {
            // Simple identifier: ${name}
            let next_byte = self.peek_byte(false);
            let is_ident = next_byte.is_some_and(|b| self.is_ident_start(b));
            if is_ident {
                let saved_pos = self.line_pos();
                let name = self.scan_ident();
                if self.peek_byte(false) == Some(b'}') {
                    self.skip(1);
                    return Ok(Spanned { token: Token::InterpScalar(name), span: Span::new(start, self.span_pos()) });
                }
                // Not a simple ${name} — backtrack and scan as expression
                if let Some(line) = self.source.line.as_mut() {
                    line.pos = saved_pos;
                }
            }
            // Expression interpolation: ${\ expr}, ${$ref}, etc.  Enter expression-parsing mode — normal code until }.
            if let Some(ctx) = self.context_stack.last_mut() {
                ctx.expr_depth = 1;
            }
            return Ok(Spanned { token: Token::InterpScalarExprStart, span: Span::new(start, self.span_pos()) });
        }

        // $name form — must start with alpha, _ or (in UTF-8 mode) a Unicode letter.
        let next_byte = self.peek_byte(false);
        let is_name = next_byte.is_some_and(|b| self.is_ident_start(b));
        if is_name {
            let name = self.scan_ident();
            // Check for subscript chain: [idx], {key}, ->[idx], ->{key}.  Only start a chain if a valid continuer is
            // actually present; a bare `->` (with nothing useful after) is treated as literal text.
            if self.peek_chain_starter() {
                if let Some(ctx) = self.context_stack.last_mut() {
                    ctx.chain_active = true;
                }
                return Ok(Spanned { token: Token::InterpScalarChainStart(name), span: Span::new(start, self.span_pos()) });
            }
            return Ok(Spanned { token: Token::InterpScalar(name), span: Span::new(start, self.span_pos()) });
        }

        // $^X — caret variable (single uppercase letter after ^).  Perl interpolates these in strings: `"v$^V"` gives
        // the Perl version string.
        if self.peek_byte(false) == Some(b'^')
            && let Some(next) = self.peek_byte_at(1)
            && (next.is_ascii_alphabetic() || next == b'[' || next == b']' || next == b'^' || next == b'_' || next == b'?' || next == b'\\')
        {
            self.skip(2); // skip ^ and the character
            let name = format!("^{}", next as char);
            return Ok(Spanned { token: Token::InterpScalar(name), span: Span::new(start, self.span_pos()) });
        }

        // $'ident — apostrophe as package separator in string interpolation.  Must check that ' is not the close
        // delimiter (e.g. qq'$'x' — the ' closes the string).
        if self.peek_byte(false) == Some(b'\'')
            && self.features.contains(Features::APOSTROPHE_AS_PACKAGE_SEPARATOR)
            && self.context_stack.last().and_then(|ctx| ctx.delim).is_none_or(|d| d != '\'')
            && self.peek_byte_at(1).is_some_and(|next| next == b'_' || next.is_ascii_alphabetic() || (self.effective_utf8 && next >= 0x80))
        {
            self.skip(1); // consume the apostrophe
            let rest = self.scan_ident();
            let full_name = format!("::{rest}");
            if self.peek_chain_starter() {
                if let Some(ctx) = self.context_stack.last_mut() {
                    ctx.chain_active = true;
                }
                return Ok(Spanned { token: Token::InterpScalarChainStart(full_name), span: Span::new(start, self.span_pos()) });
            }
            return Ok(Spanned { token: Token::InterpScalar(full_name), span: Span::new(start, self.span_pos()) });
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
        let next_byte = self.peek_byte(false);
        let is_name = next_byte.is_some_and(|b| self.is_ident_start(b));
        if is_name {
            let name = self.scan_ident();
            // Chain detection same as the scalar case: `@a[1..3]`, `@a{'k1','k2'}`, `@a->[...]` / `@a->{...}`.  The
            // semantics for arrays are slice-oriented but the lexical shape is the same.
            if self.peek_chain_starter() {
                if let Some(ctx) = self.context_stack.last_mut() {
                    ctx.chain_active = true;
                }
                return Ok(Spanned { token: Token::InterpArrayChainStart(name), span: Span::new(start, self.span_pos()) });
            }
            return Ok(Spanned { token: Token::InterpArray(name), span: Span::new(start, self.span_pos()) });
        }

        // @'ident — apostrophe as package separator in string interpolation (same logic as $'ident).
        if next_byte == Some(b'\'')
            && self.features.contains(Features::APOSTROPHE_AS_PACKAGE_SEPARATOR)
            && self.context_stack.last().and_then(|ctx| ctx.delim).is_none_or(|d| d != '\'')
            && self.peek_byte_at(1).is_some_and(|next| next == b'_' || next.is_ascii_alphabetic() || (self.effective_utf8 && next >= 0x80))
        {
            self.skip(1); // consume the apostrophe
            let rest = self.scan_ident();
            let full_name = format!("::{rest}");
            if self.peek_chain_starter() {
                if let Some(ctx) = self.context_stack.last_mut() {
                    ctx.chain_active = true;
                }
                return Ok(Spanned { token: Token::InterpArrayChainStart(full_name), span: Span::new(start, self.span_pos()) });
            }
            return Ok(Spanned { token: Token::InterpArray(full_name), span: Span::new(start, self.span_pos()) });
        }

        // Bare @ not followed by a name — treat as literal
        Ok(Spanned { token: Token::ConstSegment("@".into()), span: Span::new(start, self.span_pos()) })
    }

    /// Is the next raw-byte sequence a valid subscript chain starter?  Returns true for `[`, `{`, `->[`, `->{`, and the
    /// postderef forms `->@*`, `->%*`, `->$*`, `->&*`, `->**`.  Used both at chain entry (after `$name`/`@name`) and at
    /// chain continuation (after a closing bracket at depth 0).
    fn peek_chain_starter(&self) -> bool {
        let r = self.remaining();
        // Direct subscript: [idx] or {key}.
        matches!(r.first(), Some(b'[') | Some(b'{'))
            || (r.len() >= 3 && r[0] == b'-' && r[1] == b'>' && {
                let c = r[2];
                // Subscript forms: ->[idx], ->{key} — always valid in interpolation.
                matches!(c, b'[' | b'{')
                    // Postderef forms require `use feature 'postderef_qq'`.
                    || (self.features.contains(Features::POSTDEREF_QQ) && (
                        // Postderef whole: ->@*, ->%*, ->$*, ->&*, ->**.
                        (r.len() >= 4
                            && matches!(c, b'@' | b'%' | b'$' | b'&' | b'*')
                            && r[3] == b'*')
                        // Postderef slices: ->@[, ->@{, ->%[, ->%{.
                        || (r.len() >= 4
                            && matches!(c, b'@' | b'%')
                            && matches!(r[3], b'[' | b'{'))
                        // Postderef last-index: ->$#*.
                        || (r.len() >= 5
                            && c == b'$'
                            && r[3] == b'#'
                            && r[4] == b'*')
                    ))
            })
    }

    // ── q// qq// qw// ─────────────────────────────────────────

    fn lex_q_string(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;
        let s = self.lex_body_str(delim, false)?;
        Ok(Token::StrLit(s))
    }

    fn lex_qq_string(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;
        self.context_stack.push(LexContext::new(Some(delim), true, false, false));
        Ok(Token::QuoteSublexBegin(QuoteKind::Double, delim))
    }

    fn lex_qx(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;
        self.context_stack.push(LexContext::new(Some(delim), true, false, false));
        Ok(Token::QuoteSublexBegin(QuoteKind::Backtick, delim))
    }

    fn lex_qw(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;
        // qw is documented as equivalent to split(' ', q/.../).  Use literal (q//) escape mode: \\ → \, \delim → delim.
        let body = self.lex_body_str(delim, false)?;
        let words: Vec<String> = body.split_whitespace().map(String::from).collect();
        Ok(Token::QwList(words))
    }

    // ── Regex and friends ─────────────────────────────────────

    /// `m/pattern/flags` or `m{pattern}flags`
    fn lex_m(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;
        self.context_stack.push(LexContext::new(Some(delim), delim != '\'', true, true));
        Ok(Token::RegexSublexBegin(RegexKind::Match, delim))
    }

    /// `qr/pattern/flags` or `qr{pattern}flags`
    fn lex_qr(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;
        self.context_stack.push(LexContext::new(Some(delim), delim != '\'', true, true));
        Ok(Token::RegexSublexBegin(RegexKind::Qr, delim))
    }

    /// `s/pattern/replacement/flags` or `s{pattern}{replacement}flags`
    fn lex_s(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;

        // Push context for the pattern body (raw, regex mode).  Single-quote delimiter disables interpolation.  The
        // parser will collect body tokens until SublexEnd, then call start_subst_replacement to set up the replacement
        // body.
        self.context_stack.push(LexContext::new(Some(delim), delim != '\'', true, true));

        Ok(Token::SubstSublexBegin(delim))
    }

    /// Set up the replacement body of a substitution after the pattern has been consumed.  Called by the parser after
    /// collecting the pattern's SublexEnd.
    ///
    /// For paired delimiters, reads the replacement delimiter.  Scans ahead for flags via `start_subst_body`, then
    /// pushes the appropriate LexContext for the replacement body.  Returns the captured flags.
    pub fn start_subst_replacement(&mut self, pattern_delim: char) -> Result<Option<String>, ParseError> {
        let extra = self.features.contains(Features::EXTRA_PAIRED_DELIMITERS);
        let repl_delim = if is_paired(pattern_delim, extra) { self.read_quote_delimiter()? } else { pattern_delim };

        let flags = self.source.start_subst_body(repl_delim, extra)?;
        let has_eval = flags.as_ref().is_some_and(|f| f.contains('e'));

        // Push context for the replacement body.  delim is None — the body ends at the virtual EOF set up by
        // start_subst_body, not at a delimiter byte.  With /e: raw scan (code, parser will reparse).  Without /e:
        // interpolating string.
        self.context_stack.push(LexContext::new(None, !has_eval, has_eval, false));

        Ok(flags)
    }

    /// `tr/from/to/flags` or `y/from/to/flags`
    fn lex_tr(&mut self) -> Result<Token, ParseError> {
        let delim = self.read_quote_delimiter()?;
        let from = self.lex_body_str(delim, true)?;
        let extra = self.features.contains(Features::EXTRA_PAIRED_DELIMITERS);
        let to = if is_paired(delim, extra) {
            let delim2 = self.read_quote_delimiter()?;
            self.lex_body_str(delim2, true)?
        } else {
            self.lex_body_str(delim, true)?
        };
        let flags = self.scan_adjacent_word_chars();
        Ok(Token::TranslitLit(from, to, flags))
    }

    /// Read the delimiter character for a quote-like construct.  Skips whitespace first if the current byte is
    /// whitespace.  Returns a `char` — ASCII for single-byte delimiters, or a decoded Unicode character for multi-byte
    /// UTF-8.
    fn read_quote_delimiter(&mut self) -> Result<char, ParseError> {
        // Match toke.c's scan_str: skip whitespace before the delimiter only if the current byte IS whitespace (or the
        // line is exhausted).  `m#foo#` uses `#` as the delimiter — it's not a comment.  `m /foo/` skips the space and
        // uses `/`.
        //
        // Uses the no-pod skipper: inside a quote op's delimiter scan, `=pod` at column 0 is a candidate delimiter
        // byte, not a pod block.  See `skip_ws_and_comments_no_pod`.
        match self.peek_byte(false) {
            Some(b) if b == b' ' || b == b'\t' => {
                self.skip_ws_and_comments_no_pod()?;
            }
            None => {
                // End of line — need to cross to next line.
                self.skip_ws_and_comments_no_pod()?;
            }
            _ => {}
        }
        let b = self.peek_byte(false).ok_or_else(|| ParseError::new("expected delimiter", Span::new(self.span_pos(), self.span_pos())))?;
        if b < 0x80 {
            // ASCII delimiter — single byte.
            self.skip(1);
            Ok(b as char)
        } else if let Some((ch, len)) = self.peek_utf8_char() {
            // Multi-byte UTF-8 delimiter.
            self.skip(len);
            Ok(ch)
        } else {
            self.skip(1);
            Err(ParseError::new(format!("invalid UTF-8 byte \\x{b:02X} in delimiter position"), Span::new(self.span_pos(), self.span_pos())))
        }
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

    /// Called by the parser after consuming a `Dot` token in term position.  If the next byte is a digit, scans a
    /// leading-dot float (`.5`, `.5e2`) or v-string (`.5.6` → `v0.5.6`).  Returns `Some(FloatLit(n))` or
    /// `Some(VersionLit(s))` if found, `None` otherwise (the dot is not the start of a numeric literal).
    pub fn lex_leading_dot_float(&mut self) -> Result<Option<Token>, ParseError> {
        if !self.peek_byte(false).is_some_and(|b| b.is_ascii_digit()) {
            return Ok(None);
        }
        let start = self.line_pos();
        self.scan_digits();

        // Check for v-string: `.5.6` has a second dot+digit → VersionLit("0.5.6").
        if self.peek_byte(false) == Some(b'.') && self.peek_byte_at(1).is_some_and(|b| b.is_ascii_digit()) {
            let mut vstr = format!("0.{}", self.line_slice_str(start)?);
            while self.peek_byte(false) == Some(b'.') && self.peek_byte_at(1).is_some_and(|b| b.is_ascii_digit()) {
                vstr.push('.');
                self.skip(1);
                let seg_start = self.line_pos();
                while self.peek_byte(false).is_some_and(|b| b.is_ascii_digit()) {
                    self.skip(1);
                }
                vstr.push_str(self.line_slice_str(seg_start)?);
            }
            return Ok(Some(Token::VersionLit(vstr)));
        }

        // Not a v-string — float with optional exponent.
        self.scan_exponent();
        let s = self.line_slice_str(start)?;
        let s = format!("0.{}", s.replace('_', ""));
        let n: f64 = s.parse().map_err(|_| ParseError::new("invalid float literal", self.span_from(start)))?;
        Ok(Some(Token::FloatLit(n)))
    }

    /// Called by the parser after consuming a `Minus` token in term position.  Returns `Some(Filetest(b))` if the next
    /// byte is a single letter not followed by a word-continuation char (e.g. `-f $file`, `-d "/tmp"`).  Returns `None`
    /// otherwise.
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
        // Fat-comma autoquoting: `-f => val` → StrLit("-f").  This function is always called after a minus, so the
        // "-" prefix is implicit.
        if self.at_fat_comma() {
            return Some(Token::StrLit(format!("-{}", b as char)));
        }
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

    /// Consume adjacent ASCII word characters (letters, digits, underscore) without skipping whitespace first.  Returns
    /// `None` if the next byte is not a word character.  Used by the parser to collect regex and transliteration flags
    /// immediately after a closing delimiter.  Perl's flag scanner (`S_pmflag`) consumes all word characters and
    /// reports errors for invalid ones.
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
                // Always return ShiftLeft / ShiftLeftEq.  The parser handles heredoc detection in term position by
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

    /// Called by the parser after consuming a `NumLt` token in term position.  Attempts to scan a readline/glob
    /// construct: `<...>` where the content ends at `>` on the same line.  Returns the `Readline(content)` token if
    /// successful, or `None` if no `>` terminates the content (the parser should then treat `<` as less-than).
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
            Some(Token::Readline(content, false))
        } else {
            // Not a readline — rewind.
            if let Some(line) = self.source.line.as_mut() {
                line.pos = start_pos;
            }
            None
        }
    }

    /// Called by the parser after consuming a `ShiftLeft` token in term position.  Attempts to read a heredoc tag (with
    /// optional `~` prefix) and start the heredoc body.  Returns the token produced (`QuoteSublexBegin` for
    /// interpolating, `HeredocLit` for literal), or rewinds and returns `None` if no valid tag follows — the parser
    /// should then treat the `ShiftLeft` as a shift.
    pub fn lex_heredoc_after_shift_left(&mut self) -> Result<Option<Token>, ParseError> {
        let saved = self.line_pos();

        // `<<>>` — double diamond (safe version of <>).  Must check before heredoc tag parsing since `>` is not a valid
        // tag character.
        if self.peek_byte(false) == Some(b'>') {
            self.skip(1);
            if self.peek_byte(false) == Some(b'>') {
                self.skip(1);
                return Ok(Some(Token::Readline(String::new(), true)));
            }
            // Single `>` after `<<` — not a valid heredoc or diamond.  Rewind.
            if let Some(line) = self.source.line.as_mut() {
                line.pos = saved;
            }
            return Ok(None);
        }

        // <<~ for indented heredocs
        let indented = self.peek_byte(false) == Some(b'~');
        if indented {
            self.skip(1);
        }

        // Whitespace between <<(~) and the tag is allowed before quoted tags but NOT before bare tags.  Perl treats
        // `<< TAG` as `<<""` (empty tag) which is forbidden.
        let had_whitespace = matches!(self.peek_byte(false), Some(b' ' | b'\t'));
        if had_whitespace {
            while matches!(self.peek_byte(false), Some(b' ' | b'\t')) {
                self.skip(1);
            }
        }

        match self.peek_byte(false) {
            // Quoted tags: whitespace before them is fine.
            Some(b'"') | Some(b'\'') | Some(b'\\') | Some(b'`') => Ok(Some(self.lex_heredoc(indented)?)),
            // Bare tags: must be adjacent (no whitespace).
            Some(b) if !had_whitespace && (b == b'_' || b.is_ascii_alphanumeric() || (self.effective_utf8 && b >= 0x80)) => {
                Ok(Some(self.lex_heredoc(indented)?))
            }
            _ => {
                // No valid tag — rewind to just after << so the parser can proceed with a normal shift-left.
                if let Some(line) = self.source.line.as_mut() {
                    line.pos = saved;
                }
                Ok(None)
            }
        }
    }

    /// Lex a heredoc tag and start body processing via LexerSource.  Position is after `<<` (and optional `~`), at the
    /// tag start.
    fn lex_heredoc(&mut self, indented: bool) -> Result<Token, ParseError> {
        // Determine quoting style and extract tag.  `command` tracks backtick quoting (interpolated + executed).
        let (kind, tag, command) = match self.peek_byte(false) {
            Some(b'\'') => {
                // <<'TAG' — literal
                self.skip(1);
                let tag = self.scan_heredoc_tag(b'\'')?;
                let k = if indented { HeredocKind::IndentedLiteral } else { HeredocKind::Literal };
                (k, tag, false)
            }
            Some(b'"') => {
                // <<"TAG" — interpolating (explicit)
                self.skip(1);
                let tag = self.scan_heredoc_tag(b'"')?;
                let k = if indented { HeredocKind::Indented } else { HeredocKind::Interpolating };
                (k, tag, false)
            }
            Some(b'`') => {
                // <<`TAG` — command (interpolated, then executed).
                self.skip(1);
                let tag = self.scan_heredoc_tag(b'`')?;
                let k = if indented { HeredocKind::Indented } else { HeredocKind::Interpolating };
                (k, tag, true)
            }
            Some(b'\\') => {
                // <<\TAG — backslash form, equivalent to <<'TAG'.
                self.skip(1);
                let tag_start = self.line_pos();
                loop {
                    match self.peek_byte(false) {
                        Some(b) if b == b'_' || b.is_ascii_alphanumeric() => self.skip(1),
                        Some(b) if self.effective_utf8 && b >= 0x80 => {
                            if let Some((ch, len)) = self.peek_utf8_char()
                                && UnicodeXID::is_xid_continue(ch)
                            {
                                self.skip(len);
                            } else {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                let tag = String::from_utf8_lossy(self.line_slice(tag_start)).into_owned();
                let k = if indented { HeredocKind::IndentedLiteral } else { HeredocKind::Literal };
                (k, tag, false)
            }
            _ => {
                // Bare identifier — interpolating
                let tag_start = self.line_pos();
                loop {
                    match self.peek_byte(false) {
                        Some(b) if b == b'_' || b.is_ascii_alphanumeric() => self.skip(1),
                        Some(b) if self.effective_utf8 && b >= 0x80 => {
                            if let Some((ch, len)) = self.peek_utf8_char()
                                && UnicodeXID::is_xid_continue(ch)
                            {
                                self.skip(len);
                            } else {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                let tag = String::from_utf8_lossy(self.line_slice(tag_start)).into_owned();
                let k = if indented { HeredocKind::Indented } else { HeredocKind::Interpolating };
                (k, tag, false)
            }
        };

        let tag_bytes = Bytes::from(tag.as_bytes().to_vec());
        let quote_kind = if command { QuoteKind::Backtick } else { QuoteKind::Heredoc };

        match kind {
            HeredocKind::Interpolating => {
                self.source.start_heredoc(tag_bytes)?;
                self.context_stack.push(LexContext::new(None, true, false, false));
                Ok(Token::QuoteSublexBegin(quote_kind, '\0'))
            }
            HeredocKind::Indented => {
                self.source.start_indented_heredoc(tag_bytes)?;
                self.context_stack.push(LexContext::new(None, true, false, false));
                Ok(Token::QuoteSublexBegin(quote_kind, '\0'))
            }
            HeredocKind::Literal => {
                self.source.start_heredoc(tag_bytes)?;
                self.collect_heredoc_literal(&tag, false)
            }
            HeredocKind::IndentedLiteral => {
                self.source.start_indented_heredoc(tag_bytes)?;
                self.collect_heredoc_literal(&tag, true)
            }
        }
    }

    /// Collect a literal heredoc body as a raw string.  LexerSource handles terminator detection and indent stripping.
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
            Some(b'.') if self.features.contains(Features::BITWISE) => {
                self.skip(1);
                if self.peek_byte(false) == Some(b'=') {
                    self.skip(1);
                    Token::Assign(AssignOp::StringBitAndEq)
                } else {
                    Token::StringBitAnd
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
            Some(b'.') if self.features.contains(Features::BITWISE) => {
                self.skip(1);
                if self.peek_byte(false) == Some(b'=') {
                    self.skip(1);
                    Token::Assign(AssignOp::StringBitOrEq)
                } else {
                    Token::StringBitOr
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

/// Return the (open, close) delimiter pair for a given delimiter.  For paired brackets, open is `Some(delim)` and close
/// is the matching bracket.  For same-char delimiters, open is `None`.  Packed open delimiters (321 chars, 997 UTF-8
/// bytes).  First 4 bytes are the standard ASCII pairs; the rest are Unicode (gated on `use feature
/// 'extra_paired_delimiters'`).
const DELIM_OPEN: &str = "(<[{\u{00AB}\u{00BB}\u{0F3A}\u{0F3C}\u{169B}\u{2018}\u{2019}\u{201C}\u{201D}\u{2035}\u{2036}\u{2037}\u{2039}\u{203A}\u{2045}\u{204D}\u{207D}\u{208D}\u{2192}\u{219B}\u{219D}\u{21A0}\u{21A3}\u{21A6}\u{21AA}\u{21AC}\u{21B1}\u{21B3}\u{21C0}\u{21C1}\u{21C9}\u{21CF}\u{21D2}\u{21DB}\u{21DD}\u{21E2}\u{21E5}\u{21E8}\u{21F4}\u{21F6}\u{21F8}\u{21FB}\u{21FE}\u{2208}\u{2209}\u{220A}\u{2264}\u{2266}\u{2268}\u{226A}\u{226E}\u{2270}\u{2272}\u{2274}\u{227A}\u{227C}\u{227E}\u{2280}\u{2282}\u{2284}\u{2286}\u{2288}\u{228A}\u{22A3}\u{22A6}\u{22A8}\u{22A9}\u{22B0}\u{22D0}\u{22D6}\u{22D8}\u{22DC}\u{22DE}\u{22E0}\u{22E6}\u{22E8}\u{22F2}\u{22F3}\u{22F4}\u{22F6}\u{22F7}\u{2308}\u{230A}\u{2326}\u{2329}\u{2348}\u{23E9}\u{23ED}\u{261B}\u{261E}\u{269E}\u{2768}\u{276A}\u{276C}\u{276E}\u{2770}\u{2772}\u{2774}\u{27C3}\u{27C5}\u{27C8}\u{27DE}\u{27E6}\u{27E8}\u{27EA}\u{27EC}\u{27EE}\u{27F4}\u{27F6}\u{27F9}\u{27FC}\u{27FE}\u{27FF}\u{2900}\u{2901}\u{2903}\u{2905}\u{2907}\u{290D}\u{290F}\u{2910}\u{2911}\u{2914}\u{2915}\u{2916}\u{2917}\u{2918}\u{291A}\u{291C}\u{291E}\u{2920}\u{2933}\u{2937}\u{2945}\u{2947}\u{2953}\u{2957}\u{295B}\u{295F}\u{2964}\u{296C}\u{296D}\u{2971}\u{2972}\u{2974}\u{2975}\u{2979}\u{2983}\u{2985}\u{2987}\u{2989}\u{298B}\u{298D}\u{298F}\u{2991}\u{2993}\u{2995}\u{2997}\u{29A8}\u{29AA}\u{29B3}\u{29C0}\u{29D8}\u{29DA}\u{29FC}\u{2A79}\u{2A7B}\u{2A7D}\u{2A7F}\u{2A81}\u{2A83}\u{2A85}\u{2A87}\u{2A89}\u{2A8D}\u{2A95}\u{2A97}\u{2A99}\u{2A9B}\u{2A9D}\u{2A9F}\u{2AA1}\u{2AA6}\u{2AA8}\u{2AAA}\u{2AAC}\u{2AAF}\u{2AB1}\u{2AB3}\u{2AB5}\u{2AB7}\u{2AB9}\u{2ABB}\u{2ABD}\u{2ABF}\u{2AC1}\u{2AC3}\u{2AC5}\u{2AC7}\u{2AC9}\u{2ACB}\u{2ACF}\u{2AD1}\u{2AD5}\u{2AE5}\u{2AF7}\u{2AF9}\u{2B46}\u{2B47}\u{2B48}\u{2B4C}\u{2B62}\u{2B6C}\u{2B72}\u{2B7C}\u{2B86}\u{2B8A}\u{2B95}\u{2B9A}\u{2B9E}\u{2BA1}\u{2BA3}\u{2BA9}\u{2BAB}\u{2BB1}\u{2BB3}\u{2BEE}\u{2E02}\u{2E03}\u{2E04}\u{2E05}\u{2E09}\u{2E0A}\u{2E0C}\u{2E0D}\u{2E11}\u{2E1C}\u{2E1D}\u{2E20}\u{2E21}\u{2E22}\u{2E24}\u{2E26}\u{2E28}\u{2E36}\u{2E42}\u{2E55}\u{2E57}\u{2E59}\u{2E5B}\u{3008}\u{300A}\u{300C}\u{300E}\u{3010}\u{3014}\u{3016}\u{3018}\u{301A}\u{301D}\u{A9C1}\u{FD3E}\u{FE59}\u{FE5B}\u{FE5D}\u{FE64}\u{FF08}\u{FF1C}\u{FF3B}\u{FF5B}\u{FF5F}\u{FF62}\u{FFEB}\u{1D103}\u{1D106}\u{1F449}\u{1F508}\u{1F509}\u{1F50A}\u{1F57B}\u{1F599}\u{1F59B}\u{1F59D}\u{1F5E6}\u{1F802}\u{1F806}\u{1F80A}\u{1F812}\u{1F816}\u{1F81A}\u{1F81E}\u{1F822}\u{1F826}\u{1F82A}\u{1F82E}\u{1F832}\u{1F836}\u{1F83A}\u{1F83E}\u{1F842}\u{1F846}\u{1F852}\u{1F862}\u{1F86A}\u{1F872}\u{1F87A}\u{1F882}\u{1F892}\u{1F896}\u{1F89A}\u{1F8A1}\u{1F8A3}\u{1F8A5}\u{1F8A7}\u{1F8A9}\u{1F8AB}\u{1F8B6}";

/// Paired close delimiters at matching byte offsets.
const DELIM_CLOSE: &str = ")>]}\u{00BB}\u{00AB}\u{0F3B}\u{0F3D}\u{169C}\u{2019}\u{2018}\u{201D}\u{201C}\u{2032}\u{2033}\u{2034}\u{203A}\u{2039}\u{2046}\u{204C}\u{207E}\u{208E}\u{2190}\u{219A}\u{219C}\u{219E}\u{21A2}\u{21A4}\u{21A9}\u{21AB}\u{21B0}\u{21B2}\u{21BC}\u{21BD}\u{21C7}\u{21CD}\u{21D0}\u{21DA}\u{21DC}\u{21E0}\u{21E4}\u{21E6}\u{2B30}\u{2B31}\u{21F7}\u{21FA}\u{21FD}\u{220B}\u{220C}\u{220D}\u{2265}\u{2267}\u{2269}\u{226B}\u{226F}\u{2271}\u{2273}\u{2275}\u{227B}\u{227D}\u{227F}\u{2281}\u{2283}\u{2285}\u{2287}\u{2289}\u{228B}\u{22A2}\u{2ADE}\u{2AE4}\u{2AE3}\u{22B1}\u{22D1}\u{22D7}\u{22D9}\u{22DD}\u{22DF}\u{22E1}\u{22E7}\u{22E9}\u{22FA}\u{22FB}\u{22FC}\u{22FD}\u{22FE}\u{2309}\u{230B}\u{232B}\u{232A}\u{2347}\u{23EA}\u{23EE}\u{261A}\u{261C}\u{269F}\u{2769}\u{276B}\u{276D}\u{276F}\u{2771}\u{2773}\u{2775}\u{27C4}\u{27C6}\u{27C9}\u{27DD}\u{27E7}\u{27E9}\u{27EB}\u{27ED}\u{27EF}\u{2B32}\u{27F5}\u{27F8}\u{27FB}\u{27FD}\u{2B33}\u{2B34}\u{2B35}\u{2902}\u{2B36}\u{2906}\u{290C}\u{290E}\u{2B37}\u{2B38}\u{2B39}\u{2B3A}\u{2B3B}\u{2B3C}\u{2B3D}\u{2919}\u{291B}\u{291D}\u{291F}\u{2B3F}\u{2936}\u{2946}\u{2B3E}\u{2952}\u{2956}\u{295A}\u{295E}\u{2962}\u{296A}\u{296B}\u{2B40}\u{2B41}\u{2B4B}\u{2B42}\u{297B}\u{2984}\u{2986}\u{2988}\u{298A}\u{298C}\u{2990}\u{298E}\u{2992}\u{2994}\u{2996}\u{2998}\u{29A9}\u{29AB}\u{29B4}\u{29C1}\u{29D9}\u{29DB}\u{29FD}\u{2A7A}\u{2A7C}\u{2A7E}\u{2A80}\u{2A82}\u{2A84}\u{2A86}\u{2A88}\u{2A8A}\u{2A8E}\u{2A96}\u{2A98}\u{2A9A}\u{2A9C}\u{2A9E}\u{2AA0}\u{2AA2}\u{2AA7}\u{2AA9}\u{2AAB}\u{2AAD}\u{2AB0}\u{2AB2}\u{2AB4}\u{2AB6}\u{2AB8}\u{2ABA}\u{2ABC}\u{2ABE}\u{2AC0}\u{2AC2}\u{2AC4}\u{2AC6}\u{2AC8}\u{2ACA}\u{2ACC}\u{2AD0}\u{2AD2}\u{2AD6}\u{22AB}\u{2AF8}\u{2AFA}\u{2B45}\u{2B49}\u{2B4A}\u{2973}\u{2B60}\u{2B6A}\u{2B70}\u{2B7A}\u{2B84}\u{2B88}\u{2B05}\u{2B98}\u{2B9C}\u{2BA0}\u{2BA2}\u{2BA8}\u{2BAA}\u{2BB0}\u{2BB2}\u{2BEC}\u{2E03}\u{2E02}\u{2E05}\u{2E04}\u{2E0A}\u{2E09}\u{2E0D}\u{2E0C}\u{2E10}\u{2E1D}\u{2E1C}\u{2E21}\u{2E20}\u{2E23}\u{2E25}\u{2E27}\u{2E29}\u{2E37}\u{201E}\u{2E56}\u{2E58}\u{2E5A}\u{2E5C}\u{3009}\u{300B}\u{300D}\u{300F}\u{3011}\u{3015}\u{3017}\u{3019}\u{301B}\u{301E}\u{A9C2}\u{FD3F}\u{FE5A}\u{FE5C}\u{FE5E}\u{FE65}\u{FF09}\u{FF1E}\u{FF3D}\u{FF5D}\u{FF60}\u{FF63}\u{FFE9}\u{1D102}\u{1D107}\u{1F448}\u{1F568}\u{1F569}\u{1F56A}\u{1F57D}\u{1F598}\u{1F59A}\u{1F59C}\u{1F5E7}\u{1F800}\u{1F804}\u{1F808}\u{1F810}\u{1F814}\u{1F818}\u{1F81C}\u{1F820}\u{1F824}\u{1F828}\u{1F82C}\u{1F830}\u{1F834}\u{1F838}\u{1F83C}\u{1F840}\u{1F844}\u{1F850}\u{1F860}\u{1F868}\u{1F870}\u{1F878}\u{1F880}\u{1F890}\u{1F894}\u{1F898}\u{1F8A0}\u{1F8A2}\u{1F8A6}\u{1F8A4}\u{1F8A8}\u{1F8AA}\u{1F8B4}";

/// Byte offset past the four standard ASCII delimiter pairs.
const DELIM_ASCII_END: usize = 4;

/// Look up the matching close delimiter for `delim`.
///
/// Returns `(Some(open), close)` for paired delimiters or `(None, delim)` for non-paired (same char open and close).
///
/// ASCII pairs `()`, `[]`, `{}`, `<>` are always recognised.  Unicode pairs require `extra_paired` (the
/// `extra_paired_delimiters` feature flag).
///
/// Uses SIMD-optimized `memmem` on packed UTF-8 tables.
pub fn matching_delimiter(delim: char, extra_paired: bool) -> (Option<char>, char) {
    let limit = if extra_paired { DELIM_OPEN.len() } else { DELIM_ASCII_END };
    let haystack = &DELIM_OPEN.as_bytes()[..limit];
    let mut buf = [0u8; 4];
    let needle = delim.encode_utf8(&mut buf);
    if let Some(pos) = memchr::memmem::find(haystack, needle.as_bytes()) {
        let close_bytes = &DELIM_CLOSE.as_bytes()[pos..];
        // SAFETY: DELIM_CLOSE is a valid &str, and `pos` is a char boundary (same offset as a char boundary in
        // DELIM_OPEN, and all pairs have the same UTF-8 byte length).
        let close_str = std::str::from_utf8(close_bytes).unwrap_or("");
        if let Some(close_ch) = close_str.chars().next() {
            return (Some(delim), close_ch);
        }
    }
    (None, delim)
}

/// Whether a delimiter is a paired bracket.
pub fn is_paired(delim: char, extra_paired: bool) -> bool {
    matching_delimiter(delim, extra_paired).0.is_some()
}

fn hex_digit(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

// ── Format-body helpers ───────────────────────────────────────

/// True if `bytes` is a format terminator line: column-0 `.` optionally followed by whitespace and/or a line ending.
fn is_format_terminator(bytes: &[u8]) -> bool {
    if bytes.first() != Some(&b'.') {
        return false;
    }
    bytes[1..].iter().all(|&b| b == b' ' || b == b'\t' || b == b'\r' || b == b'\n')
}

/// Strip a trailing `\n` and optional preceding `\r` from a line.
fn strip_line_ending(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    if end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && bytes[end - 1] == b'\r' {
        end -= 1;
    }
    &bytes[..end]
}

/// Classify a picture line's repeat behavior by counting tildes.
fn classify_repeat(bytes: &[u8]) -> RepeatKind {
    let mut saw_single = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'~' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'~' {
                return RepeatKind::Repeat;
            }
            saw_single = true;
        }
        i += 1;
    }
    if saw_single { RepeatKind::Suppress } else { RepeatKind::None }
}

/// Try to parse a field specifier starting at `bytes[start]`, which must be `@` or `^`.  Returns `(FieldKind,
/// consumed_bytes)` on success, or `None` if the characters don't form a valid field (in which case the `@` or `^`
/// should be treated as literal).
///
/// Supported forms (with optional `...` truncation suffix on the three text-justify and three fill-justify variants):
///   @*  ^*                             — multi-line
///   @<<<  @>>>  @|||                   — text, justified
///   ^<<<  ^>>>  ^|||                   — fill-mode, justified
///   @####[.##]  @0###[.##]             — numeric
///   ^####[.##]  ^0###[.##]             — special numeric (undef → blank)
fn parse_field(bytes: &[u8], start: usize) -> Option<(FieldKind, usize)> {
    debug_assert!(bytes[start] == b'@' || bytes[start] == b'^');
    let caret = bytes[start] == b'^';
    let after = start + 1;
    if after >= bytes.len() {
        return None;
    }

    // `@*` / `^*`
    if bytes[after] == b'*' {
        let kind = if caret { FieldKind::FillMultiLine } else { FieldKind::MultiLine };
        return Some((kind, 2));
    }

    let c = bytes[after];
    match c {
        b'<' | b'>' | b'|' => {
            // Text field: count pad characters of the same kind.
            let pad = c;
            let mut i = after;
            while i < bytes.len() && bytes[i] == pad {
                i += 1;
            }
            // Optional `...` truncation suffix.
            let mut truncate_ellipsis = false;
            if i + 2 < bytes.len() && &bytes[i..i + 3] == b"..." {
                truncate_ellipsis = true;
                i += 3;
            }
            // Width counts from `@`/`^` through the pad chars only (not the ellipsis, which just annotates the field).
            let width = (i - start - if truncate_ellipsis { 3 } else { 0 }) as u32;
            let kind = match (caret, pad) {
                (false, b'<') => FieldKind::LeftJustify { width, truncate_ellipsis },
                (false, b'>') => FieldKind::RightJustify { width, truncate_ellipsis },
                (false, b'|') => FieldKind::Center { width, truncate_ellipsis },
                (true, b'<') => FieldKind::FillLeft { width, truncate_ellipsis },
                (true, b'>') => FieldKind::FillRight { width, truncate_ellipsis },
                (true, b'|') => FieldKind::FillCenter { width, truncate_ellipsis },
                _ => unreachable!(),
            };
            Some((kind, i - start))
        }
        b'#' | b'0' => {
            // Numeric field: `####`, `0###`, `####.##`, `0###.##`, or (rare) `.####`.  Leading `0` only counts if
            // immediately followed by `#` (or `.`); otherwise it's not a numeric start.
            let leading_zeros = c == b'0';
            let mut i = after;
            if leading_zeros {
                // `@0###`: consume the `0`, require at least one `#` or `.` to follow.
                if i + 1 >= bytes.len() || (bytes[i + 1] != b'#' && bytes[i + 1] != b'.') {
                    return None;
                }
                i += 1;
            }
            // Integer `#`s.
            let int_start = i;
            while i < bytes.len() && bytes[i] == b'#' {
                i += 1;
            }
            let integer_digits = (i - int_start + if leading_zeros { 1 } else { 0 }) as u32;
            // Optional `.` followed by decimal `#`s.
            let mut decimal_digits: Option<u32> = None;
            if i < bytes.len() && bytes[i] == b'.' {
                let dot_pos = i;
                i += 1;
                let dec_start = i;
                while i < bytes.len() && bytes[i] == b'#' {
                    i += 1;
                }
                if i == dec_start {
                    // `.` with no trailing `#`s: not part of the field.  Back up to before the dot.
                    i = dot_pos;
                } else {
                    decimal_digits = Some((i - dec_start) as u32);
                }
            }
            // Must have at least one digit somewhere.
            if integer_digits == 0 && decimal_digits.is_none() {
                return None;
            }
            Some((FieldKind::Numeric { integer_digits, decimal_digits, leading_zeros, caret }, i - start))
        }
        b'.' => {
            // `@.###` — no integer digits, decimals only.
            let mut i = after + 1;
            let dec_start = i;
            while i < bytes.len() && bytes[i] == b'#' {
                i += 1;
            }
            if i == dec_start {
                return None;
            }
            Some((FieldKind::Numeric { integer_digits: 0, decimal_digits: Some((i - dec_start) as u32), leading_zeros: false, caret }, i - start))
        }
        _ => None,
    }
}
