//! Receive descriptors the host passes over a Unix SCM_RIGHTS side channel.
//!
//! Audience: guest authors that need a directory or file the jail cannot expose
//! as a normal path string (fetch work dirs, `putFile` sources). The host opens
//! FD [`PLUGIN_FD_CHANNEL`] (always `3`), sets [`PLUGIN_FD_CHANNEL_ENV`], and
//! sends one descriptor per RPC that needs it. Pair with [`crate::FetchWorkDir`]
//! / [`crate::UploadFile`].
//!
//! Non-Unix platforms return [`crate::SdkError`] from [`recv_passed_fd`] — there
//! is no SCM_RIGHTS equivalent in this SDK.

#![cfg_attr(unix, allow(unsafe_code))]

use std::path::PathBuf;

use crate::error::{Result, SdkError};

/// Fixed descriptor number the host leaves open for the plugin side channel.
///
/// Always `3` (after stdin/stdout/stderr). Guests must not close or reuse this
/// FD for other purposes while the channel is active.
pub const PLUGIN_FD_CHANNEL: i32 = 3;

/// Environment variable the host sets to the decimal form of [`PLUGIN_FD_CHANNEL`].
///
/// Presence of this variable (value must parse to [`PLUGIN_FD_CHANNEL`]) arms
/// [`crate::FetchWorkDir::open`] / [`crate::UploadFile::open`] to call
/// [`recv_passed_fd`] instead of trusting path strings in RPC params.
pub const PLUGIN_FD_CHANNEL_ENV: &str = "BOOKCLERK_PLUGIN_FD_CHANNEL";

/// Receives one file descriptor from the host side channel (SCM_RIGHTS).
///
/// Blocks until the host sends a control message on [`PLUGIN_FD_CHANNEL`].
/// Call once per RPC that expects a passed FD (fetch dir or upload file).
///
/// # Returns
///
/// Raw OS file descriptor owned by the caller. Prefer wrapping it immediately
/// (see [`crate::FetchWorkDir`] / [`crate::UploadFile`]) so Drop closes it.
///
/// # Errors
///
/// Returns [`SdkError`] when [`PLUGIN_FD_CHANNEL_ENV`] is unset or not equal to
/// [`PLUGIN_FD_CHANNEL`], when `recvmsg` fails, or when the platform does not
/// support the side channel (non-Unix).
#[cfg(unix)]
pub fn recv_passed_fd() -> Result<i32> {
    let channel = std::env::var(PLUGIN_FD_CHANNEL_ENV)
        .map_err(|_| SdkError::message(format!("{PLUGIN_FD_CHANNEL_ENV} is not set")))?;
    let channel: i32 = channel.parse().map_err(|err| {
        SdkError::message(format!(
            "invalid {PLUGIN_FD_CHANNEL_ENV}={channel:?}: {err}"
        ))
    })?;
    if channel != PLUGIN_FD_CHANNEL {
        return Err(SdkError::message(format!(
            "unsupported side-channel fd {channel} (expected {PLUGIN_FD_CHANNEL})"
        )));
    }
    recv_one_fd(channel).map_err(SdkError::Io)
}

/// Non-Unix stub: descriptor side channels are not supported.
///
/// # Errors
///
/// Always returns [`SdkError`] explaining the platform limitation.
#[cfg(not(unix))]
pub fn recv_passed_fd() -> Result<i32> {
    Err(SdkError::message(
        "descriptor side channel is not supported on this platform".into(),
    ))
}

/// Builds the OS path that refers to an open descriptor by number.
///
/// Linux uses `/proc/self/fd/{fd}`; macOS and other Unix use `/dev/fd/{fd}`.
/// The path is only valid while the descriptor remains open in this process.
///
/// # Arguments
///
/// * `fd` - Open raw file descriptor previously received from the host.
///
/// # Returns
///
/// PathBuf that can be passed to std / tokio filesystem APIs for as long as
/// `fd` stays open.
#[must_use]
pub fn fd_proc_path(fd: i32) -> PathBuf {
    #[cfg(target_os = "linux")]
    let path = format!("/proc/self/fd/{fd}");
    #[cfg(target_os = "macos")]
    let path = format!("/dev/fd/{fd}");
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    let path = format!("/dev/fd/{fd}");
    PathBuf::from(path)
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
                "side-channel message carried no descriptor",
            ));
        }
        if (*hdr).cmsg_level != libc::SOL_SOCKET || (*hdr).cmsg_type != libc::SCM_RIGHTS {
            return Err(io::Error::other("side-channel message was not SCM_RIGHTS"));
        }
        let mut fd = 0i32;
        std::ptr::copy_nonoverlapping(
            libc::CMSG_DATA(hdr),
            &mut fd as *mut i32 as *mut u8,
            fd_size,
        );
        if fd < 0 {
            return Err(io::Error::other("side-channel descriptor was invalid"));
        }
        Ok(fd)
    }
}
