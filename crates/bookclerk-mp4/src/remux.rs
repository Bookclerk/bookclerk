//! Remux a progressive MP4 into a faststart M4B, one sample at a time.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::boxutil::FourCC;
use crate::edit::{find_box_range, find_child_in_range, find_direct_child, splice_replace};
use crate::error::{Mp4Error, Result};
use crate::parser::parse_mp4;
use crate::read::{SampleReader, IO_BUFFER_BYTES};
use crate::samples::select_samples_by_ms;

/// Optional media-time trim window in milliseconds (absolute, pre-rebase).
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct TrimRange {
    pub start_ms: u64,
    /// Exclusive end; `None` means "through end of media".
    pub end_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct RemuxOptions {
    pub trim: Option<TrimRange>,
}

/// Rewrites sample payloads as they stream from input to output.
///
/// The remuxer moves bytes and rebuilds tables; it never inspects a payload.
/// A caller that has to turn its own ciphertext into plaintext keeps the key and
/// the cipher on its side of this trait, which is how store plugins decrypt
/// their downloads without any of that living here. [`CopySamples`] is the only
/// implementation in this crate.
pub trait SampleTransform {
    /// The original sample indices kept by the trim, in output order.
    ///
    /// Called once before any payload, so a transform holding per-sample state
    /// (a table of initialization vectors, say) can narrow it to the same
    /// selection and then index it by output position.
    fn retain(&mut self, kept: &[usize]) -> Result<()> {
        let _ = kept;
        Ok(())
    }

    /// Rewrite output sample `index` in place. The length must not change.
    fn sample(&mut self, index: usize, payload: &mut [u8]) -> Result<()>;
}

/// Copies every sample through untouched.
#[derive(Debug, Clone, Copy, Default)]
pub struct CopySamples;

impl SampleTransform for CopySamples {
    fn sample(&mut self, _index: usize, _payload: &mut [u8]) -> Result<()> {
        Ok(())
    }
}

/// Inputs for writing a progressive faststart M4B (ftyp + moov + mdat) in one pass.
pub struct ProgressiveWriteInput<'a> {
    pub moov_bytes: &'a [u8],
    pub moov_file_start: u64,
    pub sample_entry_type_offset: u64,
    pub audio_timescale: u32,
    pub mvhd_timescale: u32,
    pub sample_sizes: &'a [u32],
    pub durations: &'a [u32],
}

/// Copy (and optionally trim) a progressive MP4 into a faststart M4B, passing
/// every payload through `transform`.
///
/// Sample *payloads* are streamed one buffer at a time. Only the retained sizes,
/// durations, and read offsets are held in memory; the parsed sample table is
/// never cloned.
pub fn remux_progressive(
    input: &Path,
    output: &Path,
    opts: &RemuxOptions,
    transform: &mut dyn SampleTransform,
) -> Result<()> {
    let mp4 = parse_mp4(input)?;
    let timescale = mp4.audio.timescale;
    let samples = &mp4.audio.samples;

    let kept = match opts.trim {
        Some(trim) => select_samples_by_ms(samples, timescale, trim.start_ms, trim.end_ms),
        None => (0..samples.len()).collect(),
    };
    if kept.is_empty() {
        return Err(Mp4Error::container(
            "no samples remain after trim; check brand intro/outro durations",
        ));
    }
    transform.retain(&kept)?;

    let mut offsets = Vec::with_capacity(kept.len());
    let mut sample_sizes = Vec::with_capacity(kept.len());
    let mut durations = Vec::with_capacity(kept.len());
    for &index in &kept {
        let sample = &samples[index];
        offsets.push(sample.offset);
        sample_sizes.push(sample.size);
        durations.push(sample.duration);
    }

    let mut src = SampleReader::open(input)?;

    write_progressive_m4b(
        output,
        ProgressiveWriteInput {
            moov_bytes: &mp4.moov_bytes,
            moov_file_start: mp4.moov.start,
            sample_entry_type_offset: mp4.audio.sample_entry_type_offset,
            audio_timescale: mp4.audio.timescale,
            mvhd_timescale: mp4.mvhd_timescale,
            sample_sizes: &sample_sizes,
            durations: &durations,
        },
        |i, buf| {
            src.read_sample(offsets[i], sample_sizes[i] as usize, buf)?;
            transform.sample(i, buf)
        },
    )
}

/// Write a progressive faststart M4B by streaming one sample at a time.
///
/// Layout is written as `ftyp` + `moov` + `mdat` in a single pass (no full-file
/// rewrite), and `ftyp` always declares M4B brands. `fill_sample(i, buf)` must
/// populate `buf` with the payload for sample `i` (length must match
/// `sample_sizes[i]`). Only one sample buffer is live at a time.
///
/// Callers that assemble their own sample plan — from fragments, say, rather
/// than from one progressive `mdat` — use this directly.
pub fn write_progressive_m4b<F>(
    output: &Path,
    input: ProgressiveWriteInput<'_>,
    mut fill_sample: F,
) -> Result<()>
where
    F: FnMut(usize, &mut Vec<u8>) -> Result<()>,
{
    if input.sample_sizes.is_empty() {
        return Err(Mp4Error::container("no samples to write"));
    }
    if input.sample_sizes.len() != input.durations.len() {
        return Err(Mp4Error::container(format!(
            "payload/duration count mismatch: {} vs {}",
            input.sample_sizes.len(),
            input.durations.len()
        )));
    }

    let media_duration: u64 = input.durations.iter().map(|d| u64::from(*d)).sum();
    let payload_total: u64 = input.sample_sizes.iter().map(|s| u64::from(*s)).sum();

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let ftyp_bytes = build_m4b_ftyp();
    let ftyp_len = ftyp_bytes.len() as u64;
    const MDAT_HEADER_LEN: u64 = 16; // 64-bit size form

    // Resolve moov length ↔ chunk offsets (usually converges in one extra pass).
    let mut moov_len = 0u64;
    let moov = loop {
        let mut offset = ftyp_len + moov_len + MDAT_HEADER_LEN;
        let mut chunk_offsets = Vec::with_capacity(input.sample_sizes.len());
        for &size in input.sample_sizes {
            chunk_offsets.push(offset);
            offset = offset.saturating_add(u64::from(size));
        }
        let built = rebuild_moov(
            input.moov_bytes,
            input.moov_file_start,
            input.sample_entry_type_offset,
            input.audio_timescale,
            input.sample_sizes,
            input.durations,
            &chunk_offsets,
            media_duration,
            input.mvhd_timescale,
        )?;
        let built_len = built.len() as u64;
        if built_len == moov_len {
            break built;
        }
        moov_len = built_len;
    };

    let mut out = BufWriter::with_capacity(IO_BUFFER_BYTES, File::create(output)?);
    out.write_all(&ftyp_bytes)?;
    out.write_all(&moov)?;

    let mdat_size = MDAT_HEADER_LEN + payload_total;
    out.write_all(&1u32.to_be_bytes())?; // size=1 → 64-bit
    out.write_all(b"mdat")?;
    out.write_all(&mdat_size.to_be_bytes())?;

    let mut sample_buf = Vec::new();
    for (i, &expected) in input.sample_sizes.iter().enumerate() {
        fill_sample(i, &mut sample_buf)?;
        if sample_buf.len() as u32 != expected {
            return Err(Mp4Error::container(format!(
                "sample {i} size {} != expected {expected}",
                sample_buf.len()
            )));
        }
        out.write_all(&sample_buf)?;
    }
    out.into_inner()
        .map_err(std::io::IntoInnerError::into_error)?
        .sync_all()?;
    Ok(())
}

fn build_m4b_ftyp() -> Vec<u8> {
    // major=M4B , minor=0, brands: M4B , mp42, isom, iso2
    let brands: &[&[u8; 4]] = &[b"M4B ", b"mp42", b"isom", b"iso2"];
    let size = 8 + 8 + brands.len() * 4;
    let mut buf = Vec::with_capacity(size);
    buf.extend_from_slice(&(size as u32).to_be_bytes());
    buf.extend_from_slice(b"ftyp");
    buf.extend_from_slice(b"M4B ");
    buf.extend_from_slice(&0u32.to_be_bytes());
    for b in brands {
        buf.extend_from_slice(*b);
    }
    buf
}

/// Patch moov sample tables + durations for the remuxed media.
#[allow(clippy::too_many_arguments)]
fn rebuild_moov(
    moov_bytes: &[u8],
    moov_file_start: u64,
    sample_entry_type_offset: u64,
    audio_timescale: u32,
    sample_sizes: &[u32],
    durations: &[u32],
    chunk_offsets: &[u64],
    media_duration: u64,
    mvhd_timescale: u32,
) -> Result<Vec<u8>> {
    if moov_bytes.len() < 8 {
        return Err(Mp4Error::container("moov too small"));
    }

    let mut body = moov_bytes.to_vec();
    let type_rel = sample_entry_type_offset
        .checked_sub(moov_file_start)
        .ok_or_else(|| Mp4Error::container("sample entry offset outside moov"))?;
    let type_rel = usize::try_from(type_rel)
        .map_err(|_| Mp4Error::container("sample entry offset overflow"))?;

    // Clear progressive encryption sample-entry markup before rewriting tables.
    body = clear_protection_markup(&body, type_rel)?;

    // Replace stts / stsc / stsz / stco|co64 boxes inside stbl.
    let stts = encode_stts(durations);
    let stsc = encode_stsc_one_per_chunk();
    let stsz = encode_stsz(sample_sizes);
    let need_co64 = chunk_offsets.iter().any(|&o| o > u64::from(u32::MAX));
    let stco = if need_co64 {
        encode_co64(chunk_offsets)
    } else {
        encode_stco(&chunk_offsets.iter().map(|&o| o as u32).collect::<Vec<_>>())
    };

    body = replace_stbl_child(&body, b"stts", &stts)
        .map_err(|e| Mp4Error::container(format!("replace stts: {e}")))?;
    body = replace_stbl_child(&body, b"stsc", &stsc)
        .map_err(|e| Mp4Error::container(format!("replace stsc: {e}")))?;
    body = replace_stbl_child(&body, b"stsz", &stsz)
        .map_err(|e| Mp4Error::container(format!("replace stsz: {e}")))?;
    body = replace_chunk_offset_box(&body, &stco)
        .map_err(|e| Mp4Error::container(format!("replace chunk offsets: {e}")))?;

    // Drop CENC sample-aux boxes if present (their IVs no longer describe the
    // payloads once a transform has rewritten them).
    body = remove_stbl_children_named(&body, &[b"saiz", b"saio"])?;

    // Update durations in mdhd / tkhd / mvhd.
    let movie_duration = if mvhd_timescale == 0 || audio_timescale == 0 {
        media_duration
    } else {
        media_duration * u64::from(mvhd_timescale) / u64::from(audio_timescale)
    };
    patch_duration_fields(&mut body, media_duration, movie_duration)?;

    // Fix outer moov size.
    let size = body.len() as u32;
    body[0..4].copy_from_slice(&size.to_be_bytes());
    Ok(body)
}

/// Rewrite a protected sample entry as the clear format it wraps.
///
/// `aavd` becomes `mp4a`; `enca` becomes whatever its `sinf`/`frma` names, and
/// the `sinf` describing the protection is removed. Anything else is left alone.
fn clear_protection_markup(moov: &[u8], type_rel: usize) -> Result<Vec<u8>> {
    let mut body = moov.to_vec();
    if type_rel + 4 > body.len() {
        return Ok(body);
    }
    let entry_type = &body[type_rel..type_rel + 4];
    if entry_type == b"aavd" {
        body[type_rel..type_rel + 4].copy_from_slice(b"mp4a");
        return Ok(body);
    }
    if entry_type != b"enca" {
        return Ok(body);
    }

    // Sample entry box starts 4 bytes before the type field.
    let entry_pos = type_rel.saturating_sub(4);
    if entry_pos + 8 > body.len() {
        return Err(Mp4Error::container("enca sample entry truncated"));
    }
    let entry_size =
        u32::from_be_bytes(body[entry_pos..entry_pos + 4].try_into().unwrap()) as usize;
    let entry_end = entry_pos + entry_size;
    if entry_end > body.len() || entry_size < 36 {
        return Err(Mp4Error::container("invalid enca sample entry"));
    }

    let children_start = entry_pos + 36;
    let sinf = find_child_in_range(&body, children_start, entry_end, b"sinf")?
        .ok_or_else(|| Mp4Error::container("enca missing sinf"))?;
    let frma = find_child_in_range(&body, sinf.0 + 8, sinf.1, b"frma")?
        .ok_or_else(|| Mp4Error::container("sinf missing frma"))?;
    if frma.0 + 12 > frma.1 {
        return Err(Mp4Error::container("frma truncated"));
    }
    let format = body[frma.0 + 8..frma.0 + 12].to_vec();
    body[type_rel..type_rel + 4].copy_from_slice(&format);
    body = splice_replace(&body, sinf.0, sinf.1, &[])?;
    Ok(body)
}

fn remove_stbl_children_named(moov: &[u8], names: &[&[u8; 4]]) -> Result<Vec<u8>> {
    let mut body = moov.to_vec();
    loop {
        let (stbl_start, stbl_end) = match find_box_range(&body, b"stbl")? {
            Some(r) => r,
            None => return Ok(body),
        };
        let mut removed = false;
        for name in names {
            if let Some((start, end)) = find_direct_child(&body, stbl_start, stbl_end, name)? {
                body = splice_replace(&body, start, end, &[])?;
                removed = true;
                break;
            }
        }
        if !removed {
            break;
        }
    }
    Ok(body)
}

fn encode_stts(durations: &[u32]) -> Vec<u8> {
    // Run-length encode.
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &d in durations {
        if let Some(last) = runs.last_mut() {
            if last.1 == d {
                last.0 += 1;
                continue;
            }
        }
        runs.push((1, d));
    }
    let mut buf = Vec::new();
    let size = 8 + 4 + 4 + runs.len() * 8;
    buf.extend_from_slice(&(size as u32).to_be_bytes());
    buf.extend_from_slice(b"stts");
    buf.extend_from_slice(&0u32.to_be_bytes()); // version+flags
    buf.extend_from_slice(&(runs.len() as u32).to_be_bytes());
    for (count, delta) in runs {
        buf.extend_from_slice(&count.to_be_bytes());
        buf.extend_from_slice(&delta.to_be_bytes());
    }
    buf
}

fn encode_stsc_one_per_chunk() -> Vec<u8> {
    // first_chunk=1, samples_per_chunk=1, desc=1 — one entry covers all chunks.
    let size = 8 + 4 + 4 + 12;
    let mut buf = Vec::with_capacity(size);
    buf.extend_from_slice(&(size as u32).to_be_bytes());
    buf.extend_from_slice(b"stsc");
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&1u32.to_be_bytes());
    buf.extend_from_slice(&1u32.to_be_bytes()); // first_chunk
    buf.extend_from_slice(&1u32.to_be_bytes()); // samples_per_chunk
    buf.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
    buf
}

fn encode_stsz(sizes: &[u32]) -> Vec<u8> {
    let size = 8 + 4 + 4 + 4 + sizes.len() * 4;
    let mut buf = Vec::with_capacity(size);
    buf.extend_from_slice(&(size as u32).to_be_bytes());
    buf.extend_from_slice(b"stsz");
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes()); // sample_size=0 → table follows
    buf.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
    for s in sizes {
        buf.extend_from_slice(&s.to_be_bytes());
    }
    buf
}

fn encode_stco(offsets: &[u32]) -> Vec<u8> {
    let size = 8 + 4 + 4 + offsets.len() * 4;
    let mut buf = Vec::with_capacity(size);
    buf.extend_from_slice(&(size as u32).to_be_bytes());
    buf.extend_from_slice(b"stco");
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
    for o in offsets {
        buf.extend_from_slice(&o.to_be_bytes());
    }
    buf
}

fn encode_co64(offsets: &[u64]) -> Vec<u8> {
    let size = 8 + 4 + 4 + offsets.len() * 8;
    let mut buf = Vec::with_capacity(size);
    buf.extend_from_slice(&(size as u32).to_be_bytes());
    buf.extend_from_slice(b"co64");
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
    for o in offsets {
        buf.extend_from_slice(&o.to_be_bytes());
    }
    buf
}

/// Find a direct child of the first `stbl` in `moov` and replace it.
fn replace_stbl_child(moov: &[u8], fourcc: &[u8; 4], replacement: &[u8]) -> Result<Vec<u8>> {
    let (stbl_start, stbl_end) =
        find_box_range(moov, b"stbl")?.ok_or_else(|| Mp4Error::container("moov missing stbl"))?;
    let child = find_direct_child(moov, stbl_start, stbl_end, fourcc)?
        .ok_or_else(|| Mp4Error::container(format!("stbl missing {}", FourCC(*fourcc))))?;
    splice_replace(moov, child.0, child.1, replacement)
}

fn replace_chunk_offset_box(moov: &[u8], replacement: &[u8]) -> Result<Vec<u8>> {
    let (stbl_start, stbl_end) =
        find_box_range(moov, b"stbl")?.ok_or_else(|| Mp4Error::container("moov missing stbl"))?;
    if let Some(child) = find_direct_child(moov, stbl_start, stbl_end, b"stco")? {
        return splice_replace(moov, child.0, child.1, replacement);
    }
    if let Some(child) = find_direct_child(moov, stbl_start, stbl_end, b"co64")? {
        return splice_replace(moov, child.0, child.1, replacement);
    }
    Err(Mp4Error::container("stbl missing stco/co64"))
}

fn patch_duration_fields(moov: &mut [u8], media_duration: u64, movie_duration: u64) -> Result<()> {
    // Patch first mvhd, first tkhd (audio), first mdhd durations.
    patch_named_duration(moov, b"mvhd", movie_duration)?;
    patch_named_duration(moov, b"tkhd", movie_duration)?;
    patch_named_duration(moov, b"mdhd", media_duration)?;
    Ok(())
}

fn patch_named_duration(moov: &mut [u8], fourcc: &[u8; 4], duration: u64) -> Result<()> {
    let Some((start, end)) = find_box_range(moov, fourcc)? else {
        return Ok(());
    };
    if start + 12 > end {
        return Ok(());
    }
    let version = moov[start + 8];
    match (fourcc, version) {
        (b"mvhd", 1) => {
            // version(1)+flags(3)+ctime(8)+mtime(8)+timescale(4)+duration(8)
            let off = start + 8 + 4 + 8 + 8 + 4;
            if off + 8 <= end {
                moov[off..off + 8].copy_from_slice(&duration.to_be_bytes());
            }
        }
        (b"mvhd", _) => {
            let off = start + 8 + 4 + 4 + 4 + 4;
            if off + 4 <= end {
                moov[off..off + 4].copy_from_slice(&(duration as u32).to_be_bytes());
            }
        }
        (b"tkhd", 1) => {
            // ver+flags + ctime8 + mtime8 + track_id4 + reserved4 + duration8
            let off = start + 8 + 4 + 8 + 8 + 4 + 4;
            if off + 8 <= end {
                moov[off..off + 8].copy_from_slice(&duration.to_be_bytes());
            }
        }
        (b"tkhd", _) => {
            let off = start + 8 + 4 + 4 + 4 + 4 + 4;
            if off + 4 <= end {
                moov[off..off + 4].copy_from_slice(&(duration as u32).to_be_bytes());
            }
        }
        (b"mdhd", 1) => {
            let off = start + 8 + 4 + 8 + 8 + 4;
            if off + 8 <= end {
                moov[off..off + 8].copy_from_slice(&duration.to_be_bytes());
            }
        }
        (b"mdhd", _) => {
            let off = start + 8 + 4 + 4 + 4 + 4;
            if off + 4 <= end {
                moov[off..off + 4].copy_from_slice(&(duration as u32).to_be_bytes());
            }
        }
        _ => {}
    }
    Ok(())
}
