//! Receive the per-fetch work directory the host passes over a side channel.

#![cfg_attr(unix, allow(unsafe_code))]

use std::ops::Deref;
use std::path::{Path, PathBuf};

use crate::error::{Result, SdkError};
use crate::protocol::FetchTitleParams;

/// Descriptor the host leaves open for the fetch-directory side channel.
const PLUGIN_FD_CHANNEL: i32 = 3;

/// Environment variable naming [`PLUGIN_FD_CHANNEL`].
const PLUGIN_FD_CHANNEL_ENV: &str = "BOOKCLERK_PLUGIN_FD_CHANNEL";

/// An open fetch work directory received from the host.
///
/// Holds the descriptor open for its lifetime so `/proc/self/fd/N` (or
/// `/dev/fd/N`) stays valid for the duration of the `fetch_title` call.
pub struct FetchWorkDir {
    #[cfg(unix)]
    _fd: Option<std::os::fd::OwnedFd>,
    path: PathBuf,
}

impl FetchWorkDir {
    /// Resolve the directory a `fetch_title` call should write into.
    ///
    /// Jailed guests receive an open directory descriptor immediately before the RPC
    /// arrives. Tests and unconfined development fall back to the `cache_dir` string.
    pub fn open(params: &FetchTitleParams) -> Result<Self> {
        match std::env::var(PLUGIN_FD_CHANNEL_ENV) {
            Ok(raw) => {
                let channel: i32 = raw.parse().map_err(|err| {
                    SdkError::message(format!("invalid {PLUGIN_FD_CHANNEL_ENV}={raw:?}: {err}"))
                })?;
                if channel != PLUGIN_FD_CHANNEL {
                    return Err(SdkError::message(format!(
                        "unsupported fetch-directory channel fd {channel} (expected {})",
                        PLUGIN_FD_CHANNEL
                    )));
                }
                recv_fetch_dir(channel)
            }
            Err(_) => Ok(Self {
                #[cfg(unix)]
                _fd: None,
                path: PathBuf::from(&params.cache_dir),
            }),
        }
    }

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

/// Resolve the directory a `fetch_title` call should write into.
pub fn fetch_work_dir(params: &FetchTitleParams) -> Result<FetchWorkDir> {
    FetchWorkDir::open(params)
}

#[cfg(unix)]
fn recv_fetch_dir(channel: i32) -> Result<FetchWorkDir> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let received = recv_one_fd(channel).map_err(SdkError::Io)?;
    // SAFETY: the host sent one directory descriptor and we own it now.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(received) };
    let path = fd_proc_path(owned.as_raw_fd());
    Ok(FetchWorkDir {
        _fd: Some(owned),
        path,
    })
}

#[cfg(not(unix))]
fn recv_fetch_dir(_channel: i32) -> Result<FetchWorkDir> {
    Err(SdkError::message(
        "fetch-directory descriptors are not supported on this platform".into(),
    ))
}

#[cfg(unix)]
use std::io;

#[cfg(unix)]
fn recv_one_fd(socket: i32) -> io::Result<i32> {
    let mut byte = [0u8];
    let mut iov = [libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    }];
    let fd_size = std::mem::size_of::<i32>();
    let mut cmsg = vec![0u8; unsafe { libc::CMSG_SPACE(fd_size as u32) as usize }];
    let mut msg = libc::msghdr {
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: iov.as_mut_ptr(),
        msg_iovlen: 1,
        msg_control: cmsg.as_mut_ptr().cast(),
        msg_controllen: cmsg.len() as _,
        msg_flags: 0,
    };

    // SAFETY: msghdr points at stack/local buffers for the duration of the call.
    let received = unsafe { libc::recvmsg(socket, &mut msg, 0) };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: we sized the control buffer with CMSG_SPACE and inspect one header.
    unsafe {
        let hdr = libc::CMSG_FIRSTHDR(&msg);
        if hdr.is_null() {
            return Err(io::Error::other(
                "fetch directory message carried no descriptor",
            ));
        }
        if (*hdr).cmsg_level != libc::SOL_SOCKET || (*hdr).cmsg_type != libc::SCM_RIGHTS {
            return Err(io::Error::other(
                "fetch directory message was not SCM_RIGHTS",
            ));
        }
        let mut fd = 0i32;
        std::ptr::copy_nonoverlapping(
            libc::CMSG_DATA(hdr),
            &mut fd as *mut i32 as *mut u8,
            fd_size,
        );
        if fd < 0 {
            return Err(io::Error::other("fetch directory descriptor was invalid"));
        }
        Ok(fd)
    }
}

#[cfg(unix)]
fn fd_proc_path(fd: i32) -> PathBuf {
    #[cfg(target_os = "linux")]
    let path = format!("/proc/self/fd/{fd}");
    #[cfg(target_os = "macos")]
    let path = format!("/dev/fd/{fd}");
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    let path = format!("/dev/fd/{fd}");
    PathBuf::from(path)
}
