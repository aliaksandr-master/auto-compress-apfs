//! Batched `ditto --hfsCompression` into staging dirs, then a checked, atomic swap per file.
//!
//! The original stays the source of truth until a verified compressed copy is swapped in with
//! `renamex_np(RENAME_SWAP)`; every failure path leaves the original untouched. Per-file outcomes
//! are reported as typed [`FileEvent`]s (log line plus terminal line). No ditto call starts
//! without `min_free_bytes` left after its data, and a parent directory found not writable is
//! remembered for the rest of the run.

use crate::logger::{Level, Logger};
use crate::progress::{FileEvent, human};
use crate::sys::{self, O_NOFOLLOW, UF_COMPRESSED};
use crate::walk::{Candidate, FileKey, RESCUE_PREFIX, STAGING_PREFIX, is_stale_staging_name};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, DirBuilder, File, FileTimes, Metadata};
use std::io::{self, Read};
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DITTO: &str = "/usr/bin/ditto";
/// Upper bound of source path bytes passed to one ditto call (ARG_MAX is 1 MiB incl. env).
const MAX_ARG_BYTES: usize = 256 * 1024;
const STDERR_KEEP: usize = 4096;
const DITTO_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const VERIFY_BUF: usize = 1 << 20;
/// Upper bound of file data copied by one ditto call (bounds staging space and timeout risk).
const MAX_BATCH_DATA: u64 = 2 << 30;
/// Consecutive failed single-file retries after which the rest of a batch is given up.
const MAX_RETRY_FAILURES: u32 = 3;
/// Marker directory inside a staging dir holding a rescued file; stale cleanup keeps such a dir.
const KEEP_MARKER: &str = ".acz-keep";

/// A file replaced by its compressed copy: the identity and sizes for `state.tsv`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    pub path: PathBuf,
    pub dev: u64,
    /// Inode of the compressed copy now at `path`.
    pub ino: u64,
    /// Logical size.
    pub size: u64,
    /// Allocated bytes after compression.
    pub after: u64,
}

/// Counters and cache/state additions of the compress phase.
#[derive(Debug, Default)]
pub struct Outcome {
    pub compressed: u64,
    pub incompressible: u64,
    pub skipped: u64,
    /// Files not attempted because the volume would drop below `min_free_bytes`.
    pub skipped_low_space: u64,
    pub errors: u64,
    /// Allocated bytes of the replaced originals.
    pub bytes_before: u64,
    /// Allocated bytes of their compressed replacements.
    pub bytes_after: u64,
    pub ditto_calls: u64,
    pub new_incompressible: Vec<FileKey>,
    pub committed: Vec<Committed>,
}

impl Outcome {
    fn merge(&mut self, o: Outcome) {
        self.compressed += o.compressed;
        self.incompressible += o.incompressible;
        self.skipped += o.skipped;
        self.skipped_low_space += o.skipped_low_space;
        self.errors += o.errors;
        self.bytes_before += o.bytes_before;
        self.bytes_after += o.bytes_after;
        self.ditto_calls += o.ditto_calls;
        self.new_incompressible.extend(o.new_incompressible);
        self.committed.extend(o.committed);
    }

    fn record(&mut self, c: &Commit) {
        match c {
            Commit::Compressed { before, after, .. } => {
                self.compressed += 1;
                self.bytes_before += before;
                self.bytes_after += after;
            }
            Commit::Incompressible(key) => {
                self.incompressible += 1;
                self.new_incompressible.push(*key);
            }
            Commit::Skipped => self.skipped += 1,
            Commit::LowSpace => self.skipped_low_space += 1,
            Commit::Failed | Commit::Rescued(_) => self.errors += 1,
        }
    }
}

/// Result of committing one staged copy.
#[derive(Debug, PartialEq, Eq)]
enum Commit {
    Compressed {
        before: u64,
        after: u64,
        /// Inode of the compressed copy now at the original path.
        ino: u64,
    },
    Incompressible(FileKey),
    /// The original changed or vanished during the run.
    Skipped,
    /// Not attempted: not enough free space for the copy.
    LowSpace,
    Failed,
    /// Swap-back failed; the staging dir was moved to this rescue dir (or left in place).
    Rescued(PathBuf),
}

/// Per-worker verify buffers, allocated on first use and reused for every file.
#[derive(Default)]
struct Buffers {
    a: Vec<u8>,
    b: Vec<u8>,
}

impl Buffers {
    fn get(&mut self) -> (&mut [u8], &mut [u8]) {
        if self.a.is_empty() {
            self.a = vec![0; VERIFY_BUF];
            self.b = vec![0; VERIFY_BUF];
        }
        (&mut self.a, &mut self.b)
    }
}

#[derive(Debug)]
enum DittoStatus {
    Exited(ExitStatus),
    TimedOut,
    SpawnFailed(io::Error),
    /// `try_wait` failed; the child was killed and reaped.
    WaitFailed(io::Error),
}

struct DittoRun {
    status: DittoStatus,
    /// First [`STDERR_KEEP`] bytes of stderr, on one line.
    stderr: String,
}

impl DittoRun {
    fn success(&self) -> bool {
        matches!(&self.status, DittoStatus::Exited(s) if s.success())
    }

    fn describe(&self) -> String {
        let status = match &self.status {
            DittoStatus::Exited(s) => s.to_string(),
            DittoStatus::TimedOut => format!("timed out after {} s", DITTO_TIMEOUT.as_secs()),
            DittoStatus::SpawnFailed(e) => format!("cannot spawn {DITTO}: {e}"),
            DittoStatus::WaitFailed(e) => format!("cannot wait for {DITTO}: {e}"),
        };
        if self.stderr.is_empty() {
            status
        } else {
            format!("{status}: {}", self.stderr)
        }
    }
}

fn drain_stderr(mut pipe: ChildStderr) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let room = STDERR_KEEP - kept.len();
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    kept
}

/// Waits for `child` until `timeout`, killing (and reaping) it when the deadline passes.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> DittoStatus {
    let deadline = Instant::now() + timeout;
    let mut pause = Duration::from_millis(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return DittoStatus::Exited(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return DittoStatus::TimedOut;
            }
            Ok(None) => {
                thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(100));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return DittoStatus::WaitFailed(e);
            }
        }
    }
}

/// `ditto --hfsCompression -- <sources...> <staging>/`; stderr drained to EOF on a helper thread.
fn run_ditto(sources: &[&Path], staging: &Path, timeout: Duration) -> DittoRun {
    let mut dest = staging.as_os_str().to_owned();
    dest.push("/");
    let spawned = Command::new(DITTO)
        .arg("--hfsCompression")
        .arg("--")
        .args(sources)
        .arg(&dest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            return DittoRun {
                status: DittoStatus::SpawnFailed(e),
                stderr: String::new(),
            };
        }
    };
    // If the helper thread cannot be spawned the pipe is dropped: ditto then fails on its
    // first stderr write instead of blocking.
    let reader = child.stderr.take().and_then(|pipe| {
        thread::Builder::new()
            .name("ditto-stderr".into())
            .spawn(move || drain_stderr(pipe))
            .ok()
    });
    let status = wait_with_timeout(&mut child, timeout);
    let stderr = reader.and_then(|r| r.join().ok()).unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr).trim().replace('\n', " | ");
    DittoRun { status, stderr }
}

/// Splits one match root's files into batches: <= `max_files` files, <= `max_bytes` of source
/// path bytes, <= [`MAX_BATCH_DATA`] of file data, and basenames unique per batch ignoring case
/// (ditto flattens sources into one directory on a case-insensitive volume).
/// The k-th file with a given basename goes to layer k, so the batch count stays close to
/// the maximum basename multiplicity.
fn split_batches(files: Vec<Candidate>, max_files: usize, max_bytes: usize) -> Vec<Vec<Candidate>> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut layers: Vec<Vec<Candidate>> = Vec::new();
    for f in files {
        let name = f.path.file_name().unwrap_or_default();
        let k = seen
            .entry(name.to_string_lossy().to_lowercase())
            .or_default();
        if layers.len() <= *k {
            layers.push(Vec::new());
        }
        layers[*k].push(f);
        *k += 1;
    }
    let mut batches = Vec::new();
    for layer in layers {
        let mut cur: Vec<Candidate> = Vec::new();
        let (mut bytes, mut data) = (0, 0);
        for f in layer {
            let n = f.path.as_os_str().len() + 1;
            let full =
                cur.len() >= max_files || bytes + n > max_bytes || data + f.size() > MAX_BATCH_DATA;
            if !cur.is_empty() && full {
                batches.push(std::mem::take(&mut cur));
                (bytes, data) = (0, 0);
            }
            bytes += n;
            data += f.size();
            cur.push(f);
        }
        if !cur.is_empty() {
            batches.push(cur);
        }
    }
    batches
}

fn system_time(sec: i64, nsec: i64) -> SystemTime {
    let nanos = Duration::from_nanos(nsec as u64);
    if sec >= 0 {
        UNIX_EPOCH + Duration::from_secs(sec as u64) + nanos
    } else {
        UNIX_EPOCH - Duration::from_secs(sec.unsigned_abs()) + nanos
    }
}

/// Reads until `buf` is full or EOF; returns the byte count.
fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

enum Verify {
    Same,
    Changed,
    Mismatch,
}

/// True when `dir` contains the rescue marker directory.
fn has_keep_marker(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join(KEEP_MARKER)).is_ok_and(|m| m.is_dir())
}

/// True when every top-level entry of a staging dir is a compressed regular file (a copy made
/// by ditto). Anything else may be a user's original swapped out right before a crash.
fn only_staged_copies(dir: &Path) -> io::Result<bool> {
    for entry in fs::read_dir(dir)? {
        let m = entry?.metadata()?;
        if !m.is_file() || m.st_flags() & UF_COMPRESSED == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Cleans leftover staging dirs of crashed runs (only names matching the exact staging regex).
/// A dir with the [`KEEP_MARKER`] directory is kept; a dir holding anything but compressed
/// copies is renamed to `.acz-rescue-<pid>-<n>` instead of being deleted.
pub fn remove_stale(dirs: &[PathBuf], log: &Logger) {
    for dir in dirs {
        let name = dir.file_name().map(|n| n.as_bytes()).unwrap_or_default();
        if !is_stale_staging_name(name) {
            continue;
        }
        if has_keep_marker(dir) {
            log.warn(format_args!("kept rescue staging dir {}", dir.display()));
            continue;
        }
        match only_staged_copies(dir) {
            Ok(true) => {}
            Ok(false) => {
                let rescue = rescue_path(dir);
                match fs::rename(dir, &rescue) {
                    Ok(()) => log.warn(format_args!(
                        "stale staging dir {} holds uncompressed or non-regular entries; moved to {}",
                        dir.display(),
                        rescue.display()
                    )),
                    Err(e) => log.warn(format_args!(
                        "stale staging dir {} holds uncompressed or non-regular entries, kept (cannot move to {}: {e})",
                        dir.display(),
                        rescue.display()
                    )),
                }
                continue;
            }
            Err(e) => {
                log.warn(format_args!(
                    "cannot inspect stale staging dir {}, kept: {e}",
                    dir.display()
                ));
                continue;
            }
        }
        match fs::remove_dir_all(dir) {
            Ok(()) => log.warn(format_args!("removed stale staging dir {}", dir.display())),
            Err(e) => log.warn(format_args!(
                "cannot remove stale staging dir {}: {e}",
                dir.display()
            )),
        }
    }
}

/// `.../.acz-staging-<pid>-<n>` -> `.../.acz-rescue-<pid>-<n>`.
fn rescue_path(staging: &Path) -> PathBuf {
    let name = staging
        .file_name()
        .map(|n| {
            n.to_string_lossy()
                .replacen(STAGING_PREFIX, RESCUE_PREFIX, 1)
        })
        .unwrap_or_default();
    staging.with_file_name(name)
}

/// Indices of `files` whose expected staged entry is one inode shared with another file of the
/// batch (names that differ only by case or Unicode normalisation on this volume).
fn shared_staged(staging: &Path, files: &[&Candidate]) -> HashSet<usize> {
    let mut by_inode: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    for (i, c) in files.iter().enumerate() {
        if let Ok(m) = fs::symlink_metadata(staging.join(c.path.file_name().unwrap_or_default())) {
            by_inode.entry((m.dev(), m.ino())).or_default().push(i);
        }
    }
    by_inode
        .into_values()
        .filter(|v| v.len() > 1)
        .flatten()
        .collect()
}

pub struct Compressor<'a> {
    pub log: &'a Logger,
    pub verify: bool,
    pub batch_size: usize,
    pub threads: usize,
    /// Free bytes that must remain on the volume after a ditto call's data.
    pub min_free_bytes: u64,
    /// The low-space WARN was written (later ones are DEBUG).
    low_space_warned: AtomicBool,
    /// Directories where creating staging or swapping failed with EACCES/EPERM in this run.
    unwritable: Mutex<HashSet<PathBuf>>,
}

impl<'a> Compressor<'a> {
    pub fn new(
        log: &'a Logger,
        verify: bool,
        batch_size: usize,
        threads: usize,
        min_free_bytes: u64,
    ) -> Self {
        Self {
            log,
            verify,
            batch_size,
            threads,
            min_free_bytes,
            low_space_warned: AtomicBool::new(false),
            unwritable: Mutex::new(HashSet::new()),
        }
    }

    /// Compresses `files` (all classified as to-compress), batches running in parallel.
    pub fn run(&self, files: Vec<Candidate>) -> Outcome {
        let mut groups: BTreeMap<(Arc<Path>, u64), Vec<Candidate>> = BTreeMap::new();
        for c in files {
            groups
                .entry((c.match_root.clone(), c.key.dev))
                .or_default()
                .push(c);
        }
        let batches: Vec<Vec<Candidate>> = groups
            .into_values()
            .flat_map(|g| split_batches(g, self.batch_size, MAX_ARG_BYTES))
            .collect();
        if batches.is_empty() {
            return Outcome::default();
        }
        let workers = self.threads.clamp(1, batches.len());
        let queue = Mutex::new(batches.into_iter().enumerate());
        thread::scope(|s| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    s.spawn(|| {
                        let mut out = Outcome::default();
                        let mut bufs = Buffers::default();
                        loop {
                            let next = queue.lock().unwrap_or_else(|e| e.into_inner()).next();
                            let Some((no, batch)) = next else { break };
                            self.run_batch(no, &batch, &mut bufs, &mut out);
                        }
                        out
                    })
                })
                .collect();
            let mut total = Outcome::default();
            for h in handles {
                total.merge(h.join().expect("compress worker panicked"));
            }
            total
        })
    }

    /// Logs one ERROR for `c`; `kind` is ditto, verify, swap, perm, timeout, staging or io.
    fn fail(&self, c: &Candidate, kind: &str, reason: impl std::fmt::Display) -> Commit {
        self.log
            .file_event(Level::Error, &c.path, FileEvent::Error(kind), reason);
        Commit::Failed
    }

    /// Accounts the final result of one file and advances the progress line.
    fn finish(&self, out: &mut Outcome, c: &Candidate, r: &Commit) {
        out.record(r);
        if let Commit::Compressed { after, ino, .. } = r {
            out.committed.push(Committed {
                path: c.path.clone(),
                dev: c.key.dev,
                ino: *ino,
                size: c.size(),
                after: *after,
            });
        }
        self.log.advance(c.size());
    }

    fn parent(c: &Candidate) -> &Path {
        c.path.parent().unwrap_or(Path::new("/"))
    }

    fn unwritable(&self) -> std::sync::MutexGuard<'_, HashSet<PathBuf>> {
        self.unwritable.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn parent_unwritable(&self, c: &Candidate) -> bool {
        self.unwritable().contains(Self::parent(c))
    }

    fn remember_unwritable(&self, dir: &Path) {
        self.unwritable().insert(dir.to_path_buf());
    }

    fn fail_unwritable(&self, c: &Candidate) -> Commit {
        self.fail(c, "perm", "parent dir not writable, skipped for this run")
    }

    /// `Err(reason)` when `data` more bytes in `staging` would leave less than
    /// `min_free_bytes` free (or free space cannot be determined).
    fn space_ok(&self, staging: &Path, data: u64) -> Result<(), String> {
        match sys::statfs(staging) {
            Ok(fs) if fs.bavail_bytes >= data.saturating_add(self.min_free_bytes) => Ok(()),
            Ok(fs) => Err(format!(
                "low disk space: {} free, {} needed plus min_free_bytes {}",
                human(fs.bavail_bytes),
                human(data),
                human(self.min_free_bytes)
            )),
            Err(e) => Err(format!("cannot check free space: {e}")),
        }
    }

    /// One WARN per run (DEBUG afterwards) for `count` files skipped for lack of space, plus a
    /// per-file DEBUG line; the caller accounts each file as [`Commit::LowSpace`].
    fn low_space(&self, staging: &Path, files: &[&Candidate], why: &str) {
        let dir = staging.parent().unwrap_or(staging).display();
        let msg = format_args!("{dir}: {why}; {} file(s) skipped", files.len());
        if self.low_space_warned.swap(true, Ordering::Relaxed) {
            self.log.debug(msg);
        } else {
            self.log.warn(msg);
        }
        for c in files {
            self.log.file_event(
                Level::Debug,
                &c.path,
                FileEvent::Skip("lowspace"),
                "skipped: low disk space",
            );
        }
    }

    fn make_staging(dir: &Path, no: usize) -> io::Result<PathBuf> {
        let staging = dir.join(format!("{STAGING_PREFIX}{}-{no}", std::process::id()));
        DirBuilder::new().mode(0o700).create(&staging)?;
        Ok(staging)
    }

    /// Stages a batch (one match root, one device) in the match root; when that is impossible
    /// or the root is on another device, stages per parent directory instead.
    fn run_batch(&self, no: usize, batch: &[Candidate], bufs: &mut Buffers, out: &mut Outcome) {
        let root = &batch[0].match_root;
        let same_dev = fs::symlink_metadata(root).is_ok_and(|m| m.dev() == batch[0].key.dev);
        let staged_in_root = if same_dev {
            Self::make_staging(root, no)
        } else {
            Err(io::Error::other("match root is on another device"))
        };
        match staged_in_root {
            Ok(staging) => {
                let files: Vec<&Candidate> = batch.iter().collect();
                self.process(&staging, &files, bufs, out);
            }
            Err(e) => {
                self.log.debug(format_args!(
                    "cannot stage in {}: {e}; using per-directory staging",
                    root.display()
                ));
                let mut by_parent: BTreeMap<&Path, Vec<&Candidate>> = BTreeMap::new();
                for c in batch {
                    let parent = c.path.parent().unwrap_or(Path::new("/"));
                    by_parent.entry(parent).or_default().push(c);
                }
                for (parent, files) in by_parent {
                    if self.unwritable().contains(parent) {
                        for c in files {
                            let r = self.fail_unwritable(c);
                            self.finish(out, c, &r);
                        }
                        continue;
                    }
                    match Self::make_staging(parent, no) {
                        Ok(staging) => self.process(&staging, &files, bufs, out),
                        Err(e) => {
                            let perm = e.kind() == io::ErrorKind::PermissionDenied;
                            if perm {
                                self.remember_unwritable(parent);
                            }
                            for c in files {
                                let r = if perm {
                                    self.fail(
                                        c,
                                        "perm",
                                        format_args!(
                                            "parent dir not writable (cannot create staging dir: {e})"
                                        ),
                                    )
                                } else {
                                    self.fail(
                                        c,
                                        "staging",
                                        format_args!("cannot create staging dir: {e}"),
                                    )
                                };
                                self.finish(out, c, &r);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Runs ditto for `files` into `staging`, commits every staged copy, removes `staging`.
    fn process(&self, staging: &Path, files: &[&Candidate], bufs: &mut Buffers, out: &mut Outcome) {
        // Files in a directory already found not writable would fail the swap again.
        let (blocked, files): (Vec<&Candidate>, Vec<&Candidate>) =
            files.iter().partition(|c| self.parent_unwritable(c));
        for c in blocked {
            let r = self.fail_unwritable(c);
            self.finish(out, c, &r);
        }
        let files = files.as_slice();
        if files.is_empty() {
            self.remove_staging(staging);
            return;
        }
        let data: u64 = files.iter().map(|c| c.size()).sum();
        if let Err(why) = self.space_ok(staging, data) {
            self.low_space(staging, files, &why);
            for c in files {
                self.finish(out, c, &Commit::LowSpace);
            }
            self.remove_staging(staging);
            return;
        }
        let sources: Vec<&Path> = files.iter().map(|c| c.path.as_path()).collect();
        out.ditto_calls += 1;
        let run = run_ditto(&sources, staging, DITTO_TIMEOUT);
        let batch_ok = match &run.status {
            DittoStatus::Exited(_) => run.success(),
            DittoStatus::TimedOut | DittoStatus::SpawnFailed(_) | DittoStatus::WaitFailed(_) => {
                // Staged copies may be partial: commit none of them.
                let kind = if matches!(run.status, DittoStatus::TimedOut) {
                    "timeout"
                } else {
                    "ditto"
                };
                for c in files {
                    let r = self.fail(c, kind, format_args!("ditto failed: {}", run.describe()));
                    self.finish(out, c, &r);
                }
                self.remove_staging(staging);
                return;
            }
        };
        if !batch_ok {
            self.log.debug(format_args!(
                "ditto batch into {} failed ({}); committing staged copies, retrying the rest one by one",
                staging.display(),
                run.describe()
            ));
        }
        // Defense in depth against names that collide on this volume despite the batching.
        let shared = shared_staged(staging, files);
        let mut rescued: Option<PathBuf> = None;
        let mut retry_failures = 0;
        // Why the rest of the batch is not attempted, once given up.
        let mut gave_up: Option<&str> = None;
        for (i, c) in files.iter().enumerate() {
            if let Some(rescue) = &rescued {
                let r = self.fail(
                    c,
                    "swap",
                    format_args!("not committed: batch staging moved to {}", rescue.display()),
                );
                self.finish(out, c, &r);
                continue;
            }
            let staged = staging.join(c.path.file_name().unwrap_or_default());
            // A copy produced by a failed ditto call is always byte-compared and never taken
            // as proof of incompressibility.
            let mut from_failed_call = !batch_ok;
            let mut need_retry =
                shared.contains(&i) || (!batch_ok && fs::symlink_metadata(&staged).is_err());
            let r = loop {
                if need_retry {
                    if shared.contains(&i) {
                        // Never commit from the shared entry; free the name for a lone copy.
                        let _ = fs::remove_file(&staged);
                    }
                    if let Err(e) = fs::symlink_metadata(&c.path)
                        && e.kind() == io::ErrorKind::NotFound
                    {
                        self.log.file_event(
                            Level::Info,
                            &c.path,
                            FileEvent::Skip("changed"),
                            "vanished during run, skipped",
                        );
                        break Commit::Skipped;
                    }
                    if let Some(reason) = gave_up {
                        break self.fail(
                            c,
                            "ditto",
                            format_args!("not attempted: {reason} in {}", staging.display()),
                        );
                    }
                    if !fs::symlink_metadata(staging).is_ok_and(|m| m.is_dir()) {
                        gave_up = Some("staging dir vanished");
                        self.log.error(format_args!(
                            "staging dir {} vanished; remaining files of this batch not attempted",
                            staging.display()
                        ));
                        break Commit::Failed;
                    }
                    if let Err(why) = self.space_ok(staging, c.size()) {
                        self.low_space(staging, &[c], &why);
                        break Commit::LowSpace;
                    }
                    out.ditto_calls += 1;
                    let retry = run_ditto(&[&c.path], staging, DITTO_TIMEOUT);
                    if !retry.success() {
                        retry_failures += 1;
                        let kind = if matches!(retry.status, DittoStatus::TimedOut) {
                            "timeout"
                        } else {
                            "ditto"
                        };
                        let r =
                            self.fail(c, kind, format_args!("ditto failed: {}", retry.describe()));
                        if retry_failures >= MAX_RETRY_FAILURES {
                            gave_up = Some("ditto kept failing");
                            self.log.error(format_args!(
                                "ditto keeps failing in {}: {}; remaining files of this batch not attempted",
                                staging.display(),
                                retry.describe()
                            ));
                        }
                        break r;
                    }
                    retry_failures = 0;
                    need_retry = false;
                    from_failed_call = false;
                }
                if self.parent_unwritable(c) {
                    break Self::discard(&staged, self.fail_unwritable(c));
                }
                let r = self.commit(c, &staged, from_failed_call, bufs, &sys::rename_swap);
                if from_failed_call && matches!(r, Commit::Incompressible(_)) {
                    // Uncompressed copy from a failed call: retry alone before believing it.
                    need_retry = true;
                    continue;
                }
                break r;
            };
            self.finish(out, c, &r);
            if let Commit::Rescued(p) = r {
                rescued = Some(p);
            }
        }
        if rescued.is_none() {
            self.remove_staging(staging);
        }
    }

    fn remove_staging(&self, staging: &Path) {
        if let Err(e) = fs::remove_dir_all(staging) {
            self.log.warn(format_args!(
                "cannot remove staging dir {}: {e}",
                staging.display()
            ));
        }
    }

    /// Removes a staged copy that will not be committed; batch-end cleanup is the fallback.
    fn discard(staged: &Path, result: Commit) -> Commit {
        let _ = fs::remove_file(staged);
        result
    }

    fn changed(&self, c: &Candidate, staged: &Path) -> Commit {
        self.log.file_event(
            Level::Info,
            &c.path,
            FileEvent::Skip("changed"),
            "changed during run, skipped",
        );
        Self::discard(staged, Commit::Skipped)
    }

    /// Checks the staged copy against the original and swaps it in. The byte compare runs when
    /// `verify` is configured or `force_verify` is set (copy from a failed ditto call). `swap` is
    /// [`sys::rename_swap`] in production; tests inject failures through it.
    fn commit(
        &self,
        c: &Candidate,
        staged: &Path,
        force_verify: bool,
        bufs: &mut Buffers,
        swap: &dyn Fn(&Path, &Path) -> io::Result<()>,
    ) -> Commit {
        // a. The original is still the version the walk saw.
        match fs::symlink_metadata(&c.path) {
            Ok(m)
                if m.is_file() && FileKey::of(&m) == c.key && m.st_flags() & UF_COMPRESSED == 0 => {
            }
            Ok(_) => return self.changed(c, staged),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return self.changed(c, staged),
            Err(e) => {
                return Self::discard(staged, self.fail(c, "io", format_args!("cannot stat: {e}")));
            }
        }
        // R6: carry the original atime/mtime over before checking.
        let times = FileTimes::new()
            .set_accessed(system_time(c.atime_sec, c.atime_nsec))
            .set_modified(system_time(c.key.mtime_sec, c.key.mtime_nsec));
        let opened = File::options()
            .read(true)
            .custom_flags(O_NOFOLLOW)
            .open(staged);
        if let Err(e) = opened.and_then(|f| f.set_times(times)) {
            let r = self.fail(
                c,
                "io",
                format_args!("cannot set times on staged copy {}: {e}", staged.display()),
            );
            return Self::discard(staged, r);
        }
        // b. The staged copy is a compressed, metadata-identical regular file.
        let sm = match fs::symlink_metadata(staged) {
            Ok(m) if m.is_file() => m,
            Ok(_) => {
                let r = self.fail(c, "io", "staged copy is not a regular file");
                return Self::discard(staged, r);
            }
            Err(e) => {
                let r = self.fail(c, "io", format_args!("staged copy missing: {e}"));
                return Self::discard(staged, r);
            }
        };
        // Metadata first: only a complete copy may mark the original as incompressible.
        if let Some(diff) = metadata_diff(c, &sm) {
            let r = self.fail(
                c,
                "io",
                format_args!("staged copy differs ({diff}), discarded"),
            );
            return Self::discard(staged, r);
        }
        if sm.st_flags() & UF_COMPRESSED == 0 {
            self.log
                .debug(format_args!("{}: incompressible, cached", c.path.display()));
            return Self::discard(staged, Commit::Incompressible(c.key));
        }
        if sm.gid() != c.gid
            && let Err(e) = std::os::unix::fs::chown(staged, None, Some(c.gid))
        {
            let r = self.fail(
                c,
                "io",
                format_args!("cannot set group on staged copy: {e}"),
            );
            return Self::discard(staged, r);
        }
        // c. Byte compare against the very inode the walk saw.
        if self.verify || force_verify {
            match verify(c, staged, bufs) {
                Ok(Verify::Same) => {}
                Ok(Verify::Changed) => return self.changed(c, staged),
                Ok(Verify::Mismatch) => {
                    let r = self.fail(
                        c,
                        "verify",
                        "verify failed: compressed copy differs, discarded",
                    );
                    return Self::discard(staged, r);
                }
                Err(e) => {
                    let r = self.fail(c, "verify", format_args!("verify failed: {e}"));
                    return Self::discard(staged, r);
                }
            }
        }
        // d. Atomic exchange; afterwards `staged` names the previous original.
        if let Err(e) = swap(staged, &c.path) {
            let flagged =
                fs::symlink_metadata(&c.path).map_or(0, |m| m.st_flags() & sys::PROTECTED_FLAGS);
            let r = if e.kind() == io::ErrorKind::PermissionDenied && flagged != 0 {
                // Flagged during the run: the file is left alone, the directory is fine.
                self.log.file_event(
                    Level::Info,
                    &c.path,
                    FileEvent::Skip("flag"),
                    format_args!("skipped, flag {} set during run", sys::flag_names(flagged)),
                );
                Commit::Skipped
            } else if e.kind() == io::ErrorKind::PermissionDenied {
                self.remember_unwritable(Self::parent(c));
                self.fail(
                    c,
                    "perm",
                    format_args!("parent dir not writable (rename failed: {e})"),
                )
            } else {
                self.fail(c, "swap", format_args!("rename failed: {e}"))
            };
            return Self::discard(staged, r);
        }
        let swapped_out_ok =
            fs::symlink_metadata(staged).is_ok_and(|m| m.is_file() && FileKey::of(&m) == c.key);
        if !swapped_out_ok {
            return match swap(staged, &c.path) {
                Ok(()) => self.changed(c, staged),
                Err(e) => self.rescue(c, staged, &e),
            };
        }
        if let Err(e) = fs::remove_file(staged) {
            self.log.warn(format_args!(
                "cannot remove replaced original {}: {e}",
                staged.display()
            ));
        }
        // e.
        let after = sm.blocks() * 512;
        self.log.file_event(
            Level::Info,
            &c.path,
            FileEvent::Compressed {
                size: c.size(),
                after,
            },
            "",
        );
        Commit::Compressed {
            before: c.allocated(),
            after,
            ino: sm.ino(),
        }
    }

    /// Swap-back failed: the file that was at the path now sits in staging. Mark the staging
    /// dir with the [`KEEP_MARKER`] directory first (so stale cleanup never removes it), then move it to a
    /// rescue dir that is never cleaned, walked or removed by this tool.
    fn rescue(&self, c: &Candidate, staged: &Path, err: &io::Error) -> Commit {
        let staging = staged.parent().unwrap_or(Path::new("/"));
        // A directory, so it can never truncate or clobber a user file of that name.
        let marked = match DirBuilder::new().create(staging.join(KEEP_MARKER)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && has_keep_marker(staging) => {
                Ok(())
            }
            Err(e) => Err(e),
        };
        if let Err(e) = marked {
            self.log.error(format_args!(
                "cannot create {} in {}: {e}",
                KEEP_MARKER,
                staging.display()
            ));
        }
        let rescue = rescue_path(staging);
        let base = staged.file_name().unwrap_or_default();
        match fs::rename(staging, &rescue) {
            Ok(()) => {
                self.fail(
                    c,
                    "swap",
                    format_args!(
                        "changed during run and swap back failed ({err}); the file that was at this path is preserved at {}",
                        rescue.join(base).display()
                    ),
                );
                Commit::Rescued(rescue)
            }
            Err(e) => {
                self.fail(
                    c,
                    "swap",
                    format_args!(
                        "changed during run and swap back failed ({err}); the file that was at this path is preserved at {} (cannot move it to {}: {e})",
                        staged.display(),
                        rescue.display()
                    ),
                );
                Commit::Rescued(staging.to_path_buf())
            }
        }
    }
}

/// First metadata mismatch between the staged copy and the original, if any (gid excluded).
fn metadata_diff(c: &Candidate, sm: &Metadata) -> Option<&'static str> {
    if sm.size() != c.size() {
        Some("size")
    } else if sm.mode() != c.mode {
        Some("mode")
    } else if (sm.mtime(), sm.mtime_nsec()) != (c.key.mtime_sec, c.key.mtime_nsec) {
        Some("mtime")
    } else if sm.uid() != c.uid {
        Some("owner")
    } else {
        None
    }
}

/// Opens the original once (no symlink follow), checks it is still the walked inode and
/// byte-compares it with the staged copy.
fn verify(c: &Candidate, staged: &Path, bufs: &mut Buffers) -> io::Result<Verify> {
    let mut orig = match File::options()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(&c.path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Verify::Changed),
        Err(e) => return Err(e),
    };
    let m = orig.metadata()?;
    if !m.is_file() || FileKey::of(&m) != c.key {
        return Ok(Verify::Changed);
    }
    let mut copy = File::open(staged)?;
    let (a, b) = bufs.get();
    loop {
        let na = read_full(&mut orig, a)?;
        let nb = read_full(&mut copy, b)?;
        if na != nb || a[..na] != b[..nb] {
            return Ok(Verify::Mismatch);
        }
        if na == 0 {
            return Ok(Verify::Same);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acz-cmp-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cand(path: &str) -> Candidate {
        let m = fs::symlink_metadata("/").unwrap();
        Candidate::new(PathBuf::from(path), Arc::from(Path::new("/r")), &m)
    }

    #[test]
    fn batches_unique_basenames_and_limits() {
        let mut files = Vec::new();
        for d in 0..5 {
            for f in 0..10 {
                files.push(cand(&format!("/r/d{d}/f{f}")));
            }
        }
        files.push(cand("/r/d0/unique"));
        let batches = split_batches(files, 4, MAX_ARG_BYTES);
        let total: usize = batches.iter().map(Vec::len).sum();
        assert_eq!(total, 51);
        for b in &batches {
            assert!(b.len() <= 4);
            let names: HashSet<_> = b.iter().map(|c| c.path.file_name().unwrap()).collect();
            assert_eq!(names.len(), b.len(), "duplicate basename in a batch");
        }
        // 5 layers of 10 (+1) files, 4 per batch: 3 batches per layer.
        assert_eq!(batches.len(), 15);

        let files: Vec<_> = (0..10).map(|i| cand(&format!("/r/{i:03}"))).collect();
        // "/r/000" + NUL = 7 bytes: 3 per 21-byte batch.
        let batches = split_batches(files, 100, 21);
        assert_eq!(
            batches.iter().map(Vec::len).collect::<Vec<_>>(),
            [3, 3, 3, 1]
        );
    }

    #[test]
    fn ditto_with_many_missing_sources_terminates() {
        let dir = temp_dir("missing");
        let missing: Vec<PathBuf> = (0..300)
            .map(|i| dir.join(format!("missing-file-with-a-long-name-{i}")))
            .collect();
        let refs: Vec<&Path> = missing.iter().map(PathBuf::as_path).collect();
        let run = run_ditto(&refs, &dir, Duration::from_secs(120));
        assert!(matches!(run.status, DittoStatus::Exited(_)));
        assert!(!run.success());
        assert!(!run.stderr.is_empty() && run.stderr.len() <= STDERR_KEEP + 1024);
        fs::remove_dir_all(&dir).unwrap();
    }

    struct Fixture {
        dir: PathBuf,
        log: Logger,
        cand: Candidate,
        staged: PathBuf,
    }

    /// An original with compressible content and its ditto-compressed staged copy.
    fn fixture(tag: &str) -> Fixture {
        let dir = temp_dir(tag);
        let root = dir.join("root");
        fs::create_dir(&root).unwrap();
        let path = root.join("data.txt");
        fs::write(&path, "compress me please\n".repeat(20_000)).unwrap();
        let m = fs::symlink_metadata(&path).unwrap();
        let cand = Candidate::new(path.clone(), Arc::from(root.as_path()), &m);
        let staging = Compressor::make_staging(&root, 0).unwrap();
        let run = run_ditto(&[&path], &staging, Duration::from_secs(120));
        assert!(run.success(), "{}", run.describe());
        let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
        Fixture {
            staged: staging.join("data.txt"),
            dir,
            log,
            cand,
        }
    }

    impl Fixture {
        fn compressor(&self) -> Compressor<'_> {
            Compressor::new(&self.log, true, 10, 1, 0)
        }

        /// Replaces the original by a new inode with different content.
        fn replace_original(&self) {
            let tmp = self.dir.join("new");
            fs::write(&tmp, "new content").unwrap();
            fs::rename(&tmp, &self.cand.path).unwrap();
        }
    }

    fn flags(p: &Path) -> u32 {
        fs::symlink_metadata(p).unwrap().st_flags()
    }

    #[test]
    fn commit_swaps_in_compressed_copy() {
        let f = fixture("ok");
        let r = f.compressor().commit(
            &f.cand,
            &f.staged,
            false,
            &mut Buffers::default(),
            &sys::rename_swap,
        );
        let Commit::Compressed { ino, .. } = r else {
            panic!("{r:?}")
        };
        assert_eq!(ino, fs::symlink_metadata(&f.cand.path).unwrap().ino());
        assert_ne!(flags(&f.cand.path) & UF_COMPRESSED, 0);
        assert_eq!(
            fs::read_to_string(&f.cand.path).unwrap(),
            "compress me please\n".repeat(20_000)
        );
        assert!(!f.staged.exists(), "old original left in staging");
        fs::remove_dir_all(&f.dir).unwrap();
    }

    /// Clears `uchg` on drop, also when the test fails.
    struct Unflag(PathBuf);

    impl Drop for Unflag {
        fn drop(&mut self) {
            let _ = std::process::Command::new("/usr/bin/chflags")
                .arg("nouchg")
                .arg(&self.0)
                .status();
        }
    }

    #[test]
    fn file_flagged_during_run_is_skipped_not_blamed_on_the_dir() {
        let f = fixture("flagrace");
        let _unflag = Unflag(f.cand.path.clone());
        let ok = std::process::Command::new("/usr/bin/chflags")
            .arg("uchg")
            .arg(&f.cand.path)
            .status()
            .unwrap();
        assert!(ok.success());
        let cmp = f.compressor();
        let r = cmp.commit(
            &f.cand,
            &f.staged,
            false,
            &mut Buffers::default(),
            &sys::rename_swap,
        );
        assert_eq!(r, Commit::Skipped);
        assert!(
            !cmp.parent_unwritable(&f.cand),
            "parent remembered as unwritable"
        );
        assert_eq!(flags(&f.cand.path) & UF_COMPRESSED, 0);
        f.log.flush();
        let log = fs::read_to_string(f.dir.join("log").join(crate::logger::LOG_NAME)).unwrap();
        assert!(log.contains("skipped, flag uchg set during run"), "{log}");
        assert!(!log.contains(" ERROR "), "{log}");
        drop(_unflag);
        fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn replaced_inode_before_commit_is_skipped() {
        let f = fixture("replaced");
        f.replace_original();
        let r = f.compressor().commit(
            &f.cand,
            &f.staged,
            false,
            &mut Buffers::default(),
            &sys::rename_swap,
        );
        assert_eq!(r, Commit::Skipped);
        assert_eq!(fs::read_to_string(&f.cand.path).unwrap(), "new content");
        fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn replaced_inode_before_swap_is_swapped_back() {
        let f = fixture("swapback");
        let calls = Cell::new(0);
        let swap = |a: &Path, b: &Path| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                f.replace_original();
            }
            sys::rename_swap(a, b)
        };
        let r = f
            .compressor()
            .commit(&f.cand, &f.staged, false, &mut Buffers::default(), &swap);
        assert_eq!(r, Commit::Skipped);
        assert_eq!(calls.get(), 2);
        assert_eq!(fs::read_to_string(&f.cand.path).unwrap(), "new content");
        assert_eq!(flags(&f.cand.path) & UF_COMPRESSED, 0);
        fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn failed_swap_back_moves_staging_to_rescue() {
        let f = fixture("rescue");
        let calls = Cell::new(0);
        let swap = |a: &Path, b: &Path| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                f.replace_original();
                sys::rename_swap(a, b)
            } else {
                Err(io::Error::other("injected swap-back failure"))
            }
        };
        let r = f
            .compressor()
            .commit(&f.cand, &f.staged, false, &mut Buffers::default(), &swap);
        let Commit::Rescued(rescue) = r else {
            panic!("{r:?}")
        };
        assert!(rescue.join(KEEP_MARKER).exists(), "keep marker missing");
        let name = rescue.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(RESCUE_PREFIX), "{name}");
        assert!(!f.staged.parent().unwrap().exists());
        // The file that was at the path survives in the rescue dir.
        assert_eq!(
            fs::read_to_string(rescue.join("data.txt")).unwrap(),
            "new content"
        );
        // The path holds the verified compressed copy of the walked version.
        assert_eq!(
            fs::read_to_string(&f.cand.path).unwrap(),
            "compress me please\n".repeat(20_000)
        );
        let log = fs::read_to_string(f.dir.join("log").join(crate::logger::LOG_NAME)).unwrap();
        assert!(
            log.contains(" ERROR ") && log.contains("swap back failed"),
            "{log}"
        );
        fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn copy_of_failed_ditto_call_is_verified_even_without_verify() {
        let f = fixture("forced");
        // Same size and metadata, different bytes: only a byte compare can tell.
        let other = f.dir.join("other");
        fs::create_dir(&other).unwrap();
        let src = other.join("data.txt");
        fs::write(&src, "compress me PLEASE\n".repeat(20_000)).unwrap();
        let staging2 = f.dir.join("staging2");
        fs::create_dir(&staging2).unwrap();
        assert!(run_ditto(&[&src], &staging2, Duration::from_secs(120)).success());
        fs::rename(staging2.join("data.txt"), &f.staged).unwrap();
        let mut cmp = f.compressor();
        cmp.verify = false;
        let r = cmp.commit(
            &f.cand,
            &f.staged,
            true,
            &mut Buffers::default(),
            &sys::rename_swap,
        );
        assert_eq!(r, Commit::Failed);
        assert_eq!(flags(&f.cand.path) & UF_COMPRESSED, 0);
        assert_eq!(
            fs::read_to_string(&f.cand.path).unwrap(),
            "compress me please\n".repeat(20_000)
        );
        fs::remove_dir_all(&f.dir).unwrap();
    }

    /// A compressed file made by ditto at `dest_dir/name`.
    fn compressed_copy(dir: &Path, dest_dir: &Path, name: &str) {
        let src_dir = dir.join(format!("src-{name}"));
        fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join(name);
        fs::write(&src, "compressible ".repeat(10_000)).unwrap();
        assert!(run_ditto(&[&src], dest_dir, Duration::from_secs(120)).success());
        assert_ne!(flags(&dest_dir.join(name)) & UF_COMPRESSED, 0);
    }

    fn log_text(dir: &Path) -> String {
        fs::read_to_string(dir.join("log").join(crate::logger::LOG_NAME)).unwrap()
    }

    #[test]
    fn stale_cleanup_deletes_only_compressed_copies() {
        let dir = temp_dir("stale");
        let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
        // Only compressed copies (or empty): removed.
        let stale = dir.join(".acz-staging-1-2");
        fs::create_dir(&stale).unwrap();
        compressed_copy(&dir, &stale, "copy.txt");
        let empty = dir.join(".acz-staging-1-5");
        fs::create_dir(&empty).unwrap();
        // Not our name: untouched.
        let other = dir.join(".acz-staging-x");
        fs::create_dir(&other).unwrap();
        // Rescue marker directory: kept as is.
        let kept = dir.join(".acz-staging-1-3");
        fs::create_dir_all(kept.join(KEEP_MARKER)).unwrap();
        fs::write(kept.join("data.txt"), "rescued").unwrap();
        // Crash between swap and post-check: an uncompressed user inode -> moved, not deleted.
        let crashed = dir.join(".acz-staging-1-4");
        fs::create_dir(&crashed).unwrap();
        fs::write(crashed.join("orig.py"), "user data").unwrap();
        // A user FILE named like the marker does not protect (and is not truncated).
        let fake = dir.join(".acz-staging-1-6");
        fs::create_dir(&fake).unwrap();
        fs::write(fake.join(KEEP_MARKER), "user file").unwrap();

        let all = [&stale, &empty, &other, &kept, &crashed, &fake].map(|p| p.to_path_buf());
        remove_stale(&all, &log);
        assert!(!stale.exists() && !empty.exists());
        assert!(other.exists());
        assert_eq!(
            fs::read_to_string(kept.join("data.txt")).unwrap(),
            "rescued"
        );
        assert!(!crashed.exists());
        assert_eq!(
            fs::read_to_string(dir.join(".acz-rescue-1-4/orig.py")).unwrap(),
            "user data"
        );
        assert_eq!(
            fs::read_to_string(dir.join(".acz-rescue-1-6").join(KEEP_MARKER)).unwrap(),
            "user file"
        );
        let text = log_text(&dir);
        assert!(text.contains("WARN kept rescue staging dir"), "{text}");
        assert!(text.contains("moved to"), "{text}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn batches_never_mix_names_differing_only_by_case() {
        let files = ["/r/a/LICENSE", "/r/b/license", "/r/c/License", "/r/d/other"]
            .map(cand)
            .to_vec();
        let batches = split_batches(files, 100, MAX_ARG_BYTES);
        for b in &batches {
            let names: HashSet<String> = b
                .iter()
                .map(|c| c.path.file_name().unwrap().to_string_lossy().to_lowercase())
                .collect();
            assert_eq!(names.len(), b.len(), "case-colliding names in one batch");
        }
        assert_eq!(batches.len(), 3);
    }

    #[test]
    fn batches_respect_data_limit() {
        let mut files: Vec<Candidate> = (0..5).map(|i| cand(&format!("/r/f{i}"))).collect();
        for f in &mut files {
            f.key.size = MAX_BATCH_DATA / 2;
        }
        let batches = split_batches(files, 100, MAX_ARG_BYTES);
        assert_eq!(batches.iter().map(Vec::len).collect::<Vec<_>>(), [2, 2, 1]);
    }

    #[test]
    fn lone_file_over_the_data_limit_is_its_own_batch() {
        let mut files: Vec<Candidate> = (0..3).map(|i| cand(&format!("/r/f{i}"))).collect();
        files[1].key.size = MAX_BATCH_DATA + 1;
        let batches = split_batches(files, 100, MAX_ARG_BYTES);
        let sizes: Vec<Vec<u64>> = batches
            .iter()
            .map(|b| b.iter().map(Candidate::size).collect())
            .collect();
        assert_eq!(sizes.len(), 3, "{sizes:?}");
        assert!(sizes.contains(&vec![MAX_BATCH_DATA + 1]), "{sizes:?}");
    }

    /// `n` compressible files under `<dir>/root/<sub>`.
    fn files_in(dir: &Path, sub: &str, n: usize) -> Vec<Candidate> {
        let root = dir.join("root");
        let d = root.join(sub);
        fs::create_dir_all(&d).unwrap();
        (0..n)
            .map(|i| {
                let p = d.join(format!("{sub}{i}.txt"));
                fs::write(&p, format!("{sub} {i} compressible\n").repeat(5000)).unwrap();
                Candidate::new(
                    p.clone(),
                    Arc::from(root.as_path()),
                    &fs::symlink_metadata(&p).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn low_disk_space_skips_without_spawning_ditto() {
        let dir = temp_dir("lowspace");
        let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
        let files = files_in(&dir, "a", 3);
        let before: Vec<u64> = files.iter().map(|c| c.key.ino).collect();
        let out = Compressor::new(&log, true, 100, 2, u64::MAX / 2).run(files.clone());
        assert_eq!((out.ditto_calls, out.compressed, out.errors), (0, 0, 0));
        assert_eq!(out.skipped_low_space, 3);
        for (c, ino) in files.iter().zip(before) {
            let m = fs::symlink_metadata(&c.path).unwrap();
            assert_eq!(m.ino(), ino);
            assert_eq!(m.st_flags() & UF_COMPRESSED, 0);
        }
        let text = log_text(&dir);
        assert_eq!(text.matches(" WARN ").count(), 1, "{text}");
        assert!(text.contains("low disk space"), "{text}");
        assert!(fs::read_dir(dir.join("root")).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(STAGING_PREFIX)
        }));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unwritable_parent_is_remembered_for_the_run() {
        let dir = temp_dir("eacces");
        let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
        let mut files = files_in(&dir, "ro", 3);
        files.extend(files_in(&dir, "rw", 1));
        let ro = dir.join("root/ro");
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
        let out = Compressor::new(&log, true, 100, 1, 0).run(files.clone());
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            (out.ditto_calls, out.compressed, out.errors),
            (1, 1, 3),
            "{}",
            log_text(&dir)
        );
        assert_eq!(out.committed.len(), 1);
        assert_eq!(out.committed[0].path, files[3].path);
        let text = log_text(&dir);
        assert_eq!(text.matches("parent dir not writable").count(), 3, "{text}");
        assert_eq!(text.matches("rename failed").count(), 1, "{text}");
        for c in &files[..3] {
            let m = fs::symlink_metadata(&c.path).unwrap();
            assert_eq!(m.ino(), c.key.ino);
            assert_eq!(m.st_flags() & UF_COMPRESSED, 0);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Two files of equal size, different bytes, names equal up to case.
    fn case_pair(dir: &Path) -> (Candidate, Candidate) {
        let root = dir.join("root");
        fs::create_dir_all(root.join("x")).unwrap();
        fs::create_dir_all(root.join("y")).unwrap();
        let a = root.join("x/README");
        let b = root.join("y/readme");
        fs::write(&a, "AAAA compressible\n".repeat(10_000)).unwrap();
        fs::write(&b, "bbbb compressible\n".repeat(10_000)).unwrap();
        let mk = |p: &Path| {
            Candidate::new(
                p.to_path_buf(),
                Arc::from(root.as_path()),
                &fs::symlink_metadata(p).unwrap(),
            )
        };
        (mk(&a), mk(&b))
    }

    fn quiet(log: &Logger) -> Compressor<'_> {
        Compressor::new(log, false, 100, 2, 0)
    }

    #[test]
    fn case_colliding_names_in_one_staging_keep_their_bytes() {
        let dir = temp_dir("case-process");
        let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
        let (a, b) = case_pair(&dir);
        let staging = Compressor::make_staging(&a.match_root, 0).unwrap();
        let mut out = Outcome::default();
        quiet(&log).process(&staging, &[&a, &b], &mut Buffers::default(), &mut out);
        assert_eq!(out.errors, 0, "{}", log_text(&dir));
        assert_eq!(out.compressed, 2, "{}", log_text(&dir));
        assert_eq!(
            fs::read_to_string(&a.path).unwrap(),
            "AAAA compressible\n".repeat(10_000)
        );
        assert_eq!(
            fs::read_to_string(&b.path).unwrap(),
            "bbbb compressible\n".repeat(10_000)
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn case_colliding_names_end_to_end() {
        let dir = temp_dir("case-run");
        let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
        let (a, b) = case_pair(&dir);
        let out = quiet(&log).run(vec![a.clone(), b.clone()]);
        assert_eq!((out.errors, out.compressed, out.ditto_calls), (0, 2, 2));
        assert_eq!(
            fs::read_to_string(&a.path).unwrap(),
            "AAAA compressible\n".repeat(10_000)
        );
        assert_eq!(
            fs::read_to_string(&b.path).unwrap(),
            "bbbb compressible\n".repeat(10_000)
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failing_retries_stop_after_three() {
        let dir = temp_dir("storm");
        let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
        let root = dir.join("root");
        fs::create_dir(&root).unwrap();
        let files: Vec<Candidate> = (0..6)
            .map(|i| {
                let p = root.join(format!("locked{i}.txt"));
                fs::write(&p, "compressible ".repeat(1000)).unwrap();
                fs::set_permissions(&p, fs::Permissions::from_mode(0o000)).unwrap();
                Candidate::new(
                    p.clone(),
                    Arc::from(root.as_path()),
                    &fs::symlink_metadata(&p).unwrap(),
                )
            })
            .collect();
        let out = quiet(&log).run(files);
        // One batch call + three single retries, then the batch is given up.
        assert_eq!(out.ditto_calls, 4);
        assert_eq!(out.errors, 6);
        assert!(log_text(&dir).contains("ditto keeps failing"));
        let not_attempted = log_text(&dir)
            .lines()
            .filter(|l| l.contains(" ERROR ") && l.contains("not attempted: ditto kept failing"))
            .count();
        assert_eq!(not_attempted, 3, "one ERROR line per not-attempted file");
        for e in fs::read_dir(&root).unwrap() {
            fs::set_permissions(e.unwrap().path(), fs::Permissions::from_mode(0o644)).unwrap();
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
