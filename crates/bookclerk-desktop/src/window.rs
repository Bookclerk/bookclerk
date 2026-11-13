//! The Tauri window and the `bookclerk` custom scheme that feeds it.

use std::sync::{Arc, Mutex};

use bookclerkd::embedded::{
    live_refusal, EmbeddedHost, EmbeddedOptions, LiveRefusal, Navigation, MAIN_WINDOW_LABEL,
};
use tauri::ipc::Channel;
use tauri::{Manager as _, RunEvent, State, Url, Webview, WebviewUrl, WebviewWindowBuilder};
use tokio::runtime::Handle;
use tokio::task::AbortHandle;

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

/// What the `live` command needs: the host, the runtime its feed runs on,
/// and the feed running now, which the next call replaces.
struct LiveFeed {
    /// The in-process host whose live frames are fed.
    host: Arc<EmbeddedHost>,
    /// The runtime the host runs on.
    runtime: Handle,
    /// The feed for the page that called last; a page that reloads or
    /// reconnects calls again, and the old feed stops.
    current: Mutex<Option<AbortHandle>>,
}

/// The page's live updates: starts feeding `on_frame` the frames the
/// `/api/live` socket would carry (see [`EmbeddedHost::live`]), replacing
/// any feed an earlier call started.
///
/// The only command the window allows (`capabilities/main.json`), and only
/// on the app's own pages: the library's sign-in pages the window shows
/// during a join are remote, so Tauri refuses them. The page passes
/// nothing but the channel.
///
/// # Arguments
///
/// * `webview` - The calling webview; anything but the main window on
///   one of the app's own pages is refused (see [`live_refusal`]).
/// * `feed` - The host and the current feed.
/// * `on_frame` - The page's channel; each frame is its JSON text.
///
/// # Errors
///
/// Returns an error when another webview, or a page that is not the app's
/// own, calls it.
#[tauri::command]
fn live(
    webview: Webview,
    feed: State<'_, LiveFeed>,
    on_frame: Channel<String>,
) -> Result<(), String> {
    let page = webview.url().map(String::from).unwrap_or_default();
    match live_refusal(webview.label(), &page) {
        None => {}
        Some(LiveRefusal::OtherWebview) => {
            return Err("live updates are for the main window".into());
        }
        Some(LiveRefusal::ForeignPage) => {
            return Err("live updates are for the app's own pages".into());
        }
    }
    let host = Arc::clone(&feed.host);
    let task = feed.runtime.spawn(async move {
        host.live(move |text| on_frame.send(text).is_ok()).await;
    });
    let previous = match feed.current.lock() {
        Ok(mut current) => current.replace(task.abort_handle()),
        Err(poisoned) => poisoned.into_inner().replace(task.abort_handle()),
    };
    let replaced = previous.is_some();
    if let Some(previous) = previous {
        previous.abort();
    }
    tracing::debug!(replaced, "window live updates started");
    Ok(())
}

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
    let live_feed = LiveFeed {
        host: host.clone(),
        runtime: runtime.handle().clone(),
        current: Mutex::new(None),
    };
    let app = tauri::Builder::default()
        .manage(live_feed)
        .invoke_handler(tauri::generate_handler![live])
        .register_asynchronous_uri_scheme_protocol(SCHEME, move |_ctx, request, responder| {
            let host = scheme_host.clone();
            spawner.spawn(async move {
                responder.respond(host.handle(request).await);
            });
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::CustomProtocol(start))
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
        if let Some(window) = window_handle.get_webview_window(MAIN_WINDOW_LABEL) {
            if let Err(err) = window.navigate(target) {
                tracing::warn!(error = %err, "could not return the window to the app");
            }
        }
    });
    if let Err(err) = queued {
        tracing::warn!(error = %err, "could not queue the window's return to the app");
    }
}
