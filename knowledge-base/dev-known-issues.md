# Known issues and platform facts

Small gotchas that shaped the design or cost time while running the tool. One block per issue.

## ditto never compresses files of 16 KiB or less

**Symptom**: after a first run most `.venv` files (small `.py`) stay uncompressed and land in the
incompressible cache; the run spent most of its time copying them through ditto.
**Cause**: `ditto --hfsCompression` leaves files of 16384 bytes or less uncompressed, whatever the
content (measured 2026-10-09: 16383 and 16384 bytes plain, 16385 bytes and up compressed; real 9-16 KB
`.py` files plain even when copied one by one). There is no ditto option to change it.
**Action**: default `min_size = 16385`, so such files are never sent to ditto. Compressing them would
need a native decmpfs writer or a non-Apple tool, both out of scope.

## Every process spawn can cost hundreds of milliseconds

**Symptom**: one ditto call per file makes a run of 100k+ files take hours.
**Cause**: on a machine with an endpoint security agent each exec is scanned; measured 2026-10-08
~258 ms even for `/usr/bin/true`, while one `ditto` call with 500 sources took 0.88 s.
**Action**: ditto always runs in batches (many sources into one staging dir). Keep it that way; never
add a per-file subprocess on the hot path.

## Batches must not mix names that differ only by case

**Symptom**: one file got another file's bytes in a staging dir (found in review, reproduced).
**Cause**: the data volume is case- and normalization-insensitive; `ditto -- a/README b/readme st/`
exits 0 and leaves one entry `st/README` with b's content.
**Action**: batches key basenames by their lowercase form, and a staged inode shared by two files
of a batch is never committed (both are retried alone). Keep both defenses.

## Downloads (TCC): who needs the permission

**Symptom**: `no access to ~/Downloads ... Operation not permitted` although Full Disk Access looks
granted.
**Cause**: macOS checks the responsible process. Run from a terminal, that is the terminal app, not
this binary; under launchd it is the binary itself. A "Full Disk Access" row under the app in
Privacy & Security > Files and Folders was seen while `ls ~/Downloads` in that app still failed.
**Action**: check with `/bin/ls ~/Downloads` in the same terminal first. If it fails, toggle the
terminal in Full Disk Access (or `tccutil reset SystemPolicyDownloadsFolder com.apple.Terminal`
and allow the prompt), then quit and reopen the terminal. The scheduled run needs Full Disk Access
for the release binary separately. Hypothesis: a new ad-hoc signature after a rebuild may need the
grant again.
**Diag**: terminal runs and the scheduled run write to the same `auto-compress-apfs.log`, so the
latest `no access` lines may come from a terminal run only. To judge the scheduled run, read the
lines stamped at its start time (`grep 'T09:0'`): `compressed .../Downloads/...` there means the
binary has access.

## Clones keep compression; uv venvs are clones

**Fact**: `cp -c` (clonefile) of a compressed file yields a compressed file (same decmpfs type), and
writing to the clone leaves the original intact (checked 2026-10-08). uv's default `link-mode` on
macOS is `clone` (`uv help sync`, uv 0.12.17), so a venv shares blocks with `~/.cache/uv`.
**Action**: compress `~/.cache/uv` too; venvs created afterwards clone already-compressed files.
Compressing only the venv un-shares its blocks from the cache.

## Paths that cannot be written fail every run

**Symptom**: daily `ERROR(perm)` lines for the same files.
**Cause**: some caches keep files and dirs read-only (Go's module cache under `~/go/pkg/mod`,
hypothesis: some unpacked wheels); the swap needs write access to the parent dir and the tool never
chmods.
**Action**: expected; drop such patterns from the config if the lines are unwanted.

## ditto on macOS 27 clones instead of compressing

**Symptom**: `ditto --hfsCompression` copies a file but the copy is not compressed.
**Cause**: since macOS 27 ditto clones files by default (`cloneFiles`), and a clone keeps the
source's (uncompressed) storage.
**Action**: `compress --apply` first runs a probe (`ditto-probe.txt` next to the config, compressed
into `ditto-probe.out/`, recreated every run) with the args it will use; when the probe copy is not
compressed, the run stops with a warning instead of swapping uncompressed copies. The args that
passed are cached in `ditto-args.txt`.

## Endpoint security may flag `install-launchd`

**Symptom**: right after `install-launchd`, an endpoint security agent (seen with SentinelOne)
kills the process tree that ran it or quarantines the binary.
**Cause**: behavioural rule "unsigned process adds a property list to `~/Library/LaunchAgents`"
(persistence). The release binary is ad-hoc signed.
**Action**: run `install-launchd` from a terminal, not from a build tool or agent; or add an
exclusion for the folder holding the binary. Tests never write a plist below a
`Library/LaunchAgents` path (`ACZ_LAUNCH_AGENTS_DIR`).

## Pipeline tests can run a stale debug binary

**Symptom**: after `cargo package` / `cargo publish --dry-run`, `tests/pipeline.rs` keeps failing
on behaviour the source no longer has; the binary under `target/debug` predates the last edit.
**Cause**: hypothesis: the packaging build leaves a fingerprint that makes cargo treat the old
binary as fresh. The stale binary itself was observed (2026-10-20).
**Action**: `cargo clean -p auto-compress-apfs`, then `make test` again.

## Time Machine sticky exclusion without tmutil

**Symptom**: `tm-exclude` must exclude directories from Time Machine without executing anything
but ditto; how the attribute behaves was unknown.
**Facts** (verified 2026-11-02 on macOS 27): the sticky exclusion is the xattr
`com.apple.metadata:com_apple_backup_excludeItem`, a 61-byte binary plist holding the string
`com.apple.backupd` (what `tmutil addexclusion` writes). `fsetxattr` on a directory fd opened
`O_RDONLY | O_NOFOLLOW | O_DIRECTORY` works; `XATTR_CREATE` makes a second write fail with
`EEXIST` (so an existing exclusion is never replaced); `XATTR_NOFOLLOW` on an fd call fails with
`EINVAL`. `tmutil isexcluded` reports a directory with the written value as `[Excluded]`.
**Action**: undo by hand with `tmutil removeexclusion <path>`; the tool never removes one.

## Not every gitignored directory is rebuildable

**Symptom**: when choosing `[tm_exclude]` patterns, "ignored by git" looked like "safe to drop
from backups".
**Cause**: project scratch dirs such as `.var` hold build output next to local data (database
backups, client reports, exports), and a name like `target` also occurs as a source directory
(a crate's `src/target`, e2e fixtures, ESP-IDF examples).
**Action**: check a `target` dir for `CACHEDIR.TAG` or `.rustc_info.json`, inspect `.var`-like
dirs before matching them, and run `tm-exclude` without `--apply` first.

## Reading a directory can fail with EINTR

**Symptom**: one daily run logged `WARN cannot read dir ~/Downloads: Interrupted system call (os
error 4)` (2026-10-31) and skipped that whole subtree for the day.
**Cause**: hypothesis: a signal interrupted the directory read; the walk does not retry on EINTR.
**Real fix (TODO)**: retry the read on `ErrorKind::Interrupted`.

## decmpfs facts behind `verify = "extended"`

**Symptom**: none; these are measured facts (macOS 27.2, 2026-11-02) that the extended verify
relies on, worth re-checking on a new macOS.
**Cause / facts**:
- `ditto --hfsCompression` writes only decmpfs type 8 (LZVN in the resource fork), never inline;
  an incompressible block is stored raw with a `0x06` prefix. The applesauce backend writes type 11
  (inline) or 12. applesauce-core with the `lzfse` + `lzvn` features decodes all of them
  byte-identically to a kernel read.
- `F_NOCACHE` does not help to read a compressed file cold: the pages land in the cache anyway.
  Right after ditto no page of the copy is cached, so the plain compare already decodes it through
  the kernel. To force a fresh decode later (post-swap check), `mmap` + `msync(MS_SYNC|MS_INVALIDATE)`
  on a read-only mapping evicts the pages; the compressed flag stays.
- `setxattr` of `com.apple.ResourceFork` on a `UF_COMPRESSED` file makes the kernel decompress
  the file (flag cleared, data kept): stored data of a compressed file cannot be tampered with
  from userspace, so tamper tests use files without the flag.
- `F_FULLFSYNC` (std `sync_all` on macOS) costs ~4 ms per file, a plain `fsync` ~0.13 ms.
**Action**: a new decmpfs type from a future ditto fails every file in extended mode; the startup
probe reports it once ("ditto copies cannot be verified independently"); `verify = true` is the
fallback until a decoder exists.
