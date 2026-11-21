//! Which origins webhook URLs are handed out on.

use super::*;

#[test]
fn only_public_https_origins_get_webhook_urls() {
    let auto = WebhooksExposure::Auto;
    for (origin, expected) in [
        (
            "https://library.example.org",
            Some("https://library.example.org"),
        ),
        (
            "https://library.example.org/",
            Some("https://library.example.org"),
        ),
        (
            "https://books.example.net:8443",
            Some("https://books.example.net:8443"),
        ),
        ("https://203.0.113.9", None),
        ("https://8.8.8.8", Some("https://8.8.8.8")),
        (
            "https://[2606:4700::1111]",
            Some("https://[2606:4700::1111]"),
        ),
        ("http://library.example.org", None),
        ("https://localhost:8787", None),
        ("https://nas.local", None),
        ("https://nas", None),
        ("https://library.home.arpa", None),
        ("https://192.168.1.10", None),
        ("https://10.0.0.1", None),
        ("https://100.64.1.1", None),
        ("https://127.0.0.1", None),
        ("https://[::1]", None),
        ("https://[fd00::1]", None),
        ("https://[fe80::1]", None),
        ("https://[::ffff:192.168.1.1]", None),
        ("https://library.example.org/sub", None),
        ("https://user@library.example.org", None),
        ("", None),
    ] {
        assert_eq!(
            webhook_origin(Some(origin), auto).as_deref(),
            expected,
            "{origin}"
        );
    }
    assert_eq!(webhook_origin(None, auto), None);
    assert_eq!(
        webhook_origin(Some("https://library.example.org"), WebhooksExposure::Off),
        None
    );
}

mod route {
    //! The public route's refusals before a request reaches a running guest.

    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use bookclerk_config::{Config, WebhooksExposure};
    use bookclerk_library::control_plane::{
        bootstrap_control_plane, webhook_url_token, PluginInstanceId,
    };
    use tower::ServiceExt;

    use crate::api::AppState;

    /// A control plane over a fresh library, and the app serving the route.
    async fn app(files: &std::path::Path) -> (Arc<AppState>, axum::Router) {
        std::fs::write(files.join("config.toml"), "").unwrap();
        let db = bookclerk_plugin_database_sqlite::open(&files.join("library.db"))
            .await
            .unwrap();
        bookclerk_library::apply_host_schema(&db).await.unwrap();
        let store = bookclerk_library::LibraryStore::from_connection(db);
        bootstrap_control_plane(
            &store,
            files,
            &files.join("cache"),
            None,
            &Config::default(),
        )
        .await
        .unwrap();
        let config =
            Config::load(Some(files.to_path_buf()), Some(files.join("config.toml"))).unwrap();
        let state = Arc::new(crate::config_authority::control_plane_test_state(
            store, config,
        ));
        let app = super::webhook_router::<()>(Arc::clone(&state));
        (state, app)
    }

    async fn post(app: &axum::Router, uri: &str) -> StatusCode {
        let request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        app.clone().oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn requests_are_refused_until_a_node_runs_the_instance() {
        let _dek = crate::auth::tests::process_dek_lock().await;
        let files = tempfile::tempdir().unwrap();
        let (state, app) = app(files.path()).await;

        // A path that is not a token, an unknown token, and a bad name.
        assert_eq!(
            post(&app, "/hooks/short/events").await,
            StatusCode::NOT_FOUND
        );
        let unknown = "A".repeat(43);
        assert_eq!(
            post(&app, &format!("/hooks/{unknown}/events")).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post(&app, &format!("/hooks/{unknown}/Bad_Name")).await,
            StatusCode::NOT_FOUND
        );

        // A real token answers 404 for another row's name, and 503 while no
        // deployment on this node runs the instance.
        let instance = PluginInstanceId::mint();
        let token = {
            let library = state.library_snapshot().await;
            webhook_url_token(&library, &instance, "events")
                .await
                .unwrap()
        };
        assert_eq!(
            post(&app, &format!("/hooks/{}/other", token.as_str())).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post(&app, &format!("/hooks/{}/events", token.as_str())).await,
            StatusCode::SERVICE_UNAVAILABLE
        );

        // Turning webhooks off closes the route.
        state.config.write().await.plugins.webhooks = WebhooksExposure::Off;
        assert_eq!(
            post(&app, &format!("/hooks/{}/events", token.as_str())).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn publish_origin_follows_the_config() {
        let mut config = Config::default();
        config.integrations.public_origin = Some("https://library.example.org".into());
        config.plugins.webhooks = WebhooksExposure::Auto;
        super::publish_origin(&config);
        assert_eq!(
            bookclerk_plugin_host::webhook_public_origin().as_deref(),
            Some("https://library.example.org")
        );
        config.plugins.webhooks = WebhooksExposure::Off;
        super::publish_origin(&config);
        assert_eq!(bookclerk_plugin_host::webhook_public_origin(), None);
    }
}
