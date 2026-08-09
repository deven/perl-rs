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
