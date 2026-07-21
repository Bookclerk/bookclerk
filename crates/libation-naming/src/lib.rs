//! Libation Chardonnay naming-template engine.
//!
//! This crate ports Libation's `FileManager.NamingTemplate` engine together with
//! the `LibationFileManager` template tags and formatters, giving full parity
//! with the classic / Chardonnay naming templates (property tags, conditionals,
//! and the text / number / list / name / series / date formatters).
//!
//! It also understands the legacy `libation-rs` template syntax for backward
//! compatibility: `<if series>...<end if>`, `%asin%`, and bare `<asin>` tags.
//!
//! # Example
//! ```
//! use libation_naming::{expand_template, BookContext, Contributor};
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
/// Re-exported from `libation-config` so callers can share a single rule type.
pub use libation_config::ReplacementRule;

/// Errors produced while parsing or evaluating a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamingError {
    /// The template string could not be parsed.
    Parse(String),
}

impl std::fmt::Display for NamingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NamingError::Parse(msg) => write!(f, "template parse error: {msg}"),
        }
    }
}

impl std::error::Error for NamingError {}

/// A convenient result alias for this crate.
pub type Result<T> = std::result::Result<T, NamingError>;

/// A parsed, reusable naming template.
#[derive(Debug, Clone)]
pub struct Template {
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
    libation_config::apply_replacements(input, rules)
}

/// Collapse whitespace across an ordered list of path-segment part strings,
/// mirroring Libation's `Templates.RemoveSpaces`:
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

fn or_default_rules(rules: &[ReplacementRule]) -> std::borrow::Cow<'_, [ReplacementRule]> {
    // Empty means "no replacement" — callers that want a profile must pass
    // resolved rules from `libation_config::resolve_replacement_characters`.
    std::borrow::Cow::Borrowed(rules)
}

/// Evaluate `template` into a single sanitised filename component.
///
/// Every part (literal and tag) has the replacement `rules` applied to it, then
/// whitespace is collapsed with [`remove_spaces`], matching the output of
/// Libation's `FileTemplate.GetFilename` for one path segment. Passing an empty
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
/// Literal `/` characters delimit directories (like Libation's `FolderTemplate`),
/// while `/` produced by a tag value is replaced via `rules`. Each resulting
/// segment is space-collapsed with [`remove_spaces`]; empty segments are dropped.
///
/// # Errors
/// Returns [`NamingError::Parse`] if the template is fundamentally invalid.
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
