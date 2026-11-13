//! Tests for the daemon's startup: [`run`]'s early exits and
//! [`assemble_host`] on a control plane opened over an in-process database,
//! the way the other daemon tests open one, since [`start_host`] would
//! launch the database plugin as a jailed guest.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bookclerk_config::Config;
use tower::ServiceExt;

use super::embedded::{EmbeddedHost, EmbeddedOptions};
use super::{assemble_host, run, serve_listeners, Args, HostMode, OpenApiVersion};

/// Opens the control plane over an in-process SQLite database in `files`,
/// with `config_toml` as its `config.toml`.
async fn opened(
    files: &tempfile::TempDir,
    config_toml: &str,
) -> bookclerk_plugin_host::HostControlPlane {
    let files_path = files.path();
    std::fs::write(files_path.join("config.toml"), config_toml).unwrap();
    let config = Config::load(
        Some(files_path.to_path_buf()),
        Some(files_path.join("config.toml")),
    )
    .unwrap();
    config.paths().ensure_dirs().unwrap();
    let db = bookclerk_plugin_database_sqlite::open(&files_path.join("library.db"))
        .await
        .unwrap();
    bookclerk_library::apply_host_schema(&db).await.unwrap();
    let store = bookclerk_library::LibraryStore::from_connection(db);
    bookclerk_plugin_host::open_host_control_plane_with_store(
        &config,
        bookclerk_plugin_host::DatabaseRegistry::default(),
        store,
    )
    .await
    .unwrap()
}

/// Arguments that point the daemon at `files` and nothing else.
fn args_for(files: &tempfile::TempDir) -> Args {
    Args {
        bookclerk_files: Some(files.path().to_path_buf()),
        config: None,
        listen: None,
        print_openapi: None,
    }
}

#[tokio::test]
async fn an_embedded_host_serves_its_router_without_binding_anything() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let host = assemble_host(opened(&files, "").await, HostMode::Embedded)
        .await
        .unwrap();
    assert!(host.state.last_bound_listen.read().await.is_none());
    let response = host
        .app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    host.process_shutdown
        .store(true, std::sync::atomic::Ordering::SeqCst);
}

#[tokio::test]
async fn a_listening_host_refuses_an_unsafe_listen_address() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let config = "[daemon]\nlisten = \"0.0.0.0:0\"\n\n[daemon.auth]\nenabled = false\n";
    let Err(err) = assemble_host(opened(&files, config).await, HostMode::Listening).await else {
        panic!("a non-loopback listen without auth must be refused");
    };
    assert!(err.to_string().contains("unsafe"), "{err}");
}

#[tokio::test]
async fn an_embedded_host_ignores_the_listen_address() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let config = "[daemon]\nlisten = \"0.0.0.0:0\"\n\n[daemon.auth]\nenabled = false\n";
    let host = assemble_host(opened(&files, config).await, HostMode::Embedded).await;
    assert!(host.is_ok(), "nothing is bound, so listen is not checked");
}

#[tokio::test]
async fn run_prints_the_openapi_document_and_starts_nothing() {
    for version in [OpenApiVersion::V31, OpenApiVersion::V30] {
        let files = tempfile::tempdir().unwrap();
        let args = Args {
            print_openapi: Some(version),
            ..args_for(&files)
        };
        run(args).await.unwrap();
        assert!(
            !files.path().join("library.db").exists(),
            "printing the document opens no library"
        );
    }
}

#[tokio::test]
async fn run_refuses_an_unparsable_listen_flag_before_starting() {
    let files = tempfile::tempdir().unwrap();
    let args = Args {
        listen: Some("not an address".into()),
        ..args_for(&files)
    };
    let err = run(args).await.unwrap_err();
    assert!(err.to_string().contains("--listen"), "{err}");
    assert!(!files.path().join("library.db").exists());
}

#[tokio::test]
async fn the_desktop_host_answers_as_the_owner_on_an_assembled_host() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let parts = assemble_host(opened(&files, "").await, HostMode::Embedded)
        .await
        .unwrap();
    let host = EmbeddedHost::from_host(parts);
    let response = host
        .handle(Request::get("/api/status").body(Vec::new()).unwrap())
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the window is signed in as the owner without a token of its own"
    );
    host.shutdown().await;
}

#[tokio::test]
async fn the_desktop_host_refuses_to_start_on_an_interrupted_shared_branch_join() {
    let files = tempfile::tempdir().unwrap();
    // A copy beside `config.toml` is what a shared branch join that died while
    // replacing the bootstrap files leaves behind.
    std::fs::write(files.path().join("config.toml.pre-shared-branch"), "").unwrap();
    let options = EmbeddedOptions {
        bookclerk_files: Some(files.path().to_path_buf()),
        config: None,
    };
    let Err(err) = EmbeddedHost::start(options, "bookclerk-desktop-test").await else {
        panic!("an interrupted shared branch join must stop the start");
    };
    assert!(err.to_string().contains("interrupted"), "{err}");
    assert!(!files.path().join("library.db").exists());
}

#[tokio::test]
async fn a_listening_host_binds_serves_and_stops_on_shutdown() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let config = "[daemon]\nlisten = \"127.0.0.1:0\"\ntray = false\n";
    let parts = assemble_host(opened(&files, config).await, HostMode::Listening)
        .await
        .unwrap();
    let state = parts.state.clone();
    let reload = parts.listen_reload.clone();
    let shutdown = parts.process_shutdown.clone();
    let served = tokio::spawn(serve_listeners(parts));
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while state.last_bound_listen.read().await.is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the listener binds");
    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    // The listeners stop on a reload notice too; with the flag set, the
    // daemon then exits instead of rebinding.
    reload.notify_waiters();
    tokio::time::timeout(std::time::Duration::from_secs(30), served)
        .await
        .expect("the daemon stops")
        .unwrap()
        .unwrap();
}
