//! Declarations of the C shim (`src/shim.c`), mirrored by hand.
//!
//! These are the shim's own plain structs and functions, never FFmpeg's:
//! the shim is compiled against the pinned FFmpeg headers, so FFmpeg's
//! struct layouts never cross into Rust. Keep every declaration here in
//! step with `shim.c`.

#![allow(unsafe_code)]

use std::ffi::c_void;

/// The call succeeded.
pub(crate) const BC_OK: i32 = 0;
/// The decoder wants another packet before it can give a frame.
pub(crate) const BC_AGAIN: i32 = 1;
/// The stream or the decoder has nothing more.
pub(crate) const BC_EOF: i32 = 2;
/// An argument was out of range.
pub(crate) const BC_ERR_ARGS: i32 = -1;
/// An allocation failed.
pub(crate) const BC_ERR_NOMEM: i32 = -2;
/// The codec or container is not one this build handles.
pub(crate) const BC_ERR_UNSUPPORTED: i32 = -3;
/// The decoder rejected a packet.
pub(crate) const BC_ERR_DECODE: i32 = -4;
/// The demuxer failed.
pub(crate) const BC_ERR_DEMUX: i32 = -5;
/// A decoded frame had a layout outside the shim's limits.
pub(crate) const BC_ERR_LAYOUT: i32 = -6;

/// A track's decoding parameters (`bc_codec_config`).
#[repr(C)]
pub(crate) struct BcCodecConfig {
    /// The shim's codec number ([`crate::Codec::number`]).
    pub(crate) codec: i32,
    /// Stated sample rate in Hz, or 0.
    pub(crate) sample_rate: i32,
    /// Stated channel count, or 0.
    pub(crate) channels: i32,
    /// Bytes per block (WMA), or 0.
    pub(crate) block_align: i32,
    /// Stated bit rate in bits per second, or 0.
    pub(crate) bit_rate: i64,
    /// Codec-specific setup bytes, or null.
    pub(crate) extradata: *const u8,
    /// Length of `extradata` in bytes.
    pub(crate) extradata_len: usize,
}

/// A demuxed track (`bc_track`).
#[repr(C)]
pub(crate) struct BcTrack {
    /// Decoding parameters; `extradata` points into the demuxer.
    pub(crate) config: BcCodecConfig,
    /// Time base numerator, positive.
    pub(crate) time_base_num: i32,
    /// Time base denominator, positive.
    pub(crate) time_base_den: i32,
    /// Track length in time-base units, or -1 when unknown.
    pub(crate) duration: i64,
    /// First timestamp in time-base units.
    pub(crate) start: i64,
}

/// One decoded frame (`bc_frame`).
#[repr(C)]
pub(crate) struct BcFrame {
    /// `frames * channels` interleaved samples, owned by the decoder.
    pub(crate) samples: *const f32,
    /// Frames (samples per channel).
    pub(crate) frames: i32,
    /// Interleaved channels, 1 to 8.
    pub(crate) channels: i32,
    /// Sample rate in Hz.
    pub(crate) sample_rate: i32,
    /// Speaker positions, one bit per channel in interleaved order (WAVE
    /// numbering); 0 when unknown.
    pub(crate) channel_mask: u64,
}

/// Opaque decoder handle (`bc_dec`).
#[repr(C)]
pub(crate) struct BcDec {
    /// Never constructed in Rust.
    _private: [u8; 0],
}

/// Opaque demuxer handle (`bc_fmt`).
#[repr(C)]
pub(crate) struct BcFmt {
    /// Never constructed in Rust.
    _private: [u8; 0],
}

/// Reads up to `len` bytes into `buf`; returns the count, or a negative
/// FFmpeg error (`AVERROR_EOF` at the end).
pub(crate) type BcReadFn = unsafe extern "C" fn(opaque: *mut c_void, buf: *mut u8, len: i32) -> i32;

/// Seeks like `fseek` (`whence` 0, 1, 2) or answers the size
/// (`AVSEEK_SIZE`); returns the new position, the size, or a negative
/// error.
pub(crate) type BcSeekFn =
    unsafe extern "C" fn(opaque: *mut c_void, offset: i64, whence: i32) -> i64;

extern "C" {
    /// Opens a decoder for `config`.
    pub(crate) fn bc_dec_open(config: *const BcCodecConfig, out: *mut *mut BcDec) -> i32;
    /// Hands the decoder one packet.
    pub(crate) fn bc_dec_send(dec: *mut BcDec, data: *const u8, len: usize) -> i32;
    /// Takes the next decoded frame.
    pub(crate) fn bc_dec_receive(dec: *mut BcDec, out: *mut BcFrame) -> i32;
    /// Signals end of stream so a delayed decoder gives up held frames.
    pub(crate) fn bc_dec_drain(dec: *mut BcDec) -> i32;
    /// Drops the decoder's buffered state (after a seek).
    pub(crate) fn bc_dec_flush(dec: *mut BcDec);
    /// Frees a decoder.
    pub(crate) fn bc_dec_close(dec: *mut BcDec);
    /// Opens a demuxer that reads through `read` and `seek`.
    pub(crate) fn bc_fmt_open(
        opaque: *mut c_void,
        read: BcReadFn,
        seek: BcSeekFn,
        out: *mut *mut BcFmt,
    ) -> i32;
    /// Copies out the chosen audio track.
    pub(crate) fn bc_fmt_track(fmt: *const BcFmt, out: *mut BcTrack) -> i32;
    /// Reads the chosen track's next packet; the bytes stay valid until
    /// the next call.
    pub(crate) fn bc_fmt_read(
        fmt: *mut BcFmt,
        data: *mut *const u8,
        len: *mut usize,
        pts: *mut i64,
        duration: *mut i64,
    ) -> i32;
    /// Seeks to the last key point at or before `ts` (time-base units).
    pub(crate) fn bc_fmt_seek(fmt: *mut BcFmt, ts: i64) -> i32;
    /// Frees a demuxer.
    pub(crate) fn bc_fmt_close(fmt: *mut BcFmt);
}
