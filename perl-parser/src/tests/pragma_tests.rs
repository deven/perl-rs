//! Pragma tests.

use super::*;

// ── Bitflag mechanics ──

#[test]
fn features_empty_contains_nothing() {
    let f = Features::EMPTY;
    assert!(!f.contains(Features::SAY));
    assert!(!f.contains(Features::SIGNATURES));
}

#[test]
fn features_insert_and_contains() {
    let mut f = Features::EMPTY;
    f.insert(Features::SAY);
    assert!(f.contains(Features::SAY));
    assert!(!f.contains(Features::STATE));
}

#[test]
fn features_remove() {
    let mut f = Features::SAY | Features::STATE;
    f.remove(Features::SAY);
    assert!(!f.contains(Features::SAY));
    assert!(f.contains(Features::STATE));
}

#[test]
fn features_combined_contains() {
    let f = Features::SAY | Features::STATE | Features::SIGNATURES;
    assert!(f.contains(Features::SAY | Features::STATE));
    assert!(!f.contains(Features::SAY | Features::ISA));
}

// ── Name resolution ──

#[test]
fn resolve_known_feature_names() {
    assert_eq!(resolve_feature_name("say"), Some(Features::SAY));
    assert_eq!(resolve_feature_name("signatures"), Some(Features::SIGNATURES));
    assert_eq!(resolve_feature_name("postderef"), Some(Features::POSTDEREF));
    assert_eq!(resolve_feature_name("postderef_qq"), Some(Features::POSTDEREF_QQ));
    assert_ne!(Features::POSTDEREF, Features::POSTDEREF_QQ, "postderef and postderef_qq must be distinct");
    assert_eq!(resolve_feature_name("apostrophe_as_package_separator"), Some(Features::APOSTROPHE_AS_PACKAGE_SEPARATOR));
    assert_eq!(resolve_feature_name("smartmatch"), Some(Features::SMARTMATCH));
}

#[test]
fn resolve_unknown_feature_returns_none() {
    assert_eq!(resolve_feature_name("nope"), None);
}

#[test]
fn resolve_bundle_all_and_default() {
    assert_eq!(resolve_feature_name(":all"), Some(Features::ALL));
    assert_eq!(resolve_feature_name(":default"), Some(Features::DEFAULT));
}

#[test]
fn resolve_bundle_version() {
    assert_eq!(resolve_feature_name(":5.36"), Some(version_bundle(5, 36)));
    // Sub-version ignored per perlfeature.
    assert_eq!(resolve_feature_name(":5.36.0"), Some(version_bundle(5, 36)));
    assert_eq!(resolve_feature_name(":5.36.1"), Some(version_bundle(5, 36)));
}

// ── Default bundle ──

#[test]
fn default_bundle_matches_perlfeature() {
    let d = Features::DEFAULT;
    assert!(d.contains(Features::INDIRECT));
    assert!(d.contains(Features::MULTIDIMENSIONAL));
    assert!(d.contains(Features::BAREWORD_FILEHANDLES));
    assert!(d.contains(Features::APOSTROPHE_AS_PACKAGE_SEPARATOR));
    assert!(d.contains(Features::SMARTMATCH));
    assert!(!d.contains(Features::SAY));
    assert!(!d.contains(Features::SIGNATURES));
}

#[test]
fn pragmas_default_has_default_bundle() {
    let p = Pragmas::default();
    assert_eq!(p.features, Features::DEFAULT);
    assert!(!p.utf8);
}

// ── Version bundles: cross-check with perlfeature table ──

#[test]
fn bundle_below_5_10_is_default() {
    assert_eq!(version_bundle(5, 8), Features::DEFAULT);
    assert_eq!(version_bundle(4, 0), Features::DEFAULT);
}

#[test]
fn bundle_5_10() {
    // :5.10 = apostrophe_as_package_separator bareword_filehandles
    //         indirect multidimensional say smartmatch state switch
    let b = version_bundle(5, 10);
    let expected = Features::APOSTROPHE_AS_PACKAGE_SEPARATOR
        | Features::BAREWORD_FILEHANDLES
        | Features::INDIRECT
        | Features::MULTIDIMENSIONAL
        | Features::SAY
        | Features::SMARTMATCH
        | Features::STATE
        | Features::SWITCH;
    assert_eq!(b, expected);
}

#[test]
fn bundle_5_12_adds_unicode_strings() {
    let b = version_bundle(5, 12);
    assert!(b.contains(Features::UNICODE_STRINGS));
    assert!(b.contains(Features::SAY));
    assert!(!b.contains(Features::FC), "fc arrives in 5.16");
}

#[test]
fn bundle_5_16_adds_current_sub_evalbytes_fc_unicode_eval() {
    let b = version_bundle(5, 16);
    assert!(b.contains(Features::CURRENT_SUB));
    assert!(b.contains(Features::EVALBYTES));
    assert!(b.contains(Features::FC));
    assert!(b.contains(Features::UNICODE_EVAL));
    assert!(!b.contains(Features::POSTDEREF_QQ), "postderef_qq arrives in 5.24");
}

#[test]
fn bundle_5_24_adds_postderef_qq() {
    let b = version_bundle(5, 24);
    assert!(b.contains(Features::POSTDEREF_QQ));
    assert!(!b.contains(Features::BITWISE), "bitwise arrives in 5.28");
}

#[test]
fn bundle_5_28_adds_bitwise() {
    let b = version_bundle(5, 28);
    assert!(b.contains(Features::BITWISE));
}

#[test]
fn bundle_5_36_full_membership() {
    // Per perlfeature: :5.36 = apostrophe_as_package_separator
    // bareword_filehandles bitwise current_sub evalbytes fc isa
    // postderef_qq say signatures smartmatch state unicode_eval
    // unicode_strings.
    let expected = Features::APOSTROPHE_AS_PACKAGE_SEPARATOR
        | Features::BAREWORD_FILEHANDLES
        | Features::BITWISE
        | Features::CURRENT_SUB
        | Features::EVALBYTES
        | Features::FC
        | Features::ISA
        | Features::POSTDEREF_QQ
        | Features::SAY
        | Features::SIGNATURES
        | Features::SMARTMATCH
        | Features::STATE
        | Features::UNICODE_EVAL
        | Features::UNICODE_STRINGS;
    assert_eq!(version_bundle(5, 36), expected);
}

#[test]
fn bundle_5_36_drops_indirect_multidim_switch() {
    let b = version_bundle(5, 36);
    assert!(!b.contains(Features::INDIRECT));
    assert!(!b.contains(Features::MULTIDIMENSIONAL));
    assert!(!b.contains(Features::SWITCH));
}

#[test]
fn bundle_5_38_drops_bareword_filehandles_adds_module_true() {
    let b = version_bundle(5, 38);
    assert!(b.contains(Features::MODULE_TRUE));
    assert!(!b.contains(Features::BAREWORD_FILEHANDLES));
}

#[test]
fn bundle_5_40_adds_try() {
    let b = version_bundle(5, 40);
    assert!(b.contains(Features::TRY));
    assert!(b.contains(Features::SMARTMATCH), "smartmatch still in 5.40");
}

#[test]
fn bundle_5_42_drops_apostrophe_and_smartmatch() {
    let b = version_bundle(5, 42);
    assert!(!b.contains(Features::APOSTROPHE_AS_PACKAGE_SEPARATOR));
    assert!(!b.contains(Features::SMARTMATCH));
    assert!(b.contains(Features::TRY));
}

#[test]
fn bundle_odd_minor_rounds_down() {
    assert_eq!(version_bundle(5, 35), version_bundle(5, 34));
    assert_eq!(version_bundle(5, 37), version_bundle(5, 36));
}

#[test]
fn bundle_above_known_uses_latest() {
    assert_eq!(version_bundle(5, 100), version_bundle(5, 42));
}

#[test]
fn apply_version_bundle_resets_not_unions() {
    // `use v5.36` = implicit `no feature ':all'; use feature ':5.36';`.
    let mut f = Features::EMPTY;
    f.insert(Features::KEYWORD_ANY);
    f.apply_version_bundle(5, 36);
    assert_eq!(f, version_bundle(5, 36));
    assert!(!f.contains(Features::KEYWORD_ANY));
}

// ── ALL bundle sanity ──

#[test]
fn all_bundle_contains_every_feature() {
    let all = Features::ALL;
    assert!(all.contains(Features::SAY));
    assert!(all.contains(Features::SIGNATURES));
    assert!(all.contains(Features::TRY));
    assert!(all.contains(Features::CLASS));
    assert!(all.contains(Features::KEYWORD_ANY));
    assert!(all.contains(Features::KEYWORD_ALL));
}
