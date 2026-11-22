//! Replays the checked-in `model_pack` fuzz corpus in PR CI (no libFuzzer
//! required), so a model description or a row pack that once broke the
//! readers stays fixed (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! `UPDATE_FUZZ_SEEDS=1` rewrites the generated seeds; without it the test
//! fails when a generated seed no longer matches its file.
// Test code: a failed assertion or a missing corpus file is the test failing.
#![allow(clippy::missing_panics_doc, clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bookclerk_model_pack::{
    encode_rows, Element, GraphSpec, LanguageTag, PackFile, PackFileName, PackManifest, Pooling,
    Rows, TableSpec, TokenizerSpec, PACK_FORMAT,
};

#[path = "../../../fuzz/oracles/model_pack.rs"]
mod oracle;

/// `fuzz/corpus/model_pack`.
fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/model_pack")
}

/// Regular files directly under the corpus directory, canonicalized and
/// confined to it.
fn corpus_files() -> Vec<PathBuf> {
    let dir = corpus_dir().canonicalize().expect("canonicalize corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fuzz corpus")
        .map(|entry| entry.expect("entry").path())
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty corpus");
    files
}

/// A file entry named `name`.
fn file(name: &str) -> PackFile {
    PackFile::describe(
        PackFileName::try_from(name.to_owned()).expect("name"),
        name.as_bytes(),
    )
}

/// A description shaped like bekko-a8m's.
fn description() -> PackManifest {
    PackManifest {
        format: PACK_FORMAT,
        model_id: "bekko-a8m".into(),
        dimensions: 384,
        max_tokens: 512,
        graph: GraphSpec {
            model: file("core.onnx"),
            ids_input: "input_ids".into(),
            mask_input: "attention_mask".into(),
            output: "last_hidden_state".into(),
            pooling: Pooling::Mean,
        },
        tokenizer: TokenizerSpec {
            model: file("tokenizer.spm.fb"),
            literal_pieces: vec!["\n".into(), "\n\n".into()],
            prefix_pieces: vec!["<bos>".into()],
            suffix_pieces: vec!["<eos>".into()],
            document_prefix: String::new(),
            query_prefix: "Query: ".into(),
        },
        tables: vec![
            TableSpec {
                input: "embeddings.tok_embeddings.weight".into(),
                element: Element::I8,
                width: 384,
            },
            TableSpec {
                input: "static_small_embeddings.rowwise_scale".into(),
                element: Element::F32,
                width: 1,
            },
        ],
        base_rows: file("base.rows"),
        languages: BTreeMap::from([(
            LanguageTag::try_from("en".to_owned()).expect("tag"),
            file("en.rows"),
        )]),
    }
}

/// A seed read as a description: the selector byte, then the JSON.
fn as_description(json: &[u8]) -> Vec<u8> {
    let mut seed = vec![0u8];
    seed.extend_from_slice(json);
    seed
}

/// A seed read as a pack of `row_bytes`-byte rows.
fn as_rows(row_bytes: u8, pack: &[u8]) -> Vec<u8> {
    let mut seed = vec![row_bytes];
    seed.extend_from_slice(pack);
    seed
}

/// The generated seeds: a description and a row pack whole, and each with
/// one rule of its layout broken.
fn seeds() -> Vec<(&'static str, Vec<u8>)> {
    let json = description().to_json().expect("json");
    let text = String::from_utf8(json.clone()).expect("utf-8");
    let pack = encode_rows(3, &[(2, b"abc"), (7, b"def"), (4_000_000_000, b"ghi")]).expect("pack");
    let mut unordered = pack.clone();
    unordered[16..20].copy_from_slice(&9u32.to_le_bytes());
    let mut huge_count = pack.clone();
    huge_count[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut bad_magic = pack.clone();
    bad_magic[6] = b'2';
    vec![
        ("description.json", as_description(&json)),
        (
            "description_parent_path.json",
            as_description(text.replace("core.onnx", "../core.onnx").as_bytes()),
        ),
        (
            "description_own_name.json",
            as_description(text.replace("en.rows", "model.json").as_bytes()),
        ),
        (
            "description_file_twice.json",
            as_description(text.replace("en.rows", "base.rows").as_bytes()),
        ),
        (
            "description_unknown_field.json",
            as_description(text.replacen('{', "{\"extra\": 1,", 1).as_bytes()),
        ),
        (
            "description_format_2.json",
            as_description(text.replace("\"format\": 1", "\"format\": 2").as_bytes()),
        ),
        (
            "description_cut.json",
            as_description(&json[..json.len() / 2]),
        ),
        ("rows.bin", as_rows(3, &pack)),
        (
            "rows_empty.bin",
            as_rows(5, &encode_rows(5, &[]).expect("pack")),
        ),
        ("rows_other_width.bin", as_rows(4, &pack)),
        ("rows_unordered.bin", as_rows(3, &unordered)),
        ("rows_huge_count.bin", as_rows(3, &huge_count)),
        ("rows_bad_magic.bin", as_rows(3, &bad_magic)),
        ("rows_cut.bin", as_rows(3, &pack[..pack.len() - 1])),
        ("rows_header_only.bin", as_rows(3, &pack[..12])),
    ]
}

#[test]
fn generated_seeds_are_current() {
    for (name, bytes) in seeds() {
        let path = corpus_dir().join(name);
        if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
            std::fs::write(&path, &bytes).expect("write seed");
        }
        assert_eq!(
            std::fs::read(&path).expect("read seed"),
            bytes,
            "{} is stale; rerun with UPDATE_FUZZ_SEEDS=1",
            path.display()
        );
    }
}

#[test]
fn fuzz_corpus_model_pack_holds_invariants() {
    let (mut descriptions, mut packs) = (0, 0);
    for path in corpus_files() {
        let data = std::fs::read(&path).expect("read");
        oracle::check(&data);
        let Some((kind, rest)) = data.split_first() else {
            continue;
        };
        if *kind == 0 {
            descriptions += usize::from(PackManifest::parse(rest).is_ok());
        } else {
            packs += usize::from(Rows::parse(rest.to_vec(), usize::from(*kind)).is_ok());
        }
    }
    // The whole files in the corpus exercise the success invariants.
    assert!(
        descriptions >= 1 && packs >= 2,
        "{descriptions} descriptions, {packs} packs read"
    );
}
