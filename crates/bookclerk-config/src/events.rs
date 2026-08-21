//! Durable domain-event outbox settings (`[events]` section).

use serde::{Deserialize, Serialize};

/// Default days to keep acked/rejected deliveries (and empty parent events).
pub const DEFAULT_EVENT_RETENTION_DAYS: u64 = 7;
/// Default days to keep dead-lettered deliveries.
pub const DEFAULT_DEAD_LETTER_RETENTION_DAYS: u64 = 30;
/// Default number of local delivery workers.
pub const DEFAULT_EVENT_CONCURRENCY: u32 = 1;

/// `[events]` — outbox retention and local delivery-worker concurrency.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct EventsConfig {
    /// Days to retain acked/rejected deliveries and parent events with no
    /// remaining live deliveries.
    pub retention_days: u64,
    /// Days to retain `dead_letter` deliveries (independent of [`Self::retention_days`]).
    pub dead_letter_retention_days: u64,
    /// Local delivery workers (`[events.concurrency]`; default 1).
    pub concurrency: u32,
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            retention_days: DEFAULT_EVENT_RETENTION_DAYS,
            dead_letter_retention_days: DEFAULT_DEAD_LETTER_RETENTION_DAYS,
            concurrency: DEFAULT_EVENT_CONCURRENCY,
        }
    }
}

impl EventsConfig {
    /// Apply `BOOKCLERK_EVENTS_*` environment overrides.
    ///
    /// Environment wins over TOML, matching every other section.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("BOOKCLERK_EVENTS_RETENTION_DAYS") {
            if let Ok(n) = v.trim().parse::<u64>() {
                self.retention_days = n.max(1);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_EVENTS_DEAD_LETTER_RETENTION_DAYS") {
            if let Ok(n) = v.trim().parse::<u64>() {
                self.dead_letter_retention_days = n.max(1);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_EVENTS_CONCURRENCY") {
            if let Ok(n) = v.trim().parse::<u32>() {
                self.concurrency = n.max(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_small_vps_profile() {
        let cfg = EventsConfig::default();
        assert_eq!(cfg.retention_days, 7);
        assert_eq!(cfg.dead_letter_retention_days, 30);
        assert_eq!(cfg.concurrency, 1);
    }

    #[test]
    fn section_round_trips_through_toml() {
        let config = EventsConfig {
            retention_days: 3,
            dead_letter_retention_days: 14,
            concurrency: 2,
        };
        let encoded = toml::to_string(&config).expect("serialize");
        let decoded: EventsConfig = toml::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, config);
    }
}
