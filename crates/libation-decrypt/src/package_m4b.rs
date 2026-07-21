//! Package ordered MP3 chapter files into a single progressive M4B (AAC-LC).

use std::fs::File;
use std::path::{Path, PathBuf};

use fdk_aac::enc::{AudioObjectType, BitRate, ChannelMode, Encoder, EncoderParams, Transport};
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::error::{DecryptError, Result};
use crate::mp4::mux_aac::{write_aac_m4b, AacAccessUnit, MuxAacRequest};
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

/// Decode MP3 parts → AAC-LC → progressive M4B.
///
/// Returns the output path plus chapter list `(title, start_ms)`.
pub async fn package_m4b_from_mp3(
    req: PackageM4bRequest,
) -> Result<(DecryptOutcome, Vec<(String, u64)>)> {
    if req.parts.is_empty() {
        return Err(DecryptError::Native(
            "package_m4b_from_mp3 requires at least one MP3 part".into(),
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
    tokio::task::spawn_blocking(move || package_m4b_from_mp3_native(&req))
        .await
        .map_err(|err| DecryptError::Native(format!("m4b package task join error: {err}")))?
}

/// Encode interleaved PCM to AAC-LC and mux an M4B (test / internal helper).
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

    let (samples, asc, _) = encode_pcm_to_aac(pcm, sample_rate, channels)?;
    write_aac_m4b(
        output,
        &MuxAacRequest {
            sample_rate,
            channels,
            asc: &asc,
            samples: &samples,
        },
    )?;

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

fn package_m4b_from_mp3_native(
    req: &PackageM4bRequest,
) -> Result<(DecryptOutcome, Vec<(String, u64)>)> {
    tracing::info!(
        parts = req.parts.len(),
        output = %req.output.display(),
        "package m4b from mp3 (symphonia + fdk-aac)"
    );

    let mut all_pcm: Vec<i16> = Vec::new();
    let mut sample_rate: Option<u32> = None;
    let mut channels: Option<u16> = None;
    let mut chapter_starts_ms: Vec<u64> = Vec::with_capacity(req.parts.len());
    let mut cumulative_frames: u64 = 0;

    for part in &req.parts {
        let decoded = decode_mp3_to_pcm(part)?;
        let rate = decoded.sample_rate;
        let ch = decoded.channels;
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

        let start_ms = if rate == 0 {
            0
        } else {
            cumulative_frames.saturating_mul(1000) / u64::from(rate)
        };
        chapter_starts_ms.push(start_ms);
        let frames = (decoded.pcm.len() as u64) / u64::from(ch.max(1));
        cumulative_frames = cumulative_frames.saturating_add(frames);
        all_pcm.extend_from_slice(&decoded.pcm);
    }

    let sample_rate = sample_rate.ok_or_else(|| DecryptError::Native("no sample rate".into()))?;
    let channels = channels.ok_or_else(|| DecryptError::Native("no channel count".into()))?;

    let (samples, asc, _) = encode_pcm_to_aac(&all_pcm, sample_rate, channels)?;
    write_aac_m4b(
        &req.output,
        &MuxAacRequest {
            sample_rate,
            channels,
            asc: &asc,
            samples: &samples,
        },
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

struct DecodedPcm {
    pcm: Vec<i16>,
    sample_rate: u32,
    channels: u16,
}

fn decode_mp3_to_pcm(input: &Path) -> Result<DecodedPcm> {
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

    let mut pcm = Vec::new();
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
        append_pcm_i16(&decoded, channels, &mut pcm);
    }

    if pcm.is_empty() {
        return Err(DecryptError::Native(format!(
            "decoded no PCM from {}",
            input.display()
        )));
    }

    Ok(DecodedPcm {
        pcm,
        sample_rate,
        channels,
    })
}

fn encode_pcm_to_aac(
    pcm: &[i16],
    sample_rate: u32,
    channels: u16,
) -> Result<(Vec<AacAccessUnit>, Vec<u8>, u32)> {
    let channel_mode = match channels {
        1 => ChannelMode::Mono,
        _ => ChannelMode::Stereo,
    };
    // ~64 kbps mono / ~96 kbps stereo — fine for speech audiobooks.
    let bitrate = if channels == 1 { 64_000 } else { 96_000 };

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

    // Ensure stereo encoder gets stereo PCM (duplicate mono if needed).
    let interleaved = match (channels, out_channels) {
        (1, 2) => {
            let mut stereo = Vec::with_capacity(pcm.len() * 2);
            for &s in pcm {
                stereo.push(s);
                stereo.push(s);
            }
            stereo
        }
        (2, 1) => pcm.iter().step_by(2).copied().collect(),
        _ => pcm.to_vec(),
    };

    // Pad to a whole number of encoder frames so we never need EOF flush.
    let mut padded = interleaved;
    let rem = padded.len() % samples_per_frame;
    if rem != 0 {
        padded.resize(padded.len() + (samples_per_frame - rem), 0);
    }

    let mut out_buf = vec![0u8; info.maxOutBufBytes.max(2048) as usize];
    let mut samples = Vec::new();
    let mut offset = 0usize;
    while offset < padded.len() {
        let end = (offset + samples_per_frame).min(padded.len());
        let chunk = &padded[offset..end];
        let enc = encoder
            .encode(chunk, &mut out_buf)
            .map_err(|err| DecryptError::Native(format!("fdk-aac encode failed: {err}")))?;
        offset += enc.input_consumed;
        if enc.output_size > 0 {
            samples.push(AacAccessUnit {
                data: out_buf[..enc.output_size].to_vec(),
                duration: frame_length,
            });
        }
        if enc.input_consumed == 0 {
            // Avoid infinite loop if encoder stalls.
            break;
        }
    }

    if samples.is_empty() {
        return Err(DecryptError::Native(
            "fdk-aac produced no AAC access units".into(),
        ));
    }

    Ok((samples, asc, frame_length))
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
}
