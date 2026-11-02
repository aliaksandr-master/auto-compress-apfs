//! Thin FFI wrappers over libSystem and the BSD flag constants this tool relies on.

use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint};
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// `st_flags`: file is stored with transparent (decmpfs) compression.
pub const UF_COMPRESSED: u32 = 0x0000_0020;
/// `st_flags`: user immutable.
pub const UF_IMMUTABLE: u32 = 0x0000_0002;
/// `st_flags`: user append-only.
pub const UF_APPEND: u32 = 0x0000_0004;
/// `st_flags`: system immutable.
pub const SF_IMMUTABLE: u32 = 0x0002_0000;
/// `st_flags`: system append-only.
pub const SF_APPEND: u32 = 0x0004_0000;
/// `st_flags`: entitlement required for writing (SIP protected).
pub const SF_RESTRICTED: u32 = 0x0008_0000;
/// `st_flags`: dataless file or directory (cloud placeholder); reading it downloads the data.
pub const SF_DATALESS: u32 = 0x4000_0000;
/// Flags of files and directories this tool never touches.
pub const PROTECTED_FLAGS: u32 =
    UF_IMMUTABLE | UF_APPEND | SF_IMMUTABLE | SF_APPEND | SF_RESTRICTED;
/// `statfs.f_flags`: filesystem is stored locally.
pub const MNT_LOCAL: u32 = 0x0000_1000;
/// `open(2)`: fail instead of following a symlink in the last component.
pub const O_NOFOLLOW: i32 = 0x0000_0100;
/// `renamex_np(2)`: atomically exchange the two paths.
const RENAME_SWAP: c_uint = 0x0000_0002;
/// `renamex_np` flag: fail with `EEXIST` instead of replacing an existing destination.
const RENAME_EXCL: c_uint = 0x0000_0004;
/// `renamex_np` flag: fail instead of following a symlink in any component of either path.
const RENAME_NOFOLLOW_ANY: c_uint = 0x0000_0010;

#[repr(C)]
struct Tm {
    tm_sec: c_int,
    tm_min: c_int,
    tm_hour: c_int,
    tm_mday: c_int,
    tm_mon: c_int,
    tm_year: c_int,
    tm_wday: c_int,
    tm_yday: c_int,
    tm_isdst: c_int,
    tm_gmtoff: c_long,
    tm_zone: *mut c_char,
}

/// `struct statfs` of the 64-bit inode ABI (`<sys/mount.h>`).
#[repr(C)]
struct Statfs {
    f_bsize: u32,
    f_iosize: i32,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_fsid: [i32; 2],
    f_owner: u32,
    f_type: u32,
    f_flags: u32,
    f_fssubtype: u32,
    f_fstypename: [c_char; 16],
    f_mntonname: [c_char; 1024],
    f_mntfromname: [c_char; 1024],
    f_flags_ext: u32,
    f_reserved: [u32; 7],
}

const _: () = assert!(std::mem::size_of::<Statfs>() == 2168);

/// `setiopolicy_np(2)`: I/O policy type controlling materialization of dataless files.
const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: c_int = 3;
const IOPOL_SCOPE_PROCESS: c_int = 0;
const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: c_int = 1;

unsafe extern "C" {
    fn geteuid() -> u32;
    fn localtime_r(clock: *const i64, result: *mut Tm) -> *mut Tm;
    fn renamex_np(from: *const c_char, to: *const c_char, flags: c_uint) -> c_int;
    #[cfg_attr(target_arch = "x86_64", link_name = "statfs$INODE64")]
    #[cfg_attr(not(target_arch = "x86_64"), link_name = "statfs")]
    fn c_statfs(path: *const c_char, buf: *mut Statfs) -> c_int;
    fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
}

/// Effective user id of this process.
pub fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, cannot fail and has no side effects.
    unsafe { geteuid() }
}

/// Broken-down local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// Offset from UTC in seconds.
    pub gmtoff: i64,
}

/// Converts seconds since the Unix epoch to local time, `None` if libc cannot represent it.
pub fn local_time(secs: i64) -> Option<LocalTime> {
    let mut tm = Tm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: std::ptr::null_mut(),
    };
    // SAFETY: both pointers refer to live, properly aligned locals for the duration of the call;
    // localtime_r is the reentrant variant and only writes into `tm`. `tm_zone` ends up pointing
    // into libc's static timezone data and is never dereferenced here.
    let res = unsafe { localtime_r(&secs, &mut tm) };
    if res.is_null() {
        return None;
    }
    Some(LocalTime {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec as u32,
        gmtoff: tm.tm_gmtoff,
    })
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// Atomically exchanges the directory entries `a` and `b` (`renamex_np(a, b, RENAME_SWAP)`).
/// Both must exist on the same filesystem.
pub fn rename_swap(a: &Path, b: &Path) -> io::Result<()> {
    let a = c_path(a)?;
    let b = c_path(b)?;
    // SAFETY: `a` and `b` are valid NUL-terminated strings that outlive the call; renamex_np
    // does not retain the pointers.
    let rc = unsafe { renamex_np(a.as_ptr(), b.as_ptr(), RENAME_SWAP) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Renames `from` to `to` unless `to` exists (`EEXIST` then), never following a symlink in any
/// component of either path (`renamex_np(from, to, RENAME_EXCL | RENAME_NOFOLLOW_ANY)`); never
/// replaces an entry. Both must be on the same filesystem.
pub fn rename_excl(from: &Path, to: &Path) -> io::Result<()> {
    let a = c_path(from)?;
    let b = c_path(to)?;
    // SAFETY: `a` and `b` are valid NUL-terminated strings that outlive the call; renamex_np
    // does not retain the pointers.
    let rc = unsafe { renamex_np(a.as_ptr(), b.as_ptr(), RENAME_EXCL | RENAME_NOFOLLOW_ANY) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Renames `from` to `to` unless `to` exists (`EEXIST` then); never replaces an entry
/// (`renamex_np(from, to, RENAME_EXCL)`). Unlike [`rename_excl`] it follows symlinks in the
/// leading components, as [`rename_swap`] does: for two names inside one staging dir of ours.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let a = c_path(from)?;
    let b = c_path(to)?;
    // SAFETY: `a` and `b` are valid NUL-terminated strings that outlive the call; renamex_np
    // does not retain the pointers.
    let rc = unsafe { renamex_np(a.as_ptr(), b.as_ptr(), RENAME_EXCL) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Names (as `chflags(1)` spells them) of the [`PROTECTED_FLAGS`] set in `flags`, comma separated.
pub fn flag_names(flags: u32) -> String {
    [
        (UF_IMMUTABLE, "uchg"),
        (UF_APPEND, "uappnd"),
        (SF_IMMUTABLE, "schg"),
        (SF_APPEND, "sappnd"),
        (SF_RESTRICTED, "restricted"),
    ]
    .iter()
    .filter(|(bit, _)| flags & bit != 0)
    .map(|(_, name)| *name)
    .collect::<Vec<_>>()
    .join(",")
}

/// The parts of `statfs(2)` this tool uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsInfo {
    /// Filesystem id (`f_fsid`), unique per mounted volume.
    pub fsid: [i32; 2],
    /// Mount flags (`f_flags`), e.g. [`MNT_LOCAL`].
    pub flags: u32,
    /// Filesystem type name, e.g. `apfs`.
    pub fstype: String,
    /// Mount point (`f_mntonname`).
    pub mount_on: PathBuf,
    /// Bytes available to unprivileged users (`f_bavail * f_bsize`).
    pub bavail_bytes: u64,
}

/// Bytes of a fixed-size C string field up to its NUL (or the whole field).
fn c_chars(chars: &[c_char]) -> Vec<u8> {
    chars
        .iter()
        .map(|&c| c as u8)
        .take_while(|&b| b != 0)
        .collect()
}

/// `statfs(2)` of the filesystem holding `path` (follows symlinks, never reads file data).
pub fn statfs(path: &Path) -> io::Result<FsInfo> {
    let p = c_path(path)?;
    // SAFETY: Statfs is plain old data (integers and char arrays); all-zero is a valid value.
    let mut buf: Statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `p` is a valid NUL-terminated string and `buf` a live, properly aligned Statfs
    // whose layout matches the 64-bit inode ABI of the linked symbol (size asserted above);
    // statfs only writes into `buf` and does not retain either pointer.
    let rc = unsafe { c_statfs(p.as_ptr(), &mut buf) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FsInfo {
        fsid: buf.f_fsid,
        flags: buf.f_flags,
        fstype: String::from_utf8_lossy(&c_chars(&buf.f_fstypename)).into_owned(),
        mount_on: PathBuf::from(std::ffi::OsStr::from_bytes(&c_chars(&buf.f_mntonname))),
        bavail_bytes: buf.f_bavail.saturating_mul(u64::from(buf.f_bsize)),
    })
}

/// Turns off materialization of dataless (cloud placeholder) files for this process, so a read
/// of such a file fails with `EDEADLK` instead of downloading it.
pub fn disable_dataless_materialization() -> io::Result<()> {
    // SAFETY: setiopolicy_np takes three integers, has no pointer arguments and only changes
    // the I/O policy of this process.
    let rc = unsafe {
        setiopolicy_np(
            IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
            IOPOL_SCOPE_PROCESS,
            IOPOL_MATERIALIZE_DATALESS_FILES_OFF,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Name of the resource fork xattr; a decmpfs file keeps its compressed blocks there.
pub const RESOURCE_FORK: &CStr = c"com.apple.ResourceFork";
/// Name of the decmpfs header xattr of a transparently compressed file.
pub const DECMPFS: &CStr = c"com.apple.decmpfs";
/// `fcopyfile(3)`: copy the extended attributes.
pub const COPYFILE_XATTR: u32 = libc::COPYFILE_XATTR;
/// `fcopyfile(3)`: copy the access control list.
pub const COPYFILE_ACL: u32 = libc::COPYFILE_ACL;

fn check(rc: c_int) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn xattr_opts(show_compression: bool) -> c_int {
    if show_compression {
        libc::XATTR_SHOWCOMPRESSION
    } else {
        0
    }
}

/// Length of the xattr `name` of `f`, `None` when it is not set. `show_compression` passes
/// `XATTR_SHOWCOMPRESSION`, which makes the decmpfs and resource fork xattrs of a compressed
/// file visible.
pub fn fxattr_len(f: &File, name: &CStr, show_compression: bool) -> io::Result<Option<usize>> {
    // SAFETY: the fd is open for the lifetime of `f`, `name` is NUL-terminated; a null
    // buffer of size 0 asks only for the value length.
    let len = unsafe {
        libc::fgetxattr(
            f.as_raw_fd(),
            name.as_ptr(),
            std::ptr::null_mut(),
            0,
            0,
            xattr_opts(show_compression),
        )
    };
    if len >= 0 {
        return Ok(Some(len as usize));
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOATTR) => Ok(None),
        _ => Err(e),
    }
}

/// Value of the xattr `name` of `f`, `None` when it is not set (`show_compression` as for
/// [`fxattr_len`]).
pub fn fget_xattr(f: &File, name: &CStr, show_compression: bool) -> io::Result<Option<Vec<u8>>> {
    loop {
        let Some(len) = fxattr_len(f, name, show_compression)? else {
            return Ok(None);
        };
        let mut buf = vec![0u8; len];
        // SAFETY: `buf` is a live allocation of `buf.len()` writable bytes; fgetxattr writes
        // at most that many and does not retain any pointer.
        let got = unsafe {
            libc::fgetxattr(
                f.as_raw_fd(),
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                0,
                xattr_opts(show_compression),
            )
        };
        if got >= 0 {
            buf.truncate(got as usize);
            return Ok(Some(buf));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            // Grew between the two calls: ask for the length again.
            Some(libc::ERANGE) => continue,
            Some(libc::ENOATTR) => return Ok(None),
            _ => return Err(e),
        }
    }
}

/// Writes `value` into the xattr `name` of `f` at byte `position` (only the resource fork
/// takes a non-zero position), with `XATTR_SHOWCOMPRESSION`.
pub fn fset_xattr(f: &File, name: &CStr, value: &[u8], position: u32) -> io::Result<()> {
    // SAFETY: the fd is open for the lifetime of `f`, `name` is NUL-terminated and `value` is
    // valid for `value.len()` bytes; fsetxattr only reads them and retains no pointer.
    check(unsafe {
        libc::fsetxattr(
            f.as_raw_fd(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            position,
            libc::XATTR_SHOWCOMPRESSION,
        )
    })
}

/// Creates the xattr `name` of `f` with `value` (`XATTR_CREATE`); an existing value is never
/// replaced: `Ok(false)` when the xattr is already set. No `XATTR_NOFOLLOW`: fsetxattr rejects
/// it with `EINVAL` (an fd has no path to follow; open it with `O_NOFOLLOW` instead).
pub fn fset_xattr_new(f: &File, name: &CStr, value: &[u8]) -> io::Result<bool> {
    // SAFETY: the fd is open for the lifetime of `f`, `name` is NUL-terminated and `value` is
    // valid for `value.len()` bytes; fsetxattr only reads them and retains no pointer.
    let rc = unsafe {
        libc::fsetxattr(
            f.as_raw_fd(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            libc::XATTR_CREATE,
        )
    };
    match check(rc) {
        Ok(()) => Ok(true),
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Names of the xattrs of `f` as a plain listing shows them (no `XATTR_SHOWCOMPRESSION`: the
/// decmpfs and resource fork xattrs of a compressed file are hidden).
pub fn flist_xattr(f: &File) -> io::Result<Vec<CString>> {
    loop {
        // SAFETY: the fd is open for the lifetime of `f`; a null buffer of size 0 asks only
        // for the list length.
        let len = unsafe { libc::flistxattr(f.as_raw_fd(), std::ptr::null_mut(), 0, 0) };
        if len < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u8; len as usize];
        // SAFETY: `buf` is a live allocation of `buf.len()` writable bytes; flistxattr writes
        // at most that many and does not retain the pointer.
        let got = unsafe { libc::flistxattr(f.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if got < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ERANGE) {
                continue;
            }
            return Err(e);
        }
        buf.truncate(got as usize);
        return Ok(buf
            .split(|&b| b == 0)
            .filter(|n| !n.is_empty())
            .map(|n| CString::new(n).expect("split at NUL"))
            .collect());
    }
}

/// Sets the `st_flags` of `f` to `flags` (`fchflags(2)`).
pub fn fchflags(f: &File, flags: u32) -> io::Result<()> {
    // SAFETY: the fd is open for the lifetime of `f`; fchflags takes no pointers.
    check(unsafe { libc::fchflags(f.as_raw_fd(), flags) })
}

/// Copies what `flags` ([`COPYFILE_XATTR`], [`COPYFILE_ACL`]) select from `src` to `dst`
/// (`fcopyfile(3)` without state).
pub fn fcopyfile(src: &File, dst: &File, flags: u32) -> io::Result<()> {
    // SAFETY: both fds are open for the lifetime of the borrows; a null state is allowed and
    // fcopyfile retains nothing.
    check(unsafe {
        libc::fcopyfile(
            src.as_raw_fd(),
            dst.as_raw_fd(),
            std::ptr::null_mut(),
            flags,
        )
    })
}

/// The resource fork of a file as a seekable byte stream: every write is an
/// [`fset_xattr`] of [`RESOURCE_FORK`] at the current position. Positions beyond `u32::MAX`
/// are errors (the xattr position is 32-bit). The fork is assumed absent when created.
pub struct RforkWriter<'a> {
    file: &'a File,
    pos: u64,
    /// Bytes written so far up to the furthest end (the fork length).
    len: u64,
}

impl<'a> RforkWriter<'a> {
    pub fn new(file: &'a File) -> Self {
        Self {
            file,
            pos: 0,
            len: 0,
        }
    }
}

fn too_far() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "resource fork position beyond 4 GiB",
    )
}

impl Write for RforkWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let end = self.pos + buf.len() as u64;
        if end > u64::from(u32::MAX) {
            return Err(too_far());
        }
        fset_xattr(self.file, RESOURCE_FORK, buf, self.pos as u32)?;
        self.pos = end;
        self.len = self.len.max(end);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for RforkWriter<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
        };
        match pos {
            Some(p) if p <= u64::from(u32::MAX) => {
                self.pos = p;
                Ok(p)
            }
            Some(_) => Err(too_far()),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the resource fork",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::macos::fs::MetadataExt as _;

    #[test]
    fn local_time_is_plausible() {
        let t = local_time(1_700_000_000).expect("localtime_r");
        assert!(t.year == 2023 && t.month == 11 && (13..=15).contains(&t.day));
        assert!(t.gmtoff.abs() <= 14 * 3600);
    }

    #[test]
    fn rename_swap_exchanges_files() {
        let dir = std::env::temp_dir().join(format!("acz-sys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, "A").unwrap();
        std::fs::write(&b, "B").unwrap();
        rename_swap(&a, &b).unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "B");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "A");
        assert!(rename_swap(&a, &dir.join("missing")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn statfs_of_temp_dir_and_data_volume() {
        let data = statfs(Path::new("/System/Volumes/Data")).unwrap();
        assert_eq!(data.fstype, "apfs");
        assert_ne!(data.flags & MNT_LOCAL, 0);
        assert_eq!(data.mount_on, Path::new("/System/Volumes/Data"));
        assert!(data.bavail_bytes > 0);
        let tmp = statfs(&std::env::temp_dir()).unwrap();
        assert_eq!(tmp.fstype, "apfs");
        assert_ne!(tmp.flags & MNT_LOCAL, 0);
        assert_eq!(tmp.fsid, data.fsid, "temp dir is on the data volume");
        let devfs = statfs(Path::new("/dev")).unwrap();
        assert_eq!(devfs.fstype, "devfs");
        assert!(statfs(Path::new("/nonexistent-acz")).is_err());
    }

    #[test]
    fn flag_names_in_chflags_spelling() {
        assert_eq!(flag_names(UF_IMMUTABLE), "uchg");
        assert_eq!(flag_names(UF_IMMUTABLE | UF_APPEND), "uchg,uappnd");
        assert_eq!(
            flag_names(SF_IMMUTABLE | SF_APPEND | SF_RESTRICTED),
            "schg,sappnd,restricted"
        );
        assert_eq!(flag_names(UF_COMPRESSED), "");
    }

    #[test]
    fn dataless_policy_can_be_set() {
        disable_dataless_materialization().unwrap();
    }

    #[test]
    fn xattr_wrappers_round_trip() {
        let dir = std::env::temp_dir().join(format!("acz-sys-xattr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, "A").unwrap();
        std::fs::write(&b, "B").unwrap();
        let fa = File::options().read(true).write(true).open(&a).unwrap();
        let fb = File::options().read(true).write(true).open(&b).unwrap();
        assert_eq!(fget_xattr(&fa, c"user.acz", false).unwrap(), None);
        assert!(fset_xattr_new(&fa, c"user.acz-new", b"one").unwrap());
        assert!(!fset_xattr_new(&fa, c"user.acz-new", b"two").unwrap());
        assert_eq!(
            fget_xattr(&fa, c"user.acz-new", false).unwrap().as_deref(),
            Some(&b"one"[..])
        );
        fset_xattr(&fa, c"user.acz", b"value", 0).unwrap();
        assert_eq!(
            fget_xattr(&fa, c"user.acz", false).unwrap().as_deref(),
            Some(&b"value"[..])
        );
        // The system may add com.apple.provenance to files this process creates.
        let has = |f: &File| flist_xattr(f).unwrap().contains(&c"user.acz".to_owned());
        assert!(has(&fa));
        assert!(!has(&fb));
        fcopyfile(&fa, &fb, COPYFILE_XATTR).unwrap();
        assert!(has(&fb));
        let flags = std::fs::metadata(&b).unwrap().st_flags();
        fchflags(&fb, flags | 0x0000_8000).unwrap(); // UF_HIDDEN
        assert_ne!(std::fs::metadata(&b).unwrap().st_flags() & 0x0000_8000, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rfork_writer_gap_seek_and_overwrite() {
        let dir = std::env::temp_dir().join(format!("acz-sys-rfork-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f");
        std::fs::write(&p, "").unwrap();
        let f = File::options().read(true).write(true).open(&p).unwrap();
        let mut w = RforkWriter::new(&f);
        // Like the decmpfs writer: skip the header, write the blocks, then the header.
        assert_eq!(w.seek(SeekFrom::Start(8)).unwrap(), 8);
        w.write_all(b"blocks").unwrap();
        assert_eq!(w.seek(SeekFrom::End(0)).unwrap(), 14);
        assert_eq!(w.seek(SeekFrom::Current(-14)).unwrap(), 0);
        w.write_all(b"HEADER__").unwrap();
        assert_eq!(w.stream_position().unwrap(), 8);
        assert_eq!(
            fget_xattr(&f, RESOURCE_FORK, true).unwrap().as_deref(),
            Some(&b"HEADER__blocks"[..])
        );
        assert!(w.seek(SeekFrom::Current(-9)).is_err());
        assert!(w.seek(SeekFrom::Start(u64::from(u32::MAX) + 1)).is_err());
        w.seek(SeekFrom::Start(u64::from(u32::MAX) - 1)).unwrap();
        assert!(w.write(b"xy").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
