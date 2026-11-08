//! Shared oracle for the `db_stream` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! A database adapter answers `executeStreamed` with consecutive
//! single-segment Cap'n Proto frames that the host reassembles under its
//! streamed result budget. The input's first byte picks the budget, the
//! second the chunk size the bytes are fed in, and the rest is the stream.
//! The invariants:
//!
//! - decoding the stream in one push and in chunks agree: both succeed with
//!   the same reply, or both fail;
//! - an accepted reply stays within the budget: no statement has more rows
//!   than the row cap;
//! - an accepted reply re-encodes to frames that decode to the same reply.

use bookclerk_plugin_abi::{
    decode_execute_stream, encode_execute_stream, DbCapabilities, ExecuteStreamDecoder,
    ExecuteStreamLimits, PluginErrorCode, FIRST_PARTY_MAX_STREAMED_RESULT_BYTES,
};

/// Largest input worth checking; a frame the wire would carry is far
/// smaller than this.
pub const MAX_INPUT_BYTES: usize = 128 * 1024;

/// The budgets the first input byte selects.
pub const BUDGETS: u8 = 3;

/// The budget `selector` names.
///
/// # Arguments
///
/// * `selector` - Input byte; reduced modulo [`BUDGETS`].
#[must_use]
pub fn limits_for(selector: u8) -> ExecuteStreamLimits {
    match selector % BUDGETS {
        // A tiny budget, so the row and byte ceilings are reachable.
        0 => ExecuteStreamLimits {
            max_bytes: 4096,
            bytes_limit: "maxStreamedResultBytes",
            max_rows: 4,
            rows_limit: "maxStreamedResultRows",
        },
        // First-party sqlite as the host reads it.
        1 => ExecuteStreamLimits::from_capabilities(&DbCapabilities::advertised_sqlite()),
        // An adapter that does not stream: inline caps apply.
        _ => {
            let mut caps = DbCapabilities::advertised_sqlite();
            caps.max_streamed_result_bytes = 0;
            caps.max_streamed_result_rows = 0;
            let limits = ExecuteStreamLimits::from_capabilities(&caps);
            debug_assert!(limits.max_bytes < FIRST_PARTY_MAX_STREAMED_RESULT_BYTES);
            limits
        }
    }
}

/// Checks that a receiver under `selector`'s budget refuses a stream once
/// its raw bytes pass the budget's ceiling, whatever the bytes are.
///
/// Fuzz inputs are capped at [`MAX_INPUT_BYTES`], below every budget's raw
/// ceiling (the budget plus a window of slack), so [`check`] cannot reach
/// this rejection on its own; the corpus replay test calls this once per
/// budget instead.
///
/// # Arguments
///
/// * `selector` - Budget byte, as the first input byte of [`check`].
///
/// # Panics
///
/// When bytes past the ceiling are accepted, or the refusal is not the
/// budget's typed byte limit.
pub fn check_raw_ceiling(selector: u8) {
    let limits = limits_for(selector);
    let ceiling = usize::try_from(limits.raw_ceiling()).unwrap_or(usize::MAX);
    let mut decoder = ExecuteStreamDecoder::new(limits);
    // Under the ceiling, plain zero bytes are an incomplete frame header,
    // which the decoder holds for more bytes rather than rejecting.
    decoder
        .push(&[0u8; 4])
        .unwrap_or_else(|err| panic!("four bytes under the ceiling: {err}"));
    let Err(err) = decoder.push(&vec![0u8; ceiling - 4 + 1]) else {
        panic!("bytes past the raw ceiling must be refused");
    };
    assert_eq!(err.code, PluginErrorCode::LimitExceeded, "{err}");
    let limit = err.limit().unwrap_or_else(|| panic!("typed limit: {err}"));
    assert_eq!(limit.limit, limits.bytes_limit);
    assert!(decoder.is_finished(), "the refusal is final");
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - Budget byte, chunk-size byte, then the stream bytes.
///
/// # Panics
///
/// When the decoder breaks an invariant.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let [selector, chunk, stream @ ..] = data else {
        return;
    };
    let limits = limits_for(*selector);
    let chunk = usize::from(*chunk) + 1;

    let whole = decode_execute_stream(stream, limits);

    let mut decoder = ExecuteStreamDecoder::new(limits);
    let mut pushed = Ok(());
    for piece in stream.chunks(chunk) {
        pushed = decoder.push(piece);
        if pushed.is_err() {
            break;
        }
    }
    let split = pushed.and_then(|()| decoder.finish());

    match (whole, split) {
        (Ok(whole), Ok(split)) => {
            assert_eq!(whole, split, "chunked decode must agree with one push");
            if limits.max_rows > 0 {
                for (i, stmt) in whole.statements.iter().enumerate() {
                    assert!(
                        stmt.rows.len() <= usize::try_from(limits.max_rows).unwrap_or(usize::MAX),
                        "statement {i} returned {} rows over the cap {}",
                        stmt.rows.len(),
                        limits.max_rows
                    );
                }
            }
            let frames = encode_execute_stream(&whole).unwrap_or_else(|err| {
                panic!("an accepted reply must re-encode: {err}");
            });
            let again = decode_execute_stream(&frames, limits).unwrap_or_else(|err| {
                panic!("re-encoded frames must decode under the same budget: {err}");
            });
            assert_eq!(
                again, whole,
                "re-encoded frames must decode to the same reply"
            );
        }
        (Err(_), Err(_)) => {}
        (Ok(_), Err(err)) => panic!("one push accepted what chunks of {chunk} refused: {err}"),
        (Err(err), Ok(_)) => panic!("chunks of {chunk} accepted what one push refused: {err}"),
    }
}
