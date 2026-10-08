//! Thread-safe, size-rotated file logger with local-time timestamps. When interactive progress
//! is enabled it also owns the terminal [`Progress`], so log mirroring, event lines and the
//! progress line are written under one mutex.

use crate::progress::{FileEvent, Progress};
use crate::sys;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const LOG_NAME: &str = "auto-compress-apfs.log";
/// File a service manager may redirect stdout/stderr into.
pub const LAUNCHD_LOG_NAME: &str = "launchd.log";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
        })
    }
}

struct Inner {
    out: BufWriter<File>,
    /// Bytes in the current log file, including buffered ones.
    bytes: u64,
    /// Terminal progress on stderr, when enabled.
    progress: Option<Progress>,
}

pub struct Logger {
    inner: Mutex<Inner>,
    path: PathBuf,
    max_bytes: u64,
    keep: u32,
    verbose: bool,
    /// Set after the first write/rotation failure has been reported on stderr.
    io_error_reported: AtomicBool,
    /// Mirrors `inner.progress.is_some()` for a lock-free fast path.
    progress_on: AtomicBool,
}

impl Logger {
    /// Opens (appends to) `<dir>/auto-compress-apfs.log`, creating `dir` if needed.
    pub fn open(dir: &Path, max_bytes: u64, keep: u32, verbose: bool) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join(LOG_NAME);
        let file = open_append(&path)?;
        let bytes = file.metadata()?.len();
        Ok(Self {
            inner: Mutex::new(Inner {
                out: BufWriter::new(file),
                bytes,
                progress: None,
            }),
            path,
            max_bytes,
            keep,
            verbose,
            io_error_reported: AtomicBool::new(false),
            progress_on: AtomicBool::new(false),
        })
    }

    /// Starts interactive progress on stderr (scan phase).
    pub fn enable_progress(&self, progress: Progress) {
        let mut inner = self.lock();
        inner.progress = Some(progress);
        self.progress_on.store(true, Ordering::Relaxed);
    }

    /// Clears the progress line, prints `final_line` in its place (if any) and stops progress.
    pub fn finish_progress(&self, final_line: Option<&str>) {
        if !self.progress_on.swap(false, Ordering::Relaxed) {
            return;
        }
        let mut inner = self.lock();
        if let Some(mut p) = inner.progress.take() {
            let mut err = io::stderr().lock();
            p.clear(&mut err);
            if let Some(line) = final_line {
                let _ = writeln!(err, "{line}");
            }
        }
    }

    /// `n` more files found by the walk.
    pub fn scan_found(&self, n: u64) {
        self.with_progress(|p, err| {
            p.scan_found(n);
            p.draw(err, false);
        });
    }

    /// Switches the progress line to the compress phase.
    pub fn start_compress(&self, files: u64, bytes: u64) {
        self.with_progress(|p, err| {
            p.start_compress(files, bytes);
            p.draw(err, true);
        });
    }

    /// One file of the compress phase is finished (any outcome).
    pub fn advance(&self, bytes: u64) {
        self.with_progress(|p, err| {
            p.advance(bytes);
            p.draw(err, false);
        });
    }

    fn with_progress(&self, f: impl FnOnce(&mut Progress, &mut io::StderrLock<'static>)) {
        if !self.progress_on.load(Ordering::Relaxed) {
            return;
        }
        let mut inner = self.lock();
        if let Some(p) = inner.progress.as_mut() {
            f(p, &mut io::stderr().lock());
        }
    }

    /// Logs one per-file outcome at `level` and, with progress enabled, prints its terminal line.
    /// The log line is `compressed <path> <size> -> <after>` or `<path>: <msg>`.
    pub fn file_event(&self, level: Level, path: &Path, ev: FileEvent<'_>, msg: impl fmt::Display) {
        if !self.wanted(level) {
            return;
        }
        let msg = msg.to_string();
        let text = match ev {
            FileEvent::Compressed { size, after } => {
                format!("compressed {} {size} -> {after}", path.display())
            }
            FileEvent::Error(_) | FileEvent::Skip(_) => format!("{}: {msg}", path.display()),
        };
        self.write(level, &text, |p, err| p.event(err, path, ev, &msg));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn error(&self, msg: impl fmt::Display) {
        self.log(Level::Error, msg);
    }

    pub fn warn(&self, msg: impl fmt::Display) {
        self.log(Level::Warn, msg);
    }

    pub fn info(&self, msg: impl fmt::Display) {
        self.log(Level::Info, msg);
    }

    pub fn debug(&self, msg: impl fmt::Display) {
        self.log(Level::Debug, msg);
    }

    pub fn enabled(&self, level: Level) -> bool {
        level != Level::Debug || self.verbose
    }

    /// The line goes to the log file or to the terminal progress.
    fn wanted(&self, level: Level) -> bool {
        self.enabled(level) || self.progress_on.load(Ordering::Relaxed)
    }

    /// Writes one line. The first I/O error of the run is reported on stderr, the rest are
    /// ignored: logging must never abort the run.
    pub fn log(&self, level: Level, msg: impl fmt::Display) {
        if !self.wanted(level) {
            return;
        }
        let text = msg.to_string();
        self.write(level, &text, |p, err| {
            if level <= Level::Warn {
                p.message(err, &format!("{level}: {text}"));
            }
        });
    }

    /// Writes `text` as a log line (when `level` is enabled) and lets `on_tty` draw on the
    /// terminal when progress is on; `-v` mirrors the log line to stderr instead.
    fn write(
        &self,
        level: Level,
        text: &str,
        on_tty: impl FnOnce(&mut Progress, &mut io::StderrLock<'static>),
    ) {
        let enabled = self.enabled(level);
        let line = format!("{} {level} {text}\n", timestamp(SystemTime::now()));
        let mut inner = self.lock();
        if let Some(p) = inner.progress.as_mut() {
            let mut err = io::stderr().lock();
            if self.verbose {
                if enabled {
                    p.message(&mut err, line.trim_end());
                }
            } else {
                on_tty(p, &mut err);
            }
        } else if self.verbose && enabled {
            eprint!("{line}");
        }
        if enabled && let Err(e) = self.write_line(&mut inner, &line, level <= Level::Warn) {
            self.report_io_error(&e);
        }
    }

    fn write_line(&self, inner: &mut Inner, line: &str, flush: bool) -> io::Result<()> {
        let len = line.len() as u64;
        if inner.bytes > 0 && inner.bytes + len > self.max_bytes {
            self.rotate(inner)?;
        }
        inner.out.write_all(line.as_bytes())?;
        inner.bytes += len;
        if flush {
            inner.out.flush()?;
        }
        Ok(())
    }

    /// `.log.(k)` -> `.log.(k+1)` for k = keep-1..1, `.log` -> `.log.1`, fresh `.log`.
    fn rotate(&self, inner: &mut Inner) -> io::Result<()> {
        inner.out.flush()?;
        for k in (1..self.keep).rev() {
            match fs::rename(self.rotated(k), self.rotated(k + 1)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        let file = if self.keep == 0 {
            OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&self.path)?
        } else {
            fs::rename(&self.path, self.rotated(1))?;
            open_append(&self.path)?
        };
        inner.out = BufWriter::new(file);
        inner.bytes = 0;
        Ok(())
    }

    fn report_io_error(&self, e: &io::Error) {
        if !self.io_error_reported.swap(true, Ordering::Relaxed) {
            eprintln!(
                "auto-compress-apfs: cannot write log {}: {e} (further log errors not reported)",
                self.path.display()
            );
        }
    }

    fn rotated(&self, k: u32) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push(format!(".{k}"));
        PathBuf::from(s)
    }

    pub fn flush(&self) {
        let mut inner = self.lock();
        if let Err(e) = inner.out.flush() {
            self.report_io_error(&e);
        }
    }
}

impl Drop for Logger {
    fn drop(&mut self) {
        self.flush();
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// Truncates `<dir>/launchd.log` to zero when it is larger than `max`. Returns whether it did.
pub fn truncate_launchd_log(dir: &Path, max: u64) -> io::Result<bool> {
    let path = dir.join(LAUNCHD_LOG_NAME);
    match fs::metadata(&path) {
        Ok(m) if m.len() > max => {
            OpenOptions::new().write(true).truncate(true).open(&path)?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmm+HH:MM` in local time.
pub fn timestamp(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let millis = d.subsec_millis();
    let Some(lt) = sys::local_time(secs) else {
        return format!("@{secs}.{millis:03}");
    };
    let sign = if lt.gmtoff < 0 { '-' } else { '+' };
    let off = lt.gmtoff.unsigned_abs();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{millis:03}{sign}{:02}:{:02}",
        lt.year,
        lt.month,
        lt.day,
        lt.hour,
        lt.minute,
        lt.second,
        off / 3600,
        off % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acz-log-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn log_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn fill(logger: &Logger, lines: usize) {
        let msg = "x".repeat(1000);
        for i in 0..lines {
            logger.info(format_args!("{i} {msg}"));
        }
        logger.flush();
    }

    #[test]
    fn rotation_keep_1() {
        let dir = temp_dir("k1");
        let logger = Logger::open(&dir, 64 * 1024, 1, false).unwrap();
        fill(&logger, 300);
        assert_eq!(log_files(&dir), [LOG_NAME, &format!("{LOG_NAME}.1")]);
        for name in log_files(&dir) {
            assert!(fs::metadata(dir.join(name)).unwrap().len() <= 64 * 1024);
        }
        let current = fs::read_to_string(dir.join(LOG_NAME)).unwrap();
        assert!(current.lines().last().unwrap().contains(" INFO 299 "));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotation_keep_2() {
        let dir = temp_dir("k2");
        let logger = Logger::open(&dir, 64 * 1024, 2, false).unwrap();
        fill(&logger, 400);
        let n = format!("{LOG_NAME}.");
        assert_eq!(
            log_files(&dir),
            [LOG_NAME.to_string(), format!("{n}1"), format!("{n}2")]
        );
        // .1 is newer than .2
        let first = |name: &str| -> usize {
            let text = fs::read_to_string(dir.join(name)).unwrap();
            let line = text.lines().next().unwrap().to_string();
            line.split(' ').nth(2).unwrap().parse().unwrap()
        };
        assert!(first(&format!("{n}1")) > first(&format!("{n}2")));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn debug_only_when_verbose_and_line_format() {
        let dir = temp_dir("fmt");
        let logger = Logger::open(&dir, 64 * 1024, 1, false).unwrap();
        logger.debug("hidden");
        logger.warn("shown");
        let text = fs::read_to_string(dir.join(LOG_NAME)).unwrap();
        assert!(!text.contains("hidden"));
        let line = text.lines().next().unwrap();
        let (ts, rest) = line.split_once(' ').unwrap();
        assert_eq!(rest, "WARN shown");
        // 2026-10-08T21:22:13.123+03:00
        assert_eq!(ts.len(), 29, "{ts}");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[19..20], ".");
        assert!(matches!(&ts[23..24], "+" | "-"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn launchd_log_truncated_only_when_large() {
        let dir = temp_dir("ld");
        fs::create_dir_all(&dir).unwrap();
        assert!(!truncate_launchd_log(&dir, 10).unwrap());
        fs::write(dir.join(LAUNCHD_LOG_NAME), "0123456789").unwrap();
        assert!(!truncate_launchd_log(&dir, 10).unwrap());
        fs::write(dir.join(LAUNCHD_LOG_NAME), "0123456789A").unwrap();
        assert!(truncate_launchd_log(&dir, 10).unwrap());
        assert_eq!(fs::metadata(dir.join(LAUNCHD_LOG_NAME)).unwrap().len(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }
}
