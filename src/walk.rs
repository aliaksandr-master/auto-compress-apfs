//! Parallel, symlink-free directory walk that turns glob patterns into candidate files.
//!
//! Every walk root passes the [`Guard`] before its first `read_dir`: only the internal APFS data
//! volume (plus `allow_volumes`) is walked, cloud/volume/system subtrees and privacy-protected
//! (TCC) folders are never entered unless a root lies inside them. Excludes win over patterns
//! for files and directories in both walk modes.

use crate::classify;
use crate::logger::{Level, Logger};
use crate::progress::FileEvent;
use crate::sys::{self, FsInfo, MNT_LOCAL, PROTECTED_FLAGS, SF_DATALESS};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::io::{self, IsTerminal as _};
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
    /// The patterns as given, in `patterns` order.
    pattern_strs: Vec<String>,
}

/// The ERROR text for a walk root `dir` the process may not read. The folder to grant is the
/// last component of `dir` when it is Desktop, Documents or Downloads, else "this folder". From
/// a terminal (`is_tty`) the grant goes to the terminal app (`term_program`, from
/// `TERM_PROGRAM`), since TCC checks the responsible app; elsewhere (launchd, a file) it names
/// the binary `exe`.
fn access_hint(
    is_tty: bool,
    term_program: Option<&str>,
    exe: &str,
    dir: &str,
    err: &io::Error,
) -> String {
    let folder = Path::new(dir)
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|n| matches!(*n, "Desktop" | "Documents" | "Downloads"))
        .unwrap_or("this folder");
    if is_tty {
        let app = term_program.map_or_else(
            || "your terminal app".to_owned(),
            |t| format!("your terminal app ({t})"),
        );
        format!(
            "no access to {dir}: grant access to {folder} (or Full Disk Access) to {app} in System \
             Settings > Privacy & Security; the scheduled launchd run needs Full Disk Access for \
             {exe} ({err})"
        )
    } else {
        format!(
            "no access to {dir}: grant Full Disk Access (or access to {folder}) to {exe} in \
             System Settings ({err})"
        )
    }
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
            pattern_strs: patterns.to_vec(),
        })
    }

    /// True when an exclude matches `path` (name or full path), as during the walk.
    pub fn excludes(&self, path: &Path, is_dir: bool) -> bool {
        let name = path.file_name().unwrap_or(path.as_os_str());
        self.excluded(&globset::Candidate::new(path), name, is_dir)
    }

    /// The first pattern matching `path`.
    pub fn matching_pattern(&self, path: &Path) -> Option<&str> {
        let i = *self.patterns.matches(path).first()?;
        self.pattern_strs.get(i).map(String::as_str)
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
            // `/`, the data volume or an ancestor of home would lift the hard exclusions
            // (CloudStorage, /Volumes, ...) and widen the trash/purge scope to everything.
            if v == Path::new("/") || v == Path::new(DATA_VOLUME) || home.starts_with(v) {
                log.warn(format_args!(
                    "allow_volumes entry {} ignored: the root, the data volume or an ancestor of \
                     the home directory",
                    v.display()
                ));
                continue;
            }
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

    /// Accepted `allow_volumes` mount points.
    pub fn allowed_mounts(&self) -> &[PathBuf] {
        &self.allowed_mounts
    }

    /// Why `path` (a matched entry) must never be trashed or purged: it lies in a never-walked
    /// subtree, or it is a protected location or an ancestor of one (lexical, data prefix
    /// stripped).
    pub fn removal_refused(&self, path: &Path) -> Option<String> {
        let p = without_data_prefix(path);
        if self.hard_excluded(&p) {
            return Some("lies in a protected location".into());
        }
        self.hard_excluded
            .iter()
            .chain(&self.tcc)
            .find(|h| h.starts_with(p.as_ref()))
            .map(|h| format!("is or contains protected location {}", h.display()))
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
    pub(crate) fn filesystem_allowed(&self, path: &Path) -> Result<(), String> {
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
    /// Deduplicated by (dev, ino), sorted by path. From [`walk_reduced`]: without the too-small
    /// files below a walk root (a too-small literal file root is still in it).
    pub candidates: Vec<Candidate>,
    /// [`walk_reduced`] only: distinct too-small inodes not found as a kept candidate.
    pub too_small: u64,
    /// [`walk_reduced`] only: paths of counted too-small files that `Reduce::tracked` accepted.
    pub too_small_tracked: Vec<PathBuf>,
    /// Per worker: (dev, ino) of too-small files, before the merge.
    small: Vec<(u64, u64)>,
    /// Per worker: too-small files that `Reduce::tracked` accepted, before the merge.
    small_tracked: Vec<(u64, u64, PathBuf)>,
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
    /// Walk roots (only, no crossings) refused by the guard; part of `refused_roots`.
    pub refused_walk_roots: u64,
    /// Walk roots that could not be read (permission denied).
    pub unreadable_roots: u64,
    /// Directories not entered because of uchg/uappnd/schg/sappnd/restricted, with their flags.
    pub flagged_dirs: Vec<(PathBuf, u32)>,
    /// [`walk_entries`] only: files and directories matched by a pattern (a matched directory
    /// is not entered). Sorted by path, each once; nested matches (from overlapping roots) kept.
    pub matched: Vec<Matched>,
}

impl WalkResult {
    /// Walk roots that exist but could not be read (an access problem the user can fix).
    /// Roots refused by the guard and refused filesystem crossings are not counted.
    pub fn inaccessible_roots(&self) -> u64 {
        self.unreadable_roots
    }
}

/// A file or directory matched by a pattern, as the walk saw it (`lstat`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched {
    pub path: PathBuf,
    pub is_dir: bool,
    pub dev: u64,
    pub ino: u64,
}

impl Matched {
    fn new(path: PathBuf, m: &Metadata) -> Self {
        Self {
            path,
            is_dir: m.is_dir(),
            dev: m.dev(),
            ino: m.ino(),
        }
    }
}

/// Makes [`walk_reduced`] count too-small files instead of keeping them as candidates.
pub struct Reduce<'a> {
    pub min_size: u64,
    /// The caller needs this path back (to mark its state row `seen`).
    pub tracked: &'a (dyn Fn(&Path) -> bool + Sync),
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
    /// Collect matched entries (files and dirs, never entered) instead of compress candidates.
    entries: bool,
    /// Count too-small files (see [`Reduce`]) instead of collecting them.
    reduce: Option<&'a Reduce<'a>>,
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
        let entries = match read_retrying(|| fs::read_dir(&job.dir)) {
            Ok(entries) => entries,
            Err(e) => {
                self.read_error(job, &e, out);
                return;
            }
        };
        let mut found = 0u64;
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
            if self.entries {
                if matched {
                    match self.lstat(&path, &entry) {
                        // Another filesystem (a mount point): never acted on, as in `enter`.
                        Some(m) if m.dev() != job.root_dev => {
                            self.log.info(format_args!(
                                "{}: on another filesystem, not matched",
                                path.display()
                            ));
                            out.refused_roots += 1;
                        }
                        Some(m) => out.matched.push(Matched::new(path, &m)),
                        None => complete = false,
                    }
                    continue;
                }
                if ft.is_file() {
                    continue;
                }
            }
            if ft.is_file() {
                if inside.is_none() && !matched {
                    continue;
                }
                let Some(m) = self.lstat(&path, &entry) else {
                    complete = false;
                    continue;
                };
                found += 1;
                if let Some(r) = self
                    .reduce
                    .filter(|r| classify::too_small(m.st_flags(), m.size(), r.min_size))
                {
                    if (r.tracked)(&path) {
                        out.small_tracked.push((m.dev(), m.ino(), path));
                    } else {
                        out.small.push((m.dev(), m.ino()));
                    }
                    continue;
                }
                let root = match inside {
                    Some(root) => root.clone(),
                    None => Arc::from(job.dir.as_path()),
                };
                out.candidates.push(Candidate::new(path, root, &m));
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
        if found > 0 {
            self.log.scan_found(found);
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

    /// `EDEADLK` means dataless (counted, DEBUG); a walk root denied by permissions gets one
    /// ERROR with the [`access_hint`] and counts in `unreadable_roots`; anything else is a WARN.
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
            let term = std::env::var("TERM_PROGRAM").ok();
            let tty = io::stderr().is_terminal();
            self.log.error(format_args!(
                "{}",
                access_hint(tty, term.as_deref(), &exe, &dir.to_string(), e)
            ));
            out.unreadable_roots += 1;
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
        out.refused_walk_roots += 1;
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
        // Entries: only a root the patterns match itself is an item; below an ancestor's match
        // it is searched like any other root.
        if self.entries && self.matcher.patterns.is_match(root) {
            if symlink {
                return None;
            }
            out.matched.push(Matched::new(root.to_path_buf(), &meta));
            return None;
        }
        let matched = matched && !self.entries;
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
        false,
        None,
    )
}

/// Like [`walk`], but a file below a walk root that `classify::too_small` accepts is only
/// counted in `too_small` (its path kept in `too_small_tracked` when `reduce.tracked` accepts
/// it) and is not a candidate.
pub fn walk_reduced(
    matcher: &Matcher,
    guard: &Guard,
    one_file_system: bool,
    threads: usize,
    log: &Logger,
    track_dirs: Option<&HashSet<PathBuf>>,
    reduce: &Reduce,
) -> WalkResult {
    walk_with_fault(
        matcher,
        guard,
        one_file_system,
        threads,
        log,
        track_dirs,
        None,
        false,
        Some(reduce),
    )
}

/// Like [`walk`], but collects the files and directories matched by a pattern (for `trash`
/// and `purge`); a matched directory is not entered.
pub fn walk_entries(
    matcher: &Matcher,
    guard: &Guard,
    one_file_system: bool,
    threads: usize,
    log: &Logger,
) -> WalkResult {
    walk_with_fault(
        matcher,
        guard,
        one_file_system,
        threads,
        log,
        None,
        None,
        true,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn walk_with_fault(
    matcher: &Matcher,
    guard: &Guard,
    one_file_system: bool,
    threads: usize,
    log: &Logger,
    track_dirs: Option<&HashSet<PathBuf>>,
    fault: Option<&Path>,
    entries: bool,
    reduce: Option<&Reduce>,
) -> WalkResult {
    let walker = Walker {
        matcher,
        guard,
        one_file_system,
        log,
        track_dirs,
        fault,
        entries,
        reduce,
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
    result
        .candidates
        .reserve_exact(parts.iter().map(|p| p.candidates.len()).sum());
    let mut small = Vec::with_capacity(parts.iter().map(|p| p.small.len()).sum());
    for part in parts {
        result.candidates.extend(part.candidates);
        small.extend(part.small);
        result.small_tracked.extend(part.small_tracked);
        result.stale_staging.extend(part.stale_staging);
        result.rescue_dirs.extend(part.rescue_dirs);
        result.read_dirs.extend(part.read_dirs);
        result.excluded += part.excluded;
        result.dataless += part.dataless;
        result.refused_roots += part.refused_roots;
        result.refused_walk_roots += part.refused_walk_roots;
        result.unreadable_roots += part.unreadable_roots;
        result.flagged_dirs.extend(part.flagged_dirs);
        result.matched.extend(part.matched);
    }
    // Overlapping roots can find an entry twice; one nested in another matched entry is kept
    // here (the outer one may be refused) and dropped by the caller when the outer one is not.
    result.matched.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    let mut ids = HashSet::new();
    result.matched.retain(|m| ids.insert((m.dev, m.ino)));
    result
        .candidates
        .retain(|c| seen.insert((c.key.dev, c.key.ino)));
    // Too-small duplicates: a kept candidate wins, then one with a state row, then the rest.
    for (dev, ino, path) in std::mem::take(&mut result.small_tracked) {
        if seen.insert((dev, ino)) {
            result.too_small += 1;
            result.too_small_tracked.push(path);
        }
    }
    small.sort_unstable();
    small.dedup();
    result.too_small += small.iter().filter(|id| !seen.contains(*id)).count() as u64;
    drop(small);
    result
        .candidates
        .sort_unstable_by(|a, b| a.path.cmp(&b.path));
    result.stale_staging.sort_unstable();
    result.stale_staging.dedup();
    result.rescue_dirs.sort_unstable();
    result.rescue_dirs.dedup();
    result
}

/// Attempts at reading one directory when a read is interrupted by a signal (EINTR).
const READ_ATTEMPTS: u32 = 3;

/// Reads a whole directory listing, starting over when the open or any entry read fails with
/// `Interrupted` (at most [`READ_ATTEMPTS`] times), so a signal does not skip the directory.
/// Any other entry error ends the listing as its last item, as an unbuffered read would.
fn read_retrying<T, I>(mut open: impl FnMut() -> io::Result<I>) -> io::Result<Vec<io::Result<T>>>
where
    I: Iterator<Item = io::Result<T>>,
{
    let mut attempt = 1;
    'read: loop {
        let mut entries = Vec::new();
        let it = match open() {
            Ok(it) => it,
            Err(e) if e.kind() == io::ErrorKind::Interrupted && attempt < READ_ATTEMPTS => {
                attempt += 1;
                continue;
            }
            Err(e) => return Err(e),
        };
        for entry in it {
            match entry {
                Err(e) if e.kind() == io::ErrorKind::Interrupted && attempt < READ_ATTEMPTS => {
                    attempt += 1;
                    continue 'read;
                }
                Err(e) => {
                    entries.push(Err(e));
                    break;
                }
                ok => entries.push(ok),
            }
        }
        return Ok(entries);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_retrying_starts_over_on_eintr() {
        let eintr = || io::Error::from(io::ErrorKind::Interrupted);
        // Interrupted mid-listing once, then a full read: only the full read is returned.
        let mut calls = 0;
        let r = read_retrying(|| {
            calls += 1;
            Ok(if calls == 1 {
                vec![Ok(1), Err(eintr())]
            } else {
                vec![Ok(1), Ok(2)]
            }
            .into_iter())
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(
            r.into_iter().map(Result::unwrap).collect::<Vec<_>>(),
            [1, 2]
        );
        // An interrupted open is retried too.
        let mut calls = 0;
        let r = read_retrying(|| {
            calls += 1;
            if calls == 1 {
                Err(eintr())
            } else {
                Ok(vec![Ok(7)].into_iter())
            }
        })
        .unwrap();
        assert_eq!((calls, r.len()), (2, 1));
        // Never more than READ_ATTEMPTS reads; the last EINTR is the reported error.
        let mut calls = 0;
        let e = read_retrying(|| -> io::Result<std::vec::IntoIter<io::Result<u8>>> {
            calls += 1;
            Err(eintr())
        })
        .unwrap_err();
        assert_eq!(
            (calls, e.kind()),
            (READ_ATTEMPTS, io::ErrorKind::Interrupted)
        );
        // An EINTR mid-listing on the last attempt ends the listing as an error item.
        let mut calls = 0;
        let r = read_retrying(|| {
            calls += 1;
            Ok(vec![Ok(1), Err(eintr())].into_iter())
        })
        .unwrap();
        assert_eq!((calls, r.len()), (READ_ATTEMPTS, 2));
        assert_eq!(
            r[1].as_ref().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        // Another error ends the listing, keeping what was read before it.
        let r = read_retrying(|| Ok(vec![Ok(1), Err(io::Error::other("x")), Ok(3)].into_iter()))
            .unwrap();
        assert_eq!(r.len(), 2);
        assert!(r[1].is_err());
    }

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
    fn reduced_walk_matches_full_walk() {
        let t = Tree::new("reduced");
        let big = |rel: &str| {
            let p = t.file(rel);
            fs::write(&p, [b'x'; 64]).unwrap();
            p
        };
        t.file("a/s1.txt");
        t.file("a/b/s2.txt");
        let tracked = t.file("a/b/tracked.txt");
        big("a/g1.bin");
        big("a/b/g2.bin");
        fs::hard_link(t.file("a/c/small.txt"), t.dir.join("a/b/small_link.txt")).unwrap();
        fs::hard_link(big("a/c/big.bin"), t.dir.join("a/b/big_link.bin")).unwrap();
        let m = Matcher::new(&[t.pat("a/**"), t.pat("a/b/**")], &[], None).unwrap();
        let guard = accept_all(&t.dir.join("home"));
        let track = all_dirs(&t.dir);
        let min_size = 16;
        let link = t.dir.join("a/b/small_link.txt");
        let is_tracked = |p: &Path| p == tracked || p == link;
        let reduce = Reduce {
            min_size,
            tracked: &is_tracked,
        };
        let small = |c: &Candidate| classify::too_small(c.flags, c.size(), min_size);
        let ids = |cs: &mut dyn Iterator<Item = &Candidate>| -> HashSet<(u64, u64)> {
            cs.map(|c| (c.key.dev, c.key.ino)).collect()
        };
        let full = walk(&m, &guard, true, 3, &t.log, Some(&track));
        // The tracked path of the small hardlink pair always wins over the untracked one.
        for _ in 0..5 {
            let mut got =
                walk_reduced(&m, &guard, true, 3, &t.log, Some(&track), &reduce).too_small_tracked;
            got.sort();
            assert_eq!(got, vec![link.clone(), tracked.clone()]);
        }
        let red = walk_reduced(&m, &guard, true, 3, &t.log, Some(&track), &reduce);
        let kept: Vec<&Candidate> = full.candidates.iter().filter(|c| !small(c)).collect();
        assert_eq!(red.candidates.len(), kept.len());
        assert_eq!(ids(&mut red.candidates.iter()), ids(&mut kept.into_iter()));
        let full_small = full.candidates.iter().filter(|c| small(c)).count() as u64;
        assert_eq!(red.too_small, full_small);
        assert_eq!(full_small, 4);
        assert_eq!(
            red.candidates.len() as u64 + red.too_small,
            full.candidates.len() as u64
        );
        assert_eq!(red.too_small_tracked.len(), 2);
        assert_eq!(full.too_small, 0);
        assert!(full.too_small_tracked.is_empty());
        assert_eq!(red.excluded, full.excluded);
        assert_eq!(red.read_dirs, full.read_dirs);
        assert_eq!(red.stale_staging, full.stale_staging);
        assert_eq!(red.rescue_dirs, full.rescue_dirs);
        assert_eq!(red.flagged_dirs, full.flagged_dirs);
        // One worker visits in a fixed order: the surviving paths are equal too.
        let full = walk(&m, &guard, true, 1, &t.log, Some(&track));
        let red = walk_reduced(&m, &guard, true, 1, &t.log, Some(&track), &reduce);
        assert_eq!(
            t.found(&red),
            full.candidates
                .iter()
                .filter(|c| !small(c))
                .map(|c| c.path.strip_prefix(&t.dir).unwrap().display().to_string())
                .collect::<Vec<_>>()
        );
        let mut got = red.too_small_tracked;
        got.sort();
        assert_eq!(got, vec![link, tracked]);
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
        let r = walk_with_fault(
            &m,
            &guard,
            true,
            2,
            &t.log,
            Some(&track),
            Some(&fault),
            false,
            None,
        );
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
        // The terminal variant (stderr a TTY) words it differently; both name Full Disk Access.
        assert!(log.contains("Full Disk Access"), "{log}");
        assert!(!log.contains(" WARN "), "{log}");
        assert_eq!(r.unreadable_roots, 1);
    }

    #[test]
    fn refused_crossings_and_roots_are_not_inaccessible() {
        let t = Tree::new("crossing");
        t.file("a/x/m.py");
        let guard = t.guard(&t.dir.join("home"));
        let m = Matcher::new(&[t.pat("a/x")], &[], None).unwrap();
        let walker = Walker {
            matcher: &m,
            guard: &guard,
            one_file_system: true,
            log: &t.log,
            track_dirs: None,
            fault: None,
            entries: true,
            reduce: None,
        };
        // A device id no entry has: the matched `a/x` reads as a mount point below the root.
        let job = Job {
            dir: t.dir.join("a"),
            mode: Mode::Search,
            root_dev: u64::MAX,
            is_root: true,
        };
        let mut out = WalkResult::default();
        walker.visit(&job, &mut out, &mut Vec::new());
        assert_eq!(out.refused_roots, 1, "{}", t.log_text());
        assert_eq!(out.refused_walk_roots, 0);
        assert_eq!(out.inaccessible_roots(), 0);
        // A root refused by the guard is not an access problem either.
        let r = t.walk(&["/Volumes/X/**".into()], &[], None, &guard);
        assert_eq!((r.refused_roots, r.refused_walk_roots), (1, 1));
        assert_eq!(r.inaccessible_roots(), 0);
    }

    #[test]
    fn access_hint_names_terminal_app_on_tty() {
        let e = io::Error::from_raw_os_error(1);
        let h = access_hint(true, Some("Apple_Terminal"), "/x/acz", "/u/Downloads", &e);
        assert!(h.starts_with(
            "no access to /u/Downloads: grant access to Downloads (or Full Disk Access)"
        ));
        assert!(h.contains("to your terminal app (Apple_Terminal) in System Settings > Privacy"));
        assert!(h.contains("needs Full Disk Access for /x/acz"), "{h}");
        let h = access_hint(true, None, "/x/acz", "/d", &e);
        assert!(h.contains("to your terminal app in System Settings"), "{h}");
        let h = access_hint(false, Some("Apple_Terminal"), "/x/acz", "/d", &e);
        assert!(
            h.contains("grant Full Disk Access (or access to this folder) to /x/acz"),
            "{h}"
        );
        assert!(!h.contains("terminal"), "{h}");
        // The protected folder is named from the root, any other root says "this folder".
        let h = access_hint(true, None, "/x/acz", "/u/Documents", &e);
        assert!(
            h.contains("grant access to Documents (or Full Disk Access)"),
            "{h}"
        );
        let h = access_hint(false, None, "/x/acz", "/u/Documents", &e);
        assert!(h.contains("(or access to Documents) to /x/acz"), "{h}");
        let h = access_hint(false, None, "/x/acz", "/u/p", &e);
        assert!(h.contains("(or access to this folder) to /x/acz"), "{h}");
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
