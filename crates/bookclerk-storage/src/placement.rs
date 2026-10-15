//! Node placement for node-local storage scans.
//!
//! The id is the node id in `node-identity.json` under the files directory,
//! the same id the node registers with the cluster. A scan of local bytes is
//! adopted only when this id matches the checkpoint. Object stores leave
//! placement unset and stay portable.

use std::path::Path;
use std::sync::OnceLock;

/// Placement used when no node identity is available.
static PROCESS_PLACEMENT: OnceLock<String> = OnceLock::new();

/// This node's id, or a process-lifetime id when the node identity cannot be
/// read.
///
/// `BOOKCLERK_PLACEMENT_ID` overrides it for tests. Otherwise the value is
/// the `node_id` in `$BOOKCLERK_FILES_DIR/node-identity.json`, which the
/// control plane writes when the node first opens the library. When that is
/// not available the id lasts for this process only, so a scan is not
/// adopted by another process that cannot prove it is the same node.
#[must_use]
pub fn node_placement_id() -> String {
    if let Ok(explicit) = std::env::var("BOOKCLERK_PLACEMENT_ID") {
        let explicit = explicit.trim();
        if !explicit.is_empty() {
            return explicit.to_string();
        }
    }
    if let Ok(dir) = std::env::var("BOOKCLERK_FILES_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            if let Some(id) = read_node_id(Path::new(dir)) {
                return id;
            }
        }
    }
    PROCESS_PLACEMENT
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .clone()
}

/// Reads `node_id` from `$files_dir/node-identity.json`.
fn read_node_id(files_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(files_dir.join("node-identity.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let id = value.get("node_id")?.as_str()?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_comes_from_the_node_identity_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_node_id(dir.path()), None);
        std::fs::write(
            dir.path().join("node-identity.json"),
            r#"{"v":1,"node_id":"node-1","cluster_id":"c"}"#,
        )
        .unwrap();
        assert_eq!(read_node_id(dir.path()).as_deref(), Some("node-1"));
    }
}
