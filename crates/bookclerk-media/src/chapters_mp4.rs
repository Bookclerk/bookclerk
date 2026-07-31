//! Write Nero `chpl` + QuickTime chapter text tracks in one pass.
//!
//! Apple AVFoundation (iOS players) ignores Nero `chpl` and only surfaces a
//! QuickTime text track that matches ffmpeg's layout:
//! - `tkhd` flags = TrackInMovie (`0x2`), not fully disabled
//! - media timescale 1000 (milliseconds)
//! - `edts`/`elst` mapping the media onto the movie timeline
//! - `gmhd` with `gmin` + Text Media Information `text` atom
//! - `stsd` Text SampleEntry with `reserved[6]` + `data_reference_index=1`
//!   before `displayFlags`, plus trailing `ftab`
//! - samples: `u16` length + UTF-8 title + `encd` (UTF-8)
//!
//! `mp4ameta` writes a different (non-AVFoundation) chapter track; we therefore
//! leave chapter writing to this module and use mp4ameta for tags only.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{MediaError, Result};

/// Media timescale for the QuickTime chapter text track (ffmpeg / AVFoundation).
const CHAPTER_TIMESCALE: u32 = 1000;
/// Nero `chpl` start-time timescale (mp4ameta / Nero default).
const CHPL_TIMESCALE: u64 = 10_000_000;

const ENCD: [u8; 12] = [
    0, 0, 0, 12, // size
    b'e', b'n', b'c', b'd', //
    0, 0, 1, 0, // UTF-8
];

/// Known-good QuickTime `text` sample entry (59 bytes), matching ffmpeg.
const TEXT_SAMPLE_ENTRY: &[u8] = &[
    0x00, 0x00, 0x00, 0x3B, // size = 59
    b't', b'e', b'x', b't', //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // reserved
    0x00, 0x01, // data_reference_index
    0x00, 0x00, 0x00, 0x01, // displayFlags
    0x00, 0x00, // justification
    0x00, 0x00, 0x00, 0x00, // bg color
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // default text box
    0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, // style record
    0x00, 0x00, 0x00, 0x00, // fg color
    0x00, 0x00, 0x00, 0x0D, b'f', b't', b'a', b'b', // font table
    0x00, 0x01, 0x00, 0x01, 0x00,
];

/// ffmpeg Text Media Information payload inside `gmhd`/`text` (36 bytes).
const GMHD_TEXT_PAYLOAD: &[u8] = &[
    0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x40, 0x00,
];

/// Write Nero `chpl` and an AVFoundation-readable QuickTime chapter track.
///
/// Replaces any existing chapter track / `chpl`. Works on either M4B layout: the
/// title samples go into their own `mdat` at the end of the file, so the audio
/// never moves and only `moov` is read, edited and written back.
pub fn write_audiobook_chapters(path: &Path, chapters: &[(String, u64)]) -> Result<()> {
    if chapters.is_empty() {
        return Ok(());
    }

    let (at, mut moov_buf) = bookclerk_mp4::read_moov(path)?;
    bookclerk_mp4::strip_trailing_free(&mut moov_buf)?;
    strip_existing_chapters(&mut moov_buf)?;

    let (movie_timescale, movie_duration) = read_mvhd_timing(&moov_buf)?;
    if movie_timescale == 0 {
        return Err(MediaError::Native("mvhd timescale is zero".into()));
    }

    let duration_ms = ((movie_duration as u128) * 1000 / movie_timescale as u128) as u64;
    let (sample_payload, sample_sizes, sample_deltas) =
        build_chapter_samples(chapters, duration_ms)?;
    let sample_offset = append_chapter_media(path, &sample_payload)?;

    let audio_track_id = first_audio_track_id(&moov_buf)?.unwrap_or(1);
    let chapter_track_id = next_track_id(&moov_buf);

    let chapter_trak = build_chapter_trak(
        chapter_track_id,
        movie_timescale,
        movie_duration,
        duration_ms,
        &sample_sizes,
        &sample_deltas,
        sample_offset,
    )?;
    insert_chapter_trak(&mut moov_buf, &chapter_trak)?;
    set_audio_chap_ref(&mut moov_buf, audio_track_id, chapter_track_id)?;
    upsert_nero_chpl(&mut moov_buf, chapters)?;
    let moov_buf_len = moov_buf.len();
    set_atom_size(&mut moov_buf, 0, moov_buf_len)?;

    crate::moov::write_moov(path, at, moov_buf)?;

    tracing::info!(
        path = %path.display(),
        chapters = chapters.len(),
        "wrote Nero chpl + AVFoundation QuickTime chapter track"
    );
    Ok(())
}

/// Put the chapter title samples in their own `mdat` at the end of the file, and
/// answer with the offset the first one landed at.
///
/// Chapter text is a few kilobytes against a few hundred megabytes of audio, and
/// a sample table addresses the file rather than a particular box, so there is
/// nothing to gain by squeezing these in beside the audio and a whole book to
/// rewrite if we try. A second `mdat` is ordinary ISO-BMFF.
///
/// Re-running drops the one left by the previous run first, so writing chapters
/// twice does not leave the first set stranded in the file.
fn append_chapter_media(path: &Path, payload: &[u8]) -> Result<u64> {
    let mut file = File::options().read(true).write(true).open(path)?;
    let boxes = bookclerk_mp4::top_level_boxes(&mut file)?;

    let media = boxes
        .iter()
        .position(|header| header.kind.0 == *b"mdat")
        .ok_or_else(|| MediaError::Native("M4B missing mdat; cannot write chapters".into()))?;
    let end = match boxes.last() {
        Some(last) if last.kind.0 == *b"mdat" && last.start != boxes[media].start => last.start,
        _ => file.seek(SeekFrom::End(0))?,
    };
    file.set_len(end)?;

    let header_len: u64 = if payload.len() as u64 + 8 > u64::from(u32::MAX) {
        16
    } else {
        8
    };
    let mut header = Vec::with_capacity(header_len as usize);
    write_atom_header(&mut header, header_len + payload.len() as u64, *b"mdat")?;

    file.seek(SeekFrom::Start(end))?;
    file.write_all(&header)?;
    file.write_all(payload)?;
    file.sync_all()?;
    Ok(end + header_len)
}

fn build_chapter_samples(
    chapters: &[(String, u64)],
    duration_ms: u64,
) -> Result<(Vec<u8>, Vec<u32>, Vec<u32>)> {
    let mut payload = Vec::new();
    let mut sizes = Vec::with_capacity(chapters.len());
    let mut deltas = Vec::with_capacity(chapters.len());

    for (i, (title, start_ms)) in chapters.iter().enumerate() {
        let end_ms = chapters
            .get(i + 1)
            .map(|(_, s)| *s)
            .unwrap_or(duration_ms)
            .max(*start_ms + 1);
        let delta = (end_ms - start_ms).min(u64::from(u32::MAX)) as u32;
        deltas.push(delta.max(1));

        let title_bytes = title.as_bytes();
        let title_len = title_bytes.len().min(usize::from(u16::MAX));
        let sample_size = 2 + title_len + ENCD.len();
        sizes.push(sample_size as u32);
        payload.extend_from_slice(&(title_len as u16).to_be_bytes());
        payload.extend_from_slice(&title_bytes[..title_len]);
        payload.extend_from_slice(&ENCD);
    }
    Ok((payload, sizes, deltas))
}

fn build_chapter_trak(
    track_id: u32,
    movie_timescale: u32,
    movie_duration: u64,
    duration_ms: u64,
    sample_sizes: &[u32],
    sample_deltas: &[u32],
    sample_offset: u64,
) -> Result<Vec<u8>> {
    let _ = movie_timescale; // duration already in movie units
    let media_duration = duration_ms.min(u64::from(u32::MAX)) as u32;
    let movie_dur_u32 = movie_duration.min(u64::from(u32::MAX)) as u32;

    let tkhd = build_tkhd(track_id, movie_dur_u32);
    let edts = build_edts(movie_dur_u32);
    let mdhd = build_mdhd(CHAPTER_TIMESCALE, media_duration);
    let hdlr = build_hdlr_text();
    let minf = build_minf(sample_sizes, sample_deltas, sample_offset)?;
    let mdia = wrap_container(b"mdia", &[mdhd, hdlr, minf]);
    Ok(wrap_container(b"trak", &[tkhd, edts, mdia]))
}

fn build_tkhd(track_id: u32, movie_duration: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(84);
    body.push(0); // version
    body.extend_from_slice(&[0x00, 0x00, 0x02]); // TrackInMovie (ffmpeg)
    body.extend_from_slice(&0u32.to_be_bytes()); // creation
    body.extend_from_slice(&0u32.to_be_bytes()); // modification
    body.extend_from_slice(&track_id.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes()); // reserved
    body.extend_from_slice(&movie_duration.to_be_bytes());
    body.extend_from_slice(&[0u8; 8]); // reserved
    body.extend_from_slice(&0u16.to_be_bytes()); // layer
    body.extend_from_slice(&0u16.to_be_bytes()); // alternate_group
    body.extend_from_slice(&0u16.to_be_bytes()); // volume
    body.extend_from_slice(&0u16.to_be_bytes()); // reserved
    body.extend_from_slice(&identity_matrix());
    body.extend_from_slice(&0u32.to_be_bytes()); // width
    body.extend_from_slice(&0u32.to_be_bytes()); // height
    wrap_atom(b"tkhd", &body)
}

fn build_edts(movie_duration: u32) -> Vec<u8> {
    let mut elst = Vec::with_capacity(20);
    elst.extend_from_slice(&0u32.to_be_bytes()); // ver/flags
    elst.extend_from_slice(&1u32.to_be_bytes()); // entry count
    elst.extend_from_slice(&movie_duration.to_be_bytes());
    elst.extend_from_slice(&0u32.to_be_bytes()); // media time
    elst.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
    wrap_container(b"edts", &[wrap_atom(b"elst", &elst)])
}

fn build_mdhd(timescale: u32, duration: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&0u32.to_be_bytes()); // ver/flags
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&timescale.to_be_bytes());
    body.extend_from_slice(&duration.to_be_bytes());
    body.extend_from_slice(&0x55C4u16.to_be_bytes()); // und
    body.extend_from_slice(&0u16.to_be_bytes());
    wrap_atom(b"mdhd", &body)
}

fn build_hdlr_text() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(b"text");
    body.extend_from_slice(&[0u8; 12]);
    body.extend_from_slice(b"ChapterHandler\0");
    wrap_atom(b"hdlr", &body)
}

fn build_minf(sample_sizes: &[u32], sample_deltas: &[u32], sample_offset: u64) -> Result<Vec<u8>> {
    let gmhd = build_gmhd();
    let dinf = build_dinf();
    let stbl = build_stbl(sample_sizes, sample_deltas, sample_offset)?;
    Ok(wrap_container(b"minf", &[gmhd, dinf, stbl]))
}

fn build_gmhd() -> Vec<u8> {
    let mut gmin = Vec::with_capacity(16);
    gmin.extend_from_slice(&0u32.to_be_bytes());
    gmin.extend_from_slice(&0x0040u16.to_be_bytes());
    for _ in 0..3 {
        gmin.extend_from_slice(&0x8000u16.to_be_bytes());
    }
    gmin.extend_from_slice(&0u16.to_be_bytes());
    gmin.extend_from_slice(&0u16.to_be_bytes());
    let text = wrap_atom(b"text", GMHD_TEXT_PAYLOAD);
    wrap_container(b"gmhd", &[wrap_atom(b"gmin", &gmin), text])
}

fn build_dinf() -> Vec<u8> {
    let mut url = Vec::with_capacity(4);
    url.extend_from_slice(&0u32.to_be_bytes());
    url[3] = 1; // self-contained
    let url = wrap_atom(b"url ", &url);
    let mut dref = Vec::new();
    dref.extend_from_slice(&0u32.to_be_bytes());
    dref.extend_from_slice(&1u32.to_be_bytes());
    dref.extend_from_slice(&url);
    wrap_container(b"dinf", &[wrap_atom(b"dref", &dref)])
}

fn build_stbl(sample_sizes: &[u32], sample_deltas: &[u32], sample_offset: u64) -> Result<Vec<u8>> {
    let mut stsd_body = Vec::new();
    stsd_body.extend_from_slice(&0u32.to_be_bytes());
    stsd_body.extend_from_slice(&1u32.to_be_bytes());
    stsd_body.extend_from_slice(TEXT_SAMPLE_ENTRY);
    let stsd = wrap_atom(b"stsd", &stsd_body);

    let mut stts_body = Vec::new();
    stts_body.extend_from_slice(&0u32.to_be_bytes());
    stts_body.extend_from_slice(&(sample_deltas.len() as u32).to_be_bytes());
    for delta in sample_deltas {
        stts_body.extend_from_slice(&1u32.to_be_bytes());
        stts_body.extend_from_slice(&delta.to_be_bytes());
    }
    let stts = wrap_atom(b"stts", &stts_body);

    let mut stsc_body = Vec::new();
    stsc_body.extend_from_slice(&0u32.to_be_bytes());
    stsc_body.extend_from_slice(&1u32.to_be_bytes());
    stsc_body.extend_from_slice(&1u32.to_be_bytes());
    stsc_body.extend_from_slice(&(sample_sizes.len() as u32).to_be_bytes());
    stsc_body.extend_from_slice(&1u32.to_be_bytes());
    let stsc = wrap_atom(b"stsc", &stsc_body);

    let mut stsz_body = Vec::new();
    stsz_body.extend_from_slice(&0u32.to_be_bytes());
    stsz_body.extend_from_slice(&0u32.to_be_bytes());
    stsz_body.extend_from_slice(&(sample_sizes.len() as u32).to_be_bytes());
    for sz in sample_sizes {
        stsz_body.extend_from_slice(&sz.to_be_bytes());
    }
    let stsz = wrap_atom(b"stsz", &stsz_body);

    let stco = if sample_offset <= u64::from(u32::MAX) {
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&(sample_offset as u32).to_be_bytes());
        wrap_atom(b"stco", &body)
    } else {
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&sample_offset.to_be_bytes());
        wrap_atom(b"co64", &body)
    };

    Ok(wrap_container(b"stbl", &[stsd, stts, stsc, stsz, stco]))
}

fn identity_matrix() -> [u8; 36] {
    let mut m = [0u8; 36];
    m[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    m[16..20].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    m[32..36].copy_from_slice(&0x4000_0000u32.to_be_bytes());
    m
}

fn wrap_atom(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(body);
    out
}

fn wrap_container(fourcc: &[u8; 4], children: &[Vec<u8>]) -> Vec<u8> {
    let body_len: usize = children.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(8 + body_len);
    out.extend_from_slice(&((8 + body_len) as u32).to_be_bytes());
    out.extend_from_slice(fourcc);
    for child in children {
        out.extend_from_slice(child);
    }
    out
}

fn write_atom_header(buf: &mut Vec<u8>, size: u64, fourcc: [u8; 4]) -> Result<()> {
    if size > u64::from(u32::MAX) {
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(&fourcc);
        buf.extend_from_slice(&size.to_be_bytes());
    } else {
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(&fourcc);
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct AtomSpan {
    offset: usize,
    size: usize,
    header: usize,
}

/// Only the tests reach for a whole file now; the writer reads `moov` alone.
#[cfg(test)]
fn find_atom(data: &[u8], start: usize, end: usize, fourcc: [u8; 4]) -> Option<AtomSpan> {
    let mut pos = start;
    while pos + 8 <= end {
        let size32 = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?);
        let typ = <[u8; 4]>::try_from(&data[pos + 4..pos + 8]).ok()?;
        let (size, header) = if size32 == 1 {
            if pos + 16 > end {
                break;
            }
            let size64 = u64::from_be_bytes(data[pos + 8..pos + 16].try_into().ok()?) as usize;
            (size64, 16)
        } else if size32 == 0 {
            (end - pos, 8)
        } else {
            (size32 as usize, 8)
        };
        if size < header || pos + size > end {
            break;
        }
        if typ == fourcc {
            return Some(AtomSpan {
                offset: pos,
                size,
                header,
            });
        }
        pos += size;
    }
    None
}

fn iter_children(data: &[u8], parent: AtomSpan) -> Vec<AtomSpan> {
    let mut out = Vec::new();
    let mut pos = parent.offset + parent.header;
    let end = parent.offset + parent.size;
    while pos + 8 <= end {
        let size32 = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap());
        let (size, header) = if size32 == 1 {
            if pos + 16 > end {
                break;
            }
            let size64 = u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap()) as usize;
            (size64, 16)
        } else if size32 == 0 {
            (end - pos, 8)
        } else {
            (size32 as usize, 8)
        };
        if size < header || pos + size > end {
            break;
        }
        out.push(AtomSpan {
            offset: pos,
            size,
            header,
        });
        pos += size;
    }
    out
}

fn atom_type(data: &[u8], atom: AtomSpan) -> [u8; 4] {
    data[atom.offset + 4..atom.offset + 8]
        .try_into()
        .unwrap_or([0; 4])
}

fn set_atom_size(buf: &mut [u8], atom_offset: usize, new_size: usize) -> Result<()> {
    if new_size > u32::MAX as usize {
        return Err(MediaError::Native(
            "atom too large for 32-bit size field".into(),
        ));
    }
    buf[atom_offset..atom_offset + 4].copy_from_slice(&(new_size as u32).to_be_bytes());
    Ok(())
}

fn moov_span(moov: &[u8]) -> AtomSpan {
    AtomSpan {
        offset: 0,
        size: moov.len(),
        header: 8,
    }
}

fn read_mvhd_timing(moov: &[u8]) -> Result<(u32, u64)> {
    let mvhd = iter_children(moov, moov_span(moov))
        .into_iter()
        .find(|a| atom_type(moov, *a) == *b"mvhd")
        .ok_or_else(|| MediaError::Native("moov missing mvhd".into()))?;
    let p = mvhd.offset + mvhd.header;
    let version = moov[p];
    if version == 0 {
        let timescale = u32::from_be_bytes(moov[p + 12..p + 16].try_into().unwrap());
        let duration = u32::from_be_bytes(moov[p + 16..p + 20].try_into().unwrap()) as u64;
        Ok((timescale, duration))
    } else {
        let timescale = u32::from_be_bytes(moov[p + 20..p + 24].try_into().unwrap());
        let duration = u64::from_be_bytes(moov[p + 24..p + 32].try_into().unwrap());
        Ok((timescale, duration))
    }
}

fn read_tkhd_id(data: &[u8], tkhd: AtomSpan) -> u32 {
    let p = tkhd.offset + tkhd.header;
    if data[p] == 0 {
        u32::from_be_bytes(data[p + 12..p + 16].try_into().unwrap())
    } else {
        u32::from_be_bytes(data[p + 20..p + 24].try_into().unwrap())
    }
}

fn hdlr_type(data: &[u8], mdia: AtomSpan) -> Option<[u8; 4]> {
    let hdlr = iter_children(data, mdia)
        .into_iter()
        .find(|a| atom_type(data, *a) == *b"hdlr")?;
    let p = hdlr.offset + hdlr.header;
    data.get(p + 8..p + 12)?.try_into().ok()
}

fn first_audio_track_id(moov: &[u8]) -> Result<Option<u32>> {
    for trak in iter_children(moov, moov_span(moov))
        .into_iter()
        .filter(|a| atom_type(moov, *a) == *b"trak")
    {
        let kids = iter_children(moov, trak);
        let Some(tkhd) = kids
            .iter()
            .copied()
            .find(|a| atom_type(moov, *a) == *b"tkhd")
        else {
            continue;
        };
        let Some(mdia) = kids
            .iter()
            .copied()
            .find(|a| atom_type(moov, *a) == *b"mdia")
        else {
            continue;
        };
        if hdlr_type(moov, mdia) == Some(*b"soun") {
            return Ok(Some(read_tkhd_id(moov, tkhd)));
        }
    }
    Ok(None)
}

fn next_track_id(moov: &[u8]) -> u32 {
    let mut max_id = 0u32;
    for trak in iter_children(moov, moov_span(moov))
        .into_iter()
        .filter(|a| atom_type(moov, *a) == *b"trak")
    {
        if let Some(tkhd) = iter_children(moov, trak)
            .into_iter()
            .find(|a| atom_type(moov, *a) == *b"tkhd")
        {
            max_id = max_id.max(read_tkhd_id(moov, tkhd));
        }
    }
    max_id.saturating_add(1).max(2)
}

fn strip_existing_chapters(moov: &mut Vec<u8>) -> Result<()> {
    // Collect chapter track IDs referenced by chap, plus any text handler traks.
    let mut remove_ids = Vec::new();
    let children = iter_children(moov, moov_span(moov));
    for trak in children
        .iter()
        .copied()
        .filter(|a| atom_type(moov, *a) == *b"trak")
    {
        let kids = iter_children(moov, trak);
        if let Some(tref) = kids
            .iter()
            .copied()
            .find(|a| atom_type(moov, *a) == *b"tref")
        {
            if let Some(chap) = iter_children(moov, tref)
                .into_iter()
                .find(|a| atom_type(moov, *a) == *b"chap")
            {
                let payload = &moov[chap.offset + chap.header..chap.offset + chap.size];
                for chunk in payload.chunks_exact(4) {
                    remove_ids.push(u32::from_be_bytes(chunk.try_into().unwrap()));
                }
            }
        }
        if let Some(mdia) = kids
            .iter()
            .copied()
            .find(|a| atom_type(moov, *a) == *b"mdia")
        {
            if hdlr_type(moov, mdia) == Some(*b"text") {
                if let Some(tkhd) = kids
                    .iter()
                    .copied()
                    .find(|a| atom_type(moov, *a) == *b"tkhd")
                {
                    remove_ids.push(read_tkhd_id(moov, tkhd));
                }
            }
        }
    }
    remove_ids.sort_unstable();
    remove_ids.dedup();

    // Remove matching traks and chap trefs (rebuild moov children).
    let mut rebuilt = Vec::with_capacity(moov.len());
    rebuilt.extend_from_slice(&moov[..8]); // moov header placeholder
    for child in iter_children(moov, moov_span(moov)) {
        let typ = atom_type(moov, child);
        if typ == *b"trak" {
            let kids = iter_children(moov, child);
            let id = kids
                .iter()
                .copied()
                .find(|a| atom_type(moov, *a) == *b"tkhd")
                .map(|tkhd| read_tkhd_id(moov, tkhd));
            if id.is_some_and(|id| remove_ids.contains(&id)) {
                continue;
            }
            // Strip tref/chap from remaining tracks.
            let mut trak_out = Vec::with_capacity(child.size);
            trak_out.extend_from_slice(&moov[child.offset..child.offset + 8]);
            for k in kids {
                let ktyp = atom_type(moov, k);
                if ktyp == *b"tref" {
                    // Drop entire tref when it only held chap; otherwise drop chap child.
                    let tref_kids = iter_children(moov, k);
                    let only_chap = tref_kids.len() == 1
                        && tref_kids
                            .first()
                            .is_some_and(|c| atom_type(moov, *c) == *b"chap");
                    if only_chap {
                        continue;
                    }
                    let mut tref_out = Vec::new();
                    tref_out.extend_from_slice(&moov[k.offset..k.offset + 8]);
                    for tk in tref_kids {
                        if atom_type(moov, tk) == *b"chap" {
                            continue;
                        }
                        tref_out.extend_from_slice(&moov[tk.offset..tk.offset + tk.size]);
                    }
                    if tref_out.len() > 8 {
                        let tref_out_len = tref_out.len();
                        set_atom_size(&mut tref_out, 0, tref_out_len)?;
                        trak_out.extend_from_slice(&tref_out);
                    }
                    continue;
                }
                trak_out.extend_from_slice(&moov[k.offset..k.offset + k.size]);
            }
            let trak_out_len = trak_out.len();
            set_atom_size(&mut trak_out, 0, trak_out_len)?;
            rebuilt.extend_from_slice(&trak_out);
            continue;
        }
        if typ == *b"udta" {
            let mut udta_out = Vec::with_capacity(child.size);
            udta_out.extend_from_slice(&moov[child.offset..child.offset + 8]);
            for k in iter_children(moov, child) {
                if atom_type(moov, k) == *b"chpl" {
                    continue;
                }
                udta_out.extend_from_slice(&moov[k.offset..k.offset + k.size]);
            }
            let udta_out_len = udta_out.len();
            set_atom_size(&mut udta_out, 0, udta_out_len)?;
            rebuilt.extend_from_slice(&udta_out);
            continue;
        }
        rebuilt.extend_from_slice(&moov[child.offset..child.offset + child.size]);
    }
    let rebuilt_len = rebuilt.len();
    set_atom_size(&mut rebuilt, 0, rebuilt_len)?;
    *moov = rebuilt;
    Ok(())
}

fn insert_chapter_trak(moov: &mut Vec<u8>, chapter_trak: &[u8]) -> Result<()> {
    // Insert before udta when present, else append.
    let children = iter_children(moov, moov_span(moov));
    let insert_at = children
        .iter()
        .find(|a| atom_type(moov, **a) == *b"udta")
        .map(|a| a.offset)
        .unwrap_or(moov.len());
    moov.splice(insert_at..insert_at, chapter_trak.iter().copied());
    let moov_len = moov.len();
    set_atom_size(moov, 0, moov_len)?;
    Ok(())
}

fn set_audio_chap_ref(
    moov: &mut Vec<u8>,
    audio_track_id: u32,
    chapter_track_id: u32,
) -> Result<()> {
    let children = iter_children(moov, moov_span(moov));
    let Some(audio_trak) = children.into_iter().find(|trak| {
        atom_type(moov, *trak) == *b"trak"
            && iter_children(moov, *trak)
                .into_iter()
                .find(|a| atom_type(moov, *a) == *b"tkhd")
                .is_some_and(|tkhd| read_tkhd_id(moov, tkhd) == audio_track_id)
    }) else {
        return Err(MediaError::Native(
            "audio track not found for chap reference".into(),
        ));
    };

    let tref = {
        let mut chap = Vec::new();
        chap.extend_from_slice(&chapter_track_id.to_be_bytes());
        wrap_container(b"tref", &[wrap_atom(b"chap", &chap)])
    };

    // Insert tref after tkhd (QuickTime order).
    let kids = iter_children(moov, audio_trak);
    if kids.iter().all(|a| atom_type(moov, *a) != *b"tkhd") {
        return Err(MediaError::Native("audio trak missing tkhd".into()));
    }
    // Remove any existing tref first (should already be stripped).
    let mut cleaned = Vec::with_capacity(audio_trak.size + tref.len());
    cleaned.extend_from_slice(&moov[audio_trak.offset..audio_trak.offset + 8]);
    for k in kids {
        if atom_type(moov, k) == *b"tref" {
            continue;
        }
        cleaned.extend_from_slice(&moov[k.offset..k.offset + k.size]);
    }
    // Recompute insert relative to cleaned: after tkhd.
    let cleaned_tkhd_end = {
        let span = AtomSpan {
            offset: 0,
            size: cleaned.len(),
            header: 8,
        };
        let tk = iter_children(&cleaned, span)
            .into_iter()
            .find(|a| atom_type(&cleaned, *a) == *b"tkhd")
            .ok_or_else(|| MediaError::Native("cleaned audio trak missing tkhd".into()))?;
        tk.offset + tk.size
    };
    cleaned.splice(cleaned_tkhd_end..cleaned_tkhd_end, tref.iter().copied());
    let cleaned_len = cleaned.len();
    set_atom_size(&mut cleaned, 0, cleaned_len)?;

    moov.splice(
        audio_trak.offset..audio_trak.offset + audio_trak.size,
        cleaned.iter().copied(),
    );
    let moov_len = moov.len();
    set_atom_size(moov, 0, moov_len)?;
    Ok(())
}

fn upsert_nero_chpl(moov: &mut Vec<u8>, chapters: &[(String, u64)]) -> Result<()> {
    let mut chpl_body = Vec::new();
    chpl_body.extend_from_slice(&0u32.to_be_bytes()); // ver/flags
    chpl_body.push(chapters.len().min(255) as u8);
    for (title, start_ms) in chapters.iter().take(255) {
        let start = (*start_ms as u128) * (CHPL_TIMESCALE as u128) / 1000;
        chpl_body.extend_from_slice(&(start as u64).to_be_bytes());
        let title_bytes = title.as_bytes();
        let len = title_bytes.len().min(255);
        chpl_body.push(len as u8);
        chpl_body.extend_from_slice(&title_bytes[..len]);
    }
    let chpl = wrap_atom(b"chpl", &chpl_body);

    let children = iter_children(moov, moov_span(moov));
    if let Some(udta) = children
        .iter()
        .copied()
        .find(|a| atom_type(moov, *a) == *b"udta")
    {
        let mut udta_out = Vec::with_capacity(udta.size + chpl.len());
        udta_out.extend_from_slice(&moov[udta.offset..udta.offset + 8]);
        for k in iter_children(moov, udta) {
            if atom_type(moov, k) == *b"chpl" {
                continue;
            }
            udta_out.extend_from_slice(&moov[k.offset..k.offset + k.size]);
        }
        udta_out.extend_from_slice(&chpl);
        let udta_out_len = udta_out.len();
        set_atom_size(&mut udta_out, 0, udta_out_len)?;
        moov.splice(
            udta.offset..udta.offset + udta.size,
            udta_out.iter().copied(),
        );
    } else {
        let udta = wrap_container(b"udta", &[chpl]);
        moov.extend_from_slice(&udta);
    }
    let moov_len = moov.len();
    set_atom_size(moov, 0, moov_len)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{fixup_audiobook, FixupRequest};
    use crate::package_m4b::package_m4b_from_pcm;

    fn chapter_track_info(path: &Path) -> (u32, u32, u32, usize) {
        let data = std::fs::read(path).unwrap();
        let moov = find_atom(&data, 0, data.len(), *b"moov").unwrap();
        let moov_buf = &data[moov.offset..moov.offset + moov.size];
        for trak in iter_children(moov_buf, moov_span(moov_buf))
            .into_iter()
            .filter(|a| atom_type(moov_buf, *a) == *b"trak")
        {
            let kids = iter_children(moov_buf, trak);
            let Some(mdia) = kids
                .iter()
                .copied()
                .find(|a| atom_type(moov_buf, *a) == *b"mdia")
            else {
                continue;
            };
            if hdlr_type(moov_buf, mdia) != Some(*b"text") {
                continue;
            }
            let tkhd = kids
                .iter()
                .copied()
                .find(|a| atom_type(moov_buf, *a) == *b"tkhd")
                .unwrap();
            let flags = u32::from_be_bytes([
                0,
                moov_buf[tkhd.offset + tkhd.header + 1],
                moov_buf[tkhd.offset + tkhd.header + 2],
                moov_buf[tkhd.offset + tkhd.header + 3],
            ]);
            let mdhd = iter_children(moov_buf, mdia)
                .into_iter()
                .find(|a| atom_type(moov_buf, *a) == *b"mdhd")
                .unwrap();
            let ts = u32::from_be_bytes(
                moov_buf[mdhd.offset + mdhd.header + 12..mdhd.offset + mdhd.header + 16]
                    .try_into()
                    .unwrap(),
            );
            assert!(
                kids.iter().any(|a| atom_type(moov_buf, *a) == *b"edts"),
                "missing edts"
            );
            let minf = iter_children(moov_buf, mdia)
                .into_iter()
                .find(|a| atom_type(moov_buf, *a) == *b"minf")
                .unwrap();
            let stbl = iter_children(moov_buf, minf)
                .into_iter()
                .find(|a| atom_type(moov_buf, *a) == *b"stbl")
                .unwrap();
            let stsd = iter_children(moov_buf, stbl)
                .into_iter()
                .find(|a| atom_type(moov_buf, *a) == *b"stsd")
                .unwrap();
            let entries_at = stsd.offset + stsd.header + 8;
            let entry_size =
                u32::from_be_bytes(moov_buf[entries_at..entries_at + 4].try_into().unwrap())
                    as usize;
            return (flags, ts, entry_size as u32, entry_size);
        }
        panic!("no text chapter track");
    }

    #[tokio::test]
    async fn writes_ffmpeg_like_chapter_track_once() {
        // This exercises the chapter muxer, not the jail, and unit tests have
        // no worker binary to spawn. Without this the default pool refuses the
        // job, which is the behaviour production wants.
        crate::init_pool(crate::MediaPool::in_process()).ok();

        let sample_rate = 16_000u32;
        let pcm = vec![0i16; sample_rate as usize * 2];
        let dir = tempfile::tempdir().unwrap();
        let packaged = dir.path().join("raw.m4b");
        package_m4b_from_pcm(&pcm, sample_rate, 1, &packaged, &[]).unwrap();

        let fixed = dir.path().join("fixed.m4b");
        fixup_audiobook(FixupRequest {
            input: packaged,
            output: fixed.clone(),
            title: "Test Book".into(),
            author: Some("Author".into()),
            narrator: None,
            cover: None,
            chapters: vec![("Opening".into(), 0), ("Chapter 1".into(), 500)],
            replace_chapters: true,
            subtitle: None,
            publisher: None,
            year: None,
            genre: None,
            series: None,
            series_index: None,
            asin: None,
            isbn: None,
            description: None,
            language: None,
            tool: None,
        })
        .await
        .unwrap();

        let (flags, timescale, entry_size_u32, entry_size) = chapter_track_info(&fixed);
        assert_eq!(flags, 0x2, "TrackInMovie like ffmpeg");
        assert_eq!(timescale, 1000);
        assert_eq!(entry_size_u32, TEXT_SAMPLE_ENTRY.len() as u32);
        assert_eq!(entry_size, TEXT_SAMPLE_ENTRY.len());

        // Idempotent rewrite should still succeed.
        write_audiobook_chapters(&fixed, &[("Opening".into(), 0), ("Chapter 1".into(), 500)])
            .unwrap();
        let (flags2, ts2, _, _) = chapter_track_info(&fixed);
        assert_eq!(flags2, 0x2);
        assert_eq!(ts2, 1000);
    }

    #[test]
    fn sample_durations_cover_book() {
        let chapters = [("A".into(), 0u64), ("B".into(), 1500u64)];
        let (_payload, sizes, deltas) = build_chapter_samples(&chapters, 3000).unwrap();
        assert_eq!(sizes.len(), 2);
        assert_eq!(deltas, vec![1500, 1500]);
        assert!(sizes.iter().all(|&s| s >= 2 + ENCD.len() as u32));
    }
}
