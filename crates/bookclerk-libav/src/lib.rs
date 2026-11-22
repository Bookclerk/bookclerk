//! Decoders-only FFmpeg for the audio formats Bookclerk's own decoders do
//! not cover: AC-3 and E-AC-3, Windows Media Audio (v1, v2, Pro and
//! Lossless) and AMR (narrowband and wideband).
//!
//! The build script compiles a pinned FFmpeg release with nothing but
//! those decoders, their demuxers (ASF, raw AC-3/E-AC-3, raw AMR and
//! ISO-BMFF for 3GP) and the AC-3 parser, and links it statically. A
//! small C shim (`src/shim.c`) is the only code that touches FFmpeg's
//! structs, so this crate never depends on their layout.
//!
//! The media worker is the only user: it decodes untrusted files with
//! [`Demuxer`] and [`Decoder`] inside its confinement, the same as every
//! other codec. FFmpeg is LGPL-2.1-or-later in this configuration; see
//! `docs/contributing/licensing.md`.
//!
//! Unix and Windows (MSVC) only: on other targets the crate is empty, and
//! the `libav` feature of `bookclerk-media` that pulls it in stays off
//! there.
#![cfg(any(unix, all(windows, target_env = "msvc")))]

use std::ffi::c_void;
use std::io::{Read, Seek, SeekFrom};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

mod ffi;

#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;

/// The FFmpeg release this crate builds, as `configure` names it.
pub const FFMPEG_VERSION: &str = "9.0.2";

/// Most bytes of codec setup data accepted from a container (1 MiB),
/// matching the shim's own limit.
pub const MAX_EXTRADATA: usize = 1 << 20;

/// FFmpeg's end-of-file error (`AVERROR_EOF`).
const AVERROR_EOF: i32 = -0x2046_4F45;
/// FFmpeg's I/O error for a failed read or seek (`AVERROR(EIO)`).
const AVERROR_EIO: i32 = -5;
/// The `whence` value that asks a seek callback for the stream size.
const AVSEEK_SIZE: i32 = 0x1_0000;
/// Bit FFmpeg may set on `whence` to allow a costly seek; ignored here.
const AVSEEK_FORCE: i32 = 0x2_0000;

/// Why a call into FFmpeg failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A value handed to the shim was out of range.
    #[error("libav: argument out of range")]
    Arguments,
    /// An allocation failed.
    #[error("libav: out of memory")]
    OutOfMemory,
    /// This build has no decoder or demuxer for the input.
    #[error("libav: unsupported codec or container")]
    Unsupported,
    /// The decoder rejected a packet as corrupt.
    #[error("libav: packet could not be decoded")]
    Decode,
    /// The demuxer failed.
    #[error("libav: container could not be read")]
    Demux,
    /// A decoded frame had a channel count, rate or length outside the
    /// shim's limits.
    #[error("libav: decoded frame has an unusable layout")]
    Layout,
}

impl Error {
    /// The error for a negative shim result.
    fn from_code(code: i32) -> Self {
        match code {
            ffi::BC_ERR_NOMEM => Self::OutOfMemory,
            ffi::BC_ERR_UNSUPPORTED => Self::Unsupported,
            ffi::BC_ERR_DECODE => Self::Decode,
            ffi::BC_ERR_DEMUX => Self::Demux,
            ffi::BC_ERR_LAYOUT => Self::Layout,
            ffi::BC_ERR_ARGS => Self::Arguments,
            // A code the shim does not define.
            _ => Self::Arguments,
        }
    }
}

/// A codec this build decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    /// Dolby Digital (AC-3).
    Ac3,
    /// Dolby Digital Plus (E-AC-3).
    Eac3,
    /// Windows Media Audio 1.
    WmaV1,
    /// Windows Media Audio 2.
    WmaV2,
    /// Windows Media Audio 9 Professional.
    WmaPro,
    /// Windows Media Audio 9 Lossless.
    WmaLossless,
    /// Adaptive Multi-Rate narrowband (8 kHz speech).
    AmrNb,
    /// Adaptive Multi-Rate wideband (16 kHz speech).
    AmrWb,
}

impl Codec {
    /// Every codec, in the shim's numbering order.
    pub const ALL: [Codec; 8] = [
        Codec::Ac3,
        Codec::Eac3,
        Codec::WmaV1,
        Codec::WmaV2,
        Codec::WmaPro,
        Codec::WmaLossless,
        Codec::AmrNb,
        Codec::AmrWb,
    ];

    /// The shim's number for this codec (1 to 8).
    #[must_use]
    pub fn number(self) -> u8 {
        match self {
            Self::Ac3 => 1,
            Self::Eac3 => 2,
            Self::WmaV1 => 3,
            Self::WmaV2 => 4,
            Self::WmaPro => 5,
            Self::WmaLossless => 6,
            Self::AmrNb => 7,
            Self::AmrWb => 8,
        }
    }

    /// The codec with shim number `number`, if any.
    #[must_use]
    pub fn from_number(number: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|codec| codec.number() == number)
    }

    /// FFmpeg's short name for the decoder (`ac3`, `wmav2`, `amrwb`, …).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Ac3 => "ac3",
            Self::Eac3 => "eac3",
            Self::WmaV1 => "wmav1",
            Self::WmaV2 => "wmav2",
            Self::WmaPro => "wmapro",
            Self::WmaLossless => "wmalossless",
            Self::AmrNb => "amrnb",
            Self::AmrWb => "amrwb",
        }
    }
}

/// What a decoder needs to know about a track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecConfig {
    /// The codec.
    pub codec: Codec,
    /// Stated sample rate in Hz, or 0 when the stream carries its own.
    pub sample_rate: u32,
    /// Stated channel count, or 0 when the stream carries its own.
    pub channels: u16,
    /// Bytes per coded block (WMA needs it), or 0.
    pub block_align: u32,
    /// Stated bit rate in bits per second (WMA needs it), or 0.
    pub bit_rate: u64,
    /// Codec setup bytes from the container (WMA needs them), at most
    /// [`MAX_EXTRADATA`].
    pub extradata: Vec<u8>,
}

impl CodecConfig {
    /// A config for `codec` with nothing stated: enough for AC-3, E-AC-3
    /// and AMR, whose streams describe themselves.
    #[must_use]
    pub fn bare(codec: Codec) -> Self {
        Self {
            codec,
            sample_rate: 0,
            channels: 0,
            block_align: 0,
            bit_rate: 0,
            extradata: Vec::new(),
        }
    }

    /// The shim struct for this config, borrowing [`Self::extradata`].
    ///
    /// # Errors
    ///
    /// [`Error::Arguments`] when a value does not fit the shim's fields or
    /// the setup bytes are longer than [`MAX_EXTRADATA`].
    fn raw(&self) -> Result<ffi::BcCodecConfig, Error> {
        if self.extradata.len() > MAX_EXTRADATA {
            return Err(Error::Arguments);
        }
        Ok(ffi::BcCodecConfig {
            codec: i32::from(self.codec.number()),
            sample_rate: i32::try_from(self.sample_rate).map_err(|_| Error::Arguments)?,
            channels: i32::from(self.channels),
            block_align: i32::try_from(self.block_align).map_err(|_| Error::Arguments)?,
            bit_rate: i64::try_from(self.bit_rate).map_err(|_| Error::Arguments)?,
            extradata: if self.extradata.is_empty() {
                ptr::null()
            } else {
                self.extradata.as_ptr()
            },
            extradata_len: self.extradata.len(),
        })
    }
}

/// One decoded frame: interleaved 32-bit float samples in -1.0..=1.0.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    /// `frames * channels` samples, channel-interleaved.
    pub samples: &'a [f32],
    /// Samples per channel.
    pub frames: usize,
    /// Interleaved channels, 1 to 8, in FFmpeg's default order for the
    /// count (left, right, centre, LFE, …).
    pub channels: usize,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Speaker positions in WAVE bit numbering (bit 0 front left, 1 front
    /// right, 2 centre, 3 LFE, …), one bit per channel in interleaved
    /// order; 0 when the decoder states none.
    pub channel_mask: u64,
}

/// An FFmpeg audio decoder for one track.
pub struct Decoder {
    /// The shim's handle, never null.
    raw: *mut ffi::BcDec,
}

// SAFETY: the handle is owned by this value alone and FFmpeg decoders keep
// no thread-local state; moving it to another thread is sound.
#[allow(unsafe_code)]
unsafe impl Send for Decoder {}
// SAFETY: every method that touches the handle takes `&mut self`, so a
// shared reference gives no access to it.
#[allow(unsafe_code)]
unsafe impl Sync for Decoder {}

impl Decoder {
    /// Opens a decoder for `config`.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when FFmpeg will not open the codec with
    /// these parameters, [`Error::Arguments`] for out-of-range values, or
    /// [`Error::OutOfMemory`].
    pub fn new(config: &CodecConfig) -> Result<Self, Error> {
        let raw_config = config.raw()?;
        let mut raw = ptr::null_mut();
        // SAFETY: `raw_config` borrows `config.extradata`, which outlives
        // the call; the shim copies what it keeps.
        #[allow(unsafe_code)]
        let code = unsafe { ffi::bc_dec_open(&raw_config, &mut raw) };
        if code != ffi::BC_OK || raw.is_null() {
            return Err(Error::from_code(code.min(-1)));
        }
        Ok(Self { raw })
    }

    /// Decodes one packet, calling `on_frame` for each frame it gives.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the decoder rejects the packet (the caller
    /// may skip it and go on), [`Error::Layout`] for a frame outside the
    /// shim's limits, or [`Error::OutOfMemory`].
    pub fn decode<F>(&mut self, packet: &[u8], mut on_frame: F) -> Result<(), Error>
    where
        F: FnMut(Frame<'_>),
    {
        // SAFETY: `packet` is valid for its length during the call; the
        // shim copies it into a padded FFmpeg packet.
        #[allow(unsafe_code)]
        let code = unsafe { ffi::bc_dec_send(self.raw, packet.as_ptr(), packet.len()) };
        if code < 0 {
            return Err(Error::from_code(code));
        }
        self.receive(&mut on_frame)
    }

    /// Ends the stream: hands back the frames a delayed decoder (WMA v1,
    /// v2 and lossless) still holds, then resets it so later packets, as
    /// after a seek, decode from a clean state.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the decoder refuses the end of stream,
    /// [`Error::Layout`] for a frame outside the shim's limits, or
    /// [`Error::OutOfMemory`]. The decoder is reset either way.
    pub fn drain<F>(&mut self, mut on_frame: F) -> Result<(), Error>
    where
        F: FnMut(Frame<'_>),
    {
        // SAFETY: the handle is valid for the life of `self`.
        #[allow(unsafe_code)]
        let code = unsafe { ffi::bc_dec_drain(self.raw) };
        let drained = if code < 0 {
            Err(Error::from_code(code))
        } else {
            self.receive(&mut on_frame)
        };
        self.flush();
        drained
    }

    /// Takes every frame the decoder has ready, calling `on_frame` for
    /// each, until it wants another packet or the stream has ended.
    ///
    /// # Errors
    ///
    /// As for [`Decoder::decode`].
    fn receive<F>(&mut self, on_frame: &mut F) -> Result<(), Error>
    where
        F: FnMut(Frame<'_>),
    {
        loop {
            let mut frame = ffi::BcFrame {
                samples: ptr::null(),
                frames: 0,
                channels: 0,
                sample_rate: 0,
                channel_mask: 0,
            };
            // SAFETY: `frame` is a valid out-pointer; on success the shim
            // fills it with a buffer it owns until the next call.
            #[allow(unsafe_code)]
            let code = unsafe { ffi::bc_dec_receive(self.raw, &mut frame) };
            match code {
                ffi::BC_OK => {}
                ffi::BC_AGAIN | ffi::BC_EOF => return Ok(()),
                err => return Err(Error::from_code(err)),
            }
            let (Ok(frames), Ok(channels), Ok(sample_rate)) = (
                usize::try_from(frame.frames),
                usize::try_from(frame.channels),
                u32::try_from(frame.sample_rate),
            ) else {
                return Err(Error::Layout);
            };
            let Some(len) = frames.checked_mul(channels) else {
                return Err(Error::Layout);
            };
            let samples: &[f32] = if len == 0 || frame.samples.is_null() {
                &[]
            } else {
                // SAFETY: the shim guarantees `samples` holds `frames *
                // channels` floats, valid until the next decoder call,
                // which cannot happen while this borrow of `self` lives.
                #[allow(unsafe_code)]
                unsafe {
                    std::slice::from_raw_parts(frame.samples, len)
                }
            };
            on_frame(Frame {
                samples,
                frames: if samples.is_empty() { 0 } else { frames },
                channels,
                sample_rate,
                channel_mask: frame.channel_mask,
            });
        }
    }

    /// Drops buffered decoder state, as after a seek.
    pub fn flush(&mut self) {
        // SAFETY: the handle is valid for the life of `self`.
        #[allow(unsafe_code)]
        unsafe {
            ffi::bc_dec_flush(self.raw);
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the handle came from `bc_dec_open` and is freed once.
        #[allow(unsafe_code)]
        unsafe {
            ffi::bc_dec_close(self.raw);
        }
    }
}

/// Magic bytes of an ASF header object (the container of WMA files).
const ASF_MAGIC: [u8; 16] = [
    0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];

/// Whether `head`, the first bytes of a file, starts a container that
/// only this crate reads: ASF, raw AMR (`#!AMR`) or a raw AC-3/E-AC-3
/// stream (sync word `0x0B77` at offset 0).
///
/// Callers check this before handing the file to another demuxer, which
/// might otherwise mistake a raw stream for MPEG audio while hunting for
/// frame syncs.
#[must_use]
pub fn sniff(head: &[u8]) -> bool {
    head.starts_with(&ASF_MAGIC) || head.starts_with(b"#!AMR") || head.starts_with(&[0x0B, 0x77])
}

/// Bytes [`sniff`] needs to decide.
pub const SNIFF_LEN: usize = 16;

/// A seekable byte source a [`Demuxer`] reads from.
pub trait Source: Read + Seek + Send {}

impl<T: Read + Seek + Send> Source for T {}

/// The reader behind a demuxer, boxed so its address is stable for the
/// shim's callbacks.
struct Input {
    /// The bytes.
    source: Box<dyn Source>,
}

/// The audio track a [`Demuxer`] chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    /// What a [`Decoder`] needs for it.
    pub config: CodecConfig,
    /// Seconds per timestamp tick: `time_base.0 / time_base.1`, both
    /// positive.
    pub time_base: (u32, u32),
    /// Length in ticks, when the container states it.
    pub duration: Option<u64>,
    /// First timestamp in ticks.
    pub start: i64,
}

/// One demuxed packet of the chosen track.
#[derive(Debug, Clone, Copy)]
pub struct Packet<'a> {
    /// The coded bytes, valid until the next demuxer call.
    pub data: &'a [u8],
    /// Presentation timestamp in ticks (may be negative or missing as
    /// `i64::MIN` for some streams).
    pub pts: i64,
    /// Length in ticks, or 0 when not stated.
    pub duration: u64,
}

/// An FFmpeg demuxer over a byte source, reading one audio track.
pub struct Demuxer {
    /// The shim's handle, never null.
    raw: *mut ffi::BcFmt,
    /// The source the shim's callbacks read through a raw pointer; never
    /// touched from Rust, and dropped only after `raw` is closed.
    _input: Box<Input>,
    /// The chosen track.
    track: Track,
}

// SAFETY: the handle and its input are owned by this value alone, the
// source is `Send`, and FFmpeg demuxers keep no thread-local state.
#[allow(unsafe_code)]
unsafe impl Send for Demuxer {}
// SAFETY: the only `&self` method returns the cached [`Track`]; every
// method that touches the handle takes `&mut self`.
#[allow(unsafe_code)]
unsafe impl Sync for Demuxer {}

impl Demuxer {
    /// Opens the container in `source` and picks its best audio track.
    ///
    /// # Errors
    ///
    /// [`Error::Demux`] when no enabled demuxer recognises the bytes or
    /// the header is broken, [`Error::Unsupported`] when there is no audio
    /// track in a codec this build decodes, or [`Error::OutOfMemory`].
    pub fn open(source: Box<dyn Source>) -> Result<Self, Error> {
        let mut input = Box::new(Input { source });
        let opaque: *mut Input = &mut *input;
        let mut raw = ptr::null_mut();
        // SAFETY: `opaque` points at the boxed input, which the returned
        // demuxer owns and drops only after closing the handle; the
        // callbacks treat it as `*mut Input`.
        #[allow(unsafe_code)]
        let code = unsafe { ffi::bc_fmt_open(opaque.cast::<c_void>(), read_cb, seek_cb, &mut raw) };
        if code != ffi::BC_OK || raw.is_null() {
            return Err(Error::from_code(code.min(-1)));
        }
        let mut demuxer = Self {
            raw,
            _input: input,
            track: Track {
                config: CodecConfig::bare(Codec::Ac3),
                time_base: (1, 1),
                duration: None,
                start: 0,
            },
        };
        demuxer.track = demuxer.read_track()?;
        Ok(demuxer)
    }

    /// Copies the chosen track out of the shim.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] for a codec number this crate does not know,
    /// or [`Error::Demux`] for values out of range.
    fn read_track(&mut self) -> Result<Track, Error> {
        let mut raw = ffi::BcTrack {
            config: ffi::BcCodecConfig {
                codec: 0,
                sample_rate: 0,
                channels: 0,
                block_align: 0,
                bit_rate: 0,
                extradata: ptr::null(),
                extradata_len: 0,
            },
            time_base_num: 0,
            time_base_den: 0,
            duration: -1,
            start: 0,
        };
        // SAFETY: valid handle and out-pointer.
        #[allow(unsafe_code)]
        let code = unsafe { ffi::bc_fmt_track(self.raw, &mut raw) };
        if code != ffi::BC_OK {
            return Err(Error::from_code(code.min(-1)));
        }
        let codec = u8::try_from(raw.config.codec)
            .ok()
            .and_then(Codec::from_number)
            .ok_or(Error::Unsupported)?;
        if raw.config.extradata_len > MAX_EXTRADATA {
            return Err(Error::Demux);
        }
        let extradata = if raw.config.extradata.is_null() || raw.config.extradata_len == 0 {
            Vec::new()
        } else {
            // SAFETY: the shim points at the stream's setup bytes, owned
            // by the demuxer and `extradata_len` long; copied at once.
            #[allow(unsafe_code)]
            unsafe { std::slice::from_raw_parts(raw.config.extradata, raw.config.extradata_len) }
                .to_vec()
        };
        let positive = |value: i32| u32::try_from(value).ok().filter(|v| *v > 0);
        let (Some(num), Some(den)) = (positive(raw.time_base_num), positive(raw.time_base_den))
        else {
            return Err(Error::Demux);
        };
        Ok(Track {
            config: CodecConfig {
                codec,
                sample_rate: u32::try_from(raw.config.sample_rate).unwrap_or(0),
                channels: u16::try_from(raw.config.channels).unwrap_or(0),
                block_align: u32::try_from(raw.config.block_align).unwrap_or(0),
                bit_rate: u64::try_from(raw.config.bit_rate).unwrap_or(0),
                extradata,
            },
            time_base: (num, den),
            duration: u64::try_from(raw.duration).ok(),
            start: raw.start,
        })
    }

    /// The chosen audio track.
    #[must_use]
    pub fn track(&self) -> &Track {
        &self.track
    }

    /// Reads the chosen track's next packet.
    ///
    /// # Returns
    ///
    /// The packet, or `None` at the end of the stream.
    ///
    /// # Errors
    ///
    /// [`Error::Demux`] when the container is broken (or the source
    /// failed), or [`Error::OutOfMemory`].
    pub fn next_packet(&mut self) -> Result<Option<Packet<'_>>, Error> {
        let mut data = ptr::null();
        let mut len = 0usize;
        let mut pts = 0i64;
        let mut duration = 0i64;
        // SAFETY: valid handle and out-pointers.
        #[allow(unsafe_code)]
        let code =
            unsafe { ffi::bc_fmt_read(self.raw, &mut data, &mut len, &mut pts, &mut duration) };
        match code {
            ffi::BC_OK => {}
            ffi::BC_EOF => return Ok(None),
            err => return Err(Error::from_code(err.min(-1))),
        }
        if data.is_null() || len == 0 {
            return Err(Error::Demux);
        }
        // SAFETY: the shim's packet owns `len` bytes at `data` until the
        // next demuxer call, which this borrow of `self` prevents.
        #[allow(unsafe_code)]
        let data = unsafe { std::slice::from_raw_parts(data, len) };
        Ok(Some(Packet {
            data,
            pts,
            duration: u64::try_from(duration).unwrap_or(0),
        }))
    }

    /// Seeks to the last key point at or before `ts` (in track ticks).
    ///
    /// # Errors
    ///
    /// [`Error::Demux`] when the container cannot seek there.
    pub fn seek(&mut self, ts: i64) -> Result<(), Error> {
        // SAFETY: valid handle.
        #[allow(unsafe_code)]
        let code = unsafe { ffi::bc_fmt_seek(self.raw, ts) };
        if code != ffi::BC_OK {
            return Err(Error::from_code(code.min(-1)));
        }
        Ok(())
    }
}

impl Drop for Demuxer {
    fn drop(&mut self) {
        // SAFETY: the handle came from `bc_fmt_open` and is freed once,
        // before `input` (which its callbacks read) is dropped.
        #[allow(unsafe_code)]
        unsafe {
            ffi::bc_fmt_close(self.raw);
        }
    }
}

/// The shim's read callback: fills `buf` from the [`Input`] at `opaque`.
#[allow(unsafe_code)]
unsafe extern "C" fn read_cb(opaque: *mut c_void, buf: *mut u8, len: i32) -> i32 {
    let Ok(len) = usize::try_from(len) else {
        return AVERROR_EIO;
    };
    if opaque.is_null() || buf.is_null() || len == 0 {
        return AVERROR_EIO;
    }
    // SAFETY: `opaque` is the demuxer's boxed `Input` (see
    // `Demuxer::open`), alive and not otherwise borrowed during the call;
    // FFmpeg hands a writable buffer of `len` bytes.
    let (input, out) = unsafe {
        (
            &mut *opaque.cast::<Input>(),
            std::slice::from_raw_parts_mut(buf, len),
        )
    };
    let read = catch_unwind(AssertUnwindSafe(|| loop {
        match input.source.read(out) {
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }));
    match read {
        Ok(Ok(0)) => AVERROR_EOF,
        Ok(Ok(n)) => i32::try_from(n).unwrap_or(AVERROR_EIO),
        Ok(Err(_)) | Err(_) => AVERROR_EIO,
    }
}

/// The shim's seek callback: seeks the [`Input`] at `opaque`, or answers
/// its size for `AVSEEK_SIZE`.
#[allow(unsafe_code)]
unsafe extern "C" fn seek_cb(opaque: *mut c_void, offset: i64, whence: i32) -> i64 {
    if opaque.is_null() {
        return i64::from(AVERROR_EIO);
    }
    // SAFETY: as in `read_cb`.
    let input = unsafe { &mut *opaque.cast::<Input>() };
    let whence = whence & !AVSEEK_FORCE;
    let result = catch_unwind(AssertUnwindSafe(|| -> std::io::Result<u64> {
        let source = &mut input.source;
        match whence {
            AVSEEK_SIZE => {
                let here = source.stream_position()?;
                let end = source.seek(SeekFrom::End(0))?;
                source.seek(SeekFrom::Start(here))?;
                Ok(end)
            }
            0 => source.seek(SeekFrom::Start(
                u64::try_from(offset).map_err(|_| std::io::ErrorKind::InvalidInput)?,
            )),
            1 => source.seek(SeekFrom::Current(offset)),
            2 => source.seek(SeekFrom::End(offset)),
            _ => Err(std::io::ErrorKind::InvalidInput.into()),
        }
    }));
    match result {
        Ok(Ok(pos)) => i64::try_from(pos).unwrap_or(i64::from(AVERROR_EIO)),
        Ok(Err(_)) | Err(_) => i64::from(AVERROR_EIO),
    }
}

#[cfg(test)]
mod tests;
