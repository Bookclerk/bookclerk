//! Hand over no descriptor the host did not mean to hand over.
//!
//! Both sandbox backends confine by path, and a descriptor that is already open
//! is past the path check for good: the guest reads it without naming anything,
//! so no allowlist can take it back. Whatever is still open across the handoff
//! is therefore a hole straight through the jail.
//!
//! Nothing leaks today, because Rust opens files `O_CLOEXEC`. That is a property
//! of every library the host links, though, and one that would have to be
//! re-checked on every dependency bump; sweeping here makes it a property of the
//! jail instead.
//!
//! stdin, stdout and stderr stay: they are the JSON-RPC stream and the log, and
//! the host wired them up deliberately.

#![cfg_attr(unix, allow(unsafe_code))] // close, and the close_range syscall.

/// Close every descriptor above stdio.
///
/// A sweep that could not be performed is not a sweep that can be assumed, so
/// this fails closed like the rest of the launcher.
#[cfg(unix)]
pub fn close_inherited() -> Result<(), String> {
    if close_range_above_stdio() {
        return Ok(());
    }

    let listed = list_open().ok_or_else(|| {
        "cannot enumerate open descriptors, so cannot promise the guest inherits none".to_string()
    })?;
    for fd in listed.into_iter().filter(|fd| *fd > libc::STDERR_FILENO) {
        // SAFETY: closing a descriptor this process owns. A number that is
        // already closed just answers `EBADF`, and nothing reopens between the
        // listing and here.
        unsafe { libc::close(fd) };
    }
    Ok(())
}

/// Windows inherits only the handles a spawn names, and this one names stdio.
#[cfg(not(unix))]
pub fn close_inherited() -> Result<(), String> {
    Ok(())
}

/// Close everything above stderr in one call.
///
/// `close_range` arrived in Linux 5.9. It is exact and needs no listing, so it
/// is worth asking for before reaching into the filesystem for one.
#[cfg(target_os = "linux")]
fn close_range_above_stdio() -> bool {
    let first = libc::c_uint::try_from(libc::STDERR_FILENO).unwrap_or(2) + 1;
    // SAFETY: a raw syscall taking three scalars. The range starts above stdio,
    // so the descriptors the host handed over are not in it.
    let rc = unsafe { libc::syscall(libc::SYS_close_range, first, libc::c_uint::MAX, 0) };
    rc == 0
}

/// No equivalent outside Linux; macOS and the BSDs are served by the listing.
#[cfg(all(unix, not(target_os = "linux")))]
fn close_range_above_stdio() -> bool {
    false
}

/// The descriptors this process holds, as the kernel reports them.
///
/// Linux publishes the listing at `/proc/self/fd` and macOS at `/dev/fd`; on
/// Linux the second is a symlink to the first, so one order covers both.
///
/// The listing is read to the end before anything is closed, because the
/// directory handle is itself an entry in it.
#[cfg(unix)]
fn list_open() -> Option<Vec<std::os::fd::RawFd>> {
    let dir = std::fs::read_dir("/proc/self/fd")
        .or_else(|_| std::fs::read_dir("/dev/fd"))
        .ok()?;
    Some(
        dir.filter_map(|entry| {
            let name = entry.ok()?.file_name();
            name.to_str()?.parse().ok()
        })
        .collect(),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The listing has to include a descriptor that was just opened, or the
    /// sweep would run over an empty set and report success having done nothing.
    #[test]
    fn the_listing_sees_a_freshly_opened_file() {
        use std::os::fd::AsRawFd;

        let file = tempfile::NamedTempFile::new().expect("tempfile");
        let fd = file.as_file().as_raw_fd();
        let listed = list_open().expect("this platform must publish a descriptor listing");
        assert!(
            listed.contains(&fd),
            "listing {listed:?} is missing the file at fd {fd}"
        );
    }
}
