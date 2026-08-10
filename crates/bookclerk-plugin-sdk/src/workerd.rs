//! Workerd guest bridge — same [`crate::BookclerkPlugin`] contract as native.
//!
//! Dual-stack with native guests:
//!
//! - **Native:** `use bookclerk_plugin_sdk::{BookclerkPlugin, BookclerkPluginGuest};`
//! - **Workerd:** JS modules import the package; Rust implements ABI dispatch:
//!
//! ```js
//! import { wasmBookclerkPlugin } from "@bookclerk/plugin-sdk/workerd";
//! import { initSync, dispatch } from "./pkg/….js";
//! import wasmModule from "./pkg/…_bg.wasm";
//! initSync({ module: wasmModule });
//! export default wasmBookclerkPlugin(dispatch);
//! ```
//!
//! `bookclerk-workerd` injects `@bookclerk/plugin-sdk/workerd` (and the package
//! root) into the isolate — authors do not vendor a relative filepath.
//!
//! Layout (see `examples/plugins-echo-workerd-rust/`):
//!
//! ```text
//! plugin.toml                 runtime = "workerd"
//! modules/index.js            package import + wasm glue
//! modules/pkg/*.js + *.wasm   wasm-bindgen output (`./build-wasm.sh`)
//! src/lib.rs                  ABI-typed `dispatch_json`
//! ```

pub use crate::plugin::BookclerkPlugin;

/// Suggested main module filename for JS+Wasm workerd guests.
pub const DEFAULT_MAIN_MODULE: &str = "index.js";

/// Suggested modules directory (matches `plugin.toml` default).
pub const DEFAULT_MODULES_DIR: &str = "modules";

/// Filename used by optional `sync-embed` vendors (prefer package imports).
pub const EMBED_BOOKCLERK_PLUGIN_JS: &str = "bookclerk_plugin.js";

/// Embeddable workerd runtime (`BookclerkPlugin` + `wasmBookclerkPlugin`).
pub const EMBED_BOOKCLERK_PLUGIN_JS_SRC: &str = include_str!("../embed/bookclerk_plugin.js");
