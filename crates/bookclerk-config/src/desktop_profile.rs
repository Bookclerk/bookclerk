//! The desktop app's per-OS-user profile: who the window is.
//!
//! Each person who uses the desktop app on a computer does so from their own
//! OS account, and that account's standard configuration directory holds a
//! `profile.toml` naming the patron the window acts as (see
//! `docs/adr/desktop-window-identity.md`). The OS sign-in is what proves who
//! is at the keyboard; the profile only remembers which patron that is.
//!
//! The file holds the patron's own session token for the library on this
//! computer, so it is a secret: it is written owner-only, never logged, and
//! never put on argv. Nothing else is in it. A computer that is only a clerk
//! of another library has no patrons of its own and needs no profile.
//!
//! The format is version 1:
//!
//! ```toml
//! version = 1
//! mode = "branch"
//! session = "0b6e1f8a-3c4d-4e5f-8a9b-0c1d2e3f4a5b"
//! ```
//!
//! [`parse_profile`] is sans-IO so the `desktop_profile` fuzz target drives
//! the same code the window does; [`read_profile`], [`write_profile`] and
//! [`remove_profile`] do the file work.

use std::fmt;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;

/// The profile's file name inside the OS user's config directory.
pub const PROFILE_FILE_NAME: &str = "profile.toml";

/// The only profile format version this build reads and writes.
pub const PROFILE_VERSION: u32 = 1;

/// Largest profile file read, in bytes. A real one is under 100 bytes.
pub const MAX_PROFILE_BYTES: usize = 4096;

/// The patron session a profile names: the raw token of a portal session
/// cookie on the library this computer runs (never its hash).
///
/// Always a lowercase hyphenated UUID, the shape the host mints. `Debug`
/// and `Display` print a placeholder so the token cannot reach a log line.
#[derive(Clone, PartialEq, Eq)]
pub struct ProfileSession(String);

impl ProfileSession {
    /// Validates `raw` as a session token.
    ///
    /// # Arguments
    ///
    /// * `raw` - The token text.
    ///
    /// # Returns
    ///
    /// The token, or `None` when it is not a lowercase hyphenated UUID
    /// (36 characters: 8-4-4-4-12 lowercase hex digits).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let bytes = raw.as_bytes();
        if bytes.len() != 36 {
            return None;
        }
        let shaped = bytes.iter().enumerate().all(|(i, &b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        });
        shaped.then(|| Self(raw.to_owned()))
    }

    /// The raw token, for the window's cookie and for hashing.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProfileSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProfileSession(<redacted>)")
    }
}

/// What kind of computer the profile belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileMode {
    /// The computer runs its own library (a branch, with or without a
    /// parent), and the window acts as one of its patrons.
    Branch,
}

impl ProfileMode {
    /// The value written as `mode`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Branch => "branch",
        }
    }
}

/// A parsed `profile.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopProfile {
    /// What kind of computer this is.
    pub mode: ProfileMode,
    /// The patron session the window opens with.
    pub session: ProfileSession,
}

impl DesktopProfile {
    /// A branch profile for `session`.
    ///
    /// # Arguments
    ///
    /// * `session` - The patron session the window should open with.
    ///
    /// # Returns
    ///
    /// The profile.
    #[must_use]
    pub fn branch(session: ProfileSession) -> Self {
        Self {
            mode: ProfileMode::Branch,
            session,
        }
    }

    /// The file's text, in the version 1 format.
    ///
    /// # Returns
    ///
    /// Three `key = value` lines; [`parse_profile`] reads them back to an
    /// equal profile.
    #[must_use]
    pub fn to_toml(&self) -> String {
        format!(
            "version = {PROFILE_VERSION}\nmode = \"{}\"\nsession = \"{}\"\n",
            self.mode.as_str(),
            self.session.expose()
        )
    }
}

/// Why a profile file was not accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    /// The file is larger than [`MAX_PROFILE_BYTES`].
    #[error("the desktop profile is larger than {MAX_PROFILE_BYTES} bytes")]
    TooLarge,
    /// The file is not valid UTF-8 TOML.
    #[error("the desktop profile is not valid TOML")]
    Syntax,
    /// `version` is missing or not [`PROFILE_VERSION`].
    #[error("the desktop profile's version is not {PROFILE_VERSION}")]
    Version,
    /// `mode` is missing or not a known mode.
    #[error("the desktop profile's mode is not \"branch\"")]
    Mode,
    /// `session` is missing or not a session token.
    #[error("the desktop profile's session is not a session token")]
    Session,
    /// The file has a key this version does not define.
    #[error("the desktop profile has a key this version does not know")]
    UnknownKey,
}

/// Parses a profile file's bytes.
///
/// # Arguments
///
/// * `bytes` - The file's contents.
///
/// # Returns
///
/// The profile.
///
/// # Errors
///
/// Returns a [`ProfileError`] naming the first problem: the file too large,
/// not TOML, the wrong version, an unknown mode, a malformed session or an
/// extra key.
pub fn parse_profile(bytes: &[u8]) -> Result<DesktopProfile, ProfileError> {
    if bytes.len() > MAX_PROFILE_BYTES {
        return Err(ProfileError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ProfileError::Syntax)?;
    let table: toml::Table = toml::from_str(text).map_err(|_| ProfileError::Syntax)?;
    match table.get("version").and_then(toml::Value::as_integer) {
        Some(v) if v == i64::from(PROFILE_VERSION) => {}
        _ => return Err(ProfileError::Version),
    }
    let mode = match table.get("mode").and_then(toml::Value::as_str) {
        Some("branch") => ProfileMode::Branch,
        _ => return Err(ProfileError::Mode),
    };
    let session = table
        .get("session")
        .and_then(toml::Value::as_str)
        .and_then(ProfileSession::parse)
        .ok_or(ProfileError::Session)?;
    if table
        .keys()
        .any(|key| !matches!(key.as_str(), "version" | "mode" | "session"))
    {
        return Err(ProfileError::UnknownKey);
    }
    Ok(DesktopProfile { mode, session })
}

/// Where this OS user's profile lives: `profile.toml` in the standard
/// configuration directory (`$XDG_CONFIG_HOME/bookclerk` on Linux,
/// `~/Library/Application Support/app.Bookclerk.bookclerk` on macOS,
/// `%APPDATA%\Bookclerk\bookclerk\config` on Windows).
///
/// # Returns
///
/// The path, or `None` when the OS reports no home directory.
#[must_use]
pub fn profile_path() -> Option<PathBuf> {
    ProjectDirs::from("app", "Bookclerk", "bookclerk")
        .map(|dirs| dirs.config_dir().join(PROFILE_FILE_NAME))
}

/// Reads the profile at `path`.
///
/// # Arguments
///
/// * `path` - The profile file.
///
/// # Returns
///
/// `Ok(None)` when there is no file; the profile when it parses.
///
/// # Errors
///
/// Returns an I/O error when the file exists but cannot be read, or
/// `InvalidData` (carrying the [`ProfileError`]) when it does not parse.
pub fn read_profile(path: &Path) -> std::io::Result<Option<DesktopProfile>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    parse_profile(&bytes)
        .map(Some)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}

/// Writes `profile` to `path` atomically, owner-only.
///
/// Creates the parent directory (`0700` on Unix) when missing, writes a
/// `0600` staging file beside the target and moves it into place with
/// [`crate::replace_file`], so a reader sees the old profile or the new one,
/// never half of either. On Windows the file sits in the user's own
/// `%APPDATA%`, which only that user can read.
///
/// # Arguments
///
/// * `path` - The profile file.
/// * `profile` - What to write.
///
/// # Errors
///
/// Returns an I/O error when the directory, the staging file or the move
/// fails; the staging file is removed on failure.
pub fn write_profile(path: &Path, profile: &DesktopProfile) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.exists() {
        std::fs::create_dir_all(parent)?;
        harden(parent, 0o700)?;
    }
    let staging = crate::settings::staging_toml_path(parent, path);
    let written = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&staging)?;
        file.write_all(profile.to_toml().as_bytes())?;
        file.sync_all()?;
        harden(&staging, 0o600)?;
        crate::replace_file(&staging, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    written
}

/// Deletes the profile at `path`; a missing file is not an error.
///
/// # Arguments
///
/// * `path` - The profile file.
///
/// # Errors
///
/// Returns an I/O error when the file exists but cannot be removed.
pub fn remove_profile(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Sets Unix permission bits `mode` on `path`; nothing on other systems.
fn harden(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    let _ = (path, mode);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0b6e1f8a-3c4d-4e5f-8a9b-0c1d2e3f4a5b";

    fn profile() -> DesktopProfile {
        DesktopProfile::branch(ProfileSession::parse(TOKEN).unwrap())
    }

    #[test]
    fn a_written_profile_reads_back_equal() {
        let text = profile().to_toml();
        assert_eq!(parse_profile(text.as_bytes()), Ok(profile()));
    }

    #[test]
    fn each_problem_is_named() {
        let cases: [(&str, ProfileError); 7] = [
            ("version = ", ProfileError::Syntax),
            ("mode = \"branch\"", ProfileError::Version),
            ("version = 2\nmode = \"branch\"", ProfileError::Version),
            ("version = 1\nmode = \"host\"", ProfileError::Mode),
            ("version = 1\nmode = \"branch\"", ProfileError::Session),
            (
                "version = 1\nmode = \"branch\"\nsession = \"0B6E1F8A-3C4D-4E5F-8A9B-0C1D2E3F4A5B\"",
                ProfileError::Session,
            ),
            (
                "version = 1\nmode = \"branch\"\nsession = \"0b6e1f8a-3c4d-4e5f-8a9b-0c1d2e3f4a5b\"\npatron = 1",
                ProfileError::UnknownKey,
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_profile(text.as_bytes()), Err(expected), "{text}");
        }
        assert_eq!(
            parse_profile(&vec![b' '; MAX_PROFILE_BYTES + 1]),
            Err(ProfileError::TooLarge)
        );
        assert_eq!(parse_profile(&[0xff, 0xfe]), Err(ProfileError::Syntax));
    }

    #[test]
    fn the_session_never_prints() {
        let shown = format!("{:?}", profile());
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn write_read_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bookclerk").join(PROFILE_FILE_NAME);
        assert_eq!(read_profile(&path).unwrap(), None);
        write_profile(&path, &profile()).unwrap();
        assert_eq!(read_profile(&path).unwrap(), Some(profile()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(file & 0o777, 0o600);
            let dir_mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700);
        }
        std::fs::write(&path, "version = 9").unwrap();
        let err = read_profile(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        remove_profile(&path).unwrap();
        remove_profile(&path).unwrap();
        assert_eq!(read_profile(&path).unwrap(), None);
    }

    #[test]
    fn the_path_is_the_profile_file_in_a_config_directory() {
        if let Some(path) = profile_path() {
            assert!(path.ends_with(PROFILE_FILE_NAME));
        }
    }
}
