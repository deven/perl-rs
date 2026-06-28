//! Line-oriented source delivery for the lexer.
//!
//! The source layer manages line splitting, CRLF normalization, heredoc body sequencing, and indentation stripping.
//! The lexer receives one line at a time via `LexerLine` and scans bytes within it, never dealing with line boundaries,
//! newline encoding, or heredoc line reordering.
//!
//! See design document §5.4 for the full design rationale.

use crate::error::ParseError;
use crate::lexer::{FrameRole, LexContext, Lexer, matching_delimiter};
use crate::span::Span;
use bytes::Bytes;
use std::collections::VecDeque;
use std::mem;
use std::ops::{Deref, DerefMut};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/source_tests.rs"]
mod tests;

// ── LexerLine ─────────────────────────────────────────────────────
/// A single line of source code with a byte-scanning cursor.
///
/// The lexer's working unit.  The visible `line`/`terminated` are an *effective view*: the physical line, possibly
/// narrowed to a frame's EOF bound (`set_eof`).  The physical truth lives in the hidden `full`/`full_terminated`, which
/// the view is always rebuilt from, so a bound can narrow or widen at any time.  Because the hidden fields must stay
/// the authoritative source of the view, a `LexerLine` can only be built through `LexerLine::new`.
#[derive(Clone, Debug)]
pub(crate) struct LexerLine {
    /// 1-based line number in the original source.
    pub number: u32,

    /// Byte offset of the start of this line in the original source.  Physically invariant — narrowing never shifts
    /// it — so it serves as the equality key for endpoint detection.
    pub offset: u32,

    /// Effective line content delivered to the lexer: the physical line, possibly narrowed by an EOF bound.  Never
    /// includes the line ending.
    pub line: Bytes,

    /// Effective termination: whether the *effective view* ends at a real newline.  `false` whenever `line` is narrowed
    /// below the physical extent, since the narrow strips the virtual newline first.
    pub terminated: bool,

    /// Required indent for this line, in bytes from `offset`.  Stamped during a `<<~` body scan; `0` for fresh reads
    /// and every other construct.  Carried so replayed lines keep their indent.
    pub indent: u32,

    /// Current scanning position within `line`.
    pub pos: u32,

    /// Whether the line contains only ASCII bytes (all < 0x80).  Computed for free during newline scanning and used to
    /// skip UTF-8 decoding and NFC normalization for all-ASCII lines.
    pub ascii_only: bool,

    /// Physical line content, full width — the source `set_eof` narrows or widens from.  `Bytes` is refcounted, so this
    /// is a second handle on the same buffer, not a copy.
    full: Bytes,

    /// Physical termination, restored when `set_eof` widens back to the full line.
    full_terminated: bool,
}

impl LexerLine {
    /// Construct a freshly read line: the effective view starts equal to the physical line (un-narrowed), and `number`,
    /// `indent`, and `pos` default to `0`.  `number` is stamped at delivery; callers set `pos`/`indent` after when a
    /// body resumes mid-line or carries a required indent.
    pub fn new(offset: u32, content: Bytes, terminated: bool, ascii_only: bool) -> Self {
        LexerLine { number: 0, offset, full: content.clone(), line: content, full_terminated: terminated, terminated, indent: 0, pos: 0, ascii_only }
    }

    /// Test-only constructor mirroring the old field-literal shape, so source tests can build a line with an explicit
    /// `number`/`pos` without naming the hidden physical fields.  The effective view starts un-narrowed (`full == line`,
    /// `indent == 0`).
    #[cfg(test)]
    pub(crate) fn for_test(number: u32, offset: u32, line: Bytes, terminated: bool, pos: u32, ascii_only: bool) -> Self {
        let mut l = LexerLine::new(offset, line, terminated, ascii_only);
        l.number = number;
        l.pos = pos;
        l
    }

    /// Apply a frame's EOF bound — a global byte offset, or `None` for real EOF — rebuilding the effective view from
    /// `full`.  A total, idempotent pure function of `(full, bound)`: this line covers the global range
    /// `[offset, offset + extent)` where `extent` includes the virtual newline, so the bound either narrows within that
    /// range, widens to the full line (bound at/after the end), or empties the line (bound before the start).
    pub fn set_eof(&mut self, bound: Option<u32>) {
        let start = self.offset;
        // Global end of the line, one past the virtual newline when physically terminated.
        let extent = start + self.full.len() as u32 + self.full_terminated as u32;
        match bound {
            // Endpoint on an earlier line: this line is wholly past EOF, so it delivers nothing.
            Some(g) if g < start => {
                self.line = self.full.slice(..0);
                self.terminated = false;
            }
            // Endpoint within this line: narrow to it.  Any cut below the full extent strips at least the virtual
            // newline, so the effective view is unterminated.
            Some(g) if g < extent => {
                self.line = self.full.slice(..(g - start) as usize);
                self.terminated = false;
            }
            // No bound, or an endpoint at/after this line's end: the full physical line.
            _ => {
                self.line = self.full.clone();
                self.terminated = self.full_terminated;
            }
        }
    }

    /// Peek at the current byte without advancing.  Returns `b'\n'` at the end of a terminated line.  Returns `None`
    /// only when truly exhausted (past \n or unterminated line fully consumed).
    #[inline]
    pub fn peek_byte(&self) -> Option<u8> {
        let pos = self.pos as usize;
        if pos < self.line.len() {
            Some(self.line[pos])
        } else if pos == self.line.len() && self.terminated {
            Some(b'\n')
        } else {
            None
        }
    }

    /// Peek at a byte at an offset from the current position.
    #[inline]
    pub fn peek_byte_at(&self, offset: usize) -> Option<u8> {
        let idx = self.pos as usize + offset;
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
        let pos = self.pos as usize;
        if pos < self.line.len() {
            let b = self.line[pos];
            self.pos += 1;
            Some(b)
        } else if pos == self.line.len() && self.terminated {
            self.pos += 1;
            Some(b'\n')
        } else {
            None
        }
    }

    /// The remaining unscanned content bytes (not including the virtual `\n` line terminator).
    #[inline]
    pub fn remaining(&self) -> &[u8] {
        let pos = self.pos as usize;
        if pos < self.line.len() { &self.line[pos..] } else { &[] }
    }

    /// Byte offset in the original source at the current cursor position.  Used for span construction.
    #[inline]
    pub fn global_pos(&self) -> u32 {
        self.offset + self.pos
    }
}

// ── Source layer (impl Lexer) ──────────────────────────────────────────────────────────────────────────────────────
/// A raw line read from the source buffer before indent processing.
struct RawLine {
    offset: u32,
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
    pub fn line_number(&self) -> u32 {
        self.line_number
    }

    /// Name of the source file being lexed, for `__FILE__` resolution and diagnostics.  Defaults to `"(script)"` when
    /// the caller used [`Self::new`] without a filename.
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// Override the line number for `# line N` directives.
    pub fn set_line_number(&mut self, n: u32) {
        // A directive crossed during a speculative scan must not renumber anything: the line it sits on might turn out
        // to be heredoc body text.  The directive takes effect on the real pass, when the line is delivered for real.
        if self.lookahead_mode {
            return;
        }
        self.line_number = n;
    }

    /// Override the filename for `# line N "file"` directives.
    pub fn set_filename(&mut self, name: String) {
        if self.lookahead_mode {
            return;
        }
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

    /// Deliver the next line as the current line (`self.line`), or `Ok(&None)` at virtual EOF — the top frame's bound
    /// reached, or real EOF.  The bound was fixed up front by the body's setup scan (§5.4.5/§5.4.6), so there is no
    /// per-line terminator check here; delivery just runs the line stream to the bound.  Lines come from the replay
    /// queue first, then fresh reads.  On `Ok(&None)` the previous current line is left in place, so callers that still
    /// need its position (e.g. `span_pos` at a virtual EOF) see it unchanged.
    pub fn next_line(&mut self) -> Result<&Option<LexerLine>, ParseError> {
        let bound = self.context_stack.last().and_then(|c| c.bound);

        // The next line's start offset is known before reading it — the queue front's offset if queued, else the read
        // frontier.  Peeking it lets the bound test fire without consuming a line that starts past the end.
        let next_start = self.queued_lines.front().map_or(self.cursor as u32, |l| l.offset);
        if let Some(b) = bound
            && next_start > b
        {
            return Ok(&None); // Virtual EOF: the next line starts past the frame's bound.
        }

        // Obtain the line: replayed, else freshly read.  `install_line` applies the bound (`set_eof`), so the endpoint
        // line is narrowed there, at the single choke point — not here.
        let mut line = if let Some(line) = self.queued_lines.pop_front() {
            line
        } else {
            match self.read_raw_line() {
                Some(raw) => LexerLine::new(raw.offset, raw.content, raw.terminated, raw.ascii_only),
                None => return Ok(&None), // Real EOF.
            }
        };

        // Stamped at delivery, so replayed lookahead lines pick up the current counter.
        line.number = self.line_number;
        self.line_number += 1;
        Ok(self.install_line(line))
    }

    /// Install `line` as the current line, recompute `effective_utf8`, and return a borrow of `self.line` (always the
    /// freshly-installed `Some`).  A "set this line" primitive: it does not renumber, so callers that restore a saved
    /// line keep its original number.  Line numbering happens in `next_line` at delivery; capture, the bound narrow,
    /// and `consume_lookahead` window tracking are install-event concerns and live here.
    ///
    /// The single choke point every delivered line passes through, so it is where the top frame's EOF bound is applied
    /// (`set_eof`): no installed line ever exceeds the current frame's bound, no matter the delivery path.  During a
    /// scan the borrowed ceiling sits past every line read, so `set_eof` widens to the full line automatically — no
    /// mode gate needed; when the guard drops and restores the real bound, the re-installed entry line re-narrows.
    fn install_line(&mut self, mut line: LexerLine) -> &Option<LexerLine> {
        // During an active lookahead, capture the line being displaced (in displacement order) so the guard can restore
        // the original current line and queue the rest for replay.
        if self.lookahead_mode
            && let Some(displaced) = self.line.take()
        {
            self.lookahead.push_back(displaced);
        }

        line.set_eof(self.context_stack.last().and_then(|c| c.bound));
        self.effective_utf8 = self.utf8_mode && !line.ascii_only;
        self.line = Some(line);

        // Outside a scan, narrow then close the `consume_lookahead` window as normal delivery reaches and then passes
        // the line the scan ended on (identified by its unique source offset).
        if !self.lookahead_mode {
            match self.lookahead_offset {
                Some(off) if self.line.as_ref().is_some_and(|l| l.offset == off) => self.lookahead_offset = None,
                None if self.lookahead_pos.is_some() => self.lookahead_pos = None,
                _ => {}
            }
        }

        &self.line
    }

    /// Pop the top context frame, restoring its suspended parent line as the live line (§5.5).  The body's bound goes
    /// with the frame, so the restored line is re-narrowed to the new top's bound — real EOF at the base.  The resume
    /// cursor is already baked into the suspended line (past the close for a delimited body, past `<<TAG` for a
    /// heredoc), so the pop carries no resume logic.  Returns the popped frame for any mode-specific teardown.
    pub(crate) fn pop_context(&mut self) -> Option<LexContext> {
        let mut ctx = self.context_stack.pop()?;
        self.line = ctx.line.take();
        if let Some(line) = self.line.as_mut() {
            line.set_eof(self.context_stack.last().and_then(|c| c.bound));
        }
        Some(ctx)
    }

    /// TEMPORARY: owned-clone wrapper around `next_line`, for the source tests which still consume lines by value.
    /// Remove once those tests are updated to the borrowing API.
    #[cfg(test)]
    pub(crate) fn temp_next_line(&mut self) -> Result<Option<LexerLine>, ParseError> {
        self.next_line().cloned()
    }

    /// Begin a speculative lookahead.  Returns an RAII guard; while it is alive the consumer scans forward with the
    /// normal line API (`next_line`, `peek_byte`, …) and `# line` directive setters are suppressed.  On drop the lexer
    /// snaps back to the line and cursor where the scan began, and the previewed lines are re-queued for normal
    /// re-delivery — unless the consumer then calls [`Self::consume_lookahead`] to commit to where the scan ended.
    #[allow(dead_code)] // No consumers yet — the primitive lands ahead of its first user (e.g. fat-comma autoquoting).
    pub(crate) fn lookahead(&mut self) -> Lookahead<'_> {
        // A new lookahead supersedes any still-open consume window from a previous one.
        self.lookahead_offset = None;
        self.lookahead_pos = None;
        let entry_pos = self.line.as_ref().map_or(0, |l| l.pos);
        self.lookahead_mode = true;
        Lookahead { lexer: self, entry_pos, saved_bound: None }
    }

    /// Begin a lookahead that borrows a virtual EOF for its duration.  Like [`Self::lookahead`], but the top frame's
    /// bound is replaced by `ceiling` (a global offset, or `None` for real EOF) so the scan runs to that ceiling and
    /// cannot escape it — used by the heredoc terminator scan to search down to the host's EOF without targeting a
    /// frame (§5.4.5, §5.4.11).  The original bound is saved and restored when the guard drops, on every path.  The
    /// ceiling must be in place before the first `next_line`, so it is taken at construction.
    #[allow(dead_code)] // No consumers yet — paired with `start_heredoc`.
    pub(crate) fn lookahead_to(&mut self, ceiling: Option<u32>) -> Lookahead<'_> {
        self.lookahead_offset = None;
        self.lookahead_pos = None;
        let entry_pos = self.line.as_ref().map_or(0, |l| l.pos);
        self.lookahead_mode = true;
        // Borrow the ceiling on the top frame (the frame `next_line` bounds by).  An empty stack has no frame to
        // borrow and already bounds by real EOF, which is the only ceiling the base host yields, so nothing to save.
        let saved_bound = self.context_stack.last_mut().map(|c| {
            let old = c.bound;
            c.bound = ceiling;
            old
        });
        Lookahead { lexer: self, entry_pos, saved_bound }
    }

    /// Commit to where the most recent lookahead ended instead of rewinding.  Drives normal delivery forward to the
    /// line the scan ended on (re-stamping and re-delivering the intervening lines as it goes) and advances the cursor
    /// to the scan-end position.  A no-op if the window has already expired — never opened, already consumed, or passed
    /// by normal lexing — so it is always safe to call.
    #[allow(dead_code)] // No consumers yet — paired with `lookahead`.
    pub(crate) fn consume_lookahead(&mut self) -> Result<(), ParseError> {
        // Deliver lines until the scan-end line is current; `install_line` clears `lookahead_offset` on arrival.
        while self.lookahead_offset.is_some() {
            if self.next_line()?.is_none() {
                return Err(ParseError::new("consume_lookahead: lookahead end line is unreachable", Span::DUMMY));
            }
        }
        // On (or already past) the end line: advance the cursor to the scan-end position unless we're there or beyond.
        if let Some(end_pos) = self.lookahead_pos.take()
            && let Some(line) = self.line.as_mut()
            && end_pos > line.pos
        {
            line.pos = end_pos;
        }
        Ok(())
    }

    /// Begin a heredoc body — `<<TAG`, `<<~TAG`, interpolating or not.
    ///
    /// A heredoc's body is the run of lines after the introducer, up to the terminator, delivered immediately —
    /// before the rest of the introducer line (§5.4.5).  Walks to the host (the nearest frame whose current line is
    /// terminated), scans under the host's bound as a borrowed ceiling for the terminator, resolves and stamps the
    /// `<<~` indent on the captured body lines while they are still in the lookahead queue, drops the terminator, and
    /// pushes the body frame with the introducer suspended as its parent — so popping resumes just past `<<TAG`.
    pub fn start_heredoc(&mut self, tag: Bytes, indented: bool, interpolating: bool) -> Result<(), ParseError> {
        // Host-walk: down from the real top, the nearest frame whose current line ends in a real newline hosts the
        // body, and its bound is the scan ceiling; mid-line frames (e.g. an `/e` replacement at its close) can't host
        // and are skipped.  At stack level `idx` the current line is `self.line` (top) or `stack[idx].line`, paired
        // with bound `stack[idx-1].bound`, or `None` at the base.
        let intro_offset = self.line.as_ref().map_or(self.cursor as u32, |l| l.offset);
        let ceiling = {
            let mut idx = self.context_stack.len();
            let mut cur = self.line.as_ref();
            loop {
                let bound = if idx > 0 { self.context_stack[idx - 1].bound } else { None };
                if cur.is_some_and(|l| l.terminated) {
                    break bound;
                }
                if idx == 0 {
                    break None; // The base line is unterminated only at real EOF; the ceiling is real EOF anyway.
                }
                cur = self.context_stack[idx - 1].line.as_ref();
                idx -= 1;
            }
        };

        // Scan under the borrowed ceiling for the terminator, capturing the body lines.  The bound is the terminator's
        // offset; for `<<~`, its leading whitespace is the new required indent, stamped on the captured body lines
        // before the guard drops.
        let bound;
        {
            let mut g = self.lookahead_to(ceiling);
            bound = loop {
                let found = match g.next_line()? {
                    Some(line) => Self::terminator_matches(line, &tag, indented).map(|ws| {
                        let new_indent = indented.then(|| line.line.slice(line.pos as usize..line.pos as usize + ws as usize));
                        (line.offset, new_indent)
                    }),
                    None => {
                        let kind = (if interpolating { FrameRole::Heredoc } else { FrameRole::LiteralHeredoc }).unterminated_kind();
                        let tag = String::from_utf8_lossy(&tag);
                        return Err(ParseError::unterminated(kind, Some(tag.as_ref()), Span::new(intro_offset, intro_offset)));
                    }
                };
                if let Some((offset, new_indent)) = found {
                    if let Some(indent) = new_indent {
                        g.stamp_heredoc_indent(&indent)?;
                    }
                    // Drop the terminator: the read frontier is already past it, so the body reaches virtual EOF
                    // without it being delivered or replayed.
                    g.line = None;
                    break offset;
                }
            };
        } // Guard drops: the introducer is restored as `self.line` (past `<<TAG`), the body lines re-queued.

        // Promote: suspend the introducer as the parent and push the body frame bounded at the terminator's offset.
        // The live line is left `None` — the first `peek_byte` in `lex_body` auto-loads body line 1, consuming the
        // pending virtual-EOF signal (§5.5).  Popping later restores the introducer and resumes after `<<TAG` — the
        // uniform pop a delimited body uses.
        // A heredoc is either interpolating (`<<TAG`, escapes processed like `"..."`) or literal (`<<'TAG'`, no escape
        // processing at all — verified against perl: `\\` stays `\\`).  The `Heredoc`/`LiteralHeredoc` role carries
        // which, and `lex_body` reads `role.raw()` (false for the former, true for the latter).
        let introducer = self.line.take();
        let mut ctx = LexContext::new(None, if interpolating { FrameRole::Heredoc } else { FrameRole::LiteralHeredoc });
        ctx.line = introducer;
        ctx.bound = Some(bound);
        self.context_stack.push(ctx);
        Ok(())
    }

    /// Test whether `line` is a heredoc terminator for `tag`, read from `pos = indent`.  A plain `<<TAG` requires the
    /// line from `pos` to equal the tag exactly; a `<<~TAG` skips leading whitespace and matches the tag after it.
    /// Returns the leading-whitespace byte count — the `<<~` new required indent, `0` for a plain match — on a match.
    fn terminator_matches(line: &LexerLine, tag: &[u8], indented: bool) -> Option<u32> {
        let rest = &line.line[line.pos as usize..];
        if indented {
            let ws = rest.iter().take_while(|&&b| b == b' ' || b == b'\t').count();
            (&rest[ws..] == tag).then_some(ws as u32)
        } else {
            (rest == tag).then_some(0)
        }
    }

    /// Validate and stamp the `<<~` required indent on the captured body lines.  The introducer is `lookahead[0]`,
    /// left alone; each body line is checked from its current `pos` (the enclosing indent already in effect): empty
    /// from `pos` is a logical blank (left as-is), one carrying the full new indent advances `pos`/`indent` past it,
    /// anything else is the fatal mismatch.  Runs while the lines are still captured, so nested scans compose and
    /// replayed lines keep their indent (§5.4.5).
    fn stamp_heredoc_indent(&mut self, new_indent: &[u8]) -> Result<(), ParseError> {
        for line in self.lookahead.iter_mut().skip(1) {
            let pos = line.pos as usize;
            let rest = &line.line[pos..];
            if rest.is_empty() {
                continue; // Logical blank line at this stage.
            }
            if rest.starts_with(new_indent) {
                line.pos += new_indent.len() as u32;
                line.indent = line.pos;
            } else {
                return Err(ParseError::new("indentation of here-doc doesn't match delimiter", Span::new(line.offset, line.offset)));
            }
        }
        Ok(())
    }

    /// Advance the cursor `n` bytes within the current line and return the byte now under it: a content byte, the
    /// virtual `\n` at line end, or `None` once past it.  Stays within the line — `None` is the caller's cue to cross
    /// to the next line via `next_line`, the sole point a new physical line offset enters a scan.  A global position
    /// is only ever `offset + pos` of one line and is never carried across a newline (§5.4.3), so this primitive
    /// never synthesizes a position past the line end.
    fn advance(&mut self, n: usize) -> Option<u8> {
        let line = self.line.as_mut()?;
        line.pos += n as u32;
        line.peek_byte()
    }

    /// Begin a delimited body — the shared front end for every quote-like construct (`q//`, `qq{}`, `m//`, `s///`,
    /// `tr///`, …).  The caller has already consumed the opening delimiter, so the cursor sits at the first body byte,
    /// and has built `ctx` with the body's lex mode (`LexContext::new`); this scans ahead to the matching close,
    /// records the body's bound, suspends the parent at the resume point past the close, and pushes `ctx` as the body
    /// frame with its source-delivery cluster (`line`/`bound`) filled.
    ///
    /// The scan is `scan_str` parity (§5.4.6): a byte walk over the lookahead stream, oblivious to where lines fall,
    /// in which a backslash protects the next character (unless the delimiter is itself a backslash), paired
    /// delimiters nest by depth, and only the close offset is recorded.  Keeping or stripping the escaping backslash,
    /// the `s///` flags, and the body's interpolation are all the sublexer's concern, downstream of the frame.
    pub fn start_delimited_body(&mut self, mut ctx: LexContext, extra_paired: bool) -> Result<(), ParseError> {
        let Some(delim) = ctx.delim else {
            return Err(ParseError::new("start_delimited_body requires a delimiter", Span::new(self.cursor as u32, self.cursor as u32)));
        };
        let (open, close) = matching_delimiter(delim, extra_paired);
        let close_is_backslash = close == '\\';

        // Encode open/close to the bytes the source holds them as under the active mode: UTF-8 (1–4 bytes) under
        // `use utf8`, a single Latin-1 byte under `no utf8` (where a delimiter is always <= U+00FF).  The body is
        // single-mode, so this encoding is fixed for the whole scan.  `matching_delimiter` itself is mode-independent
        // — only the byte width differs (`«` is `C2 AB` under utf8, `AB` under `no utf8`).
        let mut close_buf = [0u8; 4];
        let close_len = if self.utf8_mode {
            close.encode_utf8(&mut close_buf).len()
        } else {
            close_buf[0] = close as u8;
            1
        };
        let close_bytes = &close_buf[..close_len];
        let mut open_buf = [0u8; 4];
        let open_bytes: Option<&[u8]> = if let Some(o) = open {
            let len = if self.utf8_mode {
                o.encode_utf8(&mut open_buf).len()
            } else {
                open_buf[0] = o as u8;
                1
            };
            Some(&open_buf[..len])
        } else {
            None
        };

        // Span anchor for an unterminated-body error, read before the scan borrows `self`.
        let body_start = self.line.as_ref().map_or(self.cursor as u32, |l| l.global_pos());

        // Scan for the close.  Two independent partial-match indices — `i` over the close needle, `j` over the open —
        // advance in lockstep with the byte walk; a mismatch resets that needle to 0 with no re-anchor, exact because
        // the input is valid UTF-8 by construction (§5.4.6): a byte that breaks a partial match is a continuation
        // byte and so can never equal a needle's lead byte.  A paired open raises `depth`; a close at depth 0 ends
        // the body.  The close-line clone and bound are produced by the loop's `break` value, captured before the
        // guard drops and rewinds.
        let (parent, bound) = {
            let mut g = self.lookahead();
            let mut i = 0usize;
            let mut j = 0usize;
            let mut depth = 0u32;
            let mut b = g.peek_byte_at(0);
            loop {
                let Some(byte) = b else {
                    // Line exhausted: cross to the next line, re-anchoring on its own physical offset.
                    if g.next_line()?.is_none() {
                        let tok = close.to_string();
                        return Err(ParseError::unterminated(ctx.role.unterminated_kind(), Some(tok.as_str()), Span::new(body_start, body_start)));
                    }
                    b = g.peek_byte_at(0);
                    continue;
                };

                if byte == b'\\' && !close_is_backslash {
                    // Escape: skip the backslash and the next character's lead byte.  Its continuation bytes (if any)
                    // then match no needle, so a multi-byte escaped delimiter is protected with no width logic.
                    b = g.advance(2);
                    i = 0;
                    j = 0;
                    continue;
                }

                i = if byte == close_bytes[i] { i + 1 } else { 0 };
                if let Some(ob) = open_bytes {
                    j = if byte == ob[j] { j + 1 } else { 0 };
                }

                if i == close_len {
                    if depth == 0 {
                        // Found the close.  The cursor sits on its last byte; the close begins `close_len - 1` bytes
                        // back on this same line (a delimiter never spans a newline), and the parent resumes one byte
                        // past it.  Every position is `offset + pos` of this one line — no cross-line arithmetic.  A
                        // missing current line here is impossible (the matched byte came from it), but is reported as
                        // an unterminated body rather than panicked on.
                        let Some(line) = g.line.as_ref() else {
                            let tok = close.to_string();
                            return Err(ParseError::unterminated(ctx.role.unterminated_kind(), Some(tok.as_str()), Span::new(body_start, body_start)));
                        };
                        let bound = line.offset + line.pos - (close_len as u32 - 1);
                        let mut parent = line.clone();
                        parent.pos = line.pos + 1;
                        break (parent, bound);
                    }
                    depth -= 1;
                    i = 0;
                    j = 0;
                } else if let Some(ob) = open_bytes
                    && j == ob.len()
                {
                    depth += 1;
                    i = 0;
                    j = 0;
                }

                b = g.advance(1);
            }
        }; // Guard drops: `self.line` is restored to the body's first line at the body start, previewed lines re-queued.

        // Promote: suspend the parent at the resume point, push the body frame with its bound, and narrow the live
        // body-first line to the bound.  A single-line body shares the physical line with the parent (two views, one
        // buffer); a multi-line body keeps the first line whole and the re-queued close line narrows on delivery.
        ctx.line = Some(parent);
        ctx.bound = Some(bound);
        self.context_stack.push(ctx);
        if let Some(line) = self.line.as_mut() {
            line.set_eof(Some(bound));
        }
        Ok(())
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
        // The line number is stamped at delivery in `next_line` (which also advances the counter), so lines
        // redelivered from `queued_lines` are renumbered; the raw read carries no number.

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

        Some(RawLine { offset: start as u32, content: self.src.slice(start..content_end), terminated, ascii_only })
    }
}

/// RAII guard for a speculative lookahead, returned by [`Lexer::lookahead`].  Derefs to the `Lexer` so the consumer
/// scans with the normal line API.  On drop it snaps the lexer back to the line and cursor where the scan began,
/// leaving the previewed lines queued for re-delivery and recording where the scan ended for `consume_lookahead`.
#[allow(dead_code)] // Constructed only by `lookahead`, which has no consumers yet.
pub(crate) struct Lookahead<'a> {
    lexer: &'a mut Lexer,
    /// Cursor position on the current line at the moment the scan began.
    entry_pos: u32,
    /// For a `lookahead_to` ceiling borrow, the top frame's original bound, restored on drop.  `None` for a plain
    /// `lookahead` (no ceiling was borrowed, so nothing to restore).
    saved_bound: Option<Option<u32>>,
}

impl Drop for Lookahead<'_> {
    fn drop(&mut self) {
        let entry_pos = self.entry_pos;
        let lx = &mut *self.lexer;

        // Restore a borrowed ceiling first, so the entry line is re-narrowed (via `install_line`) to the host frame's
        // real bound, not the ceiling that was in force during the scan.
        if let Some(old) = self.saved_bound
            && let Some(top) = lx.context_stack.last_mut()
        {
            top.bound = old;
        }

        // Record where the scan ended — the current line's unique offset (the `consume_lookahead` key) and cursor —
        // before any lines move, plus whether the scan ever left its starting line.
        let end_offset = lx.line.as_ref().map(|l| l.offset);
        let end_pos = lx.line.as_ref().map_or(0, |l| l.pos);
        let displaced = !lx.lookahead.is_empty();

        // Reclaim the original current line: the front of the capture queue if the scan crossed a boundary, else the
        // still-current line.  Re-install it through `install_line` — while `lookahead_mode` is still set it captures
        // the line it displaces, so installing the entry line sweeps the scan-end line onto the back of the capture
        // queue and refreshes the restored line's derived state, with its cursor already back at the entry position.
        let entry = if displaced { lx.lookahead.pop_front() } else { lx.line.take() };
        if let Some(mut entry) = entry {
            entry.pos = entry_pos;
            lx.install_line(entry);
        }

        // The capture queue now holds exactly the previewed run; re-queue it for replay, each line from its start —
        // which for a `<<~`-stamped line is its required indent, not 0, so replayed lines keep their indent (§5.4.3).
        let mut replay = mem::take(&mut lx.lookahead);
        for line in replay.iter_mut() {
            line.pos = line.indent;
        }
        lx.push_back(replay);

        // The scan-end line is reachable for `consume_lookahead` only when the scan actually left the original line.
        lx.lookahead_offset = if displaced { end_offset } else { None };
        lx.lookahead_pos = Some(end_pos);

        // Rewind the counter to just past the restored line; its number is unchanged, so delivery resumes exactly
        // where it left off, and no separate saved value is needed.
        if let Some(number) = lx.line.as_ref().map(|l| l.number) {
            lx.line_number = number + 1;
        }
        lx.lookahead_mode = false;
    }
}

impl Deref for Lookahead<'_> {
    type Target = Lexer;

    fn deref(&self) -> &Lexer {
        self.lexer
    }
}

impl DerefMut for Lookahead<'_> {
    fn deref_mut(&mut self) -> &mut Lexer {
        self.lexer
    }
}
