//! Shared oracle for the `metadata_file_record` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one unpacked `MetadataFileRecord` message, what a
//! metadata-file dialect says a sidecar holds. The invariants:
//!
//! - decoding and validation never panic;
//! - [`validate_metadata_file_record`] accepts the record exactly when the
//!   independent restatement below finds every rule kept;
//! - [`check_record_against_dialect`] accepts an accepted record exactly
//!   when it states only the fields and credit roles of a fixed dialect;
//! - an accepted record re-encodes, decodes back equal, and is accepted again.

use std::collections::BTreeSet;

use bookclerk_plugin_abi::{
    check_record_against_dialect, decode_metadata_file_record_bytes,
    encode_metadata_file_record_bytes, validate_metadata_file_record, Abridgement, CreditRole,
    Explicitness, ExpressionKind, MetadataField, MetadataFileDialect, MetadataFileRecord,
};

/// Largest input worth decoding; the RPC caps a record far below this.
pub const MAX_INPUT_BYTES: usize = 512 * 1024;

/// Description cap in bytes (`maxMetadataFileDescriptionBytes`).
const DESCRIPTION: usize = 65_536;
/// Other text cap in bytes (`maxMetadataFileTextBytes`).
const TEXT: usize = 1_024;
/// List cap (`maxMetadataFileListEntries`).
const LIST: usize = 256;
/// Chapter cap (`maxMetadataFileChapters`).
const CHAPTERS: usize = 2_000;

/// The dialect every accepted record is also checked against: an
/// Audiobookshelf-like `metadata.json`.
fn dialect() -> MetadataFileDialect {
    MetadataFileDialect {
        id: "abs_metadata_json".into(),
        fields: vec![
            MetadataField::Title,
            MetadataField::Subtitle,
            MetadataField::Description,
            MetadataField::Publisher,
            MetadataField::PublishedAt,
            MetadataField::Language,
            MetadataField::Genres,
            MetadataField::Tags,
            MetadataField::Abridgement,
            MetadataField::Explicitness,
            MetadataField::Identifiers,
            MetadataField::Credits,
            MetadataField::Series,
            MetadataField::Chapters,
        ],
        credit_roles: vec![CreditRole::Author, CreditRole::Narrator],
        writes: true,
    }
}

/// A single-line value: no control character at all.
fn line(value: &str, max: usize) -> bool {
    value.len() <= max && !value.chars().any(char::is_control)
}

/// A required label: non-blank single line within the text cap.
fn label(value: &str) -> bool {
    !value.trim().is_empty() && line(value, TEXT)
}

/// A date as `YYYY`, `YYYY-MM` or `YYYY-MM-DD`, checked through a calendar
/// walk rather than a month-length table.
fn date(value: &str) -> bool {
    let b = value.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    let num = |r: std::ops::Range<usize>| -> u32 {
        b[r].iter().fold(0, |n, d| n * 10 + u32::from(d - b'0'))
    };
    match b.len() {
        4 => digits(0..4),
        7 => digits(0..4) && b[4] == b'-' && digits(5..7) && (1..=12).contains(&num(5..7)),
        10 => {
            if !(digits(0..4) && b[4] == b'-' && digits(5..7) && b[7] == b'-' && digits(8..10)) {
                return false;
            }
            let (year, month, day) = (num(0..4), num(5..7), num(8..10));
            if !(1..=12).contains(&month) || day == 0 {
                return false;
            }
            // The day exists when it is not past the day before the 1st of
            // the next month: step the days of a month by trying 28..=31.
            let leap = year % 400 == 0 || (year % 4 == 0 && year % 100 != 0);
            let days = (28..=31)
                .filter(|d| match (month, d) {
                    (2, 29) => leap,
                    (2, 30 | 31) => false,
                    (4 | 6 | 9 | 11, 31) => false,
                    _ => true,
                })
                .max()
                .unwrap_or(28);
            day <= days
        }
        _ => false,
    }
}

/// The record rules restated directly: true when the record keeps them all.
fn keeps_every_rule(r: &MetadataFileRecord) -> bool {
    let present: BTreeSet<_> = r.present.iter().map(|f| f.wire_name()).collect();
    if present.len() != r.present.len() {
        return false;
    }
    let roles: BTreeSet<_> = r.credit_roles.iter().map(|c| c.wire_name()).collect();
    if roles.len() != r.credit_roles.len() {
        return false;
    }
    let stated = |field: MetadataField| present.contains(field.wire_name());
    let has_values = [
        (MetadataField::Title, r.title.is_some()),
        (MetadataField::Subtitle, r.subtitle.is_some()),
        (MetadataField::Description, r.description.is_some()),
        (MetadataField::Publisher, r.publisher.is_some()),
        (MetadataField::PublishedAt, r.published_at.is_some()),
        (MetadataField::Language, r.language.is_some()),
        (MetadataField::Genres, !r.genres.is_empty()),
        (MetadataField::Tags, !r.tags.is_empty()),
        (
            MetadataField::Abridgement,
            r.abridgement != Abridgement::Unknown,
        ),
        (
            MetadataField::ExpressionKind,
            r.expression_kind != ExpressionKind::Unspecified,
        ),
        (MetadataField::Identifiers, !r.identifiers.is_empty()),
        (
            MetadataField::Credits,
            !r.credits.is_empty() || !r.credit_roles.is_empty(),
        ),
        (MetadataField::Series, !r.series_entries.is_empty()),
        (MetadataField::Chapters, !r.chapters.is_empty()),
        (MetadataField::Duration, r.duration_ms.is_some()),
        (
            MetadataField::Explicitness,
            r.explicitness != Explicitness::Unknown,
        ),
    ];
    if has_values
        .iter()
        .any(|(field, has)| *has && !stated(*field))
    {
        return false;
    }
    let optional = |v: &Option<String>| v.as_deref().is_none_or(label);
    let description_ok = r.description.as_deref().is_none_or(|d| {
        !d.trim().is_empty()
            && d.len() <= DESCRIPTION
            && d.chars()
                .all(|c| !c.is_control() || c == '\n' || c == '\r' || c == '\t')
    });
    let mut previous: Option<(u64, u32)> = None;
    let chapters_ok = r.chapters.len() <= CHAPTERS
        && r.chapters.iter().all(|c| {
            let ok = line(&c.title, TEXT)
                && previous.is_none_or(|(start, depth)| {
                    c.start_ms >= start && u64::from(c.depth) <= u64::from(depth) + 1
                })
                && (previous.is_some() || c.depth == 0);
            previous = Some((c.start_ms, c.depth));
            ok
        });
    optional(&r.title)
        && optional(&r.subtitle)
        && description_ok
        && optional(&r.publisher)
        && optional(&r.language)
        && r.published_at.as_deref().is_none_or(date)
        && r.genres.len() <= LIST
        && r.genres.iter().all(|g| label(g))
        && r.tags.len() <= LIST
        && r.tags.iter().all(|t| label(t))
        && r.identifiers.len() <= LIST
        && r.identifiers
            .iter()
            .all(|i| label(&i.value) && optional(&i.marketplace))
        && r.credits.len() <= LIST
        && r.credits
            .iter()
            .all(|c| roles.contains(c.role.wire_name()) && label(&c.name))
        && r.series_entries.len() <= LIST
        && r.series_entries
            .iter()
            .all(|s| label(&s.name) && optional(&s.position) && optional(&s.store_series_id))
        && chapters_ok
}

/// Runs every invariant on one input; panics on a violation.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Ok(record) = decode_metadata_file_record_bytes(data) else {
        return;
    };
    let verdict = validate_metadata_file_record(&record);
    assert_eq!(
        verdict.is_ok(),
        keeps_every_rule(&record),
        "validation disagrees with the oracle: {verdict:?}"
    );
    if verdict.is_err() {
        return;
    }
    let dialect = dialect();
    let inside = record.present.iter().all(|f| dialect.fields.contains(f))
        && record
            .credit_roles
            .iter()
            .all(|r| dialect.credit_roles.contains(r));
    let against = check_record_against_dialect(&record, &dialect);
    assert_eq!(
        against.is_ok(),
        inside,
        "dialect check disagrees with the oracle: {against:?}"
    );
    let bytes = encode_metadata_file_record_bytes(&record)
        .unwrap_or_else(|err| panic!("an accepted record must encode: {err}"));
    let again = decode_metadata_file_record_bytes(&bytes)
        .unwrap_or_else(|err| panic!("an encoded record must decode: {err}"));
    assert_eq!(again, record, "an accepted record round-trips");
    assert!(validate_metadata_file_record(&again).is_ok());
}
