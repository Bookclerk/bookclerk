//! Plugin id grammar shared by manifest parse, host registry, and SDKs.

use crate::error::{Error, Result};

/// Strict plugin id: `[a-z0-9_]{2,32}` with no leading/trailing `_` and no `__`.
///
/// Ids are globally unique across kinds. This grammar is non-lossy — characters
/// that would need rewriting (e.g. `/` → `_`) are rejected instead of sanitized,
/// so `a/b` and `a_b` cannot collide.
pub fn validate_plugin_id(id: &str) -> Result<()> {
    // Non-lossy: reject padding rather than silently trimming (e.g. `" echo"` ≠ `"echo"`).
    if id != id.trim() {
        return Err(Error::message(format!(
            "plugin id `{id}` must not have leading or trailing whitespace"
        )));
    }
    if id.len() < 2 || id.len() > 32 {
        return Err(Error::message(format!(
            "plugin id `{id}` must be 2–32 characters"
        )));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(Error::message(format!(
            "plugin id `{id}` must be lowercase ascii letters, digits, or `_`"
        )));
    }
    if id.starts_with('_') || id.ends_with('_') || id.contains("__") {
        return Err(Error::message(format!(
            "plugin id `{id}` must not start/end with `_` or contain `__`"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_ids() {
        assert!(validate_plugin_id("ab").is_ok());
        assert!(validate_plugin_id("sqlite").is_ok());
        assert!(validate_plugin_id("my_store").is_ok());
        assert!(validate_plugin_id("echo_native_rust").is_ok());
        assert!(validate_plugin_id("a1").is_ok());
    }

    #[test]
    fn rejects_invalid_ids() {
        assert!(validate_plugin_id("a").is_err());
        assert!(validate_plugin_id("_ab").is_err());
        assert!(validate_plugin_id("ab_").is_err());
        assert!(validate_plugin_id("a__b").is_err());
        assert!(validate_plugin_id("a/b").is_err());
        assert!(validate_plugin_id("a-b").is_err());
        assert!(validate_plugin_id("../evil").is_err());
        assert!(validate_plugin_id("HasUpper").is_err());
        assert!(validate_plugin_id(&"x".repeat(33)).is_err());
    }

    #[test]
    fn rejects_leading_or_trailing_whitespace() {
        let leading = validate_plugin_id(" echo").expect_err("leading space");
        assert!(leading.to_string().contains("whitespace"), "{leading}");
        let trailing = validate_plugin_id("echo ").expect_err("trailing space");
        assert!(trailing.to_string().contains("whitespace"), "{trailing}");
    }
}
