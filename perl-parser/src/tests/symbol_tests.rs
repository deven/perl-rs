//! Symbol tests.

use super::*;

// ── Prototype parsing ──

fn parse_ok(raw: &str) -> SubPrototype {
    SubPrototype::parse(raw).unwrap()
}

#[test]
fn prototype_empty() {
    let p = parse_ok("");
    assert_eq!(p.slots, vec![]);
    assert_eq!(p.required, 0);
}

#[test]
fn prototype_single_scalar() {
    let p = parse_ok("$");
    assert_eq!(p.slots, vec![ProtoSlot::Scalar]);
    assert_eq!(p.required, 1);
}

#[test]
fn prototype_two_scalars() {
    let p = parse_ok("$$");
    assert_eq!(p.slots, vec![ProtoSlot::Scalar, ProtoSlot::Scalar]);
    assert_eq!(p.required, 2);
}

#[test]
fn prototype_with_optional() {
    let p = parse_ok("$;$");
    assert_eq!(p.slots, vec![ProtoSlot::Scalar, ProtoSlot::Scalar]);
    assert_eq!(p.required, 1);
}

#[test]
fn prototype_slurpy_list() {
    let p = parse_ok("@");
    assert_eq!(p.slots, vec![ProtoSlot::SlurpyList]);
    assert_eq!(p.required, 1);
}

#[test]
fn prototype_slurpy_hash() {
    let p = parse_ok("%");
    assert_eq!(p.slots, vec![ProtoSlot::SlurpyHash]);
}

#[test]
fn prototype_scalar_then_list() {
    let p = parse_ok("$@");
    assert_eq!(p.slots, vec![ProtoSlot::Scalar, ProtoSlot::SlurpyList]);
    assert_eq!(p.required, 2);
}

#[test]
fn prototype_block_and_list() {
    // map/grep style: &@
    let p = parse_ok("&@");
    assert_eq!(p.slots, vec![ProtoSlot::Block, ProtoSlot::SlurpyList]);
}

#[test]
fn prototype_defaulted_scalar() {
    let p = parse_ok("_");
    assert_eq!(p.slots, vec![ProtoSlot::DefaultedScalar]);
}

#[test]
fn prototype_array_or_hash() {
    let p = parse_ok("+");
    assert_eq!(p.slots, vec![ProtoSlot::ArrayOrHash]);
}

#[test]
fn prototype_glob() {
    let p = parse_ok("*");
    assert_eq!(p.slots, vec![ProtoSlot::Glob]);
}

#[test]
fn prototype_auto_ref_array() {
    let p = parse_ok("\\@");
    assert_eq!(p.slots, vec![ProtoSlot::AutoRef(RefKind::Array)]);
}

#[test]
fn prototype_auto_ref_hash() {
    let p = parse_ok("\\%");
    assert_eq!(p.slots, vec![ProtoSlot::AutoRef(RefKind::Hash)]);
}

#[test]
fn prototype_auto_ref_one_of() {
    let p = parse_ok("\\[$@%]");
    assert_eq!(p.slots, vec![ProtoSlot::AutoRefOneOf(vec![RefKind::Scalar, RefKind::Array, RefKind::Hash,])]);
}

#[test]
fn prototype_whitespace_ignored() {
    let p = parse_ok(" $ $ ; $ ");
    assert_eq!(p.slots, vec![ProtoSlot::Scalar; 3]);
    assert_eq!(p.required, 2);
}

#[test]
fn prototype_raw_preserved() {
    let p = parse_ok("\\@;@");
    assert_eq!(p.raw, "\\@;@");
}

#[test]
fn prototype_error_after_slurpy() {
    // Anything after @ or % is a hard error.
    assert!(SubPrototype::parse("@$").is_err());
    assert!(SubPrototype::parse("%@").is_err());
}

#[test]
fn prototype_error_trailing_backslash() {
    assert!(SubPrototype::parse("$\\").is_err());
}

#[test]
fn prototype_error_bad_ref_char() {
    assert!(SubPrototype::parse("\\X").is_err());
}

#[test]
fn prototype_error_unterminated_group() {
    assert!(SubPrototype::parse("\\[$@").is_err());
}

#[test]
fn prototype_error_empty_group() {
    assert!(SubPrototype::parse("\\[]").is_err());
}

#[test]
fn prototype_extra_semicolons_tolerated() {
    // Only the first `;` matters; later ones are no-ops.
    let p = parse_ok("$;;$");
    assert_eq!(p.required, 1);
    assert_eq!(p.slots.len(), 2);
}

// ── Symbol table: declaration and lookup ──

#[test]
fn symtab_declare_and_lookup() {
    let mut st = SymbolTable::new();
    st.entry("main").declare_sub("foo", None, vec![], false);
    let info = st.lookup("foo", "main").expect("should find foo");
    assert_eq!(info.name.as_ref(), "foo");
    assert!(info.prototype.is_none());
}

#[test]
fn symtab_lookup_fully_qualified() {
    let mut st = SymbolTable::new();
    st.entry("Foo").declare_sub("bar", None, vec![], false);
    let info = st.lookup("Foo::bar", "main").expect("should find Foo::bar");
    assert_eq!(info.name.as_ref(), "bar");
}

#[test]
fn symtab_lookup_miss() {
    let st = SymbolTable::new();
    assert!(st.lookup("nothing", "main").is_none());
}

#[test]
fn symtab_lookup_wrong_package() {
    let mut st = SymbolTable::new();
    st.entry("Foo").declare_sub("bar", None, vec![], false);
    // Bare "bar" looked up in "main" must not find Foo::bar.
    assert!(st.lookup("bar", "main").is_none());
}

#[test]
fn symtab_forward_then_full_declaration() {
    let mut st = SymbolTable::new();
    let proto = SubPrototype::parse("$$").unwrap();
    st.entry("main").declare_sub("foo", Some(proto.clone()), vec![], true);
    // Second declaration replaces the forward-decl entry.
    st.entry("main").declare_sub("foo", Some(proto), vec![], false);
    let info = st.lookup("foo", "main").unwrap();
    assert!(!info.forward_declaration);
    assert_eq!(info.prototype.as_ref().unwrap().raw, "$$");
}

#[test]
fn symtab_declare_with_prototype() {
    let mut st = SymbolTable::new();
    let proto = SubPrototype::parse("&@").unwrap();
    st.entry("main").declare_sub("my_map", Some(proto), vec![], false);
    let info = st.lookup("my_map", "main").unwrap();
    let p = info.prototype.as_ref().unwrap();
    assert_eq!(p.slots, vec![ProtoSlot::Block, ProtoSlot::SlurpyList]);
}

// ── Symbol table: imports ──

#[test]
fn symtab_import_basic() {
    let mut st = SymbolTable::new();
    // Source has the real sub.
    st.entry("Source").declare_sub("helper", None, vec![], false);
    // Import it into User.
    st.import("User", "helper", "Source", "helper");
    // Looking up "helper" in User follows the import.
    let info = st.lookup("helper", "User").expect("should resolve via import");
    assert_eq!(info.name.as_ref(), "helper");
}

#[test]
fn symtab_import_with_rename() {
    let mut st = SymbolTable::new();
    st.entry("Source").declare_sub("internal_name", None, vec![], false);
    st.import("User", "public_name", "Source", "internal_name");
    let info = st.lookup("public_name", "User").unwrap();
    assert_eq!(info.name.as_ref(), "internal_name");
}

#[test]
fn symtab_import_chain() {
    let mut st = SymbolTable::new();
    // A → B → C, where C has the real sub.
    st.entry("C").declare_sub("helper", None, vec![], false);
    st.import("B", "helper", "C", "helper");
    st.import("A", "helper", "B", "helper");
    let info = st.lookup("helper", "A").expect("should follow chain");
    assert_eq!(info.name.as_ref(), "helper");
}

#[test]
fn symtab_import_cycle_returns_none() {
    let mut st = SymbolTable::new();
    // A imports from B imports from A — infinite loop without
    // a visited guard.
    st.import("A", "x", "B", "x");
    st.import("B", "x", "A", "x");
    assert!(st.lookup("x", "A").is_none());
}

#[test]
fn symtab_import_to_unknown_target() {
    let mut st = SymbolTable::new();
    // Importing from a package that has no such sub → lookup fails.
    st.import("User", "helper", "NoSuchPackage", "helper");
    assert!(st.lookup("helper", "User").is_none());
}

#[test]
fn symtab_local_overrides_import() {
    let mut st = SymbolTable::new();
    st.entry("Source").declare_sub("helper", None, vec![], false);
    st.import("User", "helper", "Source", "helper");
    // Now User also declares its own helper; local wins.
    let proto = SubPrototype::parse("$").unwrap();
    st.entry("User").declare_sub("helper", Some(proto), vec![], false);
    let info = st.lookup("helper", "User").unwrap();
    // Should find the local one with the prototype, not the import.
    assert!(info.prototype.is_some());
}

#[test]
fn symtab_fqn_does_not_follow_default_imports() {
    let mut st = SymbolTable::new();
    // Default package has an import; explicit FQN to another
    // package should ignore it.
    st.entry("Real").declare_sub("x", None, vec![], false);
    st.import("Shadow", "x", "Real", "x");
    // Looking up "Shadow::x" resolves via Shadow's imports →
    // Real::x.  Looking up "Real::x" finds it directly.
    assert!(st.lookup("Shadow::x", "Other").is_some());
    assert!(st.lookup("Real::x", "Other").is_some());
    // But "x" (bare) in Other finds nothing.
    assert!(st.lookup("x", "Other").is_none());
}

// ── Symbol table: namespace bookkeeping ──

#[test]
fn symtab_entry_creates_namespace() {
    let mut st = SymbolTable::new();
    assert!(st.get("Foo").is_none());
    let _ = st.entry("Foo");
    assert!(st.get("Foo").is_some());
}

#[test]
fn symtab_import_creates_both_namespaces() {
    let mut st = SymbolTable::new();
    st.import("Into", "name", "From", "source_name");
    assert!(st.get("Into").is_some());
    assert!(st.get("From").is_some());
}

#[test]
fn symtab_packages_iterates_all() {
    let mut st = SymbolTable::new();
    st.entry("A").declare_sub("a", None, vec![], false);
    st.entry("B").declare_sub("b", None, vec![], false);
    let names: Vec<_> = st.packages().map(|ns| ns.name.as_ref().to_string()).collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"A".to_string()));
    assert!(names.contains(&"B".to_string()));
}
