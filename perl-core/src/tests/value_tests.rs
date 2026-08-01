use super::*;

fn s(text: &str) -> Value {
    Value::String(text.parse().unwrap())
}

// ── The payload principle (§2.2.2): the retired flag-matrix bug class ─────
#[test]
fn payload_stays_authoritative_through_coercion() {
    // Verified perl 5.38: my $x = 3.7 used as an integer still stringifies as "3.7" (FLAGS = NOK,pIOK — private
    // cache only).
    let x = Value::Float(3.7, Tainted::CLEAN);
    assert_eq!(x.to_int(), 3); // truncating coercion
    assert_eq!(x.stringify().unwrap().as_bytes(), b"3.7"); // payload answers
}

#[test]
fn truthiness_survives_numeric_use() {
    // The three container-verified cases the flag-matrix model failed: "0.0", "abc", "0.5" remain true through
    // numeric use, because truthiness is a payload question and coercion cannot replace the payload.
    for text in ["0.0", "abc", "0.5", "00", " "] {
        let v = s(text);
        let _ = v.to_int();
        let _ = v.to_float();
        assert!(v.to_bool(), "{text:?} must stay true through numeric use");
        assert_eq!(v.stringify().unwrap().as_bytes(), text.as_bytes());
    }
}

#[test]
fn truthiness_matrix() {
    assert!(!Value::default().to_bool());
    assert!(!Value::Int(0, Tainted::CLEAN).to_bool());
    assert!(Value::Int(-1, Tainted::CLEAN).to_bool());
    assert!(!Value::Float(0.0, Tainted::CLEAN).to_bool());
    assert!(!Value::Float(-0.0, Tainted::CLEAN).to_bool(), "-0.0 is false (container-verified)");
    assert!(Value::Float(f64::NAN, Tainted::CLEAN).to_bool(), "NaN is true (container-verified)");
    assert!(!s("").to_bool());
    assert!(!s("0").to_bool());
    assert!(s("0.0").to_bool());
    assert!(Value::True.to_bool());
    assert!(!Value::False.to_bool());
}

#[test]
fn stringification_matrix() {
    assert_eq!(Value::default().stringify().unwrap().as_bytes(), b"");
    assert_eq!(Value::Int(-42, Tainted::CLEAN).stringify().unwrap().as_bytes(), b"-42");
    assert_eq!(Value::Float(1e15, Tainted::CLEAN).stringify().unwrap().as_bytes(), b"1e+15");
    assert_eq!(Value::True.stringify().unwrap().as_bytes(), b"1");
    assert_eq!(Value::False.stringify().unwrap().as_bytes(), b"");
}

#[test]
fn numify_classification() {
    assert_eq!(s("42").numify(), Numeric::Int(42));
    assert_eq!(s("  +42  junk").numify(), Numeric::Int(42));
    assert_eq!(s("-9223372036854775808").numify(), Numeric::Int(i64::MIN));
    assert_eq!(s("9223372036854775807").numify(), Numeric::Int(i64::MAX), "IV_MAX string is exact (verified)");
    assert_eq!(s("3.5").numify(), Numeric::Float(3.5));
    assert_eq!(s("1e2").numify(), Numeric::Float(100.0));

    // UV-exact-but-beyond-i64: Float under the deferred-UV rule; to_int supplies the pinned wrap.
    assert_eq!(s("9223372036854775808").numify(), Numeric::Float(9.223372036854776e18));
    assert_eq!(Value::True.numify(), Numeric::Int(1));
    assert_eq!(Value::False.numify(), Numeric::Int(0));
}

// ── Integer coercions (all container-verified) ────────────────
#[test]
fn parse_int_basics() {
    assert_eq!(parse_int_i64_visible(b"42"), 42);
    assert_eq!(parse_int_i64_visible(b"  +42"), 42);
    assert_eq!(parse_int_i64_visible(b"-17abc"), -17);
    assert_eq!(parse_int_i64_visible(b"010"), 10, "leading-zero strings are decimal (verified)");
    assert_eq!(parse_int_i64_visible(b"0xff"), 0, "radix prefixes are never interpreted (verified)");
    assert_eq!(parse_int_i64_visible(b"0b234"), 0);
    assert_eq!(parse_int_i64_visible(b"-"), 0);
    assert_eq!(parse_int_i64_visible(b"+"), 0);
    assert_eq!(parse_int_i64_visible(b""), 0);
    assert_eq!(parse_int_i64_visible(b"abc"), 0);
}

#[test]
fn parse_int_beyond_i64_is_the_wrapping_cast() {
    // The resolution of the old "never 0" red: container-verified printf %d gives the wrapped cast.
    assert_eq!(parse_int_i64_visible(b"9223372036854775808"), i64::MIN, "2^63 wraps (verified -9223372036854775808)");
    assert_eq!(parse_int_i64_visible(b"18446744073709551615"), -1, "UV_MAX wraps to -1");
    assert_eq!(parse_int_i64_visible(b"18446744073709551616"), -1, "beyond UV_MAX is UV_MAX-visible (verified)");
    assert_eq!(parse_int_i64_visible(b"99999999999999999999999999"), -1);
    assert_eq!(parse_int_i64_visible(b"-9223372036854775808"), i64::MIN);
    assert_eq!(parse_int_i64_visible(b"-9223372036854775809"), i64::MIN, "negative overflow clamps (verified)");
    assert_eq!(parse_int_i64_visible(b"-99999999999999999999"), i64::MIN);
}

#[test]
fn float_to_int_contracts() {
    assert_eq!(float_to_int_i64_visible(3.7), 3);
    assert_eq!(float_to_int_i64_visible(-3.7), -3, "truncation toward zero (verified)");
    assert_eq!(float_to_int_i64_visible(f64::NAN), 0, "NaN caches 0 (Devel::Peek-verified)");
    assert_eq!(float_to_int_i64_visible(f64::INFINITY), -1, "Inf caches UV_MAX (Devel::Peek-verified)");
    assert_eq!(float_to_int_i64_visible(f64::NEG_INFINITY), i64::MIN, "-Inf caches IV_MIN");
    assert_eq!(float_to_int_i64_visible(1e30), -1, "finite beyond UV_MAX (verified printf %d)");
    assert_eq!(float_to_int_i64_visible(-1e30), i64::MIN);
    assert_eq!(float_to_int_i64_visible(9.3e18), -9146744073709551616, "the UV range wraps (verified)");
    assert_eq!(float_to_int_i64_visible(9223372036854775808.0), i64::MIN, "exactly 2^63 (verified)");
}

// ── Float parsing (container-verified) ────────────────────────
#[test]
fn parse_float_basics() {
    assert_eq!(parse_float(b"3.5"), 3.5);
    assert_eq!(parse_float(b"  -2.5e2xyz"), -250.0);
    assert_eq!(parse_float(b"1e"), 1.0, "dangling exponent backtracks (verified)");
    assert_eq!(parse_float(b"1e+"), 1.0);
    assert_eq!(parse_float(b".5"), 0.5);
    assert_eq!(parse_float(b""), 0.0);
    assert_eq!(parse_float(b"abc"), 0.0);
    let nv = parse_float(b"9223372036854775808");
    assert!((nv - 9.223372036854776e18).abs() < 1e4);
}

#[test]
fn parse_float_inf_nan_prefix_forms() {
    // All container-verified: case-insensitive prefixes after whitespace and sign.
    assert_eq!(parse_float(b"inf"), f64::INFINITY);
    assert_eq!(parse_float(b"Infinity"), f64::INFINITY);
    assert_eq!(parse_float(b"infx"), f64::INFINITY, "prefix match (verified: \"infx\"+0 is Inf)");
    assert_eq!(parse_float(b"  +inF"), f64::INFINITY);
    assert_eq!(parse_float(b"-inf"), f64::NEG_INFINITY);
    assert_eq!(parse_float(b"in"), 0.0, "\"in\" is not a number (verified)");
    assert!(parse_float(b"nan").is_nan());
    assert!(parse_float(b"-nan").is_nan());
    assert!(parse_float(b"nanx").is_nan());
    assert!(parse_float(b"NaN").is_nan());
}

// ── format_float (ported; all values verified against perl 5.38.2 print output) ──
#[test]
fn format_float_matches_perl_g15() {
    assert_eq!(format_float(0.1 + 0.2), "0.3");
    assert_eq!(format_float(0.0), "0");
    assert_eq!(format_float(-0.0), "0");
    assert_eq!(format_float(42.0), "42");
    assert_eq!(format_float(3.7), "3.7");
    assert_eq!(format_float(1e15), "1e+15");
    assert_eq!(format_float(999999999999999.0), "999999999999999");
    assert_eq!(format_float(1e-5), "1e-05");
    assert_eq!(format_float(0.0001), "0.0001");
    assert_eq!(format_float(f64::NAN), "NaN");
    assert_eq!(format_float(f64::INFINITY), "Inf");
    assert_eq!(format_float(f64::NEG_INFINITY), "-Inf");
    assert_eq!(format_float(-2.5), "-2.5");
}

// ── Taint (§2.6.1/§2.6.3) ─────────────────────────────────────
#[test]
fn taint_is_monotone_and_placed_per_variant() {
    assert!(!Tainted::CLEAN.is_tainted());
    assert!(Tainted::TAINTED.is_tainted());
    assert!(Tainted::CLEAN.tainted_by(Tainted::TAINTED).is_tainted());
    assert!(Tainted::TAINTED.tainted_by(Tainted::CLEAN).is_tainted(), "OR raises, never lowers");
    assert!(!Tainted::laundered().is_tainted());

    // Tainted undef is real (§2.6.1: readline at EOF).
    let tu = Value::Undef(Tainted::TAINTED);
    assert!(tu.is_tainted());
    assert!(!tu.to_bool());

    // Booleans alone carry no taint state.
    assert!(!Value::True.is_tainted());

    // String taint lives in the tag and survives stringification (a clone).
    let mut ps: PerlString = "secret".parse().unwrap();
    ps.taint();
    let v = Value::String(ps);
    assert!(v.is_tainted());
    assert!(v.stringify().unwrap().is_tainted());

    // Numeric stringification propagates the operand's taint into the tag.
    let ti = Value::Int(7, Tainted::TAINTED);
    assert!(ti.stringify().unwrap().is_tainted());
    assert!(!Value::Int(7, Tainted::CLEAN).stringify().unwrap().is_tainted());
}

// ── ArraySlot semantics (§2.2.1, container-verified) ──────────
#[test]
fn array_slot_hole_and_truncation_rules() {
    let mk = || vec![Some(Value::Int(1, Tainted::CLEAN)), Some(Value::Int(2, Tainted::CLEAN)), Some(Value::Int(3, Tainted::CLEAN))];

    // delete-mid: hole, length unchanged, value returned, exists false.
    let mut a = mk();
    let d = array_delete(&mut a, 1);
    assert_eq!(d.to_int(), 2);
    assert_eq!(a.len(), 3);
    assert!(!array_exists(&a, 1));
    assert!(array_exists(&a, 2));

    // delete-last after a mid hole: truncate through trailing holes (verified: length 1, not 2).
    let d2 = array_delete(&mut a, 2);
    assert_eq!(d2.to_int(), 3);
    assert_eq!(a.len(), 1);

    // delete beyond the end: undef returned, untouched (verified).
    let mut b = mk();
    let d3 = array_delete(&mut b, 9);
    assert!(!d3.to_bool());
    assert!(matches!(d3, Value::Undef(_)));
    assert_eq!(b.len(), 3);

    // A hole is not an undef element: Some(Undef) exists.
    let mut c = vec![Some(Value::default())];
    assert!(array_exists(&c, 0));
    let _ = array_delete(&mut c, 0);
    assert!(c.is_empty(), "deleting the last (undef) element truncates");
}

// ── References (§2.2.8, step 5; container-verified) ──────────
#[test]
fn take_ref_identity_is_idempotent_and_distinct_per_slot() {
    let mut slot = s("hello");
    let r1 = Value::take_ref(&mut slot);
    let r2 = Value::take_ref(&mut slot);
    assert_eq!(r1.to_int(), r2.to_int(), "same slot, same identity (address)");
    assert!(crate::scalar::ScalarRef::ptr_eq(&r1.deref_scalar().unwrap(), &r2.deref_scalar().unwrap()));

    let mut other = s("hello");
    let r3 = Value::take_ref(&mut other);
    assert_ne!(r1.to_int(), r3.to_int(), "equal payloads, distinct identities");
}

#[test]
fn aliasing_transparency_and_write_through() {
    let mut slot = Value::Int(5, Tainted::CLEAN);
    let r = Value::take_ref(&mut slot);

    // The promoted slot still answers as the payload: aliasing transparency.
    assert!(matches!(slot, Value::ScalarMut(_)));
    assert_eq!(slot.to_int(), 5);
    assert!(slot.to_bool());
    assert_eq!(slot.stringify().unwrap().as_bytes(), b"5");

    // Writes through the dereferenced identity are visible through the slot.
    let view = r.deref_scalar().unwrap();
    view.write().unwrap().assign(ScalarPayload::Int(9, Tainted::CLEAN)).unwrap();
    assert_eq!(slot.to_int(), 9, "$$r = 9 observed via $x");
}

#[test]
fn boolean_slots_promote_to_their_own_cells() {
    // Container-verified: \$x and \$y for two boolean variables are distinct, and distinct from the immortal
    // (\(1==1)).
    let mut x = Value::True;
    let mut y = Value::True;
    let rx = Value::take_ref(&mut x);
    let ry = Value::take_ref(&mut y);
    assert_ne!(rx.to_int(), ry.to_int(), "distinct cells per variable");

    let immortal = Value::True.upgrade_to_scalar().unwrap();
    assert!(!crate::scalar::ScalarRef::ptr_eq(&rx.deref_scalar().unwrap(), &immortal));

    // The promoted boolean keeps is_bool through the variant payload.
    let view = rx.deref_scalar().unwrap();
    assert!(matches!(view.read().payload(), ScalarPayload::True));
}

#[test]
fn reference_coercions_are_the_address() {
    let mut slot = s("target");
    let r = Value::take_ref(&mut slot);

    assert!(r.to_bool(), "references are unconditionally true (container-verified)");
    let addr = r.to_int();
    assert!(addr != 0);
    assert_eq!(r.to_float(), addr as f64);
    assert_eq!(r.numify(), Numeric::Int(addr));
    let rendered = r.stringify().unwrap();
    let expected = format!("SCALAR(0x{:x})", addr as usize);
    assert_eq!(rendered.as_bytes(), expected.as_bytes(), "SCALAR(0x...) lowercase hex (verified)");
}

#[test]
fn ref_of_ref_chains() {
    let mut base = s("x");
    let r1 = Value::take_ref(&mut base);

    let mut holder = r1; // a slot now holding the reference value
    let r2 = Value::take_ref(&mut holder);

    // $$$rr reaches the base cell: two derefs, then the payload.
    let mid = r2.deref_scalar().unwrap();
    let inner = mid.read().payload().clone();
    let inner = Value::from_payload(inner);
    let base_view = inner.deref_scalar().unwrap();
    assert_eq!(base_view.read().stringify().unwrap().as_bytes(), b"x");

    // And writing through the chain is visible via the original slot.
    base_view.write().unwrap().assign(ScalarPayload::Int(7, Tainted::CLEAN)).unwrap();
    assert_eq!(base.to_int(), 7);
}

#[test]
fn reference_taint_belongs_to_the_referent() {
    let mut ps: PerlString = "secret".parse().unwrap();
    ps.taint();
    let mut slot = Value::String(ps);

    let r = Value::take_ref(&mut slot);
    assert!(!r.is_tainted(), "the reference value is clean");
    assert!(r.deref_scalar().unwrap().read().is_tainted(), "the referent carries the taint");
    assert!(slot.is_tainted(), "and the slot still answers tainted through the alias");
}

#[test]
fn const_slots_alias_frozen_cells() {
    let cs = crate::scalar::ConstScalar::materialize(ScalarPayload::Float(3.7, Tainted::CLEAN)).unwrap();
    let mut slot = Value::ScalarConst(HeapArc::new(cs));

    assert_eq!(slot.to_int(), 3);
    assert_eq!(slot.stringify().unwrap().as_bytes(), b"3.7");

    let r = Value::take_ref(&mut slot);
    assert!(matches!(r, Value::ScalarRefConst(..)));
    let view = r.deref_scalar().unwrap();
    assert!(matches!(view.write(), Err(crate::scalar::ScalarError::ReadOnly)), "frozen through the ref");
}

// ── Layout (§2.3.6) ───────────────────────────────────────────
#[test]
fn envelope_sizes() {
    assert_eq!(size_of::<ScalarPayload>(), 24);
    assert_eq!(size_of::<Value>(), 24);
    assert_eq!(size_of::<Option<Value>>(), 24);
    assert_eq!(size_of::<ArraySlot>(), 24);
    assert_eq!(size_of::<Numeric>(), 16);
}

// ── format_float against perl's default NV stringification ────────
//
// Every expectation below is container perl 5.38.2's own output for the same literal, captured by differential run:
// `print 1e15` and friends.  Note that these are NV *literals* — perl's arithmetic returns an IV whenever the result
// is integral and fits, so `1e15 + 0.0` prints as 1000000000000000 rather than 1e+15, which is integer
// stringification and a different path.
#[test]
fn format_float_matches_container_perl() {
    let cases: &[(f64, &str)] = &[
        (0.1_f64, "0.1"),
        (0.30000000000000004_f64, "0.3"),
        (0.0_f64, "0"),
        (-0.0_f64, "0"),
        (1.0_f64, "1"),
        (-1.0_f64, "-1"),
        (42.0_f64, "42"),
        (3.7_f64, "3.7"),
        (-2.5_f64, "-2.5"),
        (1.25_f64, "1.25"),
        (0.5_f64, "0.5"),
        (100.0_f64, "100"),
        (1e14_f64, "100000000000000"),
        (1e15_f64, "1e+15"),
        (1e16_f64, "1e+16"),
        (1e21_f64, "1e+21"),
        (1e100_f64, "1e+100"),
        (1e308_f64, "1e+308"),
        (999999999999999.0_f64, "999999999999999"),
        (1000000000000000.0_f64, "1e+15"),
        (1234567890123456.0_f64, "1.23456789012346e+15"),
        (123456789012345.0_f64, "123456789012345"),
        (1.5e15_f64, "1.5e+15"),
        (-1e15_f64, "-1e+15"),
        (0.0001_f64, "0.0001"),
        (0.00001_f64, "1e-05"),
        (0.00012345_f64, "0.00012345"),
        (0.000123456789012345_f64, "0.000123456789012345"),
        (0.3333333333333333_f64, "0.333333333333333"),
        (-0.3333333333333333_f64, "-0.333333333333333"),
        (2.220446049250313e-16_f64, "2.22044604925031e-16"),
        (1.7976931348623157e308_f64, "1.79769313486232e+308"),
        (2.2250738585072014e-308_f64, "2.2250738585072e-308"),
        (-2.2250738585072014e-308_f64, "-2.2250738585072e-308"),
        (5e-324_f64, "4.94065645841247e-324"),
        (9.88131291682493e-324_f64, "9.88131291682493e-324"),
        (1e-300_f64, "1e-300"),
        (123.456_f64, "123.456"),
        (0.007_f64, "0.007"),
        (7.0_f64, "7"),
        (1e5_f64, "100000"),
        (1e-1_f64, "0.1"),
        (0.9999999999999999_f64, "1"),
        (1.0000000000000002_f64, "1"),
    ];
    for (value, expected) in cases {
        assert_eq!(&format_float(*value), expected, "rendering {value:?}");
    }
}

#[test]
fn format_float_specials_use_perls_capitalization() {
    assert_eq!(format_float(f64::NAN), "NaN");
    assert_eq!(format_float(f64::INFINITY), "Inf");
    assert_eq!(format_float(f64::NEG_INFINITY), "-Inf");
}

#[test]
fn numeric_stringification_does_not_allocate() {
    // The keystone invariant (§2.2.3): default numeric renderings stay in the value itself.  If a rendering ever
    // exceeds the non-allocating capacity this fails rather than silently allocating on constant-traffic paths.
    for value in [0.0_f64, 3.7, -2.5, 1e15, 1e-5, f64::MIN_POSITIVE, f64::MAX, -f64::MAX, f64::NAN] {
        let rendered = format_float(value);
        assert!(PerlString::inline(&rendered).is_some(), "{rendered} should need no allocation");
    }
    for value in [0_i64, -1, i64::MAX, i64::MIN] {
        assert!(PerlString::inline(value.to_string()).is_some(), "{value} should need no allocation");
    }
}
