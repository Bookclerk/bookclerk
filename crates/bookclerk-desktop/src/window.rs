//! The Tauri window and the `bookclerk` custom scheme that feeds it.

use std::sync::Arc;

use bookclerkd::embedded::{EmbeddedHost, EmbeddedOptions};
use tauri::{RunEvent, Url, WebviewUrl, WebviewWindowBuilder};

/// The custom scheme the web app is served on.
const SCHEME: &str = "bookclerk";

/// The name the OS log facility records for this process.
const LOG_IDENTIFIER: &str = "bookclerk-desktop";

/// The window's first page. WebView2 (Windows) only loads custom schemes
/// as `http://<scheme>.localhost`; the other webviews take `<scheme>://`.
#[cfg(windows)]
const START_URL: &str = "http://bookclerk.localhost/";
/// The window's first page. WebView2 (Windows) only loads custom schemes
/// as `http://<scheme>.localhost`; the other webviews take `<scheme>://`.
#[cfg(not(windows))]
const START_URL: &str = "bookclerk://localhost/";

/// Starts the host, opens the window and runs until it closes.
///
/// # Errors
///
/// Returns an error when the Tokio runtime cannot be built, the host does
/// not start (see [`EmbeddedHost::start`]), or Tauri cannot build the app.
pub fn run() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("bookclerk-host")
        .build()?;
    let host = Arc::new(runtime.block_on(EmbeddedHost::start(
        EmbeddedOptions::default(),
        LOG_IDENTIFIER,
    ))?);
    let start = Url::parse(START_URL)?;

    let scheme_host = host.clone();
    let spawner = runtime.handle().clone();
    let app = tauri::Builder::default()
        .register_asynchronous_uri_scheme_protocol(SCHEME, move |_ctx, request, responder| {
            let host = scheme_host.clone();
            spawner.spawn(async move {
                responder.respond(host.handle(request).await);
            });
        })
        .setup(move |app| {
            WebviewWindowBuilder::new(app, "main", WebviewUrl::CustomProtocol(start))
                .title("Bookclerk")
                .inner_size(1280.0, 820.0)
                .min_inner_size(480.0, 360.0)
                .on_navigation(|url| {
                    let allowed = is_app_url(url);
                    if !allowed {
                        tracing::info!(url = %url, "window refused to navigate away from the app");
                    }
                    allowed
                })
                .build()?;
            Ok(())
        })
        .build(tauri::generate_context!())?;

    app.run(move |_app, event| {
        if let RunEvent::Exit = event {
            runtime.block_on(host.shutdown());
        }
    });
    Ok(())
}

/// Whether `url` is a page of the app itself (the custom scheme).
///
/// # Arguments
///
/// * `url` - Where the window is about to navigate.
///
/// # Returns
///
/// `true` only for the custom scheme (or its WebView2 form on Windows).
fn is_app_url(url: &Url) -> bool {
    if cfg!(windows) {
        url.scheme() == "http" && url.host_str() == Some("bookclerk.localhost")
    } else {
        url.scheme() == SCHEME && url.host_str() == Some("localhost")
    }
}
