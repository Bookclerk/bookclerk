//! Speed change without a pitch change, for speech: WSOLA (waveform
//! similarity overlap-add).
//!
//! The output is built from windowed segments of the input, overlapped by
//! half. Each segment is taken from near where the speed says the next one
//! should start, shifted by up to [`SEARCH_MS`] to where it best lines up
//! with the natural continuation of the segment before it, so voiced
//! sounds stay in phase and the voice keeps its pitch. At speed 1 the
//! input passes through untouched.
//!
//! Sans IO: [`Stretcher::push`] takes decoded samples, [`Stretcher::pull`]
//! hands out stretched ones, and [`Stretcher::source_frames`] says how much
//! of the input the output so far stands for, which is the player's
//! position.

/// Length of one segment, in milliseconds: long enough for a few pitch
/// periods of a low voice.
const SEGMENT_MS: u32 = 40;

/// How far a segment may move to line up with the previous one, in
/// milliseconds each way: a little over one period of the lowest voice.
const SEARCH_MS: u32 = 12;

/// Slowest speed accepted.
pub const MIN_SPEED: f32 = 0.5;

/// Fastest speed accepted.
pub const MAX_SPEED: f32 = 3.0;

/// The geometry of a stretcher at one sample rate.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    /// Interleaved channels.
    channels: usize,
    /// Frames in one segment; even.
    segment: usize,
    /// Frames the output moves per segment: half a segment.
    hop: usize,
    /// Frames a segment may move either way.
    search: usize,
}

/// A speed changer for one stream of interleaved samples.
#[derive(Debug)]
pub struct Stretcher {
    /// Segment sizes.
    geometry: Geometry,
    /// Output speed: input frames per output frame.
    speed: f32,
    /// Input samples not yet dropped, interleaved; frame 0 is input frame
    /// [`Self::base`].
    input: Vec<f32>,
    /// Absolute input frame of `input[0]`.
    base: u64,
    /// Whether the input has ended.
    ended: bool,
    /// The absolute frame the input really ended at, once silence was
    /// added after it.
    real_end: Option<u64>,
    /// Absolute input frame the next segment should start near.
    analysis: f64,
    /// Absolute input frame where the last segment started, once one has.
    previous: Option<u64>,
    /// The second half of the last windowed segment, waiting to be added
    /// to the first half of the next; interleaved, `hop` frames.
    tail: Vec<f32>,
    /// Stretched samples made and not yet pulled.
    ready: Vec<f32>,
    /// Absolute input frames the output pulled so far stands for.
    source_frames: f64,
    /// The analysis window (periodic Hann), one weight per frame.
    window: Vec<f32>,
}

impl Stretcher {
    /// A stretcher for `channels` interleaved channels at `sample_rate`,
    /// at speed 1.
    ///
    /// # Arguments
    ///
    /// * `sample_rate` - Frames per second, at least 1.
    /// * `channels` - Interleaved channels, at least 1.
    #[must_use]
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        let per_ms = |ms: u32| {
            usize::try_from(u64::from(sample_rate) * u64::from(ms) / 1000).unwrap_or(usize::MAX)
        };
        let segment = (per_ms(SEGMENT_MS).max(8) / 2) * 2;
        let hop = segment / 2;
        let window = (0..segment)
            .map(|n| {
                #[allow(clippy::cast_precision_loss)] // A window index, far below 2^24.
                let phase = std::f32::consts::TAU * n as f32 / segment as f32;
                0.5 - 0.5 * phase.cos()
            })
            .collect();
        Self {
            geometry: Geometry {
                channels: channels.max(1),
                segment,
                hop,
                search: per_ms(SEARCH_MS).max(1),
            },
            speed: 1.0,
            input: Vec::new(),
            base: 0,
            ended: false,
            real_end: None,
            analysis: 0.0,
            previous: None,
            tail: Vec::new(),
            ready: Vec::new(),
            source_frames: 0.0,
            window,
        }
    }

    /// The current speed.
    #[must_use]
    pub fn speed(&self) -> f32 {
        self.speed
    }

    /// Changes the speed, clamped to [`MIN_SPEED`]..=[`MAX_SPEED`]. The
    /// change applies from the next segment; samples already made keep the
    /// old speed.
    pub fn set_speed(&mut self, speed: f32) {
        let speed = if speed.is_finite() {
            speed.clamp(MIN_SPEED, MAX_SPEED)
        } else {
            1.0
        };
        if (speed - self.speed).abs() < f32::EPSILON {
            return;
        }
        // Start the overlap-add afresh where the input has got to.
        self.analysis = self.consumed_frame();
        self.previous = None;
        self.speed = speed;
    }

    /// Adds decoded samples (a whole number of frames).
    pub fn push(&mut self, samples: &[f32]) {
        self.input.extend_from_slice(samples);
    }

    /// Marks the end of the input: what is left is played out.
    pub fn finish(&mut self) {
        self.ended = true;
    }

    /// Input frames held and not yet played.
    #[must_use]
    pub fn buffered_frames(&self) -> u64 {
        let held = u64::try_from(self.input.len() / self.geometry.channels).unwrap_or(0);
        let held = self
            .real_end
            .map_or(held, |end| held.min(end.saturating_sub(self.base)));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // Non-negative, frame counts.
        let played = (self.consumed_frame() - self.base as f64).max(0.0) as u64;
        held.saturating_sub(played)
    }

    /// Absolute input frames the output pulled so far stands for.
    #[must_use]
    pub fn source_frames(&self) -> f64 {
        self.source_frames
    }

    /// Whether the input ended and everything has been pulled.
    #[must_use]
    pub fn is_drained(&self) -> bool {
        self.ended && self.ready.is_empty() && self.buffered_frames() == 0 && self.tail.is_empty()
    }

    /// Where the input has been read to, in absolute frames.
    #[allow(clippy::cast_precision_loss)] // Frame counts stay far below 2^52.
    fn consumed_frame(&self) -> f64 {
        if self.speed_is_one() {
            self.analysis
        } else {
            self.analysis.max(self.base as f64)
        }
    }

    /// Whether the stretcher passes samples through.
    fn speed_is_one(&self) -> bool {
        (self.speed - 1.0).abs() < f32::EPSILON
    }

    /// Moves up to `max_frames` stretched frames into `out`.
    ///
    /// # Returns
    ///
    /// Frames moved; fewer than asked when more input is needed (or it
    /// ended).
    pub fn pull(&mut self, out: &mut Vec<f32>, max_frames: usize) -> usize {
        let channels = self.geometry.channels;
        let mut moved = 0;
        while moved < max_frames {
            if self.ready.is_empty() && !self.make() {
                break;
            }
            let frames = (self.ready.len() / channels).min(max_frames - moved);
            out.extend(self.ready.drain(..frames * channels));
            moved += frames;
        }
        moved
    }

    /// Makes the next block of output into `ready`.
    ///
    /// # Returns
    ///
    /// False when it needs more input, or everything was played out.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn make(&mut self) -> bool {
        let Geometry {
            channels,
            segment,
            hop,
            search,
        } = self.geometry;
        let held = self.input.len() / channels;
        if self.speed_is_one() {
            // Pass through, after playing out a stretched tail.
            if !self.tail.is_empty() {
                self.ready.append(&mut self.tail);
                return true;
            }
            let from = (self.analysis as u64).saturating_sub(self.base) as usize;
            let available = held.saturating_sub(from).min(4096);
            if available == 0 {
                return false;
            }
            self.ready
                .extend_from_slice(&self.input[from * channels..(from + available) * channels]);
            self.analysis += available as f64;
            self.source_frames = self.analysis;
            self.trim(self.analysis as u64);
            return true;
        }
        let nominal = self.analysis.round() as u64;
        if let Some(real_end) = self.real_end.filter(|end| nominal >= *end) {
            self.analysis = real_end as f64;
            self.source_frames = self.analysis;
            if self.tail.is_empty() {
                return false;
            }
            self.ready.append(&mut self.tail);
            return true;
        }
        let need_to = nominal + (search + segment) as u64;
        let natural = self.previous.map(|previous| previous + hop as u64);
        let need_to = need_to.max(natural.map_or(0, |at| at + segment as u64));
        let have_to = self.base + held as u64;
        if have_to < need_to {
            if !self.ended {
                return false;
            }
            // The input ended. Past its last frame the input reads as
            // silence, so segments keep their spacing up to the end.
            if self.real_end.is_none() {
                self.real_end = Some(have_to);
                if nominal >= have_to {
                    return self.make();
                }
            }
            let missing = usize::try_from(need_to - have_to).unwrap_or(0);
            self.input
                .resize(self.input.len() + missing * channels, 0.0);
        }
        let start = match natural {
            None => nominal,
            Some(natural) => self.best_start(nominal, natural),
        };
        let at = (start - self.base) as usize;
        let mut block = Vec::with_capacity(hop * channels);
        let mut tail = Vec::with_capacity(hop * channels);
        for frame in 0..segment {
            let weight = self.window[frame];
            for channel in 0..channels {
                let sample = self.input[(at + frame) * channels + channel] * weight;
                if frame < hop {
                    let carried = self.tail.get(frame * channels + channel).copied();
                    block.push(sample + carried.unwrap_or(0.0));
                } else {
                    tail.push(sample);
                }
            }
        }
        self.tail = tail;
        self.ready.extend(block);
        self.previous = Some(start);
        self.source_frames = self.analysis;
        self.analysis += hop as f64 * f64::from(self.speed);
        let keep_from = (self.analysis.round() as u64)
            .saturating_sub(search as u64)
            .min(start + hop as u64);
        self.trim(keep_from);
        true
    }

    /// The segment start within `search` of `nominal` whose first half best
    /// matches the `natural` continuation of the last segment, by
    /// normalised cross-correlation of the channels' sum.
    #[allow(clippy::cast_possible_truncation)]
    fn best_start(&self, nominal: u64, natural: u64) -> u64 {
        let Geometry {
            channels,
            hop,
            search,
            ..
        } = self.geometry;
        let low = nominal.saturating_sub(search as u64).max(self.base);
        let high = nominal + search as u64;
        let mono = |frame: u64| -> f32 {
            let at = (frame - self.base) as usize * channels;
            self.input[at..at + channels].iter().sum()
        };
        // Every second frame is enough to find the best alignment.
        let reference: Vec<f32> = (0..hop as u64)
            .step_by(2)
            .map(|k| mono(natural + k))
            .collect();
        let mut best = (f32::MIN, nominal);
        for candidate in low..=high {
            let (mut dot, mut energy) = (0.0f32, 0.0f32);
            for (k, wanted) in (0..hop as u64).step_by(2).zip(&reference) {
                let sample = mono(candidate + k);
                dot += sample * wanted;
                energy += sample * sample;
            }
            let score = dot / (energy.sqrt() + 1e-9);
            if score > best.0 {
                best = (score, candidate);
            }
        }
        best.1
    }

    /// Drops input before absolute frame `frame`.
    #[allow(clippy::cast_possible_truncation)]
    fn trim(&mut self, frame: u64) {
        let drop = frame
            .saturating_sub(self.base)
            .min((self.input.len() / self.geometry.channels) as u64);
        if drop > 4096 {
            self.input.drain(..drop as usize * self.geometry.channels);
            self.base += drop;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, frames: usize, hz: f32) -> Vec<f32> {
        (0..frames)
            .map(|n| (std::f32::consts::TAU * hz * n as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    fn run(stretcher: &mut Stretcher, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        for chunk in input.chunks(1000) {
            stretcher.push(chunk);
            while stretcher.pull(&mut out, 512) > 0 {}
        }
        stretcher.finish();
        while stretcher.pull(&mut out, 512) > 0 {}
        out
    }

    /// Zero crossings per second, a rough pitch.
    fn crossings_per_second(samples: &[f32], rate: u32) -> f32 {
        let crossings = samples
            .windows(2)
            .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
            .count();
        crossings as f32 / (samples.len() as f32 / rate as f32)
    }

    #[test]
    fn speed_one_passes_samples_through() {
        let input = tone(8_000, 10_000, 220.0);
        let mut stretcher = Stretcher::new(8_000, 1);
        assert_eq!(run(&mut stretcher, &input), input);
        assert!((stretcher.source_frames() - 10_000.0).abs() < 1.0);
        assert!(stretcher.is_drained());
    }

    #[test]
    fn other_speeds_change_length_but_not_pitch() {
        let rate = 16_000;
        let input = tone(rate, 4 * rate as usize, 200.0);
        for speed in [0.5f32, 0.8, 1.5, 2.0, 3.0] {
            let mut stretcher = Stretcher::new(rate, 1);
            stretcher.set_speed(speed);
            let out = run(&mut stretcher, &input);
            let expected = input.len() as f32 / speed;
            let ratio = out.len() as f32 / expected;
            assert!((0.97..1.03).contains(&ratio), "speed {speed}: {ratio}");
            // Skip the start and end, where segments meet the edges.
            let middle = &out[out.len() / 4..out.len() * 3 / 4];
            let pitch = crossings_per_second(middle, rate) / 2.0;
            assert!((190.0..210.0).contains(&pitch), "speed {speed}: {pitch} Hz");
            assert!((stretcher.source_frames() - input.len() as f64).abs() < 2.0);
        }
    }

    #[test]
    fn a_steady_tone_keeps_its_level() {
        let rate = 16_000;
        let input = tone(rate, 2 * rate as usize, 300.0);
        let mut stretcher = Stretcher::new(rate, 1);
        stretcher.set_speed(1.5);
        let out = run(&mut stretcher, &input);
        let middle = &out[out.len() / 4..out.len() * 3 / 4];
        let peak = middle.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        assert!((0.4..0.6).contains(&peak), "peak {peak}");
    }

    #[test]
    fn stereo_frames_stay_whole_and_the_speed_is_clamped() {
        let rate = 8_000;
        let mono = tone(rate, rate as usize, 150.0);
        let stereo: Vec<f32> = mono.iter().flat_map(|s| [*s, -*s]).collect();
        let mut stretcher = Stretcher::new(rate, 2);
        stretcher.set_speed(9.0);
        assert!((stretcher.speed() - MAX_SPEED).abs() < f32::EPSILON);
        stretcher.set_speed(f32::NAN);
        assert!((stretcher.speed() - 1.0).abs() < f32::EPSILON);
        stretcher.set_speed(2.0);
        let out = run(&mut stretcher, &stereo);
        assert_eq!(out.len() % 2, 0);
        for frame in out.chunks(2) {
            assert!((frame[0] + frame[1]).abs() < 1e-5);
        }
    }

    #[test]
    fn a_speed_change_mid_stream_keeps_the_position() {
        let rate = 8_000;
        let input = tone(rate, 3 * rate as usize, 180.0);
        let mut stretcher = Stretcher::new(rate, 1);
        let mut out = Vec::new();
        stretcher.push(&input[..rate as usize]);
        while stretcher.pull(&mut out, 512) > 0 {}
        let before = stretcher.source_frames();
        stretcher.set_speed(2.0);
        stretcher.push(&input[rate as usize..]);
        stretcher.finish();
        while stretcher.pull(&mut out, 512) > 0 {}
        assert!(before >= 7_000.0, "{before}");
        assert!((stretcher.source_frames() - input.len() as f64).abs() < 2.0);
        let expected = before as usize + (input.len() - before as usize) / 2;
        assert!(
            out.len().abs_diff(expected) < 800,
            "{} vs {expected}",
            out.len()
        );
    }
}
