//! Replays the checked-in `granted_http` fuzz corpus in PR CI (no libFuzzer
//! required), so a request that once broke the granted channel's parsing
//! stays fixed (`docs/adr/untrusted-input-fuzzing.md`).

use std::path::{Path, PathBuf};

use bookclerk_workerd::granted_http::{
    body_framing, parse_request_head, percent_decode, query_param, BodyError, BodyFraming,
    BodyReader, ChunkedDecoder, HeadParse,
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
        // A `regression_*` seed pins a body the decoder must refuse; every
        // other seed is a well-formed request whose body must read to its
        // declared end, or it no longer covers the path it was written for.
        let is_regression = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("regression_"));
        let body = &data[consumed..];
        // A seed whose framing headers the parser rejects is a regression
        // seed for a framing fix; the route reader must refuse it the same way.
        let framing = match body_framing(&head.headers) {
            Ok(framing) => framing,
            Err(err) => {
                assert!(
                    is_regression,
                    "seed framing rejected ({err:?}): {}",
                    path.display()
                );
                assert_eq!(
                    BodyReader::new(&head.headers).err(),
                    Some(BodyError::Framing(err)),
                    "{}",
                    path.display()
                );
                continue;
            }
        };
        // The route body reader must agree with itself whole and byte by byte.
        let whole = drive(framing, std::iter::once(body));
        let bytewise = drive(framing, body.chunks(1));
        assert_eq!(whole, bytewise, "{}", path.display());
        match (&whole.1, framing) {
            (Ok(()), BodyFraming::Length(len)) => assert_eq!(whole.0, &body[..len]),
            (Ok(()), BodyFraming::UntilEof) => assert_eq!(whole.0, body),
            (Ok(()), BodyFraming::Chunked) => assert!(!is_regression, "{}", path.display()),
            (Err(_), _) => assert!(is_regression, "{}", path.display()),
        }
        if framing == BodyFraming::Chunked {
            let mut decoder = ChunkedDecoder::new();
            let mut out = Vec::new();
            match decoder.decode(&data[consumed..], &mut out) {
                Ok(progress) => {
                    assert!(
                        progress.consumed <= data.len() - consumed,
                        "{}",
                        path.display()
                    );
                    assert!(
                        is_regression || progress.done,
                        "chunked seed does not complete: {}",
                        path.display()
                    );
                }
                Err(err) => assert!(
                    is_regression,
                    "chunked seed rejected ({err}): {}",
                    path.display()
                ),
            }
        }
    }
}

/// Feeds `pieces` to a fresh uncapped reader, then end of stream.
fn drive<'a>(
    framing: BodyFraming,
    pieces: impl Iterator<Item = &'a [u8]>,
) -> (Vec<u8>, Result<(), BodyError>) {
    let mut reader = BodyReader::for_framing(framing);
    let mut out = Vec::new();
    for piece in pieces {
        match reader.push(piece, &mut out) {
            Ok(true) => return (out, Ok(())),
            Ok(false) => {}
            Err(err) => return (out, Err(err)),
        }
    }
    if let Err(err) = reader.push(&[], &mut out) {
        return (out, Err(err));
    }
    let verdict = reader.finish();
    (out, verdict)
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
