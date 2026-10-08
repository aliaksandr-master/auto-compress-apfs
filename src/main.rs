//! auto-compress-apfs: turns on APFS transparent compression for files matched by glob
//! patterns, using Apple's `/usr/bin/ditto --hfsCompression`.

mod cache;
mod classify;
mod compress;
mod config;
mod logger;
mod progress;
mod quarantine;
mod stat;
mod state;
mod sys;
mod walk;

use classify::{Class, Rules};
use compress::{Compressor, Outcome};
use config::Config;
use logger::{Level, Logger};
use progress::{FileEvent, Progress, abbreviate_home, human};
use quarantine::{Entry, Release};
use state::{Day, Decision, Row, State};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, TryLockError};
use std::io::IsTerminal;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const NAME: &str = "auto-compress-apfs";
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Terminal width when `COLUMNS` is not set.
const PROGRESS_WIDTH: usize = 100;
/// Roots listed in the preview totals.
const PREVIEW_TOP_ROOTS: usize = 20;

const USAGE: &str = "\
Usage:
  auto-compress-apfs [compress] [OPTIONS]    preview (default); with --apply: compress
  auto-compress-apfs stat [--top N] [--all] [--config PATH]
  auto-compress-apfs quarantine list [--config PATH]
  auto-compress-apfs quarantine clear <PATH>|--all [--config PATH]
  auto-compress-apfs -h|--help | -V|--version

Turns on APFS transparent compression (ditto --hfsCompression) for files matched by the
glob patterns in the config. Without --apply nothing is changed: the files that would be
compressed are listed.

compress options:
  --apply                    compress (default: preview only, nothing is changed)
  --config PATH              config file (default: ~/.config/auto-compress-apfs/default.toml,
                             created from the built-in default when missing)
  --dir DIR                  only DIR's subtree, intersected with the patterns
  --search GLOB...           patterns added for this run; a glob not starting with / or ~ is
                             relative to --dir (or the current directory)
  --exclude GLOB...          excludes added for this run (without /: a name at any depth)
  --ignore-global-search     ignore the config's patterns and exclude; only --search and
                             --exclude apply (hard protections always apply)
  --progress, --no-progress  force or suppress terminal progress (default: stderr is a terminal)
  -v, --verbose              DEBUG level, log lines mirrored to stderr

stat options:
  --top N                    rows to list (default 30)
  --all                      list every row";

/// Options of `compress` (also the default command).
#[derive(Debug, Default)]
struct CompressArgs {
    config: Option<PathBuf>,
    apply: bool,
    verbose: bool,
    dir: Option<String>,
    search: Vec<String>,
    exclude: Vec<String>,
    ignore_global: bool,
    /// `None`: on when stderr is a terminal.
    progress: Option<bool>,
    /// The options as given (without `--apply`/`--dry-run`), for the preview hint.
    raw: Vec<String>,
}

#[derive(Debug)]
struct StatArgs {
    config: Option<PathBuf>,
    /// `None` lists every row.
    top: Option<usize>,
}

#[derive(Debug)]
enum QuarantineCmd {
    List,
    /// `None` releases every entry.
    Clear(Option<String>),
}

#[derive(Debug)]
enum Cli {
    Compress(CompressArgs),
    Stat(StatArgs),
    Quarantine {
        config: Option<PathBuf>,
        cmd: QuarantineCmd,
    },
    Exit(ExitCode),
}

fn help_or_version(a: &str) -> Option<Cli> {
    match a {
        "-h" | "--help" => {
            println!("{USAGE}");
            Some(Cli::Exit(ExitCode::SUCCESS))
        }
        "-V" | "--version" => {
            println!("{NAME} {VERSION}");
            Some(Cli::Exit(ExitCode::SUCCESS))
        }
        _ => None,
    }
}

/// `--config PATH` / `--config=PATH`; `Ok(false)` when `a` is something else.
fn config_opt(
    a: &str,
    it: &mut impl Iterator<Item = String>,
    config: &mut Option<PathBuf>,
) -> Result<bool, String> {
    if a == "--config" {
        *config = Some(PathBuf::from(it.next().ok_or("--config needs a PATH")?));
        Ok(true)
    } else if let Some(p) = a.strip_prefix("--config=") {
        *config = Some(PathBuf::from(p));
        Ok(true)
    } else {
        Ok(false)
    }
}

fn is_option(a: &str) -> bool {
    a.len() > 1 && a.starts_with('-')
}

/// Values of a multi-value option: every following argument up to the next option.
fn values(
    opt: &str,
    it: &mut std::iter::Peekable<impl Iterator<Item = String>>,
) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    while let Some(v) = it.next_if(|v| !is_option(v)) {
        out.push(v);
    }
    if out.is_empty() {
        return Err(format!("{opt} needs at least one GLOB"));
    }
    Ok(out)
}

fn parse_compress(it: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut it = it.peekable();
    let mut a = CompressArgs::default();
    let mut dry_run = false;
    while let Some(arg) = it.next() {
        if let Some(exit) = help_or_version(&arg) {
            return Ok(exit);
        }
        let start = a.raw.len();
        a.raw.push(arg.clone());
        if config_opt(&arg, &mut it, &mut a.config)? {
            if let Some(p) = a.config.as_ref().filter(|_| arg == "--config") {
                a.raw.push(p.display().to_string());
            }
            continue;
        }
        match arg.as_str() {
            "--apply" => {
                a.apply = true;
                a.raw.truncate(start);
            }
            "--dry-run" => {
                dry_run = true;
                a.raw.truncate(start);
            }
            "-v" | "--verbose" => a.verbose = true,
            "--progress" => a.progress = Some(true),
            "--no-progress" => a.progress = Some(false),
            "--ignore-global-search" => a.ignore_global = true,
            "--dir" => {
                let d = it.next().ok_or("--dir needs a DIR")?;
                a.raw.push(d.clone());
                a.dir = Some(d);
            }
            "--search" => {
                let v = values("--search", &mut it)?;
                a.raw.extend(v.iter().cloned());
                a.search.extend(v);
            }
            "--exclude" => {
                let v = values("--exclude", &mut it)?;
                a.raw.extend(v.iter().cloned());
                a.exclude.extend(v);
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    if dry_run && a.apply {
        return Err("--dry-run and --apply exclude each other".into());
    }
    Ok(Cli::Compress(a))
}

fn parse_stat(mut it: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut a = StatArgs {
        config: None,
        top: Some(stat::DEFAULT_TOP),
    };
    while let Some(arg) = it.next() {
        if let Some(exit) = help_or_version(&arg) {
            return Ok(exit);
        }
        if config_opt(&arg, &mut it, &mut a.config)? {
            continue;
        }
        match arg.as_str() {
            "--all" => a.top = None,
            "--top" => {
                let n = it.next().ok_or("--top needs a number")?;
                let n = n
                    .parse()
                    .map_err(|_| format!("--top: `{n}` is not a number"))?;
                a.top = Some(n);
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(Cli::Stat(a))
}

fn parse_quarantine(mut it: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut config = None;
    let mut words = Vec::new();
    let mut all = false;
    while let Some(arg) = it.next() {
        if let Some(exit) = help_or_version(&arg) {
            return Ok(exit);
        }
        if config_opt(&arg, &mut it, &mut config)? {
            continue;
        }
        match arg.as_str() {
            "--all" => all = true,
            a if is_option(a) => return Err(format!("unknown argument `{a}`")),
            _ => words.push(arg),
        }
    }
    let cmd = match (
        words.first().map(String::as_str),
        words.get(1..).unwrap_or(&[]),
        all,
    ) {
        (Some("list"), [], false) => QuarantineCmd::List,
        (Some("clear"), [], true) => QuarantineCmd::Clear(None),
        (Some("clear"), [p], false) => QuarantineCmd::Clear(Some(p.clone())),
        _ => return Err("usage: quarantine list | quarantine clear <PATH>|--all".into()),
    };
    Ok(Cli::Quarantine { config, cmd })
}

fn parse_args(it: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut it = it.peekable();
    match it.peek().map(String::as_str) {
        Some("compress") => parse_compress(it.skip(1)),
        Some("stat") => parse_stat(it.skip(1)),
        Some("quarantine") => parse_quarantine(it.skip(1)),
        _ => parse_compress(it),
    }
}

fn main() -> ExitCode {
    let cli = match parse_args(std::env::args().skip(1)) {
        Ok(Cli::Exit(code)) => return code,
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("{NAME}: {e}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
        eprintln!("{NAME}: HOME is not set");
        return ExitCode::FAILURE;
    };
    let home = PathBuf::from(home);
    let result = match cli {
        Cli::Compress(args) => return cmd_compress(&args, &home),
        Cli::Stat(args) => cmd_stat(&args, &home),
        Cli::Quarantine { config, cmd } => cmd_quarantine(config, &cmd, &home),
        Cli::Exit(code) => return code,
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn config_path(arg: Option<&PathBuf>, home: &Path) -> PathBuf {
    arg.cloned()
        .unwrap_or_else(|| home.join(".config").join(NAME).join("default.toml"))
}

/// Lock and `state.tsv` live here.
fn state_dir(home: &Path) -> PathBuf {
    home.join("Library/Application Support").join(NAME)
}

/// `quarantine.txt` sits next to the effective config.
fn quarantine_file(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(quarantine::QUARANTINE_NAME)
}

/// Removes `.` and resolves `..` lexically (no symlink resolution).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// `~` expanded, relative paths joined to `base`, then normalized lexically.
fn absolute(p: &str, base: &Path, home: &Path) -> PathBuf {
    let p = config::expand_tilde(p, home);
    normalize(&base.join(p))
}

/// The current directory as the shell shows it: `$PWD` when it names the same directory
/// (keeps symlinked spellings like `/var/...` that patterns use), else `getcwd`.
fn current_dir() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir()
        .map_err(|e| format!("cannot determine the current directory: {e}"))?;
    let id = |p: &Path| fs::metadata(p).ok().map(|m| (m.dev(), m.ino()));
    Ok(std::env::var_os("PWD")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && id(p).is_some() && id(p) == id(&cwd))
        .unwrap_or(cwd))
}

/// `stat`: read-only view of `state.tsv` and `quarantine.txt` (no lock, no log, no config parse).
fn cmd_stat(a: &StatArgs, home: &Path) -> Result<ExitCode, String> {
    let state_path = state_dir(home).join(state::STATE_NAME);
    let st = State::load(&state_path)
        .map_err(|e| format!("cannot read {}: {e}", state_path.display()))?;
    let qfile = quarantine_file(&config_path(a.config.as_ref(), home));
    let q =
        quarantine::load(&qfile).map_err(|e| format!("cannot read {}: {e}", qfile.display()))?;
    let width = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(stat::DEFAULT_WIDTH);
    print!("{}", stat::render(&st, &q, a.top, width));
    Ok(ExitCode::SUCCESS)
}

/// `quarantine list` (read-only) and `quarantine clear` (under the run lock; also resets the
/// streak in `state.tsv`).
fn cmd_quarantine(
    config: Option<PathBuf>,
    cmd: &QuarantineCmd,
    home: &Path,
) -> Result<ExitCode, String> {
    let qfile = quarantine_file(&config_path(config.as_ref(), home));
    let err = |e: std::io::Error| format!("{}: {e}", qfile.display());
    let target = match cmd {
        QuarantineCmd::List => {
            let entries = quarantine::load(&qfile).map_err(err)?;
            if entries.is_empty() {
                println!("quarantine is empty ({})", qfile.display());
            }
            for e in entries {
                let stamp = if e.stamp.is_empty() { "-" } else { &e.stamp };
                println!("{stamp}\t{}", e.path.display());
            }
            return Ok(ExitCode::SUCCESS);
        }
        QuarantineCmd::Clear(None) => None,
        QuarantineCmd::Clear(Some(p)) => Some(absolute(p, &current_dir()?, home)),
    };
    let dir = state_dir(home);
    let Some(_lock) = acquire_lock(&dir)? else {
        eprintln!("{NAME}: a run is in progress, try again when it has finished");
        return Ok(ExitCode::FAILURE);
    };
    let what = target.as_deref().map_or(Release::All, Release::Path);
    let released = quarantine::clear(&qfile, what).map_err(err)?;
    if released.is_empty() {
        return Ok(match &target {
            Some(p) => {
                eprintln!("{NAME}: not in quarantine: {}", p.display());
                ExitCode::FAILURE
            }
            None => {
                println!("quarantine is empty ({})", qfile.display());
                ExitCode::SUCCESS
            }
        });
    }
    let state_path = dir.join(state::STATE_NAME);
    let mut st = State::load(&state_path)
        .map_err(|e| format!("cannot read {}: {e}", state_path.display()))?;
    for p in &released {
        if let Some(r) = st.rows.get_mut(p) {
            r.streak = 0;
        }
        println!("released {}", p.display());
    }
    st.store(&state_path, None)
        .map_err(|e| format!("cannot write {}: {e}", state_path.display()))?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_compress(args: &CompressArgs, home: &Path) -> ExitCode {
    if args.ignore_global && args.search.is_empty() {
        println!("no search patterns: config patterns ignored and no --search given");
        return ExitCode::SUCCESS;
    }
    let config_path = config_path(args.config.as_ref(), home);
    let (cfg, created) = match Config::load_or_create(&config_path, home) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return ExitCode::FAILURE;
        }
    };
    // No patterns at all: a hint, and nothing else (no log, lock, state, cache or walk).
    if !args.ignore_global && cfg.patterns.is_empty() && args.search.is_empty() {
        if created {
            println!("created {}", config_path.display());
        }
        println!(
            "no search patterns: add `patterns` to {} or pass --search GLOB",
            config_path.display()
        );
        return ExitCode::SUCCESS;
    }
    let log_dir = cfg.log_dir();
    let log = match Logger::open(&log_dir, cfg.log_max_bytes, cfg.log_keep, args.verbose) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{NAME}: cannot open log in {}: {e}", log_dir.display());
            return ExitCode::FAILURE;
        }
    };
    let code = match run(args, &cfg, &config_path, created, home, &log) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log.finish_progress(None);
            log.error(&e);
            if !args.verbose {
                eprintln!("{NAME}: {e}");
            }
            ExitCode::FAILURE
        }
    };
    log.flush();
    code
}

/// The run's patterns and excludes: config + CLI, or only CLI with `--ignore-global-search`.
/// CLI globs not starting with `/` or `~` are relative to `base` (`--dir` or the cwd); an
/// exclude without `/` stays a name glob.
fn active_globs(
    args: &CompressArgs,
    cfg: &Config,
    base: &Path,
    home: &Path,
) -> Result<(Vec<String>, Vec<String>), String> {
    let (mut patterns, mut exclude) = if args.ignore_global {
        (Vec::new(), Vec::new())
    } else {
        (cfg.patterns.clone(), cfg.exclude.clone())
    };
    let resolve = |g: &String| absolute(g, base, home).to_string_lossy().into_owned();
    patterns.extend(args.search.iter().map(resolve));
    exclude.extend(args.exclude.iter().map(|g| {
        if !g.contains('/') || g.starts_with("**/") {
            g.clone()
        } else {
            resolve(g)
        }
    }));
    config::validate_excludes(&exclude)?;
    Ok((patterns, exclude))
}

/// Local day of this run; debug builds honour `ACZ_TODAY=YYYY-MM-DD` (tests simulate days).
fn today(now: i64) -> Result<Day, String> {
    #[cfg(debug_assertions)]
    if let Ok(v) = std::env::var("ACZ_TODAY") {
        return Day::parse(&v).ok_or_else(|| format!("ACZ_TODAY={v} is not YYYY-MM-DD"));
    }
    sys::local_time(now)
        .and_then(|t| Day::new(t.year, t.month, t.day))
        .ok_or_else(|| "cannot determine the local date".to_owned())
}

#[derive(Default)]
struct Summary {
    found: u64,
    already_compressed: u64,
    too_small: u64,
    too_new: u64,
    skipped: u64,
    incompressible: u64,
    to_compress: u64,
    quarantined: u64,
    requarantined_new: u64,
    dataless: u64,
    flagged: u64,
}

/// A skipped file shown in the preview listing.
struct PreviewSkip {
    path: PathBuf,
    kind: &'static str,
    reason: String,
}

fn run(
    args: &CompressArgs,
    cfg: &Config,
    config_path: &Path,
    created: bool,
    home: &Path,
    log: &Logger,
) -> Result<(), String> {
    let started = Instant::now();
    let apply = args.apply;
    let threads = cfg.worker_threads();
    log.info(format_args!(
        "start {NAME} {VERSION} config={} threads={threads} dry_run={}",
        config_path.display(),
        !apply
    ));
    if created {
        log.info(format_args!(
            "created default config {}",
            config_path.display()
        ));
    }
    // Reading a dataless (cloud placeholder) file must fail instead of downloading it.
    if let Err(e) = sys::disable_dataless_materialization() {
        log.warn(format_args!(
            "cannot turn off materialization of dataless files: {e}"
        ));
    }
    match logger::truncate_launchd_log(&cfg.log_dir(), config::LAUNCHD_LOG_MAX) {
        Ok(true) => log.info("truncated launchd.log"),
        Ok(false) => {}
        Err(e) => log.warn(format_args!("cannot truncate launchd.log: {e}")),
    }

    let cwd = current_dir()?;
    let scope = match &args.dir {
        Some(d) => {
            let dir = absolute(d, &cwd, home);
            if !fs::metadata(&dir).is_ok_and(|m| m.is_dir()) {
                return Err(format!(
                    "--dir {}: not an existing directory",
                    dir.display()
                ));
            }
            Some(dir)
        }
        None => None,
    };
    let base = scope.as_deref().unwrap_or(&cwd);
    let (patterns, exclude) = active_globs(args, cfg, base, home)?;

    // Held until the end of `run`; previews neither take nor need it.
    let _lock = if apply {
        match acquire_lock(&state_dir(home))? {
            Some(f) => Some(f),
            None => {
                log.info("another run in progress, exiting");
                return Ok(());
            }
        }
    } else {
        None
    };

    let cache_path = home
        .join("Library/Caches")
        .join(NAME)
        .join(cache::CACHE_NAME);
    let known = cache::load(&cache_path).unwrap_or_else(|e| {
        log.warn(format_args!("cannot read {}: {e}", cache_path.display()));
        HashSet::new()
    });
    let state_path = state_dir(home).join(state::STATE_NAME);
    // Fail closed: an --apply run never overwrites a state it could not read (a missing file
    // is an empty state); a preview goes on without the history.
    let mut st = match State::load(&state_path) {
        Ok(st) => st,
        Err(e) if apply => return Err(format!("cannot read {}: {e}", state_path.display())),
        Err(e) => {
            log.warn(format_args!("cannot read {}: {e}", state_path.display()));
            State::default()
        }
    };
    // Only a run over the whole default-config search may prune state rows, count as a run day
    // and move streaks; a narrowed run (--config, --dir, --search, --exclude,
    // --ignore-global-search) only upserts rows. The state is shared by every config of a HOME.
    let unrestricted = args.config.is_none()
        && args.dir.is_none()
        && args.search.is_empty()
        && args.exclude.is_empty()
        && !args.ignore_global;
    let prune = apply && unrestricted;
    let track_dirs: Option<HashSet<PathBuf>> = prune.then(|| {
        st.rows
            .keys()
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect()
    });
    let qfile = quarantine_file(config_path);
    // Fail closed: without the list, quarantined files would be compressed again.
    let quarantined = quarantine::load(&qfile)
        .map(|q| quarantine::paths(&q))
        .map_err(|e| format!("cannot read {}: {e}", qfile.display()))?;

    let guard = walk::Guard::new(home, &cfg.allow_volumes, log)?;
    let matcher = walk::Matcher::new(&patterns, &exclude, scope.as_deref())?;
    if args
        .progress
        .unwrap_or_else(|| std::io::stderr().is_terminal())
    {
        let width = std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(PROGRESS_WIDTH);
        let color = std::env::var_os("NO_COLOR").is_none();
        log.enable_progress(Progress::new(width, color, Some(home)));
    }
    let walked = walk::walk(
        &matcher,
        &guard,
        cfg.one_file_system,
        threads,
        log,
        track_dirs.as_ref(),
    );
    drop(track_dirs);
    if !apply {
        log.finish_progress(None);
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let today = today(now)?;
    let prev_day = st.previous_run_day(today);
    let rules = Rules {
        euid: sys::effective_uid(),
        min_size: cfg.min_size,
        mtime_cutoff: now.saturating_sub_unsigned(cfg.min_age_secs),
        incompressible: &known,
    };
    let mut sum = Summary {
        found: walked.candidates.len() as u64,
        dataless: walked.dataless,
        flagged: walked.flagged_dirs.len() as u64,
        ..Summary::default()
    };
    let mut preview_skips: Vec<PreviewSkip> = walked
        .flagged_dirs
        .iter()
        .map(|(p, flags)| PreviewSkip {
            path: p.clone(),
            kind: "flag",
            reason: format!(
                "directory skipped with its contents, flag {} set",
                sys::flag_names(*flags)
            ),
        })
        .collect();
    let mut skip = |level: Level, path: &Path, kind: &'static str, reason: String| {
        log.file_event(level, path, FileEvent::Skip(kind), &reason);
        if !apply {
            preview_skips.push(PreviewSkip {
                path: path.to_path_buf(),
                kind,
                reason,
            });
        }
    };
    let mut new_quarantine: Vec<Entry> = Vec::new();
    let mut keep_cached = Vec::new();
    let mut todo = Vec::new();
    for c in walked.candidates {
        let listed = quarantined.contains(&c.path);
        if let Some(r) = st.rows.get_mut(&c.path) {
            r.seen = true;
        }
        match rules.classify(&c) {
            Class::AlreadyCompressed => {
                sum.already_compressed += 1;
                match st.rows.get_mut(&c.path) {
                    Some(r) if !listed && unrestricted => state::on_compressed(r, today),
                    Some(_) => {}
                    // Compressed earlier (another version, another tool): recorded for `stat`.
                    None if state::storable(&c.path) => {
                        let row = Row::new(c.key.dev, c.key.ino, c.size(), c.allocated(), 0);
                        st.rows.insert(c.path.clone(), row);
                    }
                    None => {}
                }
            }
            Class::Flagged(flags) => {
                sum.skipped += 1;
                sum.flagged += 1;
                let reason = format!("skipped, flag {} set", sys::flag_names(flags));
                skip(Level::Info, &c.path, "flag", reason);
            }
            Class::Dataless => {
                sum.dataless += 1;
                skip(
                    Level::Debug,
                    &c.path,
                    "dataless",
                    "dataless (cloud placeholder), skipped".into(),
                );
            }
            Class::TooSmall => sum.too_small += 1,
            Class::TooNew => {
                sum.too_new += 1;
                let what = if c.key.mtime_sec > now {
                    "mtime in the future"
                } else {
                    "modified recently"
                };
                log.debug(format_args!("{}: {what}, skipped", c.path.display()));
            }
            Class::Skip { kind, reason } => {
                sum.skipped += 1;
                skip(Level::Info, &c.path, kind, reason.to_owned());
            }
            Class::Incompressible => {
                sum.incompressible += 1;
                keep_cached.push(c.key);
            }
            Class::Compress if listed => sum.quarantined += 1,
            Class::Compress => {
                // The streak is reset only once the file is listed in quarantine.txt. Narrowed
                // runs leave streaks alone and only honour one already at the threshold (an
                // append that failed earlier).
                let threshold = cfg.quarantine_after;
                let decision = match st.rows.get_mut(&c.path) {
                    Some(r) if unrestricted => {
                        state::on_redecompressed(r, today, prev_day, threshold)
                    }
                    Some(r) if threshold > 0 && r.streak >= threshold => Decision::Quarantine,
                    _ => Decision::Compress,
                };
                if decision == Decision::Quarantine {
                    sum.requarantined_new += 1;
                    let reason = format!(
                        "decompressed again on {} consecutive run days, quarantined (release: \
                         {NAME} quarantine clear <path>)",
                        cfg.quarantine_after
                    );
                    skip(Level::Info, &c.path, "quarantine", reason);
                    new_quarantine.push(Entry {
                        stamp: logger::timestamp(SystemTime::now()),
                        path: c.path.clone(),
                    });
                    continue;
                }
                sum.to_compress += 1;
                if !apply {
                    log.debug(format_args!("would compress {}", c.path.display()));
                }
                todo.push(c);
            }
        }
    }

    let out = if apply {
        compress::remove_stale(&walked.stale_staging, log);
        log.start_compress(todo.len() as u64, todo.iter().map(|c| c.size()).sum());
        let out =
            Compressor::new(log, cfg.verify, cfg.batch_size, threads, cfg.min_free_bytes).run(todo);
        let keys = keep_cached.iter().chain(&out.new_incompressible);
        if let Err(e) = cache::store(&cache_path, keys) {
            log.warn(format_args!("cannot write {}: {e}", cache_path.display()));
        }
        for c in &out.committed {
            let row = Row::new(c.dev, c.ino, c.size, c.after, now);
            if !st.record_compressed(&c.path, row) {
                log.debug(format_args!(
                    "{}: path contains TAB or LF, not recorded in state",
                    c.path.display()
                ));
            }
        }
        // quarantine.txt first: a streak is reset only for files actually listed, so after a
        // failed append the streak stays at the threshold and the next run retries (and still
        // does not compress the file).
        if !new_quarantine.is_empty() {
            match quarantine::append(&qfile, &new_quarantine) {
                Ok(_) => {
                    for e in &new_quarantine {
                        if let Some(r) = st.rows.get_mut(&e.path) {
                            r.streak = 0;
                        }
                    }
                }
                Err(e) => log.error(format_args!(
                    "cannot write {}: {e}; quarantine retried next run",
                    qfile.display()
                )),
            }
        }
        let pruned = if prune {
            st.mark_run(today);
            Some(&walked.read_dirs)
        } else {
            None
        };
        if let Err(e) = st.store(&state_path, pruned) {
            log.warn(format_args!("cannot write {}: {e}", state_path.display()));
        }
        out
    } else {
        print_preview(&todo, &preview_skips, home, args);
        Outcome::default()
    };

    let elapsed = started.elapsed().as_secs_f64();
    log.info(format_args!(
        "summary found={} already_compressed={} to_compress={} compressed={} incompressible={} \
         too_small={} too_new={} skipped={} errors={} bytes_before={} bytes_after={} \
         ditto_calls={} stale_staging={} quarantined={} requarantined_new={} refused_roots={} \
         dataless={} excluded={} flagged={} skipped_low_space={} dry_run={} elapsed={elapsed:.3}s",
        sum.found,
        sum.already_compressed,
        sum.to_compress,
        out.compressed,
        sum.incompressible + out.incompressible,
        sum.too_small,
        sum.too_new,
        sum.skipped + out.skipped,
        out.errors,
        out.bytes_before,
        out.bytes_after,
        out.ditto_calls,
        walked.stale_staging.len(),
        sum.quarantined,
        sum.requarantined_new,
        walked.refused_roots,
        sum.dataless,
        walked.excluded,
        sum.flagged,
        out.skipped_low_space,
        !apply,
    ));
    log.finish_progress(Some(&format!(
        "done: {} compressed ({} -> {}), {} errors, {} skipped, {} low space, {} quarantined, \
         {} newly quarantined in {elapsed:.1}s",
        out.compressed,
        human(out.bytes_before),
        human(out.bytes_after),
        out.errors,
        sum.skipped + out.skipped,
        out.skipped_low_space,
        sum.quarantined,
        sum.requarantined_new,
    )));
    Ok(())
}

/// Quotes an argument for a POSIX shell when it needs it.
fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&b));
    if plain {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Preview on stdout: every file that would be compressed with its size, the skipped files,
/// totals with the biggest roots, and the command that would do it.
fn print_preview(
    todo: &[walk::Candidate],
    skips: &[PreviewSkip],
    home: &Path,
    args: &CompressArgs,
) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let show = |p: &Path| abbreviate_home(p, Some(home));
    let col = todo
        .iter()
        .map(|c| show(&c.path).chars().count())
        .max()
        .unwrap_or(0)
        .min(100);
    let total: u64 = todo.iter().map(|c| c.size()).sum();
    for c in todo {
        let _ = writeln!(out, "{:<col$}   {:>10}", show(&c.path), human(c.size()));
    }
    for s in skips {
        let _ = writeln!(out, "{}   SKIP({}): {}", show(&s.path), s.kind, s.reason);
    }
    let mut roots: HashMap<&Arc<Path>, u64> = HashMap::new();
    for c in todo {
        *roots.entry(&c.match_root).or_default() += c.size();
    }
    let mut roots: Vec<_> = roots.into_iter().collect();
    roots.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let _ = writeln!(
        out,
        "\nwould compress: {} files, {} ({} skipped)",
        todo.len(),
        human(total),
        skips.len()
    );
    if !roots.is_empty() {
        let _ = writeln!(out, "top roots by size:");
        for (root, bytes) in roots.iter().take(PREVIEW_TOP_ROOTS) {
            let _ = writeln!(out, "  {:>10}  {}", human(*bytes), show(root));
        }
    }
    let extra: String = args
        .raw
        .iter()
        .map(|a| format!(" {}", shell_quote(a)))
        .collect();
    let _ = writeln!(
        out,
        "Nothing changed. Run '{NAME} compress --apply{extra}' to compress these {} files ({}).",
        todo.len(),
        human(total)
    );
}

fn acquire_lock(state_dir: &Path) -> Result<Option<File>, String> {
    fs::create_dir_all(state_dir)
        .map_err(|e| format!("cannot create {}: {e}", state_dir.display()))?;
    let path = state_dir.join("run.lock");
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(format!("cannot lock {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    fn compress(args: &[&str]) -> CompressArgs {
        match parse(args) {
            Ok(Cli::Compress(a)) => a,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    #[test]
    fn cli_parsing() {
        // No command: preview with the old flags still accepted.
        let a = compress(&["--config", "/x.toml", "--dry-run", "-v"]);
        assert_eq!(a.config.as_deref(), Some(Path::new("/x.toml")));
        assert!(!a.apply && a.verbose);
        let a = compress(&["--config=/y.toml"]);
        assert_eq!(a.config.as_deref(), Some(Path::new("/y.toml")));
        assert!(!compress(&[]).apply);
        assert!(compress(&["compress", "--apply"]).apply);
        assert!(parse(&["--config"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["compress", "--apply", "--dry-run"]).is_err());
        assert!(parse(&["compress", "extra"]).is_err());
    }

    #[test]
    fn multi_value_options() {
        let a = compress(&[
            "compress",
            "--dir",
            "~/w",
            "--search",
            "**/.venv",
            "*.csv",
            "--exclude",
            "*.log",
            "-v",
            "--search",
            "/abs/**",
            "--ignore-global-search",
            "--no-progress",
        ]);
        assert_eq!(a.dir.as_deref(), Some("~/w"));
        assert_eq!(a.search, ["**/.venv", "*.csv", "/abs/**"]);
        assert_eq!(a.exclude, ["*.log"]);
        assert!(a.verbose && a.ignore_global && a.progress == Some(false));
        assert!(parse(&["--search", "--apply"]).is_err());
        assert!(parse(&["--exclude"]).is_err());
        let a = compress(&["--apply", "--search", "a b", "--config", "/c.toml"]);
        assert_eq!(a.raw, ["--search", "a b", "--config", "/c.toml"]);
    }

    #[test]
    fn stat_and_quarantine_parsing() {
        let Ok(Cli::Stat(a)) = parse(&["stat"]) else {
            panic!()
        };
        assert_eq!(a.top, Some(stat::DEFAULT_TOP));
        let Ok(Cli::Stat(a)) = parse(&["stat", "--top", "5", "--config", "/c.toml"]) else {
            panic!()
        };
        assert_eq!(
            (a.top, a.config.as_deref()),
            (Some(5), Some(Path::new("/c.toml")))
        );
        let Ok(Cli::Stat(a)) = parse(&["stat", "--all"]) else {
            panic!()
        };
        assert_eq!(a.top, None);
        assert!(parse(&["stat", "--top", "x"]).is_err());
        let ok = |args: &[&str]| matches!(parse(args), Ok(Cli::Quarantine { .. }));
        assert!(ok(&["quarantine", "list"]));
        assert!(ok(&["quarantine", "clear", "--all"]));
        assert!(ok(&[
            "quarantine",
            "clear",
            "/a/b.py",
            "--config",
            "/c.toml"
        ]));
        let Ok(Cli::Quarantine {
            cmd: QuarantineCmd::Clear(Some(p)),
            ..
        }) = parse(&["quarantine", "clear", "/a"])
        else {
            panic!()
        };
        assert_eq!(p, "/a");
        for bad in [
            &["quarantine"][..],
            &["quarantine", "clear"],
            &["quarantine", "clear", "/a", "--all"],
            &["quarantine", "list", "x"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn relative_globs_resolve_against_dir_or_cwd() {
        let home = Path::new("/Users/u");
        let cfg = Config::parse(
            "patterns = [\"~/Work_*/**/.venv\"]\nexclude = [\"target\"]",
            home,
        )
        .unwrap();
        let mut a = compress(&[
            "--search",
            "**/.venv",
            "../other/*.csv",
            "~/x/**",
            "/abs/**",
            "--exclude",
            "*.log",
            "sub/**",
            "**/tmp",
            "/opt/**",
        ]);
        let base = Path::new("/Users/u/Work_me");
        let (p, e) = active_globs(&a, &cfg, base, home).unwrap();
        assert_eq!(
            p,
            [
                "/Users/u/Work_*/**/.venv",
                "/Users/u/Work_me/**/.venv",
                "/Users/u/other/*.csv",
                "/Users/u/x/**",
                "/abs/**"
            ]
        );
        assert_eq!(
            e,
            [
                "target",
                "*.log",
                "/Users/u/Work_me/sub/**",
                "**/tmp",
                "/opt/**"
            ]
        );
        a.ignore_global = true;
        let (p, e) = active_globs(&a, &cfg, base, home).unwrap();
        assert_eq!(p.len(), 4);
        assert_eq!(e.len(), 4);
        assert_eq!(normalize(Path::new("/a/./b/../c")), Path::new("/a/c"));
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("--dir"), "--dir");
        assert_eq!(shell_quote("/a/b"), "/a/b");
        assert_eq!(shell_quote("**/.venv"), "'**/.venv'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn second_lock_is_refused() {
        let dir = std::env::temp_dir().join(format!("acz-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let first = acquire_lock(&dir).unwrap();
        assert!(first.is_some());
        assert!(acquire_lock(&dir).unwrap().is_none());
        drop(first);
        // A child forked by a parallel test briefly shares the lock until its exec closes the
        // CLOEXEC descriptor, so allow a short retry.
        let reacquired = (0..100).any(|_| {
            let ok = acquire_lock(&dir).unwrap().is_some();
            if !ok {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            ok
        });
        assert!(reacquired);
        fs::remove_dir_all(&dir).unwrap();
    }
}
