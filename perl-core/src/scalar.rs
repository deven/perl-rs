//! The promoted-scalar layer (§2.3.1–§2.3.4): `ScalarRef` shared identity over the Mut/Const split, `ScalarCell` with
//! in-place `Plain`→`Full` upgrade, `ConstScalar` with coercions materialized at birth, the boolean immortal
//! singletons, the structural readonly error path, and numification-warning state.
//!
//! The module name is temporary in the same sense as `string.rs` and `payload.rs`: final names arrive when the
//! superseded flag-matrix modules are deleted.  `MagicChain` and `Stash` are carried over at their current stub
//! fidelity; their real shapes are later design sections.

use crate::cow_buffer::AllocError;
use crate::string::PerlString;
use crate::value::{Numeric, ScalarPayload, Tainted, string_would_warn};
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{LazyLock, OnceLock};

use crate::heap::HeapArc;

// ── Carried-over stubs (§2.3.7: "carried over") ───────────────────
/// A chain of magic (tie, overload, ...) attached to a scalar.  Shape is a later design section.
pub struct MagicChain {
    _private: (),
}

/// A package stash — the symbol table for a package.  Shape is a later design section.
pub struct Stash {
    _private: (),
}

// ── The fallible-operation error (§2.3.7 roster) ──────────────────
/// Errors from fallible scalar operations.  `ReadOnly` is the structural mutation failure the runtime maps to perl's
/// message; allocation failures thread through from the string layer.
#[derive(Debug, PartialEq, Eq)]
pub enum ScalarError {
    /// Modification of a read-only value attempted (§2.3.1): structural for `Const` cells, the dynamic readonly flag
    /// for `Mut` cells.
    ReadOnly,
    Alloc(AllocError),
}

impl From<AllocError> for ScalarError {
    fn from(e: AllocError) -> ScalarError {
        ScalarError::Alloc(e)
    }
}

impl std::fmt::Display for ScalarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScalarError::ReadOnly => f.write_str("Modification of a read-only value attempted"),
            ScalarError::Alloc(_) => f.write_str("Out of memory!"),
        }
    }
}

// ── FullScalar — boxed rare state (§2.3.2) ────────────────────────
/// The rare-state extension: payload plus lazy caches plus identity state, colocated in one box.
///
/// **Cache mechanism (ruled §2.3.2):** the numeric slots are plain atomics — while any reader holds the read lock the
/// payload is frozen (writes require the write lock and clear the caches under it), so racing fillers compute the
/// identical value and the race is benign; value stores are `Relaxed` paired with a `Release` validity store and
/// `Acquire` validity load.  The string slot is `OnceLock<PerlString>` (a `PerlString` cannot be an atomic): the value
/// sits inline in the slot, and invalidation is `take()` through the write guard's `&mut`.
pub struct FullScalar {
    payload: ScalarPayload,

    // Derived caches (lazy; §2.2.2: derived state, never consulted for anything the payload answers).
    cached_int: AtomicI64,
    cached_int_valid: AtomicBool,
    cached_float_bits: AtomicU64,
    cached_float_valid: AtomicBool,
    cached_string: OnceLock<PerlString>,

    // Rare identity state.
    magic: Option<Box<MagicChain>>,
    stash: Option<HeapArc<Stash>>,

    /// The dynamic readonly flag (`Internals::SvREADONLY`, toggleable) — `Mut`-cell readonly, distinct from the
    /// structural `Const` kind (§2.3.1).  Mutated under the write lock only.
    readonly: bool,
}

impl FullScalar {
    fn new(payload: ScalarPayload) -> Box<FullScalar> {
        Box::new(FullScalar {
            payload,
            cached_int: AtomicI64::new(0),
            cached_int_valid: AtomicBool::new(false),
            cached_float_bits: AtomicU64::new(0),
            cached_float_valid: AtomicBool::new(false),
            cached_string: OnceLock::new(),
            magic: None,
            stash: None,
            readonly: false,
        })
    }

    fn invalidate_caches(&mut self) {
        *self.cached_int_valid.get_mut() = false;
        *self.cached_float_valid.get_mut() = false;
        let _ = self.cached_string.take();
    }
}

// ── ScalarCell — the mutable interior (§2.3.2) ────────────────────
/// `Plain` is the common promoted case; `Full` is a single pointer threading the payload's spare niche encodings,
/// keeping the cell at 24 bytes (§2.3.6).  Upgrade happens in place under the write lock: the `Arc` address never
/// changes, preserving every outstanding reference — perl's `sv_upgrade` identity guarantee with a different mechanism.
pub enum ScalarCell {
    Plain(ScalarPayload),
    Full(Box<FullScalar>),
}

impl Drop for ScalarCell {
    /// Iterative teardown (§2.4.9): a dying cell hands its payload to the release worklist instead of letting drop glue
    /// recurse through a chain of referents.
    fn drop(&mut self) {
        let payload = match self {
            ScalarCell::Plain(p) => std::mem::replace(p, ScalarPayload::Undef(Tainted::CLEAN)),
            ScalarCell::Full(f) => std::mem::replace(&mut f.payload, ScalarPayload::Undef(Tainted::CLEAN)),
        };
        if matches!(payload, ScalarPayload::ScalarRefMut(..) | ScalarPayload::ScalarRefConst(..) | ScalarPayload::ArrayRef(..) | ScalarPayload::HashRef(..)) {
            crate::heap::release_payload(payload);
        }
    }
}

impl std::fmt::Debug for ScalarCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScalarCell::Plain(p) => f.debug_tuple("Plain").field(p).finish(),
            ScalarCell::Full(full) => f.debug_struct("Full").field("payload", &full.payload).finish_non_exhaustive(),
        }
    }
}

impl ScalarCell {
    /// The authoritative payload (§2.2.2).
    pub fn payload(&self) -> &ScalarPayload {
        match self {
            ScalarCell::Plain(p) => p,
            ScalarCell::Full(f) => &f.payload,
        }
    }

    pub fn to_bool(&self) -> bool {
        self.payload().to_bool()
    }

    /// The integer coercion; `Full` cells memoize through the atomic pair (mechanism in [`FullScalar`]).
    pub fn to_int(&self) -> i64 {
        match self {
            ScalarCell::Plain(p) => p.to_int(),
            ScalarCell::Full(f) => {
                if f.cached_int_valid.load(Ordering::Acquire) {
                    return f.cached_int.load(Ordering::Relaxed);
                }

                let v = f.payload.to_int();
                f.cached_int.store(v, Ordering::Relaxed);
                f.cached_int_valid.store(true, Ordering::Release);

                v
            }
        }
    }

    /// The float coercion; `Full` cells memoize as bits through the atomic pair.
    pub fn to_float(&self) -> f64 {
        match self {
            ScalarCell::Plain(p) => p.to_float(),
            ScalarCell::Full(f) => {
                if f.cached_float_valid.load(Ordering::Acquire) {
                    return f64::from_bits(f.cached_float_bits.load(Ordering::Relaxed));
                }

                let v = f.payload.to_float();
                f.cached_float_bits.store(v.to_bits(), Ordering::Relaxed);
                f.cached_float_valid.store(true, Ordering::Release);

                v
            }
        }
    }

    pub fn numify(&self) -> Numeric {
        self.payload().numify()
    }

    /// Stringification; `Full` cells memoize in the `OnceLock` slot.  The set-then-get shape (rather than
    /// `get_or_init`) threads the allocation `Result` out; a racing loser's identical value is dropped.
    pub fn to_string_repr(&self) -> Result<PerlString, AllocError> {
        match self {
            ScalarCell::Plain(p) => p.to_string_repr(),
            ScalarCell::Full(f) => {
                if let Some(s) = f.cached_string.get() {
                    return Ok(s.clone());
                }

                let v = f.payload.to_string_repr()?;
                let _ = f.cached_string.set(v.clone());

                Ok(v)
            }
        }
    }

    pub fn is_tainted(&self) -> bool {
        self.payload().is_tainted()
    }

    /// Whether the dynamic readonly flag is set (`Plain` cells never carry it).
    pub fn is_readonly(&self) -> bool {
        matches!(self, ScalarCell::Full(f) if f.readonly)
    }

    /// Replace the payload — the single choke point (§2.2.2): derived state drops here.  Fails structurally on the
    /// dynamic readonly flag.
    pub fn assign(&mut self, payload: ScalarPayload) -> Result<(), ScalarError> {
        match self {
            ScalarCell::Plain(p) => {
                *p = payload;
                Ok(())
            }
            ScalarCell::Full(f) => {
                if f.readonly {
                    return Err(ScalarError::ReadOnly);
                }

                f.payload = payload;
                f.invalidate_caches();

                Ok(())
            }
        }
    }

    /// In-place `Plain`→`Full` upgrade (§2.3.2); idempotent.  Callers hold the write lock, so the `Arc` address — the
    /// identity — never changes.
    pub fn upgrade_to_full(&mut self) -> &mut FullScalar {
        if let ScalarCell::Plain(p) = self {
            let payload = std::mem::replace(p, ScalarPayload::Undef(Tainted::CLEAN));
            *self = ScalarCell::Full(FullScalar::new(payload));
        }

        match self {
            ScalarCell::Full(f) => f,
            ScalarCell::Plain(_) => unreachable!("upgraded above"),
        }
    }

    /// Set or clear the dynamic readonly flag (`Internals::SvREADONLY` semantics: toggleable).  Setting upgrades to
    /// `Full`; clearing on a `Plain` cell is a no-op.
    pub fn set_readonly(&mut self, readonly: bool) {
        match self {
            ScalarCell::Plain(_) if !readonly => {}
            _ => self.upgrade_to_full().readonly = readonly,
        }
    }

    /// Attach magic (upgrades to `Full`).  Magic *dispatch* is a later design section; step 4 pins only that attachment
    /// preserves identity and payload.
    pub fn set_magic(&mut self, magic: MagicChain) {
        self.upgrade_to_full().magic = Some(Box::new(magic));
    }

    pub fn has_magic(&self) -> bool {
        matches!(self, ScalarCell::Full(f) if f.magic.is_some())
    }

    /// Bless into a stash (upgrades to `Full`).
    pub fn bless(&mut self, stash: HeapArc<Stash>) {
        self.upgrade_to_full().stash = Some(stash);
    }

    /// Numify, noting the once-only warning state (§2.3.4).  Returns the numeric result and whether the ops layer
    /// should emit the warning *now*: true exactly when the payload would warn and this is the first such numification.
    /// The once-bit rides the `PerlString` tag, so slot-to-slot copies carry it (the verified copy semantics); requires
    /// the write lock because first-warn is a tag transition.
    pub fn numify_noting_warning(&mut self) -> (Numeric, bool) {
        let n = self.numify();

        let payload = match self {
            ScalarCell::Plain(p) => p,
            ScalarCell::Full(f) => &mut f.payload,
        };

        let emit = match payload {
            ScalarPayload::String(s) if string_would_warn(s.as_bytes()) && !s.is_warned() => {
                s.mark_warned();
                true
            }
            _ => false,
        };

        (n, emit)
    }
}

const _: () = assert!(size_of::<ScalarCell>() == 24);

// ── ConstScalar — frozen at birth (§2.3.3) ────────────────────────
/// The lockless immutable cell: every coercion materialized at construction, reads are plain field access, trivially
/// `Sync`.  The single mutable exception is the numification-warning once-bit, present only when the payload can warn
/// (`None` makes "cannot warn" structural — eager knowledge, lazy surfacing, §2.3.4).
pub struct ConstScalar {
    payload: ScalarPayload,
    int: i64,
    float: f64,
    string: PerlString,
    numify_warned: Option<AtomicBool>,
}

impl Drop for ConstScalar {
    /// Iterative teardown (§2.4.9): frozen payloads can carry graph edges too (§2.4.10).
    fn drop(&mut self) {
        let payload = std::mem::replace(&mut self.payload, ScalarPayload::Undef(Tainted::CLEAN));
        if matches!(payload, ScalarPayload::ScalarRefMut(..) | ScalarPayload::ScalarRefConst(..) | ScalarPayload::ArrayRef(..) | ScalarPayload::HashRef(..)) {
            crate::heap::release_payload(payload);
        }
    }
}

impl std::fmt::Debug for ConstScalar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConstScalar").field("payload", &self.payload).finish_non_exhaustive()
    }
}

impl ConstScalar {
    /// Materialize a payload into a frozen cell (at most two short strings and two numbers, §2.3.3).
    pub fn materialize(payload: ScalarPayload) -> Result<ConstScalar, AllocError> {
        let int = payload.to_int();
        let float = payload.to_float();
        let string = payload.to_string_repr()?;

        let can_warn = matches!(&payload, ScalarPayload::String(s) if string_would_warn(s.as_bytes()));
        let numify_warned = can_warn.then(|| AtomicBool::new(false));

        Ok(ConstScalar { payload, int, float, string, numify_warned })
    }

    pub fn payload(&self) -> &ScalarPayload {
        &self.payload
    }

    pub fn to_bool(&self) -> bool {
        self.payload.to_bool()
    }

    pub fn to_int(&self) -> i64 {
        self.int
    }

    pub fn to_float(&self) -> f64 {
        self.float
    }

    pub fn to_string_repr(&self) -> &PerlString {
        &self.string
    }

    pub fn is_tainted(&self) -> bool {
        self.payload.is_tainted()
    }

    /// Note a numification against the once-only warning state; returns whether the ops layer should emit the warning
    /// now.  Statically-unwarnable payloads (`None`) answer false with no atomic traffic.
    pub fn note_numify_warning(&self) -> bool {
        match &self.numify_warned {
            Some(flag) => !flag.swap(true, Ordering::AcqRel),
            None => false,
        }
    }
}

// ── ScalarRef — shared identity (§2.3.1) ──────────────────────────
/// The Mut/Const split.  Reference identity is `Arc::ptr_eq`; `Const` reads take no lock; `write()` on a `Const` has no
/// lock to hand out — the mutation failure is structural.
#[derive(Clone)]
pub enum ScalarRef {
    Mut(HeapArc<RwLock<ScalarCell>>),
    Const(HeapArc<ConstScalar>),
}

impl std::fmt::Debug for ScalarRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            ScalarRef::Mut(_) => "Mut",
            ScalarRef::Const(_) => "Const",
        };
        write!(f, "ScalarRef::{kind}(0x{:x})", self.addr())
    }
}

impl ScalarRef {
    pub fn new_mut(payload: ScalarPayload) -> ScalarRef {
        ScalarRef::Mut(HeapArc::new(RwLock::new(ScalarCell::Plain(payload))))
    }

    pub fn new_const(cell: ConstScalar) -> ScalarRef {
        ScalarRef::Const(HeapArc::new(cell))
    }

    /// The cell address — the value perl exposes when a reference is numified or stringified (`SCALAR(0x...)`); stable
    /// for the identity's lifetime, shared by clones.
    pub fn addr(&self) -> usize {
        match self {
            ScalarRef::Mut(c) => HeapArc::as_ptr(c) as usize,
            ScalarRef::Const(c) => HeapArc::as_ptr(c) as usize,
        }
    }

    /// Reference identity (§2.3.1): what `==` on Perl references compares.
    pub fn ptr_eq(a: &ScalarRef, b: &ScalarRef) -> bool {
        match (a, b) {
            (ScalarRef::Mut(x), ScalarRef::Mut(y)) => HeapArc::ptr_eq(x, y),
            (ScalarRef::Const(x), ScalarRef::Const(y)) => HeapArc::ptr_eq(x, y),
            _ => false,
        }
    }

    /// The unified read accessor (§2.3.1): a guard viewing the cell either way.  `Const` reads take no lock.
    pub fn read(&self) -> ScalarReadGuard<'_> {
        match self {
            ScalarRef::Mut(cell) => ScalarReadGuard::Mut(cell.read()),
            ScalarRef::Const(cell) => ScalarReadGuard::Const(cell),
        }
    }

    /// The write accessor: `Const` has no lock to hand out — `ReadOnly` is structural, before any lock talk.
    pub fn write(&self) -> Result<ScalarWriteGuard<'_>, ScalarError> {
        match self {
            ScalarRef::Mut(cell) => Ok(ScalarWriteGuard(cell.write())),
            ScalarRef::Const(_) => Err(ScalarError::ReadOnly),
        }
    }
}

/// The read view over either cell kind.  Coercion reads on `Mut` go through the cell's caches; on `Const` they are the
/// materialized fields.
pub enum ScalarReadGuard<'a> {
    Mut(RwLockReadGuard<'a, ScalarCell>),
    Const(&'a ConstScalar),
}

impl ScalarReadGuard<'_> {
    pub fn payload(&self) -> &ScalarPayload {
        match self {
            ScalarReadGuard::Mut(g) => g.payload(),
            ScalarReadGuard::Const(c) => c.payload(),
        }
    }

    pub fn to_bool(&self) -> bool {
        match self {
            ScalarReadGuard::Mut(g) => g.to_bool(),
            ScalarReadGuard::Const(c) => c.to_bool(),
        }
    }

    pub fn to_int(&self) -> i64 {
        match self {
            ScalarReadGuard::Mut(g) => g.to_int(),
            ScalarReadGuard::Const(c) => c.to_int(),
        }
    }

    pub fn to_float(&self) -> f64 {
        match self {
            ScalarReadGuard::Mut(g) => g.to_float(),
            ScalarReadGuard::Const(c) => c.to_float(),
        }
    }

    pub fn to_string_repr(&self) -> Result<PerlString, AllocError> {
        match self {
            ScalarReadGuard::Mut(g) => g.to_string_repr(),
            ScalarReadGuard::Const(c) => Ok(c.to_string_repr().clone()),
        }
    }

    pub fn is_tainted(&self) -> bool {
        match self {
            ScalarReadGuard::Mut(g) => g.is_tainted(),
            ScalarReadGuard::Const(c) => c.is_tainted(),
        }
    }
}

/// The write view (only `Mut` cells reach here).  The dynamic readonly flag is checked at the mutation (`assign`), not
/// at guard acquisition — acquiring a write guard to *toggle* readonly must remain possible.
pub struct ScalarWriteGuard<'a>(RwLockWriteGuard<'a, ScalarCell>);

impl std::ops::Deref for ScalarWriteGuard<'_> {
    type Target = ScalarCell;

    fn deref(&self) -> &ScalarCell {
        &self.0
    }
}

impl std::ops::DerefMut for ScalarWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut ScalarCell {
        &mut self.0
    }
}

// ── The boolean immortals (§2.3.3) ────────────────────────────────
/// Fallback-free materialization for the immortals: the payloads' renderings are tiny ASCII, so the inline path cannot
/// allocate; the unreachable error arm degrades to an unmaterialized-string cell rather than panicking (no-panic
/// policy).
fn immortal(payload: ScalarPayload) -> ScalarRef {
    let cell = ConstScalar::materialize(payload.clone()).unwrap_or_else(|_| ConstScalar {
        payload,
        int: 0,
        float: 0.0,
        string: PerlString::empty(),
        numify_warned: None,
    });

    ScalarRef::Const(HeapArc::new(cell))
}

/// The true immortal: `ScalarPayload::True`, materialized as 1 / 1.0 / `"1"` (§2.3.3, as amended).
pub static TRUE_SCALAR: LazyLock<ScalarRef> = LazyLock::new(|| immortal(ScalarPayload::True));

/// The false immortal: `ScalarPayload::False`, the dualvar — numerically 0, string `""` (§2.3.3).
pub static FALSE_SCALAR: LazyLock<ScalarRef> = LazyLock::new(|| immortal(ScalarPayload::False));

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests/scalar_tests.rs"]
mod tests;
