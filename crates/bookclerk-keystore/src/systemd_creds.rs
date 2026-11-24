//! A key store sealed with `systemd-creds`, for headless Linux.
//!
//! A server usually has no Secret Service, so the OS keychain can't hold its
//! key. `systemd-creds` can instead seal the key to the machine (its TPM 2.0
//! chip where there is one, and the machine's host key) and to the user the
//! daemon runs as. The sealed credential sits beside `master.key` as
//! `master-key-<item>.cred`. A copy of the files directory on another machine
//! can't unseal it.
//!
//! [`SystemdCredsVault`] seals and unseals by running the system's own
//! `systemd-creds --user`, which works without root from systemd 256. The
//! envelope travels on the tool's stdin and stdout, never on its argv or in
//! its environment.
//!
//! An administrator can instead seal the key themselves and have the service
//! unit hand it in (`LoadCredentialEncrypted=` or `ImportCredential=` named
//! [`CREDENTIAL_NAME`]). That works on any systemd with credentials. The
//! unit's copy is read from `$CREDENTIALS_DIRECTORY` when no sealed file of
//! this install's own exists.
//!
//! A hosted database's DEK-sealed API credential uses the same store with
//! its own names ([`ItemKind::DatabaseCredential`]): files
//! `database-credential-<item>.cred` sealed under
//! [`DATABASE_CREDENTIAL_NAME`], so neither kind's unit copy or sealed file
//! is ever read as the other.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use crate::{
    check_len, ErrorStore, ItemKind, KeychainItemId, KeystoreError, KeystoreErrorKind, Result,
    Vault,
};

/// The credential name the key is sealed under, and the name a service
/// unit hands it in as (`LoadCredentialEncrypted=bookclerk-master-key`).
pub const CREDENTIAL_NAME: &str = "bookclerk-master-key";

/// The credential name a database credential is sealed under, and the name
/// a service unit hands one in as
/// (`LoadCredentialEncrypted=bookclerk-database-credential`).
pub const DATABASE_CREDENTIAL_NAME: &str = "bookclerk-database-credential";

/// Where the system's `systemd-creds` is looked for, in order. Only fixed
/// paths: a `PATH` the daemon inherited never chooses the tool that sees
/// the key.
const PROGRAM_PATHS: [&str; 2] = ["/usr/bin/systemd-creds", "/bin/systemd-creds"];

/// How long one `systemd-creds` run may take before it is stopped. A TPM
/// operation takes well under a second; this only bounds a hung tool.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest stderr excerpt kept in an error message, in characters.
const STDERR_EXCERPT_CHARS: usize = 300;

/// A key store that seals items with `systemd-creds`.
///
/// Items live in one directory (the files directory) as
/// `master-key-<item>.cred` (or `database-credential-<item>.cred` for
/// [`ItemKind::DatabaseCredential`]). See the module docs.
#[derive(Debug, Clone)]
pub struct SystemdCredsVault {
    /// The directory sealed items are written to.
    dir: PathBuf,
    /// The `systemd-creds` binary, or `None` when it isn't installed.
    program: Option<PathBuf>,
    /// The service's credentials directory (`$CREDENTIALS_DIRECTORY`), when
    /// systemd started the process with credentials.
    credentials_dir: Option<PathBuf>,
    /// The kind of item this store seals, which names its files and its
    /// credential.
    kind: ItemKind,
}

impl SystemdCredsVault {
    /// A store sealing items into `dir` with the system's `systemd-creds`,
    /// reading a unit-provided copy from `$CREDENTIALS_DIRECTORY` when set.
    ///
    /// # Arguments
    ///
    /// * `dir` - the directory sealed items go in (the files directory).
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let program = PROGRAM_PATHS
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_file());
        let credentials_dir = std::env::var_os("CREDENTIALS_DIRECTORY")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute());
        Self::with_program(dir, program, credentials_dir)
    }

    /// A store with an explicit tool and credentials directory, for tests
    /// and for systems that install `systemd-creds` elsewhere.
    ///
    /// # Arguments
    ///
    /// * `dir` - the directory sealed items go in.
    /// * `program` - the `systemd-creds` binary, or `None` for none.
    /// * `credentials_dir` - the unit's credentials directory, if any.
    #[must_use]
    pub fn with_program(
        dir: impl Into<PathBuf>,
        program: Option<PathBuf>,
        credentials_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            dir: dir.into(),
            program,
            credentials_dir,
            kind: ItemKind::MasterKey,
        }
    }

    /// The same store, sealing items of `kind` (its own file prefix and
    /// credential name) instead of master-key items.
    ///
    /// # Arguments
    ///
    /// * `kind` - the kind of item to seal.
    #[must_use]
    pub fn for_kind(mut self, kind: ItemKind) -> Self {
        self.kind = kind;
        self
    }

    /// Where `item` is sealed: `<dir>/<prefix>-<item>.cred`, the prefix
    /// being `master-key` or `database-credential` ([`ItemKind::file_prefix`]).
    ///
    /// # Arguments
    ///
    /// * `item` - the item id the pointer file names.
    #[must_use]
    pub fn sealed_path(&self, item: KeychainItemId) -> PathBuf {
        self.dir
            .join(format!("{}-{item}.cred", self.kind.file_prefix()))
    }

    /// The copy the service unit handed in, when there is one.
    #[must_use]
    pub fn unit_credential(&self) -> Option<PathBuf> {
        self.credentials_dir
            .as_ref()
            .map(|dir| dir.join(self.kind.credential_name()))
            .filter(|path| path.is_file())
    }

    /// Reads the unit's copy, if there is one.
    fn read_unit_credential(&self) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let Some(path) = self.unit_credential() else {
            return Ok(None);
        };
        let bytes = Zeroizing::new(std::fs::read(&path).map_err(|err| {
            KeystoreError::new(
                KeystoreErrorKind::AccessDenied,
                format!(
                    "could not read the unit credential {}: {err}",
                    path.display()
                ),
            )
        })?);
        check_len(&bytes)?;
        Ok(Some(bytes))
    }

    /// Unseals the credential at `sealed`.
    fn decrypt(&self, sealed: &Path) -> Result<Zeroizing<Vec<u8>>> {
        let input = sealed.as_os_str().to_owned();
        let name = format!("--name={}", self.kind.credential_name());
        let out = self.run(
            &[
                "--user".as_ref(),
                "decrypt".as_ref(),
                name.as_ref(),
                &input,
                "-".as_ref(),
            ],
            &[],
            KeystoreErrorKind::AccessDenied,
        )?;
        check_len(&out).map_err(|_| {
            KeystoreError::new(
                KeystoreErrorKind::Corrupt,
                format!("{} unsealed to more than a key envelope", sealed.display()),
            )
        })?;
        Ok(out)
    }

    /// Seals `bytes`, returning the credential's bytes.
    fn encrypt(&self, bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        let name = format!("--name={}", self.kind.credential_name());
        self.run(
            &[
                "--user".as_ref(),
                "encrypt".as_ref(),
                name.as_ref(),
                "-".as_ref(),
                "-".as_ref(),
            ],
            bytes,
            KeystoreErrorKind::Unavailable,
        )
    }

    /// Runs `systemd-creds` with `args`, feeding `stdin` and returning its
    /// stdout. The child gets an empty environment apart from a fixed
    /// `PATH`, and is stopped after [`RUN_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// [`KeystoreErrorKind::Unavailable`] when the tool isn't installed or
    /// can't be started; `failure` (with a stderr excerpt) when it exits
    /// unsuccessfully or times out.
    fn run(
        &self,
        args: &[&std::ffi::OsStr],
        stdin: &[u8],
        failure: KeystoreErrorKind,
    ) -> Result<Zeroizing<Vec<u8>>> {
        let program = self.program.as_ref().ok_or_else(|| {
            KeystoreError::new(
                KeystoreErrorKind::Unavailable,
                "systemd-creds is not installed",
            )
        })?;
        let mut child = Command::new(program)
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| {
                KeystoreError::new(
                    KeystoreErrorKind::Unavailable,
                    format!("could not start {}: {err}", program.display()),
                )
            })?;
        let stdout = child.stdout.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut out = Zeroizing::new(Vec::new());
                let _ = pipe.read_to_end(&mut out);
                out
            })
        });
        let stderr = child.stderr.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut out = Vec::new();
                let _ = pipe.read_to_end(&mut out);
                out
            })
        });
        if let Some(mut pipe) = child.stdin.take() {
            // A tool that exits early closes the pipe; its exit status
            // reports why, so a write error here is not the error to keep.
            let _ = pipe.write_all(stdin);
        }
        let deadline = Instant::now() + RUN_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(err) => {
                    let _ = child.kill();
                    return Err(KeystoreError::new(
                        failure,
                        format!("systemd-creds could not be waited for: {err}"),
                    ));
                }
            }
        };
        let out = stdout
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        let err = stderr
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        match status {
            Some(status) if status.success() => Ok(out),
            Some(status) => Err(KeystoreError::new(
                failure,
                format!("systemd-creds {status}: {}", excerpt(&err)),
            )),
            None => Err(KeystoreError::new(
                failure,
                format!(
                    "systemd-creds did not finish in {} s",
                    RUN_TIMEOUT.as_secs()
                ),
            )),
        }
    }
}

/// The last non-empty line of `stderr`, cut to [`STDERR_EXCERPT_CHARS`].
fn excerpt(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("no message")
        .trim();
    line.chars().take(STDERR_EXCERPT_CHARS).collect()
}

/// Writes `bytes` to a temp file beside `path` (mode 0600 on Unix) and
/// renames it over `path`.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    let written = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

impl Vault for SystemdCredsVault {
    fn get(&self, item: KeychainItemId) -> Result<Zeroizing<Vec<u8>>> {
        self.get_item(item).map_err(name_store)
    }

    fn set(&self, item: KeychainItemId, bytes: &[u8]) -> Result<()> {
        self.set_item(item, bytes).map_err(name_store)
    }

    fn delete(&self, item: KeychainItemId) -> Result<()> {
        self.delete_item(item).map_err(name_store)
    }
}

/// Names `systemd-creds` as the store an error came from.
fn name_store(err: KeystoreError) -> KeystoreError {
    err.in_store(ErrorStore::SystemdCreds)
}

/// The [`Vault`] operations, before errors are labelled with the store.
impl SystemdCredsVault {
    /// Reads `item`: this install's sealed file, else the unit's copy.
    fn get_item(&self, item: KeychainItemId) -> Result<Zeroizing<Vec<u8>>> {
        let sealed = self.sealed_path(item);
        if sealed.is_file() {
            return self.decrypt(&sealed);
        }
        if let Some(bytes) = self.read_unit_credential()? {
            return Ok(bytes);
        }
        Err(KeystoreError::new(
            KeystoreErrorKind::NotFound,
            format!(
                "neither {} nor a unit credential named {} exists",
                sealed.display(),
                self.kind.credential_name()
            ),
        ))
    }

    /// Seals `bytes` as `item`, or adopts an identical unit credential.
    fn set_item(&self, item: KeychainItemId, bytes: &[u8]) -> Result<()> {
        check_len(bytes)?;
        let sealed = self.sealed_path(item);
        // The unit already hands in exactly this envelope: nothing to seal.
        if !sealed.is_file() {
            if let Some(unit) = self.read_unit_credential()? {
                if unit.as_slice() == bytes {
                    return Ok(());
                }
            }
        }
        let credential = self.encrypt(bytes)?;
        // Staged and read back before it replaces anything: a credential
        // that won't unseal here is no home for the key, and an existing
        // sealed copy (a re-wrap) must survive a failed attempt.
        let staged = self
            .dir
            .join(format!("{}-{item}.cred.staged", self.kind.file_prefix()));
        let reject = |path: &Path, err: std::io::Error| {
            KeystoreError::new(
                KeystoreErrorKind::Rejected,
                format!("could not write {}: {err}", path.display()),
            )
        };
        write_private_atomic(&staged, &credential).map_err(|err| reject(&staged, err))?;
        let verified = match self.decrypt(&staged) {
            Ok(back) if back.as_slice() == bytes => Ok(()),
            Ok(_) => Err(KeystoreError::new(
                KeystoreErrorKind::Corrupt,
                format!("{} unsealed to different bytes", staged.display()),
            )),
            Err(err) => Err(err),
        };
        let committed = verified
            .and_then(|()| std::fs::rename(&staged, &sealed).map_err(|err| reject(&sealed, err)));
        if committed.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        committed
    }

    /// Removes this install's sealed file for `item`, if any.
    fn delete_item(&self, item: KeychainItemId) -> Result<()> {
        let sealed = self.sealed_path(item);
        match std::fs::remove_file(&sealed) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(KeystoreError::new(
                KeystoreErrorKind::Rejected,
                format!("could not remove {}: {err}", sealed.display()),
            )),
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    /// A stand-in `systemd-creds`: "sealing" prefixes `SEALED`, unsealing
    /// strips it. It checks the arguments the real tool needs and fails on
    /// anything else.
    const FAKE: &str = r#"#!/bin/sh
[ "$1" = "--user" ] || { echo "missing --user" >&2; exit 2; }
[ "$3" = "--name=bookclerk-master-key" ] || { echo "bad name" >&2; exit 2; }
case "$2" in
  encrypt) [ "$4" = "-" ] && [ "$5" = "-" ] || exit 2; printf 'SEALED'; cat ;;
  decrypt) [ "$5" = "-" ] || exit 2; head -c 6 "$4" | grep -q SEALED || { echo "not sealed here" >&2; exit 1; }; tail -c +7 "$4" ;;
  *) exit 2 ;;
esac
"#;

    /// Writes an executable script `body` into `dir` and returns its path.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn an_item_is_sealed_beside_master_key_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(dir.path(), "systemd-creds", FAKE);
        let vault = SystemdCredsVault::with_program(dir.path(), Some(program), None);
        let item = KeychainItemId::random();
        assert_eq!(
            vault.get(item).map(|_| ()).map_err(|e| e.kind),
            Err(KeystoreErrorKind::NotFound)
        );

        vault.set(item, b"BCK1-envelope").unwrap();
        let sealed = std::fs::read(vault.sealed_path(item)).unwrap();
        assert_eq!(sealed, b"SEALEDBCK1-envelope");
        assert_eq!(
            std::fs::metadata(vault.sealed_path(item))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(vault.get(item).unwrap().as_slice(), b"BCK1-envelope");

        vault.set(item, b"BCK2-envelope").unwrap();
        assert_eq!(vault.get(item).unwrap().as_slice(), b"BCK2-envelope");

        vault.delete(item).unwrap();
        assert!(!vault.sealed_path(item).exists());
        vault.delete(item).unwrap();
    }

    #[test]
    fn errors_name_systemd_creds_not_the_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let vault = SystemdCredsVault::with_program(dir.path(), None, None);
        let err = vault.get(KeychainItemId::random()).unwrap_err();
        assert_eq!(err.store, ErrorStore::SystemdCreds);
        assert!(
            err.to_string()
                .starts_with("systemd-creds item not found: "),
            "{err}"
        );
        let err = vault.set(KeychainItemId::random(), b"key").unwrap_err();
        assert!(
            err.to_string().starts_with("systemd-creds unavailable: "),
            "{err}"
        );
    }

    #[test]
    fn a_missing_tool_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let vault = SystemdCredsVault::with_program(dir.path(), None, None);
        let err = vault.set(KeychainItemId::random(), b"key").unwrap_err();
        assert_eq!(err.kind, KeystoreErrorKind::Unavailable);
        assert!(err.message.contains("not installed"), "{err}");

        let gone = SystemdCredsVault::with_program(
            dir.path(),
            Some(dir.path().join("no-such-tool")),
            None,
        );
        let err = gone.set(KeychainItemId::random(), b"key").unwrap_err();
        assert_eq!(err.kind, KeystoreErrorKind::Unavailable);
    }

    #[test]
    fn a_failing_tool_reports_its_last_line_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            "systemd-creds",
            "#!/bin/sh\ncat >/dev/null\necho 'first line' >&2\necho 'No TPM2 device found.' >&2\nexit 1\n",
        );
        let vault = SystemdCredsVault::with_program(dir.path(), Some(program), None);
        let item = KeychainItemId::random();
        let err = vault.set(item, b"key").unwrap_err();
        assert_eq!(err.kind, KeystoreErrorKind::Unavailable);
        assert!(err.message.ends_with("No TPM2 device found."), "{err}");
        assert!(!vault.sealed_path(item).exists());
    }

    #[test]
    fn a_credential_that_wont_unseal_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        // Seals, but unsealing always fails.
        let program = script(
            dir.path(),
            "systemd-creds",
            "#!/bin/sh\nif [ \"$2\" = encrypt ]; then cat; exit 0; fi\necho 'TPM2 policy mismatch' >&2\nexit 1\n",
        );
        let vault = SystemdCredsVault::with_program(dir.path(), Some(program), None);
        let item = KeychainItemId::random();
        let err = vault.set(item, b"key").unwrap_err();
        assert_eq!(err.kind, KeystoreErrorKind::AccessDenied);
        assert!(!vault.sealed_path(item).exists());
    }

    #[test]
    fn a_credential_that_unseals_to_other_bytes_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            "systemd-creds",
            "#!/bin/sh\nif [ \"$2\" = encrypt ]; then cat; exit 0; fi\nprintf other\n",
        );
        let vault = SystemdCredsVault::with_program(dir.path(), Some(program), None);
        let item = KeychainItemId::random();
        let err = vault.set(item, b"key").unwrap_err();
        assert_eq!(err.kind, KeystoreErrorKind::Corrupt);
        assert!(!vault.sealed_path(item).exists());
    }

    #[test]
    fn a_failed_reseal_keeps_the_sealed_copy() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(dir.path(), "systemd-creds", FAKE);
        let vault = SystemdCredsVault::with_program(dir.path(), Some(program), None);
        let item = KeychainItemId::random();
        vault.set(item, b"BCK1-envelope").unwrap();
        // The tool now seals, but nothing it seals unseals again.
        let broken = script(
            dir.path(),
            "broken-creds",
            "#!/bin/sh\nif [ \"$2\" = encrypt ]; then cat; exit 0; fi\nhead -c 6 \"$4\" | grep -q SEALED || { echo 'policy mismatch' >&2; exit 1; }\ntail -c +7 \"$4\"\n",
        );
        let rewrap = SystemdCredsVault::with_program(dir.path(), Some(broken), None);
        assert!(rewrap.set(item, b"BCK2-envelope").is_err());
        assert_eq!(rewrap.get(item).unwrap().as_slice(), b"BCK1-envelope");
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter(|e| e
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".staged"))
                .count(),
            0
        );
    }

    #[test]
    fn a_unit_credential_is_read_when_nothing_is_sealed_here() {
        let dir = tempfile::tempdir().unwrap();
        let creds = tempfile::tempdir().unwrap();
        std::fs::write(creds.path().join(CREDENTIAL_NAME), b"BCK2-from-unit").unwrap();
        // No tool at all: the unit's copy needs none.
        let vault =
            SystemdCredsVault::with_program(dir.path(), None, Some(creds.path().to_path_buf()));
        let item = KeychainItemId::random();
        assert_eq!(vault.get(item).unwrap().as_slice(), b"BCK2-from-unit");
        // Storing the envelope the unit already holds needs no sealing.
        vault.set(item, b"BCK2-from-unit").unwrap();
        assert!(!vault.sealed_path(item).exists());
        // A different envelope must be sealed, which needs the tool.
        assert_eq!(
            vault.set(item, b"BCK2-other").unwrap_err().kind,
            KeystoreErrorKind::Unavailable
        );
    }

    #[test]
    fn this_installs_own_credential_wins_over_the_units() {
        let dir = tempfile::tempdir().unwrap();
        let creds = tempfile::tempdir().unwrap();
        std::fs::write(creds.path().join(CREDENTIAL_NAME), b"BCK2-from-unit").unwrap();
        let program = script(dir.path(), "systemd-creds", FAKE);
        let vault = SystemdCredsVault::with_program(
            dir.path(),
            Some(program),
            Some(creds.path().to_path_buf()),
        );
        let item = KeychainItemId::random();
        vault.set(item, b"BCK1-own").unwrap();
        assert_eq!(vault.get(item).unwrap().as_slice(), b"BCK1-own");
    }

    #[test]
    fn a_database_credential_has_its_own_file_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let creds = tempfile::tempdir().unwrap();
        // The unit hands in a master key; a database credential must never
        // read it.
        std::fs::write(creds.path().join(CREDENTIAL_NAME), b"BCK2-from-unit").unwrap();
        let program = script(
            dir.path(),
            "systemd-creds",
            &FAKE.replace("bookclerk-master-key", DATABASE_CREDENTIAL_NAME),
        );
        let vault = SystemdCredsVault::with_program(
            dir.path(),
            Some(program),
            Some(creds.path().to_path_buf()),
        )
        .for_kind(ItemKind::DatabaseCredential);
        let item = KeychainItemId::random();
        assert_eq!(
            vault.get(item).map(|_| ()).map_err(|e| e.kind),
            Err(KeystoreErrorKind::NotFound)
        );
        vault.set(item, b"sealed-token").unwrap();
        let sealed = dir.path().join(format!("database-credential-{item}.cred"));
        assert_eq!(vault.sealed_path(item), sealed);
        assert!(sealed.is_file());
        assert_eq!(vault.get(item).unwrap().as_slice(), b"sealed-token");
        vault.delete(item).unwrap();
        assert!(!sealed.exists());
    }

    #[test]
    fn an_oversized_item_is_refused_before_the_tool_runs() {
        let dir = tempfile::tempdir().unwrap();
        let vault = SystemdCredsVault::with_program(dir.path(), None, None);
        let err = vault
            .set(KeychainItemId::random(), &[0u8; crate::MAX_ITEM_LEN + 1])
            .unwrap_err();
        assert_eq!(err.kind, KeystoreErrorKind::Rejected);
    }

    #[test]
    fn stderr_excerpts_are_short_and_never_empty() {
        assert_eq!(excerpt(b""), "no message");
        assert_eq!(excerpt(b"a\nlast line\n\n"), "last line");
        assert_eq!(excerpt(&[b'x'; 1000]).len(), STDERR_EXCERPT_CHARS);
    }

    #[test]
    fn the_default_store_looks_for_the_system_tool() {
        let dir = tempfile::tempdir().unwrap();
        let vault = SystemdCredsVault::new(dir.path());
        if let Some(program) = &vault.program {
            assert!(PROGRAM_PATHS.iter().any(|p| Path::new(p) == program));
        }
    }
}
