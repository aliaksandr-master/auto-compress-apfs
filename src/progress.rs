//! Interactive terminal output: one line per file event above a progress line that is redrawn
//! in place. Owned by the [`Logger`](crate::logger::Logger) and driven under its mutex, so event
//! lines and the progress line never interleave. Plain ASCII except the Unicode blocks of the bar;
//! colours only when enabled.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

/// Clears the current terminal line and returns the cursor to its start.
const CLEAR: &str = "\r\x1b[2K";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";
/// Redraws of the progress line alone are throttled to this interval (10 per second).
const REDRAW_EVERY: Duration = Duration::from_millis(100);
/// No estimate before this much time of compress data.
const ETA_AFTER: Duration = Duration::from_secs(2);
/// Width reserved right of the path column of a `compressed` event line.
const RIGHT_COLUMN: usize = 34;
const MIN_PATH_COLUMN: usize = 20;

/// What happened to one file, for the terminal line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEvent<'a> {
    /// Logical size before, allocated bytes after.
    Compressed { size: u64, after: u64 },
    /// `ERROR(<kind>)`: ditto, verify, swap, perm, timeout, staging, io.
    Error(&'a str),
    /// `SKIP(<kind>)`: hardlink, owner, flag, setuid, quarantine, changed, lowspace, dataless.
    Skip(&'a str),
}

enum Phase {
    Scan {
        found: u64,
    },
    Compress {
        total_files: u64,
        total_bytes: u64,
        done_files: u64,
        done_bytes: u64,
        started: Instant,
    },
}

/// Terminal state: phase counters, line width, and whether a progress line is on screen.
pub struct Progress {
    width: usize,
    color: bool,
    home: Option<Box<Path>>,
    phase: Phase,
    last_draw: Option<Instant>,
    shown: bool,
    /// The line on screen was drawn right after an event line, before that file's counters were
    /// advanced: the next draw is not throttled.
    stale: bool,
}

impl Progress {
    pub fn new(width: usize, color: bool, home: Option<&Path>) -> Self {
        Self {
            width: width.max(40),
            color,
            home: home.map(Box::from),
            phase: Phase::Scan { found: 0 },
            last_draw: None,
            shown: false,
            stale: false,
        }
    }

    /// Erases the progress line, if one is on screen.
    pub fn clear(&mut self, w: &mut impl Write) {
        if self.shown {
            let _ = w.write_all(CLEAR.as_bytes());
            self.shown = false;
        }
    }

    /// Redraws the progress line; without `force` at most every [`REDRAW_EVERY`] (unless the
    /// line is stale after an event).
    pub fn draw(&mut self, w: &mut impl Write, force: bool) {
        let now = Instant::now();
        if !force && !self.stale && self.last_draw.is_some_and(|t| now - t < REDRAW_EVERY) {
            return;
        }
        self.stale = false;
        let line = match &self.phase {
            Phase::Scan { found } => scan_line(*found),
            Phase::Compress {
                total_files,
                total_bytes,
                done_files,
                done_bytes,
                started,
            } => progress_line(
                (*done_files, *total_files),
                (*done_bytes, *total_bytes),
                now - *started,
                self.width,
            ),
        };
        let line = truncate_right(&line, self.width - 1);
        let _ = write!(w, "{CLEAR}{line}");
        let _ = w.flush();
        self.last_draw = Some(now);
        self.shown = true;
    }

    pub fn scan_found(&mut self, n: u64) {
        if let Phase::Scan { found } = &mut self.phase {
            *found += n;
        }
    }

    pub fn start_compress(&mut self, total_files: u64, total_bytes: u64) {
        self.phase = Phase::Compress {
            total_files,
            total_bytes,
            done_files: 0,
            done_bytes: 0,
            started: Instant::now(),
        };
    }

    /// One file of the compress phase is finished.
    pub fn advance(&mut self, bytes: u64) {
        if let Phase::Compress {
            done_files,
            done_bytes,
            ..
        } = &mut self.phase
        {
            *done_files += 1;
            *done_bytes += bytes;
        }
    }

    /// Writes one event line above the progress line.
    pub fn event(&mut self, w: &mut impl Write, path: &Path, ev: FileEvent<'_>, msg: &str) {
        self.clear(w);
        let shown = abbreviate_home(path, self.home.as_deref());
        let line = event_line(&shown, ev, msg, self.width, self.color);
        let _ = writeln!(w, "{line}");
        self.draw(w, true);
        self.stale = matches!(self.phase, Phase::Compress { .. });
    }

    /// Writes a free-form line (a WARN/ERROR log message) above the progress line.
    pub fn message(&mut self, w: &mut impl Write, text: &str) {
        self.clear(w);
        let _ = writeln!(w, "{text}");
        self.draw(w, true);
    }
}

/// `1023 B`, `1.5 KiB`, `123.3 MiB`, `12.0 GiB` (binary units, one decimal).
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KiB", "MiB", "GiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// Keeps the last `max` chars of `s`, replacing the cut head with `...`.
pub fn truncate_left(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_owned();
    }
    if max <= 3 {
        return ".".repeat(max);
    }
    let tail: String = s.chars().skip(n - (max - 3)).collect();
    format!("...{tail}")
}

/// Keeps the first `max` chars of `s`.
fn truncate_right(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// `path` with a leading `home` shown as `~`.
pub fn abbreviate_home(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// `1h 4m 20s`, `4m 0s`, `20s`.
pub fn duration_text(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

pub fn scan_line(found: u64) -> String {
    format!("scanning... {found} files found")
}

/// Partial cells of the filled bar part, by eighths: index `n` is `n/8` of a cell.
const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
/// Widest `human` text below 1 GiB (`1023.9 MiB`); reserved for the done bytes.
const BYTES_COLUMN: usize = 10;
/// Reserved for the estimate (`99h 59m 59s`).
const ESTIMATE_COLUMN: usize = 11;

/// ` 10% [███▏____] ( 33/111)    1.0 GiB/10.0 GiB  estimate: 1h 4m 20s`; percent and estimate by
/// bytes, counts by files; the bar takes the width the rest leaves. Every number has its column
/// reserved up front, so the bar keeps its width for the whole run.
pub fn progress_line(
    files: (u64, u64),
    bytes: (u64, u64),
    elapsed: Duration,
    width: usize,
) -> String {
    let (done_bytes, total_bytes) = bytes;
    let estimate = if elapsed >= ETA_AFTER && done_bytes > 0 && total_bytes >= done_bytes {
        let rest = elapsed.as_secs_f64() * (total_bytes - done_bytes) as f64 / done_bytes as f64;
        duration_text(rest.round() as u64)
    } else {
        "--".to_owned()
    };
    // Done share as an exact fraction, by bytes, else by files; nothing to do counts as done.
    let (done, total) = match (bytes, files) {
        ((d, t), _) if t > 0 => (u128::from(d.min(t)), u128::from(t)),
        (_, (d, t)) if t > 0 => (u128::from(d.min(t)), u128::from(t)),
        _ => (1, 1),
    };
    let pct = done * 100 / total;
    let pct = format!("{pct:>3}%");
    let total_text = human(total_bytes);
    // Below 1 KiB every done value is no wider than the total; above it, up to `1023.9 MiB`.
    let bytes_col = if total_bytes < 1024 {
        total_text.len()
    } else {
        BYTES_COLUMN.max(total_text.len())
    };
    let files_col = files.1.to_string().len();
    let tail = format!(
        "({:>files_col$}/{}) {:>bytes_col$}/{total_text}  estimate: {estimate:<ESTIMATE_COLUMN$}",
        files.0,
        files.1,
        human(done_bytes),
    );
    // "<pct> [" + bar + "] " + tail, one char spare so the line never wraps; all ASCII but the bar.
    let fixed = pct.len() + 2 + 2 + tail.len() + 1;
    let bar_len = width.saturating_sub(fixed).max(10);
    let eighths = (done * bar_len as u128 * 8 / total) as usize;
    let (full, part) = (eighths / 8, eighths % 8);
    let mut bar = "█".repeat(full);
    if part > 0 {
        bar.push(EIGHTHS[part]);
    }
    let empty = bar_len - full - usize::from(part > 0);
    format!("{pct} [{bar}{}] {tail}", "_".repeat(empty))
}

/// One event line: the path column (left-truncated, padded) and the outcome.
pub fn event_line(path: &str, ev: FileEvent<'_>, msg: &str, width: usize, color: bool) -> String {
    let col = width.saturating_sub(RIGHT_COLUMN + 3).max(MIN_PATH_COLUMN);
    let path = truncate_left(path, col);
    let pad = col - path.chars().count();
    let paint = |c: &str, text: &str| {
        if color {
            format!("{c}{text}{RESET}")
        } else {
            text.to_owned()
        }
    };
    let right = match ev {
        FileEvent::Compressed { size, after } => {
            let saved = if size == 0 {
                0.0
            } else {
                (1.0 - after as f64 / size as f64) * 100.0
            };
            format!(
                "{:>10} -> {:>10} = {}",
                human(size),
                human(after),
                paint(GREEN, &format!("{saved:.1}%"))
            )
        }
        FileEvent::Error(kind) => paint(RED, &format!("ERROR({kind}): {msg}")),
        FileEvent::Skip(kind) => paint(YELLOW, &format!("SKIP({kind}): {msg}")),
    };
    format!("{path}{}   {right}", " ".repeat(pad))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_sizes() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(129_289_011), "123.3 MiB");
        assert_eq!(human(12 << 30), "12.0 GiB");
        assert_eq!(human(3 << 40), "3072.0 GiB");
    }

    #[test]
    fn truncation_and_home() {
        assert_eq!(truncate_left("abcdef", 6), "abcdef");
        assert_eq!(truncate_left("abcdefgh", 6), "...fgh");
        assert_eq!(truncate_left("žžžžžž", 5), "...žž");
        assert_eq!(truncate_left("abcdef", 2), "..");
        let home = Path::new("/Users/u");
        assert_eq!(
            abbreviate_home(Path::new("/Users/u/a/b"), Some(home)),
            "~/a/b"
        );
        assert_eq!(abbreviate_home(Path::new("/Users/u"), Some(home)), "~");
        assert_eq!(
            abbreviate_home(Path::new("/Users/uu/a"), Some(home)),
            "/Users/uu/a"
        );
        assert_eq!(abbreviate_home(Path::new("/x"), None), "/x");
    }

    #[test]
    fn durations() {
        assert_eq!(duration_text(0), "0s");
        assert_eq!(duration_text(59), "59s");
        assert_eq!(duration_text(240), "4m 0s");
        assert_eq!(duration_text(3860), "1h 4m 20s");
    }

    #[test]
    fn progress_lines() {
        let gib = 1u64 << 30;
        let chars = |s: &str| s.chars().count();
        let zero = progress_line((0, 111), (0, 12 * gib), Duration::from_secs(5), 80);
        assert!(zero.starts_with("  0% [____"), "{zero}");
        assert!(
            zero.ends_with("(  0/111)        0 B/12.0 GiB  estimate: --         "),
            "{zero}"
        );
        assert_eq!(chars(&zero), 79, "{zero}");

        let ten = progress_line((33, 111), (gib, 10 * gib), Duration::from_secs(429), 100);
        // 10% of a 39-cell bar is 3.9 cells.
        assert!(ten.starts_with(" 10% [███▉___"), "{ten}");
        // 429 s for 1 of 10 GiB: 9 times as long to go.
        assert!(
            ten.ends_with("( 33/111)    1.0 GiB/10.0 GiB  estimate: 1h 4m 21s  "),
            "{ten}"
        );
        assert_eq!(chars(&ten), 99);

        // The bar keeps its width whatever the numbers are.
        let bar = |s: &str| {
            s.chars()
                .skip_while(|&c| c != '[')
                .take_while(|&c| c != ']')
                .count()
        };
        let mid = progress_line(
            (100, 111),
            (1023 << 20, 12 * gib),
            Duration::from_secs(7),
            80,
        );
        assert_eq!(bar(&mid), bar(&zero), "{mid}");
        assert_eq!(chars(&mid), 79, "{mid}");

        // Eighths: a 10-cell bar at 15% is one full cell and a 4/8 cell.
        let part = progress_line((0, 1), (15, 100), Duration::ZERO, 10);
        assert!(part.starts_with(" 15% [█▌________]"), "{part}");

        // Under two seconds: no estimate yet.
        let early = progress_line((1, 2), (1, 2), Duration::from_secs(1), 100);
        assert!(early.trim_end().ends_with("estimate: --"), "{early}");

        let full = progress_line((5, 5), (100, 100), Duration::from_secs(3), 60);
        assert!(full.starts_with("100% [██████████"), "{full}");
        assert!(!full.contains('_'), "{full}");
        assert!(full.trim_end().ends_with("estimate: 0s"), "{full}");
        // Nothing to do at all counts as done.
        assert!(progress_line((0, 0), (0, 0), Duration::ZERO, 60).starts_with("100%"));
    }

    #[test]
    fn event_lines() {
        let ev = FileEvent::Compressed {
            size: 129_289_011,
            after: 11_848_909,
        };
        let line = event_line("~/a/file.py", ev, "", 80, false);
        assert!(line.starts_with("~/a/file.py "), "{line}");
        assert!(line.ends_with("123.3 MiB ->   11.3 MiB = 90.8%"), "{line}");
        assert_eq!(line.find("123.3"), Some(80 - 34 - 3 + 3 + 1), "{line}");

        let long = format!("~/{}", "d/".repeat(60));
        let line = event_line(&long, FileEvent::Skip("hardlink"), "hardlinked", 80, false);
        assert!(line.starts_with("...d/"), "{line}");
        assert!(line.ends_with("   SKIP(hardlink): hardlinked"), "{line}");

        let line = event_line("~/x", FileEvent::Error("verify"), "differs", 80, true);
        assert!(
            line.contains("\x1b[31mERROR(verify): differs\x1b[0m"),
            "{line}"
        );
        assert!(line.is_ascii());
    }

    #[test]
    fn redraw_is_throttled_and_cleared_around_events() {
        let mut p = Progress::new(80, false, None);
        let mut out = Vec::new();
        p.scan_found(7);
        p.draw(&mut out, false);
        p.draw(&mut out, false);
        let text = String::from_utf8(out.clone()).unwrap();
        assert_eq!(
            text.matches("scanning... 7 files found").count(),
            1,
            "{text:?}"
        );
        out.clear();
        p.event(&mut out, Path::new("/f"), FileEvent::Skip("owner"), "owned");
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with(CLEAR), "{text:?}");
        assert!(text.contains("SKIP(owner): owned\n"), "{text:?}");
        assert!(text.ends_with("scanning... 7 files found"), "{text:?}");

        // Compress phase: the count advanced right after an event is drawn at once.
        let mut out = Vec::new();
        p.start_compress(2, 200);
        p.draw(&mut out, true);
        p.event(
            &mut out,
            Path::new("/f"),
            FileEvent::Compressed {
                size: 100,
                after: 10,
            },
            "",
        );
        p.advance(100);
        p.draw(&mut out, false);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.trim_end().ends_with("(1/2) 100 B/200 B  estimate: --"),
            "{text:?}"
        );
    }
}
