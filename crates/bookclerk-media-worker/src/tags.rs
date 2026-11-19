//! `bookclerk-media-worker tags`: a long-lived embedded-tag reader.
//!
//! Tags live in a file's metadata region, and the file may be on S3, so this
//! mode opens no file at all. For each file the host opens, it runs a
//! [`TagReader`], asks the host for the byte ranges the reader needs, and
//! answers with the record, framed as [`bookclerk_tags::wire`] describes. So
//! it confines itself to nothing (no readable or writable path, no network)
//! before it reads the first frame, and can safely stay up for many pages of
//! files.
//!
//! Files are handled on this one thread, interleaved by id: parsing headers
//! is cheap next to the host's fetches. A reader panic answers that file
//! with an error and leaves the worker running.

use std::collections::HashMap;
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::process::ExitCode;

use bookclerk_tags::wire::{self, Reply, Request};
use bookclerk_tags::{Poll, TagReader};

/// First argument that selects this mode.
pub use bookclerk_media::TAG_WORKER_ARG as ARG;

/// Most files open at once. The host opens at most 16; more is a broken host.
const MAX_OPEN: usize = 256;

/// Runs the tag reader until stdin closes.
pub fn main() -> ExitCode {
    // Nothing to read or write: file bytes arrive on stdin.
    if let Err(err) = super::confine_to("media-worker:tags", Vec::new(), Vec::new()) {
        eprintln!("bookclerk-media-worker: {err}");
        return ExitCode::FAILURE;
    }
    let mut files = Files::default();
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    loop {
        let body = match super::covers::read_frame(&mut stdin, wire::MAX_REQUEST_BODY) {
            Ok(Some(body)) => body,
            Ok(None) => return ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("bookclerk-media-worker: tag stream broken: {err}");
                return ExitCode::FAILURE;
            }
        };
        let request = match wire::decode_request(&body) {
            Ok(request) => request,
            Err(err) => {
                eprintln!("bookclerk-media-worker: bad tag request: {err}");
                return ExitCode::FAILURE;
            }
        };
        if let Some(reply) = files.handle(request) {
            if send(&mut stdout, &reply).is_err() {
                // The host closed the pipe; nobody is waiting for the rest.
                return ExitCode::SUCCESS;
            }
        }
    }
}

/// The files being read, by the host's id.
#[derive(Debug, Default)]
struct Files {
    /// Open readers.
    readers: HashMap<u64, TagReader>,
}

impl Files {
    /// Applies one request.
    ///
    /// # Returns
    ///
    /// The reply to send, or `None` while the file still waits for more of
    /// the spans it asked for (or after a `Close`).
    fn handle(&mut self, request: Request) -> Option<Reply> {
        match request {
            Request::Open { id, size } => {
                if self.readers.contains_key(&id) {
                    // Answering would confuse the read already using `id`.
                    return None;
                }
                if self.readers.len() >= MAX_OPEN {
                    return Some(Reply::Failed {
                        id,
                        message: "too many files open".into(),
                    });
                }
                self.readers.insert(id, TagReader::new(size));
                self.advance(id)
            }
            Request::Data { id, offset, bytes } => {
                let reader = self.readers.get_mut(&id)?;
                if let Err(err) = reader.supply(offset, bytes) {
                    self.readers.remove(&id);
                    return Some(Reply::Failed {
                        id,
                        message: err.to_string(),
                    });
                }
                if reader.is_waiting() {
                    return None;
                }
                self.advance(id)
            }
            Request::Close { id } => {
                self.readers.remove(&id);
                None
            }
        }
    }

    /// Polls one file's reader and turns the result into a reply, forgetting
    /// the file once it is done.
    fn advance(&mut self, id: u64) -> Option<Reply> {
        let reader = self.readers.get_mut(&id)?;
        let poll = std::panic::catch_unwind(AssertUnwindSafe(|| reader.poll()));
        let reply = match poll {
            Ok(Poll::Need(spans)) => return Some(Reply::Need { id, spans }),
            Ok(Poll::Done(Ok(record))) => Reply::Done {
                id,
                record: Box::new(record),
            },
            Ok(Poll::Done(Err(err))) => Reply::Failed {
                id,
                message: err.to_string(),
            },
            Err(_) => Reply::Failed {
                id,
                message: "tag reader panicked".into(),
            },
        };
        self.readers.remove(&id);
        Some(reply)
    }
}

/// Writes one reply frame.
///
/// # Errors
///
/// The write failure (the host is gone).
fn send(out: &mut impl Write, reply: &Reply) -> std::io::Result<()> {
    let frame = match wire::encode_reply(reply) {
        Ok(frame) => frame,
        Err(err) => wire::encode_reply(&Reply::Failed {
            id: reply.id(),
            message: format!("could not encode reply: {err}"),
        })
        .map_err(std::io::Error::other)?,
    };
    out.write_all(&frame)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal FLAC with one comment.
    fn flac() -> Vec<u8> {
        let mut comments = Vec::new();
        comments.extend_from_slice(&0u32.to_le_bytes());
        comments.extend_from_slice(&1u32.to_le_bytes());
        comments.extend_from_slice(&10u32.to_le_bytes());
        comments.extend_from_slice(b"TITLE=Book");
        let mut file = b"fLaC".to_vec();
        file.extend_from_slice(&[0, 0, 0, 34]);
        file.extend_from_slice(&[0; 34]);
        let len = u32::try_from(comments.len()).expect("len").to_be_bytes();
        file.extend_from_slice(&[0x84, len[1], len[2], len[3]]);
        file.extend_from_slice(&comments);
        file
    }

    #[test]
    fn reads_a_file_by_rounds() {
        let file = flac();
        let mut files = Files::default();
        let size = u64::try_from(file.len()).expect("len");
        let mut reply = files.handle(Request::Open { id: 7, size });
        loop {
            match reply {
                Some(Reply::Need { id, spans }) => {
                    assert_eq!(id, 7);
                    reply = None;
                    for span in spans {
                        let from = usize::try_from(span.offset).expect("offset");
                        let to = usize::try_from(span.offset + span.len).expect("end");
                        assert!(reply.is_none(), "replied before every span arrived");
                        reply = files.handle(Request::Data {
                            id,
                            offset: span.offset,
                            bytes: file[from..to].to_vec(),
                        });
                    }
                }
                Some(Reply::Done { id, record }) => {
                    assert_eq!(id, 7);
                    assert_eq!(record.entries[0].value, "Book");
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(files.readers.is_empty());
    }

    #[test]
    fn bad_requests_fail_only_their_file() {
        let mut files = Files::default();
        assert!(matches!(
            files.handle(Request::Open { id: 1, size: 100 }),
            Some(Reply::Need { .. })
        ));
        // A second open of the same id is ignored.
        assert_eq!(files.handle(Request::Open { id: 1, size: 5 }), None);
        assert!(matches!(
            files.handle(Request::Data {
                id: 1,
                offset: 3,
                bytes: vec![0]
            }),
            Some(Reply::Failed { id: 1, .. })
        ));
        assert_eq!(
            files.handle(Request::Data {
                id: 1,
                offset: 0,
                bytes: vec![0]
            }),
            None
        );
        assert!(matches!(
            files.handle(Request::Open { id: 2, size: 0 }),
            Some(Reply::Failed { id: 2, .. })
        ));
        files.handle(Request::Open { id: 3, size: 10 });
        assert_eq!(files.handle(Request::Close { id: 3 }), None);
        assert!(files.readers.is_empty());
    }

    #[test]
    fn send_writes_one_frame() {
        let mut out = Vec::new();
        let reply = Reply::Failed {
            id: 4,
            message: "x".into(),
        };
        send(&mut out, &reply).expect("send");
        assert_eq!(wire::decode_reply(&out[4..]).expect("decode"), reply);
    }
}
