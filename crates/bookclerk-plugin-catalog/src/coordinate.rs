//! Source-qualified package coordinates.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{CatalogError, Result};

/// Where a package was discovered / will be fetched from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RegistrySource {
    /// crates.io (or alternate Cargo registry).
    Cargo {
        #[serde(default = "default_crates_io")]
        registry_url: String,
    },
    /// npm registry.
    Npm {
        #[serde(default = "default_npm")]
        registry_url: String,
    },
    /// PyPI (exact lookup only for search-less discovery).
    Pypi {
        #[serde(default = "default_pypi")]
        simple_url: String,
    },
    /// Static HTTPS JSON/TOML index.
    Static { index_url: String },
    /// Local archive path (offline / fixture install).
    LocalArchive,
}

fn default_crates_io() -> String {
    "https://crates.io".into()
}
fn default_npm() -> String {
    "https://registry.npmjs.org".into()
}
fn default_pypi() -> String {
    "https://pypi.org".into()
}

impl RegistrySource {
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Cargo { .. } => "cargo",
            Self::Npm { .. } => "npm",
            Self::Pypi { .. } => "pypi",
            Self::Static { .. } => "registry",
            Self::LocalArchive => "local",
        }
    }
}

/// Exact package coordinate: source + name + version.
///
/// Display forms:
/// - `cargo:bookclerk-plugin-source-example@1.2.3`
/// - `npm:@publisher/bookclerk-plugin-source-example@1.2.3`
/// - `pypi:bookclerk-plugin-source-example==1.2.3`
/// - `registry:https://example.com/index.json#community/example@1.2.3`
/// - `local:/path/to/archive.tar.gz` (version may be `0.0.0` placeholder)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageCoordinate {
    pub source: RegistrySource,
    pub name: String,
    pub version: String,
}

impl PackageCoordinate {
    /// Parse a source-qualified coordinate string.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if let Some(path) = raw.strip_prefix("local:") {
            return Ok(Self {
                source: RegistrySource::LocalArchive,
                name: path.to_string(),
                version: "0.0.0".into(),
            });
        }
        if let Some(rest) = raw.strip_prefix("cargo:") {
            let (name, version) = split_at_version(rest, '@')?;
            return Ok(Self {
                source: RegistrySource::Cargo {
                    registry_url: default_crates_io(),
                },
                name,
                version,
            });
        }
        if let Some(rest) = raw.strip_prefix("npm:") {
            let (name, version) = split_at_version(rest, '@')?;
            return Ok(Self {
                source: RegistrySource::Npm {
                    registry_url: default_npm(),
                },
                name,
                version,
            });
        }
        if let Some(rest) = raw.strip_prefix("pypi:") {
            let (name, version) = if let Some((n, v)) = rest.split_once("==") {
                (n.to_string(), v.to_string())
            } else {
                split_at_version(rest, '@')?
            };
            return Ok(Self {
                source: RegistrySource::Pypi {
                    simple_url: default_pypi(),
                },
                name,
                version,
            });
        }
        if let Some(rest) = raw.strip_prefix("registry:") {
            // registry:<index_url>#<name>@<version>
            let (index_url, name_ver) = rest.split_once('#').ok_or_else(|| {
                CatalogError::message(
                    "registry coordinate must be registry:<index_url>#<name>@<version>",
                )
            })?;
            let (name, version) = split_at_version(name_ver, '@')?;
            return Ok(Self {
                source: RegistrySource::Static {
                    index_url: index_url.to_string(),
                },
                name,
                version,
            });
        }
        Err(CatalogError::message(format!(
            "coordinate `{raw}` must start with cargo:|npm:|pypi:|registry:|local:"
        )))
    }
}

fn split_at_version(s: &str, sep: char) -> Result<(String, String)> {
    // npm scoped packages: @scope/name@version — split on last @
    let idx = s.rfind(sep).ok_or_else(|| {
        CatalogError::message(format!("missing version separator `{sep}` in `{s}`"))
    })?;
    let name = s[..idx].to_string();
    let version = s[idx + 1..].to_string();
    if name.is_empty() || version.is_empty() {
        return Err(CatalogError::message(format!(
            "empty name or version in `{s}`"
        )));
    }
    Ok((name, version))
}

impl fmt::Display for PackageCoordinate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            RegistrySource::Cargo { .. } => {
                write!(f, "cargo:{}@{}", self.name, self.version)
            }
            RegistrySource::Npm { .. } => write!(f, "npm:{}@{}", self.name, self.version),
            RegistrySource::Pypi { .. } => {
                write!(f, "pypi:{}=={}", self.name, self.version)
            }
            RegistrySource::Static { index_url } => {
                write!(f, "registry:{index_url}#{}@{}", self.name, self.version)
            }
            RegistrySource::LocalArchive => write!(f, "local:{}", self.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cargo_npm_pypi_registry_local() {
        let c = PackageCoordinate::parse("cargo:bookclerk-plugin-source-example@1.2.3").unwrap();
        assert_eq!(c.name, "bookclerk-plugin-source-example");
        assert_eq!(c.version, "1.2.3");
        assert_eq!(c.source.kind_name(), "cargo");

        let c = PackageCoordinate::parse("npm:@pub/bookclerk-plugin-source-example@1.0.0").unwrap();
        assert_eq!(c.name, "@pub/bookclerk-plugin-source-example");

        let c = PackageCoordinate::parse("pypi:bookclerk-plugin-source-example==0.1.0").unwrap();
        assert_eq!(c.version, "0.1.0");

        let c = PackageCoordinate::parse(
            "registry:https://example.com/index.json#community/example@2.0.0",
        )
        .unwrap();
        assert_eq!(c.name, "community/example");
        assert_eq!(
            c.to_string(),
            "registry:https://example.com/index.json#community/example@2.0.0"
        );

        let c = PackageCoordinate::parse("local:/tmp/plugin.tar.gz").unwrap();
        assert_eq!(c.source, RegistrySource::LocalArchive);
    }

    #[test]
    fn rejects_unqualified() {
        assert!(PackageCoordinate::parse("bookclerk-plugin-source-x@1").is_err());
    }
}
