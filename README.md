# gentoo-interner

[![LICENSE](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Crates.io](https://img.shields.io/crates/v/gentoo-interner.svg)](https://crates.io/crates/gentoo-interner)
[![docs.rs](https://docs.rs/gentoo-interner/badge.svg)](https://docs.rs/gentoo-interner)

String interning for Gentoo-related Rust crates.

## Features

- Process-wide deduplication via a hybrid `papaya` + `boxcar` backend (default)
- Optional `lasso` arena-backed backend for benchmarking
- `Box<str>` fallback when interning disabled
- Optional serde support
- `Copy` types with global interner (4 bytes)

## Backend design (default `interner` feature)

The default backend is a deliberate hybrid:

- **Forward map** (`str` → `u32` id): `papaya::HashMap` — lock-free reads.
  The hot `get_or_intern` fast path checks here with no lock and no
  allocation when the string is already interned.
- **Reverse map** (`u32` id → `&'static str`): `boxcar::Vec` — a
  concurrent append-only vec. `resolve(id)` is an O(1) indexed read with
  no hashing and no `.pin()` guard, which makes the read path
  dramatically faster than going through a second hash map.
- **Insert serialization**: 32 sharded `parking_lot::Mutex<()>` — the
  string's hash picks a shard. Threads on different shards proceed in
  parallel; threads colliding on the same shard serialize. Inside the
  lock we use papaya's cheap `insert()` rather than the lock-free
  `get_or_insert_with()`, which carries extra CAS overhead even when
  uncontended.

This mirrors the pattern `lasso::ThreadedRodeo` uses with `DashMap`,
but the `boxcar` reverse path is markedly faster than a sharded
`DashMap<id, &str>` for the read-heavy access patterns common in our
crates (e.g. solver hot loops repeatedly resolving interned slot/use
flag names).

### Help wanted: testing and review

This backend is new in 0.2.1 and the multi-threaded soundness deserves
extra scrutiny. The relevant invariants are:

1. A `u32` id observed via the forward map must always resolve in the
   reverse vec — the slow path holds the shard mutex across both the
   `boxcar::Vec::push` and the `papaya::HashMap::insert`, so the id is
   published in the forward map only after the reverse slot exists.
2. `Box::leak` is never reached on a lost race — the re-check inside the
   shard lock returns the existing id without leaking.
3. The shard hasher is process-stable (initialized once via `OnceLock`)
   so shard assignment doesn't drift across calls for the same string.

If you find a stress test or race scenario that breaks any of these,
please open an issue. Reproducer benchmarks (criterion-style) and
`loom`-based tests would be especially welcome.

You can compare backends locally with the included microbenchmarks:

```sh
cargo bench --bench interner                                              # papaya (default)
cargo bench --bench interner --no-default-features --features lasso       # lasso
```

## Benchmarks

Results from a 32-core Linux machine. **Bold** marks the faster backend.

### Microbenchmark (`cargo bench --bench interner`)

| Workload                 | Papaya (default)  | Lasso         | Winner  |
|--------------------------|-------------------|---------------|---------|
| `intern_new/100`         | 143 µs            | **120 µs**    | Lasso   |
| `intern_new/1000`        | **1.18 ms**       | 1.41 ms       | Papaya  |
| `intern_new/10000`       | 24.9 ms           | **9.05 ms**   | Lasso   |
| `intern_existing/100`    | 6.60 µs           | **5.94 µs**   | Lasso   |
| `intern_existing/1000`   | 76.2 µs           | **71.6 µs**   | Lasso   |
| `intern_existing/10000`  | **1.25 ms**       | 1.49 ms       | Papaya  |
| `resolve_dense/100`      | **211 ns**        | 4.67 µs       | Papaya 22× |
| `resolve_dense/1000`     | **2.12 µs**       | 56.7 µs       | Papaya 27× |
| `resolve_dense/10000`    | **21.9 µs**       | 1.11 ms       | Papaya 50× |
| `mixed_st/1000`          | **382 µs**        | 587 µs        | Papaya  |
| `mixed_st/10000`         | **4.31 ms**       | 6.60 ms       | Papaya  |
| `mixed_mt/2`             | **1.40 ms**       | 1.83 ms       | Papaya  |
| `mixed_mt/4`             | **2.26 ms**       | 2.55 ms       | Papaya  |
| `mixed_mt/8`             | 11.3 ms           | **4.31 ms**   | Lasso   |

Papaya wins read-heavy paths dramatically (22-50× on dense resolve)
and most single-threaded and low-thread-count mixed workloads. Lasso
wins under heavy concurrent insert pressure (8+ threads all interning
new strings) because its per-shard write lock pattern serializes less
than ours at saturation.

### Real workload — `portage-bench` resolve

Loading the full Gentoo tree (~70k ebuilds) and running the pubgrub
solver on common targets.

| Bench                      | Papaya     | Lasso      | Winner |
|----------------------------|------------|------------|--------|
| `resolve/load/load_repo`   | 1.274 s    | 1.270 s    | tied   |
| `resolve/load/build_provider` | 632 ms  | **583 ms** | Lasso  |
| `resolve/targets/firefox`  | 199 ms     | **175 ms** | Lasso  |
| `resolve/targets/gcc`      | 176 ms     | **149 ms** | Lasso  |
| `resolve/targets/rust`     | 186 ms     | **151 ms** | Lasso  |
| `resolve/targets/openssh`  | 181 ms     | **147 ms** | Lasso  |
| `resolve/targets/python`   | 183 ms     | **149 ms** | Lasso  |

Lasso wins solver hot paths by 12-19%. Profiling indicates the gap is
not from interner operation cost itself (microbenchmarks favor papaya
here) but from adjacent inlining/code-layout effects we have not fully
explained.

### Real workload — `bench-regen.sh` (full Gentoo tree)

Metadata regeneration on `/var/db/repos/gentoo`. This workload spawns
bash subprocesses per ebuild, which dominates total time — interner
backend choice is invisible.

| Threads | Papaya real | Lasso real | Winner |
|---------|-------------|------------|--------|
| 1       | 2m15.6s     | 2m14.4s    | tied   |
| 20      | 11.64s      | 11.48s     | tied   |
| 24      | **10.41s**  | 10.46s     | tied   |
| 32      | 11.79s      | 11.49s     | tied   |

### Takeaway

For read-heavy workloads (CLI tools, query commands, anything that
resolves interned keys repeatedly) papaya is the clear winner. For
solver-heavy pipelines lasso edges ahead by 12-19%; for batch metadata
work the interner choice is invisible. The default ships papaya
because most consumers in this ecosystem are read-heavy.

## Installation

```toml
[dependencies]
gentoo-interner = "0.2"
```

## Usage

```rust
use gentoo_interner::{Interned, DefaultInterner};

let a = Interned::<DefaultInterner>::intern("amd64");
assert_eq!(a.resolve(), "amd64");

let b = Interned::<DefaultInterner>::intern("amd64");
assert_eq!(a, b); // Same key, cheap equality
```

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `interner` | Yes | Hybrid `papaya` + `boxcar` backend with sharded mutex on the insert slow path |
| `lasso` | No | Alternative `lasso::ThreadedRodeo` backend (mutually exclusive with `interner` at runtime; takes precedence if both enabled) |
| `serde` | No | Serde serialization |

## Breaking changes in 0.2

- `Interner` trait now requires `Clone` as a supertrait.
- Default `GlobalInterner` backend switched from `lasso` to `papaya`.
  Enable `--features lasso` to restore the old backend.

## License

MIT
