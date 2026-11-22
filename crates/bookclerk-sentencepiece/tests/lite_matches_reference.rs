//! The Lite runtime gives the ids full SentencePiece gave for the fixture
//! model (`tests/fixtures/make_fixture.py` recorded them).
// Test code: a failed read or parse is the test failing.
#![allow(clippy::expect_used, clippy::missing_panics_doc)]

use bookclerk_sentencepiece::{SpecialPiece, Tokenizer, TokenizerError};

/// The fixture model: a 640-piece BPE model with byte fallback.
fn tiny() -> Tokenizer {
    Tokenizer::from_bytes(include_bytes!("fixtures/tiny.spm.fb").to_vec()).expect("fixture loads")
}

/// The ids the reference tokenizer recorded.
fn reference() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/tiny.ids.json")).expect("fixture json")
}

/// A JSON number as `u32`.
fn as_u32(value: &serde_json::Value) -> u32 {
    u32::try_from(value.as_u64().expect("number")).expect("fits u32")
}

#[test]
fn ids_match_full_sentencepiece() {
    let tokenizer = tiny();
    let reference = reference();
    let texts = reference["texts"].as_array().expect("texts");
    assert!(texts.len() >= 10);
    for case in texts {
        let text = case["text"].as_str().expect("text");
        let want: Vec<u32> = case["ids"]
            .as_array()
            .expect("ids")
            .iter()
            .map(as_u32)
            .collect();
        let got = tokenizer.encode(text, 1024).expect("encodes");
        assert_eq!(got.ids, want, "ids differ for {text:?}");
        assert_eq!(got.total, want.len());
        assert!(!got.truncated());
    }
}

#[test]
fn the_limit_cuts_ids_and_reports_the_total() {
    let tokenizer = tiny();
    let whole = tokenizer.encode(&"x".repeat(300), 1024).expect("encodes");
    let cut = tokenizer.encode(&"x".repeat(300), 5).expect("encodes");
    assert_eq!(cut.ids, whole.ids[..5]);
    assert_eq!(cut.total, whole.total);
    assert!(cut.truncated());
    let none = tokenizer.encode("anything", 0).expect("encodes");
    assert!(none.ids.is_empty());
    assert!(none.total > 0);
}

#[test]
fn special_ids_and_size_match_the_model() {
    let tokenizer = tiny();
    let reference = reference();
    assert_eq!(
        tokenizer.vocab_size(),
        usize::try_from(as_u32(&reference["vocab_size"])).expect("fits")
    );
    let special = &reference["special"];
    for (piece, name) in [
        (SpecialPiece::Padding, "padding"),
        (SpecialPiece::End, "end"),
        (SpecialPiece::Begin, "begin"),
        (SpecialPiece::Unknown, "unknown"),
    ] {
        assert_eq!(
            tokenizer.special_id(piece),
            Some(as_u32(&special[name])),
            "{name}"
        );
    }
}

#[test]
fn pieces_round_trip_by_text() {
    let tokenizer = tiny();
    let size = u32::try_from(tokenizer.vocab_size()).expect("fits");
    for id in 0..size {
        let piece = tokenizer.id_to_piece(id).expect("piece text");
        assert_eq!(tokenizer.piece_to_id(piece), Some(id), "{piece:?}");
    }
    assert_eq!(tokenizer.id_to_piece(size), None);
    assert_eq!(tokenizer.id_to_piece(u32::MAX), None);
    assert_eq!(tokenizer.piece_to_id("no such piece in the model"), None);
    // Byte fallback: every byte has a piece.
    assert!(tokenizer.piece_to_id("<0xE3>").is_some());
}

#[test]
fn bytes_that_are_not_a_model_are_refused() {
    for bytes in [
        Vec::new(),
        vec![0_u8; 64],
        b"not a flatbuffer".to_vec(),
        include_bytes!("fixtures/tiny.spm.fb")[..200].to_vec(),
    ] {
        assert_eq!(
            Tokenizer::from_bytes(bytes).err(),
            Some(TokenizerError::InvalidModel)
        );
    }
}

#[test]
fn a_tokenizer_moves_between_threads() {
    let tokenizer = tiny();
    let here = tokenizer
        .encode("Dragons and a stolen ring", 64)
        .expect("encodes");
    let there = std::thread::spawn(move || tokenizer.encode("Dragons and a stolen ring", 64))
        .join()
        .expect("thread")
        .expect("encodes");
    assert_eq!(here, there);
}
