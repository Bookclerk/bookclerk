//! Where the password that wraps `master.key` comes from.
//!
//! The auth password can reach a host three ways besides `[auth].password`,
//! checked in this order:
//!
//! 1. [`AUTH_PASSWORD_ENV`] (`BOOKCLERK_AUTH_PASSWORD`), the value itself.
//! 2. [`AUTH_PASSWORD_FILE_ENV`] (`BOOKCLERK_AUTH_PASSWORD_FILE`), the path
//!    of a file holding it: a Docker, Compose, Podman or Kubernetes secret
//!    mounted under `/run/secrets/`, so the value never sits in the
//!    container's environment where `docker inspect` or `/proc/<pid>/environ`
//!    shows it.
//! 3. A systemd credential named [`AUTH_PASSWORD_CREDENTIAL`]
//!    (`bookclerk-auth-password`) in `$CREDENTIALS_DIRECTORY`, which a unit
//!    hands in with `LoadCredential=` / `LoadCredentialEncrypted=` (sealed to
//!    the TPM where there is one).
//!
//! A source that is named but can't give a password (an unreadable, empty,
//! oversized or non-UTF-8 file) is an error, never "no password": falling
//! through would let a fresh install mint an unwrapped `master.key`. Setting
//! both the variable and the file variable is an error too, so an operator
//! never wonders which one wrapped the key. Surrounding whitespace is trimmed
//! from every source, as it always has been for the variable, so a trailing
//! newline in a secret file doesn't change the password.
//!
//! [`crate::Config::load`] resolves the environment's password once and keeps
//! it ([`EnvAuthPassword`]), so a secret file that changes while the process
//! runs (a rotated Kubernetes secret) can't hand later key operations a
//! different password than the one that opened `master.key`. A new value is
//! picked up on the next load: a restart or a daemon config reload.
//!
//! The file is the operator's own, as trusted as `config.toml`, so it is not
//! an untrusted-input boundary; it is still read with a size cap.

use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

use crate::error::{ConfigError, Result};

/// Env var holding the password that wraps `master.key`.
pub const AUTH_PASSWORD_ENV: &str = "BOOKCLERK_AUTH_PASSWORD";

/// Env var naming a file that holds the password that wraps `master.key`
/// (for Docker, Podman and Kubernetes secrets).
pub const AUTH_PASSWORD_FILE_ENV: &str = "BOOKCLERK_AUTH_PASSWORD_FILE";

/// The systemd credential name the password is read from, in
/// `$CREDENTIALS_DIRECTORY` (`LoadCredentialEncrypted=bookclerk-auth-password`).
pub const AUTH_PASSWORD_CREDENTIAL: &str = "bookclerk-auth-password";

/// The env var systemd sets to the service's credentials directory.
const CREDENTIALS_DIRECTORY_ENV: &str = "CREDENTIALS_DIRECTORY";

/// The largest password file read, in bytes. A password is far smaller;
/// this only stops a mistaken path (a disk image, `/dev/zero`) from being
/// read whole into memory.
const MAX_PASSWORD_FILE_BYTES: u64 = 4096;

/// The environment's auth password as [`crate::Config::load`] resolved it.
///
/// `Unresolved` for a [`crate::Config`] built without `load` (tests, embedded
/// defaults), which then reads the environment on each use instead.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum EnvAuthPassword {
    /// Not resolved yet: read the environment when asked.
    #[default]
    Unresolved,
    /// Resolved at load: the password, or `None` when no environment source
    /// was set.
    Resolved(Option<Zeroizing<String>>),
}

impl std::fmt::Debug for EnvAuthPassword {
    /// Never prints the password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unresolved => f.write_str("Unresolved"),
            Self::Resolved(Some(_)) => f.write_str("Resolved(<redacted>)"),
            Self::Resolved(None) => f.write_str("Resolved(None)"),
        }
    }
}

/// Resolve the auth password from the process environment: the variable,
/// the file it names, or the systemd credential, in that order.
///
/// Registers a found password for log redaction.
///
/// # Returns
///
/// The trimmed password, or `None` when no environment source is set (the
/// caller may then use `[auth].password`).
///
/// # Errors
///
/// [`ConfigError::Invalid`] when both [`AUTH_PASSWORD_ENV`] and
/// [`AUTH_PASSWORD_FILE_ENV`] are set, or when the named file or the
/// present credential can't be read, is larger than 4 KiB, isn't UTF-8 or
/// holds only whitespace. The message names the path, never the contents.
pub fn auth_password_from_env() -> Result<Option<String>> {
    let password = resolve_auth_password(|key| std::env::var_os(key))?;
    if let Some(pw) = &password {
        crate::redact::register_secret(pw);
    }
    Ok(password)
}

/// [`auth_password_from_env`] over an explicit variable lookup, so tests
/// don't touch the process environment.
///
/// # Arguments
///
/// * `lookup` - returns an environment variable's value, or `None` when unset.
///
/// # Errors
///
/// As [`auth_password_from_env`].
fn resolve_auth_password(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Option<String>> {
    let direct = match lookup(AUTH_PASSWORD_ENV) {
        None => None,
        Some(v) => {
            let v = Zeroizing::new(v.into_string().map_err(|_| {
                ConfigError::Invalid(format!("{AUTH_PASSWORD_ENV} is not valid UTF-8"))
            })?);
            Some(v.trim().to_string()).filter(|v| !v.is_empty())
        }
    };
    let file = lookup(AUTH_PASSWORD_FILE_ENV)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty());
    match (direct, file) {
        (Some(_), Some(_)) => Err(ConfigError::Invalid(format!(
            "both {AUTH_PASSWORD_ENV} and {AUTH_PASSWORD_FILE_ENV} are set; set only one"
        ))),
        (Some(pw), None) => Ok(Some(pw)),
        (None, Some(path)) => read_password_file(&path, AUTH_PASSWORD_FILE_ENV).map(Some),
        (None, None) => {
            let Some(dir) = lookup(CREDENTIALS_DIRECTORY_ENV)
                .map(PathBuf::from)
                .filter(|d| d.is_absolute())
            else {
                return Ok(None);
            };
            let path = dir.join(AUTH_PASSWORD_CREDENTIAL);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => read_password_file(&path, "the systemd credential").map(Some),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(ConfigError::Invalid(format!(
                    "reading the systemd credential {}: {err}",
                    path.display()
                ))),
            }
        }
    }
}

/// Read a password file: at most [`MAX_PASSWORD_FILE_BYTES`], UTF-8, and
/// not blank once trimmed.
///
/// # Arguments
///
/// * `path` - the file to read.
/// * `source` - what named the file, for error messages.
///
/// # Errors
///
/// [`ConfigError::Invalid`] naming `source` and `path` when the file can't
/// be read, is too large, isn't UTF-8 or is blank.
fn read_password_file(path: &Path, source: &str) -> Result<String> {
    let invalid = |why: String| {
        ConfigError::Invalid(format!(
            "{source} names {}, which {why}; it must hold the auth password",
            path.display()
        ))
    };
    let file = std::fs::File::open(path).map_err(|e| invalid(format!("can't be read ({e})")))?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_PASSWORD_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| invalid(format!("can't be read ({e})")))?;
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if len > MAX_PASSWORD_FILE_BYTES {
        return Err(invalid(format!(
            "is larger than {MAX_PASSWORD_FILE_BYTES} bytes"
        )));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid("is not UTF-8".into()))?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(invalid("is empty".into()));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A lookup over a fixed set of variables.
    fn env(vars: &[(&str, &OsString)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).clone()))
            .collect();
        move |key| map.get(key).cloned()
    }

    /// Writes `contents` to `name` in `dir` and returns its path as an env value.
    fn write(dir: &Path, name: &str, contents: &[u8]) -> OsString {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path.into_os_string()
    }

    #[test]
    fn nothing_set_is_no_password() {
        assert_eq!(resolve_auth_password(env(&[])).unwrap(), None);
    }

    #[test]
    fn the_variable_is_trimmed_and_blank_is_absent() {
        let pw = OsString::from("  hunter2\n");
        assert_eq!(
            resolve_auth_password(env(&[(AUTH_PASSWORD_ENV, &pw)])).unwrap(),
            Some("hunter2".into())
        );
        let blank = OsString::from("  ");
        assert_eq!(
            resolve_auth_password(env(&[(AUTH_PASSWORD_ENV, &blank)])).unwrap(),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_variable_is_an_error_not_a_lossy_password() {
        use std::os::unix::ffi::OsStringExt as _;
        let pw = OsString::from_vec(vec![0xff, 0xfe, b'a']);
        let err = resolve_auth_password(env(&[(AUTH_PASSWORD_ENV, &pw)])).unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");
    }

    #[test]
    fn the_resolved_password_never_debug_prints() {
        let resolved = EnvAuthPassword::Resolved(Some(Zeroizing::new("hunter2".into())));
        assert!(!format!("{resolved:?}").contains("hunter2"));
    }

    #[test]
    fn the_file_is_read_and_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "pw", b"from-a-secret\n");
        assert_eq!(
            resolve_auth_password(env(&[(AUTH_PASSWORD_FILE_ENV, &path)])).unwrap(),
            Some("from-a-secret".into())
        );
    }

    #[test]
    fn both_variable_and_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "pw", b"a");
        let pw = OsString::from("b");
        let err = resolve_auth_password(env(&[
            (AUTH_PASSWORD_ENV, &pw),
            (AUTH_PASSWORD_FILE_ENV, &path),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("set only one"), "{err}");
    }

    #[test]
    fn a_named_file_that_gives_no_password_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing").into_os_string();
        let empty = write(dir.path(), "empty", b" \n\t");
        let big = write(
            dir.path(),
            "big",
            &vec![b'a'; usize::try_from(MAX_PASSWORD_FILE_BYTES).unwrap() + 1],
        );
        let binary = write(dir.path(), "binary", &[0xff, 0xfe, b'x']);
        for path in [missing, empty, big, binary] {
            let err = resolve_auth_password(env(&[(AUTH_PASSWORD_FILE_ENV, &path)]))
                .expect_err("a named file must give a password or fail");
            assert!(
                err.to_string().contains(AUTH_PASSWORD_FILE_ENV),
                "error names the source: {err}"
            );
        }
    }

    #[test]
    fn errors_never_show_the_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let mut contents = b"secret-value".to_vec();
        contents.push(0xff);
        let path = write(dir.path(), "pw", &contents);
        let err = resolve_auth_password(env(&[(AUTH_PASSWORD_FILE_ENV, &path)])).unwrap_err();
        assert!(!err.to_string().contains("secret-value"), "{err}");
    }

    #[test]
    fn a_max_size_file_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "pw",
            &vec![b'a'; usize::try_from(MAX_PASSWORD_FILE_BYTES).unwrap()],
        );
        let pw = resolve_auth_password(env(&[(AUTH_PASSWORD_FILE_ENV, &path)]))
            .unwrap()
            .unwrap();
        assert_eq!(pw.len(), usize::try_from(MAX_PASSWORD_FILE_BYTES).unwrap());
    }

    #[test]
    fn the_systemd_credential_is_read_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().as_os_str().to_os_string();
        assert_eq!(
            resolve_auth_password(env(&[(CREDENTIALS_DIRECTORY_ENV, &creds)])).unwrap(),
            None,
            "no credential of that name is no password"
        );
        write(dir.path(), AUTH_PASSWORD_CREDENTIAL, b"sealed\n");
        assert_eq!(
            resolve_auth_password(env(&[(CREDENTIALS_DIRECTORY_ENV, &creds)])).unwrap(),
            Some("sealed".into())
        );
        let pw = OsString::from("direct");
        assert_eq!(
            resolve_auth_password(env(&[
                (CREDENTIALS_DIRECTORY_ENV, &creds),
                (AUTH_PASSWORD_ENV, &pw),
            ]))
            .unwrap(),
            Some("direct".into()),
            "the variable wins over the credential"
        );
    }

    #[test]
    fn an_empty_systemd_credential_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), AUTH_PASSWORD_CREDENTIAL, b"\n");
        let creds = dir.path().as_os_str().to_os_string();
        assert!(resolve_auth_password(env(&[(CREDENTIALS_DIRECTORY_ENV, &creds)])).is_err());
    }

    #[test]
    fn a_relative_credentials_directory_is_ignored() {
        let rel = OsString::from("relative/creds");
        assert_eq!(
            resolve_auth_password(env(&[(CREDENTIALS_DIRECTORY_ENV, &rel)])).unwrap(),
            None
        );
    }
}
