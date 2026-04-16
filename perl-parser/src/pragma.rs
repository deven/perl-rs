//! Lexically-scoped pragma state that affects parsing.
//!
//! Perl's `use feature`, `use utf8`, and version bundles
//! (`use v5.36`) all influence how source code is parsed.  The
//! parser tracks them in [`Pragmas`], which is saved on block entry
//! and restored on block exit so the effect remains lexical.
//!
//! The feature set and bundle contents are modeled on the
//! `perlfeature` manpage; see the module tests for the exact
//! bundle membership.
//!
//! ## Phase 1 scope
//!
//! This module tracks state only; no existing parsing behavior
//! changes based on the recorded flags yet.  Future phases will
//! consult [`Pragmas::features`] to, e.g., choose between prototype
//! and signature parsing for `sub foo (...)`, or to enable
//! postderef syntax.

use std::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not};

/// Set of features enabled via `use feature`.
///
/// Stored as a bit-set for cheap copy (save/restore across block
/// boundaries is the hot path).  Each feature has an associated
/// constant; operator overloads let bundles be expressed naturally
/// as `Features::SAY | Features::STATE | Features::SIGNATURES`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features(u32);

impl Features {
    /// No features active.  Not the same as the `:default` bundle —
    /// see [`Features::DEFAULT`] for the pre-`use feature` baseline.
    pub const EMPTY: Features = Features(0);

    // ── Individual features ───────────────────────────────────

    /// `say` — makes `say` a keyword (5.10+).
    pub const SAY: Features = Features(1 << 0);
    /// `state` — makes `state` a keyword (5.10+).
    pub const STATE: Features = Features(1 << 1);
    /// `switch` — enables `given`/`when`/`default` (5.10, removed
    /// from the `:5.36` bundle and later).
    pub const SWITCH: Features = Features(1 << 2);
    /// `smartmatch` — the `~~` operator.  Enabled by default
    /// through the `:5.40` bundle, removed from `:5.42`.
    pub const SMARTMATCH: Features = Features(1 << 3);
    /// `evalbytes` — makes `evalbytes` a keyword (5.16+).
    pub const EVALBYTES: Features = Features(1 << 4);
    /// `current_sub` — makes `__SUB__` work (5.16+).
    pub const CURRENT_SUB: Features = Features(1 << 5);
    /// `fc` — makes `fc` a keyword (5.16+).
    pub const FC: Features = Features(1 << 6);
    /// `postderef` — the historical name for postfix dereference
    /// syntax *outside* interpolating strings.  As of Perl 5.24
    /// the syntax is always on regardless of this flag, so the
    /// bit is effectively cosmetic: enabling or disabling it has
    /// no parsing effect.  Kept as a distinct flag from
    /// [`Features::POSTDEREF_QQ`] so that `use feature 'postderef';`
    /// doesn't accidentally enable the qq extension.
    pub const POSTDEREF: Features = Features(1 << 7);
    /// `postderef_qq` — postfix dereference inside interpolating
    /// strings (5.20+, stable in `:5.24`+).  This is the flag
    /// that actually gates parsing behavior; plain `postderef` is
    /// a no-op but distinct.
    pub const POSTDEREF_QQ: Features = Features(1 << 27);
    /// `signatures` — `sub foo ($x, $y) { ... }` parses as a
    /// signature rather than a prototype (stable in `:5.36`+).
    pub const SIGNATURES: Features = Features(1 << 8);
    /// `refaliasing` — `\$x = \$y;` (experimental, 5.22+).
    pub const REFALIASING: Features = Features(1 << 9);
    /// `declared_refs` — `my \$x = \$y;` (5.26+).
    pub const DECLARED_REFS: Features = Features(1 << 10);
    /// `isa` — the `isa` infix operator (stable in `:5.36`+).
    pub const ISA: Features = Features(1 << 11);
    /// `try` — `try { ... } catch ($e) { ... }` (enters the
    /// `:5.40` bundle).
    pub const TRY: Features = Features(1 << 12);
    /// `defer` — `defer { ... }` (experimental, 5.36+).
    pub const DEFER: Features = Features(1 << 13);
    /// `class` — `class Name { field $x; method ... }`
    /// (experimental, 5.38+).
    pub const CLASS: Features = Features(1 << 14);
    /// `extra_paired_delimiters` — more delimiter pairs for
    /// quote-like operators (experimental, 5.36+).
    pub const EXTRA_PAIRED_DELIMITERS: Features = Features(1 << 15);
    /// `bareword_filehandles` — bareword filehandles recognized;
    /// in `:default`, dropped from `:5.38` and later.
    pub const BAREWORD_FILEHANDLES: Features = Features(1 << 16);
    /// `indirect` — indirect method call syntax (`new Foo`);
    /// in `:default`, dropped from `:5.36` and later.
    pub const INDIRECT: Features = Features(1 << 17);
    /// `apostrophe_as_package_separator` — `'` acts as `::` in
    /// source-level names.  In `:default`, dropped from `:5.42`.
    pub const APOSTROPHE_AS_PACKAGE_SEPARATOR: Features = Features(1 << 18);
    /// `multidimensional` — `$h{a,b}` lookup emulation;
    /// in `:default`, dropped from `:5.36` and later.
    pub const MULTIDIMENSIONAL: Features = Features(1 << 19);
    /// `unicode_strings` (5.12+).
    pub const UNICODE_STRINGS: Features = Features(1 << 20);
    /// `unicode_eval` (5.16+).
    pub const UNICODE_EVAL: Features = Features(1 << 21);
    /// `bitwise` — numeric vs. string-bitwise op selection
    /// (stable in `:5.28`+).
    pub const BITWISE: Features = Features(1 << 22);
    /// `module_true` — modules don't need an explicit trailing
    /// true value (enters the `:5.38` bundle).
    pub const MODULE_TRUE: Features = Features(1 << 23);
    /// `lexical_subs` — `my sub` etc.  Enabled unconditionally
    /// since 5.26, tracked here for pre-5.26 source.
    pub const LEXICAL_SUBS: Features = Features(1 << 24);
    /// `keyword_any` — experimental `any` operator.
    pub const KEYWORD_ANY: Features = Features(1 << 25);
    /// `keyword_all` — experimental `all` operator.
    pub const KEYWORD_ALL: Features = Features(1 << 26);

    // ── Pre-made bundles ──────────────────────────────────────

    /// The `:default` bundle — features active before any
    /// `use feature` or `use vN.M` declaration.
    pub const DEFAULT: Features =
        Features(Self::INDIRECT.0 | Self::MULTIDIMENSIONAL.0 | Self::BAREWORD_FILEHANDLES.0 | Self::APOSTROPHE_AS_PACKAGE_SEPARATOR.0 | Self::SMARTMATCH.0);

    /// Every known feature.  Used for `use feature ':all'`.
    pub const ALL: Features = Features(
        Self::SAY.0
            | Self::STATE.0
            | Self::SWITCH.0
            | Self::SMARTMATCH.0
            | Self::EVALBYTES.0
            | Self::CURRENT_SUB.0
            | Self::FC.0
            | Self::POSTDEREF.0
            | Self::POSTDEREF_QQ.0
            | Self::SIGNATURES.0
            | Self::REFALIASING.0
            | Self::DECLARED_REFS.0
            | Self::ISA.0
            | Self::TRY.0
            | Self::DEFER.0
            | Self::CLASS.0
            | Self::EXTRA_PAIRED_DELIMITERS.0
            | Self::BAREWORD_FILEHANDLES.0
            | Self::INDIRECT.0
            | Self::APOSTROPHE_AS_PACKAGE_SEPARATOR.0
            | Self::MULTIDIMENSIONAL.0
            | Self::UNICODE_STRINGS.0
            | Self::UNICODE_EVAL.0
            | Self::BITWISE.0
            | Self::MODULE_TRUE.0
            | Self::LEXICAL_SUBS.0
            | Self::KEYWORD_ANY.0
            | Self::KEYWORD_ALL.0,
    );

    // ── Predicates / mutation ─────────────────────────────────

    /// True if all features in `other` are also in `self`.
    pub const fn contains(self, other: Features) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Enable the features in `other`.
    pub fn insert(&mut self, other: Features) {
        self.0 |= other.0;
    }

    /// Disable the features in `other`.
    pub fn remove(&mut self, other: Features) {
        self.0 &= !other.0;
    }

    /// Apply a version bundle.  `use v5.36` or `use 5.036` both
    /// arrive here as `(5, 36)`.  Replaces `self` with exactly the
    /// bundle's feature set — matches Perl's "use vN.M does an
    /// implicit `no feature ':all'; use feature ':5.N';`"
    /// behavior.
    ///
    /// Versions below 5.10 load the `:default` bundle.
    pub fn apply_version_bundle(&mut self, major: u32, minor: u32) {
        *self = version_bundle(major, minor);
    }
}

/// Look up a feature or bundle name as written in `use feature
/// 'NAME'`.  Handles both individual features and bundle
/// aliases (`:all`, `:default`, `:5.N`).  Returns `None` for
/// unknown names.
pub fn resolve_feature_name(name: &str) -> Option<Features> {
    // Bundle alias: `:all`, `:default`, `:5.N`, `:5.N.M`.
    if let Some(tail) = name.strip_prefix(':') {
        return resolve_bundle_alias(tail);
    }
    Some(match name {
        "say" => Features::SAY,
        "state" => Features::STATE,
        "switch" => Features::SWITCH,
        "smartmatch" => Features::SMARTMATCH,
        "evalbytes" => Features::EVALBYTES,
        "current_sub" => Features::CURRENT_SUB,
        "fc" => Features::FC,
        // `postderef` and `postderef_qq` are distinct.  Plain
        // `postderef` is historical and effectively cosmetic
        // (syntax is always on since 5.24); `postderef_qq` is
        // the flag that actually gates interpolation behavior.
        "postderef" => Features::POSTDEREF,
        "postderef_qq" => Features::POSTDEREF_QQ,
        "signatures" => Features::SIGNATURES,
        "refaliasing" => Features::REFALIASING,
        "declared_refs" => Features::DECLARED_REFS,
        "isa" => Features::ISA,
        "try" => Features::TRY,
        "defer" => Features::DEFER,
        "class" => Features::CLASS,
        "extra_paired_delimiters" => Features::EXTRA_PAIRED_DELIMITERS,
        "bareword_filehandles" => Features::BAREWORD_FILEHANDLES,
        "indirect" => Features::INDIRECT,
        "apostrophe_as_package_separator" => Features::APOSTROPHE_AS_PACKAGE_SEPARATOR,
        "multidimensional" => Features::MULTIDIMENSIONAL,
        "unicode_strings" => Features::UNICODE_STRINGS,
        "unicode_eval" => Features::UNICODE_EVAL,
        "bitwise" => Features::BITWISE,
        "module_true" => Features::MODULE_TRUE,
        "lexical_subs" => Features::LEXICAL_SUBS,
        "keyword_any" => Features::KEYWORD_ANY,
        "keyword_all" => Features::KEYWORD_ALL,
        _ => return None,
    })
}

/// Resolve the tail of a `:NAME` bundle alias.  `:all` and
/// `:default` are named; `:5.N[.M]` parses as a version bundle
/// (sub-version ignored per perlfeature).
fn resolve_bundle_alias(tail: &str) -> Option<Features> {
    match tail {
        "all" => return Some(Features::ALL),
        "default" => return Some(Features::DEFAULT),
        _ => {}
    }
    // `5.N` or `5.N.M` — parse first two dotted components.
    let mut it = tail.split('.');
    let major: u32 = it.next()?.parse().ok()?;
    let minor: u32 = it.next()?.parse().ok()?;
    // Third component, if present, is ignored per perlfeature.
    Some(version_bundle(major, minor))
}

/// Compute the feature set for a `use vN.M` bundle.
///
/// Bundles below 5.10 load `:default`.  Odd minor versions round
/// down (per perlfeature: development versions share the previous
/// stable bundle).  This table is built directly from the
/// `perlfeature` "FEATURE BUNDLES" table.
fn version_bundle(major: u32, minor: u32) -> Features {
    if major < 5 || (major == 5 && minor < 10) {
        return Features::DEFAULT;
    }
    if major != 5 {
        // Unknown major — fall back to latest known.
        return bundle_5_42();
    }
    // Odd minors map to the prior even minor.
    let minor = if minor % 2 == 1 { minor - 1 } else { minor };
    match minor {
        10 | 12 | 14 => bundle_5_10_through_14(minor),
        16 | 18 | 20 | 22 => bundle_5_16_through_22(),
        24 | 26 => bundle_5_24_through_26(),
        28 | 30 | 32 | 34 => bundle_5_28_through_34(),
        36 => bundle_5_36(),
        38 => bundle_5_38(),
        40 => bundle_5_40(),
        42 => bundle_5_42(),
        _ if minor > 42 => bundle_5_42(),
        _ => Features::DEFAULT,
    }
}

/// 5.10 / 5.12 / 5.14 bundle base.  5.12 adds `unicode_strings`;
/// 5.14 is identical to 5.12.
fn bundle_5_10_through_14(minor: u32) -> Features {
    let mut b = Features::APOSTROPHE_AS_PACKAGE_SEPARATOR
        | Features::BAREWORD_FILEHANDLES
        | Features::INDIRECT
        | Features::MULTIDIMENSIONAL
        | Features::SAY
        | Features::SMARTMATCH
        | Features::STATE
        | Features::SWITCH;
    if minor >= 12 {
        b |= Features::UNICODE_STRINGS;
    }
    b
}

/// 5.16 through 5.22 — adds current_sub, evalbytes, fc, unicode_eval.
fn bundle_5_16_through_22() -> Features {
    bundle_5_10_through_14(14) | Features::CURRENT_SUB | Features::EVALBYTES | Features::FC | Features::UNICODE_EVAL
}

/// 5.24 / 5.26 — adds postderef_qq.
fn bundle_5_24_through_26() -> Features {
    bundle_5_16_through_22() | Features::POSTDEREF_QQ
}

/// 5.28 through 5.34 — adds bitwise.
fn bundle_5_28_through_34() -> Features {
    bundle_5_24_through_26() | Features::BITWISE
}

/// 5.36 — drops indirect, multidimensional, switch; adds isa, signatures.
fn bundle_5_36() -> Features {
    let mut b = bundle_5_28_through_34();
    b.remove(Features::INDIRECT);
    b.remove(Features::MULTIDIMENSIONAL);
    b.remove(Features::SWITCH);
    b | Features::ISA | Features::SIGNATURES
}

/// 5.38 — drops bareword_filehandles; adds module_true.
fn bundle_5_38() -> Features {
    let mut b = bundle_5_36();
    b.remove(Features::BAREWORD_FILEHANDLES);
    b | Features::MODULE_TRUE
}

/// 5.40 — adds try.
fn bundle_5_40() -> Features {
    bundle_5_38() | Features::TRY
}

/// 5.42 — drops apostrophe_as_package_separator, smartmatch.
fn bundle_5_42() -> Features {
    let mut b = bundle_5_40();
    b.remove(Features::APOSTROPHE_AS_PACKAGE_SEPARATOR);
    b.remove(Features::SMARTMATCH);
    b
}

// ── Bit-op traits ─────────────────────────────────────────────

impl BitOr for Features {
    type Output = Features;
    fn bitor(self, rhs: Features) -> Features {
        Features(self.0 | rhs.0)
    }
}

impl BitOrAssign for Features {
    fn bitor_assign(&mut self, rhs: Features) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for Features {
    type Output = Features;
    fn bitand(self, rhs: Features) -> Features {
        Features(self.0 & rhs.0)
    }
}

impl BitAndAssign for Features {
    fn bitand_assign(&mut self, rhs: Features) {
        self.0 &= rhs.0;
    }
}

impl Not for Features {
    type Output = Features;
    fn not(self) -> Features {
        Features(!self.0)
    }
}

// ── Pragmas aggregate ─────────────────────────────────────────

/// All parser-visible pragma state for the current lexical scope.
///
/// Currently tracks the `feature` pragma (via [`Features`]) and the
/// `utf8` pragma.  Future pragmas that affect parsing would be
/// added here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pragmas {
    pub features: Features,
    /// `use utf8` — source treated as UTF-8.  Identifiers may
    /// contain non-ASCII characters, string literal bytes are
    /// interpreted as Unicode code points.
    pub utf8: bool,
}

impl Pragmas {
    /// Default parser state: features = `:default` bundle, no
    /// `use utf8`.
    pub const fn new() -> Self {
        Pragmas { features: Features::DEFAULT, utf8: false }
    }
}

impl Default for Pragmas {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
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
}
