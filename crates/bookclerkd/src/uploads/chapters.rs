//! The chapters an uploaded book's files carry, as one list on the book's
//! timeline (`docs/adr/edition-chapters.md`, decision 2: "a file's chapters
//! as first imported").
//!
//! Each audio file's embedded chapters are shifted by the running time of
//! the files before it. A file with no chapters of its own starts one
//! chapter named after the file, so the list still covers the whole book
//! once any file carries chapters. A book none of whose files carries
//! chapters has no list: one chapter per file is not a chapter list a file
//! stated.

use bookclerk_library::{ChapterCheck, ChapterInput, ChapterList};
use bookclerk_media::TagRecord;

/// One audio file of an uploaded book, in play order.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FilePart<'a> {
    /// The file's path in the batch.
    pub path: &'a str,
    /// What the tag reader found; `None` when the file could not be read.
    pub tags: Option<&'a TagRecord>,
}

/// The book's chapters from its files, and the running time they were
/// measured on.
///
/// # Returns
///
/// `None` when no file carries chapters, when a file other than the last
/// states no running time (so the files after it cannot be placed), or
/// when the list does not pass the chapter checks even leniently. The
/// running time is `None` when the last file states none.
pub(crate) fn file_chapters(parts: &[FilePart<'_>]) -> Option<(ChapterList, Option<u64>)> {
    if !parts
        .iter()
        .any(|part| part.tags.is_some_and(|tags| !tags.chapters.is_empty()))
    {
        return None;
    }
    let mut chapters = Vec::new();
    let mut offset: Option<u64> = Some(0);
    for part in parts {
        let start = offset?;
        let own = part
            .tags
            .map(|tags| tags.chapters.as_slice())
            .unwrap_or_default();
        if own.is_empty() {
            chapters.push(ChapterInput {
                title: file_title(part.path),
                start_ms: start,
                depth: 0,
            });
        } else {
            chapters.extend(own.iter().map(|chapter| ChapterInput {
                title: chapter.title.clone(),
                start_ms: start.saturating_add(chapter.start_ms),
                depth: 0,
            }));
        }
        offset = part
            .tags
            .and_then(|tags| tags.duration)
            .map(|duration| start.saturating_add(duration.ms));
    }
    let list = ChapterList::new(chapters, ChapterCheck::Lenient).ok()?;
    Some((list, offset))
}

/// The name a file without chapters gives its one chapter: its file name
/// without the folder or the extension.
fn file_title(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    if stem.is_empty() {
        name.to_string()
    } else {
        stem.to_string()
    }
}

#[cfg(test)]
mod tests {
    use bookclerk_tags::fixtures::{tagged_m4b, tagged_mp3};
    use bookclerk_tags::read_all;

    use super::*;

    /// The tags of `bytes` as the reader finds them.
    fn tags(bytes: &[u8]) -> TagRecord {
        read_all(bytes).expect("a fixture reads")
    }

    #[test]
    fn a_single_file_gives_its_own_chapters_and_running_time() {
        let record = tags(&tagged_m4b());
        assert!(!record.chapters.is_empty(), "the fixture has chapters");
        let (list, runtime) = file_chapters(&[FilePart {
            path: "Book/Book.m4b",
            tags: Some(&record),
        }])
        .expect("a list");
        assert_eq!(list.chapters().len(), record.chapters.len());
        assert_eq!(list.chapters()[0].title, record.chapters[0].title);
        assert_eq!(runtime, record.duration.map(|d| d.ms));
    }

    #[test]
    fn later_files_are_shifted_and_a_file_without_chapters_is_named() {
        let with = tags(&tagged_m4b());
        let mut without = tags(&tagged_mp3());
        without.chapters.clear();
        let first_ms = with.duration.expect("the fixture states a duration").ms;
        let (list, _) = file_chapters(&[
            FilePart {
                path: "Book/01 Opening.m4b",
                tags: Some(&with),
            },
            FilePart {
                path: "Book/02 Ending.mp3",
                tags: Some(&without),
            },
        ])
        .expect("a list");
        let last = list.chapters().last().expect("chapters");
        assert_eq!(last.title, "02 Ending");
        assert_eq!(last.start_ms, first_ms);
    }

    #[test]
    fn no_chapters_anywhere_or_an_unplaceable_file_gives_no_list() {
        let mut bare = tags(&tagged_mp3());
        bare.chapters.clear();
        let part = FilePart {
            path: "a.mp3",
            tags: Some(&bare),
        };
        assert!(file_chapters(&[part, part]).is_none());
        assert!(file_chapters(&[]).is_none());

        let with = tags(&tagged_m4b());
        let mut untimed = bare.clone();
        untimed.duration = None;
        let parts = [
            FilePart {
                path: "a.mp3",
                tags: Some(&untimed),
            },
            FilePart {
                path: "b.m4b",
                tags: Some(&with),
            },
        ];
        assert!(
            file_chapters(&parts).is_none(),
            "the second file has no start"
        );
        let unread = [
            FilePart {
                path: "a.m4b",
                tags: Some(&with),
            },
            FilePart {
                path: "b.mp3",
                tags: None,
            },
        ];
        let (list, runtime) = file_chapters(&unread).expect("the last file needs no length");
        assert_eq!(list.chapters().last().map(|c| c.title.as_str()), Some("b"));
        assert_eq!(runtime, None);
    }

    #[test]
    fn a_file_title_drops_the_folder_and_extension() {
        assert_eq!(file_title("A/B/03 Part.mp3"), "03 Part");
        assert_eq!(file_title("plain"), "plain");
        assert_eq!(file_title(".hidden"), ".hidden");
    }
}
