//! Shared oracle for the `model_pack` fuzz target and the
//! `bookclerk-model-pack` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The files of a model directory are downloaded, so the embedder plugin
//! reads them as untrusted bytes. The first input byte picks what the rest
//! is read as:
//!
//! - `0`: a model description (`model.json`). An accepted description
//!   names only files that stay inside the model directory by a rule
//!   written apart from the crate's, each once and none the description's
//!   own name; its limits hold (rows of 1 to [`MAX_ROW_BYTES`] bytes, room
//!   for text after the framing pieces); and it writes back to JSON that
//!   reads as the same description.
//! - anything else: a row pack whose rows have that many bytes. The
//!   verdict equals a layout check written apart from the crate's; an
//!   accepted pack gives every listed token exactly the bytes at the
//!   position the layout states, gives nothing for a token it does not
//!   list, and re-encodes to exactly its own bytes.

use std::collections::BTreeSet;

use bookclerk_model_pack::{
    encode_rows, PackFile, PackManifest, Rows, MANIFEST_FILE, MAX_ROWS, MAX_ROW_BYTES,
};

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Whether `name`, joined to a model directory, stays a file directly in
/// it on every platform the plugin runs on.
fn stays_inside(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name
            .chars()
            .any(|c| matches!(c, '/' | '\\' | ':' | '\0') || c.is_control())
}

/// Checks an accepted description.
fn check_manifest(manifest: &PackManifest, source: &[u8]) {
    let mut files: Vec<&PackFile> = vec![
        &manifest.graph.model,
        &manifest.tokenizer.model,
        &manifest.base_rows,
    ];
    files.extend(manifest.languages.values());
    let mut names = BTreeSet::new();
    for file in files {
        let name = file.file.as_str();
        assert!(
            stays_inside(name),
            "file name {name:?} leaves the directory"
        );
        assert_ne!(name, MANIFEST_FILE, "a file takes the description's name");
        assert!(names.insert(name), "file {name:?} listed twice");
    }
    assert!(
        (1..=MAX_ROW_BYTES).contains(&manifest.row_bytes()),
        "row of {} bytes",
        manifest.row_bytes()
    );
    assert!(manifest.dimensions > 0, "no dimensions");
    let framing = manifest.tokenizer.prefix_pieces.len() + manifest.tokenizer.suffix_pieces.len();
    assert!(
        u64::try_from(framing).expect("fits") < u64::from(manifest.max_tokens),
        "no room for text"
    );
    let written = manifest.to_json().expect("accepted description writes");
    let again = PackManifest::parse(&written).expect("written description reads");
    assert_eq!(&again, manifest, "description changed in a round trip");
    assert!(
        std::str::from_utf8(source).is_ok(),
        "accepted description is not UTF-8"
    );
}

/// The layout of a row pack, restated: the token ids and where the rows
/// start, or `None` when `bytes` is not a pack of `row_bytes`-byte rows.
fn layout(bytes: &[u8], row_bytes: usize) -> Option<(Vec<u32>, usize)> {
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    if bytes.get(..8)? != b"BCROWS1\0" {
        return None;
    }
    let count = usize::try_from(word(8)?).ok()?;
    let width = usize::try_from(word(12)?).ok()?;
    if count > MAX_ROWS || width == 0 || width > MAX_ROW_BYTES || width != row_bytes {
        return None;
    }
    let rows_at = 16usize.checked_add(count.checked_mul(4)?)?;
    if bytes.len() != rows_at.checked_add(count.checked_mul(width)?)? {
        return None;
    }
    let ids: Vec<u32> = (0..count)
        .map(|i| word(16 + i * 4))
        .collect::<Option<_>>()?;
    ids.windows(2)
        .all(|pair| pair[0] < pair[1])
        .then_some((ids, rows_at))
}

/// Checks the row-pack reader on `bytes` for rows of `row_bytes` bytes.
fn check_rows(bytes: &[u8], row_bytes: usize) {
    let expected = layout(bytes, row_bytes);
    let Ok(rows) = Rows::parse(bytes.to_vec(), row_bytes) else {
        assert!(expected.is_none(), "a well-formed pack was refused");
        return;
    };
    let (ids, rows_at) = expected.expect("a malformed pack was accepted");
    assert_eq!(rows.len(), ids.len());
    assert_eq!(rows.is_empty(), ids.is_empty());
    assert_eq!(rows.row_bytes(), row_bytes);
    assert_eq!(rows.ids().collect::<Vec<_>>(), ids);
    let mut listed = Vec::with_capacity(ids.len());
    for (index, id) in ids.iter().enumerate() {
        let start = rows_at + index * row_bytes;
        let row = rows.row(*id).expect("listed token has a row");
        assert_eq!(row, &bytes[start..start + row_bytes], "wrong row for {id}");
        listed.push((*id, row));
        for other in [id.wrapping_sub(1), id.wrapping_add(1)] {
            assert_eq!(
                rows.row(other).is_some(),
                ids.binary_search(&other).is_ok(),
                "row for unlisted token {other}"
            );
        }
    }
    assert_eq!(
        encode_rows(row_bytes, &listed).expect("accepted pack re-encodes"),
        bytes,
        "pack changed in a round trip"
    );
}

/// Runs the oracle on one input.
///
/// # Panics
///
/// When the reader breaks one of the rules in the module documentation.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((kind, rest)) = data.split_first() else {
        return;
    };
    if *kind == 0 {
        if let Ok(manifest) = PackManifest::parse(rest) {
            check_manifest(&manifest, rest);
        }
    } else {
        check_rows(rest, usize::from(*kind));
    }
}
