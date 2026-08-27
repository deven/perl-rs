# Benchmarking notes

How to measure PerlOxide's hot paths without being misled.  Every rule here was
earned by a measurement that produced a confident, wrong answer first; the
failures are recorded alongside the rules because the rule alone is easy to
nod at and forget.

`design.md` records what was decided.  This file records how to find out.

## The rules

**Compare things that differ in exactly one way.**  The first AVX-512
measurement of character counting came out 3.9x *slower* than AVX2, and the
conclusion drawn was "AVX-512 does not help counting".  The AVX-512 version had
also switched from a vector accumulator to a mask population count.  Holding
the algorithm fixed and changing only the width reversed the result: 60.9 B/ns
against AVX2's 42.8.  Two variables, one measurement, and the wrong one was
blamed.

**Match the harness to how the code is actually compiled.**  Three harnesses
gave three different rankings of the same kernels:

- Dispatching through a `fn` pointer costs an indirect call that LLVM
  devirtualizes for some variants and not others.  At small inputs that is as
  expensive as the work, and it reversed the ranking of the loop-form variants
  entirely.
- Pasting the algorithm inline (a macro taking the body as a token stream)
  removes the call, but measures a scenario that may not occur.
- A direct call to an `#[inline(never)]` function matches what most of this
  crate does.

Which is right is a question about the generated code, not about the benchmark.
Check first — see below.

**Watch for a baseline measured differently from the variants.**  A crossover
table showed `scalar` at 0.60x itself, because the baseline called the
reference directly (inlinable) while the variants went through a pointer.
Every ratio in the table was wrong.  If the reference does not read exactly
1.00, the harness is broken.

**Batch short inputs, and vary them.**  Below roughly 30 ns the timing loop's
own machinery is a measurable fraction of the result.  Running the algorithm
over a batch of distinct inputs inside one timed iteration amortizes that, and
the variation also stops the branch predictor memorizing a single answer.  A
sweep from batch 1 to 1M showed the median falling about 16-19% out to batch 8,
flat through roughly 128K, then rising again as the working set left cache.
**Use 256 to 1024**: past the knee, well clear of the cliff.

**Discard the first trial.**  Frequency ramp and cold caches made one
first-measurement read 8.00 ns against 4.85 for every subsequent run.

**Prefer L1-resident data for comparing algorithms.**  A single forward scan
touches each byte once, so memory cost is a term common to every variant: it
adds equally and compresses the ratios without reordering them.  Measuring from
DRAM hides the difference you can act on.  The same AVX2 kernel reads 107.5
B/ns from L1, 65.4 from a 512 KiB working set, and 27.4 from DRAM — the first
figure is the one that says anything about the algorithm.  Measure a larger
working set as a separate question when the concern is whether an advantage
survives memory pressure.

**Report variance, not just the median.**  A coefficient of variation above a
few percent means differences below that are not real.  It also diagnoses:
the single-accumulator NEON kernel read CV 15.5% where the four-accumulator
version read 0.11%, because a stalled dependency chain is sensitive to whatever
else the core is doing.  Slow *and* erratic is a signature.

**Know the machine.**  A shared container resolves to a few percent with
occasional excursions of 10-70%; a quiet laptop resolves to under 0.5%.
Spikes that move between runs are the machine, not the code.  Do not draw
conclusions the hardware cannot support.

## Read the assembly before theorizing

Four hypotheses about *why* two measurements differed —
algorithm-versus-width, population counts, `chunks_exact` versus indexing,
and inlining-versus-vectorization — were each falsified by the next
measurement.  Every one could
have been settled by looking at the generated code, which was available
throughout and took one command:

```
cargo rustc -p perl-core --release -- --emit=asm
rustc -O --edition=2024 --emit=asm bench.rs -o bench.s     # standalone
```

Note `--crate-type=lib` on a standalone file makes everything unreachable and
emits nothing; leave it off so `main` roots the program.

What that showed, in the end:

- `digit_run` survives as a real symbol and `parse_float` calls it seven times,
  so the direct-call harness is the right model for it.
- Runtime feature detection is a cached load and a bit test on the steady-state
  path; the initialization call sits in a cold branch.  The length guard skips
  it entirely for short inputs.
- LLVM does auto-vectorize the portable word loop on aarch64, but it
  re-zeroes the accumulator every iteration instead of carrying it —
  vectorized, with the accumulation strategy thrown away.  Hence 29.1 B/ns
  standalone, 15.5 inlined, against 65.8 hand-written.
- The bounds check in `&bytes[i..i + 8]` is not always elided even under a
  `while i + 8 <= bytes.len()` guard, which is the likely reason `chunks_exact`
  measured faster on x86 despite being slower on other targets.

## Editions

Editions are a front-end concern: an identical kernel compiled under 2018,
2021, and 2024 produced byte-identical assembly.  But an edition changes what
a construct *means* — array `into_iter` yields values rather than references
from 2021, closures capture disjoint fields — so identical source can compile
to different code.  Build benchmarks with `--edition=2024` to match the crate.

## Confirm what landed, not what was intended

The same failure appears in a second guise: measuring or verifying a tree that
does not contain the change.  A scripted edit whose anchor no longer matches
writes nothing; the assertion aborts, `git commit` reports a clean tree, and
every check afterwards passes — against the old code.  Tests, wrap checks,
and lint runs cannot catch this: they are all correct about a tree where
nothing happened.

Both symptoms are visible in output already being produced:

- **After a commit, the hash and subject must be the new ones.**  An unchanged
  hash is the whole report.  This is cheaper than adding a `git status` step,
  and it was already on screen when it was missed.
- **After a scripted edit, a non-empty diff proves the anchors matched.**
  A script that replaces N sites should say how many it replaced, and the
  number should be the expected one — a pattern matching N when N-1 was meant
  produced infinite recursion once by rewriting a helper's own tail.

The same discipline applies to what is being benchmarked.  Every x86 kernel
figure in this file's history was measured against a copy of the algorithm
written fresh in the harness, not against the crate's own code — which is how
an AVX2 kernel sat for several commits still using an inferior formulation that
had already been measured and rejected.  No amount of benchmarking finds that;
one look at the emitted assembly does.

## A check that shares the defect cannot fail

Verification has to be independent of the thing it verifies.  Two ways that
went wrong here, both of which reported success:

**A sweep verified with its own pattern.**  A pass converting British
spellings used `\b` word boundaries, which do not fire inside `snake_case`:
Python and Rust regex both treat `_` as a word character, so `_honour_` has
no boundary on either side and `all_four_tiers_honour_the_whole_protocol`
went untouched.  Rescanning with the same regex then reported zero
remaining.  Letter-only boundaries, `(?<![A-Za-z])word(?![A-Za-z])`, treat
`_` and digits as separators and are what an identifier-aware pass wants.

**A rewrite that lost what the draft had.**  That same name *had* been caught
by an earlier, cruder pass which hard-coded the rename after spotting it by
eye.  Replacing that pass with a systematic one, far better in vocabulary,
silently dropped the fix.  A broader second version is not automatically a
superset of the first, and the same thing happened to an AVX2 kernel earlier:
a measured improvement, then a rewrite that did not carry it.

Both are the same rule.  Check against something that does not share the
suspect assumption — a different pattern, the previous version's output, the
emitted assembly, the file on disk — because a check built from the same
premise as the work will agree with it.

## Synchronization primitives

Measured on the session container (Xeon 2.8 GHz, single vCPU), uncontended,
one run of the complete surface:

| primitive                          | ns/op |
| ---------------------------------- | ----: |
| unsynchronized read (floor)        |  0.99 |
| seqlock read (per-word atomics)    |  1.31 |
| hand-rolled spinlock               |  8.83 |
| `HeapArc` clone + drop (two RMW)   | 13.05 |
| `std::sync::Mutex`                 | 13.36 |
| `parking_lot::Mutex`               | 13.73 |
| `parking_lot::RwLock` write        | 13.74 |
| `std::sync::RwLock` write          | 15.23 |
| `std::sync::RwLock` read           | 18.19 |
| `parking_lot::RwLock` read         | 19.28 |

Four conclusions, in ascending order of how much they should change.

**Switching lock libraries is not a lever.**  `std` and `parking_lot` land
within about 6% of each other, with `std`'s `RwLock` read marginally the
faster of the two; the futex rewrite closed the gap that once justified the
dependency.  What `parking_lot` still buys is a smaller lock word, no
poisoning in the API, and different fairness under contention — none of which
this benchmark can see, because contention is exactly what a single vCPU
cannot produce.

**A read lock costs more than a write lock**, in both implementations.
Releasing a write lock is a store; releasing a read lock is a
read-modify-write on the shared counter, so a reader pays two RMWs to a
writer's one.  The "cheap read path" intuition is inverted here.

**A refcount is a peer of the lock, not a rounding error.**  Cloning and
dropping a `HeapArc` costs 13 ns, against ~19 ns for the read lock it sits
inside.  Any API returning an owned pointer-bearing value pays both.

**`RwLock`'s benefit is parallel readers, not cheaper reads**, and the
capability is one this crate's critical sections are too short to use.  A
hash lookup is ~17 ns, an array index ~2 ns, a scalar read ~1 ns — all
shorter than the 19 ns lock around them.  Worse, `RwLock` readers *write* to
the shared counter, so every reader on every core must take that line
exclusive: N logically parallel readers serialize on cache-line ownership.
Reader parallelism only repays that coherence traffic when the critical
section is long enough to amortize it, which is why the kernel has per-CPU
reader locks and why RCU exists.  For sections shorter than the lock, a
naive `RwLock` can scale *worse* than a mutex.

A seqlock inverts that: its readers issue no writes at all, the line stays
shared across every core, and readers genuinely scale.  Hence 1.31 ns,
essentially at the unsynchronized floor — 14x cheaper than any lock here, and
that figure is for the per-word-atomic version, which is sound under a
concurrent writer rather than the technically-UB direct read, and costs the
same in codegen.

The payload measured owns nothing, which is also the case that needs no
synchronization at the read at all: a `Value` living entirely in the
sixteen-byte envelope has nothing to keep alive.  Whether the figure extends
to a payload that owns pointers is a question for a concrete design, not one
this measurement answers.

Two lifetime facts belong beside that figure, because it invites lock-free
reasoning and the first one is easy to get backwards.

**`clone` racing `drop` is unconditionally sound**, with no reclamation
machinery of any kind.  `clone` takes `&self`, so the caller holds a live
borrow of an owned handle, and that handle's own contribution to the count
cannot be removed underneath it.  A concurrent drop implies a second handle,
hence a count of at least two, hence a floor of one — never zero during a
clone.  That is the entire point of atomic refcounting.  It is stated here
affirmatively because the opposite is the obvious guess, and an argument that
some read path is unsound *because clone races drop* has gone wrong earlier
than it looks.

**Monotonic publication is free.**  A pointer that goes null to non-null once
and never changes again has no lifetime hazard at all: nothing is ever
released, so there is nothing for a reader to outlive.  Publish with a release
CAS, read with an acquire load; a racing initializer that loses frees only its
own candidate, which no other thread ever observed.  Write-once faces and
`narrow_scan`'s CAS-meet are both in this class and need no lock.  What is
*not* in it is a cache that resets: `FullScalar::invalidate_caches` takes
`&mut self` and uses `get_mut`/`take`, so its exclusivity comes from the cell's
write lock rather than from the atomics, which serve the read side only.

Which mechanism such a face should use is an occupancy question rather than an
argument, and it is easy to settle the wrong way from the empty case alone.  A
null `AtomicPtr` is eight bytes against `OnceLock`'s twenty-four, but a filled
one is eight *plus* a separate allocation, and a sixteen-byte payload lands in
a 32-byte class — so the two cross near fifty percent occupancy, and boxing an
otherwise-inline value purely to obtain a pointer can erase the saving
entirely.  Measure the fill rate first.

### Sequence counter width

Use 64 bits.  The reasoning is worth recording because two shorter widths look
defensible and are not.

The reader's window is not its execution time — it is bounded by *scheduling*.
A userspace reader can be preempted between taking the sequence and
re-checking it, and a descheduled thread is off-CPU for milliseconds.  With a
write costing 2.34 ns, a full cycle takes 300 ns at one byte and 77 µs at
two, so a single scheduling quantum overruns both by orders of magnitude.

Wraparound only creates the *opportunity*; failure requires observing the
identical value.  For a preemption long enough to cycle the counter, the
value at resume is effectively uniform, so the probability is 1/2^(W−1) per
exposure: 1 in 128 for one byte, 1 in 32,768 for two, 1 in 2.1 billion for
four.  One in 32,768 is a corruption every few seconds under load.  Four
bytes is a rate rather than an impossibility, and the consequence is a
dereferenced garbage pointer — an unreproducible crash, the worst class of
defect to ship.

The kernel is comfortable at four bytes because its readers often run with
preemption disabled and its structures are replicated by the million.
Neither holds here.

And eight bytes is free in this layout.  `u32 seq + 16-byte payload` and
`u64 seq + 16-byte payload` both measure 24 bytes, the `u32` spending its
saving on alignment padding, and both read within noise of the floor (1.34
versus 1.64 ns).  Taking the wide counter converts a probabilistic argument
into a structural one and retires the question.  Put the writer-in-progress
flag in the low bit, as the kernel does, so it stays one word.

Where the counter lives, if a seqlock is ever built: not inside the payload.
A scalar slot is `rc_state` (8 bytes) plus a 16-byte payload in a 32-byte
stride, so eight bytes are already unclaimed there — but *unclaimed is not
free*, and spending them should be a recorded decision, since the design
moved `class_id` to a per-page array specifically to avoid per-slot bytes.
For containers the counter replaces the `RwLock` word rather than joining
it, and the identity allocation class holds — unless a reader-fallback hybrid
keeps both, in which case it is eight bytes on top.

### The measurement that is still owed

Everything above is uncontended, on one vCPU.  The choice between `RwLock`,
a mutex, and a seqlock-with-writer-mutex turns on concurrent readers across
cores at realistic critical-section lengths, which this container cannot
measure.  The same run should show what a descheduled writer costs: the stall
is no worse than a preempted lock holder's, since both are one scheduling
latency, but seqlock readers retry where lock waiters sleep, so it appears as
burnt CPU rather than as blocked threads.  This container cannot see that
either: any "contention" benchmark here measures timesharing, not coherence
traffic, and would flatter whichever primitive the harness favored.  That
experiment needs a multi-core machine and should settle the question rather
than argument settling it.

## Checklist

1. Does the reference row read exactly 1.00?
2. Does the harness match how the real code is compiled — called or inlined?
3. Are short inputs batched (256-1024 distinct inputs) with the first trial
   discarded?
4. Is the working set L1-resident when comparing algorithms?
5. Is the variance small enough for the difference claimed?
6. Do the compared variants differ in exactly one way?
7. Has the assembly been read before any explanation is offered for *why*?
8. Is the code being measured the code that ships, or a copy of it?
9. Did the commit hash change, and did the scripted edit report the expected
   number of replacements?
10. Is the verification independent of the work — a different pattern, an
    earlier version, the artifact on disk — or does it share the assumption
    being tested?
11. For a synchronization result: is it uncontended on one core, and is the
    question actually about contention?
