//! crates.io catalog client for Bookclerk plugin discovery.

use serde::Deserialize;

use crate::registry::{PluginCatalogEntry, PluginCrateName, CRATE_NAME_PREFIX, REGISTRY_KEYWORD};
use crate::{PluginError, Result};

/// crates.io User-Agent (`bookclerk/<version>` plus catalog contact URL).
const CRATES_IO_USER_AGENT: &str = concat!(
    "bookclerk/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/fritz-fritz/bookclerk; plugin-catalog)"
);

/// Search crates.io for Bookclerk plugins.
///
/// Uses keyword [`REGISTRY_KEYWORD`] plus optional free text, then keeps hits
/// whose crate name starts with [`CRATE_NAME_PREFIX`] and parses cleanly.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn search_crates_io(query: Option<&str>, per_page: u32) -> Result<Vec<PluginCatalogEntry>> {
    let per_page = per_page.clamp(1, 100);
    let q = match query.map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) => format!("{REGISTRY_KEYWORD} {extra}"),
        None => REGISTRY_KEYWORD.to_string(),
    };
    let url = format!(
        "https://crates.io/api/v1/crates?q={}&per_page={per_page}&sort=downloads",
        urlencoding_encode(&q)
    );

    let body: CratesSearchResponse = http_get_json(&url)?;
    let mut out = Vec::new();
    for c in body.crates {
        if !c.name.starts_with(CRATE_NAME_PREFIX) {
            continue;
        }
        let Ok(parsed) = PluginCrateName::parse(&c.name) else {
            continue;
        };
        out.push(PluginCatalogEntry {
            crate_name: c.name,
            version: c.max_version.unwrap_or_default(),
            description: c.description,
            downloads: c.downloads.unwrap_or(0),
            documentation: c.documentation,
            repository: c.repository,
            homepage: c.homepage,
            parsed: Some(parsed),
            metadata: None,
        });
    }
    Ok(out)
}

/// GET `url` as JSON; non-2xx or decode failures become [`PluginError`].
fn http_get_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T> {
    let mut response = ureq::get(url)
        .header("User-Agent", CRATES_IO_USER_AGENT)
        .header("Accept", "application/json")
        .call()
        .map_err(|e| PluginError::message(format!("crates.io request failed ({url}): {e}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(PluginError::message(format!(
            "crates.io returned HTTP {status} for {url}"
        )));
    }
    response
        .body_mut()
        .read_json::<T>()
        .map_err(|e| PluginError::message(format!("crates.io JSON decode failed: {e}")))
}

/// Minimal percent-encoding for query values (space → `+`, reserved → `%XX`).
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Debug, Deserialize)]
/// crates.io search envelope (`crates` array).
struct CratesSearchResponse {
    /// Hits from `/api/v1/crates` before prefix / parse filtering.
    crates: Vec<CrateHit>,
}

#[derive(Debug, Deserialize)]
/// One crates.io search hit; only `bookclerk-plugin-*` names are kept.
struct CrateHit {
    /// Crate name on crates.io.
    name: String,
    #[serde(default)]
    /// crates.io description when published.
    description: Option<String>,
    #[serde(default)]
    /// Lifetime download count; treated as `0` when omitted.
    downloads: Option<u64>,
    #[serde(default)]
    /// docs.rs (or other) documentation URL when published.
    documentation: Option<String>,
    #[serde(default)]
    /// Source repository URL when published.
    repository: Option<String>,
    #[serde(default)]
    /// Project homepage URL when published.
    homepage: Option<String>,
    #[serde(default)]
    /// Highest published version; empty string when crates.io omitted it.
    max_version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_query() {
        assert_eq!(urlencoding_encode("bookclerk-plugin"), "bookclerk-plugin");
        assert_eq!(urlencoding_encode("a b"), "a+b");
    }

    #[test]
    fn search_crates_io_smoke() {
        // Network may be unavailable in some CI sandboxes; skip soft-fail.
        match search_crates_io(None, 5) {
            Ok(hits) => {
                for h in &hits {
                    assert!(h.crate_name.starts_with(CRATE_NAME_PREFIX));
                    assert!(h.parsed.is_some());
                }
            }
            Err(err) => {
                let msg = err.to_string();
                assert!(
                    msg.contains("crates.io") || msg.contains("request"),
                    "unexpected error: {msg}"
                );
            }
        }
    }
}
