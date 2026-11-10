//! auto-compress-apfs: turns on APFS transparent compression for files matched by glob
//! patterns, using Apple's `/usr/bin/ditto --hfsCompression` or an in-process LZFSE writer.

mod applesauce;
mod cache;
mod cargo_clean;
mod classify;
mod clean;
mod compress;
mod config;
mod dedup;
mod launchd;
mod logger;
mod progress;
mod quarantine;
mod remove;
mod stat;
mod state;
mod sys;
mod tm_exclude;
mod walk;

use classify::{Class, Rules};
use compress::{Backend, Compressor, Outcome, ProbeError};
use config::Config;
use logger::{Level, Logger};
use progress::{FileEvent, Progress, abbreviate_home, human};
use quarantine::{Entry, Release};
use state::{Day, Decision, DedupRow, DedupState, Row, State};
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
/// Preview width on a terminal when `COLUMNS` is not set.
const PREVIEW_WIDTH: usize = 120;
/// Roots listed in the preview totals.
const PREVIEW_TOP_ROOTS: usize = 20;

const USAGE: &str = "\
Usage:
  auto-compress-apfs                         this help
  auto-compress-apfs compress [OPTIONS]      preview; with --apply: compress (options
                                             without a command also mean compress)
  auto-compress-apfs trash [--apply] [--config PATH] [-v]
  auto-compress-apfs purge [--apply] [--config PATH] [-v]
  auto-compress-apfs cargo-clean [--apply] [--config PATH] [-v]
  auto-compress-apfs tm-exclude [--apply] [--config PATH]
  auto-compress-apfs dedup [--apply] [--config PATH] [-v]
  auto-compress-apfs run [--apply] [--config PATH] [-v]
  auto-compress-apfs stat [--top N] [--all] [--config PATH]
  auto-compress-apfs quarantine list [--config PATH]
  auto-compress-apfs quarantine clear <PATH>|--all [--config PATH]
  auto-compress-apfs clean [--apply] [--config PATH]
  auto-compress-apfs install-launchd [--config PATH]
  auto-compress-apfs uninstall-launchd [--config PATH]
  auto-compress-apfs check-launchd [--config PATH]
  auto-compress-apfs -h|--help | -V|--version

Turns on APFS transparent compression (ditto --hfsCompression, or the opt-in in-process
applesauce backend) for files matched by the glob patterns in the config. Without --apply
nothing is changed: the files that would be compressed are listed.

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
  --backend NAME             ditto or applesauce for every size, this run (default: the
                             config's backend_by_size)
  --progress, --no-progress  force or suppress terminal progress (default: stderr is a terminal)
  -v, --verbose              DEBUG level, log lines mirrored to stderr

stat options:
  --top N                    rows to list (default 30)
  --all                      list every row

clean options:
  --apply                    delete the listed leftover dirs that are not blocked
                             (default: preview only, nothing is changed)

trash / purge: files and directories matched by the [trash] / [purge] patterns of the config
(a directory as a whole) are listed; with --apply moved to ~/.Trash / deleted for good, each
written to <log_dir>/removed.tsv. Protected locations and the tool's own dirs are refused.
cargo-clean: Cargo projects are found by their Cargo.toml below the [cargo_clean] roots; in
their target and build dirs (workspace root, build.target-dir / build.build-dir of
.cargo/config.toml, only with Cargo's CACHEDIR.TAG) the build units (.fingerprint, build,
deps, incremental entries) not used for max_age_secs are listed, plus the final debug /
release artifacts when keep_final_debug / keep_final_release is false; with --apply deleted
for good, the rest stays. A profile whose .cargo-lock a build holds is skipped.
trash / purge / cargo-clean / dedup show a progress line on a terminal and the totals; -v
lists every item (and why one is blocked) and mirrors the log to stderr; run -v passes it on.
tm-exclude: directories matched by the [tm_exclude] patterns are listed; with --apply excluded
from Time Machine (the sticky attribute `tmutil addexclusion` sets; tmutil is not run). An
existing exclusion is never removed or changed.
dedup: identical files below the [dedup] patterns are listed by group; with --apply each
duplicate whose metadata equals the kept file's is replaced by an APFS clone of it.
run: trash, purge, cargo-clean, compress, dedup, tm-exclude, each as its section's `run` says (true, false
or dry-run-only; compress is on by default, the others off); what launchd runs.

install-launchd: writes run.sh next to the config and a per-user launchd agent that starts it
at login and daily at 09:00; run.sh runs `run --apply` once a day from 09:00 on (a missed
run starts once later) and only logs to launchd.log when the binary is gone.
uninstall-launchd: unloads the agent and removes it and run.sh.
check-launchd: read-only; prints OK (exit 0) when the agent is installed, loaded and written by
this version, else INSTALL or REINSTALL with the reason and the command to run (exit 1);
exit 2 when the check itself failed.";

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
    /// `None`: the config's routing (`backend_by_size`).
    backend: Option<config::Backend>,
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
struct CleanArgs {
    config: Option<PathBuf>,
    apply: bool,
    /// `trash` / `purge` / `cargo-clean` (and `run`): list every item and mirror the log to
    /// stderr; without it a progress line (on a terminal) and the totals only.
    verbose: bool,
}

/// Options of `dedup`.
#[derive(Debug)]
struct DedupArgs {
    clean: CleanArgs,
    verbose: bool,
}

#[derive(Debug)]
enum QuarantineCmd {
    List,
    /// `None` releases every entry.
    Clear(Option<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchdCmd {
    Install,
    Uninstall,
    Check,
}

#[derive(Debug)]
enum Cli {
    Compress(CompressArgs),
    Stat(StatArgs),
    Clean(CleanArgs),
    /// `trash` or `purge` (same options as `clean`).
    Remove(CleanArgs, remove::Action),
    /// `tm-exclude` (same options as `clean`).
    TmExclude(CleanArgs),
    Dedup(DedupArgs),
    /// `run`: the configured steps in order, as the launchd agent starts them.
    Run(CleanArgs),
    Quarantine {
        config: Option<PathBuf>,
        cmd: QuarantineCmd,
    },
    /// `install-launchd`, `uninstall-launchd` or `check-launchd`.
    Launchd {
        config: Option<PathBuf>,
        cmd: LaunchdCmd,
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
            "-v" | "--verbose" => {
                // Not repeated in the `--apply` hint: it only changes what is printed.
                a.verbose = true;
                a.raw.truncate(start);
            }
            "--progress" => a.progress = Some(true),
            "--no-progress" => a.progress = Some(false),
            "--ignore-global-search" => a.ignore_global = true,
            "--backend" => {
                let b = it.next().ok_or("--backend needs ditto or applesauce")?;
                a.raw.push(b.clone());
                a.backend = Some(
                    config::Backend::parse(&b)
                        .ok_or_else(|| format!("--backend: `{b}` is not ditto or applesauce"))?,
                );
            }
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

fn parse_clean(mut it: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut a = CleanArgs {
        config: None,
        apply: false,
        verbose: false,
    };
    while let Some(arg) = it.next() {
        if let Some(exit) = help_or_version(&arg) {
            return Ok(exit);
        }
        if config_opt(&arg, &mut it, &mut a.config)? {
            continue;
        }
        match arg.as_str() {
            "--apply" => a.apply = true,
            "-v" | "--verbose" => a.verbose = true,
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(Cli::Clean(a))
}

fn parse_dedup(mut it: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut a = DedupArgs {
        clean: CleanArgs {
            config: None,
            apply: false,
            verbose: false,
        },
        verbose: false,
    };
    while let Some(arg) = it.next() {
        if let Some(exit) = help_or_version(&arg) {
            return Ok(exit);
        }
        if config_opt(&arg, &mut it, &mut a.clean.config)? {
            continue;
        }
        match arg.as_str() {
            "--apply" => a.clean.apply = true,
            "-v" | "--verbose" => {
                a.verbose = true;
                a.clean.verbose = true;
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(Cli::Dedup(a))
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

fn parse_launchd(mut it: impl Iterator<Item = String>, cmd: LaunchdCmd) -> Result<Cli, String> {
    let mut config = None;
    while let Some(arg) = it.next() {
        if let Some(exit) = help_or_version(&arg) {
            return Ok(exit);
        }
        if !config_opt(&arg, &mut it, &mut config)? {
            return Err(format!("unknown argument `{arg}`"));
        }
    }
    Ok(Cli::Launchd { config, cmd })
}

fn parse_args(it: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut it = it.peekable();
    match it.peek().map(String::as_str) {
        None => {
            println!("{USAGE}");
            Ok(Cli::Exit(ExitCode::SUCCESS))
        }
        Some(c @ ("trash" | "purge" | "cargo-clean" | "tm-exclude" | "run")) => {
            let c = c.to_owned();
            match parse_clean(it.skip(1))? {
                Cli::Clean(a) => Ok(match c.as_str() {
                    "trash" => Cli::Remove(a, remove::Action::Trash),
                    "purge" => Cli::Remove(a, remove::Action::Purge),
                    "cargo-clean" => Cli::Remove(a, remove::Action::CargoClean),
                    "tm-exclude" => Cli::TmExclude(a),
                    _ => Cli::Run(a),
                }),
                other => Ok(other),
            }
        }
        Some("compress") => parse_compress(it.skip(1)),
        Some("stat") => parse_stat(it.skip(1)),
        Some("dedup") => parse_dedup(it.skip(1)),
        Some("clean") => parse_clean(it.skip(1)),
        Some("quarantine") => parse_quarantine(it.skip(1)),
        Some("install-launchd") => parse_launchd(it.skip(1), LaunchdCmd::Install),
        Some("uninstall-launchd") => parse_launchd(it.skip(1), LaunchdCmd::Uninstall),
        Some("check-launchd") => parse_launchd(it.skip(1), LaunchdCmd::Check),
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
        Cli::Clean(args) => return cmd_clean(&args, &home),
        Cli::Remove(args, action) => return cmd_remove(&args, action, &home),
        Cli::TmExclude(args) => return cmd_tm_exclude(&args, &home),
        Cli::Dedup(args) => return cmd_dedup(&args, &home),
        Cli::Run(args) => return cmd_run(&args, &home),
        Cli::Quarantine { config, cmd } => cmd_quarantine(config, &cmd, &home),
        Cli::Launchd { config, cmd } => cmd_launchd(config, cmd, &home),
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

/// `install-launchd` / `uninstall-launchd` for the effective config (see [`launchd`]). Both
/// refuse while a run holds the lock: unloading the agent would kill that run. Uninstall
/// neither reads nor creates the config.
fn cmd_launchd(config: Option<PathBuf>, cmd: LaunchdCmd, home: &Path) -> Result<ExitCode, String> {
    let cwd = current_dir()?;
    let config = config.map(|c| normalize(&cwd.join(c)));
    let config_path = config_path(config.as_ref(), home);
    let script = config_path.with_file_name(launchd::SCRIPT_NAME);
    let plist = launchd::agents_dir(home).join(format!("{}.plist", launchd::LABEL));
    if cmd == LaunchdCmd::Check {
        return check_launchd(config.as_deref(), &plist, &script, home);
    }
    // Released before the agent is loaded: its first run takes the lock itself.
    if acquire_lock(&state_dir(home))?.is_none() {
        return Err("a run is in progress, retry when it has finished".into());
    }
    if cmd == LaunchdCmd::Uninstall {
        launchd::uninstall(&plist, &script)?;
        return Ok(ExitCode::SUCCESS);
    }
    let (cfg, created) = Config::load_or_create(&config_path, home)?;
    if created {
        println!("created {}", config_path.display());
    }
    // Lexically normalized, not canonicalized: a symlink stays a symlink.
    let bin = std::env::current_exe()
        .map(|b| normalize(&b))
        .map_err(|e| format!("cannot locate this binary: {e}"))?;
    launchd::install(&launchd::Agent {
        bin,
        config,
        script,
        plist,
        launchd_log: cfg.log_dir().join(logger::LAUNCHD_LOG_NAME),
        last_run: state_dir(home).join(launchd::LAST_RUN_NAME),
    })?;
    Ok(ExitCode::SUCCESS)
}

/// `check-launchd`: read-only. The script checked is the one the installed plist starts
/// (else the one next to the config). Compares the installed run.sh and plist with what this
/// version would write for the binary and config run.sh names, and asks launchd whether the
/// agent is loaded. Exit 0: nothing to do; 1: run `install-launchd` (the printed command keeps
/// the installed config); 2: the check itself failed (unreadable file, invalid config).
fn check_launchd(
    config: Option<&Path>,
    plist: &Path,
    script: &Path,
    home: &Path,
) -> Result<ExitCode, String> {
    let script = fs::read_to_string(plist)
        .ok()
        .and_then(|t| launchd::plist_script(&t))
        .unwrap_or_else(|| script.to_path_buf());
    let script = script.as_path();
    let agent_for = |bin: PathBuf, installed_config: Option<PathBuf>| {
        let path = config_path(installed_config.as_ref(), home);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Err(format!(
                    "{} named by run.sh is missing",
                    path.display()
                )));
            }
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let cfg = Config::parse(&text, home)
            .map_err(|e| format!("invalid config {}: {e}", path.display()))?;
        Ok(Ok(launchd::Agent {
            bin,
            config: installed_config,
            script: script.to_path_buf(),
            plist: plist.to_path_buf(),
            launchd_log: cfg.log_dir().join(logger::LAUNCHD_LOG_NAME),
            last_run: state_dir(home).join(launchd::LAST_RUN_NAME),
        }))
    };
    let ask_launchd = !launchd::skip_launchctl();
    let verdict = match launchd::check(plist, script, ask_launchd, agent_for) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{NAME}: check-launchd failed: {e}");
            return Ok(ExitCode::from(2));
        }
    };
    // Keep the config the installed agent runs; fall back to --config for a new install.
    let config = match launchd::installed_config(script) {
        Some(installed) => installed,
        None => config.map(Path::to_path_buf),
    };
    let fix = match config {
        Some(c) => format!(
            "{NAME} install-launchd --config {}",
            shell_quote(&c.to_string_lossy())
        ),
        None => format!("{NAME} install-launchd"),
    };
    Ok(match verdict {
        launchd::Verdict::UpToDate => {
            let loaded = if ask_launchd {
                "loaded"
            } else {
                "launchd not asked"
            };
            println!(
                "OK: the launchd agent is installed ({loaded}) and up to date ({})",
                plist.display()
            );
            ExitCode::SUCCESS
        }
        launchd::Verdict::Install(why) => {
            println!("INSTALL: {why}; run: {fix}");
            ExitCode::FAILURE
        }
        launchd::Verdict::Reinstall(why) => {
            println!("REINSTALL: {why}; run: {fix}");
            ExitCode::FAILURE
        }
    })
}

/// `quarantine list` (read-only) and `quarantine clear` (under the run lock; also resets the
/// streaks in `state.tsv` and `dedup.tsv`).
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
    // Both states are read first: a read failure releases nothing.
    let state_path = dir.join(state::STATE_NAME);
    let mut st = State::load(&state_path)
        .map_err(|e| format!("cannot read {}: {e}", state_path.display()))?;
    let dedup_path = dir.join(state::DEDUP_STATE_NAME);
    let mut dst = DedupState::load(&dedup_path)
        .map_err(|e| format!("cannot read {}: {e}", dedup_path.display()))?;
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
    for p in &released {
        if let Some(r) = st.rows.get_mut(p) {
            r.streak = 0;
        }
        println!("released {}", p.display());
    }
    st.store(&state_path, None)
        .map_err(|e| format!("cannot write {}: {e}", state_path.display()))?;
    let mut changed = false;
    for p in &released {
        if let Some(r) = dst.rows.get_mut(p).filter(|r| r.streak != 0) {
            r.streak = 0;
            changed = true;
        }
    }
    if changed {
        dst.store(&dedup_path, None)
            .map_err(|e| format!("cannot write {}: {e}", dedup_path.display()))?;
    }
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

/// `clean`: lists the leftover staging and rescue dirs found under the config's patterns; with
/// `--apply` deletes them (see [`clean`]). Exit 1 when the lock is busy, or with `--apply` when a
/// dir was blocked or could not be removed.
fn cmd_clean(args: &CleanArgs, home: &Path) -> ExitCode {
    let config_path = config_path(args.config.as_ref(), home);
    let (cfg, created) = match Config::load_or_create(&config_path, home) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Leftovers are only looked for below the compress and dedup patterns.
    if cfg.patterns.is_empty() && cfg.dedup.patterns.is_empty() {
        if created {
            println!("created {}", config_path.display());
        }
        println!(
            "no search patterns: add `patterns` or `[dedup] patterns` to {}",
            config_path.display()
        );
        return ExitCode::SUCCESS;
    }
    let log_dir = cfg.log_dir();
    let log = match Logger::open(&log_dir, cfg.log_max_bytes, cfg.log_keep, false) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{NAME}: cannot open log in {}: {e}", log_dir.display());
            return ExitCode::FAILURE;
        }
    };
    let code = match run_clean(args, &cfg, &config_path, created, home, &log) {
        Ok(code) => code,
        Err(e) => {
            log.error(&e);
            eprintln!("{NAME}: {e}");
            ExitCode::FAILURE
        }
    };
    log.flush();
    code
}

/// `trash` / `purge`: preview by default; with `--apply` under the run lock. Exit 1 when the
/// config cannot be loaded, the log cannot be opened, the lock is busy, or an item failed.
fn cmd_remove(args: &CleanArgs, action: remove::Action, home: &Path) -> ExitCode {
    let config_path = config_path(args.config.as_ref(), home);
    let (cfg, created) = match Config::load_or_create(&config_path, home) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return ExitCode::FAILURE;
        }
    };
    if created {
        println!("created {}", config_path.display());
    }
    let (patterns, _) = remove_rule(&cfg, action);
    if patterns.is_empty() {
        let (section, key) = match action {
            remove::Action::CargoClean => ("cargo_clean", "roots"),
            _ => (action.name(), "patterns"),
        };
        println!(
            "no [{section}] {key}: add them to {}",
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
    let code = match run_remove(args, action, &cfg, &config_path, home, &log) {
        Ok(code) => code,
        Err(e) => {
            log.error(&e);
            eprintln!("{NAME}: {e}");
            ExitCode::FAILURE
        }
    };
    log.flush();
    code
}

fn run_remove(
    args: &CleanArgs,
    action: remove::Action,
    cfg: &Config,
    config_path: &Path,
    home: &Path,
    log: &Logger,
) -> Result<ExitCode, String> {
    let name = action.name();
    log.info(format_args!(
        "start {NAME} {VERSION} {name} config={} dry_run={}",
        config_path.display(),
        !args.apply
    ));
    // Deleting or moving never reads file data, but a stray read must not download.
    if let Err(e) = sys::disable_dataless_materialization() {
        log.warn(format_args!(
            "cannot turn off materialization of dataless files: {e}"
        ));
    }
    let _lock = if args.apply {
        match acquire_lock(&state_dir(home))? {
            Some(f) => Some(f),
            None => {
                log.info("another run in progress, exiting");
                eprintln!("{NAME}: a run is in progress, try again when it has finished");
                return Ok(ExitCode::FAILURE);
            }
        }
    } else {
        None
    };
    let (patterns, exclude) = remove_rule(cfg, action);
    let guard = walk::Guard::new(home, &cfg.allow_volumes, log)?;
    let matcher = walk::Matcher::new(patterns, exclude, None)?;
    // cargo-clean walks its roots for `Cargo.toml`, never into build output, VCS or JS deps.
    let manifest_matcher = if action == remove::Action::CargoClean {
        let globs: Vec<String> = patterns
            .iter()
            .map(|r| format!("{}/**/Cargo.toml", r.trim_end_matches('/')))
            .collect();
        let mut skip = exclude.to_vec();
        skip.extend(["target", ".git", "node_modules"].map(String::from));
        Some(walk::Matcher::new(&globs, &skip, None)?)
    } else {
        None
    };
    // Without -v: a progress line on a terminal, the items stay in the log.
    if !args.verbose && std::io::stderr().is_terminal() {
        let width = std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(PROGRESS_WIDTH);
        let color = std::env::var_os("NO_COLOR").is_none();
        log.enable_progress(Progress::new(width, color, Some(home)));
    }
    let walked = walk::walk_entries(
        manifest_matcher.as_ref().unwrap_or(&matcher),
        &guard,
        cfg.one_file_system,
        cfg.worker_threads(),
        log,
    );
    let own = own_paths(cfg, config_path, home);
    let trash_dir = home.join(".Trash");
    let mut scope = vec![home.to_path_buf()];
    scope.extend(guard.allowed_mounts().iter().cloned());
    let keep = remove::Keep {
        scope: &scope,
        own: &own,
        trash_dir: &trash_dir,
    };
    // cargo-clean: the walk finds target directories; their stale entries are the items. The
    // targets hold the profile locks until the deletes are done.
    let (matched, targets) = if action == remove::Action::CargoClean {
        let manifests: Vec<PathBuf> = walked
            .matched
            .into_iter()
            .filter(|m| !m.is_dir)
            .map(|m| m.path)
            .collect();
        let found = cargo_clean::target_dirs(&manifests, home);
        log.info(format_args!(
            "{name}: {} manifests, {} target dirs",
            manifests.len(),
            found.len()
        ));
        let keep_final = cargo_clean::KeepFinal {
            debug: cfg.cargo_clean.keep_final_debug,
            release: cfg.cargo_clean.keep_final_release,
        };
        let targets =
            cargo_clean::collect(found, &matcher, cfg.cargo_clean.max_age_secs, keep_final);
        let mut matched = Vec::new();
        for t in &targets {
            for (profile, why) in &t.skipped {
                log.info(format_args!("{name}: skipped {}: {why}", profile.display()));
                println!("SKIPPED {}: {why}", abbreviate_home(profile, Some(home)));
            }
            matched.extend(t.entries.iter().cloned().filter_map(cargo_clean::matched));
        }
        matched.sort_by(|a, b| a.path.cmp(&b.path));
        (matched, targets)
    } else {
        (walked.matched, Vec::new())
    };
    log.start_compress(matched.len() as u64, 0);
    let mut items = remove::plan(matched, &matcher, &guard, &keep, action, &|| log.advance(0));
    for i in &mut items {
        if let Some(t) = targets.iter().find(|t| i.path.starts_with(&t.root)) {
            i.rule.clone_from(&t.rule);
        }
    }
    let blocked = items.iter().filter(|i| i.blocked.is_some()).count() as u64;
    let out = if args.apply {
        let journal = cfg.log_dir().join(remove::JOURNAL_NAME);
        let out = remove::apply(&items, action, &trash_dir, &journal, log);
        log.finish_progress(None);
        let out = out?;
        println!(
            "{name}: {} done ({}), {} failed, {} blocked; journal {}",
            out.done,
            human(out.bytes),
            out.failed,
            out.blocked,
            journal.display()
        );
        out
    } else {
        log.finish_progress(None);
        let extra: String = args
            .config
            .iter()
            .map(|p| format!(" --config {}", shell_quote(&p.display().to_string())))
            .collect();
        print!(
            "{}",
            remove::render(
                &items,
                home,
                action,
                &format!("{NAME} {name} --apply{extra}"),
                args.verbose
            )
        );
        remove::Outcome {
            blocked,
            ..Default::default()
        }
    };
    drop(targets);
    let (count, bytes) = if args.apply {
        (out.done, out.bytes)
    } else {
        items
            .iter()
            .filter(|i| i.blocked.is_none())
            .fold((0, 0), |(n, b), i| (n + 1, b + i.bytes))
    };
    record_total(StepTotal {
        name: match action {
            remove::Action::Trash => "trash",
            remove::Action::Purge => "purge",
            remove::Action::CargoClean => "cargo-clean",
        },
        dry_run: !args.apply,
        what: match action {
            remove::Action::Trash => "items",
            _ => "items",
        },
        count,
        freed: if action == remove::Action::Trash {
            0
        } else {
            bytes
        },
        trashed: if action == remove::Action::Trash {
            bytes
        } else {
            0
        },
        failed: out.failed,
        ..StepTotal::default()
    });
    log.info(format_args!(
        "{name}_summary found={} done={} failed={} blocked={} bytes={} dry_run={}",
        items.len(),
        out.done,
        out.failed,
        out.blocked,
        out.bytes,
        !args.apply
    ));
    Ok(if out.failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Patterns (for `cargo-clean` its roots) and excludes of the section of an action.
fn remove_rule(cfg: &Config, action: remove::Action) -> (&[String], &[String]) {
    match action {
        remove::Action::Trash => (&cfg.trash.patterns, &cfg.trash.exclude),
        remove::Action::Purge => (&cfg.purge.patterns, &cfg.purge.exclude),
        remove::Action::CargoClean => (&cfg.cargo_clean.roots, &cfg.cargo_clean.exclude),
    }
}

/// The tool's own files next to the config (not that whole directory: it may be HOME), its log,
/// state and cache directories.
fn own_paths(cfg: &Config, config_path: &Path, home: &Path) -> [PathBuf; 8] {
    let config_dir = config_path.parent().unwrap_or(Path::new("/"));
    [
        config_path.to_path_buf(),
        config_dir.join(launchd::SCRIPT_NAME),
        quarantine_file(config_path),
        config_dir.join("ditto-probe.txt"),
        config_dir.join("ditto-probe.out"),
        cfg.log_dir(),
        state_dir(home),
        home.join("Library/Caches").join(NAME),
    ]
}

/// `tm-exclude`: preview by default; with `--apply` under the run lock. Exit 1 when the config
/// cannot be loaded, the log cannot be opened, the lock is busy, or a directory had an error.
fn cmd_tm_exclude(args: &CleanArgs, home: &Path) -> ExitCode {
    let config_path = config_path(args.config.as_ref(), home);
    let (cfg, created) = match Config::load_or_create(&config_path, home) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return ExitCode::FAILURE;
        }
    };
    if created {
        println!("created {}", config_path.display());
    }
    if cfg.tm_exclude.patterns.is_empty() {
        println!(
            "no [tm_exclude] patterns: add them to {}",
            config_path.display()
        );
        return ExitCode::SUCCESS;
    }
    let log_dir = cfg.log_dir();
    let log = match Logger::open(&log_dir, cfg.log_max_bytes, cfg.log_keep, false) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{NAME}: cannot open log in {}: {e}", log_dir.display());
            return ExitCode::FAILURE;
        }
    };
    let code = match run_tm_exclude(args, &cfg, &config_path, home, &log) {
        Ok(code) => code,
        Err(e) => {
            log.error(&e);
            eprintln!("{NAME}: {e}");
            ExitCode::FAILURE
        }
    };
    log.flush();
    code
}

fn run_tm_exclude(
    args: &CleanArgs,
    cfg: &Config,
    config_path: &Path,
    home: &Path,
    log: &Logger,
) -> Result<ExitCode, String> {
    log.info(format_args!(
        "start {NAME} {VERSION} tm-exclude config={} dry_run={}",
        config_path.display(),
        !args.apply
    ));
    // Setting an xattr never reads file data, but a stray read must not download.
    if let Err(e) = sys::disable_dataless_materialization() {
        log.warn(format_args!(
            "cannot turn off materialization of dataless files: {e}"
        ));
    }
    let _lock = if args.apply {
        match acquire_lock(&state_dir(home))? {
            Some(f) => Some(f),
            None => {
                log.info("another run in progress, exiting");
                eprintln!("{NAME}: a run is in progress, try again when it has finished");
                return Ok(ExitCode::FAILURE);
            }
        }
    } else {
        None
    };
    let rule = &cfg.tm_exclude;
    let guard = walk::Guard::new(home, &cfg.allow_volumes, log)?;
    let matcher = walk::Matcher::new(&rule.patterns, &rule.exclude, None)?;
    let walked = walk::walk_entries(
        &matcher,
        &guard,
        cfg.one_file_system,
        cfg.worker_threads(),
        log,
    );
    let own = own_paths(cfg, config_path, home);
    let trash_dir = home.join(".Trash");
    let mut scope = vec![home.to_path_buf()];
    scope.extend(guard.allowed_mounts().iter().cloned());
    let keep = remove::Keep {
        scope: &scope,
        own: &own,
        trash_dir: &trash_dir,
    };
    let items = tm_exclude::plan(
        walked.matched,
        &matcher,
        &guard,
        &keep,
        sys::effective_uid(),
        log,
    );
    let out = if args.apply {
        let out = tm_exclude::apply(&items, sys::effective_uid(), log);
        println!(
            "tm-exclude: {} excluded from Time Machine, {} already, {} blocked, {} errors",
            out.excluded, out.already, out.blocked, out.errors
        );
        out
    } else {
        let extra: String = args
            .config
            .iter()
            .map(|p| format!(" --config {}", shell_quote(&p.display().to_string())))
            .collect();
        print!(
            "{}",
            tm_exclude::render(&items, home, &format!("{NAME} tm-exclude --apply{extra}"))
        );
        tm_exclude::tally(&items)
    };
    record_total(StepTotal {
        name: "tm-exclude",
        dry_run: !args.apply,
        what: if args.apply {
            "dirs excluded from Time Machine"
        } else {
            "dirs to exclude from Time Machine"
        },
        count: if args.apply {
            out.excluded
        } else {
            items.len() as u64 - out.already - out.blocked - out.errors
        },
        failed: out.errors,
        ..StepTotal::default()
    });
    log.info(format_args!(
        "tm_exclude_summary matched={} already={} excluded={} blocked={} errors={} dry_run={}",
        items.len(),
        out.already,
        out.excluded,
        out.blocked,
        out.errors,
        !args.apply
    ));
    Ok(if out.errors > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// `dedup`: preview by default; with `--apply` under the run lock. Exit 1 when the config
/// cannot be loaded, the log cannot be opened, the lock is busy, or a replacement failed.
fn cmd_dedup(args: &DedupArgs, home: &Path) -> ExitCode {
    let config_path = config_path(args.clean.config.as_ref(), home);
    let (cfg, created) = match Config::load_or_create(&config_path, home) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return ExitCode::FAILURE;
        }
    };
    if created {
        println!("created {}", config_path.display());
    }
    if cfg.dedup.patterns.is_empty() {
        println!("no [dedup] patterns: add them to {}", config_path.display());
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
    let code = match run_dedup(&args.clean, &cfg, &config_path, home, &log) {
        Ok(code) => code,
        Err(e) => {
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

fn run_dedup(
    args: &CleanArgs,
    cfg: &Config,
    config_path: &Path,
    home: &Path,
    log: &Logger,
) -> Result<ExitCode, String> {
    log.info(format_args!(
        "start {NAME} {VERSION} dedup config={} dry_run={}",
        config_path.display(),
        !args.apply
    ));
    // Hashing and comparing read file data: a dataless file must never be downloaded.
    if let Err(e) = sys::disable_dataless_materialization() {
        log.warn(format_args!(
            "cannot turn off materialization of dataless files: {e}"
        ));
    }
    let _lock = if args.apply {
        match acquire_lock(&state_dir(home))? {
            Some(f) => Some(f),
            None => {
                log.info("another run in progress, exiting");
                eprintln!("{NAME}: a run is in progress, try again when it has finished");
                return Ok(ExitCode::FAILURE);
            }
        }
    } else {
        None
    };
    let qfile = quarantine_file(config_path);
    // Fail closed: without the list, quarantined files would be deduplicated again.
    let quarantined = quarantine::load(&qfile)
        .map(|q| quarantine::paths(&q))
        .map_err(|e| format!("cannot read {}: {e}", qfile.display()))?;
    let state_path = state_dir(home).join(state::DEDUP_STATE_NAME);
    // As in compress: --apply never overwrites a dedup state it could not read.
    let mut st = match DedupState::load(&state_path) {
        Ok(st) => st,
        Err(e) if args.apply => {
            return Err(format!("cannot read {}: {e}", state_path.display()));
        }
        Err(e) => {
            log.warn(format_args!("cannot read {}: {e}", state_path.display()));
            DedupState::default()
        }
    };
    // Only a run over the default config prunes rows, counts as a run day and moves streaks.
    let unrestricted = args.config.is_none();
    let prune = args.apply && unrestricted;
    let track_dirs: Option<HashSet<PathBuf>> = prune.then(|| {
        st.rows
            .keys()
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect()
    });
    let rule = &cfg.dedup;
    let guard = walk::Guard::new(home, &cfg.allow_volumes, log)?;
    let matcher = walk::Matcher::new(&rule.patterns, &rule.exclude, None)?;
    let walked = walk::walk(
        &matcher,
        &guard,
        cfg.one_file_system,
        cfg.worker_threads(),
        log,
        track_dirs.as_ref(),
    );
    for c in &walked.candidates {
        if let Some(r) = st.rows.get_mut(&c.path) {
            r.seen = true;
        }
    }
    let found = walked.candidates.len() as u64;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let today = today(now)?;
    let cands = dedup::eligible(
        walked.candidates,
        rule.min_size,
        sys::effective_uid(),
        now.saturating_sub_unsigned(cfg.min_age_secs),
    );
    let opts = dedup::PlanOptions {
        verify: cfg.verify,
        quarantined: &quarantined,
    };
    let mut plan = dedup::plan(cands, found, &opts, log);
    let streaks = dedup::Streaks {
        today,
        prev: st.previous_run_day(today),
        threshold: cfg.quarantine_after,
        unrestricted,
    };
    let reason = format!(
        "no longer shares its data with identical files on {} consecutive run days, \
         quarantined (release: {NAME} quarantine clear <path>)",
        cfg.quarantine_after
    );
    let new_quarantine = dedup::track(&mut plan, &mut st, &streaks, &reason);
    let (quarantined_n, requarantined_new) = plan.quarantined();
    let targets: usize = plan.groups.iter().map(|g| g.targets.len()).sum();
    let out = if args.apply {
        let out = dedup::apply(&plan, log);
        for (p, ident) in &out.deduped_paths {
            if !st.record_deduped(p, DedupRow::new(*ident, now)) {
                log.debug(format_args!(
                    "{}: path contains TAB or LF, not recorded in state",
                    p.display()
                ));
            }
        }
        // quarantine.txt first: a streak is reset only for files actually listed.
        if !new_quarantine.is_empty() {
            let stamp = format!("{} dedup", logger::timestamp(SystemTime::now()));
            let entries: Vec<Entry> = new_quarantine
                .iter()
                .map(|p| Entry {
                    stamp: stamp.clone(),
                    path: p.clone(),
                })
                .collect();
            match quarantine::append(&qfile, &entries) {
                Ok(_) => {
                    for p in &new_quarantine {
                        if let Some(r) = st.rows.get_mut(p) {
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
        println!(
            "dedup: deduped={} freed\u{2248}{} skipped={} failed={} quarantined={} \
             requarantined_new={}\n{}",
            out.deduped,
            human(out.freed),
            out.skipped,
            out.failed,
            quarantined_n,
            requarantined_new,
            dedup::SNAPSHOT_NOTE
        );
        out
    } else {
        let extra: String = args
            .config
            .iter()
            .map(|p| format!(" --config {}", shell_quote(&p.display().to_string())))
            .collect();
        print!(
            "{}",
            dedup::render(
                &plan,
                home,
                &format!("{NAME} dedup --apply{extra}"),
                args.verbose
            )
        );
        dedup::Outcome::default()
    };
    let (count, freed) = if args.apply {
        (out.deduped, out.freed)
    } else {
        plan.pending()
            .fold((0, 0), |(n, b), (_, t)| (n + 1, b + t.c.allocated()))
    };
    record_total(StepTotal {
        name: "dedup",
        dry_run: !args.apply,
        what: if args.apply {
            "files cloned"
        } else {
            "files to clone"
        },
        count,
        freed,
        failed: out.failed,
        ..StepTotal::default()
    });
    log.info(format_args!(
        "dedup_summary walked={} groups={} targets={} already_shared={} unreadable={} \
         deduped={} freed={} skipped={} failed={} quarantined={} requarantined_new={} dry_run={}",
        plan.walked,
        plan.groups.len(),
        targets,
        plan.already_shared,
        plan.unreadable,
        out.deduped,
        out.freed,
        out.skipped,
        out.failed,
        quarantined_n,
        requarantined_new,
        !args.apply
    ));
    Ok(if out.failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// `run`: trash, purge, cargo-clean, compress, dedup and tm-exclude in that order, each as its section's
/// `run` says: `false` skips it, `true` runs it with the same `--apply` and `--config`,
/// `"dry-run-only"` runs it as a preview even under `--apply`. A section without patterns is
/// skipped (compress prints its own hint). Exit 1 when any step failed.
fn cmd_run(args: &CleanArgs, home: &Path) -> ExitCode {
    println!("{NAME} {VERSION}");
    let config_path = config_path(args.config.as_ref(), home);
    let cfg = match Config::load_or_create(&config_path, home) {
        Ok((c, _)) => c,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut failed = false;
    let steps = [
        ("trash", cfg.trash.run, !cfg.trash.patterns.is_empty()),
        ("purge", cfg.purge.run, !cfg.purge.patterns.is_empty()),
        (
            "cargo-clean",
            cfg.cargo_clean.run,
            !cfg.cargo_clean.roots.is_empty(),
        ),
        ("compress", cfg.compress.run, true),
        ("dedup", cfg.dedup.run, !cfg.dedup.patterns.is_empty()),
        (
            "tm-exclude",
            cfg.tm_exclude.run,
            !cfg.tm_exclude.patterns.is_empty(),
        ),
    ];
    for (name, mode, has_patterns) in steps {
        let apply = match mode {
            config::RunMode::Off => continue,
            _ if !has_patterns => continue,
            config::RunMode::On => args.apply,
            config::RunMode::DryRunOnly => {
                log_run_step(&cfg, name, args.verbose);
                false
            }
        };
        println!("== {name} ==");
        let step = CleanArgs {
            config: args.config.clone(),
            apply,
            verbose: args.verbose,
        };
        let code = match name {
            "trash" => cmd_remove(&step, remove::Action::Trash, home),
            "purge" => cmd_remove(&step, remove::Action::Purge, home),
            "cargo-clean" => cmd_remove(&step, remove::Action::CargoClean, home),
            "compress" => cmd_compress(
                &CompressArgs {
                    config: step.config,
                    apply,
                    verbose: step.verbose,
                    ..CompressArgs::default()
                },
                home,
            ),
            "dedup" => cmd_dedup(
                &DedupArgs {
                    verbose: step.verbose,
                    clean: step,
                },
                home,
            ),
            _ => cmd_tm_exclude(&step, home),
        };
        failed |= code != ExitCode::SUCCESS;
    }
    let totals = STEP_TOTALS.lock().map(|v| v.clone()).unwrap_or_default();
    if !totals.is_empty() {
        let text = render_totals(&totals);
        print!("{text}");
        if let Ok(log) = Logger::open(&cfg.log_dir(), cfg.log_max_bytes, cfg.log_keep, false) {
            for line in text
                .lines()
                .filter(|l| !l.is_empty() && !l.starts_with("=="))
            {
                log.info(format_args!("run total: {line}"));
            }
            log.flush();
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// What one command did (with `--apply`) or would do (a preview), for the closing total of
/// `run`. `freed` is space given back to the volume; `trashed` only moves to the Trash.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct StepTotal {
    name: &'static str,
    dry_run: bool,
    /// What the count is of, e.g. "items deleted".
    what: &'static str,
    count: u64,
    freed: u64,
    trashed: u64,
    /// Bytes a preview of compress would compress (the saving is not known before).
    to_compress: u64,
    failed: u64,
}

/// Totals of the commands run in this process; `run` prints them at the end.
static STEP_TOTALS: std::sync::Mutex<Vec<StepTotal>> = std::sync::Mutex::new(Vec::new());

fn record_total(t: StepTotal) {
    if let Ok(mut v) = STEP_TOTALS.lock() {
        v.push(t);
    }
}

/// The closing total of `run`: one line per step, then the space freed (or that would be).
fn render_totals(totals: &[StepTotal]) -> String {
    use std::fmt::Write as _;
    let mut s = String::from("\n== total ==\n");
    for t in totals {
        let mode = if t.dry_run { "preview" } else { "applied" };
        let mut line = format!("{:<12} {mode:<8} {} {}", t.name, t.count, t.what);
        if t.freed > 0 {
            let _ = write!(
                line,
                ", {} {}",
                if t.dry_run { "would free" } else { "freed" },
                human(t.freed)
            );
        }
        if t.trashed > 0 {
            let _ = write!(line, ", {} to the Trash", human(t.trashed));
        }
        if t.to_compress > 0 {
            let _ = write!(line, ", {} to compress", human(t.to_compress));
        }
        if t.failed > 0 {
            let _ = write!(line, ", {} failed", t.failed);
        }
        let _ = writeln!(s, "{line}");
    }
    let sum = |dry: bool| -> (u64, u64) {
        totals
            .iter()
            .filter(|t| t.dry_run == dry)
            .fold((0, 0), |(f, tr), t| (f + t.freed, tr + t.trashed))
    };
    let (freed, trashed) = sum(false);
    if totals.iter().any(|t| !t.dry_run) {
        let _ = write!(s, "freed: about {}", human(freed));
        if trashed > 0 {
            let _ = write!(
                s,
                " (plus {} moved to the Trash, freed when it is emptied)",
                human(trashed)
            );
        }
        let _ = writeln!(s);
    }
    let (freed, trashed) = sum(true);
    if totals.iter().any(|t| t.dry_run) {
        let _ = write!(s, "would free: about {}", human(freed));
        if trashed > 0 {
            let _ = write!(s, " (plus {} to the Trash)", human(trashed));
        }
        let _ = writeln!(s, "; compress savings are not estimated");
    }
    s
}

/// Logs that `run` executes the step `name` as a preview only (`run = "dry-run-only"`); with
/// `verbose` it is printed too.
fn log_run_step(cfg: &Config, name: &str, verbose: bool) {
    let line = format!("run step {name} mode=dry-run-only");
    if verbose {
        println!("{line}");
    }
    match Logger::open(&cfg.log_dir(), cfg.log_max_bytes, cfg.log_keep, false) {
        Ok(log) => {
            log.info(&line);
            log.flush();
        }
        Err(e) => eprintln!(
            "{NAME}: cannot open log in {}: {e}",
            cfg.log_dir().display()
        ),
    }
}

fn run_clean(
    args: &CleanArgs,
    cfg: &Config,
    config_path: &Path,
    created: bool,
    home: &Path,
    log: &Logger,
) -> Result<ExitCode, String> {
    log.info(format_args!(
        "start {NAME} {VERSION} clean config={} dry_run={}",
        config_path.display(),
        !args.apply
    ));
    if created {
        log.info(format_args!(
            "created default config {}",
            config_path.display()
        ));
    }
    if let Err(e) = sys::disable_dataless_materialization() {
        log.warn(format_args!(
            "cannot turn off materialization of dataless files: {e}"
        ));
    }
    // Held across walk, scan and delete; the preview neither takes nor needs it.
    let _lock = if args.apply {
        match acquire_lock(&state_dir(home))? {
            Some(f) => Some(f),
            None => {
                log.info("another run in progress, exiting");
                eprintln!("{NAME}: a run is in progress, try again when it has finished");
                return Ok(ExitCode::FAILURE);
            }
        }
    } else {
        None
    };
    let guard = walk::Guard::new(home, &cfg.allow_volumes, log)?;
    // Each scope with its own excludes (as compress and dedup walk them); a scope without
    // patterns is not walked.
    let mut leftovers: Vec<clean::Leftover> = Vec::new();
    for (patterns, exclude) in [
        (&cfg.patterns, &cfg.exclude),
        (&cfg.dedup.patterns, &cfg.dedup.exclude),
    ] {
        if patterns.is_empty() {
            continue;
        }
        let matcher = walk::Matcher::new(patterns, exclude, None)?;
        let walked = walk::walk(
            &matcher,
            &guard,
            cfg.one_file_system,
            cfg.worker_threads(),
            log,
            None,
        );
        let staging = walked
            .stale_staging
            .iter()
            .map(|p| (p, clean::Kind::Staging));
        let rescue = walked.rescue_dirs.iter().map(|p| (p, clean::Kind::Rescue));
        // A dir found by both scopes is scanned once, by the first; classify then compares this
        // scope's files with every leftover, trying only the files an earlier pass left
        // without a copy.
        let known: HashSet<&Path> = leftovers.iter().map(|l| l.path.as_path()).collect();
        let found: Vec<clean::Leftover> = staging
            .chain(rescue)
            .filter(|(p, _)| !known.contains(p.as_path()))
            .filter_map(|(p, kind)| clean::scan(p, kind, log))
            .collect();
        clean::merge(&mut leftovers, found);
        clean::classify(&mut leftovers, &walked.candidates);
        // One scope's walk at a time: its candidates are dropped before the next walk starts.
        drop(walked);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    clean::block_live(&mut leftovers, now);
    let blocked = leftovers.iter().filter(|l| l.blocked.is_some()).count();
    let removal = if args.apply {
        let r = clean::remove(&leftovers, log);
        println!(
            "removed {} leftover dirs ({} freed), {} failed, {} blocked",
            r.removed,
            human(r.freed),
            r.failed,
            blocked
        );
        for (path, why) in &r.kept {
            eprintln!("{NAME}: kept {}: {why}", path.display());
        }
        if r.failed == 0 && blocked > 0 {
            eprintln!(
                "{NAME}: exit 1 only because {blocked} leftover dirs are blocked; nothing failed"
            );
        }
        r
    } else {
        let extra: String = args
            .config
            .iter()
            .map(|p| format!(" --config {}", shell_quote(&p.display().to_string())))
            .collect();
        let cmd = format!("{NAME} clean --apply{extra}");
        print!("{}", clean::render(&leftovers, home, now, &cmd));
        clean::Removal::default()
    };
    log.info(format_args!(
        "clean_summary found={} removed={} failed={} blocked={blocked} freed_bytes={} dry_run={}",
        leftovers.len(),
        removal.removed,
        removal.failed,
        removal.freed,
        !args.apply
    ));
    Ok(if args.apply && (removal.failed > 0 || blocked > 0) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
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
    too_large: u64,
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
                eprintln!("{NAME}: a run is in progress, exiting (see the log for its progress)");
                return Ok(());
            }
        }
    } else {
        None
    };
    // Before the walk: a compressor that does not compress would only cache every file as
    // incompressible. A preview never runs ditto or writes a copy.
    let routing = match args.backend {
        Some(b) => config::forced_routing(&cfg.routing, b),
        None => cfg.routing.clone(),
    };
    // Every backend the routing uses, in a stable order (ditto first), probed with --apply.
    let mut backends: Vec<(config::Backend, Backend)> = Vec::new();
    for b in [config::Backend::Ditto, config::Backend::Applesauce] {
        if routing.iter().any(|r| r.backend == b) {
            let ready = if apply {
                probe_backend(config_path, home, log, b, cfg.verify)?
            } else {
                Backend::Ditto(&[])
            };
            backends.push((b, ready));
        }
    }
    if cfg.single_backend() && args.backend.is_none() {
        log.info(format_args!(
            "config sets the top-level `backend`: only {} is compressed, other sizes are \
             skipped ([[backend_by_size]] routes sizes to backends)",
            routing[0]
        ));
    }
    // What filled the incompressible cache: per backend in use (ditto first) its options or
    // versions, then `@min..max` of each of its ranges, so any routing change drops it once.
    let mut record: Vec<String> = Vec::new();
    for (b, ready) in &backends {
        let args: &[&str] = match ready {
            Backend::Ditto(args) => args,
            Backend::Applesauce => &applesauce::CACHE_ARGS,
        };
        record.extend(args.iter().map(|a| (*a).to_owned()));
        for r in routing.iter().filter(|r| r.backend == *b) {
            record.push(match r.max_size {
                Some(max) => format!("@{}..{max}", r.min_size),
                None => format!("@{}..", r.min_size),
            });
        }
    }
    let cache_args: Vec<&str> = record.iter().map(String::as_str).collect();
    let cache_args = cache_args.as_slice();

    let cache_path = home
        .join("Library/Caches")
        .join(NAME)
        .join(cache::CACHE_NAME);
    let mut known = cache::load(&cache_path).unwrap_or_else(|e| {
        log.warn(format_args!("cannot read {}: {e}", cache_path.display()));
        HashSet::new()
    });
    // Entries cached by another compressor (or ditto with other options) may be compressible
    // files: re-check them.
    let args_path = cache_path.with_file_name(cache::ARGS_NAME);
    if apply && !known.is_empty() && cache::filled_with_other_args(&args_path, cache_args) {
        log.info(format_args!(
            "incompressible cache was filled under another routing or other backend options, dropped {} entries",
            known.len()
        ));
        known.clear();
    }
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
    let progress = args
        .progress
        .unwrap_or_else(|| std::io::stderr().is_terminal());
    // Without progress or -v, ERROR/WARN lines go to the log file only.
    let notes_in = (!progress && !args.verbose).then(|| log.path());
    if progress {
        let width = std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(PROGRESS_WIDTH);
        let color = std::env::var_os("NO_COLOR").is_none();
        log.enable_progress(Progress::new(width, color, Some(home)));
    }
    // Too-small files are only counted; `seen` is read only when this run prunes.
    let walked = walk::walk_reduced(
        &matcher,
        &guard,
        cfg.one_file_system,
        threads,
        log,
        track_dirs.as_ref(),
        &walk::Reduce {
            min_size: config::lowest_min_size(&routing),
            tracked: &|p: &Path| prune && st.rows.contains_key(p),
        },
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
        routing: &routing,
        mtime_cutoff: now.saturating_sub_unsigned(cfg.min_age_secs),
        incompressible: &known,
    };
    let mut sum = Summary {
        found: walked.candidates.len() as u64 + walked.too_small,
        too_small: walked.too_small,
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
    // The backend of each `todo` file, by index.
    let mut todo_backends = Vec::new();
    // Before `candidates` moves out below.
    let preview_roots = (walked.inaccessible_roots(), walked.refused_walk_roots);
    for p in &walked.too_small_tracked {
        if let Some(r) = st.rows.get_mut(p) {
            r.seen = true;
        }
    }
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
            Class::TooLarge => {
                sum.too_large += 1;
                log.debug(format_args!(
                    "{}: outside every backend size range, skipped",
                    c.path.display()
                ));
            }
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
            Class::Compress(_) if listed => sum.quarantined += 1,
            Class::Compress(backend) => {
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
                todo_backends.push(backend);
            }
        }
    }

    let todo_count = todo.len() as u64;
    let todo_bytes: u64 = todo.iter().map(|c| c.size()).sum();
    let out = if apply {
        compress::remove_stale(&walked.stale_staging, log);
        log.start_compress(todo_count, todo_bytes);
        let out = Compressor::new(log, cfg.verify, cfg.batch_size, threads, cfg.min_free_bytes)
            .run_routed(
                todo.into_iter()
                    .zip(todo_backends)
                    .map(|(c, b)| {
                        let ready = backends.iter().find(|(k, _)| *k == b).map(|(_, r)| *r);
                        (c, ready.expect("every routed backend was probed"))
                    })
                    .collect(),
            );
        let keys = keep_cached.iter().chain(&out.new_incompressible);
        // The record follows the cache: a stale record only costs a re-check.
        match cache::store(&cache_path, keys) {
            Ok(()) => {
                if let Err(e) = cache::store_args(&args_path, cache_args) {
                    log.warn(format_args!("cannot write {}: {e}", args_path.display()));
                }
            }
            Err(e) => log.warn(format_args!("cannot write {}: {e}", cache_path.display())),
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
        print_preview(&todo, &preview_skips, home, args, preview_roots, notes_in);
        Outcome::default()
    };

    record_total(StepTotal {
        name: "compress",
        dry_run: !apply,
        what: "files",
        count: if apply { out.compressed } else { todo_count },
        freed: out.bytes_before.saturating_sub(out.bytes_after),
        to_compress: if apply { 0 } else { todo_bytes },
        failed: out.errors,
        ..StepTotal::default()
    });
    let elapsed = started.elapsed().as_secs_f64();
    log.info(format_args!(
        "summary found={} already_compressed={} to_compress={} compressed={} incompressible={} \
         too_small={} too_large={} too_new={} skipped={} errors={} bytes_before={} bytes_after={} \
         ditto_calls={} stale_staging={} quarantined={} requarantined_new={} refused_roots={} \
         unreadable_roots={} dataless={} excluded={} flagged={} skipped_low_space={} dry_run={} elapsed={elapsed:.3}s",
        sum.found,
        sum.already_compressed,
        sum.to_compress,
        out.compressed,
        sum.incompressible + out.incompressible,
        sum.too_small,
        sum.too_large,
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
        walked.unreadable_roots,
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

/// The backend ready to compress here: for ditto the options that compress, for applesauce a
/// passed self-probe. Probed next to the config, or in the state dir when the probe files
/// cannot be written there (or it is not on APFS). With `verify = "extended"` the ditto
/// probe copy must also decode independently ([`compress::probe_decode`]). `Err` when the
/// backend does not compress (or cannot be verified that way).
fn probe_backend(
    config_path: &Path,
    home: &Path,
    log: &Logger,
    backend: config::Backend,
    verify: config::VerifyMode,
) -> Result<Backend, String> {
    type Probe = fn(&Path) -> Result<Backend, ProbeError>;
    let (name, probe): (&str, Probe) = match backend {
        config::Backend::Ditto => ("ditto", |d| compress::probe(d).map(Backend::Ditto)),
        config::Backend::Applesauce => ("applesauce", |d| {
            compress::probe_applesauce(d).map(|()| Backend::Applesauce)
        }),
    };
    let config_dir = config_path.parent().unwrap_or(Path::new("."));
    let mut r = probe(config_dir);
    let mut dir = config_dir.to_path_buf();
    if let Err(ProbeError::Setup(e)) = &r {
        log.warn(format_args!("{name} probe: {e}; probing in the state dir"));
        dir = state_dir(home);
        r = fs::create_dir_all(&dir)
            .map_err(|e| ProbeError::Setup(format!("cannot create {}: {e}", dir.display())))
            .and_then(|()| probe(&dir));
    }
    if verify == config::VerifyMode::Extended
        && let Ok(Backend::Ditto(_)) = r
        && let Err(e) = compress::probe_decode(&dir)
    {
        return Err(format!(
            "ditto copies cannot be verified independently, nothing was changed (probe in {}: {e})",
            dir.display()
        ));
    }
    match r {
        Ok(Backend::Ditto(args)) => {
            log.info(format_args!("ditto compresses with: {}", args.join(" ")));
            Ok(Backend::Ditto(args))
        }
        Ok(Backend::Applesauce) => {
            log.info(format_args!(
                "applesauce compresses (LZFSE, {})",
                applesauce::CACHE_ARGS[2..].join(" ")
            ));
            Ok(Backend::Applesauce)
        }
        Err(ProbeError::Setup(e)) => Err(format!("cannot probe {name}, nothing was changed: {e}")),
        Err(ProbeError::NoCompression(e)) => Err(format!(
            "{name} does not compress on this system, nothing was changed (probe in {}: {e})",
            dir.display()
        )),
    }
}

/// Quotes an argument for a POSIX shell when it needs it.
pub(crate) fn shell_quote(s: &str) -> String {
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
/// totals with the biggest roots, and the command that would do it. `(inaccessible, refused)`
/// walk roots get a note each before the totals ([`preview_notes`], pointing at `log` when the
/// ERROR/WARN lines were not shown on stderr); inaccessible ones also change the closing hint
/// ([`preview_hint`]).
fn print_preview(
    todo: &[walk::Candidate],
    skips: &[PreviewSkip],
    home: &Path,
    args: &CompressArgs,
    (inaccessible, refused): (u64, u64),
    log: Option<&Path>,
) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let show = |p: &Path| abbreviate_home(p, Some(home));
    let width = std::io::stdout().is_terminal().then(|| {
        std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(PREVIEW_WIDTH)
    });
    let rows: Vec<(String, String)> = todo
        .iter()
        .map(|c| (show(&c.path), human(c.size())))
        .collect();
    let total: u64 = todo.iter().map(|c| c.size()).sum();
    // Without -v: the totals and top roots only.
    if args.verbose {
        for line in preview_lines(&rows, width) {
            let _ = writeln!(out, "{line}");
        }
        for s in skips {
            let _ = writeln!(out, "{}   SKIP({}): {}", show(&s.path), s.kind, s.reason);
        }
    }
    let mut roots: HashMap<&Arc<Path>, u64> = HashMap::new();
    for c in todo {
        *roots.entry(&c.match_root).or_default() += c.size();
    }
    let mut roots: Vec<_> = roots.into_iter().collect();
    roots.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let _ = write!(out, "{}", preview_notes(inaccessible, refused, log));
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
    if !args.verbose && (!todo.is_empty() || !skips.is_empty()) {
        let _ = writeln!(out, "Add -v to list every file (and why one is skipped).");
    }
    let _ = write!(
        out,
        "{}",
        preview_hint(todo.len(), total, inaccessible, &extra)
    );
}

/// Notes before the preview totals: roots that could not be read (an access problem, ERROR
/// logged) and roots refused by the guard (by design, WARN logged), each on its own line. The
/// note says where those lines are: "above" on stderr, or in `log` when they went only there.
fn preview_notes(inaccessible: u64, refused: u64, log: Option<&Path>) -> String {
    let at = log.map_or_else(|| "above".to_owned(), |p| format!("in {}", p.display()));
    let mut s = String::new();
    if inaccessible > 0 {
        s += &format!(
            "\n{inaccessible} root(s) not accessible (see ERROR {at}); files there are not \
             listed\n"
        );
    }
    if refused > 0 {
        s += &format!(
            "\n{refused} root(s) refused by the volume/protection guard (see WARN {at})\n"
        );
    }
    s
}

/// The closing lines of the preview: the `--apply` command, preceded by a warning when
/// `inaccessible` roots exist, or a "fix access first" line when nothing would be compressed.
fn preview_hint(files: usize, total: u64, inaccessible: u64, extra: &str) -> String {
    if inaccessible > 0 && files == 0 {
        return "Nothing changed. Fix access first; nothing would be compressed there.\n".into();
    }
    let warn = if inaccessible > 0 {
        format!("Warning: {inaccessible} root(s) not accessible; --apply would skip them too.\n")
    } else {
        String::new()
    };
    format!(
        "{warn}Nothing changed. Run '{NAME} compress --apply{extra}' to compress these {files} \
         files ({}).\n",
        human(total)
    )
}

/// Preview file lines from `(path, size)` rows. Widths count chars, so wide (CJK, emoji)
/// chars may misalign the size column. With a terminal `width` the size column is
/// right-aligned and paths too long for it are cut on the left with `...`; without one (stdout
/// piped or redirected) each line is the full path, one space and the size, so nothing is lost.
fn preview_lines(rows: &[(String, String)], width: Option<usize>) -> Vec<String> {
    const SIZE: usize = 10;
    const GAP: usize = 3;
    let Some(width) = width else {
        return rows.iter().map(|(p, s)| format!("{p} {s}")).collect();
    };
    let col = rows
        .iter()
        .map(|(p, _)| p.chars().count())
        .max()
        .unwrap_or(0)
        .min(width.saturating_sub(SIZE + GAP).max(8));
    rows.iter()
        .map(|(p, s)| {
            format!(
                "{:<col$}{:GAP$}{s:>SIZE$}",
                progress::truncate_left(p, col),
                ""
            )
        })
        .collect()
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

    #[test]
    fn totals_split_applied_and_preview() {
        let t = |name, dry_run, freed, trashed| StepTotal {
            name,
            dry_run,
            what: "items",
            count: 2,
            freed,
            trashed,
            ..StepTotal::default()
        };
        let text = render_totals(&[
            t("trash", false, 0, 2048),
            t("purge", false, 1 << 20, 0),
            t("cargo-clean", true, 3 << 30, 0),
        ]);
        assert!(
            text.contains("purge        applied  2 items, freed 1.0 MiB"),
            "{text}"
        );
        assert!(
            text.contains("cargo-clean  preview  2 items, would free 3.0 GiB"),
            "{text}"
        );
        assert!(
            text.contains("freed: about 1.0 MiB (plus 2.0 KiB moved to the Trash"),
            "{text}"
        );
        assert!(text.contains("would free: about 3.0 GiB"), "{text}");
        let only_preview = render_totals(&[t("purge", true, 5, 0)]);
        assert!(
            !only_preview.contains("\nfreed: ") && !only_preview.contains("plus"),
            "{only_preview}"
        );
    }

    #[test]
    fn preview_lines_align_on_tty_and_stay_whole_when_piped() {
        let long = format!("~/{}/file.py", "d".repeat(200));
        let rows = vec![
            ("~/a.py".to_owned(), "20.0 KiB".to_owned()),
            (long.clone(), "1.5 MiB".to_owned()),
        ];
        let tty = preview_lines(&rows, Some(80));
        assert!(tty.iter().all(|l| l.chars().count() == 80), "{tty:?}");
        assert!(tty[0].ends_with("  20.0 KiB") && tty[1].ends_with("   1.5 MiB"));
        assert!(tty[1].starts_with("...") && tty[1].contains("/file.py "));
        let piped = preview_lines(&rows, None);
        assert_eq!(piped[0], "~/a.py 20.0 KiB");
        assert_eq!(piped[1], format!("{long} 1.5 MiB"));
    }

    #[test]
    fn preview_hint_after_inaccessible_roots() {
        let ok = preview_hint(2, 2048, 0, " -c x");
        assert!(ok.starts_with("Nothing changed. Run 'auto-compress-apfs compress --apply -c x'"));
        assert!(ok.contains("these 2 files (2.0 KiB)"), "{ok}");
        let none = preview_hint(0, 0, 1, "");
        assert_eq!(
            none,
            "Nothing changed. Fix access first; nothing would be compressed there.\n"
        );
        let some = preview_hint(2, 2048, 1, "");
        assert!(
            some.starts_with("Warning: 1 root(s) not accessible"),
            "{some}"
        );
        assert!(some.contains("\nNothing changed. Run "), "{some}");
    }

    #[test]
    fn preview_notes_keep_guard_refusals_apart() {
        let refused = preview_notes(0, 1, None);
        assert!(refused.contains("1 root(s) refused by the volume/protection guard (see WARN"));
        assert!(!refused.contains("see ERROR"), "{refused}");
        assert!(!preview_hint(0, 0, 0, "").contains("Fix access"));
        let both = preview_notes(2, 1, None);
        assert!(
            both.contains("2 root(s) not accessible (see ERROR above)"),
            "{both}"
        );
        assert!(preview_notes(0, 0, None).is_empty());
        let logged = preview_notes(1, 1, Some(Path::new("/l/acz.log")));
        assert!(logged.contains("(see ERROR in /l/acz.log)"), "{logged}");
        assert!(logged.contains("(see WARN in /l/acz.log)"), "{logged}");
        assert!(!logged.contains("above"), "{logged}");
    }

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
        assert!(matches!(parse(&[]), Ok(Cli::Exit(_))), "no arguments: help");
        assert!(!compress(&["compress"]).apply);
        assert!(matches!(
            parse(&["trash", "--apply"]),
            Ok(Cli::Remove(a, remove::Action::Trash)) if a.apply
        ));
        assert!(matches!(
            parse(&["purge", "-v"]),
            Ok(Cli::Remove(a, remove::Action::Purge)) if !a.apply && a.verbose
        ));
        assert!(
            matches!(parse(&["run", "--config=/c.toml"]), Ok(Cli::Run(a)) if a.config.is_some())
        );
        assert!(matches!(
            parse(&["tm-exclude", "--apply", "--config", "/c.toml"]),
            Ok(Cli::TmExclude(a)) if a.apply && a.config.is_some()
        ));
        assert!(matches!(parse(&["tm-exclude"]), Ok(Cli::TmExclude(a)) if !a.apply));
        assert!(matches!(
            parse(&["dedup", "--apply", "-v", "--config", "/c"]),
            Ok(Cli::Dedup(a)) if a.clean.apply && a.verbose && a.clean.config.is_some()
        ));
        assert!(matches!(parse(&["dedup"]), Ok(Cli::Dedup(a)) if !a.clean.apply && !a.verbose));
        assert!(parse(&["dedup", "--dir", "/x"]).is_err());
        assert!(parse(&["tm-exclude", "x"]).is_err());
        assert!(parse(&["trash", "x"]).is_err());
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
        assert_eq!(a.backend, None);
        let a = compress(&["--backend", "applesauce"]);
        assert_eq!(a.backend, Some(config::Backend::Applesauce));
        assert_eq!(a.raw, ["--backend", "applesauce"]);
        assert_eq!(
            compress(&["--backend", "ditto"]).backend,
            Some(config::Backend::Ditto)
        );
        assert!(parse(&["--backend", "lzfse"]).is_err());
        assert!(parse(&["--backend"]).is_err());
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
        let Ok(Cli::Clean(a)) = parse(&["clean"]) else {
            panic!()
        };
        assert!(!a.apply && a.config.is_none());
        let Ok(Cli::Clean(a)) = parse(&["clean", "--config=/c.toml", "--apply"]) else {
            panic!()
        };
        assert!(a.apply);
        assert_eq!(a.config.as_deref(), Some(Path::new("/c.toml")));
        for bad in [
            &["clean", "--dir", "/x"][..],
            &["clean", "x"],
            &["clean", "--config"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
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
