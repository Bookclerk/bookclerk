//! Plays one track at a time: opens its decoded stream, keeps the
//! [`Renderer`] fed, and reports what happens.
//!
//! The web app's player decides what to play and when (tracks, sessions,
//! reports, the sleep timer); this is the audio element it drives in the
//! desktop window. Like an audio element it plays one track: the page
//! loads a track, seeks within it, plays, pauses and sets the speed, and
//! hears back [`PlayerEvent`]s. A seek opens a new stream from the new
//! position, because the decoder runs in a separate process that streams
//! forward only.
//!
//! The device side is not here: whatever plays audio calls
//! [`Renderer::render`] on the shared renderer from its own thread.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::renderer::Renderer;

/// Decoded audio kept ahead of the output, in milliseconds of the track.
/// A short stall in the decoder or the storage is covered by this.
const AHEAD_MS: u64 = 4_000;

/// How often the feeder looks again when enough is buffered, and the
/// clock reports the position while playing.
const TICK: Duration = Duration::from_millis(250);

/// The layout and start of a decoded stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackHeader {
    /// Decoded frames per second.
    pub sample_rate: u32,
    /// Decoded channels: 1 or 2.
    pub channels: u16,
    /// Where the first sample lies on the track, in milliseconds.
    pub start_ms: u64,
    /// The track's length in milliseconds, when known.
    pub duration_ms: Option<u64>,
}

/// One track's decoded samples from a start position.
pub trait TrackStream: Send + 'static {
    /// The stream's layout and start.
    fn header(&self) -> TrackHeader;

    /// The next chunk of interleaved samples; `None` at the end.
    ///
    /// # Errors
    ///
    /// Why the rest cannot be decoded, in words for a log.
    fn next_chunk(&mut self) -> impl Future<Output = Option<Result<Vec<i16>, String>>> + Send;
}

/// Opens tracks.
pub trait TrackOpener: Send + Sync + 'static {
    /// What an opened track streams.
    type Stream: TrackStream;

    /// Opens `track` from `start_ms`.
    ///
    /// # Errors
    ///
    /// Why the track cannot be played, in words for the listener.
    fn open(
        &self,
        track: &str,
        start_ms: u64,
    ) -> impl Future<Output = Result<Self::Stream, String>> + Send;
}

/// What the player reports, in the order it happens.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    /// A track's stream opened: its length, when known, and where it
    /// starts.
    Loaded {
        /// The track's length in milliseconds.
        duration_ms: Option<u64>,
        /// Where playback starts on the track, in milliseconds.
        position_ms: u64,
    },
    /// The position while playing, about four times a second, and after
    /// each seek.
    Time {
        /// Milliseconds into the track.
        position_ms: u64,
    },
    /// Playback started or resumed.
    Playing,
    /// Playback paused.
    Paused,
    /// The track played to its end; the player is paused.
    Ended,
    /// The track could not be opened or decoded further; the player is
    /// paused.
    Error {
        /// What went wrong, for the listener.
        message: String,
    },
}

/// What the player holds between calls.
#[derive(Debug, Default)]
struct State {
    /// The loaded track.
    track: Option<String>,
    /// The task feeding the renderer.
    feeder: Option<JoinHandle<()>>,
    /// Whether the listener asked to play.
    playing: bool,
    /// Whether `Ended` was reported for the current stream.
    ended: bool,
    /// The last position reported or sought to.
    position_ms: u64,
}

/// The player's shared parts.
struct Shared<O> {
    /// Opens tracks.
    opener: O,
    /// The pipeline the device pulls from.
    renderer: Arc<Mutex<Renderer>>,
    /// Where events go.
    events: mpsc::UnboundedSender<PlayerEvent>,
    /// The player's state.
    state: Mutex<State>,
    /// The runtime the feeder and the clock run on.
    runtime: Handle,
}

/// A one-track player over a shared [`Renderer`].
pub struct TrackPlayer<O: TrackOpener> {
    /// Everything the tasks share.
    shared: Arc<Shared<O>>,
    /// The clock task; stopped with the player.
    clock: JoinHandle<()>,
}

impl<O: TrackOpener> std::fmt::Debug for TrackPlayer<O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackPlayer").finish_non_exhaustive()
    }
}

impl<O: TrackOpener> Drop for TrackPlayer<O> {
    fn drop(&mut self) {
        self.clock.abort();
        if let Some(feeder) = lock(&self.shared.state).feeder.take() {
            feeder.abort();
        }
        lock(&self.shared.renderer).clear();
    }
}

/// Locks a mutex, recovering it from a panicked holder: the state stays
/// usable, at worst one stale field.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<O: TrackOpener> TrackPlayer<O> {
    /// A player that opens tracks with `opener` and feeds `renderer`.
    ///
    /// # Arguments
    ///
    /// * `opener` - Opens a track's decoded stream.
    /// * `renderer` - The pipeline the device pulls from.
    /// * `runtime` - Where the feeder and the clock run.
    ///
    /// # Returns
    ///
    /// The player, and the receiving end of its events.
    pub fn new(
        opener: O,
        renderer: Arc<Mutex<Renderer>>,
        runtime: &Handle,
    ) -> (Self, mpsc::UnboundedReceiver<PlayerEvent>) {
        let (events, receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            opener,
            renderer,
            events,
            state: Mutex::new(State::default()),
            runtime: runtime.clone(),
        });
        let clock = runtime.spawn(clock(Arc::downgrade(&shared)));
        (Self { shared, clock }, receiver)
    }

    /// Loads `track` and positions it at `start_ms`, keeping play or
    /// pause as it was. Replaces any track loaded before.
    pub fn load(&self, track: &str, start_ms: u64) {
        let mut state = lock(&self.shared.state);
        state.track = Some(track.to_owned());
        drop(state);
        self.restart(start_ms);
    }

    /// Moves to `position_ms` in the loaded track.
    pub fn seek(&self, position_ms: u64) {
        if lock(&self.shared.state).track.is_some() {
            self.restart(position_ms);
        }
    }

    /// Plays the loaded track (or, after its end, plays it again from the
    /// start).
    pub fn play(&self) {
        let mut state = lock(&self.shared.state);
        if state.track.is_none() || state.playing {
            return;
        }
        let ended = state.ended;
        state.playing = true;
        drop(state);
        if ended {
            self.restart(0);
        }
        lock(&self.shared.renderer).set_playing(true);
        let _ = self.shared.events.send(PlayerEvent::Playing);
    }

    /// Pauses.
    pub fn pause(&self) {
        let mut state = lock(&self.shared.state);
        if !state.playing {
            return;
        }
        state.playing = false;
        drop(state);
        let mut renderer = lock(&self.shared.renderer);
        renderer.set_playing(false);
        let position = renderer.position_ms();
        drop(renderer);
        if let Some(position_ms) = position {
            lock(&self.shared.state).position_ms = position_ms;
        }
        let _ = self.shared.events.send(PlayerEvent::Paused);
    }

    /// Sets the speed (0.5 to 3; anything else is clamped).
    pub fn set_speed(&self, speed: f32) {
        lock(&self.shared.renderer).set_speed(speed);
    }

    /// Unloads the track and stops.
    pub fn stop(&self) {
        let mut state = lock(&self.shared.state);
        if let Some(feeder) = state.feeder.take() {
            feeder.abort();
        }
        let was_playing = state.playing;
        *state = State::default();
        drop(state);
        let mut renderer = lock(&self.shared.renderer);
        renderer.clear();
        renderer.set_playing(false);
        drop(renderer);
        if was_playing {
            let _ = self.shared.events.send(PlayerEvent::Paused);
        }
    }

    /// The position in the loaded track, in milliseconds.
    #[must_use]
    pub fn position_ms(&self) -> u64 {
        let rendered = lock(&self.shared.renderer).position_ms();
        rendered.unwrap_or_else(|| lock(&self.shared.state).position_ms)
    }

    /// Opens the loaded track afresh at `position_ms`.
    fn restart(&self, position_ms: u64) {
        let mut state = lock(&self.shared.state);
        if let Some(feeder) = state.feeder.take() {
            feeder.abort();
        }
        let Some(track) = state.track.clone() else {
            return;
        };
        state.ended = false;
        state.position_ms = position_ms;
        lock(&self.shared.renderer).clear();
        let shared = Arc::clone(&self.shared);
        state.feeder = Some(
            self.shared
                .runtime
                .spawn(async move { feed(shared, track, position_ms).await }),
        );
        drop(state);
        let _ = self.shared.events.send(PlayerEvent::Time { position_ms });
    }
}

/// Opens `track` at `start_ms` and keeps the renderer fed until the end.
async fn feed<O: TrackOpener>(shared: Arc<Shared<O>>, track: String, start_ms: u64) {
    let mut stream = match shared.opener.open(&track, start_ms).await {
        Ok(stream) => stream,
        Err(message) => return fail(&shared, message),
    };
    let header = stream.header();
    let generation =
        lock(&shared.renderer).begin(header.sample_rate, header.channels, header.start_ms);
    let _ = shared.events.send(PlayerEvent::Loaded {
        duration_ms: header.duration_ms,
        position_ms: header.start_ms,
    });
    loop {
        if lock(&shared.renderer).buffered_ms() >= AHEAD_MS {
            tokio::time::sleep(TICK / 2).await;
            continue;
        }
        match stream.next_chunk().await {
            Some(Ok(samples)) => {
                if !lock(&shared.renderer).push(generation, &samples) {
                    return;
                }
            }
            Some(Err(message)) => return fail(&shared, message),
            None => {
                lock(&shared.renderer).finish_input(generation);
                return;
            }
        }
    }
}

/// Pauses on a failure and reports it.
fn fail<O: TrackOpener>(shared: &Shared<O>, message: String) {
    lock(&shared.state).playing = false;
    lock(&shared.renderer).set_playing(false);
    let _ = shared.events.send(PlayerEvent::Error { message });
}

/// Reports the position while playing, and the end of a track.
async fn clock<O: TrackOpener>(shared: Weak<Shared<O>>) {
    let mut ticks = tokio::time::interval(TICK);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticks.tick().await;
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let mut state = lock(&shared.state);
        if !state.playing {
            continue;
        }
        let renderer = lock(&shared.renderer);
        let ended = renderer.is_ended();
        let position = renderer.position_ms();
        drop(renderer);
        if let Some(position_ms) = position {
            state.position_ms = position_ms;
            let _ = shared.events.send(PlayerEvent::Time { position_ms });
        }
        if ended && !state.ended {
            state.ended = true;
            state.playing = false;
            drop(state);
            lock(&shared.renderer).set_playing(false);
            let _ = shared.events.send(PlayerEvent::Ended);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::OutputFormat;
    use std::collections::VecDeque;

    /// A track of `frames` mono frames at 8 kHz, in 800-frame chunks.
    struct Fake {
        /// Chunks still to hand out.
        chunks: VecDeque<Result<Vec<i16>, String>>,
        /// The header.
        header: TrackHeader,
    }

    impl TrackStream for Fake {
        fn header(&self) -> TrackHeader {
            self.header
        }

        async fn next_chunk(&mut self) -> Option<Result<Vec<i16>, String>> {
            self.chunks.pop_front()
        }
    }

    /// Opens fake tracks: "ok" is two seconds long, "broken" fails after
    /// a second, anything else cannot be opened.
    struct Opener;

    impl TrackOpener for Opener {
        type Stream = Fake;

        async fn open(&self, track: &str, start_ms: u64) -> Result<Fake, String> {
            let total = match track {
                "ok" => 16_000u64,
                "broken" => 8_000,
                _ => return Err("no such track".into()),
            };
            let from = (start_ms * 8).min(total);
            let mut chunks: VecDeque<_> = (from..total)
                .step_by(800)
                .map(|at| {
                    Ok(vec![
                        4_000i16;
                        usize::try_from((total - at).min(800)).unwrap()
                    ])
                })
                .collect();
            if track == "broken" {
                chunks.push_back(Err("bad packet".into()));
            }
            Ok(Fake {
                chunks,
                header: TrackHeader {
                    sample_rate: 8_000,
                    channels: 1,
                    start_ms,
                    duration_ms: Some(2_000),
                },
            })
        }
    }

    /// A device: pulls the renderer in real time from a thread.
    fn device(renderer: &Arc<Mutex<Renderer>>) -> std::thread::JoinHandle<()> {
        let renderer = Arc::downgrade(renderer);
        std::thread::spawn(move || {
            let mut buf = vec![0.0f32; 160];
            while let Some(renderer) = renderer.upgrade() {
                lock(&renderer).render(&mut buf);
                drop(renderer);
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    }

    fn renderer() -> Arc<Mutex<Renderer>> {
        Arc::new(Mutex::new(Renderer::new(OutputFormat {
            sample_rate: 32_000,
            channels: 1,
        })))
    }

    async fn until(
        events: &mut mpsc::UnboundedReceiver<PlayerEvent>,
        wanted: impl Fn(&PlayerEvent) -> bool,
    ) -> Vec<PlayerEvent> {
        let mut seen = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("event in time")
                .expect("player alive");
            let done = wanted(&event);
            seen.push(event);
            if done {
                return seen;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_track_plays_to_its_end_at_double_speed() {
        let renderer = renderer();
        let _device = device(&renderer);
        let (player, mut events) =
            TrackPlayer::new(Opener, Arc::clone(&renderer), &Handle::current());
        player.set_speed(2.0);
        player.load("ok", 0);
        player.play();
        let started = std::time::Instant::now();
        let seen = until(&mut events, |e| *e == PlayerEvent::Ended).await;
        assert!(seen.contains(&PlayerEvent::Loaded {
            duration_ms: Some(2_000),
            position_ms: 0
        }));
        assert!(seen.contains(&PlayerEvent::Playing));
        assert!(seen
            .iter()
            .any(|e| matches!(e, PlayerEvent::Time { position_ms } if *position_ms > 500)));
        // Two seconds of audio at double speed takes about one.
        let took = started.elapsed();
        assert!(took < Duration::from_millis(1_900), "{took:?}");
        assert_eq!(player.position_ms(), 2_000);
        // Play after the end starts again from the top.
        player.play();
        until(&mut events, |e| {
            matches!(e, PlayerEvent::Loaded { position_ms: 0, .. })
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn seeking_reopens_at_the_new_position_and_pause_holds_it() {
        let renderer = renderer();
        let _device = device(&renderer);
        let (player, mut events) =
            TrackPlayer::new(Opener, Arc::clone(&renderer), &Handle::current());
        player.load("ok", 0);
        until(&mut events, |e| matches!(e, PlayerEvent::Loaded { .. })).await;
        player.seek(1_500);
        until(&mut events, |e| {
            matches!(
                e,
                PlayerEvent::Loaded {
                    position_ms: 1_500,
                    ..
                }
            )
        })
        .await;
        assert_eq!(player.position_ms(), 1_500);
        player.play();
        tokio::time::sleep(Duration::from_millis(200)).await;
        player.pause();
        let held = player.position_ms();
        assert!(held > 1_500, "{held}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(player.position_ms(), held);
        let seen = until(&mut events, |e| *e == PlayerEvent::Paused).await;
        assert!(!seen.contains(&PlayerEvent::Ended));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failures_pause_and_say_why() {
        let renderer = renderer();
        let _device = device(&renderer);
        let (player, mut events) =
            TrackPlayer::new(Opener, Arc::clone(&renderer), &Handle::current());
        player.load("missing", 0);
        player.play();
        let seen = until(&mut events, |e| matches!(e, PlayerEvent::Error { .. })).await;
        assert_eq!(
            seen.last(),
            Some(&PlayerEvent::Error {
                message: "no such track".into()
            })
        );
        player.load("broken", 0);
        player.play();
        until(
            &mut events,
            |e| matches!(e, PlayerEvent::Error { message } if message == "bad packet"),
        )
        .await;
        player.stop();
        assert_eq!(player.position_ms(), 0);
    }
}
