# Contributing

Issues and pull requests are welcome. This tool replaces user files in place, so changes to the
walk, compress, verify and swap paths get extra scrutiny; please keep them small and tested.

## Build and test

```sh
cargo build
make lint test        # cargo fmt --check, cargo clippy --all-targets -- -D warnings, cargo test
```

`make lint test` must be green before a pull request.

- Tests build their trees under `std::env::temp_dir()` (must be APFS) and run the binary with a
  temporary `HOME`, so they never touch your real config, log, cache, state or lock.
- Never run `compress --apply` on real data while developing. The default command is a
  read-only preview; use it, or `--apply` on a throwaway tree.

## Rules

- Rust edition 2024, stable toolchain, macOS only.
- Dependencies are limited to `serde`, `toml` and `globset`. Propose anything new in an issue
  first.
- `unsafe` only for libSystem calls std lacks, each wrapper in `src/sys.rs` with a `SAFETY`
  comment.
- The original file may only be replaced by the verified `RENAME_SWAP` of a compressed copy, and
  only `/usr/bin/ditto` is executed, by absolute path.
- A bug fix comes with a test that fails without it; a feature comes with its tests.
- Code, comments, docs and commit messages in English.

## Commit style

Short imperative subject in lower case (`fix quarantine streak on narrowed runs`), a body that
says why when it is not obvious. One logical change per commit.
