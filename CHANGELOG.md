# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] - 2026-11-10

### Added

- `cargo-clean [--apply] [--config PATH] [-v]` and the `[cargo_clean]` config section
  (`roots`, `exclude`, `max_age_secs`, `keep_final_debug`, `keep_final_release`,
  `exclude_time_machine`, `run`): Cargo projects are found by their `Cargo.toml` below `roots`;
  in their target and build dirs (workspace root, `build.target-dir` / `build.build-dir`, only
  with Cargo's `CACHEDIR.TAG`) the build units not rebuilt for `max_age_secs` are deleted for
  good, except those a fresh unit was built against. The final debug / release artifacts stay
  unless `keep_final_debug` / `keep_final_release` is `false`. A target with a profile a build
  holds locked is skipped. With `exclude_time_machine = true` every target and build dir found
  is also excluded from Time Machine. `run` runs it after purge.
- `run` ends with a total of what each step did (or would do in a preview) and the space freed,
  and starts with the binary version.
- Colors on a terminal for step headers, totals, warnings and hints (`NO_COLOR` turns them off).
- A progress line in `dedup` and `tm-exclude` too; the scan line counts the directories read.

- `dedup [--apply] [--config PATH] [-v]` and the `[dedup]` config section (`patterns`,
  `exclude`, `min_size`, `run`): identical regular files are replaced by APFS clones of one of
  them (found by size, clone id, then BLAKE3 of head and tail and of all bytes). Only files
  whose owner, group, mode, flags and xattrs equal the kept file's and that have no ACL are
  replaced; the clone is byte-compared before and after the `RENAME_SWAP`. Preview by default.
  `dedup` honours `verify`: with `"extended"` a compressed clone is also decoded in-process
  before the swap, and groups or files that cannot be verified that way are skipped with the
  reason (never replaced by an uncompressed clone). Files listed in `quarantine.txt` are never
  replaced; a file `dedup` replaced that is rewritten and found unshared on `quarantine_after`
  consecutive run days is added to `quarantine.txt` (stamp `<timestamp> dedup`). Streaks are kept
  in the new `dedup.tsv` in the state dir; `quarantine clear` resets them too. New counters
  `quarantined=` and `requarantined_new=` on stdout and in `dedup_summary`.
- `clean` also walks the `[dedup]` patterns (with their own excludes), so leftover staging and
  rescue dirs of `dedup` are found; it runs when either scope has patterns.
- `run` key in `[trash]`, `[purge]`, `[compress]` (new section), `[dedup]` and `[tm_exclude]`:
  `true`, `false` or `"dry-run-only"` (a preview even under `run --apply`). Defaults keep the old
  behaviour (compress on, the rest off). Order of `run`: trash, purge, compress, dedup,
  tm-exclude.

- `tm-exclude [--apply] [--config PATH]` and the `[tm_exclude]` config section (`patterns`,
  `exclude`, `scheduled`). Install the new binary before adding `[tm_exclude]` to an existing
  config: older binaries reject the unknown section. Matched directories get the sticky Time
  Machine exclusion attribute `tmutil addexclusion` sets (tmutil is not executed); an existing
  exclusion is never removed or changed. Preview by default; `run` does it after compress when
  `scheduled = true`.
- `[[backend_by_size]]` (config): routes files to a backend by size (`backend`, `min_size`,
  optional `max_size`, inclusive). Default: ditto for 16385..536870912 bytes, applesauce for
  larger files, which were skipped before. Overlapping ranges, `min_size > max_size` and a ditto
  range without `max_size` or above 512 MiB are refused at config load by every command that
  reads the config. A batch never mixes backends; every backend in use is probed, and the
  incompressible cache record lists all of them with their ranges, so the new default (and any
  later change of the ranges) drops the cache once. An applesauce file whose compressed
  resource fork would exceed 4 GiB is cached as incompressible.
  `--backend NAME` gives every file from the lowest `min_size` up to one backend for the run.
- `verify = "extended"` (config, the new default): the compare before the swap also decodes the
  copy's decmpfs data in-process (applesauce-core, now with LZVN too), independently of the
  kernel; decmpfs types other than 7, 8, 11, 12 fail the file. After the swap the file at the
  path is compared with the original once more (cached pages evicted); a mismatch swaps the
  original back, or rescues the staging dir when that is not safe. With ditto in use the probe
  must decode too. A run killed during that post-swap compare leaves a staging dir with
  `.acz-keep`, kept by later runs like a rescue dir.

### Changed

- Without `-v`, `trash`, `purge`, `cargo-clean`, `compress` (preview) and `dedup` (preview) show
  a progress line and the totals only; `-v` lists every item as before. Skips and per-file
  results go to the log; errors are still shown.
- A file that vanishes during the scan is logged as INFO instead of WARN.
- `trash`, `purge` and `cargo-clean` check their items once per directory instead of once per
  item, and the covered-item check is linear (a preview of 10^6 items took minutes).
- `scheduled` in `[trash]`, `[purge]` and `[tm_exclude]` is now an alias of `run = true|false`;
  giving both in one section is a config error.

- `verify` takes `false`, `true` or `"extended"`; the default is `"extended"` (was `true`). An
  existing `verify = true` (what `make config` wrote) keeps the plain compare; only a new
  config or an edit gets `"extended"`.
- License: GPL-3.0-or-later (was Apache-2.0), since the new applesauce backend links the
  GPL-3.0 `applesauce-core` crate.
- The log line of a file no backend range covers: "outside every backend size range, skipped".
- A top-level `min_size` alone now sets ditto's lower bound of the default routing (larger
  files go to applesauce); a top-level `backend` keeps the old single range
  `min_size`..536870912 and logs one INFO line per run. Both are refused together with
  `backend_by_size`, and a `min_size` above 536870912 is refused; the default config
  now spells the routing out instead.

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

- A directory whose read is interrupted by a signal (EINTR) is read again, up to 3 times,
  instead of being skipped for the day with `cannot read dir ...: Interrupted system call`.
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

[Unreleased]: https://github.com/aliaksandr-master/auto-compress-apfs/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/aliaksandr-master/auto-compress-apfs/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/aliaksandr-master/auto-compress-apfs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/aliaksandr-master/auto-compress-apfs/releases/tag/v0.2.0
