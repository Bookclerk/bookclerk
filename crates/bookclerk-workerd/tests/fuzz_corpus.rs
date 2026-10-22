//! Replays the checked-in `granted_http` fuzz corpus in PR CI (no libFuzzer
//! required), so a request that once broke the granted channel's parsing
//! stays fixed (`docs/adr/untrusted-input-fuzzing.md`).

use std::path::{Path, PathBuf};

use bookclerk_workerd::granted_http::{
    body_framing, parse_request_head, percent_decode, query_param, BodyFraming, ChunkedDecoder,
    HeadParse,
};

/// Regular files directly under `fuzz/corpus/granted_http/`, canonicalized
/// and confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn corpus_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/granted_http")
        .canonicalize()
        .expect("canonicalize fuzz corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fuzz corpus")
        .map(|entry| entry.expect("entry").path())
        // Inline canonicalize → starts_with at each sink (CodeQL two-state barrier).
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty granted_http corpus");
    files
}

#[test]
fn fuzz_corpus_granted_http_does_not_panic() {
    for path in corpus_files() {
        let data = std::fs::read(&path).expect("read");
        let (head, consumed) = match parse_request_head(&data) {
            Ok(HeadParse::Complete { head, consumed }) => (head, consumed),
            Ok(HeadParse::Incomplete) | Err(_) => continue,
        };
        let (_, query) = head.split_query();
        let _ = query_param(query, "key");
        let _ = percent_decode(query);
        if let Ok(BodyFraming::Chunked) = body_framing(&head.headers) {
            let mut decoder = ChunkedDecoder::new();
            let mut out = Vec::new();
            if let Ok(progress) = decoder.decode(&data[consumed..], &mut out) {
                assert!(
                    progress.consumed <= data.len() - consumed,
                    "{}",
                    path.display()
                );
            }
        }
    }
}

/// The two regressions the corpus pins: a split UTF-8 escape in the query
/// must decode, not panic, and a garbage chunk size must be an error, never
/// the terminating chunk.
#[test]
fn granted_http_regressions_stay_fixed() {
    assert_eq!(query_param("key=%aé", "key"), Some("%aé".to_string()));
    let mut decoder = ChunkedDecoder::new();
    let mut out = Vec::new();
    assert!(decoder
        .decode(b"zz\r\nhello\r\n0\r\n\r\n", &mut out)
        .is_err());
    assert!(out.is_empty());
}
