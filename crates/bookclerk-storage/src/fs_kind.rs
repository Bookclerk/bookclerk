//! What kind of file system a local path lives on, so the local backend only
//! leans on hard links, inode numbers and atomic renames where the file
//! system keeps their promises.
//!
//! Network file systems (NFS, SMB/CIFS, AFP, WebDAV, 9p, library file
//! systems) and FUSE mounts break several of them:
//!
//! - A retried NFS `LINK` or `RENAME` can answer "already exists" or "not
//!   found" after the first attempt succeeded, so the answer no longer says
//!   whether the operation happened.
//! - SMB, AFP and many FUSE file systems have no hard links, or fake them.
//! - Inode numbers and status-change times can change after a remount or a
//!   cache eviction, so a signature built from them reports changes that
//!   never happened.
//!
//! A library folder can hold another mount below it, so callers classify the
//! directory an operation touches, not the library root.

use std::path::Path;

/// The kind of file system behind a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    /// A disk the operating system manages directly (ext4, XFS, Btrfs, ZFS,
    /// APFS, NTFS, FAT and the like).
    Local,
    /// A network or library file system, or a Windows network drive.
    Remote,
    /// A FUSE (or macFUSE) mount: its guarantees depend on the program
    /// behind it, so it is treated like a remote file system.
    Userspace,
    /// The kind could not be determined; treated like a remote file system.
    Unknown,
}

impl FsKind {
    /// True when hard links, `rename` and inode identity behave as on a
    /// local disk, so a hard link can publish a file atomically and an
    /// inode number can be part of a change signature.
    #[must_use]
    pub fn is_local(self) -> bool {
        match self {
            Self::Local => true,
            Self::Remote | Self::Userspace | Self::Unknown => false,
        }
    }
}

/// Classifies the file system that holds `path`, which must exist.
///
/// Returns [`FsKind::Unknown`] when the platform gives no answer.
#[must_use]
pub fn classify(path: &Path) -> FsKind {
    imp::classify(path)
}

/// Classifies the nearest existing ancestor of `path` (itself included), so
/// a target that does not exist yet is classified by the directory it will
/// be created in.
#[must_use]
pub fn classify_nearest(path: &Path) -> FsKind {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate.exists() {
            return classify(candidate);
        }
        current = candidate.parent();
    }
    FsKind::Unknown
}

/// The device id of the file system holding `path`, where the platform has
/// one. Two paths with different ids are on different file systems, so a
/// rename or hard link between them cannot work.
#[must_use]
pub fn device_of(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|meta| meta.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(target_os = "linux")]
mod imp {
    //! `statfs(2)` magic numbers from `linux/magic.h` and the file systems'
    //! own headers.

    use super::FsKind;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// `f_type` values of network and library file systems.
    const REMOTE: &[i64] = &[
        0x6969,      // NFS
        0x517B,      // SMB (smbfs)
        0xFF53_4D42, // CIFS
        0xFE53_4D42, // SMB2
        0x7375_7245, // Coda
        0x5346_414F, // AFS (kAFS)
        0x6B41_4653, // AFS (OpenAFS)
        0x00C3_6400, // Ceph
        0x0102_1997, // 9p (v9fs)
        0x564C,      // NCP
        0x0116_1970, // GFS2
        0x7461_636F, // OCFS2
        0x0BD0_0BD0, // Lustre
        0x1366_1366, // OrangeFS
        0x4750_4653, // GPFS
        0x786F_4256, // VirtualBox shared folders
    ];
    /// `f_type` of FUSE mounts (sshfs, rclone, mergerfs, s3fs, ...).
    const FUSE: i64 = 0x6573_5546;

    /// Classifies `path` from `statfs(2)`; [`FsKind::Unknown`] when the call
    /// fails.
    // The one FFI call this module needs; the SAFETY notes cover each block.
    #[allow(unsafe_code)]
    pub(super) fn classify(path: &Path) -> FsKind {
        let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return FsKind::Unknown;
        };
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::zeroed();
        // SAFETY: `c_path` is a valid NUL-terminated string and `stat` points
        // to writable memory the size of `libc::statfs`.
        let rc = unsafe { libc::statfs(c_path.as_ptr(), stat.as_mut_ptr()) };
        if rc != 0 {
            return FsKind::Unknown;
        }
        // SAFETY: `statfs` returned 0, so it filled the struct.
        let stat = unsafe { stat.assume_init() };
        // `f_type`'s width and signedness differ by architecture; widen it.
        #[allow(clippy::unnecessary_cast)]
        let magic = (stat.f_type as i64) & 0xFFFF_FFFF;
        if magic == FUSE {
            FsKind::Userspace
        } else if REMOTE.contains(&magic) {
            FsKind::Remote
        } else {
            FsKind::Local
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
mod imp {
    //! BSD `statfs(2)`: `MNT_LOCAL` says whether the data is stored locally,
    //! and the type name picks out FUSE mounts, which set it.

    use super::FsKind;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// Classifies `path` from `statfs(2)`; [`FsKind::Unknown`] when the call
    /// fails.
    // The one FFI call this module needs; the SAFETY notes cover each block.
    #[allow(unsafe_code)]
    pub(super) fn classify(path: &Path) -> FsKind {
        let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return FsKind::Unknown;
        };
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::zeroed();
        // SAFETY: `c_path` is a valid NUL-terminated string and `stat` points
        // to writable memory the size of `libc::statfs`.
        let rc = unsafe { libc::statfs(c_path.as_ptr(), stat.as_mut_ptr()) };
        if rc != 0 {
            return FsKind::Unknown;
        }
        // SAFETY: `statfs` returned 0, so it filled the struct.
        let stat = unsafe { stat.assume_init() };
        // SAFETY: `f_fstypename` is a NUL-terminated array inside the struct.
        let name = unsafe { std::ffi::CStr::from_ptr(stat.f_fstypename.as_ptr()) }
            .to_string_lossy()
            .to_ascii_lowercase();
        if name.contains("fuse") {
            return FsKind::Userspace;
        }
        #[allow(clippy::unnecessary_cast)]
        let local = (stat.f_flags as u64) & (libc::MNT_LOCAL as u64) != 0;
        if local {
            FsKind::Local
        } else {
            FsKind::Remote
        }
    }
}

#[cfg(windows)]
mod imp {
    //! `GetDriveTypeW` on the volume that holds the path; a UNC path is a
    //! network share.

    use super::FsKind;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Path, Prefix};
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};

    /// `DRIVE_REMOTE` from `winbase.h`.
    const DRIVE_REMOTE: u32 = 4;
    /// `DRIVE_NO_ROOT_DIR` from `winbase.h`.
    const DRIVE_NO_ROOT_DIR: u32 = 1;
    /// `DRIVE_UNKNOWN` from `winbase.h`.
    const DRIVE_UNKNOWN: u32 = 0;

    /// Classifies `path` from its UNC prefix or its volume's drive type;
    /// [`FsKind::Unknown`] when Windows cannot say.
    // The two FFI calls this module needs; the SAFETY notes cover each block.
    #[allow(unsafe_code)]
    pub(super) fn classify(path: &Path) -> FsKind {
        if let Some(Component::Prefix(prefix)) = path.components().next() {
            if matches!(prefix.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..)) {
                return FsKind::Remote;
            }
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut volume = [0u16; 1024];
        let len = u32::try_from(volume.len()).unwrap_or(u32::MAX);
        // SAFETY: `wide` is NUL-terminated and `volume` holds `len` u16s.
        let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), volume.as_mut_ptr(), len) };
        if ok == 0 {
            return FsKind::Unknown;
        }
        // SAFETY: `GetVolumePathNameW` wrote a NUL-terminated path.
        match unsafe { GetDriveTypeW(volume.as_ptr()) } {
            DRIVE_REMOTE => FsKind::Remote,
            DRIVE_UNKNOWN | DRIVE_NO_ROOT_DIR => FsKind::Unknown,
            _ => FsKind::Local,
        }
    }
}

/// The NTFS/ReFS file index of the file at `path`, Windows' counterpart of
/// an inode number: a file that replaces another by rename gets a new one.
///
/// Returns `None` when the file cannot be opened or Windows does not report
/// an index. Blocks on the open; call it off the async runtime.
#[cfg(windows)]
// One FFI call on a handle `File` owns; the SAFETY note covers it.
#[allow(unsafe_code)]
pub(crate) fn file_index(path: &std::path::Path) -> Option<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let file = std::fs::File::open(path).ok()?;
    // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid value of
    // this plain-data struct; the call only writes into it.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is open for the lifetime of `file` and `info` is a
    // valid, writable BY_HANDLE_FILE_INFORMATION.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
    if ok == 0 {
        return None;
    }
    Some((u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    windows
)))]
mod imp {
    //! No classification on other platforms: everything is
    //! [`FsKind::Unknown`], so the backend takes the cautious path.

    use super::FsKind;
    use std::path::Path;

    /// Always [`FsKind::Unknown`].
    pub(super) fn classify(_path: &Path) -> FsKind {
        FsKind::Unknown
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_temp_dir_is_classified_and_a_missing_child_follows_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let kind = classify(dir.path());
        assert_eq!(
            classify_nearest(&dir.path().join("not/there/yet.m4b")),
            kind
        );
        #[cfg(target_os = "linux")]
        assert_ne!(kind, FsKind::Unknown);
    }

    #[test]
    fn only_local_disks_are_trusted() {
        assert!(FsKind::Local.is_local());
        for kind in [FsKind::Remote, FsKind::Userspace, FsKind::Unknown] {
            assert!(!kind.is_local());
        }
    }

    #[cfg(unix)]
    #[test]
    fn device_ids_match_within_one_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"a").unwrap();
        assert_eq!(device_of(&dir.path().join("a")), device_of(dir.path()));
    }
}
