//! Collecting entries and chapters under the record limits.

use sha2::{Digest, Sha256};

use crate::cache::{Source, Step};
use crate::{
    ByteSpan, Chapter, ChapterSource, Container, Duration, EmbeddedCover, PictureFormat,
    RegionDigest, Sha256Digest, TagEntry, TagField, TagRecord, MAX_CHAPTERS,
    MAX_CHAPTER_TITLE_BYTES, MAX_ENTRIES, MAX_KEY_BYTES, MAX_VALUE_BYTES, REGION_HASH_VERSION,
};

/// `text` cut to at most `max` bytes at a character boundary.
pub(crate) fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Text from bytes that should be UTF-8, with bad sequences replaced.
pub(crate) fn utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Text from UTF-16 big-endian code units, with bad pairs replaced.
pub(crate) fn utf16be(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Text from ISO-8859-1 bytes.
pub(crate) fn latin1(bytes: &[u8]) -> String {
    bytes.iter().copied().map(char::from).collect()
}

/// Builds a [`TagRecord`] while enforcing the limits.
#[derive(Debug)]
pub(crate) struct RecordBuilder {
    /// The container being read.
    container: Container,
    /// Entries so far.
    entries: Vec<TagEntry>,
    /// Chapters so far, in file order until [`Self::finish`] sorts them.
    chapters: Vec<Chapter>,
    /// Where the chapters came from.
    chapter_source: Option<ChapterSource>,
    /// The running time.
    pub(crate) duration: Option<Duration>,
    /// The cover, once one is found.
    cover: Option<EmbeddedCover>,
    /// The metadata region, as spans in the order they were added.
    region: Vec<ByteSpan>,
    /// Whether something was cut to fit.
    truncated: bool,
}

impl RecordBuilder {
    /// An empty record for `container`.
    pub(crate) fn new(container: Container) -> Self {
        Self {
            container,
            entries: Vec::new(),
            chapters: Vec::new(),
            chapter_source: None,
            duration: None,
            cover: None,
            region: Vec::new(),
            truncated: false,
        }
    }

    /// Adds one value of `field`.
    ///
    /// NUL characters end the value (C-style strings) and surrounding
    /// whitespace is kept. An empty value is skipped; a key over
    /// [`MAX_KEY_BYTES`] drops the entry; a long value is cut.
    pub(crate) fn entry(&mut self, field: TagField, value: &str) {
        let value = value.split('\0').next().unwrap_or_default();
        if value.is_empty() {
            return;
        }
        let key_ok = match &field {
            TagField::Freeform { namespace, name } => {
                !name.is_empty()
                    && name.len() <= MAX_KEY_BYTES
                    && !name.contains('\0')
                    && namespace.as_ref().is_none_or(|ns| {
                        !ns.is_empty() && ns.len() <= MAX_KEY_BYTES && !ns.contains('\0')
                    })
            }
            TagField::Native { id } => {
                !id.is_empty() && id.len() <= MAX_KEY_BYTES && !id.contains('\0')
            }
            TagField::Title
            | TagField::Subtitle
            | TagField::Album
            | TagField::Artist
            | TagField::AlbumArtist
            | TagField::Composer
            | TagField::Genre
            | TagField::Date
            | TagField::Comment
            | TagField::Description
            | TagField::LongDescription
            | TagField::Publisher
            | TagField::Copyright
            | TagField::Language
            | TagField::Track
            | TagField::Disc
            | TagField::Grouping => true,
        };
        if !key_ok || self.entries.len() >= MAX_ENTRIES {
            self.truncated = true;
            return;
        }
        let kept = cut(value, MAX_VALUE_BYTES);
        if kept.len() < value.len() {
            self.truncated = true;
        }
        self.entries.push(TagEntry {
            field,
            value: kept.to_string(),
        });
    }

    /// Adds every NUL-separated value in `value` (ID3v2.4 multi-value text).
    pub(crate) fn entries(&mut self, field: &TagField, value: &str) {
        for part in value.split('\0') {
            self.entry(field.clone(), part);
        }
    }

    /// Adds one chapter; past [`MAX_CHAPTERS`] it is dropped.
    pub(crate) fn chapter(&mut self, start_ms: u64, title: &str) {
        if self.chapters.len() >= MAX_CHAPTERS {
            self.truncated = true;
            return;
        }
        let title = title.split('\0').next().unwrap_or_default();
        let kept = cut(title, MAX_CHAPTER_TITLE_BYTES);
        if kept.len() < title.len() {
            self.truncated = true;
        }
        self.chapters.push(Chapter {
            start_ms: start_ms.min(crate::MAX_DURATION_MS),
            title: kept.to_string(),
        });
    }

    /// Records where the chapters came from (call after adding them).
    pub(crate) fn chapter_source(&mut self, source: ChapterSource) {
        if !self.chapters.is_empty() {
            self.chapter_source = Some(source);
        }
    }

    /// Whether any chapters were found.
    pub(crate) fn has_chapters(&self) -> bool {
        !self.chapters.is_empty()
    }

    /// Notes that something was cut to fit the limits.
    pub(crate) fn mark_truncated(&mut self) {
        self.truncated = true;
    }

    /// Sets the cover unless one is already set.
    ///
    /// # Arguments
    ///
    /// * `picture` - The picture bytes.
    /// * `span` - Where they lie in the file, when stored as is.
    pub(crate) fn cover(&mut self, picture: &[u8], span: Option<ByteSpan>) {
        if self.cover.is_some() || picture.is_empty() {
            return;
        }
        let Ok(len) = u64::try_from(picture.len()) else {
            return;
        };
        self.cover = Some(EmbeddedCover {
            format: PictureFormat::sniff(picture),
            len,
            sha256: Sha256Digest(Sha256::digest(picture).into()),
            span: span.filter(|span| span.len == len),
        });
    }

    /// Adds a span to the metadata region.
    pub(crate) fn region(&mut self, span: ByteSpan) {
        if span.len > 0 {
            self.region.push(span);
        }
    }

    /// Hashes the region and returns the record.
    ///
    /// # Errors
    ///
    /// [`crate::cache::Miss::Need`] if a region span is not cached (parsers
    /// read every span they add, so this does not happen in practice).
    pub(crate) fn finish(mut self, src: &Source<'_>) -> Step<TagRecord> {
        self.region.sort();
        self.region.dedup();
        let mut hasher = Sha256::new();
        let mut len = 0u64;
        let mut covered_to = 0u64;
        for span in &self.region {
            // Overlapping spans (a box inside one already added) count once.
            let start = span.offset.max(covered_to);
            let Some(end) = span.end() else { continue };
            if start >= end {
                continue;
            }
            let bytes = src.bytes(start, end - start, "region")?;
            hasher.update((end - start).to_be_bytes());
            hasher.update(bytes);
            len = len.saturating_add(end - start);
            covered_to = end;
        }
        // Stable by start; equal starts keep file order.
        self.chapters.sort_by_key(|chapter| chapter.start_ms);
        if self.chapters.is_empty() {
            self.chapter_source = None;
        }
        Ok(TagRecord {
            container: self.container,
            entries: self.entries,
            chapters: self.chapters,
            chapter_source: self.chapter_source,
            duration: self
                .duration
                .filter(|duration| duration.ms <= crate::MAX_DURATION_MS),
            cover: self.cover,
            region: RegionDigest {
                version: REGION_HASH_VERSION,
                sha256: Sha256Digest(hasher.finalize().into()),
                len,
            },
            truncated: self.truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_keeps_whole_characters() {
        assert_eq!(cut("héllo", 2), "h");
        assert_eq!(cut("héllo", 3), "hé");
        assert_eq!(cut("abc", 10), "abc");
    }

    #[test]
    fn entries_enforce_limits() {
        let mut builder = RecordBuilder::new(Container::Mp3);
        builder.entry(TagField::Title, "");
        builder.entry(TagField::Title, "a\0b");
        builder.entries(&TagField::Genre, "x\0y");
        builder.entry(
            TagField::Native {
                id: "k".repeat(MAX_KEY_BYTES + 1),
            },
            "v",
        );
        assert!(builder.truncated);
        assert_eq!(builder.entries.len(), 3);
        assert_eq!(builder.entries[0].value, "a");
        builder.entry(TagField::Comment, &"é".repeat(MAX_VALUE_BYTES));
        assert_eq!(builder.entries[3].value.len(), MAX_VALUE_BYTES);
    }
}
