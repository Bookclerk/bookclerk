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

/// Close every descriptor above stdio except those the host wired deliberately.
///
/// A sweep that could not be performed is not a sweep that can be assumed, so
/// this fails closed like the rest of the launcher.
#[cfg(unix)]
pub fn close_inherited(preserve: &[i32]) -> Result<(), String> {
    // Nested native jails run inside the outer Landlock domain, which often
    // cannot list `/proc/self/fd`. `close_range` over the gaps around the
    // preserved fds (stdio, plus an inherited socket-proxy directory fd) does
    // not need a directory listing.
    if close_range_preserving(preserve) {
        return Ok(());
    }

    let listed = list_open().ok_or_else(|| {
        "cannot enumerate open descriptors, so cannot promise the guest inherits none".to_string()
    })?;
    for fd in listed
        .into_iter()
        .filter(|fd| *fd > libc::STDERR_FILENO && !preserve.contains(fd))
    {
        // SAFETY: closing a descriptor this process owns. A number that is
        // already closed just answers `EBADF`, and nothing reopens between the
        // listing and here.
        unsafe { libc::close(fd) };
    }
    Ok(())
}

/// Inclusive `close_range` gaps above stderr, skipping every fd in `preserve`.
///
/// Linux production uses this from [`close_range_preserving`]. Other Unix
/// builds only compile it under `cfg(test)` (gap arithmetic is OS-independent).
#[cfg(any(target_os = "linux", all(test, unix)))]
fn close_gaps_after_stdio(preserve: &[i32]) -> Vec<(u32, u32)> {
    let mut keep: Vec<u32> = preserve
        .iter()
        .copied()
        .filter(|fd| *fd > libc::STDERR_FILENO)
        .filter_map(|fd| u32::try_from(fd).ok())
        .collect();
    keep.sort_unstable();
    keep.dedup();
    let mut gaps = Vec::new();
    let mut start = u32::try_from(libc::STDERR_FILENO).unwrap_or(2) + 1;
    for fd in keep {
        if fd > start {
            gaps.push((start, fd - 1));
        }
        start = fd.saturating_add(1);
        if start == 0 {
            return gaps;
        }
    }
    gaps.push((start, u32::MAX));
    gaps
}

/// Windows inherits only the handles a spawn names, and this one names stdio.
#[cfg(not(unix))]
pub fn close_inherited(_preserve: &[i32]) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "linux")]
/// Linux `close_range` over the gaps in [`close_gaps_after_stdio`]; false means
/// the listing fallback must run.
fn close_range_preserving(preserve: &[i32]) -> bool {
    close_gaps_after_stdio(preserve)
        .into_iter()
        .all(|(first, last)| close_range_inclusive(first, last))
}

#[cfg(target_os = "linux")]
/// One inclusive `close_range` syscall; empty or inverted ranges are no-ops.
fn close_range_inclusive(first: u32, last: u32) -> bool {
    if first > last {
        return true;
    }
    // SAFETY: a raw syscall taking three scalars. The ranges skip every
    // preserved fd, including stdio and an inherited socket-proxy dir fd.
    let rc = unsafe { libc::syscall(libc::SYS_close_range, first, last, 0) };
    rc == 0
}

/// No equivalent outside Linux; macOS and the BSDs are served by the listing.
#[cfg(all(unix, not(target_os = "linux")))]
fn close_range_preserving(_preserve: &[i32]) -> bool {
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

    #[test]
    fn stdio_preserve_is_stdio_only() {
        assert_eq!(close_gaps_after_stdio(&[]), vec![(3, u32::MAX)]);
        assert_eq!(close_gaps_after_stdio(&[0, 1, 2]), vec![(3, u32::MAX)]);
        assert_eq!(close_gaps_after_stdio(&[2]), vec![(3, u32::MAX)]);
    }

    #[test]
    fn extra_preserve_fd_uses_two_close_range_gaps() {
        // Nested native SOCKET_PROXY dir fd (typically a small number after stdio).
        assert_eq!(
            close_gaps_after_stdio(&[0, 1, 2, 7]),
            vec![(3, 6), (8, u32::MAX)]
        );
    }
}
