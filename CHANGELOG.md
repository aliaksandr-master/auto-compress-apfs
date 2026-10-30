# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- License: GPL-3.0-or-later (was Apache-2.0), since the new applesauce backend links the
  GPL-3.0 `applesauce-core` crate.
- The log line of a file over 512 MiB no longer names ditto: "larger than 512 MiB (above the
  compression size limit), skipped".

- `auto-compress-apfs` without arguments prints the help instead of a compress preview; use
  `compress` for the preview. Options without a command still mean `compress`.
- The launchd agent's `run.sh` runs `run --apply` instead of `compress --apply`;
  `check-launchd` reports `REINSTALL` for an agent installed by an earlier version.
- Lower peak memory of compress runs: files below `min_size` are counted, not kept. The state
  row of a too-small file whose inode is found under two paths (hardlinks) is now always kept
  instead of being dropped at random.

### Added

- `backend = "applesauce"` (config) and `--backend ditto|applesauce` (compress): an opt-in
  in-process LZFSE compressor (`applesauce-core` 0.4.14, `lzfse-sys` 2.0.0) instead of
  `/usr/bin/ditto`. Copies go through the same staging and swap, are always byte-compared and
  must carry the original's xattrs; files with a resource fork or decmpfs xattr are skipped.
  Switching the backend drops the incompressible cache once.
- `trash` and `purge`: files and directories matched by the new `[trash]` / `[purge]` config
  rules (patterns, exclude) are listed, and with `--apply` moved to `~/.Trash` or deleted for
  good, a directory as a whole; each item is re-checked right before and written to
  `<log_dir>/removed.tsv`. Protected locations, their ancestors, the home directory, the tool's
  own directories and items with flagged or dataless entries are refused.
- `run`: `trash` and `purge` for rules with `scheduled = true`, then `compress`; this is what
  the launchd agent runs.
- `install-launchd`: writes `run.sh` next to the config and a per-user launchd agent
  (`~/Library/LaunchAgents/local.auto-compress-apfs.plist`) that starts it at login and daily at
  09:00. `run.sh` runs `run --apply` once per day from 09:00 on, so a run missed while the
  Mac was asleep or off starts once later; when the binary is gone it only logs to
  `launchd.log` and exits 0. `uninstall-launchd` unloads the agent and removes both files.
- `check-launchd`: read-only; prints `OK` (exit 0) when the agent is installed, loaded and
  written by this version, else `INSTALL` or `REINSTALL` with the reason and the command to run
  (exit 1, keeping the config the installed agent runs); exit 2 when the check itself fails.

### Fixed

- The release binary keeps its symbol table (`strip = false`): an endpoint security agent
  quarantined the stripped build right after it was linked.
- macOS 27: ditto clones files by default there and a clone is never compressed, so
  `--hfsCompression` alone compressed nothing and every file was cached as incompressible.
  `compress --apply` now probes ditto first with a small file next to the config
  (`ditto-probe.txt`, copy in `ditto-probe.out/`) and uses `--hfsCompression --noclone` or
  `--hfsCompression`, whichever compresses; when neither does, the run stops before the walk
  with exit code 1 and changes nothing.
- The incompressible cache filled by such runs is dropped once and its files are checked again;
  the options that filled the cache are now recorded next to it (`ditto-args.txt`).

## [0.3.0] - 2026-10-20

### Added

- `clean`: lists the leftover `.acz-staging-*` and `.acz-rescue-*` directories below the
  configured patterns with their age, size and which files have no copy elsewhere; with
  `--apply` deletes them under the run lock. Directories holding flagged, dataless or
  foreign-filesystem entries, and staging or rescue directories changed less than 6 hours ago,
  are never deleted.

### Changed

- Files larger than 512 MiB are skipped before ditto and counted as `too_large` in the summary
  line: measured on current macOS, `ditto --hfsCompression` leaves them uncompressed. They no
  longer cost a full staged copy on every first run and are no longer kept in the
  incompressible cache.
- `compress --apply` started while another run holds the lock prints
  `a run is in progress, exiting` to stderr (exit code still 0).

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

[Unreleased]: https://github.com/aliaksandr-master/auto-compress-apfs/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/aliaksandr-master/auto-compress-apfs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/aliaksandr-master/auto-compress-apfs/releases/tag/v0.2.0
