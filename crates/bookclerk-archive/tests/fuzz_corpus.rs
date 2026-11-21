//! Replays the checked-in `archive_extract` and `archive_report` fuzz
//! corpora in PR CI (no libFuzzer required), so an archive that once broke
//! the walk, or a worker report that once fooled the host, stays handled
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! `UPDATE_FUZZ_SEEDS=1` rewrites the generated seeds; without it the test
//! fails when a generated seed no longer matches its file.
// Test code: a failed assertion or a missing corpus file is the test failing.
#![allow(clippy::missing_panics_doc, clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use bookclerk_archive::fixtures::{gzip, tar_of, zip_of, TarEntry, ZipEntry};

#[path = "../../../fuzz/oracles/archive_extract.rs"]
mod archive_extract_oracle;

#[path = "../../../fuzz/oracles/archive_report.rs"]
mod archive_report_oracle;

/// `fuzz/corpus/<target>`.
fn corpus_dir(target: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus")
        .join(target)
}

/// Regular files directly under `fuzz/corpus/<target>/`, canonicalized and
/// confined to that directory.
fn corpus_files(target: &str) -> Vec<PathBuf> {
    let dir = corpus_dir(target)
        .canonicalize()
        .expect("canonicalize corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fuzz corpus")
        .map(|entry| entry.expect("entry").path())
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty corpus for {target}");
    files
}

/// Writes (with `UPDATE_FUZZ_SEEDS`) or checks one generated seed.
fn seed(target: &str, name: &str, bytes: &[u8]) {
    let path = corpus_dir(target).join(name);
    if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
        std::fs::write(&path, bytes).expect("write seed");
    }
    assert_eq!(
        std::fs::read(&path).expect("read seed"),
        bytes,
        "{} is stale; rerun with UPDATE_FUZZ_SEEDS=1",
        path.display()
    );
}

/// The generated `archive_extract` seeds: honest archives of each format,
/// hostile names and links, a ratio bomb, an overlap and cut archives.
fn extract_seeds() -> Vec<(&'static str, Vec<u8>)> {
    let book = zip_of(&[
        ZipEntry::Dir("Book/"),
        ZipEntry::Stored("Book/01.mp3", b"ID3 audio one"),
        ZipEntry::Deflated("Book/desc.txt", b"A description of the book."),
    ]);
    let hostile = zip_of(&[
        ZipEntry::Stored("../up.mp3", b"x"),
        ZipEntry::Stored("/abs.mp3", b"x"),
        ZipEntry::Stored("C:\\win.mp3", b"x"),
        ZipEntry::Symlink("link", "/etc/passwd"),
        ZipEntry::Stored("CON.txt", b"x"),
        ZipEntry::Stored("a.mp3", b"x"),
        ZipEntry::Stored("A.MP3", b"x"),
        ZipEntry::Stored("a.mp3/inner", b"x"),
    ]);
    let bomb = zip_of(&[ZipEntry::Deflated("zeros", &vec![0u8; 256 * 1024])]);
    let nested = zip_of(&[ZipEntry::Stored("inner.zip", &book)]);
    let tar = tar_of(&[
        TarEntry::Dir("Book/"),
        TarEntry::File("Book/01.flac", b"fLaC audio"),
        TarEntry::File("../up", b"x"),
        TarEntry::Symlink("Book/s", "/etc/passwd"),
        TarEntry::HardLink("Book/h", "Book/01.flac"),
        TarEntry::Fifo("Book/f"),
    ]);
    // Two central directory records pointing at one local header: rename
    // the second entry's local name so the archive parses, then point its
    // central record at the first entry's header.
    let overlap = {
        let mut bytes = zip_of(&[
            ZipEntry::Stored("one.mp3", b"same data"),
            ZipEntry::Stored("two.mp3", b"same data"),
        ]);
        let central = bytes
            .windows(4)
            .rposition(|w| w == b"PK\x01\x02")
            .expect("second central record");
        // Local header offset field sits 42 bytes into a central record.
        bytes[central + 42..central + 46].copy_from_slice(&0u32.to_le_bytes());
        bytes
    };
    vec![
        ("book.zip", book.clone()),
        ("hostile.zip", hostile),
        ("bomb.zip", bomb),
        ("nested.zip", nested),
        ("overlap.zip", overlap),
        ("book.tar", tar.clone()),
        ("book.tar.gz", gzip(&tar)),
        ("cut.zip", book[..book.len() / 2].to_vec()),
        ("cut.tar.gz", gzip(&tar)[..40].to_vec()),
        ("gzip_not_tar.gz", gzip(b"plain text, not a tar archive")),
        ("empty.zip", zip_of(&[])),
    ]
}

/// The generated `archive_report` seeds: a good report and reports that
/// lie about paths, sizes, totals and clashes.
fn report_seeds() -> Vec<(&'static str, Vec<u8>)> {
    let good = br#"{"format":"zip","written":[{"path":"Book/01.mp3","bytes":10},{"path":"Book/cover.jpg","bytes":5}],"refused":[{"name":"../x","reason":{"kind":"path","refusal":"parent_dir"}},{"name":"l","reason":{"kind":"link"}}],"stopped":null,"totalBytes":15}"#;
    let stopped = br#"{"format":"tar_gz","written":[],"refused":[],"stopped":"total_exceeded","totalBytes":0}"#;
    let escape = br#"{"format":"zip","written":[{"path":"../x","bytes":1}],"refused":[],"stopped":null,"totalBytes":1}"#;
    let backslash = br#"{"format":"zip","written":[{"path":"a\\b","bytes":1}],"refused":[],"stopped":null,"totalBytes":1}"#;
    let clash = br#"{"format":"zip","written":[{"path":"a","bytes":1},{"path":"A","bytes":1}],"refused":[],"stopped":null,"totalBytes":2}"#;
    let total = br#"{"format":"tar","written":[{"path":"a","bytes":18446744073709551615},{"path":"b","bytes":1}],"refused":[],"stopped":null,"totalBytes":0}"#;
    let extra =
        br#"{"format":"zip","written":[],"refused":[],"stopped":null,"totalBytes":0,"more":1}"#;
    vec![
        ("good.json", good.to_vec()),
        ("stopped.json", stopped.to_vec()),
        ("escape.json", escape.to_vec()),
        ("backslash.json", backslash.to_vec()),
        ("clash.json", clash.to_vec()),
        ("total.json", total.to_vec()),
        ("extra_field.json", extra.to_vec()),
    ]
}

#[test]
fn generated_seeds_are_current() {
    for (name, bytes) in extract_seeds() {
        seed("archive_extract", name, &bytes);
    }
    for (name, bytes) in report_seeds() {
        seed("archive_report", name, &bytes);
    }
}

#[test]
fn fuzz_corpus_archive_extract_holds_invariants() {
    for path in corpus_files("archive_extract") {
        archive_extract_oracle::check(&std::fs::read(&path).expect("read"));
    }
}

#[test]
fn fuzz_corpus_archive_report_holds_invariants() {
    for path in corpus_files("archive_report") {
        archive_report_oracle::check(&std::fs::read(&path).expect("read"));
    }
}

#[test]
fn seeds_reach_the_refusals_they_were_built_for() {
    use bookclerk_archive::{extract, MemorySink, RefusalReason};
    let walk = |name: &str| {
        let bytes = std::fs::read(corpus_dir("archive_extract").join(name)).expect("seed");
        let mut sink = MemorySink::new();
        extract(
            std::io::Cursor::new(bytes),
            &archive_extract_oracle::LIMITS,
            &mut sink,
        )
        .expect("walk")
    };
    let has = |name: &str, reason: RefusalReason| {
        assert!(
            walk(name)
                .refused
                .iter()
                .any(|entry| entry.reason == reason),
            "{name} lacks {reason:?}"
        );
    };
    has("overlap.zip", RefusalReason::Overlap);
    has("bomb.zip", RefusalReason::RatioExceeded);
    has("nested.zip", RefusalReason::NestedArchive);
    has("hostile.zip", RefusalReason::Link);
    has("book.tar.gz", RefusalReason::Special);
}
