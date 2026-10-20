//! Joining a library as a clerk (an owner node that shares the library's
//! database and DEK), the joining side.
//!
//! 1. The sign-in starts with [`crate::authorize_join`] offering a clerk;
//!    an Owner of the library chooses clerk on its consent page (after a
//!    fresh MFA or passkey step).
//! 2. [`redeem_clerk_code`] redeems the code as a clerk. The answer carries
//!    the `[database]` bootstrap and the DEK sealed to this node's X25519
//!    box key; only this process can open it, and it never passes through
//!    the browser or a log.
//! 3. [`apply_clerk_bootstrap`] writes them as this install's own
//!    bootstrap: the database connection into `config.toml` (a Postgres URL
//!    into a `0600` file named by `url_file`), the DEK into `master.key`
//!    (wrapped with this install's own `BOOKCLERK_AUTH_PASSWORD` or
//!    `[auth].password` when set), and `node-identity.json` with the node
//!    id the library recorded (`kind = owner`). The daemon then starts
//!    again on it as an owner node.
//!
//! [`install_problem`] is what the joining side checks first: only an
//! empty install (no library data of its own) may join as a clerk, and no
//! database environment override may outrank the new bootstrap.

use std::path::{Path, PathBuf};

use bookclerk_config::{Config, DatabasePluginKind};
use bookclerk_library::control_plane::{
    encode_node_identity_file, library_contents, node_identity_path, ClusterLinkKeys,
    LibraryContents,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_proto::{open_clerk_bootstrap, ClerkBootstrap, ClerkDatabase, ConnectionType};
use zeroize::Zeroizing;

use crate::branch::{post_redeem, BranchRedeem};
use crate::enroll::JoinError;

/// File under the files directory that holds a clerk's Postgres URL, mode
/// `0600`. `[database.postgres].url_file` names it by its absolute path
/// (a relative `url_file` would be read from the daemon's working
/// directory).
pub const CLERK_POSTGRES_URL_FILE: &str = "database-postgres.url";

/// Environment variables that would outrank a clerk's `[database]`
/// bootstrap; a clerk join is refused while one is set.
pub const DATABASE_ENV_OVERRIDES: &[&str] = &[
    "BOOKCLERK_DATABASE_PLUGIN",
    "BOOKCLERK_DATABASE_SQLITE_PATH",
    "BOOKCLERK_DATABASE_POSTGRES_URL",
    "BOOKCLERK_DATABASE_POSTGRES_URL_FILE",
    "BOOKCLERK_D1_ACCOUNT_ID",
    "BOOKCLERK_D1_DATABASE_ID",
    "BOOKCLERK_D1_API_BASE",
];

/// A redeemed clerk sign-in. `Debug` prints no secret.
#[derive(Debug)]
pub struct ClerkJoined {
    /// The library's cluster id.
    pub cluster_id: String,
    /// The node id the library recorded (`kind = owner`).
    pub node_id: String,
    /// The opened bootstrap.
    pub bootstrap: ClerkBootstrap,
}

/// Why this install may not join as a clerk now, or `None`.
///
/// # Errors
///
/// Returns the read error.
pub async fn install_problem(store: &LibraryStore) -> Result<Option<String>, JoinError> {
    let contents: LibraryContents = library_contents(store)
        .await
        .map_err(|err| JoinError::Record(err.to_string()))?;
    if !contents.is_empty() {
        return Ok(Some(format!(
            "this server already holds library data of its own ({}); only an empty server can join as a clerk",
            contents.reasons().join(", ")
        )));
    }
    if let Some(name) = active_database_override(|name| std::env::var(name).ok()) {
        return Ok(Some(format!(
            "{name} is set here and would override the library's database; unset it to join as a clerk"
        )));
    }
    Ok(None)
}

/// The `[database].plugin` a clerk's bootstrap configures.
#[must_use]
pub fn bootstrap_database_kind(database: &ClerkDatabase) -> DatabasePluginKind {
    match database {
        ClerkDatabase::Postgres { .. } => DatabasePluginKind::Postgres,
        ClerkDatabase::D1 { .. } => DatabasePluginKind::D1,
    }
}

/// True when the database guest for `kind` is installed for `config`, so
/// the daemon can open the library's database once the bootstrap is
/// applied. A plugin directory that cannot be read counts as not
/// installed.
#[must_use]
pub fn database_guest_installed(config: &Config, kind: DatabasePluginKind) -> bool {
    let Ok(plugins) = bookclerk_plugin_host::discover_plugins(config) else {
        return false;
    };
    let adapters: Vec<_> = plugins
        .into_iter()
        .filter(|plugin| {
            plugin
                .manifest
                .has_entrypoint(bookclerk_plugin_host::Entrypoint::DatabaseAdapter)
        })
        .collect();
    bookclerk_plugin_host::resolve_plugin_ref(&adapters, kind.as_str()).is_ok()
}

/// True when `Config::apply_env_overrides` applies database override
/// `name` set to `value`, so it would outrank a clerk's bootstrap. The D1
/// account and database ids apply whatever they hold (an empty value
/// clears the configured id); every other override applies only when it is
/// not blank after trimming.
fn database_override_applies(name: &str, value: &str) -> bool {
    match name {
        "BOOKCLERK_D1_ACCOUNT_ID" | "BOOKCLERK_D1_DATABASE_ID" => true,
        _ => !value.trim().is_empty(),
    }
}

/// The first of [`DATABASE_ENV_OVERRIDES`] that `lookup` (the process
/// environment, as `std::env::var(..).ok()`) holds with a value
/// `Config::apply_env_overrides` would apply. Like that function, a value
/// that is not UTF-8 counts as unset, since `std::env::var` refuses it.
fn active_database_override(lookup: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    DATABASE_ENV_OVERRIDES
        .iter()
        .copied()
        .find(|name| lookup(name).is_some_and(|value| database_override_applies(name, &value)))
}

/// Redeems an approved clerk sign-in and opens the bootstrap with the
/// node's box secret. The code and verifier go once in the request body,
/// and are registered for redaction (the caller unregisters them).
///
/// # Errors
///
/// Returns [`JoinError`] when the library is unreachable or refuses (the
/// code was approved for a branch, the user is no longer an Owner, or its
/// database cannot be shared). Once the library has enrolled the node, an
/// answer for another cluster or type, without a bootstrap, or with one
/// that does not open for this node gives [`JoinError::Unsaved`] naming
/// it. [`JoinError::is_retryable`] tells which failures a repeat of the
/// same exchange can fix (the library answers it with the same node and a
/// freshly sealed bootstrap).
pub async fn redeem_clerk_code(
    redeem: &BranchRedeem<'_>,
    keys: &ClusterLinkKeys,
) -> Result<ClerkJoined, JoinError> {
    let (_endpoint, enrolled) = post_redeem(redeem, keys, ConnectionType::Clerk).await?;
    // The library has enrolled the node by now: a failure names it, so the
    // operator knows which clerk to have removed there.
    let unsaved = |reason: &str| JoinError::Unsaved {
        node_id: Some(enrolled.node_id.clone()),
        reason: reason.into(),
    };
    let sealed = enrolled
        .bootstrap
        .as_deref()
        .ok_or_else(|| unsaved("the library sent no clerk bootstrap"))?;
    let bootstrap = open_clerk_bootstrap(
        keys.box_secret(),
        &enrolled.cluster_id,
        &enrolled.node_id,
        sealed,
    )
    .map_err(|_| unsaved("the library's clerk bootstrap does not open here"))?;
    tracing::info!(
        cluster_id = %enrolled.cluster_id,
        node_id = %enrolled.node_id,
        database = bootstrap.database.plugin(),
        "redeemed a clerk sign-in"
    );
    Ok(ClerkJoined {
        cluster_id: enrolled.cluster_id,
        node_id: enrolled.node_id,
        bootstrap,
    })
}

/// What [`apply_clerk_bootstrap`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedClerk {
    /// The `[database].plugin` now configured.
    pub database: DatabasePluginKind,
    /// True when `master.key` is wrapped with this install's password.
    pub wrapped: bool,
    /// The config file written.
    pub config_file: PathBuf,
}

/// Suffix of the copy [`apply_clerk_bootstrap`] keeps of each file it
/// replaces (`master.key.pre-clerk` beside `master.key`), until the apply
/// has replaced them all.
const CLERK_BACKUP_SUFFIX: &str = ".pre-clerk";

/// One file [`apply_clerk_bootstrap`] writes.
struct NewFile<'a> {
    /// What the file is, for errors (`master.key`).
    what: &'static str,
    /// The file to replace.
    target: PathBuf,
    /// Its new contents.
    bytes: &'a [u8],
    /// Mode `0600` on Unix (a secret, or a file naming one).
    private: bool,
}

/// A [`NewFile`] staged beside its target, with the copy of the original.
struct Staged {
    /// What the file is, for errors.
    what: &'static str,
    /// The file to replace.
    target: PathBuf,
    /// The new contents, written and synced beside `target`.
    staged: PathBuf,
    /// The original `target`, copied beside it; `None` when there was none.
    backup: Option<PathBuf>,
}

/// A unique hidden path beside `target` for its new contents.
fn staging_path(target: &Path) -> PathBuf {
    let mut suffix = [0u8; 8];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut suffix);
    let hex: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
    target.with_file_name(format!(".{}.clerk-{hex}", file_name(target)))
}

/// Where [`apply_clerk_bootstrap`] keeps the original of `target`.
fn backup_path(target: &Path) -> PathBuf {
    target.with_file_name(format!("{}{CLERK_BACKUP_SUFFIX}", file_name(target)))
}

/// `path`'s file name, for the names of the files kept beside it.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into())
}

/// Creates `path` (which must not exist) holding `bytes`, mode `0600` when
/// `private`, and syncs it. A partly written file is removed.
fn write_new_file(path: &Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let written = (|| {
        let mut file = opts.open(path)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(path);
    }
    written
}

/// Copies `target` to `backup` (which must not exist), keeping its
/// permissions (`std::fs::copy` creates it with `target`'s mode), and syncs
/// it.
fn back_up(target: &Path, backup: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(backup).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "{} is left from an earlier clerk join that was interrupted: move it back over {} \
                 (or delete it if that file is right), then try again",
                backup.display(),
                target.display()
            ),
        ));
    }
    let copied = std::fs::copy(target, backup).and_then(|_| {
        std::fs::OpenOptions::new()
            .write(true)
            .open(backup)?
            .sync_all()
    });
    if copied.is_err() {
        let _ = std::fs::remove_file(backup);
    }
    copied
}

/// Removes the staged files and backups of `staged`, whose targets were not
/// replaced (or were restored).
fn discard(staged: &[Staged]) {
    for file in staged {
        let _ = std::fs::remove_file(&file.staged);
        if let Some(backup) = &file.backup {
            let _ = std::fs::remove_file(backup);
        }
    }
}

/// Puts back the originals of `replaced`, latest first: a backup moves back
/// over its target, and a target that did not exist before is removed. A
/// backup that cannot be moved back stays where it is.
///
/// Returns what could not be restored, for the operator.
fn roll_back(replaced: &[Staged]) -> Vec<String> {
    let mut failed = Vec::new();
    for file in replaced.iter().rev() {
        let restored = match &file.backup {
            Some(backup) => bookclerk_config::replace_file(backup, &file.target),
            None => std::fs::remove_file(&file.target),
        };
        if let Err(err) = restored {
            failed.push(match &file.backup {
                Some(backup) => format!(
                    "{} (its original is at {}): {err}",
                    file.target.display(),
                    backup.display()
                ),
                None => format!("{} (it did not exist before): {err}", file.target.display()),
            });
        }
    }
    failed
}

/// Syncs the directories holding `staged`, so the replaces are durable
/// (best effort: not every platform can open a directory).
fn sync_dirs(staged: &[Staged]) {
    for file in staged {
        if let Some(dir) = file.target.parent().filter(|d| !d.as_os_str().is_empty()) {
            if let Ok(dir) = std::fs::File::open(dir) {
                let _ = dir.sync_all();
            }
        }
    }
}

/// Replaces every target of `files` with its new contents, in order, as one
/// recoverable step (see [`apply_clerk_bootstrap`]): stages all of them,
/// backs up the originals, replaces them with
/// [`bookclerk_config::replace_file`] (atomic per file, on Windows too),
/// rolls the earlier ones back when a later one fails, and drops the
/// backups once all are in place.
///
/// # Errors
///
/// Returns [`JoinError::Record`] naming the file that could not be staged,
/// backed up or replaced, and any file the rollback could not restore.
fn replace_files(files: &[NewFile<'_>]) -> Result<(), JoinError> {
    let failed = |what: &str, err: &dyn std::fmt::Display| {
        JoinError::Record(format!("could not write the clerk's {what}: {err}"))
    };
    // 1. Stage every new file beside its target. Nothing is replaced yet,
    //    so a failure here (a full disk, a missing permission) changes
    //    nothing.
    let mut staged: Vec<Staged> = Vec::with_capacity(files.len());
    for file in files {
        let tmp = staging_path(&file.target);
        let written = file
            .target
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| write_new_file(&tmp, file.bytes, file.private));
        if let Err(err) = written {
            discard(&staged);
            return Err(failed(file.what, &err));
        }
        staged.push(Staged {
            what: file.what,
            target: file.target.clone(),
            staged: tmp,
            backup: None,
        });
    }
    // 2. Back up each original that exists.
    for index in 0..staged.len() {
        if !staged[index].target.is_file() {
            continue;
        }
        let backup = backup_path(&staged[index].target);
        if let Err(err) = back_up(&staged[index].target, &backup) {
            discard(&staged);
            return Err(failed(staged[index].what, &err));
        }
        staged[index].backup = Some(backup);
    }
    // 3. Replace the targets in order; on a failure, put back the ones
    //    already replaced.
    for (index, file) in staged.iter().enumerate() {
        if let Err(err) = bookclerk_config::replace_file(&file.staged, &file.target) {
            let (replaced, rest) = staged.split_at(index);
            let not_restored = roll_back(replaced);
            discard(rest);
            sync_dirs(&staged);
            let outcome = if not_restored.is_empty() {
                "the files already replaced were restored".to_string()
            } else {
                format!("could not restore {}", not_restored.join("; "))
            };
            return Err(JoinError::Record(format!(
                "could not write the clerk's {}: {err}; {outcome}",
                file.what
            )));
        }
    }
    // 4. All in place: drop the backups.
    sync_dirs(&staged);
    for file in &staged {
        if let Some(backup) = &file.backup {
            let _ = std::fs::remove_file(backup);
        }
    }
    Ok(())
}

/// Writes `bootstrap` as this install's own bootstrap under `files_dir`:
///
/// - `database-postgres.url` (Postgres only): the URL, mode `0600`;
/// - `node-identity.json`: the node id the library recorded, bound to its
///   cluster;
/// - `master.key`: the library's DEK, wrapped with `password` (this
///   install's `BOOKCLERK_AUTH_PASSWORD` or `[auth].password`) when set;
/// - `[database]` in `config_file` (other settings kept): `postgres` with
///   `url_file` naming the URL file (never the URL itself in the TOML), or
///   `d1` with its ids (the D1 API token stays this node's
///   environment-only bootstrap).
///
/// The files go in as one recoverable step. All the new contents are first
/// written and synced beside their targets (hidden `.<name>.clerk-<hex>`
/// files), and each existing target is copied to `<name>.pre-clerk`; a
/// failure up to here changes no target. The targets are then replaced
/// atomically one by one, in the order above. If one fails, the ones
/// already replaced are put back from their copies (a target that did not
/// exist before is removed), so the install keeps its own bootstrap; the
/// error names any file that could not be put back. On success the copies
/// are removed. The caller restarts the daemon afterwards.
///
/// A process that dies while replacing leaves the files before that point
/// in the list new and the rest old, with every original still beside its
/// file as `<name>.pre-clerk`; a later apply refuses to start while a copy
/// is there. If `config_file` is already new, every file was replaced and
/// the join is complete: delete the copies. Otherwise moving the copies
/// back over their files restores the install (a target with no copy did
/// not exist before and can be deleted). Staged `.<name>.clerk-<hex>` files
/// left by such a crash are unused and can be deleted.
///
/// # Errors
///
/// Returns [`JoinError::Record`] when a file cannot be written or the
/// existing `config.toml` does not parse.
pub fn apply_clerk_bootstrap(
    files_dir: &Path,
    config_file: &Path,
    bootstrap: &ClerkBootstrap,
    password: Option<&str>,
) -> Result<AppliedClerk, JoinError> {
    let record = |what: &str, err: &dyn std::fmt::Display| {
        JoinError::Record(format!("could not write the clerk's {what}: {err}"))
    };
    let mut cfg = if config_file.is_file() {
        Config::from_toml_file(config_file).map_err(|err| record("config.toml", &err))?
    } else {
        Config::default()
    };
    let mut url: Option<(PathBuf, Zeroizing<String>)> = None;
    let database = match &bootstrap.database {
        ClerkDatabase::Postgres { url: secret } => {
            let text = Zeroizing::new(
                std::str::from_utf8(secret.expose())
                    .map_err(|_| JoinError::Record("malformed database URL".into()))?
                    .trim()
                    .to_string(),
            );
            if text.is_empty() {
                return Err(JoinError::Record("empty database URL".into()));
            }
            bookclerk_config::register_secret(&text);
            let url_file = std::path::absolute(files_dir.join(CLERK_POSTGRES_URL_FILE))
                .map_err(|err| record("database URL file", &err))?;
            cfg.database.plugin = DatabasePluginKind::Postgres.as_str().into();
            cfg.database.postgres.url = None;
            cfg.database.postgres.url_file = Some(url_file.clone());
            url = Some((url_file, text));
            DatabasePluginKind::Postgres
        }
        ClerkDatabase::D1 {
            account_id,
            database_id,
            api_base,
        } => {
            cfg.database.plugin = DatabasePluginKind::D1.as_str().into();
            cfg.database.d1.account_id.clone_from(account_id);
            cfg.database.d1.database_id.clone_from(database_id);
            cfg.database.d1.api_base.clone_from(api_base);
            DatabasePluginKind::D1
        }
    };
    let identity = encode_node_identity_file(&bootstrap.node_id, &bootstrap.cluster_id)
        .map_err(|err| record("node identity", &err))?;
    let password = password.map(str::trim).filter(|p| !p.is_empty());
    let master_key = bookclerk_library::encode_master_key_file(bootstrap.dek.expose(), password)
        .map_err(|err| record("master.key", &err))?;
    // `0700`, as for a minted key; done before any target changes.
    bookclerk_library::prepare_master_key_dir(files_dir)
        .map_err(|err| record("master.key", &err))?;
    let config_text = cfg
        .to_toml_string()
        .map_err(|err| record("config.toml", &err))?;
    let mut files = Vec::with_capacity(4);
    if let Some((url_file, text)) = &url {
        files.push(NewFile {
            what: "database URL file",
            target: url_file.clone(),
            bytes: text.as_bytes(),
            private: true,
        });
    }
    files.push(NewFile {
        what: "node identity",
        target: node_identity_path(files_dir),
        bytes: &identity,
        private: true,
    });
    files.push(NewFile {
        what: "master.key",
        target: bookclerk_library::master_key_path(files_dir),
        bytes: &master_key,
        private: true,
    });
    // As `Config::write_toml_file` writes it: not a secret file of its own.
    files.push(NewFile {
        what: "config.toml",
        target: config_file.to_path_buf(),
        bytes: config_text.as_bytes(),
        private: false,
    });
    replace_files(&files)?;
    if password.is_none() {
        bookclerk_library::warn_unwrapped_master_key(&bookclerk_library::master_key_path(
            files_dir,
        ));
    }
    tracing::info!(
        cluster_id = %bootstrap.cluster_id,
        node_id = %bootstrap.node_id,
        database = database.as_str(),
        wrapped = password.is_some(),
        "wrote the clerk bootstrap; this install starts as an owner node of the library"
    );
    Ok(AppliedClerk {
        database,
        wrapped: password.is_some(),
        config_file: config_file.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_node_proto::{SecretValue, CLERK_BOOTSTRAP_VERSION};

    fn bootstrap(database: ClerkDatabase) -> ClerkBootstrap {
        ClerkBootstrap {
            v: CLERK_BOOTSTRAP_VERSION,
            cluster_id: "cluster-1".into(),
            node_id: "0b5c3d52-6f1e-4f39-9a3c-1b7f0a2c9e11".into(),
            database,
            dek: SecretValue::new(vec![5u8; 32]),
        }
    }

    #[test]
    fn a_postgres_bootstrap_becomes_this_installs_own() {
        let files = tempfile::tempdir().unwrap();
        let config = files.path().join("config.toml");
        std::fs::write(&config, "[daemon]\ntray = false\n").unwrap();
        let applied = apply_clerk_bootstrap(
            files.path(),
            &config,
            &bootstrap(ClerkDatabase::Postgres {
                url: SecretValue::new(b"postgres://u:pw-1@db/lib".to_vec()),
            }),
            Some("joiner-pass"),
        )
        .unwrap();
        assert_eq!(applied.database, DatabasePluginKind::Postgres);
        assert!(applied.wrapped);
        let toml = std::fs::read_to_string(&config).unwrap();
        assert!(!toml.contains("pw-1"), "the URL is not in the TOML: {toml}");
        let cfg = Config::from_toml_file(&config).unwrap();
        assert_eq!(cfg.database.plugin, "postgres");
        assert!(!cfg.daemon.tray, "other settings are kept");
        let url_file = files.path().join(CLERK_POSTGRES_URL_FILE);
        assert_eq!(
            cfg.database.postgres.url_file.as_deref(),
            Some(url_file.as_path())
        );
        assert_eq!(
            std::fs::read_to_string(&url_file).unwrap(),
            "postgres://u:pw-1@db/lib"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&url_file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // master.key is the DEK wrapped with this install's password.
        let raw = std::fs::read(files.path().join("master.key")).unwrap();
        assert_eq!(&raw[..4], b"BCK2");
        let identity =
            bookclerk_library::control_plane::load_or_create_node_identity(files.path()).unwrap();
        assert_eq!(identity.node_id, "0b5c3d52-6f1e-4f39-9a3c-1b7f0a2c9e11");
        assert_eq!(identity.cluster_id.as_deref(), Some("cluster-1"));
        // The copy of the replaced config.toml and the staged files are gone.
        for entry in std::fs::read_dir(files.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                !name.ends_with(CLERK_BACKUP_SUFFIX) && !name.contains(".clerk-"),
                "{name}"
            );
        }
    }

    #[test]
    fn a_d1_bootstrap_without_a_password_writes_an_unwrapped_key() {
        let files = tempfile::tempdir().unwrap();
        let config = files.path().join("config.toml");
        let applied = apply_clerk_bootstrap(
            files.path(),
            &config,
            &bootstrap(ClerkDatabase::D1 {
                account_id: "acct".into(),
                database_id: "db-uuid".into(),
                api_base: "https://api.cloudflare.com/client/v4".into(),
            }),
            None,
        )
        .unwrap();
        assert_eq!(applied.database, DatabasePluginKind::D1);
        assert!(!applied.wrapped);
        let cfg = Config::from_toml_file(&config).unwrap();
        assert_eq!(cfg.database.plugin, "d1");
        assert_eq!(cfg.database.d1.database_id, "db-uuid");
        let raw = std::fs::read(files.path().join("master.key")).unwrap();
        assert_eq!(raw, [b"BCK1".as_slice(), &[5u8; 32]].concat());
    }

    #[test]
    fn a_failed_config_write_restores_the_identity_and_key() {
        let files = tempfile::tempdir().unwrap();
        let identity = node_identity_path(files.path());
        let key = bookclerk_library::master_key_path(files.path());
        std::fs::write(&identity, b"original identity").unwrap();
        std::fs::write(&key, b"original key").unwrap();
        // config.toml's path is a directory: its replace, the last one,
        // fails after node-identity.json and master.key were replaced.
        let config = files.path().join("config.toml");
        std::fs::create_dir(&config).unwrap();
        let err = apply_clerk_bootstrap(
            files.path(),
            &config,
            &bootstrap(ClerkDatabase::Postgres {
                url: SecretValue::new(b"postgres://u:pw-1@db/lib".to_vec()),
            }),
            None,
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("config.toml"), "{message}");
        assert!(message.contains("were restored"), "{message}");
        assert_eq!(std::fs::read(&identity).unwrap(), b"original identity");
        assert_eq!(std::fs::read(&key).unwrap(), b"original key");
        assert!(config.is_dir());
        // The URL file did not exist before, so it is gone again, and no
        // staged file or backup is left behind.
        let mut left: Vec<String> = std::fs::read_dir(files.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["config.toml", "master.key", "node-identity.json"]);
    }

    #[test]
    fn a_backup_left_by_an_interrupted_apply_stops_a_new_one() {
        let files = tempfile::tempdir().unwrap();
        let key = bookclerk_library::master_key_path(files.path());
        std::fs::write(&key, b"half-applied key").unwrap();
        let backup = backup_path(&key);
        std::fs::write(&backup, b"original key").unwrap();
        let config = files.path().join("config.toml");
        let err = apply_clerk_bootstrap(
            files.path(),
            &config,
            &bootstrap(ClerkDatabase::D1 {
                account_id: "acct".into(),
                database_id: "db-uuid".into(),
                api_base: "https://api.cloudflare.com/client/v4".into(),
            }),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("interrupted"), "{err}");
        // Nothing changed, and the copy of the original is kept.
        assert_eq!(std::fs::read(&key).unwrap(), b"half-applied key");
        assert_eq!(std::fs::read(&backup).unwrap(), b"original key");
        assert!(!config.exists());
        assert!(!node_identity_path(files.path()).exists());
    }

    #[test]
    fn database_overrides_count_as_apply_env_overrides_applies_them() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_string())
            }
        };
        assert_eq!(active_database_override(env(&[])), None);
        // The D1 ids apply even when empty (they clear the configured id).
        for name in ["BOOKCLERK_D1_ACCOUNT_ID", "BOOKCLERK_D1_DATABASE_ID"] {
            let lookup = move |key: &str| (key == name).then(String::new);
            assert_eq!(active_database_override(lookup), Some(name));
        }
        // Every other override applies only when not blank.
        for name in DATABASE_ENV_OVERRIDES.iter().copied().filter(|name| {
            !matches!(
                *name,
                "BOOKCLERK_D1_ACCOUNT_ID" | "BOOKCLERK_D1_DATABASE_ID"
            )
        }) {
            for blank in ["", "  "] {
                let lookup = move |key: &str| (key == name).then(|| blank.to_string());
                assert_eq!(active_database_override(lookup), None, "{name}={blank:?}");
            }
            let lookup = move |key: &str| (key == name).then(|| "x".to_string());
            assert_eq!(active_database_override(lookup), Some(name));
        }
        assert_eq!(
            active_database_override(env(&[("BOOKCLERK_DATABASE_PLUGIN", " sqlite ")])),
            Some("BOOKCLERK_DATABASE_PLUGIN")
        );
    }
}
