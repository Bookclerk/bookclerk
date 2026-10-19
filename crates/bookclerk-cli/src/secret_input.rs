//! Reading a secret from stdin without echoing it.
//!
//! On a terminal the prompt goes to stderr and the line is read with the
//! terminal's echo turned off (restored afterwards, also on error), so the
//! secret never appears on screen or in a scrollback. Piped stdin is read
//! as before: the whole input, of which the caller keeps the first line.

use std::io::{self, BufRead as _, IsTerminal as _, Read as _, Write as _};

use zeroize::Zeroizing;

/// Reads a secret: on a terminal, prompts with `prompt` on stderr and reads
/// one line without echo; otherwise reads all of stdin.
///
/// # Arguments
///
/// * `prompt` - Text shown before the input on a terminal (for example
///   `"enrollment link: "`).
///
/// # Returns
///
/// The raw text read, zeroized on drop. The caller trims it and keeps the
/// first line.
///
/// # Errors
///
/// Returns the read error, or the error of switching the terminal's echo
/// off (the secret is then not read, so it is never echoed).
pub fn read_secret_stdin(prompt: &str) -> io::Result<Zeroizing<String>> {
    let mut text = Zeroizing::new(String::new());
    let stdin = io::stdin();
    if stdin.is_terminal() {
        let echo = EchoOff::new()?;
        eprint!("{prompt}");
        let _ = io::stderr().flush();
        let read = stdin.lock().read_line(&mut text);
        drop(echo);
        // The Enter key was not echoed either.
        eprintln!();
        read?;
    } else {
        stdin.lock().read_to_string(&mut text)?;
    }
    Ok(text)
}

/// Turns the terminal's echo off for stdin until dropped.
struct EchoOff {
    /// The terminal settings to restore.
    #[cfg(unix)]
    saved: libc::termios,
    /// The console mode to restore.
    #[cfg(windows)]
    saved: u32,
}

#[cfg(unix)]
impl EchoOff {
    /// Saves stdin's terminal settings and clears `ECHO`.
    #[allow(unsafe_code)]
    fn new() -> io::Result<Self> {
        // SAFETY: `termios` is plain data; `tcgetattr` fills it for the
        // stdin descriptor, which stays open for the whole process.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut saved) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut quiet = saved;
        quiet.c_lflag &= !(libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL);
        // SAFETY: `quiet` is a valid `termios` derived from the current one.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &quiet) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { saved })
    }
}

#[cfg(unix)]
impl Drop for EchoOff {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: restores the settings read in `new` on the same descriptor.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
    }
}

#[cfg(windows)]
impl EchoOff {
    /// Saves the console input mode and clears `ENABLE_ECHO_INPUT`.
    #[allow(unsafe_code)]
    fn new() -> io::Result<Self> {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, STD_INPUT_HANDLE,
        };
        let mut saved = 0u32;
        // SAFETY: the standard input handle is owned by the process; the
        // mode is written into a local.
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            if GetConsoleMode(handle, &mut saved) == 0 {
                return Err(io::Error::last_os_error());
            }
            if SetConsoleMode(handle, saved & !ENABLE_ECHO_INPUT) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { saved })
    }
}

#[cfg(windows)]
impl Drop for EchoOff {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::{GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE};
        // SAFETY: restores the mode read in `new` on the same handle.
        unsafe {
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), self.saved);
        }
    }
}

#[cfg(not(any(unix, windows)))]
impl EchoOff {
    /// No terminal API here: refuse rather than echo the secret.
    fn new() -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot hide terminal input on this platform; pipe the secret on stdin",
        ))
    }
}
