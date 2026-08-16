// Perl's warning-category tree (§2.3.4): the vocabulary every warning enum's parts speak, and the index space warning
// pragmas compile into.  Extracted from perl 5.44.0's `regen/warnings.pl` tree joined with `%warnings::Offsets`, with
// the default-enabled set cross-checked against `$warnings::DEFAULT` — the discriminants are perl's own bit numbers,
// so a pragma bit vector indexes by discriminant with no translation.

use std::fmt;

/// One of perl's 81 warning categories.  The discriminant is perl's bit number; the variant is a *stable* leaf-derived
/// identifier — with judgment overrides where mechanical derivation misleads (`RegexStrict` for `re_strict`, the `re`
/// pragma's strict mode; `VariableLengthLookbehind` for `vlb`) — while perl's spelling, parent path included, is data,
/// living only in [`WarningCategory::name`] and [`WarningCategory::from_name`]: perl has both prefixed names when
/// subcategorizing (`deprecated::*`) and unprefixed them when experiments graduate, and spelling drift should move one
/// string table, not every use site.  [`WarningCategory::parent`] encodes the tree (`use warnings 'io'` enables the
/// whole `io` subtree), with `all` at the root.
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
    UnicodePropertyName = 48,

    /// `non_unicode`
    NonUnicode = 49,

    /// `nonchar`
    Nonchar = 50,

    /// `surrogate`
    Surrogate = 51,

    /// `experimental`
    Experimental = 52,

    /// `experimental::regex_sets`
    RegexSets = 53,

    /// `syscalls`
    Syscalls = 54,

    /// `experimental::re_strict` — enabled by default (`ckWARN_d` semantics).
    RegexStrict = 55,

    /// `experimental::refaliasing` — enabled by default (`ckWARN_d` semantics).
    Refaliasing = 56,

    /// `locale` — enabled by default (`ckWARN_d` semantics).
    Locale = 57,

    /// `missing`
    Missing = 58,

    /// `redundant`
    Redundant = 59,

    /// `experimental::declared_refs` — enabled by default (`ckWARN_d` semantics).
    DeclaredRefs = 60,

    /// `deprecated::dot_in_inc` — enabled by default (`ckWARN_d` semantics).
    DotInInc = 61,

    /// `shadow`
    Shadow = 62,

    /// `experimental::private_use` — enabled by default (`ckWARN_d` semantics).
    PrivateUse = 63,

    /// `experimental::uniprop_wildcards` — enabled by default (`ckWARN_d` semantics).
    UnipropWildcards = 64,

    /// `experimental::vlb` — enabled by default (`ckWARN_d` semantics).
    VariableLengthLookbehind = 65,

    /// `experimental::try` — enabled by default (`ckWARN_d` semantics).
    Try = 66,

    /// `experimental::args_array_with_signatures` — enabled by default (`ckWARN_d` semantics).
    ArgsArrayWithSignatures = 67,

    /// `experimental::builtin` — enabled by default (`ckWARN_d` semantics).
    Builtin = 68,

    /// `experimental::defer` — enabled by default (`ckWARN_d` semantics).
    Defer = 69,

    /// `experimental::extra_paired_delimiters` — enabled by default (`ckWARN_d` semantics).
    ExtraPairedDelimiters = 70,

    /// `scalar`
    Scalar = 71,

    /// `deprecated::version_downgrade` — enabled by default (`ckWARN_d` semantics).
    VersionDowngrade = 72,

    /// `deprecated::delimiter_will_be_paired` — enabled by default (`ckWARN_d` semantics).
    DelimiterWillBePaired = 73,

    /// `experimental::class` — enabled by default (`ckWARN_d` semantics).
    Class = 74,

    /// `deprecated::subsequent_use_version` — enabled by default (`ckWARN_d` semantics).
    SubsequentUseVersion = 75,

    /// `experimental::keyword_all` — enabled by default (`ckWARN_d` semantics).
    KeywordAll = 76,

    /// `experimental::keyword_any` — enabled by default (`ckWARN_d` semantics).
    KeywordAny = 77,

    /// `experimental::enhanced_xx` — enabled by default (`ckWARN_d` semantics).
    EnhancedXx = 78,

    /// `experimental::signature_named_parameters` — enabled by default (`ckWARN_d` semantics).
    SignatureNamedParameters = 79,

    /// `missing_import` — enabled by default (`ckWARN_d` semantics).
    MissingImport = 80,
}

/// The number of categories: bit vectors sized to this cover every discriminant.
pub const WARNING_CATEGORY_COUNT: usize = 81;

impl WarningCategory {
    /// Perl's spelling, as `use warnings '...'` writes it — parent path included for the subcategories.
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
            WarningCategory::UnicodePropertyName => "deprecated::unicode_property_name",
            WarningCategory::NonUnicode => "non_unicode",
            WarningCategory::Nonchar => "nonchar",
            WarningCategory::Surrogate => "surrogate",
            WarningCategory::Experimental => "experimental",
            WarningCategory::RegexSets => "experimental::regex_sets",
            WarningCategory::Syscalls => "syscalls",
            WarningCategory::RegexStrict => "experimental::re_strict",
            WarningCategory::Refaliasing => "experimental::refaliasing",
            WarningCategory::Locale => "locale",
            WarningCategory::Missing => "missing",
            WarningCategory::Redundant => "redundant",
            WarningCategory::DeclaredRefs => "experimental::declared_refs",
            WarningCategory::DotInInc => "deprecated::dot_in_inc",
            WarningCategory::Shadow => "shadow",
            WarningCategory::PrivateUse => "experimental::private_use",
            WarningCategory::UnipropWildcards => "experimental::uniprop_wildcards",
            WarningCategory::VariableLengthLookbehind => "experimental::vlb",
            WarningCategory::Try => "experimental::try",
            WarningCategory::ArgsArrayWithSignatures => "experimental::args_array_with_signatures",
            WarningCategory::Builtin => "experimental::builtin",
            WarningCategory::Defer => "experimental::defer",
            WarningCategory::ExtraPairedDelimiters => "experimental::extra_paired_delimiters",
            WarningCategory::Scalar => "scalar",
            WarningCategory::VersionDowngrade => "deprecated::version_downgrade",
            WarningCategory::DelimiterWillBePaired => "deprecated::delimiter_will_be_paired",
            WarningCategory::Class => "experimental::class",
            WarningCategory::SubsequentUseVersion => "deprecated::subsequent_use_version",
            WarningCategory::KeywordAll => "experimental::keyword_all",
            WarningCategory::KeywordAny => "experimental::keyword_any",
            WarningCategory::EnhancedXx => "experimental::enhanced_xx",
            WarningCategory::SignatureNamedParameters => "experimental::signature_named_parameters",
            WarningCategory::MissingImport => "missing_import",
        }
    }

    /// The category perl's spelling names — the parser's direction, `use warnings 'numeric'` to the bit.
    pub fn from_name(name: &str) -> Option<WarningCategory> {
        match name {
            "all" => Some(WarningCategory::All),
            "closure" => Some(WarningCategory::Closure),
            "deprecated" => Some(WarningCategory::Deprecated),
            "exiting" => Some(WarningCategory::Exiting),
            "glob" => Some(WarningCategory::Glob),
            "io" => Some(WarningCategory::Io),
            "closed" => Some(WarningCategory::Closed),
            "exec" => Some(WarningCategory::Exec),
            "layer" => Some(WarningCategory::Layer),
            "newline" => Some(WarningCategory::Newline),
            "pipe" => Some(WarningCategory::Pipe),
            "unopened" => Some(WarningCategory::Unopened),
            "misc" => Some(WarningCategory::Misc),
            "numeric" => Some(WarningCategory::Numeric),
            "once" => Some(WarningCategory::Once),
            "overflow" => Some(WarningCategory::Overflow),
            "pack" => Some(WarningCategory::Pack),
            "portable" => Some(WarningCategory::Portable),
            "recursion" => Some(WarningCategory::Recursion),
            "redefine" => Some(WarningCategory::Redefine),
            "regexp" => Some(WarningCategory::Regexp),
            "severe" => Some(WarningCategory::Severe),
            "debugging" => Some(WarningCategory::Debugging),
            "inplace" => Some(WarningCategory::Inplace),
            "internal" => Some(WarningCategory::Internal),
            "malloc" => Some(WarningCategory::Malloc),
            "signal" => Some(WarningCategory::Signal),
            "substr" => Some(WarningCategory::Substr),
            "syntax" => Some(WarningCategory::Syntax),
            "ambiguous" => Some(WarningCategory::Ambiguous),
            "bareword" => Some(WarningCategory::Bareword),
            "digit" => Some(WarningCategory::Digit),
            "parenthesis" => Some(WarningCategory::Parenthesis),
            "precedence" => Some(WarningCategory::Precedence),
            "printf" => Some(WarningCategory::Printf),
            "prototype" => Some(WarningCategory::Prototype),
            "qw" => Some(WarningCategory::Qw),
            "reserved" => Some(WarningCategory::Reserved),
            "semicolon" => Some(WarningCategory::Semicolon),
            "taint" => Some(WarningCategory::Taint),
            "threads" => Some(WarningCategory::Threads),
            "uninitialized" => Some(WarningCategory::Uninitialized),
            "unpack" => Some(WarningCategory::Unpack),
            "untie" => Some(WarningCategory::Untie),
            "utf8" => Some(WarningCategory::Utf8),
            "void" => Some(WarningCategory::Void),
            "imprecision" => Some(WarningCategory::Imprecision),
            "illegalproto" => Some(WarningCategory::Illegalproto),
            "deprecated::unicode_property_name" => Some(WarningCategory::UnicodePropertyName),
            "non_unicode" => Some(WarningCategory::NonUnicode),
            "nonchar" => Some(WarningCategory::Nonchar),
            "surrogate" => Some(WarningCategory::Surrogate),
            "experimental" => Some(WarningCategory::Experimental),
            "experimental::regex_sets" => Some(WarningCategory::RegexSets),
            "syscalls" => Some(WarningCategory::Syscalls),
            "experimental::re_strict" => Some(WarningCategory::RegexStrict),
            "experimental::refaliasing" => Some(WarningCategory::Refaliasing),
            "locale" => Some(WarningCategory::Locale),
            "missing" => Some(WarningCategory::Missing),
            "redundant" => Some(WarningCategory::Redundant),
            "experimental::declared_refs" => Some(WarningCategory::DeclaredRefs),
            "deprecated::dot_in_inc" => Some(WarningCategory::DotInInc),
            "shadow" => Some(WarningCategory::Shadow),
            "experimental::private_use" => Some(WarningCategory::PrivateUse),
            "experimental::uniprop_wildcards" => Some(WarningCategory::UnipropWildcards),
            "experimental::vlb" => Some(WarningCategory::VariableLengthLookbehind),
            "experimental::try" => Some(WarningCategory::Try),
            "experimental::args_array_with_signatures" => Some(WarningCategory::ArgsArrayWithSignatures),
            "experimental::builtin" => Some(WarningCategory::Builtin),
            "experimental::defer" => Some(WarningCategory::Defer),
            "experimental::extra_paired_delimiters" => Some(WarningCategory::ExtraPairedDelimiters),
            "scalar" => Some(WarningCategory::Scalar),
            "deprecated::version_downgrade" => Some(WarningCategory::VersionDowngrade),
            "deprecated::delimiter_will_be_paired" => Some(WarningCategory::DelimiterWillBePaired),
            "experimental::class" => Some(WarningCategory::Class),
            "deprecated::subsequent_use_version" => Some(WarningCategory::SubsequentUseVersion),
            "experimental::keyword_all" => Some(WarningCategory::KeywordAll),
            "experimental::keyword_any" => Some(WarningCategory::KeywordAny),
            "experimental::enhanced_xx" => Some(WarningCategory::EnhancedXx),
            "experimental::signature_named_parameters" => Some(WarningCategory::SignatureNamedParameters),
            "missing_import" => Some(WarningCategory::MissingImport),
            _ => None,
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
            WarningCategory::UnicodePropertyName => Some(WarningCategory::Deprecated),
            WarningCategory::NonUnicode => Some(WarningCategory::Utf8),
            WarningCategory::Nonchar => Some(WarningCategory::Utf8),
            WarningCategory::Surrogate => Some(WarningCategory::Utf8),
            WarningCategory::Experimental => Some(WarningCategory::All),
            WarningCategory::RegexSets => Some(WarningCategory::Experimental),
            WarningCategory::Syscalls => Some(WarningCategory::Io),
            WarningCategory::RegexStrict => Some(WarningCategory::Experimental),
            WarningCategory::Refaliasing => Some(WarningCategory::Experimental),
            WarningCategory::Locale => Some(WarningCategory::All),
            WarningCategory::Missing => Some(WarningCategory::All),
            WarningCategory::Redundant => Some(WarningCategory::All),
            WarningCategory::DeclaredRefs => Some(WarningCategory::Experimental),
            WarningCategory::DotInInc => Some(WarningCategory::Deprecated),
            WarningCategory::Shadow => Some(WarningCategory::All),
            WarningCategory::PrivateUse => Some(WarningCategory::Experimental),
            WarningCategory::UnipropWildcards => Some(WarningCategory::Experimental),
            WarningCategory::VariableLengthLookbehind => Some(WarningCategory::Experimental),
            WarningCategory::Try => Some(WarningCategory::Experimental),
            WarningCategory::ArgsArrayWithSignatures => Some(WarningCategory::Experimental),
            WarningCategory::Builtin => Some(WarningCategory::Experimental),
            WarningCategory::Defer => Some(WarningCategory::Experimental),
            WarningCategory::ExtraPairedDelimiters => Some(WarningCategory::Experimental),
            WarningCategory::Scalar => Some(WarningCategory::All),
            WarningCategory::VersionDowngrade => Some(WarningCategory::Deprecated),
            WarningCategory::DelimiterWillBePaired => Some(WarningCategory::Deprecated),
            WarningCategory::Class => Some(WarningCategory::Experimental),
            WarningCategory::SubsequentUseVersion => Some(WarningCategory::Deprecated),
            WarningCategory::KeywordAll => Some(WarningCategory::Experimental),
            WarningCategory::KeywordAny => Some(WarningCategory::Experimental),
            WarningCategory::EnhancedXx => Some(WarningCategory::Experimental),
            WarningCategory::SignatureNamedParameters => Some(WarningCategory::Experimental),
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
                | WarningCategory::UnicodePropertyName
                | WarningCategory::RegexStrict
                | WarningCategory::Refaliasing
                | WarningCategory::Locale
                | WarningCategory::DeclaredRefs
                | WarningCategory::DotInInc
                | WarningCategory::PrivateUse
                | WarningCategory::UnipropWildcards
                | WarningCategory::VariableLengthLookbehind
                | WarningCategory::Try
                | WarningCategory::ArgsArrayWithSignatures
                | WarningCategory::Builtin
                | WarningCategory::Defer
                | WarningCategory::ExtraPairedDelimiters
                | WarningCategory::VersionDowngrade
                | WarningCategory::DelimiterWillBePaired
                | WarningCategory::Class
                | WarningCategory::SubsequentUseVersion
                | WarningCategory::KeywordAll
                | WarningCategory::KeywordAny
                | WarningCategory::EnhancedXx
                | WarningCategory::SignatureNamedParameters
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
            48 => Some(WarningCategory::UnicodePropertyName),
            49 => Some(WarningCategory::NonUnicode),
            50 => Some(WarningCategory::Nonchar),
            51 => Some(WarningCategory::Surrogate),
            52 => Some(WarningCategory::Experimental),
            53 => Some(WarningCategory::RegexSets),
            54 => Some(WarningCategory::Syscalls),
            55 => Some(WarningCategory::RegexStrict),
            56 => Some(WarningCategory::Refaliasing),
            57 => Some(WarningCategory::Locale),
            58 => Some(WarningCategory::Missing),
            59 => Some(WarningCategory::Redundant),
            60 => Some(WarningCategory::DeclaredRefs),
            61 => Some(WarningCategory::DotInInc),
            62 => Some(WarningCategory::Shadow),
            63 => Some(WarningCategory::PrivateUse),
            64 => Some(WarningCategory::UnipropWildcards),
            65 => Some(WarningCategory::VariableLengthLookbehind),
            66 => Some(WarningCategory::Try),
            67 => Some(WarningCategory::ArgsArrayWithSignatures),
            68 => Some(WarningCategory::Builtin),
            69 => Some(WarningCategory::Defer),
            70 => Some(WarningCategory::ExtraPairedDelimiters),
            71 => Some(WarningCategory::Scalar),
            72 => Some(WarningCategory::VersionDowngrade),
            73 => Some(WarningCategory::DelimiterWillBePaired),
            74 => Some(WarningCategory::Class),
            75 => Some(WarningCategory::SubsequentUseVersion),
            76 => Some(WarningCategory::KeywordAll),
            77 => Some(WarningCategory::KeywordAny),
            78 => Some(WarningCategory::EnhancedXx),
            79 => Some(WarningCategory::SignatureNamedParameters),
            80 => Some(WarningCategory::MissingImport),
            _ => None,
        }
    }
}

/// The contract every warning enum speaks (§2.3.4): perl-exact `Display` of the message body, and [`parts`] as the
/// whole gating truth — atomic warnings in emission order, each paired with the category that gates it.  There is
/// deliberately no `category` method: a compound spans categories, so any single answer would be arbitrary, and
/// delivering each category beside its part makes the question unaskable instead of awkwardly answered.  An atomic
/// warning yields one pair (itself); a compound yields its parts, whose own `Display`s are the per-line bodies the
/// whole's `Display` joins.
///
/// [`parts`]: PerlWarning::parts
pub trait PerlWarning: fmt::Display + Clone + Sized {
    /// The atomic warnings in emission order, each with its gating category.  Emission gates each pair by its category
    /// and renders the part; a whole-event pre-check is `parts().any(|(c, _)| enabled(c))`.
    fn parts(&self) -> impl Iterator<Item = (WarningCategory, Self)>;

    /// Whether this warning belongs to perl's deprecation family.  Defaults to `false` — the overwhelmingly common
    /// answer — and enums that own deprecation warnings override it.
    fn deprecated(&self) -> bool {
        false
    }

    /// Whether this warning belongs to perl's experimental family.  Defaults to `false`, overridden by enums that own
    /// experimental-feature warnings.
    fn experimental(&self) -> bool {
        false
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/warnings_tests.rs"]
mod tests;
