//! Author compatibility date, flag allowlist, and module-extension rules.
//!
//! These match the pinned local `workerd` binary. [`crate::PluginManifest::validate`]
//! and the TypeScript / Python author checkers enforce the same calendar, flag,
//! and module rules. A `compatibility_date` newer than the pin stays legal:
//! check and load warn, then run at the pin, because workerd refuses a date
//! past the one baked into the binary. `[[kv_namespaces]]` and `[queues]` are
//! not rejected here: they stay legal declarations and fail later at load or
//! spawn with "not implemented yet".

use crate::error::{Error, Result};
use crate::types::ModuleSpec;

/// Newest `compatibility_date` this pin's `workerd` binary can honor.
///
/// Kept equal to `BUNDLED_WORKERD_COMPAT_DATE` / `workerd-pin.json`
/// `bundled_compat_date`. Older calendar dates are passed through. A newer
/// date warns and falls back to this value when the isolate is configured.
pub const WORKERD_PIN_COMPAT_DATE: &str = "2026-08-01";

/// [`WORKERD_PIN_COMPAT_DATE`] as a calendar tuple for ordering.
const PIN_COMPAT_YMD: (i32, u32, u32) = (2026, 8, 1);

/// Author `compatibility_flags` this pin accepts.
///
/// Both are required together when the guest is Python, and forbidden
/// otherwise. Host-only flags such as egress `experimental` are not in this
/// list.
pub const PYTHON_COMPATIBILITY_FLAGS: &[&str] = &["python_workers", "disable_python_external_sdk"];

/// Date written into the workerd config, and a fallback warning when clamped.
///
/// Bookclerk host surfaces (events, jobs, and later bindings) must follow
/// [`applied`](Self::applied). That is the date workerd actually runs, so a
/// release gates its own behavior changes on the same calendar the isolate uses.
pub struct AppliedCompatibilityDate {
    /// Author date when it is on or before the pin, otherwise [`WORKERD_PIN_COMPAT_DATE`].
    pub applied: String,
    /// Wrangler-style warning when [`applied`](Self::applied) was clamped.
    ///
    /// `None` when the author date is equal to or older than the pin.
    pub warning: Option<String>,
}

/// Rejects a `compatibility_date` that is not a real `YYYY-MM-DD`.
///
/// A date newer than [`WORKERD_PIN_COMPAT_DATE`] is still valid. Load falls
/// back to the pin and warns; see [`apply_author_compatibility_date`].
///
/// # Arguments
///
/// * `date` - Author `workerd.compatibility_date` string, untrimmed.
///
/// # Errors
///
/// Returns [`Error::Message`] when the string is not a real calendar date.
pub fn validate_author_compatibility_date(date: &str) -> Result<()> {
    if parse_calendar_date(date).is_none() {
        return Err(Error::message(
            "plugin.toml: workerd.compatibility_date must be a calendar YYYY-MM-DD",
        ));
    }
    Ok(())
}

/// Resolves an author `compatibility_date` against this Bookclerk release.
///
/// workerd enables each compatibility flag whose default-on date is on or
/// before the date it is given, and it refuses to start when that date is
/// newer than `supported-compatibility-date.txt` in the binary. Wrangler
/// works around that refusal by warning and starting at the newest date the
/// installed runtime supports. This function does the same with
/// [`WORKERD_PIN_COMPAT_DATE`].
///
/// # Arguments
///
/// * `date` - Author `workerd.compatibility_date` string, untrimmed.
///
/// # Errors
///
/// Returns [`Error::Message`] when the string is not a real calendar date.
pub fn apply_author_compatibility_date(date: &str) -> Result<AppliedCompatibilityDate> {
    validate_author_compatibility_date(date)?;
    let parsed = parse_calendar_date(date).ok_or_else(|| {
        Error::message("plugin.toml: workerd.compatibility_date must be a calendar YYYY-MM-DD")
    })?;
    if parsed > PIN_COMPAT_YMD {
        return Ok(AppliedCompatibilityDate {
            applied: WORKERD_PIN_COMPAT_DATE.to_string(),
            warning: Some(compatibility_date_fallback_warning(date)),
        });
    }
    Ok(AppliedCompatibilityDate {
        applied: date.to_string(),
        warning: None,
    })
}

/// Wrangler-shaped warning for a date this pin cannot run exactly.
fn compatibility_date_fallback_warning(requested: &str) -> String {
    format!(
        "The latest compatibility date supported by the installed Bookclerk workerd runtime is \"{WORKERD_PIN_COMPAT_DATE}\",\n\
but you've requested \"{requested}\". Falling back to \"{WORKERD_PIN_COMPAT_DATE}\"...\n\
Features enabled by your requested compatibility date may not be available.\n\
Upgrade Bookclerk to a release that supports this date."
    )
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
#[allow(clippy::missing_panics_doc)]
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
    fn older_and_equal_dates_pass_through_newer_falls_back() {
        assert!(validate_author_compatibility_date("2024-09-23").is_ok());
        assert!(validate_author_compatibility_date(WORKERD_PIN_COMPAT_DATE).is_ok());
        assert!(validate_author_compatibility_date("2026-08-02").is_ok());
        let older = apply_author_compatibility_date("2024-09-23").expect("older date");
        assert_eq!(older.applied, "2024-09-23");
        assert!(older.warning.is_none());
        let equal = apply_author_compatibility_date(WORKERD_PIN_COMPAT_DATE).expect("pin date");
        assert_eq!(equal.applied, WORKERD_PIN_COMPAT_DATE);
        assert!(equal.warning.is_none());
        let newer = apply_author_compatibility_date("2026-08-02").expect("newer date");
        assert_eq!(newer.applied, WORKERD_PIN_COMPAT_DATE);
        let warning = newer.warning.expect("fallback warning");
        assert!(warning.contains("Falling back"), "{warning}");
        assert!(warning.contains("2026-08-02"), "{warning}");
        assert!(warning.contains(WORKERD_PIN_COMPAT_DATE), "{warning}");
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
