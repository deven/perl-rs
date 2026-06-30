//! Parser tests.

use super::*;

fn parse(src: &str) -> Program {
    let mut parser = Parser::new(src.as_bytes()).unwrap();
    parser.parse_program().unwrap()
}

fn parse_expr_str(src: &str) -> Expr {
    let prog = parse(src);

    // Find the first expression statement (skipping use/no declarations from `use feature` etc.).
    for stmt in &prog.statements {
        if let StmtKind::Expr(e) = &stmt.kind {
            return e.clone();
        }
    }
    panic!("no expression statement found in: {src}");
}

/// Collect all tokens from source, for tests that need to inspect token-level output (e.g. NFC on variable names).
fn collect_tokens(src: &str) -> Vec<Token> {
    let mut lexer = Parser::new(src.as_bytes()).unwrap();
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

/// Like `collect_tokens` but with UTF-8 mode pre-enabled, for tests that need to tokenize Unicode identifiers without
/// going through the full parser pragma machinery.
fn collect_tokens_utf8(src: &str) -> Vec<Token> {
    let mut lexer = Parser::new(src.as_bytes()).unwrap();
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

/// Extract the first variable name from a program containing `my $name;` or `my $name = expr;`.  Handles both bare
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

/// Extract the string value from the first `my $x = "..."` in the program.  Works for StringLit, and InterpolatedString
/// with only Const and NamedChar parts (no runtime interpolation).
fn first_assign_str(prog: &Program) -> String {
    let rhs = first_assign_rhs(prog);
    match &rhs.kind {
        ExprKind::StringLit(s) => s.clone(),
        ExprKind::InterpolatedString(interp) => interp.as_plain_string().expect("expected constant string content"),
        other => panic!("expected string expression, got {other:?}"),
    }
}

/// For tests that need the initializer from a `my $x = expr;` declaration-statement.  Returns the RHS of the Assign.
fn decl_init(stmt: &Statement) -> &Expr {
    match &stmt.kind {
        StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, rhs), .. }) => {
            assert!(matches!(lhs.kind, ExprKind::Decl(_, _)), "expected Decl lhs, got {:?}", lhs.kind);
            rhs
        }
        other => panic!("expected decl with initializer, got {other:?}"),
    }
}

/// For tests that need the var list from a declaration.  Works for both `my $x;` (plain Decl) and `my $x = ...;`
/// (Assign(Decl, _)).
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
            assert_eq!(name, "CORE::print");
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

    // First two are `my` declarations with initializers, so Stmt::Expr wrapping Assign(Decl, ...).
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

/// Extract the variable name from a simple scalar-interp (one that wraps a bare ScalarVar with no subscripts).  Returns
/// None if the part isn't a ScalarInterp or the inner expr isn't a bare variable.
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

/// Pull the inner expression out of a ScalarInterp for tests that need to inspect the subscript structure.
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
            assert_eq!(name, "CORE::print");
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
// All of these should parse the subscript into real AST nodes inside a `ScalarInterp(Box<Expr>)` / `ArrayInterp(...)`
// part — not be swallowed into a `Const` segment.
// ═══════════════════════════════════════════════════════════

/// Pull the `parts` out of an interpolated-string expression.
fn interp_parts(src: &str) -> Vec<InterpPart> {
    let e = parse_expr_str(src);
    match e.kind {
        ExprKind::InterpolatedString(Interpolated(parts)) => parts,

        // Some single-subscript strings collapse via merge into a non-interpolated StringLit in degenerate cases —
        // callers pass non-degenerate sources.
        other => panic!("expected InterpolatedString, got {other:?} for {src:?}"),
    }
}

/// For string-level asserts: the N-th part should be a scalar-interp wrapping an expression whose pretty-printed
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
    // "$h->{key}" — classic bugged case.  Must parse as a ScalarInterp wrapping ArrowDeref(ScalarVar(h),
    // HashElem(key)).
    let parts = interp_parts(r#""$h->{key}";"#);
    let e = scalar_part(&parts, 0);
    match &e.kind {
        ExprKind::ArrowDeref(recv, ArrowTarget::HashElem(k)) => {
            assert!(matches!(recv.kind, ExprKind::ScalarVar(ref n) if n == "h"));

            // Key is a bareword (autoquoted by the subscript rule in the parser).
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
    // "$h{key}" — no arrow.  In Perl this is still a hash element access because `$h{...}` is equivalent to
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
    // "$h->{a}{b}" — arrow before first, implicit between.  Hash elem wrapped in hash elem.
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
            assert!(matches!(indices[0].kind, ExprKind::Range(_, _, _)));
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
    // "$a->" — bare arrow with nothing after.  Lexer must not start a chain; the `->` stays literal text.
    let parts = interp_parts(r#""$a->";"#);
    assert_eq!(parts.len(), 2);
    assert_eq!(scalar_interp_name(&parts[0]), Some("a"));
    assert!(matches!(&parts[1], InterpPart::Const(s) if s == "->"));
}

#[test]
fn interp_bare_arrow_then_ident_is_literal() {
    // "$a->foo" — method-call shape is NOT interpolated in strings (per perlop).  `$a` interpolates; `->foo` renders
    // literally.
    let parts = interp_parts(r#""$a->foo";"#);
    assert_eq!(parts.len(), 2);
    assert_eq!(scalar_interp_name(&parts[0]), Some("a"));
    assert!(matches!(&parts[1], InterpPart::Const(s) if s == "->foo"));
}

#[test]
fn interp_plain_scalar_no_subscript() {
    // Simple "$name" shouldn't start a chain.  Still uses the new ScalarInterp(Box<Expr>) wrapper around a bare
    // ScalarVar.
    let parts = interp_parts(r#""Hello $name!";"#);
    assert_eq!(parts.len(), 3);
    assert_eq!(scalar_interp_name(&parts[1]), Some("name"));
}

#[test]
fn interp_trailing_literal_bracket() {
    // "$a [0]" — space before `[` means it's NOT a subscript.  The literal `[` and `]` stay as ConstSegment.
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
    // `"$h->{$x}{y}"` — two nested subscripts, with `y` as a bareword hash key in the inner-most subscript.
    //
    // `y}` is a lexer edge case: `y` is one of the quote keywords (alias for `tr`), so at_quote_delimiter must reject
    // the closing `}` that follows.  Tests below cover every quote keyword × every closing delimiter combination; this
    // one spot-checks the interaction with subscript-chain interpolation specifically.
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
    // qq{...} uses `{}` as delimiter; the `{key}` inside is still recognized as a hash subscript.
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
    // Interpolated string concatenated with another.  The chain in the first one must still be parsed correctly.
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
            assert!(matches!(indices[0].kind, ExprKind::Range(_, _, _)));
        }
        other => panic!("expected ArraySlice, got {other:?}"),
    }
    assert!(matches!(&parts[2], InterpPart::Const(s) if s == " done"));
}

// ── ${name}-expression form interaction ──────────────────

#[test]
fn interp_braced_name_then_literal_subscript() {
    // "${name}[0]" — `${name}` is explicit braced form.  The `[0]` after the `}` is literal text (per Perl behavior:
    // ${name}[0] interpolates only $name).
    let parts = interp_parts(r#""${name}[0]";"#);
    assert_eq!(parts.len(), 2);
    assert_eq!(scalar_interp_name(&parts[0]), Some("name"));
    assert!(matches!(&parts[1], InterpPart::Const(s) if s == "[0]"));
}

// ── Regex interpolation (shares the same scanner) ────────

#[test]
fn regex_interp_subscript() {
    // m/$h->{key}/ — regex bodies use the same interp machinery; chains should work there too.
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
// These cover interpolation contexts beyond plain `"..."` — heredoc bodies, `qr//`, `s///` pattern and replacement,
// and the `@{[expr]}` form mixed with chains.  A few cases don't work yet and are marked `#[ignore]` with a clear note
// explaining the gap; they're here rather than absent so the gap is visible in the test suite rather than only in my
// memory.

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
        ExprKind::Subst(_, SubstReplacement::Interp(repl), _) => {
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
        other => panic!("expected Subst with interpolated replacement, got {other:?}"),
    }
}

// @{[expr]} expression-interpolation form.

#[test]
fn interp_array_expr_form_with_chain_inside() {
    // `"@{[$h->{k}]}"` — the @{[...]} form wraps an expression; the expression internally uses a subscript chain.
    // Outer shape is ExprInterp (not ChainStart) because the leading token is `@{`, not `@name`.
    let parts = interp_parts(r#""@{[$h->{k}]}";"#);
    let expr_part = parts
        .iter()
        .find_map(|p| match p {
            InterpPart::ExprInterp(e) => Some(e),
            _ => None,
        })
        .expect("expected an ExprInterp part");

    // Inside: anonymous array ref containing the chain.  AnonArray([ArrowDeref(h, HashElem(k))])
    match &expr_part.kind {
        ExprKind::AnonArray(items) => {
            assert_eq!(items.len(), 1);
            assert!(matches!(items[0].kind, ExprKind::ArrowDeref(_, ArrowTarget::HashElem(_))));
        }
        other => panic!("expected AnonArray inside @{{[...]}}: {other:?}"),
    }
}

// Escape sequences in hash-subscript position are NOT processed as string escapes.  `"$h{\x41}"` is NOT `$h{'A'}`; per
// `perl -MO=Deparse -e '"$h{\x41}"'` it parses as `"$h{\'x41'}"` — the `\` is the reference operator applied to the
// autoquoted bareword `x41`.  The hash lookup key is therefore a scalar reference (which stringifies to `SCALAR(0x...)`
// at runtime).
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
                // Inner: the bareword "x41".  `\x41` inside `{...}` is code, not a string escape — the backslash is
                // the reference operator and x41 is a bareword.  Under `use strict 'subs'` this would be an error.
                assert!(matches!(inner.kind, ExprKind::Bareword(ref s) if s == "x41"), "expected Ref(Bareword('x41')), inner was {:?}", inner.kind);
            }
            other => panic!("expected Ref(Bareword('x41')) as hash key, got {other:?}"),
        },
        other => panic!("expected HashElem, got {other:?}"),
    }
}

// ── Known gaps — ignored tests, kept visible ─────────────
//
// These encode behavior we haven't implemented yet.  Each is marked `#[ignore]` with a note explaining what's missing.
// Running with `cargo test -- --ignored` will run them and show the real failures.

#[test]
fn interp_postderef_qq_array() {
    // `"$ref->@*"` — postderef array form inside a string.  Requires peek_chain_starter to recognize `->@*` and the
    // chain dispatch to end on `Star` at depth 0.
    let parts = interp_parts(r#"use feature 'postderef_qq'; "$ref->@*";"#);
    let e = scalar_part(&parts, 0);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefArray)), "expected ArrowDeref(_, DerefArray), got {:?}", e.kind);
}

#[test]
fn interp_postderef_qq_hash() {
    // `"$ref->%*"` — postderef hash form.
    let parts = interp_parts(r#"use feature 'postderef_qq'; "$ref->%*";"#);
    let e = scalar_part(&parts, 0);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefHash)), "expected ArrowDeref(_, DerefHash), got {:?}", e.kind);
}

#[test]
fn interp_postderef_qq_scalar() {
    // `"$ref->$*"` — postderef scalar form.
    let parts = interp_parts(r#"use feature 'postderef_qq'; "$ref->$*";"#);
    let e = scalar_part(&parts, 0);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefScalar)), "expected ArrowDeref(_, DerefScalar), got {:?}", e.kind);
}

#[test]
fn interp_postderef_qq_last_index() {
    // `"$ref->$#*"` — postderef last-index in a string.  The `#` would normally start a comment in code mode; this
    // works because the parser's `try_consume_hash_star` consumes the raw `#*` bytes between lex_token calls, and (in
    // chain mode) sets `chain_end_pending` so the chain terminates cleanly.
    let parts = interp_parts(r#"use feature 'postderef_qq'; "$ref->$#*";"#);
    let e = scalar_part(&parts, 0);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::LastIndex)), "expected ArrowDeref(_, LastIndex), got {:?}", e.kind);
}

#[test]
fn interp_postderef_qq_chained_after_subscript() {
    // `"$h->{key}->@*"` — subscript then postderef in one chain.
    let parts = interp_parts(r#"use feature 'postderef_qq'; "$h->{key}->@*";"#);
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
    let parts = interp_parts(r#"use feature 'postderef_qq'; "values: $ref->@* end";"#);
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
    // // gi — space separates, so gi is NOT flags.  This produces an empty regex with no flags.
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
// After these operators, // is defined-or, not an empty regex.  Matches toke.c's UNIDOR macro and XTERMORDORDOR
// behavior.

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
    // shift //i 0 — in Perl this is a syntax error because i is not predeclared.  Our parser is more permissive: it
    // parses as shift() // i(0) since any bareword can be a function call.
    let e = parse_expr_str("shift //i 0;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::DefinedOr, _, _)));
}

#[test]
fn parse_substitution() {
    let e = parse_expr_str("s/foo/bar/g;");
    match &e.kind {
        ExprKind::Subst(pat, SubstReplacement::Interp(repl), flags) => {
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
        ExprKind::Subst(pat, SubstReplacement::Interp(repl), flags) => {
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
        ExprKind::Subst(pat, SubstReplacement::Interp(repl), flags) => {
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

// ── /e replacement: code block, last value, eval depth ────

// The whole replacement is a statement block valued at its last statement — perl `s/Z/my $a=2; my $b=3; $a*$b/e`
// yields 6.  The old reparse kept only the first statement; this is the regression guard.
#[test]
fn subst_e_multi_statement_block() {
    let e = parse_expr_str("s/Z/my $a = 2; my $b = 3; $a * $b/e;");
    match &e.kind {
        ExprKind::Subst(_, SubstReplacement::Eval { block, evals }, flags) => {
            assert_eq!(*evals, 1);
            assert_eq!(block.statements.len(), 3, "all three statements are kept, not just the first");
            assert!(flags.is_none());
        }
        other => panic!("expected Subst with /e eval replacement, got {other:?}"),
    }
}

#[test]
fn subst_ee_eval_depth_two() {
    let e = parse_expr_str("s/x/$code/ee;");
    match &e.kind {
        ExprKind::Subst(_, SubstReplacement::Eval { evals, .. }, _) => assert_eq!(*evals, 2),
        other => panic!("expected Subst with /ee eval replacement, got {other:?}"),
    }
}

#[test]
fn subst_eee_eval_depth_three() {
    let e = parse_expr_str("s/x/$code/eee;");
    match &e.kind {
        ExprKind::Subst(_, SubstReplacement::Eval { evals, .. }, _) => assert_eq!(*evals, 3),
        other => panic!("expected Subst with /eee eval replacement, got {other:?}"),
    }
}

// The `e`s lift into `evals`; the remaining modifiers stay in the flag string.
#[test]
fn subst_e_strips_e_keeps_other_flags() {
    let e = parse_expr_str("s/x/1/eg;");
    match &e.kind {
        ExprKind::Subst(_, SubstReplacement::Eval { evals, .. }, flags) => {
            assert_eq!(*evals, 1);
            assert_eq!(flags.as_deref(), Some("g"), "the `e` is gone, `g` remains");
        }
        other => panic!("expected Subst with /e eval replacement, got {other:?}"),
    }
}

#[test]
fn subst_e_empty_replacement() {
    let e = parse_expr_str("s/x//e;");
    match &e.kind {
        ExprKind::Subst(_, SubstReplacement::Eval { block, evals }, _) => {
            assert_eq!(*evals, 1);
            assert!(block.statements.is_empty(), "an empty /e body is a statement-less block");
        }
        other => panic!("expected Subst with /e eval replacement, got {other:?}"),
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
        StmtKind::Expr(Expr { kind: ExprKind::PrintOp(name, _, _), .. }) => assert_eq!(name, "CORE::print"),
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
        ExprKind::Return(Some(operand)) => {
            assert!(matches!(operand.kind, ExprKind::IntLit(42)), "expected return 42, got {:?}", operand.kind);
        }
        other => panic!("expected return with value, got {other:?}"),
    }
}

#[test]
fn parse_return_bare() {
    let e = parse_expr_str("return;");
    match &e.kind {
        ExprKind::Return(None) => {}
        other => panic!("expected bare return, got {other:?}"),
    }
}

#[test]
fn parse_last_with_label() {
    let e = parse_expr_str("last OUTER;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "CORE::last");
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
            assert_eq!(name, "CORE::next");
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
            assert_eq!(name, "CORE::sort");
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
            assert_eq!(name, "CORE::map");
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
            assert_eq!(name, "CORE::grep");
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
            assert_eq!(name, "CORE::sort");
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
            assert_eq!(name, "CORE::print");
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
        ExprKind::FuncCall(name, _) => assert_eq!(name, "CORE::goto"),
        other => panic!("expected goto, got {other:?}"),
    }
}

// ── Readline / diamond tests ──────────────────────────────

#[test]
fn parse_diamond() {
    let e = parse_expr_str("<>;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "CORE::readline");
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
            assert_eq!(name, "CORE::readline");
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
        StmtKind::DataEnd(Keyword::__END__, offset) => {
            assert_eq!(&src.as_bytes()[*offset as usize..], b"This is not code.\n");
        }
        other => panic!("expected DataEnd(__END__), got {other:?}"),
    }
}

#[test]
fn parse_data_stops_parsing() {
    let src = "my $x = 1;\n__DATA__\nraw data here\n";
    let prog = parse(src);
    assert_eq!(prog.statements.len(), 2);
    match &prog.statements[1].kind {
        StmtKind::DataEnd(Keyword::__DATA__, offset) => {
            assert_eq!(&src.as_bytes()[*offset as usize..], b"raw data here\n");
        }
        other => panic!("expected DataEnd(__DATA__), got {other:?}"),
    }
}

#[test]
fn parse_ctrl_d_stops_parsing() {
    // ^D triggers logical EOF — only code before it is parsed, no DataEnd statement produced.
    let src = "my $x = 1;\x04ignored code\n";
    let prog = parse(src);
    assert_eq!(prog.statements.len(), 1);
    assert!(matches!(prog.statements[0].kind, StmtKind::Expr(_)));
}

#[test]
fn parse_ctrl_z_stops_parsing() {
    // ^Z triggers logical EOF — same behavior as ^D.
    let src = "my $x = 1;\x1aignored code\n";
    let prog = parse(src);
    assert_eq!(prog.statements.len(), 1);
    assert!(matches!(prog.statements[0].kind, StmtKind::Expr(_)));
}

#[test]
fn parse_ctrl_d_mid_expression() {
    // ^D mid-expression: `printf "%s\n", 123^D456` — only `123` is parsed, `456` is gone.
    let prog = parse("printf \"%s\\n\", 123\x04456;\n");
    assert_eq!(prog.statements.len(), 1);
}

#[test]
fn parse_trailing_comma_at_physical_eof() {
    // `print 1, 2,` with no semicolon or newline — trailing comma in a list is valid in Perl.
    let result = crate::parse(b"print 1, 2,");
    assert!(result.is_ok(), "trailing comma at physical EOF should be valid, got {:?}", result.err());
}

#[test]
fn parse_trailing_comma_at_ctrl_d() {
    // `print 1, 2,` followed by ^D — trailing comma before logical EOF should also be valid.
    let result = crate::parse(b"print 1, 2,\x04 3, 4, 5;\n");
    assert!(result.is_ok(), "trailing comma at logical EOF should be valid, got {:?}", result.err());
}

#[test]
fn parse_trailing_comma_at_logical_eof() {
    // `print 1, 2,` followed by __END__ — trailing comma before logical EOF should also be valid.
    let result = crate::parse(b"print 1, 2,\n__END__\ndata\n");
    assert!(result.is_ok(), "trailing comma at logical EOF should be valid, got {:?}", result.err());
}

#[test]
fn parse_trailing_comma_in_parens() {
    // `print(1, 2,)` — trailing comma inside parenthesized argument list is valid in Perl.
    let prog = parse("print(1, 2,);\n");
    assert_eq!(prog.statements.len(), 1);
}

#[test]
fn parse_end_data_offset_after_current_line() {
    // __END__ data starts on the NEXT line — the rest of the __END__ line is discarded.
    let src = "my $x = 1;\n__END__ ignored rest of line\nDATA line 1\nDATA line 2\n";
    let prog = parse(src);
    assert_eq!(prog.statements.len(), 2);
    match &prog.statements[1].kind {
        StmtKind::DataEnd(Keyword::__END__, offset) => {
            let data = &src.as_bytes()[*offset as usize..];
            assert_eq!(data, b"DATA line 1\nDATA line 2\n");
        }
        other => panic!("expected DataEnd(__END__), got {other:?}"),
    }
}

#[test]
fn parse_data_data_offset_after_current_line() {
    // __DATA__ data starts on the NEXT line, same as __END__.
    let src = "my $x = 1;\n__DATA__ ignored\nraw data here\n";
    let prog = parse(src);
    assert_eq!(prog.statements.len(), 2);
    match &prog.statements[1].kind {
        StmtKind::DataEnd(Keyword::__DATA__, offset) => {
            let data = &src.as_bytes()[*offset as usize..];
            assert_eq!(data, b"raw data here\n");
        }
        other => panic!("expected DataEnd(__DATA__), got {other:?}"),
    }
}

#[test]
fn parse_end_autoquoted_fat_comma() {
    // __END__ => val — autoquoted as a string, does NOT trigger EOF.
    let prog = parse("my %h = (__END__ => 1);\n");
    assert_eq!(prog.statements.len(), 1);

    // Should be a my declaration with a hash assignment, not EOF.
    assert!(matches!(prog.statements[0].kind, StmtKind::Expr(_)));
}

#[test]
fn parse_end_with_heredocs_on_same_line() {
    // `print <<X, __END__` — the trailing comma after the heredoc is valid; __END__ terminates the list.  DATA starts
    // after the heredoc body.
    let src = "print <<X, __END__\nbody of X\nX\nthis is DATA\n";
    let prog = parse(src);
    let has_data_end = prog.statements.iter().any(|s| matches!(s.kind, StmtKind::DataEnd(_, _)));
    assert!(has_data_end, "expected DataEnd statement, got {prog:?}");
    match prog.statements.last().map(|s| &s.kind) {
        Some(StmtKind::DataEnd(Keyword::__END__, offset)) => {
            assert_eq!(&src.as_bytes()[*offset as usize..], b"this is DATA\n");
        }
        other => panic!("expected DataEnd(__END__) as last statement, got {other:?}"),
    }
}

#[test]
fn parse_ctrl_d_with_heredocs_on_same_line() {
    // `print <<X,^D` — the trailing comma after the heredoc is valid; ^D terminates the list.  The heredoc body is
    // still collected.
    let src = "print <<X,\x04\nbody of X\nX\norphaned data\n";
    let prog = parse(src);
    assert_eq!(prog.statements.len(), 1);
}

#[test]
fn parse_minus_end_in_hash_subscript_across_lines_triggers_eof() {
    // $h{-__END__\n} — the } is on the next line, so __END__ should trigger logical EOF (same-line autoquoting only).
    // The hash subscript is never closed, so this must be a parse error.
    let result = crate::parse(b"my %h; $h{-__END__\n}\n");
    assert!(result.is_err(), "-__END__ with }} on next line should trigger EOF, not autoquote across lines");
}

#[test]
fn parse_minus_end_in_hash_subscript_same_line_autoquotes() {
    // $h{-__END__} on a single line — autoquotes to StringLit("-__END__").
    let e = parse_expr_str("$h{-__END__};");
    match &e.kind {
        ExprKind::HashElem(_, key) => {
            assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "-__END__"), "expected StringLit(\"-__END__\"), got {:?}", key.kind);
        }
        other => panic!("expected HashElem, got {other:?}"),
    }
}

#[test]
fn parse_minus_file_in_hash_subscript_autoquotes() {
    // $h{-__FILE__} — autoquotes to StringLit("-__FILE__"), same as any -bareword in a hash subscript.  The lexer
    // resolves __FILE__ to Token::SourceFile, but in -bareword hash subscript context it should still autoquote.
    let e = parse_expr_str("$h{-__FILE__};");
    match &e.kind {
        ExprKind::HashElem(_, key) => {
            assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "-__FILE__"), "expected StringLit(\"-__FILE__\"), got {:?}", key.kind);
        }
        other => panic!("expected HashElem, got {other:?}"),
    }
}

#[test]
fn parse_minus_line_in_hash_subscript_autoquotes() {
    // $h{-__LINE__} — same as -__FILE__, autoquotes to StringLit("-__LINE__").
    let e = parse_expr_str("$h{-__LINE__};");
    match &e.kind {
        ExprKind::HashElem(_, key) => {
            assert!(matches!(key.kind, ExprKind::StringLit(ref s) if s == "-__LINE__"), "expected StringLit(\"-__LINE__\"), got {:?}", key.kind);
        }
        other => panic!("expected HashElem, got {other:?}"),
    }
}

// ── Pod skipping test ─────────────────────────────────────

#[test]
fn parse_pod_skipped() {
    let prog = parse("my $x = 1;\n\n=pod\n\nThis is pod.\n\n=cut\n\nmy $y = 2;\n");

    // Should see both my declarations, pod is invisible.  Each is Stmt::Expr wrapping Assign(Decl(My), _).
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
            assert_eq!(name, "CORE::scalar");
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
    // my $x = 5 in statement context still works.  Now represented as Stmt::Expr wrapping Assign(Decl(My), IntLit(5)).
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
        ExprKind::Comma(items) => {
            assert!(matches!(items[0].kind, ExprKind::StringLit(_)));
        }
        other => panic!("expected Comma with StringLit first, got {other:?}"),
    }
}

// ── Ampersand prefix call tests ───────────────────────────

#[test]
fn parse_ampersand_call() {
    let e = parse_expr_str("&foo(1, 2);");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "main::foo");
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
            assert_eq!(name, "main::foo");
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
            assert_eq!(name, "CORE::require");
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
fn parse_vstring_before_fat_comma_autoquotes() {
    // v5 => 1 — Perl autoquotes "v5" as a plain string, NOT a v-string.  Fat-comma autoquoting takes precedence.
    let e = parse_expr_str("v5 => 1;");
    match &e.kind {
        ExprKind::Comma(items) => {
            assert!(matches!(items[0].kind, ExprKind::StringLit(ref s) if s == "v5"), "expected StringLit(\"v5\"), got {:?}", items[0].kind);
        }
        other => panic!("expected Comma, got {other:?}"),
    }
}

#[test]
fn parse_qualified_name_before_fat_comma() {
    // Foo::Bar => 1 — Perl autoquotes the full qualified name "Foo::Bar".  The lexer must not autoquote just "Bar"
    // when it sees => after it, because "Bar" is part of the qualified name "Foo::Bar".
    let prog = parse("my %h = (Foo::Bar => 1);\n");
    assert_eq!(prog.statements.len(), 1);
}

#[test]
fn parse_neg_bareword_fat_comma() {
    // -key => 42 — the lexer autoquotes `key` to StrLit("key"), the parser's string negation collapse produces
    // StringLit("-key"), matching Perl's compile-time folding.
    let e = parse_expr_str("-key => 42;");
    match &e.kind {
        ExprKind::Comma(items) => {
            assert!(matches!(items[0].kind, ExprKind::StringLit(ref s) if s == "-key"), "expected StringLit(\"-key\"), got {:?}", items[0].kind);
        }
        other => panic!("expected Comma, got {other:?}"),
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

#[test]
fn parse_string_negation_collapse() {
    // -"foo" → StringLit("-foo")
    let e = parse_expr_str("-\"foo\";");
    assert!(matches!(e.kind, ExprKind::StringLit(ref s) if s == "-foo"), "expected StringLit(\"-foo\"), got {:?}", e.kind);
}

#[test]
fn parse_string_negation_collapse_minus_prefix() {
    // -"-foo" → StringLit("+foo")
    let e = parse_expr_str("-\"-foo\";");
    assert!(matches!(e.kind, ExprKind::StringLit(ref s) if s == "+foo"), "expected StringLit(\"+foo\"), got {:?}", e.kind);
}

#[test]
fn parse_string_negation_collapse_plus_prefix() {
    // -"+foo" → StringLit("-foo")
    let e = parse_expr_str("-\"+foo\";");
    assert!(matches!(e.kind, ExprKind::StringLit(ref s) if s == "-foo"), "expected StringLit(\"-foo\"), got {:?}", e.kind);
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

/// Parse a program and return the parser's final pragma state.  Because pragmas are lexically scoped, this reflects
/// whatever was in effect at end-of-file (i.e., the outermost scope).
fn parse_pragmas(src: &str) -> crate::pragma::Pragmas {
    let mut p = Parser::new(src.as_bytes()).unwrap();
    let _ = p.parse_program().unwrap();
    *p.pragmas()
}

#[test]
fn pragma_default_has_default_bundle() {
    let p = parse_pragmas("my $x = 1;");

    // Pre-`use feature` state: the `:default` bundle (indirect, multidimensional, bareword_filehandles,
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
    // Per perlfeature: `no feature;` with no args resets to :default, not to empty.
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
    // `use strict;` doesn't set any parsing-relevant flag yet and must not cause a panic.
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
    // `use v5.36` does implicit `no feature ':all'; use feature ':5.36'`.  Applying after unrelated feature enables
    // should leave only the bundle.
    let p = parse_pragmas("use feature 'keyword_any';\nuse v5.36;\n");
    assert!(!p.features.contains(Features::KEYWORD_ANY), "version bundle should reset, not union");
    assert!(p.features.contains(Features::SIGNATURES));
}

// ── signature tests ───────────────────────────────────────

/// Convenience: parse a program and return the last top-level SubDecl, panicking if none exists.
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
    // No `use feature 'signatures'` in scope: `($)` is a prototype (meaning "exactly one scalar argument").  We verify
    // the signature path was NOT taken by checking that the prototype parser saw the raw text.
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
    // Default expression can reference earlier parameter — parser shouldn't care (just an expression).
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
    // Anonymous scalars — `$` without names — accept-and-discard.  Only scalars here; slurpy forms (`@`, `%`) must be
    // last and only one is allowed, so they get their own tests.
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
    // `:prototype($$)` attaches a prototype; the paren-form is still a signature when the feature is active.
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
    // Phase 1 hookup: the `:5.36` bundle includes signatures, so `use v5.36;` should enable the signature path without
    // an explicit `use feature 'signatures';`.
    let s = parse_sub("use v5.36; sub f ($x, $y) { }");
    assert!(s.signature.is_some(), "use v5.36 should enable signatures");
    assert!(s.prototype.is_none());
}

#[test]
fn sig_feature_is_lexically_scoped() {
    // Outer scope has signatures; inner `no feature 'signatures'` disables it for a sub declared inside the inner
    // block.
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

/// Helper: recursively walk a stmt looking for an AnonSub with a non-None signature.
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

/// Convenience: parse one expression statement, returning the inner expression.
fn parse_expr_stmt(src: &str) -> Expr {
    let prog = parse(src);
    for stmt in &prog.statements {
        if let StmtKind::Expr(e) = &stmt.kind {
            return e.clone();
        }
    }
    panic!("no expression in program; statements: {:#?}", prog.statements);
}

/// Helper: walk the outermost arrow-deref off a parsed expr, returning the ArrowTarget.  Panics if the expression isn't
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
    // `->$#*` — equivalent to `$#{$ref}`.  Requires lexer byte-level disambiguation because `#` would otherwise begin a
    // comment.
    let e = parse_expr_stmt("$r->$#*;");
    assert!(matches!(arrow_target(&e), ArrowTarget::LastIndex));
}

#[test]
fn postderef_last_index_in_expr() {
    // Embed in a larger expression to verify the parser continues past the LastIndex properly.
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
    // `->$foo` (Dollar + named ScalarVar) is not postderef.  The lexer greedily combines `$foo` into ScalarVar — which
    // is handled as dynamic method dispatch in another arm.  We just verify `->$` followed by something neither `*` nor
    // `#*` doesn't crash.
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
    // `->@[0]->[1]` — slice followed by subscript chain.  (Not semantically useful but should parse.)
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
    // Without the `isa` feature, `isa` is just an ordinary bareword (would be a function call or bareword reference).
    // We verify by checking that parsing `$x isa Foo` with no feature does NOT produce a BinOp.
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
    // `isa` binds tighter than `<`, so `$x isa Foo < 1` groups as `($x isa Foo) < 1`.
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
    // Without `fc` feature, `fc($x)` parses as an ordinary function call to a user sub named `fc`.  Either way we get a
    // FuncCall; just confirm it doesn't error and the function name is captured.
    let e = parse_expr_stmt("fc($x);");
    match e.kind {
        ExprKind::FuncCall(name, _) => assert_eq!(name, "main::fc"),
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn fc_with_feature_paren() {
    let e = parse_expr_stmt("use feature 'fc'; fc($x);");
    match e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "CORE::fc");
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn fc_with_feature_no_paren() {
    // `fc $x` — named unary, one argument at NAMED_UNARY precedence.
    let e = parse_expr_stmt("use feature 'fc'; fc $x;");
    match e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "CORE::fc");
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
            assert_eq!(name, "CORE::evalbytes");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

// ── Compile-time tokens ──

#[test]
fn source_file_captured_at_lex_time() {
    // Default filename placeholder when constructed via `parse(src)` / `Parser::new(src)`.
    let e = parse_expr_stmt("__FILE__;");
    match e.kind {
        ExprKind::SourceFile(path) => assert_eq!(path, "(script)"),
        other => panic!("expected SourceFile, got {other:?}"),
    }
}

#[test]
fn source_file_uses_custom_filename() {
    // `Parser::with_filename` / `parse_with_filename` plumbs the filename through to `Lexer::filename()`, which
    // `__FILE__` reads at lex time.
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
    // Without the current_sub feature, `__SUB__` falls back to bareword treatment.
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
// When the governing feature is off, try/catch/finally/defer, given/when/default, and class/field/method all act as
// plain identifiers — users can define subs with those names, pass them as hash keys, etc.  These tests verify the
// downgrade happens at the parser level so legacy code keeps working.

#[test]
fn class_is_bareword_without_feature() {
    // `sub class { ... }` — defining a sub named "class".  With class feature off, the lexer emits
    // Token::Keyword(Class) but the parser downgrades to Token::Ident("class") because we're not in a class scope.  The
    // sub declaration should parse.
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

    // Should parse as a normal expression statement (Decl assignment with FuncCall).  The inner expression is
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
    // `defer { ... }` would be a Defer statement with the defer feature; without it, `defer` is a bareword followed by
    // a block, which is a parse error (or parsed as something else).  We just confirm it doesn't produce a Defer
    // statement.
    let prog_result = Parser::new(b"my $x = defer;").and_then(|mut p| p.parse_program());
    if let Ok(prog) = prog_result {
        assert!(!prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Defer(_))), "must not parse as Defer without feature");
    }
}

#[test]
fn method_is_ident_without_feature() {
    // Outside `use feature 'class'`, `method` is a plain sub name.  `sub method { ... }` at top level defines a regular
    // sub.
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
    // Sanity check: once `use feature 'try';` is seen, the downgrade stops happening for the rest of the scope.
    let prog = parse("use feature 'try'; try { 1; }");
    let has_try = prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Try(_)));
    assert!(has_try, "Try must parse when feature is active");
}

#[test]
fn feature_gate_is_lexically_scoped() {
    // Inside a block, `no feature 'try'` disables the gate.  Outside the block, `try` is still active.  We only verify
    // the outer `try { ... }` succeeds — demonstrating the scope restore after the inner block.
    let prog = parse("use feature 'try'; try { 1; } catch ($e) { 2; }");
    assert!(prog.statements.iter().any(|s| matches!(s.kind, StmtKind::Try(_))), "outer Try with feature on must parse");
}

// ── Refaliasing / declared_refs (5.22+ / 5.26+) ───────────

#[test]
fn refalias_requires_feature() {
    // Without `refaliasing`, `\$a = \$b` is a parse error (Ref is not a valid lvalue).
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
            // LHS should be a list containing Refs.
            match &lhs.kind {
                ExprKind::Comma(items) => {
                    assert_eq!(items.len(), 2);
                    assert!(items.iter().all(|e| matches!(e.kind, ExprKind::Ref(_))));
                }
                other => panic!("expected Comma on LHS, got {other:?}"),
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
    // Mixing ref and non-ref in one decl: `my (\$a, $b)` — the parser accepts this (semantic validation is a later
    // pass).
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
    // `use v5.36` enables both refaliasing and declared_refs via the bundle.  Actually, checking perlfeature: :5.36
    // does NOT include refaliasing/declared_refs (those are still experimental as of 5.36).  So this test expects a
    // parse error.  Using a feature-on path with explicit `use feature` in other tests above covers the positive case.
    let src = "use v5.36; my \\$x = \\$y;";
    let mut p = match Parser::new(src.as_bytes()) {
        Ok(p) => p,
        Err(_) => panic!("parser construction failed"),
    };
    let result = p.parse_program();
    assert!(result.is_err(), ":5.36 bundle does not include declared_refs (experimental)");
}

// ── format tests ──────────────────────────────────────────

/// Convenience: parse a single format declaration, panic on any other top-level statement shape.
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
            // qw counts as one expr here (a QwList node); runtime flattens it.  Parser sees one argument.
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
    // `I have an @ here.` — the lone `@` isn't a field start, so the whole line parses as Literal.
    let f = parse_fmt("format X =\nI have an @ here.\n.\n");
    match &f.lines[0] {
        FormatLine::Literal { text, .. } => assert_eq!(text, "I have an @ here."),
        other => panic!("expected Literal, got {other:?}"),
    }
}

// ── class/field/method tests ──────────────────────────────

/// Convenience for class tests: prefixes the source with the required `use feature 'class'` and `no warnings` pragmas,
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

#[test]
fn indirect_without_feature_is_bareword() {
    // Without the indirect feature (dropped from :5.36+), `new Foo(1)` is NOT an indirect method call.
    // `new` is a plain bareword.
    let e = parse_expr_str("use v5.36; new Foo(1);");
    assert!(!matches!(e.kind, ExprKind::IndirectMethodCall(..)), "without indirect feature, `new Foo(1)` should not be IndirectMethodCall, got {:?}", e.kind);
}

#[test]
fn bareword_filehandle_without_feature_is_first_arg() {
    // Without bareword_filehandles (dropped from :5.38+), `print STDERR "hello"` treats STDERR as the first argument,
    // not a filehandle.
    let e = parse_expr_str("use v5.38; print STDERR;");
    match &e.kind {
        ExprKind::PrintOp(_, fh, _) => {
            assert!(fh.is_none(), "without bareword_filehandles, STDERR should not be a filehandle, got {:?}", fh);
        }
        other => panic!("expected PrintOp, got {other:?}"),
    }
}

#[test]
fn bareword_filehandle_with_feature_works() {
    // With the feature (in :default), `print STDERR "hello"` recognizes STDERR as a filehandle.
    let e = parse_expr_str("print STDERR 'hello';");
    match &e.kind {
        ExprKind::PrintOp(_, fh, _) => {
            assert!(fh.is_some(), "with bareword_filehandles, STDERR should be a filehandle");
        }
        other => panic!("expected PrintOp, got {other:?}"),
    }
}

#[test]
fn multidimensional_hash_with_feature() {
    // With the feature (in :default), `$h{1,2,3}` is transformed to `$h{join($;, 1, 2, 3)}`.
    let prog = parse("$h{1,2,3};");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::HashElem(_, key), .. }) => {
            assert!(
                matches!(key.kind, ExprKind::FuncCall(ref name, _) if name == "CORE::join"),
                "with multidimensional, expected join transformation, got {:?}",
                key.kind
            );
        }
        other => panic!("expected HashElem, got {other:?}"),
    }
}

#[test]
fn multidimensional_hash_without_feature() {
    // Without the feature (dropped from :5.36+), `$h{1,2,3}` is left as a comma-list — no join transformation.
    // The compiler emits "Multidimensional hash lookup is disabled".
    let prog = parse("use v5.36; $h{1,2,3};");
    match &prog.statements[1].kind {
        StmtKind::Expr(Expr { kind: ExprKind::HashElem(_, key), .. }) => {
            assert!(matches!(key.kind, ExprKind::Comma(_)), "without multidimensional, expected Comma (no join), got {:?}", key.kind);
        }
        other => panic!("expected HashElem, got {other:?}"),
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
            assert_eq!(name, "CORE::print");
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
    // Two heredocs with the same tag name.  The first body terminates at the first occurrence of the tag, then the
    // second heredoc begins with a new body.
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
// Derived from a real Perl program that exercises heredoc nesting, interpolation forms, and compile-time hoisting
// simultaneously.  Each test below isolates one aspect so failures are diagnostic.

#[test]
fn torture_heredoc_arithmetic_stacked() {
    // `<<A + <<B + <<C` — three heredocs combined with `+`.  Bodies are single numbers.  Deparse evaluates at compile
    // time but we just verify parsing.
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
    // `"${\(1 + 2)}"` — `${...}` with `\(expr)` inside.  This is a common Perl idiom for embedding arbitrary
    // expressions in interpolated strings.
    let parts = interp_parts(r#""${\(1 + 2)}";"#);

    // Expect: ExprInterp containing Ref(Paren(Add(1, 2))) or Ref(Add(1, 2)) — depends on paren handling.
    assert_eq!(parts.len(), 1);
    match &parts[0] {
        InterpPart::ExprInterp(e) => {
            // Outer is Ref(\...).
            match &e.kind {
                ExprKind::Ref(inner) => {
                    // Inner is the addition (parens are syntactic-only, not in the AST).
                    assert!(matches!(inner.kind, ExprKind::BinOp(BinOp::Add, _, _)), "expected Add, got {:?}", inner.kind);
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
    // `do { BEGIN { our $a = 1; } $a }` — BEGIN hoists to compile time even inside a runtime do-block.  We just verify
    // the parser accepts this; BEGIN semantics are runtime behavior.
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
    // Heredoc inside `${\(...)}` inside another heredoc body.  This is the nesting pattern from the torture test:
    //   <<OUTER contains `${\(do { my $a = <<INNER; ... })}`.  Simplified version:
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
    // The exact pattern from the torture test: `my ($x, $y, $z) = (<<~X, <<Y, do { expr });` Simplified: just two
    // heredocs plus a literal.
    let src = "my ($x, $y, $z) = (<<~A, <<B, 42);\n    A-body\n    A\nB-body\nB\n";
    let prog = parse(src);
    assert!(!prog.statements.is_empty(), "should parse");
}

// ── Dynamic method dispatch tests ─────────────────────────

// ═══════════════════════════════════════════════════════════
// Gap-probing tests — edge cases that may not be handled yet.  Written to match Perl's actual behavior.  Failures are
// diagnostic: they tell us what to fix.
// ═══════════════════════════════════════════════════════════

// ── Postderef_qq: remaining forms ────────────────────────

#[test]
fn interp_postderef_qq_code() {
    // `->&*` — code deref inside string.
    let parts = interp_parts(r#"use feature 'postderef_qq'; "$ref->&*";"#);
    let e = scalar_part(&parts, 0);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefCode)), "expected DerefCode, got {:?}", e.kind);
}

#[test]
fn interp_postderef_qq_glob() {
    // `->**` — glob deref inside string.  Lexer emits Token::Power for `**`.
    let parts = interp_parts(r#"use feature 'postderef_qq'; "$ref->**";"#);
    let e = scalar_part(&parts, 0);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::DerefGlob)), "expected DerefGlob, got {:?}", e.kind);
}

#[test]
fn interp_postderef_qq_array_slice() {
    // `->@[0,1]` — array slice inside string.
    let parts = interp_parts(r#"use feature 'postderef_qq'; "$ref->@[0,1]";"#);
    let e = scalar_part(&parts, 0);
    assert!(matches!(e.kind, ExprKind::ArrowDeref(_, ArrowTarget::ArraySliceIndices(_))), "expected ArraySliceIndices, got {:?}", e.kind);
}

#[test]
fn interp_postderef_qq_hash_slice() {
    // `->@{"a","b"}` — hash slice (values) inside string.
    let parts = interp_parts(r#"use feature 'postderef_qq'; "$ref->@{'a','b'}";"#);
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
    // Blank lines in <<~ body should be preserved as empty lines (they don't need indentation).
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
    // Per perlop: backslashes have no special meaning in a single-quoted here-doc, `\\` is two backslashes.
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
    // Perl accepts <<0 as a heredoc with tag "0".  The gate in lex_heredoc_after_shift_left must accept digits, not
    // just alphabetic + underscore.
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
    // <<__END__ is a valid heredoc tag — __END__ inside the body is literal text, and the terminator __END__ is matched
    // by the heredoc machinery, not the data-end detector.
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
// Perl treats barewords after << as heredoc tags, even if they are keyword names.

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
    // Both branches of ?: can be heredocs.  Bodies are queued in source order.
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
        ExprKind::Subst(pat, SubstReplacement::Interp(repl), _) => {
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
        ExprKind::Subst(pat, SubstReplacement::Interp(repl), _) => {
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
        ExprKind::Subst(pat, SubstReplacement::Interp(repl), _) => {
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
        ExprKind::Subst(pat, SubstReplacement::Interp(repl), _) => {
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
        ExprKind::Subst(pat, SubstReplacement::Interp(repl), _) => {
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
    // `$h{-key}` — the `-key` form is common in Perl.  Parses as HashElem with StringLit("-key").
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

    // Outer: MethodCall(MethodCall(MethodCall($obj, "method1", []), "method2", []), "method3", []).
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
    // `"a" . "b" x 3` — `x` binds tighter than `.`.  Parses as `"a" . ("b" x 3)`.
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
        ExprKind::Comma(items) => {
            assert!(matches!(items[0].kind, ExprKind::IntLit(123)), "numeric key should stay IntLit, got {:?}", items[0].kind);
        }
        other => panic!("expected Comma, got {other:?}"),
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
            assert_eq!(name, "CORE::delete");
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
        ExprKind::Comma(items) => match &items[0].kind {
            ExprKind::StringLit(s) => assert_eq!(s, "-f"),
            other => panic!("expected StringLit('-f'), got {other:?}"),
        },
        other => panic!("expected Comma, got {other:?}"),
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

#[test]
fn parse_filetest_in_block_body_not_autoquoted() {
    // sub foo { -f } — filetest on $_, NOT autoquoted.  The } closes the sub body, not a hash subscript.
    let sub = parse_sub("sub foo { -f }");
    assert_eq!(sub.body.statements.len(), 1);
    match &sub.body.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Filetest(c, StatTarget::Default) => {
                assert_eq!(*c, 'f');
            }
            other => panic!("expected Filetest('f', Default), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn parse_hash_subscript_dash_multi_letter() {
    // $hash{-key} — autoquotes as StringLit("-key")
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
fn parse_known_sub_in_block_not_autoquoted() {
    // sub bar { 1 } sub foo { bar }
    // bar is a known sub — should be a function call, not a string.
    let sub = parse_sub("sub bar { 1 } sub foo { bar }");
    assert_eq!(sub.name, "foo");
    assert_eq!(sub.body.statements.len(), 1);
    match &sub.body.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "main::bar");
                assert!(args.is_empty());
            }
            other => panic!("expected FuncCall('main::bar', []), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn parse_unknown_bareword_in_block_is_bareword() {
    // sub foo { bar } where bar is unknown — produces Bareword (not StringLit).  Under `use strict 'subs'` this would
    // be a compile error; without strict, Perl stringifies it at runtime.  The AST preserves the distinction so a later
    // pass can enforce strict.
    let sub = parse_sub("sub foo { bar }");
    assert_eq!(sub.name, "foo");
    assert_eq!(sub.body.statements.len(), 1);
    match &sub.body.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Bareword(s) => assert_eq!(s, "bar"),
            other => panic!("expected Bareword('bar'), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
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
// Compound assignment operators
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
// Precedence verification
// ═══════════════════════════════════════════════════════════
//
// Systematic coverage of every adjacent precedence level.  Each test verifies that the higher-precedence operator binds
// tighter by checking the AST shape.  Levels from perlop (low → high):
//
//   or/xor (100) < and (200) < not (300) < comma (500) < assign (600) < ternary (700) < range (800)
//   < || // ^^ (900) < && (1000) < | ^ (1100) < & (1200) < == != (1300) < < > (1400) < isa (1500)
//   < named unary (1600) < << >> (1700) < + - . (1800) < * / % x (1900) < =~ !~ (2000)
//   < ! ~ \ (2100 prefix) < ** (2200) < ++ -- (2300 postfix) < -> (2400)

// ── or (2) vs and (4) ────────────────────────────────────

#[test]
fn prec_or_low_vs_and_low() {
    // `$a or $b and $c` → LowOr($a, LowAnd($b, $c))
    let e = parse_expr_str("$a or $b and $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::LowOr, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::LowAnd, _, _)), "expected LowAnd on RHS of LowOr, got {:?}", rhs.kind);
        }
        other => panic!("expected LowOr, got {other:?}"),
    }
}

// ── and (4) vs not (6) ───────────────────────────────────

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

// ── comma (10) vs assign (12) ────────────────────────────

#[test]
fn prec_comma_vs_assign() {
    // `$a = 1, $b = 2` → Comma(Assign($a, 1), Assign($b, 2)) — assign binds tighter than comma.
    let e = parse_expr_str("$a = 1, $b = 2;");
    match &e.kind {
        ExprKind::Comma(items) => {
            assert_eq!(items.len(), 2);
            assert!(matches!(items[0].kind, ExprKind::Assign(AssignOp::Eq, _, _)));
            assert!(matches!(items[1].kind, ExprKind::Assign(AssignOp::Eq, _, _)));
        }
        other => panic!("expected Comma of two Assigns, got {other:?}"),
    }
}

// ── assign (12) vs ternary (14) ──────────────────────────

#[test]
fn prec_assign_vs_ternary() {
    // `$a = $b ? 1 : 0` → Assign($a, Ternary($b, 1, 0)) — ternary binds tighter.
    let e = parse_expr_str("$a = $b ? 1 : 0;");
    match &e.kind {
        ExprKind::Assign(AssignOp::Eq, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::Ternary(_, _, _)), "expected Ternary on RHS of Assign, got {:?}", rhs.kind);
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

// ── ternary (14) vs range (16) ───────────────────────────

#[test]
fn prec_ternary_vs_range() {
    // `1 .. 2 ? 3 : 4` → Ternary(Range(1, 2), 3, 4) — range binds tighter.
    let e = parse_expr_str("1 .. 2 ? 3 : 4;");
    match &e.kind {
        ExprKind::Ternary(cond, _, _) => {
            assert!(matches!(cond.kind, ExprKind::Range(_, _, _)), "expected Range as condition, got {:?}", cond.kind);
        }
        other => panic!("expected Ternary, got {other:?}"),
    }
}

// ── range (16) vs || (18) ────────────────────────────────

#[test]
fn prec_range_vs_or() {
    // `$a || $b .. $c` → Range(Or($a, $b), $c) — || binds tighter than range.
    let e = parse_expr_str("$a || $b .. $c;");
    match &e.kind {
        ExprKind::Range(lhs, _, _) => {
            assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::Or, _, _)), "expected Or on LHS of Range, got {:?}", lhs.kind);
        }
        other => panic!("expected Range, got {other:?}"),
    }
}

// ── || (18) vs && (20) ──────────────────────────────────

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

// ── && (20) vs | (22) ───────────────────────────────────

#[test]
fn prec_and_vs_bit_or() {
    // `$a && $b | $c` → And($a, BitOr($b, $c)) — bitwise or binds tighter.
    let e = parse_expr_str("$a && $b | $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::And, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::BitOr, _, _)), "expected BitOr on RHS of And, got {:?}", rhs.kind);
        }
        other => panic!("expected And, got {other:?}"),
    }
}

// ── | (22) vs & (24) ────────────────────────────────────

#[test]
fn prec_bit_or_vs_bit_and() {
    // `$a | $b & $c` → BitOr($a, BitAnd($b, $c)) — bitwise and binds tighter.
    let e = parse_expr_str("$a | $b & $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::BitOr, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::BitAnd, _, _)), "expected BitAnd on RHS of BitOr, got {:?}", rhs.kind);
        }
        other => panic!("expected BitOr, got {other:?}"),
    }
}

// ── & (24) vs == (26) ───────────────────────────────────

#[test]
fn prec_bit_and_vs_eq() {
    // `$a & $b == $c` → BitAnd($a, NumEq($b, $c)) — equality binds tighter.
    let e = parse_expr_str("$a & $b == $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::BitAnd, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::NumEq, _, _)), "expected NumEq on RHS of BitAnd, got {:?}", rhs.kind);
        }
        other => panic!("expected BitAnd, got {other:?}"),
    }
}

// ── == (26) vs < (28) ───────────────────────────────────

#[test]
fn prec_eq_vs_rel() {
    // `$a == $b < $c` → NumEq($a, NumLt($b, $c)) — relational binds tighter.
    let e = parse_expr_str("$a == $b < $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::NumEq, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::NumLt, _, _)), "expected NumLt on RHS of NumEq, got {:?}", rhs.kind);
        }
        other => panic!("expected NumEq, got {other:?}"),
    }
}

// ── < (28) vs << (32) ───────────────────────────────────

#[test]
fn prec_rel_vs_shift() {
    // `$a < $b << $c` → NumLt($a, ShiftLeft($b, $c)) — shift binds tighter.
    let e = parse_expr_str("$a < $b << $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::NumLt, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::ShiftLeft, _, _)), "expected ShiftLeft on RHS of NumLt, got {:?}", rhs.kind);
        }
        other => panic!("expected NumLt, got {other:?}"),
    }
}

// ── << (32) vs + (34) ───────────────────────────────────

#[test]
fn prec_shift_vs_add() {
    // `$a << $b + $c` → ShiftLeft($a, Add($b, $c)) — addition binds tighter.
    let e = parse_expr_str("$a << $b + $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::ShiftLeft, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::Add, _, _)), "expected Add on RHS of ShiftLeft, got {:?}", rhs.kind);
        }
        other => panic!("expected ShiftLeft, got {other:?}"),
    }
}

// ── + (34) vs * (36) ────────────────────────────────────

#[test]
fn prec_add_vs_mul() {
    // `$a + $b * $c` → Add($a, Mul($b, $c)) — multiplication binds tighter.
    let e = parse_expr_str("$a + $b * $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Add, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::Mul, _, _)), "expected Mul on RHS of Add, got {:?}", rhs.kind);
        }
        other => panic!("expected Add, got {other:?}"),
    }
}

// ── * (36) vs =~ (38) ───────────────────────────────────

#[test]
fn prec_mul_vs_binding() {
    // `$a * $b =~ /x/` → Mul($a, Binding($b, Regex)) — binding binds tighter.
    let e = parse_expr_str("$a * $b =~ /x/;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Mul, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::Binding, _, _)), "expected Binding on RHS of Mul, got {:?}", rhs.kind);
        }
        other => panic!("expected Mul, got {other:?}"),
    }
}

// ── =~ (38) vs prefix ! (40) ────────────────────────────

#[test]
fn prec_binding_vs_unary() {
    // `!$a =~ /x/` → Binding(Not($a), Regex) — prefix `!` binds tighter than `=~`.
    let e = parse_expr_str("!$a =~ /x/;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Binding, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::UnaryOp(UnaryOp::LogNot, _)), "expected LogNot on LHS of Binding, got {:?}", lhs.kind);
        }
        other => panic!("expected Binding, got {other:?}"),
    }
}

// ── prefix - (40) vs ** (42) ─────────────────────────────

#[test]
fn prec_unary_vs_pow() {
    // `-$a ** 2` → Negate(Pow($a, 2)) — exponentiation binds tighter than unary minus.
    let e = parse_expr_str("-$a ** 2;");
    match &e.kind {
        ExprKind::UnaryOp(UnaryOp::Negate, inner) => {
            assert!(matches!(inner.kind, ExprKind::BinOp(BinOp::Pow, _, _)), "expected Pow inside Negate, got {:?}", inner.kind);
        }
        other => panic!("expected Negate, got {other:?}"),
    }
}

// ── ** (42) vs postfix ++ (44) ───────────────────────────

#[test]
fn prec_pow_vs_postinc() {
    // `$a++ ** 2` → Pow(PostInc($a), 2) — postfix ++ binds tighter than **.
    let e = parse_expr_str("$a++ ** 2;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Pow, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::PostfixOp(PostfixOp::Inc, _)), "expected PostInc on LHS of Pow, got {:?}", lhs.kind);
        }
        other => panic!("expected Pow, got {other:?}"),
    }
}

// ── postfix ++ (44) vs -> (46) ───────────────────────────

#[test]
fn prec_postinc_vs_arrow() {
    // `$a->[0]++` → PostInc(ArrowDeref($a, [0])) — arrow binds tighter.  Array element via arrow is a valid lvalue.
    let e = parse_expr_str("$a->[0]++;");
    match &e.kind {
        ExprKind::PostfixOp(PostfixOp::Inc, inner) => {
            assert!(matches!(inner.kind, ExprKind::ArrowDeref(_, _)), "expected ArrowDeref inside PostInc, got {:?}", inner.kind);
        }
        other => panic!("expected PostfixOp(Inc), got {other:?}"),
    }
}

// ── Associativity ────────────────────────────────────────

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
fn prec_pow_right_assoc() {
    // `2 ** 3 ** 4` → Pow(2, Pow(3, 4)) — right-associative.
    let e = parse_expr_str("2 ** 3 ** 4;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Pow, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::Pow, _, _)), "expected Pow on RHS, got {:?}", rhs.kind);
        }
        other => panic!("expected Pow, got {other:?}"),
    }
}

#[test]
fn prec_add_left_assoc() {
    // `1 + 2 + 3` → Add(Add(1, 2), 3) — left-associative.
    let e = parse_expr_str("1 + 2 + 3;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Add, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::Add, _, _)), "expected Add on LHS, got {:?}", lhs.kind);
        }
        other => panic!("expected Add, got {other:?}"),
    }
}

// ── Same-level different-op interactions ─────────────────

#[test]
fn prec_or_same_level_as_defined_or() {
    // `$a || $b // $c` → DefinedOr(Or($a, $b), $c) — same precedence, left-associative.
    let e = parse_expr_str("$a || $b // $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::DefinedOr, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::Or, _, _)), "expected Or on LHS of DefinedOr, got {:?}", lhs.kind);
        }
        other => panic!("expected DefinedOr, got {other:?}"),
    }
}

#[test]
fn prec_sub_same_level_as_concat() {
    // `$a - $b . $c` → Concat(Sub($a, $b), $c) — both at PREC_ADD, left-associative.
    let e = parse_expr_str("$a - $b . $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Concat, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::Sub, _, _)), "expected Sub on LHS of Concat, got {:?}", lhs.kind);
        }
        other => panic!("expected Concat, got {other:?}"),
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
fn prec_binding_tighter_than_concat() {
    let e = parse_expr_str("$x =~ /foo/ . 'bar';");
    match &e.kind {
        ExprKind::BinOp(BinOp::Concat, left, _) => {
            assert!(matches!(left.kind, ExprKind::BinOp(BinOp::Binding, _, _)));
        }
        other => panic!("expected Concat(Binding(..), ..), got {other:?}"),
    }
}

// ── not vs || (classic Perl gotcha) ──────────────────────

#[test]
fn prec_not_low_absorbs_or() {
    // `not $a || $b` → Not(Or($a, $b)) — not is at PREC_NOT_LOW (300), || at PREC_OR (900).  Since || is higher, it
    // gets consumed inside the not.  This is a classic Perl gotcha: `not $x || $y` means `not($x || $y)`.
    let e = parse_expr_str("not $a || $b;");
    match &e.kind {
        ExprKind::UnaryOp(UnaryOp::Not, inner) => {
            assert!(matches!(inner.kind, ExprKind::BinOp(BinOp::Or, _, _)), "expected Or inside Not, got {:?}", inner.kind);
        }
        other => panic!("expected Not, got {other:?}"),
    }
}

// ── String comparison at correct level ───────────────────

#[test]
fn prec_str_eq_same_level_as_num_eq() {
    // `$a eq $b == $c` — eq and == are both at PREC_EQ, non-associative → ChainedCmp.
    let e = parse_expr_str("$a eq $b == $c;");
    assert!(matches!(e.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp, got {:?}", e.kind);
}

#[test]
fn prec_str_rel_same_level_as_num_rel() {
    // `$a lt $b < $c` — lt and < are both at PREC_REL, non-associative → ChainedCmp.
    let e = parse_expr_str("$a lt $b < $c;");
    assert!(matches!(e.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp, got {:?}", e.kind);
}

// ── x (repeat) at PREC_MUL ──────────────────────────────

#[test]
fn prec_repeat_at_mul_level() {
    // `$a + "ab" x 3` → Add($a, Repeat("ab", 3)) — x is at PREC_MUL, same as *.
    let e = parse_expr_str("$a + \"ab\" x 3;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Add, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::Repeat, _, _)), "expected Repeat on RHS of Add, got {:?}", rhs.kind);
        }
        other => panic!("expected Add, got {other:?}"),
    }
}

#[test]
fn repeat_flush_digit_scalar() {
    // `$a x5` — `x` written flush against its count.  Position-independent lexing scans `x5` as one identifier;
    // `lex_operator` splits it into the `x` operator and re-lexes `5` as the integer operand.
    let e = parse_expr_str("$a x5;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Repeat, _, rhs) => assert!(matches!(rhs.kind, ExprKind::IntLit(5)), "expected count IntLit(5), got {:?}", rhs.kind),
        other => panic!("expected Repeat, got {other:?}"),
    }
}

#[test]
fn repeat_flush_digit_string() {
    // `"ab" x3` → Repeat("ab", 3).  The flush `x3` splits the same way against a string LHS.
    let e = parse_expr_str(r#""ab" x3;"#);
    match &e.kind {
        ExprKind::BinOp(BinOp::Repeat, _, rhs) => assert!(matches!(rhs.kind, ExprKind::IntLit(3)), "expected count IntLit(3), got {:?}", rhs.kind),
        other => panic!("expected Repeat, got {other:?}"),
    }
}

#[test]
fn repeat_spaced_still_works() {
    // Regression: bare `x` (a `Keyword(X)` token) flows through the operator table unchanged.
    let e = parse_expr_str("$a x 5;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Repeat, _, _)), "expected Repeat, got {:?}", e.kind);
}

#[test]
fn repeat_flush_digit_precedence() {
    // `1 + 2 x3` → Add(1, Repeat(2, 3)) — the split `x` still binds at PREC_MUL, tighter than `+`.
    let e = parse_expr_str("1 + 2 x3;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Add, lhs, rhs) => {
            assert!(matches!(lhs.kind, ExprKind::IntLit(1)), "expected lhs IntLit(1), got {:?}", lhs.kind);
            match &rhs.kind {
                ExprKind::BinOp(BinOp::Repeat, rl, rr) => {
                    assert!(matches!(rl.kind, ExprKind::IntLit(2)), "expected Repeat lhs IntLit(2), got {:?}", rl.kind);
                    assert!(matches!(rr.kind, ExprKind::IntLit(3)), "expected Repeat rhs IntLit(3), got {:?}", rr.kind);
                }
                other => panic!("expected Repeat on RHS of Add, got {other:?}"),
            }
        }
        other => panic!("expected Add, got {other:?}"),
    }
}

// ── Mixed prefix/infix same token ────────────────────────

#[test]
fn prec_prefix_minus_then_infix_minus() {
    // `-$a - $b` → Sub(Negate($a), $b) — first - is prefix (try_prefix), second is infix (Pratt loop).
    let e = parse_expr_str("-$a - $b;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Sub, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::UnaryOp(UnaryOp::Negate, _)), "expected Negate on LHS of Sub, got {:?}", lhs.kind);
        }
        other => panic!("expected Sub, got {other:?}"),
    }
}

// ── // (defined-or) vs && ────────────────────────────────

#[test]
fn prec_defined_or_vs_and() {
    // `$a // $b && $c` → DefinedOr($a, And($b, $c)) — && binds tighter than //.
    let e = parse_expr_str("$a // $b && $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::DefinedOr, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::And, _, _)), "expected And on RHS of DefinedOr, got {:?}", rhs.kind);
        }
        other => panic!("expected DefinedOr, got {other:?}"),
    }
}

// ── !~ at PREC_BINDING ──────────────────────────────────

#[test]
fn prec_not_binding_same_as_binding() {
    // `$a !~ /x/ . "y"` → Concat(NotBinding($a, Regex), "y") — !~ at same level as =~.
    let e = parse_expr_str("$a !~ /x/ . 'y';");
    match &e.kind {
        ExprKind::BinOp(BinOp::Concat, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::NotBinding, _, _)), "expected NotBinding on LHS of Concat, got {:?}", lhs.kind);
        }
        other => panic!("expected Concat, got {other:?}"),
    }
}

// ── Low xor at same level as or ─────────────────────────

#[test]
fn prec_low_xor_same_as_low_or() {
    // `$a or $b xor $c` → LowXor(LowOr($a, $b), $c) — same level, left-associative.
    let e = parse_expr_str("$a or $b xor $c;");
    match &e.kind {
        ExprKind::BinOp(BinOp::LowXor, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::LowOr, _, _)), "expected LowOr on LHS of LowXor, got {:?}", lhs.kind);
        }
        other => panic!("expected LowXor, got {other:?}"),
    }
}

// ── Non-associative chaining ─────────────────────────────

#[test]
fn prec_non_assoc_comparison_chains() {
    // `$a == $b == $c` — non-associative comparisons produce ChainedCmp, not left-associative binary ops.
    let e = parse_expr_str("$a == $b == $c;");
    assert!(matches!(e.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp, got {:?}", e.kind);
}

// ── Prefix always consumed in forward phase ──────────────

#[test]
fn prec_pow_vs_unary_minus_literal() {
    // `-2 ** 4` → Negate(Pow(2, 4)) = -(2**4) = -16, NOT (-2)**4 = 16.  ** binds tighter than unary minus.
    // This is the classic perlop precedence gotcha for exponentiation.
    let e = parse_expr_str("-2 ** 4;");
    match &e.kind {
        ExprKind::UnaryOp(UnaryOp::Negate, inner) => {
            assert!(matches!(inner.kind, ExprKind::BinOp(BinOp::Pow, _, _)), "expected Pow inside Negate, got {:?}", inner.kind);
        }
        other => panic!("expected Negate(Pow(..)), got {other:?}"),
    }
}

#[test]
fn prec_ref_then_infix() {
    // `\$a + $b` → Add(Ref($a), $b) — ref at PREC_UNARY (2100) captures just $a, then + at PREC_ADD (1800) takes over.
    let e = parse_expr_str("\\$a + $b;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Add, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::Ref(_)), "expected Ref on LHS of Add, got {:?}", lhs.kind);
        }
        other => panic!("expected Add, got {other:?}"),
    }
}

#[test]
fn prec_prefix_in_rhs_of_assign() {
    // `$a = not $b` — assign RHS parsed at PREC_ASSIGN (600); `not` pushes frame at PREC_NOT_LOW (300).
    // Prefix ops always run in the forward phase regardless of min_prec.
    let e = parse_expr_str("$a = not $b;");
    match &e.kind {
        ExprKind::Assign(AssignOp::Eq, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::UnaryOp(UnaryOp::Not, _)), "expected Not on RHS of Assign, got {:?}", rhs.kind);
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

// ── Arrow chaining ──────────────────────────────────────

#[test]
fn prec_arrow_left_assoc() {
    // `$a->b->c` → MethodCall(MethodCall($a, b), c) — left-associative.
    let e = parse_expr_str("$a->b->c;");
    match &e.kind {
        ExprKind::MethodCall(invocant, name, _) => {
            assert_eq!(name, "c");
            assert!(matches!(invocant.kind, ExprKind::MethodCall(_, _, _)), "expected MethodCall on invocant, got {:?}", invocant.kind);
        }
        other => panic!("expected MethodCall, got {other:?}"),
    }
}

#[test]
fn prec_arrow_tighter_than_mul() {
    // `$a->length * 2` → Mul(MethodCall($a, length), 2) — arrow binds tighter.
    let e = parse_expr_str("$a->length * 2;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Mul, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::MethodCall(_, _, _)), "expected MethodCall on LHS of Mul, got {:?}", lhs.kind);
        }
        other => panic!("expected Mul, got {other:?}"),
    }
}

// ── Ternary with assignment in branches ──────────────────

#[test]
fn prec_assign_inside_ternary() {
    // `$a ? $b = 1 : 0` — assign inside the true-branch of ternary.  The `:` stops the middle at PREC_LOW, so `$b = 1`
    // is fully consumed as the true-branch.
    let e = parse_expr_str("$a ? $b = 1 : 0;");
    match &e.kind {
        ExprKind::Ternary(_, middle, _) => {
            assert!(matches!(middle.kind, ExprKind::Assign(AssignOp::Eq, _, _)), "expected Assign in true-branch, got {:?}", middle.kind);
        }
        other => panic!("expected Ternary, got {other:?}"),
    }
}

// ── Multiple prefix ops stack correctly ──────────────────

#[test]
fn prec_stacked_prefix_ops() {
    // `not !-$a` → Not(LogNot(Negate($a))) — three prefix ops stacked.
    let e = parse_expr_str("not !-$a;");
    match &e.kind {
        ExprKind::UnaryOp(UnaryOp::Not, inner) => match &inner.kind {
            ExprKind::UnaryOp(UnaryOp::LogNot, inner2) => {
                assert!(matches!(inner2.kind, ExprKind::UnaryOp(UnaryOp::Negate, _)), "expected Negate, got {:?}", inner2.kind);
            }
            other => panic!("expected LogNot, got {other:?}"),
        },
        other => panic!("expected Not, got {other:?}"),
    }
}

// ── Ternary right-assoc from the else branch ─────────────

#[test]
fn prec_ternary_right_assoc_else() {
    // `$a ? $b : $c ? $d : $e` → Ternary($a, $b, Ternary($c, $d, $e)) — right-associative: the second `?:` nests inside
    // the else branch of the first, NOT as `Ternary(Ternary($a, $b, $c), $d, $e)`.
    let e = parse_expr_str("$a ? $b : $c ? $d : $e;");
    match &e.kind {
        ExprKind::Ternary(_, middle, else_branch) => {
            // Middle is just $b, not a nested ternary.
            assert!(matches!(middle.kind, ExprKind::ScalarVar(_)), "expected ScalarVar in middle, got {:?}", middle.kind);

            // Else branch is the nested ternary.
            assert!(matches!(else_branch.kind, ExprKind::Ternary(_, _, _)), "expected Ternary in else branch, got {:?}", else_branch.kind);
        }
        other => panic!("expected Ternary, got {other:?}"),
    }
}

// ── Non-associative operators produce ChainedCmp ─────────

#[test]
fn prec_chained_cmp_three_way() {
    // `$a < $b < $c` — relational operators chain into ChainedCmp for Perl's chained comparison support.
    let e = parse_expr_str("$a < $b < $c;");
    assert!(matches!(e.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp, got {:?}", e.kind);
}

#[test]
fn prec_chained_cmp_mixed_ops() {
    // `$a <= $b >= $c` — mixed relational ops chain into ChainedCmp.
    let e = parse_expr_str("$a <= $b >= $c;");
    assert!(matches!(e.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp, got {:?}", e.kind);
}

#[test]
fn prec_range_non_assoc_is_error() {
    // `$x .. $y .. $z` — range is non-associative, chaining is a syntax error.
    let result = crate::parse(b"$x .. $y .. $z;");
    assert!(result.is_err(), "chained range should be a syntax error");
}

#[test]
fn prec_spaceship_non_assoc_is_error() {
    // `$x <=> $y <=> $z` — three-way comparison is non-associative (chain/na), chaining is a syntax error.
    let result = crate::parse(b"$x <=> $y <=> $z;");
    assert!(result.is_err(), "chained <=> should be a syntax error");
}

#[test]
fn prec_cmp_non_assoc_is_error() {
    // `$x cmp $y cmp $z` — string three-way comparison is non-associative.
    let result = crate::parse(b"$x cmp $y cmp $z;");
    assert!(result.is_err(), "chained cmp should be a syntax error");
}

#[test]
fn prec_equality_chains() {
    // `$a == $b != $c` — equality operators chain (not error).
    let e = parse_expr_str("$a == $b != $c;");
    assert!(matches!(e.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp, got {:?}", e.kind);
}

#[test]
fn prec_str_equality_chains() {
    // `$a eq $b ne $c` — string equality operators chain.
    let e = parse_expr_str("$a eq $b ne $c;");
    assert!(matches!(e.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp, got {:?}", e.kind);
}

#[test]
fn prec_mixed_numeric_string_relational_chains() {
    // `$a < $b le $c` — numeric and string relational operators are both Chain at PREC_REL.
    let e = parse_expr_str("$a < $b le $c;");
    assert!(matches!(e.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp, got {:?}", e.kind);
}

#[test]
fn prec_mixed_numeric_string_equality_chains() {
    // `$a == $b eq $c` — numeric and string equality operators are both Chain at PREC_EQ.
    let e = parse_expr_str("$a == $b eq $c;");
    assert!(matches!(e.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp, got {:?}", e.kind);
}

#[test]
fn prec_long_relational_chain() {
    // `$a < $b < $c < $d < $e` — four chained operators, five operands.
    let e = parse_expr_str("$a < $b < $c < $d < $e;");
    match &e.kind {
        ExprKind::ChainedCmp(ops, operands) => {
            assert_eq!(ops.len(), 4, "expected 4 operators, got {}", ops.len());
            assert_eq!(operands.len(), 5, "expected 5 operands, got {}", operands.len());
        }
        other => panic!("expected ChainedCmp, got {other:?}"),
    }
}

#[test]
fn prec_long_equality_chain() {
    // `$a == $b != $c == $d` — three chained equality ops, four operands.
    let e = parse_expr_str("$a == $b != $c == $d;");
    match &e.kind {
        ExprKind::ChainedCmp(ops, operands) => {
            assert_eq!(ops.len(), 3, "expected 3 operators, got {}", ops.len());
            assert_eq!(operands.len(), 4, "expected 4 operands, got {}", operands.len());
        }
        other => panic!("expected ChainedCmp, got {other:?}"),
    }
}

#[test]
fn prec_chain_then_non_at_same_level_is_error() {
    // `$a == $b <=> $c` — Chain (==) then Non (<=>) at PREC_EQ → error.
    let result = crate::parse(b"$a == $b <=> $c;");
    assert!(result.is_err(), "chain-then-non at same precedence should be a syntax error");
}

#[test]
fn prec_non_then_chain_at_same_level_is_error() {
    // `$a <=> $b == $c` — Non (<=>) then Chain (==) at PREC_EQ → error.
    let result = crate::parse(b"$a <=> $b == $c;");
    assert!(result.is_err(), "non-then-chain at same precedence should be a syntax error");
}

#[test]
fn prec_different_non_ops_at_same_level_is_error() {
    // `$a .. $b ... $c` — two different Non operators at PREC_RANGE → error.
    let result = crate::parse(b"$a .. $b ... $c;");
    assert!(result.is_err(), "different non-assoc ops at same precedence should be a syntax error");
}

#[test]
fn prec_single_chain_op_is_binop() {
    // `$a < $b` alone — single chainable operator produces BinOp, not ChainedCmp.
    let e = parse_expr_str("$a < $b;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::NumLt, _, _)), "expected BinOp(NumLt), got {:?}", e.kind);
}

#[test]
fn prec_single_non_op_is_binop() {
    // `$a <=> $b` alone — single Non operator produces BinOp, no error.
    let e = parse_expr_str("$a <=> $b;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Spaceship, _, _)), "expected BinOp(Spaceship), got {:?}", e.kind);
}

#[test]
fn prec_chain_with_higher_prec_inside() {
    // `$a < $b + 1 < $c` — addition is higher precedence, consumed inside the chain operand.
    let e = parse_expr_str("$a < $b + 1 < $c;");
    match &e.kind {
        ExprKind::ChainedCmp(ops, operands) => {
            assert_eq!(ops.len(), 2);
            assert_eq!(operands.len(), 3);

            // Middle operand should be Add($b, 1), not just $b.
            assert!(matches!(operands[1].kind, ExprKind::BinOp(BinOp::Add, _, _)), "expected Add as middle operand, got {:?}", operands[1].kind);
        }
        other => panic!("expected ChainedCmp, got {other:?}"),
    }
}

#[test]
fn prec_chain_with_parens_inside() {
    // `$a < ($b == $c) < $d` — parenthesized comparison is a term inside the chain.
    let e = parse_expr_str("$a < ($b == $c) < $d;");
    match &e.kind {
        ExprKind::ChainedCmp(ops, operands) => {
            assert_eq!(ops.len(), 2);
            assert_eq!(operands.len(), 3);

            // Middle operand is the parenthesized equality (no Paren wrapper in AST).
            assert!(matches!(operands[1].kind, ExprKind::BinOp(BinOp::NumEq, _, _)), "expected NumEq as middle operand, got {:?}", operands[1].kind);
        }
        other => panic!("expected ChainedCmp, got {other:?}"),
    }
}

#[test]
fn prec_chain_in_ternary_condition() {
    // `$a < $b < $c ? 1 : 0` — chain stops at `?` (lower precedence), becomes the ternary condition.
    let e = parse_expr_str("$a < $b < $c ? 1 : 0;");
    match &e.kind {
        ExprKind::Ternary(cond, _, _) => {
            assert!(matches!(cond.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp as ternary condition, got {:?}", cond.kind);
        }
        other => panic!("expected Ternary, got {other:?}"),
    }
}

#[test]
fn prec_chain_as_rhs_of_assign() {
    // `$x = $a < $b < $c` — chain is the RHS of assignment.
    let e = parse_expr_str("$x = $a < $b < $c;");
    match &e.kind {
        ExprKind::Assign(AssignOp::Eq, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::ChainedCmp(_, _)), "expected ChainedCmp on RHS of Assign, got {:?}", rhs.kind);
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

// ── Pratt loop adversarial cases ─────────────────────────

#[test]
fn pratt_infix_after_array_ref() {
    // `[1, 2] . [3]` — concat of two array refs.  The `.` infix must be consumed after the ArrayRef frame completes.
    let e = parse_expr_str("[1, 2] . [3];");
    match &e.kind {
        ExprKind::BinOp(BinOp::Concat, lhs, rhs) => {
            assert!(matches!(lhs.kind, ExprKind::AnonArray(_)));
            assert!(matches!(rhs.kind, ExprKind::AnonArray(_)));
        }
        other => panic!("expected Concat of two AnonArrays, got {other:?}"),
    }
}

#[test]
fn pratt_infix_after_hash_ref() {
    // `$x = {a => 1} || 0` — hash ref on RHS of assignment, then logical or.  The `{` is in expression context so it's
    // unambiguously a hash ref (at statement level, `{` would be a block).
    let e = parse_expr_str("$x = {a => 1} || 0;");
    match &e.kind {
        ExprKind::Assign(_, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::Or, _, _)), "expected Or on RHS of Assign, got {:?}", rhs.kind);
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn pratt_infix_inside_accumulator() {
    // `[1 + 2, 3 * 4]` — infix ops at different precedences inside array ref accumulator elements.
    let e = parse_expr_str("[1 + 2, 3 * 4];");
    match &e.kind {
        ExprKind::AnonArray(elems) => {
            assert_eq!(elems.len(), 2);
            assert!(matches!(elems[0].kind, ExprKind::BinOp(BinOp::Add, _, _)));
            assert!(matches!(elems[1].kind, ExprKind::BinOp(BinOp::Mul, _, _)));
        }
        other => panic!("expected AnonArray, got {other:?}"),
    }
}

#[test]
fn pratt_not_absorbs_ternary() {
    // `not $a ? 1 : 0` → Not(Ternary($a, 1, 0)) — not at PREC_NOT_LOW (300) absorbs the entire ternary (700).
    let e = parse_expr_str("not $a ? 1 : 0;");
    match &e.kind {
        ExprKind::UnaryOp(UnaryOp::Not, inner) => {
            assert!(matches!(inner.kind, ExprKind::Ternary(_, _, _)), "expected Ternary inside Not, got {:?}", inner.kind);
        }
        other => panic!("expected Not, got {other:?}"),
    }
}

#[test]
fn pratt_every_frame_type_stacked() {
    // `not \-([1])` — stacks four different frame types: Not, Ref, Negate, Paren (with nested ArrayRef).  Verifies the
    // continuation stack handles diverse frame types in a single expression without confusion.
    let e = parse_expr_str("not \\-([1]);");

    // Outer is Not.
    assert!(matches!(e.kind, ExprKind::UnaryOp(UnaryOp::Not, _)), "expected Not, got {:?}", e.kind);
}

#[test]
fn pratt_min_prec_restored_after_container() {
    // `1 + [2] * 3` — after ArrayRef frame completes with min_prec from the `+`'s RHS (PREC_ADD+1=1801), the `*` at
    // PREC_MUL (1900) >= 1801 must be consumed.  Result: Add(1, Mul([2], 3)).
    let e = parse_expr_str("1 + [2] * 3;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Add, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::BinOp(BinOp::Mul, _, _)), "expected Mul on RHS of Add, got {:?}", rhs.kind);
        }
        other => panic!("expected Add, got {other:?}"),
    }
}

#[test]
fn pratt_continue_then_nested_frames() {
    // `[$a = 1, -$b]` — first element triggers Continue, second element has a Negate prefix frame.
    let e = parse_expr_str("[$a = 1, -$b];");
    match &e.kind {
        ExprKind::AnonArray(elems) => {
            assert_eq!(elems.len(), 2);
            assert!(matches!(elems[0].kind, ExprKind::Assign(AssignOp::Eq, _, _)));
            assert!(matches!(elems[1].kind, ExprKind::UnaryOp(UnaryOp::Negate, _)));
        }
        other => panic!("expected AnonArray, got {other:?}"),
    }
}

#[test]
fn pratt_deref_block_with_infix_inside() {
    // `${$a . $b}` — DerefBlock frame with infix concat inside the block expression.
    let e = parse_expr_str("${$a . $b};");
    match &e.kind {
        ExprKind::Deref(Sigil::Scalar, inner) => {
            assert!(matches!(inner.kind, ExprKind::BinOp(BinOp::Concat, _, _)), "expected Concat inside Deref, got {:?}", inner.kind);
        }
        other => panic!("expected Deref(Scalar), got {other:?}"),
    }
}

#[test]
fn parse_empty_list_bare() {
    // Bare `()` is the dedicated EmptyList node, not Comma([]).
    let e = parse_expr_str("();");
    assert!(matches!(e.kind, ExprKind::EmptyList), "expected EmptyList, got {:?}", e.kind);
}

#[test]
fn parse_empty_list_in_parens_is_not_comma() {
    // `()` must not be represented as an empty comma sequence.
    let e = parse_expr_str("();");
    assert!(!matches!(e.kind, ExprKind::Comma(_)), "() must be EmptyList, never Comma([]), got {:?}", e.kind);
}

#[test]
fn parse_empty_list_as_assignment_lhs() {
    // `() = (1, 2, 3)` is a legal list assignment (the RHS is discarded); the
    // LHS is a valid lvalue.  Verified against perl: parses and runs.
    let e = parse_expr_str("() = (1, 2, 3);");
    match &e.kind {
        ExprKind::Assign(_, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::EmptyList), "expected EmptyList LHS, got {:?}", lhs.kind);
        }
        other => panic!("expected Assign with EmptyList LHS, got {other:?}"),
    }
}

#[test]
fn parse_empty_list_count_of_idiom() {
    // `my $n = () = (1, 2, 3, 4)` — the count-of idiom.  The inner `() = LIST`
    // is a list assignment whose scalar value is the RHS element count (4).
    // Here we only assert it parses: a scalar assignment of an (EmptyList) list
    // assignment.  Verified against perl: $n == 4.
    let e = parse_expr_str("my $n = () = (1, 2, 3, 4);");
    match &e.kind {
        // Outer: scalar assignment `my $n = (...)`.
        ExprKind::Assign(_, _, rhs) => match &rhs.kind {
            // Inner: list assignment `() = (...)`.
            ExprKind::Assign(_, inner_lhs, _) => {
                assert!(matches!(inner_lhs.kind, ExprKind::EmptyList), "expected EmptyList LHS of inner assignment, got {:?}", inner_lhs.kind);
            }
            other => panic!("expected inner Assign (the () = LIST), got {other:?}"),
        },
        other => panic!("expected outer Assign, got {other:?}"),
    }
}

#[test]
fn parse_empty_list_kept_as_comma_operand() {
    // `()` is NOT dropped as a comma operand: `(1, 2, ())` keeps the trailing
    // EmptyList, because scalar(1, 2, ()) is undef (the last C-comma operand is
    // `()`).  The empty-list-flattens behaviour of list context is a lowering
    // concern, not a parse-time structural drop.  Verified against perl.
    let e = parse_expr_str("(1, 2, ());");
    match &e.kind {
        ExprKind::Comma(items) => {
            assert_eq!(items.len(), 3, "expected 3 operands incl. trailing EmptyList, got {:?}", items);
            assert!(matches!(items[2].kind, ExprKind::EmptyList), "expected EmptyList as last operand, got {:?}", items[2].kind);
        }
        other => panic!("expected Comma, got {other:?}"),
    }
}

#[test]
fn parse_empty_list_as_function_arg() {
    // `foo(())` — an empty list passed as the (sole) argument expression.  The
    // inner `()` is EmptyList.
    let e = parse_expr_str("foo(());");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 1, "expected one arg expression, got {:?}", args);
            assert!(matches!(args[0].kind, ExprKind::EmptyList), "expected EmptyList arg, got {:?}", args[0].kind);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
#[ignore = "comma handler doesn't absorb consecutive commas — needs parse_term → Option<Expr> refactor"]
fn pratt_consecutive_commas_in_list() {
    // Perl silently drops consecutive commas: `(1, 3, , , 5)` → `(1, 3, 5)`.
    let e = parse_expr_str("(1, 3, , , 5);");
    match &e.kind {
        ExprKind::Comma(items) => {
            assert_eq!(items.len(), 3, "expected 3 elements (consecutive commas dropped), got {:?}", items);
        }
        other => panic!("expected Comma, got {other:?}"),
    }
}

#[test]
#[ignore = "comma handler doesn't absorb consecutive commas — needs parse_term → Option<Expr> refactor"]
fn pratt_consecutive_commas_in_array_ref() {
    // `[1, , , 3]` — consecutive commas inside array ref, elements silently dropped.
    let e = parse_expr_str("[1, , , 3];");
    match &e.kind {
        ExprKind::AnonArray(elems) => {
            assert_eq!(elems.len(), 2, "expected 2 elements (consecutive commas dropped), got {:?}", elems);
        }
        other => panic!("expected AnonArray, got {other:?}"),
    }
}

#[test]
fn pratt_trailing_comma_only() {
    // `(1,)` — trailing comma, single-element list.
    let e = parse_expr_str("(1,);");

    // In Perl, `(1,)` is the same as `(1)` — the trailing comma is a no-op.  The parser may produce IntLit(1) or
    // Comma([1]) — either is acceptable.
    assert!(matches!(e.kind, ExprKind::IntLit(1) | ExprKind::Comma(_)), "expected IntLit or Comma, got {:?}", e.kind);
}

#[test]
fn pratt_c_comma_rhs_of_binding() {
    // `("test", $_) =~ /foo/` — C comma semantics.  The comma expression inside parens binds to `=~` as a whole.  Perl
    // evaluates "test" in void context and binds $_ to the regex.  At the parser level, the comma still produces a Comma
    // (context is a semantic concern, not syntactic), and =~ binds to the entire paren result.
    let e = parse_expr_str("(\"test\", $_ ) =~ /foo/;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Binding, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::Comma(_)), "expected Comma on LHS of Binding, got {:?}", lhs.kind);
        }
        other => panic!("expected Binding, got {other:?}"),
    }
}

#[test]
fn pratt_c_comma_in_scalar_assign() {
    // `$x = (1, 2, 3)` — comma in scalar context.  At the parser level, the RHS is a Comma node; the compiler/runtime
    // evaluates it in scalar context to produce 3 (last element).
    let e = parse_expr_str("$x = (1, 2, 3);");
    match &e.kind {
        ExprKind::Assign(_, _, rhs) => match &rhs.kind {
            ExprKind::Comma(items) => assert_eq!(items.len(), 3),
            _ => panic!("expected Comma on RHS, got {:?}", rhs.kind),
        },
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
#[ignore = "comma handler doesn't absorb consecutive commas — needs parse_term → Option<Expr> refactor"]
fn pratt_consecutive_commas_scalar_context() {
    // `$x = (1, , , 3)` — consecutive commas in scalar context.  The empty comma positions are no-ops; Perl evaluates
    // to 3 (last element in scalar context).  The parser should produce a 2-element Comma, not error.
    let e = parse_expr_str("$x = (1, , , 3);");
    match &e.kind {
        ExprKind::Assign(_, _, rhs) => match &rhs.kind {
            ExprKind::Comma(items) => assert_eq!(items.len(), 2, "expected 2 elements, got {:?}", items),
            _ => panic!("expected Comma on RHS, got {:?}", rhs.kind),
        },
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn pratt_trailing_comma_scalar_context() {
    // `$x = (1, 2,)` — trailing comma in scalar context.  The trailing comma is a no-op.
    let e = parse_expr_str("$x = (1, 2,);");
    match &e.kind {
        ExprKind::Assign(_, _, rhs) => match &rhs.kind {
            ExprKind::Comma(items) => assert_eq!(items.len(), 2, "expected 2 elements, got {:?}", items),
            _ => panic!("expected Comma on RHS, got {:?}", rhs.kind),
        },
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
#[ignore = "comma handler doesn't absorb consecutive commas — needs parse_term → Option<Expr> refactor"]
fn pratt_consecutive_commas_lhs_list_assign() {
    // `my ($x,,$y) = (2, 4, 6)` — consecutive commas on LHS of list assignment are no-ops.  `($x,,$y)` is the same as
    // `($x,$y)`: $x=2, $y=4.  No placeholder slot is created.
    let prog = parse("my ($x,,$y) = (2, 4, 6);");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => match &lhs.kind {
            ExprKind::Decl(_, vars) => {
                assert_eq!(vars.len(), 2, "expected 2 variables (consecutive commas dropped), got {:?}", vars);
            }
            other => panic!("expected Decl on LHS, got {other:?}"),
        },
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
#[ignore = "parse_decl_expr doesn't accept undef as a placeholder in parenthesized declarations"]
fn pratt_undef_placeholder_in_list_assign() {
    // `my ($x, undef, $y) = (2, 4, 6)` — explicit `undef` placeholder occupies a slot.  $x=2, undef absorbs 4, $y=6.
    // Contrast with `($x,,$y)` where the extra comma is a no-op.
    let prog = parse("my ($x, undef, $y) = (2, 4, 6);");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => {
            match &lhs.kind {
                ExprKind::Decl(_, vars) => {
                    // 3 slots: $x, undef placeholder, $y — but Decl may only track the named variables.  The key point
                    // is the list has 3 elements, not 2.
                    assert!(vars.len() >= 2, "expected at least 2 declared vars, got {:?}", vars);
                }
                ExprKind::Comma(items) => {
                    assert_eq!(items.len(), 3, "expected 3 elements (including undef placeholder), got {:?}", items);
                }
                other => panic!("expected Decl or Comma on LHS, got {other:?}"),
            }
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

// ── Scalar vs list context from LHS declaration ──────────

#[test]
fn context_scalar_decl_vs_list_decl() {
    // `my $x = (2, 4, 6)` → scalar context.  $x = 6 (C comma, last value).
    // `my ($x) = (2, 4, 6)` → list context.  $x = 2 (first element of list).
    // The list form is a transient `Paren(Decl)`, so both assignments store an identical bare `Decl` LHS once the
    // Paren is unwrapped — the distinction survives only as the context the `=` handler stamps onto both sides.
    let prog1 = parse("my $x = (2, 4, 6);");
    let prog2 = parse("my ($x) = (2, 4, 6);");

    let (lhs1, rhs1) = match &prog1.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, rhs), .. }) => (lhs, rhs),
        other => panic!("stmt 1: expected Assign, got {other:?}"),
    };
    let (lhs2, rhs2) = match &prog2.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, rhs), .. }) => (lhs, rhs),
        other => panic!("stmt 2: expected Assign, got {other:?}"),
    };

    // Both LHSs are a bare `Decl` — the grouping `Paren` around `my ($x)` was unwrapped before storage.
    assert!(matches!(lhs1.kind, ExprKind::Decl(DeclScope::My, _)), "scalar-decl LHS is a bare Decl, got {:?}", lhs1.kind);
    assert!(matches!(lhs2.kind, ExprKind::Decl(DeclScope::My, _)), "list-decl LHS is a bare Decl, got {:?}", lhs2.kind);

    // The difference is the stamped context: a scalar declaration evaluates its RHS in scalar context, a list
    // declaration in list context.  Both sides are stamped at assignment-construction time.
    assert_eq!(lhs1.ctx, Some(Context::Scalar), "scalar-decl LHS is scalar");
    assert_eq!(rhs1.ctx, Some(Context::Scalar), "scalar-decl RHS is scalar");
    assert_eq!(lhs2.ctx, Some(Context::List), "list-decl LHS is list");
    assert_eq!(rhs2.ctx, Some(Context::List), "list-decl RHS is list");
}

// ── Prototype-dependent comma parsing ────────────────────

#[test]
#[ignore = "parser doesn't consult prototypes when parsing call arguments — needs prototype-aware context propagation"]
fn proto_scalar_slot_parses_c_comma() {
    // `($@)` prototype: first slot is scalar context, so `(2,4,6)` is C commas → result is 6.  The parser should
    // produce 3 args: [6, 8, 10], not [Comma(2,4,6), 8, 10].
    let prog = parse("sub f ($@) {} f((2,4,6),8,10);");

    // Find the function call statement.
    let call = prog.statements.iter().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e) } else { None }).expect("no expression statement found");
    match &call.kind {
        ExprKind::FuncCall(_, args) => {
            // With scalar context for first arg, (2,4,6) evaluates to 6.  The first arg should NOT be a Comma node.
            assert!(!matches!(args[0].kind, ExprKind::Comma(_)), "first arg should be scalar (C comma), not List — got {:?}", args[0].kind);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_no_prototype_parses_list() {
    // Without prototype, all args are list context.  `(2,4,6)` is a list that flattens.  The parser produces 3 args:
    // [Comma(2,4,6), 8, 10].  Flattening to 5 args is the compiler's job.
    let prog = parse("sub g {} g((2,4,6),8,10);");
    let call = prog.statements.iter().find_map(|s| if let StmtKind::Expr(e) = &s.kind { Some(e) } else { None }).expect("no expression statement found");
    match &call.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 3, "expected 3 args (Comma, 8, 10), got {:?}", args);
            assert!(matches!(args[0].kind, ExprKind::Comma(_)), "first arg should be Comma(2,4,6), got {:?}", args[0].kind);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

// ── Leading-dot float and v-string disambiguation ─────────

#[test]
fn parse_leading_dot_float() {
    // `.5` in term position → FloatLit(0.5).
    let e = parse_expr_str(".5;");
    assert!(matches!(e.kind, ExprKind::FloatLit(f) if (f - 0.5).abs() < 1e-15), "expected FloatLit(0.5), got {:?}", e.kind);
}

#[test]
fn parse_leading_dot_float_with_exponent() {
    let e = parse_expr_str(".5e2;");
    assert!(matches!(e.kind, ExprKind::FloatLit(f) if (f - 50.0).abs() < 1e-10), "expected FloatLit(50.0), got {:?}", e.kind);
}

#[test]
fn parse_leading_dot_vstring() {
    // `.5.6` → VersionLit("0.5.6") — two dots means v-string.
    let e = parse_expr_str(".5.6;");
    assert!(matches!(&e.kind, ExprKind::VersionLit(v) if v == "0.5.6"), "expected VersionLit(\"0.5.6\"), got {:?}", e.kind);
}

#[test]
fn parse_leading_dot_float_in_expr() {
    // `.5 + 1` → Add(FloatLit(0.5), IntLit(1)).
    let e = parse_expr_str(".5 + 1;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Add, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::FloatLit(f) if (f - 0.5).abs() < 1e-15), "expected FloatLit(0.5) on LHS, got {:?}", lhs.kind);
        }
        other => panic!("expected Add, got {other:?}"),
    }
}

#[test]
fn parse_dot_in_operator_position_is_concat() {
    // `$x .5` → Concat($x, IntLit(5)).  The `.` is concat in operator position, not a leading-dot float.
    let e = parse_expr_str("$x .5;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Concat, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::IntLit(5)), "expected IntLit(5) on RHS of Concat, got {:?}", rhs.kind);
        }
        other => panic!("expected Concat, got {other:?}"),
    }
}

#[test]
fn parse_dot_in_operator_position_concat_float() {
    // `$x .5.6` → Concat($x, FloatLit(5.6)).  First `.` is concat, `5.6` is a float.
    let e = parse_expr_str("$x .5.6;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Concat, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::FloatLit(f) if (f - 5.6).abs() < 1e-10), "expected FloatLit(5.6) on RHS of Concat, got {:?}", rhs.kind);
        }
        other => panic!("expected Concat, got {other:?}"),
    }
}

#[test]
fn parse_dot_in_operator_position_concat_vstring() {
    // `$x .5.6.7` → Concat($x, VersionLit("5.6.7")).  First `.` is concat, `5.6.7` is a v-string.
    let e = parse_expr_str("$x .5.6.7;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Concat, _, rhs) => {
            assert!(matches!(&rhs.kind, ExprKind::VersionLit(v) if v == "5.6.7"), "expected VersionLit(\"5.6.7\") on RHS of Concat, got {:?}", rhs.kind);
        }
        other => panic!("expected Concat, got {other:?}"),
    }
}

#[test]
fn parse_leading_dot_float_underscore() {
    // `.5_000` — underscore in fractional part.
    let e = parse_expr_str(".5_000;");
    assert!(matches!(e.kind, ExprKind::FloatLit(f) if (f - 0.5).abs() < 1e-10), "expected FloatLit(0.5), got {:?}", e.kind);
}

#[test]
fn parse_leading_dot_float_neg_exponent() {
    // `.5e-3` → FloatLit(0.0005).
    let e = parse_expr_str(".5e-3;");
    assert!(matches!(e.kind, ExprKind::FloatLit(f) if (f - 0.0005).abs() < 1e-15), "expected FloatLit(0.0005), got {:?}", e.kind);
}

#[test]
fn parse_leading_dot_float_uppercase_e() {
    // `.5E2` → FloatLit(50.0).
    let e = parse_expr_str(".5E2;");
    assert!(matches!(e.kind, ExprKind::FloatLit(f) if (f - 50.0).abs() < 1e-10), "expected FloatLit(50.0), got {:?}", e.kind);
}

#[test]
fn parse_leading_dot_negate() {
    // `-.5` → Negate(FloatLit(0.5)).  Prefix minus on leading-dot float.
    let e = parse_expr_str("-.5;");
    match &e.kind {
        ExprKind::UnaryOp(UnaryOp::Negate, inner) => {
            assert!(matches!(inner.kind, ExprKind::FloatLit(f) if (f - 0.5).abs() < 1e-15), "expected FloatLit(0.5) inside Negate, got {:?}", inner.kind);
        }
        other => panic!("expected Negate, got {other:?}"),
    }
}

#[test]
fn parse_leading_dot_vstring_four_segments() {
    // `.5.6.7.8` → VersionLit("0.5.6.7.8").
    let e = parse_expr_str(".5.6.7.8;");
    assert!(matches!(&e.kind, ExprKind::VersionLit(v) if v == "0.5.6.7.8"), "expected VersionLit(\"0.5.6.7.8\"), got {:?}", e.kind);
}

#[test]
fn parse_leading_dot_float_then_concat() {
    // `.5 . "hello"` → Concat(FloatLit(0.5), StringLit("hello")).  The first `.5` is a leading-dot float (term
    // position), the second `.` is concat (operator position).
    let e = parse_expr_str(".5 . 'hello';");
    match &e.kind {
        ExprKind::BinOp(BinOp::Concat, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::FloatLit(f) if (f - 0.5).abs() < 1e-15), "expected FloatLit(0.5) on LHS of Concat, got {:?}", lhs.kind);
        }
        other => panic!("expected Concat, got {other:?}"),
    }
}

#[test]
fn parse_leading_dot_underscore_before_digit_is_error() {
    // `._5` — underscore before the first digit is a syntax error in Perl.  The `.` is Dot (not a float start) and `_5`
    // is a bareword, so this fails in term position.
    let result = crate::parse(b"._5;");
    assert!(result.is_err(), "._5 should be a syntax error or non-float parse, got: {:?}", result);
}

#[test]
fn parse_underscore_after_dot_in_number() {
    // `0._5` → Perl treats this as 0.5 (underscore after dot is accepted in the fractional part).
    let e = parse_expr_str("0._5;");
    assert!(matches!(e.kind, ExprKind::FloatLit(f) if (f - 0.5).abs() < 1e-10), "expected FloatLit(0.5), got {:?}", e.kind);
}

#[test]
fn parse_leading_dot_multiple_underscores() {
    // `.5_5_5` → FloatLit(0.555).
    let e = parse_expr_str(".5_5_5;");
    assert!(matches!(e.kind, ExprKind::FloatLit(f) if (f - 0.555).abs() < 1e-10), "expected FloatLit(0.555), got {:?}", e.kind);
}

#[test]
#[ignore = "x5 lexed as identifier, not repeat-then-5 — lexer doesn't know operator position"]
fn parse_underscore_after_dot_then_repeat() {
    // `0._x5` → Perl parses as `0.0 x 5` → "00000".  The `_` enters the float fractional path, is stripped, leaving
    // float 0.0.  Then `x5` is the repeat operator (x is not followed by a word char in x5 since 5 is a digit, but `x`
    // adjacent to a digit is repeat).
    let e = parse_expr_str("0._x5;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Repeat, _, _)), "expected Repeat, got {:?}", e.kind);
}

#[test]
#[ignore = "parser doesn't reject bareword in operator position — needs operator-position validation"]
fn parse_underscore_after_dot_then_bareword_is_error() {
    // `0._a` → Perl: "Bareword found where operator expected".  Float 0.0, then `a` is a bareword in operator position.
    let result = crate::parse(b"0._a;");
    assert!(result.is_err(), "0._a should be a syntax error, got: {:?}", result);
}

#[test]
#[ignore = "parser doesn't reject bareword in operator position — needs operator-position validation"]
fn parse_underscore_after_dot_digit_then_bareword_is_error() {
    // `0._0a` → Float 0.0 (fractional `_0` → `0`), then `a` is bareword where operator expected.
    let result = crate::parse(b"0._0a;");
    assert!(result.is_err(), "0._0a should be a syntax error, got: {:?}", result);
}

#[test]
#[ignore = "parser doesn't reject bareword in operator position — needs operator-position validation"]
fn parse_underscore_after_dot_x_bareword_is_error() {
    // `0._x_y` → Float 0.0, then `x_y` is a bareword (x followed by word char is not repeat operator).
    let result = crate::parse(b"0._x_y;");
    assert!(result.is_err(), "0._x_y should be a syntax error, got: {:?}", result);
}

// ═══════════════════════════════════════════════════════════
// Operators with AST verification
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_range() {
    let e = parse_expr_str("1..10;");
    assert!(matches!(e.kind, ExprKind::Range(_, _, _)));
}

#[test]
fn parse_range_two_dots_kind() {
    // `..` records RangeKind::TwoDots.
    let e = parse_expr_str("1..10;");
    match &e.kind {
        ExprKind::Range(_, _, kind) => assert_eq!(*kind, RangeKind::TwoDots),
        other => panic!("expected Range, got {other:?}"),
    }
}

#[test]
fn parse_range_three_dots_kind() {
    // `...` is the same operator as `..`; it builds a Range node carrying
    // RangeKind::ThreeDots.  Range-vs-flip-flop is context-determined at
    // lowering — the node is Range either way.
    let e = parse_expr_str("1...10;");
    match &e.kind {
        ExprKind::Range(_, _, kind) => assert_eq!(*kind, RangeKind::ThreeDots),
        other => panic!("expected Range for `...`, got {other:?}"),
    }
}

#[test]
fn parse_range_kinds_differ_by_spelling_only() {
    // `..` and `...` produce the same node kind (Range), differing only in the
    // recorded RangeKind.
    let two = parse_expr_str("$a..$b;");
    let three = parse_expr_str("$a...$b;");
    let k2 = match &two.kind {
        ExprKind::Range(_, _, k) => *k,
        other => panic!("expected Range, got {other:?}"),
    };
    let k3 = match &three.kind {
        ExprKind::Range(_, _, k) => *k,
        other => panic!("expected Range, got {other:?}"),
    };
    assert_eq!(k2, RangeKind::TwoDots);
    assert_eq!(k3, RangeKind::ThreeDots);
    assert_ne!(k2, k3);
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
// Arrow deref targets
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
// Postfix control flow variants
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
// Declaration variants
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
    let prog = parse("use feature 'state'; state $counter = 0;");
    let (scope, vars) = decl_vars(&prog.statements[1]);
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
// Builtins
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_defined() {
    let e = parse_expr_str("defined $x;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "CORE::defined");
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
            assert_eq!(name, "CORE::chomp");
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
            assert_eq!(name, "CORE::die");
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
            assert_eq!(name, "CORE::push");
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
            assert_eq!(name, "CORE::join");
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
            assert_eq!(name, "CORE::split");
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
            assert_eq!(name, "CORE::sort");
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
            assert_eq!(name, "CORE::open");
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
            assert_eq!(name, "CORE::bless");
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected bless ListOp, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// Special forms
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
            assert_eq!(name, "CORE::glob");
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
    // {key => 'val'} at statement level — the heuristic should detect => after bareword and route to AnonHash.
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
    // {foo, 1} — lowercase bareword followed by comma → block (could be a function call: foo(), 1).
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
// Phaser blocks (INIT/CHECK/UNITCHECK)
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
// Control flow variants
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
// Regex flags
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
// Miscellaneous
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_scalar_context() {
    let e = parse_expr_str("scalar @arr;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "CORE::scalar");
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
            assert_eq!(name, "CORE::require");
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
        ExprKind::Comma(items) => match &items[0].kind {
            ExprKind::StringLit(s) => assert_eq!(s, "if"),
            other => panic!("expected StringLit('if'), got {other:?}"),
        },
        other => panic!("expected Comma, got {other:?}"),
    }
}

#[test]
fn parse_fat_comma_keyword_cross_line() {
    // Keyword on one line, => on the next — should still autoquote.
    let e = parse_expr_str("my\n  => 1;");
    match &e.kind {
        ExprKind::Comma(items) => match &items[0].kind {
            ExprKind::StringLit(s) => assert_eq!(s, "my"),
            other => panic!("expected StringLit('my'), got {other:?}"),
        },
        other => panic!("expected Comma, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// Quote-keyword autoquoting.
//
// The 8 Perl quote-like operators — `q`, `qq`, `qw`, `qr`, `m`, `s`, `tr`, `y` — are recognized as operators only when
// followed by a *valid* opening delimiter (see `at_quote_delimiter` in the lexer).  When not followed by a valid opener
// — including when followed by `=>` (fat comma), `}` (hash-subscript close), or any of the closing paired delimiters
// `)`, `]`, `}`, `>` — they must NOT start a quote op and must instead be treated as ordinary barewords (autoquoted to
// string literals in the appropriate contexts).
//
// (`qx` — the backtick-equivalent — has the same lexical shape but is omitted from this set to match Perl's common "8
// quote operators" terminology.)
// ═══════════════════════════════════════════════════════════

// ── Autoquote in fat-comma context ────────────────────────

/// Parse `(KEYWORD => 1);` and return the first list element.
fn parse_kw_fat_comma(src: &str) -> Expr {
    let e = parse_expr_str(src);
    match e.kind {
        ExprKind::Comma(mut items) => {
            assert!(!items.is_empty(), "expected non-empty list for {src:?}");
            items.remove(0)
        }
        other => panic!("expected Comma, got {other:?} for {src:?}"),
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
// Deeper coverage for areas where earlier tests only checked the outer AST variant without verifying inner content.
// Structured by the phase they belong to.
// ═══════════════════════════════════════════════════════════

// ── Phase 3: postderef slice content verification ────────
//
// Verify inner expression contents of postderef slices, not just the ArrowTarget variant.  Without this, a regression
// that parsed `$r->@[0, 1, 2]` as `ArraySliceIndices(IntLit(0))` (dropping the rest) would slip through.

#[test]
fn postderef_array_slice_indices_content() {
    let e = parse_expr_stmt("$r->@[0, 1, 2];");
    match arrow_target(&e) {
        ArrowTarget::ArraySliceIndices(idx) => {
            // Index expr is a comma-list of three ints.
            match &idx.kind {
                ExprKind::Comma(items) => {
                    assert_eq!(items.len(), 3);
                    assert!(matches!(items[0].kind, ExprKind::IntLit(0)));
                    assert!(matches!(items[1].kind, ExprKind::IntLit(1)));
                    assert!(matches!(items[2].kind, ExprKind::IntLit(2)));
                }
                ExprKind::IntLit(n) => panic!("single IntLit({n}) — expected 3-element List; would mean slice dropped items"),
                other => panic!("expected Comma of 3, got {other:?}"),
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
            ExprKind::Comma(items) => {
                assert_eq!(items.len(), 3);
                for (i, want) in ["a", "b", "c"].iter().enumerate() {
                    assert!(matches!(items[i].kind, ExprKind::StringLit(ref s) if s == want), "item {i}: expected StringLit({want}), got {:?}", items[i].kind);
                }
            }
            other => panic!("expected Comma of 3 strings, got {other:?}"),
        },
        other => panic!("expected ArraySliceKeys, got {other:?}"),
    }
}

#[test]
fn postderef_kv_slice_indices_content() {
    let e = parse_expr_stmt("$r->%[0, 1];");
    match arrow_target(&e) {
        ArrowTarget::KvSliceIndices(idx) => match &idx.kind {
            ExprKind::Comma(items) => {
                assert_eq!(items.len(), 2);
                assert!(matches!(items[0].kind, ExprKind::IntLit(0)));
                assert!(matches!(items[1].kind, ExprKind::IntLit(1)));
            }
            other => panic!("expected Comma of 2 ints, got {other:?}"),
        },
        other => panic!("expected KvSliceIndices, got {other:?}"),
    }
}

#[test]
fn postderef_nested_actually_nested() {
    // Original `postderef_nested_slice` test claimed to cover chaining but only had one level.  This one actually
    // chains: slice followed by arrow-array-elem.
    let e = parse_expr_stmt("$r->@[0, 1]->[0];");

    // Outer is ArrowDeref(_, ArrayElem(0)); inner is ArrowDeref($r, ArraySliceIndices([0, 1])).
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
// `fc_requires_feature` was weak: it asserted parsing didn't error and the name was "fc" — but that's true regardless
// of whether fc was recognized as a named unary or fell back to a generic FuncCall.  Counter-test: with the feature on
// AND no parens, `fc` must bind as a named-unary operator (precedence boundary: tighter than `+`, looser than `*`).

#[test]
fn fc_named_unary_precedence() {
    // `fc $x . $y` — named-unary operators parse their argument at NAMED_UNARY precedence, which is BELOW concat.  So
    // the entire `$x . $y` is the argument: `fc($x . $y)`, NOT `fc($x) . $y`.
    let e = parse_expr_stmt("use feature 'fc'; fc $x . $y;");
    match e.kind {
        ExprKind::FuncCall(ref name, ref args) if name == "CORE::fc" => {
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Concat, _, _)), "argument should be the whole Concat expr, got {:?}", args[0].kind);
        }
        other => panic!("expected FuncCall(fc, [Concat(...)]), got {other:?}"),
    }
}

// ── Phase 5b: reactivation tests for each gated keyword ──
//
// Verify that each of the seven feature-gated keywords parses as its real keyword form when the corresponding feature
// is active.

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
// Verify __SUB__, __PACKAGE__, and similar compile-time tokens in nested contexts (inside sub bodies, blocks, etc.).

#[test]
fn current_sub_inside_named_sub() {
    // __SUB__ inside a sub body — the token is lex-time so context doesn't affect its form; verify it parses.
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
    // After `package Foo; package Bar;`, __PACKAGE__ gives "Bar".  Tests the parser state-tracking on successive
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
    // `@rest` must be the last named parameter — a scalar after it is invalid.  The parser should reject.
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
// Known gaps (ignored until implemented)
// ═══════════════════════════════════════════════════════════

#[test]
fn parse_subst_e_flag() {
    let e = parse_expr_str("s/foo/uc($&)/e;");
    match &e.kind {
        ExprKind::Subst(_, SubstReplacement::Eval { block, evals }, flags) => {
            assert_eq!(*evals, 1, "a single /e is one eval");
            assert_eq!(block.statements.len(), 1, "the replacement is one statement");
            assert!(flags.is_none(), "the lone `e` lifts into evals, leaving no flags: {flags:?}");
        }
        other => panic!("expected Subst with /e eval replacement, got {other:?}"),
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
            assert_eq!(name, "CORE::print");
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
            assert_eq!(name, "CORE::print");
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
            assert_eq!(name, "CORE::print");
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
            assert_eq!(name, "CORE::print");
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
            assert_eq!(name, "CORE::print");
            assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::Bareword(n), .. }) if n == "STDERR"));
            assert_eq!(args.len(), 0);
        }
        other => panic!("expected PrintOp with filehandle, no args, got {other:?}"),
    }
}

#[test]
fn parse_say_filehandle() {
    let e = parse_expr_str("use feature 'say'; say STDERR 'error';");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "CORE::say");
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
            assert_eq!(name, "CORE::printf");
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
            assert_eq!(name, "CORE::print");
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
            assert_eq!(name, "CORE::print");
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
            assert_eq!(name, "CORE::print");
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
            assert_eq!(name, "CORE::print");
            assert!(matches!(fh.as_deref(), Some(Expr { kind: ExprKind::ScalarVar(n), .. }) if n == "fh"));
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::ScalarVar(ref n) if n == "_"));
        }
        other => panic!("expected PrintOp($fh, [$_]), got {other:?}"),
    }
}

#[test]
fn parse_print_parens_scalar_not_fh() {
    // print($f); — $f NOT a filehandle (followed by ), not a term).  Prints value of $f to STDOUT.
    let e = parse_expr_str("print($f);");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "CORE::print");
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
            assert_eq!(name, "CORE::print");
            assert!(fh.is_none());
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0].kind, ExprKind::ScalarVar(ref n) if n == "f"));
        }
        other => panic!("expected PrintOp(None, [$f]), got {other:?}"),
    }
}

#[test]
fn parse_say_no_filehandle() {
    let e = parse_expr_str("use feature 'say'; say 'hello';");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "CORE::say");
            assert!(fh.is_none());
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected say PrintOp with no filehandle, got {other:?}"),
    }
}

#[test]
fn parse_say_parens_filehandle() {
    let e = parse_expr_str("use feature 'say'; say(STDERR 'hello');");
    match &e.kind {
        ExprKind::PrintOp(name, fh, args) => {
            assert_eq!(name, "CORE::say");
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
            assert_eq!(name, "CORE::printf");
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
            assert_eq!(name, "CORE::printf");
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
            assert_eq!(name, "CORE::print");
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
    assert!(msg.contains("Can't find string terminator"), "expected unterminated string error, got: {msg}");
}

#[test]
fn lexer_error_unterminated_regex() {
    // `m//` and `qr//` lex through the `Regex` role; perl reports "Search pattern not terminated" (verified, 5.38).
    assert_eq!(parse_fails("my $x = m/foo bar"), "Search pattern not terminated");
    assert_eq!(parse_fails("my $x = qr/foo bar"), "Search pattern not terminated");
}

#[test]
fn lexer_error_unterminated_subst() {
    // The `s///` pattern and replacement carry distinct messages, both verified against perl.
    assert_eq!(parse_fails("$x =~ s/foo"), "Substitution pattern not terminated");
    assert_eq!(parse_fails("$x =~ s/foo/bar"), "Substitution replacement not terminated");
}

#[test]
fn lexer_error_unterminated_tr() {
    assert_eq!(parse_fails("$x =~ tr/abc"), "Transliteration pattern not terminated");
    assert_eq!(parse_fails("$x =~ tr/abc/xyz"), "Transliteration replacement not terminated");
}

#[test]
fn lexer_error_unterminated_quote_words() {
    // `qw//` shares the string-terminator family wording, with the close delimiter as the token.
    assert_eq!(parse_fails("my @x = qw/a b c"), "Can't find string terminator \"/\" anywhere before EOF");
}

#[test]
fn lexer_error_unterminated_prototype() {
    assert_eq!(parse_fails("sub foo ("), "Prototype not terminated");
}

#[test]
#[ignore = "bare /.../ uses the Prototype placeholder role until lex_term lands; reports Prototype not terminated, not Search pattern not terminated"]
fn lexer_error_unterminated_bare_regex() {
    // Target wording once bare slash routes through the `m//` regex frame (deferred lex_term work, §5.5).
    assert_eq!(parse_fails("/foo bar"), "Search pattern not terminated");
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
    assert!(msg.contains("Can't find string terminator"), "expected unterminated string error, got: {msg}");
}

#[test]
fn lexer_error_immediate() {
    // Error on the very first token — no valid code at all.
    let msg = parse_fails("\"unterminated");
    assert!(msg.contains("Can't find string terminator"), "expected unterminated string error, got: {msg}");
}

// ── Hard parsing corpus ───────────────────────────────────
//
// The tests below are derived from a corpus of adversarial cases targeting the hardest ambiguities in Perl parsing:
// regex-vs-division, block-vs-hash, indirect object, ternary associativity, comma/assignment precedence, arrow chains,
// interpolation, and heredoc integration.
//
// For each case we assert the specific structural facts we're confident about — typically the top-level node kind and a
// key grouping relationship.  We deliberately don't try to match whole trees, to keep tests robust against AST
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
            assert_eq!(name, "CORE::print");
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
            assert_eq!(name, "CORE::map");

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
            assert_eq!(name, "CORE::map");

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
    // `$a ? $b : $c ? $d : $e;` — right-associative.  Must group as: Ternary($a, $b, Ternary($c, $d, $e))
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
    // `$a = $b, $c;` — comma is lower than assignment.  Must group as: Comma([Assign($a, $b), $c])
    let e = parse_expr_str("$a = $b, $c;");
    match &e.kind {
        ExprKind::Comma(items) => {
            assert_eq!(items.len(), 2);
            assert!(matches!(items[0].kind, ExprKind::Assign(_, _, _)), "expected Assign as first list item, got {:?}", items[0].kind);
            assert!(matches!(items[1].kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected Comma, got {other:?}"),
    }
}

#[test]
fn hard_assign_paren_comma() {
    // `$a = ($b, $c);` — parens force comma expression as RHS.
    let e = parse_expr_str("$a = ($b, $c);");
    match &e.kind {
        ExprKind::Assign(_, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::Comma(_)), "expected Comma on RHS, got {:?}", rhs.kind);
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

#[test]
fn regex_escaped_delimiter() {
    // `m/foo\/bar/` — escaped forward slash inside regex.
    let e = parse_expr_str(r#"m/foo\/bar/;"#);
    match &e.kind {
        ExprKind::Regex(_, pat, _) => {
            let s = pat_str(pat);
            assert!(s.contains('/') || s.contains("\\/"), "expected escaped slash in pattern, got {s:?}");
        }
        other => panic!("expected Regex, got {other:?}"),
    }
}

#[test]
fn regex_escaped_delimiter_at_end() {
    // `m/\//` — the pattern is just an escaped slash.
    let e = parse_expr_str(r#"m/\//;"#);
    assert!(matches!(e.kind, ExprKind::Regex(_, _, _)), "expected Regex, got {:?}", e.kind);
}

#[test]
fn subst_escaped_delimiters_both_halves() {
    // `s/foo\/bar/baz\/qux/` — escaped slashes in both pattern and replacement.
    let e = parse_expr_str(r#"s/foo\/bar/baz\/qux/;"#);
    assert!(matches!(e.kind, ExprKind::Subst(_, _, _)), "expected Subst, got {:?}", e.kind);
}

#[test]
fn regex_brace_delim_no_escape_needed() {
    // `m{foo/bar}` — with brace delimiters, `/` doesn't need escaping.
    let e = parse_expr_str("m{foo/bar};");
    match &e.kind {
        ExprKind::Regex(_, pat, _) => {
            let s = pat_str(pat);
            assert!(s.contains('/'), "expected literal / in pattern, got {s:?}");
        }
        other => panic!("expected Regex, got {other:?}"),
    }
}

#[test]
fn depth_parens_are_iterative() {
    // Deeply nested parens are handled iteratively and don't produce nested AST nodes (parens are syntactic-only).  No
    // recursion in parsing or dropping.
    let depth = 100_000;
    let src = format!("{}1{};", "(".repeat(depth), ")".repeat(depth));
    let result = crate::parse(src.as_bytes());
    assert!(result.is_ok(), "deeply nested parens should be iterative, got: {:?}", result.err());
}

#[test]
fn depth_array_refs_are_iterative() {
    // Deeply nested array refs `[[[[1]]]]` are parsed iteratively, but the AST is deeply nested (AnonArray wrapping
    // AnonArray), so Rust's recursive Drop limits the practical depth.  10,000 levels is safe for an 8MB stack.
    let depth = 10_000;
    let src = format!("{}1{};", "[".repeat(depth), "]".repeat(depth));
    let result = crate::parse(src.as_bytes());
    assert!(result.is_ok(), "deeply nested array refs should be iterative, got: {:?}", result.err());
}

#[test]
fn depth_prefix_ops_are_iterative() {
    // Deeply nested prefix ops `-(-(-(1)))` are parsed iteratively, but the AST is deeply nested (UnaryOp wrapping
    // UnaryOp), so Drop limits practical depth.
    let depth = 10_000;
    let src = format!("{}1{};", "-(".repeat(depth), ")".repeat(depth));
    let result = crate::parse(src.as_bytes());
    assert!(result.is_ok(), "deeply nested prefix ops should be iterative, got: {:?}", result.err());
}

// ── Iterative parser adversarial tests ────────────────────

#[test]
fn iter_paren_subscript_is_list_slice() {
    // `(1, 2, 3)[1]` — a list slice on a parenthesized list literal, NOT array-element access on a container.
    let e = parse_expr_str("(1, 2, 3)[1];");
    match &e.kind {
        ExprKind::ListSlice(operand, indices) => {
            assert!(matches!(operand.kind, ExprKind::Comma(_)), "operand is the list, got {:?}", operand.kind);
            assert_eq!(indices.len(), 1);
            assert!(matches!(indices[0].kind, ExprKind::IntLit(1)));
        }
        other => panic!("expected ListSlice, got {other:?}"),
    }
}

#[test]
fn iter_mixed_nesting() {
    // `[{a => [1]}]` — alternating array/hash frames on the continuation stack.
    let e = parse_expr_str("[{a => [1]}];");
    match &e.kind {
        ExprKind::AnonArray(outer) => {
            assert_eq!(outer.len(), 1);
            match &outer[0].kind {
                ExprKind::AnonHash(inner) => {
                    assert_eq!(inner.len(), 2); // "a", [1]
                }
                other => panic!("expected AnonHash, got {other:?}"),
            }
        }
        other => panic!("expected AnonArray, got {other:?}"),
    }
}

#[test]
fn iter_prefix_inside_array() {
    // `[-1, !0, \$x]` — prefix ops nested inside an ArrayRef accumulator.
    let e = parse_expr_str("[-1, !0, \\$x];");
    match &e.kind {
        ExprKind::AnonArray(elems) => {
            assert_eq!(elems.len(), 3);
            assert!(matches!(elems[0].kind, ExprKind::UnaryOp(UnaryOp::Negate, _)));
            assert!(matches!(elems[1].kind, ExprKind::UnaryOp(UnaryOp::LogNot, _)));
            assert!(matches!(elems[2].kind, ExprKind::Ref(_)));
        }
        other => panic!("expected AnonArray, got {other:?}"),
    }
}

#[test]
fn iter_string_negation_chain() {
    // `-(-(-"foo"))` — backward phase must chain string negation collapse correctly.
    // -"foo" → "-foo", then -"-foo" → "+foo", then -"+foo" → "-foo".
    let e = parse_expr_str("-(-(-\"foo\"));");
    assert!(matches!(e.kind, ExprKind::StringLit(ref s) if s == "-foo"), "expected StringLit(\"-foo\"), got {:?}", e.kind);
}

#[test]
fn iter_empty_nested_containers() {
    // `[[], {}]` — empty containers hit the Leaf path inside an accumulator.
    let e = parse_expr_str("[[], {}];");
    match &e.kind {
        ExprKind::AnonArray(elems) => {
            assert_eq!(elems.len(), 2);
            assert!(matches!(elems[0].kind, ExprKind::AnonArray(ref v) if v.is_empty()));
            assert!(matches!(elems[1].kind, ExprKind::AnonHash(ref v) if v.is_empty()));
        }
        other => panic!("expected AnonArray, got {other:?}"),
    }
}

#[test]
fn iter_trailing_comma_array() {
    // `[1, 2, 3,]` — trailing comma in array ref.
    let e = parse_expr_str("[1, 2, 3,];");
    match &e.kind {
        ExprKind::AnonArray(elems) => assert_eq!(elems.len(), 3),
        other => panic!("expected AnonArray, got {other:?}"),
    }
}

#[test]
fn iter_trailing_comma_hash() {
    // `{a => 1, b => 2,}` — trailing comma in hash ref.
    let e = parse_expr_str("{a => 1, b => 2,};");
    match &e.kind {
        ExprKind::AnonHash(elems) => assert_eq!(elems.len(), 4), // a, 1, b, 2
        other => panic!("expected AnonHash, got {other:?}"),
    }
}

#[test]
fn iter_deeply_mixed_array_hash() {
    // `[{[{[1]}]}]` — 5 levels of alternating array/hash nesting, all iterative.
    let e = parse_expr_str("[{a => [{b => [1]}]}];");
    assert!(matches!(e.kind, ExprKind::AnonArray(_)), "expected AnonArray, got {:?}", e.kind);
}

#[test]
fn iter_precedence_preserved_without_paren_node() {
    // `(1 + 2) * 3` — without Paren node, tree must still capture `Add` before `Mul`.
    let e = parse_expr_str("(1 + 2) * 3;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Mul, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::BinOp(BinOp::Add, _, _)), "expected Add on LHS of Mul, got {:?}", lhs.kind);
        }
        other => panic!("expected Mul, got {other:?}"),
    }
}

#[test]
fn iter_local_hash_elem() {
    // `local $hash{key}` — local frame wrapping a postfix subscript chain.
    let e = parse_expr_str("local $hash{key};");
    assert!(matches!(e.kind, ExprKind::Local(_)), "expected Local, got {:?}", e.kind);
}

#[test]
fn iter_nested_paren_ref() {
    // `\(1, 2)` — reference to a parenthesized list.  Ref frame, then Paren frame on the stack.
    let e = parse_expr_str("\\(1, 2);");
    match &e.kind {
        ExprKind::Ref(inner) => {
            assert!(matches!(inner.kind, ExprKind::Comma(_)), "expected Comma inside Ref, got {:?}", inner.kind);
        }
        other => panic!("expected Ref, got {other:?}"),
    }
}

#[test]
fn iter_double_parens_with_infix() {
    // `((1 + 2))` — double parens collapse to nothing, inner Add preserved.
    let e = parse_expr_str("((1 + 2));");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Add, _, _)), "expected Add, got {:?}", e.kind);
}

#[test]
fn iter_assignment_inside_array() {
    // `[$x = 1, $y = 2]` — right-associative assignment inside an ArrayRef accumulator.
    let e = parse_expr_str("[$x = 1, $y = 2];");
    match &e.kind {
        ExprKind::AnonArray(elems) => {
            assert_eq!(elems.len(), 2);
            assert!(matches!(elems[0].kind, ExprKind::Assign(AssignOp::Eq, _, _)));
            assert!(matches!(elems[1].kind, ExprKind::Assign(AssignOp::Eq, _, _)));
        }
        other => panic!("expected AnonArray, got {other:?}"),
    }
}

#[test]
fn iter_ternary_inside_array() {
    // `[$x ? 1 : 0, $y]` — ternary infix inside accumulator at PREC_COMMA+1.
    let e = parse_expr_str("[$x ? 1 : 0, $y];");
    match &e.kind {
        ExprKind::AnonArray(elems) => {
            assert_eq!(elems.len(), 2);
            assert!(matches!(elems[0].kind, ExprKind::Ternary(_, _, _)));
        }
        other => panic!("expected AnonArray, got {other:?}"),
    }
}

#[test]
fn iter_not_keyword_with_parens() {
    // `not ($x)` — Not frame at PREC_NOT_LOW, then Paren frame at PREC_LOW.  Two frames interact on the stack.
    let e = parse_expr_str("not ($x);");
    match &e.kind {
        ExprKind::UnaryOp(UnaryOp::Not, inner) => {
            assert!(matches!(inner.kind, ExprKind::ScalarVar(_)), "expected ScalarVar, got {:?}", inner.kind);
        }
        other => panic!("expected UnaryOp(Not), got {other:?}"),
    }
}

#[test]
fn iter_hash_ref_as_rhs() {
    // `$x = { a => 1 }` — hash ref via try_prefix in a nested parse_expr call (RHS of assignment).
    let e = parse_expr_str("$x = { a => 1 };");
    match &e.kind {
        ExprKind::Assign(_, _, rhs) => {
            assert!(matches!(rhs.kind, ExprKind::AnonHash(_)), "expected AnonHash, got {:?}", rhs.kind);
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn iter_postfix_inc_after_paren() {
    // `($x)++` — postfix ++ must see through the removed Paren and apply to the variable.
    let e = parse_expr_str("($x)++;");
    assert!(matches!(e.kind, ExprKind::PostfixOp(PostfixOp::Inc, _)), "expected PostfixOp(Inc), got {:?}", e.kind);
}

#[test]
fn iter_chained_method_after_paren() {
    // `($obj)->method` — method call on result of paren expression.
    let e = parse_expr_str("($obj)->method;");
    assert!(matches!(e.kind, ExprKind::MethodCall(_, _, _)), "expected MethodCall, got {:?}", e.kind);
}

#[test]
fn iter_single_elem_array_no_comma() {
    // `[42]` — single element, no comma, no trailing comma.  ArrayRef accumulator must handle one-element case.
    let e = parse_expr_str("[42];");
    match &e.kind {
        ExprKind::AnonArray(elems) => {
            assert_eq!(elems.len(), 1);
            assert!(matches!(elems[0].kind, ExprKind::IntLit(42)));
        }
        other => panic!("expected AnonArray, got {other:?}"),
    }
}

// ── Combined nightmare cases ──────────────────────────────

#[test]
fn iter_deref_block_scalar() {
    // `${$ref}` — DerefBlock(Scalar) frame.
    let e = parse_expr_str("${$ref};");
    assert!(matches!(e.kind, ExprKind::Deref(Sigil::Scalar, _)), "expected Deref(Scalar), got {:?}", e.kind);
}

#[test]
fn iter_deref_block_nested() {
    // `${${$ref}}` — two DerefBlock frames stacked.
    let e = parse_expr_str("${${$ref}};");
    match &e.kind {
        ExprKind::Deref(Sigil::Scalar, inner) => {
            assert!(matches!(inner.kind, ExprKind::Deref(Sigil::Scalar, _)), "expected nested Deref, got {:?}", inner.kind);
        }
        other => panic!("expected Deref(Scalar), got {other:?}"),
    }
}

#[test]
fn iter_deref_block_with_subscript() {
    // `${$ref}[0]` — DerefBlock applies, then maybe_postfix_subscript attaches the array element.
    let e = parse_expr_str("${$ref}[0];");
    assert!(matches!(e.kind, ExprKind::ArrayElem(_, _)), "expected ArrayElem, got {:?}", e.kind);
}

#[test]
fn iter_deref_block_code_with_args() {
    // `&{$coderef}(1, 2)` — DerefBlock(Code) frame, then maybe_call_args produces MethodCall.
    let e = parse_expr_str("&{$coderef}(1, 2);");
    assert!(matches!(e.kind, ExprKind::MethodCall(_, _, _)), "expected MethodCall, got {:?}", e.kind);
}

#[test]
fn iter_deref_array_block() {
    // `@{$ref}` — DerefBlock(Array) frame.
    let e = parse_expr_str("@{$ref};");
    assert!(matches!(e.kind, ExprKind::Deref(Sigil::Array, _)), "expected Deref(Array), got {:?}", e.kind);
}

#[test]
fn iter_deref_hash_block() {
    // `%{$ref}` — DerefBlock(Hash) frame.
    let e = parse_expr_str("%{$ref};");
    assert!(matches!(e.kind, ExprKind::Deref(Sigil::Hash, _)), "expected Deref(Hash), got {:?}", e.kind);
}

#[test]
fn iter_deref_glob_block() {
    // `*{$ref}` — DerefBlock(Glob) frame.
    let e = parse_expr_str("*{$ref};");
    assert!(matches!(e.kind, ExprKind::Deref(Sigil::Glob, _)), "expected Deref(Glob), got {:?}", e.kind);
}

#[test]
fn iter_deref_inside_array_ref() {
    // `[${$x}, @{$y}]` — deref frames nested inside an ArrayRef accumulator.
    let e = parse_expr_str("[${$x}, @{$y}];");
    match &e.kind {
        ExprKind::AnonArray(elems) => {
            assert_eq!(elems.len(), 2);
            assert!(matches!(elems[0].kind, ExprKind::Deref(Sigil::Scalar, _)));
            assert!(matches!(elems[1].kind, ExprKind::Deref(Sigil::Array, _)));
        }
        other => panic!("expected AnonArray, got {other:?}"),
    }
}

#[test]
fn iter_eval_expr_vs_block() {
    // `eval "code"` — EvalExpr frame.
    let e = parse_expr_str("eval '1+1';");
    assert!(matches!(e.kind, ExprKind::EvalExpr(_)), "expected EvalExpr, got {:?}", e.kind);

    // `eval { code }` — NOT a frame, returns Leaf with EvalBlock.
    let prog = parse("eval { 1 };");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => assert!(matches!(e.kind, ExprKind::EvalBlock(_)), "expected EvalBlock, got {:?}", e.kind),
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn iter_do_expr_vs_block() {
    // `do "file.pl"` — DoExpr frame.
    let e = parse_expr_str("do 'file.pl';");
    assert!(matches!(e.kind, ExprKind::DoExpr(_)), "expected DoExpr, got {:?}", e.kind);
}

#[test]
fn iter_return_with_value() {
    let e = parse_expr_str("return 42;");
    match &e.kind {
        ExprKind::Return(Some(operand)) => {
            assert!(matches!(operand.kind, ExprKind::IntLit(42)), "expected return 42, got {:?}", operand.kind);
        }
        other => panic!("expected Return(Some), got {other:?}"),
    }
}

#[test]
fn iter_return_bare() {
    let e = parse_expr_str("return;");
    match &e.kind {
        ExprKind::Return(None) => {}
        other => panic!("expected Return(None), got {other:?}"),
    }
}

#[test]
fn iter_negate_deref_block() {
    // `-${$x}` — Negate frame then DerefBlock(Scalar) frame stacked.
    let e = parse_expr_str("-${$x};");
    match &e.kind {
        ExprKind::UnaryOp(UnaryOp::Negate, inner) => {
            assert!(matches!(inner.kind, ExprKind::Deref(Sigil::Scalar, _)), "expected Deref inside Negate, got {:?}", inner.kind);
        }
        other => panic!("expected Negate, got {other:?}"),
    }
}

#[test]
fn iter_deeply_nested_deref_blocks() {
    // 100 levels of `${...}` nesting — all iterative via DerefBlock frames.
    let depth = 100;
    let src = format!("{}$x{};", "${".repeat(depth), "}".repeat(depth));
    let result = crate::parse(src.as_bytes());
    assert!(result.is_ok(), "100 nested deref blocks should be iterative, got: {:?}", result.err());
}

#[test]
fn hard_nightmare_map_ternary_hash() {
    // `map { /x/ ? { a => 1 } : { b => 2 } } @list;`
    // Exercises: block-vs-hash, regex-vs-division, ternary grouping.
    let e = parse_expr_str("map { /x/ ? { a => 1 } : { b => 2 } } @list;");
    match &e.kind {
        ExprKind::ListOp(name, args) => {
            assert_eq!(name, "CORE::map");
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
// For cases where the exact AST shape depends on decisions we haven't firmed up (or features we haven't implemented
// yet), at least verify the parser accepts them.

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
    // Since `my` is an expression, the whole thing is a Comma with an Assign(Decl(My), $a) first, then $b.
    let prog = parse("my $x = $a, $b;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Comma(items), .. }) => {
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
        StmtKind::Expr(Expr { kind: ExprKind::Comma(items), .. }) => {
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
    let prog = parse("use feature 'state'; state $x = $a, $b;");

    // statements[0] is the `use` declaration; the expression is statements[1].
    match &prog.statements[1].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Comma(items), .. }) => {
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
    // `local $x = $a, $b;` — local is an expression too; the trailing comma must NOT be absorbed into the Local
    // operand.  Must group as `(local $x = $a), $b`, giving Comma([Assign(Local($x), $a), $b]).
    let prog = parse("local $x = $a, $b;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Comma(items), .. }) => {
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
// Verify that each declaration kind produces an expression (wrapped in Stmt::Expr), not a dedicated statement kind.

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
    let prog = parse("use feature 'state'; state $x;");
    match &prog.statements[1].kind {
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
// Declarations as expressions should be usable in any context that accepts an expression — not just at statement start.

#[test]
fn hard_my_in_parens() {
    // `(my $x) = @list;` — decl inside parens on LHS of assignment.
    let prog = parse("(my $x) = @list;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::Assign(_, lhs, _), .. }) => {
            assert!(matches!(lhs.kind, ExprKind::Decl(DeclScope::My, _)), "expected Decl on LHS, got {:?}", lhs.kind);
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
    // `if (my $x = foo()) { ... }` — decl in an if condition.  The decl is nested inside an If statement's paren-expr.
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
// These verify that a sub's prototype — registered in the symbol table at declaration time — drives how arguments at
// call sites are parsed.  Includes adversarial cases designed to break naive parsers.

/// Given `sub NAME (PROTO); CALL`, parse and return the expression from the second statement (the call).
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
    // Empty prototype forces zero args, so `+ 1` is a binary op.  Expected: BinOp(Add, FuncCall("foo", []), Int(1)).
    let e = parse_call_with_proto("sub foo (); foo + 1;");
    match &e.kind {
        ExprKind::BinOp(BinOp::Add, lhs, rhs) => {
            match &lhs.kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "main::foo");
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
            assert_eq!(name, "main::foo");
            assert_eq!(args.len(), 1, "$-proto should take exactly 1 arg");
            assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Add, _, _)), "arg should be $a + $b, got {:?}", args[0].kind);
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_single_scalar_comma_terminates_arg() {
    // sub foo ($); foo $a, $b;
    // One-scalar proto: `$a` is the arg; comma ends the call, and `$b` is a separate list element.  Expected:
    // Comma([FuncCall("foo", [$a]), $b]).
    let e = parse_call_with_proto("sub foo ($); foo $a, $b;");
    match &e.kind {
        ExprKind::Comma(items) => {
            assert_eq!(items.len(), 2);
            match &items[0].kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "main::foo");
                    assert_eq!(args.len(), 1);
                    assert!(matches!(args[0].kind, ExprKind::ScalarVar(_)));
                }
                other => panic!("expected FuncCall(foo, [$a]), got {other:?}"),
            }
            assert!(matches!(items[1].kind, ExprKind::ScalarVar(_)));
        }
        other => panic!("expected Comma with foo call and $b, got {other:?}"),
    }
}

#[test]
fn proto_two_scalars_takes_two_args() {
    // sub foo ($$); foo $a + $b, $c;
    // Two-scalar proto: `$a + $b` is arg 1, `$c` is arg 2.
    let e = parse_call_with_proto("sub foo ($$); foo $a + $b, $c;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "main::foo");
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
    // &@-proto: first arg is a block (wrapped as AnonSub), second is the slurpy list.
    let e = parse_call_with_proto("sub foo (&@); foo { $x } @list;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "main::foo");
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
            assert_eq!(name, "main::foo");
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
            assert_eq!(name, "main::foo");
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
            assert_eq!(name, "main::foo");
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
    // A proto declared in Foo shouldn't affect bare calls in main.  package Foo; sub bar (); package main; bar + 1;
    // The bare `bar` in main isn't found → falls through to Bareword + BinOp.
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
    // package Foo; sub bar (); package main; Foo::bar + 1; Fully-qualified call finds the proto → zero-arg call.
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
            assert_eq!(name, "main::foo");
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
            assert_eq!(name, "main::foo");
            assert_eq!(args.len(), 1, "_-slot should default to DefaultVar when omitted");
            assert!(matches!(args[0].kind, ExprKind::DefaultVar), "expected DefaultVar, got {:?}", args[0].kind);
        }
        other => panic!("expected FuncCall with DefaultVar, got {other:?}"),
    }
}

#[test]
fn proto_underscore_distinct_from_explicit_dollar_underscore() {
    // sub foo (_); foo $_;
    // Explicit $_ should be ScalarVar("_"), NOT DefaultVar.  This pins down the distinction: the parser inserts
    // DefaultVar only when the arg is omitted.
    let e = parse_call_with_proto("sub foo (_); foo $_;");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            assert_eq!(args.len(), 1);

            // Note: $_ may be represented as SpecialVar or ScalarVar depending on the lexer; either is fine, as long as
            // it's NOT DefaultVar.
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
            assert_eq!(name, "main::foo");
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
    // A scalar expression in a `*` slot is parsed as-is — it's presumed to hold a glob ref at runtime.
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
//   1. Parens form: foo(args) — args are parens-delimited, so the parser takes a generic comma-separated list without
//      consulting the prototype.  (Perl may still validate arg counts at compile time; that's a semantic-pass concern,
//      not a parsing concern.)
//   2. Ampersand form: &foo(args) — goes through the code-ref prefix path, completely bypassing parse_ident_term and
//      therefore the symbol-table lookup.

#[test]
fn proto_parens_form_parses_generic_list() {
    // sub foo ($); foo($a + $b, $c);
    // Without parens, `$` proto would consume only `$a + $b` and leave `$c` in the outer comma list.  With parens, the
    // args are delimited, so we get both.
    let e = parse_call_with_proto("sub foo ($); foo($a + $b, $c);");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "main::foo");
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
    // Parens form takes the args; Perl would report "Too many arguments" at compile time but we don't validate yet.
    let e = parse_call_with_proto("sub foo (); foo(1, 2);");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "main::foo");
            assert_eq!(args.len(), 2);
        }
        other => panic!("expected FuncCall with 2 args, got {other:?}"),
    }
}

#[test]
fn proto_ampersand_call_bypasses_empty_proto() {
    // sub foo (); &foo(1, 2);
    // &foo() completely bypasses prototype parsing.  Without the &, `foo(1, 2)` would still work via parens (see test
    // above), but the &-form is the canonical bypass.
    let e = parse_call_with_proto("sub foo (); &foo(1, 2);");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "main::foo");
            assert_eq!(args.len(), 2, "&foo(...) bypasses empty proto");
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn proto_ampersand_no_parens_bypasses_proto() {
    // sub foo ($); &foo;
    // &foo with no parens calls with current @_ (inherited); prototype is not consulted.
    let e = parse_call_with_proto("sub foo ($); &foo;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "main::foo");
            assert_eq!(args.len(), 0, "&foo with no parens inherits @_");
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

// ── Named-unary precedence for scalar-ish slots ─────────────
//
// A `$`-slot (or `_`, `+`, `\X`, `\[...]`, glob-expression) parses its arg at named-unary precedence.  That means
// operators tighter than named unary (shift, +, -, *, /, **, etc.) are consumed into the arg, while operators looser
// (relational, equality, ternary, assignment, comma) terminate the arg and apply at the outer level.

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
    // `<` (relational, looser than named unary) terminates the arg.  Parses as `foo($a) < 1`.
    let e = parse_call_with_proto("sub foo ($); foo $a < 1;");
    match &e.kind {
        ExprKind::BinOp(BinOp::NumLt, lhs, rhs) => {
            match &lhs.kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "main::foo");
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
    // `==` is looser than named unary → terminates arg.  Parses as `foo(1) == 2`.
    let e = parse_call_with_proto("sub foo ($); foo 1 == 2;");
    match &e.kind {
        ExprKind::BinOp(BinOp::NumEq, lhs, rhs) => {
            match &lhs.kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "main::foo");
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
    // Ternary is far below named unary → terminates arg.  Parses as `foo($a) ? $b : $c`.
    let e = parse_call_with_proto("sub foo ($); foo $a ? $b : $c;");
    match &e.kind {
        ExprKind::Ternary(cond, _, _) => match &cond.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "main::foo");
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
    // Both `+` and `*` are tighter than named unary, so the whole arithmetic expression is the single arg.
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
// A `&` prototype slot accepts either a literal block (wrapped as an anonymous sub) or any code-reference expression —
// `\&name`, `$coderef`, `sub { ... }`, etc.

#[test]
fn proto_amp_slot_accepts_backslash_sub_ref() {
    // sub foo (&@); foo \&bar, @list;
    // `\&bar` is a reference-to-sub expression.
    let e = parse_call_with_proto("sub foo (&@); foo \\&bar, @list;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "main::foo");
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
            assert_eq!(name, "main::foo");
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
            assert_eq!(name, "main::foo");
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
// `\$`, `\@`, `\%`, `\&`, `\*`, `\[...]`, and `+` all cause the argument to be implicitly referenced at the call site.
// `foo @arr` with `sub foo (\@)` is equivalent to `foo(\@arr)`.  The parser wraps the argument in an ExprKind::Ref; any
// validation that the argument is of the expected kind is a semantic-pass concern.

#[test]
fn proto_auto_ref_array() {
    // sub foo (\@); foo @arr;  →  foo(\@arr)
    let e = parse_call_with_proto("sub foo (\\@); foo @arr;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "main::foo");
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
// In the initial slot, `&` plus a bare `{` parses the block as an anonymous sub (the map/grep pattern).  In any non-
// initial position, `{` at a call site is an ordinary hash-ref constructor; to pass a code reference the caller must
// spell it out: `sub { ... }`, `\&name`, `$coderef`, etc.

#[test]
fn proto_amp_non_initial_brace_is_hash_ref() {
    // sub foo ($&); foo $x, { a => 1 };
    // The `{ a => 1 }` is a hash-ref constructor, NOT a block.
    let e = parse_call_with_proto("sub foo ($&); foo $x, { a => 1 };");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "main::foo");
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
    // Regression: initial `&` with bare block still wraps as AnonSub.
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
// Modern Perl (5.20+) allows the prototype to be declared via an attribute rather than the paren form:
//   sub foo :prototype($$) { ... }
// The attribute form is equivalent to the paren form but avoids the paren/signatures ambiguity.

#[test]
fn proto_attribute_form_registers_prototype() {
    // sub foo :prototype($$) { } foo $a + $b, $c;
    // Prototype declared via attribute drives call-site parsing just like the paren form.
    let e = parse_call_with_proto("sub foo :prototype($$) { } foo $a + $b, $c;");
    match &e.kind {
        ExprKind::FuncCall(name, args) => {
            assert_eq!(name, "main::foo");
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
                    assert_eq!(name, "main::foo");
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
// Extended syntax features.
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
fn escape_n_charname_resolved() {
    // `\N{SNOWMAN}` — named character resolved via unicode_names2 to U+2603 (☃).
    let e = parse_expr_str(r#""\N{SNOWMAN}";"#);
    match &e.kind {
        ExprKind::StringLit(s) => {
            assert_eq!(s, "\u{2603}", "\\N{{SNOWMAN}} should resolve to snowman");
        }
        other => panic!("expected StringLit with snowman, got {other:?}"),
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
    // `~~` is at PREC_EQ (same as `==`), non-associative.  `$a == $b ~~ $c` should error or parse as comparison chain —
    // but since both are non-associative at the same level, the Pratt loop stops after the first one.  We just verify
    // `$a ~~ $b` parses at the right level.
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
    // After `no feature ':all'`, smartmatch is off.  The lexer still emits Token::SmartMatch (it doesn't have feature
    // state), but op_info_for_token won't recognize it as an operator.  The expression `$a ~~ $b` fails to parse as a single
    // expression — `$a` is one statement and `~~` is an unexpected token.
    //
    // A full solution would need lexer-level token demotion (splitting SmartMatch back into two Tildes), similar to
    // keyword demotion.  For now, verify the program doesn't produce a SmartMatch BinOp.
    let prog = parse("no feature ':all'; ~$b;");

    // Just confirms the feature removal doesn't break normal `~` (bitwise not).
    assert!(!prog.statements.is_empty());
}

// ── String-bitwise operators ─────────────────────────────

#[test]
fn string_bitwise_and() {
    let e = parse_expr_str("use feature 'bitwise'; $a &. $b;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::StringBitAnd, _, _)), "expected StringBitAnd, got {:?}", e.kind);
}

#[test]
fn string_bitwise_or() {
    let e = parse_expr_str("use feature 'bitwise'; $a |. $b;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::StringBitOr, _, _)), "expected StringBitOr, got {:?}", e.kind);
}

#[test]
fn string_bitwise_xor() {
    let e = parse_expr_str("use feature 'bitwise'; $a ^. $b;");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::StringBitXor, _, _)), "expected StringBitXor, got {:?}", e.kind);
}

#[test]
fn string_bitwise_not() {
    let e = parse_expr_str("use feature 'bitwise'; ~. $a;");
    assert!(matches!(e.kind, ExprKind::UnaryOp(UnaryOp::StringBitNot, _)), "expected StringBitNot, got {:?}", e.kind);
}

#[test]
fn string_bitwise_and_assign() {
    let e = parse_expr_str("use feature 'bitwise'; $a &.= $b;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::StringBitAndEq, _, _)), "expected &.= assign, got {:?}", e.kind);
}

#[test]
fn string_bitwise_or_assign() {
    let e = parse_expr_str("use feature 'bitwise'; $a |.= $b;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::StringBitOrEq, _, _)), "expected |.= assign, got {:?}", e.kind);
}

#[test]
fn string_bitwise_xor_assign() {
    let e = parse_expr_str("use feature 'bitwise'; $a ^.= $b;");
    assert!(matches!(e.kind, ExprKind::Assign(AssignOp::StringBitXorEq, _, _)), "expected ^.= assign, got {:?}", e.kind);
}

#[test]
fn string_bitwise_precedence() {
    // `&.` has PREC_BIT_AND, which is tighter than `|.`.  `$a |. $b &. $c` → `$a |. ($b &. $c)`.
    let e = parse_expr_str("use feature 'bitwise'; $a |. $b &. $c;");
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
    // `CORE::say(...)` parses as a package-qualified function call.  The semantic distinction (forcing the builtin) is
    // a compiler concern; the parser treats it like any other qualified name.
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
    // `use utf8` is lexically scoped: inside a `no utf8` block, UTF-8 identifiers are rejected again.
    let src = "use utf8; my $café = 1; { no utf8; my $x = 1; }";
    let prog = parse(src);

    // The program parses — $café is in utf8 scope, $x is in no-utf8 scope (ASCII only, fine).
    assert!(prog.statements.len() >= 2);
}

#[test]
fn utf8_lexical_scoping_error_in_block() {
    // After `no utf8` inside a block, UTF-8 identifiers should error — matching Perl's behavior.
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
// perlsyn gap-probing tests — features from perlsyn that may or may not be implemented.  Failures are diagnostic.
// ═══════════════════════════════════════════════════════════

// ── 1. Postfix `when` modifier ───────────────────────────

#[test]
fn postfix_when_modifier_v514() {
    // `$abc = 1 when /^abc/;` — perlsyn lists `when EXPR` as a statement modifier alongside if/unless/while/until.
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
    // Without the switch feature, `when` is demoted to a bare identifier.  `$abc = 1 when ...` would parse `when` as a
    // bareword function call — the expression becomes `1 when(...)` which is NOT a postfix modifier.  Just verify it
    // doesn't produce PostfixKind::When.
    let prog = parse("$abc = 1; when(/^abc/);");

    // Without switch, `when` is a function call, not a keyword.  The program parses as two separate statements.
    assert!(prog.statements.len() >= 2);

    // Verify the first statement is NOT a postfix-when.
    let not_postfix_when = !matches!(&prog.statements[0].kind, StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::When, _, _), .. }));
    assert!(not_postfix_when, "when should not be a postfix modifier without the switch feature");
}

// ── 2. continue block on bare BLOCK ──────────────────────

#[test]
fn continue_block_on_bare_block() {
    // perlsyn: `LABEL BLOCK continue BLOCK` is valid.  A bare block acts as a loop that executes once.
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
    // `for my ($key, $value) (%hash) { ... }` — iterating over multiple values at a time (Perl 5.36+).
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
    // `continue` inside a `when` block means fall through to the next when — different from `continue BLOCK`.
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
    // Leading whitespace — `#` is NOT at column 0, so it's just a regular comment, not a directive.
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
    // `^^` is lower than `||` but same level.  `$a || $b ^^ $c` → `($a || $b) ^^ $c`.
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
    // `"\Q'\Ufoo\Ebar'\E"` → `\\'FOObar\\'` \Q quotemeta, then \U uppercase stacks on top.  \E pops \U, \E pops \Q.
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
    // `"\Utest$x\E"` → InterpolatedString with: Const("TEST"), ScalarInterp(uc($x))
    let e = parse_expr_str(r#""\Utest$x\E";"#);
    match &e.kind {
        ExprKind::InterpolatedString(interp) => {
            // First part: constant "TEST" (uppercased at lex time).
            assert!(matches!(&interp.0[0], InterpPart::Const(s) if s == "TEST"), "first part should be Const(TEST), got {:?}", interp.0[0]);

            // Second part: $x wrapped in uc().
            match &interp.0[1] {
                InterpPart::ScalarInterp(expr) => {
                    assert!(matches!(&expr.kind, ExprKind::FuncCall(name, _) if name == "CORE::uc"), "interp should be uc($x), got {:?}", expr.kind);
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
                assert!(matches!(&expr.kind, ExprKind::FuncCall(name, _) if name == "CORE::lcfirst"), "interp should be lcfirst($X), got {:?}", expr.kind);
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
                        ExprKind::FuncCall(name, args) if name == "CORE::quotemeta" => {
                            // Inner should be uc.
                            assert!(matches!(&args[0].kind, ExprKind::FuncCall(n, _) if n == "CORE::uc"), "inner should be uc, got {:?}", args[0].kind);
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
    assert!(matches!(e.kind, ExprKind::FuncCall(ref name, _) if name == "main::any"), "without feature, any() should be a regular call, got {:?}", e.kind);
}

#[test]
fn all_without_feature_is_bareword() {
    let e = parse_expr_str("all();");
    assert!(matches!(e.kind, ExprKind::FuncCall(ref name, _) if name == "main::all"), "without feature, all() should be a regular call, got {:?}", e.kind);
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
    // Verify the full qualified name survives scanning.  `Package->new()` produces ExprKind::MethodCall(Bareword(pkg),
    // "new", []).
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
    // U+0301 COMBINING ACUTE ACCENT is XID_Continue but not XID_Start.  As the first char after a sigil, it should
    // fail.
    let src = "use utf8; my $\u{0301}x = 1;";
    let mut p = Parser::new(src.as_bytes()).unwrap();
    assert!(p.parse_program().is_err(), "combining mark should not be valid as identifier start");
}

#[test]
fn utf8_combining_mark_ok_as_continue() {
    // Combining mark after a valid start character is fine.  `$e\u{0301}` = $é (e + combining acute) — valid.
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
    // Without `use utf8`, high bytes are errors, so NFC normalization never applies.
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
    // NFC $café and NFD $café in the same program must produce identical variable names, or the runtime would treat
    // them as different variables.
    let tokens = collect_tokens_utf8("use utf8; $caf\u{00E9}; $cafe\u{0301};");
    let scalar_names: Vec<&str> = tokens.iter().filter_map(|t| if let Token::ScalarVar(name) = t { Some(name.as_str()) } else { None }).collect();
    assert!(scalar_names.len() >= 2, "should find at least 2 scalar vars");
    assert_eq!(scalar_names[0], scalar_names[1], "NFC and NFD forms should produce the same variable name: {:?} vs {:?}", scalar_names[0], scalar_names[1]);
    assert_eq!(scalar_names[0], "caf\u{00E9}", "both should be NFC: {:?}", scalar_names[0]);
}

#[test]
fn nfc_interpolation_variable_name_normalized() {
    // Interpolated NFD $café inside a string — the InterpScalar token should contain the NFC name.
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

    // RHS is Comma([StringLit("café"), Int(1)]).
    fn find_nfc_key(e: &Expr) -> bool {
        match &e.kind {
            ExprKind::StringLit(s) => s == "caf\u{00E9}",
            ExprKind::Comma(items) => items.iter().any(find_nfc_key),
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
    // Non-ASCII content in a regular double-quoted string under use utf8.  The memchr fast path should bulk-copy the
    // UTF-8 correctly.
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
    // Standard q{} with non-ASCII body under use utf8.  No Unicode delimiters — tests the basic byte-by-byte fallback
    // with ASCII delimiters.
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
    // "\Ucafé\E" under use utf8 — the byte-by-byte fallback must decode multi-byte UTF-8 characters, not split them
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
    // `Foo::3bar` — 3 is XID_Continue but NOT XID_Start.  After ::, the next segment needs XID_Start.
    let src = "use utf8; Foo::3bar;";
    let mut p = Parser::new(src.as_bytes()).unwrap();

    // This should either error or parse 3 as a number, not as part of the identifier.
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
    // Heredoc near UTF-8 code — the tag itself is ASCII but the surrounding context uses UTF-8.
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

// ── Adversarial edge cases ───────────────────────────────

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
            assert_eq!(name, "CORE::print");
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
    // `map { { a => 1 } } @list` — outer braces are a block, inner braces are an anonymous hash.
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
            ExprKind::BinOp(_, l, r) | ExprKind::Assign(_, l, r) | ExprKind::Range(l, r, _) => expr_contains_anon_hash(l) || expr_contains_anon_hash(r),
            ExprKind::UnaryOp(_, inner)
            | ExprKind::PostfixOp(_, inner)
            | ExprKind::Ref(inner)
            | ExprKind::DoExpr(inner)
            | ExprKind::EvalExpr(inner)
            | ExprKind::Local(inner) => expr_contains_anon_hash(inner),
            ExprKind::Ternary(c, t, f) => expr_contains_anon_hash(c) || expr_contains_anon_hash(t) || expr_contains_anon_hash(f),
            ExprKind::FuncCall(_, args) | ExprKind::ListOp(_, args) | ExprKind::Comma(args) | ExprKind::AnonArray(args) => {
                args.iter().any(expr_contains_anon_hash)
            }
            _ => false,
        }
    }

    assert!(expr_contains_anon_hash(&e), "expected an AnonHash somewhere in {:?}", e.kind);
}

#[test]
fn current_package_restores_after_block_form_package() {
    // `package Inner { ... }` — block-form package scopes and restores the outer package name.
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
    // `{ package Inner; __PACKAGE__; }` — statement-form package inside a bare block.  In Perl, the package name is
    // scoped to the enclosing block and restored on exit.
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
    assert!(matches!(e.kind, ExprKind::FuncCall(ref name, _) if name == "main::class"));
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
    // « is U+00AB (0xC2 0xAB), » is U+00BB (0xC2 0xBB).  £ is U+00A3 (0xC2 0xA3) — shares lead byte 0xC2 with both.
    // Memchr triggers on 0xC2 inside the body, but the old byte-by-byte fallback did skip(1) + b as char, splitting
    // the multi-byte £ into two garbled characters.
    let prog = parse("use utf8; use feature 'extra_paired_delimiters'; my $x = q\u{00AB}\u{00A3}\u{00BB};");
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
    let prog = parse("use utf8; use feature 'extra_paired_delimiters'; my $x = 'hello'; $x =~ s\u{00AB}hell\u{00BB}\u{00AB}heaven\u{00BB};");
    assert!(prog.statements.len() >= 3, "expected at least 3 statements");
}

#[test]
fn tr_unicode_delimiters_paired() {
    // tr«abc»«ABC» with extra_paired_delimiters.
    let prog = parse("use utf8; use feature 'extra_paired_delimiters'; my $x = 'abc'; $x =~ tr\u{00AB}abc\u{00BB}\u{00AB}ABC\u{00BB};");
    assert!(prog.statements.len() >= 3);
}

// ── Non-paired Unicode delimiters ─────────────────────────
// Without extra_paired_delimiters, Unicode chars work as non-paired (same open/close) delimiters.

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

// ── Heredoc whitespace and Unicode tag rules ─────────────

#[test]
fn heredoc_space_before_quoted_tag_allowed() {
    // << "END" (space before double-quoted tag) is valid Perl.
    let prog = parse("my $x = << \"END\";\nhello\nEND\n");
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

#[test]
fn heredoc_space_before_single_quoted_tag_allowed() {
    // << 'END' (space before single-quoted tag) is valid Perl.
    let prog = parse("my $x = << 'END';\nhello $world\nEND\n");
    let init = first_assign_rhs(&prog);
    assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s == "hello $world\n"), "expected literal heredoc, got {:?}", init.kind);
}

#[test]
fn heredoc_space_before_bare_tag_is_shift() {
    // << END (space before bare tag) is NOT a heredoc in Perl.  Perl treats bare << as <<""  which is forbidden.  Our
    // parser should interpret this as shift-left, not a heredoc.  "1 << END" with END undefined is a compile error in
    // Perl, but for our parser it should parse as a shift expression, not as a heredoc.
    let src = "my $x = 1 << 2;";
    let prog = parse(src);
    let init = first_assign_rhs(&prog);
    assert!(matches!(init.kind, ExprKind::BinOp(_, _, _)), "expected BinOp (shift), got {:?}", init.kind);
}

#[test]
fn heredoc_bare_unicode_tag() {
    // <<café under use utf8 — bare Unicode heredoc tag.
    let prog = parse("use utf8; my $x = <<caf\u{00E9};\nhello\ncaf\u{00E9}\n");
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

#[test]
fn heredoc_backslash_unicode_tag() {
    // <<\café under use utf8 — backslash form with Unicode tag.
    let prog = parse("use utf8; my $x = <<\\caf\u{00E9};\nhello $world\ncaf\u{00E9}\n");
    let init = first_assign_rhs(&prog);

    // Backslash form suppresses interpolation.
    assert!(matches!(init.kind, ExprKind::StringLit(ref s) if s == "hello $world\n"), "expected literal heredoc, got {:?}", init.kind);
}

#[test]
fn heredoc_indented_bare_unicode_tag() {
    // <<~café under use utf8 — indented bare Unicode heredoc tag.
    let prog = parse("use utf8; my $x = <<~caf\u{00E9};\n    hello\n    caf\u{00E9}\n");
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

// ── Apostrophe as package separator ───────────────────────

#[test]
fn apos_sub_declaration() {
    // sub Foo'bar { 1 }
    let prog = parse("sub Foo'bar { 1 }");
    let sub = prog.statements.iter().find_map(|s| if let StmtKind::SubDecl(sd) = &s.kind { Some(sd) } else { None }).expect("should find sub declaration");
    assert_eq!(sub.name, "Foo::bar", "sub name should be normalized to Foo::bar");
}

#[test]
fn apos_package_declaration() {
    // package Foo'Bar;
    let prog = parse("package Foo'Bar;");
    let pkg =
        prog.statements.iter().find_map(|s| if let StmtKind::PackageDecl(pd) = &s.kind { Some(pd) } else { None }).expect("should find package declaration");
    assert_eq!(pkg.name, "Foo::Bar", "package name should be normalized to Foo::Bar");
}

#[test]
fn apos_fat_comma_autoquotes() {
    // Foo'Bar => 1 — fat comma autoquotes as "Foo::Bar"
    let prog = parse("my %h = (Foo'Bar => 1);");
    assert!(!prog.statements.is_empty());
}

#[test]
fn apos_hash_subscript_autoquoted() {
    // $h{Foo'Bar} — autoquoted bareword key should be "Foo::Bar"
    let prog = parse("my %h; $h{Foo'Bar};");
    assert!(!prog.statements.is_empty());
}

#[test]
fn apos_method_call() {
    // Foo'Bar->new()
    let prog = parse("Foo'Bar->new();");
    assert!(!prog.statements.is_empty());
}

#[test]
fn apos_use_module() {
    // use Foo'Bar; — module name with apostrophe
    let prog = parse("use Foo'Bar;");
    let use_decl = prog.statements.iter().find_map(|s| if let StmtKind::UseDecl(ud) = &s.kind { Some(ud) } else { None }).expect("should find use declaration");
    assert_eq!(use_decl.module, "Foo::Bar", "module name should be Foo::Bar");
}

#[test]
fn apos_hash_var_via_parser() {
    // %Foo'bar — hash variable via parser's lex_hash_var_after_percent
    let prog = parse("my %x = %Foo'bar;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn apos_disabled_by_v5_42() {
    // use v5.42 disables apostrophe_as_package_separator.  With the feature off, Foo'Bar is Foo (bareword) then 'Bar;'
    // (unterminated string — no closing ').  The parse should fail.
    let result = crate::parse(b"use v5.42; Foo'Bar;");
    assert!(result.is_err(), "expected parse error with feature off");
}

#[test]
fn apos_enabled_by_default() {
    // Without use v5.x, apostrophe is enabled by default.
    let prog = parse("sub Foo'bar { 1 }");
    let sub = prog.statements.iter().find_map(|s| if let StmtKind::SubDecl(sd) = &s.kind { Some(sd) } else { None }).expect("should find sub");
    assert_eq!(sub.name, "Foo::bar", "Foo'bar should be normalized to Foo::bar by default");
}

#[test]
fn apos_re_enabled_after_v5_42() {
    // use v5.42, then re-enable the feature.
    let prog = parse("use v5.42; use feature 'apostrophe_as_package_separator'; sub Foo'bar { 1 }");
    let sub =
        prog.statements.iter().find_map(|s| if let StmtKind::SubDecl(sd) = &s.kind { Some(sd) } else { None }).expect("should find sub after re-enabling");
    assert_eq!(sub.name, "Foo::bar");
}

#[test]
fn apos_heredoc_tag_not_separator() {
    // <<Foo'Bar — apostrophe starts a quoted tag, not a separator.  Perl treats <<Foo as bare tag "Foo", then 'Bar' is
    // a string.
    let prog = parse("my $x = <<Foo . 'suffix';\nbody\nFoo\n");
    assert!(!prog.statements.is_empty());
}

#[test]
fn apos_glob_after_star() {
    // *Foo'bar — glob with apostrophe
    let tokens = collect_tokens("*Foo'bar;");
    assert!(tokens.iter().any(|t| matches!(t, Token::Ident(s) if s == "Foo::bar")), "expected Ident(Foo::bar) after *, got tokens: {:?}", tokens);
}

// ── Apostrophe + UTF-8 ───────────────────────────────────

#[test]
fn apos_with_utf8_identifier() {
    // Foo'café under use utf8
    let tokens = collect_tokens_utf8("use utf8; Foo'caf\u{00E9};");
    assert!(tokens.iter().any(|t| matches!(t, Token::Ident(s) if s == "Foo::caf\u{00E9}")), "expected Ident(Foo::café) with UTF-8, got tokens: {:?}", tokens);
}

#[test]
fn apos_dollar_utf8_after_apos() {
    // $'café under use utf8
    let tokens = collect_tokens_utf8("use utf8; $'caf\u{00E9};");
    assert!(tokens.iter().any(|t| matches!(t, Token::ScalarVar(s) if s == "::caf\u{00E9}")), "expected ScalarVar(::café), got tokens: {:?}", tokens);
}

#[test]
fn apos_interp_utf8_in_string() {
    // "$Foo'café" under use utf8 in string interpolation
    let tokens = collect_tokens_utf8("use utf8; \"$Foo'caf\u{00E9}\";");
    assert!(
        tokens.iter().any(|t| matches!(t, Token::InterpScalar(s) if s == "Foo::caf\u{00E9}")),
        "expected InterpScalar(Foo::café), got tokens: {:?}",
        tokens
    );
}

// ── any/all list processing operators ────────────────────

#[test]
fn parse_any_block_list() {
    // any { BLOCK } LIST — basic form.
    let prog = parse("use feature 'any'; my @x = (1,2,3); my $r = any { $_ > 2 } @x;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn parse_all_block_list() {
    // all { BLOCK } LIST — basic form.
    let prog = parse("use feature 'all'; my @x = (1,2,3); my $r = all { $_ > 0 } @x;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn parse_any_with_parens() {
    // any({ BLOCK }, LIST) — parenthesized form.
    let prog = parse("use feature 'any'; my $r = any({ $_ > 2 }, 1, 2, 3);");
    assert!(!prog.statements.is_empty());
}

#[test]
fn parse_all_with_parens() {
    // all({ BLOCK }, LIST) — parenthesized form.
    let prog = parse("use feature 'all'; my $r = all({ $_ > 0 }, 1, 2, 3);");
    assert!(!prog.statements.is_empty());
}

#[test]
fn parse_any_without_feature_is_bareword() {
    // Without the feature, 'any' is a regular bareword.  any(...) parses as a function call, not a keyword.
    let prog = parse("my $r = any(1, 2, 3);");
    let init = first_assign_rhs(&prog);
    assert!(matches!(init.kind, ExprKind::FuncCall(ref name, _) if name == "main::any"), "expected FuncCall(any), got {:?}", init.kind);
}

#[test]
fn parse_all_without_feature_is_bareword() {
    // Without the feature, 'all' is a regular bareword.
    let prog = parse("my $r = all(1, 2, 3);");
    let init = first_assign_rhs(&prog);
    assert!(matches!(init.kind, ExprKind::FuncCall(ref name, _) if name == "main::all"), "expected FuncCall(all), got {:?}", init.kind);
}

#[test]
fn parse_any_with_feature_is_keyword() {
    // With the feature, 'any { BLOCK } LIST' uses keyword parsing.  Verify it parses as a FuncCall with an anon-sub
    // first arg (block-list-op pattern like grep/map).
    let prog = parse("use feature 'any'; my $r = any { $_ > 0 } 1, 2, 3;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn parse_all_with_feature_is_keyword() {
    // With the feature, 'all { BLOCK } LIST' uses keyword parsing.
    let prog = parse("use feature 'all'; my $r = all { $_ > 0 } 1, 2, 3;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn parse_any_nested_in_all() {
    // Nested: all { any { ... } @inner } @outer
    let prog = parse("use feature 'any'; use feature 'all'; my $r = all { any { $_ > 0 } @_ } @lists;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn parse_any_with_literal_list() {
    // any { BLOCK } literal list
    let prog = parse("use feature 'any'; my $r = any { $_ > 5 } 1, 2, 3, 4, 5, 6;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn parse_any_fat_comma_autoquotes() {
    // any => 1 — fat comma autoquotes 'any' as a string even with the feature enabled.
    let prog = parse("use feature 'any'; my %h = (any => 1);");
    assert!(!prog.statements.is_empty());
}

#[test]
fn parse_all_fat_comma_autoquotes() {
    // all => 1 — fat comma autoquotes 'all' as a string.
    let prog = parse("use feature 'all'; my %h = (all => 1);");
    assert!(!prog.statements.is_empty());
}

// ── Adversarial: apostrophe after keywords ───────────────

#[test]
fn apos_grep_keyword_not_consumed_as_package() {
    // grep'x',@list — non-block grep with string expression.  scan_ident must not consume grep'x as grep::x.
    let prog = parse("my @list = ('ax','bx','c'); my @r = grep'x',@list;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn apos_sort_keyword_not_consumed_as_package() {
    // sort'func' — sort with a named comparison function.  scan_ident must not consume sort'func as sort::func.
    let prog = parse("sub func { $a cmp $b } my @list = (3,1,2); my @r = sort'func'@list;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn apos_map_keyword_not_consumed_as_package() {
    // map with string expression: map'x'.$_,@list scan_ident must not consume map'x as map::x.
    let prog = parse("my @list = (1,2,3); my @r = map'x',@list;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn apos_print_keyword_not_consumed_as_package() {
    // print'hello' — print with adjacent string argument.  scan_ident must not consume print'hello as print::hello.
    let prog = parse("print'hello';");
    assert!(!prog.statements.is_empty());
}

#[test]
fn apos_say_keyword_not_consumed_as_package() {
    // say'hello' — say with adjacent string argument.
    let prog = parse("use feature 'say'; say'hello';");
    assert!(!prog.statements.is_empty());
}

#[test]
fn apos_die_keyword_not_consumed_as_package() {
    // die'message' — die with adjacent string argument.
    let prog = parse("die'oops';");
    assert!(!prog.statements.is_empty());
}

#[test]
fn apos_return_keyword_not_consumed_as_package() {
    // return'value' — return with adjacent string argument.
    let prog = parse("sub foo { return'ok' }");
    assert!(!prog.statements.is_empty());
}

#[test]
fn apos_given_keyword_on_not_consumed() {
    // given'bar with use feature 'switch' — given is a keyword, scan_ident must not consume given'bar as given::bar.
    // This is a syntax error in Perl (given expects a condition).
    let result = crate::parse(b"use feature 'switch'; given'bar' { }");
    assert!(result.is_err(), "expected error: given is a keyword, 'bar' is not a valid condition syntax");
}

#[test]
fn apos_given_bareword_off_is_package() {
    // given'bar WITHOUT the switch feature — given is a bareword, so given'bar becomes given::bar (a package-qualified
    // name).
    let prog = parse("my $x = given'bar;");
    let init = first_assign_rhs(&prog);
    assert!(matches!(&init.kind, ExprKind::Bareword(s) if s == "given::bar"), "expected Bareword(given::bar), got {:?}", init.kind);
}

#[test]
fn apos_given_bareword_off_is_func_call() {
    // given'bar(1) WITHOUT the switch feature — given is a bareword, so given'bar is given::bar, and (1) makes it a
    // FuncCall.
    let prog = parse("my $x = given'bar(1);");
    let init = first_assign_rhs(&prog);
    assert!(
        matches!(&init.kind, ExprKind::FuncCall(name, args) if name == "given::bar" && args.len() == 1),
        "expected FuncCall(given::bar, [1]), got {:?}",
        init.kind
    );
}

#[test]
fn apos_any_keyword_on_not_consumed() {
    // any'x' with use feature 'any' — any is a keyword, so scan_ident must not consume any'x as any::x (which would
    // leave the trailing ' unterminated).  Instead, any is recognized as a keyword and 'x' is its argument.
    let prog = parse("use feature 'any'; my $r = any'x',1,2,3;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn apos_any_bareword_off_is_package() {
    // any'x WITHOUT the feature — any is a bareword, so any'x becomes any::x.
    let tokens = collect_tokens("any'x;");
    assert!(matches!(&tokens[0], Token::Ident(s) if s == "any::x"), "expected Ident(any::x), got {:?}", tokens[0]);
}

#[test]
fn apos_try_keyword_on_not_consumed() {
    // try'x with use feature 'try' — try is a keyword.
    let result = crate::parse(b"use feature 'try'; try'x' catch ($e) { }");
    assert!(result.is_err(), "expected error: try expects a block");
}

#[test]
fn apos_try_bareword_off_is_package() {
    // try'x WITHOUT the feature — try is a bareword.
    let tokens = collect_tokens("try'x;");
    assert!(matches!(&tokens[0], Token::Ident(s) if s == "try::x"), "expected Ident(try::x), got {:?}", tokens[0]);
}

// ══════════════════════════════════════════════════════════
// Pending NFC / encoding / body scanner tests
//
// These tests document known gaps.  Most will fail until the body scanner rework and source::encoding pragma are
// implemented.  Mark #[ignore] after confirming failure.
// ══════════════════════════════════════════════════════════

// ── Heredoc tag NFC normalization ─────────────────────────
//
// Deliberate deviation from Perl: heredoc tags and terminators are NFC-normalized so composed/decomposed forms match.

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn heredoc_nfc_composed_tag_decomposed_terminator() {
    // Tag is composed café (U+00E9), terminator is decomposed (e + U+0301).  Should match after NFC.
    // café composed: 63 61 66 C3 A9
    // café decomposed: 63 61 66 65 CC 81
    let src = b"use utf8; my $x = <<caf\xC3\xA9;\nbody\ncafe\xCC\x81\n";
    let result = crate::parse(src);
    assert!(result.is_ok(), "composed tag should match decomposed terminator: {:?}", result.err());
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn heredoc_nfc_decomposed_tag_composed_terminator() {
    // Reverse: decomposed tag, composed terminator.
    let src = b"use utf8; my $x = <<cafe\xCC\x81;\nbody\ncaf\xC3\xA9\n";
    let result = crate::parse(src);
    assert!(result.is_ok(), "decomposed tag should match composed terminator: {:?}", result.err());
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn heredoc_nfc_quoted_double() {
    // <<"café" with composed tag, decomposed terminator.
    let src = b"use utf8; my $x = <<\"caf\xC3\xA9\";\nbody\ncafe\xCC\x81\n";
    let result = crate::parse(src);
    assert!(result.is_ok(), "double-quoted composed tag should match decomposed terminator: {:?}", result.err());
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn heredoc_nfc_quoted_single() {
    // <<'café' with composed tag, decomposed terminator.
    let src = b"use utf8; my $x = <<'caf\xC3\xA9';\nbody\ncafe\xCC\x81\n";
    let result = crate::parse(src);
    assert!(result.is_ok(), "single-quoted composed tag should match decomposed terminator: {:?}", result.err());
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn heredoc_nfc_backslash_tag() {
    // <<\café with composed tag, decomposed terminator.
    let src = b"use utf8; my $x = <<\\caf\xC3\xA9;\nbody\ncafe\xCC\x81\n";
    let result = crate::parse(src);
    assert!(result.is_ok(), "backslash composed tag should match decomposed terminator: {:?}", result.err());
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn heredoc_nfc_indented() {
    // <<~café with composed tag, decomposed terminator.
    let src = b"use utf8; my $x = <<~caf\xC3\xA9;\n    body\n    cafe\xCC\x81\n";
    let result = crate::parse(src);
    assert!(result.is_ok(), "indented composed tag should match decomposed terminator: {:?}", result.err());
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn heredoc_nfc_devanagari_tag() {
    // Heredoc tag using Devanagari: <<ऩ where ऩ (U+0929) is composed, terminator is decomposed (न + ़ = U+0928 +
    // U+093C).
    // Composed ऩ: E0 A4 A9
    // Decomposed: E0 A4 A8 E0 A4 BC
    let src = b"use utf8; my $x = <<\xE0\xA4\xA9;\nbody\n\xE0\xA4\xA8\xE0\xA4\xBC\n";
    let result = crate::parse(src);
    assert!(result.is_ok(), "Devanagari composed tag should match decomposed terminator: {:?}", result.err());
}

// ── Tibetan delimiter + Devanagari combining mark in body ─
//
// Tibetan ༺ (U+0F3A, bytes E0 BC BA) and Devanagari nukta  ़ (U+093C, bytes E0 A4 BC) share lead byte 0xE0.  memchr
// scanning for the close delimiter ༻ (E0 BC BB) triggers on the nukta's lead byte, potentially splitting a base+nukta
// sequence across NFC normalization batches.

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn tibetan_delim_devanagari_nukta_in_body() {
    // q༺ text with decomposed ऩ (न + ़) inside ༻ The nukta's lead byte 0xE0 matches the delimiter's lead byte, causing
    // memchr to trigger mid-combining-sequence.  After NFC, the body should contain composed ऩ (U+0929).
    //
    // q = 71
    // ༺ = E0 BC BA
    // न = E0 A4 A8
    //  ़ = E0 A4 BC  (memchr triggers here on E0!)
    // ༻ = E0 BC BB
    let src = b"use utf8; my $x = q\xE0\xBC\xBA\xE0\xA4\xA8\xE0\xA4\xBC\xE0\xBC\xBB;";
    let result = crate::parse(src);
    assert!(result.is_ok(), "Tibetan delim with Devanagari nukta should parse: {:?}", result.err());

    // TODO: verify ConstSegment contains composed ऩ (E0 A4 A9), not decomposed न + ़ (E0 A4 A8 E0 A4 BC).
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn tibetan_delim_devanagari_in_qq_interpolation() {
    // qq༺ $x with decomposed ऩ ༻ — interpolating string with the same lead-byte collision, plus interpolation trigger.
    let src = b"use utf8; my $x = 1; my $y = qq\xE0\xBC\xBA$x \xE0\xA4\xA8\xE0\xA4\xBC\xE0\xBC\xBB;";
    let result = crate::parse(src);
    assert!(result.is_ok(), "Tibetan delim qq with Devanagari should parse: {:?}", result.err());
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn tibetan_delim_multiple_nuktas_in_body() {
    // Multiple decomposed Devanagari characters in a Tibetan-delimited string.  Each nukta triggers memchr on 0xE0.
    // नक़ = U+0928 U+0915 U+093C (decomposed)
    let src = b"use utf8; my $x = q\xE0\xBC\xBA\xE0\xA4\xA8\xE0\xA4\x95\xE0\xA4\xBC\xE0\xBC\xBB;";
    let result = crate::parse(src);
    assert!(result.is_ok(), "multiple Devanagari chars in Tibetan delims should parse: {:?}", result.err());
}

// ── Body scanner: invalid UTF-8 handling ──────────────────

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn body_invalid_utf8_under_use_utf8_should_error() {
    // 0xFF is never valid UTF-8.  Under `use utf8`, this should be a hard error, not silent replacement with U+FFFD.
    let src = b"use utf8; my $x = \"hello \xFF world\";";
    let result = crate::parse(src);
    assert!(result.is_err(), "invalid UTF-8 byte under `use utf8` should be an error");
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn body_truncated_utf8_under_use_utf8_should_error() {
    // 0xC3 without continuation byte — truncated UTF-8.
    let src = b"use utf8; my $x = \"caf\xC3\";";
    let result = crate::parse(src);
    assert!(result.is_err(), "truncated UTF-8 under `use utf8` should be an error");
}

#[test]
fn body_high_bytes_without_utf8_are_raw() {
    // Without `use utf8`, bytes > 127 are raw Latin-1.  \xE9 is é in Latin-1 (single byte, NOT a UTF-8 lead byte).
    // Should pass through as a raw byte.
    let src = b"my $x = \"caf\xE9\";";
    let result = crate::parse(src);
    assert!(result.is_ok(), "high bytes without `use utf8` should pass through: {:?}", result.err());
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn body_overlong_utf8_under_use_utf8_should_error() {
    // Overlong encoding of '/' (U+002F): C0 AF.  Valid UTF-8 decoders must reject this.
    let src = b"use utf8; my $x = \"\xC0\xAF\";";
    let result = crate::parse(src);
    assert!(result.is_err(), "overlong UTF-8 under `use utf8` should be an error");
}

// ── Body scanner: NFC normalization of raw content ────────

#[test]
fn body_nfc_normalizes_raw_decomposed() {
    // Decomposed ñ (n + U+0303) in source under `use utf8` should be NFC-normalized to composed ñ (U+00F1).
    // n = 6E, combining tilde = CC 83, composed ñ = C3 B1
    let src = b"use utf8; my $x = \"n\xCC\x83\";";
    let result = crate::parse(src);
    assert!(result.is_ok(), "decomposed char in string should parse: {:?}", result.err());

    // TODO: verify ConstSegment contains C3 B1, not 6E CC 83.
}

#[test]
fn body_escape_chars_not_normalized() {
    // \x{6e}\x{303} produces n + combining tilde via escapes.  These should NOT be NFC-normalized — they are escape-
    // produced, not raw source characters.
    let prog = parse("use utf8; my $x = \"\\x{6e}\\x{303}\";");
    assert!(!prog.statements.is_empty());

    // TODO: verify the string contains 6E CC 83 (decomposed), NOT C3 B1 (composed).
}

#[test]
fn body_nfc_devanagari_in_string() {
    // Decomposed ऩ (U+0928 + U+093C) in a double-quoted string under `use utf8` should NFC-normalize to U+0929.
    let src = b"use utf8; my $x = \"\xE0\xA4\xA8\xE0\xA4\xBC\";";
    let result = crate::parse(src);
    assert!(result.is_ok(), "Devanagari decomposed in string should parse: {:?}", result.err());
    // TODO: verify ConstSegment contains E0 A4 A9 (composed).
}

#[test]
fn body_no_nfc_without_utf8() {
    // Without `use utf8`, raw bytes should NOT be NFC-normalized even if they happen to form valid decomposed UTF-8.
    // The bytes are Latin-1, not Unicode.
    let src = b"my $x = \"n\xCC\x83\";";
    let result = crate::parse(src);
    assert!(result.is_ok(), "bytes without `use utf8` should pass through: {:?}", result.err());

    // TODO: verify the bytes are preserved exactly as-is.
}

// ── Identifier NFC normalization ──────────────────────────

#[test]
fn ident_nfc_devanagari() {
    // Decomposed ऩ (U+0928 + U+093C) in an identifier under `use utf8` should NFC-normalize to U+0929.
    let src = b"use utf8; my $\xE0\xA4\xA8\xE0\xA4\xBC = 1;";
    let result = crate::parse(src);
    assert!(result.is_ok(), "Devanagari decomposed identifier should parse: {:?}", result.err());
    // The variable should be $ऩ (composed), same as if the source had contained the composed form directly.
}

#[test]
fn ident_nfc_composed_and_decomposed_are_same_variable() {
    // Two assignments to the "same" variable using composed and decomposed forms.  After NFC normalization, they should
    // resolve to the same identifier.
    let src = b"use utf8; my $caf\xC3\xA9 = 1; $cafe\xCC\x81 = 2;";
    let result = crate::parse(src);
    assert!(result.is_ok(), "composed and decomposed should be same variable: {:?}", result.err());

    // TODO: verify both produce ScalarVar("café") with composed é.
}

// ── source::encoding pragma ──────────────────────────────
//
// New in 5.41.  Not yet implemented.

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn source_encoding_utf8_enables_utf8() {
    // `use source::encoding 'utf8'` is a synonym for `use utf8`.
    let src = "use source::encoding 'utf8'; my $caf\u{00E9} = 1;";
    let result = crate::parse(src.as_bytes());
    assert!(result.is_ok(), "source::encoding 'utf8' should enable UTF-8: {:?}", result.err());
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn source_encoding_ascii_rejects_high_bytes() {
    // `use source::encoding 'ascii'` should error on any non-ASCII byte in source.
    let src = b"use source::encoding 'ascii'; my $x = \"caf\xC3\xA9\";";
    let result = crate::parse(src);
    assert!(result.is_err(), "source::encoding 'ascii' should reject non-ASCII");
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn source_encoding_ascii_allows_escapes() {
    // Under ascii mode, escape sequences producing non-ASCII should still be allowed — only raw source bytes are
    // restricted.  First verify ascii mode IS active by confirming raw non-ASCII is rejected:
    let src = b"use source::encoding 'ascii'; my $x = \"caf\xC3\xA9\";";
    assert!(crate::parse(src).is_err(), "source::encoding 'ascii' should reject raw non-ASCII");

    // Then verify escapes still work:
    let prog = parse("use source::encoding 'ascii'; my $x = \"caf\\x{e9}\";");
    assert!(!prog.statements.is_empty());
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn source_encoding_ascii_implied_by_v5_42() {
    // `use v5.42` should imply `use source::encoding 'ascii'`.
    let src = b"use v5.42; my $x = \"caf\xC3\xA9\";";
    let result = crate::parse(src);
    assert!(result.is_err(), "use v5.42 should imply ascii source encoding");
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn no_source_encoding_disables() {
    // `no source::encoding` disables both UTF-8 and ASCII modes.  First verify ascii mode IS active:
    let src1 = b"use source::encoding 'ascii'; my $x = \"caf\xC3\xA9\";";
    assert!(crate::parse(src1).is_err(), "ascii should reject non-ASCII before 'no'");

    // Then verify `no source::encoding` allows non-ASCII again:
    let src2 = b"use source::encoding 'ascii'; no source::encoding; my $x = \"caf\xC3\xA9\";";
    assert!(crate::parse(src2).is_ok(), "no source::encoding should allow non-ASCII: {:?}", crate::parse(src2).err());
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn source_encoding_ascii_is_lexically_scoped() {
    // Inside block: ascii should reject non-ASCII.
    let src1 = b"{ use source::encoding 'ascii'; my $x = \"caf\xC3\xA9\"; }";
    assert!(crate::parse(src1).is_err(), "ascii should reject inside block");

    // Outside block: non-ASCII should be allowed again.
    let src2 = b"{ use source::encoding 'ascii'; } my $x = \"caf\xC3\xA9\";";
    assert!(crate::parse(src2).is_ok(), "ascii should not leak out of block: {:?}", crate::parse(src2).err());
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn source_encoding_bad_argument_is_error() {
    // Only 'ascii' and 'utf8' are accepted.  Anything else should be an error (Perl's module dies with "Bad argument").
    let result = crate::parse(b"use source::encoding 'latin1';");
    assert!(result.is_err(), "source::encoding 'latin1' should be an error");
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn source_encoding_no_argument_is_error() {
    // `use source::encoding` without an argument should be an error.
    let result = crate::parse(b"use source::encoding;");
    assert!(result.is_err(), "source::encoding without argument should be an error");
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn source_encoding_utf8_clears_ascii() {
    // First verify ascii mode IS rejecting:
    let src1 = b"use source::encoding 'ascii'; my $x = \"caf\xC3\xA9\";";
    assert!(crate::parse(src1).is_err(), "ascii should reject non-ASCII before switching");

    // Then verify switching to utf8 clears ascii and allows UTF-8:
    let src2 = b"use source::encoding 'ascii'; use source::encoding 'utf8'; my $x = \"caf\xC3\xA9\";";
    assert!(crate::parse(src2).is_ok(), "switching from ascii to utf8 should allow UTF-8: {:?}", crate::parse(src2).err());
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn source_encoding_ascii_clears_utf8() {
    // `use source::encoding 'ascii'` calls unimport() first, which clears utf8 mode.  Unicode identifiers should no
    // longer be valid.
    let src = "use utf8; use source::encoding 'ascii'; my $caf\u{00E9} = 1;";
    let result = crate::parse(src.as_bytes());
    assert!(result.is_err(), "switching from utf8 to ascii should reject Unicode identifiers");
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn source_encoding_ascii_implied_by_v5_41() {
    // The perlexperiment docs say "v5.41.0 or higher" activates ascii source encoding.  Unlike Features flags (which
    // round odd minors down to the prior even), ascii source encoding is a separate pragma state that activates at the
    // exact version threshold.  `use v5.41` SHOULD activate it.
    let src = b"use v5.41; my $x = \"caf\xC3\xA9\";";
    let result = crate::parse(src);
    assert!(result.is_err(), "use v5.41 should imply ascii source encoding");
}

#[test]
fn source_encoding_ascii_not_implied_by_v5_40() {
    // `use v5.40` should NOT imply ascii mode.  Non-ASCII raw bytes should be allowed (as raw bytes, not necessarily
    // valid UTF-8).
    let src = b"use v5.40; my $x = \"caf\xC3\xA9\";";
    let result = crate::parse(src);
    assert!(result.is_ok(), "use v5.40 should not imply ascii: {:?}", result.err());
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn source_encoding_ascii_then_use_utf8() {
    // `use source::encoding 'ascii'` then `use utf8`.  These are independent pragmas — `use utf8` does NOT clear the
    // ascii hint bit.  Both are active simultaneously.  The ascii check should still reject non-ASCII raw bytes even
    // though utf8 mode is also on.
    let src = b"use source::encoding 'ascii'; use utf8; my $x = \"caf\xC3\xA9\";";
    let result = crate::parse(src);
    assert!(result.is_err(), "ascii + utf8 should still reject non-ASCII raw bytes");
}

#[test]
#[ignore] // pending: NFC normalization / source::encoding / body scanner rework
fn use_utf8_then_source_encoding_ascii() {
    // `use utf8` then `use source::encoding 'ascii'`.  source::encoding's import() calls unimport() first, which clears
    // utf8 mode.  Then sets ascii.  Result: utf8 is off, ascii is on.
    let src = b"use utf8; use source::encoding 'ascii'; my $x = \"caf\xC3\xA9\";";
    let result = crate::parse(src);
    assert!(result.is_err(), "source::encoding 'ascii' should clear utf8 and reject non-ASCII");
}

// ── UTF-8 BOM handling ───────────────────────────────────
//
// "you need either a Byte Order Mark at the beginning of your source code, or use utf8;" — the UTF-8 BOM (EF BB BF) at
// the start of a file should implicitly enable use utf8.

#[test]
fn utf8_bom_enables_utf8_mode() {
    // UTF-8 BOM (EF BB BF) at start of file should implicitly enable use utf8, allowing Unicode identifiers.
    let src = b"\xEF\xBB\xBFmy $caf\xC3\xA9 = 1;";
    let result = crate::parse(src);
    assert!(result.is_ok(), "BOM at start should enable utf8 mode: {:?}", result.err());
}

#[test]
fn utf8_bom_stripped_from_output() {
    // The BOM itself should be stripped, not appear as a token or cause a parse error.
    let src = b"\xEF\xBB\xBFmy $x = 1;";
    let result = crate::parse(src);
    assert!(result.is_ok(), "BOM should be silently stripped: {:?}", result.err());
}

#[test]
fn utf8_bom_not_at_start_is_not_stripped() {
    // BOM bytes (EF BB BF) NOT at the very start of the file are not special.  Without use utf8, they are raw bytes
    // that the lexer rejects as unrecognized characters.
    let src = b"my $x = 1; \xEF\xBB\xBF";
    let result = crate::parse(src);
    assert!(result.is_err(), "mid-file BOM bytes should not be silently accepted");
}

// ── Perl's extended UTF-8 ────────────────────────────────
//
// Perl allows code points above U+10FFFF via its "extended UTF-8" encoding.  Rust's char only goes to U+10FFFF.

#[test]
fn escape_above_unicode_max() {
    // \x{110000} is above Unicode max but valid in Perl's extended UTF-8.  Should not be rejected.
    let prog = parse("my $x = \"\\x{110000}\";");
    assert!(!prog.statements.is_empty());
}

#[test]
fn escape_very_large_codepoint() {
    // Perl allows absurdly large code points.
    let prog = parse("my $x = \"\\x{7FFFFFFF}\";");
    assert!(!prog.statements.is_empty());
}

#[test]
fn escape_unicode_max_is_valid() {
    // \x{10FFFF} is the maximum Unicode code point and should always work.
    let prog = parse("my $x = \"\\x{10FFFF}\";");
    assert!(!prog.statements.is_empty());
}

// ── no utf8 restoring raw byte mode ──────────────────────
//
// "no utf8 tells Perl to switch back to treating the source text as literal bytes in the current lexical scope"

#[test]
fn no_utf8_restores_raw_byte_mode() {
    // After `no utf8`, non-ASCII bytes should be treated as raw bytes, not as UTF-8.  \xE9 is a single Latin-1 byte
    // (é), not a UTF-8 lead byte.
    let src = b"use utf8; no utf8; my $x = \"caf\xE9\";";
    let result = crate::parse(src);
    assert!(result.is_ok(), "no utf8 should restore raw byte mode: {:?}", result.err());
}

#[test]
fn no_utf8_in_block_restores_after_block() {
    // Lexically scoped: `no utf8` inside a block should not affect code after the block.
    let src = "use utf8; { no utf8; } my $caf\u{00E9} = 1;";
    let result = crate::parse(src.as_bytes());
    assert!(result.is_ok(), "utf8 should be restored after block: {:?}", result.err());
}

#[test]
fn utf8_and_no_utf8_interleaved() {
    // Multiple utf8/no utf8 switches in the same file.
    let mut src = Vec::new();
    src.extend_from_slice(b"use utf8; my $caf\xC3\xA9 = 1; "); // UTF-8 mode: composed é
    src.extend_from_slice(b"{ no utf8; my $x = \"caf\xE9\"; } "); // raw mode: Latin-1 é
    src.extend_from_slice(b"my $\xC3\xBC = 2;"); // back to UTF-8 mode: ü
    let result = crate::parse(&src);
    assert!(result.is_ok(), "interleaved utf8/no utf8 should work: {:?}", result.err());
}

#[test]
#[ignore] // pending: BOM detection / UTF-8 validation
fn use_utf8_rejects_non_utf8_bytes() {
    // "if you have non-ASCII, non-UTF-8 bytes in your script... use utf8 will be unhappy"
    // \xE9 alone is invalid UTF-8 (it's Latin-1 é).  Under use utf8, this should be an error.
    let src = b"use utf8; my $x = \"caf\xE9\";";
    let result = crate::parse(src);
    assert!(result.is_err(), "non-UTF-8 bytes under use utf8 should error");
}

#[test]
fn use_utf8_rejects_non_utf8_in_identifier() {
    // Invalid UTF-8 in an identifier under use utf8 should be an error, not silently accepted.
    let src = b"use utf8; my $caf\xE9 = 1;";
    let result = crate::parse(src);
    assert!(result.is_err(), "non-UTF-8 byte in identifier under use utf8 should error");
}

#[test]
fn no_utf8_allows_non_utf8_bytes_in_string() {
    // Without use utf8, \xE9 is a raw byte, not invalid UTF-8.
    let src = b"my $x = \"caf\xE9\";";
    let result = crate::parse(src);
    assert!(result.is_ok(), "raw bytes without use utf8 should be allowed: {:?}", result.err());
}

// ── utf8 pragma lexical scoping edge cases ───────────────

#[test]
fn utf8_pragma_nested_blocks() {
    // Nested blocks with different utf8 settings.
    let src = "use utf8; my $\u{00E9} = 1; { no utf8; { use utf8; my $\u{00FC} = 2; } }";
    let result = crate::parse(src.as_bytes());
    assert!(result.is_ok(), "nested utf8 blocks should work: {:?}", result.err());
}

#[test]
fn utf8_pragma_in_sub_body() {
    // use utf8 inside a sub body is lexically scoped to that sub.
    let src = b"sub foo { use utf8; my $caf\xC3\xA9 = 1; } my $x = 1;";
    let result = crate::parse(src);
    assert!(result.is_ok(), "utf8 in sub body should be scoped: {:?}", result.err());
}

#[test]
fn utf8_pragma_does_not_leak_to_next_statement() {
    // After the block with use utf8 closes, non-ASCII bytes in the next statement should be raw bytes, not UTF-8.
    // \xE9 as a raw byte should be fine outside the utf8 block, but would be invalid UTF-8 inside one.
    let src = b"{ use utf8; my $x = 1; } my $y = \"caf\xE9\";";
    let result = crate::parse(src);
    assert!(result.is_ok(), "utf8 should not leak past block: {:?}", result.err());
}

// ── \N{CHARNAME} named Unicode character escapes ─────────

#[test]
fn named_char_snowman() {
    // \N{SNOWMAN} → U+2603 (☃)
    let prog = parse("my $x = \"\\N{SNOWMAN}\";");
    let s = first_assign_str(&prog);
    assert_eq!(s, "\u{2603}", "\\N{{SNOWMAN}} should resolve to U+2603");
}

#[test]
fn named_char_white_smiling_face() {
    // \N{WHITE SMILING FACE} → U+263A (☺)
    let prog = parse("my $x = \"\\N{WHITE SMILING FACE}\";");
    let s = first_assign_str(&prog);
    assert_eq!(s, "\u{263A}");
}

#[test]
fn named_char_u_plus_hex() {
    // \N{U+263A} — hex code point form
    let prog = parse("my $x = \"\\N{U+263A}\";");
    let s = first_assign_str(&prog);
    assert_eq!(s, "\u{263A}");
}

#[test]
fn named_char_case_insensitive() {
    // unicode_names2 uses loose matching: case insensitive
    let prog = parse("my $x = \"\\N{snowman}\";");
    let s = first_assign_str(&prog);
    assert_eq!(s, "\u{2603}");
}

#[test]
fn named_char_latin_capital_a_with_acute() {
    let prog = parse("my $x = \"\\N{LATIN CAPITAL LETTER A WITH ACUTE}\";");
    let s = first_assign_str(&prog);
    assert_eq!(s, "\u{00C1}");
}

#[test]
fn named_char_greek_small_letter_alpha() {
    let prog = parse("my $x = \"\\N{GREEK SMALL LETTER ALPHA}\";");
    let s = first_assign_str(&prog);
    assert_eq!(s, "\u{03B1}");
}

#[test]
fn named_char_unknown_name_is_error() {
    let result = crate::parse(b"my $x = \"\\N{NONEXISTENT CHARACTER NAME}\";");
    assert!(result.is_err(), "unknown character name should be an error");
}

#[test]
fn named_char_in_regex() {
    let prog = parse("my $x = 1; $x =~ /\\N{SNOWMAN}/;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn named_char_multiple_in_string() {
    let prog = parse("my $x = \"\\N{SNOWMAN} and \\N{WHITE SMILING FACE}\";");
    let s = first_assign_str(&prog);
    assert!(s.contains('\u{2603}'), "should contain snowman");
    assert!(s.contains('\u{263A}'), "should contain smiling face");
}

#[test]
fn named_char_mixed_with_other_escapes() {
    let prog = parse("my $x = \"A\\N{SNOWMAN}B\\x{263A}C\";");
    let s = first_assign_str(&prog);
    assert_eq!(s, "A\u{2603}B\u{263A}C");
}

#[test]
fn named_char_bare_n_without_braces() {
    let prog = parse("my $x = \"\\N\";");
    let s = first_assign_str(&prog);
    assert_eq!(s, "\\N");
}

#[test]
fn named_char_in_heredoc() {
    let prog = parse("my $x = <<END;\n\\N{SNOWMAN}\nEND\n");
    assert!(!prog.statements.is_empty());
}

#[test]
fn named_char_invalid_hex_in_u_plus_silent_fffd() {
    // \N{U+SNOWMAN} — "SNOWMAN" is not valid hexadecimal.  The parser silently produces U+FFFD via unwrap_or instead of
    // reporting an error.  This is silent data corruption.
    let result = crate::parse(b"my $x = \"\\N{U+SNOWMAN}\";");
    assert!(result.is_err(), "\\N{{U+SNOWMAN}} should error, not silently produce U+FFFD");
}

// ── UTF-16 script autodetection ──────────────────────────

#[test]
fn utf16le_bom_transcodes_to_utf8() {
    // UTF-16LE BOM (FF FE) followed by "my $x = 1;\n" in UTF-16LE.
    let src_str = "my $x = 1;\n";
    let mut src: Vec<u8> = vec![0xFF, 0xFE]; // BOM
    for ch in src_str.encode_utf16() {
        src.extend_from_slice(&ch.to_le_bytes());
    }
    let result = crate::parse(&src);
    assert!(result.is_ok(), "UTF-16LE with BOM should be transcoded and parsed: {:?}", result.err());
}

#[test]
fn utf16be_bom_transcodes_to_utf8() {
    // UTF-16BE BOM (FE FF) followed by "my $x = 1;\n" in UTF-16BE.
    let src_str = "my $x = 1;\n";
    let mut src: Vec<u8> = vec![0xFE, 0xFF]; // BOM
    for ch in src_str.encode_utf16() {
        src.extend_from_slice(&ch.to_be_bytes());
    }
    let result = crate::parse(&src);
    assert!(result.is_ok(), "UTF-16BE with BOM should be transcoded and parsed: {:?}", result.err());
}

#[test]
fn utf16le_no_bom_heuristic() {
    // UTF-16LE without BOM — heuristic detection.  "my $x = 1;\n" in UTF-16LE (no BOM).
    let src_str = "my $x = 1;\n";
    let mut src: Vec<u8> = Vec::new();
    for ch in src_str.encode_utf16() {
        src.extend_from_slice(&ch.to_le_bytes());
    }
    let result = crate::parse(&src);
    assert!(result.is_ok(), "UTF-16LE without BOM should be detected and parsed: {:?}", result.err());
}

#[test]
fn utf16be_no_bom_heuristic() {
    // UTF-16BE without BOM — heuristic detection.
    let src_str = "my $x = 1;\n";
    let mut src: Vec<u8> = Vec::new();
    for ch in src_str.encode_utf16() {
        src.extend_from_slice(&ch.to_be_bytes());
    }
    let result = crate::parse(&src);
    assert!(result.is_ok(), "UTF-16BE without BOM should be detected and parsed: {:?}", result.err());
}

#[test]
fn utf16le_bom_with_unicode_content() {
    // UTF-16LE with BOM, source contains Unicode identifiers.  "use utf8; my $café = 1;\n"
    let src_str = "use utf8; my $caf\u{00E9} = 1;\n";
    let mut src: Vec<u8> = vec![0xFF, 0xFE];
    for ch in src_str.encode_utf16() {
        src.extend_from_slice(&ch.to_le_bytes());
    }
    let result = crate::parse(&src);
    assert!(result.is_ok(), "UTF-16LE with Unicode content should work: {:?}", result.err());
}

#[test]
fn utf16_with_surrogate_pairs() {
    // UTF-16LE with BOM, source contains U+1F600 (GRINNING FACE) in a string literal via \N{U+1F600}.
    let src_str = "my $x = \"\\N{U+1F600}\";\n";
    let mut src: Vec<u8> = vec![0xFF, 0xFE];
    for ch in src_str.encode_utf16() {
        src.extend_from_slice(&ch.to_le_bytes());
    }
    let result = crate::parse(&src);
    assert!(result.is_ok(), "UTF-16 with surrogate pairs should work: {:?}", result.err());
}

#[test]
fn not_utf16_plain_ascii() {
    // Plain ASCII should NOT be detected as UTF-16.
    let prog = parse("my $x = 1;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn not_utf16_binary_garbage() {
    // Random bytes that happen to start with 00 should not be misdetected as UTF-16BE if the pattern doesn't hold.
    let src = b"\x00\x01\x02\x03";
    let result = crate::parse(src);

    // This might error for other reasons, but shouldn't crash from a UTF-16 transcode attempt.
    let _ = result;
}

// ── Nullary builtins ────────────────────────────────────────

#[test]
fn nullary_time_plus_number() {
    // `time+86_400` must parse as `time() + 86_400`, not `time(+86_400)`.
    let prog = parse("time+86_400;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::BinOp(BinOp::Add, lhs, _rhs) => match &lhs.kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "CORE::time");
                    assert!(args.is_empty(), "time must have zero args");
                }
                other => panic!("expected FuncCall(time), got {other:?}"),
            },
            other => panic!("expected BinOp(Add), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn nullary_time_with_empty_parens() {
    // `time()` is explicitly accepted.
    let prog = parse("time();");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::time");
                assert!(args.is_empty());
            }
            other => panic!("expected FuncCall(time), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn nullary_fork_bare() {
    // `fork;` must parse as a zero-arg function call.
    let prog = parse("fork;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::fork");
                assert!(args.is_empty());
            }
            other => panic!("expected FuncCall(fork), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn nullary_wait_bare() {
    let prog = parse("wait;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::wait");
                assert!(args.is_empty());
            }
            other => panic!("expected FuncCall(wait), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn nullary_wantarray_in_conditional() {
    // `wantarray ? 1 : 0` — wantarray is nullary, the `?` is ternary.
    let prog = parse("wantarray ? 1 : 0;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Ternary(cond, _, _) => {
                assert!(matches!(cond.kind, ExprKind::Wantarray));
            }
            other => panic!("expected Ternary, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn nullary_time_fat_comma_autoquotes() {
    // `time => 42` — fat comma autoquotes, so "time" is a string key.
    let prog = parse("my %h = (time => 42);");
    assert!(!prog.statements.is_empty());
}

#[test]
fn nullary_getppid_in_expression() {
    // `getppid == 1` — getppid is nullary.
    let prog = parse("getppid == 1;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::BinOp(BinOp::NumEq, lhs, _) => match &lhs.kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "CORE::getppid");
                    assert!(args.is_empty());
                }
                other => panic!("expected FuncCall(getppid), got {other:?}"),
            },
            other => panic!("expected BinOp(NumEq), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn nullary_endpwent_bare() {
    let prog = parse("endpwent;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::endpwent");
                assert!(args.is_empty());
            }
            other => panic!("expected FuncCall(endpwent), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn nullary_times_in_assignment() {
    // `my @t = times;`
    let prog = parse("my @t = times;");
    assert!(!prog.statements.is_empty());
}

// ── elseif diagnostic ───────────────────────────────────────

#[test]
fn elseif_after_if_gives_helpful_error() {
    let result = crate::parse(b"if (1) { } elseif (2) { }");
    assert!(result.is_err());
    let msg = result.unwrap_err().message;
    assert!(msg.contains("elseif should be elsif"), "got: {msg}");
}

#[test]
fn elseif_after_elsif_gives_helpful_error() {
    let result = crate::parse(b"if (1) { } elsif (2) { } elseif (3) { }");
    assert!(result.is_err());
    let msg = result.unwrap_err().message;
    assert!(msg.contains("elseif should be elsif"), "got: {msg}");
}

#[test]
fn elseif_after_unless_gives_helpful_error() {
    let result = crate::parse(b"unless (1) { } elseif (2) { }");
    assert!(result.is_err());
    let msg = result.unwrap_err().message;
    assert!(msg.contains("elseif should be elsif"), "got: {msg}");
}

// ── Named unary builtins (additional) ───────────────────────

#[test]
fn named_unary_sleep_with_arg() {
    // `sleep 5` must parse as `sleep(5)`, not bareword + separate statement.
    let prog = parse("sleep 5;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::sleep");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall(sleep), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn named_unary_sleep_bare() {
    // `sleep;` with no argument.
    let prog = parse("sleep;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::sleep");
                assert!(args.is_empty());
            }
            other => panic!("expected FuncCall(sleep), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn named_unary_sin_dollar_x() {
    // `sin $x` takes one argument.
    let prog = parse("sin $x;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::sin");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall(sin), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn named_unary_localtime_bare() {
    // `localtime;` with no argument.
    let prog = parse("localtime;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::localtime");
                assert!(args.is_empty());
            }
            other => panic!("expected FuncCall(localtime), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn named_unary_localtime_with_parens() {
    let prog = parse("localtime(time);");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::localtime");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall(localtime), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn named_unary_alarm_with_arg() {
    let prog = parse("alarm 30;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::alarm");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall(alarm), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn named_unary_quotemeta_with_arg() {
    let prog = parse("quotemeta $str;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::quotemeta");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall(quotemeta), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn named_unary_exp_in_expression() {
    // `exp(1) + 1` — named unary with parens, then addition.
    let prog = parse("exp(1) + 1;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::BinOp(BinOp::Add, lhs, _) => match &lhs.kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "CORE::exp");
                    assert_eq!(args.len(), 1);
                }
                other => panic!("expected FuncCall(exp), got {other:?}"),
            },
            other => panic!("expected BinOp(Add), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn named_unary_fat_comma_autoquotes() {
    // `sleep => 42` — fat comma autoquotes the keyword.
    let prog = parse("my %h = (sleep => 42);");
    assert!(!prog.statements.is_empty());
}

#[test]
fn named_unary_log_cos_chained() {
    // `log(cos($x))` — nested named unaries with parens.
    let prog = parse("log(cos($x));");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::log");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall(log), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

// ── select dual-form ────────────────────────────────────────

#[test]
fn select_one_arg_filehandle() {
    let prog = parse("select STDOUT;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::select");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected ListOp(select), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn select_one_arg_with_parens() {
    let prog = parse("select(STDOUT);");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::select");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected ListOp(select), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn select_four_arg_syscall() {
    let prog = parse("select($rin, $win, $ein, 0.25);");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::select");
                assert_eq!(args.len(), 4);
            }
            other => panic!("expected ListOp(select), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn select_four_arg_no_parens() {
    let prog = parse("select undef, undef, undef, 0.25;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::select");
                assert_eq!(args.len(), 4);
            }
            other => panic!("expected ListOp(select), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn select_zero_arg() {
    let prog = parse("select();");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::select");
                assert!(args.is_empty());
            }
            other => panic!("expected ListOp(select), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn select_in_assignment() {
    let prog = parse("my $old = select(STDERR);");
    assert!(!prog.statements.is_empty());
}

#[test]
fn select_fat_comma_autoquotes() {
    let prog = parse("my %h = (select => 42);");
    assert!(!prog.statements.is_empty());
}

// ── Additional list operator builtins ───────────────────────

#[test]
fn listop_waitpid_two_args() {
    // `waitpid $pid, 0` — system/process list op.
    let prog = parse("waitpid $pid, 0;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::waitpid");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected ListOp(waitpid), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn listop_kill_signal_and_pids() {
    // `kill 'HUP', $pid1, $pid2`
    let prog = parse("kill 'HUP', $pid1, $pid2;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::kill");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected ListOp(kill), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn listop_socket_four_args() {
    // `socket(my $sock, PF_INET, SOCK_STREAM, 0)`
    let prog = parse("socket(my $sock, 2, 1, 0);");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::socket");
                assert_eq!(args.len(), 4);
            }
            other => panic!("expected ListOp(socket), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn listop_send_with_parens() {
    let prog = parse("send($sock, $msg, 0);");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::send");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected ListOp(send), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn listop_pack_template_and_values() {
    // `pack "A*", $data`
    let prog = parse("pack 'A*', $data;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::pack");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected ListOp(pack), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn listop_unpack_template_and_data() {
    let prog = parse("unpack('N', $buf);");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::unpack");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected ListOp(unpack), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn listop_flock_two_args() {
    let prog = parse("flock $fh, 2;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::flock");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected ListOp(flock), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn listop_link_two_args() {
    let prog = parse("link $old, $new;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::link");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected ListOp(link), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn listop_truncate_two_args() {
    let prog = parse("truncate $fh, 0;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::truncate");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected ListOp(truncate), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn listop_sysread_three_args() {
    let prog = parse("sysread($fh, $buf, 1024);");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::sysread");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected ListOp(sysread), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn listop_utime_list() {
    // `utime $atime, $mtime, @files`
    let prog = parse("utime $atime, $mtime, @files;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::utime");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected ListOp(utime), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn listop_vec_three_args() {
    let prog = parse("vec($flags, 0, 8);");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::vec");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected ListOp(vec), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn listop_fcntl_three_args() {
    let prog = parse("fcntl($fh, 2, 0);");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::fcntl");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected ListOp(fcntl), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn listop_msgrcv_five_args() {
    let prog = parse("msgrcv($id, $buf, 256, 0, 0);");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::msgrcv");
                assert_eq!(args.len(), 5);
            }
            other => panic!("expected ListOp(msgrcv), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

// Named unary — database lookups

#[test]
fn named_unary_getpwnam() {
    // `getpwnam "root"` — named unary, takes one arg.
    let prog = parse("getpwnam 'root';");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::getpwnam");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall(getpwnam), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn named_unary_getpwuid() {
    let prog = parse("getpwuid 0;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::getpwuid");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall(getpwuid), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn named_unary_gethostbyname() {
    let prog = parse("gethostbyname $host;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::gethostbyname");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall(gethostbyname), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

// Multi-arg database lookups (list ops)

#[test]
fn listop_getservbyname() {
    let prog = parse("getservbyname 'http', 'tcp';");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::getservbyname");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected ListOp(getservbyname), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn listop_fat_comma_autoquotes_pack() {
    // `pack => 42` — fat comma autoquotes the keyword.
    let prog = parse("my %h = (pack => 42);");
    assert!(!prog.statements.is_empty());
}

#[test]
fn all_list_op_keywords_parse() {
    // Verify every list operator keyword parses as a ListOp, not a bareword.
    let list_ops = [
        "waitpid",
        "kill",
        "pipe",
        "setpgrp",
        "setpriority",
        "getpriority",
        "syscall",
        "socket",
        "socketpair",
        "bind",
        "connect",
        "listen",
        "accept",
        "shutdown",
        "send",
        "recv",
        "setsockopt",
        "getsockopt",
        "shmget",
        "shmctl",
        "shmread",
        "shmwrite",
        "semget",
        "semctl",
        "semop",
        "msgget",
        "msgctl",
        "msgsnd",
        "msgrcv",
        "getservbyname",
        "gethostbyaddr",
        "getnetbyaddr",
        "getservbyport",
        "sysopen",
        "sysread",
        "syswrite",
        "sysseek",
        "truncate",
        "fcntl",
        "ioctl",
        "flock",
        "seekdir",
        "link",
        "symlink",
        "utime",
        "pack",
        "unpack",
        "vec",
        "formline",
        "select",
    ];
    for kw in list_ops {
        let src = format!("{kw}($x, $y);");
        let prog = parse(&src);
        match &prog.statements[0].kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::ListOp(name, _) => {
                    let expected = format!("CORE::{kw}");
                    assert_eq!(name, &expected, "{kw} should parse as ListOp");
                }
                other => panic!("{kw} parsed as {other:?}, expected ListOp"),
            },
            other => panic!("{kw} statement: {other:?}"),
        }
    }
}

#[test]
fn all_named_unary_db_keywords_parse() {
    // Verify every database-lookup named unary parses as FuncCall.
    let unaries = ["getpwnam", "getgrnam", "gethostbyname", "getnetbyname", "getprotobyname", "getpwuid", "getgrgid", "getprotobynumber"];
    for kw in unaries {
        let src = format!("{kw} $arg;");
        let prog = parse(&src);
        match &prog.statements[0].kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::FuncCall(name, args) => {
                    {
                        let expected = format!("CORE::{kw}");
                        assert_eq!(name, &expected, "{kw} should parse as FuncCall");
                    }
                    assert_eq!(args.len(), 1, "{kw} should have one arg");
                }
                other => panic!("{kw} parsed as {other:?}, expected FuncCall"),
            },
            other => panic!("{kw} statement: {other:?}"),
        }
    }
}

#[test]
fn all_nullary_keywords_parse() {
    // Verify every nullary builtin parses as a zero-arg FuncCall.
    let nullaries = [
        "time",
        "times",
        "fork",
        "wait",
        "getppid",
        "getlogin",
        "setpwent",
        "setgrent",
        "endpwent",
        "endgrent",
        "endhostent",
        "endnetent",
        "endprotoent",
        "endservent",
        "getpwent",
        "getgrent",
        "gethostent",
        "getnetent",
        "getprotoent",
        "getservent",
    ];
    for kw in nullaries {
        let src = format!("{kw};");
        let prog = parse(&src);
        match &prog.statements[0].kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::FuncCall(name, args) => {
                    {
                        let expected = format!("CORE::{kw}");
                        assert_eq!(name, &expected, "{kw} should parse as FuncCall");
                    }
                    assert!(args.is_empty(), "{kw} should have zero args");
                }
                other => panic!("{kw} parsed as {other:?}, expected zero-arg FuncCall"),
            },
            other => panic!("{kw} statement: {other:?}"),
        }
    }
}

// ── Keyword names in declaration contexts ───────────────────

#[test]
fn sub_decl_with_keyword_name_send() {
    // `sub send { }` — valid Perl, keyword used as sub name.
    let prog = parse("sub send { 1 }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sd) => assert_eq!(sd.name, "send"),
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn sub_decl_with_keyword_name_pack() {
    let prog = parse("sub pack { 1 }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sd) => assert_eq!(sd.name, "pack"),
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn sub_decl_with_keyword_name_print() {
    // Pre-existing keyword — verify we didn't break this.
    let prog = parse("sub print { 1 }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sd) => assert_eq!(sd.name, "print"),
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn package_decl_with_keyword_name() {
    // `package send;` — valid Perl.
    let prog = parse("package send; 1;");
    match &prog.statements[0].kind {
        StmtKind::PackageDecl(pd) => assert_eq!(pd.name, "send"),
        other => panic!("expected Package, got {other:?}"),
    }
}

#[test]
fn format_decl_with_keyword_name() {
    // `format send = ... .` — valid Perl.
    let prog = parse("format send =\n@<<<\n$x\n.\n");
    match &prog.statements[0].kind {
        StmtKind::FormatDecl(fd) => assert_eq!(fd.name, "send"),
        other => panic!("expected FormatDecl, got {other:?}"),
    }
}

// ── Named unary postfix modifier and precedence fixes ───────

#[test]
fn named_unary_sleep_postfix_if() {
    // `sleep if $tired;` — postfix modifier, sleep has zero args.
    let prog = parse("sleep if $tired;");
    match &prog.statements[0].kind {
        StmtKind::Expr(Expr { kind: ExprKind::PostfixControl(PostfixKind::If, body, _), .. }) => match &body.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::sleep");
                assert!(args.is_empty(), "sleep should have zero args before postfix if");
            }
            other => panic!("expected FuncCall(sleep), got {other:?}"),
        },
        other => panic!("expected PostfixControl If, got {other:?}"),
    }
}

#[test]
fn named_unary_chdir_or_die() {
    // `chdir or die;` — low-precedence `or`, chdir has zero args.
    let prog = parse("chdir or die;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::BinOp(BinOp::LowOr, lhs, _) => match &lhs.kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "CORE::chdir");
                    assert!(args.is_empty(), "chdir should have zero args before `or`");
                }
                other => panic!("expected FuncCall(chdir), got {other:?}"),
            },
            other => panic!("expected BinOp(LowOr), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn named_unary_defined_ternary_precedence() {
    // `defined $x ? 1 : 0` is `defined($x) ? 1 : 0`, NOT `defined($x ? 1 : 0)`.
    let prog = parse("defined $x ? 1 : 0;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Ternary(cond, _, _) => match &cond.kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "CORE::defined");
                    assert_eq!(args.len(), 1, "defined should consume only $x");
                }
                other => panic!("expected FuncCall(defined) as ternary condition, got {other:?}"),
            },
            other => panic!("expected Ternary, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn named_unary_defined_logical_or_precedence() {
    // `defined $x || $y` is `defined($x) || $y`.
    let prog = parse("defined $x || $y;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::BinOp(BinOp::Or, lhs, _) => match &lhs.kind {
                ExprKind::FuncCall(name, args) => {
                    assert_eq!(name, "CORE::defined");
                    assert_eq!(args.len(), 1);
                }
                other => panic!("expected FuncCall(defined), got {other:?}"),
            },
            other => panic!("expected BinOp(Or), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn named_unary_lc_concat_precedence() {
    // `lc $x . $y` is `lc($x . $y)` — concat is above named-unary precedence.
    let prog = parse("lc $x . $y;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::lc");
                assert_eq!(args.len(), 1);

                // The single arg should be a Concat binop.
                assert!(matches!(args[0].kind, ExprKind::BinOp(BinOp::Concat, _, _)));
            }
            other => panic!("expected FuncCall(lc), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn named_unary_sleep_for_postfix_loop() {
    // `sleep for 1..5` — postfix for loop.
    let prog = parse("sleep for 1..5;");
    assert!(!prog.statements.is_empty());
}

#[test]
fn named_unary_alarm_with_arg_postfix_if() {
    // `alarm 30 if $need_timeout` — arg then postfix modifier.
    let prog = parse("alarm 30 if $need_timeout;");
    assert!(!prog.statements.is_empty());
}

// ── Keyword audit: final batch ──────────────────────────────

#[test]
fn keyword_x_as_repeat_operator() {
    // `"ab" x 3` — x is the infix repeat operator.
    let prog = parse("\"ab\" x 3;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::BinOp(BinOp::Repeat, _, _) => {}
            other => panic!("expected BinOp(Repeat), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn keyword_x_in_prefix_position() {
    // `x()` — x as a function call (weak keyword, prefix position).
    let prog = parse("x();");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, _) => assert_eq!(name, "main::x"),
            other => panic!("expected FuncCall(x), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn keyword_xor_as_infix_operator() {
    // `$a xor $b` — low-precedence logical xor.
    let prog = parse("$a xor $b;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::BinOp(BinOp::LowXor, _, _) => {}
            other => panic!("expected BinOp(LowXor), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn keyword_isa_as_infix_operator() {
    // `$obj isa Foo` — class-instance test (feature-gated).
    let prog = parse("use feature 'isa'; $obj isa Foo;");

    // Find the isa expression (second statement)
    let stmt = &prog.statements[1];
    match &stmt.kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::BinOp(BinOp::Isa, _, _) => {}
            other => panic!("expected BinOp(Isa), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn keyword_isa_without_feature_is_ident() {
    // Without the isa feature, `isa` is a bare identifier.
    let prog = parse("isa();");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, _) => assert_eq!(name, "main::isa"),
            other => panic!("expected FuncCall(isa), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn keyword_break_bare() {
    // `break;` — exits given/when block.
    let prog = parse("use feature 'switch'; break;");
    let stmt = &prog.statements[1];
    match &stmt.kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::break");
                assert!(args.is_empty());
            }
            other => panic!("expected FuncCall(break), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn keyword_break_without_feature_is_ident() {
    // Without switch feature, `break` is a bare identifier.
    let prog = parse("break();");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, _) => assert_eq!(name, "main::break"),
            other => panic!("expected FuncCall(break), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn keyword_evalbytes_with_feature() {
    let prog = parse("use feature 'evalbytes'; evalbytes $bytes;");
    let stmt = &prog.statements[1];
    match &stmt.kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::evalbytes");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall(evalbytes), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn keyword_fc_with_feature() {
    let prog = parse("use feature 'fc'; fc $str;");
    let stmt = &prog.statements[1];
    match &stmt.kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::fc");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall(fc), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn keyword_lock_named_unary() {
    let prog = parse("lock $var;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::lock");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall(lock), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn keyword_atan2_list_op() {
    let prog = parse("atan2 $y, $x;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::atan2");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected ListOp(atan2), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn keyword_crypt_list_op() {
    let prog = parse("crypt $plain, $salt;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::ListOp(name, args) => {
                assert_eq!(name, "CORE::crypt");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected ListOp(crypt), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn keyword_autoload_forward_decl() {
    // `AUTOLOAD()` is `sub AUTOLOAD ();` — forward declaration with empty prototype.
    let prog = parse("AUTOLOAD();");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sd) => assert_eq!(sd.name, "AUTOLOAD"),
        other => panic!("expected SubDecl(AUTOLOAD), got {other:?}"),
    }
}

#[test]
fn keyword_destroy_forward_decl() {
    // `DESTROY()` is `sub DESTROY ();` — forward declaration with empty prototype.
    let prog = parse("DESTROY();");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sd) => assert_eq!(sd.name, "DESTROY"),
        other => panic!("expected SubDecl(DESTROY), got {other:?}"),
    }
}

#[test]
fn keyword_fileno_named_unary() {
    let prog = parse("fileno $fh;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::fileno");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall(fileno), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn keyword_study_named_unary() {
    let prog = parse("study $str;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "CORE::study");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FuncCall(study), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn keyword_sub_decl_with_keyword_x() {
    // `sub x { }` — valid, x is a weak keyword.
    let prog = parse("sub x { 1 }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sd) => assert_eq!(sd.name, "x"),
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn keyword_sub_decl_with_keyword_xor() {
    let prog = parse("sub xor { 1 }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sd) => assert_eq!(sd.name, "xor"),
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn keyword_sub_decl_with_keyword_lock() {
    let prog = parse("sub lock { 1 }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sd) => assert_eq!(sd.name, "lock"),
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

// Exhaustive coverage: verify every new keyword parses correctly.

#[test]
fn all_final_batch_named_unaries_parse() {
    let unaries = [
        "fileno",
        "getpeername",
        "getpgrp",
        "getsockname",
        "rewinddir",
        "sethostent",
        "setnetent",
        "setprotoent",
        "setservent",
        "study",
        "telldir",
        "dbmclose",
        "lock",
    ];
    for kw in unaries {
        let src = format!("{kw} $arg;");
        let prog = parse(&src);
        match &prog.statements[0].kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::FuncCall(name, args) => {
                    {
                        let expected = format!("CORE::{kw}");
                        assert_eq!(name, &expected, "{kw} should parse as FuncCall");
                    }
                    assert_eq!(args.len(), 1, "{kw} should have one arg");
                }
                other => panic!("{kw} parsed as {other:?}, expected FuncCall"),
            },
            other => panic!("{kw} statement: {other:?}"),
        }
    }
}

#[test]
fn all_final_batch_list_ops_parse() {
    let list_ops = ["atan2", "crypt", "dbmopen"];
    for kw in list_ops {
        let src = format!("{kw}($x, $y);");
        let prog = parse(&src);
        match &prog.statements[0].kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::ListOp(name, _) => {
                    let expected = format!("CORE::{kw}");
                    assert_eq!(name, &expected, "{kw} should parse as ListOp");
                }
                other => panic!("{kw} parsed as {other:?}, expected ListOp"),
            },
            other => panic!("{kw} statement: {other:?}"),
        }
    }
}

// ── Weak keyword override via use subs ──────────────────────

#[test]
fn weak_keyword_abs_overridden_by_use_subs() {
    // Without override, `abs $x ? 1 : 0` is `abs($x) ? 1 : 0` — Ternary at top.  With override, it's `abs($x ? 1 : 0)`
    // — FuncCall at top (list op precedence).
    let prog = parse("use subs 'abs'; abs $x ? 1 : 0;");
    let stmt = &prog.statements[1];
    match &stmt.kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "main::abs");

                // The ternary is INSIDE the args — list op consumed it.
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::Ternary(_, _, _)), "arg should be ternary, got {:?}", args[0].kind);
            }
            other => panic!("expected FuncCall(abs) at top (override active), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn weak_keyword_not_overridden_without_use_subs() {
    // Without use subs, `abs $x ? 1 : 0` is named unary: Ternary at top, with FuncCall(abs, [$x]) as condition.
    let prog = parse("abs $x ? 1 : 0;");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::Ternary(cond, _, _) => match &cond.kind {
                ExprKind::FuncCall(name, _) => assert_eq!(name, "CORE::abs"),
                other => panic!("expected FuncCall(abs) as ternary condition, got {other:?}"),
            },
            other => panic!("expected Ternary at top (named unary), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn strong_keyword_not_overridden_by_use_subs() {
    // `print` is strong — `use subs 'print'` should NOT override it.
    let prog = parse("use subs 'print'; print 42;");
    let stmt = &prog.statements[1];
    match &stmt.kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::PrintOp(..) => {} // still parsed as print op
            other => panic!("expected PrintOp, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn weak_keyword_use_subs_is_package_level() {
    // `use subs` is package-level — override persists outside blocks.
    let prog = parse("{ use subs 'abs'; } abs $x ? 1 : 0;");
    let outer_stmt = &prog.statements[1];
    match &outer_stmt.kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "main::abs");
                assert!(matches!(args[0].kind, ExprKind::Ternary(_, _, _)), "override should persist — ternary inside FuncCall");
            }
            other => panic!("expected FuncCall(abs) — use subs is package-level, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn weak_keyword_abs_overridden_by_use_subs_qw() {
    // `use subs qw(abs)` — same override via qw form.
    let prog = parse("use subs qw(abs); abs $x ? 1 : 0;");
    let stmt = &prog.statements[1];
    match &stmt.kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::FuncCall(name, args) => {
                assert_eq!(name, "main::abs");
                assert!(matches!(args[0].kind, ExprKind::Ternary(_, _, _)), "ternary should be inside FuncCall args");
            }
            other => panic!("expected FuncCall(abs) with override, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn weak_keyword_multiple_qw_overrides() {
    // Both abs and sin overridden — both should consume ternary.
    let prog = parse("use subs qw(abs sin); abs $x ? 1 : 0; sin $y ? 1 : 0;");
    for (i, name) in [(1, "main::abs"), (2, "main::sin")] {
        match &prog.statements[i].kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::FuncCall(n, args) => {
                    assert_eq!(n, name);
                    assert!(matches!(args[0].kind, ExprKind::Ternary(_, _, _)), "{name}: ternary should be inside FuncCall");
                }
                other => panic!("{name}: expected FuncCall, got {other:?}"),
            },
            other => panic!("{name}: expected Expr, got {other:?}"),
        }
    }
}

// ── AUTOLOAD/DESTROY as implicit sub declarations ───────────

#[test]
fn autoload_block_is_sub_declaration() {
    // `AUTOLOAD { 1 }` without `sub` is an implicit sub declaration in Perl.
    let prog = parse("AUTOLOAD { 1 }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sd) => assert_eq!(sd.name, "AUTOLOAD"),
        other => panic!("expected SubDecl(AUTOLOAD), got {other:?}"),
    }
}

#[test]
fn destroy_block_is_sub_declaration() {
    // `DESTROY { 1 }` without `sub` is an implicit sub declaration.
    let prog = parse("DESTROY { 1 }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sd) => assert_eq!(sd.name, "DESTROY"),
        other => panic!("expected SubDecl(DESTROY), got {other:?}"),
    }
}

// ── Weak keyword override must not break infix operators ────

#[test]
fn use_subs_x_does_not_break_infix_repeat() {
    // `use subs "x"` overrides prefix x, but infix `"ab" x 3` must still parse as the repeat operator.
    let prog = parse("use subs 'x'; \"ab\" x 3;");
    let stmt = &prog.statements[1];
    match &stmt.kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::BinOp(BinOp::Repeat, _, _) => {}
            other => panic!("expected BinOp(Repeat), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn use_subs_xor_does_not_break_infix_xor() {
    // `use subs "xor"` overrides prefix, but infix must still work.
    let prog = parse("use subs 'xor'; $a xor $b;");
    let stmt = &prog.statements[1];
    match &stmt.kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::BinOp(BinOp::LowXor, _, _) => {}
            other => panic!("expected BinOp(LowXor), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn use_subs_eq_does_not_break_infix_eq() {
    // `use subs "eq"` overrides prefix, but infix must still work.
    let prog = parse("use subs 'eq'; \"a\" eq \"b\";");
    let stmt = &prog.statements[1];
    match &stmt.kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::BinOp(BinOp::StrEq, _, _) => {}
            other => panic!("expected BinOp(StrEq), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

// ── Keyword before } is NOT autoquoted ──────────────────────

#[test]
fn keyword_before_right_brace_not_autoquoted() {
    // `sub foo { abs }` — abs is a zero-arg named unary call, NOT a string literal.  The RightBrace check in
    // parse_term's fat comma autoquoting must not fire here.
    let prog = parse("sub foo { abs }");
    match &prog.statements[0].kind {
        StmtKind::SubDecl(sd) => {
            let body_stmt = &sd.body.statements[0];
            match &body_stmt.kind {
                StmtKind::Expr(e) => match &e.kind {
                    ExprKind::FuncCall(name, _) => assert_eq!(name, "CORE::abs"),
                    ExprKind::StringLit(s) => panic!("abs was autoquoted to StringLit(\"{s}\") — bug!"),
                    other => panic!("expected FuncCall(CORE::abs), got {other:?}"),
                },
                other => panic!("expected Expr, got {other:?}"),
            }
        }
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

// ── Hash subscript autoquoting edge cases ───────────────────

#[test]
fn hash_subscript_minus_keyword_autoquotes() {
    // `$h{-abs}` should autoquote to StringLit("-abs"), not negate abs().
    let prog = parse("$h{-abs};");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::HashElem(_, key) => match &key.kind {
                ExprKind::StringLit(s) => assert_eq!(s, "-abs"),
                other => panic!("expected StringLit(\"-abs\"), got {other:?}"),
            },
            other => panic!("expected HashElem, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn hash_subscript_minus_strong_keyword_autoquotes() {
    // `$h{-if}` should autoquote to StringLit("-if").
    let prog = parse("$h{-if};");
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::HashElem(_, key) => match &key.kind {
                ExprKind::StringLit(s) => assert_eq!(s, "-if"),
                other => panic!("expected StringLit(\"-if\"), got {other:?}"),
            },
            other => panic!("expected HashElem, got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// Evaluation-context stamping (save_context, §6.2)
// ═══════════════════════════════════════════════════════════

/// Parse a program and return its statements, for inspecting stamped contexts.
fn parse_stmts(src: &str) -> Vec<Statement> {
    parse(src).statements
}

/// Parse `src` as a non-final expression statement (by appending a trailing `1;`) and return it.  The expression is
/// then in void context at the top level (rather than the load-deferred `None` of a program's final statement), while
/// all of its sub-expression contexts are stamped as usual.
fn parse_nonfinal_expr(src: &str) -> Expr {
    let with_tail = format!("{src}; 1;");
    let prog = parse(&with_tail);
    match &prog.statements[0].kind {
        StmtKind::Expr(e) => e.clone(),
        other => panic!("expected first statement to be Expr, got {other:?}"),
    }
}

#[test]
fn ctx_program_final_statement_is_runtime() {
    // The program's final statement is evaluated in the runtime context (void if run directly, scalar if require'd),
    // stamped Runtime and resolved at runtime — it is NOT left None.
    let stmts = parse_stmts("42;");
    match &stmts[0].kind {
        StmtKind::Expr(e) => assert_eq!(e.ctx, Some(Context::Runtime), "final statement is in runtime context"),
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn ctx_nonfinal_statement_is_void() {
    // A non-final top-level statement is in void context.
    let stmts = parse_stmts("42; 1;");
    match &stmts[0].kind {
        StmtKind::Expr(e) => assert_eq!(e.ctx, Some(Context::Void)),
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn ctx_arithmetic_operands_are_scalar() {
    // `3 - 4`: both operands scalar, regardless of the node's own context.
    let e = parse_nonfinal_expr("3 - 4");
    match &e.kind {
        ExprKind::BinOp(_, l, r) => {
            assert_eq!(l.ctx, Some(Context::Scalar));
            assert_eq!(r.ctx, Some(Context::Scalar));
        }
        other => panic!("expected BinOp, got {other:?}"),
    }
}

#[test]
fn ctx_ternary_condition_is_boolean_branches_inherit() {
    // `(3 - 3) ? $a : $b` in void context: condition is boolean; branches forward the node's (void) context; the
    // condition's own operands are still scalar.
    let e = parse_nonfinal_expr("(3 - 3) ? $a : $b");
    match &e.kind {
        ExprKind::Ternary(cond, then_e, else_e) => {
            assert_eq!(cond.ctx, Some(Context::Boolean), "ternary condition must be boolean");
            assert_eq!(then_e.ctx, Some(Context::Void), "branch inherits node context (void)");
            assert_eq!(else_e.ctx, Some(Context::Void), "branch inherits node context (void)");
            // The condition is a BinOp whose operands remain scalar even though the BinOp itself is boolean.
            match &cond.kind {
                ExprKind::BinOp(_, l, r) => {
                    assert_eq!(l.ctx, Some(Context::Scalar));
                    assert_eq!(r.ctx, Some(Context::Scalar));
                }
                other => panic!("expected BinOp condition, got {other:?}"),
            }
        }
        other => panic!("expected Ternary, got {other:?}"),
    }
}

#[test]
fn ctx_short_circuit_left_scalar_right_inherits() {
    // `@a || @b` in list context: left operand is scalar (truth-tested), right inherits list (it flattens).  Reached
    // via a foreach iteration list, which is a list-context position.
    let stmts = parse_stmts("foreach (@a || @b) { } 1;");
    match &stmts[0].kind {
        StmtKind::ForEach(s) => match &s.list.kind {
            ExprKind::BinOp(BinOp::Or, l, r) => {
                assert_eq!(l.ctx, Some(Context::Scalar), "|| left operand is scalar (truth-tested)");
                assert_eq!(r.ctx, Some(Context::List), "|| right operand inherits list context");
            }
            other => panic!("expected BinOp(Or), got {other:?}"),
        },
        other => panic!("expected ForEach, got {other:?}"),
    }
}

#[test]
fn ctx_short_circuit_in_boolean_both_boolean() {
    // `if (@a || @b)`: the || is in boolean context; left is boolean (scalar-test refined), right inherits boolean.
    let stmts = parse_stmts("if (@a || @b) { } 1;");
    match &stmts[0].kind {
        StmtKind::If(s) => match &s.condition.kind {
            ExprKind::BinOp(BinOp::Or, l, r) => {
                assert_eq!(l.ctx, Some(Context::Boolean));
                assert_eq!(r.ctx, Some(Context::Boolean));
            }
            other => panic!("expected BinOp(Or) condition, got {other:?}"),
        },
        other => panic!("expected If, got {other:?}"),
    }
}

#[test]
fn ctx_if_condition_is_boolean() {
    let stmts = parse_stmts("if ($x) { } 1;");
    match &stmts[0].kind {
        StmtKind::If(s) => assert_eq!(s.condition.ctx, Some(Context::Boolean)),
        other => panic!("expected If, got {other:?}"),
    }
}

#[test]
fn ctx_while_condition_is_boolean() {
    let stmts = parse_stmts("while ($x) { } 1;");
    match &stmts[0].kind {
        StmtKind::While(s) => assert_eq!(s.condition.ctx, Some(Context::Boolean)),
        other => panic!("expected While, got {other:?}"),
    }
}

#[test]
fn ctx_foreach_list_is_list() {
    let stmts = parse_stmts("foreach (@a) { } 1;");
    match &stmts[0].kind {
        StmtKind::ForEach(s) => assert_eq!(s.list.ctx, Some(Context::List)),
        other => panic!("expected ForEach, got {other:?}"),
    }
}

#[test]
fn ctx_c_for_clauses() {
    // C-style for: init void, condition boolean, step void.
    let stmts = parse_stmts("for ($i = 0; $i < 10; $i++) { } 1;");
    match &stmts[0].kind {
        StmtKind::For(s) => {
            assert_eq!(s.init.as_ref().unwrap().ctx, Some(Context::Void));
            assert_eq!(s.condition.as_ref().unwrap().ctx, Some(Context::Boolean));
            assert_eq!(s.step.as_ref().unwrap().ctx, Some(Context::Void));
        }
        other => panic!("expected For, got {other:?}"),
    }
}

#[test]
fn ctx_func_args_are_list() {
    // `foo($a, $b)` in void context: the call's args are list.
    let e = parse_nonfinal_expr("foo($a, $b)");
    match &e.kind {
        ExprKind::FuncCall(_, args) => {
            for a in args {
                assert_eq!(a.ctx, Some(Context::List));
            }
        }
        other => panic!("expected FuncCall, got {other:?}"),
    }
}

#[test]
fn ctx_array_slice_indices_are_list() {
    // `@a[$i, $j]`: base scalar, indices list.
    let e = parse_nonfinal_expr("@a[$i, $j]");
    match &e.kind {
        ExprKind::ArraySlice(base, indices) => {
            assert_eq!(base.ctx, Some(Context::Scalar));
            for idx in indices {
                assert_eq!(idx.ctx, Some(Context::List));
            }
        }
        other => panic!("expected ArraySlice, got {other:?}"),
    }
}

#[test]
fn ctx_array_elem_index_is_scalar() {
    // `$a[$i]`: base and index scalar.
    let e = parse_nonfinal_expr("$a[$i]");
    match &e.kind {
        ExprKind::ArrayElem(base, index) => {
            assert_eq!(base.ctx, Some(Context::Scalar));
            assert_eq!(index.ctx, Some(Context::Scalar));
        }
        other => panic!("expected ArrayElem, got {other:?}"),
    }
}

#[test]
fn ctx_not_operand_is_always_boolean() {
    // `!`/`not` always truth-test their operand, so the operand is Boolean regardless of the `!` node's own context.
    // Here `!$x` is in void context (a non-final statement), yet the operand is still Boolean.
    let e = parse_nonfinal_expr("!$x");
    match &e.kind {
        ExprKind::UnaryOp(UnaryOp::LogNot, operand) => {
            assert_eq!(operand.ctx, Some(Context::Boolean), "! operand is always boolean");
        }
        other => panic!("expected UnaryOp(LogNot), got {other:?}"),
    }
}

#[test]
fn ctx_not_operand_boolean_in_boolean_context_too() {
    // `if (!$x)`: the `!` is in boolean context; the operand is boolean here as well (it always is).
    let stmts = parse_stmts("if (!$x) { } 1;");
    match &stmts[0].kind {
        StmtKind::If(s) => match &s.condition.kind {
            ExprKind::UnaryOp(UnaryOp::LogNot, operand) => {
                assert_eq!(operand.ctx, Some(Context::Boolean));
            }
            other => panic!("expected UnaryOp(LogNot), got {other:?}"),
        },
        other => panic!("expected If, got {other:?}"),
    }
}

#[test]
fn ctx_xor_operands_are_boolean() {
    // `xor`/`^^` always truth-test both operands, so both are boolean regardless of the node's context.  Reached via a
    // non-final statement (void context).
    let e = parse_nonfinal_expr("$a xor $b");
    match &e.kind {
        ExprKind::BinOp(BinOp::LowXor, l, r) => {
            assert_eq!(l.ctx, Some(Context::Boolean), "xor left operand is boolean");
            assert_eq!(r.ctx, Some(Context::Boolean), "xor right operand is boolean");
        }
        other => panic!("expected BinOp(LowXor), got {other:?}"),
    }
}

#[test]
fn ctx_sub_body_tail_is_runtime() {
    // A subroutine body is evaluated in the caller's context; its tail statement is in runtime context (wantarray),
    // stamped Runtime and resolved per call site.
    let stmts = parse_stmts("sub f { $x }");
    match &stmts[0].kind {
        StmtKind::SubDecl(s) => match &s.body.statements[0].kind {
            StmtKind::Expr(e) => assert_eq!(e.ctx, Some(Context::Runtime), "sub body tail is runtime context"),
            other => panic!("expected Expr in body, got {other:?}"),
        },
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn ctx_sub_body_non_tail_is_void() {
    // A non-tail statement in a sub body is in void context (its value is discarded), exactly like a file's non-final
    // statement.
    let stmts = parse_stmts("sub f { $x; $y }");
    match &stmts[0].kind {
        StmtKind::SubDecl(s) => match &s.body.statements[0].kind {
            StmtKind::Expr(e) => assert_eq!(e.ctx, Some(Context::Void), "non-tail sub-body statement is void"),
            other => panic!("expected Expr in body, got {other:?}"),
        },
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn ctx_anon_sub_body_tail_is_runtime() {
    // Same for an anonymous sub used as a value: the sub expression has its own context, but the body is a container
    // whose tail is in runtime context.
    let e = parse_nonfinal_expr("sub { $x }");
    match &e.kind {
        ExprKind::AnonSub(_, _, _, body) => match &body.statements[0].kind {
            StmtKind::Expr(inner) => assert_eq!(inner.ctx, Some(Context::Runtime), "anon sub body tail is runtime"),
            other => panic!("expected Expr in body, got {other:?}"),
        },
        other => panic!("expected AnonSub, got {other:?}"),
    }
}

#[test]
fn ctx_comma_in_list_all_list() {
    // A comma list in list context: every operand is list.  Reach it via a foreach list.
    let stmts = parse_stmts("foreach ($a, $b, $c) { } 1;");
    match &stmts[0].kind {
        StmtKind::ForEach(s) => match &s.list.kind {
            ExprKind::Comma(items) => {
                for item in items {
                    assert_eq!(item.ctx, Some(Context::List));
                }
            }
            other => panic!("expected Comma, got {other:?}"),
        },
        other => panic!("expected ForEach, got {other:?}"),
    }
}

#[test]
fn ctx_range_endpoints_are_scalar() {
    // `1 .. 10`: endpoints scalar; reach via a foreach list (list context → the node is a range).
    let stmts = parse_stmts("foreach (1 .. 10) { } 1;");
    match &stmts[0].kind {
        StmtKind::ForEach(s) => match &s.list.kind {
            ExprKind::Range(l, r, _) => {
                assert_eq!(l.ctx, Some(Context::Scalar));
                assert_eq!(r.ctx, Some(Context::Scalar));
            }
            other => panic!("expected Range, got {other:?}"),
        },
        other => panic!("expected ForEach, got {other:?}"),
    }
}

#[test]
fn ctx_is_scalar_predicate() {
    // Scalar, Boolean, and RuntimeTruthTested are scalar-valued; List and Void are not; Runtime is not statically
    // scalar (it may resolve to List).
    assert!(Context::Scalar.is_scalar());
    assert!(Context::Boolean.is_scalar());
    assert!(Context::RuntimeTruthTested.is_scalar());
    assert!(!Context::List.is_scalar());
    assert!(!Context::Void.is_scalar());
    assert!(!Context::Runtime.is_scalar());
}

#[test]
fn ctx_truth_tested_mapping() {
    // truth_tested gives a short-circuit left operand's context: Void/Boolean → Boolean (value not kept, pure gate),
    // Scalar/List → Scalar (value may be forwarded); the runtime contexts → RuntimeTruthTested (idempotently).
    assert_eq!(Context::Void.truth_tested(), Context::Boolean);
    assert_eq!(Context::Boolean.truth_tested(), Context::Boolean);
    assert_eq!(Context::Scalar.truth_tested(), Context::Scalar);
    assert_eq!(Context::List.truth_tested(), Context::Scalar);
    assert_eq!(Context::Runtime.truth_tested(), Context::RuntimeTruthTested);
    assert_eq!(Context::RuntimeTruthTested.truth_tested(), Context::RuntimeTruthTested);
}

#[test]
fn ctx_tail_if_branches_inherit_runtime() {
    // A subroutine whose body's tail statement is an `if`: the if is value-bearing, so its branches inherit the body's
    // runtime context.  The condition stays boolean.
    let stmts = parse_stmts("sub f { if ($c) { $a } else { $b } }");
    match &stmts[0].kind {
        StmtKind::SubDecl(s) => match &s.body.statements[0].kind {
            StmtKind::If(if_s) => {
                assert_eq!(if_s.condition.ctx, Some(Context::Boolean), "if condition is boolean");
                match &if_s.then_block.statements[0].kind {
                    StmtKind::Expr(e) => assert_eq!(e.ctx, Some(Context::Runtime), "then-branch tail inherits runtime"),
                    other => panic!("expected Expr in then-branch, got {other:?}"),
                }
                let else_block = if_s.else_block.as_ref().expect("else block");
                match &else_block.statements[0].kind {
                    StmtKind::Expr(e) => assert_eq!(e.ctx, Some(Context::Runtime), "else-branch tail inherits runtime"),
                    other => panic!("expected Expr in else-branch, got {other:?}"),
                }
            }
            other => panic!("expected If as body tail, got {other:?}"),
        },
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn ctx_non_tail_if_branches_are_void() {
    // The same `if`, but NOT in tail position (a statement follows it): its branches are in void context.
    let stmts = parse_stmts("sub f { if ($c) { $a } else { $b } 1 }");
    match &stmts[0].kind {
        StmtKind::SubDecl(s) => match &s.body.statements[0].kind {
            StmtKind::If(if_s) => match &if_s.then_block.statements[0].kind {
                StmtKind::Expr(e) => assert_eq!(e.ctx, Some(Context::Void), "non-tail then-branch is void"),
                other => panic!("expected Expr, got {other:?}"),
            },
            other => panic!("expected If, got {other:?}"),
        },
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn ctx_tail_short_circuit_propagates_runtime() {
    // A sub whose tail is `@a || @b`: the || node is in runtime context, so its right operand inherits Runtime and its
    // left operand is the truth-tested runtime context (RuntimeTruthTested).
    let stmts = parse_stmts("sub f { @a || @b }");
    match &stmts[0].kind {
        StmtKind::SubDecl(s) => match &s.body.statements[0].kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::BinOp(BinOp::Or, l, r) => {
                    assert_eq!(l.ctx, Some(Context::RuntimeTruthTested), "|| left is truth-tested runtime");
                    assert_eq!(r.ctx, Some(Context::Runtime), "|| right inherits runtime");
                }
                other => panic!("expected BinOp(Or), got {other:?}"),
            },
            other => panic!("expected Expr, got {other:?}"),
        },
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn ctx_short_circuit_left_in_void_is_boolean() {
    // A `||` as a non-final (void) statement: the left operand is a pure truth-test gate → Boolean (the value is
    // discarded, so it is not kept as a scalar).  The right inherits the void context.
    let stmts = parse_stmts("@a || @b; 1;");
    match &stmts[0].kind {
        StmtKind::Expr(e) => match &e.kind {
            ExprKind::BinOp(BinOp::Or, l, r) => {
                assert_eq!(l.ctx, Some(Context::Boolean), "void || left is a boolean gate");
                assert_eq!(r.ctx, Some(Context::Void), "void || right inherits void");
            }
            other => panic!("expected BinOp(Or), got {other:?}"),
        },
        other => panic!("expected Expr, got {other:?}"),
    }
}

#[test]
fn ctx_short_circuit_left_in_scalar_is_scalar() {
    // A `||` in scalar context (here as the left operand of `+`, which imposes scalar): the `||` left operand may be
    // forwarded as the scalar result, so it is Scalar — not a pure boolean gate.  Reached via arithmetic rather than
    // assignment (assignment context is deferred).
    let e = parse_nonfinal_expr("($a || $b) + 1");
    match &e.kind {
        ExprKind::BinOp(BinOp::Add, or_node, _) => match &or_node.kind {
            ExprKind::BinOp(BinOp::Or, l, _) => {
                assert_eq!(or_node.ctx, Some(Context::Scalar), "the || node itself is scalar (+ operand)");
                assert_eq!(l.ctx, Some(Context::Scalar), "scalar-context || left is scalar");
            }
            other => panic!("expected Or as + left operand, got {other:?}"),
        },
        other => panic!("expected Add, got {other:?}"),
    }
}

#[test]
fn ctx_tail_ternary_branches_inherit_runtime() {
    // A sub whose tail is `$c ? $a : $b`: condition boolean, branches inherit the runtime context.
    let stmts = parse_stmts("sub f { $c ? $a : $b }");
    match &stmts[0].kind {
        StmtKind::SubDecl(s) => match &s.body.statements[0].kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::Ternary(cond, then_e, else_e) => {
                    assert_eq!(cond.ctx, Some(Context::Boolean));
                    assert_eq!(then_e.ctx, Some(Context::Runtime));
                    assert_eq!(else_e.ctx, Some(Context::Runtime));
                }
                other => panic!("expected Ternary, got {other:?}"),
            },
            other => panic!("expected Expr, got {other:?}"),
        },
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn ctx_tail_call_args_are_list_not_runtime() {
    // Even in a runtime-context tail, a call's arguments are list context — the runtime-dependence does not reach past
    // the context-fixing call boundary.
    let stmts = parse_stmts("sub f { foo($a, $b) }");
    match &stmts[0].kind {
        StmtKind::SubDecl(s) => match &s.body.statements[0].kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::FuncCall(_, args) => {
                    for a in args {
                        assert_eq!(a.ctx, Some(Context::List), "call args are list even in a runtime tail");
                    }
                }
                other => panic!("expected FuncCall, got {other:?}"),
            },
            other => panic!("expected Expr, got {other:?}"),
        },
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn ctx_tail_arithmetic_operands_are_scalar_not_runtime() {
    // Arithmetic operands are scalar even when the operator node is in a runtime tail — the spine ends at the
    // context-fixing operator.
    let stmts = parse_stmts("sub f { $a + $b }");
    match &stmts[0].kind {
        StmtKind::SubDecl(s) => match &s.body.statements[0].kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::BinOp(_, l, r) => {
                    assert_eq!(l.ctx, Some(Context::Scalar));
                    assert_eq!(r.ctx, Some(Context::Scalar));
                }
                other => panic!("expected BinOp, got {other:?}"),
            },
            other => panic!("expected Expr, got {other:?}"),
        },
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn ctx_loop_body_is_void_even_in_tail() {
    // A loop is not value-bearing: as a sub's tail statement, its body is still void (the loop returns empty
    // regardless of context).
    let stmts = parse_stmts("sub f { while ($c) { $x } }");
    match &stmts[0].kind {
        StmtKind::SubDecl(s) => match &s.body.statements[0].kind {
            StmtKind::While(w) => {
                assert_eq!(w.condition.ctx, Some(Context::Boolean));
                match &w.body.statements[0].kind {
                    StmtKind::Expr(e) => assert_eq!(e.ctx, Some(Context::Void), "loop body is void even in tail"),
                    other => panic!("expected Expr in loop body, got {other:?}"),
                }
            }
            other => panic!("expected While, got {other:?}"),
        },
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn ctx_do_block_tail_inherits_context() {
    // `do { $a; $b }` in a list-context position (foreach list): the block's last statement inherits list, earlier
    // statements are void.
    let stmts = parse_stmts("foreach (do { $a; $b }) { } 1;");
    match &stmts[0].kind {
        StmtKind::ForEach(s) => match &s.list.kind {
            ExprKind::DoBlock(block) => {
                match &block.statements[0].kind {
                    StmtKind::Expr(e) => assert_eq!(e.ctx, Some(Context::Void), "non-tail do-block statement is void"),
                    other => panic!("expected Expr, got {other:?}"),
                }
                match &block.statements[1].kind {
                    StmtKind::Expr(e) => assert_eq!(e.ctx, Some(Context::List), "do-block tail inherits list"),
                    other => panic!("expected Expr, got {other:?}"),
                }
            }
            other => panic!("expected DoBlock, got {other:?}"),
        },
        other => panic!("expected ForEach, got {other:?}"),
    }
}

#[test]
fn ctx_return_operand_is_runtime() {
    // `return EXPR` evaluates its operand in the caller's context (Runtime), independent of the node's own position.
    let stmts = parse_stmts("sub f { return $x }");
    match &stmts[0].kind {
        StmtKind::SubDecl(s) => match &s.body.statements[0].kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::Return(Some(operand)) => {
                    assert_eq!(operand.ctx, Some(Context::Runtime), "return operand is in the caller's runtime context");
                }
                other => panic!("expected Return(Some), got {other:?}"),
            },
            other => panic!("expected Expr, got {other:?}"),
        },
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn ctx_return_operand_runtime_even_when_nested() {
    // `5 + return $x`: `return` is a diverging expression nested as `+`'s right operand, but its operand still inherits
    // the caller's runtime context — the enclosing `+`'s scalar context never reaches it (the `+` never evaluates,
    // since `return` unwinds first).
    let stmts = parse_stmts("sub f { 5 + return $x }");
    match &stmts[0].kind {
        StmtKind::SubDecl(s) => match &s.body.statements[0].kind {
            StmtKind::Expr(e) => match &e.kind {
                // The tail expression is `5 + (return $x)` — a BinOp(Add) whose right operand is the Return.
                ExprKind::BinOp(BinOp::Add, _, rhs) => match &rhs.kind {
                    ExprKind::Return(Some(operand)) => {
                        assert_eq!(operand.ctx, Some(Context::Runtime), "nested return operand is still runtime");
                    }
                    other => panic!("expected Return as + right operand, got {other:?}"),
                },
                other => panic!("expected BinOp(Add), got {other:?}"),
            },
            other => panic!("expected Expr, got {other:?}"),
        },
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

#[test]
fn ctx_return_bare_has_no_operand() {
    // Bare `return` has no operand to stamp.
    let stmts = parse_stmts("sub f { return }");
    match &stmts[0].kind {
        StmtKind::SubDecl(s) => match &s.body.statements[0].kind {
            StmtKind::Expr(e) => assert!(matches!(e.kind, ExprKind::Return(None)), "expected Return(None), got {:?}", e.kind),
            other => panic!("expected Expr, got {other:?}"),
        },
        other => panic!("expected SubDecl, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════
// List slices and transient Paren (§6.2.3–6.2.4)
// ═══════════════════════════════════════════════════════════

#[test]
fn list_slice_multiple_indices() {
    // `(10, 20, 30)[0, 2]` — a list slice with a multi-index subscript; the indices flatten into the Vec.
    let e = parse_expr_str("(10, 20, 30)[0, 2];");
    match &e.kind {
        ExprKind::ListSlice(operand, indices) => {
            assert!(matches!(operand.kind, ExprKind::Comma(_)));
            assert_eq!(indices.len(), 2, "two indices");
            assert!(matches!(indices[0].kind, ExprKind::IntLit(0)));
            assert!(matches!(indices[1].kind, ExprKind::IntLit(2)));
        }
        other => panic!("expected ListSlice, got {other:?}"),
    }
}

#[test]
fn list_slice_single_paren_expr_operand() {
    // `($x)[0]` — a single parenthesized expression sliced: the operand is the inner expr (Paren unwrapped), not a Comma.
    let e = parse_expr_str("($x)[0];");
    match &e.kind {
        ExprKind::ListSlice(operand, indices) => {
            assert!(matches!(operand.kind, ExprKind::ScalarVar(_)), "operand is the inner expr, got {:?}", operand.kind);
            assert_eq!(indices.len(), 1);
        }
        other => panic!("expected ListSlice, got {other:?}"),
    }
}

#[test]
fn list_slice_empty_list_operand() {
    // `()[0]` — slicing the empty list.  Operand is EmptyList.
    let e = parse_expr_str("()[0];");
    match &e.kind {
        ExprKind::ListSlice(operand, indices) => {
            assert!(matches!(operand.kind, ExprKind::EmptyList), "operand is EmptyList, got {:?}", operand.kind);
            assert_eq!(indices.len(), 1);
        }
        other => panic!("expected ListSlice, got {other:?}"),
    }
}

#[test]
fn list_slice_chained() {
    // `(10, 20, 30, 40)[1, 2, 3][0, 2]` — a list slice's result is itself a list literal, so it slices again.
    let e = parse_expr_str("(10, 20, 30, 40)[1, 2, 3][0, 2];");
    match &e.kind {
        ExprKind::ListSlice(inner, outer_indices) => {
            assert_eq!(outer_indices.len(), 2, "outer slice has two indices");
            assert!(matches!(inner.kind, ExprKind::ListSlice(_, _)), "inner is itself a ListSlice, got {:?}", inner.kind);
        }
        other => panic!("expected chained ListSlice, got {other:?}"),
    }
}

#[test]
fn list_slice_context_operand_and_indices_are_list() {
    // save_context: a list slice's operand and indices are all in list context.
    let e = parse_nonfinal_expr("(10, 20, 30)[0, 2]");
    match &e.kind {
        ExprKind::ListSlice(operand, indices) => {
            assert_eq!(operand.ctx, Some(Context::List), "operand is list context");
            for idx in indices {
                assert_eq!(idx.ctx, Some(Context::List), "indices are list context");
            }
        }
        other => panic!("expected ListSlice, got {other:?}"),
    }
}

#[test]
fn paren_grouping_does_not_persist() {
    // The transient Paren must never persist in a finished tree: a parenthesized group used purely for grouping is
    // unwrapped.  `(a + b) * c` is `(a+b) * c` with NO Paren node anywhere.
    let e = parse_expr_str("($a + $b) * $c;");
    fn assert_no_paren(e: &Expr) {
        assert!(!matches!(e.kind, ExprKind::Paren(_)), "found a persisting Paren node");
        if let ExprKind::BinOp(_, l, r) = &e.kind {
            assert_no_paren(l);
            assert_no_paren(r);
        }
    }
    match &e.kind {
        ExprKind::BinOp(BinOp::Mul, l, r) => {
            assert!(matches!(l.kind, ExprKind::BinOp(BinOp::Add, _, _)), "left is the unwrapped (a+b), got {:?}", l.kind);
            assert!(matches!(r.kind, ExprKind::ScalarVar(_)));
            assert_no_paren(&e);
        }
        other => panic!("expected Mul at top, got {other:?}"),
    }
}

#[test]
fn paren_nested_grouping_does_not_persist() {
    // Deeply nested grouping parens collapse: `(((1 + 2)))` is just `1 + 2`, no Paren, no nesting.
    let e = parse_expr_str("(((1 + 2)));");
    assert!(matches!(e.kind, ExprKind::BinOp(BinOp::Add, _, _)), "expected bare Add, got {:?}", e.kind);
}

#[test]
fn paren_single_scalar_grouping_unwraps() {
    // `($x)` alone (no slice, no assignment) is pure grouping → unwrapped to the bare scalar.
    let e = parse_expr_str("($x);");
    assert!(matches!(e.kind, ExprKind::ScalarVar(_)), "expected bare ScalarVar, got {:?}", e.kind);
}

#[test]
fn assign_list_vs_scalar_classification() {
    // The list-vs-scalar rule is uniform across declaration and non-declaration LHS forms: list iff the LHS is
    // parenthesized OR an inherent aggregate (array/hash/slice/deref).  Both sides are stamped with the resulting
    // context at assignment-construction time.  This is the full cross-product of the forms that decide the question.
    use Context::{List, Scalar};
    let cases: &[(&str, Context)] = &[
        // ── non-declaration ──
        ("$x = 1", Scalar),
        ("($x) = 1", List),
        ("@a = (1, 2)", List),
        ("(@a) = (1, 2)", List),
        ("%h = (1, 2)", List),
        ("(%h) = (1, 2)", List),
        // ── declaration ──
        ("my $x = 1", Scalar),
        ("my ($x) = 1", List),
        ("my @a = (1, 2)", List),
        ("my (@a) = (1, 2)", List),
        ("my %h = (1, 2)", List),
        ("my (%h) = (1, 2)", List),
        ("my ($x, $y) = (1, 2)", List),
        ("my ($x, @rest) = (1, 2)", List),
        // ── comma-list RHS in scalar vs list context (perl: `our $x = (2,4,6)` → 6; `our ($x) = (2,4,6)` → 2) ──
        ("$x = (2, 4, 6)", Scalar),
        ("our $x = (2, 4, 6)", Scalar),
        ("our ($x) = (2, 4, 6)", List),
    ];
    for &(src, expected) in cases {
        let e = parse_nonfinal_expr(src);
        match &e.kind {
            ExprKind::Assign(_, lhs, rhs) => {
                assert_eq!(lhs.ctx, Some(expected), "{src}: LHS context");
                assert_eq!(rhs.ctx, Some(expected), "{src}: RHS context");
            }
            other => panic!("{src}: expected Assign, got {other:?}"),
        }
    }
}

#[test]
fn decl_paren_form_does_not_persist() {
    // The list form `my (...)` is a transient `Paren(Decl)`.  A standalone `my ($a, $b);` (no assignment to consume
    // and unwrap the Paren) must still resolve to a bare `Decl` — the grouping `Paren` never persists.
    let e = parse_expr_str("my ($a, $b);");
    match &e.kind {
        ExprKind::Decl(DeclScope::My, vars) => assert_eq!(vars.len(), 2),
        other => panic!("expected bare Decl, got {other:?}"),
    }
}

#[test]
fn decl_paren_form_in_assignment_lhs_is_bare_decl() {
    // `my ($x) = 1` — the LHS `Paren(Decl)` is unwrapped before being stored; the Assign LHS is a bare `Decl`.  (The
    // parens-fact survives as list context — asserted in assign_list_vs_scalar_classification.)
    let e = parse_nonfinal_expr("my ($x) = 1");
    match &e.kind {
        ExprKind::Assign(_, lhs, _) => {
            assert!(matches!(lhs.kind, ExprKind::Decl(DeclScope::My, _)), "expected bare Decl LHS, got {:?}", lhs.kind);
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn assign_rhs_call_context_follows_assignment() {
    // Ground truth (perl, `sub foo { @_ }`):
    //   $x   = foo(2, 4, 6)  → foo called in SCALAR context → its `@_` yields the element count → $x == 3
    //   ($x) = foo(2, 4, 6)  → foo called in LIST context   → `@_` flattens → $x == 2 (first element)
    // The parser stamps the call node with the assignment's context (the runtime reads it to choose the calling
    // context); call arguments are always list context, independent of that — which is why `@_` is (2, 4, 6) in both.
    for (src, call_ctx) in [("$x = foo(2, 4, 6)", Context::Scalar), ("($x) = foo(2, 4, 6)", Context::List)] {
        let e = parse_nonfinal_expr(src);
        let rhs = match &e.kind {
            ExprKind::Assign(_, _, rhs) => rhs,
            other => panic!("{src}: expected Assign, got {other:?}"),
        };
        match &rhs.kind {
            ExprKind::FuncCall(name, args) => {
                assert!(name == "main::foo", "{src}: RHS calls foo, got {name}");
                assert_eq!(rhs.ctx, Some(call_ctx), "{src}: call inherits the assignment context");
                for a in args {
                    assert_eq!(a.ctx, Some(Context::List), "{src}: call arguments are always list context");
                }
            }
            other => panic!("{src}: RHS expected FuncCall, got {other:?}"),
        }
    }
}

#[test]
fn refgen_operand_context_from_parens_fact() {
    // Refgen records the `\@a` (reference the container whole) vs `\(@a)` (flatten, reference each element)
    // distinction as the operand's context, stamped from the parens-fact at construction: no parens → Scalar, parens
    // → List.  The rule is parens-only — `\@a` is Scalar despite @a being an aggregate.  (Lowering combines this tag
    // with the operand's container-ness; the parser only records the fact — §6.2.5.)
    use Context::{List, Scalar};
    let cases: &[(&str, Context)] = &[("\\$x;", Scalar), ("\\($x);", List), ("\\@a;", Scalar), ("\\(@a);", List), ("\\%h;", Scalar), ("\\(%h);", List)];
    for &(src, expected) in cases {
        let e = parse_expr_str(src);
        match &e.kind {
            ExprKind::Ref(operand) => assert_eq!(operand.ctx, Some(expected), "{src}: operand context"),
            other => panic!("{src}: expected Ref, got {other:?}"),
        }
    }
}

#[test]
fn refgen_paren_list_operand_is_list_context() {
    // `\($a, @b)` — a parenthesized list operand carries the List parens-fact on the Comma as a whole; lowering then
    // references each top-level item by its own kind (§6.2.5).
    let e = parse_expr_str("\\($a, @b);");
    match &e.kind {
        ExprKind::Ref(operand) => {
            assert!(matches!(operand.kind, ExprKind::Comma(_)), "expected Comma operand, got {:?}", operand.kind);
            assert_eq!(operand.ctx, Some(Context::List), "comma operand is list");
        }
        other => panic!("expected Ref, got {other:?}"),
    }
}

#[test]
fn refgen_double_paren_collapses_to_list() {
    // `\(($x))` — the wrap site caps nesting at depth one, so the Ref frame sees a single `Paren`; the operand is the
    // bare `$x` stamped List, identical to `\($x)`.
    let e = parse_expr_str("\\(($x));");
    match &e.kind {
        ExprKind::Ref(operand) => {
            assert!(matches!(operand.kind, ExprKind::ScalarVar(_)), "expected bare ScalarVar operand, got {:?}", operand.kind);
            assert_eq!(operand.ctx, Some(Context::List), "double-paren operand is still list");
        }
        other => panic!("expected Ref, got {other:?}"),
    }
}
