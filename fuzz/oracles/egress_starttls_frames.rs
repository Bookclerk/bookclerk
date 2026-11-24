//! Shared oracle for the `egress_starttls_frames` fuzz target and the
//! `bookclerk-workerd` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! On a framed tunnel ([`bookclerk_workerd::egress_starttls`]) the launcher's
//! egress relay decodes frames the plugin wrote. Byte 0 picks how the rest
//! is split into reads (`1 + byte0 % 64` bytes each, so frames straddle
//! reads); the rest is the plugin's byte stream. The invariants, checked
//! against an independent whole-buffer reading of the documented layout:
//!
//! - decoding never panics;
//! - the decoder yields exactly the frames the reference reads, in order,
//!   and fails exactly where the reference finds a malformed header;
//! - re-encoding the frames reproduces the bytes they were read from;
//! - the decoder never holds more than one frame header and payload plus one
//!   read.

use bookclerk_workerd::egress_starttls::{
    encode_client_frame, ClientFrame, FrameDecoder, FRAME_HEADER_BYTES, MAX_FRAME_PAYLOAD,
};

/// What the reference reading found.
#[derive(Debug, PartialEq, Eq)]
enum Reference {
    /// Complete frames, then fewer bytes than the next frame needs.
    Frames(Vec<ClientFrame>),
    /// Complete frames, then a malformed header.
    Error(Vec<ClientFrame>),
}

/// Reads `bytes` as the documented layout in one pass.
fn reference(bytes: &[u8]) -> Reference {
    let mut frames = Vec::new();
    let mut at = 0;
    while bytes.len() - at >= 3 {
        let kind = bytes[at];
        let len = (usize::from(bytes[at + 1]) << 8) | usize::from(bytes[at + 2]);
        let valid = match kind {
            0 => (1..=16 * 1024).contains(&len),
            1 => len == 0,
            _ => false,
        };
        if !valid {
            return Reference::Error(frames);
        }
        if bytes.len() - at - 3 < len {
            break;
        }
        frames.push(if kind == 0 {
            ClientFrame::Data(bytes[at + 3..at + 3 + len].to_vec())
        } else {
            ClientFrame::StartTls
        });
        at += 3 + len;
    }
    Reference::Frames(frames)
}

/// Runs every invariant on one input.
pub fn check(data: &[u8]) {
    let Some((&selector, stream)) = data.split_first() else {
        return;
    };
    let read = 1 + usize::from(selector % 64);
    let mut decoder = FrameDecoder::new();
    let mut frames = Vec::new();
    let mut failed = false;
    'reads: for chunk in stream.chunks(read) {
        decoder.push(chunk);
        loop {
            match decoder.next_frame() {
                Ok(Some(frame)) => frames.push(frame),
                Ok(None) => break,
                Err(_) => {
                    failed = true;
                    break 'reads;
                }
            }
        }
        assert!(
            decoder.buffered() < FRAME_HEADER_BYTES + MAX_FRAME_PAYLOAD + read,
            "decoder holds {} bytes",
            decoder.buffered()
        );
    }
    let got = if failed {
        Reference::Error(frames.clone())
    } else {
        Reference::Frames(frames.clone())
    };
    assert_eq!(got, reference(stream));
    let encoded: Vec<u8> = frames.iter().flat_map(encode_client_frame).collect();
    assert_eq!(&stream[..encoded.len()], &encoded[..]);
}
