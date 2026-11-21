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
//! A copy the library acquired from another store is not taken on sight:
//! the install downloads its own first, and the pipeline keeps the better
//! of the two by probe, once.
//!
//! The acquire asks through [`LibraryCopies`], which each kind of install
//! implements over its own connection to the library. This module decides
//! which of the copy's files are taken ([`plan_files`]), downloads them
//! into the acquire's scratch directory and reads the audio's chapters in
//! the confined media worker ([`fetch_library_copy`]), giving the pipeline
//! the same [`PlainFetch`] a store fetch gives.
//!
//! Only a copy held as one audio file is taken, with its cover and, when
//! this install keeps companion PDFs, the PDF the library holds for the
//! edition (the same file the store gives). A book held as several audio
//! files comes from the store. Sidecars are written again by this
//! install's own pipeline from its own options.

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

    /// What the library's probe found in the copy's primary audio: its
    /// media type, average bitrate and whether it is lossless. The acquire
    /// compares it with its own store download when the copy came from
    /// another store ([`bookclerk_library::FileProbe::is_better_than`]),
    /// and probes the copy itself before it believes the answer.
    fn probe(&self) -> bookclerk_library::FileProbe;

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

    /// Called once the acquire holds the copy's files and before it stores
    /// any of them, so the implementation can record where the book comes
    /// from (a branch never shares a book back to the library it took it
    /// from). Recording first means audio a failed store step leaves on a
    /// destination is already known as the library's. It may run again for
    /// the same title, so it must be safe to repeat.
    ///
    /// # Errors
    ///
    /// Returns an error when the record cannot be written; the acquire
    /// then does not use the copy.
    async fn taken(&self) -> Result<(), LibraryCopyError>;
}

/// Where an acquire asks for the copy a library holds of a title.
#[async_trait::async_trait]
pub trait LibraryCopies: Send + Sync {
    /// The copy a library this install joined offers of `listing`; `None`
    /// when no library holds one this install's person may take, when
    /// this install does not keep copies, or when no library answers.
    ///
    /// A copy from the listing's own store ([`from_store`]) is taken
    /// instead of the store download; a copy from another store only
    /// replaces the store download when it is the better one. An
    /// implementation that asks several libraries therefore keeps asking
    /// past a copy from another store and returns it only when no library
    /// has one from the listing's own, and never returns a copy whose
    /// store the library does not know.
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
    /// Where the audio the acquire stores came from, when not the title's
    /// own store.
    stored_from: StoredFrom,
}

/// The store the audio an acquire stores was downloaded from, when it is
/// not the title's own: set when a library's copy from another store
/// replaced the title's own download, and read when a written file's
/// probe is recorded. Clones share the one value, so the request and the
/// recording destinations of one acquire agree.
#[derive(Clone, Debug, Default)]
pub(crate) struct StoredFrom(Arc<std::sync::Mutex<Option<String>>>);

impl StoredFrom {
    /// Names the store the stored audio came from.
    pub(crate) fn set(&self, store: &str) {
        // A poisoned lock still holds a whole value.
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(store.to_owned());
    }

    /// Forgets the store named: the audio stored from now on is the title's
    /// own store download.
    pub(crate) fn clear(&self) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// The store named, if any.
    pub(crate) fn get(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl LibraryCopySource {
    /// Asks `copies` for each title before its store.
    #[must_use]
    pub fn new(copies: Arc<dyn LibraryCopies>) -> Self {
        Self {
            copies: Some(copies),
            media: None,
            stored_from: StoredFrom::default(),
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

    /// The same source for one acquire: one job's titles share a source,
    /// and each must name where its own stored audio came from.
    #[must_use]
    pub(crate) fn for_one_acquire(mut self) -> Self {
        self.stored_from = StoredFrom::default();
        self
    }

    /// Where this acquire's stored audio came from, when not the title's
    /// own store.
    pub(crate) fn stored_from(&self) -> &StoredFrom {
        &self.stored_from
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
/// know is never taken, and one from another store only replaces the
/// install's own download when it is the better copy.
///
/// An implementation of [`LibraryCopies`] that asks several libraries uses
/// this to pass over a copy from another store and ask the next library.
#[must_use]
pub fn from_store(copy_store: Option<&str>, store: &str) -> bool {
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
    /// The companion PDF.
    Pdf,
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
const AUDIO_EXTS: [&str; 8] = ["m4b", "m4a", "mp4", "mp3", "flac", "opus", "ogg", "oga"];

/// Image types a copy's cover may be.
const COVER_EXTS: [&str; 3] = ["jpg", "jpeg", "png"];

/// The one file type a copy's companion PDF may be.
const PDF_EXTS: [&str; 1] = ["pdf"];

/// The files of a copy an acquire downloads, in download order: the one
/// audio file, then the first cover (unless [`CopyWants::skip_cover`]),
/// then (when [`CopyWants::pdf`]) the first companion PDF.
///
/// Only a copy held as **one** audio file is taken. A book the library
/// holds as several audio files (parts, or one file per chapter) is not:
/// the grant lists those by storage key, which need not be playing order,
/// and it may list a whole-book file beside its own chapter files.
///
/// # Returns
///
/// `None` when the copy cannot be taken: it lists no audio, its first
/// file is not the primary audio, it lists more than one audio file, the
/// audio is of a kind the tag reader does not understand, or
/// [`CopyWants::mp4_audio_only`] is set and the audio is in another
/// container.
#[must_use]
pub(crate) fn plan_files(files: &[LibraryCopyFile], wants: CopyWants) -> Option<Vec<TakenFile>> {
    let first = files.first()?;
    if first.role != FileRole::Audio {
        return None;
    }
    let ext = known_ext(&first.name, &AUDIO_EXTS)?;
    let kind = if matches!(ext, "m4b" | "m4a" | "mp4") {
        Taken::Mp4Audio
    } else {
        Taken::OtherAudio
    };
    if wants.mp4_audio_only && kind != Taken::Mp4Audio {
        return None;
    }
    let mut taken = vec![TakenFile {
        index: 0,
        kind,
        ext,
        size: first.size,
    }];
    let mut cover = None;
    let mut pdf = None;
    for (index, file) in files.iter().enumerate().skip(1) {
        match file.role {
            FileRole::Audio | FileRole::Part | FileRole::Chapter => return None,
            FileRole::Cover => {
                if !wants.skip_cover && cover.is_none() && file.size > 0 {
                    cover = known_ext(&file.name, &COVER_EXTS).map(|ext| TakenFile {
                        index,
                        kind: Taken::Cover,
                        ext,
                        size: file.size,
                    });
                }
            }
            FileRole::Pdf => {
                if wants.pdf && pdf.is_none() && file.size > 0 {
                    pdf = known_ext(&file.name, &PDF_EXTS).map(|ext| TakenFile {
                        index,
                        kind: Taken::Pdf,
                        ext,
                        size: file.size,
                    });
                }
            }
            FileRole::Sidecar | FileRole::Transcript | FileRole::Epub => {}
        }
    }
    taken.extend(cover);
    taken.extend(pdf);
    Some(taken)
}

/// What an acquire's own output options ask of a library's copy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CopyWants {
    /// The acquire keeps companion PDFs, so the copy's PDF is taken.
    pub pdf: bool,
    /// The acquire splits its output by chapter. The splitter cuts MP4
    /// audio only, and a single file in another container is not
    /// repackaged first, so only a copy whose audio is MP4 is taken.
    pub mp4_audio_only: bool,
    /// The acquire already has a cover (its own store download's), so
    /// the copy's cover is not taken.
    pub skip_cover: bool,
}

/// A library's copy downloaded into scratch.
#[derive(Debug)]
pub(crate) struct LibraryFetch {
    /// The audio and cover, described as a store fetch describes them.
    pub fetch: PlainFetch,
    /// The companion PDF the library holds for the edition, when the
    /// acquire wants one and the copy lists one.
    pub pdf: Option<PathBuf>,
}

/// Downloads the files of `copy` an acquire uses into `dir` and describes
/// them as a store fetch would.
///
/// The one audio file keeps the chapters the confined tag reader finds in
/// it. The companion PDF is downloaded as it is: it is the same file the
/// store gives for the edition, and it is stored, never opened.
///
/// # Arguments
///
/// * `copy` - The copy the library offered.
/// * `dir` - A scratch directory of this acquire alone; files are written
///   as `<index>.<ext>`, never under a name the library gave.
/// * `budget` - Most bytes the files may take together.
/// * `media` - Reads the downloaded audio file's tags.
/// * `wants` - What the acquire's output options ask of the copy.
///
/// # Errors
///
/// Returns an error when the copy has no usable audio (see
/// [`plan_files`]), is larger than `budget`, a download fails or leaves a
/// file of another size than listed, or the tag reader cannot read the
/// audio. `dir` may then hold partial files; the caller removes it.
pub(crate) async fn fetch_library_copy(
    copy: &dyn LibraryCopy,
    dir: &Path,
    budget: u64,
    media: &MediaPool,
    wants: CopyWants,
) -> Result<LibraryFetch, LibraryCopyError> {
    let taken = plan_files(copy.files(), wants)
        .ok_or("the copy is not one audio file this install can use")?;
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
    let mut pdf = None;
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
        match file.kind {
            Taken::Cover => fetch.cover_path = Some(path),
            Taken::Pdf => pdf = Some(path),
            Taken::Mp4Audio | Taken::OtherAudio => {
                let tags = media.read_tags(file.size, &FileRange::new(&path)).await?;
                fetch.chapters = tags
                    .chapters
                    .iter()
                    .map(|chapter| (chapter.title.clone(), chapter.start_ms))
                    .collect();
                if file.kind == Taken::Mp4Audio {
                    fetch.m4b_path = Some(path);
                } else {
                    fetch.parts.push(audio_part(path, &tags));
                }
            }
        }
    }
    Ok(LibraryFetch { fetch, pdf })
}

/// The one downloaded audio file as a part, with the running time the tag
/// reader found. Its chapters are carried by the fetch, so it has no title
/// of its own.
fn audio_part(path: PathBuf, tags: &bookclerk_media::TagRecord) -> PlainAudioPart {
    PlainAudioPart {
        path,
        title: None,
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
        /// True when [`LibraryCopy::taken`] fails (after counting).
        pub refuses_taken: bool,
        /// What the library says its probe found: `(bitrate, lossless)`.
        pub claims: (Option<u32>, bool),
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
                refuses_taken: false,
                claims: (None, false),
            }
        }
    }

    #[async_trait::async_trait]
    impl LibraryCopy for MemoryCopy {
        fn probe(&self) -> bookclerk_library::FileProbe {
            // The media type follows the first file's extension, as a
            // library's probe would name the file it read.
            let ext = self
                .listed
                .first()
                .and_then(|f| f.name.rsplit_once('.'))
                .map(|(_, ext)| ext.to_ascii_lowercase());
            let content_type = match ext.as_deref() {
                Some("flac") => "audio/flac",
                Some("mp3") => "audio/mpeg",
                Some("ogg" | "oga" | "opus") => "audio/ogg",
                _ => "audio/mp4",
            };
            bookclerk_library::FileProbe {
                content_type: content_type.into(),
                bitrate_kbps: self.claims.0,
                lossless: self.claims.1,
                source_store: self.store.clone(),
            }
        }

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
            if self.refuses_taken {
                return Err("the record could not be written".into());
            }
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

    /// Hands out prepared copies, one per ask in order, then none.
    pub(crate) struct QueuedCopies {
        /// The copies still to hand out, the next first.
        copies: Mutex<std::collections::VecDeque<MemoryCopy>>,
        /// How many times a copy was asked for.
        asked: AtomicUsize,
    }

    impl QueuedCopies {
        /// Hands out `copies`, the first to the first ask.
        pub(crate) fn of(copies: Vec<MemoryCopy>) -> Arc<Self> {
            Arc::new(Self {
                copies: Mutex::new(copies.into()),
                asked: AtomicUsize::new(0),
            })
        }

        /// How many times a copy was asked for.
        pub(crate) fn asks(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl LibraryCopies for QueuedCopies {
        async fn copy_of(&self, _listing: LibraryCopyListing<'_>) -> Option<Box<dyn LibraryCopy>> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            let next = self.copies.lock().unwrap().pop_front()?;
            Some(Box::new(next))
        }
    }

    /// An acquire that keeps companion PDFs.
    const PDF: CopyWants = CopyWants {
        pdf: true,
        mp4_audio_only: false,
        skip_cover: false,
    };

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
    fn the_audio_the_first_cover_and_a_wanted_pdf_are_taken() {
        let files = [
            listed("Book.M4B", FileRole::Audio, 10),
            listed("Book.cue", FileRole::Sidecar, 3),
            listed("empty.pdf", FileRole::Pdf, 0),
            listed("Book.pdf.exe", FileRole::Pdf, 2),
            listed("Book.PDF", FileRole::Pdf, 4),
            listed("Other.pdf", FileRole::Pdf, 4),
            listed("empty.jpg", FileRole::Cover, 0),
            listed("cover.webp", FileRole::Cover, 5),
            listed("cover.JPG", FileRole::Cover, 6),
            listed("Book.txt", FileRole::Transcript, 7),
            listed("Book.epub", FileRole::Epub, 8),
        ];
        // The first cover that has bytes and is of a type the pipeline
        // embeds is the one taken, and the first PDF that has bytes.
        let audio = TakenFile {
            index: 0,
            kind: Taken::Mp4Audio,
            ext: "m4b",
            size: 10,
        };
        let cover = TakenFile {
            index: 8,
            kind: Taken::Cover,
            ext: "jpg",
            size: 6,
        };
        let pdf = TakenFile {
            index: 4,
            kind: Taken::Pdf,
            ext: "pdf",
            size: 4,
        };
        assert_eq!(plan_files(&files, PDF).unwrap(), [audio, cover, pdf]);
        // An acquire that keeps no companion PDFs does not download one.
        assert_eq!(
            plan_files(&files, CopyWants::default()).unwrap(),
            [audio, cover]
        );

        let mp3 = [
            listed("art/cover.png", FileRole::Cover, 9),
            listed("Book.mp3", FileRole::Audio, 1),
        ];
        assert_eq!(plan_files(&mp3, PDF), None, "the audio is listed first");
        let mp3 = [
            listed("Book.mp3", FileRole::Audio, 1),
            listed("art/cover.png", FileRole::Cover, 9),
        ];
        assert_eq!(
            plan_files(&mp3, PDF)
                .unwrap()
                .iter()
                .map(|f| (f.index, f.kind, f.ext))
                .collect::<Vec<_>>(),
            [(0, Taken::OtherAudio, "mp3"), (1, Taken::Cover, "png")]
        );
        // An acquire that keeps its own cover takes the audio alone.
        let own_cover = CopyWants {
            skip_cover: true,
            ..PDF
        };
        assert_eq!(
            plan_files(&mp3, own_cover)
                .unwrap()
                .iter()
                .map(|f| (f.index, f.kind))
                .collect::<Vec<_>>(),
            [(0, Taken::OtherAudio)]
        );
    }

    #[test]
    fn an_acquire_that_splits_by_chapter_takes_mp4_audio_only() {
        // The chapter splitter cuts MP4 audio; one MP3 or FLAC with
        // chapters inside would reach it as it is and fail the acquire.
        let split = CopyWants {
            pdf: false,
            mp4_audio_only: true,
            skip_cover: false,
        };
        for name in ["Book.mp3", "Book.flac"] {
            let files = [listed(name, FileRole::Audio, 1)];
            assert_eq!(plan_files(&files, split), None, "{name}");
            assert!(plan_files(&files, CopyWants::default()).is_some(), "{name}");
        }
        for name in ["Book.m4b", "Book.m4a", "Book.mp4"] {
            assert!(
                plan_files(&[listed(name, FileRole::Audio, 1)], split).is_some(),
                "{name}"
            );
        }
    }

    #[test]
    fn a_book_held_as_several_audio_files_is_not_taken() {
        // Parts and chapter files are listed by storage key, which need
        // not be playing order, and a library may hold a whole-book file
        // beside its chapter files: such a book comes from the store.
        for role in [FileRole::Part, FileRole::Chapter, FileRole::Audio] {
            assert_eq!(
                plan_files(
                    &[
                        listed("Book.m4b", FileRole::Audio, 1),
                        listed("cover.jpg", FileRole::Cover, 1),
                        listed("Book - Epilogue.m4b", role, 2),
                    ],
                    PDF
                ),
                None,
                "{role:?}"
            );
        }
    }

    #[test]
    fn a_copy_without_readable_primary_audio_is_not_taken() {
        assert_eq!(plan_files(&[], PDF), None);
        // The primary audio is always listed first.
        assert_eq!(
            plan_files(&[listed("cover.jpg", FileRole::Cover, 1)], PDF),
            None
        );
        assert_eq!(plan_files(&[listed("a.mp3", FileRole::Part, 1)], PDF), None);
        // Audio of a kind the tag reader does not know, or with no
        // extension at all.
        for name in ["Book.wav", "Book", "Book.mp3.exe", ".m4b/Book"] {
            assert_eq!(
                plan_files(&[listed(name, FileRole::Audio, 1)], PDF),
                None,
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn the_one_audio_file_keeps_its_chapters() {
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
                ("../Book.pdf", FileRole::Pdf, b"%PDF-1.4".to_vec()),
            ],
        );
        let dir = tmp.path().join("one");
        let taken = fetch_library_copy(&copy, &dir, u64::MAX, &media, PDF)
            .await
            .unwrap();
        let fetch = taken.fetch;
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
        assert_eq!(taken.pdf.as_deref(), Some(dir.join("003.pdf").as_path()));
        // The sidecar was not asked for.
        assert_eq!(copy.fetched.load(Ordering::SeqCst), 3);
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["000.m4b", "002.jpg", "003.pdf"]);

        // An acquire that keeps no PDFs leaves the library's where it is.
        let dir = tmp.path().join("no-pdf");
        let taken = fetch_library_copy(&copy, &dir, u64::MAX, &media, CopyWants::default())
            .await
            .unwrap();
        assert_eq!(taken.pdf, None);
        assert!(!dir.join("003.pdf").exists());

        // One MP3 is a single part, with the chapters it carries.
        let copy = MemoryCopy::new(
            Some("libro"),
            vec![("Book.mp3", FileRole::Audio, tagged_mp3())],
        );
        let dir = tmp.path().join("mp3");
        let fetch = fetch_library_copy(&copy, &dir, u64::MAX, &media, PDF)
            .await
            .unwrap()
            .fetch;
        assert_eq!(fetch.m4b_path, None);
        assert_eq!(fetch.parts.len(), 1);
        assert_eq!(fetch.parts[0].path, dir.join("000.mp3"));

        // Several audio files are not taken, and nothing is asked for.
        let copy = MemoryCopy::new(
            Some("libro"),
            vec![
                ("Book-p001.mp3", FileRole::Audio, tagged_mp3()),
                ("Book-p002.m4b", FileRole::Part, tagged_m4b()),
            ],
        );
        let dir = tmp.path().join("parts");
        let err = fetch_library_copy(&copy, &dir, u64::MAX, &media, PDF)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("not one audio file this install can use"),
            "{err}"
        );
        assert_eq!(copy.fetched.load(Ordering::SeqCst), 0);
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn a_copy_that_does_not_arrive_as_listed_is_refused() {
        let media = MediaPool::in_process();
        let tmp = tempfile::tempdir().unwrap();

        // Larger than the scratch budget: nothing is asked for.
        let copy = MemoryCopy::new(Some("a"), vec![("Book.m4b", FileRole::Audio, tagged_m4b())]);
        let size = copy.listed[0].size;
        let err = fetch_library_copy(&copy, &tmp.path().join("big"), size - 1, &media, PDF)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("left for scratch"), "{err}");
        assert_eq!(copy.fetched.load(Ordering::SeqCst), 0);
        assert!(!tmp.path().join("big").exists());

        // A cut transfer.
        let mut copy =
            MemoryCopy::new(Some("a"), vec![("Book.m4b", FileRole::Audio, tagged_m4b())]);
        copy.failing = Some(0);
        let err = fetch_library_copy(&copy, &tmp.path().join("cut"), u64::MAX, &media, PDF)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cut"), "{err}");

        // A file of another size than listed.
        let mut copy =
            MemoryCopy::new(Some("a"), vec![("Book.m4b", FileRole::Audio, tagged_m4b())]);
        copy.listed[0].size += 1;
        let err = fetch_library_copy(&copy, &tmp.path().join("size"), u64::MAX, &media, PDF)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not the"), "{err}");

        // Audio the confined reader cannot read.
        let copy = MemoryCopy::new(
            Some("a"),
            vec![("Book.m4b", FileRole::Audio, b"not audio".to_vec())],
        );
        assert!(
            fetch_library_copy(&copy, &tmp.path().join("bad"), u64::MAX, &media, PDF)
                .await
                .is_err()
        );

        // No audio at all.
        let copy = MemoryCopy::new(Some("a"), vec![("cover.jpg", FileRole::Cover, vec![1])]);
        let err = fetch_library_copy(&copy, &tmp.path().join("none"), u64::MAX, &media, PDF)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("not one audio file this install can use"),
            "{err}"
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
