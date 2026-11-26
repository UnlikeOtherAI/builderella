# Working in this repository

Read `docs/architecture.md` first. It is the contract between crates.

## Rules

- Rust 2024, MSRV 1.89. Every crate must build and pass tests on Linux,
  macOS and Windows unless its section in the architecture says otherwise.
- `bella-core` is the shared contract. Other crates do not redefine its
  types. Changes to it are additive and called out in the change
  description.
- Library crates return typed errors (`thiserror`). No `unwrap`/`expect` on
  anything that can fail at runtime; no `println!` in libraries (use
  `tracing`).
- `unsafe` only for OS calls in `bella-contain` and `bella-probe`, each
  block with a `SAFETY:` comment.
- A dependency that only one crate needs is declared in that crate's
  `Cargo.toml`, with default features trimmed. Prefer the standard library
  and what the workspace already uses. No dependency may raise the MSRV or
  need a C toolchain beyond what `rusqlite` (bundled) and `zstd` already do.
- Never weaken an invariant from §3 or §4 of the architecture to make a
  test pass. If one cannot be met on a platform, report the weaker
  guarantee honestly (`advisory`, `unknown`) instead of claiming it.
- Tests are deterministic: inject `bella_core::Clock`, use temporary
  directories, bind to port 0, no sleeps longer than needed for a real OS
  effect, no network beyond loopback.
- Every acceptance scenario a crate is assigned has a test whose name or
  doc comment contains its id (for example `a27_deleted_file_does_not_survive`).

## Checks

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`scripts/dev/remote-check.sh` runs a command against the current commit on
another machine over SSH, for platform-specific verification.

## Layout

```
crates/        Rust workspace members (see the table in docs/architecture.md)
dashboard/     Web UI (React + TypeScript + Vite)
docs/          Architecture, recipe format, API, operations
scripts/       Development and install scripts
```
