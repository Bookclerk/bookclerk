//! Storage key naming: default layout + classic Libation-style templates.
//!
//! Template evaluation is delegated to the [`libation_naming`] Chardonnay engine
//! (full property-tag / conditional / formatter parity). [`NamingContext`] is the
//! liberate-facing input; it is converted into a [`libation_naming::BookContext`]
//! internally.

use libation_config::ReplacementRule;
use libation_naming::{BookContext, ChapterContext, ContentKind, Contributor, Series};

const DEFAULT_FOLDER_TEMPLATE: &str = "<author>/<title>";
const DEFAULT_FILE_TEMPLATE: &str = "<asin>";
const DEFAULT_CHAPTER_TEMPLATE: &str = "<ch#> - <chapter title>";

/// Metadata available to naming templates.
#[derive(Debug, Clone, Default)]
pub struct NamingContext {
    pub asin: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    /// Podcast parent / series ASIN for `SavePodcastsToParentFolder`.
    pub series_asin: Option<String>,
    pub account_id: Option<String>,
    pub account_nickname: Option<String>,
    pub locale: Option<String>,
    pub language: Option<String>,
    pub publisher: Option<String>,
    pub categories: Option<String>,
    pub length_minutes: Option<i64>,
    pub bitrate: Option<i64>,
    pub samplerate: Option<i64>,
    pub channels: Option<i64>,
    pub codec: Option<String>,
    pub is_abridged: bool,
    pub content_kind: Option<String>,
    /// Chapter-specific: 1-based index.
    pub chapter_number: Option<u32>,
    pub chapter_count: Option<u32>,
    pub chapter_title: Option<String>,
}

/// Split a comma-joined display string (e.g. `"Jane Doe, John Smith"`) into
/// individual contributor names.
fn split_names(joined: Option<&str>) -> Vec<Contributor> {
    joined
        .into_iter()
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|name| Contributor::new(name, None))
        .collect()
}

fn map_content_kind(kind: Option<&str>) -> ContentKind {
    match kind.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("episode") => ContentKind::Episode,
        // audible-rs `item_kind` returns "podcast" for show parents.
        Some("podcast" | "parent" | "podcastparent" | "podcast_parent" | "season") => {
            ContentKind::PodcastParent
        }
        _ => ContentKind::Book,
    }
}

/// Convert the liberate-facing [`NamingContext`] into a naming-engine
/// [`BookContext`].
fn to_book_context(ctx: &NamingContext) -> BookContext {
    let series = vec![Series::new(
        ctx.series.clone().unwrap_or_default(),
        // Classic clears series order on podcast parents.
        if map_content_kind(ctx.content_kind.as_deref()) == ContentKind::PodcastParent {
            None
        } else {
            ctx.series_index.clone().filter(|s| !s.is_empty())
        },
        ctx.series_asin.clone(),
    )]
    .into_iter()
    .filter(|s| !s.name.is_empty() || s.id.is_some())
    .collect();

    BookContext {
        isbn: ctx.asin.clone(),
        title: Some(ctx.title.clone()),
        subtitle: ctx.subtitle.clone(),
        title_with_subtitle: None,
        authors: split_names(ctx.authors.as_deref()),
        narrators: split_names(ctx.narrators.as_deref()),
        series,
        tags: Vec::new(),
        account: ctx.account_id.clone(),
        account_nickname: ctx.account_nickname.clone(),
        locale: ctx.locale.clone(),
        language: ctx.language.clone(),
        publisher: ctx.publisher.clone(),
        categories: ctx
            .categories
            .as_deref()
            .map(|c| {
                c.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        length_minutes: ctx.length_minutes.map(|m| m as f64),
        bitrate: ctx.bitrate,
        samplerate: ctx.samplerate,
        channels: ctx.channels,
        codec: ctx.codec.clone(),
        is_abridged: ctx.is_abridged,
        content_kind: map_content_kind(ctx.content_kind.as_deref()),
        ..Default::default()
    }
}

/// Build a [`ChapterContext`] when the naming context describes a chapter.
fn to_chapter_context(ctx: &NamingContext) -> Option<ChapterContext> {
    let chapter_number = ctx.chapter_number?;
    Some(ChapterContext {
        chapter_number,
        chapter_count: ctx.chapter_count.unwrap_or(0),
        chapter_title: ctx.chapter_title.clone(),
        file_date: None,
    })
}

/// Rules to apply. Empty means no character replacement (callers should pass
/// already-resolved rules from [`libation_config::resolve_replacement_characters`]).
fn effective_rules(rules: &[ReplacementRule]) -> std::borrow::Cow<'_, [ReplacementRule]> {
    std::borrow::Cow::Borrowed(rules)
}

/// Final filesystem hardening applied on top of the naming engine's output:
/// strip control characters and trim leading / trailing dots and spaces.
fn harden_segment(segment: &str) -> String {
    let cleaned: String = segment.chars().filter(|c| !c.is_control()).collect();
    cleaned.trim().trim_matches('.').trim().to_string()
}

/// Build a relative storage key for a liberated title.
///
/// Uses POSIX separator rules for creation. Reconcile probes this layout with
/// wildcard sanitization so historical Windows/S3 keys still match.
#[must_use]
pub fn default_storage_key(authors: Option<&str>, title: &str, asin: &str, ext: &str) -> String {
    storage_key_with_rules(
        &NamingContext {
            asin: asin.to_string(),
            title: title.to_string(),
            authors: authors.map(str::to_string),
            ..Default::default()
        },
        None,
        None,
        ext,
        &libation_config::posix_replacement_characters(),
    )
}

/// Build a storage key from optional classic Libation folder/file templates.
#[must_use]
pub fn storage_key(
    ctx: &NamingContext,
    folder_template: Option<&str>,
    file_template: Option<&str>,
    ext: &str,
) -> String {
    storage_key_with_rules(
        ctx,
        folder_template,
        file_template,
        ext,
        &libation_config::posix_replacement_characters(),
    )
}

/// Like [`storage_key`] but applies classic `ReplacementCharacters`.
#[must_use]
pub fn storage_key_with_rules(
    ctx: &NamingContext,
    folder_template: Option<&str>,
    file_template: Option<&str>,
    ext: &str,
    replacement_rules: &[ReplacementRule],
) -> String {
    storage_key_with_contexts(
        ctx,
        ctx,
        folder_template,
        file_template,
        ext,
        replacement_rules,
    )
}

/// Build a storage key using separate folder and file naming contexts.
///
/// Used when `SavePodcastsToParentFolder` evaluates the folder template against
/// the podcast parent while the file template still uses the episode.
#[must_use]
pub fn storage_key_with_contexts(
    folder_ctx: &NamingContext,
    file_ctx: &NamingContext,
    folder_template: Option<&str>,
    file_template: Option<&str>,
    ext: &str,
    replacement_rules: &[ReplacementRule],
) -> String {
    let folder_book = to_book_context(folder_ctx);
    let folder_chapter = to_chapter_context(folder_ctx);
    let file_book = to_book_context(file_ctx);
    let file_chapter = to_chapter_context(file_ctx);
    let rules = effective_rules(replacement_rules);

    let folder_parts = expand_folder_segments(
        folder_template.unwrap_or(DEFAULT_FOLDER_TEMPLATE),
        &folder_book,
        folder_chapter.as_ref(),
        &rules,
    );
    let file = expand_file_segment(
        file_template.unwrap_or(DEFAULT_FILE_TEMPLATE),
        &file_book,
        file_chapter.as_ref(),
        &rules,
    );
    let file = if file.is_empty() {
        harden_segment(&libation_naming::apply_path_replacements(
            &file_ctx.asin,
            &rules,
        ))
    } else {
        file
    };

    let ext = ext.trim_start_matches('.');
    let mut parts = folder_parts;
    parts.push(format!("{file}.{ext}"));
    parts.join("/")
}

/// Storage key for a split chapter file.
#[must_use]
pub fn chapter_storage_key(
    ctx: &NamingContext,
    folder_template: Option<&str>,
    chapter_file_template: Option<&str>,
    replacement_rules: &[ReplacementRule],
    chapter_number: usize,
    chapter_title: &str,
    ext: &str,
) -> String {
    chapter_storage_key_with_folder(
        ctx,
        ctx,
        folder_template,
        chapter_file_template,
        replacement_rules,
        chapter_number,
        chapter_title,
        ext,
    )
}

/// Chapter storage key with a separate folder naming context (podcast parent folder).
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn chapter_storage_key_with_folder(
    folder_ctx: &NamingContext,
    file_ctx: &NamingContext,
    folder_template: Option<&str>,
    chapter_file_template: Option<&str>,
    replacement_rules: &[ReplacementRule],
    chapter_number: usize,
    chapter_title: &str,
    ext: &str,
) -> String {
    let mut ch_ctx = file_ctx.clone();
    ch_ctx.chapter_number = Some(chapter_number as u32);
    ch_ctx.chapter_title = Some(chapter_title.to_string());

    let folder_book = to_book_context(folder_ctx);
    let file_book = to_book_context(&ch_ctx);
    let chapter = to_chapter_context(&ch_ctx);
    let rules = effective_rules(replacement_rules);

    let folder_parts = expand_folder_segments(
        folder_template.unwrap_or(DEFAULT_FOLDER_TEMPLATE),
        &folder_book,
        chapter.as_ref(),
        &rules,
    );
    let file = expand_file_segment(
        chapter_file_template.unwrap_or(DEFAULT_CHAPTER_TEMPLATE),
        &file_book,
        chapter.as_ref(),
        &rules,
    );

    let ext = ext.trim_start_matches('.');
    if folder_parts.is_empty() {
        format!("{file}.{ext}")
    } else {
        format!("{}/{file}.{ext}", folder_parts.join("/"))
    }
}

/// Expand a folder template into hardened, non-empty path segments.
fn expand_folder_segments(
    template: &str,
    book: &BookContext,
    chapter: Option<&ChapterContext>,
    rules: &[ReplacementRule],
) -> Vec<String> {
    libation_naming::expand_folder(template, book, chapter, rules)
        .unwrap_or_default()
        .into_iter()
        .map(|s| harden_segment(&s))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Expand a file template into a single hardened filename component.
fn expand_file_segment(
    template: &str,
    book: &BookContext,
    chapter: Option<&ChapterContext>,
    rules: &[ReplacementRule],
) -> String {
    harden_segment(
        &libation_naming::expand_filename(template, book, chapter, rules).unwrap_or_default(),
    )
}

/// Replace the audio file extension on a storage key (e.g. `.m4b` → `.mp3`).
#[must_use]
pub fn swap_audio_extension(key: &str, new_ext: &str) -> String {
    let ext = new_ext.trim_start_matches('.');
    key.rsplit_once('.')
        .map(|(stem, _)| format!("{stem}.{ext}"))
        .unwrap_or_else(|| format!("{key}.{ext}"))
}

/// Replace the audio file extension on a storage key with a sidecar suffix.
#[must_use]
pub fn sidecar_key(audio_storage_key: &str, suffix: &str) -> String {
    let base = audio_storage_key
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(audio_storage_key);
    format!("{base}.{suffix}")
}

/// Basename of the liberated audio file (for `.cue` `FILE` lines).
#[must_use]
pub fn audio_basename(storage_key: &str) -> String {
    storage_key
        .rsplit('/')
        .next()
        .unwrap_or(storage_key)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_key_replaces_audio_extension() {
        assert_eq!(
            sidecar_key("Author/Title/B00X.m4b", "pdf"),
            "Author/Title/B00X.pdf"
        );
    }

    #[test]
    fn builds_safe_key() {
        // With no rules, only harden_segment runs — `/` in authors would inject
        // segments, so liberate always passes resolved rules. Explicit Windows
        // map matches classic sanitization.
        let key = storage_key_with_rules(
            &NamingContext {
                asin: "B00X".into(),
                title: "Hello: World".into(),
                authors: Some("A/B".into()),
                ..Default::default()
            },
            None,
            None,
            "m4b",
            &libation_config::windows_replacement_characters(),
        );
        assert_eq!(key, "A_B/Hello_ World/B00X.m4b");
    }

    #[test]
    fn posix_keeps_colon() {
        let key = storage_key_with_rules(
            &NamingContext {
                asin: "B00X".into(),
                title: "Hello: World".into(),
                authors: Some("Jane Doe".into()),
                ..Default::default()
            },
            None,
            None,
            "m4b",
            &libation_config::posix_replacement_characters(),
        );
        assert_eq!(key, "Jane Doe/Hello: World/B00X.m4b");
    }

    #[test]
    fn classic_templates() {
        let ctx = NamingContext {
            asin: "B00EXAMPLE1".into(),
            title: "Cool Book".into(),
            authors: Some("Jane Doe, John Smith".into()),
            narrators: Some("Reader".into()),
            series: Some("Cool Series".into()),
            series_index: Some("2".into()),
            account_id: None,
            ..Default::default()
        };
        let key = storage_key(
            &ctx,
            Some("<author>/<title>"),
            Some("<title> [<asin>]"),
            "m4b",
        );
        assert_eq!(
            key,
            "Jane Doe, John Smith/Cool Book/Cool Book [B00EXAMPLE1].m4b"
        );
    }

    #[test]
    fn conditional_series_tag() {
        // Legacy `<if series>…<end if>` syntax routed through the naming engine.
        let ctx = NamingContext {
            series: Some("S".into()),
            title: "T".into(),
            asin: "B00X".into(),
            ..Default::default()
        };
        let key = storage_key(
            &ctx,
            Some("folder"),
            Some("<if series>[<series>] <end if><title>"),
            "m4b",
        );
        assert_eq!(key, "folder/[S] T.m4b");
    }

    #[test]
    fn chapter_key_uses_engine() {
        let ctx = NamingContext {
            asin: "B00X".into(),
            title: "Book".into(),
            authors: Some("Jane Doe".into()),
            ..Default::default()
        };
        let key = chapter_storage_key(
            &ctx,
            Some("<author>/<title>"),
            Some("<ch# 0> - <chapter title>"),
            &[],
            3,
            "Intro",
            "m4b",
        );
        assert_eq!(key, "Jane Doe/Book/3 - Intro.m4b");
    }

    #[test]
    fn new_conditional_syntax() {
        let ctx = NamingContext {
            asin: "B00X".into(),
            title: "T".into(),
            is_abridged: true,
            ..Default::default()
        };
        let key = storage_key(
            &ctx,
            Some("d"),
            Some("<if abridged->[abr] <-if abridged><title>"),
            "m4b",
        );
        assert_eq!(key, "d/[abr] T.m4b");
    }
}
