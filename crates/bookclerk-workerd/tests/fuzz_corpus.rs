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

/// The `workerd_control` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/workerd_control.rs"]
mod workerd_control_oracle;

/// Mirrors `fuzz/fuzz_targets/workerd_control.rs` with the shared oracle.
// Test code: a missing or unreadable corpus is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_workerd_control_holds_invariants() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/workerd_control")
        .canonicalize()
        .expect("canonicalize workerd_control corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("workerd_control corpus")
        .map(|entry| entry.expect("entry").path())
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty workerd_control corpus");
    let mut accepted = 0;
    for path in &files {
        let data = std::fs::read(path).expect("read");
        workerd_control_oracle::check(&data);
        if bookclerk_workerd::control::listen_port(&data, bookclerk_workerd::control::RPC_SOCKET)
            .is_some()
        {
            accepted += 1;
        }
    }
    // `listen_rpc.json` and `listen_rpc_crlf.json` are the only real control
    // lines for the RPC socket; every other seed must be refused.
    assert_eq!(accepted, 2, "seeds read as RPC listen events");
}

/// The `socket_broker` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/socket_broker.rs"]
mod socket_broker_oracle;

/// Regular files directly under `fuzz/corpus/socket_broker/`.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn socket_broker_corpus() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/socket_broker")
        .canonicalize()
        .expect("canonicalize socket_broker corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("socket_broker corpus")
        .map(|entry| entry.expect("entry").path())
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty socket_broker corpus");
    files
}

/// Mirrors `fuzz/fuzz_targets/socket_broker.rs` with the shared oracle.
// Test code: an unreadable seed is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_socket_broker_holds_invariants() {
    for path in socket_broker_corpus() {
        socket_broker_oracle::check(&std::fs::read(&path).expect("read"));
    }
}

/// Builds `socket_broker` scripts (see the oracle's module docs).
#[derive(Default)]
struct BrokerScript(Vec<u8>);

impl BrokerScript {
    /// Op 0 with `pairs` text heartbeat pairs and `attach16` x 16 bytes.
    fn reserve(mut self, url: u8, pairs: &[(&str, &str)], attach16: u8) -> Self {
        self.0
            .extend([0, url, u8::try_from(pairs.len()).unwrap_or(0)]);
        for (request, response) in pairs {
            for frame in [request, response] {
                self.0.push(0);
                self.0.push(u8::try_from(frame.len()).unwrap_or(0));
                self.0.extend_from_slice(frame.as_bytes());
            }
        }
        self.0.push(attach16);
        self
    }

    /// Op 1.
    fn establish(mut self, slot: u8, protocol: bool) -> Self {
        self.0.extend([1, slot, u8::from(protocol)]);
        self
    }

    /// Op 2.
    fn abandon(mut self, slot: u8) -> Self {
        self.0.extend([2, slot]);
        self
    }

    /// Op 3 with a text (`binary` false) or binary frame.
    fn inbound(mut self, slot: u8, binary: bool, bytes: &[u8]) -> Self {
        let len = u16::try_from(bytes.len()).unwrap_or(0).to_le_bytes();
        self.0
            .extend([3, slot, u8::from(binary), len[0], len[1], 0]);
        self.0.extend_from_slice(bytes);
        self
    }

    /// Op 3 with a frame one byte over the cap.
    fn inbound_huge(mut self, slot: u8) -> Self {
        self.0.extend([3, slot, 1, 0, 0, 1]);
        self
    }

    /// Op 4.
    fn attach(mut self, slot: u8, len: u16, fill: u8) -> Self {
        let len = len.to_le_bytes();
        self.0.extend([4, slot, len[0], len[1], fill]);
        self
    }

    /// Op 5.
    fn closed(mut self, slot: u8, code: u16, reason_len: u8) -> Self {
        let code = code.to_le_bytes();
        self.0.extend([5, slot, code[0], code[1], reason_len]);
        self
    }

    /// Op 6 with a text frame.
    fn outbound(mut self, slot: u8, text: &str) -> Self {
        self.0
            .extend([6, slot, 0, u8::try_from(text.len()).unwrap_or(0)]);
        self.0.extend_from_slice(text.as_bytes());
        self
    }

    /// Op 7.
    fn close(mut self, slot: u8, code: u16, reason_len: u8) -> Self {
        let code = code.to_le_bytes();
        self.0.extend([7, slot, code[0], code[1], reason_len]);
        self
    }
}

/// Seeds for `socket_broker`. Refresh with `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed I/O is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn socket_broker_seeds_are_current() {
    let socket_io = [("2", "3")];
    let seeds: Vec<(&str, Vec<u8>)> = vec![
        (
            "heartbeat_then_event.bin",
            BrokerScript::default()
                .reserve(0, &socket_io, 1)
                .establish(0, true)
                .inbound(0, false, b"2")
                .inbound(0, true, b"2")
                .inbound(0, false, b"42[\"item_updated\",{}]")
                .attach(0, 32, 7)
                .inbound(0, false, b"42[\"item_removed\",{}]")
                .outbound(0, "40")
                .closed(0, 1000, 0)
                .inbound(0, false, b"late")
                .0,
        ),
        (
            "instance_limit.bin",
            (0..9)
                .fold(BrokerScript::default(), |s, _| s.reserve(1, &[], 0))
                .abandon(3)
                .reserve(1, &[], 0)
                .0,
        ),
        (
            "too_big.bin",
            BrokerScript::default()
                .reserve(0, &[], 0)
                .establish(0, false)
                .inbound_huge(0)
                .outbound(0, "after")
                .0,
        ),
        (
            "pending_is_inert.bin",
            BrokerScript::default()
                .reserve(1, &socket_io, 0)
                .inbound(0, false, b"2")
                .attach(0, 4, 1)
                .outbound(0, "x")
                .closed(0, 1006, 0)
                .abandon(0)
                .establish(0, false)
                .0,
        ),
        (
            "refused_requests.bin",
            BrokerScript::default()
                .reserve(2, &[], 0)
                .reserve(3, &[], 0)
                .reserve(0, &[("", "3")], 0)
                .reserve(0, &[], 200)
                .0,
        ),
        (
            "close_codes.bin",
            BrokerScript::default()
                .reserve(0, &[], 0)
                .establish(0, false)
                .close(0, 0, 0)
                .close(0, 1005, 0)
                .close(0, 4000, 124)
                .close(1, 1000, 0)
                .attach(0, 2049, 0)
                .attach(0, 2048, 9)
                .closed(0, 1001, 5)
                .0,
        ),
    ];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/socket_broker");
    if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
        std::fs::create_dir_all(&dir).expect("corpus dir");
    }
    for (name, seed) in seeds {
        let path = dir.join(name);
        if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
            std::fs::write(&path, &seed).expect("write seed");
        }
        assert_eq!(
            std::fs::read(&path).expect("read seed"),
            seed,
            "{} is stale; rerun with UPDATE_FUZZ_SEEDS=1",
            path.display()
        );
    }
}
