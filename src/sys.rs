//! Thin FFI wrappers over libSystem and the BSD flag constants this tool relies on.

use std::ffi::{CString, c_char, c_int, c_long, c_uint};
use std::io;
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
