//! Sans-IO identification for ingest batches: which files make a book,
//! what each file is, and the import listing a book's tags and names
//! support (see `docs/adr/uploads-and-ingest.md`).
//!
//! A title never decides on its own: only an ASIN or an ISBN, from the
//! tags or from a bracketed token in a name, can join an existing edition
//! (through the resolver). Everything else becomes a new local edition an
//! Administrator reviews.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use bookclerk_library::FileRole;
use bookclerk_resolve::{
    Abridgement, CreditRole, ExpressionKind, IdentifierScheme, ListingDraft, MediaKind, WorkKind,
};
use bookclerk_tags::{TagField, TagRecord};

/// The storefront key of import listings: books people uploaded, which no
/// store sells.
pub(crate) const IMPORT_PLUGIN_KEY: &str = "core.import";

/// What a member file is, by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileKind {
    /// Audio the library stores and plays.
    Audio,
    /// An EPUB.
    Epub,
    /// A PDF.
    Pdf,
    /// Cover art.
    Image,
    /// Text beside the audio: a CUE sheet, chapter list, description or
    /// metadata file. Kept with the book; not read for identification yet.
    Sidecar,
    /// Anything else, refused.
    Other,
}

impl FileKind {
    /// The kind of the file at `path`, from its extension (case ignored).
    pub(crate) fn of(path: &str) -> Self {
        let name = path.rsplit('/').next().unwrap_or(path);
        let Some((_, ext)) = name.rsplit_once('.') else {
            return Self::Other;
        };
        match ext.to_ascii_lowercase().as_str() {
            "mp3" | "m4a" | "m4b" | "mp4" | "aac" | "flac" | "ogg" | "oga" | "opus" | "wav"
            | "wma" | "aiff" | "aif" => Self::Audio,
            "epub" => Self::Epub,
            "pdf" => Self::Pdf,
            "jpg" | "jpeg" | "png" | "webp" => Self::Image,
            "cue" | "nfo" | "opf" | "txt" | "json" | "xml" | "chapters" | "lrc" | "srt" | "vtt" => {
                Self::Sidecar
            }
            _ => Self::Other,
        }
    }

    /// The `edition_files` role of a file of this kind; `first_audio` says
    /// whether it is the book's first audio file (the others are parts).
    pub(crate) fn role(self, first_audio: bool) -> FileRole {
        match self {
            Self::Audio if first_audio => FileRole::Audio,
            Self::Audio => FileRole::Part,
            Self::Epub => FileRole::Epub,
            Self::Pdf => FileRole::Pdf,
            Self::Image => FileRole::Cover,
            Self::Sidecar | Self::Other => FileRole::Sidecar,
        }
    }
}

/// One member considered for grouping: its number and path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// The member's number in the batch.
    pub member_no: u32,
    /// Its path in the batch.
    pub path: String,
}

/// The files that make one book: everything in one folder, audio in
/// natural order first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BookGroup {
    /// The folder (empty for the batch's top level, where each audio file
    /// is its own book).
    pub folder: String,
    /// Audio files, in natural order.
    pub audio: Vec<Candidate>,
    /// Ebooks, images and sidecars kept with the book.
    pub extras: Vec<(Candidate, FileKind)>,
}

/// A member grouping refused, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// The member.
    pub member_no: u32,
    /// The stable reason.
    pub reason: &'static str,
}

/// The folder of `path` (everything before its last `/`).
fn folder_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(folder, _)| folder)
}

/// Compares two names the way people number files: runs of digits by
/// value, the rest case-insensitively, so `2.mp3` sorts before `10.mp3`.
pub(crate) fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut left, mut right) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (left.peek().copied(), right.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                    let mut digits = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        digits.push(c);
                        it.next();
                    }
                    digits
                };
                let (dx, dy) = (take(&mut left), take(&mut right));
                let (tx, ty) = (dx.trim_start_matches('0'), dy.trim_start_matches('0'));
                let order = tx.len().cmp(&ty.len()).then_with(|| tx.cmp(ty));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                let order = x.to_lowercase().cmp(y.to_lowercase());
                if order != Ordering::Equal {
                    return order;
                }
                left.next();
                right.next();
            }
        }
    }
}

/// Groups members into books: one per folder that holds audio, and one
/// per audio file at the top level. A folder's extras join its book; a
/// file in a folder with no audio, or of a type the library does not
/// keep, is refused.
pub(crate) fn group(candidates: &[Candidate]) -> (Vec<BookGroup>, Vec<Refusal>) {
    let mut folders: BTreeMap<String, Vec<(Candidate, FileKind)>> = BTreeMap::new();
    let mut refused = Vec::new();
    for candidate in candidates {
        let kind = FileKind::of(&candidate.path);
        if kind == FileKind::Other {
            refused.push(Refusal {
                member_no: candidate.member_no,
                reason: "unsupported_type",
            });
            continue;
        }
        folders
            .entry(folder_of(&candidate.path).to_string())
            .or_default()
            .push((candidate.clone(), kind));
    }
    let mut groups = Vec::new();
    for (folder, files) in folders {
        let (mut audio, extras): (Vec<_>, Vec<_>) = files
            .into_iter()
            .partition(|(_, kind)| *kind == FileKind::Audio);
        audio.sort_by(|(a, _), (b, _)| natural_cmp(&a.path, &b.path));
        if audio.is_empty() {
            refused.extend(extras.into_iter().map(|(candidate, _)| Refusal {
                member_no: candidate.member_no,
                reason: "no_audio",
            }));
            continue;
        }
        if folder.is_empty() {
            // Loose files at the top: each audio file is a book, and the
            // extras there belong to none of them.
            for (candidate, _) in audio {
                groups.push(BookGroup {
                    folder: String::new(),
                    audio: vec![candidate],
                    extras: Vec::new(),
                });
            }
            refused.extend(extras.into_iter().map(|(candidate, _)| Refusal {
                member_no: candidate.member_no,
                reason: "no_audio",
            }));
            continue;
        }
        groups.push(BookGroup {
            folder,
            audio: audio.into_iter().map(|(candidate, _)| candidate).collect(),
            extras,
        });
    }
    (groups, refused)
}

/// What a book's tags and names say about it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Evidence {
    /// The book's title.
    pub title: String,
    /// Its subtitle.
    pub subtitle: Option<String>,
    /// Authors, in billing order.
    pub authors: Vec<String>,
    /// Narrators, in billing order.
    pub narrators: Vec<String>,
    /// An Audible ASIN.
    pub asin: Option<String>,
    /// An ISBN-13 (or ISBN-10) as digits.
    pub isbn: Option<String>,
    /// Language as tagged.
    pub language: Option<String>,
    /// Release date, as RFC 3339.
    pub published_at: Option<String>,
    /// Running time in whole minutes, when one file carries the book.
    pub duration_minutes: Option<i64>,
    /// Series name.
    pub series: Option<String>,
    /// Position in the series.
    pub series_position: Option<String>,
    /// Genre as tagged.
    pub genre: Option<String>,
}

impl Evidence {
    /// True when a strong or weak identifier can name an edition.
    pub(crate) fn identified(&self) -> bool {
        self.asin.is_some() || self.isbn.is_some()
    }
}

/// An Audible ASIN: exactly `B0` and eight more upper-case letters or
/// digits.
fn asin_of(raw: &str) -> Option<String> {
    let raw = raw.trim();
    (raw.len() == 10
        && raw.starts_with("B0")
        && raw
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase()))
    .then(|| raw.to_string())
}

/// The digits of an ISBN-13 (or ISBN-10, whose last digit may be `X`),
/// with hyphens and spaces dropped, when the checksum holds.
pub(crate) fn isbn_of(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| *c != '-' && *c != ' ')
        .collect();
    let bytes = cleaned.as_bytes();
    match bytes.len() {
        13 if bytes.iter().all(u8::is_ascii_digit) => {
            let sum: u32 = bytes
                .iter()
                .enumerate()
                .map(|(i, b)| u32::from(b - b'0') * if i % 2 == 0 { 1 } else { 3 })
                .sum();
            sum.is_multiple_of(10).then_some(cleaned)
        }
        10 if bytes[..9].iter().all(u8::is_ascii_digit)
            && (bytes[9].is_ascii_digit() || bytes[9] == b'X' || bytes[9] == b'x') =>
        {
            let sum: u32 = bytes
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    let value = if *b == b'X' || *b == b'x' {
                        10
                    } else {
                        u32::from(b - b'0')
                    };
                    value * (10 - u32::try_from(i).unwrap_or(0))
                })
                .sum();
            sum.is_multiple_of(11).then(|| cleaned.to_ascii_uppercase())
        }
        _ => None,
    }
}

/// Bracketed tokens in a name (`Title [B0ABCDEFGH]`): the only place a
/// file or folder name may carry an identifier, so ordinary words are
/// never read as one.
fn bracketed(name: &str) -> impl Iterator<Item = &str> {
    name.split('[')
        .skip(1)
        .filter_map(|rest| rest.split_once(']').map(|(token, _)| token))
}

/// Splits a tagged list of names (`A; B`, `A, B`, `A & B`).
fn names(raw: &str) -> Vec<String> {
    raw.split([';', ','])
        .flat_map(|part| part.split(" & "))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// A tagged date as RFC 3339: `YYYY`, `YYYY-MM` or `YYYY-MM-DD`, at
/// midnight UTC; anything else is dropped.
fn date_of(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    let mut parts = raw.get(..10.min(raw.len()))?.split('-');
    let year = parts.next().filter(|y| digits(y, 4))?;
    let month = parts
        .next()
        .map_or(Some("01"), |m| digits(m, 2).then_some(m))?;
    let day = parts
        .next()
        .map_or(Some("01"), |d| digits(d, 2).then_some(d))?;
    let date = format!("{year}-{month}-{day}T00:00:00Z");
    chrono::DateTime::parse_from_rfc3339(&date).ok()?;
    Some(date)
}

/// The last folder of `folder`, or the file stem of `file`, as a fallback
/// title.
fn name_title(folder: &str, file: &str) -> String {
    let base = if folder.is_empty() {
        let name = file.rsplit('/').next().unwrap_or(file);
        name.rsplit_once('.').map_or(name, |(stem, _)| stem)
    } else {
        folder.rsplit('/').next().unwrap_or(folder)
    };
    // Drop a bracketed identifier from the title.
    let cut = base.find(" [").unwrap_or(base.len());
    base[..cut].trim().to_string()
}

/// Reads a book's evidence from its first audio file's tags (when the
/// worker could read them) and from the names of its folder and files.
pub(crate) fn evidence(group: &BookGroup, tags: Option<&TagRecord>, files: usize) -> Evidence {
    let mut out = Evidence::default();
    let first = group.audio.first().map_or("", |c| c.path.as_str());
    let mut title = None;
    let mut track_title = None;
    let mut album_artist = None;
    let mut artist = None;
    if let Some(record) = tags {
        for entry in &record.entries {
            let value = entry.value.trim();
            if value.is_empty() {
                continue;
            }
            match &entry.field {
                TagField::Album => title = title.or_else(|| Some(value.to_string())),
                TagField::Title => track_title = track_title.or_else(|| Some(value.to_string())),
                TagField::Subtitle => {
                    out.subtitle = out.subtitle.take().or_else(|| Some(value.to_string()))
                }
                TagField::AlbumArtist => {
                    album_artist = album_artist.or_else(|| Some(value.to_string()))
                }
                TagField::Artist => artist = artist.or_else(|| Some(value.to_string())),
                TagField::Composer => {
                    if out.narrators.is_empty() {
                        out.narrators = names(value);
                    }
                }
                TagField::Language => {
                    out.language = out.language.take().or_else(|| Some(value.to_string()))
                }
                TagField::Date => {
                    out.published_at = out.published_at.take().or_else(|| date_of(value))
                }
                TagField::Genre => out.genre = out.genre.take().or_else(|| Some(value.to_string())),
                TagField::Freeform { name, .. } => match name.to_ascii_uppercase().as_str() {
                    "ASIN" | "AUDIBLE_ASIN" => {
                        out.asin = out.asin.take().or_else(|| asin_of(value))
                    }
                    "ISBN" => out.isbn = out.isbn.take().or_else(|| isbn_of(value)),
                    "SERIES" | "MVNM" => {
                        out.series = out.series.take().or_else(|| Some(value.to_string()))
                    }
                    "SERIES-PART" | "PART" | "MVIN" => {
                        out.series_position = out
                            .series_position
                            .take()
                            .or_else(|| Some(value.to_string()));
                    }
                    "NARRATOR" | "NARRATEDBY" if out.narrators.is_empty() => {
                        out.narrators = names(value);
                    }
                    _ => {}
                },
                TagField::Grouping
                | TagField::Comment
                | TagField::Description
                | TagField::LongDescription
                | TagField::Publisher
                | TagField::Copyright
                | TagField::Track
                | TagField::Disc
                | TagField::Native { .. } => {}
            }
        }
        if files == 1 {
            out.duration_minutes = record
                .duration
                .and_then(|d| i64::try_from(d.ms / 60_000).ok());
        }
    }
    // Names carry identifiers only in brackets, folder first.
    for name in [group.folder.as_str(), first] {
        for token in bracketed(name) {
            if out.asin.is_none() {
                out.asin = asin_of(token);
            }
            if out.isbn.is_none() {
                out.isbn = isbn_of(token);
            }
        }
    }
    out.authors = album_artist
        .or(artist)
        .map(|raw| names(&raw))
        .unwrap_or_default();
    // A single file's track title is the book's; across many files it is
    // a chapter's.
    out.title = title
        .or(if files == 1 { track_title } else { None })
        .unwrap_or_else(|| name_title(&group.folder, first));
    if out.title.is_empty() {
        out.title = "Untitled upload".to_string();
    }
    out
}

/// The import listing for a book: product id `product_id`, the evidence's
/// identifiers and credits, an audio narration of a book.
pub(crate) fn listing_draft(evidence: &Evidence, product_id: &str) -> ListingDraft {
    let mut identifiers = Vec::new();
    if let Some(asin) = &evidence.asin {
        identifiers.push((IdentifierScheme::Asin, asin.clone(), None));
    }
    if let Some(isbn) = &evidence.isbn {
        identifiers.push((IdentifierScheme::Isbn, isbn.clone(), None));
    }
    let mut credits = Vec::new();
    credits.extend(
        evidence
            .authors
            .iter()
            .map(|name| (name.clone(), CreditRole::Author, None)),
    );
    credits.extend(
        evidence
            .narrators
            .iter()
            .map(|name| (name.clone(), CreditRole::Narrator, None)),
    );
    ListingDraft {
        plugin_key: IMPORT_PLUGIN_KEY.to_string(),
        marketplace: None,
        product_id: product_id.to_string(),
        work_kind: Some(WorkKind::Book),
        expression_kind: Some(ExpressionKind::Narration),
        media_kind: Some(MediaKind::Audio),
        language: evidence.language.clone(),
        abridgement: Some(Abridgement::Unknown),
        title: evidence.title.clone(),
        subtitle: evidence.subtitle.clone(),
        published_at: evidence.published_at.clone(),
        duration_minutes: evidence.duration_minutes,
        categories: evidence.genre.clone(),
        identifiers,
        credits,
        series: evidence
            .series
            .iter()
            .map(|name| (name.clone(), evidence.series_position.clone()))
            .collect(),
        parent_product_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_tags::TagEntry;

    fn candidate(member_no: u32, path: &str) -> Candidate {
        Candidate {
            member_no,
            path: path.to_string(),
        }
    }

    #[test]
    fn files_sort_the_way_people_number_them() {
        let mut names = vec!["Book/10.mp3", "Book/2.mp3", "Book/1.mp3", "Book/01a.mp3"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            vec!["Book/1.mp3", "Book/01a.mp3", "Book/2.mp3", "Book/10.mp3"]
        );
        assert_eq!(natural_cmp("A.mp3", "a.mp3"), "A.mp3".cmp("a.mp3"));
    }

    #[test]
    fn folders_make_books_and_loose_files_stand_alone() {
        let (groups, refused) = group(&[
            candidate(0, "Book/02.mp3"),
            candidate(1, "Book/01.mp3"),
            candidate(2, "Book/cover.jpg"),
            candidate(3, "Book/notes.exe"),
            candidate(4, "loose.m4b"),
            candidate(5, "readme.txt"),
            candidate(6, "Art/cover.png"),
        ]);
        assert_eq!(groups.len(), 2);
        let book = groups.iter().find(|g| g.folder == "Book").unwrap();
        assert_eq!(
            book.audio.iter().map(|c| c.member_no).collect::<Vec<_>>(),
            vec![1, 0]
        );
        assert_eq!(book.extras.len(), 1);
        assert!(groups
            .iter()
            .any(|g| g.folder.is_empty() && g.audio[0].member_no == 4));
        let mut reasons: Vec<_> = refused.iter().map(|r| (r.member_no, r.reason)).collect();
        reasons.sort_unstable();
        assert_eq!(
            reasons,
            vec![(3, "unsupported_type"), (5, "no_audio"), (6, "no_audio")]
        );
    }

    #[test]
    fn identifiers_come_from_tags_and_bracketed_names_only() {
        assert_eq!(asin_of("B0ABCDEF12"), Some("B0ABCDEF12".into()));
        assert_eq!(asin_of("Bookshelves"), None);
        assert_eq!(asin_of("b0abcdef12"), None);
        assert_eq!(isbn_of("978-0-261-10221-7"), Some("9780261102217".into()));
        assert_eq!(isbn_of("9780261102218"), None);
        assert_eq!(isbn_of("0-261-10221-4"), Some("0261102214".into()));
        let group = BookGroup {
            folder: "Tolkien/The Hobbit [B0ABCDEF12]".into(),
            audio: vec![candidate(
                0,
                "Tolkien/The Hobbit [B0ABCDEF12]/Bookshelves 01.mp3",
            )],
            extras: Vec::new(),
        };
        let found = evidence(&group, None, 1);
        assert_eq!(found.asin.as_deref(), Some("B0ABCDEF12"));
        assert_eq!(found.title, "The Hobbit");
        let plain = BookGroup {
            folder: "Bookshelves".into(),
            audio: vec![candidate(0, "Bookshelves/9780261102217.mp3")],
            extras: Vec::new(),
        };
        let found = evidence(&plain, None, 1);
        assert!(!found.identified(), "{found:?}");
    }

    #[test]
    fn tags_fill_the_listing() {
        let entry = |field: TagField, value: &str| TagEntry {
            field,
            value: value.to_string(),
        };
        let record = TagRecord {
            entries: vec![
                entry(TagField::Title, "Chapter 1"),
                entry(TagField::Album, "The Hobbit"),
                entry(TagField::Artist, "J. R. R. Tolkien"),
                entry(TagField::Composer, "Andy Serkis"),
                entry(TagField::Date, "2020-09-24"),
                entry(
                    TagField::Freeform {
                        namespace: Some("com.apple.iTunes".into()),
                        name: "ASIN".into(),
                    },
                    "B0ABCDEF12",
                ),
            ],
            container: bookclerk_tags::Container::Mp3,
            chapters: Vec::new(),
            chapter_source: None,
            duration: None,
            codec: None,
            cover: None,
            region: bookclerk_tags::RegionDigest {
                version: bookclerk_tags::REGION_HASH_VERSION,
                sha256: bookclerk_tags::Sha256Digest([0; 32]),
                len: 0,
            },
            truncated: false,
        };
        let group = BookGroup {
            folder: "x".into(),
            audio: vec![candidate(0, "x/1.mp3"), candidate(1, "x/2.mp3")],
            extras: Vec::new(),
        };
        let found = evidence(&group, Some(&record), 2);
        assert_eq!(found.title, "The Hobbit");
        assert_eq!(found.authors, vec!["J. R. R. Tolkien"]);
        assert_eq!(found.narrators, vec!["Andy Serkis"]);
        assert_eq!(found.published_at.as_deref(), Some("2020-09-24T00:00:00Z"));
        let draft = listing_draft(&found, "p1");
        assert_eq!(draft.plugin_key, IMPORT_PLUGIN_KEY);
        assert_eq!(draft.identifiers.len(), 1);
        assert!(bookclerk_resolve::Listing::from_draft(&draft).is_ok());
    }
}
