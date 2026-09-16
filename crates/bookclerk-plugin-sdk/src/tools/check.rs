//! `bookclerk-plugin check` and optional workerd embed sync.
//!
//! Audience: authors validating on-disk plugin layout before packaging or
//! loading. Always available (no `tools` feature required for these library
//! entry points); the CLI wrapper lives behind feature `tools`.

use std::path::{Path, PathBuf};

use bookclerk_plugin_manifest::{parse, Entrypoint, PluginRuntimeKind};

use crate::error::{Result, SdkError};

/// Validates `plugin.toml` and the on-disk layout under `plugin_dir`.
///
/// Checks embedded logo paths, native `command` presence when
/// `.require-binary` is set, and workerd `modules_dir` / `main_module` plus the
/// api_version 3 author-model heuristics of [`check_main_module_source`].
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
/// are absent, or a workerd main module fails the author-model heuristics
/// (removed `BookclerkPlugin`, bare `WorkerEntrypoint`, missing entrypoint class).
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
            let language = if main_lower.ends_with(".js") || main_lower.ends_with(".mjs") {
                Some(MainModuleLanguage::Js)
            } else if main_lower.ends_with(".py") {
                Some(MainModuleLanguage::Python)
            } else {
                None
            };
            if let Some(language) = language {
                let src = std::fs::read_to_string(&main).unwrap_or_default();
                check_main_module_source(&w.main_module, &src, &manifest.entrypoints, language)?;
            }
        }
    }

    Ok(format!(
        "ok id={} family={} entrypoints=[{}] runtime={:?}",
        manifest.id,
        manifest.primary_family().as_str(),
        manifest
            .entrypoints
            .iter()
            .map(|e| e.wire_name())
            .collect::<Vec<_>>()
            .join(","),
        manifest.runtime
    ))
}

/// Source language of a workerd `main_module`, chosen by file extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MainModuleLanguage {
    /// `.js` / `.mjs` module authored against `@bookclerk/plugin-sdk/workerd`.
    Js,
    /// `.py` Python Worker authored against `bookclerk_plugin_sdk.workerd`.
    Python,
}

/// Exported class name the v3 author model expects for a named entrypoint
/// (`storefront` → `Storefront`, `databaseAdapter` → `DatabaseAdapter`).
///
/// # Arguments
///
/// * `entrypoint` - Named entrypoint from `plugin.toml`.
///
/// # Returns
///
/// Class name the adapter isolate resolves through `PLUGIN_<ENTRYPOINT>`.
#[must_use]
pub fn entrypoint_export_class(entrypoint: Entrypoint) -> String {
    let wire = entrypoint.wire_name();
    let mut chars = wire.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// True when `src` contains `needle` delimited by non-identifier characters.
fn contains_word(src: &str, needle: &str) -> bool {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';
    let mut from = 0;
    while let Some(pos) = src[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = start == 0 || !src[..start].chars().next_back().is_some_and(is_ident);
        let after_ok = !src[end..].chars().next().is_some_and(is_ident);
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

/// True when a JS module exports `class` (`export class Cls` or `export { …, Cls, … }`).
fn js_exports_class(src: &str, class: &str) -> bool {
    let mut from = 0;
    while let Some(pos) = src[from..].find("export") {
        let after = &src[from + pos + "export".len()..];
        let trimmed = after.trim_start();
        if let Some(rest) = trimmed.strip_prefix("class") {
            let rest = rest.trim_start();
            if rest.starts_with(class)
                && !rest[class.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
            {
                return true;
            }
        } else if let Some(rest) = trimmed.strip_prefix('{') {
            if let Some(close) = rest.find('}') {
                if contains_word(&rest[..close], class) {
                    return true;
                }
            }
        }
        from += pos + "export".len();
    }
    false
}

/// True when a Python module defines `class Cls(` at column zero.
fn py_defines_class(src: &str, class: &str) -> bool {
    src.lines().any(|line| {
        line.strip_prefix("class ")
            .and_then(|rest| rest.strip_prefix(class))
            .is_some_and(|rest| rest.trim_start().starts_with('('))
    })
}

/// Checks a workerd main module against the api_version 3 author model.
///
/// Mirrors `checkMainModuleSource` in the TypeScript SDK and
/// `check_main_module_source` in the Python SDK: the module must import the
/// SDK, must not reference the removed `BookclerkPlugin` base, must not
/// subclass bare `WorkerEntrypoint`, and must export (JS) or define (Python)
/// one class per named entrypoint in `entrypoints`.
///
/// # Arguments
///
/// * `main_name` - Module file name used in error messages.
/// * `src` - Module source text.
/// * `entrypoints` - Named entrypoints declared in `plugin.toml`.
/// * `language` - Module language chosen from the file extension.
///
/// # Errors
///
/// Returns [`SdkError`] describing the first violated rule.
pub fn check_main_module_source(
    main_name: &str,
    src: &str,
    entrypoints: &[Entrypoint],
    language: MainModuleLanguage,
) -> Result<()> {
    const BASE: &str = "BookclerkEntrypoint";
    if src.contains("BookclerkPlugin") {
        return Err(SdkError::message(format!(
            "{main_name}: `BookclerkPlugin` was removed in api_version 3; extend `{BASE}` \
             (default export with event()/job() triggers) and export named entrypoint classes \
             (Storefront, Storage, RemoteLibrary, DatabaseAdapter, Cli, Oidc)"
        )));
    }
    match language {
        MainModuleLanguage::Js => {
            let uses_package = src.contains("@bookclerk/plugin-sdk") || src.contains(BASE);
            if !uses_package {
                return Err(SdkError::message(format!(
                    "{main_name}: import {BASE} from \"@bookclerk/plugin-sdk/workerd\" \
                     (or \"@bookclerk/plugin-sdk\")"
                )));
            }
            if src.contains("WorkerEntrypoint")
                && !src.contains(BASE)
                && !src.contains("Entrypoint")
            {
                return Err(SdkError::message(format!(
                    "{main_name}: subclass {BASE} from \"@bookclerk/plugin-sdk/workerd\", \
                     not bare WorkerEntrypoint"
                )));
            }
        }
        MainModuleLanguage::Python => {
            let uses_package = src.contains("bookclerk_plugin_sdk") || src.contains(BASE);
            if !uses_package {
                return Err(SdkError::message(format!(
                    "{main_name}: import {BASE} from bookclerk_plugin_sdk.workerd"
                )));
            }
        }
    }
    for entrypoint in entrypoints {
        let class = entrypoint_export_class(*entrypoint);
        let present = match language {
            MainModuleLanguage::Js => js_exports_class(src, &class),
            MainModuleLanguage::Python => py_defines_class(src, &class),
        };
        if !present {
            let hint = match language {
                MainModuleLanguage::Js => format!("export class {class} extends {class}Entrypoint"),
                MainModuleLanguage::Python => format!("class {class}({class}Entrypoint)"),
            };
            return Err(SdkError::message(format!(
                "{main_name}: entrypoints = [\"{}\"] requires `{hint}`",
                entrypoint.wire_name()
            )));
        }
    }
    Ok(())
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

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    const JS_V3: &str = r#"
import { BookclerkEntrypoint, CliEntrypoint } from "@bookclerk/plugin-sdk/workerd";
export class Cli extends CliEntrypoint {}
export default class Echo extends BookclerkEntrypoint {}
"#;

    #[test]
    fn export_class_names_follow_wire_names() {
        assert_eq!(
            entrypoint_export_class(Entrypoint::Storefront),
            "Storefront"
        );
        assert_eq!(
            entrypoint_export_class(Entrypoint::DatabaseAdapter),
            "DatabaseAdapter"
        );
        assert_eq!(entrypoint_export_class(Entrypoint::Cli), "Cli");
    }

    #[test]
    fn js_v3_module_passes() {
        check_main_module_source(
            "index.js",
            JS_V3,
            &[Entrypoint::Cli],
            MainModuleLanguage::Js,
        )
        .expect("v3 module");
    }

    #[test]
    fn js_rejects_removed_base_and_bare_worker_entrypoint() {
        let err = check_main_module_source(
            "index.js",
            "import { BookclerkPlugin } from \"@bookclerk/plugin-sdk/workerd\";",
            &[],
            MainModuleLanguage::Js,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("removed in api_version 3"),
            "{err}"
        );
        let err = check_main_module_source(
            "index.js",
            "import { WorkerEntrypoint } from \"cloudflare:workers\";\nexport default class X extends WorkerEntrypoint {}",
            &[],
            MainModuleLanguage::Js,
        )
        .unwrap_err();
        assert!(err.to_string().contains("@bookclerk/plugin-sdk"), "{err}");
    }

    #[test]
    fn js_requires_exported_class_per_entrypoint() {
        let err = check_main_module_source(
            "index.js",
            JS_V3,
            &[Entrypoint::Cli, Entrypoint::Storefront],
            MainModuleLanguage::Js,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("export class Storefront extends StorefrontEntrypoint"),
            "{err}"
        );
        let braced = format!(
            "{JS_V3}\nclass Storefront extends StorefrontEntrypoint {{}}\nexport {{ Storefront }};"
        );
        check_main_module_source(
            "index.js",
            &braced,
            &[Entrypoint::Cli, Entrypoint::Storefront],
            MainModuleLanguage::Js,
        )
        .expect("braced export list");
        let prefixed = format!("{JS_V3}\nexport class StorefrontHelper {{}}");
        assert!(check_main_module_source(
            "index.js",
            &prefixed,
            &[Entrypoint::Storefront],
            MainModuleLanguage::Js,
        )
        .is_err());
    }

    #[test]
    fn python_requires_top_level_class_per_entrypoint() {
        let src = "from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint, CliEntrypoint\n\nclass Cli(CliEntrypoint):\n    pass\n\nclass Default(BookclerkEntrypoint):\n    pass\n";
        check_main_module_source(
            "plugin.py",
            src,
            &[Entrypoint::Cli],
            MainModuleLanguage::Python,
        )
        .expect("v3 python module");
        let err = check_main_module_source(
            "plugin.py",
            src,
            &[Entrypoint::Oidc],
            MainModuleLanguage::Python,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("class Oidc(OidcEntrypoint)"),
            "{err}"
        );
        let err = check_main_module_source(
            "plugin.py",
            "class Default:\n    pass\n",
            &[],
            MainModuleLanguage::Python,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("bookclerk_plugin_sdk.workerd"),
            "{err}"
        );
    }
}
