//! Test support for model downloads: a tiny model and a registry on
//! loopback that serves it the way the real one does (a token endpoint,
//! then blobs by digest with `Range` support).
//!
//! Compiled for this crate's tests and, behind the `test-support`
//! feature, for the tests of the crates that drive downloads. Never part
//! of a product build.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};

use bookclerk_model_pack::{PackManifest, Sha256Hex, MANIFEST_FILE};

use crate::model_fetch::{DescriptionPin, ModelOffer, ModelOffers, ModelRegistry};

/// A tiny model: its files by name, `model.json` among them.
pub struct Pack {
    /// File content by name.
    pub files: BTreeMap<String, Vec<u8>>,
}

impl Pack {
    /// A model with three small files and the description that lists
    /// them.
    #[must_use]
    pub fn new(model_id: &str) -> Self {
        let mut files = BTreeMap::new();
        files.insert("core.onnx".to_owned(), b"graph bytes".repeat(300));
        files.insert("tokenizer.spm.fb".to_owned(), b"pieces".to_vec());
        files.insert(
            "base.rows".to_owned(),
            (0..=255u8).cycle().take(9000).collect(),
        );
        let entry = |name: &str| {
            let content = &files[name];
            serde_json::json!({
                "file": name,
                "bytes": content.len(),
                "sha256": Sha256Hex::of(content).as_str(),
            })
        };
        let description = serde_json::json!({
            "format": 1,
            "model_id": model_id,
            "dimensions": 4,
            "max_tokens": 16,
            "graph": {
                "model": entry("core.onnx"),
                "ids_input": "input_ids",
                "mask_input": "attention_mask",
                "output": "sentence_embedding",
                "pooling": "none",
            },
            "tokenizer": { "model": entry("tokenizer.spm.fb") },
            "tables": [{ "input": "rows", "element": "u8", "width": 4 }],
            "base_rows": entry("base.rows"),
            "languages": {},
        });
        let mut bytes = serde_json::to_vec_pretty(&description).expect("json");
        bytes.push(b'\n');
        PackManifest::parse(&bytes).expect("a valid description");
        files.insert(MANIFEST_FILE.to_owned(), bytes);
        Self { files }
    }

    /// The offer that pins this model.
    #[must_use]
    pub fn offer(&self) -> ModelOffer {
        let description = &self.files[MANIFEST_FILE];
        ModelOffer {
            label: "Tiny".into(),
            summary: "A model for tests.".into(),
            model_json: DescriptionPin::of(description),
            download_bytes: self.files.values().map(|f| f.len() as u64).sum(),
            memory_bytes: 1,
        }
    }

    /// Content by `sha256:<hex>` blob name.
    #[must_use]
    pub fn blobs(&self) -> BTreeMap<String, Vec<u8>> {
        self.files
            .values()
            .map(|content| (Sha256Hex::of(content).as_str().to_owned(), content.clone()))
            .collect()
    }
}

/// What the fake registry was asked: request line and whether the
/// request carried the token.
pub type Seen = Arc<Mutex<Vec<(String, bool)>>>;

/// A registry on loopback that hands out the token `t0ken` and serves
/// `blobs` to requests that carry it. `cut` truncates the first answer
/// for the blob with that digest, to exercise a resumed download.
///
/// # Returns
///
/// The registry's address (`http://127.0.0.1:<port>`) and what it was
/// asked.
#[must_use]
pub fn serve(blobs: BTreeMap<String, Vec<u8>>, cut: Option<String>) -> (String, Seen) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        let mut cut = cut;
        for socket in listener.incoming() {
            let Ok(mut socket) = socket else { return };
            let mut request = Vec::new();
            let mut buf = [0u8; 2048];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match socket.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let text = String::from_utf8_lossy(&request).into_owned();
            let line = text.lines().next().unwrap_or_default().to_owned();
            let header = |name: &str| {
                text.lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix(name).map(str::to_owned))
            };
            let authorized = header("authorization:").is_some_and(|v| v.trim() == "bearer t0ken");
            log.lock().expect("log").push((line.clone(), authorized));
            let path = line.split(' ').nth(1).unwrap_or_default();
            let answer = if path.starts_with("/token?") {
                let body = br#"{"token":"t0ken","expires_in":300}"#;
                let mut out = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                out.extend_from_slice(body);
                out
            } else if let Some(body) = path
                .rsplit_once("/blobs/sha256:")
                .filter(|_| authorized)
                .and_then(|(_, digest)| blobs.get(digest).map(|body| (digest, body)))
                .map(|(digest, body)| {
                    let from = header("range:").and_then(|v| {
                        v.trim()
                            .strip_prefix("bytes=")?
                            .strip_suffix('-')?
                            .parse::<usize>()
                            .ok()
                    });
                    (digest.to_owned(), body, from)
                })
            {
                let (digest, body, from) = body;
                match from {
                    Some(from) => {
                        let mut out = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                            body.len() - from,
                            from,
                            body.len() - 1,
                            body.len()
                        )
                        .into_bytes();
                        out.extend_from_slice(&body[from..]);
                        out
                    }
                    None => {
                        let mut out = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"{digest}\"\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        if cut.as_deref() == Some(digest.as_str()) {
                            cut = None;
                            out.extend_from_slice(&body[..body.len() / 2]);
                        } else {
                            out.extend_from_slice(body);
                        }
                        out
                    }
                }
            } else {
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            };
            let _ = socket.write_all(&answer);
            let _ = socket.flush();
        }
    });
    (base, seen)
}

/// [`serve`], as the registry [`offers_json`] names reached on loopback.
#[must_use]
pub fn registry(blobs: BTreeMap<String, Vec<u8>>, cut: Option<String>) -> (ModelRegistry, Seen) {
    let (base, seen) = serve(blobs, cut);
    let offers = ModelOffers::parse(&offers_json(&Pack::new("tiny"))).expect("offers");
    (offers.registry().at(&base).expect("loopback"), seen)
}

/// A model list that offers `pack` as `tiny`, the default.
#[must_use]
pub fn offers_json(pack: &Pack) -> Vec<u8> {
    let offer = pack.offer();
    serde_json::to_vec(&serde_json::json!({
        "registry": "registry.example/acme/models",
        "default": "tiny",
        "models": { "tiny": {
            "label": offer.label,
            "summary": offer.summary,
            "model_json": {
                "bytes": offer.model_json.bytes,
                "sha256": offer.model_json.sha256.as_str(),
            },
            "download_bytes": offer.download_bytes,
            "memory_bytes": offer.memory_bytes,
        } },
    }))
    .expect("json")
}
