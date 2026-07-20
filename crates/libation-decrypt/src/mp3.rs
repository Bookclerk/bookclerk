//! Native MP3 re-encode via Symphonia (decode) + LAME (encode).

use std::fs::File;
use std::io::Write;
use std::path::Path;

use mp3lame_encoder::{Builder, DualPcm, Encoder, FlushNoGap, InterleavedPcm};
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::error::{DecryptError, Result};
use crate::DecryptOutcome;

/// Re-encode audio to MP3 (classic Libation `DecryptToLossy`).
///
/// Defaults to the source sample rate. When `max_sample_rate` is set lower than
/// the source, PCM is linearly resampled before LAME so the MP3 header matches
/// the encoded PCM rate.
pub fn encode_to_mp3_native(
    input: &Path,
    output: &Path,
    lame: &libation_config::LameConfig,
    max_sample_rate: Option<u32>,
) -> Result<DecryptOutcome> {
    if !input.exists() {
        return Err(DecryptError::InputMissing(input.to_path_buf()));
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    tracing::info!(
        input = %input.display(),
        output = %output.display(),
        "native mp3 encode (symphonia + lame)"
    );

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
        .max(1);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|err| DecryptError::Native(format!("decoder init failed: {err}")))?;

    let out_channels = if lame.downsample_mono || lame.mode.eq_ignore_ascii_case("mono") {
        1u32
    } else {
        channels.min(2) as u32
    };

    // Default: match source. Optional down-sample only when max_sample_rate is lower.
    let target_rate = match max_sample_rate {
        Some(max) if max > 0 && max < sample_rate => {
            tracing::warn!(
                source_hz = sample_rate,
                target_hz = max,
                "max_sample_rate below source; resampling PCM before MP3 encode"
            );
            max
        }
        Some(max) if max > 0 && max > sample_rate => {
            tracing::info!(
                source_hz = sample_rate,
                max_sample_rate = max,
                "max_sample_rate above source; encoding at source rate"
            );
            sample_rate
        }
        _ => sample_rate,
    };

    let mut builder = Builder::new()
        .ok_or_else(|| DecryptError::Native("failed to create LAME encoder (mp3lame)".into()))?;
    builder
        .set_num_channels(out_channels as u8)
        .map_err(|err| DecryptError::Native(format!("lame channels: {err:?}")))?;
    builder
        .set_sample_rate(target_rate)
        .map_err(|err| DecryptError::Native(format!("lame sample rate: {err:?}")))?;

    if lame.constant_bitrate || lame.target.eq_ignore_ascii_case("bitrate") {
        let br = match lame.bitrate_kbps {
            0..=64 => mp3lame_encoder::Bitrate::Kbps64,
            65..=96 => mp3lame_encoder::Bitrate::Kbps96,
            97..=128 => mp3lame_encoder::Bitrate::Kbps128,
            129..=160 => mp3lame_encoder::Bitrate::Kbps160,
            161..=192 => mp3lame_encoder::Bitrate::Kbps192,
            193..=256 => mp3lame_encoder::Bitrate::Kbps256,
            _ => mp3lame_encoder::Bitrate::Kbps320,
        };
        builder
            .set_brate(br)
            .map_err(|err| DecryptError::Native(format!("lame bitrate: {err:?}")))?;
    } else {
        let quality = match lame.vbr_quality.min(9) {
            0 => mp3lame_encoder::Quality::Best,
            1 => mp3lame_encoder::Quality::SecondBest,
            2 => mp3lame_encoder::Quality::NearBest,
            3 => mp3lame_encoder::Quality::VeryNice,
            4 => mp3lame_encoder::Quality::Nice,
            5 => mp3lame_encoder::Quality::Good,
            6 => mp3lame_encoder::Quality::Decent,
            7 => mp3lame_encoder::Quality::Ok,
            8 => mp3lame_encoder::Quality::SecondWorst,
            _ => mp3lame_encoder::Quality::Worst,
        };
        builder
            .set_vbr_mode(mp3lame_encoder::VbrMode::Mtrh)
            .map_err(|err| DecryptError::Native(format!("lame vbr mode: {err:?}")))?;
        builder
            .set_vbr_quality(quality)
            .map_err(|err| DecryptError::Native(format!("lame vbr quality: {err:?}")))?;
    }

    let mut encoder: Encoder = builder
        .build()
        .map_err(|err| DecryptError::Native(format!("lame build: {err:?}")))?;

    let mut out_file = File::create(output)?;
    let mut mp3_chunk = Vec::new();
    let mut decoded_pcm: Vec<i16> = Vec::new();
    let mut encode_pcm: Vec<i16> = Vec::new();
    let mut resampler = (target_rate != sample_rate)
        .then(|| LinearResampler::new(sample_rate, target_rate, out_channels as usize));

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

        decoded_pcm.clear();
        append_pcm_i16(&decoded, out_channels, &mut decoded_pcm);

        if let Some(rs) = resampler.as_mut() {
            rs.push(&decoded_pcm, &mut encode_pcm);
        } else {
            encode_pcm.extend_from_slice(&decoded_pcm);
        }

        drain_encode_chunks(
            &mut encoder,
            &mut encode_pcm,
            out_channels,
            &mut mp3_chunk,
            &mut out_file,
        )?;
    }

    if let Some(rs) = resampler.as_mut() {
        rs.flush(&mut encode_pcm);
    }
    if !encode_pcm.is_empty() {
        encode_pcm_chunk(&mut encoder, &encode_pcm, out_channels, &mut mp3_chunk)?;
        out_file.write_all(&mp3_chunk)?;
        mp3_chunk.clear();
        encode_pcm.clear();
    }

    encoder
        .flush_to_vec::<FlushNoGap>(&mut mp3_chunk)
        .map_err(|err| DecryptError::Native(format!("lame flush: {err:?}")))?;
    if !mp3_chunk.is_empty() {
        out_file.write_all(&mp3_chunk)?;
    }
    out_file.sync_all()?;

    let expected_rate = max_sample_rate
        .filter(|m| *m > 0)
        .map(|m| m.min(sample_rate))
        .unwrap_or(sample_rate);
    if target_rate != expected_rate {
        tracing::warn!(
            source_hz = sample_rate,
            expected_hz = expected_rate,
            encoded_hz = target_rate,
            "MP3 output sample rate does not match configured target"
        );
    }

    if !output.exists() {
        return Err(DecryptError::OutputMissing(output.to_path_buf()));
    }
    Ok(DecryptOutcome {
        output: output.to_path_buf(),
    })
}

fn drain_encode_chunks(
    encoder: &mut Encoder,
    pcm: &mut Vec<i16>,
    channels: u32,
    mp3_chunk: &mut Vec<u8>,
    out_file: &mut File,
) -> Result<()> {
    const CHUNK: usize = 1152 * 8;
    let frame = channels as usize;
    while pcm.len() >= CHUNK * frame {
        let take = CHUNK * frame;
        let chunk: Vec<i16> = pcm.drain(..take).collect();
        encode_pcm_chunk(encoder, &chunk, channels, mp3_chunk)?;
        out_file.write_all(mp3_chunk)?;
        mp3_chunk.clear();
    }
    Ok(())
}

/// Streaming linear resampler for interleaved PCM.
struct LinearResampler {
    channels: usize,
    /// Input-frame advance per output frame (`in_hz / out_hz`).
    step: f64,
    /// Position in `buf` (input frames) for the next output sample.
    pos: f64,
    buf: Vec<i16>,
}

impl LinearResampler {
    fn new(from_hz: u32, to_hz: u32, channels: usize) -> Self {
        Self {
            channels,
            step: f64::from(from_hz) / f64::from(to_hz),
            pos: 0.0,
            buf: Vec::new(),
        }
    }

    fn push(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if self.channels == 0 || input.is_empty() {
            return;
        }
        self.buf.extend_from_slice(input);
        self.emit(out, false);
    }

    fn flush(&mut self, out: &mut Vec<i16>) {
        self.emit(out, true);
        self.buf.clear();
        self.pos = 0.0;
    }

    fn emit(&mut self, out: &mut Vec<i16>, final_flush: bool) {
        let ch = self.channels;
        if ch == 0 {
            return;
        }
        loop {
            let frames = self.buf.len() / ch;
            let i0 = self.pos.floor() as usize;
            let i1 = i0 + 1;
            if i1 >= frames {
                if final_flush && i0 < frames {
                    // Last partial: hold the final input frame.
                    let base = i0 * ch;
                    out.extend_from_slice(&self.buf[base..base + ch]);
                    self.pos += self.step;
                }
                break;
            }
            let frac = self.pos - i0 as f64;
            for c in 0..ch {
                let a = f64::from(self.buf[i0 * ch + c]);
                let b = f64::from(self.buf[i1 * ch + c]);
                let sample = a + (b - a) * frac;
                out.push(sample.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
            }
            self.pos += self.step;
        }

        let consumed = self.pos.floor() as usize;
        if consumed > 0 {
            let max_consume = (self.buf.len() / ch).saturating_sub(1);
            let drop_frames = consumed.min(max_consume);
            if drop_frames > 0 {
                self.buf.drain(..drop_frames * ch);
                self.pos -= drop_frames as f64;
            }
        }
    }
}

fn encode_pcm_chunk(
    encoder: &mut Encoder,
    pcm: &[i16],
    channels: u32,
    out: &mut Vec<u8>,
) -> Result<()> {
    if channels == 1 {
        encoder
            .encode_to_vec(InterleavedPcm(pcm), out)
            .map_err(|err| DecryptError::Native(format!("lame encode: {err:?}")))?;
    } else {
        let frames = pcm.len() / 2;
        let mut left = Vec::with_capacity(frames);
        let mut right = Vec::with_capacity(frames);
        for pair in pcm.chunks_exact(2) {
            left.push(pair[0]);
            right.push(pair[1]);
        }
        encoder
            .encode_to_vec(
                DualPcm {
                    left: &left,
                    right: &right,
                },
                out,
            )
            .map_err(|err| DecryptError::Native(format!("lame encode: {err:?}")))?;
    }
    Ok(())
}

fn append_pcm_i16(buf: &AudioBufferRef<'_>, out_channels: u32, dst: &mut Vec<i16>) {
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
            tracing::warn!(
                frames,
                "unsupported PCM sample format for mp3 encode; inserting silence"
            );
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

    #[test]
    fn linear_resampler_halves_mono_rate() {
        let mut rs = LinearResampler::new(4, 2, 1);
        let mut out = Vec::new();
        rs.push(&[0, 1000, 2000, 3000], &mut out);
        rs.flush(&mut out);
        // 4 input frames @ 4 Hz → ~2 output frames @ 2 Hz (+ possible final hold).
        assert!(out.len() >= 2, "got {} samples: {out:?}", out.len());
        assert!(out.len() <= 4, "got {} samples: {out:?}", out.len());
    }

    #[test]
    fn linear_resampler_step_for_identity() {
        let rs = LinearResampler::new(44100, 44100, 2);
        assert!((rs.step - 1.0).abs() < f64::EPSILON);
    }
}
