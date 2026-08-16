//! The promoted-scalar layer (§2.3.1–§2.3.4): `ScalarRef` shared identity over the Mut/Const split, `ScalarCell` with
//! in-place `Plain`→`Full` upgrade, `ConstScalar` with coercions materialized at birth, the boolean immortal
//! singletons, the structural readonly error path, and numification-warning state.
//!
//! The module name is temporary in the same sense as `string.rs` and `payload.rs`: final names arrive when the
//! superseded flag-matrix modules are deleted.  `MagicChain` and `Stash` are carried over at their current stub
//! fidelity; their real shapes are later design sections.

use crate::cow_buffer::AllocError;
use crate::string::{DECODE_MAX, PerlString};
use crate::value::{DualPayload, Numeric, ScalarPayload, Tainted};
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::fmt;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{LazyLock, OnceLock};

use crate::heap::{HeapArc, release_payload};

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

impl fmt::Display for ScalarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
            ScalarCell::Plain(p) => mem::replace(p, ScalarPayload::undef(Tainted::CLEAN)),
            ScalarCell::Full(f) => mem::replace(&mut f.payload, ScalarPayload::undef(Tainted::CLEAN)),
        };
        if crate::value::Value::payload_carries_strong_edge(&payload) {
            release_payload(payload);
        }
    }
}

impl fmt::Debug for ScalarCell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
    pub fn stringify(&self) -> Result<PerlString, AllocError> {
        match self {
            ScalarCell::Plain(p) => p.stringify(),
            ScalarCell::Full(f) => {
                if let Some(s) = f.cached_string.get() {
                    return Ok(s.clone());
                }

                let v = f.payload.stringify()?;
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
            let payload = mem::replace(p, ScalarPayload::undef(Tainted::CLEAN));
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
            // Clearing a flag a `Plain` cell cannot carry: nothing to do, and no reason to promote it.
            ScalarCell::Plain(_) if !readonly => {}
            ScalarCell::Plain(_) | ScalarCell::Full(_) => self.upgrade_to_full().readonly = readonly,
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
    /// should emit the warning *now*: true exactly when the payload would warn and no numeric face has been cached yet.
    ///
    /// The suppressor is that cached face rather than a flag — perl's own mechanism, where numifying stores the
    /// salvaged number under `IOKp` and later numifications read it instead of re-parsing.  Installing it replaces the
    /// payload with a `Dual`, so the write lock is required; perl needs mutable access here too, since `$arr[0] + 0`
    /// mutates the element's SV.  Copies then carry the face, which is why copy-after-numification is silent while
    /// copy-before warns on both (container-verified).
    pub fn numify_noting_warning(&mut self) -> Result<(Numeric, Option<NumifyWarning>), AllocError> {
        let payload = match self {
            ScalarCell::Plain(p) => p,
            ScalarCell::Full(f) => &mut f.payload,
        };

        // Only a bare string can warn: a `Dual` already holds the face, and every other payload is numeric already.
        // For the string the value and the verdict come from the one walk (§2.3.4).
        let (n, warns) = match &*payload {
            ScalarPayload::String(s) => s.numify_noting_warning(),
            other => (other.numify(), false),
        };
        if !warns {
            return Ok((n, None));
        }

        let taken = mem::replace(payload, ScalarPayload::Undef);
        let (snippet, truncated) = match taken {
            ScalarPayload::String(s) => {
                let tainted = s.is_tainted();
                let bound = if s.is_utf8() { WARN_SNIPPET_CHARS } else { WARN_SNIPPET_BYTES };
                let snippet = s.message_prefix(bound)?;
                let dual = HeapArc::new(DualPayload { string: s, numeric: n });
                *payload = if tainted { ScalarPayload::DualTainted(dual) } else { ScalarPayload::Dual(dual) };
                snippet
            }
            other => {
                *payload = other;

                // Unreachable: `warns` is true only for the string arm above.  Reported rather than silently laundered,
                // of the bomb's family.
                unreachable!("a non-string payload claimed a numify warning");
            }
        };

        Ok((n, Some(NumifyWarning::NotNumeric { snippet, truncated })))
    }
}

const _: () = assert!(size_of::<ScalarCell>() == 16);

// ── ConstScalar — frozen at birth (§2.3.3) ────────────────────────
/// The lockless immutable cell: every coercion materialized at construction, reads are plain field access, trivially
/// `Sync`.  The single mutable exception is the numification-warning once-bit, present only when the payload can warn
/// (`None` makes "cannot warn" structural — eager knowledge, lazy surfacing, §2.3.4).
pub struct ConstScalar {
    payload: ScalarPayload,
    int: i64,
    float: f64,
    string: PerlString,
    numify_warned: Option<(AtomicBool, PerlString, bool)>,
}

impl Drop for ConstScalar {
    /// Iterative teardown (§2.4.9): frozen payloads can carry graph edges too (§2.4.10).
    fn drop(&mut self) {
        let payload = mem::replace(&mut self.payload, ScalarPayload::undef(Tainted::CLEAN));
        if crate::value::Value::payload_carries_strong_edge(&payload) {
            release_payload(payload);
        }
    }
}

impl fmt::Debug for ConstScalar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConstScalar").field("payload", &self.payload).finish_non_exhaustive()
    }
}

impl ConstScalar {
    /// Materialize a payload into a frozen cell (at most two short strings and two numbers, §2.3.3).
    pub fn materialize(payload: ScalarPayload) -> Result<ConstScalar, AllocError> {
        let int = payload.to_int();
        let float = payload.to_float();
        let string = payload.stringify()?;

        // The snippet is precomputed here, where materialization is already fallible, so the once-bit's gate stays
        // free of allocation at note time.
        let numify_warned = match &payload {
            ScalarPayload::String(s) if s.numify_noting_warning().1 => {
                let bound = if s.is_utf8() { WARN_SNIPPET_CHARS } else { WARN_SNIPPET_BYTES };
                let (snippet, truncated) = s.message_prefix(bound)?;
                Some((AtomicBool::new(false), snippet, truncated))
            }
            _ => None,
        };

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

    pub fn stringify(&self) -> &PerlString {
        &self.string
    }

    pub fn is_tainted(&self) -> bool {
        self.payload.is_tainted()
    }

    /// Note a numification against the once-only warning state; `Some` carries the typed event exactly once, the first
    /// time.  Statically-unwarnable payloads answer `None` with no atomic traffic, and the snippet was precomputed at
    /// materialization, so noting never allocates.
    pub fn note_numify_warning(&self) -> Option<NumifyWarning> {
        match &self.numify_warned {
            Some((flag, snippet, truncated)) if !flag.swap(true, Ordering::AcqRel) => {
                Some(NumifyWarning::NotNumeric { snippet: snippet.clone(), truncated: *truncated })
            }
            Some(_) | None => None,
        }
    }
}

// ── NumifyWarning — the typed warning event (§2.3.4) ──────────────
/// A warning a numify operation raises, as data — the full inventory of perl 5.44's numification warnings, each variant
/// carrying the raw payload its message needs and never more (a caller wanting the full value clones it before
/// numifying).  `Display` *is* perl's standard formatting: the message body, rendered on demand from the raw payload
/// per the two-regime fragment law (§2.3.4), compound variants emitting their bodies as emission-ordered lines.  The
/// interpreter composes by suffixing — the op clause (`NotNumeric` only, per perl's `PL_op` behavior) and the location
/// follow the body in every form — and owns category bits, FATALization, and `$SIG{__WARN__}` dispatch.
#[derive(Debug)]
pub enum NumifyWarning {
    /// `Argument "%s" isn't numeric`: not one complete numeric token (§2.3.4).  The snippet is bounded by the rendering
    /// law — [`WARN_SNIPPET_BYTES`] unflagged, [`WARN_SNIPPET_CHARS`] flagged, the cut sequence-clean, under the face's
    /// own utf8 flag — and `truncated` says the face extended beyond it, which is what the trailing `...` reports when
    /// rendering exhausts the snippet.
    NotNumeric { snippet: PerlString, truncated: bool },

    /// `Argument "%s" treated as 0 in increment (++)`: the magic string increment received content it cannot step.
    /// Same snippet law as `NotNumeric`.  Constructed when the increment family lands.
    NotIncrementable { snippet: PerlString, truncated: bool },

    /// `Lost precision when incrementing %f by 1` (or decrementing): the float's integer neighbors are farther than one
    /// apart.  Constructed when the increment family lands.
    LostPrecision { value: f64, decrement: bool },

    /// `Illegal %s digit '%c' ignored`: the radix scan stopped at a digit outside its base (§2.3.4) — never base 10,
    /// octal only for the digits 8 and 9.  Constructed when the grok operations land, as are its four siblings.
    IllegalDigit { base: RadixBase, digit: u8 },

    /// `Integer overflow in %s number`: the magnitude passed `u64::MAX` mid-scan (§2.3.4).
    Overflow { base: RadixBase },

    /// `%s non-portable`: the value exceeded 32 bits (§2.3.4).  Perl's overflow path suppresses this, so overflow and
    /// non-portable never co-fire — which the variant set makes unrepresentable rather than merely documented.
    NonPortable { base: RadixBase },

    /// The compound of a scan that overflowed and then stopped at an illegal digit — the one legal pair in that order
    /// (§2.3.4): overflow fires mid-scan, scanning continues to find the number's end, and the finish block fires the
    /// digit warning.
    OverflowThenIllegalDigit { base: RadixBase, digit: u8 },

    /// The compound of a scan that stopped at an illegal digit and whose value exceeded 32 bits — the other legal pair,
    /// in that order (§2.3.4).
    IllegalDigitThenNonPortable { base: RadixBase, digit: u8 },

    /// `Use of uninitialized value`: undef numified.  The variable-name diagnosis is interpreter machinery entire, so
    /// the event is the bare fact.  Constructed when undef numification routes events.
    Uninitialized,
}

/// The radix a grok warning names.  Decimal is absent by perl's law: base 10 has historically never warned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RadixBase {
    Binary,
    Octal,
    Hexadecimal,
}

impl RadixBase {
    /// The word perl's messages use.
    fn name(self) -> &'static str {
        match self {
            RadixBase::Binary => "binary",
            RadixBase::Octal => "octal",
            RadixBase::Hexadecimal => "hexadecimal",
        }
    }

    /// The threshold phrase of the non-portable message, which quotes the 32-bit ceiling in its own radix.
    fn non_portable_threshold(self) -> &'static str {
        match self {
            RadixBase::Binary => "Binary number > 0b11111111111111111111111111111111",
            RadixBase::Octal => "Octal number > 037777777777",
            RadixBase::Hexadecimal => "Hexadecimal number > 0xffffffff",
        }
    }
}

/// The unflagged snippet bound (§2.3.4): the byte renderer consumes source while its output is under 56 columns and
/// every byte renders at least one, so at most 56 bytes are consumed and the 57th's existence is the last fact the
/// ellipsis needs.
pub const WARN_SNIPPET_BYTES: usize = 57;

/// The flagged snippet bound (§2.3.4): the character renderer's cap is 32 columns, so at most 32 characters are
/// consumed, plus the 33rd for the ellipsis.
pub const WARN_SNIPPET_CHARS: usize = 33;

impl fmt::Display for NumifyWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NumifyWarning::NotNumeric { snippet, truncated } => {
                write!(f, "Argument \"")?;
                render_warn_fragment(snippet, *truncated, f)?;
                write!(f, "\" isn't numeric")
            }
            NumifyWarning::NotIncrementable { snippet, truncated } => {
                write!(f, "Argument \"")?;
                render_warn_fragment(snippet, *truncated, f)?;
                write!(f, "\" treated as 0 in increment (++)")
            }
            NumifyWarning::LostPrecision { value, decrement } => {
                let verb = if *decrement { "decrementing" } else { "incrementing" };
                write!(f, "Lost precision when {verb} {value} by 1")
            }
            NumifyWarning::IllegalDigit { base, digit } => fmt_illegal_digit(f, *base, *digit),
            NumifyWarning::Overflow { base } => fmt_overflow(f, *base),
            NumifyWarning::NonPortable { base } => fmt_non_portable(f, *base),
            NumifyWarning::OverflowThenIllegalDigit { base, digit } => {
                // A compound's lines are its warnings in emission order (§2.3.4), one perl body each.
                fmt_overflow(f, *base)?;
                writeln!(f)?;
                fmt_illegal_digit(f, *base, *digit)
            }
            NumifyWarning::IllegalDigitThenNonPortable { base, digit } => {
                fmt_illegal_digit(f, *base, *digit)?;
                writeln!(f)?;
                fmt_non_portable(f, *base)
            }
            NumifyWarning::Uninitialized => write!(f, "Use of uninitialized value"),
        }
    }
}

fn fmt_illegal_digit(f: &mut fmt::Formatter<'_>, base: RadixBase, digit: u8) -> fmt::Result {
    write!(f, "Illegal {} digit '{}' ignored", base.name(), digit as char)
}

fn fmt_overflow(f: &mut fmt::Formatter<'_>, base: RadixBase) -> fmt::Result {
    write!(f, "Integer overflow in {} number", base.name())
}

fn fmt_non_portable(f: &mut fmt::Formatter<'_>, base: RadixBase) -> fmt::Result {
    write!(f, "{} non-portable", base.non_portable_threshold())
}

/// The two-regime fragment renderer (§2.3.4), exact to `S_sv_display` and the container probes.  Unflagged: output cap
/// 56, checked before each byte with the last expansion free to overrun; `M-` meta notation re-dispatching the low
/// seven bits through the same table; `\n \r \f \\ \0` backslash forms; other controls caret; printability pinned to
/// the C locale.  Flagged: output cap 32; printable ASCII verbatim; backslash doubled; everything else — newline
/// included — `\x{lowercase-hex}` per code point.  Both append three ASCII periods iff source remains, which is the
/// snippet running out early or the face having extended past it.
fn render_warn_fragment(snippet: &PerlString, truncated: bool, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut scratch = [0u8; DECODE_MAX];
    let bytes = snippet.as_bytes(&mut scratch);
    let mut columns = 0usize;
    let mut remains = truncated;

    if !snippet.is_utf8() {
        let mut at = 0usize;
        while at < bytes.len() {
            if columns >= 56 {
                remains = true;
                break;
            }

            let mut ch = bytes[at];
            at += 1;
            if !(0x20..=0x7E).contains(&ch) && ch >= 0x80 {
                write!(f, "M-")?;
                columns += 2;
                ch &= 0x7F;
            }

            match ch {
                b'\n' => {
                    write!(f, "\\n")?;
                    columns += 2;
                }
                b'\r' => {
                    write!(f, "\\r")?;
                    columns += 2;
                }
                0x0C => {
                    write!(f, "\\f")?;
                    columns += 2;
                }
                b'\\' => {
                    write!(f, "\\\\")?;
                    columns += 2;
                }
                0 => {
                    write!(f, "\\0")?;
                    columns += 2;
                }
                0x20..=0x7E => {
                    write!(f, "{}", ch as char)?;
                    columns += 1;
                }
                other => {
                    // Caret notation: control characters XOR 0x40 (DEL becomes `^?`).
                    write!(f, "^{}", (other ^ 0x40) as char)?;
                    columns += 2;
                }
            }
        }
    } else {
        let mut at = 0usize;
        while at < bytes.len() {
            if columns >= 32 {
                remains = true;
                break;
            }

            // One perl-extended character: lead byte plus continuations; the decoded code point feeds the escape.
            let start = at;
            at += 1;
            while at < bytes.len() && bytes[at] & 0xC0 == 0x80 {
                at += 1;
            }

            let unit = &bytes[start..at];
            if unit.len() == 1 && (0x20..=0x7E).contains(&unit[0]) && unit[0] != b'\\' {
                write!(f, "{}", unit[0] as char)?;
                columns += 1;
            } else if unit == b"\\" {
                write!(f, "\\\\")?;
                columns += 2;
            } else {
                let cp = decode_extended_code_point(unit);
                let rendered = format!("\\x{{{cp:x}}}");
                columns += rendered.len();
                write!(f, "{rendered}")?;
            }
        }
    }

    if remains {
        write!(f, "...")?;
    }

    Ok(())
}

/// Decode one perl-extended UTF-8 unit to its code point, for the flagged fragment's `\x{{...}}` escape.  The snippet's
/// cuts are sequence-clean by construction, so the unit is whole; a malformed unit inside flagged content renders as
/// its first byte's value, which is perl's own display degradation rather than a crash.
fn decode_extended_code_point(unit: &[u8]) -> u64 {
    let lead = unit[0];
    if lead < 0x80 {
        return u64::from(lead);
    }

    let payload_bits = match lead {
        0xC0..=0xDF => 5,
        0xE0..=0xEF => 4,
        0xF0..=0xF7 => 3,
        0xF8..=0xFB => 2,
        0xFC..=0xFD => 1,
        _ => 0,
    };

    let mut cp = u64::from(lead & ((1 << payload_bits) - 1));
    for &b in &unit[1..] {
        cp = (cp << 6) | u64::from(b & 0x3F);
    }

    cp
}

// ── ScalarRef — shared identity (§2.3.1) ──────────────────────────
/// The Mut/Const split.  Reference identity is `Arc::ptr_eq`; `Const` reads take no lock; `write()` on a `Const` has no
/// lock to hand out — the mutation failure is structural.
#[derive(Clone)]
pub enum ScalarRef {
    Mut(HeapArc<RwLock<ScalarCell>>),
    Const(HeapArc<ConstScalar>),
}

impl fmt::Debug for ScalarRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
        // Exhaustive over the pairs: a wildcard here would report that a reference of some future kind is not equal to
        // itself, which is the one answer this function must never give.
        match (a, b) {
            (ScalarRef::Mut(x), ScalarRef::Mut(y)) => HeapArc::ptr_eq(x, y),
            (ScalarRef::Const(x), ScalarRef::Const(y)) => HeapArc::ptr_eq(x, y),
            (ScalarRef::Mut(_), ScalarRef::Const(_)) | (ScalarRef::Const(_), ScalarRef::Mut(_)) => false,
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

    pub fn stringify(&self) -> Result<PerlString, AllocError> {
        match self {
            ScalarReadGuard::Mut(g) => g.stringify(),
            ScalarReadGuard::Const(c) => Ok(c.stringify().clone()),
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

impl Deref for ScalarWriteGuard<'_> {
    type Target = ScalarCell;

    fn deref(&self) -> &ScalarCell {
        &self.0
    }
}

impl DerefMut for ScalarWriteGuard<'_> {
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
