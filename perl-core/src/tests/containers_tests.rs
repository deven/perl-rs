use super::*;
use crate::scalar::Referent;
use crate::string::DECODE_MAX;
use crate::value::{Tainted, Value};

/// Both engines (§2.2.13): the container-verified semantics are engine-independent, so the shared batteries run against
/// each.
fn both_engines() -> Vec<Hash> {
    #[allow(unused_mut)]
    let mut engines = vec![Hash::new()];

    #[cfg(feature = "indexmap")]
    engines.push(Hash::ordered());

    #[cfg(feature = "imbl")]
    engines.push(Hash::immutable());

    engines
}

fn int(n: i64) -> Value {
    Value::integer(n, Tainted::CLEAN)
}

fn key(text: &str) -> PString {
    text.parse().unwrap()
}

// ── Arrays ────────────────────────────────────────────────────
#[test]
fn array_holes_below_length() {
    // Container-verified: $a[5] = "x" on empty — length 6, 0–4 nonexistent, 5 exists.
    let a = Array::new();
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
    let a = Array::new();
    a.ensure_element(3, |slot| {
        assert!(matches!(slot, Value::Undef | Value::UndefTainted));
    })
    .unwrap();
    assert_eq!(a.len(), 4);
    assert!(a.exists(3));
    assert!(!a.exists(0), "the get/ensure split: indices below stay holes");

    // Write-through: take a ref of the vivified slot, assign, observe (the \$a[3] round trip).
    let r = a.ensure_element(3, Value::take_ref).unwrap();
    r.deref_scalar().unwrap().write().unwrap().assign(Value::integer(5, Tainted::CLEAN)).unwrap();
    assert_eq!(a.get(3).unwrap().to_int(), 5, "$$r = 5 lands in the array");
}

#[test]
fn array_delete_rules() {
    // Migrated §2.2.1 pins: delete-mid holes, delete-last truncates through trailing holes.
    let a = Array::new();
    for i in 0..3 {
        a.set(i, int(i as i64 + 1)).unwrap();
    }

    assert_eq!(a.delete(1).unwrap().to_int(), 2);
    assert_eq!(a.len(), 3, "delete-mid leaves a hole, length unchanged");
    assert!(!a.exists(1));
    assert_eq!(a.delete(2).unwrap().to_int(), 3);
    assert_eq!(a.len(), 1, "delete-last truncates through trailing holes");
    assert!(matches!(a.delete(9).unwrap(), Value::Undef | Value::UndefTainted));
    assert_eq!(a.len(), 1, "delete beyond the end touches nothing");
}

#[test]
fn array_push_pop_shift_unshift() {
    let a = Array::new();
    a.push_value(int(1)).unwrap();
    a.push_value(int(2)).unwrap();
    a.unshift_value(int(0)).unwrap();
    assert_eq!(a.len(), 3);
    assert_eq!(a.shift_value().unwrap().to_int(), 0);
    assert_eq!(a.pop_value().unwrap().to_int(), 2);
    assert_eq!(a.pop_value().unwrap().to_int(), 1);
    assert!(matches!(a.pop_value().unwrap(), Value::Undef | Value::UndefTainted), "pop on empty is undef");

    // Pop after a sparse set: the value comes off; the holes remain (length 5, all holes).
    let sparse = Array::new();
    sparse.set(5, int(9)).unwrap();
    assert_eq!(sparse.pop_value().unwrap().to_int(), 9);
    assert_eq!(sparse.len(), 5);
    assert!(!sparse.exists(0));
    assert!(matches!(sparse.pop_value().unwrap(), Value::Undef | Value::UndefTainted), "popping a hole is undef");
    assert_eq!(sparse.len(), 4);
}

#[test]
fn array_readonly() {
    let a = Array::new();
    a.set(0, int(1)).unwrap();
    a.set_readonly(true);
    assert_eq!(a.set(1, int(2)), Err(ScalarError::ReadOnly));
    assert_eq!(a.delete(0).map(|_| ()), Err(ScalarError::ReadOnly));
    assert_eq!(a.push_value(int(2)), Err(ScalarError::ReadOnly));
    assert_eq!(a.ensure_element(3, |_| ()), Err(ScalarError::ReadOnly));
    assert_eq!(a.clear(), Err(ScalarError::ReadOnly));
    assert_eq!(a.get(0).unwrap().to_int(), 1, "reads stay legal");
    a.set_readonly(false);
    a.set(1, int(2)).unwrap();
}

// ── Hashes ────────────────────────────────────────────────────
#[test]
fn hash_store_get_exists_delete() {
    for h in both_engines() {
        h.store(key("a"), int(1)).unwrap();
        h.store(key("b"), int(2)).unwrap();
        assert_eq!(h.len(), 2);
        assert!(h.exists(&key("a")));
        assert!(!h.exists(&key("z")));
        assert_eq!(h.get(&key("b")).unwrap().to_int(), 2);
        assert_eq!(h.delete(&key("b")).unwrap().to_int(), 2, "delete returns the value (verified)");
        assert!(!h.exists(&key("b")));
        assert!(matches!(h.delete(&key("z")).unwrap(), Value::Undef | Value::UndefTainted));

        h.store(key("a"), int(9)).unwrap();
        assert_eq!(h.get(&key("a")).unwrap().to_int(), 9, "re-store replaces the value");
        assert_eq!(h.len(), 1);
    }
}

#[test]
fn hash_keys_are_laundered_at_storage() {
    for h in both_engines() {
        // Container-verified under -T: a tainted key stores clean; keys returns clean strings.
        let mut tainted_key = key("secret");
        tainted_key.taint();
        assert!(tainted_key.is_tainted());

        h.store(tainted_key.clone(), int(1)).unwrap();
        let stored = h.keys();
        assert_eq!(stored.len(), 1);
        assert!(!stored[0].is_tainted(), "the §2.6.2 sanctioned laundering path");

        // Same through the lvalue path.
        let h2 = Hash::new();
        h2.entry_or_undef(tainted_key, |_| ()).unwrap();
        assert!(!h2.keys()[0].is_tainted());
    }
}

#[test]
fn hash_entry_or_undef_vivifies() {
    for h in both_engines() {
        // Container-verified: \$h{k} — the entry exists, undef.  The lvalue path is closure-shaped (§2.2.13).
        h.entry_or_undef(key("k"), |slot| {
            assert!(matches!(slot, Value::Undef | Value::UndefTainted));
        })
        .unwrap();
        assert!(h.exists(&key("k")));

        let r = h.entry_or_undef(key("k"), Value::take_ref).unwrap();
        r.deref_scalar().unwrap().write().unwrap().assign(Value::integer(7, Tainted::CLEAN)).unwrap();
        assert_eq!(h.get(&key("k")).unwrap().to_int(), 7);
    }
}

#[test]
fn each_visits_all_when_deleting_current() {
    for h in both_engines() {
        // Container-verified: deleting the current item mid-each still visits all 4 keys.
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
}

#[test]
fn each_exhausts_restarts_and_keys_resets() {
    for h in both_engines() {
        h.store(key("x"), int(1)).unwrap();
        h.store(key("y"), int(2)).unwrap();

        // Exhaust: two yields, one None, then a restart (container-verified).
        assert!(h.each().is_some());
        assert!(h.each().is_some());
        assert!(h.each().is_none());
        assert!(h.each().is_some(), "the iterator restarts after exhaustion");
    }

    // keys() resets mid-iteration (container-verified).
    for g in both_engines() {
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
}

#[test]
fn keys_values_stable_and_corresponding() {
    // Container-verified: stable without mutation; keys/values correspond.  Engine-shared: order is per-engine,
    // stability and correspondence are not.
    for h in both_engines() {
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
}

#[test]
fn hash_readonly() {
    for h in both_engines() {
        h.store(key("a"), int(1)).unwrap();
        h.set_readonly(true);
        assert_eq!(h.store(key("b"), int(2)), Err(ScalarError::ReadOnly));
        assert_eq!(h.delete(&key("a")).map(|_| ()), Err(ScalarError::ReadOnly));
        assert_eq!(h.entry_or_undef(key("c"), |_| ()), Err(ScalarError::ReadOnly));
        assert_eq!(h.clear(), Err(ScalarError::ReadOnly));
        assert_eq!(h.get(&key("a")).unwrap().to_int(), 1);
        assert_eq!(h.keys().len(), 1, "reads and iteration stay legal");
        h.set_readonly(false);
        h.store(key("b"), int(2)).unwrap();
    }
}

// ── The §2.2.12 front-gap engine ──────────────────────────────
#[test]
fn shift_is_a_window_slide_and_unshift_reclaims_the_gap() {
    let a = Array::new();
    for i in 0..8 {
        a.push_value(int(i)).unwrap();
    }
    let base = a.probe_base();

    // Three shifts: the window slides, the buffer stays put, the gap opens.
    for expect in 0..3 {
        assert_eq!(a.shift_value().unwrap().to_int(), expect);
    }
    let (start, len, _cap, large) = a.probe_geometry();
    assert_eq!((start, len, large), (3, 5, false));
    assert_eq!(a.probe_base(), base, "shift moved no memory");

    // Phase-one unshift: reclaim exactly one slot of the gap — surplus preserved, no element movement.
    a.unshift_value(int(100)).unwrap();
    let (start, len, _cap, _) = a.probe_geometry();
    assert_eq!((start, len), (2, 6), "take = min(start, n): one reclaimed, two remain");
    assert_eq!(a.probe_base(), base, "gap reclaim moved no memory");
    assert_eq!(a.get(0).unwrap().to_int(), 100);
    assert_eq!(a.get(5).unwrap().to_int(), 7);
}

#[test]
fn shortfall_unshift_carves_the_fill_back_as_gap() {
    // §2.2.12 phase two: with no gap, the slide is need + fill, and the fill's worth returns as fresh gap — the prepaid
    // buffer equals the live count.
    let a = Array::new();
    for i in 0..5 {
        a.push_value(int(i)).unwrap();
    }

    let (start, _, _, _) = a.probe_geometry();
    assert_eq!(start, 0, "no gap yet");

    a.unshift_value(int(100)).unwrap();
    let (start, len, _, _) = a.probe_geometry();
    assert_eq!(len, 6);
    assert_eq!(start, 4, "the carved gap equals the pre-slide fill (five live, fill four)");
    assert_eq!(a.get(0).unwrap().to_int(), 100);
    assert_eq!(a.get(5).unwrap().to_int(), 4);

    // The prepaid gap makes the next four unshifts pure phase-one.
    let base = a.probe_base();
    for v in [101, 102, 103, 104] {
        a.unshift_value(int(v)).unwrap();
    }
    assert_eq!(a.probe_base(), base, "four prepaid unshifts moved no memory");
    assert_eq!(a.probe_geometry().0, 0, "the prepaid gap is spent");
}

#[test]
fn growth_follows_the_ruled_curve() {
    // §2.2.12: growth requests at least min_cap + cap/5, then harvests the allocator's class — so the landed capacity
    // is bounded below by the curve, never mere fit.
    let a = Array::new();
    a.set(9, int(1)).unwrap();
    let (_, _, cap_before, _) = a.probe_geometry();
    a.set(cap_before, int(2)).unwrap();
    let (_, _, cap_after, _) = a.probe_geometry();
    assert!(cap_after >= cap_before + 1 + cap_before / 5, "curve lower bound: {cap_after} >= {cap_before} + 1 + {}", cap_before / 5);
}

#[test]
fn empty_shift_and_pop_return_undef() {
    let a = Array::new();
    assert!(matches!(a.shift_value().unwrap(), Value::Undef));
    assert!(matches!(a.pop_value().unwrap(), Value::Undef));
}

#[test]
fn the_wide_arm_runs_the_same_battery() {
    // §2.2.12 spill parity: the boxed wide geometry serves the identical surface — the u32 overflow trigger being
    // untestable at 64 GiB, the arm is forced and exercised whole.
    let a = Array::new();
    a.push_value(int(0)).unwrap();
    a.force_large_for_test();
    assert!(a.probe_geometry().3, "the arm is wide");

    for i in 1..40 {
        a.push_value(int(i)).unwrap();
    }
    for expect in 0..5 {
        assert_eq!(a.shift_value().unwrap().to_int(), expect);
    }
    a.unshift_value(int(100)).unwrap();
    a.set(50, int(50)).unwrap();
    assert_eq!(a.len(), 51);
    assert!(!a.exists(49) && a.exists(50));
    assert_eq!(a.get(0).unwrap().to_int(), 100);
    assert_eq!(a.delete(50).unwrap().to_int(), 50);
    assert_eq!(a.len(), 36, "trailing-hole truncation under the wide arm");
    assert_eq!(a.pop_value().unwrap().to_int(), 39);
    let mut visited = 0;
    a.for_each_value(|_| visited += 1);
    assert_eq!(visited, 35);
    a.clear().unwrap();
    assert!(a.is_empty());
}

// ── The §2.2.12 immutable array engine ────────────────────────
#[test]
fn array_snapshot_is_supported_only_on_the_immutable_engine() {
    assert_eq!(Array::new().snapshot().map(|_| ()), Err(ScalarError::SnapshotUnsupported));
}

#[cfg(feature = "imbl")]
#[test]
fn immutable_array_runs_the_semantic_battery() {
    // The container-verified semantics are engine-independent: holes, vivification, truncation, both-end
    // operations, and readonly all hold on the RRB engine.
    let a = Array::immutable();
    a.set(3, int(3)).unwrap();
    assert_eq!(a.len(), 4);
    assert!(!a.exists(0) && a.exists(3), "indices below a sparse set stay holes");
    a.ensure_element(0, |slot| {
        assert!(matches!(slot, Value::Undef | Value::UndefTainted));
    })
    .unwrap();
    assert!(a.exists(0), "vivified undef exists");

    a.unshift_value(int(100)).unwrap();
    a.push_value(int(9)).unwrap();
    assert_eq!(a.len(), 6);
    assert_eq!(a.shift_value().unwrap().to_int(), 100);
    assert_eq!(a.pop_value().unwrap().to_int(), 9);

    assert_eq!(a.delete(3).unwrap().to_int(), 3, "deleting the last element…");
    assert!(a.len() < 4, "…truncates through trailing holes");

    a.set_readonly(true);
    assert_eq!(a.push_value(int(1)), Err(ScalarError::ReadOnly));
    a.set_readonly(false);
    a.clear().unwrap();
    assert!(a.is_empty());
}

#[cfg(feature = "imbl")]
#[test]
fn immutable_array_snapshots_are_detached_diverging_copies() {
    let a = Array::immutable();
    for i in 0..4 {
        a.push_value(int(i)).unwrap();
    }

    let snap = a.snapshot().unwrap();
    a.push_value(int(4)).unwrap();
    a.shift_value().unwrap();
    snap.push_value(int(99)).unwrap();

    // The snapshot holds the moment: untouched by the original's divergence, diverging on its own.
    assert_eq!(snap.len(), 5);
    assert_eq!(snap.get(0).unwrap().to_int(), 0);
    assert_eq!(snap.get(4).unwrap().to_int(), 99);
    assert_eq!(a.len(), 4);
    assert_eq!(a.get(0).unwrap().to_int(), 1);
}

// ── The §2.2.13 bucket-engine discipline ──────────────────────
#[cfg(feature = "indexmap")]
#[test]
fn ordered_mode_iterates_in_insertion_order() {
    // The mode's reason to exist: a pinned, predictable order on explicit request.
    let h = Hash::ordered();
    for k in ["delta", "alpha", "omega", "beta"] {
        h.store(key(k), int(1)).unwrap();
    }
    let spelled: Vec<Vec<u8>> = h.keys().iter().map(|k| k.as_bytes(&mut [0u8; DECODE_MAX]).to_vec()).collect();
    assert_eq!(spelled, vec![b"delta".to_vec(), b"alpha".to_vec(), b"omega".to_vec(), b"beta".to_vec()]);
}

#[test]
fn value_update_does_not_disturb_each() {
    // Contract-specified safe: updating an existing key's value during iteration; the cursor must not reset (§2.2.13:
    // the find-first store path).
    for h in both_engines() {
        for k in ["a", "b", "c"] {
            h.store(key(k), int(1)).unwrap();
        }
        let first = h.each().expect("one of three");
        h.store(first.0.clone(), int(99)).unwrap();
        let mut rest = 0;
        while h.each().is_some() {
            rest += 1;
        }
        assert_eq!(rest, 2, "the update neither restarted nor truncated the pass");
        assert_eq!(h.get(&first.0).unwrap().to_int(), 99);
    }
}

#[test]
fn new_key_insertion_restarts_the_bucket_walk() {
    // §2.2.13: a rehash may scramble positions, so a new key resets the cursor — the post-insert pass is complete:
    // every current key appears, none twice.
    let h = Hash::new();
    for i in 0..8 {
        h.store(key(&format!("k{i}")), int(i)).unwrap();
    }
    let _ = h.each();
    let _ = h.each();
    h.store(key("fresh"), int(100)).unwrap();

    let mut seen = std::collections::BTreeSet::new();
    while let Some((k, _)) = h.each() {
        assert!(seen.insert(k.as_bytes(&mut [0u8; DECODE_MAX]).to_vec()), "no duplicates within the pass");
    }
    assert_eq!(seen.len(), 9, "the restarted pass covers every current key");
}

#[test]
fn bucket_delete_exactness_canary() {
    // The §2.2.13 canary: bucket-index stability across deletion is mechanically certain but contractually silent in
    // hashbrown's public docs.  Interleaved deletions at scale must leave the visit set exact — every surviving key
    // visited exactly once, every pre-visit-deleted key never visited.  A hashbrown behavior change fails this loudly.
    let h = Hash::new();
    let n = 300;
    for i in 0..n {
        h.store(key(&format!("k{i:03}")), int(i)).unwrap();
    }

    let mut visited = std::collections::BTreeSet::new();
    let mut deleted_unvisited = std::collections::BTreeSet::new();
    let mut step = 0;
    while let Some((k, _)) = h.each() {
        let spelled = k.as_bytes(&mut [0u8; DECODE_MAX]).to_vec();
        assert!(!deleted_unvisited.contains(&spelled), "a tombstoned unvisited entry must never be yielded");
        assert!(visited.insert(spelled), "no entry yields twice under delete-only interleaving");

        step += 1;

        if step % 3 == 0 {
            // Delete the current entry (the blessed idiom)...
            h.delete(&k).unwrap();
        }

        if step % 7 == 0 {
            // ...and an arbitrary not-yet-visited entry, when one exists.
            let target = (0..n).map(|i| format!("k{i:03}")).find(|s| {
                let ks = key(s);
                !visited.contains(s.as_bytes()) && h.exists(&ks)
            });
            if let Some(s) = target {
                h.delete(&key(&s)).unwrap();
                deleted_unvisited.insert(s.into_bytes());
            }
        }
    }

    let survivors: usize = (0..n).filter(|i| h.exists(&key(&format!("k{i:03}")))).count();
    assert_eq!(visited.len() + deleted_unvisited.len(), n as usize, "every key visited or deleted-unvisited");
    assert_eq!(h.len(), survivors);
}

// ── The §2.2.13 immutable engine ──────────────────────────────
#[test]
fn snapshot_is_supported_only_on_the_immutable_engine() {
    assert_eq!(Hash::new().snapshot().map(|_| ()), Err(ScalarError::SnapshotUnsupported));

    #[cfg(feature = "indexmap")]
    assert_eq!(Hash::ordered().snapshot().map(|_| ()), Err(ScalarError::SnapshotUnsupported));
}

#[cfg(feature = "imbl")]
#[test]
fn snapshots_are_detached_diverging_copies() {
    let h = Hash::immutable();
    for k in ["a", "b", "c"] {
        h.store(key(k), int(1)).unwrap();
    }

    let snap = h.snapshot().unwrap();
    h.store(key("d"), int(4)).unwrap();
    h.delete(&key("a")).unwrap();
    snap.store(key("z"), int(26)).unwrap();

    // The snapshot holds the moment: untouched by the original's divergence, diverging on its own.
    assert_eq!(snap.len(), 4);
    assert!(snap.exists(&key("a")) && snap.exists(&key("z")) && !snap.exists(&key("d")));
    assert_eq!(h.len(), 3);
    assert!(!h.exists(&key("a")) && h.exists(&key("d")) && !h.exists(&key("z")));
}

#[cfg(feature = "imbl")]
#[test]
fn immutable_each_revalidates_live() {
    // §2.2.13: the parked snapshot walk skips keys deleted since, reads values live, and holds new keys for the restart
    // — with no reset forced by any mutation.
    let h = Hash::immutable();
    for i in 0..6 {
        h.store(key(&format!("k{i}")), int(i)).unwrap();
    }

    let first = h.each().expect("six live");

    // Update the first-yielded key's value: a later pass reads live, and this pass is undisturbed.
    h.store(first.0.clone(), int(100)).unwrap();

    // Delete an unvisited key and insert a fresh one mid-walk.
    let mut deleted = None;
    let mut inserted_seen = false;
    let mut yielded = vec![first.0.clone()];
    while let Some((k, v)) = h.each() {
        if deleted.is_none() {
            // Choose a victim the walk has not yielded yet, if any remains.
            let victim = (0..6).map(|i| format!("k{i}")).find(|s| {
                let ks = key(s);
                ks.as_bytes(&mut [0u8; DECODE_MAX]) != k.as_bytes(&mut [0u8; DECODE_MAX])
                    && !yielded.iter().any(|y| y.as_bytes(&mut [0u8; DECODE_MAX]) == s.as_bytes())
                    && h.exists(&ks)
            });

            if let Some(s) = victim {
                h.delete(&key(&s)).unwrap();
                h.store(key("fresh"), int(7)).unwrap();
                deleted = Some(s);
            }
        }
        assert!(v.to_int() < 200, "values read live, never stale");
        if k.as_bytes(&mut [0u8; DECODE_MAX]) == b"fresh" {
            inserted_seen = true;
        }
        yielded.push(k);
    }

    let deleted = deleted.expect("a victim existed");
    assert!(!yielded.iter().any(|y| y.as_bytes(&mut [0u8; DECODE_MAX]) == deleted.as_bytes()), "deleted key skipped");
    assert!(!inserted_seen, "the inserted key waits for the restart");

    // The restart sees the live truth: five originals minus the deletion, plus the insertion.
    let mut restart = 0;
    while h.each().is_some() {
        restart += 1;
    }

    assert_eq!(restart, 6, "post-restart pass covers the live set");
}

// ── Handles ───────────────────────────────────────────────────
#[test]
fn handle_identity_and_traversal() {
    // Array is its own handle (§2.2.13): clones share the identity, locks are internal.
    let a = Array::new();
    let a2 = a.clone();
    assert!(Array::ptr_eq(&a, &a2));
    let b = Array::new();
    assert!(!Array::ptr_eq(&a, &b));
    assert_ne!(a.addr(), 0);

    a.push_value(int(1)).unwrap();
    a.push_value(int(2)).unwrap();
    assert_eq!(a2.len(), 2, "writes visible through the clone: shared identity");
    let mut sum = 0;
    a.for_each_value(|v| sum += v.to_int());
    assert_eq!(sum, 3, "collector hook");

    // Hash is its own handle (§2.2.13): clones share the identity, locks are internal.
    let h = Hash::new();
    let h2 = h.clone();
    h.store(key("k"), int(5)).unwrap();
    assert_eq!(h2.len(), 1, "writes visible through the clone: shared identity");
    assert!(Hash::ptr_eq(&h, &h2));
    assert_ne!(h.addr(), 0);
    let mut seen = 0;
    h.for_each_value(|_| seen += 1);
    assert_eq!(seen, 1);
}

#[test]
fn concurrency_foundation_send_sync() {
    // The utility-crate contract: every shared-capable type crosses threads.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PString>();
    assert_send_sync::<Value>();
    assert_send_sync::<Hash>();
    assert_send_sync::<Array>();
    assert_send_sync::<Referent>();

    // The immutable engine's map and parked iterator must cross threads with the rest.
    #[cfg(feature = "imbl")]
    assert_send_sync::<imbl::HashMap<PString, Value>>();

    #[cfg(feature = "imbl")]
    assert_send_sync::<imbl::Vector<ArraySlot>>();
}
