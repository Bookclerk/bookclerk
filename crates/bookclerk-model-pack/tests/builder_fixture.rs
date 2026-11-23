//! The readers accept what the pack builder writes.
//!
//! `tests/fixtures/builder-model.json` and `builder-base.rows` are written
//! by `scripts/model_packs.py`; `scripts/tests/test_model_packs.py` fails
//! when the builder would write other bytes. Here the same files go
//! through the readers the plugin uses.
// Test code: a failed assertion or a bad fixture is the test failing.
#![allow(clippy::missing_panics_doc, clippy::expect_used, clippy::unwrap_used)]

use bookclerk_model_pack::{Element, PackManifest, Pooling, Rows};

/// The model description the builder wrote.
const MODEL_JSON: &[u8] = include_bytes!("fixtures/builder-model.json");
/// The row pack the builder wrote: six rows of six bytes.
const BASE_ROWS: &[u8] = include_bytes!("fixtures/builder-base.rows");

#[test]
fn the_builders_model_description_parses_and_writes_back_unchanged() {
    let manifest = PackManifest::parse(MODEL_JSON).expect("the builder's model.json parses");
    assert_eq!(manifest.model_id, "tiny");
    assert_eq!((manifest.dimensions, manifest.max_tokens), (4, 16));
    assert_eq!(manifest.graph.pooling, Pooling::Mean);
    assert_eq!(manifest.tokenizer.literal_pieces, ["\n", "<br>"]);
    assert_eq!(manifest.tokenizer.query_prefix, "Query: ");
    assert_eq!(
        manifest
            .tables
            .iter()
            .map(|table| (table.element, table.width))
            .collect::<Vec<_>>(),
        [(Element::I8, 2), (Element::F32, 1)]
    );
    assert!(manifest.languages.is_empty());
    // The builder and this crate write the same bytes, so a digest of
    // `model.json` means the same whichever wrote it.
    assert_eq!(manifest.to_json().expect("writes"), MODEL_JSON);
}

#[test]
fn the_builders_row_pack_holds_the_rows_the_description_lists() {
    let manifest = PackManifest::parse(MODEL_JSON).expect("parses");
    manifest
        .base_rows
        .verify(BASE_ROWS)
        .expect("the description lists this file");
    let rows = Rows::parse(BASE_ROWS.to_vec(), manifest.row_bytes()).expect("the row pack parses");
    assert_eq!(rows.len(), 6);
    assert_eq!(rows.ids().collect::<Vec<_>>(), [0, 1, 2, 3, 4, 5]);
    assert_eq!(rows.row(0), Some(&[0u8, 1, 2, 3, 4, 5][..]));
    assert_eq!(rows.row(5), Some(&[30u8, 31, 32, 33, 34, 35][..]));
    assert_eq!(rows.row(6), None);
}
