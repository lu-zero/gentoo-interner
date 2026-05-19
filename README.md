# gentoo-interner

[![LICENSE](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Crates.io](https://img.shields.io/crates/v/gentoo-interner.svg)](https://crates.io/crates/gentoo-interner)
[![docs.rs](https://docs.rs/gentoo-interner/badge.svg)](https://docs.rs/gentoo-interner)

String interning for Gentoo-related Rust crates.

## Features

- Process-wide deduplication via `papaya` lock-free hashmaps (default)
- Optional `lasso` arena-backed backend for benchmarking
- `Box<str>` fallback when interning disabled
- Optional serde support
- `Copy` types with global interner (4 bytes)

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
| `interner` | Yes | Global interning via `papaya` (lock-free) |
| `lasso` | No | Alternative `lasso` arena backend (for benchmarking) |
| `serde` | No | Serde serialization |

## Breaking changes in 0.2

- `Interner` trait now requires `Clone` as a supertrait.
- Default `GlobalInterner` backend switched from `lasso` to `papaya`.
  Enable `--features lasso` to restore the old backend.

## License

MIT
