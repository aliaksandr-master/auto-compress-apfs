//! Persistent record of the files this tool compressed (`state.tsv`) and deduplicated
//! (`dedup.tsv`), and the per-day streak logic of the quarantine.
//!
//! Format: a header `#v1\tlast_run_day=<D>\tprev_run_day=<D>` (`-` when unknown), then one row per
//! file: `path\tdev\tino\torig_size\tdisk_after\tcompressed_at_unix\tstreak\tlast_found_day`.
//! `dedup.tsv` has the header `#dedup-v1\t..` with the same fields and rows
//! `path\tdev\tino\tctime_sec\tctime_nsec\tdeduped_at_unix\tstreak\tlast_found_day`.
//! Corrupt rows are ignored; paths containing TAB or LF are never stored.

use crate::cache::write_atomic;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub const STATE_NAME: &str = "state.tsv";
pub const DEDUP_STATE_NAME: &str = "dedup.tsv";

/// A local calendar day, stored as `yyyymmdd`; only compared for equality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Day(u32);

impl Day {
    pub fn new(year: i32, month: u32, day: u32) -> Option<Self> {
        let ok = (0..=9999).contains(&year) && (1..=12).contains(&month) && (1..=31).contains(&day);
        ok.then(|| Self(year as u32 * 10_000 + month * 100 + day))
    }

    /// Parses `YYYY-MM-DD`.
    pub fn parse(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
            return None;
        }
        let num = |r: std::ops::Range<usize>| -> Option<u32> {
            let part = &s[r];
            part.bytes()
                .all(|c| c.is_ascii_digit())
                .then(|| part.parse().ok())?
        };
        Self::new(num(0..4)? as i32, num(5..7)?, num(8..10)?)
    }
}

impl fmt::Display for Day {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.0;
        write!(f, "{:04}-{:02}-{:02}", d / 10_000, d / 100 % 100, d % 100)
    }
}

/// One file compressed by this tool (or found compressed under the patterns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub dev: u64,
    pub ino: u64,
    /// Logical size.
    pub orig_size: u64,
    /// Allocated bytes (`st_blocks * 512`) after compression.
    pub disk_after: u64,
    /// Unix seconds of the compression; 0 = unknown (found already compressed).
    pub compressed_at: i64,
    /// Consecutive run days on which the file was found decompressed again.
    pub streak: u32,
    /// Local day the file was last found decompressed again.
    pub last_found_day: Option<Day>,
    /// Found by this run's walk (not stored).
    pub seen: bool,
}

impl Row {
    pub fn new(dev: u64, ino: u64, orig_size: u64, disk_after: u64, compressed_at: i64) -> Self {
        Self {
            dev,
            ino,
            orig_size,
            disk_after,
            compressed_at,
            streak: 0,
            last_found_day: None,
            seen: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct State {
    pub last_run_day: Option<Day>,
    pub prev_run_day: Option<Day>,
    pub rows: HashMap<PathBuf, Row>,
}

/// Paths with TAB or LF cannot be stored in the TSV.
pub fn storable(path: &Path) -> bool {
    !path
        .as_os_str()
        .as_bytes()
        .iter()
        .any(|&b| b == b'\t' || b == b'\n')
}

fn day_field(s: &[u8]) -> Option<Option<Day>> {
    match s {
        b"-" => Some(None),
        _ => Some(Some(Day::parse(std::str::from_utf8(s).ok()?)?)),
    }
}

fn num<T: std::str::FromStr>(s: &[u8]) -> Option<T> {
    std::str::from_utf8(s).ok()?.parse().ok()
}

fn parse_row(line: &[u8]) -> Option<(PathBuf, Row)> {
    let mut it = line.split(|&b| b == b'\t');
    let path = it.next().filter(|p| p.first() == Some(&b'/'))?;
    let row = Row {
        dev: num(it.next()?)?,
        ino: num(it.next()?)?,
        orig_size: num(it.next()?)?,
        disk_after: num(it.next()?)?,
        compressed_at: num(it.next()?)?,
        streak: num(it.next()?)?,
        last_found_day: day_field(it.next()?)?,
        seen: false,
    };
    it.next()
        .is_none()
        .then(|| (PathBuf::from(OsStr::from_bytes(path)), row))
}

fn fmt_day(d: Option<Day>) -> String {
    d.map_or_else(|| "-".to_owned(), |d| d.to_string())
}

/// Parses the lines of a state file: header fields into `last`/`prev`, other lines through
/// `row`. Malformed rows and unknown header fields are ignored.
fn parse_lines(
    bytes: &[u8],
    last: &mut Option<Day>,
    prev: &mut Option<Day>,
    mut row: impl FnMut(&[u8]),
) {
    for line in bytes.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if line.first() == Some(&b'#') {
            for field in line.split(|&b| b == b'\t').skip(1) {
                let text = String::from_utf8_lossy(field);
                match text.split_once('=') {
                    Some(("last_run_day", v)) => *last = Day::parse(v),
                    Some(("prev_run_day", v)) => *prev = Day::parse(v),
                    _ => {}
                }
            }
            continue;
        }
        row(line);
    }
}

/// Reads a state file whose header starts with `tag`; a missing or empty file is `None`.
/// Any other header is an error (`InvalidData`): not ours or not this version, never overwritten.
fn read_tagged(path: &Path, tag: &str) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) if bytes.is_empty() => Ok(None),
        Ok(bytes) => {
            let t = tag.as_bytes();
            let ok = bytes.starts_with(t) && matches!(bytes.get(t.len()), Some(b'\t' | b'\n'));
            if ok {
                Ok(Some(bytes))
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unrecognized header (expected `{tag}`)"),
                ))
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The run day before `today`: `prev` when a run already happened today, else `last`.
fn previous_run_day(last: Option<Day>, prev: Option<Day>, today: Day) -> Option<Day> {
    if last == Some(today) { prev } else { last }
}

/// Records that a run happened on `today` (shifts the header days once per day).
fn mark_run(last: &mut Option<Day>, prev: &mut Option<Day>, today: Day) {
    if *last != Some(today) {
        *prev = *last;
        *last = Some(today);
    }
}

/// Whether a row survives pruning: rows not seen by this run are dropped when their parent
/// directory was read by this run (so the file is gone or no longer matched).
fn keep(path: &Path, seen: bool, read_dirs: &HashSet<PathBuf>) -> bool {
    seen || !path.parent().is_some_and(|p| read_dirs.contains(p))
}

impl State {
    /// Parses state text; malformed rows and unknown header fields are ignored.
    pub fn parse(bytes: &[u8]) -> Self {
        let mut st = State::default();
        let mut rows = HashMap::new();
        parse_lines(bytes, &mut st.last_run_day, &mut st.prev_run_day, |line| {
            if let Some((path, row)) = parse_row(line) {
                rows.insert(path, row);
            }
        });
        st.rows = rows;
        st
    }

    /// Loads the state; a missing or empty file is an empty state. A file without the `#v1`
    /// header is an error (`InvalidData`): it is not ours or not this version, never overwritten.
    pub fn load(path: &Path) -> io::Result<Self> {
        Ok(read_tagged(path, "#v1")?.map_or_else(Self::default, |b| Self::parse(&b)))
    }

    /// The run day before `today`: `prev_run_day` when a run already happened today, else
    /// `last_run_day`.
    pub fn previous_run_day(&self, today: Day) -> Option<Day> {
        previous_run_day(self.last_run_day, self.prev_run_day, today)
    }

    /// Records that a run happened on `today` (shifts the header days once per day).
    pub fn mark_run(&mut self, today: Day) {
        mark_run(&mut self.last_run_day, &mut self.prev_run_day, today);
    }

    /// Upserts the row of a file just compressed: identity and sizes are replaced, the streak
    /// history is kept. Returns false (nothing stored) for paths with TAB/LF.
    pub fn record_compressed(&mut self, path: &Path, row: Row) -> bool {
        if !storable(path) {
            return false;
        }
        match self.rows.get_mut(path) {
            Some(r) => {
                *r = Row {
                    streak: r.streak,
                    last_found_day: r.last_found_day,
                    ..row
                }
            }
            None => {
                self.rows.insert(path.to_path_buf(), row);
            }
        }
        true
    }

    /// Atomically writes the state, streaming the rows; with `read_dirs` rows are pruned.
    pub fn store(&self, path: &Path, read_dirs: Option<&HashSet<PathBuf>>) -> io::Result<()> {
        write_atomic(path, |w| {
            writeln!(
                w,
                "#v1\tlast_run_day={}\tprev_run_day={}",
                fmt_day(self.last_run_day),
                fmt_day(self.prev_run_day)
            )?;
            for (p, r) in &self.rows {
                if !storable(p) || read_dirs.is_some_and(|d| !keep(p, r.seen, d)) {
                    continue;
                }
                w.write_all(p.as_os_str().as_bytes())?;
                writeln!(
                    w,
                    "\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    r.dev,
                    r.ino,
                    r.orig_size,
                    r.disk_after,
                    r.compressed_at,
                    r.streak,
                    fmt_day(r.last_found_day)
                )?;
            }
            Ok(())
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Compress,
    Quarantine,
}

/// A file with a row (compressed by us before) is found decompressed again on `today`.
/// The streak counts consecutive run days: unchanged when already counted today, +1 when the
/// previous count was on the previous run day, else restarts at 1. `threshold` 0 disables.
pub fn on_redecompressed(
    row: &mut Row,
    today: Day,
    previous_run_day: Option<Day>,
    threshold: u32,
) -> Decision {
    count_day(
        &mut row.streak,
        &mut row.last_found_day,
        today,
        previous_run_day,
    );
    decide(row.streak, threshold)
}

/// A file with a row is found compressed: the streak ends, unless it was counted today (a
/// second run on the same day sees the file this day's first run compressed).
pub fn on_compressed(row: &mut Row, today: Day) {
    end_streak(&mut row.streak, row.last_found_day, today);
}

/// Counts `today` once: +1 when the previous count was on the previous run day, else 1.
fn count_day(streak: &mut u32, last_found: &mut Option<Day>, today: Day, prev: Option<Day>) {
    if *last_found != Some(today) {
        if last_found.is_some() && *last_found == prev {
            *streak += 1;
        } else {
            *streak = 1;
        }
        *last_found = Some(today);
    }
}

/// Ends the streak unless it was counted `today`.
fn end_streak(streak: &mut u32, last_found: Option<Day>, today: Day) {
    if last_found != Some(today) {
        *streak = 0;
    }
}

fn decide(streak: u32, threshold: u32) -> Decision {
    if threshold > 0 && streak >= threshold {
        Decision::Quarantine
    } else {
        Decision::Compress
    }
}

/// Identity of a file as found by a walk or after a swap: a rewrite or replacement of the file
/// changes it (as does any metadata change, through ctime).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub dev: u64,
    pub ino: u64,
    pub ctime_sec: i64,
    pub ctime_nsec: i64,
}

impl Identity {
    pub fn of(m: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        Self {
            dev: m.dev(),
            ino: m.ino(),
            ctime_sec: m.ctime(),
            ctime_nsec: m.ctime_nsec(),
        }
    }
}

/// One file this tool deduplicated (made a clone of an identical file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupRow {
    /// The clone swapped in, or the file as last counted unshared.
    pub ident: Identity,
    /// Unix seconds of the last dedup.
    pub deduped_at: i64,
    /// Consecutive run days on which the file was found rewritten and no longer shared.
    pub streak: u32,
    pub last_found_day: Option<Day>,
    /// Found by this run's walk (not stored).
    pub seen: bool,
}

impl DedupRow {
    pub fn new(ident: Identity, deduped_at: i64) -> Self {
        Self {
            ident,
            deduped_at,
            streak: 0,
            last_found_day: None,
            seen: true,
        }
    }
}

/// `dedup.tsv`: the files deduplicated by this tool, with their own run days (dedup runs on
/// its own schedule, separate from compress).
#[derive(Debug, Default)]
pub struct DedupState {
    pub last_run_day: Option<Day>,
    pub prev_run_day: Option<Day>,
    pub rows: HashMap<PathBuf, DedupRow>,
}

fn parse_dedup_row(line: &[u8]) -> Option<(PathBuf, DedupRow)> {
    let mut it = line.split(|&b| b == b'\t');
    let path = it.next().filter(|p| p.first() == Some(&b'/'))?;
    let row = DedupRow {
        ident: Identity {
            dev: num(it.next()?)?,
            ino: num(it.next()?)?,
            ctime_sec: num(it.next()?)?,
            ctime_nsec: num(it.next()?)?,
        },
        deduped_at: num(it.next()?)?,
        streak: num(it.next()?)?,
        last_found_day: day_field(it.next()?)?,
        seen: false,
    };
    it.next()
        .is_none()
        .then(|| (PathBuf::from(OsStr::from_bytes(path)), row))
}

impl DedupState {
    /// Parses dedup state text; malformed rows and unknown header fields are ignored.
    pub fn parse(bytes: &[u8]) -> Self {
        let mut st = DedupState::default();
        let mut rows = HashMap::new();
        parse_lines(bytes, &mut st.last_run_day, &mut st.prev_run_day, |line| {
            if let Some((path, row)) = parse_dedup_row(line) {
                rows.insert(path, row);
            }
        });
        st.rows = rows;
        st
    }

    /// Loads `dedup.tsv`; missing or empty is empty, any header but `#dedup-v1` is
    /// `InvalidData` (never overwritten).
    pub fn load(path: &Path) -> io::Result<Self> {
        Ok(read_tagged(path, "#dedup-v1")?.map_or_else(Self::default, |b| Self::parse(&b)))
    }

    /// See [`State::previous_run_day`].
    pub fn previous_run_day(&self, today: Day) -> Option<Day> {
        previous_run_day(self.last_run_day, self.prev_run_day, today)
    }

    /// See [`State::mark_run`].
    pub fn mark_run(&mut self, today: Day) {
        mark_run(&mut self.last_run_day, &mut self.prev_run_day, today);
    }

    /// Upserts the row of a file just deduplicated: identity and time are replaced, the streak
    /// history is kept. Returns false (nothing stored) for paths with TAB/LF.
    pub fn record_deduped(&mut self, path: &Path, row: DedupRow) -> bool {
        if !storable(path) {
            return false;
        }
        match self.rows.get_mut(path) {
            Some(r) => {
                *r = DedupRow {
                    streak: r.streak,
                    last_found_day: r.last_found_day,
                    ..row
                }
            }
            None => {
                self.rows.insert(path.to_path_buf(), row);
            }
        }
        true
    }

    /// Atomically writes the dedup state; with `read_dirs` rows are pruned (see [`State::store`]).
    pub fn store(&self, path: &Path, read_dirs: Option<&HashSet<PathBuf>>) -> io::Result<()> {
        write_atomic(path, |w| {
            writeln!(
                w,
                "#dedup-v1\tlast_run_day={}\tprev_run_day={}",
                fmt_day(self.last_run_day),
                fmt_day(self.prev_run_day)
            )?;
            for (p, r) in &self.rows {
                if !storable(p) || read_dirs.is_some_and(|d| !keep(p, r.seen, d)) {
                    continue;
                }
                w.write_all(p.as_os_str().as_bytes())?;
                writeln!(
                    w,
                    "\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    r.ident.dev,
                    r.ident.ino,
                    r.ident.ctime_sec,
                    r.ident.ctime_nsec,
                    r.deduped_at,
                    r.streak,
                    fmt_day(r.last_found_day)
                )?;
            }
            Ok(())
        })
    }
}

/// A deduplicated file is a target again (no longer shares its data with an identical file).
/// The streak counts only when the file itself changed since the row was written (`now`
/// differs from the row's identity), so a rewritten reference or an unreadable clone id does
/// not count; a counted change takes over `now` as the row's identity, so one rewrite counts
/// once however many re-dedups fail. An unchanged file is quarantined only while its streak is
/// current (counted today or on the previous run day: an append that failed is retried).
/// `threshold` 0 disables.
pub fn on_unshared(
    row: &mut DedupRow,
    now: Identity,
    today: Day,
    previous_run_day: Option<Day>,
    threshold: u32,
) -> Decision {
    if now != row.ident {
        count_day(
            &mut row.streak,
            &mut row.last_found_day,
            today,
            previous_run_day,
        );
        row.ident = now;
    } else if !streak_current(row, today, previous_run_day) {
        return Decision::Compress;
    }
    decide(row.streak, threshold)
}

/// Whether the row's streak still counts: last counted today or on the previous run day (an
/// older count is stale, a later rewrite restarts it).
pub fn streak_current(row: &DedupRow, today: Day, previous_run_day: Option<Day>) -> bool {
    row.last_found_day == Some(today) || row.last_found_day == previous_run_day
}

/// A deduplicated file is found sharing its data again: the streak ends, unless it was
/// counted today.
pub fn on_shared(row: &mut DedupRow, today: Day) {
    end_streak(&mut row.streak, row.last_found_day, today);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> Day {
        Day::parse(s).unwrap()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acz-state-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn day_parse_and_display() {
        assert_eq!(day("2026-10-08").to_string(), "2026-10-08");
        assert_eq!(Day::new(2026, 1, 2).unwrap().to_string(), "2026-01-02");
        for bad in [
            "2026-13-01",
            "2026-00-10",
            "2026-1-01",
            "20261008",
            "2026-10-0x",
            "",
            "+026-10-08",
        ] {
            assert!(Day::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn roundtrip_with_corrupt_rows_and_unstorable_paths() {
        let dir = temp_dir("rt");
        let path = dir.join("sub").join(STATE_NAME);
        assert!(State::load(&path).unwrap().rows.is_empty());
        let mut st = State::default();
        st.mark_run(day("2026-10-07"));
        st.mark_run(day("2026-10-08"));
        let mut r = Row::new(1, 2, 3000, 1000, 1_700_000_000);
        r.streak = 2;
        r.last_found_day = Some(day("2026-10-08"));
        assert!(st.record_compressed(Path::new("/a/b c.py"), r.clone()));
        assert!(st.record_compressed(Path::new("/a/\u{e9}"), Row::new(1, 3, 5, 4, 0)));
        assert!(!st.record_compressed(Path::new("/a/tab\there"), Row::new(1, 4, 5, 4, 0)));
        assert!(!st.record_compressed(Path::new("/a/lf\nhere"), Row::new(1, 5, 5, 4, 0)));
        st.store(&path, None).unwrap();

        let back = State::load(&path).unwrap();
        assert_eq!(back.last_run_day, Some(day("2026-10-08")));
        assert_eq!(back.prev_run_day, Some(day("2026-10-07")));
        assert_eq!(back.rows.len(), 2);
        let got = &back.rows[Path::new("/a/b c.py")];
        assert_eq!(
            (got.streak, got.last_found_day, got.seen),
            (2, r.last_found_day, false)
        );
        assert_eq!(
            (got.dev, got.ino, got.orig_size, got.disk_after),
            (1, 2, 3000, 1000)
        );

        let mut bytes = fs::read(&path).unwrap();
        bytes.extend_from_slice(
            b"garbage\n/x\t1\t2\t3\t4\t5\t6\n/x\t1\t2\t3\t4\t5\t6\t-\textra\n\
              relative\t1\t2\t3\t4\t5\t6\t-\n/y\t1\t2\t3\t4\t5\tz\t-\n/z\t1\t2\t3\t4\t5\t6\t2026-99-01\n\n",
        );
        fs::write(&path, bytes).unwrap();
        assert_eq!(State::load(&path).unwrap().rows.len(), 2);
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "temp file left behind");

        // Empty is fine; a foreign or corrupt header is refused, not treated as empty.
        fs::write(&path, "").unwrap();
        assert!(State::load(&path).unwrap().rows.is_empty());
        for bad in [
            "garbage\n",
            "/a\t1\t2\t3\t4\t5\t6\t-\n",
            "#v2\tlast_run_day=-\n",
        ] {
            fs::write(&path, bad).unwrap();
            let e = State::load(&path).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{bad}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn record_compressed_keeps_streak_history() {
        let mut st = State::default();
        let mut r = Row::new(1, 2, 10, 5, 100);
        r.streak = 2;
        r.last_found_day = Some(day("2026-10-08"));
        st.rows.insert(PathBuf::from("/f"), r);
        st.record_compressed(Path::new("/f"), Row::new(1, 9, 20, 6, 200));
        let r = &st.rows[Path::new("/f")];
        assert_eq!(
            (r.ino, r.orig_size, r.disk_after, r.compressed_at),
            (9, 20, 6, 200)
        );
        assert_eq!((r.streak, r.last_found_day), (2, Some(day("2026-10-08"))));
    }

    #[test]
    fn pruning_only_below_read_dirs() {
        let dir = temp_dir("prune");
        let path = dir.join(STATE_NAME);
        let mut st = State::default();
        let mut add = |p: &str, seen: bool| {
            let mut r = Row::new(1, 1, 1, 1, 1);
            r.seen = seen;
            st.rows.insert(PathBuf::from(p), r);
        };
        add("/root/read/seen.py", true);
        add("/root/read/vanished.py", false);
        add("/root/read/deeper/unread.py", false);
        add("/refused/root/kept.py", false);
        let read: HashSet<PathBuf> = [PathBuf::from("/root"), PathBuf::from("/root/read")].into();
        st.store(&path, Some(&read)).unwrap();
        let mut left: Vec<_> = State::load(&path).unwrap().rows.into_keys().collect();
        left.sort();
        assert_eq!(
            left,
            [
                "/refused/root/kept.py",
                "/root/read/deeper/unread.py",
                "/root/read/seen.py"
            ]
            .map(PathBuf::from)
        );
        // Without pruning everything stays.
        st.store(&path, None).unwrap();
        assert_eq!(State::load(&path).unwrap().rows.len(), 4);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn streak_by_run_days() {
        let (d1, d2, d3, d5) = (
            day("2026-10-01"),
            day("2026-10-02"),
            day("2026-10-03"),
            day("2026-10-05"),
        );
        let mut r = Row::new(1, 1, 1, 1, 1);
        // First time found decompressed: 1.
        assert_eq!(on_redecompressed(&mut r, d1, None, 3), Decision::Compress);
        assert_eq!(r.streak, 1);
        // Same day again: unchanged.
        assert_eq!(on_redecompressed(&mut r, d1, None, 3), Decision::Compress);
        assert_eq!(r.streak, 1);
        // Next run day: +1.
        assert_eq!(
            on_redecompressed(&mut r, d2, Some(d1), 3),
            Decision::Compress
        );
        assert_eq!(r.streak, 2);
        // Third consecutive run day: quarantine.
        assert_eq!(
            on_redecompressed(&mut r, d3, Some(d2), 3),
            Decision::Quarantine
        );
        assert_eq!(r.streak, 3);
        // A gap (a run day without finding it decompressed): restart.
        let mut r = Row::new(1, 1, 1, 1, 1);
        on_redecompressed(&mut r, d1, None, 3);
        on_redecompressed(&mut r, d3, Some(d2), 3);
        assert_eq!(r.streak, 1);
        // Days without any run in between do not break the streak (run days count).
        on_redecompressed(&mut r, d5, Some(d3), 3);
        assert_eq!(r.streak, 2);
        // Threshold 0 disables.
        r.streak = 99;
        assert_eq!(
            on_redecompressed(&mut r, d5, Some(d3), 0),
            Decision::Compress
        );
    }

    #[test]
    fn found_compressed_resets_unless_counted_today() {
        let (d1, d2) = (day("2026-10-01"), day("2026-10-02"));
        let mut r = Row::new(1, 1, 1, 1, 1);
        r.streak = 2;
        r.last_found_day = Some(d2);
        on_compressed(&mut r, d2);
        assert_eq!(r.streak, 2);
        r.last_found_day = Some(d1);
        on_compressed(&mut r, d2);
        assert_eq!(r.streak, 0);
    }

    fn ident(ino: u64, ctime: i64) -> Identity {
        Identity {
            dev: 1,
            ino,
            ctime_sec: ctime,
            ctime_nsec: 7,
        }
    }

    #[test]
    fn dedup_state_roundtrip_corrupt_rows_and_foreign_header() {
        let dir = temp_dir("dedup-rt");
        let path = dir.join("sub").join(DEDUP_STATE_NAME);
        assert!(DedupState::load(&path).unwrap().rows.is_empty());
        let mut st = DedupState::default();
        st.mark_run(day("2026-10-07"));
        st.mark_run(day("2026-10-08"));
        let mut r = DedupRow::new(ident(2, 1_700_000_000), 1_700_000_001);
        r.streak = 2;
        r.last_found_day = Some(day("2026-10-08"));
        assert!(st.record_deduped(Path::new("/a/b c.py"), r.clone()));
        assert!(!st.record_deduped(Path::new("/a/tab\there"), r.clone()));
        assert!(!st.record_deduped(Path::new("/a/lf\nhere"), r.clone()));
        st.store(&path, None).unwrap();

        let back = DedupState::load(&path).unwrap();
        assert_eq!(back.last_run_day, Some(day("2026-10-08")));
        assert_eq!(back.prev_run_day, Some(day("2026-10-07")));
        assert_eq!(back.rows.len(), 1);
        let got = &back.rows[Path::new("/a/b c.py")];
        assert_eq!(*got, DedupRow { seen: false, ..r });

        let mut bytes = fs::read(&path).unwrap();
        bytes.extend_from_slice(
            b"garbage\n/x\t1\t2\t3\t4\t5\t6\n/x\t1\t2\t3\t4\t5\t6\t-\textra\n\
              relative\t1\t2\t3\t4\t5\t6\t-\n/y\t1\t2\t3\t4\t5\tz\t-\n#dedup-v1\tfuture=1\n\
              /ok\t1\t2\t3\t4\t5\t6\t-\n",
        );
        fs::write(&path, bytes).unwrap();
        let back = DedupState::load(&path).unwrap();
        assert_eq!(back.rows.len(), 2);
        assert_eq!(back.rows[Path::new("/ok")].streak, 6);

        fs::write(&path, "").unwrap();
        assert!(DedupState::load(&path).unwrap().rows.is_empty());
        for bad in [
            "garbage\n",
            "#v1\tlast_run_day=-\n",
            "#dedup-v2\n",
            "#dedup-v1x\n",
        ] {
            fs::write(&path, bad).unwrap();
            let e = DedupState::load(&path).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{bad}");
        }
        // A dedup file is not a compress state either.
        fs::write(&path, "#dedup-v1\n").unwrap();
        assert_eq!(
            State::load(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dedup_state_pruning_only_below_read_dirs() {
        let dir = temp_dir("dedup-prune");
        let path = dir.join(DEDUP_STATE_NAME);
        let mut st = DedupState::default();
        for (p, seen) in [
            ("/root/read/seen", true),
            ("/root/read/vanished", false),
            ("/root/read/deeper/unread", false),
        ] {
            let mut r = DedupRow::new(ident(1, 1), 1);
            r.seen = seen;
            st.rows.insert(PathBuf::from(p), r);
        }
        let read: HashSet<PathBuf> = [PathBuf::from("/root/read")].into();
        st.store(&path, Some(&read)).unwrap();
        let mut left: Vec<_> = DedupState::load(&path).unwrap().rows.into_keys().collect();
        left.sort();
        assert_eq!(
            left,
            ["/root/read/deeper/unread", "/root/read/seen"].map(PathBuf::from)
        );
        st.store(&path, None).unwrap();
        assert_eq!(DedupState::load(&path).unwrap().rows.len(), 3);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn record_deduped_keeps_streak_history() {
        let mut st = DedupState::default();
        let mut r = DedupRow::new(ident(2, 10), 100);
        r.streak = 2;
        r.last_found_day = Some(day("2026-10-08"));
        st.rows.insert(PathBuf::from("/f"), r);
        st.record_deduped(Path::new("/f"), DedupRow::new(ident(9, 20), 200));
        let r = &st.rows[Path::new("/f")];
        assert_eq!((r.ident, r.deduped_at), (ident(9, 20), 200));
        assert_eq!((r.streak, r.last_found_day), (2, Some(day("2026-10-08"))));
    }

    #[test]
    fn unshared_counts_only_a_changed_identity() {
        let (d1, d2, d3, d4) = (
            day("2026-10-01"),
            day("2026-10-02"),
            day("2026-10-03"),
            day("2026-10-04"),
        );
        let mut r = DedupRow::new(ident(1, 1), 1);
        // Same identity (reference rewritten, clone id unreadable): no move.
        assert_eq!(
            on_unshared(&mut r, ident(1, 1), d1, None, 2),
            Decision::Compress
        );
        assert_eq!((r.streak, r.last_found_day), (0, None));
        // Changed: counted, and the new identity is taken over.
        on_unshared(&mut r, ident(2, 1), d1, None, 2);
        assert_eq!((r.streak, r.ident), (1, ident(2, 1)));
        // The same change seen again on the next run day (re-dedup failed): no move.
        on_unshared(&mut r, ident(2, 1), d2, Some(d1), 2);
        assert_eq!(r.streak, 1);
        // A new change after a run day without one: the streak restarts.
        on_unshared(&mut r, ident(2, 5), d3, Some(d2), 2);
        assert_eq!(r.streak, 1);
        // Changed again on the next run day: consecutive, quarantine.
        assert_eq!(
            on_unshared(&mut r, ident(2, 6), d4, Some(d3), 2),
            Decision::Quarantine
        );
        assert_eq!(r.streak, 2);
        // Still at the threshold without a change (quarantine append failed): quarantine again.
        assert_eq!(
            on_unshared(&mut r, ident(2, 6), d4, Some(d3), 2),
            Decision::Quarantine
        );
        // Threshold 0 disables.
        assert_eq!(
            on_unshared(&mut r, ident(2, 6), d4, Some(d3), 0),
            Decision::Compress
        );
        // Shared again: the streak ends, unless counted today.
        on_shared(&mut r, d4);
        assert_eq!(r.streak, 2);
        on_shared(&mut r, day("2026-10-05"));
        assert_eq!(r.streak, 0);
    }

    #[test]
    fn unshared_does_not_requarantine_a_stale_streak() {
        let mut r = DedupRow::new(ident(1, 1), 1);
        r.streak = 3;
        r.last_found_day = Some(day("2026-10-01"));
        let d = on_unshared(
            &mut r,
            ident(1, 1),
            day("2026-10-05"),
            Some(day("2026-10-04")),
            3,
        );
        assert_eq!(d, Decision::Compress);
        assert_eq!((r.streak, r.last_found_day), (3, Some(day("2026-10-01"))));
    }

    #[test]
    fn two_runs_per_day_keep_the_previous_run_day() {
        let (d1, d2) = (day("2026-10-01"), day("2026-10-02"));
        let mut st = State::default();
        st.mark_run(d1);
        // First run on D2 sees D1 as the previous run day ...
        assert_eq!(st.previous_run_day(d2), Some(d1));
        st.mark_run(d2);
        // ... and so does the second run on D2.
        assert_eq!(st.previous_run_day(d2), Some(d1));
        st.mark_run(d2);
        assert_eq!((st.last_run_day, st.prev_run_day), (Some(d2), Some(d1)));
        // Second run of D2 finds a file last counted on D1: streak 2.
        let mut r = Row::new(1, 1, 1, 1, 1);
        r.streak = 1;
        r.last_found_day = Some(d1);
        on_redecompressed(&mut r, d2, st.previous_run_day(d2), 3);
        assert_eq!(r.streak, 2);
    }
}
