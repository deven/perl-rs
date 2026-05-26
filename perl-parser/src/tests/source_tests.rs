//! Source-layer `Lexer` tests.

use super::*;

/// Helper: collect all lines from a source.
fn collect_lines(src: &str) -> Vec<String> {
    let mut lexer = Lexer::new(src.as_bytes());
    let mut lines = Vec::new();
    while let Ok(Some(line)) = lexer.temp_next_line(false) {
        lines.push(String::from_utf8_lossy(&line.line).into_owned());
    }
    lines
}

// ── Basic line splitting ──────────────────────────────────────

#[test]
fn empty_source() {
    let mut lexer = Lexer::new(b"");
    assert!(matches!(lexer.temp_next_line(false), Ok(None)));
}

#[test]
fn single_line_no_newline() {
    let mut lexer = Lexer::new(b"hello");
    let line = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&line.line[..], b"hello");
    assert!(!line.terminated);
    assert_eq!(line.number, 1);
    assert!(matches!(lexer.temp_next_line(false), Ok(None)));
}

#[test]
fn single_line_with_newline() {
    let mut lexer = Lexer::new(b"hello\n");
    let line = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&line.line[..], b"hello");
    assert!(line.terminated);
    assert!(matches!(lexer.temp_next_line(false), Ok(None)));
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
    assert_eq!(lexer.temp_next_line(false).unwrap().unwrap().number, 1);
    assert_eq!(lexer.temp_next_line(false).unwrap().unwrap().number, 2);
    assert_eq!(lexer.temp_next_line(false).unwrap().unwrap().number, 3);
}

#[test]
fn byte_offsets() {
    let mut lexer = Lexer::new(b"ab\ncde\nf\n");
    let l1 = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(l1.offset, 0);
    let l2 = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(l2.offset, 3);
    let l3 = lexer.temp_next_line(false).unwrap().unwrap();
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
    let line = lexer.temp_next_line(false).unwrap().unwrap();
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
    let mut line = lexer.temp_next_line(false).unwrap().unwrap();
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
    let mut line = lexer.temp_next_line(false).unwrap().unwrap();
    line.pos = 3;
    assert_eq!(line.remaining(), b"def");
}

#[test]
fn lexer_line_slice() {
    let mut lexer = Lexer::new(b"hello world\n");
    let line = lexer.temp_next_line(false).unwrap().unwrap();
    let s = line.line.slice(0..5);
    assert_eq!(&s[..], b"hello");
    let s2 = line.line.slice(6..11);
    assert_eq!(&s2[..], b"world");
}

#[test]
fn lexer_line_slice_since() {
    let mut lexer = Lexer::new(b"abcdef\n");
    let mut line = lexer.temp_next_line(false).unwrap().unwrap();
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
    let mut lexer = Lexer::new(src);

    // Line 1: the declaration line.
    let decl = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&decl.line[..], b"my $x = <<END . \"suffix\";");

    // Simulate lexer: found <<END at some position in decl.  Save the remainder and start the heredoc.
    let mut current_line = Some(LexerLine {
        number: decl.number,
        offset: decl.offset,
        line: decl.line.clone(),
        terminated: decl.terminated,
        pos: 13, // pointing at ` . "suffix";`
        ascii_only: true,
    });
    lexer.line = current_line.take();
    lexer.start_heredoc(Bytes::from_static(b"END")).unwrap();
    assert!(current_line.is_none());

    // Next line: heredoc body.
    let body = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&body.line[..], b"hello");

    // Next line: terminator → None.
    assert!(lexer.temp_next_line(false).unwrap().is_none());

    // Next line: saved remainder (the declaration tail).
    let remainder = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(remainder.pos, 13); // cursor preserved
    assert_eq!(&remainder.line[remainder.pos..], b" . \"suffix\";");

    // Next line: code after the heredoc.
    let after = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&after.line[..], b"more code");
}

#[test]
fn heredoc_empty_body() {
    let src = b"<<END;\nEND\n";
    let mut lexer = Lexer::new(src);
    let decl = lexer.temp_next_line(false).unwrap().unwrap();

    let mut current = Some(LexerLine { number: decl.number, offset: decl.offset, line: decl.line, terminated: decl.terminated, pos: 5, ascii_only: true });
    lexer.line = current.take();
    lexer.start_heredoc(Bytes::from_static(b"END")).unwrap();

    // Immediate terminator → None.
    assert!(lexer.temp_next_line(false).unwrap().is_none());
}

#[test]
fn heredoc_unterminated() {
    let src = b"<<END;\nhello\nworld\n";
    let mut lexer = Lexer::new(src);
    let decl = lexer.temp_next_line(false).unwrap().unwrap();

    let mut current = Some(LexerLine { number: decl.number, offset: decl.offset, line: decl.line, terminated: decl.terminated, pos: 5, ascii_only: true });
    lexer.line = current.take();
    lexer.start_heredoc(Bytes::from_static(b"END")).unwrap();

    // Read body lines.
    lexer.temp_next_line(false).unwrap().unwrap(); // hello
    lexer.temp_next_line(false).unwrap().unwrap(); // world

    // EOF without terminator → error.
    assert!(lexer.temp_next_line(false).is_err());
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
    let decl = lexer.temp_next_line(false).unwrap().unwrap();

    // Start <<A, save remainder ", <<B);"
    let mut current = Some(LexerLine {
        number: decl.number,
        offset: decl.offset,
        line: decl.line.clone(),
        terminated: decl.terminated,
        pos: 4, // after "<<A"
        ascii_only: true,
    });
    lexer.line = current.take();
    lexer.start_heredoc(Bytes::from_static(b"A")).unwrap();

    // A's body.
    let body_a = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&body_a.line[..], b"body A");

    // A's terminator → None.
    assert!(lexer.temp_next_line(false).unwrap().is_none());

    // Remainder restored: ", <<B);"
    let remainder = lexer.temp_next_line(false).unwrap().unwrap();
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
    lexer.line = current.take();
    lexer.start_heredoc(Bytes::from_static(b"B")).unwrap();

    // B's body.
    let body_b = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&body_b.line[..], b"body B");

    // B's terminator → None.
    assert!(lexer.temp_next_line(false).unwrap().is_none());

    // Remainder restored: ");"
    let remainder2 = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(remainder2.pos, 10);

    // After heredocs.
    let after = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&after.line[..], b"after");
}

// ── Indented heredoc ──────────────────────────────────────────

#[test]
fn heredoc_indented() {
    let src = b"<<~END;\n    hello\n    world\n    END\n";
    let mut lexer = Lexer::new(src);
    let decl = lexer.temp_next_line(false).unwrap().unwrap();

    let mut current = Some(LexerLine { number: decl.number, offset: decl.offset, line: decl.line, terminated: decl.terminated, pos: 6, ascii_only: true });
    lexer.line = current.take();
    lexer.start_indented_heredoc(Bytes::from_static(b"END")).unwrap();

    // Body lines with indent stripped.
    // Source: "<<~END;\n    hello\n    world\n    END\n"
    //          0       8          18
    // "    hello" at raw offset 8, 4-byte indent stripped → offset 12.
    let l1 = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&l1.line[..], b"hello");
    assert_eq!(l1.offset, 12);
    let l2 = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&l2.line[..], b"world");
    assert_eq!(l2.offset, 22);

    // Terminator → None.
    assert!(lexer.temp_next_line(false).unwrap().is_none());
}

#[test]
fn heredoc_indented_empty_lines() {
    // Empty lines are allowed without indentation.
    let src = b"<<~END;\n    hello\n\n    world\n    END\n";
    let mut lexer = Lexer::new(src);
    let decl = lexer.temp_next_line(false).unwrap().unwrap();

    let mut current = Some(LexerLine { number: decl.number, offset: decl.offset, line: decl.line, terminated: decl.terminated, pos: 6, ascii_only: true });
    lexer.line = current.take();
    lexer.start_indented_heredoc(Bytes::from_static(b"END")).unwrap();

    let l1 = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&l1.line[..], b"hello");
    let l2 = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&l2.line[..], b""); // empty line
    let l3 = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&l3.line[..], b"world");
    assert!(lexer.temp_next_line(false).unwrap().is_none());
}

#[test]
fn heredoc_indented_mismatch() {
    // Body line with wrong indentation.
    let src = b"<<~END;\n    hello\n  bad\n    END\n";
    let mut lexer = Lexer::new(src);
    let decl = lexer.temp_next_line(false).unwrap().unwrap();

    let mut current = Some(LexerLine { number: decl.number, offset: decl.offset, line: decl.line, terminated: decl.terminated, pos: 6, ascii_only: true });
    lexer.line = current.take();
    lexer.start_indented_heredoc(Bytes::from_static(b"END")).unwrap();

    let l1 = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&l1.line[..], b"hello");

    // Next line has wrong indent → error.
    assert!(lexer.temp_next_line(false).is_err());
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
    let mut lexer = Lexer::new(src);
    let decl = lexer.temp_next_line(false).unwrap().unwrap();

    // Start <<~OUTER
    let mut current = Some(LexerLine { number: decl.number, offset: decl.offset, line: decl.line, terminated: decl.terminated, pos: 9, ascii_only: true });
    lexer.line = current.take();
    lexer.start_indented_heredoc(Bytes::from_static(b"OUTER")).unwrap();

    // First body line of OUTER (indent stripped).
    let l1 = lexer.temp_next_line(false).unwrap().unwrap();
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
    lexer.line = current.take();
    lexer.start_heredoc(Bytes::from_static(b"INNER")).unwrap();

    // INNER body (outer indent still stripped).
    let inner_body = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&inner_body.line[..], b"inner body");

    // INNER terminator → None.
    assert!(lexer.temp_next_line(false).unwrap().is_none());

    // Remainder of OUTER body line restored.
    let remainder = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(remainder.pos, 14);
    assert_eq!(&remainder.line[remainder.pos..], b" suffix");

    // OUTER body continues.
    let l2 = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&l2.line[..], b"outer continues");

    // OUTER terminator → None.
    assert!(lexer.temp_next_line(false).unwrap().is_none());
}

// ── ascii_only flag ─────────────────────────────────────

#[test]
fn ascii_only_pure_ascii_line() {
    let mut lexer = Lexer::new(b"hello world\n");
    let line = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(line.ascii_only, "pure ASCII line should have ascii_only = true");
}

#[test]
fn ascii_only_empty_line() {
    let mut lexer = Lexer::new(b"\n");
    let line = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(line.ascii_only, "empty line should have ascii_only = true");
}

#[test]
fn ascii_only_with_high_bytes() {
    let mut lexer = Lexer::new("café\n".as_bytes());
    let line = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(!line.ascii_only, "line with UTF-8 should have ascii_only = false");
}

#[test]
fn ascii_only_high_byte_at_end() {
    let mut lexer = Lexer::new(b"hello\xff\n");
    let line = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(!line.ascii_only, "line with high byte should have ascii_only = false");
}

#[test]
fn ascii_only_high_byte_at_start() {
    let mut lexer = Lexer::new(b"\x80rest\n");
    let line = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(!line.ascii_only, "line starting with high byte should have ascii_only = false");
}

#[test]
fn ascii_only_multiline_mixed() {
    let mut lexer = Lexer::new("ascii\ncafé\nmore ascii\n".as_bytes());
    let l1 = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(l1.ascii_only, "first line is ASCII");
    let l2 = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(!l2.ascii_only, "second line has UTF-8");
    let l3 = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(l3.ascii_only, "third line is ASCII");
}

#[test]
fn ascii_only_unterminated_line() {
    let mut lexer = Lexer::new(b"no newline");
    let line = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(line.ascii_only, "unterminated ASCII line should have ascii_only = true");
}

#[test]
fn ascii_only_unterminated_with_utf8() {
    let mut lexer = Lexer::new("no newline café".as_bytes());
    let line = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(!line.ascii_only, "unterminated UTF-8 line should have ascii_only = false");
}

#[test]
fn ascii_only_crlf_line() {
    let mut lexer = Lexer::new(b"hello\r\n");
    let line = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(line.ascii_only, "CRLF line with ASCII content should have ascii_only = true");
}

#[test]
fn ascii_only_only_control_chars() {
    // Control chars (0x01..0x1F) are all < 0x80.
    let mut lexer = Lexer::new(b"\x01\x1f\t\n");
    let line = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(line.ascii_only, "control chars are ASCII");
}

#[test]
fn ascii_only_boundary_byte_0x7f() {
    // 0x7F (DEL) is the highest ASCII byte.
    let mut lexer = Lexer::new(b"\x7f\n");
    let line = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(line.ascii_only, "0x7F is still ASCII");
}

#[test]
fn ascii_only_boundary_byte_0x80() {
    // 0x80 is the first non-ASCII byte.
    let mut lexer = Lexer::new(b"\x80\n");
    let line = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(!line.ascii_only, "0x80 is not ASCII");
}

#[test]
fn ascii_only_heredoc_body_lines() {
    // Heredoc body lines should have correct ascii_only flags.
    let mut lexer = Lexer::new("<<END\nascii line\ncaf\u{00E9} line\nEND\n".as_bytes());
    let decl = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(decl.ascii_only, "declaration line is ASCII");

    // Start heredoc.
    let mut current =
        Some(LexerLine { number: decl.number, offset: decl.offset, line: decl.line, terminated: decl.terminated, pos: 5, ascii_only: decl.ascii_only });
    lexer.line = current.take();
    lexer.start_heredoc(Bytes::from_static(b"END")).unwrap();

    // First body line: ASCII.
    let body1 = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(body1.ascii_only, "first heredoc body line is ASCII");

    // Second body line: has UTF-8.
    let body2 = lexer.temp_next_line(false).unwrap().unwrap();
    assert!(!body2.ascii_only, "second heredoc body line has UTF-8");

    // Terminator → None.
    assert!(lexer.temp_next_line(false).unwrap().is_none());
}

// ── Adversarial edge cases ───────────────────────────────

#[test]
fn heredoc_terminator_at_eof_without_newline() {
    let src = b"<<END\nbody\nEND";
    let mut lexer = Lexer::new(src);
    let line = lexer.temp_next_line(false).unwrap().unwrap();
    lexer.line = Some(line);
    lexer.start_heredoc(Bytes::from("END")).unwrap();

    let body = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&body.line[..], b"body");

    // Terminator line at EOF still terminates.
    assert!(matches!(lexer.temp_next_line(false), Ok(None)));
}

#[test]
fn peeked_heredoc_terminator_stays_pending_until_consumed() {
    let mut lexer = Lexer::new(b"body\nEND\nrest\n");

    let mut current_line = Some(LexerLine { number: 999, offset: 123, line: Bytes::from_static(b"saved"), terminated: false, pos: 0, ascii_only: true });

    lexer.line = current_line.take();
    lexer.start_heredoc(Bytes::from_static(b"END")).unwrap();

    let body = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&body.line[..], b"body");

    // Peek sees end-of-heredoc but does not consume it.
    assert!(matches!(lexer.temp_next_line(true), Ok(None)));

    // Repeated peeks still see pending end.
    assert!(matches!(lexer.temp_next_line(true), Ok(None)));

    // Consuming call now pops the heredoc and restores saved line.
    assert!(matches!(lexer.temp_next_line(false), Ok(None)));

    let restored = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&restored.line[..], b"saved");

    let rest = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&rest.line[..], b"rest");
}

#[test]
fn subst_body_virtual_eof_restores_remainder() {
    let mut lexer = Lexer::new(b"");

    let mut current_line = Some(LexerLine { number: 1, offset: 0, line: Bytes::from_static(b"bar/e + 1"), terminated: false, pos: 0, ascii_only: true });

    lexer.line = current_line.take();
    let flags = lexer.start_subst_body('/', false).unwrap();
    assert_eq!(flags.as_deref(), Some("e"));

    let body = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&body.line[body.pos..], b"bar");

    // Virtual EOF after all body lines are delivered.
    assert!(matches!(lexer.temp_next_line(false), Ok(None)));

    // Then the saved remainder appears.
    let rest = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&rest.line[rest.pos..], b" + 1");
}

#[test]
fn subst_body_captures_multiple_flags_and_restores_remainder() {
    let mut lexer = Lexer::new(b"");
    let mut current_line = Some(LexerLine { number: 1, offset: 0, line: Bytes::from_static(b"bar/msix + 1"), terminated: false, pos: 0, ascii_only: true });

    lexer.line = current_line.take();
    let flags = lexer.start_subst_body('/', false).unwrap();
    assert_eq!(flags.as_deref(), Some("msix"));

    let body = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&body.line[body.pos..], b"bar");

    assert!(matches!(lexer.temp_next_line(false), Ok(None)));

    let rest = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&rest.line[rest.pos..], b" + 1");
}

#[test]
fn subst_body_with_paired_delimiter_nesting() {
    let mut lexer = Lexer::new(b"");
    let mut current_line = Some(LexerLine { number: 1, offset: 0, line: Bytes::from_static(b"a{b}c}r"), terminated: false, pos: 0, ascii_only: true });

    lexer.line = current_line.take();
    let flags = lexer.start_subst_body('{', false).unwrap();
    assert_eq!(flags.as_deref(), Some("r"));

    let body = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&body.line[body.pos..], b"a{b}c");

    assert!(matches!(lexer.temp_next_line(false), Ok(None)));
}

#[test]
fn subst_body_errors_on_eof() {
    let mut lexer = Lexer::new(b"");
    let mut current_line = Some(LexerLine { number: 1, offset: 0, line: Bytes::from_static(b"unterminated"), terminated: false, pos: 0, ascii_only: true });

    lexer.line = current_line.take();
    let err = lexer.start_subst_body('/', false).unwrap_err();
    assert!(err.message.contains("unterminated substitution"));
}

#[test]
fn indented_heredoc_errors_on_mismatched_indent_after_start() {
    let mut lexer = Lexer::new(b"  body\nEND\n");
    let mut current_line = Some(LexerLine { number: 1, offset: 0, line: Bytes::from_static(b"saved"), terminated: false, pos: 0, ascii_only: true });

    // Terminator has no indent, so required indent becomes empty.
    lexer.line = current_line.take();
    lexer.start_indented_heredoc(Bytes::from_static(b"END")).unwrap();

    // First body line should still come through.
    let body = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&body.line[..], b"  body");

    // Terminator ends the heredoc.
    assert!(matches!(lexer.temp_next_line(false), Ok(None)));
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
    q.push_back(LexerLine { number: 999, offset: 123, line: Bytes::from_static(b"queued"), terminated: false, pos: 0, ascii_only: true });
    lexer.push_back(q);

    let first = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&first.line[..], b"queued", "push_back lines should come before underlying source");
    assert_eq!(first.number, 999, "pushed line number should be preserved");

    let second = lexer.temp_next_line(false).unwrap().unwrap();
    assert_eq!(&second.line[..], b"real", "underlying source should follow pushed lines");
}
