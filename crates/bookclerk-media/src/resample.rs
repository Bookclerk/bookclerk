//! Streaming sample-rate conversion by windowed-sinc interpolation.
//!
//! Opus encodes at 8, 12, 16, 24 or 48 kHz only, so audio at any other
//! rate (44.1 kHz above all) is converted before encoding. Linear
//! interpolation aliases audibly at those ratios; this filter keeps the
//! passband flat to about 95% of the lower Nyquist frequency and rejects
//! images well below 16-bit noise.
//!
//! The filter is polyphase: the ratio is reduced to `up / down`, and the
//! weights for each of the `up` fractional positions are computed once. A
//! ratio whose `up` exceeds [`MAX_PHASES`] (an odd source rate) rounds the
//! fractional position to the nearest of [`MAX_PHASES`] phases instead,
//! an error of under a 2,000th of a sample.

/// Zero crossings of the sinc on each side of the centre, at the lower of
/// the two rates. More widen the filter and sharpen its cut-off.
const ZERO_CROSSINGS: usize = 24;

/// Most precomputed fractional positions.
const MAX_PHASES: u64 = 2_048;

/// Where the passband ends, as a fraction of the lower Nyquist frequency.
const PASSBAND: f64 = 0.95;

/// Converts interleaved `f32` PCM from one rate to another.
pub(crate) struct Resampler {
    /// Interleaved channels.
    channels: usize,
    /// Output frames per `down` input frames (the reduced ratio).
    up: u64,
    /// Input frames per `up` output frames.
    down: u64,
    /// Fractional positions in [`Self::table`].
    phases: u64,
    /// Taps per output sample, `half` on each side of the centre.
    taps: usize,
    /// Weights: `phases` rows of `taps`, each row summing to one.
    table: Vec<f32>,
    /// Input frames held, interleaved. Frames are counted from the start
    /// of a zero history of `half - 1` frames before the stream, so the
    /// first tap of output frame `n` reads frame `floor(n * down / up)`.
    held: Vec<f32>,
    /// The frame index (in that count) of `held[0]`.
    base: u64,
    /// Output frames emitted so far.
    emitted: u64,
    /// Input frames pushed so far.
    pushed: u64,
}

/// The greatest common divisor of `a` and `b`.
fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// `sin(pi x) / (pi x)`.
fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// The Blackman-Harris window at `x` in `[-1, 1]`, zero outside.
fn window(x: f64) -> f64 {
    if x.abs() >= 1.0 {
        return 0.0;
    }
    let t = std::f64::consts::PI * (x + 1.0);
    0.358_75 - 0.488_29 * t.cos() + 0.141_28 * (2.0 * t).cos() - 0.011_68 * (3.0 * t).cos()
}

/// Taps on each side of the centre for a cut-off of `cutoff` (a fraction
/// of the input's Nyquist frequency): [`ZERO_CROSSINGS`] at the cut-off.
fn half_taps(from: u64, to: u64) -> usize {
    // Rounded up: ZERO_CROSSINGS * from / (to * PASSBAND), at least
    // ZERO_CROSSINGS when converting up. PASSBAND is 95/100.
    let crossings = u64::try_from(ZERO_CROSSINGS).unwrap_or(u64::MAX);
    let half = if to >= from {
        (crossings * 100).div_ceil(95)
    } else {
        (crossings * from * 100).div_ceil(to * 95)
    };
    usize::try_from(half).unwrap_or(usize::MAX)
}

impl Resampler {
    /// A resampler from `from_hz` to `to_hz` for `channels` interleaved
    /// channels. A zero rate or channel count is taken as one.
    // The weights are computed in f64 and stored as f32, which is the
    // precision the filter runs at.
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn new(from_hz: u32, to_hz: u32, channels: usize) -> Self {
        let (from, to) = (u64::from(from_hz.max(1)), u64::from(to_hz.max(1)));
        let common = gcd(from, to);
        let (up, down) = (to / common, from / common);
        // Below 1 when converting down: the cut-off follows the output.
        let cutoff = (to as f64 / from as f64).min(1.0) * PASSBAND;
        let half = half_taps(from, to);
        let taps = 2 * half;
        let phases = up.min(MAX_PHASES);
        let mut table = Vec::new();
        for phase in 0..phases {
            let frac = phase as f64 / phases as f64;
            let row: Vec<f64> = (0..taps)
                .map(|k| {
                    // Tap k reads input frame floor(x) - half + 1 + k.
                    let distance = k as f64 - half as f64 + 1.0 - frac;
                    cutoff * sinc(cutoff * distance) * window(distance / half as f64)
                })
                .collect();
            let sum: f64 = row.iter().sum();
            table.extend(row.into_iter().map(|w| (w / sum) as f32));
        }
        let channels = channels.max(1);
        Self {
            channels,
            up,
            down,
            phases,
            taps,
            table,
            held: vec![0.0; (half - 1) * channels],
            base: 0,
            emitted: 0,
            pushed: 0,
        }
    }

    /// Frames held, as a `u64`.
    fn held_frames(&self) -> u64 {
        u64::try_from(self.held.len() / self.channels).unwrap_or(u64::MAX)
    }

    /// Appends interleaved input and emits every output frame it allows.
    pub(crate) fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let frames = u64::try_from(input.len() / self.channels).unwrap_or(u64::MAX);
        self.pushed = self.pushed.saturating_add(frames);
        self.held.extend_from_slice(input);
        self.emit(out, u64::MAX);
    }

    /// Emits the output frames still owed for the input pushed so far,
    /// reading zeros past its end.
    pub(crate) fn flush(&mut self, out: &mut Vec<f32>) {
        let owed = self.pushed.saturating_mul(self.up).div_ceil(self.down);
        self.held
            .extend(std::iter::repeat_n(0.0, self.taps * self.channels));
        self.emit(out, owed);
        self.held.clear();
    }

    /// Emits output frames while their taps are all held, up to `limit`
    /// frames in total, then drops the input no later frame reads.
    fn emit(&mut self, out: &mut Vec<f32>, limit: u64) {
        let ch = self.channels;
        let taps = u64::try_from(self.taps).unwrap_or(u64::MAX);
        while self.emitted < limit {
            let numerator = self.emitted.saturating_mul(self.down);
            let first = numerator / self.up;
            let Some(start) = first.checked_sub(self.base) else {
                break;
            };
            if start.saturating_add(taps) > self.held_frames() {
                break;
            }
            let phase = (numerator % self.up) * self.phases / self.up;
            let (Ok(phase), Ok(start)) = (usize::try_from(phase), usize::try_from(start)) else {
                break;
            };
            let row = &self.table[phase * self.taps..][..self.taps];
            let window = &self.held[start * ch..][..self.taps * ch];
            for c in 0..ch {
                let mut acc = 0.0f32;
                for (k, weight) in row.iter().enumerate() {
                    acc += weight * window[k * ch + c];
                }
                out.push(acc);
            }
            self.emitted += 1;
        }
        // Keep from the first frame the next output reads.
        let next_first = self.emitted.saturating_mul(self.down) / self.up;
        let drop = next_first.saturating_sub(self.base).min(self.held_frames());
        if let Ok(frames) = usize::try_from(drop) {
            if frames > 0 {
                self.held.drain(..frames * ch);
                self.base += drop;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A sine of `hz` at `rate`, `frames` long, mono.
    fn sine(hz: f64, rate: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|n| {
                let t = n as f64 / f64::from(rate);
                ((2.0 * std::f64::consts::PI * hz * t).sin() * 0.5) as f32
            })
            .collect()
    }

    /// Resamples `input` in uneven pushes, then flushes.
    fn run(input: &[f32], from: u32, to: u32, channels: usize) -> Vec<f32> {
        let mut rs = Resampler::new(from, to, channels);
        let mut out = Vec::new();
        for chunk in input.chunks(1_237 * channels) {
            rs.push(chunk, &mut out);
        }
        rs.flush(&mut out);
        out
    }

    #[test]
    fn the_output_length_follows_the_ratio() {
        for (from, to) in [
            (44_100, 48_000),
            (22_050, 48_000),
            (96_000, 48_000),
            (44_099, 48_000),
        ] {
            let input = sine(440.0, from, 10_000);
            let out = run(&input, from, to, 1);
            let expected = (10_000u64 * u64::from(to)).div_ceil(u64::from(from));
            assert_eq!(u64::try_from(out.len()).unwrap(), expected, "{from}->{to}");
        }
    }

    #[test]
    fn a_tone_keeps_its_level_and_shape() {
        let input = sine(1_000.0, 44_100, 44_100);
        let out = run(&input, 44_100, 48_000, 1);
        let reference = sine(1_000.0, 48_000, out.len());
        // Away from the edges the output matches a tone made at 48 kHz.
        let worst = out[1_000..out.len() - 1_000]
            .iter()
            .zip(&reference[1_000..])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-3, "worst error {worst}");
    }

    #[test]
    fn content_above_the_new_nyquist_is_removed() {
        // 30 kHz at 96 kHz cannot exist at 48 kHz; it must not fold to 18 kHz.
        let input = sine(30_000.0, 96_000, 96_000);
        let out = run(&input, 96_000, 48_000, 1);
        let peak = out[500..out.len() - 500]
            .iter()
            .fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(peak < 1e-3, "aliased peak {peak}");
    }

    #[test]
    fn channels_stay_apart() {
        let left = sine(500.0, 22_050, 5_000);
        let interleaved: Vec<f32> = left.iter().flat_map(|s| [*s, 0.0]).collect();
        let out = run(&interleaved, 22_050, 48_000, 2);
        assert!(out.chunks(2).all(|frame| frame[1] == 0.0));
        assert!(out.chunks(2).any(|frame| frame[0].abs() > 0.4));
    }
}
