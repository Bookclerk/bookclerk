//! A tiny model directory for tests of the model plugin.
//!
//! Shared by this crate's tests and by the staged-plugin suite in
//! `bookclerk-plugin-e2e`, which includes this file by path. The model is
//! `tests/fixtures/tiny.onnx` (see `make_fixture.py` beside it) over the
//! tokenizer fixture of `bookclerk-sentencepiece`. Its token tables are a
//! formula ([`weight`], [`scale`]), so a test can compute the vector the
//! model must give for a list of token ids ([`expected`]) without running
//! any of the plugin's code.

// Each test binary uses its own part of this file.
#![allow(dead_code)]
// Test support: a failed write or a missing fixture is the test failing,
// and the casts are over the fixture's own small, known numbers.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bookclerk_model_pack::{
    encode_rows, Element, GraphSpec, LanguageTag, PackFile, PackFileName, PackManifest, Pooling,
    TableSpec, TokenizerSpec, MANIFEST_FILE, PACK_FORMAT,
};
use bookclerk_sentencepiece::Tokenizer;

/// The test graph.
const GRAPH: &[u8] = include_bytes!("../fixtures/tiny.onnx");
/// The 640-piece tokenizer fixture.
const TOKENIZER: &[u8] =
    include_bytes!("../../../../../bookclerk-sentencepiece/tests/fixtures/tiny.spm.fb");
/// Pieces in the tokenizer fixture.
const VOCAB: u32 = 640;
/// Numbers per vector, and signed bytes per table row.
pub const WIDTH: usize = 8;
/// The piece SentencePiece puts where a word starts.
pub const BOUNDARY: &str = "\u{2581}";

/// Table value of token `id` at column `column`.
pub fn weight(id: u32, column: usize) -> i8 {
    (((id as usize * 31 + column * 17) % 251) as i32 - 125) as i8
}

/// Scale of token `id`.
pub fn scale(id: u32) -> f32 {
    0.01 + (id % 7) as f32 * 0.005
}

/// The row-pack row of token `id`: eight signed bytes, then the scale.
fn row(id: u32) -> Vec<u8> {
    let mut row: Vec<u8> = (0..WIDTH)
        .map(|column| weight(id, column).to_le_bytes()[0])
        .collect();
    row.extend_from_slice(&scale(id).to_le_bytes());
    row
}

/// The vector the tiny model gives for `ids`: the mean of the scaled rows,
/// scaled to length one.
pub fn expected(ids: &[u32]) -> Vec<f32> {
    let mut mean = [0f64; WIDTH];
    for id in ids {
        for (column, sum) in mean.iter_mut().enumerate() {
            *sum += f64::from(f32::from(weight(*id, column)) * scale(*id));
        }
    }
    let norm = mean
        .iter()
        .map(|sum| (sum / ids.len() as f64).powi(2))
        .sum::<f64>()
        .sqrt();
    mean.iter()
        .map(|sum| (sum / ids.len() as f64 / norm) as f32)
        .collect()
}

/// The tokenizer fixture, loaded.
pub fn tokenizer() -> Tokenizer {
    Tokenizer::from_bytes(TOKENIZER.to_vec()).unwrap()
}

/// Whether the base rows hold token `id`: special tokens, the word
/// boundary, single characters and byte pieces.
pub fn in_base(tokenizer: &Tokenizer, id: u32) -> bool {
    let piece = tokenizer.id_to_piece(id).unwrap();
    id < 4 || piece.chars().count() == 1 || (piece.starts_with("<0x") && piece.ends_with('>'))
}

/// A pack file name.
fn name(text: &str) -> PackFileName {
    PackFileName::try_from(text.to_owned()).unwrap()
}

/// A language tag.
fn tag(text: &str) -> LanguageTag {
    LanguageTag::try_from(text.to_owned()).unwrap()
}

/// A row pack with the formula's rows for `ids`.
fn pack(ids: impl Iterator<Item = u32>) -> Vec<u8> {
    let rows: Vec<(u32, Vec<u8>)> = ids.map(|id| (id, row(id))).collect();
    let borrowed: Vec<(u32, &[u8])> = rows.iter().map(|(id, row)| (*id, row.as_slice())).collect();
    encode_rows(WIDTH + 4, &borrowed).unwrap()
}

/// A model directory under `models_dir`.
///
/// The `even` language pack holds every token with an even id that is not
/// in the base rows, `odd` the others; with both, no token is missing.
pub struct ModelDir {
    /// The model directory.
    pub dir: PathBuf,
    /// The `even` language pack, not yet written.
    even: Vec<u8>,
    /// The `odd` language pack, not yet written.
    odd: Vec<u8>,
}

/// Writes a model directory with its base rows and no language pack.
///
/// `pooling` picks the graph output: per-token vectors the plugin averages,
/// or the graph's own average.
pub fn write_model(
    models_dir: &Path,
    model_id: &str,
    pooling: Pooling,
    query_prefix: &str,
) -> ModelDir {
    let tokenizer = tokenizer();
    let base = pack((0..VOCAB).filter(|id| in_base(&tokenizer, *id)));
    let even = pack((0..VOCAB).filter(|id| !in_base(&tokenizer, *id) && id % 2 == 0));
    let odd = pack((0..VOCAB).filter(|id| !in_base(&tokenizer, *id) && id % 2 == 1));
    let manifest = PackManifest {
        format: PACK_FORMAT,
        model_id: model_id.to_owned(),
        dimensions: WIDTH as u32,
        max_tokens: 32,
        graph: GraphSpec {
            model: PackFile::describe(name("core.onnx"), GRAPH),
            ids_input: "input_ids".into(),
            mask_input: "attention_mask".into(),
            output: match pooling {
                Pooling::Mean => "last_hidden_state".into(),
                Pooling::None => "sentence_embedding".into(),
            },
            pooling,
        },
        tokenizer: TokenizerSpec {
            model: PackFile::describe(name("tokenizer.spm.fb"), TOKENIZER),
            literal_pieces: Vec::new(),
            prefix_pieces: vec!["<s>".into()],
            suffix_pieces: vec!["</s>".into()],
            document_prefix: String::new(),
            query_prefix: query_prefix.to_owned(),
        },
        tables: vec![
            TableSpec {
                input: "tok.weight".into(),
                element: Element::I8,
                width: WIDTH as u32,
            },
            TableSpec {
                input: "tok.scale".into(),
                element: Element::F32,
                width: 1,
            },
        ],
        base_rows: PackFile::describe(name("base.rows"), &base),
        languages: BTreeMap::from([
            (tag("even"), PackFile::describe(name("even.rows"), &even)),
            (tag("odd"), PackFile::describe(name("odd.rows"), &odd)),
        ]),
    };
    let dir = models_dir.join(model_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(MANIFEST_FILE), manifest.to_json().unwrap()).unwrap();
    std::fs::write(dir.join("core.onnx"), GRAPH).unwrap();
    std::fs::write(dir.join("tokenizer.spm.fb"), TOKENIZER).unwrap();
    std::fs::write(dir.join("base.rows"), base).unwrap();
    ModelDir { dir, even, odd }
}

impl ModelDir {
    /// Writes the `even` language pack into the directory.
    pub fn add_even(&self) {
        std::fs::write(self.dir.join("even.rows"), &self.even).unwrap();
    }

    /// Writes the `odd` language pack into the directory.
    pub fn add_odd(&self) {
        std::fs::write(self.dir.join("odd.rows"), &self.odd).unwrap();
    }
}

/// The ids the published tokenizer pipeline gives for `text`: the framing
/// pieces around the tokenizer's own ids, cut at the model's 32 tokens.
pub fn framed(text: &str) -> Vec<u32> {
    let mut ids = vec![2];
    ids.extend(tokenizer().encode(text, 30).unwrap().ids);
    ids.push(1);
    ids
}

/// Asserts two vectors are equal to within float rounding.
pub fn assert_close(found: &[f32], wanted: &[f32]) {
    assert_eq!(found.len(), wanted.len());
    for (a, b) in found.iter().zip(wanted) {
        assert!((a - b).abs() < 1e-5, "{found:?} != {wanted:?}");
    }
}
