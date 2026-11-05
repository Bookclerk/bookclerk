//! Shared oracle for the `cover_wire` fuzz target and the `bookclerk-cover`
//! corpus replay test (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The cover worker decodes request frames from the host, and the host
//! decodes reply frames from the worker, which handles untrusted images and
//! so is treated as compromised. The input's first byte picks the decoder
//! (even: request, odd: reply), the rest is the frame body. The invariant:
//! whatever a decoder accepts re-encodes to exactly the same bytes, so a
//! frame has one reading, and an accepted reply's images all start with the
//! JPEG signature and its size is nonzero.

use bookclerk_cover::wire::{
    decode_reply, decode_request, encode_reply, encode_request, Reply, MAX_ERROR_BYTES,
};

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - Selector byte, then the frame body.
///
/// # Panics
///
/// When a decoder accepts a frame that does not re-encode to itself.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((&selector, body)) = data.split_first() else {
        return;
    };
    if selector % 2 == 0 {
        if let Ok(request) = decode_request(body) {
            assert_eq!(
                encode_request(&request).expect("re-encode").get(4..),
                Some(body)
            );
        }
        return;
    }
    let Ok(reply) = decode_reply(body) else {
        return;
    };
    match &reply {
        Reply::Ok { set, .. } => {
            assert!(set.width >= 1 && set.height >= 1, "empty size accepted");
            for image in [&set.thumb, &set.card, &set.full] {
                assert!(
                    image.starts_with(&[0xFF, 0xD8, 0xFF]),
                    "non-JPEG image accepted"
                );
            }
        }
        Reply::Err { message, .. } => {
            assert!(message.len() <= MAX_ERROR_BYTES, "long message accepted");
        }
    }
    assert_eq!(
        encode_reply(&reply).expect("re-encode").get(4..),
        Some(body)
    );
}
