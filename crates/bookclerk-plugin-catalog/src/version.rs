//! Lightweight semver parsing for registry version selection.

use std::cmp::Ordering;

/// Parsed `major.minor.patch` with optional pre-release / build metadata.
///
/// Ordering follows SemVer 2.0 for the subset we care about: a release is
/// greater than any pre-release with the same major/minor/patch
/// (`1.0.0` > `1.0.0-beta`). Pre-release identifiers are compared
/// lexicographically after numeric segments where both sides are integers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// Semver major component.
    pub major: u64,
    /// Semver minor component.
    pub minor: u64,
    /// Semver patch component.
    pub patch: u64,
    /// `None` means a release (no `-pre` suffix). Empty vs missing is not used.
    pub pre: Option<String>,
}

impl Version {
    /// Parse a version string (`1.2.3`, `1.2.3-beta`, `v1.2.3`).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches('v');
        let (core, pre) = match s.split_once('-') {
            Some((core, rest)) => {
                let pre = rest.split('+').next().unwrap_or(rest);
                if pre.is_empty() {
                    return None;
                }
                (core, Some(pre.to_string()))
            }
            None => {
                let core = s.split('+').next().unwrap_or(s);
                (core, None)
            }
        };
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// Returns true when this version has a prerelease suffix.
    #[must_use]
    pub fn is_prerelease(&self) -> bool {
        self.pre.is_some()
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.major
            .cmp(&other.major)
            .then(self.minor.cmp(&other.minor))
            .then(self.patch.cmp(&other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => cmp_prerelease(a, b),
            })
    }
}

/// Compares SemVer pre-release identifiers (numeric vs lexical, per spec).
fn cmp_prerelease(a: &str, b: &str) -> Ordering {
    let mut a_parts = a.split('.');
    let mut b_parts = b.split('.');
    loop {
        match (a_parts.next(), b_parts.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ap), Some(bp)) => {
                let ord = match (ap.parse::<u64>(), bp.parse::<u64>()) {
                    (Ok(an), Ok(bn)) => an.cmp(&bn),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => ap.cmp(bp),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// Prefer the greatest release version; if only pre-releases exist, the
/// greatest pre-release. Falls back to lexical max when nothing parses.
#[must_use]
pub fn max_version<'a>(versions: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let mut best_release: Option<(Version, &'a str)> = None;
    let mut best_pre: Option<(Version, &'a str)> = None;
    let mut lexical: Option<&'a str> = None;
    for v in versions {
        lexical = match lexical {
            Some(cur) if cur >= v => Some(cur),
            _ => Some(v),
        };
        let Some(parsed) = Version::parse(v) else {
            continue;
        };
        if parsed.is_prerelease() {
            best_pre = match best_pre {
                Some((cur, s)) if cur >= parsed => Some((cur, s)),
                _ => Some((parsed, v)),
            };
        } else {
            best_release = match best_release {
                Some((cur, s)) if cur >= parsed => Some((cur, s)),
                _ => Some((parsed, v)),
            };
        }
    }
    best_release.or(best_pre).map(|(_, s)| s).or(lexical)
}

/// Newest version strictly greater than `current`, if any.
///
/// Prefer a newer release over a newer pre-release when both qualify.
#[must_use]
pub fn newest_newer_than<'a>(
    current: &str,
    versions: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let current = Version::parse(current)?;
    let mut best_release: Option<(Version, &'a str)> = None;
    let mut best_pre: Option<(Version, &'a str)> = None;
    for v in versions {
        let Some(parsed) = Version::parse(v) else {
            continue;
        };
        if parsed <= current {
            continue;
        }
        if parsed.is_prerelease() {
            best_pre = match best_pre {
                Some((cur, s)) if cur >= parsed => Some((cur, s)),
                _ => Some((parsed, v)),
            };
        } else {
            best_release = match best_release {
                Some((cur, s)) if cur >= parsed => Some((cur, s)),
                _ => Some((parsed, v)),
            };
        }
    }
    best_release.or(best_pre).map(|(_, s)| s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_orders_not_lexically() {
        assert!(Version::parse("10.0.0").unwrap() > Version::parse("2.0.0").unwrap());
        assert_eq!(max_version(["2.0.0", "10.0.0", "3.1.0"]), Some("10.0.0"));
    }

    #[test]
    fn release_outranks_matching_prerelease() {
        assert!(Version::parse("1.0.0").unwrap() > Version::parse("1.0.0-beta").unwrap());
        assert_eq!(
            max_version(["1.0.0-beta", "1.0.0", "1.0.0-alpha"]),
            Some("1.0.0")
        );
        assert_eq!(
            max_version(["1.0.0-beta", "1.0.0-rc.1"]),
            Some("1.0.0-rc.1")
        );
    }

    #[test]
    fn newest_newer_than_skips_equal() {
        assert_eq!(
            newest_newer_than("1.0.0", ["1.0.0", "1.0.1", "0.9.0"]),
            Some("1.0.1")
        );
    }

    #[test]
    fn newest_newer_than_prefers_release() {
        assert_eq!(
            newest_newer_than("1.0.0", ["1.1.0-beta", "1.1.0"]),
            Some("1.1.0")
        );
    }
}
