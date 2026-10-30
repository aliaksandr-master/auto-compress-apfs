//! In-process LZFSE compression (`backend = "applesauce"`): builds a transparently compressed
//! copy of one file in a staging dir with the `applesauce-core` decmpfs writer.
//!
//! The copy is an empty data fork plus the `com.apple.decmpfs` header and, for anything but
//! a small single block, the `com.apple.ResourceFork` blocks; `UF_COMPRESSED` is set last.
//! The original is only read. Committing the copy (byte compare, swap) is the job of
//! [`crate::compress`], exactly as for a ditto copy.

use crate::compress::read_full;
use crate::sys::{self, O_NOFOLLOW, UF_COMPRESSED};
use crate::walk::{Candidate, FileKey};
use applesauce_core::compressor::{Compressor, Kind};
use applesauce_core::writer::Writer;
use applesauce_core::{BLOCK_SIZE, decmpfs, num_blocks, round_to_block_size};
use std::ffi::CStr;
use std::fs::{self, DirBuilder, File, FileTimes, Permissions};
use std::io::{self, BufWriter};
use std::os::macos::fs::{FileTimesExt as _, MetadataExt as _};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Record of the incompressible cache for this backend: the compressor and the exact versions
/// pinned in Cargo.toml (`applesauce-core`, `lzfse-sys`); another version may compress
/// differently, so a change drops the cache.
pub const CACHE_ARGS: [&str; 4] = ["applesauce", "lzfse", "0.4.14", "2.0.0"];
/// Directory inside a staging dir holding the copies being written. A directory, so it can
/// never be a user's original swapped out into staging (only regular files are swapped):
/// stale cleanup deletes it as ours.
pub const PART_DIR: &str = ".acz-part";
/// Allocation unit the saving rule rounds to.
const ALLOC: u64 = 4096;
/// Largest resource fork (block table plus blocks): the writer's block offsets are 32-bit.
const FORK_MAX: u64 = u32::MAX as u64;
/// Write buffer in front of the resource fork xattr (one `fsetxattr` per flush).
const RFORK_BUF: usize = 1 << 20;
/// The xattrs a copy cannot carry over byte for byte, ditto included (checked by the probe
/// test against ditto's own copies):
/// - `com.apple.provenance` is assigned by the kernel to every file this process creates and
///   cannot be written: its presence and value follow the process, not the original;
/// - `com.apple.quarantine` is rewritten by copyfile (new timestamp, agent dropped): its
///   presence and flags field (before the first `;`) must match, the rest need not.
pub const XATTR_NOT_COPIED: &CStr = c"com.apple.provenance";
pub const XATTR_VALUE_REWRITTEN: &CStr = c"com.apple.quarantine";

/// Outcome of [`stage`].
#[derive(Debug, PartialEq, Eq)]
pub enum Staged {
    /// A compressed copy at this path (in the staging dir, named like the original).
    Copy(PathBuf),
    /// Read in full, unchanged, and compression would not save an allocation unit.
    Incompressible,
    /// The original changed or vanished while it was read.
    Changed,
    /// Never compressed: the original already has this compression xattr.
    HasXattr(&'static str),
    /// Never compressed: the name equals [`PART_DIR`] (ignoring case), the staged copy's name
    /// would clash with the part dir.
    ReservedName,
}

/// `st_flags` of the copy: the original's user-settable flags (`UF_SETTABLE`) plus
/// `UF_COMPRESSED`; system flags (`SF_*`) are never carried over.
pub fn copy_flags(orig: u32) -> u32 {
    (orig & 0x0000_ffff) | UF_COMPRESSED
}

/// Reusable per-worker buffers: one source block and its compressed form.
pub struct Buffers {
    src: Vec<u8>,
    dst: Vec<u8>,
    lzfse: Compressor,
}

impl Default for Buffers {
    fn default() -> Self {
        Self {
            src: vec![0; BLOCK_SIZE],
            // `Compressor::compress` needs room for more than the source block.
            dst: vec![0; 2 * BLOCK_SIZE],
            lzfse: Compressor::lzfse(),
        }
    }
}

/// True when compression saves at least one allocation unit: `stored` (resource fork length,
/// or the inline decmpfs xattr bytes) rounded up to 4 KiB is below `size` rounded up.
pub fn saves_space(stored: u64, size: u64) -> bool {
    round_to_block_size(stored, ALLOC) < round_to_block_size(size, ALLOC)
}

/// Bytes the compressed form takes: the decmpfs xattr when the only block fits inline (the
/// writer's own rule), else the resource fork (block table plus blocks).
fn stored_size(size: u64, block_lens: &[u64]) -> u64 {
    let blocks = num_blocks(size);
    let data: u64 = block_lens.iter().sum();
    if blocks <= 1 && data <= decmpfs::MAX_XATTR_DATA_SIZE as u64 {
        decmpfs::HEADER_LEN as u64 + data
    } else {
        Kind::Lzfse.header_size(blocks) + data
    }
}

/// What [`write_copy`] made of the original.
enum Written {
    Compressed,
    Incompressible,
    Changed,
    HasXattr(&'static str),
}

/// Writes a compressed copy of `c` into `staging`. The copy is built at
/// `<staging>/.acz-part/<name>` and renamed to `<staging>/<name>` only when complete; any
/// other outcome removes it. Order (as applesauce's own writer): xattrs (fcopyfile) ->
/// blocks -> saving rule -> decmpfs xattr -> ACL -> mode -> birthtime -> `UF_COMPRESSED` ->
/// fsync. atime/mtime are not set here: the commit sets them on every staged copy, after
/// this, as for a ditto copy, and then restores the birthtime they may have lowered.
pub fn stage(c: &Candidate, staging: &Path, bufs: &mut Buffers) -> io::Result<Staged> {
    stage_with(c, staging, bufs, FORK_MAX, &mut || {})
}

/// [`stage`] with the resource fork limit `fork_max` (tests lower it) and `after_block`
/// called after every block read (tests change the original there).
fn stage_with(
    c: &Candidate,
    staging: &Path,
    bufs: &mut Buffers,
    fork_max: u64,
    after_block: &mut dyn FnMut(),
) -> io::Result<Staged> {
    let name = c.path.file_name().unwrap_or_default();
    if name
        .as_encoded_bytes()
        .eq_ignore_ascii_case(PART_DIR.as_bytes())
    {
        return Ok(Staged::ReservedName);
    }
    let orig = match File::options()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(&c.path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Staged::Changed),
        Err(e) => return Err(e),
    };
    let m = orig.metadata()?;
    if !m.is_file() || FileKey::of(&m) != c.key || m.st_flags() & UF_COMPRESSED != 0 {
        return Ok(Staged::Changed);
    }
    if let Some(name) = compression_xattr(&orig)? {
        return Ok(Staged::HasXattr(name));
    }
    let parts = staging.join(PART_DIR);
    match DirBuilder::new().mode(0o700).create(&parts) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let part = parts.join(name);
    let dst = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(&part)?;
    let r = write_copy(c, &orig, &m, &dst, bufs, fork_max, after_block);
    drop(dst);
    let r = match r {
        Ok(Written::Compressed) => {
            let copy = staging.join(name);
            sys::rename_noreplace(&part, &copy).map(|()| Staged::Copy(copy))
        }
        Ok(Written::Incompressible) => Ok(Staged::Incompressible),
        Ok(Written::Changed) => Ok(Staged::Changed),
        Ok(Written::HasXattr(n)) => Ok(Staged::HasXattr(n)),
        Err(e) => Err(e),
    };
    if !matches!(r, Ok(Staged::Copy(_))) {
        let _ = fs::remove_file(&part);
    }
    r
}

/// The decmpfs or resource fork xattr `f` already carries, if any.
fn compression_xattr(f: &File) -> io::Result<Option<&'static str>> {
    for (name, label) in [
        (sys::RESOURCE_FORK, "com.apple.ResourceFork"),
        (sys::DECMPFS, "com.apple.decmpfs"),
    ] {
        if sys::fxattr_len(f, name, true)?.is_some() {
            return Ok(Some(label));
        }
    }
    Ok(None)
}

/// Fills `dst` (empty, just created) from `orig`, whose metadata `m` matched the walk. A
/// resource fork that would outgrow `fork_max` makes the file incompressible.
fn write_copy(
    c: &Candidate,
    mut orig: &File,
    m: &fs::Metadata,
    dst: &File,
    bufs: &mut Buffers,
    fork_max: u64,
    after_block: &mut dyn FnMut(),
) -> io::Result<Written> {
    let size = c.size();
    sys::fcopyfile(orig, dst, sys::COPYFILE_XATTR)?;
    // Added to the original after the first check: the blocks would land in a foreign fork.
    if let Some(name) = compression_xattr(dst)? {
        return Ok(Written::HasXattr(name));
    }
    let mut writer = Writer::new(Kind::Lzfse, size, || {
        BufWriter::with_capacity(RFORK_BUF, sys::RforkWriter::new(dst))
    })?;
    // Stops early only when the fork cannot be written; otherwise the whole file is read
    // before any verdict.
    let fork_header = Kind::Lzfse.header_size(num_blocks(size));
    let mut stored = 0u64;
    let mut read = 0u64;
    let mut block_lens = Vec::with_capacity(num_blocks(size) as usize);
    loop {
        let n = read_full(&mut orig, &mut bufs.src)?;
        if n == 0 {
            break;
        }
        after_block();
        read += n as u64;
        // Grown: the writer was sized for `size` and must never get an extra block.
        if read > size {
            return Ok(Written::Changed);
        }
        let len = bufs.lzfse.compress(&mut bufs.dst, &bufs.src[..n], 0)?;
        block_lens.push(len as u64);
        stored += len as u64;
        // The writer's 32-bit block offsets cannot address this fork.
        if fork_header + stored > fork_max {
            return Ok(Written::Incompressible);
        }
        writer.add_block(&bufs.dst[..len])?;
    }
    if read != size || FileKey::of(&orig.metadata()?) != c.key {
        return Ok(Written::Changed);
    }
    if !saves_space(stored_size(size, &block_lens), size) {
        return Ok(Written::Incompressible);
    }
    let mut header = Vec::with_capacity(decmpfs::MAX_XATTR_SIZE);
    writer.finish_decmpfs_data(&mut header)?;
    sys::fset_xattr(dst, sys::DECMPFS, &header, 0)?;
    sys::fcopyfile(orig, dst, sys::COPYFILE_ACL)?;
    // The mode as of now, not of the walk: a chmod since then makes the commit's metadata
    // check refuse the swap, as for a ditto copy.
    dst.set_permissions(Permissions::from_mode(m.mode() & 0o7777))?;
    dst.set_times(FileTimes::new().set_created(m.created()?))?;
    sys::fchflags(dst, copy_flags(m.st_flags()))?;
    // After the flag, so the whole final state is on disk; std's sync_all is F_FULLFSYNC on
    // macOS, a cache flush per file: exactness over speed.
    dst.sync_all()?;
    let dm = dst.metadata()?;
    if dm.st_flags() & UF_COMPRESSED == 0 || dm.size() != size {
        return Err(io::Error::other("copy did not become a compressed file"));
    }
    Ok(Written::Compressed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::Arc;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acz-as-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn candidate(path: &Path) -> Candidate {
        let m = fs::symlink_metadata(path).unwrap();
        Candidate::new(path.to_path_buf(), Arc::from(path.parent().unwrap()), &m)
    }

    /// About 4 bits of entropy per byte: compresses, but a 64 KiB block not into the xattr.
    fn mixed(n: usize) -> Vec<u8> {
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                b'@' | (x as u8 & 0x0f)
            })
            .collect()
    }

    fn text(n: usize) -> Vec<u8> {
        b"applesauce determinism: a line that repeats and compresses\n"
            .iter()
            .copied()
            .cycle()
            .take(n)
            .collect()
    }

    fn xattr(path: &Path, name: &CStr) -> Option<Vec<u8>> {
        sys::fget_xattr(&File::open(path).unwrap(), name, true).unwrap()
    }

    #[test]
    fn cache_args_match_the_pinned_versions() {
        let manifest = include_str!("../Cargo.toml");
        let pin = |krate: &str| {
            manifest
                .lines()
                .find(|l| l.starts_with(&format!("{krate} = ")))
                .and_then(|l| l.split("\"=").nth(1))
                .and_then(|v| v.split('"').next())
                .unwrap_or_else(|| panic!("{krate} is not pinned with ="))
                .to_owned()
        };
        assert_eq!(CACHE_ARGS[2], pin("applesauce-core"));
        assert_eq!(CACHE_ARGS[3], pin("lzfse-sys"));
    }

    #[test]
    fn copy_flags_keep_only_user_flags() {
        assert_eq!(copy_flags(0), UF_COMPRESSED);
        // UF_NODUMP and UF_HIDDEN kept, SF_ARCHIVED and SF_DATALESS dropped.
        assert_eq!(
            copy_flags(0x0000_0001 | 0x0000_8000 | 0x0001_0000 | sys::SF_DATALESS),
            0x0000_0001 | 0x0000_8000 | UF_COMPRESSED
        );
    }

    #[test]
    fn fork_over_the_limit_is_incompressible() {
        let dir = temp_dir("forkmax");
        let src = dir.join("f");
        let data = mixed(3 * BLOCK_SIZE);
        fs::write(&src, &data).unwrap();
        let c = candidate(&src);
        let staging = dir.join("s");
        fs::create_dir(&staging).unwrap();
        let mut bufs = Buffers::default();
        // A limit the first block already passes (the real one is 4 GiB).
        let got = stage_with(&c, &staging, &mut bufs, 1024, &mut || {}).unwrap();
        assert_eq!(got, Staged::Incompressible);
        assert_eq!(fs::read_dir(staging.join(PART_DIR)).unwrap().count(), 0);
        assert!(!staging.join("f").exists());
        let m = fs::symlink_metadata(&src).unwrap();
        assert_eq!((m.ino(), m.st_flags() & UF_COMPRESSED), (c.key.ino, 0));
        assert_eq!(fs::read(&src).unwrap(), data);
        // Under the real limit the same file compresses.
        let got = stage_with(&c, &staging, &mut bufs, FORK_MAX, &mut || {}).unwrap();
        assert!(matches!(got, Staged::Copy(_)), "{got:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn original_changed_mid_read_is_not_staged() {
        let dir = temp_dir("grow");
        let mut bufs = Buffers::default();
        for size in [1_000, 3 * BLOCK_SIZE] {
            for grow in [true, false] {
                let src = dir.join(format!("f{size}-{grow}"));
                fs::write(&src, mixed(size)).unwrap();
                let c = candidate(&src);
                let staging = dir.join(format!("s{size}-{grow}"));
                fs::create_dir(&staging).unwrap();
                let mut once = true;
                let mut hook = || {
                    if std::mem::take(&mut once) {
                        let f = File::options().write(true).open(&src).unwrap();
                        if grow {
                            f.set_len(size as u64 + 10).unwrap();
                        } else {
                            f.set_len(size as u64 / 2).unwrap();
                        }
                    }
                };
                let got = stage_with(&c, &staging, &mut bufs, FORK_MAX, &mut hook).unwrap();
                assert_eq!(got, Staged::Changed, "{size} grow={grow}");
                assert_eq!(fs::read_dir(staging.join(PART_DIR)).unwrap().count(), 0);
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn part_dir_name_is_never_staged() {
        let dir = temp_dir("reserved");
        for (i, name) in [".acz-part", ".ACZ-Part"].into_iter().enumerate() {
            let src = dir.join(name);
            fs::write(&src, text(100_000)).unwrap();
            let staging = dir.join(format!("s{i}"));
            fs::create_dir(&staging).unwrap();
            let got = stage(&candidate(&src), &staging, &mut Buffers::default()).unwrap();
            assert_eq!(got, Staged::ReservedName);
            assert_eq!(fs::read_dir(&staging).unwrap().count(), 0);
            fs::remove_file(&src).unwrap();
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn chmod_between_walk_and_stage_is_not_reverted() {
        let dir = temp_dir("chmod");
        let src = dir.join("f");
        fs::write(&src, text(100_000)).unwrap();
        let c = candidate(&src);
        fs::set_permissions(&src, Permissions::from_mode(0o600)).unwrap();
        let staging = dir.join("s");
        fs::create_dir(&staging).unwrap();
        let Staged::Copy(copy) = stage(&c, &staging, &mut Buffers::default()).unwrap() else {
            panic!("not staged");
        };
        // The copy has the mode of now, so the commit sees it differ from the walk's.
        assert_eq!(fs::symlink_metadata(&copy).unwrap().mode() & 0o7777, 0o600);
        assert_ne!(c.mode & 0o7777, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saving_rule_rounds_to_allocation_units() {
        assert!(!saves_space(0, 0));
        assert!(!saves_space(10, 1));
        assert!(!saves_space(4000, 4096));
        assert!(saves_space(4096, 4097));
        assert!(saves_space(100, 8192));
        assert!(!saves_space(8193, 12288));
    }

    #[test]
    fn same_input_gives_same_compressed_bytes() {
        let dir = temp_dir("det");
        let mut bufs = Buffers::default();
        // Inline (one small block), one block in the resource fork, several blocks.
        for size in [5_000, BLOCK_SIZE, 5 * BLOCK_SIZE + 17] {
            let content = if size == 5_000 {
                text(size)
            } else {
                mixed(size)
            };
            let src = dir.join(format!("f{size}"));
            fs::write(&src, &content).unwrap();
            let mut got = Vec::new();
            for run in 0..2 {
                let staging = dir.join(format!("s{size}-{run}"));
                fs::create_dir(&staging).unwrap();
                let Staged::Copy(copy) = stage(&candidate(&src), &staging, &mut bufs).unwrap()
                else {
                    panic!("{size}: not staged");
                };
                assert_eq!(fs::read(&copy).unwrap(), content);
                assert!(!staging.join(PART_DIR).join(format!("f{size}")).exists());
                got.push((
                    xattr(&copy, sys::DECMPFS).unwrap(),
                    xattr(&copy, sys::RESOURCE_FORK),
                ));
            }
            assert_eq!(got[0], got[1], "{size}");
            assert_eq!(got[0].1.is_some(), size > 5_000, "{size}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn decmpfs_header_of_edge_sizes() {
        let dir = temp_dir("edge");
        let mut bufs = Buffers::default();
        for size in [0, 1, 65_535, 65_536, 65_537, 5 << 20] {
            let src = dir.join(format!("e{size}"));
            fs::write(&src, text(size)).unwrap();
            let staging = dir.join(format!("s{size}"));
            fs::create_dir(&staging).unwrap();
            let got = stage(&candidate(&src), &staging, &mut bufs).unwrap();
            // Below one allocation unit nothing can be saved.
            if size < 2 {
                assert_eq!(got, Staged::Incompressible, "{size}");
                continue;
            }
            let Staged::Copy(copy) = got else {
                panic!("{size}: {got:?}");
            };
            let h = xattr(&copy, sys::DECMPFS).unwrap();
            assert_eq!(&h[..4], b"fpmc", "{size}");
            let kind = u32::from_le_bytes(h[4..8].try_into().unwrap());
            // 11: LZFSE inline in the xattr, 12: LZFSE in the resource fork.
            assert!(kind == 11 || kind == 12, "{size}: type {kind}");
            assert_eq!(
                u64::from_le_bytes(h[8..16].try_into().unwrap()),
                size as u64
            );
            let m = fs::symlink_metadata(&copy).unwrap();
            assert_ne!(m.st_flags() & UF_COMPRESSED, 0);
            assert_eq!(m.size(), size as u64);
            assert_eq!(fs::read(&copy).unwrap(), text(size), "{size}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn incompressible_and_empty_files_leave_nothing_behind() {
        let dir = temp_dir("incomp");
        let mut bufs = Buffers::default();
        let random = dir.join("random");
        let mut data = vec![0u8; 3 * BLOCK_SIZE];
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for b in &mut data {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        fs::write(&random, &data).unwrap();
        let empty = dir.join("empty");
        fs::write(&empty, "").unwrap();
        for src in [&random, &empty] {
            let staging = dir.join(format!("s-{}", src.file_name().unwrap().display()));
            fs::create_dir(&staging).unwrap();
            assert_eq!(
                stage(&candidate(src), &staging, &mut bufs).unwrap(),
                Staged::Incompressible
            );
            let left: Vec<_> = fs::read_dir(staging.join(PART_DIR)).unwrap().collect();
            assert!(left.is_empty(), "{left:?}");
            assert!(!staging.join(src.file_name().unwrap()).exists());
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn existing_compression_xattr_and_changed_original_are_not_staged() {
        let dir = temp_dir("skip");
        let mut bufs = Buffers::default();
        let staging = dir.join("s");
        fs::create_dir(&staging).unwrap();
        let src = dir.join("rfork");
        fs::write(&src, text(100_000)).unwrap();
        let f = File::options().write(true).open(&src).unwrap();
        sys::fset_xattr(&f, sys::RESOURCE_FORK, b"fork", 0).unwrap();
        assert_eq!(
            stage(&candidate(&src), &staging, &mut bufs).unwrap(),
            Staged::HasXattr("com.apple.ResourceFork")
        );
        let src = dir.join("grown");
        fs::write(&src, text(100_000)).unwrap();
        let c = candidate(&src);
        fs::write(&src, text(200_000)).unwrap();
        assert_eq!(stage(&c, &staging, &mut bufs).unwrap(), Staged::Changed);
        fs::remove_file(&src).unwrap();
        assert_eq!(stage(&c, &staging, &mut bufs).unwrap(), Staged::Changed);
        // Refused before a part file was ever created.
        assert!(!staging.join(PART_DIR).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// xattr names and values of `path` as a plain listing shows them; the quarantine
    /// timestamp field (`flags;TIME;agent;uuid`) is blanked, it is "now" of the copy.
    fn xattrs(path: &Path) -> Vec<(String, Vec<u8>)> {
        let f = File::open(path).unwrap();
        sys::flist_xattr(&f)
            .unwrap()
            .into_iter()
            .map(|n| {
                let mut v = sys::fget_xattr(&f, &n, false).unwrap().unwrap();
                if n.as_c_str() == XATTR_VALUE_REWRITTEN {
                    let s = String::from_utf8_lossy(&v).into_owned();
                    let mut parts: Vec<&str> = s.split(';').collect();
                    if parts.len() > 1 {
                        parts[1] = "";
                    }
                    v = parts.join(";").into_bytes();
                }
                (n.to_string_lossy().into_owned(), v)
            })
            .collect()
    }

    /// R1 probe: the xattrs (and ACL) of a staged copy are exactly those of ditto's copy of the
    /// same original.
    #[test]
    fn copy_carries_the_same_xattrs_and_acl_as_ditto() {
        let dir = temp_dir("xattr");
        let src = dir.join("orig.txt");
        fs::write(&src, text(200_000)).unwrap();
        let p = src.to_str().unwrap();
        let f = File::open(&src).unwrap();
        sys::fset_xattr(&f, c"user.acz", b"value", 0).unwrap();
        sys::fset_xattr(&f, XATTR_VALUE_REWRITTEN, b"0081;5f000000;Safari;", 0).unwrap();
        drop(f);
        // An ACL has no wrapper here: chmod(1) sets it.
        let acl = Command::new("/bin/chmod")
            .args(["+a", "user:nobody allow read", p])
            .status()
            .unwrap();
        assert!(acl.success());
        let ditto_out = dir.join("ditto");
        fs::create_dir(&ditto_out).unwrap();
        let mut dest = ditto_out.clone().into_os_string();
        dest.push("/");
        let ditto = Command::new("/usr/bin/ditto")
            .args(["--hfsCompression", "--noclone", p])
            .arg(&dest)
            .status()
            .unwrap();
        assert!(ditto.success());
        let staging = dir.join("s");
        fs::create_dir(&staging).unwrap();
        let Staged::Copy(copy) =
            stage(&candidate(&src), &staging, &mut Buffers::default()).unwrap()
        else {
            panic!("not staged");
        };
        let by_ditto = ditto_out.join("orig.txt");
        let names = xattrs(&src);
        assert!(names.iter().any(|(n, _)| n == "user.acz"), "{names:?}");
        assert_eq!(xattrs(&copy), xattrs(&by_ditto));
        let acl = |p: &Path| {
            let out = Command::new("/bin/ls").arg("-le").arg(p).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.lines().skip(1).map(str::to_owned).collect::<Vec<_>>()
        };
        assert_eq!(acl(&copy).len(), 1, "{:?}", acl(&copy));
        assert_eq!(acl(&copy), acl(&by_ditto));
        let born = |p: &Path| fs::metadata(p).unwrap().created().unwrap();
        // The copy keeps the exact birthtime; ditto's copy does not (measured: it gets about
        // the original's mtime), so ditto is no reference here.
        assert_eq!(born(&copy), born(&src));
        assert!(born(&by_ditto) >= born(&src));
        fs::remove_dir_all(&dir).unwrap();
    }
}
