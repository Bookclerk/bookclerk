//! Lightweight chapter boundary alignment via local waveform analysis.
//!
//! Around each chapter start (except 0), decode only a small window
//! (`±window_ms`), estimate **speech-band** energy (to ignore thematic music
//! beds), snap to the spoken-title onset, then walk backward through quiet
//! frames up to `max_lead_in_ms` without crossing a prior vocal waveform.

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

use crate::error::{MediaError, Result};

/// Tunables for silence / speech-onset chapter snapping.
#[derive(Debug, Clone, Copy)]
pub struct ChapterAlignOptions {
    /// Search window on each side of the expected chapter start.
    pub window_ms: u64,
    /// Minimum contiguous quiet frames before a speech onset counts.
    pub min_silence_ms: u64,
    /// RMS frame size used for energy analysis.
    pub frame_ms: u64,
    /// Absolute speech-band RMS floor (s16) treated as quiet.
    pub silence_rms_floor: f32,
    /// Fraction of window median speech-band RMS used as the quiet threshold.
    pub silence_rms_ratio: f32,
    /// Maximum ms to place the marker before the spoken-title onset. The
    /// marker stops earlier when walking back would hit prior vocal energy.
    pub max_lead_in_ms: u64,
    /// Bandpass low cutoff for vocal detection (Hz).
    pub vocal_low_hz: f32,
    /// Bandpass high cutoff for vocal detection (Hz).
    pub vocal_high_hz: f32,
}

impl Default for ChapterAlignOptions {
    fn default() -> Self {
        Self {
            window_ms: 5_000,
            min_silence_ms: 80,
            frame_ms: 20,
            silence_rms_floor: 200.0,
            silence_rms_ratio: 0.35,
            max_lead_in_ms: 2_000,
            // Telephone/speech formant band — rejects bass beds and bright FX
            // better than broadband RMS (GraphicAudio-style scores).
            vocal_low_hz: 300.0,
            vocal_high_hz: 3_400.0,
        }
    }
}

/// Snap chapter starts just before nearby speech onsets (spoken chapter titles).
///
/// Detects the speech-band onset of the spoken title, then walks backward
/// through quiet frames up to `max_lead_in_ms` so the marker precedes the title
/// without landing on a prior vocal waveform. Chapters at `0` are left
/// unchanged. Failures keep the original timestamp. Results stay monotonic.
///
/// Opens the media once and reuses a seek index across chapter windows so a
/// long book stays cheap (~tens of seconds of decode total, not a full pass).
#[must_use]
pub fn align_chapter_starts(
    path: &Path,
    chapters: &[(String, u64)],
    opts: ChapterAlignOptions,
) -> Vec<(String, u64)> {
    if chapters.len() < 2 || opts.window_ms == 0 {
        return chapters.to_vec();
    }

    let mut reader = match AlignReader::open(path) {
        Ok(reader) => reader,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "chapter align open failed; keeping original chapter starts"
            );
            return chapters.to_vec();
        }
    };

    let mut out = Vec::with_capacity(chapters.len());
    let mut shifted = 0u32;
    for (idx, (title, start_ms)) in chapters.iter().enumerate() {
        if *start_ms == 0 || idx == 0 {
            out.push((title.clone(), *start_ms));
            continue;
        }
        let aligned = match snap_chapter_start(&mut reader, *start_ms, opts) {
            Ok(Some(ms)) => {
                if ms != *start_ms {
                    shifted += 1;
                }
                ms
            }
            Ok(None) => *start_ms,
            Err(err) => {
                tracing::debug!(
                    chapter = %title,
                    start_ms,
                    error = %err,
                    "chapter align window failed; keeping original start"
                );
                *start_ms
            }
        };
        out.push((title.clone(), aligned));
    }

    enforce_monotonic(&mut out);

    if shifted > 0 {
        tracing::info!(
            chapters = chapters.len(),
            shifted,
            window_ms = opts.window_ms,
            max_lead_in_ms = opts.max_lead_in_ms,
            vocal_band_hz = format!("{}-{}", opts.vocal_low_hz, opts.vocal_high_hz),
            path = %path.display(),
            "aligned chapter starts via speech-band waveform analysis"
        );
    } else {
        tracing::debug!(
            chapters = chapters.len(),
            window_ms = opts.window_ms,
            path = %path.display(),
            "chapter align found no start shifts"
        );
    }
    out
}

struct AlignReader {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    sample_rate: u32,
    channels: usize,
    /// Interleaved PCM scratch reused across packets/windows (avoids per-AU alloc).
    interleaved_scratch: Vec<i16>,
}

impl AlignReader {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        // One seek index for all chapter windows.
        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default().prebuild_seek_index(true),
                MetadataOptions::default(),
            )
            .map_err(|err| MediaError::Native(format!("chapter align probe failed: {err}")))?;

        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| MediaError::Native("chapter align: no decodable audio track".into()))?
            .clone();
        let track_id = track.id;
        let audio_params = match track.codec_params.as_ref() {
            Some(CodecParameters::Audio(params)) => params.clone(),
            _ => {
                return Err(MediaError::Native(
                    "chapter align: selected track is missing audio codec parameters".into(),
                ));
            }
        };
        let sample_rate = audio_params
            .sample_rate
            .ok_or_else(|| MediaError::Native("chapter align: missing sample rate".into()))?;
        let channels = audio_params
            .channels
            .as_ref()
            .map(symphonia::core::audio::Channels::count)
            .unwrap_or(1)
            .max(1);

        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
            .map_err(|err| {
                MediaError::Native(format!("chapter align decoder init failed: {err}"))
            })?;

        Ok(Self {
            format,
            decoder,
            track_id,
            sample_rate,
            channels,
            interleaved_scratch: Vec::new(),
        })
    }

    /// Decode `[start_ms, end_ms)` to mono, bandpass to the vocal band, and
    /// return per-frame speech-band RMS energy.
    fn decode_window_vocal_energy(
        &mut self,
        start_ms: u64,
        end_ms: u64,
        opts: ChapterAlignOptions,
    ) -> Result<Vec<(u64, f32)>> {
        if end_ms <= start_ms {
            return Ok(Vec::new());
        }

        self.format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::from_millis_u64(start_ms),
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|err| MediaError::Native(format!("chapter align seek failed: {err}")))?;
        self.decoder.reset();

        let frame_ms = opts.frame_ms.max(1);
        let frame_samples = ((u64::from(self.sample_rate) * frame_ms) / 1000).max(1) as usize;
        let mut mono_frame = Vec::with_capacity(frame_samples);
        let mut energies = Vec::new();
        let mut samples_seen: u64 = 0;
        let window_samples =
            ((end_ms.saturating_sub(start_ms)) * u64::from(self.sample_rate) / 1000).max(1);

        let mut bandpass = Bandpass::new(
            self.sample_rate as f32,
            opts.vocal_low_hz,
            opts.vocal_high_hz,
        );

        loop {
            if samples_seen >= window_samples {
                break;
            }
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => break,
                Err(SymphoniaError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(err) => {
                    return Err(MediaError::Native(format!(
                        "chapter align demux error: {err}"
                    )));
                }
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(buf) => buf,
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(err) => {
                    return Err(MediaError::Native(format!(
                        "chapter align decode error: {err}"
                    )));
                }
            };
            append_mono_i16(
                &decoded,
                self.channels,
                &mut self.interleaved_scratch,
                &mut mono_frame,
            );
            while mono_frame.len() >= frame_samples {
                // Analyze in place, then drop the consumed prefix (no per-frame alloc).
                let vocal = vocal_band_rms(&mono_frame[..frame_samples], &mut bandpass);
                let t_ms = start_ms + samples_seen * 1000 / u64::from(self.sample_rate);
                energies.push((t_ms, vocal));
                mono_frame.drain(..frame_samples);
                samples_seen = samples_seen.saturating_add(frame_samples as u64);
                if samples_seen >= window_samples {
                    break;
                }
            }
        }

        Ok(energies)
    }
}

fn snap_chapter_start(
    reader: &mut AlignReader,
    expected_ms: u64,
    opts: ChapterAlignOptions,
) -> Result<Option<u64>> {
    let window_start_ms = expected_ms.saturating_sub(opts.window_ms);
    let window_end_ms = expected_ms.saturating_add(opts.window_ms);
    let energies = reader.decode_window_vocal_energy(window_start_ms, window_end_ms, opts)?;
    if energies.is_empty() {
        return Ok(None);
    }

    // Baseline = typical in-window speech-band level (silence *or* music bed).
    // Spoken titles are a clear rise above that baseline — this is what lets us
    // walk lead-in through GraphicAudio-style beds without treating them as
    // chapter speech.
    let baseline = percentile(&energies, 0.40);
    let speech_thr = (baseline * 2.5)
        .max(opts.silence_rms_floor)
        .max(baseline + opts.silence_rms_floor);
    let frame_ms = opts.frame_ms.max(1);
    let min_quiet_frames = opts.min_silence_ms.div_ceil(frame_ms).max(1) as usize;

    let Some(onset_idx) = find_vocal_onset(&energies, expected_ms, speech_thr, min_quiet_frames)
    else {
        return Ok(None);
    };
    let onset_ms = energies[onset_idx].0;
    let marker_ms = adaptive_lead_in(&energies, onset_idx, speech_thr, opts.max_lead_in_ms);
    Ok(Some(marker_ms.min(onset_ms)))
}

/// Prefer the speech-band onset closest to the expected chapter time that
/// follows a short quiet run.
fn find_vocal_onset(
    energies: &[(u64, f32)],
    expected_ms: u64,
    thr: f32,
    min_quiet_frames: usize,
) -> Option<usize> {
    let mut best: Option<(u64, usize, f32)> = None; // (abs_delta, idx, rise)
    let mut quiet_run = 0usize;
    for i in 0..energies.len() {
        let (t_ms, rms) = energies[i];
        if rms < thr {
            quiet_run = quiet_run.saturating_add(1);
            continue;
        }
        if quiet_run >= min_quiet_frames {
            let prev = if i > 0 { energies[i - 1].1 } else { 0.0 };
            let rise = (rms - prev).max(0.0);
            let delta = t_ms.abs_diff(expected_ms);
            let cand = (delta, i, rise);
            match best {
                None => best = Some(cand),
                Some(cur) => {
                    if cand.0 < cur.0 || (cand.0 == cur.0 && cand.2 > cur.2) {
                        best = Some(cand);
                    }
                }
            }
        }
        quiet_run = 0;
    }
    best.map(|(_, idx, _)| idx)
}

/// Walk backward from the onset through quiet frames, up to `max_lead_in_ms`,
/// stopping when prior vocal energy appears.
fn adaptive_lead_in(
    energies: &[(u64, f32)],
    onset_idx: usize,
    thr: f32,
    max_lead_in_ms: u64,
) -> u64 {
    let onset_ms = energies[onset_idx].0;
    let earliest_ms = onset_ms.saturating_sub(max_lead_in_ms);
    let mut marker_ms = onset_ms;
    // Step back one frame at a time while still quiet.
    let mut i = onset_idx;
    while i > 0 {
        i -= 1;
        let (t_ms, rms) = energies[i];
        if t_ms < earliest_ms {
            break;
        }
        if rms >= thr {
            // Hit a prior vocal/music-in-band waveform; keep marker after it.
            break;
        }
        marker_ms = t_ms;
    }
    marker_ms
}

/// Cascaded high-pass + low-pass one-pole filters approximating a vocal bandpass.
///
/// Cheap and stateful across frames so GraphicAudio beds (bass/pads outside the
/// band) contribute much less energy than spoken titles.
#[derive(Debug, Clone, Copy)]
struct Bandpass {
    hp_a: f32,
    hp_x: f32,
    hp_y: f32,
    lp_a: f32,
    lp_y: f32,
}

impl Bandpass {
    fn new(sample_rate: f32, low_hz: f32, high_hz: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let low = low_hz.clamp(20.0, sr * 0.45);
        let high = high_hz.clamp(low + 20.0, sr * 0.49);
        // One-pole coefficients: a = exp(-2π fc / fs)
        let hp_a = (-2.0 * std::f32::consts::PI * low / sr).exp();
        let lp_a = (-2.0 * std::f32::consts::PI * high / sr).exp();
        Self {
            hp_a,
            hp_x: 0.0,
            hp_y: 0.0,
            lp_a,
            lp_y: 0.0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        // High-pass
        let hp = self.hp_a * (self.hp_y + x - self.hp_x);
        self.hp_x = x;
        self.hp_y = hp;
        // Low-pass
        self.lp_y += (1.0 - self.lp_a) * (hp - self.lp_y);
        self.lp_y
    }
}

fn vocal_band_rms(samples: &[i16], filter: &mut Bandpass) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sum_sq = 0.0f64;
    for &s in samples {
        let y = filter.process(f32::from(s));
        sum_sq += f64::from(y) * f64::from(y);
    }
    (sum_sq / samples.len() as f64).sqrt() as f32
}

fn append_mono_i16(
    buf: &GenericAudioBufferRef<'_>,
    channels: usize,
    interleaved: &mut Vec<i16>,
    dst: &mut Vec<i16>,
) {
    let frames = buf.frames();
    if frames == 0 {
        return;
    }
    let in_channels = buf.spec().channels().count().max(1);

    // Mono source: copy straight into `dst` (no scratch / downmix).
    if in_channels == 1 {
        let start = dst.len();
        dst.resize(start + frames, 0);
        buf.copy_to_slice_interleaved(&mut dst[start..]);
        return;
    }

    let need = frames.saturating_mul(in_channels);
    interleaved.resize(need, 0);
    buf.copy_to_slice_interleaved(&mut interleaved[..need]);

    let mix_channels = in_channels.min(channels).max(1);
    dst.reserve(frames);
    for frame in interleaved[..need].chunks_exact(in_channels) {
        let mut acc = 0i32;
        for sample in frame.iter().take(mix_channels) {
            acc += i32::from(*sample);
        }
        dst.push((acc / mix_channels as i32) as i16);
    }
}

fn percentile(energies: &[(u64, f32)], q: f32) -> f32 {
    if energies.is_empty() {
        return 0.0;
    }
    let mut vals: Vec<f32> = energies.iter().map(|(_, e)| *e).collect();
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Less));
    let idx = ((vals.len() as f32 - 1.0) * q.clamp(0.0, 1.0)).round() as usize;
    vals[idx.min(vals.len() - 1)]
}

fn enforce_monotonic(chapters: &mut [(String, u64)]) {
    let mut prev = 0u64;
    for (idx, ch) in chapters.iter_mut().enumerate() {
        if idx == 0 {
            prev = ch.1;
            continue;
        }
        if ch.1 <= prev {
            ch.1 = prev.saturating_add(1);
        }
        prev = ch.1;
    }
}

/// Scale rebased chapter starts so they span the probed plain-audio duration.
///
/// When Audible content duration (`runtime - intro - outro`) differs from the
/// plain file, apply a uniform scale. No-ops when either duration is missing or
/// the relative delta is tiny.
#[must_use]
pub fn scale_chapters_to_duration(
    chapters: &[(String, u64)],
    content_duration_ms: Option<u64>,
    plain_duration_ms: Option<u64>,
) -> Vec<(String, u64)> {
    let (Some(content), Some(plain)) = (content_duration_ms, plain_duration_ms) else {
        return chapters.to_vec();
    };
    if content == 0 || plain == 0 || content == plain {
        return chapters.to_vec();
    }
    let delta = content.abs_diff(plain);
    // Ignore sub-250ms measurement noise.
    if delta < 250 {
        return chapters.to_vec();
    }
    let scale = plain as f64 / content as f64;
    // Guard against pathological mismatches (wrong title / bad runtime).
    if !(0.98..=1.02).contains(&scale) {
        tracing::debug!(
            content_duration_ms = content,
            plain_duration_ms = plain,
            scale,
            "skipping chapter duration scale; ratio outside ±2%"
        );
        return chapters.to_vec();
    }
    tracing::info!(
        content_duration_ms = content,
        plain_duration_ms = plain,
        scale,
        "scaling chapter starts to plain-audio duration"
    );
    chapters
        .iter()
        .map(|(title, start)| {
            let scaled = ((*start as f64) * scale).round().max(0.0) as u64;
            let clamped = scaled.min(plain.saturating_sub(1));
            (title.clone(), if *start == 0 { 0 } else { clamped })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package_m4b::package_m4b_from_pcm;

    fn tone_pcm(sample_rate: u32, ms: u64, freq: f32, amp: f32) -> Vec<i16> {
        let frames = (u64::from(sample_rate) * ms / 1000) as usize;
        let mut out = Vec::with_capacity(frames);
        for i in 0..frames {
            let t = i as f32 / sample_rate as f32;
            let s = (2.0 * std::f32::consts::PI * freq * t).sin() * amp;
            out.push((s * f32::from(i16::MAX)) as i16);
        }
        out
    }

    fn mix(a: &[i16], b: &[i16]) -> Vec<i16> {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| {
                let s = i32::from(*x) + i32::from(*y);
                s.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
            })
            .collect()
    }

    #[test]
    fn scales_when_plain_shorter() {
        // ~1% shorter plain duration (within the ±2% safety guard).
        let chapters = vec![
            ("A".into(), 0u64),
            ("B".into(), 1_000_000),
            ("C".into(), 2_000_000),
        ];
        let out = scale_chapters_to_duration(&chapters, Some(2_000_000), Some(1_980_000));
        assert_eq!(out[0].1, 0);
        assert_eq!(out[1].1, 990_000);
        assert_eq!(out[2].1, 1_980_000 - 1);
    }

    #[test]
    fn adaptive_lead_in_uses_full_quiet_gap_up_to_2s() {
        let sample_rate = 16_000u32;
        // 1.5s speech | 3s silence | 2s speech. Onset at 4500ms; cap lead-in at 2s.
        let mut pcm = tone_pcm(sample_rate, 1_500, 800.0, 0.25);
        pcm.extend(std::iter::repeat_n(0i16, sample_rate as usize * 3));
        pcm.extend(tone_pcm(sample_rate, 2_000, 1_000.0, 0.25));

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("long_gap.m4b");
        package_m4b_from_pcm(&pcm, sample_rate, 1, &out, &[]).unwrap();

        let chapters = vec![("One".into(), 0u64), ("Two".into(), 4_200)];
        let aligned = align_chapter_starts(&out, &chapters, ChapterAlignOptions::default());
        assert_eq!(aligned[0].1, 0);
        // Onset ~4500; marker should be ~2500 (2s lead), not the full 3s quiet.
        assert!(
            aligned[1].1.abs_diff(2_500) <= 200,
            "aligned={} expected~2500 (onset 4500 - max lead 2000)",
            aligned[1].1
        );
    }

    #[test]
    fn adaptive_lead_in_stops_before_prior_waveform() {
        let sample_rate = 16_000u32;
        // 2s speech | 0.8s silence | 2s speech. Lead-in must not cross into ch1.
        let mut pcm = tone_pcm(sample_rate, 2_000, 800.0, 0.25);
        pcm.extend(std::iter::repeat_n(
            0i16,
            (sample_rate as f32 * 0.8) as usize,
        ));
        pcm.extend(tone_pcm(sample_rate, 2_000, 1_000.0, 0.25));

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("short_gap.m4b");
        package_m4b_from_pcm(&pcm, sample_rate, 1, &out, &[]).unwrap();

        let chapters = vec![("One".into(), 0u64), ("Two".into(), 2_500)];
        let aligned = align_chapter_starts(&out, &chapters, ChapterAlignOptions::default());
        // Quiet starts at 2000ms; marker should land near there (not 800ms before onset).
        // AAC framing can eat some of the quiet gap; marker must stay inside the
        // gap (after prior speech ~2000) and before/at the second onset ~2800.
        assert!(
            aligned[1].1 >= 1_900 && aligned[1].1 <= 2_700,
            "aligned={} expected inside 0.8s quiet gap after prior speech",
            aligned[1].1
        );
    }

    #[test]
    fn speech_band_ignores_low_frequency_music_bed() {
        let sample_rate = 16_000u32;
        // Continuous low "music" bed under a spoken title that starts at 3s.
        // Broadband energy is never silent; speech-band scoring should still
        // find the 1kHz onset and walk lead-in through the bed (up to 2s).
        let music = tone_pcm(sample_rate, 5_000, 60.0, 0.4);
        let mut speech = vec![0i16; sample_rate as usize * 3];
        speech.extend(tone_pcm(sample_rate, 2_000, 1_200.0, 0.35));
        let mut music = music;
        music.resize(speech.len(), 0);
        let pcm = mix(&music, &speech);

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("bed.m4b");
        package_m4b_from_pcm(&pcm, sample_rate, 1, &out, &[]).unwrap();

        let chapters = vec![("One".into(), 0u64), ("Two".into(), 2_800)];
        let aligned = align_chapter_starts(&out, &chapters, ChapterAlignOptions::default());
        // Onset ~3000ms; with music suppressed in-band, lead-in can extend toward
        // the 2s cap (~1000ms) without requiring broadband silence.
        assert!(
            aligned[1].1 >= 900 && aligned[1].1 <= 3_100,
            "aligned={} — expected speech-band onset~3000 with up to 2s lead-in",
            aligned[1].1
        );
    }

    #[test]
    fn adaptive_lead_in_helper_caps_and_stops() {
        // Synthetic energies: vocal, quiet×10 frames (200ms), vocal. frame_ms=20.
        let mut energies = Vec::new();
        for i in 0..10 {
            energies.push((i * 20, 2_000.0)); // prior vocal
        }
        for i in 10..60 {
            energies.push((i * 20, 10.0)); // 1s quiet
        }
        let onset_idx = 60;
        energies.push((onset_idx as u64 * 20, 2_500.0));
        let thr = 200.0;
        let marker = adaptive_lead_in(&energies, onset_idx, thr, 2_000);
        // Quiet starts at 200ms; onset at 1200ms → marker at 200 (full quiet < 2s).
        assert_eq!(marker, 200);

        // Longer quiet: 3s quiet before onset → cap at 2s lead.
        let mut energies = Vec::new();
        for i in 0..150 {
            energies.push((i * 20, 10.0));
        }
        let onset_idx = 150; // 3000ms
        energies.push((3_000, 2_500.0));
        let marker = adaptive_lead_in(&energies, onset_idx, thr, 2_000);
        assert_eq!(marker, 1_000); // 3000 - 2000
    }
}
