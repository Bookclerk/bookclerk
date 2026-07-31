//! Editing a faststart audiobook without moving its audio.
//!
//! A faststart file puts `moov` in front of the media, so everything added after
//! the samples are written — tags, a cover, a chapter track — would push the
//! whole book along. The remuxer reserves slack for exactly that, and these
//! cover the promise that goes with it: the media stays byte-for-byte where it
//! was, and the file does not grow by the size of its own audio.

use std::path::Path;

use bookclerk_media::{fixup_audiobook, FixupRequest};
use bookclerk_mp4::fixture::ProgressiveFixture;
use bookclerk_mp4::{
    parse_mp4, read_moov, remux_progressive, CopySamples, RemuxOptions, RESERVED_MOOV_SLACK,
};

/// Every sample payload, read back through the file's own tables.
fn payloads(path: &Path) -> Vec<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mp4 = parse_mp4(path).expect("parse");
    let mut file = std::fs::File::open(path).expect("open");
    mp4.audio
        .samples
        .iter()
        .map(|sample| {
            let mut buf = vec![0u8; sample.size as usize];
            file.seek(SeekFrom::Start(sample.offset)).expect("seek");
            file.read_exact(&mut buf).expect("read");
            buf
        })
        .collect()
}

/// A faststart M4B with recognisable payloads.
fn faststart_book(dir: &Path) -> std::path::PathBuf {
    let input = dir.join("in.m4a");
    let output = dir.join("book.m4b");
    ProgressiveFixture {
        timescale: 1000,
        sample_duration: 100,
        ..ProgressiveFixture::default()
    }
    .with_samples((0..200).map(|i| vec![(i % 251) as u8; 256]).collect())
    .write(&input)
    .expect("write fixture");
    remux_progressive(&input, &output, &RemuxOptions::default(), &mut CopySamples).expect("remux");
    output
}

fn request(input: &Path, output: &Path, cover: Option<&Path>) -> FixupRequest {
    FixupRequest {
        input: input.to_path_buf(),
        output: output.to_path_buf(),
        title: "The Long Book".into(),
        author: Some("A Writer".into()),
        narrator: Some("A Reader".into()),
        cover: cover.map(Path::to_path_buf),
        chapters: vec![
            ("Opening".into(), 0),
            ("Chapter 1".into(), 4_000),
            ("Chapter 2".into(), 12_000),
        ],
        replace_chapters: true,
        subtitle: Some("And Its Sequel".into()),
        publisher: Some("A Publisher".into()),
        year: Some("2011".into()),
        genre: Some("Fiction".into()),
        series: Some("A Series".into()),
        series_index: Some("2".into()),
        asin: Some("B00TEST123".into()),
        isbn: None,
        description: Some("A description.".into()),
        language: Some("en".into()),
        tool: None,
    }
}

#[tokio::test]
async fn tags_and_chapters_leave_the_audio_untouched() {
    // The fix-up runs in a confined worker in production; a unit test has no
    // worker binary to spawn, so run it here.
    bookclerk_media::init_pool(bookclerk_media::MediaPool::in_process()).ok();

    let dir = tempfile::tempdir().unwrap();
    let book = faststart_book(dir.path());
    let before = payloads(&book);
    let (at, _) = read_moov(&book).unwrap();
    let audio_bytes: usize = before.iter().map(Vec::len).sum();

    let tagged = dir.path().join("tagged.m4b");
    fixup_audiobook(request(&book, &tagged, None))
        .await
        .unwrap();

    assert_eq!(
        payloads(&tagged),
        before,
        "the audio must survive tagging unchanged"
    );

    let (after, _) = read_moov(&tagged).unwrap();
    assert_eq!(
        after.start, at.start,
        "moov must stay where the remuxer put it"
    );
    assert_eq!(
        after.len, at.len,
        "moov must keep the length it reserved, so the media never moves"
    );

    let tag = mp4ameta::Tag::read_from_path(&tagged).unwrap();
    assert_eq!(tag.title(), Some("The Long Book"));
    assert_eq!(tag.artist(), Some("A Writer"));
    assert_eq!(tag.composer(), Some("A Reader"));
    assert_eq!(tag.year(), Some("2011"));
    // This layout used to be refused outright by the chapter writer.
    assert_eq!(
        tag.chapter_list()
            .iter()
            .map(|chapter| chapter.title.as_str())
            .collect::<Vec<_>>(),
        ["Opening", "Chapter 1", "Chapter 2"]
    );

    // Chapter text is small and goes on the end, so growth is bounded by the
    // header rather than by the book.
    let grown = std::fs::metadata(&tagged).unwrap().len() - std::fs::metadata(&book).unwrap().len();
    assert!(
        (grown as usize) < audio_bytes,
        "the file grew by {grown} bytes, which is more than its own audio"
    );
}

/// A cover is the largest thing that gets added after the fact, and the point of
/// reserving a megabyte is that a normal one still fits.
#[tokio::test]
async fn a_cover_fits_the_reserved_slack() {
    bookclerk_media::init_pool(bookclerk_media::MediaPool::in_process()).ok();

    let dir = tempfile::tempdir().unwrap();
    let book = faststart_book(dir.path());
    let before = payloads(&book);
    let (at, _) = read_moov(&book).unwrap();

    // A 400 KiB JPEG is a typical retail audiobook cover.
    let cover = dir.path().join("cover.jpg");
    let mut jpeg = vec![0xFF, 0xD8, 0xFF];
    jpeg.resize(400 * 1024, 0x42);
    std::fs::write(&cover, &jpeg).unwrap();
    assert!(jpeg.len() < RESERVED_MOOV_SLACK, "the test cover must fit");

    let tagged = dir.path().join("tagged.m4b");
    fixup_audiobook(request(&book, &tagged, Some(&cover)))
        .await
        .unwrap();

    let (after, _) = read_moov(&tagged).unwrap();
    assert_eq!(
        after.len, at.len,
        "a cover this size must come out of the slack, not out of the media's position"
    );
    assert_eq!(payloads(&tagged), before);
    assert_eq!(
        mp4ameta::Tag::read_from_path(&tagged)
            .unwrap()
            .artwork()
            .map(|art| art.data.len()),
        Some(jpeg.len())
    );
}

/// The escape hatch has to work too: something larger than the slack rebuilds
/// the file, and the audio still comes out intact.
#[tokio::test]
async fn an_edit_larger_than_the_slack_rebuilds_the_file_correctly() {
    bookclerk_media::init_pool(bookclerk_media::MediaPool::in_process()).ok();

    let dir = tempfile::tempdir().unwrap();
    let book = faststart_book(dir.path());
    let before = payloads(&book);
    let (at, _) = read_moov(&book).unwrap();

    let cover = dir.path().join("huge.jpg");
    let mut jpeg = vec![0xFF, 0xD8, 0xFF];
    jpeg.resize(RESERVED_MOOV_SLACK * 2, 0x42);
    std::fs::write(&cover, &jpeg).unwrap();

    let tagged = dir.path().join("tagged.m4b");
    fixup_audiobook(request(&book, &tagged, Some(&cover)))
        .await
        .unwrap();

    let (after, _) = read_moov(&tagged).unwrap();
    assert!(
        after.len > at.len,
        "an oversized edit is expected to have grown moov"
    );
    assert_eq!(
        payloads(&tagged),
        before,
        "a rebuild must still line the tables up with the media"
    );
}
