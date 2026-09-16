//! Receive host-passed work paths for fetch and (legacy) upload helpers.
//!
//! Audience: source plugin authors implementing `fetch_title`. Prefer
//! [`FetchWorkDir::open`] so a guest writes into the `cache_dir` the host put
//! on the RPC (a subdirectory of the guest's jail `TMPDIR`).
//!
//! [`crate::PLUGIN_FD_CHANNEL_ENV`] is **not** set by current hosts. If it is
//! set, these helpers still block on `recvmsg` for a future native SCM_RIGHTS
//! shortcut — do not set the env unless the host is actually sending an FD.
//!
//! See `docs/plugins.md` (guest jail / download cache).

#![cfg_attr(unix, allow(unsafe_code))]

use std::ops::Deref;
use std::path::{Path, PathBuf};

use crate::error::{Result, SdkError};
use crate::pass_fd::{fd_proc_path, recv_passed_fd, PLUGIN_FD_CHANNEL_ENV};
use crate::protocol::FetchTitleParams;

/// An open fetch work directory received from the host for one `fetchTitle` call.
///
/// Holds the descriptor open for its lifetime so `/proc/self/fd/N` (or
/// `/dev/fd/N`) stays valid for the duration of the call. When the side channel
/// is not armed, falls back to the `cache_dir` field on
/// [`FetchTitleParams`] from the RPC params (absolute path string the host
/// already prepared inside the guest jail).
pub struct FetchWorkDir {
    #[cfg(unix)]
    /// Kept open so `/proc/self/fd/N` stays valid for the `fetchTitle` call.
    _fd: Option<std::os::fd::OwnedFd>,
    /// Absolute work directory (FD proc path, or `cache_dir` from RPC params).
    path: PathBuf,
}

impl FetchWorkDir {
    /// Resolves the directory a `fetchTitle` call should write downloaded audio into.
    ///
    /// Uses the `cache_dir` field on `params` (hosts pass a jail-granted
    /// directory under the guest `TMPDIR`). When [`PLUGIN_FD_CHANNEL_ENV`] is
    /// set, receives one SCM_RIGHTS directory FD instead — hosts do not set
    /// this env; do not arm it without a matching host send.
    ///
    /// # Arguments
    ///
    /// * `params` - Host `fetchTitle` params; `cache_dir` is the work directory
    ///   unless the unused FD side channel is armed.
    ///
    /// # Returns
    ///
    /// A [`FetchWorkDir`] whose [`Self::path`] is valid for the rest of the RPC.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the side channel env is set but
    /// [`recv_passed_fd`] fails, or when FD → path mapping fails on this OS.
    pub fn open(params: &FetchTitleParams) -> Result<Self> {
        if std::env::var(PLUGIN_FD_CHANNEL_ENV).is_ok() {
            let fd = recv_passed_fd()?;
            return owned_fd_path(fd).map(|(owned, path)| Self {
                #[cfg(unix)]
                _fd: Some(owned),
                path,
            });
        }
        Ok(Self {
            #[cfg(unix)]
            _fd: None,
            path: PathBuf::from(&params.cache_dir),
        })
    }

    /// Absolute filesystem path of the fetch work directory.
    ///
    /// On Unix with a passed FD this is typically `/proc/self/fd/N` (Linux) or
    /// `/dev/fd/N` (macOS/BSD). Keep `self` alive while writing under this path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Deref for FetchWorkDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

/// An open upload file received from the host for one `putFile` call.
///
/// Same FD-lifetime rules as [`FetchWorkDir`]: keep the value alive while
/// reading the path so the `/proc/self/fd/N` symlink remains valid.
pub struct UploadFile {
    #[cfg(unix)]
    /// Kept open so `/proc/self/fd/N` stays valid while reading the upload.
    _fd: Option<std::os::fd::OwnedFd>,
    /// Absolute path of the file the host wants uploaded.
    path: PathBuf,
}

impl UploadFile {
    /// Resolves the local file path for a `putFile` upload.
    ///
    /// Uses `local_path` from the RPC params. When [`PLUGIN_FD_CHANNEL_ENV`] is
    /// set, receives one SCM_RIGHTS file FD instead — hosts do not set this
    /// env (destinations stream bytes). Do not arm it without a matching send.
    ///
    /// # Arguments
    ///
    /// * `local_path` - Optional absolute path from host params; ignored when
    ///   the FD channel is active. Must be `Some` and non-empty when the
    ///   channel is inactive.
    ///
    /// # Returns
    ///
    /// An [`UploadFile`] whose [`Self::path`] points at the bytes to upload.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when neither an FD nor a usable `local_path` is
    /// available, or when receiving/mapping the FD fails.
    pub fn open(local_path: Option<&str>) -> Result<Self> {
        if std::env::var(PLUGIN_FD_CHANNEL_ENV).is_ok() {
            let fd = recv_passed_fd()?;
            return owned_fd_path(fd).map(|(owned, path)| Self {
                #[cfg(unix)]
                _fd: Some(owned),
                path,
            });
        }
        let path = local_path
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                SdkError::message(
                    "put_file requires a side-channel descriptor or an explicit local_path",
                )
            })?;
        Ok(Self {
            #[cfg(unix)]
            _fd: None,
            path,
        })
    }

    /// Absolute filesystem path of the file the host wants uploaded.
    ///
    /// Keep `self` alive for the duration of the read so a passed FD path stays
    /// valid.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Deref for UploadFile {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

/// Resolves the directory a `fetchTitle` call should write into.
///
/// Thin wrapper around [`FetchWorkDir::open`].
///
/// # Arguments
///
/// * `params` - Host `fetchTitle` params (see [`FetchWorkDir::open`]).
///
/// # Returns
///
/// Open work directory for this RPC.
///
/// # Errors
///
/// Propagates [`FetchWorkDir::open`] failures.
pub fn fetch_work_dir(params: &FetchTitleParams) -> Result<FetchWorkDir> {
    FetchWorkDir::open(params)
}

/// Resolves the local file path for a `putFile` call.
///
/// Thin wrapper around [`UploadFile::open`].
///
/// # Arguments
///
/// * `local_path` - Optional path from host params (see [`UploadFile::open`]).
///
/// # Returns
///
/// Open upload file handle for this RPC.
///
/// # Errors
///
/// Propagates [`UploadFile::open`] failures.
pub fn upload_file_path(local_path: Option<&str>) -> Result<UploadFile> {
    UploadFile::open(local_path)
}

#[cfg(unix)]
/// Takes ownership of a host-passed FD and maps it to `/proc/self/fd/N` (or `/dev/fd/N`).
///
/// # Errors
///
/// This function does not fail; it always returns the owned descriptor and
/// `/dev/fd/<n>` path for the received file descriptor.
fn owned_fd_path(fd: i32) -> Result<(std::os::fd::OwnedFd, PathBuf)> {
    use std::os::fd::{AsRawFd, FromRawFd};

    // SAFETY: fd was received from the host for this RPC and is not used elsewhere.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let path = fd_proc_path(owned.as_raw_fd());
    Ok((owned, path))
}

#[cfg(not(unix))]
fn owned_fd_path(_fd: i32) -> Result<((), PathBuf)> {
    Err(SdkError::message(
        "descriptor side channel is not supported on this platform".into(),
    ))
}
