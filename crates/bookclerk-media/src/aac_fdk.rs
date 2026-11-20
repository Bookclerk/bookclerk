//! AAC decoding through the Fraunhofer FDK decoder, plugged into Symphonia.
//!
//! Symphonia's own AAC decoder handles AAC-LC only: it refuses explicit SBR
//! and decodes implicitly signalled HE-AAC as half-rate, muffled AAC-LC.
//! FDK (already linked for encoding) decodes every AAC profile an
//! audiobook store ships: AAC-LC, HE-AAC (SBR), HE-AAC v2 (parametric
//! stereo) and xHE-AAC (USAC), with MPEG Surround folded to stereo.
//!
//! [`FdkAacDecoder`] implements Symphonia's decoder traits so it slots into
//! the codec registry the decode front ([`crate::decode`]) uses, and the
//! demuxers (MP4, ADTS) stay Symphonia's. Every AAC stream goes through it.
//!
//! The output layout comes from FDK after each frame, not from the
//! container: SBR doubles the sample rate the container states for an
//! implicitly signalled stream, and parametric stereo turns a mono core
//! into stereo.

use fdk_aac::dec::{Decoder, DecoderError, Transport};
use symphonia::core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioSpec, Channels, GenericAudioBufferRef,
};
use symphonia::core::codecs::audio::well_known::profiles::{
    CODEC_PROFILE_AAC_HE, CODEC_PROFILE_AAC_HE_V2, CODEC_PROFILE_AAC_LC, CODEC_PROFILE_AAC_USAC,
};
use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia::core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia::core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia::core::codecs::{CodecInfo, CodecProfileInfo};
use symphonia::core::errors::{decode_error, unsupported_error, Result};
use symphonia::core::packet::PacketRef;

use crate::decode::MAX_SAMPLE_RATE_HZ;

/// Most channels FDK is asked to output. Wider layouts (5.1, MPEG Surround)
/// are downmixed by FDK itself, so every consumer sees mono or stereo.
const MAX_OUTPUT_CHANNELS: usize = 2;

/// Interleaved samples in the scratch FDK decodes one frame into: room for
/// the longest frame (4096 samples per channel, xHE-AAC with 4:1 SBR) at
/// eight channels, so a stream that ignores the downmix request still fits.
/// FDK refuses, rather than overruns, a buffer that is too small.
const PCM_SCRATCH_SAMPLES: usize = 4096 * 8;

/// Sample rates an AAC sampling-frequency index names (ISO/IEC 14496-3
/// table 1.18), in index order.
const AAC_SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// The AAC object type for AAC-LC, used when building a configuration for a
/// stream that carried none (ADTS).
const AOT_AAC_LC: u8 = 2;

/// Profiles reported for the codec FDK handles.
const PROFILES: &[CodecProfileInfo] = &[
    CodecProfileInfo {
        profile: CODEC_PROFILE_AAC_LC,
        short_name: "aac-lc",
        long_name: "Low Complexity",
    },
    CodecProfileInfo {
        profile: CODEC_PROFILE_AAC_HE,
        short_name: "he-aac",
        long_name: "High Efficiency (SBR)",
    },
    CodecProfileInfo {
        profile: CODEC_PROFILE_AAC_HE_V2,
        short_name: "he-aac-v2",
        long_name: "High Efficiency v2 (SBR + parametric stereo)",
    },
    CodecProfileInfo {
        profile: CODEC_PROFILE_AAC_USAC,
        short_name: "xhe-aac",
        long_name: "Extended High Efficiency (USAC)",
    },
];

/// The codecs [`FdkAacDecoder`] registers for.
const SUPPORTED: &[SupportedAudioCodec] = &[SupportedAudioCodec {
    id: CODEC_ID_AAC,
    info: CodecInfo {
        short_name: "aac",
        long_name: "Advanced Audio Coding (Fraunhofer FDK)",
        profiles: PROFILES,
    },
}];

/// A Symphonia audio decoder backed by FDK's AAC decoder.
pub(crate) struct FdkAacDecoder {
    /// The FDK decoder, configured with [`Self::asc`] for raw access units.
    decoder: Decoder,
    /// The AudioSpecificConfig the decoder was configured with, kept so
    /// [`AudioDecoder::reset`] can start a fresh decoder.
    asc: Vec<u8>,
    /// The parameters the decoder was built from.
    params: AudioCodecParameters,
    /// Interleaved scratch FDK writes one frame into
    /// ([`PCM_SCRATCH_SAMPLES`] long).
    pcm: Vec<i16>,
    /// The last decoded frame, planar, in the layout FDK reported for it.
    buf: AudioBuffer<i16>,
}

impl FdkAacDecoder {
    /// Builds a decoder for an AAC track.
    ///
    /// The track's AudioSpecificConfig (MP4 `esds`) configures FDK. A track
    /// without one (ADTS, whose headers the demuxer strips) gets an AAC-LC
    /// configuration built from its sample rate and channel count; FDK
    /// still finds SBR and parametric stereo in the frames themselves.
    ///
    /// # Errors
    ///
    /// An unsupported-codec error when the track is not AAC, carries no
    /// configuration and no usable rate or channel count, or FDK refuses
    /// the configuration.
    fn try_new(params: &AudioCodecParameters) -> Result<Self> {
        if params.codec != CODEC_ID_AAC {
            return unsupported_error("fdk-aac: not an AAC track");
        }
        let asc = match params.extra_data.as_deref() {
            Some(asc) if !asc.is_empty() => asc.to_vec(),
            _ => match lc_config(params) {
                Some(asc) => asc.to_vec(),
                None => {
                    return unsupported_error(
                        "fdk-aac: no AudioSpecificConfig and no usable rate or channels",
                    )
                }
            },
        };
        let decoder = configured_decoder(&asc)?;
        Ok(Self {
            decoder,
            asc,
            params: params.clone(),
            pcm: vec![0; PCM_SCRATCH_SAMPLES],
            buf: AudioBuffer::new(AudioSpec::new(1, Channels::Discrete(1)), 0),
        })
    }

    /// Decodes one access unit into [`Self::buf`].
    ///
    /// A frame FDK decoded with concealment (a damaged but parseable access
    /// unit) still yields its concealed PCM, as players do, so the running
    /// time does not shift. A unit with no output yet (decoder delay)
    /// leaves the buffer empty.
    ///
    /// # Errors
    ///
    /// A decode error when FDK cannot take or decode the unit, or reports a
    /// layout outside the limits.
    fn decode_inner(&mut self, data: &[u8]) -> Result<()> {
        self.buf.clear();
        // `fill` returns the bytes FDK took; a raw access unit must fit whole.
        match self.decoder.fill(data) {
            Ok(taken) if taken == data.len() => {}
            Ok(_) => return decode_error("fdk-aac: access unit larger than the decoder buffer"),
            Err(err) => return decode_error(err.message()),
        }
        match self.decoder.decode_frame(&mut self.pcm) {
            Ok(()) => {}
            Err(err) if err == DecoderError::NOT_ENOUGH_BITS => return Ok(()),
            Err(err) if is_concealed(err) => {}
            Err(err) => return decode_error(err.message()),
        }

        let info = self.decoder.stream_info();
        let (Ok(rate), Ok(channels), Ok(frames)) = (
            u32::try_from(info.sampleRate),
            u16::try_from(info.numChannels),
            usize::try_from(info.frameSize),
        ) else {
            return decode_error("fdk-aac: negative stream layout");
        };
        if !(1..=MAX_SAMPLE_RATE_HZ).contains(&rate) || channels == 0 || frames == 0 {
            return decode_error("fdk-aac: unusable stream layout");
        }
        let width = usize::from(channels);
        let Some(samples) = frames.checked_mul(width).filter(|n| *n <= self.pcm.len()) else {
            return decode_error("fdk-aac: frame larger than the output buffer");
        };

        let spec = AudioSpec::new(rate, Channels::Discrete(channels));
        if *self.buf.spec() != spec || self.buf.capacity() < frames {
            self.buf = AudioBuffer::new(spec, frames);
        }
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

/// Opens an FDK decoder for raw access units and configures it.
///
/// # Errors
///
/// An unsupported-codec error when FDK refuses `asc` or the downmix limit.
fn configured_decoder(asc: &[u8]) -> Result<Decoder> {
    let mut decoder = Decoder::new(Transport::Raw);
    // A configuration FDK accepts also proves the handle is live, so the
    // stream-info reads after each frame never see a null handle.
    if decoder.config_raw(asc).is_err() {
        return unsupported_error("fdk-aac: AudioSpecificConfig refused");
    }
    if decoder
        .set_max_output_channels(MAX_OUTPUT_CHANNELS)
        .is_err()
    {
        return unsupported_error("fdk-aac: downmix limit refused");
    }
    Ok(decoder)
}

/// Builds a two-byte AAC-LC AudioSpecificConfig from a track's sample rate
/// and channel count, or `None` when either is missing or has no AAC code.
fn lc_config(params: &AudioCodecParameters) -> Option<[u8; 2]> {
    let rate = params.sample_rate?;
    let rate_index = u8::try_from(AAC_SAMPLE_RATES.iter().position(|r| *r == rate)?).ok()?;
    let channel_config: u8 = match params.channels.as_ref()?.count() {
        count @ 1..=6 => u8::try_from(count).ok()?,
        8 => 7,
        _ => return None,
    };
    // 5 bits object type, 4 bits rate index, 4 bits channel configuration,
    // then three zero flag bits.
    Some([
        (AOT_AAC_LC << 3) | (rate_index >> 1),
        ((rate_index & 1) << 7) | (channel_config << 3),
    ])
}

/// Whether FDK still wrote (concealed) PCM despite `err`: its decode-error
/// class, where the output buffer is valid but concealed.
fn is_concealed(err: DecoderError) -> bool {
    [
        DecoderError::TRANSPORT_ERROR,
        DecoderError::PARSE_ERROR,
        DecoderError::UNSUPPORTED_EXTENSION_PAYLOAD,
        DecoderError::DECODE_FRAME_ERROR,
        DecoderError::CRC_ERROR,
        DecoderError::INVALID_CODE_BOOK,
        DecoderError::UNSUPPORTED_PREDICTION,
        DecoderError::UNSUPPORTED_CCE,
        DecoderError::UNSUPPORTED_LFE,
        DecoderError::UNSUPPORTED_GAIN_CONTROL_DATA,
        DecoderError::UNSUPPORTED_SBA,
        DecoderError::TNS_READ_ERROR,
        DecoderError::RVLC_ERROR,
    ]
    .contains(&err)
}

impl AudioDecoder for FdkAacDecoder {
    fn reset(&mut self) {
        // FDK keeps overlap and SBR state across frames; after a seek a
        // fresh decoder avoids splicing unrelated audio. The configuration
        // was accepted once, so a refusal here would be an allocation
        // failure; the old decoder is kept then.
        if let Ok(decoder) = configured_decoder(&self.asc) {
            self.decoder = decoder;
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

impl RegisterableAudioDecoder for FdkAacDecoder {
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
    use symphonia::core::audio::Position;

    #[test]
    fn lc_config_matches_the_spec_layout() {
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_AAC)
            .with_sample_rate(44_100)
            .with_channels(Channels::Positioned(
                Position::FRONT_LEFT | Position::FRONT_RIGHT,
            ));
        // AAC-LC, 44.1 kHz (index 4), stereo: 00010 0100 0010 000.
        assert_eq!(lc_config(&params), Some([0x12, 0x10]));
        params.with_sample_rate(44_000);
        assert_eq!(lc_config(&params), None);
    }

    #[test]
    fn a_track_that_is_not_aac_is_refused() {
        let params = AudioCodecParameters::new();
        assert!(FdkAacDecoder::try_new(&params).is_err());
    }

    #[test]
    fn garbage_access_units_are_decode_errors_not_panics() {
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_AAC).extra_data = Some(vec![0x12, 0x10].into_boxed_slice());
        let mut decoder = FdkAacDecoder::try_new(&params).expect("decoder");
        for data in [&[][..], &[0xFF; 3][..], &[0x5A; 9000][..]] {
            let _ = decoder.decode_inner(data);
        }
        decoder.reset();
    }
}
