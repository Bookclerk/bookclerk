//! Host placement for node-local storage scans.
//!
//! The id is the same `event_node_id` file the event runtime keeps under the
//! files directory. A scan of local bytes is adopted only when this id matches
//! the checkpoint. Object stores leave placement unset and stay portable.

use std::path::Path;
use std::sync::OnceLock;

/// Placement used when no files directory is configured.
static PROCESS_PLACEMENT: OnceLock<String> = OnceLock::new();

/// Stable id of this host's files directory, or a process-lifetime id when
/// that directory is not configured.
///
/// `BOOKCLERK_PLACEMENT_ID` overrides the file for tests. Otherwise the value
/// is read or created at `$BOOKCLERK_FILES_DIR/event_node_id`, the same path
/// the event runtime uses for its catalog node id. When neither is available
/// the id lasts for this process only, so a scan is not adopted by another
/// process that cannot prove it is the same node.
#[must_use]
pub fn host_placement_id() -> String {
    if let Ok(explicit) = std::env::var("BOOKCLERK_PLACEMENT_ID") {
        let explicit = explicit.trim();
        if !explicit.is_empty() {
            return explicit.to_string();
        }
    }
    if let Ok(dir) = std::env::var("BOOKCLERK_FILES_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return read_or_create_node_id(Path::new(dir));
        }
    }
    PROCESS_PLACEMENT
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .clone()
}

/// Reads `$files_dir/event_node_id`, creating it when missing.
fn read_or_create_node_id(files_dir: &Path) -> String {
    let path = files_dir.join("event_node_id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    if std::fs::create_dir_all(files_dir).is_ok() {
        let _ = std::fs::write(&path, &id);
    }
    id
}
