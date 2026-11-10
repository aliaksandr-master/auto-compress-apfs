//! End-to-end runs of the binary on temp trees (APFS under `std::env::temp_dir()`) with a
//! temporary `HOME`; `chflags uchg` and mode-000 dirs are undone on drop, also on failure.

use std::collections::HashMap;
use std::fs::{self, Metadata};
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

const UF_COMPRESSED: u32 = 0x20;

fn compressible(seed: &str) -> String {
    format!("{seed}: the quick brown fox jumps over the lazy dog\n").repeat(5000)
}

/// xorshift64 noise: incompressible.
fn random_bytes(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn lstat(p: &Path) -> Metadata {
    fs::symlink_metadata(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn compressed(p: &Path) -> bool {
    lstat(p).st_flags() & UF_COMPRESSED != 0
}

struct Env {
    base: PathBuf,
    home: PathBuf,
    config: PathBuf,
    log: PathBuf,
    venv: PathBuf,
}

/// Undoes `chflags uchg` and mode-000 entries below `dir`, then removes it.
fn force_remove(dir: &Path) {
    if !dir.exists() {
        return;
    }
    let run = |cmd: &str, args: &[&str]| {
        let _ = Command::new(cmd)
            .args(args)
            .arg(dir)
            .stderr(std::process::Stdio::null())
            .status();
    };
    run("/bin/chmod", &["-R", "u+rwx"]);
    run("/usr/bin/chflags", &["-R", "nouchg"]);
    run("/bin/chmod", &["-R", "u+rwx"]);
    let _ = fs::remove_dir_all(dir);
}

fn summary_of(log: &str) -> HashMap<String, String> {
    let line = log
        .lines()
        .rev()
        .find(|l| l.contains(" INFO summary "))
        .expect("summary line");
    line.split(' ')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

impl Env {
    /// A temp tree `acz-it-<tag>-<pid>` with `home/`, and a config holding `body` plus the
    /// test defaults (log dir, no free-space reserve, no minimum age).
    fn new(tag: &str, body: &str) -> Self {
        let base = std::env::temp_dir().join(format!("acz-it-{tag}-{}", std::process::id()));
        force_remove(&base);
        let home = base.join("home");
        fs::create_dir_all(&home).unwrap();
        let log_dir = base.join("logs");
        // The default location: runs without --config are the unrestricted ones.
        let config = home.join(".config/auto-compress-apfs/default.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            &config,
            format!(
                "{body}\nthreads = 4\nmin_age_secs = 0\nmin_free_bytes = 0\nlog_dir = \"{}\"\n",
                log_dir.display()
            ),
        )
        .unwrap();
        Env {
            log: log_dir.join("auto-compress-apfs.log"),
            venv: base.join("tree/Work_x/proj/.venv"),
            base,
            home,
            config,
        }
    }

    /// Runs the binary with `args` (default config under the temp HOME); `day` sets ACZ_TODAY.
    fn exec(&self, args: &[&str], day: Option<&str>) -> std::process::Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_auto-compress-apfs"));
        cmd.args(args)
            .env("HOME", &self.home)
            .env_remove("ACZ_TODAY")
            // install-launchd must never load an agent into the real login session.
            .env("ACZ_SKIP_LAUNCHCTL", "1")
            // Not below `Library/LaunchAgents`: endpoint security flags plists written there.
            .env("ACZ_LAUNCH_AGENTS_DIR", self.base.join("agents"))
            .env("COLUMNS", "400")
            .env("PWD", &self.base)
            .current_dir(&self.base);
        if let Some(d) = day {
            cmd.env("ACZ_TODAY", d);
        }
        cmd.output().unwrap()
    }

    /// Runs and returns the summary line's key/values and stdout.
    fn run_day(&self, args: &[&str], day: Option<&str>) -> (HashMap<String, String>, String) {
        let out = self.exec(args, day);
        assert!(
            out.status.success(),
            "exit {:?}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        (
            summary_of(&self.log_text()),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    fn run(&self, args: &[&str]) -> HashMap<String, String> {
        self.run_day(args, None).0
    }

    fn apply(&self) -> HashMap<String, String> {
        self.run(&["compress", "--apply"])
    }

    fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap()
    }

    fn cache(&self) -> PathBuf {
        self.home
            .join("Library/Caches/auto-compress-apfs/incompressible.tsv")
    }

    fn state(&self) -> PathBuf {
        self.home
            .join("Library/Application Support/auto-compress-apfs/state.tsv")
    }

    fn quarantine(&self) -> PathBuf {
        self.config.with_file_name("quarantine.txt")
    }

    /// The streak column of `path`'s row in state.tsv.
    fn streak(&self, path: &Path) -> Option<u32> {
        let text = fs::read_to_string(self.state()).unwrap();
        let prefix = format!("{}\t", path.display());
        let line = text.lines().find(|l| l.starts_with(&prefix))?;
        line.split('\t').nth(6)?.parse().ok()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        force_remove(&self.base);
    }
}

fn setup() -> Env {
    let env = Env::new(
        "main",
        &format!(
            "patterns = [\"{}/tree/Work_*/**/.venv\"]\nexclude = []",
            std::env::temp_dir()
                .join(format!("acz-it-main-{}", std::process::id()))
                .display()
        ),
    );
    let tree = env.base.join("tree");
    let venv = env.venv.clone();
    fs::create_dir_all(venv.join("lib/sub")).unwrap();
    fs::write(venv.join("lib/a.txt"), compressible("a")).unwrap();
    fs::write(venv.join("lib/sub/a.txt"), compressible("sub a")).unwrap();
    let spaced = venv.join("lib/name with spaces.txt");
    fs::write(&spaced, compressible("spaces")).unwrap();
    fs::set_permissions(&spaced, fs::Permissions::from_mode(0o640)).unwrap();
    fs::write(venv.join("lib/random.bin"), random_bytes(256 * 1024)).unwrap();
    fs::write(venv.join("lib/empty.txt"), "").unwrap();
    fs::write(venv.join("lib/hard1.txt"), compressible("hard")).unwrap();
    fs::hard_link(venv.join("lib/hard1.txt"), venv.join("lib/hard2.txt")).unwrap();
    std::os::unix::fs::symlink("a.txt", venv.join("lib/link.txt")).unwrap();
    let secret = venv.join("lib/secret.txt");
    fs::write(&secret, compressible("secret")).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
    // Leftover of a crashed run and a rescue dir the tool must never touch.
    // Empty leftover: deleted. Leftover holding an uncompressed file (possibly a swapped-out
    // original): moved to a rescue dir, never deleted.
    fs::create_dir_all(venv.join(".acz-staging-1-2")).unwrap();
    fs::create_dir_all(venv.join(".acz-staging-1-4")).unwrap();
    fs::write(venv.join(".acz-staging-1-4/orig.txt"), "user data").unwrap();
    fs::create_dir_all(venv.join(".acz-rescue-1-3")).unwrap();
    fs::write(venv.join(".acz-rescue-1-3/kept.txt"), compressible("kept")).unwrap();
    fs::write(tree.join("Work_x/outside.txt"), compressible("outside")).unwrap();
    // R1: a sibling that must never be read (it would WARN with EACCES).
    fs::create_dir_all(tree.join("Other_x")).unwrap();
    fs::set_permissions(tree.join("Other_x"), fs::Permissions::from_mode(0o000)).unwrap();
    env
}

fn staging_left(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return found;
    };
    for e in rd.flatten() {
        let p = e.path();
        if e.file_name().to_string_lossy().starts_with(".acz-staging-") {
            found.push(p.clone());
        }
        if e.file_type().unwrap().is_dir() {
            found.extend(staging_left(&p));
        }
    }
    found
}

#[test]
fn pipeline() {
    let env = setup();
    let lib = env.venv.join("lib");
    let targets = ["a.txt", "sub/a.txt", "name with spaces.txt"].map(|n| lib.join(n));
    let before: Vec<(String, Metadata)> = targets
        .iter()
        .map(|p| (fs::read_to_string(p).unwrap(), lstat(p)))
        .collect();
    let hard_ino = lstat(&lib.join("hard1.txt")).ino();
    let stale = env.venv.join(".acz-staging-1-2");

    // 0. Preview without -v: totals and top roots, no file list.
    let (_, plain) = env.run_day(&["compress"], None);
    assert!(
        plain.contains("would compress: 5 files")
            && !plain.contains("a.txt")
            && plain.contains("Add -v"),
        "{plain}"
    );
    // 1. Preview (old `--dry-run` spelling): classifies, changes nothing.
    let (s, stdout) = env.run_day(&["--dry-run", "-v"], None);
    assert_eq!(s["dry_run"], "true");
    // hard1/hard2 are one inode: deduplicated by (dev, ino).
    assert_eq!(s["found"], "7", "{s:?}");
    assert_eq!(s["to_compress"], "5", "{s:?}");
    assert_eq!(s["compressed"], "0");
    assert_eq!(s["ditto_calls"], "0");
    assert_eq!(s["skipped"], "1");
    assert_eq!(s["too_small"], "1");
    assert!(targets.iter().all(|p| !compressed(p)));
    assert!(stale.exists(), "dry run removed stale staging");
    assert!(!env.cache().exists(), "dry run wrote the cache");
    assert!(!env.state().exists(), "preview wrote the state");
    assert!(!env.quarantine().exists(), "preview wrote the quarantine");
    assert!(env.log_text().contains("hard1.txt: hardlinked, skipped"));
    assert!(
        stdout.contains("SKIP(hardlink): hardlinked, skipped"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "Nothing changed. Run 'auto-compress-apfs compress --apply' to compress these 5 files ("
        ),
        "{stdout}"
    );
    assert!(stdout.contains("to compress these 5 files ("), "{stdout}");
    // Without arguments: the help, and nothing is walked.
    let out = env.exec(&[], None);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("Usage:"));
    // `run` without --apply is the same preview (no trash/purge rule is scheduled).
    let (s, stdout) = env.run_day(&["run"], None);
    let (first, rest) = stdout.split_once('\n').unwrap();
    assert_eq!(
        first,
        concat!("auto-compress-apfs ", env!("CARGO_PKG_VERSION"))
    );
    assert!(rest.starts_with("== compress =="), "{stdout}");
    assert_eq!(
        (s["dry_run"].as_str(), s["to_compress"].as_str()),
        ("true", "5")
    );
    assert!(!env.state().exists());
    let probe_src = env.config.with_file_name("ditto-probe.txt");
    let probe_copy = env.config.with_file_name("ditto-probe.out/ditto-probe.txt");
    assert!(!probe_src.exists(), "preview probed ditto");

    // 2. Real run: ditto is probed next to the config first.
    let s = env.apply();
    assert!(!compressed(&probe_src) && compressed(&probe_copy));
    assert!(
        env.log_text()
            .contains("INFO ditto compresses with: --hfsCompression")
    );
    assert_eq!(s["compressed"], "3", "{s:?}");
    assert_eq!(s["incompressible"], "1", "{s:?}");
    assert_eq!(s["errors"], "1", "{s:?}");
    // Two batches (`a.txt` twice), a single-file retry of the unreadable secret.txt, and one
    // of random.bin: an uncompressed copy from a failed call is not trusted as incompressible.
    assert_eq!(s["ditto_calls"], "4", "{s:?}");
    for (p, (content, m)) in targets.iter().zip(&before) {
        assert!(compressed(p), "{} not compressed", p.display());
        assert_eq!(&fs::read_to_string(p).unwrap(), content);
        let now = lstat(p);
        assert_eq!(now.mode(), m.mode(), "{}", p.display());
        assert_eq!(now.uid(), m.uid());
        assert_eq!(now.gid(), m.gid());
        assert_eq!(
            (now.mtime(), now.mtime_nsec()),
            (m.mtime(), m.mtime_nsec()),
            "{}",
            p.display()
        );
        assert_ne!(now.ino(), m.ino(), "replacement must be a new inode");
    }
    let random = lib.join("random.bin");
    assert!(!compressed(&random));
    assert_eq!(fs::read(&random).unwrap(), random_bytes(256 * 1024));
    let cache = fs::read_to_string(env.cache()).unwrap();
    let random_ino = lstat(&random).ino().to_string();
    assert!(
        cache
            .lines()
            .any(|l| l.split('\t').nth(1) == Some(&random_ino)),
        "{cache}"
    );
    assert!(!compressed(&lib.join("hard1.txt")));
    assert_eq!(lstat(&lib.join("hard2.txt")).ino(), hard_ino);
    assert!(lstat(&lib.join("link.txt")).file_type().is_symlink());
    assert!(!compressed(&env.base.join("tree/Work_x/outside.txt")));
    assert!(!compressed(&env.venv.join(".acz-rescue-1-3/kept.txt")));
    assert!(!stale.exists(), "stale staging not removed");
    assert!(!env.venv.join(".acz-staging-1-4").exists());
    assert_eq!(
        fs::read_to_string(env.venv.join(".acz-rescue-1-4/orig.txt")).unwrap(),
        "user data"
    );
    assert!(staging_left(&env.base.join("tree")).is_empty());
    let log = env.log_text();
    assert!(log.contains(" INFO compressed "), "{log}");
    assert!(log.contains("secret.txt: ditto failed"), "{log}");
    assert!(log.contains("WARN removed stale staging dir"), "{log}");
    assert!(!log.contains("Other_x"), "sibling dir was read: {log}");

    assert!(!compressed(&lib.join("secret.txt")));

    // 3. Re-run on the unchanged tree (minus the failing file): no ditto spawned.
    fs::remove_file(lib.join("secret.txt")).unwrap();
    let s = env.apply();
    assert_eq!(s["compressed"], "0", "{s:?}");
    assert_eq!(s["errors"], "0", "{s:?}");
    assert_eq!(s["ditto_calls"], "0", "{s:?}");
    assert_eq!(s["already_compressed"], "3");
    assert_eq!(s["incompressible"], "1");

    // 4. `stat` lists the compressed files, biggest original first, and writes nothing.
    let state_before = fs::read(env.state()).unwrap();
    let log_before = env.log_text();
    let out = env.exec(&["stat"], None);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("files          3\n"), "{text}");
    let order: Vec<&str> = text
        .lines()
        .filter_map(|l| l.split("  ").last())
        .filter(|p| p.ends_with(".txt"))
        .collect();
    let want = ["name with spaces.txt", "sub/a.txt", "a.txt"].map(|n| lib.join(n));
    assert_eq!(
        order,
        want.iter().map(|p| p.to_str().unwrap()).collect::<Vec<_>>()
    );
    assert_eq!(fs::read(env.state()).unwrap(), state_before);
    assert_eq!(env.log_text(), log_before, "stat wrote the log");

    // 5. An APFS clone of a compressed file stays compressed (uv clones from its cache).
    let clone = env.base.join("clone.txt");
    let cp = Command::new("/bin/cp")
        .arg("-c")
        .arg(lib.join("a.txt"))
        .arg(&clone)
        .status();
    assert!(cp.unwrap().success());
    assert!(compressed(&clone));
    assert_eq!(
        fs::read(&clone).unwrap(),
        fs::read(lib.join("a.txt")).unwrap()
    );
}

/// Rewrites `p` with new, uncompressed content of the same kind.
fn rewrite(p: &Path, round: u32) {
    fs::write(p, compressible(&format!("round {round}"))).unwrap();
    assert!(!compressed(p));
}

/// The cache line of `p` (`dev ino size mtime_sec mtime_nsec`).
fn cache_line(p: &Path) -> String {
    let m = lstat(p);
    format!(
        "{}\t{}\t{}\t{}\t{}\n",
        m.dev(),
        m.ino(),
        m.size(),
        m.mtime(),
        m.mtime_nsec()
    )
}

#[test]
fn cache_filled_with_other_ditto_args_is_dropped() {
    let env = Env::new("cache", "");
    let dir = env.base.join("tree/Work_c/.venv");
    fs::write(
        &env.config,
        fs::read_to_string(&env.config).unwrap().replace(
            "\nthreads",
            &format!("patterns = [\"{}\"]\nexclude = []\nthreads", dir.display()),
        ),
    )
    .unwrap();
    fs::create_dir_all(&dir).unwrap();
    let a = dir.join("a.py");
    fs::write(&a, compressible("a")).unwrap();
    fs::create_dir_all(env.cache().parent().unwrap()).unwrap();
    let record = env.cache().with_file_name("ditto-args.txt");
    // A record of other options: the compressible file cached by them is compressed.
    fs::write(env.cache(), cache_line(&a)).unwrap();
    fs::write(&record, "--other\n").unwrap();
    let s = env.apply();
    assert_eq!(
        (s["compressed"].as_str(), s["incompressible"].as_str()),
        ("1", "0"),
        "{s:?}"
    );
    assert!(env.log_text().contains("dropped 1 entries"));
    let probed = fs::read_to_string(&record).unwrap();
    assert!(probed.starts_with("--hfsCompression"), "{probed}");
    // A record of the probed options: the cache is trusted.
    let b = dir.join("b.py");
    fs::write(&b, compressible("b")).unwrap();
    fs::write(env.cache(), cache_line(&b)).unwrap();
    let s = env.apply();
    assert_eq!(
        (s["compressed"].as_str(), s["incompressible"].as_str()),
        ("0", "1"),
        "{s:?}"
    );
    assert!(!compressed(&b));
    force_remove(&env.base);
}

#[test]
fn install_and_uninstall_launchd() {
    let env = Env::new("launchd", "");
    let plist = env.base.join("agents/local.auto-compress-apfs.plist");
    let script = env.config.with_file_name("run.sh");
    let out = env.exec(&["install-launchd"], None);
    assert!(out.status.success(), "{out:?}");
    // launchctl was not run (ACZ_SKIP_LAUNCHCTL).
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("launchctl skipped") && !stdout.contains("loaded local."));
    let lint = Command::new("/usr/bin/plutil")
        .arg("-lint")
        .arg(&plist)
        .output()
        .unwrap();
    assert!(lint.status.success(), "{lint:?}");
    let p = fs::read_to_string(&plist).unwrap();
    assert!(
        p.contains(&format!("<string>{}</string>", script.display())),
        "{p}"
    );
    assert!(
        p.contains(&format!("{}/launchd.log", env.base.join("logs").display())),
        "{p}"
    );
    assert_eq!(lstat(&script).mode() & 0o777, 0o755);
    let s = fs::read_to_string(&script).unwrap();
    let bin = env!("CARGO_BIN_EXE_auto-compress-apfs");
    assert!(s.contains(&format!("BIN={bin}\n")), "{s}");
    // The default config is not passed: such runs stay unrestricted.
    assert!(s.contains("exec \"$BIN\" run --apply\n"), "{s}");
    // check-launchd: OK after install, REINSTALL for a changed script, INSTALL when removed.
    let check = |env: &Env| {
        let out = env.exec(&["check-launchd"], None);
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    let (ok, text) = check(&env);
    assert!(ok && text.starts_with("OK: "), "{text}");
    fs::write(&script, s.replace("exec ", "")).unwrap();
    let (ok, text) = check(&env);
    assert!(
        !ok && text.starts_with("REINSTALL: ") && text.contains("another version"),
        "{text}"
    );
    fs::write(&script, &s).unwrap();
    // Uninstall neither needs a valid config nor writes one.
    fs::write(&env.config, "not = [valid").unwrap();
    let out = env.exec(&["uninstall-launchd"], None);
    assert!(out.status.success(), "{out:?}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("loaded local."));
    assert!(!plist.exists() && !script.exists());
    assert_eq!(fs::read_to_string(&env.config).unwrap(), "not = [valid");
    let (ok, text) = check(&env);
    assert!(!ok && text.starts_with("INSTALL: "), "{text}");
    // A second uninstall finds nothing and is fine.
    assert!(env.exec(&["uninstall-launchd"], None).status.success());
    assert!(
        !env.exec(&["install-launchd", "--bogus"], None)
            .status
            .success()
    );
    // Both refuse while a run holds the lock.
    let lock = fs::File::create(env.state().with_file_name("run.lock")).unwrap();
    lock.try_lock().unwrap();
    for cmd in ["install-launchd", "uninstall-launchd"] {
        let out = env.exec(&[cmd], None);
        assert!(!out.status.success(), "{cmd}: {out:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("a run is in progress"));
    }
    assert!(!plist.exists());
    drop(lock);
    force_remove(&env.base);
}

#[test]
fn check_launchd_follows_the_installed_config_and_writes_nothing() {
    let env = Env::new("check", "");
    let check = |args: &[&str]| {
        let out = env.exec(args, None);
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    // A fresh HOME: nothing installed, and the check creates nothing.
    let fresh = env.base.join("fresh");
    fs::create_dir_all(&fresh).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_auto-compress-apfs"))
        .arg("check-launchd")
        .env("HOME", &fresh)
        .env("ACZ_SKIP_LAUNCHCTL", "1")
        .env("ACZ_LAUNCH_AGENTS_DIR", fresh.join("agents"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("INSTALL: "));
    assert_eq!(
        fs::read_dir(&fresh).unwrap().count(),
        0,
        "check-launchd wrote into HOME"
    );
    // Installed with another config: a check without --config follows the plist.
    let other = env.base.join("other/c.toml");
    fs::create_dir_all(other.parent().unwrap()).unwrap();
    fs::copy(&env.config, &other).unwrap();
    let other_arg = other.to_string_lossy().into_owned();
    assert!(
        env.exec(&["install-launchd", "--config", &other_arg], None)
            .status
            .success()
    );
    let (code, text) = check(&["check-launchd"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("launchd not asked"), "{text}");
    // A stale script: the fix keeps the installed config.
    let script = other.with_file_name("run.sh");
    let s = fs::read_to_string(&script).unwrap();
    fs::write(&script, s.replace("exec ", "")).unwrap();
    let (code, text) = check(&["check-launchd"]);
    assert_eq!(code, Some(1), "{text}");
    assert!(
        text.contains(&format!("install-launchd --config {other_arg}")),
        "{text}"
    );
    fs::write(&script, &s).unwrap();
    // The config run.sh names is gone: reinstall (install-launchd recreates it).
    fs::rename(&other, other.with_extension("bak")).unwrap();
    let (code, text) = check(&["check-launchd"]);
    assert!(
        code == Some(1) && text.contains("named by run.sh is missing"),
        "{text}"
    );
    // An invalid one: the check fails with exit 2.
    fs::write(&other, "not = [valid").unwrap();
    assert_eq!(check(&["check-launchd"]).0, Some(2));
    force_remove(&env.base);
}

#[test]
fn trash_purge_and_run() {
    let env = Env::new("remove", "");
    // Inside HOME: trash and purge never act outside it (or an allow_volumes mount).
    let tree = env.home.join("junk");
    let dl = tree.join("dl");
    fs::create_dir_all(dl.join("old/sub")).unwrap();
    fs::write(dl.join("old/sub/a.txt"), "a").unwrap();
    fs::write(dl.join("x.dmg"), "x").unwrap();
    fs::write(dl.join("keep.log"), "k").unwrap();
    fs::create_dir_all(tree.join("cache/c1")).unwrap();
    fs::write(tree.join("cache/c1/f"), "f").unwrap();
    let body = fs::read_to_string(&env.config).unwrap();
    let rules = format!(
        "\n[trash]\npatterns = [\"{dl}/*\", \"{home}\"]\nexclude = [\"*.log\"]\nscheduled = true\n\
         \n[purge]\npatterns = [\"{cache}/*\"]\nscheduled = false\n",
        dl = dl.display(),
        home = env.home.display(),
        cache = tree.join("cache").display(),
    );
    fs::write(&env.config, format!("{body}{rules}")).unwrap();
    let trash = env.home.join(".Trash");
    fs::create_dir_all(&trash).unwrap();
    let journal = env.log.with_file_name("removed.tsv");
    // Preview: lists, refuses the home directory itself, changes nothing.
    // Without -v: the totals only, no item list.
    let out = env.exec(&["trash"], None);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        !text.contains("x.dmg") && !text.contains("BLOCKED") && text.contains("would be moved"),
        "{text}"
    );
    assert!(text.contains("Add -v"), "{text}");
    let out = env.exec(&["trash", "-v"], None);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        text.contains("dir ") && text.contains("old") && text.contains("x.dmg"),
        "{text}"
    );
    assert!(
        text.contains("BLOCKED") && !text.contains("keep.log"),
        "{text}"
    );
    assert!(dl.join("x.dmg").exists() && !journal.exists());
    // run --apply: the scheduled trash rule, not the unscheduled purge one, then compress.
    let out = env.exec(&["run", "--apply"], None);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        text.contains("== trash ==") && !text.contains("== purge =="),
        "{text}"
    );
    assert!(text.contains("== compress =="), "{text}");
    // The closing total: what each step did, then the space freed and moved to the Trash.
    let total = &text[text.find("== total ==").expect("total")..];
    assert!(
        total.contains("trash") && total.contains("applied") && total.contains("to the Trash"),
        "{total}"
    );
    // compress has no patterns here: it prints its hint and no total line.
    assert!(
        total.contains("2 items, 8.0 KiB to the Trash")
            && total.contains("freed: about 0 B")
            && !total.contains("compress"),
        "{total}"
    );
    assert!(!dl.join("old").exists() && !dl.join("x.dmg").exists());
    assert!(dl.join("keep.log").exists() && env.home.exists());
    assert!(trash.join("old/sub/a.txt").exists() && trash.join("x.dmg").exists());
    assert!(tree.join("cache/c1").exists());
    let j = fs::read_to_string(&journal).unwrap();
    assert!(j.lines().all(|l| l.ends_with("\tdone")), "{j}");
    // purge --apply deletes for good; the journal has a line per item.
    let out = env.exec(&["purge", "--apply"], None);
    assert!(out.status.success(), "{out:?}");
    assert!(!tree.join("cache/c1").exists());
    let j = fs::read_to_string(&journal).unwrap();
    assert_eq!(
        j.lines().filter(|l| l.contains("\ttrash\t")).count(),
        2,
        "{j}"
    );
    assert_eq!(
        j.lines().filter(|l| l.contains("\tpurge\t")).count(),
        1,
        "{j}"
    );
    force_remove(&env.base);
}

#[test]
fn cargo_clean_keeps_fresh_and_locked_units() {
    let env = Env::new("cargo", "");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(100 * 86400);
    let set_old = |p: &Path| {
        fs::File::open(p)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old).set_accessed(old))
            .unwrap();
    };
    // Two target dirs: `a` free, `b` with its profile lock held (a build in progress).
    let mut paths = Vec::new();
    for name in ["a", "b"] {
        let proj = env.home.join("src").join(name);
        fs::create_dir_all(&proj).unwrap();
        fs::write(
            proj.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\n"),
        )
        .unwrap();
        let t = proj.join("target");
        let p = t.join("debug");
        for f in [
            ".fingerprint/old-0123456789abcdef/lib-old",
            "deps/libold-0123456789abcdef.rlib",
            ".fingerprint/new-fedcba9876543210/lib-new",
            "deps/libnew-fedcba9876543210.rlib",
            "app",
            ".cargo-lock",
        ] {
            fs::create_dir_all(p.join(f).parent().unwrap()).unwrap();
            fs::write(p.join(f), "x").unwrap();
        }
        fs::write(
            t.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n# created by cargo.\n",
        )
        .unwrap();
        for f in [
            ".fingerprint/old-0123456789abcdef/lib-old",
            ".fingerprint/old-0123456789abcdef",
            "deps/libold-0123456789abcdef.rlib",
            "app",
        ] {
            set_old(&p.join(f));
        }
        paths.push(p);
    }
    let held = fs::File::open(paths[1].join(".cargo-lock")).unwrap();
    held.lock().unwrap();
    let body = fs::read_to_string(&env.config).unwrap();
    fs::write(
        &env.config,
        format!(
            "{body}\n[cargo_clean]\nroots = [\"{}/src\"]\nmax_age_secs = 2592000\n",
            env.home.display()
        ),
    )
    .unwrap();
    let journal = env.log.with_file_name("removed.tsv");
    // Preview: lists the old unit of `a`, skips `b`, changes nothing.
    let out = env.exec(&["cargo-clean", "--verbose"], None);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        text.contains("old-0123456789abcdef") && !text.contains("new-fedcba9876543210"),
        "{text}"
    );
    assert!(
        text.contains("SKIPPED") && text.contains("cargo-lock"),
        "{text}"
    );
    assert!(paths[0].join("deps/libold-0123456789abcdef.rlib").exists());
    assert!(!journal.exists());
    // --apply: only the old unit of the unlocked target goes.
    let out = env.exec(&["cargo-clean", "--apply"], None);
    assert!(out.status.success(), "{out:?}");
    let a = &paths[0];
    assert!(!a.join(".fingerprint/old-0123456789abcdef").exists());
    assert!(!a.join("deps/libold-0123456789abcdef.rlib").exists());
    assert!(a.join(".fingerprint/new-fedcba9876543210/lib-new").exists());
    assert!(a.join("deps/libnew-fedcba9876543210.rlib").exists());
    assert!(a.join("app").exists());
    let b = &paths[1];
    assert!(b.join(".fingerprint/old-0123456789abcdef/lib-old").exists());
    assert!(b.join("deps/libold-0123456789abcdef.rlib").exists());
    let j = fs::read_to_string(&journal).unwrap();
    assert_eq!(
        j.lines().filter(|l| l.contains("\tcargo-clean\t")).count(),
        2,
        "{j}"
    );
    drop(held);
    force_remove(&env.base);
}

#[test]
fn tm_exclude_and_run() {
    let env = Env::new("tmx", "");
    let nm = env.home.join("Work_t/proj/node_modules");
    fs::create_dir_all(nm.join("pkg")).unwrap();
    let src = env.home.join("Work_t/proj/src");
    fs::create_dir_all(&src).unwrap();
    // A compress pattern too: its summary must stay the last `summary` line.
    fs::create_dir_all(&env.venv).unwrap();
    fs::write(env.venv.join("a.py"), compressible("a")).unwrap();
    let body = fs::read_to_string(&env.config).unwrap().replace(
        "\nthreads",
        &format!("patterns = [\"{}\"]\nthreads", env.venv.display()),
    );
    let rules = format!(
        "\n[tm_exclude]\npatterns = [\"{}/**/node_modules\"]\nscheduled = true\n",
        env.home.join("Work_t").display()
    );
    fs::write(&env.config, format!("{body}{rules}")).unwrap();
    let xattr = |p: &Path| {
        Command::new("/usr/bin/xattr")
            .args(["-px", "com.apple.metadata:com_apple_backup_excludeItem"])
            .arg(p)
            .output()
            .unwrap()
    };
    // Preview: lists, writes nothing.
    let out = env.exec(&["tm-exclude"], None);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(text.contains("would exclude from Time Machine: "), "{text}");
    assert!(text.contains("node_modules"), "{text}");
    assert!(!xattr(&nm).status.success());
    // run --apply: compress first, then the scheduled tm-exclude.
    let out = env.exec(&["run", "--apply"], None);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    let (c, t) = (
        text.find("== compress ==").unwrap(),
        text.find("== tm-exclude ==").unwrap(),
    );
    assert!(c < t, "{text}");
    let log = env.log_text();
    let line = log
        .lines()
        .rev()
        .find(|l| l.contains("tm_exclude_summary"))
        .unwrap();
    assert!(
        line.contains(" excluded=1 ") && line.contains("dry_run=false"),
        "{line}"
    );
    let hex: String = String::from_utf8_lossy(&xattr(&nm).stdout)
        .split_whitespace()
        .collect();
    assert_eq!(
        hex,
        "62706C69737430305F1011636F6D2E6170706C652E6261636B75706408000000000000010100000000000000010000000000000000000000000000001C"
    );
    assert!(!xattr(&src).status.success());
    // The compress summary is still the one `summary_of` finds.
    assert!(summary_of(&log).contains_key("compressed"), "{log}");
    // A second run finds it already excluded.
    let out = env.exec(&["run", "--apply"], None);
    assert!(out.status.success(), "{out:?}");
    let log = env.log_text();
    let line = log
        .lines()
        .rev()
        .find(|l| l.contains("tm_exclude_summary"))
        .unwrap();
    assert!(
        line.contains(" already=1 ") && line.contains(" excluded=0 "),
        "{line}"
    );
    force_remove(&env.base);
}

#[test]
fn quarantine_after_three_run_days() {
    let env = Env::new("quar", "");
    let venv = env.base.join("tree/Work_q/.venv");
    fs::write(
        &env.config,
        fs::read_to_string(&env.config).unwrap().replace(
            "\nthreads",
            &format!("patterns = [\"{}\"]\nexclude = []\nthreads", venv.display()),
        ),
    )
    .unwrap();
    fs::create_dir_all(&venv).unwrap();
    let hot = venv.join("hot.py");
    let calm = venv.join("calm.py");
    rewrite(&hot, 0);
    fs::write(&calm, compressible("calm")).unwrap();
    let apply = |day: &str| env.run_day(&["compress", "--apply"], Some(day)).0;

    let s = apply("2026-10-01");
    assert_eq!(s["compressed"], "2", "{s:?}");
    // Found decompressed again on D2 (streak 1); a second run on D2 sees it compressed.
    rewrite(&hot, 2);
    assert_eq!(apply("2026-10-02")["compressed"], "1");
    assert_eq!(apply("2026-10-02")["compressed"], "0");
    rewrite(&hot, 3);
    assert_eq!(apply("2026-10-03")["compressed"], "1");
    // Third consecutive run day: quarantined, not compressed.
    rewrite(&hot, 4);
    // quarantine.txt cannot be written (its directory is read-only): the file is still not
    // compressed and its streak stays at the threshold, so the same day's next run retries.
    let qdir = env.config.parent().unwrap().to_path_buf();
    fs::set_permissions(&qdir, fs::Permissions::from_mode(0o555)).unwrap();
    let s = apply("2026-10-04");
    fs::set_permissions(&qdir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(s["requarantined_new"], "1", "{s:?}");
    // The ditto probe could not write next to the config either: it ran in the state dir.
    assert!(env.log_text().contains("probing in the state dir"));
    assert!(compressed(
        &env.state()
            .with_file_name("ditto-probe.out/ditto-probe.txt")
    ));
    assert!(!compressed(&hot) && !env.quarantine().exists());
    assert!(env.log_text().contains("quarantine retried next run"));
    assert_eq!(env.streak(&hot), Some(3));
    let s = apply("2026-10-04");
    assert_eq!(
        (s["compressed"].as_str(), s["requarantined_new"].as_str()),
        ("0", "1"),
        "{s:?}"
    );
    assert!(!compressed(&hot));
    assert_eq!(env.streak(&hot), Some(0));
    let q = fs::read_to_string(env.quarantine()).unwrap();
    assert!(q.starts_with("# auto-compress-apfs quarantine"), "{q}");
    assert!(q.contains(&format!("\t{}\n", hot.display())), "{q}");
    assert!(
        env.log_text()
            .contains("quarantined (release: auto-compress-apfs quarantine clear")
    );
    // Skipped from now on, without a per-file line.
    let lines_before = env.log_text().matches("hot.py").count();
    let s = apply("2026-10-05");
    assert_eq!(
        (s["compressed"].as_str(), s["quarantined"].as_str()),
        ("0", "1"),
        "{s:?}"
    );
    assert_eq!(env.log_text().matches("hot.py").count(), lines_before);
    assert!(!compressed(&hot) && compressed(&calm));

    // `quarantine list` is read-only; `clear` releases it and the next run compresses it.
    let out = env.exec(&["quarantine", "list"], None);
    assert!(String::from_utf8_lossy(&out.stdout).contains(hot.to_str().unwrap()));
    let out = env.exec(&["quarantine", "clear", hot.to_str().unwrap()], None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !fs::read_to_string(env.quarantine())
            .unwrap()
            .contains("hot.py")
    );
    assert!(
        fs::read_to_string(env.quarantine())
            .unwrap()
            .starts_with("# auto-compress-apfs")
    );
    let out = env.exec(&["quarantine", "clear", hot.to_str().unwrap()], None);
    assert!(!out.status.success(), "clearing twice must report the miss");
    let s = apply("2026-10-06");
    assert_eq!(s["compressed"], "1", "{s:?}");
    assert!(compressed(&hot));
}

#[test]
fn protected_folders_excludes_and_flags() {
    let env = Env::new("prot", "");
    let home = &env.home;
    let t = env.base.join("tree/t");
    // TCC: `~/**/.venv` must never read ~/Documents (mode 000 would WARN).
    fs::create_dir_all(home.join("Work/.venv")).unwrap();
    fs::write(home.join("Work/.venv/w.py"), compressible("w")).unwrap();
    fs::create_dir_all(home.join("Documents/p/.venv")).unwrap();
    let doc = home.join("Documents/p/.venv/doc.py");
    fs::write(&doc, compressible("doc")).unwrap();
    fs::set_permissions(home.join("Documents"), fs::Permissions::from_mode(0o000)).unwrap();
    // Exclude beats include (dir `X/**` and brace alternation), section I shape.
    for f in [
        "export/a.txt",
        "export/b.gz",
        "export/sub/c.png",
        "export/sub/d.txt",
        "export/margin/m.txt",
        "export/margins/k.txt",
    ] {
        let p = t.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, compressible(f)).unwrap();
    }
    // uchg: a flagged file and a flagged directory inside a match stay untouched.
    let venv = env.base.join("tree/v/.venv");
    fs::create_dir_all(venv.join("lockdir")).unwrap();
    let locked = venv.join("locked.py");
    let in_dir = venv.join("lockdir/inner.py");
    fs::write(&locked, compressible("locked")).unwrap();
    fs::write(&in_dir, compressible("inner")).unwrap();
    fs::write(venv.join("free.py"), compressible("free")).unwrap();
    for p in [&locked, &venv.join("lockdir")] {
        assert!(
            Command::new("/usr/bin/chflags")
                .arg("uchg")
                .arg(p)
                .status()
                .unwrap()
                .success()
        );
    }
    let ids = |p: &Path| (lstat(p).ino(), fs::read(p).unwrap());
    let locked_before = ids(&locked);
    let inner_before = ids(&in_dir);
    let body = format!(
        "patterns = [\"~/**/.venv\", \"{t}/export/**\", \"{v}\"]\n\
         exclude = [\"{t}/export/margin/**\", \"{t}/**/*.{{gz,png}}\"]",
        t = t.display(),
        v = venv.display()
    );
    fs::write(
        &env.config,
        fs::read_to_string(&env.config).unwrap() + &body,
    )
    .unwrap();

    let s = env.apply();
    assert_eq!(s["errors"], "0", "{s:?}");
    for p in [
        home.join("Work/.venv/w.py"),
        t.join("export/a.txt"),
        t.join("export/sub/d.txt"),
        t.join("export/margins/k.txt"),
        venv.join("free.py"),
    ] {
        assert!(compressed(&p), "{} not compressed", p.display());
    }
    for p in ["export/b.gz", "export/sub/c.png", "export/margin/m.txt"] {
        assert!(!compressed(&t.join(p)), "{p} compressed");
    }
    assert_eq!(ids(&locked), locked_before);
    assert_eq!(ids(&in_dir), inner_before);
    assert!(!compressed(&locked) && !compressed(&in_dir));
    let log = env.log_text();
    assert!(!log.contains(" WARN "), "{log}");
    assert!(!log.contains("Documents"), "{log}");
    assert!(
        log.contains(&format!("{}: skipped, flag uchg set", locked.display())),
        "{log}"
    );
    assert!(
        log.contains(&format!(
            "{}: directory skipped with its contents, flag uchg set",
            venv.join("lockdir").display()
        )),
        "{log}"
    );
    assert_eq!(s["flagged"], "2", "{s:?}");
    fs::set_permissions(home.join("Documents"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!compressed(&doc));
}

#[test]
fn dir_scope_and_relative_search() {
    let env = Env::new("cli", "patterns = [\"/nonexistent-acz/**\"]\nexclude = []");
    let tree = env.base.join("tree");
    for f in ["a/.venv/x.txt", "a/sub/.venv/y.txt", "b/.venv/z.txt"] {
        let p = tree.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, compressible(f)).unwrap();
    }
    // --dir relative to the cwd (the env base), --search relative to --dir.
    let (s, out) = env.run_day(
        &[
            "--ignore-global-search",
            "--dir",
            "tree/a",
            "--search",
            "**/.venv",
            "-v",
        ],
        None,
    );
    assert_eq!(s["to_compress"], "2", "{s:?}");
    assert!(
        out.contains("a/.venv/x.txt") && out.contains("a/sub/.venv/y.txt"),
        "{out}"
    );
    assert!(!out.contains("z.txt"), "{out}");
    assert!(
        out.contains("Run 'auto-compress-apfs compress --apply --ignore-global-search --dir tree/a --search '**/.venv'' to compress"),
        "{out}"
    );
    // --dir inside a match: everything below it.
    let (s, _) = env.run_day(
        &[
            "--dir",
            "tree/b/.venv",
            "--search",
            &format!("{}/**/.venv", tree.display()),
        ],
        None,
    );
    assert_eq!(s["to_compress"], "1", "{s:?}");
    // Nothing was written by the previews.
    assert!(!env.state().exists() && !env.cache().exists() && !env.quarantine().exists());
    assert!(!compressed(&tree.join("a/.venv/x.txt")));
    // No patterns at all: explained, nothing walked.
    let out = env.exec(&["compress", "--ignore-global-search"], None);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .contains("no search patterns: config patterns ignored and no --search given")
    );
}

#[test]
fn narrowed_runs_keep_state_and_unreadable_state_fails_closed() {
    let env = Env::new("narrow", "");
    let venv = env.base.join("tree/Work_n/.venv");
    fs::write(
        &env.config,
        fs::read_to_string(&env.config).unwrap().replace(
            "\nthreads",
            &format!("patterns = [\"{}\"]\nexclude = []\nthreads", venv.display()),
        ),
    )
    .unwrap();
    fs::create_dir_all(&venv).unwrap();
    fs::create_dir_all(env.base.join("tree/data")).unwrap();
    let py = venv.join("a.py");
    fs::write(&py, compressible("py")).unwrap();
    fs::write(env.base.join("tree/data/t.csv"), compressible("csv")).unwrap();
    let s = env.run_day(&["compress", "--apply"], Some("2026-10-01")).0;
    assert_eq!(s["compressed"], "1", "{s:?}");
    assert!(env.streak(&py).is_some());
    // A narrowed run walks the .venv dir without finding a.py: the row must survive, and the
    // run is not a run day.
    let s = env
        .run_day(
            &[
                "compress",
                "--apply",
                "--ignore-global-search",
                "--search",
                "tree/**/*.csv",
            ],
            Some("2026-10-02"),
        )
        .0;
    assert_eq!(s["compressed"], "1", "{s:?}");
    let state = fs::read_to_string(env.state()).unwrap();
    assert!(env.streak(&py).is_some(), "{state}");
    assert!(state.contains("t.csv\t"), "{state}");
    assert!(
        state.starts_with("#v1\tlast_run_day=2026-10-01\t"),
        "{state}"
    );

    // An explicit --config is narrowed too: the rewritten file is compressed again, but its
    // streak, last-found day and the run days stay as they were.
    fs::write(&py, compressible("py again")).unwrap();
    let cfg = env.config.to_str().unwrap().to_owned();
    let s = env
        .run_day(
            &["compress", "--apply", "--config", &cfg],
            Some("2026-10-02"),
        )
        .0;
    assert_eq!(s["compressed"], "1", "{s:?}");
    let state = fs::read_to_string(env.state()).unwrap();
    let row = state.lines().find(|l| l.contains("a.py\t")).unwrap();
    assert!(row.ends_with("\t0\t-"), "{row}");
    assert!(
        state.starts_with("#v1\tlast_run_day=2026-10-01\t"),
        "{state}"
    );

    // A state file that cannot be parsed stops --apply (exit 1) and is left as is.
    fs::write(env.state(), "not a state file\n").unwrap();
    let out = env.exec(&["compress", "--apply"], Some("2026-10-03"));
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unrecognized header"));
    assert_eq!(
        fs::read_to_string(env.state()).unwrap(),
        "not a state file\n"
    );
    // A preview still runs.
    assert!(env.exec(&["compress"], Some("2026-10-03")).status.success());
}

#[test]
fn empty_patterns_print_a_hint_and_touch_nothing() {
    let env = Env::new("empty", "patterns = []\nexclude = []");
    let body = fs::read_to_string(&env.config).unwrap();
    let f = env.base.join("tree/a/.venv/x.txt");
    fs::create_dir_all(f.parent().unwrap()).unwrap();
    fs::write(&f, compressible("x")).unwrap();
    let hint = format!(
        "no search patterns: add `patterns` to {} or pass --search GLOB\n",
        env.config.display()
    );
    let stdout = |out: &std::process::Output| {
        assert!(
            out.status.success(),
            "exit {:?}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    // A missing config is created from the (pattern-less) default and named in the hint.
    fs::remove_file(&env.config).unwrap();
    let out = stdout(&env.exec(&["compress", "--apply"], None));
    assert_eq!(out, format!("created {}\n{hint}", env.config.display()));
    assert!(
        fs::read_to_string(&env.config)
            .unwrap()
            .contains("\npatterns = []\n")
    );
    assert!(
        !env.home.join("Library").exists(),
        "log, lock, state or cache written"
    );
    fs::write(&env.config, &body).unwrap();
    for args in [&["compress", "--apply"][..], &["--dir", "tree/a"]] {
        assert_eq!(stdout(&env.exec(args, None)), hint, "{args:?}");
    }
    assert!(!env.log.exists() && !env.state().exists() && !env.cache().exists());
    assert!(!compressed(&f));
    // state.tsv is neither read nor pruned (an unreadable one would fail an --apply run).
    fs::create_dir_all(env.state().parent().unwrap()).unwrap();
    fs::write(env.state(), "not a state file\n").unwrap();
    assert_eq!(stdout(&env.exec(&["compress", "--apply"], None)), hint);
    assert_eq!(
        fs::read_to_string(env.state()).unwrap(),
        "not a state file\n"
    );
    // --search alone still runs.
    let (s, _) = env.run_day(&["--search", "tree/**/.venv"], None);
    assert_eq!(s["to_compress"], "1", "{s:?}");
    assert!(!compressed(&f));
}

/// Every entry below `dir` with its inode; symlinks are listed, not followed.
fn snapshot(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        out.push((p.clone(), lstat(&p).ino()));
        if e.file_type().unwrap().is_dir() {
            out.extend(snapshot(&p));
        }
    }
    out.sort();
    out
}

/// Sets mtime/atime two days back; APFS lowers the birthtime with them (checked, so a non-APFS
/// temp dir fails here). The ctime cannot be set back.
fn backdate(p: &Path) {
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86400);
    fs::File::open(p)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(past).set_accessed(past))
        .unwrap();
    let past = past
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(lstat(p).st_birthtime() <= past, "{}", p.display());
}

#[test]
fn clean_lists_and_removes_leftovers() {
    let env = Env::new("clean", "");
    let tree = env.base.join("tree");
    let venv = tree.join("v/.venv");
    fs::write(
        &env.config,
        fs::read_to_string(&env.config).unwrap()
            + &format!("patterns = [\"{}\"]\nexclude = []\n", venv.display()),
    )
    .unwrap();
    let put = |p: &Path, data: &[u8]| {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    };
    put(&venv.join("a.txt"), compressible("a").as_bytes());
    // Fresh staging dir holding a copy: possibly a running compress or dedup, never removed.
    let live = venv.join(".acz-staging-1-2");
    put(&live.join("a.txt"), compressible("a").as_bytes());
    // Old empty staging and rescue dirs: removed.
    let old = venv.join(".acz-staging-1-3");
    fs::create_dir_all(&old).unwrap();
    backdate(&old);
    let old_rescue = venv.join(".acz-rescue-1-6");
    fs::create_dir_all(&old_rescue).unwrap();
    backdate(&old_rescue);
    // Rescue dirs holding files: backdated, but their files' ctime is fresh, so they are inside
    // the 6h live guard and kept (removal of such a dir is covered by the unit tests).
    let rescue = venv.join(".acz-rescue-1-4");
    put(&rescue.join("kept.txt"), b"unique user data");
    fs::create_dir_all(rescue.join(".acz-keep")).unwrap();
    let partial = venv.join("sub/.acz-rescue-1-5");
    put(&partial.join(".BC.T_abc"), b"partial");
    for p in [
        rescue.join("kept.txt"),
        rescue.join(".acz-keep"),
        partial.join(".BC.T_abc"),
    ] {
        backdate(&p);
    }
    backdate(&rescue);
    backdate(&partial);
    // Never touched: a symlink with a rescue name, names outside the regex, a leftover outside
    // the patterns, a leftover holding a uchg file.
    let target = env.base.join("target");
    put(&target.join("t.txt"), b"target");
    std::os::unix::fs::symlink(&target, venv.join(".acz-rescue-2-2")).unwrap();
    fs::create_dir_all(venv.join(".acz-rescue-x")).unwrap();
    fs::create_dir_all(venv.join(".acz-staging-1-2-3")).unwrap();
    let outside = tree.join("other/.acz-rescue-3-3");
    fs::create_dir_all(&outside).unwrap();
    let locked = venv.join(".acz-rescue-4-4/locked.txt");
    put(&locked, b"locked");
    assert!(
        Command::new("/usr/bin/chflags")
            .arg("uchg")
            .arg(&locked)
            .status()
            .unwrap()
            .success()
    );
    let before = snapshot(&env.base.join("tree"));

    // 1. Preview: lists, classifies, changes nothing.
    let out = env.exec(&["clean"], None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    for want in [
        &format!("{}   staging, pid 1\n", live.display()),
        &format!("{}   rescue, pid 1\n", rescue.display()),
        "  has live copy  1, ",
        "  no copy found  1, 16 B\n",
        &format!("    {}\n", rescue.join("kept.txt").display()),
        "  ditto partial  1, 7 B\n",
        "  BLOCKED        changed less than 6h ago (last change ",
        "may belong to a running compress or dedup; never removed\n",
        "flag uchg",
        "  created        ",
        "s ago)\n",
        "d 0h ago)\n",
        "found 6 leftover dirs (2 staging, 4 rescue)",
        "Nothing changed. Run 'auto-compress-apfs clean --apply' to delete these 2 dirs",
    ] {
        assert!(stdout.contains(want), "{want:?} missing in:\n{stdout}");
    }
    assert_eq!(snapshot(&env.base.join("tree")), before);
    let log = env.log_text();
    assert!(
        log.contains(
            "clean_summary found=6 removed=0 failed=0 blocked=4 freed_bytes=0 dry_run=true"
        ),
        "{log}"
    );

    // 2. A held run lock: --apply exits 1 and removes nothing.
    let lock_dir = env
        .home
        .join("Library/Application Support/auto-compress-apfs");
    fs::create_dir_all(&lock_dir).unwrap();
    let lock = fs::File::create(lock_dir.join("run.lock")).unwrap();
    lock.try_lock().unwrap();
    let out = env.exec(&["clean", "--apply"], None);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("a run is in progress"));
    assert_eq!(snapshot(&env.base.join("tree")), before);
    // compress --apply keeps exit 0 for a busy lock (launchd) but says so on stderr.
    let out = env.exec(&["compress", "--apply"], None);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stderr).contains("a run is in progress, exiting"));
    assert_eq!(snapshot(&env.base.join("tree")), before);
    drop(lock);

    // 3. --apply: removes the unblocked leftovers, exits 1 for the blocked ones, among them a
    // rescue dir created seconds before.
    let fresh = venv.join(".acz-rescue-5-5/orig.txt");
    put(&fresh, b"only original");
    let out = env.exec(&["clean", "--apply"], None);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    for want in [
        format!(
            "auto-compress-apfs: kept {}: blocked: changed less than 6h ago",
            fresh.parent().unwrap().display()
        ),
        format!("auto-compress-apfs: kept {}: blocked: ", rescue.display()),
        "auto-compress-apfs: exit 1 only because 5 leftover dirs are blocked; nothing failed"
            .into(),
    ] {
        assert!(stderr.contains(&want), "{want:?} missing in:\n{stderr}");
    }
    assert_eq!(fs::read(&fresh).unwrap(), b"only original");
    for gone in [&old, &old_rescue] {
        assert!(!gone.exists(), "{} not removed", gone.display());
    }
    assert_eq!(
        fs::read(rescue.join("kept.txt")).unwrap(),
        b"unique user data"
    );
    assert!(partial.join(".BC.T_abc").exists());
    assert_eq!(
        fs::read(live.join("a.txt")).unwrap(),
        compressible("a").as_bytes()
    );
    assert_eq!(fs::read(&locked).unwrap(), b"locked");
    assert!(
        lstat(&venv.join(".acz-rescue-2-2"))
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(target.join("t.txt")).unwrap(), b"target");
    for kept in [
        venv.join(".acz-rescue-x"),
        venv.join(".acz-staging-1-2-3"),
        outside.clone(),
        venv.join("a.txt"),
    ] {
        assert!(kept.exists(), "{} removed", kept.display());
    }
    let log = env.log_text();
    assert!(
        log.contains(&format!(
            "INFO clean: removed leftover dir {}",
            old_rescue.display()
        )),
        "{log}"
    );
    assert!(
        log.contains("clean_summary found=7 removed=2 failed=0 blocked=5 "),
        "{log}"
    );
    assert!(log.contains(" dry_run=false"), "{log}");
    assert!(!log.contains(" INFO summary "), "{log}");
}

#[test]
fn too_large_for_ditto_is_skipped_and_uncached() {
    // `MAX_SIZE` in src/classify.rs (a bin crate, not importable here).
    const MAX_SIZE: u64 = 512 << 20;
    let env = Env::new("large", "");
    let venv = env.base.join("tree/Work_l/.venv");
    fs::write(
        &env.config,
        fs::read_to_string(&env.config).unwrap()
            + &format!("patterns = [\"{}\"]\nexclude = []\n", venv.display()),
    )
    .unwrap();
    fs::create_dir_all(&venv).unwrap();
    // Sparse: no real disk use.
    let big = venv.join("big.bin");
    fs::File::create(&big)
        .unwrap()
        .set_len(MAX_SIZE + 1)
        .unwrap();
    // A cache entry from earlier runs, when ditto was still tried on it.
    let m = lstat(&big);
    fs::create_dir_all(env.cache().parent().unwrap()).unwrap();
    fs::write(
        env.cache(),
        format!(
            "{}\t{}\t{}\t{}\t{}\n",
            m.dev(),
            m.ino(),
            m.size(),
            m.mtime(),
            m.mtime_nsec()
        ),
    )
    .unwrap();

    // ditto alone (the default routing would give it to applesauce).
    let s = env.run(&["compress", "--apply", "-v", "--backend", "ditto"]);
    assert_eq!(s["too_large"], "1", "{s:?}");
    assert_eq!(s["incompressible"], "0", "{s:?}");
    assert_eq!(s["ditto_calls"], "0", "{s:?}");
    let now = lstat(&big);
    assert!(!compressed(&big));
    assert_eq!((now.ino(), now.size()), (m.ino(), MAX_SIZE + 1));
    let ino = m.ino().to_string();
    let cache = fs::read_to_string(env.cache()).unwrap();
    assert!(
        !cache.lines().any(|l| l.split('\t').nth(1) == Some(&ino)),
        "{cache}"
    );
    assert!(
        env.log_text().contains(&format!(
            "{}: outside every backend size range, skipped",
            big.display()
        )),
        "{}",
        env.log_text()
    );
}

#[test]
fn too_small_file_keeps_its_state_row() {
    let env = Env::new("small", "");
    let venv = env.base.join("tree/Work_s/.venv");
    fs::write(
        &env.config,
        fs::read_to_string(&env.config).unwrap()
            + &format!("patterns = [\"{}\"]\nexclude = []\n", venv.display()),
    )
    .unwrap();
    fs::create_dir_all(&venv).unwrap();
    let file = venv.join("a.txt");
    fs::write(&file, compressible("a")).unwrap();
    let s = env.apply();
    assert_eq!(s["compressed"], "1", "{s:?}");
    assert!(
        env.streak(&file).is_some(),
        "no state row after compressing"
    );

    // Rewritten small in place: its parent is fully read, so the unrestricted run prunes.
    fs::write(&file, "0123456789").unwrap();
    let s = env.apply();
    assert_eq!(s["too_small"], "1", "{s:?}");
    assert!(
        env.streak(&file).is_some(),
        "{}",
        fs::read_to_string(env.state()).unwrap()
    );
}

/// Edge-size files under one match root, run with `backend`. Returns the env, the root and
/// the (name, content) pairs written; `big.bin` (sparse, over 512 MiB) is extra.
fn edge_files(tag: &str, backend: &str) -> (Env, PathBuf, Vec<(String, Vec<u8>)>) {
    let env = Env::new(tag, "");
    let root = env.base.join(format!("tree/Work_{tag}/.venv"));
    fs::write(
        &env.config,
        fs::read_to_string(&env.config).unwrap()
            + &format!(
                "patterns = [\"{}\"]\nexclude = []\nmin_size = 0\nbackend = \"{backend}\"\n",
                root.display()
            ),
    )
    .unwrap();
    fs::create_dir_all(&root).unwrap();
    let text = |n: usize| compressible("edge").into_bytes().repeat(n / 250_000 + 1)[..n].to_vec();
    let files: Vec<(String, Vec<u8>)> = [0, 1, 65_535, 65_536, 65_537, 5 << 20]
        .into_iter()
        .map(|n| (format!("e{n}.txt"), text(n)))
        .chain([("random.bin".to_owned(), random_bytes(256 * 1024))])
        .collect();
    for (name, data) in &files {
        fs::write(root.join(name), data).unwrap();
    }
    let tagged = root.join("e65537.txt");
    assert!(
        Command::new("/usr/bin/xattr")
            .args(["-w", "user.acz", "kept"])
            .arg(&tagged)
            .status()
            .unwrap()
            .success()
    );
    fs::File::create(root.join("big.bin"))
        .unwrap()
        .set_len((512 << 20) + 1)
        .unwrap();
    (env, root, files)
}

/// The edge-size run of `backend`: `compressed_from` is the smallest size it compresses.
fn edge_sizes(backend: &str, compressed_from: usize) {
    let (env, root, files) = edge_files(&format!("edge-{backend}"), backend);
    let before: HashMap<String, Metadata> = files
        .iter()
        .map(|(n, _)| (n.clone(), lstat(&root.join(n))))
        .collect();
    let s = env.run(&["compress", "--apply", "-v"]);
    let want_compressed = files
        .iter()
        .filter(|(n, d)| n.starts_with('e') && d.len() >= compressed_from)
        .count();
    assert_eq!(s["compressed"], want_compressed.to_string(), "{s:?}");
    assert_eq!(s["errors"], "0", "{s:?}");
    assert_eq!(s["too_large"], "1", "{s:?}");
    assert_eq!(
        s["incompressible"],
        (files.len() - want_compressed).to_string(),
        "{s:?}"
    );
    let plain = env.base.join("plain");
    for (name, data) in &files {
        let p = root.join(name);
        let (m, b) = (lstat(&p), &before[name]);
        let should = name.starts_with('e') && data.len() >= compressed_from;
        assert_eq!(compressed(&p), should, "{name}");
        // Kernel read, size and metadata as before.
        assert_eq!(&fs::read(&p).unwrap(), data, "{name}");
        assert_eq!(m.size(), data.len() as u64, "{name}");
        assert_eq!(
            (m.mode(), m.uid(), m.gid(), m.mtime(), m.mtime_nsec()),
            (b.mode(), b.uid(), b.gid(), b.mtime(), b.mtime_nsec()),
            "{name}"
        );
        if backend == "applesauce" {
            assert_eq!(m.created().unwrap(), b.created().unwrap(), "{name}");
        }
        // An uncompressed copy made by ditto is byte-identical (without
        // --nopreserveHFSCompression ditto keeps the copy compressed).
        fs::create_dir_all(&plain).unwrap();
        let mut dest = plain.clone().into_os_string();
        dest.push("/");
        let ok = Command::new("/usr/bin/ditto")
            .args([
                "--nohfsCompression",
                "--nopreserveHFSCompression",
                "--noclone",
            ])
            .arg(&p)
            .arg(&dest)
            .status()
            .unwrap();
        assert!(ok.success(), "{name}");
        let copy = plain.join(name);
        assert!(!compressed(&copy), "{name}");
        assert_eq!(&fs::read(&copy).unwrap(), data, "{name}");
    }
    let tagged = root.join("e65537.txt");
    let out = Command::new("/usr/bin/xattr")
        .args(["-p", "user.acz"])
        .arg(&tagged)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "kept");
    assert!(!compressed(&root.join("big.bin")));
}

#[test]
fn ditto_edge_sizes() {
    // ditto leaves 16 KiB and less uncompressed: expected, not a failure.
    edge_sizes("ditto", 16 * 1024 + 1);
}

#[test]
fn applesauce_edge_sizes() {
    // Below one 4 KiB allocation unit nothing can be saved.
    edge_sizes("applesauce", 2);
}

#[test]
fn switching_backend_drops_the_incompressible_cache() {
    let (env, _, _) = edge_files("switch", "applesauce");
    let s = env.apply();
    assert_eq!(s["incompressible"], "3", "{s:?}");
    let record = env.cache().with_file_name("ditto-args.txt");
    assert_eq!(
        fs::read_to_string(&record).unwrap(),
        "applesauce lzfse 0.4.14 2.0.0 @0..536870912\n"
    );
    // The old top-level `backend`: one INFO line names the single range.
    assert!(
        env.log_text().contains(
            "config sets the top-level `backend`: only applesauce 0..536870912 is compressed"
        ),
        "{}",
        env.log_text()
    );
    // Same backend again: the cache holds.
    let s = env.apply();
    assert_eq!(s["incompressible"], "3", "{s:?}");
    assert!(!env.log_text().contains("dropped"), "{}", env.log_text());
    // ditto for one run: entries of another compressor are re-checked.
    env.run(&["compress", "--apply", "--backend", "ditto"]);
    assert!(
        env.log_text().contains(
            "incompressible cache was filled under another routing or other backend options, dropped 3 entries"
        ),
        "{}",
        env.log_text()
    );
    assert!(
        fs::read_to_string(&record)
            .unwrap()
            .starts_with("--hfsCompression")
    );
}

#[test]
fn applesauce_keeps_birthtime_older_mtime() {
    use std::os::macos::fs::FileTimesExt as _;
    use std::time::{Duration, UNIX_EPOCH};
    let env = Env::new("birth", "");
    let venv = env.base.join("tree/Work_b/.venv");
    fs::write(
        &env.config,
        fs::read_to_string(&env.config).unwrap()
            + &format!(
                "patterns = [\"{}\"]\nexclude = []\nbackend = \"applesauce\"\n",
                venv.display()
            ),
    )
    .unwrap();
    fs::create_dir_all(&venv).unwrap();
    let file = venv.join("old.txt");
    fs::write(&file, compressible("old")).unwrap();
    // mtime in 2001, birthtime in 2005: the commit's mtime would pull the birthtime down.
    let f = fs::File::options().write(true).open(&file).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::from_secs(978_307_200))
        .unwrap();
    f.set_times(fs::FileTimes::new().set_created(UNIX_EPOCH + Duration::from_secs(1_104_537_600)))
        .unwrap();
    drop(f);
    let before = lstat(&file);
    assert!(before.created().unwrap() > before.modified().unwrap());
    let s = env.apply();
    assert_eq!(s["compressed"], "1", "{s:?}");
    let after = lstat(&file);
    assert!(compressed(&file));
    assert_eq!(after.created().unwrap(), before.created().unwrap());
    assert_eq!(after.modified().unwrap(), before.modified().unwrap());
}

/// `[[backend_by_size]]` tables (after every plain key of the config).
fn routing_tables(ranges: &[(&str, u64, Option<u64>)]) -> String {
    ranges
        .iter()
        .map(|(b, min, max)| {
            let max = max.map_or(String::new(), |m| format!("max_size = {m}\n"));
            format!("[[backend_by_size]]\nbackend = \"{b}\"\nmin_size = {min}\n{max}")
        })
        .collect()
}

#[test]
fn size_routing_sends_each_file_to_its_backend() {
    let env = Env::new("routing", "");
    let root = env.base.join("tree/Work_r/.venv");
    // A small test-only boundary: ditto up to 64 KiB, applesauce above it and below 16 KiB
    // (ditto leaves those plain, so a compressed one proves applesauce made it).
    fs::write(
        &env.config,
        fs::read_to_string(&env.config).unwrap()
            + &format!("patterns = [\"{}\"]\nexclude = []\n", root.display())
            + &routing_tables(&[
                ("applesauce", 4096, Some(16384)),
                ("ditto", 16385, Some(65536)),
                ("applesauce", 65537, None),
            ]),
    )
    .unwrap();
    fs::create_dir_all(&root).unwrap();
    let text = |n: usize| compressible("route").into_bytes()[..n].to_vec();
    let files: Vec<(PathBuf, Vec<u8>)> = [10_000, 40_000, 200_000]
        .into_iter()
        .map(|n| (root.join(format!("f{n}.txt")), text(n)))
        .collect();
    for (p, data) in &files {
        fs::write(p, data).unwrap();
    }
    let tiny = root.join("tiny.txt");
    fs::write(&tiny, text(1000)).unwrap();

    let s = env.apply();
    assert_eq!(s["compressed"], "3", "{s:?}");
    assert_eq!(s["too_small"], "1", "{s:?}");
    assert_eq!(s["errors"], "0", "{s:?}");
    // Only the 40 KB file went through ditto.
    assert_eq!(s["ditto_calls"], "1", "{s:?}");
    for (p, data) in &files {
        assert!(compressed(p), "{}", p.display());
        assert_eq!(&fs::read(p).unwrap(), data, "{}", p.display());
    }
    assert!(!compressed(&tiny));
    // Both backends' records fill the cache record, ditto's options first.
    let record = fs::read_to_string(env.cache().with_file_name("ditto-args.txt")).unwrap();
    assert!(record.starts_with("--hfsCompression"), "{record}");
    assert!(
        record.ends_with(" @16385..65536 applesauce lzfse 0.4.14 2.0.0 @4096..16384 @65537..\n"),
        "{record}"
    );
}

#[test]
fn overlapping_size_ranges_refuse_to_start() {
    let env = Env::new("overlap", "");
    let root = env.base.join("tree/Work_o/.venv");
    fs::write(
        &env.config,
        fs::read_to_string(&env.config).unwrap()
            + &format!("patterns = [\"{}\"]\nexclude = []\n", root.display())
            + &routing_tables(&[
                ("ditto", 16385, Some(100_000)),
                ("applesauce", 90_000, None),
            ]),
    )
    .unwrap();
    fs::create_dir_all(&root).unwrap();
    let file = root.join("f.txt");
    fs::write(&file, compressible("overlap")).unwrap();
    let before = lstat(&file);
    // Every command that reads the config refuses it (stat and quarantine do not read it).
    let commands: [&[&str]; 6] = [
        &["compress"],
        &["compress", "--apply"],
        &["clean"],
        &["trash"],
        &["purge"],
        &["run", "--apply"],
    ];
    for args in commands {
        let out = env.exec(args, None);
        assert!(!out.status.success(), "{args:?}: {out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(
                "backend_by_size ranges overlap: ditto 16385..100000 and applesauce 90000..unbounded"
            ),
            "{args:?}: {err}"
        );
    }
    let now = lstat(&file);
    assert_eq!((now.ino(), now.mtime()), (before.ino(), before.mtime()));
    assert!(!compressed(&file));
    assert!(!env.cache().exists() && !env.state().exists());
}

#[test]
fn changing_range_bounds_drops_the_incompressible_cache() {
    let env = Env::new("bounds", "");
    let root = env.base.join("tree/Work_bd/.venv");
    let write_config = |ditto_max: u64| {
        let base = fs::read_to_string(&env.config).unwrap();
        let plain = base.split("[[backend_by_size]]").next().unwrap().to_owned();
        fs::write(
            &env.config,
            plain
                + &routing_tables(&[
                    ("ditto", 16385, Some(ditto_max)),
                    ("applesauce", ditto_max + 1, None),
                ]),
        )
        .unwrap();
    };
    fs::write(
        &env.config,
        fs::read_to_string(&env.config).unwrap()
            + &format!("patterns = [\"{}\"]\nexclude = []\n", root.display()),
    )
    .unwrap();
    write_config(1 << 20);
    fs::create_dir_all(&root).unwrap();
    for i in 0..2 {
        fs::write(root.join(format!("r{i}.bin")), random_bytes(256 * 1024)).unwrap();
    }
    let s = env.apply();
    assert_eq!(s["incompressible"], "2", "{s:?}");
    // Same routing: the cache holds.
    env.apply();
    assert!(!env.log_text().contains("dropped"), "{}", env.log_text());
    // Only a bound moves (same backends): the cache is dropped once.
    write_config(2 << 20);
    env.apply();
    assert!(
        env.log_text()
            .contains("incompressible cache was filled under another routing or other backend options, dropped 2 entries"),
        "{}",
        env.log_text()
    );
    env.apply();
    assert_eq!(
        env.log_text().matches("dropped").count(),
        1,
        "{}",
        env.log_text()
    );
}

/// Path, inode, mtime, mode and bytes of every file below `dir`.
fn file_states(dir: &Path) -> Vec<(PathBuf, u64, i64, i64, u32, Vec<u8>)> {
    snapshot(dir)
        .into_iter()
        .filter(|(p, _)| lstat(p).is_file())
        .map(|(p, ino)| {
            let m = lstat(&p);
            let bytes = fs::read(&p).unwrap();
            (p, ino, m.mtime(), m.mtime_nsec(), m.mode(), bytes)
        })
        .collect()
}

fn dedup_summary(log: &str) -> String {
    log.lines()
        .rev()
        .find(|l| l.contains(" dedup_summary "))
        .expect("dedup_summary line")
        .to_owned()
}

#[test]
fn dedup_preview_run_modes_and_apply() {
    let env = Env::new("dedup", "");
    let dir = env.base.join("tree/dd");
    fs::create_dir_all(dir.join("sub")).unwrap();
    let data = random_bytes(1_200_000);
    let (a1, a2, a3) = (dir.join("a1"), dir.join("a2"), dir.join("sub/a3"));
    for p in [&a1, &a2, &a3] {
        fs::write(p, &data).unwrap();
    }
    // a2 is the oldest: the reference.
    backdate(&a2);
    // Same size, different content.
    let mut other = data.clone();
    other[600_000] ^= 1;
    fs::write(dir.join("b1"), &other).unwrap();
    other[600_001] ^= 1;
    fs::write(dir.join("b2"), &other).unwrap();
    // Identical, but the mode or an xattr differs.
    let m = random_bytes(1_100_000);
    let (m1, m2) = (dir.join("m1"), dir.join("m2"));
    fs::write(&m1, &m).unwrap();
    fs::write(&m2, &m).unwrap();
    fs::set_permissions(&m2, fs::Permissions::from_mode(0o600)).unwrap();
    let x = random_bytes(1_050_000);
    let (x1, x2) = (dir.join("x1"), dir.join("x2"));
    fs::write(&x1, &x).unwrap();
    fs::write(&x2, &x).unwrap();
    let st = Command::new("/usr/bin/xattr")
        .args(["-w", "user.acz", "v"])
        .arg(&x2)
        .status()
        .unwrap();
    assert!(st.success());
    // An uncompressed file's resource fork is user data: compared like any xattr.
    let rf = random_bytes(1_400_000);
    let (r1, r2) = (dir.join("r1"), dir.join("r2"));
    fs::write(&r1, &rf).unwrap();
    fs::write(&r2, &rf).unwrap();
    let fork = |args: &[&str]| {
        Command::new("/usr/bin/xattr")
            .args(args)
            .arg(&r2)
            .output()
            .unwrap()
    };
    assert!(
        fork(&["-w", "com.apple.ResourceFork", "fork"])
            .status
            .success()
    );
    // Hardlinked: never touched, its identical twin has no partner left.
    let h = random_bytes(1_300_000);
    fs::write(dir.join("h1"), &h).unwrap();
    fs::hard_link(dir.join("h1"), dir.join("h2")).unwrap();
    fs::write(dir.join("h3"), &h).unwrap();
    let base = fs::read_to_string(&env.config).unwrap();
    let with = |dedup_run: &str, compress_run: &str| {
        fs::write(
            &env.config,
            format!(
                "{base}\n[compress]\nrun = {compress_run}\n\n[dedup]\npatterns = [\"{}\"]\nrun = {dedup_run}\n",
                dir.display()
            ),
        )
        .unwrap();
    };
    with("false", "false");
    let before = file_states(&dir);
    // Preview: groups listed, nothing changed.
    let out = env.exec(&["dedup"], None);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        text.contains("  keep  ") && text.contains("dd/a2"),
        "{text}"
    );
    assert!(
        text.contains("  clone ") && text.contains("dd/sub/a3"),
        "{text}"
    );
    assert!(
        text.contains("m2: skipped: metadata differs (mode)"),
        "{text}"
    );
    assert!(
        text.contains("x2: skipped: metadata differs (xattr names)"),
        "{text}"
    );
    assert!(
        !text.contains("b1") && !text.contains("h1") && !text.contains("h3"),
        "{text}"
    );
    assert!(
        text.contains("dedup: groups=4 files=2 would_free="),
        "{text}"
    );
    assert!(
        text.contains("dedup --apply") && text.contains("snapshots"),
        "{text}"
    );
    assert_eq!(file_states(&dir), before);
    assert!(dedup_summary(&env.log_text()).contains("dry_run=true"));
    // run --apply with dedup off and compress off: neither step.
    let out = env.exec(&["run", "--apply"], None);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        !text.contains("== dedup ==") && !text.contains("== compress =="),
        "{text}"
    );
    // dry-run-only: a preview even under --apply.
    with("\"dry-run-only\"", "false");
    let out = env.exec(&["run", "--apply"], None);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(text.contains("run step dedup mode=dry-run-only"), "{text}");
    assert!(
        text.contains("== dedup ==") && text.contains("would_free="),
        "{text}"
    );
    assert!(!text.contains("== compress =="), "{text}");
    assert_eq!(file_states(&dir), before);
    let log = env.log_text();
    assert!(
        log.contains(" INFO run step dedup mode=dry-run-only"),
        "{log}"
    );
    assert!(dedup_summary(&log).contains("dry_run=true"));
    // run = true: replaced under run --apply (compress back on: it runs first).
    with("true", "true");
    let out = env.exec(&["run", "--apply"], None);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    let (c, d) = (
        text.find("== compress ==").unwrap(),
        text.find("== dedup ==").unwrap(),
    );
    assert!(c < d, "{text}");
    assert!(
        text.contains("dedup: deduped=2 ") && text.contains(" failed=0"),
        "{text}"
    );
    let summary = dedup_summary(&env.log_text());
    assert!(
        summary.contains(" deduped=2 ") && summary.contains("dry_run=false"),
        "{summary}"
    );
    let after = file_states(&dir);
    assert_eq!(after.len(), before.len());
    for (b, a) in before.iter().zip(&after) {
        // Same path, mtime, mode and bytes everywhere.
        assert_eq!((&b.0, b.2, b.3, b.4, &b.5), (&a.0, a.2, a.3, a.4, &a.5));
        let replaced = a.0 == a1 || a.0 == a3;
        assert_eq!(b.1 != a.1, replaced, "{}", a.0.display());
    }
    assert!(lstat(&a1).nlink() == 1 && lstat(&a3).nlink() == 1);
    assert!(staging_left(&dir).is_empty(), "{:?}", staging_left(&dir));
    let log = env.log_text();
    assert!(
        log.contains(&format!(
            "deduped {} clone_of={}",
            a1.display(),
            a2.display()
        )),
        "{log}"
    );
    // The clones share the reference's clone id now: nothing left to do.
    let out = env.exec(&["dedup"], None);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("dedup: groups=3 files=0 "), "{text}");
    assert!(
        text.contains("r2: skipped: metadata differs (xattr names)"),
        "{text}"
    );
    let out = fork(&["-p", "com.apple.ResourceFork"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "fork");
    assert!(!text.contains("dd/a"), "{text}");
    // `scheduled` and `run` in one section: refused at startup.
    fs::write(
        &env.config,
        format!("{base}\n[trash]\nscheduled = true\nrun = true\n"),
    )
    .unwrap();
    let out = env.exec(&["run"], None);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("give only `run`"),
        "{out:?}"
    );
    fs::write(
        &env.config,
        format!("{base}\n[dedup]\nrun = \"sometimes\"\n"),
    )
    .unwrap();
    let out = env.exec(&["dedup"], None);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(r#"true, false or "dry-run-only""#),
        "{out:?}"
    );
}

/// The key/values of the last dedup_summary line.
fn dedup_kv(log: &str) -> HashMap<String, String> {
    dedup_summary(log)
        .split(' ')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

impl Env {
    fn dedup_state(&self) -> PathBuf {
        self.state().with_file_name("dedup.tsv")
    }

    /// The streak column of `path`'s row in dedup.tsv.
    fn dedup_streak(&self, path: &Path) -> Option<u32> {
        let text = fs::read_to_string(self.dedup_state()).ok()?;
        let prefix = format!("{}\t", path.display());
        let line = text.lines().find(|l| l.starts_with(&prefix))?;
        line.split('\t').nth(6)?.parse().ok()
    }

    /// A config with `[dedup]` over `dir`.
    fn dedup_config(&self, dir: &Path) {
        let base = fs::read_to_string(&self.config).unwrap();
        fs::write(
            &self.config,
            format!("{base}\n[dedup]\npatterns = [\"{}\"]\n", dir.display()),
        )
        .unwrap();
    }
}

/// Replaces `p` by a new file (new inode, so no longer a clone) with the same bytes.
fn replace_by_rename(p: &Path) {
    let bytes = fs::read(p).unwrap();
    let tmp = p.with_file_name("rewrite.tmp");
    fs::write(&tmp, bytes).unwrap();
    fs::rename(&tmp, p).unwrap();
}

#[test]
fn dedup_quarantine_after_three_run_days() {
    let env = Env::new("dedup-q", "quarantine_after = 3");
    let dir = env.base.join("tree/dq");
    fs::create_dir_all(&dir).unwrap();
    env.dedup_config(&dir);
    let data = random_bytes(1_100_000);
    let (a, b) = (dir.join("a"), dir.join("b"));
    fs::write(&a, &data).unwrap();
    fs::write(&b, &data).unwrap();
    backdate(&a);
    let dedup = |args: &[&str], day: &str| {
        let out = env.exec(args, Some(day));
        assert!(out.status.success(), "{args:?} {day}: {out:?}");
        (
            dedup_kv(&env.log_text()),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    let apply = |day: &str| dedup(&["dedup", "--apply"], day);
    let (s, _) = apply("2026-10-01");
    assert_eq!(s["deduped"], "1", "{s:?}");
    assert_eq!(env.dedup_streak(&b), Some(0));
    // Still a clone of `a` on the next run day: nothing to do, the streak stays 0.
    let (s, _) = apply("2026-10-02");
    assert_eq!(
        (s["deduped"].as_str(), s["already_shared"].as_str()),
        ("0", "1"),
        "{s:?}"
    );
    assert_eq!(env.dedup_streak(&b), Some(0));
    // Rewritten (a new inode) on three consecutive run days: deduped on two, then quarantined.
    for (day, streak) in [("2026-10-03", 1), ("2026-10-04", 2)] {
        replace_by_rename(&b);
        let (s, _) = apply(day);
        assert_eq!(s["deduped"], "1", "{day} {s:?}");
        assert_eq!(env.dedup_streak(&b), Some(streak), "{day}");
    }
    replace_by_rename(&b);
    let rewritten = lstat(&b).ino();
    // quarantine.txt cannot be written: not deduped, the streak stays, the next run retries.
    let qdir = env.config.parent().unwrap().to_path_buf();
    fs::set_permissions(&qdir, fs::Permissions::from_mode(0o555)).unwrap();
    let (s, out) = apply("2026-10-05");
    fs::set_permissions(&qdir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        (s["deduped"].as_str(), s["requarantined_new"].as_str()),
        ("0", "1"),
        "{s:?}"
    );
    assert!(out.contains(" requarantined_new=1"), "{out}");
    assert!(!env.quarantine().exists());
    assert!(env.log_text().contains("quarantine retried next run"));
    assert_eq!(env.dedup_streak(&b), Some(3));
    let (s, _) = apply("2026-10-05");
    assert_eq!(
        (
            s["deduped"].as_str(),
            s["requarantined_new"].as_str(),
            s["skipped"].as_str()
        ),
        ("0", "1", "0"),
        "{s:?}"
    );
    assert_eq!(lstat(&b).ino(), rewritten, "deduped while quarantined");
    assert_eq!(env.dedup_streak(&b), Some(0));
    let q = fs::read_to_string(env.quarantine()).unwrap();
    assert!(q.starts_with("# auto-compress-apfs quarantine"), "{q}");
    assert!(q.contains(&format!(" dedup\t{}\n", b.display())), "{q}");
    assert!(
        env.log_text().contains(&format!(
            "dedup: {}: skipped: no longer shares its data with identical files on 3 \
             consecutive run days, quarantined (release: auto-compress-apfs quarantine clear",
            b.display()
        )),
        "{}",
        env.log_text()
    );
    // Listed: skipped silently, counted only as quarantined.
    let lines = env.log_text().lines().count();
    let (s, out) = apply("2026-10-06");
    assert_eq!(
        (
            s["deduped"].as_str(),
            s["quarantined"].as_str(),
            s["requarantined_new"].as_str(),
            s["skipped"].as_str()
        ),
        ("0", "1", "0", "0"),
        "{s:?}"
    );
    assert!(out.contains(" quarantined=1 requarantined_new=0"), "{out}");
    let new_lines: Vec<String> = env
        .log_text()
        .lines()
        .skip(lines)
        .map(String::from)
        .collect();
    assert!(
        !new_lines
            .iter()
            .any(|l| l.contains(&format!("{}:", b.display()))),
        "{new_lines:?}"
    );
    let (_, out) = dedup(&["dedup"], "2026-10-06");
    assert!(out.contains("/dq/b: skipped: quarantined"), "{out}");
    let s = dedup_kv(&env.log_text());
    assert_eq!(
        (s["quarantined"].as_str(), s["skipped"].as_str()),
        ("1", "0"),
        "{s:?}"
    );
    // `quarantine clear` resets the dedup streak too (set by hand here) and releases it.
    let text = fs::read_to_string(env.dedup_state()).unwrap();
    let prefix = format!("{}\t", b.display());
    let edited: String = text
        .lines()
        .map(|l| {
            if l.starts_with(&prefix) {
                let mut f: Vec<&str> = l.split('\t').collect();
                f[6] = "2";
                f.join("\t") + "\n"
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    fs::write(env.dedup_state(), edited).unwrap();
    assert_eq!(env.dedup_streak(&b), Some(2));
    let b_arg = b.display().to_string();
    let out = env.exec(&["quarantine", "clear", &b_arg], None);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(env.dedup_streak(&b), Some(0));
    let (s, _) = apply("2026-10-07");
    assert_eq!(
        (s["deduped"].as_str(), s["quarantined"].as_str()),
        ("1", "0"),
        "{s:?}"
    );
    assert_ne!(lstat(&b).ino(), rewritten);
    assert_eq!(fs::read(&b).unwrap(), data);
}

#[test]
fn dedup_fails_closed_on_unreadable_quarantine() {
    let env = Env::new("dedup-qfail", "");
    let dir = env.base.join("tree/dqf");
    fs::create_dir_all(&dir).unwrap();
    env.dedup_config(&dir);
    let data = random_bytes(1_100_000);
    fs::write(dir.join("a"), &data).unwrap();
    fs::write(dir.join("b"), &data).unwrap();
    backdate(&dir.join("a"));
    fs::write(env.quarantine(), "# nothing\n").unwrap();
    fs::set_permissions(env.quarantine(), fs::Permissions::from_mode(0o000)).unwrap();
    let before = file_states(&dir);
    for args in [&["dedup"][..], &["dedup", "--apply"]] {
        let out = env.exec(args, None);
        assert_eq!(out.status.code(), Some(1), "{args:?} {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("quarantine.txt"),
            "{out:?}"
        );
        assert_eq!(file_states(&dir), before, "{args:?}");
    }
    fs::set_permissions(env.quarantine(), fs::Permissions::from_mode(0o644)).unwrap();
}

#[test]
fn dedup_rewritten_reference_does_not_quarantine_the_target() {
    let env = Env::new("dedup-ref", "quarantine_after = 3");
    let dir = env.base.join("tree/dr");
    fs::create_dir_all(&dir).unwrap();
    env.dedup_config(&dir);
    let data = random_bytes(1_100_000);
    let (a, b) = (dir.join("a"), dir.join("b"));
    fs::write(&a, &data).unwrap();
    fs::write(&b, &data).unwrap();
    backdate(&a);
    let apply = |day: &str| {
        let out = env.exec(&["dedup", "--apply"], Some(day));
        assert!(out.status.success(), "{day}: {out:?}");
        dedup_kv(&env.log_text())
    };
    assert_eq!(apply("2026-10-01")["deduped"], "1");
    for day in ["2026-10-02", "2026-10-03", "2026-10-04"] {
        // The reference is replaced (new inode), keeping an older mtime: it stays reference.
        replace_by_rename(&a);
        backdate(&a);
        let before = lstat(&b).ino();
        let s = apply(day);
        assert_eq!(
            (s["deduped"].as_str(), s["requarantined_new"].as_str()),
            ("1", "0"),
            "{day} {s:?}"
        );
        assert_ne!(lstat(&b).ino(), before, "{day}");
        assert_eq!(env.dedup_streak(&b), Some(0), "{day}");
    }
    assert!(!env.quarantine().exists());
    assert_eq!(fs::read(&b).unwrap(), data);
}

#[test]
fn clean_finds_dedup_leftovers() {
    let env = Env::new("clean-dd", "");
    let (comp, dd) = (env.home.join("comp"), env.home.join("dd"));
    let base = fs::read_to_string(&env.config).unwrap();
    let both = format!(
        "{base}patterns = [\"{}\"]\nexclude = [\".git\"]\n\n[dedup]\npatterns = [\"{}\"]\n\
         exclude = [\"keepme\"]\n",
        comp.display(),
        dd.display()
    );
    fs::write(&env.config, &both).unwrap();
    // Below `keepme`, which only [dedup].exclude names: a union of both scopes' excludes would
    // hide it, the compress scope with its own excludes finds it.
    let c_old = comp.join("a/keepme/.acz-staging-1-5");
    let d_old = dd.join("b/.acz-staging-1-1048576");
    let d_rescue = dd.join("c/.acz-rescue-1-1048577");
    for p in [&c_old, &d_old, &d_rescue] {
        fs::create_dir_all(p).unwrap();
        backdate(p);
    }
    let out = env.exec(&["clean"], None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    for p in [&c_old, &d_old, &d_rescue] {
        assert!(
            stdout.contains(&format!(
                "~/{}   ",
                p.strip_prefix(&env.home).unwrap().display()
            )),
            "{stdout}"
        );
    }
    assert!(
        stdout.contains("found 3 leftover dirs (2 staging, 1 rescue)"),
        "{stdout}"
    );
    assert!(
        env.log_text()
            .contains("clean_summary found=3 removed=0 failed=0 blocked=0 "),
        "{}",
        env.log_text()
    );
    let out = env.exec(&["clean", "--apply"], None);
    assert!(out.status.success(), "{out:?}");
    for p in [&c_old, &d_old, &d_rescue] {
        assert!(!p.exists(), "{}", p.display());
    }
    // Only dedup patterns: clean still runs.
    fs::write(
        &env.config,
        format!("{base}\n[dedup]\npatterns = [\"{}\"]\n", dd.display()),
    )
    .unwrap();
    fs::create_dir_all(&d_old).unwrap();
    backdate(&d_old);
    let out = env.exec(&["clean"], None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        stdout.contains(&format!(
            "~/{}   ",
            d_old.strip_prefix(&env.home).unwrap().display()
        )),
        "{stdout}"
    );
    assert!(stdout.contains("found 1 leftover dirs"), "{stdout}");
    // Neither: the hint names both.
    fs::write(&env.config, &base).unwrap();
    let out = env.exec(&["clean"], None);
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .contains("no search patterns: add `patterns` or `[dedup] patterns` to"),
        "{out:?}"
    );
}
