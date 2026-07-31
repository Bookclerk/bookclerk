//! Native Widevine / DASH fragmented-MP4 CENC decrypt → progressive M4B.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::boxutil::{
    find_child, read_box_header, read_fourcc, read_full_box_version_flags, read_u32, read_u64,
    read_u8, walk_children, BoxHeader, FourCC, DASH, ENCA, FTYP, HDLR, MDAT, MDHD, MDIA, MINF,
    MOOF, MOOV, MVEX, MVHD, SCHI, SCHM, SENC, SIDX, SINF, STBL, STSD, TENC, TFHD, TRAF, TRAK, TRUN,
};
use super::remux::{
    find_box_range, find_direct_child, splice_replace, write_progressive_m4b, ProgressiveWriteInput,
};
use super::TrimRange;
use crate::drm::crypto::{decrypt_cenc_sample_in_place, expand_cenc_iv, parse_aes128_hex};
use crate::drm::error::{DrmError, Result};
use crate::drm::DecryptOutcome;

/// True when the file looks like a fragmented DASH / CENC MP4 (`dash` brand, or top-level `sidx`/`moof`).
pub fn looks_like_dash(path: &Path) -> Result<bool> {
    let mut file = File::open(path)?;
    let file_size = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;
    let mut pos = 0u64;
    let mut saw_dash_brand = false;
    let mut saw_sidx = false;
    let mut saw_moof = false;
    while pos + 8 <= file_size {
        file.seek(SeekFrom::Start(pos))?;
        let header = read_box_header(&mut file)?;
        match header.kind {
            FTYP => {
                let (major, brands) = parse_ftyp_brands(&mut file, &header)?;
                saw_dash_brand = major == DASH || brands.contains(&DASH);
            }
            SIDX => saw_sidx = true,
            MOOF => saw_moof = true,
            _ => {}
        }
        if header.size == 0 {
            break;
        }
        pos = header.end();
        if saw_moof && (saw_sidx || saw_dash_brand) {
            return Ok(true);
        }
    }
    Ok(saw_moof || (saw_dash_brand && saw_sidx))
}

/// Decrypt an Audible DASH / CENC fragmented MP4 into a progressive DRM-free M4B.
pub fn decrypt_dash_cenc(
    input: &Path,
    output: &Path,
    kid_hex: &str,
    key_hex: &str,
    trim: Option<TrimRange>,
) -> Result<DecryptOutcome> {
    if !input.exists() {
        return Err(DrmError::InputMissing(input.to_path_buf()));
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let kid = parse_aes128_hex(kid_hex)?;
    let key = parse_aes128_hex(key_hex)?;

    let dash = parse_dash_file(input)?;
    if let Some(default_kid) = dash.default_kid {
        if default_kid != kid {
            return Err(DrmError::InvalidKey(format!(
                "supplied kid does not match tenc DefaultKID (got {}, expected {})",
                hex::encode(kid),
                hex::encode(default_kid)
            )));
        }
    }

    tracing::info!(
        input = %input.display(),
        output = %output.display(),
        fragments = dash.fragment_count_hint,
        trim = trim.is_some(),
        "native DASH/CENC decrypt"
    );

    let mut src = File::open(input)?;
    let plan = collect_sample_plan(&mut src, &dash, &key)?;
    let selected = if let Some(trim) = trim {
        filter_sample_plan_by_ms(&plan, dash.timescale, trim.start_ms, trim.end_ms)
    } else {
        plan
    };
    if selected.is_empty() {
        return Err(DrmError::Mp4("no DASH samples remain after trim".into()));
    }

    let sample_sizes: Vec<u32> = selected.iter().map(|s| s.size).collect();
    let durations: Vec<u32> = selected.iter().map(|s| s.duration).collect();
    let moov = patch_dash_moov(&dash.moov_bytes)?;
    let mut sample_src = File::open(input)?;

    write_progressive_m4b(
        output,
        ProgressiveWriteInput {
            moov_bytes: &moov,
            // `moov` is a standalone box buffer; offsets are relative to byte 0.
            moov_file_start: 0,
            sample_entry_type_offset: dash.sample_entry_type_rel,
            audio_timescale: dash.timescale,
            mvhd_timescale: dash.mvhd_timescale,
            sample_sizes: &sample_sizes,
            durations: &durations,
            rewrite_ftyp: true,
        },
        |i, buf| {
            let sample = &selected[i];
            buf.resize(sample.size as usize, 0);
            sample_src.seek(SeekFrom::Start(sample.offset))?;
            sample_src.read_exact(buf)?;
            let iv = sample.iv.ok_or_else(|| {
                DrmError::Mp4(format!(
                    "DASH sample {i} (offset {}) missing CENC IV — refusing to copy encrypted bytes",
                    sample.offset
                ))
            })?;
            decrypt_cenc_sample_in_place(&key, &iv, buf);
            Ok(())
        },
    )?;

    if !output.exists() {
        return Err(DrmError::OutputMissing(output.to_path_buf()));
    }
    Ok(DecryptOutcome {
        output: output.to_path_buf(),
    })
}

#[derive(Debug, Clone)]
struct SamplePlanEntry {
    start_cts: u64,
    duration: u32,
    size: u32,
    /// Absolute file offset of the encrypted sample payload.
    offset: u64,
    iv: Option<[u8; 16]>,
}

#[derive(Debug)]
struct DashFileInfo {
    moov_bytes: Vec<u8>,
    /// Sample-entry type offset relative to start of `moov_bytes`.
    sample_entry_type_rel: u64,
    timescale: u32,
    mvhd_timescale: u32,
    default_kid: Option<[u8; 16]>,
    first_moof: BoxHeader,
    first_mdat: BoxHeader,
    /// End of media data region (`first_moof.start + Σ sidx.reference_size`), or EOF.
    media_end: u64,
    fragment_count_hint: usize,
    trex_default_duration: Option<u32>,
    trex_default_size: Option<u32>,
}

fn parse_dash_file(path: &Path) -> Result<DashFileInfo> {
    let mut file = File::open(path)?;
    let file_size = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;

    let mut ftyp = None;
    let mut moov = None;
    let mut sidx = None;
    let mut first_moof = None;
    let mut first_mdat = None;
    let mut pos = 0u64;
    while pos + 8 <= file_size {
        file.seek(SeekFrom::Start(pos))?;
        let header = read_box_header(&mut file)?;
        match header.kind {
            FTYP if ftyp.is_none() => ftyp = Some(header.clone()),
            MOOV if moov.is_none() => moov = Some(header.clone()),
            SIDX if sidx.is_none() => sidx = Some(header.clone()),
            MOOF if first_moof.is_none() => first_moof = Some(header.clone()),
            MDAT if first_mdat.is_none() && first_moof.is_some() => {
                first_mdat = Some(header.clone());
            }
            _ => {}
        }
        if header.size == 0 {
            break;
        }
        pos = header.end();
    }

    let _ftyp = ftyp.ok_or_else(|| DrmError::Mp4("DASH: missing ftyp".into()))?;
    let moov = moov.ok_or_else(|| DrmError::Mp4("DASH: missing moov".into()))?;
    let first_moof = first_moof.ok_or_else(|| DrmError::Mp4("DASH: missing moof".into()))?;
    let first_mdat = first_mdat.ok_or_else(|| DrmError::Mp4("DASH: missing mdat".into()))?;

    let (mvhd_timescale, _) = parse_mvhd(&mut file, &moov)?;
    let audio = parse_dash_audio_track(&mut file, &moov)?;

    let mut moov_bytes = vec![
        0u8;
        usize::try_from(moov.size).map_err(|_| {
            DrmError::Mp4(format!("moov too large: {}", moov.size))
        })?
    ];
    file.seek(SeekFrom::Start(moov.start))?;
    file.read_exact(&mut moov_bytes)?;

    let sample_entry_type_rel = audio
        .sample_entry_type_abs
        .checked_sub(moov.start)
        .ok_or_else(|| DrmError::Mp4("sample entry outside moov".into()))?;

    let (media_end, fragment_count_hint) = if let Some(sidx_hdr) = &sidx {
        let segs = parse_sidx_segments(&mut file, sidx_hdr)?;
        let total: u64 = segs.iter().map(|s| u64::from(s.reference_size)).sum();
        (first_moof.start.saturating_add(total), segs.len())
    } else {
        (file_size, 0)
    };

    Ok(DashFileInfo {
        moov_bytes,
        sample_entry_type_rel,
        timescale: audio.timescale,
        mvhd_timescale,
        default_kid: audio.default_kid,
        first_moof,
        first_mdat,
        media_end,
        fragment_count_hint,
        trex_default_duration: audio.trex_default_duration,
        trex_default_size: audio.trex_default_size,
    })
}

#[derive(Debug)]
struct SidxSegment {
    reference_size: u32,
}

fn parse_sidx_segments(file: &mut File, sidx: &BoxHeader) -> Result<Vec<SidxSegment>> {
    file.seek(SeekFrom::Start(sidx.content_start()))?;
    let (version, _) = read_full_box_version_flags(file)?;
    let _reference_id = read_u32(file)?;
    let _timescale = read_u32(file)?;
    if version == 0 {
        let _earliest = read_u32(file)?;
        let _first_offset = read_u32(file)?;
    } else {
        let _earliest = read_u64(file)?;
        let _first_offset = read_u64(file)?;
    }
    let mut reserved = [0u8; 2];
    file.read_exact(&mut reserved)?;
    let mut count_buf = [0u8; 2];
    file.read_exact(&mut count_buf)?;
    let reference_count = u16::from_be_bytes(count_buf);

    let mut segs = Vec::with_capacity(reference_count as usize);
    for _ in 0..reference_count {
        let type_and_size = read_u32(file)?;
        let _subsegment_duration = read_u32(file)?;
        let _sap = read_u32(file)?;
        let reference_type = (type_and_size & 0x8000_0000) != 0;
        let reference_size = type_and_size & 0x7fff_ffff;
        if reference_type {
            return Err(DrmError::Mp4(
                "DASH sidx reference_type=1 is not supported".into(),
            ));
        }
        segs.push(SidxSegment { reference_size });
    }
    Ok(segs)
}

fn parse_ftyp_brands(file: &mut File, ftyp: &BoxHeader) -> Result<(FourCC, Vec<FourCC>)> {
    file.seek(SeekFrom::Start(ftyp.content_start()))?;
    let major = read_fourcc(file)?;
    let _minor = read_u32(file)?;
    let mut brands = Vec::new();
    let end = ftyp.end();
    while file.stream_position()? + 4 <= end {
        brands.push(read_fourcc(file)?);
    }
    Ok((major, brands))
}

fn parse_mvhd(file: &mut File, moov: &BoxHeader) -> Result<(u32, u64)> {
    let mvhd = find_child(file, moov.content_start(), moov.end(), MVHD)?
        .ok_or_else(|| DrmError::Mp4("missing mvhd".into()))?;
    file.seek(SeekFrom::Start(mvhd.content_start()))?;
    let (version, _) = read_full_box_version_flags(file)?;
    if version == 1 {
        let _c = read_u64(file)?;
        let _m = read_u64(file)?;
        Ok((read_u32(file)?, read_u64(file)?))
    } else {
        let _c = read_u32(file)?;
        let _m = read_u32(file)?;
        Ok((read_u32(file)?, u64::from(read_u32(file)?)))
    }
}

fn parse_dash_audio_track(file: &mut File, moov: &BoxHeader) -> Result<DashAudioMeta> {
    let mut result = None;
    walk_children(file, moov.content_start(), moov.end(), |file, header| {
        if header.kind != TRAK || result.is_some() {
            return Ok(());
        }
        if let Some(v) = try_parse_dash_audio_trak(file, header)? {
            result = Some(v);
        }
        Ok(())
    })?;

    let trex = parse_trex_defaults(file, moov)?;
    let track = result.ok_or_else(|| DrmError::Mp4("DASH: no audio track".into()))?;
    Ok(DashAudioMeta {
        timescale: track.timescale,
        sample_entry_type_abs: track.sample_entry_type_abs,
        default_kid: track.default_kid,
        trex_default_duration: trex.0,
        trex_default_size: trex.1,
    })
}

struct DashAudioMeta {
    timescale: u32,
    sample_entry_type_abs: u64,
    default_kid: Option<[u8; 16]>,
    trex_default_duration: Option<u32>,
    trex_default_size: Option<u32>,
}

struct DashTrackMeta {
    timescale: u32,
    sample_entry_type_abs: u64,
    default_kid: Option<[u8; 16]>,
}

fn parse_trex_defaults(file: &mut File, moov: &BoxHeader) -> Result<(Option<u32>, Option<u32>)> {
    let Some(mvex) = find_child(file, moov.content_start(), moov.end(), MVEX)? else {
        return Ok((None, None));
    };
    let mut duration = None;
    let mut size = None;
    walk_children(file, mvex.content_start(), mvex.end(), |file, header| {
        if header.kind.0 != *b"trex" {
            return Ok(());
        }
        file.seek(SeekFrom::Start(header.content_start()))?;
        let (_v, _) = read_full_box_version_flags(file)?;
        let _track_id = read_u32(file)?;
        let _desc = read_u32(file)?;
        duration = Some(read_u32(file)?);
        size = Some(read_u32(file)?);
        let _flags = read_u32(file)?;
        Ok(())
    })?;
    Ok((duration, size))
}

fn try_parse_dash_audio_trak(file: &mut File, trak: &BoxHeader) -> Result<Option<DashTrackMeta>> {
    let mdia = match find_child(file, trak.content_start(), trak.end(), MDIA)? {
        Some(b) => b,
        None => return Ok(None),
    };
    let hdlr = match find_child(file, mdia.content_start(), mdia.end(), HDLR)? {
        Some(b) => b,
        None => return Ok(None),
    };
    file.seek(SeekFrom::Start(hdlr.content_start()))?;
    let (_version, _) = read_full_box_version_flags(file)?;
    let _pre = read_u32(file)?;
    let handler = read_fourcc(file)?;
    if handler.0 != *b"soun" {
        return Ok(None);
    }

    let mdhd = find_child(file, mdia.content_start(), mdia.end(), MDHD)?
        .ok_or_else(|| DrmError::Mp4("audio track missing mdhd".into()))?;
    file.seek(SeekFrom::Start(mdhd.content_start()))?;
    let (version, _) = read_full_box_version_flags(file)?;
    let timescale = if version == 1 {
        let _c = read_u64(file)?;
        let _m = read_u64(file)?;
        read_u32(file)?
    } else {
        let _c = read_u32(file)?;
        let _m = read_u32(file)?;
        read_u32(file)?
    };
    let _duration = if version == 1 {
        read_u64(file)?
    } else {
        u64::from(read_u32(file)?)
    };

    let minf = find_child(file, mdia.content_start(), mdia.end(), MINF)?
        .ok_or_else(|| DrmError::Mp4("audio track missing minf".into()))?;
    let stbl = find_child(file, minf.content_start(), minf.end(), STBL)?
        .ok_or_else(|| DrmError::Mp4("audio track missing stbl".into()))?;
    let stsd = find_child(file, stbl.content_start(), stbl.end(), STSD)?
        .ok_or_else(|| DrmError::Mp4("missing stsd".into()))?;

    file.seek(SeekFrom::Start(stsd.content_start()))?;
    let (_v, _) = read_full_box_version_flags(file)?;
    let entry_count = read_u32(file)?;
    if entry_count == 0 {
        return Err(DrmError::Mp4("stsd has no sample entries".into()));
    }
    let entry_start = file.stream_position()?;
    let entry_size = u64::from(read_u32(file)?);
    let type_offset = file.stream_position()?;
    let entry_type = read_fourcc(file)?;
    let entry_end = entry_start + entry_size;

    let kid = if entry_type == ENCA {
        parse_tenc_kid_from_entry(file, type_offset + 4, entry_end)?
    } else {
        None
    };

    Ok(Some(DashTrackMeta {
        timescale,
        sample_entry_type_abs: type_offset,
        default_kid: kid,
    }))
}

fn parse_tenc_kid_from_entry(
    file: &mut File,
    after_type: u64,
    entry_end: u64,
) -> Result<Option<[u8; 16]>> {
    let children_start = after_type + 28;
    let Some(sinf) = find_child(file, children_start, entry_end, SINF)? else {
        return Ok(None);
    };
    if let Some(schm) = find_child(file, sinf.content_start(), sinf.end(), SCHM)? {
        file.seek(SeekFrom::Start(schm.content_start()))?;
        let (_v, _) = read_full_box_version_flags(file)?;
        let scheme = read_fourcc(file)?;
        if scheme.0 != *b"cenc" {
            return Err(DrmError::Mp4(format!(
                "unsupported DRM scheme {scheme}; only cenc is supported"
            )));
        }
    }
    let schi = find_child(file, sinf.content_start(), sinf.end(), SCHI)?
        .ok_or_else(|| DrmError::Mp4("sinf missing schi".into()))?;
    let tenc = find_child(file, schi.content_start(), schi.end(), TENC)?
        .ok_or_else(|| DrmError::Mp4("schi missing tenc".into()))?;
    Ok(Some(parse_tenc_default_kid(file, &tenc)?))
}

fn parse_tenc_default_kid(file: &mut (impl Read + Seek), tenc: &BoxHeader) -> Result<[u8; 16]> {
    file.seek(SeekFrom::Start(tenc.content_start()))?;
    let (_version, _) = read_full_box_version_flags(file)?;
    let _reserved0 = read_u8(file)?;
    let _reserved1_or_pattern = read_u8(file)?;
    let _is_protected = read_u8(file)?;
    let _per_sample_iv_size = read_u8(file)?;
    let mut kid = [0u8; 16];
    file.read_exact(&mut kid)?;
    Ok(kid)
}

fn collect_sample_plan(
    src: &mut File,
    dash: &DashFileInfo,
    _key: &[u8; 16],
) -> Result<Vec<SamplePlanEntry>> {
    let mut out = Vec::new();
    let mut cts = 0u64;

    let mut moof = dash.first_moof.clone();
    let mut mdat = dash.first_mdat.clone();
    loop {
        let fragment = parse_fragment(src, &moof, dash)?;
        let sizes_sum: u64 = fragment.sizes.iter().map(|s| u64::from(*s)).sum();
        if sizes_sum != mdat.content_len() {
            return Err(DrmError::Mp4(format!(
                "mdat size {} does not match trun sample sizes {sizes_sum}",
                mdat.content_len()
            )));
        }
        let ivs = fragment.ivs.as_ref().ok_or_else(|| {
            DrmError::Mp4("DASH fragment missing senc IVs — cannot decrypt CENC samples".into())
        })?;
        if ivs.len() != fragment.sizes.len() {
            return Err(DrmError::Mp4(format!(
                "senc IV count {} != sample count {}",
                ivs.len(),
                fragment.sizes.len()
            )));
        }

        let mut offset = mdat.content_start();
        for (i, &size) in fragment.sizes.iter().enumerate() {
            let iv = normalize_cenc_iv(&ivs[i])?;
            let duration = fragment.durations[i];
            out.push(SamplePlanEntry {
                start_cts: cts,
                duration,
                size,
                offset,
                iv: Some(iv),
            });
            offset = offset.saturating_add(u64::from(size));
            cts = cts.saturating_add(u64::from(duration));
        }

        let next_pos = mdat.end();
        if next_pos + 8 > dash.media_end {
            break;
        }
        src.seek(SeekFrom::Start(next_pos))?;
        moof = read_box_header(src)?;
        if moof.kind != MOOF {
            break;
        }
        src.seek(SeekFrom::Start(moof.end()))?;
        mdat = read_box_header(src)?;
        if mdat.kind != MDAT {
            return Err(DrmError::Mp4(format!(
                "expected mdat after moof, found {}",
                mdat.kind
            )));
        }
    }
    Ok(out)
}

struct FragmentInfo {
    sizes: Vec<u32>,
    durations: Vec<u32>,
    ivs: Option<Vec<Vec<u8>>>,
}

fn parse_fragment(file: &mut File, moof: &BoxHeader, dash: &DashFileInfo) -> Result<FragmentInfo> {
    let traf = find_child(file, moof.content_start(), moof.end(), TRAF)?
        .ok_or_else(|| DrmError::Mp4("moof missing traf".into()))?;
    let tfhd = find_child(file, traf.content_start(), traf.end(), TFHD)?
        .ok_or_else(|| DrmError::Mp4("traf missing tfhd".into()))?;
    let (default_duration, default_size) = parse_tfhd_defaults(file, &tfhd)?;
    let default_duration = default_duration.or(dash.trex_default_duration);
    let default_size = default_size.or(dash.trex_default_size);

    let trun = find_child(file, traf.content_start(), traf.end(), TRUN)?
        .ok_or_else(|| DrmError::Mp4("traf missing trun".into()))?;
    let (sizes, durations) = parse_trun(file, &trun, default_duration, default_size)?;

    let ivs = if let Some(senc) = find_child(file, traf.content_start(), traf.end(), SENC)? {
        Some(parse_senc_ivs(file, &senc, sizes.len())?)
    } else {
        None
    };

    Ok(FragmentInfo {
        sizes,
        durations,
        ivs,
    })
}

fn parse_tfhd_defaults(file: &mut File, tfhd: &BoxHeader) -> Result<(Option<u32>, Option<u32>)> {
    file.seek(SeekFrom::Start(tfhd.content_start()))?;
    let (_version, flags) = read_full_box_version_flags(file)?;
    let _track_id = read_u32(file)?;
    if flags & 0x000001 != 0 {
        let _base = read_u64(file)?;
    }
    if flags & 0x000002 != 0 {
        let _desc = read_u32(file)?;
    }
    let default_duration = if flags & 0x000008 != 0 {
        Some(read_u32(file)?)
    } else {
        None
    };
    let default_size = if flags & 0x000010 != 0 {
        Some(read_u32(file)?)
    } else {
        None
    };
    Ok((default_duration, default_size))
}

fn parse_trun(
    file: &mut (impl Read + Seek),
    trun: &BoxHeader,
    default_duration: Option<u32>,
    default_size: Option<u32>,
) -> Result<(Vec<u32>, Vec<u32>)> {
    file.seek(SeekFrom::Start(trun.content_start()))?;
    let (_version, flags) = read_full_box_version_flags(file)?;
    let sample_count = read_u32(file)? as usize;
    if flags & 0x000001 != 0 {
        let _data_offset = read_u32(file)?;
    }
    if flags & 0x000004 != 0 {
        let _first_sample_flags = read_u32(file)?;
    }
    let has_duration = flags & 0x000100 != 0;
    let has_size = flags & 0x000200 != 0;
    let has_flags = flags & 0x000400 != 0;
    let has_cts = flags & 0x000800 != 0;

    let mut sizes = Vec::with_capacity(sample_count);
    let mut durations = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        let duration = if has_duration {
            read_u32(file)?
        } else {
            default_duration.ok_or_else(|| {
                DrmError::Mp4("trun missing sample durations and no tfhd/trex default".into())
            })?
        };
        let size = if has_size {
            read_u32(file)?
        } else {
            default_size.ok_or_else(|| {
                DrmError::Mp4("trun missing sample sizes and no tfhd/trex default".into())
            })?
        };
        if has_flags {
            let _ = read_u32(file)?;
        }
        if has_cts {
            let _ = read_u32(file)?;
        }
        sizes.push(size);
        durations.push(duration);
    }
    Ok((sizes, durations))
}

fn parse_senc_ivs(
    file: &mut (impl Read + Seek),
    senc: &BoxHeader,
    expect_count: usize,
) -> Result<Vec<Vec<u8>>> {
    file.seek(SeekFrom::Start(senc.content_start()))?;
    let (_version, flags) = read_full_box_version_flags(file)?;
    if flags & 0x000002 != 0 {
        return Err(DrmError::Mp4(
            "senc subsample encryption is not supported".into(),
        ));
    }
    let sample_count = read_u32(file)? as usize;
    if sample_count != expect_count {
        return Err(DrmError::Mp4(format!(
            "senc sample_count {sample_count} != trun count {expect_count}"
        )));
    }
    let pos = file.stream_position()?;
    if pos > senc.end() {
        return Err(DrmError::Mp4("senc underflow".into()));
    }
    let remaining = senc.end() - pos;
    if sample_count == 0 {
        return Ok(Vec::new());
    }
    if !remaining.is_multiple_of(sample_count as u64) {
        return Err(DrmError::Mp4(format!(
            "senc IV region {remaining} not divisible by sample_count {sample_count}"
        )));
    }
    let iv_size = (remaining / sample_count as u64) as usize;
    if iv_size != 8 && iv_size != 16 {
        return Err(DrmError::Mp4(format!(
            "unsupported senc IV size {iv_size} (want 8 or 16)"
        )));
    }
    let mut ivs = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        let mut iv = vec![0u8; iv_size];
        file.read_exact(&mut iv)?;
        ivs.push(iv);
    }
    Ok(ivs)
}

fn normalize_cenc_iv(iv: &[u8]) -> Result<[u8; 16]> {
    match iv.len() {
        16 => {
            let mut out = [0u8; 16];
            out.copy_from_slice(iv);
            Ok(out)
        }
        8 => {
            let mut iv8 = [0u8; 8];
            iv8.copy_from_slice(iv);
            Ok(expand_cenc_iv(&iv8))
        }
        n => Err(DrmError::Mp4(format!("unexpected CENC IV length {n}"))),
    }
}

fn filter_sample_plan_by_ms(
    samples: &[SamplePlanEntry],
    timescale: u32,
    start_ms: u64,
    end_ms: Option<u64>,
) -> Vec<SamplePlanEntry> {
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
        out.push(sample.clone());
    }
    let mut cts = 0u64;
    for sample in &mut out {
        sample.start_cts = cts;
        cts = cts.saturating_add(u64::from(sample.duration));
    }
    out
}

/// Patch enca→frma format, strip sinf / pssh / mvex from a moov box buffer.
fn patch_dash_moov(moov_bytes: &[u8]) -> Result<Vec<u8>> {
    let mut body = moov_bytes.to_vec();

    let (stbl_start, stbl_end) =
        find_box_range(&body, b"stbl")?.ok_or_else(|| DrmError::Mp4("moov missing stbl".into()))?;
    let (stsd_start, stsd_end) = find_direct_child(&body, stbl_start, stbl_end, b"stsd")?
        .ok_or_else(|| DrmError::Mp4("stbl missing stsd".into()))?;

    let entry_pos = stsd_start + 16;
    if entry_pos + 8 > stsd_end {
        return Err(DrmError::Mp4("stsd truncated".into()));
    }
    let entry_size =
        u32::from_be_bytes(body[entry_pos..entry_pos + 4].try_into().unwrap()) as usize;
    let entry_end = entry_pos + entry_size;
    if entry_end > stsd_end || entry_size < 36 {
        return Err(DrmError::Mp4("invalid sample entry".into()));
    }
    let entry_type = &body[entry_pos + 4..entry_pos + 8];

    if entry_type == b"enca" {
        let children_start = entry_pos + 36;
        let sinf = find_child_in_buf(&body, children_start, entry_end, b"sinf")?
            .ok_or_else(|| DrmError::Mp4("enca missing sinf".into()))?;
        let frma = find_child_in_buf(&body, sinf.0 + 8, sinf.1, b"frma")?
            .ok_or_else(|| DrmError::Mp4("sinf missing frma".into()))?;
        if frma.0 + 12 > frma.1 {
            return Err(DrmError::Mp4("frma truncated".into()));
        }
        let format = body[frma.0 + 8..frma.0 + 12].to_vec();
        body[entry_pos + 4..entry_pos + 8].copy_from_slice(&format);
        body = splice_replace(&body, sinf.0, sinf.1, &[])?;
    }

    body = remove_moov_children_named(&body, &[b"pssh", b"mvex"])?;

    let size = body.len() as u32;
    body[0..4].copy_from_slice(&size.to_be_bytes());
    Ok(body)
}

fn find_child_in_buf(
    buf: &[u8],
    start: usize,
    end: usize,
    fourcc: &[u8; 4],
) -> Result<Option<(usize, usize)>> {
    let mut pos = start;
    while pos + 8 <= end {
        let size = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        if size < 8 || pos + size > end {
            break;
        }
        let kind = &buf[pos + 4..pos + 8];
        let box_end = pos + size;
        if kind == fourcc {
            return Ok(Some((pos, box_end)));
        }
        pos = box_end;
    }
    Ok(None)
}

fn remove_moov_children_named(moov: &[u8], names: &[&[u8; 4]]) -> Result<Vec<u8>> {
    if moov.len() < 8 || &moov[4..8] != b"moov" {
        return Err(DrmError::Mp4("expected moov box".into()));
    }
    let mut body = moov.to_vec();
    loop {
        let mut removed = false;
        let end = body.len();
        let mut pos = 8usize;
        while pos + 8 <= end {
            let size = u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap()) as usize;
            if size < 8 || pos + size > end {
                break;
            }
            let kind = body[pos + 4..pos + 8].to_vec();
            let box_end = pos + size;
            if names.iter().any(|n| kind.as_slice() == n.as_slice()) {
                body = splice_replace(&body, pos, box_end, &[])?;
                removed = true;
                break;
            }
            pos = box_end;
        }
        if !removed {
            break;
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use tempfile::NamedTempFile;

    #[test]
    fn expand_iv_pads_high_bytes() {
        let iv8 = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let iv16 = expand_cenc_iv(&iv8);
        assert_eq!(&iv16[..8], &iv8);
        assert_eq!(&iv16[8..], &[0u8; 8]);
    }

    #[test]
    fn normalize_accepts_8_and_16() {
        let iv8 = [1u8; 8];
        assert_eq!(normalize_cenc_iv(&iv8).unwrap()[8..], [0u8; 8]);
        let iv16 = [2u8; 16];
        assert_eq!(normalize_cenc_iv(&iv16).unwrap(), iv16);
    }

    #[test]
    fn parse_tenc_kid_bytes() {
        let mut tenc = Vec::new();
        tenc.extend_from_slice(&32u32.to_be_bytes());
        tenc.extend_from_slice(b"tenc");
        tenc.extend_from_slice(&0u32.to_be_bytes());
        tenc.push(0);
        tenc.push(0);
        tenc.push(1);
        tenc.push(8);
        let kid = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10,
        ];
        tenc.extend_from_slice(&kid);
        let size = tenc.len() as u32;
        tenc[0..4].copy_from_slice(&size.to_be_bytes());

        let header = BoxHeader {
            start: 0,
            size: u64::from(size),
            header_len: 8,
            kind: TENC,
        };
        let mut cur = Cursor::new(tenc);
        let parsed = parse_tenc_default_kid(&mut cur, &header).unwrap();
        assert_eq!(parsed, kid);
    }

    #[test]
    fn trun_flag_parsing() {
        let mut trun = Vec::new();
        trun.extend_from_slice(&0u32.to_be_bytes());
        trun.extend_from_slice(b"trun");
        trun.push(0);
        trun.extend_from_slice(&0x00_03_01u32.to_be_bytes()[1..]);
        trun.extend_from_slice(&2u32.to_be_bytes());
        trun.extend_from_slice(&8u32.to_be_bytes());
        trun.extend_from_slice(&1024u32.to_be_bytes());
        trun.extend_from_slice(&16u32.to_be_bytes());
        trun.extend_from_slice(&1024u32.to_be_bytes());
        trun.extend_from_slice(&20u32.to_be_bytes());
        let size = trun.len() as u32;
        trun[0..4].copy_from_slice(&size.to_be_bytes());

        let header = BoxHeader {
            start: 0,
            size: u64::from(size),
            header_len: 8,
            kind: TRUN,
        };
        let mut cur = Cursor::new(trun);
        let (sizes, durs) = parse_trun(&mut cur, &header, None, None).unwrap();
        assert_eq!(sizes, vec![16, 20]);
        assert_eq!(durs, vec![1024, 1024]);
    }

    fn push_box(buf: &mut Vec<u8>, kind: &[u8; 4], content: &[u8]) {
        let size = 8 + content.len() as u32;
        buf.extend_from_slice(&size.to_be_bytes());
        buf.extend_from_slice(kind);
        buf.extend_from_slice(content);
    }

    fn push_fullbox(buf: &mut Vec<u8>, kind: &[u8; 4], version: u8, flags: u32, content: &[u8]) {
        let mut body = vec![
            version,
            ((flags >> 16) & 0xff) as u8,
            ((flags >> 8) & 0xff) as u8,
            (flags & 0xff) as u8,
        ];
        body.extend_from_slice(content);
        push_box(buf, kind, &body);
    }

    #[test]
    fn synthetic_dash_roundtrip() {
        let key = [0x42u8; 16];
        let kid = [0x11u8; 16];
        let plain = b"SYNTHETIC_AAC_FRAME_DATA!!".to_vec();
        let mut enc = plain.clone();
        let iv8 = [0xAAu8; 8];
        let iv16 = expand_cenc_iv(&iv8);
        decrypt_cenc_sample_in_place(&key, &iv16, &mut enc);

        let mut tenc_content = vec![0u8, 0, 1, 8];
        tenc_content.extend_from_slice(&kid);
        let mut tenc_box = Vec::new();
        push_fullbox(&mut tenc_box, b"tenc", 0, 0, &tenc_content);

        let mut schi_box = Vec::new();
        push_box(&mut schi_box, b"schi", &tenc_box);

        let mut frma_box = Vec::new();
        push_box(&mut frma_box, b"frma", b"mp4a");

        let mut schm_content = Vec::new();
        schm_content.extend_from_slice(b"cenc");
        schm_content.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        let mut schm_box = Vec::new();
        push_fullbox(&mut schm_box, b"schm", 0, 0, &schm_content);

        let mut sinf_children = Vec::new();
        sinf_children.extend_from_slice(&frma_box);
        sinf_children.extend_from_slice(&schm_box);
        sinf_children.extend_from_slice(&schi_box);
        let mut sinf_box = Vec::new();
        push_box(&mut sinf_box, b"sinf", &sinf_children);

        let mut ase_fields = vec![0u8; 28];
        ase_fields[16..18].copy_from_slice(&2u16.to_be_bytes());
        ase_fields[18..20].copy_from_slice(&16u16.to_be_bytes());
        ase_fields[24..28].copy_from_slice(&(44100u32 << 16).to_be_bytes());

        let mut enca_content = Vec::new();
        enca_content.extend_from_slice(&ase_fields);
        enca_content.extend_from_slice(&sinf_box);
        let mut enca_box = Vec::new();
        let enca_size = 8 + enca_content.len() as u32;
        enca_box.extend_from_slice(&enca_size.to_be_bytes());
        enca_box.extend_from_slice(b"enca");
        enca_box.extend_from_slice(&enca_content);

        let mut stsd_payload = Vec::new();
        stsd_payload.extend_from_slice(&1u32.to_be_bytes());
        stsd_payload.extend_from_slice(&enca_box);
        let mut stsd_box = Vec::new();
        push_fullbox(&mut stsd_box, b"stsd", 0, 0, &stsd_payload);

        let mut stts = Vec::new();
        push_fullbox(&mut stts, b"stts", 0, 0, &0u32.to_be_bytes());
        let mut stsc = Vec::new();
        push_fullbox(&mut stsc, b"stsc", 0, 0, &0u32.to_be_bytes());
        let mut stsz_body = Vec::new();
        stsz_body.extend_from_slice(&0u32.to_be_bytes());
        stsz_body.extend_from_slice(&0u32.to_be_bytes());
        let mut stsz = Vec::new();
        push_fullbox(&mut stsz, b"stsz", 0, 0, &stsz_body);
        let mut stco = Vec::new();
        push_fullbox(&mut stco, b"stco", 0, 0, &0u32.to_be_bytes());

        let mut stbl_children = Vec::new();
        stbl_children.extend_from_slice(&stsd_box);
        stbl_children.extend_from_slice(&stts);
        stbl_children.extend_from_slice(&stsc);
        stbl_children.extend_from_slice(&stsz);
        stbl_children.extend_from_slice(&stco);
        let mut stbl = Vec::new();
        push_box(&mut stbl, b"stbl", &stbl_children);

        let mut smhd = Vec::new();
        push_fullbox(&mut smhd, b"smhd", 0, 0, &[0u8; 4]);
        let mut dref_body = Vec::new();
        dref_body.extend_from_slice(&1u32.to_be_bytes());
        let mut url = Vec::new();
        push_fullbox(&mut url, b"url ", 0, 1, &[]);
        dref_body.extend_from_slice(&url);
        let mut dref = Vec::new();
        push_fullbox(&mut dref, b"dref", 0, 0, &dref_body);
        let mut dinf = Vec::new();
        push_box(&mut dinf, b"dinf", &dref);

        let mut minf_children = Vec::new();
        minf_children.extend_from_slice(&smhd);
        minf_children.extend_from_slice(&dinf);
        minf_children.extend_from_slice(&stbl);
        let mut minf = Vec::new();
        push_box(&mut minf, b"minf", &minf_children);

        let mut mdhd_body = Vec::new();
        mdhd_body.extend_from_slice(&0u32.to_be_bytes());
        mdhd_body.extend_from_slice(&0u32.to_be_bytes());
        mdhd_body.extend_from_slice(&44100u32.to_be_bytes());
        mdhd_body.extend_from_slice(&0u32.to_be_bytes());
        mdhd_body.extend_from_slice(&0u16.to_be_bytes());
        mdhd_body.extend_from_slice(&0u16.to_be_bytes());
        let mut mdhd = Vec::new();
        push_fullbox(&mut mdhd, b"mdhd", 0, 0, &mdhd_body);

        let mut hdlr_body = Vec::new();
        hdlr_body.extend_from_slice(&0u32.to_be_bytes());
        hdlr_body.extend_from_slice(b"soun");
        hdlr_body.extend_from_slice(&[0u8; 12]);
        hdlr_body.push(0);
        let mut hdlr = Vec::new();
        push_fullbox(&mut hdlr, b"hdlr", 0, 0, &hdlr_body);

        let mut mdia_children = Vec::new();
        mdia_children.extend_from_slice(&mdhd);
        mdia_children.extend_from_slice(&hdlr);
        mdia_children.extend_from_slice(&minf);
        let mut mdia = Vec::new();
        push_box(&mut mdia, b"mdia", &mdia_children);

        let mut tkhd_body = Vec::new();
        tkhd_body.extend_from_slice(&0u32.to_be_bytes());
        tkhd_body.extend_from_slice(&0u32.to_be_bytes());
        tkhd_body.extend_from_slice(&1u32.to_be_bytes());
        tkhd_body.extend_from_slice(&0u32.to_be_bytes());
        tkhd_body.extend_from_slice(&0u32.to_be_bytes());
        tkhd_body.extend_from_slice(&[0u8; 8]);
        tkhd_body.extend_from_slice(&0u16.to_be_bytes());
        tkhd_body.extend_from_slice(&0u16.to_be_bytes());
        tkhd_body.extend_from_slice(&0u16.to_be_bytes());
        tkhd_body.extend_from_slice(&0u16.to_be_bytes());
        tkhd_body.extend_from_slice(&[0u8; 36]);
        tkhd_body.extend_from_slice(&0u32.to_be_bytes());
        tkhd_body.extend_from_slice(&0u32.to_be_bytes());
        let mut tkhd = Vec::new();
        push_fullbox(&mut tkhd, b"tkhd", 0, 3, &tkhd_body);

        let mut trak_children = Vec::new();
        trak_children.extend_from_slice(&tkhd);
        trak_children.extend_from_slice(&mdia);
        let mut trak = Vec::new();
        push_box(&mut trak, b"trak", &trak_children);

        let mut mvhd_body = Vec::new();
        mvhd_body.extend_from_slice(&0u32.to_be_bytes());
        mvhd_body.extend_from_slice(&0u32.to_be_bytes());
        mvhd_body.extend_from_slice(&44100u32.to_be_bytes());
        mvhd_body.extend_from_slice(&0u32.to_be_bytes());
        mvhd_body.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        mvhd_body.extend_from_slice(&0x0100u16.to_be_bytes());
        mvhd_body.extend_from_slice(&0u16.to_be_bytes());
        mvhd_body.extend_from_slice(&[0u8; 8]);
        mvhd_body.extend_from_slice(&[0u8; 36]);
        mvhd_body.extend_from_slice(&[0u8; 24]);
        mvhd_body.extend_from_slice(&2u32.to_be_bytes());
        let mut mvhd = Vec::new();
        push_fullbox(&mut mvhd, b"mvhd", 0, 0, &mvhd_body);

        let mut trex_body = Vec::new();
        trex_body.extend_from_slice(&1u32.to_be_bytes());
        trex_body.extend_from_slice(&1u32.to_be_bytes());
        trex_body.extend_from_slice(&1024u32.to_be_bytes());
        trex_body.extend_from_slice(&0u32.to_be_bytes());
        trex_body.extend_from_slice(&0u32.to_be_bytes());
        let mut trex = Vec::new();
        push_fullbox(&mut trex, b"trex", 0, 0, &trex_body);
        let mut mvex = Vec::new();
        push_box(&mut mvex, b"mvex", &trex);

        let mut moov_children = Vec::new();
        moov_children.extend_from_slice(&mvhd);
        moov_children.extend_from_slice(&trak);
        moov_children.extend_from_slice(&mvex);
        let mut moov = Vec::new();
        push_box(&mut moov, b"moov", &moov_children);

        let mut ftyp = Vec::new();
        let brands = [b"dash", b"iso6", b"mp41"];
        let ftyp_size = 8 + 8 + brands.len() as u32 * 4;
        ftyp.extend_from_slice(&ftyp_size.to_be_bytes());
        ftyp.extend_from_slice(b"ftyp");
        ftyp.extend_from_slice(b"dash");
        ftyp.extend_from_slice(&0u32.to_be_bytes());
        for b in brands {
            ftyp.extend_from_slice(b);
        }

        let mut tfhd_body = Vec::new();
        tfhd_body.extend_from_slice(&1u32.to_be_bytes());
        let mut tfhd = Vec::new();
        push_fullbox(&mut tfhd, b"tfhd", 0, 0x020000, &tfhd_body);

        let mut trun_body = Vec::new();
        trun_body.extend_from_slice(&1u32.to_be_bytes());
        trun_body.extend_from_slice(&8u32.to_be_bytes());
        trun_body.extend_from_slice(&1024u32.to_be_bytes());
        trun_body.extend_from_slice(&(enc.len() as u32).to_be_bytes());
        let mut trun = Vec::new();
        push_fullbox(&mut trun, b"trun", 0, 0x301, &trun_body);

        let mut senc_body = Vec::new();
        senc_body.extend_from_slice(&1u32.to_be_bytes());
        senc_body.extend_from_slice(&iv8);
        let mut senc = Vec::new();
        push_fullbox(&mut senc, b"senc", 0, 0, &senc_body);

        let mut traf_children = Vec::new();
        traf_children.extend_from_slice(&tfhd);
        traf_children.extend_from_slice(&trun);
        traf_children.extend_from_slice(&senc);
        let mut traf = Vec::new();
        push_box(&mut traf, b"traf", &traf_children);

        let mut mfhd = Vec::new();
        push_fullbox(&mut mfhd, b"mfhd", 0, 0, &1u32.to_be_bytes());

        let mut moof_children = Vec::new();
        moof_children.extend_from_slice(&mfhd);
        moof_children.extend_from_slice(&traf);
        let mut moof = Vec::new();
        push_box(&mut moof, b"moof", &moof_children);

        let mut mdat = Vec::new();
        let mdat_size = 8 + enc.len() as u32;
        mdat.extend_from_slice(&mdat_size.to_be_bytes());
        mdat.extend_from_slice(b"mdat");
        mdat.extend_from_slice(&enc);

        let fragment_size = (moof.len() + mdat.len()) as u32;

        let mut sidx_body = Vec::new();
        sidx_body.extend_from_slice(&1u32.to_be_bytes());
        sidx_body.extend_from_slice(&44100u32.to_be_bytes());
        sidx_body.extend_from_slice(&0u32.to_be_bytes());
        sidx_body.extend_from_slice(&0u32.to_be_bytes());
        sidx_body.extend_from_slice(&0u16.to_be_bytes());
        sidx_body.extend_from_slice(&1u16.to_be_bytes());
        sidx_body.extend_from_slice(&fragment_size.to_be_bytes());
        sidx_body.extend_from_slice(&1024u32.to_be_bytes());
        sidx_body.extend_from_slice(&0x8000_0000u32.to_be_bytes());
        let mut sidx = Vec::new();
        push_fullbox(&mut sidx, b"sidx", 0, 0, &sidx_body);

        let mut file_bytes = Vec::new();
        file_bytes.extend_from_slice(&ftyp);
        file_bytes.extend_from_slice(&moov);
        file_bytes.extend_from_slice(&sidx);
        file_bytes.extend_from_slice(&moof);
        file_bytes.extend_from_slice(&mdat);

        let mut input = NamedTempFile::new().unwrap();
        input.write_all(&file_bytes).unwrap();
        input.flush().unwrap();
        let output = NamedTempFile::new().unwrap();
        let out_path = output.path().with_extension("m4b");

        assert!(looks_like_dash(input.path()).unwrap());

        let outcome = decrypt_dash_cenc(
            input.path(),
            &out_path,
            &hex::encode(kid),
            &hex::encode(key),
            None,
        )
        .expect("decrypt_dash_cenc");

        let out_data = std::fs::read(&outcome.output).unwrap();
        assert!(
            out_data.windows(plain.len()).any(|w| w == plain.as_slice()),
            "decrypted payload missing from output"
        );
        assert!(out_data.windows(4).any(|w| w == b"mp4a"));
        assert!(!out_data.windows(4).any(|w| w == b"enca"));
        assert!(!out_data.windows(4).any(|w| w == b"sinf"));

        let _ = std::fs::remove_file(&out_path);
    }
}
