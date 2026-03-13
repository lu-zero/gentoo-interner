# Project Conventions

## Build Commands

```bash
cargo test                        # Run all tests (unit + doc)
cargo clippy -- -D warnings       # Lint — must be warning-free
cargo fmt --check                 # Format check — must pass
cargo doc --no-deps               # Build docs — must have no warnings
```

## Architecture

- Single module crate (`lib.rs`) — small enough to keep in one file
- Public API: `Interner` trait, `Interned<I>` wrapper, `GlobalInterner`, `NoInterner`, `DefaultInterner`
- `Interner` trait uses static methods; `Interned<I>` carries `PhantomData<I>`
- Feature-gated implementations:
  - `interner` feature (default): `GlobalInterner` using `lasso`
  - No `interner`: `NoInterner` using `Box<str>`

## Dependencies

Minimal. Any new dependency must be justified.

Current dependencies:
- `lasso` (multi-threaded feature, optional) — string interning; gated behind `interner` feature
- `serde` (optional) — serialization support

## Coding Style

- `rustfmt` — all code must be formatted
- No dead code, no unused dependencies
- Doc comments on all public types and methods
- Tests in `#[cfg(test)] mod tests` block

## Commits

[Conventional Commits](https://www.conventionalcommits.org/):

- `feat:` — new functionality
- `fix:` — bug fix
- `refactor:` — code restructuring without behaviour change
- `docs:` — documentation only
- `test:` — adding or updating tests
- `ci:` — CI/CD changes
- `chore:` — maintenance (dependencies, tooling)

Use `{tag}!:` when the commit breaks the API.

## MSRV

Minimum Supported Rust Version is **1.85** (edition 2024).
