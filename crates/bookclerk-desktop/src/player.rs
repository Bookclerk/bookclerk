//! The window's own player: audio output and the system's media controls.
//!
//! The window cannot play audio through its webview (the custom scheme
//! carries no `Range`), so the page's player drives this one instead, as
//! if it were an audio element:
//!
//! - `player` commands load a play session's track, seek, play, pause and
//!   set the speed. The host reads the track through its stream route and
//!   a confined media worker decodes it
//!   ([`bookclerkd::embedded::EmbeddedHost::open_track`]); the engine
//!   ([`bookclerk_player_engine`]) changes the speed and the rate and
//!   feeds the output device, which [`Output`] opens on its own thread.
//! - `player_events` hands the page a channel for what happens: the
//!   track's length, the position, play, pause, the end, errors, and the
//!   buttons pressed in the system's media controls.
//! - The page describes what is playing (`describe`, `state`, `position`)
//!   so the system's media controls show it: MPRIS on Linux, the System
//!   Media Transport Controls on Windows, Now Playing on macOS
//!   ([`souvlaki`]). Those are driven from the main thread only.
//!
//! The page passes plain values: every command is parsed into
//! [`PlayerCommand`] and checked before anything runs, and a track must be
//! a play session's track path, which the host checks again.

use std::cell::RefCell;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bookclerk_player_engine::{
    OutputFormat, PlayerEvent, Renderer, TrackHeader, TrackOpener, TrackPlayer, TrackStream,
    MAX_SPEED, MIN_SPEED,
};
use bookclerkd::embedded::{EmbeddedHost, PcmStream};
use serde::Deserialize;
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager as _};
use tokio::runtime::Handle;

/// Longest position or length the page may state, in milliseconds: sixty
/// days, the playback core's own limit.
const MAX_POSITION_MS: u64 = 60 * 24 * 60 * 60 * 1000;

/// Longest title or author line passed to the system, in bytes.
const MAX_TEXT_BYTES: usize = 512;

/// Most bytes of a track path.
const MAX_TRACK_BYTES: usize = 256;

/// The name the system's media controls show, and the D-Bus name on Linux
/// (`org.mpris.MediaPlayer2.bookclerk`).
const CONTROLS_NAME: &str = "Bookclerk";

/// The D-Bus name suffix on Linux.
const DBUS_NAME: &str = "bookclerk";

/// What the page may ask of the player.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum PlayerCommand {
    /// Load a play session's track at a position.
    Load {
        /// The track's stream path, as the session listed it.
        track: String,
        /// Milliseconds into the track.
        position_ms: u64,
    },
    /// Play the loaded track.
    Play,
    /// Pause.
    Pause,
    /// Move within the loaded track.
    Seek {
        /// Milliseconds into the track.
        position_ms: u64,
    },
    /// Set the speed.
    Speed {
        /// 0.5 to 3.
        speed: f32,
    },
    /// Unload the track.
    Stop,
    /// Read a track's length without playing it.
    Probe {
        /// The track's stream path.
        track: String,
    },
    /// What the system's media controls show; no title clears them.
    Describe {
        /// Book title.
        title: Option<String>,
        /// Authors.
        artist: Option<String>,
    },
    /// Whether the system's media controls show playing or paused.
    State {
        /// `none`, `paused` or `playing`.
        state: ControlsState,
    },
    /// The position the system's media controls show, on the edition's
    /// whole timeline.
    Position {
        /// The edition's length in milliseconds.
        duration_ms: u64,
        /// Milliseconds from its start.
        position_ms: u64,
        /// The speed.
        rate: f32,
    },
}

/// What the system's media controls show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ControlsState {
    /// Nothing loaded.
    None,
    /// Loaded and paused.
    Paused,
    /// Playing.
    Playing,
}

/// Why a command was refused before it ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    /// A position or length over [`MAX_POSITION_MS`].
    PositionOutOfRange,
    /// A speed outside 0.5 to 3, or not a number.
    SpeedOutOfRange,
    /// A track path that is empty, too long, or not a path.
    BadTrack,
    /// A title or author over [`MAX_TEXT_BYTES`].
    TextTooLong,
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PositionOutOfRange => "the position is out of range",
            Self::SpeedOutOfRange => "the speed is out of range",
            Self::BadTrack => "not a track path",
            Self::TextTooLong => "the text is too long",
        })
    }
}

impl PlayerCommand {
    /// Checks every value against its limit.
    ///
    /// # Errors
    ///
    /// The first [`CommandError`] found.
    pub fn validate(&self) -> Result<(), CommandError> {
        let position = |ms: u64| {
            if ms <= MAX_POSITION_MS {
                Ok(())
            } else {
                Err(CommandError::PositionOutOfRange)
            }
        };
        let speed = |speed: f32| {
            if speed.is_finite() && (MIN_SPEED..=MAX_SPEED).contains(&speed) {
                Ok(())
            } else {
                Err(CommandError::SpeedOutOfRange)
            }
        };
        let track = |track: &str| {
            if track.starts_with('/')
                && track.len() <= MAX_TRACK_BYTES
                && track.bytes().all(|byte| byte.is_ascii_graphic())
            {
                Ok(())
            } else {
                Err(CommandError::BadTrack)
            }
        };
        let text = |text: &Option<String>| {
            if text
                .as_ref()
                .is_none_or(|text| text.len() <= MAX_TEXT_BYTES)
            {
                Ok(())
            } else {
                Err(CommandError::TextTooLong)
            }
        };
        match self {
            Self::Load {
                track: t,
                position_ms,
            } => {
                track(t)?;
                position(*position_ms)
            }
            Self::Seek { position_ms } => position(*position_ms),
            Self::Speed { speed: s } => speed(*s),
            Self::Probe { track: t } => track(t),
            Self::Describe { title, artist } => {
                text(title)?;
                text(artist)
            }
            Self::Position {
                duration_ms,
                position_ms,
                rate,
            } => {
                position(*duration_ms)?;
                position(*position_ms)?;
                speed(*rate)
            }
            Self::Play | Self::Pause | Self::Stop | Self::State { .. } => Ok(()),
        }
    }
}

/// What the page hears, as JSON text on its channel.
///
/// # Returns
///
/// The event's JSON.
pub fn event_json(event: &PlayerEvent) -> String {
    let value = match event {
        PlayerEvent::Loaded {
            duration_ms,
            position_ms,
        } => {
            serde_json::json!({"type": "loaded", "durationMs": duration_ms, "positionMs": position_ms})
        }
        PlayerEvent::Time { position_ms } => {
            serde_json::json!({"type": "time", "positionMs": position_ms})
        }
        PlayerEvent::Playing => serde_json::json!({"type": "playing"}),
        PlayerEvent::Paused => serde_json::json!({"type": "paused"}),
        PlayerEvent::Ended => serde_json::json!({"type": "ended"}),
        PlayerEvent::Error { message } => serde_json::json!({"type": "error", "message": message}),
    };
    value.to_string()
}

/// What the page hears when a button in the system's media controls is
/// pressed, named as the Media Session API names its actions.
///
/// # Returns
///
/// The JSON text, or `None` for a control the page does not answer.
pub fn remote_json(event: &MediaControlEvent) -> Option<String> {
    let seconds = |duration: &Duration| duration.as_secs_f64();
    let value = match event {
        MediaControlEvent::Play => serde_json::json!({"type": "remote", "action": "play"}),
        MediaControlEvent::Pause => serde_json::json!({"type": "remote", "action": "pause"}),
        MediaControlEvent::Toggle => serde_json::json!({"type": "remote", "action": "toggle"}),
        MediaControlEvent::Stop => serde_json::json!({"type": "remote", "action": "pause"}),
        MediaControlEvent::Next => serde_json::json!({"type": "remote", "action": "nexttrack"}),
        MediaControlEvent::Previous => {
            serde_json::json!({"type": "remote", "action": "previoustrack"})
        }
        MediaControlEvent::Seek(direction) => serde_json::json!({
            "type": "remote",
            "action": seek_action(*direction),
        }),
        MediaControlEvent::SeekBy(direction, by) => serde_json::json!({
            "type": "remote",
            "action": seek_action(*direction),
            "seekOffset": seconds(by),
        }),
        MediaControlEvent::SetPosition(MediaPosition(at)) => serde_json::json!({
            "type": "remote",
            "action": "seekto",
            "seekTime": seconds(at),
        }),
        _ => return None,
    };
    Some(value.to_string())
}

/// The Media Session action for a seek direction.
fn seek_action(direction: SeekDirection) -> &'static str {
    match direction {
        SeekDirection::Forward => "seekforward",
        SeekDirection::Backward => "seekbackward",
    }
}

/// Locks a mutex, recovering it from a panicked holder.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A play session's tracks, read from the in-process host.
struct HostTracks(
    /// The host.
    Arc<EmbeddedHost>,
);

/// One decoded track.
struct HostStream(
    /// The media worker's stream.
    PcmStream,
);

impl TrackStream for HostStream {
    fn header(&self) -> TrackHeader {
        let header = self.0.header();
        TrackHeader {
            sample_rate: header.sample_rate,
            channels: u16::from(header.channels),
            start_ms: header.start_ms,
            duration_ms: header.duration_ms,
        }
    }

    async fn next_chunk(&mut self) -> Option<Result<Vec<i16>, String>> {
        self.0
            .next_chunk()
            .await
            .map(|chunk| chunk.map_err(|err| err.to_string()))
    }
}

impl TrackOpener for HostTracks {
    type Stream = HostStream;

    async fn open(&self, track: &str, start_ms: u64) -> Result<HostStream, String> {
        self.0
            .open_track(track, start_ms)
            .await
            .map(HostStream)
            .map_err(|err| err.to_string())
    }
}

/// The output device's stream, kept on its own thread (a device stream
/// may not move between threads on every platform).
struct Output {
    /// Dropped to stop the stream and end the thread.
    _stop: std::sync::mpsc::Sender<()>,
}

impl Output {
    /// Opens the default output device, pulling from a new renderer.
    ///
    /// # Errors
    ///
    /// Why no device could be opened, in words for the listener.
    fn open() -> Result<(Self, Arc<Mutex<Renderer>>), String> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("bookclerk-audio".into())
            .spawn(move || match start_device() {
                Ok((stream, renderer)) => {
                    let _ = ready_tx.send(Ok(renderer));
                    // Runs until the player goes away.
                    let _ = stop_rx.recv();
                    drop(stream);
                }
                Err(message) => {
                    let _ = ready_tx.send(Err(message));
                }
            })
            .map_err(|err| format!("could not start audio: {err}"))?;
        let renderer = ready_rx
            .recv()
            .map_err(|_| "the audio thread ended".to_owned())??;
        Ok((Self { _stop: stop_tx }, renderer))
    }
}

/// Opens the default output device and starts it pulling from a renderer.
fn start_device() -> Result<(cpal::Stream, Arc<Mutex<Renderer>>), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| "no audio output device".to_owned())?;
    let supported = device
        .default_output_config()
        .map_err(|err| format!("the audio device has no usable format: {err}"))?;
    let format = OutputFormat {
        sample_rate: supported.sample_rate(),
        channels: supported.channels(),
    };
    let renderer = Arc::new(Mutex::new(Renderer::new(format)));
    let config = supported.config();
    let on_error = |err: cpal::Error| tracing::warn!(error = %err, "audio output error");
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
            config,
            pull(Arc::clone(&renderer), |sample| sample),
            on_error,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
            config,
            pull(Arc::clone(&renderer), cpal::Sample::from_sample::<f32>),
            on_error,
            None,
        ),
        cpal::SampleFormat::I32 => device.build_output_stream::<i32, _, _>(
            config,
            pull(Arc::clone(&renderer), cpal::Sample::from_sample::<f32>),
            on_error,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_output_stream::<u16, _, _>(
            config,
            pull(Arc::clone(&renderer), cpal::Sample::from_sample::<f32>),
            on_error,
            None,
        ),
        other => {
            return Err(format!(
                "the audio device's {other} samples are not supported"
            ))
        }
    }
    .map_err(|err| format!("could not open the audio device: {err}"))?;
    stream
        .play()
        .map_err(|err| format!("could not start the audio device: {err}"))?;
    Ok((stream, renderer))
}

/// The device callback: renders into a scratch buffer and converts each
/// sample. A renderer busy for an instant (a chunk being added) gives one
/// buffer of silence rather than a wait on the audio thread.
fn pull<T: Copy + Default + Send + 'static>(
    renderer: Arc<Mutex<Renderer>>,
    convert: fn(f32) -> T,
) -> impl FnMut(&mut [T], &cpal::OutputCallbackInfo) + Send + 'static {
    let mut scratch = Vec::new();
    move |out: &mut [T], _info| {
        scratch.resize(out.len(), 0.0f32);
        match renderer.try_lock() {
            Ok(mut renderer) => renderer.render(&mut scratch),
            Err(_) => scratch.fill(0.0),
        }
        for (slot, sample) in out.iter_mut().zip(&scratch) {
            *slot = convert(sample.clamp(-1.0, 1.0));
        }
    }
}

thread_local! {
    /// The system's media controls; made and used on the main thread only.
    static CONTROLS: RefCell<Option<MediaControls>> = const { RefCell::new(None) };
}

/// What the media controls show, kept so a state change can restate the
/// position.
#[derive(Debug, Clone, Default)]
struct Shown {
    /// Whether playing.
    state: Option<ControlsState>,
    /// The last position stated, in milliseconds.
    position_ms: u64,
    /// The title shown, if any.
    title: Option<String>,
    /// The author shown, if any.
    artist: Option<String>,
    /// The length shown, in milliseconds, once a position stated it.
    duration_ms: Option<u64>,
}

/// The window's player.
pub struct NativePlayer {
    /// The in-process host.
    host: Arc<EmbeddedHost>,
    /// The runtime the engine runs on.
    runtime: Handle,
    /// The running app, for the main thread.
    app: AppHandle,
    /// The engine and the device, once something was loaded.
    running: Mutex<Option<(TrackPlayer<HostTracks>, Output)>>,
    /// The page's channel for events; replaced when the page subscribes
    /// again.
    events: Arc<Mutex<Option<Channel<String>>>>,
    /// What the media controls show.
    shown: Mutex<Shown>,
}

impl NativePlayer {
    /// A player for the window of `app`, reading tracks from `host`.
    pub fn new(host: Arc<EmbeddedHost>, runtime: Handle, app: AppHandle) -> Self {
        Self {
            host,
            runtime,
            app,
            running: Mutex::new(None),
            events: Arc::new(Mutex::new(None)),
            shown: Mutex::new(Shown::default()),
        }
    }

    /// Sends the page's events to `channel` from now on.
    pub fn subscribe(&self, channel: Channel<String>) {
        *lock(&self.events) = Some(channel);
    }

    /// Runs one checked command.
    ///
    /// # Returns
    ///
    /// For `probe`, the track's length in milliseconds when known;
    /// otherwise `None`.
    ///
    /// # Errors
    ///
    /// Why the command could not run, in words for the listener.
    pub async fn run(&self, command: PlayerCommand) -> Result<Option<u64>, String> {
        command.validate().map_err(|err| err.to_string())?;
        match command {
            PlayerCommand::Load { track, position_ms } => {
                self.with_engine(|player| player.load(&track, position_ms))?;
            }
            PlayerCommand::Play => self.if_running(TrackPlayer::play),
            PlayerCommand::Pause => self.if_running(TrackPlayer::pause),
            PlayerCommand::Seek { position_ms } => {
                self.if_running(|player| player.seek(position_ms))
            }
            PlayerCommand::Speed { speed } => self.if_running(|player| player.set_speed(speed)),
            PlayerCommand::Stop => self.if_running(TrackPlayer::stop),
            PlayerCommand::Probe { track } => {
                let stream = self
                    .host
                    .open_track(&track, 0)
                    .await
                    .map_err(|err| err.to_string())?;
                return Ok(stream.header().duration_ms);
            }
            PlayerCommand::Describe { title, artist } => self.describe(title, artist),
            PlayerCommand::State { state } => {
                let position_ms = {
                    let mut shown = lock(&self.shown);
                    shown.state = Some(state);
                    shown.position_ms
                };
                self.on_main(move |controls| set_playback(controls, state, position_ms));
            }
            PlayerCommand::Position {
                duration_ms,
                position_ms,
                ..
            } => {
                let (state, retitle) = {
                    let mut shown = lock(&self.shown);
                    shown.position_ms = position_ms;
                    // The length goes in the metadata, so state it again
                    // when it first arrives or changes.
                    let retitle = (shown.title.is_some() && shown.duration_ms != Some(duration_ms))
                        .then(|| {
                            shown.duration_ms = Some(duration_ms);
                            (shown.title.clone(), shown.artist.clone())
                        });
                    (shown.state, retitle)
                };
                if let Some((title, artist)) = retitle {
                    self.on_main(move |controls| {
                        let metadata = MediaMetadata {
                            title: title.as_deref(),
                            artist: artist.as_deref(),
                            duration: Some(Duration::from_millis(duration_ms)),
                            ..MediaMetadata::default()
                        };
                        if let Err(err) = controls.set_metadata(metadata) {
                            tracing::debug!(error = ?err, "media controls refused the length");
                        }
                    });
                }
                if let Some(state) = state {
                    self.on_main(move |controls| set_playback(controls, state, position_ms));
                }
            }
        }
        Ok(None)
    }

    /// Runs `action` on the engine, starting it (and the device) first.
    fn with_engine(&self, action: impl FnOnce(&TrackPlayer<HostTracks>)) -> Result<(), String> {
        let mut running = lock(&self.running);
        if running.is_none() {
            let (output, renderer) = Output::open()?;
            let (player, mut events) =
                TrackPlayer::new(HostTracks(Arc::clone(&self.host)), renderer, &self.runtime);
            let channel = Arc::clone(&self.events);
            self.runtime.spawn(async move {
                while let Some(event) = events.recv().await {
                    if let Some(channel) = lock(&channel).as_ref() {
                        let _ = channel.send(event_json(&event));
                    }
                }
            });
            *running = Some((player, output));
        }
        if let Some((player, _)) = running.as_ref() {
            action(player);
        }
        Ok(())
    }

    /// Runs `action` on the engine when it is running.
    fn if_running(&self, action: impl FnOnce(&TrackPlayer<HostTracks>)) {
        if let Some((player, _)) = lock(&self.running).as_ref() {
            action(player);
        }
    }

    /// Shows a title in the media controls, or clears them.
    fn describe(&self, title: Option<String>, artist: Option<String>) {
        {
            let mut shown = lock(&self.shown);
            if title.is_none() {
                *shown = Shown::default();
            } else {
                shown.title.clone_from(&title);
                shown.artist.clone_from(&artist);
                shown.duration_ms = None;
            }
        }
        let app = self.app.clone();
        let events = Arc::clone(&self.events);
        self.on_main_or_attach(app, events, move |controls| {
            let metadata = MediaMetadata {
                title: title.as_deref(),
                artist: artist.as_deref(),
                ..MediaMetadata::default()
            };
            let outcome = match title {
                Some(_) => controls.set_metadata(metadata),
                None => controls
                    .set_metadata(MediaMetadata::default())
                    .and_then(|()| controls.set_playback(MediaPlayback::Stopped)),
            };
            if let Err(err) = outcome {
                tracing::debug!(error = ?err, "media controls refused the metadata");
            }
        });
    }

    /// Runs `update` on the media controls, on the main thread, when they
    /// exist.
    fn on_main(&self, update: impl FnOnce(&mut MediaControls) + Send + 'static) {
        let queued = self.app.run_on_main_thread(move || {
            CONTROLS.with_borrow_mut(|controls| {
                if let Some(controls) = controls.as_mut() {
                    update(controls);
                }
            });
        });
        if let Err(err) = queued {
            tracing::debug!(error = %err, "could not reach the main thread");
        }
    }

    /// Like [`Self::on_main`], making the media controls first if need be.
    fn on_main_or_attach(
        &self,
        app: AppHandle,
        events: Arc<Mutex<Option<Channel<String>>>>,
        update: impl FnOnce(&mut MediaControls) + Send + 'static,
    ) {
        let queued = self.app.run_on_main_thread(move || {
            CONTROLS.with_borrow_mut(|controls| {
                if controls.is_none() {
                    *controls = attach_controls(&app, events);
                }
                if let Some(controls) = controls.as_mut() {
                    update(controls);
                }
            });
        });
        if let Err(err) = queued {
            tracing::debug!(error = %err, "could not reach the main thread");
        }
    }
}

/// States play or pause, with the position, in the media controls.
fn set_playback(controls: &mut MediaControls, state: ControlsState, position_ms: u64) {
    let progress = Some(MediaPosition(Duration::from_millis(position_ms)));
    let playback = match state {
        ControlsState::None => MediaPlayback::Stopped,
        ControlsState::Paused => MediaPlayback::Paused { progress },
        ControlsState::Playing => MediaPlayback::Playing { progress },
    };
    if let Err(err) = controls.set_playback(playback) {
        tracing::debug!(error = ?err, "media controls refused the state");
    }
}

/// Makes the system's media controls for the main window and routes their
/// buttons to the page. Runs on the main thread.
///
/// # Returns
///
/// `None` when this system has none to offer (no session bus, for one);
/// the player still plays.
fn attach_controls(
    app: &AppHandle,
    events: Arc<Mutex<Option<Channel<String>>>>,
) -> Option<MediaControls> {
    #[cfg(windows)]
    let hwnd = app
        .get_webview_window(bookclerkd::embedded::MAIN_WINDOW_LABEL)
        .and_then(|window| window.hwnd().ok())
        .map(|hwnd| hwnd.0);
    #[cfg(not(windows))]
    let hwnd = None;
    let config = PlatformConfig {
        display_name: CONTROLS_NAME,
        dbus_name: DBUS_NAME,
        hwnd,
    };
    let mut controls = match MediaControls::new(config) {
        Ok(controls) => controls,
        Err(err) => {
            tracing::info!(error = ?err, "no system media controls");
            return None;
        }
    };
    let raise = app.clone();
    let attached = controls.attach(move |event| {
        if matches!(event, MediaControlEvent::Raise) {
            if let Some(window) = raise.get_webview_window(bookclerkd::embedded::MAIN_WINDOW_LABEL)
            {
                let _ = window.set_focus();
            }
            return;
        }
        if let Some(json) = remote_json(&event) {
            if let Some(channel) = lock(&events).as_ref() {
                let _ = channel.send(json);
            }
        }
    });
    if let Err(err) = attached {
        tracing::info!(error = ?err, "the system media controls could not be attached");
        return None;
    }
    Some(controls)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<PlayerCommand, String> {
        serde_json::from_str(json).map_err(|err| err.to_string())
    }

    #[test]
    fn commands_parse_from_the_pages_json() {
        assert_eq!(
            parse(r#"{"kind":"load","track":"/api/playback/sessions/x/tracks/0","positionMs":5}"#),
            Ok(PlayerCommand::Load {
                track: "/api/playback/sessions/x/tracks/0".into(),
                position_ms: 5
            })
        );
        assert_eq!(parse(r#"{"kind":"play"}"#), Ok(PlayerCommand::Play));
        assert_eq!(
            parse(r#"{"kind":"state","state":"playing"}"#),
            Ok(PlayerCommand::State {
                state: ControlsState::Playing
            })
        );
        assert_eq!(
            parse(r#"{"kind":"position","durationMs":10,"positionMs":2,"rate":1.5}"#),
            Ok(PlayerCommand::Position {
                duration_ms: 10,
                position_ms: 2,
                rate: 1.5
            })
        );
        assert!(parse(r#"{"kind":"seek","positionMs":1,"extra":1}"#).is_err());
        assert!(parse(r#"{"kind":"seek","positionMs":-1}"#).is_err());
        assert!(parse(r#"{"kind":"launch"}"#).is_err());
    }

    #[test]
    fn commands_out_of_range_are_refused() {
        let check = |json: &str| parse(json).unwrap().validate();
        assert_eq!(
            check(r#"{"kind":"seek","positionMs":5184000001}"#),
            Err(CommandError::PositionOutOfRange)
        );
        assert_eq!(
            check(r#"{"kind":"speed","speed":3.5}"#),
            Err(CommandError::SpeedOutOfRange)
        );
        assert_eq!(check(r#"{"kind":"speed","speed":2}"#), Ok(()));
        assert_eq!(
            check(r#"{"kind":"probe","track":"relative"}"#),
            Err(CommandError::BadTrack)
        );
        assert_eq!(
            check(r#"{"kind":"probe","track":"/has space"}"#),
            Err(CommandError::BadTrack)
        );
        let long = "x".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(
            check(&format!(r#"{{"kind":"describe","title":"{long}"}}"#)),
            Err(CommandError::TextTooLong)
        );
        assert_eq!(check(r#"{"kind":"describe"}"#), Ok(()));
    }

    #[test]
    fn events_and_remote_buttons_become_the_pages_json() {
        assert_eq!(
            event_json(&PlayerEvent::Loaded {
                duration_ms: None,
                position_ms: 3
            }),
            r#"{"durationMs":null,"positionMs":3,"type":"loaded"}"#
        );
        assert_eq!(event_json(&PlayerEvent::Ended), r#"{"type":"ended"}"#);
        assert_eq!(
            remote_json(&MediaControlEvent::SeekBy(
                SeekDirection::Backward,
                Duration::from_secs(10)
            ))
            .as_deref(),
            Some(r#"{"action":"seekbackward","seekOffset":10.0,"type":"remote"}"#)
        );
        assert_eq!(
            remote_json(&MediaControlEvent::SetPosition(MediaPosition(
                Duration::from_millis(1_500)
            )))
            .as_deref(),
            Some(r#"{"action":"seekto","seekTime":1.5,"type":"remote"}"#)
        );
        assert_eq!(remote_json(&MediaControlEvent::Quit), None);
    }
}
