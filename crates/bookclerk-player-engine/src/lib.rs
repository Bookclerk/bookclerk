//! Native playback for the desktop window.
//!
//! The window's custom scheme cannot stream audio by `Range`, so the window
//! does not play through its webview. The decoding happens in the confined
//! media worker (`bookclerk_media::MediaPool::open_pcm`); this crate does
//! what is left, all of it trusted signal processing on PCM:
//!
//! - [`Stretcher`]: speed change without a pitch change (WSOLA).
//! - [`Resampler`]: the book's sample rate to the output device's.
//! - [`Renderer`]: both, plus channel mapping, pulled by the device.
//! - [`TrackPlayer`]: one track at a time, fed from a [`TrackOpener`], with
//!   [`PlayerEvent`]s for the page that drives it.
//!
//! Nothing here touches an audio device or the OS: the desktop crate owns
//! the output stream and the system media controls, so this crate builds
//! and tests everywhere.
//!
//! Style: `docs/contributing/code-documentation.md`. Design: the playback
//! ADR, `docs/adr/playback.md`.

mod player;
mod renderer;
mod resample;
mod stretch;

pub use player::{PlayerEvent, TrackHeader, TrackOpener, TrackPlayer, TrackStream};
pub use renderer::{OutputFormat, Renderer};
pub use resample::Resampler;
pub use stretch::{Stretcher, MAX_SPEED, MIN_SPEED};
