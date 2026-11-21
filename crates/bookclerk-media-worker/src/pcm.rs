//! `bookclerk-media-worker pcm`: decodes one audio file to PCM for a player.
//!
//! The file may be on S3 or any other destination, so this mode opens no
//! file: it confines itself to nothing (no readable or writable path, no
//! network) before it reads the first frame, then asks the host for the
//! byte ranges its demuxer needs and writes decoded samples back, framed as
//! [`bookclerk_media::pcm_wire`] describes. One process serves one file
//! from one start position; the host starts another to seek.

use std::process::ExitCode;

/// First argument that selects this mode.
pub use bookclerk_media::PCM_WORKER_ARG as ARG;

/// Decodes the file the host opens, then exits.
pub fn main() -> ExitCode {
    // Nothing to read or write: file bytes arrive on stdin.
    if let Err(err) = super::confine_to("media-worker:pcm", Vec::new(), Vec::new()) {
        eprintln!("bookclerk-media-worker: {err}");
        return ExitCode::FAILURE;
    }
    match bookclerk_media::serve_pcm(std::io::stdin(), std::io::stdout()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // The host closed the pipe or broke the protocol; it is no
            // longer listening for an answer.
            eprintln!("bookclerk-media-worker: pcm stream ended: {err}");
            ExitCode::FAILURE
        }
    }
}
