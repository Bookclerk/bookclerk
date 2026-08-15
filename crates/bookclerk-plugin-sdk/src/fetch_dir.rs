//! Receive host-passed descriptors for fetch and upload side channels.
//!
//! Audience: source and destination plugin authors implementing
//! [`crate::PluginRoot`] storefront `fetch_title` or destination `put`. Prefer these helpers over reading
//! `cache_dir` / `local_path` strings alone when the host sets
//! [`crate::PLUGIN_FD_CHANNEL_ENV`] — the jail may not grant a usable path
//! string, only an SCM_RIGHTS descriptor.
//!
//! See `docs/plugins.md` (guest jail / download cache) and
//! [`crate::recv_passed_fd`].

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
    /// When [`PLUGIN_FD_CHANNEL_ENV`] is set, receives one SCM_RIGHTS directory
    /// FD from the host and maps it via [`fd_proc_path`]. Otherwise uses the
    /// `cache_dir` field on `params` as an absolute path string.
    ///
    /// # Arguments
    ///
    /// * `params` - Host `fetchTitle` params; `cache_dir` is used only when the
    ///   FD side channel is inactive.
    ///
    /// # Returns
    ///
    /// A [`FetchWorkDir`] whose [`Self::path`] is valid for the rest of the RPC.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the side channel is required but
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
    /// Prefer the FD side channel when [`PLUGIN_FD_CHANNEL_ENV`] is set.
    /// Otherwise requires a non-empty `local_path` from the RPC params.
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
