use super::*;
use crate::string::DECODE_MAX;
use crate::value::Value;

fn plain(payload: Value) -> ScalarRef {
    ScalarRef::new_mut(payload)
}

fn str_payload(text: &str) -> Value {
    Value::String(text.parse().unwrap())
}

// ── The §2.3.3 singleton contract, pinned ─────────────────────
#[test]
fn boolean_immortals_share_identity() {
    // Verified perl 5.38: \(1==1) yields the same address twice.
    let a = Value::True.upgrade_to_scalar().unwrap();
    let b = Value::True.upgrade_to_scalar().unwrap();
    assert!(ScalarRef::ptr_eq(&a, &b));
    assert!(matches!(a, ScalarRef::Const(_)));

    let f1 = Value::False.upgrade_to_scalar().unwrap();
    let f2 = Value::False.upgrade_to_scalar().unwrap();
    assert!(ScalarRef::ptr_eq(&f1, &f2));
    assert!(!ScalarRef::ptr_eq(&a, &f1), "the two singletons are distinct");
}

#[test]
fn immortals_prematerialized_values() {
    let t = TRUE_SCALAR.read();
    assert!(matches!(t.payload(), Value::True));
    assert_eq!(t.to_int(), 1);
    assert_eq!(t.to_float(), 1.0);
    assert_eq!(t.stringify().unwrap().as_bytes(&mut [0u8; DECODE_MAX]), b"1");
    assert!(t.to_bool());

    // The dualvar: numerically 0, string "" (not "0") — verified: (1==0)."" has length 0.
    let f = FALSE_SCALAR.read();
    assert!(matches!(f.payload(), Value::False));
    assert_eq!(f.to_int(), 0);
    assert_eq!(f.to_float(), 0.0);
    assert_eq!(f.stringify().unwrap().as_bytes(&mut [0u8; DECODE_MAX]), b"");
    assert!(!f.to_bool());
}

#[test]
fn immortal_mutation_is_the_readonly_error_never_a_panic() {
    match TRUE_SCALAR.write() {
        Err(ScalarError::ReadOnly) => {}
        _ => panic!("Const write must fail structurally"),
    }

    assert_eq!(ScalarError::ReadOnly.to_string(), "Modification of a read-only value attempted");
}

#[test]
fn cross_thread_upgrades_still_ptr_eq() {
    // Guards LazyLock initialization races: a fresh thread's upgrade is the same singleton.
    let here = Value::True.upgrade_to_scalar().unwrap();
    let there = std::thread::spawn(|| Value::True.upgrade_to_scalar().unwrap());
    let there = there.join().unwrap_or_else(|_| Value::True.upgrade_to_scalar().unwrap());
    assert!(ScalarRef::ptr_eq(&here, &there));
}

#[test]
fn is_bool_answers_from_the_variant() {
    assert!(Value::True.is_bool());
    assert!(Value::False.is_bool());
    assert!(!Value::integer(1, Tainted::CLEAN).is_bool());
    assert!(!Value::String("".parse().unwrap()).is_bool());
}

// ── ScalarRef / guards ────────────────────────────────────────
#[test]
fn reference_identity_and_clone_share() {
    let r1 = plain(Value::integer(42, Tainted::CLEAN));
    let r2 = r1.clone();
    assert!(ScalarRef::ptr_eq(&r1, &r2));
    let r3 = plain(Value::integer(42, Tainted::CLEAN));
    assert!(!ScalarRef::ptr_eq(&r1, &r3), "equal payloads, distinct identities");

    // Writes through one handle are visible through the other: shared identity.
    r1.write().unwrap().assign(Value::integer(7, Tainted::CLEAN)).unwrap();
    assert_eq!(r2.read().to_int(), 7);
}

#[test]
fn concurrent_const_reads_take_no_lock() {
    // Trivially concurrent: many threads reading the same Const cell simultaneously.
    let cell = ConstScalar::materialize(str_payload("3.7")).unwrap();
    let r = ScalarRef::new_const(cell);
    std::thread::scope(|s| {
        for _ in 0..4 {
            let r = &r;
            s.spawn(move || {
                for _ in 0..1000 {
                    assert_eq!(r.read().to_int(), 3);
                    assert_eq!(r.read().to_float(), 3.7);
                }
            });
        }
    });
}

// ── ScalarCell: payload authority, caches, upgrade ────────────
#[test]
fn payload_stays_authoritative_through_coercion() {
    // The §21.1 illustrative test: 3.7 used as an integer still stringifies as "3.7".
    let r = plain(Value::float(3.7, Tainted::CLEAN));
    assert_eq!(r.read().to_int(), 3);
    assert_eq!(r.read().stringify().unwrap().as_bytes(&mut [0u8; DECODE_MAX]), b"3.7");
}

#[test]
fn full_cell_caches_and_invalidation() {
    let r = plain(Value::float(3.7, Tainted::CLEAN));
    r.write().unwrap().upgrade_to_full();

    // Repeated coercions agree through the caches (fill under concurrent read guards).
    std::thread::scope(|s| {
        for _ in 0..4 {
            let r = &r;
            s.spawn(move || {
                for _ in 0..500 {
                    let g = r.read();
                    assert_eq!(g.to_int(), 3);
                    assert_eq!(g.to_float(), 3.7);
                    assert_eq!(g.stringify().unwrap().as_bytes(&mut [0u8; DECODE_MAX]), b"3.7");
                }
            });
        }
    });

    // Assignment is the single choke point: caches drop with the payload.
    r.write().unwrap().assign(Value::integer(9, Tainted::CLEAN)).unwrap();
    let g = r.read();
    assert_eq!(g.to_int(), 9);
    assert_eq!(g.to_float(), 9.0);
    assert_eq!(g.stringify().unwrap().as_bytes(&mut [0u8; DECODE_MAX]), b"9");
}

#[test]
fn upgrade_preserves_identity_and_payload() {
    let r = plain(str_payload("hello"));
    let alias = r.clone();

    {
        let mut g = r.write().unwrap();
        assert!(matches!(&*g, ScalarCell::Plain(_)));
        g.upgrade_to_full();
        g.upgrade_to_full(); // idempotent
        assert!(matches!(&*g, ScalarCell::Full(_)));
    }

    // The Arc address never changed: the outstanding alias still reaches the upgraded cell.
    assert!(ScalarRef::ptr_eq(&r, &alias));
    assert_eq!(alias.read().stringify().unwrap().as_bytes(&mut [0u8; DECODE_MAX]), b"hello");
}

#[test]
fn magic_and_bless_attach_in_place() {
    let r = plain(Value::integer(1, Tainted::CLEAN));

    {
        let mut g = r.write().unwrap();
        assert!(!g.has_magic());
        g.set_magic(MagicChain { _private: () });
        g.bless(HeapArc::new(Stash { _private: () }));
        assert!(g.has_magic());
    }

    assert_eq!(r.read().to_int(), 1, "payload survives the attachments");
}

// ── The readonly error path ───────────────────────────────────
#[test]
fn dynamic_readonly_is_toggleable() {
    let r = plain(Value::integer(5, Tainted::CLEAN));

    r.write().unwrap().set_readonly(true);
    assert!(r.write().unwrap().is_readonly(), "the flag is set; acquiring the guard stays legal");
    assert_eq!(r.write().unwrap().assign(Value::integer(6, Tainted::CLEAN)), Err(ScalarError::ReadOnly));
    assert_eq!(r.read().to_int(), 5, "the failed assignment changed nothing");

    // Internals::SvREADONLY is toggleable: clear and assign.
    r.write().unwrap().set_readonly(false);
    r.write().unwrap().assign(Value::integer(6, Tainted::CLEAN)).unwrap();
    assert_eq!(r.read().to_int(), 6);

    // Clearing readonly on a Plain cell is a no-op that must not upgrade.
    let p = plain(Value::integer(1, Tainted::CLEAN));
    p.write().unwrap().set_readonly(false);
    assert!(matches!(&*p.write().unwrap(), ScalarCell::Plain(_)));
}

// ── Numification-warning state (§2.3.4, container-verified) ───
#[test]
fn numify_warns_once_and_copies_carry_the_state() {
    // "abc" + 1 twice warns once, and the event carries the face — the exact string the message quotes, intact even
    // though the payload just promoted to Dual.
    let r = plain(str_payload("abc"));
    let (n1, emit1) = r.write().unwrap().numify_noting_warning().unwrap();
    assert_eq!(n1, Numeric::Float(0.0));

    let Some(NumifyWarning::NotNumeric { snippet, truncated }) = emit1 else {
        panic!("first numification warns, with the snippet");
    };
    assert_eq!(snippet.as_bytes(&mut [0u8; DECODE_MAX]), b"abc", "a short face is carried whole");
    assert!(!truncated);

    let (_, emit2) = r.write().unwrap().numify_noting_warning().unwrap();
    assert!(emit2.is_none(), "second is silent — the cached face is the suppressor");

    // Copy AFTER first numification: the copy is silent (the face rides the payload).
    let copied = r.read().payload().clone();
    let r2 = plain(copied);
    let (_, emit3) = r2.write().unwrap().numify_noting_warning().unwrap();
    assert!(emit3.is_none(), "copy after first numification is silent (verified)");

    // Copy BEFORE: both warn.
    let a = plain(str_payload("12abc"));
    let b = plain(a.read().payload().clone());
    assert!(a.write().unwrap().numify_noting_warning().unwrap().1.is_some());
    assert!(b.write().unwrap().numify_noting_warning().unwrap().1.is_some(), "copy before numification warns independently");

    // Clean numerics never emit.
    let c = plain(str_payload("  12  "));
    assert!(c.write().unwrap().numify_noting_warning().unwrap().1.is_none());
}

#[test]
fn numify_warning_display_is_perl_exact() {
    // Every expectation below is the container's own perl 5.44 output, byte for byte, minus the op clause and location
    // the interpreter suffixes.
    let body = |content: &[u8]| {
        let mut cell = ScalarCell::Plain(Value::String(PerlString::from_bytes(content).unwrap()));
        let (_, emit) = cell.numify_noting_warning().unwrap();
        format!("{}", emit.expect("warn-worthy content"))
    };

    assert_eq!(body(b"abc"), "Argument \"abc\" isn't numeric");
    assert_eq!(body(b"12abc"), "Argument \"12abc\" isn't numeric");
    assert_eq!(body(&b"a".repeat(100)), format!("Argument \"{}...\" isn't numeric", "a".repeat(56)));
    assert_eq!(body(&b"a".repeat(56)), format!("Argument \"{}\" isn't numeric", "a".repeat(56)));
    assert_eq!(body(b"ab\x01cd"), "Argument \"ab^Acd\" isn't numeric");
    assert_eq!(body(b"ab\tcd\nef"), "Argument \"ab^Icd\\nef\" isn't numeric");
    assert_eq!(body(b"\xE9abc"), "Argument \"M-iabc\" isn't numeric");

    // The last expansion may overrun the 56-column cap, exactly as S_sv_display's loop does: 55 columns plus a caret
    // pair prints all 57 with no ellipsis; a second control is past the check and clips.
    let mut edge = b"a".repeat(55);
    edge.push(1);
    assert_eq!(body(&edge), format!("Argument \"{}^A\" isn't numeric", "a".repeat(55)));
    edge.push(1);
    assert_eq!(body(&edge), format!("Argument \"{}^A...\" isn't numeric", "a".repeat(55)));

    // The flagged regime: cap 32 output columns, backslash doubled, everything non-printable \x{lowercase-hex} --
    // newline included, diverging from the byte regime's backslash-n.
    let flagged_body = |content: &str| {
        let mut s = PerlString::new(content).unwrap();
        s.set_utf8_for_test();
        let mut cell = ScalarCell::Plain(Value::String(s));
        let (_, emit) = cell.numify_noting_warning().unwrap();
        format!("{}", emit.expect("warn-worthy content"))
    };
    assert_eq!(flagged_body("é\nz\\q"), "Argument \"\\x{e9}\\x{a}z\\\\q\" isn't numeric");
    assert_eq!(flagged_body(&format!("é{}", "a".repeat(40))), format!("Argument \"\\x{{e9}}{}...\" isn't numeric", "a".repeat(26)));

    // The rest of the ruled inventory renders its perl bodies; constructors arrive with their operations.
    assert_eq!(format!("{}", NumifyWarning::LostPrecision { value: 1e17, decrement: false }), "Lost precision when incrementing 100000000000000000 by 1");
    assert_eq!(format!("{}", NumifyWarning::IllegalDigit { base: RadixBase::Binary, digit: b'2' }), "Illegal binary digit '2' ignored");
    assert_eq!(format!("{}", NumifyWarning::Overflow { base: RadixBase::Octal }), "Integer overflow in octal number");
    assert_eq!(format!("{}", NumifyWarning::NonPortable { base: RadixBase::Hexadecimal }), "Hexadecimal number > 0xffffffff non-portable");
    assert_eq!(
        format!("{}", NumifyWarning::OverflowThenIllegalDigit { base: RadixBase::Hexadecimal, digit: b'G' }),
        "Integer overflow in hexadecimal number\nIllegal hexadecimal digit 'G' ignored"
    );
    assert_eq!(
        format!("{}", NumifyWarning::IllegalDigitThenNonPortable { base: RadixBase::Octal, digit: b'9' }),
        "Illegal octal digit '9' ignored\nOctal number > 037777777777 non-portable"
    );
    assert_eq!(format!("{}", NumifyWarning::Uninitialized), "Use of uninitialized value");
}

#[test]
fn const_cell_warning_state() {
    let warns = ConstScalar::materialize(str_payload("abc")).unwrap();
    let Some(NumifyWarning::NotNumeric { snippet, truncated }) = warns.note_numify_warning() else {
        panic!("first note emits, with the snippet");
    };
    assert_eq!(snippet.as_bytes(&mut [0u8; DECODE_MAX]), b"abc");
    assert!(!truncated);
    assert!(warns.note_numify_warning().is_none(), "second is silent");

    // Statically-unwarnable payloads carry nothing (§2.3.4).
    let silent = ConstScalar::materialize(Value::integer(5, Tainted::CLEAN)).unwrap();
    assert!(silent.numify_warned.is_none());
    assert!(silent.note_numify_warning().is_none());

    let clean_str = ConstScalar::materialize(str_payload("42")).unwrap();
    assert!(clean_str.numify_warned.is_none());
}

// ── The §2.3.4 would-warn boundary table, pinned in full ──────
#[test]
fn would_warn_boundary_table() {
    let warns = [
        "abc",
        "12abc",
        "1e",
        "1e+",
        "0x10",
        "",
        "12.5abc",
        ".",
        "+",
        "-",
        "0.5.3",
        "1_000",
        "infx",
        "nanx",
        "  ",
        "0 But True",
        "0 but true ",
        " 0 but true",
        "0 but false",
    ];
    let silent = [
        "12",
        " 12",
        "12 ",
        "  12  ",
        "\t12\n",
        "3.5",
        "1e5",
        "0 but true",
        "inf",
        "Inf",
        "+5",
        "5.",
        ".5",
        "nan",
        "infinity",
        "INFINITY",
        "0E0",
        "-inf",
        "+nan",
    ];

    for form in warns {
        let s: PerlString = form.parse().unwrap();
        assert!(s.would_warn(), "{form:?} must warn (container-verified)");
        assert!(crate::value::string_would_warn(form.as_bytes()), "{form:?}: oracle disagrees with the byproduct");
    }

    for form in silent {
        let s: PerlString = form.parse().unwrap();
        assert!(!s.would_warn(), "{form:?} must be silent (container-verified)");
        assert!(!crate::value::string_would_warn(form.as_bytes()), "{form:?}: oracle disagrees with the byproduct");
    }
}

// ── Layout (§2.3.6) ───────────────────────────────────────────
#[test]
fn envelope_sizes() {
    assert_eq!(size_of::<ScalarCell>(), 16, "Full threads the payload's niche (measured, §2.3.2)");
    assert_eq!(size_of::<ScalarRef>(), 16);
}

#[test]
fn the_cached_numeric_face_is_what_suppresses_the_repeat_warning() {
    // §2.3.4: perl has no warned flag.  Numifying caches the salvaged number, and every later numification reads that
    // cache instead of re-parsing — so warning once is a consequence of caching, verified in the container as one
    // warning from "12abc" numified three times.
    let mut cell = ScalarCell::Plain(str_payload("12abc"));

    let (first, emit) = cell.numify_noting_warning().unwrap();
    assert!(emit.is_some(), "the first numification of warn-worthy content emits");
    assert_eq!(first, Numeric::Integer(12), "and salvages what perl salvages");

    let (second, emit) = cell.numify_noting_warning().unwrap();
    assert!(emit.is_none(), "the second reads the cached face and stays silent");
    assert_eq!(second, Numeric::Integer(12));

    // The face is what changed, not a flag: the payload is now a Dual whose string side is untouched.
    match &cell {
        ScalarCell::Plain(Value::Dual(d)) => {
            assert_eq!(d.string.as_bytes(&mut [0u8; DECODE_MAX]), b"12abc", "the string face survives verbatim");
            assert_eq!(d.numeric, Numeric::Integer(12));
        }
        other => panic!("expected a Dual payload, got {other:?}"),
    }

    // Stringification still yields the string face, and truth reads it too.
    assert_eq!(cell.stringify().unwrap().as_bytes(&mut [0u8; DECODE_MAX]), b"12abc");
    assert!(cell.to_bool());

    // A dual's rendering is its string face, present by construction, so the digit-cache predicate answers yes:
    // a caller consulting it to skip formatting must skip for a dual.
    if let ScalarCell::Plain(payload) = &cell {
        assert!(payload.has_cached_digits(), "the string face is the cached rendering");
    }
}

#[test]
fn cleanly_numeric_content_never_caches_a_face() {
    // Only warn-worthy content pays the allocation: a clean numeric string re-parses, which costs nanoseconds and
    // avoids an allocation per numeric value (§2.3.4).
    let mut cell = ScalarCell::Plain(str_payload("42"));
    for _ in 0..3 {
        let (n, emit) = cell.numify_noting_warning().unwrap();
        assert_eq!(n, Numeric::Integer(42));
        assert!(emit.is_none(), "clean content never warns");
    }
    assert!(matches!(cell, ScalarCell::Plain(Value::String(_))), "and stays a plain string");
}

#[test]
fn taint_survives_the_face_installation() {
    let mut tainted: PerlString = "12abc".parse().unwrap();
    tainted.taint();
    let mut cell = ScalarCell::Plain(Value::String(tainted));

    let (_, emit) = cell.numify_noting_warning().unwrap();
    assert!(emit.is_some());
    assert!(matches!(cell, ScalarCell::Plain(Value::DualTainted(_))), "the tainted twin is chosen");
    assert!(cell.is_tainted(), "and taint reads through the Dual");
}
