//! Coverage-guided parsing of the granted reverse channel (`bookclerk-workerd`
//! `granted_http`): request head, body framing, chunked bodies and the query
//! string, every byte of which a plugin isolate controls.
//!
//! Oracles: no panic; a chunked body decodes to the same bytes whether it is
//! fed whole or split; whenever an `httparse`-based reference decoder
//! completes a chunked body, ours completes it with identical bytes; and the
//! route body reader (`BodyReader`: framing dispatch, byte cap, end of
//! stream) yields the same bytes and verdict under any input split and
//! matches the framing the head declared.

#![no_main]

use bookclerk_workerd::granted_http::{
    body_framing, declared_content_length_over_cap, parse_request_head, percent_decode,
    query_param, transfer_encoding_is_chunked, BodyError, BodyFraming, BodyReader, ChunkedDecoder,
    HeadParse, MAX_CHUNK_LINE_BYTES,
};
use bookclerk_workerd::socket_proxy::parse_authority;
use libfuzzer_sys::fuzz_target;

/// Largest input worth parsing: the head cap is 64 KiB, bodies stream.
const MAX_INPUT_BYTES: usize = 96 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let (head, consumed) = match parse_request_head(data) {
        Ok(HeadParse::Complete { head, consumed }) => (head, consumed),
        Ok(HeadParse::Incomplete) | Err(_) => return,
    };
    assert!(consumed <= data.len());
    let (path, query) = head.split_query();
    let _ = query_param(query, "key");
    let _ = percent_decode(query);
    let _ = parse_authority(path);
    let _ = transfer_encoding_is_chunked(&head.headers);
    let _ = declared_content_length_over_cap(&head.headers, 16);
    let body = &data[consumed..];
    match body_framing(&head.headers) {
        Ok(framing) => {
            if framing == BodyFraming::Chunked {
                check_chunked(body);
            }
            check_body_reader(framing, body);
        }
        Err(err) => assert_eq!(
            BodyReader::new(&head.headers).err(),
            Some(BodyError::Framing(err))
        ),
    }
});

/// Drives the route body reader whole and in input-chosen pieces, with and
/// without a cap, and checks the result against what the framing promises.
fn check_body_reader(framing: BodyFraming, body: &[u8]) {
    let cap = body
        .first()
        .map(|b| usize::from(*b).saturating_mul(16))
        .filter(|cap| cap % 3 != 0);
    let (out, verdict) = drive(framing, cap, std::iter::once(body));
    let step = body.get(1).map_or(7, |b| usize::from(*b) + 1);
    let (piece_out, piece_verdict) = drive(framing, cap, body.chunks(step));
    assert_eq!(
        verdict, piece_verdict,
        "split changed the body reader's verdict"
    );
    if let Some(cap) = cap {
        assert!(out.len() <= cap, "yielded past the cap");
        assert!(piece_out.len() <= cap, "yielded past the cap");
        if verdict == Err(BodyError::TooLarge { cap }) {
            // How much was yielded before the cap tripped depends on the
            // split; the server discards it either way.
            return;
        }
    }
    assert_eq!(out, piece_out, "split changed the yielded body");
    match framing {
        BodyFraming::Length(len) => {
            if body.len() >= len {
                assert_eq!(verdict, Ok(()));
                assert_eq!(out, &body[..len]);
            } else {
                assert_eq!(verdict, Err(BodyError::Truncated));
                assert_eq!(out, body);
            }
        }
        BodyFraming::UntilEof => {
            assert_eq!(verdict, Ok(()));
            assert_eq!(out, body);
        }
        BodyFraming::Chunked => {
            if let Some(reference) = reference_chunked(body) {
                assert_eq!(verdict, Ok(()), "reference completed; reader must too");
                assert_eq!(out, reference);
            }
            if verdict.is_ok() {
                assert!(ChunkedDecoder::new()
                    .decode(body, &mut Vec::new())
                    .is_ok_and(|p| p.done));
            }
        }
    }
}

/// Feeds `pieces` to a fresh reader, then end of stream; returns what it
/// yielded and its final verdict.
fn drive<'a>(
    framing: BodyFraming,
    cap: Option<usize>,
    pieces: impl Iterator<Item = &'a [u8]>,
) -> (Vec<u8>, Result<(), BodyError>) {
    let mut reader = BodyReader::for_framing(framing);
    if let Some(cap) = cap {
        reader = reader.with_cap(cap);
    }
    let mut out = Vec::new();
    for piece in pieces {
        match reader.push(piece, &mut out) {
            Ok(true) => return (out, Ok(())),
            Ok(false) => {}
            Err(err) => return (out, Err(err)),
        }
    }
    // An empty body never reaches `push`; the reader must still judge it.
    if let Err(err) = reader.push(&[], &mut out) {
        return (out, Err(err));
    }
    let verdict = reader.finish();
    (out, verdict)
}

/// Decodes `body` whole and in pieces, and against the reference.
fn check_chunked(body: &[u8]) {
    let mut whole = ChunkedDecoder::new();
    let mut whole_out = Vec::new();
    let whole_result = whole.decode(body, &mut whole_out);
    if let Ok(progress) = &whole_result {
        assert!(progress.consumed <= body.len());
        assert_eq!(progress.done, whole.is_done());
    }

    // Split-invariance: feeding the same bytes in two halves (at a position
    // chosen by the input) must decode to the same bytes and verdict.
    let split = body
        .first()
        .map_or(0, |b| usize::from(*b) % (body.len().max(1)));
    let mut piecewise = ChunkedDecoder::new();
    let mut piece_out = Vec::new();
    let mut buf = body[..split].to_vec();
    let first = piecewise.decode(&buf, &mut piece_out);
    let piece_result = match first {
        Err(err) => Err(err),
        Ok(progress) => {
            buf.drain(..progress.consumed);
            buf.extend_from_slice(&body[split..]);
            piecewise.decode(&buf, &mut piece_out).map(|second| {
                bookclerk_workerd::granted_http::ChunkedProgress {
                    consumed: progress.consumed + second.consumed,
                    done: second.done,
                }
            })
        }
    };
    match (&whole_result, &piece_result) {
        (Ok(w), Ok(p)) => {
            assert_eq!(w.done, p.done, "split changed completion");
            assert_eq!(whole_out, piece_out, "split changed decoded bytes");
            if w.done {
                assert_eq!(w.consumed, p.consumed, "split changed consumed length");
            }
        }
        (Err(w), Err(p)) => assert_eq!(w, p, "split changed the error"),
        // One side may hit the error only once it sees the later bytes.
        (Ok(w), Err(_)) => assert!(!w.done, "whole decode finished but split failed"),
        (Err(_), Ok(p)) => assert!(!p.done, "split decode finished but whole failed"),
    }

    if let Some(reference) = reference_chunked(body) {
        let progress = whole_result.expect("reference completed; ours must not fail");
        assert!(progress.done, "reference completed; ours must too");
        assert_eq!(whole_out, reference, "decoded bytes differ from reference");
    }
}

/// `httparse`-based chunked decoder: `Some(body)` when the framing is
/// complete and well-formed by its rules, `None` otherwise (incomplete,
/// malformed, or trailers present, which it does not model).
fn reference_chunked(mut input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        // `httparse` tolerates an empty or whitespace-led size line and a bare
        // LF terminator; RFC 9112 and our decoder require a hex digit first
        // and CRLF, within the decoder's line cap.
        if !input.first().is_some_and(u8::is_ascii_hexdigit) {
            return None;
        }
        let newline = input.iter().position(|&b| b == b'\n')?;
        if newline == 0 || input[newline - 1] != b'\r' || newline + 1 > MAX_CHUNK_LINE_BYTES {
            return None;
        }
        let (consumed, size) = match httparse::parse_chunk_size(input) {
            Ok(httparse::Status::Complete(v)) => v,
            Ok(httparse::Status::Partial) | Err(_) => return None,
        };
        input = &input[consumed..];
        let size = usize::try_from(size).ok()?;
        if size == 0 {
            return input.starts_with(b"\r\n").then_some(out);
        }
        let end = size.checked_add(2)?;
        if input.len() < end || &input[size..end] != b"\r\n" {
            return None;
        }
        out.extend_from_slice(&input[..size]);
        input = &input[size + 2..];
    }
}
