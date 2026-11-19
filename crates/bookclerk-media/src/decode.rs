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
//! Demuxing is Symphonia's. Decoding is Symphonia's for every codec but
//! AAC, which goes through FDK ([`crate::aac_fdk`]) so HE-AAC, HE-AAC v2
//! and xHE-AAC decode at their real rate and layout.
//!
//! The PCM layout is the decoder's, not the container's: the stream is
//! primed by decoding up to its first audible packet when it is opened, so
//! an implicitly signalled HE-AAC track the container calls 22.05 kHz mono
//! is reported, and encoded, as the 44.1 kHz stereo it decodes to.

use std::fs::File;
use std::io::Cursor;
use std::path::Path;
use std::sync::OnceLock;

use symphonia::core::audio::{Channels, GenericAudioBufferRef};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::registry::CodecRegistry;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::common::Tier;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;

use crate::aac_fdk::FdkAacDecoder;
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
    /// The decoded sample rate in Hz: the first audible packet's, else the
    /// track's stated rate. A later packet at another rate ends the decode
    /// with an error.
    pub(crate) sample_rate: u32,
    /// The decoded channel count, at least 1: the first audible packet's,
    /// else the track's stated count (1 when it states none).
    pub(crate) channels: usize,
    /// The first audible packet, decoded while priming and handed out
    /// first by [`Self::decode_each`]. A caller that seeks ignores it.
    pub(crate) primed: Primed,
}

/// The PCM of the packet a stream was primed with.
#[derive(Default)]
pub(crate) struct Primed {
    /// Interleaved samples, in the source layout.
    pcm: Vec<i16>,
    /// How the source layout folds to mono or stereo.
    downmix: Downmix,
}

/// How a source channel layout folds to one or two output channels.
///
/// One and two channels mix as before: mono is the mean of
/// the pair, stereo copies them, and a mono source is duplicated. Wider
/// layouts are mixed from every channel by its position: left channels go
/// left, right channels go right, centre channels go to both at -3 dB and
/// LFE is dropped. Each side is normalised by its total weight, so the mix
/// never clips. A layout with no positions (discrete channels) averages
/// every channel into both sides, so no channel is lost.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Downmix {
    /// `(left, right)` weight of each source channel, in plane order; empty
    /// for a mono or stereo source, which needs no weights.
    weights: Vec<(f32, f32)>,
    /// Source channels per frame, at least 1.
    channels: usize,
}

/// Bits of Symphonia's channel positions that sit on the left.
const LEFT_POSITION_BITS: [u32; 9] = [0, 4, 6, 9, 12, 15, 19, 22, 24];
/// Bits of Symphonia's channel positions that sit on the right.
const RIGHT_POSITION_BITS: [u32; 9] = [1, 5, 7, 10, 14, 17, 20, 23, 25];
/// Bits of Symphonia's LFE positions, which carry no speech.
const LFE_POSITION_BITS: [u32; 2] = [3, 18];
/// Gain of a centre channel on each side (-3 dB).
const CENTRE_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;

impl Downmix {
    /// The downmix for a source with `channels`.
    pub(crate) fn for_channels(channels: &Channels) -> Self {
        let count = channels.count().max(1);
        if count <= 2 {
            return Self {
                weights: Vec::new(),
                channels: count,
            };
        }
        let weights = match channels {
            Channels::Positioned(positions) => (0..u32::BITS)
                .filter(|bit| positions.bits() & (1 << bit) != 0)
                .map(|bit| {
                    if LEFT_POSITION_BITS.contains(&bit) {
                        (1.0, 0.0)
                    } else if RIGHT_POSITION_BITS.contains(&bit) {
                        (0.0, 1.0)
                    } else if LFE_POSITION_BITS.contains(&bit) {
                        (0.0, 0.0)
                    } else {
                        (CENTRE_GAIN, CENTRE_GAIN)
                    }
                })
                .collect(),
            // Discrete, ambisonic and labelled channels carry no speaker
            // position, so every channel feeds both sides equally. Symphonia
            // marks `Channels` non-exhaustive; a future layout lands here too.
            _ => vec![(1.0, 1.0); count],
        };
        Self {
            weights,
            channels: count,
        }
    }

    /// Source channels per frame.
    pub(crate) fn channels(&self) -> usize {
        self.channels
    }

    /// The `(left, right)` mix of one wide frame, each normalised by its
    /// side's total weight (an all-zero side takes the other side's mix).
    fn mix_wide(&self, frame: &[i16]) -> (i16, i16) {
        let (mut left, mut right, mut wl, mut wr) = (0f32, 0f32, 0f32, 0f32);
        for (sample, (l, r)) in frame.iter().zip(&self.weights) {
            let sample = f32::from(*sample);
            left += sample * l;
            right += sample * r;
            wl += l;
            wr += r;
        }
        let side = |sum: f32, weight: f32| (weight > 0.0).then(|| sum / weight);
        let (left, right) = match (side(left, wl), side(right, wr)) {
            (Some(l), Some(r)) => (l, r),
            (Some(l), None) => (l, l),
            (None, Some(r)) => (r, r),
            // Only LFE channels: nothing to keep but silence.
            (None, None) => (0.0, 0.0),
        };
        (to_i16(left), to_i16(right))
    }
}

/// Rounds a mixed sample into the i16 range.
#[allow(clippy::cast_possible_truncation)] // Clamped to the i16 range first.
fn to_i16(sample: f32) -> i16 {
    sample
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// The codec registry the decode front uses: Symphonia's enabled decoders,
/// with FDK preferred for AAC.
fn codecs() -> &'static CodecRegistry {
    static REGISTRY: OnceLock<CodecRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut registry);
        registry.register_audio_decoder_at_tier::<FdkAacDecoder>(Tier::Preferred);
        registry
    })
}

/// Opens the audio in `source` for decoding, primed to its first audible
/// packet so the layout reported is the decoder's.
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
/// handles its codec; or a demux or fatal decode error while priming.
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
    let decoder = codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|err| MediaError::Native(format!("decoder init failed: {err}")))?;
    let mut opened = OpenedAudio {
        format,
        decoder,
        track_id: track.id,
        sample_rate,
        channels,
        primed: Primed::default(),
    };
    opened.prime()?;
    Ok(opened)
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

/// What the next packet of the track gave.
enum NextPcm<'a> {
    /// A decoded buffer, possibly empty (decoder delay).
    Buffer(GenericAudioBufferRef<'a>),
    /// A packet skipped: another track's, or one the decoder rejected as
    /// corrupt.
    Skipped,
    /// The end of the stream.
    End,
}

impl OpenedAudio {
    /// Demuxes and decodes the next packet.
    ///
    /// # Errors
    ///
    /// [`MediaError::Native`] on a demux or fatal decode error.
    fn next_pcm(&mut self) -> Result<NextPcm<'_>> {
        let packet = match self.format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => return Ok(NextPcm::End),
            Err(SymphoniaError::ResetRequired) => {
                self.decoder.reset();
                return Ok(NextPcm::Skipped);
            }
            Err(err) => return Err(MediaError::Native(format!("demux error: {err}"))),
        };
        if packet.track_id != self.track_id {
            return Ok(NextPcm::Skipped);
        }
        match self.decoder.decode(&packet) {
            Ok(buf) => Ok(NextPcm::Buffer(buf)),
            Err(SymphoniaError::DecodeError(_)) => Ok(NextPcm::Skipped),
            Err(err) => Err(MediaError::Native(format!("decode error: {err}"))),
        }
    }

    /// Decodes up to the first packet that gives samples, takes the stream
    /// layout from it and keeps its PCM in [`Self::primed`]. A stream with
    /// no audible packet keeps the track's stated layout.
    ///
    /// # Errors
    ///
    /// [`MediaError::Native`] on a demux or fatal decode error.
    fn prime(&mut self) -> Result<()> {
        loop {
            let buf = match self.next_pcm()? {
                NextPcm::Buffer(buf) if buf.frames() > 0 => buf,
                NextPcm::Buffer(_) | NextPcm::Skipped => continue,
                NextPcm::End => return Ok(()),
            };
            let rate = buf.spec().rate();
            if !(1..=MAX_SAMPLE_RATE_HZ).contains(&rate) {
                continue;
            }
            let downmix = Downmix::for_channels(buf.spec().channels());
            let mut pcm = Vec::new();
            buf.copy_to_vec_interleaved(&mut pcm);
            self.sample_rate = rate;
            self.channels = downmix.channels();
            self.primed = Primed { pcm, downmix };
            return Ok(());
        }
    }

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
        let primed = std::mem::take(&mut self.primed);
        if !primed.pcm.is_empty() {
            mix_interleaved(&primed.pcm, &primed.downmix, out_channels, &mut pcm);
            decoded_any = true;
            on_pcm(&pcm)?;
        }
        let rate = self.sample_rate;
        loop {
            let buf = match self.next_pcm()? {
                NextPcm::Buffer(buf) => buf,
                NextPcm::Skipped => continue,
                NextPcm::End => break,
            };
            same_rate(rate, buf.spec().rate(), buf.frames())?;
            pcm.clear();
            append_pcm_i16(&buf, out_channels, &mut interleaved, &mut pcm);
            if !pcm.is_empty() {
                decoded_any = true;
                on_pcm(&pcm)?;
            }
        }
        Ok(decoded_any)
    }
}

/// Checks that a decoded packet keeps the stream's sample rate.
///
/// A packet at another rate cannot join the stream without resampling, and
/// dropping it would cut audio out of a conversion that then reports
/// success, so the decode fails instead. Symphonia's MPEG audio reader
/// already skips frames whose header disagrees with the stream; the case
/// left is a decoder changing its output rate, such as AAC whose SBR
/// extension first appears after the stream starts.
///
/// # Arguments
///
/// * `stream_rate` - The rate the stream was primed at, in Hz.
/// * `packet_rate` - The rate of the decoded packet, in Hz.
/// * `frames` - Frames in the packet; an empty packet is not checked.
///
/// # Errors
///
/// [`MediaError::Native`] naming both rates when they differ.
fn same_rate(stream_rate: u32, packet_rate: u32, frames: usize) -> Result<()> {
    if frames > 0 && packet_rate != stream_rate {
        return Err(MediaError::Native(format!(
            "sample rate changed mid-stream ({stream_rate} Hz to {packet_rate} Hz)"
        )));
    }
    Ok(())
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
    let downmix = Downmix::for_channels(buf.spec().channels());
    mix_interleaved(&interleaved[..need], &downmix, out_channels, dst);
}

/// Appends interleaved PCM in the source layout `downmix` describes to
/// `dst` with `out_channels` channels (1 or 2) ([`Downmix`]). A trailing
/// partial frame is dropped.
pub(crate) fn mix_interleaved(
    src: &[i16],
    downmix: &Downmix,
    out_channels: u16,
    dst: &mut Vec<i16>,
) {
    let in_channels = downmix.channels().max(1);
    let out_ch = usize::from(out_channels.clamp(1, 2));
    if out_ch == in_channels {
        dst.extend_from_slice(&src[..src.len() - src.len() % in_channels]);
        return;
    }
    dst.reserve((src.len() / in_channels).saturating_mul(out_ch));
    for frame in src.chunks_exact(in_channels) {
        let (left, right) = if in_channels > 2 {
            downmix.mix_wide(frame)
        } else {
            let left = frame[0];
            (left, frame.get(1).copied().unwrap_or(left))
        };
        if out_ch == 1 {
            // The mean of two i16 values fits an i16.
            let mean = (i32::from(left) + i32::from(right)) / 2;
            dst.push(i16::try_from(mean).unwrap_or(i16::MAX));
        } else {
            dst.push(left);
            dst.push(right);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia::core::audio::Position;

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

    /// Two seconds of a stereo 44.1 kHz tone, encoded as HE-AAC (or v2).
    fn he_aac_bytes(v2: bool) -> Vec<u8> {
        let dir = tempfile::tempdir().expect("dir");
        let pcm: Vec<i16> = (0..44_100 * 2)
            .flat_map(|i| {
                let t = i as f32 / 44_100.0;
                let left = (f32::sin(t * 440.0 * std::f32::consts::TAU) * 8000.0) as i16;
                let right = (f32::sin(t * 660.0 * std::f32::consts::TAU) * 8000.0) as i16;
                [left, right]
            })
            .collect();
        let out = dir.path().join("he.m4b");
        let bitrate = if v2 { 24_000 } else { 48_000 };
        crate::package_m4b::package_he_aac_m4b_from_pcm(&pcm, 44_100, 2, bitrate, v2, &out)
            .expect("encode");
        std::fs::read(out).expect("read")
    }

    #[test]
    fn he_aac_decodes_at_its_full_rate_and_length() {
        for v2 in [false, true] {
            let mut frames = 0usize;
            let format =
                decode_pcm_bytes(he_aac_bytes(v2), OutChannels::SourceUpToStereo, |f, pcm| {
                    frames += pcm.len() / usize::from(f.channels);
                    Ok(())
                })
                .expect("decode");
            // The container states the 22.05 kHz core (implicit SBR); the
            // decoder's layout wins.
            assert_eq!(
                format,
                PcmFormat {
                    sample_rate: 44_100,
                    channels: 2
                },
                "v2={v2}"
            );
            // Two seconds, give or take encoder padding and decoder delay.
            assert!(
                (88_200..88_200 + 3 * 2048 + 4096).contains(&frames),
                "v2={v2} frames={frames}"
            );
        }
    }

    #[test]
    fn every_input_container_and_codec_decodes() {
        // Quarter-second 16 kHz tones made with ffmpeg; they double as
        // `media_decode` fuzz seeds.
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/media_decode");
        for (name, channels) in [
            ("tone_1ch.flac", 1),
            ("tone_2ch.flac", 2),
            ("tone_2ch_alac.m4a", 2),
            ("tone_1ch.wav", 1),
            ("tone_2ch_adpcm.wav", 2),
            ("tone_2ch.aiff", 2),
            ("tone_1ch.caf", 1),
            ("tone_2ch.mp2", 2),
            ("tone_1ch.aac", 1),
        ] {
            let bytes = std::fs::read(corpus.join(name)).expect(name);
            let mut frames = 0usize;
            let format = decode_pcm_bytes(bytes, OutChannels::SourceUpToStereo, |f, pcm| {
                frames += pcm.len() / usize::from(f.channels);
                Ok(())
            })
            .unwrap_or_else(|err| panic!("{name}: {err}"));
            assert_eq!(
                format,
                PcmFormat {
                    sample_rate: 16_000,
                    channels
                },
                "{name}"
            );
            // 4,000 frames, give or take a codec frame of padding.
            assert!((3_000..=6_000).contains(&frames), "{name}: {frames} frames");
        }
    }

    #[test]
    fn mix_interleaved_folds_and_widens() {
        let discrete = |n| Downmix::for_channels(&Channels::Discrete(n));
        let mut dst = Vec::new();
        mix_interleaved(&[10, 20, 30, 40, 50], &discrete(2), 1, &mut dst);
        assert_eq!(dst, [15, 35]);
        dst.clear();
        mix_interleaved(&[7, 8], &discrete(1), 2, &mut dst);
        assert_eq!(dst, [7, 7, 8, 8]);
        dst.clear();
        // Discrete wide channels all reach both sides.
        mix_interleaved(&[3, 6, 9, 0, 0, 30, 7], &discrete(3), 2, &mut dst);
        assert_eq!(dst, [6, 6, 10, 10]);
        dst.clear();
        mix_interleaved(&[1, 2, 3], &discrete(2), 2, &mut dst);
        assert_eq!(dst, [1, 2]);
    }

    #[test]
    fn a_wide_layout_keeps_its_centre_channel() {
        // 5.1 in plane order: FL, FR, FC, LFE, RL, RR. Speech only in the
        // centre must survive both output modes.
        let layout = Channels::Positioned(
            Position::FRONT_LEFT
                | Position::FRONT_RIGHT
                | Position::FRONT_CENTER
                | Position::LFE1
                | Position::REAR_LEFT
                | Position::REAR_RIGHT,
        );
        let downmix = Downmix::for_channels(&layout);
        let centre_only = [0, 0, 12_000, 0, 0, 0];
        let mut stereo = Vec::new();
        mix_interleaved(&centre_only, &downmix, 2, &mut stereo);
        assert!(stereo.iter().all(|s| *s > 3_000), "{stereo:?}");
        assert_eq!(stereo[0], stereo[1]);
        let mut mono = Vec::new();
        mix_interleaved(&centre_only, &downmix, 1, &mut mono);
        assert!(mono[0] > 3_000, "{mono:?}");
        // LFE alone carries nothing; left stays left.
        let mut out = Vec::new();
        mix_interleaved(&[0, 0, 0, 30_000, 0, 0], &downmix, 2, &mut out);
        assert_eq!(out, [0, 0]);
        out.clear();
        mix_interleaved(&[i16::MAX, 0, 0, 0, i16::MAX, 0], &downmix, 2, &mut out);
        assert!(out[0] > 10_000 && out[1] == 0, "{out:?}");
    }

    #[test]
    fn a_rate_change_mid_stream_fails_the_decode() {
        assert!(same_rate(44_100, 44_100, 1024).is_ok());
        // A packet with no frames carries no rate worth checking.
        assert!(same_rate(44_100, 22_050, 0).is_ok());
        let err = same_rate(22_050, 44_100, 2048).expect_err("rate change");
        assert!(err.to_string().contains("22050 Hz to 44100 Hz"), "{err}");
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
    /// mono and stereo, whole, cut short and with a damaged middle, and
    /// half a second of HE-AAC and HE-AAC v2. The ffmpeg-made seeds for the
    /// other containers are checked in as they are. The
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
        for v2 in [false, true] {
            let pcm: Vec<i16> = tone(2).into_iter().cycle().take(32_000).collect();
            let out = dir.path().join(format!("he{v2}.m4b"));
            crate::package_m4b::package_he_aac_m4b_from_pcm(&pcm, 32_000, 2, 16_000, v2, &out)
                .expect("he-aac");
            let name = if v2 { "he_aac_v2.m4b" } else { "he_aac.m4b" };
            seeds.push((name.into(), std::fs::read(&out).expect("read")));
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
