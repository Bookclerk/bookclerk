//! Archive builders for tests and fuzz seeds, including archives a careful
//! tool would refuse to write.
// Fixture builders: a failed write into memory is a broken test.
#![allow(clippy::expect_used, clippy::missing_panics_doc)]

use std::io::{Cursor, Write};

use flate2::write::GzEncoder;
use flate2::Compression;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// How a fixture ZIP entry is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZipEntry<'a> {
    /// A file stored without compression.
    Stored(&'a str, &'a [u8]),
    /// A file compressed with Deflate.
    Deflated(&'a str, &'a [u8]),
    /// A symbolic link to a target.
    Symlink(&'a str, &'a str),
    /// A folder.
    Dir(&'a str),
}

/// A ZIP archive of `entries`, names written exactly as given.
#[must_use]
pub fn zip_of(entries: &[ZipEntry<'_>]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for entry in entries {
        match *entry {
            ZipEntry::Stored(name, data) => {
                let options =
                    SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
                writer.start_file(name, options).expect("start");
                writer.write_all(data).expect("write");
            }
            ZipEntry::Deflated(name, data) => {
                let options =
                    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
                writer.start_file(name, options).expect("start");
                writer.write_all(data).expect("write");
            }
            ZipEntry::Symlink(name, target) => {
                writer
                    .add_symlink(name, target, SimpleFileOptions::default())
                    .expect("symlink");
            }
            ZipEntry::Dir(name) => {
                writer
                    .add_directory(name, SimpleFileOptions::default())
                    .expect("dir");
            }
        }
    }
    writer.finish().expect("finish").into_inner()
}

/// How a fixture tar entry is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TarEntry<'a> {
    /// A regular file.
    File(&'a str, &'a [u8]),
    /// A symbolic link to a target.
    Symlink(&'a str, &'a str),
    /// A hard link to an earlier entry.
    HardLink(&'a str, &'a str),
    /// A FIFO.
    Fifo(&'a str),
    /// A folder.
    Dir(&'a str),
}

/// A tar archive of `entries`. Names are written into the header bytes as
/// given, so names a tar tool would refuse (`../x`, `/etc/x`) can be built.
#[must_use]
pub fn tar_of(entries: &[TarEntry<'_>]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for entry in entries {
        let mut header = tar::Header::new_gnu();
        let (name, data, kind, link): (&str, &[u8], tar::EntryType, Option<&str>) = match *entry {
            TarEntry::File(name, data) => (name, data, tar::EntryType::Regular, None),
            TarEntry::Symlink(name, target) => (name, b"", tar::EntryType::Symlink, Some(target)),
            TarEntry::HardLink(name, target) => (name, b"", tar::EntryType::Link, Some(target)),
            TarEntry::Fifo(name) => (name, b"", tar::EntryType::Fifo, None),
            TarEntry::Dir(name) => (name, b"", tar::EntryType::Directory, None),
        };
        let bytes = name.as_bytes();
        let field = &mut header.as_old_mut().name;
        let len = bytes.len().min(field.len());
        field[..len].copy_from_slice(&bytes[..len]);
        if let Some(link) = link {
            let bytes = link.as_bytes();
            let field = &mut header.as_old_mut().linkname;
            let len = bytes.len().min(field.len());
            field[..len].copy_from_slice(&bytes[..len]);
        }
        header.set_entry_type(kind);
        header.set_mode(0o644);
        header.set_size(u64::try_from(data.len()).expect("size"));
        header.set_cksum();
        builder.append(&header, data).expect("append");
    }
    builder.into_inner().expect("finish")
}

/// `data` inside one gzip stream.
#[must_use]
pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(data).expect("gzip");
    encoder.finish().expect("gzip finish")
}
