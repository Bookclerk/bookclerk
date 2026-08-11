//! Workerd guest bridge — same [`crate::BookclerkPlugin`] contract as native.
//!
//! Audience: authors of `runtime = "workerd"` plugins (JS modules and optional
//! Rust→Wasm). Dual-stack with native guests:
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
//! root) into the isolate — authors do not vendor a relative filepath. Optional
//! offline vendors can copy [`EMBED_BOOKCLERK_PLUGIN_JS_SRC`] via
//! `bookclerk-plugin sync-embed` (feature `tools`).
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
///
/// Matches the common `plugin.toml` `[workerd] main_module = "index.js"` default
/// and the layout under [`DEFAULT_MODULES_DIR`].
pub const DEFAULT_MAIN_MODULE: &str = "index.js";

/// Suggested modules directory name relative to the plugin root.
///
/// Matches the `plugin.toml` `[workerd] modules_dir` default (`"modules"`).
pub const DEFAULT_MODULES_DIR: &str = "modules";

/// Filename historically used by optional `sync-embed` vendors.
///
/// Prefer importing `@bookclerk/plugin-sdk/workerd` (injected by
/// `bookclerk-workerd`) over shipping this file next to guest modules.
pub const EMBED_BOOKCLERK_PLUGIN_JS: &str = "bookclerk_plugin.js";

/// Source text of the embeddable workerd runtime helper.
///
/// Contains the `BookclerkPlugin` base class and `wasmBookclerkPlugin` factory
/// mirrored by the npm package. Written to
/// `modules/@bookclerk/plugin-sdk/workerd.js` by [`crate::tools::sync_embed`]
/// when authors need an offline vendor (feature `tools`).
pub const EMBED_BOOKCLERK_PLUGIN_JS_SRC: &str = include_str!("../embed/bookclerk_plugin.js");
