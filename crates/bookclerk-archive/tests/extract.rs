//! End-to-end walks over built archives: what is written, what is refused,
//! and when a walk stops.
// Test code: a failed assertion is the test failing.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::io::Cursor;

use bookclerk_archive::fixtures::{gzip, tar_of, zip_of, TarEntry, ZipEntry};
use bookclerk_archive::{
    extract, ArchiveFormat, DirSink, EntryPath, ExtractError, Limits, MemorySink, NameClash,
    PathRefusal, RefusalReason, StopReason,
};

fn walk(bytes: &[u8], limits: &Limits) -> (bookclerk_archive::ExtractReport, MemorySink) {
    let mut sink = MemorySink::new();
    let report = extract(Cursor::new(bytes), limits, &mut sink).expect("walk");
    report.validate(limits).expect("valid report");
    (report, sink)
}

fn reasons(report: &bookclerk_archive::ExtractReport) -> Vec<RefusalReason> {
    report.refused.iter().map(|entry| entry.reason).collect()
}

#[test]
fn a_zip_book_comes_out_whole() {
    let audio = vec![7u8; 300_000];
    let bytes = zip_of(&[
        ZipEntry::Dir("Book/"),
        ZipEntry::Stored("Book/01.mp3", &audio),
        ZipEntry::Deflated("Book/cover.jpg", b"jpeg bytes"),
        ZipEntry::Deflated("Book\\desc.txt", b"A description."),
    ]);
    let (report, sink) = walk(&bytes, &Limits::UPLOAD_DEFAULTS);
    assert_eq!(report.format, ArchiveFormat::Zip);
    assert_eq!(report.stopped, None);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    let files = sink.files();
    assert_eq!(files.len(), 3);
    assert_eq!(files[&EntryPath::admit("Book/01.mp3").unwrap()], audio);
    assert_eq!(
        files[&EntryPath::admit("Book/desc.txt").unwrap()],
        b"A description."
    );
    assert_eq!(report.total_bytes, 300_000 + 10 + 14);
}

#[test]
fn hostile_zip_entries_are_refused_and_the_rest_kept() {
    let inner = zip_of(&[ZipEntry::Stored("x", b"x")]);
    let bytes = zip_of(&[
        ZipEntry::Stored("../escape.mp3", b"no"),
        ZipEntry::Stored("/abs.mp3", b"no"),
        ZipEntry::Stored("C:/win.mp3", b"no"),
        ZipEntry::Symlink("link", "/etc/passwd"),
        ZipEntry::Stored("__MACOSX/._a.mp3", b"no"),
        ZipEntry::Stored("nested.zip", &inner),
        ZipEntry::Stored("ok.mp3", b"yes"),
        ZipEntry::Stored("OK.MP3", b"dup"),
    ]);
    let (report, sink) = walk(&bytes, &Limits::UPLOAD_DEFAULTS);
    assert_eq!(
        reasons(&report),
        vec![
            RefusalReason::Path {
                refusal: PathRefusal::ParentDir
            },
            RefusalReason::Path {
                refusal: PathRefusal::Absolute
            },
            RefusalReason::Path {
                refusal: PathRefusal::Colon
            },
            RefusalReason::Link,
            RefusalReason::SystemFile,
            RefusalReason::NestedArchive,
            RefusalReason::Clash {
                clash: NameClash::Duplicate
            },
        ]
    );
    assert_eq!(sink.files().len(), 1);
    assert_eq!(report.stopped, None);
}

#[test]
fn a_deflate_bomb_entry_is_refused_by_ratio() {
    let zeros = vec![0u8; 8 * 1024 * 1024];
    let bytes = zip_of(&[
        ZipEntry::Deflated("bomb.bin", &zeros),
        ZipEntry::Stored("after.mp3", b"kept"),
    ]);
    let (report, sink) = walk(&bytes, &Limits::UPLOAD_DEFAULTS);
    assert_eq!(reasons(&report), vec![RefusalReason::RatioExceeded]);
    assert_eq!(sink.files().len(), 1);
}

#[test]
fn entry_and_total_caps_hold() {
    let limits = Limits {
        max_entry_bytes: 100,
        max_total_bytes: 150,
        ..Limits::UPLOAD_DEFAULTS
    };
    let bytes = zip_of(&[
        ZipEntry::Stored("big", &[1u8; 101]),
        ZipEntry::Stored("a", &[1u8; 100]),
        ZipEntry::Stored("b", &[1u8; 100]),
    ]);
    let (report, sink) = walk(&bytes, &limits);
    assert_eq!(reasons(&report), vec![RefusalReason::EntryTooLarge]);
    assert_eq!(report.stopped, Some(StopReason::TotalExceeded));
    assert_eq!(sink.files().len(), 1);
}

#[test]
fn too_many_entries_stop_before_writing() {
    let limits = Limits {
        max_entries: 2,
        ..Limits::UPLOAD_DEFAULTS
    };
    let bytes = zip_of(&[
        ZipEntry::Stored("a", b"a"),
        ZipEntry::Stored("b", b"b"),
        ZipEntry::Stored("c", b"c"),
    ]);
    let (report, sink) = walk(&bytes, &limits);
    assert_eq!(report.stopped, Some(StopReason::TooManyEntries));
    assert!(sink.files().is_empty());
}

#[test]
fn tar_and_tar_gz_walk_alike() {
    let bytes = tar_of(&[
        TarEntry::Dir("Book/"),
        TarEntry::File("Book/01.flac", b"fLaC audio"),
        TarEntry::File("../up.flac", b"no"),
        TarEntry::Symlink("Book/link", "/etc/passwd"),
        TarEntry::HardLink("Book/hard", "Book/01.flac"),
        TarEntry::Fifo("Book/fifo"),
        TarEntry::File("Book/02.flac", b"fLaC more"),
    ]);
    let expect = vec![
        RefusalReason::Path {
            refusal: PathRefusal::ParentDir,
        },
        RefusalReason::Link,
        RefusalReason::Link,
        RefusalReason::Special,
    ];
    let (plain, sink) = walk(&bytes, &Limits::UPLOAD_DEFAULTS);
    assert_eq!(plain.format, ArchiveFormat::Tar);
    assert_eq!(reasons(&plain), expect);
    assert_eq!(sink.files().len(), 2);
    let (gz, sink) = walk(&gzip(&bytes), &Limits::UPLOAD_DEFAULTS);
    assert_eq!(gz.format, ArchiveFormat::TarGz);
    assert_eq!(reasons(&gz), expect);
    assert_eq!(sink.files().len(), 2);
    assert_eq!(gz.written, plain.written);
}

#[test]
fn a_gzip_bomb_stops_the_walk() {
    let limits = Limits {
        max_total_bytes: 4 * 1024 * 1024,
        ratio_floor_bytes: 1024,
        ..Limits::UPLOAD_DEFAULTS
    };
    let zeros = vec![0u8; 16 * 1024 * 1024];
    let bytes = gzip(&tar_of(&[TarEntry::File("zeros.bin", &zeros)]));
    let (report, sink) = walk(&bytes, &limits);
    assert_eq!(report.stopped, Some(StopReason::TotalExceeded));
    assert!(sink.files().is_empty());
}

#[test]
fn junk_is_not_an_archive_and_a_cut_archive_stops() {
    let mut sink = MemorySink::new();
    assert!(matches!(
        extract(
            Cursor::new(b"ID3 not an archive"),
            &Limits::default(),
            &mut sink
        ),
        Err(ExtractError::NotArchive)
    ));
    let bytes = zip_of(&[ZipEntry::Stored("a.mp3", &[3u8; 4096])]);
    let (report, _) = walk(&bytes[..bytes.len() / 2], &Limits::default());
    assert_eq!(report.stopped, Some(StopReason::Corrupt));
}

#[test]
fn dir_sink_writes_inside_its_root_only() {
    let root = tempfile::tempdir().unwrap();
    let bytes = zip_of(&[
        ZipEntry::Stored("Book/01.mp3", b"audio"),
        ZipEntry::Stored("../outside.mp3", b"no"),
    ]);
    let mut sink = DirSink::new(root.path());
    let report = extract(Cursor::new(bytes), &Limits::default(), &mut sink).unwrap();
    assert_eq!(report.written.len(), 1);
    assert_eq!(
        std::fs::read(root.path().join("Book/01.mp3")).unwrap(),
        b"audio"
    );
    assert!(!root.path().parent().unwrap().join("outside.mp3").exists());
}
