//! Native MP3 re-encode via Symphonia (decode) + LAME (encode).

use std::fs::File;
use std::io::Write;
use std::path::Path;

use mp3lame_encoder::{Builder, DualPcm, Encoder, FlushNoGap, InterleavedPcm};
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::error::{MediaError, Result};
use crate::MediaOutcome;

/// Re-encode audio to MP3 (classic Libation `DecryptToLossy`).
///
/// Defaults to the source sample rate. When `max_sample_rate` is set lower than
/// the source, PCM is linearly resampled before LAME so the MP3 header matches
/// the encoded PCM rate.
pub fn encode_to_mp3_native(
    input: &Path,
    output: &Path,
    lame: &bookclerk_config::LameConfig,
    max_sample_rate: Option<u32>,
) -> Result<MediaOutcome> {
    if !input.exists() {
        return Err(MediaError::InputMissing(input.to_path_buf()));
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

    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|err| MediaError::Native(format!("probe failed: {err}")))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| MediaError::Native("no decodable audio track".into()))?
        .clone();
    let track_id = track.id;
    let audio_params = match track.codec_params.as_ref() {
        Some(CodecParameters::Audio(params)) => params.clone(),
        _ => {
            return Err(MediaError::Native(
                "selected track is missing audio codec parameters".into(),
            ));
        }
    };
    let sample_rate = audio_params
        .sample_rate
        .ok_or_else(|| MediaError::Native("missing sample rate".into()))?;
    let channels = audio_params
        .channels
        .as_ref()
        .map(symphonia::core::audio::Channels::count)
        .unwrap_or(2)
        .max(1);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|err| MediaError::Native(format!("decoder init failed: {err}")))?;

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
        .ok_or_else(|| MediaError::Native("failed to create LAME encoder (mp3lame)".into()))?;
    builder
        .set_num_channels(out_channels as u8)
        .map_err(|err| MediaError::Native(format!("lame channels: {err:?}")))?;
    builder
        .set_sample_rate(target_rate)
        .map_err(|err| MediaError::Native(format!("lame sample rate: {err:?}")))?;

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
            .map_err(|err| MediaError::Native(format!("lame bitrate: {err:?}")))?;
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
            .map_err(|err| MediaError::Native(format!("lame vbr mode: {err:?}")))?;
        builder
            .set_vbr_quality(quality)
            .map_err(|err| MediaError::Native(format!("lame vbr quality: {err:?}")))?;
    }

    let mut encoder: Encoder = builder
        .build()
        .map_err(|err| MediaError::Native(format!("lame build: {err:?}")))?;

    let mut out_file = File::create(output)?;
    let mut mp3_chunk = Vec::new();
    let mut decoded_pcm: Vec<i16> = Vec::new();
    let mut encode_pcm: Vec<i16> = Vec::new();
    // Reused across packets so decode does not allocate per AU.
    let mut interleaved_scratch: Vec<i16> = Vec::new();
    let mut resampler = (target_rate != sample_rate)
        .then(|| LinearResampler::new(sample_rate, target_rate, out_channels as usize));

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(err) => {
                return Err(MediaError::Native(format!("demux error: {err}")));
            }
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(buf) => buf,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(err) => {
                return Err(MediaError::Native(format!("decode error: {err}")));
            }
        };

        decoded_pcm.clear();
        append_pcm_i16(
            &decoded,
            out_channels,
            &mut interleaved_scratch,
            &mut decoded_pcm,
        );

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

    // Same contract as encode_pcm_chunk: the flush needs real spare capacity,
    // at minimum one whole MP3 frame.
    mp3_chunk.reserve(mp3lame_encoder::max_required_buffer_size(0));
    encoder
        .flush_to_vec::<FlushNoGap>(&mut mp3_chunk)
        .map_err(|err| MediaError::Native(format!("lame flush: {err:?}")))?;
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
        return Err(MediaError::OutputMissing(output.to_path_buf()));
    }
    Ok(MediaOutcome {
        output: output.to_path_buf(),
    })
}

/// Encodes 1152×8-frame PCM chunks through LAME and appends MP3 bytes to `out_file`.
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
    /// Interleaved channel count; `0` makes `push`/`emit` no-ops.
    channels: usize,
    /// Input-frame advance per output frame (`in_hz / out_hz`).
    step: f64,
    /// Position in `buf` (input frames) for the next output sample.
    pos: f64,
    /// Pending interleaved input samples not yet consumed by `pos`.
    buf: Vec<i16>,
}

impl LinearResampler {
    /// Builds a linear resampler from `from_hz` to `to_hz` for interleaved PCM.
    fn new(from_hz: u32, to_hz: u32, channels: usize) -> Self {
        Self {
            channels,
            step: f64::from(from_hz) / f64::from(to_hz),
            pos: 0.0,
            buf: Vec::new(),
        }
    }

    /// Appends interleaved input and emits as many output frames as the buffer allows.
    fn push(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if self.channels == 0 || input.is_empty() {
            return;
        }
        self.buf.extend_from_slice(input);
        self.emit(out, false);
    }

    /// Emits a held last frame if needed, then clears the input buffer and position.
    fn flush(&mut self, out: &mut Vec<i16>) {
        self.emit(out, true);
        self.buf.clear();
        self.pos = 0.0;
    }

    /// Linear-interpolates output frames; on final flush, holds the last input frame.
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

/// Encode one PCM chunk, appending MP3 bytes to `out`.
///
/// `encode_to_vec` writes into `out`'s *spare capacity* and passes that length
/// to LAME. LAME reads a length of zero as "caller guarantees the buffer is big
/// enough" and writes regardless, so calling it on a `Vec` with no spare room
/// corrupts the heap instead of returning an error. Reserving up front is not
/// an optimisation here — it is what keeps the call in bounds.
fn encode_pcm_chunk(
    encoder: &mut Encoder,
    pcm: &[i16],
    channels: u32,
    out: &mut Vec<u8>,
) -> Result<()> {
    out.reserve(mp3lame_encoder::max_required_buffer_size(pcm.len()));
    if channels == 1 {
        encoder
            .encode_to_vec(InterleavedPcm(pcm), out)
            .map_err(|err| MediaError::Native(format!("lame encode: {err:?}")))?;
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
            .map_err(|err| MediaError::Native(format!("lame encode: {err:?}")))?;
    }
    Ok(())
}

/// Copies or down/up-mixes a Symphonia buffer into interleaved i16 at `out_channels`.
fn append_pcm_i16(
    buf: &GenericAudioBufferRef<'_>,
    out_channels: u32,
    interleaved: &mut Vec<i16>,
    dst: &mut Vec<i16>,
) {
    let frames = buf.frames();
    if frames == 0 {
        return;
    }
    let in_channels = buf.spec().channels().count().max(1);
    let out_ch = out_channels.max(1) as usize;

    // Common path: layout already matches — copy straight into `dst` (no scratch).
    if (out_ch == 1 && in_channels == 1) || (out_ch == 2 && in_channels == 2) {
        let start = dst.len();
        let need = frames.saturating_mul(in_channels);
        dst.resize(start + need, 0);
        buf.copy_to_slice_interleaved(&mut dst[start..]);
        return;
    }

    let need = frames.saturating_mul(in_channels);
    interleaved.resize(need, 0);
    buf.copy_to_slice_interleaved(&mut interleaved[..need]);

    let mix_channels = in_channels.min(2);
    let out_samples = if out_ch == 1 {
        frames
    } else {
        frames.saturating_mul(2)
    };
    dst.reserve(out_samples);
    if out_ch == 1 {
        for frame in interleaved[..need].chunks_exact(in_channels) {
            let mut acc = 0i32;
            for sample in frame.iter().take(mix_channels) {
                acc += i32::from(*sample);
            }
            dst.push((acc / mix_channels as i32) as i16);
        }
    } else {
        for frame in interleaved[..need].chunks_exact(in_channels) {
            let l = frame[0];
            let r = if mix_channels > 1 { frame[1] } else { l };
            dst.push(l);
            dst.push(r);
        }
    }
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

    /// Encoding into a `Vec` with no spare capacity used to hand LAME a buffer
    /// length of zero, which it reads as "no bounds check" — the encoder then
    /// wrote past the allocation and the process died with SIGSEGV. Nothing
    /// covered a full encode, so it went unnoticed.
    ///
    /// A regression here aborts the test process rather than failing an
    /// assertion, which is exactly the signal we want.
    #[test]
    fn encodes_a_full_file_without_overrunning_the_output_buffer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.m4b");
        let encoded = dir.path().join("encoded.mp3");

        let sample_rate = 44_100usize;
        let pcm: Vec<i16> = (0..sample_rate * 2)
            .map(|n| {
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let value = ((n as f32 / 40.0).sin() * 2_000.0) as i16;
                value
            })
            .collect();
        crate::package_m4b_from_pcm(
            &pcm,
            u32::try_from(sample_rate).expect("sample rate fits u32"),
            1,
            &source,
            &[("One".to_string(), 0)],
        )
        .expect("build source audiobook");

        encode_to_mp3_native(
            &source,
            &encoded,
            &bookclerk_config::LameConfig::default(),
            None,
        )
        .expect("encode to mp3");

        let size = std::fs::metadata(&encoded).expect("encoded file").len();
        assert!(
            size > 1_000,
            "encoded MP3 is implausibly small: {size} bytes"
        );
    }

    /// The buffer contract also has to hold when the resampler is in play,
    /// since that path feeds differently-sized chunks to the encoder.
    #[test]
    fn encodes_with_downsampling_without_overrunning_the_output_buffer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.m4b");
        let encoded = dir.path().join("encoded.mp3");

        let sample_rate = 44_100usize;
        let pcm: Vec<i16> = (0..sample_rate)
            .map(|n| {
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let value = ((n as f32 / 40.0).sin() * 2_000.0) as i16;
                value
            })
            .collect();
        crate::package_m4b_from_pcm(
            &pcm,
            u32::try_from(sample_rate).expect("sample rate fits u32"),
            1,
            &source,
            &[("One".to_string(), 0)],
        )
        .expect("build source audiobook");

        encode_to_mp3_native(
            &source,
            &encoded,
            &bookclerk_config::LameConfig::default(),
            Some(22_050),
        )
        .expect("encode to mp3 at a lower rate");

        assert!(std::fs::metadata(&encoded).expect("encoded file").len() > 500);
    }
}
