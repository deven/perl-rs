//! Source-layer `Lexer` tests.

use super::*;

/// Helper: collect all lines from a source.
fn collect_lines(src: &str) -> Vec<String> {
    let mut lexer = Lexer::new(src.as_bytes());
    let mut lines = Vec::new();
    while let Ok(Some(line)) = lexer.temp_next_line() {
        lines.push(String::from_utf8_lossy(&line.line).into_owned());
    }
    lines
}

// ── Basic line splitting ──────────────────────────────────────

#[test]
fn empty_source() {
    let mut lexer = Lexer::new(b"");
    assert!(matches!(lexer.temp_next_line(), Ok(None)));
}

#[test]
fn single_line_no_newline() {
    let mut lexer = Lexer::new(b"hello");
    let line = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&line.line[..], b"hello");
    assert!(!line.terminated);
    assert_eq!(line.number, 1);
    assert!(matches!(lexer.temp_next_line(), Ok(None)));
}

#[test]
fn single_line_with_newline() {
    let mut lexer = Lexer::new(b"hello\n");
    let line = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&line.line[..], b"hello");
    assert!(line.terminated);
    assert!(matches!(lexer.temp_next_line(), Ok(None)));
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
    let mut lexer = Lexer::new(b"a\nb\nc\n");
    assert_eq!(lexer.temp_next_line().unwrap().unwrap().number, 1);
    assert_eq!(lexer.temp_next_line().unwrap().unwrap().number, 2);
    assert_eq!(lexer.temp_next_line().unwrap().unwrap().number, 3);
}

#[test]
fn byte_offsets() {
    let mut lexer = Lexer::new(b"ab\ncde\nf\n");
    let l1 = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(l1.offset, 0);
    let l2 = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(l2.offset, 3);
    let l3 = lexer.temp_next_line().unwrap().unwrap();
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
    let mut lexer = Lexer::new(b"a\rb\n");
    let line = lexer.temp_next_line().unwrap().unwrap();
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
    let mut lexer = Lexer::new(b"abc\n");
    let mut line = lexer.temp_next_line().unwrap().unwrap();
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
    let mut lexer = Lexer::new(b"abcdef\n");
    let mut line = lexer.temp_next_line().unwrap().unwrap();
    line.pos = 3;
    assert_eq!(line.remaining(), b"def");
}

#[test]
fn lexer_line_slice() {
    let mut lexer = Lexer::new(b"hello world\n");
    let line = lexer.temp_next_line().unwrap().unwrap();
    let s = line.line.slice(0..5);
    assert_eq!(&s[..], b"hello");
    let s2 = line.line.slice(6..11);
    assert_eq!(&s2[..], b"world");
}

#[test]
fn lexer_line_slice_since() {
    let mut lexer = Lexer::new(b"abcdef\n");
    let mut line = lexer.temp_next_line().unwrap().unwrap();
    line.pos = 4;
    let s = line.line.slice(2..line.pos as usize);
    assert_eq!(&s[..], b"cd");
}

// ── Non-indented heredoc ──────────────────────────────────────

#[test]
fn heredoc_basic() {
    // my $x = <<END . "suffix";
    // hello
    // END
    let src = b"my $x = <<END . \"suffix\";\nhello\nEND\nmore code\n";
    let mut lexer = Lexer::new(src);

    // Line 1: the declaration line.
    let decl = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&decl.line[..], b"my $x = <<END . \"suffix\";");

    // Simulate the lexer: the introducer sits at `<<END`'s tail (pos 13, at ` . "suffix";`).  `start_heredoc` scans
    // for the terminator, suspends the introducer as the body frame's parent, and leaves the live line `None`.
    lexer.line = Some(LexerLine::for_test(decl.number, decl.offset, decl.line.clone(), decl.terminated, 13, true));
    lexer.start_heredoc(Bytes::from_static(b"END"), false, true).unwrap();

    // Body line.
    let body = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&body.line[..], b"hello");

    // Body exhausted → virtual EOF (the terminator was dropped by the scan and the cursor sits past it).
    assert!(lexer.temp_next_line().unwrap().is_none());

    // Pop the body frame (the lexer does this at SublexEnd): the suspended introducer returns as the live line.
    lexer.pop_context();
    let remainder = lexer.line.clone().unwrap();
    assert_eq!(remainder.pos, 13); // cursor preserved
    assert_eq!(&remainder.line[remainder.pos as usize..], b" . \"suffix\";");

    // Next line: code after the heredoc.
    let after = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&after.line[..], b"more code");
}

#[test]
fn heredoc_empty_body() {
    let src = b"<<END;\nEND\n";
    let mut lexer = Lexer::new(src);
    let decl = lexer.temp_next_line().unwrap().unwrap();

    lexer.line = Some(LexerLine::for_test(decl.number, decl.offset, decl.line, decl.terminated, 5, true));
    lexer.start_heredoc(Bytes::from_static(b"END"), false, true).unwrap();

    // Empty body → immediate virtual EOF.
    assert!(lexer.temp_next_line().unwrap().is_none());
}

#[test]
fn heredoc_unterminated() {
    let src = b"<<END;\nhello\nworld\n";
    let mut lexer = Lexer::new(src);
    let decl = lexer.temp_next_line().unwrap().unwrap();

    lexer.line = Some(LexerLine::for_test(decl.number, decl.offset, decl.line, decl.terminated, 5, true));

    // No terminator in the source: `start_heredoc`'s up-front scan reaches EOF and errors.
    assert!(lexer.start_heredoc(Bytes::from_static(b"END"), false, true).is_err());
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
    let mut lexer = Lexer::new(src);
    let decl = lexer.temp_next_line().unwrap().unwrap();

    // Start <<A: introducer at pos 4 (after "<<A").
    lexer.line = Some(LexerLine::for_test(decl.number, decl.offset, decl.line.clone(), decl.terminated, 4, true));
    lexer.start_heredoc(Bytes::from_static(b"A"), false, true).unwrap();

    // A's body, then virtual EOF.
    let body_a = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&body_a.line[..], b"body A");
    assert!(lexer.temp_next_line().unwrap().is_none());

    // Pop A: remainder ", <<B);" restored at pos 4.
    lexer.pop_context();
    let remainder = lexer.line.clone().unwrap();
    assert_eq!(remainder.pos, 4);

    // Start <<B from the remainder: introducer at pos 10 (after ", <<B").
    lexer.line = Some(LexerLine::for_test(remainder.number, remainder.offset, remainder.line.clone(), remainder.terminated, 10, true));
    lexer.start_heredoc(Bytes::from_static(b"B"), false, true).unwrap();

    // B's body, then virtual EOF.
    let body_b = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&body_b.line[..], b"body B");
    assert!(lexer.temp_next_line().unwrap().is_none());

    // Pop B: remainder ");" restored at pos 10.
    lexer.pop_context();
    let remainder2 = lexer.line.clone().unwrap();
    assert_eq!(remainder2.pos, 10);

    // After the heredocs.
    let after = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&after.line[..], b"after");
}

// ── Indented heredoc ──────────────────────────────────────────

#[test]
fn heredoc_indented() {
    let src = b"<<~END;\n    hello\n    world\n    END\n";
    let mut lexer = Lexer::new(src);
    let decl = lexer.temp_next_line().unwrap().unwrap();

    lexer.line = Some(LexerLine::for_test(decl.number, decl.offset, decl.line, decl.terminated, 6, true));
    lexer.start_heredoc(Bytes::from_static(b"END"), true, true).unwrap();

    // `<<~` records the required indent rather than stripping it: the physical offset and the full line are unchanged,
    // and the cursor (`pos`) is stamped past the 4-space indent.
    // Source: "<<~END;\n    hello\n    world\n    END\n", body lines at raw offsets 8 and 18.
    let l1 = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&l1.line[..], b"    hello");
    assert_eq!(l1.offset, 8);
    assert_eq!(l1.pos, 4);
    assert_eq!(&l1.line[l1.pos as usize..], b"hello");
    let l2 = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&l2.line[..], b"    world");
    assert_eq!(l2.offset, 18);
    assert_eq!(l2.pos, 4);

    // Terminator → None.
    assert!(lexer.temp_next_line().unwrap().is_none());
}

#[test]
fn heredoc_indented_empty_lines() {
    // Empty lines are allowed without indentation.
    let src = b"<<~END;\n    hello\n\n    world\n    END\n";
    let mut lexer = Lexer::new(src);
    let decl = lexer.temp_next_line().unwrap().unwrap();

    lexer.line = Some(LexerLine::for_test(decl.number, decl.offset, decl.line, decl.terminated, 6, true));
    lexer.start_heredoc(Bytes::from_static(b"END"), true, true).unwrap();

    let l1 = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&l1.line[..], b"    hello");
    assert_eq!(l1.pos, 4);
    let l2 = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&l2.line[..], b""); // empty line — a blank, no indent to record
    assert_eq!(l2.pos, 0);
    let l3 = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&l3.line[..], b"    world");
    assert_eq!(l3.pos, 4);
    assert!(lexer.temp_next_line().unwrap().is_none());
}

#[test]
fn heredoc_indented_mismatch() {
    // Body line with wrong indentation.
    let src = b"<<~END;\n    hello\n  bad\n    END\n";
    let mut lexer = Lexer::new(src);
    let decl = lexer.temp_next_line().unwrap().unwrap();

    lexer.line = Some(LexerLine::for_test(decl.number, decl.offset, decl.line, decl.terminated, 6, true));

    // "  bad" carries too little indent; the up-front indent stamping in `start_heredoc` rejects it.
    assert!(lexer.start_heredoc(Bytes::from_static(b"END"), true, true).is_err());
}

// ── Nested heredocs ───────────────────────────────────────────

#[test]
fn heredoc_non_indented_inside_indented() {
    // <<~OUTER with <<INNER inside
    //     <<INNER body line
    //     INNER
    //     outer body continues
    //     OUTER
    // A plain `<<INNER` opened inside a `<<~OUTER` body: INNER's body inherits OUTER's required indent (verified
    // against perl), so the nested body lines keep `pos = 4` even though INNER itself is non-indented — INNER's scan
    // adds no new indent, so OUTER's stamp persists.
    let src = b"<<~OUTER;\n    prefix <<INNER suffix\n    inner body\n    INNER\n    outer continues\n    OUTER\n";
    let mut lexer = Lexer::new(src);
    let decl = lexer.temp_next_line().unwrap().unwrap();

    // Start <<~OUTER: introducer at pos 9 (at ";").
    lexer.line = Some(LexerLine::for_test(decl.number, decl.offset, decl.line, decl.terminated, 9, true));
    lexer.start_heredoc(Bytes::from_static(b"OUTER"), true, true).unwrap();

    // OUTER body line 1: indent recorded (pos = 4), not stripped.
    let l1 = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&l1.line[..], b"    prefix <<INNER suffix");
    assert_eq!(l1.offset, 10);
    assert_eq!(l1.pos, 4);

    // Start <<INNER from that line: introducer at pos 18 (after the 4-space indent + "prefix <<INNER").  Its scan is
    // ceilinged at OUTER's bound, and adds no new indent.
    lexer.line = Some(LexerLine::for_test(l1.number, l1.offset, l1.line.clone(), l1.terminated, 18, true));
    lexer.start_heredoc(Bytes::from_static(b"INNER"), false, true).unwrap();

    // INNER body inherits OUTER's indent: pos = 4, the indent still in the line.
    let inner_body = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&inner_body.line[..], b"    inner body");
    assert_eq!(inner_body.pos, 4);
    assert!(lexer.temp_next_line().unwrap().is_none());

    // Pop INNER: the OUTER body line resumes at pos 18, tail " suffix".
    lexer.pop_context();
    let remainder = lexer.line.clone().unwrap();
    assert_eq!(remainder.pos, 18);
    assert_eq!(&remainder.line[remainder.pos as usize..], b" suffix");

    // OUTER body continues (indent recorded), then OUTER's virtual EOF.
    let l2 = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&l2.line[..], b"    outer continues");
    assert_eq!(l2.pos, 4);
    assert!(lexer.temp_next_line().unwrap().is_none());
}

// ── ascii_only flag ─────────────────────────────────────

#[test]
fn ascii_only_pure_ascii_line() {
    let mut lexer = Lexer::new(b"hello world\n");
    let line = lexer.temp_next_line().unwrap().unwrap();
    assert!(line.ascii_only, "pure ASCII line should have ascii_only = true");
}

#[test]
fn ascii_only_empty_line() {
    let mut lexer = Lexer::new(b"\n");
    let line = lexer.temp_next_line().unwrap().unwrap();
    assert!(line.ascii_only, "empty line should have ascii_only = true");
}

#[test]
fn ascii_only_with_high_bytes() {
    let mut lexer = Lexer::new("café\n".as_bytes());
    let line = lexer.temp_next_line().unwrap().unwrap();
    assert!(!line.ascii_only, "line with UTF-8 should have ascii_only = false");
}

#[test]
fn ascii_only_high_byte_at_end() {
    let mut lexer = Lexer::new(b"hello\xff\n");
    let line = lexer.temp_next_line().unwrap().unwrap();
    assert!(!line.ascii_only, "line with high byte should have ascii_only = false");
}

#[test]
fn ascii_only_high_byte_at_start() {
    let mut lexer = Lexer::new(b"\x80rest\n");
    let line = lexer.temp_next_line().unwrap().unwrap();
    assert!(!line.ascii_only, "line starting with high byte should have ascii_only = false");
}

#[test]
fn ascii_only_multiline_mixed() {
    let mut lexer = Lexer::new("ascii\ncafé\nmore ascii\n".as_bytes());
    let l1 = lexer.temp_next_line().unwrap().unwrap();
    assert!(l1.ascii_only, "first line is ASCII");
    let l2 = lexer.temp_next_line().unwrap().unwrap();
    assert!(!l2.ascii_only, "second line has UTF-8");
    let l3 = lexer.temp_next_line().unwrap().unwrap();
    assert!(l3.ascii_only, "third line is ASCII");
}

#[test]
fn ascii_only_unterminated_line() {
    let mut lexer = Lexer::new(b"no newline");
    let line = lexer.temp_next_line().unwrap().unwrap();
    assert!(line.ascii_only, "unterminated ASCII line should have ascii_only = true");
}

#[test]
fn ascii_only_unterminated_with_utf8() {
    let mut lexer = Lexer::new("no newline café".as_bytes());
    let line = lexer.temp_next_line().unwrap().unwrap();
    assert!(!line.ascii_only, "unterminated UTF-8 line should have ascii_only = false");
}

#[test]
fn ascii_only_crlf_line() {
    let mut lexer = Lexer::new(b"hello\r\n");
    let line = lexer.temp_next_line().unwrap().unwrap();
    assert!(line.ascii_only, "CRLF line with ASCII content should have ascii_only = true");
}

#[test]
fn ascii_only_only_control_chars() {
    // Control chars (0x01..0x1F) are all < 0x80.
    let mut lexer = Lexer::new(b"\x01\x1f\t\n");
    let line = lexer.temp_next_line().unwrap().unwrap();
    assert!(line.ascii_only, "control chars are ASCII");
}

#[test]
fn ascii_only_boundary_byte_0x7f() {
    // 0x7F (DEL) is the highest ASCII byte.
    let mut lexer = Lexer::new(b"\x7f\n");
    let line = lexer.temp_next_line().unwrap().unwrap();
    assert!(line.ascii_only, "0x7F is still ASCII");
}

#[test]
fn ascii_only_boundary_byte_0x80() {
    // 0x80 is the first non-ASCII byte.
    let mut lexer = Lexer::new(b"\x80\n");
    let line = lexer.temp_next_line().unwrap().unwrap();
    assert!(!line.ascii_only, "0x80 is not ASCII");
}

#[test]
fn ascii_only_heredoc_body_lines() {
    // Heredoc body lines should have correct ascii_only flags.
    let mut lexer = Lexer::new("<<END\nascii line\ncaf\u{00E9} line\nEND\n".as_bytes());
    let decl = lexer.temp_next_line().unwrap().unwrap();
    assert!(decl.ascii_only, "declaration line is ASCII");

    // Start heredoc.
    lexer.line = Some(LexerLine::for_test(decl.number, decl.offset, decl.line, decl.terminated, 5, decl.ascii_only));
    lexer.start_heredoc(Bytes::from_static(b"END"), false, true).unwrap();

    // First body line: ASCII.
    let body1 = lexer.temp_next_line().unwrap().unwrap();
    assert!(body1.ascii_only, "first heredoc body line is ASCII");

    // Second body line: has UTF-8.
    let body2 = lexer.temp_next_line().unwrap().unwrap();
    assert!(!body2.ascii_only, "second heredoc body line has UTF-8");

    // Terminator → None.
    assert!(lexer.temp_next_line().unwrap().is_none());
}

// ── Adversarial edge cases ───────────────────────────────

#[test]
fn heredoc_terminator_at_eof_without_newline() {
    let src = b"<<END\nbody\nEND";
    let mut lexer = Lexer::new(src);
    let line = lexer.temp_next_line().unwrap().unwrap();
    lexer.line = Some(line);
    lexer.start_heredoc(Bytes::from("END"), false, true).unwrap();

    let body = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&body.line[..], b"body");

    // Terminator line at EOF still terminates.
    assert!(matches!(lexer.temp_next_line(), Ok(None)));
}

#[test]
fn heredoc_virtual_eof_is_stable_then_pop_restores_introducer() {
    let mut lexer = Lexer::new(b"body\nEND\nrest\n");

    lexer.line = Some(LexerLine::for_test(999, 123, Bytes::from_static(b"saved"), false, 0, true));
    lexer.start_heredoc(Bytes::from_static(b"END"), false, true).unwrap();

    let body = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&body.line[..], b"body");

    // Virtual EOF is stable: line delivery keeps returning None at the bound; it does not itself pop the frame.
    assert!(matches!(lexer.temp_next_line(), Ok(None)));
    assert!(matches!(lexer.temp_next_line(), Ok(None)));

    // Popping the frame (what the lexer does at SublexEnd) restores the saved introducer, then delivery resumes.
    lexer.pop_context();
    let restored = lexer.line.clone().unwrap();
    assert_eq!(&restored.line[..], b"saved");

    let rest = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&rest.line[..], b"rest");
}

#[test]
fn subst_body_reads_flags_and_restores_remainder() {
    let mut lexer = Lexer::new(b"");
    lexer.line = Some(LexerLine::for_test(1, 0, Bytes::from_static(b"bar/e + 1"), false, 0, true));

    // Scan the `/`-delimited body: it becomes the live (narrowed) line, and the parent is suspended past the close.
    lexer.start_delimited_body(LexContext::new(Some('/'), FrameRole::String), false).unwrap();
    let body = lexer.line.clone().unwrap();
    assert_eq!(&body.line[body.pos as usize..], b"bar");

    // Flags sit on the suspended parent, immediately past the close.
    let flags = lexer.read_quote_flags();
    assert_eq!(flags.as_deref(), Some("e"));

    // Popping restores the parent past the flags.
    lexer.pop_context();
    let rest = lexer.line.clone().unwrap();
    assert_eq!(&rest.line[rest.pos as usize..], b" + 1");
}

#[test]
fn subst_body_captures_multiple_flags_and_restores_remainder() {
    let mut lexer = Lexer::new(b"");
    lexer.line = Some(LexerLine::for_test(1, 0, Bytes::from_static(b"bar/msix + 1"), false, 0, true));

    lexer.start_delimited_body(LexContext::new(Some('/'), FrameRole::String), false).unwrap();
    let body = lexer.line.clone().unwrap();
    assert_eq!(&body.line[body.pos as usize..], b"bar");

    let flags = lexer.read_quote_flags();
    assert_eq!(flags.as_deref(), Some("msix"));

    lexer.pop_context();
    let rest = lexer.line.clone().unwrap();
    assert_eq!(&rest.line[rest.pos as usize..], b" + 1");
}

#[test]
fn subst_body_with_paired_delimiter_nesting() {
    let mut lexer = Lexer::new(b"");
    lexer.line = Some(LexerLine::for_test(1, 0, Bytes::from_static(b"a{b}c}r"), false, 0, true));

    // `{`-delimited: the inner `{...}` nests, so the body ends at the second `}`.
    lexer.start_delimited_body(LexContext::new(Some('{'), FrameRole::String), false).unwrap();
    let body = lexer.line.clone().unwrap();
    assert_eq!(&body.line[body.pos as usize..], b"a{b}c");

    let flags = lexer.read_quote_flags();
    assert_eq!(flags.as_deref(), Some("r"));
}

#[test]
fn subst_body_errors_on_eof() {
    let mut lexer = Lexer::new(b"");
    lexer.line = Some(LexerLine::for_test(1, 0, Bytes::from_static(b"unterminated"), false, 0, true));

    let err = lexer.start_delimited_body(LexContext::new(Some('/'), FrameRole::String), false).unwrap_err();
    assert!(err.message.contains("Can't find string terminator"));
}

#[test]
fn indented_heredoc_empty_required_indent_delivers_body_as_is() {
    let mut lexer = Lexer::new(b"  body\nEND\n");
    // Terminator "END" has no indent, so the required indent is empty and body lines are delivered unchanged.
    lexer.line = Some(LexerLine::for_test(1, 0, Bytes::from_static(b"saved"), false, 0, true));
    lexer.start_heredoc(Bytes::from_static(b"END"), true, true).unwrap();

    // First body line still comes through, indent intact.
    let body = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&body.line[..], b"  body");

    // Terminator ends the heredoc.
    assert!(matches!(lexer.temp_next_line(), Ok(None)));
}

#[test]
fn filename_is_preserved() {
    let lexer = Lexer::with_filename(b"1;\n", "foo.pl");
    assert_eq!(lexer.filename(), "foo.pl");

    // Default filename should be a sensible default.
    let default = Lexer::new(b"1;\n");
    assert!(!default.filename().is_empty(), "default filename should not be empty");
}

#[test]
fn push_back_precedes_underlying_source() {
    let mut lexer = Lexer::new(b"real\n");
    let mut q = VecDeque::new();
    q.push_back(LexerLine::for_test(999, 123, Bytes::from_static(b"queued"), false, 0, true));
    lexer.push_back(q);

    let first = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&first.line[..], b"queued", "push_back lines should come before underlying source");
    assert_eq!(first.number, 1, "delivered lines are renumbered from the counter, not from the pushed value");

    let second = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&second.line[..], b"real", "underlying source should follow pushed lines");
    assert_eq!(second.number, 2, "the following source line continues the delivery numbering");
}

// ── Lookahead ─────────────────────────────────────────────────

#[test]
fn lookahead_rewinds_to_entry_line_and_pos() {
    let mut lexer = Lexer::new(b"aaa\nbbb\nccc\nddd\n");
    let _ = lexer.temp_next_line(); // deliver line 1 "aaa"
    if let Some(l) = lexer.line.as_mut() {
        l.pos = 2; // advance the cursor within line 1
    }

    {
        let mut g = lexer.lookahead();
        let l2 = g.next_line().unwrap().clone().unwrap();
        assert_eq!(&l2.line[..], b"bbb");
        let l3 = g.next_line().unwrap().clone().unwrap();
        assert_eq!(&l3.line[..], b"ccc");
    } // guard drops -> snap back to line 1 at pos 2

    let cur = lexer.line.clone().unwrap();
    assert_eq!(&cur.line[..], b"aaa");
    assert_eq!(cur.pos, 2);
    assert_eq!(cur.number, 1);
}

#[test]
fn lookahead_previewed_lines_replay_in_order_with_numbers() {
    let mut lexer = Lexer::new(b"aaa\nbbb\nccc\nddd\n");
    let _ = lexer.temp_next_line(); // line 1
    {
        let mut g = lexer.lookahead();
        let _ = g.next_line(); // bbb
        let _ = g.next_line(); // ccc
    }

    // Previewed lines replay before fresh reads, renumbered from the rewound counter (aaa=1, so bbb=2, ccc=3, ddd=4).
    let b = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&b.line[..], b"bbb");
    assert_eq!(b.number, 2);
    let c = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&c.line[..], b"ccc");
    assert_eq!(c.number, 3);
    let d = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&d.line[..], b"ddd");
    assert_eq!(d.number, 4);
    assert!(matches!(lexer.temp_next_line(), Ok(None)));
}

#[test]
fn lookahead_suppresses_line_directive_setters() {
    let mut lexer = Lexer::new(b"aaa\nbbb\n");
    let _ = lexer.temp_next_line();
    let saved_number = lexer.line_number;
    let saved_name = lexer.filename().to_string();

    {
        let mut g = lexer.lookahead();
        g.set_line_number(999);
        g.set_filename("other".to_string());
        assert_eq!(g.line_number, saved_number, "set_line_number is suppressed during a scan");
        assert_eq!(g.filename(), saved_name, "set_filename is suppressed during a scan");
    }

    // After the scan the setters work again.
    lexer.set_line_number(50);
    assert_eq!(lexer.line_number, 50);
}

#[test]
fn lookahead_directive_applies_to_replayed_lines() {
    // A `# line` directive the lexer processes on the real pass renumbers what follows.  Driven here at the source
    // layer: scan ahead, rewind, then on replay set the counter between deliveries the way the parser would.
    let mut lexer = Lexer::new(b"aaa\nbbb\nccc\n");
    let _ = lexer.temp_next_line(); // line 1
    {
        let mut g = lexer.lookahead();
        let _ = g.next_line(); // bbb
        let _ = g.next_line(); // ccc
    }

    let b = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(b.number, 2, "bbb re-stamped from the rewound counter");
    lexer.set_line_number(100); // directive encountered on the real pass
    let c = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(c.number, 100, "ccc picks up the directive on replay");
}

#[test]
fn consume_lookahead_commits_to_scan_end() {
    let mut lexer = Lexer::new(b"aaa\nbbb\nccc\nddd\n");
    let _ = lexer.temp_next_line(); // line 1 "aaa"
    {
        let mut g = lexer.lookahead();
        let _ = g.next_line(); // bbb
        let _ = g.next_line(); // ccc (scan ends here)
    }

    lexer.consume_lookahead().unwrap();
    let cur = lexer.line.clone().unwrap();
    assert_eq!(&cur.line[..], b"ccc", "consume lands on the line the scan ended on");
    assert_eq!(cur.number, 3);

    let d = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&d.line[..], b"ddd");
    assert_eq!(d.number, 4, "numbering continues correctly past the consumed region");
}

#[test]
fn consume_lookahead_single_line_advances_pos() {
    // A scan that never crosses a line boundary: consume advances the cursor to the scan-end position.
    let mut lexer = Lexer::new(b"abcdef\n");
    let _ = lexer.temp_next_line(); // "abcdef" at pos 0
    {
        let mut g = lexer.lookahead();
        if let Some(l) = g.line.as_mut() {
            l.pos = 4; // scan forward within the line
        }
    } // drop -> pos restored to 0, scan-end pos (4) recorded

    assert_eq!(lexer.line.as_ref().unwrap().pos, 0);
    lexer.consume_lookahead().unwrap();
    assert_eq!(lexer.line.as_ref().unwrap().pos, 4);
}

#[test]
fn consume_lookahead_without_window_is_noop() {
    let mut lexer = Lexer::new(b"aaa\nbbb\n");
    let _ = lexer.temp_next_line();
    let before = lexer.line.as_ref().unwrap().pos;
    lexer.consume_lookahead().unwrap(); // no lookahead opened -> no-op
    assert_eq!(lexer.line.as_ref().unwrap().pos, before);
}

#[test]
fn consume_lookahead_after_replay_is_noop() {
    let mut lexer = Lexer::new(b"aaa\nbbb\nccc\n");
    let _ = lexer.temp_next_line(); // line 1
    {
        let mut g = lexer.lookahead();
        let _ = g.next_line(); // preview bbb
    }
    // Normal lexing replays bbb, then reads ccc fresh -- moving past the scan-end line closes the consume window.
    let _ = lexer.temp_next_line(); // bbb (replay)
    let _ = lexer.temp_next_line(); // ccc (fresh)

    let pos_before = lexer.line.as_ref().unwrap().pos;
    lexer.consume_lookahead().unwrap(); // window closed -> no-op
    let cur = lexer.line.clone().unwrap();
    assert_eq!(&cur.line[..], b"ccc");
    assert_eq!(cur.pos, pos_before);
}

#[test]
fn lookahead_replayed_lines_restart_at_pos_zero() {
    let mut lexer = Lexer::new(b"aaa\nbbb\nccc\n");
    let _ = lexer.temp_next_line(); // line 1
    {
        let mut g = lexer.lookahead();
        let _ = g.next_line(); // bbb becomes current
        if let Some(l) = g.line.as_mut() {
            l.pos = 2; // scan partway into bbb
        }
        let _ = g.next_line(); // ccc
    }

    // On replay, bbb restarts at the beginning despite the mid-line scan position.
    let b = lexer.temp_next_line().unwrap().unwrap();
    assert_eq!(&b.line[..], b"bbb");
    assert_eq!(b.pos, 0);
}
