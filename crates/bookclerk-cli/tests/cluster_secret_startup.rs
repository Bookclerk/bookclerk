//! Command startup against an enrolled library, through the real `bookclerk`
//! binary and the jailed workerd sqlite guest.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use bookclerk_library::master_key_path;

fn bookclerk_bin() -> &'static str {
    env!("CARGO_BIN_EXE_bookclerk")
}

fn sqlite_plugin_bin() -> PathBuf {
    Path::new(bookclerk_bin())
        .parent()
        .expect("target dir")
        .join("bookclerk-plugin-database-sqlite")
}

fn stage_sqlite_plugin(files: &Path) {
    let key =
        bookclerk_plugin_catalog::PluginKey::platform("bookclerk-plugin-database-sqlite", "sqlite")
            .expect("platform key");
    let plugin = files.join("plugins").join(key.fs_id());
    fs::create_dir_all(&plugin).unwrap();
    let bin = sqlite_plugin_bin();
    assert!(
        bin.is_file(),
        "build the sqlite guest before this test: {}",
        bin.display()
    );
    let dest = plugin.join("bookclerk-plugin-database-sqlite");
    if fs::hard_link(&bin, &dest).is_err() {
        fs::copy(&bin, &dest).unwrap();
    }
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../bookclerk-plugins/platform/database-sqlite/plugin.toml");
    fs::copy(&manifest_path, plugin.join("plugin.toml")).unwrap();
    let text = fs::read_to_string(plugin.join("plugin.toml")).unwrap();
    let manifest = bookclerk_plugin_manifest::PluginManifest::parse(&text).expect("manifest");
    bookclerk_plugin_catalog::stamp_platform_receipt(
        &plugin,
        files,
        "bookclerk-plugin-database-sqlite",
        &manifest,
        "0.1.0",
    )
    .expect("stamp platform sqlite");
}

fn write_config(files: &Path) {
    fs::write(
        files.join("config.toml"),
        "\
[database]
plugin = \"sqlite\"

[plugins]
isolation = \"required\"

[daemon]
listen = \"127.0.0.1:9\"

[daemon.auth]
enabled = true
",
    )
    .unwrap();
}

fn secret_rows(files: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let conn = rusqlite::Connection::open(files.join("library.db")).expect("open library.db");
    let mut stmt = conn
        .prepare(
            "SELECT ciphertext, IFNULL(cipher_nonce, X'') \
             FROM encrypted_secrets ORDER BY kind, name",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .unwrap();
    rows.collect::<Result<Vec<_>, _>>().unwrap()
}

fn run_bookclerk(files: &Path, args: &[&str], extra: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(bookclerk_bin());
    cmd.env("BOOKCLERK_FILES_DIR", files)
        .env_remove("BOOKCLERK_PLUGIN_DIRS")
        .env_remove("BOOKCLERK_AUTH_PASSWORD")
        .env_remove("BOOKCLERK_CONFIG")
        .env_remove("BOOKCLERK_DATABASE_PLUGIN")
        .env_remove("BOOKCLERK_DATABASE_SQLITE_PATH")
        .env_remove("BOOKCLERK_EVENTS_RETENTION_DAYS")
        .env_remove("BOOKCLERK_EVENTS_DEAD_LETTER_RETENTION_DAYS")
        .env_remove("BOOKCLERK_EVENTS_CONCURRENCY")
        .env_remove("BOOKCLERK_OPERATOR_TOKEN")
        .env_remove("BOOKCLERK_AWS_ACCESS_KEY_ID")
        .env_remove("BOOKCLERK_AWS_SECRET_ACCESS_KEY")
        .env_remove("BOOKCLERK_AWS_SESSION_TOKEN")
        .args(args);
    for (key, value) in extra {
        cmd.env(key, value);
    }
    cmd.output().expect("spawn bookclerk")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn version_and_master_key_commands_do_not_mint() {
    let files = tempfile::tempdir().unwrap();
    fs::create_dir_all(files.path()).unwrap();
    let version = run_bookclerk(files.path(), &["version"], &[]);
    assert!(version.status.success(), "{}", stderr(&version));
    assert!(!master_key_path(files.path()).exists());

    let status = run_bookclerk(files.path(), &["config", "master-key", "status"], &[]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(!master_key_path(files.path()).exists());
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("missing"),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );

    let wrap = run_bookclerk(
        files.path(),
        &["config", "master-key", "wrap"],
        &[("BOOKCLERK_AUTH_PASSWORD", "test-passphrase")],
    );
    assert!(!wrap.status.success(), "wrap must not mint a missing key");
    assert!(!master_key_path(files.path()).exists());
}

#[test]
fn enrolled_database_rejects_missing_and_wrong_keys_and_accepts_the_cluster_key() {
    let files_dir = tempfile::tempdir().unwrap();
    let files = files_dir.path();
    write_config(files);
    stage_sqlite_plugin(files);
    let approved = run_bookclerk(files, &["plugins", "approve", "sqlite", "--yes"], &[]);
    assert!(
        approved.status.success(),
        "approve sqlite: {}",
        stderr(&approved)
    );

    let enrolled = run_bookclerk(files, &["config", "get", "events.retention_days"], &[]);
    assert!(
        enrolled.status.success(),
        "first enrollment should mint and read events: {}",
        stderr(&enrolled)
    );
    let enrolled_out = String::from_utf8_lossy(&enrolled.stdout);
    assert!(enrolled_out.contains('7'), "{enrolled_out}");
    let key = fs::read(master_key_path(files)).expect("enrollment mints master.key");

    let aws = [
        ("BOOKCLERK_AWS_ACCESS_KEY_ID", "AKIAtestkey"),
        ("BOOKCLERK_AWS_SECRET_ACCESS_KEY", "secretsecretsecret"),
    ];
    let saved = run_bookclerk(files, &["config", "s3-credentials", "set"], &aws);
    assert!(
        saved.status.success(),
        "correct key should seal S3 credentials: {}",
        stderr(&saved)
    );
    let secrets = secret_rows(files);
    assert_eq!(secrets.len(), 1, "s3 set should write one secret row");

    fs::remove_file(master_key_path(files)).unwrap();
    let missing = run_bookclerk(files, &["config", "s3-credentials", "set"], &aws);
    let missing_err = stderr(&missing);
    assert!(!missing.status.success(), "missing key should fail");
    assert!(
        missing_err.contains("secret root missing"),
        "missing key must fail alignment, got: {missing_err}"
    );
    assert!(
        !master_key_path(files).exists(),
        "a missing cluster key must not be minted"
    );
    assert_eq!(secret_rows(files), secrets);

    let wrong_dir = tempfile::tempdir().unwrap();
    bookclerk_library::configure_master_key(wrong_dir.path()).unwrap();
    fs::copy(master_key_path(wrong_dir.path()), master_key_path(files)).unwrap();
    let wrong_bytes = fs::read(master_key_path(files)).unwrap();
    assert_ne!(wrong_bytes, key);
    let wrong = run_bookclerk(files, &["config", "get", "events.retention_days"], &[]);
    let wrong_err = stderr(&wrong);
    assert!(!wrong.status.success(), "wrong key should fail");
    assert!(
        wrong_err.contains("secret root mismatch"),
        "wrong key must fail alignment, got: {wrong_err}"
    );
    assert_eq!(
        fs::read(master_key_path(files)).unwrap(),
        wrong_bytes,
        "the existing wrong key must not be replaced"
    );
    assert_eq!(secret_rows(files), secrets);

    fs::write(master_key_path(files), &key).unwrap();
    let again = run_bookclerk(files, &["config", "get", "events.retention_days"], &[]);
    assert!(
        again.status.success(),
        "correct key should read events: {}",
        stderr(&again)
    );
    assert!(
        String::from_utf8_lossy(&again.stdout).contains('7'),
        "{}",
        String::from_utf8_lossy(&again.stdout)
    );
    let shown = run_bookclerk(files, &["config", "s3-credentials", "show"], &[]);
    assert!(
        shown.status.success(),
        "correct key should read S3 credentials: {}",
        stderr(&shown)
    );
    let shown_out = String::from_utf8_lossy(&shown.stdout);
    assert!(
        shown_out.contains("AKIAt") || shown_out.contains("present"),
        "{shown_out}"
    );
    assert_eq!(fs::read(master_key_path(files)).unwrap(), key);
    assert_eq!(secret_rows(files), secrets);
}

const EVENTS_SECRET_SENTINEL: &str = "bc-show-sentinel-7f3c";

fn credential_url() -> String {
    format!("postgres://operator:{EVENTS_SECRET_SENTINEL}@127.0.0.1:1/library")
}

fn secret_env(url: &str) -> [(&str, &str); 2] {
    [
        ("AWS_SECRET_ACCESS_KEY", EVENTS_SECRET_SENTINEL),
        ("BOOKCLERK_DATABASE_POSTGRES_URL", url),
    ]
}

fn combined_output(output: &std::process::Output) -> (String, String, String) {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let combined = format!("{stdout}\n{stderr}");
    (stdout, stderr, combined)
}

fn assert_events_secrets_absent(output: &std::process::Output, url: &str, label: &str) {
    let (stdout, stderr, combined) = combined_output(output);
    assert!(
        !combined.contains(EVENTS_SECRET_SENTINEL),
        "{label} printed the secret sentinel\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !combined.contains(url),
        "{label} printed the credential URL\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !combined.contains(&format!("operator:{EVENTS_SECRET_SENTINEL}@")),
        "{label} printed URL userinfo\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn config_show_redacts_events_errors_and_prints_valid_values() {
    let url = credential_url();

    let backend_dir = tempfile::tempdir().unwrap();
    fs::write(
        backend_dir.path().join("config.toml"),
        format!("[database]\nplugin = \"{EVENTS_SECRET_SENTINEL}\"\n"),
    )
    .unwrap();
    let backend = run_bookclerk(backend_dir.path(), &["config", "show"], &secret_env(&url));
    assert!(
        backend.status.success(),
        "config show stays successful when events cannot load: {}",
        stderr(&backend)
    );
    let (backend_out, _, _) = combined_output(&backend);
    assert!(
        backend_out.contains("database unavailable"),
        "backend failure should stay understandable:\n{backend_out}"
    );
    assert!(
        backend_out.contains("not installed"),
        "backend error should still name the missing plugin:\n{backend_out}"
    );
    assert!(
        backend_out.contains("[REDACTED]"),
        "backend error should scrub the registered plugin id:\n{backend_out}"
    );
    assert_events_secrets_absent(&backend, &url, "backend error");

    let files_dir = tempfile::tempdir().unwrap();
    let files = files_dir.path();
    write_config(files);
    stage_sqlite_plugin(files);
    let approved = run_bookclerk(files, &["plugins", "approve", "sqlite", "--yes"], &[]);
    assert!(
        approved.status.success(),
        "approve sqlite: {}",
        stderr(&approved)
    );
    let enrolled = run_bookclerk(files, &["config", "get", "events.retention_days"], &[]);
    assert!(
        enrolled.status.success(),
        "enrollment: {}",
        stderr(&enrolled)
    );

    let shown = run_bookclerk(files, &["config", "show"], &secret_env(&url));
    assert!(shown.status.success(), "valid show: {}", stderr(&shown));
    let (shown_out, _, _) = combined_output(&shown);
    assert!(
        shown_out.contains("events.authority = database"),
        "{shown_out}"
    );
    assert!(shown_out.contains("events.revision = "), "{shown_out}");
    assert!(
        shown_out.contains("events.retention_days = 7"),
        "{shown_out}"
    );
    assert!(
        shown_out.contains("events.dead_letter_retention_days = 30"),
        "{shown_out}"
    );
    assert!(shown_out.contains("events.concurrency = 1"), "{shown_out}");
    assert!(
        !shown_out.contains("database unavailable"),
        "valid events must not look like a load failure:\n{shown_out}"
    );
    assert_events_secrets_absent(&shown, &url, "valid show");

    let conn = rusqlite::Connection::open(files.join("library.db")).expect("library.db");
    let malformed =
        format!(r#"{{"retention_days":"{url}","dead_letter_retention_days":30,"concurrency":1}}"#);
    let updated = conn
        .execute(
            "UPDATE configuration_documents SET document_json = ?1 WHERE namespace = 'core.events'",
            [&malformed],
        )
        .expect("corrupt events document");
    assert_eq!(updated, 1, "events document should exist after enrollment");
    let parsed = run_bookclerk(files, &["config", "show"], &secret_env(&url));
    assert!(
        parsed.status.success(),
        "parser failure is reported on stdout: {}",
        stderr(&parsed)
    );
    let (parsed_out, _, _) = combined_output(&parsed);
    assert!(
        parsed_out.contains("events.authority = transitional"),
        "{parsed_out}"
    );
    assert!(
        parsed_out.contains("database unavailable"),
        "parser failure should stay understandable:\n{parsed_out}"
    );
    assert!(
        parsed_out.contains("invalid configuration"),
        "parser failure should still say the document is invalid:\n{parsed_out}"
    );
    assert!(
        parsed_out.contains("[REDACTED]"),
        "parser error should scrub the credential URL:\n{parsed_out}"
    );
    assert!(
        parsed_out.contains("events.retention_days = 7"),
        "transitional retention should still display:\n{parsed_out}"
    );
    assert_events_secrets_absent(&parsed, &url, "parser error");
}
