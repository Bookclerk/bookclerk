//! Durable job-queue settings (`[jobs]` section).

use serde::{Deserialize, Serialize};

/// Default pending+running admission cap.
pub const DEFAULT_MAX_PENDING: u32 = 32;
/// Default worker lease length in seconds.
pub const DEFAULT_LEASE_SECONDS: u64 = 60;
/// Default maximum claims before a job fails terminally.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// Default number of days to keep terminal job rows.
pub const DEFAULT_RETENTION_DAYS: u64 = 7;
/// Default scratch-directory quota (2 GiB).
pub const DEFAULT_TEMP_QUOTA_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Default network-class concurrency (one-at-a-time on a small VPS).
pub const DEFAULT_NETWORK_CONCURRENCY: u32 = 1;

/// `[jobs]` — durable queue admission, leases, and resource-class concurrency.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct JobsConfig {
    /// Maximum pending+running jobs. Further admits return queue-full.
    pub max_pending: u32,
    /// Lease length in seconds; expired running jobs are reclaimed.
    pub lease_seconds: u64,
    /// Maximum claims before a failure is terminal.
    pub max_attempts: u32,
    /// Days to retain succeeded/failed/cancelled rows.
    pub retention_days: u64,
    /// Refuse new acquire scratch dirs when registered temps exceed this.
    pub temp_quota_bytes: u64,
    /// Per-class concurrency (`[jobs.concurrency]`).
    pub concurrency: JobsConcurrencyConfig,
}

/// Per-resource-class worker counts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct JobsConcurrencyConfig {
    /// Scan / acquire / listening-sync workers (default 1).
    pub network: u32,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            max_pending: DEFAULT_MAX_PENDING,
            lease_seconds: DEFAULT_LEASE_SECONDS,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            retention_days: DEFAULT_RETENTION_DAYS,
            temp_quota_bytes: DEFAULT_TEMP_QUOTA_BYTES,
            concurrency: JobsConcurrencyConfig::default(),
        }
    }
}

impl Default for JobsConcurrencyConfig {
    fn default() -> Self {
        Self {
            network: DEFAULT_NETWORK_CONCURRENCY,
        }
    }
}

impl JobsConfig {
    /// Apply `BOOKCLERK_JOBS_*` environment overrides.
    ///
    /// Environment wins over TOML, matching every other section.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("BOOKCLERK_JOBS_MAX_PENDING") {
            if let Ok(n) = v.trim().parse::<u32>() {
                self.max_pending = n.max(1);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_JOBS_LEASE_SECONDS") {
            if let Ok(n) = v.trim().parse::<u64>() {
                self.lease_seconds = n.max(5);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_JOBS_MAX_ATTEMPTS") {
            if let Ok(n) = v.trim().parse::<u32>() {
                self.max_attempts = n.max(1);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_JOBS_RETENTION_DAYS") {
            if let Ok(n) = v.trim().parse::<u64>() {
                self.retention_days = n.max(1);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_JOBS_TEMP_QUOTA_BYTES") {
            if let Ok(n) = v.trim().parse::<u64>() {
                self.temp_quota_bytes = n;
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_JOBS_CONCURRENCY_NETWORK") {
            if let Ok(n) = v.trim().parse::<u32>() {
                self.concurrency.network = n.max(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_small_vps_profile() {
        let cfg = JobsConfig::default();
        assert_eq!(cfg.max_pending, 32);
        assert_eq!(cfg.lease_seconds, 60);
        assert_eq!(cfg.max_attempts, 3);
        assert_eq!(cfg.concurrency.network, 1);
        assert_eq!(cfg.temp_quota_bytes, 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn section_round_trips_through_toml() {
        let config = JobsConfig {
            max_pending: 8,
            lease_seconds: 30,
            max_attempts: 2,
            retention_days: 3,
            temp_quota_bytes: 1024,
            concurrency: JobsConcurrencyConfig { network: 1 },
        };
        let encoded = toml::to_string(&config).expect("serialize");
        let decoded: JobsConfig = toml::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, config);
    }
}
