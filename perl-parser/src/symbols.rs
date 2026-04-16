//! Symbol table — tracks packages, subroutines, and imports.
//!
//! Populated as the parser encounters `sub` declarations, `package`
//! statements, and (eventually) `use` imports.  Consulted at call
//! sites to drive prototype-aware argument parsing.
//!
//! ## Organization
//!
//! * [`SymbolTable`] owns the tree of all known packages, keyed by
//!   package name.
//! * [`Namespace`] represents a single package's stash: a map of
//!   subs declared in that package, plus a map of imports that have
//!   been pulled into its local namespace.
//! * [`SubInfo`] is what we know about a single subroutine: its
//!   prototype (if any), attributes, and whether we've seen a body
//!   for it yet.
//!
//! The parser holds a `SymbolTable` and a `current_package: Arc<str>`
//! pointer into it.  Package switches (`package Foo;`, block form
//! `package Foo { ... }`) just update the pointer.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

// ─── Prototype representation ─────────────────────────────────────

/// A parsed Perl subroutine prototype.
///
/// Perl stores prototypes as raw strings like `"$$"`, `"\@;@"`,
/// `"&@"`.  We parse once at declaration time into a structured form
/// for efficient consultation at call sites.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubPrototype {
    /// The raw prototype string as written in the source.  Preserved
    /// for round-tripping and diagnostics.
    pub raw: String,
    /// Number of required slots at the front of `slots`.  The
    /// remaining `slots.len() - required` slots are optional (after
    /// the `;` separator in the raw prototype).
    pub required: usize,
    /// Ordered list of argument slots.
    pub slots: Vec<ProtoSlot>,
}

/// A single argument slot in a prototype.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtoSlot {
    /// `$` — scalar expression.
    Scalar,
    /// `_` — scalar expression, defaults to `$_` if omitted.
    DefaultedScalar,
    /// `&` — block or coderef.
    Block,
    /// `*` — typeglob.
    Glob,
    /// `+` — array or hash (Perl 5.14+).  Not auto-referenced.
    ArrayOrHash,
    /// `\X` — auto-reference the argument, which must be of the
    /// given reference kind.
    AutoRef(RefKind),
    /// `\[...]` — auto-reference, with the argument allowed to be
    /// any one of the listed kinds.
    AutoRefOneOf(Vec<RefKind>),
    /// `@` — absorb all remaining arguments as a list.  Always last.
    SlurpyList,
    /// `%` — absorb all remaining arguments as key-value pairs.
    /// Always last.
    SlurpyHash,
}

/// Kind of reference, for auto-ref prototype slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    /// `$` — scalar
    Scalar,
    /// `@` — array
    Array,
    /// `%` — hash
    Hash,
    /// `&` — subroutine
    Sub,
    /// `*` — typeglob
    Glob,
}

/// Error from parsing a prototype string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrototypeError {
    pub message: String,
    /// Byte offset into the prototype string where the error was
    /// detected.
    pub position: usize,
}

impl SubPrototype {
    /// Parse a prototype string into structured form.
    ///
    /// Accepts the raw characters from between `(...)` in a `sub`
    /// declaration, e.g. `"$$"`, `"\@;@"`, `"&@"`, `"_"`, `"\[$@%]"`.
    ///
    /// Whitespace in prototypes is ignored (Perl allows spacing for
    /// readability).
    pub fn parse(raw: &str) -> Result<Self, PrototypeError> {
        let bytes = raw.as_bytes();
        let mut slots = Vec::new();
        let mut required_complete = false;
        let mut required_count: usize = 0;
        let mut i = 0;

        while i < bytes.len() {
            let c = bytes[i];
            match c {
                b' ' | b'\t' | b'\n' | b'\r' => {
                    // Perl ignores whitespace inside prototypes.
                    i += 1;
                    continue;
                }
                b';' => {
                    // Marker between required and optional.  If we
                    // see this twice, later ones are harmless no-ops
                    // in Perl (only the first `;` matters).
                    if !required_complete {
                        required_count = slots.len();
                        required_complete = true;
                    }
                    i += 1;
                    continue;
                }
                _ => {}
            }

            // Slurpy slots (@, %) must appear last.  Once we've seen
            // one, no further slots are allowed.
            if matches!(slots.last(), Some(ProtoSlot::SlurpyList | ProtoSlot::SlurpyHash)) {
                return Err(PrototypeError { message: "prototype character after slurpy @ or %".to_string(), position: i });
            }

            let slot = match c {
                b'$' => {
                    i += 1;
                    ProtoSlot::Scalar
                }
                b'_' => {
                    i += 1;
                    ProtoSlot::DefaultedScalar
                }
                b'&' => {
                    i += 1;
                    ProtoSlot::Block
                }
                b'*' => {
                    i += 1;
                    ProtoSlot::Glob
                }
                b'+' => {
                    i += 1;
                    ProtoSlot::ArrayOrHash
                }
                b'@' => {
                    i += 1;
                    ProtoSlot::SlurpyList
                }
                b'%' => {
                    i += 1;
                    ProtoSlot::SlurpyHash
                }
                b'\\' => {
                    // Backslash introduces auto-ref.  Next byte is
                    // either a single ref char or a `[...]` group.
                    i += 1;
                    if i >= bytes.len() {
                        return Err(PrototypeError { message: "trailing backslash in prototype".to_string(), position: i - 1 });
                    }
                    match bytes[i] {
                        b'[' => {
                            i += 1;
                            let group_start = i;
                            let mut kinds = Vec::new();
                            while i < bytes.len() && bytes[i] != b']' {
                                let b = bytes[i];
                                if b == b' ' || b == b'\t' {
                                    i += 1;
                                    continue;
                                }
                                let kind = ref_kind_from_byte(b).ok_or_else(|| PrototypeError {
                                    message: format!("unexpected character '{}' in prototype ref group", b as char),
                                    position: i,
                                })?;
                                kinds.push(kind);
                                i += 1;
                            }
                            if i >= bytes.len() {
                                return Err(PrototypeError { message: "unterminated \\[...] in prototype".to_string(), position: group_start - 2 });
                            }
                            i += 1; // consume ]
                            if kinds.is_empty() {
                                return Err(PrototypeError { message: "empty \\[] in prototype".to_string(), position: group_start - 2 });
                            }
                            ProtoSlot::AutoRefOneOf(kinds)
                        }
                        other => {
                            let kind = ref_kind_from_byte(other).ok_or_else(|| PrototypeError {
                                message: format!("unexpected character '{}' after backslash in prototype", other as char),
                                position: i,
                            })?;
                            i += 1;
                            ProtoSlot::AutoRef(kind)
                        }
                    }
                }
                other => {
                    return Err(PrototypeError { message: format!("unexpected character '{}' in prototype", other as char), position: i });
                }
            };
            slots.push(slot);
        }

        if !required_complete {
            required_count = slots.len();
        }

        Ok(SubPrototype { raw: raw.to_string(), required: required_count, slots })
    }
}

fn ref_kind_from_byte(b: u8) -> Option<RefKind> {
    match b {
        b'$' => Some(RefKind::Scalar),
        b'@' => Some(RefKind::Array),
        b'%' => Some(RefKind::Hash),
        b'&' => Some(RefKind::Sub),
        b'*' => Some(RefKind::Glob),
        _ => None,
    }
}

// ─── Sub information ──────────────────────────────────────────────

/// Everything the parser knows about a subroutine.
///
/// `name` is the bare name (no `Foo::` prefix); the containing
/// package is implicit from which `Namespace` holds this `SubInfo`.
#[derive(Clone, Debug)]
pub struct SubInfo {
    pub name: Arc<str>,
    pub prototype: Option<SubPrototype>,
    pub attributes: Vec<String>,
    /// `true` if we've only seen a forward declaration so far
    /// (`sub foo ($$);` without a body).  Cleared when a full
    /// definition is encountered.
    pub forward_declaration: bool,
}

// ─── Import target ────────────────────────────────────────────────

/// The target a local name resolves to in another package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportTarget {
    pub package: Arc<str>,
    pub name: Arc<str>,
}

// ─── Single-package namespace ─────────────────────────────────────

/// One Perl package's stash: its locally-declared subs and its
/// imports.
#[derive(Clone, Debug, Default)]
pub struct Namespace {
    pub name: Arc<str>,
    /// Subs declared in this package, keyed by bare name.
    pub subs: BTreeMap<Arc<str>, SubInfo>,
    /// Imports pulled into this package: local_name → (target_package,
    /// target_name).  Populated by `use` (not yet implemented; API
    /// ready for it).
    pub imports: BTreeMap<Arc<str>, ImportTarget>,
}

impl Namespace {
    pub fn new(name: Arc<str>) -> Self {
        Namespace { name, subs: BTreeMap::new(), imports: BTreeMap::new() }
    }

    /// Look up a sub in this package's local subs only.  Does not
    /// follow imports.  For cross-package resolution with import
    /// chasing, use [`SymbolTable::lookup`].
    pub fn lookup_sub(&self, bare_name: &str) -> Option<&SubInfo> {
        self.subs.get(bare_name)
    }

    /// Declare a sub in this package, replacing any existing entry
    /// of the same name (which handles forward-decl → full-defn).
    pub fn declare_sub(&mut self, bare_name: &str, prototype: Option<SubPrototype>, attributes: Vec<String>, forward_declaration: bool) {
        let name: Arc<str> = Arc::from(bare_name);
        self.subs.insert(name.clone(), SubInfo { name, prototype, attributes, forward_declaration });
    }
}

// ─── Symbol table: tree of all packages ───────────────────────────

#[derive(Clone, Debug, Default)]
pub struct SymbolTable {
    namespaces: HashMap<Arc<str>, Namespace>,
}

impl SymbolTable {
    pub fn new() -> Self {
        SymbolTable { namespaces: HashMap::new() }
    }

    /// Read-only access to a package's namespace, if it exists.
    pub fn get(&self, package: &str) -> Option<&Namespace> {
        self.namespaces.get(package)
    }

    /// Mutable access to a package's namespace, if it exists.
    pub fn get_mut(&mut self, package: &str) -> Option<&mut Namespace> {
        self.namespaces.get_mut(package)
    }

    /// Get a package's namespace, creating it lazily if absent.
    pub fn entry(&mut self, package: &str) -> &mut Namespace {
        let key: Arc<str> = Arc::from(package);
        self.namespaces.entry(key.clone()).or_insert_with(|| Namespace::new(key))
    }

    /// Iterate over all known packages (in unspecified order).
    pub fn packages(&self) -> impl Iterator<Item = &Namespace> {
        self.namespaces.values()
    }

    /// Import a sub from one package into another's local namespace.
    /// After this, looking up `local_name` with `into_package` as
    /// the default package resolves to (`from_package`, `from_name`).
    ///
    /// Creates either namespace lazily if absent.  The target is
    /// stored as a name pair; it does not need to exist at the time
    /// of import (forward references are allowed).
    pub fn import(&mut self, into_package: &str, local_name: &str, from_package: &str, from_name: &str) {
        // Ensure the source namespace exists (even if empty) so that
        // packages() reflects everything we've heard of.
        let _ = self.entry(from_package);

        let ns = self.entry(into_package);
        ns.imports.insert(Arc::from(local_name), ImportTarget { package: Arc::from(from_package), name: Arc::from(from_name) });
    }

    /// Look up a sub by the name as written at a call site.
    ///
    /// * If `name` contains `::`, it's treated as fully-qualified:
    ///   the part before the last `::` is the package, and we look
    ///   only in that package (plus its import chain).
    /// * Otherwise, `name` is bare and we look in `default_package`
    ///   first, then follow that namespace's imports.
    ///
    /// Import chains are followed up to a depth sufficient to detect
    /// cycles; a cycle returns `None`.
    pub fn lookup(&self, name: &str, default_package: &str) -> Option<&SubInfo> {
        let (package, bare) = split_qualified_name(name, default_package);
        self.resolve(package, bare)
    }

    /// Resolve a (package, bare) pair, following imports.
    fn resolve(&self, package: &str, bare: &str) -> Option<&SubInfo> {
        // Use Arc<str> for the iteration variables.  The first
        // iteration allocates from the caller's &str; subsequent
        // iterations clone the Arc<str>s stored on the import
        // target, which is an atomic ref-count bump rather than a
        // byte copy.  This also keeps the types consistent with
        // the storage on `SubInfo` / `ImportTarget` / `Namespace`.
        let mut package: Arc<str> = Arc::from(package);
        let mut bare: Arc<str> = Arc::from(bare);
        let mut visited: HashSet<(Arc<str>, Arc<str>)> = HashSet::new();
        loop {
            if !visited.insert((package.clone(), bare.clone())) {
                return None; // import cycle
            }
            let ns = self.namespaces.get(package.as_ref())?;
            if let Some(info) = ns.subs.get(bare.as_ref()) {
                return Some(info);
            }
            match ns.imports.get(bare.as_ref()) {
                Some(target) => {
                    package = target.package.clone();
                    bare = target.name.clone();
                }
                None => return None,
            }
        }
    }
}

/// Split a name that may be bare or fully-qualified.
///
/// Returns `(package, bare_name)`.  If the name contains `::`, the
/// package is the prefix before the last `::`; otherwise it's
/// `default_package`.
fn split_qualified_name<'a>(name: &'a str, default_package: &'a str) -> (&'a str, &'a str) {
    match name.rfind("::") {
        Some(idx) => (&name[..idx], &name[idx + 2..]),
        None => (default_package, name),
    }
}

// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
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
}
