//! The FFmpeg decoders (`bookclerk-libav`) plugged into Symphonia, for
//! builds with the `libav` feature.
//!
//! Two pieces join the decode front ([`crate::decode`]):
//!
//! - [`LibavDecoder`], a Symphonia decoder for AC-3 and E-AC-3 tracks
//!   that Symphonia's own MP4 reader finds, and for every track
//!   [`LibavReader`] reads (whose parameters travel in a private codec id,
//!   [`CODEC_ID_LIBAV`], because WMA needs fields Symphonia's parameters
//!   have no place for).
//! - [`LibavReader`], a Symphonia format reader over FFmpeg's demuxers, for
//!   the containers Symphonia does not read: ASF (WMA), raw AC-3/E-AC-3,
//!   raw AMR, and MP4/3GP files whose audio Symphonia has no decoder for
//!   (AMR in 3GP).
//!
//! Both run inside the media worker's confinement like every other
//! decoder; nothing here opens files or networks.

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex, PoisonError};

use bookclerk_libav::{Codec, CodecConfig, Decoder, Demuxer, Error as LibavError};
use symphonia::core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioSpec, Channels, GenericAudioBufferRef,
    Position,
};
use symphonia::core::codecs::audio::well_known::{CODEC_ID_AC3, CODEC_ID_EAC3};
use symphonia::core::codecs::audio::{
    AudioCodecId, AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia::core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia::core::codecs::{CodecInfo, CodecParameters};
use symphonia::core::common::FourCc;
use symphonia::core::errors::{decode_error, seek_error, unsupported_error, Result, SeekErrorKind};
use symphonia::core::formats::{
    FormatId, FormatInfo, FormatReader, SeekMode, SeekTo, SeekedTo, Track, TrackFlags,
};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::{Metadata, MetadataLog};
use symphonia::core::packet::{Packet, PacketRef};
use symphonia::core::units::{Duration, TimeBase, Timestamp};

use crate::decode::{open_reader, probe, Opened, OpenedAudio, MAX_SAMPLE_RATE_HZ};
use crate::error::MediaError;

/// The private codec id of tracks [`LibavReader`] reads; their extra data
/// is a [`Blob`].
pub(crate) const CODEC_ID_LIBAV: AudioCodecId = AudioCodecId::new(FourCc::new(*b"bclv"));

/// The format id [`LibavReader`] reports.
const FORMAT_ID_LIBAV: FormatId = FormatId::new(FourCc::new(*b"bclv"));

/// What [`LibavReader::format_info`] answers.
const FORMAT_INFO: FormatInfo = FormatInfo {
    format: FORMAT_ID_LIBAV,
    short_name: "libav",
    long_name: "FFmpeg demuxers (ASF, raw AC-3/E-AC-3, raw AMR, 3GP)",
};

/// The codecs [`LibavDecoder`] registers for.
const SUPPORTED: &[SupportedAudioCodec] = &[
    SupportedAudioCodec {
        id: CODEC_ID_AC3,
        info: CodecInfo {
            short_name: "ac3",
            long_name: "Dolby Digital (FFmpeg)",
            profiles: &[],
        },
    },
    SupportedAudioCodec {
        id: CODEC_ID_EAC3,
        info: CodecInfo {
            short_name: "eac3",
            long_name: "Dolby Digital Plus (FFmpeg)",
            profiles: &[],
        },
    },
    SupportedAudioCodec {
        id: CODEC_ID_LIBAV,
        info: CodecInfo {
            short_name: "libav",
            long_name: "FFmpeg decoders (AC-3, E-AC-3, WMA, AMR)",
            profiles: &[],
        },
    },
];

/// Magic that starts a [`Blob`].
const BLOB_MAGIC: &[u8; 6] = b"BCLAV1";

/// Bytes of a [`Blob`] before its setup data: magic, codec number, rate,
/// channels, block align, bit rate.
const BLOB_HEADER: usize = 6 + 1 + 4 + 2 + 4 + 8;

/// A [`CodecConfig`] carried in a Symphonia track's extra data, so a
/// [`LibavReader`] track reaches [`LibavDecoder`] with every field FFmpeg
/// needs. Little-endian: magic `BCLAV1`, codec number (u8), sample rate
/// (u32), channels (u16), block align (u32), bit rate (u64), then the
/// container's setup bytes.
struct Blob;

impl Blob {
    /// Encodes `config`.
    fn encode(config: &CodecConfig) -> Box<[u8]> {
        let mut out = Vec::with_capacity(BLOB_HEADER + config.extradata.len());
        out.extend_from_slice(BLOB_MAGIC);
        out.push(config.codec.number());
        out.extend_from_slice(&config.sample_rate.to_le_bytes());
        out.extend_from_slice(&config.channels.to_le_bytes());
        out.extend_from_slice(&config.block_align.to_le_bytes());
        out.extend_from_slice(&config.bit_rate.to_le_bytes());
        out.extend_from_slice(&config.extradata);
        out.into_boxed_slice()
    }

    /// Decodes a blob, or `None` when the bytes are not one.
    fn decode(bytes: &[u8]) -> Option<CodecConfig> {
        let rest = bytes.strip_prefix(BLOB_MAGIC)?;
        let (&codec, rest) = rest.split_first()?;
        let (rate, rest) = rest.split_first_chunk::<4>()?;
        let (channels, rest) = rest.split_first_chunk::<2>()?;
        let (block_align, rest) = rest.split_first_chunk::<4>()?;
        let (bit_rate, extradata) = rest.split_first_chunk::<8>()?;
        if extradata.len() > bookclerk_libav::MAX_EXTRADATA {
            return None;
        }
        Some(CodecConfig {
            codec: Codec::from_number(codec)?,
            sample_rate: u32::from_le_bytes(*rate),
            channels: u16::from_le_bytes(*channels),
            block_align: u32::from_le_bytes(*block_align),
            bit_rate: u64::from_le_bytes(*bit_rate),
            extradata: extradata.to_vec(),
        })
    }
}

/// The FFmpeg config for a Symphonia track, or an unsupported-codec
/// error.
fn config_for(params: &AudioCodecParameters) -> Result<CodecConfig> {
    if params.codec == CODEC_ID_LIBAV {
        return match Blob::decode(params.extra_data.as_deref().unwrap_or_default()) {
            Some(config) => Ok(config),
            None => unsupported_error("libav: track parameters are missing"),
        };
    }
    let codec = if params.codec == CODEC_ID_AC3 {
        Codec::Ac3
    } else if params.codec == CODEC_ID_EAC3 {
        Codec::Eac3
    } else {
        return unsupported_error("libav: not a codec this build decodes");
    };
    // AC-3 and E-AC-3 frames describe themselves; the MP4 `dac3`/`dec3`
    // box is not needed.
    Ok(CodecConfig::bare(codec))
}

/// The Symphonia layout for a decoded frame's channels.
///
/// FFmpeg's native order and the first 18 positions of Symphonia's mask
/// both follow `WAVEFORMATEXTENSIBLE`, so a mask within those bits maps
/// one to one; anything else is reported as discrete channels, which the
/// decode front averages rather than drops.
fn layout(channels: usize, mask: u64) -> Option<Channels> {
    const WAVE_BITS: u64 = (1 << 18) - 1;
    if mask != 0 && mask & !WAVE_BITS == 0 && mask.count_ones() as usize == channels {
        return Position::from_bits(mask).map(Channels::Positioned);
    }
    Some(Channels::Discrete(u16::try_from(channels).ok()?))
}

/// A Symphonia audio decoder backed by an FFmpeg decoder.
pub(crate) struct LibavDecoder {
    /// The FFmpeg decoder. Symphonia decoders must be `Sync`; every use
    /// goes through `&mut self` ([`Mutex::get_mut`]), so the lock is never
    /// taken.
    decoder: Mutex<Decoder>,
    /// The config the decoder was opened with, kept for
    /// [`AudioDecoder::reset`].
    config: CodecConfig,
    /// The parameters the decoder was built from.
    params: AudioCodecParameters,
    /// Interleaved samples of the packet being decoded.
    pcm: Vec<f32>,
    /// The last decoded packet, planar.
    buf: AudioBuffer<f32>,
}

impl LibavDecoder {
    /// Builds a decoder for a track.
    ///
    /// # Errors
    ///
    /// An unsupported-codec error when the track is not one FFmpeg
    /// decodes here or FFmpeg will not open it.
    pub(crate) fn try_new(params: &AudioCodecParameters) -> Result<Self> {
        let config = config_for(params)?;
        let Ok(decoder) = Decoder::new(&config) else {
            return unsupported_error("libav: decoder refused the track");
        };
        Ok(Self {
            decoder: Mutex::new(decoder),
            config,
            params: params.clone(),
            pcm: Vec::new(),
            buf: AudioBuffer::new(AudioSpec::new(1, Channels::Discrete(1)), 0),
        })
    }

    /// Decodes one packet into [`Self::buf`]; an empty packet (the one
    /// [`LibavReader`] sends at the end of the stream) drains the frames
    /// a delayed decoder still holds instead.
    ///
    /// # Errors
    ///
    /// A decode error when FFmpeg rejects the packet, or the frames it
    /// gives disagree on their layout or have an unusable one; an I/O
    /// error of kind out-of-memory, which ends decoding, when FFmpeg
    /// cannot allocate.
    fn decode_inner(&mut self, data: &[u8]) -> Result<()> {
        self.buf.clear();
        self.pcm.clear();
        let decoder = self
            .decoder
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        let mut spec: Option<(u32, usize, u64)> = None;
        let mut mismatch = false;
        let pcm = &mut self.pcm;
        let on_frame = |frame: bookclerk_libav::Frame<'_>| {
            if frame.frames == 0 {
                return;
            }
            let this = (frame.sample_rate, frame.channels, frame.channel_mask);
            match spec {
                None => spec = Some(this),
                Some(seen) if seen != this => mismatch = true,
                Some(_) => {}
            }
            pcm.extend_from_slice(frame.samples);
        };
        let decoded = if data.is_empty() {
            decoder.drain(on_frame)
        } else {
            decoder.decode(data, on_frame)
        };
        match decoded {
            Ok(()) => {}
            // Not a bad packet to skip: the next one would fail the same
            // way, so the job stops.
            Err(LibavError::OutOfMemory) => {
                return Err(symphonia::core::errors::Error::IoError(
                    std::io::Error::new(std::io::ErrorKind::OutOfMemory, "libav: out of memory"),
                ))
            }
            Err(_) => return decode_error("libav: packet could not be decoded"),
        }
        if mismatch {
            return decode_error("libav: frames of one packet disagree on their layout");
        }
        let Some((rate, width, mask)) = spec else {
            // Decoder delay: no samples yet.
            return Ok(());
        };
        if !(1..=MAX_SAMPLE_RATE_HZ).contains(&rate) {
            return decode_error("libav: unusable sample rate");
        }
        let Some(channels) = layout(width, mask) else {
            return decode_error("libav: unusable channel layout");
        };
        let frames = self.pcm.len() / width;
        let new_spec = AudioSpec::new(rate, channels);
        if *self.buf.spec() != new_spec || self.buf.capacity() < frames {
            self.buf = AudioBuffer::new(new_spec, frames.max(self.buf.capacity()));
        }
        let pcm = &self.pcm;
        self.buf.render_with(Some(frames), |frame, planes| {
            for (channel, plane) in planes.iter_mut().enumerate() {
                plane[frame] = pcm[frame * width + channel];
            }
            Ok(())
        })?;
        Ok(())
    }
}

impl AudioDecoder for LibavDecoder {
    fn reset(&mut self) {
        // A seek lands on unrelated audio: start a fresh decoder. The
        // config opened once, so a refusal here would be an allocation
        // failure; flush the old decoder then.
        match Decoder::new(&self.config) {
            Ok(decoder) => self.decoder = Mutex::new(decoder),
            Err(_) => self
                .decoder
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .flush(),
        }
        self.buf.clear();
    }

    fn codec_info(&self) -> &CodecInfo {
        let index = SUPPORTED
            .iter()
            .position(|codec| codec.id == self.params.codec)
            .unwrap_or(SUPPORTED.len() - 1);
        &SUPPORTED[index].info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        if let Err(err) = self.decode_inner(packet.data) {
            self.buf.clear();
            return Err(err);
        }
        Ok(self.buf.as_generic_audio_buffer_ref())
    }

    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

impl RegisterableAudioDecoder for LibavDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        _opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(Self::try_new(params)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        SUPPORTED
    }
}

/// A packet read ahead by a seek, handed out by the next
/// [`FormatReader::next_packet`].
struct Pending {
    /// Its bytes.
    data: Box<[u8]>,
    /// Its presentation timestamp, in track ticks.
    pts: i64,
    /// Its length in ticks, or 0.
    duration: u64,
}

/// A Symphonia format reader over FFmpeg's demuxers, reading one audio
/// track.
pub(crate) struct LibavReader {
    /// The demuxer.
    demuxer: Demuxer,
    /// The one track, id 0, with codec [`CODEC_ID_LIBAV`].
    tracks: Vec<Track>,
    /// Media timing, from the track.
    media: symphonia::core::formats::MediaInfo,
    /// Always empty: tags are read by `bookclerk-tags`, not here.
    metadata: MetadataLog,
    /// The packet a seek read to learn where it landed.
    pending: Option<Pending>,
    /// Where the packet after the last one handed out starts, in track
    /// ticks: the timestamp of the end-of-stream packet.
    next_ts: i64,
    /// Whether the empty end-of-stream packet, which drains the decoder,
    /// has been handed out since the open or the last seek.
    drained: bool,
}

impl LibavReader {
    /// Opens the container in `source`.
    ///
    /// # Errors
    ///
    /// An unsupported-format error when no FFmpeg demuxer of this build
    /// reads it or it has no audio track FFmpeg decodes here.
    pub(crate) fn open(source: Box<dyn MediaSource>) -> Result<Self> {
        let demuxer = match Demuxer::open(Box::new(source)) {
            Ok(demuxer) => demuxer,
            Err(_) => return unsupported_error("libav: no demuxer reads this file"),
        };
        let found = demuxer.track().clone();
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_LIBAV)
            .with_extra_data(Blob::encode(&found.config));
        if (1..=MAX_SAMPLE_RATE_HZ).contains(&found.config.sample_rate) {
            params.with_sample_rate(found.config.sample_rate);
        }
        if let Some(channels) =
            layout(usize::from(found.config.channels), 0).filter(|_| found.config.channels > 0)
        {
            params.with_channels(channels);
        }
        let (num, den) = found.time_base;
        let Some(time_base) = TimeBase::try_new(num, den) else {
            return unsupported_error("libav: track has no usable time base");
        };
        let mut track = Track::new(0);
        track
            .with_codec_params(CodecParameters::Audio(params))
            .with_time_base(time_base)
            .with_start_ts(Timestamp::new(found.start))
            .with_flags(TrackFlags::DEFAULT);
        if let Some(duration) = found.duration {
            track.with_duration(Duration::new(duration));
        }
        let tracks = vec![track];
        let media = symphonia::core::formats::MediaInfo::from_tracks(&tracks);
        Ok(Self {
            demuxer,
            tracks,
            media,
            metadata: MetadataLog::default(),
            pending: None,
            next_ts: found.start,
            drained: false,
        })
    }

    /// Hands out a demuxed packet for track 0, noting where the next one
    /// starts.
    fn hand_out(&mut self, data: Box<[u8]>, pts: i64, duration: u64) -> Packet {
        self.next_ts = pts.saturating_add(i64::try_from(duration).unwrap_or(0));
        packet(data, pts, duration)
    }

    /// The track's time base.
    fn time_base(&self) -> Option<TimeBase> {
        self.tracks.first().and_then(|track| track.time_base)
    }
}

/// A Symphonia packet for track 0.
fn packet(data: Box<[u8]>, pts: i64, duration: u64) -> Packet {
    Packet::new(0, Timestamp::new(pts), Duration::new(duration), data)
}

impl FormatReader for LibavReader {
    fn format_info(&self) -> &FormatInfo {
        &FORMAT_INFO
    }

    fn media_info(&self) -> &symphonia::core::formats::MediaInfo {
        &self.media
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        let required = match to {
            SeekTo::Timestamp { ts, .. } => ts,
            SeekTo::Time { time, .. } => {
                match self.time_base().and_then(|base| base.calc_timestamp(time)) {
                    Some(ts) => ts,
                    None => return seek_error(SeekErrorKind::OutOfRange),
                }
            }
        };
        self.pending = None;
        self.drained = false;
        if self.demuxer.seek(required.get()).is_err() {
            return seek_error(SeekErrorKind::Unseekable);
        }
        // Read the packet the seek landed on to report where that is; it
        // is handed out next.
        let actual = match self.demuxer.next_packet() {
            Ok(Some(found)) => {
                let pts = found.pts;
                self.pending = Some(Pending {
                    data: found.data.into(),
                    pts,
                    duration: found.duration,
                });
                Timestamp::new(pts.min(required.get()))
            }
            Ok(None) => required,
            Err(_) => return seek_error(SeekErrorKind::Unseekable),
        };
        Ok(SeekedTo {
            track_id: 0,
            required_ts: required,
            actual_ts: actual,
        })
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        if let Some(pending) = self.pending.take() {
            return Ok(Some(self.hand_out(
                pending.data,
                pending.pts,
                pending.duration,
            )));
        }
        // Copied out of the demuxer's buffer before `self` is used again.
        let next = self
            .demuxer
            .next_packet()
            .map(|found| found.map(|found| (Box::from(found.data), found.pts, found.duration)));
        match next {
            Ok(Some((data, pts, duration))) => Ok(Some(self.hand_out(data, pts, duration))),
            // One empty packet past the last real one tells the decoder
            // the stream ended, so WMA's held-back final frames come out.
            Ok(None) if !self.drained => {
                self.drained = true;
                Ok(Some(packet(Box::default(), self.next_ts, 0)))
            }
            Ok(None) => Ok(None),
            Err(_) => decode_error("libav: container could not be read"),
        }
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        // The source now belongs to FFmpeg's reader and goes with it. No
        // caller rereads a file after this reader; an empty stream keeps
        // the trait's contract without handing back half-read state.
        MediaSourceStream::new(Box::new(Cursor::new(Vec::new())), Default::default())
    }
}

/// Opens `source` for decoding, through FFmpeg where Symphonia cannot.
///
/// Files [`bookclerk_libav::sniff`] claims (ASF, raw AMR, raw
/// AC-3/E-AC-3) go straight to [`LibavReader`], before Symphonia's probe
/// can mistake a raw stream for MPEG audio. Everything else is probed by
/// Symphonia first; when it recognises no container, or finds an audio
/// track it has no decoder for (AMR in 3GP), the file is reopened through
/// FFmpeg.
///
/// # Errors
///
/// What [`crate::decode::open_audio`] documents; when FFmpeg cannot read a
/// file Symphonia also failed on, Symphonia's error.
pub(crate) fn open_audio(
    source: Box<dyn MediaSource>,
    seek_index: bool,
) -> crate::error::Result<OpenedAudio> {
    let mut source = source;
    if sniffed(&mut source)? {
        let reader = LibavReader::open(source)
            .map_err(|err| MediaError::Native(format!("probe failed: {err}")))?;
        return ready(open_reader(Box::new(reader))?);
    }
    let shared = SharedSource::new(source);
    let symphonia_err = match probe(Box::new(shared.clone()), seek_index) {
        Ok(format) => match open_reader(format)? {
            Opened::Ready(opened) => return Ok(opened),
            Opened::NoDecoder(reason) => MediaError::Native(reason),
        },
        Err(err) => err,
    };
    // Symphonia's reader is gone; the source is ours again.
    shared.rewind()?;
    match LibavReader::open(Box::new(shared)) {
        Ok(reader) => ready(open_reader(Box::new(reader))?),
        Err(_) => Err(symphonia_err),
    }
}

/// The opened stream, or the reason there is no decoder for it.
fn ready(opened: Opened) -> crate::error::Result<OpenedAudio> {
    match opened {
        Opened::Ready(opened) => Ok(opened),
        Opened::NoDecoder(reason) => Err(MediaError::Native(reason)),
    }
}

/// Whether the start of `source` is a container only FFmpeg reads. Leaves
/// the source at its start.
///
/// # Errors
///
/// An I/O error from reading or rewinding the source.
fn sniffed(source: &mut Box<dyn MediaSource>) -> crate::error::Result<bool> {
    let mut head = [0u8; bookclerk_libav::SNIFF_LEN];
    let mut filled = 0;
    while filled < head.len() {
        match source.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err.into()),
        }
    }
    source.seek(SeekFrom::Start(0))?;
    Ok(bookclerk_libav::sniff(&head[..filled]))
}

/// A media source two readers can take turns on: Symphonia's probe gets
/// one handle, and the other reopens the file through FFmpeg once
/// Symphonia's reader has been dropped. Only one reader uses it at a
/// time, so the lock is never contended.
#[derive(Clone)]
struct SharedSource(Arc<Mutex<Box<dyn MediaSource>>>);

impl SharedSource {
    /// Shares `source`.
    fn new(source: Box<dyn MediaSource>) -> Self {
        Self(Arc::new(Mutex::new(source)))
    }

    /// Seeks back to the start of the file.
    ///
    /// # Errors
    ///
    /// The source's seek error.
    fn rewind(&self) -> std::io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .seek(SeekFrom::Start(0))
            .map(|_| ())
    }
}

impl Read for SharedSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .read(buf)
    }
}

impl Seek for SharedSource {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .seek(pos)
    }
}

impl MediaSource for SharedSource {
    fn is_seekable(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_seekable()
    }

    fn byte_len(&self) -> Option<u64> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .byte_len()
    }
}

#[cfg(test)]
mod tests {
    // Test code: a fixture that does not decode is the test failing.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use bookclerk_libav::fixtures;
    use symphonia::core::units::Time;

    use super::*;
    use crate::decode::{decode_pcm_bytes, OutChannels, PcmFormat};

    #[test]
    fn every_format_decodes_through_the_decode_front() {
        let amr_nb = fixtures::amr_nb();
        let amr_wb = fixtures::amr_wb();
        for (name, bytes, rate, channels) in [
            ("wmav2", fixtures::WMAV2, 44_100, 2),
            ("wmav1", fixtures::WMAV1, 44_100, 1),
            ("ac3", fixtures::AC3, 48_000, 2),
            ("eac3", fixtures::EAC3, 48_000, 2),
            // Symphonia's MP4 reader with the FFmpeg decoder.
            ("eac3 in mp4", fixtures::EAC3_MP4, 48_000, 2),
            ("amr-nb", &amr_nb[..], 8_000, 1),
            ("amr-wb", &amr_wb[..], 16_000, 1),
            // Symphonia's MP4 reader finds no decoder; FFmpeg reopens it.
            ("amr-wb in 3gp", fixtures::AMR_WB_3GP, 16_000, 1),
        ] {
            let mut frames = 0usize;
            let format =
                decode_pcm_bytes(bytes.to_vec(), OutChannels::SourceUpToStereo, |f, pcm| {
                    frames += pcm.len() / usize::from(f.channels);
                    Ok(())
                })
                .unwrap_or_else(|err| panic!("{name}: {err}"));
            assert_eq!(
                format,
                PcmFormat {
                    sample_rate: rate,
                    channels
                },
                "{name}"
            );
            let seconds = frames as f64 / f64::from(rate);
            assert!((0.4..0.7).contains(&seconds), "{name}: {seconds} s");
        }
    }

    #[test]
    fn a_wma_file_converts_to_mp3() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("book.wma");
        std::fs::write(&input, fixtures::WMAV2).unwrap();
        let output = dir.path().join("book.mp3");
        crate::mp3::encode_to_mp3_native(
            &input,
            &output,
            &bookclerk_config::LameConfig::default(),
            None,
            None,
        )
        .expect("encode");
        assert!(std::fs::metadata(&output).unwrap().len() > 1_000);
    }

    #[test]
    fn a_seek_reports_where_it_landed_and_hands_that_packet_out() {
        let opened = open_audio(Box::new(Cursor::new(fixtures::AC3.to_vec())), true).unwrap();
        let mut format = opened.format;
        let seeked = format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::from_millis(250),
                    track_id: None,
                },
            )
            .expect("seek");
        assert!(seeked.actual_ts <= seeked.required_ts);
        assert!(seeked.actual_ts.get() > 0);
        let next = format.next_packet().unwrap().expect("a packet");
        assert_eq!(next.pts, seeked.actual_ts);
        let ts = Timestamp::new(1_000);
        let seeked = format
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts, track_id: 0 })
            .expect("seek by timestamp");
        assert_eq!(seeked.required_ts, ts);
        assert_eq!(format.tracks().len(), 1);
        assert_eq!(format.format_info().short_name, "libav");
        assert!(format.media_info().time_base.is_some());
        assert!(format.metadata().current().is_none());
        let _ = format.into_inner();
    }

    #[test]
    fn the_stream_ends_with_one_empty_packet_that_drains_the_decoder() {
        let opened = open_audio(Box::new(Cursor::new(fixtures::WMAV2.to_vec())), true).unwrap();
        let mut format = opened.format;
        let track = format.tracks()[0].clone();
        let CodecParameters::Audio(params) = track.codec_params.clone().expect("params") else {
            panic!("an audio track");
        };
        let mut decoder = LibavDecoder::try_new(&params).expect("decoder");
        let mut end = None;
        let mut empties = 0;
        while let Some(packet) = format.next_packet().unwrap() {
            if packet.data.is_empty() {
                empties += 1;
                assert!(
                    packet.pts.get() >= end.unwrap_or(0),
                    "after the last packet"
                );
            } else {
                end = Some(packet.pts.get() + i64::try_from(packet.dur.get()).unwrap());
            }
            decoder.decode(&packet).expect("decode or drain");
        }
        assert_eq!(empties, 1);
        assert!(format.next_packet().unwrap().is_none());
        // A seek starts a new run that ends with its own drain packet.
        format
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp {
                    ts: Timestamp::new(0),
                    track_id: 0,
                },
            )
            .expect("seek");
        let mut last = None;
        while let Some(packet) = format.next_packet().unwrap() {
            last = Some(packet.data.is_empty());
        }
        assert_eq!(last, Some(true));
    }

    #[test]
    fn garbage_and_cut_files_fail_cleanly() {
        for bytes in [
            Vec::new(),
            b"#!AMR\n".to_vec(),
            fixtures::WMAV2[..300].to_vec(),
            vec![0x0B, 0x77, 0, 0, 0, 0],
            b"definitely not audio".to_vec(),
        ] {
            let _ = decode_pcm_bytes(bytes, OutChannels::Mono, |_, _| Ok(()));
        }
        // A cut ASF file still gives what it holds or a clean error.
        let cut = fixtures::WMAV2[..fixtures::WMAV2.len() / 2].to_vec();
        let _ = decode_pcm_bytes(cut, OutChannels::Mono, |_, _| Ok(()));
    }

    #[test]
    fn blobs_round_trip_and_garbage_is_refused() {
        let config = CodecConfig {
            codec: Codec::WmaPro,
            sample_rate: 44_100,
            channels: 6,
            block_align: 8_192,
            bit_rate: 384_000,
            extradata: vec![1, 2, 3],
        };
        let blob = Blob::encode(&config);
        assert_eq!(Blob::decode(&blob), Some(config));
        assert_eq!(Blob::decode(&blob[..BLOB_HEADER - 1]), None);
        let mut wrong = blob.to_vec();
        wrong[6] = 0;
        assert_eq!(Blob::decode(&wrong), None);
        assert_eq!(Blob::decode(b"OpusHead"), None);
    }

    #[test]
    fn masks_map_to_positions_or_discrete_channels() {
        assert_eq!(
            layout(2, 0b11),
            Some(Channels::Positioned(
                Position::FRONT_LEFT | Position::FRONT_RIGHT
            ))
        );
        assert_eq!(layout(2, 0), Some(Channels::Discrete(2)));
        // A mask that disagrees with the count, or uses positions past
        // the WAVE set, is not trusted.
        assert_eq!(layout(3, 0b11), Some(Channels::Discrete(3)));
        assert_eq!(layout(1, 1 << 40), Some(Channels::Discrete(1)));
    }

    #[test]
    fn a_track_of_another_codec_is_refused() {
        let mut params = AudioCodecParameters::new();
        params.for_codec(symphonia::core::codecs::audio::well_known::CODEC_ID_MP3);
        assert!(LibavDecoder::try_new(&params).is_err());
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_LIBAV);
        assert!(LibavDecoder::try_new(&params).is_err());
    }
}
