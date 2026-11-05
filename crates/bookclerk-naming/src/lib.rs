//! Port of Libation's [NamingTemplate] tag template naming engine.
//!
//! Because it derives from Libation, this crate is licensed under
//! GPL-3.0-only rather than the workspace's AGPL-3.0-or-later.
//!
//! # Audience
//!
//! Host acquire / settings code that expands folder and file templates. Guest
//! plugins do not depend on this crate.
//!
//! Upstream lives in `FileManager.NamingTemplate` (class `NamingTemplate`); the
//! book/property tags and formatters come from `LibationFileManager.Templates`.
//! This crate targets classic / Chardonnay parity for property tags,
//! conditionals, and the text / number / list / name / series / date formatters.
//!
//! Current Libation templates use `<tag->…<-tag>` conditionals. Older Libation
//! forms are still accepted for compatibility: `<if series>…<end if>`,
//! `%asin%`, and bare `<asin>` tags.
//!
//! Style: `docs/contributing/code-documentation.md`.
//!
//! [NamingTemplate]: https://github.com/rmcrackan/Libation/tree/master/Source/FileManager/NamingTemplate
//!
//! # Example
//! ```
//! use bookclerk_naming::{expand_template, BookContext, Contributor};
//!
//! let book = BookContext {
//!     isbn: "B00X".into(),
//!     title_with_subtitle: Some("A Study in Scarlet".into()),
//!     authors: vec![Contributor::new("Arthur Conan Doyle", None)],
//!     ..Default::default()
//! };
//! let out = expand_template("<author> - <title> [<id>]", &book, None).unwrap();
//! assert_eq!(out, "Arthur Conan Doyle - A Study in Scarlet [B00X]");
//! ```

// Pre-existing sites, grandfathered until fixed: https://github.com/Bookclerk/bookclerk/issues/284
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::unwrap_used
)]

mod compare;
mod context;
mod dotnet_format;
mod engine;
mod items;
mod listformat;
mod nameparser;
mod series_order;
mod tags;
mod template_string;
mod value;

pub use context::{BookContext, ChapterContext, ContentKind, Contributor, Series};
pub use engine::TemplatePart;

/// Character-replacement rule used for path sanitisation.
///
/// Re-exported from `bookclerk-config` so callers can share a single rule type.
pub use bookclerk_config::ReplacementRule;

/// Errors produced while parsing or evaluating a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamingError {
    /// The template string could not be parsed.
    Parse(String),
    /// Neither the folder template nor both file templates always write
    /// `<library id>` ([`require_library_id`]).
    MissingLibraryId,
}

impl std::fmt::Display for NamingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NamingError::Parse(msg) => write!(f, "template parse error: {msg}"),
            NamingError::MissingLibraryId => f.write_str(
                "naming templates must include <library id> in the folder template, \
                 or in both the file and chapter file templates, so two editions of \
                 a book never share a path",
            ),
        }
    }
}

impl std::error::Error for NamingError {}

/// A convenient result alias for this crate.
pub type Result<T> = std::result::Result<T, NamingError>;

/// A parsed, reusable naming template.
#[derive(Debug, Clone)]
pub struct Template {
    /// Parsed tag/literal sequence evaluated against book and optional chapter context.
    inner: engine::Template,
}

impl Template {
    /// Parse a template string.
    ///
    /// # Errors
    /// Returns [`NamingError::Parse`] if the template is fundamentally invalid.
    pub fn parse(template: &str) -> Result<Self> {
        Ok(Self {
            inner: engine::parse_template(template),
        })
    }

    /// Evaluate the template against a book (and optional chapter) context,
    /// returning the concatenated result string.
    #[must_use]
    pub fn evaluate(&self, book: &BookContext, chapter: Option<&ChapterContext>) -> String {
        self.evaluate_parts(book, chapter)
            .into_iter()
            .map(|p| p.value)
            .collect()
    }

    /// Evaluate the template into its individual [`TemplatePart`]s, preserving
    /// whether each fragment came from literal text or a resolved tag. This is
    /// useful for path building where literal `/` should create directories but
    /// tag values should not.
    #[must_use]
    pub fn evaluate_parts(
        &self,
        book: &BookContext,
        chapter: Option<&ChapterContext>,
    ) -> Vec<TemplatePart> {
        engine::evaluate_parts(&self.inner, book, chapter)
    }
}

/// Parse and evaluate `template` in one step.
///
/// # Errors
/// Returns [`NamingError::Parse`] if the template is fundamentally invalid.
pub fn expand_template(
    template: &str,
    book: &BookContext,
    chapter: Option<&ChapterContext>,
) -> Result<String> {
    Ok(Template::parse(template)?.evaluate(book, chapter))
}

/// Apply classic Libation `ReplacementCharacters` rules to `input`.
#[must_use]
pub fn apply_path_replacements(input: &str, rules: &[ReplacementRule]) -> String {
    bookclerk_config::apply_replacements(input, rules)
}

/// Collapse whitespace across an ordered list of path-segment part strings,
/// mirroring Bookclerk's `Templates.RemoveSpaces`:
///
/// * leading / trailing parts that are entirely whitespace are dropped,
/// * the first part is left-trimmed and the last part is right-trimmed,
/// * runs of spaces inside each part are collapsed to a single space, and
/// * a doubled space straddling a part boundary is collapsed.
///
/// The parts are then concatenated into the final segment string.
#[must_use]
pub fn remove_spaces(parts: &[String]) -> String {
    let mut parts: Vec<String> = parts.to_vec();

    while parts.first().is_some_and(|p| p.trim().is_empty()) {
        parts.remove(0);
    }
    while parts.last().is_some_and(|p| p.trim().is_empty()) {
        parts.pop();
    }
    if parts.is_empty() {
        return String::new();
    }

    let last = parts.len() - 1;
    parts[0] = parts[0].trim_start().to_string();
    parts[last] = parts[last].trim_end().to_string();

    for part in &mut parts {
        while part.contains("  ") {
            *part = part.replace("  ", " ");
        }
    }

    let mut i = 1;
    while i < parts.len() {
        if parts[i - 1].ends_with(' ') && parts[i].starts_with(' ') {
            parts[i] = parts[i][1..].to_string();
            if parts[i].is_empty() {
                parts.remove(i);
                continue;
            }
        }
        i += 1;
    }

    parts.concat()
}

/// Borrows `rules` as-is; an empty slice means no character replacement.
fn or_default_rules(rules: &[ReplacementRule]) -> std::borrow::Cow<'_, [ReplacementRule]> {
    // Empty means "no replacement" — callers that want a profile must pass
    // resolved rules from `bookclerk_config::resolve_replacement_characters`.
    std::borrow::Cow::Borrowed(rules)
}

/// Evaluate `template` into a single sanitised filename component.
///
/// Every part (literal and tag) has the replacement `rules` applied to it, then
/// whitespace is collapsed with [`remove_spaces`], matching the output of
/// Bookclerk's `FileTemplate.GetFilename` for one path segment. Passing an empty
/// `rules` slice performs no character replacement (useful for exact
/// tag-output assertions).
///
/// # Errors
/// Returns [`NamingError::Parse`] if the template is fundamentally invalid.
pub fn expand_filename(
    template: &str,
    book: &BookContext,
    chapter: Option<&ChapterContext>,
    rules: &[ReplacementRule],
) -> Result<String> {
    let parts = Template::parse(template)?.evaluate_parts(book, chapter);
    let strings: Vec<String> = parts
        .into_iter()
        .map(|p| apply_path_replacements(&p.value, rules))
        .collect();
    Ok(remove_spaces(&strings))
}

/// Evaluate a folder `template` into its sanitised path segments.
///
/// Literal `/` characters delimit directories (like Bookclerk's `FolderTemplate`),
/// while `/` produced by a tag value is replaced via `rules`. Each resulting
/// segment is space-collapsed with [`remove_spaces`]; empty segments are dropped.
///
/// # Errors
/// Returns [`NamingError::Parse`] if the template is fundamentally invalid.
///
/// # Panics
///
/// Panics when an internal invariant does not hold.
pub fn expand_folder(
    template: &str,
    book: &BookContext,
    chapter: Option<&ChapterContext>,
    rules: &[ReplacementRule],
) -> Result<Vec<String>> {
    let rules = or_default_rules(rules);
    let parts = Template::parse(template)?.evaluate_parts(book, chapter);

    let mut segments: Vec<Vec<String>> = vec![Vec::new()];
    for part in parts {
        if part.is_literal {
            // Literal separators create new directory segments.
            for (i, piece) in part.value.split('/').enumerate() {
                if i > 0 {
                    segments.push(Vec::new());
                }
                segments
                    .last_mut()
                    .unwrap()
                    .push(apply_path_replacements(piece, &rules));
            }
        } else {
            // Tag-produced separators are sanitised away, staying in-segment.
            segments
                .last_mut()
                .unwrap()
                .push(apply_path_replacements(&part.value, &rules));
        }
    }

    Ok(segments
        .iter()
        .map(|seg| remove_spaces(seg))
        .filter(|s| !s.is_empty())
        .collect())
}

/// Returns `(tag_name, supports_format_specifier)` for every Chardonnay property tag.
#[must_use]
pub fn property_tag_names() -> &'static [(&'static str, bool)] {
    tags::property_tags()
}

/// A `<library id>` value no metadata produces, used to see whether a
/// template always writes the tag.
const LIBRARY_ID_PROBE: &str = "0libraryid0";

/// True when `template` writes `<library id>` for every title: the tag
/// must sit outside every conditional. A conditional on another field
/// (`<has series->…<-has>`, `<!has series->…<-has>`) can be false for some
/// title, so a tag inside one does not count, whatever a sample title
/// renders.
fn always_writes_library_id(template: &str) -> Result<bool> {
    let book = BookContext {
        library_id: String::from(LIBRARY_ID_PROBE),
        ..BookContext::default()
    };
    Ok(engine::always_writes_property(
        &Template::parse(template)?.inner,
        "libraryid",
        &book,
        LIBRARY_ID_PROBE,
    ))
}

/// Checks that templates name every edition apart: the folder template, or
/// both the file and the chapter-file templates, always write
/// `<library id>`. Two editions of a title (an abridged and an unabridged
/// recording, two narrators' readings) would otherwise share one path.
///
/// # Arguments
///
/// * `folder`, `file`, `chapter_file` - The resolved folder, file and
///   chapter-file templates.
///
/// # Errors
///
/// Returns [`NamingError::MissingLibraryId`] when no such placement holds,
/// and [`NamingError::Parse`] when a template does not parse.
pub fn require_library_id(folder: &str, file: &str, chapter_file: &str) -> Result<()> {
    if always_writes_library_id(folder)?
        || (always_writes_library_id(file)? && always_writes_library_id(chapter_file)?)
    {
        return Ok(());
    }
    Err(NamingError::MissingLibraryId)
}
