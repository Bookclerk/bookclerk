//! Build per-sample offset / timing tables from stbl boxes.

use crate::drm::error::{DrmError, Result};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ChunkMapEntry {
    pub first_chunk: u32,
    pub samples_per_chunk: u32,
    pub sample_description_index: u32,
}

/// One audio sample ready for decrypt / remux.
#[derive(Debug, Clone)]
pub struct SampleInfo {
    /// Absolute file offset of the sample payload.
    pub offset: u64,
    pub size: u32,
    /// Composition start time in media timescale ticks.
    pub start_cts: u64,
    /// Sample duration in media timescale ticks.
    pub duration: u32,
    pub chunk_index: u32,
}

pub fn build_samples(
    stts: &[(u32, u32)],
    stsc: &[ChunkMapEntry],
    sample_sizes: &[u32],
    chunk_offsets: &[u64],
) -> Result<Vec<SampleInfo>> {
    if stsc.is_empty() {
        return Err(DrmError::Mp4("stsc is empty".into()));
    }
    if chunk_offsets.is_empty() {
        return Err(DrmError::Mp4("no chunk offsets".into()));
    }

    let mut durations = Vec::with_capacity(sample_sizes.len());
    for &(count, delta) in stts {
        for _ in 0..count {
            durations.push(delta);
        }
    }
    if durations.len() != sample_sizes.len() {
        // Some files use a single stts entry covering all samples; if stts is short,
        // pad with the last delta. If longer, truncate.
        if durations.is_empty() {
            return Err(DrmError::Mp4("stts produced no sample durations".into()));
        }
        if durations.len() < sample_sizes.len() {
            let last = *durations.last().unwrap();
            durations.resize(sample_sizes.len(), last);
        } else {
            durations.truncate(sample_sizes.len());
        }
    }

    // Expand stsc → samples_per_chunk for each chunk.
    let mut samples_per_chunk = Vec::with_capacity(chunk_offsets.len());
    for chunk_idx in 0..chunk_offsets.len() {
        let chunk_number = (chunk_idx as u32) + 1; // 1-based
        let mut spc = stsc[0].samples_per_chunk;
        for entry in stsc {
            if entry.first_chunk <= chunk_number {
                spc = entry.samples_per_chunk;
            } else {
                break;
            }
        }
        samples_per_chunk.push(spc);
    }

    let total_from_chunks: u64 = samples_per_chunk.iter().map(|n| u64::from(*n)).sum();
    if total_from_chunks != sample_sizes.len() as u64 {
        return Err(DrmError::Mp4(format!(
            "sample count mismatch: stsz={} vs stsc/chunks={total_from_chunks}",
            sample_sizes.len()
        )));
    }

    let mut samples = Vec::with_capacity(sample_sizes.len());
    let mut sample_idx = 0usize;
    let mut cts = 0u64;
    for (chunk_idx, &chunk_offset) in chunk_offsets.iter().enumerate() {
        let mut offset = chunk_offset;
        let spc = samples_per_chunk[chunk_idx] as usize;
        for _ in 0..spc {
            let size = sample_sizes[sample_idx];
            let duration = durations[sample_idx];
            samples.push(SampleInfo {
                offset,
                size,
                start_cts: cts,
                duration,
                chunk_index: chunk_idx as u32,
            });
            offset += u64::from(size);
            cts = cts.saturating_add(u64::from(duration));
            sample_idx += 1;
        }
    }
    Ok(samples)
}

/// Select samples whose media time range overlaps `[start_ms, end_ms)`.
#[must_use]
pub fn filter_samples_by_ms(
    samples: &[SampleInfo],
    timescale: u32,
    start_ms: u64,
    end_ms: Option<u64>,
) -> Vec<SampleInfo> {
    if timescale == 0 {
        return samples.to_vec();
    }
    let start_ticks = start_ms.saturating_mul(u64::from(timescale)) / 1000;
    let end_ticks = end_ms.map(|ms| ms.saturating_mul(u64::from(timescale)) / 1000);

    let mut out = Vec::new();
    for sample in samples {
        let sample_end = sample.start_cts.saturating_add(u64::from(sample.duration));
        if sample_end <= start_ticks {
            continue;
        }
        if let Some(end) = end_ticks {
            if sample.start_cts >= end {
                break;
            }
        }
        let mut adjusted = sample.clone();
        // Rebase composition time so the first kept sample starts at 0.
        adjusted.start_cts = sample.start_cts.saturating_sub(start_ticks);
        out.push(adjusted);
    }
    // Ensure contiguous rebase from 0.
    let mut cts = 0u64;
    for sample in &mut out {
        sample.start_cts = cts;
        cts = cts.saturating_add(u64::from(sample.duration));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_linear_samples() {
        let stts = vec![(4, 1024)];
        let stsc = vec![ChunkMapEntry {
            first_chunk: 1,
            samples_per_chunk: 2,
            sample_description_index: 1,
        }];
        let sizes = vec![100, 110, 120, 130];
        let offsets = vec![1000, 2000];
        let samples = build_samples(&stts, &stsc, &sizes, &offsets).unwrap();
        assert_eq!(samples.len(), 4);
        assert_eq!(samples[0].offset, 1000);
        assert_eq!(samples[1].offset, 1100);
        assert_eq!(samples[2].offset, 2000);
        assert_eq!(samples[3].start_cts, 3 * 1024);
    }

    #[test]
    fn filters_by_time() {
        let samples: Vec<_> = (0..10)
            .map(|i| SampleInfo {
                offset: i * 100,
                size: 50,
                start_cts: i * 1000,
                duration: 1000,
                chunk_index: 0,
            })
            .collect();
        // timescale 1000 → 1 tick = 1 ms. Keep 2000..5000 ms.
        let kept = filter_samples_by_ms(&samples, 1000, 2000, Some(5000));
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].start_cts, 0);
        assert_eq!(kept[2].start_cts, 2000);
    }
}
