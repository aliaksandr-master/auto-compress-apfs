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

    // 1. Preview (old `--dry-run` spelling): classifies, changes nothing.
    let (s, stdout) = env.run_day(&["--dry-run"], None);
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
    // The default command is the same preview.
    let (s, _) = env.run_day(&[], None);
    assert_eq!(
        (s["dry_run"].as_str(), s["to_compress"].as_str()),
        ("true", "5")
    );
    assert!(!env.state().exists());

    // 2. Real run.
    let s = env.apply();
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
    // Fresh staging dir holding a copy: possibly a running compress, never removed.
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
        "may belong to a running compress; never removed\n",
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
