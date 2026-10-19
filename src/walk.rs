//! Parallel, symlink-free directory walk that turns glob patterns into candidate files.
//!
//! Every walk root passes the [`Guard`] before its first `read_dir`: only the internal APFS data
//! volume (plus `allow_volumes`) is walked, cloud/volume/system subtrees and privacy-protected
//! (TCC) folders are never entered unless a root lies inside them. Excludes win over patterns
//! for files and directories in both walk modes.

use crate::logger::{Level, Logger};
use crate::progress::FileEvent;
use crate::sys::{self, FsInfo, MNT_LOCAL, PROTECTED_FLAGS, SF_DATALESS};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::io;
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

/// `EDEADLK`: returned instead of materializing a dataless file once that is turned off.
const EDEADLK: i32 = 11;

pub const STAGING_PREFIX: &str = ".acz-staging-";
pub const RESCUE_PREFIX: &str = ".acz-rescue-";

/// Identity of a file version: same key means same inode with unchanged size and mtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileKey {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
}

impl FileKey {
    pub fn of(m: &Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime_sec: m.mtime(),
            mtime_nsec: m.mtime_nsec(),
        }
    }
}

/// A regular file found below a pattern match, with the `lstat` data taken during the walk.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    /// The matched directory (or the parent of a matched file); staging happens there.
    pub match_root: Arc<Path>,
    pub key: FileKey,
    pub atime_sec: i64,
    pub atime_nsec: i64,
    pub blocks: u64,
    pub mode: u32,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub flags: u32,
}

impl Candidate {
    pub fn new(path: PathBuf, match_root: Arc<Path>, m: &Metadata) -> Self {
        Self {
            path,
            match_root,
            key: FileKey::of(m),
            atime_sec: m.atime(),
            atime_nsec: m.atime_nsec(),
            blocks: m.blocks(),
            mode: m.mode(),
            nlink: m.nlink(),
            uid: m.uid(),
            gid: m.gid(),
            flags: m.st_flags(),
        }
    }

    pub fn size(&self) -> u64 {
        self.key.size
    }

    /// Allocated bytes (`st_blocks * 512`).
    pub fn allocated(&self) -> u64 {
        self.blocks * 512
    }
}

/// Compiled patterns, excludes and the R1 "descend" set that bounds the search.
pub struct Matcher {
    patterns: GlobSet,
    /// Excludes without `/`: matched against the entry name.
    exclude_name: GlobSet,
    /// Excludes with `/`: matched against the full path.
    exclude_path: GlobSet,
    /// `X` for every path exclude `X/**`: also excludes the directory X itself.
    exclude_dir: GlobSet,
    /// Directories that may still lead to a pattern match.
    descend: GlobSet,
    /// Distinct walk roots (literal prefixes of the patterns, narrowed to the scope dir).
    roots: Vec<PathBuf>,
}

fn glob(s: &str) -> Result<Glob, String> {
    GlobBuilder::new(s)
        .literal_separator(true)
        .build()
        .map_err(|e| format!("bad glob `{s}`: {e}"))
}

fn has_meta(component: &str) -> bool {
    component.contains(['*', '?', '[', '{'])
}

/// Splits an absolute pattern into its components and the number of leading literal ones.
fn split_pattern(pattern: &str) -> (Vec<&str>, usize) {
    let comps: Vec<&str> = pattern.split('/').filter(|c| !c.is_empty()).collect();
    let prefix_len = comps.iter().take_while(|c| !has_meta(c)).count();
    (comps, prefix_len)
}

/// Walk root of an absolute pattern: its leading components without glob metacharacters.
pub fn literal_prefix(pattern: &str) -> PathBuf {
    let (comps, prefix_len) = split_pattern(pattern);
    let mut root = PathBuf::from("/");
    root.extend(&comps[..prefix_len]);
    root
}

fn slice_glob(comps: &[&str]) -> String {
    format!("/{}", comps.join("/"))
}

impl Matcher {
    /// `patterns` must be absolute (validated by the config). An exclude without `/` matches
    /// entry names at any depth, one with `/` the full path. With `scope` the walk is limited to
    /// that directory's subtree: a pattern root above it is replaced by `scope`, a root below it
    /// is kept, any other root dropped.
    pub fn new(
        patterns: &[String],
        exclude: &[String],
        scope: Option<&Path>,
    ) -> Result<Self, String> {
        let mut pat = GlobSetBuilder::new();
        let mut desc = GlobSetBuilder::new();
        let mut roots = Vec::new();
        for p in patterns {
            pat.add(glob(p)?);
            let (comps, prefix_len) = split_pattern(p);
            // Every proper prefix slice longer than the literal prefix; a slice ending in `**`
            // matches any deeper directory as well.
            for k in prefix_len + 1..comps.len() {
                desc.add(glob(&slice_glob(&comps[..k]))?);
            }
            let prefix = literal_prefix(p);
            let root = match scope {
                None => prefix,
                Some(s) if s.starts_with(&prefix) => s.to_path_buf(),
                Some(s) if prefix.starts_with(s) => prefix,
                Some(_) => continue,
            };
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        let (mut by_name, mut by_path, mut dir) = (
            GlobSetBuilder::new(),
            GlobSetBuilder::new(),
            GlobSetBuilder::new(),
        );
        for e in exclude {
            if !e.contains('/') {
                by_name.add(glob(e)?);
                continue;
            }
            by_path.add(glob(e)?);
            if let Some(x) = e.strip_suffix("/**").filter(|x| !x.is_empty()) {
                dir.add(glob(x)?);
            }
        }
        let build = |b: GlobSetBuilder| b.build().map_err(|e| e.to_string());
        Ok(Self {
            patterns: build(pat)?,
            exclude_name: build(by_name)?,
            exclude_path: build(by_path)?,
            exclude_dir: build(dir)?,
            descend: build(desc)?,
            roots,
        })
    }

    /// True when an exclude matches the entry (`path` as walked, `name` its last component).
    fn excluded(&self, path: &globset::Candidate<'_>, name: &OsStr, is_dir: bool) -> bool {
        self.exclude_name.is_match(name)
            || self.exclude_path.is_match_candidate(path)
            || (is_dir && self.exclude_dir.is_match_candidate(path))
    }
}

type Probe = dyn Fn(&Path) -> io::Result<FsInfo> + Sync;

/// Hard limits of the walk that no config can lift: the allowed filesystems, subtrees that are
/// never entered, and privacy-protected folders entered only by a root inside them.
pub struct Guard {
    /// `f_fsid` of the data volume and of the accepted `allow_volumes`.
    allowed: Vec<[i32; 2]>,
    /// Accepted `allow_volumes` mount points (exempt from the `/Volumes` exclusion).
    allowed_mounts: Vec<PathBuf>,
    hard_excluded: Vec<PathBuf>,
    tcc: Vec<PathBuf>,
    probe: Box<Probe>,
}

/// The volume that holds the user's data; its filesystem is the one always allowed.
const DATA_VOLUME: &str = "/System/Volumes/Data";

/// Flags that make an ancestor of a root refuse it: [`PROTECTED_FLAGS`] without `restricted`.
const ANCESTOR_FLAGS: u32 = PROTECTED_FLAGS & !sys::SF_RESTRICTED;

/// The nearest real (non-symlink) ancestor of `canon` with an [`ANCESTOR_FLAGS`] bit;
/// `lstat` gives (is symlink, st_flags) of a path.
fn flagged_ancestor_with(
    canon: &Path,
    lstat: impl Fn(&Path) -> Option<(bool, u32)>,
) -> Option<(PathBuf, u32)> {
    let canon = without_data_prefix(canon);
    canon.ancestors().skip(1).find_map(|a| {
        let (symlink, flags) = lstat(a)?;
        let flags = flags & ANCESTOR_FLAGS;
        (!symlink && flags != 0).then(|| (a.to_path_buf(), flags))
    })
}

/// `/System/Volumes/Data/x` -> `/x` (the same file through the firmlinked root), so data volume
/// paths are not mistaken for `/System` and meet the same protected paths.
fn without_data_prefix(p: &Path) -> Cow<'_, Path> {
    match p.strip_prefix(DATA_VOLUME) {
        Ok(rest) => Cow::Owned(Path::new("/").join(rest)),
        Err(_) => Cow::Borrowed(p),
    }
}

fn local_apfs(fs: &FsInfo) -> bool {
    fs.fstype == "apfs" && fs.flags & MNT_LOCAL != 0
}

impl Guard {
    /// The data volume plus every `allow_volumes` entry that is a local apfs mount point.
    pub fn new(home: &Path, allow_volumes: &[String], log: &Logger) -> Result<Self, String> {
        let data = sys::statfs(Path::new(DATA_VOLUME))
            .map_err(|e| format!("cannot statfs {DATA_VOLUME}: {e}"))?;
        if !local_apfs(&data) {
            return Err(format!("{DATA_VOLUME} is not a local apfs volume"));
        }
        let mut allowed = vec![data.fsid];
        let mut mounts = Vec::new();
        for v in allow_volumes {
            let v = Path::new(v);
            match sys::statfs(v) {
                Ok(fs) if local_apfs(&fs) && fs.mount_on == v => {
                    allowed.push(fs.fsid);
                    mounts.push(v.to_path_buf());
                }
                Ok(fs) => log.warn(format_args!(
                    "allow_volumes entry {} ignored: not the mount point of a local apfs volume \
                     (mounted on {}, type {})",
                    v.display(),
                    fs.mount_on.display(),
                    fs.fstype
                )),
                Err(e) => log.warn(format_args!(
                    "allow_volumes entry {} ignored: {e}",
                    v.display()
                )),
            }
        }
        Ok(Self::with_probe(
            home,
            allowed,
            mounts,
            Box::new(sys::statfs),
        ))
    }

    /// A guard with an injected filesystem probe (tests). Home-relative paths are listed for
    /// `home` as given and, when it differs, for its canonical form (symlink targets resolve to it).
    fn with_probe(
        home: &Path,
        allowed: Vec<[i32; 2]>,
        allowed_mounts: Vec<PathBuf>,
        probe: Box<Probe>,
    ) -> Self {
        let mut homes = vec![home.to_path_buf()];
        if let Ok(canon) = fs::canonicalize(home)
            && canon != home
        {
            homes.push(canon);
        }
        let below_homes = |rel: &[&str]| -> Vec<PathBuf> {
            homes
                .iter()
                .flat_map(|h| rel.iter().map(move |r| h.join(r)))
                .collect()
        };
        let mut hard_excluded = below_homes(&["Library/CloudStorage", "Library/Mobile Documents"]);
        hard_excluded
            .extend(["/Volumes", "/System", "/dev", "/Network", "/net"].map(PathBuf::from));
        let tcc = below_homes(&[
            "Desktop",
            "Documents",
            "Downloads",
            "Pictures",
            "Movies",
            "Music",
            "Library",
        ]);
        Self {
            allowed,
            allowed_mounts,
            hard_excluded,
            tcc,
            probe,
        }
    }

    /// `path` lies in a subtree that is never walked (lexical, no syscall).
    fn hard_excluded(&self, path: &Path) -> bool {
        let path = without_data_prefix(path);
        self.hard_excluded.iter().any(|h| path.starts_with(h))
            && !self.allowed_mounts.iter().any(|m| path.starts_with(m))
    }

    /// A directory found by the walk that must not be entered: a hard-excluded subtree or a TCC
    /// folder. A root inside a TCC folder never meets that folder again below itself.
    fn blocks_child(&self, dir: &Path) -> bool {
        let dir = without_data_prefix(dir);
        let dir = dir.as_ref();
        self.tcc.iter().any(|t| t == dir)
            || (self.hard_excluded.iter().any(|h| h == dir)
                && !self.allowed_mounts.iter().any(|m| dir.starts_with(m)))
    }

    fn protected(&self, p: &Path) -> bool {
        self.hard_excluded.iter().chain(&self.tcc).any(|h| h == p)
    }

    /// Checks of a root against its on-disk path `canon` (realpath: symlinks in any component
    /// resolved, on-disk case). Refused when `canon` lies in a never-walked subtree, inside a TCC
    /// folder the lexical root does not lie in, or above a protected path that the walk would
    /// reach under another name (where the exact child checks cannot see it).
    fn root_refused(&self, root: &Path, canon: &Path) -> Option<String> {
        let (r, c) = (without_data_prefix(root), without_data_prefix(canon));
        if self.hard_excluded(&c) {
            return Some(format!(
                "resolves to protected location {}",
                canon.display()
            ));
        }
        for t in &self.tcc {
            let opted_in = |t2: &PathBuf| r.starts_with(t2) && t2.file_name() == t.file_name();
            if c.starts_with(t) && !self.tcc.iter().any(opted_in) {
                return Some(format!(
                    "resolves into privacy-protected folder {}; spell the folder exactly as \
                     on disk (e.g. ~/Downloads)",
                    t.display()
                ));
            }
        }
        if c != r {
            for p in self.hard_excluded.iter().chain(&self.tcc) {
                if let Ok(rel) = p.strip_prefix(c.as_ref())
                    && !rel.as_os_str().is_empty()
                    && !self.protected(&r.join(rel))
                {
                    return Some(format!(
                        "would reach protected location {} under another name",
                        p.display()
                    ));
                }
            }
        }
        None
    }

    /// `Ok` when `path` is on an allowed filesystem: apfs, local, and the data volume or an
    /// accepted `allow_volumes` entry.
    fn filesystem_allowed(&self, path: &Path) -> Result<(), String> {
        let fs = (self.probe)(path).map_err(|e| format!("cannot statfs: {e}"))?;
        if fs.fstype != "apfs" {
            Err(format!("filesystem type {} is not apfs", fs.fstype))
        } else if fs.flags & MNT_LOCAL == 0 {
            Err("not a local filesystem".to_owned())
        } else if !self.allowed.contains(&fs.fsid) {
            Err(format!(
                "volume {} is not the internal data volume (see allow_volumes)",
                fs.mount_on.display()
            ))
        } else {
            Ok(())
        }
    }
}

/// True for `name` = `prefix` followed by `\d+-\d+` and nothing else.
fn is_tool_dir_name(name: &[u8], prefix: &str) -> bool {
    let Some(rest) = name.strip_prefix(prefix.as_bytes()) else {
        return false;
    };
    let digits = |s: &[u8]| !s.is_empty() && s.iter().all(u8::is_ascii_digit);
    match rest.iter().position(|&b| b == b'-') {
        Some(i) => digits(&rest[..i]) && digits(&rest[i + 1..]),
        None => false,
    }
}

/// True for names produced by this tool for staging dirs: `^\.acz-staging-\d+-\d+$`.
pub fn is_stale_staging_name(name: &[u8]) -> bool {
    is_tool_dir_name(name, STAGING_PREFIX)
}

/// True for names produced by this tool for rescue dirs: `^\.acz-rescue-\d+-\d+$`.
pub fn is_rescue_name(name: &[u8]) -> bool {
    is_tool_dir_name(name, RESCUE_PREFIX)
}

#[derive(Clone)]
enum Mode {
    /// Looking for pattern matches.
    Search,
    /// Below a matched directory: every regular file not excluded is a candidate.
    Inside(Arc<Path>),
}

struct Job {
    dir: PathBuf,
    mode: Mode,
    root_dev: u64,
    /// A walk root (a refused `read_dir` gets the permission hint).
    is_root: bool,
}

struct QueueState {
    jobs: Vec<Job>,
    in_flight: usize,
}

/// Shared LIFO of directories; workers stop when it is empty and nobody is still producing.
struct Queue {
    state: Mutex<QueueState>,
    cv: Condvar,
}

impl Queue {
    fn pop(&self) -> Option<Job> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(job) = s.jobs.pop() {
                s.in_flight += 1;
                return Some(job);
            }
            if s.in_flight == 0 {
                return None;
            }
            s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Publishes the children of a finished job and marks the job done.
    fn finish(&self, children: &mut Vec<Job>) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.jobs.append(children);
        s.in_flight -= 1;
        if !s.jobs.is_empty() || s.in_flight == 0 {
            self.cv.notify_all();
        }
    }
}

#[derive(Default)]
pub struct WalkResult {
    /// Deduplicated by (dev, ino), sorted by path.
    pub candidates: Vec<Candidate>,
    /// Leftover staging dirs of crashed runs.
    pub stale_staging: Vec<PathBuf>,
    /// Rescue dirs (exact name regex); never entered, only listed for `clean`.
    pub rescue_dirs: Vec<PathBuf>,
    /// Tracked directories (parents of state rows) whose listing was read to the end without an
    /// error: state rows in them that this walk did not find are gone.
    pub read_dirs: HashSet<PathBuf>,
    /// Files and directories skipped by an exclude.
    pub excluded: u64,
    /// Dataless (cloud placeholder) directories not entered.
    pub dataless: u64,
    /// Walk roots (and filesystem crossings) refused by the guard.
    pub refused_roots: u64,
    /// Directories not entered because of uchg/uappnd/schg/sappnd/restricted, with their flags.
    pub flagged_dirs: Vec<(PathBuf, u32)>,
}

struct Walker<'a> {
    matcher: &'a Matcher,
    guard: &'a Guard,
    one_file_system: bool,
    log: &'a Logger,
    /// Directories to report in `read_dirs`; `None` reports none.
    track_dirs: Option<&'a HashSet<PathBuf>>,
    /// Test hook: every entry of this directory's listing reads as an error.
    fault: Option<&'a Path>,
}

impl Walker<'_> {
    fn lstat(&self, path: &Path, entry: &fs::DirEntry) -> Option<Metadata> {
        entry
            .metadata()
            .inspect_err(|e| {
                self.log
                    .warn(format_args!("cannot stat {}: {e}", path.display()))
            })
            .ok()
    }

    /// Reads one directory, appending found candidates/stale dirs to `out` and subdirectories to `children`.
    fn visit(&self, job: &Job, out: &mut WalkResult, children: &mut Vec<Job>) {
        let entries = match fs::read_dir(&job.dir) {
            Ok(rd) => rd,
            Err(e) => {
                self.read_error(job, &e, out);
                return;
            }
        };
        let found_before = out.candidates.len();
        let mut complete = true;
        for entry in entries {
            let entry = match entry {
                Ok(_) if self.fault == Some(job.dir.as_path()) => {
                    Err(io::Error::other("injected read error"))
                }
                other => other,
            };
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    self.read_error(job, &e, out);
                    complete = false;
                    break;
                }
            };
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(e) => {
                    let p = entry.path();
                    self.log
                        .warn(format_args!("cannot stat {}: {e}", p.display()));
                    complete = false;
                    continue;
                }
            };
            if ft.is_symlink() || !(ft.is_dir() || ft.is_file()) {
                continue;
            }
            let file_name = entry.file_name();
            let name = file_name.as_bytes();
            if name.starts_with(STAGING_PREFIX.as_bytes()) {
                if ft.is_dir() && is_stale_staging_name(name) {
                    out.stale_staging.push(entry.path());
                }
                continue;
            }
            if name.starts_with(RESCUE_PREFIX.as_bytes()) {
                if ft.is_dir() && is_rescue_name(name) {
                    out.rescue_dirs.push(entry.path());
                }
                continue;
            }
            let path = entry.path();
            let c = globset::Candidate::new(&path);
            let (inside, matched) = match &job.mode {
                Mode::Inside(root) => (Some(root), false),
                Mode::Search => (None, self.matcher.patterns.is_match_candidate(&c)),
            };
            // In Search mode only matches and dirs that can lead to one are looked at.
            let wanted = inside.is_some()
                || matched
                || (ft.is_dir() && self.matcher.descend.is_match_candidate(&c));
            if !wanted {
                continue;
            }
            // Exclude wins over a match, for files and directories alike.
            if self.matcher.excluded(&c, &file_name, ft.is_dir()) {
                out.excluded += 1;
                continue;
            }
            if ft.is_file() {
                let root = match inside {
                    Some(root) => root.clone(),
                    None if matched => Arc::from(job.dir.as_path()),
                    None => continue,
                };
                match self.lstat(&path, &entry) {
                    Some(m) => out.candidates.push(Candidate::new(path, root, &m)),
                    None => complete = false,
                }
                continue;
            }
            let mode = match inside {
                Some(_) => job.mode.clone(),
                None if matched => Mode::Inside(Arc::from(path.as_path())),
                None => Mode::Search,
            };
            if !self.enter(path, &entry, job, mode, out, children) {
                complete = false;
            }
        }
        if complete && self.track_dirs.is_some_and(|t| t.contains(&job.dir)) {
            out.read_dirs.insert(job.dir.clone());
        }
        let found = out.candidates.len() - found_before;
        if found > 0 {
            self.log.scan_found(found as u64);
        }
    }

    /// Queues a subdirectory unless the guard blocks it, it carries a protected flag, it is
    /// dataless, or it lies on another filesystem (crossed only without `one_file_system` and
    /// only onto an allowed one). Returns false only when the entry could not be stat'ed.
    fn enter(
        &self,
        path: PathBuf,
        entry: &fs::DirEntry,
        job: &Job,
        mode: Mode,
        out: &mut WalkResult,
        children: &mut Vec<Job>,
    ) -> bool {
        if self.guard.blocks_child(&path) {
            self.log.debug(format_args!(
                "{}: protected location, not entered",
                path.display()
            ));
            return true;
        }
        let Some(m) = self.lstat(&path, entry) else {
            return false;
        };
        if !self.dir_allowed(&path, &m, out) {
            return true;
        }
        let mut root_dev = job.root_dev;
        if m.dev() != job.root_dev {
            if self.one_file_system {
                return true;
            }
            if let Err(reason) = self.guard.filesystem_allowed(&path) {
                self.log.warn(format_args!(
                    "not crossing into {}: {reason}",
                    path.display()
                ));
                out.refused_roots += 1;
                return true;
            }
            root_dev = m.dev();
        }
        children.push(Job {
            dir: path,
            mode,
            root_dev,
            is_root: false,
        });
        true
    }

    /// Flagged directories (with their whole subtree) and dataless ones are never entered.
    fn dir_allowed(&self, path: &Path, m: &Metadata, out: &mut WalkResult) -> bool {
        let flags = m.st_flags();
        if flags & PROTECTED_FLAGS != 0 {
            self.log.file_event(
                Level::Info,
                path,
                FileEvent::Skip("flag"),
                format_args!(
                    "directory skipped with its contents, flag {} set",
                    sys::flag_names(flags)
                ),
            );
            out.flagged_dirs
                .push((path.to_path_buf(), flags & PROTECTED_FLAGS));
            return false;
        }
        if flags & SF_DATALESS != 0 {
            self.log.debug(format_args!(
                "{}: dataless directory, not entered",
                path.display()
            ));
            out.dataless += 1;
            return false;
        }
        true
    }

    /// `EDEADLK` means dataless (counted, DEBUG); a refused walk root gets one ERROR with the
    /// permission hint; anything else is a WARN.
    fn read_error(&self, job: &Job, e: &io::Error, out: &mut WalkResult) {
        let dir = job.dir.display();
        if e.raw_os_error() == Some(EDEADLK) {
            self.log
                .debug(format_args!("{dir}: dataless, not read ({e})"));
            out.dataless += 1;
        } else if job.is_root && e.kind() == io::ErrorKind::PermissionDenied {
            let exe = std::env::current_exe()
                .and_then(fs::canonicalize)
                .map_or_else(
                    |_| "auto-compress-apfs".to_owned(),
                    |p| p.display().to_string(),
                );
            self.log.error(format_args!(
                "no access to {dir}: grant Full Disk Access (or Downloads access) to {exe} in \
                 System Settings ({e})"
            ));
        } else {
            self.log.warn(format_args!("cannot read dir {dir}: {e}"));
        }
    }

    fn refuse(&self, root: &Path, reason: &str, out: &mut WalkResult) {
        self.log.warn(format_args!(
            "walk root {} refused: {reason}",
            root.display()
        ));
        out.refused_roots += 1;
    }

    /// An ancestor of `root` matched by a name exclude or a path exclude (lexical).
    fn excluded_ancestor<'p>(&self, root: &'p Path) -> Option<&'p Path> {
        let m = self.matcher;
        root.ancestors().skip(1).find(|a| {
            a.file_name().is_some_and(|n| m.exclude_name.is_match(n))
                || m.exclude_path.is_match(a)
                || m.exclude_dir.is_match(a)
        })
    }

    /// The nearest real (non-symlink) ancestor of `canon` carrying a protected flag.
    /// `restricted` (SIP) is not checked on ancestors: system dirs like `/usr` carry it while
    /// user-writable trees below them (`/usr/local`) do not; the root and every entered dir are
    /// still checked against the full set.
    fn flagged_ancestor(canon: &Path) -> Option<(PathBuf, u32)> {
        flagged_ancestor_with(canon, |a| {
            let m = fs::symlink_metadata(a).ok()?;
            Some((m.file_type().is_symlink(), m.st_flags()))
        })
    }

    /// Initial job for a walk root, after the guard: hard-excluded subtrees (lexical, before any
    /// syscall), excluded ancestors, excludes, the on-disk path (realpath of every root: symlinks
    /// in any component, case), flagged ancestors, flags, dataless, and the filesystem check.
    /// The root itself may lie inside a pattern match (fully literal pattern, or a scope dir
    /// inside a match); a symlinked root is followed only for searching, never compressed.
    fn seed(&self, root: &Path, out: &mut WalkResult) -> Option<Job> {
        if self.guard.hard_excluded(root) {
            self.refuse(root, "protected location", out);
            return None;
        }
        if let Some(a) = self.excluded_ancestor(root) {
            self.log.info(format_args!(
                "walk root {} skipped: it lies in excluded {}",
                root.display(),
                a.display()
            ));
            out.excluded += 1;
            return None;
        }
        let lmeta = match fs::symlink_metadata(root) {
            Ok(m) => m,
            Err(e) => {
                self.log
                    .debug(format_args!("walk root {} skipped: {e}", root.display()));
                return None;
            }
        };
        let name = root.file_name().unwrap_or(root.as_os_str());
        let c = globset::Candidate::new(root);
        if self.matcher.excluded(&c, name, lmeta.is_dir()) {
            self.log
                .debug(format_args!("walk root {} excluded", root.display()));
            out.excluded += 1;
            return None;
        }
        let canon = match fs::canonicalize(root) {
            Ok(c) => c,
            Err(e) => {
                self.log
                    .debug(format_args!("walk root {} skipped: {e}", root.display()));
                return None;
            }
        };
        if let Some(reason) = self.guard.root_refused(root, &canon) {
            self.refuse(root, &reason, out);
            return None;
        }
        if let Some((a, flags)) = Self::flagged_ancestor(&canon) {
            self.log.file_event(
                Level::Info,
                root,
                FileEvent::Skip("flag"),
                format_args!(
                    "walk root skipped: {} has flag {} set",
                    a.display(),
                    sys::flag_names(flags)
                ),
            );
            out.flagged_dirs.push((a, flags));
            return None;
        }
        let matched = root.ancestors().any(|a| self.matcher.patterns.is_match(a));
        let symlink = lmeta.file_type().is_symlink();
        let meta = if symlink {
            if matched {
                return None;
            }
            fs::metadata(root).ok()?
        } else {
            lmeta
        };
        if meta.is_dir() && !self.dir_allowed(root, &meta, out) {
            return None;
        }
        if let Err(reason) = self.guard.filesystem_allowed(root) {
            self.refuse(root, &reason, out);
            return None;
        }
        if matched && meta.is_file() && !symlink {
            let parent: Arc<Path> = Arc::from(root.parent().unwrap_or(Path::new("/")));
            out.candidates
                .push(Candidate::new(root.to_path_buf(), parent, &meta));
            return None;
        }
        meta.is_dir().then(|| Job {
            dir: root.to_path_buf(),
            mode: if matched {
                Mode::Inside(Arc::from(root))
            } else {
                Mode::Search
            },
            root_dev: meta.dev(),
            is_root: true,
        })
    }
}

/// Walks every pattern root that passes the guard on `threads` workers. Read/stat errors are
/// logged as WARN and skipped. Of `track_dirs` (parents of state rows, when this run prunes
/// state) the completely read ones are returned in `read_dirs`.
pub fn walk(
    matcher: &Matcher,
    guard: &Guard,
    one_file_system: bool,
    threads: usize,
    log: &Logger,
    track_dirs: Option<&HashSet<PathBuf>>,
) -> WalkResult {
    walk_with_fault(
        matcher,
        guard,
        one_file_system,
        threads,
        log,
        track_dirs,
        None,
    )
}

fn walk_with_fault(
    matcher: &Matcher,
    guard: &Guard,
    one_file_system: bool,
    threads: usize,
    log: &Logger,
    track_dirs: Option<&HashSet<PathBuf>>,
    fault: Option<&Path>,
) -> WalkResult {
    let walker = Walker {
        matcher,
        guard,
        one_file_system,
        log,
        track_dirs,
        fault,
    };
    let mut result = WalkResult::default();
    let jobs: Vec<Job> = matcher
        .roots
        .iter()
        .filter_map(|root| walker.seed(root, &mut result))
        .collect();
    let queue = Queue {
        state: Mutex::new(QueueState { jobs, in_flight: 0 }),
        cv: Condvar::new(),
    };
    let parts: Vec<WalkResult> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads.max(1))
            .map(|_| {
                s.spawn(|| {
                    let mut out = WalkResult::default();
                    let mut children = Vec::new();
                    while let Some(job) = queue.pop() {
                        walker.visit(&job, &mut out, &mut children);
                        queue.finish(&mut children);
                    }
                    out
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("walk worker panicked"))
            .collect()
    });

    let mut seen = HashSet::new();
    for part in parts {
        result.candidates.extend(part.candidates);
        result.stale_staging.extend(part.stale_staging);
        result.rescue_dirs.extend(part.rescue_dirs);
        result.read_dirs.extend(part.read_dirs);
        result.excluded += part.excluded;
        result.dataless += part.dataless;
        result.refused_roots += part.refused_roots;
        result.flagged_dirs.extend(part.flagged_dirs);
    }
    result
        .candidates
        .retain(|c| seen.insert((c.key.dev, c.key.ino)));
    result
        .candidates
        .sort_unstable_by(|a, b| a.path.cmp(&b.path));
    result.stale_staging.sort_unstable();
    result.stale_staging.dedup();
    result.rescue_dirs.sort_unstable();
    result.rescue_dirs.dedup();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_prefix_split() {
        assert_eq!(
            literal_prefix("/Users/u/Work_*/**/.venv"),
            Path::new("/Users/u")
        );
        assert_eq!(literal_prefix("/a/b/c"), Path::new("/a/b/c"));
        assert_eq!(literal_prefix("/*/x"), Path::new("/"));
        assert_eq!(literal_prefix("/a/{b,c}/d"), Path::new("/a"));
        assert_eq!(literal_prefix("/a/b?/d"), Path::new("/a"));
        assert_eq!(literal_prefix("/a/[bc]/d"), Path::new("/a"));
    }

    #[test]
    fn matcher_sets() {
        let m = Matcher::new(
            &["/h/Work_*/**/.venv".into(), "/h/one/*/x".into()],
            &["**/node_modules".into(), "/Volumes/**".into()],
            None,
        )
        .unwrap();
        assert_eq!(m.roots, [PathBuf::from("/h"), PathBuf::from("/h/one")]);
        assert!(m.patterns.is_match("/h/Work_a/.venv"));
        assert!(m.patterns.is_match("/h/Work_a/b/c/.venv"));
        assert!(!m.patterns.is_match("/h/Work_a/b/c/.venv/lib"));
        assert!(!m.patterns.is_match("/h/x/Work_a/.venv"));
        assert!(m.descend.is_match("/h/Work_a"));
        assert!(m.descend.is_match("/h/Work_a/b/c"));
        assert!(!m.descend.is_match("/h/Library"));
        assert!(!m.descend.is_match("/h/Other_x"));
        // pattern without `**` prunes at its depth
        assert!(m.descend.is_match("/h/one/y"));
        assert!(!m.descend.is_match("/h/one/y/z"));
        assert!(m.exclude_path.is_match("/h/Work_a/node_modules"));
        assert!(m.exclude_path.is_match("/Volumes/disk/a"));
        assert!(!m.exclude_path.is_match("/h/Work_a/src"));
    }

    #[test]
    fn staging_name_regex() {
        assert!(is_stale_staging_name(b".acz-staging-123-0"));
        assert!(is_stale_staging_name(b".acz-staging-1-99999"));
        for bad in [
            ".acz-staging-",
            ".acz-staging-1",
            ".acz-staging-1-",
            ".acz-staging--1",
            ".acz-staging-a-1",
            ".acz-staging-1-2-3",
            ".acz-staging-1-2x",
            ".acz-rescue-1-2",
            "x.acz-staging-1-2",
        ] {
            assert!(!is_stale_staging_name(bad.as_bytes()), "{bad}");
        }
    }

    #[test]
    fn rescue_name_regex() {
        assert!(is_rescue_name(b".acz-rescue-123-0"));
        assert!(is_rescue_name(b".acz-rescue-99999999999999999999999-1"));
        for bad in [
            ".acz-rescue-",
            ".acz-rescue-1",
            ".acz-rescue-1-",
            ".acz-rescue-x",
            ".acz-rescue-1-2-3",
            ".acz-staging-1-2",
            "x.acz-rescue-1-2",
        ] {
            assert!(!is_rescue_name(bad.as_bytes()), "{bad}");
        }
    }

    /// A temp tree removed on drop, after undoing `chflags uchg` and mode-000 dirs (also when a
    /// test fails).
    struct Tree {
        dir: PathBuf,
        log: Logger,
    }

    impl Tree {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("acz-walk-{tag}-{}", std::process::id()));
            Self::cleanup(&dir);
            fs::create_dir_all(&dir).unwrap();
            let log = Logger::open(&dir.join("log"), 1 << 20, 1, true).unwrap();
            Self { dir, log }
        }

        fn cleanup(dir: &Path) {
            if dir.exists() {
                let run = |cmd: &str, args: &[&str]| {
                    let _ = std::process::Command::new(cmd)
                        .args(args)
                        .arg(dir)
                        .stderr(std::process::Stdio::null())
                        .status();
                };
                run("/bin/chmod", &["-R", "u+rwx"]);
                run("/usr/bin/chflags", &["-R", "nouchg"]);
                run("/bin/chmod", &["-R", "u+rwx"]);
                let _ = fs::remove_dir_all(dir);
            }
        }

        fn file(&self, rel: &str) -> PathBuf {
            let p = self.dir.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, "data").unwrap();
            p
        }

        fn mkdir(&self, rel: &str) -> PathBuf {
            let p = self.dir.join(rel);
            fs::create_dir_all(&p).unwrap();
            p
        }

        fn lock(&self, rel: &str) {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(self.dir.join(rel), fs::Permissions::from_mode(0o000)).unwrap();
        }

        fn pat(&self, rel: &str) -> String {
            format!("{}/{rel}", self.dir.display())
        }

        fn log_text(&self) -> String {
            self.log.flush();
            fs::read_to_string(self.dir.join("log").join(crate::logger::LOG_NAME)).unwrap()
        }

        fn walk(
            &self,
            patterns: &[String],
            exclude: &[String],
            scope: Option<&Path>,
            guard: &Guard,
        ) -> WalkResult {
            let m = Matcher::new(patterns, exclude, scope).unwrap();
            let track = all_dirs(&self.dir);
            walk(&m, guard, true, 3, &self.log, Some(&track))
        }

        fn guard(&self, home: &Path) -> Guard {
            Guard::new(home, &[], &self.log).unwrap()
        }

        /// Candidate paths relative to the tree.
        fn found(&self, r: &WalkResult) -> Vec<String> {
            r.candidates
                .iter()
                .map(|c| {
                    c.path
                        .strip_prefix(&self.dir)
                        .unwrap()
                        .display()
                        .to_string()
                })
                .collect()
        }
    }

    /// Every directory below `dir` that can be listed (and `dir`).
    fn all_dirs(dir: &Path) -> HashSet<PathBuf> {
        let mut out = HashSet::from([dir.to_path_buf()]);
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    out.extend(all_dirs(&e.path()));
                }
            }
        }
        out
    }

    fn accept_all(home: &Path) -> Guard {
        Guard::with_probe(
            home,
            vec![[1, 1]],
            vec![],
            Box::new(|_| Ok(fake_fs([1, 1], "apfs", MNT_LOCAL))),
        )
    }

    #[test]
    fn symlinked_or_miscased_components_cannot_bypass_the_guard() {
        let t = Tree::new("alias");
        let home = t.mkdir("home");
        t.file("home/Documents/p/.venv/doc.py");
        t.file("home/Library/CloudStorage/Drive/cloud.txt");
        t.file("home/Work/.venv/w.py");
        let link = |target: &str, name: &str| {
            std::os::unix::fs::symlink(home.join(target), home.join(name)).unwrap()
        };
        link("Documents", "docs");
        link("Library/CloudStorage", "cl");
        link("", "h");
        let guard = accept_all(&home);
        let pat = |rel: &str| format!("{}/{rel}", home.display());
        for p in [
            pat("docs/p/**"),
            pat("cl/Drive/**"),
            pat("library/CloudStorage/Drive/**"),
            pat("h/**/.venv"),
        ] {
            let r = t.walk(std::slice::from_ref(&p), &[], None, &guard);
            assert!(r.candidates.is_empty(), "{p}: {:?}", t.found(&r));
            assert!(r.read_dirs.is_empty(), "{p}");
            assert_eq!(r.refused_roots, 1, "{p}");
        }
        // The real spellings still work where allowed.
        let r = t.walk(&[pat("Work/.venv")], &[], None, &guard);
        assert_eq!(t.found(&r), ["home/Work/.venv/w.py"]);
        let r = t.walk(&[pat("Documents/p/**")], &[], None, &guard);
        assert_eq!(t.found(&r), ["home/Documents/p/.venv/doc.py"]);
    }

    #[test]
    fn ancestor_flags_ignore_restricted() {
        use crate::sys::{SF_RESTRICTED, UF_IMMUTABLE};
        let canon = Path::new("/usr/local/w/.venv");
        let with = |at: &'static str, flags: u32| {
            flagged_ancestor_with(canon, move |a| {
                Some((false, if a == Path::new(at) { flags } else { 0 }))
            })
        };
        assert_eq!(with("/usr", SF_RESTRICTED), None);
        assert_eq!(
            with("/usr/local", UF_IMMUTABLE | SF_RESTRICTED),
            Some((PathBuf::from("/usr/local"), UF_IMMUTABLE))
        );
        // The root itself is not its own ancestor; symlinks are ignored.
        assert_eq!(with("/usr/local/w/.venv", UF_IMMUTABLE), None);
        assert_eq!(
            flagged_ancestor_with(canon, |_| Some((true, UF_IMMUTABLE))),
            None
        );
    }

    #[test]
    fn data_volume_prefix_is_not_taken_for_system() {
        let t = Tree::new("datavol");
        t.file("a/.venv/x.py");
        let canon = fs::canonicalize(&t.dir).unwrap();
        let root = Path::new(DATA_VOLUME).join(canon.strip_prefix("/").unwrap());
        let r = t.walk(
            &[format!("{}/a/.venv", root.display())],
            &[],
            None,
            &t.guard(&t.dir.join("home")),
        );
        assert_eq!(r.refused_roots, 0, "{}", t.log_text());
        assert_eq!(r.candidates.len(), 1);
    }

    #[test]
    fn root_below_excluded_or_flagged_ancestor_is_refused() {
        let t = Tree::new("ancestor");
        t.file("node_modules/pkg/.venv/a.py");
        t.file("locked/sub/.venv/b.py");
        assert!(
            std::process::Command::new("/usr/bin/chflags")
                .arg("uchg")
                .arg(t.dir.join("locked"))
                .status()
                .unwrap()
                .success()
        );
        let guard = t.guard(&t.dir.join("home"));
        let r = t.walk(
            &[t.pat("node_modules/pkg/.venv")],
            &["node_modules".into()],
            None,
            &guard,
        );
        assert!(r.candidates.is_empty() && r.read_dirs.is_empty());
        assert_eq!(r.excluded, 1);
        let r = t.walk(&[t.pat("locked/sub/.venv")], &[], None, &guard);
        assert!(r.candidates.is_empty() && r.read_dirs.is_empty());
        assert_eq!(r.flagged_dirs.len(), 1);
        let log = t.log_text();
        assert!(log.contains("it lies in excluded"), "{log}");
        assert!(log.contains("has flag uchg set"), "{log}");
    }

    #[test]
    fn only_tracked_and_completely_read_dirs_are_reported() {
        let t = Tree::new("partial");
        t.file("v/.venv/a/f.py");
        t.file("v/.venv/b/g.py");
        t.file("v/.venv/c/h.py");
        let venv = t.dir.join("v/.venv");
        let track: HashSet<PathBuf> = [venv.join("a"), venv.join("b")].into();
        let m = Matcher::new(&[t.pat("v/.venv")], &[], None).unwrap();
        let guard = t.guard(&t.dir.join("home"));
        let fault = venv.join("a");
        let r = walk_with_fault(&m, &guard, true, 2, &t.log, Some(&track), Some(&fault));
        // `a` failed midway, `c` is untracked, `.venv` itself untracked.
        assert_eq!(r.read_dirs, HashSet::from([venv.join("b")]));
        assert!(t.log_text().contains("injected read error"));
        // No tracking (preview, narrowed run): nothing reported.
        let r = walk(&m, &guard, true, 2, &t.log, None);
        assert!(r.read_dirs.is_empty());
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            Self::cleanup(&self.dir);
        }
    }

    fn excl(m: &Matcher, path: &str, is_dir: bool) -> bool {
        let p = Path::new(path);
        m.excluded(&globset::Candidate::new(p), p.file_name().unwrap(), is_dir)
    }

    #[test]
    fn exclude_name_vs_path_and_dir_convention() {
        let m = Matcher::new(
            &["/t/export/**".into()],
            &[
                "*.log".into(),
                "/t/export/margin/**".into(),
                "/t/**/*.{gz,png}".into(),
            ],
            None,
        )
        .unwrap();
        // No `/`: the name at any depth.
        assert!(excl(&m, "/a/b/x.log", false));
        assert!(excl(&m, "/x.log", true));
        assert!(!excl(&m, "/a/x.log.txt", false));
        // `X/**` excludes X itself (as a directory) and everything below.
        assert!(excl(&m, "/t/export/margin", true));
        assert!(excl(&m, "/t/export/margin/a.txt", false));
        assert!(!excl(&m, "/t/export/margins", true));
        assert!(!excl(&m, "/t/export/margin.txt", false));
        assert!(!excl(&m, "/t/export/margin", false));
        // Brace alternation in a path glob.
        assert!(excl(&m, "/t/export/a.gz", false));
        assert!(excl(&m, "/t/export/sub/b.png", false));
        assert!(!excl(&m, "/t/export/a.txt", false));
        assert!(!excl(&m, "/u/a.gz", false));
    }

    #[test]
    fn exclude_beats_include_inside_matches() {
        let t = Tree::new("excl");
        for f in [
            "export/a.txt",
            "export/b.gz",
            "export/margin/m.txt",
            "export/margin/sub/n.txt",
            "export/margins/k.txt",
            "export/margin.txt",
            "export/sub/c.png",
            "export/sub/d.txt",
            "v/.venv/ok.py",
            "v/.venv/x.log",
            "v/.venv/node_modules/pkg.js",
        ] {
            t.file(f);
        }
        // Read would WARN: proves excluded dirs are never read.
        t.lock("export/margin");
        t.lock("v/.venv/node_modules");
        let r = t.walk(
            &[t.pat("export/**"), t.pat("v/.venv")],
            &[
                t.pat("export/margin/**"),
                t.pat("**/*.{gz,png}"),
                "node_modules".into(),
                "*.log".into(),
            ],
            None,
            &t.guard(&t.dir.join("home")),
        );
        assert_eq!(
            t.found(&r),
            [
                "export/a.txt",
                "export/margin.txt",
                "export/margins/k.txt",
                "export/sub/d.txt",
                "v/.venv/ok.py"
            ]
        );
        // margin dir, b.gz, c.png, x.log, node_modules
        assert_eq!(r.excluded, 5);
        assert!(!r.read_dirs.contains(&t.dir.join("export/margin")));
        assert!(r.read_dirs.contains(&t.dir.join("export/sub")));
        let log = t.log_text();
        assert!(!log.contains(" WARN "), "{log}");
    }

    #[test]
    fn rescue_dirs_are_listed_not_entered() {
        let t = Tree::new("rescue");
        t.file("v/.venv/a.py");
        t.file("v/.venv/.acz-rescue-1-2/kept.py");
        t.file("v/.venv/sub/.acz-rescue-3-4/deep.py");
        t.file("v/.venv/.acz-rescue-x/other.py");
        t.file("v/.venv/.acz-staging-5-6/staged.py");
        t.file("v/.venv/.acz-rescue-7-8");
        let target = t.mkdir("elsewhere");
        std::os::unix::fs::symlink(&target, t.dir.join("v/.venv/.acz-rescue-9-9")).unwrap();
        let r = t.walk(&[t.pat("v/.venv")], &[], None, &accept_all(&t.dir));
        assert_eq!(t.found(&r), ["v/.venv/a.py"]);
        assert_eq!(
            r.rescue_dirs,
            [
                t.dir.join("v/.venv/.acz-rescue-1-2"),
                t.dir.join("v/.venv/sub/.acz-rescue-3-4")
            ]
        );
        assert_eq!(r.stale_staging, [t.dir.join("v/.venv/.acz-staging-5-6")]);
    }

    #[test]
    fn flagged_dir_is_not_descended() {
        let t = Tree::new("uchg");
        t.file("v/.venv/a.py");
        t.file("v/.venv/locked/b.py");
        t.file("v/.venv/locked/deep/c.py");
        t.file("w/.venv/x.py");
        let status = |args: &[&str], rel: &str| {
            std::process::Command::new("/usr/bin/chflags")
                .args(args)
                .arg(t.dir.join(rel))
                .status()
                .unwrap()
        };
        assert!(status(&["uchg"], "v/.venv/locked").success());
        // A flagged walk root is refused before its read_dir as well.
        assert!(status(&["uchg"], "w/.venv").success());
        let r = t.walk(
            &[t.pat("v/.venv"), t.pat("w/.venv")],
            &[],
            None,
            &t.guard(&t.dir.join("home")),
        );
        assert_eq!(t.found(&r), ["v/.venv/a.py"]);
        assert!(!r.read_dirs.contains(&t.dir.join("v/.venv/locked")));
        assert!(!r.read_dirs.contains(&t.dir.join("w/.venv")));
        assert_eq!(r.flagged_dirs.len(), 2);
        let log = t.log_text();
        let line = format!(
            "{}: directory skipped with its contents, flag uchg set",
            t.dir.join("v/.venv/locked").display()
        );
        assert_eq!(log.matches(&line).count(), 1, "{log}");
        assert!(log.contains(" INFO "), "{log}");
    }

    fn fake_fs(fsid: [i32; 2], fstype: &str, flags: u32) -> FsInfo {
        FsInfo {
            fsid,
            flags,
            fstype: fstype.into(),
            mount_on: PathBuf::from("/Volumes/Other"),
            bavail_bytes: 1 << 40,
        }
    }

    #[test]
    fn guard_refuses_foreign_non_apfs_and_non_local_roots() {
        let t = Tree::new("guard");
        t.file("a/.venv/x.py");
        let home = t.dir.join("home");
        let cases = [
            (
                fake_fs([9, 9], "apfs", MNT_LOCAL),
                "is not the internal data volume",
            ),
            (
                fake_fs([1, 1], "smbfs", MNT_LOCAL),
                "filesystem type smbfs is not apfs",
            ),
            (fake_fs([1, 1], "apfs", 0), "not a local filesystem"),
        ];
        for (fs_info, reason) in cases {
            let guard = Guard::with_probe(
                &home,
                vec![[1, 1]],
                vec![],
                Box::new(move |_| Ok(fs_info.clone())),
            );
            let r = t.walk(&[t.pat("a/.venv")], &[], None, &guard);
            assert!(r.candidates.is_empty(), "{reason}");
            assert!(r.read_dirs.is_empty(), "{reason}");
            assert_eq!(r.refused_roots, 1, "{reason}");
            assert!(t.log_text().contains(reason), "{reason}");
        }
        let ok = fake_fs([1, 1], "apfs", MNT_LOCAL);
        let guard = Guard::with_probe(
            &home,
            vec![[1, 1]],
            vec![],
            Box::new(move |_| Ok(ok.clone())),
        );
        let r = t.walk(&[t.pat("a/.venv")], &[], None, &guard);
        assert_eq!(t.found(&r), ["a/.venv/x.py"]);
        assert_eq!(r.refused_roots, 0);
    }

    #[test]
    fn protected_subtrees_and_tcc_folders() {
        let t = Tree::new("tcc");
        let home = t.mkdir("home");
        t.file("home/Work/p/.venv/a.py");
        t.file("home/Documents/p/.venv/secret.py");
        t.file("home/Downloads/p/.venv/dl.py");
        t.file("home/Downloads/file.txt");
        t.file("home/Library/CloudStorage/Drive/.venv/cloud.py");
        std::os::unix::fs::symlink(home.join("Documents"), home.join("docs")).unwrap();
        t.lock("home/Documents");
        let calls = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
        let seen = calls.clone();
        let guard = Guard::with_probe(
            &home,
            vec![[1, 1]],
            vec![],
            Box::new(move |p| {
                seen.lock().unwrap().push(p.to_path_buf());
                Ok(fake_fs([1, 1], "apfs", MNT_LOCAL))
            }),
        );
        let pat = |rel: &str| format!("{}/{rel}", home.display());
        // `~/**/.venv` never enters Documents or Downloads (no WARN for the mode-000 dir).
        let r = t.walk(&[pat("**/.venv")], &[], None, &guard);
        assert_eq!(t.found(&r), ["home/Work/p/.venv/a.py"]);
        assert!(!t.log_text().contains(" WARN "), "{}", t.log_text());
        // A root inside a TCC folder is an explicit opt-in.
        let r = t.walk(&[pat("Downloads")], &[], None, &guard);
        assert_eq!(
            t.found(&r),
            ["home/Downloads/file.txt", "home/Downloads/p/.venv/dl.py"]
        );
        // Lexically protected roots are refused before any syscall (the probe is never asked).
        calls.lock().unwrap().clear();
        let r = t.walk(
            &[
                pat("Library/CloudStorage/**/.venv"),
                "/Volumes/X/**".into(),
                "/dev/fd".into(),
            ],
            &[],
            None,
            &guard,
        );
        assert!(r.candidates.is_empty() && r.read_dirs.is_empty());
        assert_eq!(r.refused_roots, 3);
        assert!(calls.lock().unwrap().is_empty());
        // A symlinked root resolving into a TCC folder it does not lie in is refused.
        fs::set_permissions(
            home.join("Documents"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let r = t.walk(&[pat("docs/**/.venv")], &[], None, &guard);
        assert_eq!(r.refused_roots, 1);
        assert!(
            t.log_text()
                .contains("resolves into privacy-protected folder")
        );
    }

    #[test]
    fn unreadable_root_gets_one_permission_error() {
        let t = Tree::new("eacces");
        t.file("locked/a.py");
        t.lock("locked");
        let r = t.walk(&[t.pat("locked")], &[], None, &t.guard(&t.dir.join("home")));
        assert!(r.candidates.is_empty());
        let log = t.log_text();
        assert_eq!(log.matches(" ERROR no access to ").count(), 1, "{log}");
        assert!(log.contains("grant Full Disk Access"), "{log}");
        assert!(!log.contains(" WARN "), "{log}");
    }

    #[test]
    fn scope_dir_intersects_with_patterns() {
        let t = Tree::new("scope");
        t.file("a/x/.venv/lib/m.py");
        t.file("a/x/.venv/top.py");
        t.file("a/y.py");
        t.file("b/.venv/n.py");
        let guard = t.guard(&t.dir.join("home"));
        let pats = [t.pat("**/.venv")];
        let r = t.walk(&pats, &[], Some(&t.dir.join("a")), &guard);
        assert_eq!(t.found(&r), ["a/x/.venv/lib/m.py", "a/x/.venv/top.py"]);
        // A scope inside a match: every file below it is a candidate.
        let lib = t.dir.join("a/x/.venv/lib");
        let r = t.walk(&pats, &[], Some(&lib), &guard);
        assert_eq!(t.found(&r), ["a/x/.venv/lib/m.py"]);
        assert_eq!(&*r.candidates[0].match_root, lib.as_path());
        // A pattern root below the scope is kept, one beside it dropped.
        let m = Matcher::new(
            &[t.pat("b/.venv"), "/elsewhere/**".into()],
            &[],
            Some(&t.dir),
        )
        .unwrap();
        assert_eq!(m.roots, [t.dir.join("b/.venv")]);
    }
}
