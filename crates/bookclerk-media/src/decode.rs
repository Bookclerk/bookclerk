//! The one place untrusted audio is demuxed and decoded to PCM.
//!
//! Every codec job that reads audio (MP3 encode, M4B packaging, chapter
//! alignment) opens its input here. The container and codec are found by
//! sniffing the bytes, never from the file name, so a file named `.mp3` that
//! holds an MP4 is read as an MP4, and a name cannot steer which parser runs.
//!
//! Decoding is a stream: each packet's PCM is handed to the caller and
//! dropped, so a long book is never held in memory. The same entry point
//! ([`decode_pcm_bytes`]) drives the `media_decode` fuzz target and its
//! corpus replay test, so the fuzzer exercises the code the worker runs.
//!
//! Today this is Symphonia with its AAC-LC, MP3 and PCM decoders and its
//! MP4 demuxer (the crate's enabled features). More codecs join here.

use std::fs::File;
use std::io::Cursor;
use std::path::Path;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;

use crate::error::{MediaError, Result};

/// Highest sample rate a decoded stream may report, in Hz. Anything above
/// is refused as malformed rather than handed to an encoder.
pub const MAX_SAMPLE_RATE_HZ: u32 = 768_000;

/// The PCM layout of a decoded stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    /// Samples per second per channel, in Hz; between 1 and
    /// [`MAX_SAMPLE_RATE_HZ`].
    pub sample_rate: u32,
    /// Interleaved channels in every chunk: 1 (mono) or 2 (stereo).
    pub channels: u16,
}

/// How many channels the caller wants decoded PCM in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutChannels {
    /// The source's own count, folded to stereo when it has more than two.
    SourceUpToStereo,
    /// One channel: the average of the source's first two.
    Mono,
}

/// An audio stream opened for decoding: its demuxer, decoder and the
/// properties of the audio track it chose.
pub(crate) struct OpenedAudio {
    /// The demuxer, positioned at the first packet.
    pub(crate) format: Box<dyn FormatReader>,
    /// The decoder for [`Self::track_id`].
    pub(crate) decoder: Box<dyn AudioDecoder>,
    /// The audio track decoded; packets of other tracks are skipped.
    pub(crate) track_id: u32,
    /// The track's sample rate in Hz.
    pub(crate) sample_rate: u32,
    /// The track's channel count, at least 1 (1 when the track omits it).
    pub(crate) channels: usize,
}

/// Opens the audio in `source` for decoding.
///
/// # Arguments
///
/// * `source` - The whole media file.
/// * `seek_index` - Build a seek index up front, for callers that seek.
///
/// # Errors
///
/// [`MediaError::Native`] when no known container is found, it has no
/// audio track, the track states no usable sample rate, or no decoder
/// handles its codec.
pub(crate) fn open_audio(source: Box<dyn MediaSource>, seek_index: bool) -> Result<OpenedAudio> {
    let mss = MediaSourceStream::new(source, Default::default());
    // No extension hint: the probe recognises the container by its bytes.
    let format = symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            mss,
            FormatOptions::default().prebuild_seek_index(seek_index),
            MetadataOptions::default(),
        )
        .map_err(|err| MediaError::Native(format!("probe failed: {err}")))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| MediaError::Native("no decodable audio track".into()))?
        .clone();
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
        .filter(|rate| (1..=MAX_SAMPLE_RATE_HZ).contains(rate))
        .ok_or_else(|| MediaError::Native("missing or unusable sample rate".into()))?;
    let channels = audio_params
        .channels
        .as_ref()
        .map(symphonia::core::audio::Channels::count)
        .unwrap_or(1)
        .max(1);
    let decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|err| MediaError::Native(format!("decoder init failed: {err}")))?;
    Ok(OpenedAudio {
        format,
        decoder,
        track_id: track.id,
        sample_rate,
        channels,
    })
}

/// Opens the audio file at `path` for decoding ([`open_audio`]).
///
/// # Errors
///
/// [`MediaError::InputMissing`] when the file does not exist, an I/O error
/// when it cannot be opened, or what [`open_audio`] returns.
pub(crate) fn open_audio_file(path: &Path, seek_index: bool) -> Result<OpenedAudio> {
    if !path.exists() {
        return Err(MediaError::InputMissing(path.to_path_buf()));
    }
    open_audio(Box::new(File::open(path)?), seek_index)
}

impl OpenedAudio {
    /// The layout PCM comes out in when decoded to `out`.
    pub(crate) fn pcm_format(&self, out: OutChannels) -> PcmFormat {
        let channels = match out {
            OutChannels::Mono => 1,
            OutChannels::SourceUpToStereo if self.channels >= 2 => 2,
            OutChannels::SourceUpToStereo => 1,
        };
        PcmFormat {
            sample_rate: self.sample_rate,
            channels,
        }
    }

    /// Decodes every packet of the track to interleaved 16-bit PCM in the
    /// [`Self::pcm_format`] for `out`, calling `on_pcm` once per packet that
    /// gave samples.
    ///
    /// A packet the decoder rejects as corrupt is skipped, as players do;
    /// a demuxer error ends the stream with an error.
    ///
    /// # Returns
    ///
    /// Whether any PCM was produced.
    ///
    /// # Errors
    ///
    /// [`MediaError::Native`] on a demux or fatal decode error, or what
    /// `on_pcm` returns.
    pub(crate) fn decode_each<F>(&mut self, out: OutChannels, mut on_pcm: F) -> Result<bool>
    where
        F: FnMut(&[i16]) -> Result<()>,
    {
        let out_channels = self.pcm_format(out).channels;
        let mut pcm = Vec::new();
        // Reused across packets so decode does not allocate per access unit.
        let mut interleaved = Vec::new();
        let mut decoded_any = false;
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => break,
                Err(SymphoniaError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(err) => return Err(MediaError::Native(format!("demux error: {err}"))),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(buf) => buf,
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(err) => return Err(MediaError::Native(format!("decode error: {err}"))),
            };
            pcm.clear();
            append_pcm_i16(&decoded, out_channels, &mut interleaved, &mut pcm);
            if !pcm.is_empty() {
                decoded_any = true;
                on_pcm(&pcm)?;
            }
        }
        Ok(decoded_any)
    }
}

/// Decodes a whole media file held in memory to PCM, the path the
/// `media_decode` fuzz target and its replay test drive.
///
/// # Arguments
///
/// * `bytes` - The file.
/// * `out` - The channel layout wanted.
/// * `on_pcm` - Called once per decoded packet with the stream's layout and
///   that packet's interleaved samples (a whole number of frames).
///
/// # Errors
///
/// [`MediaError::Native`] when no known container or audio track is found,
/// no decoder handles the codec, the demuxer fails, or the file decodes to
/// no PCM at all; or what `on_pcm` returns.
pub fn decode_pcm_bytes<F>(bytes: Vec<u8>, out: OutChannels, mut on_pcm: F) -> Result<PcmFormat>
where
    F: FnMut(PcmFormat, &[i16]) -> Result<()>,
{
    let mut opened = open_audio(Box::new(Cursor::new(bytes)), false)?;
    let format = opened.pcm_format(out);
    if !opened.decode_each(out, |pcm| on_pcm(format, pcm))? {
        return Err(MediaError::Native("decoded no PCM".into()));
    }
    Ok(format)
}

/// Appends a decoded buffer to `dst` as interleaved i16 with `out_channels`
/// channels (1 or 2): copied when the layouts match, otherwise mixed from
/// the source's first two channels.
pub(crate) fn append_pcm_i16(
    buf: &GenericAudioBufferRef<'_>,
    out_channels: u16,
    interleaved: &mut Vec<i16>,
    dst: &mut Vec<i16>,
) {
    let frames = buf.frames();
    if frames == 0 {
        return;
    }
    let in_channels = buf.spec().channels().count().max(1);
    let out_ch = usize::from(out_channels.clamp(1, 2));

    // Common path: layout already matches, so copy straight into `dst`.
    if out_ch == in_channels {
        let start = dst.len();
        dst.resize(start + frames.saturating_mul(in_channels), 0);
        buf.copy_to_slice_interleaved(&mut dst[start..]);
        return;
    }

    let need = frames.saturating_mul(in_channels);
    interleaved.resize(need, 0);
    buf.copy_to_slice_interleaved(&mut interleaved[..need]);

    let mix_channels = in_channels.min(2);
    dst.reserve(frames.saturating_mul(out_ch));
    for frame in interleaved[..need].chunks_exact(in_channels) {
        if out_ch == 1 {
            let sum: i32 = frame.iter().take(mix_channels).map(|s| i32::from(*s)).sum();
            // The mean of at most two i16 values fits an i16.
            let mean = sum / if mix_channels > 1 { 2 } else { 1 };
            dst.push(i16::try_from(mean).unwrap_or(i16::MAX));
        } else {
            let left = frame[0];
            let right = if mix_channels > 1 { frame[1] } else { left };
            dst.push(left);
            dst.push(right);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short MP3 encoded from a tone by the crate's own encoder.
    fn tone_mp3(dir: &Path, channels: u16) -> std::path::PathBuf {
        let pcm: Vec<i16> = (0..44_100 / 4)
            .flat_map(|i| {
                let sample = (f32::sin(i as f32 * 0.05) * 8000.0) as i16;
                std::iter::repeat_n(sample, usize::from(channels))
            })
            .collect();
        let m4b = dir.join("tone.m4b");
        crate::package_m4b::package_m4b_from_pcm(&pcm, 44_100, channels, &m4b, &[])
            .expect("package");
        m4b
    }

    #[test]
    fn a_file_named_mp3_that_holds_mp4_still_decodes() {
        let dir = tempfile::tempdir().expect("dir");
        let m4b = tone_mp3(dir.path(), 2);
        let misnamed = dir.path().join("really-an-mp4.mp3");
        std::fs::rename(&m4b, &misnamed).expect("rename");
        let mut opened = open_audio_file(&misnamed, false).expect("open");
        let mut samples = 0usize;
        assert!(opened
            .decode_each(OutChannels::SourceUpToStereo, |pcm| {
                samples += pcm.len();
                Ok(())
            })
            .expect("decode"));
        assert!(samples > 0);
    }

    #[test]
    fn bytes_decode_to_whole_frames_in_the_layout_asked_for() {
        let dir = tempfile::tempdir().expect("dir");
        let bytes = std::fs::read(tone_mp3(dir.path(), 2)).expect("read");
        for (out, channels) in [(OutChannels::SourceUpToStereo, 2), (OutChannels::Mono, 1)] {
            let format = decode_pcm_bytes(bytes.clone(), out, |format, pcm| {
                assert_eq!(pcm.len() % usize::from(format.channels), 0);
                Ok(())
            })
            .expect("decode");
            assert_eq!(
                format,
                PcmFormat {
                    sample_rate: 44_100,
                    channels
                }
            );
        }
    }

    #[test]
    fn garbage_and_empty_input_are_refused() {
        for bytes in [Vec::new(), b"definitely not audio".to_vec()] {
            assert!(decode_pcm_bytes(bytes, OutChannels::Mono, |_, _| Ok(())).is_err());
        }
    }

    #[test]
    fn a_missing_file_is_named() {
        let err = open_audio_file(Path::new("/no/such/file.m4b"), false)
            .err()
            .expect("missing");
        assert!(matches!(err, MediaError::InputMissing(_)), "{err}");
    }

    #[test]
    fn errors_from_the_callback_stop_the_decode() {
        let dir = tempfile::tempdir().expect("dir");
        let bytes = std::fs::read(tone_mp3(dir.path(), 1)).expect("read");
        let mut calls = 0;
        let err = decode_pcm_bytes(bytes, OutChannels::Mono, |_, _| {
            calls += 1;
            Err(MediaError::Native("stop".into()))
        });
        assert!(err.is_err());
        assert_eq!(calls, 1);
    }

    /// Writes the generated `media_decode` seeds when `UPDATE_FUZZ_SEEDS` is
    /// set: real AAC-in-MP4 and MP3 files from the crate's own encoders,
    /// mono and stereo, whole, cut short and with a damaged middle. The
    /// replay test in `tests/fuzz_corpus.rs` checks every seed.
    #[test]
    fn generate_fuzz_seeds() {
        if std::env::var_os("UPDATE_FUZZ_SEEDS").is_none() {
            return;
        }
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/media_decode");
        let dir = tempfile::tempdir().expect("dir");
        let tone = |channels: u16| -> Vec<i16> {
            (0..8_000 / 2)
                .flat_map(|i| {
                    let sample = (f32::sin(i as f32 * 0.07) * 6000.0) as i16;
                    std::iter::repeat_n(sample, usize::from(channels))
                })
                .collect()
        };
        let mut seeds: Vec<(String, Vec<u8>)> = Vec::new();
        for channels in [1u16, 2] {
            let m4b = dir.path().join(format!("tone{channels}.m4b"));
            crate::package_m4b::package_m4b_from_pcm(&tone(channels), 8_000, channels, &m4b, &[])
                .expect("package");
            let m4b_bytes = std::fs::read(&m4b).expect("read");
            let mp3 = dir.path().join(format!("tone{channels}.mp3"));
            let lame = bookclerk_config::LameConfig {
                bitrate_kbps: 32,
                constant_bitrate: true,
                ..Default::default()
            };
            crate::mp3::encode_to_mp3_native(&m4b, &mp3, &lame, None, None).expect("mp3");
            let mp3_bytes = std::fs::read(&mp3).expect("read");
            for (ext, bytes) in [("m4b", m4b_bytes), ("mp3", mp3_bytes)] {
                let mut damaged = bytes.clone();
                let mid = damaged.len() / 2;
                for byte in &mut damaged[mid..(mid + 64).min(bytes.len())] {
                    *byte ^= 0x5A;
                }
                seeds.push((format!("tone_{channels}ch.{ext}"), bytes.clone()));
                seeds.push((
                    format!("cut_{channels}ch.{ext}"),
                    bytes[..bytes.len() / 3].to_vec(),
                ));
                seeds.push((format!("damaged_{channels}ch.{ext}"), damaged));
            }
        }
        seeds.push((
            "not_audio.txt".into(),
            b"just some text, not audio".to_vec(),
        ));
        for (name, bytes) in seeds {
            std::fs::write(corpus.join(name), bytes).expect("write seed");
        }
    }
}
