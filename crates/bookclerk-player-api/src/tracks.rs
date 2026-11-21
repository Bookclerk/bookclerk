//! How long each audio file of an item plays, for the track list apps
//! seek across.
//!
//! The playback core knows each file's size and, once probed, its average
//! bitrate; the catalog knows the whole runtime to the minute. Apps need
//! each track's start and duration in seconds, so a file's duration comes
//! from its bitrate when every file has one, and otherwise from its share
//! of the bytes of the whole runtime.

use crate::time::MAX_MS;

/// One file to time: its size and its probed average bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackSize {
    /// Size in bytes.
    pub bytes: u64,
    /// Average bitrate in kilobits per second, when probed (`0` counts as
    /// not probed).
    pub bitrate_kbps: Option<u32>,
}

/// Each track's duration in milliseconds, in order.
///
/// # Arguments
///
/// * `tracks` - The files in play order.
/// * `runtime_ms` - The whole runtime the catalog states, if known.
///
/// # Returns
///
/// From the bitrates when every file has one; else each file's share of
/// the bytes of `runtime_ms`; else zeros. Each value is at most
/// [`MAX_MS`], and the shares add up to the runtime (the last track takes
/// the remainder).
#[must_use]
pub fn track_durations_ms(tracks: &[TrackSize], runtime_ms: Option<u64>) -> Vec<u64> {
    let by_bitrate: Option<Vec<u64>> = tracks
        .iter()
        .map(|track| {
            let kbps = u64::from(track.bitrate_kbps.filter(|kbps| *kbps > 0)?);
            // bytes * 8 bits / (kbps * 1000 bits/s) seconds, in ms.
            let ms = u128::from(track.bytes) * 8 / u128::from(kbps);
            Some(u64::try_from(ms).unwrap_or(MAX_MS).min(MAX_MS))
        })
        .collect();
    if let Some(durations) = by_bitrate {
        return durations;
    }
    let total_bytes: u128 = tracks.iter().map(|track| u128::from(track.bytes)).sum();
    let Some(runtime) = runtime_ms.map(|ms| ms.min(MAX_MS)) else {
        return vec![0; tracks.len()];
    };
    if total_bytes == 0 {
        // Nothing to share by: split the runtime evenly.
        let count = u64::try_from(tracks.len()).unwrap_or(u64::MAX).max(1);
        let each = runtime / count;
        let mut durations = vec![each; tracks.len()];
        if let Some(last) = durations.last_mut() {
            *last = runtime - each * (count - 1);
        }
        return durations;
    }
    let mut durations: Vec<u64> = tracks
        .iter()
        .map(|track| {
            let share = u128::from(runtime) * u128::from(track.bytes) / total_bytes;
            u64::try_from(share).unwrap_or(runtime)
        })
        .collect();
    let given: u64 = durations.iter().sum();
    if let Some(last) = durations.last_mut() {
        *last += runtime.saturating_sub(given);
    }
    durations
}

/// Each track's start in milliseconds: the sum of the durations before it.
///
/// # Arguments
///
/// * `durations_ms` - The durations, in order.
#[must_use]
pub fn track_starts_ms(durations_ms: &[u64]) -> Vec<u64> {
    let mut at = 0u64;
    durations_ms
        .iter()
        .map(|duration| {
            let start = at;
            at = at.saturating_add(*duration);
            start
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrates_time_every_track_when_all_are_probed() {
        let tracks = [
            TrackSize {
                bytes: 16_000,
                bitrate_kbps: Some(64),
            },
            TrackSize {
                bytes: 8_000,
                bitrate_kbps: Some(64),
            },
        ];
        assert_eq!(track_durations_ms(&tracks, Some(1)), [2000, 1000]);
    }

    #[test]
    fn otherwise_the_runtime_is_shared_by_bytes() {
        let tracks = [
            TrackSize {
                bytes: 300,
                bitrate_kbps: Some(64),
            },
            TrackSize {
                bytes: 700,
                bitrate_kbps: None,
            },
        ];
        assert_eq!(track_durations_ms(&tracks, Some(10_001)), [3000, 7001]);
        assert_eq!(track_durations_ms(&tracks, None), [0, 0]);
        let empty = [TrackSize {
            bytes: 0,
            bitrate_kbps: None,
        }; 3];
        assert_eq!(track_durations_ms(&empty, Some(10)), [3, 3, 4]);
        assert_eq!(track_starts_ms(&[3, 3, 4]), [0, 3, 6]);
    }
}
