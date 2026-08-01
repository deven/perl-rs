use super::*;
use crate::string::DECODE_MAX;
use crate::value::{ScalarPayload, Tainted};

fn int(n: i64) -> Value {
    Value::Integer(n, Tainted::CLEAN)
}

fn key(text: &str) -> PerlString {
    text.parse().unwrap()
}

// ── Arrays ────────────────────────────────────────────────────
#[test]
fn array_holes_below_length() {
    // Container-verified: $a[5] = "x" on empty — length 6, 0–4 nonexistent, 5 exists.
    let mut a = PerlArray::new();
    a.set(5, int(1)).unwrap();
    assert_eq!(a.len(), 6);
    assert!(!a.exists(0));
    assert!(a.exists(5));
    assert!(a.get(0).is_none());
    assert_eq!(a.get(5).unwrap().to_int(), 1);
    assert!(a.get(99).is_none());
}

#[test]
fn array_ensure_element_vivifies_undef() {
    // Container-verified: \$a[3] on empty — length 4, element exists, undef.
    let mut a = PerlArray::new();
    let slot = a.ensure_element(3).unwrap();
    assert!(matches!(slot, Value::Undef(_)));
    assert_eq!(a.len(), 4);
    assert!(a.exists(3));
    assert!(!a.exists(0), "the get/ensure split: indices below stay holes");

    // Write-through: take a ref of the vivified slot, assign, observe (the \$a[3] round trip).
    let r = Value::take_ref(a.ensure_element(3).unwrap());
    r.deref_scalar().unwrap().write().unwrap().assign(ScalarPayload::Integer(5, Tainted::CLEAN)).unwrap();
    assert_eq!(a.get(3).unwrap().to_int(), 5, "$$r = 5 lands in the array");
}

#[test]
fn array_delete_rules() {
    // Migrated §2.2.1 pins: delete-mid holes, delete-last truncates through trailing holes.
    let mut a = PerlArray::new();
    for i in 0..3 {
        a.set(i, int(i as i64 + 1)).unwrap();
    }

    assert_eq!(a.delete(1).unwrap().to_int(), 2);
    assert_eq!(a.len(), 3, "delete-mid leaves a hole, length unchanged");
    assert!(!a.exists(1));
    assert_eq!(a.delete(2).unwrap().to_int(), 3);
    assert_eq!(a.len(), 1, "delete-last truncates through trailing holes");
    assert!(matches!(a.delete(9).unwrap(), Value::Undef(_)));
    assert_eq!(a.len(), 1, "delete beyond the end touches nothing");
}

#[test]
fn array_push_pop_shift_unshift() {
    let mut a = PerlArray::new();
    a.push_value(int(1)).unwrap();
    a.push_value(int(2)).unwrap();
    a.unshift_value(int(0)).unwrap();
    assert_eq!(a.len(), 3);
    assert_eq!(a.shift_value().unwrap().to_int(), 0);
    assert_eq!(a.pop_value().unwrap().to_int(), 2);
    assert_eq!(a.pop_value().unwrap().to_int(), 1);
    assert!(matches!(a.pop_value().unwrap(), Value::Undef(_)), "pop on empty is undef");

    // Pop after a sparse set: the value comes off; the holes remain (length 5, all holes).
    let mut sparse = PerlArray::new();
    sparse.set(5, int(9)).unwrap();
    assert_eq!(sparse.pop_value().unwrap().to_int(), 9);
    assert_eq!(sparse.len(), 5);
    assert!(!sparse.exists(0));
    assert!(matches!(sparse.pop_value().unwrap(), Value::Undef(_)), "popping a hole is undef");
    assert_eq!(sparse.len(), 4);
}

#[test]
fn array_readonly() {
    let mut a = PerlArray::new();
    a.set(0, int(1)).unwrap();
    a.set_readonly(true);
    assert_eq!(a.set(1, int(2)), Err(ScalarError::ReadOnly));
    assert_eq!(a.delete(0).map(|_| ()), Err(ScalarError::ReadOnly));
    assert_eq!(a.push_value(int(2)), Err(ScalarError::ReadOnly));
    assert_eq!(a.ensure_element(3).map(|_| ()), Err(ScalarError::ReadOnly));
    assert_eq!(a.clear(), Err(ScalarError::ReadOnly));
    assert_eq!(a.get(0).unwrap().to_int(), 1, "reads stay legal");
    a.set_readonly(false);
    a.set(1, int(2)).unwrap();
}

// ── Hashes ────────────────────────────────────────────────────
#[test]
fn hash_store_get_exists_delete() {
    let mut h = PerlHash::new();
    h.store(key("a"), int(1)).unwrap();
    h.store(key("b"), int(2)).unwrap();
    assert_eq!(h.len(), 2);
    assert!(h.exists(&key("a")));
    assert!(!h.exists(&key("z")));
    assert_eq!(h.get(&key("b")).unwrap().to_int(), 2);
    assert_eq!(h.delete(&key("b")).unwrap().to_int(), 2, "delete returns the value (verified)");
    assert!(!h.exists(&key("b")));
    assert!(matches!(h.delete(&key("z")).unwrap(), Value::Undef(_)));
    h.store(key("a"), int(9)).unwrap();
    assert_eq!(h.get(&key("a")).unwrap().to_int(), 9, "re-store replaces the value");
    assert_eq!(h.len(), 1);
}

#[test]
fn hash_keys_are_laundered_at_storage() {
    // Container-verified under -T: a tainted key stores clean; keys returns clean strings.
    let mut tainted_key = key("secret");
    tainted_key.taint();
    assert!(tainted_key.is_tainted());

    let mut h = PerlHash::new();
    h.store(tainted_key.clone(), int(1)).unwrap();
    let stored = h.keys();
    assert_eq!(stored.len(), 1);
    assert!(!stored[0].is_tainted(), "the §2.6.2 sanctioned laundering path");

    // Same through the lvalue path.
    let mut h2 = PerlHash::new();
    let _ = h2.entry_or_undef(tainted_key).unwrap();
    assert!(!h2.keys()[0].is_tainted());
}

#[test]
fn hash_entry_or_undef_vivifies() {
    // Container-verified: \$h{k} — the entry exists, undef.
    let mut h = PerlHash::new();
    let slot = h.entry_or_undef(key("k")).unwrap();
    assert!(matches!(slot, Value::Undef(_)));
    assert!(h.exists(&key("k")));

    let r = Value::take_ref(h.entry_or_undef(key("k")).unwrap());
    r.deref_scalar().unwrap().write().unwrap().assign(ScalarPayload::Integer(7, Tainted::CLEAN)).unwrap();
    assert_eq!(h.get(&key("k")).unwrap().to_int(), 7);
}

#[test]
fn each_visits_all_when_deleting_current() {
    // Container-verified: deleting the current item mid-each still visits all 4 keys.
    let mut h = PerlHash::new();
    for k in ["a", "b", "c", "d"] {
        h.store(key(k), int(1)).unwrap();
    }

    let mut visited = Vec::new();
    while let Some((k, _)) = h.each() {
        let is_b = k.as_bytes(&mut [0u8; DECODE_MAX]) == b"b";
        visited.push(k.clone());
        if is_b {
            h.delete(&k).unwrap();
        }
    }

    assert_eq!(visited.len(), 4, "all keys visited despite delete-current (verified)");
    assert_eq!(h.len(), 3);
}

#[test]
fn each_exhausts_restarts_and_keys_resets() {
    let mut h = PerlHash::new();
    h.store(key("x"), int(1)).unwrap();
    h.store(key("y"), int(2)).unwrap();

    // Exhaust: two yields, one None, then a restart (container-verified).
    assert!(h.each().is_some());
    assert!(h.each().is_some());
    assert!(h.each().is_none());
    assert!(h.each().is_some(), "the iterator restarts after exhaustion");

    // keys() resets mid-iteration (container-verified).
    let mut g = PerlHash::new();
    g.store(key("x"), int(1)).unwrap();
    g.store(key("y"), int(2)).unwrap();
    let _ = g.each();
    let _ = g.keys();
    let mut count = 0;

    while g.each().is_some() {
        count += 1;
    }

    assert_eq!(count, 2, "full pass after the reset");
}

#[test]
fn keys_values_stable_and_corresponding() {
    // Container-verified: stable without mutation; keys/values correspond.
    let mut h = PerlHash::new();
    for (i, k) in ["a", "b", "c"].iter().enumerate() {
        h.store(key(k), int(i as i64)).unwrap();
    }

    let k1 = h.keys();
    let k2 = h.keys();
    assert_eq!(
        k1.iter().map(|k| k.as_bytes(&mut [0u8; DECODE_MAX]).to_vec()).collect::<Vec<_>>(),
        k2.iter().map(|k| k.as_bytes(&mut [0u8; DECODE_MAX]).to_vec()).collect::<Vec<_>>()
    );

    let vals = h.values();
    for (k, v) in k1.iter().zip(vals.iter()) {
        assert_eq!(h.get(k).unwrap().to_int(), v.to_int());
    }
}

#[test]
fn hash_readonly() {
    let mut h = PerlHash::new();
    h.store(key("a"), int(1)).unwrap();
    h.set_readonly(true);
    assert_eq!(h.store(key("b"), int(2)), Err(ScalarError::ReadOnly));
    assert_eq!(h.delete(&key("a")).map(|_| ()), Err(ScalarError::ReadOnly));
    assert_eq!(h.entry_or_undef(key("c")).map(|_| ()), Err(ScalarError::ReadOnly));
    assert_eq!(h.clear(), Err(ScalarError::ReadOnly));
    assert_eq!(h.get(&key("a")).unwrap().to_int(), 1);
    assert_eq!(h.keys().len(), 1, "reads and iteration stay legal");
    h.set_readonly(false);
    h.store(key("b"), int(2)).unwrap();
}

// ── Handles ───────────────────────────────────────────────────
#[test]
fn handle_identity_and_traversal() {
    let a = ArrayRef::new(PerlArray::new());
    let a2 = a.clone();
    assert!(ArrayRef::ptr_eq(&a, &a2));
    let b = ArrayRef::new(PerlArray::new());
    assert!(!ArrayRef::ptr_eq(&a, &b));
    assert_ne!(a.addr(), 0);

    a.write().push_value(int(1)).unwrap();
    a.write().push_value(int(2)).unwrap();
    assert_eq!(a2.read().len(), 2, "writes visible through the clone: shared identity");
    assert_eq!(a.read().values_iter().map(Value::to_int).sum::<i64>(), 3, "collector hook");

    let h = HashRef::new(PerlHash::new());
    h.write().store(key("k"), int(5)).unwrap();
    assert_eq!(h.read().values_iter().count(), 1);
    assert!(format!("{h:?}").starts_with("HashRef(0x"));
}
