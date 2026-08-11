//! Low-level big-endian readers and MP4 box walking.

use std::io::{Read, Seek, SeekFrom};

use crate::error::{Mp4Error, Result};

/// ISO-BMFF four-character type code (FourCC).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourCC(pub [u8; 4]);

impl FourCC {
    /// Wraps a four-byte type code (no allocation).
    ///
    /// # Arguments
    ///
    /// * `b` - Exactly four ASCII bytes (e.g. `b"moov"`).
    ///
    /// # Returns
    ///
    /// A [`FourCC`] viewing those bytes.
    pub const fn new(b: &[u8; 4]) -> Self {
        Self(*b)
    }

    /// Returns the FourCC as a UTF-8 string when the bytes are valid ASCII.
    ///
    /// # Returns
    ///
    /// The type code as `&str`, or `"????"` when the bytes are not UTF-8.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("????")
    }
}

impl std::fmt::Debug for FourCC {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::fmt::Display for FourCC {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// ISO-BMFF `ftyp` (file type) FourCC.
pub const FTYP: FourCC = FourCC::new(b"ftyp");
/// ISO-BMFF `moov` (movie metadata) FourCC.
pub const MOOV: FourCC = FourCC::new(b"moov");
/// ISO-BMFF `mdat` (media data) FourCC.
pub const MDAT: FourCC = FourCC::new(b"mdat");
/// ISO-BMFF `trak` (track) FourCC.
pub const TRAK: FourCC = FourCC::new(b"trak");
/// ISO-BMFF `mdia` (media) FourCC.
pub const MDIA: FourCC = FourCC::new(b"mdia");
/// ISO-BMFF `minf` (media information) FourCC.
pub const MINF: FourCC = FourCC::new(b"minf");
/// ISO-BMFF `stbl` (sample table) FourCC.
pub const STBL: FourCC = FourCC::new(b"stbl");
/// ISO-BMFF `stsd` (sample description) FourCC.
pub const STSD: FourCC = FourCC::new(b"stsd");
/// ISO-BMFF `stts` (time-to-sample) FourCC.
pub const STTS: FourCC = FourCC::new(b"stts");
/// ISO-BMFF `stsc` (sample-to-chunk) FourCC.
pub const STSC: FourCC = FourCC::new(b"stsc");
/// ISO-BMFF `stsz` (sample sizes) FourCC.
pub const STSZ: FourCC = FourCC::new(b"stsz");
/// ISO-BMFF `stz2` (compact sample sizes) FourCC.
pub const STZ2: FourCC = FourCC::new(b"stz2");
/// ISO-BMFF `stco` (32-bit chunk offsets) FourCC.
pub const STCO: FourCC = FourCC::new(b"stco");
/// ISO-BMFF `co64` (64-bit chunk offsets) FourCC.
pub const CO64: FourCC = FourCC::new(b"co64");
/// ISO-BMFF `mvhd` (movie header) FourCC.
pub const MVHD: FourCC = FourCC::new(b"mvhd");
/// ISO-BMFF `mdhd` (media header) FourCC.
pub const MDHD: FourCC = FourCC::new(b"mdhd");
/// ISO-BMFF `hdlr` (handler reference) FourCC.
pub const HDLR: FourCC = FourCC::new(b"hdlr");
/// Audible `aavd` encrypted-audio sample-entry FourCC.
pub const AAVD: FourCC = FourCC::new(b"aavd");
/// MPEG-4 audio (`mp4a`) sample-entry FourCC.
pub const MP4A: FourCC = FourCC::new(b"mp4a");
/// Encrypted audio (`enca`) sample-entry FourCC.
pub const ENCA: FourCC = FourCC::new(b"enca");

// Fragmented (DASH) and Common Encryption boxes. Naming the box types costs
// nothing here and keeps every reader in the workspace spelling them the same
// way; the schemes themselves are a caller's business.
/// ISO-BMFF `sidx` (segment index) FourCC.
pub const SIDX: FourCC = FourCC::new(b"sidx");
/// ISO-BMFF `moof` (movie fragment) FourCC.
pub const MOOF: FourCC = FourCC::new(b"moof");
/// ISO-BMFF `traf` (track fragment) FourCC.
pub const TRAF: FourCC = FourCC::new(b"traf");
/// ISO-BMFF `tfhd` (track fragment header) FourCC.
pub const TFHD: FourCC = FourCC::new(b"tfhd");
/// ISO-BMFF `trun` (track fragment run) FourCC.
pub const TRUN: FourCC = FourCC::new(b"trun");
/// ISO-BMFF `senc` (sample encryption) FourCC.
pub const SENC: FourCC = FourCC::new(b"senc");
/// ISO-BMFF `sinf` (protection scheme info) FourCC.
pub const SINF: FourCC = FourCC::new(b"sinf");
/// ISO-BMFF `schm` (scheme type) FourCC.
pub const SCHM: FourCC = FourCC::new(b"schm");
/// ISO-BMFF `schi` (scheme information) FourCC.
pub const SCHI: FourCC = FourCC::new(b"schi");
/// ISO-BMFF `tenc` (track encryption) FourCC.
pub const TENC: FourCC = FourCC::new(b"tenc");
/// ISO-BMFF `saiz` (sample auxiliary info sizes) FourCC.
pub const SAIZ: FourCC = FourCC::new(b"saiz");
/// ISO-BMFF `saio` (sample auxiliary info offsets) FourCC.
pub const SAIO: FourCC = FourCC::new(b"saio");
/// ISO-BMFF `mvex` (movie extends) FourCC.
pub const MVEX: FourCC = FourCC::new(b"mvex");
/// ISO-BMFF `dash` brand FourCC.
pub const DASH: FourCC = FourCC::new(b"dash");

/// Header for one ISO-BMFF box.
#[derive(Debug, Clone)]
pub struct BoxHeader {
    /// Byte offset of this box or region within the file.
    pub start: u64,
    /// Total box size in bytes including the header.
    pub size: u64,
    /// Header length in bytes (8 or 16 for extended-size boxes).
    pub header_len: u64,
    /// Discriminant or category for this value.
    pub kind: FourCC,
}

impl BoxHeader {
    /// Byte offset where this box's content (after the header) begins.
    pub fn content_start(&self) -> u64 {
        self.start + self.header_len
    }

    /// Payload length in bytes (total size minus header length).
    pub fn content_len(&self) -> u64 {
        self.size.saturating_sub(self.header_len)
    }

    /// Byte offset immediately after this box (exclusive end).
    pub fn end(&self) -> u64 {
        self.start + self.size
    }
}

/// Reads one unsigned byte from `data` at `offset` and advances it.
///
/// # Arguments
///
/// * `r` - `r` input for this call.
///
/// # Returns
///
/// On success, the inner `u8` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn read_u8(r: &mut impl Read) -> Result<u8> {
    let mut buf = [0u8; 1];
    r.read_exact(&mut buf)?;
    Ok(buf[0])
}

/// Reads a big-endian `u32` from `data` at `offset` and advances it.
///
/// # Arguments
///
/// * `r` - `r` input for this call.
///
/// # Returns
///
/// On success, the inner `u32` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn read_u32(r: &mut impl Read) -> Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_be_bytes(buf))
}

/// Reads a big-endian `u64` from `data` at `offset` and advances it.
///
/// # Arguments
///
/// * `r` - `r` input for this call.
///
/// # Returns
///
/// On success, the inner `u64` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn read_u64(r: &mut impl Read) -> Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_be_bytes(buf))
}

/// Reads a four-byte FourCC from `data` at `offset` and advances it.
///
/// # Arguments
///
/// * `r` - `r` input for this call.
///
/// # Returns
///
/// On success, the inner `FourCC` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn read_fourcc(r: &mut impl Read) -> Result<FourCC> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(FourCC(buf))
}

/// Reads an ISO-BMFF box header (size + type, including extended size).
///
/// # Arguments
///
/// * `r` - `r` input for this call.
///
/// # Returns
///
/// On success, the inner `BoxHeader` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn read_box_header(r: &mut (impl Read + Seek)) -> Result<BoxHeader> {
    let start = r.stream_position()?;
    let size32 = read_u32(r)?;
    let kind = read_fourcc(r)?;
    let (size, header_len) = if size32 == 1 {
        let size64 = read_u64(r)?;
        (size64, 16u64)
    } else if size32 == 0 {
        // Extends to EOF.
        let end = r.seek(SeekFrom::End(0))?;
        r.seek(SeekFrom::Start(start + 8))?;
        (end - start, 8u64)
    } else {
        (u64::from(size32), 8u64)
    };
    if size < header_len {
        return Err(Mp4Error::container(format!(
            "box {kind} at {start} has size {size} < header {header_len}"
        )));
    }
    Ok(BoxHeader {
        start,
        size,
        header_len,
        kind,
    })
}

/// Iterate immediate children of a container box whose content starts at `start`
/// and ends at `end`.
///
/// The visitor keeps its own error type, so a caller reading boxes this crate
/// knows nothing about can fail in its own vocabulary.
///
/// # Arguments
///
/// * `r` - `r` input for this call.
/// * `start` - Numeric `start` value for this call.
/// * `end` - Numeric `end` value for this call.
/// * `visit` - `visit` input for this call.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn walk_children<R, F, E>(
    r: &mut R,
    start: u64,
    end: u64,
    mut visit: F,
) -> std::result::Result<(), E>
where
    R: Read + Seek,
    F: FnMut(&mut R, &BoxHeader) -> std::result::Result<(), E>,
    E: From<Mp4Error>,
{
    let seek = |r: &mut R, to: u64| -> std::result::Result<u64, E> {
        r.seek(SeekFrom::Start(to))
            .map_err(|err| E::from(Mp4Error::from(err)))
    };
    seek(r, start)?;
    loop {
        let pos = r
            .stream_position()
            .map_err(|err| E::from(Mp4Error::from(err)))?;
        if pos + 8 > end {
            break;
        }
        let header = read_box_header(r).map_err(E::from)?;
        if header.end() > end {
            return Err(E::from(Mp4Error::container(format!(
                "box {} overflows parent (end {} > {end})",
                header.kind,
                header.end()
            ))));
        }
        let next = header.end();
        visit(r, &header)?;
        seek(r, next)?;
    }
    Ok(())
}

/// Finds the first direct child box with FourCC `want` inside parent content.
///
/// # Arguments
///
/// * `r` - `r` input for this call.
/// * `start` - Numeric `start` value for this call.
/// * `end` - Numeric `end` value for this call.
/// * `want` - FourCC to search for among child boxes.
///
/// # Returns
///
/// On success, the inner `Option<BoxHeader>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn find_child<R: Read + Seek>(
    r: &mut R,
    start: u64,
    end: u64,
    want: FourCC,
) -> Result<Option<BoxHeader>> {
    let mut found = None;
    walk_children(r, start, end, |_, header| -> Result<()> {
        if header.kind == want && found.is_none() {
            found = Some(header.clone());
        }
        Ok(())
    })?;
    Ok(found)
}

/// Reads a FullBox version (`u8`) and flags (`u24`) at `offset`.
///
/// # Arguments
///
/// * `r` - `r` input for this call.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn read_full_box_version_flags(r: &mut impl Read) -> Result<(u8, u32)> {
    let version = read_u8(r)?;
    let mut flags = [0u8; 3];
    r.read_exact(&mut flags)?;
    let flags_u32 = (u32::from(flags[0]) << 16) | (u32::from(flags[1]) << 8) | u32::from(flags[2]);
    Ok((version, flags_u32))
}
