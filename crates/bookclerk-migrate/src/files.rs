//! Import `FileLocationsV2.json` / legacy `FileLocations.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{MigrateError, Result};

/// ASIN → absolute audio path from classic Libation file cache.
pub type AudioPathMap = HashMap<String, PathBuf>;

/// Load audio paths keyed by ASIN.
///
/// # Arguments
///
/// * `path` - Filesystem path involved in this operation.
///
/// # Returns
///
/// On success, the inner `AudioPathMap` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn load_audio_paths(path: &Path) -> Result<AudioPathMap> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        MigrateError::Library(format!("failed to read {}: {err}", path.display()))
    })?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|err| MigrateError::Library(format!("invalid file locations JSON: {err}")))?;

    let mut out = AudioPathMap::new();

    // V2: { "Dictionary": { "ASIN": [ { "Id", "FileType", "Path": { "Path": "..." } } ] } }
    if let Some(dict) = value.get("Dictionary").and_then(Value::as_object) {
        for (asin, entries) in dict {
            if let Some(path) = first_audio_path(entries) {
                out.insert(asin.clone(), path);
            }
        }
        return Ok(out);
    }

    // Legacy array form
    if let Some(arr) = value.as_array() {
        for entry in arr {
            let asin = entry
                .get("Id")
                .or_else(|| entry.get("id"))
                .and_then(Value::as_str);
            let Some(asin) = asin else { continue };
            if let Some(path) = audio_path_from_entry(entry) {
                out.insert(asin.to_string(), path);
            }
        }
    }

    Ok(out)
}

/// Picks the first V2 entry with `FileType` audio (1), else the first path-like entry.
fn first_audio_path(entries: &Value) -> Option<PathBuf> {
    let arr = entries.as_array()?;
    // Prefer FileType Audio (1); fall back to first path-like entry.
    for entry in arr {
        let file_type = entry.get("FileType").and_then(Value::as_i64).or_else(|| {
            entry
                .get("FileType")
                .and_then(Value::as_u64)
                .map(|n| n as i64)
        });
        if file_type == Some(1) {
            if let Some(path) = audio_path_from_entry(entry) {
                return Some(path);
            }
        }
    }
    arr.iter().find_map(audio_path_from_entry)
}

/// Reads `Path.Path` / `Path` / `path` from one file-location entry.
fn audio_path_from_entry(entry: &Value) -> Option<PathBuf> {
    let path = entry
        .get("Path")
        .and_then(|p| p.get("Path").and_then(Value::as_str).or_else(|| p.as_str()))
        .or_else(|| entry.get("path").and_then(Value::as_str))?;
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// Relativize `audio` under `books_root` when possible; otherwise return the
/// absolute path as a storage key string.
///
/// # Arguments
///
/// * `audio` - Filesystem path (`audio`).
/// * `books_root` - Filesystem path (`books_root`).
///
/// # Returns
///
/// String result for this operation.
pub fn storage_key_for(audio: &Path, books_root: &Path) -> String {
    if let Ok(rel) = audio.strip_prefix(books_root) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    audio.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v2_dictionary() {
        let value = serde_json::json!({
            "Dictionary": {
                "B00TEST": [
                    {
                        "Id": "B00TEST",
                        "FileType": 1,
                        "Path": { "Path": "/data/Author/Title/book.m4b" }
                    }
                ]
            }
        });
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), value.to_string()).unwrap();
        let map = load_audio_paths(tmp.path()).unwrap();
        assert_eq!(
            map.get("B00TEST").unwrap(),
            &PathBuf::from("/data/Author/Title/book.m4b")
        );
        assert_eq!(
            storage_key_for(Path::new("/data/Author/Title/book.m4b"), Path::new("/data")),
            "Author/Title/book.m4b"
        );
    }
}
