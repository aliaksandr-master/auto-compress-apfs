//! Incompressible-file cache: `dev\tino\tsize\tmtime_sec\tmtime_nsec` per line.

use crate::walk::FileKey;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::Path;

pub const CACHE_NAME: &str = "incompressible.tsv";
/// Next to the cache: the ditto options that filled it, on one line.
pub const ARGS_NAME: &str = "ditto-args.txt";

/// Whether the cache was filled by ditto with options other than `args`, per the record at
/// `path`. Without a record the cache is trusted only for `--hfsCompression` alone: before the
/// record existed, macOS 27 ditto compressed nothing with it and every file was cached.
pub fn filled_with_other_args(path: &Path, args: &[&str]) -> bool {
    match fs::read_to_string(path) {
        Ok(text) => text.trim_end() != args.join(" "),
        Err(e) if e.kind() == io::ErrorKind::NotFound => args != ["--hfsCompression"],
        Err(_) => true,
    }
}

/// Atomically records `args` as the options that filled the cache.
pub fn store_args(path: &Path, args: &[&str]) -> io::Result<()> {
    write_atomic(path, |w| writeln!(w, "{}", args.join(" ")))
}

/// Parses cache text; malformed lines are ignored.
pub fn parse(text: &str) -> HashSet<FileKey> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<FileKey> {
    let mut it = line.split('\t');
    let key = FileKey {
        dev: it.next()?.parse().ok()?,
        ino: it.next()?.parse().ok()?,
        size: it.next()?.parse().ok()?,
        mtime_sec: it.next()?.parse().ok()?,
        mtime_nsec: it.next()?.parse().ok()?,
    };
    it.next().is_none().then_some(key)
}

/// Loads the cache; a missing file is an empty cache.
pub fn load(path: &Path) -> io::Result<HashSet<FileKey>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(parse(&text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(HashSet::new()),
        Err(e) => Err(e),
    }
}

/// Atomically replaces the cache with `keys` (write to a temp file, then rename).
pub fn store<'a>(path: &Path, keys: impl IntoIterator<Item = &'a FileKey>) -> io::Result<()> {
    write_atomic(path, |w| {
        for k in keys {
            writeln!(
                w,
                "{}\t{}\t{}\t{}\t{}",
                k.dev, k.ino, k.size, k.mtime_sec, k.mtime_nsec
            )?;
        }
        Ok(())
    })
}

/// Atomically replaces `path`: `fill` streams the new content into `<path>.tmp.<pid>`, which is
/// synced to disk and then renamed over `path` (parent dirs are created). On any error the temp
/// file is removed.
pub fn write_atomic(
    path: &Path,
    fill: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>,
) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{}", std::process::id()));
    let written = File::create(&tmp).and_then(|f| {
        let mut w = BufWriter::new(f);
        fill(&mut w)?;
        w.into_inner()
            .map_err(io::IntoInnerError::into_error)?
            .sync_all()
    });
    written
        .and_then(|()| fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_record_decides_whether_the_cache_is_trusted() {
        let dir = std::env::temp_dir().join(format!("acz-cache-args-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(ARGS_NAME);
        // No record (a cache from before it existed): trusted only for `--hfsCompression`.
        assert!(filled_with_other_args(
            &path,
            &["--hfsCompression", "--noclone"]
        ));
        assert!(!filled_with_other_args(&path, &["--hfsCompression"]));
        store_args(&path, &["a", "b"]).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "a b\n");
        assert!(!filled_with_other_args(&path, &["a", "b"]));
        assert!(filled_with_other_args(&path, &["a"]));
        // An unreadable record is not trusted.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(filled_with_other_args(&path, &["--hfsCompression"]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn roundtrip_with_corrupt_lines() {
        let dir = std::env::temp_dir().join(format!("acz-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("state").join(CACHE_NAME);
        assert!(load(&path).unwrap().is_empty());
        let keys = [
            FileKey {
                dev: 1,
                ino: 2,
                size: 3,
                mtime_sec: -4,
                mtime_nsec: 5,
            },
            FileKey {
                dev: u64::MAX,
                ino: 7,
                size: 8,
                mtime_sec: 9,
                mtime_nsec: 999_999_999,
            },
        ];
        store(&path, &keys).unwrap();
        assert_eq!(load(&path).unwrap(), HashSet::from(keys));

        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("garbage\n1\t2\t3\n1\t2\t3\t4\t5\t6\nx\t2\t3\t4\t5\n\n");
        fs::write(&path, text).unwrap();
        assert_eq!(load(&path).unwrap(), HashSet::from(keys));
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "temp file left behind");
        fs::remove_dir_all(&dir).unwrap();
    }
}
