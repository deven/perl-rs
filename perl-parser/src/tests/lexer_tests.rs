//! Lexer tests.

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
            | Token::SublexEnd
            | Token::TranslitLit(_, _, _)
            | Token::HeredocLit(_, _, _)
            | Token::Readline(_, _)
            | Token::GlobVar(_)
            | Token::QwList(_)
            | Token::SpecialArrayVar(_)
            | Token::SpecialHashVar(_)
            | Token::Arrow
            | Token::SourceFile(_)
            | Token::SourceLine(_)
            | Token::CurrentPackage
            | Token::CurrentSub
            | Token::CurrentClass => {
                term_context = false;
            }
            // Sub-tokens inside strings/regex don't change context.
            Token::QuoteSublexBegin(_, _)
            | Token::RegexSublexBegin(_, _)
            | Token::SubstSublexBegin(_)
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
    assert_eq!(heredoc_tok, Token::QuoteSublexBegin(QuoteKind::Heredoc, 0));
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
    assert_eq!(tokens[0], Token::QuoteSublexBegin(QuoteKind::Heredoc, 0));
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
    assert_eq!(tokens, vec![Token::QuoteSublexBegin(QuoteKind::Double, b'"'), Token::ConstSegment("world\n".into()), Token::SublexEnd,]);
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
            Token::QuoteSublexBegin(QuoteKind::Double, b'"'),
            Token::ConstSegment("Hello, world!\n".into()),
            Token::SublexEnd,
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
            Token::QuoteSublexBegin(QuoteKind::Double, b'"'),
            Token::ConstSegment("Hello, ".into()),
            Token::InterpScalar("name".into()),
            Token::ConstSegment("!".into()),
            Token::SublexEnd,
        ]
    );
}

#[test]
fn lex_interp_braced() {
    let tokens = lex_all(r#""${name}bar""#);
    assert_eq!(
        tokens,
        vec![Token::QuoteSublexBegin(QuoteKind::Double, b'"'), Token::InterpScalar("name".into()), Token::ConstSegment("bar".into()), Token::SublexEnd,]
    );
}

#[test]
fn lex_interp_array() {
    let tokens = lex_all(r#""items: @list.""#);
    assert_eq!(
        tokens,
        vec![
            Token::QuoteSublexBegin(QuoteKind::Double, b'"'),
            Token::ConstSegment("items: ".into()),
            Token::InterpArray("list".into()),
            Token::ConstSegment(".".into()),
            Token::SublexEnd,
        ]
    );
}

#[test]
fn lex_interp_escaped_sigil() {
    let tokens = lex_all(r#""price: \$100""#);
    assert_eq!(tokens, vec![Token::QuoteSublexBegin(QuoteKind::Double, b'"'), Token::ConstSegment("price: $100".into()), Token::SublexEnd,]);
}

#[test]
fn lex_interp_no_interpolation() {
    // A double-quoted string with no variables is still sub-tokens.
    let tokens = lex_all(r#""plain text""#);
    assert_eq!(tokens, vec![Token::QuoteSublexBegin(QuoteKind::Double, b'"'), Token::ConstSegment("plain text".into()), Token::SublexEnd,]);
}

#[test]
fn lex_interp_multiple_vars() {
    let tokens = lex_all(r#""$x + $y""#);
    assert_eq!(
        tokens,
        vec![
            Token::QuoteSublexBegin(QuoteKind::Double, b'"'),
            Token::InterpScalar("x".into()),
            Token::ConstSegment(" + ".into()),
            Token::InterpScalar("y".into()),
            Token::SublexEnd,
        ]
    );
}

#[test]
fn lex_qq_interp() {
    let tokens = lex_all(r#"qq{Hello, $name!}"#);
    assert_eq!(
        tokens,
        vec![
            Token::QuoteSublexBegin(QuoteKind::Double, b'{'),
            Token::ConstSegment("Hello, ".into()),
            Token::InterpScalar("name".into()),
            Token::ConstSegment("!".into()),
            Token::SublexEnd,
        ]
    );
}

#[test]
fn lex_empty_string() {
    let tokens = lex_all(r#""""#);
    assert_eq!(tokens, vec![Token::QuoteSublexBegin(QuoteKind::Double, b'"'), Token::SublexEnd,]);
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
    assert_eq!(
        tokens,
        vec![Token::RegexSublexBegin(RegexKind::Match, b'{'), Token::ConstSegment("foo".into()), Token::SublexEnd, Token::Ident("i".into()),]
    );
}

#[test]
fn lex_m_regex_slash() {
    let tokens = lex_all("m/bar/gx");
    assert_eq!(
        tokens,
        vec![Token::RegexSublexBegin(RegexKind::Match, b'/'), Token::ConstSegment("bar".into()), Token::SublexEnd, Token::Ident("gx".into()),]
    );
}

#[test]
fn lex_qr_regex() {
    let tokens = lex_all("qr/\\d+/");
    assert_eq!(tokens, vec![Token::RegexSublexBegin(RegexKind::Qr, b'/'), Token::ConstSegment("\\d+".into()), Token::SublexEnd,]);
}

#[test]
fn lex_substitution() {
    // lex_all only tests the pattern side; the full pipeline
    // (replacement + flags) is tested by parser tests.
    let tokens = lex_all("s/foo/bar/g");
    assert_eq!(tokens[0], Token::SubstSublexBegin(b'/'));
    assert_eq!(tokens[1], Token::ConstSegment("foo".into()));
    assert_eq!(tokens[2], Token::SublexEnd);
}

#[test]
fn lex_substitution_braces() {
    let tokens = lex_all("s{foo}{bar}g");
    assert_eq!(tokens[0], Token::SubstSublexBegin(b'{'));
    assert_eq!(tokens[1], Token::ConstSegment("foo".into()));
    assert_eq!(tokens[2], Token::SublexEnd);
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
    assert_eq!(tokens, vec![Token::QuoteSublexBegin(QuoteKind::Heredoc, 0), Token::ConstSegment("Hello, world!\n".into()), Token::SublexEnd, Token::Semi,]);
}

#[test]
fn lex_heredoc_double_quoted() {
    let src = "<<\"END\";\nHello!\nEND\n";
    let tokens = lex_all(src);
    assert_eq!(tokens, vec![Token::QuoteSublexBegin(QuoteKind::Heredoc, 0), Token::ConstSegment("Hello!\n".into()), Token::SublexEnd, Token::Semi,]);
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
        vec![Token::QuoteSublexBegin(QuoteKind::Heredoc, 0), Token::ConstSegment("line 1\nline 2\nline 3\n".into()), Token::SublexEnd, Token::Semi,]
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
            Token::QuoteSublexBegin(QuoteKind::Heredoc, 0),
            Token::ConstSegment("body\n".into()),
            Token::SublexEnd,
            Token::Dot,
            Token::QuoteSublexBegin(QuoteKind::Double, b'"'),
            Token::ConstSegment(" suffix".into()),
            Token::SublexEnd,
            Token::Semi,
        ]
    );
}

#[test]
fn lex_heredoc_indented() {
    let src = "<<~END;\n    hello\n    world\n    END\n";
    let tokens = lex_all(src);
    assert_eq!(tokens[0], Token::QuoteSublexBegin(QuoteKind::Heredoc, 0));
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
    assert_eq!(tokens[0], Token::QuoteSublexBegin(QuoteKind::Heredoc, 0));
    // Collect heredoc body content.
    let mut i = 1;
    let mut body = String::new();
    while i < tokens.len() && tokens[i] != Token::SublexEnd {
        if let Token::ConstSegment(s) = &tokens[i] {
            body.push_str(s);
        }
        i += 1;
    }
    assert_eq!(body, "body\n");
    assert_eq!(tokens[i], Token::SublexEnd);
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
    assert_eq!(tokens, vec![Token::QuoteSublexBegin(QuoteKind::Double, b'"'), Token::ConstSegment("\t".into()), Token::SublexEnd]);
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
        vec![Token::QuoteSublexBegin(QuoteKind::Double, b'|'), Token::ConstSegment("hello ".into()), Token::InterpScalar("name".into()), Token::SublexEnd,]
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
    assert_eq!(tokens, vec![Token::QuoteSublexBegin(QuoteKind::Backtick, b'`'), Token::ConstSegment("ls -la".into()), Token::SublexEnd]);
}

#[test]
fn lex_heredoc_empty_body() {
    let src = "<<END;\nEND\n";
    let tokens = lex_all(src);
    assert_eq!(tokens, vec![Token::QuoteSublexBegin(QuoteKind::Heredoc, 0), Token::SublexEnd, Token::Semi,]);
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
    assert_eq!(tokens, vec![Token::QuoteSublexBegin(QuoteKind::Double, b'"'), Token::ConstSegment("line 1\nline 2\nline 3".into()), Token::SublexEnd,]);
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
            Token::QuoteSublexBegin(QuoteKind::Double, b'"'),
            Token::ConstSegment("line 1\nline 2\n".into()),
            Token::InterpScalar("x".into()),
            Token::ConstSegment("\nline 4".into()),
            Token::SublexEnd,
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
    assert_eq!(
        tokens,
        vec![Token::QuoteSublexBegin(QuoteKind::Heredoc, 0), Token::ConstSegment("line 1\nline 2\n".into()), Token::SublexEnd, Token::Semi,]
    );
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
            Token::RegexSublexBegin(RegexKind::Match, b'/'),
            Token::ConstSegment("foo".into()),
            Token::RegexCodeStart,
            Token::ScalarVar("x".into()),
            Token::RightBrace,
            Token::ConstSegment(")bar".into()),
            Token::SublexEnd,
        ]
    );
}

#[test]
fn lex_regex_cond_code_block() {
    let tokens = lex_all("m/foo(??{ $x })bar/");
    assert_eq!(
        tokens,
        vec![
            Token::RegexSublexBegin(RegexKind::Match, b'/'),
            Token::ConstSegment("foo".into()),
            Token::RegexCondCodeStart,
            Token::ScalarVar("x".into()),
            Token::RightBrace,
            Token::ConstSegment(")bar".into()),
            Token::SublexEnd,
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
            Token::RegexSublexBegin(RegexKind::Match, b'{'),
            Token::ConstSegment("foo".into()),
            Token::RegexCodeStart,
            Token::IntLit(1),
            Token::RightBrace,
            Token::ConstSegment(")bar".into()),
            Token::SublexEnd,
        ]
    );
}

#[test]
fn lex_code_block_not_in_string() {
    // (?{ in a double-quoted string is literal, not a code block.
    let tokens = lex_all(r#""foo(?{bar})""#);
    assert_eq!(tokens, vec![Token::QuoteSublexBegin(QuoteKind::Double, b'"'), Token::ConstSegment("foo(?{bar})".into()), Token::SublexEnd,]);
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
    assert_eq!(tokens, vec![Token::QuoteSublexBegin(QuoteKind::Double, b'"'), Token::ConstSegment("foo(??{bar})".into()), Token::SublexEnd,]);
}

// ── Regex interpolation ───────────────────────────────────

#[test]
fn lex_regex_scalar_interp() {
    // $var in a regex pattern triggers interpolation.
    let tokens = lex_all("m/foo$bar/");
    assert_eq!(
        tokens,
        vec![Token::RegexSublexBegin(RegexKind::Match, b'/'), Token::ConstSegment("foo".into()), Token::InterpScalar("bar".into()), Token::SublexEnd,]
    );
}

#[test]
fn lex_regex_array_interp() {
    // @arr in a regex pattern triggers interpolation.
    let tokens = lex_all("m/foo@arr/");
    assert_eq!(
        tokens,
        vec![Token::RegexSublexBegin(RegexKind::Match, b'/'), Token::ConstSegment("foo".into()), Token::InterpArray("arr".into()), Token::SublexEnd,]
    );
}

#[test]
fn lex_regex_interp_and_code_block() {
    // Both $var and (?{...}) in the same pattern.
    let tokens = lex_all("m/foo$bar(?{ $x })baz/");
    assert_eq!(
        tokens,
        vec![
            Token::RegexSublexBegin(RegexKind::Match, b'/'),
            Token::ConstSegment("foo".into()),
            Token::InterpScalar("bar".into()),
            Token::RegexCodeStart,
            Token::ScalarVar("x".into()),
            Token::RightBrace,
            Token::ConstSegment(")baz".into()),
            Token::SublexEnd,
        ]
    );
}

#[test]
fn lex_regex_literal_no_interp() {
    // m'...' with single-quote delimiter: no $var interpolation.
    let tokens = lex_all("m'foo$bar'");
    assert_eq!(tokens, vec![Token::RegexSublexBegin(RegexKind::Match, b'\''), Token::ConstSegment("foo$bar".into()), Token::SublexEnd,]);
}

#[test]
fn lex_regex_literal_with_code_block() {
    // m'...' still recognizes (?{...}) code blocks.
    let tokens = lex_all("m'foo(?{ $x })bar'");
    assert_eq!(
        tokens,
        vec![
            Token::RegexSublexBegin(RegexKind::Match, b'\''),
            Token::ConstSegment("foo".into()),
            Token::RegexCodeStart,
            Token::ScalarVar("x".into()),
            Token::RightBrace,
            Token::ConstSegment(")bar".into()),
            Token::SublexEnd,
        ]
    );
}

#[test]
fn lex_subst_pattern_interp() {
    // $var in s/// pattern triggers interpolation.
    let tokens = lex_all("s/foo$bar/baz/");
    assert_eq!(tokens[0], Token::SubstSublexBegin(b'/'));
    assert_eq!(tokens[1], Token::ConstSegment("foo".into()));
    assert_eq!(tokens[2], Token::InterpScalar("bar".into()));
    // ConstSegment("") before SublexEnd for the empty segment
    // after the interpolation.
    assert!(tokens.contains(&Token::SublexEnd));
}

#[test]
fn lex_substitution_global() {
    let tokens = lex_all("s/old/new/g");
    assert_eq!(tokens[0], Token::SubstSublexBegin(b'/'));
    assert_eq!(tokens[1], Token::ConstSegment("old".into()));
    assert_eq!(tokens[2], Token::SublexEnd);
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
    assert_eq!(tokens, vec![Token::Readline("STDIN".into(), false)]);
}

#[test]
fn lex_readline_diamond() {
    let tokens = lex_all("<>");
    assert_eq!(tokens, vec![Token::Readline("".into(), false)]);
}

#[test]
fn lex_glob_wildcard() {
    let tokens = lex_all("<*.txt>");
    assert_eq!(tokens, vec![Token::Readline("*.txt".into(), false)]);
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
fn lex_end_not_at_column_0_is_bareword() {
    // Indented __END__ is a bareword, not end-of-source.
    let tokens = lex_all("1;\n  __END__\nstuff\n");
    assert!(!tokens.contains(&Token::DataEnd(DataEndMarker::End)));
    assert!(tokens.contains(&Token::Ident("__END__".into())));
    // Code after the pseudo-__END__ is still lexed as code.
    assert!(tokens.contains(&Token::Ident("stuff".into())));
}

#[test]
fn lex_end_after_other_token_is_bareword() {
    // __END__ after another token on the same line is not special.
    let tokens = lex_all("my $x = __END__;\n");
    assert!(!tokens.contains(&Token::DataEnd(DataEndMarker::End)));
    assert!(tokens.contains(&Token::Ident("__END__".into())));
}

#[test]
fn lex_data_not_at_column_0_is_bareword() {
    let tokens = lex_all("foo __DATA__\nbar\n");
    assert!(!tokens.contains(&Token::DataEnd(DataEndMarker::Data)));
    assert!(tokens.contains(&Token::Ident("__DATA__".into())));
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
// Quote-operator delimiter handling.
//
// Per Perl, any non-word ASCII byte — INCLUDING the closers
// `)`, `]`, `}`, `>` — is a valid unpaired quote-operator
// delimiter.  `q}foo}`, `m>foo>`, `s]a]b]` etc. are real
// quote operators and should lex as such.
//
// The context-sensitive exceptions are:
//   1. `q => 1` — fat-comma lookahead suppresses the quote
//      op so the parser can autoquote `q` as a bareword.
//   2. `$h{q}` — parser-driven autoquoting via
//      `try_autoquoted_bareword_subscript` takes precedence
//      before the lexer gets to interpret `q`.
//
// Tests for (1) live here (token-level).  Tests for (2)
// live in parser.rs (they require parser context to set up).
// ═══════════════════════════════════════════════════════════

// ── Closers ARE valid unpaired delimiters ────────────────

#[test]
fn quote_op_q_with_rbrace_delim() {
    // `q}foo}` is a q-string with body "foo".
    let tokens = lex_all("q}foo};");
    assert!(matches!(tokens[0], Token::StrLit(ref s) if s == "foo"), "expected StrLit(foo), got {:?}", tokens[0]);
}

#[test]
fn quote_op_q_with_rparen_delim() {
    let tokens = lex_all("q)foo);");
    assert!(matches!(tokens[0], Token::StrLit(ref s) if s == "foo"), "expected StrLit(foo), got {:?}", tokens[0]);
}

#[test]
fn quote_op_q_with_rbracket_delim() {
    let tokens = lex_all("q]foo];");
    assert!(matches!(tokens[0], Token::StrLit(ref s) if s == "foo"), "expected StrLit(foo), got {:?}", tokens[0]);
}

#[test]
fn quote_op_q_with_gt_delim() {
    // `q>foo>` — `>` as unpaired delimiter.
    let tokens = lex_all("q>foo>;");
    assert!(matches!(tokens[0], Token::StrLit(ref s) if s == "foo"), "expected StrLit(foo), got {:?}", tokens[0]);
}

#[test]
fn quote_op_q_with_equals_delim() {
    // `q=foo=` — `=` as delimiter.  No fat comma to trigger
    // autoquoting.
    let tokens = lex_all("q=foo=;");
    assert!(matches!(tokens[0], Token::StrLit(ref s) if s == "foo"), "expected StrLit(foo), got {:?}", tokens[0]);
}

#[test]
fn quote_op_y_with_rbrace_delim() {
    // `y}abc}xyz}` — transliteration with `}` as delimiter.
    // Shouldn't error out as unterminated.
    let tokens = lex_all("y}abc}xyz};");
    // tr/y produces a Subst-like token tree; exact shape
    // depends on the implementation.  The key thing is no
    // error, and the first token is NOT a lone Ident("y").
    assert!(!matches!(tokens[0], Token::Ident(ref s) if s == "y"), "y should be a quote op here, got Ident(y): {tokens:?}");
}

// ── Fat-comma lookahead suppresses quote-op recognition ──

/// For each keyword, `KEYWORD => 1` must NOT start a quote
/// op — the lexer should emit the keyword as an ordinary
/// identifier (or Keyword for `qw`) so the parser's
/// fat-comma autoquote fires.
fn assert_kw_before_fat_comma_is_bareword(src: &str, expected_name: &str) {
    let tokens = lex_all(src);
    assert!(tokens.len() >= 3, "expected at least 3 tokens for {src:?}, got {tokens:?}");
    let is_bareword = matches!(&tokens[0], Token::Ident(s) if s == expected_name)
        || matches!(&tokens[0], Token::Keyword(kw) if {
            let n: &str = (*kw).into();
            n == expected_name
        });
    assert!(is_bareword, "expected bareword `{expected_name}` for {src:?}, got {:?}", tokens[0]);
    assert!(matches!(tokens[1], Token::FatComma), "expected FatComma second for {src:?}, got {:?}", tokens[1]);
}

#[test]
fn fat_comma_lookahead_q() {
    assert_kw_before_fat_comma_is_bareword("q => 1;", "q");
}
#[test]
fn fat_comma_lookahead_qq() {
    assert_kw_before_fat_comma_is_bareword("qq => 1;", "qq");
}
#[test]
fn fat_comma_lookahead_qw() {
    assert_kw_before_fat_comma_is_bareword("qw => 1;", "qw");
}
#[test]
fn fat_comma_lookahead_qr() {
    assert_kw_before_fat_comma_is_bareword("qr => 1;", "qr");
}
#[test]
fn fat_comma_lookahead_m() {
    assert_kw_before_fat_comma_is_bareword("m => 1;", "m");
}
#[test]
fn fat_comma_lookahead_s() {
    assert_kw_before_fat_comma_is_bareword("s => 1;", "s");
}
#[test]
fn fat_comma_lookahead_tr() {
    assert_kw_before_fat_comma_is_bareword("tr => 1;", "tr");
}
#[test]
fn fat_comma_lookahead_y() {
    assert_kw_before_fat_comma_is_bareword("y => 1;", "y");
}

/// No-whitespace form: `q=>1` — Perl's `=>` is recognized
/// as a single token before `q=...=` interpretation, so
/// this also autoquotes.
#[test]
fn fat_comma_lookahead_no_ws_q() {
    let tokens = lex_all("q=>1;");
    assert!(matches!(tokens[0], Token::Ident(ref s) if s == "q"), "expected Ident(q), got {:?}", tokens[0]);
    assert!(matches!(tokens[1], Token::FatComma));
}

#[test]
fn fat_comma_lookahead_no_ws_y() {
    let tokens = lex_all("y=>1;");
    assert!(matches!(tokens[0], Token::Ident(ref s) if s == "y"), "expected Ident(y), got {:?}", tokens[0]);
    assert!(matches!(tokens[1], Token::FatComma));
}

/// Negative: `q=foo=` (bare `=` without `>`) must still be
/// a q-string.  Fat-comma lookahead must not false-positive
/// on bare `=`.
#[test]
fn fat_comma_lookahead_bare_equals_is_still_quote_op() {
    let tokens = lex_all("q=foo=;");
    assert!(matches!(tokens[0], Token::StrLit(ref s) if s == "foo"), "bare `=` must still be a delimiter, got {:?}", tokens[0]);
}

// ── Multi-line fat-comma lookahead ───────────────────────
//
// The lookahead spans lines: `q\n=>\n1` must autoquote
// even though the keyword and `=>` are on different lines.
// Perl behaves this way because whitespace between a quote
// keyword and its delimiter can span lines anyway — if
// we're about to consume that whitespace to find a delim,
// we can check for `=>` at the same time.

#[test]
fn fat_comma_lookahead_q_across_newline() {
    let tokens = lex_all("q\n  => 1;");
    assert!(matches!(tokens[0], Token::Ident(ref s) if s == "q"), "expected Ident(q) across newline, got {:?}", tokens[0]);
    assert!(matches!(tokens[1], Token::FatComma));
}

#[test]
fn fat_comma_lookahead_y_across_newline() {
    let tokens = lex_all("y\n=>\n1;");
    assert!(matches!(tokens[0], Token::Ident(ref s) if s == "y"), "expected Ident(y) across newlines, got {:?}", tokens[0]);
    assert!(matches!(tokens[1], Token::FatComma));
}

#[test]
fn fat_comma_lookahead_skips_comment_to_find_arrow() {
    // Comment between the keyword and `=>` counts as
    // whitespace-like for this lookahead, matching what the
    // quote-op delim scan would do anyway.
    let tokens = lex_all("m # comment\n => 1;");
    assert!(matches!(tokens[0], Token::Ident(ref s) if s == "m"), "expected Ident(m) past comment, got {:?}", tokens[0]);
    assert!(matches!(tokens[1], Token::FatComma));
}

// ── Alphanumeric delimiter after whitespace ──────────────
//
// When the keyword is followed by whitespace, an
// alphanumeric byte IS a valid quote-op delimiter — Perl
// parses `q xabcx` as a q-string with `x` as delim, body
// "abc".  This contrasts with the no-whitespace case where
// scan_ident would have consumed the alphanumeric as part
// of the identifier itself.
//
// The counter-test guards against a regression where the
// post-ws lookahead for `=>` accidentally disqualifies all
// alphanumeric delimiters.

#[test]
fn quote_op_alnum_delim_after_newline_is_quote_string() {
    // `q\nxabcx` — newline is whitespace; `x` after the
    // newline is a valid delimiter.
    let tokens = lex_all("q\nxabcx;");
    assert!(matches!(tokens[0], Token::StrLit(ref s) if s == "abc"), "expected StrLit(abc) from q\\nxabcx, got {:?}", tokens[0]);
}

#[test]
fn quote_op_alnum_delim_after_space_is_quote_string() {
    // `q xabcx` — single space; same principle.
    let tokens = lex_all("q xabcx;");
    assert!(matches!(tokens[0], Token::StrLit(ref s) if s == "abc"), "expected StrLit(abc) from `q xabcx`, got {:?}", tokens[0]);
}

#[test]
fn quote_op_q_across_newline_then_fat_comma_autoquotes() {
    // Paired with the test above: `q\n=>` must still
    // autoquote, so the lookahead isn't defeated by the
    // relaxed post-ws delimiter rule.
    let tokens = lex_all("q\n=>1;");
    assert!(matches!(tokens[0], Token::Ident(ref s) if s == "q"), "expected Ident(q) from q\\n=>, got {:?}", tokens[0]);
    assert!(matches!(tokens[1], Token::FatComma));
}

#[test]
fn quote_op_m_alnum_delim_after_ws() {
    // `m xabcx` — match operator with `x` as delim.
    // The exact token shape for `m` is implementation-
    // specific (m-sublex vs. a single Regex token); the
    // critical assertion is that it's NOT a lone Ident("m").
    let tokens = lex_all("m xabcx;");
    assert!(!matches!(tokens[0], Token::Ident(ref s) if s == "m"), "m should be a match op here, got Ident(m): {tokens:?}");
}

// ── `=` delimiter × whitespace, across every quote kw ────
//
// The Phase A/B logic applies uniformly to all nine quote-
// like operators — q, qq, qw, qr, m, s, tr, y, qx.  The
// critical disambiguation (`=` is a delimiter, `=>` is a
// fat comma) must behave the same for every keyword.  The
// shape of the resulting token differs (q/qq → StrLit-ish,
// m/qr → regex, s/tr/y → subst/trans, qw → word list), but
// the invariant we verify here is weaker and uniform:
// the first token is NOT a bare `Ident(keyword)` (and not
// `Keyword(Qw)` for `qw`), which would indicate autoquote.
//
// `s`, `tr`, `y` need three delimiters, so we use
// `{kw}{ws}=a=b=` for them and `{kw}{ws}=test=` for the rest.
//
// POD interaction: `=word` at column 0 starts a POD block
// (this is Perl, not a bug in our skip_ws_and_comments).
// The cross-newline variant therefore indents the
// continuation line by a space, putting `=` at column 1
// where POD extraction no longer fires.  The in-line (space)
// variant is naturally safe since the `=` is never at col 0.

fn src_equals_delim(kw: &str, ws: &str) -> String {
    match kw {
        "s" | "tr" | "y" => format!("{kw}{ws}=a=b=;"),
        _ => format!("{kw}{ws}=test=;"),
    }
}

/// First token is NOT a plain Ident/Keyword form of `name`
/// — i.e. the keyword was NOT autoquoted and DID start a
/// quote op.  (The specific token kind depends on the
/// keyword; we don't pin it down here.)
fn assert_not_autoquoted(tokens: &[Token], name: &str, src: &str) {
    let autoquoted_ident = matches!(&tokens[0], Token::Ident(s) if s == name);
    let autoquoted_qw = name == "qw" && matches!(&tokens[0], Token::Keyword(Keyword::Qw));
    assert!(!autoquoted_ident && !autoquoted_qw, "{name} should be a quote op for {src:?}, got autoquoted first token: {:?}", tokens[0]);
}

// `=` delim across newline — nine tests, one per keyword.

#[test]
fn quote_op_equals_delim_across_newline_q() {
    let src = src_equals_delim("q", "\n");
    let tokens = lex_all(&src);
    assert!(matches!(tokens[0], Token::StrLit(ref s) if s == "test"), "expected StrLit(test), got {:?} for {src:?}", tokens[0]);
}
#[test]
fn quote_op_equals_delim_across_newline_qq() {
    let src = src_equals_delim("qq", "\n");
    assert_not_autoquoted(&lex_all(&src), "qq", &src);
}
#[test]
fn quote_op_equals_delim_across_newline_qw() {
    let src = src_equals_delim("qw", "\n");
    assert_not_autoquoted(&lex_all(&src), "qw", &src);
}
#[test]
fn quote_op_equals_delim_across_newline_qr() {
    let src = src_equals_delim("qr", "\n");
    assert_not_autoquoted(&lex_all(&src), "qr", &src);
}
#[test]
fn quote_op_equals_delim_across_newline_m() {
    let src = src_equals_delim("m", "\n");
    assert_not_autoquoted(&lex_all(&src), "m", &src);
}
#[test]
fn quote_op_equals_delim_across_newline_s() {
    let src = src_equals_delim("s", "\n");
    assert_not_autoquoted(&lex_all(&src), "s", &src);
}
#[test]
fn quote_op_equals_delim_across_newline_tr() {
    let src = src_equals_delim("tr", "\n");
    assert_not_autoquoted(&lex_all(&src), "tr", &src);
}
#[test]
fn quote_op_equals_delim_across_newline_y() {
    let src = src_equals_delim("y", "\n");
    assert_not_autoquoted(&lex_all(&src), "y", &src);
}
#[test]
fn quote_op_equals_delim_across_newline_qx() {
    let src = src_equals_delim("qx", "\n");
    assert_not_autoquoted(&lex_all(&src), "qx", &src);
}

// `=` delim after space — nine tests, one per keyword.

#[test]
fn quote_op_equals_delim_after_space_q() {
    let src = src_equals_delim("q", " ");
    let tokens = lex_all(&src);
    assert!(matches!(tokens[0], Token::StrLit(ref s) if s == "test"), "expected StrLit(test), got {:?} for {src:?}", tokens[0]);
}
#[test]
fn quote_op_equals_delim_after_space_qq() {
    let src = src_equals_delim("qq", " ");
    assert_not_autoquoted(&lex_all(&src), "qq", &src);
}
#[test]
fn quote_op_equals_delim_after_space_qw() {
    let src = src_equals_delim("qw", " ");
    assert_not_autoquoted(&lex_all(&src), "qw", &src);
}
#[test]
fn quote_op_equals_delim_after_space_qr() {
    let src = src_equals_delim("qr", " ");
    assert_not_autoquoted(&lex_all(&src), "qr", &src);
}
#[test]
fn quote_op_equals_delim_after_space_m() {
    let src = src_equals_delim("m", " ");
    assert_not_autoquoted(&lex_all(&src), "m", &src);
}
#[test]
fn quote_op_equals_delim_after_space_s() {
    let src = src_equals_delim("s", " ");
    assert_not_autoquoted(&lex_all(&src), "s", &src);
}
#[test]
fn quote_op_equals_delim_after_space_tr() {
    let src = src_equals_delim("tr", " ");
    assert_not_autoquoted(&lex_all(&src), "tr", &src);
}
#[test]
fn quote_op_equals_delim_after_space_y() {
    let src = src_equals_delim("y", " ");
    assert_not_autoquoted(&lex_all(&src), "y", &src);
}
#[test]
fn quote_op_equals_delim_after_space_qx() {
    let src = src_equals_delim("qx", " ");
    assert_not_autoquoted(&lex_all(&src), "qx", &src);
}

// Alphanumeric delim across newline — nine tests, one per keyword.

fn src_alnum_delim(kw: &str) -> String {
    match kw {
        "s" | "tr" | "y" => format!("{kw}\nxaxbx;"),
        _ => format!("{kw}\nxabcx;"),
    }
}

#[test]
fn quote_op_alnum_delim_across_newline_q() {
    let src = src_alnum_delim("q");
    let tokens = lex_all(&src);
    assert!(matches!(tokens[0], Token::StrLit(ref s) if s == "abc"), "expected StrLit(abc), got {:?} for {src:?}", tokens[0]);
}
#[test]
fn quote_op_alnum_delim_across_newline_qq() {
    let src = src_alnum_delim("qq");
    assert_not_autoquoted(&lex_all(&src), "qq", &src);
}
#[test]
fn quote_op_alnum_delim_across_newline_qw() {
    let src = src_alnum_delim("qw");
    assert_not_autoquoted(&lex_all(&src), "qw", &src);
}
#[test]
fn quote_op_alnum_delim_across_newline_qr() {
    let src = src_alnum_delim("qr");
    assert_not_autoquoted(&lex_all(&src), "qr", &src);
}
#[test]
fn quote_op_alnum_delim_across_newline_m() {
    let src = src_alnum_delim("m");
    assert_not_autoquoted(&lex_all(&src), "m", &src);
}
#[test]
fn quote_op_alnum_delim_across_newline_s() {
    let src = src_alnum_delim("s");
    assert_not_autoquoted(&lex_all(&src), "s", &src);
}
#[test]
fn quote_op_alnum_delim_across_newline_tr() {
    let src = src_alnum_delim("tr");
    assert_not_autoquoted(&lex_all(&src), "tr", &src);
}
#[test]
fn quote_op_alnum_delim_across_newline_y() {
    let src = src_alnum_delim("y");
    assert_not_autoquoted(&lex_all(&src), "y", &src);
}
#[test]
fn quote_op_alnum_delim_across_newline_qx() {
    let src = src_alnum_delim("qx");
    assert_not_autoquoted(&lex_all(&src), "qx", &src);
}

// Fat-comma after newline — paired counter-tests to the
// equals-delim-across-newline set above.  Same whitespace
// lead-in, `>` appended, opposite outcome.

fn assert_autoquoted_via_fat_comma(src: &str, name: &str) {
    let tokens = lex_all(src);
    let is_bareword = matches!(&tokens[0], Token::Ident(s) if s == name)
        || matches!(&tokens[0], Token::Keyword(kw) if {
            let n: &str = (*kw).into();
            n == name
        });
    assert!(is_bareword, "expected bareword `{name}` for {src:?}, got {:?}", tokens[0]);
    assert!(matches!(tokens[1], Token::FatComma), "expected FatComma second for {src:?}, got {:?}", tokens[1]);
}

#[test]
fn fat_comma_across_newline_q() {
    assert_autoquoted_via_fat_comma("q\n=>1;", "q");
}
#[test]
fn fat_comma_across_newline_qq() {
    assert_autoquoted_via_fat_comma("qq\n=>1;", "qq");
}
#[test]
fn fat_comma_across_newline_qw() {
    assert_autoquoted_via_fat_comma("qw\n=>1;", "qw");
}
#[test]
fn fat_comma_across_newline_qr() {
    assert_autoquoted_via_fat_comma("qr\n=>1;", "qr");
}
#[test]
fn fat_comma_across_newline_m() {
    assert_autoquoted_via_fat_comma("m\n=>1;", "m");
}
#[test]
fn fat_comma_across_newline_s() {
    assert_autoquoted_via_fat_comma("s\n=>1;", "s");
}
#[test]
fn fat_comma_across_newline_tr() {
    assert_autoquoted_via_fat_comma("tr\n=>1;", "tr");
}
#[test]
fn fat_comma_across_newline_y() {
    assert_autoquoted_via_fat_comma("y\n=>1;", "y");
}
#[test]
fn fat_comma_across_newline_qx() {
    assert_autoquoted_via_fat_comma("qx\n=>1;", "qx");
}

// ── POD-interaction corner cases ─────────────────────────
//
// Perl's POD extraction does NOT fire inside a quote-op's
// delimiter-finding scan.  `qq\n=pod\n\ntesting\n\n=` is a
// qq-string with body `"pod\n\ntesting\n\n"`, not a pod
// block followed by broken code.  This matches real Perl:
//
// ```perl
// $_ = qq
//
// =pod
//
// testing
//
// =;
// print "$_\n";  # → "pod\n\ntesting\n\n"
// ```
//
// The lexer achieves this by using `skip_ws_and_comments_no_pod`
// for the delim-search whitespace skip.  Outside that context,
// `=word` at col 0 still starts a pod block as usual.

#[test]
fn pod_not_triggered_in_delim_scan_qq() {
    // The exact scenario from the Perl debugger output: qq
    // followed by blank lines, then `=pod` at col 0, blank
    // lines, `testing`, blank lines, and the closing `=`.
    // Body is everything between the two `=` delimiters.
    let src = "qq\n\n=pod\n\ntesting\n\n=;";
    let tokens = lex_all(src);
    // Body starts with "pod" (the `=` is the delim, not
    // part of the body).  qq interpolates but the body here
    // has no variables, so we expect a plain StrLit or an
    // InterpolatedString whose constant part is "pod\n\ntesting\n\n".
    // Assert the first token isn't an Ident("qq") autoquote.
    assert!(!matches!(tokens[0], Token::Ident(ref s) if s == "qq"), "qq should be a quote op, not autoquoted; got {:?}", tokens[0]);
}

#[test]
fn pod_not_triggered_in_delim_scan_q_equals_letter() {
    // Simpler form: `q\n=test=` — `=` on line 2 followed by
    // letter `t`.  Would be POD in normal code, but here
    // we're scanning for the q-delim, so POD is suspended
    // and `=` is the delimiter.
    let tokens = lex_all("q\n=test=;");
    assert!(matches!(tokens[0], Token::StrLit(ref s) if s == "test"), "expected StrLit(test), got {:?}", tokens[0]);
}

#[test]
fn pod_still_fires_outside_delim_scan() {
    // Regression guard: POD recognition must still work in
    // normal top-level code.  `1;\n=pod\n...\n=cut\n2;` —
    // the `=pod` block is skipped, leaving IntLit(1), Semi,
    // IntLit(2), Semi.
    let tokens = lex_all("1;\n=pod\n\ntext\n\n=cut\n2;");
    assert!(matches!(tokens[0], Token::IntLit(1)));
    assert!(matches!(tokens[1], Token::Semi));
    assert!(matches!(tokens[2], Token::IntLit(2)));
    assert!(matches!(tokens[3], Token::Semi));
}

#[test]
fn comment_skipped_in_delim_scan() {
    // Counterpoint to the POD tests: `#` comments ARE
    // skipped during the delim scan — unlike POD.  From a
    // real Perl trace:
    //
    // ```perl
    // $_ = qq
    //
    // # testing
    //
    // =swd;fkjasfd;klj\n=;
    // print;               # prints "swd;fkjasfd;klj\n"
    // ```
    //
    // qq keyword → skip whitespace + `# testing` comment →
    // find `=` delimiter → body "swd;fkjasfd;klj\n".  The
    // assertion here is just that `qq` isn't emitted as a
    // bare Ident, i.e. the delim scan successfully crossed
    // both the blank lines and the comment.
    let src = "qq\n\n# testing\n\n=swd;fkjasfd;klj\\n=;";
    let tokens = lex_all(src);
    assert!(!matches!(tokens[0], Token::Ident(ref s) if s == "qq"), "qq should be a quote op after comment skip; got {:?}", tokens[0]);
}

#[test]
fn lex_repeat_assign_adjacent() {
    // `x=` adjacent → single RepeatEq token.
    let tokens = lex_all("$s x= 3;");
    assert!(tokens.contains(&Token::Assign(AssignOp::RepeatEq)), "x= should produce RepeatEq; got {:?}", tokens);
}

#[test]
fn lex_repeat_assign_with_space() {
    // `x =` with space → Ident("x") then Assign(Eq), NOT RepeatEq.
    let tokens = lex_all("$s x = 3;");
    assert!(!tokens.contains(&Token::Assign(AssignOp::RepeatEq)), "x = (with space) should NOT produce RepeatEq; got {:?}", tokens);
    assert!(tokens.contains(&Token::Ident("x".into())), "x should be Ident; got {:?}", tokens);
}

#[test]
fn lex_x_fat_comma_not_repeat_assign() {
    // `x =>` should autoquote x, not produce RepeatEq.
    let tokens = lex_all("x => 1;");
    assert!(!tokens.contains(&Token::Assign(AssignOp::RepeatEq)), "x => should NOT produce RepeatEq; got {:?}", tokens);
}

#[test]
fn lex_hex_float() {
    // 0xA.8p1 = 10.5 * 2 = 21.0 (exactly representable)
    let tokens = lex_all("0xA.8p1;");
    match tokens[0] {
        Token::FloatLit(v) => assert_eq!(v, 21.0, "0xA.8p1 should be 21.0, got {v}"),
        ref other => panic!("expected FloatLit, got {other:?}"),
    }
}

#[test]
fn lex_hex_float_no_fraction() {
    // 0x1p10 = 1024.0
    let tokens = lex_all("0x1p10;");
    match tokens[0] {
        Token::FloatLit(v) => assert_eq!(v, 1024.0),
        ref other => panic!("expected FloatLit(1024.0), got {other:?}"),
    }
}

#[test]
fn lex_binary_float() {
    // 0b101.01p2 = 5.25 * 4 = 21.0
    let tokens = lex_all("0b101.01p2;");
    match tokens[0] {
        Token::FloatLit(v) => assert_eq!(v, 21.0),
        ref other => panic!("expected FloatLit(21.0), got {other:?}"),
    }
}

#[test]
fn lex_octal_float() {
    // 0o7.4p2 = 7.5 * 4 = 30.0
    let tokens = lex_all("0o7.4p2;");
    match tokens[0] {
        Token::FloatLit(v) => assert_eq!(v, 30.0),
        ref other => panic!("expected FloatLit(30.0), got {other:?}"),
    }
}

#[test]
fn lex_legacy_octal_float() {
    // 07.4p2 = 7.5 * 4 = 30.0 (legacy octal without 0o prefix)
    let tokens = lex_all("07.4p2;");
    match tokens[0] {
        Token::FloatLit(v) => assert_eq!(v, 30.0),
        ref other => panic!("expected FloatLit(30.0), got {other:?}"),
    }
}

#[test]
fn lex_vstring_no_v_prefix() {
    // 102.111.111 — v-string without v prefix (2+ dots).
    let tokens = lex_all("102.111.111;");
    match &tokens[0] {
        Token::VersionLit(s) => assert_eq!(s, "102.111.111"),
        other => panic!("expected VersionLit, got {other:?}"),
    }
}

#[test]
fn lex_float_one_dot_not_vstring() {
    // 102.111 — only one dot, should be a float, NOT a v-string.
    let tokens = lex_all("102.111;");
    assert!(matches!(tokens[0], Token::FloatLit(_)), "one-dot number should be float, got {:?}", tokens[0]);
}

#[test]
fn lex_dollar_space_name() {
    // `$ foo` ≡ `$foo` per perldata.
    let tokens = lex_all("$ foo;");
    assert!(matches!(&tokens[0], Token::ScalarVar(n) if n == "foo"), "$ foo should be ScalarVar(foo), got {:?}", tokens[0]);
}

#[test]
fn lex_at_space_name() {
    // `@ bar` ≡ `@bar`.
    let tokens = lex_all("@ bar;");
    assert!(matches!(&tokens[0], Token::ArrayVar(n) if n == "bar"), "@ bar should be ArrayVar(bar), got {:?}", tokens[0]);
}

#[test]
fn lex_dollar_space_no_ident() {
    // `$ {` — space followed by non-ident should still be bare Dollar.
    let tokens = lex_all("$ {foo};");
    assert!(matches!(tokens[0], Token::Dollar), "$ {{ should be Dollar, got {:?}", tokens[0]);
}

#[test]
fn lex_dollar_comment_then_name() {
    // `$ # comment\nx` ≡ `$x` — comment between sigil and name.
    let tokens = lex_all("$ # comment\nx;");
    assert!(matches!(&tokens[0], Token::ScalarVar(n) if n == "x"), "$ # comment\\nx should be ScalarVar(x), got {:?}", tokens[0]);
}

#[test]
fn lex_bare_dollar_caret() {
    // `$^` alone — format_top_name.
    let tokens = lex_all("$^;");
    assert!(matches!(&tokens[0], Token::SpecialVar(n) if n == "^"), "$^ should be SpecialVar(^), got {:?}", tokens[0]);
}

#[test]
fn lex_dollar_open_bracket() {
    // `$[` — array base (deprecated).
    let tokens = lex_all("$[;");
    assert!(matches!(&tokens[0], Token::SpecialVar(n) if n == "["), "$[ should be SpecialVar([), got {:?}", tokens[0]);
}

#[test]
fn lex_caret_underscore() {
    // `$^_` — reserved caret var with underscore.
    let tokens = lex_all("$^_;");
    assert!(matches!(&tokens[0], Token::SpecialVar(n) if n == "^_"), "$^_ should be SpecialVar(^_), got {:?}", tokens[0]);
}

#[test]
fn lex_percent_caret_h() {
    // `%^H` — hints hash, caret hash variable.
    let tokens = lex_all("%^H;");
    assert!(matches!(&tokens[0], Token::SpecialHashVar(n) if n == "^H"), "%^H should be SpecialHashVar(^H), got {:?}", tokens[0]);
}

#[test]
fn lex_regex_optimistic_code_block() {
    // (*{code}) — optimistic code block (5.37.7+).
    let tokens = lex_all("m/foo(*{ $x })bar/");
    assert_eq!(
        tokens,
        vec![
            Token::RegexSublexBegin(RegexKind::Match, b'/'),
            Token::ConstSegment("foo".into()),
            Token::RegexCodeStart,
            Token::ScalarVar("x".into()),
            Token::RightBrace,
            Token::ConstSegment(")bar".into()),
            Token::SublexEnd,
        ]
    );
}

// ── Quote-op edge cases ─────────────────────────────────

#[test]
fn quote_op_q_with_comment_before_delimiter() {
    // q # comment\nfoof — comment between q and delimiter.
    // Delimiter is 'f', body is "oo".
    let tokens = lex_all("q # comment\nfoof;");
    assert!(matches!(tokens[0], Token::StrLit(ref s) if s == "oo"), "expected StrLit(\"oo\"), got {:?}", tokens[0]);
}

#[test]
fn quote_op_q_with_backslash_delimiter() {
    let tokens = lex_all(r"q\foo\;");
    assert!(matches!(tokens[0], Token::StrLit(ref s) if s == "foo"), "expected StrLit(\"foo\"), got {:?}", tokens[0]);
}

#[test]
fn quote_op_qw_whitespace_is_not_escaped() {
    // Backslash does NOT escape whitespace in qw().
    let tokens = lex_all(r"qw[a\ b c]");
    assert!(matches!(
        &tokens[0],
        Token::QwList(words)
            if words == &vec![
                String::from("a\\"),
                String::from("b"),
                String::from("c"),
            ]
    ));
}

#[test]
fn quote_op_qw_delimiter_can_be_escaped_but_space_cannot() {
    let tokens = lex_all(r"qw[a\] b\ c]");
    assert!(matches!(
        &tokens[0],
        Token::QwList(words)
            if words == &vec![
                String::from("a]"),
                String::from("b\\"),
                String::from("c"),
            ]
    ));
}

#[test]
fn quote_op_qr_with_alnum_delimiter_after_space() {
    let tokens = lex_all("qr abcda;");
    assert!(!matches!(tokens[0], Token::Ident(ref s) if s == "qr"), "qr should be recognized as a quote op here: {:?}", tokens);
}

// ── Interpolation chain tokens ──────────────────────────

#[test]
fn lex_interpolated_scalar_chain() {
    let tokens = lex_all(r#""$h->{k}[0]""#);
    assert!(tokens.iter().any(|t| matches!(t, Token::InterpScalarChainStart(_))));
    assert!(tokens.iter().any(|t| matches!(t, Token::InterpChainEnd)));
}

#[test]
fn lex_interpolated_array_chain() {
    let tokens = lex_all(r#""@a[1..3]""#);
    assert!(tokens.iter().any(|t| matches!(t, Token::InterpArrayChainStart(_))));
    assert!(tokens.iter().any(|t| matches!(t, Token::InterpChainEnd)));
}

// ── Defined-or vs regex ─────────────────────────────────

#[test]
fn lex_defined_or_stays_defined_or() {
    let tokens = lex_all("$x // $y");
    assert!(tokens.iter().any(|t| matches!(t, Token::DefinedOr)));
}

#[test]
fn lex_empty_regex_flags_must_be_adjacent() {
    // `// i` — the space means `i` is a separate ident, not a flag.
    let tokens = lex_all("// i");
    assert!(tokens.iter().any(|t| matches!(t, Token::DefinedOr)));
    assert!(tokens.iter().any(|t| matches!(t, Token::Ident(s) if s == "i")));
}
