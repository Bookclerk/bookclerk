//! Low-level big-endian readers and MP4 box walking.

use std::io::{Read, Seek, SeekFrom};

use crate::error::{MediaError, Result};

/// Four-character code.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourCC(pub [u8; 4]);

impl FourCC {
    pub const fn new(b: &[u8; 4]) -> Self {
        Self(*b)
    }

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

pub const FTYP: FourCC = FourCC::new(b"ftyp");
pub const MOOV: FourCC = FourCC::new(b"moov");
pub const MDAT: FourCC = FourCC::new(b"mdat");
pub const TRAK: FourCC = FourCC::new(b"trak");
pub const MDIA: FourCC = FourCC::new(b"mdia");
pub const MINF: FourCC = FourCC::new(b"minf");
pub const STBL: FourCC = FourCC::new(b"stbl");
pub const STSD: FourCC = FourCC::new(b"stsd");
pub const STTS: FourCC = FourCC::new(b"stts");
pub const STSC: FourCC = FourCC::new(b"stsc");
pub const STSZ: FourCC = FourCC::new(b"stsz");
pub const STZ2: FourCC = FourCC::new(b"stz2");
pub const STCO: FourCC = FourCC::new(b"stco");
pub const CO64: FourCC = FourCC::new(b"co64");
pub const MVHD: FourCC = FourCC::new(b"mvhd");
pub const MDHD: FourCC = FourCC::new(b"mdhd");
pub const HDLR: FourCC = FourCC::new(b"hdlr");
pub const AAVD: FourCC = FourCC::new(b"aavd");
pub const MP4A: FourCC = FourCC::new(b"mp4a");
pub const ENCA: FourCC = FourCC::new(b"enca");

/// Header for one ISO-BMFF box.
#[derive(Debug, Clone)]
pub struct BoxHeader {
    pub start: u64,
    pub size: u64,
    pub header_len: u64,
    pub kind: FourCC,
}

impl BoxHeader {
    pub fn content_start(&self) -> u64 {
        self.start + self.header_len
    }

    pub fn content_len(&self) -> u64 {
        self.size.saturating_sub(self.header_len)
    }

    pub fn end(&self) -> u64 {
        self.start + self.size
    }
}

pub fn read_u8(r: &mut impl Read) -> Result<u8> {
    let mut buf = [0u8; 1];
    r.read_exact(&mut buf)?;
    Ok(buf[0])
}

pub fn read_u32(r: &mut impl Read) -> Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_be_bytes(buf))
}

pub fn read_u64(r: &mut impl Read) -> Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_be_bytes(buf))
}

pub fn read_fourcc(r: &mut impl Read) -> Result<FourCC> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(FourCC(buf))
}

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
        return Err(MediaError::Mp4(format!(
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
pub fn walk_children<R, F>(r: &mut R, start: u64, end: u64, mut visit: F) -> Result<()>
where
    R: Read + Seek,
    F: FnMut(&mut R, &BoxHeader) -> Result<()>,
{
    r.seek(SeekFrom::Start(start))?;
    while r.stream_position()? + 8 <= end {
        let header = read_box_header(r)?;
        if header.end() > end {
            return Err(MediaError::Mp4(format!(
                "box {} overflows parent (end {} > {end})",
                header.kind,
                header.end()
            )));
        }
        let next = header.end();
        visit(r, &header)?;
        r.seek(SeekFrom::Start(next))?;
    }
    Ok(())
}

pub fn find_child<R: Read + Seek>(
    r: &mut R,
    start: u64,
    end: u64,
    want: FourCC,
) -> Result<Option<BoxHeader>> {
    let mut found = None;
    walk_children(r, start, end, |_, header| {
        if header.kind == want && found.is_none() {
            found = Some(header.clone());
        }
        Ok(())
    })?;
    Ok(found)
}

pub fn read_full_box_version_flags(r: &mut impl Read) -> Result<(u8, u32)> {
    let version = read_u8(r)?;
    let mut flags = [0u8; 3];
    r.read_exact(&mut flags)?;
    let flags_u32 = (u32::from(flags[0]) << 16) | (u32::from(flags[1]) << 8) | u32::from(flags[2]);
    Ok((version, flags_u32))
}
