//! Package ordered audio chapter files into a single progressive M4B (AAC-LC).
//!
//! - Clear AAC in MP4/M4A/M4B parts are **losslessly remuxed** (sample copy; no
//!   decode/re-encode) — the fast path for Chirp and similar sources.
//! - MP3 (and other decode-only) parts stream through Symphonia → a small PCM
//!   staging buffer → fdk-aac, spilling AAC access units to a temp file so the
//!   full book is never held in memory. This is typically much faster than
//!   realtime playback for a single encode pass.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fdk_aac::enc::{AudioObjectType, BitRate, ChannelMode, Encoder, EncoderParams, Transport};
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tempfile::NamedTempFile;

use crate::error::{DecryptError, Result};
use crate::mp4::mux_aac::{write_aac_m4b_from_reader, AuTiming, MuxAacStreamRequest};
use crate::mp4::{extract_mp4a_config, parse_mp4, SampleEntryKind};
use crate::DecryptOutcome;

/// Request to package ordered MP3 parts into one M4B.
#[derive(Debug, Clone)]
pub struct PackageM4bRequest {
    /// Ordered MP3 chapter / part files.
    pub parts: Vec<PathBuf>,
    /// Destination `.m4b` path.
    pub output: PathBuf,
    /// Optional chapter titles (length may match `parts`).
    pub chapter_titles: Vec<String>,
}

/// Decode/remux ordered parts → progressive M4B.
///
/// Clear AAC MP4/M4A/M4B parts are remuxed without re-encoding. MP3 parts are
/// decoded and encoded to AAC-LC via a streaming path.
///
/// Returns the output path plus chapter list `(title, start_ms)`.
pub async fn package_m4b_from_mp3(
    req: PackageM4bRequest,
) -> Result<(DecryptOutcome, Vec<(String, u64)>)> {
    if req.parts.is_empty() {
        return Err(DecryptError::Native(
            "package_m4b_from_mp3 requires at least one audio part".into(),
        ));
    }
    for part in &req.parts {
        if !part.exists() {
            return Err(DecryptError::InputMissing(part.clone()));
        }
    }
    if let Some(parent) = req.output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let req = req.clone();
    tokio::task::spawn_blocking(move || package_m4b_from_parts_native(&req))
        .await
        .map_err(|err| DecryptError::Native(format!("m4b package task join error: {err}")))?
}

/// Encode interleaved PCM to AAC-LC and mux an M4B (test / internal helper).
///
/// Feeds PCM in bounded chunks so callers with large buffers still do not
/// duplicate the full stream as AAC access units in memory.
pub fn package_m4b_from_pcm(
    pcm: &[i16],
    sample_rate: u32,
    channels: u16,
    output: &Path,
    chapter_boundaries_ms: &[(String, u64)],
) -> Result<(DecryptOutcome, Vec<(String, u64)>)> {
    if pcm.is_empty() {
        return Err(DecryptError::Native("PCM is empty".into()));
    }
    if sample_rate == 0 || channels == 0 {
        return Err(DecryptError::Native(
            "sample_rate and channels must be non-zero".into(),
        ));
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut session = StreamingAacSession::new(sample_rate, channels)?;
    // Feed in ~1s chunks so even large test buffers encode without holding AUs.
    let chunk_frames = sample_rate.max(1) as usize;
    let stride = chunk_frames * usize::from(channels);
    for chunk in pcm.chunks(stride) {
        session.push_pcm(chunk)?;
    }
    let encoded = session.finish()?;
    mux_encoded_to_m4b(output, &encoded)?;

    let chapters = if chapter_boundaries_ms.is_empty() {
        vec![("Chapter 1".into(), 0u64)]
    } else {
        chapter_boundaries_ms.to_vec()
    };

    Ok((
        DecryptOutcome {
            output: output.to_path_buf(),
        },
        chapters,
    ))
}

fn package_m4b_from_parts_native(
    req: &PackageM4bRequest,
) -> Result<(DecryptOutcome, Vec<(String, u64)>)> {
    if req.parts.iter().all(|p| looks_like_aac_mp4_part(p)) {
        return package_m4b_remux_aac_parts(req);
    }
    package_m4b_transcode_parts(req)
}

fn looks_like_aac_mp4_part(path: &Path) -> bool {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("m4a" | "m4b" | "mp4") => true,
        Some("mp3") => false,
        _ => sniff_ftyp_major_brand(path).is_some_and(|b| {
            matches!(
                b.as_slice(),
                b"M4A " | b"M4B " | b"mp42" | b"isom" | b"iso2" | b"mp41"
            )
        }),
    }
}

fn sniff_ftyp_major_brand(path: &Path) -> Option<[u8; 4]> {
    let mut file = File::open(path).ok()?;
    let mut hdr = [0u8; 12];
    file.read_exact(&mut hdr).ok()?;
    if &hdr[4..8] != b"ftyp" {
        return None;
    }
    let mut brand = [0u8; 4];
    brand.copy_from_slice(&hdr[8..12]);
    Some(brand)
}

/// Losslessly concatenate clear AAC MP4/M4A parts into one progressive M4B.
fn package_m4b_remux_aac_parts(
    req: &PackageM4bRequest,
) -> Result<(DecryptOutcome, Vec<(String, u64)>)> {
    tracing::info!(
        parts = req.parts.len(),
        output = %req.output.display(),
        "package m4b from aac parts (lossless remux)"
    );

    let mut sample_sizes: Vec<u32> = Vec::new();
    let mut sample_durations: Vec<u32> = Vec::new();
    let mut chapter_starts_ms: Vec<u64> = Vec::with_capacity(req.parts.len());
    let mut cumulative_ticks: u64 = 0;
    let mut config = None;
    let mut timescale = 0u32;

    let mut payload = NamedTempFile::new()
        .map_err(|err| DecryptError::Native(format!("create AAC remux temp: {err}")))?;
    let mut sample_buf = Vec::new();

    for part in &req.parts {
        let mp4 = parse_mp4(part)?;
        if mp4.audio.sample_entry_kind != SampleEntryKind::Mp4a {
            return Err(DecryptError::Native(format!(
                "part {} is not clear AAC (mp4a); found {:?}",
                part.display(),
                mp4.audio.sample_entry_kind
            )));
        }
        let part_cfg = extract_mp4a_config(&mp4)?;
        match &config {
            None => {
                timescale = mp4.audio.timescale.max(1);
                config = Some(part_cfg);
            }
            Some(prev) => {
                if prev.sample_rate != part_cfg.sample_rate
                    || prev.channels != part_cfg.channels
                    || prev.asc != part_cfg.asc
                {
                    return Err(DecryptError::Native(format!(
                        "AAC parts have mismatched decoder config ({} vs {})",
                        part.display(),
                        req.parts[0].display()
                    )));
                }
                if mp4.audio.timescale != timescale {
                    return Err(DecryptError::Native(format!(
                        "AAC parts have mismatched timescale ({timescale} vs {})",
                        mp4.audio.timescale
                    )));
                }
            }
        }

        let start_ms = if timescale == 0 {
            0
        } else {
            cumulative_ticks.saturating_mul(1000) / u64::from(timescale)
        };
        chapter_starts_ms.push(start_ms);

        if mp4.audio.samples.is_empty() {
            return Err(DecryptError::Native(format!(
                "part {} has no AAC samples",
                part.display()
            )));
        }

        let mut input = File::open(part)?;
        for sample in &mp4.audio.samples {
            sample_buf.resize(sample.size as usize, 0);
            input
                .seek(SeekFrom::Start(sample.offset))
                .map_err(|err| DecryptError::Native(format!("seek AAC sample: {err}")))?;
            input
                .read_exact(&mut sample_buf)
                .map_err(|err| DecryptError::Native(format!("read AAC sample: {err}")))?;
            payload
                .write_all(&sample_buf)
                .map_err(|err| DecryptError::Native(format!("write AAC sample: {err}")))?;
            sample_sizes.push(sample.size);
            sample_durations.push(sample.duration.max(1));
            cumulative_ticks = cumulative_ticks.saturating_add(u64::from(sample.duration));
        }
    }

    let config = config.ok_or_else(|| DecryptError::Native("no AAC parts decoded".into()))?;
    if sample_sizes.is_empty() {
        return Err(DecryptError::Native("no AAC samples to remux".into()));
    }

    payload
        .flush()
        .map_err(|err| DecryptError::Native(format!("flush AAC remux temp: {err}")))?;
    let mut reader = payload
        .reopen()
        .map_err(|err| DecryptError::Native(format!("reopen AAC remux temp: {err}")))?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|err| DecryptError::Native(format!("seek AAC remux temp: {err}")))?;

    let timing = if sample_durations.iter().all(|d| *d == sample_durations[0]) {
        AuTiming::Uniform(sample_durations[0])
    } else {
        AuTiming::Variable(&sample_durations)
    };

    write_aac_m4b_from_reader(
        &req.output,
        &MuxAacStreamRequest {
            sample_rate: config.sample_rate,
            channels: config.channels,
            asc: &config.asc,
            sample_sizes: &sample_sizes,
            timing,
        },
        reader,
    )?;

    let chapters: Vec<(String, u64)> = chapter_starts_ms
        .into_iter()
        .enumerate()
        .map(|(i, start_ms)| {
            let title = req
                .chapter_titles
                .get(i)
                .cloned()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| format!("Chapter {}", i + 1));
            (title, start_ms)
        })
        .collect();

    if !req.output.exists() {
        return Err(DecryptError::OutputMissing(req.output.clone()));
    }

    Ok((
        DecryptOutcome {
            output: req.output.clone(),
        },
        chapters,
    ))
}

fn package_m4b_transcode_parts(
    req: &PackageM4bRequest,
) -> Result<(DecryptOutcome, Vec<(String, u64)>)> {
    tracing::info!(
        parts = req.parts.len(),
        output = %req.output.display(),
        "package m4b from mp3 (streaming symphonia + fdk-aac)"
    );

    let mut sample_rate: Option<u32> = None;
    let mut channels: Option<u16> = None;
    let mut chapter_starts_ms: Vec<u64> = Vec::with_capacity(req.parts.len());
    let mut cumulative_frames: u64 = 0;
    let mut session: Option<StreamingAacSession> = None;
    let mut decoded_any = false;

    for part in &req.parts {
        let start_ms = match (sample_rate, channels) {
            (Some(rate), Some(_)) if rate > 0 => {
                cumulative_frames.saturating_mul(1000) / u64::from(rate)
            }
            _ => 0,
        };
        chapter_starts_ms.push(start_ms);

        decode_audio_file_streaming(part, |rate, ch, pcm_chunk| {
            match sample_rate {
                None => sample_rate = Some(rate),
                Some(prev) if prev != rate => {
                    return Err(DecryptError::Native(format!(
                        "MP3 parts have mismatched sample rates ({prev} vs {rate})"
                    )));
                }
                _ => {}
            }
            match channels {
                None => channels = Some(ch),
                Some(prev) if prev != ch => {
                    return Err(DecryptError::Native(format!(
                        "MP3 parts have mismatched channel counts ({prev} vs {ch})"
                    )));
                }
                _ => {}
            }

            if session.is_none() {
                session = Some(StreamingAacSession::new(rate, ch)?);
            }
            let sess = session.as_mut().expect("session just initialized");
            let frames = (pcm_chunk.len() as u64) / u64::from(ch.max(1));
            cumulative_frames = cumulative_frames.saturating_add(frames);
            decoded_any = true;
            sess.push_pcm(pcm_chunk)
        })?;
    }

    if !decoded_any {
        return Err(DecryptError::Native(
            "decoded no PCM from input parts".into(),
        ));
    }

    let encoded = session
        .ok_or_else(|| DecryptError::Native("AAC encoder was never initialized".into()))?
        .finish()?;
    mux_encoded_to_m4b(&req.output, &encoded)?;

    let chapters: Vec<(String, u64)> = chapter_starts_ms
        .into_iter()
        .enumerate()
        .map(|(i, start_ms)| {
            let title = req
                .chapter_titles
                .get(i)
                .cloned()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| format!("Chapter {}", i + 1));
            (title, start_ms)
        })
        .collect();

    if !req.output.exists() {
        return Err(DecryptError::OutputMissing(req.output.clone()));
    }

    Ok((
        DecryptOutcome {
            output: req.output.clone(),
        },
        chapters,
    ))
}

struct EncodedAacStream {
    sample_rate: u32,
    channels: u16,
    asc: Vec<u8>,
    sample_sizes: Vec<u32>,
    sample_duration: u32,
    /// Concatenated AAC AU payloads on disk.
    payload: NamedTempFile,
}

fn mux_encoded_to_m4b(output: &Path, encoded: &EncodedAacStream) -> Result<()> {
    let mut reader = encoded
        .payload
        .reopen()
        .map_err(|err| DecryptError::Native(format!("reopen AAC payload temp: {err}")))?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|err| DecryptError::Native(format!("seek AAC payload temp: {err}")))?;
    write_aac_m4b_from_reader(
        output,
        &MuxAacStreamRequest {
            sample_rate: encoded.sample_rate,
            channels: encoded.channels,
            asc: &encoded.asc,
            sample_sizes: &encoded.sample_sizes,
            timing: AuTiming::Uniform(encoded.sample_duration),
        },
        reader,
    )
}

/// Streaming fdk-aac session: bounded PCM staging + AU spill to temp file.
struct StreamingAacSession {
    sample_rate: u32,
    /// Channel count of PCM fed via [`Self::push_pcm`].
    pcm_channels: u16,
    encoder: Encoder,
    frame_length: u32,
    samples_per_frame: usize,
    out_channels: usize,
    asc: Vec<u8>,
    /// Interleaved PCM awaiting encode (encoder input channel layout).
    staging: Vec<i16>,
    out_buf: Vec<u8>,
    sample_sizes: Vec<u32>,
    payload: NamedTempFile,
}

impl StreamingAacSession {
    fn new(sample_rate: u32, pcm_channels: u16) -> Result<Self> {
        let channel_mode = match pcm_channels {
            1 => ChannelMode::Mono,
            _ => ChannelMode::Stereo,
        };
        // ~64 kbps mono / ~96 kbps stereo — fine for speech audiobooks.
        let bitrate = if pcm_channels == 1 { 64_000 } else { 96_000 };

        let encoder = Encoder::new(EncoderParams {
            bit_rate: BitRate::Cbr(bitrate),
            sample_rate,
            transport: Transport::Raw,
            channels: channel_mode,
            audio_object_type: AudioObjectType::Mpeg4LowComplexity,
        })
        .map_err(|err| DecryptError::Native(format!("fdk-aac init failed: {err}")))?;

        let info = encoder
            .info()
            .map_err(|err| DecryptError::Native(format!("fdk-aac info failed: {err}")))?;
        let frame_length = info.frameLength.max(1);
        let out_channels = info.inputChannels.max(1) as usize;
        let samples_per_frame = frame_length as usize * out_channels;
        let asc = info.confBuf[..info.confSize as usize].to_vec();
        if asc.is_empty() {
            return Err(DecryptError::Native(
                "fdk-aac returned empty AudioSpecificConfig".into(),
            ));
        }

        let payload = NamedTempFile::new()
            .map_err(|err| DecryptError::Native(format!("create AAC payload temp: {err}")))?;

        Ok(Self {
            sample_rate,
            pcm_channels,
            encoder,
            frame_length,
            samples_per_frame,
            out_channels,
            asc,
            staging: Vec::with_capacity(samples_per_frame * 4),
            out_buf: vec![0u8; info.maxOutBufBytes.max(2048) as usize],
            sample_sizes: Vec::new(),
            payload,
        })
    }

    fn push_pcm(&mut self, pcm: &[i16]) -> Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        self.append_converted(pcm);
        self.encode_full_frames()
    }

    fn finish(mut self) -> Result<EncodedAacStream> {
        // Pad to a whole number of encoder frames so we never need EOF flush.
        let rem = self.staging.len() % self.samples_per_frame;
        if rem != 0 {
            self.staging
                .resize(self.staging.len() + (self.samples_per_frame - rem), 0);
        }
        self.encode_full_frames()?;

        if self.sample_sizes.is_empty() {
            return Err(DecryptError::Native(
                "fdk-aac produced no AAC access units".into(),
            ));
        }

        self.payload
            .flush()
            .map_err(|err| DecryptError::Native(format!("flush AAC payload temp: {err}")))?;

        Ok(EncodedAacStream {
            sample_rate: self.sample_rate,
            channels: self.pcm_channels,
            asc: self.asc,
            sample_sizes: self.sample_sizes,
            sample_duration: self.frame_length,
            payload: self.payload,
        })
    }

    fn append_converted(&mut self, pcm: &[i16]) {
        match (self.pcm_channels, self.out_channels) {
            (1, 2) => {
                self.staging.reserve(pcm.len() * 2);
                for &s in pcm {
                    self.staging.push(s);
                    self.staging.push(s);
                }
            }
            (2, 1) => {
                self.staging.reserve(pcm.len() / 2);
                for sample in pcm.iter().step_by(2) {
                    self.staging.push(*sample);
                }
            }
            _ => {
                self.staging.extend_from_slice(pcm);
            }
        }
    }

    fn encode_full_frames(&mut self) -> Result<()> {
        while self.staging.len() >= self.samples_per_frame {
            let enc = self
                .encoder
                .encode(&self.staging[..self.samples_per_frame], &mut self.out_buf)
                .map_err(|err| DecryptError::Native(format!("fdk-aac encode failed: {err}")))?;
            if enc.input_consumed == 0 {
                // Avoid infinite loop if encoder stalls.
                break;
            }
            self.staging.drain(..enc.input_consumed);
            if enc.output_size > 0 {
                let au = &self.out_buf[..enc.output_size];
                self.payload
                    .write_all(au)
                    .map_err(|err| DecryptError::Native(format!("write AAC AU: {err}")))?;
                self.sample_sizes.push(au.len() as u32);
            }
        }
        Ok(())
    }
}

/// Decode one audio file, invoking `on_pcm` with interleaved i16 chunks.
///
/// Only one decoded packet's PCM is staged at a time (plus the encoder's small buffer).
fn decode_audio_file_streaming<F>(input: &Path, mut on_pcm: F) -> Result<()>
where
    F: FnMut(u32, u16, &[i16]) -> Result<()>,
{
    let file = File::open(input)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = input.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|err| DecryptError::Native(format!("probe failed: {err}")))?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| DecryptError::Native("no decodable audio track".into()))?
        .clone();
    let track_id = track.id;
    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| DecryptError::Native("missing sample rate".into()))?;
    let channels = track
        .codec_params
        .channels
        .map(|c| c.count())
        .unwrap_or(2)
        .clamp(1, 2) as u16;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|err| DecryptError::Native(format!("decoder init failed: {err}")))?;

    let mut pcm_scratch = Vec::new();
    let mut decoded_any = false;
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(SymphoniaError::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(err) => {
                return Err(DecryptError::Native(format!("demux error: {err}")));
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(buf) => buf,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(err) => {
                return Err(DecryptError::Native(format!("decode error: {err}")));
            }
        };
        pcm_scratch.clear();
        append_pcm_i16(&decoded, channels, &mut pcm_scratch);
        if !pcm_scratch.is_empty() {
            decoded_any = true;
            on_pcm(sample_rate, channels, &pcm_scratch)?;
        }
    }

    if !decoded_any {
        return Err(DecryptError::Native(format!(
            "decoded no PCM from {}",
            input.display()
        )));
    }
    Ok(())
}

fn append_pcm_i16(buf: &AudioBufferRef<'_>, out_channels: u16, dst: &mut Vec<i16>) {
    match buf {
        AudioBufferRef::F32(buf) => {
            let frames = buf.frames();
            let chans = buf.spec().channels.count().min(2);
            for i in 0..frames {
                if out_channels == 1 {
                    let mut acc = 0.0f32;
                    for c in 0..chans {
                        acc += buf.chan(c)[i];
                    }
                    dst.push(float_to_i16(acc / chans as f32));
                } else {
                    let l = buf.chan(0)[i];
                    let r = if chans > 1 { buf.chan(1)[i] } else { l };
                    dst.push(float_to_i16(l));
                    dst.push(float_to_i16(r));
                }
            }
        }
        AudioBufferRef::S16(buf) => {
            let frames = buf.frames();
            let chans = buf.spec().channels.count().min(2);
            for i in 0..frames {
                if out_channels == 1 {
                    let mut acc = 0i32;
                    for c in 0..chans {
                        acc += i32::from(buf.chan(c)[i]);
                    }
                    dst.push((acc / chans as i32) as i16);
                } else {
                    let l = buf.chan(0)[i];
                    let r = if chans > 1 { buf.chan(1)[i] } else { l };
                    dst.push(l);
                    dst.push(r);
                }
            }
        }
        other => {
            let frames = other.frames();
            for _ in 0..frames {
                if out_channels == 1 {
                    dst.push(0);
                } else {
                    dst.push(0);
                    dst.push(0);
                }
            }
        }
    }
}

fn float_to_i16(sample: f32) -> i16 {
    let s = sample.clamp(-1.0, 1.0);
    (s * f32::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::parse_mp4;
    use std::io::Read;

    #[test]
    fn package_silent_pcm_to_m4b() {
        let sample_rate = 44_100u32;
        let channels = 2u16;
        // ~0.25 s of silence.
        let frames = sample_rate / 4;
        let pcm = vec![0i16; (frames * u32::from(channels)) as usize];
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("silent.m4b");

        let (outcome, chapters) = package_m4b_from_pcm(
            &pcm,
            sample_rate,
            channels,
            &out,
            &[("Part 1".into(), 0), ("Part 2".into(), 100)],
        )
        .unwrap();

        assert_eq!(outcome.output, out);
        assert!(out.exists());
        assert!(out.metadata().unwrap().len() > 100);
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0].0, "Part 1");

        let mp4 = parse_mp4(&out).unwrap();
        assert!(!mp4.audio.samples.is_empty());
        assert_eq!(mp4.audio.timescale, sample_rate);
    }

    #[test]
    fn package_two_tone_parts_as_chapters() {
        let sample_rate = 24_000u32;
        let channels = 1u16;
        let frames_a = sample_rate / 5; // 200 ms
        let frames_b = sample_rate / 5;
        let mut pcm = Vec::new();
        for i in 0..frames_a {
            let t = i as f32 / sample_rate as f32;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            pcm.push((s * 0.2 * f32::from(i16::MAX)) as i16);
        }
        let boundary_ms = u64::from(frames_a) * 1000 / u64::from(sample_rate);
        for i in 0..frames_b {
            let t = i as f32 / sample_rate as f32;
            let s = (2.0 * std::f32::consts::PI * 660.0 * t).sin();
            pcm.push((s * 0.2 * f32::from(i16::MAX)) as i16);
        }

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("tone.m4b");
        let (_outcome, chapters) = package_m4b_from_pcm(
            &pcm,
            sample_rate,
            channels,
            &out,
            &[("A".into(), 0), ("B".into(), boundary_ms)],
        )
        .unwrap();

        assert_eq!(chapters[1].1, boundary_ms);
        let mp4 = parse_mp4(&out).unwrap();
        assert!(mp4.audio.samples.len() >= 2);
    }

    #[test]
    fn streaming_session_spills_aus_not_heap() {
        let sample_rate = 16_000u32;
        let channels = 1u16;
        let mut session = StreamingAacSession::new(sample_rate, channels).unwrap();
        // ~2 seconds of silence, pushed in small packets.
        let packet = vec![0i16; 256];
        for _ in 0..(sample_rate as usize * 2 / 256) {
            session.push_pcm(&packet).unwrap();
        }
        let encoded = session.finish().unwrap();
        assert!(!encoded.sample_sizes.is_empty());
        let payload_bytes: u64 = encoded.sample_sizes.iter().map(|s| u64::from(*s)).sum();
        let mut reader = encoded.payload.reopen().unwrap();
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).unwrap();
        assert_eq!(buf.len() as u64, payload_bytes);

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("stream.m4b");
        mux_encoded_to_m4b(&out, &encoded).unwrap();
        assert!(out.metadata().unwrap().len() > 100);
    }

    #[test]
    fn package_mp3_parts_streaming() {
        // Verify the streaming mux reader path end-to-end.
        let sample_rate = 22_050u32;
        let channels = 2u16;
        let mut session = StreamingAacSession::new(sample_rate, channels).unwrap();
        session
            .push_pcm(&vec![0i16; (sample_rate as usize) * 2])
            .unwrap();
        let encoded = session.finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("from_reader.m4b");
        let mut reader = encoded.payload.reopen().unwrap();
        reader.seek(SeekFrom::Start(0)).unwrap();
        write_aac_m4b_from_reader(
            &out,
            &MuxAacStreamRequest {
                sample_rate: encoded.sample_rate,
                channels: encoded.channels,
                asc: &encoded.asc,
                sample_sizes: &encoded.sample_sizes,
                timing: AuTiming::Uniform(encoded.sample_duration),
            },
            reader,
        )
        .unwrap();

        let mp4 = parse_mp4(&out).unwrap();
        assert_eq!(mp4.audio.samples.len(), encoded.sample_sizes.len());
    }

    #[test]
    fn remux_two_aac_parts_losslessly() {
        let sample_rate = 24_000u32;
        let channels = 1u16;
        let dir = tempfile::tempdir().unwrap();
        let part_a = dir.path().join("a.m4b");
        let part_b = dir.path().join("b.m4b");
        let frames = sample_rate / 5;
        let pcm_a = vec![1_000i16; (frames * u32::from(channels)) as usize];
        let pcm_b = vec![-1_000i16; (frames * u32::from(channels)) as usize];
        package_m4b_from_pcm(&pcm_a, sample_rate, channels, &part_a, &[]).unwrap();
        package_m4b_from_pcm(&pcm_b, sample_rate, channels, &part_b, &[]).unwrap();

        let out = dir.path().join("joined.m4b");
        let (_outcome, chapters) = package_m4b_remux_aac_parts(&PackageM4bRequest {
            parts: vec![part_a.clone(), part_b.clone()],
            output: out.clone(),
            chapter_titles: vec!["A".into(), "B".into()],
        })
        .unwrap();

        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0], ("A".into(), 0));
        assert!(chapters[1].1 > 0);
        let mp4 = parse_mp4(&out).unwrap();
        let a = parse_mp4(&part_a).unwrap();
        let b = parse_mp4(&part_b).unwrap();
        assert_eq!(
            mp4.audio.samples.len(),
            a.audio.samples.len() + b.audio.samples.len()
        );
        assert!(out.metadata().unwrap().len() > 100);
    }
}
