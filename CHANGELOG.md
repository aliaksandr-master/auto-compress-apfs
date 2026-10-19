# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `clean`: lists the leftover `.acz-staging-*` and `.acz-rescue-*` directories below the
  configured patterns with their age, size and which files have no copy elsewhere; with
  `--apply` deletes them under the run lock. Directories holding flagged, dataless or
  foreign-filesystem entries, and staging or rescue directories changed less than 6 hours ago,
  are never deleted.

## [0.2.0] - 2026-10-09

First public release.

### Added

- `compress` (default command): preview by default, `--apply` compresses with
  `/usr/bin/ditto --hfsCompression` in batches; each file is replaced only by a verified
  `RENAME_SWAP` with its compressed copy.
- Run-time scoping: `--dir`, `--search`, `--exclude`, `--ignore-global-search`.
- `stat`: what was compressed and how much was saved (from `state.tsv`).
- Quarantine: files found decompressed again on `quarantine_after` consecutive run days are no
  longer compressed; `quarantine list` / `quarantine clear`.
- Hard protections: only the internal APFS data volume (plus `allow_volumes`), no cloud,
  `/Volumes` or system subtrees, privacy-protected folders only by explicit roots, flagged and
  dataless items never touched.
- The default config has no `patterns`: nothing is compressed until patterns are configured or
  `--search` is given; until then `compress` prints a hint and exits 0.
- Archives and other already-compressed formats are excluded by default.
- Example configs in `examples/`.

[Unreleased]: https://github.com/aliaksandr-master/auto-compress-apfs/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/aliaksandr-master/auto-compress-apfs/releases/tag/v0.2.0
