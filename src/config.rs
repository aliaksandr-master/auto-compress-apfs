//! TOML configuration: schema, defaults, `~` expansion and validation.

use serde::Deserialize;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
    /// Free space that must remain on the volume after a ditto call's copies.
    pub min_free_bytes: u64,
    pub one_file_system: bool,
    pub threads: usize,
    pub batch_size: usize,
    pub verify: bool,
    pub min_size: u64,
    pub min_age_secs: u64,
    pub log_dir: String,
    pub log_max_bytes: u64,
    pub log_keep: u32,
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
            verify: true,
            min_size: 1,
            min_age_secs: 300,
            log_dir: "~/Library/Logs/auto-compress-apfs".into(),
            log_max_bytes: 4 * MIB,
            log_keep: 1,
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
        {
            *p = expand_tilde(p, home);
        }
        cfg.log_dir = expand_tilde(&cfg.log_dir, home);
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        if let Some(p) = self.patterns.iter().find(|p| !p.starts_with('/')) {
            return Err(format!("pattern `{p}` is not absolute after `~` expansion"));
        }
        validate_excludes(&self.exclude)?;
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
        let parsed = Config::parse(DEFAULT_TOML, &home()).unwrap();
        let empty = Config::parse("", &home()).unwrap();
        assert_eq!(parsed, empty);
        assert!(parsed.patterns.is_empty());
        assert_eq!(&parsed.exclude[..3], ["node_modules", ".git", "target"]);
        assert!(parsed.allow_volumes.is_empty());
        assert_eq!(parsed.quarantine_after, 3);
        assert_eq!(parsed.min_free_bytes, 10 * GIB);
        assert_eq!(parsed.log_dir, "/Users/u/Library/Logs/auto-compress-apfs");
        assert_eq!(parsed.batch_size, 1000);
        assert_eq!(parsed.min_age_secs, 300);
        assert!(parsed.verify && parsed.one_file_system);
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
