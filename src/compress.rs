//! Batched `ditto --hfsCompression` (or the in-process [`crate::applesauce`] writer) into
//! staging dirs, then a checked, atomic swap per file.
//!
//! The original stays the source of truth until a verified compressed copy is swapped in with
//! `renamex_np(RENAME_SWAP)`; every failure path leaves the original untouched. Per-file outcomes
//! are reported as typed [`FileEvent`]s (log line plus terminal line). No batch starts
//! without `min_free_bytes` left after its data, and a parent directory found not writable is
//! remembered for the rest of the run.

use crate::applesauce::{self, PART_DIR, Staged};
use crate::config::VerifyMode;
use crate::logger::{Level, Logger};
use crate::progress::{FileEvent, human};
use crate::sys::{self, O_NOFOLLOW, UF_COMPRESSED};
use crate::walk::{Candidate, FileKey, RESCUE_PREFIX, STAGING_PREFIX, is_stale_staging_name};
use applesauce_core::compressor::{self as codec, Kind};
use applesauce_core::reader::Reader;
use applesauce_core::{BLOCK_SIZE, decmpfs};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, DirBuilder, File, FileTimes, Metadata};
use std::io::{self, Read};
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DITTO: &str = "/usr/bin/ditto";
/// ditto argument sets, newest first. Since macOS 27 ditto clones files by default
/// (`cloneFiles: true`) and a clone is never compressed, so `--noclone` is required there;
/// older ditto may not know the option. [`probe`] picks the first set that compresses.
pub(crate) const DITTO_ARGS: [DittoArgs; 2] =
    [&["--hfsCompression", "--noclone"], &["--hfsCompression"]];
/// Source file of the start-up probe, kept uncompressed next to the config.
const PROBE_NAME: &str = "ditto-probe.txt";
/// Directory next to the config holding the probe's compressed copy, rebuilt by every probe.
const PROBE_OUT: &str = "ditto-probe.out";
/// Upper bound of source path bytes passed to one ditto call (`ARG_MAX` is 1 MiB incl. env).
const MAX_ARG_BYTES: usize = 256 * 1024;
const STDERR_KEEP: usize = 4096;
const DITTO_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// The probe copies one small file: a hung ditto must not hold the start of a run for long.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const VERIFY_BUF: usize = 1 << 20;
/// Upper bound of file data copied by one ditto call (bounds staging space and timeout risk).
const MAX_BATCH_DATA: u64 = 2 << 30;
/// Consecutive failed single-file retries after which the rest of a batch is given up.
const MAX_RETRY_FAILURES: u32 = 3;
/// Marker directory inside a staging dir holding a rescued file; stale cleanup keeps such a dir.
pub(crate) const KEEP_MARKER: &str = ".acz-keep";

/// The options passed to ditto before `--`.
pub(crate) type DittoArgs = &'static [&'static str];

/// How the staged copies are made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Backend {
    /// `/usr/bin/ditto` with the options chosen by [`probe`].
    Ditto(DittoArgs),
    /// [`applesauce::stage`] per file; every copy is byte-compared and its xattrs checked.
    Applesauce,
}

/// A file replaced by its compressed copy: the identity and sizes for `state.tsv`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Committed {
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
pub(crate) struct Outcome {
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

/// Per-worker verify (and applesauce) buffers, allocated on first use and reused for every file.
#[derive(Default)]
pub(crate) struct Buffers {
    a: Vec<u8>,
    b: Vec<u8>,
    applesauce: Option<applesauce::Buffers>,
    decode: Decode,
}

/// Buffers of the independent decode of [`verify_extended`], allocated on first use.
#[derive(Default)]
struct Decode {
    /// One verify chunk as decoded from the stored data.
    chunk: Vec<u8>,
    /// One decompressed block; one byte more than [`BLOCK_SIZE`] (the decompressor fails
    /// when the output fills its buffer).
    block: Vec<u8>,
    /// One compressed block as stored.
    compressed: Vec<u8>,
    lzvn: Option<codec::Compressor>,
    lzfse: Option<codec::Compressor>,
}

impl Buffers {
    pub(crate) fn get(&mut self) -> (&mut [u8], &mut [u8]) {
        if self.a.is_empty() {
            self.a = vec![0; VERIFY_BUF];
            self.b = vec![0; VERIFY_BUF];
        }
        (&mut self.a, &mut self.b)
    }

    /// The verify buffers plus the decode buffers.
    fn get_extended(&mut self) -> (&mut [u8], &mut [u8], &mut Decode) {
        self.get();
        let d = &mut self.decode;
        if d.chunk.is_empty() {
            d.chunk = vec![0; VERIFY_BUF];
            d.block = vec![0; BLOCK_SIZE + 1];
        }
        (&mut self.a, &mut self.b, d)
    }

    fn applesauce(&mut self) -> &mut applesauce::Buffers {
        self.applesauce
            .get_or_insert_with(applesauce::Buffers::default)
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
    timeout: Duration,
}

impl DittoRun {
    fn success(&self) -> bool {
        matches!(&self.status, DittoStatus::Exited(s) if s.success())
    }

    fn describe(&self) -> String {
        let status = match &self.status {
            DittoStatus::Exited(s) => s.to_string(),
            DittoStatus::TimedOut => format!("timed out after {} s", self.timeout.as_secs()),
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
                child.kill().ok();
                child.wait().ok();
                return DittoStatus::TimedOut;
            }
            Ok(None) => {
                thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(100));
            }
            Err(e) => {
                child.kill().ok();
                child.wait().ok();
                return DittoStatus::WaitFailed(e);
            }
        }
    }
}

/// `ditto <args...> -- <sources...> <staging>/`; stderr drained to EOF on a helper thread.
fn run_ditto(args: DittoArgs, sources: &[&Path], staging: &Path, timeout: Duration) -> DittoRun {
    let mut dest = staging.as_os_str().to_owned();
    dest.push("/");
    let spawned = Command::new(DITTO)
        .args(args)
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
                timeout,
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
    DittoRun {
        status,
        stderr,
        timeout,
    }
}

/// Content of the probe source: plain text that always compresses.
fn probe_content() -> Vec<u8> {
    b"auto-compress-apfs ditto probe: this line repeats, so the file compresses well\n".repeat(2048)
}

/// Why [`probe`] found no ditto arguments.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProbeError {
    /// The probe files could not be written in its directory; ditto was not judged.
    Setup(String),
    /// No set of [`DITTO_ARGS`] produced a compressed, byte-identical copy.
    NoCompression(String),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Setup(e) | Self::NoCompression(e) => f.write_str(e),
        }
    }
}

/// Finds the ditto arguments that compress on this system. `<dir>/ditto-probe.txt` is
/// (re)written when missing, changed or compressed, then copied into `<dir>/ditto-probe.out/`
/// with each set of [`DITTO_ARGS`] in turn; the first copy that is compressed and
/// byte-identical wins and stays there until the next probe. A `dir` outside APFS is a
/// [`ProbeError::Setup`]: no copy there could be compressed.
pub(crate) fn probe(dir: &Path) -> Result<DittoArgs, ProbeError> {
    probe_with(dir, &DITTO_ARGS)
}

/// [`probe`] with the candidate argument sets given.
fn probe_with(dir: &Path, sets: &[DittoArgs]) -> Result<DittoArgs, ProbeError> {
    let (src, want) = probe_source(dir)?;
    let out = dir.join(PROBE_OUT);
    let copy = out.join(PROBE_NAME);
    let mut failed = Vec::new();
    for &args in sets {
        reset_probe_out(&out)?;
        let run = run_ditto(args, &[&src], &out, PROBE_TIMEOUT);
        let why = if run.success() {
            match fs::symlink_metadata(&copy) {
                Ok(m) if !m.is_file() => "copy is not a regular file".to_owned(),
                Ok(m) if m.st_flags() & UF_COMPRESSED == 0 => "copy is not compressed".to_owned(),
                Ok(_) if fs::read(&copy).is_ok_and(|b| b == want) => return Ok(args),
                Ok(_) => "copy differs from the source".to_owned(),
                Err(e) => format!("no copy: {e}"),
            }
        } else {
            format!("ditto failed: {}", run.describe())
        };
        failed.push(format!("{}: {why}", args.join(" ")));
    }
    Err(ProbeError::NoCompression(failed.join("; ")))
}

/// The applesauce counterpart of [`probe`]: `<dir>/ditto-probe.txt` (kept as for [`probe`])
/// is staged by [`applesauce::stage`] into `<dir>/ditto-probe.out/`; the copy must be
/// compressed and read back byte-identical through the kernel.
pub(crate) fn probe_applesauce(dir: &Path) -> Result<(), ProbeError> {
    let (src, want) = probe_source(dir)?;
    let out = dir.join(PROBE_OUT);
    reset_probe_out(&out)?;
    let m = fs::symlink_metadata(&src)
        .map_err(|e| ProbeError::Setup(format!("cannot stat {}: {e}", src.display())))?;
    let c = Candidate::new(src, Arc::from(out.as_path()), &m);
    let why = match applesauce::stage(&c, &out, &mut applesauce::Buffers::default()) {
        Ok(Staged::Copy(copy)) => match fs::symlink_metadata(&copy) {
            Ok(m) if m.st_flags() & UF_COMPRESSED == 0 => "copy is not compressed".to_owned(),
            Ok(_) if fs::read(&copy).is_ok_and(|b| b == want) => return Ok(()),
            Ok(_) => "copy differs from the source".to_owned(),
            Err(e) => format!("no copy: {e}"),
        },
        Ok(other) => format!("not staged: {other:?}"),
        Err(e) => format!("cannot write the copy: {e}"),
    };
    Err(ProbeError::NoCompression(format!("applesauce: {why}")))
}

/// For `verify = "extended"`, after a passed [`probe`] in `dir`: the ditto copy of the probe
/// must have a decmpfs type the independent decoder reads and decode to the probe source.
#[expect(
    clippy::many_single_char_names,
    reason = "short local names in a short probe helper"
)]
pub(crate) fn probe_decode(dir: &Path) -> Result<(), ProbeError> {
    let src = dir.join(PROBE_NAME);
    let copy = dir.join(PROBE_OUT).join(PROBE_NAME);
    let no = |e: String| ProbeError::NoCompression(format!("independent decode: {e}"));
    let f = File::open(&copy).map_err(|e| no(format!("cannot open {}: {e}", copy.display())))?;
    let header = sys::fget_xattr(&f, sys::DECMPFS, true)
        .map_err(|e| no(format!("cannot read the decmpfs xattr: {e}")))?
        .ok_or_else(|| no("the ditto copy has no decmpfs xattr".to_owned()))?;
    let value = decmpfs::Value::from_data(&header).map_err(|e| no(e.to_string()))?;
    let t = value.compression_type.raw_type();
    if !DECODABLE_TYPES.contains(&t) {
        return Err(no(format!(
            "ditto writes decmpfs type {t}; set verify = true"
        )));
    }
    let m = fs::symlink_metadata(&src)
        .map_err(|e| ProbeError::Setup(format!("cannot stat {}: {e}", src.display())))?;
    let c = Candidate::new(src, Arc::from(dir), &m);
    match verify_extended(&c, &copy, &mut Buffers::default()) {
        Ok(Verify::Same) => Ok(()),
        Ok(Verify::Changed) => Err(no("the probe source changed".to_owned())),
        Ok(Verify::Mismatch) => Err(no("the decoded copy differs from the source".to_owned())),
        Err(e) => Err(no(e.to_string())),
    }
}

/// Recreates the empty probe output dir `out`.
fn reset_probe_out(out: &Path) -> Result<(), ProbeError> {
    match fs::remove_dir_all(out) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            let e = format!("cannot remove {}: {e}", out.display());
            return Err(ProbeError::Setup(e));
        }
        _ => {}
    }
    fs::create_dir(out)
        .map_err(|e| ProbeError::Setup(format!("cannot create {}: {e}", out.display())))
}

/// Checks that `dir` is on APFS and (re)writes `<dir>/ditto-probe.txt` when it is missing,
/// changed or compressed; returns its path and content.
fn probe_source(dir: &Path) -> Result<(PathBuf, Vec<u8>), ProbeError> {
    match sys::statfs(dir) {
        Ok(fs) if fs.fstype == "apfs" => {}
        Ok(fs) => {
            let e = format!("{} is on {}, not APFS", dir.display(), fs.fstype);
            return Err(ProbeError::Setup(e));
        }
        Err(e) => {
            return Err(ProbeError::Setup(format!(
                "cannot statfs {}: {e}",
                dir.display()
            )));
        }
    }
    let src = dir.join(PROBE_NAME);
    let want = probe_content();
    let intact = fs::symlink_metadata(&src)
        .is_ok_and(|m| m.is_file() && m.st_flags() & UF_COMPRESSED == 0)
        && fs::read(&src).is_ok_and(|b| b == want);
    if !intact {
        fs::remove_file(&src).ok();
        fs::write(&src, &want)
            .map_err(|e| ProbeError::Setup(format!("cannot write {}: {e}", src.display())))?;
    }
    Ok((src, want))
}

/// Groups `files` by backend, match root and device and splits each group by
/// [`split_batches`]: every batch has one backend.
fn batches_of(
    files: Vec<(Candidate, Backend)>,
    batch_size: usize,
) -> Vec<(Backend, Vec<Candidate>)> {
    let mut groups: BTreeMap<(Backend, Arc<Path>, u64), Vec<Candidate>> = BTreeMap::new();
    for (c, backend) in files {
        groups
            .entry((backend, Arc::clone(&c.match_root), c.key.dev))
            .or_default()
            .push(c);
    }
    groups
        .into_iter()
        .flat_map(|((backend, ..), g)| {
            split_batches(g, batch_size, MAX_ARG_BYTES)
                .into_iter()
                .map(move |b| (backend, b))
        })
        .collect()
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

pub(crate) fn system_time(sec: i64, nsec: i64) -> SystemTime {
    let nanos = Duration::from_nanos(nsec.cast_unsigned());
    if sec >= 0 {
        UNIX_EPOCH + Duration::from_secs(sec.cast_unsigned()) + nanos
    } else {
        UNIX_EPOCH - Duration::from_secs(sec.unsigned_abs()) + nanos
    }
}

/// Reads until `buf` is full or EOF; returns the byte count.
pub(crate) fn read_full(mut f: impl Read, buf: &mut [u8]) -> io::Result<usize> {
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

pub(crate) enum Verify {
    Same,
    Changed,
    Mismatch,
}

/// What a rescued staging dir holds, phrased around its path.
type Preserved = fn(&str) -> String;

const PRESERVED_PREVIOUS: Preserved =
    |at| format!("the file that was at this path is preserved at {at}");
const PRESERVED_COPY: Preserved = |at| {
    format!(
        "the compressed copy (not the original) is preserved at {at}; the original is at the path"
    )
};
const PRESERVED_CHANGED: Preserved = |at| {
    format!(
        "the file that was at this path after the swap, possibly with newer writes, is preserved at {at}; the original is back at the path"
    )
};
const PRESERVED_UNKNOWN: Preserved =
    |at| format!("state unknown, nothing deleted: check both the path and {at}");

/// Creates the [`KEEP_MARKER`] directory in `staging` (a directory, so it can never truncate
/// or clobber a user file of that name); an existing marker directory is fine.
pub(crate) fn mark_keep(staging: &Path) -> io::Result<()> {
    match DirBuilder::new().create(staging.join(KEEP_MARKER)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && has_keep_marker(staging) => Ok(()),
        Err(e) => Err(e),
    }
}

/// True when `dir` contains the rescue marker directory.
pub(crate) fn has_keep_marker(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join(KEEP_MARKER)).is_ok_and(|m| m.is_dir())
}

/// True when every top-level entry of a staging dir is a compressed regular file (a copy made
/// by ditto or applesauce) or the [`PART_DIR`] directory of copies being written (a directory
/// is never a swapped-out original). Anything else may be a user's original swapped out right
/// before a crash.
fn only_staged_copies(dir: &Path) -> io::Result<bool> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let m = entry.metadata()?;
        if m.is_dir() && entry.file_name() == PART_DIR {
            continue;
        }
        if !m.is_file() || m.st_flags() & UF_COMPRESSED == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Cleans leftover staging dirs of crashed runs (only names matching the exact staging regex).
/// A dir with the [`KEEP_MARKER`] directory is kept; a dir holding anything but compressed
/// copies (and the [`PART_DIR`] of partial applesauce copies) is renamed to `.acz-rescue-<pid>-<n>` instead of being deleted.
pub(crate) fn remove_stale(dirs: &[PathBuf], log: &Logger) {
    for dir in dirs {
        let name = dir
            .file_name()
            .map(std::os::unix::ffi::OsStrExt::as_bytes)
            .unwrap_or_default();
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

pub(crate) struct Compressor<'a> {
    pub log: &'a Logger,
    pub verify: VerifyMode,
    pub batch_size: usize,
    pub threads: usize,
    /// Free bytes that must remain on the volume after a batch's data.
    pub min_free_bytes: u64,
    /// The low-space WARN was written (later ones are DEBUG).
    low_space_warned: AtomicBool,
    /// Directories where creating staging or swapping failed with EACCES/EPERM in this run.
    unwritable: Mutex<HashSet<PathBuf>>,
    /// Test seam: runs between an applesauce stage and its commit.
    #[cfg(test)]
    after_stage: Option<fn(&Candidate)>,
    /// Test seam: replaces the post-swap compare of `verify = "extended"`; gets the path.
    #[cfg(test)]
    recheck: Option<fn(&Path) -> io::Result<Verify>>,
}

impl<'a> Compressor<'a> {
    pub(crate) fn new(
        log: &'a Logger,
        verify: VerifyMode,
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
            #[cfg(test)]
            after_stage: None,
            #[cfg(test)]
            recheck: None,
        }
    }

    /// [`Self::run_routed`] with one backend for all `files`.
    #[cfg(test)]
    fn run(&self, files: Vec<Candidate>, backend: Backend) -> Outcome {
        self.run_routed(files.into_iter().map(|c| (c, backend)).collect())
    }

    /// Compresses `files` (all classified as to-compress), each with its backend, batches
    /// running in parallel. A batch never mixes backends.
    pub(crate) fn run_routed(&self, files: Vec<(Candidate, Backend)>) -> Outcome {
        let batches = batches_of(files, self.batch_size);
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
                            let next = queue
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .next();
                            let Some((no, batch)) = next else { break };
                            let (backend, batch) = batch;
                            self.run_batch(no, backend, &batch, &mut bufs, &mut out);
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
        c.path.parent().unwrap_or_else(|| Path::new("/"))
    }

    fn unwritable(&self) -> std::sync::MutexGuard<'_, HashSet<PathBuf>> {
        self.unwritable
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    fn run_batch(
        &self,
        no: usize,
        backend: Backend,
        batch: &[Candidate],
        bufs: &mut Buffers,
        out: &mut Outcome,
    ) {
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
                self.process(&staging, &files, backend, bufs, out);
            }
            Err(e) => {
                self.log.debug(format_args!(
                    "cannot stage in {}: {e}; using per-directory staging",
                    root.display()
                ));
                let mut by_parent: BTreeMap<&Path, Vec<&Candidate>> = BTreeMap::new();
                for c in batch {
                    let parent = c.path.parent().unwrap_or_else(|| Path::new("/"));
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
                        Ok(staging) => self.process(&staging, &files, backend, bufs, out),
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

    /// Makes the staged copies of `files` in `staging` with `backend` (ditto, or applesauce per
    /// file), commits every staged copy, removes `staging`.
    fn process(
        &self,
        staging: &Path,
        files: &[&Candidate],
        backend: Backend,
        bufs: &mut Buffers,
        out: &mut Outcome,
    ) {
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
        let ditto_args = match backend {
            Backend::Ditto(args) => args,
            Backend::Applesauce => return self.process_applesauce(staging, files, bufs, out),
        };
        let sources: Vec<&Path> = files.iter().map(|c| c.path.as_path()).collect();
        out.ditto_calls += 1;
        let run = run_ditto(ditto_args, &sources, staging, DITTO_TIMEOUT);
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
                        fs::remove_file(&staged).ok();
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
                    let retry = run_ditto(ditto_args, &[&c.path], staging, DITTO_TIMEOUT);
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
                let r = self.commit(
                    c,
                    backend,
                    &staged,
                    from_failed_call,
                    bufs,
                    &sys::rename_swap,
                );
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

    /// [`Self::process`] for the applesauce backend: one [`applesauce::stage`] per file, each
    /// copy committed with the byte compare forced on; free space was checked for the batch.
    fn process_applesauce(
        &self,
        staging: &Path,
        files: &[&Candidate],
        bufs: &mut Buffers,
        out: &mut Outcome,
    ) {
        let mut rescued: Option<PathBuf> = None;
        // The volume ran full: the rest of the batch is not attempted, as on the ditto path.
        let mut full = false;
        for c in files {
            if let Some(rescue) = &rescued {
                let r = self.fail(
                    c,
                    "swap",
                    format_args!("not committed: batch staging moved to {}", rescue.display()),
                );
                self.finish(out, c, &r);
                continue;
            }
            if full {
                self.finish(out, c, &Commit::LowSpace);
                continue;
            }
            let r = if self.parent_unwritable(c) {
                self.fail_unwritable(c)
            } else {
                match applesauce::stage(c, staging, bufs.applesauce()) {
                    Ok(Staged::Copy(staged)) => {
                        #[cfg(test)]
                        if let Some(hook) = self.after_stage {
                            hook(c);
                        }
                        // Always byte-compared, whatever `verify` says.
                        self.commit(
                            c,
                            Backend::Applesauce,
                            &staged,
                            true,
                            bufs,
                            &sys::rename_swap,
                        )
                    }
                    Ok(Staged::Incompressible) => {
                        self.log
                            .debug(format_args!("{}: incompressible, cached", c.path.display()));
                        Commit::Incompressible(c.key)
                    }
                    Ok(Staged::Changed) => {
                        self.log.file_event(
                            Level::Info,
                            &c.path,
                            FileEvent::Skip("changed"),
                            "changed during run, skipped",
                        );
                        Commit::Skipped
                    }
                    Ok(Staged::HasXattr(name)) => {
                        self.log.file_event(
                            Level::Info,
                            &c.path,
                            FileEvent::Skip("xattr"),
                            format_args!("has {name} already, skipped"),
                        );
                        Commit::Skipped
                    }
                    Ok(Staged::ReservedName) => {
                        self.log.file_event(
                            Level::Info,
                            &c.path,
                            FileEvent::Skip("name"),
                            format_args!("named like the staging part dir {PART_DIR}, skipped"),
                        );
                        Commit::Skipped
                    }
                    Err(e) if e.kind() == io::ErrorKind::StorageFull => {
                        full = true;
                        let why = format!("low disk space: {e}");
                        let rest: Vec<&Candidate> = files
                            .iter()
                            .skip_while(|f| !std::ptr::eq(**f, *c))
                            .copied()
                            .collect();
                        self.low_space(staging, &rest, &why);
                        Commit::LowSpace
                    }
                    Err(e) => self.fail(c, "io", format_args!("cannot write compressed copy: {e}")),
                }
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
        fs::remove_file(staged).ok();
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

    /// Checks the staged copy made by `backend` against the original and swaps it in. The
    /// byte compare runs unless `verify = false` and `force_verify` is unset (`force_verify`:
    /// an applesauce copy or a copy from a failed ditto call); with `verify = "extended"` it is
    /// [`verify_extended`] (a ditto copy is `F_FULLFSYNC`ed first) and the swapped-in file is
    /// compared again by [`Self::recheck_swapped`]. `swap` is [`sys::rename_swap`] in
    /// production; tests inject failures through it.
    fn commit(
        &self,
        c: &Candidate,
        backend: Backend,
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
        // Setting an mtime older than the birthtime lowers the birthtime on APFS: the
        // applesauce copy got the original's birthtime in staging, put it back after.
        let keep_birth = backend == Backend::Applesauce;
        let set = |f: File| -> io::Result<()> {
            let birth = if keep_birth {
                Some(f.metadata()?.created()?)
            } else {
                None
            };
            f.set_times(times)?;
            if let Some(b) = birth {
                use std::os::macos::fs::FileTimesExt as _;
                f.set_times(FileTimes::new().set_created(b))?;
            }
            Ok(())
        };
        if let Err(e) = opened.and_then(set) {
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
        // The xattrs ditto would carry over, checked for applesauce copies only.
        if backend == Backend::Applesauce {
            match xattr_diff(&c.path, &c.key, staged) {
                Ok(XattrCheck::Same) => {}
                Ok(XattrCheck::Changed) => return self.changed(c, staged),
                Ok(XattrCheck::Differs(diff)) => {
                    let r = self.fail(
                        c,
                        "io",
                        format_args!("staged copy differs ({diff}), discarded"),
                    );
                    return Self::discard(staged, r);
                }
                Err(e) => {
                    let r = self.fail(c, "io", format_args!("cannot compare xattrs: {e}"));
                    return Self::discard(staged, r);
                }
            }
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
        if self.verify != VerifyMode::Off || force_verify {
            let extended = self.verify == VerifyMode::Extended;
            let checked = if !extended {
                verify(c, staged, bufs)
            } else if matches!(backend, Backend::Ditto(_))
                && let Err(e) = File::open(staged).and_then(|f| f.sync_all())
            {
                Err(io::Error::new(
                    e.kind(),
                    format!("cannot sync staged copy: {e}"),
                ))
            } else {
                verify_extended(c, staged, bufs)
            };
            match checked {
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
                Err(e) => self.rescue(
                    c,
                    staged,
                    &format!("changed during run and swap back failed ({e})"),
                    PRESERVED_PREVIOUS,
                ),
            };
        }
        if self.verify == VerifyMode::Extended
            && let Some(r) = self.recheck_swapped(c, staged, &sm, bufs, swap)
        {
            return r;
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

    /// `verify = "extended"`, right after the swap (the original now at `staged`): the file at
    /// the path, opened once without following a symlink, must still be the swapped-in copy
    /// (`sm`, walked size and mtime, compressed); with its cached pages evicted it is then
    /// byte-compared with the original, under the [`KEEP_MARKER`] of the staging dir. `None`
    /// = commit as usual: compare passed, or the path no longer holds the copy (the user wrote
    /// after the swap: DEBUG line, no swap back). On a mismatch or error the original is
    /// swapped back; when anything but the untouched copy then sits in staging, or the
    /// swap back fails, the staging dir is rescued instead of removing anything.
    fn recheck_swapped(
        &self,
        c: &Candidate,
        staged: &Path,
        sm: &Metadata,
        bufs: &mut Buffers,
        swap: &dyn Fn(&Path, &Path) -> io::Result<()>,
    ) -> Option<Commit> {
        let identity = |m: &Metadata| {
            (
                m.size(),
                m.mtime(),
                m.mtime_nsec(),
                m.st_flags() & UF_COMPRESSED,
            )
        };
        let opened = File::options()
            .read(true)
            .custom_flags(O_NOFOLLOW)
            .open(&c.path);
        let (cur, seen) = match opened.and_then(|f| f.metadata().map(|m| (f, m))) {
            Ok((f, m))
                if m.is_file()
                    && (m.dev(), m.ino()) == (c.key.dev, sm.ino())
                    && identity(&m)
                        == (c.size(), c.key.mtime_sec, c.key.mtime_nsec, UF_COMPRESSED) =>
            {
                (f, m)
            }
            other => {
                let (level, why) = match other {
                    Ok(_) => (Level::Debug, "no longer the swapped-in copy".to_owned()),
                    Err(e) => {
                        // NotFound, PermissionDenied, or ELOOP (a symlink now).
                        let expected = matches!(
                            e.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                        ) || e.raw_os_error() == Some(libc::ELOOP);
                        let level = if expected { Level::Debug } else { Level::Warn };
                        (level, format!("cannot be opened ({e})"))
                    }
                };
                let line = format!(
                    "{}: {why} right after the swap, not re-checked; the after-size is from the pre-swap stat",
                    c.path.display()
                );
                if level == Level::Warn {
                    self.log.warn(format_args!("{line}"));
                } else {
                    self.log.debug(format_args!("{line}"));
                }
                return None;
            }
        };
        let staging = staged.parent().unwrap_or_else(|| Path::new("/"));
        if let Err(e) = mark_keep(staging) {
            self.log.warn(format_args!(
                "cannot create {} in {}: {e}",
                KEEP_MARKER,
                staging.display()
            ));
        }
        let compare = |bufs: &mut Buffers| -> io::Result<Verify> {
            #[cfg(test)]
            if let Some(seam) = self.recheck {
                return seam(&c.path);
            }
            sys::evict_cached(&cur, c.size()).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("cannot evict cached pages of the copy: {e}"),
                )
            })?;
            let orig = File::options()
                .read(true)
                .custom_flags(O_NOFOLLOW)
                .open(staged)
                .map_err(|e| {
                    io::Error::new(e.kind(), format!("cannot reopen original in staging: {e}"))
                })?;
            if FileKey::of(&orig.metadata()?) != c.key {
                return Ok(Verify::Changed);
            }
            let (a, b) = bufs.get();
            same_bytes(&orig, &cur, a, b)
        };
        let checked = compare(bufs);
        let what = match checked {
            Ok(Verify::Same) => {
                fs::remove_dir(staging.join(KEEP_MARKER)).ok();
                return None;
            }
            Ok(Verify::Changed) => "original in staging changed".to_owned(),
            Ok(Verify::Mismatch) => "file at the path differs from the original".to_owned(),
            Err(e) => e.to_string(),
        };
        if let Err(e) = swap(staged, &c.path) {
            return Some(self.rescue(
                c,
                staged,
                &format!("verify failed after swap ({what}) and swap back failed ({e})"),
                PRESERVED_PREVIOUS,
            ));
        }
        // The copy must be back in staging untouched and the walked original at the path.
        let restored = cur
            .metadata()
            .is_ok_and(|m| identity(&m) == identity(&seen))
            && fs::symlink_metadata(staged).is_ok_and(|m| m.ino() == seen.ino())
            && fs::symlink_metadata(&c.path).is_ok_and(|m| FileKey::of(&m) == c.key);
        let original_back =
            || fs::symlink_metadata(&c.path).is_ok_and(|m| FileKey::of(&m) == c.key);
        if !restored {
            return Some(self.rescue(
                c,
                staged,
                &format!(
                    "verify failed after swap ({what}) and the files changed before the swap back"
                ),
                if original_back() {
                    PRESERVED_CHANGED
                } else {
                    PRESERVED_UNKNOWN
                },
            ));
        }
        if let Err(e) = fs::remove_file(staged) {
            return Some(self.rescue(
                c,
                staged,
                &format!("verify failed after swap ({what}) and the copy cannot be removed ({e})"),
                if original_back() {
                    PRESERVED_COPY
                } else {
                    PRESERVED_UNKNOWN
                },
            ));
        }
        fs::remove_dir(staging.join(KEEP_MARKER)).ok();
        Some(self.fail(
            c,
            "verify",
            format_args!("verify failed after swap: {what}; original restored"),
        ))
    }

    /// The file that was at the path now sits in staging and must survive: mark the staging
    /// dir with the [`KEEP_MARKER`] directory first (so stale cleanup never removes it), then
    /// move it to a rescue dir that `compress` never cleans, walks or removes (only
    /// `clean --apply` deletes it). `why` starts the ERROR line; `held` (a `PRESERVED_*`
    /// text with `{}` for the place) says what sits in staging.
    fn rescue(&self, c: &Candidate, staged: &Path, why: &str, held: Preserved) -> Commit {
        let staging = staged.parent().unwrap_or_else(|| Path::new("/"));
        if let Err(e) = mark_keep(staging) {
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
                    format_args!("{why}; {}", held(&rescue.join(base).display().to_string())),
                );
                Commit::Rescued(rescue)
            }
            Err(e) => {
                self.fail(
                    c,
                    "swap",
                    format_args!(
                        "{why}; {} (cannot move it to {}: {e})",
                        held(&staged.display().to_string()),
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

/// Result of [`xattr_diff`].
#[derive(Debug, PartialEq, Eq)]
enum XattrCheck {
    Same,
    /// The path no longer holds the walked inode.
    Changed,
    /// The first difference.
    Differs(String),
}

/// Compares the xattrs of the original (opened without following a symlink, must still be
/// `key`) and the staged copy, as a plain listing shows them: the name sets must be equal and
/// every value too, except [`applesauce::XATTR_NOT_COPIED`] (ignored, kernel-assigned per
/// creating process) and [`applesauce::XATTR_VALUE_REWRITTEN`], of which only the flags field
/// (before the first `;`) is compared (copyfile rewrites time and agent, as ditto does).
fn xattr_diff(orig: &Path, key: &FileKey, staged: &Path) -> io::Result<XattrCheck> {
    let open = |p: &Path| File::options().read(true).custom_flags(O_NOFOLLOW).open(p);
    let o = match open(orig) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(XattrCheck::Changed),
        Err(e) => return Err(e),
    };
    if FileKey::of(&o.metadata()?) != *key {
        return Ok(XattrCheck::Changed);
    }
    let s = open(staged)?;
    let names = |f: &File| -> io::Result<Vec<std::ffi::CString>> {
        let mut v = sys::flist_xattr(f)?;
        v.retain(|n| n.as_c_str() != applesauce::XATTR_NOT_COPIED);
        v.sort();
        Ok(v)
    };
    let (on, sn) = (names(&o)?, names(&s)?);
    if on != sn {
        return Ok(XattrCheck::Differs("xattr names".to_owned()));
    }
    for n in &on {
        let (mut a, mut b) = (
            sys::fget_xattr(&o, n, false)?,
            sys::fget_xattr(&s, n, false)?,
        );
        if n.as_c_str() == applesauce::XATTR_VALUE_REWRITTEN {
            let flags = |v: &mut Option<Vec<u8>>| {
                if let Some(v) = v {
                    v.truncate(v.iter().position(|&b| b == b';').unwrap_or(v.len()));
                }
            };
            flags(&mut a);
            flags(&mut b);
        }
        if a != b {
            return Ok(XattrCheck::Differs(format!(
                "xattr {}",
                n.to_string_lossy()
            )));
        }
    }
    Ok(XattrCheck::Same)
}

/// Opens the original once (no symlink follow), checks it is still the walked inode and
/// byte-compares it with the staged copy.
pub(crate) fn verify(c: &Candidate, staged: &Path, bufs: &mut Buffers) -> io::Result<Verify> {
    let orig = match File::options()
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
    let copy = File::open(staged)?;
    let (a, b) = bufs.get();
    same_bytes(&orig, &copy, a, b)
}

/// Byte-compares `orig` and `copy` from their current offsets to EOF in chunks of `a.len()`.
pub(crate) fn same_bytes(
    orig: &File,
    copy: &File,
    a: &mut [u8],
    b: &mut [u8],
) -> io::Result<Verify> {
    loop {
        let na = read_full(orig, a)?;
        let nb = read_full(copy, b)?;
        if na != nb || a[..na] != b[..nb] {
            return Ok(Verify::Mismatch);
        }
        if na == 0 {
            return Ok(Verify::Same);
        }
    }
}

/// decmpfs types the independent decoder reads: LZVN and LZFSE, inline or in the resource fork.
const DECODABLE_TYPES: [u32; 4] = [7, 8, 11, 12];

/// Whether the independent decoder reads decmpfs type `t`.
pub(crate) fn is_decodable(t: u32) -> bool {
    DECODABLE_TYPES.contains(&t)
}

/// The decmpfs compression type of `f` (an error when it has no decmpfs xattr).
pub(crate) fn decmpfs_type(f: &File) -> io::Result<u32> {
    let header = sys::fget_xattr(f, sys::DECMPFS, true)?
        .ok_or_else(|| invalid("no decmpfs xattr".to_owned()))?;
    Ok(decmpfs::Value::from_data(&header)?
        .compression_type
        .raw_type())
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// The decmpfs data stored in a compressed copy, decoded block by block in-process
/// (applesauce-core), without the kernel's decompression.
struct StoredData<'f> {
    reader: Reader<sys::RforkReader<'f>>,
    kind: Kind,
    /// Uncompressed size from the decmpfs header (checked equal to the original's).
    size: u64,
    /// Bytes decoded so far.
    total: u64,
    /// Valid bytes of the current block and how many of them were consumed.
    block_len: usize,
    block_pos: usize,
}

impl<'f> StoredData<'f> {
    /// Reads and validates the decmpfs header of `copy`: a decodable type (else an error,
    /// fail closed) and an uncompressed size of `size`.
    fn open(copy: &'f File, size: u64) -> io::Result<Self> {
        let header = sys::fget_xattr(copy, sys::DECMPFS, true)?
            .ok_or_else(|| invalid("no decmpfs xattr".to_owned()))?;
        let value = decmpfs::Value::from_data(&header)?;
        let t = value.compression_type.raw_type();
        if !DECODABLE_TYPES.contains(&t) {
            return Err(invalid(format!(
                "compression type {t} cannot be decoded independently"
            )));
        }
        if value.uncompressed_size != size {
            return Err(invalid(format!(
                "decmpfs size {} differs from the file size {size}",
                value.uncompressed_size
            )));
        }
        // Inline data (types 7, 11) never touches the resource fork.
        let rfork = if matches!(t, 7 | 11) {
            sys::RforkReader::empty(copy)
        } else {
            sys::RforkReader::new(copy)?
        };
        let reader = Reader::new(&header, || rfork)?;
        Ok(Self {
            kind: reader.compression_kind(),
            reader,
            size,
            total: 0,
            block_len: 0,
            block_pos: 0,
        })
    }

    /// Decodes the next block into `d.block`; every block but the last must hold exactly
    /// [`BLOCK_SIZE`] bytes. `false` once `size` bytes were decoded and no block is left.
    fn next_block(&mut self, d: &mut Decode) -> io::Result<bool> {
        if self.total == self.size {
            if self.reader.remaining_blocks() != 0 {
                return Err(invalid("stored data has blocks past its size".to_owned()));
            }
            return Ok(false);
        }
        d.compressed.clear();
        if !self.reader.read_block_into(&mut d.compressed)? {
            return Err(invalid(format!(
                "stored data ends after {} of {} bytes",
                self.total, self.size
            )));
        }
        let slot = match self.kind {
            Kind::Lzfse => &mut d.lzfse,
            _ => &mut d.lzvn,
        };
        let comp = match slot {
            Some(c) => c,
            None => slot.insert(
                self.kind
                    .compressor()
                    .ok_or_else(|| invalid(format!("no {} decoder built in", self.kind)))?,
            ),
        };
        let n = comp.decompress(&mut d.block, &d.compressed)?;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "at most BLOCK_SIZE after `min`"
        )]
        let want = (self.size - self.total).min(BLOCK_SIZE as u64) as usize;
        if n != want {
            return Err(invalid(format!(
                "block at {} decodes to {n} bytes, expected {want}",
                self.total
            )));
        }
        self.total += n as u64;
        self.block_len = n;
        self.block_pos = 0;
        Ok(true)
    }

    /// Fills `out` with the next decoded bytes; fewer only at the end of the data.
    fn read_into(&mut self, out: &mut [u8], d: &mut Decode) -> io::Result<usize> {
        let mut n = 0;
        while n < out.len() {
            if self.block_pos == self.block_len && !self.next_block(d)? {
                break;
            }
            let k = (out.len() - n).min(self.block_len - self.block_pos);
            out[n..n + k].copy_from_slice(&d.block[self.block_pos..self.block_pos + k]);
            self.block_pos += k;
            n += k;
        }
        Ok(n)
    }
}

/// [`verify`] plus an independent decode: in one pass, every chunk of the original must equal
/// both the kernel's read of the copy and the copy's decmpfs data decoded in-process. Stored
/// data that cannot be decoded (unknown type, bad block table, wrong block size) is an error.
fn verify_extended(c: &Candidate, staged: &Path, bufs: &mut Buffers) -> io::Result<Verify> {
    let orig = match File::options()
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
    let copy = File::open(staged)?;
    same_decoded(&orig, &copy, c.size(), bufs)
}

/// In one pass from their current offsets, every chunk of `orig` must equal both the kernel's
/// read of `copy` and `copy`'s decmpfs data (uncompressed size `size`) decoded in-process.
/// Stored data that cannot be decoded is an error.
pub(crate) fn same_decoded(
    orig: &File,
    copy: &File,
    size: u64,
    bufs: &mut Buffers,
) -> io::Result<Verify> {
    let mut stored = StoredData::open(copy, size)?;
    let (a, b, d) = bufs.get_extended();
    let mut chunk = std::mem::take(&mut d.chunk);
    let r = (|| loop {
        let na = read_full(orig, a)?;
        let nb = read_full(copy, b)?;
        if na != nb || a[..na] != b[..nb] {
            return Ok(Verify::Mismatch);
        }
        let nd = stored.read_into(&mut chunk[..na], d)?;
        if nd != na || a[..na] != chunk[..nd] {
            return Ok(Verify::Mismatch);
        }
        if na == 0 {
            // The decoded data must end exactly here too.
            return if stored.next_block(d)? {
                Ok(Verify::Mismatch)
            } else {
                Ok(Verify::Same)
            };
        }
    })();
    d.chunk = chunk;
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acz-cmp-{tag}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The ditto options of this system, probed once per test process.
    fn ditto_args() -> DittoArgs {
        static ARGS: std::sync::OnceLock<DittoArgs> = std::sync::OnceLock::new();
        ARGS.get_or_init(|| probe(&temp_dir("probe")).unwrap())
    }

    #[test]
    fn probe_finds_compressing_args_and_restores_its_source() {
        let dir = temp_dir("probe-restore");
        let src = dir.join(PROBE_NAME);
        let copy = dir.join(PROBE_OUT).join(PROBE_NAME);
        let args = probe(&dir).unwrap();
        assert!(DITTO_ARGS.contains(&args));
        assert_eq!(flags(&src) & UF_COMPRESSED, 0);
        assert_ne!(flags(&copy) & UF_COMPRESSED, 0);
        assert_eq!(fs::read(&copy).unwrap(), probe_content());
        // A changed source is rewritten.
        fs::write(&src, "changed").unwrap();
        assert_eq!(probe(&dir), Ok(args));
        assert_eq!(fs::read(&src).unwrap(), probe_content());
        // A compressed source would only be cloned or copied as is: it is rewritten too.
        fs::rename(&copy, &src).unwrap();
        assert_ne!(flags(&src) & UF_COMPRESSED, 0);
        assert_eq!(probe(&dir), Ok(args));
        assert_eq!(flags(&src) & UF_COMPRESSED, 0);
        assert_ne!(flags(&copy) & UF_COMPRESSED, 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn probe_without_compressing_args_fails() {
        let dir = temp_dir("probe-none");
        // Plain ditto copies (or clones) an uncompressed source uncompressed on every macOS.
        let r = probe_with(&dir, &[&[], &["--nohfsCompression"]]);
        let Err(ProbeError::NoCompression(e)) = r else {
            panic!("{r:?}")
        };
        assert!(
            e.starts_with(": copy is not compressed; --nohfsCompression: "),
            "{e}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn probe_in_unwritable_dir_fails() {
        let dir = temp_dir("probe-ro");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let r = probe(&dir);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(r, Err(ProbeError::Setup(e)) if e.starts_with("cannot write")));
        fs::remove_dir_all(&dir).unwrap();
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
        let run = run_ditto(ditto_args(), &refs, &dir, Duration::from_secs(120));
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
        let run = run_ditto(ditto_args(), &[&path], &staging, Duration::from_secs(120));
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
        #[expect(
            clippy::unused_self,
            reason = "test fixture method, called like the other fixture accessors"
        )]
        fn backend(&self) -> Backend {
            Backend::Ditto(ditto_args())
        }

        fn compressor(&self) -> Compressor<'_> {
            Compressor::new(&self.log, VerifyMode::Compare, 10, 1, 0)
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
            f.backend(),
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
            Command::new("/usr/bin/chflags")
                .arg("nouchg")
                .arg(&self.0)
                .status()
                .ok();
        }
    }

    #[test]
    fn file_flagged_during_run_is_skipped_not_blamed_on_the_dir() {
        let f = fixture("flagrace");
        let unflag = Unflag(f.cand.path.clone());
        let ok = Command::new("/usr/bin/chflags")
            .arg("uchg")
            .arg(&f.cand.path)
            .status()
            .unwrap();
        assert!(ok.success());
        let cmp = f.compressor();
        let r = cmp.commit(
            &f.cand,
            f.backend(),
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
        drop(unflag);
        fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn replaced_inode_before_commit_is_skipped() {
        let f = fixture("replaced");
        f.replace_original();
        let r = f.compressor().commit(
            &f.cand,
            f.backend(),
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
        let r = f.compressor().commit(
            &f.cand,
            f.backend(),
            &f.staged,
            false,
            &mut Buffers::default(),
            &swap,
        );
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
        let r = f.compressor().commit(
            &f.cand,
            f.backend(),
            &f.staged,
            false,
            &mut Buffers::default(),
            &swap,
        );
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
        assert!(run_ditto(ditto_args(), &[&src], &staging2, Duration::from_secs(120)).success());
        fs::rename(staging2.join("data.txt"), &f.staged).unwrap();
        let mut cmp = f.compressor();
        cmp.verify = VerifyMode::Off;
        let r = cmp.commit(
            &f.cand,
            f.backend(),
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
        assert!(run_ditto(ditto_args(), &[&src], dest_dir, Duration::from_secs(120)).success());
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

        // A crash while applesauce wrote a copy: the part dir is ours.
        let part = dir.join(".acz-staging-1-7");
        fs::create_dir_all(part.join(PART_DIR)).unwrap();
        fs::write(part.join(PART_DIR).join("half.txt"), "").unwrap();
        compressed_copy(&dir, &part, "done.txt");
        // A user FILE named like the part dir is a swapped-out original: moved, not deleted.
        let named = dir.join(".acz-staging-1-8");
        fs::create_dir(&named).unwrap();
        fs::write(named.join(PART_DIR), "user file").unwrap();

        let all = [
            &stale, &empty, &other, &kept, &crashed, &fake, &part, &named,
        ]
        .map(Clone::clone);
        remove_stale(&all, &log);
        assert!(!stale.exists() && !empty.exists() && !part.exists());
        assert_eq!(
            fs::read_to_string(dir.join(".acz-rescue-1-8").join(PART_DIR)).unwrap(),
            "user file"
        );
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
        let out = Compressor::new(&log, VerifyMode::Compare, 100, 2, u64::MAX / 2)
            .run(files.clone(), Backend::Ditto(ditto_args()));
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
    fn batches_never_mix_backends() {
        let dir = temp_dir("routed");
        let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
        let files = files_in(&dir, "a", 4);
        let content: Vec<Vec<u8>> = files.iter().map(|c| fs::read(&c.path).unwrap()).collect();
        let ditto = Backend::Ditto(ditto_args());
        // One match root, one device: only the backend splits them.
        let routed: Vec<(Candidate, Backend)> = files
            .iter()
            .enumerate()
            .map(|(i, c)| {
                (
                    c.clone(),
                    if i % 2 == 0 {
                        ditto
                    } else {
                        Backend::Applesauce
                    },
                )
            })
            .collect();
        let batches = batches_of(routed.clone(), 100);
        assert_eq!(batches.len(), 2);
        for (backend, batch) in &batches {
            for c in batch {
                assert!(routed.iter().any(|(r, b)| r.path == c.path && b == backend));
            }
        }
        let out = quiet(&log).run_routed(routed);
        // Two ditto files in one call; the applesauce ones never reach ditto.
        assert_eq!((out.compressed, out.errors, out.ditto_calls), (4, 0, 1));
        for (c, want) in files.iter().zip(&content) {
            let m = fs::symlink_metadata(&c.path).unwrap();
            assert_ne!(m.st_flags() & UF_COMPRESSED, 0);
            assert_eq!(&fs::read(&c.path).unwrap(), want);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn applesauce_write_error_fails_the_file_without_swap() {
        let dir = temp_dir("as-error");
        let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
        let files = files_in(&dir, "a", 2);
        let before: Vec<u64> = files.iter().map(|c| c.key.ino).collect();
        // A file where the part dir belongs: every stage returns an io error.
        let staging = dir.join("root/staging");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join(PART_DIR), "").unwrap();
        let refs: Vec<&Candidate> = files.iter().collect();
        let mut out = Outcome::default();
        quiet(&log).process(
            &staging,
            &refs,
            Backend::Applesauce,
            &mut Buffers::default(),
            &mut out,
        );
        assert_eq!((out.compressed, out.errors), (0, 2));
        assert!(log_text(&dir).contains("cannot write compressed copy"));
        for (c, ino) in files.iter().zip(before) {
            let m = fs::symlink_metadata(&c.path).unwrap();
            assert_eq!(m.ino(), ino);
            assert_eq!(m.st_flags() & UF_COMPRESSED, 0);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn applesauce_backend_compresses_and_verifies() {
        let dir = temp_dir("applesauce");
        let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
        let files = files_in(&dir, "a", 3);
        let content: Vec<Vec<u8>> = files.iter().map(|c| fs::read(&c.path).unwrap()).collect();
        // Verify off in the config: applesauce copies are byte-compared anyway.
        let out = Compressor::new(&log, VerifyMode::Off, 100, 2, 0)
            .run(files.clone(), Backend::Applesauce);
        assert_eq!((out.compressed, out.errors, out.ditto_calls), (3, 0, 0));
        for (c, want) in files.iter().zip(&content) {
            let m = fs::symlink_metadata(&c.path).unwrap();
            assert_ne!(m.st_flags() & UF_COMPRESSED, 0);
            assert_eq!(
                (m.size(), m.mtime(), m.mtime_nsec()),
                (c.size(), c.key.mtime_sec, c.key.mtime_nsec)
            );
            assert_eq!(&fs::read(&c.path).unwrap(), want);
        }
        assert!(fs::read_dir(dir.join("root")).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(STAGING_PREFIX)
        }));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn xattr_diff_flags_lost_or_changed_xattrs() {
        let dir = temp_dir("xattr-diff");
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();
        let key = FileKey::of(&fs::symlink_metadata(&a).unwrap());
        let set = |p: &Path, n: &std::ffi::CStr, v: &[u8]| {
            sys::fset_xattr(&File::open(p).unwrap(), n, v, 0).unwrap();
        };
        let diff = || xattr_diff(&a, &key, &b).unwrap();
        let differs = |s: &str| XattrCheck::Differs(s.to_owned());
        assert_eq!(diff(), XattrCheck::Same);
        set(&a, c"user.x", b"1");
        assert_eq!(diff(), differs("xattr names"));
        set(&b, c"user.x", b"2");
        assert_eq!(diff(), differs("xattr user.x"));
        set(&b, c"user.x", b"1");
        // Quarantine: presence and flags count, time and agent are rewritten by copyfile.
        set(
            &a,
            applesauce::XATTR_VALUE_REWRITTEN,
            b"0081;5f000000;Safari;",
        );
        assert_eq!(diff(), differs("xattr names"));
        set(&b, applesauce::XATTR_VALUE_REWRITTEN, b"0082;6a000000;;");
        assert_eq!(diff(), differs("xattr com.apple.quarantine"));
        set(&b, applesauce::XATTR_VALUE_REWRITTEN, b"0081;6a000000;;");
        assert_eq!(diff(), XattrCheck::Same);
        // Another inode at the original's path: changed, never compared.
        assert_eq!(xattr_diff(&b, &key, &a).unwrap(), XattrCheck::Changed);
        fs::remove_file(&a).unwrap();
        assert_eq!(diff(), XattrCheck::Changed);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn applesauce_copy_is_verified_even_with_verify_off() {
        let dir = temp_dir("applesauce-verify");
        let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
        let files = files_in(&dir, "v", 1);
        let want = fs::read(&files[0].path).unwrap();
        let mut comp = Compressor::new(&log, VerifyMode::Off, 100, 1, 0);
        // Same size and mtime, other bytes: only a byte compare can tell.
        comp.after_stage = Some(|c| {
            let mut data = fs::read(&c.path).unwrap();
            data[0] ^= 1;
            fs::write(&c.path, data).unwrap();
            let t = system_time(c.key.mtime_sec, c.key.mtime_nsec);
            File::options()
                .write(true)
                .open(&c.path)
                .unwrap()
                .set_modified(t)
                .unwrap();
        });
        let out = comp.run(files.clone(), Backend::Applesauce);
        assert_eq!((out.compressed, out.errors), (0, 1));
        let p = &files[0].path;
        assert_eq!(
            fs::symlink_metadata(p).unwrap().st_flags() & UF_COMPRESSED,
            0
        );
        assert_ne!(
            fs::read(p).unwrap(),
            want,
            "the original stays, as modified"
        );
        assert!(
            log_text(&dir).contains("verify failed"),
            "{}",
            log_text(&dir)
        );
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
        let out = Compressor::new(&log, VerifyMode::Compare, 100, 1, 0)
            .run(files.clone(), Backend::Ditto(ditto_args()));
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
        Compressor::new(log, VerifyMode::Off, 100, 2, 0)
    }

    #[test]
    fn case_colliding_names_in_one_staging_keep_their_bytes() {
        let dir = temp_dir("case-process");
        let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
        let (a, b) = case_pair(&dir);
        let staging = Compressor::make_staging(&a.match_root, 0).unwrap();
        let mut out = Outcome::default();
        quiet(&log).process(
            &staging,
            &[&a, &b],
            Backend::Ditto(ditto_args()),
            &mut Buffers::default(),
            &mut out,
        );
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
        let out = quiet(&log).run(vec![a.clone(), b.clone()], Backend::Ditto(ditto_args()));
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
        let out = quiet(&log).run(files, Backend::Ditto(ditto_args()));
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

    /// Pseudo-random, incompressible bytes (xorshift).
    #[expect(
        clippy::cast_possible_truncation,
        reason = "test noise: keeps the low byte on purpose"
    )]
    fn noise(n: usize, mut x: u64) -> Vec<u8> {
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    /// An original `<dir>/root/<name>` with `data` and its ditto copy in `<dir>/copies/`.
    fn original_and_copy(dir: &Path, name: &str, data: &[u8]) -> (Candidate, PathBuf) {
        let root = dir.join("root");
        let copies = dir.join("copies");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&copies).unwrap();
        let path = root.join(name);
        fs::write(&path, data).unwrap();
        let m = fs::symlink_metadata(&path).unwrap();
        let c = Candidate::new(path.clone(), Arc::from(root.as_path()), &m);
        assert!(run_ditto(ditto_args(), &[&path], &copies, Duration::from_secs(120)).success());
        (c, copies.join(name))
    }

    /// A plain (not `UF_COMPRESSED`) file with `data` in its data fork and the given decmpfs
    /// xattr and resource fork: the kernel reads `data`, only the decoder sees the rest.
    fn forged(path: &Path, data: &[u8], header: &[u8], rfork: &[u8]) {
        fs::remove_file(path).ok();
        fs::write(path, data).unwrap();
        let f = File::options().read(true).write(true).open(path).unwrap();
        sys::fset_xattr(&f, sys::DECMPFS, header, 0).unwrap();
        if !rfork.is_empty() {
            io::Write::write_all(&mut sys::RforkWriter::new(&f), rfork).unwrap();
        }
        assert_eq!(flags(path) & UF_COMPRESSED, 0);
    }

    fn stored(copy: &Path) -> (Vec<u8>, Vec<u8>) {
        let f = File::open(copy).unwrap();
        let header = sys::fget_xattr(&f, sys::DECMPFS, true).unwrap().unwrap();
        let rfork = sys::fget_xattr(&f, sys::RESOURCE_FORK, true)
            .unwrap()
            .unwrap_or_default();
        (header, rfork)
    }

    fn extended(c: &Candidate, copy: &Path) -> io::Result<Verify> {
        verify_extended(c, copy, &mut Buffers::default())
    }

    #[test]
    fn extended_verify_decodes_ditto_copies_of_every_shape() {
        let dir = temp_dir("ext-shapes");
        let text = |n: usize| b"decode me independently\n".repeat(n / 24 + 1)[..n].to_vec();
        let mut three = text(BLOCK_SIZE);
        three.extend(noise(BLOCK_SIZE, 7)); // stored raw (0x06 prefix)
        three.extend(text(1000));
        let mut sparse = vec![0u8; 3 * BLOCK_SIZE + 5];
        sparse[BLOCK_SIZE + 3..BLOCK_SIZE + 10].copy_from_slice(b"islands");
        let shapes = [
            ("a16385", text(16385)),
            ("a65536", text(65536)),
            ("a65537", text(65537)),
            ("three", three),
            ("big", text(VERIFY_BUF + 3 * BLOCK_SIZE + 17)),
        ];
        for (name, data) in &shapes {
            let (c, copy) = original_and_copy(&dir, name, data);
            assert_ne!(flags(&copy) & UF_COMPRESSED, 0, "{name}");
            assert!(matches!(extended(&c, &copy), Ok(Verify::Same)), "{name}");
        }
        // Sparse source and a user xattr.
        let root = dir.join("root");
        let p = root.join("sparse");
        let f = File::create(&p).unwrap();
        f.set_len(sparse.len() as u64).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&f, b"islands", BLOCK_SIZE as u64 + 3).unwrap();
        sys::fset_xattr(&f, c"user.acz", b"v", 0).unwrap();
        drop(f);
        let m = fs::symlink_metadata(&p).unwrap();
        let c = Candidate::new(p.clone(), Arc::from(root.as_path()), &m);
        let copies = dir.join("copies");
        assert!(run_ditto(ditto_args(), &[&p], &copies, Duration::from_secs(120)).success());
        assert_eq!(fs::read(&p).unwrap(), sparse);
        assert!(matches!(
            extended(&c, &copies.join("sparse")),
            Ok(Verify::Same)
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[expect(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        reason = "test: a few-KiB length plus or minus one"
    )]
    fn extended_verify_rejects_tampered_stored_data() {
        let dir = temp_dir("ext-tamper");
        let mut data = b"tamper with me\n".repeat(BLOCK_SIZE / 15 + 1)[..BLOCK_SIZE].to_vec();
        data.extend(noise(BLOCK_SIZE, 3));
        data.extend(b"tail".repeat(300));
        let (c, copy) = original_and_copy(&dir, "data", &data);
        let (header, rfork) = stored(&copy);
        let forged_at = dir.join("forged");
        let check = |header: &[u8], rfork: &[u8]| {
            forged(&forged_at, &data, header, rfork);
            extended(&c, &forged_at)
        };
        // The untouched stored data passes through a forged file too.
        assert!(matches!(check(&header, &rfork), Ok(Verify::Same)));
        // A flipped byte in the last (compressed) block.
        let mut flip = rfork.clone();
        let at = flip.len() - 10;
        flip[at] ^= 0x55;
        assert!(!matches!(check(&header, &flip), Ok(Verify::Same)));
        // A flipped byte in the raw middle block: decodes, but to other bytes.
        let mut raw = rfork.clone();
        let mid = rfork.len() / 2;
        raw[mid] ^= 0x01;
        assert!(matches!(check(&header, &raw), Ok(Verify::Mismatch)));
        for delta in [1i64, -1] {
            let mut h = header.clone();
            let size = (data.len() as i64 + delta) as u64;
            h[8..16].copy_from_slice(&size.to_le_bytes());
            assert!(check(&h, &rfork).is_err(), "size {delta:+}");
        }
        let mut zlib = header.clone();
        zlib[4..8].copy_from_slice(&4u32.to_le_bytes());
        let e = check(&zlib, &rfork).err().unwrap();
        assert!(
            e.to_string()
                .contains("type 4 cannot be decoded independently"),
            "{e}"
        );
        assert!(
            check(&header, &rfork[..rfork.len() - 7]).is_err(),
            "truncated"
        );
        let mut shifted = rfork;
        let first = u32::from_le_bytes(shifted[..4].try_into().unwrap()) + 1;
        shifted[..4].copy_from_slice(&first.to_le_bytes());
        assert!(check(&header, &shifted).is_err(), "shifted block table");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn extended_verify_rejects_tampered_inline_data() {
        let dir = temp_dir("ext-inline");
        let root = dir.join("root");
        fs::create_dir(&root).unwrap();
        let path = root.join("small.txt");
        let data = b"inline\n".repeat(4000);
        fs::write(&path, &data).unwrap();
        let m = fs::symlink_metadata(&path).unwrap();
        let c = Candidate::new(path, Arc::from(root.as_path()), &m);
        let staging = Compressor::make_staging(&root, 0).unwrap();
        let Ok(Staged::Copy(copy)) =
            applesauce::stage(&c, &staging, &mut applesauce::Buffers::default())
        else {
            panic!("not staged")
        };
        let (header, rfork) = stored(&copy);
        assert_eq!(u32::from_le_bytes(header[4..8].try_into().unwrap()), 11);
        assert!(rfork.is_empty());
        assert!(matches!(extended(&c, &copy), Ok(Verify::Same)));
        let forged_at = dir.join("forged");
        let mut flip = header;
        let at = flip.len() - 3;
        flip[at] ^= 0x10;
        forged(&forged_at, &data, &flip, &[]);
        assert!(!matches!(extended(&c, &forged_at), Ok(Verify::Same)));
        fs::remove_dir_all(&dir).unwrap();
    }

    fn extended_commit(
        f: &Fixture,
        cmp: &Compressor<'_>,
        swap: &dyn Fn(&Path, &Path) -> io::Result<()>,
    ) -> Commit {
        cmp.commit(
            &f.cand,
            f.backend(),
            &f.staged,
            false,
            &mut Buffers::default(),
            swap,
        )
    }

    fn extended_compressor(f: &Fixture) -> Compressor<'_> {
        let mut cmp = f.compressor();
        cmp.verify = VerifyMode::Extended;
        cmp
    }

    const FIXTURE_TEXT: &str = "compress me please\n";

    #[test]
    fn extended_commit_of_ditto_and_applesauce_copies() {
        let f = fixture("ext-ok");
        let r = extended_commit(&f, &extended_compressor(&f), &sys::rename_swap);
        assert!(matches!(r, Commit::Compressed { .. }), "{r:?}");
        assert_eq!(
            fs::read_to_string(&f.cand.path).unwrap(),
            FIXTURE_TEXT.repeat(20_000)
        );
        assert!(!f.staged.parent().unwrap().join(KEEP_MARKER).exists());
        fs::remove_dir_all(&f.dir).unwrap();

        let dir = temp_dir("ext-applesauce");
        let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
        let files = files_in(&dir, "a", 2);
        let out =
            Compressor::new(&log, VerifyMode::Extended, 100, 1, 0).run(files, Backend::Applesauce);
        assert_eq!((out.compressed, out.errors), (2, 0));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn extended_verify_fails_a_differing_forced_copy() {
        let f = fixture("ext-forced");
        let other = f.dir.join("other");
        fs::create_dir(&other).unwrap();
        let src = other.join("data.txt");
        fs::write(&src, "compress me PLEASE\n".repeat(20_000)).unwrap();
        let staging2 = f.dir.join("staging2");
        fs::create_dir(&staging2).unwrap();
        assert!(run_ditto(ditto_args(), &[&src], &staging2, Duration::from_secs(120)).success());
        fs::rename(staging2.join("data.txt"), &f.staged).unwrap();
        let r = extended_commit(&f, &extended_compressor(&f), &sys::rename_swap);
        assert_eq!(r, Commit::Failed);
        assert_eq!(flags(&f.cand.path) & UF_COMPRESSED, 0);
        assert_eq!(
            fs::read_to_string(&f.cand.path).unwrap(),
            FIXTURE_TEXT.repeat(20_000)
        );
        fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn post_swap_mismatch_swaps_the_original_back() {
        let f = fixture("ext-recheck");
        let mut cmp = extended_compressor(&f);
        // The keep marker protects the staging dir during the post-swap compare.
        cmp.recheck = Some(|p| {
            let marked = fs::read_dir(p.parent().unwrap())?.any(|e| {
                e.is_ok_and(|e| {
                    e.file_name().to_string_lossy().starts_with(STAGING_PREFIX)
                        && has_keep_marker(&e.path())
                })
            });
            Ok(if marked {
                Verify::Mismatch
            } else {
                Verify::Same
            })
        });
        let r = extended_commit(&f, &cmp, &sys::rename_swap);
        assert_eq!(r, Commit::Failed);
        assert!(!has_keep_marker(f.staged.parent().unwrap()));
        let m = fs::symlink_metadata(&f.cand.path).unwrap();
        assert_eq!(FileKey::of(&m), f.cand.key);
        assert_eq!(m.ino(), f.cand.key.ino);
        assert_eq!(m.st_flags() & UF_COMPRESSED, 0);
        assert!(!f.staged.exists(), "copy left in staging");
        f.log.flush();
        let log = log_text(&f.dir);
        assert!(
            log.contains(" ERROR ") && log.contains("verify failed after swap"),
            "{log}"
        );
        assert!(log.contains("original restored"), "{log}");
        fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn post_swap_mismatch_with_failed_swap_back_is_rescued() {
        let f = fixture("ext-recheck-rescue");
        let mut cmp = extended_compressor(&f);
        cmp.recheck = Some(|_| Err(io::Error::other("injected")));
        let calls = Cell::new(0);
        let swap = |a: &Path, b: &Path| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                sys::rename_swap(a, b)
            } else {
                Err(io::Error::other("injected swap-back failure"))
            }
        };
        let r = extended_commit(&f, &cmp, &swap);
        let Commit::Rescued(rescue) = r else {
            panic!("{r:?}")
        };
        assert!(rescue.join(KEEP_MARKER).exists());
        // The original survives in the rescue dir, the verified-at-first copy is at the path.
        let kept = fs::symlink_metadata(rescue.join("data.txt")).unwrap();
        assert_eq!(FileKey::of(&kept), f.cand.key);
        assert_eq!(
            fs::read_to_string(&f.cand.path).unwrap(),
            FIXTURE_TEXT.repeat(20_000)
        );
        fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn post_swap_mismatch_after_a_user_write_is_rescued() {
        let f = fixture("ext-recheck-write");
        let mut cmp = extended_compressor(&f);
        cmp.recheck = Some(|p| {
            let mut w = File::options().append(true).open(p)?;
            io::Write::write_all(&mut w, b"user")?;
            Ok(Verify::Mismatch)
        });
        let r = extended_commit(&f, &cmp, &sys::rename_swap);
        let Commit::Rescued(rescue) = r else {
            panic!("{r:?}")
        };
        let m = fs::symlink_metadata(&f.cand.path).unwrap();
        assert_eq!(FileKey::of(&m), f.cand.key);
        let modified = fs::read(rescue.join("data.txt")).unwrap();
        assert!(modified.ends_with(b"user"));
        f.log.flush();
        let log = log_text(&f.dir);
        assert!(
            log.contains("possibly with newer writes")
                && !log.contains("the compressed copy (not the original)"),
            "{log}"
        );
        fs::remove_dir_all(&f.dir).unwrap();
    }

    /// After the swap, changes the original (now at `a`) by `edit`, keeping its times.
    fn swap_then_edit_original(
        f: &Fixture,
        edit: fn(&Path),
    ) -> impl Fn(&Path, &Path) -> io::Result<()> + '_ {
        move |a: &Path, b: &Path| {
            sys::rename_swap(a, b)?;
            if fs::symlink_metadata(a).is_ok_and(|m| m.ino() == f.cand.key.ino) {
                edit(a);
                let times = FileTimes::new()
                    .set_accessed(system_time(f.cand.atime_sec, f.cand.atime_nsec))
                    .set_modified(system_time(f.cand.key.mtime_sec, f.cand.key.mtime_nsec));
                File::options()
                    .write(true)
                    .open(a)
                    .unwrap()
                    .set_times(times)
                    .unwrap();
            }
            Ok(())
        }
    }

    #[test]
    fn post_swap_real_compare_detects_a_changed_original() {
        let f = fixture("ext-real-flip");
        let swap = swap_then_edit_original(&f, |a| {
            let mut data = fs::read(a).unwrap();
            data[100] ^= 0x01;
            fs::write(a, data).unwrap();
        });
        let r = extended_commit(&f, &extended_compressor(&f), &swap);
        assert_eq!(r, Commit::Failed);
        let m = fs::symlink_metadata(&f.cand.path).unwrap();
        assert_eq!(m.ino(), f.cand.key.ino);
        assert!(!f.staged.exists(), "copy left in staging");
        f.log.flush();
        let log = log_text(&f.dir);
        assert!(
            log.contains("verify failed after swap") && log.contains("original restored"),
            "{log}"
        );
        fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn post_swap_real_compare_detects_a_grown_original() {
        let f = fixture("ext-real-grow");
        let swap = swap_then_edit_original(&f, |a| {
            let mut w = File::options().append(true).open(a).unwrap();
            io::Write::write_all(&mut w, b"x").unwrap();
        });
        let r = extended_commit(&f, &extended_compressor(&f), &swap);
        // A changed size is caught by the swapped-out check before the post-swap compare:
        // swapped back and skipped.
        assert_eq!(r, Commit::Skipped);
        let m = fs::symlink_metadata(&f.cand.path).unwrap();
        assert_eq!(m.ino(), f.cand.key.ino);
        assert_eq!(m.st_flags() & UF_COMPRESSED, 0);
        assert!(fs::read(&f.cand.path).unwrap().ends_with(b"x"));
        fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn write_right_after_the_swap_is_kept_without_swap_back() {
        let f = fixture("ext-after-write");
        let calls = Cell::new(0);
        let swap = |a: &Path, b: &Path| {
            calls.set(calls.get() + 1);
            sys::rename_swap(a, b)?;
            fs::write(b, "user wrote this").unwrap();
            Ok(())
        };
        let r = extended_commit(&f, &extended_compressor(&f), &swap);
        assert!(matches!(r, Commit::Compressed { .. }), "{r:?}");
        assert_eq!(calls.get(), 1, "swapped back");
        assert_eq!(fs::read_to_string(&f.cand.path).unwrap(), "user wrote this");
        // The DEBUG line is not written by a non-verbose logger; the outcome is what counts.
        fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn symlink_right_after_the_swap_is_not_swapped_back() {
        let f = fixture("ext-after-link");
        let calls = Cell::new(0);
        let target = f.dir.join("target");
        let swap = |a: &Path, b: &Path| {
            calls.set(calls.get() + 1);
            sys::rename_swap(a, b)?;
            fs::rename(b, &target).unwrap();
            std::os::unix::fs::symlink(&target, b).unwrap();
            Ok(())
        };
        let r = extended_commit(&f, &extended_compressor(&f), &swap);
        assert!(matches!(r, Commit::Compressed { .. }), "{r:?}");
        assert_eq!(calls.get(), 1, "swapped back");
        assert!(fs::symlink_metadata(&f.cand.path).unwrap().is_symlink());
        fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn extended_probe_decodes_the_ditto_probe_copy() {
        let dir = temp_dir("probe-decode");
        probe(&dir).unwrap();
        assert!(probe_content().len() > 12_000);
        assert_eq!(probe_decode(&dir), Ok(()));
        // A copy without decmpfs data gets its own error.
        let copy = dir.join(PROBE_OUT).join(PROBE_NAME);
        fs::remove_file(&copy).unwrap();
        fs::write(&copy, probe_content()).unwrap();
        let e = probe_decode(&dir).unwrap_err().to_string();
        assert!(e.contains("the ditto copy has no decmpfs xattr"), "{e}");
        fs::remove_file(&copy).unwrap();
        assert!(probe_decode(&dir).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
