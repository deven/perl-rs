//! The promoted-scalar layer (§2.3.1–§2.3.4): `ScalarRef` shared identity over the Mut/Const split, `ScalarCell` with
//! in-place `Plain`→`Full` upgrade, `ConstScalar` with coercions materialized at birth, the boolean immortal
//! singletons, the structural readonly error path, and numification-warning state.
//!
//! The module name is temporary in the same sense as `string.rs` and `payload.rs`: final names arrive when the
//! superseded flag-matrix modules are deleted.  `MagicChain` and `Stash` are carried over at their current stub
//! fidelity; their real shapes are later design sections.

use crate::cow_buffer::AllocError;
use crate::payload::{Numeric, ScalarPayload, Tainted, string_would_warn};
use crate::string::PerlString;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};

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
    stash: Option<Arc<Stash>>,

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
    pub fn bless(&mut self, stash: Arc<Stash>) {
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
    Mut(Arc<RwLock<ScalarCell>>),
    Const(Arc<ConstScalar>),
}

impl ScalarRef {
    pub fn new_mut(payload: ScalarPayload) -> ScalarRef {
        ScalarRef::Mut(Arc::new(RwLock::new(ScalarCell::Plain(payload))))
    }

    pub fn new_const(cell: ConstScalar) -> ScalarRef {
        ScalarRef::Const(Arc::new(cell))
    }

    /// Reference identity (§2.3.1): what `==` on Perl references compares.
    pub fn ptr_eq(a: &ScalarRef, b: &ScalarRef) -> bool {
        match (a, b) {
            (ScalarRef::Mut(x), ScalarRef::Mut(y)) => Arc::ptr_eq(x, y),
            (ScalarRef::Const(x), ScalarRef::Const(y)) => Arc::ptr_eq(x, y),
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

    ScalarRef::Const(Arc::new(cell))
}

/// The true immortal: `ScalarPayload::True`, materialized as 1 / 1.0 / `"1"` (§2.3.3, as amended).
pub static TRUE_SCALAR: LazyLock<ScalarRef> = LazyLock::new(|| immortal(ScalarPayload::True));

/// The false immortal: `ScalarPayload::False`, the dualvar — numerically 0, string `""` (§2.3.3).
pub static FALSE_SCALAR: LazyLock<ScalarRef> = LazyLock::new(|| immortal(ScalarPayload::False));

// ── Tests ─────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::Value;

    fn plain(payload: ScalarPayload) -> ScalarRef {
        ScalarRef::new_mut(payload)
    }

    fn str_payload(text: &str) -> ScalarPayload {
        ScalarPayload::String(text.parse().unwrap())
    }

    // ── The §2.3.3 singleton contract, pinned ─────────────────────
    #[test]
    fn boolean_immortals_share_identity() {
        // Verified perl 5.38: \(1==1) yields the same address twice.
        let a = Value::True.upgrade_to_scalar().unwrap();
        let b = Value::True.upgrade_to_scalar().unwrap();
        assert!(ScalarRef::ptr_eq(&a, &b));
        assert!(matches!(a, ScalarRef::Const(_)));

        let f1 = Value::False.upgrade_to_scalar().unwrap();
        let f2 = Value::False.upgrade_to_scalar().unwrap();
        assert!(ScalarRef::ptr_eq(&f1, &f2));
        assert!(!ScalarRef::ptr_eq(&a, &f1), "the two singletons are distinct");
    }

    #[test]
    fn immortals_prematerialized_values() {
        let t = TRUE_SCALAR.read();
        assert!(matches!(t.payload(), ScalarPayload::True));
        assert_eq!(t.to_int(), 1);
        assert_eq!(t.to_float(), 1.0);
        assert_eq!(t.to_string_repr().unwrap().as_bytes(), b"1");
        assert!(t.to_bool());

        // The dualvar: numerically 0, string "" (not "0") — verified: (1==0)."" has length 0.
        let f = FALSE_SCALAR.read();
        assert!(matches!(f.payload(), ScalarPayload::False));
        assert_eq!(f.to_int(), 0);
        assert_eq!(f.to_float(), 0.0);
        assert_eq!(f.to_string_repr().unwrap().as_bytes(), b"");
        assert!(!f.to_bool());
    }

    #[test]
    fn immortal_mutation_is_the_readonly_error_never_a_panic() {
        match TRUE_SCALAR.write() {
            Err(ScalarError::ReadOnly) => {}
            _ => panic!("Const write must fail structurally"),
        }

        assert_eq!(ScalarError::ReadOnly.to_string(), "Modification of a read-only value attempted");
    }

    #[test]
    fn cross_thread_upgrades_still_ptr_eq() {
        // Guards LazyLock initialization races: a fresh thread's upgrade is the same singleton.
        let here = Value::True.upgrade_to_scalar().unwrap();
        let there = std::thread::spawn(|| Value::True.upgrade_to_scalar().unwrap());
        let there = there.join().unwrap_or_else(|_| Value::True.upgrade_to_scalar().unwrap());
        assert!(ScalarRef::ptr_eq(&here, &there));
    }

    #[test]
    fn is_bool_answers_from_the_variant() {
        assert!(Value::True.is_bool());
        assert!(Value::False.is_bool());
        assert!(!Value::Int(1, Tainted::CLEAN).is_bool());
        assert!(!Value::String("".parse().unwrap()).is_bool());
    }

    // ── ScalarRef / guards ────────────────────────────────────────
    #[test]
    fn reference_identity_and_clone_share() {
        let r1 = plain(ScalarPayload::Int(42, Tainted::CLEAN));
        let r2 = r1.clone();
        assert!(ScalarRef::ptr_eq(&r1, &r2));
        let r3 = plain(ScalarPayload::Int(42, Tainted::CLEAN));
        assert!(!ScalarRef::ptr_eq(&r1, &r3), "equal payloads, distinct identities");

        // Writes through one handle are visible through the other: shared identity.
        r1.write().unwrap().assign(ScalarPayload::Int(7, Tainted::CLEAN)).unwrap();
        assert_eq!(r2.read().to_int(), 7);
    }

    #[test]
    fn concurrent_const_reads_take_no_lock() {
        // Trivially concurrent: many threads reading the same Const cell simultaneously.
        let cell = ConstScalar::materialize(str_payload("3.7")).unwrap();
        let r = ScalarRef::new_const(cell);
        std::thread::scope(|s| {
            for _ in 0..4 {
                let r = &r;
                s.spawn(move || {
                    for _ in 0..1000 {
                        assert_eq!(r.read().to_int(), 3);
                        assert_eq!(r.read().to_float(), 3.7);
                    }
                });
            }
        });
    }

    // ── ScalarCell: payload authority, caches, upgrade ────────────
    #[test]
    fn payload_stays_authoritative_through_coercion() {
        // The §21.1 illustrative test: 3.7 used as an integer still stringifies as "3.7".
        let r = plain(ScalarPayload::Float(3.7, Tainted::CLEAN));
        assert_eq!(r.read().to_int(), 3);
        assert_eq!(r.read().to_string_repr().unwrap().as_bytes(), b"3.7");
    }

    #[test]
    fn full_cell_caches_and_invalidation() {
        let r = plain(ScalarPayload::Float(3.7, Tainted::CLEAN));
        r.write().unwrap().upgrade_to_full();

        // Repeated coercions agree through the caches (fill under concurrent read guards).
        std::thread::scope(|s| {
            for _ in 0..4 {
                let r = &r;
                s.spawn(move || {
                    for _ in 0..500 {
                        let g = r.read();
                        assert_eq!(g.to_int(), 3);
                        assert_eq!(g.to_float(), 3.7);
                        assert_eq!(g.to_string_repr().unwrap().as_bytes(), b"3.7");
                    }
                });
            }
        });

        // Assignment is the single choke point: caches drop with the payload.
        r.write().unwrap().assign(ScalarPayload::Int(9, Tainted::CLEAN)).unwrap();
        let g = r.read();
        assert_eq!(g.to_int(), 9);
        assert_eq!(g.to_float(), 9.0);
        assert_eq!(g.to_string_repr().unwrap().as_bytes(), b"9");
    }

    #[test]
    fn upgrade_preserves_identity_and_payload() {
        let r = plain(str_payload("hello"));
        let alias = r.clone();

        {
            let mut g = r.write().unwrap();
            assert!(matches!(&*g, ScalarCell::Plain(_)));
            g.upgrade_to_full();
            g.upgrade_to_full(); // idempotent
            assert!(matches!(&*g, ScalarCell::Full(_)));
        }

        // The Arc address never changed: the outstanding alias still reaches the upgraded cell.
        assert!(ScalarRef::ptr_eq(&r, &alias));
        assert_eq!(alias.read().to_string_repr().unwrap().as_bytes(), b"hello");
    }

    #[test]
    fn magic_and_bless_attach_in_place() {
        let r = plain(ScalarPayload::Int(1, Tainted::CLEAN));

        {
            let mut g = r.write().unwrap();
            assert!(!g.has_magic());
            g.set_magic(MagicChain { _private: () });
            g.bless(Arc::new(Stash { _private: () }));
            assert!(g.has_magic());
        }

        assert_eq!(r.read().to_int(), 1, "payload survives the attachments");
    }

    // ── The readonly error path ───────────────────────────────────
    #[test]
    fn dynamic_readonly_is_toggleable() {
        let r = plain(ScalarPayload::Int(5, Tainted::CLEAN));

        r.write().unwrap().set_readonly(true);
        assert!(r.write().unwrap().is_readonly(), "the flag is set; acquiring the guard stays legal");
        assert_eq!(r.write().unwrap().assign(ScalarPayload::Int(6, Tainted::CLEAN)), Err(ScalarError::ReadOnly));
        assert_eq!(r.read().to_int(), 5, "the failed assignment changed nothing");

        // Internals::SvREADONLY is toggleable: clear and assign.
        r.write().unwrap().set_readonly(false);
        r.write().unwrap().assign(ScalarPayload::Int(6, Tainted::CLEAN)).unwrap();
        assert_eq!(r.read().to_int(), 6);

        // Clearing readonly on a Plain cell is a no-op that must not upgrade.
        let p = plain(ScalarPayload::Int(1, Tainted::CLEAN));
        p.write().unwrap().set_readonly(false);
        assert!(matches!(&*p.write().unwrap(), ScalarCell::Plain(_)));
    }

    // ── Numification-warning state (§2.3.4, container-verified) ───
    #[test]
    fn numify_warns_once_and_copies_carry_the_state() {
        // "abc" + 1 twice warns once.
        let r = plain(str_payload("abc"));
        let (n1, emit1) = r.write().unwrap().numify_noting_warning();
        assert_eq!(n1, Numeric::Float(0.0));
        assert!(emit1, "first numification warns");
        let (_, emit2) = r.write().unwrap().numify_noting_warning();
        assert!(!emit2, "second is silent — the once-bit");

        // Copy AFTER first numification: the copy is silent (the bit rides the PerlString tag).
        let copied = r.read().payload().clone();
        let r2 = plain(copied);
        let (_, emit3) = r2.write().unwrap().numify_noting_warning();
        assert!(!emit3, "copy after first numification is silent (verified)");

        // Copy BEFORE: both warn.
        let a = plain(str_payload("12abc"));
        let b = plain(a.read().payload().clone());
        assert!(a.write().unwrap().numify_noting_warning().1);
        assert!(b.write().unwrap().numify_noting_warning().1, "copy before numification warns independently");

        // Clean numerics never emit.
        let c = plain(str_payload("  12  "));
        assert!(!c.write().unwrap().numify_noting_warning().1);
    }

    #[test]
    fn const_cell_warning_state() {
        let warns = ConstScalar::materialize(str_payload("abc")).unwrap();
        assert!(warns.note_numify_warning(), "first note emits");
        assert!(!warns.note_numify_warning(), "second is silent");

        // Statically-unwarnable payloads carry nothing (§2.3.4).
        let silent = ConstScalar::materialize(ScalarPayload::Int(5, Tainted::CLEAN)).unwrap();
        assert!(silent.numify_warned.is_none());
        assert!(!silent.note_numify_warning());
        let clean_str = ConstScalar::materialize(str_payload("42")).unwrap();
        assert!(clean_str.numify_warned.is_none());
    }

    // ── The §2.3.4 would-warn boundary table, pinned in full ──────
    #[test]
    fn would_warn_boundary_table() {
        let warns = [
            "abc",
            "12abc",
            "1e",
            "1e+",
            "0x10",
            "",
            "12.5abc",
            ".",
            "+",
            "-",
            "0.5.3",
            "1_000",
            "infx",
            "nanx",
            "  ",
            "0 But True",
            "0 but true ",
            " 0 but true",
            "0 but false",
        ];
        let silent = [
            "12",
            " 12",
            "12 ",
            "  12  ",
            "\t12\n",
            "3.5",
            "1e5",
            "0 but true",
            "inf",
            "Inf",
            "+5",
            "5.",
            ".5",
            "nan",
            "infinity",
            "INFINITY",
            "0E0",
            "-inf",
            "+nan",
        ];

        for form in warns {
            assert!(string_would_warn(form.as_bytes()), "{form:?} must warn (container-verified)");
        }

        for form in silent {
            assert!(!string_would_warn(form.as_bytes()), "{form:?} must be silent (container-verified)");
        }
    }

    // ── Layout (§2.3.6) ───────────────────────────────────────────
    #[test]
    fn envelope_sizes() {
        assert_eq!(size_of::<ScalarCell>(), 24, "Full threads the payload's niche (measured, §2.3.2)");
        assert_eq!(size_of::<ScalarRef>(), 16);
    }
}
