//! Pass a per-fetch directory to a jailed guest over a side channel.
//!
//! The guest's filesystem allowlist is fixed at spawn, so it cannot be granted
//! the whole download cache. Instead the host opens one work directory per
//! `fetch_title`, sends its descriptor with `SCM_RIGHTS`, and the guest writes
//! through that descriptor alone.

#![cfg(unix)]
#![allow(unsafe_code)]
#![allow(dead_code)] // retained for native fetch/upload side-channel once v2 sessions wire SCM_RIGHTS

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::{PluginError, Result};

/// Send an open directory handle for `dir` to the guest's side channel.
pub fn send_fetch_dir(channel: &UnixStream, dir: &Path) -> Result<()> {
    let dir_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(dir)
        .map_err(|err| {
            PluginError::message(format!(
                "could not open fetch directory {}: {err}",
                dir.display()
            ))
        })?;
    send_one_fd(channel.as_raw_fd(), dir_file.as_raw_fd()).map_err(|err| {
        PluginError::message(format!(
            "could not pass fetch directory {} to the guest: {err}",
            dir.display()
        ))
    })
}

/// Send an open file handle for `path` to the guest's side channel.
pub fn send_upload_file(channel: &UnixStream, path: &Path) -> Result<()> {
    let file = OpenOptions::new().read(true).open(path).map_err(|err| {
        PluginError::message(format!(
            "could not open upload file {}: {err}",
            path.display()
        ))
    })?;
    send_one_fd(channel.as_raw_fd(), file.as_raw_fd()).map_err(|err| {
        PluginError::message(format!(
            "could not pass upload file {} to the guest: {err}",
            path.display()
        ))
    })
}

/// Send an open SQLite database file (read+write, created if missing).
#[allow(clippy::suspicious_open_options)] // create(true) is required on first library open
pub fn send_database_file(channel: &UnixStream, path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
        .map_err(|err| {
            PluginError::message(format!(
                "could not open database file {}: {err}",
                path.display()
            ))
        })?;
    send_one_fd(channel.as_raw_fd(), file.as_raw_fd()).map_err(|err| {
        PluginError::message(format!(
            "could not pass database file {} to the guest: {err}",
            path.display()
        ))
    })
}

/// Sends one SCM_RIGHTS file descriptor over a Unix socket as a single `sendmsg`.
fn send_one_fd(socket: RawFd, fd: RawFd) -> io::Result<()> {
    let mut byte = [0u8];
    let iov = [libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    }];
    let fd_size = std::mem::size_of::<RawFd>();
    let mut cmsg = vec![0u8; space_for_one_fd(fd_size)];
    let msg = libc::msghdr {
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: iov.as_ptr().cast_mut(),
        msg_iovlen: 1,
        msg_control: cmsg.as_mut_ptr().cast(),
        msg_controllen: cmsg.len() as _,
        msg_flags: 0,
    };

    // SAFETY: `cmsg` is sized with CMSG_SPACE and we write one SCM_RIGHTS entry.
    unsafe {
        let hdr = libc::CMSG_FIRSTHDR(&msg);
        if hdr.is_null() {
            return Err(io::Error::other(
                "could not build SCM_RIGHTS control message",
            ));
        }
        (*hdr).cmsg_level = libc::SOL_SOCKET;
        (*hdr).cmsg_type = libc::SCM_RIGHTS;
        (*hdr).cmsg_len = libc::CMSG_LEN(fd_size as _) as _;
        std::ptr::copy_nonoverlapping(
            &fd as *const RawFd as *const u8,
            libc::CMSG_DATA(hdr),
            fd_size,
        );
    }

    // SAFETY: msghdr points at stack/local buffers for the duration of the call.
    let sent = unsafe { libc::sendmsg(socket, &msg, 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Control-message buffer size (`CMSG_SPACE`) for one file descriptor.
fn space_for_one_fd(fd_size: usize) -> usize {
    // CMSG_SPACE is a macro in C; libc exposes it on most Unix targets.
    unsafe { libc::CMSG_SPACE(fd_size as u32) as usize }
}
