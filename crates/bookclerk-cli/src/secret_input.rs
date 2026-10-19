//! Reading a secret from stdin without echoing it.
//!
//! On a terminal the prompt goes to stderr and the line is read with the
//! terminal's echo turned off (restored afterwards, also on error or an
//! interrupt such as Ctrl+C), so the secret never appears on screen or in
//! a scrollback. Piped stdin is read as before: the whole input, of which
//! the caller keeps the first line.

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
///
/// Dropping it restores the saved settings, but an interrupt (Ctrl+C, or a
/// console control event on Windows) ends the process without unwinding.
/// So while echo is off an interrupt handler restores the terminal first and
/// then lets the default handling go on: on Unix it reinstates the previous
/// disposition and re-raises the signal (the exit status stays, for
/// example, 130 for SIGINT); on Windows it returns `FALSE` so the next
/// handler (by default `ExitProcess`) runs. A stop from the terminal
/// (Ctrl+Z, `SIGTSTP`) restores echo too before the process stops, and
/// turns it off again once the shell resumes it, so the shell is never left
/// without echo. Drop removes the handler again.
///
/// One `EchoOff` exists at a time: the CLI reads one secret at a time, from
/// one thread, and `new` refuses a second one rather than corrupt the saved
/// state.
struct EchoOff {
    /// Keeps the type from being built outside `new`.
    _private: (),
}

/// Set while an [`EchoOff`] is alive and its saved state is valid.
#[cfg(any(unix, windows))]
static ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Held by the one live [`EchoOff`].
#[cfg(any(unix, windows))]
static CLAIMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Claims the single [`EchoOff`] slot.
#[cfg(any(unix, windows))]
fn claim() -> io::Result<()> {
    if CLAIMED.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return Err(io::Error::other("a secret is already being read"));
    }
    Ok(())
}

/// Releases the slot claimed by [`claim`].
#[cfg(any(unix, windows))]
fn release() {
    CLAIMED.store(false, std::sync::atomic::Ordering::Release);
}

#[cfg(unix)]
mod platform {
    //! termios and signal handling.

    use std::io;
    use std::sync::atomic::Ordering;

    use super::ARMED;

    /// State written by `EchoOff::new` before [`ARMED`] is set and read only
    /// while it is set (by the interrupt handler and by drop).
    struct Saved<T>(std::cell::UnsafeCell<std::mem::MaybeUninit<T>>);

    // SAFETY: written once by the single `EchoOff` owner before `ARMED` is
    // published with `Release`; every reader loads `ARMED` with `Acquire` first
    // and nothing writes it again until the owner has cleared `ARMED` and
    // removed the handler.
    #[allow(unsafe_code)]
    unsafe impl<T> Sync for Saved<T> {}

    impl<T> Saved<T> {
        /// An empty slot.
        const fn new() -> Self {
            Self(std::cell::UnsafeCell::new(std::mem::MaybeUninit::uninit()))
        }

        /// The slot, for writing before arming and reading while armed.
        fn ptr(&self) -> *mut T {
            self.0.get().cast()
        }
    }

    /// Signals that would end or stop the process with echo still off.
    const SIGNALS: [libc::c_int; 5] = [
        libc::SIGINT,
        libc::SIGTERM,
        libc::SIGHUP,
        libc::SIGQUIT,
        libc::SIGTSTP,
    ];

    /// The terminal settings to restore.
    static TERMIOS: Saved<libc::termios> = Saved::new();
    /// The settings with echo off, applied again after a stop.
    static QUIET: Saved<libc::termios> = Saved::new();
    /// The dispositions in place before ours, one per [`SIGNALS`] entry.
    static PREVIOUS: Saved<[libc::sigaction; 5]> = Saved::new();
    /// Which [`SIGNALS`] entries got our handler (an ignored signal keeps
    /// being ignored).
    static INSTALLED: Saved<[bool; 5]> = Saved::new();

    /// Restores echo, then either stops the process and turns echo off
    /// again once it is resumed (`SIGTSTP`), or reinstates the previous
    /// disposition and re-raises (every other signal).
    ///
    /// Calls only async-signal-safe functions (`tcsetattr`, `sigaction`,
    /// `raise`). The signal is blocked while this runs, so a re-raised one
    /// is delivered to the previous disposition once it returns. A stop
    /// uses `SIGSTOP`, which cannot be blocked, so the process stops inside
    /// the handler and carries on here after `SIGCONT`.
    #[allow(unsafe_code)]
    extern "C" fn on_signal(signal: libc::c_int) {
        if !ARMED.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: armed, so the saved state is complete and unchanging.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, TERMIOS.ptr());
            if signal == libc::SIGTSTP {
                libc::raise(libc::SIGSTOP);
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, QUIET.ptr());
                return;
            }
            if let Some(i) = SIGNALS.iter().position(|s| *s == signal) {
                libc::sigaction(signal, &(*PREVIOUS.ptr())[i], std::ptr::null_mut());
            }
            libc::raise(signal);
        }
    }

    /// Saves stdin's terminal settings, installs the interrupt handlers and
    /// clears `ECHO`.
    #[allow(unsafe_code)]
    pub(super) fn arm() -> io::Result<()> {
        // SAFETY: the caller holds the single claim, so nothing else reads
        // or writes the saved state until `ARMED` is set below. `termios`
        // and `sigaction` are plain data filled by the calls.
        unsafe {
            if libc::tcgetattr(libc::STDIN_FILENO, TERMIOS.ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut quiet = *TERMIOS.ptr();
            quiet.c_lflag &= !(libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL);
            QUIET.ptr().write(quiet);
            let previous = &mut *PREVIOUS.ptr();
            let installed = &mut *INSTALLED.ptr();
            for (i, signal) in SIGNALS.iter().enumerate() {
                installed[i] = false;
                if libc::sigaction(*signal, std::ptr::null(), &mut previous[i]) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            ARMED.store(true, Ordering::Release);
            for (i, signal) in SIGNALS.iter().enumerate() {
                if previous[i].sa_sigaction == libc::SIG_IGN {
                    continue;
                }
                let mut ours: libc::sigaction = std::mem::zeroed();
                ours.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
                libc::sigemptyset(&mut ours.sa_mask);
                if libc::sigaction(*signal, &ours, std::ptr::null_mut()) != 0 {
                    let err = io::Error::last_os_error();
                    disarm();
                    return Err(err);
                }
                installed[i] = true;
            }
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, QUIET.ptr()) != 0 {
                let err = io::Error::last_os_error();
                disarm();
                return Err(err);
            }
        }
        Ok(())
    }

    /// Restores the terminal settings and the previous dispositions.
    #[allow(unsafe_code)]
    pub(super) fn disarm() {
        // SAFETY: called by the claim holder after `arm` saved the state;
        // the handler only reads it.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, TERMIOS.ptr());
            let previous = &*PREVIOUS.ptr();
            let installed = &*INSTALLED.ptr();
            for (i, signal) in SIGNALS.iter().enumerate() {
                if installed[i] {
                    libc::sigaction(*signal, &previous[i], std::ptr::null_mut());
                }
            }
        }
        ARMED.store(false, Ordering::Release);
    }
}

#[cfg(windows)]
mod platform {
    //! Console mode and console control handling.

    use std::io;
    use std::sync::atomic::{AtomicU32, Ordering};

    use windows_sys::core::BOOL;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleCtrlHandler, SetConsoleMode, ENABLE_ECHO_INPUT,
        STD_INPUT_HANDLE,
    };

    use super::ARMED;

    /// The console input mode to restore.
    static MODE: AtomicU32 = AtomicU32::new(0);

    /// Restores the console mode and lets the next handler run (`FALSE`),
    /// which by default ends the process.
    #[allow(unsafe_code)]
    unsafe extern "system" fn on_control(_event: u32) -> BOOL {
        if ARMED.load(Ordering::Acquire) {
            // SAFETY: the standard input handle is owned by the process.
            unsafe {
                SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), MODE.load(Ordering::Acquire));
            }
        }
        0
    }

    /// Saves the console input mode, installs the control handler and
    /// clears `ENABLE_ECHO_INPUT`.
    #[allow(unsafe_code)]
    pub(super) fn arm() -> io::Result<()> {
        // SAFETY: the standard input handle is owned by the process; the
        // mode is written into a local.
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            let mut saved = 0u32;
            if GetConsoleMode(handle, &mut saved) == 0 {
                return Err(io::Error::last_os_error());
            }
            MODE.store(saved, Ordering::Release);
            ARMED.store(true, Ordering::Release);
            if SetConsoleCtrlHandler(Some(on_control), 1) == 0 {
                let err = io::Error::last_os_error();
                ARMED.store(false, Ordering::Release);
                return Err(err);
            }
            if SetConsoleMode(handle, saved & !ENABLE_ECHO_INPUT) == 0 {
                let err = io::Error::last_os_error();
                disarm();
                return Err(err);
            }
        }
        Ok(())
    }

    /// Restores the console mode and removes the control handler.
    #[allow(unsafe_code)]
    pub(super) fn disarm() {
        // SAFETY: restores the mode read in `arm` on the same handle and
        // removes the handler `arm` added.
        unsafe {
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), MODE.load(Ordering::Acquire));
            SetConsoleCtrlHandler(Some(on_control), 0);
        }
        ARMED.store(false, Ordering::Release);
    }
}

#[cfg(any(unix, windows))]
impl EchoOff {
    /// Saves the terminal state, arms the interrupt handler and turns echo
    /// off.
    fn new() -> io::Result<Self> {
        claim()?;
        if let Err(err) = platform::arm() {
            release();
            return Err(err);
        }
        Ok(Self { _private: () })
    }
}

#[cfg(any(unix, windows))]
impl Drop for EchoOff {
    fn drop(&mut self) {
        platform::disarm();
        release();
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
