//! TOML configuration: schema, defaults, `~` expansion and validation.

use serde::Deserialize;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::classify::MAX_SIZE;

/// The embedded default configuration, written out when the config file does not exist.
pub const DEFAULT_TOML: &str = include_str!("../config/default.toml");

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
/// Hard cap of the whole log footprint (current + rotated logs + launchd.log).
const LOG_FOOTPRINT_MAX: u64 = 10 * MIB;
/// launchd.log (a file a service manager may redirect stdout/stderr into) is truncated at
/// startup once it exceeds this size.
pub const LAUNCHD_LOG_MAX: u64 = MIB;

/// Runtime configuration. After [`Config::parse`] every path-like value is `~`-expanded.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub patterns: Vec<String>,
    pub exclude: Vec<String>,
    /// Mount points of further LOCAL apfs volumes that may be walked (the data volume always is).
    pub allow_volumes: Vec<String>,
    /// Consecutive run days a compressed file may be found decompressed again before it is
    /// quarantined; 0 disables the quarantine.
    pub quarantine_after: u32,
    /// Free space that must remain on the volume after a batch's copies.
    pub min_free_bytes: u64,
    pub one_file_system: bool,
    pub threads: usize,
    pub batch_size: usize,
    /// How a compressed copy is checked before (and after) it replaces the original.
    pub verify: VerifyMode,
    /// Old single-backend form: how compressed copies are made. Not allowed together with
    /// `backend_by_size`.
    pub backend: Option<Backend>,
    /// Old single-backend form: the smallest size compressed. Not allowed together with
    /// `backend_by_size`.
    pub min_size: Option<u64>,
    /// Which backend compresses which file sizes (as written; `None` when absent).
    pub backend_by_size: Option<Vec<SizeRange>>,
    /// The routing in effect after [`Config::parse`]: `backend_by_size`; else the single range
    /// `[min_size, MAX_SIZE]` of a top-level `backend`; else [`default_routing`] with ditto
    /// from `min_size` when that is given. Sorted by `min_size`, non-overlapping.
    #[serde(skip)]
    pub routing: Vec<SizeRange>,
    pub min_age_secs: u64,
    pub log_dir: String,
    pub log_max_bytes: u64,
    pub log_keep: u32,
    /// `trash`: matched files and directories are moved to `~/.Trash`.
    pub trash: Rule,
    /// `purge`: matched files and directories are deleted for good.
    pub purge: Rule,
    /// `compress` as a step of `run`.
    pub compress: CompressRule,
    /// `dedup`: identical files are replaced by APFS clones of one of them.
    pub dedup: DedupRule,
    /// `tm-exclude`: matched directories get the Time Machine exclusion attribute.
    pub tm_exclude: Rule,
}

/// `run` of a command section: whether (and how) `run` executes that command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RunMode {
    /// `false`: skipped by `run`.
    #[default]
    Off,
    /// `true`: run, with `--apply` when `run --apply`.
    On,
    /// `"dry-run-only"`: always a preview, even under `run --apply`.
    DryRunOnly,
}

impl<'de> Deserialize<'de> for RunMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = RunMode;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(r#"true, false or "dry-run-only""#)
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<RunMode, E> {
                Ok(if v { RunMode::On } else { RunMode::Off })
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<RunMode, E> {
                match v {
                    "dry-run-only" => Ok(RunMode::DryRunOnly),
                    _ => Err(E::invalid_value(serde::de::Unexpected::Str(v), &self)),
                }
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<RunMode, E> {
                self.visit_str(&v)
            }
        }
        d.deserialize_any(Visitor)
    }
}

/// `verify`: `false`, `true` or `"extended"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VerifyMode {
    /// `false`: no compare of a ditto copy (an applesauce copy is still compared).
    Off,
    /// `true`: the copy's kernel-decompressed bytes are compared with the original.
    Compare,
    /// `"extended"`: the compare also decodes the copy's decmpfs data independently of the
    /// kernel, and after the swap the file at the path is compared with the original again.
    #[default]
    Extended,
}

impl<'de> Deserialize<'de> for VerifyMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = VerifyMode;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(r#"true, false or "extended""#)
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<VerifyMode, E> {
                Ok(if v {
                    VerifyMode::Compare
                } else {
                    VerifyMode::Off
                })
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<VerifyMode, E> {
                match v {
                    "extended" => Ok(VerifyMode::Extended),
                    _ => Err(E::invalid_value(serde::de::Unexpected::Str(v), &self)),
                }
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<VerifyMode, E> {
                self.visit_str(&v)
            }
        }
        d.deserialize_any(Visitor)
    }
}

/// The compressor that writes the staged copies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// `/usr/bin/ditto --hfsCompression` (the system's own compressor).
    #[default]
    Ditto,
    /// In-process LZFSE through the `applesauce-core` decmpfs writer; every copy is
    /// byte-compared before the swap.
    Applesauce,
}

impl Backend {
    /// Parses the config/CLI spelling (`ditto`, `applesauce`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ditto" => Some(Self::Ditto),
            "applesauce" => Some(Self::Applesauce),
            _ => None,
        }
    }
}

/// One `[[backend_by_size]]` entry: files of `min_size..=max_size` bytes go to `backend`;
/// `max_size` absent is unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizeRange {
    pub backend: Backend,
    pub min_size: u64,
    #[serde(default)]
    pub max_size: Option<u64>,
}

impl SizeRange {
    fn contains(&self, size: u64) -> bool {
        size >= self.min_size && self.max_size.is_none_or(|m| size <= m)
    }
}

impl std::fmt::Display for SizeRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.backend {
            Backend::Ditto => "ditto",
            Backend::Applesauce => "applesauce",
        };
        match self.max_size {
            Some(max) => write!(f, "{name} {}..{max}", self.min_size),
            None => write!(f, "{name} {}..unbounded", self.min_size),
        }
    }
}

/// Default lower bound: files below it are never compressed.
pub const DEFAULT_MIN_SIZE: u64 = 16385;

/// The builtin routing: ditto up to [`MAX_SIZE`] (512 MiB), applesauce above it.
pub fn default_routing() -> Vec<SizeRange> {
    vec![
        SizeRange {
            backend: Backend::Ditto,
            min_size: DEFAULT_MIN_SIZE,
            max_size: Some(MAX_SIZE),
        },
        SizeRange {
            backend: Backend::Applesauce,
            min_size: MAX_SIZE + 1,
            max_size: None,
        },
    ]
}

/// The backend whose range holds `size`, if any (`routing` as in [`Config::routing`]).
pub fn backend_for(routing: &[SizeRange], size: u64) -> Option<Backend> {
    routing.iter().find(|r| r.contains(size)).map(|r| r.backend)
}

/// The lowest `min_size` of `routing`: smaller files are too small for every backend.
pub fn lowest_min_size(routing: &[SizeRange]) -> u64 {
    routing.iter().map(|r| r.min_size).min().unwrap_or(0)
}

/// The routing of `--backend`: `backend` alone from the lowest `min_size` of `routing`, up
/// to [`MAX_SIZE`] for ditto (it leaves bigger files uncompressed), unbounded for applesauce.
pub fn forced_routing(routing: &[SizeRange], backend: Backend) -> Vec<SizeRange> {
    vec![SizeRange {
        backend,
        min_size: lowest_min_size(routing),
        max_size: match backend {
            Backend::Ditto => Some(MAX_SIZE),
            Backend::Applesauce => None,
        },
    }]
}

/// Checks `ranges` (min <= max, ditto bounded by [`MAX_SIZE`], no overlaps) and returns them
/// sorted by `min_size`.
fn check_routing(ranges: &[SizeRange]) -> Result<Vec<SizeRange>, String> {
    if ranges.is_empty() {
        return Err("`backend_by_size` needs at least one range".into());
    }
    for r in ranges {
        if r.max_size.is_some_and(|m| r.min_size > m) {
            return Err(format!("backend_by_size range {r}: min_size > max_size"));
        }
        if r.backend == Backend::Ditto && r.max_size.is_none_or(|m| m > MAX_SIZE) {
            return Err(format!(
                "backend_by_size range {r}: a ditto range needs max_size <= {MAX_SIZE} (ditto \
                 leaves bigger files uncompressed)"
            ));
        }
    }
    let mut sorted = ranges.to_vec();
    sorted.sort_by_key(|r| r.min_size);
    // Sorted by min_size: any overlap shows between neighbours.
    for w in sorted.windows(2) {
        if w[0].max_size.is_none_or(|m| m >= w[1].min_size) {
            return Err(format!(
                "backend_by_size ranges overlap: {} and {}",
                w[0], w[1]
            ));
        }
    }
    Ok(sorted)
}

/// Patterns and excludes of `trash`, `purge` or `tm_exclude` (same syntax as the compress
/// ones). A matched directory is acted on as a whole; `run` says what `run` does with the rule
/// (the old `scheduled = true|false` is read as `run = true|false`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawRule")]
pub struct Rule {
    pub patterns: Vec<String>,
    pub exclude: Vec<String>,
    pub run: RunMode,
}

/// [`Rule`] as written: `scheduled` and `run` may not both be given.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawRule {
    patterns: Vec<String>,
    exclude: Vec<String>,
    run: Option<RunMode>,
    scheduled: Option<bool>,
}

impl TryFrom<RawRule> for Rule {
    type Error = String;
    fn try_from(r: RawRule) -> Result<Self, String> {
        let run = match (r.run, r.scheduled) {
            (Some(_), Some(_)) => {
                return Err("`scheduled` is the old name of `run`: give only `run`".into());
            }
            (Some(run), None) => run,
            (None, Some(true)) => RunMode::On,
            (None, Some(false) | None) => RunMode::Off,
        };
        Ok(Self {
            patterns: r.patterns,
            exclude: r.exclude,
            run,
        })
    }
}

/// `[compress]`: only what `run` does with compress (on by default).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompressRule {
    pub run: RunMode,
}

impl Default for CompressRule {
    fn default() -> Self {
        Self { run: RunMode::On }
    }
}

/// `[dedup]`: files below the patterns of at least `min_size` bytes are compared; identical
/// ones are replaced by APFS clones of one of them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DedupRule {
    pub patterns: Vec<String>,
    pub exclude: Vec<String>,
    pub min_size: u64,
    pub run: RunMode,
}

impl Default for DedupRule {
    fn default() -> Self {
        Self {
            patterns: Vec::new(),
            exclude: [".git", "target", "node_modules"]
                .map(String::from)
                .to_vec(),
            min_size: MIB,
            run: RunMode::Off,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            patterns: Vec::new(),
            exclude: [
                "node_modules",
                ".git",
                "target",
                "*.{xz,gz,tgz,zip,zst,bz2,lz4,lzma,txz,tbz2,tbz,whl,jar,xip,parquet,png,jpg,jpeg,7z,rar,dmg,mp4,mov,webp,heic,pkg}",
                "*.crdownload",
                "*.part",
                "*.download",
            ]
            .map(String::from)
            .to_vec(),
            allow_volumes: Vec::new(),
            quarantine_after: 3,
            min_free_bytes: 10 * GIB,
            one_file_system: true,
            threads: 0,
            batch_size: 1000,
            verify: VerifyMode::default(),
            backend: None,
            min_size: None,
            backend_by_size: None,
            routing: Vec::new(),
            min_age_secs: 300,
            log_dir: "~/Library/Logs/auto-compress-apfs".into(),
            log_max_bytes: 4 * MIB,
            log_keep: 1,
            trash: Rule::default(),
            purge: Rule::default(),
            compress: CompressRule::default(),
            dedup: DedupRule::default(),
            tm_exclude: Rule::default(),
        }
    }
}

/// Exclude syntax: a pattern without `/` matches an entry name at any depth; a pattern with `/`
/// is a full-path glob and must be absolute (or start with `**/`).
pub fn validate_excludes(exclude: &[String]) -> Result<(), String> {
    match exclude
        .iter()
        .find(|e| e.contains('/') && !e.starts_with('/') && !e.starts_with("**/"))
    {
        Some(e) => Err(format!(
            "exclude `{e}` contains `/` but is not absolute after `~` expansion"
        )),
        None => Ok(()),
    }
}

/// Expands a leading `~` or `~/` to `home`; anything else (including `~user`) is returned as is.
pub fn expand_tilde(s: &str, home: &Path) -> String {
    if s == "~" {
        home.to_string_lossy().into_owned()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest).to_string_lossy().into_owned()
    } else {
        s.to_owned()
    }
}

impl Config {
    /// Parses TOML text, expands `~` against `home` and validates the result.
    pub fn parse(text: &str, home: &Path) -> Result<Self, String> {
        let mut cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        for p in cfg
            .patterns
            .iter_mut()
            .chain(cfg.exclude.iter_mut())
            .chain(cfg.allow_volumes.iter_mut())
            .chain(cfg.trash.patterns.iter_mut())
            .chain(cfg.trash.exclude.iter_mut())
            .chain(cfg.purge.patterns.iter_mut())
            .chain(cfg.purge.exclude.iter_mut())
            .chain(cfg.dedup.patterns.iter_mut())
            .chain(cfg.dedup.exclude.iter_mut())
            .chain(cfg.tm_exclude.patterns.iter_mut())
            .chain(cfg.tm_exclude.exclude.iter_mut())
        {
            *p = expand_tilde(p, home);
        }
        cfg.log_dir = expand_tilde(&cfg.log_dir, home);
        cfg.validate()?;
        cfg.routing = match &cfg.backend_by_size {
            Some(_) if cfg.backend.is_some() || cfg.min_size.is_some() => {
                return Err(
                    "`backend_by_size` replaces the top-level `backend` and `min_size`: remove both"
                        .into(),
                );
            }
            Some(ranges) => check_routing(ranges)?,
            // `min_size` alone: ditto's lower bound of the default routing.
            None if cfg.backend.is_none() => {
                let mut routing = default_routing();
                if let Some(min) = cfg.min_size {
                    if min > MAX_SIZE {
                        return Err(format!(
                            "`min_size` {min} is above {MAX_SIZE}, the largest file ditto compresses"
                        ));
                    }
                    routing[0].min_size = min;
                }
                routing
            }
            // An explicit `backend`: that backend alone for [min_size, MAX_SIZE], as before
            // backend_by_size.
            None => {
                let min = cfg.min_size.unwrap_or(DEFAULT_MIN_SIZE);
                if min > MAX_SIZE {
                    return Err(format!(
                        "`min_size` {min} is above {MAX_SIZE}, the top of the single `backend` range"
                    ));
                }
                check_routing(&[SizeRange {
                    backend: cfg.backend.unwrap_or_default(),
                    min_size: min,
                    max_size: Some(MAX_SIZE),
                }])?
            }
        };
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        if let Some(p) = self.patterns.iter().find(|p| !p.starts_with('/')) {
            return Err(format!("pattern `{p}` is not absolute after `~` expansion"));
        }
        validate_excludes(&self.exclude)?;
        for (name, patterns, exclude) in [
            ("trash", &self.trash.patterns, &self.trash.exclude),
            ("purge", &self.purge.patterns, &self.purge.exclude),
            ("dedup", &self.dedup.patterns, &self.dedup.exclude),
            (
                "tm_exclude",
                &self.tm_exclude.patterns,
                &self.tm_exclude.exclude,
            ),
        ] {
            if let Some(p) = patterns.iter().find(|p| !p.starts_with('/')) {
                return Err(format!(
                    "[{name}] pattern `{p}` is not absolute after `~` expansion"
                ));
            }
            validate_excludes(exclude).map_err(|e| format!("[{name}] {e}"))?;
        }
        if self.dedup.min_size < 1 {
            return Err("[dedup] `min_size` must be >= 1".into());
        }
        // An exclusion covers everything inside a matched directory, so an exclude below a
        // directory a pattern names (`.../node_modules/keep`) would look like it protects
        // entries the exclusion still covers. Caught by name: a literal last component of a
        // pattern may not be a non-last component of an exclude (a trailing `/**` dropped from
        // both, as the matcher does). A pattern `X/**` matches the children of `X`, so an
        // exclude may still name a child itself. Known misses: a pattern whose last component
        // is a glob.
        let dir = |s: &str| {
            let s = s.trim_end_matches('/');
            match s.strip_suffix("/**").filter(|x| !x.is_empty()) {
                Some(d) => (d.trim_end_matches('/').to_owned(), 1),
                None => (s.to_owned(), 0),
            }
        };
        let names: Vec<(String, usize)> = self
            .tm_exclude
            .patterns
            .iter()
            .filter_map(|p| {
                let (d, extra) = dir(p);
                Some((d.rsplit('/').next()?.to_owned(), extra))
            })
            .filter(|(n, _)| !n.is_empty() && !n.contains(['*', '?', '[', '{']))
            .collect();
        if let Some((e, n)) = self.tm_exclude.exclude.iter().find_map(|e| {
            let d = dir(e).0;
            let parts: Vec<&str> = d.split('/').collect();
            let last = parts.len().checked_sub(1)?;
            parts.iter().enumerate().find_map(|(i, c)| {
                names
                    .iter()
                    .find(|(n, extra)| n == c && i + extra < last)
                    .map(|(n, _)| (e, n.clone()))
            })
        }) {
            return Err(format!(
                "[tm_exclude] exclude `{e}` lies inside a directory named `{n}`, which a pattern \
                 may exclude (an exclusion covers everything inside a matched directory)"
            ));
        }
        if let Some(v) = self.allow_volumes.iter().find(|v| !v.starts_with('/')) {
            return Err(format!(
                "allow_volumes entry `{v}` is not absolute after `~` expansion"
            ));
        }
        if !self.log_dir.starts_with('/') {
            return Err(format!(
                "log_dir `{}` is not absolute after `~` expansion",
                self.log_dir
            ));
        }
        if self.batch_size < 1 {
            return Err("`batch_size` must be >= 1".into());
        }
        if self.log_max_bytes < 64 * 1024 {
            return Err("`log_max_bytes` must be >= 65536".into());
        }
        if self.log_keep > 9 {
            return Err("`log_keep` must be <= 9".into());
        }
        let footprint = self
            .log_max_bytes
            .checked_mul(u64::from(self.log_keep) + 1)
            .and_then(|b| b.checked_add(LAUNCHD_LOG_MAX));
        if footprint.is_none_or(|b| b > LOG_FOOTPRINT_MAX) {
            return Err("`log_max_bytes * (log_keep + 1) + 1 MiB` must not exceed 10 MiB".into());
        }
        Ok(())
    }

    /// Reads the config at `path`; when the file does not exist it is created from
    /// [`DEFAULT_TOML`] first. Returns the config and whether the file was created.
    pub fn load_or_create(path: &Path, home: &Path) -> Result<(Self, bool), String> {
        let (text, created) = match fs::read_to_string(path) {
            Ok(text) => (text, false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                write_default(path)
                    .map_err(|e| format!("cannot create config {}: {e}", path.display()))?;
                (DEFAULT_TOML.to_owned(), true)
            }
            Err(e) => return Err(format!("cannot read config {}: {e}", path.display())),
        };
        let cfg = Self::parse(&text, home)
            .map_err(|e| format!("invalid config {}: {e}", path.display()))?;
        Ok((cfg, created))
    }

    /// True when the old top-level `backend` alone decides the routing (one range).
    pub fn single_backend(&self) -> bool {
        self.backend.is_some() && self.backend_by_size.is_none()
    }

    /// Worker thread count with `0` resolved to the available parallelism.
    pub fn worker_threads(&self) -> usize {
        match self.threads {
            0 => std::thread::available_parallelism().map_or(4, |n| n.get()),
            n => n,
        }
    }

    pub fn log_dir(&self) -> PathBuf {
        PathBuf::from(&self.log_dir)
    }
}

fn write_default(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // create_new: never clobber a file that appeared concurrently.
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => io::Write::write_all(&mut f, DEFAULT_TOML.as_bytes()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/Users/u")
    }

    #[test]
    fn tilde_expansion() {
        let h = home();
        assert_eq!(expand_tilde("~", &h), "/Users/u");
        assert_eq!(expand_tilde("~/Work_*/x", &h), "/Users/u/Work_*/x");
        assert_eq!(expand_tilde("~other/x", &h), "~other/x");
        assert_eq!(expand_tilde("/abs/~/x", &h), "/abs/~/x");
        assert_eq!(expand_tilde("**/node_modules", &h), "**/node_modules");
    }

    #[test]
    fn embedded_default_equals_builtin_default() {
        let mut parsed = Config::parse(DEFAULT_TOML, &home()).unwrap();
        let empty = Config::parse("", &home()).unwrap();
        // The file spells the builtin routing out; everything else is equal as parsed.
        assert_eq!(parsed.backend_by_size.take(), Some(default_routing()));
        assert_eq!(parsed, empty);
        assert!(parsed.patterns.is_empty());
        assert_eq!(&parsed.exclude[..3], ["node_modules", ".git", "target"]);
        assert!(parsed.allow_volumes.is_empty());
        assert_eq!(parsed.quarantine_after, 3);
        assert_eq!(parsed.min_free_bytes, 10 * GIB);
        assert_eq!(parsed.log_dir, "/Users/u/Library/Logs/auto-compress-apfs");
        assert_eq!(parsed.batch_size, 1000);
        assert_eq!(parsed.min_age_secs, 300);
        assert_eq!(parsed.routing, default_routing());
        assert_eq!(lowest_min_size(&parsed.routing), 16385);
        assert_eq!(parsed.routing[0].max_size, Some(512 * MIB));
        assert_eq!(parsed.routing[1].min_size, 512 * MIB + 1);
        // A user value is honoured as written, not clamped to the default.
        let one = Config::parse("min_size = 1", &home()).unwrap();
        assert_eq!(one.routing[0], range(Backend::Ditto, 1, Some(MAX_SIZE)));
        assert_eq!(parsed.verify, VerifyMode::Extended);
        assert!(parsed.one_file_system);
        assert_eq!((parsed.backend, parsed.min_size), (None, None));
    }

    #[test]
    fn verify_parses_bool_or_extended() {
        let parse = |t: &str| Config::parse(t, &home()).map(|c| c.verify);
        assert_eq!(parse("verify = false").unwrap(), VerifyMode::Off);
        assert_eq!(parse("verify = true").unwrap(), VerifyMode::Compare);
        assert_eq!(
            parse("verify = \"extended\"").unwrap(),
            VerifyMode::Extended
        );
        assert_eq!(parse("").unwrap(), VerifyMode::Extended);
        for bad in ["verify = \"Extended\"", "verify = \"true\"", "verify = 1"] {
            let err = parse(bad).unwrap_err();
            assert!(
                err.contains(r#"expected true, false or "extended""#),
                "{bad}: {err}"
            );
        }
    }

    fn range(backend: Backend, min_size: u64, max_size: Option<u64>) -> SizeRange {
        SizeRange {
            backend,
            min_size,
            max_size,
        }
    }

    #[test]
    fn backend_by_size_parses_sorted() {
        let text = "[[backend_by_size]]\nbackend = \"applesauce\"\nmin_size = 101\n\
                    [[backend_by_size]]\nbackend = \"ditto\"\nmin_size = 10\nmax_size = 100\n";
        let cfg = Config::parse(text, &home()).unwrap();
        assert_eq!(
            cfg.routing,
            [
                range(Backend::Ditto, 10, Some(100)),
                range(Backend::Applesauce, 101, None)
            ]
        );
        // The same backend twice is fine when the ranges do not overlap.
        let text = "[[backend_by_size]]\nbackend = \"applesauce\"\nmin_size = 1\nmax_size = 5\n\
                    [[backend_by_size]]\nbackend = \"applesauce\"\nmin_size = 6\n";
        assert_eq!(Config::parse(text, &home()).unwrap().routing.len(), 2);
    }

    #[test]
    fn backend_by_size_errors() {
        let table = |b: &str, min: u64, max: Option<u64>| {
            let max = max.map_or(String::new(), |m| format!("max_size = {m}\n"));
            format!("[[backend_by_size]]\nbackend = \"{b}\"\nmin_size = {min}\n{max}")
        };
        let err = |text: &str| Config::parse(text, &home()).unwrap_err();
        let e = err(&(table("ditto", 10, Some(100)) + &table("applesauce", 100, None)));
        assert_eq!(
            e,
            "backend_by_size ranges overlap: ditto 10..100 and applesauce 100..unbounded"
        );
        let e = err(&(table("applesauce", 1, None) + &table("applesauce", 50, Some(60))));
        assert!(
            e.contains("overlap: applesauce 1..unbounded and applesauce 50..60"),
            "{e}"
        );
        let e = err(&table("ditto", 10, Some(9)));
        assert!(e.contains("min_size > max_size"), "{e}");
        let e = err(&table("ditto", 10, None));
        assert!(e.contains("ditto range needs max_size"), "{e}");
        let e = err(&table("ditto", 10, Some(MAX_SIZE + 1)));
        assert!(e.contains("ditto range needs max_size"), "{e}");
        assert!(err("backend_by_size = []").contains("at least one range"));
        for old in ["backend = \"ditto\"\n", "min_size = 5\n"] {
            let e = err(&(old.to_owned() + &table("ditto", 10, Some(100))));
            assert!(e.contains("`backend` and `min_size`"), "{e}");
        }
        assert!(err(&(table("ditto", 10, Some(100)) + "bogus = 1\n")).contains("bogus"));
    }

    #[test]
    fn old_keys_and_forced_routing() {
        let cfg = Config::parse("backend = \"applesauce\"", &home()).unwrap();
        assert_eq!(
            cfg.routing,
            [range(Backend::Applesauce, DEFAULT_MIN_SIZE, Some(MAX_SIZE))]
        );
        assert!(cfg.single_backend());
        let cfg = Config::parse("backend = \"ditto\"\nmin_size = 7", &home()).unwrap();
        assert_eq!(cfg.routing, [range(Backend::Ditto, 7, Some(MAX_SIZE))]);
        // `min_size` alone: the default routing with ditto from it.
        let cfg = Config::parse("min_size = 7", &home()).unwrap();
        assert_eq!(
            cfg.routing,
            [
                range(Backend::Ditto, 7, Some(MAX_SIZE)),
                range(Backend::Applesauce, MAX_SIZE + 1, None)
            ]
        );
        assert!(!cfg.single_backend());
        assert!(!Config::parse("", &home()).unwrap().single_backend());
        let big = MAX_SIZE + 1;
        for text in [
            format!("min_size = {big}"),
            format!("backend = \"ditto\"\nmin_size = {big}"),
        ] {
            let e = Config::parse(&text, &home()).unwrap_err();
            assert!(
                e.starts_with(&format!("`min_size` {big} is above {MAX_SIZE}")),
                "{e}"
            );
        }
        let routing = default_routing();
        assert_eq!(
            forced_routing(&routing, Backend::Ditto),
            [range(Backend::Ditto, DEFAULT_MIN_SIZE, Some(MAX_SIZE))]
        );
        assert_eq!(
            forced_routing(&routing, Backend::Applesauce),
            [range(Backend::Applesauce, DEFAULT_MIN_SIZE, None)]
        );
        assert_eq!(backend_for(&routing, DEFAULT_MIN_SIZE - 1), None);
        assert_eq!(
            backend_for(&routing, MAX_SIZE + 1),
            Some(Backend::Applesauce)
        );
    }

    #[test]
    fn backend_values() {
        let cfg = Config::parse("backend = \"applesauce\"", &home()).unwrap();
        assert_eq!(cfg.backend, Some(Backend::Applesauce));
        let cfg = Config::parse("backend = \"ditto\"", &home()).unwrap();
        assert_eq!(cfg.backend, Some(Backend::Ditto));
        assert!(Config::parse("backend = \"Applesauce\"", &home()).is_err());
        assert!(Config::parse("backend = \"zlib\"", &home()).is_err());
        assert_eq!(Backend::parse("applesauce"), Some(Backend::Applesauce));
        assert_eq!(Backend::parse("ditto"), Some(Backend::Ditto));
        assert_eq!(Backend::parse("lzfse"), None);
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = Config::parse("bogus = 1", &home()).unwrap_err();
        assert!(err.contains("bogus"), "{err}");
    }

    #[test]
    fn validation_errors() {
        let bad = [
            "patterns = [\"relative/**\"]",
            "batch_size = 0",
            "log_max_bytes = 1000",
            "log_keep = 10",
            "log_max_bytes = 4194304\nlog_keep = 2",
            "log_dir = \"logs\"",
            "exclude = [\"rel/path/**\"]",
            "allow_volumes = [\"Volumes/Fast\"]",
        ];
        for text in bad {
            assert!(Config::parse(text, &home()).is_err(), "accepted: {text}");
        }
        assert!(
            Config::parse("patterns = []", &home())
                .unwrap()
                .patterns
                .is_empty()
        );
        let ok = "log_max_bytes = 3145728\nlog_keep = 2";
        assert!(Config::parse(ok, &home()).is_ok());
        let ok = "exclude = [\"*.log\", \"**/x\", \"~/a/**\"]\nallow_volumes = [\"/Volumes/Fast\"]";
        let cfg = Config::parse(ok, &home()).unwrap();
        assert_eq!(cfg.exclude, ["*.log", "**/x", "/Users/u/a/**"]);
    }

    #[test]
    fn tm_exclude_section() {
        let cfg = Config::parse("", &home()).unwrap();
        assert_eq!(cfg.tm_exclude, Rule::default());
        let text = "[tm_exclude]\npatterns = [\"~/Work_*/**/target\"]\n\
                    exclude = [\"keep\", \"~/Work_a/x/target\"]\nscheduled = true";
        let cfg = Config::parse(text, &home()).unwrap();
        assert_eq!(cfg.tm_exclude.patterns, ["/Users/u/Work_*/**/target"]);
        assert_eq!(cfg.tm_exclude.exclude, ["keep", "/Users/u/Work_a/x/target"]);
        assert_eq!(cfg.tm_exclude.run, RunMode::On);
        let err = Config::parse("[tm_exclude]\npatterns = [\"rel/**\"]", &home()).unwrap_err();
        assert!(err.contains("[tm_exclude] pattern `rel/**`"), "{err}");
        let pats = "patterns = [\"~/Work_*/**/node_modules\", \"~/Work_*/**/*.tmp\"]";
        for e in [
            "~/Work_*/**/node_modules/keep",
            "/a/node_modules/b/c",
            "**/node_modules/x",
            "/a/node_modules/x/**",
        ] {
            let text = format!("[tm_exclude]\n{pats}\nexclude = [\"{e}\"]");
            let err = Config::parse(&text, &home()).unwrap_err();
            assert!(err.contains("named `node_modules`"), "{err}");
        }
        // A pattern ending in `/**` names the directory before it.
        // A pattern ending in `/**` matches the children, so a child itself may be excluded.
        let cache = |e: &str| {
            let text = format!("[tm_exclude]\npatterns = [\"/a/cache/**\"]\nexclude = [\"{e}\"]");
            Config::parse(&text, &home())
        };
        assert!(cache("/a/cache/sub/keep").is_err());
        assert!(cache("/a/cache/keep").is_ok());
        assert!(cache("/a/cache/keep/**").is_ok());
        // Wildcards anywhere are fine when the exclude is not below a pattern's directory; a
        // pattern whose last component is a glob names no directory to check against.
        let ok = format!(
            "[tm_exclude]\n{pats}\nexclude = [\"~/Work_*/**/*.fixtures\", \"/a/**/b\", \
             \"**/x\", \"/a/b/*.tmp\", \"n*\", \"/a/node_modules\", \"/a/x.tmp/y\", \
             \"/a/node_modules/**\", \"**/node_modules/**\"]"
        );
        assert!(Config::parse(&ok, &home()).is_ok());
    }

    #[test]
    fn run_modes_and_scheduled_alias() {
        let cfg = Config::parse("", &home()).unwrap();
        assert_eq!(cfg.compress.run, RunMode::On);
        for r in [&cfg.trash, &cfg.purge, &cfg.tm_exclude] {
            assert_eq!(r.run, RunMode::Off);
        }
        assert_eq!(cfg.dedup.run, RunMode::Off);
        let parse = |t: &str| Config::parse(t, &home());
        let cfg = parse(
            "[trash]\nrun = true\n[purge]\nrun = \"dry-run-only\"\n[compress]\nrun = false\n\
             [dedup]\nrun = \"dry-run-only\"\n[tm_exclude]\nscheduled = true",
        )
        .unwrap();
        assert_eq!(cfg.trash.run, RunMode::On);
        assert_eq!(cfg.purge.run, RunMode::DryRunOnly);
        assert_eq!(cfg.compress.run, RunMode::Off);
        assert_eq!(cfg.dedup.run, RunMode::DryRunOnly);
        assert_eq!(cfg.tm_exclude.run, RunMode::On);
        assert_eq!(
            parse("[purge]\nscheduled = false").unwrap().purge.run,
            RunMode::Off
        );
        let e = parse("[trash]\nscheduled = true\nrun = true").unwrap_err();
        assert!(e.contains("give only `run`"), "{e}");
        for bad in [
            "[trash]\nrun = \"yes\"",
            "[compress]\nrun = 1",
            "[dedup]\nrun = \"dry-run\"",
        ] {
            let e = parse(bad).unwrap_err();
            assert!(e.contains(r#"true, false or "dry-run-only""#), "{bad}: {e}");
        }
        // `scheduled` is no key of the new sections.
        assert!(parse("[dedup]\nscheduled = true").is_err());
        assert!(parse("[compress]\nscheduled = true").is_err());
    }

    #[test]
    fn dedup_section() {
        let cfg = Config::parse("", &home()).unwrap();
        assert_eq!(cfg.dedup, DedupRule::default());
        assert_eq!(cfg.dedup.exclude, [".git", "target", "node_modules"]);
        assert_eq!(cfg.dedup.min_size, 1048576);
        let cfg = Config::parse(
            "[dedup]\npatterns = [\"~/x/**\"]\nexclude = [\"~/x/keep\"]\nmin_size = 1",
            &home(),
        )
        .unwrap();
        assert_eq!(cfg.dedup.patterns, ["/Users/u/x/**"]);
        assert_eq!(cfg.dedup.exclude, ["/Users/u/x/keep"]);
        for bad in [
            "[dedup]\nmin_size = 0",
            "[dedup]\npatterns = [\"rel/**\"]",
            "[dedup]\nexclude = [\"rel/x\"]",
            "[dedup]\nbogus = 1",
        ] {
            assert!(Config::parse(bad, &home()).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn examples_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        let mut n = 0;
        for e in fs::read_dir(&dir).unwrap() {
            let path = e.unwrap().path();
            if path.extension().is_some_and(|x| x == "toml") {
                let text = fs::read_to_string(&path).unwrap();
                let cfg = Config::parse(&text, &home())
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                assert!(!cfg.patterns.is_empty(), "{}", path.display());
                n += 1;
            }
        }
        assert!(n >= 4, "{n} examples");
    }

    #[test]
    fn load_or_create_writes_default() {
        let dir = std::env::temp_dir().join(format!("acz-cfg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("sub/default.toml");
        let (cfg, created) = Config::load_or_create(&path, &home()).unwrap();
        assert!(created);
        assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_TOML);
        let (again, created) = Config::load_or_create(&path, &home()).unwrap();
        assert!(!created);
        assert_eq!(cfg, again);
        fs::remove_dir_all(&dir).unwrap();
    }
}
