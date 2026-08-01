use super::*;
use crate::value::Value;

fn plain(payload: ScalarPayload) -> ScalarRef {
    ScalarRef::new_mut(payload)
}

fn str_payload(text: &str) -> ScalarPayload {
    ScalarPayload::String(text.parse().unwrap())
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
    assert!(matches!(t.payload(), ScalarPayload::True));
    assert_eq!(t.to_int(), 1);
    assert_eq!(t.to_float(), 1.0);
    assert_eq!(t.stringify().unwrap().as_bytes(), b"1");
    assert!(t.to_bool());

    // The dualvar: numerically 0, string "" (not "0") — verified: (1==0)."" has length 0.
    let f = FALSE_SCALAR.read();
    assert!(matches!(f.payload(), ScalarPayload::False));
    assert_eq!(f.to_int(), 0);
    assert_eq!(f.to_float(), 0.0);
    assert_eq!(f.stringify().unwrap().as_bytes(), b"");
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
    assert!(!Value::Int(1, Tainted::CLEAN).is_bool());
    assert!(!Value::String("".parse().unwrap()).is_bool());
}

// ── ScalarRef / guards ────────────────────────────────────────
#[test]
fn reference_identity_and_clone_share() {
    let r1 = plain(ScalarPayload::Int(42, Tainted::CLEAN));
    let r2 = r1.clone();
    assert!(ScalarRef::ptr_eq(&r1, &r2));
    let r3 = plain(ScalarPayload::Int(42, Tainted::CLEAN));
    assert!(!ScalarRef::ptr_eq(&r1, &r3), "equal payloads, distinct identities");

    // Writes through one handle are visible through the other: shared identity.
    r1.write().unwrap().assign(ScalarPayload::Int(7, Tainted::CLEAN)).unwrap();
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
    let r = plain(ScalarPayload::Float(3.7, Tainted::CLEAN));
    assert_eq!(r.read().to_int(), 3);
    assert_eq!(r.read().stringify().unwrap().as_bytes(), b"3.7");
}

#[test]
fn full_cell_caches_and_invalidation() {
    let r = plain(ScalarPayload::Float(3.7, Tainted::CLEAN));
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
                    assert_eq!(g.stringify().unwrap().as_bytes(), b"3.7");
                }
            });
        }
    });

    // Assignment is the single choke point: caches drop with the payload.
    r.write().unwrap().assign(ScalarPayload::Int(9, Tainted::CLEAN)).unwrap();
    let g = r.read();
    assert_eq!(g.to_int(), 9);
    assert_eq!(g.to_float(), 9.0);
    assert_eq!(g.stringify().unwrap().as_bytes(), b"9");
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
    assert_eq!(alias.read().stringify().unwrap().as_bytes(), b"hello");
}

#[test]
fn magic_and_bless_attach_in_place() {
    let r = plain(ScalarPayload::Int(1, Tainted::CLEAN));

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
    let r = plain(ScalarPayload::Int(5, Tainted::CLEAN));

    r.write().unwrap().set_readonly(true);
    assert!(r.write().unwrap().is_readonly(), "the flag is set; acquiring the guard stays legal");
    assert_eq!(r.write().unwrap().assign(ScalarPayload::Int(6, Tainted::CLEAN)), Err(ScalarError::ReadOnly));
    assert_eq!(r.read().to_int(), 5, "the failed assignment changed nothing");

    // Internals::SvREADONLY is toggleable: clear and assign.
    r.write().unwrap().set_readonly(false);
    r.write().unwrap().assign(ScalarPayload::Int(6, Tainted::CLEAN)).unwrap();
    assert_eq!(r.read().to_int(), 6);

    // Clearing readonly on a Plain cell is a no-op that must not upgrade.
    let p = plain(ScalarPayload::Int(1, Tainted::CLEAN));
    p.write().unwrap().set_readonly(false);
    assert!(matches!(&*p.write().unwrap(), ScalarCell::Plain(_)));
}

// ── Numification-warning state (§2.3.4, container-verified) ───
#[test]
fn numify_warns_once_and_copies_carry_the_state() {
    // "abc" + 1 twice warns once.
    let r = plain(str_payload("abc"));
    let (n1, emit1) = r.write().unwrap().numify_noting_warning();
    assert_eq!(n1, Numeric::Float(0.0));
    assert!(emit1, "first numification warns");
    let (_, emit2) = r.write().unwrap().numify_noting_warning();
    assert!(!emit2, "second is silent — the once-bit");

    // Copy AFTER first numification: the copy is silent (the bit rides the PerlString tag).
    let copied = r.read().payload().clone();
    let r2 = plain(copied);
    let (_, emit3) = r2.write().unwrap().numify_noting_warning();
    assert!(!emit3, "copy after first numification is silent (verified)");

    // Copy BEFORE: both warn.
    let a = plain(str_payload("12abc"));
    let b = plain(a.read().payload().clone());
    assert!(a.write().unwrap().numify_noting_warning().1);
    assert!(b.write().unwrap().numify_noting_warning().1, "copy before numification warns independently");

    // Clean numerics never emit.
    let c = plain(str_payload("  12  "));
    assert!(!c.write().unwrap().numify_noting_warning().1);
}

#[test]
fn const_cell_warning_state() {
    let warns = ConstScalar::materialize(str_payload("abc")).unwrap();
    assert!(warns.note_numify_warning(), "first note emits");
    assert!(!warns.note_numify_warning(), "second is silent");

    // Statically-unwarnable payloads carry nothing (§2.3.4).
    let silent = ConstScalar::materialize(ScalarPayload::Int(5, Tainted::CLEAN)).unwrap();
    assert!(silent.numify_warned.is_none());
    assert!(!silent.note_numify_warning());
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
        assert!(string_would_warn(form.as_bytes()), "{form:?} must warn (container-verified)");
    }

    for form in silent {
        assert!(!string_would_warn(form.as_bytes()), "{form:?} must be silent (container-verified)");
    }
}

// ── Layout (§2.3.6) ───────────────────────────────────────────
#[test]
fn envelope_sizes() {
    assert_eq!(size_of::<ScalarCell>(), 24, "Full threads the payload's niche (measured, §2.3.2)");
    assert_eq!(size_of::<ScalarRef>(), 16);
}
