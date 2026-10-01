//! Author compatibility date, flag allowlist, and module-extension rules.
//!
//! These match the pinned local `workerd` binary. [`PluginManifest::validate`]
//! and the TypeScript / Python author checkers enforce the same rules.
//! `[[kv_namespaces]]` and `[queues]` are not rejected here: they stay legal
//! declarations and fail later at load or spawn with "not implemented yet".

use crate::error::{Error, Result};
use crate::types::ModuleSpec;

/// Newest `compatibility_date` this pin's `workerd` binary can honor.
///
/// Kept equal to `BUNDLED_WORKERD_COMPAT_DATE` / `workerd-pin.json`
/// `bundled_compat_date`. Older calendar dates stay valid. Newer dates are
/// rejected rather than warned.
pub const WORKERD_PIN_COMPAT_DATE: &str = "2026-08-01";

/// [`WORKERD_PIN_COMPAT_DATE`] as a calendar tuple for ordering.
const PIN_COMPAT_YMD: (i32, u32, u32) = (2026, 8, 1);

/// Author `compatibility_flags` this pin accepts.
///
/// Both are required together when the guest is Python, and forbidden
/// otherwise. Host-only flags such as egress `experimental` are not in this
/// list.
pub const PYTHON_COMPATIBILITY_FLAGS: &[&str] = &["python_workers", "disable_python_external_sdk"];

/// Rejects a `compatibility_date` that is not `YYYY-MM-DD` or is newer than
/// [`WORKERD_PIN_COMPAT_DATE`].
///
/// # Arguments
///
/// * `date` - Author `workerd.compatibility_date` string, untrimmed.
///
/// # Errors
///
/// Returns [`Error::Message`] when the string is not a real calendar date or
/// is newer than the pin. Equal and older dates succeed.
pub fn validate_author_compatibility_date(date: &str) -> Result<()> {
    let Some(parsed) = parse_calendar_date(date) else {
        return Err(Error::message(
            "plugin.toml: workerd.compatibility_date must be a calendar YYYY-MM-DD",
        ));
    };
    if parsed > PIN_COMPAT_YMD {
        return Err(Error::message(format!(
            "plugin.toml: workerd.compatibility_date `{date}` is newer than the pinned workerd compatibility date {WORKERD_PIN_COMPAT_DATE}"
        )));
    }
    Ok(())
}

/// Rejects author flags outside [`PYTHON_COMPATIBILITY_FLAGS`], a partial
/// Python pair, Python flags without a Python module, or flags missing when
/// `python` is true.
///
/// # Arguments
///
/// * `flags` - `workerd.compatibility_flags` in author order.
/// * `python` - True when the guest has a Python module (manifest or the
///   modules-directory walk). The flag names themselves are not evidence.
///
/// # Errors
///
/// Returns [`Error::Message`] for the first violated rule. Does not insert
/// missing flags.
pub fn validate_author_compatibility_flags(flags: &[String], python: bool) -> Result<()> {
    for flag in flags {
        if flag == "experimental" {
            return Err(Error::message(
                "plugin.toml: workerd.compatibility_flags `experimental` is host-only",
            ));
        }
        if !PYTHON_COMPATIBILITY_FLAGS.contains(&flag.as_str()) {
            return Err(Error::message(format!(
                "plugin.toml: workerd.compatibility_flags `{flag}` is not allowed"
            )));
        }
    }
    let has = |name: &str| flags.iter().any(|flag| flag == name);
    let both = PYTHON_COMPATIBILITY_FLAGS.iter().all(|flag| has(flag));
    let any = PYTHON_COMPATIBILITY_FLAGS.iter().any(|flag| has(flag));
    if python && !both {
        return Err(Error::message(
            "plugin.toml: workerd.compatibility_flags must include python_workers and disable_python_external_sdk when the guest is Python",
        ));
    }
    if any && !python {
        return Err(Error::message(
            "plugin.toml: workerd.compatibility_flags require a Python module",
        ));
    }
    Ok(())
}

/// Checks `main_module` and each `[[modules]]` row against the extensions the
/// directory walk embeds.
///
/// A type that disagrees with the extension is rejected. `.txt`, `.md`,
/// `.ts`, and any other extension the walk skips fail with "not implemented
/// yet" (expected authoring parity, not a permanent ban). This does not look
/// at the filesystem; callers that have a modules directory also require each
/// row to be in the walk's load set.
///
/// # Arguments
///
/// * `main_module` - `[workerd].main_module`.
/// * `modules` - `[[modules]]` rows.
///
/// # Errors
///
/// Returns [`Error::Message`] for the first row or main file that fails.
pub fn validate_module_declarations(main_module: &str, modules: &[ModuleSpec]) -> Result<()> {
    if workerd_embed_class(main_module).is_none() {
        return Err(Error::message(format!(
            "plugin.toml: workerd.main_module `{main_module}` is not implemented yet"
        )));
    }
    for module in modules {
        let path = if module.path.is_empty() {
            module.name.as_str()
        } else {
            module.path.as_str()
        };
        let Some(class) = workerd_embed_class(path) else {
            return Err(Error::message(format!(
                "plugin.toml: [[modules]] `{path}` is not implemented yet"
            )));
        };
        if !module_type_matches(class, &module.module_type) {
            return Err(Error::message(format!(
                "plugin.toml: [[modules]] `{path}` type `{}` does not match the file extension",
                module.module_type
            )));
        }
    }
    Ok(())
}

/// Relative key a modules-directory walk uses for a `[[modules]]` path or name.
///
/// Strips a leading `modules_dir/` prefix and `./` so `modules/index.js` and
/// `index.js` both match a walk of `modules_dir = "modules"`.
///
/// # Arguments
///
/// * `modules_dir` - `[workerd].modules_dir`.
/// * `raw` - Author `path` or `name`.
///
/// # Returns
///
/// Slash-separated relative key, or an empty string when `raw` is empty.
#[must_use]
pub fn module_load_key(modules_dir: &str, raw: &str) -> String {
    let mut key = raw.replace('\\', "/");
    let dir = modules_dir.trim_matches('/');
    if !dir.is_empty() {
        let prefix = format!("{dir}/");
        if let Some(rest) = key.strip_prefix(&prefix) {
            key = rest.to_string();
        }
    }
    while let Some(rest) = key.strip_prefix("./") {
        key = rest.to_string();
    }
    key
}

/// True when `path` ends in an extension the modules walk embeds.
///
/// # Arguments
///
/// * `path` - Module path or filename.
#[must_use]
pub fn workerd_module_is_embedded(path: &str) -> bool {
    workerd_embed_class(path).is_some()
}

/// Embed class for a path: `js`, `python`, `wasm`, or `json`.
///
/// `None` means the walk will not embed the file (text, TypeScript, or an
/// unknown extension). Those are "not implemented yet", not a permanent ban.
fn workerd_embed_class(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".py") {
        Some("python")
    } else if lower.ends_with(".wasm") {
        Some("wasm")
    } else if lower.ends_with(".mjs") || lower.ends_with(".js") {
        Some("js")
    } else if lower.ends_with(".json") {
        Some("json")
    } else {
        None
    }
}

/// True when a `[[modules]]` `type` agrees with [`workerd_embed_class`].
fn module_type_matches(class: &str, module_type: &str) -> bool {
    let kind = module_type.trim().to_ascii_lowercase();
    match class {
        "python" => kind == "python",
        "wasm" => kind == "wasm",
        "json" => kind == "json",
        "js" => matches!(kind.as_str(), "js" | "javascript" | "esm" | "esmodule"),
        _ => false,
    }
}

/// `(year, month, day)` when `text` is a real proleptic Gregorian `YYYY-MM-DD`.
fn parse_calendar_date(text: &str) -> Option<(i32, u32, u32)> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..10].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let year: i32 = text[..4].parse().ok()?;
    let month: u32 = text[5..7].parse().ok()?;
    let day: u32 = text[8..10].parse().ok()?;
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return None,
    };
    if day == 0 || day > max_day {
        return None;
    }
    Some((year, month, day))
}

/// Proleptic Gregorian leap year.
fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_date_parses() {
        assert_eq!(
            parse_calendar_date(WORKERD_PIN_COMPAT_DATE),
            Some(PIN_COMPAT_YMD)
        );
    }

    #[test]
    fn rejects_unpadded_and_impossible_dates() {
        assert!(validate_author_compatibility_date("2026-8-01").is_err());
        assert!(validate_author_compatibility_date("2026-02-31").is_err());
        assert!(validate_author_compatibility_date("latest").is_err());
        assert!(validate_author_compatibility_date("2025-02-29").is_err());
        assert!(validate_author_compatibility_date("2024-02-29").is_ok());
    }

    #[test]
    fn older_and_equal_dates_pass_newer_fails() {
        assert!(validate_author_compatibility_date("2024-09-23").is_ok());
        assert!(validate_author_compatibility_date(WORKERD_PIN_COMPAT_DATE).is_ok());
        let err = validate_author_compatibility_date("2026-08-02").unwrap_err();
        assert!(err.to_string().contains("newer than"), "{err}");
    }

    #[test]
    fn experimental_is_host_only_and_unknown_flags_fail() {
        let experimental =
            validate_author_compatibility_flags(&["experimental".to_string()], false).unwrap_err();
        assert!(
            experimental.to_string().contains("host-only"),
            "{experimental}"
        );
        let unknown =
            validate_author_compatibility_flags(&["nodejs_compat".to_string()], false).unwrap_err();
        assert!(unknown.to_string().contains("not allowed"), "{unknown}");
    }

    #[test]
    fn python_flags_are_a_required_pair() {
        let flags = vec![
            "python_workers".to_string(),
            "disable_python_external_sdk".to_string(),
        ];
        assert!(validate_author_compatibility_flags(&flags, true).is_ok());
        assert!(validate_author_compatibility_flags(&[], false).is_ok());
        let only_one = vec!["python_workers".to_string()];
        let missing = validate_author_compatibility_flags(&only_one, true).unwrap_err();
        assert!(missing.to_string().contains("must include"), "{missing}");
        let idle = validate_author_compatibility_flags(&flags, false).unwrap_err();
        assert!(idle.to_string().contains("Python module"), "{idle}");
    }

    #[test]
    fn module_load_key_strips_modules_dir_prefix() {
        assert_eq!(module_load_key("modules", "index.js"), "index.js");
        assert_eq!(
            module_load_key("modules", "modules/pkg/echo.wasm"),
            "pkg/echo.wasm"
        );
        assert_eq!(
            module_load_key("dist/modules", "./nested/a.js"),
            "nested/a.js"
        );
    }
}
