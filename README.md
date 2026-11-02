# auto-compress-apfs

Turns on APFS transparent compression for files matched by glob patterns, using Apple's
standard tool `/usr/bin/ditto --hfsCompression` for files up to 512 MiB and an in-process LZFSE
writer (`applesauce`, built on the `applesauce-core` crate) for larger ones
(`[[backend_by_size]]`). Compressed files stay readable by every program as usual; the
filesystem decompresses them on read. Good targets are large trees of small, rarely
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
# or: make release  (release build into release/)
#     make install  (copy it to release/installed/, symlink ~/.local/bin/auto-compress-apfs,
#                    copy config/default.toml to ~/.config/auto-compress-apfs/ if missing)
```

**Update:** run the same `cargo install --locked auto-compress-apfs` again; `cargo install --list`
shows the installed version, `auto-compress-apfs --version` the one on your `PATH`.

**Uninstall:**

```sh
auto-compress-apfs uninstall-launchd    # only if install-launchd was used
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
     local apfs mount point listed in `allow_volumes`; `/`, `/System/Volumes/Data` and any
     ancestor of the home directory are ignored there with a WARN); otherwise one WARN and the
     root is not read. Network, cloud and external filesystems are never touched, not even with a stat of
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
   flagged, dataless, too small (below the lowest `min_size` of `backend_by_size`), outside
   every backend size range (a gap between ranges, or above a bounded top range), hardlinked, owned by another user,
   setuid/setgid, modified in the last `min_age_secs`, known incompressible (cache), quarantined,
   or to compress with the backend whose size range holds the file. A ditto range may not go
   above 512 MiB (measured on current macOS: ditto compresses a file of exactly 512 MiB and
   leaves anything larger uncompressed; neither its options nor the private Bom.framework
   copier options behind it, including AppleFSCompression's `kAFSCAllowLargeResourceForks`,
   lift the limit), so the default routing gives larger files to applesauce.
4. **Probe ditto.** Before the walk, `compress --apply` copies `ditto-probe.txt` next to the
   config (written there if missing, changed or compressed) into `ditto-probe.out/` with
   `--hfsCompression --noclone`, then with `--hfsCompression` alone, and uses the first set that
   yields a compressed, byte-identical copy (one INFO line names it). Since macOS 27 ditto clones
   files by default and a clone is never compressed; older ditto may not know `--noclone`. When
   the config dir is not writable or not on APFS the probe runs in the state dir (one WARN line).
   A config dir synced elsewhere (dotfiles repo, cloud folder) gets these two entries too. When
   no set compresses, the run stops before the walk with exit code 1 and changes nothing:
   otherwise every file would be cached as incompressible. The options that filled the
   incompressible cache are recorded next to it (`ditto-args.txt`); a cache filled with other
   options, or one without a record while the probe needs `--noclone`, is dropped once and its
   files are checked again. A preview runs no ditto and keeps showing the cache until then.
   Every backend the routing uses is probed. For applesauce the probe stages `ditto-probe.txt`
   with the in-process writer into `ditto-probe.out/` and requires a compressed copy that reads
   back byte-identical. The cache record lists, per backend in use (ditto first), its options or
   `applesauce lzfse <applesauce-core> <lzfse-sys>` (the pinned versions), then `@min..max` of
   each of its ranges, so any routing change (a bound too) or version change drops the cache
   once.
5. **Compress in batches.** Files of one match root (and device) are copied by one
   `ditto <probed options> -- <files...> <root>/.acz-staging-<pid>-<n>/` call (up to `batch_size`
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
     are byte-compared; with `verify = "extended"` (the default) in the same pass the copy's
     decmpfs data (types 7, 8, 11, 12) is also decoded in-process by applesauce-core,
     independently of the kernel, and must match too (a ditto copy is `F_FULLFSYNC`ed first;
     stored data of another type, a bad block table or a wrong block size fails the file);
     `verify = false` skips the compare of ditto copies. An existing `verify = true` (what
     `make config` wrote so far) keeps the plain compare; only a new config or an edit gets
     `"extended"`;
   - `renamex_np(RENAME_SWAP)` atomically exchanges copy and original; the swapped-out file must
     still be the expected version, otherwise the swap is reversed and the file skipped;
   - with `verify = "extended"` the path is then opened once more (`O_NOFOLLOW`): if it is still
     the swapped-in copy (inode, size, mtime, compressed), its cached pages are evicted and it is
     byte-compared with the original still in staging (protected by `.acz-keep` meanwhile). A
     mismatch swaps the original back (ERROR verify); if the copy changed in that window or the
     swap back fails, the staging dir is rescued instead. A path that no longer holds the copy
     (the user wrote to it right after the swap) is left as is;
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

   **applesauce** (the files in its `backend_by_size` ranges, or every file with
   `--backend applesauce`) uses the same batches (a batch never mixes backends),
   staging dirs and commit checks, but makes each copy in-process instead of with ditto: the
   original is opened `O_NOFOLLOW` and must still be the walked inode; a file that already has a
   `com.apple.ResourceFork` or `com.apple.decmpfs` xattr is skipped. The copy is written in
   `<staging>/.acz-part/` (xattrs via `fcopyfile`, then the whole file read in 64 KiB blocks and
   compressed with LZFSE into the resource fork, or inline into the decmpfs xattr for one small
   block; then the decmpfs header, ACL, the current mode, birthtime, the user flags plus
   `UF_COMPRESSED`, and `fsync` last) and
   renamed next to the other staged copies only when complete. A file is incompressible when
   the stored bytes rounded up to 4 KiB are not below its size rounded up, decided only after
   the whole file was read unchanged. Every applesauce copy is byte-compared before the swap,
   whatever `verify` says, and its xattrs must match the original's: same names, same values,
   except `com.apple.provenance` (assigned by the kernel to every file the process creates and
   not writable, ditto's copies get the same) and the value of `com.apple.quarantine`
   (rewritten by copyfile with a new timestamp and without the agent, as ditto does; only its
   flags field must match). Its range may be unbounded, but a file whose compressed resource
   fork would exceed 4 GiB (32-bit offsets) stops being read at that point and is cached as
   incompressible; the original stays. A file named `.acz-part` is never
   compressed by this backend (its copy would clash with the part dir). A full volume
   (`ENOSPC`) while writing a copy skips the rest of the batch as low space. The birthtime is
   kept, also when the mtime is older than it (it is set again after the mtime).
   A crash mid-copy leaves a `.acz-part/` directory, which stale cleanup removes with the
   staging dir.

The original stays the source of truth until the verified atomic swap; every failure leaves it
untouched. Only `/usr/bin/ditto` is executed, by absolute path.

## Usage

```
auto-compress-apfs                         this help
auto-compress-apfs compress [OPTIONS]      preview; with --apply: compress
auto-compress-apfs trash [--apply] [--config PATH]
auto-compress-apfs purge [--apply] [--config PATH]
auto-compress-apfs tm-exclude [--apply] [--config PATH]
auto-compress-apfs dedup [--apply] [--config PATH] [-v]
auto-compress-apfs run [--apply] [--config PATH]
auto-compress-apfs stat [--top N] [--all] [--config PATH]
auto-compress-apfs quarantine list [--config PATH]
auto-compress-apfs quarantine clear <PATH>|--all [--config PATH]
auto-compress-apfs clean [--apply] [--config PATH]
auto-compress-apfs install-launchd [--config PATH]
auto-compress-apfs uninstall-launchd [--config PATH]
auto-compress-apfs check-launchd [--config PATH]
auto-compress-apfs -h|--help | -V|--version
```

- **Preview** (no arguments, or `compress` without `--apply`): walks per config and prints to
  stdout every not yet compressed file that would be compressed: on a terminal `~/path   size`
  with the size column right-aligned to `COLUMNS` (120 when unset) and long paths cut on the left
  with `...`; piped or redirected `~/path size` (full path, one space), so nothing is lost. Then the
  skipped files (`SKIP(<kind>): reason`), totals with the 20 biggest roots, and the exact command
  that would do it: `Nothing changed. Run 'auto-compress-apfs compress --apply ...' to compress
  these N files (X GiB).` Roots that could not be read get a `not accessible (see ERROR above)`
  line, and with nothing else to compress the hint becomes `Fix access first`; roots refused by
  the guard get their own `refused by the volume/protection guard (see WARN above)` line. Without
  progress (the default when stderr is not a terminal, or `--no-progress`) and without `-v` those
  lines go only to the log, and the notes name the log file instead of `above`. It never spawns
  ditto and never writes staging dirs, the cache, `state.tsv` or `quarantine.txt`; it may create
  the config, the log dir and the log, and like every run it empties `<log_dir>/launchd.log` once
  that file exceeds 1 MiB.
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
- `clean` lists the leftover `.acz-staging-<pid>-<n>` and `.acz-rescue-<pid>-<n>` directories the
  walk of the config's patterns finds (same guard and excludes as `compress`): per directory its
  kind and pid, when it was created, last modified and last written (how long ago), the mtime range
  of its files (copies keep the original's mtime), file count, logical and on-disk size, and its
  regular files classified as ditto partial files (`.BC.T_*`), files with a same-named,
  byte-identical live file below the directory's parent (`has live copy`), and files with no copy
  found, which are listed by path; symlinks and special files are listed on a line of their own,
  directories are not counted. The classification is informational and compares at most 4 GiB of
  leftover data per run (8 compares per file); past that a directory is marked `INCOMPLETE`. The
  preview changes nothing (it takes no lock and writes only the log, and the config when missing)
  and ends with the exact command:
  `Nothing changed. Run 'auto-compress-apfs clean --apply' to delete these N dirs ...`.
  `clean --apply` takes the run lock before the walk (exits 1 while a run holds it) and deletes
  every listed directory that is not blocked, files without a copy included. A directory is
  blocked (listed with the reason, never deleted) when anything in it carries `uchg`, `uappnd`,
  `schg`, `sappnd`, `restricted` or is dataless, lies on another filesystem than its parent or
  cannot be read, and when it or anything in it changed less than 6 hours ago, staging and rescue
  dirs alike (it may belong to a `compress` run of another `HOME`, whose lock `clean` does not
  see, and may hold the only copy of a file). Right before each delete the name must still match
  the exact pattern and the path must still be the same directory (not a symlink, same device and
  inode) on its parent's filesystem; a directory swapped in between that check and the delete
  would be deleted instead (a symlink at the leftover's own path is never followed). Each
  delete is an INFO line `clean: removed leftover dir ...`; the run ends with
  `clean_summary found= removed= failed= blocked= freed_bytes= dry_run=`. Every directory left
  in place is printed to stderr with its reason. Exit code of `clean --apply`: 1 when a directory
  was blocked or could not be (fully) removed (stderr says when it is only because of blocked
  ones), else 0.

- `trash` and `purge` act on the files and directories matched by the `[trash]` and `[purge]`
  patterns of the config (same glob and exclude syntax as for compress; an exclude wins, also
  over a directory matched above it: such a directory is refused; excludes match the walked
  spelling, case-sensitively, and below a matched item also its on-disk spelling). A matched directory is not
  entered: it goes as a whole, whatever its age. Only items strictly below the home directory
  or an `allow_volumes` mount are acted on. Without
  `--apply` they list each item (size, file count, the pattern that matched) and the refused ones
  with the reason, and change nothing. With `--apply`, under the run lock, `trash` moves each item
  to `~/.Trash` (a name already there is never replaced: the moved item gets a time suffix) and
  `purge` deletes it for good. Right before acting the path must still be the same file or
  directory (device, inode, type, symlinks resolved) and carry no protected flag; the Trash move
  never follows a symlink. Each item attempted is one line in `<log_dir>/removed.tsv` (time,
  action, path, allocated bytes, pattern, Trash path or `-`, status `done`, `failed: ...` or
  `partial: N of M entries left: ...`); nothing is done when the journal cannot be opened.
  Refused, never acted on: a path with `.` or `..`, a mount point or an item on another
  filesystem, `trash` items on another volume than `~/.Trash` (or without one; so items on an
  `allow_volumes` volume can only be purged), an item holding a `.acz-staging-*` or
  `.acz-rescue-*` entry (see `clean`), anything the compress walk would not enter, a protected
  folder itself (`~/Downloads`, `~/Library`, ...), the home directory and any ancestor of a
  protected location, the config file and the tool's files next to it (`run.sh`, `quarantine.txt`, the ditto
  probe), anything inside or holding the log, state or cache directory of this tool or the Trash (and, for `trash`, anything already in the Trash), any item containing a flagged
  (`uchg`, `uappnd`, `schg`, `sappnd`, `restricted`) or dataless entry or an entry on another
  filesystem, and symlinks. Exit 1 when an item failed.
- `tm-exclude` excludes the directories matched by the `[tm_exclude]` patterns of the config
  from Time Machine backups: it creates the sticky exclusion attribute
  `com.apple.metadata:com_apple_backup_excludeItem` with the value `tmutil addexclusion` writes
  (`tmutil` itself is never executed). Matched files are ignored; a directory matched inside
  another matched one is covered by it. An existing exclusion (any value) is never removed or
  changed and counts as already excluded. Without `--apply` it lists each directory that would
  be excluded and the refused ones with the reason, and changes nothing. With `--apply`, under
  the run lock, each directory is reopened without following a symlink, must still be the same
  directory (device, inode) without a protected flag, and gets the attribute, read back to
  check. Refused as for `trash`/`purge` (location, own files, mount points, flagged or dataless
  directories, symlinks), plus directories not owned by the user running it. Each one excluded
  is an INFO line `excluded from Time Machine <path> (rule ...)`; the run ends with
  `tm_exclude_summary matched= already= excluded= blocked= errors= dry_run=`. A directory gone,
  replaced or not accessible since the walk is blocked, not an error. Exit 1 when a directory
  had an error. Exclusions are never removed by the tool; undo one with
  `tmutil removeexclusion <path>` or
  `xattr -d com.apple.metadata:com_apple_backup_excludeItem <path>` (the excluded paths are the
  INFO lines above in the log).
- `dedup` replaces identical regular files below the `[dedup]` patterns by APFS clones of one
  of them: the files stay separate (a later write to one does not change the other), their data
  is stored once. The walk, its hard protections and excludes are those of `compress`. Files
  below `min_size`, hardlinked, flagged, dataless, owned by another user or modified less than
  `min_age_secs` ago are never touched. Candidates are grouped by size, files that already
  share a clone id count once, then BLAKE3 hashes of the first and last 64 KiB and of all bytes
  regroup them. Per group the reference is a file that is already compressed, else the oldest,
  else the first path; it is only read. Another file of the group is replaced only when its
  owner, group, mode, BSD flags (except compressed) and extended attributes (except
  `com.apple.lastuseddate#PS`; the compressed data of a compressed file is not listed as an
  xattr, the resource fork of an uncompressed one is compared) equal the reference's and
  neither has an ACL; otherwise it is listed as `skipped: metadata differs (<what>)`.
  Setuid/setgid files are skipped. A replaced file's `com.apple.provenance` is the one the clone
  got, not its old value. Without `--apply` it lists each group (`keep`, `clone`,
  `skip` lines) and `dedup: groups= files= would_free=` (allocated blocks of the targets,
  approximately), and changes nothing. With `--apply`, under the run lock, the reference is
  cloned (`fclonefileat`) into a `.acz-staging-<pid>-<n>` dir next to the target, the clone
  gets the target's times, its metadata and every byte are compared with the target's, and it
  replaces the target by `RENAME_SWAP` only while the target is unchanged (device, inode, size,
  mtime, ctime); after the swap the file at the path is compared with the old one again, and a
  difference swaps it back (a file written to right after the swap is left as written). The
  staging dir carries the keep marker from the swap until the outcome is checked; when a swap
  back fails it is moved to `.acz-rescue-*` with the original inside. Each replacement is an INFO line `deduped <path> clone_of=<ref>
  bytes=<size>`; the run ends with `dedup: deduped= freed≈ skipped= failed=` and the log line
  `dedup_summary ... dry_run=`. Time Machine local snapshots may keep the freed blocks until
  they expire. Exit 1 when a replacement failed.
- `run` is what the launchd agent starts: `trash`, `purge`, `compress`, `dedup`, `tm-exclude` in
  that order, each as the `run` key of its config section says: `false` skips it, `true` runs
  it with the same `--apply` and `--config`, `"dry-run-only"` runs it as a preview even under
  `run --apply` (logged as `run step <name> mode=dry-run-only`). Compress is on by default, the
  others off; a section without patterns is skipped. Without `--apply` every step is a preview.
  Exit 1 when a step failed.
- `install-launchd` sets up the daily run for the current user (no `sudo`):
  - writes `run.sh` next to the config (mode 0755) with the absolute path of the binary it was
    started as (a symlink stays a symlink, so a rebuilt binary is picked up) and, for a
    non-default config, its `--config`;
  - writes `~/Library/LaunchAgents/local.auto-compress-apfs.plist`, which starts `/bin/sh run.sh`
    at load (login, install) and daily at 09:00 local time, with stdout and stderr appended to
    `<log_dir>/launchd.log` and `Nice` 10;
  - reloads the agent with `/bin/launchctl bootout` + `bootstrap gui/<uid>`, replacing an agent
    with the same label.

  `run.sh` starts `run --apply` at most once per local day and never before 09:00: it records
  the day in `last-run-day` in the state dir. A run missed during sleep is started by launchd on
  wake, one missed while the Mac was off at the next login, in both cases once (a wake or login
  before 09:00 waits for that day's 09:00 run). When the binary is gone (or not executable) it
  only appends a line to `launchd.log` and exits 0. Otherwise it `exec`s the binary, whose errors
  land in `launchd.log` through stderr. With `exec` the job's process is the binary itself, so a
  Full Disk Access or Downloads grant for the binary should apply to the scheduled run as it did
  to a job starting the binary directly (not verified: if the log shows `Operation not permitted`
  in a protected folder, grant access to the binary again; if that does not help, the run is
  attributed to `/bin/sh`, and running the binary directly from a launchd job is the way out).
  Installing after 09:00 on a day
  without a recorded run (also the first day after a scheduler that did not record it) starts
  one right away. `uninstall-launchd` boots the agent out and removes the plist and `run.sh`
  (anything already gone is fine; it does not read the config); pass the same `--config` as to
  `install-launchd`. Both commands refuse while a run holds the lock, since unloading the agent
  would stop that run.
- `check-launchd` changes nothing and creates nothing. It reads the `run.sh` the installed plist
  starts (without a plist, the one next to the config), takes the binary and config named there,
  and compares `run.sh` and the plist byte for byte with what this version would write for them;
  it also checks that the binary is executable and that launchd has the agent loaded from that
  plist. It prints one line: `OK: ...` (exit 0), or `INSTALL: ...` (nothing installed) or
  `REINSTALL: <reason>` (a file missing or different from what `install-launchd` writes now, the
  config run.sh names gone, the binary gone, the agent not loaded), both exit 1 and both with the
  command to run, which keeps the config the installed agent uses. Exit 2 (message on stderr,
  nothing on stdout) when the check itself fails, e.g. on an invalid config. A new binary at the
  same path needs no reinstall: `run.sh` starts whatever is there.

Exit code 0 when the run finished (per-file errors are logged, not fatal) and when another run holds
the lock (then `a run is in progress, exiting` goes to stderr and the log); 1 on a fatal error
(bad config, log cannot be opened, unreadable `quarantine.txt`, or with `--apply` an unreadable
`state.tsv` or a ditto that does not compress, ...).

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
- `[[backend_by_size]]`: which backend compresses which sizes (`backend`, `min_size`, optional
  `max_size`, both inclusive). Default: ditto for 16385..536870912 bytes, applesauce from
  536870913 up. `ditto --hfsCompression` leaves files of 16384 bytes or less plain (measured on
  macOS), so files below the lowest `min_size` are not even tried; a value you set is used as
  written. Ranges must not overlap and a ditto range needs a `max_size` of at most 536870912:
  otherwise every command that reads the config refuses to start, naming both ranges.
  `--backend NAME` gives every file from the lowest `min_size` up (up to 512 MiB for ditto) to
  one backend for a run. Old configs: a top-level `min_size` alone sets ditto's lower bound of
  the default routing; a top-level `backend` means that backend alone for
  `min_size`..536870912 (one INFO line per run says that other sizes are skipped). Neither may
  be combined with `backend_by_size`; a `min_size` above 536870912 is refused.
- `[trash]`, `[purge]`, `[tm_exclude]`: `patterns`, `exclude` (empty by default) and `run`
  (`false` by default) for those commands. The old `scheduled = true|false` is still read as
  `run = true|false`; a section with both is refused. Any other `run` value than `true`,
  `false` or `"dry-run-only"` is refused.
- `[compress]`: only `run` (`true` by default), for the compress step of `run`.
- `[dedup]`: `patterns` (empty by default), `exclude` (default `[".git", "target",
  "node_modules"]`), `min_size` (bytes, default 1048576, at least 1) and `run` (`false` by
  default). A `[tm_exclude]` exclude may not lie inside a directory a pattern
  names (`.../node_modules/keep` with `**/node_modules`), since an exclusion covers everything
  inside a matched directory. Install a binary
  that knows `[tm_exclude]` before adding it to an existing config: older binaries reject the
  unknown section.

**Full Disk Access.** `~/Downloads` is a TCC-protected folder. A run that walks it needs Full Disk
Access (or Downloads access) for the binary (System Settings > Privacy & Security); without it the
run logs one ERROR and goes on with the other roots. From a terminal the ERROR names the terminal
app (`TERM_PROGRAM`) and the folder (Desktop/Documents/Downloads) to grant, since macOS checks
the app the run started from; under launchd it names the binary's real path. Hypothesis: the
ad-hoc signature changes with every rebuild, so macOS may ask again after a rebuild.

## Logs and state

- `~/Library/Logs/auto-compress-apfs/auto-compress-apfs.log`: one line per event,
  `YYYY-MM-DDTHH:MM:SS.mmm+HH:MM LEVEL message`. INFO `compressed <path> <size> -> <allocated>` per
  file, ERROR with path and reason per failure, a start line and a `summary` line per run
  (`found`, `already_compressed`, `to_compress`, `compressed`, `incompressible`, `too_small`,
  `too_large`, `too_new`, `skipped`, `errors`, `bytes_before`, `bytes_after`, `ditto_calls`,
  `stale_staging`, `quarantined`, `requarantined_new`, `refused_roots`, `dataless`, `excluded`,
  `flagged`, `skipped_low_space`, `dry_run`, `elapsed`).
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
- **New inode.** The compressed file is a new inode: with ditto the birthtime changes (measured:
  the copy gets about the original's mtime; the applesauce backend keeps it), a process that holds the
  old file open keeps reading the old data until it reopens it; hardlinked files are skipped.
  Extended attributes, ACLs, mode, owner, group, atime and mtime are preserved.
- Files modified within `min_age_secs` (default 5 min) are skipped to avoid racing writers.
- Recompiling a package or `pip install` rewrites files uncompressed; the next run picks them
  up again (until the quarantine stops it, see above).
- **applesauce backend.** Used by default for files over 512 MiB only. The compressed data is written by third-party code
  (`applesauce-core` 0.4.14 with the bundled C `lzfse` 2.0.0), not by Apple's compressor; the
  byte compare through the kernel before every swap is the safety net. Its output differs from
  ditto's (LZFSE instead of LZVN), and files ditto leaves plain (16 KiB and less) may be
  compressed when an applesauce range covers them. A file whose compressed resource fork would
  exceed 4 GiB is cached as incompressible.
- **Verify cost.** With `verify = true` every file is read twice (original and copy), so a 1.5 GiB
  dump costs 3 GiB of reads; `"extended"` adds the in-process decode of the copy and, after the
  swap, one more read of the original and of the copy (decompressed from disk again).
- **Side effects of the swap** (new inode): Time Machine backs the swapped files up once more and
  Spotlight reindexes them. Hypothesis, not verified: the `com.apple.macl` attribute may not
  survive the copy, so per-file app grants on files in `~/Downloads` may be lost.
- **Rescue dirs.** `.acz-rescue-<pid>-<n>/` directories hold files the tool did not dare to
  delete. `compress` never walks, compresses or removes them, so they stay until a human removes
  them: read the ERROR/WARN lines that name them (or run `clean`, which lists them with the files
  that have no copy elsewhere), restore what you need, then delete the dir by hand or with
  `clean --apply`. One can hold up to a whole batch of copies (up to 2 GiB). They appear when
  - a swap could not be reversed after a detected race (practically never); the staging dir is
    first marked with an `.acz-keep` directory and then renamed; if that rename fails it keeps its
    `.acz-staging-*` name and stale cleanup leaves it alone (WARN `kept rescue staging dir`);
  - a crashed run left a staging dir holding anything but compressed copies (for example the
    original of a file swapped right before the crash): the next run renames it instead of
    deleting it.
- Leftover `.acz-staging-<pid>-<n>` directories holding only compressed copies (a crash mid-run)
  are removed by the next run.
- `clean` only finds leftovers in directories the walk of the current patterns reads. Not listed
  (remove those by hand): a leftover in a directory no longer walked (patterns changed, excluded
  since), and one next to a pattern without glob characters that names a single file (that file
  is a walk root, its directory is never listed).
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

Licensed under the [GNU General Public License v3.0 or later](LICENSE) (GPL-3.0-or-later).
Versions up to 0.3.0 were released under the Apache License, Version 2.0.
