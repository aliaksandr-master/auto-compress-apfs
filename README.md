# auto-compress-apfs

Turns on APFS transparent compression for files matched by glob patterns, using only Apple's
standard tool `/usr/bin/ditto --hfsCompression`. Compressed files stay readable by every program
as usual; the filesystem decompresses them on read. Good targets are large trees of small, rarely
rewritten, well compressible files: Python virtualenvs, package caches, `node_modules`.

- Preview first: without `--apply` it only lists what it would compress.
- Nothing is compressed until you configure `patterns` (empty by default) or pass `--search`.
- The original file is replaced only by an atomic swap with a verified, compressed copy.

Status: v0.2, macOS only. Run it on data you have a backup of (Time Machine or similar).

## Requirements

- macOS with the files on the internal APFS data volume (other local APFS volumes only when
  listed in `allow_volumes`).
- Rust >= 1.89 to build (`rustup` stable is fine).

## Install

```sh
cargo install --locked auto-compress-apfs
```

The binary lands in `~/.cargo/bin/auto-compress-apfs`. From a source checkout:

```sh
cargo install --locked --path .
# or: make install  (release build into release/, symlink ~/.local/bin/auto-compress-apfs,
#                    copy config/default.toml to ~/.config/auto-compress-apfs/ if missing)
```

**Update:** run the same `cargo install --locked auto-compress-apfs` again; `cargo install --list`
shows the installed version, `auto-compress-apfs --version` the one on your `PATH`.

**Uninstall:**

```sh
cargo uninstall auto-compress-apfs
rm -rf ~/.config/auto-compress-apfs ~/Library/Logs/auto-compress-apfs \
       ~/Library/Caches/auto-compress-apfs "$HOME/Library/Application Support/auto-compress-apfs"
```

After a from-source `make install`, also remove the symlink `~/.local/bin/auto-compress-apfs`
and the checkout's `release/` directory.

Files already compressed stay compressed and readable; nothing has to be undone.

## Quick start

```sh
auto-compress-apfs                     # first run: creates ~/.config/auto-compress-apfs/default.toml
                                       # and says that no patterns are configured
$EDITOR ~/.config/auto-compress-apfs/default.toml   # e.g. patterns = ["~/projects/**/.venv"]
auto-compress-apfs                     # preview: what would be compressed, nothing changes
auto-compress-apfs compress --apply    # compress
auto-compress-apfs stat                # what was compressed, how much was saved
```

A one-off run without touching the config:

```sh
auto-compress-apfs --dir ~/projects --search '**/.venv'            # preview
auto-compress-apfs --dir ~/projects --search '**/.venv' --apply    # compress
```

To run it regularly, schedule `auto-compress-apfs compress --apply` with the scheduler of your
choice. A run on an unchanged tree spawns no ditto at all, and a run started while another one
holds the lock exits 0 at once.

## How it works (safety model)

1. **Walk.** Every pattern is split into a literal prefix (the walk root) and a glob. The walk runs
   on a thread pool, never follows symlinks below the walk root (a walk root that is itself a
   symlink to a directory, like `/tmp`, is followed for searching only and never compressed),
   never crosses into another filesystem
   (`one_file_system`), and only descends into directories that can still lead to a match
   (`~/projects/**/.venv` never reads `~/Library`, `~/Documents`, ...). A matched directory means
   every regular file below it, recursively. Excludes are applied everywhere, inside matches too,
   to files and directories (an excluded directory is never read), and always win over a pattern.
2. **Hard protections** (no config or flag disables them):
   - Only the internal data volume is walked: before the first `read_dir` of a root, `statfs`
     must report `apfs`, `MNT_LOCAL` and the filesystem id of `/System/Volumes/Data` (or of a
     local apfs mount point listed in `allow_volumes`); otherwise one WARN and the root is not
     read. Network, cloud and external filesystems are never touched, not even with a stat of
     their contents.
   - `~/Library/CloudStorage`, `~/Library/Mobile Documents`, `/Volumes`, `/System`, `/dev`,
     `/Network` and `/net` are never walked (checked lexically, before any syscall; they share
     the data volume's filesystem id, so the `statfs` check alone would not catch them).
     Every root is also checked by its real path (realpath: symlinks in any component, on-disk
     case), so `~/docs -> ~/Documents`, `~/cl -> ~/Library/CloudStorage`, `~/library/...` or a
     symlink to an ancestor of a protected folder are refused. `/System/Volumes/Data/x` is
     treated as `/x`.
   - A root that lies inside an excluded directory (by name or path) or below a directory with
     `uchg`, `uappnd`, `schg` or `sappnd` is skipped with one INFO line (`restricted` is not
     checked on ancestors: `/usr` carries it while `/usr/local` does not).
   - The privacy-protected (TCC) folders `~/Desktop`, `~/Documents`, `~/Downloads`,
     `~/Pictures`, `~/Movies`, `~/Music` and `~/Library` are entered only when a walk root lies
     inside them (a pattern like `~/Downloads`, or `--dir ~/Downloads/x`), so a broad pattern
     such as `~/**/.venv` never triggers a permission prompt. A root that cannot be read for
     lack of permission gets one ERROR naming the binary to grant Full Disk Access to.
   - Files and directories with `uchg`, `uappnd`, `schg`, `sappnd` or `restricted` are never
     touched: a flagged file is skipped, a flagged directory is not entered at all (whole
     subtree); one INFO line each names the flags.
   - Dataless files and directories (cloud placeholders) are never read or entered, and the
     process turns off their materialization (`setiopolicy_np`), so an accidental read fails
     instead of downloading.
3. **Classify** from the `lstat` data of the walk, without extra syscalls: already compressed,
   flagged, dataless, too small, hardlinked, owned by another user, setuid/setgid, modified in
   the last `min_age_secs`, known incompressible (cache), quarantined, or to compress.
4. **Compress in batches.** Files of one match root (and device) are copied by one
   `ditto --hfsCompression -- <files...> <root>/.acz-staging-<pid>-<n>/` call (up to `batch_size`
   files and 2 GiB of data; basenames unique per batch ignoring case, because APFS volumes are
   usually case- and normalisation-insensitive and ditto flattens all sources into one directory;
   staged copies that still end up sharing one entry are never committed, each such file is
   retried alone). When the match root is read-only or on another device, staging happens in each
   file's parent directory instead. So a run spawns about one process per 1000 files instead of
   one per file. Basenames repeated across directories (`__init__.py`, `METADATA`) need one batch per
   repetition, so a big venv's first run takes as many ditto calls as its most frequent basename
   occurs; batches run in parallel. For every staged copy:
   - the original must still be the inode the walk saw (dev, inode, size, mtime) and uncompressed;
   - the copy gets the original's atime/mtime, must be a compressed regular file with the same
     size, mode, mtime and owner (the group is fixed with `chown` when it differs); a complete but
     uncompressed copy from a successful ditto call means the data is incompressible and is
     remembered in the cache (from a failed call the file is retried alone first);
   - with `verify = true` the original (opened once, `O_NOFOLLOW`, inode re-checked) and the copy
     are byte-compared;
   - `renamex_np(RENAME_SWAP)` atomically exchanges copy and original; the swapped-out file must
     still be the expected version, otherwise the swap is reversed and the file skipped;
     only then the old original is unlinked.

   If ditto fails for a batch, every staged copy that passes all checks is still committed (always
   byte-compared, whatever `verify` says) and each remaining file is retried alone, so one bad file
   costs one ERROR line and nothing else. After 3 consecutive failed retries (or when the staging
   dir vanished) the rest of the batch is given up with one ERROR line. Copies from a timed-out
   call (15 min) are never committed. Assumption (not measured): LZVN compresses fast
   enough that one batch of at most 2 GiB (or one larger file, which always gets a batch of its
   own) finishes well within the 15 minutes.

   Before every ditto call the free space of the volume is checked: if the batch's data plus
   `min_free_bytes` (10 GiB) does not fit, its files are skipped (one WARN per run, counter
   `skipped_low_space`) and no copy is made. A directory found not writable (staging or swap
   fails with EACCES/EPERM) is remembered for the rest of the run: its other files get one
   ERROR each ("parent dir not writable") without another copy. Nothing is ever `chmod`ed.

The original stays the source of truth until the verified atomic swap; every failure leaves it
untouched. Only `/usr/bin/ditto` is executed, by absolute path.

## Usage

```
auto-compress-apfs [compress] [OPTIONS]    preview (default); with --apply: compress
auto-compress-apfs stat [--top N] [--all] [--config PATH]
auto-compress-apfs quarantine list [--config PATH]
auto-compress-apfs quarantine clear <PATH>|--all [--config PATH]
auto-compress-apfs -h|--help | -V|--version
```

- **Preview** (no arguments, or `compress` without `--apply`): walks per config and prints to
  stdout every not yet compressed file that would be compressed (`~/path   size`), then the
  skipped files (`SKIP(<kind>): reason`), totals with the 20 biggest roots, and the exact command
  that would do it: `Nothing changed. Run 'auto-compress-apfs compress --apply ...' to compress
  these N files (X GiB).` It never spawns ditto and never writes staging dirs, the cache,
  `state.tsv` or `quarantine.txt`; it may create the config, the log dir and the log, and like
  every run it empties `<log_dir>/launchd.log` once that file exceeds 1 MiB.
  `--dry-run` is kept as a hidden alias.
- `compress --apply` does the work.
- Without any pattern (empty `patterns` in the config and no `--search`) `compress` prints one
  line naming the config file and how to add patterns, and exits 0 without walking, locking or
  writing anything (besides creating a missing config).
- Options of `compress`:
  - `--config PATH` overrides `~/.config/auto-compress-apfs/default.toml` (created from the
    built-in default when missing).
  - `--dir DIR` restricts the walk to DIR's subtree, intersected with the active patterns; when
    DIR lies inside a match (e.g. inside a `.venv`) every file below it is a candidate. DIR must
    exist; it goes through the same volume/cloud/TCC guard as any root (an explicit `--dir`
    inside `~/Downloads` counts as opting in).
  - `--search GLOB...` adds patterns for this run; a glob not starting with `/` or `~` is
    relative to `--dir` (or the current directory): `--dir ~/projects --search '**/.venv' '**/*.csv'`.
  - `--exclude GLOB...` adds excludes (same syntax as in the config).
  - `--ignore-global-search` ignores the config's `patterns` and `exclude`; only `--search` and
    `--exclude` apply (without `--search` it says so and exits). The hard protections above, the
    hardlink/owner/setuid skips and the quarantine always apply.
  - `--progress` / `--no-progress`: interactive output on stderr is on when stderr is a terminal.
    Then each file event is one line (`~/path   123.3 MiB ->   11.3 MiB = 90.8%`,
    `ERROR(<kind>): ...`, `SKIP(<kind>): ...`) above a progress line (`scanning... N files
    found`, then percent, bar, file counts, bytes and an estimate, by bytes). Width from
    `COLUMNS` (else 100), colours unless `NO_COLOR` is set. When stderr is not a terminal
    nothing extra is written.
  - `-v` logs DEBUG lines and mirrors the log to stderr.
- `stat` reads only `state.tsv` and `quarantine.txt` (no walk, no lock, no log, no config
  parse): totals (files, original, on disk, saved, ratio, quarantined, last run day), the files
  sorted by original size (top 30, `--top N`, `--all`), path left-truncated to `COLUMNS` (else
  160), and the quarantine.
- `quarantine list` prints the quarantine; `quarantine clear <PATH>|--all` releases entries and
  resets their streak (takes the run lock; exits 1 while a run holds it).

Exit code 0 when the run finished (per-file errors are logged, not fatal) and when another run holds
the lock; 1 on a fatal error (bad config, log cannot be opened, unreadable `quarantine.txt`, or
with `--apply` an unreadable `state.tsv`, ...).

## Configuration

The config lives in `~/.config/auto-compress-apfs/default.toml` and is created from the built-in
default on the first run. See [`config/default.toml`](config/default.toml) for every key with its
default and meaning, and [`examples/`](examples) for complete configs (Python venvs, the uv cache,
`node_modules`, `~/Downloads`).
`~` / `~/` expands to `$HOME` in `patterns`, `exclude`, `allow_volumes` and `log_dir`; unknown keys
are an error. Glob syntax: `*` and `?` never cross `/`, `**` matches any number of directories,
`[...]` and `{a,b}` work as usual. Matching uses the walked path as written (no symlink
resolution), so `exclude = ["/private/var/**"]` does not exclude a walk started at `/var/...`.

- `patterns` (empty by default) must be absolute after `~` expansion; `X/**` matches X's contents.
- `exclude`: a pattern without `/` matches an entry **name** at any depth (`node_modules`,
  `*.log`, `*.{gz,zip}`); a pattern with `/` is a full-path glob (`~/projects/**/big-venv`) and
  must be absolute (or start with `**/`). `X/**` also excludes the directory X itself, so it is
  never read. The default list skips `node_modules`, `.git`, `target`, archives and other
  already-compressed formats
  (`*.{xz,gz,tgz,zip,zst,bz2,lz4,lzma,txz,tbz2,tbz,whl,jar,xip,parquet,png,jpg,jpeg,7z,rar,dmg,mp4,mov,webp,heic,pkg}`)
  and partial downloads (`*.crdownload`, `*.part`, `*.download`). Setting `exclude` replaces
  this list, so repeat the entries you want to keep.
- `allow_volumes`: mount points of further local apfs volumes that may be walked.
- `quarantine_after` (3, 0 disables), `min_free_bytes` (10 GiB): see below.

**Full Disk Access.** `~/Downloads` is a TCC-protected folder. A run that walks it needs Full Disk
Access (or Downloads access) for the binary (System Settings > Privacy & Security); without it the
run logs one ERROR naming the binary's real path and goes on with the other roots. Hypothesis: the
ad-hoc signature changes with every rebuild, so macOS may ask again after a rebuild.

## Logs and state

- `~/Library/Logs/auto-compress-apfs/auto-compress-apfs.log`: one line per event,
  `YYYY-MM-DDTHH:MM:SS.mmm+HH:MM LEVEL message`. INFO `compressed <path> <size> -> <allocated>` per
  file, ERROR with path and reason per failure, a start line and a `summary` line per run
  (`found`, `already_compressed`, `to_compress`, `compressed`, `incompressible`, `too_small`,
  `too_new`, `skipped`, `errors`, `bytes_before`, `bytes_after`, `ditto_calls`, `stale_staging`,
  `quarantined`, `requarantined_new`, `refused_roots`, `dataless`, `excluded`, `flagged`,
  `skipped_low_space`, `dry_run`, `elapsed`).
- Rotation: at `log_max_bytes` (4 MiB) the log moves to `.log.1` (`log_keep` = 1 rotated file).
  `<log_dir>/launchd.log` (for stdout/stderr redirected there, if any) is emptied at the start of
  every run, preview or `--apply`, once it exceeds 1 MiB; it is the 1 MiB reserve in the bound.
  Worst case 4 MiB x 2 + 1 MiB = 9 MiB; the config is rejected if the bound exceeds 10 MiB. A huge first run can rotate away its own beginning; the summary line is always in the
  current log.
- `~/Library/Caches/auto-compress-apfs/incompressible.tsv`: files whose data did not compress, so a
  run on an unchanged tree spawns no ditto at all.
- `~/Library/Application Support/auto-compress-apfs/run.lock`: single-instance lock.
- `~/Library/Application Support/auto-compress-apfs/state.tsv`: every file compressed by this
  tool (path, dev, inode, original size, size on disk, time, streak, last day found decompressed
  again), the source of `stat`. Files found already compressed under the patterns are added with
  an unknown time (`-`), so files compressed earlier or by other tools show up after the first
  run. Rows of files that are gone are dropped when their directory was listed completely
  in an unrestricted run; this also drops rows of files that are now excluded (history only,
  nothing on disk changes). Written atomically (synced, then renamed) at the end of every
  `--apply` run, after `quarantine.txt`. A narrowed `--apply` run (`--config`, `--dir`,
  `--search`, `--exclude` or `--ignore-global-search`; the state is shared by every config of a
  user) only adds and updates rows: it never drops any, does not count as a run day and does not
  change streaks. An existing `state.tsv` that cannot be read or has
  no `#v1` header stops an `--apply` run with an ERROR (exit 1) and is left untouched; a preview
  goes on without it.
- `~/.config/auto-compress-apfs/quarantine.txt` (next to the config): see Quarantine.

## Quarantine

A file this tool compressed that is found decompressed again (rewritten by a program) on
`quarantine_after` (3) consecutive run days is not compressed any more: it is appended to
`quarantine.txt` (`<timestamp><TAB><path>`, with an explanatory header) with one INFO line, and
every later run skips it silently (counter `quarantined`). Days without a run do not break the
streak; two runs on one day count once. Run days and streaks count only unrestricted runs (the
default config, no `--dir`/`--search`/`--exclude`/`--ignore-global-search`); a narrowed run skips
listed files and files whose streak already reached the threshold, and compresses the rest
without touching their streak. If `quarantine.txt` cannot be
written, the file is still not compressed, one ERROR is logged and the next run retries.
Release only by hand: delete the line, or run
`auto-compress-apfs quarantine clear <path>` (or `--all`). Comments and lines you add are kept.
By design, files rewritten every day (for example dumps or exports regenerated daily) reach the
quarantine after three run days.

## Risks and limits

- **Back up first.** The tool replaces files in place (see the safety model above). Keep a
  backup of anything you point it at.
- **uv clones.** Fact (uv docs, `link-mode`): on macOS uv installs packages into a venv with
  `clone` (APFS copy-on-write) by default, so the venv's files share disk blocks with the uv cache
  (`~/.cache/uv`). Compressing the venv copy writes new blocks and un-shares them: disk usage can
  grow instead of shrink as long as the cache copy stays. Mitigations: run `uv cache prune` /
  `uv cache clean` after compressing, or restrict `patterns` to venvs not created by uv, or create
  them with `UV_LINK_MODE=copy`.
- **uv cache.** Compressing `~/.cache/uv` means new venvs cloned from it get already-compressed
  files. Fact (`uv help sync`, uv 0.12.17): uv's default link mode on macOS is `clone`. Fact
  (checked with `cp -c` on APFS): a clone of a compressed file is compressed too, and writing to
  the clone leaves the original intact.
- **New inode.** The compressed file is a new inode: birthtime changes, a process that holds the
  old file open keeps reading the old data until it reopens it; hardlinked files are skipped.
  Extended attributes, ACLs, mode, owner, group, atime and mtime are preserved.
- Files modified within `min_age_secs` (default 5 min) are skipped to avoid racing writers.
- Recompiling a package or `pip install` rewrites files uncompressed; the next run picks them
  up again (until the quarantine stops it, see above).
- **Verify cost.** With `verify = true` every file is read twice (original and copy), so a 1.5 GiB
  dump costs 3 GiB of reads.
- **Side effects of the swap** (new inode): Time Machine backs the swapped files up once more and
  Spotlight reindexes them. Hypothesis, not verified: the `com.apple.macl` attribute may not
  survive the copy, so per-file app grants on files in `~/Downloads` may be lost.
- **Rescue dirs.** `.acz-rescue-<pid>-<n>/` directories hold files the tool did not dare to
  delete. The tool never walks, compresses or removes them, so they stay until a human removes
  them: read the ERROR/WARN lines that name them, restore what you need, then delete the dir by
  hand. One can hold up to a whole batch of copies (up to 2 GiB). They appear when
  - a swap could not be reversed after a detected race (practically never); the staging dir is
    first marked with an `.acz-keep` directory and then renamed; if that rename fails it keeps its
    `.acz-staging-*` name and stale cleanup leaves it alone (WARN `kept rescue staging dir`);
  - a crashed run left a staging dir holding anything but compressed copies (for example the
    original of a file swapped right before the crash): the next run renames it instead of
    deleting it.
- Leftover `.acz-staging-<pid>-<n>` directories holding only compressed copies (a crash mid-run)
  are removed by the next run.
- Residual limitation: if the process dies in the microsecond window between a swap and its
  post-check, AND the original had been replaced concurrently by an inode that is itself already
  compressed, the swapped-out file in staging looks exactly like a staged copy, and the next run's
  stale cleanup deletes it.

## Development

```sh
make lint test        # cargo fmt --check, clippy -D warnings, cargo test
```

Tests create their trees under `std::env::temp_dir()` (APFS) and run the binary with a temporary
`HOME`, so they never touch the real config, log, cache or lock. See
[CONTRIBUTING.md](CONTRIBUTING.md); security issues: [SECURITY.md](SECURITY.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
