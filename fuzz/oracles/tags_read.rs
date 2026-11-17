//! Shared oracle for the `tags_read` fuzz target and the `bookclerk-tags`
//! corpus replay test (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is a whole media file from storage, which anyone who can put a
//! file there controls. The oracle drives [`TagReader`] over it twice, once
//! supplying the spans it asks for in order and once in reverse, and checks:
//!
//! - every span asked for is non-empty, inside the file, and at most
//!   [`MAX_SPANS_PER_ROUND`] come per round, within [`MAX_ROUNDS`] rounds;
//! - both drives, and [`read_all`] with every byte available up front, give
//!   the same answer, so the result does not depend on how bytes arrive;
//! - an accepted record passes [`TagRecord::validate`] (what the host
//!   checks), and survives the worker's `Done` frame unchanged.

use bookclerk_tags::wire::{decode_reply, encode_reply, Reply};
use bookclerk_tags::{
    read_all, Poll, TagError, TagReader, TagRecord, MAX_ROUNDS, MAX_SPANS_PER_ROUND,
};

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 256 * 1024;

/// Drives a reader over `file`, supplying each round's spans in order or
/// reversed.
fn drive(file: &[u8], reverse: bool) -> Result<TagRecord, TagError> {
    let size = u64::try_from(file.len()).expect("size fits");
    let mut reader = TagReader::new(size);
    let mut rounds = 0u32;
    loop {
        match reader.poll() {
            Poll::Done(result) => return result,
            Poll::Need(mut spans) => {
                rounds += 1;
                assert!(rounds <= MAX_ROUNDS, "more than {MAX_ROUNDS} rounds");
                assert!(
                    !spans.is_empty() && spans.len() <= MAX_SPANS_PER_ROUND,
                    "{} spans in one round",
                    spans.len()
                );
                if reverse {
                    spans.reverse();
                }
                for span in spans {
                    let end = span.end().expect("span end overflows");
                    assert!(span.len > 0 && end <= size, "span {span:?} outside file");
                    let from = usize::try_from(span.offset).expect("offset fits");
                    let to = usize::try_from(end).expect("end fits");
                    reader
                        .supply(span.offset, file[from..to].to_vec())
                        .expect("requested span refused");
                }
            }
        }
    }
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
    let forward = drive(data, false);
    let backward = drive(data, true);
    let whole = read_all(data);
    assert_eq!(forward, backward, "answer depends on span order");
    assert_eq!(forward, whole, "answer depends on how bytes arrive");
    let Ok(record) = forward else {
        return;
    };
    let size = u64::try_from(data.len()).expect("size fits");
    record
        .validate(size)
        .expect("reader produced a record the host refuses");
    let reply = Reply::Done {
        id: 1,
        record: Box::new(record),
    };
    let frame = encode_reply(&reply).expect("encode record");
    assert_eq!(decode_reply(&frame[4..]).expect("decode record"), reply);
}
