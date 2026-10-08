//! `quarantine.txt` next to the config: files that kept being decompressed again and are never
//! compressed until a human releases them. Lines `<timestamp>\t<absolute path>`; `#` comments
//! and blank lines are kept verbatim on every rewrite. A line holding only an absolute path is
//! accepted too (added by hand).

use crate::cache::write_atomic;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub const QUARANTINE_NAME: &str = "quarantine.txt";

const HEADER: &str = "\
# auto-compress-apfs quarantine: files found decompressed again (rewritten) on several
# consecutive run days. Files listed here are never compressed again.
# Release a file by deleting its line, or run
#   auto-compress-apfs quarantine clear <path>      (or: quarantine clear --all)
# Format: <timestamp><TAB><absolute path>
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// When the file was quarantined (empty for lines added by hand without one).
    pub stamp: String,
    pub path: PathBuf,
}

/// The entry on `line`, `None` for comments, blank and malformed lines.
fn parse_line(line: &[u8]) -> Option<Entry> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.first() == Some(&b'#') {
        return None;
    }
    let (stamp, path) = match line.iter().position(|&b| b == b'\t') {
        Some(i) => (&line[..i], &line[i + 1..]),
        None => (&b""[..], line),
    };
    // Trailing blanks typed after a path by hand are not part of it.
    let path = path.trim_ascii_end();
    (path.first() == Some(&b'/')).then(|| Entry {
        stamp: String::from_utf8_lossy(stamp).into_owned(),
        path: PathBuf::from(OsStr::from_bytes(path)),
    })
}

fn read(file: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(file) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    body.split(|&b| b == b'\n')
        .filter(move |_| !bytes.is_empty())
}

/// Entries in file order; a missing file is an empty quarantine.
pub fn load(file: &Path) -> io::Result<Vec<Entry>> {
    Ok(read(file)?
        .map(|b| lines(&b).filter_map(parse_line).collect())
        .unwrap_or_default())
}

pub fn paths(entries: &[Entry]) -> HashSet<PathBuf> {
    entries.iter().map(|e| e.path.clone()).collect()
}

/// Appends `new` entries not listed yet, re-reading the file at write time so concurrent edits
/// by the user are kept; a new file starts with the explanatory header. Returns the number added.
pub fn append(file: &Path, new: &[Entry]) -> io::Result<usize> {
    let existing = read(file)?;
    let listed: HashSet<PathBuf> = existing
        .as_deref()
        .map(|b| lines(b).filter_map(parse_line).map(|e| e.path).collect())
        .unwrap_or_default();
    let mut added = HashSet::new();
    let fresh: Vec<&Entry> = new
        .iter()
        .filter(|e| !listed.contains(&e.path) && added.insert(&e.path))
        .collect();
    if fresh.is_empty() {
        return Ok(0);
    }
    write_atomic(file, |w| {
        match &existing {
            Some(b) => {
                w.write_all(b)?;
                if !b.is_empty() && !b.ends_with(b"\n") {
                    w.write_all(b"\n")?;
                }
            }
            None => w.write_all(HEADER.as_bytes())?,
        }
        for e in &fresh {
            w.write_all(e.stamp.as_bytes())?;
            w.write_all(b"\t")?;
            w.write_all(e.path.as_os_str().as_bytes())?;
            w.write_all(b"\n")?;
        }
        Ok(())
    })?;
    Ok(fresh.len())
}

/// What `quarantine clear` releases.
#[derive(Debug, Clone, Copy)]
pub enum Release<'a> {
    Path(&'a Path),
    All,
}

/// Removes the matching entry lines (comments and other lines stay); returns the released paths.
pub fn clear(file: &Path, what: Release<'_>) -> io::Result<Vec<PathBuf>> {
    let Some(bytes) = read(file)? else {
        return Ok(Vec::new());
    };
    let mut released = Vec::new();
    let mut kept: Vec<&[u8]> = Vec::new();
    for line in lines(&bytes) {
        match parse_line(line) {
            Some(e)
                if matches!(what, Release::All)
                    || matches!(what, Release::Path(p) if p == e.path) =>
            {
                released.push(e.path)
            }
            _ => kept.push(line),
        }
    }
    if !released.is_empty() {
        write_atomic(file, |w| {
            for line in kept {
                w.write_all(line)?;
                w.write_all(b"\n")?;
            }
            Ok(())
        })?;
    }
    Ok(released)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(stamp: &str, path: &str) -> Entry {
        Entry {
            stamp: stamp.into(),
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn roundtrip_preserves_comments_and_user_lines() {
        let dir = std::env::temp_dir().join(format!("acz-quar-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let file = dir.join(QUARANTINE_NAME);
        assert!(load(&file).unwrap().is_empty());
        assert!(clear(&file, Release::All).unwrap().is_empty());

        let a = entry("2026-10-08T09:00:00.000+03:00", "/w/a.py");
        assert_eq!(append(&file, &[a.clone(), a.clone()]).unwrap(), 1);
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.starts_with(HEADER), "{text}");

        // The user adds a comment and a bare path line (no trailing newline).
        let mut text = text;
        text.push_str("# keep this note\n\n/w/by hand.py  ");
        fs::write(&file, &text).unwrap();
        let b = entry("2026-10-09T09:00:00.000+03:00", "/w/b.py");
        assert_eq!(append(&file, &[a.clone(), b.clone()]).unwrap(), 1);
        let got = load(&file).unwrap();
        assert_eq!(got, [a.clone(), entry("", "/w/by hand.py"), b.clone()]);
        let text = fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("# keep this note\n\n/w/by hand.py  \n2026-10-09"),
            "{text}"
        );

        assert_eq!(
            clear(&file, Release::Path(Path::new("/w/a.py"))).unwrap(),
            std::slice::from_ref(&a.path)
        );
        assert!(
            clear(&file, Release::Path(Path::new("/w/none")))
                .unwrap()
                .is_empty()
        );
        let text = fs::read_to_string(&file).unwrap();
        assert!(
            text.starts_with(HEADER) && text.contains("# keep this note"),
            "{text}"
        );
        assert_eq!(paths(&load(&file).unwrap()).len(), 2);
        assert_eq!(clear(&file, Release::All).unwrap().len(), 2);
        assert!(load(&file).unwrap().is_empty());
        assert!(
            fs::read_to_string(&file)
                .unwrap()
                .contains("# keep this note")
        );
        let leftovers: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "temp file left behind");
        fs::remove_dir_all(&dir).unwrap();
    }
}
