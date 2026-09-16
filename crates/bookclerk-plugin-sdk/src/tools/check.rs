//! `bookclerk-plugin check` and optional workerd embed sync.
//!
//! Audience: authors validating on-disk plugin layout before packaging or
//! loading. Always available (no `tools` feature required for these library
//! entry points); the CLI wrapper lives behind feature `tools`.

use std::path::{Path, PathBuf};

use bookclerk_plugin_manifest::{parse, PluginRuntimeKind};

use crate::error::{Result, SdkError};

/// Validates `plugin.toml` and the on-disk layout under `plugin_dir`.
///
/// Checks embedded logo paths, native `command` presence when
/// `.require-binary` is set, and workerd `modules_dir` / `main_module` plus a
/// light JS import heuristic for `@bookclerk/plugin-sdk`.
///
/// # Arguments
///
/// * `plugin_dir` - Directory containing `plugin.toml` (usually `.` or the
///   staged plugin root).
///
/// # Returns
///
/// Human-readable success line (`ok id=… kind=… runtime=…`) for CLI stdout.
///
/// # Errors
///
/// Returns [`SdkError`] when the manifest is missing/invalid, required files
/// are absent, or a workerd main module looks like a bare `WorkerEntrypoint`
/// without the Bookclerk base class.
pub fn check_plugin(plugin_dir: &Path) -> Result<String> {
    let toml_path = plugin_dir.join("plugin.toml");
    let text = std::fs::read_to_string(&toml_path)
        .map_err(|e| SdkError::message(format!("read {}: {e}", toml_path.display())))?;
    let manifest = parse(&text).map_err(|e| SdkError::message(e.to_string()))?;

    if let Some(logo) = manifest.logo.as_deref() {
        let kind = bookclerk_plugin_manifest::validate_logo(logo)
            .map_err(|e| SdkError::message(e.to_string()))?;
        if let bookclerk_plugin_manifest::LogoKind::EmbeddedPath(rel) = kind {
            let path = plugin_dir.join(&rel);
            if !path.is_file() {
                return Err(SdkError::message(format!(
                    "embedded logo missing: {}",
                    path.display()
                )));
            }
        }
    }

    match manifest.runtime {
        PluginRuntimeKind::Native => {
            let Some(cmd) = manifest.command.as_ref() else {
                return Err(SdkError::message("native plugin missing command"));
            };
            let resolved = resolve_plugin_path(plugin_dir, cmd);
            if !resolved.exists() && plugin_dir.join(".require-binary").exists() {
                return Err(SdkError::message(format!(
                    "native command not found: {}",
                    resolved.display()
                )));
            }
        }
        PluginRuntimeKind::Workerd => {
            let w = manifest
                .workerd
                .as_ref()
                .ok_or_else(|| SdkError::message("workerd config missing"))?;
            let modules_dir = plugin_dir.join(&w.modules_dir);
            if !modules_dir.is_dir() {
                return Err(SdkError::message(format!(
                    "workerd modules_dir missing: {}",
                    modules_dir.display()
                )));
            }
            let main = modules_dir.join(&w.main_module);
            if !main.is_file() {
                return Err(SdkError::message(format!(
                    "workerd main_module missing: {}",
                    main.display()
                )));
            }
            let main_lower = w.main_module.to_ascii_lowercase();
            if main_lower.ends_with(".js") || main_lower.ends_with(".mjs") {
                let src = std::fs::read_to_string(&main).unwrap_or_default();
                let uses_package =
                    src.contains("@bookclerk/plugin-sdk") || src.contains("BookclerkPlugin");
                if !uses_package {
                    return Err(SdkError::message(format!(
                        "{}: import BookclerkPlugin from \
                         \"@bookclerk/plugin-sdk/workerd\"",
                        w.main_module
                    )));
                }
                if src.contains("WorkerEntrypoint") && !src.contains("BookclerkPlugin") {
                    return Err(SdkError::message(format!(
                        "{}: subclass BookclerkPlugin from \"@bookclerk/plugin-sdk/workerd\", \
                         not bare WorkerEntrypoint",
                        w.main_module
                    )));
                }
            }
        }
    }

    Ok(format!(
        "ok id={} kind={} runtime={:?}",
        manifest.id,
        manifest.kind.as_str(),
        manifest.runtime
    ))
}

/// Optionally vendors the workerd JS embed under the plugin modules tree.
///
/// Writes [`crate::workerd::EMBED_BOOKCLERK_PLUGIN_JS_SRC`] to
/// `modules/@bookclerk/plugin-sdk/workerd.js` (paths from the manifest). Hosts
/// normally inject this package at runtime — use sync only for offline /
/// hermetic trees.
///
/// # Arguments
///
/// * `plugin_dir` - Workerd plugin root (`runtime = "workerd"`).
///
/// # Returns
///
/// Success message including the destination path.
///
/// # Errors
///
/// Returns [`SdkError`] when the manifest is not workerd, `main_module` is not
/// `.js`/`.mjs`, or filesystem writes fail.
pub fn sync_embed(plugin_dir: &Path) -> Result<String> {
    let toml_path = plugin_dir.join("plugin.toml");
    let text = std::fs::read_to_string(&toml_path)
        .map_err(|e| SdkError::message(format!("read {}: {e}", toml_path.display())))?;
    let manifest = parse(&text).map_err(|e| SdkError::message(e.to_string()))?;
    if manifest.runtime != PluginRuntimeKind::Workerd {
        return Err(SdkError::message(
            "sync-embed requires runtime = \"workerd\"",
        ));
    }
    let w = manifest
        .workerd
        .as_ref()
        .ok_or_else(|| SdkError::message("workerd config missing"))?;
    let main_lower = w.main_module.to_ascii_lowercase();
    if !(main_lower.ends_with(".js") || main_lower.ends_with(".mjs")) {
        return Err(SdkError::message(format!(
            "sync-embed (Rust SDK): main_module must be .js/.mjs (got {})",
            w.main_module
        )));
    }
    let dest_dir = plugin_dir
        .join(&w.modules_dir)
        .join("@bookclerk")
        .join("plugin-sdk");
    std::fs::create_dir_all(&dest_dir).map_err(SdkError::from)?;
    let dest = dest_dir.join("workerd.js");
    std::fs::write(&dest, crate::workerd::EMBED_BOOKCLERK_PLUGIN_JS_SRC).map_err(SdkError::from)?;
    Ok(format!(
        "synced {} (optional vendor; prefer package import + bookclerk-workerd inject)",
        dest.display()
    ))
}

/// Resolves a plugin command against `root` unless the path is already absolute.
fn resolve_plugin_path(root: &Path, command: &Path) -> PathBuf {
    if command.is_absolute() {
        command.to_path_buf()
    } else {
        root.join(command)
    }
}
