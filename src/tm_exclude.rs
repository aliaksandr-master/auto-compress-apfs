//! `tm-exclude`: every directory matched by the rule's patterns is excluded from Time Machine
//! backups by creating the sticky exclusion xattr that `tmutil addexclusion` writes (tmutil is
//! never executed). Matched files are ignored. Before anything happens each directory is
//! checked like a trash/purge item: never a protected location or an ancestor of one, never
//! anything holding the tool's own config, log or state, never a mount point, a flagged
//! (uchg/schg/...) or dataless directory, one owned by another user, or a symlink. The xattr is
//! only ever created, never replaced or removed: any existing value counts as already excluded.

use crate::clean::flag_reason;
use crate::logger::Logger;
use crate::progress::abbreviate_home;
use crate::remove::{Checker, Keep};
use crate::sys;
use crate::walk::{Guard, Matched, Matcher};
use std::ffi::CStr;
use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// The sticky Time Machine exclusion xattr.
pub const XATTR: &CStr = c"com.apple.metadata:com_apple_backup_excludeItem";

/// Its value as `tmutil addexclusion` writes it: the binary plist string `com.apple.backupd`.
pub const VALUE: [u8; 61] = [
    0x62, 0x70, 0x6C, 0x69, 0x73, 0x74, 0x30, 0x30, 0x5F, 0x10, 0x11, 0x63, 0x6F, 0x6D, 0x2E, 0x61,
    0x70, 0x70, 0x6C, 0x65, 0x2E, 0x62, 0x61, 0x63, 0x6B, 0x75, 0x70, 0x64, 0x08, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1C,
];

/// How to undo an exclusion; the tool never removes one.
const UNDO: &str = "Exclusions are never removed by this tool; undo one with \
    `tmutil removeexclusion <path>` or `xattr -d com.apple.metadata:com_apple_backup_excludeItem \
    <path>`. Each excluded path is an INFO line in the log.";

/// What the checks found for a matched directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Would be (or will be) excluded.
    Pending,
    /// Already carries the exclusion xattr (whatever its value).
    Already,
    /// Never touched, with the reason.
    Blocked(String),
    /// A check failed, with the error.
    Error(String),
}

/// A matched directory with what the checks found.
#[derive(Debug)]
pub struct Item {
    pub path: PathBuf,
    dev: u64,
    ino: u64,
    /// The pattern that matched.
    pub rule: String,
    pub state: State,
}

/// Opens the directory `path` read-only without following a symlink in its last component.
fn open_dir(path: &Path) -> io::Result<File> {
    File::options()
        .read(true)
        .custom_flags(sys::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
}

/// Opens `path` and checks the directory behind the fd: still the walked one (device and
/// inode), not flagged or dataless, owned by `euid`. A directory gone, replaced or not
/// accessible is blocked, not an error: churn between walk and apply must not fail a run.
fn open_checked(path: &Path, dev: u64, ino: u64, euid: u32) -> Result<(File, Metadata), State> {
    let f = match open_dir(path) {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(libc::EDEADLK) => {
            return Err(State::Blocked("dataless".into()));
        }
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(libc::ENOENT | libc::EACCES | libc::ELOOP | libc::ENOTDIR)
            ) =>
        {
            return Err(State::Blocked(format!("cannot open: {e}")));
        }
        Err(e) => return Err(State::Error(format!("cannot open: {e}"))),
    };
    let m = match f.metadata() {
        Ok(m) => m,
        Err(e) => return Err(State::Error(format!("cannot stat: {e}"))),
    };
    if m.dev() != dev || m.ino() != ino {
        return Err(State::Blocked("changed since the walk".into()));
    }
    if let Some(why) = flag_reason(path, &m) {
        return Err(State::Blocked(why));
    }
    if m.uid() != euid {
        return Err(State::Blocked("not owned by you".into()));
    }
    Ok((f, m))
}

/// Checks every matched directory (files are dropped); nothing is changed. A directory inside
/// an earlier one that is or will be excluded is dropped: the exclusion covers it.
pub fn plan(
    matched: Vec<Matched>,
    matcher: &Matcher,
    guard: &Guard,
    keep: &Keep<'_>,
    euid: u32,
    log: &Logger,
) -> Vec<Item> {
    let total = matched.len();
    let dirs: Vec<Matched> = matched.into_iter().filter(|m| m.is_dir).collect();
    if dirs.len() < total {
        log.debug(format_args!(
            "tm-exclude: {} matched files ignored (only directories are excluded)",
            total - dirs.len()
        ));
    }
    let mut kept: Vec<Item> = Vec::new();
    let checker = Checker::new(keep);
    for m in dirs {
        // Sorted by path: a directory inside an earlier excluded one goes with it, unchecked.
        let covered = kept.iter().any(|k| {
            matches!(k.state, State::Pending | State::Already) && m.path.starts_with(&k.path)
        });
        if covered {
            continue;
        }
        let rule = matcher.matching_pattern(&m.path).unwrap_or("?").to_owned();
        let state = check(&m, guard, &checker, euid);
        kept.push(Item {
            path: m.path,
            dev: m.dev,
            ino: m.ino,
            rule,
            state,
        });
    }
    kept
}

fn check(m: &Matched, guard: &Guard, checker: &Checker<'_>, euid: u32) -> State {
    if let Some(why) = checker
        .location_refused(&m.path, guard)
        .or_else(|| checker.foreign(&m.path, m.dev, guard))
    {
        return State::Blocked(why);
    }
    let (f, _) = match open_checked(&m.path, m.dev, m.ino, euid) {
        Ok(v) => v,
        Err(state) => return state,
    };
    match sys::fget_xattr(&f, XATTR, false) {
        Ok(Some(_)) => State::Already,
        Ok(None) => State::Pending,
        Err(e) => State::Error(format!("cannot read {}: {e}", XATTR.to_string_lossy())),
    }
}

/// The preview: every directory that would be excluded, blocked ones and errors with the
/// reason, totals and the command that would do it.
pub fn render(items: &[Item], home: &Path, apply_cmd: &str) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for i in items.iter().filter(|i| i.state == State::Pending) {
        let _ = writeln!(
            s,
            "would exclude from Time Machine: {}   rule {}",
            abbreviate_home(&i.path, Some(home)),
            i.rule
        );
    }
    for i in items {
        let (tag, why) = match &i.state {
            State::Blocked(why) => ("BLOCKED", why),
            State::Error(why) => ("ERROR", why),
            _ => continue,
        };
        let _ = writeln!(s, "{tag} {}: {why}", abbreviate_home(&i.path, Some(home)));
    }
    let o = tally(items);
    let pending = items.iter().filter(|i| i.state == State::Pending).count();
    let _ = writeln!(
        s,
        "\nwould be excluded from Time Machine: {pending} directories \
         ({} already, {} blocked, {} errors)",
        o.already, o.blocked, o.errors
    );
    if pending > 0 {
        let _ = writeln!(s, "Nothing changed. Run '{apply_cmd}' to do it.");
    }
    let _ = writeln!(s, "{UNDO}");
    s
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub excluded: u64,
    pub already: u64,
    pub blocked: u64,
    pub errors: u64,
}

/// The counts of a plan as it stands (nothing excluded yet).
pub fn tally(items: &[Item]) -> Outcome {
    let mut o = Outcome::default();
    for i in items {
        match i.state {
            State::Pending => {}
            State::Already => o.already += 1,
            State::Blocked(_) => o.blocked += 1,
            State::Error(_) => o.errors += 1,
        }
    }
    o
}

/// Excludes every pending directory. Right before, the path is reopened and must still be the
/// same unflagged directory (device, inode); the xattr is then created, never replaced (a value
/// set meanwhile counts as already), and read back. One INFO line per directory excluded, a
/// WARN (also on stderr) per error.
pub fn apply(items: &[Item], euid: u32, log: &Logger) -> Outcome {
    let mut out = tally(items);
    for i in items.iter().filter(|i| i.state == State::Pending) {
        let result =
            open_checked(&i.path, i.dev, i.ino, euid).and_then(|(f, _)| match sys::fset_xattr_new(
                &f, XATTR, &VALUE,
            ) {
                Ok(false) => Ok(false),
                Ok(true) => match sys::fget_xattr(&f, XATTR, false) {
                    Ok(Some(v)) if v == VALUE => Ok(true),
                    Ok(_) => Err(State::Error("read back a different value".into())),
                    Err(e) => Err(State::Error(format!("cannot read back: {e}"))),
                },
                Err(e) => Err(State::Error(format!("cannot set: {e}"))),
            });
        match result {
            Ok(true) => {
                out.excluded += 1;
                log.info(format_args!(
                    "excluded from Time Machine {} (rule {})",
                    i.path.display(),
                    i.rule
                ));
            }
            Ok(false) => out.already += 1,
            Err(State::Blocked(why)) => {
                out.blocked += 1;
                log.info(format_args!(
                    "tm-exclude: blocked {}: {why}",
                    i.path.display()
                ));
            }
            Err(state) => {
                out.errors += 1;
                let why = match state {
                    State::Error(why) => why,
                    other => format!("{other:?}"),
                };
                log.warn(format_args!("tm-exclude: {}: {why}", i.path.display()));
                eprintln!("tm-exclude: {}: {why}", i.path.display());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk;
    use std::fs;

    struct Tmp {
        dir: PathBuf,
        log: Logger,
    }

    impl Tmp {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("acz-tmx-{tag}-{}", std::process::id()));
            let _ = std::process::Command::new("/usr/bin/chflags")
                .args(["-R", "nouchg"])
                .arg(&dir)
                .status();
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let log = Logger::open(&dir.join("log"), 1 << 20, 1, false).unwrap();
            Self { dir, log }
        }

        fn mkdir(&self, rel: &str) -> PathBuf {
            let p = self.dir.join(rel);
            fs::create_dir_all(&p).unwrap();
            p
        }

        fn items(&self, patterns: &[&str], own: &[PathBuf]) -> Vec<Item> {
            self.items_with(patterns, own, Vec::new())
        }

        /// `extra`: matches the walk would not report (it never yields a flagged directory).
        fn items_with(&self, patterns: &[&str], own: &[PathBuf], extra: Vec<Matched>) -> Vec<Item> {
            let pat: Vec<String> = patterns
                .iter()
                .map(|p| self.dir.join(p).to_string_lossy().into_owned())
                .collect();
            let m = Matcher::new(&pat, &[], None).unwrap();
            let guard = Guard::new(&self.dir, &[], &self.log).unwrap();
            let mut w = walk::walk_entries(&m, &guard, true, 2, &self.log);
            w.matched.extend(extra);
            w.matched.sort_by(|a, b| a.path.cmp(&b.path));
            let trash = self.dir.join(".Trash");
            let scope = [self.dir.clone()];
            let keep = Keep {
                scope: &scope,
                own,
                trash_dir: &trash,
            };
            plan(
                w.matched,
                &m,
                &guard,
                &keep,
                sys::effective_uid(),
                &self.log,
            )
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

    fn xattr(p: &Path) -> Option<Vec<u8>> {
        sys::fget_xattr(&open_dir(p).unwrap(), XATTR, false).unwrap()
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
    fn preview_writes_nothing_and_apply_writes_the_exact_value_once() {
        let t = Tmp::new("apply");
        let nm = t.mkdir("p/node_modules");
        let other = t.mkdir("p/src");
        fs::write(t.dir.join("p/node_modules.txt"), "x").unwrap();
        let items = t.items(&["p/node_modules*"], &[]);
        assert_eq!(names(&items, &t.dir), ["p/node_modules"]);
        assert_eq!(items[0].state, State::Pending);
        let text = render(&items, &t.dir, "acz tm-exclude --apply");
        assert!(text.contains("would exclude from Time Machine: "), "{text}");
        assert!(text.contains("tmutil removeexclusion"), "{text}");
        assert!(text.contains("acz tm-exclude --apply"), "{text}");
        assert_eq!(xattr(&nm), None);
        let out = apply(&items, sys::effective_uid(), &t.log);
        assert_eq!(
            out,
            Outcome {
                excluded: 1,
                ..Default::default()
            }
        );
        assert_eq!(xattr(&nm).as_deref(), Some(&VALUE[..]));
        assert_eq!(xattr(&other), None);
        let file = File::open(t.dir.join("p/node_modules.txt")).unwrap();
        assert_eq!(sys::fget_xattr(&file, XATTR, false).unwrap(), None);
        let items = t.items(&["p/node_modules"], &[]);
        assert_eq!(items[0].state, State::Already);
        let out = apply(&items, sys::effective_uid(), &t.log);
        assert_eq!((out.excluded, out.already), (0, 1));
    }

    #[test]
    fn a_foreign_existing_value_is_preserved() {
        let t = Tmp::new("foreign");
        let d = t.mkdir("d");
        sys::fset_xattr(&open_dir(&d).unwrap(), XATTR, b"other", 0).unwrap();
        let items = t.items(&["d"], &[]);
        assert_eq!(items[0].state, State::Already);
        assert_eq!(apply(&items, sys::effective_uid(), &t.log).excluded, 0);
        assert_eq!(xattr(&d).as_deref(), Some(&b"other"[..]));
    }

    #[test]
    fn symlinked_dirs_are_untouched() {
        let t = Tmp::new("link");
        let target = t.mkdir("real");
        t.mkdir("l");
        std::os::unix::fs::symlink(&target, t.dir.join("l/link")).unwrap();
        let items = t.items(&["l/*"], &[]);
        // The walk never reports a symlink.
        assert!(items.is_empty(), "{items:?}");
        let out = apply(&items, sys::effective_uid(), &t.log);
        assert_eq!(out, Outcome::default());
        assert_eq!(xattr(&target), None);
        let link = t.dir.join("l/link");
        assert!(
            sys::fget_xattr(
                &File::options()
                    .read(true)
                    .custom_flags(libc::O_SYMLINK)
                    .open(&link)
                    .unwrap(),
                XATTR,
                false
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn own_and_protected_and_flagged_dirs_are_blocked() {
        let t = Tmp::new("blocked");
        let conf = t.mkdir("conf");
        let lib = t.mkdir("Library");
        let locked = t.mkdir("locked");
        assert!(
            std::process::Command::new("/usr/bin/chflags")
                .arg("uchg")
                .arg(&locked)
                .status()
                .unwrap()
                .success()
        );
        let lm = fs::symlink_metadata(&locked).unwrap();
        let extra = vec![Matched {
            path: locked.clone(),
            is_dir: true,
            dev: lm.dev(),
            ino: lm.ino(),
        }];
        let items = t.items_with(&["conf", "Library"], std::slice::from_ref(&conf), extra);
        let state = |n: &str| {
            items
                .iter()
                .find(|i| i.path.ends_with(n))
                .unwrap_or_else(|| panic!("{n}: {items:?}"))
                .state
                .clone()
        };
        assert!(
            matches!(state("conf"), State::Blocked(w) if w.contains("used by this tool")),
            "{items:?}"
        );
        assert!(matches!(state("Library"), State::Blocked(_)), "{items:?}");
        assert!(
            matches!(state("locked"), State::Blocked(w) if w.contains("uchg")),
            "{items:?}"
        );
        assert_eq!(apply(&items, sys::effective_uid(), &t.log).excluded, 0);
        for d in [&conf, &lib, &locked] {
            assert_eq!(xattr(d), None, "{}", d.display());
        }
    }

    #[test]
    fn nested_matches_collapse_and_a_changed_dir_is_not_written() {
        let t = Tmp::new("nested");
        t.mkdir("a/node_modules/x/node_modules");
        let items = t.items(&["**/node_modules"], &[]);
        assert_eq!(names(&items, &t.dir), ["a/node_modules"]);
        let d = t.dir.join("a/node_modules");
        fs::rename(&d, t.dir.join("moved")).unwrap();
        fs::create_dir(&d).unwrap();
        let out = apply(&items, sys::effective_uid(), &t.log);
        assert_eq!((out.excluded, out.blocked, out.errors), (0, 1, 0));
        assert_eq!(xattr(&d), None);
        assert_eq!(xattr(&t.dir.join("moved")), None);
    }

    #[test]
    fn symlink_at_matched_path_is_not_followed() {
        let t = Tmp::new("link-at");
        let target = t.mkdir("real");
        let link = t.dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let tm = fs::metadata(&target).unwrap();
        let extra = vec![Matched {
            path: link.clone(),
            is_dir: true,
            dev: tm.dev(),
            ino: tm.ino(),
        }];
        let items = t.items_with(&[], &[], extra);
        assert_eq!(items.len(), 1, "{items:?}");
        assert!(matches!(items[0].state, State::Blocked(_)), "{items:?}");
        assert_eq!(apply(&items, sys::effective_uid(), &t.log).excluded, 0);
        assert_eq!(xattr(&target), None);
    }

    #[test]
    fn a_value_set_between_plan_and_apply_is_kept() {
        let t = Tmp::new("race");
        let d = t.mkdir("d");
        let items = t.items(&["d"], &[]);
        assert_eq!(items[0].state, State::Pending);
        sys::fset_xattr(&open_dir(&d).unwrap(), XATTR, b"other", 0).unwrap();
        let out = apply(&items, sys::effective_uid(), &t.log);
        assert_eq!((out.already, out.excluded), (1, 0));
        assert_eq!(xattr(&d).as_deref(), Some(&b"other"[..]));
    }
}
