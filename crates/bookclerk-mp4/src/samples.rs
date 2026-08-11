//! Build per-sample offset / timing tables from stbl boxes.

use crate::error::{Mp4Error, Result};

/// One run from the sample-to-chunk (`stsc`) table.
#[derive(Debug, Clone)]
pub struct ChunkMapEntry {
    /// 1-based first chunk index for this `stsc` entry.
    pub first_chunk: u32,
    /// Number of samples in each chunk of this run.
    pub samples_per_chunk: u32,
    /// 1-based index into the sample description table.
    pub sample_description_index: u32,
}

/// One audio sample, located and timed.
#[derive(Debug, Clone)]
pub struct SampleInfo {
    /// Absolute file offset of the sample payload.
    pub offset: u64,
    /// Total box size in bytes including the header.
    pub size: u32,
    /// Composition start time in media timescale ticks.
    pub start_cts: u64,
    /// Sample duration in media timescale ticks.
    pub duration: u32,
    /// 1-based chunk index that contains this sample.
    pub chunk_index: u32,
}

/// Builds a flat sample table from `stts` / `stsc` / `stsz` / `stco` (or `co64`).
///
/// # Arguments
///
/// * `stts` - Numeric `stts` value for this call.
/// * `stsc` - `stsc` input for this call.
/// * `sample_sizes` - Numeric `sample_sizes` value for this call.
/// * `chunk_offsets` - Numeric `chunk_offsets` value for this call.
///
/// # Returns
///
/// On success, the inner `Vec<SampleInfo>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn build_samples(
    stts: &[(u32, u32)],
    stsc: &[ChunkMapEntry],
    sample_sizes: &[u32],
    chunk_offsets: &[u64],
) -> Result<Vec<SampleInfo>> {
    if stsc.is_empty() {
        return Err(Mp4Error::container("stsc is empty"));
    }
    if chunk_offsets.is_empty() {
        return Err(Mp4Error::container("no chunk offsets"));
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
            return Err(Mp4Error::container("stts produced no sample durations"));
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
        return Err(Mp4Error::container(format!(
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

/// Indices of the samples whose media time overlaps `[start_ms, end_ms)`.
///
/// Indices rather than clones: a long audiobook has millions of samples, and the
/// caller needs the original positions anyway to line up any per-sample state it
/// keeps of its own. Output composition times are rebased by the writer.
///
/// # Arguments
///
/// * `samples` - `samples` input for this call.
/// * `timescale` - Numeric `timescale` value for this call.
/// * `start_ms` - Numeric `start_ms` value for this call.
/// * `end_ms` - Numeric `end_ms` value for this call.
///
/// # Returns
///
/// Collected results (may be empty).
#[must_use]
pub fn select_samples_by_ms(
    samples: &[SampleInfo],
    timescale: u32,
    start_ms: u64,
    end_ms: Option<u64>,
) -> Vec<usize> {
    if timescale == 0 {
        return (0..samples.len()).collect();
    }
    let start_ticks = start_ms.saturating_mul(u64::from(timescale)) / 1000;
    let end_ticks = end_ms.map(|ms| ms.saturating_mul(u64::from(timescale)) / 1000);

    let mut kept = Vec::new();
    for (index, sample) in samples.iter().enumerate() {
        let sample_end = sample.start_cts.saturating_add(u64::from(sample.duration));
        if sample_end <= start_ticks {
            continue;
        }
        if let Some(end) = end_ticks {
            if sample.start_cts >= end {
                break;
            }
        }
        kept.push(index);
    }
    kept
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
    fn selects_by_time() {
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
        assert_eq!(
            select_samples_by_ms(&samples, 1000, 2000, Some(5000)),
            vec![2, 3, 4]
        );
        // An open end runs to the last sample.
        assert_eq!(select_samples_by_ms(&samples, 1000, 8000, None), vec![8, 9]);
        // A timescale of zero cannot be converted, so nothing is dropped.
        assert_eq!(
            select_samples_by_ms(&samples, 0, 2000, Some(5000)).len(),
            10
        );
    }
}
