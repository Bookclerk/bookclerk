//! Turns decoded samples into what the output device plays: speed change,
//! rate conversion and channel mapping, pulled by the device's callback.
//!
//! A [`Renderer`] holds one stream at a time (a track from a start
//! position). The decoder side calls [`Renderer::begin`], then
//! [`Renderer::push`] as chunks arrive and [`Renderer::finish_input`] at the
//! end; the device side calls [`Renderer::render`] for each buffer it
//! needs. Anything it cannot fill (paused, still loading, or a stall) is
//! silence. [`Renderer::position_ms`] is where the output has got to on
//! the track's own timeline.

use crate::resample::Resampler;
use crate::stretch::Stretcher;

/// Output frames made per step: small enough that a speed change is
/// heard at once.
const STEP_FRAMES: usize = 512;

/// The output device's layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputFormat {
    /// Frames per second the device plays.
    pub sample_rate: u32,
    /// Interleaved channels the device takes; at least 1.
    pub channels: u16,
}

/// The stream being played.
#[derive(Debug)]
struct Stream {
    /// The decoded rate.
    sample_rate: u32,
    /// Decoded channels: 1 or 2.
    channels: usize,
    /// Where the stream starts on the track, in milliseconds.
    start_ms: u64,
    /// Speed change.
    stretch: Stretcher,
    /// Rate conversion to the device.
    resample: Resampler,
    /// Whether the decoder reached the end.
    input_ended: bool,
    /// Whether everything was played out.
    ended: bool,
    /// Which [`Renderer::begin`] call this is, so a decoder that was
    /// replaced cannot push into its successor's stream.
    generation: u64,
}

/// The playback pipeline between the decoder and the device.
#[derive(Debug)]
pub struct Renderer {
    /// The device's layout.
    output: OutputFormat,
    /// The stream, once one began.
    stream: Option<Stream>,
    /// Speed applied to every stream.
    speed: f32,
    /// Whether the device should hear the stream (else silence).
    playing: bool,
    /// The last generation handed out.
    generation: u64,
    /// Stretched samples, reused between steps.
    stretched: Vec<f32>,
    /// Converted samples not yet copied to the device, in the stream's
    /// channels.
    converted: Vec<f32>,
}

impl Renderer {
    /// A renderer for a device with `output`'s layout, paused, at speed 1.
    #[must_use]
    pub fn new(output: OutputFormat) -> Self {
        Self {
            output: OutputFormat {
                sample_rate: output.sample_rate.max(1),
                channels: output.channels.max(1),
            },
            stream: None,
            speed: 1.0,
            playing: false,
            generation: 0,
            stretched: Vec::new(),
            converted: Vec::new(),
        }
    }

    /// The device's layout.
    #[must_use]
    pub fn output(&self) -> OutputFormat {
        self.output
    }

    /// Starts a new stream, dropping any before it.
    ///
    /// # Arguments
    ///
    /// * `sample_rate` - The decoded rate, at least 1.
    /// * `channels` - Decoded channels: 1 or 2.
    /// * `start_ms` - Where the first sample lies on the track.
    ///
    /// # Returns
    ///
    /// The stream's generation, which [`Self::push`] and
    /// [`Self::finish_input`] must name.
    pub fn begin(&mut self, sample_rate: u32, channels: u16, start_ms: u64) -> u64 {
        self.generation += 1;
        let channels = usize::from(channels.clamp(1, 2));
        let mut stretch = Stretcher::new(sample_rate.max(1), channels);
        stretch.set_speed(self.speed);
        self.stream = Some(Stream {
            sample_rate: sample_rate.max(1),
            channels,
            start_ms,
            stretch,
            resample: Resampler::new(sample_rate.max(1), self.output.sample_rate, channels),
            input_ended: false,
            ended: false,
            generation: self.generation,
        });
        self.converted.clear();
        self.generation
    }

    /// Drops the stream: the device hears silence until the next begins.
    /// A decoder still pushing to it is ignored.
    pub fn clear(&mut self) {
        self.generation += 1;
        self.stream = None;
        self.converted.clear();
    }

    /// Adds decoded samples to the stream of `generation`.
    ///
    /// # Returns
    ///
    /// False when that stream was replaced or cleared.
    pub fn push(&mut self, generation: u64, samples: &[i16]) -> bool {
        let Some(stream) = self.stream_of(generation) else {
            return false;
        };
        let scaled: Vec<f32> = samples
            .iter()
            .map(|sample| f32::from(*sample) / 32_768.0)
            .collect();
        stream.stretch.push(&scaled);
        true
    }

    /// Marks the end of the stream of `generation`'s input.
    pub fn finish_input(&mut self, generation: u64) {
        if let Some(stream) = self.stream_of(generation) {
            stream.input_ended = true;
            stream.stretch.finish();
        }
    }

    /// The stream of `generation`, if it is still the current one.
    fn stream_of(&mut self, generation: u64) -> Option<&mut Stream> {
        self.stream
            .as_mut()
            .filter(|stream| stream.generation == generation)
    }

    /// Changes the speed, clamped to what the stretcher allows.
    pub fn set_speed(&mut self, speed: f32) {
        self.speed = speed;
        if let Some(stream) = self.stream.as_mut() {
            stream.stretch.set_speed(speed);
            self.speed = stream.stretch.speed();
        }
    }

    /// Plays or pauses; paused, the device hears silence and the position
    /// holds.
    pub fn set_playing(&mut self, playing: bool) {
        self.playing = playing;
    }

    /// Whether the stream played out to its end.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.stream.as_ref().is_some_and(|stream| stream.ended)
    }

    /// Decoded audio held and not yet played, in milliseconds of the
    /// track.
    #[must_use]
    pub fn buffered_ms(&self) -> u64 {
        self.stream.as_ref().map_or(0, |stream| {
            stream.stretch.buffered_frames() * 1000 / u64::from(stream.sample_rate)
        })
    }

    /// Where the output has got to on the track, in milliseconds; `None`
    /// with no stream.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn position_ms(&self) -> Option<u64> {
        let stream = self.stream.as_ref()?;
        let pending = stream.resample.pending_frames() as f64 * f64::from(stream.stretch.speed())
            + (self.converted.len() / stream.channels) as f64 * f64::from(stream.sample_rate)
                / f64::from(self.output.sample_rate)
                * f64::from(stream.stretch.speed());
        let frames = (stream.stretch.source_frames() - pending).max(0.0);
        Some(stream.start_ms + (frames * 1000.0 / f64::from(stream.sample_rate)) as u64)
    }

    /// Fills one device buffer (interleaved in the device's channels).
    ///
    /// Anything not filled from the stream is silence: while paused, with
    /// no stream, while the decoder catches up, or after the end.
    pub fn render(&mut self, out: &mut [f32]) {
        out.fill(0.0);
        if !self.playing {
            return;
        }
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        let out_channels = usize::from(self.output.channels);
        let wanted = out.len() / out_channels;
        let channels = stream.channels;
        while self.converted.len() / channels < wanted && !stream.ended {
            let missing = wanted - self.converted.len() / channels;
            let made =
                stream
                    .resample
                    .pull(&mut self.converted, missing, stream.stretch.is_drained());
            if made > 0 {
                continue;
            }
            self.stretched.clear();
            if stream.stretch.pull(&mut self.stretched, STEP_FRAMES) > 0 {
                stream.resample.push(&self.stretched);
                continue;
            }
            if stream.input_ended && stream.stretch.is_drained() {
                if stream.resample.pull(&mut self.converted, missing, true) == 0 {
                    stream.ended = true;
                }
                continue;
            }
            // The decoder is behind: the rest of this buffer is silence.
            break;
        }
        let frames = (self.converted.len() / channels).min(wanted);
        for (frame, slot) in out.chunks_mut(out_channels).take(frames).enumerate() {
            let source = &self.converted[frame * channels..(frame + 1) * channels];
            map_channels(source, slot);
        }
        self.converted.drain(..frames * channels);
    }
}

/// Copies one frame from 1 or 2 channels to the device's: mono goes to
/// every channel, stereo to the first two (and their mean to a mono
/// device); extra device channels stay silent.
fn map_channels(source: &[f32], slot: &mut [f32]) {
    match (source, slot.len()) {
        ([mono], _) => slot.fill(*mono),
        ([left, right], 1) => slot[0] = (left + right) / 2.0,
        ([left, right, ..], _) => {
            slot[0] = *left;
            slot[1] = *right;
        }
        _ => {}
    }
}

#[cfg(test)]
// Test signals are built from small, known values.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
mod tests {
    use super::*;

    fn device(rate: u32, channels: u16) -> Renderer {
        Renderer::new(OutputFormat {
            sample_rate: rate,
            channels,
        })
    }

    fn drain(renderer: &mut Renderer, frames: usize, channels: usize) -> Vec<f32> {
        let mut all = Vec::new();
        let mut buf = vec![0.0; 256 * channels];
        while all.len() < frames * channels && !renderer.is_ended() {
            renderer.render(&mut buf);
            all.extend_from_slice(&buf);
        }
        all
    }

    #[test]
    fn paused_or_empty_is_silence() {
        let mut renderer = device(48_000, 2);
        let mut buf = vec![1.0; 64];
        renderer.render(&mut buf);
        assert!(buf.iter().all(|s| *s == 0.0));
        let generation = renderer.begin(48_000, 1, 0);
        renderer.push(generation, &[1000; 64]);
        buf.fill(1.0);
        renderer.render(&mut buf);
        assert!(buf.iter().all(|s| *s == 0.0), "paused plays nothing");
        assert_eq!(renderer.position_ms(), Some(0));
    }

    #[test]
    fn a_stream_plays_to_its_end_and_reports_its_position() {
        let mut renderer = device(8_000, 2);
        renderer.set_playing(true);
        let generation = renderer.begin(8_000, 1, 5_000);
        renderer.push(generation, &vec![16_384; 8_000]);
        let first = drain(&mut renderer, 4_000, 2);
        assert!((first[0] - 0.5).abs() < 1e-3 && (first[1] - 0.5).abs() < 1e-3);
        let at = renderer.position_ms().unwrap();
        assert!((5_400..=5_600).contains(&at), "{at}");
        renderer.finish_input(generation);
        drain(&mut renderer, 1_000_000, 2);
        assert!(renderer.is_ended());
        assert_eq!(renderer.position_ms(), Some(6_000));
    }

    #[test]
    fn a_replaced_stream_refuses_its_old_decoder() {
        let mut renderer = device(8_000, 1);
        let old = renderer.begin(8_000, 1, 0);
        let new = renderer.begin(8_000, 2, 1_000);
        assert!(!renderer.push(old, &[1, 2]));
        assert!(renderer.push(new, &[1, 2]));
        renderer.clear();
        assert!(!renderer.push(new, &[1, 2]));
        assert_eq!(renderer.position_ms(), None);
    }

    #[test]
    fn rate_and_speed_change_the_output_length() {
        let mut renderer = device(48_000, 1);
        renderer.set_playing(true);
        renderer.set_speed(2.0);
        let generation = renderer.begin(24_000, 2, 0);
        let stereo: Vec<i16> = (0..24_000 * 2)
            .flat_map(|n| {
                let s = ((n as f32 / 10.0).sin() * 8_000.0) as i16;
                [s, s]
            })
            .collect();
        renderer.push(generation, &stereo);
        renderer.finish_input(generation);
        let out = drain(&mut renderer, usize::MAX / 4, 1);
        let audible = out.iter().rposition(|s| s.abs() > 1e-4).unwrap_or(0);
        // Two seconds at double speed is one second at 48 kHz.
        assert!(audible.abs_diff(48_000) < 2_000, "{audible}");
        assert_eq!(renderer.position_ms(), Some(2_000));
    }

    #[test]
    fn a_stall_is_silence_and_playback_resumes_after_it() {
        let mut renderer = device(8_000, 1);
        renderer.set_playing(true);
        let generation = renderer.begin(8_000, 1, 0);
        renderer.push(generation, &[8_000; 100]);
        let mut buf = vec![0.0; 400];
        renderer.render(&mut buf);
        assert!(buf[..100].iter().all(|s| *s > 0.2));
        assert!(buf[100..].iter().all(|s| *s == 0.0));
        renderer.push(generation, &[8_000; 400]);
        renderer.render(&mut buf);
        assert!(buf.iter().all(|s| *s > 0.2));
        assert!(!renderer.is_ended());
    }

    #[test]
    fn channels_map_to_the_device() {
        let mut slot = [0.0; 4];
        map_channels(&[0.25], &mut slot);
        assert_eq!(slot, [0.25; 4]);
        let mut slot = [0.0; 4];
        map_channels(&[0.5, -0.5], &mut slot);
        assert_eq!(slot, [0.5, -0.5, 0.0, 0.0]);
        let mut slot = [0.0; 1];
        map_channels(&[0.5, 0.25], &mut slot);
        assert_eq!(slot, [0.375]);
    }
}
