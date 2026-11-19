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
//!   whether a file is audio.

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
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let format = decode_pcm_bytes(data.to_vec(), out, |format, pcm| {
        let channels = usize::from(format.channels);
        assert!(
            (1..=2).contains(&channels),
            "{channels} channels in a chunk"
        );
        assert!(!pcm.is_empty(), "empty chunk handed to the encoder");
        assert_eq!(pcm.len() % channels, 0, "chunk is not whole frames");
        frames += u64::try_from(pcm.len() / channels).expect("frames fit");
        for sample in pcm {
            for byte in sample.to_le_bytes() {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
            }
        }
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
        hash,
    })
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
}
