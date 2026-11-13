//! Generates the Tauri context (`tauri.conf.json`, the empty capability set)
//! when the `window` feature is on; does nothing otherwise.

fn main() {
    #[cfg(feature = "window")]
    tauri_build::build();
}
