//! Shared oracle for the `media_decode` fuzz target and the `bookclerk-media`
//! corpus replay test (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is a whole audio file from a store download, a library's copy
//! or storage, which the media worker demuxes and decodes before it encodes
//! or repackages it. The oracle decodes it twice with
//! [`decode_pcm_bytes`], once at the source's channel count and once mixed
//! to mono, and checks:
//!
//! - an accepted stream reports a sample rate in `1..=MAX_SAMPLE_RATE_HZ`
//!   and one or two channels, and every chunk is a whole number of frames
//!   in that layout;
//! - decoding is deterministic: the same bytes give the same PCM;
//! - the mono decode accepts exactly the inputs the stereo decode accepts,
//!   at the same rate and frame count, so the channel mixer never decides
//!   whether a file is audio;
//! - a plain 16-bit PCM WAV (one `fmt ` chunk, then one `data` chunk that
//!   runs exactly to the end) decodes to exactly the samples it holds, as
//!   read by the independent parser [`plain_wav`].

use bookclerk_media::{decode_pcm_bytes, OutChannels, PcmFormat, MAX_SAMPLE_RATE_HZ};

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 128 * 1024;

/// What one decode produced: the layout, the frame count and a running
/// hash of every sample in order.
#[derive(Debug, PartialEq, Eq)]
struct Decoded {
    /// The stream's layout.
    format: PcmFormat,
    /// Frames decoded, summed over chunks.
    frames: u64,
    /// FNV-1a over the samples, in order.
    hash: u64,
}

/// Decodes `data` to `out`, checking each chunk's shape.
fn decode(data: &[u8], out: OutChannels) -> Option<Decoded> {
    let mut frames = 0u64;
    let mut samples = Vec::new();
    let format = decode_pcm_bytes(data.to_vec(), out, |format, pcm| {
        let channels = usize::from(format.channels);
        assert!(
            (1..=2).contains(&channels),
            "{channels} channels in a chunk"
        );
        assert!(!pcm.is_empty(), "empty chunk handed to the encoder");
        assert_eq!(pcm.len() % channels, 0, "chunk is not whole frames");
        frames += u64::try_from(pcm.len() / channels).expect("frames fit");
        samples.extend_from_slice(pcm);
        Ok(())
    })
    .ok()?;
    assert!(
        (1..=MAX_SAMPLE_RATE_HZ).contains(&format.sample_rate),
        "sample rate {} out of range",
        format.sample_rate
    );
    if out == OutChannels::Mono {
        assert_eq!(format.channels, 1, "mono decode reported stereo");
    }
    Some(Decoded {
        format,
        frames,
        hash: hash_samples(&samples),
    })
}

/// The layout and samples of a plain 16-bit PCM WAV, or `None` when `data`
/// is anything else: `RIFF` with a size that matches, `WAVE`, a 16-byte
/// `fmt ` chunk (PCM, one or two channels, 16 bits, consistent block align
/// and byte rate, rate in range), then a non-empty `data` chunk of whole
/// frames that ends exactly at the end of the file.
fn plain_wav(data: &[u8]) -> Option<(PcmFormat, Vec<i16>)> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?));
    if data.get(0..4)? != b"RIFF" || data.get(8..12)? != b"WAVE" {
        return None;
    }
    if usize::try_from(u32_at(4)?).ok()? != data.len().checked_sub(8)? {
        return None;
    }
    if data.get(12..16)? != b"fmt " || u32_at(16)? != 16 || u16_at(20)? != 1 {
        return None;
    }
    let channels = u16_at(22)?;
    let sample_rate = u32_at(24)?;
    let block_align = u16_at(32)?;
    if !(1..=2).contains(&channels)
        || !(1..=MAX_SAMPLE_RATE_HZ).contains(&sample_rate)
        || u16_at(34)? != 16
        || block_align != channels * 2
        || u32_at(28)? != sample_rate.checked_mul(u32::from(block_align))?
    {
        return None;
    }
    if data.get(36..40)? != b"data" {
        return None;
    }
    let body = data.get(44..)?;
    if usize::try_from(u32_at(40)?).ok()? != body.len()
        || body.is_empty()
        || body.len() % usize::from(block_align) != 0
    {
        return None;
    }
    let samples = body
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect();
    Some((
        PcmFormat {
            sample_rate,
            channels,
        },
        samples,
    ))
}

/// FNV-1a over `samples` in order, as [`decode`] hashes them.
fn hash_samples(samples: &[i16]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for sample in samples {
        for byte in sample.to_le_bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

/// Runs the oracle on one input.
///
/// # Panics
///
/// When an invariant above does not hold.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let source = decode(data, OutChannels::SourceUpToStereo);
    assert_eq!(
        source,
        decode(data, OutChannels::SourceUpToStereo),
        "decoding the same bytes twice differed"
    );
    let mono = decode(data, OutChannels::Mono);
    match (&source, &mono) {
        (Some(source), Some(mono)) => {
            assert_eq!(source.format.sample_rate, mono.format.sample_rate);
            assert_eq!(source.frames, mono.frames, "mixing changed the length");
        }
        (None, None) => {}
        _ => panic!("mono and source decodes disagree on whether this is audio"),
    }
    if let Some((format, samples)) = plain_wav(data) {
        let frames = u64::try_from(samples.len() / usize::from(format.channels)).expect("fits");
        assert_eq!(
            source,
            Some(Decoded {
                format,
                frames,
                hash: hash_samples(&samples),
            }),
            "a plain PCM WAV did not decode to the samples it holds"
        );
    }
}
