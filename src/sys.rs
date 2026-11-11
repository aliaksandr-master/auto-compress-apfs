//! Thin FFI wrappers over libSystem and the BSD flag constants this tool relies on.

#![expect(
    unsafe_code,
    reason = "the FFI home: every libSystem call std lacks is wrapped here, each with a SAFETY comment"
)]

use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// `st_flags`: file is stored with transparent (decmpfs) compression.
pub(crate) const UF_COMPRESSED: u32 = 0x0000_0020;
/// `st_flags`: user immutable.
pub(crate) const UF_IMMUTABLE: u32 = 0x0000_0002;
/// `st_flags`: user append-only.
pub(crate) const UF_APPEND: u32 = 0x0000_0004;
/// `st_flags`: system immutable.
pub(crate) const SF_IMMUTABLE: u32 = 0x0002_0000;
/// `st_flags`: system append-only.
pub(crate) const SF_APPEND: u32 = 0x0004_0000;
/// `st_flags`: entitlement required for writing (SIP protected).
pub(crate) const SF_RESTRICTED: u32 = 0x0008_0000;
/// `st_flags`: dataless file or directory (cloud placeholder); reading it downloads the data.
pub(crate) const SF_DATALESS: u32 = 0x4000_0000;
/// Flags of files and directories this tool never touches.
pub(crate) const PROTECTED_FLAGS: u32 =
    UF_IMMUTABLE | UF_APPEND | SF_IMMUTABLE | SF_APPEND | SF_RESTRICTED;
/// `statfs.f_flags`: filesystem is stored locally.
pub(crate) const MNT_LOCAL: u32 = 0x0000_1000;
/// `open(2)`: fail instead of following a symlink in the last component.
pub(crate) const O_NOFOLLOW: i32 = 0x0000_0100;
/// `renamex_np(2)`: atomically exchange the two paths.
const RENAME_SWAP: c_uint = 0x0000_0002;
/// `renamex_np` flag: fail with `EEXIST` instead of replacing an existing destination.
const RENAME_EXCL: c_uint = 0x0000_0004;
/// `renamex_np` flag: fail instead of following a symlink in any component of either path.
const RENAME_NOFOLLOW_ANY: c_uint = 0x0000_0010;

#[expect(
    clippy::struct_field_names,
    reason = "mirrors the `tm_*` fields of the C `struct tm`"
)]
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
#[expect(
    clippy::struct_field_names,
    reason = "mirrors the `f_*` fields of the C `struct statfs`"
)]
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

const _: () = assert!(size_of::<Statfs>() == 2168);

/// `setiopolicy_np(2)`: I/O policy type controlling materialization of dataless files.
const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: c_int = 3;
const IOPOL_SCOPE_PROCESS: c_int = 0;
const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: c_int = 1;

#[expect(
    clippy::unnecessary_safety_comment,
    reason = "the SAFETY comment documents why these declarations match the C prototypes"
)]
// SAFETY: each declaration matches its libSystem prototype on 64-bit macOS (arm64 and x86_64):
// `uid_t geteuid(void)` (geteuid(2), uid_t = u32); `struct tm *localtime_r(const time_t *,
// struct tm *)` (localtime(3), time_t = i64); `int renamex_np(const char *, const char *,
// unsigned int)` (rename(2)); `int statfs(const char *, struct statfs *)` (statfs(2)), bound to
// `statfs$INODE64` on x86_64 and to the plain symbol on arm64, both the 64-bit inode layout of
// `Statfs`; `int setiopolicy_np(int, int, int)` (setiopolicy_np(3)).
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
pub(crate) fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, cannot fail and has no side effects.
    unsafe { geteuid() }
}

/// Broken-down local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalTime {
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
pub(crate) fn local_time(secs: i64) -> Option<LocalTime> {
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
    let res = unsafe { localtime_r(&raw const secs, &raw mut tm) };
    if res.is_null() {
        return None;
    }
    Some(LocalTime {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1).cast_unsigned(),
        day: tm.tm_mday.cast_unsigned(),
        hour: tm.tm_hour.cast_unsigned(),
        minute: tm.tm_min.cast_unsigned(),
        second: tm.tm_sec.cast_unsigned(),
        gmtoff: tm.tm_gmtoff,
    })
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// Atomically exchanges the directory entries `a` and `b` (`renamex_np(a, b, RENAME_SWAP)`).
/// Both must exist on the same filesystem.
pub(crate) fn rename_swap(a: &Path, b: &Path) -> io::Result<()> {
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
pub(crate) fn rename_excl(from: &Path, to: &Path) -> io::Result<()> {
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
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
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
pub(crate) fn flag_names(flags: u32) -> String {
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
pub(crate) struct FsInfo {
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
        .map(|&c| c.cast_unsigned())
        .take_while(|&b| b != 0)
        .collect()
}

/// `statfs(2)` of the filesystem holding `path` (follows symlinks, never reads file data).
pub(crate) fn statfs(path: &Path) -> io::Result<FsInfo> {
    let p = c_path(path)?;
    let mut buf = MaybeUninit::<Statfs>::uninit();
    // SAFETY: `p` is a valid NUL-terminated string and `buf` live, properly aligned storage for
    // a Statfs whose layout matches the 64-bit inode ABI of the linked symbol (size asserted
    // above); statfs only writes into `buf` and does not retain either pointer.
    let rc = unsafe { c_statfs(p.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statfs returned 0, so it filled the whole struct.
    let buf = unsafe { buf.assume_init() };
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
pub(crate) fn disable_dataless_materialization() -> io::Result<()> {
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
pub(crate) const RESOURCE_FORK: &CStr = c"com.apple.ResourceFork";
/// Name of the decmpfs header xattr of a transparently compressed file.
pub(crate) const DECMPFS: &CStr = c"com.apple.decmpfs";
/// `fcopyfile(3)`: copy the extended attributes.
pub(crate) const COPYFILE_XATTR: u32 = libc::COPYFILE_XATTR;
/// `fcopyfile(3)`: copy the access control list.
pub(crate) const COPYFILE_ACL: u32 = libc::COPYFILE_ACL;

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
pub(crate) fn fxattr_len(
    f: &File,
    name: &CStr,
    show_compression: bool,
) -> io::Result<Option<usize>> {
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
        return Ok(Some(len.cast_unsigned()));
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOATTR) => Ok(None),
        _ => Err(e),
    }
}

/// Value of the xattr `name` of `f`, `None` when it is not set (`show_compression` as for
/// [`fxattr_len`]).
pub(crate) fn fget_xattr(
    f: &File,
    name: &CStr,
    show_compression: bool,
) -> io::Result<Option<Vec<u8>>> {
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
            buf.truncate(got.cast_unsigned());
            return Ok(Some(buf));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            // Grew between the two calls: ask for the length again.
            Some(libc::ERANGE) => {}
            Some(libc::ENOATTR) => return Ok(None),
            _ => return Err(e),
        }
    }
}

/// Reads the xattr `name` of `f` from byte `position` into `buf` with
/// `XATTR_SHOWCOMPRESSION` (only the resource fork takes a non-zero position); returns the
/// bytes read, 0 at or past the end of the value.
pub(crate) fn fget_xattr_at(
    f: &File,
    name: &CStr,
    buf: &mut [u8],
    position: u32,
) -> io::Result<usize> {
    // SAFETY: `buf` is a live allocation of `buf.len()` writable bytes, the fd is open for the
    // lifetime of `f` and `name` is NUL-terminated; fgetxattr writes at most `buf.len()` bytes
    // and retains no pointer.
    let got = unsafe {
        libc::fgetxattr(
            f.as_raw_fd(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            position,
            libc::XATTR_SHOWCOMPRESSION,
        )
    };
    if got >= 0 {
        return Ok(got.cast_unsigned());
    }
    Err(io::Error::last_os_error())
}

/// Drops the cached pages of the first `len` bytes of `f`, so the next read decompresses
/// from disk: a read-only shared mapping is `msync`ed with `MS_INVALIDATE`, then unmapped.
/// A no-op for `len` 0.
pub(crate) fn evict_cached(f: &File, len: u64) -> io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    let len = usize::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: a fresh read-only mapping of an fd open for the lifetime of `f`; the kernel
    // picks the address, and nothing dereferences the mapping.
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            f.as_raw_fd(),
            0,
        )
    };
    if addr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `addr`/`len` are exactly the mapping created above, still mapped.
    let synced = check(unsafe { libc::msync(addr, len, libc::MS_SYNC | libc::MS_INVALIDATE) });
    // SAFETY: unmaps exactly the mapping created above; no reference into it exists.
    let unmapped = check(unsafe { libc::munmap(addr, len) });
    synced.and(unmapped)
}

/// Writes `value` into the xattr `name` of `f` at byte `position` (only the resource fork
/// takes a non-zero position), with `XATTR_SHOWCOMPRESSION`.
pub(crate) fn fset_xattr(f: &File, name: &CStr, value: &[u8], position: u32) -> io::Result<()> {
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
pub(crate) fn fset_xattr_new(f: &File, name: &CStr, value: &[u8]) -> io::Result<bool> {
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
pub(crate) fn flist_xattr(f: &File) -> io::Result<Vec<CString>> {
    loop {
        // SAFETY: the fd is open for the lifetime of `f`; a null buffer of size 0 asks only
        // for the list length.
        let len = unsafe { libc::flistxattr(f.as_raw_fd(), std::ptr::null_mut(), 0, 0) };
        if len < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u8; len.cast_unsigned()];
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
        buf.truncate(got.cast_unsigned());
        return Ok(buf
            .split(|&b| b == 0)
            .filter(|n| !n.is_empty())
            // Never drops a name: a piece split at NUL holds no interior NUL.
            .filter_map(|n| CString::new(n).ok())
            .collect());
    }
}

/// Sets the `st_flags` of `f` to `flags` (`fchflags(2)`).
pub(crate) fn fchflags(f: &File, flags: u32) -> io::Result<()> {
    // SAFETY: the fd is open for the lifetime of `f`; fchflags takes no pointers.
    check(unsafe { libc::fchflags(f.as_raw_fd(), flags) })
}

/// Copies what `flags` ([`COPYFILE_XATTR`], [`COPYFILE_ACL`]) select from `src` to `dst`
/// (`fcopyfile(3)` without state).
pub(crate) fn fcopyfile(src: &File, dst: &File, flags: u32) -> io::Result<()> {
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

/// `clonefile(2)` flag: fail instead of following a symlink as the source.
const CLONE_NOFOLLOW: u32 = 0x0001;

/// Creates `name` (one path component, must not exist) in the directory `dir` as an APFS
/// clone of `src`: its own inode sharing `src`'s data blocks, with `src`'s mode, flags,
/// extended attributes and ACL (`fclonefileat(2)` with `CLONE_NOFOLLOW`).
pub(crate) fn fclone_at(src: &File, dir: &File, name: &CStr) -> io::Result<()> {
    // SAFETY: both fds are open for the lifetime of the borrows and `name` is NUL-terminated;
    // fclonefileat retains no pointer.
    check(unsafe {
        libc::fclonefileat(
            src.as_raw_fd(),
            dir.as_raw_fd(),
            name.as_ptr(),
            CLONE_NOFOLLOW,
        )
    })
}

/// What [`fclone_info`] reads with `fgetattrlist(2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CloneInfo {
    /// `ATTR_CMNEXT_CLONEID`: equal for files that are clones of each other; `None` when the
    /// filesystem does not report it.
    pub clone_id: Option<u64>,
    /// `ATTR_CMNEXT_PRIVATESIZE`: bytes of the file not shared with a clone, when reported.
    pub private_size: Option<u64>,
    /// `ATTR_CMN_EXTENDED_SECURITY` is set and not empty: the file has an ACL.
    pub acl: bool,
}

/// The clone id, private size and ACL presence of `f` (`fgetattrlist(2)` with
/// `FSOPT_PACK_INVAL_ATTRS`, so the fixed layout holds whatever the filesystem reports).
pub(crate) fn fclone_info(f: &File) -> io::Result<CloneInfo> {
    let mut req = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_EXTENDED_SECURITY,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        // With FSOPT_ATTR_CMN_EXTENDED this group holds ATTR_CMNEXT_* bits.
        forkattr: libc::ATTR_CMNEXT_PRIVATESIZE | libc::ATTR_CMNEXT_CLONEID,
    };
    // Layout: u32 length, attribute_set_t (20 bytes), attrreference_t of the ACL (8), then
    // the extended common attributes in bit order: private size (off_t), clone id (u64). The
    // ACL data itself follows; a buffer too small for it only truncates that data.
    let mut buf = [0u8; 4096];
    // SAFETY: `req` and `buf` are live locals for the duration of the call, `buf.len()` bytes
    // are writable; the fd is open for the lifetime of `f` and nothing is retained.
    check(unsafe {
        libc::fgetattrlist(
            f.as_raw_fd(),
            (&raw mut req).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::FSOPT_PACK_INVAL_ATTRS | libc::FSOPT_ATTR_CMN_EXTENDED,
        )
    })?;
    // Constant offsets inside the fixed buffer; the slice index panics on a layout bug.
    let u32_at = |o: usize| {
        let mut b = [0u8; 4];
        b.copy_from_slice(&buf[o..o + 4]);
        u32::from_ne_bytes(b)
    };
    let u64_at = |o: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&buf[o..o + 8]);
        u64::from_ne_bytes(b)
    };
    let (common, fork) = (u32_at(4), u32_at(20));
    let valid = |set: u32, bit: u32| set & bit != 0;
    Ok(CloneInfo {
        acl: valid(common, libc::ATTR_CMN_EXTENDED_SECURITY) && u32_at(28) > 0,
        private_size: valid(fork, libc::ATTR_CMNEXT_PRIVATESIZE).then(|| u64_at(32)),
        clone_id: valid(fork, libc::ATTR_CMNEXT_CLONEID).then(|| u64_at(40)),
    })
}

/// The resource fork of a file as a seekable byte stream: every write is an
/// [`fset_xattr`] of [`RESOURCE_FORK`] at the current position. Positions beyond `u32::MAX`
/// are errors (the xattr position is 32-bit). The fork is assumed absent when created.
pub(crate) struct RforkWriter<'a> {
    file: &'a File,
    pos: u64,
    /// Bytes written so far up to the furthest end (the fork length).
    len: u64,
}

impl<'a> RforkWriter<'a> {
    pub(crate) fn new(file: &'a File) -> Self {
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
        fset_xattr(
            self.file,
            RESOURCE_FORK,
            buf,
            u32::try_from(self.pos).map_err(|_| too_far())?,
        )?;
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
            Some(p) if u32::try_from(p).is_ok() => {
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

/// The resource fork of a file as a seekable read stream, the counterpart of
/// [`RforkWriter`]: reads are [`fget_xattr_at`] of [`RESOURCE_FORK`] refilling a 1 MiB
/// buffer. The fork length is taken when created; positions beyond `u32::MAX` are errors.
pub(crate) struct RforkReader<'a> {
    file: &'a File,
    pos: u64,
    len: u64,
    buf: Vec<u8>,
    /// Fork offset of `buf[0]`; `buf_len` bytes of `buf` are valid.
    buf_start: u64,
    buf_len: usize,
}

const RFORK_READ_BUF: usize = 1 << 20;

impl<'a> RforkReader<'a> {
    /// A reader at position 0; an absent fork reads as empty.
    pub(crate) fn new(file: &'a File) -> io::Result<Self> {
        let len = fxattr_len(file, RESOURCE_FORK, true)?.unwrap_or(0) as u64;
        Ok(Self {
            file,
            pos: 0,
            len,
            buf: vec![0; RFORK_READ_BUF],
            buf_start: 0,
            buf_len: 0,
        })
    }

    /// A reader of no bytes that never calls into the kernel nor allocates its buffer: the
    /// stand-in for data stored inline in the decmpfs xattr.
    pub(crate) fn empty(file: &'a File) -> Self {
        Self {
            file,
            pos: 0,
            len: 0,
            buf: Vec::new(),
            buf_start: 0,
            buf_len: 0,
        }
    }
}

impl Read for RforkReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || out.is_empty() {
            return Ok(0);
        }
        let in_buf = self.pos >= self.buf_start && self.pos < self.buf_start + self.buf_len as u64;
        if !in_buf {
            let at = u32::try_from(self.pos).map_err(|_| too_far())?;
            let got = fget_xattr_at(self.file, RESOURCE_FORK, &mut self.buf, at)?;
            self.buf_start = self.pos;
            self.buf_len = got;
            if got == 0 {
                return Ok(0);
            }
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "pos - buf_start < buf_len, a usize"
        )]
        let at = (self.pos - self.buf_start) as usize;
        let n = out.len().min(self.buf_len - at);
        out[..n].copy_from_slice(&self.buf[at..at + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for RforkReader<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
        };
        match pos {
            Some(p) if u32::try_from(p).is_ok() => {
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
        std::fs::remove_dir_all(&dir).ok();
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
    fn clone_shares_clone_id_and_acl_is_seen() {
        let dir = std::env::temp_dir().join(format!("acz-sys-clone-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a");
        std::fs::write(&a, vec![7u8; 200_000]).unwrap();
        let src = File::open(&a).unwrap();
        let d = File::open(&dir).unwrap();
        fclone_at(&src, &d, c"b").unwrap();
        assert!(fclone_at(&src, &d, c"b").is_err(), "existing name");
        let b = File::open(dir.join("b")).unwrap();
        let (ia, ib) = (fclone_info(&src).unwrap(), fclone_info(&b).unwrap());
        assert!(ia.clone_id.is_some(), "{ia:?}");
        assert_eq!(ia.clone_id, ib.clone_id);
        assert!(!ia.acl);
        assert_eq!(std::fs::read(dir.join("b")).unwrap(), vec![7u8; 200_000]);
        let c = dir.join("c");
        std::fs::write(&c, vec![7u8; 200_000]).unwrap();
        let ic = fclone_info(&File::open(&c).unwrap()).unwrap();
        assert_ne!(ic.clone_id, ia.clone_id);
        let d = dir.join("d");
        std::fs::write(&d, vec![7u8; 200_000]).unwrap();
        let id = fclone_info(&File::open(&d).unwrap()).unwrap();
        assert_ne!(ic.clone_id, id.clone_id);
        // A shared clone keeps (almost) nothing private.
        assert!(ib.private_size.is_some_and(|p| p < 200_000), "{ib:?}");
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow read"])
            .arg(&c)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(fclone_info(&File::open(&c).unwrap()).unwrap().acl);
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
        std::fs::remove_dir_all(&dir).ok();
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
        std::fs::remove_dir_all(&dir).ok();
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

    #[test]
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "test: a fork of a few KiB; i % 251 fits a byte"
    )]
    fn rfork_reader_seeks_and_ends() {
        let dir = std::env::temp_dir().join(format!("acz-sys-rfork-r-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f");
        std::fs::write(&p, "").unwrap();
        let f = File::options().read(true).write(true).open(&p).unwrap();
        // Longer than the reader's buffer, so a refill is crossed.
        let data: Vec<u8> = (0..RFORK_READ_BUF + 1000)
            .map(|i| (i % 251) as u8)
            .collect();
        RforkWriter::new(&f).write_all(&data).unwrap();
        let mut r = RforkReader::new(&f).unwrap();
        let mut all = Vec::new();
        r.read_to_end(&mut all).unwrap();
        assert_eq!(all, data);
        let mut two = [0u8; 2];
        r.seek(SeekFrom::Start(5)).unwrap();
        r.read_exact(&mut two).unwrap();
        assert_eq!(two, [5, 6]);
        r.seek(SeekFrom::End(-1)).unwrap();
        assert_eq!(r.read(&mut two).unwrap(), 1, "short read at the end");
        assert_eq!(two[0], data[data.len() - 1]);
        assert_eq!(r.read(&mut two).unwrap(), 0);
        // fget_xattr_at at or past the fork end reads nothing.
        let len = data.len() as u32;
        assert_eq!(fget_xattr_at(&f, RESOURCE_FORK, &mut two, len).unwrap(), 0);
        assert_eq!(
            fget_xattr_at(&f, RESOURCE_FORK, &mut two, len + 10).unwrap(),
            0
        );
        assert!(r.seek(SeekFrom::Start(u64::from(u32::MAX) + 1)).is_err());
        assert!(r.seek(SeekFrom::Current(-(data.len() as i64) - 1)).is_err());
        // test: n/a - eviction is not observable without mincore; only the calls succeed.
        evict_cached(&f, 0).unwrap();
        std::fs::write(&p, &data).unwrap();
        evict_cached(&f, data.len() as u64).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
