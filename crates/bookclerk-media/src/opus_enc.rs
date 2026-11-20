//! Opus output: decoded audio encoded by libopus into an Ogg Opus file
//! (RFC 7845).
//!
//! The audio comes from the decode front, mixed to mono or kept up to
//! stereo, and is converted to 48 kHz first unless it is already at a rate
//! Opus encodes (8, 12, 16, 24 or 48 kHz). Packets are 20 ms. The stream's
//! pre-skip is the encoder's look-ahead, and the last page's granule
//! position ends the stream at the last real sample, so a player trims the
//! padding of the final packet.
//!
//! The file gets a minimal `OpusTags` header (vendor string only); the
//! metadata fix-up writes the book's tags, cover and chapters afterwards
//! ([`crate::vorbis_tags`]).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use opusic_c::{Application, Bitrate, Channels as OpusChannels, Encoder, SampleRate};
use serde::{Deserialize, Serialize};

use crate::decode::{open_audio_file, OutChannels};
use crate::error::{MediaError, Result};
use crate::mp3::write_output_within_budget;
use crate::opus_dec::OPUS_RATE_HZ;
use crate::resample::Resampler;
use crate::MediaOutcome;

/// Lowest bitrate an Opus copy is encoded at, in kbit/s.
pub const MIN_OPUS_KBPS: u16 = 6;

/// Highest bitrate an Opus copy is encoded at, in kbit/s (per stream; the
/// Opus limit for stereo).
pub const MAX_OPUS_KBPS: u16 = 510;

/// Packet length, in milliseconds.
const PACKET_MS: u32 = 20;

/// Largest packet libopus is asked to write, in bytes (its recommended
/// ceiling).
const MAX_PACKET_BYTES: usize = 4_000;

/// Packets per page: one second of audio, so seeking reads little and the
/// page overhead stays under a percent.
const PACKETS_PER_PAGE: usize = 50;

/// Serial number of the single logical stream.
const STREAM_SERIAL: u32 = 0x4243_4C4B;

/// Page header flag: the first page of the stream.
const BEGINNING_OF_STREAM: u8 = 0x02;

/// Page header flag: the last page of the stream.
const END_OF_STREAM: u8 = 0x04;

/// How an Opus copy is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpusEncodeOptions {
    /// Target bitrate in kbit/s, between [`MIN_OPUS_KBPS`] and
    /// [`MAX_OPUS_KBPS`] (others are clamped). Opus encodes in variable
    /// bitrate around it.
    pub bitrate_kbps: u16,
    /// Mix down to one channel.
    pub mono: bool,
}

/// The rates libopus encodes at, as its enum.
fn opus_rate(hz: u32) -> Option<SampleRate> {
    match hz {
        8_000 => Some(SampleRate::Hz8000),
        12_000 => Some(SampleRate::Hz12000),
        16_000 => Some(SampleRate::Hz16000),
        24_000 => Some(SampleRate::Hz24000),
        48_000 => Some(SampleRate::Hz48000),
        _ => None,
    }
}

/// A libopus failure as a media error.
fn opus_error(what: &str, err: opusic_c::ErrorCode) -> MediaError {
    MediaError::Native(format!("opus {what}: {}", err.message()))
}

/// Encodes the audio in `input` to an Ogg Opus file at `output`.
///
/// # Arguments
///
/// * `input` - Any file the decode front opens.
/// * `output` - Where the `.opus` file is written (replaced if present).
/// * `options` - Bitrate and channel choice.
/// * `max_output_bytes` - Fail rather than write past this many bytes.
///
/// # Errors
///
/// [`MediaError::InputMissing`] when the input does not exist; what the
/// decode front returns for audio it cannot open or decode;
/// [`MediaError::Native`] when libopus refuses a setting or a frame, the
/// input decodes to nothing, or the byte budget would be passed; an I/O
/// error when the output cannot be written.
pub fn encode_to_opus_native(
    input: &Path,
    output: &Path,
    options: OpusEncodeOptions,
    max_output_bytes: Option<u64>,
) -> Result<MediaOutcome> {
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    tracing::info!(
        input = %input.display(),
        output = %output.display(),
        bitrate_kbps = options.bitrate_kbps,
        mono = options.mono,
        "native opus encode (symphonia + libopus)"
    );
    let mut opened = open_audio_file(input, false)?;
    let layout = if options.mono {
        OutChannels::Mono
    } else {
        OutChannels::SourceUpToStereo
    };
    let format = opened.pcm_format(layout);
    let channels = usize::from(format.channels);
    let (encode_hz, rate) = match opus_rate(format.sample_rate) {
        Some(rate) => (format.sample_rate, rate),
        None => (OPUS_RATE_HZ, SampleRate::Hz48000),
    };
    let opus_channels = if channels == 1 {
        OpusChannels::Mono
    } else {
        OpusChannels::Stereo
    };
    let mut encoder = Encoder::new(opus_channels, rate, Application::Audio)
        .map_err(|err| opus_error("encoder", err))?;
    let kbps = options.bitrate_kbps.clamp(MIN_OPUS_KBPS, MAX_OPUS_KBPS);
    encoder
        .set_bitrate(Bitrate::Value(u32::from(kbps) * 1000))
        .map_err(|err| opus_error("bitrate", err))?;
    encoder
        .set_vbr(true)
        .map_err(|err| opus_error("vbr", err))?;
    encoder
        .set_complexity(10)
        .map_err(|err| opus_error("complexity", err))?;
    let lookahead = encoder
        .get_look_ahead()
        .map_err(|err| opus_error("look-ahead", err))?;
    // Granule positions and the pre-skip count 48 kHz samples whatever the
    // encoder's rate.
    let scale = u64::from(OPUS_RATE_HZ / encode_hz);
    let pre_skip = u16::try_from(u64::from(lookahead) * scale)
        .map_err(|_| MediaError::Native("opus look-ahead longer than a pre-skip holds".into()))?;

    let file = File::create(output)?;
    let mut ogg = OggWriter {
        out: BufWriter::new(file),
        written: 0,
        max: max_output_bytes,
        sequence: 0,
    };
    ogg.page(
        BEGINNING_OF_STREAM,
        0,
        &[&opus_head(format.channels, pre_skip, format.sample_rate)],
    )?;
    ogg.page(0, 0, &[&opus_tags()])?;

    let frame = usize::try_from(encode_hz / (1000 / PACKET_MS)).unwrap_or(960) * channels;
    let mut resampler = (encode_hz != format.sample_rate)
        .then(|| Resampler::new(format.sample_rate, encode_hz, channels));
    let mut stream = PacketStream {
        encoder: &mut encoder,
        frame,
        scale,
        pre_skip: u64::from(pre_skip),
        pending: Vec::new(),
        page: Vec::new(),
        encoded_frames: 0,
        scratch: vec![0u8; MAX_PACKET_BYTES],
    };
    let mut floats = Vec::new();
    let mut converted = Vec::new();
    let decoded = opened.decode_each(layout, |pcm| {
        floats.clear();
        floats.extend(pcm.iter().map(|s| f32::from(*s) / 32_768.0));
        let samples = match resampler.as_mut() {
            Some(rs) => {
                converted.clear();
                rs.push(&floats, &mut converted);
                &converted
            }
            None => &floats,
        };
        stream.push(samples, &mut ogg)
    })?;
    if !decoded {
        return Err(MediaError::Native("input decoded to no audio".into()));
    }
    if let Some(rs) = resampler.as_mut() {
        converted.clear();
        rs.flush(&mut converted);
        stream.push(&converted, &mut ogg)?;
    }
    stream.finish(&mut ogg)?;
    let mut out = ogg.out.into_inner().map_err(|err| err.into_error())?;
    out.flush()?;
    out.sync_all()?;
    Ok(MediaOutcome {
        output: output.to_path_buf(),
    })
}

/// The `OpusHead` identification header (RFC 7845 section 5.1), mapping
/// family 0.
fn opus_head(channels: u16, pre_skip: u16, input_hz: u32) -> Vec<u8> {
    let mut head = b"OpusHead".to_vec();
    head.push(1);
    head.push(u8::try_from(channels).unwrap_or(2));
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&input_hz.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);
    head
}

/// A minimal `OpusTags` header: the libopus vendor string, no comments.
fn opus_tags() -> Vec<u8> {
    let vendor = format!("libopus {}", opusic_c::version());
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&u32::try_from(vendor.len()).unwrap_or(0).to_le_bytes());
    tags.extend_from_slice(vendor.as_bytes());
    tags.extend_from_slice(&0u32.to_le_bytes());
    tags
}

/// Cuts PCM into packets, encodes them and fills pages.
struct PacketStream<'a> {
    /// The libopus encoder.
    encoder: &'a mut Encoder,
    /// Interleaved samples per packet.
    frame: usize,
    /// 48 kHz samples per encoder-rate sample.
    scale: u64,
    /// The stream's pre-skip, in 48 kHz samples.
    pre_skip: u64,
    /// Samples not yet a whole packet.
    pending: Vec<f32>,
    /// Encoded packets waiting for their page.
    page: Vec<Vec<u8>>,
    /// Frames (per channel, at the encoder's rate) encoded so far,
    /// padding excluded.
    encoded_frames: u64,
    /// Buffer libopus writes one packet into.
    scratch: Vec<u8>,
}

impl PacketStream<'_> {
    /// Interleaved channels.
    fn channels(&self) -> usize {
        match self.encoder.channels() {
            OpusChannels::Mono => 1,
            OpusChannels::Stereo => 2,
        }
    }

    /// Queues samples and encodes every whole packet.
    fn push<W: Write>(&mut self, samples: &[f32], ogg: &mut OggWriter<W>) -> Result<()> {
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= self.frame {
            let packet: Vec<f32> = self.pending.drain(..self.frame).collect();
            self.encode(&packet, self.frame, ogg)?;
        }
        Ok(())
    }

    /// Encodes one packet's samples (`real` of them audio, the rest
    /// padding) and writes a page when it is full.
    fn encode<W: Write>(
        &mut self,
        packet: &[f32],
        real: usize,
        ogg: &mut OggWriter<W>,
    ) -> Result<()> {
        let len = self
            .encoder
            .encode_float_to_slice(packet, &mut self.scratch)
            .map_err(|err| opus_error("encode", err))?;
        self.page.push(self.scratch[..len].to_vec());
        let frames = u64::try_from(real / self.channels()).unwrap_or(0);
        self.encoded_frames += frames;
        if self.page.len() >= PACKETS_PER_PAGE {
            self.flush_page(0, ogg)?;
        }
        Ok(())
    }

    /// The granule position after everything encoded so far.
    fn granule(&self) -> u64 {
        self.pre_skip + self.encoded_frames * self.scale
    }

    /// Writes the queued packets as one page.
    fn flush_page<W: Write>(&mut self, flags: u8, ogg: &mut OggWriter<W>) -> Result<()> {
        let packets: Vec<&[u8]> = self.page.iter().map(Vec::as_slice).collect();
        ogg.page(flags, self.granule(), &packets)?;
        self.page.clear();
        Ok(())
    }

    /// Encodes the remaining samples, padded with silence to a whole
    /// packet, plus enough silence to flush the look-ahead, and writes the
    /// last page.
    fn finish<W: Write>(mut self, ogg: &mut OggWriter<W>) -> Result<()> {
        let real = self.pending.len();
        let mut last = std::mem::take(&mut self.pending);
        last.resize(self.frame, 0.0);
        self.encode(&last, real, ogg)?;
        // The decoder's output lags by the pre-skip; one more packet of
        // silence carries the last real samples out of the encoder.
        let silence = vec![0.0; self.frame];
        self.encode(&silence, 0, ogg)?;
        self.flush_page(END_OF_STREAM, ogg)
    }
}

/// Writes Ogg pages of the single stream.
struct OggWriter<W: Write> {
    /// The file.
    out: W,
    /// Bytes written so far.
    written: u64,
    /// Byte budget, when the caller set one.
    max: Option<u64>,
    /// The next page sequence number.
    sequence: u32,
}

impl<W: Write> OggWriter<W> {
    /// Writes one page holding `packets`, each ending on it.
    ///
    /// # Errors
    ///
    /// [`MediaError::Native`] when the packets need more than 255 lacing
    /// values or the page would pass the byte budget; an I/O error.
    fn page(&mut self, flags: u8, granule: u64, packets: &[&[u8]]) -> Result<()> {
        let mut lacing = Vec::new();
        for packet in packets {
            lacing.extend(std::iter::repeat_n(255u8, packet.len() / 255));
            lacing.push(u8::try_from(packet.len() % 255).unwrap_or(0));
        }
        let segments = u8::try_from(lacing.len())
            .map_err(|_| MediaError::Native("Ogg page needs too many segments".into()))?;
        let mut page = b"OggS\0".to_vec();
        page.push(flags);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&STREAM_SERIAL.to_le_bytes());
        page.extend_from_slice(&self.sequence.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes());
        page.push(segments);
        page.extend_from_slice(&lacing);
        for packet in packets {
            page.extend_from_slice(packet);
        }
        let crc = ogg_crc(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        write_output_within_budget(&mut self.out, &page, &mut self.written, self.max)?;
        self.sequence = self.sequence.wrapping_add(1);
        Ok(())
    }
}

/// The Ogg page checksum: CRC-32, polynomial `0x04C11DB7`, unreflected,
/// zero initial value and no final XOR.
fn ogg_crc(bytes: &[u8]) -> u32 {
    /// The table for one byte at a time, built at compile time.
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut index = 0;
        while index < 256 {
            let mut crc = (index as u32) << 24;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04C1_1DB7
                } else {
                    crc << 1
                };
                bit += 1;
            }
            table[index] = crc;
            index += 1;
        }
        table
    };
    bytes.iter().fold(0u32, |crc, byte| {
        (crc << 8) ^ TABLE[usize::from(u8::try_from(crc >> 24).unwrap_or(0) ^ byte)]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode_pcm_bytes;

    #[test]
    fn the_page_checksum_matches_the_reference() {
        // An empty page header from the Ogg specification's examples
        // checks to its own CRC; here a known vector: "OggS" alone.
        assert_eq!(ogg_crc(b""), 0);
        // Verified against libogg's ogg_page_checksum_set.
        let page = bookclerk_tags::fixtures::ogg_page(2, 0, 1, &[b"OpusHead"]);
        let mut zeroed = page.clone();
        zeroed[22..26].fill(0);
        assert_eq!(ogg_crc(&zeroed).to_le_bytes(), page[22..26]);
    }

    /// Encodes a corpus file and decodes the result back.
    fn round_trip(name: &str, options: OpusEncodeOptions) -> (Vec<u8>, usize, crate::PcmFormat) {
        let dir = tempfile::tempdir().expect("dir");
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/media_decode");
        let out = dir.path().join("book.opus");
        encode_to_opus_native(&corpus.join(name), &out, options, None).expect("encode");
        let bytes = std::fs::read(&out).expect("read");
        let mut frames = 0usize;
        let format = decode_pcm_bytes(bytes.clone(), OutChannels::SourceUpToStereo, |f, pcm| {
            frames += pcm.len() / usize::from(f.channels);
            Ok(())
        })
        .expect("decode");
        (bytes, frames, format)
    }

    #[test]
    fn a_tone_round_trips_at_full_length() {
        let (bytes, frames, format) = round_trip(
            "tone_2ch.flac",
            OpusEncodeOptions {
                bitrate_kbps: 32,
                mono: false,
            },
        );
        assert_eq!(format.sample_rate, 48_000);
        assert_eq!(format.channels, 2);
        // A quarter second at 48 kHz, plus at most the pre-skip and padding
        // (the decode front does not trim them).
        assert!(
            (12_000..=12_000 + 312 + 2 * 960).contains(&frames),
            "{frames}"
        );
        let record = bookclerk_tags::read_all(&bytes).expect("tags");
        assert_eq!(record.codec, Some(bookclerk_tags::AudioCodec::Opus));
        // The granule position ends the stream at the last real sample.
        assert_eq!(record.duration.map(|d| d.ms), Some(250));
    }

    #[test]
    fn mono_and_resampled_sources_encode() {
        let (_, frames, format) = round_trip(
            "tone_1ch.mp3",
            OpusEncodeOptions {
                bitrate_kbps: 16,
                mono: true,
            },
        );
        assert_eq!(format.channels, 1);
        assert!(frames > 0);
    }

    #[test]
    fn the_byte_budget_is_enforced() {
        let dir = tempfile::tempdir().expect("dir");
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/media_decode");
        let err = encode_to_opus_native(
            &corpus.join("tone_2ch.flac"),
            &dir.path().join("book.opus"),
            OpusEncodeOptions {
                bitrate_kbps: 64,
                mono: false,
            },
            Some(100),
        )
        .expect_err("budget");
        assert!(err.to_string().contains("budget"), "{err}");
    }
}
