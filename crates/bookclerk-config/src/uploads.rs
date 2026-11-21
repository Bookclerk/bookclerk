//! Upload limits (`[uploads]` section): what one node accepts from the
//! upload API before a batch is identified and stored (see
//! `docs/server/uploads.md`).

use serde::{Deserialize, Serialize};

/// One gibibyte.
const GIB: u64 = 1024 * 1024 * 1024;

/// Default largest single file: 20 GiB.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 20 * GIB;
/// Default most bytes one person may have staged at once: 20 GiB.
pub const DEFAULT_MAX_STAGED_BYTES: u64 = 20 * GIB;
/// Default free-disk floor: staging stops while less than this share of
/// the node's disk would stay free.
pub const DEFAULT_FREE_DISK_FLOOR_PERCENT: u8 = 10;
/// Default idle time after which an unfinished batch is pruned: 24 hours.
pub const DEFAULT_IDLE_TTL_HOURS: u64 = 24;

/// `[uploads]`: per-node upload limits.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct UploadsConfig {
    /// Largest single file, in bytes.
    pub max_file_bytes: u64,
    /// Most bytes one person may have staged in unfinished batches.
    pub max_staged_bytes: u64,
    /// Percent of the node's disk (0 to 90) that must stay free; a batch
    /// or chunk that would leave less is refused.
    pub free_disk_floor_percent: u8,
    /// Hours an open batch may sit without a chunk before it is pruned.
    pub idle_ttl_hours: u64,
}

impl Default for UploadsConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_staged_bytes: DEFAULT_MAX_STAGED_BYTES,
            free_disk_floor_percent: DEFAULT_FREE_DISK_FLOOR_PERCENT,
            idle_ttl_hours: DEFAULT_IDLE_TTL_HOURS,
        }
    }
}

impl UploadsConfig {
    /// The free-disk floor as a share, capped at 90 percent so a typo
    /// cannot close staging for good.
    #[must_use]
    pub fn free_disk_floor(&self) -> u8 {
        self.free_disk_floor_percent.min(90)
    }

    /// The idle time, at least one hour.
    #[must_use]
    pub fn idle_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.idle_ttl_hours.max(1).saturating_mul(3600))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_limits() {
        let config: UploadsConfig = toml::from_str("").expect("empty table");
        assert_eq!(config, UploadsConfig::default());
        assert_eq!(config.max_file_bytes, 20 * GIB);
        assert_eq!(config.free_disk_floor(), 10);
        assert_eq!(config.idle_ttl().as_secs(), 24 * 3600);
    }

    #[test]
    fn floors_and_ttls_are_clamped() {
        let config = UploadsConfig {
            free_disk_floor_percent: 200,
            idle_ttl_hours: 0,
            ..UploadsConfig::default()
        };
        assert_eq!(config.free_disk_floor(), 90);
        assert_eq!(config.idle_ttl().as_secs(), 3600);
        assert!(toml::from_str::<UploadsConfig>("max_bytes = 1").is_err());
    }
}
