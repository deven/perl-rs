# Perl in Rust: Architecture and Design

## 0. Why This Document Exists

This is the master design for a new Perl implementation written in Rust,
targeting high compatibility with Perl 5 and eventually offering improved
concurrency, ahead-of-time compilation, and a cleaner extension story.

The existing ChatGPT design documents identify the right pipeline stages and
correctly call out sublexing as the central lexer problem.  This document
goes deeper on the decisions that actually determine whether the project
succeeds: value representation, memory management, the compile/execute
interleaving that Perl demands, the lexer–parser feedback loop, and the
concurrency model.  These are the places where a wrong early choice costs
months.

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

### 2.2 Arena-Based Allocation with Generational Indices

The recommended approach is **arena allocation with typed generational
indices**, not `Rc<RefCell<T>>`.

**Rationale:**

- `Rc<RefCell<T>>` imposes per-access overhead, panics on double borrow
  (or requires `try_borrow` everywhere), and leaks cycles.
- Arena allocation gives cache-friendly locality, trivial bulk deallocation,
  and index-based "pointers" that compose cleanly with Rust's ownership.
- Generational indices (each slot has a generation counter; indices carry
  the expected generation) catch use-after-free at negligible cost.

**Concrete structure:**

```text
┌─────────────────────────────────────┐
│  Heap (one per interpreter thread)  │
├──────────┬──────────┬───────────────┤
│ SvArena  │ AvArena  │ HvArena       │
│ (scalars)│ (arrays) │ (hashes)      │
└──────────┴──────────┴───────────────┘

SvId = { index: u32, generation: u32 }
AvId = { index: u32, generation: u32 }
HvId = { index: u32, generation: u32 }
```

Each arena is a `Vec<Slot<T>>` where `Slot<T>` is either `Occupied { gen, value }` or `Free { gen, next_free }`.  A free list provides O(1) allocation.

A `Value` enum then becomes:

```rust
enum Value {
    Undef,
    Int(i64),
    Float(f64),
    Sv(SvId),    // heap-allocated scalar
    Av(AvId),    // heap-allocated array
    Hv(HvId),    // heap-allocated hash
    Code(CodeId),
    // ... other first-class types
}
```

Small scalars (integers, floats, short strings below an inlining threshold)
can be represented inline in `Value` without a heap allocation, as a
performance optimization applied later.

### 2.3 The Scalar Internals

A heap-allocated scalar (`Sv`) needs to carry multiple simultaneous
representations:

```rust
struct Scalar {
    flags: SvFlags,           // IOK, NOK, POK, ROK, UTF8, READONLY, etc.
    iv: i64,                  // integer cache
    nv: f64,                  // float cache
    pv: Option<PerlString>,   // string cache
    rv: Option<Value>,        // reference target
    magic: Option<Box<MagicChain>>,
    stash: Option<HvId>,      // blessed package (for objects)
}
```

`PerlString` deserves its own type: Perl strings are octet sequences with
an optional UTF-8 flag, not Rust `String`.  A `PerlString` is essentially
`Vec<u8>` plus a `bool is_utf8` flag, with operations that respect Perl's
specific UTF-8 upgrade/downgrade semantics.

### 2.4 Reference Counting and Cycle Collection

Even with arenas, we need to know when a value is dead.  The options are:

1. **Tracing GC.**  Correct but adds latency and complexity.
2. **Pure reference counting.**  Simple, deterministic, leaks cycles — same
   as Perl 5.
3. **Reference counting + backup cycle collector.**  Deterministic in the
   common case, correct for cycles.

Recommendation: **option 3**, matching CPython's approach.  Implement
straightforward reference counting on arena slots.  Supplement with a
cycle-detecting collector (trial deletion / Bacon–Rajan) that runs
periodically or on demand.  This gives Perl-compatible `DESTROY` timing
(deterministic in the non-cyclic case) while being actually correct for
cycles.

The reference count can live in the arena slot header, not in the value
itself, to keep the hot path clean.

### 2.5 Magic (Tied Variables and Friends)

Perl "magic" is a mechanism for attaching callbacks to variable access.
It implements `tie`, special variables (`$!`, `$/`, `$_`, etc.), `pos()`,
`taint`, and other behaviors.

Model this as an optional chain of trait objects attached to a scalar:

```rust
trait Magic: Send {
    fn mg_get(&self, heap: &mut Heap, sv: SvId) -> Result<()>;
    fn mg_set(&self, heap: &mut Heap, sv: SvId) -> Result<()>;
    fn mg_clear(&self, heap: &mut Heap, sv: SvId) -> Result<()>;
    fn mg_free(&self, heap: &mut Heap, sv: SvId) -> Result<()>;
    fn mg_type(&self) -> MagicType;
}
```

The `Send` bound is intentional: it prepares for the concurrency model
where magic implementations must not hold thread-local references.

Tied variables specifically dispatch to Perl-level `FETCH`/`STORE`/etc.
methods through a `TieMagic` implementation.

---

## 3. Memory Management Details

### 3.1 The Heap Object

All value storage lives inside a `Heap`, which is the unit of ownership
for a single interpreter thread:

```rust
struct Heap {
    scalars: Arena<Scalar>,
    arrays: Arena<Vec<Value>>,
    hashes: Arena<HashMap<PerlString, Value>>,
    codes: Arena<Code>,
    regexes: Arena<CompiledRegex>,
    // ... globs, formats, IO handles
}
```

All access to values goes through the `Heap`.  This makes ownership
explicit and makes the borrow checker work for us instead of against us:
a `&mut Heap` gives you full access to all values; no `RefCell` needed.

### 3.2 Temporary Values and the Mortal Stack

Perl has a concept of "mortal" SVs — temporaries whose refcount is
decremented at the end of the current statement or scope.  This is
essential for expression evaluation.

Implement this as a stack of `SvId`s on the `Heap`.  At scope exit, walk
the mortal stack and decrement refcounts.  This mirrors `SAVETMPS` /
`FREETMPS` from Perl 5.

### 3.3 The Save Stack (Dynamic Scope)

`local` creates dynamically scoped bindings.  Perl 5 implements this with
a save stack that records "restore this variable to this value at scope
exit."

The Rust implementation should use the same strategy: a save stack of
`SaveEntry` variants:

```rust
enum SaveEntry {
    ScalarRestore { target: SvId, old_value: Scalar },
    ArrayRestore  { target: AvId, old_value: Vec<Value> },
    HashRestore   { target: HvId, old_value: HashMap<PerlString, Value> },
    GlobRestore   { package: HvId, name: PerlString, old_glob: Glob },
    StackMark,    // scope boundary
    // ... other restorable state
}
```

Scope entry pushes a `StackMark`; scope exit pops and restores entries
down to the mark.

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

Perl cannot be lexed context-free.  The lexer must know parser state to
make fundamental tokenization decisions:

| Situation | How it parses | Why |
|-----------|---------------|-----|
| `print (1+2)*3` | `print(1+2) * 3` or `print((1+2)*3)` | depends on whether `print` is known as a named unary |
| `map { ... } @list` | block or hashref | parser expectation |
| `$h{shift}` | hash subscript with bareword key | identifier after `{` in hash context |
| `sub foo { ... }` | sub declaration | `sub` keyword triggers block-opening |
| `Foo::Bar` | package name | `::` changes tokenization of preceding bareword |
| `/regex/` vs `$x / $y` | regex or division | parser expectation state |

The lexer and parser are therefore not independent.  They share state
through an explicit expectation mechanism, analogous to `PL_expect` and
`PL_lex_state` in `toke.c`.

### 5.2 Expectation-Based Tokenization

The lexer should carry an explicit `Expect` state set by the parser after
consuming each token:

```rust
enum Expect {
    Operator,     // next token is an operator or statement terminator
    Operand,      // next token is a term (variable, literal, prefix op)
    BlockOrHash,  // next '{' could be a block or hashref
    SubName,      // next token is a subroutine name
    Label,        // next token might be a label
    // ... other states as needed
}
```

The parser updates this after every shift/reduce.  The lexer reads it
before tokenizing the next token.  This is the mechanism that resolves
`/` as regex-vs-division, `{` as block-vs-hashref, and barewords as
various different token types.

### 5.3 Symbol Table Feedback

Beyond parser expectation, the lexer also needs symbol table access to
resolve:

- Whether a bareword is a known subroutine (and its prototype)
- Whether a name refers to a constant sub (to inline the value)
- Whether a name has been imported into the current package
- Whether a `CONSTANT` pragma or `use overload` is active

This should be implemented as a callback or shared reference from the
lexer to the symbol table, not by embedding the symbol table inside the
lexer.

### 5.4 Sublexing and the Context Stack

The ChatGPT lexer design document correctly identifies sublexing as the
core architectural requirement.  The implementation should use an explicit
context stack:

```rust
struct LexerContext {
    source: SourceBuffer,
    position: usize,
    mode: LexMode,
    delimiters: Option<DelimiterInfo>,
    subst_phase: Option<SubstPhase>,
    parent: Option<usize>,  // index into context stack
}

enum LexMode {
    Normal,                 // regular code
    QuoteInterpolating,     // inside "...", qq//, etc.
    QuoteLiteral,           // inside '...', q//
    QuoteWords,             // inside qw//
    RegexPattern,           // inside m//, qr//, or s///pattern
    SubstReplacement,       // inside s///replacement
    TranslitBody,           // inside tr/// or y///
    HeredocBody,            // collecting heredoc lines
    HeredocInterpolating,   // inside an interpolating heredoc
    Format,                 // inside format/write body
}
```

Quote-like scanning produces a stream of sub-tokens:

```text
QuoteBegin(qq, delimiter='|')
ConstSegment("Hello, ")
InterpScalar($name)
ConstSegment("! You have ")
InterpExprBegin
  ... tokens for the expression ...
InterpExprEnd
ConstSegment(" messages.\n")
QuoteEnd
```

The parser reassembles these into string concatenation / interpolation
AST nodes.

### 5.5 Heredoc Handling

Heredocs require special lexer context management because their body
appears on subsequent lines, while the rest of the statement (after the
`<<TAG`) continues on the *current* line:

```perl
my $x = <<END . "suffix";
body here
END
```

The lexer should:

1. When `<<TAG` is seen: record the heredoc (tag, quoting style) in a
   pending-heredoc queue on the current context.
2. When the end of the logical line is reached (after scanning the rest
   of the expression): push a new `HeredocBody` context that reads
   subsequent lines until the terminator.
3. When the terminator is found: pop the heredoc context and resume the
   enclosing context at the next logical line.

For multiple heredocs on one line (`<<A . <<B`), they are queued in order
and collected sequentially.

For heredocs inside interpolating contexts (the `s///` case from the
lexer design doc), the heredoc context must be able to walk up the context
stack to find the right source buffer for body collection — mirroring
Perl 5's `LEXSHARED` walk in `scan_heredoc()`.

### 5.6 Raw Token Layer

The lexer should emit raw tokens that are close to `toke.c`'s output
categories, not simplified parser-convenience tokens.  A separate adapter
can map raw tokens to parser tokens during the bootstrapping period when
the parser is still evolving.

Core raw token categories:

- Identifiers (barewords, with package qualification info)
- Variables (`$`, `@`, `%`, `*` sigils, with name)
- Numeric literals (integer, float, hex, octal, binary, underscored)
- String/quote sub-tokens (as described in §5.4)
- Regex sub-tokens
- Operators (arithmetic, string, logical, bitwise, comparison, binding)
- Punctuation (delimiters, semicolons, arrows, fat comma)
- Keywords (control flow, declaration, special forms)
- Heredoc markers
- Special tokens (end of input, format lines, `__END__`/`__DATA__`)

---

## 6. Parser Architecture

### 6.1 Pratt Parsing with Precedence Climbing

Perl's grammar as described in `perly.y` is an operator-precedence grammar
with many special forms.  A **Pratt parser** (top-down operator
precedence) is an excellent fit for Rust because:

- It handles precedence and associativity naturally without a grammar file.
- It is easy to extend with new operators or syntax.
- It handles prefix, infix, and postfix operators cleanly.
- It gives excellent error messages with minimal effort.
- It can be written as straightforward recursive Rust functions.

The parser should be structured as:

1. A `parse_expr(min_precedence)` core using Pratt precedence climbing.
2. Statement-level parsing functions for declarations, control flow, etc.
3. Special-form parsers for `sub`, `my`/`our`/`local`, `use`/`no`, etc.
4. A `parse_block()` that handles brace-delimited statement lists.

### 6.2 Parser–Lexer Feedback

After consuming each token, the parser must update the lexer's `Expect`
state.  This is not optional.  Examples:

- After consuming `print`: set `Expect::Operand` (the next thing is
  an argument list, so `/` is a regex, not division).
- After consuming a closing `)` of a sub call: set `Expect::Operator`.
- After consuming `sub`: set `Expect::SubName`.
- After consuming `{` as a block opener: set `Expect::Operand`.

This is implemented by having the parser call `lexer.set_expect(...)` at
the appropriate points.

### 6.3 Prototype-Guided Parsing

When the parser encounters a known subroutine name, it should check the
sub's prototype (if any) to determine how to parse the argument list.
Prototypes change the parsing:

- `sub foo ($)` — expects one scalar argument
- `sub foo (&@)` — first arg is a block, rest is a list
- `sub foo ()` — takes no arguments, so `foo + 1` is `foo() + 1`
- no prototype — standard list operator parsing

This requires symbol table access from the parser, reinforcing the
co-resident compiler/runtime architecture.

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
    ScalarInterp($name),
    ConstSegment("!"),
]
```

This preserves the structure from the lexer's sub-token stream and makes
lowering straightforward (it becomes a series of concatenations).

---

## 8. Semantic Lowering and HIR

### 8.1 Purpose

The lowering pass transforms the syntax-oriented AST into a
semantics-oriented High-level IR (HIR) where implicit Perl behaviors
become explicit.  This is where we encode Perl's actual semantics rather
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
    save_stack_mark: usize,   // for dynamic scope restoration
    mortal_stack_mark: usize,
}

struct Interpreter {
    heap: Heap,
    call_stack: Vec<CallFrame>,
    save_stack: Vec<SaveEntry>,
    mortal_stack: Vec<SvId>,
    symbol_tables: SymbolTableSet,
    special_vars: SpecialVars,
    compiler: Compiler,       // always available for eval STRING
}
```

### 10.2 The Symbol Table

Perl's symbol table is a hierarchy of hashes (stashes).  Each entry is a
typeglob containing slots for scalar, array, hash, code, IO, and format:

```rust
struct Glob {
    scalar: Option<SvId>,
    array: Option<AvId>,
    hash: Option<HvId>,
    code: Option<CodeId>,
    io: Option<IoId>,
    format: Option<FormatId>,
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

The Rust `regex` crate is fast but deliberately omits features that Perl
requires:

- Backreferences (`\1`, `\k<name>`)
- Lookahead and lookbehind (though `fancy-regex` supports these)
- Embedded code blocks `(?{ ... })` and `(??{ ... })`
- Backtracking control verbs (`(*MARK:name)`, `(*FAIL)`, `(*SKIP)`, etc.)
- Recursive/reentrant patterns (`(?R)`, `(?1)`)
- The `\G` assertion
- `pos()` interaction
- `(?{ ... })` runs Perl code during matching

These features require a backtracking NFA engine, not a DFA.

### 11.2 Recommended Approach

Build a custom backtracking regex engine as a separate crate within the
project.  The engine should:

1. Compile Perl regex syntax to an internal bytecode representation.
2. Execute via recursive backtracking (like Perl 5's `regexec.c`).
3. Support capture groups, backreferences, lookaround, and backtracking
   control verbs.
4. Provide hooks for embedded code blocks that call back into the
   interpreter.
5. Optionally delegate simple patterns (no backrefs, no embedded code) to
   the `regex` crate for performance — but this optimization can come
   later.

The regex bytecode compiler is separate from the Perl compiler; it takes
a parsed regex AST and produces a regex-specific instruction sequence.

### 11.3 Regex Compilation Pipeline

```text
Regex source ──► Regex parser ──► Regex AST ──► Regex compiler ──► Regex bytecode
```

The regex parser handles Perl regex syntax including character classes,
Unicode properties, and all special constructs.  The regex AST is
lowered to bytecode instructions like:

```text
Literal(bytes), CharClass(set), AnyChar, Anchor(type),
Split(branch1, branch2), Jump(target),
Save(group), BackRef(group),
LookAhead(subprog, negated), LookBehind(subprog, negated),
EmbeddedCode(ir_function_id),
Mark(name), Fail, Skip, Prune, Commit,
Recurse(group), Match
```

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
cloning the entire interpreter state for each thread.  This is safe but
extremely expensive, and sharing data between threads requires `threads::shared`,
which serializes access.

The goal for "modern mode" is to support genuine multi-threaded execution
with shared-nothing defaults and explicit sharing where needed.

### 13.2 The Design: Interpreter-Per-Thread with Message Passing

Each OS thread (or async task) gets its own `Interpreter` with its own
`Heap`, symbol table, and compiler.  This is safe by construction —
there is no shared mutable state.

Communication between interpreters uses message passing, with values
serialized across the boundary.  This is the Erlang/actor model, not
the shared-memory model.

```rust
struct InterpreterHandle {
    sender: mpsc::Sender<Message>,
    // ... other coordination
}

enum Message {
    Call { function: String, args: Vec<SerializedValue>, reply: oneshot::Sender<Result<SerializedValue>> },
    Eval { source: String, reply: oneshot::Sender<Result<SerializedValue>> },
    Shutdown,
}
```

For values that genuinely need to be shared (e.g., concurrent data
structures), provide an explicit `SharedValue` type with atomic
operations, analogous to `threads::shared` but designed for Rust's
ownership model:

```rust
// Perl-side API in modern mode:
// use shared;
// my $counter :shared = 0;
// lock($counter); $counter++; unlock($counter);
```

### 13.3 Async Integration

In modern mode, the interpreter can be driven by a Tokio (or other async
runtime) event loop.  Blocking operations (`sleep`, IO, system calls) can
yield to the async runtime instead of blocking the OS thread.

This requires the interpreter's main loop to be `async`-aware:

```rust
async fn run_op(&mut self, op: &IrOp) -> Result<()> {
    match op {
        IrOp::CallBuiltin { builtin: BuiltinId::Sleep, .. } => {
            tokio::time::sleep(duration).await;
        }
        IrOp::CallBuiltin { builtin: BuiltinId::ReadLine, .. } => {
            let line = reader.read_line().await?;
            // ...
        }
        _ => {
            self.execute_sync(op)?;
        }
    }
    Ok(())
}
```

In compat mode, the interpreter runs synchronously on a single thread,
preserving Perl 5 semantics exactly.

---

## 14. Extension and FFI

### 14.1 The Native Extension API

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

### 14.2 C FFI

For calling C libraries from Perl, provide a mechanism analogous to
Perl's `FFI::Platypus` that uses Rust's `libffi` bindings.  This
avoids the need for XS glue code entirely.

### 14.3 XS Compatibility (Deferred)

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

## 15. Source Filters

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

## 16. Error Handling and Diagnostics

### 16.1 Source Spans

Every token, AST node, and IR instruction should carry a source span
(`Span { file, start_byte, end_byte }`).  This enables precise error
messages at every compilation and execution stage.

### 16.2 Error Representation

Compile-time and runtime errors should be a single error type that
carries:

- Span (where in source the error occurred)
- Category (syntax, type, runtime, etc.)
- Severity (error, warning, note)
- Message (human-readable)
- Suggestions (when applicable)
- Chain (underlying cause, for error chains)

### 16.3 Warnings

Perl's `warnings` pragma is lexically scoped.  The implementation should
carry a warnings bitmask in the compilation context (alongside hints),
and the runtime should check the active warnings state before emitting
a warning.

---

## 17. Testing Strategy

### 17.1 Upstream Test Suite as Oracle

The Perl 5 `t/` directory is the primary test oracle.  Progress is
measured by how many upstream `.t` files pass.

### 17.2 Phased Test Progression

**Phase 1: Lexer/parser foundations**
- `t/base/lex.t` — lexer basics
- `t/base/cond.t` — conditionals
- `t/base/if.t` — if/elsif/else
- `t/base/pat.t` — basic patterns
- `t/base/term.t` — basic terms

**Phase 2: Core semantics**
- `t/op/arith.t` — arithmetic
- `t/op/string.t` — string operations
- `t/op/cond.t` — conditional expressions
- `t/op/assignop.t` — assignment operators
- `t/op/array.t`, `t/op/hash.t` — data structures
- `t/op/sub.t`, `t/op/closure.t` — subroutines and closures

**Phase 3: Advanced features**
- `t/op/re_tests` — regex
- `t/op/heredoc.t` — heredocs
- `t/op/subst.t` — substitution
- `t/op/eval.t` — eval
- `t/comp/use.t` — use/require
- `t/op/tie.t` — tied variables

**Phase 4: Module ecosystem**
- Core module tests
- Selected CPAN module tests
- Smoker-style automated test runs

### 17.3 Rust Unit Tests

Each subsystem (lexer, parser, lowering, IR, runtime, regex engine) should
also have its own Rust-level unit and integration tests that do not depend
on the Perl test harness.  These are faster to run and easier to debug
than end-to-end `.t` tests.

---

## 18. Implementation Order

This is a recommended sequence of implementation work, ordered to
maximize the ratio of "useful progress" to "infrastructure investment"
at each step.

### Step 1: Value model and heap (2-3 weeks)

Build the arena-based heap, `Scalar`, `Value`, reference counting, and
the mortal/save stacks.  Write extensive unit tests for value coercion,
reference creation, and scope save/restore.  This is the foundation
everything else stands on.

### Step 2: Lexer with sublexing (3-4 weeks)

Build the lexer with the full context stack, sublexing, heredoc handling,
and expectation-based tokenization.  Target passing `t/base/lex.t`.
This is the hardest front-end component and should be done thoroughly.

### Step 3: Parser and AST (2-3 weeks)

Build the Pratt parser producing a syntax-oriented AST.  Wire up the
parser–lexer feedback loop.  Target parsing (not necessarily executing)
the `t/base/` test files.

### Step 4: Minimal interpreter via AST walking (1-2 weeks)

Build a quick-and-dirty AST-walking interpreter — just enough to run
`print`, basic arithmetic, string operations, conditionals, and loops.
This is throwaway scaffolding to get rapid feedback from the test suite.

### Step 5: Compile-time execution (`BEGIN`, `use`) (1-2 weeks)

Implement the compilation/execution interleaving so that `use strict`,
`use warnings`, `use constant`, and simple `BEGIN` blocks work.  This
unblocks virtually all real Perl code.

### Step 6: Lowering and IR (2-3 weeks)

Build the HIR lowering and IR code generation.  Migrate the interpreter
from AST walking to IR execution.  The AST walker can remain as a
fallback during transition.

### Step 7: Regex engine (3-4 weeks)

Build the backtracking regex engine.  Target passing `t/base/pat.t` and
then `t/op/re_tests`.

### Step 8: Subroutines, closures, and packages (2-3 weeks)

Implement lexical pads, closures, package declarations, method dispatch,
and `@ISA`-based inheritance.

### Step 9: Module loading (1-2 weeks)

Implement `require`, `use`, `do`, `@INC` search, and the standard
import/export mechanisms.

### Step 10: Core builtins (ongoing)

Implement builtins incrementally, guided by which upstream tests are
closest to passing.

### Step 11: Concurrency (when core is stable)

Implement the interpreter-per-thread model and message passing.  This
can happen in parallel with builtin work once the core is stable.

---

## 19. Project Structure

```text
crates/
    perl-value/          # Value, Scalar, Heap, arenas, magic
    perl-string/         # PerlString (octet + UTF-8 flag)
    perl-lexer/          # Lexer with sublexing and context stack
    perl-parser/         # Pratt parser, AST
    perl-hir/            # HIR and lowering from AST
    perl-ir/             # IR definition and codegen from HIR
    perl-runtime/        # Interpreter, call frames, symbol tables
    perl-regex/          # Regex parser, compiler, and engine
    perl-compiler/       # Orchestrates lex → parse → lower → codegen
    perl-cli/            # Binary entry point, CLI arg handling
    perl-extensions/     # Native extension API, FFI support

docs/
    design.md            # This document
    lexer-notes.md       # Detailed lexer design notes
    compat-log.md        # Tracking compatibility decisions and divergences

tests/
    upstream/            # Symlink or copy of Perl 5 t/ directory
    unit/                # Rust unit tests
    integration/         # End-to-end Perl source tests
```

Using a Cargo workspace with separate crates enforces clean boundaries:
`perl-lexer` cannot accidentally depend on `perl-runtime` internals,
the regex engine is independently testable, and the value model can be
used by all layers without circular dependencies.

---

## 20. What This Design Omits (Intentionally)

The following are real concerns but are deliberately deferred:

- **Raku front end.**  Build it when the Perl 5 implementation is solid.
  Share the IR/runtime layer where possible, but do not compromise the
  Perl 5 design for speculative Raku compatibility.

- **AOT compilation.**  The IR is designed to support it, but the actual
  compiler backend (Cranelift, LLVM, or custom) is a future project.

- **Full XS compatibility.**  The thin shim approach is practical; full
  ABI emulation is an enormous project of its own.

- **Debugger protocol.**  Important eventually, but the IR + span model
  provides the hooks needed.  Implementing a debugger can wait until
  the runtime is stable.

- **Threads::shared in compat mode.**  The interpreter-per-thread model
  is the modern story; compat-mode threads are a separate, lower-priority
  compatibility target.

- **Unicode edge cases.**  The `PerlString` type and UTF-8 flag model
  covers the architecture; full Unicode compliance (grapheme clusters,
  normalization, case folding tables) is an incremental effort.

- **Formats (`format`/`write`).**  Rare in modern Perl.  Add when a test
  demands it.

---

## 21. Key Differences from the ChatGPT Designs

For reference, the main places where this design diverges from or extends
the prior design documents:

1. **Value representation is specified concretely.**  Arena-based allocation
   with generational indices, not a vague "Value enum."  `PerlString` as a
   distinct type (octet vec + UTF-8 flag), not Rust `String`.

2. **Memory management has a concrete strategy.**  Reference counting plus
   cycle detection, mortal stack, save stack.  Not left as "to be decided."

3. **Compile-time execution is treated as a first-class architectural
   constraint**, not a footnote.  The compiler and runtime are co-resident
   from day one.

4. **The lexer–parser feedback loop is explicit.**  `Expect` state,
   symbol table queries, prototype-guided parsing.  The prior docs
   acknowledge the lexer's complexity but do not specify how parser state
   flows back in.

5. **The regex engine is scoped.**  A custom backtracking engine is needed;
   the Rust `regex` crate is insufficient.

6. **Concurrency model is concrete.**  Interpreter-per-thread with message
   passing, not "modern mode will have better concurrency."

7. **Implementation order is calibrated to the dependency graph.**  Value
   model first (because everything depends on it), not lexer first.

8. **Workspace-level crate structure** enforces the intended dependency
   boundaries at the Cargo level, not just by convention.
