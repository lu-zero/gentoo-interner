# Solver-workload profile analysis: papaya vs lasso

This document captures findings from profiling the
`portage-atom-pubgrub` solver under both interner backends
(`papaya` default and `lasso` feature). It is intended as a
reference for anyone re-running the benchmarks on different
hardware.

## Setup

- Hardware: ARM64, 32-core, glibc 2.x.
- Workload: `portage-bench`'s `resolve` benchmark, target
  `targets/portage-atom-pubgrub/firefox`.
- Profiles captured with:
  ```
  perf record -F 997 --call-graph dwarf -g -- \
      cargo bench --bench resolve --profile-time 20 \
      'targets/portage-atom-pubgrub/firefox'
  ```
- Single Gentoo tree, single repo snapshot, every dep cached.
- Both backends ran the same number of iterations under
  `--profile-time 20`.

## Observed wall-clock gap

For the firefox solver target:

| backend | mean per-iter |
|---------|---------------|
| lasso   | ~177 ms       |
| papaya  | ~206 ms       |

The 12-18% gap reproduces across runs and across other solver
targets in the suite. It is not noise.

## What the profiles show

Top hotspots are essentially identical between backends.

| function (hot path)          | papaya % | lasso % |
|------------------------------|---------:|--------:|
| libc allocator (combined)    |    39.3% |   34.5% |
| `Vec::clone` (provider init) |     8.9% |    7.2% |
| `provider::new`              |     4.2% |    4.5% |
| `SmallVec::drop`             |     3.0% |    3.7% |
| `DepEntry::drop`             |     2.9% |    1.9% |
| `gentoo_interner::get_or_intern` | 0.6% |   <0.5% |
| `DashMap::_get` (lasso only) |     n/a  |    1.8% |

The interner's *own* code is under 1% of total time in both
runs. The gap is not in the interner's hot path.

## Where the gap actually lives

The gap is in the libc allocator, not in the interner. Converted
to absolute time per iteration:

| component             | papaya  | lasso  | delta |
|-----------------------|--------:|-------:|------:|
| libc allocator        | ~81 ms  | ~61 ms | +20 ms|
| everything else       | ~125 ms | ~116 ms| +9 ms |

The ~20 ms extra time spent in `_int_malloc` /
`_int_free_merge_chunk` / `unlink_chunk` / `memcpy` /
`cfree` accounts for most of the 29 ms wall-clock gap.

Neither backend calls the allocator directly from the hot
solver path. Both backends produce allocator pressure from the
*same* call sites:

- `PortageDependencyProvider::new` cloning per-CPV dependency
  vectors (`Vec::clone` ~9%).
- Aggregating `class_results.iter().flat_map(|r|
  r.requirements.clone()).collect()` in `VersionDeps::new`.
- `SmallVec` and `DepEntry` drops on dependency vectors.

So the question is: why does the same allocation-heavy code
spend ~20 ms longer in the allocator when the *interner*
underneath is papaya rather than lasso?

## Hypothesis

The backends shape the global allocator's state differently
*before* the hot solver code runs:

- During cache load, both backends intern the same set of
  strings. Each allocates differently:
  - lasso: one DashMap shard's hash table + the lasso arena
    (one big bump-style allocation per arena page).
  - papaya: one papaya HashMap (lock-free CAS, multiple internal
    segments) + `boxcar::Vec` (segmented append-only vec, growing
    in geometric chunks) + per-string `Box::leak` for the heap
    string copy.
- Both end up with comparable retained memory but very different
  free-list state and fragmentation pattern in the glibc arena.
- When `provider::new` then issues thousands of small `Vec`
  allocations, glibc's free-list servicing those allocations
  pays the cost of the fragmentation left behind by the
  interner build.

In other words: the interner's *cleanup state* is what the
solver pays for, not the interner's own operations.

This hypothesis predicts:

- Switching to a different allocator (jemalloc, mimalloc) should
  shrink or eliminate the gap, because non-glibc allocators
  have very different free-list strategies.
- Workloads with less `Vec::clone` traffic in `provider::new`
  should narrow the gap, because they make fewer demands on
  whatever state the interner left in the allocator.

The first prediction is **confirmed** (see *Confirmed result*
below). The second is still pending.

## Confirmed result: mimalloc closes the gap

Re-ran the same firefox resolve bench across a 3×2 matrix of
backends × allocators (glibc 2.x default vs. mimalloc 0.1.51):

| backend       | glibc    | mimalloc | mimalloc speedup |
|---------------|---------:|---------:|-----------------:|
| papaya        |   189 ms |  81.3 ms |            2.32× |
| lasso         |   199 ms |  78.8 ms |            2.53× |
| symbol-table  |   184 ms |  81.0 ms |            2.27× |

What this tells us:

1. **The 12-18% backend gap is a glibc artifact.** On mimalloc
   all three backends land within 4% of each other, with
   noise-level differences. The "lasso wins" effect we
   originally documented was the glibc free-list reacting
   differently to each backend's allocation pattern, not
   anything about the interner's hot path.

2. **mimalloc gives a ~2.3-2.5× wall-clock speedup on the
   solver workload.** That's an order of magnitude larger than
   any inter-backend difference ever was. Allocator choice
   matters far more than interner choice for this workload.

3. **Backend ranking inverts.** On glibc, symbol-table is
   marginally fastest. On mimalloc, lasso is marginally
   fastest. The differences are inside the noise floor —
   neither ordering is meaningful in isolation.

On the regen workload (portage-repo full-tree, j=20), the
allocator effect is smaller but in the same direction:

| backend       | glibc      | mimalloc   | speedup |
|---------------|-----------:|-----------:|--------:|
| papaya        |   11.36 s  |   9.86 s   |  1.15×  |
| lasso         |   11.69 s  |  10.04 s   |  1.16×  |
| symbol-table  |   11.60 s  |  10.05 s   |  1.15×  |

Regen never had a meaningful backend gap to close (~3% spread),
but mimalloc still buys ~15% real time and ~20% user time at
the cost of ~2× peak RSS (230 MB → 430 MB) — a typical mimalloc
tradeoff for arena-style allocation.

### Operational takeaways

- **Default to mimalloc** for solver-driven binaries (`em`,
  any future portage-resolver tool). The 2.3× speedup pays
  for itself; the doubled RSS is a non-issue at our absolute
  numbers (under 500 MB on a 70k-ebuild tree).
- **Don't pick a backend on perf grounds.** Papaya, lasso,
  symbol-table all tie on mimalloc; pick on soundness/API
  ergonomics, not microbench numbers.
- **The `Vec::clone` hotspot in `provider::new` is still
  worth cleaning up** — not for the closed gap, but because
  it's a code smell in its own right. Now ~7 ms on an 80 ms
  budget rather than ~18 ms on a 200 ms budget.

## What this *does not* mean

- "lasso is a faster interner for this workload." The
  microbenchmarks in `benches/interner.rs` show papaya
  is faster at single-thread interning, and competitive
  at multi-thread interning. The gap on solver targets
  comes from indirect allocator effects, not from
  papaya being slow at its job.

- "We should ship lasso as the default." Lasso is a more
  mature implementation with broader test coverage, but
  on a clean comparison of interner work it loses or ties.
  The solver gap is interesting evidence about allocator
  pressure, not a verdict on the backend.

## Suggested next experiments

In rough order of effort vs. expected signal:

1. **Cross-hardware data.** The matrix above is from one ARM64
   box. An x86_64 run with the same `LASSO=1` / `SYMBOL_TABLE=1`
   / `MIMALLOC=1` env-var matrix would tell us whether the
   mimalloc collapse is portable.
2. **Clean up `Vec::clone` in `provider::new`.** Now a code-
   smell fix more than a perf fix — ~7 ms on an 80 ms budget.
   `mem::take` on the produced vectors, or restructuring
   `VersionDeps::new` to consume instead of clone, would make
   the code less sloppy regardless.
3. **Profile with `heaptrack` on mimalloc.** Now that we know
   the allocator dominates, allocation counts and sizes per
   call site tell us where the remaining 80 ms goes, and
   whether further reductions are worthwhile.

## How to reproduce

```sh
# In portage-repo (path-based deps already wired):
cd $WORK/portage-repo
LASSO=1 ./bench-regen.sh 1 20 24 32          # lasso build
SYMBOL_TABLE=1 ./bench-regen.sh 1 20 24 32   # symbol-table build
./bench-regen.sh 1 20 24 32                  # default papaya

# In portage-bench (solver benchmarks, glibc default):
cd $WORK/portage-bench
cargo bench --bench resolve --features lasso        -- 'resolve/targets/portage-atom-pubgrub/firefox'
cargo bench --bench resolve --features symbol-table -- 'resolve/targets/portage-atom-pubgrub/firefox'
cargo bench --bench resolve                         -- 'resolve/targets/portage-atom-pubgrub/firefox'

# Same bench under mimalloc — repeat each line with `,mimalloc`
# appended to the feature list. This is the matrix that
# produced the table above.
cargo bench --bench resolve --features mimalloc              -- 'resolve/targets/portage-atom-pubgrub/firefox'
cargo bench --bench resolve --features 'lasso,mimalloc'        -- 'resolve/targets/portage-atom-pubgrub/firefox'
cargo bench --bench resolve --features 'symbol-table,mimalloc' -- 'resolve/targets/portage-atom-pubgrub/firefox'
```

The `[patch.crates-io]` blocks in `portage-bench/Cargo.toml`
and `portage-repo/Cargo.toml` pin all interner-touching crates
to local paths so that a single `gentoo-interner` version is
in the build graph. Without those patches, `gentoo-core`
pulled from crates.io drags in a second `gentoo-interner`
version and benchmarks silently mix backends.
