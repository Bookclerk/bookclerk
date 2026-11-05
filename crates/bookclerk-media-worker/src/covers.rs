//! `bookclerk-media-worker covers`: a long-lived cover image server.
//!
//! Unlike a media job, a cover needs no file at all: the host writes the
//! downloaded bytes to stdin and reads the JPEGs back from stdout, framed as
//! [`bookclerk_cover::wire`] describes. So this mode confines itself to
//! nothing (no readable or writable path, no network) before it reads the
//! first frame, and can then safely stay up for many covers. That keeps
//! process start-up off the path of every search result thumbnail.
//!
//! Covers are processed on a rayon pool of [`COVER_THREADS_ENV`] threads, so
//! one worker uses the node's cores without the host spawning a process per
//! cover. Replies go out as each cover finishes, in any order; the request id
//! pairs them up. A decoder panic answers that one request with an error and
//! leaves the worker running.

use std::io::{Read, Write};
use std::panic::AssertUnwindSafe;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use bookclerk_cover::wire::{self, Reply, Request};
use bookclerk_media::COVER_THREADS_ENV;

/// First argument that selects this mode.
pub use bookclerk_media::COVER_WORKER_ARG as ARG;

/// Most decode threads the worker starts, whatever the host asks for.
const MAX_THREADS: usize = 64;

/// Runs the cover server until stdin closes.
pub fn main() -> ExitCode {
    let threads = std::env::var(COVER_THREADS_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, MAX_THREADS);

    // Nothing to read or write: the covers arrive on stdin.
    if let Err(err) = super::confine_to("media-worker:covers", Vec::new(), Vec::new()) {
        eprintln!("bookclerk-media-worker: {err}");
        return ExitCode::FAILURE;
    }

    // This thread reads stdin; the pool's threads decode.
    let pool = match rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("cover-{i}"))
        .build()
    {
        Ok(pool) => pool,
        Err(err) => {
            eprintln!("bookclerk-media-worker: could not start cover threads: {err}");
            return ExitCode::FAILURE;
        }
    };

    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let mut stdin = std::io::stdin().lock();
    let outcome = pool.in_place_scope(|scope| -> Result<(), String> {
        loop {
            let Some(body) = read_frame(&mut stdin)? else {
                return Ok(());
            };
            let request = wire::decode_request(&body).map_err(|err| err.to_string())?;
            let stdout = Arc::clone(&stdout);
            scope.spawn(move |_| {
                let reply = answer(request);
                send(&stdout, &reply);
            });
        }
    });
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("bookclerk-media-worker: cover stream broken: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Reads one request frame body.
///
/// # Returns
///
/// The body, or `None` at a clean end of input (no partial frame).
///
/// # Errors
///
/// A message for a read failure, a truncated frame, or a frame over
/// [`wire::MAX_REQUEST_BODY`].
fn read_frame(input: &mut impl Read) -> Result<Option<Vec<u8>>, String> {
    let mut prefix = [0u8; 4];
    let mut filled = 0;
    while filled < prefix.len() {
        match input.read(&mut prefix[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err("truncated frame length".into()),
            Ok(n) => filled += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err.to_string()),
        }
    }
    let len = wire::body_len(prefix);
    if len > wire::MAX_REQUEST_BODY {
        return Err(format!("request of {len} bytes is too large"));
    }
    let mut body = vec![0u8; len];
    input
        .read_exact(&mut body)
        .map_err(|err| format!("truncated frame: {err}"))?;
    Ok(Some(body))
}

/// Processes one cover, turning a refusal or a decoder panic into an error
/// reply.
fn answer(request: Request) -> Reply {
    let id = request.id;
    match std::panic::catch_unwind(AssertUnwindSafe(|| {
        bookclerk_cover::process(&request.cover)
    })) {
        Ok(Ok(set)) => Reply::Ok { id, set },
        Ok(Err(err)) => Reply::Err {
            id,
            message: err.to_string(),
        },
        Err(_) => Reply::Err {
            id,
            message: "cover decoder panicked".into(),
        },
    }
}

/// Writes one reply frame, or exits when the host has gone away.
fn send(stdout: &Mutex<std::io::Stdout>, reply: &Reply) {
    let frame = match wire::encode_reply(reply) {
        Ok(frame) => frame,
        Err(err) => match wire::encode_reply(&Reply::Err {
            id: reply.id(),
            message: format!("could not encode reply: {err}"),
        }) {
            Ok(frame) => frame,
            Err(_) => return,
        },
    };
    let mut out = stdout
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if out.write_all(&frame).and_then(|()| out.flush()).is_err() {
        // The host closed the pipe; nobody is waiting for the rest.
        std::process::exit(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_frame_splits_and_bounds_frames() {
        let one = wire::encode_request(&Request {
            id: 1,
            cover: vec![7, 8],
        })
        .expect("encode");
        let mut input = [one.clone(), one].concat();
        let mut reader = &input[..];
        for _ in 0..2 {
            let body = read_frame(&mut reader).expect("read").expect("frame");
            assert_eq!(
                wire::decode_request(&body).expect("decode").cover,
                vec![7, 8]
            );
        }
        assert_eq!(read_frame(&mut reader), Ok(None));

        input.truncate(3);
        assert!(read_frame(&mut &input[..]).is_err());
        let huge = u32::try_from(wire::MAX_REQUEST_BODY + 1)
            .expect("fits")
            .to_be_bytes();
        assert!(read_frame(&mut &huge[..]).is_err());
        let short = [0, 0, 0, 9, 1];
        assert!(read_frame(&mut &short[..]).is_err());
    }

    #[test]
    fn answer_reports_refusals() {
        let reply = answer(Request {
            id: 4,
            cover: b"not an image".to_vec(),
        });
        let Reply::Err { id, message } = reply else {
            panic!("error reply");
        };
        assert_eq!(id, 4);
        assert!(message.contains("JPEG"), "{message}");
    }
}
