//! `cargo-clean`: Cargo projects are found by their `Cargo.toml` below the `[cargo_clean]`
//! roots; each manifest gives its workspace and so its target dir (or `build.target-dir` /
//! `build.build-dir` of the Cargo config), kept only with Cargo's `CACHEDIR.TAG`. There the
//! build units nobody rebuilt for `max_age_secs` are deleted, the rest stays, so the next build
//! only rebuilds what was removed. A unit is a `.fingerprint/<name>-<hash>` directory of a
//! profile directory (`target/<profile>` or `target/<triple>/<profile>`) together with
//! `build/<name>-<hash>` and every `deps` entry carrying `-<hash>`. A unit is kept when it was
//! modified (files in `deps`/`build` also: read) within the window, or when a kept unit of any
//! profile of the target was built against it (its fingerprint JSON names it). Orphan `deps`
//! entries and `incremental/<name>-<hash>` directories are aged by themselves. Top-level files
//! of a profile (the final binaries) are never touched, nor anything reached via a symlink. A
//! target with a profile whose Cargo lock another process holds (a build in progress) is
//! skipped; the locks taken are held until the deletes are done, so a build started meanwhile
//! waits.

use crate::walk::{Matched, Matcher};
use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The signature line of the Cache Directory Tagging Specification.
const CACHEDIR_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";

/// Whether `dir` holds the `CACHEDIR.TAG` Cargo writes into a target directory.
pub(crate) fn is_cargo_target(dir: &Path) -> bool {
    let Ok(f) = File::open(dir.join("CACHEDIR.TAG")) else {
        return false;
    };
    let mut text = String::new();
    // The tag is a few lines; more is not Cargo's.
    if f.take(4096).read_to_string(&mut text).is_err() {
        return false;
    }
    text.starts_with(CACHEDIR_SIGNATURE) && text.contains("cargo")
}

fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// Child directories (never through a symlink), sorted.
fn child_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// Profile directories of a target: those holding `.fingerprint`, one or two levels down.
pub(crate) fn profile_dirs(target: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in child_dirs(target) {
        if is_real_dir(&d.join(".fingerprint")) {
            out.push(d);
        } else {
            out.extend(
                child_dirs(&d)
                    .into_iter()
                    .filter(|p| is_real_dir(&p.join(".fingerprint"))),
            );
        }
    }
    out
}

/// The lock files Cargo holds in a profile directory during a build.
const LOCK_NAMES: [&str; 3] = [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];

/// Takes every Cargo lock file present in a profile without waiting. `Err`: another process
/// holds one, one cannot be opened, or there is none (not a profile Cargo built).
pub(crate) fn lock_profile(profile: &Path) -> Result<Vec<File>, String> {
    let mut held = Vec::new();
    for name in LOCK_NAMES {
        let path = profile.join(name);
        let f = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("cannot open {}: {e}", path.display())),
        };
        match f.try_lock() {
            Ok(()) => held.push(f),
            Err(fs::TryLockError::WouldBlock) => return Err(format!("a build holds {name}")),
            Err(fs::TryLockError::Error(e)) => {
                return Err(format!("cannot lock {}: {e}", path.display()));
            }
        }
    }
    if held.is_empty() {
        return Err("no Cargo lock file".into());
    }
    Ok(held)
}

/// Modification time, and with `atime` for a file also its access time; never a directory's
/// access time (listing it, as this tool, Spotlight or a backup do, moves it).
fn used(m: &Metadata, atime: bool) -> SystemTime {
    let t = |s: i64, ns: i64| {
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(s.max(0).unsigned_abs())
            + Duration::from_nanos(ns.max(0).unsigned_abs())
    };
    let modified = t(m.mtime(), m.mtime_nsec());
    if m.is_dir() || !atime {
        modified
    } else {
        modified.max(t(m.atime(), m.atime_nsec()))
    }
}

/// Newest use of `p` and, for a directory, of its direct children. Unreadable counts as now:
/// what cannot be aged is kept.
fn newest_use(p: &Path, atime: bool) -> SystemTime {
    let Ok(m) = fs::symlink_metadata(p) else {
        return SystemTime::now();
    };
    let mut newest = used(&m, atime);
    if m.is_dir() {
        let Ok(rd) = fs::read_dir(p) else {
            return SystemTime::now();
        };
        for e in rd {
            match e.and_then(|e| e.metadata()) {
                Ok(cm) => newest = newest.max(used(&cm, atime)),
                Err(_) => return SystemTime::now(),
            }
        }
    }
    newest
}

/// The `<hash>` of `<name>-<hash>`: 16 lowercase hex digits after the last `-`.
fn unit_hash(name: &str) -> Option<&str> {
    let (_, h) = name.rsplit_once('-')?;
    (h.len() == 16 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))).then_some(h)
}

/// The unit hash in a `deps` entry name: `-<16 hex>` followed by `.` or the end.
fn deps_hash(name: &str) -> Option<&str> {
    let mut rest = name;
    while let Some(i) = rest.find('-') {
        let after = &rest[i + 1..];
        if let Some(h) = after.get(..16)
            && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            && matches!(after.as_bytes().get(16), None | Some(b'.'))
        {
            return Some(h);
        }
        rest = after;
    }
    None
}

/// Entries of `dir` with UTF-8 names, sorted; nothing when `dir` is not a real directory (a
/// symlinked `deps`, `build` or `incremental` is never entered).
fn names(dir: &Path) -> Vec<(String, PathBuf)> {
    if !is_real_dir(dir) {
        return Vec::new();
    }
    let mut out: Vec<(String, PathBuf)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
        .collect();
    out.sort();
    out
}

/// The fingerprint hashes a unit's fingerprint dir stores: files holding 16 hex digits, the
/// little-endian bytes of the u64 its dependents' JSON names.
fn own_fingerprints(fp: &Path) -> Vec<u64> {
    names(fp)
        .into_iter()
        .filter(|(n, _)| !n.contains('.') && !n.starts_with("dep-") && !n.starts_with("output-"))
        .filter_map(|(_, p)| {
            let m = fs::symlink_metadata(&p).ok()?;
            if !m.is_file() || m.len() > 64 {
                return None;
            }
            let text = fs::read_to_string(&p).ok()?;
            let text = text.trim();
            if text.len() != 16 {
                return None;
            }
            let mut bytes = [0u8; 8];
            for (k, b) in bytes.iter_mut().enumerate() {
                *b = u8::from_str_radix(text.get(2 * k..2 * k + 2)?, 16).ok()?;
            }
            Some(u64::from_le_bytes(bytes))
        })
        .collect()
}

/// The fingerprint hashes of the dependencies a unit's fingerprint JSON lists: the last number
/// of every `[pkg, "name", public, fingerprint]` in its `"deps"` array. `None`: not parseable.
fn json_deps(text: &str) -> Option<Vec<u64>> {
    let start = text.find("\"deps\":[")? + "\"deps\":[".len();
    let mut out = Vec::new();
    let (mut depth, mut in_str, mut esc) = (1u32, false, false);
    let mut token = String::new();
    for c in text[start..].chars() {
        if in_str {
            match (esc, c) {
                (true, _) => esc = false,
                (false, '\\') => esc = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '[' => {
                depth += 1;
                token.clear();
            }
            ',' => token.clear(),
            ']' => {
                if depth == 2 {
                    out.push(token.trim().parse().ok()?);
                }
                depth -= 1;
                if depth == 0 {
                    return Some(out);
                }
                token.clear();
            }
            _ if depth == 2 => token.push(c),
            _ => {}
        }
    }
    None
}

struct Unit {
    fp: PathBuf,
    build: Option<PathBuf>,
    deps: Vec<PathBuf>,
    fresh: bool,
    fingerprints: Vec<u64>,
    /// Fingerprints of the units it was built against; `None`: a JSON was not parseable.
    uses: Option<Vec<u64>>,
}

/// Adds the units of one profile to `units`; returns its `deps` entries whose hash names no
/// unit there.
#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "cargo names its fingerprint json files with a lowercase `.json`"
)]
fn profile_units(
    profile: &Path,
    keep_all: bool,
    cutoff: SystemTime,
    units: &mut Vec<Unit>,
) -> HashMap<String, Vec<PathBuf>> {
    let mut deps: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for (name, p) in names(&profile.join("deps")) {
        if let Some(h) = deps_hash(&name) {
            deps.entry(h.to_owned()).or_default().push(p);
        }
    }
    for (name, fp) in names(&profile.join(".fingerprint")) {
        let Some(hash) = unit_hash(&name) else {
            continue;
        };
        if !is_real_dir(&fp) {
            continue;
        }
        let build = Some(profile.join("build").join(&name))
            .filter(|b| is_real_dir(&profile.join("build")) && fs::symlink_metadata(b).is_ok());
        let unit_deps = deps.remove(hash).unwrap_or_default();
        // This tool reads the fingerprint files (moving their access time): only their
        // modification time counts. `deps` and `build` are never read here.
        let newest = std::iter::once(newest_use(&fp, false))
            .chain(unit_deps.iter().chain(&build).map(|p| newest_use(p, true)))
            .max()
            .unwrap_or_else(SystemTime::now);
        let uses = names(&fp)
            .into_iter()
            .filter(|(n, _)| n.ends_with(".json"))
            .try_fold(Vec::new(), |mut acc, (_, p)| {
                acc.extend(json_deps(&fs::read_to_string(p).ok()?)?);
                Some(acc)
            });
        units.push(Unit {
            fingerprints: own_fingerprints(&fp),
            fp,
            build,
            deps: unit_deps,
            fresh: keep_all || newest >= cutoff,
            uses,
        });
    }
    deps
}

/// Which final artifacts `cargo-clean` keeps whatever their age: those of the `debug` (dev)
/// and the `release` profile, and those of every other profile (kept outside a git worktree).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeepFinal {
    pub debug: bool,
    pub release: bool,
    /// Every other profile (`false` only inside a git worktree).
    pub others: bool,
}

impl KeepFinal {
    /// Whether the final artifacts of `profile` stay.
    fn keeps(self, profile: &Path) -> bool {
        match profile.file_name().and_then(|n| n.to_str()) {
            Some("debug") => self.debug,
            Some("release") => self.release,
            _ => self.others,
        }
    }
}

/// The regular files at the top of `profile` and of its `examples` (not Cargo's dotfiles,
/// such as its locks) not used since `cutoff`.
fn stale_finals(profile: &Path, cutoff: SystemTime) -> Vec<PathBuf> {
    let examples = profile.join("examples");
    let mut dirs = vec![profile.to_path_buf()];
    if is_real_dir(&examples) {
        dirs.push(examples);
    }
    dirs.iter()
        .flat_map(|d| names(d))
        .filter(|(n, p)| {
            !n.starts_with('.')
                && fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file())
                && newest_use(p, true) < cutoff
        })
        .map(|(_, p)| p)
        .collect()
}

/// The stale entries of the profiles of one target, sorted: the dependency graph spans them
/// all (a cross build uses the build scripts and proc-macros of the host profile). A unit is
/// kept when one of its entries was used since `cutoff`, it is in a profile marked `true`
/// (excluded: everything there stays), or a kept unit was built against it (its fingerprint
/// JSON names it), so a dependency of a crate rebuilt recently stays however old its files are.
/// The final artifacts of a profile (its top-level files and those of `examples`, hard links
/// of `deps` entries) stay unless `keep` allows them to go; those not used since `cutoff` are
/// then listed too.
/// `Err` when a kept unit's dependencies cannot be read: then nothing is listed.
pub(crate) fn stale_entries(
    profiles: &[(PathBuf, bool)],
    cutoff: SystemTime,
    keep: KeepFinal,
) -> Result<Vec<PathBuf>, String> {
    let mut units = Vec::new();
    let mut out = Vec::new();
    for (profile, keep_all) in profiles {
        let mut deps = profile_units(profile, *keep_all, cutoff, &mut units);
        // `deps` entries whose unit is gone (an interrupted build or delete): by their own age.
        if !keep_all {
            out.extend(
                deps.drain()
                    .flat_map(|(_, v)| v)
                    .filter(|p| newest_use(p, true) < cutoff),
            );
            for (name, p) in names(&profile.join("incremental")) {
                if name.contains('-') && is_real_dir(&p) && newest_use(&p, true) < cutoff {
                    out.push(p);
                }
            }
            if !keep.keeps(profile) {
                out.extend(stale_finals(profile, cutoff));
            }
        }
    }
    let by_fingerprint: HashMap<u64, usize> = units
        .iter()
        .enumerate()
        .flat_map(|(k, u)| u.fingerprints.iter().map(move |f| (*f, k)))
        .collect();
    let mut stack: Vec<usize> = (0..units.len()).filter(|&k| units[k].fresh).collect();
    while let Some(k) = stack.pop() {
        let Some(uses) = units[k].uses.clone() else {
            return Err(format!(
                "cannot read the dependencies of {}",
                units[k].fp.display()
            ));
        };
        for f in uses {
            if let Some(&d) = by_fingerprint.get(&f)
                && !units[d].fresh
            {
                units[d].fresh = true;
                stack.push(d);
            }
        }
    }
    for u in units.into_iter().filter(|u| !u.fresh) {
        out.push(u.fp);
        out.extend(u.build);
        out.extend(u.deps);
    }
    out.sort();
    Ok(out)
}

/// A walk entry for `p` (lstat), for the checks of `remove::plan`.
pub(crate) fn matched(p: PathBuf) -> Option<Matched> {
    let m = fs::symlink_metadata(&p).ok()?;
    if m.file_type().is_symlink() {
        return None;
    }
    Some(Matched {
        is_dir: m.is_dir(),
        dev: m.dev(),
        ino: m.ino(),
        path: p,
    })
}

/// One matched target directory: its stale entries, or why it is skipped.
pub(crate) struct Target {
    pub root: PathBuf,
    pub rule: String,
    pub entries: Vec<PathBuf>,
    /// Profiles skipped (a build in progress, no or an unopenable lock, an unreadable
    /// dependency graph), with the reason.
    pub skipped: Vec<(PathBuf, String)>,
    /// Locks held until the deletes are done.
    pub locks: Vec<File>,
}

/// The table at `path` (TOML), or `None` when it is missing or not TOML.
fn read_toml(path: &Path) -> Option<toml::Table> {
    let text = fs::read_to_string(path).ok()?;
    text.parse::<toml::Table>().ok()
}

/// `build.<key>` of the nearest Cargo config file at or above `dir`, else of
/// `<home>/.cargo/config.toml`, resolved against the directory holding that `.cargo`. A value
/// with a `{...}` template is not resolved (`None`).
fn config_dir(dir: &Path, home: &Path, key: &str) -> Option<PathBuf> {
    let found = dir.ancestors().chain(std::iter::once(home)).find_map(|d| {
        ["config.toml", "config"].iter().find_map(|n| {
            let v = read_toml(&d.join(".cargo").join(n))?
                .get("build")?
                .get(key)?
                .as_str()?
                .to_owned();
            Some((d.to_path_buf(), v))
        })
    });
    let (base, v) = found?;
    if v.contains('{') {
        return None;
    }
    Some(base.join(crate::config::expand_tilde(&v, home)))
}

/// The workspace root of the package whose manifest is `manifest`: the nearest directory at or
/// above it whose `Cargo.toml` has a `[workspace]` table, else the manifest's own directory.
/// `None` when the manifest is not a Cargo manifest (no `[package]` nor `[workspace]`).
fn workspace_root(manifest: &Path) -> Option<PathBuf> {
    let own = read_toml(manifest)?;
    let dir = manifest.parent()?;
    if own.contains_key("workspace") {
        return Some(dir.to_path_buf());
    }
    if !own.contains_key("package") {
        return None;
    }
    let ws = dir
        .ancestors()
        .skip(1)
        .find(|d| read_toml(&d.join("Cargo.toml")).is_some_and(|t| t.contains_key("workspace")));
    Some(ws.unwrap_or(dir).to_path_buf())
}

/// The target and build directories of the projects whose `Cargo.toml` files were found, each
/// once, with the manifest that named it: `build.target-dir` (default `<workspace>/target`) and
/// `build.build-dir` of the Cargo config. Only directories carrying Cargo's `CACHEDIR.TAG` are
/// kept.
/// The second value lists the existing directories a manifest names that lack the tag (never
/// cleaned, reported so the skip is visible).
pub(crate) fn target_dirs(
    manifests: &[PathBuf],
    home: &Path,
) -> (Vec<(PathBuf, String)>, Vec<PathBuf>) {
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    let mut untagged: Vec<PathBuf> = Vec::new();
    for m in manifests {
        let Some(ws) = workspace_root(m) else {
            continue;
        };
        let target = config_dir(&ws, home, "target-dir").unwrap_or_else(|| ws.join("target"));
        let build = config_dir(&ws, home, "build-dir");
        for d in std::iter::once(target).chain(build) {
            let Ok(real) = fs::canonicalize(&d) else {
                continue;
            };
            if !is_cargo_target(&real) {
                if is_real_dir(&real) && !untagged.contains(&real) {
                    untagged.push(real);
                }
            } else if !out.iter().any(|(p, _)| *p == real) {
                out.push((real, ws.join("Cargo.toml").display().to_string()));
            }
        }
    }
    out.sort();
    untagged.sort();
    (out, untagged)
}

/// Adds the Cargo target directories no manifest names (a `CARGO_TARGET_DIR`): the parents of
/// the `CACHEDIR.TAG` files `tags` the walk found, then tagged directories one or two levels
/// below any found target (`target/dev-stand/<name>`). Each is named by its own tag file. Only
/// directories with Cargo's tag are added, and never one that holds or lies in a profile of an
/// already found target (Cargo also tags `target/<triple>`, a part of its target).
pub(crate) fn add_tagged_targets(found: &mut Vec<(PathBuf, String)>, tags: &[PathBuf]) {
    let mut profiles: Vec<PathBuf> = found.iter().flat_map(|(r, _)| profile_dirs(r)).collect();
    let add = |found: &mut Vec<(PathBuf, String)>, profiles: &mut Vec<PathBuf>, d: &Path| {
        let Ok(real) = fs::canonicalize(d) else {
            return;
        };
        if !is_cargo_target(&real)
            || found.iter().any(|(p, _)| *p == real)
            || profiles
                .iter()
                .any(|p| p.starts_with(&real) || real.starts_with(p))
        {
            return;
        }
        profiles.extend(profile_dirs(&real));
        let rule = real.join("CACHEDIR.TAG").display().to_string();
        found.push((real, rule));
    };
    // Ancestors first: an outer tagged dir is never dropped for holding a nested one's profile.
    let mut dirs: Vec<&Path> = tags.iter().filter_map(|t| t.parent()).collect();
    dirs.sort_by_key(|d| d.components().count());
    for d in dirs {
        add(found, &mut profiles, d);
    }
    let mut i = 0;
    while i < found.len() {
        let root = found[i].0.clone();
        for d in child_dirs(&root) {
            add(found, &mut profiles, &d);
            for c in child_dirs(&d) {
                add(found, &mut profiles, &c);
            }
        }
        i += 1;
    }
    found.sort();
}

/// Whether `dir` lies in a linked git worktree: the nearest `.git` at or above it is a file
/// whose `gitdir:` names a directory inside a `worktrees` directory that holds `commondir`.
pub(crate) fn in_worktree(dir: &Path) -> bool {
    let Some(git) = dir
        .ancestors()
        .map(|a| a.join(".git"))
        .find(|g| fs::symlink_metadata(g).is_ok())
    else {
        return false;
    };
    if !fs::symlink_metadata(&git).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let mut text = String::new();
    if File::open(&git)
        .and_then(|f| f.take(4096).read_to_string(&mut text))
        .is_err()
    {
        return false;
    }
    text.lines()
        .next()
        .and_then(|l| l.strip_prefix("gitdir:"))
        .is_some_and(|p| {
            // Only a linked worktree's gitdir holds `commondir` (a submodule's does not).
            let g = git.parent().unwrap_or(dir).join(p.trim());
            g.join("commondir").is_file()
                && g.parent()
                    .is_some_and(|d| d.file_name().is_some_and(|n| n == "worktrees"))
        })
}

/// Whether an exclude matches `p` or a directory between `root` (not included) and `p`.
fn excluded_below(p: &Path, root: &Path, matcher: &Matcher) -> bool {
    let mut cur = Some(p);
    while let Some(c) = cur {
        if c == root || !c.starts_with(root) {
            return false;
        }
        if matcher.excludes(c, c != p || is_real_dir(c)) {
            return true;
        }
        cur = c.parent();
    }
    false
}

/// Whether an exclude keeps the whole target at `root` (as `collect` checks it): the root, its
/// realpath or one of their ancestors.
pub(crate) fn root_excluded(root: &Path, matcher: &Matcher) -> bool {
    excluded_at_or_above(root, matcher)
        || fs::canonicalize(root).is_ok_and(|r| excluded_at_or_above(&r, matcher))
}

/// Whether an exclude matches `p` or one of its ancestors.
fn excluded_at_or_above(p: &Path, matcher: &Matcher) -> bool {
    p.ancestors()
        .take_while(|a| a.parent().is_some())
        .any(|a| matcher.excludes(a, true))
}

/// Collects the stale entries (not used for `max_age_secs`) of every target directory; entries
/// an exclude matches are left out.
pub(crate) fn collect(
    targets: Vec<(PathBuf, String)>,
    matcher: &Matcher,
    max_age_secs: u64,
    keep: KeepFinal,
) -> Vec<Target> {
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(max_age_secs))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut out = Vec::new();
    for (root, rule) in targets {
        let mut t = Target {
            root,
            rule,
            entries: Vec::new(),
            skipped: Vec::new(),
            locks: Vec::new(),
        };
        let Ok(real_root) = fs::canonicalize(&t.root) else {
            continue;
        };
        // Every profile is locked first: the graph spans them, so one busy profile skips the
        // whole target.
        if excluded_at_or_above(&t.root, matcher) || excluded_at_or_above(&real_root, matcher) {
            continue;
        }
        let profiles: Vec<(PathBuf, bool)> = profile_dirs(&t.root)
            .into_iter()
            .map(|p| {
                let ex = excluded_below(&p, &t.root, matcher);
                (p, ex)
            })
            .collect();
        for (profile, _) in &profiles {
            match lock_profile(profile) {
                Ok(lock) => t.locks.extend(lock),
                Err(why) => t.skipped.push((profile.clone(), why)),
            }
        }
        if !t.skipped.is_empty() {
            t.locks.clear();
            out.push(t);
            continue;
        }
        // A worktree's builds are throwaway: every profile's stale final artifacts go too.
        let keep = if in_worktree(&t.root) {
            KeepFinal {
                debug: false,
                release: false,
                others: false,
            }
        } else {
            keep
        };
        match stale_entries(&profiles, cutoff, keep) {
            Err(why) => t.skipped.push((t.root.clone(), why)),
            Ok(entries) => {
                // realpath per parent directory: thousands of entries share one.
                let mut inside: HashMap<PathBuf, bool> = HashMap::new();
                t.entries.extend(entries.into_iter().filter(|p| {
                    // An excluded entry or ancestor stays, and nothing outside the target goes
                    // (backstop: symlinked directories are never entered).
                    !excluded_below(p, &t.root, matcher)
                        && p.parent().is_some_and(|d| {
                            *inside.entry(d.to_path_buf()).or_insert_with(|| {
                                fs::canonicalize(d).is_ok_and(|d| d.starts_with(&real_root))
                            })
                        })
                }));
            }
        }
        out.push(t);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;

    const KEEP: KeepFinal = KeepFinal {
        debug: true,
        release: true,
        others: true,
    };

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!(
            "acz-cargo-clean-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::remove_dir_all(&d).ok();
        fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    fn old(p: &Path) {
        let t = SystemTime::now() - Duration::from_secs(100 * 86_400);
        let times = FileTimes::new().set_accessed(t).set_modified(t);
        // Directories are opened read-only; set_times works on them too.
        File::open(p).unwrap().set_times(times).unwrap();
    }

    fn file(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"x").unwrap();
    }

    /// A target with one old unit `a` and one fresh unit `b` in `debug`.
    fn target(root: &Path) -> PathBuf {
        let t = root.join("target");
        fs::create_dir_all(&t).unwrap();
        fs::write(
            t.join("CACHEDIR.TAG"),
            format!(
                "{CACHEDIR_SIGNATURE}\n# This file is a cache directory tag created by cargo.\n"
            ),
        )
        .unwrap();
        let p = t.join("debug");
        let ha = "0123456789abcdef";
        let hb = "fedcba9876543210";
        for f in [
            format!(".fingerprint/a-{ha}/lib-a"),
            format!("deps/liba-{ha}.rlib"),
            format!("deps/a-{ha}.d"),
            format!("build/a-{ha}/out"),
            "incremental/a-0abc/s-1/x".into(),
            format!(".fingerprint/b-{hb}/lib-b"),
            format!("deps/libb-{hb}.rlib"),
            "incremental/b-0def/s-1/x".into(),
            "app".into(),
            ".cargo-lock".into(),
        ] {
            file(&p.join(f));
        }
        for f in [
            format!(".fingerprint/a-{ha}/lib-a"),
            format!(".fingerprint/a-{ha}"),
            format!("deps/liba-{ha}.rlib"),
            format!("deps/a-{ha}.d"),
            format!("build/a-{ha}/out"),
            format!("build/a-{ha}"),
            "incremental/a-0abc/s-1".into(),
            "incremental/a-0abc".into(),
            "app".into(),
        ] {
            old(&p.join(f));
        }
        t
    }

    #[test]
    fn final_artifacts_go_only_when_allowed() {
        let d = tmp("finals");
        let t = target(&d.0);
        let p = t.join("debug");
        for n in ["app", "app.d", "examples/demo", "new", "examples/.x"] {
            file(&p.join(n));
        }
        for n in ["app", "app.d", "examples/demo", "examples/.x"] {
            old(&p.join(n));
        }
        old(&p.join(".cargo-lock"));
        let finals = |keep| {
            stale_entries(&[(p.clone(), false)], cutoff(), keep)
                .unwrap()
                .into_iter()
                .filter_map(|e| e.strip_prefix(&p).ok().map(|r| r.display().to_string()))
                .filter(|r| {
                    !r.starts_with("deps")
                        && !r.starts_with(".fingerprint")
                        && !r.starts_with("build")
                        && !r.starts_with("incremental")
                })
                .collect::<Vec<_>>()
        };
        assert!(finals(KEEP).is_empty());
        let rel = KeepFinal {
            debug: true,
            release: false,
            others: true,
        };
        assert!(finals(rel).is_empty());
        let dbg = KeepFinal {
            debug: false,
            release: true,
            others: true,
        };
        assert_eq!(finals(dbg), ["app", "app.d", "examples/demo"]);
    }

    #[test]
    fn hashes() {
        assert_eq!(
            unit_hash("blake3-4705c8f789ee46af"),
            Some("4705c8f789ee46af")
        );
        assert_eq!(unit_hash("foo-bar"), None);
        assert_eq!(
            deps_hash("blake3-4705c8f789ee46af.blake3.81d4-cgu.0.rcgu.o"),
            Some("4705c8f789ee46af")
        );
        assert_eq!(deps_hash("app-4705c8f789ee46af"), Some("4705c8f789ee46af"));
        assert_eq!(deps_hash("app-4705c8f789ee46afx"), None);
        assert_eq!(deps_hash("app.d"), None);
    }

    #[test]
    fn only_old_units_are_stale() {
        let d = tmp("stale");
        let t = target(&d.0);
        assert!(is_cargo_target(&t));
        assert!(!is_cargo_target(&d.0));
        let p = t.join("debug");
        assert_eq!(profile_dirs(&t), vec![p.clone()]);
        let cutoff = SystemTime::now() - Duration::from_secs(30 * 86_400);
        let got: Vec<String> = stale_entries(&[(p.clone(), false)], cutoff, KEEP)
            .unwrap()
            .iter()
            .map(|x| x.strip_prefix(&p).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            got,
            vec![
                ".fingerprint/a-0123456789abcdef",
                "build/a-0123456789abcdef",
                "deps/a-0123456789abcdef.d",
                "deps/liba-0123456789abcdef.rlib",
                "incremental/a-0abc",
            ]
        );
    }

    #[test]
    fn a_fresh_deps_file_keeps_its_unit() {
        let d = tmp("fresh");
        let t = target(&d.0);
        let p = t.join("debug");
        // Used now: the whole unit `a` stays.
        file(&p.join("deps/liba-0123456789abcdef.rmeta"));
        let cutoff = SystemTime::now() - Duration::from_secs(30 * 86_400);
        let got = stale_entries(&[(p, false)], cutoff, KEEP).unwrap();
        assert!(
            !got.iter()
                .any(|x| x.to_string_lossy().contains("0123456789abcdef"))
        );
        assert!(got.iter().any(|x| x.ends_with("incremental/a-0abc")));
    }

    #[test]
    fn a_held_lock_skips_the_profile() {
        let d = tmp("lock");
        let t = target(&d.0);
        let held = File::open(t.join("debug/.cargo-lock")).unwrap();
        held.lock().unwrap();
        let m = Matcher::new(&[format!("{}/target", d.0.display())], &[], None).unwrap();
        let got = collect(vec![(t.clone(), "r".into())], &m, 30 * 86_400, KEEP);
        assert_eq!(got.len(), 1);
        assert!(got[0].entries.is_empty());
        assert_eq!(got[0].skipped.len(), 1);
        drop(held);
        // A child another test forks (ditto) shares the descriptor until its exec: retry.
        let mut got = collect(vec![(t.clone(), "r".into())], &m, 30 * 86_400, KEEP);
        for _ in 0..100 {
            if got[0].skipped.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            got = collect(vec![(t.clone(), "r".into())], &m, 30 * 86_400, KEEP);
        }
        assert_eq!(got[0].entries.len(), 5, "{:?}", got[0].skipped);
        assert_eq!(got[0].locks.len(), 1);
    }

    #[test]
    fn excludes() {
        let d = tmp("exc");
        let t = target(&d.0);
        let m = Matcher::new(&[], &["a-0abc".to_owned()], None).unwrap();
        let got = collect(vec![(t, "r".into())], &m, 30 * 86_400, KEEP);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].entries.len(), 4);
    }

    #[test]
    fn target_dirs_follow_workspace_and_cargo_config() {
        let d = tmp("ws");
        let home = d.0.join("home");
        fs::create_dir_all(&home).unwrap();
        // Workspace `ws` with a member: one target, named by the workspace manifest.
        let ws = d.0.join("ws");
        file(&ws.join("crates/m/src/lib.rs"));
        fs::write(
            ws.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/m\"]\n",
        )
        .unwrap();
        fs::write(ws.join("crates/m/Cargo.toml"), "[package]\nname = \"m\"\n").unwrap();
        target(&ws);
        // Package `p` whose .cargo/config.toml moves the target dir to `out`.
        let p = d.0.join("p");
        file(&p.join(".cargo/config.toml"));
        fs::write(
            p.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"out\"\n",
        )
        .unwrap();
        fs::write(p.join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
        let out = target(&p);
        fs::rename(&out, p.join("out")).unwrap();
        // A `target` without the tag and a file that is not a manifest are ignored.
        let q = d.0.join("q");
        fs::create_dir_all(q.join("target")).unwrap();
        fs::write(q.join("Cargo.toml"), "[package]\nname = \"q\"\n").unwrap();
        file(&d.0.join("z/Cargo.toml"));
        let (got, untagged) = target_dirs(
            &[
                ws.join("crates/m/Cargo.toml"),
                ws.join("Cargo.toml"),
                p.join("Cargo.toml"),
                q.join("Cargo.toml"),
                d.0.join("z/Cargo.toml"),
            ],
            &home,
        );
        let real = |x: PathBuf| fs::canonicalize(x).unwrap();
        assert_eq!(
            got,
            vec![
                (
                    real(p.join("out")),
                    p.join("Cargo.toml").display().to_string()
                ),
                (
                    real(ws.join("target")),
                    ws.join("Cargo.toml").display().to_string()
                ),
            ]
        );
        assert_eq!(untagged, vec![real(q.join("target"))]);
    }

    const CUTOFF: u64 = 30 * 86_400;

    fn cutoff() -> SystemTime {
        SystemTime::now() - Duration::from_secs(CUTOFF)
    }

    #[test]
    fn json_deps_parse() {
        let j = r#"{"rustc":1,"features":"[\"a]\"]","deps":[[30,"globset",false,1469],[31,"x,y",true,7]],"local":[]}"#;
        assert_eq!(json_deps(j), Some(vec![1469, 7]));
        assert_eq!(json_deps(r#"{"deps":[]}"#), Some(vec![]));
        assert_eq!(json_deps(r#"{"deps":[[1,"a",false,"x"]]}"#), None);
        assert_eq!(json_deps(r#"{"x":1}"#), None);
    }

    #[test]
    #[expect(clippy::format_collect, reason = "test fixture: a hex fingerprint")]
    fn a_dependency_of_a_fresh_unit_stays() {
        let d = tmp("graph");
        let t = target(&d.0);
        let p = t.join("debug");
        let n: u64 = 0x1122_3344_5566_7788;
        let le: String = n.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let fa = p.join(".fingerprint/a-0123456789abcdef");
        fs::write(fa.join("lib-a"), le).unwrap();
        old(&fa.join("lib-a"));
        old(&fa);
        fs::write(
            p.join(".fingerprint/b-fedcba9876543210/lib-b.json"),
            format!(r#"{{"deps":[[1,"a",false,{n}]]}}"#),
        )
        .unwrap();
        let got = stale_entries(&[(p.clone(), false)], cutoff(), KEEP).unwrap();
        assert!(
            !got.iter()
                .any(|x| x.to_string_lossy().contains("0123456789abcdef")),
            "{got:?}"
        );
        // An unparseable JSON of a fresh unit: nothing of the profile is listed.
        fs::write(p.join(".fingerprint/b-fedcba9876543210/lib-b.json"), "{").unwrap();
        assert!(stale_entries(&[(p, false)], cutoff(), KEEP).is_err());
    }

    #[test]
    fn symlinked_subdirs_are_not_entered() {
        let d = tmp("link");
        let t = target(&d.0);
        let p = t.join("debug");
        let outside = d.0.join("outside");
        file(&outside.join("x-1/f"));
        file(&outside.join("liba-0123456789abcdef.rlib"));
        for f in ["x-1/f", "x-1", "liba-0123456789abcdef.rlib"] {
            old(&outside.join(f));
        }
        fs::remove_dir_all(p.join("incremental")).unwrap();
        std::os::unix::fs::symlink(&outside, p.join("incremental")).unwrap();
        fs::remove_dir_all(p.join("deps")).unwrap();
        std::os::unix::fs::symlink(&outside, p.join("deps")).unwrap();
        let got = stale_entries(&[(p.clone(), false)], cutoff(), KEEP).unwrap();
        assert!(
            got.iter()
                .all(|x| !x.starts_with(p.join("incremental")) && !x.starts_with(p.join("deps"))),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|x| x.ends_with(".fingerprint/a-0123456789abcdef"))
        );
    }

    #[test]
    fn an_excluded_profile_stays() {
        let d = tmp("exprof");
        let t = target(&d.0);
        let m = Matcher::new(&[], &["debug".to_owned()], None).unwrap();
        let got = collect(vec![(t, "r".into())], &m, CUTOFF, KEEP);
        assert!(got[0].entries.is_empty() && got[0].skipped.is_empty());
    }

    #[test]
    fn every_lock_file_counts() {
        let d = tmp("locks");
        let t = target(&d.0);
        let p = t.join("debug");
        fs::write(p.join(".cargo-build-lock"), "").unwrap();
        let held = File::open(p.join(".cargo-build-lock")).unwrap();
        held.lock().unwrap();
        assert!(lock_profile(&p).unwrap_err().contains(".cargo-build-lock"));
        drop(held);
        // Probably a child forked by a concurrent test: it shares the open file (and so the flock) until
        // its exec closes it, so the release may be seen a moment late.
        let mut got = lock_profile(&p);
        for _ in 0..100 {
            if got.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
            got = lock_profile(&p);
        }
        assert_eq!(got.unwrap().len(), 2);
        fs::remove_file(p.join(".cargo-lock")).unwrap();
        fs::remove_file(p.join(".cargo-build-lock")).unwrap();
        assert_eq!(lock_profile(&p).unwrap_err(), "no Cargo lock file");
    }

    #[test]
    #[expect(
        clippy::format_collect,
        clippy::many_single_char_names,
        reason = "test fixture: a hex fingerprint and short path names"
    )]
    fn the_graph_spans_the_profiles_of_a_target() {
        let d = tmp("cross");
        let t = target(&d.0);
        let n: u64 = 42;
        let le: String = n.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let fa = t.join("debug/.fingerprint/a-0123456789abcdef");
        fs::write(fa.join("lib-a"), le).unwrap();
        old(&fa.join("lib-a"));
        old(&fa);
        // A fresh unit of a cross profile built against the host unit `a`.
        let x = t.join("aarch64-apple-darwin/debug");
        let fx = x.join(".fingerprint/app-1111111111111111");
        file(&fx.join("bin-app.json"));
        fs::write(
            fx.join("bin-app.json"),
            format!(r#"{{"deps":[[1,"a",false,{n}]]}}"#),
        )
        .unwrap();
        file(&x.join(".cargo-lock"));
        let m = Matcher::new(&[], &[], None).unwrap();
        let got = collect(vec![(t.clone(), "r".into())], &m, CUTOFF, KEEP);
        assert!(got[0].skipped.is_empty(), "{:?}", got[0].skipped);
        assert!(
            !got[0]
                .entries
                .iter()
                .any(|p| p.to_string_lossy().contains("0123456789abcdef"))
        );
        assert!(
            got[0]
                .entries
                .iter()
                .any(|p| p.ends_with("incremental/a-0abc"))
        );
        // One busy profile skips the whole target.
        drop(got);
        let held = File::open(x.join(".cargo-lock")).unwrap();
        held.lock().unwrap();
        let got = collect(vec![(t, "r".into())], &m, CUTOFF, KEEP);
        assert!(got[0].entries.is_empty() && got[0].locks.is_empty());
    }

    #[test]
    fn untagged_targets_are_reported() {
        let d = tmp("untagged");
        let q = d.0.join("q");
        fs::create_dir_all(q.join("target")).unwrap();
        fs::write(q.join("Cargo.toml"), "[package]\nname = \"q\"\n").unwrap();
        let (got, untagged) = target_dirs(&[q.join("Cargo.toml")], &d.0);
        assert!(got.is_empty());
        assert_eq!(untagged, vec![fs::canonicalize(q.join("target")).unwrap()]);
    }

    #[test]
    fn tagged_dirs_below_a_target_and_from_the_walk_are_added() {
        let d = tmp("tagged");
        let outer = target(&d.0);
        // `target/dev-stand/cargo` (two levels): a tagged target; `target/n` holds a profile, so
        // it is part of the outer target (like a `<triple>` dir).
        let nested = target(&outer.join("dev-stand"));
        fs::rename(&nested, outer.join("dev-stand/cargo")).unwrap();
        let one = target(&outer.join("tmp"));
        fs::rename(&one, outer.join("n")).unwrap();
        fs::remove_dir_all(outer.join("tmp")).unwrap();
        // A dir with a foreign tag is not Cargo's.
        file(&outer.join("other/CACHEDIR.TAG"));
        // A tag the walk found elsewhere (`.var/x`).
        let var = target(&d.0.join(".var"));
        fs::rename(&var, d.0.join(".var/x")).unwrap();
        let real = |x: PathBuf| fs::canonicalize(x).unwrap();
        let mut found = vec![(real(outer.clone()), "m".to_owned())];
        add_tagged_targets(&mut found, &[d.0.join(".var/x/CACHEDIR.TAG")]);
        let roots: Vec<PathBuf> = found.iter().map(|(p, _)| p.clone()).collect();
        let mut want = vec![
            real(d.0.join(".var/x")),
            real(outer.clone()),
            real(outer.join("dev-stand/cargo")),
        ];
        want.sort();
        assert_eq!(roots, want);
        // Profiles of the outer target are never taken as nested targets.
        assert!(!roots.contains(&real(outer.join("debug"))));
    }

    #[test]
    fn worktree_detection() {
        let d = tmp("wt");
        let main = d.0.join("main");
        fs::create_dir_all(main.join(".git/worktrees/w")).unwrap();
        fs::write(main.join(".git/worktrees/w/commondir"), "../..\n").unwrap();
        let wt = d.0.join("w");
        fs::create_dir_all(wt.join("sub")).unwrap();
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/w").display()),
        )
        .unwrap();
        assert!(in_worktree(&wt.join("sub")));
        assert!(!in_worktree(&main));
        // A submodule's `.git` file points into `modules`, not `worktrees`.
        let sm = d.0.join("sm");
        fs::create_dir_all(&sm).unwrap();
        fs::write(sm.join(".git"), "gitdir: ../main/.git/modules/sm\n").unwrap();
        assert!(!in_worktree(&sm));
    }

    #[test]
    fn a_worktree_target_loses_stale_finals_of_every_profile() {
        let d = tmp("wtfinal");
        let main = d.0.join("main");
        fs::create_dir_all(main.join(".git/worktrees/w")).unwrap();
        fs::write(main.join(".git/worktrees/w/commondir"), "../..\n").unwrap();
        let wt = d.0.join("w");
        fs::create_dir_all(&wt).unwrap();
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/w").display()),
        )
        .unwrap();
        let t = target(&wt);
        for prof in ["release", "dist"] {
            let p = t.join(prof);
            file(&p.join(".fingerprint/x-00000000000000aa/f"));
            file(&p.join(".cargo-lock"));
            file(&p.join("bin"));
            old(&p.join("bin"));
        }
        let m = Matcher::new(&[], &[], None).unwrap();
        let got = collect(vec![(t.clone(), "r".into())], &m, CUTOFF, KEEP);
        for prof in ["debug", "release", "dist"] {
            let fin = if prof == "debug" { "app" } else { "bin" };
            assert!(
                got[0]
                    .entries
                    .contains(&fs::canonicalize(&t).unwrap().join(prof).join(fin))
                    || got[0].entries.contains(&t.join(prof).join(fin)),
                "{prof}: {:?}",
                got[0].entries
            );
        }
        // Outside a worktree the same finals stay.
        fs::remove_file(wt.join(".git")).unwrap();
        let got = collect(vec![(t, "r".into())], &m, CUTOFF, KEEP);
        assert!(
            !got[0]
                .entries
                .iter()
                .any(|e| e.ends_with("bin") || e.ends_with("app"))
        );
    }

    #[test]
    fn a_tagged_triple_dir_is_not_a_second_target() {
        let d = tmp("triple");
        let t = target(&d.0);
        // Cargo tags `target/<triple>`; its profiles belong to the outer target.
        let tr = target(&d.0.join("x"));
        fs::rename(&tr, t.join("aarch64-apple-darwin")).unwrap();
        // A tag the walk found inside the outer target's profile is not a target either.
        let inner = target(&d.0.join("y"));
        fs::create_dir_all(t.join("debug/build/z-0123456789abcdef")).unwrap();
        fs::rename(&inner, t.join("debug/build/z-0123456789abcdef/out")).unwrap();
        let real = fs::canonicalize(&t).unwrap();
        let mut found = vec![(real.clone(), "m".to_owned())];
        add_tagged_targets(
            &mut found,
            &[t.join("debug/build/z-0123456789abcdef/out/CACHEDIR.TAG")],
        );
        assert_eq!(found, vec![(real, "m".to_owned())]);
        let m = Matcher::new(&[], &[], None).unwrap();
        let got = collect(found, &m, CUTOFF, KEEP);
        assert!(got.iter().all(|t| t.skipped.is_empty()));
    }

    #[test]
    fn a_submodule_named_worktrees_is_not_a_worktree() {
        let d = tmp("smwt");
        let sm = d.0.join("worktrees/x");
        fs::create_dir_all(d.0.join(".git/modules/worktrees/x")).unwrap();
        fs::create_dir_all(&sm).unwrap();
        fs::write(sm.join(".git"), "gitdir: ../../.git/modules/worktrees/x\n").unwrap();
        assert!(!in_worktree(&sm));
    }
}
