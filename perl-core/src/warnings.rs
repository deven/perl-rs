// Perl's warning-category tree (§2.3.4): the vocabulary every warning enum's `category` speaks, and the index space
// warning pragmas compile into.  Extracted from perl 5.44.0's `regen/warnings.pl` tree joined with
// `%warnings::Offsets`, with the default-enabled set cross-checked against `$warnings::DEFAULT` — the discriminants are
// perl's own bit numbers, so a pragma bit vector indexes by discriminant with no translation.

use std::fmt;

/// One of perl's 81 warning categories.  The discriminant is perl's bit number; [`WarningCategory::name`] is perl's
/// spelling; [`WarningCategory::parent`] encodes the tree (`use warnings 'io'` enables the whole `io` subtree), with
/// `all` at the root.  Categories are vocabulary, not messages: every warning enum names its category through
/// [`PerlWarning::category`], and the interpreter's pragma state is a bit vector indexed by these discriminants.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum WarningCategory {
    /// `all`
    All = 0,

    /// `closure`
    Closure = 1,

    /// `deprecated` — enabled by default (`ckWARN_d` semantics).
    Deprecated = 2,

    /// `exiting`
    Exiting = 3,

    /// `glob` — enabled by default (`ckWARN_d` semantics).
    Glob = 4,

    /// `io`
    Io = 5,

    /// `closed`
    Closed = 6,

    /// `exec`
    Exec = 7,

    /// `layer`
    Layer = 8,

    /// `newline`
    Newline = 9,

    /// `pipe`
    Pipe = 10,

    /// `unopened`
    Unopened = 11,

    /// `misc`
    Misc = 12,

    /// `numeric`
    Numeric = 13,

    /// `once`
    Once = 14,

    /// `overflow`
    Overflow = 15,

    /// `pack`
    Pack = 16,

    /// `portable`
    Portable = 17,

    /// `recursion`
    Recursion = 18,

    /// `redefine`
    Redefine = 19,

    /// `regexp`
    Regexp = 20,

    /// `severe`
    Severe = 21,

    /// `debugging` — enabled by default (`ckWARN_d` semantics).
    Debugging = 22,

    /// `inplace` — enabled by default (`ckWARN_d` semantics).
    Inplace = 23,

    /// `internal`
    Internal = 24,

    /// `malloc` — enabled by default (`ckWARN_d` semantics).
    Malloc = 25,

    /// `signal`
    Signal = 26,

    /// `substr`
    Substr = 27,

    /// `syntax`
    Syntax = 28,

    /// `ambiguous`
    Ambiguous = 29,

    /// `bareword`
    Bareword = 30,

    /// `digit`
    Digit = 31,

    /// `parenthesis`
    Parenthesis = 32,

    /// `precedence`
    Precedence = 33,

    /// `printf`
    Printf = 34,

    /// `prototype`
    Prototype = 35,

    /// `qw`
    Qw = 36,

    /// `reserved`
    Reserved = 37,

    /// `semicolon`
    Semicolon = 38,

    /// `taint`
    Taint = 39,

    /// `threads`
    Threads = 40,

    /// `uninitialized`
    Uninitialized = 41,

    /// `unpack`
    Unpack = 42,

    /// `untie`
    Untie = 43,

    /// `utf8`
    Utf8 = 44,

    /// `void`
    Void = 45,

    /// `imprecision`
    Imprecision = 46,

    /// `illegalproto`
    Illegalproto = 47,

    /// `deprecated::unicode_property_name` — enabled by default (`ckWARN_d` semantics).
    DeprecatedUnicodePropertyName = 48,

    /// `non_unicode`
    NonUnicode = 49,

    /// `nonchar`
    Nonchar = 50,

    /// `surrogate`
    Surrogate = 51,

    /// `experimental`
    Experimental = 52,

    /// `experimental::regex_sets`
    ExperimentalRegexSets = 53,

    /// `syscalls`
    Syscalls = 54,

    /// `experimental::re_strict` — enabled by default (`ckWARN_d` semantics).
    ExperimentalReStrict = 55,

    /// `experimental::refaliasing` — enabled by default (`ckWARN_d` semantics).
    ExperimentalRefaliasing = 56,

    /// `locale` — enabled by default (`ckWARN_d` semantics).
    Locale = 57,

    /// `missing`
    Missing = 58,

    /// `redundant`
    Redundant = 59,

    /// `experimental::declared_refs` — enabled by default (`ckWARN_d` semantics).
    ExperimentalDeclaredRefs = 60,

    /// `deprecated::dot_in_inc` — enabled by default (`ckWARN_d` semantics).
    DeprecatedDotInInc = 61,

    /// `shadow`
    Shadow = 62,

    /// `experimental::private_use` — enabled by default (`ckWARN_d` semantics).
    ExperimentalPrivateUse = 63,

    /// `experimental::uniprop_wildcards` — enabled by default (`ckWARN_d` semantics).
    ExperimentalUnipropWildcards = 64,

    /// `experimental::vlb` — enabled by default (`ckWARN_d` semantics).
    ExperimentalVlb = 65,

    /// `experimental::try` — enabled by default (`ckWARN_d` semantics).
    ExperimentalTry = 66,

    /// `experimental::args_array_with_signatures` — enabled by default (`ckWARN_d` semantics).
    ExperimentalArgsArrayWithSignatures = 67,

    /// `experimental::builtin` — enabled by default (`ckWARN_d` semantics).
    ExperimentalBuiltin = 68,

    /// `experimental::defer` — enabled by default (`ckWARN_d` semantics).
    ExperimentalDefer = 69,

    /// `experimental::extra_paired_delimiters` — enabled by default (`ckWARN_d` semantics).
    ExperimentalExtraPairedDelimiters = 70,

    /// `scalar`
    Scalar = 71,

    /// `deprecated::version_downgrade` — enabled by default (`ckWARN_d` semantics).
    DeprecatedVersionDowngrade = 72,

    /// `deprecated::delimiter_will_be_paired` — enabled by default (`ckWARN_d` semantics).
    DeprecatedDelimiterWillBePaired = 73,

    /// `experimental::class` — enabled by default (`ckWARN_d` semantics).
    ExperimentalClass = 74,

    /// `deprecated::subsequent_use_version` — enabled by default (`ckWARN_d` semantics).
    DeprecatedSubsequentUseVersion = 75,

    /// `experimental::keyword_all` — enabled by default (`ckWARN_d` semantics).
    ExperimentalKeywordAll = 76,

    /// `experimental::keyword_any` — enabled by default (`ckWARN_d` semantics).
    ExperimentalKeywordAny = 77,

    /// `experimental::enhanced_xx` — enabled by default (`ckWARN_d` semantics).
    ExperimentalEnhancedXx = 78,

    /// `experimental::signature_named_parameters` — enabled by default (`ckWARN_d` semantics).
    ExperimentalSignatureNamedParameters = 79,

    /// `missing_import` — enabled by default (`ckWARN_d` semantics).
    MissingImport = 80,
}

/// The number of categories: bit vectors sized to this cover every discriminant.
pub const WARNING_CATEGORY_COUNT: usize = 81;

impl WarningCategory {
    /// Perl's spelling, as `use warnings '...'` writes it.
    pub fn name(self) -> &'static str {
        match self {
            WarningCategory::All => "all",
            WarningCategory::Closure => "closure",
            WarningCategory::Deprecated => "deprecated",
            WarningCategory::Exiting => "exiting",
            WarningCategory::Glob => "glob",
            WarningCategory::Io => "io",
            WarningCategory::Closed => "closed",
            WarningCategory::Exec => "exec",
            WarningCategory::Layer => "layer",
            WarningCategory::Newline => "newline",
            WarningCategory::Pipe => "pipe",
            WarningCategory::Unopened => "unopened",
            WarningCategory::Misc => "misc",
            WarningCategory::Numeric => "numeric",
            WarningCategory::Once => "once",
            WarningCategory::Overflow => "overflow",
            WarningCategory::Pack => "pack",
            WarningCategory::Portable => "portable",
            WarningCategory::Recursion => "recursion",
            WarningCategory::Redefine => "redefine",
            WarningCategory::Regexp => "regexp",
            WarningCategory::Severe => "severe",
            WarningCategory::Debugging => "debugging",
            WarningCategory::Inplace => "inplace",
            WarningCategory::Internal => "internal",
            WarningCategory::Malloc => "malloc",
            WarningCategory::Signal => "signal",
            WarningCategory::Substr => "substr",
            WarningCategory::Syntax => "syntax",
            WarningCategory::Ambiguous => "ambiguous",
            WarningCategory::Bareword => "bareword",
            WarningCategory::Digit => "digit",
            WarningCategory::Parenthesis => "parenthesis",
            WarningCategory::Precedence => "precedence",
            WarningCategory::Printf => "printf",
            WarningCategory::Prototype => "prototype",
            WarningCategory::Qw => "qw",
            WarningCategory::Reserved => "reserved",
            WarningCategory::Semicolon => "semicolon",
            WarningCategory::Taint => "taint",
            WarningCategory::Threads => "threads",
            WarningCategory::Uninitialized => "uninitialized",
            WarningCategory::Unpack => "unpack",
            WarningCategory::Untie => "untie",
            WarningCategory::Utf8 => "utf8",
            WarningCategory::Void => "void",
            WarningCategory::Imprecision => "imprecision",
            WarningCategory::Illegalproto => "illegalproto",
            WarningCategory::DeprecatedUnicodePropertyName => "deprecated::unicode_property_name",
            WarningCategory::NonUnicode => "non_unicode",
            WarningCategory::Nonchar => "nonchar",
            WarningCategory::Surrogate => "surrogate",
            WarningCategory::Experimental => "experimental",
            WarningCategory::ExperimentalRegexSets => "experimental::regex_sets",
            WarningCategory::Syscalls => "syscalls",
            WarningCategory::ExperimentalReStrict => "experimental::re_strict",
            WarningCategory::ExperimentalRefaliasing => "experimental::refaliasing",
            WarningCategory::Locale => "locale",
            WarningCategory::Missing => "missing",
            WarningCategory::Redundant => "redundant",
            WarningCategory::ExperimentalDeclaredRefs => "experimental::declared_refs",
            WarningCategory::DeprecatedDotInInc => "deprecated::dot_in_inc",
            WarningCategory::Shadow => "shadow",
            WarningCategory::ExperimentalPrivateUse => "experimental::private_use",
            WarningCategory::ExperimentalUnipropWildcards => "experimental::uniprop_wildcards",
            WarningCategory::ExperimentalVlb => "experimental::vlb",
            WarningCategory::ExperimentalTry => "experimental::try",
            WarningCategory::ExperimentalArgsArrayWithSignatures => "experimental::args_array_with_signatures",
            WarningCategory::ExperimentalBuiltin => "experimental::builtin",
            WarningCategory::ExperimentalDefer => "experimental::defer",
            WarningCategory::ExperimentalExtraPairedDelimiters => "experimental::extra_paired_delimiters",
            WarningCategory::Scalar => "scalar",
            WarningCategory::DeprecatedVersionDowngrade => "deprecated::version_downgrade",
            WarningCategory::DeprecatedDelimiterWillBePaired => "deprecated::delimiter_will_be_paired",
            WarningCategory::ExperimentalClass => "experimental::class",
            WarningCategory::DeprecatedSubsequentUseVersion => "deprecated::subsequent_use_version",
            WarningCategory::ExperimentalKeywordAll => "experimental::keyword_all",
            WarningCategory::ExperimentalKeywordAny => "experimental::keyword_any",
            WarningCategory::ExperimentalEnhancedXx => "experimental::enhanced_xx",
            WarningCategory::ExperimentalSignatureNamedParameters => "experimental::signature_named_parameters",
            WarningCategory::MissingImport => "missing_import",
        }
    }

    /// The tree: each category's parent, `None` at the root.  Perl's warning names are not hierarchical namespaces —
    /// `io::pipe` does not exist — but enabling a parent enables its subtree's bits.
    pub fn parent(self) -> Option<WarningCategory> {
        match self {
            WarningCategory::All => None,
            WarningCategory::Closure => Some(WarningCategory::All),
            WarningCategory::Deprecated => Some(WarningCategory::All),
            WarningCategory::Exiting => Some(WarningCategory::All),
            WarningCategory::Glob => Some(WarningCategory::All),
            WarningCategory::Io => Some(WarningCategory::All),
            WarningCategory::Closed => Some(WarningCategory::Io),
            WarningCategory::Exec => Some(WarningCategory::Io),
            WarningCategory::Layer => Some(WarningCategory::Io),
            WarningCategory::Newline => Some(WarningCategory::Io),
            WarningCategory::Pipe => Some(WarningCategory::Io),
            WarningCategory::Unopened => Some(WarningCategory::Io),
            WarningCategory::Misc => Some(WarningCategory::All),
            WarningCategory::Numeric => Some(WarningCategory::All),
            WarningCategory::Once => Some(WarningCategory::All),
            WarningCategory::Overflow => Some(WarningCategory::All),
            WarningCategory::Pack => Some(WarningCategory::All),
            WarningCategory::Portable => Some(WarningCategory::All),
            WarningCategory::Recursion => Some(WarningCategory::All),
            WarningCategory::Redefine => Some(WarningCategory::All),
            WarningCategory::Regexp => Some(WarningCategory::All),
            WarningCategory::Severe => Some(WarningCategory::All),
            WarningCategory::Debugging => Some(WarningCategory::Severe),
            WarningCategory::Inplace => Some(WarningCategory::Severe),
            WarningCategory::Internal => Some(WarningCategory::Severe),
            WarningCategory::Malloc => Some(WarningCategory::Severe),
            WarningCategory::Signal => Some(WarningCategory::All),
            WarningCategory::Substr => Some(WarningCategory::All),
            WarningCategory::Syntax => Some(WarningCategory::All),
            WarningCategory::Ambiguous => Some(WarningCategory::Syntax),
            WarningCategory::Bareword => Some(WarningCategory::Syntax),
            WarningCategory::Digit => Some(WarningCategory::Syntax),
            WarningCategory::Parenthesis => Some(WarningCategory::Syntax),
            WarningCategory::Precedence => Some(WarningCategory::Syntax),
            WarningCategory::Printf => Some(WarningCategory::Syntax),
            WarningCategory::Prototype => Some(WarningCategory::Syntax),
            WarningCategory::Qw => Some(WarningCategory::Syntax),
            WarningCategory::Reserved => Some(WarningCategory::Syntax),
            WarningCategory::Semicolon => Some(WarningCategory::Syntax),
            WarningCategory::Taint => Some(WarningCategory::All),
            WarningCategory::Threads => Some(WarningCategory::All),
            WarningCategory::Uninitialized => Some(WarningCategory::All),
            WarningCategory::Unpack => Some(WarningCategory::All),
            WarningCategory::Untie => Some(WarningCategory::All),
            WarningCategory::Utf8 => Some(WarningCategory::All),
            WarningCategory::Void => Some(WarningCategory::All),
            WarningCategory::Imprecision => Some(WarningCategory::All),
            WarningCategory::Illegalproto => Some(WarningCategory::Syntax),
            WarningCategory::DeprecatedUnicodePropertyName => Some(WarningCategory::Deprecated),
            WarningCategory::NonUnicode => Some(WarningCategory::Utf8),
            WarningCategory::Nonchar => Some(WarningCategory::Utf8),
            WarningCategory::Surrogate => Some(WarningCategory::Utf8),
            WarningCategory::Experimental => Some(WarningCategory::All),
            WarningCategory::ExperimentalRegexSets => Some(WarningCategory::Experimental),
            WarningCategory::Syscalls => Some(WarningCategory::Io),
            WarningCategory::ExperimentalReStrict => Some(WarningCategory::Experimental),
            WarningCategory::ExperimentalRefaliasing => Some(WarningCategory::Experimental),
            WarningCategory::Locale => Some(WarningCategory::All),
            WarningCategory::Missing => Some(WarningCategory::All),
            WarningCategory::Redundant => Some(WarningCategory::All),
            WarningCategory::ExperimentalDeclaredRefs => Some(WarningCategory::Experimental),
            WarningCategory::DeprecatedDotInInc => Some(WarningCategory::Deprecated),
            WarningCategory::Shadow => Some(WarningCategory::All),
            WarningCategory::ExperimentalPrivateUse => Some(WarningCategory::Experimental),
            WarningCategory::ExperimentalUnipropWildcards => Some(WarningCategory::Experimental),
            WarningCategory::ExperimentalVlb => Some(WarningCategory::Experimental),
            WarningCategory::ExperimentalTry => Some(WarningCategory::Experimental),
            WarningCategory::ExperimentalArgsArrayWithSignatures => Some(WarningCategory::Experimental),
            WarningCategory::ExperimentalBuiltin => Some(WarningCategory::Experimental),
            WarningCategory::ExperimentalDefer => Some(WarningCategory::Experimental),
            WarningCategory::ExperimentalExtraPairedDelimiters => Some(WarningCategory::Experimental),
            WarningCategory::Scalar => Some(WarningCategory::All),
            WarningCategory::DeprecatedVersionDowngrade => Some(WarningCategory::Deprecated),
            WarningCategory::DeprecatedDelimiterWillBePaired => Some(WarningCategory::Deprecated),
            WarningCategory::ExperimentalClass => Some(WarningCategory::Experimental),
            WarningCategory::DeprecatedSubsequentUseVersion => Some(WarningCategory::Deprecated),
            WarningCategory::ExperimentalKeywordAll => Some(WarningCategory::Experimental),
            WarningCategory::ExperimentalKeywordAny => Some(WarningCategory::Experimental),
            WarningCategory::ExperimentalEnhancedXx => Some(WarningCategory::Experimental),
            WarningCategory::ExperimentalSignatureNamedParameters => Some(WarningCategory::Experimental),
            WarningCategory::MissingImport => Some(WarningCategory::All),
        }
    }

    /// Whether this category or any ancestor is `ancestor` — the subtree relation pragma expansion walks.
    pub fn within(self, ancestor: WarningCategory) -> bool {
        let mut at = self;
        loop {
            if at == ancestor {
                return true;
            }
            match at.parent() {
                Some(up) => at = up,
                None => return false,
            }
        }
    }

    /// Whether perl enables this category with no pragma in effect (`ckWARN_d` semantics) — the interpreter's initial
    /// bit vector.  Cross-checked against `$warnings::DEFAULT`.
    pub fn default_enabled(self) -> bool {
        matches!(
            self,
            WarningCategory::Deprecated
                | WarningCategory::Glob
                | WarningCategory::Debugging
                | WarningCategory::Inplace
                | WarningCategory::Malloc
                | WarningCategory::DeprecatedUnicodePropertyName
                | WarningCategory::ExperimentalReStrict
                | WarningCategory::ExperimentalRefaliasing
                | WarningCategory::Locale
                | WarningCategory::ExperimentalDeclaredRefs
                | WarningCategory::DeprecatedDotInInc
                | WarningCategory::ExperimentalPrivateUse
                | WarningCategory::ExperimentalUnipropWildcards
                | WarningCategory::ExperimentalVlb
                | WarningCategory::ExperimentalTry
                | WarningCategory::ExperimentalArgsArrayWithSignatures
                | WarningCategory::ExperimentalBuiltin
                | WarningCategory::ExperimentalDefer
                | WarningCategory::ExperimentalExtraPairedDelimiters
                | WarningCategory::DeprecatedVersionDowngrade
                | WarningCategory::DeprecatedDelimiterWillBePaired
                | WarningCategory::ExperimentalClass
                | WarningCategory::DeprecatedSubsequentUseVersion
                | WarningCategory::ExperimentalKeywordAll
                | WarningCategory::ExperimentalKeywordAny
                | WarningCategory::ExperimentalEnhancedXx
                | WarningCategory::ExperimentalSignatureNamedParameters
                | WarningCategory::MissingImport
        )
    }

    /// The category for a discriminant, for reading pragma bit vectors back.
    pub fn from_bit(bit: u8) -> Option<WarningCategory> {
        match bit {
            0 => Some(WarningCategory::All),
            1 => Some(WarningCategory::Closure),
            2 => Some(WarningCategory::Deprecated),
            3 => Some(WarningCategory::Exiting),
            4 => Some(WarningCategory::Glob),
            5 => Some(WarningCategory::Io),
            6 => Some(WarningCategory::Closed),
            7 => Some(WarningCategory::Exec),
            8 => Some(WarningCategory::Layer),
            9 => Some(WarningCategory::Newline),
            10 => Some(WarningCategory::Pipe),
            11 => Some(WarningCategory::Unopened),
            12 => Some(WarningCategory::Misc),
            13 => Some(WarningCategory::Numeric),
            14 => Some(WarningCategory::Once),
            15 => Some(WarningCategory::Overflow),
            16 => Some(WarningCategory::Pack),
            17 => Some(WarningCategory::Portable),
            18 => Some(WarningCategory::Recursion),
            19 => Some(WarningCategory::Redefine),
            20 => Some(WarningCategory::Regexp),
            21 => Some(WarningCategory::Severe),
            22 => Some(WarningCategory::Debugging),
            23 => Some(WarningCategory::Inplace),
            24 => Some(WarningCategory::Internal),
            25 => Some(WarningCategory::Malloc),
            26 => Some(WarningCategory::Signal),
            27 => Some(WarningCategory::Substr),
            28 => Some(WarningCategory::Syntax),
            29 => Some(WarningCategory::Ambiguous),
            30 => Some(WarningCategory::Bareword),
            31 => Some(WarningCategory::Digit),
            32 => Some(WarningCategory::Parenthesis),
            33 => Some(WarningCategory::Precedence),
            34 => Some(WarningCategory::Printf),
            35 => Some(WarningCategory::Prototype),
            36 => Some(WarningCategory::Qw),
            37 => Some(WarningCategory::Reserved),
            38 => Some(WarningCategory::Semicolon),
            39 => Some(WarningCategory::Taint),
            40 => Some(WarningCategory::Threads),
            41 => Some(WarningCategory::Uninitialized),
            42 => Some(WarningCategory::Unpack),
            43 => Some(WarningCategory::Untie),
            44 => Some(WarningCategory::Utf8),
            45 => Some(WarningCategory::Void),
            46 => Some(WarningCategory::Imprecision),
            47 => Some(WarningCategory::Illegalproto),
            48 => Some(WarningCategory::DeprecatedUnicodePropertyName),
            49 => Some(WarningCategory::NonUnicode),
            50 => Some(WarningCategory::Nonchar),
            51 => Some(WarningCategory::Surrogate),
            52 => Some(WarningCategory::Experimental),
            53 => Some(WarningCategory::ExperimentalRegexSets),
            54 => Some(WarningCategory::Syscalls),
            55 => Some(WarningCategory::ExperimentalReStrict),
            56 => Some(WarningCategory::ExperimentalRefaliasing),
            57 => Some(WarningCategory::Locale),
            58 => Some(WarningCategory::Missing),
            59 => Some(WarningCategory::Redundant),
            60 => Some(WarningCategory::ExperimentalDeclaredRefs),
            61 => Some(WarningCategory::DeprecatedDotInInc),
            62 => Some(WarningCategory::Shadow),
            63 => Some(WarningCategory::ExperimentalPrivateUse),
            64 => Some(WarningCategory::ExperimentalUnipropWildcards),
            65 => Some(WarningCategory::ExperimentalVlb),
            66 => Some(WarningCategory::ExperimentalTry),
            67 => Some(WarningCategory::ExperimentalArgsArrayWithSignatures),
            68 => Some(WarningCategory::ExperimentalBuiltin),
            69 => Some(WarningCategory::ExperimentalDefer),
            70 => Some(WarningCategory::ExperimentalExtraPairedDelimiters),
            71 => Some(WarningCategory::Scalar),
            72 => Some(WarningCategory::DeprecatedVersionDowngrade),
            73 => Some(WarningCategory::DeprecatedDelimiterWillBePaired),
            74 => Some(WarningCategory::ExperimentalClass),
            75 => Some(WarningCategory::DeprecatedSubsequentUseVersion),
            76 => Some(WarningCategory::ExperimentalKeywordAll),
            77 => Some(WarningCategory::ExperimentalKeywordAny),
            78 => Some(WarningCategory::ExperimentalEnhancedXx),
            79 => Some(WarningCategory::ExperimentalSignatureNamedParameters),
            80 => Some(WarningCategory::MissingImport),
            _ => None,
        }
    }
}

/// The contract every warning enum speaks (§2.3.4): perl-exact `Display` of the message body, the category the pragma
/// system gates it by, and decomposition for compound events — a compound spans categories, and emission gates each
/// part by its own, so `for_each_part` yields the singleton parts (the default is the value itself).
pub trait PerlWarning: fmt::Display {
    /// The warning category the pragma system gates this warning by.  For a compound event this is the first part's
    /// category; per-part gating goes through [`PerlWarning::for_each_part`].
    fn category(&self) -> WarningCategory;

    /// Visit each atomic warning in emission order: singletons visit themselves, compounds their parts.
    fn for_each_part(&self, f: impl FnMut(&Self))
    where
        Self: Sized,
    {
        let mut f = f;
        f(self);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/warnings_tests.rs"]
mod tests;
