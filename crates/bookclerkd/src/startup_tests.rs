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
async fn the_desktop_host_answers_as_the_operator_before_an_owner_exists() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let parts = assemble_host(opened(&files, "").await, HostMode::Embedded)
        .await
        .unwrap();
    let host = EmbeddedHost::from_host(parts, None);
    let response = host
        .handle(Request::get("/api/status").body(Vec::new()).unwrap())
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the window is the operator without a token of its own"
    );
    host.shutdown().await;
}

/// A JSON request from the window's own page.
fn window_json(method: &str, path: &str, body: &str) -> Request<Vec<u8>> {
    Request::builder()
        .method(method)
        .uri(format!("bookclerk://localhost{path}"))
        .header("origin", "bookclerk://localhost")
        .header("content-type", "application/json")
        .body(body.as_bytes().to_vec())
        .unwrap()
}

/// The window's `GET /api/auth/me` answer.
async fn who(host: &EmbeddedHost) -> (StatusCode, serde_json::Value) {
    let response = host.handle(window_json("GET", "/api/auth/me", "")).await;
    let status = response.status();
    let body = serde_json::from_slice(response.body()).unwrap_or(serde_json::Value::Null);
    (status, body)
}

#[tokio::test]
async fn the_desktop_window_opens_as_its_kept_patron_and_elevates_like_the_web() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let files = tempfile::tempdir().unwrap();
    let node = "window-test-node";
    // Public origin set: the window's cookie requests still pass the CSRF check.
    let config = "[integrations]\npublic_origin = \"https://library.example\"\n";
    let parts = assemble_host(opened(&files, config).await, HostMode::Embedded)
        .await
        .unwrap();
    let state = parts.state.clone();
    let host = EmbeddedHost::from_host(parts, Some(node.to_owned()));
    let library = state.library_snapshot().await;
    let kept = || async {
        crate::embedded::session::remembered(&library, node)
            .await
            .unwrap()
    };

    // First run: the operator creates the Owner, and the window becomes them.
    let created = host
        .handle(window_json(
            "POST",
            "/api/auth/bootstrap",
            r#"{"login_name":"dana","password":"correct horse battery staple"}"#,
        ))
        .await;
    assert_eq!(created.status(), StatusCode::OK);
    assert!(created.headers().get("set-cookie").is_none());
    let first = kept().await.expect("session kept");
    let (status, me) = who(&host).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["role"], "owner", "{me}");
    assert_eq!(me["elevated"], false);

    // Server settings need the operator; the Owner elevates with a password.
    let refused = host
        .handle(window_json("GET", "/api/config/plugin-instances", ""))
        .await;
    assert!(refused.status().is_client_error(), "{}", refused.status());
    let elevated = host
        .handle(window_json(
            "POST",
            "/api/auth/elevate",
            r#"{"password":"correct horse battery staple"}"#,
        ))
        .await;
    assert_eq!(elevated.status(), StatusCode::OK);
    assert!(elevated.headers().get("set-cookie").is_none());
    let (_, me) = who(&host).await;
    assert_eq!(me["elevated"], true, "{me}");
    let status = host
        .handle(window_json("GET", "/api/config/plugin-instances", ""))
        .await;
    assert_eq!(status.status(), StatusCode::OK);

    // Elevated, the Owner impersonates a Member and stops, as on the web.
    let member = host
        .handle(window_json(
            "POST",
            "/api/users",
            r#"{"role":"member","login_name":"sam","mint_invite":false}"#,
        ))
        .await;
    assert!(member.status().is_success(), "{}", member.status());
    let member: serde_json::Value = serde_json::from_slice(member.body()).unwrap();
    let member_id = member["user"]["id"].as_i64().expect("new member id");
    let acting = host
        .handle(window_json(
            "POST",
            "/api/auth/impersonate",
            &format!(r#"{{"user_id":{member_id}}}"#),
        ))
        .await;
    assert_eq!(acting.status(), StatusCode::OK);
    let (_, me) = who(&host).await;
    assert_eq!(me["impersonating"]["user_id"], member_id, "{me}");
    let stopped = host
        .handle(window_json("DELETE", "/api/auth/impersonate", ""))
        .await;
    assert_eq!(stopped.status(), StatusCode::OK);
    let (_, me) = who(&host).await;
    assert!(me["impersonating"].is_null(), "{me}");
    assert_eq!(me["elevated"], true, "{me}");

    let ended = host
        .handle(window_json("DELETE", "/api/auth/elevate", ""))
        .await;
    assert_eq!(ended.status(), StatusCode::OK);
    let (_, me) = who(&host).await;
    assert_eq!(
        (me["role"].clone(), me["elevated"].clone()),
        ("owner".into(), false.into())
    );
    assert_eq!(
        kept().await,
        Some(first.clone()),
        "elevating and impersonating never rewrite the kept session"
    );

    // The next launch opens as the same patron, on a fresh session.
    let reopened = host.relaunched();
    reopened.open_session().await;
    let renewed = kept().await.expect("session still kept");
    assert_ne!(renewed, first, "the session is swapped at launch");
    assert_eq!(
        crate::embedded::session::patron(&library, &first)
            .await
            .unwrap(),
        None
    );
    let (_, me) = who(&reopened).await;
    assert_eq!(me["role"], "owner", "{me}");

    // Signing out forgets this computer, and the window asks for a sign-in.
    let out = reopened
        .handle(window_json("POST", "/api/auth/logout", "{}"))
        .await;
    assert!(out.status().is_success(), "{}", out.status());
    assert_eq!(kept().await, None);
    let (status, _) = who(&reopened).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Signing in again remembers the patron.
    let signed_in = reopened
        .handle(window_json(
            "POST",
            "/api/auth/password",
            r#"{"login":"dana","password":"correct horse battery staple"}"#,
        ))
        .await;
    assert_eq!(
        signed_in.status(),
        StatusCode::OK,
        "{:?}",
        String::from_utf8_lossy(signed_in.body())
    );
    let current = kept().await.expect("the sign-in is kept");
    // The sign-in's web session was swapped for a window session.
    let sessions = reopened
        .handle(window_json("GET", "/api/auth/sessions", ""))
        .await;
    let sessions: serde_json::Value = serde_json::from_slice(sessions.body()).unwrap();
    let current_session = sessions["sessions"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["is_current"] == true))
        .unwrap_or_else(|| panic!("no current session in {sessions}"));
    assert!(
        current_session["client_label"]
            .as_str()
            .is_some_and(|label| label.starts_with("Desktop app on")),
        "{current_session}"
    );

    // The portal's own sign-out forgets the window's session too.
    let out = reopened
        .handle(window_json("POST", "/api/portal/logout", "{}"))
        .await;
    assert!(out.status().is_success(), "{}", out.status());
    assert_eq!(kept().await, None);
    let signed_in = reopened
        .handle(window_json(
            "POST",
            "/api/auth/password",
            r#"{"login":"dana","password":"correct horse battery staple"}"#,
        ))
        .await;
    assert_eq!(signed_in.status(), StatusCode::OK);
    assert_ne!(kept().await, Some(current));
    let current = kept().await.expect("the new sign-in is kept");

    // A revoked session sends the next launch to the sign-in screen.
    crate::embedded::session::end(&library, &current).await;
    let revoked = host.relaunched();
    revoked.open_session().await;
    assert_eq!(kept().await, None);
    let (status, _) = who(&revoked).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
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
