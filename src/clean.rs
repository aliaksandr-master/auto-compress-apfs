//! `clean` command: lists the leftover staging and rescue dirs the walk finds, classifies their
//! files, and with `--apply` deletes them.
//!
//! A leftover is deleted only when its name matches the exact staging/rescue regex, it is still
//! the directory the scan saw (same dev and inode, not a symlink), it lies on its parent's
//! filesystem, and the scan found no flagged, dataless or foreign-filesystem entry in it. A
//! staging or rescue dir changed within [`LIVE_GUARD`] may belong to a running compress (the
//! run lock is per `HOME`, and such a dir can hold the user's only original) and is kept.
//! The file classification is informational: it never decides what is deleted.
//!
//! Residual window: between the re-check and `remove_dir_all` nothing stops another process of
//! the user from replacing the dir with another directory of the same name, which would then be
//! deleted instead. A symlink put at the leftover's own path is not followed: std's
//! `remove_dir_all` unlinks it itself; a parent component swapped for a symlink in that window
//! would be followed.

use crate::compress::{self, Buffers, KEEP_MARKER, Verify};
use crate::logger::Logger;
use crate::progress::{abbreviate_home, human};
use crate::stat::when;
use crate::sys::{self, PROTECTED_FLAGS, SF_DATALESS};
use crate::walk::{
    Candidate, RESCUE_PREFIX, STAGING_PREFIX, is_rescue_name, is_stale_staging_name,
};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, Metadata};
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// A leftover with a timestamp younger than this (seconds) may belong to a running compress.
pub const LIVE_GUARD: i64 = 6 * 3600;
/// Byte-compares tried per leftover file before it counts as having no copy.
const MAX_COMPARES: usize = 8;
/// Leftover bytes byte-compared by one `clean` run at most.
const VERIFY_BUDGET: u64 = 4 << 30;
/// Name prefix of the partial files an interrupted ditto call leaves behind.
const DITTO_TEMP_PREFIX: &[u8] = b".BC.T_";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Staging,
    Rescue,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::Staging => STAGING_PREFIX,
            Kind::Rescue => RESCUE_PREFIX,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Staging => "staging",
            Kind::Rescue => "rescue",
        }
    }

    /// The exact name regex of this kind.
    fn matches(self, name: &[u8]) -> bool {
        match self {
            Kind::Staging => is_stale_staging_name(name),
            Kind::Rescue => is_rescue_name(name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileClass {
    /// Partial file of an interrupted ditto call (`.BC.T_*`).
    DittoTemp,
    /// Byte-identical to this file found by the walk.
    HasCopy(PathBuf),
    /// No identical file found, not compared, or not a regular file: possibly the only copy.
    /// Only regular files are reported as such; other entries are listed apart.
    NoCopy,
}

/// One entry below a leftover dir, from `lstat`.
#[derive(Debug)]
pub struct Item {
    pub path: PathBuf,
    pub class: FileClass,
    pub size: u64,
    pub allocated: u64,
    pub is_file: bool,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub mtime: i64,
    pub ctime: i64,
}

/// A leftover staging or rescue dir with its scanned contents.
#[derive(Debug)]
pub struct Leftover {
    pub path: PathBuf,
    pub kind: Kind,
    /// Digits of the creating process id, as in the name (never parsed).
    pub pid: String,
    pub dev: u64,
    pub ino: u64,
    /// Birthtime of the dir.
    pub created: i64,
    pub mtime: i64,
    pub keep_marker: bool,
    pub entries: Vec<Item>,
    /// Why the dir is never deleted, if it is not.
    pub blocked: Option<String>,
    /// Why the classification is incomplete, if it is.
    pub incomplete: Option<String>,
}

impl Leftover {
    /// Keeps the first reason.
    fn block(&mut self, why: String) {
        if self.blocked.is_none() {
            self.blocked = Some(why);
        }
    }

    fn files(&self) -> impl Iterator<Item = &Item> {
        self.entries.iter().filter(|i| i.is_file)
    }

    /// Logical size of the regular files.
    pub fn size(&self) -> u64 {
        self.files().map(|i| i.size).sum()
    }

    /// Allocated bytes of every entry.
    pub fn allocated(&self) -> u64 {
        self.entries.iter().map(|i| i.allocated).sum()
    }
}

/// Why an entry with these flags must never be touched, if it must not.
fn flag_reason(path: &Path, m: &Metadata) -> Option<String> {
    let flags = m.st_flags();
    if flags & PROTECTED_FLAGS != 0 {
        Some(format!(
            "{} has flag {}",
            path.display(),
            sys::flag_names(flags & PROTECTED_FLAGS)
        ))
    } else if flags & SF_DATALESS != 0 {
        Some(format!(
            "{} is dataless (cloud placeholder)",
            path.display()
        ))
    } else {
        None
    }
}

/// Lists `path` recursively with `lstat` only: never follows a symlink, never enters a
/// directory on another filesystem or a dataless one. Any flagged, dataless, foreign-filesystem
/// or unreadable entry blocks the dir. `None` (with a WARN) when `path` is no longer a directory.
pub fn scan(path: &Path, kind: Kind, log: &Logger) -> Option<Leftover> {
    let root = match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => m,
        Ok(_) => {
            log.warn(format_args!(
                "clean: {} is no longer a directory, skipped",
                path.display()
            ));
            return None;
        }
        Err(e) => {
            log.warn(format_args!("clean: cannot stat {}: {e}", path.display()));
            return None;
        }
    };
    let name = path.file_name().map(OsStrExt::as_bytes).unwrap_or_default();
    let rest = name.get(kind.prefix().len()..).unwrap_or_default();
    let pid = rest.split(|&b| b == b'-').next().unwrap_or_default();
    let mut lo = Leftover {
        path: path.to_path_buf(),
        kind,
        pid: String::from_utf8_lossy(pid).into_owned(),
        dev: root.dev(),
        ino: root.ino(),
        created: root.st_birthtime(),
        mtime: root.mtime(),
        keep_marker: false,
        entries: Vec::new(),
        blocked: None,
        incomplete: None,
    };
    if let Some(why) = flag_reason(path, &root) {
        lo.block(why);
    }
    match fs::symlink_metadata(path.parent().unwrap_or(Path::new("/"))) {
        Ok(pm) if pm.dev() == lo.dev => {}
        Ok(_) => lo.block("on another filesystem than its parent".into()),
        Err(e) => lo.block(format!("cannot stat its parent: {e}")),
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                lo.block(format!("cannot read {}: {e}", dir.display()));
                continue;
            }
        };
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    lo.block(format!("cannot read {}: {e}", dir.display()));
                    break;
                }
            };
            let p = entry.path();
            // `DirEntry::metadata` is an lstat: symlinks are not followed.
            let m = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    lo.block(format!("cannot stat {}: {e}", p.display()));
                    continue;
                }
            };
            if let Some(why) = flag_reason(&p, &m) {
                lo.block(why);
            }
            let foreign = m.dev() != lo.dev;
            if foreign {
                lo.block(format!("{} is on another filesystem", p.display()));
            }
            if m.is_dir() {
                if dir == path && entry.file_name() == KEEP_MARKER {
                    lo.keep_marker = true;
                }
                if !foreign && m.st_flags() & SF_DATALESS == 0 {
                    stack.push(p.clone());
                }
            }
            lo.entries.push(Item {
                path: p,
                class: FileClass::NoCopy,
                size: m.size(),
                allocated: m.blocks() * 512,
                is_file: m.is_file(),
                is_dir: m.is_dir(),
                is_symlink: m.is_symlink(),
                mtime: m.mtime(),
                ctime: m.ctime(),
            });
        }
    }
    lo.entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    Some(lo)
}

/// Classifies the regular files of every leftover: `.BC.T_*` is a ditto partial file; a file
/// byte-identical to a same-named, same-sized candidate below the leftover's parent has a copy
/// (at most [`MAX_COMPARES`] compares per file, the leftover's own directory first, and
/// [`VERIFY_BUDGET`] bytes for the whole run, after which the rest stays unclassified and the
/// leftover is marked incomplete). Leftovers already blocked by the scan are not read.
/// Correct because a staged copy keeps the original's basename and basenames are unique within
/// a batch (see `Compressor::process` and `split_batches`).
pub fn classify(leftovers: &mut [Leftover], candidates: &[Candidate]) {
    classify_with(leftovers, candidates, VERIFY_BUDGET);
}

/// [`classify`] with `budget` bytes to compare.
fn classify_with(leftovers: &mut [Leftover], candidates: &[Candidate], mut budget: u64) {
    let mut by_name: HashMap<OsString, Vec<usize>> = HashMap::new();
    for lo in leftovers.iter().filter(|l| l.blocked.is_none()) {
        for item in lo.files() {
            if let Some(n) = item.path.file_name() {
                by_name.entry(n.to_os_string()).or_default();
            }
        }
    }
    for (i, c) in candidates.iter().enumerate() {
        if c.flags & SF_DATALESS != 0 {
            continue;
        }
        if let Some(v) = c.path.file_name().and_then(|n| by_name.get_mut(n)) {
            v.push(i);
        }
    }
    let mut bufs = Buffers::default();
    for lo in leftovers.iter_mut() {
        let parent = lo.path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let compare = lo.blocked.is_none();
        for item in lo.entries.iter_mut().filter(|i| i.is_file) {
            let name = item.path.file_name().unwrap_or_default();
            if name.as_bytes().starts_with(DITTO_TEMP_PREFIX) {
                item.class = FileClass::DittoTemp;
                continue;
            }
            let Some(idx) = by_name.get(name).filter(|_| compare) else {
                continue;
            };
            let mut found: Vec<&Candidate> = idx
                .iter()
                .map(|&i| &candidates[i])
                .filter(|c| c.size() == item.size && c.path.starts_with(&parent))
                .collect();
            // Stable: same directory as the leftover first, then by path.
            found.sort_by_key(|c| c.path.parent() != Some(parent.as_path()));
            for c in found.into_iter().take(MAX_COMPARES) {
                if item.size > budget {
                    budget = 0;
                    lo.incomplete = Some("verify budget exhausted".into());
                    break;
                }
                budget -= item.size;
                if let Ok(Verify::Same) = compress::verify(c, &item.path, &mut bufs) {
                    item.class = FileClass::HasCopy(c.path.clone());
                    break;
                }
            }
        }
    }
}

/// Blocks every leftover (staging or rescue) whose birthtime, mtime or newest content
/// mtime/ctime is younger than [`LIVE_GUARD`] (or in the future).
pub fn block_live(leftovers: &mut [Leftover], now: i64) {
    for lo in leftovers.iter_mut() {
        let newest = lo
            .entries
            .iter()
            .flat_map(|i| [i.mtime, i.ctime])
            .chain([lo.created, lo.mtime])
            .max()
            .unwrap_or(0);
        if now.saturating_sub(newest) < LIVE_GUARD {
            lo.block(format!(
                "changed less than {}h ago (last change {}), may belong to a running compress",
                LIVE_GUARD / 3600,
                ago(now.saturating_sub(newest))
            ));
        }
    }
}

/// `3d 4h`, `4h 12m`, `12m`, `40s`.
fn span(secs: i64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{secs}s")
    }
}

/// `3d 4h ago` for a past age in seconds, `5m in the future` for a negative one.
pub fn ago(secs: i64) -> String {
    if secs < 0 {
        format!("{} in the future", span(secs.saturating_neg()))
    } else {
        format!("{} ago", span(secs))
    }
}

/// `2026-10-18 10:00 (1d 2h ago)`, `-` when unknown.
fn when_ago(t: Option<i64>, now: i64) -> String {
    match t.filter(|&t| t > 0) {
        Some(t) => format!("{} ({})", when(t), ago(now - t)),
        None => "-".to_owned(),
    }
}

/// The preview on stdout: every leftover with its age, sizes, per-class counts, the files with
/// no copy, why it is kept; totals; and `apply_cmd`, the command that would delete them.
pub fn render(leftovers: &[Leftover], home: &Path, now: i64, apply_cmd: &str) -> String {
    let show = |p: &Path| abbreviate_home(p, Some(home));
    let mut out = String::new();
    if leftovers.is_empty() {
        out.push_str("no leftover staging or rescue dirs found\n");
        return out;
    }
    for lo in leftovers {
        let _ = writeln!(
            out,
            "{}   {}, pid {}",
            show(&lo.path),
            lo.kind.name(),
            lo.pid
        );
        let _ = writeln!(out, "  created        {}", when_ago(Some(lo.created), now));
        let _ = writeln!(out, "  dir modified   {}", when_ago(Some(lo.mtime), now));
        let last = lo.entries.iter().map(|i| i.ctime).max();
        let _ = writeln!(out, "  last written   {}", when_ago(last, now));
        let oldest = lo.files().map(|i| i.mtime).min();
        let newest = lo.files().map(|i| i.mtime).max();
        if let (Some(a), Some(b)) = (oldest, newest) {
            let _ = writeln!(
                out,
                "  content mtime  {} .. {} (copies keep the original's mtime)",
                when(a),
                when(b)
            );
        }
        let _ = writeln!(
            out,
            "  files          {} ({} entries), {}, {} on disk",
            lo.files().count(),
            lo.entries.len(),
            human(lo.size()),
            human(lo.allocated())
        );
        let mut no_copy = Vec::new();
        let mut not_files = Vec::new();
        let (mut temp, mut copy, mut none) = ((0, 0), (0, 0), (0, 0));
        for i in &lo.entries {
            if !i.is_file {
                // Directories (the .acz-keep marker included) are no files of their own.
                if !i.is_dir {
                    not_files.push(i);
                }
                continue;
            }
            let slot = match &i.class {
                FileClass::DittoTemp => &mut temp,
                FileClass::HasCopy(_) => &mut copy,
                FileClass::NoCopy => {
                    no_copy.push(i);
                    &mut none
                }
            };
            slot.0 += 1;
            slot.1 += i.size;
        }
        for (label, (n, bytes)) in [
            ("ditto partial ", temp),
            ("has live copy ", copy),
            ("no copy found ", none),
        ] {
            let _ = writeln!(out, "  {label} {n}, {}", human(bytes));
        }
        for i in no_copy {
            let _ = writeln!(out, "    {}", show(&i.path));
        }
        if !not_files.is_empty() {
            let _ = writeln!(
                out,
                "  not a file     {} (symlinks and special files, never compared)",
                not_files.len()
            );
            for i in not_files {
                let what = if i.is_symlink {
                    "symlink"
                } else {
                    "special file"
                };
                let _ = writeln!(out, "    {} ({what})", show(&i.path));
            }
        }
        if lo.keep_marker {
            let _ = writeln!(
                out,
                "  note           has {KEEP_MARKER}: holds a file compress could not swap back"
            );
        }
        if let Some(why) = &lo.incomplete {
            let _ = writeln!(
                out,
                "  INCOMPLETE     {why}: unchecked files count as no copy"
            );
        }
        if let Some(why) = &lo.blocked {
            let _ = writeln!(out, "  BLOCKED        {why}; never removed");
        }
        out.push('\n');
    }
    let count = |k: Kind| leftovers.iter().filter(|l| l.kind == k).count();
    let removable: Vec<&Leftover> = leftovers.iter().filter(|l| l.blocked.is_none()).collect();
    let _ = writeln!(
        out,
        "found {} leftover dirs ({} staging, {} rescue): {} files, {}, {} on disk; {} blocked",
        leftovers.len(),
        count(Kind::Staging),
        count(Kind::Rescue),
        leftovers.iter().map(|l| l.files().count()).sum::<usize>(),
        human(leftovers.iter().map(Leftover::size).sum()),
        human(leftovers.iter().map(Leftover::allocated).sum()),
        leftovers.len() - removable.len()
    );
    let _ = writeln!(
        out,
        "A running compress has staging and rescue dirs of its own; a dir changed less than {}h \
         ago is blocked.",
        LIVE_GUARD / 3600
    );
    if removable.is_empty() {
        out.push_str("Nothing changed. Nothing can be removed.\n");
    } else {
        let _ = writeln!(
            out,
            "Nothing changed. Run '{apply_cmd}' to delete these {} dirs ({} on disk), files \
             without a copy included.",
            removable.len(),
            human(removable.iter().map(|l| l.allocated()).sum())
        );
    }
    out
}

/// Why `lo` must not be deleted now, if it must not: re-checked right before the delete.
fn recheck(lo: &Leftover) -> Result<(), String> {
    let name = lo
        .path
        .file_name()
        .map(OsStrExt::as_bytes)
        .unwrap_or_default();
    if !lo.kind.matches(name) {
        return Err("name does not match the exact pattern".into());
    }
    let m = fs::symlink_metadata(&lo.path).map_err(|e| format!("cannot stat: {e}"))?;
    if !m.is_dir() {
        return Err("no longer a directory".into());
    }
    if (m.dev(), m.ino()) != (lo.dev, lo.ino) {
        return Err("replaced since the scan".into());
    }
    let parent = lo.path.parent().unwrap_or(Path::new("/"));
    let pm = fs::symlink_metadata(parent).map_err(|e| format!("cannot stat parent: {e}"))?;
    if pm.dev() != m.dev() {
        return Err("on another filesystem than its parent".into());
    }
    Ok(())
}

/// Entries below `dir` (recursive, `lstat` only; 0 when it is gone).
fn count_entries(dir: &Path) -> u64 {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            n += 1;
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(e.path());
            }
        }
    }
    n
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Removal {
    pub removed: u64,
    /// Refused by the re-check, not or partially removed.
    pub failed: u64,
    /// Allocated bytes of the removed dirs, as scanned.
    pub freed: u64,
    /// Every dir left in place (blocked, refused or failed) with the reason, for the terminal.
    pub kept: Vec<(PathBuf, String)>,
}

/// Deletes every leftover not blocked, each only after [`recheck`]. Every dir left in place
/// gets a WARN and an entry in [`Removal::kept`].
pub fn remove(leftovers: &[Leftover], log: &Logger) -> Removal {
    let mut r = Removal::default();
    let keep = |r: &mut Removal, lo: &Leftover, why: String| {
        log.warn(format_args!(
            "clean: kept leftover dir {}: {why}",
            lo.path.display()
        ));
        r.kept.push((lo.path.clone(), why));
    };
    for lo in leftovers {
        if let Some(why) = &lo.blocked {
            keep(&mut r, lo, format!("blocked: {why}"));
            continue;
        }
        if let Err(why) = recheck(lo) {
            r.failed += 1;
            keep(&mut r, lo, format!("refused: {why}"));
            continue;
        }
        let before = count_entries(&lo.path);
        match fs::remove_dir_all(&lo.path) {
            Ok(()) => {
                r.removed += 1;
                r.freed += lo.allocated();
                log.info(format_args!(
                    "clean: removed leftover dir {} ({} files, {} on disk)",
                    lo.path.display(),
                    lo.files().count(),
                    human(lo.allocated())
                ));
            }
            Err(e) => {
                r.failed += 1;
                // Counted right before and after the delete attempt.
                let left = count_entries(&lo.path);
                let why = if left < before {
                    format!("partially removed ({left} of {before} entries left): {e}")
                } else {
                    format!("cannot remove: {e}")
                };
                keep(&mut r, lo, why);
            }
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    /// A temp dir with a logger, removed on drop (after undoing `chflags uchg`).
    struct Tmp {
        dir: PathBuf,
        log: Logger,
    }

    impl Tmp {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("acz-clean-{tag}-{}", std::process::id()));
            Self::cleanup(&dir);
            fs::create_dir_all(&dir).unwrap();
            let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
            Self { dir, log }
        }

        fn cleanup(dir: &Path) {
            if dir.exists() {
                let _ = std::process::Command::new("/usr/bin/chflags")
                    .args(["-R", "nouchg"])
                    .arg(dir)
                    .status();
                let _ = fs::remove_dir_all(dir);
            }
        }

        fn write(&self, rel: &str, data: &[u8]) -> PathBuf {
            let p = self.dir.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, data).unwrap();
            p
        }

        fn cand(&self, rel: &str) -> Candidate {
            let p = self.dir.join(rel);
            let m = fs::symlink_metadata(&p).unwrap();
            Candidate::new(p, Arc::from(self.dir.as_path()), &m)
        }

        fn scan(&self, rel: &str, kind: Kind) -> Leftover {
            scan(&self.dir.join(rel), kind, &self.log).unwrap()
        }

        fn log_text(&self) -> String {
            self.log.flush();
            fs::read_to_string(self.dir.join("log").join(crate::logger::LOG_NAME)).unwrap()
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            Self::cleanup(&self.dir);
        }
    }

    fn class_of<'a>(lo: &'a Leftover, name: &str) -> &'a FileClass {
        &lo.entries
            .iter()
            .find(|i| i.path.file_name().unwrap() == name)
            .unwrap()
            .class
    }

    /// Sets mtime/atime `secs` back; APFS lowers the birthtime with them (checked, so a
    /// non-APFS temp dir fails here instead of passing for the wrong reason).
    fn backdate(p: &Path, secs: u64) {
        let t = SystemTime::now() - Duration::from_secs(secs);
        File::open(p)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(t).set_accessed(t))
            .unwrap();
        let t_secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64;
        let birth = fs::symlink_metadata(p).unwrap().st_birthtime();
        assert!(birth <= t_secs, "birthtime of {} not lowered", p.display());
    }

    fn now() -> i64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    #[test]
    fn age_text() {
        assert_eq!(ago(0), "0s ago");
        assert_eq!(ago(59), "59s ago");
        assert_eq!(ago(12 * 60 + 5), "12m ago");
        assert_eq!(ago(4 * 3600 + 12 * 60), "4h 12m ago");
        assert_eq!(ago(3 * 86400 + 4 * 3600 + 59), "3d 4h ago");
        assert_eq!(ago(-300), "5m in the future");
        assert_eq!(ago(i64::MIN), "106751991167300d 15h in the future");
    }

    #[test]
    fn files_are_classified() {
        let t = Tmp::new("classify");
        t.write("v/a.txt", b"original a");
        t.write("v/sub/b.txt", b"original b");
        t.write("v/c.txt", b"same size 1");
        t.write("v/.acz-staging-1-2/a.txt", b"original a");
        t.write("v/.acz-staging-1-2/b.txt", b"original b");
        t.write("v/.acz-staging-1-2/c.txt", b"same size 2");
        t.write("v/.acz-staging-1-2/.BC.T_x", b"partial");
        t.write("v/.acz-rescue-3-4/a.txt", b"unique data");
        fs::create_dir(t.dir.join("v/.acz-rescue-3-4/.acz-keep")).unwrap();
        std::os::unix::fs::symlink("a.txt", t.dir.join("v/.acz-rescue-3-4/link")).unwrap();
        let candidates = ["v/a.txt", "v/sub/b.txt", "v/c.txt"].map(|r| t.cand(r));
        let mut los = vec![
            t.scan("v/.acz-staging-1-2", Kind::Staging),
            t.scan("v/.acz-rescue-3-4", Kind::Rescue),
        ];
        classify(&mut los, &candidates);
        let [staging, rescue] = &los[..] else {
            panic!()
        };
        assert_eq!(
            class_of(staging, "a.txt"),
            &FileClass::HasCopy(t.dir.join("v/a.txt"))
        );
        assert_eq!(
            class_of(staging, "b.txt"),
            &FileClass::HasCopy(t.dir.join("v/sub/b.txt"))
        );
        assert_eq!(class_of(staging, "c.txt"), &FileClass::NoCopy);
        assert_eq!(class_of(staging, ".BC.T_x"), &FileClass::DittoTemp);
        assert_eq!(class_of(rescue, "a.txt"), &FileClass::NoCopy);
        assert!(rescue.keep_marker && !staging.keep_marker);
        assert_eq!((staging.pid.as_str(), rescue.pid.as_str()), ("1", "3"));
        assert!(
            los.iter()
                .all(|l| l.blocked.is_none() && l.incomplete.is_none())
        );

        let text = render(&los, &t.dir, now(), "acz clean --apply");
        assert!(
            text.contains("~/v/.acz-rescue-3-4   rescue, pid 3\n"),
            "{text}"
        );
        assert!(
            text.contains("  no copy found  1, 11 B\n    ~/v/.acz-staging-1-2/c.txt\n"),
            "{text}"
        );
        // Directories (the marker included) are not files; a symlink is listed apart.
        assert!(
            text.contains(
                "  no copy found  1, 11 B\n    ~/v/.acz-rescue-3-4/a.txt\n  not a file     1 \
                 (symlinks and special files, never compared)\n    ~/v/.acz-rescue-3-4/link \
                 (symlink)\n"
            ),
            "{text}"
        );
        assert!(!text.contains(".acz-keep\n"), "{text}");
        assert!(text.contains("  has live copy  2, 20 B\n"), "{text}");
        assert!(text.contains("  ditto partial  1, 7 B\n"), "{text}");
        assert!(text.contains("s ago)\n"), "{text}");
        assert!(
            text.contains("Run 'acz clean --apply' to delete these 2 dirs"),
            "{text}"
        );
    }

    #[test]
    fn fresh_staging_and_rescue_are_blocked() {
        let t = Tmp::new("live");
        t.write("v/.acz-staging-1-2/a.txt", b"x");
        fs::create_dir_all(t.dir.join("v/.acz-staging-1-3")).unwrap();
        fs::create_dir_all(t.dir.join("v/.acz-staging-1-4")).unwrap();
        t.write("v/.acz-rescue-1-5/a.txt", b"x");
        fs::create_dir_all(t.dir.join("v/.acz-rescue-1-6")).unwrap();
        // 5h59m: still inside the 6h window.
        fs::create_dir_all(t.dir.join("v/.acz-rescue-1-7")).unwrap();
        backdate(&t.dir.join("v/.acz-staging-1-4"), 2 * 86400);
        backdate(&t.dir.join("v/.acz-rescue-1-6"), 2 * 86400);
        backdate(&t.dir.join("v/.acz-rescue-1-7"), 6 * 3600 - 60);
        let mut los = vec![
            t.scan("v/.acz-staging-1-2", Kind::Staging),
            t.scan("v/.acz-staging-1-3", Kind::Staging),
            t.scan("v/.acz-staging-1-4", Kind::Staging),
            t.scan("v/.acz-rescue-1-5", Kind::Rescue),
            t.scan("v/.acz-rescue-1-6", Kind::Rescue),
            t.scan("v/.acz-rescue-1-7", Kind::Rescue),
        ];
        block_live(&mut los, now());
        let blocked: Vec<bool> = los.iter().map(|l| l.blocked.is_some()).collect();
        assert_eq!(blocked, [true, true, false, true, false, true]);
        assert!(
            los[3]
                .blocked
                .as_ref()
                .unwrap()
                .starts_with("changed less than 6h ago (last change "),
            "{los:?}"
        );
        let r = remove(&los, &t.log);
        assert_eq!((r.removed, r.failed, r.kept.len()), (2, 0, 4));
        assert!(t.dir.join("v/.acz-staging-1-2/a.txt").exists());
        assert!(t.dir.join("v/.acz-staging-1-3").exists());
        assert!(!t.dir.join("v/.acz-staging-1-4").exists());
        assert!(t.dir.join("v/.acz-rescue-1-5/a.txt").exists());
        assert!(!t.dir.join("v/.acz-rescue-1-6").exists());
        assert!(t.dir.join("v/.acz-rescue-1-7").exists());
        let log = t.log_text();
        assert!(log.contains("WARN clean: kept leftover dir"), "{log}");
        assert!(log.contains("INFO clean: removed leftover dir"), "{log}");
    }

    #[test]
    fn unblocked_dir_with_contents_is_removed() {
        let t = Tmp::new("rm");
        t.write("v/.acz-rescue-1-2/a.txt", b"unique");
        t.write("v/.acz-rescue-1-2/.acz-keep/x", b"x");
        let los = [t.scan("v/.acz-rescue-1-2", Kind::Rescue)];
        let r = remove(&los, &t.log);
        assert_eq!((r.removed, r.failed), (1, 0));
        assert_eq!(r.freed, los[0].allocated());
        assert!(!t.dir.join("v/.acz-rescue-1-2").exists());
        assert!(t.dir.join("v").exists());
    }

    #[test]
    fn exhausted_verify_budget_marks_incomplete() {
        let t = Tmp::new("budget");
        t.write("v/a.txt", b"original a");
        t.write("v/.acz-rescue-1-2/a.txt", b"original a");
        let mut los = vec![t.scan("v/.acz-rescue-1-2", Kind::Rescue)];
        classify_with(&mut los, &[t.cand("v/a.txt")], 1);
        assert_eq!(class_of(&los[0], "a.txt"), &FileClass::NoCopy);
        assert_eq!(
            los[0].incomplete.as_deref(),
            Some("verify budget exhausted")
        );
        let text = render(&los, &t.dir, now(), "acz clean --apply");
        assert!(
            text.contains(
                "  INCOMPLETE     verify budget exhausted: unchecked files count as no copy\n"
            ),
            "{text}"
        );
        // With the budget it is found.
        classify(&mut los, &[t.cand("v/a.txt")]);
        assert_eq!(
            class_of(&los[0], "a.txt"),
            &FileClass::HasCopy(t.dir.join("v/a.txt"))
        );
    }

    #[test]
    fn flagged_entry_blocks_the_dir() {
        let t = Tmp::new("flag");
        let f = t.write("v/.acz-rescue-1-2/sub/locked.txt", b"x");
        assert!(
            std::process::Command::new("/usr/bin/chflags")
                .arg("uchg")
                .arg(&f)
                .status()
                .unwrap()
                .success()
        );
        let los = [t.scan("v/.acz-rescue-1-2", Kind::Rescue)];
        assert!(
            los[0].blocked.as_ref().unwrap().contains("flag uchg"),
            "{los:?}"
        );
        let r = remove(&los, &t.log);
        assert_eq!((r.removed, r.failed, r.kept.len()), (0, 0, 1));
        assert!(r.kept[0].1.starts_with("blocked: "), "{r:?}");
        assert!(f.exists());
    }

    #[test]
    fn dir_replaced_by_symlink_is_refused() {
        let t = Tmp::new("swap");
        t.write("v/.acz-rescue-1-2/a.txt", b"x");
        let target = t.write("other/keep.txt", b"keep");
        let los = [t.scan("v/.acz-rescue-1-2", Kind::Rescue)];
        let p = t.dir.join("v/.acz-rescue-1-2");
        fs::remove_dir_all(&p).unwrap();
        std::os::unix::fs::symlink(t.dir.join("other"), &p).unwrap();
        let r = remove(&los, &t.log);
        assert_eq!((r.removed, r.failed), (0, 1));
        assert!(fs::symlink_metadata(&p).unwrap().is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        assert!(t.log_text().contains("no longer a directory"));
    }

    #[test]
    fn recreated_dir_is_refused() {
        let t = Tmp::new("ino");
        t.write("v/.acz-rescue-1-2/a.txt", b"x");
        let los = [t.scan("v/.acz-rescue-1-2", Kind::Rescue)];
        let p = t.dir.join("v/.acz-rescue-1-2");
        fs::remove_dir_all(&p).unwrap();
        let again = t.write("v/.acz-rescue-1-2/new.txt", b"new");
        let r = remove(&los, &t.log);
        assert_eq!((r.removed, r.failed), (0, 1));
        assert_eq!(fs::read(&again).unwrap(), b"new");
        assert!(t.log_text().contains("replaced since the scan"));
    }

    #[test]
    fn name_outside_the_regex_is_refused() {
        let t = Tmp::new("name");
        fs::create_dir_all(t.dir.join("v/.acz-rescue-x")).unwrap();
        let los = [t.scan("v/.acz-rescue-x", Kind::Rescue)];
        let r = remove(&los, &t.log);
        assert_eq!((r.removed, r.failed), (0, 1));
        assert!(t.dir.join("v/.acz-rescue-x").exists());
    }
}
