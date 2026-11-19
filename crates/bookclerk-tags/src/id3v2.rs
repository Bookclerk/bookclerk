//! ID3v2 tags at the start of a file. The tag's own bytes are fetched in one
//! span and handed to the `id3` crate as a slice.

use std::io::Cursor;

use id3::frame::{Content, PictureType};

use crate::cache::{Miss, Source, Step};
use crate::text::RecordBuilder;
use crate::{ByteSpan, ChapterSource, TagError, TagField};

/// Container name in errors.
const NAME: &str = "ID3v2";

/// Header (and footer) length.
const HEADER_LEN: u64 = 10;

/// Reads the ID3v2 tag at offset 0, if there is one.
///
/// # Returns
///
/// The offset just past the tag (where audio starts), or 0 without a tag.
///
/// # Errors
///
/// A malformed header, or a tag body the `id3` crate cannot read at all.
pub(crate) fn read(src: &Source<'_>, builder: &mut RecordBuilder) -> Step<u64> {
    let head = src.up_to(0, HEADER_LEN, NAME)?;
    if !head.starts_with(b"ID3") || head.len() < 10 {
        return Ok(0);
    }
    let syncsafe = &head[6..10];
    if syncsafe.iter().any(|&b| b & 0x80 != 0) || !(2..=4).contains(&head[3]) {
        return Err(Miss::Fail(TagError::Malformed {
            container: NAME,
            detail: "bad tag header",
        }));
    }
    let size = syncsafe
        .iter()
        .fold(0u64, |acc, &b| (acc << 7) | u64::from(b));
    let footer = if head[3] == 4 && head[5] & 0x10 != 0 {
        HEADER_LEN
    } else {
        0
    };
    let tag_len = HEADER_LEN + size + footer;
    let raw = src.bytes(0, tag_len, NAME)?;
    builder.region(ByteSpan {
        offset: 0,
        len: tag_len,
    });

    let tag = match id3::Tag::read_from2(Cursor::new(raw)) {
        Ok(tag) => tag,
        Err(id3::Error {
            partial_tag: Some(tag),
            ..
        }) => {
            builder.mark_truncated();
            tag
        }
        Err(err) => return Err(Miss::Fail(TagError::Id3(err.description))),
    };

    let mut chapters = Vec::new();
    let mut pictures = Vec::new();
    for frame in tag.frames() {
        match frame.content() {
            Content::Text(text) => builder.entries(&text_field(frame.id()), text),
            Content::ExtendedText(text) => builder.entries(
                &TagField::Freeform {
                    namespace: None,
                    name: text.description.clone(),
                },
                &text.value,
            ),
            Content::Comment(comment) => builder.entry(TagField::Comment, &comment.text),
            Content::Link(link) => builder.entry(
                TagField::Native {
                    id: frame.id().to_string(),
                },
                link,
            ),
            Content::Lyrics(lyrics) => builder.entry(
                TagField::Native {
                    id: frame.id().to_string(),
                },
                &lyrics.text,
            ),
            Content::Picture(picture) => pictures.push(picture),
            Content::Chapter(chapter) => chapters.push(chapter),
            // Binary and structural frames carry no text the resolver uses.
            _ => {}
        }
    }

    for chapter in chapters {
        let title = chapter
            .frames
            .iter()
            .find(|frame| frame.id() == "TIT2")
            .and_then(|frame| frame.content().text())
            .unwrap_or_default();
        builder.chapter(u64::from(chapter.start_time), title);
    }
    builder.chapter_source(ChapterSource::Id3Chap);

    let cover = pictures
        .iter()
        .find(|picture| picture.picture_type == PictureType::CoverFront)
        .or_else(|| pictures.first());
    if let Some(picture) = cover {
        let span = memchr::memmem::find(raw, &picture.data).and_then(|at| {
            Some(ByteSpan {
                offset: u64::try_from(at).ok()?,
                len: u64::try_from(picture.data.len()).ok()?,
            })
        });
        builder.cover(&picture.data, span);
    }
    Ok(tag_len)
}

/// The field an ID3v2 text frame fills.
fn text_field(id: &str) -> TagField {
    match id {
        "TIT2" => TagField::Title,
        "TIT3" => TagField::Subtitle,
        "TALB" => TagField::Album,
        "TPE1" => TagField::Artist,
        "TPE2" => TagField::AlbumArtist,
        "TCOM" => TagField::Composer,
        "TCON" => TagField::Genre,
        "TDRC" | "TYER" => TagField::Date,
        "TPUB" => TagField::Publisher,
        "TCOP" => TagField::Copyright,
        "TLAN" => TagField::Language,
        "TRCK" => TagField::Track,
        "TPOS" => TagField::Disc,
        "GRP1" | "TIT1" => TagField::Grouping,
        other => TagField::Native {
            id: other.to_string(),
        },
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::fixtures::tagged_mp3;
    use crate::reader::tests::drive;
    use crate::{read_all, ChapterSource, Container, PictureFormat, TagField};

    #[test]
    fn reads_text_chapters_and_cover() {
        let file = tagged_mp3();
        let record = drive(&file).expect("read");
        assert_eq!(record, read_all(&file).expect("read all"));
        assert_eq!(record.container, Container::Mp3);
        let values = |field: TagField| -> Vec<String> {
            record
                .entries
                .iter()
                .filter(|e| e.field == field)
                .map(|e| e.value.clone())
                .collect()
        };
        assert_eq!(values(TagField::Title), ["The Book"]);
        assert_eq!(values(TagField::Composer), ["A Narrator"]);
        assert_eq!(values(TagField::Genre), ["Fantasy", "Epic"]);
        assert_eq!(
            values(TagField::Freeform {
                namespace: None,
                name: "ASIN".into()
            }),
            ["B000000001"]
        );
        assert_eq!(record.chapter_source, Some(ChapterSource::Id3Chap));
        assert_eq!(record.chapters.len(), 2);
        assert_eq!(record.chapters[1].start_ms, 1500);
        assert_eq!(record.chapters[1].title, "One");
        let cover = record.cover.expect("cover");
        assert_eq!(cover.format, PictureFormat::Jpeg);
        let span = cover.span.expect("span");
        let at = usize::try_from(span.offset).expect("offset");
        assert_eq!(&file[at..at + 4], &[0xFF, 0xD8, 0xFF, 0xE0]);
        assert!(record.duration.is_some());
    }

    #[test]
    fn region_digest_follows_the_tag_only() {
        let file = tagged_mp3();
        let base = read_all(&file).expect("read").region;
        let mut audio_changed = file.clone();
        let last = audio_changed.len() - 1;
        audio_changed[last] ^= 0x55;
        assert_eq!(read_all(&audio_changed).expect("read").region, base);
        let mut tag_changed = file;
        let at = tag_changed
            .windows(8)
            .position(|w| w == b"The Book")
            .expect("title");
        tag_changed[at] = b't';
        assert_ne!(read_all(&tag_changed).expect("read").region, base);
    }
}
