//! The caps one extraction runs under.

use serde::{Deserialize, Serialize};

/// The caps one extraction runs under.
///
/// Every cap counts bytes as they are written, never sizes an archive
/// declares. The host chooses them per job (the total is never more than the
/// uploader's remaining quota) and sends them to the worker with the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    /// Most entries the archive may list, counting refused ones. An archive
    /// that lists more is stopped before anything is written.
    pub max_entries: u32,
    /// Most bytes the whole extraction may write. Reaching it stops the
    /// extraction.
    pub max_total_bytes: u64,
    /// Most bytes one entry may write. An entry that needs more is refused
    /// and its partial file discarded.
    pub max_entry_bytes: u64,
    /// Largest ratio of written bytes to compressed bytes an entry may reach
    /// once it is past [`Limits::ratio_floor_bytes`]. Audio barely
    /// compresses, so an honest audiobook sits near 1:1.
    pub max_ratio: u64,
    /// Bytes an entry (or a whole gzip stream) may write before the ratio
    /// applies, so small, very compressible text files are not refused.
    pub ratio_floor_bytes: u64,
}

impl Limits {
    /// The defaults for uploads: 10,000 entries, 64 GiB in all, 20 GiB per
    /// entry, 100:1 past 1 MiB.
    pub const UPLOAD_DEFAULTS: Self = Self {
        max_entries: 10_000,
        max_total_bytes: 64 * GIB,
        max_entry_bytes: 20 * GIB,
        max_ratio: 100,
        ratio_floor_bytes: 1024 * 1024,
    };

    /// The same caps with the total lowered to `total` when that is smaller,
    /// for an uploader with less quota left than the default total.
    ///
    /// # Arguments
    ///
    /// * `total` - The most bytes this extraction may write.
    #[must_use]
    pub fn with_total_at_most(self, total: u64) -> Self {
        Self {
            max_total_bytes: self.max_total_bytes.min(total),
            ..self
        }
    }

    /// Most bytes something compressed to `compressed` bytes may expand to.
    ///
    /// # Arguments
    ///
    /// * `compressed` - The entry's (or stream's) compressed size in bytes.
    #[must_use]
    pub fn ratio_allowance(&self, compressed: u64) -> u64 {
        compressed
            .saturating_mul(self.max_ratio)
            .max(self.ratio_floor_bytes)
    }
}

/// One gibibyte.
const GIB: u64 = 1024 * 1024 * 1024;

impl Default for Limits {
    fn default() -> Self {
        Self::UPLOAD_DEFAULTS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_allowance_has_a_floor() {
        let limits = Limits::UPLOAD_DEFAULTS;
        assert_eq!(limits.ratio_allowance(0), 1024 * 1024);
        assert_eq!(limits.ratio_allowance(1024 * 1024), 100 * 1024 * 1024);
        assert_eq!(limits.ratio_allowance(u64::MAX), u64::MAX);
    }

    #[test]
    fn total_only_shrinks() {
        let limits = Limits::UPLOAD_DEFAULTS.with_total_at_most(10);
        assert_eq!(limits.max_total_bytes, 10);
        let limits = Limits::UPLOAD_DEFAULTS.with_total_at_most(u64::MAX);
        assert_eq!(limits.max_total_bytes, 64 * GIB);
    }
}
