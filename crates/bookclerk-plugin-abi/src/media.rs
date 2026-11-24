//! The media a plugin declares it works with, and the checks the host runs
//! against that declaration.
//!
//! Storefronts and metadata providers declare `MediaSupport` in
//! `plugin.toml` (`[media]`) and in `describe()`. The declaration is
//! required, never defaulted: a plugin with those entrypoints and an empty
//! or `unspecified` declaration is refused at spawn, and every record it
//! returns must name kinds inside it. A `fetchTitle` reply must name a
//! carrier the plugin declared it fetches and list its files by role. The
//! checks are sans-IO so the fuzz targets and the host run the same code.

use crate::{ExpressionKind, FetchedFileRole, MediaKind, MediaSupport, PlainFetch, WorkKind};

/// Why a `MediaSupport` declaration, or a record against it, was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaError {
    /// A plugin that must declare its media left `workKinds` or `lists`
    /// empty.
    Missing(&'static str),
    /// A plugin that may not declare media (it exports neither the
    /// storefront nor the metadata entrypoint) declared some.
    NotAllowed,
    /// A declaration list named `unspecified`.
    UnspecifiedInDeclaration(&'static str),
    /// `fetches` named a carrier `lists` does not.
    FetchNotListed(MediaKind),
    /// A metadata provider declared `fetches`; only storefronts download.
    FetchWithoutStorefront,
    /// A record left one of its kinds `unspecified`.
    UnspecifiedKind(&'static str),
    /// A record's work kind is not in `workKinds`.
    WorkKindNotDeclared(WorkKind),
    /// A record's carrier is not in `lists`.
    MediaKindNotDeclared(MediaKind),
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(field) => write!(f, "media declaration `{field}` is empty"),
            Self::NotAllowed => {
                f.write_str("media declaration needs the `storefront` or `metadata` entrypoint")
            }
            Self::UnspecifiedInDeclaration(field) => {
                write!(f, "media declaration `{field}` names `unspecified`")
            }
            Self::FetchNotListed(kind) => {
                write!(f, "media declaration fetches {kind:?} but does not list it")
            }
            Self::FetchWithoutStorefront => {
                f.write_str("media declaration `fetches` needs the `storefront` entrypoint")
            }
            Self::UnspecifiedKind(field) => write!(f, "record leaves `{field}` unspecified"),
            Self::WorkKindNotDeclared(kind) => {
                write!(
                    f,
                    "record work kind {kind:?} is outside the plugin's declaration"
                )
            }
            Self::MediaKindNotDeclared(kind) => {
                write!(
                    f,
                    "record carrier {kind:?} is outside the plugin's declaration"
                )
            }
        }
    }
}

impl std::error::Error for MediaError {}

/// Whether a declaration says nothing at all (every list empty).
#[must_use]
pub fn media_support_is_empty(media: &MediaSupport) -> bool {
    media.work_kinds.is_empty() && media.lists.is_empty() && media.fetches.is_empty()
}

/// Checks a declaration on its own.
///
/// # Arguments
///
/// * `media` - The declaration (from `plugin.toml` or `describe()`).
/// * `storefront` - Whether the plugin exports the storefront entrypoint.
/// * `metadata` - Whether the plugin exports the metadata entrypoint.
///
/// # Errors
///
/// Returns the first rule the declaration breaks: a storefront or metadata
/// provider must name at least one work kind and one listed carrier, no
/// list may name `unspecified`, `fetches` must be a subset of `lists` and
/// is for storefronts only, and any other plugin must declare nothing.
pub fn validate_media_support(
    media: &MediaSupport,
    storefront: bool,
    metadata: bool,
) -> Result<(), MediaError> {
    if !storefront && !metadata {
        return if media_support_is_empty(media) {
            Ok(())
        } else {
            Err(MediaError::NotAllowed)
        };
    }
    if media.work_kinds.is_empty() {
        return Err(MediaError::Missing("workKinds"));
    }
    if media.lists.is_empty() {
        return Err(MediaError::Missing("lists"));
    }
    if media.work_kinds.contains(&WorkKind::Unspecified) {
        return Err(MediaError::UnspecifiedInDeclaration("workKinds"));
    }
    if media.lists.contains(&MediaKind::Unspecified) {
        return Err(MediaError::UnspecifiedInDeclaration("lists"));
    }
    if media.fetches.contains(&MediaKind::Unspecified) {
        return Err(MediaError::UnspecifiedInDeclaration("fetches"));
    }
    if !storefront && !media.fetches.is_empty() {
        return Err(MediaError::FetchWithoutStorefront);
    }
    if let Some(kind) = media.fetches.iter().find(|k| !media.lists.contains(k)) {
        return Err(MediaError::FetchNotListed(*kind));
    }
    Ok(())
}

/// Whether two declarations name the same kinds, ignoring order and
/// repeats: the host requires `describe()` to say what `plugin.toml` says.
#[must_use]
pub fn same_media_support(a: &MediaSupport, b: &MediaSupport) -> bool {
    fn same<T: PartialEq>(x: &[T], y: &[T]) -> bool {
        x.iter().all(|v| y.contains(v)) && y.iter().all(|v| x.contains(v))
    }
    same(&a.work_kinds, &b.work_kinds) && same(&a.lists, &b.lists) && same(&a.fetches, &b.fetches)
}

/// Checks one record's kinds against the plugin's declaration.
///
/// # Arguments
///
/// * `media` - The plugin's declaration.
/// * `work` - The record's work kind.
/// * `expression` - The record's expression kind.
/// * `carrier` - The record's media kind.
///
/// # Errors
///
/// Returns [`MediaError::UnspecifiedKind`] when the record leaves a kind
/// unspecified, or a `*NotDeclared` error when its work kind or carrier is
/// outside the declaration.
pub fn check_record_kinds(
    media: &MediaSupport,
    work: WorkKind,
    expression: ExpressionKind,
    carrier: MediaKind,
) -> Result<(), MediaError> {
    if work == WorkKind::Unspecified {
        return Err(MediaError::UnspecifiedKind("workKind"));
    }
    if expression == ExpressionKind::Unspecified {
        return Err(MediaError::UnspecifiedKind("expressionKind"));
    }
    if carrier == MediaKind::Unspecified {
        return Err(MediaError::UnspecifiedKind("mediaKind"));
    }
    if !media.work_kinds.contains(&work) {
        return Err(MediaError::WorkKindNotDeclared(work));
    }
    if !media.lists.contains(&carrier) {
        return Err(MediaError::MediaKindNotDeclared(carrier));
    }
    Ok(())
}

/// Why a `fetchTitle` reply ([`PlainFetch`]) was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// `mediaKind` was left `unspecified`.
    UnspecifiedCarrier,
    /// `mediaKind` is not in the plugin's declared `fetches`.
    CarrierNotFetched(MediaKind),
    /// A file left its role `unspecified`.
    UnspecifiedRole {
        /// Index of the file in `files`.
        index: usize,
    },
    /// A file had an empty path.
    EmptyPath {
        /// Index of the file in `files`.
        index: usize,
    },
    /// The reply listed neither a `part` nor a `whole` file.
    NoContent,
    /// The reply listed both `part`s and a `whole` file.
    PartsAndWhole,
    /// The reply listed more than one `whole` file.
    SecondWhole,
    /// The reply listed more than one `cover`.
    SecondCover,
    /// The reply listed more than one `catalogRecord`.
    SecondCatalogRecord,
    /// The reply listed more than one `annotations` file.
    SecondAnnotations,
    /// A file's path is not a plain absolute path inside the fetch's cache
    /// directory (relative, holding `.` or `..`, or elsewhere).
    OutsideCacheDir {
        /// Index of the file in `files`.
        index: usize,
    },
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnspecifiedCarrier => f.write_str("fetch leaves `mediaKind` unspecified"),
            Self::CarrierNotFetched(kind) => {
                write!(
                    f,
                    "fetch carrier {kind:?} is outside the plugin's declared `fetches`"
                )
            }
            Self::UnspecifiedRole { index } => {
                write!(f, "fetched file {index} leaves its role unspecified")
            }
            Self::EmptyPath { index } => write!(f, "fetched file {index} has an empty path"),
            Self::NoContent => f.write_str("fetch lists no `part` or `whole` file"),
            Self::PartsAndWhole => f.write_str("fetch lists both `part` files and a `whole` file"),
            Self::SecondWhole => f.write_str("fetch lists more than one `whole` file"),
            Self::SecondCover => f.write_str("fetch lists more than one `cover`"),
            Self::SecondCatalogRecord => f.write_str("fetch lists more than one `catalogRecord`"),
            Self::SecondAnnotations => f.write_str("fetch lists more than one `annotations` file"),
            Self::OutsideCacheDir { index } => {
                write!(
                    f,
                    "fetched file {index} is not inside the fetch's cache directory"
                )
            }
        }
    }
}

impl std::error::Error for FetchError {}

/// Checks a `fetchTitle` reply against the plugin's declaration.
///
/// The carrier must be one the plugin declared in `fetches`, and the files
/// must describe one edition: ordered `part`s or a single `whole` file, plus
/// at most one `cover`, one `catalogRecord` and one `annotations` file, each
/// with a role and a path.
///
/// # Arguments
///
/// * `fetch` - The guest's reply (untrusted).
/// * `media` - The plugin's declaration.
///
/// # Errors
///
/// Returns the first [`FetchError`] the reply breaks, checking the carrier
/// before the files.
pub fn validate_plain_fetch(fetch: &PlainFetch, media: &MediaSupport) -> Result<(), FetchError> {
    if fetch.media_kind == MediaKind::Unspecified {
        return Err(FetchError::UnspecifiedCarrier);
    }
    if !media.fetches.contains(&fetch.media_kind) {
        return Err(FetchError::CarrierNotFetched(fetch.media_kind));
    }
    let (mut parts, mut wholes, mut covers) = (0usize, 0usize, 0usize);
    let (mut records, mut annotations) = (0usize, 0usize);
    for (index, file) in fetch.files.iter().enumerate() {
        if file.path.trim().is_empty() {
            return Err(FetchError::EmptyPath { index });
        }
        match file.role {
            FetchedFileRole::Unspecified => return Err(FetchError::UnspecifiedRole { index }),
            FetchedFileRole::Part => parts += 1,
            FetchedFileRole::Whole => wholes += 1,
            FetchedFileRole::Cover => covers += 1,
            FetchedFileRole::CatalogRecord => records += 1,
            FetchedFileRole::Annotations => annotations += 1,
        }
    }
    if wholes > 1 {
        return Err(FetchError::SecondWhole);
    }
    if covers > 1 {
        return Err(FetchError::SecondCover);
    }
    if records > 1 {
        return Err(FetchError::SecondCatalogRecord);
    }
    if annotations > 1 {
        return Err(FetchError::SecondAnnotations);
    }
    match (parts, wholes) {
        (0, 0) => Err(FetchError::NoContent),
        (0, _) | (_, 0) => Ok(()),
        _ => Err(FetchError::PartsAndWhole),
    }
}

/// Checks that every file a `fetchTitle` reply names lies inside the cache
/// directory the host handed the guest, by its spelling alone.
///
/// A path passes when it is absolute, has no `.` or `..` component, and
/// starts with the components of one of `cache_dirs` followed by at least
/// one more. The host passes the directory as it sent it and as it resolves
/// on disk, since a guest may answer with either spelling (on macOS the
/// temporary directory sits behind the `/var` to `/private/var` link). The
/// host then resolves each path on disk (following symlinks) and checks it
/// again, since a guest can plant a symlink under its own cache directory;
/// this check refuses the replies that need no disk to refuse.
///
/// # Arguments
///
/// * `fetch` - The guest's reply (untrusted).
/// * `cache_dirs` - Spellings of `FetchTitleParams.cacheDir`; a file must
///   lie inside one of them.
///
/// # Errors
///
/// Returns [`FetchError::OutsideCacheDir`] for the first file outside them.
pub fn validate_fetched_paths(
    fetch: &PlainFetch,
    cache_dirs: &[&std::path::Path],
) -> Result<(), FetchError> {
    for (index, file) in fetch.files.iter().enumerate() {
        let path = std::path::Path::new(&file.path);
        if !cache_dirs.iter().any(|dir| lexically_inside(path, dir)) {
            return Err(FetchError::OutsideCacheDir { index });
        }
    }
    Ok(())
}

/// Whether `path` is a plain absolute path strictly below `dir`, by spelling.
///
/// # Arguments
///
/// * `path` - The guest's path.
/// * `dir` - The directory it must lie under.
///
/// # Returns
///
/// `true` when `path` is absolute, holds no `.` or `..` component, and has
/// every component of `dir` as a prefix plus at least one more.
fn lexically_inside(path: &std::path::Path, dir: &std::path::Path) -> bool {
    use std::path::Component;
    if !path.is_absolute() || !dir.is_absolute() {
        return false;
    }
    // `components()` drops a lone `.` in the middle of a path, so look at
    // the spelling for it too.
    let spelled_dot = path
        .to_str()
        .is_some_and(|s| s.split(['/', '\\']).any(|seg| seg == "." || seg == ".."));
    if spelled_dot
        || path
            .components()
            .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return false;
    }
    let mut rest = path.components();
    for want in dir.components() {
        if rest.next() != Some(want) {
            return false;
        }
    }
    rest.next().is_some()
}

/// Decodes an unpacked single-segment Cap'n Proto message whose root is a
/// `PlainFetch`, with a traversal budget derived from the input size.
///
/// The host decodes replies through the RPC layer; this byte entry point
/// lets the `plain_fetch` fuzz target drive the same reader and validator.
///
/// # Arguments
///
/// * `bytes` - The message bytes (untrusted).
///
/// # Returns
///
/// The decoded reply.
///
/// # Errors
///
/// Returns the Cap'n Proto error the framing or the codec reports.
pub fn decode_plain_fetch_bytes(bytes: &[u8]) -> capnp::Result<PlainFetch> {
    let words = (bytes.len() / 8).saturating_add(16);
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(words));
    opts.nesting_limit(32);
    let reader = capnp::serialize::read_message(&mut std::io::Cursor::new(bytes), opts)?;
    crate::read_plain_fetch(reader.get_root::<crate::plugin_capnp::plain_fetch::Reader<'_>>()?)
}

/// Encodes `fetch` as an unpacked single-segment Cap'n Proto message, the
/// inverse of [`decode_plain_fetch_bytes`].
///
/// # Arguments
///
/// * `fetch` - The reply to encode.
///
/// # Returns
///
/// The message bytes.
///
/// # Errors
///
/// Returns the Cap'n Proto error the builder reports.
pub fn encode_plain_fetch_bytes(fetch: &PlainFetch) -> capnp::Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    crate::write_plain_fetch(
        message.init_root::<crate::plugin_capnp::plain_fetch::Builder<'_>>(),
        fetch,
    )?;
    Ok(capnp::serialize::write_message_to_words(&message))
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::FetchedFile;

    fn audio_store() -> MediaSupport {
        MediaSupport {
            work_kinds: vec![WorkKind::Book, WorkKind::Podcast],
            lists: vec![MediaKind::Audio],
            fetches: vec![MediaKind::Audio],
        }
    }

    #[test]
    fn storefronts_and_metadata_providers_must_declare() {
        assert!(validate_media_support(&audio_store(), true, false).is_ok());
        let empty = MediaSupport::default();
        assert_eq!(
            validate_media_support(&empty, true, false),
            Err(MediaError::Missing("workKinds"))
        );
        assert_eq!(
            validate_media_support(&empty, false, true),
            Err(MediaError::Missing("workKinds"))
        );
        assert!(validate_media_support(&empty, false, false).is_ok());
        assert_eq!(
            validate_media_support(&audio_store(), false, false),
            Err(MediaError::NotAllowed)
        );
    }

    #[test]
    fn declarations_name_real_kinds_and_fetch_only_what_they_list() {
        let mut m = audio_store();
        m.lists = Vec::new();
        assert_eq!(
            validate_media_support(&m, true, false),
            Err(MediaError::Missing("lists"))
        );
        let mut m = audio_store();
        m.work_kinds.push(WorkKind::Unspecified);
        assert_eq!(
            validate_media_support(&m, true, false),
            Err(MediaError::UnspecifiedInDeclaration("workKinds"))
        );
        let mut m = audio_store();
        m.fetches.push(MediaKind::Ebook);
        assert_eq!(
            validate_media_support(&m, true, false),
            Err(MediaError::FetchNotListed(MediaKind::Ebook))
        );
        assert_eq!(
            validate_media_support(&audio_store(), false, true),
            Err(MediaError::FetchWithoutStorefront)
        );
    }

    #[test]
    fn records_must_name_declared_kinds() {
        let m = audio_store();
        let ok = check_record_kinds(
            &m,
            WorkKind::Podcast,
            ExpressionKind::OriginalAudio,
            MediaKind::Audio,
        );
        assert!(ok.is_ok());
        assert_eq!(
            check_record_kinds(
                &m,
                WorkKind::Book,
                ExpressionKind::Unspecified,
                MediaKind::Audio
            ),
            Err(MediaError::UnspecifiedKind("expressionKind"))
        );
        assert_eq!(
            check_record_kinds(&m, WorkKind::Book, ExpressionKind::Text, MediaKind::Print),
            Err(MediaError::MediaKindNotDeclared(MediaKind::Print))
        );
    }

    #[test]
    fn order_and_repeats_do_not_change_a_declaration() {
        let mut b = audio_store();
        b.work_kinds.reverse();
        b.lists.push(MediaKind::Audio);
        assert!(same_media_support(&audio_store(), &b));
        b.fetches.clear();
        assert!(!same_media_support(&audio_store(), &b));
    }

    fn file(path: &str, role: FetchedFileRole) -> FetchedFile {
        FetchedFile {
            path: path.into(),
            role,
            ..FetchedFile::default()
        }
    }

    #[test]
    fn fetches_name_a_declared_carrier_and_one_edition() {
        let m = audio_store();
        let mut fetch = PlainFetch {
            media_kind: MediaKind::Audio,
            files: vec![
                file("/c/1.mp3", FetchedFileRole::Part),
                file("/c/2.mp3", FetchedFileRole::Part),
                file("/c/cover.jpg", FetchedFileRole::Cover),
            ],
            ..PlainFetch::default()
        };
        assert_eq!(validate_plain_fetch(&fetch, &m), Ok(()));
        fetch
            .files
            .push(file("/c/book.m4b", FetchedFileRole::Whole));
        assert_eq!(
            validate_plain_fetch(&fetch, &m),
            Err(FetchError::PartsAndWhole)
        );
        fetch.files.retain(|f| f.role != FetchedFileRole::Part);
        assert_eq!(validate_plain_fetch(&fetch, &m), Ok(()));
        fetch.files.push(file("/c/b.jpg", FetchedFileRole::Cover));
        assert_eq!(
            validate_plain_fetch(&fetch, &m),
            Err(FetchError::SecondCover)
        );
        fetch.files = vec![file("/c/cover.jpg", FetchedFileRole::Cover)];
        assert_eq!(validate_plain_fetch(&fetch, &m), Err(FetchError::NoContent));
        fetch.files = vec![file("", FetchedFileRole::Whole)];
        assert_eq!(
            validate_plain_fetch(&fetch, &m),
            Err(FetchError::EmptyPath { index: 0 })
        );
        fetch.files = vec![file("/c/x", FetchedFileRole::Unspecified)];
        assert_eq!(
            validate_plain_fetch(&fetch, &m),
            Err(FetchError::UnspecifiedRole { index: 0 })
        );
        fetch.media_kind = MediaKind::Ebook;
        assert_eq!(
            validate_plain_fetch(&fetch, &m),
            Err(FetchError::CarrierNotFetched(MediaKind::Ebook))
        );
        fetch.media_kind = MediaKind::Unspecified;
        assert_eq!(
            validate_plain_fetch(&fetch, &m),
            Err(FetchError::UnspecifiedCarrier)
        );
    }

    #[test]
    fn a_fetch_carries_at_most_one_catalog_record_and_annotations_file() {
        let m = audio_store();
        let mut fetch = PlainFetch {
            media_kind: MediaKind::Audio,
            files: vec![
                file("/c/book.m4b", FetchedFileRole::Whole),
                file("/c/book.json", FetchedFileRole::CatalogRecord),
                file("/c/clips.json", FetchedFileRole::Annotations),
            ],
            ..PlainFetch::default()
        };
        assert_eq!(validate_plain_fetch(&fetch, &m), Ok(()));
        fetch
            .files
            .push(file("/c/again.json", FetchedFileRole::CatalogRecord));
        assert_eq!(
            validate_plain_fetch(&fetch, &m),
            Err(FetchError::SecondCatalogRecord)
        );
        fetch.files.pop();
        fetch
            .files
            .push(file("/c/again.json", FetchedFileRole::Annotations));
        assert_eq!(
            validate_plain_fetch(&fetch, &m),
            Err(FetchError::SecondAnnotations)
        );
        // Companions alone are no edition.
        fetch.files = vec![file("/c/book.json", FetchedFileRole::CatalogRecord)];
        assert_eq!(validate_plain_fetch(&fetch, &m), Err(FetchError::NoContent));
    }

    #[test]
    fn fetched_paths_must_lie_inside_the_cache_dir() {
        // An absolute path on this platform: Windows needs a drive.
        let abs = |path: &str| format!("{}{path}", if cfg!(windows) { "C:" } else { "" });
        let dir_spelling = abs("/scratch/fetch");
        let dir = std::path::Path::new(&dir_spelling);
        let fetch = |path: &str| PlainFetch {
            media_kind: MediaKind::Audio,
            files: vec![
                file(&abs("/scratch/fetch/B0/book.m4b"), FetchedFileRole::Whole),
                file(path, FetchedFileRole::Cover),
            ],
            ..PlainFetch::default()
        };
        assert_eq!(
            validate_fetched_paths(&fetch(&abs("/scratch/fetch/B0/c.jpg")), &[dir]),
            Ok(())
        );
        let outside = [
            abs("/etc/passwd"),
            "relative/c.jpg".to_owned(),
            abs("/scratch/fetch"),
            abs("/scratch/fetch/"),
            abs("/scratch/fetchx/c.jpg"),
            abs("/scratch/fetch/../fetch/c.jpg"),
            abs("/scratch/fetch/./c.jpg"),
            abs("/scratch/fetch/B0/../../../etc/passwd"),
        ];
        for outside in &outside {
            assert_eq!(
                validate_fetched_paths(&fetch(outside), &[dir]),
                Err(FetchError::OutsideCacheDir { index: 1 }),
                "{outside}"
            );
        }
        assert_eq!(
            validate_fetched_paths(
                &fetch(&abs("/scratch/fetch/c.jpg")),
                &[std::path::Path::new("rel")]
            ),
            Err(FetchError::OutsideCacheDir { index: 0 })
        );
        // Either spelling of the directory admits a file.
        let resolved_spelling = abs("/private/scratch/fetch");
        let resolved = std::path::Path::new(&resolved_spelling);
        let in_resolved = abs("/private/scratch/fetch/c.jpg");
        assert_eq!(
            validate_fetched_paths(&fetch(&in_resolved), &[dir, resolved]),
            Ok(())
        );
        assert_eq!(
            validate_fetched_paths(&fetch(&in_resolved), &[dir]),
            Err(FetchError::OutsideCacheDir { index: 1 })
        );
    }
}
