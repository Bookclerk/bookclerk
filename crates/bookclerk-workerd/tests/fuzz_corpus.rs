//! Replays the checked-in `granted_http`, `workerd_control`, `socket_broker`,
//! `socket_route`, `webhook_route`, `webhook_request`, `egress_bridge` and
//! `egress_relay_head` fuzz corpora in
//! PR CI (no libFuzzer required), so an input that once broke one of them
//! stays fixed
//! (`docs/adr/untrusted-input-fuzzing.md`).

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

/// The `socket_route` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/socket_route.rs"]
mod socket_route_oracle;

/// Regular files directly under `fuzz/corpus/socket_route/`.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn socket_route_corpus() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/socket_route")
        .canonicalize()
        .expect("canonicalize socket_route corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("socket_route corpus")
        .map(|entry| entry.expect("entry").path())
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty socket_route corpus");
    files
}

/// Mirrors `fuzz/fuzz_targets/socket_route.rs` with the shared oracle.
// Test code: an unreadable seed is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_socket_route_holds_invariants() {
    for path in socket_route_corpus() {
        socket_route_oracle::check(&std::fs::read(&path).expect("read"));
    }
}

/// One `socket_route` seed: the operation selector byte, then the body.
fn route_seed(op: u8, body: &str) -> Vec<u8> {
    let mut seed = vec![op];
    seed.extend_from_slice(body.as_bytes());
    seed
}

/// Seeds for `socket_route`. Refresh with `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed I/O is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn socket_route_seeds_are_current() {
    let seeds: Vec<(&str, Vec<u8>)> = vec![
        (
            "connect_abs.bin",
            route_seed(
                0,
                r#"{"url":"wss://abs.example:13378/socket.io/?EIO=4&transport=websocket","protocols":["chat"],"headers":[{"name":"Authorization","value":"Bearer t"}],"autoResponses":[{"request":{"text":"2"},"response":{"text":"3"}}],"attachment":"aGk="}"#,
            ),
        ),
        (
            "connect_owned_header.bin",
            route_seed(
                0,
                r#"{"url":"wss://a.example/","headers":[{"name":"Sec-WebSocket-Key","value":"x"}]}"#,
            ),
        ),
        (
            "connect_tls.bin",
            route_seed(
                0,
                r#"{"url":"tls://connect.ngrok-agent.com:443","protocols":["x-tunnel/1"],"attachment":"aGk="}"#,
            ),
        ),
        (
            "connect_extra_field.bin",
            route_seed(0, r#"{"url":"wss://a.example/","proxy":"x"}"#),
        ),
        (
            "send_binary.bin",
            route_seed(1, r#"{"socket":"g1_abc","frame":{"binary":"AAEC"}}"#),
        ),
        (
            "send_both_kinds.bin",
            route_seed(
                1,
                r#"{"socket":"g1_abc","frame":{"text":"a","binary":"AA=="}}"#,
            ),
        ),
        (
            "send_bad_id.bin",
            route_seed(1, r#"{"socket":"../g1","frame":{"text":"a"}}"#),
        ),
        (
            "close_reserved.bin",
            route_seed(2, r#"{"socket":"g1_abc","code":1006,"reason":"x"}"#),
        ),
        ("close_default.bin", route_seed(2, r#"{"socket":"g1_abc"}"#)),
        (
            "attachment.bin",
            route_seed(3, r#"{"socket":"g2_x","attachment":"AQID"}"#),
        ),
        ("list.bin", route_seed(4, "{}")),
        ("list_not_object.bin", route_seed(4, "[]")),
        ("unknown_op.bin", route_seed(9, "{}")),
    ];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/socket_route");
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

/// The `webhook_route` oracle, shared with the fuzz target.
#[path = "../../../fuzz/oracles/webhook_route.rs"]
mod webhook_route_oracle;

/// Mirrors `fuzz/fuzz_targets/webhook_route.rs` with the shared oracle.
// Test code: an unreadable seed is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_webhook_route_holds_invariants() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/webhook_route")
        .canonicalize()
        .expect("canonicalize webhook_route corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("webhook_route corpus")
        .map(|entry| entry.expect("entry").path())
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty webhook_route corpus");
    for path in files {
        webhook_route_oracle::check(&std::fs::read(&path).expect("read"));
    }
}

/// Seeds for `webhook_route`. Refresh with `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed I/O is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn webhook_route_seeds_are_current() {
    let seeds: Vec<(&str, Vec<u8>)> = vec![
        ("url.bin", route_seed(0, r#"{"name":"events"}"#)),
        ("url_bad_name.bin", route_seed(0, r#"{"name":"-x"}"#)),
        (
            "pull.bin",
            route_seed(
                1,
                r#"{"name":"events","batchSize":10,"visibilityTimeoutMs":30000}"#,
            ),
        ),
        (
            "pull_zero_batch.bin",
            route_seed(1, r#"{"name":"events","batchSize":0}"#),
        ),
        (
            "pull_extra_field.bin",
            route_seed(1, r#"{"name":"events","batchSize":1,"instance":"x"}"#),
        ),
        (
            "ack.bin",
            route_seed(2, r#"{"name":"events","acks":["l1"],"retries":["l2"]}"#),
        ),
        (
            "ack_overlap.bin",
            route_seed(2, r#"{"name":"events","acks":["l1"],"retries":["l1"]}"#),
        ),
        ("ack_array.bin", route_seed(2, r#"["events",["l1"],[]]"#)),
        ("unknown_op.bin", route_seed(7, "{}")),
    ];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/webhook_route");
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

/// The `egress_bridge` oracle, shared with the fuzz target: the Rust
/// `EgressPolicy` side of the differential against the workerd egress gate
/// (`bridge/egress.js`). It derives every probe of every seed with the Rust
/// verdicts, which [`egress_bridge_vectors_are_current`] pins in
/// `packages/plugin-sdk/scripts/egress_bridge_vectors.json` for the SDK's
/// `test:egress-bridge` script to replay against `egress.js` under Node. A
/// Rust change that moves a verdict makes the vectors stale here; a
/// JavaScript change that breaks the differential fails the script.
#[path = "../../../fuzz/oracles/egress_bridge.rs"]
#[allow(dead_code)]
mod egress_bridge_oracle;

/// Regular files directly under `fuzz/corpus/egress_bridge/`, canonicalized
/// and confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn egress_bridge_corpus() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/egress_bridge")
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
    assert!(!files.is_empty(), "empty egress_bridge corpus");
    files
}

/// One seed's policy and probes, as the Node replay reads them.
#[derive(serde::Serialize)]
struct SeedVectors {
    /// Seed file name.
    seed: String,
    /// The `EGRESS_POLICY` value the bridge receives.
    policy: serde_json::Value,
    /// Every probe with its Rust verdicts.
    probes: Vec<egress_bridge_oracle::Probe>,
}

// Test code: failed I/O or encoding is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn egress_bridge_vectors_are_current() {
    let mut seeds = Vec::new();
    for path in egress_bridge_corpus() {
        let data = std::fs::read(&path).expect("read");
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("seed name")
            .to_string();
        let policy = egress_bridge_oracle::decode(&data)
            .unwrap_or_else(|| panic!("{name} is not a policy the host can emit"));
        seeds.push(SeedVectors {
            seed: name,
            policy: policy.to_policy_json(),
            probes: egress_bridge_oracle::probes(&policy),
        });
    }
    let mut text = serde_json::to_string_pretty(&seeds).expect("vectors serialize");
    text.push('\n');
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/plugin-sdk/scripts/egress_bridge_vectors.json");
    if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
        std::fs::write(&path, &text).expect("write vectors");
    }
    assert!(
        std::fs::read_to_string(&path).expect("read vectors") == text,
        "{} is stale; rerun with UPDATE_FUZZ_SEEDS=1",
        path.display()
    );
}

/// The bridge source still has the shape the fuzz target rewrites.
#[test]
fn egress_bridge_script_loads_the_gate_functions() {
    let script = egress_bridge_oracle::bridge_script();
    for name in [
        "function allowsFetch(",
        "function allowsRedirectTo(",
        "function allowsTcp(",
        "function canonicalSocketHost(",
    ] {
        assert!(script.contains(name), "egress.js has no {name}");
    }
}

/// A sample of the Rust side, so the vectors are known to exercise both
/// verdicts rather than refusing everything.
#[test]
fn egress_bridge_probes_cover_both_verdicts() {
    let mut allowed = 0;
    let mut refused = 0;
    for path in egress_bridge_corpus() {
        let Some(policy) = egress_bridge_oracle::decode(&std::fs::read(&path).unwrap_or_default())
        else {
            continue;
        };
        for probe in egress_bridge_oracle::probes(&policy) {
            for verdict in [probe.rust.initial, probe.rust.redirect, probe.rust.tcp] {
                if verdict {
                    allowed += 1;
                } else {
                    refused += 1;
                }
            }
        }
    }
    assert!(
        allowed > 0 && refused > 0,
        "allowed {allowed}, refused {refused}"
    );
}

/// The `egress_relay_head` oracle, shared with the fuzz target: it checks
/// each head the egress relay parses against an independent parser.
#[path = "../../../fuzz/oracles/egress_relay_head.rs"]
// Test code: the shared oracle asserts with expect/unwrap.
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod egress_relay_head_oracle;

/// Regular files directly under `fuzz/corpus/egress_relay_head/`,
/// canonicalized and confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn egress_relay_head_corpus() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/egress_relay_head")
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
    assert!(!files.is_empty(), "empty egress_relay_head corpus");
    files
}

/// Every checked-in `egress_relay_head` input still holds the oracle, so a
/// head that once broke the egress relay's parsing stays fixed.
#[test]
#[allow(clippy::expect_used)]
fn fuzz_corpus_egress_relay_head_holds_the_oracle() {
    for path in egress_relay_head_corpus() {
        let data = std::fs::read(&path).expect("read");
        egress_relay_head_oracle::check(&data);
    }
}

#[path = "../../../fuzz/oracles/webhook_request.rs"]
mod webhook_request_oracle;

/// Regular files directly under `fuzz/corpus/webhook_request/`,
/// canonicalized and confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn webhook_request_corpus() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/webhook_request")
        .canonicalize()
        .expect("canonicalize webhook_request corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("webhook_request corpus")
        .map(|entry| entry.expect("entry").path())
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty webhook_request corpus");
    files
}

/// Mirrors `fuzz/fuzz_targets/webhook_request.rs` with the shared oracle.
// Test code: an unreadable seed is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_webhook_request_holds_invariants() {
    for path in webhook_request_corpus() {
        webhook_request_oracle::check(&std::fs::read(&path).expect("read"));
    }
}

/// One seed: the mode byte, then the secret, the body and header pairs
/// joined by `0xff`, each pair `name 0x00 value`.
fn request_seed(mode: u8, secret: &[u8], body: &[u8], headers: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = vec![mode];
    out.extend_from_slice(secret);
    out.push(0xff);
    out.extend_from_slice(body);
    for (name, value) in headers {
        out.push(0xff);
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        out.extend_from_slice(value);
    }
    out
}

/// Seeds for `webhook_request`. Refresh with `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed I/O is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn webhook_request_seeds_are_current() {
    let json: &[(&str, &[u8])] = &[("Content-Type", b"application/json")];
    let seeds: Vec<(&str, Vec<u8>)> = vec![
        (
            "unsigned.bin",
            request_seed(
                0,
                b"",
                b"{}",
                &[
                    ("Content-Type", b"application/json"),
                    ("X-Event", b"object.created"),
                    ("Authorization", b"Bearer dropped"),
                ],
            ),
        ),
        (
            "hex_signed.bin",
            request_seed(1 | 4, b"secret", b"{\"a\":1}", json),
        ),
        (
            "prefixed_signed.bin",
            request_seed(2 | 4, b"secret", b"{}", json),
        ),
        (
            "base64_signed.bin",
            request_seed(3 | 4, b"secret", b"{}", json),
        ),
        (
            "wrong_signature.bin",
            request_seed(1, b"secret", b"{}", &[("x-signature", b"00ff")]),
        ),
        ("no_secret.bin", request_seed(1 | 4, b"", b"{}", json)),
        ("oversize.bin", request_seed(0, b"", &[b'a'; 257], json)),
        (
            "repeated_signature.bin",
            request_seed(1 | 4, b"secret", b"{}", &[("X-Signature", b"00")]),
        ),
        (
            "control_value.bin",
            request_seed(0, b"", b"{}", &[("x-timestamp", b"1\x012")]),
        ),
        (
            "long_key.bin",
            request_seed(3 | 4, &[b'k'; 100], b"{}", json),
        ),
    ];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/webhook_request");
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
