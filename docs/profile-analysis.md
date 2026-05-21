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

Both predictions are testable. Neither has been confirmed yet
on this hardware.

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

1. **Re-run the resolve bench with `mimalloc`.** Portage-repo
   already has a `mimalloc` feature; portage-bench can be told
   to use it. If the gap collapses, this confirms the
   allocator-state hypothesis.
2. **Reduce `Vec::clone` in `provider::new`.** The 9% spent in
   `Vec::clone` from the `from_iter`/`flat_map` aggregation is
   hot in *both* backends. `mem::take` on the produced vectors,
   or restructuring `VersionDeps::new` to avoid the clone,
   would help both backends and may eliminate the differential.
3. **Re-run on a different machine and different allocator.**
   The numbers in this document are from one ARM64 box. We
   need at least one x86_64 result and ideally one with
   non-glibc allocator before drawing portable conclusions.
4. **Profile with `perf c2c` or `heaptrack`.** Allocation
   counts and sizes per call site, rather than just CPU time,
   would tell us whether the backends differ in *how much*
   they allocate vs *how slow* each allocation is.

## How to reproduce

```sh
# In portage-repo (path-based deps already wired):
cd $WORK/portage-repo
LASSO=1 ./bench-regen.sh 1 20 24 32          # lasso build
SYMBOL_TABLE=1 ./bench-regen.sh 1 20 24 32   # symbol-table build
./bench-regen.sh 1 20 24 32                  # default papaya

# In portage-bench (solver benchmarks):
cd $WORK/portage-bench
cargo bench --bench resolve --features lasso        --save-baseline lasso
cargo bench --bench resolve --features symbol-table --save-baseline symbol-table
cargo bench --bench resolve                         --save-baseline papaya
cargo bench --bench resolve --features lasso --baseline papaya
```

The `[patch.crates-io]` blocks in `portage-bench/Cargo.toml`
and `portage-repo/Cargo.toml` pin all interner-touching crates
to local paths so that a single `gentoo-interner` version is
in the build graph. Without those patches, `gentoo-core`
pulled from crates.io drags in a second `gentoo-interner`
version and benchmarks silently mix backends.
