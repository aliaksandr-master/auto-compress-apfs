//! `dedup`: identical regular files below the `[dedup]` patterns are replaced by APFS clones
//! of one of them (the reference), so their data is stored once.
//!
//! Candidates go through a funnel: walk (with the compress walk's guard and excludes), cheap
//! `lstat` filters, (dev, size) groups, files that already share a clone id collapsed into
//! one, a BLAKE3 hash of the first and last [`HEAD_TAIL`] bytes, then a full BLAKE3 hash. A
//! target is only replaced when its owner, group, mode, flags and xattrs equal the
//! reference's and neither has an ACL. The replacement is a clone of the reference made in a
//! `.acz-staging-<pid>-<n>` dir next to the target (so a crash leaves a leftover the walk and
//! `clean` already know), byte-compared with the target, swapped in with `RENAME_SWAP` and
//! compared once more after the swap; any failure leaves the target as it was. The reference
//! is only read, never changed.

use crate::compress::{KEEP_MARKER, Verify, mark_keep, read_full, same_bytes, system_time};
use crate::logger::{Level, Logger};
use crate::progress::{abbreviate_home, human};
use crate::sys::{self, O_NOFOLLOW, PROTECTED_FLAGS, SF_DATALESS, UF_COMPRESSED};
use crate::walk::{Candidate, FileKey, RESCUE_PREFIX, STAGING_PREFIX};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fs::{self, DirBuilder, File, FileTimes, Metadata};
use std::hash::Hash;
use std::io::{self, Seek, SeekFrom};
use std::os::macos::fs::{FileTimesExt as _, MetadataExt as _};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Bytes hashed at each end of a file for the partial hash.
pub const HEAD_TAIL: u64 = 64 * 1024;
/// Read buffer of the hashes and compares.
const BUF: usize = 1 << 20;
/// Xattrs that may differ between a target and its reference: the last-used date Finder keeps
/// per file. (The decmpfs and resource fork xattrs of a compressed file are hidden from a plain
/// listing; on an uncompressed file a resource fork is user data and compared.)
const IGNORED_XATTRS: [&CStr; 1] = [c"com.apple.lastuseddate#PS"];
/// Kernel-assigned per creating process: a fresh clone may carry its own value.
const PROVENANCE: &CStr = c"com.apple.provenance";
/// Staging dir numbers of dedup start here, apart from the batch numbers of compress (which
/// runs in the same process under `run`).
static STAGING_NO: AtomicUsize = AtomicUsize::new(1 << 20);

/// A candidate with what the funnel adds: its ctime and clone id at scan time.
#[derive(Debug, Clone)]
pub struct Member {
    pub c: Candidate,
    pub ctime: (i64, i64),
    /// `None` when the filesystem did not report it (then never collapsed with another file).
    pub clone_id: Option<u64>,
}

/// What happens to a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Would be (or will be) replaced by a clone of the reference.
    Pending,
    /// Never touched, with the reason.
    Skipped(String),
}

/// Files with identical content: one reference, the rest targets.
#[derive(Debug)]
pub struct Group {
    pub reference: Member,
    pub targets: Vec<(Member, State)>,
}

/// The groups found and how many candidates the funnel let through at each step.
#[derive(Debug, Default)]
pub struct Plan {
    pub groups: Vec<Group>,
    /// Regular files the walk found.
    pub walked: u64,
    /// Files left out by the clone-id collapse (already clones of a kept file).
    pub already_shared: u64,
    /// Files that changed during the scan or could not be read.
    pub unreadable: u64,
}

impl Plan {
    fn pending(&self) -> impl Iterator<Item = (&Member, &Member)> {
        self.groups.iter().flat_map(|g| {
            g.targets
                .iter()
                .filter(|(_, s)| *s == State::Pending)
                .map(move |(t, _)| (&g.reference, t))
        })
    }
}

/// Funnel step 1: drops files below `min_size`, hardlinked, flagged (uchg/uappnd/schg/sappnd/
/// restricted), dataless, owned by another user than `euid` or modified after `mtime_cutoff`.
pub fn eligible(
    cands: Vec<Candidate>,
    min_size: u64,
    euid: u32,
    mtime_cutoff: i64,
) -> Vec<Candidate> {
    cands
        .into_iter()
        .filter(|c| {
            c.size() >= min_size
                && c.nlink == 1
                && c.flags & (PROTECTED_FLAGS | SF_DATALESS) == 0
                && c.uid == euid
                && c.key.mtime_sec <= mtime_cutoff
        })
        .collect()
}

/// Groups `items` by `key`, keeping groups of at least two, each in input order; groups are
/// ordered by their first item's position.
pub fn regroup<T, K: Hash + Eq>(items: Vec<T>, key: impl Fn(&T) -> K) -> Vec<Vec<T>> {
    let mut index: HashMap<K, usize> = HashMap::new();
    let mut groups: Vec<Vec<T>> = Vec::new();
    for it in items {
        let k = key(&it);
        match index.get(&k) {
            Some(&i) => groups[i].push(it),
            None => {
                index.insert(k, groups.len());
                groups.push(vec![it]);
            }
        }
    }
    groups.retain(|g| g.len() >= 2);
    groups
}

/// Reference order: an already compressed file first, then the oldest mtime, then the
/// lexicographically smallest path.
fn reference_order(m: &Member) -> (bool, i64, i64, &Path) {
    (
        m.c.flags & UF_COMPRESSED == 0,
        m.c.key.mtime_sec,
        m.c.key.mtime_nsec,
        &m.c.path,
    )
}

/// Index of the reference of `members` (see [`reference_order`]).
pub fn pick_reference(members: &[Member]) -> usize {
    (0..members.len())
        .min_by(|&a, &b| reference_order(&members[a]).cmp(&reference_order(&members[b])))
        .unwrap_or(0)
}

/// Funnel step 3: of files with the same clone id only the first in reference order stays;
/// returns the kept members (in reference order) and how many were left out.
pub fn collapse_clones(mut members: Vec<Member>) -> (Vec<Member>, u64) {
    members.sort_by(|a, b| reference_order(a).cmp(&reference_order(b)));
    let before = members.len();
    let mut seen = std::collections::HashSet::new();
    members.retain(|m| m.clone_id.is_none_or(|id| seen.insert(id)));
    let dropped = (before - members.len()) as u64;
    (members, dropped)
}

/// True when `m` (an `fstat`/`lstat` of the member's path) is still the version the scan saw:
/// same dev, inode, size, mtime and ctime.
pub fn unchanged(member: &Member, m: &Metadata) -> bool {
    m.is_file() && FileKey::of(m) == member.c.key && (m.ctime(), m.ctime_nsec()) == member.ctime
}

fn open_nofollow(p: &Path) -> io::Result<File> {
    File::options().read(true).custom_flags(O_NOFOLLOW).open(p)
}

/// Opens the member's file and checks it is still the scanned version; `Ok(None)` when not.
fn open_unchanged(m: &Member) -> io::Result<Option<File>> {
    let f = match open_nofollow(&m.c.path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(unchanged(m, &f.metadata()?).then_some(f))
}

/// BLAKE3 of the first and last [`HEAD_TAIL`] bytes (the whole file when it is shorter than
/// both together); `Ok(None)` when the file changed since the scan.
fn partial_hash(m: &Member, buf: &mut [u8]) -> io::Result<Option<[u8; 32]>> {
    let Some(mut f) = open_unchanged(m)? else {
        return Ok(None);
    };
    let size = m.c.size();
    let mut h = blake3::Hasher::new();
    let head = HEAD_TAIL.min(size) as usize;
    let n = read_full(&f, &mut buf[..head])?;
    h.update(&buf[..n]);
    if size > HEAD_TAIL {
        let from = (size - HEAD_TAIL).max(HEAD_TAIL);
        f.seek(SeekFrom::Start(from))?;
        let n = read_full(&f, &mut buf[..(size - from) as usize])?;
        h.update(&buf[..n]);
    }
    Ok(unchanged(m, &f.metadata()?).then(|| *h.finalize().as_bytes()))
}

/// BLAKE3 of the whole file; `Ok(None)` when the file changed since the scan.
fn full_hash(m: &Member, buf: &mut [u8]) -> io::Result<Option<[u8; 32]>> {
    let Some(f) = open_unchanged(m)? else {
        return Ok(None);
    };
    let mut h = blake3::Hasher::new();
    let mut total = 0u64;
    loop {
        let n = read_full(&f, buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        h.update(&buf[..n]);
    }
    Ok((total == m.c.size() && unchanged(m, &f.metadata()?)).then(|| *h.finalize().as_bytes()))
}

/// The xattrs of `f` as a plain listing shows them, without [`IGNORED_XATTRS`] (and
/// [`PROVENANCE`] when `skip_provenance`), sorted, with their values.
fn xattrs(f: &File, skip_provenance: bool) -> io::Result<Vec<(CString, Option<Vec<u8>>)>> {
    let mut names = sys::flist_xattr(f)?;
    names.retain(|n| {
        !IGNORED_XATTRS.contains(&n.as_c_str()) && !(skip_provenance && n.as_c_str() == PROVENANCE)
    });
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let v = sys::fget_xattr(f, &n, false)?;
            Ok((n, v))
        })
        .collect()
}

/// First difference between `a` and `b` in owner, group, mode, flags (except compressed) and
/// xattrs; with `skip_provenance` [`PROVENANCE`] is not compared.
fn attr_diff(
    a: (&File, &Metadata),
    b: (&File, &Metadata),
    skip_provenance: bool,
) -> io::Result<Option<String>> {
    let (fa, ma) = a;
    let (fb, mb) = b;
    let why = if ma.uid() != mb.uid() {
        "owner"
    } else if ma.gid() != mb.gid() {
        "group"
    } else if ma.mode() != mb.mode() {
        "mode"
    } else if ma.st_flags() & !UF_COMPRESSED != mb.st_flags() & !UF_COMPRESSED {
        "flags"
    } else {
        let (xa, xb) = (xattrs(fa, skip_provenance)?, xattrs(fb, skip_provenance)?);
        if xa.iter().map(|x| &x.0).ne(xb.iter().map(|x| &x.0)) {
            return Ok(Some("xattr names".into()));
        }
        return Ok(xa
            .iter()
            .zip(&xb)
            .find(|(x, y)| x.1 != y.1)
            .map(|(x, _)| format!("xattr {}", x.0.to_string_lossy())));
    };
    Ok(Some(why.into()))
}

/// The metadata gate of a target against its reference: `Ok(None)` when it may be replaced,
/// else the reason it is skipped.
fn gate(r: &Member, t: &Member) -> io::Result<Option<String>> {
    let (Some(fr), Some(ft)) = (open_unchanged(r)?, open_unchanged(t)?) else {
        return Ok(Some("changed since scan".into()));
    };
    if sys::fclone_info(&fr)?.acl || sys::fclone_info(&ft)?.acl {
        return Ok(Some("metadata differs (ACL)".into()));
    }
    let (mr, mt) = (fr.metadata()?, ft.metadata()?);
    if (mr.mode() | mt.mode()) & 0o6000 != 0 {
        return Ok(Some("setuid/setgid".into()));
    }
    Ok(attr_diff((&fr, &mr), (&ft, &mt), false)?.map(|w| format!("metadata differs ({w})")))
}

/// Funnel steps 2 to 6 on the walked candidates (see the module doc).
pub fn plan(cands: Vec<Candidate>, walked: u64, log: &Logger) -> Plan {
    let mut out = Plan {
        walked,
        ..Plan::default()
    };
    let mut buf = vec![0u8; BUF];
    for same_size in regroup(cands, |c| (c.key.dev, c.size())) {
        let members: Vec<Member> = same_size
            .into_iter()
            .filter_map(|c| match scan(c) {
                Ok(Some(m)) => Some(m),
                Ok(None) => {
                    out.unreadable += 1;
                    None
                }
                Err((p, e)) => {
                    log.warn(format_args!("dedup: cannot stat {}: {e}", p.display()));
                    out.unreadable += 1;
                    None
                }
            })
            .collect();
        let (members, shared) = collapse_clones(members);
        out.already_shared += shared;
        if members.len() < 2 {
            continue;
        }
        let hashed = hash_all(members, &mut buf, partial_hash, &mut out, log);
        for part in regroup(hashed, |(_, h)| *h) {
            let members = part.into_iter().map(|(m, _)| m).collect();
            let hashed = hash_all(members, &mut buf, full_hash, &mut out, log);
            for same in regroup(hashed, |(_, h)| *h) {
                let mut members: Vec<Member> = same.into_iter().map(|(m, _)| m).collect();
                let reference = members.swap_remove(pick_reference(&members));
                members.sort_by(|a, b| a.c.path.cmp(&b.c.path));
                let targets = members
                    .into_iter()
                    .map(|t| {
                        let state = match gate(&reference, &t) {
                            Ok(None) => State::Pending,
                            Ok(Some(why)) => State::Skipped(why),
                            Err(e) => State::Skipped(format!("cannot read metadata: {e}")),
                        };
                        (t, state)
                    })
                    .collect();
                out.groups.push(Group { reference, targets });
            }
        }
    }
    out.groups
        .sort_by(|a, b| a.reference.c.path.cmp(&b.reference.c.path));
    out
}

type Hash32 = [u8; 32];

/// Hashes every member with `step`; changed or unreadable ones are left out (and counted).
fn hash_all(
    members: Vec<Member>,
    buf: &mut [u8],
    step: fn(&Member, &mut [u8]) -> io::Result<Option<Hash32>>,
    out: &mut Plan,
    log: &Logger,
) -> Vec<(Member, Hash32)> {
    members
        .into_iter()
        .filter_map(|m| match step(&m, buf) {
            Ok(Some(h)) => Some((m, h)),
            Ok(None) => {
                log.debug(format_args!(
                    "dedup: {} changed during the scan",
                    m.c.path.display()
                ));
                out.unreadable += 1;
                None
            }
            Err(e) => {
                log.warn(format_args!(
                    "dedup: cannot read {}: {e}",
                    m.c.path.display()
                ));
                out.unreadable += 1;
                None
            }
        })
        .collect()
}

/// The scan snapshot of a candidate: its ctime and clone id; `Ok(None)` when the file is no
/// longer the walked version. A clone id that cannot be read is `None`.
fn scan(c: Candidate) -> Result<Option<Member>, (PathBuf, io::Error)> {
    let f = match open_nofollow(&c.path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err((c.path, e)),
    };
    let m = match f.metadata() {
        Ok(m) => m,
        Err(e) => return Err((c.path, e)),
    };
    if !m.is_file() || FileKey::of(&m) != c.key {
        return Ok(None);
    }
    let clone_id = sys::fclone_info(&f).ok().and_then(|i| i.clone_id);
    Ok(Some(Member {
        ctime: (m.ctime(), m.ctime_nsec()),
        c,
        clone_id,
    }))
}

/// The preview: every group with its reference and targets (skipped ones with the reason),
/// the totals and the command that would do it.
pub fn render(plan: &Plan, home: &Path, apply_cmd: &str) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let path = |m: &Member| abbreviate_home(&m.c.path, Some(home));
    for g in &plan.groups {
        let _ = writeln!(
            s,
            "identical: {} files of {}",
            g.targets.len() + 1,
            human(g.reference.c.size())
        );
        let _ = writeln!(s, "  keep  {}", path(&g.reference));
        for (t, state) in &g.targets {
            match state {
                State::Pending => {
                    let _ = writeln!(s, "  clone {}", path(t));
                }
                State::Skipped(why) => {
                    let _ = writeln!(s, "  skip  {}: skipped: {why}", path(t));
                }
            }
        }
    }
    let (files, bytes) = plan
        .pending()
        .fold((0u64, 0u64), |(n, b), (_, t)| (n + 1, b + t.c.allocated()));
    let _ = writeln!(
        s,
        "\ndedup: groups={} files={files} would_free={} (allocated blocks of targets, approx.)",
        plan.groups.len(),
        human(bytes)
    );
    if files > 0 {
        let _ = writeln!(
            s,
            "Nothing changed. Run '{apply_cmd}' to replace duplicates by APFS clones."
        );
    }
    let _ = writeln!(s, "{SNAPSHOT_NOTE}");
    s
}

/// Freed blocks may stay allocated while a snapshot references them.
pub const SNAPSHOT_NOTE: &str =
    "Time Machine local snapshots may keep the freed blocks until they expire.";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub deduped: u64,
    /// Allocated bytes of the replaced targets (approximately what is freed).
    pub freed: u64,
    pub skipped: u64,
    pub failed: u64,
}

/// Why one replacement did not happen.
enum Miss {
    /// Not an error: the target stays as it is.
    Skip(String),
    Fail(String),
    /// Failed, and the staging dir is kept (it may hold the original).
    Kept(String),
}

/// Replaces every pending target by a clone of its reference (see [`replace`]).
pub fn apply(plan: &Plan, log: &Logger) -> Outcome {
    let mut out = Outcome::default();
    let mut bufs = (vec![0u8; BUF], vec![0u8; BUF]);
    for g in &plan.groups {
        for (t, state) in &g.targets {
            if let State::Skipped(why) = state {
                log.info(format_args!(
                    "dedup: {}: skipped: {why}",
                    t.c.path.display()
                ));
                out.skipped += 1;
                continue;
            }
            match replace(&g.reference, t, &mut bufs, &sys::rename_swap) {
                Ok(Done::NotRechecked(level, why)) => {
                    log.log(
                        level,
                        format_args!(
                            "dedup: {}: {why} right after the swap, not re-checked",
                            t.c.path.display()
                        ),
                    );
                    out.skipped += 1;
                }
                Ok(Done::Deduped) => {
                    log.info(format_args!(
                        "deduped {} clone_of={} bytes={}",
                        t.c.path.display(),
                        g.reference.c.path.display(),
                        t.c.size()
                    ));
                    out.deduped += 1;
                    out.freed += t.c.allocated();
                }
                Err(Miss::Skip(why)) => {
                    log.info(format_args!(
                        "dedup: {}: skipped: {why}",
                        t.c.path.display()
                    ));
                    out.skipped += 1;
                }
                Err(Miss::Fail(why) | Miss::Kept(why)) => {
                    log.error(format_args!("dedup: {}: {why}", t.c.path.display()));
                    out.failed += 1;
                }
            }
        }
    }
    out
}

/// Creates a fresh `.acz-staging-<pid>-<n>` dir (mode 0700) in `dir`.
fn make_staging(dir: &Path) -> io::Result<PathBuf> {
    loop {
        let no = STAGING_NO.fetch_add(1, Ordering::Relaxed);
        let staging = dir.join(format!("{STAGING_PREFIX}{}-{no}", std::process::id()));
        match DirBuilder::new().mode(0o700).create(&staging) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            other => return other.map(|()| staging),
        }
    }
}

/// Replaces the target `t` by a clone of the reference `r`: both must still be the scanned
/// versions; the clone (in a staging dir next to `t`) gets `t`'s times, must have `t`'s
/// metadata and bytes, and is swapped in only while `t` is unchanged; after the swap the file
/// at the path is compared with the old `t` again (a difference swaps back). On every miss
/// `t` stays as it was and the clone is removed; `r` is only read. `swap` is
/// [`sys::rename_swap`] in production; tests inject writes through it.
fn replace(
    r: &Member,
    t: &Member,
    bufs: &mut (Vec<u8>, Vec<u8>),
    swap: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> Result<Done, Miss> {
    let changed = || Miss::Skip("changed since scan".into());
    let io_fail = |what: &str, e: io::Error| Miss::Fail(format!("{what}: {e}"));
    let fr = open_unchanged(r).map_err(|e| io_fail("cannot open the reference", e))?;
    let ft = open_unchanged(t).map_err(|e| io_fail("cannot open", e))?;
    let (Some(fr), Some(ft)) = (fr, ft) else {
        return Err(changed());
    };
    let parent = t.c.path.parent().unwrap_or(Path::new("/"));
    let name = t.c.path.file_name().unwrap_or_default();
    let cname = CString::new(name.as_bytes())
        .map_err(|_| Miss::Fail("file name contains a NUL byte".into()))?;
    let staging = make_staging(parent).map_err(|e| io_fail("cannot create a staging dir", e))?;
    let clone = staging.join(name);
    let made = File::open(&staging).and_then(|d| sys::fclone_at(&fr, &d, &cname));
    let result = match made {
        Ok(()) => swap_in(t, &ft, &clone, bufs, swap),
        Err(e) => Err(io_fail("cannot clone the reference", e)),
    };
    // Only after a verified outcome: the replaced original, or the clone after a checked
    // swap back (or one never swapped in).
    if !matches!(result, Err(Miss::Kept(_))) {
        // The marker first: a crash in between leaves a plain staging dir that stale
        // cleanup handles, never a marked one kept forever.
        let _ = fs::remove_dir(staging.join(KEEP_MARKER));
        let _ = fs::remove_file(&clone);
        let _ = fs::remove_dir(&staging);
    }
    result
}

/// Steps after the clone exists at `clone` (see [`replace`]). The caller removes the clone
/// path and the staging dir afterwards unless the result is [`Miss::Kept`].
fn swap_in(
    t: &Member,
    ft: &File,
    clone: &Path,
    bufs: &mut (Vec<u8>, Vec<u8>),
    swap: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> Result<Done, Miss> {
    let io_fail = |what: &str, e: io::Error| Miss::Fail(format!("{what}: {e}"));
    let fc = open_nofollow(clone).map_err(|e| io_fail("cannot open the clone", e))?;
    let cm = fc
        .metadata()
        .map_err(|e| io_fail("cannot stat the clone", e))?;
    if !cm.is_file() || cm.size() != t.c.size() {
        return Err(Miss::Fail(format!(
            "the clone has {} bytes, the file {}",
            cm.size(),
            t.c.size()
        )));
    }
    if cm.gid() != t.c.gid {
        std::os::unix::fs::fchown(&fc, None, Some(t.c.gid))
            .map_err(|e| io_fail("cannot set the clone's group", e))?;
    }
    let times = FileTimes::new()
        .set_accessed(system_time(t.c.atime_sec, t.c.atime_nsec))
        .set_modified(system_time(t.c.key.mtime_sec, t.c.key.mtime_nsec));
    fc.set_times(times)
        .map_err(|e| io_fail("cannot set the clone's times", e))?;
    // Setting an mtime older than the birthtime lowers the birthtime: put the target's back.
    let created = ft
        .metadata()
        .and_then(|m| m.created())
        .and_then(|c| fc.set_times(FileTimes::new().set_created(c)));
    created.map_err(|e| io_fail("cannot set the clone's birthtime", e))?;
    match sys::fclone_info(&fc) {
        Ok(i) if !i.acl => {}
        Ok(_) => return Err(Miss::Fail("the clone has an ACL".into())),
        Err(e) => return Err(io_fail("cannot read the clone's attributes", e)),
    }
    let cm = fc
        .metadata()
        .map_err(|e| io_fail("cannot stat the clone", e))?;
    let tm = ft.metadata().map_err(|e| io_fail("cannot stat", e))?;
    if !unchanged(t, &tm) {
        return Err(Miss::Skip("changed since scan".into()));
    }
    if (cm.mtime(), cm.mtime_nsec()) != (t.c.key.mtime_sec, t.c.key.mtime_nsec) {
        return Err(Miss::Fail("the clone's mtime could not be set".into()));
    }
    // clonefile copies the reference's xattrs, flags and mode: they must be the target's.
    match attr_diff((&fc, &cm), (ft, &tm), true) {
        Ok(None) => {}
        Ok(Some(w)) => return Err(Miss::Fail(format!("the clone's metadata differs ({w})"))),
        Err(e) => return Err(io_fail("cannot read the clone's metadata", e)),
    }
    let (a, b) = (&mut bufs.0[..], &mut bufs.1[..]);
    let compared = (|| {
        let mut f = ft;
        f.seek(SeekFrom::Start(0))?;
        same_bytes(ft, &fc, a, b)
    })();
    // A write during the compare shows as a changed target, not as a mismatch.
    let tm = ft.metadata().map_err(|e| io_fail("cannot stat", e))?;
    let at_path = fs::symlink_metadata(&t.c.path).is_ok_and(|m| unchanged(t, &m));
    if !unchanged(t, &tm) || !at_path {
        return Err(Miss::Skip("changed since scan".into()));
    }
    match compared {
        Ok(Verify::Same) => {}
        Ok(_) => {
            return Err(Miss::Fail(
                "the clone differs from the file, not replaced".into(),
            ));
        }
        Err(e) => return Err(io_fail("cannot compare with the clone", e)),
    }
    mark_keep(clone.parent().unwrap_or(Path::new("/")))
        .map_err(|e| io_fail("cannot mark the staging dir", e))?;
    swap(clone, &t.c.path).map_err(|e| io_fail("cannot swap in the clone", e))?;
    // The old file is at `clone` now, the clone at the path.
    let what = match recheck(t, ft, &cm, clone, bufs) {
        Recheck::Same => return Ok(Done::Deduped),
        Recheck::Replaced(level, why) => return Ok(Done::NotRechecked(level, why)),
        Recheck::Back(what) => what,
    };
    if let Err(e) = swap(clone, &t.c.path) {
        return Err(Miss::Kept(keep_original(
            clone,
            &format!("{what}; the swap back failed ({e})"),
            "the original is preserved at",
        )));
    }
    // The original must be back at the path and the untouched clone in staging before the
    // clone is deleted. Not the ctime: the renames of the swap and the swap back change it.
    // Residual risk, as in compress: a write to the clone that restores its size and mtime
    // between the recheck and here goes unnoticed and is deleted with the clone.
    let clone_back = fs::symlink_metadata(clone).is_ok_and(|m| {
        m.is_file()
            && m.ino() == cm.ino()
            && m.size() == cm.size()
            && (m.mtime(), m.mtime_nsec()) == (cm.mtime(), cm.mtime_nsec())
    });
    let original_back = fs::symlink_metadata(&t.c.path).is_ok_and(|m| m.ino() == t.c.key.ino);
    if !(clone_back && original_back) {
        return Err(Miss::Kept(keep_original(
            clone,
            &format!("{what}; the files changed during the swap back"),
            "nothing deleted: check both the path and",
        )));
    }
    Err(Miss::Fail(format!("{what}; original restored")))
}

/// A replacement that went through.
enum Done {
    Deduped,
    /// Swapped in, but the path changed right after the swap (a user's write or rename): left
    /// as it is, with the log level and the reason.
    NotRechecked(Level, String),
}

/// What the compare after the swap found.
enum Recheck {
    Same,
    /// The path no longer holds the untouched clone (a write or a rename by someone else):
    /// that version stays, nothing is swapped back.
    Replaced(Level, String),
    /// Swap the original back, with the reason.
    Back(String),
}

/// After the swap: the path must still hold the clone (`cm`: inode, size, mtime), `old` the
/// unchanged original `ft`; the clone's cached pages are dropped and its bytes compared with
/// the original's. A change of the file at the path during the compare (its ctime) means a
/// user wrote to it: [`Recheck::Replaced`].
fn recheck(
    t: &Member,
    ft: &File,
    cm: &Metadata,
    old: &Path,
    bufs: &mut (Vec<u8>, Vec<u8>),
) -> Recheck {
    let is_clone = |m: &Metadata| {
        m.is_file()
            && m.ino() == cm.ino()
            && m.size() == cm.size()
            && (m.mtime(), m.mtime_nsec()) == (cm.mtime(), cm.mtime_nsec())
    };
    let replaced = || Recheck::Replaced(Level::Debug, "no longer the swapped-in clone".into());
    let (cur, now) = match open_nofollow(&t.c.path).and_then(|f| f.metadata().map(|m| (f, m))) {
        Ok(v) => v,
        Err(e) => {
            // NotFound, PermissionDenied, or ELOOP (a symlink now) are someone else's doing.
            let expected = matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) || e.raw_os_error() == Some(libc::ELOOP);
            let level = if expected { Level::Debug } else { Level::Warn };
            return Recheck::Replaced(level, format!("cannot be opened ({e})"));
        }
    };
    if !is_clone(&now) {
        return replaced();
    }
    let original_kept = || {
        ft.metadata().is_ok_and(|m| FileKey::of(&m) == t.c.key)
            && fs::symlink_metadata(old).is_ok_and(|m| m.ino() == t.c.key.ino)
    };
    if !original_kept() {
        return Recheck::Back("the original changed right after the swap".into());
    }
    let compared = (|| {
        sys::evict_cached(&cur, t.c.size())?;
        let mut f = ft;
        f.seek(SeekFrom::Start(0))?;
        same_bytes(ft, &cur, &mut bufs.0, &mut bufs.1)
    })();
    let still = cur.metadata().is_ok_and(|m| {
        is_clone(&m) && (m.ctime(), m.ctime_nsec()) == (now.ctime(), now.ctime_nsec())
    }) && fs::symlink_metadata(&t.c.path).is_ok_and(|m| m.ino() == cm.ino());
    if !still {
        return replaced();
    }
    if !original_kept() {
        return Recheck::Back("the original changed right after the swap".into());
    }
    match compared {
        Ok(Verify::Same) => Recheck::Same,
        Ok(_) => Recheck::Back("the file at the path differs from the original".into()),
        Err(e) => Recheck::Back(format!("cannot re-check after the swap: {e}")),
    }
}

/// The staging dir (holding whatever is at `old`) is marked and moved to
/// `.acz-rescue-<pid>-<n>`, which no command but `clean --apply` removes. Returns the failure
/// text: `why`, then `held` followed by where the file is now.
fn keep_original(old: &Path, why: &str, held: &str) -> String {
    let staging = old.parent().unwrap_or(Path::new("/"));
    let _ = mark_keep(staging);
    let name = staging
        .file_name()
        .map(|n| {
            n.to_string_lossy()
                .replacen(STAGING_PREFIX, RESCUE_PREFIX, 1)
        })
        .unwrap_or_default();
    let rescue = staging.with_file_name(name);
    let at = match fs::rename(staging, &rescue) {
        Ok(()) => rescue.join(old.file_name().unwrap_or_default()),
        Err(_) => old.to_path_buf(),
    };
    format!("{why}; {held} {}", at.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn member(
        path: &str,
        ino: u64,
        size: u64,
        mtime: i64,
        flags: u32,
        clone_id: Option<u64>,
    ) -> Member {
        Member {
            c: Candidate {
                path: PathBuf::from(path),
                match_root: Arc::from(Path::new("/r")),
                key: FileKey {
                    dev: 1,
                    ino,
                    size,
                    mtime_sec: mtime,
                    mtime_nsec: 0,
                },
                atime_sec: 0,
                atime_nsec: 0,
                blocks: size.div_ceil(512),
                mode: 0o100644,
                nlink: 1,
                uid: 501,
                gid: 20,
                flags,
            },
            ctime: (mtime, 0),
            clone_id,
        }
    }

    #[test]
    fn eligible_drops_what_is_never_touched() {
        let ok = member("/r/ok", 1, 100, 10, 0, None).c;
        let mut cands = vec![ok.clone()];
        let mut with = |f: &dyn Fn(&mut Candidate)| {
            let mut c = ok.clone();
            f(&mut c);
            cands.push(c);
        };
        with(&|c| c.key.size = 99);
        with(&|c| c.nlink = 2);
        with(&|c| c.flags = sys::UF_IMMUTABLE);
        with(&|c| c.flags = SF_DATALESS);
        with(&|c| c.uid = 0);
        with(&|c| c.key.mtime_sec = 11);
        with(&|c| c.flags = UF_COMPRESSED);
        let kept = eligible(cands, 100, 501, 10);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|c| c.nlink == 1 && c.uid == 501));
    }

    #[test]
    fn regroup_keeps_groups_of_two_in_order() {
        let g = regroup(
            vec![(1, 'a'), (2, 'b'), (1, 'c'), (3, 'd'), (2, 'e'), (1, 'f')],
            |x| x.0,
        );
        assert_eq!(
            g,
            [vec![(1, 'a'), (1, 'c'), (1, 'f')], vec![(2, 'b'), (2, 'e')]]
        );
        assert!(regroup(vec![1, 2, 3], |x| *x).is_empty());
    }

    #[test]
    fn reference_is_compressed_then_oldest_then_smallest_path() {
        let ms = vec![
            member("/r/b", 1, 9, 5, 0, None),
            member("/r/a", 2, 9, 5, 0, None),
            member("/r/c", 3, 9, 4, 0, None),
        ];
        assert_eq!(ms[pick_reference(&ms)].c.path, Path::new("/r/c"));
        let ms = vec![
            member("/r/b", 1, 9, 5, 0, None),
            member("/r/a", 2, 9, 5, 0, None),
        ];
        assert_eq!(ms[pick_reference(&ms)].c.path, Path::new("/r/a"));
        let ms = vec![
            member("/r/c", 3, 9, 4, 0, None),
            member("/r/z", 4, 9, 9, UF_COMPRESSED, None),
        ];
        assert_eq!(ms[pick_reference(&ms)].c.path, Path::new("/r/z"));
    }

    #[test]
    fn clones_collapse_to_the_first_in_reference_order() {
        let ms = vec![
            member("/r/b", 1, 9, 5, 0, Some(7)),
            member("/r/a", 2, 9, 5, 0, Some(7)),
            member("/r/c", 3, 9, 6, 0, None),
            member("/r/d", 4, 9, 6, 0, None),
            member("/r/e", 5, 9, 6, 0, Some(8)),
        ];
        let (kept, dropped) = collapse_clones(ms);
        assert_eq!(dropped, 1);
        let paths: Vec<_> = kept.iter().map(|m| m.c.path.to_str().unwrap()).collect();
        assert_eq!(paths, ["/r/a", "/r/c", "/r/d", "/r/e"]);
    }

    #[test]
    fn recheck_sees_a_target_modified_after_the_scan() {
        let dir = std::env::temp_dir().join(format!("acz-dedup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t");
        fs::write(&p, vec![1u8; 4096]).unwrap();
        let m = fs::symlink_metadata(&p).unwrap();
        let snap = scan(Candidate::new(p.clone(), Arc::from(dir.as_path()), &m))
            .unwrap()
            .unwrap();
        assert!(unchanged(&snap, &fs::symlink_metadata(&p).unwrap()));
        assert!(open_unchanged(&snap).unwrap().is_some());
        // Same size and mtime put back: only the ctime shows the write.
        let f = File::options().write(true).open(&p).unwrap();
        std::os::unix::fs::FileExt::write_at(&f, b"x", 0).unwrap();
        f.set_modified(system_time(m.mtime(), m.mtime_nsec()))
            .unwrap();
        let now = fs::symlink_metadata(&p).unwrap();
        assert_eq!(FileKey::of(&now), snap.c.key);
        assert!(!unchanged(&snap, &now));
        assert!(open_unchanged(&snap).unwrap().is_none());
        let mut buf = vec![0u8; BUF];
        assert_eq!(full_hash(&snap, &mut buf).unwrap(), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn partial_hash_reads_head_and_tail_only() {
        let dir = std::env::temp_dir().join(format!("acz-dedup-ph-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut data = vec![3u8; 300_000];
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::write(&a, &data).unwrap();
        data[150_000] = 4;
        fs::write(&b, &data).unwrap();
        let snap = |p: &Path| {
            let m = fs::symlink_metadata(p).unwrap();
            scan(Candidate::new(
                p.to_path_buf(),
                Arc::from(dir.as_path()),
                &m,
            ))
            .unwrap()
            .unwrap()
        };
        let (ma, mb) = (snap(&a), snap(&b));
        let mut buf = vec![0u8; BUF];
        assert_eq!(
            partial_hash(&ma, &mut buf).unwrap(),
            partial_hash(&mb, &mut buf).unwrap()
        );
        assert_ne!(
            full_hash(&ma, &mut buf).unwrap(),
            full_hash(&mb, &mut buf).unwrap()
        );
        assert_eq!(
            full_hash(&ma, &mut buf).unwrap(),
            Some(*blake3::hash(&vec![3u8; 300_000]).as_bytes())
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A reference and an identical target in a fresh temp dir, scanned.
    fn pair(tag: &str) -> (PathBuf, Member, Member) {
        let dir = std::env::temp_dir().join(format!("acz-dedup-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let data = vec![5u8; 300_000];
        let snap = |name: &str| {
            let p = dir.join(name);
            fs::write(&p, &data).unwrap();
            let m = fs::symlink_metadata(&p).unwrap();
            scan(Candidate::new(p, Arc::from(dir.as_path()), &m))
                .unwrap()
                .unwrap()
        };
        let (r, t) = (snap("r"), snap("t"));
        (dir, r, t)
    }

    fn staging_in(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let n = p.file_name().unwrap().to_string_lossy().into_owned();
                n.starts_with(STAGING_PREFIX) || n.starts_with(RESCUE_PREFIX)
            })
            .collect()
    }

    #[test]
    fn write_right_after_the_swap_is_kept_without_swap_back() {
        let (dir, r, t) = pair("after-write");
        let calls = std::cell::Cell::new(0);
        let swap = |a: &Path, b: &Path| {
            calls.set(calls.get() + 1);
            sys::rename_swap(a, b)?;
            fs::write(b, "user wrote this").unwrap();
            Ok(())
        };
        let mut bufs = (vec![0u8; BUF], vec![0u8; BUF]);
        assert!(matches!(
            replace(&r, &t, &mut bufs, &swap),
            Ok(Done::NotRechecked(Level::Debug, _))
        ));
        assert_eq!(calls.get(), 1, "swapped back");
        assert_eq!(fs::read_to_string(&t.c.path).unwrap(), "user wrote this");
        assert_eq!(fs::symlink_metadata(&r.c.path).unwrap().ino(), r.c.key.ino);
        assert_eq!(fs::read(&r.c.path).unwrap(), vec![5u8; 300_000]);
        assert!(staging_in(&dir).is_empty(), "{:?}", staging_in(&dir));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn changed_original_after_the_swap_is_swapped_back_under_the_keep_marker() {
        let (dir, r, t) = pair("swap-back");
        let marked = std::cell::RefCell::new(Vec::new());
        let swap = |a: &Path, b: &Path| {
            marked
                .borrow_mut()
                .push(crate::compress::has_keep_marker(a.parent().unwrap()));
            sys::rename_swap(a, b)?;
            if marked.borrow().len() == 1 {
                // A write through an fd of the old file, now in staging.
                let f = File::options().append(true).open(a).unwrap();
                std::io::Write::write_all(&mut &f, b"x").unwrap();
            }
            Ok(())
        };
        let mut bufs = (vec![0u8; BUF], vec![0u8; BUF]);
        match replace(&r, &t, &mut bufs, &swap) {
            Err(Miss::Fail(why)) => assert!(why.ends_with("original restored"), "{why}"),
            _ => panic!("not a failure"),
        }
        assert_eq!(*marked.borrow(), [true, true]);
        let m = fs::symlink_metadata(&t.c.path).unwrap();
        assert_eq!(m.ino(), t.c.key.ino);
        assert!(fs::read(&t.c.path).unwrap().ends_with(b"x"));
        assert!(staging_in(&dir).is_empty(), "{:?}", staging_in(&dir));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replace_keeps_times_and_shares_the_clone_id() {
        let (dir, r, t) = pair("ok");
        let before = fs::symlink_metadata(&t.c.path).unwrap();
        let mut bufs = (vec![0u8; BUF], vec![0u8; BUF]);
        assert!(matches!(
            replace(&r, &t, &mut bufs, &sys::rename_swap),
            Ok(Done::Deduped)
        ));
        let after = fs::symlink_metadata(&t.c.path).unwrap();
        assert_ne!(after.ino(), before.ino());
        assert_eq!(after.st_birthtime(), before.st_birthtime());
        assert_eq!(
            (after.mtime(), after.mtime_nsec()),
            (before.mtime(), before.mtime_nsec())
        );
        let id = |p: &Path| sys::fclone_info(&File::open(p).unwrap()).unwrap().clone_id;
        assert_eq!(id(&t.c.path), id(&r.c.path));
        assert_eq!(fs::symlink_metadata(&r.c.path).unwrap().ino(), r.c.key.ino);
        assert!(staging_in(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
