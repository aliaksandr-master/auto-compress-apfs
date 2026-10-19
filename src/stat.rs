//! `stat` command: totals and a per-file table of `state.tsv`, largest original first.

use crate::progress::{human, truncate_left};
use crate::quarantine::Entry;
use crate::state::{Row, State};
use crate::sys;
use std::fmt::Write as _;
use std::path::PathBuf;

/// Rows shown without `--top`/`--all`.
pub const DEFAULT_TOP: usize = 30;
/// Width when `COLUMNS` is not set.
pub const DEFAULT_WIDTH: usize = 160;

const NUM: usize = 10;
const RATIO: usize = 6;
const WHEN: usize = 16;

/// `orig / disk` with two decimals, `inf` for nothing on disk.
fn ratio(orig: u64, disk: u64) -> String {
    if disk == 0 {
        "inf".to_owned()
    } else {
        format!("{:.2}", orig as f64 / disk as f64)
    }
}

/// `orig - disk`, negative when compression made a file bigger.
fn saved(orig: u64, disk: u64) -> String {
    if disk > orig {
        format!("-{}", human(disk - orig))
    } else {
        human(orig - disk)
    }
}

/// `YYYY-MM-DD HH:MM` local time, `-` when unknown.
pub(crate) fn when(secs: i64) -> String {
    match (secs > 0).then(|| sys::local_time(secs)).flatten() {
        Some(t) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            t.year, t.month, t.day, t.hour, t.minute
        ),
        None => "-".to_owned(),
    }
}

/// The whole `stat` output. `top = None` lists every row; `width` bounds each table line.
pub fn render(state: &State, quarantine: &[Entry], top: Option<usize>, width: usize) -> String {
    let (orig, disk) = state.rows.values().fold((0u64, 0u64), |(o, d), r| {
        (o + r.orig_size, d + r.disk_after)
    });
    let mut out = String::new();
    let last = state
        .last_run_day
        .map_or_else(|| "-".to_owned(), |d| d.to_string());
    let _ = writeln!(out, "files          {}", state.rows.len());
    let _ = writeln!(out, "original       {}", human(orig));
    let _ = writeln!(out, "on disk        {}", human(disk));
    let _ = writeln!(out, "saved          {}", saved(orig, disk));
    let _ = writeln!(out, "ratio          {}", ratio(orig, disk));
    let _ = writeln!(out, "quarantined    {}", quarantine.len());
    let _ = writeln!(out, "last run day   {last}");

    let mut rows: Vec<(&PathBuf, &Row)> = state.rows.iter().collect();
    rows.sort_unstable_by(|a, b| b.1.orig_size.cmp(&a.1.orig_size).then_with(|| a.0.cmp(b.0)));
    let shown = top.map_or(rows.len(), |n| n.min(rows.len()));
    if shown > 0 {
        let idx = shown.to_string().len().max(1);
        // "#", 3 sizes, ratio, timestamp, two spaces between columns.
        let fixed = idx + 3 * (NUM + 2) + (RATIO + 2) + (WHEN + 2) + 2;
        let path_w = width.saturating_sub(fixed).max(20);
        let header = format!(
            "{:>idx$}  {:>NUM$}  {:>NUM$}  {:>NUM$}  {:>RATIO$}  {:<WHEN$}  PATH",
            "#", "ORIGINAL", "ON DISK", "SAVED", "RATIO", "COMPRESSED AT"
        );
        let _ = writeln!(out, "\n{header}");
        let _ = writeln!(out, "{}", "-".repeat((fixed + path_w).min(width)));
        for (i, (path, r)) in rows.iter().take(shown).enumerate() {
            let p = truncate_left(&path.to_string_lossy(), path_w);
            let _ = writeln!(
                out,
                "{:>idx$}  {:>NUM$}  {:>NUM$}  {:>NUM$}  {:>RATIO$}  {:<WHEN$}  {p}",
                i + 1,
                human(r.orig_size),
                human(r.disk_after),
                saved(r.orig_size, r.disk_after),
                ratio(r.orig_size, r.disk_after),
                when(r.compressed_at),
            );
        }
        if shown < rows.len() {
            let _ = writeln!(
                out,
                "... {} more files (--top N or --all to list them)",
                rows.len() - shown
            );
        }
    }
    if !quarantine.is_empty() {
        let _ = writeln!(out, "\nquarantine ({}):", quarantine.len());
        for e in quarantine {
            let stamp = if e.stamp.is_empty() { "-" } else { &e.stamp };
            let _ = writeln!(out, "  {stamp}  {}", e.path.display());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        let mut st = State {
            last_run_day: crate::state::Day::parse("2026-10-08"),
            ..State::default()
        };
        for (p, orig, disk, at) in [
            ("/w/small.py", 2048u64, 1024u64, 1_700_000_000i64),
            ("/w/big.bin", 3 << 30, 1 << 30, 0),
            ("/w/zero.txt", 100, 0, 1_700_000_000),
            ("/w/grew.txt", 1000, 4096, 1_700_000_000),
        ] {
            st.rows
                .insert(PathBuf::from(p), Row::new(1, 1, orig, disk, at));
        }
        st
    }

    #[test]
    fn totals_and_sorting() {
        let q = [Entry {
            stamp: "2026-10-08T09:00:00.000+03:00".into(),
            path: PathBuf::from("/w/q.py"),
        }];
        let text = render(&state(), &q, Some(DEFAULT_TOP), 160);
        assert!(text.is_ascii(), "{text}");
        assert!(text.contains("files          4\n"), "{text}");
        assert!(text.contains("quarantined    1\n"), "{text}");
        assert!(text.contains("last run day   2026-10-08\n"), "{text}");
        let order: Vec<&str> = text
            .lines()
            .filter_map(|l| l.split_whitespace().last())
            .filter(|w| w.starts_with("/w/"))
            .collect();
        assert_eq!(
            order,
            [
                "/w/big.bin",
                "/w/small.py",
                "/w/grew.txt",
                "/w/zero.txt",
                "/w/q.py"
            ]
        );
        let big = text.lines().find(|l| l.ends_with("/w/big.bin")).unwrap();
        assert!(
            big.contains("   3.0 GiB     1.0 GiB     2.0 GiB    3.00  -"),
            "{big}"
        );
        let grew = text.lines().find(|l| l.ends_with("/w/grew.txt")).unwrap();
        assert!(grew.contains("-3.0 KiB") && grew.contains("0.24"), "{grew}");
        let zero = text.lines().find(|l| l.ends_with("/w/zero.txt")).unwrap();
        assert!(zero.contains("   inf  "), "{zero}");
        assert!(text.contains("\nquarantine (1):\n  2026-10-08T09:00:00.000+03:00  /w/q.py\n"));
    }

    #[test]
    fn top_and_truncation() {
        let mut st = state();
        let long = format!("/w/{}/end.py", "x".repeat(300));
        st.rows
            .insert(PathBuf::from(&long), Row::new(1, 1, 1, 1, 0));
        let text = render(&st, &[], Some(2), 100);
        assert!(text.contains("... 3 more files"), "{text}");
        assert!(!text.contains("end.py"));
        let text = render(&st, &[], None, 100);
        let line = text.lines().find(|l| l.ends_with("/end.py")).unwrap();
        assert!(line.contains("..."), "{line}");
        assert_eq!(line.chars().count(), 100, "{line}");
        assert!(text.lines().all(|l| l.chars().count() <= 100), "{text}");
    }

    #[test]
    fn empty_state() {
        let text = render(&State::default(), &[], Some(DEFAULT_TOP), 160);
        assert!(text.contains("files          0\n") && text.contains("ratio          inf"));
        assert!(!text.contains("ORIGINAL"));
    }
}
