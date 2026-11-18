//! Taking the copy a library already holds instead of fetching from the
//! store.
//!
//! An install that joined a library (a branch through its link, a clerk
//! through its library card) acquires its person's books into its own
//! storage. When the library already acquired the same edition from the
//! same store, the install takes the library's files and runs them through
//! its own acquire pipeline, so the copy is converted, named, tagged and
//! recorded here exactly as a store download would be.
//!
//! The acquire asks through [`LibraryCopies`], which each kind of install
//! implements over its own connection to the library. This module decides
//! which of the copy's files are taken ([`plan_files`]), downloads them
//! into the acquire's scratch directory and reads their chapters and part
//! titles in the confined media worker ([`fetch_library_copy`]), giving the
//! pipeline the same [`PlainFetch`] a store fetch gives.
//!
//! Only audio and the cover are taken. Sidecars are written again by this
//! install's own pipeline from its own options, and the companion PDF
//! still comes from the store: the pipeline asks the store for it once the
//! copy is stored, when companion PDFs are turned on.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bookclerk_library::FileRole;
use bookclerk_media::{FileRange, MediaPool};
use bookclerk_source::{PlainAudioPart, PlainFetch};

/// Why a library's copy could not be asked for, downloaded or used. The
/// acquire logs it and fetches from the store instead.
pub type LibraryCopyError = Box<dyn std::error::Error + Send + Sync>;

/// The title an acquire wants, as its store lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibraryCopyListing<'a> {
    /// Store id as this install's books name it (the store's alias, such
    /// as `audible`).
    pub store: &'a str,
    /// Key of the plugin the title belongs to on this install.
    pub plugin_key: &'a str,
    /// Store account on this install that owns the title.
    pub account_id: &'a str,
    /// Lowercase marketplace; empty for a store without marketplaces.
    pub marketplace: &'a str,
    /// The store's product id.
    pub product_id: &'a str,
}

/// One file of a library's copy, as the library listed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryCopyFile {
    /// The file's path below the title's folder on the library. Only its
    /// extension is read, to tell what kind of file it is; nothing is ever
    /// written to this path.
    pub name: String,
    /// What the file is.
    pub role: FileRole,
    /// Exact size in bytes.
    pub size: u64,
}

/// A copy of one edition a library offered this install.
#[async_trait::async_trait]
pub trait LibraryCopy: Send + Sync {
    /// The store the library downloaded the copy from, when it knows.
    fn source_store(&self) -> Option<&str>;

    /// The copy's files, the primary audio first; a file's place is its
    /// download index.
    fn files(&self) -> &[LibraryCopyFile];

    /// Downloads file `index` to `dest`, which must end up holding exactly
    /// the file's listed size. `dest` is a path nothing else writes to, so
    /// an implementation may resume what it already holds.
    ///
    /// # Errors
    ///
    /// Returns an error when the library refuses or the transfer fails.
    async fn fetch_file(&self, index: usize, dest: &Path) -> Result<(), LibraryCopyError>;

    /// Called once the acquire stored the copy on this install, so the
    /// implementation can record where the book came from (a branch never
    /// shares a book back to the library it took it from).
    ///
    /// # Errors
    ///
    /// Returns an error when the record cannot be written.
    async fn taken(&self) -> Result<(), LibraryCopyError>;
}

/// Where an acquire asks for the copy a library holds of a title.
#[async_trait::async_trait]
pub trait LibraryCopies: Send + Sync {
    /// The copy a library this install joined offers of `listing`; `None`
    /// when no library holds one this install's person may take, when
    /// this install does not keep copies, or when no library answers.
    async fn copy_of(&self, listing: LibraryCopyListing<'_>) -> Option<Box<dyn LibraryCopy>>;
}

/// The libraries an acquire asks for a copy before it fetches from the
/// store. The default asks none, so every title comes from its store.
#[derive(Clone, Default)]
pub struct LibraryCopySource {
    /// Who is asked; `None` asks no one.
    copies: Option<Arc<dyn LibraryCopies>>,
    /// The pool that reads a downloaded file's tags; `None` uses the
    /// process pool ([`bookclerk_media::pool`]).
    media: Option<Arc<MediaPool>>,
}

impl LibraryCopySource {
    /// Asks `copies` for each title before its store.
    #[must_use]
    pub fn new(copies: Arc<dyn LibraryCopies>) -> Self {
        Self {
            copies: Some(copies),
            media: None,
        }
    }

    /// The same source, reading downloaded files with `media` instead of
    /// the process pool.
    #[must_use]
    pub fn read_by(mut self, media: Arc<MediaPool>) -> Self {
        self.media = Some(media);
        self
    }

    /// Who is asked, if anyone.
    pub(crate) fn copies(&self) -> Option<&dyn LibraryCopies> {
        self.copies.as_deref()
    }

    /// The pool that reads downloaded files.
    pub(crate) fn media(&self) -> Arc<MediaPool> {
        self.media.clone().unwrap_or_else(bookclerk_media::pool)
    }
}

impl std::fmt::Debug for LibraryCopySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.copies.is_some() {
            "LibraryCopySource(asked)"
        } else {
            "LibraryCopySource(none)"
        })
    }
}

/// True when the library downloaded its copy from `store`, the store this
/// install's title belongs to. A copy whose store the library does not
/// know, or one from another store, is not taken: the install fetches its
/// own.
#[must_use]
pub(crate) fn from_store(copy_store: Option<&str>, store: &str) -> bool {
    let store = store.trim();
    !store.is_empty() && copy_store.is_some_and(|theirs| theirs.trim().eq_ignore_ascii_case(store))
}

/// What a taken file is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Taken {
    /// Audio in an MP4 container (`.m4b`, `.m4a`, `.mp4`).
    Mp4Audio,
    /// Audio in another container the tag reader understands.
    OtherAudio,
    /// The cover image.
    Cover,
}

/// One file of a copy the acquire downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TakenFile {
    /// The file's download index.
    pub index: usize,
    /// What it is used for.
    pub kind: Taken,
    /// The extension of its scratch file, from a fixed list.
    pub ext: &'static str,
    /// Its listed size in bytes.
    pub size: u64,
}

/// The extension of `name` when it is one of `known`, compared without
/// case; the returned text is the list's own, never the library's.
fn known_ext(name: &str, known: &[&'static str]) -> Option<&'static str> {
    let ext = Path::new(name).extension()?.to_str()?;
    known
        .iter()
        .copied()
        .find(|candidate| candidate.eq_ignore_ascii_case(ext))
}

/// Audio containers a copy's audio may be in: the ones the tag reader
/// understands.
const AUDIO_EXTS: [&str; 5] = ["m4b", "m4a", "mp4", "mp3", "flac"];

/// Image types a copy's cover may be.
const COVER_EXTS: [&str; 3] = ["jpg", "jpeg", "png"];

/// The files of a copy an acquire downloads, in download order: every
/// audio file, then the first cover.
///
/// # Returns
///
/// `None` when the copy cannot be taken: it lists no audio, its first
/// file is not the primary audio, or an audio file is of a kind the tag
/// reader does not understand.
#[must_use]
pub(crate) fn plan_files(files: &[LibraryCopyFile]) -> Option<Vec<TakenFile>> {
    if files.first()?.role != FileRole::Audio {
        return None;
    }
    let mut audio = Vec::new();
    let mut cover = None;
    for (index, file) in files.iter().enumerate() {
        match file.role {
            FileRole::Audio | FileRole::Part | FileRole::Chapter => {
                let ext = known_ext(&file.name, &AUDIO_EXTS)?;
                let kind = if matches!(ext, "m4b" | "m4a" | "mp4") {
                    Taken::Mp4Audio
                } else {
                    Taken::OtherAudio
                };
                audio.push(TakenFile {
                    index,
                    kind,
                    ext,
                    size: file.size,
                });
            }
            FileRole::Cover => {
                if cover.is_none() && file.size > 0 {
                    cover = known_ext(&file.name, &COVER_EXTS).map(|ext| TakenFile {
                        index,
                        kind: Taken::Cover,
                        ext,
                        size: file.size,
                    });
                }
            }
            FileRole::Pdf | FileRole::Sidecar | FileRole::Transcript | FileRole::Epub => {}
        }
    }
    audio.extend(cover);
    Some(audio)
}

/// Downloads the files of `copy` an acquire uses into `dir` and describes
/// them as a store fetch would.
///
/// A single audio file keeps the chapters the confined tag reader finds in
/// it. Several audio files become parts in listed order, each titled by
/// its one chapter when it has exactly one (a chapter file), and are
/// packaged by the pipeline as a store's parts are; when a part holds
/// several chapters, the parts' chapters are carried on the joined
/// timeline ([`joined_chapters`]).
///
/// # Arguments
///
/// * `copy` - The copy the library offered.
/// * `dir` - A scratch directory of this acquire alone; files are written
///   as `<index>.<ext>`, never under a name the library gave.
/// * `budget` - Most bytes the files may take together.
/// * `media` - Reads each downloaded audio file's tags.
///
/// # Errors
///
/// Returns an error when the copy has no usable audio, is larger than
/// `budget`, a download fails or leaves a file of another size than
/// listed, or the tag reader cannot read an audio file. `dir` may then
/// hold partial files; the caller removes it.
pub(crate) async fn fetch_library_copy(
    copy: &dyn LibraryCopy,
    dir: &Path,
    budget: u64,
    media: &MediaPool,
) -> Result<PlainFetch, LibraryCopyError> {
    let taken = plan_files(copy.files()).ok_or("the copy lists no audio this install can read")?;
    let total = taken
        .iter()
        .fold(0u64, |sum, file| sum.saturating_add(file.size));
    if total > budget {
        return Err(format!("the copy is {total} bytes and {budget} are left for scratch").into());
    }
    tokio::fs::create_dir_all(dir).await?;

    let mut fetch = PlainFetch {
        parts: Vec::new(),
        m4b_path: None,
        cover_path: None,
        chapters: Vec::new(),
        pdf_url: None,
    };
    let audio_files = taken
        .iter()
        .filter(|file| file.kind != Taken::Cover)
        .count();
    let mut part_tags = Vec::new();
    for file in &taken {
        let path = dir.join(format!("{:03}.{}", file.index, file.ext));
        copy.fetch_file(file.index, &path).await?;
        let on_disk = tokio::fs::metadata(&path).await?.len();
        if on_disk != file.size {
            return Err(format!(
                "file {} of the copy is {on_disk} bytes, not the {} listed",
                file.index, file.size
            )
            .into());
        }
        if file.kind == Taken::Cover {
            fetch.cover_path = Some(path);
            continue;
        }
        let tags = media.read_tags(file.size, &FileRange::new(&path)).await?;
        if audio_files == 1 {
            fetch.chapters = tags
                .chapters
                .iter()
                .map(|chapter| (chapter.title.clone(), chapter.start_ms))
                .collect();
            if file.kind == Taken::Mp4Audio {
                fetch.m4b_path = Some(path);
                continue;
            }
        }
        fetch.parts.push(audio_part(path, &tags));
        part_tags.push(tags);
    }
    if audio_files > 1 {
        let parts: Vec<PartChapters> = part_tags.iter().map(PartChapters::of).collect();
        fetch.chapters = joined_chapters(&parts);
    }
    Ok(fetch)
}

/// What the tag reader found in one part of a copy with several audio
/// files, for [`joined_chapters`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PartChapters {
    /// The part's own chapters: title and start within the part, in
    /// milliseconds.
    pub chapters: Vec<(String, u64)>,
    /// The part's running time in milliseconds, when its container states
    /// it exactly; an estimate is not enough to place later chapters.
    pub exact_ms: Option<u64>,
}

impl PartChapters {
    /// The chapters and exact running time of one part's tags.
    fn of(tags: &bookclerk_media::TagRecord) -> Self {
        Self {
            chapters: tags
                .chapters
                .iter()
                .map(|chapter| (chapter.title.clone(), chapter.start_ms))
                .collect(),
            exact_ms: tags
                .duration
                .filter(|duration| {
                    duration.ms > 0
                        && match duration.precision {
                            bookclerk_tags::DurationPrecision::Exact => true,
                            bookclerk_tags::DurationPrecision::Estimated => false,
                        }
                })
                .map(|duration| duration.ms),
        }
    }
}

/// The chapters of a book held as several audio files, on the timeline of
/// the files joined in order: each part's own chapters moved by the
/// running time of the parts before it.
///
/// A part without chapters of its own is one chapter, `Part <n>`, at its
/// start. A chapter that a part before the last places at or past that
/// part's own end is left out, so the starts never go backwards.
///
/// # Returns
///
/// Empty when no part has more than one chapter (the pipeline then makes
/// one chapter per part from the part titles, timed by the audio it
/// joins), or when a part before the last states no exact running time,
/// so later chapters could not be placed.
#[must_use]
pub(crate) fn joined_chapters(parts: &[PartChapters]) -> Vec<(String, u64)> {
    if !parts.iter().any(|part| part.chapters.len() > 1) {
        return Vec::new();
    }
    let mut joined = Vec::new();
    let mut offset = 0u64;
    for (n, part) in parts.iter().enumerate() {
        if part.chapters.is_empty() {
            joined.push((format!("Part {}", n + 1), offset));
        }
        let last = n + 1 == parts.len();
        if !last && part.exact_ms.is_none() {
            return Vec::new();
        }
        for (title, start_ms) in &part.chapters {
            // A chapter a part places at or past its own end would land
            // inside a later part: it is left out, so starts never go
            // backwards.
            if part.exact_ms.is_some_and(|ms| *start_ms >= ms) && !last {
                continue;
            }
            joined.push((title.clone(), offset.saturating_add(*start_ms)));
        }
        offset = offset.saturating_add(part.exact_ms.unwrap_or(0));
    }
    joined
}

/// One downloaded audio file as a part: titled by its one chapter when it
/// has exactly one, with the running time the tag reader found.
fn audio_part(path: PathBuf, tags: &bookclerk_media::TagRecord) -> PlainAudioPart {
    let title = match tags.chapters.as_slice() {
        [only] if !only.title.trim().is_empty() => Some(only.title.clone()),
        _ => None,
    };
    PlainAudioPart {
        path,
        title,
        duration_ms: tags
            .duration
            .map(|duration| duration.ms)
            .filter(|ms| *ms > 0),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use bookclerk_tags::fixtures::{tagged_m4b, tagged_mp3};

    use super::*;

    /// A copy held in memory, counting what was asked of it.
    pub(crate) struct MemoryCopy {
        /// The store the copy says it came from.
        pub store: Option<String>,
        /// The listed files.
        pub listed: Vec<LibraryCopyFile>,
        /// Each file's bytes, by index.
        pub bytes: Vec<Vec<u8>>,
        /// Downloads asked for.
        pub fetched: Arc<AtomicUsize>,
        /// Times [`LibraryCopy::taken`] ran.
        pub taken: Arc<AtomicUsize>,
        /// An index whose download fails.
        pub failing: Option<usize>,
    }

    impl MemoryCopy {
        /// A copy from `store` of `files`: `(name, role, bytes)`.
        pub(crate) fn new(store: Option<&str>, files: Vec<(&str, FileRole, Vec<u8>)>) -> Self {
            Self {
                store: store.map(str::to_string),
                listed: files
                    .iter()
                    .map(|(name, role, bytes)| LibraryCopyFile {
                        name: (*name).to_string(),
                        role: *role,
                        size: u64::try_from(bytes.len()).unwrap(),
                    })
                    .collect(),
                bytes: files.into_iter().map(|(_, _, bytes)| bytes).collect(),
                fetched: Arc::new(AtomicUsize::new(0)),
                taken: Arc::new(AtomicUsize::new(0)),
                failing: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl LibraryCopy for MemoryCopy {
        fn source_store(&self) -> Option<&str> {
            self.store.as_deref()
        }

        fn files(&self) -> &[LibraryCopyFile] {
            &self.listed
        }

        async fn fetch_file(&self, index: usize, dest: &Path) -> Result<(), LibraryCopyError> {
            self.fetched.fetch_add(1, Ordering::SeqCst);
            if self.failing == Some(index) {
                // A cut transfer leaves part of the file behind.
                tokio::fs::write(dest, &self.bytes[index][..1]).await?;
                return Err("the transfer was cut".into());
            }
            tokio::fs::write(dest, &self.bytes[index]).await?;
            Ok(())
        }

        async fn taken(&self) -> Result<(), LibraryCopyError> {
            self.taken.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Hands out one prepared copy, once, and remembers what was asked.
    #[derive(Default)]
    pub(crate) struct OneCopy {
        /// The copy to hand out.
        pub copy: Mutex<Option<Box<dyn LibraryCopy>>>,
        /// Every listing asked for: `(store, marketplace, product id)`.
        pub asked: Mutex<Vec<(String, String, String)>>,
    }

    impl OneCopy {
        /// Hands out `copy` to the first ask.
        pub(crate) fn of(copy: MemoryCopy) -> Arc<Self> {
            Arc::new(Self {
                copy: Mutex::new(Some(Box::new(copy))),
                asked: Mutex::default(),
            })
        }
    }

    #[async_trait::async_trait]
    impl LibraryCopies for OneCopy {
        async fn copy_of(&self, listing: LibraryCopyListing<'_>) -> Option<Box<dyn LibraryCopy>> {
            self.asked.lock().unwrap().push((
                listing.store.to_string(),
                listing.marketplace.to_string(),
                listing.product_id.to_string(),
            ));
            self.copy.lock().unwrap().take()
        }
    }

    /// A listed file of `size` bytes.
    fn listed(name: &str, role: FileRole, size: u64) -> LibraryCopyFile {
        LibraryCopyFile {
            name: name.into(),
            role,
            size,
        }
    }

    #[test]
    fn a_copy_is_taken_only_from_the_titles_own_store() {
        assert!(from_store(Some("audible"), "audible"));
        assert!(from_store(Some(" Audible "), "audible"));
        assert!(!from_store(Some("libro"), "audible"));
        // A library that does not know where its copy came from, and a
        // title with no store, never match.
        assert!(!from_store(None, "audible"));
        assert!(!from_store(Some(""), ""));
    }

    #[test]
    fn audio_and_the_first_cover_are_taken_in_listed_order() {
        let files = [
            listed("Book.M4B", FileRole::Audio, 10),
            listed("Book.cue", FileRole::Sidecar, 3),
            listed("Book.pdf", FileRole::Pdf, 4),
            listed("empty.jpg", FileRole::Cover, 0),
            listed("cover.webp", FileRole::Cover, 5),
            listed("cover.JPG", FileRole::Cover, 6),
            listed("Book.txt", FileRole::Transcript, 7),
            listed("Book.epub", FileRole::Epub, 8),
        ];
        // The first cover that has bytes and is of a type the pipeline
        // embeds is the one taken.
        let taken = plan_files(&files).unwrap();
        assert_eq!(
            taken,
            [
                TakenFile {
                    index: 0,
                    kind: Taken::Mp4Audio,
                    ext: "m4b",
                    size: 10
                },
                TakenFile {
                    index: 5,
                    kind: Taken::Cover,
                    ext: "jpg",
                    size: 6
                }
            ]
        );

        let parts = [
            listed("Book-p001.mp3", FileRole::Audio, 1),
            listed("art/cover.png", FileRole::Cover, 9),
            listed("Book-p002.mp3", FileRole::Part, 2),
            listed("Book - 03.flac", FileRole::Chapter, 3),
        ];
        let taken = plan_files(&parts).unwrap();
        assert_eq!(
            taken
                .iter()
                .map(|f| (f.index, f.kind, f.ext))
                .collect::<Vec<_>>(),
            [
                (0, Taken::OtherAudio, "mp3"),
                (2, Taken::OtherAudio, "mp3"),
                (3, Taken::OtherAudio, "flac"),
                (1, Taken::Cover, "png"),
            ]
        );
    }

    #[test]
    fn a_copy_without_readable_primary_audio_is_not_taken() {
        assert_eq!(plan_files(&[]), None);
        // The primary audio is always listed first.
        assert_eq!(plan_files(&[listed("cover.jpg", FileRole::Cover, 1)]), None);
        assert_eq!(
            plan_files(&[
                listed("a.mp3", FileRole::Part, 1),
                listed("b.mp3", FileRole::Audio, 1)
            ]),
            None
        );
        // Audio of a kind the tag reader does not know, or with no
        // extension at all.
        for name in ["Book.ogg", "Book", "Book.mp3.exe", ".m4b/Book"] {
            assert_eq!(
                plan_files(&[listed(name, FileRole::Audio, 1)]),
                None,
                "{name}"
            );
        }
        assert_eq!(
            plan_files(&[
                listed("a.mp3", FileRole::Audio, 1),
                listed("b.wma", FileRole::Part, 1)
            ]),
            None
        );
    }

    #[tokio::test]
    async fn one_file_keeps_its_chapters_and_several_become_parts() {
        let media = MediaPool::in_process();
        let tmp = tempfile::tempdir().unwrap();

        // The library's names are never paths here: files land as
        // `<index>.<ext>` in the scratch directory.
        let copy = MemoryCopy::new(
            Some("audible"),
            vec![
                ("../../escape/Book.m4b", FileRole::Audio, tagged_m4b()),
                ("Book.cue", FileRole::Sidecar, b"FILE".to_vec()),
                ("cover.jpg", FileRole::Cover, b"jpeg".to_vec()),
            ],
        );
        let dir = tmp.path().join("one");
        let fetch = fetch_library_copy(&copy, &dir, u64::MAX, &media)
            .await
            .unwrap();
        assert_eq!(
            fetch.m4b_path.as_deref(),
            Some(dir.join("000.m4b").as_path())
        );
        assert!(fetch.parts.is_empty());
        assert!(
            !fetch.chapters.is_empty(),
            "the file's chapters are carried"
        );
        assert_eq!(
            fetch.cover_path.as_deref(),
            Some(dir.join("002.jpg").as_path())
        );
        assert_eq!(fetch.pdf_url, None);
        // The sidecar was not asked for.
        assert_eq!(copy.fetched.load(Ordering::SeqCst), 2);
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["000.m4b", "002.jpg"]);

        // One MP3 is a single part, with the chapters it carries.
        let copy = MemoryCopy::new(
            Some("libro"),
            vec![("Book.mp3", FileRole::Audio, tagged_mp3())],
        );
        let dir = tmp.path().join("mp3");
        let fetch = fetch_library_copy(&copy, &dir, u64::MAX, &media)
            .await
            .unwrap();
        assert_eq!(fetch.m4b_path, None);
        assert_eq!(fetch.parts.len(), 1);
        assert_eq!(fetch.parts[0].path, dir.join("000.mp3"));

        // Several files are parts in listed order.
        let copy = MemoryCopy::new(
            Some("libro"),
            vec![
                ("Book-p001.mp3", FileRole::Audio, tagged_mp3()),
                ("Book-p002.m4b", FileRole::Part, tagged_m4b()),
            ],
        );
        let dir = tmp.path().join("parts");
        let fetch = fetch_library_copy(&copy, &dir, u64::MAX, &media)
            .await
            .unwrap();
        assert_eq!(fetch.m4b_path, None);
        // The second part holds several chapters: they are carried, moved
        // past the first part's running time.
        let first_ms = fetch.parts[0].duration_ms.expect("the MP3 states its time");
        assert!(fetch.chapters.len() >= 2, "{:?}", fetch.chapters);
        assert!(fetch.chapters.windows(2).all(|pair| pair[0].1 <= pair[1].1));
        assert!(fetch.chapters.iter().any(|(_, start)| *start >= first_ms));
        assert_eq!(
            fetch
                .parts
                .iter()
                .map(|p| p.path.clone())
                .collect::<Vec<_>>(),
            [dir.join("000.mp3"), dir.join("001.m4b")]
        );
    }

    #[tokio::test]
    async fn a_copy_that_does_not_arrive_as_listed_is_refused() {
        let media = MediaPool::in_process();
        let tmp = tempfile::tempdir().unwrap();

        // Larger than the scratch budget: nothing is asked for.
        let copy = MemoryCopy::new(Some("a"), vec![("Book.m4b", FileRole::Audio, tagged_m4b())]);
        let size = copy.listed[0].size;
        let err = fetch_library_copy(&copy, &tmp.path().join("big"), size - 1, &media)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("left for scratch"), "{err}");
        assert_eq!(copy.fetched.load(Ordering::SeqCst), 0);
        assert!(!tmp.path().join("big").exists());

        // A cut transfer.
        let mut copy =
            MemoryCopy::new(Some("a"), vec![("Book.m4b", FileRole::Audio, tagged_m4b())]);
        copy.failing = Some(0);
        let err = fetch_library_copy(&copy, &tmp.path().join("cut"), u64::MAX, &media)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cut"), "{err}");

        // A file of another size than listed.
        let mut copy =
            MemoryCopy::new(Some("a"), vec![("Book.m4b", FileRole::Audio, tagged_m4b())]);
        copy.listed[0].size += 1;
        let err = fetch_library_copy(&copy, &tmp.path().join("size"), u64::MAX, &media)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not the"), "{err}");

        // Audio the confined reader cannot read.
        let copy = MemoryCopy::new(
            Some("a"),
            vec![("Book.m4b", FileRole::Audio, b"not audio".to_vec())],
        );
        assert!(
            fetch_library_copy(&copy, &tmp.path().join("bad"), u64::MAX, &media)
                .await
                .is_err()
        );

        // No audio at all.
        let copy = MemoryCopy::new(Some("a"), vec![("cover.jpg", FileRole::Cover, vec![1])]);
        let err = fetch_library_copy(&copy, &tmp.path().join("none"), u64::MAX, &media)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no audio"), "{err}");
    }

    /// A part with `chapters` (`(title, start)`) and an exact running time.
    fn part(chapters: &[(&str, u64)], exact_ms: Option<u64>) -> PartChapters {
        PartChapters {
            chapters: chapters
                .iter()
                .map(|(title, start)| ((*title).to_string(), *start))
                .collect(),
            exact_ms,
        }
    }

    #[test]
    fn chapters_inside_parts_are_carried_on_the_joined_timeline() {
        let titled = |chapters: Vec<(String, u64)>| -> Vec<(String, u64)> { chapters };
        // Two parts with chapters of their own, and one with none.
        let joined = joined_chapters(&[
            part(&[("One", 0), ("Two", 40_000)], Some(100_000)),
            part(&[], Some(50_000)),
            part(&[("Three", 5_000), ("Four", 60_000)], None),
        ]);
        assert_eq!(
            titled(joined),
            [
                ("One".to_string(), 0),
                ("Two".to_string(), 40_000),
                ("Part 2".to_string(), 100_000),
                ("Three".to_string(), 155_000),
                ("Four".to_string(), 210_000),
            ]
        );

        // Chapter files (one chapter each) and untagged parts are left to
        // the pipeline, which times one chapter per part from the audio.
        assert!(joined_chapters(&[
            part(&[("One", 0)], Some(1_000)),
            part(&[("Two", 0)], Some(1_000))
        ])
        .is_empty());
        assert!(joined_chapters(&[part(&[], None), part(&[], None)]).is_empty());
        assert!(joined_chapters(&[]).is_empty());

        // A part before the last without an exact running time: later
        // chapters cannot be placed, so none are stated.
        assert!(joined_chapters(&[
            part(&[("One", 0), ("Two", 9)], None),
            part(&[("Three", 0)], Some(1_000))
        ])
        .is_empty());
        // A chapter placed past its own part's end is left out.
        assert_eq!(
            joined_chapters(&[
                part(&[("One", 0), ("Two", 500), ("Stray", 1_000)], Some(1_000)),
                part(&[("Three", 0)], Some(1_000))
            ]),
            [
                ("One".to_string(), 0),
                ("Two".to_string(), 500),
                ("Three".to_string(), 1_000)
            ]
        );
        // The last part's running time is never needed.
        assert_eq!(
            joined_chapters(&[
                part(&[("One", 0)], Some(1_000)),
                part(&[("Two", 0), ("Three", 7)], None)
            ])
            .len(),
            3
        );
    }

    #[test]
    fn the_source_says_whether_anyone_is_asked() {
        let none = LibraryCopySource::default();
        assert!(none.copies().is_none());
        assert_eq!(format!("{none:?}"), "LibraryCopySource(none)");
        let asked = LibraryCopySource::new(OneCopy::of(MemoryCopy::new(None, Vec::new())))
            .read_by(Arc::new(MediaPool::in_process()));
        assert!(asked.copies().is_some());
        assert_eq!(format!("{asked:?}"), "LibraryCopySource(asked)");
    }
}
