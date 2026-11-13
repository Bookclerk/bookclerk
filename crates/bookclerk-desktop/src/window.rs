//! The Tauri window and the `bookclerk` custom scheme that feeds it.

use std::sync::Arc;

use bookclerkd::embedded::{EmbeddedHost, EmbeddedOptions, Navigation};
use tauri::{Manager as _, RunEvent, Url, WebviewUrl, WebviewWindowBuilder};

/// The custom scheme the web app is served on.
const SCHEME: &str = "bookclerk";

/// The name the OS log facility records for this process.
const LOG_IDENTIFIER: &str = "bookclerk-desktop";

/// The app's origin. WebView2 (Windows) only loads custom schemes as
/// `http://<scheme>.localhost`; the other webviews take `<scheme>://`.
#[cfg(windows)]
const APP_ORIGIN: &str = "http://bookclerk.localhost";
/// The app's origin. WebView2 (Windows) only loads custom schemes as
/// `http://<scheme>.localhost`; the other webviews take `<scheme>://`.
#[cfg(not(windows))]
const APP_ORIGIN: &str = "bookclerk://localhost";

/// The window's label.
const WINDOW_LABEL: &str = "main";

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
    let start = Url::parse(&format!("{APP_ORIGIN}/"))?;

    let scheme_host = host.clone();
    let navigation_host = host.clone();
    let spawner = runtime.handle().clone();
    let app = tauri::Builder::default()
        .register_asynchronous_uri_scheme_protocol(SCHEME, move |_ctx, request, responder| {
            let host = scheme_host.clone();
            spawner.spawn(async move {
                responder.respond(host.handle(request).await);
            });
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::CustomProtocol(start))
                .title("Bookclerk")
                .inner_size(1280.0, 820.0)
                .min_inner_size(480.0, 360.0)
                .on_navigation(move |url| match navigation_host.navigation(url.as_str()) {
                    Navigation::App | Navigation::JoinSignIn => true,
                    Navigation::JoinReturn(path_and_query) => {
                        return_to_app(&handle, &path_and_query);
                        false
                    }
                    Navigation::Refuse => {
                        tracing::info!(url = %url, "window refused to navigate away from the app");
                        false
                    }
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

/// Opens `path_and_query` on the app scheme: where the library's return
/// from a join sign-in lands, so the app finishes the join.
///
/// # Arguments
///
/// * `handle` - The running app.
/// * `path_and_query` - The return path and its query (code and state).
fn return_to_app(handle: &tauri::AppHandle, path_and_query: &str) {
    let target = match Url::parse(&format!("{APP_ORIGIN}{path_and_query}")) {
        Ok(target) => target,
        Err(err) => {
            tracing::warn!(error = %err, "join return address is not a valid app URL");
            return;
        }
    };
    let window_handle = handle.clone();
    // Navigating from inside the navigation callback would re-enter the
    // webview; do it on the next turn of the main loop instead.
    let queued = handle.run_on_main_thread(move || {
        if let Some(window) = window_handle.get_webview_window(WINDOW_LABEL) {
            if let Err(err) = window.navigate(target) {
                tracing::warn!(error = %err, "could not return the window to the app");
            }
        }
    });
    if let Err(err) = queued {
        tracing::warn!(error = %err, "could not queue the window's return to the app");
    }
}
