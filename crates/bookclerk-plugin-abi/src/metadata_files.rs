//! The checks the host runs on what a `metadataFiles` plugin returns and on
//! what it sends one.
//!
//! A metadata-file dialect turns the bytes of a sidecar (Audiobookshelf
//! `metadata.json`, OPF, NFO, CUE, ...) into a [`MetadataFileRecord`] and
//! renders a record back into bytes. The plugin is jailed with no file or
//! network grants, but its replies are still untrusted: every record is
//! checked here before the host stores a claim from it. The checks are
//! sans-IO, so the `metadata_file_record` fuzz target and the host run the
//! same code.
//!
//! Two layers:
//!
//! - [`validate_metadata_file_record`] checks a record on its own: caps,
//!   text shape, date shape, chapter order, and that every value it carries
//!   is for a field it says the file states.
//! - [`check_record_against_dialect`] checks it against the dialect the
//!   manifest declares: a record may not state a field or credit role its
//!   dialect does not carry.

use std::collections::HashSet;

use crate::{
    Abridgement, CreditRole, ExpressionKind, MetadataField, MetadataFileParseParams,
    MetadataFileRecord, MetadataFileRenderParams, MetadataFileRendered, MAX_METADATA_AUDIO_FILES,
    MAX_METADATA_FILE_BYTES, MAX_METADATA_FILE_CHAPTERS, MAX_METADATA_FILE_DESCRIPTION_BYTES,
    MAX_METADATA_FILE_LIST_ENTRIES, MAX_METADATA_FILE_TEXT_BYTES,
};

/// Longest file name `MetadataFiles.parse` accepts, in UTF-8 bytes (the
/// usual file-system component limit).
pub const MAX_METADATA_FILE_NAME_BYTES: usize = 255;

/// Longest dialect id, in bytes; dialect ids follow the ABI identifier rule.
pub const MAX_METADATA_DIALECT_ID_BYTES: usize = 64;

/// One dialect as the plugin's manifest declares it: the fields its format
/// carries and whether it can write files. The host builds this from the
/// `[[metadata_files.dialects]]` row it routed a file to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataFileDialect {
    /// Dialect id (`[a-z][a-z0-9_]{0,63}`), unique within the plugin.
    pub id: String,
    /// Fields the format carries; a record may state only these.
    pub fields: Vec<MetadataField>,
    /// Credit roles the format carries when `fields` holds
    /// [`MetadataField::Credits`]; empty otherwise.
    pub credit_roles: Vec<CreditRole>,
    /// Whether the dialect renders files (`MetadataFiles.render`), not only
    /// reads them.
    pub writes: bool,
}

/// Why a metadata-file request or reply was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataFileError {
    /// A dialect id is not `[a-z][a-z0-9_]{0,63}`.
    BadDialectId,
    /// A file name is empty, too long, a dot name, or holds a path
    /// separator or a control character.
    BadFileName,
    /// File bytes are over [`MAX_METADATA_FILE_BYTES`] (or a rendered file
    /// is empty).
    BadFileSize(usize),
    /// A field appears twice in a field list (`present` or a render
    /// request's `fields`).
    DuplicateField(MetadataField),
    /// A credit role appears twice in `creditRoles`.
    DuplicateCreditRole(CreditRole),
    /// The record carries a value for a field missing from `present`.
    ValueNotPresent(MetadataField),
    /// A credit's role is not in `creditRoles`.
    CreditRoleNotStated(CreditRole),
    /// A list is longer than its cap.
    TooMany(&'static str),
    /// A text value is empty where it may not be, over its cap, or holds
    /// a NUL or another control character it may not.
    BadText(&'static str),
    /// `publishedAt` is not `YYYY`, `YYYY-MM` or `YYYY-MM-DD`.
    BadDate,
    /// Chapters are out of order or nest more than one level at a time.
    BadChapters,
    /// The record or request names a field the dialect does not carry.
    FieldNotCarried(MetadataField),
    /// The record names a credit role the dialect does not carry.
    CreditRoleNotCarried(CreditRole),
    /// A render request named a dialect that only reads.
    ReadOnlyDialect,
    /// A request named a dialect other than the one being checked against.
    WrongDialect,
    /// An audio file's duration is past what a JavaScript number holds
    /// exactly (2^53 - 1 ms).
    BadAudioDuration,
}

impl std::fmt::Display for MetadataFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadDialectId => f.write_str("metadata file dialect id is not an identifier"),
            Self::BadFileName => f.write_str("metadata file name is not a plain file name"),
            Self::BadFileSize(len) => write!(
                f,
                "metadata file of {len} bytes is empty or over {MAX_METADATA_FILE_BYTES}"
            ),
            Self::DuplicateField(field) => {
                write!(f, "metadata field `{}` is listed twice", field.wire_name())
            }
            Self::DuplicateCreditRole(role) => {
                write!(f, "credit role `{}` is listed twice", role.wire_name())
            }
            Self::ValueNotPresent(field) => write!(
                f,
                "metadata record carries `{}` without listing it in `present`",
                field.wire_name()
            ),
            Self::CreditRoleNotStated(role) => write!(
                f,
                "metadata record credits a `{}` without listing the role",
                role.wire_name()
            ),
            Self::TooMany(what) => write!(f, "metadata record has too many {what}"),
            Self::BadText(what) => write!(f, "metadata record `{what}` is malformed"),
            Self::BadDate => f.write_str("metadata record `publishedAt` is not a date"),
            Self::BadChapters => f.write_str("metadata record chapters are out of order"),
            Self::FieldNotCarried(field) => write!(
                f,
                "metadata field `{}` is not carried by this dialect",
                field.wire_name()
            ),
            Self::CreditRoleNotCarried(role) => write!(
                f,
                "credit role `{}` is not carried by this dialect",
                role.wire_name()
            ),
            Self::ReadOnlyDialect => f.write_str("metadata file dialect does not write files"),
            Self::WrongDialect => f.write_str("metadata file request names another dialect"),
            Self::BadAudioDuration => {
                f.write_str("metadata file audio duration is past 2^53 - 1 milliseconds")
            }
        }
    }
}

impl std::error::Error for MetadataFileError {}

/// Converts a `u32` ABI cap to `usize`, saturating on 16-bit targets.
fn cap(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// Whether `id` follows the ABI identifier rule `[a-z][a-z0-9_]{0,63}`.
///
/// # Arguments
///
/// * `id` - The dialect id to check.
///
/// # Returns
///
/// True when the id is well formed.
#[must_use]
pub fn is_dialect_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    match bytes.split_first() {
        Some((first, rest)) => {
            first.is_ascii_lowercase()
                && bytes.len() <= MAX_METADATA_DIALECT_ID_BYTES
                && rest
                    .iter()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
        }
        None => false,
    }
}

/// Whether `name` is a plain file name: non-empty, at most
/// [`MAX_METADATA_FILE_NAME_BYTES`], not `.` or `..`, and free of `/`, `\`
/// and control characters.
///
/// # Arguments
///
/// * `name` - The file name to check.
///
/// # Returns
///
/// True when the name names one file with no folder.
#[must_use]
pub fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_METADATA_FILE_NAME_BYTES
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
}

/// Checks one text value: within `max` bytes and free of control
/// characters, except tab and line breaks when `multiline`.
///
/// # Errors
///
/// [`MetadataFileError::BadText`] naming `what` when the value breaks a rule.
fn check_text(
    value: &str,
    max: usize,
    multiline: bool,
    what: &'static str,
) -> Result<(), MetadataFileError> {
    let allowed = |c: char| multiline && matches!(c, '\n' | '\r' | '\t');
    if value.len() > max || value.chars().any(|c| c.is_control() && !allowed(c)) {
        return Err(MetadataFileError::BadText(what));
    }
    Ok(())
}

/// Checks a list of labels: at most the list cap, each non-blank and a
/// single line within the text cap.
///
/// # Errors
///
/// [`MetadataFileError::TooMany`] for a long list, or
/// [`MetadataFileError::BadText`] for a bad entry.
fn check_labels(values: &[String], what: &'static str) -> Result<(), MetadataFileError> {
    if values.len() > cap(MAX_METADATA_FILE_LIST_ENTRIES) {
        return Err(MetadataFileError::TooMany(what));
    }
    for value in values {
        check_required(value, what)?;
    }
    Ok(())
}

/// Checks a value that must be a non-blank single line within the text cap.
///
/// # Errors
///
/// [`MetadataFileError::BadText`] naming `what` when the value breaks a rule.
fn check_required(value: &str, what: &'static str) -> Result<(), MetadataFileError> {
    if value.trim().is_empty() {
        return Err(MetadataFileError::BadText(what));
    }
    check_text(value, cap(MAX_METADATA_FILE_TEXT_BYTES), false, what)
}

/// Checks an optional single-line value within the text cap; `None` passes.
///
/// # Errors
///
/// [`MetadataFileError::BadText`] naming `what` when a present value breaks
/// a rule.
fn check_optional(value: Option<&String>, what: &'static str) -> Result<(), MetadataFileError> {
    value.map_or(Ok(()), |v| check_required(v, what))
}

/// Whether `date` is `YYYY`, `YYYY-MM` or `YYYY-MM-DD` with a real month
/// and a day the month can hold (29 February only in a leap year).
///
/// # Arguments
///
/// * `date` - The date text to check.
///
/// # Returns
///
/// True when the date has one of the three shapes and is a real date.
#[must_use]
pub fn is_partial_date(date: &str) -> bool {
    let parts: Vec<&str> = date.split('-').collect();
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    let number = |s: &str| s.parse::<u32>().ok();
    match parts.as_slice() {
        [y] => digits(y, 4),
        [y, m] => digits(y, 4) && digits(m, 2) && number(m).is_some_and(|m| (1..=12).contains(&m)),
        [y, m, d] if digits(y, 4) && digits(m, 2) && digits(d, 2) => {
            match (number(y), number(m), number(d)) {
                (Some(year), Some(month), Some(day)) => {
                    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
                    let last = match month {
                        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
                        4 | 6 | 9 | 11 => 30,
                        2 if leap => 29,
                        2 => 28,
                        _ => 0,
                    };
                    (1..=last).contains(&day)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// Checks that a field list names each field at most once.
///
/// # Errors
///
/// [`MetadataFileError::DuplicateField`] with the first repeated field.
fn check_unique_fields(fields: &[MetadataField]) -> Result<(), MetadataFileError> {
    let mut seen = HashSet::new();
    for field in fields {
        if !seen.insert(*field) {
            return Err(MetadataFileError::DuplicateField(*field));
        }
    }
    Ok(())
}

/// The fields a record carries a value for, in [`MetadataField`] order.
///
/// A value is anything other than the zero value: a set text, a non-empty
/// list, a known abridgement or expression kind, or a duration. Credit
/// roles count as a value for [`MetadataField::Credits`].
///
/// # Arguments
///
/// * `record` - The record to inspect.
///
/// # Returns
///
/// Every field with a value.
#[must_use]
pub fn fields_with_values(record: &MetadataFileRecord) -> Vec<MetadataField> {
    let checks = [
        (MetadataField::Title, record.title.is_some()),
        (MetadataField::Subtitle, record.subtitle.is_some()),
        (MetadataField::Description, record.description.is_some()),
        (MetadataField::Publisher, record.publisher.is_some()),
        (MetadataField::PublishedAt, record.published_at.is_some()),
        (MetadataField::Language, record.language.is_some()),
        (MetadataField::Genres, !record.genres.is_empty()),
        (MetadataField::Tags, !record.tags.is_empty()),
        (
            MetadataField::Abridgement,
            record.abridgement != Abridgement::Unknown,
        ),
        (
            MetadataField::ExpressionKind,
            record.expression_kind != ExpressionKind::Unspecified,
        ),
        (MetadataField::Identifiers, !record.identifiers.is_empty()),
        (
            MetadataField::Credits,
            !record.credits.is_empty() || !record.credit_roles.is_empty(),
        ),
        (MetadataField::Series, !record.series_entries.is_empty()),
        (MetadataField::Chapters, !record.chapters.is_empty()),
        (MetadataField::Duration, record.duration_ms.is_some()),
    ];
    checks
        .into_iter()
        .filter_map(|(field, has)| has.then_some(field))
        .collect()
}

/// Checks a record on its own, without its dialect.
///
/// The rules:
///
/// - `present` and `creditRoles` name each entry at most once, and every
///   field with a value (see [`fields_with_values`]) is in `present`;
/// - every credit's role is in `creditRoles`;
/// - lists stay within [`MAX_METADATA_FILE_LIST_ENTRIES`] and chapters within
///   [`MAX_METADATA_FILE_CHAPTERS`];
/// - text values are within [`MAX_METADATA_FILE_TEXT_BYTES`] (the
///   description within [`MAX_METADATA_FILE_DESCRIPTION_BYTES`]), labels and
///   names are non-blank single lines, and only the description may hold
///   line breaks or tabs;
/// - `publishedAt` passes [`is_partial_date`];
/// - chapter starts never decrease, the first chapter has depth 0, and each
///   chapter is at most one level deeper than the one before it.
///
/// # Arguments
///
/// * `record` - A record a dialect returned (untrusted).
///
/// # Returns
///
/// `Ok(())` when the record keeps every rule.
///
/// # Errors
///
/// The first rule the record breaks, as a [`MetadataFileError`].
pub fn validate_metadata_file_record(record: &MetadataFileRecord) -> Result<(), MetadataFileError> {
    check_unique_fields(&record.present)?;
    let mut roles = HashSet::new();
    for role in &record.credit_roles {
        if !roles.insert(*role) {
            return Err(MetadataFileError::DuplicateCreditRole(*role));
        }
    }
    for field in fields_with_values(record) {
        if !record.present.contains(&field) {
            return Err(MetadataFileError::ValueNotPresent(field));
        }
    }
    check_optional(record.title.as_ref(), "title")?;
    check_optional(record.subtitle.as_ref(), "subtitle")?;
    if let Some(description) = &record.description {
        if description.trim().is_empty() {
            return Err(MetadataFileError::BadText("description"));
        }
        check_text(
            description,
            cap(MAX_METADATA_FILE_DESCRIPTION_BYTES),
            true,
            "description",
        )?;
    }
    check_optional(record.publisher.as_ref(), "publisher")?;
    check_optional(record.language.as_ref(), "language")?;
    if let Some(date) = &record.published_at {
        if !is_partial_date(date) {
            return Err(MetadataFileError::BadDate);
        }
    }
    check_labels(&record.genres, "genres")?;
    check_labels(&record.tags, "tags")?;
    if record.identifiers.len() > cap(MAX_METADATA_FILE_LIST_ENTRIES) {
        return Err(MetadataFileError::TooMany("identifiers"));
    }
    for id in &record.identifiers {
        check_required(&id.value, "identifiers.value")?;
        check_optional(id.marketplace.as_ref(), "identifiers.marketplace")?;
    }
    if record.credits.len() > cap(MAX_METADATA_FILE_LIST_ENTRIES) {
        return Err(MetadataFileError::TooMany("credits"));
    }
    for credit in &record.credits {
        if !roles.contains(&credit.role) {
            return Err(MetadataFileError::CreditRoleNotStated(credit.role));
        }
        check_required(&credit.name, "credits.name")?;
    }
    if record.series_entries.len() > cap(MAX_METADATA_FILE_LIST_ENTRIES) {
        return Err(MetadataFileError::TooMany("series"));
    }
    for series in &record.series_entries {
        check_required(&series.name, "series.name")?;
        check_optional(series.position.as_ref(), "series.position")?;
        check_optional(series.store_series_id.as_ref(), "series.storeSeriesId")?;
    }
    if record.chapters.len() > cap(MAX_METADATA_FILE_CHAPTERS) {
        return Err(MetadataFileError::TooMany("chapters"));
    }
    let mut previous: Option<(u64, u32)> = None;
    for chapter in &record.chapters {
        check_text(
            &chapter.title,
            cap(MAX_METADATA_FILE_TEXT_BYTES),
            false,
            "chapters.title",
        )?;
        let ordered = match previous {
            None => chapter.depth == 0,
            Some((start, depth)) => {
                chapter.start_ms >= start && chapter.depth <= depth.saturating_add(1)
            }
        };
        if !ordered {
            return Err(MetadataFileError::BadChapters);
        }
        previous = Some((chapter.start_ms, chapter.depth));
    }
    Ok(())
}

/// Checks a record against the dialect that produced it: `present` names
/// only fields the dialect carries and `creditRoles` only roles it carries.
///
/// # Arguments
///
/// * `record` - A record that already passed [`validate_metadata_file_record`].
/// * `dialect` - The dialect the host routed the file to.
///
/// # Returns
///
/// `Ok(())` when the record stays inside the dialect.
///
/// # Errors
///
/// [`MetadataFileError::FieldNotCarried`] or
/// [`MetadataFileError::CreditRoleNotCarried`].
pub fn check_record_against_dialect(
    record: &MetadataFileRecord,
    dialect: &MetadataFileDialect,
) -> Result<(), MetadataFileError> {
    if let Some(field) = record
        .present
        .iter()
        .find(|field| !dialect.fields.contains(field))
    {
        return Err(MetadataFileError::FieldNotCarried(*field));
    }
    if let Some(role) = record
        .credit_roles
        .iter()
        .find(|role| !dialect.credit_roles.contains(role))
    {
        return Err(MetadataFileError::CreditRoleNotCarried(*role));
    }
    Ok(())
}

/// Checks a `MetadataFiles.parse` request before it is sent: a dialect id,
/// a plain file name, and bytes within [`MAX_METADATA_FILE_BYTES`].
///
/// # Arguments
///
/// * `params` - The request the host is about to send.
///
/// # Returns
///
/// `Ok(())` when the request is well formed.
///
/// # Errors
///
/// [`MetadataFileError::BadDialectId`], [`MetadataFileError::BadFileName`]
/// or [`MetadataFileError::BadFileSize`].
pub fn check_parse_params(params: &MetadataFileParseParams) -> Result<(), MetadataFileError> {
    if !is_dialect_id(&params.dialect) {
        return Err(MetadataFileError::BadDialectId);
    }
    if !is_plain_file_name(&params.file_name) {
        return Err(MetadataFileError::BadFileName);
    }
    if params.bytes.len() > cap(MAX_METADATA_FILE_BYTES) {
        return Err(MetadataFileError::BadFileSize(params.bytes.len()));
    }
    Ok(())
}

/// Longest audio file duration a render request carries, in milliseconds:
/// the largest integer a JavaScript number holds exactly.
const MAX_SAFE_MS: u64 = (1 << 53) - 1;

/// Checks a `MetadataFiles.render` request before it is sent: a dialect id,
/// fields named once each, existing bytes within [`MAX_METADATA_FILE_BYTES`],
/// at most [`MAX_METADATA_AUDIO_FILES`] audio files with plain names and
/// durations a JavaScript number holds exactly, and a record that passes
/// [`validate_metadata_file_record`].
///
/// # Arguments
///
/// * `params` - The request the host is about to send.
///
/// # Returns
///
/// `Ok(())` when the request is well formed.
///
/// # Errors
///
/// The first rule the request breaks.
pub fn check_render_params(params: &MetadataFileRenderParams) -> Result<(), MetadataFileError> {
    if !is_dialect_id(&params.dialect) {
        return Err(MetadataFileError::BadDialectId);
    }
    check_unique_fields(&params.fields)?;
    if params.existing.len() > cap(MAX_METADATA_FILE_BYTES) {
        return Err(MetadataFileError::BadFileSize(params.existing.len()));
    }
    if params.audio_files.len() > cap(MAX_METADATA_AUDIO_FILES) {
        return Err(MetadataFileError::TooMany("audioFiles"));
    }
    for file in &params.audio_files {
        if !is_plain_file_name(&file.name) {
            return Err(MetadataFileError::BadFileName);
        }
        if file.duration_ms > MAX_SAFE_MS {
            return Err(MetadataFileError::BadAudioDuration);
        }
    }
    validate_metadata_file_record(&params.record)
}

/// Checks a render request against the dialect it names: the dialect
/// writes, and every field to write is one it carries.
///
/// # Arguments
///
/// * `params` - A request that already passed [`check_render_params`].
/// * `dialect` - The dialect from the manifest.
///
/// # Returns
///
/// `Ok(())` when the dialect can write every requested field.
///
/// # Errors
///
/// [`MetadataFileError::WrongDialect`], [`MetadataFileError::ReadOnlyDialect`]
/// or [`MetadataFileError::FieldNotCarried`].
pub fn check_render_against_dialect(
    params: &MetadataFileRenderParams,
    dialect: &MetadataFileDialect,
) -> Result<(), MetadataFileError> {
    if params.dialect != dialect.id {
        return Err(MetadataFileError::WrongDialect);
    }
    if !dialect.writes {
        return Err(MetadataFileError::ReadOnlyDialect);
    }
    if let Some(field) = params
        .fields
        .iter()
        .find(|field| !dialect.fields.contains(field))
    {
        return Err(MetadataFileError::FieldNotCarried(*field));
    }
    Ok(())
}

/// Checks a rendered file: within [`MAX_METADATA_FILE_BYTES`]. An empty file
/// is allowed: it means the dialect has nothing left to write, and the host
/// decides whether to keep the empty file or remove it.
///
/// # Arguments
///
/// * `rendered` - A dialect's `render` reply (untrusted).
///
/// # Returns
///
/// `Ok(())` when the host may write the bytes.
///
/// # Errors
///
/// [`MetadataFileError::BadFileSize`].
pub fn check_rendered(rendered: &MetadataFileRendered) -> Result<(), MetadataFileError> {
    let len = rendered.bytes.len();
    if len > cap(MAX_METADATA_FILE_BYTES) {
        return Err(MetadataFileError::BadFileSize(len));
    }
    Ok(())
}

/// Decodes one unpacked single-segment Cap'n Proto `MetadataFileRecord`, the
/// form the fuzz target feeds.
///
/// # Arguments
///
/// * `bytes` - The message bytes (untrusted).
///
/// # Returns
///
/// The decoded record.
///
/// # Errors
///
/// Returns the Cap'n Proto error the framing or the codec reports.
pub fn decode_metadata_file_record_bytes(bytes: &[u8]) -> capnp::Result<MetadataFileRecord> {
    let words = (bytes.len() / 8).saturating_add(16);
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(words));
    opts.nesting_limit(32);
    let reader = capnp::serialize::read_message(&mut std::io::Cursor::new(bytes), opts)?;
    crate::read_metadata_file_record(
        reader.get_root::<crate::plugin_capnp::metadata_file_record::Reader<'_>>()?,
    )
}

/// Encodes `record` as an unpacked single-segment Cap'n Proto message, the
/// inverse of [`decode_metadata_file_record_bytes`].
///
/// # Arguments
///
/// * `record` - The record to encode.
///
/// # Returns
///
/// The message bytes.
///
/// # Errors
///
/// Returns the Cap'n Proto error the builder reports.
pub fn encode_metadata_file_record_bytes(record: &MetadataFileRecord) -> capnp::Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    crate::write_metadata_file_record(
        message.init_root::<crate::plugin_capnp::metadata_file_record::Builder<'_>>(),
        record,
    )?;
    Ok(capnp::serialize::write_message_to_words(&message))
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::{
        Credit, Identifier, IdentifierScheme, MetadataAudioFile, MetadataChapter, SeriesEntry,
    };

    fn dialect() -> MetadataFileDialect {
        MetadataFileDialect {
            id: "abs_metadata_json".into(),
            fields: vec![
                MetadataField::Title,
                MetadataField::Description,
                MetadataField::Credits,
                MetadataField::Chapters,
                MetadataField::PublishedAt,
            ],
            credit_roles: vec![CreditRole::Author, CreditRole::Narrator],
            writes: true,
        }
    }

    fn good() -> MetadataFileRecord {
        MetadataFileRecord {
            present: vec![
                MetadataField::Title,
                MetadataField::Credits,
                MetadataField::Chapters,
                MetadataField::Description,
            ],
            title: Some("The Hobbit".into()),
            description: Some("There and\nback again.".into()),
            credits: vec![Credit {
                name: "J. R. R. Tolkien".into(),
                role: CreditRole::Author,
                ..Credit::default()
            }],
            credit_roles: vec![CreditRole::Author, CreditRole::Narrator],
            chapters: vec![
                MetadataChapter {
                    title: "One".into(),
                    start_ms: 0,
                    depth: 0,
                    ..MetadataChapter::default()
                },
                MetadataChapter {
                    title: "One, part two".into(),
                    start_ms: 10,
                    depth: 1,
                    ..MetadataChapter::default()
                },
                MetadataChapter {
                    title: String::new(),
                    start_ms: 10,
                    depth: 0,
                    ..MetadataChapter::default()
                },
            ],
            ..MetadataFileRecord::default()
        }
    }

    #[test]
    fn a_good_record_passes_both_layers() {
        assert_eq!(validate_metadata_file_record(&good()), Ok(()));
        assert_eq!(check_record_against_dialect(&good(), &dialect()), Ok(()));
    }

    #[test]
    fn an_empty_record_is_a_file_that_states_nothing() {
        assert_eq!(
            validate_metadata_file_record(&MetadataFileRecord::default()),
            Ok(())
        );
    }

    #[test]
    fn a_stated_empty_field_needs_no_value() {
        let record = MetadataFileRecord {
            present: vec![MetadataField::Genres, MetadataField::Credits],
            credit_roles: vec![CreditRole::Narrator],
            ..MetadataFileRecord::default()
        };
        assert_eq!(validate_metadata_file_record(&record), Ok(()));
    }

    #[test]
    fn values_must_be_listed_in_present() {
        let mut record = good();
        record.present.retain(|f| *f != MetadataField::Title);
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::ValueNotPresent(MetadataField::Title))
        );
        let record = MetadataFileRecord {
            abridgement: Abridgement::Abridged,
            ..MetadataFileRecord::default()
        };
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::ValueNotPresent(
                MetadataField::Abridgement
            ))
        );
        let record = MetadataFileRecord {
            credit_roles: vec![CreditRole::Author],
            ..MetadataFileRecord::default()
        };
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::ValueNotPresent(MetadataField::Credits))
        );
    }

    #[test]
    fn duplicates_are_refused() {
        let mut record = good();
        record.present.push(MetadataField::Title);
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::DuplicateField(MetadataField::Title))
        );
        let mut record = good();
        record.credit_roles.push(CreditRole::Author);
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::DuplicateCreditRole(CreditRole::Author))
        );
    }

    #[test]
    fn credits_need_a_stated_role_and_a_name() {
        let mut record = good();
        record.credits[0].role = CreditRole::Translator;
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::CreditRoleNotStated(
                CreditRole::Translator
            ))
        );
        let mut record = good();
        record.credits[0].name = "  ".into();
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::BadText("credits.name"))
        );
    }

    #[test]
    fn text_rules() {
        let mut record = good();
        record.title = Some("a\nb".into());
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::BadText("title"))
        );
        let mut record = good();
        record.title = Some("x".repeat(cap(MAX_METADATA_FILE_TEXT_BYTES) + 1));
        assert!(validate_metadata_file_record(&record).is_err());
        let mut record = good();
        record.description = Some("x\u{0}".into());
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::BadText("description"))
        );
        let mut record = good();
        record.description = Some("x".repeat(cap(MAX_METADATA_FILE_DESCRIPTION_BYTES)));
        assert_eq!(validate_metadata_file_record(&record), Ok(()));
        record.description = Some("x".repeat(cap(MAX_METADATA_FILE_DESCRIPTION_BYTES) + 1));
        assert!(validate_metadata_file_record(&record).is_err());
        let mut record = good();
        record.present.push(MetadataField::Genres);
        record.genres = vec!["Fantasy".into(), String::new()];
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::BadText("genres"))
        );
    }

    #[test]
    fn list_caps() {
        let mut record = good();
        record.present.push(MetadataField::Tags);
        record.tags = vec!["t".into(); cap(MAX_METADATA_FILE_LIST_ENTRIES) + 1];
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::TooMany("tags"))
        );
        let mut record = good();
        record.present.push(MetadataField::Identifiers);
        record.identifiers = vec![
            Identifier {
                scheme: IdentifierScheme::Asin,
                value: "B000".into(),
                ..Identifier::default()
            };
            cap(MAX_METADATA_FILE_LIST_ENTRIES) + 1
        ];
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::TooMany("identifiers"))
        );
        let mut record = good();
        record.present.push(MetadataField::Series);
        record.series_entries = vec![SeriesEntry {
            name: String::new(),
            ..SeriesEntry::default()
        }];
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::BadText("series.name"))
        );
        let mut record = good();
        record.chapters = vec![MetadataChapter::default(); cap(MAX_METADATA_FILE_CHAPTERS) + 1];
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::TooMany("chapters"))
        );
    }

    #[test]
    fn chapter_order() {
        let mut record = good();
        record.chapters[1].start_ms = 0;
        record.chapters[2].start_ms = 0;
        assert_eq!(validate_metadata_file_record(&record), Ok(()));
        record.chapters[2].start_ms = 0;
        record.chapters[1].start_ms = 5;
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::BadChapters)
        );
        let mut record = good();
        record.chapters[0].depth = 1;
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::BadChapters)
        );
        let mut record = good();
        record.chapters[1].depth = 2;
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::BadChapters)
        );
    }

    #[test]
    fn dates() {
        for ok in ["2024", "2024-02", "2024-02-29", "2000-02-29", "1999-12-31"] {
            assert!(is_partial_date(ok), "{ok}");
        }
        for bad in [
            "",
            "24",
            "2023-02-29",
            "1900-02-29",
            "2024-13",
            "2024-00",
            "2024-1",
            "2024-04-31",
            "2024-01-00",
            "2024-01-01T00",
            "２０２４",
            "2024--01",
        ] {
            assert!(!is_partial_date(bad), "{bad}");
        }
        let mut record = good();
        record.present.push(MetadataField::PublishedAt);
        record.published_at = Some("May 2024".into());
        assert_eq!(
            validate_metadata_file_record(&record),
            Err(MetadataFileError::BadDate)
        );
    }

    #[test]
    fn dialect_layer() {
        let mut record = good();
        record.present.push(MetadataField::Tags);
        assert_eq!(
            check_record_against_dialect(&record, &dialect()),
            Err(MetadataFileError::FieldNotCarried(MetadataField::Tags))
        );
        let mut record = good();
        record.credit_roles.push(CreditRole::Cast);
        assert_eq!(
            check_record_against_dialect(&record, &dialect()),
            Err(MetadataFileError::CreditRoleNotCarried(CreditRole::Cast))
        );
    }

    #[test]
    fn identifiers_and_names() {
        assert!(is_dialect_id("abs_metadata_json"));
        assert!(is_dialect_id(&format!("a{}", "b".repeat(63))));
        for bad in ["", "Abs", "1abs", "abs-json", "a\u{e9}", &"a".repeat(65)] {
            assert!(!is_dialect_id(bad), "{bad}");
        }
        assert!(is_plain_file_name("metadata.json"));
        assert!(is_plain_file_name("Book Title (Unabridged).opf"));
        for bad in ["", ".", "..", "a/b", "a\\b", "a\nb", &"n".repeat(256)] {
            assert!(!is_plain_file_name(bad), "{bad}");
        }
    }

    #[test]
    fn parse_and_render_requests() {
        let parse = MetadataFileParseParams {
            dialect: "opf".into(),
            file_name: "book.opf".into(),
            bytes: vec![b'x'; cap(MAX_METADATA_FILE_BYTES)],
        };
        assert_eq!(check_parse_params(&parse), Ok(()));
        let too_big = MetadataFileParseParams {
            bytes: vec![b'x'; cap(MAX_METADATA_FILE_BYTES) + 1],
            ..parse.clone()
        };
        assert!(matches!(
            check_parse_params(&too_big),
            Err(MetadataFileError::BadFileSize(_))
        ));
        let bad_name = MetadataFileParseParams {
            file_name: "../book.opf".into(),
            ..parse.clone()
        };
        assert_eq!(
            check_parse_params(&bad_name),
            Err(MetadataFileError::BadFileName)
        );
        let bad_dialect = MetadataFileParseParams {
            dialect: "OPF".into(),
            ..parse
        };
        assert_eq!(
            check_parse_params(&bad_dialect),
            Err(MetadataFileError::BadDialectId)
        );

        let render = MetadataFileRenderParams {
            dialect: "abs_metadata_json".into(),
            record: good(),
            fields: vec![MetadataField::Title, MetadataField::Credits],
            existing: Vec::new(),
            audio_files: vec![MetadataAudioFile {
                name: "Part 1.mp3".into(),
                duration_ms: 3_600_000,
            }],
        };
        assert_eq!(check_render_params(&render), Ok(()));
        let pathed = MetadataFileRenderParams {
            audio_files: vec![MetadataAudioFile {
                name: "../x.mp3".into(),
                duration_ms: 0,
            }],
            ..render.clone()
        };
        assert_eq!(
            check_render_params(&pathed),
            Err(MetadataFileError::BadFileName)
        );
        let long = MetadataFileRenderParams {
            audio_files: vec![MetadataAudioFile {
                name: "a.mp3".into(),
                duration_ms: 1 << 53,
            }],
            ..render.clone()
        };
        assert_eq!(
            check_render_params(&long),
            Err(MetadataFileError::BadAudioDuration)
        );
        let many = MetadataFileRenderParams {
            audio_files: vec![MetadataAudioFile::default(); cap(MAX_METADATA_AUDIO_FILES) + 1],
            ..render.clone()
        };
        assert_eq!(
            check_render_params(&many),
            Err(MetadataFileError::TooMany("audioFiles"))
        );
        assert_eq!(check_render_against_dialect(&render, &dialect()), Ok(()));
        let twice = MetadataFileRenderParams {
            fields: vec![MetadataField::Title, MetadataField::Title],
            ..render.clone()
        };
        assert_eq!(
            check_render_params(&twice),
            Err(MetadataFileError::DuplicateField(MetadataField::Title))
        );
        let big = MetadataFileRenderParams {
            existing: vec![0; cap(MAX_METADATA_FILE_BYTES) + 1],
            ..render.clone()
        };
        assert!(check_render_params(&big).is_err());
        let uncarried = MetadataFileRenderParams {
            fields: vec![MetadataField::Tags],
            ..render.clone()
        };
        assert_eq!(
            check_render_against_dialect(&uncarried, &dialect()),
            Err(MetadataFileError::FieldNotCarried(MetadataField::Tags))
        );
        let read_only = MetadataFileDialect {
            writes: false,
            ..dialect()
        };
        assert_eq!(
            check_render_against_dialect(&render, &read_only),
            Err(MetadataFileError::ReadOnlyDialect)
        );
        let other = MetadataFileRenderParams {
            dialect: "opf".into(),
            ..render
        };
        assert_eq!(
            check_render_against_dialect(&other, &dialect()),
            Err(MetadataFileError::WrongDialect)
        );
    }

    #[test]
    fn rendered_size() {
        let ok = MetadataFileRendered { bytes: vec![b'{'] };
        assert_eq!(check_rendered(&ok), Ok(()));
        let empty = MetadataFileRendered { bytes: Vec::new() };
        assert_eq!(check_rendered(&empty), Ok(()));
        let big = MetadataFileRendered {
            bytes: vec![0; cap(MAX_METADATA_FILE_BYTES) + 1],
        };
        assert!(check_rendered(&big).is_err());
    }

    #[test]
    fn records_round_trip_through_bytes() {
        let bytes = encode_metadata_file_record_bytes(&good()).expect("encode");
        assert_eq!(
            decode_metadata_file_record_bytes(&bytes).expect("decode"),
            good()
        );
    }

    #[test]
    fn errors_read_well() {
        for err in [
            MetadataFileError::BadDialectId,
            MetadataFileError::BadFileName,
            MetadataFileError::BadFileSize(3),
            MetadataFileError::DuplicateField(MetadataField::Title),
            MetadataFileError::DuplicateCreditRole(CreditRole::Author),
            MetadataFileError::ValueNotPresent(MetadataField::Title),
            MetadataFileError::CreditRoleNotStated(CreditRole::Author),
            MetadataFileError::TooMany("tags"),
            MetadataFileError::BadText("title"),
            MetadataFileError::BadDate,
            MetadataFileError::BadChapters,
            MetadataFileError::FieldNotCarried(MetadataField::Tags),
            MetadataFileError::CreditRoleNotCarried(CreditRole::Cast),
            MetadataFileError::ReadOnlyDialect,
            MetadataFileError::WrongDialect,
            MetadataFileError::BadAudioDuration,
        ] {
            assert!(!err.to_string().is_empty());
        }
    }
}
