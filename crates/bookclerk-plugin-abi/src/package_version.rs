//! `plugin.toml` `version` as a semantic version.
//!
//! Every plugin declares a semver `version`. The host pins the `[[settings]]`
//! schema per plugin key together with the version that produced it, and
//! orders versions to decide whether a host may re-pin (newer), must refuse
//! to spawn (older), or is unchanged.

use std::cmp::Ordering;

/// Parses `raw` as a semantic version.
///
/// # Errors
///
/// Returns an operator-facing message when `raw` is empty or is not semver.
pub fn parse_package_version(raw: &str) -> Result<semver::Version, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(
            "`version` is required and must be a semantic version (for example 0.1.0)".into(),
        );
    }
    semver::Version::parse(trimmed)
        .map_err(|err| format!("`version` `{trimmed}` is not a semantic version: {err}"))
}

/// Orders a host's local package version against the pinned one.
///
/// A pinned version that is empty or does not parse counts as `0.0.0`, so a
/// host with a real version repairs the pin instead of refusing forever.
///
/// # Errors
///
/// Returns an error when `local` is not a semantic version.
pub fn package_version_order(local: &str, pinned: &str) -> Result<Ordering, String> {
    let local = parse_package_version(local)?;
    let pinned =
        semver::Version::parse(pinned.trim()).unwrap_or_else(|_| semver::Version::new(0, 0, 0));
    Ok(local.cmp(&pinned))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::missing_panics_doc)]

    use super::*;

    #[test]
    fn parses_semver_and_rejects_the_rest() {
        assert_eq!(parse_package_version("1.2.3").unwrap().to_string(), "1.2.3");
        assert_eq!(
            parse_package_version(" 0.1.0-rc.1 ").unwrap().to_string(),
            "0.1.0-rc.1"
        );
        assert!(parse_package_version("").unwrap_err().contains("required"));
        assert!(parse_package_version("v1")
            .unwrap_err()
            .contains("not a semantic version"));
        assert!(parse_package_version("1.0").is_err());
    }

    #[test]
    fn orders_against_the_pin_and_treats_a_bad_pin_as_oldest() {
        assert_eq!(
            package_version_order("0.2.0", "0.1.0").unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            package_version_order("0.1.0", "0.1.0").unwrap(),
            Ordering::Equal
        );
        assert_eq!(
            package_version_order("0.1.0", "0.2.0").unwrap(),
            Ordering::Less
        );
        assert_eq!(
            package_version_order("0.0.1", "").unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            package_version_order("0.0.1", "garbage").unwrap(),
            Ordering::Greater
        );
        assert!(package_version_order("", "0.1.0").is_err());
    }
}
