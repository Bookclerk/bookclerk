//! Sample rate conversion from a book's rate to the output device's.
//!
//! A windowed-sinc interpolator ([`ZERO_CROSSINGS`] lobes each side, a
//! Blackman window), with the cutoff lowered to the output's Nyquist
//! frequency when converting down so nothing folds back. The kernel is
//! tabulated once per converter and read with linear interpolation. Equal
//! rates pass samples through.
//!
//! Sans IO: [`Resampler::push`] takes input frames, [`Resampler::pull`]
//! hands out converted ones.

/// Lobes of the sinc kernel on each side of its centre.
const ZERO_CROSSINGS: usize = 16;

/// Kernel table entries per input sample.
const TABLE_STEPS: usize = 256;

/// A converter for one stream of interleaved samples.
#[derive(Debug)]
pub struct Resampler {
    /// Interleaved channels.
    channels: usize,
    /// Input frames per output frame.
    step: f64,
    /// Whether the rates are equal.
    bypass: bool,
    /// The kernel, sampled every `1 / TABLE_STEPS` input frames from its
    /// centre outwards, already scaled by the cutoff.
    table: Vec<f32>,
    /// Kernel half-width in input frames.
    reach: f64,
    /// Cutoff as a fraction of the input's Nyquist frequency (at most 1).
    cutoff: f64,
    /// Input samples held, interleaved; frame 0 is input frame `base`.
    input: Vec<f32>,
    /// Absolute input frame of `input[0]`.
    base: u64,
    /// Absolute input position of the next output frame.
    position: f64,
}

impl Resampler {
    /// A converter from `from_rate` to `to_rate` for `channels` channels.
    #[must_use]
    pub fn new(from_rate: u32, to_rate: u32, channels: usize) -> Self {
        let from = f64::from(from_rate.max(1));
        let to = f64::from(to_rate.max(1));
        let cutoff = (to / from).min(1.0);
        let reach = ZERO_CROSSINGS as f64 / cutoff;
        let entries = ZERO_CROSSINGS * TABLE_STEPS + 1;
        let table = (0..entries)
            .map(|index| {
                // `x` in units of the kernel's own zero crossings.
                #[allow(clippy::cast_precision_loss)] // Small table indices.
                let x = index as f64 / TABLE_STEPS as f64;
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                };
                #[allow(clippy::cast_precision_loss)]
                let w = std::f64::consts::PI * x / ZERO_CROSSINGS as f64;
                let window = 0.42 + 0.5 * w.cos() + 0.08 * (2.0 * w).cos();
                #[allow(clippy::cast_possible_truncation)] // Kernel weights fit f32.
                let weight = (sinc * window * cutoff) as f32;
                weight
            })
            .collect();
        Self {
            channels: channels.max(1),
            step: from / to,
            bypass: from_rate == to_rate,
            table,
            reach,
            cutoff,
            input: Vec::new(),
            base: 0,
            position: 0.0,
        }
    }

    /// Adds input samples (a whole number of frames).
    pub fn push(&mut self, samples: &[f32]) {
        self.input.extend_from_slice(samples);
    }

    /// Converts what the held input allows into `out`, up to `max_frames`.
    ///
    /// # Arguments
    ///
    /// * `out` - Where converted samples go.
    /// * `max_frames` - Most output frames to make.
    /// * `flush` - The input ended: treat what lies past it as silence.
    ///
    /// # Returns
    ///
    /// Frames made.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn pull(&mut self, out: &mut Vec<f32>, max_frames: usize, flush: bool) -> usize {
        let channels = self.channels;
        let held = self.input.len() / channels;
        if self.bypass {
            let frames = held.min(max_frames);
            out.extend(self.input.drain(..frames * channels));
            self.base += frames as u64;
            return frames;
        }
        let end = self.base as f64 + held as f64;
        let mut made = 0;
        let mut acc = vec![0.0f32; channels];
        while made < max_frames {
            let centre = self.position;
            if !flush && centre + self.reach >= end {
                break;
            }
            if flush && centre >= end {
                break;
            }
            let first = (centre - self.reach).ceil().max(self.base as f64) as u64;
            let last = ((centre + self.reach).floor() as u64).min(self.base + held as u64 - 1);
            acc.iter_mut().for_each(|sample| *sample = 0.0);
            for frame in first..=last {
                let distance = (centre - frame as f64).abs() * self.cutoff;
                let at = distance * TABLE_STEPS as f64;
                let index = at as usize;
                let Some(&low) = self.table.get(index) else {
                    continue;
                };
                let high = self.table.get(index + 1).copied().unwrap_or(0.0);
                let weight = low + (high - low) * (at - index as f64) as f32;
                let from = (frame - self.base) as usize * channels;
                for (sample, input) in acc.iter_mut().zip(&self.input[from..from + channels]) {
                    *sample += input * weight;
                }
            }
            out.extend_from_slice(&acc);
            made += 1;
            self.position += self.step;
        }
        // Keep what the next kernel reaches back to.
        let keep = (self.position - self.reach).floor().max(0.0) as u64;
        let drop = keep.saturating_sub(self.base).min(held as u64);
        if drop > 4096 || (flush && drop > 0) {
            self.input.drain(..drop as usize * channels);
            self.base += drop;
        }
        made
    }

    /// Input frames held that output has not covered yet.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn pending_frames(&self) -> u64 {
        let held = (self.input.len() / self.channels) as u64;
        if self.bypass {
            return held;
        }
        (self.base + held).saturating_sub(self.position.max(0.0) as u64)
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

    fn tone(rate: u32, frames: usize, hz: f64) -> Vec<f32> {
        (0..frames)
            .map(|n| ((std::f64::consts::TAU * hz * n as f64 / f64::from(rate)).sin() * 0.5) as f32)
            .collect()
    }

    fn convert(from: u32, to: u32, input: &[f32]) -> Vec<f32> {
        let mut resampler = Resampler::new(from, to, 1);
        let mut out = Vec::new();
        for chunk in input.chunks(777) {
            resampler.push(chunk);
            while resampler.pull(&mut out, 300, false) > 0 {}
        }
        while resampler.pull(&mut out, 300, true) > 0 {}
        out
    }

    #[test]
    fn equal_rates_pass_through() {
        let input = tone(44_100, 5_000, 440.0);
        assert_eq!(convert(44_100, 44_100, &input), input);
    }

    #[test]
    fn a_tone_keeps_its_frequency_and_level_across_rates() {
        for (from, to) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (22_050, 48_000),
            (96_000, 44_100),
        ] {
            let input = tone(from, from as usize, 1_000.0);
            let out = convert(from, to, &input);
            let expected = f64::from(to);
            assert!(
                (out.len() as f64 - expected).abs() < 3.0,
                "{from}->{to}: {} frames",
                out.len()
            );
            let reference = tone(to, out.len(), 1_000.0);
            // Away from the edges the output is the tone at the new rate.
            let middle = out.len() / 4..out.len() * 3 / 4;
            let error = out[middle.clone()]
                .iter()
                .zip(&reference[middle])
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(error < 0.01, "{from}->{to}: error {error}");
        }
    }

    #[test]
    fn content_above_the_new_nyquist_is_removed() {
        // 20 kHz at 48 kHz cannot be represented at 22.05 kHz.
        let input = tone(48_000, 48_000, 20_000.0);
        let out = convert(48_000, 22_050, &input);
        let middle = &out[out.len() / 4..out.len() * 3 / 4];
        let peak = middle.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        assert!(peak < 0.01, "aliased peak {peak}");
    }

    #[test]
    fn stereo_channels_stay_apart() {
        let mut resampler = Resampler::new(32_000, 48_000, 2);
        let left = tone(32_000, 8_000, 500.0);
        let stereo: Vec<f32> = left.iter().flat_map(|s| [*s, 0.0]).collect();
        resampler.push(&stereo);
        let mut out = Vec::new();
        while resampler.pull(&mut out, 1_000, true) > 0 {}
        assert_eq!(out.len() % 2, 0);
        assert!(out.chunks(2).all(|frame| frame[1].abs() < 1e-6));
        assert!(out.chunks(2).any(|frame| frame[0].abs() > 0.4));
        assert_eq!(resampler.pending_frames(), 0);
    }
}
