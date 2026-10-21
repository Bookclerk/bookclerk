//! `bookclerk clusters` through the real binary: the enrolled links (no
//! key material), the consent list, approve with narrowing and the
//! reviewed-request check, revoke, leave (by default the only active
//! link), and `join` refusing a link that is not one, or one on the
//! command line, and reading it from a terminal without echo.

// Test code: unwrap/expect and plain casts are fine here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use bookclerk_library::control_plane::{
    load_cluster_link_keys, load_remote_plugin_consent, record_cluster_link,
    record_remote_plugin_request, ClusterLinkKeys, NewClusterLink, RemoteArtifactRef,
};
use bookclerk_library::LibraryStore;
use bookclerk_plugin_host::PluginGrant;
use common::{run_bookclerk, stage_sqlite_plugin, stderr, stdout, write_config};
use serde_json::{json, Value};

/// Owner cluster id of the seeded link.
const CLUSTER: &str = "cluster-a";

/// Plugin key of the seeded request.
const PLUGIN: &str = "path:file:///tmp/store#store";

/// A files directory with the sqlite guest approved and the database
/// enrolled.
fn enrolled() -> tempfile::TempDir {
    let files_dir = tempfile::tempdir().unwrap();
    let files = files_dir.path();
    write_config(files);
    stage_sqlite_plugin(files);
    let approved = run_bookclerk(files, &["plugins", "approve", "sqlite", "--yes"], &[]);
    assert!(approved.status.success(), "{}", stderr(&approved));
    let enrolled = run_bookclerk(files, &["config", "get", "events.retention_days"], &[]);
    assert!(enrolled.status.success(), "{}", stderr(&enrolled));
    files_dir
}

/// Opens the files directory's `library.db` with its minted master key.
async fn open_store(files: &Path) -> LibraryStore {
    bookclerk_library::configure_master_key(files).expect("load the minted master key");
    let db = bookclerk_plugin_database_sqlite::open(&files.join("library.db"))
        .await
        .expect("open library.db");
    LibraryStore::from_connection(db)
}

/// Records a link to `CLUSTER` and that cluster's request to run `PLUGIN`
/// with two domains and two bindings.
async fn seed(store: &LibraryStore) {
    record_cluster_link(
        store,
        &NewClusterLink {
            owner_cluster_id: CLUSTER.into(),
            owner_origin: "https://owner.example".into(),
            remote_node_id: "node-7".into(),
            spki_pin: None,
        },
        &ClusterLinkKeys::generate(),
    )
    .await
    .expect("cluster link");
    let grant = PluginGrant {
        schema_version: bookclerk_plugin_host::GRANT_SCHEMA_VERSION,
        plugin_key: PLUGIN.into(),
        plugin_id: "store".into(),
        network_mode: "outbound".into(),
        domains: [
            "api.store.example".to_string(),
            "cdn.store.example".to_string(),
        ]
        .into(),
        manifest_domains: [
            "api.store.example".to_string(),
            "cdn.store.example".to_string(),
        ]
        .into(),
        bindings: ["config".to_string(), "secrets".to_string()].into(),
        approved_at: "2026-10-01T00:00:00Z".into(),
        ..PluginGrant::empty()
    };
    record_remote_plugin_request(
        store,
        CLUSTER,
        PLUGIN,
        "1.2.0",
        &serde_json::to_string(&grant).unwrap(),
        RemoteArtifactRef {
            url: "https://registry.example/store-1.2.0.tar.gz",
            archive_sha256: &"ab".repeat(32),
            payload_root_sha256: &"cd".repeat(32),
        },
    )
    .await
    .expect("plugin request");
}

/// Asserts that the command succeeded and parses its stdout as JSON.
fn json_out(output: &std::process::Output) -> Value {
    assert!(output.status.success(), "{}", stderr(output));
    serde_json::from_str(&stdout(output))
        .unwrap_or_else(|err| panic!("json output ({err}): {}", stdout(output)))
}

/// `bookclerk clusters join` with `input` on stdin.
fn join_with_stdin(files: &Path, input: &str) -> std::process::Output {
    let mut child = common::bookclerk_command(files)
        .args(["--format", "json", "clusters", "join"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bookclerk");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().expect("wait for bookclerk")
}

/// `list` shows the link without key material, `approve` checks the
/// reviewed request and narrows the grant, `revoke` clears it, and `leave`
/// marks the link left and deletes its keys.
#[tokio::test]
async fn list_approve_revoke_and_leave() {
    let files_dir = enrolled();
    let files = files_dir.path();
    let store = open_store(files).await;
    seed(&store).await;

    let listed = json_out(&run_bookclerk(
        files,
        &["--format", "json", "clusters", "list"],
        &[],
    ));
    assert_eq!(
        listed["clusters"],
        json!([{
            "cluster_id": CLUSTER,
            "owner_origin": "https://owner.example",
            "node_id": "node-7",
            "pinned": false,
            "state": "active",
            "enrolled_at": listed["clusters"][0]["enrolled_at"],
        }]),
        "a link without key material"
    );

    let consents = json_out(&run_bookclerk(
        files,
        &["--format", "json", "clusters", "consents"],
        &[],
    ));
    let rows = consents["consents"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["state"], "awaiting_host_consent");
    assert_eq!(rows[0]["requested_version"], "1.2.0");
    let digest = rows[0]["request_digest"].as_str().unwrap().to_string();
    let other = json_out(&run_bookclerk(
        files,
        &[
            "--format",
            "json",
            "clusters",
            "consents",
            "--cluster",
            "cluster-b",
        ],
        &[],
    ));
    assert!(other["consents"].as_array().unwrap().is_empty());

    // Another request than the one reviewed, or a domain the request does
    // not ask for, is refused and stores nothing.
    let other_request = "00".repeat(32);
    for extra in [
        &["--request", other_request.as_str()][..],
        &[
            "--request",
            digest.as_str(),
            "--deny-domain",
            "unasked.example",
        ][..],
    ] {
        let mut args = vec!["clusters", "approve", PLUGIN];
        args.extend_from_slice(extra);
        let refused = run_bookclerk(files, &args, &[]);
        assert!(!refused.status.success(), "{extra:?}: {}", stdout(&refused));
    }
    assert!(load_remote_plugin_consent(&store, CLUSTER, PLUGIN)
        .await
        .unwrap()
        .unwrap()
        .approved_at
        .is_none());

    let approved = json_out(&run_bookclerk(
        files,
        &[
            "--format",
            "json",
            "clusters",
            "approve",
            PLUGIN,
            "--request",
            &digest,
            "--deny-domain",
            "cdn.store.example",
            "--deny-binding",
            "secrets",
        ],
        &[],
    ));
    let consent = &approved["consent"];
    assert_eq!(consent["state"], "consented");
    assert_eq!(consent["approved_by"], "cli");
    assert_eq!(consent["grant"]["domains"], json!(["api.store.example"]));
    assert_eq!(consent["grant"]["bindings"], json!(["config"]));

    let revoked = json_out(&run_bookclerk(
        files,
        &["--format", "json", "clusters", "revoke", PLUGIN],
        &[],
    ));
    assert_eq!(revoked["consent"]["state"], "awaiting_host_consent");
    assert!(revoked["consent"]["grant"].is_null());
    let unknown = run_bookclerk(files, &["clusters", "revoke", "other"], &[]);
    assert!(!unknown.status.success());

    let unknown = run_bookclerk(files, &["clusters", "leave", "--cluster", "nope"], &[]);
    assert!(!unknown.status.success());
    let left = json_out(&run_bookclerk(
        files,
        &["--format", "json", "clusters", "leave"],
        &[],
    ));
    assert_eq!(left["cluster"]["state"], "left");
    let link = bookclerk_library::control_plane::load_cluster_link(&store, CLUSTER)
        .await
        .unwrap()
        .unwrap();
    assert!(
        load_cluster_link_keys(&store, &link).await.is_err(),
        "the node keys are gone"
    );
}

/// Records an active link to `cluster` as node `node`.
async fn link_to(store: &LibraryStore, cluster: &str, node: &str) {
    record_cluster_link(
        store,
        &NewClusterLink {
            owner_cluster_id: cluster.into(),
            owner_origin: "https://owner.example".into(),
            remote_node_id: node.into(),
            spki_pin: None,
        },
        &ClusterLinkKeys::generate(),
    )
    .await
    .expect("cluster link");
}

/// Without `--cluster`, `leave` acts on the only active link: a cluster
/// left earlier does not count, none active is "not enrolled", and several
/// active ask for `--cluster`.
#[tokio::test]
async fn leave_defaults_to_the_only_active_link() {
    let files_dir = enrolled();
    let files = files_dir.path();
    let store = open_store(files).await;
    link_to(&store, CLUSTER, "node-7").await;
    let left = json_out(&run_bookclerk(
        files,
        &["--format", "json", "clusters", "leave"],
        &[],
    ));
    assert_eq!(left["cluster"]["cluster_id"], CLUSTER);

    link_to(&store, "cluster-b", "node-8").await;
    let left = json_out(&run_bookclerk(
        files,
        &["--format", "json", "clusters", "leave"],
        &[],
    ));
    assert_eq!(
        left["cluster"]["cluster_id"], "cluster-b",
        "the left cluster is not a candidate"
    );

    let none = run_bookclerk(files, &["clusters", "leave"], &[]);
    assert!(!none.status.success());
    assert!(
        stderr(&none).contains("not enrolled in any cluster"),
        "{}",
        stderr(&none)
    );

    link_to(&store, "cluster-c", "node-9").await;
    link_to(&store, "cluster-d", "node-10").await;
    let several = run_bookclerk(files, &["clusters", "leave"], &[]);
    assert!(!several.status.success());
    assert!(
        stderr(&several).contains("pass --cluster"),
        "{}",
        stderr(&several)
    );
    let named = json_out(&run_bookclerk(
        files,
        &[
            "--format",
            "json",
            "clusters",
            "leave",
            "--cluster",
            CLUSTER,
        ],
        &[],
    ));
    assert_eq!(
        named["cluster"]["state"], "left",
        "a named cluster is taken whatever its state"
    );
}

/// `join` refuses a link on argv, empty stdin and a non-link, and never
/// prints the secret.
#[tokio::test]
async fn join_takes_the_link_only_from_stdin_and_refuses_a_non_link() {
    let files_dir = enrolled();
    let files = files_dir.path();

    // No positional link: the secret never rides on argv.
    let argv = run_bookclerk(
        files,
        &["clusters", "join", "https://owner.example/enroll#bcn1x"],
        &[],
    );
    assert!(!argv.status.success(), "{}", stdout(&argv));
    assert!(
        stderr(&argv).contains("takes no arguments"),
        "{}",
        stderr(&argv)
    );
    for out in [stdout(&argv), stderr(&argv)] {
        assert!(!out.contains("bcn1x"), "the link is not echoed: {out}");
    }

    let empty = join_with_stdin(files, "");
    assert!(!empty.status.success());
    assert!(
        stderr(&empty).contains("no enrollment link"),
        "{}",
        stderr(&empty)
    );

    let secret = "bcn1-not-a-real-token-secret";
    let bad = join_with_stdin(files, &format!("https://owner.example/enroll#{secret}\n"));
    assert!(!bad.status.success());
    let err = stderr(&bad);
    assert!(err.contains("not an enrollment link"), "{err}");
    assert!(!err.contains(secret), "the link is not echoed: {err}");
    assert!(!stdout(&bad).contains(secret));

    let store = open_store(files).await;
    assert!(bookclerk_library::control_plane::list_cluster_links(&store)
        .await
        .unwrap()
        .is_empty());
}

/// `bookclerk clusters join` running with stdin on a pseudo-terminal.
#[cfg(unix)]
struct PtyJoin {
    /// The pty's master side (the "keyboard and screen").
    master: std::fs::File,
    /// The running command.
    child: std::process::Child,
    /// Collects the command's stderr until it exits.
    reader: std::thread::JoinHandle<String>,
    /// The enrolled files directory, kept alive for the command.
    _files: tempfile::TempDir,
}

#[cfg(unix)]
impl PtyJoin {
    /// Starts `clusters join` on a fresh pty and waits for its prompt
    /// (printed once echo is off).
    #[allow(unsafe_code)]
    fn start() -> Self {
        use std::io::Read as _;
        use std::os::fd::FromRawFd as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        use std::sync::mpsc;
        use std::time::Duration;

        let files_dir = enrolled();

        // SAFETY: plain libc pty calls; every descriptor is owned by a
        // `File` right after it is opened.
        let (master, slave) = unsafe {
            let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(fd >= 0, "posix_openpt: {}", std::io::Error::last_os_error());
            let master = std::fs::File::from_raw_fd(fd);
            assert_eq!(libc::grantpt(fd), 0);
            assert_eq!(libc::unlockpt(fd), 0);
            let mut name = [0 as libc::c_char; 128];
            assert_eq!(libc::ptsname_r(fd, name.as_mut_ptr(), name.len()), 0);
            let path = std::ffi::CStr::from_ptr(name.as_ptr())
                .to_str()
                .unwrap()
                .to_string();
            let slave = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOCTTY)
                .open(path)
                .expect("open the pty slave");
            (master, slave)
        };

        let mut child = {
            use std::os::unix::process::CommandExt as _;

            let mut command = common::bookclerk_command(files_dir.path());
            command
                .args(["--format", "json", "clusters", "join"])
                .stdin(Stdio::from(slave))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // SAFETY: only async-signal-safe calls between fork and exec.
            // The command leads a new session whose controlling terminal is
            // the pty (its stdin by now), so a Ctrl+C typed on the master
            // reaches it as SIGINT the way it would from a real terminal.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0
                        || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            command.spawn().expect("spawn bookclerk")
            // The command (and its copy of the slave) is dropped here.
        };

        let mut err_pipe = child.stderr.take().unwrap();
        let (seen_tx, seen_rx) = mpsc::channel::<()>();
        let reader = std::thread::spawn(move || {
            let mut all = Vec::new();
            let mut buf = [0u8; 256];
            let mut told = false;
            loop {
                let n = err_pipe.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                all.extend_from_slice(&buf[..n]);
                if !told && String::from_utf8_lossy(&all).contains("enrollment link: ") {
                    told = true;
                    let _ = seen_tx.send(());
                }
            }
            String::from_utf8_lossy(&all).into_owned()
        });
        seen_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the prompt appears");
        Self {
            master,
            child,
            reader,
            _files: files_dir,
        }
    }

    /// Waits for the command; returns its status, its stderr, whether the
    /// terminal's echo is on afterwards, and what the terminal displayed
    /// (everything the slave side wrote back).
    #[allow(unsafe_code)]
    fn finish(mut self) -> (std::process::ExitStatus, String, bool, String) {
        use std::io::Read as _;
        use std::os::fd::AsRawFd as _;

        let status = self.child.wait().expect("wait for bookclerk");
        let err = self.reader.join().unwrap();
        let fd = self.master.as_raw_fd();
        // SAFETY: `fd` is the open master; termios is plain data.
        let echo_on = unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            let mut settings: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(fd, &mut settings), 0);
            settings.c_lflag & libc::ECHO != 0
        };
        let mut shown = Vec::new();
        let mut buf = [0u8; 1024];
        // EAGAIN (nothing more) or EIO (no slave open) both end the read.
        while let Ok(n) = self.master.read(&mut buf) {
            if n == 0 {
                break;
            }
            shown.extend_from_slice(&buf[..n]);
        }
        (
            status,
            err,
            echo_on,
            String::from_utf8_lossy(&shown).into_owned(),
        )
    }
}

/// On a terminal, `join` reads the link with echo off: the pseudo-terminal
/// shows the prompt's side of the exchange but never the link, and the
/// terminal's echo is back on once the command exits.
#[cfg(unix)]
#[test]
fn join_on_a_terminal_does_not_echo_the_link() {
    let mut join = PtyJoin::start();
    let secret = "bcn1-typed-on-a-terminal-secret";
    join.master
        .write_all(format!("https://owner.example/enroll#{secret}\n").as_bytes())
        .unwrap();
    let (status, err, echo_on, shown) = join.finish();
    assert!(!status.success());
    assert!(err.contains("not an enrollment link"), "{err}");
    assert!(!err.contains(secret), "the link is not echoed: {err}");
    assert!(
        !shown.contains(secret),
        "the terminal echoed the link: {shown:?}"
    );
    assert!(echo_on, "echo is back on after the command");
}

/// An interrupt at the prompt (Ctrl+C, so SIGINT) still restores the
/// terminal's echo, and the command dies of that signal as it would
/// without the handler.
#[cfg(unix)]
#[test]
fn an_interrupt_at_the_prompt_restores_echo() {
    use std::os::unix::process::ExitStatusExt as _;

    let mut join = PtyJoin::start();
    // Half a link typed, then Ctrl+C. Both go through the terminal's input
    // in order, so the half link is taken in with echo still off before
    // the interrupt restores it (a signal sent with `kill` could overtake
    // input the terminal had not processed yet, which would then be echoed).
    let secret = "bcn1-half-typed-secret";
    join.master
        .write_all(format!("https://owner.example/enroll#{secret}\x03").as_bytes())
        .unwrap();
    let (status, err, echo_on, shown) = join.finish();
    assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?}: {err}");
    assert!(echo_on, "echo is back on after the interrupt");
    assert!(
        !shown.contains(secret),
        "the terminal echoed the link: {shown:?}"
    );
}
