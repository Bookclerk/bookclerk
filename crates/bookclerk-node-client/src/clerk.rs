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
    install_node_identity, library_contents, ClusterLinkKeys, LibraryContents,
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
    if let Some(name) = DATABASE_ENV_OVERRIDES
        .iter()
        .find(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
    {
        return Ok(Some(format!(
            "{name} is set here and would override the library's database; unset it to join as a clerk"
        )));
    }
    Ok(None)
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

/// Writes `bytes` to `path` with mode `0600`, replacing it atomically.
fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut suffix = [0u8; 8];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut suffix);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let tmp = parent.join(format!(
        ".{name}.tmp-{}",
        suffix
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let staged = (|| {
        let mut file = opts.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(err) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Writes `bootstrap` as this install's own bootstrap under `files_dir`:
///
/// - `[database]` in `config_file` (other settings kept): `postgres` with
///   `url_file = "database-postgres.url"` (the URL in that `0600` file,
///   never in the TOML), or `d1` with its ids (the D1 API token stays this
///   node's environment-only bootstrap);
/// - `node-identity.json`: the node id the library recorded, bound to its
///   cluster;
/// - `master.key`: the library's DEK, wrapped with `password` (this
///   install's `BOOKCLERK_AUTH_PASSWORD` or `[auth].password`) when set.
///
/// `config.toml` is written last, so an interrupted apply leaves the
/// install opening its own (empty) database. The caller restarts the
/// daemon afterwards.
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
    let database = match &bootstrap.database {
        ClerkDatabase::Postgres { url } => {
            let text = Zeroizing::new(
                std::str::from_utf8(url.expose())
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
            write_private_file(&url_file, text.as_bytes())
                .map_err(|err| record("database URL file", &err))?;
            cfg.database.plugin = DatabasePluginKind::Postgres.as_str().into();
            cfg.database.postgres.url = None;
            cfg.database.postgres.url_file = Some(url_file);
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
    install_node_identity(files_dir, &bootstrap.node_id, &bootstrap.cluster_id)
        .map_err(|err| record("node identity", &err))?;
    let password = password.map(str::trim).filter(|p| !p.is_empty());
    bookclerk_library::install_master_key(files_dir, bootstrap.dek.expose(), password)
        .map_err(|err| record("master.key", &err))?;
    cfg.write_toml_file(config_file)
        .map_err(|err| record("config.toml", &err))?;
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
}
