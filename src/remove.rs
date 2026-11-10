//! `trash` and `purge`: every file or directory matched by the rule's patterns is moved to the
//! Trash or deleted for good, a directory as a whole. Before anything happens each matched item
//! is checked: never a protected location or an ancestor of one, never anything holding the
//! tool's own config, log or state, never an item with a flagged (uchg/schg/...) or dataless
//! entry, never a symlink. Right before acting the item must still be the same file or directory
//! the walk saw. Every item acted on is appended to the journal `removed.tsv`.

use crate::clean::flag_reason;
use crate::logger::{self, Logger};
use crate::progress::{abbreviate_home, human};
use crate::sys;
use crate::walk::{Guard, Matched, Matcher, RESCUE_PREFIX, STAGING_PREFIX};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Journal of every item trashed or purged, in the log dir.
pub const JOURNAL_NAME: &str = "removed.tsv";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Trash,
    Purge,
    /// Stale build units of Cargo target directories, deleted for good.
    CargoClean,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::Trash => "trash",
            Action::Purge => "purge",
            Action::CargoClean => "cargo-clean",
        }
    }

    fn done(self) -> &'static str {
        match self {
            Action::Trash => "moved to the Trash",
            Action::Purge | Action::CargoClean => "deleted",
        }
    }
}

/// A matched item with what the checks found.
#[derive(Debug)]
pub struct Item {
    pub path: PathBuf,
    pub is_dir: bool,
    dev: u64,
    ino: u64,
    /// Allocated bytes of the item and everything below it.
    pub bytes: u64,
    /// Regular files in it (1 for a file).
    pub files: u64,
    /// The pattern that matched.
    pub rule: String,
    /// Why the item is never acted on, if it is not.
    pub blocked: Option<String>,
}

/// Where items may be and what they must not be or contain, besides the guard's protected
/// locations. Compared on their on-disk form (symlinks resolved, `/System/Volumes/Data`
/// stripped) ignoring case.
pub struct Keep<'a> {
    /// An item must lie strictly below one of these: the home directory and the accepted
    /// `allow_volumes` mount points.
    pub scope: &'a [PathBuf],
    /// The tool's own config, log, state and cache directories.
    pub own: &'a [PathBuf],
    /// `~/.Trash`: never trashed into itself.
    pub trash_dir: &'a Path,
}

/// On-disk form of `p` for comparisons: realpath when it exists, data prefix stripped,
/// lowercased (APFS volumes are usually case-insensitive).
fn disk_form(p: &Path) -> PathBuf {
    let real = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let real = match real.strip_prefix("/System/Volumes/Data") {
        Ok(r) => Path::new("/").join(r),
        Err(_) => real,
    };
    PathBuf::from(real.to_string_lossy().to_lowercase())
}

/// Checks every matched item (sorted by path, as the walk and cargo-clean give them); nothing
/// is changed.
pub fn plan(
    matched: Vec<Matched>,
    matcher: &Matcher,
    guard: &Guard,
    keep: &Keep<'_>,
    action: Action,
) -> Vec<Item> {
    let trash_dev = fs::metadata(keep.trash_dir).ok().map(|m| m.dev());
    let items = matched
        .into_iter()
        .map(|m| {
            let rule = matcher.matching_pattern(&m.path).unwrap_or("?").to_owned();
            let mut item = Item {
                path: m.path,
                is_dir: m.is_dir,
                dev: m.dev,
                ino: m.ino,
                bytes: 0,
                files: 0,
                rule,
                blocked: None,
            };
            item.blocked = refused(&item.path, guard, keep, action)
                .or_else(|| foreign(&item.path, item.dev, guard))
                .or_else(|| match (action, trash_dev) {
                    (Action::Trash, None) => Some(format!(
                        "the Trash {} does not exist",
                        keep.trash_dir.display()
                    )),
                    (Action::Trash, Some(d)) if d != item.dev => {
                        Some("is on another volume than the Trash".into())
                    }
                    _ => None,
                })
                .or_else(|| measure(&mut item, matcher));
            item
        })
        .collect::<Vec<Item>>();
    drop_covered(items)
}

/// Drops every item inside an earlier directory item that will be acted on (it goes with that
/// directory). `items` must be sorted by path: everything below a directory then directly
/// follows it, so only the last such directory is compared. Linear, where comparing with every
/// kept item was quadratic (minutes for 10^5 items).
fn drop_covered(items: Vec<Item>) -> Vec<Item> {
    debug_assert!(items.windows(2).all(|w| w[0].path <= w[1].path));
    let mut kept = Vec::with_capacity(items.len());
    let mut cover: Option<PathBuf> = None;
    for item in items {
        if cover.as_ref().is_some_and(|c| item.path.starts_with(c)) {
            continue;
        }
        if item.is_dir && item.blocked.is_none() {
            cover = Some(item.path.clone());
        }
        kept.push(item);
    }
    kept
}

/// Lexical and on-disk refusals: [`location_refused`], then the Trash.
fn refused(path: &Path, guard: &Guard, keep: &Keep<'_>, action: Action) -> Option<String> {
    if let Some(why) = location_refused(path, guard, keep) {
        return Some(why);
    }
    let disk = disk_form(path);
    let trash = disk_form(keep.trash_dir);
    if action == Action::Trash && disk.starts_with(&trash) {
        return Some("is already in the Trash".into());
    }
    if trash.starts_with(&disk) {
        return Some(format!(
            "is or contains the Trash {}",
            keep.trash_dir.display()
        ));
    }
    None
}

/// Lexical and on-disk refusals besides the Trash: `.`/`..`, outside the scope, protected
/// locations and their ancestors, the tool's own directories.
pub(crate) fn location_refused(path: &Path, guard: &Guard, keep: &Keep<'_>) -> Option<String> {
    use std::path::Component::{CurDir, ParentDir};
    if path.components().any(|c| matches!(c, CurDir | ParentDir)) {
        return Some("contains `.` or `..`".into());
    }
    let real = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    for p in [path, real.as_path()] {
        if let Some(why) = guard.removal_refused(p) {
            return Some(why);
        }
    }
    let disk = disk_form(path);
    let inside = keep.scope.iter().any(|s| {
        let s = disk_form(s);
        disk != s && disk.starts_with(&s)
    });
    if !inside {
        return Some("is not below the home directory or an allow_volumes mount".into());
    }
    if let Some(own) = keep.own.iter().find(|o| {
        let o = disk_form(o);
        o.starts_with(&disk) || disk.starts_with(&o)
    }) {
        return Some(format!(
            "is or contains {} (used by this tool)",
            own.display()
        ));
    }
    None
}

/// An item `path` on device `dev` (as walked) on another filesystem than its parent (a mount
/// point), or on a filesystem the guard does not allow.
pub(crate) fn foreign(path: &Path, dev: u64, guard: &Guard) -> Option<String> {
    let parent = path.parent().unwrap_or(Path::new("/"));
    match fs::symlink_metadata(parent) {
        Ok(pm) if pm.dev() != dev => {
            return Some("is on another filesystem than its parent (a mount point)".into());
        }
        Ok(_) => {}
        Err(e) => return Some(format!("cannot stat its parent: {e}")),
    }
    guard.filesystem_allowed(path).err()
}

/// Sizes the item with `lstat` only (symlinks never followed) and returns why it is blocked:
/// a flagged or dataless entry, an entry on another filesystem, an entry an exclude matches (an
/// exclude wins over the match of a directory above it), anything unreadable.
fn measure(item: &mut Item, matcher: &Matcher) -> Option<String> {
    let root = match fs::symlink_metadata(&item.path) {
        Ok(m) => m,
        Err(e) => return Some(format!("cannot stat: {e}")),
    };
    if root.file_type().is_symlink() || root.dev() != item.dev || root.ino() != item.ino {
        return Some("changed since the walk".into());
    }
    if let Some(why) = flag_reason(&item.path, &root) {
        return Some(why);
    }
    item.bytes = root.blocks() * 512;
    if !item.is_dir {
        item.files = 1;
        return None;
    }
    // Excludes are also tried on the on-disk spelling (the item may be reached via a symlink).
    let real = fs::canonicalize(&item.path).ok();
    let mut blocked = None;
    let mut stack = vec![item.path.clone()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                blocked.get_or_insert(format!("cannot read {}: {e}", dir.display()));
                continue;
            }
        };
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    blocked.get_or_insert(format!("cannot read {}: {e}", dir.display()));
                    break;
                }
            };
            let p = entry.path();
            // `DirEntry::metadata` is an lstat.
            let m = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    blocked.get_or_insert(format!("cannot stat {}: {e}", p.display()));
                    continue;
                }
            };
            let name = entry.file_name();
            let name = name.as_encoded_bytes();
            if name.starts_with(STAGING_PREFIX.as_bytes())
                || name.starts_with(RESCUE_PREFIX.as_bytes())
            {
                blocked.get_or_insert(format!(
                    "contains {} (left by compress; see `clean`)",
                    p.display()
                ));
            }
            if let Some(why) = flag_reason(&p, &m) {
                blocked.get_or_insert(why);
                // A flagged or dataless directory is never entered.
                continue;
            }
            let real_p = real
                .as_ref()
                .and_then(|r| p.strip_prefix(&item.path).ok().map(|rel| r.join(rel)));
            if matcher.excludes(&p, m.is_dir())
                || real_p.is_some_and(|rp| matcher.excludes(&rp, m.is_dir()))
            {
                blocked.get_or_insert(format!("contains excluded {}", p.display()));
            }
            if m.dev() != item.dev {
                blocked.get_or_insert(format!("{} is on another filesystem", p.display()));
                continue;
            }
            item.bytes += m.blocks() * 512;
            if m.is_file() {
                item.files += 1;
            }
            if m.is_dir() {
                stack.push(p);
            }
        }
    }
    blocked
}

/// The preview: every item with its size, blocked ones with the reason, totals and the command
/// that would do it.
pub fn render(items: &[Item], home: &Path, action: Action, apply_cmd: &str) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let (ready, blocked): (Vec<&Item>, Vec<&Item>) =
        items.iter().partition(|i| i.blocked.is_none());
    for i in &ready {
        let kind = if i.is_dir { "dir " } else { "file" };
        let _ = writeln!(
            s,
            "{kind} {}   {} ({} files)   rule {}",
            abbreviate_home(&i.path, Some(home)),
            human(i.bytes),
            i.files,
            i.rule
        );
    }
    for i in &blocked {
        let _ = writeln!(
            s,
            "BLOCKED {}: {}",
            abbreviate_home(&i.path, Some(home)),
            i.blocked.as_deref().unwrap_or_default()
        );
    }
    let total: u64 = ready.iter().map(|i| i.bytes).sum();
    let _ = writeln!(
        s,
        "\nwould be {}: {} items, {} ({} blocked)",
        action.done(),
        ready.len(),
        human(total),
        blocked.len()
    );
    if !ready.is_empty() {
        let _ = writeln!(s, "Nothing changed. Run '{apply_cmd}' to do it.");
    }
    s
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub done: u64,
    pub failed: u64,
    pub blocked: u64,
    /// Allocated bytes of the items acted on, as measured.
    pub bytes: u64,
}

/// A free name for `name` in `dir`: the name itself, else `name <stamp>`, else with a counter.
fn trash_dest(dir: &Path, name: &std::ffi::OsStr, stamp: &str, attempt: u32) -> PathBuf {
    if attempt == 0 {
        return dir.join(name);
    }
    let mut n = name.to_owned();
    n.push(format!(" {stamp}"));
    if attempt > 1 {
        n.push(format!(" {attempt}"));
    }
    dir.join(n)
}

/// Moves `path` into `trash_dir` without ever replacing an entry there or following a symlink
/// in either path; returns where it went.
fn move_to_trash(path: &Path, trash_dir: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().unwrap_or(path.as_os_str());
    let stamp = logger::timestamp(SystemTime::now()).replace(':', ".");
    for attempt in 0..100 {
        let dest = trash_dest(trash_dir, name, &stamp, attempt);
        match sys::rename_excl(path, &dest) {
            Ok(()) => return Ok(dest),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("no free name in the Trash"))
}

/// Tabs and line feeds in a journal field are written as `\t` and `\n`.
fn field(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

/// Entries below `path` (0 when it is gone), to tell a partial delete from none.
fn count_entries(path: &Path) -> u64 {
    let mut n = 0;
    let mut stack = vec![path.to_path_buf()];
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

/// Acts on every item not blocked. Right before, the item's realpath must still be the same,
/// unflagged file or directory (device, inode, type); the Trash move then uses that realpath
/// and never follows a symlink. Nothing is done when the journal cannot be opened. Every item
/// attempted gets a journal line (status `done`, `failed: ...` or `partial: ...`), one INFO line
/// when done, one WARN when not.
pub fn apply(
    items: &[Item],
    action: Action,
    trash_dir: &Path,
    journal: &Path,
    log: &Logger,
) -> Result<Outcome, String> {
    let mut journal_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal)
        .map_err(|e| {
            format!(
                "cannot open journal {}: {e}; nothing was changed",
                journal.display()
            )
        })?;
    let mut record = |i: &Item, dest: Option<&Path>, status: &str| {
        let line = format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            logger::timestamp(SystemTime::now()),
            action.name(),
            field(&i.path.to_string_lossy()),
            i.bytes,
            field(&i.rule),
            dest.map_or_else(|| "-".to_owned(), |d| field(&d.to_string_lossy())),
            field(status)
        );
        let ok = journal_file.write_all(line.as_bytes()).is_ok();
        if !ok {
            log.error(format_args!(
                "cannot write {}; the remaining items are not attempted",
                journal.display()
            ));
        }
        ok
    };
    let trash_real = fs::canonicalize(trash_dir).unwrap_or_else(|_| trash_dir.to_path_buf());
    let mut out = Outcome::default();
    for i in items {
        if let Some(why) = &i.blocked {
            out.blocked += 1;
            log.info(format_args!(
                "{}: blocked {}: {why}",
                action.name(),
                i.path.display()
            ));
            continue;
        }
        let real = fs::canonicalize(&i.path).ok().filter(|r| {
            fs::symlink_metadata(r).is_ok_and(|m| {
                !m.file_type().is_symlink()
                    && m.is_dir() == i.is_dir
                    && m.dev() == i.dev
                    && m.ino() == i.ino
                    && flag_reason(r, &m).is_none()
            })
        });
        let before = match &real {
            Some(r) if i.is_dir => count_entries(r),
            _ => 0,
        };
        let result = match &real {
            None => Err(io::Error::other("changed since it was checked")),
            Some(r) => match action {
                Action::Purge | Action::CargoClean if i.is_dir => {
                    fs::remove_dir_all(r).map(|()| None)
                }
                Action::Purge | Action::CargoClean => fs::remove_file(r).map(|()| None),
                Action::Trash => move_to_trash(r, &trash_real).map(Some),
            },
        };
        match result {
            Ok(dest) => {
                out.done += 1;
                out.bytes += i.bytes;
                let to = dest
                    .as_ref()
                    .map(|d| format!(" -> {}", d.display()))
                    .unwrap_or_default();
                log.info(format_args!(
                    "{}: {} {}{to} ({} files, {}, rule {})",
                    action.name(),
                    action.done(),
                    i.path.display(),
                    i.files,
                    human(i.bytes),
                    i.rule
                ));
                if !record(i, dest.as_deref(), "done") {
                    // Counted as failed: the run must not look green.
                    out.failed += 1;
                    break;
                }
            }
            Err(e) => {
                out.failed += 1;
                let left = match &real {
                    Some(r) if i.is_dir => count_entries(r),
                    _ => before,
                };
                let status = if left < before {
                    format!("partial: {left} of {before} entries left: {e}")
                } else {
                    format!("failed: {e}")
                };
                log.warn(format_args!(
                    "{}: {} {}",
                    action.name(),
                    status,
                    i.path.display()
                ));
                eprintln!("{}: {status}: {}", action.name(), i.path.display());
                if !record(i, None, &status) {
                    // Counted as failed: the run must not look green.
                    out.failed += 1;
                    break;
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_covered_keeps_only_outermost_actionable() {
        let it = |p: &str, is_dir: bool, blocked: bool| Item {
            path: PathBuf::from(p),
            is_dir,
            dev: 0,
            ino: 0,
            bytes: 0,
            files: 0,
            rule: String::new(),
            blocked: blocked.then(|| "x".to_owned()),
        };
        let mut items = vec![
            it("/d/a", true, false),
            it("/d/a/x", false, false),
            it("/d/a-b", true, false),
            it("/d/a-b/y", false, false),
            it("/d/b", true, true),
            it("/d/b/z", true, false),
            it("/d/b/z/w", false, false),
            it("/d/c", false, false),
        ];
        items.sort_by(|a, b| a.path.cmp(&b.path));
        // The quadratic reference: covered by any earlier kept actionable directory.
        let reference: Vec<PathBuf> = items
            .iter()
            .fold(Vec::<&Item>::new(), |mut kept, i| {
                if !kept
                    .iter()
                    .any(|k| k.is_dir && k.blocked.is_none() && i.path.starts_with(&k.path))
                {
                    kept.push(i);
                }
                kept
            })
            .iter()
            .map(|i| i.path.clone())
            .collect();
        let got: Vec<PathBuf> = drop_covered(items).into_iter().map(|i| i.path).collect();
        assert_eq!(got, reference);
        assert_eq!(
            got,
            ["/d/a", "/d/a-b", "/d/b", "/d/b/z", "/d/c"].map(PathBuf::from)
        );
    }
    use crate::walk::{self, Matcher};

    struct Tmp {
        dir: PathBuf,
        log: Logger,
    }

    impl Tmp {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("acz-remove-{tag}-{}", std::process::id()));
            let _ = std::process::Command::new("/usr/bin/chflags")
                .args(["-R", "nouchg"])
                .arg(&dir)
                .status();
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
            Self { dir, log }
        }

        fn write(&self, rel: &str) -> PathBuf {
            let p = self.dir.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, "data").unwrap();
            p
        }

        fn items(
            &self,
            patterns: &[&str],
            exclude: &[&str],
            action: Action,
            own: &[PathBuf],
        ) -> Vec<Item> {
            let pat: Vec<String> = patterns
                .iter()
                .map(|p| self.dir.join(p).to_string_lossy().into_owned())
                .collect();
            let exc: Vec<String> = exclude.iter().map(|s| s.to_string()).collect();
            let m = Matcher::new(&pat, &exc, None).unwrap();
            let guard = Guard::new(Path::new("/nonexistent-home"), &[], &self.log).unwrap();
            let w = walk::walk_entries(&m, &guard, true, 2, &self.log);
            let trash = self.dir.join("Trash");
            let scope = [self.dir.clone()];
            let keep = Keep {
                scope: &scope,
                own,
                trash_dir: &trash,
            };
            plan(w.matched, &m, &guard, &keep, action)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::process::Command::new("/usr/bin/chflags")
                .args(["-R", "nouchg"])
                .arg(&self.dir)
                .status();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn names(items: &[Item], root: &Path) -> Vec<String> {
        items
            .iter()
            .map(|i| {
                i.path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn matched_dirs_go_whole_and_excludes_win() {
        let t = Tmp::new("plan");
        t.write("d/old/a.txt");
        t.write("d/old/sub/b.txt");
        t.write("d/keep.log");
        t.write("d/x.tmp");
        let items = t.items(&["d/*"], &["*.log"], Action::Purge, &[]);
        assert_eq!(names(&items, &t.dir), ["d/old", "d/x.tmp"]);
        let old = &items[0];
        assert!(old.is_dir && old.blocked.is_none());
        assert_eq!(old.files, 2);
        assert!(old.rule.ends_with("d/*"));
    }

    #[test]
    fn flagged_entry_blocks_its_dir_and_own_dirs_are_kept() {
        let t = Tmp::new("blocked");
        let f = t.write("d/locked/f.txt");
        t.write("d/conf/c.toml");
        assert!(
            std::process::Command::new("/usr/bin/chflags")
                .arg("uchg")
                .arg(&f)
                .status()
                .unwrap()
                .success()
        );
        let items = t.items(&["d/*"], &[], Action::Purge, &[t.dir.join("d/conf")]);
        let why = |n: &str| {
            items
                .iter()
                .find(|i| i.path.ends_with(n))
                .unwrap()
                .blocked
                .clone()
                .unwrap()
        };
        assert!(why("conf").contains("used by this tool"), "{}", why("conf"));
        assert!(why("locked").contains("uchg"), "{}", why("locked"));
        let out = apply(
            &items,
            Action::Purge,
            &t.dir.join("Trash"),
            &t.dir.join(JOURNAL_NAME),
            &t.log,
        )
        .unwrap();
        assert_eq!(
            out,
            Outcome {
                done: 0,
                failed: 0,
                blocked: 2,
                bytes: 0
            }
        );
        assert!(f.exists());
    }

    #[test]
    fn purge_and_trash_act_and_journal() {
        let t = Tmp::new("apply");
        t.write("d/old/a.txt");
        let file = t.write("d/x.tmp");
        let trash = t.dir.join("Trash");
        fs::create_dir(&trash).unwrap();
        // A name already in the Trash is never replaced.
        fs::write(trash.join("x.tmp"), "older").unwrap();
        let journal = t.dir.join(JOURNAL_NAME);
        let items = t.items(&["d/x.tmp"], &[], Action::Trash, &[]);
        let out = apply(&items, Action::Trash, &trash, &journal, &t.log).unwrap();
        assert_eq!((out.done, out.failed), (1, 0));
        assert!(!file.exists());
        assert_eq!(fs::read_to_string(trash.join("x.tmp")).unwrap(), "older");
        let moved: Vec<_> = fs::read_dir(&trash)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(moved.len(), 2);
        let items = t.items(&["d/*"], &[], Action::Purge, &[]);
        let out = apply(&items, Action::Purge, &trash, &journal, &t.log).unwrap();
        assert_eq!((out.done, out.failed), (1, 0));
        assert!(!t.dir.join("d/old").exists());
        let j = fs::read_to_string(&journal).unwrap();
        let lines: Vec<&str> = j.lines().collect();
        assert_eq!(lines.len(), 2, "{j}");
        assert!(
            lines[0].contains("\ttrash\t") && lines[0].contains("/Trash/x.tmp "),
            "{j}"
        );
        assert!(
            lines[1].contains("\tpurge\t") && lines[1].ends_with("\t-\tdone"),
            "{j}"
        );
    }

    #[test]
    fn exclude_inside_a_matched_dir_and_out_of_scope_items_block() {
        let t = Tmp::new("excl");
        t.write("d/app/keep/k.txt");
        t.write("d/app/a.txt");
        let keep_dir = t.dir.join("d/app/keep").to_string_lossy().into_owned();
        let exc = format!("{keep_dir}/**");
        let items = t.items(&["d/*"], &[&exc], Action::Purge, &[]);
        let why = items[0].blocked.clone().unwrap();
        assert!(why.contains("contains excluded"), "{why}");
        // A pattern matching the scope root itself is refused.
        let items = t.items(&[""], &[], Action::Purge, &[]);
        assert!(items.iter().all(|i| i.blocked.is_some()), "{items:?}");
    }

    #[test]
    fn items_inside_own_dirs_and_dirs_with_rescue_copies_are_refused() {
        let t = Tmp::new("own-in");
        t.write("d/conf/c.toml");
        t.write("e/proj/.acz-rescue-1-2/f");
        t.write("e/proj/a.txt");
        let items = t.items(&["d/conf/*"], &[], Action::Purge, &[t.dir.join("d/conf")]);
        assert!(!items.is_empty());
        assert!(
            items.iter().all(|i| i
                .blocked
                .as_deref()
                .is_some_and(|w| w.contains("used by this tool"))),
            "{items:?}"
        );
        let items = t.items(&["e/*"], &[], Action::Purge, &[]);
        let why = items[0].blocked.clone().unwrap();
        assert!(why.contains(".acz-rescue-1-2"), "{why}");
    }

    #[test]
    fn changed_item_is_not_acted_on() {
        let t = Tmp::new("changed");
        let f = t.write("d/x.tmp");
        let items = t.items(&["d/x.tmp"], &[], Action::Purge, &[]);
        fs::remove_file(&f).unwrap();
        t.write("d/x.tmp");
        let out = apply(
            &items,
            Action::Purge,
            &t.dir.join("Trash"),
            &t.dir.join(JOURNAL_NAME),
            &t.log,
        )
        .unwrap();
        assert_eq!((out.done, out.failed), (0, 1));
        assert!(f.exists());
    }

    #[test]
    fn protected_locations_and_their_ancestors_are_refused() {
        let log_dir = std::env::temp_dir().join(format!("acz-remove-guard-{}", std::process::id()));
        let log = Logger::open(&log_dir, 1 << 20, 1, false).unwrap();
        let home = Path::new("/Users/someone");
        // `/` and the data volume are never accepted as allow_volumes: CloudStorage stays refused.
        let lifted = ["/".to_owned(), "/System/Volumes/Data".to_owned()];
        let guard = Guard::new(home, &lifted, &log).unwrap();
        assert!(guard.allowed_mounts().is_empty());
        assert!(
            guard
                .removal_refused(Path::new("/Users/someone/Library/CloudStorage/x/y"))
                .is_some()
        );
        for p in [
            "/Users/someone",
            "/Users/someone/Downloads",
            "/Users",
            "/",
            "/System/Library",
        ] {
            assert!(guard.removal_refused(Path::new(p)).is_some(), "{p}");
        }
        assert!(
            guard
                .removal_refused(Path::new("/Users/someone/Downloads/x.dmg"))
                .is_none()
        );
        let _ = fs::remove_dir_all(&log_dir);
    }
}
