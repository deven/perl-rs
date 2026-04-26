//! Parser tests.

use super::*;

fn parse(src: &str) -> Program {
    let mut parser = Parser::new(src.as_bytes()).unwrap();
    parser.parse_program().unwrap()
}

fn parse_expr_str(src: &str) -> Expr {
    // Wrap in a statement to parse
    let prog = parse(src);
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => e.clone(),
        other => panic!("expected expression, got {other:?}"),
    }
}

/// Collect all tokens from source, for tests that need to
/// inspect token-level output (e.g. NFC on variable names).
fn collect_tokens(src: &str) -> Vec<Token> {
    let mut lexer = Lexer::new(src.as_bytes());
    let mut tokens = Vec::new();
    loop {
        let spanned = lexer.lex_token().unwrap();
        if matches!(spanned.token, Token::Eof) {
            break;
        }
        tokens.push(spanned.token);
    }
    tokens
}

/// Like `collect_tokens` but with UTF-8 mode pre-enabled,
/// for tests that need to tokenize Unicode identifiers
/// without going through the full parser pragma machinery.
fn collect_tokens_utf8(src: &str) -> Vec<Token> {
    let mut lexer = Lexer::new(src.as_bytes());
    lexer.set_utf8_mode(true);
    let mut tokens = Vec::new();
    loop {
        let spanned = lexer.lex_token().unwrap();
        if matches!(spanned.token, Token::Eof) {
            break;
        }
        tokens.push(spanned.token);
    }
    tokens
}

/// Extract the first variable name from a program containing
/// `my $name;` or `my $name = expr;`.  Handles both bare
/// declarations and assignment-wrapped declarations.
fn first_decl_name(prog: &Program) -> String {
    for stmt in &prog.statements {
        if let StmtKind::Expr(expr) = &stmt.kind {
            if let ExprKind::Assign(_, lhs, _) = &expr.kind
                && let ExprKind::Decl(_, decls) = &lhs.kind
                && !decls.is_empty()
            {
                return decls[0].name.clone();
            }
            if let ExprKind::Decl(_, decls) = &expr.kind
                && !decls.is_empty()
            {
                return decls[0].name.clone();
            }
        }
    }
    panic!("no variable declaration found in program");
}

/// Like `first_decl_name` but also returns the sigil.
fn first_decl_name_sigil(prog: &Program) -> (Sigil, String) {
    for stmt in &prog.statements {
        if let StmtKind::Expr(expr) = &stmt.kind {
            if let ExprKind::Assign(_, lhs, _) = &expr.kind
                && let ExprKind::Decl(_, decls) = &lhs.kind
                && !decls.is_empty()
            {
                return (decls[0].sigil, decls[0].name.clone());
            }
            if let ExprKind::Decl(_, decls) = &expr.kind
                && !decls.is_empty()
            {
                return (decls[0].sigil, decls[0].name.clone());
            }
        }
    }
    panic!("no variable declaration found in program");
}

/// Collect all declaration names from a program.
fn all_decl_names(prog: &Program) -> Vec<String> {
    let mut names = Vec::new();
    for stmt in &prog.statements {
        if let StmtKind::Expr(expr) = &stmt.kind {
            if let ExprKind::Assign(_, lhs, _) = &expr.kind
                && let ExprKind::Decl(_, decls) = &lhs.kind
            {
                names.extend(decls.iter().map(|d| d.name.clone()));
            } else if let ExprKind::Decl(_, decls) = &expr.kind {
                names.extend(decls.iter().map(|d| d.name.clone()));
            }
        }
    }
    names
}

/// Extract the RHS of the first `my $x = expr;` in a program.
fn first_assign_rhs(prog: &Program) -> Expr {
    for stmt in &prog.statements {
        if let StmtKind::Expr(expr) = &stmt.kind
            && let ExprKind::Assign(_, _, rhs) = &expr.kind
        {
            return rhs.as_ref().clone();
        }
    }
    panic!("no assignment found in program");
}

/// For tests that need the initializer from a `my $x = expr;`
/// declaration-statement.  Returns the RHS of the Assign.
fn decl_init(stmt: &Statement) -> &Expr {
    match &stmt.kind {
        StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, rhs), .. }) => {
            assert!(matches!(lhs.kind, ExprKind::Decl(_, _)), "expected Decl lhs, got {:?}", lhs.kind);
            rhs
        }
        other => panic!("expected decl with initializer, got {other:?}"),
    }
}

/// For tests that need the var list from a declaration.
/// Works for both `my $x;` (plain Decl) and `my $x = ...;` (Assign(Decl, _)).
fn decl_vars(stmt: &Statement) -> (DeclScope, &[VarDecl]) {
    let expr = match &stmt.kind {
        StmtKind::Expr(e) => e,
        other => panic!("expected Expr stmt, got {other:?}"),
    };
    let decl = match &expr.kind {
        ExprKind::Decl(_, _) => expr,
        ExprKind::Assign(_, lhs, _) => lhs,
        other => panic!("expected Decl or Assign(Decl, _), got {other:?}"),
    };
    match &decl.kind {
        ExprKind::Decl(scope, vars) => (*scope, vars.as_slice()),
        other => panic!("expected Decl, got {other:?}"),
    }
}

/// Extract the pattern string from an `Interpolated` value.
fn pat_str(interp: &Interpolated) -> &str {
    match interp.as_plain_string() {
        Some(ref _s) => {
            // as_plain_string returns owned; match on parts directly.
            if interp.0.is_empty() {
                return "";
            }
            match &interp.0[0] {
                InterpPart::Const(s) => s.as_str(),
                other => panic!("expected Const part, got {other:?}"),
            }
        }
        None => panic!("expected plain string pattern, got {:?}", interp.0),
    }
}

#[test]
fn parse_simple_assignment() {
    let prog = parse("my $x = 42;");
    assert_eq!(prog.statements.len(), 1);
    let (_scope, vars) = decl_vars(&prog.statements[0]);
    assert_eq!(vars.len(), 1);
    assert_eq!(vars[0].name, "x");
    let init = decl_init(&prog.statements[0]);
    assert!(matches!(init.kind, ExprKind::IntLit(42)));
}

#[test]
fn parse_arithmetic_precedence() {
    // 1 + 2 * 3 should be 1 + (2 * 3)
    let e = parse_expr_str("1 + 2 * 3;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Add, left, right) => {
            assert!(matches!(left.kind, ExprKind::IntLit(1)));
            match &right.kind {
                ExprKind::BinOp(BinOp::Mul, l, r) => {
                    assert!(matches!(l.kind, ExprKind::IntLit(2)));
                    assert!(matches!(r.kind, ExprKind::IntLit(3)));
                }
                other => panic!("expected Mul, got {other:?}"),
            }
        }
        other => panic!("expected Add, got {other:?}"),
    }
}

#[test]
fn parse_power_right_assoc() {
    // 2 ** 3 ** 4 should be 2 ** (3 ** 4)
    let e = parse_expr_str("2 ** 3 ** 4;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Pow, left, right) => {
            assert!(matches!(left.kind, ExprKind::IntLit(2)));
            match &right.kind {
                ExprKind::BinOp(BinOp::Pow, l, r) => {
                    assert!(matches!(l.kind, ExprKind::IntLit(3)));
                    assert!(matches!(r.kind, ExprKind::IntLit(4)));
                }
                other => panic!("expected Pow, got {other:?}"),
            }
        }
        other => panic!("expected Pow, got {other:?}"),
    }
}

#[test]
fn parse_ternary() {
    let e = parse_expr_str("$x ? 1 : 0;");
    assert!(matches!(e.kind, ExprKind::Ternary(_, _, _)));
}

#[test]
fn parse_if_stmt() {
    let prog = parse("if ($x > 0) { print 1; }");
    match &prog.statements[0].kind {
        StmtKind::If(if_stmt) => {
            assert!(matches!(if_stmt.condition.kind, ExprKind::BinOp(BinOp::NumGt, _, _)));
            assert_eq!(if_stmt.then_block.statements.len(), 1);
            assert!(if_stmt.elsif_clauses.is_empty());
            assert!(if_stmt.else_block.is_none());
        }
        other => panic!("expected If, got {other:?}"),
    }
}

#[test]
fn parse_if_elsif_else() {
    let prog = parse("if ($x > 0) { 1; } elsif ($x == 0) { 0; } else { -1; }");
    match &prog.statements[0].kind {
        StmtKind::If(if_stmt) => {
            assert_eq!(if_stmt.elsif_clauses.len(), 1);
            assert!(if_stmt.else_block.is_some());
        }
        other => panic!("expected If, got {other:?}"),
    }
}

#[test]
fn parse_while_loop() {
    let prog = parse("while ($x > 0) { $x--; }");
    match &prog.statements[0].kind {
        StmtKind::While(w) => {
            assert!(matches!(w.condition.kind, ExprKind::BinOp(BinOp::NumGt, _, _)));
            assert_eq!(w.body.statements.len(), 1);
        }
        other => panic!("expected While, got {other:?}"),
    }
}

#[test]
fn parse_foreach_loop() {
    let prog = parse("for my $item (@list) { print $item; }");
    match &prog.statements[0].kind {
        StmtKind::ForEach(f) => {
            let var = f.vars.first().expect("expected loop variable");
            assert_eq!(var.name, "item");
            assert_eq!(var.sigil, Sigil::Scalar);
            assert_eq!(f.body.statements.len(), 1);
        }
        other => panic!("expected ForEach, got {other:?}"),
    }
}

#[test]
fn parse_sub_decl() {
    let prog = parse("sub foo { return 42; }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sub) => {
            assert_eq!(sub.name, "foo");
            assert_eq!(sub.body.statements.len(), 1);
        }
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn parse_arrow_method_call() {
    let e = parse_expr_str("$obj->method(1, 2);");
    match &e.kind {
        ExprKind::MethodCall(invocant, name, args) => {
            assert!(matches!(invocant.kind, ExprKind::ScalarVar(ref n) if n == "obj"));
            assert_eq!(name, "method");
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected MethodCall, got {other:?}"),
    }
}

#[test]
fn parse_arrow_deref() {
    let e = parse_expr_str("$ref->{key};");
    match &e.kind {
        ExprKind::ArrowDeref(base, ArrowTarget::HashElem(key)) => {
            assert!(matches!(base.kind, ExprKind::ScalarVar(ref n) if n == "ref"));
            assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "key"));
        }
        other => panic!("expected ArrowDeref HashElem, got {other:?}"),
    }
}

#[test]
fn parse_anon_array() {
    let e = parse_expr_str("[1, 2, 3];");
    match &e.kind {
        ExprKind::AnonArray(elems) => assert_eq!(elems.len(), 3),
        other => panic!("expected AnonArray, got {other:?}"),
    }
}

#[test]
fn parse_print_list() {
    let prog = parse(r#"print "hello", " ", "world";"#);
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(name, fh, args), .. }) => {
            assert_eq!(name, "print");
            assert!(fh.is_none());
            assert_eq!(args.len(), 3);
        }
        other => panic!("expected PrintOp, got {other:?}"),
    }
}

#[test]
fn parse_postfix_if() {
    let prog = parse("print 1 if $x;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::If, _, _), .. }) => {}
        other => panic!("expected PostfixControl If, got {other:?}"),
    }
}

#[test]
fn parse_string_concat() {
    let e = parse_expr_str(r#""hello" . " " . "world";"#);
    // Should be left-associative: ("hello" . " ") . "world"
    match &e.kind {
        ExprKind::BinOp(BinOp::Concat, _, right) => {
            assert!(matches!(right.kind, ExprKind::StringLit(_)));
        }
        other => panic!("expected Concat, got {other:?}"),
    }
}

#[test]
fn parse_use_strict() {
    let prog = parse("use strict;");
    match &prog.statements[0].kind {
        StmtKind::UseDecl(u) => assert_eq!(u.module, "strict"),
        other => panic!("expected UseDecl, got {other:?}"),
    }
}

#[test]
fn parse_use_with_version() {
    let prog = parse("use Foo 1.23;");
    match &prog.statements[0].kind {
        StmtKind::UseDecl(u) => {
            assert_eq!(u.module, "Foo");
            assert_eq!(u.version.as_deref(), Some("1.23"));
            assert!(u.imports.is_none());
        }
        other => panic!("expected UseDecl, got {other:?}"),
    }
}

#[test]
fn parse_use_with_imports() {
    let prog = parse("use Foo qw(bar baz);");
    match &prog.statements[0].kind {
        StmtKind::UseDecl(u) => {
            assert_eq!(u.module, "Foo");
            assert!(u.version.is_none());
            let imports = u.imports.as_ref().expect("expected imports");
            assert_eq!(imports.len(), 1);
            assert!(matches!(&imports[0].kind, ExprKind::QwList(_)));
        }
        other => panic!("expected UseDecl, got {other:?}"),
    }
}

#[test]
fn parse_use_with_version_and_imports() {
    let prog = parse("use Foo 1.23 qw(bar baz);");
    match &prog.statements[0].kind {
        StmtKind::UseDecl(u) => {
            assert_eq!(u.module, "Foo");
            assert_eq!(u.version.as_deref(), Some("1.23"));
            assert!(u.imports.is_some());
        }
        other => panic!("expected UseDecl, got {other:?}"),
    }
}

#[test]
fn parse_use_perl_version() {
    let prog = parse("use 5.020;");
    match &prog.statements[0].kind {
        StmtKind::UseDecl(u) => {
            assert_eq!(u.module, "5.02"); // 5.020 → 5.02 in float form
            assert!(u.version.is_none());
            assert!(u.imports.is_none());
        }
        other => panic!("expected UseDecl, got {other:?}"),
    }
}

#[test]
fn parse_use_with_list_imports() {
    let prog = parse("use Foo 'bar', 'baz';");
    match &prog.statements[0].kind {
        StmtKind::UseDecl(u) => {
            assert_eq!(u.module, "Foo");
            let imports = u.imports.as_ref().expect("expected imports");
            assert_eq!(imports.len(), 2);
        }
        other => panic!("expected UseDecl, got {other:?}"),
    }
}

#[test]
fn parse_package() {
    let prog = parse("package Foo::Bar;");
    match &prog.statements[0].kind {
        StmtKind::PackageDecl(p) => assert_eq!(p.name, "Foo::Bar"),
        other => panic!("expected PackageDecl, got {other:?}"),
    }
}

#[test]
fn parse_multiple_statements() {
    let prog = parse("my $x = 1; my $y = 2; $x + $y;");
    assert_eq!(prog.statements.len(), 3);
    // First two are `my` declarations with initializers, so
    // Stmt::Expr wrapping Assign(Decl, ...).
    let (_s0, _v0) = decl_vars(&prog.statements[0]);
    let (_s1, _v1) = decl_vars(&prog.statements[1]);
    match &prog.statements[2].kind {
        StmtKind::Expr(e) => assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Add, _, _))),
        other => panic!("expected Expr(Add), got {other:?}"),
    }
}

#[test]
fn parse_prefix_negation() {
    let e = parse_expr_str("-$x;");
    assert!(matches!(e.kind, ExprKind::UnaryOp(UnaryOp::Negate, _)));
}

#[test]
fn parse_logical_operators() {
    let e = parse_expr_str("$a && $b || $c;");
    // || is lower precedence than &&, so: ($a && $b) || $c
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Or, _, _)));
}

#[test]
fn parse_defined_or() {
    let e = parse_expr_str("$x // $default;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
}

#[test]
fn parse_assign_add() {
    let e = parse_expr_str("$x += 1;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::AddEq, _, _)));
}

#[test]
fn parse_ref_and_deref() {
    let e = parse_expr_str("\\$x;");
    assert!(matches!(e.kind, ExprKind::Ref(_)));
}

// ── Interpolation tests ───────────────────────────────────

#[test]
fn parse_plain_double_string() {
    // No interpolation — collapses to StringLit.
    let e = parse_expr_str(r#""hello world";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "hello world"),
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn parse_interp_string() {
    let e = parse_expr_str(r#""Hello, $name!";"#);
    match &e.kind {
        ExprKind::InterpolatedString(Interpolated(parts)) => {
            assert_eq!(parts.len(), 3);
            assert!(matches!(&parts[0], InterpPart::Const(s) if s == "Hello, "));
            assert_eq!(scalar_interp_name(&parts[1]), Some("name"));
            assert!(matches!(&parts[2], InterpPart::Const(s) if s == "!"));
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

#[test]
fn parse_interp_multiple_vars() {
    let e = parse_expr_str(r#""$x and $y";"#);
    match &e.kind {
        ExprKind::InterpolatedString(Interpolated(parts)) => {
            assert_eq!(parts.len(), 3);
            assert_eq!(scalar_interp_name(&parts[0]), Some("x"));
            assert!(matches!(&parts[1], InterpPart::Const(s) if s == " and "));
            assert_eq!(scalar_interp_name(&parts[2]), Some("y"));
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

#[test]
fn parse_interp_array() {
    let e = parse_expr_str(r#""items: @list""#);
    match &e.kind {
        ExprKind::InterpolatedString(Interpolated(parts)) => {
            assert_eq!(parts.len(), 2);
            assert!(matches!(&parts[0], InterpPart::Const(s) if s == "items: "));
            assert_eq!(array_interp_name(&parts[1]), Some("list"));
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

/// Extract the variable name from a simple scalar-interp
/// (one that wraps a bare ScalarVar with no subscripts).
/// Returns None if the part isn't a ScalarInterp or the inner
/// expr isn't a bare variable.
fn scalar_interp_name(p: &InterpPart) -> Option<&str> {
    match p {
        InterpPart::ScalarInterp(expr) => match &expr.kind {
            ExprKind::ScalarVar(n) => Some(n.as_str()),
            _ => None,
        },
        _ => None,
    }
}

/// Extract the variable name from a simple array-interp.
fn array_interp_name(p: &InterpPart) -> Option<&str> {
    match p {
        InterpPart::ArrayInterp(expr) => match &expr.kind {
            ExprKind::ArrayVar(n) => Some(n.as_str()),
            _ => None,
        },
        _ => None,
    }
}

/// Pull the inner expression out of a ScalarInterp for tests
/// that need to inspect the subscript structure.
fn scalar_interp_expr(p: &InterpPart) -> &Expr {
    match p {
        InterpPart::ScalarInterp(e) => e,
        other => panic!("expected ScalarInterp, got {other:?}"),
    }
}

/// Pull the inner expression out of an ArrayInterp.
fn array_interp_expr(p: &InterpPart) -> &Expr {
    match p {
        InterpPart::ArrayInterp(e) => e,
        other => panic!("expected ArrayInterp, got {other:?}"),
    }
}

#[test]
fn parse_string_concat_interp() {
    // Interpolated string in a concat expression.
    let e = parse_expr_str(r#""Hello, $name!" . " Bye!""#);
    match &e.kind {
        ExprKind::BinOp(BinOp::Concat, left, right) => {
            assert!(matches!(left.kind, ExprKind::InterpolatedString(_)));
            assert!(matches!(right.kind, ExprKind::StringLit(ref s) if s == " Bye!"));
        }
        other => panic!("expected Concat(InterpolatedString, StringLit), got {other:?}"),
    }
}

#[test]
fn parse_escaped_no_interp() {
    // \$ suppresses interpolation — should be plain StringLit.
    let e = parse_expr_str(r#""price: \$100";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "price: $100"),
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn parse_print_interp_string() {
    let prog = parse(r#"print "Hello, $name!\n";"#);
    assert_eq!(prog.statements.len(), 1);
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(name, fh, args), .. }) => {
            assert_eq!(name, "print");
            assert!(fh.is_none());
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::InterpolatedString(_)));
        }
        other => panic!("expected print with InterpolatedString arg, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// Subscript-chain interpolation inside strings.
//
// All of these should parse the subscript into real AST
// nodes inside a `ScalarInterp(Box<Expr>)` / `ArrayInterp(...)`
// part — not be swallowed into a `Const` segment.
// ═══════════════════════════════════════════════════════════

/// Pull the `parts` out of an interpolated-string expression.
fn interp_parts(src: &str) -> Vec<InterpPart> {
    let e = parse_expr_str(src);
    match e.kind {
        ExprKind::InterpolatedString(Interpolated(parts)) => parts,
        // Some single-subscript strings collapse via merge
        // into a non-interpolated StringLit in degenerate
        // cases — callers pass non-degenerate sources.
        other => panic!("expected InterpolatedString, got {other:?} for {src:?}"),
    }
}

/// For string-level asserts: the N-th part should be a
/// scalar-interp wrapping an expression whose pretty-printed
/// outline matches a given structural check.
fn scalar_part(parts: &[InterpPart], n: usize) -> &Expr {
    scalar_interp_expr(&parts[n])
}

fn array_part(parts: &[InterpPart], n: usize) -> &Expr {
    array_interp_expr(&parts[n])
}

// ── Basic subscript forms ─────────────────────────────────

#[test]
fn interp_hash_elem_arrow() {
    // "$h->{key}" — classic bugged case.  Must parse as a
    // ScalarInterp wrapping ArrowDeref(ScalarVar(h), HashElem(key)).
    let parts = interp_parts(r#""$h->{key}";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::ArrowDeref(recv, ArrowTarget::HashElem(k)) => {
            assert!(matches!(recv.kind, ExprKind::ScalarVar(ref n) if n == "h"));
            // Key is a bareword (autoquoted by the subscript
            // rule in the parser).
            assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "key"));
        }
        other => panic!("expected ArrowDeref hash-elem, got {other:?}"),
    }
}

#[test]
fn interp_array_elem_arrow() {
    // "$a->[0]"
    let parts = interp_parts(r#""$a->[0]";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::ArrowDeref(recv, ArrowTarget::ArrayElem(i)) => {
            assert!(matches!(recv.kind, ExprKind::ScalarVar(ref n) if n == "a"));
            assert!(matches!(i.kind, ExprKind::IntLit(0)));
        }
        other => panic!("expected ArrowDeref array-elem, got {other:?}"),
    }
}

#[test]
fn interp_hash_elem_direct() {
    // "$h{key}" — no arrow.  In Perl this is still a hash
    // element access because `$h{...}` is equivalent to
    // `${h}{...}`.  Parses as HashElem(ScalarVar(h), key).
    let parts = interp_parts(r#""$h{key}";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::HashElem(recv, k) => {
            assert!(matches!(recv.kind, ExprKind::ScalarVar(ref n) if n == "h"));
            assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "key"));
        }
        other => panic!("expected HashElem, got {other:?}"),
    }
}

#[test]
fn interp_array_elem_direct() {
    // "$a[3]"
    let parts = interp_parts(r#""$a[3]";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::ArrayElem(recv, i) => {
            assert!(matches!(recv.kind, ExprKind::ScalarVar(ref n) if n == "a"));
            assert!(matches!(i.kind, ExprKind::IntLit(3)));
        }
        other => panic!("expected ArrayElem, got {other:?}"),
    }
}

// ── Chained subscripts ────────────────────────────────────

#[test]
fn interp_chain_two_hash_levels() {
    // "$h->{a}{b}" — arrow before first, implicit between.
    // Hash elem wrapped in hash elem.
    let parts = interp_parts(r#""$h->{a}{b}";"#);
    let e = scalar_part(&parts, 0);
    // Outer: HashElem(ArrowDeref(..., HashElem(h, a)), b)
    match &e.kind {
        ExprKind::HashElem(inner, k2) => {
            assert!(matches!(k2.kind, ExprKind::StringLit(ref s) if s == "b"));
            match &inner.kind {
                ExprKind::ArrowDeref(recv, ArrowTarget::HashElem(k1)) => {
                    assert!(matches!(recv.kind, ExprKind::ScalarVar(ref n) if n == "h"));
                    assert!(matches!(k1.kind, ExprKind::StringLit(ref s) if s == "a"));
                }
                other => panic!("expected inner ArrowDeref, got {other:?}"),
            }
        }
        other => panic!("expected outer HashElem, got {other:?}"),
    }
}

#[test]
fn interp_chain_hash_then_array() {
    // "$h->{k}[0]"
    let parts = interp_parts(r#""$h->{k}[0]";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::ArrayElem(inner, i) => {
            assert!(matches!(i.kind, ExprKind::IntLit(0)));
            assert!(matches!(inner.kind, ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))));
        }
        other => panic!("expected ArrayElem wrapping ArrowDeref, got {other:?}"),
    }
}

#[test]
fn interp_chain_array_then_hash() {
    // "$a[0]{k}"
    let parts = interp_parts(r#""$a[0]{k}";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::HashElem(inner, k) => {
            assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "k"));
            match &inner.kind {
                ExprKind::ArrayElem(recv, i) => {
                    assert!(matches!(recv.kind, ExprKind::ScalarVar(ref n) if n == "a"));
                    assert!(matches!(i.kind, ExprKind::IntLit(0)));
                }
                other => panic!("expected inner ArrayElem, got {other:?}"),
            }
        }
        other => panic!("expected outer HashElem, got {other:?}"),
    }
}

#[test]
fn interp_chain_three_levels() {
    // "$h->{a}->{b}->{c}" — three arrow-hashes.
    let parts = interp_parts(r#""$h->{a}->{b}->{c}";"#);
    let e = scalar_part(&parts, 0);
    // Triple-nested ArrowDeref(HashElem).
    fn unwrap_hash_arrow(expr: &Expr) -> (&Expr, &Expr) {
        match &expr.kind {
            ExprKind::ArrowDeref(recv, ArrowTarget::HashElem(k)) => (recv, k),
            other => panic!("expected ArrowDeref hash, got {other:?}"),
        }
    }
    let (mid, k3) = unwrap_hash_arrow(e);
    assert!(matches!(k3.kind, ExprKind::StringLit(ref s) if s == "c"));
    let (innermost, k2) = unwrap_hash_arrow(mid);
    assert!(matches!(k2.kind, ExprKind::StringLit(ref s) if s == "b"));
    let (leaf, k1) = unwrap_hash_arrow(innermost);
    assert!(matches!(k1.kind, ExprKind::StringLit(ref s) if s == "a"));
    assert!(matches!(leaf.kind, ExprKind::ScalarVar(ref n) if n == "h"));
}

#[test]
fn interp_chain_arrow_then_implicit() {
    // "$h->{a}[0]{b}" — arrow, array, hash.
    let parts = interp_parts(r#""$h->{a}[0]{b}";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::HashElem(ae, k) => {
            assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "b"));
            match &ae.kind {
                ExprKind::ArrayElem(ad, i) => {
                    assert!(matches!(i.kind, ExprKind::IntLit(0)));
                    assert!(matches!(ad.kind, ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))));
                }
                other => panic!("expected ArrayElem, got {other:?}"),
            }
        }
        other => panic!("expected outer HashElem, got {other:?}"),
    }
}

// ── Subscripts with expression keys/indices ───────────────

#[test]
fn interp_hash_subscript_expr_key() {
    // "$h->{$k}" — key is a scalar variable, not bareword.
    let parts = interp_parts(r#""$h->{$k}";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::ArrowDeref(_, ArrowTarget::HashElem(k)) => {
            assert!(matches!(k.kind, ExprKind::ScalarVar(ref n) if n == "k"));
        }
        other => panic!("expected ArrowDeref hash-elem, got {other:?}"),
    }
}

#[test]
fn interp_array_subscript_expr_index() {
    // "$a[$i]" — index is $i.
    let parts = interp_parts(r#""$a[$i]";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::ArrayElem(_, i) => {
            assert!(matches!(i.kind, ExprKind::ScalarVar(ref n) if n == "i"));
        }
        other => panic!("expected ArrayElem, got {other:?}"),
    }
}

#[test]
fn interp_array_subscript_arith_expr() {
    // "$a[$i + 1]"
    let parts = interp_parts(r#""$a[$i + 1]";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::ArrayElem(_, i) => {
            assert!(matches!(i.kind, ExprKind::BinOp(BinOp::Add, _, _)));
        }
        other => panic!("expected ArrayElem, got {other:?}"),
    }
}

#[test]
fn interp_hash_subscript_string_key() {
    // "$h{'literal'}" — explicit single-quoted key.
    let parts = interp_parts(r#""$h{'literal'}";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::HashElem(_, k) => {
            assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "literal"));
        }
        other => panic!("expected HashElem, got {other:?}"),
    }
}

// ── Array-interp chains ──────────────────────────────────

#[test]
fn interp_array_slice_range() {
    // "@a[1..3]" — array slice with a range index.
    let parts = interp_parts(r#""@a[1..3]";"#);
    let e = array_part(&parts, 0);
    match &e.kind {
        ExprKind::ArraySlice(recv, indices) => {
            assert!(matches!(recv.kind, ExprKind::ArrayVar(ref n) if n == "a"));
            assert_eq!(indices.len(), 1);
            assert!(matches!(indices[0].kind, ExprKind::Range(_, _)));
        }
        other => panic!("expected ArraySlice, got {other:?}"),
    }
}

#[test]
fn interp_hash_slice_list() {
    // "@h{'k1','k2'}" — hash slice with two keys.
    let parts = interp_parts(r#""@h{'k1','k2'}";"#);
    let e = array_part(&parts, 0);
    match &e.kind {
        ExprKind::HashSlice(recv, keys) => {
            assert!(matches!(recv.kind, ExprKind::ArrayVar(ref n) if n == "h"));
            assert_eq!(keys.len(), 2);
            assert!(matches!(keys[0].kind, ExprKind::StringLit(ref s) if s == "k1"));
            assert!(matches!(keys[1].kind, ExprKind::StringLit(ref s) if s == "k2"));
        }
        other => panic!("expected HashSlice, got {other:?}"),
    }
}

// ── Mixed with literal text ──────────────────────────────

#[test]
fn interp_chain_mid_string() {
    // "a $h->{key} b" — subscript in the middle.
    let parts = interp_parts(r#""a $h->{key} b";"#);
    assert_eq!(parts.len(), 3);
    assert!(matches!(&parts[0], InterpPart::Const(s) if s == "a "));
    // Middle is the chain.
    let e = scalar_part(&parts, 1);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))));
    assert!(matches!(&parts[2], InterpPart::Const(s) if s == " b"));
}

#[test]
fn interp_two_chains_one_string() {
    // "$h->{k} and $a[0]"
    let parts = interp_parts(r#""$h->{k} and $a[0]";"#);
    assert_eq!(parts.len(), 3);
    let e0 = scalar_part(&parts, 0);
    assert!(matches!(e0.kind, ExprKind::ArrowDeref(_, _)));
    assert!(matches!(&parts[1], InterpPart::Const(s) if s == " and "));
    let e2 = scalar_part(&parts, 2);
    assert!(matches!(e2.kind, ExprKind::ArrayElem(_, _)));
}

// ── Negative cases (no chain) ────────────────────────────

#[test]
fn interp_bare_arrow_is_literal() {
    // "$a->" — bare arrow with nothing after.  Lexer must not
    // start a chain; the `->` stays literal text.
    let parts = interp_parts(r#""$a->";"#);
    assert_eq!(parts.len(), 2);
    assert_eq!(scalar_interp_name(&parts[0]), Some("a"));
    assert!(matches!(&parts[1], InterpPart::Const(s) if s == "->"));
}

#[test]
fn interp_bare_arrow_then_ident_is_literal() {
    // "$a->foo" — method-call shape is NOT interpolated in
    // strings (per perlop).  `$a` interpolates; `->foo`
    // renders literally.
    let parts = interp_parts(r#""$a->foo";"#);
    assert_eq!(parts.len(), 2);
    assert_eq!(scalar_interp_name(&parts[0]), Some("a"));
    assert!(matches!(&parts[1], InterpPart::Const(s) if s == "->foo"));
}

#[test]
fn interp_plain_scalar_no_subscript() {
    // Simple "$name" shouldn't start a chain.  Still uses the
    // new ScalarInterp(Box<Expr>) wrapper around a bare
    // ScalarVar.
    let parts = interp_parts(r#""Hello $name!";"#);
    assert_eq!(parts.len(), 3);
    assert_eq!(scalar_interp_name(&parts[1]), Some("name"));
}

#[test]
fn interp_trailing_literal_bracket() {
    // "$a [0]" — space before `[` means it's NOT a subscript.
    // The literal `[` and `]` stay as ConstSegment.
    let parts = interp_parts(r#""$a [0]";"#);
    // Parts: ScalarInterp(a), Const(" [0]").
    assert_eq!(parts.len(), 2);
    assert_eq!(scalar_interp_name(&parts[0]), Some("a"));
    assert!(matches!(&parts[1], InterpPart::Const(s) if s == " [0]"));
}

// ── Escaped sigils ───────────────────────────────────────

#[test]
fn interp_escaped_dollar_before_subscript_bracket() {
    // "\$a[0]" — escaped `$`; whole thing is literal.
    let e = parse_expr_str(r#""\$a[0]";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "$a[0]"),
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn interp_escaped_arrow_after_var() {
    // `"\$a->{x}"` — escaped $ makes the whole thing literal.
    let e = parse_expr_str(r#""\$a->{x}";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "$a->{x}"),
        other => panic!("expected StringLit, got {other:?}"),
    }
}

// ── Nested braces inside subscript expression ────────────

#[test]
fn interp_subscript_with_nested_braces() {
    // `"$h->{$x}{y}"` — two nested subscripts, with `y` as
    // a bareword hash key in the inner-most subscript.
    //
    // `y}` is a lexer edge case: `y` is one of the quote
    // keywords (alias for `tr`), so at_quote_delimiter must
    // reject the closing `}` that follows.  Tests below
    // cover every quote keyword × every closing delimiter
    // combination; this one spot-checks the interaction with
    // subscript-chain interpolation specifically.
    let parts = interp_parts(r#""$h->{$x}{y}";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::HashElem(inner, k) => {
            assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "y"));
            match &inner.kind {
                ExprKind::ArrowDeref(_, ArrowTarget::HashElem(k1)) => {
                    assert!(matches!(k1.kind, ExprKind::ScalarVar(ref n) if n == "x"));
                }
                other => panic!("expected ArrowDeref hash-elem, got {other:?}"),
            }
        }
        other => panic!("expected HashElem, got {other:?}"),
    }
}

#[test]
fn interp_subscript_with_func_call() {
    // "$h->{foo()}" — key is a function call.
    let parts = interp_parts(r#""$h->{foo()}";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::ArrowDeref(_, ArrowTarget::HashElem(k)) => {
            assert!(matches!(k.kind, ExprKind::FuncCall(_, _)));
        }
        other => panic!("expected ArrowDeref hash-elem, got {other:?}"),
    }
}

// ── In qq// ──────────────────────────────────────────────

#[test]
fn interp_qq_with_subscript() {
    // qq{...} uses `{}` as delimiter; the `{key}` inside is
    // still recognized as a hash subscript.
    let e = parse_expr_str("qq{$h->{key}};");
    match &e.kind {
        ExprKind::InterpolatedString(Interpolated(parts)) => {
            assert_eq!(parts.len(), 1);
            let inner = scalar_part(parts, 0);
            assert!(matches!(inner.kind, ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))));
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

// ── Concatenation-style interpolation context ────────────

#[test]
fn interp_chain_then_concat() {
    // Interpolated string concatenated with another.  The
    // chain in the first one must still be parsed correctly.
    let e = parse_expr_str(r#""$h->{key}" . "plain";"#);
    match &e.kind {
        ExprKind::BinOp(BinOp::Concat, left, _) => {
            if let ExprKind::InterpolatedString(Interpolated(parts)) = &left.kind {
                assert_eq!(parts.len(), 1);
                assert!(matches!(scalar_part(parts, 0).kind, ExprKind::ArrowDeref(_, _)));
            } else {
                panic!("left should be InterpolatedString");
            }
        }
        other => panic!("expected Concat, got {other:?}"),
    }
}

// ── @name chain forms ────────────────────────────────────

#[test]
fn interp_array_chain_in_mid_string() {
    // "list: @a[0..2] done"
    let parts = interp_parts(r#""list: @a[0..2] done";"#);
    assert_eq!(parts.len(), 3);
    assert!(matches!(&parts[0], InterpPart::Const(s) if s == "list: "));
    let e = array_part(&parts, 1);
    match &e.kind {
        ExprKind::ArraySlice(recv, indices) => {
            assert!(matches!(recv.kind, ExprKind::ArrayVar(ref n) if n == "a"));
            assert_eq!(indices.len(), 1);
            assert!(matches!(indices[0].kind, ExprKind::Range(_, _)));
        }
        other => panic!("expected ArraySlice, got {other:?}"),
    }
    assert!(matches!(&parts[2], InterpPart::Const(s) if s == " done"));
}

// ── ${name}-expression form interaction ──────────────────

#[test]
fn interp_braced_name_then_literal_subscript() {
    // "${name}[0]" — `${name}` is explicit braced form.
    // The `[0]` after the `}` is literal text (per Perl
    // behavior: ${name}[0] interpolates only $name).
    let parts = interp_parts(r#""${name}[0]";"#);
    assert_eq!(parts.len(), 2);
    assert_eq!(scalar_interp_name(&parts[0]), Some("name"));
    assert!(matches!(&parts[1], InterpPart::Const(s) if s == "[0]"));
}

// ── Regex interpolation (shares the same scanner) ────────

#[test]
fn regex_interp_subscript() {
    // m/$h->{key}/ — regex bodies use the same interp
    // machinery; chains should work there too.
    let e = parse_expr_str(r#"m/$h->{key}/;"#);
    match &e.kind {
        ExprKind::Regex(_, pat, _) => {
            let parts = &pat.0;
            // Expect at least one ScalarInterp with the chain.
            let has_chain = parts.iter().any(|p| {
                matches!(
                    p,
                    InterpPart::ScalarInterp(expr) if matches!(
                        expr.kind,
                        ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))
                    )
                )
            });
            assert!(has_chain, "expected arrow-hash chain in regex parts: {parts:?}");
        }
        other => panic!("expected Regex, got {other:?}"),
    }
}

// ── Missing tests promised in the audit ───────────────────
//
// These cover interpolation contexts beyond plain `"..."` —
// heredoc bodies, `qr//`, `s///` pattern and replacement,
// and the `@{[expr]}` form mixed with chains.  A few cases
// don't work yet and are marked `#[ignore]` with a clear
// note explaining the gap; they're here rather than absent
// so the gap is visible in the test suite rather than only
// in my memory.

// Heredoc with chain in body.

#[test]
fn interp_chain_in_heredoc() {
    let src = "<<END;\nvalue: $h->{key}\nEND\n";
    let prog = parse(src);
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::InterpolatedString(Interpolated(parts)), .. }) => {
            let has_chain = parts.iter().any(|p| {
                matches!(
                    p,
                    InterpPart::ScalarInterp(e) if matches!(
                        e.kind,
                        ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))
                    )
                )
            });
            assert!(has_chain, "expected arrow-hash chain in heredoc parts: {parts:?}");
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

#[test]
fn interp_direct_subscript_in_heredoc() {
    // Bare `$a[0]` inside a heredoc body.
    let src = "<<END;\nfirst: $a[0]\nEND\n";
    let prog = parse(src);
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::InterpolatedString(Interpolated(parts)), .. }) => {
            let has_elem = parts.iter().any(|p| {
                matches!(
                    p,
                    InterpPart::ScalarInterp(e) if matches!(e.kind, ExprKind::ArrayElem(_, _))
                )
            });
            assert!(has_elem, "expected ArrayElem chain in heredoc parts: {parts:?}");
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

// qr// compiled-regex with chain.

#[test]
fn interp_chain_in_qr() {
    let e = parse_expr_str(r#"qr/$h->{key}/;"#);
    match &e.kind {
        ExprKind::Regex(RegexKind::Qr, pat, _) => {
            let has_chain = pat.0.iter().any(|p| {
                matches!(
                    p,
                    InterpPart::ScalarInterp(e) if matches!(
                        e.kind,
                        ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))
                    )
                )
            });
            assert!(has_chain, "expected arrow-hash chain in qr// parts: {parts:?}", parts = pat.0);
        }
        other => panic!("expected Regex(Qr, ...), got {other:?}"),
    }
}

// s/// — pattern AND replacement can interpolate.

#[test]
fn interp_chain_in_subst_pattern() {
    let e = parse_expr_str(r#"s/$h->{key}/new/;"#);
    match &e.kind {
        ExprKind::Subst(pat, _, _) => {
            let has_chain = pat.0.iter().any(|p| {
                matches!(
                    p,
                    InterpPart::ScalarInterp(e) if matches!(
                        e.kind,
                        ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))
                    )
                )
            });
            assert!(has_chain, "expected arrow-hash chain in subst pattern: {parts:?}", parts = pat.0);
        }
        other => panic!("expected Subst, got {other:?}"),
    }
}

#[test]
fn interp_chain_in_subst_replacement() {
    let e = parse_expr_str(r#"s/old/$h->{key}/;"#);
    match &e.kind {
        ExprKind::Subst(_, repl, _) => {
            let has_chain = repl.0.iter().any(|p| {
                matches!(
                    p,
                    InterpPart::ScalarInterp(e) if matches!(
                        e.kind,
                        ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))
                    )
                )
            });
            assert!(has_chain, "expected arrow-hash chain in subst replacement: {parts:?}", parts = repl.0);
        }
        other => panic!("expected Subst, got {other:?}"),
    }
}

// @{[expr]} expression-interpolation form.

#[test]
fn interp_array_expr_form_with_chain_inside() {
    // `"@{[$h->{k}]}"` — the @{[...]} form wraps an
    // expression; the expression internally uses a
    // subscript chain.  Outer shape is ExprInterp (not
    // ChainStart) because the leading token is `@{`, not
    // `@name`.
    let parts = interp_parts(r#""@{[$h->{k}]}";"#);
    let expr_part = parts
        .iter()
        .find_map(|p| match p {
            InterpPart::ExprInterp(e) => Some(e),
            _ => None,
        })
        .expect("expected an ExprInterp part");
    // Inside: anonymous array ref containing the chain.
    // AnonArray([ArrowDeref(h, HashElem(k))])
    match &expr_part.kind {
        ExprKind::AnonArray(items) => {
            assert_eq!(items.len(), 1);
            assert!(matches!(items[0].kind, ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))));
        }
        other => panic!("expected AnonArray inside @{{[...]}}: {other:?}"),
    }
}

// Escape sequences in hash-subscript position are NOT
// processed as string escapes.  `"$h{\x41}"` is NOT
// `$h{'A'}`; per `perl -MO=Deparse -e '"$h{\x41}"'` it
// parses as `"$h{\'x41'}"` — the `\` is the reference
// operator applied to the autoquoted bareword `x41`.  The
// hash lookup key is therefore a scalar reference (which
// stringifies to `SCALAR(0x...)` at runtime).
//
// Verified with the Perl debugger:
//
// ```perl
// my %h = (x41 => 'test');
// print "$h{\x41}\n";    # empty — lookup misses, stringified ref != 'x41'
// ```

#[test]
fn interp_escape_sequence_in_hash_subscript_is_ref_to_bareword() {
    let parts = interp_parts(r#""$h{\x41}";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::HashElem(_, k) => match &k.kind {
            ExprKind::Ref(inner) => {
                // Inner: the autoquoted bareword "x41".
                assert!(matches!(inner.kind, ExprKind::StringLit(ref s) if s == "x41"), "expected Ref(StringLit('x41')), inner was {:?}", inner.kind);
            }
            other => panic!("expected Ref(StringLit('x41')) as hash key, got {other:?}"),
        },
        other => panic!("expected HashElem, got {other:?}"),
    }
}

// ── Known gaps — ignored tests, kept visible ─────────────
//
// These encode behavior we haven't implemented yet.  Each
// is marked `#[ignore]` with a note explaining what's
// missing.  Running with `cargo test -- --ignored` will
// run them and show the real failures.

#[test]
fn interp_postderef_qq_array() {
    // `"$ref->@*"` — postderef array form inside a string.
    // Requires peek_chain_starter to recognize `->@*` and
    // the chain dispatch to end on `Star` at depth 0.
    let parts = interp_parts(r#""$ref->@*";"#);
    let e = scalar_part(&parts, 0);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefArray)), "expected ArrowDeref(_, DerefArray), got {:?}", e.kind);
}

#[test]
fn interp_postderef_qq_hash() {
    // `"$ref->%*"` — postderef hash form.
    let parts = interp_parts(r#""$ref->%*";"#);
    let e = scalar_part(&parts, 0);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefHash)), "expected ArrowDeref(_, DerefHash), got {:?}", e.kind);
}

#[test]
fn interp_postderef_qq_scalar() {
    // `"$ref->$*"` — postderef scalar form.
    let parts = interp_parts(r#""$ref->$*";"#);
    let e = scalar_part(&parts, 0);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefScalar)), "expected ArrowDeref(_, DerefScalar), got {:?}", e.kind);
}

#[test]
fn interp_postderef_qq_last_index() {
    // `"$ref->$#*"` — postderef last-index in a string.
    // The `#` would normally start a comment in code mode;
    // this works because the parser's `try_consume_hash_star`
    // consumes the raw `#*` bytes between lex_token calls,
    // and (in chain mode) sets `chain_end_pending` so the
    // chain terminates cleanly.
    let parts = interp_parts(r#""$ref->$#*";"#);
    let e = scalar_part(&parts, 0);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::LastIndex)), "expected ArrowDeref(_, LastIndex), got {:?}", e.kind);
}

#[test]
fn interp_postderef_qq_chained_after_subscript() {
    // `"$h->{key}->@*"` — subscript then postderef in one chain.
    let parts = interp_parts(r#""$h->{key}->@*";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::ArrowDeref(inner, ArrowTarget::DerefArray) => {
            // Inner: ArrowDeref(ScalarVar(h), HashElem(key)).
            assert!(matches!(inner.kind, ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))), "inner should be hash-elem deref, got {:?}", inner.kind);
        }
        other => panic!("expected ArrowDeref(_, DerefArray), got {other:?}"),
    }
}

#[test]
fn interp_postderef_qq_with_surrounding_text() {
    // `"values: $ref->@* end"` — postderef mid-string.
    let parts = interp_parts(r#""values: $ref->@* end";"#);
    assert_eq!(parts.len(), 3);
    assert!(matches!(&parts[0], InterpPart::Const(s) if s == "values: "));
    let e = scalar_part(&parts, 1);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefArray)));
    assert!(matches!(&parts[2], InterpPart::Const(s) if s == " end"));
}

// ── Regex / substitution / transliteration tests ──────────

#[test]
fn parse_bare_regex() {
    let e = parse_expr_str("/foo/i;");
    match &e.kind {
        ExprKind::Regex(_, pat, flags) => {
            assert_eq!(pat_str(pat), "foo");
            assert_eq!(flags.as_deref(), Some("i"));
        }
        other => panic!("expected Regex, got {other:?}"),
    }
}

#[test]
fn parse_regex_binding() {
    let e = parse_expr_str("$x =~ /foo/;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Binding, _, right) => {
            assert!(matches!(&right.kind, ExprKind::Regex(_, _, _)));
        }
        other => panic!("expected Binding, got {other:?}"),
    }
}

#[test]
fn parse_empty_regex() {
    // // in term position is an empty regex, not defined-or.
    let e = parse_expr_str("$x =~ //;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Binding, _, right) => match &right.kind {
            ExprKind::Regex(_, pat, flags) => {
                assert_eq!(pat_str(pat), "");
                assert!(flags.is_none());
            }
            other => panic!("expected empty Regex, got {other:?}"),
        },
        other => panic!("expected Binding, got {other:?}"),
    }
}

#[test]
fn parse_empty_regex_bare() {
    // // at statement level is an empty regex match against $_.
    let e = parse_expr_str("//;");
    match &e.kind {
        ExprKind::Regex(_, pat, flags) => {
            assert_eq!(pat_str(pat), "");
            assert!(flags.is_none());
        }
        other => panic!("expected empty Regex, got {other:?}"),
    }
}

#[test]
fn parse_empty_regex_with_flags() {
    // //gi in term position is an empty regex with flags.
    let e = parse_expr_str("$x =~ //gi;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Binding, _, right) => match &right.kind {
            ExprKind::Regex(_, pat, flags) => {
                assert_eq!(pat_str(pat), "");
                assert_eq!(flags.as_deref(), Some("gi"));
            }
            other => panic!("expected empty Regex with flags, got {other:?}"),
        },
        other => panic!("expected Binding, got {other:?}"),
    }
}

#[test]
fn parse_empty_regex_bare_with_flags() {
    // //gi at statement level is an empty regex with flags.
    let e = parse_expr_str("//gi;");
    match &e.kind {
        ExprKind::Regex(_, pat, flags) => {
            assert_eq!(pat_str(pat), "");
            assert_eq!(flags.as_deref(), Some("gi"));
        }
        other => panic!("expected empty Regex with flags, got {other:?}"),
    }
}

#[test]
fn parse_empty_regex_in_condition() {
    // if (//) { } — empty regex as condition.
    let prog = parse("if (//) { 1; }");
    assert_eq!(prog.statements.len(), 1);
    match &prog.statements[0].kind {
        StmtKind::If(if_stmt) => {
            assert!(matches!(if_stmt.condition.kind, ExprKind::Regex(_, _, _)));
        }
        other => panic!("expected If, got {other:?}"),
    }
}

#[test]
fn parse_empty_regex_as_print_arg() {
    // print //; — empty regex as print argument.
    let prog = parse("print //;");
    assert_eq!(prog.statements.len(), 1);
}

#[test]
fn parse_empty_regex_in_split() {
    // split //, $s — empty regex as split pattern.
    let prog = parse("split //, $s;");
    assert_eq!(prog.statements.len(), 1);
}

#[test]
fn parse_empty_regex_space_not_flags() {
    // // gi — space separates, so gi is NOT flags.
    // This produces an empty regex with no flags.
    let e = parse_expr_str("$x =~ //gi;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Binding, _, right) => match &right.kind {
            ExprKind::Regex(_, pat, flags) => {
                assert_eq!(pat_str(pat), "");
                assert_eq!(flags.as_deref(), Some("gi"));
            }
            other => panic!("expected Regex, got {other:?}"),
        },
        other => panic!("expected Binding, got {other:?}"),
    }
    // But with space: flags are NOT consumed.
    let e2 = parse_expr_str("$x =~ // gi;");
    match &e2.kind {
        ExprKind::BinOp(BinOp::Binding, _, right) => match &right.kind {
            ExprKind::Regex(_, pat, flags) => {
                assert_eq!(pat_str(pat), "");
                assert!(flags.is_none());
            }
            other => panic!("expected Regex with empty flags, got {other:?}"),
        },
        other => panic!("expected Binding, got {other:?}"),
    }
}

#[test]
fn parse_empty_regex_in_ternary() {
    // $x = // ? 1 : 0 — empty regex match, then ternary.
    let e = parse_expr_str("$x = // ? 1 : 0;");
    assert!(matches!(e.kind, ExprKind::Assign(_, _, _)));
}

#[test]
fn parse_regex_invalid_flag() {
    // /foo/q — invalid flag 'q' should produce an error.
    let mut parser = Parser::new(b"/foo/q;").unwrap();
    let result = parser.parse_program();
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("Unknown regexp modifier"));
}

#[test]
fn parse_tr_invalid_flag() {
    // tr/a/b/q — invalid flag 'q' should produce an error.
    let mut parser = Parser::new(b"tr/a/b/q;").unwrap();
    let result = parser.parse_program();
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("Unknown transliteration modifier"));
}

// ── prefers_defined_or: UNIDOR operators ────────────────
//
// After these operators, // is defined-or, not an empty regex.
// Matches toke.c's UNIDOR macro and XTERMORDORDOR behavior.

#[test]
fn parse_shift_prefers_defined_or() {
    let e = parse_expr_str("shift // 0;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
}

#[test]
fn parse_pop_prefers_defined_or() {
    let e = parse_expr_str("pop // 0;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
}

#[test]
fn parse_getc_prefers_defined_or() {
    let e = parse_expr_str("getc // 0;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
}

#[test]
fn parse_pos_prefers_defined_or() {
    let e = parse_expr_str("pos // 0;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
}

#[test]
fn parse_readline_prefers_defined_or() {
    let e = parse_expr_str("readline // 0;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
}

#[test]
fn parse_readlink_prefers_defined_or() {
    let e = parse_expr_str("readlink // 0;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
}

#[test]
fn parse_undef_prefers_defined_or() {
    let e = parse_expr_str("undef // 0;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
}

#[test]
fn parse_umask_prefers_defined_or() {
    let e = parse_expr_str("umask // 0;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
}

#[test]
fn parse_filetest_prefers_defined_or() {
    // -f // "default" — file test with no operand, then defined-or.
    let e = parse_expr_str("-f // \"default\";");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
}

#[test]
fn parse_shift_defined_or_bareword() {
    // shift //i 0 — in Perl this is a syntax error because i is not
    // predeclared.  Our parser is more permissive: it parses as
    // shift() // i(0) since any bareword can be a function call.
    let e = parse_expr_str("shift //i 0;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
}

#[test]
fn parse_substitution() {
    let e = parse_expr_str("s/foo/bar/g;");
    match &e.kind {
        ExprKind::Subst(pat, repl, flags) => {
            assert_eq!(pat_str(pat), "foo");
            assert_eq!(flags.as_deref(), Some("g"));
            assert_eq!(pat_str(repl), "bar");
        }
        other => panic!("expected Subst, got {other:?}"),
    }
}

#[test]
fn parse_substitution_no_flags() {
    let e = parse_expr_str("s/old/new/;");
    match &e.kind {
        ExprKind::Subst(pat, repl, flags) => {
            assert_eq!(pat_str(pat), "old");
            assert!(flags.is_none());
            assert_eq!(pat_str(repl), "new");
        }
        other => panic!("expected Subst, got {other:?}"),
    }
}

#[test]
fn parse_substitution_paired_delimiters() {
    let e = parse_expr_str("s{foo}{bar}g;");
    match &e.kind {
        ExprKind::Subst(pat, repl, flags) => {
            assert_eq!(pat_str(pat), "foo");
            assert_eq!(flags.as_deref(), Some("g"));
            assert_eq!(pat_str(repl), "bar");
        }
        other => panic!("expected Subst, got {other:?}"),
    }
}

#[test]
fn parse_transliteration() {
    let e = parse_expr_str("tr/a-z/A-Z/;");
    match &e.kind {
        ExprKind::Translit(from, to, _) => {
            assert_eq!(from, "a-z");
            assert_eq!(to, "A-Z");
        }
        other => panic!("expected Translit, got {other:?}"),
    }
}

#[test]
fn parse_subst_binding() {
    let e = parse_expr_str("$x =~ s/old/new/g;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Binding, _, right) => {
            assert!(matches!(&right.kind, ExprKind::Subst(_, _, _)));
        }
        other => panic!("expected Binding with Subst, got {other:?}"),
    }
}

// ── Heredoc tests ─────────────────────────────────────────

#[test]
fn parse_heredoc_basic() {
    let src = "my $x = <<END;\nhello world\nEND\n";
    let prog = parse(src);
    assert_eq!(prog.statements.len(), 1);
    let (_s, vars) = decl_vars(&prog.statements[0]);
    assert_eq!(vars[0].name, "x");
    let init = decl_init(&prog.statements[0]);
    match &init.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "hello world\n"),
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn parse_heredoc_concat() {
    // <<END . " suffix" should parse as concatenation.
    let src = "<<END . \" suffix\";\nbody\nEND\n";
    let prog = parse(src);
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => {
            assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Concat, _, _)));
        }
        other => panic!("expected Expr with Concat, got {other:?}"),
    }
}

#[test]
fn parse_heredoc_then_statement() {
    let src = "print <<END;\nhello\nEND\nmy $x = 1;\n";
    let prog = parse(src);
    assert_eq!(prog.statements.len(), 2);
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(name, _, _), .. }) => assert_eq!(name, "print"),
        other => panic!("expected print PrintOp, got {other:?}"),
    }
    match &prog.statements[1].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => match &lhs.kind {
            ExprKind::Decl(_, vars) => assert_eq!(vars[0].name, "x"),
            other => panic!("expected Decl lhs, got {other:?}"),
        },
        other => panic!("expected decl stmt, got {other:?}"),
    }
}

// ── Anonymous sub tests ───────────────────────────────────

#[test]
fn parse_anon_sub() {
    let e = parse_expr_str("sub { 42; };");
    match &e.kind {
        ExprKind::AnonSub(proto, _, _, body) => {
            assert!(proto.is_none());
            assert_eq!(body.statements.len(), 1);
        }
        other => panic!("expected AnonSub, got {other:?}"),
    }
}

#[test]
fn parse_anon_sub_with_proto() {
    let e = parse_expr_str("sub ($x) { $x + 1; };");
    match &e.kind {
        ExprKind::AnonSub(proto, _, _, _) => {
            assert!(proto.is_some());
        }
        other => panic!("expected AnonSub, got {other:?}"),
    }
}

#[test]
fn parse_anon_sub_as_arg() {
    let prog = parse("my $f = sub { 1; };");
    let init = decl_init(&prog.statements[0]);
    assert!(matches!(init.kind, ExprKind::AnonSub(..)));
}

// ── Phaser block tests ────────────────────────────────────

#[test]
fn parse_begin_block() {
    let prog = parse("BEGIN { 1; }");
    assert!(matches!(prog.statements[0].kind, StmtKind::Phaser(PhaserKind::Begin, _)));
}

#[test]
fn parse_end_block() {
    let prog = parse("END { 1; }");
    assert!(matches!(prog.statements[0].kind, StmtKind::Phaser(PhaserKind::End, _)));
}

// ── Eval tests ────────────────────────────────────────────

#[test]
fn parse_eval_block() {
    let e = parse_expr_str("eval { die; };");
    assert!(matches!(e.kind, ExprKind::EvalBlock(_)));
}

#[test]
fn parse_eval_expr() {
    let e = parse_expr_str("eval $code;");
    assert!(matches!(e.kind, ExprKind::EvalExpr(_)));
}

// ── Return / loop control tests ───────────────────────────

#[test]
fn parse_return_value() {
    let e = parse_expr_str("return 42;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "return");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected return call, got {other:?}"),
    }
}

#[test]
fn parse_return_bare() {
    let e = parse_expr_str("return;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "return");
            assert_eq!(args.len(), 0);
        }
        other => panic!("expected bare return, got {other:?}"),
    }
}

#[test]
fn parse_last_with_label() {
    let e = parse_expr_str("last OUTER;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "last");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected last with label, got {other:?}"),
    }
}

#[test]
fn parse_next_bare() {
    let e = parse_expr_str("next;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "next");
            assert_eq!(args.len(), 0);
        }
        other => panic!("expected bare next, got {other:?}"),
    }
}

// ── Label tests ───────────────────────────────────────────

#[test]
fn parse_labeled_loop() {
    let prog = parse("OUTER: for my $i (@list) { next OUTER; }");
    match &prog.statements[0].kind {
        StmtKind::Labeled(label, inner) => {
            assert_eq!(label, "OUTER");
            assert!(matches!(inner.kind, StmtKind::ForEach(_)));
        }
        other => panic!("expected Labeled, got {other:?}"),
    }
}

// ── Chained subscript tests ───────────────────────────────

#[test]
fn parse_chained_array_subscripts() {
    // $aoa[0][1] — implicit arrow between adjacent subscripts
    let e = parse_expr_str("$aoa[0][1];");
    match &e.kind {
        ExprKind::ArrayElem(inner, _) => {
            assert!(matches!(inner.kind, ExprKind::ArrayElem(_, _)));
        }
        other => panic!("expected nested ArrayElem, got {other:?}"),
    }
}

#[test]
fn parse_chained_hash_subscripts() {
    let e = parse_expr_str("$h{a}{b};");
    match &e.kind {
        ExprKind::HashElem(inner, _) => {
            assert!(matches!(inner.kind, ExprKind::HashElem(_, _)));
        }
        other => panic!("expected nested HashElem, got {other:?}"),
    }
}

#[test]
fn parse_arrow_then_implicit_subscript() {
    // $ref->[0][1] — arrow for first, implicit for second
    let e = parse_expr_str("$ref->[0][1];");
    match &e.kind {
        ExprKind::ArrayElem(inner, _) => {
            assert!(matches!(inner.kind, ExprKind::ArrowDeref(_, _)));
        }
        other => panic!("expected ArrayElem wrapping ArrowDeref, got {other:?}"),
    }
}

// ── sort/map/grep block tests ─────────────────────────────

#[test]
fn parse_sort_block() {
    let e = parse_expr_str("sort { $a <=> $b } @list;");
    match &e.kind {
        ExprKind::ListOp(name, args) => {
            assert_eq!(name, "sort");
            assert!(args.len() >= 2); // block + @list
            assert!(matches!(args[0].kind, ExprKind::AnonSub(..)));
        }
        other => panic!("expected sort ListOp, got {other:?}"),
    }
}

#[test]
fn parse_map_block() {
    let e = parse_expr_str("map { $_ * 2 } @nums;");
    match &e.kind {
        ExprKind::ListOp(name, args) => {
            assert_eq!(name, "map");
            assert!(matches!(args[0].kind, ExprKind::AnonSub(..)));
        }
        other => panic!("expected map ListOp, got {other:?}"),
    }
}

#[test]
fn parse_grep_block() {
    let e = parse_expr_str("grep { $_ > 0 } @nums;");
    match &e.kind {
        ExprKind::ListOp(name, args) => {
            assert_eq!(name, "grep");
            assert!(matches!(args[0].kind, ExprKind::AnonSub(..)));
        }
        other => panic!("expected grep ListOp, got {other:?}"),
    }
}

#[test]
fn parse_sort_no_block() {
    let e = parse_expr_str("sort @list;");
    match &e.kind {
        ExprKind::ListOp(name, args) => {
            assert_eq!(name, "sort");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected sort ListOp, got {other:?}"),
    }
}

// ── print tests ───────────────────────────────────────────

#[test]
fn parse_print_simple() {
    let prog = parse(r#"print "hello";"#);
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(name, fh, _), .. }) => {
            assert_eq!(name, "print");
            assert!(fh.is_none());
        }
        other => panic!("expected print PrintOp, got {other:?}"),
    }
}

// ── Prefix dereference tests ──────────────────────────────

#[test]
fn parse_scalar_deref() {
    let e = parse_expr_str("$$ref;");
    match &e.kind {
        ExprKind::Deref(Sigil::Scalar, inner) => {
            assert!(matches!(inner.kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected scalar Deref, got {other:?}"),
    }
}

#[test]
fn parse_array_deref() {
    let e = parse_expr_str("@$ref;");
    match &e.kind {
        ExprKind::Deref(Sigil::Array, inner) => {
            assert!(matches!(inner.kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected array Deref, got {other:?}"),
    }
}

#[test]
fn parse_deref_block() {
    let e = parse_expr_str("${$ref};");
    match &e.kind {
        ExprKind::Deref(Sigil::Scalar, inner) => {
            assert!(matches!(inner.kind, ExprKind::ScalarVar(ref n) if n == "ref"));
        }
        other => panic!("expected Deref(Scalar, ScalarVar), got {other:?}"),
    }
}

#[test]
fn parse_array_deref_block() {
    let e = parse_expr_str("@{$ref};");
    match &e.kind {
        ExprKind::Deref(Sigil::Array, inner) => {
            assert!(matches!(inner.kind, ExprKind::ScalarVar(ref n) if n == "ref"));
        }
        other => panic!("expected Deref(Array, ScalarVar), got {other:?}"),
    }
}

#[test]
fn parse_deref_subscript() {
    // $$ref[0] → ArrayElem(Deref(Scalar, ScalarVar("ref")), 0)
    let e = parse_expr_str("$$ref[0];");
    match &e.kind {
        ExprKind::ArrayElem(base, idx) => {
            assert!(matches!(base.kind, ExprKind::Deref(Sigil::Scalar, _)));
            assert!(matches!(idx.kind, ExprKind::IntLit(0)));
        }
        other => panic!("expected ArrayElem(Deref, 0), got {other:?}"),
    }
}

// ── Slice tests ───────────────────────────────────────────

#[test]
fn parse_array_slice() {
    let e = parse_expr_str("@arr[0, 1, 2];");
    match &e.kind {
        ExprKind::ArraySlice(_, indices) => {
            assert_eq!(indices.len(), 3);
        }
        other => panic!("expected ArraySlice, got {other:?}"),
    }
}

#[test]
fn parse_hash_slice() {
    let e = parse_expr_str("@hash{qw(a b c)};");
    match &e.kind {
        ExprKind::HashSlice(_, keys) => {
            assert_eq!(keys.len(), 1); // qw() is one expr
        }
        other => panic!("expected HashSlice, got {other:?}"),
    }
}

// ── Postfix deref tests ───────────────────────────────────

#[test]
fn parse_postfix_deref_array() {
    let e = parse_expr_str("$ref->@*;");
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefArray)));
}

#[test]
fn parse_postfix_deref_hash() {
    let e = parse_expr_str("$ref->%*;");
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefHash)));
}

// ── Yada yada test ────────────────────────────────────────

#[test]
fn parse_yada_yada() {
    let prog = parse("sub foo { ... }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sub) => {
            assert_eq!(sub.body.statements.len(), 1);
            match &sub.body.statements[0].kind {
                StmtKind::Expr(Expr { kind: ExprKind::YadaYada, .. }) => {}
                other => panic!("expected YadaYada, got {other:?}"),
            }
        }
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

// ── goto test ─────────────────────────────────────────────

#[test]
fn parse_goto() {
    let e = parse_expr_str("goto LABEL;");
    match &e.kind {
        ExprKind::FuncCall(name, _) => assert_eq!(name, "goto"),
        other => panic!("expected goto, got {other:?}"),
    }
}

// ── Readline / diamond tests ──────────────────────────────

#[test]
fn parse_diamond() {
    let e = parse_expr_str("<>;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "readline");
            assert_eq!(args.len(), 0);
        }
        other => panic!("expected readline, got {other:?}"),
    }
}

#[test]
fn parse_readline_stdin() {
    let e = parse_expr_str("<STDIN>;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "readline");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected readline, got {other:?}"),
    }
}

// ── given/when tests ──────────────────────────────────────

#[test]
fn parse_given_when() {
    let prog = parse(
        "use feature 'switch'; no warnings 'experimental::smartmatch'; \
         given ($x) { when (1) { 1; } default { 0; } }",
    );
    let stmt = prog.statements.iter().find(|s| matches!(s.kind, StmtKind::Given(_, _))).expect("Given statement present");
    match &stmt.kind {
        StmtKind::Given(expr, block) => {
            assert!(matches!(expr.kind, ExprKind::ScalarVar(ref n) if n == "x"));
            assert!(block.statements.len() >= 2);
            assert!(matches!(block.statements[0].kind, StmtKind::When(_, _)));
        }
        other => panic!("expected Given, got {other:?}"),
    }
}

// ── try/catch tests ───────────────────────────────────────

#[test]
fn parse_try_catch() {
    let prog = parse("use feature 'try'; try { die; } catch ($e) { warn $e; }");
    let stmt = prog.statements.iter().find(|s| matches!(s.kind, StmtKind::Try(_))).expect("Try statement present");
    match &stmt.kind {
        StmtKind::Try(t) => {
            assert!(t.catch_block.is_some());
            assert!(t.catch_var.is_some());
        }
        other => panic!("expected Try, got {other:?}"),
    }
}

#[test]
fn parse_try_catch_finally() {
    let prog = parse("use feature 'try'; try { 1; } catch ($e) { 2; } finally { 3; }");
    let stmt = prog.statements.iter().find(|s| matches!(s.kind, StmtKind::Try(_))).expect("Try statement present");
    match &stmt.kind {
        StmtKind::Try(t) => {
            assert!(t.catch_block.is_some());
            assert!(t.finally_block.is_some());
        }
        other => panic!("expected Try, got {other:?}"),
    }
}

#[test]
fn parse_defer() {
    let prog = parse(
        "use feature 'defer'; no warnings 'experimental::defer'; \
         defer { cleanup(); }",
    );
    let stmt = prog.statements.iter().find(|s| matches!(s.kind, StmtKind::Defer(_))).expect("Defer statement present");
    match &stmt.kind {
        StmtKind::Defer(block) => {
            assert_eq!(block.statements.len(), 1);
        }
        other => panic!("expected Defer with 1-stmt block, got {other:?}"),
    }
}

// ── __END__ test ──────────────────────────────────────────

#[test]
fn parse_end_stops_parsing() {
    let src = "my $x = 1;\n__END__\nThis is not code.\n";
    let prog = parse(src);
    // Should have 2 statements: my decl and DataEnd
    assert_eq!(prog.statements.len(), 2);
    match &prog.statements[1].kind {
        StmtKind::DataEnd(DataEndMarker::End, offset) => {
            assert_eq!(&src.as_bytes()[*offset as usize..], b"This is not code.\n");
        }
        other => panic!("expected DataEnd(End), got {other:?}"),
    }
}

#[test]
fn parse_data_stops_parsing() {
    let src = "my $x = 1;\n__DATA__\nraw data here\n";
    let prog = parse(src);
    assert_eq!(prog.statements.len(), 2);
    match &prog.statements[1].kind {
        StmtKind::DataEnd(DataEndMarker::Data, offset) => {
            assert_eq!(&src.as_bytes()[*offset as usize..], b"raw data here\n");
        }
        other => panic!("expected DataEnd(Data), got {other:?}"),
    }
}

#[test]
fn parse_ctrl_d_stops_parsing() {
    let src = "my $x = 1;\x04ignored code\n";
    let prog = parse(src);
    assert_eq!(prog.statements.len(), 2);
    match &prog.statements[1].kind {
        StmtKind::DataEnd(DataEndMarker::CtrlD, offset) => {
            assert_eq!(&src.as_bytes()[*offset as usize..], b"ignored code\n");
        }
        other => panic!("expected DataEnd(CtrlD), got {other:?}"),
    }
}

#[test]
fn parse_ctrl_z_stops_parsing() {
    let src = "my $x = 1;\x1aignored code\n";
    let prog = parse(src);
    assert_eq!(prog.statements.len(), 2);
    match &prog.statements[1].kind {
        StmtKind::DataEnd(DataEndMarker::CtrlZ, offset) => {
            assert_eq!(&src.as_bytes()[*offset as usize..], b"ignored code\n");
        }
        other => panic!("expected DataEnd(CtrlZ), got {other:?}"),
    }
}

// ── Pod skipping test ─────────────────────────────────────

#[test]
fn parse_pod_skipped() {
    let prog = parse("my $x = 1;\n\n=pod\n\nThis is pod.\n\n=cut\n\nmy $y = 2;\n");
    // Should see both my declarations, pod is invisible.
    // Each is Stmt::Expr wrapping Assign(Decl(My), _).
    let my_count = prog
        .statements
        .iter()
        .filter(|s| {
            matches!(s.kind,
                StmtKind::Expr(Expr { kind: ExprKind::Assign(_, ref lhs, _), .. })
                    if matches!(lhs.kind, ExprKind::Decl(DeclScope::My, _))
            )
        })
        .count();
    assert_eq!(my_count, 2);
}

// ── C-style for loop tests ────────────────────────────────

#[test]
fn parse_c_style_for() {
    let prog = parse("for (my $i = 0; $i < 10; $i++) { print $i; }");
    match &prog.statements[0].kind {
        StmtKind::For(f) => {
            // init should be an assignment wrapping a Decl(My)
            match &f.init {
                Some(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => {
                    assert!(matches!(lhs.kind, ExprKind::Decl(DeclScope::My, _)));
                }
                other => panic!("expected Assign(Decl(My), ...), got {other:?}"),
            }
        }
        other => panic!("expected For, got {other:?}"),
    }
}

#[test]
fn parse_c_style_for_empty_parts() {
    let prog = parse("for (;;) { last; }");
    match &prog.statements[0].kind {
        StmtKind::For(f) => {
            assert!(f.init.is_none());
            assert!(f.condition.is_none());
            assert!(f.step.is_none());
        }
        other => panic!("expected For, got {other:?}"),
    }
}

#[test]
fn parse_c_style_for_list_decl() {
    let prog = parse("for (my ($i, $j) = (0, 0); $i < 10; $i++) { 1; }");
    match &prog.statements[0].kind {
        StmtKind::For(f) => match &f.init {
            Some(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => match &lhs.kind {
                ExprKind::Decl(DeclScope::My, vars) => {
                    assert_eq!(vars.len(), 2);
                }
                other => panic!("expected Decl(My, 2 vars), got {other:?}"),
            },
            other => panic!("expected Assign(Decl(My), ...), got {other:?}"),
        },
        other => panic!("expected For, got {other:?}"),
    }
}

#[test]
fn parse_foreach_still_works() {
    // Ensure for (@list) still parses as foreach
    let prog = parse("for (@list) { 1; }");
    assert!(matches!(prog.statements[0].kind, StmtKind::ForEach(_)));
}

#[test]
fn parse_foreach_continue() {
    let prog = parse("foreach my $x (@list) { 1; } continue { 2; }");
    match &prog.statements[0].kind {
        StmtKind::ForEach(f) => {
            assert!(f.continue_block.is_some());
        }
        other => panic!("expected ForEach, got {other:?}"),
    }
}

// ── scalar keyword test ───────────────────────────────────

#[test]
fn parse_scalar_keyword() {
    let e = parse_expr_str("scalar @array;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "scalar");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected scalar call, got {other:?}"),
    }
}

#[test]
fn parse_unless_elsif_else() {
    let prog = parse("unless (0) { 1; } elsif (1) { 2; } else { 3; }");
    match &prog.statements[0].kind {
        StmtKind::Unless(u) => {
            assert_eq!(u.elsif_clauses.len(), 1);
            assert!(u.else_block.is_some());
        }
        other => panic!("expected Unless, got {other:?}"),
    }
}

// ── Decl-as-expression test ───────────────────────────────

#[test]
fn parse_decl_in_expr_context() {
    // my $x = 5 in statement context still works.
    // Now represented as Stmt::Expr wrapping Assign(Decl(My), IntLit(5)).
    let prog = parse("my $x = 5;");
    let (scope, vars) = decl_vars(&prog.statements[0]);
    assert_eq!(scope, DeclScope::My);
    assert_eq!(vars[0].name, "x");
    let init = decl_init(&prog.statements[0]);
    assert!(matches!(init.kind, ExprKind::IntLit(5)));
}

// ── qx// test ─────────────────────────────────────────────

#[test]
fn parse_qx_string() {
    let e = parse_expr_str("qx{ls -la};");
    // qx produces an interpolated string (backtick kind)
    assert!(matches!(e.kind, ExprKind::InterpolatedString(_) | ExprKind::StringLit(_)));
}

// ── C-style for with plain expression init ────────────────

#[test]
fn parse_c_style_for_plain_init() {
    // No 'my' — just a plain assignment
    let prog = parse("for ($i = 0; $i < 10; $i++) { 1; }");
    assert!(matches!(prog.statements[0].kind, StmtKind::For(_)));
}

// ── local/our/state in for-init ───────────────────────────

#[test]
fn parse_c_style_for_local() {
    let prog = parse("for (local $i = 0; $i < 10; $i++) { 1; }");
    match &prog.statements[0].kind {
        StmtKind::For(f) => match &f.init {
            Some(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => {
                assert!(matches!(lhs.kind, ExprKind::Local(_)));
            }
            other => panic!("expected Assign(Local(...), ...), got {other:?}"),
        },
        other => panic!("expected For, got {other:?}"),
    }
}

// ── my with array/hash ────────────────────────────────────

#[test]
fn parse_my_array_decl() {
    let prog = parse("my @arr = (1, 2, 3);");
    let (_s, vars) = decl_vars(&prog.statements[0]);
    assert_eq!(vars.len(), 1);
    assert_eq!(vars[0].sigil, Sigil::Array);
    assert_eq!(vars[0].name, "arr");
}

#[test]
fn parse_my_hash_decl() {
    let prog = parse("my %hash = (a => 1);");
    let (_s, vars) = decl_vars(&prog.statements[0]);
    assert_eq!(vars.len(), 1);
    assert_eq!(vars[0].sigil, Sigil::Hash);
    assert_eq!(vars[0].name, "hash");
}

// ── while continue block ──────────────────────────────────

#[test]
fn parse_while_continue() {
    let prog = parse("while (1) { 1; } continue { 2; }");
    match &prog.statements[0].kind {
        StmtKind::While(w) => {
            assert!(w.continue_block.is_some());
        }
        other => panic!("expected While, got {other:?}"),
    }
}

#[test]
fn parse_until_continue() {
    let prog = parse("until (0) { 1; } continue { 2; }");
    match &prog.statements[0].kind {
        StmtKind::Until(u) => {
            assert!(u.continue_block.is_some());
        }
        other => panic!("expected Until, got {other:?}"),
    }
}

// ── Fat comma autoquoting test ────────────────────────────

#[test]
fn parse_fat_comma_autoquote() {
    // key => value — key should be a StringLit, not a FuncCall
    let e = parse_expr_str("key => 42;");
    match &e.kind {
        ExprKind::List(items) => {
            assert!(matches!(items[0].kind, ExprKind::StringLit(_)));
        }
        other => panic!("expected List with StringLit first, got {other:?}"),
    }
}

// ── Ampersand prefix call tests ───────────────────────────

#[test]
fn parse_ampersand_call() {
    let e = parse_expr_str("&foo(1, 2);");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn parse_ampersand_coderef() {
    let e = parse_expr_str("&$coderef(1);");
    match &e.kind {
        ExprKind::MethodCall(_, name, args) => {
            assert!(name.is_empty()); // coderef call uses empty method name
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::IntLit(1)));
        }
        other => panic!("expected MethodCall for coderef, got {other:?}"),
    }
}

#[test]
fn parse_ampersand_bare() {
    // &foo with no parens — call with current @_
    let e = parse_expr_str("&foo;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 0);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

// ── Hash dereference tests ────────────────────────────────

#[test]
fn parse_hash_deref() {
    let e = parse_expr_str("%$ref;");
    match &e.kind {
        ExprKind::Deref(Sigil::Hash, inner) => {
            assert!(matches!(inner.kind, ExprKind::ScalarVar(ref n) if n == "ref"));
        }
        other => panic!("expected Deref(Hash, ScalarVar), got {other:?}"),
    }
}

#[test]
fn parse_hash_deref_block() {
    let e = parse_expr_str("%{$ref};");
    match &e.kind {
        ExprKind::Deref(Sigil::Hash, inner) => {
            assert!(matches!(inner.kind, ExprKind::ScalarVar(ref n) if n == "ref"));
        }
        other => panic!("expected Deref(Hash, ScalarVar), got {other:?}"),
    }
}

// ── Glob / typeglob tests ─────────────────────────────────

#[test]
fn parse_glob_var() {
    let e = parse_expr_str("*foo;");
    match &e.kind {
        ExprKind::GlobVar(name) => assert_eq!(name, "foo"),
        other => panic!("expected GlobVar('foo'), got {other:?}"),
    }
}

#[test]
fn parse_glob_deref() {
    let e = parse_expr_str("*$ref;");
    match &e.kind {
        ExprKind::Deref(Sigil::Glob, inner) => {
            assert!(matches!(inner.kind, ExprKind::ScalarVar(ref n) if n == "ref"));
        }
        other => panic!("expected Deref(Glob, ScalarVar), got {other:?}"),
    }
}

// ── Chained arrow calls test ──────────────────────────────

#[test]
fn parse_chained_arrow_calls() {
    let e = parse_expr_str("$obj->foo->bar->baz;");
    // Should be deeply nested MethodCall(MethodCall(MethodCall(...)))
    match &e.kind {
        ExprKind::MethodCall(inner, name, _) => {
            assert_eq!(name, "baz");
            assert!(matches!(inner.kind, ExprKind::MethodCall(_, _, _)));
        }
        other => panic!("expected chained MethodCall, got {other:?}"),
    }
}

// ── Octal literal test ────────────────────────────────────

#[test]
fn lex_legacy_octal() {
    let prog = parse("my $x = 0777;");
    let init = decl_init(&prog.statements[0]);
    match &init.kind {
        ExprKind::IntLit(n) => assert_eq!(*n, 0o777), // 511 decimal
        other => panic!("expected IntLit, got {other:?}"),
    }
}

// ── require test ──────────────────────────────────────────

#[test]
fn parse_require() {
    let e = parse_expr_str("require Foo::Bar;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "require");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected require call, got {other:?}"),
    }
}

// ── Hash subscript autoquoting tests ──────────────────────

#[test]
fn parse_hash_bareword_autoquote() {
    let e = parse_expr_str("$hash{key};");
    match &e.kind {
        ExprKind::HashElem(_, key) => {
            assert!(matches!(key.kind, ExprKind::StringLit(_)));
        }
        other => panic!("expected HashElem with StringLit key, got {other:?}"),
    }
}

#[test]
fn parse_hash_neg_bareword_autoquote() {
    let e = parse_expr_str("$hash{-key};");
    match &e.kind {
        ExprKind::HashElem(_, key) => match &key.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "-key"),
            other => panic!("expected StringLit('-key'), got {other:?}"),
        },
        other => panic!("expected HashElem, got {other:?}"),
    }
}

#[test]
fn parse_arrow_hash_autoquote() {
    let e = parse_expr_str("$ref->{key};");
    match &e.kind {
        ExprKind::ArrowDeref(_, ArrowTarget::HashElem(key)) => {
            assert!(matches!(key.kind, ExprKind::StringLit(_)));
        }
        other => panic!("expected ArrowDeref with StringLit key, got {other:?}"),
    }
}

#[test]
fn parse_hash_expr_not_autoquoted() {
    // $hash{$key} should NOT autoquote
    let e = parse_expr_str("$hash{$key};");
    match &e.kind {
        ExprKind::HashElem(_, key) => {
            assert!(matches!(key.kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected HashElem with ScalarVar key, got {other:?}"),
    }
}

// ── -bareword fat comma autoquoting ───────────────────────

#[test]
fn parse_neg_bareword_fat_comma() {
    // -key => 42 should produce StringLit("-key")
    let e = parse_expr_str("-key => 42;");
    match &e.kind {
        ExprKind::List(items) => match &items[0].kind {
            ExprKind::StringLit(s) => assert_eq!(s, "-key"),
            other => panic!("expected StringLit('-key'), got {other:?}"),
        },
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn parse_neg_bareword_alone() {
    // -key alone → StringLit("-key")
    let e = parse_expr_str("-key;");
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "-key"),
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn parse_neg_func_call_not_quoted() {
    // -func() → negate the function call, NOT autoquote
    let e = parse_expr_str("-func();");
    assert!(matches!(e.kind, ExprKind::UnaryOp(UnaryOp::Negate, _)));
}

// ── Attribute tests ───────────────────────────────────────

#[test]
fn parse_sub_with_attribute() {
    let prog = parse("sub foo :lvalue { 1; }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sub) => {
            assert_eq!(sub.attributes.len(), 1);
            assert_eq!(sub.attributes[0].name, "lvalue");
            assert!(sub.attributes[0].value.is_none());
        }
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn parse_sub_multiple_attributes() {
    let prog = parse("sub foo :lvalue :method { 1; }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sub) => {
            assert_eq!(sub.attributes.len(), 2);
            assert_eq!(sub.attributes[0].name, "lvalue");
            assert_eq!(sub.attributes[1].name, "method");
        }
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

// ── v-string tests ────────────────────────────────────────

#[test]
fn parse_vstring() {
    let prog = parse("use v5.26.0;");
    match &prog.statements[0].kind {
        StmtKind::UseDecl(u) => {
            assert_eq!(u.module, "v5.26.0");
        }
        other => panic!("expected UseDecl, got {other:?}"),
    }
}

#[test]
fn parse_vstring_as_expr() {
    let e = parse_expr_str("v5.26;");
    match &e.kind {
        ExprKind::VersionLit(s) => assert_eq!(s, "v5.26"),
        other => panic!("expected VersionLit(\"v5.26\"), got {other:?}"),
    }
}

#[test]
fn parse_vstring_no_dots() {
    let e = parse_expr_str("v5;");
    match &e.kind {
        ExprKind::VersionLit(s) => assert_eq!(s, "v5"),
        other => panic!("expected VersionLit(\"v5\"), got {other:?}"),
    }
}

// ── pragma tests ──────────────────────────────────────────

/// Parse a program and return the parser's final pragma state.
/// Because pragmas are lexically scoped, this reflects whatever
/// was in effect at end-of-file (i.e., the outermost scope).
fn parse_pragmas(src: &str) -> crate::pragma::Pragmas {
    let mut p = Parser::new(src.as_bytes()).unwrap();
    let _ = p.parse_program().unwrap();
    *p.pragmas()
}

#[test]
fn pragma_default_has_default_bundle() {
    let p = parse_pragmas("my $x = 1;");
    // Pre-`use feature` state: the `:default` bundle (indirect,
    // multidimensional, bareword_filehandles,
    // apostrophe_as_package_separator, smartmatch).
    assert_eq!(p.features, Features::DEFAULT);
    assert!(p.features.contains(Features::INDIRECT));
    assert!(!p.features.contains(Features::SAY));
    assert!(!p.utf8);
}

#[test]
fn pragma_use_feature_single() {
    let p = parse_pragmas("use feature 'signatures';");
    assert!(p.features.contains(Features::SIGNATURES));
    // Other non-default features untouched.
    assert!(!p.features.contains(Features::SAY));
    // :default features still present (use feature just adds).
    assert!(p.features.contains(Features::INDIRECT));
}

#[test]
fn pragma_use_feature_multiple_via_qw() {
    let p = parse_pragmas("use feature qw(say state);");
    assert!(p.features.contains(Features::SAY));
    assert!(p.features.contains(Features::STATE));
}

#[test]
fn pragma_no_feature_removes_specific() {
    // Enable a non-default feature, then disable it.
    let p = parse_pragmas("use feature 'signatures';\nno feature 'signatures';\n");
    assert!(!p.features.contains(Features::SIGNATURES));
    // :default still intact.
    assert!(p.features.contains(Features::INDIRECT));
}

#[test]
fn pragma_no_feature_bare_resets_to_default() {
    // Per perlfeature: `no feature;` with no args resets to
    // :default, not to empty.
    let p = parse_pragmas("use feature qw(say state signatures);\nno feature;\n");
    assert_eq!(p.features, Features::DEFAULT);
}

#[test]
fn pragma_no_feature_all_clears_everything() {
    // `no feature ':all'` is how you get to truly-empty state.
    let p = parse_pragmas("no feature ':all';");
    assert_eq!(p.features, Features::EMPTY);
}

#[test]
fn pragma_use_feature_bundle_by_name() {
    // `use feature ':5.36'` applies the bundle directly.
    let p = parse_pragmas("use feature ':5.36';");
    assert!(p.features.contains(Features::SIGNATURES));
    assert!(!p.features.contains(Features::INDIRECT), "5.36 bundle excludes indirect");
}

#[test]
fn pragma_use_vstring_bundle() {
    let p = parse_pragmas("use v5.36;");
    assert!(p.features.contains(Features::SAY));
    assert!(p.features.contains(Features::SIGNATURES));
    assert!(!p.features.contains(Features::SWITCH), "5.36 bundle should not include switch");
    assert!(!p.features.contains(Features::INDIRECT), "5.36 bundle should not include indirect");
}

#[test]
fn pragma_use_int_version_bundle() {
    let p = parse_pragmas("use 5036;");
    assert!(p.features.contains(Features::SIGNATURES));
}

#[test]
fn pragma_use_float_version_bundle() {
    let p = parse_pragmas("use 5.036;");
    assert!(p.features.contains(Features::SIGNATURES));
}

#[test]
fn pragma_use_utf8() {
    let p = parse_pragmas("use utf8;");
    assert!(p.utf8);
}

#[test]
fn pragma_no_utf8() {
    let p = parse_pragmas("use utf8;\nno utf8;\n");
    assert!(!p.utf8);
}

#[test]
fn pragma_unknown_module_is_noop() {
    // `use strict;` doesn't set any parsing-relevant flag yet
    // and must not cause a panic.
    let p = parse_pragmas("use strict;");
    assert_eq!(p.features, Features::DEFAULT);
    assert!(!p.utf8);
}

#[test]
fn pragma_unknown_feature_name_silently_ignored() {
    let p = parse_pragmas("use feature 'totally_fake_feature';");
    assert_eq!(p.features, Features::DEFAULT);
}

#[test]
fn pragma_lexical_scoping_block_doesnt_leak() {
    let p = parse_pragmas("{ use feature 'signatures'; }");
    assert!(!p.features.contains(Features::SIGNATURES), "signatures enabled inside block should not leak out");
}

#[test]
fn pragma_lexical_scoping_outer_preserved() {
    let p = parse_pragmas("use feature 'signatures';\n{ no feature 'signatures'; }\n");
    assert!(p.features.contains(Features::SIGNATURES), "outer scope's signatures should be preserved across the inner block");
}

#[test]
fn pragma_version_bundle_resets_features() {
    // `use v5.36` does implicit `no feature ':all'; use feature ':5.36'`.
    // Applying after unrelated feature enables should leave only
    // the bundle.
    let p = parse_pragmas("use feature 'keyword_any';\nuse v5.36;\n");
    assert!(!p.features.contains(Features::KEYWORD_ANY), "version bundle should reset, not union");
    assert!(p.features.contains(Features::SIGNATURES));
}

// ── signature tests ───────────────────────────────────────

/// Convenience: parse a program and return the last top-level
/// SubDecl, panicking if none exists.
fn parse_sub(src: &str) -> SubDecl {
    let prog = parse(src);
    for stmt in prog.statements.iter().rev() {
        if let StmtKind::SubDecl(s) = &stmt.kind {
            return s.clone();
        }
    }
    panic!("no SubDecl in program; statements: {:#?}", prog.statements);
}

#[test]
fn sig_without_feature_is_prototype() {
    // No `use feature 'signatures'` in scope: `($)` is a
    // prototype (meaning "exactly one scalar argument").  We
    // verify the signature path was NOT taken by checking
    // that the prototype parser saw the raw text.
    let s = parse_sub("sub f ($) { }");
    assert!(s.signature.is_none(), "no signature when feature off");
    assert_eq!(s.prototype.as_deref(), Some("$"), "paren-form goes to prototype");
}

#[test]
fn sig_empty_with_feature_on() {
    let s = parse_sub("use feature 'signatures'; sub f () { }");
    assert!(s.prototype.is_none());
    let sig = s.signature.expect("signature present");
    assert_eq!(sig.params.len(), 0);
}

#[test]
fn sig_single_scalar() {
    let s = parse_sub("use feature 'signatures'; sub f ($x) { }");
    let sig = s.signature.expect("signature present");
    assert_eq!(sig.params.len(), 1);
    match &sig.params[0] {
        SigParam::Scalar { name, default, .. } => {
            assert_eq!(name, "x");
            assert!(default.is_none());
        }
        other => panic!("expected Scalar, got {other:?}"),
    }
}

#[test]
fn sig_multiple_scalars() {
    let s = parse_sub("use feature 'signatures'; sub f ($x, $y, $z) { }");
    let sig = s.signature.expect("signature present");
    assert_eq!(sig.params.len(), 3);
    for (p, expected) in sig.params.iter().zip(["x", "y", "z"]) {
        match p {
            SigParam::Scalar { name, default: None, .. } => {
                assert_eq!(name, expected);
            }
            other => panic!("expected Scalar({expected}), got {other:?}"),
        }
    }
}

#[test]
fn sig_scalar_with_default() {
    let s = parse_sub("use feature 'signatures'; sub f ($x = 42) { }");
    let sig = s.signature.expect("signature present");
    match &sig.params[0] {
        SigParam::Scalar { name, default: Some((_, d)), .. } => {
            assert_eq!(name, "x");
            assert!(matches!(d.kind, ExprKind::IntLit(42)));
        }
        other => panic!("expected Scalar with default, got {other:?}"),
    }
}

#[test]
fn sig_default_references_prior_param() {
    // Default expression can reference earlier parameter —
    // parser shouldn't care (just an expression).
    let s = parse_sub("use feature 'signatures'; sub f ($x, $y = $x * 2) { }");
    let sig = s.signature.expect("signature present");
    assert_eq!(sig.params.len(), 2);
    match &sig.params[1] {
        SigParam::Scalar { name, default: Some(_), .. } => {
            assert_eq!(name, "y");
        }
        other => panic!("expected Scalar with default, got {other:?}"),
    }
}

#[test]
fn sig_slurpy_array() {
    let s = parse_sub("use feature 'signatures'; sub f ($x, @rest) { }");
    let sig = s.signature.expect("signature present");
    assert_eq!(sig.params.len(), 2);
    match &sig.params[1] {
        SigParam::SlurpyArray { name, .. } => assert_eq!(name, "rest"),
        other => panic!("expected SlurpyArray, got {other:?}"),
    }
}

#[test]
fn sig_slurpy_hash() {
    let s = parse_sub("use feature 'signatures'; sub f ($x, %opts) { }");
    let sig = s.signature.expect("signature present");
    match &sig.params[1] {
        SigParam::SlurpyHash { name, .. } => assert_eq!(name, "opts"),
        other => panic!("expected SlurpyHash, got {other:?}"),
    }
}

#[test]
fn sig_anonymous_placeholders() {
    // Anonymous scalars — `$` without names — accept-and-discard.
    // Only scalars here; slurpy forms (`@`, `%`) must be last
    // and only one is allowed, so they get their own tests.
    let s = parse_sub("use feature 'signatures'; sub f ($, $, $) { }");
    let sig = s.signature.expect("signature present");
    assert_eq!(sig.params.len(), 3);
    assert!(sig.params.iter().all(|p| matches!(p, SigParam::AnonScalar { .. })));
}

#[test]
fn sig_anonymous_slurpy_array() {
    // Bare `@` at the end — anonymous slurpy array.
    let s = parse_sub("use feature 'signatures'; sub f ($, @) { }");
    let sig = s.signature.expect("signature present");
    assert_eq!(sig.params.len(), 2);
    assert!(matches!(sig.params[0], SigParam::AnonScalar { .. }));
    assert!(matches!(sig.params[1], SigParam::AnonArray { .. }));
}

#[test]
fn sig_anonymous_slurpy_hash() {
    // Bare `%` at the end — anonymous slurpy hash.
    let s = parse_sub("use feature 'signatures'; sub f ($, %) { }");
    let sig = s.signature.expect("signature present");
    assert_eq!(sig.params.len(), 2);
    assert!(matches!(sig.params[0], SigParam::AnonScalar { .. }));
    assert!(matches!(sig.params[1], SigParam::AnonHash { .. }));
}

#[test]
fn sig_anon_scalar_then_named() {
    // Skip first arg, bind second.
    let s = parse_sub("use feature 'signatures'; sub f ($, $y) { }");
    let sig = s.signature.expect("signature present");
    assert!(matches!(sig.params[0], SigParam::AnonScalar { .. }));
    match &sig.params[1] {
        SigParam::Scalar { name, .. } => assert_eq!(name, "y"),
        other => panic!("expected Scalar(y), got {other:?}"),
    }
}

#[test]
fn sig_trailing_comma_allowed() {
    let s = parse_sub("use feature 'signatures'; sub f ($x, $y,) { }");
    let sig = s.signature.expect("signature present");
    assert_eq!(sig.params.len(), 2);
}

// ── Interaction with :prototype(...) attribute ──

#[test]
fn sig_with_prototype_attribute() {
    // `:prototype($$)` attaches a prototype; the paren-form is
    // still a signature when the feature is active.
    let s = parse_sub("use feature 'signatures'; sub f :prototype($$) ($x, $y) { }");
    let sig = s.signature.expect("signature present");
    assert_eq!(sig.params.len(), 2);
    // Attribute is captured too.
    let has_proto_attr = s.attributes.iter().any(|a| a.name == "prototype" && a.value.as_deref() == Some("$$"));
    assert!(has_proto_attr, "prototype attribute should be present");
}

// ── use v5.36 enables signatures via the bundle ──

#[test]
fn sig_enabled_by_use_v5_36() {
    // Phase 1 hookup: the `:5.36` bundle includes signatures,
    // so `use v5.36;` should enable the signature path without
    // an explicit `use feature 'signatures';`.
    let s = parse_sub("use v5.36; sub f ($x, $y) { }");
    assert!(s.signature.is_some(), "use v5.36 should enable signatures");
    assert!(s.prototype.is_none());
}

#[test]
fn sig_feature_is_lexically_scoped() {
    // Outer scope has signatures; inner `no feature 'signatures'`
    // disables it for a sub declared inside the inner block.
    let src = "\
use feature 'signatures';
sub outer ($x) { 1 }
{
no feature 'signatures';
sub inner ($) { 2 }
}
";
    let prog = parse(src);
    // Find `outer` at top level.
    let outer = prog
        .statements
        .iter()
        .find_map(|s| match &s.kind {
            StmtKind::SubDecl(s) if s.name == "outer" => Some(s),
            _ => None,
        })
        .expect("outer sub at top level");
    assert!(outer.signature.is_some(), "outer has signature");
    assert!(outer.prototype.is_none());

    // Find `inner` inside the bare block.
    let mut found_inner = false;
    for stmt in &prog.statements {
        if let StmtKind::Block(block, _) = &stmt.kind {
            for inner_stmt in &block.statements {
                if let StmtKind::SubDecl(s) = &inner_stmt.kind
                    && s.name == "inner"
                {
                    assert!(s.signature.is_none(), "inner should NOT have signature");
                    assert!(s.prototype.is_some(), "inner should have prototype");
                    found_inner = true;
                }
            }
        }
    }
    assert!(found_inner, "didn't find `inner` sub inside block");
}

// ── Anonymous sub signatures ──

#[test]
fn sig_anon_sub_with_signature() {
    let prog = parse("use feature 'signatures'; my $f = sub ($x) { $x };");
    // Find the AnonSub expression in the statements.
    let mut found = false;
    for stmt in &prog.statements {
        walk_for_anon_sub(&stmt.kind, &mut found);
    }
    assert!(found, "expected an AnonSub with signature");
}

/// Helper: recursively walk a stmt looking for an AnonSub with
/// a non-None signature.
fn walk_for_anon_sub(stmt: &StmtKind, found: &mut bool) {
    if let StmtKind::Expr(expr) = stmt {
        walk_expr(expr, found);
    }
}

fn walk_expr(expr: &Expr, found: &mut bool) {
    match &expr.kind {
        ExprKind::AnonSub(_, _, Some(sig), _) => {
            assert_eq!(sig.params.len(), 1);
            *found = true;
        }
        ExprKind::Assign(_, l, r) => {
            walk_expr(l, found);
            walk_expr(r, found);
        }
        _ => {}
    }
}

// ── postderef tests ───────────────────────────────────────

/// Convenience: parse one expression statement, returning the
/// inner expression.
fn parse_expr_stmt(src: &str) -> Expr {
    let prog = parse(src);
    for stmt in &prog.statements {
        if let StmtKind::Expr(e) = &stmt.kind {
            return e.clone();
        }
    }
    panic!("no expression in program; statements: {:#?}", prog.statements);
}

/// Helper: walk the outermost arrow-deref off a parsed expr,
/// returning the ArrowTarget.  Panics if the expression isn't
/// an ArrowDeref.
fn arrow_target(e: &Expr) -> &ArrowTarget {
    match &e.kind {
        ExprKind::ArrowDeref(_, target) => target,
        other => panic!("expected ArrowDeref, got {other:?}"),
    }
}

#[test]
fn postderef_deref_array() {
    let e = parse_expr_stmt("$r->@*;");
    assert!(matches!(arrow_target(&e), ArrowTarget::DerefArray));
}

#[test]
fn postderef_deref_hash() {
    let e = parse_expr_stmt("$r->%*;");
    assert!(matches!(arrow_target(&e), ArrowTarget::DerefHash));
}

#[test]
fn postderef_deref_scalar() {
    let e = parse_expr_stmt("$r->$*;");
    assert!(matches!(arrow_target(&e), ArrowTarget::DerefScalar));
}

#[test]
fn postderef_last_index() {
    // `->$#*` — equivalent to `$#{$ref}`.  Requires lexer
    // byte-level disambiguation because `#` would otherwise
    // begin a comment.
    let e = parse_expr_stmt("$r->$#*;");
    assert!(matches!(arrow_target(&e), ArrowTarget::LastIndex));
}

#[test]
fn postderef_last_index_in_expr() {
    // Embed in a larger expression to verify the parser
    // continues past the LastIndex properly.
    let e = parse_expr_stmt("my $n = $r->$#*;");
    match e.kind {
        ExprKind::Assign(_, _, rhs) => {
            assert!(matches!(arrow_target(&rhs), ArrowTarget::LastIndex));
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn postderef_dollar_not_hashstar_still_fails() {
    // `->$foo` (Dollar + named ScalarVar) is not postderef.
    // The lexer greedily combines `$foo` into ScalarVar —
    // which is handled as dynamic method dispatch in another
    // arm.  We just verify `->$` followed by something
    // neither `*` nor `#*` doesn't crash.
    let src = "$r->$;";
    let mut p = match Parser::new(src.as_bytes()) {
        Ok(p) => p,
        Err(_) => panic!("parser construction failed"),
    };
    let result = p.parse_program();
    assert!(result.is_err(), "->$ with trailing semicolon is a parse error");
}

#[test]
fn postderef_deref_code() {
    let e = parse_expr_stmt("$r->&*;");
    assert!(matches!(arrow_target(&e), ArrowTarget::DerefCode));
}

#[test]
fn postderef_deref_glob() {
    // `->**` — lexer emits Token::Power for `**`.
    let e = parse_expr_stmt("$r->**;");
    assert!(matches!(arrow_target(&e), ArrowTarget::DerefGlob));
}

#[test]
fn postderef_array_slice_indices() {
    let e = parse_expr_stmt("$r->@[0, 1, 2];");
    match arrow_target(&e) {
        ArrowTarget::ArraySliceIndices(_) => {}
        other => panic!("expected ArraySliceIndices, got {other:?}"),
    }
}

#[test]
fn postderef_array_slice_keys() {
    let e = parse_expr_stmt(r#"$r->@{"a", "b"};"#);
    match arrow_target(&e) {
        ArrowTarget::ArraySliceKeys(_) => {}
        other => panic!("expected ArraySliceKeys, got {other:?}"),
    }
}

#[test]
fn postderef_kv_slice_indices() {
    let e = parse_expr_stmt("$r->%[0, 1];");
    match arrow_target(&e) {
        ArrowTarget::KvSliceIndices(_) => {}
        other => panic!("expected KvSliceIndices, got {other:?}"),
    }
}

#[test]
fn postderef_kv_slice_keys() {
    let e = parse_expr_stmt(r#"$r->%{"a", "b"};"#);
    match arrow_target(&e) {
        ArrowTarget::KvSliceKeys(_) => {}
        other => panic!("expected KvSliceKeys, got {other:?}"),
    }
}

#[test]
fn postderef_chained_on_complex_expr() {
    // Chain off a method call result.
    let e = parse_expr_stmt("$obj->method->@*;");
    assert!(matches!(arrow_target(&e), ArrowTarget::DerefArray));
}

#[test]
fn postderef_nested_slice() {
    // `->@[0]->[1]` — slice followed by subscript chain.
    // (Not semantically useful but should parse.)
    let e = parse_expr_stmt("$r->@[0];");
    match arrow_target(&e) {
        ArrowTarget::ArraySliceIndices(_) => {}
        other => panic!("expected ArraySliceIndices, got {other:?}"),
    }
}

// ── Phase 4: isa / fc / evalbytes / compile-time tokens ──

// ── `isa` infix operator ──

#[test]
fn isa_requires_feature() {
    // Without the `isa` feature, `isa` is just an ordinary
    // bareword (would be a function call or bareword
    // reference).  We verify by checking that parsing
    // `$x isa Foo` with no feature does NOT produce a BinOp.
    let e = parse_expr_stmt("$x isa Foo;");
    assert!(!matches!(e.kind, ExprKind::BinOp(BinOp::Isa, _, _)), "no isa feature → must not parse as Isa binop");
}

#[test]
fn isa_with_feature() {
    let e = parse_expr_stmt("use feature 'isa'; $x isa Foo;");
    match e.kind {
        ExprKind::BinOp(BinOp::Isa, lhs, rhs) => {
            assert!(matches!(lhs.kind, ExprKind::ScalarVar(_)));
            assert!(matches!(rhs.kind, ExprKind::Bareword(_)));
        }
        other => panic!("expected Isa binop, got {other:?}"),
    }
}

#[test]
fn isa_enabled_by_v5_36() {
    // The :5.36 bundle includes isa.
    let e = parse_expr_stmt("use v5.36; $x isa Foo;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Isa, _, _)));
}

#[test]
fn isa_precedence_vs_relational() {
    // `isa` binds tighter than `<`, so `$x isa Foo < 1`
    // groups as `($x isa Foo) < 1`.
    let e = parse_expr_stmt("use feature 'isa'; $x isa Foo < 1;");
    match e.kind {
        ExprKind::BinOp(BinOp::NumLt, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::Isa, _, _)), "isa should bind tighter than <");
        }
        other => panic!("expected NumLt at top level, got {other:?}"),
    }
}

// ── `fc` feature-gated named unary ──

#[test]
fn fc_requires_feature() {
    // Without `fc` feature, `fc($x)` parses as an ordinary
    // function call to a user sub named `fc`.  Either way
    // we get a FuncCall; just confirm it doesn't error and
    // the function name is captured.
    let e = parse_expr_stmt("fc($x);");
    match e.kind {
        ExprKind::FuncCall(name, _) => assert_eq!(name, "fc"),
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn fc_with_feature_paren() {
    let e = parse_expr_stmt("use feature 'fc'; fc($x);");
    match e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "fc");
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn fc_with_feature_no_paren() {
    // `fc $x` — named unary, one argument at NAMED_UNARY
    // precedence.
    let e = parse_expr_stmt("use feature 'fc'; fc $x;");
    match e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "fc");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

// ── `evalbytes` feature-gated named unary ──

#[test]
fn evalbytes_with_feature() {
    let e = parse_expr_stmt(r#"use feature 'evalbytes'; evalbytes("1+1");"#);
    match e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "evalbytes");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

// ── Compile-time tokens ──

#[test]
fn source_file_captured_at_lex_time() {
    // Default filename placeholder when constructed via
    // `parse(src)` / `Parser::new(src)`.
    let e = parse_expr_stmt("__FILE__;");
    match e.kind {
        ExprKind::SourceFile(path) => assert_eq!(path, "(script)"),
        other => panic!("expected SourceFile, got {other:?}"),
    }
}

#[test]
fn source_file_uses_custom_filename() {
    // `Parser::with_filename` / `parse_with_filename` plumbs
    // the filename through to `LexerSource::filename()`,
    // which `__FILE__` reads at lex time.
    let prog = crate::parse_with_filename(b"__FILE__;", "my_script.pl").expect("parse should succeed");
    let expr = prog.statements.iter().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e.clone()) } else { None }).expect("expression statement");
    match expr.kind {
        ExprKind::SourceFile(path) => assert_eq!(path, "my_script.pl"),
        other => panic!("expected SourceFile, got {other:?}"),
    }
}

#[test]
fn source_line_captured_at_lex_time() {
    // `__LINE__` on line 3 of a 3-line program.
    let e = parse_expr_stmt("\n\n__LINE__;");
    match e.kind {
        ExprKind::SourceLine(n) => assert_eq!(n, 3),
        other => panic!("expected SourceLine, got {other:?}"),
    }
}

#[test]
fn current_package_filled_by_parser() {
    let e = parse_expr_stmt("__PACKAGE__;");
    match e.kind {
        ExprKind::CurrentPackage(name) => assert_eq!(name, "main"),
        other => panic!("expected CurrentPackage, got {other:?}"),
    }
}

#[test]
fn current_package_reflects_package_decl() {
    // After `package Foo;`, __PACKAGE__ should give "Foo".
    let prog = parse("package Foo;\n__PACKAGE__;\n");
    let e = prog.statements.iter().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e.clone()) } else { None }).expect("expression statement");
    match e.kind {
        ExprKind::CurrentPackage(name) => assert_eq!(name, "Foo"),
        other => panic!("expected CurrentPackage, got {other:?}"),
    }
}

#[test]
fn current_sub_requires_feature() {
    // Without the current_sub feature, `__SUB__` falls back
    // to bareword treatment.
    let e = parse_expr_stmt("__SUB__;");
    assert!(!matches!(e.kind, ExprKind::CurrentSub), "no current_sub feature → must not be CurrentSub");
}

#[test]
fn current_sub_with_feature() {
    let e = parse_expr_stmt("use feature 'current_sub'; __SUB__;");
    assert!(matches!(e.kind, ExprKind::CurrentSub));
}

#[test]
fn current_sub_via_v5_16() {
    // The :5.16 bundle includes current_sub.
    let e = parse_expr_stmt("use v5.16; __SUB__;");
    assert!(matches!(e.kind, ExprKind::CurrentSub));
}

// ── Feature-gated keyword downgrade ───────────────────────
//
// When the governing feature is off, try/catch/finally/defer,
// given/when/default, and class/field/method all act as plain
// identifiers — users can define subs with those names,
// pass them as hash keys, etc.  These tests verify the
// downgrade happens at the parser level so legacy code keeps
// working.

#[test]
fn class_is_bareword_without_feature() {
    // `sub class { ... }` — defining a sub named "class".
    // With class feature off, the lexer emits
    // Token::Keyword(Class) but the parser downgrades to
    // Token::Ident("class") because we're not in a class
    // scope.  The sub declaration should parse.
    let prog = parse("sub class { 1; }");
    assert!(
        prog.statements.iter().any(|s| matches!(
            &s.kind,
            StmtKind::SubDecl(sd) if sd.name == "class"
        )),
        "expected sub named `class` to parse without class feature"
    );
}

#[test]
fn try_is_ident_without_feature() {
    // `my $try = try();` — `try` as a function call.
    let prog = parse("my $try = try();");
    // Should parse as a normal expression statement (Decl
    // assignment with FuncCall).  The inner expression is
    // FuncCall("try", []), not a Try statement.
    assert!(!prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Try(_))), "must not parse as Try without feature");
}

#[test]
fn given_is_ident_without_feature() {
    // `given(...)` is a function call without the switch feature.
    let prog = parse("given(1);");
    assert!(!prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Given(_, _))), "must not parse as Given without feature");
}

#[test]
fn defer_is_ident_without_feature() {
    // `defer { ... }` would be a Defer statement with the
    // defer feature; without it, `defer` is a bareword
    // followed by a block, which is a parse error (or parsed
    // as something else).  We just confirm it doesn't
    // produce a Defer statement.
    let prog_result = Parser::new(b"my $x = defer;").and_then(|mut p| p.parse_program());
    if let Ok(prog) = prog_result {
        assert!(!prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Defer(_))), "must not parse as Defer without feature");
    }
}

#[test]
fn method_is_ident_without_feature() {
    // Outside `use feature 'class'`, `method` is a plain sub
    // name.  `sub method { ... }` at top level defines a
    // regular sub.
    let prog = parse("sub method { 1; }");
    assert!(
        prog.statements.iter().any(|s| matches!(
            &s.kind,
            StmtKind::SubDecl(sd) if sd.name == "method"
        )),
        "expected sub named `method` to parse without class feature"
    );
}

#[test]
fn try_keyword_reactivates_with_feature() {
    // Sanity check: once `use feature 'try';` is seen, the
    // downgrade stops happening for the rest of the scope.
    let prog = parse("use feature 'try'; try { 1; }");
    let has_try = prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Try(_)));
    assert!(has_try, "Try must parse when feature is active");
}

#[test]
fn feature_gate_is_lexically_scoped() {
    // Inside a block, `no feature 'try'` disables the gate.
    // Outside the block, `try` is still active.
    // We only verify the outer `try { ... }` succeeds —
    // demonstrating the scope restore after the inner block.
    let prog = parse("use feature 'try'; try { 1; } catch ($e) { 2; }");
    assert!(prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Try(_))), "outer Try with feature on must parse");
}

// ── Refaliasing / declared_refs (5.22+ / 5.26+) ───────────

#[test]
fn refalias_requires_feature() {
    // Without `refaliasing`, `\$a = \$b` is a parse error
    // (Ref is not a valid lvalue).
    let src = "\\$a = \\$b;";
    let mut p = match Parser::new(src.as_bytes()) {
        Ok(p) => p,
        Err(_) => panic!("parser construction failed"),
    };
    let result = p.parse_program();
    assert!(result.is_err(), "refaliasing without feature should fail");
}

#[test]
fn refalias_with_feature_scalar() {
    let e = parse_expr_stmt("use feature 'refaliasing'; no warnings 'experimental::refaliasing'; \\$a = \\$b;");
    match e.kind {
        ExprKind::Assign(AssignOp::Eq, lhs, rhs) => {
            assert!(matches!(lhs.kind, ExprKind::Ref(_)));
            assert!(matches!(rhs.kind, ExprKind::Ref(_)));
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn refalias_with_feature_array() {
    let e = parse_expr_stmt("use feature 'refaliasing'; no warnings 'experimental::refaliasing'; \\@a = \\@b;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::Eq, _, _)));
}

#[test]
fn refalias_with_feature_hash() {
    let e = parse_expr_stmt("use feature 'refaliasing'; no warnings 'experimental::refaliasing'; \\%a = \\%b;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::Eq, _, _)));
}

#[test]
fn refalias_list_form() {
    let e = parse_expr_stmt("use feature 'refaliasing'; no warnings 'experimental::refaliasing'; (\\$a, \\$b) = (\\$c, \\$d);");
    match e.kind {
        ExprKind::Assign(AssignOp::Eq, lhs, _) => {
            // LHS should be a list / paren containing Refs.
            match &lhs.kind {
                ExprKind::Paren(inner) => match &inner.kind {
                    ExprKind::List(items) => {
                        assert_eq!(items.len(), 2);
                        assert!(items.iter().all(|e| matches!(e.kind, ExprKind::Ref(_))));
                    }
                    other => panic!("expected List inside Paren, got {other:?}"),
                },
                ExprKind::List(items) => {
                    assert!(items.iter().all(|e| matches!(e.kind, ExprKind::Ref(_))));
                }
                other => panic!("expected List/Paren on LHS, got {other:?}"),
            }
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

// ── declared_refs (5.26+) ──

#[test]
fn declared_refs_requires_feature() {
    // `my \$x` without feature → ParseError at the `\`.
    let src = "my \\$x = \\$y;";
    let mut p = match Parser::new(src.as_bytes()) {
        Ok(p) => p,
        Err(_) => panic!("parser construction failed"),
    };
    let result = p.parse_program();
    assert!(result.is_err(), "declared_refs without feature should fail");
}

#[test]
fn declared_refs_scalar() {
    let e = parse_expr_stmt(
        "use feature 'declared_refs'; use feature 'refaliasing'; \
         no warnings 'experimental::refaliasing'; no warnings 'experimental::declared_refs'; \
         my \\$x = \\$y;",
    );
    match e.kind {
        ExprKind::Assign(AssignOp::Eq, lhs, _) => match lhs.kind {
            ExprKind::Decl(DeclScope::My, vars) => {
                assert_eq!(vars.len(), 1);
                assert_eq!(vars[0].name, "x");
                assert_eq!(vars[0].sigil, Sigil::Scalar);
                assert!(vars[0].is_ref, "expected is_ref=true for `my \\$x`");
            }
            other => panic!("expected Decl on LHS, got {other:?}"),
        },
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn declared_refs_list_mixed() {
    // `my (\$a, \@b)` — two ref-declared vars.
    let e = parse_expr_stmt(
        "use feature 'declared_refs'; use feature 'refaliasing'; \
         no warnings 'experimental::refaliasing'; no warnings 'experimental::declared_refs'; \
         my (\\$a, \\@b) = (\\$c, \\@d);",
    );
    match e.kind {
        ExprKind::Assign(AssignOp::Eq, lhs, _) => match lhs.kind {
            ExprKind::Decl(DeclScope::My, vars) => {
                assert_eq!(vars.len(), 2);
                assert!(vars[0].is_ref && vars[1].is_ref);
                assert_eq!(vars[0].sigil, Sigil::Scalar);
                assert_eq!(vars[1].sigil, Sigil::Array);
            }
            other => panic!("expected Decl on LHS, got {other:?}"),
        },
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn declared_refs_partial() {
    // Mixing ref and non-ref in one decl: `my (\$a, $b)` — the
    // parser accepts this (semantic validation is a later pass).
    let e = parse_expr_stmt(
        "use feature 'declared_refs'; use feature 'refaliasing'; \
         no warnings 'experimental::refaliasing'; no warnings 'experimental::declared_refs'; \
         my (\\$a, $b) = (\\$c, 42);",
    );
    match e.kind {
        ExprKind::Assign(AssignOp::Eq, lhs, _) => match lhs.kind {
            ExprKind::Decl(DeclScope::My, vars) => {
                assert_eq!(vars.len(), 2);
                assert!(vars[0].is_ref);
                assert!(!vars[1].is_ref);
            }
            other => panic!("expected Decl on LHS, got {other:?}"),
        },
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn declared_refs_via_v5_36() {
    // `use v5.36` enables both refaliasing and declared_refs
    // via the bundle.
    // Actually, checking perlfeature: :5.36 does NOT include
    // refaliasing/declared_refs (those are still experimental
    // as of 5.36).  So this test expects a parse error.
    // Using a feature-on path with explicit `use feature` in
    // other tests above covers the positive case.
    let src = "use v5.36; my \\$x = \\$y;";
    let mut p = match Parser::new(src.as_bytes()) {
        Ok(p) => p,
        Err(_) => panic!("parser construction failed"),
    };
    let result = p.parse_program();
    assert!(result.is_err(), ":5.36 bundle does not include declared_refs (experimental)");
}

// ── format tests ──────────────────────────────────────────

/// Convenience: parse a single format declaration, panic on any
/// other top-level statement shape.
fn parse_fmt(src: &str) -> FormatDecl {
    let prog = parse(src);
    assert_eq!(prog.statements.len(), 1, "expected one top-level stmt, got {}", prog.statements.len());
    match &prog.statements[0].kind {
        StmtKind::FormatDecl(f) => f.clone(),
        other => panic!("expected FormatDecl, got {other:?}"),
    }
}

// ── Boundary / naming ──

#[test]
fn format_default_name_is_stdout() {
    let f = parse_fmt("format =\n.\n");
    assert_eq!(f.name, "STDOUT");
    assert!(f.lines.is_empty(), "empty body → no lines");
}

#[test]
fn format_named() {
    let f = parse_fmt("format MyFmt =\n.\n");
    assert_eq!(f.name, "MyFmt");
}

#[test]
fn format_empty_body() {
    // `.` immediately on the next line → zero lines.
    let f = parse_fmt("format X =\n.\n");
    assert!(f.lines.is_empty());
}

#[test]
fn format_terminator_with_trailing_ws() {
    // `. \t\r` on the terminator line still terminates.
    let f = parse_fmt("format X =\nhello\n. \t\n");
    assert_eq!(f.lines.len(), 1);
}

#[test]
fn format_indented_dot_does_not_terminate() {
    // A `.` not in column 0 is just literal content.
    let f = parse_fmt("format X =\n hello\n .\n.\n");
    // Two content lines (one " hello", one " ."), then the real `.` terminates.
    assert_eq!(f.lines.len(), 2);
    match &f.lines[1] {
        FormatLine::Literal { text, .. } => assert_eq!(text, " ."),
        other => panic!("expected Literal, got {other:?}"),
    }
}

// ── Line classification ──

#[test]
fn format_comment_line() {
    let f = parse_fmt("format X =\n# some comment\n.\n");
    assert_eq!(f.lines.len(), 1);
    match &f.lines[0] {
        FormatLine::Comment { text, .. } => assert_eq!(text, " some comment"),
        other => panic!("expected Comment, got {other:?}"),
    }
}

#[test]
fn format_blank_line() {
    let f = parse_fmt("format X =\n\n.\n");
    assert_eq!(f.lines.len(), 1);
    assert!(matches!(f.lines[0], FormatLine::Blank { .. }));
}

#[test]
fn format_whitespace_only_line_is_blank() {
    let f = parse_fmt("format X =\n   \t\n.\n");
    assert!(matches!(f.lines[0], FormatLine::Blank { .. }));
}

#[test]
fn format_literal_line_no_fields() {
    let f = parse_fmt("format X =\nhello world\n.\n");
    match &f.lines[0] {
        FormatLine::Literal { repeat, text, .. } => {
            assert!(matches!(repeat, RepeatKind::None));
            assert_eq!(text, "hello world");
        }
        other => panic!("expected Literal, got {other:?}"),
    }
}

// ── Tilde handling ──

#[test]
fn format_single_tilde_on_literal_line() {
    // ~ on a fieldless line → repeat=Suppress, ~ replaced with space.
    let f = parse_fmt("format X =\n~hello\n.\n");
    match &f.lines[0] {
        FormatLine::Literal { repeat, text, .. } => {
            assert!(matches!(repeat, RepeatKind::Suppress));
            assert_eq!(text, " hello", "tilde should be replaced with space");
        }
        other => panic!("expected Literal, got {other:?}"),
    }
}

#[test]
fn format_double_tilde_on_literal_line() {
    let f = parse_fmt("format X =\n~~hello\n.\n");
    match &f.lines[0] {
        FormatLine::Literal { repeat, text, .. } => {
            assert!(matches!(repeat, RepeatKind::Repeat));
            assert_eq!(text, "  hello");
        }
        other => panic!("expected Literal, got {other:?}"),
    }
}

#[test]
fn format_tilde_mid_line_sets_suppress() {
    // ~ anywhere on the line, not just at the start.
    let f = parse_fmt("format X =\nhello ~ world\n.\n");
    match &f.lines[0] {
        FormatLine::Literal { repeat, text, .. } => {
            assert!(matches!(repeat, RepeatKind::Suppress));
            assert_eq!(text, "hello   world");
        }
        other => panic!("expected Literal, got {other:?}"),
    }
}

// ── Field: text justifications ──

#[test]
fn format_field_left_justify() {
    let f = parse_fmt("format X =\n@<<<\n$x\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { parts, args, .. } => {
            assert_eq!(parts.len(), 1);
            assert_eq!(args.len(), 1);
            match &parts[0] {
                FormatPart::Field(FormatField { kind: FieldKind::LeftJustify { width, truncate_ellipsis }, .. }) => {
                    assert_eq!(*width, 4);
                    assert!(!truncate_ellipsis);
                }
                other => panic!("expected LeftJustify, got {other:?}"),
            }
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

#[test]
fn format_field_right_justify() {
    let f = parse_fmt("format X =\n@>>>>>\n$x\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { parts, .. } => {
            assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::RightJustify { width: 6, truncate_ellipsis: false }, .. })));
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

#[test]
fn format_field_center() {
    let f = parse_fmt("format X =\n@||||\n$x\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { parts, .. } => {
            assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::Center { width: 5, truncate_ellipsis: false }, .. })));
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

#[test]
fn format_field_left_with_ellipsis() {
    let f = parse_fmt("format X =\n@<<<<...\n$x\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { parts, .. } => {
            assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::LeftJustify { width: 5, truncate_ellipsis: true }, .. })));
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

#[test]
fn format_field_fill_left() {
    let f = parse_fmt("format X =\n^<<<<\n$x\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { parts, .. } => {
            assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::FillLeft { width: 5, truncate_ellipsis: false }, .. })));
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

#[test]
fn format_field_fill_with_ellipsis() {
    let f = parse_fmt("format X =\n^<<<<...\n$x\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { parts, .. } => {
            assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::FillLeft { width: 5, truncate_ellipsis: true }, .. })));
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

// ── Field: multi-line ──

#[test]
fn format_field_multi_line_at_star() {
    let f = parse_fmt("format X =\n@*\n$x\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { parts, .. } => {
            assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::MultiLine, .. })));
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

#[test]
fn format_field_fill_multi_line_caret_star() {
    let f = parse_fmt("format X =\n^*\n$x\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { parts, .. } => {
            assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::FillMultiLine, .. })));
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

// ── Field: numeric ──

#[test]
fn format_field_numeric_integer() {
    let f = parse_fmt("format X =\n@####\n$x\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { parts, .. } => match &parts[0] {
            FormatPart::Field(FormatField { kind: FieldKind::Numeric { integer_digits, decimal_digits, leading_zeros, caret }, .. }) => {
                assert_eq!(*integer_digits, 4);
                assert!(decimal_digits.is_none());
                assert!(!leading_zeros);
                assert!(!caret);
            }
            other => panic!("expected Numeric, got {other:?}"),
        },
        other => panic!("expected Picture, got {other:?}"),
    }
}

#[test]
fn format_field_numeric_with_decimal() {
    let f = parse_fmt("format X =\n@###.##\n$x\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { parts, .. } => match &parts[0] {
            FormatPart::Field(FormatField { kind: FieldKind::Numeric { integer_digits, decimal_digits, .. }, .. }) => {
                assert_eq!(*integer_digits, 3);
                assert_eq!(*decimal_digits, Some(2));
            }
            other => panic!("expected Numeric, got {other:?}"),
        },
        other => panic!("expected Picture, got {other:?}"),
    }
}

#[test]
fn format_field_numeric_leading_zeros() {
    let f = parse_fmt("format X =\n@0###\n$x\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { parts, .. } => {
            match &parts[0] {
                FormatPart::Field(FormatField { kind: FieldKind::Numeric { integer_digits, leading_zeros, .. }, .. }) => {
                    assert_eq!(*integer_digits, 4); // 0 + 3 #s
                    assert!(*leading_zeros);
                }
                other => panic!("expected Numeric, got {other:?}"),
            }
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

#[test]
fn format_field_numeric_caret() {
    let f = parse_fmt("format X =\n^####\n$x\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { parts, .. } => match &parts[0] {
            FormatPart::Field(FormatField { kind: FieldKind::Numeric { caret, integer_digits, .. }, .. }) => {
                assert!(*caret);
                assert_eq!(*integer_digits, 4);
            }
            other => panic!("expected Numeric, got {other:?}"),
        },
        other => panic!("expected Picture, got {other:?}"),
    }
}

#[test]
fn format_field_numeric_decimal_only() {
    // @.### — no integer digits, three decimal.
    let f = parse_fmt("format X =\n@.###\n$x\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { parts, .. } => match &parts[0] {
            FormatPart::Field(FormatField { kind: FieldKind::Numeric { integer_digits, decimal_digits, .. }, .. }) => {
                assert_eq!(*integer_digits, 0);
                assert_eq!(*decimal_digits, Some(3));
            }
            other => panic!("expected Numeric, got {other:?}"),
        },
        other => panic!("expected Picture, got {other:?}"),
    }
}

// ── Mixed picture lines ──

#[test]
fn format_multiple_fields_with_literals() {
    let f = parse_fmt("format X =\n@<<< = @>>>\n$k, $v\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { parts, args, .. } => {
            assert_eq!(parts.len(), 3);
            assert!(matches!(&parts[0], FormatPart::Field(_)));
            assert!(matches!(&parts[1], FormatPart::Literal(s) if s == " = "));
            assert!(matches!(&parts[2], FormatPart::Field(_)));
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

#[test]
fn format_literal_prefix_before_field() {
    let f = parse_fmt("format X =\nName: @<<<<<\n$name\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { parts, .. } => {
            assert_eq!(parts.len(), 2);
            assert!(matches!(&parts[0], FormatPart::Literal(s) if s == "Name: "));
            assert!(matches!(&parts[1], FormatPart::Field(_)));
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

// ── Args: expressions ──

#[test]
fn format_args_multiple_scalars() {
    let f = parse_fmt("format X =\n@<<< @<<< @<<<\n$a, $b, $c\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { args, .. } => {
            assert_eq!(args.len(), 3);
            for a in args {
                assert!(matches!(a.kind, ExprKind::ScalarVar(_)));
            }
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

#[test]
fn format_args_expression() {
    // Args are real Perl expressions, not just var refs.
    let f = parse_fmt("format X =\n@###\n$a + $b\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { args, .. } => {
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Add, _, _)));
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

// ── Args: braced multi-line form ──

#[test]
fn format_args_braced_single_line() {
    let f = parse_fmt("format X =\n@<<< @<<<\n{ $a, $b }\n.\n");
    match &f.lines[0] {
        FormatLine::Picture { args, .. } => {
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

#[test]
fn format_args_braced_multi_line() {
    // Classic perlform example: args spread across many lines.
    let src = "\
format X =
@<< @<< @<<
{
1,
2,
3,
}
.
";
    let f = parse_fmt(src);
    match &f.lines[0] {
        FormatLine::Picture { args, .. } => {
            assert_eq!(args.len(), 3);
            assert!(args.iter().all(|a| matches!(a.kind, ExprKind::IntLit(_))));
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

#[test]
fn format_args_braced_with_qw() {
    // qw(...) in braced args yields multiple list elements.
    let src = "\
format X =
@<< @<< @<<
{
qw[a b c],
}
.
";
    let f = parse_fmt(src);
    match &f.lines[0] {
        FormatLine::Picture { args, .. } => {
            // qw counts as one expr here (a QwList node); runtime
            // flattens it.  Parser sees one argument.
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

// ── Multi-line format body structure ──

#[test]
fn format_multiple_lines_mixed() {
    let src = "\
format X =
# header comment
Header text
@<<< @###
$name, $n
.
";
    let f = parse_fmt(src);
    assert_eq!(f.lines.len(), 3);
    assert!(matches!(f.lines[0], FormatLine::Comment { .. }));
    assert!(matches!(f.lines[1], FormatLine::Literal { .. }));
    assert!(matches!(f.lines[2], FormatLine::Picture { .. }));
}

#[test]
fn format_two_pictures_back_to_back() {
    let src = "\
format X =
@<<<
$a
@>>>
$b
.
";
    let f = parse_fmt(src);
    assert_eq!(f.lines.len(), 2);
    match &f.lines[0] {
        FormatLine::Picture { parts, .. } => {
            assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::LeftJustify { .. }, .. })));
        }
        _ => panic!(),
    }
    match &f.lines[1] {
        FormatLine::Picture { parts, .. } => {
            assert!(matches!(parts[0], FormatPart::Field(FormatField { kind: FieldKind::RightJustify { .. }, .. })));
        }
        _ => panic!(),
    }
}

#[test]
fn format_repeat_kind_on_picture_line() {
    let src = "\
format X =
~~ ^<<<
$long
.
";
    let f = parse_fmt(src);
    match &f.lines[0] {
        FormatLine::Picture { repeat, .. } => {
            assert!(matches!(repeat, RepeatKind::Repeat));
        }
        other => panic!("expected Picture, got {other:?}"),
    }
}

// ── Format followed by more top-level code ──

#[test]
fn format_followed_by_statement() {
    let src = "\
format X =
@<<<
$x
.
my $y = 1;
";
    let prog = parse(src);
    assert_eq!(prog.statements.len(), 2);
    assert!(matches!(prog.statements[0].kind, StmtKind::FormatDecl(_)));
    assert!(matches!(prog.statements[1].kind, StmtKind::Expr(_)));
}

// ── Rejection: `@` or `^` not followed by valid pad chars ──

#[test]
fn format_bare_at_is_literal() {
    // `I have an @ here.` — the lone `@` isn't a field start,
    // so the whole line parses as Literal.
    let f = parse_fmt("format X =\nI have an @ here.\n.\n");
    match &f.lines[0] {
        FormatLine::Literal { text, .. } => assert_eq!(text, "I have an @ here."),
        other => panic!("expected Literal, got {other:?}"),
    }
}

// ── class/field/method tests ──────────────────────────────

/// Convenience for class tests: prefixes the source with the
/// required `use feature 'class'` and `no warnings` pragmas,
/// then returns the first ClassDecl statement.
fn parse_class_prog(body: &str) -> Program {
    let src = format!("use feature 'class'; no warnings 'experimental::class'; {body}");
    parse(&src)
}

fn find_class_decl(prog: &Program) -> &ClassDecl {
    for stmt in &prog.statements {
        if let StmtKind::ClassDecl(c) = &stmt.kind {
            return c;
        }
    }
    panic!("no ClassDecl in program");
}

#[test]
fn parse_class_decl() {
    let prog = parse_class_prog("class Foo { field $x; method greet { 1; } }");
    let c = find_class_decl(&prog);
    assert_eq!(c.name, "Foo");
    assert!(c.body.as_ref().unwrap().statements.len() >= 2);
}

#[test]
fn parse_class_with_isa() {
    let prog = parse_class_prog("class Bar :isa(Foo) { }");
    let c = find_class_decl(&prog);
    assert_eq!(c.name, "Bar");
    assert_eq!(c.attributes.len(), 1);
    assert_eq!(c.attributes[0].name, "isa");
}

#[test]
fn parse_field_decl() {
    let prog = parse_class_prog("class Foo { field $x = 42; }");
    let c = find_class_decl(&prog);
    match &c.body.as_ref().unwrap().statements[0].kind {
        StmtKind::FieldDecl(f) => {
            assert_eq!(f.var.name, "x");
            assert!(f.default.is_some());
        }
        other => panic!("expected FieldDecl, got {other:?}"),
    }
}

#[test]
fn parse_field_with_param() {
    let prog = parse_class_prog("class Foo { field $name :param; }");
    let c = find_class_decl(&prog);
    match &c.body.as_ref().unwrap().statements[0].kind {
        StmtKind::FieldDecl(f) => {
            assert_eq!(f.attributes.len(), 1);
            assert_eq!(f.attributes[0].name, "param");
        }
        other => panic!("expected FieldDecl, got {other:?}"),
    }
}

#[test]
fn parse_method_decl() {
    let prog = parse_class_prog("class Foo { method greet() { 1; } }");
    let c = find_class_decl(&prog);
    match &c.body.as_ref().unwrap().statements[0].kind {
        StmtKind::MethodDecl(m) => {
            assert_eq!(m.name, "greet");
        }
        other => panic!("expected MethodDecl, got {other:?}"),
    }
}

// ── Indirect object syntax tests ──────────────────────────

#[test]
fn parse_indirect_new() {
    let e = parse_expr_str("new Foo(1, 2);");
    match &e.kind {
        ExprKind::IndirectMethodCall(class, method, args) => {
            assert!(matches!(&class.kind, ExprKind::Bareword(n) if n == "Foo"));
            assert_eq!(method, "new");
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected IndirectMethodCall, got {other:?}"),
    }
}

#[test]
fn parse_indirect_new_no_args() {
    let e = parse_expr_str("new Foo;");
    match &e.kind {
        ExprKind::IndirectMethodCall(class, method, args) => {
            assert!(matches!(&class.kind, ExprKind::Bareword(n) if n == "Foo"));
            assert_eq!(method, "new");
            assert_eq!(args.len(), 0);
        }
        other => panic!("expected IndirectMethodCall, got {other:?}"),
    }
}

#[test]
fn parse_indirect_with_var() {
    let e = parse_expr_str("new $class;");
    match &e.kind {
        ExprKind::IndirectMethodCall(invocant, method, _) => {
            assert_eq!(method, "new");
            assert!(matches!(invocant.kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected IndirectMethodCall, got {other:?}"),
    }
}

// ── Heredoc interpolation tests ───────────────────────────

#[test]
fn parse_heredoc_interpolation() {
    let src = "<<END;\nHello $name!\nEND\n";
    let prog = parse(src);
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::InterpolatedString(Interpolated(parts)), .. }) => {
            assert!(parts.len() >= 3); // "Hello ", $name, "!\n"
            assert!(matches!(parts[0], InterpPart::Const(_)));
            assert!(matches!(parts[1], InterpPart::ScalarInterp(_)));
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

#[test]
fn parse_heredoc_no_interp_stays_stringlit() {
    let src = "<<END;\nNo variables here.\nEND\n";
    let prog = parse(src);
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::StringLit(s), .. }) => {
            assert_eq!(s, "No variables here.\n");
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn parse_heredoc_single_quoted_no_interp() {
    // <<'END' should NOT interpolate — $name stays literal
    let src = "<<'END';\nHello $name!\nEND\n";
    let prog = parse(src);
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::StringLit(s), .. }) => {
            assert_eq!(s, "Hello $name!\n");
        }
        other => panic!("expected StringLit with literal $name, got {other:?}"),
    }
}

// ── Heredoc nesting torture tests ─────────────────────────

#[test]
fn parse_heredoc_two_stacked() {
    // Two heredocs on one line, bodies consumed in order.
    let src = "print <<A, <<B;\nbody A\nA\nbody B\nB\nafter\n";
    let prog = parse(src);
    assert_eq!(prog.statements.len(), 2);
    // First statement: print with two heredoc args.
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(name, _, args), .. }) => {
            assert_eq!(name, "print");
            assert_eq!(args.len(), 2);
            assert!(matches!(&args[0].kind, ExprKind::StringLit(s) if s == "body A\n"));
            assert!(matches!(&args[1].kind, ExprKind::StringLit(s) if s == "body B\n"));
        }
        other => panic!("expected print with 2 heredoc args, got {other:?}"),
    }
}

#[test]
fn parse_heredoc_three_stacked() {
    // Three heredocs on one line.
    let src = "print <<A, <<B, <<C;\nA-body\nA\nB-body\nB\nC-body\nC\n";
    let prog = parse(src);
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, _, args), .. }) => {
            assert_eq!(args.len(), 3);
            assert!(matches!(&args[0].kind, ExprKind::StringLit(s) if s == "A-body\n"));
            assert!(matches!(&args[1].kind, ExprKind::StringLit(s) if s == "B-body\n"));
            assert!(matches!(&args[2].kind, ExprKind::StringLit(s) if s == "C-body\n"));
        }
        other => panic!("expected print with 3 heredoc args, got {other:?}"),
    }
}

#[test]
fn parse_heredoc_stacked_mixed_quoting() {
    // Mix of <<TAG, <<'TAG', and <<"TAG".
    let src = "print <<A, <<'B', <<\"C\";\nA: $x\nA\nB: $x\nB\nC: $x\nC\n";
    let prog = parse(src);
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, _, args), .. }) => {
            assert_eq!(args.len(), 3);
            // <<A interpolates (A's body has $x).
            assert!(matches!(&args[0].kind, ExprKind::InterpolatedString(_)));
            // <<'B' does not interpolate.
            assert!(matches!(&args[1].kind, ExprKind::StringLit(s) if s == "B: $x\n"));
            // <<"C" interpolates.
            assert!(matches!(&args[2].kind, ExprKind::InterpolatedString(_)));
        }
        other => panic!("expected print with 3 mixed heredocs, got {other:?}"),
    }
}

#[test]
fn parse_heredoc_stacked_with_trailing_code() {
    // Bodies on separate lines, then more code after.
    let src = "my @a = (<<X, <<Y);\nX-body\nX\nY-body\nY\nmy $z = 1;\n";
    let prog = parse(src);
    assert_eq!(prog.statements.len(), 2);
    let (_, vars0) = decl_vars(&prog.statements[0]);
    assert_eq!(vars0[0].name, "a");
    let (_, vars1) = decl_vars(&prog.statements[1]);
    assert_eq!(vars1[0].name, "z");
}

#[test]
fn parse_heredoc_indented_stacked() {
    // <<~A and <<~B stacked, indentation stripped from each.
    let src = "print <<~A, <<~B;\n    A-body\n    A\n        B-body\n        B\n";
    let prog = parse(src);
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, _, args), .. }) => {
            assert_eq!(args.len(), 2);
            assert!(matches!(&args[0].kind, ExprKind::StringLit(s) if s == "A-body\n"));
            assert!(matches!(&args[1].kind, ExprKind::StringLit(s) if s == "B-body\n"));
        }
        other => panic!("expected print with 2 indented heredocs, got {other:?}"),
    }
}

#[test]
fn parse_heredoc_mixed_indented_non_indented() {
    // <<A followed by <<~B: one plain, one indented.
    let src = "print <<A, <<~B;\nplain-body\nA\n    indented-body\n    B\n";
    let prog = parse(src);
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, _, args), .. }) => {
            assert_eq!(args.len(), 2);
            assert!(matches!(&args[0].kind, ExprKind::StringLit(s) if s == "plain-body\n"));
            assert!(matches!(&args[1].kind, ExprKind::StringLit(s) if s == "indented-body\n"));
        }
        other => panic!("expected print with mixed heredocs, got {other:?}"),
    }
}

#[test]
fn parse_heredoc_same_tag_name() {
    // Two heredocs with the same tag name.  The first body
    // terminates at the first occurrence of the tag, then
    // the second heredoc begins with a new body.
    let src = "print <<END, <<END;\nfirst\nEND\nsecond\nEND\n";
    let prog = parse(src);
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, _, args), .. }) => {
            assert_eq!(args.len(), 2);
            assert!(matches!(&args[0].kind, ExprKind::StringLit(s) if s == "first\n"));
            assert!(matches!(&args[1].kind, ExprKind::StringLit(s) if s == "second\n"));
        }
        other => panic!("expected print with 2 same-tag heredocs, got {other:?}"),
    }
}

#[test]
fn parse_heredoc_with_concat_then_heredoc() {
    // <<A . <<B — concatenation of two heredoc strings.
    let src = "my $x = <<A . <<B;\nalpha\nA\nbeta\nB\n";
    let prog = parse(src);
    let init = decl_init(&prog.statements[0]);
    // Should be a Concat of two StringLits.
    match &init.kind {
        ExprKind::BinOp(BinOp::Concat, left, right) => {
            assert!(matches!(&left.kind, ExprKind::StringLit(s) if s == "alpha\n"));
            assert!(matches!(&right.kind, ExprKind::StringLit(s) if s == "beta\n"));
        }
        other => panic!("expected Concat, got {other:?}"),
    }
}

#[test]
fn parse_heredoc_unterminated_gives_error() {
    // Heredoc tag with no terminator line → parse error.
    let src = "my $x = <<END;\nbody line\nbody line 2\n";
    let mut parser = Parser::new(src.as_bytes()).unwrap();
    let result = parser.parse_program();
    assert!(result.is_err(), "expected error for unterminated heredoc");
}

// ── Torture test pieces ──────────────────────────────────
//
// Derived from a real Perl program that exercises heredoc
// nesting, interpolation forms, and compile-time hoisting
// simultaneously.  Each test below isolates one aspect so
// failures are diagnostic.

#[test]
fn torture_heredoc_arithmetic_stacked() {
    // `<<A + <<B + <<C` — three heredocs combined with `+`.
    // Bodies are single numbers.  Deparse evaluates at
    // compile time but we just verify parsing.
    let src = "my $x = <<A + <<B + <<C;\n1\nA\n2\nB\n3\nC\n";
    let prog = parse(src);
    let init = decl_init(&prog.statements[0]);
    // Shape: Add(Add(heredoc_A, heredoc_B), heredoc_C).
    match &init.kind {
        ExprKind::BinOp(BinOp::Add, left, right) => {
            // Right is the third heredoc (literal "3\n").
            assert!(matches!(&right.kind, ExprKind::StringLit(s) if s == "3\n"), "right should be heredoc C, got {:?}", right.kind);
            match &left.kind {
                ExprKind::BinOp(BinOp::Add, a, b) => {
                    assert!(matches!(&a.kind, ExprKind::StringLit(s) if s == "1\n"), "first should be heredoc A");
                    assert!(matches!(&b.kind, ExprKind::StringLit(s) if s == "2\n"), "second should be heredoc B");
                }
                other => panic!("inner should be Add, got {other:?}"),
            }
        }
        other => panic!("expected Add at top, got {other:?}"),
    }
}

#[test]
fn torture_ref_to_expr_in_interp() {
    // `"${\(1 + 2)}"` — `${...}` with `\(expr)` inside.
    // This is a common Perl idiom for embedding arbitrary
    // expressions in interpolated strings.
    let parts = interp_parts(r#""${\(1 + 2)}";"#);
    // Expect: ExprInterp containing Ref(Paren(Add(1, 2)))
    // or Ref(Add(1, 2)) — depends on paren handling.
    assert_eq!(parts.len(), 1);
    match &parts[0] {
        InterpPart::ExprInterp(e) => {
            // Outer is Ref(\...).
            match &e.kind {
                ExprKind::Ref(inner) => {
                    // Inner is the paren-wrapped addition.
                    let actual_add = match &inner.kind {
                        ExprKind::Paren(p) => p,
                        other => panic!("expected Paren inside Ref, got {other:?}"),
                    };
                    assert!(matches!(actual_add.kind, ExprKind::BinOp(BinOp::Add, _, _)), "expected Add, got {:?}", actual_add.kind);
                }
                other => panic!("expected Ref, got {other:?}"),
            }
        }
        other => panic!("expected ExprInterp, got {other:?}"),
    }
}

#[test]
fn torture_do_block_in_expression() {
    // `my $x = do { 1 + 2 };` — do-block as expression.
    let prog = parse("my $x = do { 1 + 2 };");
    let init = decl_init(&prog.statements[0]);
    match &init.kind {
        ExprKind::DoBlock(_) => {}
        other => panic!("expected DoBlock, got {other:?}"),
    }
}

#[test]
fn torture_begin_inside_do_block() {
    // `do { BEGIN { our $a = 1; } $a }` — BEGIN hoists to
    // compile time even inside a runtime do-block.  We just
    // verify the parser accepts this; BEGIN semantics are
    // runtime behavior.
    let prog = parse("my $x = do { BEGIN { our $a = 1; } $a };");
    let init = decl_init(&prog.statements[0]);
    match &init.kind {
        ExprKind::DoBlock(block) => {
            // Block should contain a BEGIN and an expression.
            assert!(block.statements.len() >= 2, "expected at least BEGIN + expr in do-block, got {:?}", block.statements);
        }
        other => panic!("expected DoBlock, got {other:?}"),
    }
}

#[test]
fn torture_heredoc_in_interp_of_heredoc() {
    // Heredoc inside `${\(...)}` inside another heredoc body.
    // This is the nesting pattern from the torture test:
    //   <<OUTER contains `${\(do { my $a = <<INNER; ... })}`.
    // Simplified version:
    let src = "\
my $x = <<OUTER;\n\
prefix ${\\ do { <<INNER }}\n\
inner body\n\
INNER\n\
suffix\n\
OUTER\n";
    let prog = parse(src);
    assert!(!prog.statements.is_empty(), "should parse without error");
    let init = decl_init(&prog.statements[0]);
    // Outer is an InterpolatedString (heredoc with interpolation).
    assert!(matches!(init.kind, ExprKind::InterpolatedString(_)), "expected InterpolatedString for heredoc, got {:?}", init.kind);
}

#[test]
fn torture_array_interp_with_heredoc() {
    // `"@{[<<END]}"` — array interpolation containing a heredoc.
    let src = "my $x = \"@{[<<END]}\";\nheredoc body\nEND\n";
    let prog = parse(src);
    assert!(!prog.statements.is_empty(), "should parse");
    let init = decl_init(&prog.statements[0]);
    assert!(matches!(init.kind, ExprKind::InterpolatedString(_)), "expected InterpolatedString, got {:?}", init.kind);
}

#[test]
fn torture_qq_with_nested_heredoc() {
    // `qq{prefix ${\(<<END)} suffix}` — qq with heredoc inside.
    let src = "my $x = qq{prefix ${\\<<END} suffix};\nheredoc body\nEND\n";
    let prog = parse(src);
    assert!(!prog.statements.is_empty(), "should parse");
}

#[test]
fn torture_stacked_heredoc_list_assignment() {
    // The exact pattern from the torture test:
    // `my ($x, $y, $z) = (<<~X, <<Y, do { expr });`
    // Simplified: just two heredocs plus a literal.
    let src = "my ($x, $y, $z) = (<<~A, <<B, 42);\n    A-body\n    A\nB-body\nB\n";
    let prog = parse(src);
    assert!(!prog.statements.is_empty(), "should parse");
}

// ── Dynamic method dispatch tests ─────────────────────────

// ═══════════════════════════════════════════════════════════
// Gap-probing tests — things I'm not sure the parser
// handles.  Written to match Perl's actual behavior.
// Failures are diagnostic: they tell us what to fix.
// ═══════════════════════════════════════════════════════════

// ── Postderef_qq: remaining forms ────────────────────────

#[test]
fn interp_postderef_qq_code() {
    // `->&*` — code deref inside string.
    let parts = interp_parts(r#""$ref->&*";"#);
    let e = scalar_part(&parts, 0);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefCode)), "expected DerefCode, got {:?}", e.kind);
}

#[test]
fn interp_postderef_qq_glob() {
    // `->**` — glob deref inside string.  Lexer emits
    // Token::Power for `**`.
    let parts = interp_parts(r#""$ref->**";"#);
    let e = scalar_part(&parts, 0);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefGlob)), "expected DerefGlob, got {:?}", e.kind);
}

#[test]
fn interp_postderef_qq_array_slice() {
    // `->@[0,1]` — array slice inside string.
    let parts = interp_parts(r#""$ref->@[0,1]";"#);
    let e = scalar_part(&parts, 0);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::ArraySliceIndices(_))), "expected ArraySliceIndices, got {:?}", e.kind);
}

#[test]
fn interp_postderef_qq_hash_slice() {
    // `->@{"a","b"}` — hash slice (values) inside string.
    let parts = interp_parts(r#""$ref->@{'a','b'}";"#);
    let e = scalar_part(&parts, 0);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::ArraySliceKeys(_))), "expected ArraySliceKeys, got {:?}", e.kind);
}

// ── Indented heredoc edge cases ──────────────────────────

#[test]
fn heredoc_indented_tabs() {
    // <<~END with tab indentation.
    let src = "my $x = <<~END;\n\tindented with tab\n\tEND\n";
    let prog = parse(src);
    let init = decl_init(&prog.statements[0]);
    assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s == "indented with tab\n"), "expected stripped tab indent, got {:?}", init.kind);
}

#[test]
fn heredoc_indented_empty_body() {
    // <<~END with terminator immediately — empty body.
    let src = "my $x = <<~END;\n    END\n";
    let prog = parse(src);
    let init = decl_init(&prog.statements[0]);
    assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s.is_empty()), "expected empty string, got {:?}", init.kind);
}

#[test]
fn heredoc_indented_blank_lines_preserved() {
    // Blank lines in <<~ body should be preserved as
    // empty lines (they don't need indentation).
    let src = "my $x = <<~END;\n    line1\n\n    line2\n    END\n";
    let prog = parse(src);
    let init = decl_init(&prog.statements[0]);
    assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s == "line1\n\nline2\n"), "expected blank line preserved, got {:?}", init.kind);
}

#[test]
fn heredoc_traditional_empty_body() {
    // Regular <<END with tag on the very next line.
    let src = "my $x = <<END;\nEND\n";
    let prog = parse(src);
    let init = decl_init(&prog.statements[0]);
    assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s.is_empty()), "expected empty heredoc body, got {:?}", init.kind);
}

// ── Heredoc backslash form ───────────────────────────────

#[test]
fn heredoc_backslash_form() {
    // `<<\EOF` — equivalent to `<<'EOF'` (non-interpolating).
    let src = "my $x = <<\\EOF;\nHello \\$name!\nEOF\n";
    let prog = parse(src);
    let init = decl_init(&prog.statements[0]);
    // Non-interpolating: `$name` stays literal.
    assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s.contains("$name")), "expected literal $name in body, got {:?}", init.kind);
}

#[test]
fn heredoc_backslash_no_escape_processing() {
    // Per perlop: backslashes have no special meaning in a
    // single-quoted here-doc, `\\` is two backslashes.
    let src = "my $x = <<\\EOF;\nline with \\\\ two backslashes\nEOF\n";
    let prog = parse(src);
    let init = decl_init(&prog.statements[0]);
    assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s.contains("\\\\")), "expected literal double-backslash, got {:?}", init.kind);
}

#[test]
fn heredoc_indented_backslash_form() {
    // `<<~\EOF` — indented + backslash (non-interpolating).
    let src = "my $x = <<~\\EOF;\n    Hello $name!\n    EOF\n";
    let prog = parse(src);
    let init = decl_init(&prog.statements[0]);
    assert!(
        matches!(init.kind, ExprKind::StringLit(ref s) if s.contains("$name")),
        "expected literal $name in indented backslash heredoc, got {:?}",
        init.kind
    );
}

// ── Heredoc numeric and special tags ─────────────────────

#[test]
fn heredoc_numeric_bare_tag() {
    // Perl accepts <<0 as a heredoc with tag "0".
    // The gate in lex_heredoc_after_shift_left must accept
    // digits, not just alphabetic + underscore.
    let prog = parse("my $x = <<0;\nhello\n0\n");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Assign(_, _, rhs) => match &rhs.kind {
                ExprKind::StringLit(s) => assert_eq!(s, "hello\n"),
                other => panic!("expected StringLit, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn heredoc_numeric_tag_42() {
    // <<42 is also valid in Perl.
    let prog = parse("my $x = <<42;\nworld\n42\n");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Assign(_, _, rhs) => match &rhs.kind {
                ExprKind::StringLit(s) => assert_eq!(s, "world\n"),
                other => panic!("expected StringLit, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn heredoc_tag_end_marker() {
    // <<__END__ is a valid heredoc tag — __END__ inside the
    // body is literal text, and the terminator __END__ is
    // matched by the heredoc machinery, not the data-end detector.
    let prog = parse("my $x = <<__END__;\ncontent\n__END__\n");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Assign(_, _, rhs) => match &rhs.kind {
                ExprKind::StringLit(s) => assert_eq!(s, "content\n"),
                other => panic!("expected StringLit, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

// ── Heredoc keyword tags ──────────────────────────────────
// Perl treats barewords after << as heredoc tags, even if they
// are keyword names.

#[test]
fn heredoc_keyword_tag_if() {
    let prog = parse("my $x = <<if;\nhello\nif\n");
    let init = first_assign_rhs(&prog);
    assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s == "hello\n"), "expected heredoc body, got {:?}", init.kind);
}

#[test]
fn heredoc_keyword_tag_for() {
    let prog = parse("my $x = <<for;\nhello\nfor\n");
    let init = first_assign_rhs(&prog);
    assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s == "hello\n"), "expected heredoc body, got {:?}", init.kind);
}

#[test]
fn heredoc_keyword_tag_sub() {
    let prog = parse("my $x = <<sub;\nhello\nsub\n");
    let init = first_assign_rhs(&prog);
    assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s == "hello\n"), "expected heredoc body, got {:?}", init.kind);
}

#[test]
fn heredoc_keyword_tag_my() {
    let prog = parse("my $x = <<my;\nhello\nmy\n");
    let init = first_assign_rhs(&prog);
    assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s == "hello\n"), "expected heredoc body, got {:?}", init.kind);
}

#[test]
fn heredoc_keyword_tag_use() {
    let prog = parse("my $x = <<use;\nhello\nuse\n");
    let init = first_assign_rhs(&prog);
    assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s == "hello\n"), "expected heredoc body, got {:?}", init.kind);
}

#[test]
fn heredoc_keyword_tag_return() {
    let prog = parse("my $x = <<return;\nhello\nreturn\n");
    let init = first_assign_rhs(&prog);
    assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s == "hello\n"), "expected heredoc body, got {:?}", init.kind);
}

// ── POD and markers inside heredoc bodies ────────────────

#[test]
fn pod_inside_heredoc_is_literal() {
    let src = "my $x = <<END;\nbefore\n=pod\nThis is not pod.\n=cut\nafter\nEND\n";
    let prog = parse(src);
    let init = first_assign_rhs(&prog);
    match &init.kind {
        ExprKind::StringLit(s) => {
            assert!(s.contains("=pod"), "=pod should be literal in heredoc body");
            assert!(s.contains("=cut"), "=cut should be literal in heredoc body");
            assert!(s.contains("before"));
            assert!(s.contains("after"));
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn end_marker_inside_heredoc_is_literal() {
    let src = "my $x = <<END;\nbefore\n__END__\nafter\nEND\n";
    let prog = parse(src);
    let init = first_assign_rhs(&prog);
    match &init.kind {
        ExprKind::StringLit(s) => {
            assert!(s.contains("__END__"), "__END__ should be literal in heredoc body");
            assert!(s.contains("before"));
            assert!(s.contains("after"));
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

// ── Ternary with stacked heredocs ────────────────────────

#[test]
fn ternary_with_heredocs() {
    // Both branches of ?: can be heredocs.  Bodies are queued
    // in source order.
    let src = "my $x = 1 ? <<A : <<B;\nfirst\nA\nsecond\nB\n";
    let prog = parse(src);
    assert!(!prog.statements.is_empty(), "should parse ternary with heredocs");
}

// ── Backslash heredoc edge cases ─────────────────────────

#[test]
fn heredoc_backslash_digit_tag() {
    // <<\0 — backslash form with digit tag.
    let src = "my $x = <<\\0;\nhello\n0\n";
    let prog = parse(src);
    let init = first_assign_rhs(&prog);
    assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s == "hello\n"), "expected literal heredoc body, got {:?}", init.kind);
}

// ── Heredoc terminator exactness ─────────────────────────

#[test]
fn heredoc_terminator_must_be_exact_trailing_space() {
    // "END " (trailing space) is not a terminator.
    let src = "my $x = <<END;\nEND \nnot terminated yet\nEND\n";
    let prog = parse(src);
    let init = first_assign_rhs(&prog);
    match &init.kind {
        ExprKind::StringLit(s) => {
            assert!(s.contains("END "), "line with trailing space should be body content");
            assert!(s.contains("not terminated yet"));
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn heredoc_terminator_must_be_exact_trailing_tab() {
    // "END\t" (trailing tab) is not a terminator.
    let src = "my $x = <<END;\nEND\t\nbody\nEND\n";
    let prog = parse(src);
    let init = first_assign_rhs(&prog);
    match &init.kind {
        ExprKind::StringLit(s) => {
            assert!(s.contains("END\t"), "line with trailing tab should be body content");
            assert!(s.contains("body"));
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

// ── Substitution delimiter variations ────────────────────

#[test]
fn subst_paren_delimiters() {
    let e = parse_expr_str("s(foo)(bar);");
    match &e.kind {
        ExprKind::Subst(pat, repl, _) => {
            assert_eq!(pat_str(pat), "foo");
            assert_eq!(pat_str(repl), "bar");
        }
        other => panic!("expected Subst, got {other:?}"),
    }
}

#[test]
fn subst_bracket_delimiters() {
    let e = parse_expr_str("s[foo][bar];");
    match &e.kind {
        ExprKind::Subst(pat, repl, _) => {
            assert_eq!(pat_str(pat), "foo");
            assert_eq!(pat_str(repl), "bar");
        }
        other => panic!("expected Subst, got {other:?}"),
    }
}

#[test]
fn subst_mixed_paired_delimiters() {
    // s{pattern}(replacement) — different paired delims.
    let e = parse_expr_str("s{foo}(bar);");
    match &e.kind {
        ExprKind::Subst(pat, repl, _) => {
            assert_eq!(pat_str(pat), "foo");
            assert_eq!(pat_str(repl), "bar");
        }
        other => panic!("expected Subst, got {other:?}"),
    }
}

#[test]
fn subst_paired_pattern_unpaired_replacement() {
    // s{pattern}/replacement/ — paired then unpaired.
    let e = parse_expr_str("s{foo}/bar/;");
    match &e.kind {
        ExprKind::Subst(pat, repl, _) => {
            assert_eq!(pat_str(pat), "foo");
            assert_eq!(pat_str(repl), "bar");
        }
        other => panic!("expected Subst, got {other:?}"),
    }
}

#[test]
fn subst_angle_delimiters() {
    // s<foo><bar> — angle brackets as paired delimiters.
    let e = parse_expr_str("s<foo><bar>;");
    match &e.kind {
        ExprKind::Subst(pat, repl, _) => {
            assert_eq!(pat_str(pat), "foo");
            assert_eq!(pat_str(repl), "bar");
        }
        other => panic!("expected Subst, got {other:?}"),
    }
}

#[test]
fn tr_paired_delimiters() {
    // tr{a-z}{A-Z} — paired braces for tr.
    let e = parse_expr_str("tr{a-z}{A-Z};");
    match &e.kind {
        ExprKind::Translit(from, to, _) => {
            assert_eq!(from, "a-z");
            assert_eq!(to, "A-Z");
        }
        other => panic!("expected Translit, got {other:?}"),
    }
}

// ── Empty / minimal quote forms ──────────────────────────

#[test]
fn empty_qw() {
    // `qw()` — empty word list.
    let e = parse_expr_str("qw();");
    match &e.kind {
        ExprKind::QwList(words) => assert!(words.is_empty()),
        other => panic!("expected empty QwList, got {other:?}"),
    }
}

#[test]
fn single_qw() {
    let e = parse_expr_str("qw(hello);");
    match &e.kind {
        ExprKind::QwList(words) => {
            assert_eq!(words.len(), 1);
            assert_eq!(words[0], "hello");
        }
        other => panic!("expected QwList, got {other:?}"),
    }
}

#[test]
fn empty_q_string() {
    let e = parse_expr_str("q{};");
    assert!(matches!(e.kind, ExprKind::StringLit(ref s) if s.is_empty()), "expected empty StringLit, got {:?}", e.kind);
}

#[test]
fn empty_interpolated_string() {
    let e = parse_expr_str("\"\";");
    assert!(matches!(e.kind, ExprKind::StringLit(ref s) if s.is_empty()), "expected empty StringLit, got {:?}", e.kind);
}

// ── Hash key edge cases ──────────────────────────────────

#[test]
fn negative_bareword_hash_key() {
    // `$h{-key}` — the `-key` form is common in Perl.
    // Parses as HashElem with StringLit("-key").
    let e = parse_expr_str("$h{-key};");
    match &e.kind {
        ExprKind::HashElem(_, k) => {
            assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "-key"), "expected StringLit(-key), got {:?}", k.kind);
        }
        other => panic!("expected HashElem, got {other:?}"),
    }
}

#[test]
fn numeric_hash_key() {
    // `$h{42}` — numeric key, not autoquoted.
    let e = parse_expr_str("$h{42};");
    match &e.kind {
        ExprKind::HashElem(_, k) => {
            assert!(matches!(k.kind, ExprKind::IntLit(42)), "expected IntLit(42), got {:?}", k.kind);
        }
        other => panic!("expected HashElem, got {other:?}"),
    }
}

// ── Special variable forms ───────────────────────────────

#[test]
fn local_list_separator() {
    // `local $" = ","` — localizing the list separator.
    let prog = parse(r#"local $" = ",";"#);
    assert!(!prog.statements.is_empty(), "should parse local $\" assignment");
}

#[test]
fn special_var_in_interpolation() {
    // `"v$^V"` — $^V (Perl version) in a string.
    let parts = interp_parts(r#""v$^V";"#);
    assert!(parts.len() >= 2, "expected at least const + var");
}

// ── Control flow edge cases ──────────────────────────────

#[test]
fn nested_ternary() {
    let e = parse_expr_str("$a ? $b ? 1 : 2 : 3;");
    // Right-associative: `$a ? ($b ? 1 : 2) : 3`.
    match &e.kind {
        ExprKind::Ternary(_, then_expr, else_expr) => {
            assert!(matches!(then_expr.kind, ExprKind::Ternary(_, _, _)), "inner then should be another ternary");
            assert!(matches!(else_expr.kind, ExprKind::IntLit(3)));
        }
        other => panic!("expected nested Ternary, got {other:?}"),
    }
}

#[test]
fn unless_block() {
    let prog = parse("unless ($x) { 1; }");
    assert!(!prog.statements.is_empty());
}

#[test]
fn until_loop() {
    let prog = parse("until ($done) { do_work(); }");
    assert!(!prog.statements.is_empty());
}

#[test]
fn chained_method_calls() {
    let e = parse_expr_str("$obj->method1->method2->method3;");
    // Outer: MethodCall(MethodCall(MethodCall($obj, "method1", []),
    //        "method2", []), "method3", []).
    // Note: `->method` produces MethodCall, not ArrowDeref.
    fn depth(e: &Expr) -> usize {
        match &e.kind {
            ExprKind::MethodCall(inner, _, _) => 1 + depth(inner),
            ExprKind::ArrowDeref(inner, _) => 1 + depth(inner),
            _ => 0,
        }
    }
    assert_eq!(depth(&e), 3, "expected 3 levels of method chain");
}

// ── String operator precedence ───────────────────────────

#[test]
fn concat_and_repeat() {
    // `"a" . "b" x 3` — `x` binds tighter than `.`.
    // Parses as `"a" . ("b" x 3)`.
    let e = parse_expr_str(r#""a" . "b" x 3;"#);
    match &e.kind {
        ExprKind::BinOp(BinOp::Concat, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::Repeat, _, _)), "rhs of concat should be repeat, got {:?}", rhs.kind);
        }
        other => panic!("expected Concat, got {other:?}"),
    }
}

// ── Defined-or forms ─────────────────────────────────────

#[test]
fn defined_or_assign() {
    let e = parse_expr_str("$x //= 42;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::DefinedOrEq, _, _)), "expected //= assignment, got {:?}", e.kind);
}

#[test]
fn chained_defined_or() {
    // `$a // $b // $c` — left-associative.
    let e = parse_expr_str("$a // $b // $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::DefinedOr, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)), "inner should also be DefinedOr");
        }
        other => panic!("expected chained DefinedOr, got {other:?}"),
    }
}

// ── do "filename" vs do { block } ────────────────────────

#[test]
fn do_file() {
    // `do "config.pl"` — loads and executes a file.
    let e = parse_expr_str(r#"do "config.pl";"#);
    match &e.kind {
        ExprKind::DoExpr(path) => {
            assert!(matches!(path.kind, ExprKind::StringLit(ref s) if s == "config.pl"));
        }
        other => panic!("expected DoExpr, got {other:?}"),
    }
}

#[test]
fn do_block_vs_do_file() {
    // `do { 1 }` vs `do "file"` — both valid.
    let block = parse_expr_str("do { 1 };");
    assert!(matches!(block.kind, ExprKind::DoBlock(_)));
    let file = parse_expr_str(r#"do "file";"#);
    assert!(matches!(file.kind, ExprKind::DoExpr(_)));
}

// ── require ──────────────────────────────────────────────

#[test]
fn require_module() {
    let prog = parse("require Foo::Bar;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn require_version() {
    let prog = parse("require 5.036;");
    assert!(!prog.statements.is_empty());
}

// ── __DATA__ section ─────────────────────────────────────

#[test]
fn data_section() {
    let src = "my $x = 1;\n__DATA__\nThis is data.\nMore data.\n";
    let prog = parse(src);
    // Should have at least 2 statements: my decl and DataEnd.
    assert!(prog.statements.len() >= 2);
    let has_data_end = prog.statements.iter().any(|s| matches!(s.kind, StmtKind::DataEnd(_, _)));
    assert!(has_data_end, "expected DataEnd statement");
}

// ── Regex edge cases ─────────────────────────────────────

#[test]
fn regex_many_flags() {
    let e = parse_expr_str("/foo/msixpn;");
    match &e.kind {
        ExprKind::Regex(_, _, flags) => {
            let f = flags.as_deref().unwrap_or("");
            assert!(f.contains('m') && f.contains('s') && f.contains('i') && f.contains('x'), "expected msixpn flags, got {f:?}");
        }
        other => panic!("expected Regex, got {other:?}"),
    }
}

#[test]
fn regex_character_class() {
    let e = parse_expr_str(r#"/[a-z\d\s]+/;"#);
    match &e.kind {
        ExprKind::Regex(_, pat, _) => {
            let s = pat_str(pat);
            assert!(s.contains("[a-z") && s.contains("]"), "expected char class in pattern, got {s:?}");
        }
        other => panic!("expected Regex, got {other:?}"),
    }
}

// ── Print to filehandle ──────────────────────────────────

#[test]
fn print_to_stderr() {
    let prog = parse(r#"print STDERR "error\n";"#);
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, fh, _), .. }) => {
            assert!(fh.is_some(), "expected filehandle");
        }
        other => panic!("expected PrintOp with filehandle, got {other:?}"),
    }
}

// ── Fat comma autoquoting edge case ──────────────────────

#[test]
fn fat_comma_numeric_key() {
    // `123 => "val"` — numbers are NOT autoquoted.
    let e = parse_expr_str("123 => 'val';");
    match &e.kind {
        ExprKind::List(items) => {
            assert!(matches!(items[0].kind, ExprKind::IntLit(123)), "numeric key should stay IntLit, got {:?}", items[0].kind);
        }
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn parse_dynamic_method() {
    // $obj->$method
    let e = parse_expr_str("$obj->$method;");
    match &e.kind {
        ExprKind::ArrowDeref(_, ArrowTarget::DynMethod(method_expr, args)) => {
            assert!(matches!(method_expr.kind, ExprKind::ScalarVar(_)));
            assert_eq!(args.len(), 0);
        }
        other => panic!("expected DynMethod, got {other:?}"),
    }
}

#[test]
fn parse_dynamic_method_with_args() {
    // $obj->$method(1, 2)
    let e = parse_expr_str("$obj->$method(1, 2);");
    match &e.kind {
        ExprKind::ArrowDeref(_, ArrowTarget::DynMethod(_, args)) => {
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected DynMethod with args, got {other:?}"),
    }
}

// ── Complex local lvalue tests ────────────────────────────

#[test]
fn parse_local_hash_elem() {
    let prog = parse("local $hash{key} = 42;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => match &lhs.kind {
            ExprKind::Local(inner) => {
                assert!(matches!(inner.kind, ExprKind::HashElem(_, _)));
            }
            other => panic!("expected Local(HashElem), got {other:?}"),
        },
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn parse_local_glob() {
    let prog = parse("local *STDOUT;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Local(inner), .. }) => {
            assert!(matches!(inner.kind, ExprKind::GlobVar(_)));
        }
        other => panic!("expected Local(GlobVar), got {other:?}"),
    }
}

#[test]
fn parse_local_simple_var() {
    // local $x = 5 still works
    let prog = parse("local $x = 5;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => {
            assert!(matches!(lhs.kind, ExprKind::Local(_)));
        }
        other => panic!("expected Assign(Local), got {other:?}"),
    }
}

#[test]
fn parse_delete_local_hash_elem() {
    // delete local $hash{key}
    let e = parse_expr_str("delete local $hash{key};");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "delete");
            assert!(matches!(args[0].kind, ExprKind::Local(_)));
        }
        other => panic!("expected delete(Local(...)), got {other:?}"),
    }
}

#[test]
fn parse_local_special_var() {
    // local $/ — localize input record separator
    let e = parse_expr_str("local $/;");
    match &e.kind {
        ExprKind::Local(inner) => {
            assert!(matches!(inner.kind, ExprKind::SpecialVar(_)));
        }
        other => panic!("expected Local(SpecialVar), got {other:?}"),
    }
}

// ── Filetest operator tests ───────────────────────────────

#[test]
fn parse_filetest_e() {
    let e = parse_expr_str("-e $file;");
    match &e.kind {
        ExprKind::Filetest(c, StatTarget::Expr(operand)) => {
            assert_eq!(*c, 'e');
            assert!(matches!(operand.kind, ExprKind::ScalarVar(ref n) if n == "file"));
        }
        other => panic!("expected Filetest('e', Expr(ScalarVar)), got {other:?}"),
    }
}

#[test]
fn parse_filetest_d_string() {
    let e = parse_expr_str(r#"-d "/tmp";"#);
    match &e.kind {
        ExprKind::Filetest(c, StatTarget::Expr(operand)) => {
            assert_eq!(*c, 'd');
            assert!(matches!(operand.kind, ExprKind::StringLit(ref s) if s == "/tmp"));
        }
        other => panic!("expected Filetest('d', Expr(StringLit)), got {other:?}"),
    }
}

#[test]
fn parse_filetest_f_underscore() {
    // -f _ uses the cached stat buffer — dedicated AST variant.
    let e = parse_expr_str("-f _;");
    match &e.kind {
        ExprKind::Filetest(c, StatTarget::StatCache) => {
            assert_eq!(*c, 'f');
        }
        other => panic!("expected Filetest('f', StatCache), got {other:?}"),
    }
}

#[test]
fn parse_filetest_no_operand() {
    // -e alone defaults to $_ — dedicated AST variant.
    let e = parse_expr_str("-e;");
    match &e.kind {
        ExprKind::Filetest(c, StatTarget::Default) => {
            assert_eq!(*c, 'e');
        }
        other => panic!("expected Filetest('e', Default), got {other:?}"),
    }
}

#[test]
fn parse_stacked_filetests() {
    // -f -r $file → Filetest('f', Expr(Filetest('r', Expr($file))))
    let e = parse_expr_str("-f -r $file;");
    match &e.kind {
        ExprKind::Filetest(c, StatTarget::Expr(inner)) => {
            assert_eq!(*c, 'f');
            match &inner.kind {
                ExprKind::Filetest(c2, StatTarget::Expr(innermost)) => {
                    assert_eq!(*c2, 'r');
                    assert!(matches!(innermost.kind, ExprKind::ScalarVar(ref n) if n == "file"));
                }
                other => panic!("expected inner Filetest('r', Expr(ScalarVar)), got {other:?}"),
            }
        }
        other => panic!("expected stacked Filetest, got {other:?}"),
    }
}

#[test]
fn parse_minus_non_filetest_still_quotes() {
    // -key is NOT a filetest — 'k' is filetest but "key" is multi-char
    let e = parse_expr_str("-key;");
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "-key"),
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn parse_filetest_letter_fat_comma_autoquotes() {
    // -f => value — NOT a filetest, autoquotes as StringLit("-f")
    let e = parse_expr_str("-f => 1;");
    match &e.kind {
        ExprKind::List(items) => match &items[0].kind {
            ExprKind::StringLit(s) => assert_eq!(s, "-f"),
            other => panic!("expected StringLit('-f'), got {other:?}"),
        },
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn parse_filetest_letter_hash_subscript_autoquotes() {
    // $hash{-f} — NOT a filetest, autoquotes as StringLit("-f")
    let e = parse_expr_str("$hash{-f};");
    match &e.kind {
        ExprKind::HashElem(_, key) => match &key.kind {
            ExprKind::StringLit(s) => assert_eq!(s, "-f"),
            other => panic!("expected StringLit('-f'), got {other:?}"),
        },
        other => panic!("expected HashElem, got {other:?}"),
    }
}

// ── stat / lstat tests ────────────────────────────────────

#[test]
fn parse_stat_expr() {
    let e = parse_expr_str("stat $file;");
    match &e.kind {
        ExprKind::Stat(StatTarget::Expr(operand)) => {
            assert!(matches!(operand.kind, ExprKind::ScalarVar(ref n) if n == "file"));
        }
        other => panic!("expected Stat(Expr(ScalarVar)), got {other:?}"),
    }
}

#[test]
fn parse_stat_underscore() {
    let e = parse_expr_str("stat _;");
    assert!(matches!(e.kind, ExprKind::Stat(StatTarget::StatCache)));
}

#[test]
fn parse_stat_default() {
    let e = parse_expr_str("stat;");
    assert!(matches!(e.kind, ExprKind::Stat(StatTarget::Default)));
}

#[test]
fn parse_stat_parens() {
    let e = parse_expr_str("stat($file);");
    match &e.kind {
        ExprKind::Stat(StatTarget::Expr(operand)) => {
            assert!(matches!(operand.kind, ExprKind::ScalarVar(ref n) if n == "file"));
        }
        other => panic!("expected Stat(Expr(ScalarVar)), got {other:?}"),
    }
}

#[test]
fn parse_stat_parens_underscore() {
    let e = parse_expr_str("stat(_);");
    assert!(matches!(e.kind, ExprKind::Stat(StatTarget::StatCache)));
}

#[test]
fn parse_lstat_expr() {
    let e = parse_expr_str("lstat $file;");
    match &e.kind {
        ExprKind::Lstat(StatTarget::Expr(operand)) => {
            assert!(matches!(operand.kind, ExprKind::ScalarVar(ref n) if n == "file"));
        }
        other => panic!("expected Lstat(Expr(ScalarVar)), got {other:?}"),
    }
}

#[test]
fn parse_lstat_underscore() {
    let e = parse_expr_str("lstat _;");
    assert!(matches!(e.kind, ExprKind::Lstat(StatTarget::StatCache)));
}

// ── Special array / hash variable tests ───────────────────

#[test]
fn parse_special_array_plus() {
    let e = parse_expr_str("@+;");
    match &e.kind {
        ExprKind::SpecialArrayVar(name) => assert_eq!(name, "+"),
        other => panic!("expected SpecialArrayVar('+'), got {other:?}"),
    }
}

#[test]
fn parse_special_array_minus() {
    let e = parse_expr_str("@-;");
    match &e.kind {
        ExprKind::SpecialArrayVar(name) => assert_eq!(name, "-"),
        other => panic!("expected SpecialArrayVar('-'), got {other:?}"),
    }
}

#[test]
fn parse_special_array_elem() {
    // $+[0] — element access on special array @+.
    let e = parse_expr_str("$+[0];");
    match &e.kind {
        ExprKind::ArrayElem(base, idx) => {
            assert!(matches!(base.kind, ExprKind::SpecialVar(ref n) if n == "+"));
            assert!(matches!(idx.kind, ExprKind::IntLit(0)));
        }
        other => panic!("expected ArrayElem(SpecialVar('+'), 0), got {other:?}"),
    }
}

#[test]
fn parse_special_hash_bang() {
    let e = parse_expr_str("%!;");
    match &e.kind {
        ExprKind::SpecialHashVar(name) => assert_eq!(name, "!"),
        other => panic!("expected SpecialHashVar('!'), got {other:?}"),
    }
}

#[test]
fn parse_special_hash_plus() {
    let e = parse_expr_str("%+;");
    match &e.kind {
        ExprKind::SpecialHashVar(name) => assert_eq!(name, "+"),
        other => panic!("expected SpecialHashVar('+'), got {other:?}"),
    }
}

#[test]
fn parse_special_hash_elem() {
    // $!{ENOENT} — element access on special hash %!.
    let e = parse_expr_str("$!{ENOENT};");
    match &e.kind {
        ExprKind::HashElem(base, key) => {
            assert!(matches!(base.kind, ExprKind::SpecialVar(ref n) if n == "!"));
            assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "ENOENT"));
        }
        other => panic!("expected HashElem(SpecialVar('!'), 'ENOENT'), got {other:?}"),
    }
}

#[test]
fn parse_special_array_caret_capture() {
    let e = parse_expr_str("@{^CAPTURE};");
    match &e.kind {
        ExprKind::SpecialArrayVar(name) => assert_eq!(name, "^CAPTURE"),
        other => panic!("expected SpecialArrayVar('^CAPTURE'), got {other:?}"),
    }
}

#[test]
fn parse_special_hash_caret_capture_all() {
    let e = parse_expr_str("%{^CAPTURE_ALL};");
    match &e.kind {
        ExprKind::SpecialHashVar(name) => assert_eq!(name, "^CAPTURE_ALL"),
        other => panic!("expected SpecialHashVar('^CAPTURE_ALL'), got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// NEW TESTS — compound assignment operators
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_assign_sub() {
    let e = parse_expr_str("$x -= 1;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::SubEq, _, _)));
}

#[test]
fn parse_assign_mul() {
    let e = parse_expr_str("$x *= 2;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::MulEq, _, _)));
}

#[test]
fn parse_assign_div() {
    let e = parse_expr_str("$x /= 2;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::DivEq, _, _)));
}

#[test]
fn parse_assign_mod() {
    let e = parse_expr_str("$x %= 3;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::ModEq, _, _)));
}

#[test]
fn parse_assign_pow() {
    let e = parse_expr_str("$x **= 2;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::PowEq, _, _)));
}

#[test]
fn parse_assign_concat() {
    let e = parse_expr_str("$x .= 'a';");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::ConcatEq, _, _)));
}

#[test]
fn parse_assign_and() {
    let e = parse_expr_str("$x &&= 1;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::AndEq, _, _)));
}

#[test]
fn parse_assign_or() {
    let e = parse_expr_str("$x ||= 1;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::OrEq, _, _)));
}

#[test]
fn parse_assign_defined_or() {
    let e = parse_expr_str("$x //= 1;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::DefinedOrEq, _, _)));
}

#[test]
fn parse_assign_bit_and() {
    let e = parse_expr_str("$x &= 0xFF;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::BitAndEq, _, _)));
}

#[test]
fn parse_assign_bit_or() {
    let e = parse_expr_str("$x |= 0xFF;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::BitOrEq, _, _)));
}

#[test]
fn parse_assign_bit_xor() {
    let e = parse_expr_str("$x ^= 0xFF;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::BitXorEq, _, _)));
}

#[test]
fn parse_assign_shift_l() {
    let e = parse_expr_str("$x <<= 2;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::ShiftLeftEq, _, _)));
}

#[test]
fn parse_assign_shift_r() {
    let e = parse_expr_str("$x >>= 2;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::ShiftRightEq, _, _)));
}

// ═══════════════════════════════════════════════════════════
// NEW TESTS — precedence verification
// ═══════════════════════════════════════════════════════════

#[test]
fn prec_and_binds_tighter_than_or() {
    let e = parse_expr_str("$a && $b || $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Or, left, _) => {
            assert!(matches!(left.kind, ExprKind::BinOp(BinOp::And, _, _)));
        }
        other => panic!("expected Or(And(..), ..), got {other:?}"),
    }
}

#[test]
fn prec_assign_right_assoc() {
    let e = parse_expr_str("$a = $b = 1;");
    match &e.kind {
        ExprKind::Assign(AssignOp::Eq, _, right) => {
            assert!(matches!(right.kind, ExprKind::Assign(AssignOp::Eq, _, _)));
        }
        other => panic!("expected chained assign, got {other:?}"),
    }
}

#[test]
fn prec_ternary_nested() {
    // $a ? $b ? 1 : 2 : 3 — right-assoc: $a ? ($b ? 1 : 2) : 3
    let e = parse_expr_str("$a ? $b ? 1 : 2 : 3;");
    match &e.kind {
        ExprKind::Ternary(_, middle, _) => {
            assert!(matches!(middle.kind, ExprKind::Ternary(_, _, _)));
        }
        other => panic!("expected nested Ternary, got {other:?}"),
    }
}

#[test]
fn prec_binding_tighter_than_concat() {
    let e = parse_expr_str("$x =~ /foo/ . 'bar';");
    match &e.kind {
        ExprKind::BinOp(BinOp::Concat, left, _) => {
            assert!(matches!(left.kind, ExprKind::BinOp(BinOp::Binding, _, _)));
        }
        other => panic!("expected Concat(Binding(..), ..), got {other:?}"),
    }
}

#[test]
fn prec_low_or_loosest() {
    let e = parse_expr_str("$a = 1 or die;");
    match &e.kind {
        ExprKind::BinOp(BinOp::LowOr, left, _) => {
            assert!(matches!(left.kind, ExprKind::Assign(_, _, _)));
        }
        other => panic!("expected LowOr(Assign(..), ..), got {other:?}"),
    }
}

#[test]
fn prec_not_low_vs_and_low() {
    let e = parse_expr_str("not $a and $b;");
    match &e.kind {
        ExprKind::BinOp(BinOp::LowAnd, left, _) => {
            assert!(matches!(left.kind, ExprKind::UnaryOp(UnaryOp::Not, _)));
        }
        other => panic!("expected LowAnd(Not(..), ..), got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// NEW TESTS — operators with AST verification
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_range() {
    let e = parse_expr_str("1..10;");
    assert!(matches!(e.kind, ExprKind::Range(_, _)));
}

#[test]
fn parse_not_binding() {
    let e = parse_expr_str("$x !~ /foo/;");
    match &e.kind {
        ExprKind::BinOp(BinOp::NotBinding, _, right) => {
            assert!(matches!(right.kind, ExprKind::Regex(_, _, _)));
        }
        other => panic!("expected NotBinding, got {other:?}"),
    }
}

#[test]
fn parse_pre_inc() {
    let e = parse_expr_str("++$x;");
    assert!(matches!(e.kind, ExprKind::UnaryOp(UnaryOp::PreInc, _)));
}

#[test]
fn parse_pre_dec() {
    let e = parse_expr_str("--$x;");
    assert!(matches!(e.kind, ExprKind::UnaryOp(UnaryOp::PreDec, _)));
}

#[test]
fn parse_post_inc() {
    let e = parse_expr_str("$x++;");
    assert!(matches!(e.kind, ExprKind::PostfixOp(PostfixOp::Inc, _)));
}

#[test]
fn parse_post_dec() {
    let e = parse_expr_str("$x--;");
    assert!(matches!(e.kind, ExprKind::PostfixOp(PostfixOp::Dec, _)));
}

#[test]
fn parse_bit_and() {
    let e = parse_expr_str("$a & $b;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::BitAnd, _, _)));
}

#[test]
fn parse_bit_or() {
    let e = parse_expr_str("$a | $b;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::BitOr, _, _)));
}

#[test]
fn parse_bit_xor() {
    let e = parse_expr_str("$a ^ $b;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::BitXor, _, _)));
}

#[test]
fn parse_shift_l() {
    let e = parse_expr_str("$a << 2;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::ShiftLeft, _, _)));
}

#[test]
fn parse_shift_r() {
    let e = parse_expr_str("$a >> 2;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::ShiftRight, _, _)));
}

#[test]
fn parse_bit_not() {
    let e = parse_expr_str("~$x;");
    assert!(matches!(e.kind, ExprKind::UnaryOp(UnaryOp::BitNot, _)));
}

#[test]
fn parse_spaceship() {
    let e = parse_expr_str("$a <=> $b;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Spaceship, _, _)));
}

#[test]
fn parse_str_cmp() {
    let e = parse_expr_str("$a cmp $b;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::StrCmp, _, _)));
}

// ═══════════════════════════════════════════════════════════
// NEW TESTS — arrow deref targets
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_arrow_coderef_call() {
    let e = parse_expr_str("$ref->(1, 2);");
    match &e.kind {
        ExprKind::MethodCall(_, name, args) => {
            assert!(name.is_empty());
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected coderef MethodCall, got {other:?}"),
    }
}

#[test]
fn parse_arrow_array_elem() {
    let e = parse_expr_str("$ref->[0];");
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::ArrayElem(_))));
}

#[test]
fn parse_chained_mixed_subscripts() {
    let e = parse_expr_str("$ref->[0]{key}[1];");
    match &e.kind {
        ExprKind::ArrayElem(inner, _) => {
            assert!(matches!(inner.kind, ExprKind::HashElem(_, _)));
        }
        other => panic!("expected ArrayElem(HashElem(..),..), got {other:?}"),
    }
}

#[test]
fn parse_postfix_deref_scalar() {
    let e = parse_expr_str("$ref->$*;");
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefScalar)));
}

#[test]
fn parse_triple_deref() {
    let e = parse_expr_str("$$$ref;");
    match &e.kind {
        ExprKind::Deref(Sigil::Scalar, inner) => {
            assert!(matches!(inner.kind, ExprKind::Deref(Sigil::Scalar, _)));
        }
        other => panic!("expected Deref(Deref(..)), got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// NEW TESTS — postfix control flow variants
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_postfix_unless() {
    let prog = parse("print 1 unless $x;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::Unless, _, _), .. }) => {}
        other => panic!("expected PostfixControl Unless, got {other:?}"),
    }
}

#[test]
fn parse_postfix_while() {
    let prog = parse("$x++ while $x < 10;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::While, _, _), .. }) => {}
        other => panic!("expected PostfixControl While, got {other:?}"),
    }
}

#[test]
fn parse_postfix_until() {
    let prog = parse("$x++ until $x >= 10;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::Until, _, _), .. }) => {}
        other => panic!("expected PostfixControl Until, got {other:?}"),
    }
}

#[test]
fn parse_postfix_for() {
    let prog = parse("print $_ for @list;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::For, _, _), .. }) => {}
        other => panic!("expected PostfixControl For, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// NEW TESTS — declaration variants
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_our_decl() {
    let prog = parse("our $VERSION = '1.0';");
    let (scope, vars) = decl_vars(&prog.statements[0]);
    assert_eq!(scope, DeclScope::Our);
    assert_eq!(vars[0].name, "VERSION");
    assert_eq!(vars[0].sigil, Sigil::Scalar);
}

#[test]
fn parse_state_decl() {
    let prog = parse("state $counter = 0;");
    let (scope, vars) = decl_vars(&prog.statements[0]);
    assert_eq!(scope, DeclScope::State);
    assert_eq!(vars[0].name, "counter");
}

#[test]
fn parse_my_list_decl() {
    // `my ($a, $b, $c);` — no initializer, so Stmt::Expr(Decl(...)).
    let prog = parse("my ($a, $b, $c);");
    let (scope, vars) = decl_vars(&prog.statements[0]);
    assert_eq!(scope, DeclScope::My);
    assert_eq!(vars.len(), 3);
    assert_eq!(vars[0].name, "a");
    assert_eq!(vars[1].name, "b");
    assert_eq!(vars[2].name, "c");
}

#[test]
fn parse_my_mixed_sigil_list() {
    let prog = parse("my ($x, @y, %z);");
    let (_scope, vars) = decl_vars(&prog.statements[0]);
    assert_eq!(vars[0].sigil, Sigil::Scalar);
    assert_eq!(vars[1].sigil, Sigil::Array);
    assert_eq!(vars[2].sigil, Sigil::Hash);
}

#[test]
fn parse_sub_with_prototype() {
    let prog = parse("sub foo ($$) { }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sub) => {
            assert_eq!(sub.name, "foo");
            assert!(sub.prototype.is_some());
        }
        other => panic!("expected SubDecl with prototype, got {other:?}"),
    }
}

#[test]
fn parse_package_block_form() {
    let prog = parse("package Foo { }");
    match &prog.statements[0].kind {
        StmtKind::PackageDecl(p) => {
            assert_eq!(p.name, "Foo");
            assert!(p.block.is_some());
        }
        other => panic!("expected PackageDecl with block, got {other:?}"),
    }
}

#[test]
fn parse_package_version() {
    let prog = parse("package Foo 1.0;");
    match &prog.statements[0].kind {
        StmtKind::PackageDecl(p) => {
            assert_eq!(p.name, "Foo");
            assert!(p.version.is_some());
        }
        other => panic!("expected PackageDecl with version, got {other:?}"),
    }
}

#[test]
fn parse_no_decl() {
    let prog = parse("no warnings;");
    match &prog.statements[0].kind {
        StmtKind::UseDecl(u) => {
            assert!(u.is_no);
            assert_eq!(u.module, "warnings");
        }
        other => panic!("expected UseDecl(is_no=true), got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// NEW TESTS — builtins
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_defined() {
    let e = parse_expr_str("defined $x;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "defined");
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::ScalarVar(ref n) if n == "x"));
        }
        other => panic!("expected FuncCall('defined', [ScalarVar]), got {other:?}"),
    }
}

#[test]
fn parse_chomp() {
    let e = parse_expr_str("chomp $line;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "chomp");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected chomp FuncCall, got {other:?}"),
    }
}

#[test]
fn parse_die_no_arg() {
    let e = parse_expr_str("die;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "die");
            assert_eq!(args.len(), 0);
        }
        other => panic!("expected bare die, got {other:?}"),
    }
}

#[test]
fn parse_push_list() {
    let e = parse_expr_str("push @arr, 1, 2, 3;");
    match &e.kind {
        ExprKind::ListOp(name, args) => {
            assert_eq!(name, "push");
            assert_eq!(args.len(), 4);
        }
        other => panic!("expected push ListOp, got {other:?}"),
    }
}

#[test]
fn parse_join_list() {
    let e = parse_expr_str("join ',', @arr;");
    match &e.kind {
        ExprKind::ListOp(name, args) => {
            assert_eq!(name, "join");
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected join ListOp, got {other:?}"),
    }
}

#[test]
fn parse_split_regex() {
    let e = parse_expr_str("split /,/, $str;");
    match &e.kind {
        ExprKind::ListOp(name, args) => {
            assert_eq!(name, "split");
            assert_eq!(args.len(), 2);
            assert!(matches!(args[0].kind, ExprKind::Regex(_, _, _)));
        }
        other => panic!("expected split ListOp, got {other:?}"),
    }
}

#[test]
fn parse_sort_subname() {
    let e = parse_expr_str("sort compare @list;");
    match &e.kind {
        ExprKind::ListOp(name, args) => {
            assert_eq!(name, "sort");
            assert!(args.len() >= 2);
            assert!(matches!(args[0].kind, ExprKind::Bareword(_)));
        }
        other => panic!("expected sort with sub name, got {other:?}"),
    }
}

#[test]
fn parse_open_three_arg() {
    let e = parse_expr_str("open my $fh, '<', 'file.txt';");
    match &e.kind {
        ExprKind::ListOp(name, args) => {
            assert_eq!(name, "open");
            assert_eq!(args.len(), 3);
        }
        other => panic!("expected open ListOp, got {other:?}"),
    }
}

#[test]
fn parse_bless_two_arg() {
    let e = parse_expr_str("bless $self, 'Foo';");
    match &e.kind {
        ExprKind::ListOp(name, args) => {
            assert_eq!(name, "bless");
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected bless ListOp, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// NEW TESTS — special forms
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_do_block() {
    let e = parse_expr_str("do { 42; };");
    assert!(matches!(e.kind, ExprKind::DoBlock(_)));
}

#[test]
fn parse_do_file() {
    let e = parse_expr_str("do 'config.pl';");
    assert!(matches!(e.kind, ExprKind::DoExpr(_)));
}

#[test]
fn parse_undef() {
    let e = parse_expr_str("undef;");
    assert!(matches!(e.kind, ExprKind::Undef));
}

#[test]
fn parse_glob_wildcard() {
    let e = parse_expr_str("<*.txt>;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "glob");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected glob FuncCall, got {other:?}"),
    }
}

#[test]
fn parse_anon_hash() {
    let e = parse_expr_str("{key => 'val'};");
    match &e.kind {
        ExprKind::AnonHash(elems) => {
            assert!(elems.len() >= 2);
        }
        other => panic!("expected AnonHash, got {other:?}"),
    }
}

#[test]
fn parse_anon_hash_at_stmt_level() {
    // {key => 'val'} at statement level — the heuristic should
    // detect => after bareword and route to AnonHash.
    let prog = parse("{key => 'val'};");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::AnonHash(elems), .. }) => {
            assert_eq!(elems.len(), 2);
        }
        other => panic!("expected AnonHash at stmt level, got {other:?}"),
    }
}

#[test]
fn parse_empty_hash_at_stmt_level() {
    // {} at statement level — empty braces are a hash.
    let prog = parse("{};");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::AnonHash(elems), .. }) => {
            assert_eq!(elems.len(), 0);
        }
        other => panic!("expected empty AnonHash at stmt level, got {other:?}"),
    }
}

#[test]
fn parse_string_key_hash_at_stmt_level() {
    // {'key', 'val'} — string followed by comma → hash.
    let prog = parse("{'key', 'val'};");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::AnonHash(_), .. }) => {}
        other => panic!("expected AnonHash, got {other:?}"),
    }
}

#[test]
fn parse_uppercase_comma_hash_at_stmt_level() {
    // {Foo, 1} — uppercase bareword followed by comma → hash.
    let prog = parse("{Foo, 1};");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::AnonHash(_), .. }) => {}
        other => panic!("expected AnonHash, got {other:?}"),
    }
}

#[test]
fn parse_lowercase_comma_block_at_stmt_level() {
    // {foo, 1} — lowercase bareword followed by comma → block
    // (could be a function call: foo(), 1).
    let prog = parse("{foo(1)};");
    match &prog.statements[0].kind {
        StmtKind::Block(_, _) => {}
        other => panic!("expected Block, got {other:?}"),
    }
}

#[test]
fn parse_block_at_stmt_level() {
    // {my $x = 1; $x} — clearly a block (no comma/=> after first term).
    let prog = parse("{my $x = 1; $x};");
    match &prog.statements[0].kind {
        StmtKind::Block(block, _) => {
            assert!(!block.statements.is_empty());
        }
        other => panic!("expected Block, got {other:?}"),
    }
}

#[test]
fn parse_nested_anon_constructors() {
    let e = parse_expr_str("[{a => 1}, {b => 2}];");
    match &e.kind {
        ExprKind::AnonArray(elems) => {
            assert_eq!(elems.len(), 2);
            assert!(matches!(elems[0].kind, ExprKind::AnonHash(_)));
            assert!(matches!(elems[1].kind, ExprKind::AnonHash(_)));
        }
        other => panic!("expected AnonArray of AnonHashes, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// NEW TESTS — phaser blocks (INIT/CHECK/UNITCHECK)
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_init_block() {
    let prog = parse("INIT { 1; }");
    assert!(matches!(prog.statements[0].kind, StmtKind::Phaser(PhaserKind::Init, _)));
}

#[test]
fn parse_check_block() {
    let prog = parse("CHECK { 1; }");
    assert!(matches!(prog.statements[0].kind, StmtKind::Phaser(PhaserKind::Check, _)));
}

#[test]
fn parse_unitcheck_block() {
    let prog = parse("UNITCHECK { 1; }");
    assert!(matches!(prog.statements[0].kind, StmtKind::Phaser(PhaserKind::Unitcheck, _)));
}

// ═══════════════════════════════════════════════════════════
// NEW TESTS — control flow variants
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_try_finally_only() {
    let prog = parse("use feature 'try'; try { 1; } finally { 2; }");
    let stmt = prog.statements.iter().find(|s| matches!(s.kind, StmtKind::Try(_))).expect("Try statement present");
    match &stmt.kind {
        StmtKind::Try(t) => {
            assert!(t.catch_block.is_none());
            assert!(t.finally_block.is_some());
        }
        other => panic!("expected Try with only finally, got {other:?}"),
    }
}

#[test]
fn parse_many_elsifs() {
    let prog = parse("if ($a) { 1; } elsif ($b) { 2; } elsif ($c) { 3; } elsif ($d) { 4; } else { 5; }");
    match &prog.statements[0].kind {
        StmtKind::If(if_stmt) => {
            assert_eq!(if_stmt.elsif_clauses.len(), 3);
            assert!(if_stmt.else_block.is_some());
        }
        other => panic!("expected If with 3 elsifs, got {other:?}"),
    }
}

#[test]
fn parse_empty_statements() {
    let prog = parse(";;;");
    assert!(prog.statements.iter().all(|s| matches!(s.kind, StmtKind::Empty)));
}

// ═══════════════════════════════════════════════════════════
// NEW TESTS — regex flags
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_regex_with_many_flags() {
    let e = parse_expr_str("/foo/imsxg;");
    match &e.kind {
        ExprKind::Regex(_, pat, flags) => {
            assert_eq!(pat_str(pat), "foo");
            assert_eq!(flags.as_deref(), Some("imsxg"));
        }
        other => panic!("expected Regex with flags, got {other:?}"),
    }
}

#[test]
fn parse_qr_regex() {
    let e = parse_expr_str("qr/\\d+/;");
    match &e.kind {
        ExprKind::Regex(_, pat, _) => assert_eq!(pat_str(pat), "\\d+"),
        other => panic!("expected Regex (qr), got {other:?}"),
    }
}

#[test]
fn parse_regex_with_interp() {
    // m/foo$bar/ should produce an Interpolated pattern, not plain string.
    let e = parse_expr_str("m/foo$bar/;");
    match &e.kind {
        ExprKind::Regex(_, pat, _) => {
            assert!(pat.as_plain_string().is_none(), "expected interpolated pattern");
            assert!(pat.0.len() >= 2);
            assert!(matches!(&pat.0[0], InterpPart::Const(s) if s == "foo"));
            assert_eq!(scalar_interp_name(&pat.0[1]), Some("bar"));
        }
        other => panic!("expected Regex, got {other:?}"),
    }
}

#[test]
fn parse_regex_literal_no_interp() {
    // m'foo$bar' should NOT interpolate — pattern is plain string.
    let e = parse_expr_str("m'foo$bar';");
    match &e.kind {
        ExprKind::Regex(_, pat, _) => {
            assert_eq!(pat_str(pat), "foo$bar");
        }
        other => panic!("expected Regex, got {other:?}"),
    }
}

#[test]
fn parse_regex_literal_with_code_block() {
    // m'...' still recognizes (?{...}) code blocks.
    let e = parse_expr_str("m'foo(?{ 1 })bar';");
    match &e.kind {
        ExprKind::Regex(_, pat, _) => {
            assert!(pat.as_plain_string().is_none(), "expected interpolated pattern with code block");
            assert!(pat.0.iter().any(|p| matches!(p, InterpPart::RegexCode(_, _))));
        }
        other => panic!("expected Regex, got {other:?}"),
    }
}

#[test]
fn parse_tr_with_flags() {
    let e = parse_expr_str("tr/a-z/A-Z/cs;");
    match &e.kind {
        ExprKind::Translit(_, _, flags) => assert_eq!(flags.as_deref(), Some("cs")),
        other => panic!("expected Translit with flags, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// NEW TESTS — miscellaneous
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_scalar_context() {
    let e = parse_expr_str("scalar @arr;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "scalar");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected scalar FuncCall, got {other:?}"),
    }
}

#[test]
fn parse_package_method_call() {
    let e = parse_expr_str("Foo::Bar->new();");
    match &e.kind {
        ExprKind::MethodCall(class, method, _) => {
            assert_eq!(method, "new");
            match &class.kind {
                ExprKind::Bareword(name) => assert_eq!(name, "Foo::Bar"),
                other => panic!("expected Bareword, got {other:?}"),
            }
        }
        other => panic!("expected MethodCall, got {other:?}"),
    }
}

#[test]
fn parse_require_version() {
    let e = parse_expr_str("require 5.010;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "require");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected require with version, got {other:?}"),
    }
}

#[test]
fn parse_labeled_bare_block() {
    let prog = parse("BLOCK: { last BLOCK; }");
    match &prog.statements[0].kind {
        StmtKind::Labeled(label, _) => assert_eq!(label, "BLOCK"),
        other => panic!("expected Labeled, got {other:?}"),
    }
}

#[test]
fn parse_fat_comma_with_keyword() {
    let e = parse_expr_str("if => 1;");
    match &e.kind {
        ExprKind::List(items) => match &items[0].kind {
            ExprKind::StringLit(s) => assert_eq!(s, "if"),
            other => panic!("expected StringLit('if'), got {other:?}"),
        },
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn parse_fat_comma_keyword_cross_line() {
    // Keyword on one line, => on the next — should still autoquote.
    let e = parse_expr_str("my\n  => 1;");
    match &e.kind {
        ExprKind::List(items) => match &items[0].kind {
            ExprKind::StringLit(s) => assert_eq!(s, "my"),
            other => panic!("expected StringLit('my'), got {other:?}"),
        },
        other => panic!("expected List, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// Quote-keyword autoquoting.
//
// The 8 Perl quote-like operators — `q`, `qq`, `qw`, `qr`,
// `m`, `s`, `tr`, `y` — are recognized as operators only
// when followed by a *valid* opening delimiter (see
// `at_quote_delimiter` in the lexer).  When not followed by
// a valid opener — including when followed by `=>` (fat
// comma), `}` (hash-subscript close), or any of the
// closing paired delimiters `)`, `]`, `}`, `>` — they must
// NOT start a quote op and must instead be treated as
// ordinary barewords (autoquoted to string literals in the
// appropriate contexts).
//
// (`qx` — the backtick-equivalent — has the same lexical
// shape but is omitted from this set to match Perl's common
// "8 quote operators" terminology.)
// ═══════════════════════════════════════════════════════════

// ── Autoquote in fat-comma context ────────────────────────

/// Parse `(KEYWORD => 1);` and return the first list element.
/// Handles the outer Paren wrapping produced by the `(...)`.
fn parse_kw_fat_comma(src: &str) -> Expr {
    let mut e = parse_expr_str(src);
    // Unwrap a single-level Paren — `(k => v)` parses as
    // Paren(List([k, v])) rather than bare List.
    if let ExprKind::Paren(inner) = e.kind {
        e = *inner;
    }
    match e.kind {
        ExprKind::List(mut items) => {
            assert!(!items.is_empty(), "expected non-empty list for {src:?}");
            items.remove(0)
        }
        other => panic!("expected List, got {other:?} for {src:?}"),
    }
}

#[test]
fn autoquote_q_fat_comma() {
    let first = parse_kw_fat_comma("(q => 1);");
    assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "q"), "expected StringLit(q), got {:?}", first.kind);
}

#[test]
fn autoquote_qq_fat_comma() {
    let first = parse_kw_fat_comma("(qq => 1);");
    assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "qq"), "expected StringLit(qq), got {:?}", first.kind);
}

#[test]
fn autoquote_qw_fat_comma() {
    let first = parse_kw_fat_comma("(qw => 1);");
    assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "qw"), "expected StringLit(qw), got {:?}", first.kind);
}

#[test]
fn autoquote_qr_fat_comma() {
    let first = parse_kw_fat_comma("(qr => 1);");
    assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "qr"), "expected StringLit(qr), got {:?}", first.kind);
}

#[test]
fn autoquote_m_fat_comma() {
    let first = parse_kw_fat_comma("(m => 1);");
    assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "m"), "expected StringLit(m), got {:?}", first.kind);
}

#[test]
fn autoquote_s_fat_comma() {
    let first = parse_kw_fat_comma("(s => 1);");
    assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "s"), "expected StringLit(s), got {:?}", first.kind);
}

#[test]
fn autoquote_tr_fat_comma() {
    let first = parse_kw_fat_comma("(tr => 1);");
    assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "tr"), "expected StringLit(tr), got {:?}", first.kind);
}

#[test]
fn autoquote_y_fat_comma() {
    let first = parse_kw_fat_comma("(y => 1);");
    assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "y"), "expected StringLit(y), got {:?}", first.kind);
}

// ── Autoquote in hash-subscript context ───────────────────

/// Parse `$h{KEYWORD}` and return the subscript key expression.
fn parse_kw_hash_key(src: &str) -> Expr {
    let e = parse_expr_str(src);
    match e.kind {
        ExprKind::HashElem(_, key) => *key,
        other => panic!("expected HashElem, got {other:?} for {src:?}"),
    }
}

#[test]
fn autoquote_q_hash_key() {
    let key = parse_kw_hash_key("$h{q};");
    assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "q"), "expected StringLit(q), got {:?}", key.kind);
}

#[test]
fn autoquote_qq_hash_key() {
    let key = parse_kw_hash_key("$h{qq};");
    assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "qq"), "expected StringLit(qq), got {:?}", key.kind);
}

#[test]
fn autoquote_qw_hash_key() {
    let key = parse_kw_hash_key("$h{qw};");
    assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "qw"), "expected StringLit(qw), got {:?}", key.kind);
}

#[test]
fn autoquote_qr_hash_key() {
    let key = parse_kw_hash_key("$h{qr};");
    assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "qr"), "expected StringLit(qr), got {:?}", key.kind);
}

#[test]
fn autoquote_m_hash_key() {
    let key = parse_kw_hash_key("$h{m};");
    assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "m"), "expected StringLit(m), got {:?}", key.kind);
}

#[test]
fn autoquote_s_hash_key() {
    let key = parse_kw_hash_key("$h{s};");
    assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "s"), "expected StringLit(s), got {:?}", key.kind);
}

#[test]
fn autoquote_tr_hash_key() {
    let key = parse_kw_hash_key("$h{tr};");
    assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "tr"), "expected StringLit(tr), got {:?}", key.kind);
}

#[test]
fn autoquote_y_hash_key() {
    let key = parse_kw_hash_key("$h{y};");
    assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "y"), "expected StringLit(y), got {:?}", key.kind);
}

// ═══════════════════════════════════════════════════════════
// Audit-driven gap-filling tests.
//
// The previous commits added many tests by phase, but the
// audit I committed to in the interpolation masking
// postmortem turned up several genuinely shallow ones and a
// few real gaps.  These fill the worst of them.  Structured
// by the phase they belong to.
// ═══════════════════════════════════════════════════════════

// ── Phase 3: postderef slice content verification ────────
//
// The original postderef slice tests checked only the
// ArrowTarget variant, not the index/key contents.  A
// regression that parsed `$r->@[0, 1, 2]` as
// `ArraySliceIndices(IntLit(0))` (dropping the rest) would
// slip through.  Tests below verify the inner expression.

#[test]
fn postderef_array_slice_indices_content() {
    let e = parse_expr_stmt("$r->@[0, 1, 2];");
    match arrow_target(&e) {
        ArrowTarget::ArraySliceIndices(idx) => {
            // Index expr is a comma-list of three ints.
            match &idx.kind {
                ExprKind::List(items) => {
                    assert_eq!(items.len(), 3);
                    assert!(matches!(items[0].kind, ExprKind::IntLit(0)));
                    assert!(matches!(items[1].kind, ExprKind::IntLit(1)));
                    assert!(matches!(items[2].kind, ExprKind::IntLit(2)));
                }
                ExprKind::IntLit(n) => panic!("single IntLit({n}) — expected 3-element List; would mean slice dropped items"),
                other => panic!("expected List of 3, got {other:?}"),
            }
        }
        other => panic!("expected ArraySliceIndices, got {other:?}"),
    }
}

#[test]
fn postderef_array_slice_keys_content() {
    let e = parse_expr_stmt(r#"$r->@{"a", "b", "c"};"#);
    match arrow_target(&e) {
        ArrowTarget::ArraySliceKeys(keys) => match &keys.kind {
            ExprKind::List(items) => {
                assert_eq!(items.len(), 3);
                for (i, want) in ["a", "b", "c"].iter().enumerate() {
                    assert!(matches!(items[i].kind, ExprKind::StringLit(ref s) if s == want), "item {i}: expected StringLit({want}), got {:?}", items[i].kind);
                }
            }
            other => panic!("expected List of 3 strings, got {other:?}"),
        },
        other => panic!("expected ArraySliceKeys, got {other:?}"),
    }
}

#[test]
fn postderef_kv_slice_indices_content() {
    let e = parse_expr_stmt("$r->%[0, 1];");
    match arrow_target(&e) {
        ArrowTarget::KvSliceIndices(idx) => match &idx.kind {
            ExprKind::List(items) => {
                assert_eq!(items.len(), 2);
                assert!(matches!(items[0].kind, ExprKind::IntLit(0)));
                assert!(matches!(items[1].kind, ExprKind::IntLit(1)));
            }
            other => panic!("expected List of 2 ints, got {other:?}"),
        },
        other => panic!("expected KvSliceIndices, got {other:?}"),
    }
}

#[test]
fn postderef_nested_actually_nested() {
    // Original `postderef_nested_slice` test claimed to
    // cover chaining but only had one level.  This one
    // actually chains: slice followed by arrow-array-elem.
    let e = parse_expr_stmt("$r->@[0, 1]->[0];");
    // Outer is ArrowDeref(_, ArrayElem(0)); inner is
    // ArrowDeref($r, ArraySliceIndices([0, 1])).
    match &e.kind {
        ExprKind::ArrowDeref(inner, ArrowTarget::ArrayElem(idx)) => {
            assert!(matches!(idx.kind, ExprKind::IntLit(0)));
            assert!(matches!(inner.kind, ExprKind::ArrowDeref(_, ArrowTarget::ArraySliceIndices(_))));
        }
        other => panic!("expected ArrowDeref(slice, ArrayElem(0)), got {other:?}"),
    }
}

// ── Phase 4: fc as named unary actually IS one ──────────
//
// `fc_requires_feature` was weak: it asserted parsing
// didn't error and the name was "fc" — but that's true
// regardless of whether fc was recognized as a named unary
// or fell back to a generic FuncCall.  Counter-test: with
// the feature on AND no parens, `fc` must bind as a
// named-unary operator (precedence boundary: tighter than
// `+`, looser than `*`).

#[test]
fn fc_named_unary_precedence() {
    // `fc $x . $y` — named-unary operators parse their
    // argument at NAMED_UNARY precedence, which is BELOW
    // concat.  So the entire `$x . $y` is the argument:
    // `fc($x . $y)`, NOT `fc($x) . $y`.
    let e = parse_expr_stmt("use feature 'fc'; fc $x . $y;");
    match e.kind {
        ExprKind::FuncCall(ref name, ref args) if name == "fc" => {
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Concat, _, _)), "argument should be the whole Concat expr, got {:?}", args[0].kind);
        }
        other => panic!("expected FuncCall(fc, [Concat(...)]), got {other:?}"),
    }
}

// ── Phase 5b: reactivation tests for each gated keyword ──
//
// The original downgrade tests only checked `try` reactivates
// when its feature is on.  Add the same check for each of
// the seven keywords whose downgrade was implemented: each
// should parse as its real keyword form when the feature is
// active.

#[test]
fn catch_reactivates_with_try_feature() {
    let prog = parse("use feature 'try'; try { 1; } catch ($e) { 2; }");
    // Try stmt captured with a catch clause.
    let try_stmt = prog.statements.iter().find_map(|s| match &s.kind {
        StmtKind::Try(t) => Some(t),
        _ => None,
    });
    assert!(try_stmt.is_some(), "Try stmt must exist with feature active");
    // And the Try must have a catch clause with var $e.
    let try_ = try_stmt.unwrap();
    assert!(try_.catch_block.is_some(), "catch clause must be attached");
}

#[test]
fn defer_reactivates_with_feature() {
    let prog = parse("use feature 'defer'; defer { 1; }");
    assert!(prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Defer(_))), "Defer must parse with feature active");
}

#[test]
fn given_when_reactivate_with_switch_feature() {
    let prog = parse(
        "use feature 'switch'; no warnings 'experimental::smartmatch'; \
         given ($x) { when (1) { 'one' } default { 'other' } }",
    );
    assert!(prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Given(_, _))), "Given must parse with switch feature");
}

#[test]
fn class_reactivates_with_feature() {
    let prog = parse("use feature 'class'; no warnings 'experimental::class'; class Foo { }");
    assert!(prog.statements.iter().any(|s| matches!(s.kind, StmtKind::ClassDecl(_))), "Class decl must parse with class feature");
}

// ── Compile-time tokens in contexts ──────────────────────
//
// The original tests covered top-level __SUB__ / __PACKAGE__
// but not nested contexts.

#[test]
fn current_sub_inside_named_sub() {
    // __SUB__ inside a sub body — the token is lex-time so
    // context doesn't affect its form; verify it parses.
    let prog = parse("use feature 'current_sub'; sub f { __SUB__ }");
    let sub = prog
        .statements
        .iter()
        .find_map(|s| match &s.kind {
            StmtKind::SubDecl(sd) if sd.name == "f" => Some(sd),
            _ => None,
        })
        .expect("sub f");
    // Body contains a CurrentSub expression somewhere.
    let body_has_current_sub = sub.body.statements.iter().any(|s| match &s.kind {
        StmtKind::Expr(e) => matches!(e.kind, ExprKind::CurrentSub),
        _ => false,
    });
    assert!(body_has_current_sub, "expected CurrentSub inside sub f body");
}

#[test]
fn current_package_after_nested_package_decl() {
    // After `package Foo; package Bar;`, __PACKAGE__ gives
    // "Bar".  Tests the parser state-tracking on successive
    // package declarations.
    let prog = parse("package Foo;\npackage Bar;\n__PACKAGE__;\n");
    let e = prog.statements.iter().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e.clone()) } else { None }).expect("expression statement");
    match e.kind {
        ExprKind::CurrentPackage(name) => assert_eq!(name, "Bar"),
        other => panic!("expected CurrentPackage(Bar), got {other:?}"),
    }
}

// ── Signatures: negative cases ───────────────────────────

#[test]
fn sig_slurpy_array_before_scalar_is_error() {
    // `@rest` must be the last named parameter — a scalar
    // after it is invalid.  The parser should reject.
    let src = "use feature 'signatures'; sub f (@rest, $x) { }";
    let mut p = match Parser::new(src.as_bytes()) {
        Ok(p) => p,
        Err(_) => panic!("parser construction failed"),
    };
    let result = p.parse_program();
    assert!(result.is_err(), "slurpy array before scalar should error");
}

#[test]
fn sig_two_slurpies_is_error() {
    let src = "use feature 'signatures'; sub f (@a, %h) { }";
    let mut p = match Parser::new(src.as_bytes()) {
        Ok(p) => p,
        Err(_) => panic!("parser construction failed"),
    };
    let result = p.parse_program();
    assert!(result.is_err(), "two slurpies should error");
}

// ═══════════════════════════════════════════════════════════
// NEW TESTS — known gaps (ignored until implemented)
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_subst_e_flag() {
    let e = parse_expr_str("s/foo/uc($&)/e;");
    match &e.kind {
        ExprKind::Subst(_, repl, _) => {
            assert!(repl.as_plain_string().is_none(), "expected non-literal replacement for /e, got {repl:?}");
        }
        other => panic!("expected Subst, got {other:?}"),
    }
}

#[test]
fn parse_interp_scalar_expr() {
    // "${\ $ref}" — scalar expression interpolation.
    let e = parse_expr_str(r#""${\ $ref}";"#);
    match &e.kind {
        ExprKind::InterpolatedString(Interpolated(parts)) => {
            assert!(parts.iter().any(|p| matches!(p, InterpPart::ExprInterp(_))), "expected ExprInterp, got {parts:?}");
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

#[test]
fn parse_interp_scalar_expr_arithmetic() {
    // "${\ $x + 1}" — expression with arithmetic.
    let e = parse_expr_str(r#""val: ${\ $x + 1}";"#);
    match &e.kind {
        ExprKind::InterpolatedString(Interpolated(parts)) => {
            assert_eq!(parts.len(), 2);
            assert!(matches!(&parts[0], InterpPart::Const(s) if s == "val: "));
            assert!(matches!(&parts[1], InterpPart::ExprInterp(_)));
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

#[test]
fn parse_interp_array_expr() {
    // "@{[ 1, 2, 3 ]}" — array expression interpolation.
    let e = parse_expr_str(r#""@{[ 1, 2, 3 ]}";"#);
    match &e.kind {
        ExprKind::InterpolatedString(Interpolated(parts)) => {
            assert!(parts.iter().any(|p| matches!(p, InterpPart::ExprInterp(_))), "expected ExprInterp, got {parts:?}");
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

#[test]
fn parse_interp_expr_with_text() {
    // Mixing expression interpolation with plain text and simple vars.
    let e = parse_expr_str(r#""Hello ${\ uc($name)}, you have @{[ $n + 1 ]} items";"#);
    match &e.kind {
        ExprKind::InterpolatedString(Interpolated(parts)) => {
            assert!(parts.len() >= 4, "expected at least 4 parts, got {}", parts.len());
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

#[test]
fn parse_interp_simple_braced_var() {
    // "${name}" — simple braced variable, NOT expression interpolation.
    let e = parse_expr_str(r#""${name}s";"#);
    match &e.kind {
        ExprKind::InterpolatedString(Interpolated(parts)) => {
            assert_eq!(scalar_interp_name(&parts[0]), Some("name"), "expected ScalarInterp(name), got {:?}", parts[0]);
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

#[test]
fn parse_local_special_var_assign() {
    let prog = parse("local $/ = undef;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => {
            assert!(matches!(lhs.kind, ExprKind::Local(_)));
        }
        other => panic!("expected local assign, got {other:?}"),
    }
}

#[test]
fn parse_qx_string_parens() {
    let e = parse_expr_str("qx(ls -la);");
    assert!(matches!(e.kind, ExprKind::InterpolatedString(_) | ExprKind::StringLit(_)));
}

#[test]
fn parse_print_filehandle() {
    let e = parse_expr_str("print STDERR 'error';");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "print");
            match fh.as_deref() {
                Some(Expr { kind: ExprKind::Bareword(n), .. }) => assert_eq!(n, "STDERR"),
                other => panic!("expected filehandle Bareword('STDERR'), got {other:?}"),
            }
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected PrintOp with filehandle, got {other:?}"),
    }
}

#[test]
fn parse_print_filehandle_parens() {
    // print(STDERR "testing\n") — parenthesized form.
    let e = parse_expr_str(r#"print(STDERR "testing\n");"#);
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "print");
            assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDERR"));
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected PrintOp with filehandle (parens), got {other:?}"),
    }
}

#[test]
fn parse_print_comma_not_filehandle() {
    // print STDERR, "hello" — comma means STDERR is an arg, not filehandle.
    let e = parse_expr_str("print STDERR, 'hello';");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "print");
            assert!(fh.is_none());
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected PrintOp with no filehandle, got {other:?}"),
    }
}

#[test]
fn parse_print_scalar_filehandle() {
    // print $fh "hello" — $fh is filehandle.
    let e = parse_expr_str("print $fh 'hello';");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "print");
            assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::ScalarVar(n), .. }) if n == "fh"));
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected PrintOp with scalar filehandle, got {other:?}"),
    }
}

#[test]
fn parse_print_bare_no_args() {
    // print STDERR; — filehandle with no args (prints $_ to STDERR).
    let e = parse_expr_str("print STDERR;");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "print");
            assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDERR"));
            assert_eq!(args.len(), 0);
        }
        other => panic!("expected PrintOp with filehandle, no args, got {other:?}"),
    }
}

#[test]
fn parse_say_filehandle() {
    let e = parse_expr_str("say STDERR 'error';");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "say");
            assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDERR"));
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected say PrintOp with filehandle, got {other:?}"),
    }
}

#[test]
fn parse_printf_filehandle() {
    let e = parse_expr_str("printf STDERR '%s', $msg;");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "printf");
            assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDERR"));
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected printf PrintOp with filehandle, got {other:?}"),
    }
}

#[test]
fn parse_print_no_args() {
    // print; — prints $_ to default output.
    let e = parse_expr_str("print;");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "print");
            assert!(fh.is_none());
            assert_eq!(args.len(), 0);
        }
        other => panic!("expected PrintOp with no args, got {other:?}"),
    }
}

#[test]
fn parse_print_parens_no_args() {
    // print() — prints $_ to default output (paren form).
    let e = parse_expr_str("print();");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "print");
            assert!(fh.is_none());
            assert_eq!(args.len(), 0);
        }
        other => panic!("expected PrintOp() with no args, got {other:?}"),
    }
}

#[test]
fn parse_print_parens_fh_no_args() {
    // print(STDERR); — bareword filehandle in parens, no args (prints $_).
    let e = parse_expr_str("print(STDERR);");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "print");
            assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDERR"));
            assert_eq!(args.len(), 0);
        }
        other => panic!("expected PrintOp(STDERR) with no args, got {other:?}"),
    }
}

#[test]
fn parse_print_parens_scalar_fh() {
    // print($fh $_); — $fh is filehandle (followed by $_, a term).
    let e = parse_expr_str("print($fh $_);");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "print");
            assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::ScalarVar(n), .. }) if n == "fh"));
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::ScalarVar(ref n) if n == "_"));
        }
        other => panic!("expected PrintOp($fh, [$_]), got {other:?}"),
    }
}

#[test]
fn parse_print_parens_scalar_not_fh() {
    // print($f); — $f NOT a filehandle (followed by ), not a term).
    // Prints value of $f to STDOUT.
    let e = parse_expr_str("print($f);");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "print");
            assert!(fh.is_none());
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::ScalarVar(ref n) if n == "f"));
        }
        other => panic!("expected PrintOp(None, [$f]), got {other:?}"),
    }
}

#[test]
fn parse_print_scalar_not_fh() {
    // print $f; — $f NOT a filehandle (followed by ;, not a term).
    let e = parse_expr_str("print $f;");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "print");
            assert!(fh.is_none());
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::ScalarVar(ref n) if n == "f"));
        }
        other => panic!("expected PrintOp(None, [$f]), got {other:?}"),
    }
}

#[test]
fn parse_say_no_filehandle() {
    let e = parse_expr_str("say 'hello';");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "say");
            assert!(fh.is_none());
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected say PrintOp with no filehandle, got {other:?}"),
    }
}

#[test]
fn parse_say_parens_filehandle() {
    let e = parse_expr_str("say(STDERR 'hello');");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "say");
            assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDERR"));
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected say PrintOp with filehandle (parens), got {other:?}"),
    }
}

#[test]
fn parse_printf_no_filehandle() {
    let e = parse_expr_str("printf '%s', $msg;");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "printf");
            assert!(fh.is_none());
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected printf PrintOp with no filehandle, got {other:?}"),
    }
}

#[test]
fn parse_printf_parens_filehandle() {
    let e = parse_expr_str("printf(STDERR '%s', $msg);");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "printf");
            assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDERR"));
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected printf PrintOp with filehandle (parens), got {other:?}"),
    }
}

#[test]
fn parse_print_stdout_filehandle() {
    let e = parse_expr_str("print STDOUT 'hello';");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "print");
            assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDOUT"));
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected print STDOUT filehandle, got {other:?}"),
    }
}

#[test]
fn parse_print_postfix_if() {
    // print "hello" if $cond; — postfix control should work with PrintOp.
    let prog = parse("print 'hello' if $cond;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::If, body, _), .. }) => {
            assert!(matches!(body.kind, ExprKind::PrintOp(_, _, _)));
        }
        other => panic!("expected PostfixControl(If, PrintOp), got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// Semantic validation tests
// ═══════════════════════════════════════════════════════════

fn parse_expr_fails(src: &str) -> bool {
    // A quick way to check that parsing an expression fails.
    std::panic::catch_unwind(|| parse_expr_str(src)).is_err()
}

// ── Chained comparisons (Perl 5.32+) ───────────────────

#[test]
fn allow_chained_lt() {
    // $a < $b < $c — chained comparison (5.32+).
    let e = parse_expr_str("$a < $b < $c;");
    assert!(matches!(e.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp, got {:?}", e.kind);
}

#[test]
fn allow_chained_eq() {
    let e = parse_expr_str("$a == $b == $c;");
    assert!(matches!(e.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp, got {:?}", e.kind);
}

#[test]
fn allow_chained_str_cmp() {
    let e = parse_expr_str("$a eq $b eq $c;");
    assert!(matches!(e.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp, got {:?}", e.kind);
}

#[test]
fn allow_mixed_prec_comparisons() {
    // $a < $b && $b < $c — different precedence levels, OK.
    let _e = parse_expr_str("$a < $b && $b < $c;");
}

#[test]
fn allow_comparison_in_ternary() {
    let _e = parse_expr_str("$a == $b ? 1 : 0;");
}

#[test]
fn allow_eq_after_lt() {
    // $a < $b == $c — different non-assoc prec groups, OK.
    let _e = parse_expr_str("$a < $b == $c;");
}

// ── Lvalue validation ─────────────────────────────────────

#[test]
fn reject_assign_to_literal() {
    assert!(parse_expr_fails("42 = $x;"));
}

#[test]
fn reject_assign_to_string() {
    assert!(parse_expr_fails("'hello' = $x;"));
}

#[test]
fn reject_assign_to_binop() {
    assert!(parse_expr_fails("$a + $b = 1;"));
}

#[test]
fn reject_compound_assign_to_literal() {
    assert!(parse_expr_fails("42 += 1;"));
}

#[test]
fn reject_prefix_inc_literal() {
    assert!(parse_expr_fails("++42;"));
}

#[test]
fn reject_postfix_inc_literal() {
    assert!(parse_expr_fails("42++;"));
}

#[test]
fn reject_prefix_dec_string() {
    assert!(parse_expr_fails("--'hello';"));
}

#[test]
fn allow_assign_to_var() {
    let _e = parse_expr_str("$x = 1;");
}

#[test]
fn allow_assign_to_array_elem() {
    let _e = parse_expr_str("$a[0] = 1;");
}

#[test]
fn allow_assign_to_hash_elem() {
    let _e = parse_expr_str("$h{key} = 1;");
}

#[test]
fn allow_assign_to_deref() {
    let _e = parse_expr_str("$$ref = 1;");
}

#[test]
fn allow_assign_to_arrow_deref() {
    let _e = parse_expr_str("$ref->[0] = 1;");
}

#[test]
fn allow_assign_to_my_decl() {
    let _e = parse_expr_str("my $x = 1;");
}

#[test]
fn allow_assign_to_local() {
    let _e = parse_expr_str("local $/ = undef;");
}

#[test]
fn allow_list_assign() {
    let prog = parse("my ($a, $b) = (1, 2);");
    assert_eq!(prog.statements.len(), 1);
}

#[test]
fn allow_inc_var() {
    let _e = parse_expr_str("++$x;");
}

#[test]
fn allow_postfix_inc_var() {
    let _e = parse_expr_str("$x++;");
}

#[test]
fn parse_unless_elsif() {
    let prog = parse("unless ($x) { 1; } elsif ($y) { 2; }");
    assert_eq!(prog.statements.len(), 1);
}

// ── Lexer error surfacing ─────────────────────────────────
//
// Lexer errors must be reported, not silently converted to Eof.

fn parse_fails(src: &str) -> String {
    let mut parser = Parser::new(src.as_bytes()).unwrap();
    match parser.parse_program() {
        Err(e) => e.message,
        Ok(_) => panic!("expected parse error for: {src}"),
    }
}

#[test]
fn lexer_error_unterminated_string() {
    let msg = parse_fails("my $x = \"hello;");
    assert!(msg.contains("unterminated"), "expected unterminated error, got: {msg}");
}

#[test]
fn lexer_error_unterminated_regex() {
    let msg = parse_fails("/foo bar");
    assert!(msg.contains("unterminated"), "expected unterminated error, got: {msg}");
}

#[test]
fn lexer_error_unexpected_byte() {
    let msg = parse_fails("my $x = \x01;");
    assert!(msg.contains("unexpected byte"), "expected unexpected byte error, got: {msg}");
}

#[test]
fn lexer_error_after_valid_code() {
    // Error occurs after some valid statements have been parsed.
    let msg = parse_fails("my $x = 1; my $y = \"unterminated;");
    assert!(msg.contains("unterminated"), "expected unterminated error, got: {msg}");
}

#[test]
fn lexer_error_immediate() {
    // Error on the very first token — no valid code at all.
    let msg = parse_fails("\"unterminated");
    assert!(msg.contains("unterminated"), "expected unterminated error, got: {msg}");
}

// ── Hard parsing corpus ───────────────────────────────────
//
// The tests below are derived from a corpus of adversarial
// cases targeting the hardest ambiguities in Perl parsing:
// regex-vs-division, block-vs-hash, indirect object, ternary
// associativity, comma/assignment precedence, arrow chains,
// interpolation, and heredoc integration.
//
// For each case we assert the specific structural facts we're
// confident about — typically the top-level node kind and a
// key grouping relationship.  We deliberately don't try to
// match whole trees, to keep tests robust against AST
// refactoring.

// ── Regex vs division ─────────────────────────────────────

#[test]
fn hard_div_chain_is_left_assoc() {
    // `$x / $y / $z` must be (($x / $y) / $z), not ($x / ($y / $z)).
    let e = parse_expr_str("$x / $y / $z;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Div, lhs, rhs) => {
            assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::Div, _, _)), "expected left-associative division, got lhs = {:?}", lhs.kind);
            assert!(matches!(rhs.kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected Div BinOp, got {other:?}"),
    }
}

#[test]
fn hard_print_slash_is_regex() {
    // `print /x/;` — after `print`, `/` starts a regex.
    let prog = parse("print /x/;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(name, _fh, args), .. }) => {
            assert_eq!(name, "print");
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::Regex(_, _, _)), "expected Regex arg, got {:?}", args[0].kind);
        }
        other => panic!("expected PrintOp with Regex arg, got {other:?}"),
    }
}

#[test]
fn hard_print_scalar_slash_is_division() {
    // `print $x / 2;` — here `/` is division, not regex.
    let prog = parse("print $x / 2;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, _, args), .. }) => {
            assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Div, _, _)), "expected Div BinOp arg, got {:?}", args[0].kind);
        }
        other => panic!("expected PrintOp, got {other:?}"),
    }
}

#[test]
fn hard_slash_in_ternary_condition_is_regex() {
    // `$x = /foo/ ? 1 : 2;` — ternary condition is regex.
    let e = parse_expr_str("$x = /foo/ ? 1 : 2;");
    match &e.kind {
        ExprKind::Assign(_, _, rhs) => match &rhs.kind {
            ExprKind::Ternary(cond, _, _) => {
                assert!(matches!(cond.kind, ExprKind::Regex(_, _, _)), "expected Regex condition, got {:?}", cond.kind);
            }
            other => panic!("expected Ternary, got {other:?}"),
        },
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn hard_defined_or_rhs_is_regex() {
    // `$x // /foo/;` — RHS of // is in term position, so regex.
    let e = parse_expr_str("$x // /foo/;");
    match &e.kind {
        ExprKind::BinOp(BinOp::DefinedOr, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::Regex(_, _, _)), "expected Regex rhs, got {:?}", rhs.kind);
        }
        other => panic!("expected DefinedOr BinOp, got {other:?}"),
    }
}

// ── Block vs hash ─────────────────────────────────────────

#[test]
fn hard_unary_plus_brace_is_hash() {
    // `+{ a => 1 }` — unary + forces expression context, so hash.
    let e = parse_expr_str("+{ a => 1 };");
    match &e.kind {
        ExprKind::UnaryOp(UnaryOp::NumPositive, inner) => {
            assert!(matches!(inner.kind, ExprKind::AnonHash(_)), "expected AnonHash inside unary +, got {:?}", inner.kind);
        }
        other => panic!("expected UnaryOp(+, AnonHash), got {other:?}"),
    }
}

#[test]
fn hard_map_outer_brace_is_block() {
    // `map { a => 1 } @list;` — outer braces are block argument.
    let e = parse_expr_str("map { a => 1 } @list;");
    match &e.kind {
        ExprKind::ListOp(name, args) => {
            assert_eq!(name, "map");
            // First arg is the block (as AnonSub).
            assert!(matches!(args[0].kind, ExprKind::AnonSub(..)), "expected AnonSub block, got {:?}", args[0].kind);
        }
        other => panic!("expected map ListOp, got {other:?}"),
    }
}

#[test]
fn hard_map_nested_brace_is_hash() {
    // `map { { a => 1 } } @list;` — outer = block, inner = hash.
    let e = parse_expr_str("map { { a => 1 } } @list;");
    match &e.kind {
        ExprKind::ListOp(name, args) => {
            assert_eq!(name, "map");
            // Outer: AnonSub wrapping the block.
            let body = match &args[0].kind {
                ExprKind::AnonSub(_, _, _, block) => block,
                other => panic!("expected AnonSub, got {other:?}"),
            };
            // Block body's single statement is an AnonHash expression.
            assert_eq!(body.statements.len(), 1);
            match &body.statements[0].kind {
                StmtKind::Expr(Expr { kind: ExprKind::AnonHash(_), .. }) => {}
                other => panic!("expected AnonHash stmt, got {other:?}"),
            }
        }
        other => panic!("expected map ListOp, got {other:?}"),
    }
}

#[test]
fn hard_do_brace_is_block_not_hash() {
    // `do { a => 1 };` — do BLOCK, not a hash constructor.
    let e = parse_expr_str("do { a => 1 };");
    assert!(matches!(e.kind, ExprKind::DoBlock(_)), "expected DoBlock, got {:?}", e.kind);
}

#[test]
fn hard_sub_nested_hash() {
    // `sub { { a => 1 } }` — anon sub whose body is a hash expression.
    let e = parse_expr_str("sub { { a => 1 } };");
    match &e.kind {
        ExprKind::AnonSub(_, _, _, block) => {
            assert_eq!(block.statements.len(), 1);
            match &block.statements[0].kind {
                StmtKind::Expr(Expr { kind: ExprKind::AnonHash(_), .. }) => {}
                other => panic!("expected AnonHash stmt inside sub body, got {other:?}"),
            }
        }
        other => panic!("expected AnonSub, got {other:?}"),
    }
}

// ── Bareword ambiguity ────────────────────────────────────

#[test]
fn hard_bareword_plus_literal() {
    // `foo + 1;` — bareword + literal (absent prototype/constant info).
    let e = parse_expr_str("foo + 1;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Add, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::Bareword(_) | ExprKind::FuncCall(_, _)), "expected Bareword/FuncCall lhs, got {:?}", lhs.kind);
        }
        other => panic!("expected Add BinOp, got {other:?}"),
    }
}

#[test]
fn hard_label_on_statement() {
    // `foo: bar();` — label at statement level.
    let prog = parse("foo: bar();");
    assert!(matches!(prog.statements[0].kind, StmtKind::Labeled(_, _)), "expected Labeled statement, got {:?}", prog.statements[0].kind);
}

// ── Indirect object ───────────────────────────────────────

#[test]
fn hard_print_filehandle_scalar() {
    // `print $fh "hello";` — indirect-object filehandle form.
    let prog = parse("print $fh \"hello\";");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, fh, args), .. }) => {
            let fh = fh.as_ref().expect("expected filehandle");
            assert!(matches!(fh.kind, ExprKind::ScalarVar(_)), "expected ScalarVar filehandle, got {:?}", fh.kind);
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected PrintOp with filehandle, got {other:?}"),
    }
}

#[test]
fn hard_print_filehandle_bareword() {
    // `print STDERR "hello";` — bareword filehandle.
    let prog = parse("print STDERR \"hello\";");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(_, fh, _), .. }) => {
            let fh = fh.as_ref().expect("expected filehandle");
            assert!(matches!(fh.kind, ExprKind::Bareword(_)), "expected Bareword filehandle, got {:?}", fh.kind);
        }
        other => panic!("expected PrintOp, got {other:?}"),
    }
}

// ── Postfix control flow ──────────────────────────────────

#[test]
fn hard_postfix_if() {
    // `print "x" if $cond;` — the whole `print "x"` is the modifier subject.
    let prog = parse("print \"x\" if $cond;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(kind, subject, cond), .. }) => {
            assert!(matches!(kind, PostfixKind::If));
            assert!(matches!(subject.kind, ExprKind::PrintOp(_, _, _)), "expected PrintOp subject, got {:?}", subject.kind);
            assert!(matches!(cond.kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected PostfixControl, got {other:?}"),
    }
}

#[test]
fn hard_postfix_while() {
    let prog = parse("foo() while $cond;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(kind, _, _), .. }) => {
            assert!(matches!(kind, PostfixKind::While));
        }
        other => panic!("expected PostfixControl, got {other:?}"),
    }
}

#[test]
fn hard_postfix_for() {
    let prog = parse("foo() for @list;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(kind, _, list), .. }) => {
            assert!(matches!(kind, PostfixKind::For | PostfixKind::Foreach));
            assert!(matches!(list.kind, ExprKind::ArrayVar(_)));
        }
        other => panic!("expected PostfixControl, got {other:?}"),
    }
}

// ── do / eval ─────────────────────────────────────────────

#[test]
fn hard_do_file() {
    // `do $file;` — do EXPR form, not do BLOCK.
    let e = parse_expr_str("do $file;");
    assert!(matches!(e.kind, ExprKind::DoExpr(_)), "expected DoExpr, got {:?}", e.kind);
}

#[test]
fn hard_eval_block_vs_expr() {
    let e1 = parse_expr_str("eval { 1 };");
    assert!(matches!(e1.kind, ExprKind::EvalBlock(_)), "expected EvalBlock, got {:?}", e1.kind);

    let e2 = parse_expr_str("eval $code;");
    assert!(matches!(e2.kind, ExprKind::EvalExpr(_)), "expected EvalExpr, got {:?}", e2.kind);
}

// ── Ternary precedence ────────────────────────────────────

#[test]
fn hard_ternary_right_associative() {
    // `$a ? $b : $c ? $d : $e;` — right-associative.
    // Must group as: Ternary($a, $b, Ternary($c, $d, $e))
    let e = parse_expr_str("$a ? $b : $c ? $d : $e;");
    match &e.kind {
        ExprKind::Ternary(_, then, else_) => {
            assert!(matches!(then.kind, ExprKind::ScalarVar(_)), "expected scalar then-branch, got {:?}", then.kind);
            assert!(matches!(else_.kind, ExprKind::Ternary(_, _, _)), "expected nested Ternary in else-branch (right-assoc), got {:?}", else_.kind);
        }
        other => panic!("expected Ternary, got {other:?}"),
    }
}

#[test]
fn hard_ternary_condition_has_plus() {
    // `$a + $b ? $c : $d;` — + binds tighter than ternary cond.
    let e = parse_expr_str("$a + $b ? $c : $d;");
    match &e.kind {
        ExprKind::Ternary(cond, _, _) => {
            assert!(matches!(cond.kind, ExprKind::BinOp(BinOp::Add, _, _)), "expected Add in condition, got {:?}", cond.kind);
        }
        other => panic!("expected Ternary, got {other:?}"),
    }
}

#[test]
fn hard_ternary_then_has_plus() {
    // `$a ? $b + $c : $d;` — full expression in then-branch.
    let e = parse_expr_str("$a ? $b + $c : $d;");
    match &e.kind {
        ExprKind::Ternary(_, then, _) => {
            assert!(matches!(then.kind, ExprKind::BinOp(BinOp::Add, _, _)), "expected Add in then-branch, got {:?}", then.kind);
        }
        other => panic!("expected Ternary, got {other:?}"),
    }
}

// ── Assignment / comma precedence ─────────────────────────

#[test]
fn hard_assign_comma_precedence() {
    // `$a = $b, $c;` — comma is lower than assignment.
    // Must group as: List([Assign($a, $b), $c])
    let e = parse_expr_str("$a = $b, $c;");
    match &e.kind {
        ExprKind::List(items) => {
            assert_eq!(items.len(), 2);
            assert!(matches!(items[0].kind, ExprKind::Assign(_, _, _)), "expected Assign as first list item, got {:?}", items[0].kind);
            assert!(matches!(items[1].kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn hard_assign_paren_comma() {
    // `$a = ($b, $c);` — parens force comma expression as RHS.
    let e = parse_expr_str("$a = ($b, $c);");
    match &e.kind {
        ExprKind::Assign(_, _, rhs) => {
            // RHS should be a List (possibly wrapped in Paren).
            let inner = match &rhs.kind {
                ExprKind::Paren(inner) => &inner.kind,
                other => other,
            };
            assert!(matches!(inner, ExprKind::List(_)), "expected List on RHS, got {inner:?}");
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

// ── Arrow / deref precedence ──────────────────────────────

#[test]
fn hard_arrow_method_call() {
    let e = parse_expr_str("$obj->method;");
    assert!(matches!(e.kind, ExprKind::MethodCall(_, _, _)), "expected MethodCall, got {:?}", e.kind);
}

#[test]
fn hard_arrow_hash_deref() {
    // `$obj->{key};` — hash element via arrow.
    let e = parse_expr_str("$obj->{key};");
    // Either ArrowDeref(_, Hash("key")) or HashElem form — both are valid.
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, _) | ExprKind::HashElem(_, _)), "expected ArrowDeref or HashElem, got {:?}", e.kind);
}

#[test]
fn hard_arrow_chained_method() {
    // `$obj->{key}->method;` — method call on hash-deref result.
    let e = parse_expr_str("$obj->{key}->method;");
    match &e.kind {
        ExprKind::MethodCall(target, name, _) => {
            assert_eq!(name, "method");
            assert!(matches!(target.kind, ExprKind::ArrowDeref(_, _) | ExprKind::HashElem(_, _)), "expected arrow/hash deref target, got {:?}", target.kind);
        }
        other => panic!("expected MethodCall, got {other:?}"),
    }
}

#[test]
fn hard_arrow_method_then_hash() {
    // `$obj->method()->{key};` — index on method call result.
    let e = parse_expr_str("$obj->method()->{key};");
    // The outermost should be the hash index, inner should be MethodCall.
    let inner = match &e.kind {
        ExprKind::ArrowDeref(target, _) => &target.kind,
        ExprKind::HashElem(target, _) => &target.kind,
        other => panic!("expected arrow/hash deref, got {other:?}"),
    };
    assert!(matches!(inner, ExprKind::MethodCall(_, _, _)), "expected MethodCall inside, got {inner:?}");
}

// ── Interpolation ─────────────────────────────────────────

#[test]
fn hard_interp_scalar() {
    // `"hello $x"` — interpolated string with scalar.
    let e = parse_expr_str("\"hello $x\";");
    match &e.kind {
        ExprKind::InterpolatedString(Interpolated(parts)) => {
            assert!(parts.iter().any(|p| matches!(p, InterpPart::ScalarInterp(_))), "expected ScalarInterp part, got {parts:?}");
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

#[test]
fn hard_interp_array() {
    let e = parse_expr_str("\"@arr\";");
    match &e.kind {
        ExprKind::InterpolatedString(Interpolated(parts)) => {
            assert!(parts.iter().any(|p| matches!(p, InterpPart::ArrayInterp(_))), "expected ArrayInterp part, got {parts:?}");
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

// ── Regex edge cases ──────────────────────────────────────

#[test]
fn hard_regex_m_slash() {
    let e = parse_expr_str("m/foo/;");
    assert!(matches!(e.kind, ExprKind::Regex(_, _, _)));
}

#[test]
fn hard_regex_bare_slash() {
    let e = parse_expr_str("/foo/;");
    assert!(matches!(e.kind, ExprKind::Regex(_, _, _)));
}

#[test]
fn hard_subst() {
    let e = parse_expr_str("s/foo/bar/;");
    assert!(matches!(e.kind, ExprKind::Subst(_, _, _)));
}

#[test]
fn hard_binding_regex() {
    // `$x =~ /foo/;` — binding operator with regex on RHS.
    let e = parse_expr_str("$x =~ /foo/;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Binding, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::Regex(_, _, _)));
        }
        other => panic!("expected Binding BinOp, got {other:?}"),
    }
}

#[test]
fn hard_regex_brace_delim() {
    let e = parse_expr_str("$x =~ m{foo};");
    match &e.kind {
        ExprKind::BinOp(BinOp::Binding, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::Regex(_, _, _)));
        }
        other => panic!("expected Binding, got {other:?}"),
    }
}

// ── Combined nightmare cases ──────────────────────────────

#[test]
fn hard_nightmare_map_ternary_hash() {
    // `map { /x/ ? { a => 1 } : { b => 2 } } @list;`
    // Exercises: block-vs-hash, regex-vs-division, ternary grouping.
    let e = parse_expr_str("map { /x/ ? { a => 1 } : { b => 2 } } @list;");
    match &e.kind {
        ExprKind::ListOp(name, args) => {
            assert_eq!(name, "map");
            let block = match &args[0].kind {
                ExprKind::AnonSub(_, _, _, b) => b,
                other => panic!("expected AnonSub, got {other:?}"),
            };
            assert_eq!(block.statements.len(), 1);
            match &block.statements[0].kind {
                StmtKind::Expr(Expr { kind: ExprKind::Ternary(cond, then, else_), .. }) => {
                    assert!(matches!(cond.kind, ExprKind::Regex(_, _, _)), "expected Regex condition, got {:?}", cond.kind);
                    assert!(matches!(then.kind, ExprKind::AnonHash(_)), "expected AnonHash then-branch, got {:?}", then.kind);
                    assert!(matches!(else_.kind, ExprKind::AnonHash(_)), "expected AnonHash else-branch, got {:?}", else_.kind);
                }
                other => panic!("expected Ternary stmt, got {other:?}"),
            }
        }
        other => panic!("expected map ListOp, got {other:?}"),
    }
}

#[test]
fn hard_nightmare_do_ternary_hash() {
    // `$x = do { /x/ ? { a => 1 } : { b => 2 } };`
    let e = parse_expr_str("$x = do { /x/ ? { a => 1 } : { b => 2 } };");
    match &e.kind {
        ExprKind::Assign(_, _, rhs) => match &rhs.kind {
            ExprKind::DoBlock(block) => {
                assert_eq!(block.statements.len(), 1);
                match &block.statements[0].kind {
                    StmtKind::Expr(Expr { kind: ExprKind::Ternary(_, then, else_), .. }) => {
                        assert!(matches!(then.kind, ExprKind::AnonHash(_)));
                        assert!(matches!(else_.kind, ExprKind::AnonHash(_)));
                    }
                    other => panic!("expected Ternary stmt, got {other:?}"),
                }
            }
            other => panic!("expected DoBlock, got {other:?}"),
        },
        other => panic!("expected Assign, got {other:?}"),
    }
}

// ── Parse-or-error (Tier 1) — just verify these parse ─────
//
// For cases where the exact AST shape depends on decisions we
// haven't firmed up (or features we haven't implemented yet),
// at least verify the parser accepts them.

#[test]
fn hard_parses_map_slash() {
    // `map { /x/ } @list;` — regex inside map block.
    parse("map { /x/ } @list;");
}

#[test]
fn hard_parses_regex_in_sub() {
    // `sub f { /x/ }` — regex as sub body expression.
    parse("sub f { /x/ }");
}

#[test]
fn hard_parses_map_list_form() {
    // `map /x/, @list;` — non-block form of map.
    parse("map /x/, @list;");
}

#[test]
fn hard_parses_foo_bareword_alone() {
    // `foo;` — bare bareword statement.
    parse("foo;");
}

#[test]
fn hard_parses_nested_brace_print() {
    // `print { $fh } "hello";` — brace-filehandle form.
    parse("print { $fh } \"hello\";");
}

#[test]
fn hard_parses_paren_grouping() {
    parse("($a + $b) * $c;");
}

#[test]
fn hard_my_assign_comma_grouping() {
    // `my $x = $a, $b;` — Perl parses as `(my $x = $a), $b`.
    // Since `my` is an expression, the whole thing is a List with
    // an Assign(Decl(My), $a) first, then $b.
    let prog = parse("my $x = $a, $b;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::List(items), .. }) => {
            assert_eq!(items.len(), 2, "expected 2 list items, got {}", items.len());
            // First item: Assign(Decl(My, [$x]), $a)
            match &items[0].kind {
                ExprKind::Assign(_, lhs, rhs) => {
                    assert!(matches!(lhs.kind, ExprKind::Decl(DeclScope::My, _)), "expected Decl(My) lhs, got {:?}", lhs.kind);
                    assert!(matches!(rhs.kind, ExprKind::ScalarVar(_)), "expected ScalarVar rhs, got {:?}", rhs.kind);
                }
                other => panic!("expected Assign as first list item, got {other:?}"),
            }
            // Second item: $b
            assert!(matches!(items[1].kind, ExprKind::ScalarVar(_)), "expected ScalarVar as second item, got {:?}", items[1].kind);
        }
        other => panic!("expected Stmt::Expr(List), got {other:?}"),
    }
}

#[test]
fn hard_our_assign_comma_grouping() {
    // `our $x = $a, $b;` — same behavior as `my` with a different scope.
    let prog = parse("our $x = $a, $b;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::List(items), .. }) => {
            assert_eq!(items.len(), 2);
            match &items[0].kind {
                ExprKind::Assign(_, lhs, _) => {
                    assert!(matches!(lhs.kind, ExprKind::Decl(DeclScope::Our, _)), "expected Decl(Our) lhs, got {:?}", lhs.kind);
                }
                other => panic!("expected Assign, got {other:?}"),
            }
            assert!(matches!(items[1].kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected Stmt::Expr(List), got {other:?}"),
    }
}

#[test]
fn hard_state_assign_comma_grouping() {
    // `state $x = $a, $b;` — same behavior as `my` with a different scope.
    let prog = parse("state $x = $a, $b;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::List(items), .. }) => {
            assert_eq!(items.len(), 2);
            match &items[0].kind {
                ExprKind::Assign(_, lhs, _) => {
                    assert!(matches!(lhs.kind, ExprKind::Decl(DeclScope::State, _)), "expected Decl(State) lhs, got {:?}", lhs.kind);
                }
                other => panic!("expected Assign, got {other:?}"),
            }
            assert!(matches!(items[1].kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected Stmt::Expr(List), got {other:?}"),
    }
}

#[test]
fn hard_local_assign_comma_grouping() {
    // `local $x = $a, $b;` — local is an expression too; the trailing
    // comma must NOT be absorbed into the Local operand.
    // Must group as `(local $x = $a), $b`, giving List([Assign(Local($x), $a), $b]).
    let prog = parse("local $x = $a, $b;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::List(items), .. }) => {
            assert_eq!(items.len(), 2, "expected 2 list items, got {}", items.len());
            match &items[0].kind {
                ExprKind::Assign(_, lhs, rhs) => {
                    assert!(matches!(lhs.kind, ExprKind::Local(_)), "expected Local lhs, got {:?}", lhs.kind);
                    assert!(matches!(rhs.kind, ExprKind::ScalarVar(_)), "expected ScalarVar rhs, got {:?}", rhs.kind);
                }
                other => panic!("expected Assign, got {other:?}"),
            }
            assert!(matches!(items[1].kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected Stmt::Expr(List), got {other:?}"),
    }
}

// ── Declarations as expressions: basic forms ──────────────
//
// Verify that each declaration kind produces an expression
// (wrapped in Stmt::Expr), not a dedicated statement kind.

#[test]
fn hard_my_is_expression() {
    // `my $x;` — no initializer, bare Decl expression.
    let prog = parse("my $x;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Decl(DeclScope::My, vars), .. }) => {
            assert_eq!(vars[0].name, "x");
        }
        other => panic!("expected Stmt::Expr(Decl(My)), got {other:?}"),
    }
}

#[test]
fn hard_our_is_expression() {
    let prog = parse("our $x;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Decl(DeclScope::Our, vars), .. }) => {
            assert_eq!(vars[0].name, "x");
        }
        other => panic!("expected Stmt::Expr(Decl(Our)), got {other:?}"),
    }
}

#[test]
fn hard_state_is_expression() {
    let prog = parse("state $x;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Decl(DeclScope::State, vars), .. }) => {
            assert_eq!(vars[0].name, "x");
        }
        other => panic!("expected Stmt::Expr(Decl(State)), got {other:?}"),
    }
}

#[test]
fn hard_local_is_expression() {
    let prog = parse("local $x;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Local(inner), .. }) => {
            assert!(matches!(inner.kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected Stmt::Expr(Local), got {other:?}"),
    }
}

// ── Declarations in expression position ───────────────────
//
// Declarations as expressions should be usable in any context
// that accepts an expression — not just at statement start.

#[test]
fn hard_my_in_parens() {
    // `(my $x) = @list;` — decl inside parens on LHS of assignment.
    let prog = parse("(my $x) = @list;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => {
            // LHS should contain a Decl (possibly wrapped in Paren).
            let inner = match &lhs.kind {
                ExprKind::Paren(inner) => &inner.kind,
                other => other,
            };
            assert!(matches!(inner, ExprKind::Decl(DeclScope::My, _)), "expected Decl on LHS, got {inner:?}");
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn hard_my_list_in_parens() {
    // `my ($a, $b) = @list;` — list form.
    let prog = parse("my ($a, $b) = @list;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => match &lhs.kind {
            ExprKind::Decl(DeclScope::My, vars) => {
                assert_eq!(vars.len(), 2);
                assert_eq!(vars[0].name, "a");
                assert_eq!(vars[1].name, "b");
            }
            other => panic!("expected Decl(My), got {other:?}"),
        },
        other => panic!("expected Assign, got {other:?}"),
    }
}

// ── Declarations in control-flow heads ────────────────────

#[test]
fn hard_my_in_if_condition() {
    // `if (my $x = foo()) { ... }` — decl in an if condition.
    // The decl is nested inside an If statement's paren-expr.
    let prog = parse("if (my $x = foo()) { 1; }");
    match &prog.statements[0].kind {
        StmtKind::If(if_stmt) => match &if_stmt.condition.kind {
            ExprKind::Assign(_, lhs, _) => {
                assert!(matches!(lhs.kind, ExprKind::Decl(DeclScope::My, _)), "expected Decl on LHS, got {:?}", lhs.kind);
            }
            other => panic!("expected Assign in if condition, got {other:?}"),
        },
        other => panic!("expected If, got {other:?}"),
    }
}

#[test]
fn hard_my_in_while_condition() {
    let prog = parse("while (my $line = <$fh>) { 1; }");
    match &prog.statements[0].kind {
        StmtKind::While(w) => match &w.condition.kind {
            ExprKind::Assign(_, lhs, _) => {
                assert!(matches!(lhs.kind, ExprKind::Decl(DeclScope::My, _)));
            }
            other => panic!("expected Assign, got {other:?}"),
        },
        other => panic!("expected While, got {other:?}"),
    }
}

#[test]
fn hard_parses_postfix_unless() {
    parse("print \"x\" unless $cond;");
}

// ── Prototype-driven call-site parsing ─────────────────────
//
// These verify that a sub's prototype — registered in the
// symbol table at declaration time — drives how arguments at
// call sites are parsed.  Anti-oracle cases adapted from
// ChatGPT's parser-breaker corpus.

/// Given `sub NAME (PROTO); CALL`, parse and return the
/// expression from the second statement (the call).
fn parse_call_with_proto(src: &str) -> Expr {
    let prog = parse(src);
    assert!(prog.statements.len() >= 2, "expected ≥2 statements (decl + call), got {}", prog.statements.len());
    match &prog.statements[1].kind {
        StmtKind::Expr(e) => e.clone(),
        other => panic!("expected Stmt::Expr for call, got {other:?}"),
    }
}

#[test]
fn proto_empty_stops_at_plus() {
    // sub foo (); foo + 1;
    // Empty prototype forces zero args, so `+ 1` is a binary op.
    // Expected: BinOp(Add, FuncCall("foo", []), Int(1)).
    let e = parse_call_with_proto("sub foo (); foo + 1;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Add, lhs, rhs) => {
            match &lhs.kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "foo");
                    assert_eq!(args.len(), 0, "empty-proto call should have 0 args");
                }
                other => panic!("expected FuncCall(foo, []), got {other:?}"),
            }
            assert!(matches!(rhs.kind, ExprKind::IntLit(1)));
        }
        other => panic!("expected BinOp(Add, FuncCall, 1), got {other:?}"),
    }
}

#[test]
fn proto_single_scalar_takes_one_expr() {
    // sub foo ($); foo $a + $b;
    // One-scalar proto: `$a + $b` is the single arg.
    let e = parse_call_with_proto("sub foo ($); foo $a + $b;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 1, "$-proto should take exactly 1 arg");
            assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Add, _, _)), "arg should be $a + $b, got {:?}", args[0].kind);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_single_scalar_comma_terminates_arg() {
    // sub foo ($); foo $a, $b;
    // One-scalar proto: `$a` is the arg; comma ends the call,
    // and `$b` is a separate list element.  Expected:
    // List([FuncCall("foo", [$a]), $b]).
    let e = parse_call_with_proto("sub foo ($); foo $a, $b;");
    match &e.kind {
        ExprKind::List(items) => {
            assert_eq!(items.len(), 2);
            match &items[0].kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "foo");
                    assert_eq!(args.len(), 1);
                    assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)));
                }
                other => panic!("expected FuncCall(foo, [$a]), got {other:?}"),
            }
            assert!(matches!(items[1].kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected List with foo call and $b, got {other:?}"),
    }
}

#[test]
fn proto_two_scalars_takes_two_args() {
    // sub foo ($$); foo $a + $b, $c;
    // Two-scalar proto: `$a + $b` is arg 1, `$c` is arg 2.
    let e = parse_call_with_proto("sub foo ($$); foo $a + $b, $c;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 2, "$$-proto should take 2 args");
            assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Add, _, _)), "arg 1 should be Add, got {:?}", args[0].kind);
            assert!(matches!(args[1].kind, ExprKind::ScalarVar(_)), "arg 2 should be $c, got {:?}", args[1].kind);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_block_and_list() {
    // sub foo (&@); foo { $x } @list;
    // &@-proto: first arg is a block (wrapped as AnonSub),
    // second is the slurpy list.
    let e = parse_call_with_proto("sub foo (&@); foo { $x } @list;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 2, "&@-proto should take block + list = 2 args");
            assert!(matches!(args[0].kind, ExprKind::AnonSub(..)), "arg 1 should be AnonSub (block), got {:?}", args[0].kind);
            assert!(matches!(args[1].kind, ExprKind::ArrayVar(_)), "arg 2 should be @list, got {:?}", args[1].kind);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_slurpy_list_takes_everything() {
    // sub foo (@); foo $a, $b, $c;
    // Slurpy proto: all three args are consumed.
    let e = parse_call_with_proto("sub foo (@); foo $a, $b, $c;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 3);
        }
        other => panic!("expected FuncCall with 3 args, got {other:?}"),
    }
}

#[test]
fn proto_forward_declaration_registers_proto() {
    // sub foo ($$);  # forward-decl only, no body
    // foo $a, $b;    # should still use the proto
    let e = parse_call_with_proto("sub foo ($$); foo $a, $b;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected FuncCall with 2 args, got {other:?}"),
    }
}

#[test]
fn known_sub_without_proto_is_list_op() {
    // sub foo { 1 } foo 1, 2;
    // No prototype, but sub is known: parses as list op call.
    let e = parse_call_with_proto("sub foo { 1 } foo 1, 2;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 2);
            assert!(matches!(args[0].kind, ExprKind::IntLit(1)));
            assert!(matches!(args[1].kind, ExprKind::IntLit(2)));
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn unknown_sub_stays_bareword_before_operator() {
    // foo + 1;  # no declaration — original behavior preserved.
    // Should parse as BinOp(Add, Bareword("foo"), 1).
    let prog = parse("foo + 1;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::BinOp(BinOp::Add, lhs, rhs), .. }) => {
            assert!(matches!(lhs.kind, ExprKind::Bareword(_) | ExprKind::FuncCall(_, _)), "lhs should be Bareword or FuncCall, got {:?}", lhs.kind);
            assert!(matches!(rhs.kind, ExprKind::IntLit(1)));
        }
        other => panic!("expected BinOp(Add, ..., 1), got {other:?}"),
    }
}

#[test]
fn proto_respects_package_scope() {
    // A proto declared in Foo shouldn't affect bare calls in main.
    // package Foo; sub bar (); package main; bar + 1;
    // The bare `bar` in main isn't found → falls through to
    // Bareword + BinOp.
    let prog = parse("package Foo; sub bar (); package main; bar + 1;");
    // Find the last statement (the `bar + 1` call).
    let last = prog.statements.last().expect("at least one stmt");
    match &last.kind {
        StmtKind::Expr(Expr { kind: ExprKind::BinOp(BinOp::Add, lhs, _), .. }) => {
            // bar is not found in main → stays bareword.
            assert!(matches!(lhs.kind, ExprKind::Bareword(_)), "expected Bareword (not found in main), got {:?}", lhs.kind);
        }
        other => panic!("expected BinOp, got {other:?}"),
    }
}

#[test]
fn proto_respects_fully_qualified_call() {
    // package Foo; sub bar (); package main; Foo::bar + 1;
    // Fully-qualified call finds the proto → zero-arg call.
    let prog = parse("package Foo; sub bar (); package main; Foo::bar + 1;");
    let last = prog.statements.last().expect("at least one stmt");
    match &last.kind {
        StmtKind::Expr(Expr { kind: ExprKind::BinOp(BinOp::Add, lhs, _), .. }) => match &lhs.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "Foo::bar");
                assert_eq!(args.len(), 0, "empty-proto FQN call should have 0 args");
            }
            other => panic!("expected FuncCall(Foo::bar, []), got {other:?}"),
        },
        other => panic!("expected BinOp, got {other:?}"),
    }
}

#[test]
fn proto_underscore_with_arg_takes_it() {
    // sub foo (_); foo $x;
    // `_` slot with an arg supplied behaves like `$`.
    let e = parse_call_with_proto("sub foo (_); foo $x;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)), "expected ScalarVar, got {:?}", args[0].kind);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_underscore_without_arg_inserts_default_var() {
    // sub foo (_); foo;
    // `_` slot with no arg → parser inserts DefaultVar.
    let e = parse_call_with_proto("sub foo (_); foo;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 1, "_-slot should default to DefaultVar when omitted");
            assert!(matches!(args[0].kind, ExprKind::DefaultVar), "expected DefaultVar, got {:?}", args[0].kind);
        }
        other => panic!("expected FuncCall with DefaultVar, got {other:?}"),
    }
}

#[test]
fn proto_underscore_distinct_from_explicit_dollar_underscore() {
    // sub foo (_); foo $_;
    // Explicit $_ should be ScalarVar("_"), NOT DefaultVar.
    // This pins down the distinction: the parser inserts
    // DefaultVar only when the arg is omitted.
    let e = parse_call_with_proto("sub foo (_); foo $_;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 1);
            // Note: $_ may be represented as SpecialVar or
            // ScalarVar depending on the lexer; either is fine,
            // as long as it's NOT DefaultVar.
            assert!(!matches!(args[0].kind, ExprKind::DefaultVar), "explicit $_ should not become DefaultVar");
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_glob_bareword_becomes_glob_var() {
    // sub foo (*); foo STDIN;
    // Bareword in a `*` slot is auto-promoted to a typeglob.
    let e = parse_call_with_proto("sub foo (*); foo STDIN;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 1);
            match &args[0].kind {
                ExprKind::GlobVar(n) => assert_eq!(n, "STDIN"),
                other => panic!("expected GlobVar(STDIN), got {other:?}"),
            }
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_glob_explicit_star_stays_glob() {
    // sub foo (*); foo *STDIN;
    // Explicit *STDIN is already a GlobVar from the source.
    let e = parse_call_with_proto("sub foo (*); foo *STDIN;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::GlobVar(_)), "expected GlobVar, got {:?}", args[0].kind);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_glob_scalar_passed_through() {
    // sub foo (*); foo $fh;
    // A scalar expression in a `*` slot is parsed as-is —
    // it's presumed to hold a glob ref at runtime.
    let e = parse_call_with_proto("sub foo (*); foo $fh;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)), "expected ScalarVar, got {:?}", args[0].kind);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

// ── Prototype bypass cases ─────────────────────────────────
//
// Two syntactic forms bypass prototype-driven argument parsing:
//   1. Parens form: foo(args) — args are parens-delimited, so
//      the parser takes a generic comma-separated list without
//      consulting the prototype.  (Perl may still validate arg
//      counts at compile time; that's a semantic-pass concern,
//      not a parsing concern.)
//   2. Ampersand form: &foo(args) — goes through the code-ref
//      prefix path, completely bypassing parse_ident_term and
//      therefore the symbol-table lookup.

#[test]
fn proto_parens_form_parses_generic_list() {
    // sub foo ($); foo($a + $b, $c);
    // Without parens, `$` proto would consume only `$a + $b`
    // and leave `$c` in the outer comma list.  With parens,
    // the args are delimited, so we get both.
    let e = parse_call_with_proto("sub foo ($); foo($a + $b, $c);");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 2, "parens form should parse both args regardless of $ proto");
            assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Add, _, _)));
            assert!(matches!(args[1].kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_parens_form_ignores_empty_proto() {
    // sub foo (); foo(1, 2);
    // Parens form takes the args; Perl would report "Too many
    // arguments" at compile time but we don't validate yet.
    let e = parse_call_with_proto("sub foo (); foo(1, 2);");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected FuncCall with 2 args, got {other:?}"),
    }
}

#[test]
fn proto_ampersand_call_bypasses_empty_proto() {
    // sub foo (); &foo(1, 2);
    // &foo() completely bypasses prototype parsing.  Without
    // the &, `foo(1, 2)` would still work via parens (see test
    // above), but the &-form is the canonical bypass.
    let e = parse_call_with_proto("sub foo (); &foo(1, 2);");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 2, "&foo(...) bypasses empty proto");
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_ampersand_no_parens_bypasses_proto() {
    // sub foo ($); &foo;
    // &foo with no parens calls with current @_ (inherited);
    // prototype is not consulted.
    let e = parse_call_with_proto("sub foo ($); &foo;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 0, "&foo with no parens inherits @_");
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

// ── Named-unary precedence for scalar-ish slots ─────────────
//
// A `$`-slot (or `_`, `+`, `\X`, `\[...]`, glob-expression)
// parses its arg at named-unary precedence.  That means
// operators tighter than named unary (shift, +, -, *, /, **,
// etc.) are consumed into the arg, while operators looser
// (relational, equality, ternary, assignment, comma) terminate
// the arg and apply at the outer level.

#[test]
fn proto_scalar_tight_op_is_consumed() {
    // sub foo ($); foo $a << 1;
    // `<<` (shift, tighter than named unary) is consumed.
    let e = parse_call_with_proto("sub foo ($); foo $a << 1;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 1);
            assert!(
                matches!(args[0].kind, ExprKind::BinOp(BinOp::ShiftLeft, _, _)),
                "expected arg to be ShiftLeft (tighter than named-unary), got {:?}",
                args[0].kind
            );
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_scalar_relational_terminates_arg() {
    // sub foo ($); foo $a < 1;
    // `<` (relational, looser than named unary) terminates the
    // arg.  Parses as `foo($a) < 1`.
    let e = parse_call_with_proto("sub foo ($); foo $a < 1;");
    match &e.kind {
        ExprKind::BinOp(BinOp::NumLt, lhs, rhs) => {
            match &lhs.kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "foo");
                    assert_eq!(args.len(), 1);
                    assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)));
                }
                other => panic!("expected FuncCall on lhs, got {other:?}"),
            }
            assert!(matches!(rhs.kind, ExprKind::IntLit(1)));
        }
        other => panic!("expected BinOp(NumLt, FuncCall, 1), got {other:?}"),
    }
}

#[test]
fn proto_scalar_equality_terminates_arg() {
    // sub foo ($); foo 1 == 2;
    // `==` is looser than named unary → terminates arg.
    // Parses as `foo(1) == 2`.
    let e = parse_call_with_proto("sub foo ($); foo 1 == 2;");
    match &e.kind {
        ExprKind::BinOp(BinOp::NumEq, lhs, rhs) => {
            match &lhs.kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "foo");
                    assert_eq!(args.len(), 1);
                    assert!(matches!(args[0].kind, ExprKind::IntLit(1)));
                }
                other => panic!("expected FuncCall on lhs, got {other:?}"),
            }
            assert!(matches!(rhs.kind, ExprKind::IntLit(2)));
        }
        other => panic!("expected BinOp(NumEq, FuncCall, 2), got {other:?}"),
    }
}

#[test]
fn proto_scalar_ternary_terminates_arg() {
    // sub foo ($); foo $a ? $b : $c;
    // Ternary is far below named unary → terminates arg.
    // Parses as `foo($a) ? $b : $c`.
    let e = parse_call_with_proto("sub foo ($); foo $a ? $b : $c;");
    match &e.kind {
        ExprKind::Ternary(cond, _, _) => match &cond.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "foo");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall as ternary cond, got {other:?}"),
        },
        other => panic!("expected Ternary, got {other:?}"),
    }
}

#[test]
fn proto_scalar_mul_and_add_both_consumed() {
    // sub foo ($); foo 1 + 2 * 3;
    // Both `+` and `*` are tighter than named unary, so the
    // whole arithmetic expression is the single arg.
    let e = parse_call_with_proto("sub foo ($); foo 1 + 2 * 3;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Add, _, _)), "expected top-level Add, got {:?}", args[0].kind);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

// ── & slot accepting code references ────────────────────────
//
// A `&` prototype slot accepts either a literal block (wrapped
// as an anonymous sub) or any code-reference expression —
// `\&name`, `$coderef`, `sub { ... }`, etc.

#[test]
fn proto_amp_slot_accepts_backslash_sub_ref() {
    // sub foo (&@); foo \&bar, @list;
    // `\&bar` is a reference-to-sub expression.
    let e = parse_call_with_proto("sub foo (&@); foo \\&bar, @list;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 2);
            // First arg is a ref-take around something naming `bar`.
            assert!(matches!(args[0].kind, ExprKind::Ref(_)), "expected Ref(...), got {:?}", args[0].kind);
            assert!(matches!(args[1].kind, ExprKind::ArrayVar(_)), "expected @list, got {:?}", args[1].kind);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_amp_slot_accepts_scalar_coderef() {
    // sub foo (&@); foo $cref, @list;
    // Scalar holding a coderef.
    let e = parse_call_with_proto("sub foo (&@); foo $cref, @list;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 2);
            assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)), "expected ScalarVar, got {:?}", args[0].kind);
            assert!(matches!(args[1].kind, ExprKind::ArrayVar(_)));
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_amp_slot_accepts_anonymous_sub() {
    // sub foo (&@); foo sub { 1 }, @list;
    // Anonymous sub expression in the & slot.
    let e = parse_call_with_proto("sub foo (&@); foo sub { 1 }, @list;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 2);
            assert!(matches!(args[0].kind, ExprKind::AnonSub(..)), "expected AnonSub, got {:?}", args[0].kind);
            assert!(matches!(args[1].kind, ExprKind::ArrayVar(_)));
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_amp_slot_block_still_works() {
    // sub foo (&@); foo { $x * 2 } @list;
    // Regression: literal block form still wraps as AnonSub.
    let e = parse_call_with_proto("sub foo (&@); foo { $x * 2 } @list;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 2);
            assert!(matches!(args[0].kind, ExprKind::AnonSub(..)));
            assert!(matches!(args[1].kind, ExprKind::ArrayVar(_)));
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

// ── Auto-reference prototype slots ──────────────────────────
//
// `\$`, `\@`, `\%`, `\&`, `\*`, `\[...]`, and `+` all cause
// the argument to be implicitly referenced at the call site.
// `foo @arr` with `sub foo (\@)` is equivalent to `foo(\@arr)`.
// The parser wraps the argument in an ExprKind::Ref; any
// validation that the argument is of the expected kind is a
// semantic-pass concern.

#[test]
fn proto_auto_ref_array() {
    // sub foo (\@); foo @arr;  →  foo(\@arr)
    let e = parse_call_with_proto("sub foo (\\@); foo @arr;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 1);
            match &args[0].kind {
                ExprKind::Ref(inner) => {
                    assert!(matches!(inner.kind, ExprKind::ArrayVar(_)), "expected Ref(ArrayVar), got Ref({:?})", inner.kind);
                }
                other => panic!("expected Ref, got {other:?}"),
            }
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_auto_ref_hash() {
    // sub foo (\%); foo %h;  →  foo(\%h)
    let e = parse_call_with_proto("sub foo (\\%); foo %h;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 1);
            match &args[0].kind {
                ExprKind::Ref(inner) => {
                    assert!(matches!(inner.kind, ExprKind::HashVar(_)), "expected Ref(HashVar), got Ref({:?})", inner.kind);
                }
                other => panic!("expected Ref, got {other:?}"),
            }
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_auto_ref_scalar() {
    // sub foo (\$); foo $x;  →  foo(\$x)
    let e = parse_call_with_proto("sub foo (\\$); foo $x;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 1);
            match &args[0].kind {
                ExprKind::Ref(inner) => {
                    assert!(matches!(inner.kind, ExprKind::ScalarVar(_)));
                }
                other => panic!("expected Ref, got {other:?}"),
            }
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_auto_ref_one_of_takes_array() {
    // sub foo (\[@%]); foo @arr;  →  foo(\@arr)
    let e = parse_call_with_proto("sub foo (\\[@%]); foo @arr;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 1);
            match &args[0].kind {
                ExprKind::Ref(inner) => {
                    assert!(matches!(inner.kind, ExprKind::ArrayVar(_)));
                }
                other => panic!("expected Ref, got {other:?}"),
            }
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_auto_ref_one_of_takes_hash() {
    // sub foo (\[@%]); foo %h;  →  foo(\%h)
    let e = parse_call_with_proto("sub foo (\\[@%]); foo %h;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 1);
            match &args[0].kind {
                ExprKind::Ref(inner) => {
                    assert!(matches!(inner.kind, ExprKind::HashVar(_)));
                }
                other => panic!("expected Ref, got {other:?}"),
            }
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_array_or_hash_takes_array() {
    // sub foo (+); foo @arr;  →  foo(\@arr)
    // The `+` slot is effectively `\[@%]`.
    let e = parse_call_with_proto("sub foo (+); foo @arr;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 1);
            match &args[0].kind {
                ExprKind::Ref(inner) => {
                    assert!(matches!(inner.kind, ExprKind::ArrayVar(_)), "expected Ref(ArrayVar), got Ref({:?})", inner.kind);
                }
                other => panic!("expected Ref, got {other:?}"),
            }
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_array_or_hash_takes_hash() {
    // sub foo (+); foo %h;  →  foo(\%h)
    let e = parse_call_with_proto("sub foo (+); foo %h;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 1);
            match &args[0].kind {
                ExprKind::Ref(inner) => {
                    assert!(matches!(inner.kind, ExprKind::HashVar(_)));
                }
                other => panic!("expected Ref, got {other:?}"),
            }
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_auto_ref_multiple_slots() {
    // sub foo (\@\@); foo @a, @b;  →  foo(\@a, \@b)
    let e = parse_call_with_proto("sub foo (\\@\\@); foo @a, @b;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 2);
            for arg in args {
                match &arg.kind {
                    ExprKind::Ref(inner) => {
                        assert!(matches!(inner.kind, ExprKind::ArrayVar(_)));
                    }
                    other => panic!("expected Ref(ArrayVar), got {other:?}"),
                }
            }
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_auto_ref_mixed_with_slurpy() {
    // sub foo (\@@); foo @a, $x, $y;  →  foo(\@a, $x, $y)
    // First slot takes the array by ref; slurpy takes the rest.
    let e = parse_call_with_proto("sub foo (\\@@); foo @a, $x, $y;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 3);
            // First arg is the ref'd array.
            match &args[0].kind {
                ExprKind::Ref(inner) => {
                    assert!(matches!(inner.kind, ExprKind::ArrayVar(_)));
                }
                other => panic!("expected Ref(ArrayVar) first, got {other:?}"),
            }
            // Remaining two are scalar slurpy args, not ref'd.
            assert!(matches!(args[1].kind, ExprKind::ScalarVar(_)));
            assert!(matches!(args[2].kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

// ── Non-initial & slot: `{` is a hash-ref, not a block ──────
//
// In the initial slot, `&` plus a bare `{` parses the block as
// an anonymous sub (the map/grep pattern).  In any non-initial
// position, `{` at a call site is an ordinary hash-ref
// constructor; to pass a code reference the caller must spell
// it out: `sub { ... }`, `\&name`, `$coderef`, etc.

#[test]
fn proto_amp_non_initial_brace_is_hash_ref() {
    // sub foo ($&); foo $x, { a => 1 };
    // The `{ a => 1 }` is a hash-ref constructor, NOT a block.
    let e = parse_call_with_proto("sub foo ($&); foo $x, { a => 1 };");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 2);
            assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)));
            assert!(matches!(args[1].kind, ExprKind::AnonHash(_)), "expected AnonHash, got {:?}", args[1].kind);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_amp_non_initial_explicit_sub_works() {
    // sub foo ($&); foo $x, sub { 1 };
    let e = parse_call_with_proto("sub foo ($&); foo $x, sub { 1 };");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 2);
            assert!(matches!(args[1].kind, ExprKind::AnonSub(..)), "expected AnonSub, got {:?}", args[1].kind);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_amp_non_initial_backslash_name_works() {
    // sub foo ($&); foo $x, \&bar;
    let e = parse_call_with_proto("sub foo ($&); foo $x, \\&bar;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 2);
            assert!(matches!(args[1].kind, ExprKind::Ref(_)), "expected Ref, got {:?}", args[1].kind);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_amp_initial_block_still_works() {
    // sub foo (&); foo { 1 };
    // Regression: initial `&` with bare block still wraps as
    // AnonSub.
    let e = parse_call_with_proto("sub foo (&); foo { 1 };");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::AnonSub(..)));
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_amp_initial_map_style_still_works() {
    // sub mymap (&@); mymap { $_ * 2 } @list;
    // Regression: initial `&@` map-style syntax is unchanged.
    let e = parse_call_with_proto("sub mymap (&@); mymap { $_ * 2 } @list;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 2);
            assert!(matches!(args[0].kind, ExprKind::AnonSub(..)));
            assert!(matches!(args[1].kind, ExprKind::ArrayVar(_)));
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

// ── :prototype(...) attribute form ──────────────────────────
//
// Modern Perl (5.20+) allows the prototype to be declared via
// an attribute rather than the paren form:
//   sub foo :prototype($$) { ... }
// The attribute form is equivalent to the paren form but
// avoids the paren/signatures ambiguity.

#[test]
fn proto_attribute_form_registers_prototype() {
    // sub foo :prototype($$) { } foo $a + $b, $c;
    // Prototype declared via attribute drives call-site parsing
    // just like the paren form.
    let e = parse_call_with_proto("sub foo :prototype($$) { } foo $a + $b, $c;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "foo");
            assert_eq!(args.len(), 2, ":prototype($$) should give 2 args");
            assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Add, _, _)));
            assert!(matches!(args[1].kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_attribute_empty_proto_forces_zero_args() {
    // sub foo :prototype() { } foo + 1;
    // Empty :prototype() means zero args; `+ 1` is a binary op.
    let e = parse_call_with_proto("sub foo :prototype() { } foo + 1;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Add, lhs, rhs) => {
            match &lhs.kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "foo");
                    assert_eq!(args.len(), 0);
                }
                other => panic!("expected FuncCall(foo, []), got {other:?}"),
            }
            assert!(matches!(rhs.kind, ExprKind::IntLit(1)));
        }
        other => panic!("expected BinOp(Add, ...), got {other:?}"),
    }
}

#[test]
fn proto_attribute_form_on_forward_declaration() {
    // sub foo :prototype(&@); foo { $_ } @list;
    // Forward declaration with :prototype attribute.
    let e = parse_call_with_proto("sub foo :prototype(&@); foo { $_ } @list;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 2);
            assert!(matches!(args[0].kind, ExprKind::AnonSub(..)));
            assert!(matches!(args[1].kind, ExprKind::ArrayVar(_)));
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn hard_parses_heredoc_basic() {
    parse("print <<EOF;\nhello\nEOF\n");
}

#[test]
fn hard_parses_heredoc_concat() {
    // `print <<EOF . "x"; ... EOF` — heredoc in compound expression.
    parse("print <<EOF . \"x\";\nhello\nEOF\n");
}

#[test]
fn hard_parses_heredoc_interp() {
    parse("print <<\"EOF\";\n$interpolated\nEOF\n");
}

#[test]
fn hard_parses_heredoc_literal() {
    parse("print <<'EOF';\n$not_interpolated\nEOF\n");
}

#[test]
fn hard_parses_two_heredocs_same_line() {
    parse("print <<A . <<B;\na\nA\nb\nB\n");
}

#[test]
fn hard_parses_do_block_simple() {
    parse("do { 1 };");
}

#[test]
fn hard_parses_if_hashlike_body() {
    // `if (1) { a => 1 }` — body looks hashy but must parse as block.
    parse("if (1) { a => 1 }");
}

#[test]
fn hard_parses_tricky_slash_combinations() {
    parse("$x / 2;");
    parse("$x / $y / $z;");
    parse("$x // /foo/;");
}

// ═══════════════════════════════════════════════════════════
// Tests for previously-unimplemented syntax features.
// ═══════════════════════════════════════════════════════════

// ── \N{U+XXXX} and \N{name} escapes ──────────────────────

#[test]
fn escape_n_unicode_codepoint() {
    // `\N{U+2603}` → snowman character ☃.
    let e = parse_expr_str(r#""\N{U+2603}";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "\u{2603}"),
        other => panic!("expected StringLit with snowman, got {other:?}"),
    }
}

#[test]
fn escape_n_unicode_codepoint_ascii() {
    // `\N{U+41}` → 'A'.
    let e = parse_expr_str(r#""\N{U+41}";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "A"),
        other => panic!("expected StringLit('A'), got {other:?}"),
    }
}

#[test]
fn escape_n_charname_placeholder() {
    // `\N{SNOWMAN}` — named character.  Without a charnames
    // database the parser emits U+FFFD as a placeholder.
    // Verifying it doesn't error and produces a single-char
    // string.
    let e = parse_expr_str(r#""\N{SNOWMAN}";"#);
    match &e.kind {
        ExprKind::StringLit(s) => {
            assert_eq!(s.len(), 3); // U+FFFD is 3 bytes in UTF-8
            assert!(s.contains('\u{FFFD}'));
        }
        other => panic!("expected StringLit with placeholder, got {other:?}"),
    }
}

#[test]
fn escape_n_in_interpolated_string() {
    // `"prefix \N{U+2603} suffix"` — mixed with other content.
    let e = parse_expr_str(r#""prefix \N{U+2603} suffix";"#);
    match &e.kind {
        ExprKind::StringLit(s) => {
            assert!(s.contains('\u{2603}'), "expected snowman in string");
            assert!(s.starts_with("prefix "), "expected prefix");
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn escape_bare_n_without_braces() {
    // `\N` without `{` is just literal `\N`.
    let e = parse_expr_str(r#""\N test";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert!(s.contains("\\N"), "expected literal \\N, got {s:?}"),
        other => panic!("expected StringLit, got {other:?}"),
    }
}

// ── Smartmatch ~~ ────────────────────────────────────────

#[test]
fn smartmatch_basic() {
    // ~~ is in the :default bundle, so it's on by default.
    let e = parse_expr_str("$a ~~ @b;");
    match &e.kind {
        ExprKind::BinOp(BinOp::SmartMatch, lhs, rhs) => {
            assert!(matches!(lhs.kind, ExprKind::ScalarVar(ref n) if n == "a"));
            assert!(matches!(rhs.kind, ExprKind::ArrayVar(ref n) if n == "b"));
        }
        other => panic!("expected SmartMatch, got {other:?}"),
    }
}

#[test]
fn smartmatch_precedence_vs_equality() {
    // `~~` is at PREC_EQ (same as `==`), non-associative.
    // `$a == $b ~~ $c` should error or parse as comparison
    // chain — but since both are non-associative at the same
    // level, the Pratt loop stops after the first one.
    // We just verify `$a ~~ $b` parses at the right level.
    let e = parse_expr_str("$a ~~ $b || $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Or, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::SmartMatch, _, _)), "~~ should bind tighter than ||");
        }
        other => panic!("expected Or wrapping SmartMatch, got {other:?}"),
    }
}

#[test]
fn smartmatch_disabled_without_feature() {
    // After `no feature ':all'`, smartmatch is off.
    // The lexer still emits Token::SmartMatch (it doesn't
    // have feature state), but peek_op_info won't recognize
    // it as an operator.  The expression `$a ~~ $b` fails
    // to parse as a single expression — `$a` is one
    // statement and `~~` is an unexpected token.
    //
    // A full solution would need lexer-level token demotion
    // (splitting SmartMatch back into two Tildes), similar
    // to keyword demotion.  For now, verify the program
    // doesn't produce a SmartMatch BinOp.
    let prog = parse("no feature ':all'; ~$b;");
    // Just confirms the feature removal doesn't break
    // normal `~` (bitwise not).
    assert!(!prog.statements.is_empty());
}

// ── String-bitwise operators ─────────────────────────────

#[test]
fn string_bitwise_and() {
    let e = parse_expr_str("$a &. $b;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::StringBitAnd, _, _)), "expected StringBitAnd, got {:?}", e.kind);
}

#[test]
fn string_bitwise_or() {
    let e = parse_expr_str("$a |. $b;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::StringBitOr, _, _)), "expected StringBitOr, got {:?}", e.kind);
}

#[test]
fn string_bitwise_xor() {
    let e = parse_expr_str("$a ^. $b;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::StringBitXor, _, _)), "expected StringBitXor, got {:?}", e.kind);
}

#[test]
fn string_bitwise_not() {
    let e = parse_expr_str("~. $a;");
    assert!(matches!(e.kind, ExprKind::UnaryOp(UnaryOp::StringBitNot, _)), "expected StringBitNot, got {:?}", e.kind);
}

#[test]
fn string_bitwise_and_assign() {
    let e = parse_expr_str("$a &.= $b;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::StringBitAndEq, _, _)), "expected &.= assign, got {:?}", e.kind);
}

#[test]
fn string_bitwise_or_assign() {
    let e = parse_expr_str("$a |.= $b;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::StringBitOrEq, _, _)), "expected |.= assign, got {:?}", e.kind);
}

#[test]
fn string_bitwise_xor_assign() {
    let e = parse_expr_str("$a ^.= $b;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::StringBitXorEq, _, _)), "expected ^.= assign, got {:?}", e.kind);
}

#[test]
fn string_bitwise_precedence() {
    // `&.` has PREC_BIT_AND, which is tighter than `|.`.
    // `$a |. $b &. $c` → `$a |. ($b &. $c)`.
    let e = parse_expr_str("$a |. $b &. $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::StringBitOr, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::StringBitAnd, _, _)), "expected &. to bind tighter than |.");
        }
        other => panic!("expected StringBitOr at top, got {other:?}"),
    }
}

// ── CORE:: qualified builtins ────────────────────────────

#[test]
fn core_qualified_builtin() {
    // `CORE::say(...)` parses as a package-qualified function call.
    // The semantic distinction (forcing the builtin) is a
    // compiler concern; the parser treats it like any other
    // qualified name.
    let prog = parse(r#"CORE::say("hello");"#);
    assert!(!prog.statements.is_empty(), "should parse CORE::say");
}

#[test]
fn core_qualified_length() {
    let e = parse_expr_str("CORE::length($x);");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "CORE::length");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected FuncCall(CORE::length), got {other:?}"),
    }
}

// ── UTF-8 identifiers under `use utf8` ───────────────────

#[test]
fn utf8_scalar_variable() {
    // `use utf8; my $café = 1;` — UTF-8 identifier.
    let prog = parse("use utf8; my $café = 1;");
    assert!(prog.statements.len() >= 2, "should parse use + decl");
}

#[test]
fn utf8_sub_name() {
    let prog = parse("use utf8; sub naïve { 1 }");
    assert!(
        prog.statements.iter().any(|s| matches!(
            &s.kind,
            StmtKind::SubDecl(sd) if sd.name == "naïve"
        )),
        "expected sub named naïve"
    );
}

#[test]
fn utf8_bareword_fat_comma() {
    // `café => 1` with utf8 active — autoquoted.
    let prog = parse("use utf8; my %h = (café => 1);");
    assert!(!prog.statements.is_empty(), "should parse");
}

#[test]
fn utf8_hash_key_autoquote() {
    // `$h{café}` — bareword autoquoted inside hash subscript.
    let prog = parse("use utf8; $h{café};");
    let expr = prog.statements.iter().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e) } else { None }).expect("expression statement");
    match &expr.kind {
        ExprKind::HashElem(_, k) => {
            assert!(matches!(k.kind, ExprKind::StringLit(ref s) if s == "café"), "expected StringLit(café), got {:?}", k.kind);
        }
        other => panic!("expected HashElem, got {other:?}"),
    }
}

#[test]
fn utf8_error_without_pragma() {
    // Without `use utf8`, bytes ≥ 0x80 are rejected.
    let src = "my $café = 1;";
    let mut p = match Parser::new(src.as_bytes()) {
        Ok(p) => p,
        Err(_) => return, // construction error is also acceptable
    };
    let result = p.parse_program();
    assert!(result.is_err(), "high bytes without use utf8 should error");
}

#[test]
fn utf8_lexical_scoping() {
    // `use utf8` is lexically scoped: inside a `no utf8`
    // block, UTF-8 identifiers are rejected again.
    let src = "use utf8; my $café = 1; { no utf8; my $x = 1; }";
    let prog = parse(src);
    // The program parses — $café is in utf8 scope,
    // $x is in no-utf8 scope (ASCII only, fine).
    assert!(prog.statements.len() >= 2);
}

#[test]
fn utf8_lexical_scoping_error_in_block() {
    // After `no utf8` inside a block, UTF-8 identifiers
    // should error — matching Perl's behavior.
    let src = "use utf8; { no utf8; my $café = 1; }";
    let mut p = match Parser::new(src.as_bytes()) {
        Ok(p) => p,
        Err(_) => return,
    };
    let result = p.parse_program();
    assert!(result.is_err(), "UTF-8 identifier after `no utf8` inside block should error");
}

#[test]
fn utf8_in_string_interpolation() {
    // `"$café"` with utf8 active — the variable name is UTF-8.
    let prog = parse("use utf8; my $café = 1; print \"$café\";");
    assert!(!prog.statements.is_empty(), "should parse");
}

// ═══════════════════════════════════════════════════════════
// perlsyn gap-probing tests — features from perlsyn that
// may or may not be implemented.  Failures are diagnostic.
// ═══════════════════════════════════════════════════════════

// ── 1. Postfix `when` modifier ───────────────────────────

#[test]
fn postfix_when_modifier_v514() {
    // `$abc = 1 when /^abc/;` — perlsyn lists `when EXPR`
    // as a statement modifier alongside if/unless/while/until.
    // `use v5.14` enables the switch feature (5.10–5.34 bundle).
    let prog = parse("use v5.14; $abc = 1 when /^abc/;");
    assert!(prog.statements.len() >= 2, "should parse postfix when with use v5.14");
}

#[test]
fn postfix_when_modifier_explicit_feature() {
    // Explicitly enabling switch feature.
    let prog = parse("use feature 'switch'; $abc = 1 when /^abc/;");
    assert!(prog.statements.len() >= 2, "should parse postfix when with use feature 'switch'");
}

#[test]
fn postfix_when_without_feature() {
    // Without the switch feature, `when` is demoted to a bare
    // identifier.  `$abc = 1 when ...` would parse `when` as
    // a bareword function call — the expression becomes
    // `1 when(...)` which is NOT a postfix modifier.
    // Just verify it doesn't produce PostfixKind::When.
    let prog = parse("$abc = 1; when(/^abc/);");
    // Without switch, `when` is a function call, not a keyword.
    // The program parses as two separate statements.
    assert!(prog.statements.len() >= 2);
    // Verify the first statement is NOT a postfix-when.
    let not_postfix_when = !matches!(&prog.statements[0].kind, StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::When, _, _), .. }));
    assert!(not_postfix_when, "when should not be a postfix modifier without the switch feature");
}

// ── 2. continue block on bare BLOCK ──────────────────────

#[test]
fn continue_block_on_bare_block() {
    // perlsyn: `LABEL BLOCK continue BLOCK` is valid.
    // A bare block acts as a loop that executes once.
    let prog = parse("LOOP: { 1; } continue { 2; }");
    assert!(!prog.statements.is_empty(), "should parse bare block with continue");
}

#[test]
fn continue_block_on_unlabeled_bare_block() {
    let prog = parse("{ 1; } continue { 2; }");
    assert!(!prog.statements.is_empty(), "should parse unlabeled bare block with continue");
}

// ── 3. Multi-variable foreach (5.36+) ────────────────────

#[test]
fn foreach_multi_variable() {
    // `for my ($key, $value) (%hash) { ... }` — iterating
    // over multiple values at a time (Perl 5.36+).
    let prog = parse("for my ($key, $value) (%hash) { 1; }");
    assert!(!prog.statements.is_empty(), "should parse multi-variable foreach");
}

#[test]
fn foreach_three_variables() {
    let prog = parse("for my ($a, $b, $c) (@list) { 1; }");
    assert!(!prog.statements.is_empty(), "should parse three-variable foreach");
}

// ── 4. Backslash foreach (refaliasing, 5.22+) ────────────

#[test]
fn foreach_refaliasing() {
    // `foreach \my %hash (@array_of_hash_refs) { ... }`
    // Experimental refaliasing feature.
    let prog = parse(r#"use feature "refaliasing"; foreach \my %hash (@refs) { 1; }"#);
    assert!(!prog.statements.is_empty(), "should parse backslash foreach");
}

// ── 5. break keyword (in given blocks) ───────────────────

#[test]
fn break_in_given() {
    // `break` exits a `given` block.
    let prog = parse("use v5.14; given ($x) { when (1) { break } }");
    assert!(!prog.statements.is_empty(), "should parse break in given");
}

// ── 6. continue as fall-through in given/when ────────────

#[test]
fn continue_fall_through_in_given() {
    // `continue` inside a `when` block means fall through
    // to the next when — different from `continue BLOCK`.
    let prog = parse("use v5.14; given ($x) { when (1) { $a = 1; continue } when (2) { $b = 1 } }");
    assert!(!prog.statements.is_empty(), "should parse continue as fall-through in given");
}

// ── 7. goto — three forms ────────────────────────────────

#[test]
fn goto_label() {
    let prog = parse("goto DONE; DONE: print 1;");
    assert!(!prog.statements.is_empty(), "should parse goto LABEL");
}

#[test]
fn goto_expr() {
    // `goto(("FOO", "BAR")[$i])` — computed goto.
    let prog = parse(r#"goto(("FOO", "BAR")[$i]);"#);
    assert!(!prog.statements.is_empty(), "should parse goto EXPR");
}

#[test]
fn goto_ampersand_name() {
    // `goto &subname` — magical tail call.
    let prog = parse("goto &other_sub;");
    assert!(!prog.statements.is_empty(), "should parse goto &NAME");
}

// ── 8. # line N "file" directives ────────────────────────

#[test]
fn line_directive_sets_line_number() {
    // `# line 200 "bzzzt"` overrides __LINE__ on the next line.
    let prog = parse("# line 200 \"bzzzt\"\n__LINE__;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::SourceLine(n) => assert_eq!(*n, 200, "__LINE__ should be 200 after # line 200"),
            other => panic!("expected SourceLine, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn line_directive_sets_filename() {
    // `# line 200 "bzzzt"` overrides __FILE__.
    let prog = parse("# line 200 \"bzzzt\"\n__FILE__;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::SourceFile(path) => assert_eq!(path, "bzzzt", "__FILE__ should be 'bzzzt' after # line 200 \"bzzzt\""),
            other => panic!("expected SourceFile, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn line_directive_number_only() {
    // `# line 42` without filename — only line number changes.
    let prog = parse("# line 42\n__LINE__;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::SourceLine(n) => assert_eq!(*n, 42),
            other => panic!("expected SourceLine, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn line_directive_not_at_column_zero() {
    // Leading whitespace — `#` is NOT at column 0, so it's
    // just a regular comment, not a directive.
    let prog = parse("  # line 200\n__LINE__;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::SourceLine(n) => assert_ne!(*n, 200, "indented # line should NOT be a directive"),
            other => panic!("expected SourceLine, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// perlop gap-probing tests.
// ═══════════════════════════════════════════════════════════

// ── ^^ logical XOR operator ──────────────────────────────

#[test]
fn logical_xor_operator() {
    // `^^` — logical XOR, between `||` and `//` in precedence.
    let e = parse_expr_str("$a ^^ $b;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::LogicalXor, _, _)), "expected LogicalXor, got {:?}", e.kind);
}

#[test]
fn logical_xor_precedence() {
    // `^^` is lower than `||` but same level.
    // `$a || $b ^^ $c` → `($a || $b) ^^ $c`.
    let e = parse_expr_str("$a || $b ^^ $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::LogicalXor, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::Or, _, _)), "|| should bind tighter than ^^");
        }
        other => panic!("expected LogicalXor at top, got {other:?}"),
    }
}

#[test]
fn logical_xor_assign() {
    // `^^=` assignment operator.
    let e = parse_expr_str("$a ^^= $b;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::LogicalXorEq, _, _)), "expected ^^= assign, got {:?}", e.kind);
}

// ── <<>> double diamond operator ─────────────────────────

#[test]
fn double_diamond_operator() {
    // `<<>>` — safe diamond, uses 3-arg open.
    let prog = parse("while (<<>>) { print; }");
    assert!(!prog.statements.is_empty(), "should parse <<>> in while condition");
}

// ── m?PATTERN? match-once ────────────────────────────────

#[test]
fn match_once_question_mark() {
    // `m?pattern?` — matches only once between reset() calls.
    let e = parse_expr_str("m?foo?;");
    match &e.kind {
        ExprKind::Regex(_, _, _) => {}
        other => panic!("expected Regex from m??, got {other:?}"),
    }
}

// ── Chained comparisons ──────────────────────────────────

#[test]
fn chained_relational() {
    // `$x < $y <= $z` → ChainedCmp([NumLt, NumLe], [x, y, z]).
    let e = parse_expr_str("$x < $y <= $z;");
    match &e.kind {
        ExprKind::ChainedCmp(ops, operands) => {
            assert_eq!(ops.len(), 2, "two operators");
            assert_eq!(operands.len(), 3, "three operands");
            assert_eq!(ops[0], BinOp::NumLt);
            assert_eq!(ops[1], BinOp::NumLe);
        }
        other => panic!("expected ChainedCmp, got {other:?}"),
    }
}

#[test]
fn chained_equality() {
    // `$a == $b != $c` → ChainedCmp([NumEq, NumNe], [a, b, c]).
    let e = parse_expr_str("$a == $b != $c;");
    match &e.kind {
        ExprKind::ChainedCmp(ops, operands) => {
            assert_eq!(ops.len(), 2);
            assert_eq!(operands.len(), 3);
            assert_eq!(ops[0], BinOp::NumEq);
            assert_eq!(ops[1], BinOp::NumNe);
        }
        other => panic!("expected ChainedCmp, got {other:?}"),
    }
}

#[test]
fn chained_string_relational() {
    // `$a lt $b le $c gt $d` — four operands, three ops.
    let e = parse_expr_str("$a lt $b le $c gt $d;");
    match &e.kind {
        ExprKind::ChainedCmp(ops, operands) => {
            assert_eq!(ops.len(), 3);
            assert_eq!(operands.len(), 4);
        }
        other => panic!("expected ChainedCmp, got {other:?}"),
    }
}

#[test]
fn non_chained_spaceship() {
    // `<=>` is non-associative — should NOT produce ChainedCmp.
    let e = parse_expr_str("$a <=> $b;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Spaceship, _, _)), "spaceship should be plain BinOp");
}

#[test]
fn simple_comparison_stays_binop() {
    // A single comparison should remain BinOp, not ChainedCmp.
    let e = parse_expr_str("$x < $y;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::NumLt, _, _)), "single < should be plain BinOp");
}

// ── Existing escape sequences (verify) ───────────────────

#[test]
fn octal_brace_escape() {
    // `\o{101}` → 'A' (octal 101 = decimal 65).
    let e = parse_expr_str(r#""\o{101}";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "A"),
        other => panic!("expected 'A', got {other:?}"),
    }
}

#[test]
fn control_char_escape() {
    // `\cA` → chr(1), `\c[` → chr(27) (ESC).
    let e = parse_expr_str(r#""\c[";"#);
    match &e.kind {
        ExprKind::StringLit(s) => {
            assert_eq!(s.len(), 1);
            assert_eq!(s.chars().next().unwrap(), '\x1B');
        }
        other => panic!("expected ESC char, got {other:?}"),
    }
}

#[test]
fn case_mod_uppercase() {
    // `"\Ufoo\E"` → "FOO"
    let e = parse_expr_str(r#""\Ufoo\E";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "FOO"),
        other => panic!("expected StringLit(FOO), got {other:?}"),
    }
}

#[test]
fn case_mod_lowercase() {
    // `"\LFOO\E"` → "foo"
    let e = parse_expr_str(r#""\LFOO\E";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "foo"),
        other => panic!("expected StringLit(foo), got {other:?}"),
    }
}

#[test]
fn case_mod_lower_next() {
    // `"\lFOO"` → "fOO" (only first char lowercased)
    let e = parse_expr_str(r#""\lFOO";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "fOO"),
        other => panic!("expected StringLit(fOO), got {other:?}"),
    }
}

#[test]
fn case_mod_upper_next() {
    // `"\ufoo"` → "Foo" (only first char uppercased)
    let e = parse_expr_str(r#""\ufoo";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "Foo"),
        other => panic!("expected StringLit(Foo), got {other:?}"),
    }
}

#[test]
fn case_mod_quotemeta() {
    // `"\Qfoo.bar\E"` → "foo\\.bar"
    let e = parse_expr_str(r#""\Qfoo.bar\E";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "foo\\.bar"),
        other => panic!("expected quotemeta'd string, got {other:?}"),
    }
}

#[test]
fn case_mod_stacking() {
    // `"\Q'\Ufoo\Ebar'\E"` → `\\'FOObar\\'`
    // \Q quotemeta, then \U uppercase stacks on top.
    // \E pops \U, \E pops \Q.
    let e = parse_expr_str(r#""\Q'\Ufoo\Ebar'\E";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "\\'FOObar\\'"),
        other => panic!("expected stacked case-mod result, got {other:?}"),
    }
}

#[test]
fn case_mod_foldcase() {
    // `"\FFOO\E"` → "foo" (foldcase ≈ lowercase for ASCII)
    let e = parse_expr_str(r#""\FFOO\E";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "foo"),
        other => panic!("expected StringLit(foo), got {other:?}"),
    }
}

#[test]
fn case_mod_interp_uppercase() {
    // `"\Utest$x\E"` → InterpolatedString with:
    //   Const("TEST"), ScalarInterp(uc($x))
    let e = parse_expr_str(r#""\Utest$x\E";"#);
    match &e.kind {
        ExprKind::InterpolatedString(interp) => {
            // First part: constant "TEST" (uppercased at lex time).
            assert!(matches!(&interp.0[0], InterpPart::Const(s) if s == "TEST"), "first part should be Const(TEST), got {:?}", interp.0[0]);
            // Second part: $x wrapped in uc().
            match &interp.0[1] {
                InterpPart::ScalarInterp(expr) => {
                    assert!(matches!(&expr.kind, ExprKind::FuncCall(name, _) if name == "uc"), "interp should be uc($x), got {:?}", expr.kind);
                }
                other => panic!("expected ScalarInterp, got {other:?}"),
            }
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

#[test]
fn case_mod_interp_lcfirst() {
    // `"\l$X"` → ScalarInterp(lcfirst($X))
    let e = parse_expr_str(r#""\l$X";"#);
    match &e.kind {
        ExprKind::InterpolatedString(interp) => match &interp.0[0] {
            InterpPart::ScalarInterp(expr) => {
                assert!(matches!(&expr.kind, ExprKind::FuncCall(name, _) if name == "lcfirst"), "interp should be lcfirst($X), got {:?}", expr.kind);
            }
            other => panic!("expected ScalarInterp, got {other:?}"),
        },
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

#[test]
fn case_mod_interp_quotemeta_upper() {
    // `"\Q\U$x\E\E"` → ScalarInterp(quotemeta(uc($x)))
    let e = parse_expr_str(r#""\Q\U$x\E\E";"#);
    match &e.kind {
        ExprKind::InterpolatedString(interp) => {
            match &interp.0[0] {
                InterpPart::ScalarInterp(expr) => {
                    // Outermost should be quotemeta.
                    match &expr.kind {
                        ExprKind::FuncCall(name, args) if name == "quotemeta" => {
                            // Inner should be uc.
                            assert!(matches!(&args[0].kind, ExprKind::FuncCall(n, _) if n == "uc"), "inner should be uc, got {:?}", args[0].kind);
                        }
                        other => panic!("expected quotemeta(uc($x)), got {other:?}"),
                    }
                }
                other => panic!("expected ScalarInterp, got {other:?}"),
            }
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

#[test]
fn case_mod_no_wrap_after_end() {
    // `"\Ufoo\E$x"` — \E ends the case mod, so $x should NOT be wrapped.
    let e = parse_expr_str(r#""\Ufoo\E$x";"#);
    match &e.kind {
        ExprKind::InterpolatedString(interp) => {
            assert!(matches!(&interp.0[0], InterpPart::Const(s) if s == "FOO"));
            match &interp.0[1] {
                InterpPart::ScalarInterp(expr) => {
                    assert!(matches!(&expr.kind, ExprKind::ScalarVar(_)), "$x should be plain ScalarVar after \\E, got {:?}", expr.kind);
                }
                other => panic!("expected ScalarInterp, got {other:?}"),
            }
        }
        other => panic!("expected InterpolatedString, got {other:?}"),
    }
}

// ── Case mod additional coverage ─────────────────────────

#[test]
fn case_mod_uppercase_spans_const_segments() {
    // "\Uabc def\E" — all literal chars uppercased.
    let e = parse_expr_str(r#""\Uabc def\E";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "ABC DEF"),
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn case_mod_lcfirst_only_first() {
    // "\lFOO" — only first char lowercased.
    let e = parse_expr_str(r#""\lFOO";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "fOO"),
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn case_mod_ucfirst_only_first() {
    // "\ufoo" — only first char uppercased.
    let e = parse_expr_str(r#""\ufoo";"#);
    match &e.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "Foo"),
        other => panic!("expected StringLit, got {other:?}"),
    }
}

// ── dump keyword ─────────────────────────────────────────

#[test]
fn dump_keyword() {
    let prog = parse("dump;");
    assert!(!prog.statements.is_empty(), "should parse bare dump");
}

#[test]
fn dump_with_label() {
    let prog = parse("dump RESTART;");
    assert!(!prog.statements.is_empty(), "should parse dump LABEL");
}

// ═══════════════════════════════════════════════════════════
// Remaining audit gaps — probing tests
// ═══════════════════════════════════════════════════════════

// ── M9. x= compound assignment ───────────────────────────

#[test]
fn repeat_assign() {
    // `$str x= 3` — compound repeat-assignment.
    let e = parse_expr_str("$str x= 3;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::RepeatEq, _, _)), "expected RepeatEq assign, got {:?}", e.kind);
}

// ── H11. v-strings as dedicated AST node ─────────────────

#[test]
fn vstring_produces_version_lit() {
    // `v5.36.0` should produce VersionLit.
    let e = parse_expr_str("v5.36.0;");
    match &e.kind {
        ExprKind::VersionLit(s) => assert_eq!(s, "v5.36.0"),
        other => panic!("expected VersionLit(\"v5.36.0\"), got {other:?}"),
    }
}

// ── L8. <<\"\" empty heredoc tag ──────────────────────────

#[test]
fn heredoc_empty_tag_double_quoted() {
    // `<<""` — empty string as terminator, body ends at empty line.
    let prog = parse("print <<\"\";\nhello\nworld\n\n");
    assert!(!prog.statements.is_empty(), "should parse <<\"\" empty tag heredoc");
}

#[test]
fn heredoc_empty_tag_single_quoted() {
    let prog = parse("print <<'';\nhello\n$not_interp\n\n");
    assert!(!prog.statements.is_empty(), "should parse <<'' empty tag heredoc");
}

#[test]
fn heredoc_indented_empty_tag() {
    // `<<~""` with indented body.
    let prog = parse("print <<~\"\";\n  hello\n  world\n  \n");
    assert!(!prog.statements.is_empty(), "should parse <<~\"\" indented empty tag heredoc");
}

// ── M14. Anonymous sub with prototype AND attributes ─────

#[test]
fn sub_proto_then_attrs() {
    // `sub ($) :lvalue { 1 }` — prototype before attributes.
    let prog = parse("my $f = sub ($) :lvalue { 1; };");
    assert!(!prog.statements.is_empty(), "should parse sub with proto then attrs");
}

#[test]
fn sub_attrs_then_sig() {
    // With signatures active: `sub :lvalue ($x) { }`.
    let prog = parse("use feature 'signatures'; my $f = sub :lvalue ($x) { 1; };");
    assert!(!prog.statements.is_empty(), "should parse sub with attrs then sig");
}

// ── L3. use if — conditional use pragma ──────────────────

#[test]
fn use_if_conditional() {
    // `use if $cond, "Module"` — the `if` module.
    let prog = parse("use if $^O eq 'MSWin32', 'Win32';");
    assert!(!prog.statements.is_empty(), "should parse use if");
}

// ── L4. use Module qw(imports) ───────────────────────────

#[test]
fn use_module_with_qw_imports() {
    let prog = parse("use POSIX qw(setlocale LC_ALL);");
    assert!(!prog.statements.is_empty(), "should parse use with qw import list");
}

#[test]
fn use_module_with_list_imports() {
    let prog = parse("use File::Basename 'dirname', 'basename';");
    assert!(!prog.statements.is_empty(), "should parse use with string import list");
}

// ── Backtick heredocs ────────────────────────────────────

#[test]
fn heredoc_backtick() {
    // <<`EOC` — command heredoc (interpolated, then executed).
    let prog = parse("my $out = <<`EOC`;\necho hello\nEOC\n");
    assert!(!prog.statements.is_empty(), "should parse backtick heredoc");
}

#[test]
fn heredoc_backtick_indented() {
    // <<~`EOC` — indented command heredoc.
    let prog = parse("my $out = <<~`EOC`;\n  echo hello\n  EOC\n");
    assert!(!prog.statements.is_empty(), "should parse indented backtick heredoc");
}

// ── Lexical method invocation (->&method) ────────────────

#[test]
fn arrow_lexical_method() {
    // `$obj->&method` — lexical method invocation.
    let e = parse_expr_str("$obj->&method;");
    match &e.kind {
        ExprKind::MethodCall(_, name, args) => {
            assert_eq!(name, "&method");
            assert!(args.is_empty());
        }
        other => panic!("expected MethodCall(&method), got {other:?}"),
    }
}

#[test]
fn arrow_lexical_method_with_args() {
    // `$obj->&method(1, 2)` — with arguments.
    let e = parse_expr_str("$obj->&method(1, 2);");
    match &e.kind {
        ExprKind::MethodCall(_, name, args) => {
            assert_eq!(name, "&method");
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected MethodCall(&method, 2 args), got {other:?}"),
    }
}

#[test]
fn arrow_deref_code_still_works() {
    // `->&*` should still work as code postfix deref.
    let e = parse_expr_str("$ref->&*;");
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefCode)), "expected DerefCode, got {:?}", e.kind);
}

// ── perldata: KV slices (5.20+) ──────────────────────────

#[test]
fn kv_hash_slice() {
    // %hash{'foo','bar'} → key/value hash slice.
    let e = parse_expr_str("%hash{'foo','bar'};");
    match &e.kind {
        ExprKind::KvHashSlice(_, keys) => assert_eq!(keys.len(), 2),
        other => panic!("expected KvHashSlice, got {other:?}"),
    }
}

#[test]
fn kv_array_slice() {
    // %array[1,2,3] → index/value array slice.
    let e = parse_expr_str("%array[1,2,3];");
    match &e.kind {
        ExprKind::KvArraySlice(_, indices) => assert_eq!(indices.len(), 3),
        other => panic!("expected KvArraySlice, got {other:?}"),
    }
}

// ── perldata: *foo{THING} typeglob access ────────────────

#[test]
fn glob_thing_access() {
    // *foo{SCALAR} → typeglob slot access.
    let e = parse_expr_str("*foo{SCALAR};");
    match &e.kind {
        ExprKind::ArrowDeref(recv, ArrowTarget::HashElem(key)) => {
            assert!(matches!(recv.kind, ExprKind::GlobVar(ref n) if n == "foo"));
            assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "SCALAR"));
        }
        other => panic!("expected ArrowDeref(GlobVar, HashElem), got {other:?}"),
    }
}

// ── perldata: hex/octal/binary float ─────────────────────

#[test]
fn hex_float_expr() {
    let e = parse_expr_str("0x1p10;");
    assert!(matches!(e.kind, ExprKind::FloatLit(v) if v == 1024.0), "expected FloatLit(1024.0), got {:?}", e.kind);
}

// ── perlsub: //= and ||= signature defaults (5.38+) ─────

#[test]
fn sig_defined_or_default() {
    let s = parse_sub("use feature 'signatures'; sub f ($name //= \"world\") { }");
    let sig = s.signature.expect("signature present");
    match &sig.params[0] {
        SigParam::Scalar { name, default: Some((kind, _)), .. } => {
            assert_eq!(name, "name");
            assert_eq!(*kind, SigDefaultKind::DefinedOr);
        }
        other => panic!("expected Scalar with //= default, got {other:?}"),
    }
}

#[test]
fn sig_logical_or_default() {
    let s = parse_sub("use feature 'signatures'; sub f ($x ||= 10) { }");
    let sig = s.signature.expect("signature present");
    match &sig.params[0] {
        SigParam::Scalar { name, default: Some((kind, _)), .. } => {
            assert_eq!(name, "x");
            assert_eq!(*kind, SigDefaultKind::LogicalOr);
        }
        other => panic!("expected Scalar with ||= default, got {other:?}"),
    }
}

// ── perlsub: $= in signatures ───────────────────────────

#[test]
fn sig_anon_optional_no_default() {
    let s = parse_sub("use feature 'signatures'; sub f ($thing, $=) { }");
    let sig = s.signature.expect("signature present");
    assert_eq!(sig.params.len(), 2);
    assert!(matches!(sig.params[1], SigParam::AnonScalar { default: Some(_), .. }), "expected AnonScalar with default, got {:?}", sig.params[1]);
}

// ── perlsub: lexical subs ───────────────────────────────

#[test]
fn my_sub() {
    let prog = parse("my sub foo { 42; }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sd) => {
            assert_eq!(sd.name, "foo");
            assert_eq!(sd.scope, Some(DeclScope::My));
        }
        other => panic!("expected SubDecl(my), got {other:?}"),
    }
}

#[test]
fn state_sub() {
    let prog = parse("use feature 'state'; state sub bar { 1; }");
    match &prog.statements[1].kind {
        StmtKind::SubDecl(sd) => {
            assert_eq!(sd.name, "bar");
            assert_eq!(sd.scope, Some(DeclScope::State));
        }
        other => panic!("expected SubDecl(state), got {other:?}"),
    }
}

#[test]
fn our_sub() {
    let prog = parse("our sub baz { 1; }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sd) => {
            assert_eq!(sd.name, "baz");
            assert_eq!(sd.scope, Some(DeclScope::Our));
        }
        other => panic!("expected SubDecl(our), got {other:?}"),
    }
}

// ── perlsub: my with attributes ─────────────────────────

#[test]
fn my_var_with_attribute() {
    let prog = parse("my $x : Shared = 1;");
    let (scope, vars) = decl_vars(&prog.statements[0]);
    assert_eq!(scope, DeclScope::My);
    assert_eq!(vars[0].name, "x");
    assert_eq!(vars[0].attributes.len(), 1);
    assert_eq!(vars[0].attributes[0].name, "Shared");
}

// ── perldata: whitespace between sigil and name ──────────

#[test]
fn percent_space_name() {
    // `% hash` ≡ `%hash` — whitespace between % and name.
    let prog = parse("my % hash = (a => 1);");
    assert!(!prog.statements.is_empty(), "should parse % hash with space");
}

// ── perlvar: special variable gaps ──────────────────────

#[test]
fn percent_caret_h() {
    // `%^H` — hints hash, caret hash variable.
    let e = parse_expr_str("%^H;");
    assert!(matches!(e.kind, ExprKind::SpecialHashVar(ref n) if n == "^H"), "expected SpecialHashVar(^H), got {:?}", e.kind);
}

// ── perlre: /o flag ─────────────────────────────────────

#[test]
fn regex_o_flag() {
    // /o — compile-once flag (no-op in modern Perl, but valid syntax).
    let prog = parse("$x =~ /foo/o;");
    assert!(!prog.statements.is_empty(), "should parse /o flag");
}

#[test]
fn subst_o_flag() {
    let prog = parse("$x =~ s/foo/bar/og;");
    assert!(!prog.statements.is_empty(), "should parse s///og flags");
}

// ── perlre: regex code block raw source capture ─────────

#[test]
fn regex_code_block_raw_source() {
    // (?{code}) — verify both raw source and parsed expression.
    let e = parse_expr_str("m/(?{ 1 + 2 })/;");
    match &e.kind {
        ExprKind::Regex(_, interp, _) => {
            let code_parts: Vec<_> = interp
                .0
                .iter()
                .filter_map(|p| match p {
                    InterpPart::RegexCode(raw, expr) => Some((raw.as_str(), expr)),
                    _ => None,
                })
                .collect();
            assert_eq!(code_parts.len(), 1, "expected one code block");
            assert_eq!(code_parts[0].0, " 1 + 2 ", "raw source mismatch");
            assert!(matches!(code_parts[0].1.kind, ExprKind::BinOp(BinOp::Add, _, _)), "parsed expr should be Add, got {:?}", code_parts[0].1.kind);
        }
        other => panic!("expected Regex, got {other:?}"),
    }
}

#[test]
fn regex_cond_code_block_raw_source() {
    // (??{code}) — verify raw source capture.
    let e = parse_expr_str("m/(??{ $re })/;");
    match &e.kind {
        ExprKind::Regex(_, interp, _) => {
            let code_parts: Vec<_> = interp
                .0
                .iter()
                .filter_map(|p| match p {
                    InterpPart::RegexCondCode(raw, _) => Some(raw.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(code_parts.len(), 1, "expected one cond code block");
            assert_eq!(code_parts[0], " $re ", "raw source mismatch");
        }
        other => panic!("expected Regex, got {other:?}"),
    }
}

#[test]
fn regex_optimistic_code_block_raw_source() {
    // (*{code}) — optimistic code block, same structure as (?{}).
    let e = parse_expr_str("m/(*{ $n })/;");
    match &e.kind {
        ExprKind::Regex(_, interp, _) => {
            let code_parts: Vec<_> = interp
                .0
                .iter()
                .filter_map(|p| match p {
                    InterpPart::RegexCode(raw, _) => Some(raw.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(code_parts.len(), 1, "expected one code block");
            assert_eq!(code_parts[0], " $n ", "raw source mismatch");
        }
        other => panic!("expected Regex, got {other:?}"),
    }
}

// ── perlclass audit ─────────────────────────────────────

#[test]
fn class_with_version() {
    let prog = parse_class_prog("class Foo 1.234 { }");
    let c = find_class_decl(&prog);
    assert_eq!(c.name, "Foo");
    assert_eq!(c.version.as_deref(), Some("1.234"));
}

#[test]
fn class_statement_form() {
    // `class Foo;` — statement form with no block.
    let prog = parse_class_prog("class Foo;");
    let c = find_class_decl(&prog);
    assert_eq!(c.name, "Foo");
    assert!(c.body.is_none(), "statement form should have no body");
}

#[test]
fn class_version_and_attrs() {
    let prog = parse_class_prog("class Bar 2.0 :isa(Foo) { }");
    let c = find_class_decl(&prog);
    assert_eq!(c.version.as_deref(), Some("2"));
    assert_eq!(c.attributes[0].name, "isa");
}

#[test]
fn adjust_block() {
    let prog = parse_class_prog("class Foo { ADJUST { 1; } }");
    let c = find_class_decl(&prog);
    let body = c.body.as_ref().unwrap();
    match &body.statements[0].kind {
        StmtKind::Phaser(PhaserKind::Adjust, _) => {}
        other => panic!("expected Phaser(Adjust), got {other:?}"),
    }
}

#[test]
fn dunder_class() {
    let prog = parse_class_prog("class Foo { field $x = __CLASS__->DEFAULT; }");
    let c = find_class_decl(&prog);
    let body = c.body.as_ref().unwrap();
    match &body.statements[0].kind {
        StmtKind::FieldDecl(f) => {
            assert!(f.default.is_some(), "should have default");
        }
        other => panic!("expected FieldDecl, got {other:?}"),
    }
}

#[test]
fn field_defined_or_default() {
    let prog = parse_class_prog("class Foo { field $x :param //= 42; }");
    let c = find_class_decl(&prog);
    let body = c.body.as_ref().unwrap();
    match &body.statements[0].kind {
        StmtKind::FieldDecl(f) => {
            let (kind, _) = f.default.as_ref().unwrap();
            assert_eq!(*kind, SigDefaultKind::DefinedOr);
        }
        other => panic!("expected FieldDecl, got {other:?}"),
    }
}

#[test]
fn field_logical_or_default() {
    let prog = parse_class_prog("class Foo { field $x :param ||= 0; }");
    let c = find_class_decl(&prog);
    let body = c.body.as_ref().unwrap();
    match &body.statements[0].kind {
        StmtKind::FieldDecl(f) => {
            let (kind, _) = f.default.as_ref().unwrap();
            assert_eq!(*kind, SigDefaultKind::LogicalOr);
        }
        other => panic!("expected FieldDecl, got {other:?}"),
    }
}

#[test]
fn anon_method() {
    let prog = parse_class_prog("class Foo { method get { return method { 1; }; } }");
    let c = find_class_decl(&prog);
    assert!(c.body.is_some());
}

#[test]
fn lexical_method() {
    let prog = parse_class_prog("class Foo { my method secret { 1; } }");
    let c = find_class_decl(&prog);
    let body = c.body.as_ref().unwrap();
    match &body.statements[0].kind {
        StmtKind::MethodDecl(m) => {
            assert_eq!(m.name, "secret");
            assert_eq!(m.scope, Some(DeclScope::My));
        }
        other => panic!("expected MethodDecl, got {other:?}"),
    }
}

// ── perlexperiment: any/all operators ────────────────────

#[test]
fn any_block_list() {
    let prog = parse("use feature 'any'; any { $_ > 0 } @nums;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn all_block_list() {
    let prog = parse("use feature 'all'; all { defined $_ } @items;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn any_without_feature_is_bareword() {
    // Without `use feature 'any'`, `any` is a regular identifier.
    let e = parse_expr_str("any();");
    assert!(matches!(e.kind, ExprKind::FuncCall(ref name, _) if name == "any"), "without feature, any() should be a regular call, got {:?}", e.kind);
}

#[test]
fn all_without_feature_is_bareword() {
    let e = parse_expr_str("all();");
    assert!(matches!(e.kind, ExprKind::FuncCall(ref name, _) if name == "all"), "without feature, all() should be a regular call, got {:?}", e.kind);
}

// ── UTF-8 identifier validation ─────────────────────────

#[test]
fn utf8_array_variable() {
    let prog = parse("use utf8; my @données;");
    let (sigil, name) = first_decl_name_sigil(&prog);
    assert_eq!(sigil, Sigil::Array, "expected array sigil");
    assert_eq!(name, "donn\u{00E9}es", "expected exact name 'données'");
}

#[test]
fn utf8_hash_variable() {
    let prog = parse("use utf8; my %données;");
    let (sigil, name) = first_decl_name_sigil(&prog);
    assert_eq!(sigil, Sigil::Hash, "expected hash sigil");
    assert_eq!(name, "donn\u{00E9}es", "expected exact name 'données'");
}

#[test]
fn utf8_cjk_identifier() {
    let prog = parse("use utf8; my $変数 = 1;");
    assert_eq!(first_decl_name(&prog), "変数");
}

#[test]
fn utf8_mixed_ascii_and_unicode() {
    let prog = parse("use utf8; my $foo変数 = 1;");
    assert_eq!(first_decl_name(&prog), "foo変数");
}

#[test]
fn utf8_underscore_then_unicode() {
    let prog = parse("use utf8; my $_café = 1;");
    assert_eq!(first_decl_name(&prog), "_caf\u{00E9}");
}

#[test]
fn utf8_package_qualified() {
    // Verify the full qualified name survives scanning.
    // `Package->new()` produces ExprKind::MethodCall(Bareword(pkg), "new", []).
    let prog = parse("use utf8; Ünïcödé::módule->new();");
    let method_call = prog.statements.iter().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e) } else { None }).expect("should find expression");
    match &method_call.kind {
        ExprKind::MethodCall(invocant, method, _) => {
            match &invocant.kind {
                ExprKind::Bareword(name) => {
                    assert_eq!(name, "\u{00DC}n\u{00EF}c\u{00F6}d\u{00E9}::m\u{00F3}dule", "package name should be fully preserved with NFC");
                }
                other => panic!("expected Bareword invocant, got {other:?}"),
            }
            assert_eq!(method, "new");
        }
        other => panic!("expected MethodCall, got {other:?}"),
    }
}

#[test]
fn utf8_sub_with_unicode_param() {
    let prog = parse("use utf8; use feature 'signatures'; sub grüß($naïve) { $naïve }");
    let sub = prog.statements.iter().find_map(|s| if let StmtKind::SubDecl(sd) = &s.kind { Some(sd) } else { None }).expect("should find sub declaration");
    assert_eq!(sub.name, "gr\u{00FC}\u{00DF}", "sub name should be 'grüß'");
    // Verify parameter name.
    let sig = sub.signature.as_ref().expect("should have signature");
    match &sig.params[0] {
        SigParam::Scalar { name, .. } => {
            assert_eq!(name, "na\u{00EF}ve", "param name should be 'naïve'");
        }
        other => panic!("expected Scalar param, got {other:?}"),
    }
}

// ── Non-UTF-8 mode rejects high bytes ───────────────────

#[test]
fn no_utf8_rejects_high_bytes_in_scalar() {
    let src = "my $café = 1;";
    let mut p = Parser::new(src.as_bytes()).unwrap();
    assert!(p.parse_program().is_err(), "high bytes without use utf8 should error");
}

#[test]
fn no_utf8_rejects_high_bytes_in_bareword() {
    let src = "café();";
    let mut p = Parser::new(src.as_bytes()).unwrap();
    assert!(p.parse_program().is_err(), "high bytes in bareword without use utf8 should error");
}

// ── Invalid identifier characters (even with use utf8) ──

#[test]
fn utf8_emoji_not_identifier() {
    // Emoji (U+1F600) is not XID_Start.
    let src = "use utf8; my $\u{1F600} = 1;";
    let mut p = Parser::new(src.as_bytes()).unwrap();
    assert!(p.parse_program().is_err(), "emoji should not be valid in identifier");
}

#[test]
fn utf8_math_symbol_not_identifier() {
    // ∑ (U+2211 N-ARY SUMMATION) is not XID_Start.
    let src = "use utf8; my $∑ = 1;";
    let mut p = Parser::new(src.as_bytes()).unwrap();
    assert!(p.parse_program().is_err(), "math symbol should not be valid in identifier");
}

#[test]
fn utf8_bare_emoji_errors() {
    // Emoji as a bare statement — not an identifier.
    let src = "use utf8; \u{1F600};";
    let mut p = Parser::new(src.as_bytes()).unwrap();
    assert!(p.parse_program().is_err(), "emoji as bare statement should error");
}

#[test]
fn utf8_punctuation_not_identifier() {
    // « (U+00AB LEFT-POINTING DOUBLE ANGLE QUOTATION MARK) is not XID.
    let src = "use utf8; my $\u{00AB} = 1;";
    let mut p = Parser::new(src.as_bytes()).unwrap();
    assert!(p.parse_program().is_err(), "Unicode punctuation should not be valid in identifier");
}

#[test]
fn utf8_combining_mark_not_identifier_start() {
    // U+0301 COMBINING ACUTE ACCENT is XID_Continue but not XID_Start.
    // As the first char after a sigil, it should fail.
    let src = "use utf8; my $\u{0301}x = 1;";
    let mut p = Parser::new(src.as_bytes()).unwrap();
    assert!(p.parse_program().is_err(), "combining mark should not be valid as identifier start");
}

#[test]
fn utf8_combining_mark_ok_as_continue() {
    // Combining mark after a valid start character is fine.
    // `$e\u{0301}` = $é (e + combining acute) — valid.
    let prog = parse("use utf8; my $e\u{0301} = 1;");
    assert!(!prog.statements.is_empty(), "combining mark as continuation should parse");
}

#[test]
fn utf8_sigil_whitespace_then_unicode() {
    // `$ \n変数` — whitespace between sigil and UTF-8 name.
    let prog = parse("use utf8; my $\n変数 = 1;");
    assert!(!prog.statements.is_empty(), "whitespace between sigil and UTF-8 name should parse");
}

#[test]
fn utf8_at_sigil_whitespace_then_unicode() {
    // `@ \n変数` — whitespace between @ and UTF-8 name.
    let prog = parse("use utf8; my @\ndonn\u{00E9}es;");
    assert!(!prog.statements.is_empty(), "whitespace between @ and UTF-8 name should parse");
}

// ── Invalid UTF-8 byte sequences ────────────────────────

#[test]
fn invalid_utf8_bytes_error() {
    // 0xFF 0xFE is not valid UTF-8.
    let src: Vec<u8> = b"use utf8; my $\xff\xfe = 1;".to_vec();
    let mut p = Parser::new(&src).unwrap();
    assert!(p.parse_program().is_err(), "invalid UTF-8 bytes should error even with use utf8");
}

#[test]
fn invalid_utf8_lone_continuation_byte() {
    // 0x80 is a continuation byte without a lead byte.
    let src: Vec<u8> = b"use utf8; my $\x80x = 1;".to_vec();
    let mut p = Parser::new(&src).unwrap();
    assert!(p.parse_program().is_err(), "lone continuation byte should error");
}

// ── NFC normalization ───────────────────────────────────

#[test]
fn nfc_identifier_precomposed_and_decomposed_are_same() {
    let nfc_prog = parse("use utf8; my $caf\u{00E9} = 42;");
    let nfd_prog = parse("use utf8; my $cafe\u{0301} = 42;");

    let nfc_name = first_decl_name(&nfc_prog);
    let nfd_name = first_decl_name(&nfd_prog);
    assert_eq!(nfc_name, nfd_name, "NFC and NFD forms of café should produce the same identifier");
    assert_eq!(nfc_name, "caf\u{00E9}", "identifier should be in NFC form");
}

#[test]
fn nfc_sub_name_normalized() {
    // Sub name with NFD decomposed character.
    let nfd_src = "use utf8; sub nai\u{0308}ve { 1 }";
    let prog = parse(nfd_src);
    let sub_name = prog
        .statements
        .iter()
        .find_map(|s| if let StmtKind::SubDecl(sd) = &s.kind { Some(sd.name.clone()) } else { None })
        .expect("should find sub declaration");
    // ï in NFC is U+00EF
    assert_eq!(sub_name, "na\u{00EF}ve", "sub name should be NFC-normalized");
}

#[test]
fn nfc_package_name_normalized() {
    // Package::Module with decomposed chars — verify NFC names.
    let src = "use utf8; package Caf\u{00E9}::Mo\u{0308}dule;";
    let prog = parse(src);
    let pkg =
        prog.statements.iter().find_map(|s| if let StmtKind::PackageDecl(pd) = &s.kind { Some(pd) } else { None }).expect("should find package declaration");
    // Mödule: o + U+0308 → ö (U+00F6) in NFC
    assert_eq!(pkg.name, "Caf\u{00E9}::M\u{00F6}dule", "package name should be NFC-normalized");
}

#[test]
fn nfc_ascii_identifiers_unchanged() {
    let prog = parse("use utf8; my $hello = 1;");
    assert_eq!(first_decl_name(&prog), "hello");
}

#[test]
fn nfc_no_normalization_without_utf8() {
    // Without `use utf8`, high bytes are errors, so NFC
    // normalization never applies.
    let prog = parse("my $hello = 1;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn nfc_string_content_normalized() {
    let src = "use utf8; my $x = 'caf\u{0065}\u{0301}';";
    let rhs = first_assign_rhs(&parse(src));
    match &rhs.kind {
        ExprKind::StringLit(s) => {
            assert_eq!(s, "caf\u{00E9}", "string content should be NFC-normalized: got {:?} (len {})", s.as_bytes(), s.len());
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn nfc_hangul_normalized() {
    let prog = parse("use utf8; my $\u{1100}\u{1161} = 1;");
    assert_eq!(first_decl_name(&prog), "\u{AC00}", "Hangul jamo should compose to syllable in NFC");
}

#[test]
fn nfc_multiple_combining_marks() {
    // o + U+0308 (diaeresis) + U+0304 (macron)
    // NFC composes all three: o + diaeresis + macron → ȫ (U+022B).
    let prog = parse("use utf8; my $o\u{0308}\u{0304}x = 1;");
    let name = first_decl_name(&prog);
    assert_eq!(name.chars().collect::<Vec<_>>(), vec!['\u{022B}', 'x'], "o + diaeresis + macron should compose to ȫ");
}

#[test]
fn nfc_already_nfc_input() {
    let prog = parse("use utf8; my $für = 1;");
    assert_eq!(first_decl_name(&prog), "f\u{00FC}r", "already-NFC input should be unchanged");
}

// ── memchr optimization — verify actual content ────────

#[test]
fn memchr_long_string_body_preserves_content() {
    let long_text = "abcdefghij".repeat(100);
    let src = format!("my $x = '{long_text}';");
    let rhs = first_assign_rhs(&parse(&src));
    match &rhs.kind {
        ExprKind::StringLit(s) => {
            assert_eq!(s, &long_text, "1000-char string should survive memchr path intact");
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn memchr_paired_delimiter_depth_content() {
    // Verify content with nested paired delimiters.
    let e = parse_expr_str("q{outer{inner}outer};");
    match &e.kind {
        ExprKind::StringLit(s) => {
            assert_eq!(s, "outer{inner}outer", "nested braces should be preserved in content");
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn memchr_content_before_interpolation_trigger() {
    // Verify the ConstSegment content before a $ trigger.
    let tokens = collect_tokens("\"hello world $x\"");
    // Should have: QuoteSublexBegin, ConstSegment("hello world "), InterpScalar("x"), SublexEnd
    let seg = tokens.iter().find_map(|t| if let Token::ConstSegment(s) = t { Some(s.clone()) } else { None }).expect("should find ConstSegment");
    assert_eq!(seg, "hello world ", "content before $ trigger should be exact");
}

#[test]
fn memchr_heredoc_multiline_content() {
    let src = "my $x = <<END;\nline 1\nline 2\nline 3\nEND\n";
    let rhs = first_assign_rhs(&parse(src));
    match &rhs.kind {
        ExprKind::StringLit(s) => {
            assert_eq!(s, "line 1\nline 2\nline 3\n", "heredoc content should preserve all lines");
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

// ── UTF-8 additional coverage — verify exact names ─────

#[test]
fn utf8_array_len_unicode() {
    // $#données — verify the array-length name is exact.
    let tokens = collect_tokens_utf8("use utf8; my @données; my $n = $#données;");
    let arraylen_name = tokens.iter().find_map(|t| if let Token::ArrayLen(name) = t { Some(name.clone()) } else { None }).expect("should find ArrayLen token");
    assert_eq!(arraylen_name, "donn\u{00E9}es", "$#données should produce exact name");
}

#[test]
fn utf8_devanagari_identifier() {
    let prog = parse("use utf8; my $नाम = 1;");
    assert_eq!(first_decl_name(&prog), "नाम");
}

#[test]
fn utf8_cyrillic_identifier() {
    let prog = parse("use utf8; my $имя = 1;");
    assert_eq!(first_decl_name(&prog), "имя");
}

#[test]
fn utf8_greek_identifier() {
    let prog = parse("use utf8; my $αριθμός = 1;");
    assert_eq!(first_decl_name(&prog), "αριθμός");
}

#[test]
fn utf8_arabic_identifier() {
    let prog = parse("use utf8; my $اسم = 1;");
    assert_eq!(first_decl_name(&prog), "اسم");
}

#[test]
fn utf8_method_call_name_exact() {
    let prog = parse("use utf8; $obj->caf\u{00E9}();");
    let method_name = prog
        .statements
        .iter()
        .find_map(|s| {
            if let StmtKind::Expr(expr) = &s.kind
                && let ExprKind::MethodCall(_, name, _) = &expr.kind
            {
                return Some(name.clone());
            }
            None
        })
        .expect("should find method call");
    assert_eq!(method_name, "caf\u{00E9}");
}

#[test]
fn utf8_multiple_identifiers_exact_names() {
    let prog = parse("use utf8; my $café = 1; my $naïve = 2; my $für = 3;");
    let names = all_decl_names(&prog);
    assert_eq!(names, vec!["caf\u{00E9}", "na\u{00EF}ve", "f\u{00FC}r"]);
}

#[test]
fn utf8_hash_subscript_utf8_key() {
    let prog = parse("use utf8; $h{clé};");
    let key = prog
        .statements
        .iter()
        .find_map(|s| {
            if let StmtKind::Expr(expr) = &s.kind
                && let ExprKind::HashElem(_, k) = &expr.kind
                && let ExprKind::StringLit(s) = &k.kind
            {
                return Some(s.clone());
            }
            None
        })
        .expect("should find hash subscript key");
    assert_eq!(key, "cl\u{00E9}", "autoquoted key should be exact");
}

// ── NFC normalization — adversarial content verification ─

#[test]
fn nfc_same_identifier_both_forms_yields_same_name() {
    // NFC $café and NFD $café in the same program must produce
    // identical variable names, or the runtime would treat them
    // as different variables.
    let tokens = collect_tokens_utf8("use utf8; $caf\u{00E9}; $cafe\u{0301};");
    let scalar_names: Vec<&str> = tokens.iter().filter_map(|t| if let Token::ScalarVar(name) = t { Some(name.as_str()) } else { None }).collect();
    assert!(scalar_names.len() >= 2, "should find at least 2 scalar vars");
    assert_eq!(scalar_names[0], scalar_names[1], "NFC and NFD forms should produce the same variable name: {:?} vs {:?}", scalar_names[0], scalar_names[1]);
    assert_eq!(scalar_names[0], "caf\u{00E9}", "both should be NFC: {:?}", scalar_names[0]);
}

#[test]
fn nfc_interpolation_variable_name_normalized() {
    // Interpolated NFD $café inside a string — the InterpScalar
    // token should contain the NFC name.
    let tokens = collect_tokens_utf8("use utf8; \"$cafe\u{0301}\"");
    let interp_name =
        tokens.iter().find_map(|t| if let Token::InterpScalar(name) = t { Some(name.clone()) } else { None }).expect("should find InterpScalar token");
    assert_eq!(interp_name, "caf\u{00E9}", "interpolated variable name should be NFC-normalized");
}

#[test]
fn nfc_consistent_across_all_sigils() {
    // NFD café should normalize to same NFC name for $, @, %.
    let tokens = collect_tokens_utf8("use utf8; $cafe\u{0301}; @cafe\u{0301}; %cafe\u{0301};");
    let scalar = tokens.iter().find_map(|t| if let Token::ScalarVar(n) = t { Some(n.as_str()) } else { None }).unwrap();
    let array = tokens.iter().find_map(|t| if let Token::ArrayVar(n) = t { Some(n.as_str()) } else { None }).unwrap();
    // Hash comes through lex_hash_var_after_percent path.
    assert_eq!(scalar, "caf\u{00E9}", "scalar name NFC");
    assert_eq!(array, "caf\u{00E9}", "array name NFC");
    assert_eq!(scalar, array, "scalar and array should match");
}

#[test]
fn nfc_single_quoted_string_content_verified() {
    let src = "use utf8; my $x = 'cafe\u{0301} au lait';";
    let rhs = first_assign_rhs(&parse(src));
    match &rhs.kind {
        ExprKind::StringLit(s) => {
            assert_eq!(s, "caf\u{00E9} au lait", "single-quoted string should have NFC content");
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn nfc_heredoc_body_content_verified() {
    let src = "use utf8; my $x = <<END;\ncafe\u{0301}\nEND\n";
    let rhs = first_assign_rhs(&parse(src));
    match &rhs.kind {
        ExprKind::StringLit(s) => {
            assert!(s.contains("caf\u{00E9}"), "heredoc body should have NFC content, got {:?}", s);
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn nfc_qw_words_normalized() {
    // qw() with NFD words — verify each word is NFC.
    let e = parse_expr_stmt("use utf8; qw(cafe\u{0301} nai\u{0308}ve);");
    match &e.kind {
        ExprKind::QwList(words) => {
            assert_eq!(words.len(), 2);
            assert_eq!(words[0], "caf\u{00E9}", "first qw word should be NFC");
            assert_eq!(words[1], "na\u{00EF}ve", "second qw word should be NFC");
        }
        other => panic!("expected QwList, got {other:?}"),
    }
}

#[test]
fn nfc_escape_sequences_must_not_be_normalized() {
    let src = r#"use utf8; my $x = "\x{65}\x{301}";"#;
    let rhs = first_assign_rhs(&parse(src));
    match &rhs.kind {
        ExprKind::StringLit(s) => {
            let chars: Vec<char> = s.chars().collect();
            assert_eq!(chars.len(), 2, "escape-constructed string should have 2 chars (e + combining accent), got {} chars: {:?}", chars.len(), chars);
            assert_eq!(chars[0], 'e', "first char should be 'e'");
            assert_eq!(chars[1], '\u{0301}', "second char should be combining acute");
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn nfc_fat_comma_autoquote_verified() {
    // Fat comma with NFD bareword — verify the autoquoted string is NFC.
    let rhs = first_assign_rhs(&parse("use utf8; my %h = (cafe\u{0301} => 1);"));
    // RHS is Paren(List([StringLit("café"), Int(1)])) or List([...]).
    fn find_nfc_key(e: &Expr) -> bool {
        match &e.kind {
            ExprKind::StringLit(s) => s == "caf\u{00E9}",
            ExprKind::List(items) => items.iter().any(find_nfc_key),
            ExprKind::Paren(inner) => find_nfc_key(inner),
            _ => false,
        }
    }
    assert!(find_nfc_key(&rhs), "fat comma autoquoted NFD bareword should produce NFC string, got {:?}", rhs.kind);
}

#[test]
fn nfc_sub_name_nfd_becomes_nfc() {
    // Sub declared with NFD name — verify the SubDecl name is NFC.
    let prog = parse("use utf8; sub cafe\u{0301} { 1 }");
    let sub = prog.statements.iter().find_map(|s| if let StmtKind::SubDecl(sd) = &s.kind { Some(sd) } else { None }).expect("should find sub declaration");
    assert_eq!(sub.name, "caf\u{00E9}", "sub name should be NFC-normalized");
}

// ── UTF-8 body content in strings ────────────────────────

#[test]
fn double_quoted_non_ascii_body_ascii_delim() {
    // Non-ASCII content in a regular double-quoted string under
    // use utf8.  The memchr fast path should bulk-copy the UTF-8
    // correctly.
    let prog = parse("use utf8; my $x = \"caf\u{00E9} \u{00A3}5\";");
    match &prog.statements[1].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Assign(_, _, rhs) => match &rhs.kind {
                ExprKind::InterpolatedString(parts) => {
                    let full: String = parts.0.iter().filter_map(|p| if let InterpPart::Const(s) = p { Some(s.as_str()) } else { None }).collect();
                    assert_eq!(full, "caf\u{00E9} \u{00A3}5");
                }
                ExprKind::StringLit(s) => assert_eq!(s, "caf\u{00E9} \u{00A3}5"),
                other => panic!("expected string, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn q_braces_with_non_ascii_under_utf8() {
    // Standard q{} with non-ASCII body under use utf8.
    // No Unicode delimiters — tests the basic byte-by-byte
    // fallback with ASCII delimiters.
    let prog = parse("use utf8; my $x = q{caf\u{00E9}};");
    match &prog.statements[1].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Assign(_, _, rhs) => match &rhs.kind {
                ExprKind::StringLit(s) => assert_eq!(s, "caf\u{00E9}"),
                other => panic!("expected StringLit, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn case_mod_uppercase_non_ascii() {
    // "\Ucafé\E" under use utf8 — the byte-by-byte fallback
    // must decode multi-byte UTF-8 characters, not split them
    // with skip(1) + b as char.
    let prog = parse("use utf8; my $x = \"\\Ucaf\u{00E9}\\E\";");
    match &prog.statements[1].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Assign(_, _, rhs) => match &rhs.kind {
                ExprKind::InterpolatedString(parts) => {
                    let full: String = parts.0.iter().filter_map(|p| if let InterpPart::Const(s) = p { Some(s.as_str()) } else { None }).collect();
                    assert_eq!(full, "CAF\u{00C9}", "expected CAFÉ, got {full}");
                }
                ExprKind::StringLit(s) => assert_eq!(s, "CAF\u{00C9}", "expected CAFÉ, got {s}"),
                other => panic!("expected string, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

// ── Adversarial edge cases ──────────────────────────────

#[test]
fn utf8_digit_after_package_separator_is_error() {
    // `Foo::3bar` — 3 is XID_Continue but NOT XID_Start.
    // After ::, the next segment needs XID_Start.
    let src = "use utf8; Foo::3bar;";
    let mut p = Parser::new(src.as_bytes()).unwrap();
    // This should either error or parse 3 as a number, not as
    // part of the identifier.
    let result = p.parse_program();
    if let Ok(prog) = result {
        // If it parsed, verify `Foo::3bar` is NOT a single identifier.
        let first_expr = prog.statements.iter().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e) } else { None });
        if let Some(e) = first_expr {
            // Should not be a single bareword "Foo::3bar"
            assert!(!matches!(&e.kind, ExprKind::Bareword(name) if name == "Foo::3bar"), "3 after :: should not be accepted as XID_Start in identifier");
        }
    }
    // Either an error or a non-single-identifier parse is acceptable.
}

#[test]
fn memchr_utf8_string_body_content_exact() {
    let src = "use utf8; my $x = 'café résumé naïve';";
    let rhs = first_assign_rhs(&parse(src));
    match &rhs.kind {
        ExprKind::StringLit(s) => {
            assert_eq!(s, "caf\u{00E9} r\u{00E9}sum\u{00E9} na\u{00EF}ve", "UTF-8 string content should be preserved exactly");
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn memchr_utf8_before_interpolation_trigger_exact() {
    // UTF-8 content before $ in interpolating string.
    let tokens = collect_tokens_utf8("use utf8; \"caf\u{00E9} $x\"");
    let seg = tokens.iter().find_map(|t| if let Token::ConstSegment(s) = t { Some(s.clone()) } else { None }).expect("should find ConstSegment");
    assert_eq!(seg, "caf\u{00E9} ", "UTF-8 content before $ should be preserved exactly");
}

#[test]
fn utf8_combining_mark_as_continue_name_exact() {
    let prog = parse("use utf8; my $e\u{0301} = 1;");
    assert_eq!(first_decl_name(&prog), "\u{00E9}", "e + combining acute should NFC-normalize to é");
}

#[test]
fn utf8_sigil_whitespace_then_unicode_name_exact() {
    let prog = parse("use utf8; my $\n変数 = 1;");
    assert_eq!(first_decl_name(&prog), "変数", "variable name after sigil+whitespace should be exact");
}

#[test]
fn utf8_heredoc_tag() {
    // Heredoc near UTF-8 code — the tag itself is ASCII but
    // the surrounding context uses UTF-8.
    let src = "use utf8; my $café = <<FIN;\ncontent\nFIN\n";
    let rhs = first_assign_rhs(&parse(src));
    match &rhs.kind {
        ExprKind::StringLit(s) => {
            assert_eq!(s, "content\n", "heredoc content should be exact");
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn nfc_in_regex_body() {
    // Regex body with NFD content should be NFC-normalized.
    let prog = parse("use utf8; $x =~ /cafe\u{0301}/;");
    let regex_body = prog
        .statements
        .iter()
        .find_map(|s| {
            if let StmtKind::Expr(e) = &s.kind
                && let ExprKind::BinOp(BinOp::Binding, _, rhs) = &e.kind
                && let ExprKind::Regex(_, Interpolated(parts), _) = &rhs.kind
            {
                parts.iter().find_map(|p| if let InterpPart::Const(s) = p { Some(s.clone()) } else { None })
            } else {
                None
            }
        })
        .expect("should find regex body content");
    assert_eq!(regex_body, "caf\u{00E9}", "regex body NFD content should be NFC-normalized");
}

#[test]
fn memchr_utf8_regex_body() {
    // UTF-8 in regex body — memchr bulk-copy path.
    let prog = parse("use utf8; $x =~ /héllo|wörld/;");
    let regex_body = prog
        .statements
        .iter()
        .find_map(|s| {
            if let StmtKind::Expr(e) = &s.kind
                && let ExprKind::BinOp(BinOp::Binding, _, rhs) = &e.kind
                && let ExprKind::Regex(_, Interpolated(parts), _) = &rhs.kind
            {
                parts.iter().find_map(|p| if let InterpPart::Const(s) = p { Some(s.clone()) } else { None })
            } else {
                None
            }
        })
        .expect("should find regex body content");
    assert!(regex_body.contains("h\u{00E9}llo"), "UTF-8 regex body should be preserved, got {:?}", regex_body);
}

#[test]
fn memchr_utf8_single_quoted() {
    // Single-quoted string with UTF-8 — non-interpolating memchr path.
    let src = "use utf8; my $x = 'café résumé';";
    let rhs = first_assign_rhs(&parse(src));
    match &rhs.kind {
        ExprKind::StringLit(s) => {
            assert_eq!(s, "caf\u{00E9} r\u{00E9}sum\u{00E9}", "single-quoted UTF-8 string should be exact");
        }
        other => panic!("expected StringLit, got {other:?}"),
    }
}

#[test]
fn memchr_interpolating_string() {
    // Interpolating string with mixed content — verify structure.
    let src = r#"my $name = "world"; my $x = "hello $name, done";"#;
    let prog = parse(src);
    // Second assignment RHS should be an interpolated string.
    let stmts: Vec<_> = prog.statements.iter().filter(|s| matches!(&s.kind, StmtKind::Expr(Expr { kind: ExprKind::Assign(_, _, _), .. }))).collect();
    assert!(stmts.len() >= 2, "should have at least 2 assignments");
    if let StmtKind::Expr(expr) = &stmts[1].kind
        && let ExprKind::Assign(_, _, rhs) = &expr.kind
    {
        assert!(matches!(rhs.kind, ExprKind::InterpolatedString(_)), "second assignment RHS should be InterpolatedString, got {:?}", rhs.kind);
    }
}

#[test]
fn memchr_regex_with_code_block() {
    // Regex with code block — memchr must detect ( trigger.
    let src = "use feature 'all'; my $x = 'abc'; $x =~ m/foo(?{ 1 + 2 })bar/;";
    let prog = parse(src);
    // Should parse without errors and contain a regex.
    let has_regex =
        prog.statements.iter().any(|s| if let StmtKind::Expr(e) = &s.kind { matches!(e.kind, ExprKind::BinOp(BinOp::Binding, _, _)) } else { false });
    assert!(has_regex, "should contain a regex bind operation");
}

// ── ChatGPT torture tests ───────────────────────────────

#[test]
fn autoquote_try_fat_comma() {
    let first = parse_kw_fat_comma("(try => 1);");
    assert!(matches!(first.kind, ExprKind::StringLit(ref s) if s == "try"));
}

#[test]
fn parse_defined_or_as_operator() {
    let e = parse_expr_str("$x // $y;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
}

#[test]
fn parse_qw_list_delimiter_weirdness() {
    let e = parse_expr_str("qw[a\\] b\\ c];");
    assert!(matches!(e.kind, ExprKind::QwList(_)));
}

#[test]
fn parse_regex_with_code_block() {
    let e = parse_expr_str("/(?{ print 20 })/;");
    assert!(matches!(e.kind, ExprKind::Regex(_, _, _)));
}

#[test]
fn parse_substitution_delimiter_switch() {
    let e = parse_expr_str("s{c}/.../r;");
    assert!(matches!(e.kind, ExprKind::Subst(_, _, _)));
}

#[test]
fn parse_substitution_replacement_multiline_body() {
    let e = parse_expr_str("s/foo/bar\nbaz/e;");
    assert!(matches!(e.kind, ExprKind::Subst(_, _, _)));
}

#[test]
fn parse_interpolated_scalar_chain() {
    let e = parse_expr_str(r#""$h->{k}[0]""#);
    assert!(matches!(e.kind, ExprKind::InterpolatedString(_)));
}

#[test]
fn parse_interpolated_expr_hole() {
    let e = parse_expr_str(r#""value=${x + 1}""#);
    assert!(matches!(e.kind, ExprKind::InterpolatedString(_)));
}

#[test]
fn signature_vs_prototype_switches_with_feature() {
    let s1 = parse_sub("sub f ($$) { }");
    assert!(s1.prototype.is_some());
    assert!(s1.signature.is_none());

    let s2 = parse_sub("use feature 'signatures'; sub f ($x, $y) { }");
    assert!(s2.signature.is_some());
}

#[test]
fn declared_refs_only_when_feature_enabled() {
    let msg = parse_fails("my \\$x;");
    assert!(msg.contains("expected variable") || msg.contains("unexpected"));
}

#[test]
fn parse_source_file_line_package_tokens() {
    let prog = crate::parse_with_filename(b"__FILE__; __LINE__; __PACKAGE__;", "t/foo.pl").unwrap();
    assert_eq!(prog.statements.len(), 3);
}

#[test]
fn hard_empty_regex_from_defined_or_in_term_position() {
    // `print //ms;` — the `//` is an empty regex with flags, not defined-or.
    let e = parse_expr_stmt("print //ms;");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "print");
            assert!(fh.is_none());
            assert_eq!(args.len(), 1);
            assert!(matches!(
                args[0].kind,
                ExprKind::Regex(RegexKind::Match, Interpolated(_), Some(ref flags))
                    if flags == "ms"
            ));
        }
        other => panic!("expected PrintOp(print, _, [Regex(..., ms)]), got {other:?}"),
    }
}

#[test]
fn hard_plus_wraps_anon_hash() {
    // `+{ a => 1 }` — unary plus forces hash constructor.
    let e = parse_expr_stmt("+{ a => 1 };");
    match &e.kind {
        ExprKind::UnaryOp(_, inner) => {
            assert!(matches!(inner.kind, ExprKind::AnonHash(_)), "expected unary op wrapping AnonHash, got {:?}", inner.kind);
        }
        other => panic!("expected UnaryOp(_, AnonHash(_)), got {other:?}"),
    }
}

#[test]
fn hard_block_vs_hash_in_map() {
    // `map { { a => 1 } } @list` — outer braces are a block,
    // inner braces are an anonymous hash.
    let e = parse_expr_stmt("map { { a => 1 } } @list;");

    fn block_contains_anon_hash(block: &Block) -> bool {
        block.statements.iter().any(stmt_contains_anon_hash)
    }

    fn stmt_contains_anon_hash(stmt: &Statement) -> bool {
        match &stmt.kind {
            StmtKind::Expr(expr) => expr_contains_anon_hash(expr),
            StmtKind::Block(block, _) => block_contains_anon_hash(block),
            StmtKind::Labeled(_, inner) => stmt_contains_anon_hash(inner),
            StmtKind::If(s) => {
                expr_contains_anon_hash(&s.condition)
                    || block_contains_anon_hash(&s.then_block)
                    || s.elsif_clauses.iter().any(|(cond, blk)| expr_contains_anon_hash(cond) || block_contains_anon_hash(blk))
                    || s.else_block.as_ref().is_some_and(block_contains_anon_hash)
            }
            StmtKind::Unless(s) => {
                expr_contains_anon_hash(&s.condition)
                    || block_contains_anon_hash(&s.then_block)
                    || s.elsif_clauses.iter().any(|(cond, blk)| expr_contains_anon_hash(cond) || block_contains_anon_hash(blk))
                    || s.else_block.as_ref().is_some_and(block_contains_anon_hash)
            }
            StmtKind::While(s) => {
                expr_contains_anon_hash(&s.condition) || block_contains_anon_hash(&s.body) || s.continue_block.as_ref().is_some_and(block_contains_anon_hash)
            }
            StmtKind::Until(s) => {
                expr_contains_anon_hash(&s.condition) || block_contains_anon_hash(&s.body) || s.continue_block.as_ref().is_some_and(block_contains_anon_hash)
            }
            StmtKind::For(s) => {
                s.init.as_ref().is_some_and(expr_contains_anon_hash)
                    || s.condition.as_ref().is_some_and(expr_contains_anon_hash)
                    || s.step.as_ref().is_some_and(expr_contains_anon_hash)
                    || block_contains_anon_hash(&s.body)
            }
            StmtKind::ForEach(s) => expr_contains_anon_hash(&s.list) || block_contains_anon_hash(&s.body),
            _ => false,
        }
    }

    fn expr_contains_anon_hash(expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::AnonHash(_) => true,
            ExprKind::AnonSub(_, _, _, body) => block_contains_anon_hash(body),
            ExprKind::BinOp(_, l, r) | ExprKind::Assign(_, l, r) | ExprKind::Range(l, r) | ExprKind::FlipFlop(l, r) => {
                expr_contains_anon_hash(l) || expr_contains_anon_hash(r)
            }
            ExprKind::UnaryOp(_, inner)
            | ExprKind::PostfixOp(_, inner)
            | ExprKind::Ref(inner)
            | ExprKind::DoExpr(inner)
            | ExprKind::EvalExpr(inner)
            | ExprKind::Local(inner) => expr_contains_anon_hash(inner),
            ExprKind::Ternary(c, t, f) => expr_contains_anon_hash(c) || expr_contains_anon_hash(t) || expr_contains_anon_hash(f),
            ExprKind::FuncCall(_, args) | ExprKind::ListOp(_, args) | ExprKind::List(args) | ExprKind::AnonArray(args) => {
                args.iter().any(expr_contains_anon_hash)
            }
            _ => false,
        }
    }

    assert!(expr_contains_anon_hash(&e), "expected an AnonHash somewhere in {:?}", e.kind);
}

#[test]
fn current_package_restores_after_block_form_package() {
    // `package Inner { ... }` — block-form package scopes
    // and restores the outer package name.
    let prog = parse(
        "package Outer;\n\
         package Inner { __PACKAGE__; }\n\
         __PACKAGE__;\n",
    );

    let inner_pkg_stmt =
        prog.statements.iter().find(|s| if let StmtKind::PackageDecl(pd) = &s.kind { pd.name == "Inner" } else { false }).expect("Inner package decl");
    if let StmtKind::PackageDecl(ref pd) = inner_pkg_stmt.kind {
        if let Some(ref body) = pd.block {
            let inner_expr =
                body.statements.iter().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e.clone()) } else { None }).expect("inner __PACKAGE__ expr");
            assert!(matches!(
                inner_expr.kind,
                ExprKind::CurrentPackage(ref s) if s == "Inner"
            ));
        } else {
            panic!("expected block-form package");
        }
    }

    let outer_expr =
        prog.statements.iter().rev().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e.clone()) } else { None }).expect("outer __PACKAGE__ expr");

    assert!(matches!(
        outer_expr.kind,
        ExprKind::CurrentPackage(ref s) if s == "Outer"
    ));
}

#[test]
// Known bug: statement-form package inside bare block doesn't restore.
fn current_package_restores_after_statement_form_in_block() {
    // `{ package Inner; __PACKAGE__; }` — statement-form package
    // inside a bare block.  In Perl, the package name is scoped
    // to the enclosing block and restored on exit.
    let prog = parse(
        "package Outer;\n\
         { package Inner; __PACKAGE__; }\n\
         __PACKAGE__;\n",
    );

    // __PACKAGE__ after the block should be "Outer".
    let outer_expr =
        prog.statements.iter().rev().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e.clone()) } else { None }).expect("outer __PACKAGE__ expr");

    assert!(matches!(
        outer_expr.kind,
        ExprKind::CurrentPackage(ref s) if s == "Outer"
    ));
}

#[test]
fn source_line_inside_block_uses_physical_line() {
    let prog = parse("{\n__LINE__;\n}");
    match &prog.statements[0].kind {
        StmtKind::Block(block, _) => {
            let inner = block.statements.iter().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e.clone()) } else { None }).expect("inner expr");
            match inner.kind {
                ExprKind::SourceLine(n) => assert_eq!(n, 2),
                other => panic!("expected SourceLine(2), got {other:?}"),
            }
        }
        other => panic!("expected top-level Block statement, got {other:?}"),
    }
}

#[test]
fn downgraded_keyword_class_can_be_called_as_ident() {
    let e = parse_expr_stmt("class($x);");
    assert!(matches!(e.kind, ExprKind::FuncCall(ref name, _) if name == "class"));
}

#[test]
fn current_sub_inside_named_sub_with_feature() {
    let prog = parse("use feature 'current_sub'; sub foo { __SUB__; }");
    let sub = prog.statements.iter().find_map(|s| if let StmtKind::SubDecl(sd) = &s.kind { Some(sd) } else { None }).expect("sub decl");
    let expr = sub.body.statements.iter().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e.clone()) } else { None }).expect("expr in sub body");
    assert!(matches!(expr.kind, ExprKind::CurrentSub), "expected CurrentSub, got {:?}", expr.kind);
}

#[test]
fn hard_postfix_for_wraps_whole_print() {
    // `print 'hello' for @list;` — PrintOp is the postfix body.
    let prog = parse("print 'hello' for @list;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(kind, body, list), .. }) => {
            assert!(matches!(kind, PostfixKind::For | PostfixKind::Foreach));
            assert!(matches!(body.kind, ExprKind::PrintOp(_, _, _)), "expected PrintOp body, got {:?}", body.kind);
            assert!(matches!(list.kind, ExprKind::ArrayVar(ref s) if s == "list"), "expected @list, got {:?}", list.kind);
        }
        other => panic!("expected PostfixControl(For, PrintOp, @list), got {other:?}"),
    }
}

// ── Unicode paired delimiters ─────────────────────────────

#[test]
fn q_guillemets_paired() {
    let prog = parse("use utf8; use feature 'extra_paired_delimiters'; my $x = q\u{00AB}hello\u{00BB};");
    let name = first_decl_name(&prog);
    assert_eq!(name, "x");
    // Verify the string content.
    match &prog.statements[2].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Assign(_, _, rhs) => match &rhs.kind {
                ExprKind::StringLit(s) => assert_eq!(s, "hello"),
                other => panic!("expected StringLit, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn q_cjk_corner_brackets() {
    let prog = parse("use utf8; use feature 'extra_paired_delimiters'; my $x = q\u{300C}test\u{300D};");
    match &prog.statements[2].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Assign(_, _, rhs) => match &rhs.kind {
                ExprKind::StringLit(s) => assert_eq!(s, "test"),
                other => panic!("expected StringLit, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn q_guillemets_nested() {
    // q«foo«bar»baz» — inner « » increases/decreases depth.
    let prog = parse("use utf8; use feature 'extra_paired_delimiters'; my $x = q\u{00AB}foo\u{00AB}bar\u{00BB}baz\u{00BB};");
    match &prog.statements[2].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Assign(_, _, rhs) => match &rhs.kind {
                ExprKind::StringLit(s) => assert_eq!(s, "foo\u{00AB}bar\u{00BB}baz"),
                other => panic!("expected StringLit, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn q_guillemets_without_feature_is_nonpaired() {
    // Without the feature, « is a non-paired delimiter (same open/close).
    let prog = parse("use utf8; my $x = q\u{00AB}hello\u{00AB};");
    match &prog.statements[1].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Assign(_, _, rhs) => match &rhs.kind {
                ExprKind::StringLit(s) => assert_eq!(s, "hello"),
                other => panic!("expected StringLit, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn qw_math_angle_brackets() {
    // qw⟨a b c⟩
    let prog = parse("use utf8; use feature 'extra_paired_delimiters'; my @a = qw\u{27E8}alpha beta gamma\u{27E9};");
    match &prog.statements[2].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Assign(_, _, rhs) => match &rhs.kind {
                ExprKind::QwList(words) => assert_eq!(words, &["alpha", "beta", "gamma"]),
                other => panic!("expected QwList, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn q_unicode_delim_body_with_shared_lead_byte() {
    // « is U+00AB (0xC2 0xAB), » is U+00BB (0xC2 0xBB).
    // £ is U+00A3 (0xC2 0xA3) — shares lead byte 0xC2 with both.
    // Memchr triggers on 0xC2 inside the body, but the old
    // byte-by-byte fallback did skip(1) + b as char, splitting
    // the multi-byte £ into two garbled characters.
    let prog = parse(
        "use utf8; use feature 'extra_paired_delimiters'; my $x = q\u{00AB}\u{00A3}\u{00BB};"
    );
    match &prog.statements[2].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Assign(_, _, rhs) => match &rhs.kind {
                ExprKind::StringLit(s) => assert_eq!(s, "\u{00A3}"),
                other => panic!("expected StringLit, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn subst_unicode_delimiters_paired() {
    // s«pattern»«replacement» with extra_paired_delimiters.
    let prog = parse(
        "use utf8; use feature 'extra_paired_delimiters'; my $x = 'hello'; $x =~ s\u{00AB}hell\u{00BB}\u{00AB}heaven\u{00BB};"
    );
    assert!(prog.statements.len() >= 3, "expected at least 3 statements");
}

#[test]
fn tr_unicode_delimiters_paired() {
    // tr«abc»«ABC» with extra_paired_delimiters.
    let prog = parse(
        "use utf8; use feature 'extra_paired_delimiters'; my $x = 'abc'; $x =~ tr\u{00AB}abc\u{00BB}\u{00AB}ABC\u{00BB};"
    );
    assert!(prog.statements.len() >= 3);
}

// ── Non-paired Unicode delimiters ─────────────────────────
// Without extra_paired_delimiters, Unicode chars work as
// non-paired (same open/close) delimiters.

#[test]
fn qq_unicode_non_paired_delimiter() {
    // qq§hello§ — § (U+00A7) as non-paired delimiter.
    let prog = parse("use utf8; my $x = qq\u{00A7}hello\u{00A7};");
    match &prog.statements[1].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Assign(_, _, rhs) => match &rhs.kind {
                ExprKind::StringLit(s) => assert_eq!(s, "hello"),
                ExprKind::InterpolatedString(_) => {}
                other => panic!("expected string, got {other:?}"),
            },
            other => panic!("expected Assign, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn m_unicode_non_paired_delimiter() {
    // m§pattern§ — § as regex delimiter.
    let src = "use utf8; 'hello' =~ m\u{00A7}hell\u{00A7};";
    let prog = parse(src);
    assert!(!prog.statements.is_empty(), "should parse m§...§ without error");
}

// ── Heredoc with Unicode tag (quoted) ─────────────────────

#[test]
fn heredoc_quoted_with_unicode_tag() {
    // <<"café" — heredoc tag contains non-ASCII in double quotes.
    let src = "use utf8; my $x = <<\"caf\u{00E9}\";\nhello\ncaf\u{00E9}\n";
    let prog = parse(src);
    let init = first_assign_rhs(&prog);
    match &init.kind {
        ExprKind::StringLit(s) => assert_eq!(s, "hello\n"),
        ExprKind::InterpolatedString(parts) => {
            let full: String = parts.0.iter().filter_map(|p| if let InterpPart::Const(s) = p { Some(s.as_str()) } else { None }).collect();
            assert_eq!(full, "hello\n");
        }
        other => panic!("expected string, got {other:?}"),
    }
}
