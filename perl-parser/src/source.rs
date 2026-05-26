//! Line-oriented source delivery for the lexer.
//!
//! The source layer manages line splitting, CRLF normalization, heredoc body sequencing, and indentation stripping.
//! The lexer receives one line at a time via `LexerLine` and scans bytes within it, never dealing with line boundaries,
//! newline encoding, or heredoc line reordering.
//!
//! See design document §5.4 for the full design rationale.

use crate::error::ParseError;
use crate::lexer::{Lexer, matching_delimiter};
use crate::span::Span;
use bytes::Bytes;
use std::collections::VecDeque;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/source_tests.rs"]
mod tests;

// ── LexerLine ─────────────────────────────────────────────────────
/// A single line of source code with a byte-scanning cursor.
///
/// The lexer's working unit.  All fields are `pub(crate)` — the lexer freely reads and writes `pos` for cursor control,
/// and reads `number` and `offset` for span computation.
#[derive(Clone, Debug)]
pub(crate) struct LexerLine {
    /// 1-based line number in the original source.
    pub number: usize,

    /// Byte offset of the start of this line in the original source.
    pub offset: usize,

    /// Line content without line ending.  When inside an indented heredoc, the required indentation prefix has been
    /// stripped.
    pub line: Bytes,

    /// Whether this line was terminated by a newline in the source.
    pub terminated: bool,

    /// Current scanning position within `line`.
    pub pos: usize,

    /// Whether the line contains only ASCII bytes (all < 0x80).  Computed for free during newline scanning and used to
    /// skip UTF-8 decoding and NFC normalization for all-ASCII lines.
    pub ascii_only: bool,
}

impl LexerLine {
    /// Peek at the current byte without advancing.  Returns `b'\n'` at the end of a terminated line.  Returns `None`
    /// only when truly exhausted (past \n or unterminated line fully consumed).
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

    /// Consume the current byte and advance the cursor.  Returns `b'\n'` at the end of a terminated line.  Returns
    /// `None` only when truly exhausted.
    #[cfg(test)]
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

    /// The remaining unscanned content bytes (not including the virtual `\n` line terminator).
    #[inline]
    pub fn remaining(&self) -> &[u8] {
        if self.pos < self.line.len() { &self.line[self.pos..] } else { &[] }
    }

    /// Byte offset in the original source at the current cursor position.  Used for span construction.
    #[inline]
    pub fn global_pos(&self) -> u32 {
        (self.offset + self.pos) as u32
    }
}

// ── Source layer (impl Lexer) ──────────────────────────────────────────────────────────────────────────────────────
/// Internal heredoc context saved when entering a heredoc body.
pub(crate) struct HeredocContext {
    tag: Bytes,
    saved_line: LexerLine,
    prev_indent: Option<Bytes>,
}

/// A raw line read from the source buffer before indent processing.
struct RawLine {
    number: usize,
    offset: usize,
    content: Bytes,
    terminated: bool,
    ascii_only: bool,
}

impl Lexer {
    /// Detect BOM or UTF-16 encoding at the start of a source buffer and transcode to UTF-8 if needed.  Returns the
    /// (possibly transcoded) source bytes and whether UTF-8 mode should be enabled.
    ///
    /// Takes an owned `Bytes` so the common case (no BOM, no UTF-16) is zero-copy.  UTF-8 BOM stripping uses
    /// `Bytes::slice` (also zero-copy).  Only UTF-16 transcoding allocates a new buffer.
    ///
    /// Detection order (matching Perl's `S_swallow_bom`):
    /// - `EF BB BF` → UTF-8 BOM: strip, enable utf8
    /// - `FF FE`    → UTF-16LE BOM: strip + transcode to UTF-8
    /// - `FE FF`    → UTF-16BE BOM: strip + transcode to UTF-8
    /// - `00 xx` pattern → heuristic UTF-16BE (no BOM)
    /// - `xx 00` pattern → heuristic UTF-16LE (no BOM)
    pub(crate) fn detect_and_transcode(src: Bytes) -> (Bytes, bool) {
        if src.len() >= 3 && src[0] == 0xEF && src[1] == 0xBB && src[2] == 0xBF {
            // UTF-8 BOM — strip it, enable utf8 mode.
            return (src.slice(3..), true);
        }
        if src.len() >= 2 {
            if src[0] == 0xFF && src[1] == 0xFE {
                // UTF-16LE BOM — strip BOM, transcode remainder.
                let utf8 = Self::transcode_utf16(&src[2..], true);
                return (Bytes::from(utf8), true);
            }
            if src[0] == 0xFE && src[1] == 0xFF {
                // UTF-16BE BOM — strip BOM, transcode remainder.
                let utf8 = Self::transcode_utf16(&src[2..], false);
                return (Bytes::from(utf8), true);
            }
        }

        // Heuristic: check for UTF-16 without BOM by looking at the first few bytes for a null-interleaving pattern.
        if src.len() >= 4 {
            let looks_le = src[1] == 0 && src[3] == 0 && src[0] != 0 && src[2] != 0;
            let looks_be = src[0] == 0 && src[2] == 0 && src[1] != 0 && src[3] != 0;
            if looks_le {
                let utf8 = Self::transcode_utf16(&src, true);
                return (Bytes::from(utf8), true);
            }
            if looks_be {
                let utf8 = Self::transcode_utf16(&src, false);
                return (Bytes::from(utf8), true);
            }
        }

        // No BOM, not UTF-16 — pass through unchanged (zero-copy).
        (src, false)
    }

    /// Transcode UTF-16 bytes to UTF-8.  Handles surrogate pairs for code points above U+FFFF.  Ignores a trailing odd
    /// byte.
    fn transcode_utf16(src: &[u8], little_endian: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(src.len());
        let mut i = 0;
        while i + 1 < src.len() {
            let unit = if little_endian { u16::from_le_bytes([src[i], src[i + 1]]) } else { u16::from_be_bytes([src[i], src[i + 1]]) };
            i += 2;
            let cp = if (0xD800..=0xDBFF).contains(&unit) {
                // High surrogate — read the low surrogate.
                if i + 1 < src.len() {
                    let lo = if little_endian { u16::from_le_bytes([src[i], src[i + 1]]) } else { u16::from_be_bytes([src[i], src[i + 1]]) };
                    if (0xDC00..=0xDFFF).contains(&lo) {
                        i += 2;
                        0x10000 + ((unit as u32 - 0xD800) << 10) + (lo as u32 - 0xDC00)
                    } else {
                        0xFFFD // unpaired high surrogate
                    }
                } else {
                    0xFFFD // truncated
                }
            } else if (0xDC00..=0xDFFF).contains(&unit) {
                0xFFFD // unpaired low surrogate
            } else {
                unit as u32
            };

            // Encode code point as UTF-8.
            if let Some(c) = char::from_u32(cp) {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        out
    }

    /// Current byte position in the source buffer.  Used for global position when no current line is active.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Current line number (1-based).
    #[allow(dead_code)]
    pub fn line_number(&self) -> usize {
        self.line_number
    }

    /// Name of the source file being lexed, for `__FILE__` resolution and diagnostics.  Defaults to `"(script)"` when
    /// the caller used [`Self::new`] without a filename.
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

    /// Raw slice of the source buffer.  For rare operations that need access to the underlying bytes (e.g. format body
    /// extraction).
    pub fn src_slice(&self, start: usize, end: usize) -> &[u8] {
        &self.src[start..end]
    }

    /// Push lines to be returned by future `next_line()` calls, ahead of any lines read from the source.
    pub fn push_back(&mut self, mut lines: VecDeque<LexerLine>) {
        lines.append(&mut self.queued_lines);
        self.queued_lines = lines;
    }

    /// Get the next line.
    ///
    /// Returns `Ok(Some(line))` for content, `Ok(None)` when a heredoc body is finished (the saved remainder will be
    /// returned by the next call), or `Err` for real errors (unterminated heredoc, indentation mismatch).
    ///
    /// `peek_heredoc`: when true and a heredoc terminator is found, returns `Ok(None)` without consuming the signal —
    /// the heredoc context stays on the stack and `queued_lines` is not modified.  The next call with
    /// `peek_heredoc=false` will consume it.
    pub fn next_line(&mut self, peek_heredoc: bool) -> Result<Option<LexerLine>, ParseError> {
        // 0. If a terminator was found during a previous peek call, handle it without reading another line.
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

        // 1. Return queued line if present (from heredoc remainder, push_back, or subst body — not subject to
        //    terminator check).
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
    /// Scans ahead to find the terminator, sets the required indentation from its whitespace prefix.  The current line
    /// is taken from the Option (setting it to None) and saved internally for restoration when the terminator is found.
    pub fn start_indented_heredoc(&mut self, tag: Bytes) -> Result<(), ParseError> {
        let line = self.line.take().ok_or_else(|| ParseError::new("internal error: start_indented_heredoc called without a current line", Span::DUMMY))?;
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
    /// The current line is taken from the Option (setting it to None) and saved internally for restoration when the
    /// terminator is found.  Does not change the required indentation.
    pub fn start_heredoc(&mut self, tag: Bytes) -> Result<(), ParseError> {
        let line = self.line.take().ok_or_else(|| ParseError::new("internal error: start_heredoc called without a current line", Span::DUMMY))?;
        let prev_indent = self.required_indent.clone();

        self.heredoc_stack.push(HeredocContext { tag, saved_line: line, prev_indent });
        Ok(())
    }

    /// Begin processing a substitution replacement body.
    ///
    /// Takes the current line, scans ahead to find the closing delimiter and flags, then queues the body lines for
    /// delivery with a virtual EOF at the end.  The remainder of the source line after the flags is saved for delivery
    /// after the EOF.
    ///
    /// Returns the captured flags (or None if no flags).
    pub fn start_subst_body(&mut self, delim: char, extra_paired: bool) -> Result<Option<String>, ParseError> {
        let mut line = self.line.take().ok_or_else(|| ParseError::new("internal error: start_subst_body called without a current line", Span::DUMMY))?;

        let (open, close) = matching_delimiter(delim, extra_paired);
        let close_len = close.len_utf8();
        let mut close_buf = [0u8; 4];
        let close_bytes = close.encode_utf8(&mut close_buf);
        let open_bytes = open.map(|o| {
            let mut buf = [0u8; 4];
            let len = o.len_utf8();
            o.encode_utf8(&mut buf);
            (buf, len)
        });
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
            let rest = &line.line[pos..];
            if b == b'\\' {
                // Skip escaped byte (or multi-byte delimiter after backslash).
                if rest.len() > 1 && rest[1..].starts_with(close_bytes.as_bytes()) {
                    pos += 1 + close_len;
                } else {
                    pos += 2;
                }
            } else if rest.starts_with(close_bytes.as_bytes()) && depth == 0 {
                // Found closing delimiter at `pos`.  Body content on this line: everything before `pos`.
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
                let mut flag_end = pos + close_len;
                while flag_end < line.line.len() && (line.line[flag_end].is_ascii_alphanumeric() || line.line[flag_end] == b'_') {
                    flag_end += 1;
                }
                let flags = if flag_end > pos + close_len { Some(String::from_utf8_lossy(&line.line[pos + close_len..flag_end]).into_owned()) } else { None };

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
            } else if let Some((ref obuf, olen)) = open_bytes
                && rest.starts_with(&obuf[..olen])
            {
                depth += 1;
                pos += olen;
            } else if rest.starts_with(close_bytes.as_bytes()) {
                depth -= 1;
                pos += close_len;
            } else {
                pos += 1;
            }
        }
    }

    // ── Internal methods ──────────────────────────────────────────
    /// Read the next raw line from the source buffer.
    ///
    /// Splits on `\n`, strips `\r` before `\n` (CRLF normalization).  Standalone `\r` not followed by `\n` is preserved
    /// as a literal byte.  Returns `None` at EOF.
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
    /// Returns the `LexerLine` with indent stripped and cursor at 0.  Empty lines (zero content) are allowed without
    /// the indent prefix.
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

    /// Scan ahead from the current cursor to find an indented heredoc terminator.  Returns the full raw whitespace
    /// prefix of the terminator line.  Does not advance the cursor.
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
