// Warning-category tests (§2.3.4): the tree, bits, and defaults are perl 5.44.0's own, extracted from regen/warnings.pl
// and cross-checked against %warnings::Offsets and $warnings::DEFAULT.

use crate::scalar::{NumifyWarning, RadixBase};
use crate::warnings::{PerlWarning, WARNING_CATEGORY_COUNT, WarningCategory};

#[test]
fn the_tree_matches_perl_bit_for_bit() {
    // Spot checks across the range, each (category, bit, parent) read from the container's perl.
    assert_eq!(WarningCategory::All as u8, 0);
    assert_eq!(WarningCategory::All.parent(), None);
    assert_eq!(WarningCategory::Io as u8, 5);
    assert_eq!(WarningCategory::Pipe as u8, 10);
    assert_eq!(WarningCategory::Pipe.parent(), Some(WarningCategory::Io));
    assert_eq!(WarningCategory::Numeric as u8, 13);
    assert_eq!(WarningCategory::Numeric.parent(), Some(WarningCategory::All));
    assert_eq!(WarningCategory::Overflow as u8, 15);
    assert_eq!(WarningCategory::Portable as u8, 17);
    assert_eq!(WarningCategory::Digit as u8, 31);
    assert_eq!(WarningCategory::Digit.parent(), Some(WarningCategory::Syntax));
    assert_eq!(WarningCategory::Uninitialized as u8, 41);
    assert_eq!(WarningCategory::Imprecision as u8, 46);
    assert_eq!(WarningCategory::MissingImport as u8, 80);
    assert_eq!(WARNING_CATEGORY_COUNT, 81);

    // The subtree relation pragma expansion walks: digit is within syntax and all, not io; all contains all.
    assert!(WarningCategory::Digit.within(WarningCategory::Syntax));
    assert!(WarningCategory::Digit.within(WarningCategory::All));
    assert!(!WarningCategory::Digit.within(WarningCategory::Io));
    assert!(WarningCategory::All.within(WarningCategory::All));

    // Names are perl's spellings, subcategories included.
    assert_eq!(WarningCategory::Numeric.name(), "numeric");
    assert_eq!(WarningCategory::DotInInc.name(), "deprecated::dot_in_inc");
    assert_eq!(WarningCategory::ReStrict.name(), "experimental::re_strict");

    // The spelling map runs both directions: the parser's from_name inverts name over the whole vocabulary, and
    // variants stay stable leaf identifiers while the spelling — prefix included — is data.
    for bit in 0..WARNING_CATEGORY_COUNT as u8 {
        let cat = WarningCategory::from_bit(bit).unwrap();
        assert_eq!(WarningCategory::from_name(cat.name()), Some(cat));
    }
    assert_eq!(WarningCategory::from_name("deprecated::dot_in_inc"), Some(WarningCategory::DotInInc));
    assert_eq!(WarningCategory::from_name("dot_in_inc"), None, "perl accepts only its exact spelling");
    assert_eq!(WarningCategory::from_name("no_such_category"), None);

    // Round trip through the bit space, dense and total.
    for bit in 0..WARNING_CATEGORY_COUNT as u8 {
        let cat = WarningCategory::from_bit(bit).unwrap();
        assert_eq!(cat as u8, bit);
    }

    assert!(WarningCategory::from_bit(WARNING_CATEGORY_COUNT as u8).is_none());

    // The default-enabled set is $warnings::DEFAULT's 28, spot-checked at its edges: deprecated and its children on,
    // numeric and overflow off (overflow's default-on-ness in C is ckWARN_d at the call site, not the bit).
    assert!(WarningCategory::Deprecated.default_enabled());
    assert!(WarningCategory::DotInInc.default_enabled());
    assert!(WarningCategory::Glob.default_enabled());
    assert!(WarningCategory::MissingImport.default_enabled());
    assert!(!WarningCategory::Numeric.default_enabled());
    assert!(!WarningCategory::Overflow.default_enabled());

    let on = (0..WARNING_CATEGORY_COUNT as u8).filter(|&b| WarningCategory::from_bit(b).is_some_and(WarningCategory::default_enabled)).count();
    assert_eq!(on, 28, "the default-enabled census matches $warnings::DEFAULT");
}

#[test]
fn warnings_yield_their_parts_with_gating_categories() {
    // An atomic warning is one pair: itself, beside the category that gates it.
    let warn = NumifyWarning::Overflow { base: RadixBase::Hexadecimal };
    let atoms: Vec<_> = warn.parts().map(|(cat, part)| (cat, format!("{part}"))).collect();
    assert_eq!(atoms, vec![(WarningCategory::Overflow, "Integer overflow in hexadecimal number".to_string())]);

    // A compound spans categories, so there is no category of the whole — the pairs are the truth, in emission order,
    // each part's Display one perl body.
    let compound = NumifyWarning::OverflowThenIllegalDigit { base: RadixBase::Hexadecimal, digit: b'G' };
    let parts: Vec<_> = compound.parts().map(|(cat, part)| (cat, format!("{part}"))).collect();
    assert_eq!(
        parts,
        vec![
            (WarningCategory::Overflow, "Integer overflow in hexadecimal number".to_string()),
            (WarningCategory::Digit, "Illegal hexadecimal digit 'G' ignored".to_string()),
        ]
    );

    // The parts joined are the whole's Display: the convenience form and the gated form tell one story.
    let joined = parts.into_iter().map(|(_, body)| body).collect::<Vec<_>>().join("\n");
    assert_eq!(joined, format!("{compound}"));

    // Per-part gating in the shape emission uses: with digit disabled, only the overflow line survives.
    let enabled = |cat: WarningCategory| cat != WarningCategory::Digit;
    let emitted: Vec<_> = compound.parts().filter(|(cat, _)| enabled(*cat)).map(|(_, p)| format!("{p}")).collect();
    assert_eq!(emitted, vec!["Integer overflow in hexadecimal number".to_string()]);

    // The family predicates default false: no numify warning is deprecation- or experiment-class.
    assert!(!warn.deprecated() && !warn.experimental());
    assert!(!compound.deprecated() && !compound.experimental());
}
