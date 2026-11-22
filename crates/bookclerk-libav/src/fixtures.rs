//! Small audio files in every format this crate decodes, for tests here
//! and in the crates that use it.
//!
//! The WMA and AC-3 files are half-second 440 Hz tones made with FFmpeg's
//! own encoders (bit-exact flags, no metadata). AMR has no encoder in the
//! build, so its files are assembled here: frames of the lowest bit rate
//! filled with pseudo-random bits, which any AMR decoder turns into half a
//! second of noise; the 3GP file carries those same frames.

/// WMA 2 (stereo, 44.1 kHz) in ASF.
pub const WMAV2: &[u8] = include_bytes!("../fixtures/tone-wmav2.wma");
/// WMA 1 (mono, 44.1 kHz) in ASF.
pub const WMAV1: &[u8] = include_bytes!("../fixtures/tone-wmav1.wma");
/// A raw AC-3 stream (stereo, 48 kHz).
pub const AC3: &[u8] = include_bytes!("../fixtures/tone.ac3");
/// A raw E-AC-3 stream (stereo, 48 kHz).
pub const EAC3: &[u8] = include_bytes!("../fixtures/tone.eac3");
/// E-AC-3 (stereo, 48 kHz) in an MP4 (`ec-3` sample entry).
pub const EAC3_MP4: &[u8] = include_bytes!("../fixtures/tone-eac3.m4a");

/// AMR-WB in a 3GP file: the frames of [`amr_wb`] remuxed (FFmpeg, stream
/// copy), so Symphonia's MP4 reader finds a track it has no decoder for.
pub const AMR_WB_3GP: &[u8] = include_bytes!("../fixtures/amrwb.3gp");

/// A raw AMR-NB file (`#!AMR`): 25 frames of 4.75 kbit/s, 0.5 s at 8 kHz.
#[must_use]
pub fn amr_nb() -> Vec<u8> {
    amr(b"#!AMR\n", 12)
}

/// A raw AMR-WB file (`#!AMR-WB`): 25 frames of 6.60 kbit/s, 0.5 s at
/// 16 kHz.
#[must_use]
pub fn amr_wb() -> Vec<u8> {
    amr(b"#!AMR-WB\n", 17)
}

/// `magic` then 25 mode-0 frames of `payload` bytes each.
fn amr(magic: &[u8], payload: usize) -> Vec<u8> {
    let mut out = magic.to_vec();
    let mut state = 0x2545_F491_u32;
    for _ in 0..25 {
        // Frame type 0, quality bit set, then the payload.
        let frame = std::iter::once(0x04).chain((0..payload).map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state.to_be_bytes()[0]
        }));
        out.extend(frame);
    }
    out
}
