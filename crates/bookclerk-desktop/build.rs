//! Generates the Tauri context (`tauri.conf.json`, and the capability in
//! `capabilities/` that allows the page its one command, `live`) when the
//! `window` feature is on; does nothing otherwise.

fn main() {
    #[cfg(feature = "window")]
    {
        // Declaring the app's commands makes Tauri check them against the
        // capabilities, so `live` runs only where `capabilities/main.json`
        // allows it.
        let attributes = tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(&["live"]));
        if let Err(err) = tauri_build::try_build(attributes) {
            // A build script reports failure by panicking.
            panic!("tauri-build failed: {err:#}");
        }
    }
}
