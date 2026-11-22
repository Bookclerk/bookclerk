//! Opus decoding through libopus, plugged into Symphonia.
//!
//! Symphonia demuxes Ogg Opus (RFC 7845) but has no Opus decoder. This
//! wraps libopus's decoder (through the safe `opusic-c` bindings) in
//! Symphonia's decoder traits so it slots into the codec registry the
//! decode front ([`crate::decode`]) uses, the way FDK does for AAC.
//!
//! The track's identification header (`OpusHead`) configures it: channel
//! count, output gain and channel mapping. Mono and stereo streams are
//! decoded (mapping family 0, or family 1 with one stream); surround Opus
//! (several streams) is refused as unsupported, which no audiobook store
//! is known to ship. Output is always 48 kHz, the rate every Opus stream
//! decodes at, whatever input rate the header records.

use std::sync::{Mutex, PoisonError};

use opusic_c::{Channels as OpusChannels, Decoder, SampleRate};
use symphonia::core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioSpec, Channels, GenericAudioBufferRef, Position,
};
use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia::core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia::core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia::core::codecs::CodecInfo;
use symphonia::core::errors::{decode_error, unsupported_error, Result};
use symphonia::core::packet::PacketRef;

/// The rate every Opus stream decodes at, in Hz.
pub(crate) const OPUS_RATE_HZ: u32 = 48_000;

/// Most frames one Opus packet decodes to: 120 ms at 48 kHz (RFC 6716
/// section 3.2.5).
const MAX_PACKET_FRAMES: usize = 5_760;

/// The codecs [`OpusDecoder`] registers for.
const SUPPORTED: &[SupportedAudioCodec] = &[SupportedAudioCodec {
    id: CODEC_ID_OPUS,
    info: CodecInfo {
        short_name: "opus",
        long_name: "Opus (libopus)",
        profiles: &[],
    },
}];

/// The fields of an `OpusHead` identification header (RFC 7845 section
/// 5.1) that decoding needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpusHead {
    /// Output channels: 1 or 2 for the streams this decoder takes.
    pub(crate) channels: u8,
    /// Frames at 48 kHz the decoder discards at the start of the stream.
    pub(crate) pre_skip: u16,
    /// Gain to apply to the output, in Q7.8 dB (256 is +1 dB).
    pub(crate) output_gain: i16,
}

impl OpusHead {
    /// Reads the header Symphonia hands over as the track's extra data.
    ///
    /// The Ogg reader passes the stream's `OpusHead` (version 1, fields
    /// little-endian). The MP4 reader passes the `dOps` box body behind an
    /// `OpusHead` magic: version 0 and fields big-endian (ISO/IEC 23003-5
    /// style Opus-in-ISOBMFF), so version 0 reads the fields big-endian.
    ///
    /// # Errors
    ///
    /// An unsupported-codec error when the bytes are not an `OpusHead`, its
    /// major version is not 0, or it describes a layout this decoder does
    /// not take (no channels, more than two, or several streams).
    pub(crate) fn parse(head: &[u8]) -> Result<Self> {
        let [b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd', version, channels, pre0, pre1, _, _, _, _, gain0, gain1, family, rest @ ..] =
            head
        else {
            return unsupported_error("opus: no OpusHead identification header");
        };
        // The upper four bits are the major version; only 0 is defined.
        if version >> 4 != 0 {
            return unsupported_error("opus: unknown OpusHead version");
        }
        if !(1..=2).contains(channels) {
            return unsupported_error("opus: only mono and stereo streams are decoded");
        }
        match (family, rest) {
            (0, _) => {}
            // Family 1 with a single stream: one coupled stream for stereo,
            // one uncoupled for mono, mapped in order.
            (1, [1, coupled, mapping @ ..])
                if usize::from(*coupled) == usize::from(*channels) - 1
                    && mapping.len() >= usize::from(*channels)
                    && mapping
                        .iter()
                        .take(usize::from(*channels))
                        .zip(0u8..)
                        .all(|(index, expected)| *index == expected) => {}
            _ => return unsupported_error("opus: multistream (surround) Opus is not decoded"),
        }
        let (pre_skip, output_gain) = if *version == 0 {
            (
                u16::from_be_bytes([*pre0, *pre1]),
                i16::from_be_bytes([*gain0, *gain1]),
            )
        } else {
            (
                u16::from_le_bytes([*pre0, *pre1]),
                i16::from_le_bytes([*gain0, *gain1]),
            )
        };
        Ok(Self {
            channels: *channels,
            pre_skip,
            output_gain,
        })
    }

    /// The decoder's channel setting for this stream.
    fn opus_channels(self) -> OpusChannels {
        if self.channels == 1 {
            OpusChannels::Mono
        } else {
            OpusChannels::Stereo
        }
    }

    /// The Symphonia layout of the decoded output.
    fn layout(self) -> Channels {
        if self.channels == 1 {
            Channels::Positioned(Position::FRONT_CENTER)
        } else {
            Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT)
        }
    }
}

/// A Symphonia audio decoder backed by libopus.
pub(crate) struct OpusDecoder {
    /// The libopus decoder, at 48 kHz in the header's channel count.
    /// Symphonia decoders must be `Sync` and the libopus handle is only
    /// `Send`; every use goes through `&mut self` ([`Mutex::get_mut`]), so
    /// the lock is never taken.
    decoder: Mutex<Decoder>,
    /// The header the decoder was configured from, kept for
    /// [`AudioDecoder::reset`].
    head: OpusHead,
    /// The parameters the decoder was built from.
    params: AudioCodecParameters,
    /// Interleaved scratch one packet decodes into
    /// ([`MAX_PACKET_FRAMES`] frames).
    pcm: Vec<f32>,
    /// The last decoded packet, planar.
    buf: AudioBuffer<f32>,
}

impl OpusDecoder {
    /// Builds a decoder for an Opus track.
    ///
    /// # Errors
    ///
    /// An unsupported-codec error when the track is not Opus, its header is
    /// missing or refused ([`OpusHead::parse`]), or libopus will not start.
    fn try_new(params: &AudioCodecParameters) -> Result<Self> {
        if params.codec != CODEC_ID_OPUS {
            return unsupported_error("opus: not an Opus track");
        }
        let head = OpusHead::parse(params.extra_data.as_deref().unwrap_or_default())?;
        let decoder = configured_decoder(head)?;
        let spec = AudioSpec::new(OPUS_RATE_HZ, head.layout());
        Ok(Self {
            decoder: Mutex::new(decoder),
            head,
            params: params.clone(),
            pcm: vec![0.0; MAX_PACKET_FRAMES * usize::from(head.channels)],
            buf: AudioBuffer::new(spec, MAX_PACKET_FRAMES),
        })
    }

    /// Decodes one packet into [`Self::buf`].
    ///
    /// # Errors
    ///
    /// A decode error for an empty packet (which libopus would conceal as a
    /// lost one) or one libopus refuses.
    fn decode_inner(&mut self, data: &[u8]) -> Result<()> {
        self.buf.clear();
        if data.is_empty() {
            return decode_error("opus: empty packet");
        }
        let decoder = self
            .decoder
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        let frames = match decoder.decode_float_to_slice(data, &mut self.pcm, false) {
            Ok(frames) => frames,
            Err(err) => return decode_error(err.message()),
        };
        let width = usize::from(self.head.channels);
        let Some(samples) = frames.checked_mul(width).filter(|n| *n <= self.pcm.len()) else {
            return decode_error("opus: packet longer than 120 ms");
        };
        let pcm = &self.pcm[..samples];
        self.buf.render_with(Some(frames), |frame, planes| {
            for (channel, plane) in planes.iter_mut().enumerate() {
                plane[frame] = pcm[frame * width + channel];
            }
            Ok(())
        })?;
        Ok(())
    }
}

/// Starts a libopus decoder for `head`, with its output gain applied.
///
/// # Errors
///
/// An unsupported-codec error when libopus refuses the channel count or
/// the gain.
fn configured_decoder(head: OpusHead) -> Result<Decoder> {
    let Ok(mut decoder) = Decoder::new(head.opus_channels(), SampleRate::Hz48000) else {
        return unsupported_error("opus: decoder refused the stream layout");
    };
    // libopus takes the gain in the header's own unit (Q7.8 dB).
    if decoder.set_gain(i32::from(head.output_gain)).is_err() {
        return unsupported_error("opus: decoder refused the output gain");
    }
    Ok(decoder)
}

impl AudioDecoder for OpusDecoder {
    fn reset(&mut self) {
        // A seek lands on unrelated audio; a fresh decoder state avoids
        // blending it with the last packet. The layout was accepted once,
        // so a refusal here would be an allocation failure: keep the old
        // decoder then.
        if let Ok(decoder) = configured_decoder(self.head) {
            self.decoder = Mutex::new(decoder);
        }
        self.buf.clear();
    }

    fn codec_info(&self) -> &CodecInfo {
        &SUPPORTED[0].info
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

impl RegisterableAudioDecoder for OpusDecoder {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An `OpusHead` for `channels` with mapping family `family` and the
    /// given mapping table.
    fn head(channels: u8, family: u8, table: &[u8]) -> Vec<u8> {
        let mut head = b"OpusHead".to_vec();
        head.extend([1, channels]);
        head.extend(312u16.to_le_bytes());
        head.extend(44_100u32.to_le_bytes());
        head.extend((-256i16).to_le_bytes());
        head.push(family);
        head.extend(table);
        head
    }

    #[test]
    fn heads_for_mono_and_stereo_are_read() {
        let stereo = OpusHead::parse(&head(2, 0, &[])).expect("stereo");
        assert_eq!(
            stereo,
            OpusHead {
                channels: 2,
                pre_skip: 312,
                output_gain: -256
            }
        );
        assert!(OpusHead::parse(&head(1, 1, &[1, 0, 0])).is_ok());
        // An MP4 `dOps` behind the magic: version 0, big-endian fields.
        let mut dops = b"OpusHead".to_vec();
        dops.extend([0, 2]);
        dops.extend(312u16.to_be_bytes());
        dops.extend(44_100u32.to_be_bytes());
        dops.extend((-256i16).to_be_bytes());
        dops.push(0);
        assert_eq!(OpusHead::parse(&dops).expect("dOps"), stereo);
        assert!(OpusHead::parse(&head(2, 1, &[1, 1, 0, 1])).is_ok());
    }

    #[test]
    fn surround_and_broken_heads_are_refused() {
        // Six channels in four streams: surround.
        assert!(OpusHead::parse(&head(6, 1, &[4, 2, 0, 4, 1, 2, 3, 5])).is_err());
        // Stereo split into two mono streams.
        assert!(OpusHead::parse(&head(2, 1, &[2, 0, 0, 1])).is_err());
        assert!(OpusHead::parse(&head(0, 0, &[])).is_err());
        assert!(OpusHead::parse(&head(2, 255, &[])).is_err());
        assert!(OpusHead::parse(b"OpusHead").is_err());
        let mut future = head(2, 0, &[]);
        future[8] = 0x10;
        assert!(OpusHead::parse(&future).is_err());
    }

    #[test]
    fn garbage_packets_are_decode_errors_not_panics() {
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_OPUS).extra_data = Some(head(2, 0, &[]).into_boxed_slice());
        let mut decoder = OpusDecoder::try_new(&params).expect("decoder");
        assert!(decoder.decode_inner(&[]).is_err());
        for data in [&[0xFF; 3][..], &[0x5A; 9000][..], &[0xFC, 0xFF, 0xFF][..]] {
            let _ = decoder.decode_inner(data);
        }
        decoder.reset();
    }
}
