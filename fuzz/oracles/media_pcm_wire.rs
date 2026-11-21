//! Shared oracle for the `media_pcm_wire` fuzz target and the
//! `bookclerk-media` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The PCM worker (`bookclerk-media-worker pcm`) decodes untrusted audio,
//! so the host treats it as compromised and reads its output through
//! `ReplyReader` and `ReplyOrder`. The input's first byte picks what is
//! checked, the rest is the bytes:
//!
//! - `0 mod 3`: a request body. One that decodes re-encodes to exactly the
//!   same bytes.
//! - `1 mod 3`: a reply body. One that decodes re-encodes to exactly the
//!   same bytes, and its fields are within their limits, checked here
//!   against the constants rather than by the decoder's own code.
//! - `2 mod 3`: the worker's whole output, fed to `ReplyReader` in pieces
//!   whose sizes come from the input. The replies it yields re-encode,
//!   concatenated, to exactly the bytes it consumed; it never yields a
//!   reply after an error; and every `Pcm` that `ReplyOrder` admits holds
//!   whole frames of the `Format` it admitted first. When `selector / 3` is
//!   odd the stream is a sample's (opened with `Sample`): `ReplyOrder`
//!   then admits no `Pcm` at all, `Encoded` only after a mono `Format` at
//!   most `MAX_SAMPLE_OUTPUT_HZ`, and in a PCM stream it admits no
//!   `Encoded`.

use bookclerk_media::pcm_wire::{
    decode_reply, decode_request, encode_reply, encode_request, PcmReply, ReplyOrder, ReplyReader,
    MAX_ENCODED_BYTES, MAX_ERROR_BYTES, MAX_NEED_BYTES, MAX_PCM_SAMPLES, MAX_POSITION_MS,
    MAX_SAMPLE_OUTPUT_HZ,
};
use bookclerk_media::MAX_SAMPLE_RATE_HZ;

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - Selector byte, then the bytes.
///
/// # Panics
///
/// When an invariant above does not hold.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((&selector, body)) = data.split_first() else {
        return;
    };
    match selector % 3 {
        0 => {
            if let Ok(request) = decode_request(body) {
                let frame = encode_request(&request).expect("re-encode");
                assert_eq!(frame.get(4..), Some(body), "request has two readings");
            }
        }
        1 => {
            if let Ok(reply) = decode_reply(body) {
                within_limits(&reply);
                let frame = encode_reply(&reply).expect("re-encode");
                assert_eq!(frame.get(4..), Some(body), "reply has two readings");
            }
        }
        _ => stream(body, (selector / 3) % 2 == 1),
    }
}

/// Checks a decoded reply's fields against the wire's limits.
fn within_limits(reply: &PcmReply) {
    match reply {
        PcmReply::Need { offset, len } => {
            assert!(*len > 0 && *len <= MAX_NEED_BYTES, "need of {len} bytes");
            assert!(
                offset.checked_add(u64::from(*len)).is_some(),
                "overflowing need"
            );
        }
        PcmReply::Format(header) => {
            assert!((1..=MAX_SAMPLE_RATE_HZ).contains(&header.sample_rate));
            assert!(header.channels == 1 || header.channels == 2);
            assert!(header.start_ms <= MAX_POSITION_MS);
            assert!(header.duration_ms.is_none_or(|ms| ms <= MAX_POSITION_MS));
        }
        PcmReply::Pcm(samples) => {
            assert!(!samples.is_empty() && samples.len() <= MAX_PCM_SAMPLES);
        }
        PcmReply::End => {}
        PcmReply::Failed { message } => assert!(message.len() <= MAX_ERROR_BYTES),
        PcmReply::Encoded(bytes) => {
            assert!(!bytes.is_empty() && bytes.len() <= MAX_ENCODED_BYTES);
        }
    }
}

/// Feeds `body` (after a piece-size byte) to a reader in pieces, as the
/// output of a PCM stream or, when `sample`, of a sample.
fn stream(body: &[u8], sample: bool) {
    let Some((&piece, bytes)) = body.split_first() else {
        return;
    };
    let piece = usize::from(piece).max(1);
    let mut reader = ReplyReader::default();
    let mut order = if sample {
        ReplyOrder::OpeningSample
    } else {
        ReplyOrder::Opening
    };
    let mut format = None;
    let mut reencoded = Vec::new();
    let mut broken = false;
    for part in bytes.chunks(piece) {
        reader.push(part);
        loop {
            match reader.next_reply() {
                Ok(Some(reply)) => {
                    assert!(!broken, "a reply after an error");
                    within_limits(&reply);
                    reencoded.extend(encode_reply(&reply).expect("re-encode"));
                    if order.check(&reply).is_ok() {
                        match &reply {
                            PcmReply::Format(header) => {
                                assert!(format.is_none(), "a second format admitted");
                                if sample {
                                    assert_eq!(header.channels, 1, "a stereo sample admitted");
                                    assert!(header.sample_rate <= MAX_SAMPLE_OUTPUT_HZ);
                                }
                                format = Some(*header);
                            }
                            PcmReply::Pcm(samples) => {
                                assert!(!sample, "samples admitted in a sample's stream");
                                let header = format.expect("samples admitted before a format");
                                assert_eq!(samples.len() % usize::from(header.channels), 0);
                            }
                            PcmReply::Encoded(_) => {
                                assert!(sample, "encoded bytes admitted in a PCM stream");
                                assert!(format.is_some(), "bytes admitted before a format");
                            }
                            _ => {}
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    broken = true;
                    break;
                }
            }
        }
    }
    if !broken {
        let consumed = bytes.len() - reader.buffered();
        assert_eq!(
            reencoded.as_slice(),
            &bytes[..consumed],
            "replies differ from the bytes"
        );
    } else {
        assert!(
            bytes.starts_with(&reencoded),
            "replies before the error differ"
        );
    }
}
