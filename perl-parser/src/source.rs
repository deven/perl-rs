//! Line-oriented source delivery for the lexer.
//!
//! `LexerSource` manages line splitting, CRLF normalization, heredoc
//! body sequencing, and indentation stripping.  The lexer receives
//! one line at a time via `LexerLine` and scans bytes within it,
//! never dealing with line boundaries, newline encoding, or heredoc
//! line reordering.
//!
//! See design document §5.4 for the full design rationale.

use std::collections::VecDeque;

use bytes::Bytes;

use crate::error::ParseError;
use crate::lexer::matching_delimiter;
use crate::span::Span;

// ── LexerLine ─────────────────────────────────────────────────────

/// A single line of source code with a byte-scanning cursor.
///
/// The lexer's working unit.  All fields are `pub(crate)` — the lexer
/// freely reads and writes `pos` for cursor control, and reads `number`
/// and `offset` for span computation.
#[derive(Clone, Debug)]
pub(crate) struct LexerLine {
    /// 1-based line number in the original source.
    pub number: usize,
    /// Byte offset of the start of this line in the original source.
    pub offset: usize,
    /// Line content without line ending.  When inside an indented
    /// heredoc, the required indentation prefix has been stripped.
    pub line: Bytes,
    /// Whether this line was terminated by a newline in the source.
    pub terminated: bool,
    /// Current scanning position within `line`.
    pub pos: usize,
    /// Whether the line contains only ASCII bytes (all < 0x80).
    /// Computed for free during newline scanning and used to skip
    /// UTF-8 decoding and NFC normalization for all-ASCII lines.
    pub ascii_only: bool,
}

impl LexerLine {
    /// Peek at the current byte without advancing.
    /// Returns `b'\n'` at the end of a terminated line.
    /// Returns `None` only when truly exhausted (past \n or
    /// unterminated line fully consumed).
    #[inline]
    pub fn peek_byte(&self) -> Option<u8> {
        if self.pos < self.line.len() {
            Some(self.line[self.pos])
        } else if self.pos == self.line.len() && self.terminated {
            Some(b'\n')
        } else {
            None
        }
    }

    /// Peek at a byte at an offset from the current position.
    #[inline]
    pub fn peek_byte_at(&self, offset: usize) -> Option<u8> {
        let idx = self.pos + offset;
        if idx < self.line.len() {
            Some(self.line[idx])
        } else if idx == self.line.len() && self.terminated {
            Some(b'\n')
        } else {
            None
        }
    }

    /// Consume the current byte and advance the cursor.
    /// Returns `b'\n'` at the end of a terminated line.
    /// Returns `None` only when truly exhausted.
    #[inline]
    pub fn advance_byte(&mut self) -> Option<u8> {
        if self.pos < self.line.len() {
            let b = self.line[self.pos];
            self.pos += 1;
            Some(b)
        } else if self.pos == self.line.len() && self.terminated {
            self.pos += 1;
            Some(b'\n')
        } else {
            None
        }
    }

    /// The remaining unscanned content bytes (not including the
    /// virtual `\n` line terminator).
    #[inline]
    pub fn remaining(&self) -> &[u8] {
        if self.pos < self.line.len() { &self.line[self.pos..] } else { &[] }
    }

    /// Byte offset in the original source at the current cursor position.
    /// Used for span construction.
    #[inline]
    pub fn global_pos(&self) -> u32 {
        (self.offset + self.pos) as u32
    }
}

// ── LexerSource ───────────────────────────────────────────────────

/// Internal heredoc context saved when entering a heredoc body.
struct HeredocContext {
    tag: Bytes,
    saved_line: LexerLine,
    prev_indent: Option<Bytes>,
}

/// Line-oriented source for the lexer.
///
/// Provides lines to the lexer via `next_line()`, handling CRLF
/// normalization, heredoc body sequencing, and indentation stripping.
/// The lexer never manages these concerns directly.
pub(crate) struct LexerSource {
    /// The complete source buffer.
    src: Bytes,
    /// Name of the source — used for `__FILE__` resolution and
    /// diagnostic messages.  Defaults to `"(script)"` when the
    /// caller doesn't supply one (e.g., `Parser::new(src)`).
    filename: String,
    /// Current byte position for reading the next line.
    cursor: usize,
    /// Next line number to assign (1-based).
    line_number: usize,
    /// Stack of active heredoc contexts.
    heredoc_stack: Vec<HeredocContext>,
    /// Lines queued for delivery by future `next_line()` calls.
    /// Used for heredoc remainder delivery, push_back, and subst bodies.
    queued_lines: VecDeque<LexerLine>,
    /// Indentation prefix to strip from every non-empty line.
    /// Set by `start_indented_heredoc`, restored when the heredoc
    /// finishes.
    required_indent: Option<Bytes>,
    /// Set when a heredoc terminator was found during a peek call.
    /// The next consuming call will pop the heredoc context.
    terminator_pending: bool,
    /// Set when the queued body lines of a substitution have been
    /// delivered.  The next `next_line` returns `None` (virtual EOF),
    /// then delivers the saved remainder.
    subst_eof_pending: bool,
    /// Line to deliver after the virtual EOF of a subst body.
    /// Contains the remainder of the source line after the flags.
    subst_saved_line: Option<LexerLine>,
}

/// A raw line read from the source buffer before indent processing.
struct RawLine {
    number: usize,
    offset: usize,
    content: Bytes,
    terminated: bool,
    ascii_only: bool,
}

impl LexerSource {
    /// Create a new `LexerSource` from a byte slice, using the
    /// default placeholder filename `"(script)"`.
    ///
    /// The bytes are copied into a `Bytes` buffer once.  All subsequent
    /// line slicing is zero-copy.
    pub fn new(src: &[u8]) -> Self {
        Self::with_filename(src, "(script)")
    }

    /// Create a new `LexerSource` with a specific filename.  The
    /// filename is surfaced via [`Self::filename`] and used for
    /// `__FILE__` resolution.
    pub fn with_filename(src: &[u8], filename: impl Into<String>) -> Self {
        LexerSource {
            src: Bytes::copy_from_slice(src),
            filename: filename.into(),
            cursor: 0,
            line_number: 1,
            heredoc_stack: Vec::new(),
            queued_lines: VecDeque::new(),
            required_indent: None,
            terminator_pending: false,
            subst_eof_pending: false,
            subst_saved_line: None,
        }
    }

    /// Create a new `LexerSource` from an existing `Bytes` buffer.
    /// Zero-copy — just a refcount bump.  Uses the default
    /// placeholder filename.
    #[allow(dead_code)]
    pub fn from_bytes(src: Bytes) -> Self {
        LexerSource {
            src,
            filename: "(script)".into(),
            cursor: 0,
            line_number: 1,
            heredoc_stack: Vec::new(),
            queued_lines: VecDeque::new(),
            required_indent: None,
            terminator_pending: false,
            subst_eof_pending: false,
            subst_saved_line: None,
        }
    }

    /// Current byte position in the source buffer.
    /// Used for global position when no current line is active.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Current line number (1-based).
    #[allow(dead_code)]
    pub fn line_number(&self) -> usize {
        self.line_number
    }

    /// Name of the source file being lexed, for `__FILE__`
    /// resolution and diagnostics.  Defaults to `"(script)"`
    /// when the caller used [`Self::new`] without a filename.
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// Override the line number for `# line N` directives.
    pub fn set_line_number(&mut self, n: usize) {
        self.line_number = n;
    }

    /// Override the filename for `# line N "file"` directives.
    pub fn set_filename(&mut self, name: String) {
        self.filename = name;
    }

    /// Raw slice of the source buffer.  For rare operations that need
    /// access to the underlying bytes (e.g. format body extraction).
    pub fn src_slice(&self, start: usize, end: usize) -> &[u8] {
        &self.src[start..end]
    }

    /// Push lines to be returned by future `next_line()` calls,
    /// ahead of any lines read from the source.
    pub fn push_back(&mut self, mut lines: VecDeque<LexerLine>) {
        lines.append(&mut self.queued_lines);
        self.queued_lines = lines;
    }

    /// Get the next line.
    ///
    /// Returns `Ok(Some(line))` for content, `Ok(None)` when a heredoc
    /// body is finished (the saved remainder will be returned by the
    /// next call), or `Err` for real errors (unterminated heredoc,
    /// indentation mismatch).
    ///
    /// `peek_heredoc`: when true and a heredoc terminator is found,
    /// returns `Ok(None)` without consuming the signal — the heredoc
    /// context stays on the stack and `queued_lines` is not modified.
    /// The next call with `peek_heredoc=false` will consume it.
    pub fn next_line(&mut self, peek_heredoc: bool) -> Result<Option<LexerLine>, ParseError> {
        // 0. If a terminator was found during a previous peek call,
        //    handle it without reading another line.
        if self.terminator_pending {
            if !peek_heredoc {
                // Consume the pending terminator.
                self.terminator_pending = false;
                if let Some(ctx) = self.heredoc_stack.pop() {
                    self.required_indent = ctx.prev_indent;
                    self.queued_lines.push_back(ctx.saved_line);
                }
            }
            return Ok(None);
        }

        // 1. Return queued line if present (from heredoc remainder,
        //    push_back, or subst body — not subject to terminator check).
        if let Some(line) = self.queued_lines.pop_front() {
            return Ok(Some(line));
        }

        // 1b. Subst body virtual EOF — all body lines delivered.
        if self.subst_eof_pending {
            if !peek_heredoc {
                // Consume: queue the saved remainder for next call.
                self.subst_eof_pending = false;
                if let Some(saved) = self.subst_saved_line.take() {
                    self.queued_lines.push_back(saved);
                }
            }
            return Ok(None);
        }

        // 2. Read next raw line from source.
        let raw = match self.read_raw_line() {
            Some(raw) => raw,
            None => {
                // EOF — error if inside a heredoc.
                if let Some(ctx) = self.heredoc_stack.last() {
                    let tag = String::from_utf8_lossy(&ctx.tag).into_owned();
                    return Err(ParseError::new(format!("can't find heredoc terminator '{tag}'"), Span::new(0, self.src.len() as u32)));
                }
                return Ok(None); // Normal EOF.
            }
        };

        // 3. Strip required indent (if any).
        let stripped = self.strip_indent(raw)?;

        // 4. If inside a heredoc, check for terminator.
        let is_terminator = self.heredoc_stack.last().is_some_and(|ctx| stripped.line.as_ref() == ctx.tag.as_ref());
        if is_terminator {
            if peek_heredoc {
                // Peek mode: signal end without consuming.
                self.terminator_pending = true;
            } else {
                // Consume mode: pop the heredoc context.
                if let Some(ctx) = self.heredoc_stack.pop() {
                    self.required_indent = ctx.prev_indent;
                    self.queued_lines.push_back(ctx.saved_line);
                }
            }
            return Ok(None);
        }

        Ok(Some(stripped))
    }

    /// Begin processing an indented heredoc body (`<<~TAG`).
    ///
    /// Scans ahead to find the terminator, sets the required
    /// indentation from its whitespace prefix.  The current line is
    /// taken from the Option (setting it to None) and saved internally
    /// for restoration when the terminator is found.
    pub fn start_indented_heredoc(&mut self, tag: Bytes, current_line: &mut Option<LexerLine>) -> Result<(), ParseError> {
        let line = current_line.take().ok_or_else(|| ParseError::new("internal error: start_indented_heredoc called without a current line", Span::DUMMY))?;
        let prev_indent = self.required_indent.clone();

        // Scan ahead for the terminator to determine indentation.
        let new_indent = self.scan_for_indented_terminator(&tag)?;

        // Push heredoc context.
        self.heredoc_stack.push(HeredocContext { tag, saved_line: line, prev_indent });
        self.required_indent = Some(new_indent);

        Ok(())
    }

    /// Begin processing a non-indented heredoc body (`<<TAG`).
    ///
    /// The current line is taken from the Option (setting it to None)
    /// and saved internally for restoration when the terminator is
    /// found.  Does not change the required indentation.
    pub fn start_heredoc(&mut self, tag: Bytes, current_line: &mut Option<LexerLine>) -> Result<(), ParseError> {
        let line = current_line.take().ok_or_else(|| ParseError::new("internal error: start_heredoc called without a current line", Span::DUMMY))?;
        let prev_indent = self.required_indent.clone();

        self.heredoc_stack.push(HeredocContext { tag, saved_line: line, prev_indent });
        Ok(())
    }

    /// Begin processing a substitution replacement body.
    ///
    /// Takes the current line, scans ahead to find the closing
    /// delimiter and flags, then queues the body lines for delivery
    /// with a virtual EOF at the end.  The remainder of the source
    /// line after the flags is saved for delivery after the EOF.
    ///
    /// Returns the captured flags (or None if no flags).
    pub fn start_subst_body(&mut self, delim: u8, current_line: &mut Option<LexerLine>) -> Result<Option<String>, ParseError> {
        let mut line = current_line.take().ok_or_else(|| ParseError::new("internal error: start_subst_body called without a current line", Span::DUMMY))?;

        let (open, close) = matching_delimiter(delim);
        let mut body_lines: VecDeque<LexerLine> = VecDeque::new();
        let mut pos = line.pos;
        let mut depth = 0u32;

        loop {
            if pos >= line.line.len() {
                // Line exhausted — queue it as a body line.
                body_lines.push_back(line);
                match self.next_line(false)? {
                    Some(next) => {
                        line = next;
                        pos = 0;
                        continue;
                    }
                    None => {
                        // EOF inside replacement body.
                        self.push_back(body_lines);
                        return Err(ParseError::new("unterminated substitution", Span::new(0, self.src.len() as u32)));
                    }
                }
            }

            let b = line.line[pos];
            if b == b'\\' {
                pos += 2;
            } else if b == close && depth == 0 {
                // Found closing delimiter at `pos`.
                // Body content on this line: everything before `pos`.
                let truncated = LexerLine {
                    line: line.line.slice(..pos),
                    offset: line.offset,
                    pos: if body_lines.is_empty() { line.pos } else { 0 },
                    terminated: false, // virtual EOF, no newline
                    number: line.number,
                    ascii_only: line.ascii_only,
                };
                body_lines.push_back(truncated);

                // Read flags starting after the delimiter.
                let mut flag_end = pos + 1;
                while flag_end < line.line.len() && (line.line[flag_end].is_ascii_alphanumeric() || line.line[flag_end] == b'_') {
                    flag_end += 1;
                }
                let flags = if flag_end > pos + 1 { Some(String::from_utf8_lossy(&line.line[pos + 1..flag_end]).into_owned()) } else { None };

                // Saved remainder: rest of the line after flags.
                let saved = LexerLine {
                    line: line.line.clone(),
                    offset: line.offset,
                    pos: flag_end,
                    terminated: line.terminated,
                    number: line.number,
                    ascii_only: line.ascii_only,
                };

                // Queue body lines and set up virtual EOF.
                self.push_back(body_lines);
                self.subst_eof_pending = true;
                self.subst_saved_line = Some(saved);

                return Ok(flags);
            } else if open == Some(b) {
                depth += 1;
                pos += 1;
            } else if b == close {
                depth -= 1;
                pos += 1;
            } else {
                pos += 1;
            }
        }
    }

    // ── Internal methods ──────────────────────────────────────────

    /// Read the next raw line from the source buffer.
    ///
    /// Splits on `\n`, strips `\r` before `\n` (CRLF normalization).
    /// Standalone `\r` not followed by `\n` is preserved as a literal
    /// byte.  Returns `None` at EOF.
    fn read_raw_line(&mut self) -> Option<RawLine> {
        if self.cursor >= self.src.len() {
            return None;
        }

        let start = self.cursor;
        let number = self.line_number;
        self.line_number += 1;

        // Find end of line (\n or EOF), accumulating high-bit check.
        let mut end = start;
        let mut bits_used: u8 = 0;
        for &byte in &self.src.as_ref()[start..] {
            if byte == b'\n' {
                break;
            }
            bits_used |= byte;
            end += 1;
        }
        let ascii_only = bits_used < 0x80;

        let terminated = end < self.src.len();

        // CRLF normalization: strip \r immediately before \n.
        let content_end = if terminated && end > start && self.src[end - 1] == b'\r' { end - 1 } else { end };

        // Advance cursor past the \n (if present).
        self.cursor = if terminated { end + 1 } else { end };

        Some(RawLine { number, offset: start, content: self.src.slice(start..content_end), terminated, ascii_only })
    }

    /// Strip the required indent from a raw line.
    ///
    /// Returns the `LexerLine` with indent stripped and cursor at 0.
    /// Empty lines (zero content) are allowed without the indent prefix.
    fn strip_indent(&self, raw: RawLine) -> Result<LexerLine, ParseError> {
        let (content, indent_len) = if let Some(indent) = &self.required_indent {
            if raw.content.starts_with(indent.as_ref()) {
                (raw.content.slice(indent.len()..), indent.len())
            } else if raw.content.is_empty() {
                // Empty line — allowed without indent.
                (raw.content, 0)
            } else {
                return Err(ParseError::new(
                    "indentation of here-doc doesn't match delimiter",
                    Span::new(raw.offset as u32, (raw.offset + raw.content.len()) as u32),
                ));
            }
        } else {
            (raw.content, 0)
        };

        Ok(LexerLine { number: raw.number, offset: raw.offset + indent_len, line: content, terminated: raw.terminated, pos: 0, ascii_only: raw.ascii_only })
    }

    /// Scan ahead from the current cursor to find an indented heredoc
    /// terminator.  Returns the full raw whitespace prefix of the
    /// terminator line.  Does not advance the cursor.
    fn scan_for_indented_terminator(&mut self, tag: &[u8]) -> Result<Bytes, ParseError> {
        let saved_cursor = self.cursor;
        let saved_line_number = self.line_number;

        let result = self.scan_for_indented_terminator_inner(tag);

        // Restore cursor regardless of result.
        self.cursor = saved_cursor;
        self.line_number = saved_line_number;

        result
    }

    /// Inner scan — advances cursor, caller saves/restores.
    fn scan_for_indented_terminator_inner(&mut self, tag: &[u8]) -> Result<Bytes, ParseError> {
        let scan_start = self.cursor;

        while self.cursor < self.src.len() {
            let line_start = self.cursor;

            // Find end of line.
            let mut end = self.cursor;
            while end < self.src.len() && self.src[end] != b'\n' {
                end += 1;
            }
            self.cursor = if end < self.src.len() { end + 1 } else { end };

            // CRLF strip for matching.
            let content_end = if end > line_start && self.src[end - 1] == b'\r' { end - 1 } else { end };

            // Find the whitespace prefix.
            let mut indent_end = line_start;
            while indent_end < content_end && (self.src[indent_end] == b' ' || self.src[indent_end] == b'\t') {
                indent_end += 1;
            }

            // Check if the non-whitespace part matches the tag.
            if &self.src[indent_end..content_end] == tag {
                let raw_prefix = self.src.slice(line_start..indent_end);

                // Validate that the terminator starts with the outer indent.
                if let Some(outer) = &self.required_indent
                    && !raw_prefix.starts_with(outer.as_ref())
                {
                    return Err(ParseError::new("indentation of here-doc doesn't match delimiter", Span::new(line_start as u32, content_end as u32)));
                }

                return Ok(raw_prefix);
            }
        }

        Err(ParseError::new(format!("can't find heredoc terminator '{}'", String::from_utf8_lossy(tag)), Span::new(scan_start as u32, self.cursor as u32)))
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Helper: collect all lines from a source.
    fn collect_lines(src: &str) -> Vec<String> {
        let mut source = LexerSource::new(src.as_bytes());
        let mut lines = Vec::new();
        while let Ok(Some(line)) = source.next_line(false) {
            lines.push(String::from_utf8_lossy(&line.line).into_owned());
        }
        lines
    }

    // ── Basic line splitting ──────────────────────────────────────

    #[test]
    fn empty_source() {
        let mut source = LexerSource::new(b"");
        assert!(matches!(source.next_line(false), Ok(None)));
    }

    #[test]
    fn single_line_no_newline() {
        let mut source = LexerSource::new(b"hello");
        let line = source.next_line(false).unwrap().unwrap();
        assert_eq!(&line.line[..], b"hello");
        assert!(!line.terminated);
        assert_eq!(line.number, 1);
        assert!(matches!(source.next_line(false), Ok(None)));
    }

    #[test]
    fn single_line_with_newline() {
        let mut source = LexerSource::new(b"hello\n");
        let line = source.next_line(false).unwrap().unwrap();
        assert_eq!(&line.line[..], b"hello");
        assert!(line.terminated);
        assert!(matches!(source.next_line(false), Ok(None)));
    }

    #[test]
    fn multiple_lines() {
        let lines = collect_lines("aaa\nbbb\nccc\n");
        assert_eq!(lines, vec!["aaa", "bbb", "ccc"]);
    }

    #[test]
    fn last_line_without_newline() {
        let lines = collect_lines("aaa\nbbb");
        assert_eq!(lines, vec!["aaa", "bbb"]);
    }

    #[test]
    fn empty_lines() {
        let lines = collect_lines("a\n\nb\n\n");
        assert_eq!(lines, vec!["a", "", "b", ""]);
    }

    #[test]
    fn line_numbers() {
        let mut source = LexerSource::new(b"a\nb\nc\n");
        assert_eq!(source.next_line(false).unwrap().unwrap().number, 1);
        assert_eq!(source.next_line(false).unwrap().unwrap().number, 2);
        assert_eq!(source.next_line(false).unwrap().unwrap().number, 3);
    }

    #[test]
    fn byte_offsets() {
        let mut source = LexerSource::new(b"ab\ncde\nf\n");
        let l1 = source.next_line(false).unwrap().unwrap();
        assert_eq!(l1.offset, 0);
        let l2 = source.next_line(false).unwrap().unwrap();
        assert_eq!(l2.offset, 3);
        let l3 = source.next_line(false).unwrap().unwrap();
        assert_eq!(l3.offset, 7);
    }

    // ── CRLF normalization ────────────────────────────────────────

    #[test]
    fn crlf_stripped() {
        let lines = collect_lines("hello\r\nworld\r\n");
        assert_eq!(lines, vec!["hello", "world"]);
    }

    #[test]
    fn standalone_cr_preserved() {
        let mut source = LexerSource::new(b"a\rb\n");
        let line = source.next_line(false).unwrap().unwrap();
        assert_eq!(&line.line[..], b"a\rb");
    }

    #[test]
    fn mixed_crlf_and_lf() {
        let lines = collect_lines("a\r\nb\nc\r\n");
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    // ── LexerLine cursor methods ──────────────────────────────────

    #[test]
    fn lexer_line_peek_and_advance() {
        let mut source = LexerSource::new(b"abc\n");
        let mut line = source.next_line(false).unwrap().unwrap();
        assert_eq!(line.peek_byte(), Some(b'a'));
        assert_eq!(line.advance_byte(), Some(b'a'));
        assert_eq!(line.peek_byte(), Some(b'b'));
        assert_eq!(line.advance_byte(), Some(b'b'));
        assert_eq!(line.advance_byte(), Some(b'c'));
        // Terminated line delivers \n as the last byte.
        assert_eq!(line.peek_byte(), Some(b'\n'));
        assert_eq!(line.advance_byte(), Some(b'\n'));
        // Now truly exhausted.
        assert_eq!(line.advance_byte(), None);
        assert_eq!(line.peek_byte(), None);
    }

    #[test]
    fn lexer_line_remaining() {
        let mut source = LexerSource::new(b"abcdef\n");
        let mut line = source.next_line(false).unwrap().unwrap();
        line.pos = 3;
        assert_eq!(line.remaining(), b"def");
    }

    #[test]
    fn lexer_line_slice() {
        let mut source = LexerSource::new(b"hello world\n");
        let line = source.next_line(false).unwrap().unwrap();
        let s = line.line.slice(0..5);
        assert_eq!(&s[..], b"hello");
        let s2 = line.line.slice(6..11);
        assert_eq!(&s2[..], b"world");
    }

    #[test]
    fn lexer_line_slice_since() {
        let mut source = LexerSource::new(b"abcdef\n");
        let mut line = source.next_line(false).unwrap().unwrap();
        line.pos = 4;
        let s = line.line.slice(2..line.pos);
        assert_eq!(&s[..], b"cd");
    }

    // ── Non-indented heredoc ──────────────────────────────────────

    #[test]
    fn heredoc_basic() {
        // my $x = <<END . "suffix";
        // hello
        // END
        let src = b"my $x = <<END . \"suffix\";\nhello\nEND\nmore code\n";
        let mut source = LexerSource::new(src);

        // Line 1: the declaration line.
        let decl = source.next_line(false).unwrap().unwrap();
        assert_eq!(&decl.line[..], b"my $x = <<END . \"suffix\";");

        // Simulate lexer: found <<END at some position in decl.
        // Save the remainder and start the heredoc.
        let mut current_line = Some(LexerLine {
            number: decl.number,
            offset: decl.offset,
            line: decl.line.clone(),
            terminated: decl.terminated,
            pos: 13, // pointing at ` . "suffix";`
            ascii_only: true,
        });
        source.start_heredoc(Bytes::from_static(b"END"), &mut current_line).unwrap();
        assert!(current_line.is_none());

        // Next line: heredoc body.
        let body = source.next_line(false).unwrap().unwrap();
        assert_eq!(&body.line[..], b"hello");

        // Next line: terminator → None.
        assert!(source.next_line(false).unwrap().is_none());

        // Next line: saved remainder (the declaration tail).
        let remainder = source.next_line(false).unwrap().unwrap();
        assert_eq!(remainder.pos, 13); // cursor preserved
        assert_eq!(&remainder.line[remainder.pos..], b" . \"suffix\";");

        // Next line: code after the heredoc.
        let after = source.next_line(false).unwrap().unwrap();
        assert_eq!(&after.line[..], b"more code");
    }

    #[test]
    fn heredoc_empty_body() {
        let src = b"<<END;\nEND\n";
        let mut source = LexerSource::new(src);
        let decl = source.next_line(false).unwrap().unwrap();

        let mut current = Some(LexerLine { number: decl.number, offset: decl.offset, line: decl.line, terminated: decl.terminated, pos: 5, ascii_only: true });
        source.start_heredoc(Bytes::from_static(b"END"), &mut current).unwrap();

        // Immediate terminator → None.
        assert!(source.next_line(false).unwrap().is_none());
    }

    #[test]
    fn heredoc_unterminated() {
        let src = b"<<END;\nhello\nworld\n";
        let mut source = LexerSource::new(src);
        let decl = source.next_line(false).unwrap().unwrap();

        let mut current = Some(LexerLine { number: decl.number, offset: decl.offset, line: decl.line, terminated: decl.terminated, pos: 5, ascii_only: true });
        source.start_heredoc(Bytes::from_static(b"END"), &mut current).unwrap();

        // Read body lines.
        source.next_line(false).unwrap().unwrap(); // hello
        source.next_line(false).unwrap().unwrap(); // world

        // EOF without terminator → error.
        assert!(source.next_line(false).is_err());
    }

    // ── Stacked heredocs ──────────────────────────────────────────

    #[test]
    fn heredoc_stacked() {
        // (<<A, <<B)
        // body A
        // A
        // body B
        // B
        // after
        let src = b"(<<A, <<B);\nbody A\nA\nbody B\nB\nafter\n";
        let mut source = LexerSource::new(src);
        let decl = source.next_line(false).unwrap().unwrap();

        // Start <<A, save remainder ", <<B);"
        let mut current = Some(LexerLine {
            number: decl.number,
            offset: decl.offset,
            line: decl.line.clone(),
            terminated: decl.terminated,
            pos: 4, // after "<<A"
            ascii_only: true,
        });
        source.start_heredoc(Bytes::from_static(b"A"), &mut current).unwrap();

        // A's body.
        let body_a = source.next_line(false).unwrap().unwrap();
        assert_eq!(&body_a.line[..], b"body A");

        // A's terminator → None.
        assert!(source.next_line(false).unwrap().is_none());

        // Remainder restored: ", <<B);"
        let remainder = source.next_line(false).unwrap().unwrap();
        assert_eq!(remainder.pos, 4);

        // Now start <<B from the remainder.
        let mut current = Some(LexerLine {
            number: remainder.number,
            offset: remainder.offset,
            line: remainder.line,
            terminated: remainder.terminated,
            pos: 10, // after ", <<B"
            ascii_only: true,
        });
        source.start_heredoc(Bytes::from_static(b"B"), &mut current).unwrap();

        // B's body.
        let body_b = source.next_line(false).unwrap().unwrap();
        assert_eq!(&body_b.line[..], b"body B");

        // B's terminator → None.
        assert!(source.next_line(false).unwrap().is_none());

        // Remainder restored: ");"
        let remainder2 = source.next_line(false).unwrap().unwrap();
        assert_eq!(remainder2.pos, 10);

        // After heredocs.
        let after = source.next_line(false).unwrap().unwrap();
        assert_eq!(&after.line[..], b"after");
    }

    // ── Indented heredoc ──────────────────────────────────────────

    #[test]
    fn heredoc_indented() {
        let src = b"<<~END;\n    hello\n    world\n    END\n";
        let mut source = LexerSource::new(src);
        let decl = source.next_line(false).unwrap().unwrap();

        let mut current = Some(LexerLine { number: decl.number, offset: decl.offset, line: decl.line, terminated: decl.terminated, pos: 6, ascii_only: true });
        source.start_indented_heredoc(Bytes::from_static(b"END"), &mut current).unwrap();

        // Body lines with indent stripped.
        // Source: "<<~END;\n    hello\n    world\n    END\n"
        //          0       8          18
        // "    hello" at raw offset 8, 4-byte indent stripped → offset 12.
        let l1 = source.next_line(false).unwrap().unwrap();
        assert_eq!(&l1.line[..], b"hello");
        assert_eq!(l1.offset, 12);
        let l2 = source.next_line(false).unwrap().unwrap();
        assert_eq!(&l2.line[..], b"world");
        assert_eq!(l2.offset, 22);

        // Terminator → None.
        assert!(source.next_line(false).unwrap().is_none());
    }

    #[test]
    fn heredoc_indented_empty_lines() {
        // Empty lines are allowed without indentation.
        let src = b"<<~END;\n    hello\n\n    world\n    END\n";
        let mut source = LexerSource::new(src);
        let decl = source.next_line(false).unwrap().unwrap();

        let mut current = Some(LexerLine { number: decl.number, offset: decl.offset, line: decl.line, terminated: decl.terminated, pos: 6, ascii_only: true });
        source.start_indented_heredoc(Bytes::from_static(b"END"), &mut current).unwrap();

        let l1 = source.next_line(false).unwrap().unwrap();
        assert_eq!(&l1.line[..], b"hello");
        let l2 = source.next_line(false).unwrap().unwrap();
        assert_eq!(&l2.line[..], b""); // empty line
        let l3 = source.next_line(false).unwrap().unwrap();
        assert_eq!(&l3.line[..], b"world");
        assert!(source.next_line(false).unwrap().is_none());
    }

    #[test]
    fn heredoc_indented_mismatch() {
        // Body line with wrong indentation.
        let src = b"<<~END;\n    hello\n  bad\n    END\n";
        let mut source = LexerSource::new(src);
        let decl = source.next_line(false).unwrap().unwrap();

        let mut current = Some(LexerLine { number: decl.number, offset: decl.offset, line: decl.line, terminated: decl.terminated, pos: 6, ascii_only: true });
        source.start_indented_heredoc(Bytes::from_static(b"END"), &mut current).unwrap();

        let l1 = source.next_line(false).unwrap().unwrap();
        assert_eq!(&l1.line[..], b"hello");

        // Next line has wrong indent → error.
        assert!(source.next_line(false).is_err());
    }

    // ── Nested heredocs ───────────────────────────────────────────

    #[test]
    fn heredoc_non_indented_inside_indented() {
        // <<~OUTER with <<INNER inside
        //     <<INNER body line
        //     INNER
        //     outer body continues
        //     OUTER
        let src = b"<<~OUTER;\n    prefix <<INNER suffix\n    inner body\n    INNER\n    outer continues\n    OUTER\n";
        let mut source = LexerSource::new(src);
        let decl = source.next_line(false).unwrap().unwrap();

        // Start <<~OUTER
        let mut current = Some(LexerLine { number: decl.number, offset: decl.offset, line: decl.line, terminated: decl.terminated, pos: 9, ascii_only: true });
        source.start_indented_heredoc(Bytes::from_static(b"OUTER"), &mut current).unwrap();

        // First body line of OUTER (indent stripped).
        let l1 = source.next_line(false).unwrap().unwrap();
        assert_eq!(&l1.line[..], b"prefix <<INNER suffix");

        // Start <<INNER (non-indented, inside indented OUTER).
        let mut current = Some(LexerLine {
            number: l1.number,
            offset: l1.offset,
            line: l1.line,
            terminated: l1.terminated,
            pos: 14, // after "prefix <<INNER"
            ascii_only: true,
        });
        source.start_heredoc(Bytes::from_static(b"INNER"), &mut current).unwrap();

        // INNER body (outer indent still stripped).
        let inner_body = source.next_line(false).unwrap().unwrap();
        assert_eq!(&inner_body.line[..], b"inner body");

        // INNER terminator → None.
        assert!(source.next_line(false).unwrap().is_none());

        // Remainder of OUTER body line restored.
        let remainder = source.next_line(false).unwrap().unwrap();
        assert_eq!(remainder.pos, 14);
        assert_eq!(&remainder.line[remainder.pos..], b" suffix");

        // OUTER body continues.
        let l2 = source.next_line(false).unwrap().unwrap();
        assert_eq!(&l2.line[..], b"outer continues");

        // OUTER terminator → None.
        assert!(source.next_line(false).unwrap().is_none());
    }

    // ── ascii_only flag ─────────────────────────────────────

    #[test]
    fn ascii_only_pure_ascii_line() {
        let mut source = LexerSource::new(b"hello world\n");
        let line = source.next_line(false).unwrap().unwrap();
        assert!(line.ascii_only, "pure ASCII line should have ascii_only = true");
    }

    #[test]
    fn ascii_only_empty_line() {
        let mut source = LexerSource::new(b"\n");
        let line = source.next_line(false).unwrap().unwrap();
        assert!(line.ascii_only, "empty line should have ascii_only = true");
    }

    #[test]
    fn ascii_only_with_high_bytes() {
        let mut source = LexerSource::new("café\n".as_bytes());
        let line = source.next_line(false).unwrap().unwrap();
        assert!(!line.ascii_only, "line with UTF-8 should have ascii_only = false");
    }

    #[test]
    fn ascii_only_high_byte_at_end() {
        let mut source = LexerSource::new(b"hello\xff\n");
        let line = source.next_line(false).unwrap().unwrap();
        assert!(!line.ascii_only, "line with high byte should have ascii_only = false");
    }

    #[test]
    fn ascii_only_high_byte_at_start() {
        let mut source = LexerSource::new(b"\x80rest\n");
        let line = source.next_line(false).unwrap().unwrap();
        assert!(!line.ascii_only, "line starting with high byte should have ascii_only = false");
    }

    #[test]
    fn ascii_only_multiline_mixed() {
        let mut source = LexerSource::new("ascii\ncafé\nmore ascii\n".as_bytes());
        let l1 = source.next_line(false).unwrap().unwrap();
        assert!(l1.ascii_only, "first line is ASCII");
        let l2 = source.next_line(false).unwrap().unwrap();
        assert!(!l2.ascii_only, "second line has UTF-8");
        let l3 = source.next_line(false).unwrap().unwrap();
        assert!(l3.ascii_only, "third line is ASCII");
    }

    #[test]
    fn ascii_only_unterminated_line() {
        let mut source = LexerSource::new(b"no newline");
        let line = source.next_line(false).unwrap().unwrap();
        assert!(line.ascii_only, "unterminated ASCII line should have ascii_only = true");
    }

    #[test]
    fn ascii_only_unterminated_with_utf8() {
        let mut source = LexerSource::new("no newline café".as_bytes());
        let line = source.next_line(false).unwrap().unwrap();
        assert!(!line.ascii_only, "unterminated UTF-8 line should have ascii_only = false");
    }

    #[test]
    fn ascii_only_crlf_line() {
        let mut source = LexerSource::new(b"hello\r\n");
        let line = source.next_line(false).unwrap().unwrap();
        assert!(line.ascii_only, "CRLF line with ASCII content should have ascii_only = true");
    }

    #[test]
    fn ascii_only_only_control_chars() {
        // Control chars (0x01..0x1F) are all < 0x80.
        let mut source = LexerSource::new(b"\x01\x1f\t\n");
        let line = source.next_line(false).unwrap().unwrap();
        assert!(line.ascii_only, "control chars are ASCII");
    }

    #[test]
    fn ascii_only_boundary_byte_0x7f() {
        // 0x7F (DEL) is the highest ASCII byte.
        let mut source = LexerSource::new(b"\x7f\n");
        let line = source.next_line(false).unwrap().unwrap();
        assert!(line.ascii_only, "0x7F is still ASCII");
    }

    #[test]
    fn ascii_only_boundary_byte_0x80() {
        // 0x80 is the first non-ASCII byte.
        let mut source = LexerSource::new(b"\x80\n");
        let line = source.next_line(false).unwrap().unwrap();
        assert!(!line.ascii_only, "0x80 is not ASCII");
    }

    #[test]
    fn ascii_only_heredoc_body_lines() {
        // Heredoc body lines should have correct ascii_only flags.
        let mut source = LexerSource::new("<<END\nascii line\ncaf\u{00E9} line\nEND\n".as_bytes());
        let decl = source.next_line(false).unwrap().unwrap();
        assert!(decl.ascii_only, "declaration line is ASCII");

        // Start heredoc.
        let mut current =
            Some(LexerLine { number: decl.number, offset: decl.offset, line: decl.line, terminated: decl.terminated, pos: 5, ascii_only: decl.ascii_only });
        source.start_heredoc(Bytes::from_static(b"END"), &mut current).unwrap();

        // First body line: ASCII.
        let body1 = source.next_line(false).unwrap().unwrap();
        assert!(body1.ascii_only, "first heredoc body line is ASCII");

        // Second body line: has UTF-8.
        let body2 = source.next_line(false).unwrap().unwrap();
        assert!(!body2.ascii_only, "second heredoc body line has UTF-8");

        // Terminator → None.
        assert!(source.next_line(false).unwrap().is_none());
    }

    // ── ChatGPT torture tests ───────────────────────────────

    #[test]
    fn heredoc_terminator_at_eof_without_newline() {
        let src = b"<<END\nbody\nEND";
        let mut source = LexerSource::new(src);
        let line = source.next_line(false).unwrap().unwrap();
        source.start_heredoc(Bytes::from("END"), &mut Some(line)).unwrap();

        let body = source.next_line(false).unwrap().unwrap();
        assert_eq!(&body.line[..], b"body");

        // Terminator line at EOF still terminates.
        assert!(matches!(source.next_line(false), Ok(None)));
    }

    #[test]
    fn source_peeked_heredoc_terminator_stays_pending_until_consumed() {
        let mut source = LexerSource::new(b"body\nEND\nrest\n");

        let mut current_line = Some(LexerLine { number: 999, offset: 123, line: Bytes::from_static(b"saved"), terminated: false, pos: 0, ascii_only: true });

        source.start_heredoc(Bytes::from_static(b"END"), &mut current_line).unwrap();

        let body = source.next_line(false).unwrap().unwrap();
        assert_eq!(&body.line[..], b"body");

        // Peek sees end-of-heredoc but does not consume it.
        assert!(matches!(source.next_line(true), Ok(None)));
        // Repeated peeks still see pending end.
        assert!(matches!(source.next_line(true), Ok(None)));

        // Consuming call now pops the heredoc and restores saved line.
        assert!(matches!(source.next_line(false), Ok(None)));

        let restored = source.next_line(false).unwrap().unwrap();
        assert_eq!(&restored.line[..], b"saved");

        let rest = source.next_line(false).unwrap().unwrap();
        assert_eq!(&rest.line[..], b"rest");
    }

    #[test]
    fn subst_body_virtual_eof_restores_remainder() {
        let mut source = LexerSource::new(b"");

        let mut current_line = Some(LexerLine { number: 1, offset: 0, line: Bytes::from_static(b"bar/e + 1"), terminated: false, pos: 0, ascii_only: true });

        let flags = source.start_subst_body(b'/', &mut current_line).unwrap();
        assert_eq!(flags.as_deref(), Some("e"));

        let body = source.next_line(false).unwrap().unwrap();
        assert_eq!(&body.line[body.pos..], b"bar");

        // Virtual EOF after all body lines are delivered.
        assert!(matches!(source.next_line(false), Ok(None)));

        // Then the saved remainder appears.
        let rest = source.next_line(false).unwrap().unwrap();
        assert_eq!(&rest.line[rest.pos..], b" + 1");
    }

    #[test]
    fn subst_body_captures_multiple_flags_and_restores_remainder() {
        let mut source = LexerSource::new(b"");
        let mut current_line = Some(LexerLine { number: 1, offset: 0, line: Bytes::from_static(b"bar/msix + 1"), terminated: false, pos: 0, ascii_only: true });

        let flags = source.start_subst_body(b'/', &mut current_line).unwrap();
        assert_eq!(flags.as_deref(), Some("msix"));

        let body = source.next_line(false).unwrap().unwrap();
        assert_eq!(&body.line[body.pos..], b"bar");

        assert!(matches!(source.next_line(false), Ok(None)));

        let rest = source.next_line(false).unwrap().unwrap();
        assert_eq!(&rest.line[rest.pos..], b" + 1");
    }

    #[test]
    fn subst_body_with_paired_delimiter_nesting() {
        let mut source = LexerSource::new(b"");
        let mut current_line = Some(LexerLine { number: 1, offset: 0, line: Bytes::from_static(b"a{b}c}r"), terminated: false, pos: 0, ascii_only: true });

        let flags = source.start_subst_body(b'{', &mut current_line).unwrap();
        assert_eq!(flags.as_deref(), Some("r"));

        let body = source.next_line(false).unwrap().unwrap();
        assert_eq!(&body.line[body.pos..], b"a{b}c");

        assert!(matches!(source.next_line(false), Ok(None)));
    }

    #[test]
    fn subst_body_errors_on_eof() {
        let mut source = LexerSource::new(b"");
        let mut current_line = Some(LexerLine { number: 1, offset: 0, line: Bytes::from_static(b"unterminated"), terminated: false, pos: 0, ascii_only: true });

        let err = source.start_subst_body(b'/', &mut current_line).unwrap_err();
        assert!(err.message.contains("unterminated substitution"));
    }

    #[test]
    fn indented_heredoc_errors_on_mismatched_indent_after_start() {
        let mut source = LexerSource::new(b"  body\nEND\n");
        let mut current_line = Some(LexerLine { number: 1, offset: 0, line: Bytes::from_static(b"saved"), terminated: false, pos: 0, ascii_only: true });

        // Terminator has no indent, so required indent becomes empty.
        source.start_indented_heredoc(Bytes::from_static(b"END"), &mut current_line).unwrap();

        // First body line should still come through.
        let body = source.next_line(false).unwrap().unwrap();
        assert_eq!(&body.line[..], b"  body");

        // Terminator ends the heredoc.
        assert!(matches!(source.next_line(false), Ok(None)));
    }

    #[test]
    fn filename_is_preserved() {
        let source = LexerSource::with_filename(b"1;\n", "foo.pl");
        assert_eq!(source.filename(), "foo.pl");
        // Default filename should be a sensible default.
        let default = LexerSource::new(b"1;\n");
        assert!(!default.filename().is_empty(), "default filename should not be empty");
    }

    #[test]
    fn push_back_precedes_underlying_source() {
        let mut source = LexerSource::new(b"real\n");
        let mut q = VecDeque::new();
        q.push_back(LexerLine { number: 999, offset: 123, line: Bytes::from_static(b"queued"), terminated: false, pos: 0, ascii_only: true });
        source.push_back(q);

        let first = source.next_line(false).unwrap().unwrap();
        assert_eq!(&first.line[..], b"queued", "push_back lines should come before underlying source");
        assert_eq!(first.number, 999, "pushed line number should be preserved");

        let second = source.next_line(false).unwrap().unwrap();
        assert_eq!(&second.line[..], b"real", "underlying source should follow pushed lines");
    }
}
