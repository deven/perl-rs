# Perl in Rust: Architecture and Design

## 0. Why This Document Exists

This is the master design for a new Perl implementation written in Rust,
targeting high compatibility with Perl 5 and eventually offering improved
concurrency, ahead-of-time compilation, and a cleaner extension story.

The document focuses on the decisions that actually determine whether the
project succeeds: value representation, memory management, the
compile/execute interleaving that Perl demands, the lexer–parser feedback
loop, the concurrency model, and a typed layer that bridges Perl and Rust
seamlessly.  These are the places where a wrong early choice costs months.

Throughout the document, "Perl" means Perl 5 unless otherwise qualified.
"Raku" is mentioned only where architectural boundary decisions are needed.

---

## 1. Governing Principles

### 1.1 Perl 5 Is the Specification

There is no formal Perl 5 language specification.  The specification is the
behavior of the C implementation, primarily as documented by:

- `toke.c` (lexer)
- `perly.y` (grammar)
- `op.c`, `pp.c`, `pp_ctl.c`, `pp_hot.c`, `pp_sys.c` (runtime ops)
- `sv.c`, `av.c`, `hv.c` (value model)
- the upstream `t/` test suite

When in doubt, match what `perl` does.  Deliberate divergences are allowed
only in a clearly marked "modern" mode and should be documented as such.

### 1.2 Design the Hard Parts First

A language implementation lives or dies by its answers to a handful of
load-bearing questions:

1. How are values represented in memory?
2. How is memory reclaimed?
3. How does compile-time execution interleave with parsing?
4. How do the lexer and parser communicate state?
5. What does the execution substrate look like?

Everything else — builtin libraries, test harness, CLI flags — is
important but structurally subordinate to those five.  This document
addresses them in that order of priority.

### 1.3 Clean Boundaries over Premature Generality

The project should be layered with well-defined interfaces between stages,
but it should not introduce abstraction speculatively.  Specifically:

- Do not design a "universal front-end interface" for Perl 5 and Raku
  simultaneously.  Design for Perl 5.  If Raku is added later, it gets its
  own front end; sharing happens at the IR/runtime layer where semantics
  genuinely overlap.

- Do not build a generic "plugin compiler framework."  Build a Perl
  compiler.  Generalize only when a second concrete consumer appears.

### 1.4 Exploit Rust's Strengths, Respect Its Constraints

Rust gives us: memory safety without GC, fearless concurrency, algebraic
types, pattern matching, zero-cost abstractions, and a strong ecosystem.

Rust constrains us: no implicit reference cycles, no easy interior
mutability, borrow checker friction on graph-shaped data.  The value
representation must be designed *with* those constraints, not against them.

---

## 2. Value Representation

This is the single most consequential design decision.  Every other
subsystem depends on it.

### 2.1 The Problem

A Perl scalar is not a simple tagged union.  It is a mutable container that
can simultaneously hold a string, an integer, and a float (the
`SVf_IOK | SVf_NOK | SVf_POK` situation in Perl 5).  It supports:

- lazy coercion between numeric and string forms
- reference semantics (`\$x`)
- magic (tied variables, `$!`, `pos()`, etc.)
- blessing into packages (objects)
- overloaded operators
- weak references
- read-only / constant flags
- UTF-8 flag on string payload

Arrays and hashes are separate container types with their own identity.

References to any of these create arbitrary graph structures, including
cycles, which Perl 5 leaks unless you break them manually or use
`Scalar::Util::weaken`.

### 2.2 Arc-Based Allocation

Values that need heap allocation use `Arc<RwLock<T>>` — Rust's
standard shared-ownership, thread-safe pointer.  This replaces the
arena-based allocation model that earlier drafts used.

#### 2.2.1 Why `Arc<RwLock<T>>` instead of arenas:

The original rationale for arenas was borrow-checker friendliness
with a per-interpreter `&mut Heap`.  With the shared heap design
(§3.1, §13.3), there is no `&mut Heap` — every access already goes
through per-value locking.  The borrow-checker advantage is gone,
and arenas add complexity (free lists, generation counters, slot
reuse, fragmentation, bounds checking) without compensating benefit.

`Arc<RwLock<T>>`:

- **Prevents use-after-free by construction.**  A value lives as long
  as any `Arc` referencing it exists.  No generation check needed.
- **Standard Rust pattern.**  Familiar to Rust programmers, well-
  tested, well-optimized by the allocator.
- **No arena bookkeeping.**  No free list, no generation counter,
  no slot compaction.
- **Values are independent.**  No coupling to an arena container.
  Values can be shared, moved between data structures, and dropped
  independently.

#### 2.2.2 Concrete types:

```rust
type Sv = Arc<RwLock<Scalar>>;     // full scalar (multi-rep, magic, blessed)
type Av = Arc<RwLock<Vec<Value>>>; // shared array
type Hv = Arc<RwLock<HashMap<PerlString, Value>>>;  // shared hash

enum Value {
    // Compact forms — no heap allocation, no Arc, no locking
    Undef,
    Int(i64),                 // just an integer
    Float(f64),               // just a float
    SmallStr(SmallString),    // short string, inline (≤38 bytes)
    Str(PerlString),          // longer string, heap-allocated
    Ref(Sv),                  // just a reference (points to a full Scalar)

    // Full scalar — all the Perl SV machinery
    Scalar(Sv),               // multi-rep caching, magic, blessing, etc.

    // Container and code types
    Array(Av),
    Hash(Hv),
    Code(Arc<Code>),
    Regex(Arc<CompiledRegex>),

    // Typed value (see §14).  Holds any Rust type
    // that is Send + Sync.
    Typed(Box<dyn TypedVal>),
}

/// Inline short string — covers hash keys, method names, short
/// literals, numeric stringifications, and most temporary strings.
/// Avoids a heap allocation for one of the hottest scalar cases.
struct SmallString {
    len: u8,
    flags: u8,        // see string flags below
    buf: [u8; 38],
}
```

The threshold of 38 bytes is chosen to keep `SmallString` the same
size as `PerlString` (which is `Bytes` at 32 bytes + `flags` at
1 byte + 7 bytes padding = 40 bytes), so the `Value` enum doesn't
grow.  38 bytes covers the vast majority
of short strings: hash keys, field names, small literals, numeric
stringifications like `"42"` or `"3.14159"`, single characters,
and short identifiers.

When a `SmallStr` grows past 38 bytes (via `.=`, `substr` assignment,
etc.), it promotes to `Str(PerlString)` — one heap allocation at the
growth point.  This promotion is one-way; a `Str` that shrinks does
not demote back to `SmallStr`.

The compact forms (`Int`, `Float`, `Str`) are inline — no heap
allocation, no `Arc`, no locking overhead.  The vast majority of
Perl values are simple: just a number, just a string, just a
reference.  Only values that need full Perl SV semantics are
upgraded to `Scalar(Sv)`.

#### 2.2.3 Upgrade from compact to full Scalar:

A compact value is upgraded to `Value::Scalar(Sv)` when any of
these occur:

- **Multi-representation caching.**  `$x = "42"; $x + 0` needs to
  cache both the string and integer forms.  The compact `Str` can
  only hold one.
- **Taking a reference.**  `\$x` needs a stable identity that the
  reference can point to.  The compact `Int(42)` is a copy with no
  address.  Upgrade to `Scalar(Sv)`, then `\$x` clones the `Arc`.
- **`@_` aliasing.**  When a compact value is passed to a `sub`,
  the callee needs to alias the caller's storage.  Upgrade to
  `Scalar(Sv)` so both sides share the same `Arc`.
- **Magic, blessing, taint, read-only.**  These require the `Scalar`
  struct's `magic`, `stash`, and `flags` fields.

**Once upgraded, never downgrade.**  The complexity of reversing an
upgrade is not worth it, and identity (via `Arc` address) must be
preserved — anything holding a reference to the `Sv` would break if
the value reverted to a compact form.

#### 2.2.4 Interaction with `\$x` references:

```perl
my $x = 42;              # Value::Int(42) — compact, no allocation
my $ref = \$x;           # $x upgrades to Value::Scalar(Sv)
                          # $ref = Value::Ref(Sv) — same Arc, refcount 2
my $ref2 = \$x;          # Arc clone — refcount 3
$$ref = "hello";          # mutates through the Arc — $x, $$ref, $$ref2 all see "hello"
```

For typed values (§14.9), `\$x` upgrades to `Arc<T>` as described
in the ownership model.

```rust
trait TypedVal: Send + Sync + Any {
    fn type_name(&self) -> &'static str;
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn clone_boxed(&self) -> Box<dyn TypedVal>;
    fn to_perl_value(&self) -> Value;   // coerce to compat value
}
```

### 2.3 The Scalar Internals

When a value is upgraded from a compact `Value` variant to a full
`Scalar` (see upgrade triggers in §2.2), it gains multi-representation
caching with flag-driven validity.  This matches Perl 5's SV model
where `$x = "42"` sets STR_VALID, then `$x + 0` sets INT_VALID and caches 42 in
`int` without clearing the string.  Multiple representations coexist:

```rust
struct Scalar {
    flags: ScalarFlags,                    // validity + metadata bits
    int: i64,                          // integer cache
    num: f64,                          // float cache
    bytes: PerlStringSlot,             // string cache (with small-string optimization)
    reference: Option<Value>,          // reference target
    magic: Option<Box<MagicChain>>,
    stash: Option<Arc<Stash>>,         // blessed package (for objects)
}

/// Small-string optimization within the Scalar's string cache.
/// Even after upgrading to a full Scalar, short strings avoid a
/// heap allocation for the bytes field.
enum PerlStringSlot {
    None,
    Inline { buf: [u8; 24], len: u8, flags: u8 },
    Heap(PerlString),
}
```

Note that small-string optimization exists at two levels:
`Value::SmallStr` for simple values that are just a short string
(no `Arc`, no `Scalar` struct), and `PerlStringSlot::Inline` for
the string cache inside a full `Scalar` (already behind `Arc`).

#### 2.3.1 Flag discipline:

`ScalarFlags` separates cache-validity bits from orthogonal metadata:

```rust
struct ScalarFlags(u16);

impl ScalarFlags {
    // Cache validity — which representations are current
    const INT_VALID  = 1 << 0;   // int is valid
    const NUM_VALID  = 1 << 1;   // num is valid
    const STR_VALID  = 1 << 2;   // bytes is valid
    const REF_VALID  = 1 << 3;   // ref is valid (this is a reference)

    // Metadata — orthogonal to cache validity
    const UTF8       = 1 << 4;   // bytes is valid UTF-8
    const READONLY   = 1 << 5;   // value is immutable
    const TAINT      = 1 << 6;   // value is tainted
    const MAGICAL    = 1 << 7;   // magic chain is attached
    const WEAK       = 1 << 8;   // this is a weak reference
}
```

The validity bits drive the lazy coercion engine: reading `$x` as a
number checks INT_VALID first (fast path — return `int`), falls back
to NUM_VALID, falls back to computing numeric value from `bytes`,
caches the result by setting INT_VALID/NUM_VALID.  Writing
invalidates the other caches (e.g., assigning a new string clears
INT_VALID and NUM_VALID, sets STR_VALID).

`PerlString` is the heap-allocated string type used by the `Heap`
variant of `PerlStringSlot` (and throughout the runtime for string
keys, identifiers, etc.).  Perl strings are octet sequences with
metadata flags, not Rust `String`.

`Bytes` / `BytesMut` from the `bytes` crate is the backing store,
providing Perl 5's copy-on-write (COW) string semantics:

- **Shared / COW state (`Bytes`):** Multiple SVs can reference the
  same underlying buffer.  Operations like `substr($x, 0, 5)` as an
  rvalue produce a zero-copy `Bytes::slice()`.  `Bytes` is immutable
  and reference-counted — cloning is a cheap refcount bump.

- **Mutation (`BytesMut`):** When a string is mutated (`.=`, `chop`,
  `chomp`, `substr` assignment, `s///`, `vec()`), the `Bytes` handle
  is checked for unique ownership.  If uniquely held, it converts to
  `BytesMut` in place (no copy).  If shared, copy-on-write triggers:
  a new `BytesMut` is allocated, the content is copied, then mutated.

This maps directly to Perl 5's `SvPV_COW` mechanism.

#### 2.3.2 String flags:

A single `flags: u8` byte carries two independent concerns:

```text
bit 0:     Perl UTF-8 flag (SvUTF8) — Perl's semantic "treat as characters"
bits 1-3:  Rust byte-content cache (3 bits, 5 used states):
```

| bits 1-3 | ASCII | valid UTF-8 | description |
|---|---|---|---|
| `000` | yes | yes | pure 7-bit ASCII |
| `001` | no | yes | valid UTF-8 but non-ASCII |
| `010` | no | no | invalid UTF-8 |
| `011` | unknown | yes | valid UTF-8, possibly ASCII |
| `100` | no | unknown | non-ASCII, UTF-8 unknown |
| `111` | unknown | unknown | completely unknown |

States `101` and `110` are unused (ASCII implies valid UTF-8).

The Perl UTF-8 flag (bit 0) is independent of the Rust cache.
Perl's "extended UTF-8" can represent code points above U+10FFFF
and surrogates that Rust rejects, and `Encode::_utf8_on()` sets
the Perl flag without validation.

#### 2.3.3 State transitions — scans only narrow, never re-widen:

- `is_ascii()` scan: `111` → `000` or `100`; `011` → `000` or `001`
- `as_str()` scan: `111` → `000` or `001` or `010`;
  `100` → `001` or `010`

Once a state is narrowed, no subsequent operation widens it unless
the bytes are mutated.  Mutation rules for the Rust cache (bits
1-3), preserving the Perl UTF-8 flag (bit 0) independently:

- **Appending or prepending pure ASCII bytes**: no flag change.
  Cannot affect UTF-8 validity or introduce non-ASCII content.

- **Appending or prepending `&str`** to a valid UTF-8 string:
  preserves `001` or `011`.  Valid UTF-8 concatenated with valid
  UTF-8 is valid UTF-8.

- **Inserting mid-string** into any string results in invalid
  UTF-8 if the byte at the insertion point is a UTF-8 continuation
  byte (`0x80..0xBF`), because this splits a multi-byte sequence
  — transition to `010` (invalid).  This check can be skipped for
  `000` (insertion point is guaranteed ASCII) and for `010`/`111`
  (no validity claim to protect).

- **Adding to an ASCII string** (`000`) inherits the flags of the
  content being added.

- **Adding valid UTF-8** (`&str`, `String`, or a string with known
  `001`/`011` flags) to a valid UTF-8 string preserves UTF-8
  validity.  ASCII status: `001` stays `001`; `000` transitions to
  `001` if the added content is non-ASCII.

- **Removing valid UTF-8 characters** from a valid UTF-8 string
  (respecting character boundaries, not splitting a multi-byte
  sequence): `001` resets to `011` (removing non-ASCII characters
  might leave only ASCII).  `011` stays `011`.

- **Removing non-ASCII bytes**: reset to `111` (remaining content
  is unknown).

- **Any other byte-level mutation** (raw writes, `vec()`, `s///`
  with byte-level replacement): reset to `111`.

These rules are optimizations, not requirements.  The blanket
fallback — reset bits 1-3 to `111` on any mutation — is always
correct.  The lazy scan recovers the information if needed later.
Smart preservation avoids unnecessary re-scanning for common
operations like string concatenation and `chomp`.

#### 2.3.4 `flags == 0` fast path:

When all bits are zero (`flags == 0`), the string is pure ASCII with
no Perl UTF-8 flag — the overwhelmingly common case for typical Perl
code.  A single comparison handles it: safe for `as_str()`, no
multi-byte characters, no special semantics.  This is also the
natural zero-initialized state for strings built from known-ASCII
source bytes.

#### 2.3.5 Construction policy:

- `SmallString`: always scan for ASCII at construction (checking ≤38
  bytes for a high bit is nearly free).  From raw bytes: `000` (ASCII)
  or `100` (non-ASCII, UTF-8 unknown).  From Rust `&str`/`String`:
  `000` (ASCII) or `001` (non-ASCII, valid UTF-8 from type).  This
  is the only eager scan.
- `PerlString` from Rust `&str` or `String`: set `011` (valid UTF-8
  from type, ASCII status deferred).  `String` ownership transfer is
  zero-copy via `Bytes::from()`.
- `PerlString` from arbitrary bytes (I/O, XS, `Encode`, lexer
  literals): set `111`, defer all scanning.

```rust
struct PerlString {
    buf: Bytes,
    flags: u8,
}

impl PerlString {
    const PERL_UTF8: u8   = 0b0001;
    const ASCII: u8       = 0b0000;  // bits 1-3
    const UTF8_NON_ASCII: u8 = 0b0010;
    const INVALID_UTF8: u8 = 0b0100;
    const UTF8_MAYBE_ASCII: u8 = 0b0110;
    const NON_ASCII_UNKNOWN: u8 = 0b1000;
    const UNKNOWN: u8     = 0b1110;
    const CACHE_MASK: u8  = 0b1110;

    /// Zero-cost &str view when Rust UTF-8 validity is known.
    /// Triggers a validation scan if needed.
    fn as_str(&mut self) -> Option<&str> {
        match self.flags & Self::CACHE_MASK {
            Self::ASCII | Self::UTF8_NON_ASCII | Self::UTF8_MAYBE_ASCII => {
                // Known valid UTF-8 (all three states).
                Some(unsafe { std::str::from_utf8_unchecked(&self.buf) })
            }
            Self::INVALID_UTF8 => None,
            _ => {
                // UNKNOWN or NON_ASCII_UNKNOWN — need UTF-8 scan.
                match std::str::from_utf8(&self.buf) {
                    Ok(s) => {
                        let cache = if s.is_ascii() { Self::ASCII } else { Self::UTF8_NON_ASCII };
                        self.flags = (self.flags & Self::PERL_UTF8) | cache;
                        Some(unsafe { std::str::from_utf8_unchecked(&self.buf) })
                    }
                    Err(_) => {
                        self.flags = (self.flags & Self::PERL_UTF8) | Self::INVALID_UTF8;
                        None
                    }
                }
            }
        }
    }

    /// Check whether the string is pure 7-bit ASCII.
    /// Triggers an ASCII scan if needed.
    fn is_ascii(&mut self) -> bool {
        match self.flags & Self::CACHE_MASK {
            Self::ASCII => true,
            Self::UTF8_NON_ASCII | Self::INVALID_UTF8 | Self::NON_ASCII_UNKNOWN => false,
            Self::UTF8_MAYBE_ASCII => {
                // Valid UTF-8 but haven't checked ASCII.
                if self.buf.iter().all(|&b| b < 128) {
                    self.flags = (self.flags & Self::PERL_UTF8) | Self::ASCII;
                    true
                } else {
                    self.flags = (self.flags & Self::PERL_UTF8) | Self::UTF8_NON_ASCII;
                    false
                }
            }
            _ => {
                // UNKNOWN — full scan.
                if self.buf.iter().all(|&b| b < 128) {
                    self.flags = (self.flags & Self::PERL_UTF8) | Self::ASCII;
                    true
                } else {
                    self.flags = (self.flags & Self::PERL_UTF8) | Self::NON_ASCII_UNKNOWN;
                    false
                }
            }
        }
    }

    /// Zero-copy construction from a Rust String.
    fn from_string(s: String) -> PerlString {
        PerlString {
            buf: Bytes::from(s.into_bytes()),
            flags: Self::UTF8_MAYBE_ASCII,  // valid from type, ASCII unknown
        }
    }

    /// Construction from a Rust &str — known valid UTF-8, ASCII deferred.
    fn from_str(s: &str) -> PerlString {
        PerlString { buf: Bytes::from(s.as_bytes().to_vec()), flags: Self::UTF8_MAYBE_ASCII }
    }

    /// Always-available byte view (zero-cost — Bytes derefs to &[u8]).
    fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Clone for COW — cheap refcount bump when shared.
    fn clone_cow(&self) -> PerlString {
        PerlString { buf: self.buf.clone(), flags: self.flags }
    }
}
```

For code that wants real Rust `String` or `Bytes` values directly — with
their full performance characteristics and zero-cost FFI — see §14
(Typed Values in Modern Mode).

### 2.4 Reference Counting and Cycle Collection

`Arc` provides atomic reference counting.  When the last `Arc` to a
value is dropped, the value is freed and `DESTROY` runs (if present).
This gives deterministic destruction in the common case — exactly
what Perl programmers expect.

The problem is cycles.  A Perl hash that holds a reference to an
object that holds a reference back to the hash creates a cycle.
Neither `Arc` is ever the last to drop, so neither value is freed.
Perl 5 has the same problem (it leaks cycles unless `weaken` is
used).

This implementation does better: **reference counting + backup cycle
collector**, matching CPython's approach.  The cycle collector uses the
Bacon–Rajan trial-deletion algorithm:

1. **Candidate identification.**  When an `Arc`'s strong count
   decreases but doesn't reach zero, the value is added to a
   candidate set.  Values whose refcount never exceeds 1 are never
   candidates.
2. **Trial deletion.**  The collector tentatively decrements refcounts
   along references from each candidate.  If a candidate's count
   reaches zero through trial deletion, it is part of a cycle.
3. **Collection.**  Cyclic garbage is freed (DESTROY called in
   topological order where possible).

The candidate set is a concurrent data structure (lock-free queue or
concurrent `HashSet`) that the collector scans periodically or on
demand.  Most values never enter the candidate set — only values
involved in reference graphs where refcounts go above 1 and then
decrease.

This gives:

- Deterministic DESTROY for non-cyclic values (immediately on last
  drop — the common case).
- Correct collection of cyclic garbage (periodically — the rare
  case).
- Perl-compatible `weaken` for explicit cycle breaking where the
  programmer wants control.

### 2.5 Magic (Tied Variables and Friends)

Perl "magic" is a mechanism for attaching callbacks to variable access.
It implements `tie`, special variables (`$!`, `$/`, `$_`, etc.), `pos()`,
`taint`, and other behaviors.

Model this as an optional chain of trait objects attached to a scalar:

```rust
trait Magic: Send + Sync {
    fn mg_get(&self, interp: &mut Interpreter, sv: &Sv) -> Result<()>;
    fn mg_set(&self, interp: &mut Interpreter, sv: &Sv) -> Result<()>;
    fn mg_clear(&self, interp: &mut Interpreter, sv: &Sv) -> Result<()>;
    fn mg_free(&self, interp: &mut Interpreter, sv: &Sv) -> Result<()>;
    fn mg_type(&self) -> MagicType;
}
```

The `Send + Sync` bound is required: magic callbacks are code
references on the shared heap and may execute on any task (§13.2).

Tied variables specifically dispatch to Perl-level `FETCH`/`STORE`/etc.
methods through a `TieMagic` implementation.  Per the cardinal
invariant (§13.11), magic callbacks never execute while any internal
lock is held.

---

## 3. Memory Management Details

### 3.1 Shared Runtime State

With `Arc<RwLock<T>>` values (§2.2), there is no centralized "heap"
that owns all values.  Values are self-contained — each `Arc` manages
its own lifetime through atomic reference counting.  There is no
per-interpreter data heap.

The shared runtime holds only coordination structures:

```rust
struct SharedRuntime {
    symbol_tables: RwLock<SymbolTableSet>,  // package stashes
    module_registry: RwLock<ModuleRegistry>,  // loaded module tracking
    cycle_candidates: ConcurrentQueue<Weak<dyn Any>>,  // for cycle collector
    globals: Globals,                        // $/, $\, $", etc.
}
```

Magic-bearing values (tied, overloaded, with `DESTROY`) use the same
`Arc<RwLock<T>>` representation as any other value.  Magic is
metadata on the value — "when accessed, call this code ref."  The
code ref is compiled IR plus captured values, all reference-counted.
Whatever task accesses the value runs the magic callback on its own
execution context.

Each interpreter task owns only execution context, not data:

```rust
struct Interpreter {
    runtime: Arc<SharedRuntime>,    // shared across all interpreters
    call_stack: Vec<CallFrame>,     // per-task execution context
    mortal_stack: Vec<Value>,       // per-task temporaries
    special_vars: SpecialVars,      // $@, $_, $/, $!, etc.
    compiler: Compiler,             // per-task (for eval STRING)
    // Dynamic scope (local) uses per-variable task-local storage — see §3.3
}
```

The `SpecialVars` struct holds variables that reflect execution state
(`$@`, `$_`, `$/`, `$\`, `$"`) or OS thread state (`$!` / errno,
`$$` / PID).  These are the only truly per-interpreter values.

### 3.2 Temporary Values and the Mortal Stack

Perl has a concept of "mortal" SVs — temporaries whose refcount is
decremented at the end of the current statement or scope.  This is
essential for expression evaluation.

With `Arc`-based values, "mortal" means the interpreter holds a
temporary `Arc` clone that is dropped at scope exit.  The mortal
stack is a per-task `Vec<Value>` — at scope exit, each entry is
dropped, decrementing its `Arc` refcount.  If the refcount reaches
zero, the value is freed.  This mirrors `SAVETMPS` / `FREETMPS`
from Perl 5.

### 3.3 Dynamic Scope (`local`) via Task-Local Storage

`local` creates dynamically scoped bindings — "for the duration of
this scope and everything called from it, this global variable has
this value."  Since each interpreter is a Tokio task (not necessarily
an OS thread), each task's dynamic scope is independent: task A's
`local $/` does not affect task B's view of `$/`, even if both tasks
happen to be running on the same OS thread.

The implementation uses **per-variable task-local save stacks**
rather than a single per-interpreter save stack:

```rust
enum SavedState {
    WasInactive,           // task-local was None before this local
    WasActive(Value),      // task-local had this value before this local
}

struct LocalStack {
    current: Value,            // current local value — fast read
    saved: Vec<SavedState>,    // previous task-local states for nested local
}

// Task-local, one per localizable global
// (uses tokio::task_local! on Tokio tasks, thread_local! on raw OS threads):
tokio::task_local! {
    static LOCAL_RECORD_SEP: Cell<Option<Box<LocalStack>>>;
    static LOCAL_OUTPUT_SEP: Cell<Option<Box<LocalStack>>>;
    // ... etc. for each special variable that supports local
}
```

The storage backend is selected at interpreter creation time based
on how the interpreter was spawned — `tokio::task_local!` for Tokio
tasks (the default), `thread_local!` for raw OS threads (§13.9).
The `local` mechanism doesn't depend on the backend; it only needs a
per-execution-context cell.

`Option<Box<LocalStack>>` is exactly as fast to check as a `bool`
flag — Rust guarantees `Option<Box<T>>` is pointer-sized via niche
optimization, and `None` is a null pointer.  The check compiles to a
single `test`/`jz` instruction.

#### 3.3.1 Reading a localizable global (hot path):

```rust
fn get_record_sep(&self) -> Value {
    LOCAL_RECORD_SEP.with(|cell| {
        match cell.get() {
            Some(ref stack) => stack.current.clone(),     // task-local hit
            None => self.heap.globals.record_sep.load(),  // shared global
        }
    })
}
```

Two operations: null pointer check, then either read `current` from
the task-local cell or load the shared global.

#### 3.3.2 `local $/ = undef` lifecycle:

The save stack records the previous task-local state, not the global
value.  When the task-local cell returns to `None`, reads fall through
to the shared global — whatever it currently holds at that moment.

```text
Initial:   task-local = None

local $/ = undef;
  task-local was None → allocate Box<LocalStack>
  saved = [WasInactive], current = undef
  task-local = Some(box)

  nested: local $/ = ",";
    push WasActive(undef) onto saved
    saved = [WasInactive, WasActive(undef)], current = ","

  inner scope exit:
    pop WasActive(undef) → current = undef

outer scope exit:
  pop WasInactive → task-local = None, Box deallocated
  reads now fall through to the shared global
```

When the stack empties, the `Box` is deallocated and the task-local
cell returns to `None`.  There is zero persistent overhead for globals
that are not currently `local`-ized.

**User package globals** (`local $Foo::bar`) allocate their task-local
cell lazily — the first `local` on a given package global creates it.
Most package globals are never `local`-ized.

#### 3.3.3 Multi-task behavior — concurrent mutation is visible:

The save stack never caches the shared global value, only previous
task-local states.  This means if another task mutates the shared
global while this task has `local` active, the mutation becomes
visible when the `local` goes out of scope:

```text
Shared global: $/ = "\n"

Task A:                           Task B:
  reads $/ → "\n" (shared)
  local $/ = undef;
  task-local: Some(current=undef,
                   saved=[WasInactive])
  reads $/ → undef (task-local)
                                   $/ = ",";     # mutates shared global via RwLock
  reads $/ → undef (task-local)   # A unaffected — still seeing task-local
  scope exit → task-local = None
  reads $/ → ","                   # falls through to shared global
                                   # sees B's mutation — correct behavior
```

This is the correct semantics: `local` creates a task-local shadow,
and when the shadow is removed, the current global state — including
any concurrent mutations — becomes visible.  There is no stale cached
copy of the old global value sitting in a save stack.

Task A's `local` only affects task A's cells.  Task B sees the shared
global throughout.  No locking is needed because each task only writes
to its own task-local cells.  Direct writes to the shared global
(`$/ = ","` without `local`) use the per-value `RwLock` from §13.5.

---

## 4. The Compile-Time Execution Problem

This is the architectural constraint that most Perl implementation attempts
underestimate.

### 4.1 Why You Cannot Separate Compilation from Execution

In Perl, the compiler and runtime are co-resident and interleaved.  This
is not a convenience — it is a hard language requirement:

```perl
use constant PI => 3.14159;   # BEGIN block runs, defines PI as a sub
my $area = PI * $r ** 2;      # parser must know PI is a constant sub

BEGIN { *frobnicate = sub { ... } }  # defines sub at compile time
frobnicate(42);                       # parser must see this as a sub call

use Moose;                     # runs import(), which installs 'has', 'extends',
has 'name' => (is => 'ro');    # etc. as keywords the parser then recognizes
```

`BEGIN` blocks, `use` statements, and `require` at compile time all run
arbitrary Perl code that can modify the symbol table, which in turn
changes how subsequent source text is parsed.  This means:

**The compiler must be able to invoke the runtime at any point during
compilation, and the runtime's side effects feed back into compilation.**

### 4.2 Architectural Consequence

The compilation pipeline is not a clean linear sequence.  It is a loop:

```text
     ┌──────────────────────────────────────┐
     │                                      │
     ▼                                      │
  Source ──► Lex ──► Parse ──► Compile ──► Execute (BEGIN/use)
     ▲                                      │
     │          symbol table mutations      │
     └──────────────────────────────────────┘
```

The implementation must support:

1. Compiling a chunk of source to an executable form.
2. Executing that form immediately (for `BEGIN`, `use`, `eval ""`).
3. Returning to compilation of the surrounding source with updated state.

This means the compiler, parser, lexer, and runtime must all be
**re-entrant**.  The lexer in particular must support context stacking (it
is already going to need this for sublexing, so this is a natural
extension).

### 4.3 Implementation Strategy

Model the compilation unit as a `CompilationContext` that carries:

- the lexer state (including its context stack)
- parser state
- the current pad (lexical scope) being compiled
- a reference to the `Interpreter`, which can execute compiled code

When a `BEGIN` block or `use` statement is encountered:

1. The parser recognizes the construct.
2. The current compilation state is suspended (pushed onto a stack).
3. The `BEGIN` block's code is compiled to IR.
4. The IR is immediately executed via the interpreter.
5. Compilation state is resumed.

Because the executed code can modify the symbol table (install subs,
declare constants, set prototypes), the parser and lexer must re-query
the symbol table for every new identifier lookup.  This is already how
`toke.c` works — it calls `gv_fetchpv` and checks for existing CVs and
GVs constantly.

### 4.4 `eval STRING`

`eval STRING` is the most extreme case: it invokes the full compiler at
runtime.  This means the entire compilation pipeline must be available as
a callable runtime operation, not just a startup phase.

Do not architect the system with a "compile phase" followed by a "run
phase."  The compiler is a runtime service.

---

## 5. Lexer Architecture

### 5.1 The Central Difficulty

Perl is traditionally considered impossible to lex without parser
feedback.  The same character sequences have different meanings
depending on context:

| Situation | How it parses | Why |
|-----------|---------------|-----|
| `print (1+2)*3` | `print(1+2) * 3` or `print((1+2)*3)` | depends on whether `print` is known as a named unary |
| `map { ... } @list` | block or hashref | parser position in the grammar |
| `$h{shift}` | hash subscript with bareword key | identifier after `{` in hash context |
| `sub foo { ... }` | sub declaration | `sub` keyword triggers block-opening |
| `Foo::Bar` | package name | `::` changes tokenization of preceding bareword |
| `/regex/` vs `$x / $y` | regex or division | term vs operator position |

Perl 5 solves this by coupling the lexer to the parser via shared
mutable state (`PL_expect`), so the lexer pre-decides ambiguous
tokens before the parser sees them.

The PerlOxide lexer takes a different approach: it produces
consistent, unambiguous tokens regardless of parser state.  A `/`
is always `Token::Slash`.  A `{` is always `Token::LeftBrace`.  The
**parser** resolves the semantic ambiguity based on its own
position in the grammar — `Token::Slash` in `parse_term` means
regex, in `peek_op_info` means division.  No shared disambiguation
state exists between the lexer and parser.

Both lexer and parser live in the `perl-parser` crate as
`pub(crate)` modules — the crate boundary is at the AST level.
The parser calls specialized lexer methods for context-sensitive
scanning (heredoc initiation, prototype scanning, substitution
body partitioning), but the lexer never reads parser disambiguation
state — there is no `Expect` enum or term-vs-operator flag.  The
lexer does read pragma and feature state (`utf8_mode`, `features`)
that the parser syncs to it at pragma change points, since these
affect lexical behavior (Unicode identifier acceptance, NFC
normalization, paired delimiter availability).

### 5.2 Parser-Driven Disambiguation

Perl 5's `toke.c` uses a flat `PL_expect` enum with 11 states to
communicate parser context to the lexer before each token is produced.
This is necessary because the LALR(1) parser cannot backtrack and
needs distinct token types for different syntactic roles of the same
character.

The recursive descent parser eliminates this entirely.  There is no
`Expect` enum, no shared state between the parser and lexer for
disambiguation, and no re-lexing.  Instead, the parser resolves every
ambiguous token by virtue of knowing where it is in the grammar:

**`/` (regex vs division).**  The Pratt expression parser calls
`parse_term()` when expecting a value and `peek_op_info()` when
looking for an infix operator.  `parse_term()` handles `Token::Slash`
as a regex start (calling `self.lexer.lex_body_str('/', true)` to
scan the pattern body).  `peek_op_info()` maps `Token::Slash` to
`OpInfo { prec: PREC_MUL, assoc: Left }` (division).  The parser's
own call stack determines which interpretation applies — no shared
state needed.

**`//` (defined-or vs empty regex).**  Same mechanism.  In
`parse_term()`, `Token::DefinedOr` is treated as an empty regex
(with optional flags like `//gi`).  In `peek_op_info()`, it's the
defined-or operator.  List operators that prefer defined-or stop
argument collection before `//`, matching `toke.c`'s UNIDOR behavior.

**`{` (block vs hash constructor).**  The parser always parses `{` as
a block via `parse_block()`, then calls `try_reclassify_as_hash()` to
check whether the block should be reinterpreted as a hash constructor.
This avoids the need for a `BraceDisposition` state and the
unreliable byte-level heuristics that `toke.c` uses.

**`(` (prototype vs normal).**  After `sub name`, the parser
explicitly calls `self.lexer.lex_body_str('(', true)` to scan the
prototype as a raw string, bypassing normal tokenization.  No
`Prototype` expect state needed.

**`<<` (heredoc vs shift-left).**  The parser calls
`self.lexer.lex_heredoc_after_shift_left()`, which inspects the
following bytes to determine whether this is a heredoc initiator
(returning the heredoc token) or a shift operator (returning `None`
so the parser can proceed with shift-left parsing).

**What follows `}`.**  The recursive descent call stack implicitly
encodes this.  `parse_if_stmt` calls `parse_block` and regains
control at statement level.  `parse_anon_sub` calls `parse_block`
and returns to the Pratt loop in operator position.  No explicit
`ExpectNext` state needed.

The parser communicates with the lexer through explicit method calls
rather than shared state.  The lexer exposes a rich set of methods
that the parser invokes based on its current production:

- `lex_token()` — main token production
- `lex_body_str(delim, raw)` — scan a delimited string body
- `lex_heredoc_after_shift_left()` — heredoc detection and initiation
- `lex_hash_var_after_percent()` — probe for hash variable vs bare `%`
- `lex_readline_after_lt()` — probe for `<HANDLE>` vs less-than
- `lex_filetest_after_minus()` — probe for `-f` etc. vs unary minus
- `start_subst_replacement(delim)` — set up substitution replacement
- `scan_adjacent_word_chars()` — scan flags after regex/subst closing
- `try_autoquoted_bareword_subscript()` — probe for `$hash{bareword}`
- `start_format(name, span)` — enter format body sublexing
- `set_utf8_mode(bool)` — sync UTF-8 pragma state
- `.features = ...` — sync feature flags from `Pragmas`

This approach is simpler, more testable, and eliminates entire classes
of bugs related to stale or incorrect expectation state.

### 5.3 Symbol Table Feedback

Beyond parser control flow, the lexer also needs symbol table access to
resolve:

- Whether a bareword is a known subroutine (and its prototype)
- Whether a name refers to a constant sub (to inline the value)
- Whether a name has been imported into the current package
- Whether a `CONSTANT` pragma or `use overload` is active

Prototype knowledge in particular affects not just lexical
classification but also how the parser subsequently handles the
argument list (see §6.3).

The symbol table lives on the `Parser` struct and is accessible to
the lexer through parser method calls in the sections where both
cooperate (e.g. `lex_word` produces `Token::Ident`, and the parser
then consults the symbol table to decide how to parse the arguments).

### 5.4 Source Layer (`LexerSource`)

The lexer does not read source bytes directly.  A dedicated
`LexerSource` type manages line-oriented source delivery, CRLF
normalization, heredoc body sequencing, substitution body partitioning,
and indentation stripping.  The lexer receives one line at a time and
scans bytes within it, never dealing with line boundaries, newline
encoding, or heredoc line reordering.

#### 5.4.1 Why a source layer:

Without this abstraction, the lexer must juggle byte-level position
pointers, CRLF normalization (either preprocessing via `Cow` or
per-byte checks in every accessor), heredoc redirect tables, saved
line positions for heredoc remainder restoration, indent
save/restore stacks, and terminator detection interleaved with
content scanning.  These are all line-level concerns forced into a
byte-level API, resulting in significant accidental complexity.

`LexerSource` is purpose-built for the lexer's needs — not a
general-purpose abstraction.  A future general `Source` type for
other uses (e.g. error reporting, IDE integration) could underlie
`LexerSource` as a wrapper.

#### 5.4.2 Dependencies:

`perl-parser` depends on the `bytes` crate.  `LexerSource` uses
`Bytes` for zero-copy reference-counted slices of the source buffer.
Line slicing, heredoc remainder saving, and indentation prefixes are
all `Bytes` handles into the same underlying allocation — no copying.
`perl-regex` remains dependency-free, operating on `&[u8]` and
`&str` slices; the conversion from `Bytes` to `&[u8]` is free
(`Bytes` derefs to `&[u8]`).

#### 5.4.3 `LexerLine` — the lexer's working unit:

The lexer operates on a `LexerLine` that combines line metadata with
a byte-scanning cursor.  All fields are `pub(crate)` — the lexer
freely reads and writes `pos` for cursor control, and reads `number`
and `offset` for span computation:

```rust
struct LexerLine {
    /// 1-based line number in the original source.
    number: usize,
    /// Byte offset of the start of this line in the original source.
    offset: usize,
    /// Line content without line ending.  When inside an indented
    /// heredoc, the required indentation prefix has been stripped.
    line: Bytes,
    /// Whether this line was terminated by a newline in the source.
    terminated: bool,
    /// Current scanning position within `line`.
    pos: usize,
    /// Whether the line contains only ASCII bytes (all < 0x80).
    /// Computed for free during newline scanning and used to skip
    /// UTF-8 decoding and NFC normalization on all-ASCII lines.
    /// See §5.8 for the `effective_utf8` optimization.
    ascii_only: bool,
}

impl LexerLine {
    fn peek_byte(&self) -> Option<u8>;
    fn peek_byte_at(&self, offset: usize) -> Option<u8>;
    fn remaining(&self) -> &[u8];       // borrowed view for comparisons
}
```

The typical lexer scanning pattern uses `peek_byte` for inspection
and `skip(n)` on the `Lexer` to advance, rather than a combined
`advance_byte` method.  This separates the concerns of looking at
the current byte from consuming it, and naturally supports
multi-byte UTF-8 characters where `skip(len)` advances past all
bytes of a decoded character in one step.

The lexer holds `Option<LexerLine>` as its current line.  When the
line is exhausted, the lexer sets it to `None`.  On the next token
request, the lexer's `peek_byte` method detects `None` and calls
`source.next_line()` to fetch the next line — a single funnel point
for all line loading.  This funnel also recomputes the
`effective_utf8` flag (§5.8) on every line transition.

Because `LexerLine` carries the scanning position, saving and
restoring a line (e.g. for heredoc remainder) automatically preserves
the cursor position within that line.  The lexer transparently
resumes mid-line without any special-case logic.

#### 5.4.4 `LexerSource` internal architecture:

`LexerSource` maintains a line queue (`VecDeque<LexerLine>`) and a
push-back mechanism for line reordering.  When heredoc bodies or
substitution bodies need to be served before the continuation of the
current line, the relevant lines are queued and the saved remainder
is pushed back for delivery after the body is consumed.

```rust
impl LexerSource {
    /// Get the next line.  Serves queued lines (heredoc bodies,
    /// substitution bodies) first, then reads from the source.
    /// Returns `Ok(None)` for virtual EOF (heredoc/subst body
    /// finished — the saved remainder follows on the next call).
    /// `peek_heredoc` controls whether a found terminator is
    /// consumed or merely peeked (used by `lex_body` to detect
    /// end-of-body without consuming the signal).
    fn next_line(&mut self, peek_heredoc: bool)
        -> Result<Option<LexerLine>, ParseError>;

    /// Begin processing a non-indented heredoc body.  Takes the
    /// current line (saving its remainder for later), queues body
    /// lines until the terminator is found.
    fn start_heredoc(
        &mut self,
        tag: Bytes,
        current_line: &mut Option<LexerLine>,
    ) -> Result<(), ParseError>;

    /// Begin processing an indented heredoc body (`<<~`).  Scans
    /// ahead to find the terminator, extracts its whitespace prefix
    /// as the required indentation, and strips that prefix from all
    /// body lines.
    fn start_indented_heredoc(
        &mut self,
        tag: Bytes,
        current_line: &mut Option<LexerLine>,
    ) -> Result<(), ParseError>;

    /// Partition a substitution replacement body.  Scans forward
    /// from the current position in the given line to find the
    /// closing delimiter (tracking nesting depth for paired
    /// delimiters), queues the body content as lines with a
    /// virtual EOF, extracts flags, and saves the remainder.
    fn start_subst_body(
        &mut self,
        delim: char,
        extra_paired: bool,
        current_line: &mut Option<LexerLine>,
    ) -> Result<Option<String>, ParseError>;

    /// Override line numbering for `# line` directives.
    fn set_line_number(&mut self, n: usize);
    fn set_filename(&mut self, name: String);
}
```

#### 5.4.5 Heredoc line sequencing:

When `start_heredoc` or `start_indented_heredoc` is called:

1. The current `LexerLine` (with its cursor position) is taken from
   the lexer's `Option<LexerLine>` and saved internally.
2. Subsequent source lines are read and queued as body lines until
   the terminator is found.
3. The terminator line itself is consumed (not delivered to the lexer).
4. `next_line()` serves the queued body lines, then returns
   `Ok(None)` to signal "body finished."
5. On the following call, `next_line()` returns the saved remainder
   line — the continuation of the original line after `<<TAG`.

For multiple heredocs on one line (`<<A, <<B`), this nests naturally:
A's body is consumed, the saved remainder (`, <<B);\n`) is returned,
the lexer encounters `<<B` and calls `start_heredoc` again, which
saves the new remainder (`);\n`) and queues B's body.  No special
stacking logic in the lexer.

#### 5.4.6 Substitution body partitioning:

`s/pattern/replacement/flags` requires scanning ahead to find the
replacement body's closing delimiter and extract flags before the
lexer processes the replacement content.  `start_subst_body` handles
this:

1. Scans the current line (and subsequent lines if needed) for the
   closing delimiter, tracking nesting depth for paired delimiters
   like `s{pattern}{replacement}`.
2. Partitions the line: body content is queued with a virtual EOF,
   flags are captured, and the remainder after the flags is saved.
3. The lexer processes the body lines normally, hitting the virtual
   EOF when the body is exhausted.  The saved remainder is delivered
   on the next `next_line()` call.

This is the same queue-and-save mechanism used for heredocs, extended
to handle the delimited replacement body.

#### 5.4.7 CRLF normalization and line termination:

`LexerLine` is deliberately designed to exclude the line ending
(LF or CRLF) from the `line` byte slice.  Instead, the `terminated`
boolean records whether the line was followed by a newline in the
source.  This design enables zero-copy slicing even when CRLF
normalization is needed — the `line` field is simply a `Bytes` slice
whose end boundary is set before the `\r` (for CRLF) or before the
`\n` (for LF), with no data copied or modified.  Standalone `\r`
not followed by `\n` is a literal byte in the content — not a line
ending.  This matches Perl's behavior.  No information is lost: the
`terminated` flag preserves whether the final line ended with a
newline, and the lexer synthesizes a virtual `\n` byte at the end
of terminated lines via `peek_byte`.

#### 5.4.8 `# line` directive processing:

The lexer recognizes `# line N "file"` directives (when `#` is at
column 0) via `try_line_directive()`, updating `LexerSource`'s line
number and optional filename override.  These affect `__LINE__` and
`__FILE__` resolution and diagnostic messages.  The directive is
consumed as a comment — no token is emitted.

Perl processes `# line` directives even in some surprising contexts,
such as between a sigil and the identifier it prefixes (e.g.
`$\n# line 100\nname` changes the line number while still resolving
`$name`).  The lexer matches this behavior because whitespace
skipping between sigil and identifier crosses line boundaries, and
line boundary crossing triggers `next_line()`, which processes the
directive.

#### 5.4.9 How this simplifies the lexer:

The lexer's token-production loop never manages heredoc line ordering,
CRLF normalization, indent stripping, substitution body partitioning,
or position save/restore.  These are fully encapsulated in
`LexerSource`.  The lexer simply calls `peek_byte`, which auto-loads
lines as needed through `next_line()`.

#### 5.4.10 Zero-copy flow from source to runtime:

Because both the parser and the runtime use `Bytes`, literal values
flow from source to runtime without copying.  A string literal like
`'hello world'` exists once in the source buffer.  The lexer
produces a `Bytes` slice via `line.slice_since(start)`, which flows
through the token and AST as a refcount bump at each stage, and
ultimately initializes the runtime `PerlString` with the same
underlying allocation.  This applies to all literals that need no
escape processing: single-quoted strings, `qw()` words,
`ConstSegment` parts of interpolated strings, heredoc body segments,
and regex literal fragments.

The same `Bytes` sharing works in reverse for `eval STRING` — the
string being evaluated is already a `Bytes` (or trivially freezable
to one), so `LexerSource` can lex directly from it with zero-copy
line slicing.  No need to copy the string into a separate source
buffer.

### 5.5 Sublexing and the Context Stack

Sublexing is the core architectural requirement.  The implementation
uses an explicit context stack (`Vec<LexContext>`) on the `Lexer`
struct.  This stack tracks the lexer's current mode within the line
it is scanning — it does not manage source positioning or line
sequencing (those are `LexerSource` concerns; see §5.4).

Unlike the enum-based `LexMode` design considered early on,
`LexContext` is a single struct with boolean flags that control
behavior.  The combination of `interpolating`, `raw`, and `regex`
covers all scanning modes:

```rust
struct LexContext {
    /// Opening delimiter character.  `None` for heredocs (end
    /// signaled by LexerSource returning `None` from `next_line`).
    delim: Option<char>,
    /// Delimiter nesting depth (for paired delimiters like `{}`).
    depth: u32,
    /// Brace depth inside `${expr}` or `@{expr}`.  When > 0,
    /// the lexer produces normal code tokens.  When 0, it
    /// produces string body tokens via `lex_body`.
    expr_depth: u32,
    /// Whether `$`/`@` trigger interpolation.
    interpolating: bool,
    /// Whether escapes pass through raw (regex, tr, prototypes).
    raw: bool,
    /// Whether to detect `(?{...})` code blocks (regex mode).
    regex: bool,
    /// Inside a subscript chain (e.g. `"$h->{k}[0]"`).
    chain_active: bool,
    /// Bracket/brace nesting inside the chain.
    chain_depth: u32,
    /// Chain end detected; emit `InterpChainEnd` on next call.
    chain_end_pending: bool,
}
```

The behavior for each quote-like construct is determined by the flag
combination passed to `LexContext::new(delim, interpolating, raw,
regex)`:

| Construct | `interpolating` | `raw` | `regex` | `delim` |
|-----------|-----------------|-------|---------|---------|
| `"..."`, `qq//` | true | false | false | `Some('"')` or delimiter |
| `'...'`, `q//` | false | false | false | `Some('\'')` or delimiter |
| `` `...` ``, `qx//` | true | false | false | `` Some('`') `` or delimiter |
| `m//`, `qr//` | `delim != '\''` | true | true | `Some(delim)` |
| `s///` pattern | `delim != '\''` | true | true | `Some(delim)` |
| `s///` replacement (no `/e`) | true | false | false | `None` (virtual EOF) |
| `s///` replacement (`/e`) | false | true | false | `None` (virtual EOF) |
| Heredoc (interpolating) | true | false | false | `None` |
| Heredoc (literal) | false | false | false | `None` |

Delimiter types are `char`, not `u8`, to support the Unicode paired
delimiter table (§5.8).  `delim` is `None` for heredocs and
substitution replacement bodies, where end-of-content is signaled by
`LexerSource` returning `Ok(None)` from `next_line()` rather than by
a delimiter byte in the content stream.

#### 5.5.1 Incremental sublexing:

The lexer produces sub-tokens one at a time rather than scanning the
entire body at once.  Each call to `lex_token()` re-enters `lex_body`
via the context stack, produces one token (a `ConstSegment`, an
`InterpScalar`, etc.), and returns.  The parser collects these tokens
until `SublexEnd` is emitted.  This is important for correct handling
of expression interpolation — `${expr}` and `@{expr}` increment
`expr_depth`, switching the lexer to normal code mode until the
matching `}` decrements it back to 0.

#### 5.5.2 Subscript chain tracking:

Inside interpolating strings, `$name` followed by `->`, `[`, or `{`
triggers a subscript chain: `"$h->{k}[0]"` should interpolate the
full dereference expression, not just `$h`.  The `chain_active`,
`chain_depth`, and `chain_end_pending` fields manage this — tracking
bracket nesting within the chain and detecting when no continuation
follows a closing bracket.  The chain emits normal code tokens
bracketed by `InterpScalarExprStart` / `InterpChainEnd` (or the
array equivalents).

#### 5.5.3 Token stream for quote-like constructs:

Quote-like scanning produces a stream of sub-tokens:

```text
QuoteSublexBegin(QuoteKind::Double, '"')
ConstSegment("Hello, ")
InterpScalar("name")
ConstSegment("! You have ")
InterpScalarExprStart   # triggered by ${expr}
  ... normal code tokens for the expression ...
RightBrace              # closing } decrements expr_depth to 0
ConstSegment(" messages.\n")
SublexEnd
```

Both `$name` and `${name}` (with a simple identifier) produce
`InterpScalar(name)`.  `${expr}` with operators or calls produces
`InterpScalarExprStart`, followed by normal code tokens, followed by
`RightBrace`.  `@{expr}` uses `InterpArrayExprStart`.  The parser
calls `parse_expr()` inline for expression interpolation — the same
parser instance, with proper span tracking and error reporting.

#### 5.5.4 Body scanner fast path:

The body scanner (`lex_body`) uses a `memchr`-based fast path for
bulk copying of string content.  When no case modifications are
active and no nesting depth is in play, it searches for the next
trigger byte (`$`, `@`, `\`, close delimiter, or open delimiter)
using SIMD-optimized `memchr`, then bulk-copies everything before
that trigger into the output string.  This avoids per-byte
processing for the common case of long literal string segments.

For Unicode paired delimiters, `memchr` searches for the first
UTF-8 byte of the delimiter character.  This may produce false
positives (other characters sharing the same lead byte), which are
handled safely by the byte-by-byte fallback that verifies the full
UTF-8 sequence.

### 5.6 Heredoc Handling

Heredocs are handled by the cooperation of `LexerSource` (§5.4) and
the lexer's token-production loop.  The source layer manages all line
ordering, indentation, and save/restore.  The lexer's only heredoc
awareness is recognizing `<<TAG` and framing the body as
`QuoteSublexBegin` / content tokens / `SublexEnd`.

#### 5.6.1 Basic flow:

```perl
my $x = <<END . "suffix";
body here
END
```

1. The lexer encounters `<<END` while scanning a line.  It emits
   `QuoteSublexBegin(QuoteKind::Heredoc, '\0')` (the `'\0'`
   delimiter signals "heredoc, no delimiter byte"), pushes a
   `LexContext` (with `delim: None`, `interpolating: true`) onto the
   context stack, and calls
   `source.start_heredoc(tag, &mut self.current_line)`.  The method
   takes the `LexerLine` (with cursor pointing at ` . "suffix";`)
   and sets the lexer's current line to `None` in one step.

2. The lexer's main loop calls `source.next_line()`, which returns
   the first body line.  With `interpolating: true`, the lexer
   scans it for interpolation and produces `ConstSegment`,
   `InterpScalar`, etc. — the same token stream as `"..."` and
   `qq{}`.

3. When `source.next_line()` returns `Ok(None)`, the terminator has
   been found.  The lexer pops `LexContext` from the context stack
   and emits `SublexEnd`.

4. On the next `source.next_line()` call, `LexerSource` returns the
   saved `LexerLine` — the remainder ` . "suffix";`.  Because
   `LexerLine` carries the cursor position, the lexer transparently
   resumes scanning mid-line.

#### 5.6.2 Multiple heredocs on one line:

```perl
my ($a, $b) = (<<A, <<B);
body A
A
body B
B
```

This falls out naturally from the save/restore mechanism:

1. `<<A` saves the remainder `, <<B);` and starts A's body.
2. A's terminator found → `source.next_line()` returns `None`.
   Lexer emits `SublexEnd`.
3. Next `source.next_line()` returns the saved `, <<B);`.
4. Lexer scans `, `, encounters `<<B`, saves `);\n`, starts B's body.
5. B's terminator found → `source.next_line()` returns `None`.
   Lexer emits `SublexEnd`.
6. Next `source.next_line()` returns `);`.

No special stacking logic in the lexer.  `LexerSource`'s line queue
and push-back mechanism handle all the line sequencing.

#### 5.6.3 Indented heredocs (`<<~`):

```perl
my $x = <<~END;
    Hello, $name!
    END
```

The lexer calls `source.start_indented_heredoc(tag, current_line)`.
`LexerSource` scans ahead to find the terminator line, extracts its
full whitespace prefix (`"    "` in this example), and sets that as
the required indentation.  Subsequent body lines are delivered with
the prefix stripped.  When the terminator is found, the previous
required indentation (if any) is restored.

#### 5.6.4 Heredoc tag forms:

The lexer recognizes several heredoc tag forms:

- `<<TAG` — bare identifier, interpolating.  Under `use utf8`,
  Unicode identifiers are accepted (XID_Start + XID_Continue).
  Digit-start tags (`<<0`, `<<42`) are valid, matching Perl's
  `isWORDCHAR`.  The tag must be immediately adjacent to `<<`
  with no whitespace.
- `<<"TAG"` — double-quoted, interpolating.  Whitespace between
  `<<` and the opening quote is allowed.
- `<<'TAG'` — single-quoted, literal (no interpolation).
- `` <<`TAG` `` — backtick-quoted, interpolating + executed.
- `<<\TAG` — backslash form, literal (equivalent to `<<'TAG'`).
  Under `use utf8`, Unicode identifiers are accepted.
- `<<~` prefix on any of the above — indented variant.

#### 5.6.5 Nested heredocs with indentation:

Expression interpolation in heredocs (`${...}`) increments
`expr_depth`, switching the lexer to normal code mode.  If
a nested heredoc appears inside the expression, `LexerSource`
handles the nesting correctly:

```perl
my $x = <<~OUTER;
    sum=${\(<<A + <<B)}
    1
    A
    2
    B
    OUTER
```

- `<<~OUTER` sets required indent to `"    "` (from terminator).
- Body lines are served with 4-space indent stripped.
- Inside `${\(...)}`, the lexer encounters `<<A`.  `LexerSource`
  saves the remainder and starts A's body.  A is non-indented, but
  the required indent (`"    "`) is still active — A's body lines
  `"    1\n"` have the prefix stripped, yielding `"1\n"`.  The
  terminator `"    A\n"` is also stripped to `"A"`, matching the tag.
- After A, the remainder is restored.  The lexer encounters `<<B`,
  same process.  B's body `"    2\n"` yields `"2\n"`.
- After B, the expression completes.  The lexer is back in OUTER's
  body, continuing with the next body line.

This architecture correctly handles arbitrarily deep heredoc nesting,
including the torture test case with 9+ heredocs at 4 levels of
nesting with interleaved `BEGIN` blocks, `do` blocks, and mixed
indented/non-indented heredocs.  The key property is that heredoc
bodies are consumed in source order — a `BEGIN` block inside a
deeply nested heredoc body is encountered before code on the
initiating line, matching Perl's compile-time execution order.

#### 5.6.6 Literal heredocs:

Literal heredocs (`<<'TAG'`, `<<\TAG`, `<<~'TAG'`) do not
interpolate.  The lexer collects the body into a single
`ConstSegment` token rather than producing a sub-token stream with
interpolation breaks.  `LexerSource` manages the line sequencing
identically.

#### 5.6.7 Heredoc terminator matching:

A line matches the heredoc tag only if the entire line content
(after any required indentation stripping) is an exact byte-for-byte
match with the tag.  Trailing spaces, tabs, or any other content
cause the line to be treated as body content, not a terminator.

### 5.7 Token Categories

The lexer emits tokens that reflect context-sensitive disambiguation
where the lexer can resolve it, and defers to the parser for
cases that require grammatical context (§5.2).

Core token categories:

- Identifiers (barewords, with package qualification info)
- Variables (`$`, `@`, `%`, `*` sigils, with name)
- Numeric literals (integer, float, hex, octal, binary, underscored)
- String/quote sub-tokens (as described in §5.5)
- Regex sub-tokens
- Operators (arithmetic, string, logical, bitwise, comparison, binding)
- Punctuation (delimiters, semicolons, arrows, fat comma)
- Keywords (control flow, declaration, special forms)
- Heredoc markers
- Special tokens (end of input, format lines, `__END__`/`__DATA__`)

### 5.8 Unicode Support

Under `use utf8`, Perl source is UTF-8 encoded and identifiers may
use Unicode letters.  The lexer implements full Unicode identifier
support with several optimizations.

#### 5.8.1 Identifier scanning:

`scan_ident` uses the `unicode-xid` crate for XID_Start and
XID_Continue checks on non-ASCII characters.  ASCII characters
use fast byte-range checks; UTF-8 multi-byte sequences are decoded
via `peek_utf8_char()` only when the lead byte is >= 0x80.

Bare heredoc tags, backslash heredoc tags, and the heredoc gate
condition all accept Unicode identifiers under `use utf8`, matching
Perl's `isWORDCHAR_utf8` behavior.

#### 5.8.2 NFC normalization:

Identifiers are NFC-normalized at extraction time, so `café`
(composed) and `café` (decomposed, `e` + combining acute) produce
the same identifier name.  This applies to `scan_ident` output and
to the raw source bytes in the `memchr` bulk-copy path.

Escape-produced characters (`\x{65}\x{301}`) bypass NFC
normalization — they are not source-level identifiers and should
preserve the exact characters specified.

Heredoc tags and terminator lines are both NFC-normalized, so a
composed tag matches a decomposed terminator and vice versa.  This
is a **deliberate deviation from Perl's behavior** — Perl does
byte-exact terminator matching with no normalization, meaning a
composed tag will not match a decomposed terminator even though they
represent the same text.  PerlOxide normalizes both sides because
the byte-exact behavior is a source of subtle bugs when editors or
version control systems silently renormalize Unicode text.

NFC normalization is one of several usability-motivated deviations
from strict Perl compatibility.  A PerlOxide-specific pragma (name
TBD) will be provided to disable such deviations for programs that
require strict byte-for-byte Perl compatibility.

#### 5.8.3 `effective_utf8` fast path:

Most lines in typical Perl source are pure ASCII, even under
`use utf8`.  The `effective_utf8` flag is a composite of
`utf8_mode && !current_line.ascii_only`, recomputed whenever a new
line is loaded or the UTF-8 pragma changes.  When false, all UTF-8
decoding, XID checks, and NFC normalization are skipped — the entire
line uses the fast ASCII code path.

The `ascii_only` flag on `LexerLine` is computed for free during
newline scanning (a single pass over the line bytes that's needed
anyway for line splitting).  The flag is specific to the current
line, so a non-ASCII line within an otherwise-ASCII file correctly
activates the UTF-8 code path for just that line.

Update sites for `effective_utf8` (2 total):
- `peek_byte` — the single line-loading funnel
- `set_utf8_mode()` — called by the parser when pragmas change

The parser syncs `utf8_mode` and the full `Features` bitflags to
the lexer whenever pragmas change (at `use`/`no` declarations and
at block exit when saved pragmas are restored).

#### 5.8.4 Unicode paired delimiters:

Under `use feature 'extra_paired_delimiters'` (Perl 5.36+), all
321 Unicode paired delimiter pairs from the `Bidi_Mirroring_Glyph`
property are available for quote-like operators: `q«hello»`,
`m⟨pattern⟩`, `s《from》《to》`, etc.

The delimiter table is stored as two packed `&'static str` constants
(`DELIM_OPEN` and `DELIM_CLOSE`, 997 UTF-8 bytes each) ordered with
the four ASCII pairs first.  Lookup uses SIMD-optimized `memmem`
(from the `memchr` crate) to search for the open delimiter's UTF-8
encoding in the packed table.  The close delimiter is at the same
byte offset in `DELIM_CLOSE` — this works because every open/close
pair has the same UTF-8 byte length (a consequence of UTF-8 length
being determined by codepoint range, and no pair crossing a range
boundary).

The `extra_paired_delimiters` feature flag controls how much of the
table is searched: without the feature, only the first 4 bytes
(ASCII pairs) are checked; with the feature, the full 997 bytes.
ASCII pairs always work regardless of the feature flag.

The `Features` bitflags are synced from the parser's `Pragmas` to
the lexer's `features` field at pragma change points, making them
available to `matching_delimiter()`, `is_paired()`, and all
quote-like scanning functions without requiring the full `Pragmas`
struct.

---

## 6. Parser Architecture

### 6.1 Expression Parsing Algorithm

Pratt parsing is used for expression assembly only.  Statement
lists, declarations, blocks, and other top-level grammatical forms
are handled by ordinary recursive descent that invokes `parse_expr`
where an expression sub-grammar is needed.

Expression parsing uses precedence climbing (the Pratt algorithm)
with clear naming that avoids the original paper's cryptic
terminology.

#### 6.1.1 Core components on `Parser`:

`parse_expr(min_prec)` — the main entry point for expressions.
Increments the recursion depth guard (`with_descent`), calls
`parse_term()` to get the initial left-hand side, then delegates to
`parse_expr_continuation` for the operator loop.

`parse_expr_continuation(left, min_prec)` — the Pratt operator loop.
Separated from `parse_expr` so the parser can pass a pre-consumed
left-hand side (e.g. when a consumed identifier turns out to be the
first term of an expression rather than a label or filehandle).

`parse_term` — called when the parser expects the start of an
expression.  Dispatches on the current token via a match: literals
return AST nodes directly, prefix operators recurse into
`parse_expr`, keywords like `if` and `sub` call dedicated helper
methods.  Complex arms call out to helpers (`parse_if_stmt`,
`parse_while`, `parse_sub_decl`, etc.) for readability.

`parse_operator` — called when the parser has a left-hand expression
and the next token is an infix/postfix operator that binds tightly
enough.  Dispatches on the operator token: binary operators recurse
into `parse_expr` for the right operand, postfix operators like `++`
wrap the left operand, ternary `?:` parses the middle and right
branches, `->` dispatches to method call or dereference parsing.

`peek_op_info` — inspects the lookahead token in operator position and
returns `Option<OpInfo>`.  `None` means the token is not valid in
operator position (the expression ends here).  `Some(info)` provides
the precedence and associativity for the precedence comparison.

#### 6.1.2 The expression loop:

```rust
type Precedence = u8;

fn parse_expr(
    &mut self,
    min_prec: Precedence,
) -> Result<Expr, ParseError> {
    self.with_descent(|this| {
        let left = this.parse_term()?;
        this.parse_expr_continuation(left, min_prec)
    })
}

fn parse_expr_continuation(
    &mut self,
    mut left: Expr,
    min_prec: Precedence,
) -> Result<Expr, ParseError> {
    while let Some(info) = self.peek_op_info() {
        if info.left_prec() < min_prec {
            break;
        }
        left = self.parse_operator(left, info)?;
    }
    Ok(left)
}
```

This is recursive.  Each recursive call adds one stack frame for
the right operand of an operator or a sub-expression inside a
prefix construct.  Left-associative operator chains (`$a + $b + $c`)
are handled by the loop, not by recursion — they add zero stack
depth regardless of chain length.

`parse_operator` always parses the right-hand side using
`right_prec()` from the operator's `OpInfo`, regardless of whether
the operator is built-in or provided by a plugin.  It must never
use a fixed precedence (e.g., `parse_expr(0)`) for the RHS — doing
so would silently break associativity.  This invariant ensures that
precedence and associativity are enforced uniformly across all
operators.

The split into `parse_expr` and `parse_expr_continuation` enables
the forward-only parsing pattern (§6.5): when the parser consumes
a token that might be a label, filehandle, or first term, it can
pass the already-built expression into `parse_expr_continuation`
instead of rewinding and re-parsing.

#### 6.1.3 Nesting depth control:

A `ParseDepth` counter (`u16`) is incremented on every recursive
entry via `with_descent`.  If the counter would exceed `MAX_DEPTH`
(10,000), the parser returns an error without recursing — this
prevents stack overflow on adversarial input (deeply nested
expressions, blocks within blocks).  `MAX_DEPTH` is set high enough
to exceed Perl 5's actual limits (`YYMAXDEPTH = 10,000`).

```rust
pub type ParseDepth = u16;
const MAX_DEPTH: ParseDepth = 10_000;

fn with_descent<T, F>(&mut self, f: F) -> Result<T, ParseError>
where F: FnOnce(&mut Self) -> Result<T, ParseError> {
    if self.depth + 1 >= MAX_DEPTH {
        return Err(ParseError::new("expression too deeply nested", ...));
    }
    self.depth += 1;
    let result = f(self);
    self.depth -= 1;
    result
}
```

#### 6.1.4 Precedence and associativity table:

Each infix operator has a single precedence number and an explicit
associativity.  The left and right precedence values used by the
algorithm are derived mechanically:

```rust
enum Assoc {
    Left,    // right_prec = prec + 1
    Right,   // right_prec = prec
    Non,     // right_prec = prec + 1, error if chained
}

struct OpInfo {
    prec: Precedence,
    assoc: Assoc,
}

impl OpInfo {
    fn left_prec(self) -> Precedence {
        self.prec
    }

    fn right_prec(self) -> Precedence {
        match self.assoc {
            Assoc::Left | Assoc::Non => self.prec + 1,
            Assoc::Right => self.prec,
        }
    }
}
```

Left-associative: the right operand must bind strictly tighter,
so same-precedence operators break out of recursion and are handled
by the loop (attaching to the left).

Right-associative: the right operand binds at the same level, so
same-precedence operators are consumed by recursion (attaching to
the right).

Non-associative: same mechanics as left-associative, but
`parse_operator` detects chaining by inspecting the left operand's
top-level operator node (ignoring transparent grouping such as
parentheses) — if it is the same non-associative operator, the
parser reports a syntax error (e.g., `$a == $b == $c` is rejected).

The precedence table is a match expression in `peek_op_info`,
returning `OpInfo` with one number and one keyword per operator.
Precedence values use even numbers so that `right_prec` (which adds
1 for left/non-associative) fits cleanly:

```rust
const PREC_LOW: Precedence = 0;           // statement boundary
const PREC_OR_LOW: Precedence = 2;        // or, xor
const PREC_AND_LOW: Precedence = 4;       // and
const PREC_NOT_LOW: Precedence = 6;       // not (prefix)
const PREC_LIST: Precedence = 8;          // list operators
const PREC_COMMA: Precedence = 10;        // , =>
const PREC_ASSIGN: Precedence = 12;       // = += -= etc.
const PREC_TERNARY: Precedence = 14;      // ?:
const PREC_RANGE: Precedence = 16;        // .. ...
const PREC_OR: Precedence = 18;           // || // ^^
const PREC_AND: Precedence = 20;          // &&
const PREC_BIT_OR: Precedence = 22;       // | ^ ~| ~^
const PREC_BIT_AND: Precedence = 24;      // & ~&
const PREC_EQ: Precedence = 26;           // == != eq ne <=> cmp ~~
const PREC_REL: Precedence = 28;          // < > <= >= lt gt le ge
const PREC_ISA: Precedence = 29;          // isa (feature-gated)
const PREC_NAMED_UNARY: Precedence = 30;  // named unary operators
const PREC_SHIFT: Precedence = 32;        // << >>
const PREC_ADD: Precedence = 34;          // + - .
const PREC_MUL: Precedence = 36;          // * / % x
const PREC_BINDING: Precedence = 38;      // =~ !~
const PREC_UNARY: Precedence = 40;        // ! ~ \ - + (prefix)
const PREC_POW: Precedence = 42;          // **
const PREC_INC: Precedence = 44;          // ++ -- (postfix)
const PREC_ARROW: Precedence = 46;        // ->
```

`PREC_ISA` (29, odd) sits between relational and shift, matching
Perl 5.32+'s `isa` operator placement.  `PREC_NAMED_UNARY` (30)
gives named unary operators (`defined`, `ref`, `chomp`, etc.)
their correct precedence.

Prefix operators do not have associativity — they have only a right
precedence that controls how tightly they bind to their operand.
This is a separate lookup used inside `parse_term`, not part of the
infix precedence table.

#### 6.1.5 Layer separation:

The parser operates on three cleanly separated layers:

1. **Tokenization** produces consistent, unambiguous tokens.  A `/`
   is always `Token::Slash`, a `{` is always `Token::LeftBrace`.
   The lexer does not pre-decide semantic meaning — it produces the
   same token regardless of parser context.

2. **Parser position** (term vs operator) determines which parse
   method is called and how each token is interpreted.  `parse_term`
   handles tokens at the start of an expression (so `Token::Slash`
   means regex).  `parse_operator` handles tokens after a left-hand
   expression (so `Token::Slash` means division).  `peek_op_info`
   determines whether an operator binds in the current context.

3. **Construct-specific parsing** handles the interior of each
   syntactic form.  `parse_if_stmt`, `parse_while`, `parse_sub_decl`,
   and similar helpers are plain recursive descent, called from
   `parse_term` arms.  They call `parse_expr` for sub-expressions,
   completing the mutual recursion.  Some make decisions after
   recursive calls return — for example, `try_reclassify_as_hash`
   inspects a parsed block to determine whether it should be
   reinterpreted as a hash constructor.  This post-hoc
   reclassification is impossible in Perl 5's LALR(1) grammar,
   which must decide before the production is entered.

Parser position determines which parse method is called.
A parse method does not inspect any shared lexer state — the parser's
own call stack is the context.

### 6.2 Parser–Lexer Communication

The parser communicates with the lexer exclusively through explicit
method calls (see §5.2 for the full list).  There is no shared
mutable state, no `Expect` enum, and no re-lexing.

The primary lexer method is `lex_token()`, which the parser calls
through `next_token()` (which wraps `lex_token` with one-token
lookahead buffering).  For context-sensitive scanning, the parser
calls specialized lexer methods directly:

- After seeing `<<` in `parse_term`, the parser calls
  `lex_heredoc_after_shift_left()` which either returns a heredoc
  token or `None` (indicating shift-left).
- After `sub name`, the parser calls `lex_body_str('(', true)` to
  scan a prototype as a raw string.
- After collecting the pattern body of `s///`, the parser calls
  `start_subst_replacement(delim)` to partition the replacement body.
- After closing a regex or substitution, the parser calls
  `scan_adjacent_word_chars()` to collect modifier flags.
- In hash subscript context, the parser calls
  `try_autoquoted_bareword_subscript()` to check for `$hash{word}`.

The parser also syncs pragma and feature state to the lexer at
pragma change points:

```rust
self.lexer.set_utf8_mode(self.pragmas.utf8);
self.lexer.features = self.pragmas.features;
```

This happens at `use`/`no` declarations and at block exit when
saved pragmas are restored.  The lexer reads these fields for
Unicode identifier support and paired delimiter lookup.

### 6.3 Prototype-Guided Parsing

When the parser encounters a known subroutine name, it should check the
sub's prototype (if any) to determine how to parse the argument list.
Prototypes change the parsing:

- `sub foo ($)` — expects one scalar argument
- `sub foo (&@)` — first arg is a block, rest is a list
- `sub foo ()` — takes no arguments, so `foo + 1` is `foo() + 1`
- no prototype — standard list operator parsing

This requires symbol table access from the parser, reinforcing the
co-resident compiler/runtime architecture.  The prototype data
structures exist (`SubPrototype`, `ProtoSlot` in the `symbol`
module), but full prototype-guided argument list parsing is not yet
implemented.

### 6.4 Syntax Extension API (Planned)

Perl 5 modules like `Syntax::Keyword::Try`, `Syntax::Keyword::Match`,
`Object::Pad`, `Syntax::Operator::ExistsOr`, and
`Syntax::Operator::Equ` extend the language by hooking into the lexer
and parser at well-defined extension points.  The Rust implementation
must provide equivalent (and improved) extensibility so that the same
kinds of syntax extensions are possible.

The Perl 5 C implementation provides three mechanisms:

1. **`PL_keyword_plugin`** — a global function pointer that the lexer
   calls when it encounters an unrecognized bareword.  The plugin can
   decline (letting the lexer handle it normally) or take over parsing
   and produce an op tree as a `PLUGEXPR` or `PLUGSTMT`.

2. **`PL_infix_plugin`** — a global function pointer (Perl 5.38+) that
   the lexer calls when it encounters a named or symbolic token that
   might be a custom infix operator.  The plugin registers it at a
   specific precedence level.

3. **Lexer/parser API functions** — `lex_stuff_pvn`, `lex_unstuff`,
   `lex_read_to`, `parse_fullexpr`, `parse_block`, `parse_termexpr`,
   `parse_arithexpr`, etc.  These let plugins manipulate the lexer
   buffer and recursively invoke the parser to parse sub-expressions.

The Rust implementation will provide all three, redesigned as
trait-based APIs rather than global function pointers.

#### 6.4.1 Keyword extension trait:

```rust
trait KeywordPlugin: Send + Sync {
    fn parse(
        &self,
        parser: &mut ParserContext,
    ) -> Option<PluginResult>;
}
```

#### 6.4.2 Infix operator extension trait:

```rust
trait InfixPlugin: Send + Sync {
    /// Build the AST node for `lhs OP rhs`.
    ///
    /// The core parser has already parsed both operands using the
    /// registered `OpInfo` to determine precedence and associativity.
    /// The plugin only builds the resulting AST node — it does not
    /// participate in precedence mechanics.
    fn build_op(
        &self,
        lhs: AstNode,
        rhs: AstNode,
    ) -> AstNode;
}
```

The core parser drives plugin operators identically to built-in
operators: look up the registered `OpInfo`, compute `right_prec()`
from the precedence and associativity, parse both operands, and
call the plugin's `build_op` with the results.  The plugin never
touches precedence mechanics — the Pratt contract is enforced
entirely by the core parser.

Precedence and associativity are provided at registration time, not
as a trait method — this makes operator metadata explicit at the
registration point and decouples it from parse behavior.

### 6.5 Forward-Only Parsing

The parser never backtracks.  Every ambiguity is resolved by
consuming the current token and branching on what follows.  No
checkpoints, no restore, no re-lex.

The design evolved from an initial architecture that mirrored
Perl 5's expectation state model (with improvements).  As the
recursive descent parser matured, it became clear that the shared
`Expect` state and re-lex machinery were unnecessary — the parser's
own control flow could resolve every ambiguity that the lexer had
been pre-deciding.  The expectation state was removed entirely,
yielding a cleaner architecture where the lexer produces consistent
tokens and the parser interprets them based on its grammatical
position.

**Consume-then-decide pattern.**  Every potentially ambiguous
construct follows the same principle: consume the token, look at
what follows, and branch.  When the consumed token turns out to be
the first element of an expression rather than a special syntactic
marker (label, filehandle, keyword), it is passed into
`parse_expr_continuation` as a pre-consumed initial term.

**Label vs expression statement.**  When `parse_statement` sees an
identifier, it consumes it.  If the next token is `:`, it's a label
— parse the labeled statement.  If not, it's the start of an
expression — wrap it as an `Expr` and pass it to
`parse_expr_continuation`.

**Fat comma autoquoting.**  Keywords consumed in `parse_term` check
whether the next token is `FatComma`.  If so, the keyword is
autoquoted as a string literal.  No speculative peek needed.

**C-style vs list-style `for`.**  After consuming `(` inside `for`,
the parser parses the first expression.  If it is followed by `;`,
this is C-style `for` — the expression becomes the initializer.  If
followed by `)` or `,`, it is list-style — the expression is the
first element of the iteration list.  The already-parsed expression
is kept in both cases.

**Print filehandle detection.**  After `print` or `say`, the parser
consumes the first token.  If it is a bareword followed by something
that looks like a term (not `,` or `)` or `;`), it is treated as a
filehandle.  If it is a scalar variable followed by a clear
term-start, same treatment.  Otherwise, the consumed token is the
first argument — passed into expression parsing as the initial term.

**Named vs anonymous `sub`.**  After consuming `sub`, the parser
checks the next token.  If it is an identifier, this is a named sub
declaration.  If it is `{`, `(`, or `:`, it is an anonymous sub.
Branch directly.

**Heredoc vs shift-left.**  The parser calls
`lex_heredoc_after_shift_left()` on the lexer, which inspects the
bytes after `<<` and either returns a heredoc token (consuming the
tag) or `None` (the parser proceeds with shift-left).

### 6.6 Brace Disambiguation

**The problem.** When the parser encounters `{` at statement level,
it could be a block (`{ print 1; print 2 }`) or an anonymous hash
constructor (`{ key => "value" }`).  In Perl 5, the LALR(1) parser
requires a distinct token type before it can select a grammar
production, so `toke.c` runs a ~200-line byte-level heuristic
(`yyl_leftcurly`, lines 6304–6507 in v5.42.2) that scans ahead through the raw
source, manually skipping whitespace, comments, string literals,
q-quotes with paired/unpaired delimiters, and barewords.  It then
checks whether the first term is followed by `,` or `=>` to guess
hash vs block.  This heuristic is fragile, cannot see past the
current line reliably, and disagrees with programmer intent in edge
cases (e.g. `{@pairs}` at statement level is classified as a block
even when a hash constructor was intended).

**The implementation: parse as block, reclassify.**  The recursive
descent parser is not constrained by LALR(1) token-driven dispatch.
The key insight is that hash constructor content is always valid as
block content — a block containing a single expression statement.
The converse is not true: multi-statement or keyword-bearing content
cannot be a hash.  This asymmetry enables a simple strategy:

1. The lexer always emits `LeftBrace` for `{`.  There is no
   `HashBrace` token — the lexer never pre-decides brace meaning.

2. In term position (`$x = {`, `foo({`, after `,`, after `=>`),
   `parse_term` sees `LeftBrace` and calls hash constructor parsing
   directly.  No heuristic needed — context is unambiguous.

3. In block position (`if (...) {`, `sub name {`, `eval {`),
   `parse_block` is called directly.  Also unambiguous.

4. At statement level — the only ambiguous case — `parse_statement`
   always calls `parse_block`.  After the block is parsed, the
   result is inspected by `try_reclassify_as_hash`:

   - Empty block → empty hash (`{}`).
   - Multiple statements → block.
   - Any statement terminated by a semicolon → block.
   - Any non-expression statement (`my`, `if`, `for`, labels, etc.)
     → block.
   - A single unterminated expression statement → candidate for
     reclassification.

5. Reclassification of single-expression blocks uses AST-level
   heuristics, which have strictly more information than Perl's
   byte-level scan:

   - Expression contains a top-level `=>` (fat comma) → hash.
   - Expression ends with fat comma or `}` follows fat comma → hash.
   - Otherwise → block.

   These rules replicate Perl's behavior for all common cases.

6. `parse_expr_statement` tracks whether a semicolon was consumed,
   since this is the primary signal distinguishing block from hash
   content.  The terminated/unterminated distinction is recorded and
   available to the reclassification logic.

**The `+{...}` idiom.**  Perl programmers force hash constructor
interpretation with `+{ ... }`, where the unary `+` is a no-op that
places `{` in term position.  This works automatically: `+` is a
prefix operator, `parse_term` is called for its operand, sees
`LeftBrace`, and parses as a hash constructor.  No special case
needed.

**Bug-for-bug compatibility.**  The reclassification heuristic is
tuned to match Perl's behavior, not to be theoretically optimal.
Cases like `{@pairs}` at statement level will be classified as a
block, matching Perl, even though a human might intend a hash
constructor.  The `+{@pairs}` workaround applies identically.

---

## 7. AST Design

### 7.1 Syntax-Oriented, Not Execution-Oriented

The AST should preserve syntactic distinctions that matter for diagnostics,
lowering, and future tooling (linters, formatters):

- `for` vs `foreach`
- `unless` / `until` vs negated `if` / `while`
- `unless ($x)` vs `if (!$x)`
- postfix `if`/`unless`/`while`/`until`/`for`/`foreach`
- `q//` vs `'...'` vs `qq//` vs `"..."`
- Heredoc style and tag
- `->` method call vs function call
- `my` vs `our` vs `local` vs `state`

### 7.2 Key AST Node Families

```text
Program
  Statement*

Statement = ExprStatement | Declaration | SubDecl | PackageDecl
          | UseDecl | Block | If | Unless | While | Until
          | For | ForEach | Loop | LabeledStmt | ...

Expr = Literal | Variable | BinOp | UnaryOp | Assign
     | FuncCall | MethodCall | ArrayRef | HashRef
     | ArraySlice | HashSlice | Deref | Regex | Subst
     | Transliterate | QW | InterpolatedString
     | HeredocString | Ternary | Range | Do | Eval
     | AnonymousSub | ...

Declaration = My | Our | Local | State

Variable = ScalarVar | ArrayVar | HashVar | GlobVar
         | ArrayElem | HashElem | SpecialVar
```

Each node carries a `Span` for source location.

### 7.3 Interpolation Representation

An interpolated string `"Hello, $name!"` is represented as:

```text
InterpolatedString [
    ConstSegment("Hello, "),
    ScalarInterp(name),
    ConstSegment("!"),
]
```

A format string `f"Total: {count * 2}"` produces:

```text
FormatString [
    ConstSegment("Total: "),
    ExprInterp(BinOp(Mul, Var(count), Literal(2))),
]
```

Expression interpolation in regular strings `"Answer: ${6 * 7}"` also
produces an `ExprInterp` node.

This preserves the structure from the lexer's sub-token stream and makes
lowering straightforward (it becomes a series of concatenations and
stringifications).

---

## 8. Semantic Lowering and HIR

### 8.1 Purpose

The lowering pass transforms the syntax-oriented AST into a
semantics-oriented High-level IR (HIR) where implicit Perl behaviors
become explicit.  The HIR encodes Perl's actual semantics rather
than its surface syntax.

### 8.2 Key Lowering Transformations

| Syntax | Lowered Form |
|--------|-------------|
| Bare `/.../` in expression | `$_ =~ /.../` |
| Postfix `... if COND` | `if (COND) { ... }` |
| `print LIST` | `print(STDOUT, LIST)` |
| `for (1..10)` | range + iterator |
| `foreach $x (@arr)` | aliasing loop over array |
| `chomp $x` | `$x = chomp($x)` (modifies in place) |
| `chop`, `chomp` without args | operate on `$_` |
| Diamond `<HANDLE>` | `readline(HANDLE)` |
| Implicit `$_` | all implicit `$_` uses made explicit |
| Context propagation | scalar/list/void context annotated |
| String interpolation | concatenation chain |
| `qw//` | literal list |

### 8.3 Mode-Dependent Lowering

This is where the compatibility/modern mode split takes effect.  In
"modern" mode, the lowering pass can:

- Reject or warn on `local` for variables that should be lexical.
- Enforce `strict`-like semantics by default.
- Lower `tie` operations to a restricted, thread-safe variant.
- Treat shared state differently for concurrency.

The key insight is that the **syntax remains the same** across modes; the
**semantic interpretation** diverges.  This is much cleaner than forking
the parser.

---

## 9. Executable IR

### 9.1 Design Goals

The IR is the execution substrate — the common representation consumed by
the interpreter, optimizer, and future AOT compiler.

It should be:

- Lower-level than the AST/HIR (no complex expressions; everything is
  broken into simple operations)
- Explicit about context (scalar/list/void)
- Explicit about variable access (lexical pad vs. global symbol table)
- SSA-like for locals where practical, to enable optimization
- Serializable, to support ahead-of-time compilation

### 9.2 IR Structure

A function/block compiles to a sequence of basic blocks.  Each basic block
contains a sequence of IR operations and ends with a terminator.

```rust
struct IrFunction {
    blocks: Vec<BasicBlock>,
    locals: Vec<LocalInfo>,   // pad layout
    constants: Vec<Value>,
}

struct BasicBlock {
    ops: Vec<IrOp>,
    terminator: Terminator,
}

enum Terminator {
    Goto(BlockId),
    Branch { cond: LocalId, then_: BlockId, else_: BlockId },
    Return(Option<LocalId>),
    Eval { body: IrFunctionId, next: BlockId, catch: BlockId },
    LoopControl { kind: LoopCtl, label: Option<Label>, target: BlockId },
    Die(LocalId),
}
```

IR operations are explicit about every effect:

```rust
enum IrOp {
    // Constants
    LoadConst { dst: LocalId, value: ConstId },
    LoadUndef { dst: LocalId },

    // Variable access
    GetLexical { dst: LocalId, pad_index: usize, depth: usize },
    SetLexical { pad_index: usize, depth: usize, src: LocalId },
    GetGlobal  { dst: LocalId, package: SymbolRef, name: SymbolRef, sigil: Sigil },
    SetGlobal  { package: SymbolRef, name: SymbolRef, sigil: Sigil, src: LocalId },

    // Operations
    BinOp   { dst: LocalId, op: BinOpKind, lhs: LocalId, rhs: LocalId },
    UnaryOp { dst: LocalId, op: UnaryOpKind, operand: LocalId },
    Concat  { dst: LocalId, lhs: LocalId, rhs: LocalId },
    Stringify { dst: LocalId, src: LocalId },  // explicit coercion
    Numify    { dst: LocalId, src: LocalId },

    // Calls
    CallSub    { dst: LocalId, callee: LocalId, args: Vec<LocalId>, context: Context },
    CallMethod { dst: LocalId, invocant: LocalId, method: SymbolRef,
                 args: Vec<LocalId>, context: Context },
    CallBuiltin { dst: LocalId, builtin: BuiltinId, args: Vec<LocalId>,
                  context: Context },

    // Data structure access
    ArrayGet  { dst: LocalId, array: LocalId, index: LocalId },
    ArraySet  { array: LocalId, index: LocalId, value: LocalId },
    HashGet   { dst: LocalId, hash: LocalId, key: LocalId },
    HashSet   { hash: LocalId, key: LocalId, value: LocalId },
    Deref     { dst: LocalId, ref_: LocalId, kind: DerefKind },
    MakeRef   { dst: LocalId, target: LocalId },

    // Regex
    RegexMatch  { dst: LocalId, target: LocalId, regex: RegexId, flags: MatchFlags },
    RegexSubst  { target: LocalId, regex: RegexId, replacement: LocalId, flags: SubstFlags },

    // Dynamic scope
    SaveLocal   { target: LocalId },  // push onto save stack
    RestoreScope,                      // pop save stack to mark

    // ... IO, system calls, etc.
}
```

### 9.3 Context as IR Annotation

Every operation that returns a value carries an explicit `Context`:

```rust
enum Context {
    Void,
    Scalar,
    List,
}
```

This is not inferred at execution time.  The lowering pass propagates
context from consumers to producers and encodes it in the IR.  This
enables the interpreter to avoid allocating list results in void context,
and enables the AOT compiler to specialize.

---

## 10. Runtime Architecture

### 10.1 The Interpreter

The interpreter is a straightforward IR walker.  For each `IrOp`, it
performs the operation using the `Heap` and the current call frame's
locals.

```rust
struct CallFrame {
    function: IrFunctionId,
    ip: (BlockId, usize),     // current instruction pointer
    locals: Vec<Value>,       // pad for this frame
    context: Context,
    mortal_stack_mark: usize,
}

struct Interpreter {
    runtime: Arc<SharedRuntime>,
    call_stack: Vec<CallFrame>,
    mortal_stack: Vec<Value>,
    special_vars: SpecialVars,
    compiler: Compiler,       // always available for eval STRING
    // Dynamic scope (local) uses per-variable task-local storage — see §3.3
}
```

### 10.2 The Symbol Table

Perl's symbol table is a hierarchy of hashes (stashes).  Each entry is a
typeglob containing slots for scalar, array, hash, code, IO, and format:

```rust
struct Glob {
    scalar: Option<Sv>,
    array: Option<Av>,
    hash: Option<Hv>,
    code: Option<Arc<Code>>,
    io: Option<Arc<RwLock<IoHandle>>>,
    format: Option<Arc<Format>>,
}
```

The main stash `%main::` contains all top-level symbols.  Package
declarations create nested stashes.  `use Exporter` and `import()` copy
glob slots between stashes.

The symbol table must be efficiently queryable from both the compiler (for
bareword resolution) and the runtime (for global variable access).

### 10.3 Special Variables

Perl has dozens of special variables (`$_`, `$!`, `$/`, `$\`, `$"`, `$;`,
`$@`, `$0`, `$$`, `%ENV`, `@ISA`, `@ARGV`, `%SIG`, etc.).

These should be implemented via magic on specific global SVs, not as
special cases scattered throughout the runtime.  Each special variable
gets a magic implementation that provides the appropriate read/write
behavior (e.g., `$!` reads `errno`, `$/` controls record separator,
`$SIG{INT}` installs signal handlers).

### 10.4 Subroutine Dispatch

A `Code` value holds:

```rust
struct Code {
    kind: CodeKind,
    prototype: Option<String>,
    pad_template: Vec<LocalInfo>,  // lexical variable layout
    attributes: Vec<Attribute>,
}

enum CodeKind {
    Interpreted(IrFunctionId),
    Builtin(BuiltinFn),
    NativeExtension(extern "C" fn(...)),
    Constant(Value),           // constant subs (use constant)
    Autoloaded,                // needs AUTOLOAD dispatch
}
```

Method dispatch follows Perl's standard MRO (default DFS, or C3 with
`use mro 'c3'`).  The `@ISA` array of each package determines the
inheritance chain.  Method resolution caches can be invalidated when
`@ISA` changes.

---

## 11. The Regex Engine

### 11.1 Why a Custom Engine Is Required

The Rust `regex` crate is fast but deliberately omits features that
real-world regex usage (not just Perl) often requires:

- Backreferences (`\1`, `\k<n>`)
- Lookahead and lookbehind (though `fancy-regex` supports these)
- Recursive/reentrant patterns (`(?R)`, `(?1)`)
- Backtracking control verbs (`(*MARK:name)`, `(*FAIL)`, `(*SKIP)`, etc.)
- Embedded code blocks (`(?{ ... })` and `(??{ ... })`) — Perl-specific
- The `\G` assertion and `pos()` interaction — Perl-specific
- Atomic groups (`(?>...)`)
- Conditional patterns (`(?(cond)yes|no)`)
- `\p{...}` Unicode properties with Perl's full set

The `regex` crate's design philosophy (guaranteed linear time, no
backtracking) makes these features fundamentally impossible to add.
`fancy-regex` covers some of them but not backtracking control verbs,
recursion, or embedded code.

There is no Rust crate today that provides Perl-compatible regex with
the full feature set.  This is a gap in the Rust ecosystem, not just
a need of this project.

### 11.2 Design as a Standalone Crate

The `perl-regex` crate should be designed as a **general-purpose Rust
library** that happens to also serve the Perl implementation.  It
should be independently publishable on crates.io and useful to any
Rust program that needs regex features beyond what the `regex` crate
provides.

#### 11.2.1 Design principles:

1. **Clean Rust API first.**  The primary interface should feel native
   to Rust — `Regex::new()`, `.find()`, `.captures()`, iterators over
   matches.  The Perl interpreter integration is a thin layer on top.

2. **No Perl runtime dependency.**  The core crate must not depend on
   the Perl interpreter, value model, or heap.  Embedded code blocks
   (`(?{ ... })`) are supported through a generic callback trait, not
   a hard dependency on the Perl compiler.

3. **Feature parity with Perl regex.**  The goal is to support every
   Perl regex feature, correctly, including the obscure ones.  This
   makes it the go-to crate for any Rust project that needs PCRE-level
   functionality.

4. **UTF-8 and byte-mode support.**  The engine should operate on both
   `&str` (UTF-8) and `&[u8]` (arbitrary bytes), matching Perl's own
   dual-mode string handling.

### 11.3 Public API

The API should be familiar to users of the `regex` crate:

```rust
use perl_regex::Regex;

// Compile a pattern
let re = Regex::new(r"(\w+)\s+\1")?;  // backreference

// Simple matching
assert!(re.is_match("hello hello"));

// Captures
let caps = re.captures("hello hello world").unwrap();
assert_eq!(&caps[1], "hello");

// Find with position
let m = re.find("say hello hello").unwrap();
assert_eq!(m.start(), 4);
assert_eq!(m.as_str(), "hello hello");

// Iterator over all matches
for m in re.find_iter(text) {
    println!("{}", m.as_str());
}
```

Advanced features use the same API:

```rust
use perl_regex::Regex;

// Lookahead
let re = Regex::new(r"\w+(?=\s*=)")?;

// Recursive pattern (matching balanced parens)
let re = Regex::new(r"\((?:[^()]*|(?R))*\)")?;

// Named captures
let re = Regex::new(r"(?P<year>\d{4})-(?P<month>\d{2})-(?P<day>\d{2})")?;
let caps = re.captures("2025-03-26").unwrap();
assert_eq!(&caps["year"], "2025");

// Backtracking control
let re = Regex::new(r"(*MARK:first)\w+|(*MARK:second)\d+")?;

// Byte-mode matching
use perl_regex::bytes::Regex as BytesRegex;
let re = BytesRegex::new(r"[\x00-\x1f]")?;
assert!(re.is_match(b"\x0a\x0d"));
```

### 11.4 Embedded Code Block Hooks

Embedded code blocks (`(?{ ... })` and `(??{ ... })`) are the one
feature that requires a host language.  The crate supports this through
a generic callback trait, not a Perl dependency:

```rust
/// Trait for host languages to implement embedded code execution.
trait RegexCodeHost {
    type Value;
    type Error;

    /// Called when (?{ code }) is reached during matching.
    fn eval_embedded(
        &mut self,
        code: &str,
        captures: &CaptureState,
    ) -> Result<Self::Value, Self::Error>;

    /// Called when (??{ code }) is reached.
    /// Must return a regex pattern string to match at this position.
    fn eval_interpolated(
        &mut self,
        code: &str,
        captures: &CaptureState,
    ) -> Result<String, Self::Error>;
}
```

The Perl implementation provides a `PerlCodeHost` that compiles and
executes the embedded code via the interpreter.  But a Rust program
could provide its own implementation — for example, evaluating Lua
snippets, or running a simple expression evaluator.

Without a code host, patterns containing `(?{ ... })` return a
compile-time error explaining that embedded code requires a host.
All other features work without any host.

### 11.5 Engine Architecture

The engine is a backtracking NFA executor with the following pipeline:

```text
Pattern string
    ──► Parser (regex syntax → AST)
    ──► Analyzer (optimize, detect features needed)
    ──► Compiler (AST → bytecode)
    ──► Executor (bytecode + input → match result)
```

#### 11.5.1 Bytecode instructions:

```text
Literal(bytes), LiteralInsensitive(bytes),
CharClass(set), AnyChar, AnyByte,
Anchor(Start | End | WordBoundary | LineStart | LineEnd | ...),
Split(branch1, branch2), Jump(target),
Save(group), BackRef(group), NamedBackRef(name),
LookAhead(subprog, negated), LookBehind(subprog, negated),
AtomicGroup(subprog),
Conditional(group, yes_branch, no_branch),
EmbeddedCode(code_index), InterpolatedCode(code_index),
Mark(name), Fail, Skip, Prune, Commit,
Recurse(group), Call(subpattern),
Match
```

#### 11.5.2 Optimization opportunities:

- Literal prefix extraction for fast scan-ahead (like `memchr` before
  engaging the backtracking engine).
- Simple patterns (no backrefs, no lookaround, no control verbs) can
  optionally delegate to the `regex` crate's DFA engine for
  linear-time matching.  This is a transparent optimization — the API
  is the same regardless of which engine runs.
- Common character class simplification.
- Anchored-pattern fast paths.

### 11.6 Crate Structure

```text
perl-regex/
    src/
        lib.rs           # public API: Regex, Captures, Match, etc.
        parse.rs         # regex parser (pattern string → AST)
        ast.rs           # regex AST types
        compile.rs       # AST → bytecode compiler
        bytecode.rs      # bytecode instruction definitions
        exec.rs          # backtracking executor
        exec_dfa.rs      # optional DFA fast path (wraps regex crate)
        unicode.rs       # Unicode property tables and case folding
        bytes.rs         # byte-mode API (mirrors str-mode)
        code_host.rs     # RegexCodeHost trait
        error.rs         # compile and runtime error types
```

The crate has zero required dependencies beyond `std`.  Optional
features:

- `unicode` (default on) — full Unicode property support
- `regex-delegation` — enable DFA fast path via the `regex` crate
- `bytes-crate` — interop with the `bytes` crate for `Bytes` input
---

## 12. Module System

### 12.1 `use`, `require`, and `do`

These three mechanisms all involve loading and potentially executing
external Perl source, but with different semantics:

- `require EXPR` — locates a file via `@INC`, compiles and executes it
  once.
- `use Module LIST` — equivalent to `BEGIN { require Module; Module->import(LIST) }`.
- `do EXPR` — locates and executes a file, re-executing even if already
  loaded.

All three invoke the full compiler pipeline on the loaded source.  `use`
is compile-time (it runs in a `BEGIN` block), while `require` and `do`
are runtime (but still invoke the compiler).

### 12.2 `@INC` and Module Resolution

`@INC` is a list of directories (and optionally code refs / objects) to
search for modules.  The implementation should support:

- Directory entries (simple filesystem lookup)
- Code reference entries (call the sub with the module name)
- Object entries (call the `INC` method on the object)
- Blessed reference entries (same as objects)

This is important for compatibility with module bundlers, PAR, fatpacking,
and custom module loading schemes.

### 12.3 Pragmas

Pragmas (`strict`, `warnings`, `utf8`, `feature`, etc.) are implemented
as modules whose `import` / `unimport` methods set lexically-scoped
compiler hints.  The implementation should carry a hints hash
(`%^H` / `$^H`) in the compilation context that pragmas can modify.

These hints are lexically scoped — they take effect for the enclosing
block and are restored at block exit.

---

## 13. Concurrency Model

### 13.1 The Problem

Perl 5 is intrinsically single-threaded.  Its `ithreads` model works by
cloning the entire interpreter state for each thread — safe but
enormously expensive.  Sharing data requires `threads::shared`, which
copies values into a separate shared structure with lock-protected
access.

Most alternative implementations inherit this assumption: "Perl values
are inherently task-local."  But analyzing what is *actually* tied
to a specific interpreter reveals that most values have no reason to
be task-local.

### 13.2 What Is Actually Interpreter-Local

Analyzing what is truly tied to a specific interpreter thread reveals
that *values* are almost never the bottleneck — *execution context*
is.

#### 13.2.1 Per-execution-context (not per-value):

- **Call stack and mortal stack** — each thread needs its own call
  stack and temporary value stack.  These are execution context.
- **Dynamic scope (`local`)** — per-variable task-local save stacks
  (§3.3).  Each task has its own overlay on localized globals.
- **The compiler** — `eval STRING` needs a compiler, which has its
  own lexer/parser state.

#### 13.2.2 Per-OS-thread or per-process (not per-interpreter):

- **`$!` (errno)** — per-OS-thread in POSIX (tasks on the same OS
  thread share it, so the interpreter must save/restore errno across
  yield points).
- **`$$` (PID)**, **`$0` (process name)** — per-process.
- **`%ENV`** — per-process environment.

#### 13.2.3 Per-execution-context special variables:

- **`$@`** — current exception.
- **`$_`** — current topic.
- **`$/`, `$\`, `$"`** — IO formatting state.

#### 13.2.4 Everything else is shared — including magic:

- **Plain scalars, arrays, hashes** — just data on the shared heap.
- **Blessed objects** — just data plus a stash name.
- **Compiled regexes and IR** — read-only after compilation.
- **Code references and closures** — compiled IR (shareable) plus
  captured values (on the shared heap).
- **Tied variables** — the `tie` magic is a code reference (FETCH,
  STORE, etc.) attached as metadata on the value.  The code ref is
  compiled IR + captured values, both on the shared heap.  When the
  magic fires, it runs on whatever interpreter thread accesses the
  value.
- **Overloaded objects** — operator implementations are code
  references, same as `tie`.  Shareable.
- **`DESTROY`** — a code reference called when refcount drops to
  zero.  Runs on whatever thread drops the last reference.
- **`%SIG` handlers** — code references.  Shareable.

The key insight is that magic callbacks are code references, and code
references are data.  They don't need a *specific* interpreter — they
need *an* interpreter's execution context to run on, and any thread
that has one will do.

### 13.3 One Shared Heap

All values — including magic-bearing values — live on a single
shared heap (§3.1) with atomic reference counting.  There is no
per-interpreter data heap.  No value migration between heaps.

This means sharing data across threads is the default, not a special
operation.  Every value is `Arc`-refcounted and accessible from any
task:

```perl
# All of these are on the shared heap from the moment they're created
my @data = (1, 2, 3, 4, 5);
my %config = (host => "db.example.com", port => 5432);
tie my %cache, 'LRU::Cache', max_size => 1000;

spawn {
    # All accessible — same shared heap, atomic refcount
    print "Host: $config{host}\n";
    for my $n (@data) { process($n) }
    my $v = $cache{key};    # tie magic fires on THIS thread's interpreter
};
```

The only things created per-task are execution contexts:

```rust
struct Interpreter {
    runtime: Arc<SharedRuntime>,    // shared with all other interpreters
    call_stack: Vec<CallFrame>,     // per-task execution context
    mortal_stack: Vec<Value>,       // per-task temporaries
    special_vars: SpecialVars,      // $@, $_, $/, $!, etc.
    compiler: Compiler,             // per-task (for eval STRING)
    // Dynamic scope (local) uses per-variable task-local storage — see §3.3
}
```

Spawning a thread or async task creates a new `Interpreter` with
a fresh execution context pointing to the same `SharedRuntime`.
No data is cloned.  No serialization.

### 13.4 Closures and Threads

A closure is compiled IR (read-only, `Arc<Code>`) plus captured
values (also `Arc`-refcounted).  Since both components are shared
by construction, all closures are inherently shareable across
threads.

This is a fundamental departure from Perl 5, where a closure
captures a "lexical pad" — an AV allocated in the per-interpreter
arena, making every closure interpreter-local.

This design does not use Perl 5-style pads.  A closure's captures are
a `Vec<Value>` where each value is `Arc`-refcounted:

```perl
my $name = "worker";       # Arc-refcounted
my $count = 0;             # Arc-refcounted

spawn {
    # captures are shared heap values — closure is Send
    print "Thread $name starting, count=$count\n";
};

tie my %cache, 'LRU::Cache';

spawn {
    # %cache is on the shared heap too — tie magic is just metadata
    # FETCH runs on the spawned thread's interpreter context
    print $cache{key};     # works
};
```

**Typed values work the same way, with the additional benefit of
zero atomic-refcount overhead for stack-local values:**

```perl
let config: Arc<str> = "database.example.com";
let counter: Arc<RwLock<i64>> = 0;

spawn move || {
    let mut guard = counter.write();
    *guard += 1;
    print f"Config: {config}\n";
};
```

### 13.5 Per-Value Synchronization

Shared mutable state needs synchronization.  Rather than a global
interpreter lock (GIL) or per-heap locks, synchronization is
per-value:

- **Immutable shared values** (the common case) — no synchronization
  needed.  Multiple threads read freely via atomic refcount.
- **Mutable shared values** — protected by a per-value `RwLock`.
  Readers proceed in parallel; writers acquire exclusive access.
- **Typed values** — use explicit `Arc<RwLock<T>>` or `Arc<Mutex<T>>`
  as in Rust.

For `my` variables that are shared and need mutation, the runtime
can automatically attach a `RwLock` when a value is first accessed
from multiple threads.  This is transparent to the Perl programmer:

```perl
my @results;

# Multiple workers append to @results
for my $i (1..10) {
    spawn {
        my $result = compute($i);
        push @results, $result;      # RwLock acquired automatically
    };
}
```

The `RwLock` is only allocated when contention is detected (the
value is accessed from more than one thread).  Single-threaded
access has zero synchronization overhead.

### 13.6 Automatic Parallelism via Rayon

Because all values are on the shared heap and closures are shareable,
the preconditions for work-stealing parallelism (Rayon) are met by
the base architecture.  This enables — but does not require — automatic
parallelization of list operations like `map`, `grep`, and `sort`.
This is a future optimization, not a day-one feature, but the shared
heap architecture makes it possible without any redesign:

```perl
# Standard Perl — the runtime can parallelize this automatically
my @results = map { expensive_transform($_) } @huge_list;
my @matches = grep { complex_predicate($_) } @large_dataset;
my @sorted  = sort { heavy_comparison($a, $b) } @big_array;
```

The programmer writes exactly the code they always wrote.  The
runtime decides whether to parallelize based on:

- **Workload size** — small lists run sequentially (Rayon overhead
  isn't worth it for a 10-element array).
- **Available cores** — if only one core is free, run sequentially.
- **Safety** — the callback must be safe to call from multiple
  threads simultaneously.

#### 13.6.1 Safety analysis for parallelization:

Typed closures (`fn`, `|args|`) are the easy case.  The compiler can
prove at compile time that a typed closure touching only its arguments
and local variables has no shared mutable side effects — always safe
to parallelize.

For `sub { }` blocks, the analysis is harder.  The runtime can use a
conservative approach:

- If the block only reads `$_` (or its parameters) and calls
  pure builtins → safe to parallelize.
- If the block writes to shared variables, calls IO functions, or
  uses global state → fall back to sequential execution.
- A `use parallel` pragma could opt in to aggressive parallelization
  with the programmer asserting that the block is safe.

Even the conservative approach covers the common case — map and grep
blocks that transform data are typically pure functions of their
input.

#### 13.6.2 Beyond map/grep/sort:

The same mechanism extends to:

- `for`/`foreach` with independent iterations
- Parallel file processing (`map` over file handles)
- Concurrent HTTP requests (`map` over URLs)
- Any list-processing pipeline

```perl
# Parallel pipeline — each stage auto-parallelized
my @processed =
    map  { transform($_) }
    grep { validate($_) }
    map  { parse($_) }
    @raw_lines;
```

This is not a special "parallel mode" or a different set of
functions.  It is standard Perl `map` and `grep` running on an
implementation where the architecture makes parallelism natural.

### 13.7 Async Integration with Tokio

The implementation runs on Tokio, providing two complementary async
models that correspond naturally to the `my`/`sub` vs `let`/`fn`
split.

#### 13.7.1 Implicit async for Perl code (Go-style green threads).

Each interpreter is a lightweight execution context — call stack,
mortal stack, special variables — running as a Tokio task.  When Perl
code hits a blocking operation (IO, sleep, network, system call), the
interpreter yields to Tokio instead of blocking the OS thread.

The Perl code looks completely synchronous.  Thousands of green
threads multiplex onto a small number of OS threads:

```perl
# These look synchronous but yield to Tokio under the hood
for my $url (@urls) {
    spawn {
        my $page = get($url);        # yields while waiting for network
        my $parsed = parse($page);   # runs on CPU, doesn't yield
        push @results, $parsed;      # shared heap, RwLock as needed
    };
}

# File IO — yields during read, other tasks make progress
open my $fh, '<', $filename or die "Can't open: $!";
my $content = do { local $/; <$fh> };  # yields during read
close $fh;

# Sleep — yields, doesn't block the OS thread
sleep 5;                               # other tasks run during this
```

This works because interpreters are cheap to create (a few kilobytes
of execution context, all pointing at the same shared heap) and
because Tokio's task scheduler handles the multiplexing.  Spawning
ten thousand concurrent HTTP requests is practical — each `spawn`
block is a Tokio task, not an OS thread.

Existing Perl code gets concurrent IO without changing a line.  A
web crawler, a parallel test harness, a concurrent database loader —
written in standard Perl, running on Tokio.

#### 13.7.2 No function coloring — same code, both contexts.

The same `sub`, the same `fn`, the same module works identically
whether called from a single-threaded script or from inside a `spawn`
block.  Async is an interpreter implementation detail, not a language-
level distinction.  This applies equally to untyped `sub` and typed
`fn`:

```perl
# Neither of these knows whether it's running async or not
sub fetch_and_parse {
    my ($url) = @_;
    my $page = get($url);        # "blocks" until complete
    my $parsed = parse($page);
    return $parsed;
}

fn fetch_and_transform(url: &str) -> Result<String, Error> {
    let page = http::get(url)?;  # "blocks" until complete
    transform(&page)
}

# Both work in a single-threaded script
my $result = fetch_and_parse("http://example.com");
let $other = fetch_and_transform("http://other.com");

# Both work in a spawn block — IO yields to Tokio instead of blocking
spawn { my $result = fetch_and_parse("http://example.com") };
spawn { let $result = fetch_and_transform("http://other.com") };
```

The interpreter always runs on a Tokio runtime — even in "synchronous"
mode, it uses a single-threaded Tokio runtime (`current_thread`).
The `.await` points in the interpreter loop are always present.  In
single-threaded mode there is only one task, so yielding and resuming
is a no-op.  The code path is identical; only the scheduling context
differs.

This is fundamentally different from Python's async split, where
`async def` and `def` are different function "colors" that cannot
freely call each other.  In this model there is one color.  Every
function — `sub` or `fn` — is "potentially async" because the
interpreter handles it transparently.

#### 13.7.3 `async fn` is optional, not required.

The `async` keyword on `fn` is available for when you *want* explicit
control — composing futures, using `select!`, timing out, cancelling.
But it is never *required* for basic concurrent IO.  The sync/async
monomorphization described below applies to all functions, whether or
not they are marked `async`:

```perl
# These are semantically equivalent:
fn fetch(url: &str) -> String { http::get(url)? }
async fn fetch(url: &str) -> String { http::get(url).await? }

# The first gets sync/async variants emitted automatically
# by the AOT compiler.  The second is an explicit annotation
# saying "I want to return a future and use .await syntax."
```

`async fn` gives the programmer access to `.await`, `select!`,
`join_all`, and other future combinators.  Plain `fn` gets the same
runtime behavior through the interpreter's implicit yielding, and
the same AOT variants through monomorphization.

#### 13.7.4 Explicit async/await — when you want control.

`async fn` and `async` closures return futures that compose directly
with Tokio's ecosystem.  `await` is explicit, giving the programmer
full control over concurrency, cancellation, timeouts, and select:

```perl
async fn fetch_page(url: &str) -> Result<String, Error> {
    let response = http::get(url).await?;
    Ok(response.text().await?)
}

# Concurrent fan-out — all requests in parallel
async fn fetch_all(urls: &[String]) -> Vec<Result<String, Error>> {
    let futures = urls.iter().map(|url| fetch_page(url));
    join_all(futures).await
}

# Timeout
let result = timeout(Duration::from_secs(5), fetch_page(url)).await;

# Select — first to complete wins
let winner = select! {
    page = fetch_page(primary_url) => page,
    page = fetch_page(fallback_url) => page,
};

# Spawn a background task
let handle = spawn async move || {
    let data = fetch_page(url).await?;
    process(data)
};
let result = handle.await?;
```

Because `async fn` is a registered keyword, this works on both the
Rust runtime and on standard Perl 5 via `use Typed` (where `async`
is handled by the keyword plugin).

#### 13.7.5 Futures are lazy — `spawn` makes them eager.

Following Rust semantics, an `async fn` returns a future that does
nothing until driven.  This is essential for composability — `join_all`
and `select!` need to control when execution begins.  If you want
eager background execution, wrap it in `spawn`:

```perl
# Lazy — fetch_page returns a future, nothing happens yet
let f = fetch_page(url);            # just creates the future
let page = f.await;                  # NOW it executes

# Eager — spawn starts a background task immediately
let handle = spawn { fetch_page(url) };  # running NOW
# ... do other work ...
let page = handle.await;             # collect the result

# Composition — lazy futures enable join_all and select!
let all_pages = join_all(
    urls.map(|u| fetch_page(u))      # creates futures, doesn't start them
).await;                              # NOW they all execute concurrently
```

When a `sub` calls an `async fn`, the interpreter implicitly awaits
the result — it creates the future and immediately drives it to
completion, yielding to Tokio at each `.await` point.  From the
`sub`'s perspective this looks eager, but mechanistically the future
is lazy and the interpreter is polling it.

#### 13.7.6 The two models compose.

Typed async and Perl-style green threads interoperate naturally:

```perl
# async fn called from a sub — the interpreter yields while awaiting
sub process_url {
    my ($url) = @_;
    my $page = fetch_page($url);     # calls async fn — interpreter yields
    return parse($page);             # resumes when future completes
}

# sub called from async fn — runs synchronously within the task
async fn pipeline(urls: &[String]) -> Vec<ParsedPage> {
    let mut results = Vec::new();
    for url in urls {
        let page = fetch_page(url).await?;
        let parsed = parse_page(&page);    # could call a sub internally
        results.push(parsed);
    }
    results
}
```

When a `sub` calls an `async fn`, the result is awaited implicitly —
the interpreter yields to Tokio while the future completes, and other
green threads make progress.  When an `async fn` calls a `sub`, the
sub runs synchronously within the current Tokio task.

#### 13.7.7 The shared heap makes this seamless.

No special data-passing mechanisms are needed for async code.  Futures
capture values from the shared heap.  Spawned tasks access the same
heap.  No channels for basic data sharing, no serialization, no `Arc`
wrapping for values that are already on the shared heap:

```perl
my %cache;                           # on shared heap

async fn fetch_cached(url: &str) -> String {
    if exists $cache{$url} {         # reads shared heap directly
        return $cache{$url};
    }
    let page = http::get(url).await?;
    $cache{$url} = page.clone();     # writes shared heap (RwLock)
    page
}
```

#### 13.7.8 Implementation: the interpreter as an async state machine.

Internally, the interpreter's main execution loop is `async`.  Most
IR operations execute synchronously.  Operations that might block
(IO, sleep, network) are implemented as `.await` points:

```rust
async fn run_op(&mut self, op: &IrOp) -> Result<()> {
    match op {
        IrOp::CallBuiltin { builtin: BuiltinId::Sleep, .. } => {
            tokio::time::sleep(duration).await;
        }
        IrOp::CallBuiltin { builtin: BuiltinId::Open, .. } => {
            let fh = tokio::fs::File::open(path).await?;
            // ...
        }
        IrOp::CallBuiltin { builtin: BuiltinId::ReadLine, .. } => {
            let line = reader.read_line().await?;
            // ...
        }
        IrOp::CallBuiltin { builtin: BuiltinId::HttpGet, .. } => {
            let resp = reqwest::get(url).await?;
            // ...
        }
        _ => {
            self.execute_sync(op)?;
        }
    }
    Ok(())
}
```

Each `spawn` creates a new Tokio task with its own `Interpreter`
(fresh execution context, shared heap).  Tokio's work-stealing
scheduler distributes tasks across OS threads.  The programmer never
manages threads directly.

#### 13.7.9 AOT compilation: sync/async monomorphization.

For ahead-of-time compilation, the async transparency extends
naturally through monomorphization — the same approach Rust uses for
generics.  One source function — whether `sub`, `fn`, or `async fn`
— gets two compiled variants emitted on demand based on the calling
context:

```rust
// What the AOT compiler emits for a single function:

// Sync variant — blocking IO, no Tokio dependency
fn fetch_and_parse_sync(url: &str) -> Result<String> {
    let page = reqwest::blocking::get(url)?.text()?;
    parse_sync(&page)   // calls sync variant of parse
}

// Async variant — yields at IO points
async fn fetch_and_parse_async(url: &str) -> Result<String> {
    let page = reqwest::get(url).await?.text().await?;
    parse_async(&page).await   // calls async variant of parse
}
```

A function explicitly marked `async fn` always gets an async variant
(since the programmer used `.await` syntax inside it).  A plain `fn`
or `sub` gets both variants if the call graph analysis determines it
transitively reaches IO.  The `async` keyword is a convenience for
the programmer, not a requirement for the compiler.

**Async propagates up the call graph.**  If a function calls any
function that might be async (directly or transitively), it needs
an async variant too — the async-ness is viral, just as in Rust.
The AOT compiler builds the call graph and determines which
functions transitively reach an IO point:

```text
fetch_schema()    → does IO             → needs both variants
validate()        → calls fetch_schema  → needs both variants
parse()           → calls validate      → needs both variants
compute()         → no transitive IO    → sync-only is sufficient
```

The IR is the same for both variants.  The split happens during
lowering to native code:

- The compiler walks the call graph and marks every function that
  transitively calls an IO operation.
- Marked functions get both sync and async variants emitted.
- Unmarked functions (provably no transitive IO) get sync-only.
- Within the async variant, every call to a marked function becomes
  `callee_async(...).await`.  Within the sync variant, every call
  becomes `callee_sync(...)`.
- Variants are compiled on demand — if nobody calls a function from
  an async context, the async variant is never emitted.

This is the same "function coloring" problem that Rust, JavaScript,
and Python all face.  The difference is that the programmer never
sees it — the source code is colorless, and the AOT compiler handles
the duplication automatically.

This gives `extern fn` a natural async counterpart:

```perl
# Source — one definition
extern fn fetch_page(url: &str) -> Result<String, Error> {
    let response = http::get(url)?;
    response.text()
}

# AOT emits both:
#   fn fetch_page(url: &str) -> Result<String, Error>           (blocking)
#   async fn fetch_page(url: &str) -> Result<String, Error>     (async)
#
# A Rust program can call either version depending on its context.
```

This means a library written in the language and compiled via AOT
produces a Rust crate that is both sync and async, from one source,
with no async runtime dependency for the sync variant.

#### 13.7.10 Single-threaded compat mode.

In compat mode, the interpreter runs on a single-threaded Tokio
runtime (`current_thread`), preserving Perl 5's sequential execution
semantics.  The shared heap is still used for architectural
uniformity.  Atomic refcount operations can be compiled out or
replaced with non-atomic versions in single-threaded builds.

### 13.8 Advanced Async Features

The base async architecture (§13.7) provides green threads and
async/await.  Several higher-level features build on it to address
real-world concurrency patterns.

#### 13.8.1 Structured concurrency.

A bare `spawn` is fire-and-forget — if the spawned task panics or
errors, no one notices.  Structured concurrency ensures that child
tasks are tied to a parent scope: the parent waits for all children,
errors propagate, and no task outlives the scope that created it.

```perl
# Unstructured — fire and forget
spawn { do_work() };

# Structured — parent waits, errors propagate
my @results = spawn all {
    spawn { fetch("http://a.com") };
    spawn { fetch("http://b.com") };
    spawn { fetch("http://c.com") };
};
# All three complete (or error) before we continue
# @results contains return values in spawn order
# If any task died, the exception propagates here
```

This maps to Tokio's `JoinSet` internally.  The `spawn all` block
creates a `JoinSet`, each `spawn` inside it adds a task, and the
block returns only when all tasks have completed.  If any task
panics, the remaining tasks are cancelled and the panic propagates
to the parent.

The typed equivalent uses explicit futures:

```perl
async fn fetch_all(urls: &[String]) -> Result<Vec<String>, Error> {
    let mut set = JoinSet::new();
    for url in urls {
        set.spawn(async move || { fetch_page(url).await });
    }
    set.join_all().await
}
```

#### 13.8.2 Bounded concurrency.

Spawning 10,000 IO tasks is fine for lightweight requests, but
unbounded concurrency can overwhelm databases, APIs, or file
descriptor limits.  A concurrency limiter built into list operations
provides a natural throttle:

```perl
# Process all URLs, but at most 50 concurrent requests
my @pages = map { get($_) } @urls, :concurrent(50);

# Structured concurrency with a limit
my @results = spawn all :limit(20), {
    for my $item (@work_queue) {
        spawn { process($item) };
    }
};
```

Internally this uses a Tokio `Semaphore`.  The `:concurrent` or
`:limit` adverb sets the semaphore capacity.  Each spawned task
acquires a permit before starting and releases it on completion.

#### 13.8.3 Async streams and iterators.

Data arriving over time — lines from a file, HTTP response chunks,
database result rows, WebSocket messages — maps naturally to async
streams.  Perl's `while (<$fh>)` idiom already reads one item at a
time from a source; the async version yields between items:

```perl
# Standard Perl — looks synchronous, yields between lines
while (my $line = <$socket>) {
    process($line);       # other tasks run while waiting for next line
}

# Processing a streaming HTTP response
my $response = http_get_stream("http://large-file.example.com");
while (my $chunk = <$response>) {
    append_to_file($output, $chunk);
}
```

The interpreter recognizes `<$handle>` in a `while` condition as
an async stream consumer.  Each iteration yields to Tokio while
waiting for the next item.  When the stream is exhausted, the loop
ends normally.

The typed equivalent uses Rust's async iterator trait:

```perl
async fn process_stream(stream: Stream<String>) -> Vec<Parsed> {
    let mut results = Vec::new();
    for await line in stream {
        results.push(parse(&line));
    }
    results
}
```

#### 13.8.4 Cancellation and timeouts.

Perl 5 has no concept of cancelling work in progress.  Tokio does —
dropping a future or task handle cancels it at the next `.await`
point.  This surfaces in both the Perl and typed layers:

```perl
# Timeout — Perl style
my $result = eval {
    timeout 5, sub { long_running_network_operation() };
};
if ($@ =~ /timed out/) {
    warn "Operation timed out, using fallback\n";
}

# Timeout — typed
let result = timeout(Duration::from_secs(5), fetch_page(url)).await;
match result {
    Ok(page) => process(page),
    Err(_) => use_fallback(),
}

# Cancel on scope exit — structured concurrency handles this
spawn all {
    spawn { primary_operation() };
    spawn { monitoring_sidecar() };
};
# When primary_operation completes (or dies), monitoring_sidecar
# is cancelled automatically — no orphaned tasks
```

The `timeout` function wraps any operation in a Tokio `timeout`
future.  If the deadline expires, the inner future is dropped,
cancelling it at the next yield point.  For `sub`-based code, the
interpreter checks a cancellation flag at each yield point (IO
operations, loop iterations) and throws a timeout exception.

#### 13.8.5 Select — first to complete wins.

Multiple concurrent operations can race, with the first to complete
providing the result and the rest being cancelled:

```perl
# Perl style — first successful response wins
my $page = select {
    get("http://primary.example.com"),
    get("http://mirror.example.com"),
    get("http://cdn.example.com"),
};

# Typed — Tokio's select! macro
let page = select! {
    p = fetch_page(primary_url) => p,
    p = fetch_page(mirror_url) => p,
};
```

#### 13.8.6 Compatibility with existing event loop ecosystems.

AnyEvent, IO::Async, and Mojo::IOLoop are widely used in the Perl
ecosystem.  Rather than requiring a full rewrite, a compatibility
bridge can adapt their event loops to Tokio:

- Run AnyEvent/IO::Async callbacks as Tokio tasks.
- Expose a Tokio reactor behind their existing API so modules
  that depend on them work without modification.
- Provide a gradual migration path: existing event-loop-based
  code runs unmodified, new code uses native `spawn`/async.

This is a compatibility concern, not a core architecture decision,
but it matters for adoption of modules that depend on these
frameworks.

### 13.9 Standalone OS Threads

Tokio tasks are the right default for virtually everything — IO,
concurrency, parallelism.  But real OS threads are necessary in a
few cases:

- **CPU-bound work that blocks the executor.**  A tight computational
  loop with no yield points starves other Tokio tasks.
- **FFI into blocking C libraries.**  C code that does its own
  blocking IO or synchronization should not run on a Tokio worker
  thread.
- **C libraries that require real OS thread-local storage.**  Some C
  libraries use `pthread_key` TLS internally.  Tokio tasks migrate
  between OS threads, so their TLS changes unpredictably.  A pinned
  OS thread is the only safe option.
- **Long-lived background threads with their own lifecycle.**  A
  database connection pool manager, a watchdog, a signal handler
  thread.

The runtime provides three spawning mechanisms:

```perl
# Tokio task (default) — lightweight, full interpreter, yields at IO
spawn { perl_code_here() };

# Tokio blocking pool — full interpreter, for CPU-bound Perl code
spawn blocking { cpu_intensive_perl_code() };

# Raw OS thread — full interpreter on a dedicated thread
spawn thread { long_running_ffi_work() };
```

All three get a full interpreter with access to the shared heap.
The difference is scheduling:

| Mechanism | Runs on | Yields at IO? | Task-local `local`? | Use case |
|-----------|---------|---------------|---------------------|----------|
| `spawn { }` | Tokio task | Yes | Yes (`tokio::task_local`) | IO, concurrency, general |
| `spawn blocking { }` | Tokio blocking pool | No (dedicated thread) | Yes (`tokio::task_local`) | CPU-bound Perl |
| `spawn thread { }` | Raw OS thread | No | Yes (`thread_local`) | FFI, pinned TLS, long-lived |

Syntactically, `spawn` is a registered keyword.  The modifiers
`blocking`, `thread`, and `all` are parsed within the keyword hook
as optional bareword arguments — the standard parser never sees them.

**`local` works on all three** — the interpreter switches between
`tokio::task_local!` and `thread_local!` backends depending on how
it was spawned.  The `local` mechanism (§3.3) doesn't care about the
storage backend; it only needs a per-execution-context
`Option<Box<LocalStack>>` cell.  The read path is identical either
way — null check, read `current`, or fall through to the shared
global.

**Typed-only raw threads** are also available for maximum performance
when no interpreter is needed:

```perl
use std::thread;

let data: Arc<Vec<f64>> = \@measurements;

# Raw thread, typed code only — no interpreter overhead
let handle = thread::spawn(move || -> f64 {
    # Only let/fn code here — no my, no sub, no special variables
    let sum: f64 = 0.0;
    for val in data.iter() {
        sum += val;
    }
    sum / data.len() as f64
});

let average: f64 = handle.join()?;
```

This compiles to bare Rust — no interpreter, no task-local storage,
no Tokio dependency.  It is the `extern fn` territory: pure typed
code running on a raw OS thread with zero runtime overhead.

### 13.10 Channels, Supplies, and Message Passing

The concurrency model supports three complementary message-passing
primitives, drawing from Raku's well-designed concurrency layer and
Perl's filehandle idioms.

#### 13.10.1 The three primitives:

| Primitive | Semantics | Analogy |
|-----------|-----------|---------|
| **Channel** | Queue — each item goes to one consumer | Pipe / filehandle |
| **Supply** | Broadcast — every subscriber gets every item | Event / pub-sub |
| **Promise** | Single future value | Deferred result |

All three work with `react`/`whenever`, and channels additionally
support Perl's filehandle syntax.

#### 13.10.2 Channels — queues with filehandle syntax:

A channel is a thread-safe queue.  Each item sent is received by
exactly one consumer (first come, first served).  This is the right
primitive for work distribution:

```perl
# Create a channel — returns two ends, like pipe()
my ($tx, $rx) = channel();

# Send — print to the write end
spawn {
    for my $i (1..100) {
        print $tx "$i\n";     # or: $tx->send($i)
    }
    close $tx;                 # signals no more data
};

# Receive — diamond operator on the read end
while (my $item = <$rx>) {
    chomp $item;
    process($item);
}
# Loop ends when $tx is closed — just like reading a file
```

The filehandle interface (`<$rx>`, `print $tx`, `close $tx`) is
immediately familiar to every Perl programmer.  `while (<$rx>)`
yields to Tokio between items, so other tasks make progress.

Typed channels provide compile-time guarantees:

```perl
let ($tx, $rx) = channel::<i64>();

spawn move || {
    for i in 1..=100 {
        $tx.send(i);
    }
    # $tx dropped on scope exit — channel closes automatically
};

for await item in $rx {
    process(item);
}
```

Bounded channels apply backpressure — the sender blocks when the
buffer is full:

```perl
my ($tx, $rx) = channel(100);   # buffer of 100

spawn {
    for my $line (read_huge_file()) {
        print $tx $line;         # blocks if 100 items buffered
    }
    close $tx;
};

while (<$rx>) { process($_) }
```

Fan-in with multiple producers:

```perl
my ($tx, $rx) = channel();

for my $url (@urls) {
    my $tx_clone = $tx->clone();
    spawn {
        print $tx_clone fetch($url);
        close $tx_clone;
    };
}
close $tx;

while (my $result = <$rx>) { process($result) }
```

#### 13.10.3 Supplies — broadcast streams:

A supply is a stream that can have multiple subscribers.  Every
subscriber receives every item.  This is the right primitive for
events, logs, and notification patterns:

```perl
# Live supply — subscribers see items emitted after they subscribe
my $ticker = supply live {
    for 1..∞ {
        emit $_;
        sleep 1;
    }
};

$ticker->tap(sub { say "Monitor A: $_" });
$ticker->tap(sub { say "Monitor B: $_" });
# Both A and B receive every tick

# On-demand supply — each subscriber starts from the beginning
my $data = supply {
    for my $line (read_file("data.csv")) {
        emit parse_csv($line);
    }
};

$data->tap(sub { say "Reader 1: $_" });  # gets all rows
$data->tap(sub { say "Reader 2: $_" });  # also gets all rows, independently
```

The live vs on-demand distinction (from Raku) matters: a live supply
is like a TV broadcast (miss it and it's gone), an on-demand supply
is like a streaming service (every viewer starts from the beginning).

Supplies can be transformed with familiar list operations:

```perl
my $processed = $raw_supply
    ->grep(sub { $_->{valid} })
    ->map(sub { transform($_) })
    ->batch(10);           # group into batches of 10
```

#### 13.10.4 Pipelines — Unix pipes, in-process:

Concurrent stages connected by channels, with backpressure:

```perl
my @results =
    spawn { generate_urls() }
    | spawn { map { fetch($_) } }
    | spawn { map { parse($_) } }
    | collect;
```

Each `|` creates a channel between stages.  Each stage runs as its
own Tokio task.  A slow stage pauses upstream when the channel buffer
fills.  This is the Unix pipeline model — the idiom Perl was born
from — brought in-process with type safety and structured
concurrency.

#### 13.10.5 `react`/`whenever` — unified event loop:

Borrowed from Raku, `react`/`whenever` provides a declarative way to
handle events from multiple async sources.  `whenever` is polymorphic
— it works with channels, supplies, promises, timers, and any other
async source:

```perl
my ($data_rx, $cmd_rx) = (channel(), channel());
my $heartbeat = timer(interval => 30);
my $log_stream = supply live { ... };

react {
    whenever <$data_rx>  { process($_) }        # channel (filehandle syntax)
    whenever $cmd_rx     { handle_command($_) }  # channel (object syntax)
    whenever $log_stream { log_to_file($_) }     # supply
    whenever $heartbeat  { send_ping() }         # timer
    whenever $shutdown_promise { done }           # promise
}
# react block exits when done is called or all sources close
```

`react` runs an event loop that dispatches incoming items to the
appropriate `whenever` block.  Only one `whenever` block executes at
a time within a single `react` (no internal concurrency), which
eliminates the need for locking within the handlers.  The block exits
when `done` is called or all sources are exhausted.

A `supply` block defines a custom supply using `whenever` to
compose multiple sources:

```perl
my $merged = supply {
    whenever $source_a { emit "A: $_" }
    whenever $source_b { emit "B: $_" }
};
# $merged is a new supply that interleaves items from both sources
```

#### 13.10.6 Select across channels:

```perl
while (my ($which, $item) = select($rx1, $rx2)) {
    if ($which == 0) { handle_primary($item) }
    else             { handle_fallback($item) }
}
```

### 13.11 Shared Heap: Implementation Challenges

The single shared heap is the most ambitious architectural decision
in this design.  It enables concurrency that "just works" for the
common case, but introduces implementation challenges that must be
addressed honestly.

#### 13.11.1 The cardinal invariant:

> The runtime must not execute user Perl code, magic callbacks,
> overload methods, DESTROY, tied operations, or extension callbacks
> while holding internal heap or symbol-table locks.  Any deadlock
> arising from explicit user-level synchronization primitives is the
> responsibility of user code, not the runtime.

This is the single most important implementation rule.  All other
concurrency safety properties follow from it.  The pattern for every
internal operation that might trigger user code is:

- Value reads: acquire lock → read data → release lock.
- Value writes: acquire lock → write data → release lock.
- Magic/tie/overload: acquire lock → read callback ref → release
  lock → execute callback → acquire lock → write result → release.
- DESTROY: never called while any internal lock is held.
- `eval STRING`: compilation never occurs while an internal lock is
  held.

If this invariant is maintained, then:

- User code cannot deadlock against internal locks — it never holds
  one.
- Reentrant magic cannot create lock cycles — the lock is always
  released before the callback runs.
- DESTROY callbacks are safe to run on any task — no internal lock
  is held at the point of destruction.

Explicit user-level locking (`Mutex`, `RwLock`, lock-taking methods
inside callbacks) is powerful and unsafe in the normal way — the
programmer accepts deadlock risk, just as in Rust.

The rest of this section discusses specific challenges and how this
invariant applies to each.

#### 13.11.2 Lock ordering and deadlock.

Per-value `RwLock` is simple when operations touch one value at a
time.  But compound operations — swapping two hash entries, sorting
an array by a comparison that reads other shared data, method
dispatch that walks an inheritance chain — may need to lock multiple
values.  Without lock ordering discipline, deadlock is possible.

Mitigations:

- **Try-lock with fallback.**  When a compound operation needs
  multiple locks, use `try_lock` on each.  If any fails, release
  all held locks and retry with a deterministic ordering (e.g., by
  allocation address).
- **Lock-free reads.**  Most operations are reads.  Readers never
  block (the `RwLock` allows concurrent readers).  Deadlock requires
  two concurrent writers on overlapping value sets — rare.
- **Copy-on-write for compound mutations.**  Instead of locking both
  values, clone one, perform the operation on the clone, then write
  back with a single lock.  This serializes compound writes but
  avoids multi-lock scenarios.

#### 13.11.3 Reentrant magic.

A tied hash's FETCH callback is arbitrary Perl code.  It might
access other shared values, trigger more magic, or mutate the very
value that triggered it.  The cardinal invariant handles this directly —
the lock is always released before user code executes:

1. Acquire lock, read the magic callback (a code reference).
2. Release lock.
3. Execute callback (on this task's interpreter context).
4. Acquire lock, write result back.

Between steps 2 and 4, another task could modify the value.  This is
acceptable — the per-value lock protects individual reads and writes,
not multi-step transactions.  This is the same concurrency contract
as concurrent method calls on a shared object in any language.  Perl
5 doesn't guarantee transactional semantics for magic either.

#### 13.11.4 `DESTROY` on arbitrary threads.

When the last reference to an object is dropped, `DESTROY` runs.
With atomic refcounting on a shared heap, the last `Arc::drop` could
happen on any task.  This means `DESTROY` may run on a different
task than the one that created the object.

This is semantically correct — the object's lifetime has ended
regardless of which task noticed — but it may surprise Perl code
that assumes `DESTROY` runs in the creator's context.  Mitigations:

- **Most DESTROY methods are harmless.**  Closing file handles,
  freeing external resources, updating counters — these work
  regardless of which task runs them.
- **Task-affine destruction.**  For objects that must be destroyed
  on a specific task, the runtime can queue the destruction to the
  creating task's event loop rather than running it inline.  This
  is an opt-in via an attribute: `sub DESTROY :affine { ... }`.
- **Weak references.**  Where the concern is preventing cycles
  rather than running cleanup code, weak references (`weaken`)
  avoid the DESTROY timing issue entirely.

#### 13.11.5 Symbol table mutation.

Package stashes (symbol tables) are mutable shared data.  Operations
that mutate them include `require`, `use`, `eval "sub name { ... }"`,
`AUTOLOAD`, `*glob = \&code`, and `no strict 'refs'` based
manipulations.

The symbol table is a shared hash and thus protected by its per-value
`RwLock`.  Method dispatch reads stash entries (shared read — no
contention).  Module loading writes stash entries (exclusive write —
serializes with other writers).  Note that the cardinal invariant applies:
compilation (which is user-visible code execution) never occurs while
a stash lock is held.

**Method cache invalidation.**  Method dispatch caches the resolved
method for each `(class, method_name)` pair.  When a stash is
mutated (new sub defined, glob assigned, `@ISA` changed), the cache
entries for that class and all its subclasses must be invalidated.
This uses a global generation counter: each stash mutation increments
the counter, and cache entries whose generation is stale are
recomputed on next access.  The generation counter is an atomic
integer — incrementing it is a single atomic operation, and checking
it is a non-locking read.

**Concurrent `require`.**  If two tasks `require` the same module
simultaneously, the compilation must happen only once.  This is
handled by a per-module registry:

```rust
struct ModuleRegistry {
    loaded: RwLock<HashMap<String, ModuleState>>,
}

enum ModuleState {
    Loading(TaskId),     // being compiled by this task
    Loaded(ModuleId),    // compilation complete
    Failed(Error),       // compilation failed
}
```

A task that calls `require` checks the registry.  If the module is
`Loading` by another task, it waits.  If it's `Loaded`, it proceeds.
If it's not present, it inserts `Loading(self)` and begins
compilation.  Compilation itself (which runs `BEGIN` blocks and is
therefore user code) runs without any registry lock held — only the
state transitions are locked.

#### 13.11.6 Single-threaded fast path.

The most common Perl programs are single-threaded.  They should not
pay for concurrency they don't use.  Mitigations:

- **Atomic operations are cheap on modern hardware.**  An
  `AtomicU32::fetch_add` is 1-2 nanoseconds on x86.  For
  comparison, a hash lookup is 20-50ns.  The atomic refcount
  overhead is small relative to the operations that use it.
- **Build-time feature flag.**  A `--single-threaded` build flag
  replaces atomic operations with non-atomic equivalents, removing
  even the memory-ordering overhead.
- **`RwLock` is uncontended-fast.**  An uncontended `RwLock::read`
  on x86 is essentially a single atomic compare-and-swap.  Single-
  threaded code never contends, so every lock acquisition is the
  fast path.
- **Task-count optimization.**  If only one task exists, skip
  locking entirely.  An atomic task counter allows the runtime to
  short-circuit to non-locking paths when concurrency is not active.

#### 13.11.7 Why not per-interpreter heaps with explicit sharing?

The alternative architecture — per-interpreter heaps for untyped
values, explicit sharing for typed values — was the starting point
of this design and was rejected for specific reasons:

- **Serialization cost.**  Passing a large data structure to a
  spawned task requires deep-copying it into the other interpreter's
  heap.  This is the fundamental pain point of Perl 5's `ithreads`.
- **Complexity at the boundary.**  Every crossing between typed and
  untyped code becomes a serialization/deserialization step, not
  just a type coercion.  This discourages mixing the two layers.
- **Two heap implementations.**  Maintaining both a per-interpreter
  arena and a shared arena doubles the surface area of the most
  critical infrastructure code.
- **"Works by default" vs "works by opt-in."**  The shared heap
  means `spawn { process(@data) }` just works.  The per-interpreter
  model means the programmer must explicitly share `@data`, which
  is the `threads::shared` experience that Perl 5 programmers hate.

The shared heap is harder to implement.  It requires careful handling
of lock ordering, reentrant magic, DESTROY timing, and symbol table
mutation.  These are real challenges, addressed above.  But they are
implementation challenges with known solutions, not fundamental
architectural flaws.  The alternative — per-interpreter heaps — has
a fundamental architectural flaw: it makes data sharing expensive
and unergonomic, which is the exact problem this design exists to
solve.

---

## 14. `let`, `fn`, and the Typed Layer

### 14.1 Motivation

The default Perl scalar is a `PerlString`-backed container with
int/num/string coercion flags, magic chains, and `Arc`-refcounted heap
allocation.
This is necessary for Perl 5 compatibility, but it imposes costs:

- Every crossing between Perl and Rust code requires coercion checks
  and potentially data conversion.
- The dual-representation machinery (INT_VALID/NUM_VALID/STR_VALID)
  runs on every arithmetic and string operation even when the program
  never uses the alternate representation.
- Every variable is implicitly nullable (`undef`), and every operation
  can fail silently by returning `undef` instead of signaling an error.
- All variables are mutable, so the compiler cannot reason about
  value stability or optimize based on immutability.
- Atomic refcounting on the shared heap adds overhead compared to
  plain stack-local typed values.

Modern-mode code that is willing to declare explicit types can opt out
of all of this and work directly with native Rust types, gaining
performance, safety, and zero-cost FFI.  And because typed declarations
use the `let` keyword (not a Perl 5 keyword), they can be introduced
incrementally into any Perl codebase without requiring a mode switch.

### 14.2 Design Philosophy: Rust Syntax, Not Hybrid

The typed side of the language uses Rust syntax directly, not Perl
syntax with Rust features bolted on.  This means:

- **Type annotations use `name: Type`**, not `Type name` — matching
  Rust's `let`, function signatures, and struct fields.
- **Typed functions use `fn`**, not `sub` with type annotations —
  keeping the keyword consistent with the semantics.
- **Type names are Rust types** (`String`, `Arc<str>`, `Vec<T>`),
  not Perl aliases.

The rationale is that mixing syntactic traditions produces code that
feels like it can't pick a style.  `sub greet(name: &str) -> String`
is neither Perl nor Rust.  Clean separation is better:

```perl
sub greet { my ($name) = @_; return "Hello, $name!" }  # Perl
fn greet(name: &str) -> String { f"Hello, {name}!" }   # Rust
```

Each keyword signals which world you're in: `my`/`sub` for Perl
semantics, `let`/`fn` for Rust semantics.

Parsing is not a problem: `fn` is not a Perl 5 keyword, and in type
position `<` unambiguously opens a type parameter list.  The
lexer–parser feedback loop already designed for Perl's existing
ambiguities handles both for free.

### 14.3 `let`, `fn`, and Type Inference

#### 14.3.1 `let` for variables, `fn` for functions.

`let` and `fn` are the typed counterparts of `my` and `sub`.  Neither
is a Perl 5 keyword, so both can be introduced with zero backward
compatibility impact:

```perl
# Perl layer — classic semantics
my $name = "world";
sub greet { my ($who) = @_; return "Hello, $who!" }

# Rust layer — typed semantics
let name: String = "hello";
fn greet(who: &str) -> String { f"Hello, {who}!" }
```

#### 14.3.2 Type inference.

Type annotations on `let` are optional when the compiler can infer
the type from the initializer:

```perl
let name = "hello";              # inferred String
let count = 42;                  # inferred i64
let ratio = 3.14;               # inferred f64
let flag = true;                 # inferred bool
let items = vec!["a", "b"];     # inferred Vec<String>

# Explicit annotation when needed or desired
let count: u32 = 42;            # override default integer type
let config: Arc<str> = "host";  # Arc won't be inferred from a literal
```

This makes the on-ramp as gentle as possible.  `let $name = "hello";`
looks almost like `my $name = "hello"` but gives you immutability,
type checking, and no coercion overhead — with the type inferred and
a `$` alias for interpolation.

#### 14.3.3 `fn` for typed functions.

`fn` declares a function with Rust-style typed parameters, typed
return, and value semantics.  No `@_`, no prototypes, no `wantarray`:

```perl
fn greet(name: &str) -> String {
    f"Hello, {name}!"
}

fn add(a: i64, b: i64) -> i64 {
    a + b
}

fn read_config(path: &str) -> Result<String, String> {
    if -e path {
        Ok(slurp(path))
    } else {
        Err(f"not found: {path}")
    }
}
```

Return type annotation is required on `fn` (unlike `let`, where
inference handles it).  This is deliberate: function signatures are
the primary documentation for an API, and explicit return types
make that documentation reliable.

#### 14.3.4 `sub` stays Perl.

`sub` retains its full Perl 5 semantics: `@_` argument passing,
prototypes, wantarray, dynamic context.  It is not deprecated or
diminished — it is the right choice for Perl-style code:

```perl
sub process_items {
    my (@items) = @_;
    for my $item (@items) {
        # classic Perl
    }
}
```

`fn` and `sub` can call each other freely.  When a `fn` calls a
`sub`, arguments are coerced from typed to untyped.  When a `sub`
calls a `fn`, arguments are coerced from untyped to typed (with
runtime checks where needed).

#### 14.3.5 All four keywords coexist:

| Keyword | Semantics | Typed | Mutable | Args |
|---------|-----------|-------|---------|------|
| `my` | Perl | No | Always | — |
| `sub` | Perl | No | — | `@_`, prototypes |
| `let` | Rust | Yes | Opt-in (`mut`) | — |
| `fn` | Rust | Yes | — | Named, typed |

### 14.4 Sigils, Aliases, and Interpolation

#### 14.4.1 Core rule: `let` always creates a sigil-less variable.

Every `let` declaration creates a variable in the sigil-less namespace.
A sigil-less namespace is new — it sits alongside Perl's existing
`$scalar`, `@array`, `%hash`, `&code`, and `*glob` namespaces, forming
a natural extension of Perl's multi-namespace model.

The `$` sigil on a `let` declaration is an opt-in that **also** creates
a lexical alias in the scalar (`$`) namespace, allowing easy
interpolation and a more traditionally Perlish coding style:

```perl
# Sigil-less only — no $ alias
let name: String = "world";
print name;                    # fine — sigil-less access
print name.to_uppercase();    # fine — method call
# $name does NOT refer to this variable

# Sigil-less WITH $ alias
let $name: String = "world";
print name;                    # fine — sigil-less access
print $name;                   # fine — $ alias to same variable
print name.to_uppercase();    # fine — method call
print "Hello, $name!\n";      # fine — interpolates via the alias
```

The `$` alias is not a separate variable — it is a lexical binding in
the scalar namespace that points to the same underlying typed value.
A programmer who prefers the Perlish `$`-everywhere style can declare
`let String $name` and then use `$name` throughout their code, never
writing bare `name` at all.  Conversely, a programmer who prefers
Rust-style sigil-less code can omit the `$` and use bare names
throughout.  Both are valid and natural.

**This means `let $foo` does two things:**

1. Creates `foo` in the sigil-less namespace (as every `let` does).
2. Creates `$foo` as a lexical alias in the scalar namespace, shadowing
   any `$foo` that might exist in an outer scope.

#### 14.4.2 Shadowing.

`let` follows Rust's shadowing semantics.  Redeclaring a name shadows
the previous binding:

```perl
let x: String = "hello";
let x: i64 = x.len();          # shadows the String x
let x: bool = x > 3;           # shadows again
```

The `$` alias participates in shadowing independently.  If a new `let`
omits the `$`, the alias from a previous `let $` goes away:

```perl
let $x: String = "hello";
print $x;                      # "hello" — alias exists

let x: i64 = x.len();          # shadows x, but no $ this time
print x;                       # 5 — fine
print $x;                      # error or refers to outer $x — alias is gone

let $x: bool = x > 3;          # shadows again, alias is back
print $x;                      # true
```

Shadowing across `let` and `my` works naturally within the scalar
namespace.  `let $foo` shadows `my $foo` and vice versa, because
both bind a name in the scalar namespace:

```perl
my $count = 42;
let $count: i64 = $count;       # shadows my $count with typed version
print $count;                  # typed i64 — the let's alias
print count;                   # same value — sigil-less access
```

#### 14.4.3 Coding style is a choice, not a mandate.

Some programmers will prefer the Perlish style, adding `$` to most
declarations and using `$name` throughout:

```perl
let $greeting: String = "Hello";
let $retries: i64 = 5;
let $items: Vec<String> = ("a", "b", "c");

print "$greeting, welcome!\n";
print "Retrying $retries times\n";
```

Others will prefer the Rust style, omitting `$` and using bare names,
adding `$` only where they need interpolation:

```perl
let $greeting: String = "Hello";    # $ for interpolation
let max_retries: i64 = 5;           # no $ — used in expressions only
let cache: HashMap<String, i64> = (); # no $ — data structure

print "$greeting, welcome!\n";
for let i: i64 (0..max_retries) {
    ...
}
```

Both are idiomatic and the project should not favor one over the other.

#### 14.4.4 Future consideration: `@` and `%` aliases.

The same aliasing mechanism could extend to `@` and `%` sigils,
allowing typed collections to also be accessible through familiar
Perl array/hash syntax:

```perl
let @names: Vec<String> = ("Alice", "Bob", "Carol");
# creates sigil-less 'names' AND @names alias

print names[0];            # sigil-less access
print $names[0];           # Perl array element access via @names
for my $n (@names) { ... } # Perl iteration
```

This is appealing but raises questions that need careful design:
does `@names` in list context flatten?  Do `push @names` and
`names.push()` both work?  What about `wantarray`?  These
interactions should be designed deliberately rather than assumed,
so `@`/`%` aliases are noted here as a future direction rather than
an initial commitment.

#### 14.4.5 Name resolution rules:

Sigil-less and sigiled lookups are independent:

1. Bare `count` → search the sigil-less namespace up the lexical scope
   chain.  If not found, treat as bareword/function name per standard
   Perl rules.
2. `$count` → search the scalar namespace up the lexical scope chain,
   as in Perl 5.  This finds `my $count`, `let ... $count` aliases,
   and package variables as usual.

There is no cross-namespace fallback.  `$foo` never finds a sigil-less
`let foo` (without `$`), and bare `foo` never finds a `my $foo`.  This
keeps resolution unambiguous and predictable.

#### 14.4.6 Interpolation in strings.

Because the `$` alias puts typed variables into the scalar namespace,
classic `"..."` string interpolation works without any new syntax:

```perl
let $name: String = "world";
let $count: i64 = 42;

"Hello, $name!"                # "Hello, world!"
"You have $count items"        # "You have 42 items"
"${name}bar"                   # "worldbar" — brace disambiguation
```

For sigil-less variables (without `$`), interpolation requires either
the `$` alias or format strings.  The `${+expr}` idiom works for
one-off cases:

```perl
let name: String = "world";     # no $ alias
"Hello, ${+name}!"             # unary + forces expression evaluation
"Total: ${+count * 2}"         # expression interpolation
```

#### 14.4.7 `${expr}` generalized expression interpolation.

`${...}` in double-quoted strings is generalized to accept any
expression when the content is not a bare identifier.  When it *is*
a bare identifier, it retains Perl 5 semantics (resolving as
`$identifier`):

```perl
"${foo}bar"                    # $foo + "bar" — backward compatible
"${6 * 7}"                     # "42" — expression
"${join(', ', @list)}"         # expression with function call
```

This replaces Perl's `${\expr}` and `@{[list]}` kludges:

```perl
# Old way — ref/deref hack
"The answer is ${\( 6 * 7 )}"
"Items: @{[ map { uc } @items ]}"

# New way — expression in ${...}
"The answer is ${6 * 7}"
"Items: ${join(', ', map { uc } @items)}"
```

#### 14.4.8 Format strings (`f"..."`) — optional convenience.

A format string quoting syntax provides the cleanest interpolation
for sigil-less variables, following the precedent of Python f-strings
and Rust's `format!` macro:

```perl
let name: String = "world";
let count: i64 = 42;

f"Hello, {name}!"              # "Hello, world!"
f"Total: {count * 2}"          # "Total: 84"
f"Upper: {name.to_uppercase()}" # "Upper: WORLD"
f"Literal {{braces}}"          # "Literal {braces}"

# f-strings can also access sigiled variables
my $legacy = "old";
f"New: {name}, old: {$legacy}" # both work inside f"..."
```

`{expr}` interpolates, `{{` and `}}` produce literal braces.  A
`qf//` variant is available for alternate delimiters, paralleling
`q//` / `qq//`.

Format strings are a convenience, not a necessity.  The `$` alias
mechanism covers the common case, and `${+expr}` handles one-off
sigil-less interpolation.  Format strings are most useful for code
that is heavily sigil-less and does a lot of string formatting.

#### 14.4.9 Summary of interpolation mechanisms:

| String type | Sigiled `$foo` | Sigil-less `foo` | Expressions |
|-------------|---------------|-----------------|-------------|
| `"..."` | `$foo`, `${foo}` | `${+foo}` | `${expr}` |
| `f"..."` | `{$foo}` | `{foo}` | `{expr}` |
| `'...'` | no interpolation | no interpolation | no |

### 14.5 Mutability: Immutable by Default

In Perl 5, every `my` variable is mutable.  `let` variables are
immutable by default, matching Rust's `let` vs `let mut`:

```perl
let x: i64 = 42;
x = 99;                             # COMPILE ERROR — immutable

let mut x: i64 = 42;
x = 99;                             # fine

let frozen: Vec<String> = ("a", "b", "c");
push frozen, "d";                   # COMPILE ERROR — immutable

let mut items: Vec<String> = ("a", "b", "c");
push items, "d";                    # fine

# my variables are always mutable, as in Perl 5
my $y = 42;
$y = 99;                            # fine — my is always mutable
```

Immutability enables the compiler to:

- Prove that values do not change, enabling more aggressive optimization.
- Allow safe sharing of immutable values across threads without
  synchronization.
- Catch accidental mutation bugs at compile time.

The `mut` keyword applies to the binding, not the type.  Interior
mutability (e.g., `Arc<RwLock<T>>`) is handled by the type itself,
not the binding.

### 14.6 The Type Surface

The types available correspond directly to Rust types.  The initial
set covers the most useful categories:

#### 14.6.1 Primitive types:

| Perl Syntax | Rust Type | Notes |
|-------------|-----------|-------|
| `i8`, `i16`, `i32`, `i64`, `i128` | same | Signed integers |
| `u8`, `u16`, `u32`, `u64`, `u128` | same | Unsigned integers |
| `isize`, `usize` | same | Pointer-width integers |
| `f32`, `f64` | same | Floating point |
| `bool` | `bool` | True/false, no Perl truthiness |

#### 14.6.2 String and byte types:

| Perl Syntax | Rust Type | Notes |
|-------------|-----------|-------|
| `String` | `String` | Owned, growable, valid UTF-8 |
| `&str` | `&str` | Borrowed string slice (in parameters) |
| `Bytes` | `bytes::Bytes` | Immutable, refcounted, zero-copy slice |
| `BytesMut` | `bytes::BytesMut` | Mutable byte buffer, Tokio-native |

#### 14.6.3 Smart pointers and wrappers:

| Perl Syntax | Rust Type | Notes |
|-------------|-----------|-------|
| `Box<T>` | `Box<T>` | Heap-allocated owned value |
| `Arc<T>` | `Arc<T>` | Shared ownership, atomic refcount |
| `Mutex<T>` | `Mutex<T>` | Mutable shared state, exclusive lock |
| `RwLock<T>` | `RwLock<T>` | Mutable shared state, reader/writer lock |
| `Option<T>` | `Option<T>` | Nullable value (`undef` = `None`) |
| `Result<T, E>` | `Result<T, E>` | Success-or-error value |

#### 14.6.4 Collections:

| Perl Syntax | Rust Type | Notes |
|-------------|-----------|-------|
| `Vec<T>` | `Vec<T>` | Growable array |
| `HashMap<K, V>` | `HashMap<K, V>` | Hash table |
| `HashSet<T>` | `HashSet<T>` | Set |
| `BTreeMap<K, V>` | `BTreeMap<K, V>` | Ordered map |

#### 14.6.5 What is explicitly not exposed:

- Lifetime annotations (`'a`, `'static`) — borrows are restricted to
  `fn` parameters with a simple scope rule; no lifetimes needed (§14.9)
- Move semantics — assignment clones; shared ownership uses `Arc` via
  `\$x` (§14.9)
- Generic type parameters on user-defined types — `struct Foo<T>` is
  a future extension (§14.16)
- `Pin`, `PhantomData`, `MaybeUninit` — internal Rust machinery

**User-defined types** (`struct`, `enum`, `impl`, `trait`) are
covered in §14.15.

### 14.7 `Option<T>` and `undef`

In Perl 5, every variable is implicitly nullable — anything can be
`undef` at any time.  `let` variables without `Option` reverse this
default: they *must* hold a value.

```perl
let name: String = "hello";            # must hold a String
name = undef;                         # COMPILE ERROR

let nickname: Option<String> = undef;  # explicitly nullable — fine
nickname = "Dev";                     # also fine
nickname = undef;                     # fine again

# my variables remain always-nullable, as in Perl 5
my $anything = undef;                 # fine — classic Perl
```

`undef` is the Perl spelling of `None`.  In typed context, the compiler
enforces:

- Assignment of `undef` to a non-`Option` variable is a compile error.
- A function with return type `String` cannot return `undef`.
- A function with return type `Option<String>` can return `undef`.

#### 14.7.1 Unwrapping and narrowing:

`defined` narrows an `Option<T>` to `T` within its truthful branch,
analogous to Rust's `if let Some(x)`:

```perl
let input: Option<String> = get_input();

if defined input {
    # here input is known to be String, not Option<String>
    print f"Got: {input}\n";
}

# Or explicit methods
let value: String = input // "default";         # like unwrap_or
let value: String = input.unwrap();             # panics if undef
let value: String = input.expect("need input"); # panics with message
```

#### 14.7.2 At the untyped boundary:

```perl
my $legacy = get_something();          # untyped, might be undef
let s: String = $legacy;               # RUNTIME ERROR if $legacy is undef
let s: Option<String> = $legacy;       # safe — undef maps to None
```

### 14.8 `Result<T, E>` and Auto-Unwrap

`Result<T, E>` provides typed error handling that coexists with Perl's
`eval`/`die`.  The key ergonomic feature, borrowed from Raku's `Failure`
type, is **auto-unwrap**: when a `Result` is used as its inner type, it
automatically unwraps — `Ok(v)` yields `v`, and `Err(e)` throws an
exception (becomes a `die`).

This means callers never *have* to think about `Result` if they don't
want to.  Functions can return `Result` for precision, and callers can
choose their error handling style:

```perl
fn read_config(path: String) -> Result<String, String> {
    if -e path {
        return Ok(slurp(path));
    }
    return Err(f"Config file not found: {path}");
}

# Style 1: Auto-unwrap — just use it as a String.
# Ok unwraps silently; Err throws an exception.
let text: String = read_config("/etc/app.conf");
process(text);

# Style 2: Explicit matching when you want to handle errors
let config: Result<String, String> = read_config("/etc/app.conf");
match config {
    Ok(text)  => process(text),
    Err(msg)  => warn f"Fallback: {msg}\n",
}

# Style 3: The ? operator for early return (in Result-returning functions)
fn init() -> Result<bool, String> {
    let cfg: String = read_config("/etc/app.conf")?;  # propagates Err
    return Ok(true);
}

# Style 4: Classic Perl eval/die — works seamlessly
my $text = eval { read_config("/etc/app.conf") };
if ($@) { warn "Failed: $@\n" }
```

#### 14.8.1 Auto-unwrap rules:

- Assigning a `Result<T, E>` to a `let T` variable: auto-unwraps.
  `Ok(v)` yields `v`; `Err(e)` throws `e` as an exception.
- Assigning a `Result<T, E>` to a `my` variable: auto-unwraps.
  Same behavior — `Err` becomes `die`.
- Using a `Result<T, E>` in any expression context that expects `T`:
  auto-unwraps.
- Assigning a `Result<T, E>` to a `let Result<T, E>` variable:
  no unwrap, the `Result` is preserved for explicit inspection.

This means `Result` is "strict when you ask for it, ergonomic by
default."  A function author gets to be precise about error returns.
A caller gets to choose: handle it explicitly with `match`, propagate
it with `?`, auto-unwrap it into an exception, or catch it with
`eval`.

#### 14.8.2 Bridge to Perl error model:

- `die` inside a function with a `Result` return type is caught and
  wrapped as `Err`.
- The `?` operator in a non-`Result`-returning function becomes a `die`.
- `eval { ... }` around code that produces an `Err` catches it and
  sets `$@` as usual.
- Auto-unwrap of `Err` sets `$@` just like `die` does.

Typed and untyped error handling compose naturally in all directions.

#### 14.8.3 `result` returns `Result` — `eval` and `try` are unchanged:

Perl's `eval { }` returns `undef` on exception and sets `$@`.  Code
that relies on this idiom (`my $x = eval { might_die() }`) must
continue to work.  Therefore `eval` retains exact Perl 5 semantics.

`result` is the typed error-handling expression.  It catches
exceptions and returns `Result<T, PerlException>` — never `undef`,
never sets `$@`:

```perl
# eval — unchanged Perl 5 semantics
my $x = eval { might_die() };       # undef if it died
if ($@) { warn "Failed: $@" }

# result — returns Result<T, PerlException>
let $result = result { parse_config("app.conf") };

# With ? operator — propagates Err as die
let $config = result { parse_config("app.conf") }?;

# With auto-unwrap — Err becomes die when used as T
let $config = result { parse_config("app.conf") };
process($config);     # auto-unwraps; Err throws here

# With match — handle errors explicitly
match result { parse_config("app.conf") } {
    Ok(config) => start_server(config),
    Err(e) => {
        warn f"Startup failed: {e}\n";
        use_default_config()
    }
}

# In a Result-returning function — chain with ?
fn init() -> Result<Config, PerlException> {
    let $db = result { connect_to_db() }?;
    let $cfg = result { load_config() }?;
    Ok(Config::new(db, cfg))
}
```

#### 14.8.4 `try`/`catch`/`finally` — statement form:

Structured exception handling for side-effectful code, following
`Syntax::Keyword::Try`:

```perl
try { dangerous() }
catch ($e) { handle($e) }
finally { cleanup() }
```

#### 14.8.5 Three mechanisms, three keywords, zero overlap:

| Mechanism | Returns | Sets `$@`? | Use case |
|-----------|---------|-----------|----------|
| `eval { }` | Value or `undef` | Yes | Perl 5 compat |
| `eval STRING` | Value or `undef` | Yes | Runtime compilation |
| `result { }` | `Result<T, E>` | No | Typed error handling (expression) |
| `try`/`catch`/`finally` | — (statement) | No | Structured exception handling |
| `?` operator | Propagates `Err` | No | Error propagation in `fn` |

The exception type unifies Perl's various `die` forms:

```rust
enum PerlException {
    Str(PerlString),     // die "message"
    Object(Value),       // die $object
}
```

`result`, `try`, `catch`, and `finally` are registered as keywords
via `PL_keyword_plugin`, making them available on standard Perl 5 via
`use Typed` (where `result` compiles down to `eval`/`$@` with wrapper
logic that constructs a `Result` object).

### 14.9 Ownership Model

Typed values need clear ownership semantics, but reimplementing Rust's
full borrow checker would be an enormous effort and would inflict
Rust's steepest learning curve on Perl programmers.  Instead, the
ownership model uses three simple mechanisms — clone on assign, Arc
on reference, and borrows only in `fn` parameters — that provide
safety and performance without dataflow analysis.

**No borrow checker.**  The design deliberately avoids Rust-style
move semantics and lifetime tracking.  Assignment copies values.
Shared ownership uses `Arc`.  Borrows exist only in function call
boundaries where a simple scope rule suffices.  This is the pragmatic
tradeoff: slightly more copying than Rust in some cases, vastly
simpler for both the implementation and the programmer.

#### 14.9.1 Clone on assign.

Assignment of typed values clones the data.  Both the original and
the copy are fully independent:

```perl
let $x: String = "hello";
let $y = $x;              # $y gets a clone — both accessible
let $z = $x;              # another clone — all three independent
$x;                        # still valid
```

For `Copy` types (integers, floats, bools), this is a trivial bit
copy with zero overhead — identical to Rust:

```perl
let $a: i64 = 42;
let $b = $a;              # bit copy — both accessible, zero cost
```

For heap-allocated types (`String`, `Vec<T>`, `HashMap<K,V>`), this
is a deep clone.  If profiling shows a clone is expensive on a hot
path, the programmer can switch to `Arc` for shared ownership.

#### 14.9.2 `\$x` creates `Arc` — Perl references as shared ownership.

Taking a Perl-style reference of a typed value upgrades it to `Arc<T>`
in place and returns another `Arc<T>`:

```perl
let $config: String = "database.example.com";

# Before any reference: plain String, zero overhead
process($config);

# Take a reference → upgrades to Arc<String>
let $shared = \$config;    # $config is now Arc<String>, refcount 2
let $also = \$config;      # refcount 3

print $config;             # transparent — reads through the Arc
print $$shared;            # explicit deref — same data
```

The lifecycle:

1. `let $config: String = "hello"` — plain `String`, no overhead.
2. `\$config` — runtime upgrades `$config` in place to `Arc<String>`,
   returns another `Arc<String>`.  One allocation (the Arc wrapper).
3. Subsequent `\$config` — `Arc::clone`, refcount bump only, no copy.
4. When all Arcs drop, the data is freed.

This maps directly to how `\$x` already works in Perl 5 — it creates
a reference-counted shared pointer.  The typed version just uses
`Arc` (atomic refcount, thread-safe) instead of Perl's SV refcount.

#### 14.9.3 `\$x` enables sharing across threads:

Because `Arc<T>` is `Send + Sync`, values that have been referenced
can be shared across threads without serialization:

```perl
let $config: String = "database.example.com";
let $shared = \$config;

spawn move || {
    print "Using $$shared\n";    # Arc clone moved into closure
};

print $config;                    # still accessible in parent thread
```

#### 14.9.4 Borrows are `fn` parameters only — the typed `@_`.

In Perl 5, `@_` contains aliases to the caller's values.  `$_[0]`
*is* the caller's variable — modify it and the caller sees the
change.  This is effectively a borrow.

`&T` in `fn` parameters is the typed, safe version of `@_` aliasing.
But `@_` aliasing also works when passing typed values to `sub` — the
same mechanism applies in both cases.

#### 14.9.5 `fn` parameters — explicit in the signature:

```perl
fn word_count(text: &str) -> usize {
    scalar split(/\s+/, text)       # reads without copying
}

fn append_bang(s: &mut String) {
    s.push_str("!");                # modifies caller's variable
}

let mut $msg: String = "hello";
let $n = word_count(&$msg);         # immutable borrow
append_bang(&mut $msg);              # mutable borrow — explicit at call site
print $msg;                          # "hello!"
```

#### 14.9.6 `sub` with typed values — implicit `@_` borrows:

When a typed value is passed to a `sub`, `@_` contains a borrow,
not a clone.  By default, the borrow is immutable (`&T`):

```perl
let $name: String = "hello";

sub show { print "$_[0]\n" }
show($name);                        # @_ aliases $name as &String — fine

sub bad { $_[0] .= "!" }
bad($name);                         # RUNTIME ERROR: cannot modify &T borrow
```

To pass a mutable borrow to `sub`, the caller opts in with `mut`:

```perl
let mut $name: String = "hello";

sub modify { $_[0] .= "!" }
modify($name);                      # ERROR: still &T by default
modify(mut $name);                   # OK: explicit &mut, caller sees the risk

print $name;                         # "hello!" — modified through @_ alias
```

The `mut` at the call site is the key safety feature.  It makes
mutation visible where it happens — in the caller's code, not hidden
in the function body.  This mirrors Rust's `&mut x` at the call site,
just spelled in a more Perlish way.

#### 14.9.7 Perl-native `my` variables retain interior mutability:

`my` variables live on the interpreter heap, which provides interior
mutability.  `@_` aliasing with mutation works exactly as in Perl 5,
no opt-in needed:

```perl
my $name = "hello";
sub modify { $_[0] .= "!" }
modify($name);                      # works — Perl scalars are always mutable
print $name;                         # "hello!" — standard Perl 5 behavior
```

This means existing Perl code is completely unchanged.  The immutable-
borrow default only applies to typed `let` values.

#### 14.9.8 Summary of @_ aliasing by declaration type:

| Declaration | `@_` alias type | Mutable via `@_`? |
|-------------|----------------|-------------------|
| `my $x` | Direct alias (interior mutability) | Yes (Perl 5 compat) |
| `let $x: T` | `&T` | No |
| `let mut $x: T`, caller passes `$x` | `&T` | No |
| `let mut $x: T`, caller passes `mut $x` | `&mut T` | Yes |

#### 14.9.9 Borrows do not escape.

Whether created explicitly via `fn` signatures or implicitly via
`@_`, borrows cannot outlive the function call.  The compiler
enforces:

- `&T` and `&mut T` may appear in `fn` parameter types and are
  implicitly present in `@_` during a call.
- They may not appear as `let` binding types, return types, struct
  field types, or collection element types.
- A function cannot return a `&T` that refers to a parameter.

```perl
fn good(text: &str) -> usize {   # borrow in, owned value out — fine
    text.len()
}

fn bad(text: &str) -> &str {     # COMPILE ERROR: &T in return type
    text
}
```

This is a simple syntactic rule.  No borrow checker, no dataflow
analysis.  If you need a durable reference that outlives a function
call, use `\$x` to create an `Arc`.

#### 14.9.10 Summary of how values move around:

| Mechanism | Perl equivalent | What happens | Overhead | Escapes? |
|-----------|----------------|--------------|----------|----------|
| `let $y = $x` | `my $y = $x` | Clone | One clone | N/A (owned) |
| `fn(x: T)` | `my ($x) = @_` | Clone into callee | One clone | N/A (owned) |
| `fn(x: &T)` | `$_[0]` read alias | Immutable borrow | Zero | No |
| `fn(x: &mut T)` | `$_[0]` write alias | Mutable borrow | Zero | No |
| `sub(@_)` with typed arg | `$_[0]` aliasing | `&T` or `&mut T` | Zero | No |
| `\$x` | `\$x` in Perl 5 | Upgrade to Arc | One Arc alloc (first time) | Yes |
| `Arc::clone` | multiple `\$x` | Refcount bump | Atomic increment | Yes |

#### 14.9.11 Interaction between assignment and Arc:

Once a value has been upgraded to `Arc`, assignment clones the `Arc`
(refcount bump), not the underlying data:

```perl
let $x: String = "hello";
let $r = \$x;              # $x upgraded to Arc<String>

let $s = $r;               # Arc clone — refcount bump, no data copy
let $t = $x;               # also Arc clone — $x is now Arc too

let $copy = $$r;            # explicit deref + clone — independent String
```

### 14.10 Typed/Untyped Boundary Semantics

When a value crosses between the typed and untyped worlds, explicit
coercion rules apply:

#### 14.10.1 Typed → untyped (always succeeds):

```perl
let s: String = "hello";
my $x = s;                 # $x becomes a PerlString with UTF-8 flag set

let n: i64 = 42;
my $y = n;                 # $y becomes a full scalar with INT_VALID flag set

let o: Option<String> = undef;
my $z = o;                 # $z becomes undef
```

These conversions are always valid and cheap.

#### 14.10.2 Untyped → typed (may fail):

```perl
my $raw = read_file("data.bin");       # PerlString, may not be valid UTF-8
let text: String = $raw;               # RUNTIME ERROR if not valid UTF-8
let maybe: Option<String> = $raw;      # RUNTIME ERROR if not UTF-8 and not undef
```

The compiler should warn statically about potentially-failing coercions.

#### 14.10.3 Coercion cost matrix for strings:

| From | To | Cost |
|------|----|------|
| `String` → `PerlString` | Wrap as Bytes, set flags | Cheap (refcount) |
| `PerlString` (Rust-valid) → `String` | Take bytes, no validation | Cheap (flag check) |
| `PerlString` (unknown) → `String` | Full UTF-8 validation | O(n), may fail |
| `PerlString` → `Bytes` | Access inner `buf` | Zero-cost |
| `Bytes` → `PerlString` | Wrap with flags | Cheap (refcount bump) |
| `String` → `&str` | Pointer cast | Zero-cost |
| `Arc<str>` → `&str` | Deref | Zero-cost |

### 14.11 Compiler Optimization of Typed Code

When the compiler sees operations on typed values, it emits specialized
IR that bypasses the Perl coercion machinery:

```perl
let a: i64 = 10;
let b: i64 = 20;
let c: i64 = a + b;        # emits: IrOp::AddI64 — one machine instruction
```

```perl
let mut msg: String = "Hello";
msg .= ", world!";         # emits: String::push_str — no PerlString alloc
```

```perl
let cfg: Arc<str> = "host";
let view: &str = cfg;       # emits: Arc::deref — one pointer chase
```

Immutable bindings (the default) additionally allow the compiler to
inline values, hoist them out of loops, and share them across threads
without synchronization.

### 14.12 Typed Values and Concurrency

Typed values are `Send + Sync` by construction.  This makes them the
natural vocabulary for concurrent code:

```perl
use threads;

# Immutable shared data — Arc, zero synchronization
let config: Arc<str> = load_config();
let wordlist: Arc<Vec<String>> = load_words();

# Mutable shared state — explicit locking
let counter: Arc<RwLock<i64>> = 0;

spawn {
    # config and wordlist are cloned (Arc clone = refcount bump)
    for let word: &str (wordlist.iter()) {
        if matches(config, word) {
            let mut guard = counter.write();
            *guard += 1;
        }
    }
};

# Read without write lock
let total: i64 = *counter.read();
```

`Arc<T>` for immutable shared data and `Arc<RwLock<T>>` (or
`Arc<Mutex<T>>`) for mutable shared data are the standard Rust
patterns.  Exposing them directly means Perl programmers learn the
same concurrency vocabulary that Rust programmers use, and the
implementation is zero-overhead.

All `my` variables — including magic-bearing values — live on the
shared heap (§13.3) and are shareable across threads.  Typed values
additionally provide compile-time guarantees and avoid the atomic
refcount overhead of the shared heap.

### 14.13 Typed Values and FFI

When a Rust extension function is declared with typed parameters, the
FFI boundary is zero-cost:

```rust
// Rust extension
fn greet(name: &str, count: i64) -> String {
    format!("Hello, {}! (visit #{})", name, count)
}
```

```perl
let name: String = "Deven";
let visits: i64 = 7;
let msg: String = greet(name, visits);  # zero-cost: &str deref + i64 copy
```

If the call site uses untyped values, the runtime inserts coercion
checks automatically — the extension still works, it just pays the
conversion cost.

Rust extensions can also return `Result` and `Option`.  With
auto-unwrap, the caller doesn't even need to think about `Result`:

```rust
fn parse_config(path: &str) -> Result<Config, String> { ... }
```

```perl
# Auto-unwrap: Ok yields Config, Err becomes die
let cfg: Config = parse_config("/etc/app.conf");

# Or explicit handling if desired
let cfg: Result<Config, String> = parse_config("/etc/app.conf");
match cfg { ... }
```

### 14.14 `extern fn` — Standalone Rust-Compatible Functions

A `fn` that uses only typed values and calls only other typed `fn`s
is effectively Rust code in a different syntax.  There is no reason it
can't compile to a plain native function callable from Rust without
any interpreter present.

The `extern fn` annotation makes this explicit and compiler-enforced:

```perl
# Standalone — compiles to a normal Rust function
extern fn add(a: i64, b: i64) -> i64 {
    a + b
}

# Standalone — can call other extern fn
extern fn distance(x: f64, y: f64) -> f64 {
    (x * x + y * y).sqrt()
}

# Standalone — typed collections, closures, iterators
extern fn sum_positive(items: &[i64]) -> i64 {
    items.iter().filter(|x| *x > 0).sum()
}

# Standalone — regex lowers to perl-regex crate calls, no runtime needed
extern fn has_digits(s: &str) -> bool {
    s =~ /\d+/
}

# Standalone — named captures, backreferences, etc.
extern fn parse_date(s: &str) -> Option<(i64, i64, i64)> {
    if s =~ /(?<y>\d{4})-(?<m>\d{2})-(?<d>\d{2})/ {
        Some(($+{y}.parse(), $+{m}.parse(), $+{d}.parse()))
    } else {
        None
    }
}

# Regular fn — uses the runtime, cannot be extern
fn process(text: &str) -> Result<String, String> {
    let data = eval { parse(text) };   # eval requires runtime
    # ...
}
```

`extern fn` is a promise: "this function has no Perl runtime
dependency."  The compiler enforces it by verifying the body only
uses:

- Typed values and operations on them
- Calls to other `extern fn` functions
- Rust standard library methods on typed values
- Rust-style closures (not `sub` closures)
- `match`, `if`/`else`, loops on typed values
- Regex via `=~` (lowers to `perl-regex` crate calls)
- `f"..."` format strings (lower to concatenation)
- `?` on `Result` (lowers to `Result::Err` propagation)

And does *not* use:

- `my`, `our`, `local` variables
- `sub` calls or `sub` closures
- Perl builtins (`print`, `chomp`, `split`, etc.)
- `eval` (requires the compiler-as-runtime-service)
- `$_` and other special variables
- Dynamic scope (`local`)
- Regex embedded code blocks `(?{ ... })` (these require a code host)
- Any operation that touches the interpreter `Heap`

**The practical implication is significant:**  You could write a Rust
library — a real, publishable-on-crates.io library — in this language,
using `extern fn` for the public API.  The `extern fn` functions
compile to normal Rust functions.  The library's `Cargo.toml` does not
need the Perl runtime as a dependency.  You get the language's
ergonomics for development and Rust's zero-overhead for deployment.

This also provides a concrete near-term target for AOT compilation.
Instead of trying to AOT-compile all of Perl (which requires solving
`eval STRING` and `BEGIN`-time execution), compile `extern fn`
functions to native code.  That's tractable because they are already
constrained to a Rust-compatible subset.

#### 14.14.1 AOT compilation mode: emit Rust source code.

Rather than building a custom Cranelift or LLVM backend, the AOT
compiler emits `.rs` files and lets `cargo` handle optimization,
linking, platform targeting, and cross-compilation.

This leverages the entire Rust toolchain for free — `-O3`
optimization, LTO, target-specific codegen, PGO — and produces
inspectable output that a developer can read, understand, and even
modify.

The compilation pipeline:

```text
  .pm / .pl source
       │
       ▼
  lex → parse → AST → HIR → IR
       │
       ▼
  Rust code emitter
       │
       ▼
  generated/
      Cargo.toml
      src/
          lib.rs          # module structure
          config.rs       # from Config.pm
          server.rs       # from Server.pm
          types.rs        # struct/enum definitions
       │
       ▼
  cargo build --release
       │
       ▼
  native binary / .so / .dylib
```

#### 14.14.2 Typed code emits directly to Rust:

Fully typed code with `struct`, `enum`, `impl`, `trait`, `fn`, and
`extern fn` maps 1:1 to Rust source.  The generated code is clean
and idiomatic:

```perl
# Perl source
struct User {
    name: String,
    age: i64,
}

impl User {
    fn new(name: String, age: i64) -> User {
        User { name, age }
    }

    fn display(&self) -> String {
        f"{self.name} (age {self.age})"
    }
}

extern fn create_user(name: &str, age: i64) -> User {
    User::new(name.to_string(), age)
}
```

```rust
// Generated Rust — clean, idiomatic, could pass for hand-written
pub struct User {
    pub name: String,
    pub age: i64,
}

impl User {
    pub fn new(name: String, age: i64) -> User {
        User { name, age }
    }

    pub fn display(&self) -> String {
        format!("{} (age {})", self.name, self.age)
    }
}

pub fn create_user(name: &str, age: i64) -> User {
    User::new(name.to_string(), age)
}
```

#### 14.14.3 Mixed typed/untyped code calls the runtime:

Code that mixes `let`/`fn` with `my`/`sub` generates Rust code that
depends on the `perl-runtime` crate for the untyped portions:

```rust
// Generated Rust — mixed code
use perl_runtime::{Interpreter, Value};

pub fn process_data(interp: &mut Interpreter, data: &[i64]) -> String {
    // Typed portion — direct Rust
    let total: i64 = data.iter().sum();

    // Untyped portion — calls through the runtime
    let formatted = interp.call_sub(
        "format_report",
        &[Value::Int(total)],
    );

    formatted.to_string()
}
```

#### 14.14.4 The generated crate structure:

```toml
# generated/Cargo.toml
[package]
name = "myapp"
version = "0.1.0"
edition = "2021"

[dependencies]
perl-runtime = "0.1"     # only if mixed code uses untyped features
perl-regex = "0.1"       # only if =~ is used
tokio = { version = "1", features = ["full"] }  # only if async is used
```

Dependencies are included only when needed.  A fully-typed library
with only `extern fn` functions has no `perl-runtime` dependency at
all — it's a pure Rust crate, publishable on crates.io.

#### 14.14.5 Sync/async monomorphization in generated code:

As described in §13.7, functions that transitively reach IO get both
sync and async variants emitted:

```rust
// Generated from a single fn definition
pub fn fetch_page(url: &str) -> Result<String, Error> {
    reqwest::blocking::get(url)?.text().map_err(Into::into)
}

pub async fn fetch_page_async(url: &str) -> Result<String, Error> {
    reqwest::get(url).await?.text().await.map_err(Into::into)
}
```

#### 14.14.6 Use cases for AOT-to-Rust:

- **Publish a Rust crate written in Perl syntax.**  `extern fn`
  public API, full Rust toolchain for distribution.
- **Optimize hot paths.**  Profile an interpreted application,
  identify the hot module, compile it to Rust, drop the `.rs` file
  into the project.
- **Cross-compile.**  `cargo build --target aarch64-unknown-linux-gnu`
  works on the generated code — cross-compilation for free.
- **Embed in a Rust application.**  The generated crate is a normal
  Rust dependency.  A Rust application `use`s it like any other crate.
- **Gradual migration.**  Start with interpreted Perl, compile hot
  modules to Rust one at a time, eventually the whole application is
  a Rust binary.

### 14.15 User-Defined Types: `struct`, `enum`, `impl`, `trait`

The typed layer lets you *use* existing Rust types (`String`, `Vec`,
`HashMap`), but real programs need to *define* new types.  Without
user-defined types, every composite value falls back to untyped
hashes.  `struct`, `enum`, `impl`, and `trait` close this gap.

This is not an OOP system.  It does not participate in `bless`,
`@ISA`, or method resolution order.  It is Rust's type definition
model — typed data containers with associated functions — brought
into Perl alongside `let` and `fn`.

#### 14.15.1 `struct` — typed data containers:

```perl
struct User {
    name: String,
    age: i64,
    email: Option<String>,
}

let $user = User { name: "Alice", age: 30, email: None };
print $user.name;              # "Alice"
$user.email = Some("alice@example.com");
```

Fields have declared types and are accessed by name — no hash key
typos, no runtime "key doesn't exist" errors, no `Can't locate
object method` surprises.  The compiler knows the layout at compile
time.

#### 14.15.2 `impl` — associated functions and methods:

```perl
impl User {
    fn new(name: String, age: i64) -> User {
        User { name, age, email: None }
    }

    fn display_name(&self) -> String {
        f"{self.name} (age {self.age})"
    }

    fn set_email(&mut self, email: String) {
        self.email = Some(email);
    }
}

let mut $user = User::new("Alice", 30);
print $user.display_name();         # "Alice (age 30)"
$user.set_email("alice@example.com");
```

`&self` and `&mut self` follow the same borrow rules as `fn`
parameters (§14.9) — `&self` is a read-only borrow, `&mut self`
is mutable.

#### 14.15.3 `enum` — algebraic data types:

```perl
enum Shape {
    Circle { radius: f64 },
    Rectangle { width: f64, height: f64 },
    Triangle { base: f64, height: f64 },
}

impl Shape {
    fn area(&self) -> f64 {
        match self {
            Shape::Circle { radius } =>
                3.14159 * radius * radius,
            Shape::Rectangle { width, height } =>
                width * height,
            Shape::Triangle { base, height } =>
                0.5 * base * height,
        }
    }
}

let $s = Shape::Circle { radius: 5.0 };
print $s.area();                     # 78.53975
```

Enum variants can hold data (struct-like variants), be unit variants
(`enum Color { Red, Green, Blue }`), or be tuple variants
(`enum Pair { Two(i64, i64) }`).  `match` on enums has
exhaustiveness checking — the compiler ensures every variant is
handled.

#### 14.15.4 `trait` — interfaces and generic programming:

```perl
trait Drawable {
    fn draw(&self, canvas: &mut Canvas);
    fn bounding_box(&self) -> Rect;
}

impl Drawable for Circle {
    fn draw(&self, canvas: &mut Canvas) {
        canvas.draw_circle(self.center, self.radius);
    }
    fn bounding_box(&self) -> Rect {
        Rect::from_center(self.center, self.radius * 2.0)
    }
}

impl Drawable for Rectangle {
    fn draw(&self, canvas: &mut Canvas) {
        canvas.draw_rect(self.origin, self.width, self.height);
    }
    fn bounding_box(&self) -> Rect {
        Rect::new(self.origin, self.width, self.height)
    }
}

# Dynamic dispatch via trait objects
fn render_all(items: &[&dyn Drawable], canvas: &mut Canvas) {
    for item in items {
        item.draw(canvas);
    }
}
```

Traits define a set of methods that types can implement.  This
enables polymorphism without inheritance — a `Circle` and a
`Rectangle` share no base class, but both satisfy `Drawable`.

#### 14.15.5 Interaction with the rest of the design:

- **Clone-on-assign (§14.9).**  `let $u2 = $user` clones the struct.
  For `Copy` types (fieldless enums, small structs with all-Copy
  fields), this is a bit copy.  For owned fields like `String`, it's
  a deep clone.
- **`\$x` creates `Arc` (§14.9).**  `\$user` upgrades the struct to
  `Arc<User>` — shared ownership via Perl reference syntax.
- **`fn` borrows (§14.9).**  `fn process(user: &User)` borrows the
  struct for the call duration — zero-copy, no `Arc`.
- **Concurrency.**  Structs are `Send + Sync` if all fields are.
  They work with `spawn` naturally.
- **`extern fn` (§14.14).**  Functions that take and return structs
  compile to standard Rust types — zero-cost FFI.
- **`match` (§15.2).**  Enum matching with exhaustiveness checking
  works naturally.

#### 14.15.6 Parallel to `bless`-based OOP, not a replacement:

`struct`/`impl` and `bless`/`@ISA` are parallel systems that do not
interact:

- `bless \%hash, 'Package'` — untyped, hash-based, full Perl 5 OOP
  compatibility, `@ISA` method resolution, `AUTOLOAD`, the whole
  inheritance machinery.
- `struct Foo { ... }` with `impl Foo { ... }` — typed,
  field-based, Rust model, no inheritance, no `@ISA`.

A struct is not a blessed hash.  It doesn't have a stash.  It
doesn't participate in method resolution.  If someone wants to bridge
the two worlds, they can write conversion methods — but the type
system doesn't pretend they're the same thing.

#### 14.15.7 Backward compatibility:

`struct`, `enum`, `impl`, and `trait` are registered as keywords via
`PL_keyword_plugin` in `use Typed`.  On standard Perl 5, a `struct`
lowers to a blessed arrayref with generated accessor subs.  An `enum`
lowers to a set of constructor functions and a dispatch table.
`impl` methods become subs in the struct's package.  `trait` becomes
a protocol enforced at compile time.

### 14.16 Scope and Evolution

Typed `let` declarations are available in any Perl code — no pragma
or mode switch is required.  The `let` keyword is not a Perl 5
keyword, so its introduction has zero backward compatibility impact.
Existing Perl 5 code continues to work unchanged; typed variables are
adopted incrementally at the programmer's discretion.

The design principle is: **expose Rust types and semantics that provide
concrete performance, safety, or FFI advantages.**  The scope covers
primitives, strings, smart pointers, collections, `Option`, `Result`,
clone-on-assign ownership, `Arc` via `\$x`, `fn`-parameter borrows,
immutable-by-default bindings, user-defined `struct`/`enum`/`impl`/
`trait`, and `extern fn` for standalone Rust-compatible functions.

Future extensions that may be added when justified by real usage:

- Explicit lifetime parameters for advanced borrow patterns
- Generic type parameters on user-defined structs and traits
- `async` trait methods
- Derive macros (`#[derive(Clone, Debug)]`)
- Associated types and constants in traits

Each of these should be added only when a concrete use case demands it,
not speculatively.

---

## 15. Rust Syntax Integration

Three areas of Rust syntax integrate directly into Perl code: `let`
destructuring with tuples, `match` expressions, and Rust-style
closures.  These are not cosmetic alternatives to existing Perl
syntax — each brings genuine new capability that Perl 5 lacks.

### 15.1 `let` Destructuring and Tuples

Perl's list assignment (`my ($a, $b) = @list`) is untyped and relies
on list flattening.  Typed `let` destructuring gives fixed-size,
heterogeneous tuples where each element has its own type:

```perl
# Typed tuple destructuring
let (name, age): (String, i64) = get_person();
let (min, max): (i64, i64) = (0, 100);

# Nested destructuring
let (city, (lat, lon)): (String, (f64, f64)) = get_location();

# Discard elements with _
let (name, _): (String, _) = get_person();

# Tuple as a return type
fn bounds() -> (i64, i64) {
    return (0, 100);
}

# Destructuring in let mut
let (mut lo, mut hi): (i64, i64) = (0, 100);
lo += 1;    # fine — lo declared mut
hi -= 1;    # fine — hi declared mut
```

This gives Perl something it has never had: multi-return with type
safety.  Currently `return ($a, $b)` flattens into a list and the
caller just hopes they destructure correctly.  Typed tuples make the
contract explicit and compiler-checked.

Tuple types can also appear inline:

```perl
let pair: (i64, String) = (42, "hello");
let entries: Vec<(String, i64)> = (("Alice", 95), ("Bob", 87));
```

### 15.2 `match` Expressions

Perl's `given`/`when` was experimental, poorly specified, and
effectively removed.  Rust's `match` is well-defined, exhaustive,
and works as an expression.

#### 15.2.1 Basic matching:

```perl
let label: &str = match status {
    200       => "ok",
    301 | 302 => "redirect",
    404       => "not found",
    500..=599 => "server error",
    _         => "unknown",
};
```

#### 15.2.2 Option and Result (exhaustiveness-checked):

```perl
let input: Option<String> = get_input();
match input {
    Some(text) => process(text),
    None       => warn "no input\n",
}
# compile error if you omit a branch
```

```perl
match read_config("/etc/app.conf") {
    Ok(cfg)  => use_config(cfg),
    Err(msg) => die f"Fatal: {msg}\n",
}
```

#### 15.2.3 Guard clauses:

```perl
let category: &str = match age {
    0           => "newborn",
    1..=12      => "child",
    13..=19     => "teenager",
    n if n >= 65 => "senior",
    _           => "adult",
};
```

#### 15.2.4 Tuple and struct destructuring:

```perl
match point {
    (0, 0)     => "origin",
    (0, y)     => f"y-axis at {y}",
    (x, 0)     => f"x-axis at {x}",
    (x, y)     => f"({x}, {y})",
}
```

#### 15.2.5 `match` on untyped values:

`match` should work on both typed and untyped values.  When matching
an untyped `my $x`:

- Numeric literals do numeric comparison.
- String literals do string comparison.
- Regex patterns do regex matching.
- `undef` matches `undef`.
- `_` is the wildcard.

```perl
my $input = get_user_input();
match $input {
    /^\d+$/   => process_number($input),
    /^quit$/i => exit(0),
    undef     => warn "no input\n",
    _         => process_text($input),
}
```

When matching a typed value, the compiler enforces exhaustiveness on
closed types (`Option`, `Result`, future user-defined enums) and
requires `_` for open types (integers, strings).

#### 15.2.6 Statement vs expression form:

`match` is an expression — it returns a value.  It can also be used
as a statement (where the return value is discarded).  Arms use `=>`
followed by either a single expression or a block:

```perl
match cmd {
    "start" => start_server(),
    "stop"  => {
        shutdown();
        cleanup();
    },
    _ => die f"unknown command: {cmd}\n",
}
```

### 15.3 Rust-Style Closures

Perl has `sub { ... }` for anonymous subs.  Rust-style `|...| { ... }`
closures add three things: typed parameters without boilerplate,
concise syntax for functional chains, and explicit capture semantics.

**Parsing:**  `|` is bitwise OR in Perl, always binary.  In term
position (after `=`, `,`, `(`, or anywhere the parser expects an
operand), there is no left operand, so `|` unambiguously opens a
closure parameter list.

#### 15.3.1 Basic syntax:

```perl
# Typed parameters, explicit return type
let double = |x: i64| -> i64 { x * 2 };
let add = |a: i64, b: i64| -> i64 { a + b };

# Expression body (no braces for single expression)
let double = |x: i64| x * 2;

# No parameters
let hello = || print "hello\n";

# Type inference from context
let doubled: Vec<i64> = numbers.iter().map(|x| x * 2).collect();
```

#### 15.3.2 Capture semantics:

This is the key architectural difference from `sub { ... }`.  Perl
anonymous subs capture the enclosing lexical pad by reference — the
entire pad stays alive as long as any closure referencing it exists.
This makes closures inherently non-`Send`, because the pad is tied
to the interpreter heap.

Typed closures have two capture modes:

```perl
let prefix: String = "Hello";

# Reference capture (default) — like Perl sub { }, captures by reference
let greet = |name: &str| -> String { f"{prefix}, {name}!" };
# prefix is referenced — closure is valid while prefix is in scope
# NOT Send — references the outer scope
print prefix;              # fine — prefix is still here

# Move capture — clones captured values into the closure
let greet = move |name: &str| -> String { f"{prefix}, {name}!" };
# prefix was cloned into the closure
# The closure owns its own copy and is Send
print prefix;              # fine — we still have our copy (clone-on-assign)
```

Note that `move` in this model means "clone into the closure," not
"transfer ownership" as in Rust.  This is consistent with the
clone-on-assign model (§14.9) — the outer variable remains accessible.

For `Arc` values, `move` clones the `Arc` (refcount bump), which is
the efficient way to share data with a spawned thread:

```perl
let $config = \("database.example.com");  # Arc<String>

spawn move || {
    print "Using $$config\n";   # Arc clone moved into closure
};

print $$config;                  # fine — we still have our Arc
```

`sub { ... }` continues to work as always for classic Perl closures.

#### 15.3.3 When to use which:

| Syntax | Typed | Captures | `Send` | Portable to Perl 5 |
|--------|-------|----------|--------|---------------------|
| `sub { ... }` | No | Pad reference | No | Yes |
| `fn($x: T) { ... }` | Yes | Reference | No | Yes (via `use Typed`) |
| `move fn($x: T) { ... }` | Yes | Clone | Yes | Yes (via `use Typed`) |
| `move \|x: T\| { ... }` | Yes | Clone | Yes | Yes (via `use Typed`) |
| `async \|x: T\| { ... }` | Yes | Clone | Yes | Yes (via `use Typed`) |
| `\|x: T\| { ... }` (bare) | Yes | Reference | No | With proposed core change (§15.4) |

#### 15.3.4 Keyword-triggered closure parsing:

Bare `|args| expr` without a keyword prefix cannot be implemented on
standard Perl 5 — `|` is the bitwise OR operator and neither the
keyword API nor `PL_infix_plugin` can change how it parses.

However, `move` and `async` are not Perl 5 keywords, so they can
be registered via `PL_keyword_plugin`.  Once the parser hits `move`,
the keyword hook takes over and can parse `|args| expr` freely
— the `|` is being interpreted by the custom parser at that
point, not by Perl's operator dispatch.  The standard parser never
sees it.

This means the concurrency-critical closure forms — `move` closures
for thread spawning and `async` closures for async tasks — are fully
portable to standard Perl 5:

```perl
# Portable — move keyword triggers our parser, |args| works
let $handler = move |x: i64| x * 2;
spawn move |$config: Arc<str>| {
    print "Connecting to $config\n";
};

# Portable — async keyword, same mechanism
let $fetcher = async |url: &str| {
    await fetch(url)
};

# Portable — anonymous fn
let $double = fn($x: i64) -> i64 { $x * 2 };

# NOT portable — bare |args| with no keyword prefix
let double = |x: i64| x * 2;
```

Anonymous `fn` is just `fn` without a name — it's a keyword, so the
pluggable keyword API can parse it.  On the Rust runtime, all forms
produce the same typed closure.  The `|args|` form is most concise
for iterator chains; `fn(args)` and `move |args|` are portable.

### 15.4 Bare `|args|` Closures on Standard Perl 5

Bare `|args| expr` closures (without a `move` or `async` keyword
prefix) are the one Rust syntax form that cannot be trivially
implemented on standard Perl 5.  This section analyzes the problem
in detail and proposes solutions.

#### 15.4.1 Why keyword-prefixed forms work.

`move |args| expr` and `async |args| expr` work on standard Perl 5
because `move` and `async` are registered via `PL_keyword_plugin`.
When the lexer encounters the keyword, it calls the plugin, which
takes over parsing entirely.  The plugin can then parse `|args| expr`
freely — the standard parser never sees the `|`.

#### 15.4.2 Why bare `|args|` is hard.

Without a keyword prefix, `|` hits the lexer's `yyl_verticalbar`
function, which unconditionally emits a `BITOROP` token.  The parser
then tries to parse it as an infix bitwise-or operator, expecting
a left-hand operand that doesn't exist (it's in term position).

#### 15.4.3 The near-miss: `PL_infix_plugin`.

Perl 5.38 introduced `PL_infix_plugin`, which fires *before* the
main `switch(*s)` dispatch in `yyl_try`.  The check at line 9659
of `toke.c`:

```c
if(PLUGINFIX_IS_ENABLED && isPLUGINFIX_FIRST(*s)) {
    // ... calls PL_infix_plugin
}
```

`isPLUGINFIX_FIRST('|')` is true (it matches any non-space, non-digit,
non-alpha, non-quote character).  So a plugin *can* intercept `|`
before it becomes `BITOROP`.

The problem is what happens next.  The plugin path unconditionally
emits `OPERATOR(tokentype_for_plugop(def))`, which sets
`PL_expect = XTERM` and tells the parser to expect an infix operator.
There is no way for the plugin to emit a `TERM` or `PLUGEXPR` instead.

This is exactly the same problem that `/` has (division vs regex), and
`toke.c` solves it for `/` by checking `PL_expect` in `yyl_slash`:

```c
if (PL_expect == XOPERATOR) {
    Mop(OP_DIVIDE);          // infix division
} else {
    s = scan_pat(s, OP_MATCH);
    TERM(sublex_start());    // term: regex literal
}
```

The infrastructure for context-dependent term/operator disambiguation
exists.  It just isn't wired up to the infix plugin mechanism.

#### 15.4.4 Proposed core change (preferred).

A small extension to `PL_infix_plugin` / `struct Perl_custom_infix`
that adds a term-producing path.  Roughly 5-10 lines in `toke.c`:

```c
// In the PLUGINFIX handler, after the plugin matches:
if (def->flags & INFIX_FLAG_TERM_PREFIX && PL_expect != XOPERATOR) {
    // term position — let the plugin build a term op
    OP *term_op = def->new_term_op(aTHX, result->parsedata, def);
    pl_yylval.opval = term_op;
    TERM(PLUGEXPR);
} else {
    // operator position — standard infix path
    pl_yylval.pval = result;
    OPERATOR(tokentype_for_plugop(def));
}
```

This adds a flag (`INFIX_FLAG_TERM_PREFIX`) and a callback
(`new_term_op`) to the `Perl_custom_infix` struct.  When the flag
is set and the lexer is in term position, the plugin produces a
`PLUGEXPR` term instead of an infix operator.  In operator position,
`|` behaves as bitwise-or as always.

This is a natural extension of the existing framework — the
term/operator distinction is already how `yyl_slash` works, and
`PLUGEXPR` already exists in the grammar for exactly this purpose.
It would benefit not just closure syntax but any situation where a
symbolic character needs to sometimes be a term-producing prefix
(e.g., a hypothetical prefix `~` operator for string interpolation,
or a prefix `#` for tuple construction).

This could be proposed to Paul Evans as an extension to
`XS::Parse::Infix`, or directly to perl5-porters as a small
enhancement to the `PL_infix_plugin` mechanism.

#### 15.4.5 Alternative A: Buffer rewrite via `lex_unstuff` + `lex_stuff_pvn`.

Without core changes, the most promising approach uses Perl's lexer
buffer manipulation API.  When `PL_infix_plugin` detects `|` and
determines it is in term position (by checking `PL_expect`):

1. Return 0 from `PL_infix_plugin` (decline the infix match).
2. Before returning, scan ahead through the buffer manually to
   locate the matching `|` and the expression body.
3. Use `lex_unstuff` to remove `|args| expr` from the buffer.
4. Use `lex_stuff_pvn` to inject replacement text like
   `__typed_closure(args) { expr }` into the buffer.
5. The lexer re-scans and hits `__typed_closure`, which is registered
   as a keyword via `PL_keyword_plugin` and handles parsing normally.

The difficulty is that `PL_infix_plugin` runs inside `yyl_try` where
`PL_bufptr` may not be in the expected state for `lex_unstuff`.  The
buffer manipulation may need to happen in a wrapper around
`PL_infix_plugin` that carefully manages buffer pointers, or in a
`PL_check` hook that fires after tokenization.

This approach is fragile — it is essentially a targeted source rewrite
performed during lexing.  It must correctly handle nested `|` operators,
multiline expressions, and heredocs.  It is viable as a proof of
concept but may have edge cases.

#### 15.4.6 Alternative B: Overriding `yyl_verticalbar` via XS.

The `yyl_verticalbar` function is `static` in `toke.c`, so it cannot
be directly replaced.  However, using `Devel::CallChecker`-style XS
tricks or by patching the function pointer table (if Perl exposes one
for lexer functions), it might be possible to intercept the `case '|'`
dispatch.  In practice, no clean API exists for this, so this approach
requires modifying Perl's compiled C code at load time — fragile and
non-portable.

#### 15.4.7 Alternative C: Accept the limitation gracefully.

The bare `|args|` form is a conciseness convenience, not a capability
gap.  All the important closure forms are portable:

- `move |args| expr` — portable via keyword plugin
- `async |args| expr` — portable via keyword plugin
- `fn(args) { expr }` — portable via keyword plugin
- `sub { ... }` — native Perl 5

The only non-portable form is bare `|args|` for iterator chains like
`.map(|x| x * 2)`.  On standard Perl 5 via `use Typed`, these use
anonymous `fn` instead: `.map(fn($x) { $x * 2 })`.  This is slightly
more verbose but semantically identical.

#### 15.4.8 Recommendation.

Pursue the proposed core change (it is small, clean, and broadly
useful).  If it is accepted, bare `|args|` becomes portable on
Perl 5.38+ via `use Typed`.  If it is not accepted, alternative C
(accept the limitation) is the pragmatic choice — the anonymous `fn`
form covers the use case adequately, and `move`/`async` closures
already work.

#### 15.4.9 Interaction with Perl builtins:

Rust closures should work with Perl builtins that take code blocks:

```perl
let numbers: Vec<i64> = (1, 2, 3, 4, 5);

# map/grep/sort with typed closures
let evens: Vec<i64> = grep |n: i64| n % 2 == 0, numbers;
let doubled: Vec<i64> = map |n: i64| n * 2, numbers;
let sorted: Vec<i64> = sort |a: i64, b: i64| a <=> b, numbers;

# vs classic Perl (still works)
my @evens = grep { $_ % 2 == 0 } @numbers;
```

---

## 16. Backward Compatibility with Standard Perl 5

### 16.1 The Typed Layer as a CPAN Module

The typed syntax extensions (`let`, `fn`, `match`, and anonymous `fn`
closures) can be made available on standard Perl 5 via a CPAN module
that uses Perl's pluggable keyword API and (on 5.38+) the custom
infix operator mechanism.  No transpiler, no build step, no separate
toolchain:

```perl
use Typed;   # registers let, fn, match as keywords

let $name: String = "hello";
fn greet($who: &str) -> String { "Hello, $who!" }
let $double = fn($x: i64) -> i64 { $x * 2 };
```

This works because Perl 5.12 introduced a pluggable keyword mechanism
(`PL_keyword_plugin`) that lets modules register new keywords with
custom parsing logic.  It is how `Syntax::Keyword::Try`,
`Object::Pad`, and similar modules already add new syntax to standard
Perl 5.  Perl 5.38 additionally introduced `PL_infix_plugin` for
registering custom infix operators.

The `use Typed` module would:

1. Register `let`, `fn`, `extern`, `move`, `async`, `struct`,
   `enum`, `impl`, `trait`, `result`, `try`, `catch`, and `finally`
   as keywords via the keyword API.
2. Register `match` as a keyword (see compatibility note below).
3. Parse their typed syntax at compile time using the keyword parser
   hooks.
4. Perform compile-time type checking, immutability enforcement, and
   exhaustiveness checking.
5. Emit standard Perl 5 ops for runtime execution.

The runtime behavior is Perl 5, but the compile-time checking is real.

#### 16.1.1 `match` keyword and `Syntax::Keyword::Match`:

`Syntax::Keyword::Match` (by Paul Evans) already registers `match` as
a keyword, with syntax `match($expr : op) { case(val) { ... } }`.
The `match` implementation uses Rust-style arm syntax: `match $expr { val => arm }`.
The two syntaxes are distinguishable in parsing (the `: op` after the
expression is unique to `Syntax::Keyword::Match`), but since both
register the same keyword, they cannot be loaded simultaneously.

Options include: coordinating with Paul Evans on a unified design,
having the module subsume `Syntax::Keyword::Match`'s functionality as
a subset (the `match` implementation on untyped values could support
the `: op` syntax), or accepting that `use Typed` and
`use Syntax::Keyword::Match` are alternatives.  This should be
resolved before the CPAN module is published.

#### 16.1.2 Rust-style `|args| expr` closures — partially portable:

Bare `|args| expr` without a keyword prefix cannot currently be
implemented on standard Perl 5.  A small proposed extension to Perl's
`PL_infix_plugin` mechanism would make it possible on 5.38+ — see
§15.4 for detailed analysis and the proposed core change.

However, `move |args| expr` and `async |args| expr` *are* portable
today, because `move` and `async` are not Perl 5 keywords.  When
registered via `PL_keyword_plugin`, they trigger the custom parser
which then handles the `|...|` syntax entirely within the parsing
hook — the standard Perl parser never sees the `|`.  This means the
concurrency-critical closure forms are fully portable.  Anonymous
`fn(args) { expr }` covers the remaining cases.

### 16.2 What Each Keyword Compiles To

#### 16.2.1 `let` declarations:

```perl
# Source:
let $name: String = "hello";         # immutable
let mut $count: i64 = 0;             # mutable

# Compiles to:
my $name = "hello";
Internals::SvREADONLY($name, 1);    # enforce immutability at runtime
my $count = 0;                       # mutable — no READONLY flag
```

#### 16.2.2 `fn` declarations:

```perl
# Source:
fn greet($who: &str) -> String {
    "Hello, $who!"
}

# Compiles to:
sub greet {
    # In dev mode: runtime type checks
    Typed::_check_args(\@_, ['Str']);
    my ($who) = @_;
    my $_ret = do { "Hello, $who!" };
    Typed::_check_return($_ret, 'Str');
    $_ret
}
```

#### 16.2.3 `match` expressions:

```perl
# Source:
match $status {
    200       => "ok",
    404       => "not found",
    _         => "unknown",
}

# Compiles to:
do {
    if ($status == 200)    { "ok" }
    elsif ($status == 404) { "not found" }
    else                   { "unknown" }
}
```

#### 16.2.4 Anonymous `fn` closures:

```perl
# Source:
let $double = fn($x: i64) -> i64 { $x * 2 };

# Compiles to:
my $double = sub {
    Typed::_check_args(\@_, ['Int']);
    my ($x) = @_;
    $x * 2
};
Internals::SvREADONLY($double, 1);
```

#### 16.2.5 `move` and `async` closures with `|args|` syntax:

```perl
# Source:
let $handler = move |$x: i64| $x * 2;

# Compiles to (move keyword triggers our parser for |...|):
my $handler = sub {
    Typed::_check_args(\@_, ['Int']);
    my ($x) = @_;
    $x * 2
};
Internals::SvREADONLY($handler, 1);
```

The `move` and `async` keywords are registered via `PL_keyword_plugin`.
When triggered, the custom parser handles the `|...|` syntax — the
standard Perl parser never sees the `|`.  Bare `|args| expr` without
a keyword prefix remains Rust-runtime-only.

#### 16.2.6 `Result` and `?` operator:

```perl
# Source:
fn init() -> Result<String, String> {
    let $cfg = read_config("/etc/app.conf")?;
    Ok($cfg)
}

# Compiles to:
sub init {
    my $cfg = eval { read_config("/etc/app.conf") };
    if ($@) { return Typed::Err($@) }
    return Typed::Ok($cfg);
}
```

### 16.3 Graduated Runtime Checking

Runtime type checks have overhead.  The module should support
graduated enforcement:

```perl
use Typed;                    # full checking — dev mode
use Typed -checks => 'warn';  # type mismatches warn, don't die
use Typed -checks => 'none';  # no runtime checks — production mode
```

Even with `-checks => 'none'`, the compile-time type validation from
the keyword parser still runs.  You get the diagnostics during
development and zero overhead in production.

### 16.4 The `$`-Aliased Subset

Backward compatibility works most naturally with the `$`-aliased
style (`let $name`, `fn greet($who: &str)`), because these map
directly to Perl 5's `my $name` and `sub greet { my ($who) = @_ }`.

Sigil-less variables (`let name: String`), Rust-style closures
(`|x| x * 2`), and `f"..."` format strings have no Perl 5 equivalent
and cannot be supported by the CPAN module.  This gives a clean
style guideline:

| Style | Runs on standard Perl 5 | Runs on Rust runtime |
|-------|------------------------|---------------------|
| `my $x` / `sub foo { }` | Yes | Yes |
| `let $x: Type` / `fn foo($x: Type)` | Yes (with `use Typed`) | Yes |
| `fn($x: Type) { }` (anonymous) | Yes (with `use Typed`) | Yes |
| `struct` / `enum` / `impl` / `trait` | Yes (with `use Typed`) | Yes |
| `move \|x: Type\| expr` closures | Yes (with `use Typed`) | Yes |
| `async \|x: Type\| expr` closures | Yes (with `use Typed`) | Yes |
| `\|x: Type\| expr` (bare, no keyword) | With proposed core change (§15.4) | Yes |
| `let x: Type` / `fn foo(x: Type)` | No | Yes |
| `f"..."` format strings | No | Yes |
| `extern fn foo(x: Type)` | No | Yes (standalone) |

Each tier is a superset of the one above.  A team can start at the
second tier — typed code with `$` aliases that runs on both standard
Perl 5 and the Rust runtime — and graduate to sigil-less or `extern fn`
only when they're ready to commit to the Rust runtime.

### 16.5 Adoption Strategy

This creates a zero-lock-in adoption path:

1. `cpanm Typed` on any Perl 5.12+ system.
2. Add `use Typed;` to a file.
3. Start using `let`, `fn`, `match` incrementally — existing code
   is untouched.
4. Get compile-time type checking on standard Perl 5.
5. Optionally deploy on the Rust runtime later for performance,
   `extern fn` support, sigil-less variables, and full Rust interop.

The CPAN module should be developed alongside the Rust implementation,
sharing the type-checking logic (or at least the type-checking rules)
to ensure the two runtimes agree on what is valid.

---

## 17. Extension and FFI

### 17.1 The Native Extension API

The primary extension mechanism should be Rust-native, with a clean trait
interface:

```rust
trait PerlExtension {
    fn name(&self) -> &str;
    fn init(&self, interp: &mut Interpreter) -> Result<()>;
}
```

Extensions register functions, create packages, and interact with the
heap through safe Rust APIs.  No raw pointer manipulation needed.

When extension functions use typed parameters (`&str`, `i64`, `f64`,
`bool`, `Bytes`), calls from typed Perl values cross the boundary at
zero cost (see §14.13).  Calls from untyped values pay the coercion
cost automatically.

### 17.2 C FFI

For calling C libraries from Perl, provide a mechanism analogous to
Perl's `FFI::Platypus` that uses Rust's `libffi` bindings.  This
avoids the need for XS glue code entirely.

### 17.3 XS Compatibility (Deferred)

Full XS compatibility requires emulating Perl 5's ABI: the SV/AV/HV
memory layout, the stack macros (`dSP`, `PUSHMARK`, `EXTEND`, `PUSHs`,
etc.), and the calling conventions.  This is a massive effort.

Recommendation: defer XS compatibility.  Instead, provide:

1. The native Rust extension API (for new extensions).
2. C FFI support (for calling C libraries without XS).
3. A "thin XS shim" later that translates the most common XS patterns
   to native API calls, covering a significant fraction of CPAN's
   XS modules without full ABI emulation.

The thin shim would cover: simple scalar/list argument passing, return
values, basic SV manipulation (get/set string/int/float), and hash/array
access.  It would not cover: direct SV pointer manipulation, custom magic
installation via XS, or AV/HV internals access.

---

## 18. Source Filters

Source filters (`Filter::Simple`, `Filter::Util::Call`) allow modules to
transform source text before the lexer sees it.  They are a Perl 5
feature that some CPAN modules depend on.

Implementation: when a source filter is installed (via `use` importing a
filtering module), the lexer's source-reading layer passes text through
the filter chain before feeding it to the tokenizer.  This should be a
composable pipeline:

```text
Raw source ──► Filter 1 ──► Filter 2 ──► ... ──► Lexer
```

Source filters are rare in modern Perl code and should have zero overhead
when not in use (check a flag; if no filters are installed, bypass the
pipeline entirely).

---

## 19. Error Handling, Diagnostics, and Debugging

### 19.1 Source Spans

Every token, AST node, and IR instruction should carry a source span
(`Span { file, start_byte, end_byte }`).  This enables precise error
messages at every compilation and execution stage.

### 19.2 Error Representation

Compile-time and runtime errors should be a single error type that
carries:

- Span (where in source the error occurred)
- Category (syntax, type, runtime, etc.)
- Severity (error, warning, note)
- Message (human-readable)
- Suggestions (when applicable)
- Chain (underlying cause, for error chains)

### 19.3 Warnings

Perl's `warnings` pragma is lexically scoped.  The implementation should
carry a warnings bitmask in the compilation context (alongside hints),
and the runtime should check the active warnings state before emitting
a warning.

### 19.4 Debugging and Introspection

Perl 5's debugging experience is widely acknowledged as inadequate.
`perl -d` is a line-mode debugger that most developers avoid.
`Data::Dumper` is ubiquitous but produces hard-to-read output.
Stack traces require `use Carp` or `Devel::Confess`.  There is no
standard debug protocol for IDE integration.  Introspecting a running
program — "what's in this object? what methods does it have?" — is
painful.

The architecture provides structural advantages that make better
debugging possible.  This section identifies the opportunities and
goals without prescribing detailed solutions — the debugging
experience is expected to evolve significantly during implementation.

#### 19.4.1 What the architecture gives us:

- **Source spans on everything.**  Every IR instruction maps back to
  a source span (§19.1).  Breakpoints, step-through, and "show me
  the source" are structurally supported.
- **Rich call frames.**  Every `CallFrame` has source location, the
  function name, and (for `fn`) the typed parameter signature.
  Stack traces are available by walking the call stack — no `Carp`
  required.
- **Typed values are self-describing.**  A `struct User { name:
  String, age: i64 }` knows its own field names and types at
  runtime.  Pretty-printing it is trivial.
- **Untyped values have observable state.**  `ScalarFlags` tells you
  which representations are cached (INT_VALID, STR_VALID, etc.),
  whether magic is attached, whether the value is read-only.
- **Async-aware.**  The runtime knows about all active Tokio tasks,
  their states, and their call stacks.

#### 19.4.2 Stack traces on exceptions by default:

Every `die` (and every `Err` auto-unwrap) should capture a stack
trace for the current task automatically.  This is the single most
impactful debugging improvement over Perl 5:

```text
Exception: Can't open /etc/missing.conf: No such file or directory

  at Config::load (lib/Config.pm:42)
  at Server::init (lib/Server.pm:15)
    fn init(config_path: &str) -> Result<Server, Error>
  at main (bin/app.pl:8)
```

Typed frames include the function signature.  Untyped frames show
what is available (function name, source location).  Stack trace
capture should be cheap enough to enable by default — the call stack
is already maintained for execution; formatting it into a trace is
the only additional cost, and that only happens on exceptions.

The default is a trace for the task that threw the exception — not
all tasks.  An all-task dump is a separate diagnostic operation
available on request for deadlock investigation and overall system
inspection.

#### 19.4.3 All-task dump — on request, not by default:

For debugging hangs, deadlocks, or unexpected behavior across tasks,
a full task dump shows every active interpreter's call stack and
current state:

```text
perl> :taskdump

Task 1 [running] — REPL
  at main (REPL:1)

Task 2 [waiting: TCP accept on :8080] — Server
  at Server::accept_loop (lib/Server.pm:88)
  at Server::run (lib/Server.pm:42)
    fn run(config: &Config) -> Result<(), Error>
  at main (bin/app.pl:12)

Task 3 [waiting: channel receive] — Worker
  at Worker::process (lib/Worker.pm:23)
  at Worker::run (lib/Worker.pm:8)
  at main (bin/app.pl:15)

Task 4 [waiting: timer, 3s remaining] — Monitor
  at Monitor::heartbeat (lib/Monitor.pm:45)
  at main (bin/app.pl:18)
```

For interpreted code, this is straightforward — each `Interpreter`
maintains its own call stack, and dumping all tasks walks all
interpreters.  For AOT-compiled async code, the `async-backtrace`
crate provides the same capability — the AOT compiler can annotate
generated async functions with `#[async_backtrace::framed]`
automatically, enabling `taskdump_tree` for suspended tasks that are
invisible to traditional stack traces.

#### 19.4.4 Built-in value inspection:

A `debug()` builtin (or `dd()` — "data dump") should produce
readable, structured output for any value type without requiring
a CPAN module:

```perl
# Untyped values
my %config = (host => "db.example.com", port => 5432);
dd(%config);
# {
#   host => "db.example.com",
#   port => 5432,
# }

# Typed values — includes type information
let $user = User::new("Alice", 30);
dd($user);
# User {
#   name: "Alice",
#   age:  30,
#   email: None,
# }

# Nested structures
dd(\@complex_data);
# [
#   { name => "Alice", scores => [95, 87, 92] },
#   { name => "Bob", scores => [88, 91, 76] },
# ]
```

The output format should be valid Perl syntax where possible (so
it can be copy-pasted into code), handle circular references
(printing `(circular ref)` or similar), and truncate very large
structures with a configurable depth limit.

#### 19.4.5 IDE integration via Debug Adapter Protocol (DAP):

The Debug Adapter Protocol is the standard interface used by VS Code,
Vim/Neovim (via plugins), Emacs, and other editors for debugger
integration.  Supporting DAP would give Perl developers:

- Breakpoints (line, conditional, function-entry)
- Step in / step over / step out
- Variable inspection with structured display
- Watch expressions
- Call stack navigation
- Async task listing

The IR + source span model provides the foundation.  The runtime
needs a debug server that speaks DAP and can pause execution, inspect
interpreter state, and resume.  This is a significant implementation
effort but the architecture supports it cleanly.

#### 19.4.6 Interactive REPL:

Perl has never had a proper REPL.  `perl -de0` abuses the debugger
as a substitute — no line editing, no history persistence, no syntax
highlighting, no tab completion, no multiline input handling.  Third-
party REPLs (`Reply`, `Devel::REPL`) exist but are not widely used
and have rough edges.

The REPL should be a first-class tool, built on `reedline` (the Rust
line-editor library used by Nushell), providing:

- **Line editing.**  Emacs and Vi keybinding modes, word-level
  navigation, kill ring, undo.
- **Persistent history.**  Saved across sessions, searchable with
  Ctrl-R, with deduplication.
- **Syntax highlighting.**  Perl keywords, strings, numbers,
  comments, variables, and operators colored in real time as you
  type.  Typed keywords (`let`, `fn`, `struct`) distinguished
  visually.
- **Tab completion.**  Variable names (`$`, `@`, `%` prefix-aware),
  function/method names, module names, hash keys for known hashes,
  struct field names for typed values, file paths for string
  arguments.
- **Multiline input.**  Automatic continuation when a block, string,
  or expression is incomplete.  Visual indent to show nesting depth.
  Cancel with Ctrl-C to abandon, not to exit.
- **Result display.**  Every expression's result is printed
  automatically (like `irb`, `python`, `node`), using the `dd()`
  formatter for structured output.  Explicit `print`/`say` also
  works as always.
- **Lexical persistence.**  Variables declared with `my` or `let`
  persist across REPL lines within a session.  This is unlike
  `perl -de0` where `my` variables evaporate between eval'd lines.

```
perl> my @primes = (2, 3, 5, 7, 11);
perl> let $sum: i64 = @primes.iter().sum();
55
perl> push @primes, 13;
perl> @primes
[2, 3, 5, 7, 11, 13]
perl> sub double { $_[0] * 2 }
perl> double(21)
42
```

#### 19.4.7 Introspection commands:

Prefixed with `:` to distinguish from Perl code:

```
perl> my $obj = Foo->new(42);
perl> :type $obj
Foo (blessed hashref)
  {value => 42, label => "default"}

perl> :methods Foo
Foo: new, value, label, to_string
  ISA: Base: serialize, deserialize

perl> :flags $obj
ScalarFlags: STR_VALID | REF_VALID | MAGICAL
  blessed into: Foo
  magic: TIEDSCALAR

perl> :tasks
ID  State     Location                 Waiting On
1   running   REPL                     —
2   waiting   lib/Server.pm:88         TCP accept on :8080
3   sleeping  lib/Monitor.pm:45        timer (5s remaining)

perl> :stack 2
  at Server::accept_loop (lib/Server.pm:88)
  at Server::run (lib/Server.pm:42)
  at main (bin/app.pl:12)

perl> :load lib/Config.pm
Loaded Config (3 subs exported)

perl> :time { slow_function() }
Elapsed: 1.342s
```

#### 19.4.8 REPL as a Tokio task:

The REPL itself runs as a Tokio task, meaning `spawn` works inside
it — you can start background tasks from the REPL and interact
with them while they run:

```
perl> my ($tx, $rx) = channel();
perl> spawn { for my $i (1..5) { sleep 1; print $tx "$i\n" } close $tx }
perl> <$rx>
1
perl> <$rx>
2
perl> # ... other tasks running in the background
```

This also means the REPL can load and interact with async servers,
database connections, and other long-lived concurrent code
interactively — a significant advantage for development and
debugging.

#### 19.4.9 Profiling hooks in the IR:

Rather than bolting on profiling after the fact (as NYTProf does by
hooking into the Perl interpreter at the C level), the IR can include
optional instrumentation points that activate when profiling is
enabled:

- Per-function entry/exit timing
- Per-line execution counts
- Allocation tracking (how many `Arc`s created, how many upgraded
  from compact to full Scalar)
- Lock contention tracking (which values are contended, how long
  waits take)
- Async task lifecycle events

When profiling is disabled, these points compile to no-ops — zero
overhead.  When enabled, they feed into a profiling data structure
that can be dumped to a file (compatible with speedscope, flamegraph,
or a custom viewer) or streamed to a live monitoring tool.

#### 19.4.10 Design principle:

The specifics of the debugging interface will evolve during
implementation.  What the design document commits to is:

1. Stack traces on exceptions are on by default for the current task.
2. All-task dumps are available on request for system-wide debugging.
3. A built-in value inspector (`dd()`) is available without CPAN.
4. A first-class interactive REPL with line editing, syntax
   highlighting, tab completion, and introspection commands — not
   `perl -de0`.
5. The IR and call frame structure support external debuggers.
6. Profiling instrumentation is built into the IR, not bolted on.
7. Async tasks are first-class in all debugging and introspection
   tools — `async-backtrace` for AOT code, interpreter call stacks
   for interpreted code.

---

## 20. Testing Strategy

### 20.1 Upstream Test Suite as Oracle

The Perl 5 `t/` directory is the primary test oracle.  Progress is
measured by how many upstream `.t` files pass.

### 20.2 Phased Test Progression

#### 20.2.1 Phase 1: Lexer/parser foundations
- `t/base/lex.t` — lexer basics
- `t/base/cond.t` — conditionals
- `t/base/if.t` — if/elsif/else
- `t/base/pat.t` — basic patterns
- `t/base/term.t` — basic terms

#### 20.2.2 Phase 2: Core semantics
- `t/op/arith.t` — arithmetic
- `t/op/string.t` — string operations
- `t/op/cond.t` — conditional expressions
- `t/op/assignop.t` — assignment operators
- `t/op/array.t`, `t/op/hash.t` — data structures
- `t/op/sub.t`, `t/op/closure.t` — subroutines and closures

#### 20.2.3 Phase 3: Advanced features
- `t/op/re_tests` — regex
- `t/op/heredoc.t` — heredocs
- `t/op/subst.t` — substitution
- `t/op/eval.t` — eval
- `t/comp/use.t` — use/require
- `t/op/tie.t` — tied variables

#### 20.2.4 Phase 4: Module ecosystem
- Core module tests
- Selected CPAN module tests
- Smoker-style automated test runs

### 20.3 Rust Unit Tests

Each subsystem (lexer, parser, lowering, IR, runtime, regex engine) should
also have its own Rust-level unit and integration tests that do not depend
on the Perl test harness.  These are faster to run and easier to debug
than end-to-end `.t` tests.

---

## 21. Implementation Order

This is a recommended sequence of implementation work, ordered to
maximize the ratio of "useful progress" to "infrastructure investment"
at each step.

### 21.1 Value model (2-3 weeks)

**This is the foundation everything else stands on.**

Build the `perl-core` crate first.  Every other crate depends on
it, and design mistakes here are the most expensive to fix later.

#### 21.1.1 Week 1: Strings and core value types

1. `PerlString` — `Bytes` + `flags: u8` (Perl UTF-8 flag, Rust
   validity cache).  Methods: `as_str()`, `from_str()`, `as_bytes()`,
   `clone_cow()`, concatenation, comparison, `substr`, `length`
   (byte and character).  Extensive tests for flag maintenance across
   mutation and COW transitions.

2. `SmallString` — 38-byte inline string.  Construction, conversion
   to/from `PerlString`, boundary behavior at exactly 38 bytes.

3. `ScalarFlags` — bitflags struct with INT_VALID, NUM_VALID, STR_VALID,
   REF_VALID, READONLY, UTF8, MAGICAL, TAINT, WEAK.

4. `PerlStringSlot` — the `None`/`Inline`/`Heap` enum.

#### 21.1.2 Week 2: `Scalar`, `Value`, and coercion

5. `Scalar` — full struct with `int`, `num`, `bytes`, `reference`, `magic`,
   `stash`.  Coercion methods: `get_int()`, `get_num()`,
   `get_bytes()`, `get_str()`, `set_int()`, `set_str()`, `set_ref()`.
   Each respects flag discipline — reads check the validity flag
   first, writes invalidate other caches.

6. `Value` — the enum with compact variants (`Undef`, `Int`,
   `Float`, `SmallStr`, `Str`, `Ref`) and `Scalar(Sv)`.
   Upgrade logic: triggers for multi-rep, reference-taking,
   magic attachment.  Once upgraded, never downgrade.

7. `Sv`, `Av`, `Hv` type aliases — `Arc<RwLock<Scalar>>`, etc.
   Basic operations: clone (Arc refcount bump), read through
   RwLock, write through RwLock.

#### 21.1.3 Week 3: References, cycle collection, mortal stack

8. Reference creation — `\$x` upgrades compact Value to
   `Scalar(Sv)`, returns `Ref(Sv)` pointing to the same Arc.
   Dereference in both directions.

9. Cycle collection scaffolding — candidate set, `weaken` support,
   Bacon-Rajan trial deletion.  Can be a stub initially, but the
   hooks for tracking candidates should be in place.

10. Mortal stack — `Vec<Value>` per interpreter, scope-entry marks,
    scope-exit drops.

11. Task-local `local` mechanism — `Option<Box<LocalStack>>` with
    `WasInactive`/`WasActive` save stack.  Test with simulated
    scope entry/exit.

Each of these is independently testable with `#[test]` functions.
No lexer, no parser, no interpreter — just data structures and
their operations:

```rust
#[test]
fn string_coercion() {
    let mut sv = Scalar::from_str("42");
    assert!(sv.flags.contains(ScalarFlags::STR_VALID));
    assert_eq!(sv.get_int(), 42);
    assert!(sv.flags.contains(ScalarFlags::INT_VALID));
}

#[test]
fn compact_value_upgrade() {
    let val = Value::Int(42);
    let sv = val.to_scalar();  // upgrade
    assert!(matches!(sv, Value::Scalar(_)));
}

#[test]
fn reference_identity() {
    let val = Value::SmallStr(SmallString::from("hello"));
    let (val, ref1) = val.take_ref();  // upgrades, returns Ref
    let (_, ref2) = val.take_ref();    // same Arc, refcount 3
    // ref1 and ref2 point to the same Scalar
}
```

### 21.2 Lexer, parser, and AST (5-7 weeks)

Build the lexer and parser together in the `perl-parser` crate.
The lexer and parser are inseparable — the lexer requires parser
feedback (expectation state) to tokenize correctly, and the parser
calls into the lexer with explicit expectations.  Build them
incrementally in lockstep:

- Expectation state enum and token types
- Main tokenizer loop dispatching on current character
- Pratt parser with precedence climbing
- Sublexing for quote-like constructs (`q//`, `qq//`, heredocs)
- Keyword table and keyword-specific parsing
- AST node types

Target passing `t/base/lex.t` and parsing the `t/base/` test files.
This is the hardest front-end component and should be done
thoroughly.

### 21.3 Minimal interpreter via AST walking (1-2 weeks)

Build a quick-and-dirty AST-walking interpreter in `perl-runtime` —
just enough to run `print`, basic arithmetic, string operations,
conditionals, and loops.  This is throwaway scaffolding to get rapid
feedback from the test suite.

### 21.4 Compile-time execution (`BEGIN`, `use`) (1-2 weeks)

Implement the `Executor` trait (§22.1) and the
compilation/execution interleaving so that `use strict`,
`use warnings`, `use constant`, and simple `BEGIN` blocks work.
This unblocks virtually all real Perl code.

### 21.5 Lowering and IR (2-3 weeks)

Build the HIR lowering and IR code generation in `perl-compiler`.
Migrate the interpreter from AST walking to IR execution.  The AST
walker can remain as a fallback during transition.

### 21.6 Regex engine (3-4 weeks)

Build the backtracking regex engine.  Target passing `t/base/pat.t`
and then `t/op/re_tests`.

### 21.7 Subroutines, closures, and packages (2-3 weeks)

Implement closure captures as `Vec<Value>` (not Perl 5-style pads),
package declarations, method dispatch, and `@ISA`-based inheritance.

### 21.8 Module loading (1-2 weeks)

Implement `require`, `use`, `do`, `@INC` search, and the standard
import/export mechanisms.  Module registry for concurrent `require`
(§13.11).

### 21.9 Core builtins (ongoing)

Implement builtins incrementally, guided by which upstream tests are
closest to passing.

### 21.10 Concurrency (when core is stable)

Implement per-value synchronization, `spawn`/`spawn blocking`/
`spawn thread`, and Tokio integration.  The `Arc<RwLock<T>>` value
model is concurrent from day one; this step adds the multi-task
execution paths, the Tokio event loop, and the cardinal invariant
enforcement (§13.11).

### 21.11 Typed layer (incremental, alongside other steps)

`let`/`fn` keyword registration and parsing can begin as soon as
the parser exists (Step 2).  Type checking and typed IR generation
build on Step 5.  `struct`/`enum`/`impl`/`trait` follow.  `extern fn`
and AOT Rust codegen can proceed independently once the IR is stable.

### 21.12 REPL (when interpreter is usable)

Build the REPL module in `perl-runtime` (behind the `repl` feature
flag) on `reedline` once the interpreter can execute basic code
(after Step 3 or 4).  Start with simple expression evaluation and
`dd()` output, then add introspection commands, tab completion, and
syntax highlighting incrementally.

---

## 22. Project Structure

### 22.1 Crate Architecture

The workspace consists of five library crates and one binary, with
three independent leaf crates that have no cross-dependencies:

```text
                    ┌───────────┐
                    │   perl    │  (bin: fn main())
                    └─────┬─────┘
                          │
                  ┌───────▼────────┐
                  │  perl-runtime  │  (lib: interpreter,
                  │                │   builtins, CLI/REPL)
                  └┬────┬─────────┘
                   │    │
        ┌──────────▼┐   │   ┌────────────┐
        │perl-regex │   │   │perl-compiler│
        │           │   │   │(lib: HIR,   │
        │(lib:      │   │   │ IR, lower,  │
        │ standalone│   │   │ optimize)   │
        │ regex     │   │   └──┬──────┬───┘
        │ engine)   │   │      │      │
        └───────────┘   │  ┌───▼──┐ ┌─▼──────────┐
                        │  │perl- │ │perl-parser  │
                        │  │core  │ │(lib: lexer  │
                        │  │(lib: │ │ + parser    │
                        │  │values│ │ + AST)      │
                        │  │etc.) │ │             │
                        │  └──────┘ └─────────────┘
                        │      ▲         ▲
                        └──────┴─────────┘
```

| Crate | Type | Dependencies | Contents |
|---|---|---|---|
| `perl-core` | lib | `bytes` | Strings, values, scalars, flags, typed value trait, extension API |
| `perl-parser` | lib | `bytes` | Lexer + Pratt parser + AST.  Uses raw Rust types for literals — independently useful for linters, formatters, syntax highlighters |
| `perl-regex` | lib | none | Standalone Perl-compatible regex engine.  Pure Rust API on `&str`/`&[u8]` — independently publishable (see §11) |
| `perl-compiler` | lib | `perl-core`, `perl-parser` | HIR, IR, lowering, optimization passes, `Executor` trait.  Future home for JIT (Cranelift) and AOT (Rust source emission) backends |
| `perl-runtime` | lib | `perl-compiler`, `perl-core`, `perl-regex` | Interpreter loop, `Executor` impl, call frames, symbol tables, builtins, magic, concurrency, bytecode save/load, CLI, REPL, debug |
| `perl` | bin | `perl-runtime` | Thin entry point: parses `std::env::args`, calls `perl_runtime::run`, exits |

#### 22.1.1 Design rationale:

Three leaf crates (`perl-core`, `perl-parser`, `perl-regex`) have no
cross-dependencies.  Each is independently useful as a library:
`perl-parser` for tools that need to parse Perl without executing it,
`perl-regex` for Rust programs that want Perl-compatible regex, and
`perl-core` for extensions that need the value types.  `perl-core` and
`perl-parser` depend on the `bytes` crate for zero-copy
reference-counted byte buffers (source slicing in the parser,
copy-on-write semantics for Perl strings).  `perl-regex` remains
dependency-free, operating on `&[u8]` and `&str` slices.

`perl-compiler` contains the compilation pipeline: AST → HIR → IR →
optimize.  It depends on `perl-parser` (for AST input) and
`perl-core` (for value types referenced during lowering).  It does
*not* depend on `perl-runtime` — this layering is deliberate.

`perl-runtime` depends on `perl-compiler` and implements the
`Executor` trait that the compiler uses for `BEGIN`/`use`/`eval
STRING` execution.  The mutual recursion between compiler and
interpreter is broken cleanly by dependency inversion:

```rust
/// Defined in perl-compiler.  The compiler doesn't know about the
/// interpreter — only that something can execute compiled IR.
trait Executor {
    fn execute(
        &mut self,
        ir: &CompiledUnit,
    ) -> Result<Value, PerlException>;
}
```

The compiler calls `executor.execute(begin_block)` when it hits a
`BEGIN`.  The runtime implements `Executor` and passes itself to the
compiler.  No circular dependency — just an inversion-of-control
callback.

This layering has a concrete benefit: a future AOT tool that emits
Rust source code (§14.14) can depend on `perl-compiler` without
pulling in the interpreter:

```text
perl-aot (future bin)
  └─ perl-compiler
       ├─ perl-core
       └─ perl-parser
```

No interpreter, no builtins, no Tokio runtime — just parse Perl,
compile to IR, emit Rust source.

**Bytecode serialization** lives in `perl-runtime`, not
`perl-compiler`.  The IR types are defined in `perl-compiler`, but
the bytecode format is optimized for fast loading into the
interpreter loop — it's a runtime concern.  The flow:

- First run: `perl-compiler` produces IR → `perl-runtime` executes
  it and optionally saves bytecode to a `.plc` file.
- Subsequent runs: `perl-runtime` finds the `.plc`, checks the
  timestamp, loads bytecode directly, skips compilation.

Same pattern as Python's `.pyc` files.

#### 22.1.2 Internal module structure of `perl-compiler`:

```text
perl-compiler/
    src/
        lib.rs
        hir/             # HIR types and lowering from AST
        ir/              # IR types and codegen from HIR
        optimize/        # Optimization passes
        executor.rs      # Executor trait definition
```

#### 22.1.3 Internal module structure of `perl-runtime`:

```text
perl-runtime/
    src/
        lib.rs
        interp/          # Interpreter loop, call frames
        builtins/        # print, chomp, push, sort, etc.
        symbols/         # Symbol tables, stashes, globs
        scope/           # local, my, our — dynamic/lexical scope
        magic/           # Tied variables, special vars
        concurrency/     # spawn, channels, task management
        bytecode/        # IR serialization/deserialization
        cli/             # Argument parsing, -e/-p/-n/-w flags
        repl/            # Interactive REPL (behind feature flag)
        debug/           # DAP server, profiling (behind feature flag)
```

#### 22.1.4 Internal module structure of `perl-parser`:

```text
perl-parser/
    src/
        lib.rs           # Public API: "give me source, get an AST"
        source.rs        # LexerSource, LexerLine, CRLF normalization
        token.rs         # Token enum, spans
        lexer.rs         # Tokenizer, sublexing
        parser.rs        # Pratt parser, AST construction
        ast.rs           # AST node types
        keyword.rs       # Keyword table
        expect.rs        # Expectation state
```

The lexer and parser are `pub(crate)` internals.  They share the
`Expect` state on the `Parser` struct — the parser sets it, the
lexer reads it when tokenizing the next token.  The crate boundary
is at the AST level — downstream crates see only the AST.

#### 22.1.5 Feature flags on `perl-runtime`:

```toml
[features]
default = []
repl = ["dep:reedline"]
debug = ["dep:dap"]
```

The REPL and debug server have their own dependencies that most
users don't need.  Feature flags keep them out of the default build
without requiring separate crates.  A future JIT backend would
follow the same pattern on `perl-compiler`:

```toml
# perl-compiler Cargo.toml
[features]
default = []
jit = ["dep:cranelift-codegen", "dep:cranelift-frontend"]
```

**The binary crate (`perl`) is minimal** — all logic lives in
`perl-runtime` as testable library code.  This makes the runtime
embeddable: a Rust application can depend on `perl-runtime` and
invoke Perl without going through the CLI.

**Published crate names** may differ from workspace names if the
`perl-*` namespace on crates.io is unavailable.  Fallback names
follow the pattern `perl5-core`, `perl5-parser`, `perl5-regex`, etc.
Local workspace names remain `perl-*` regardless.

### 22.2 Supporting Files

```text
docs/
    design.md            # This document
    compat-log.md        # Tracking compatibility decisions and divergences

tests/
    upstream/            # Symlink or copy of Perl 5 t/ directory
    unit/                # Rust unit tests
    integration/         # End-to-end Perl source tests
```

---

## 23. What This Design Omits (Intentionally)

The following are real concerns but are deliberately deferred:

- **Raku front end.**  Build it when the Perl 5 implementation is solid.
  Share the IR/runtime layer where possible, but do not compromise the
  Perl 5 design for speculative Raku compatibility.

- **Full AOT compilation of untyped Perl.**  The AOT compiler emits
  Rust source code (§14.14), which works well for typed code (`fn`,
  `struct`, `extern fn`).  Fully untyped code (`my`/`sub` with
  `eval STRING` and `BEGIN`-time execution) requires the runtime and
  is a harder target.  The gradual path — compile typed hot modules
  to Rust, leave the rest interpreted — is practical now.  Full
  untyped AOT is a future project.

- **Full XS compatibility.**  The thin shim approach is practical; full
  ABI emulation is an enormous project of its own.

- **Full debugger implementation.**  The goals and architecture are
  described in §19.4 (DAP support, REPL introspection, profiling
  hooks).  The detailed implementation can wait until the runtime is
  stable, but the structural commitments (stack traces by default,
  built-in `dd()`, IR instrumentation points) should be in place from
  early development.

- **Perl 5 `ithreads` compatibility.**  The shared heap model is
  fundamentally different from Perl 5's clone-everything `ithreads`.
  Compatibility with `threads.pm` and `threads::shared` would
  require an emulation layer.  Low priority given the superior
  native concurrency model.

- **Unicode edge cases.**  The `PerlString` type and UTF-8 flag model
  covers the architecture; full Unicode compliance (grapheme clusters,
  normalization, case folding tables) is an incremental effort.

- **Formats (`format`/`write`).**  Rare in modern Perl.  Add when a test
  demands it.

---

## 24. Design Summary

The key architectural decisions in this design:

1. **`Arc<RwLock<T>>`-based value representation.**  Self-managing
   reference-counted values, no centralized arena.  `PerlString` as
   a distinct type (octet vec + UTF-8 flag), not Rust `String`.
   Small values (integers, floats, short strings) inline in `Value`
   enum; heap-allocated values (scalars with magic, arrays, hashes)
   use `Arc<RwLock<T>>`.

2. **Atomic reference counting plus cycle detection** for memory
   management.  Per-variable task-local save stacks for `local`
   dynamic scope (§3.3).  Mortal stack for per-statement temporaries.

3. **Compiler and runtime are co-resident from day one.**  Compile-time
   execution (`BEGIN`, `use`, `eval STRING`) is a first-class
   architectural constraint, not an afterthought.

4. **Explicit lexer–parser feedback loop.**  `Expect` state, symbol
   table queries, and prototype-guided parsing — the mechanisms that
   make Perl's context-sensitive tokenization work.

5. **Standalone regex crate** (`perl-regex`) designed as an
   independently publishable Rust library with a clean API, filling
   a gap in the Rust ecosystem.  Embedded code blocks use a generic
   `RegexCodeHost` trait with no Perl runtime dependency.

6. **Single shared heap concurrency.**  All values — including
   magic-bearing values — live on one shared heap with atomic
   refcounting.  Only execution context (call stack, special
   variables, task-local dynamic scope) is per-task.  Magic callbacks
   are code references that run on whatever thread accesses the
   value.  Closures are shareable because captures are shared heap
   values, not Perl 5-style interpreter-local pads.  Per-value
   `RwLock` for mutable shared state, allocated on demand.
   Automatic parallelism of `map`/`grep`/`sort` via Rayon.
   Go-style green threads for implicit async IO and Rust-style
   `async`/`await` for typed code, both on Tokio.

7. **Value model first in implementation order**, because everything
   depends on it — not lexer first.

8. **Six crates plus one binary** — three independent leaf crates
   (`perl-core`, `perl-parser`, `perl-regex`) with no
   cross-dependencies.  `perl-core` and `perl-parser` depend on the
   `bytes` crate for zero-copy reference-counted buffers; `perl-regex`
   is dependency-free.  A `perl-compiler` library (HIR, IR, lowering,
   optimization, `Executor` trait), a `perl-runtime` library
   (interpreter, builtins, bytecode save/load), and a thin `perl`
   binary.  The compiler–interpreter mutual recursion
   (`BEGIN`/`use`/`eval STRING`) is broken by dependency inversion:
   `perl-compiler` defines the `Executor` trait, `perl-runtime`
   implements it.  Cargo workspace enforces dependency boundaries at
   the build system level.

9. **`let`/`fn` as the typed layer.**  Native Rust types with Rust
   syntax (`name: Type`), type inference, immutable-by-default
   semantics, `Option`/`Result` with Raku-inspired auto-unwrap,
   clone-on-assign ownership, `\$x` as `Arc` for shared ownership,
   `&T` borrows in `fn` parameters (the typed `@_`), and
   `Send + Sync` concurrency.  No borrow checker needed.  User-
   defined `struct`, `enum`, `impl`, and `trait` for composite types
   and interfaces.  `my`/`sub` retain full Perl 5 semantics.

10. **Concrete Rust syntax integration.**  `let` with type inference
    and tuple destructuring, `fn` with typed signatures, `struct`/
    `enum`/`impl`/`trait` for user-defined types, `extern fn`
    for standalone Rust-compatible functions, Rust-style `match` with
    exhaustiveness checking, Rust closure syntax with explicit capture
    modes, and `f"..."` format strings.

11. **Backward compatibility with standard Perl 5** via a CPAN module
    using the pluggable keyword API.  The `$`-aliased subset works
    on unmodified Perl 5.12+, creating a zero-lock-in adoption path.
