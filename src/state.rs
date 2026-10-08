//! Persistent record of the files this tool compressed (`state.tsv`) and the per-day streak
//! logic of the quarantine.
//!
//! Format: a header `#v1\tlast_run_day=<D>\tprev_run_day=<D>` (`-` when unknown), then one row per
//! file: `path\tdev\tino\torig_size\tdisk_after\tcompressed_at_unix\tstreak\tlast_found_day`.
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

impl State {
    /// Parses state text; malformed rows and unknown header fields are ignored.
    pub fn parse(bytes: &[u8]) -> Self {
        let mut st = State::default();
        for line in bytes.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if line.first() == Some(&b'#') {
                for field in line.split(|&b| b == b'\t').skip(1) {
                    let text = String::from_utf8_lossy(field);
                    match text.split_once('=') {
                        Some(("last_run_day", v)) => st.last_run_day = Day::parse(v),
                        Some(("prev_run_day", v)) => st.prev_run_day = Day::parse(v),
                        _ => {}
                    }
                }
                continue;
            }
            if let Some((path, row)) = parse_row(line) {
                st.rows.insert(path, row);
            }
        }
        st
    }

    /// Loads the state; a missing or empty file is an empty state. A file without the `#v1`
    /// header is an error (`InvalidData`): it is not ours or not this version, never overwritten.
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read(path) {
            Ok(bytes)
                if !bytes.is_empty()
                    && !(bytes.starts_with(b"#v1\t") || bytes.starts_with(b"#v1\n")) =>
            {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unrecognized header (expected `#v1`)",
                ))
            }
            Ok(bytes) => Ok(Self::parse(&bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// The run day before `today`: `prev_run_day` when a run already happened today, else
    /// `last_run_day`.
    pub fn previous_run_day(&self, today: Day) -> Option<Day> {
        if self.last_run_day == Some(today) {
            self.prev_run_day
        } else {
            self.last_run_day
        }
    }

    /// Records that a run happened on `today` (shifts the header days once per day).
    pub fn mark_run(&mut self, today: Day) {
        if self.last_run_day != Some(today) {
            self.prev_run_day = self.last_run_day;
            self.last_run_day = Some(today);
        }
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

    /// Whether a row survives pruning: rows not seen by this run are dropped when their parent
    /// directory was read by this run (so the file is gone or no longer matched).
    fn keep(path: &Path, row: &Row, read_dirs: &HashSet<PathBuf>) -> bool {
        row.seen || !path.parent().is_some_and(|p| read_dirs.contains(p))
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
                if !storable(p) || read_dirs.is_some_and(|d| !Self::keep(p, r, d)) {
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
    if row.last_found_day != Some(today) {
        if row.last_found_day.is_some() && row.last_found_day == previous_run_day {
            row.streak += 1;
        } else {
            row.streak = 1;
        }
        row.last_found_day = Some(today);
    }
    if threshold > 0 && row.streak >= threshold {
        Decision::Quarantine
    } else {
        Decision::Compress
    }
}

/// A file with a row is found compressed: the streak ends, unless it was counted today (a
/// second run on the same day sees the file this day's first run compressed).
pub fn on_compressed(row: &mut Row, today: Day) {
    if row.last_found_day != Some(today) {
        row.streak = 0;
    }
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
