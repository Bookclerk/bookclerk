//! Checks every response the daemon's tests produce against the OpenAPI
//! document, and records what it saw.
//!
//! The router adds [`check`] as a layer in test builds only. For every
//! response to a route the router matched, the layer looks the route's
//! template, the method, the status and the media type up in the document
//! and, for a JSON body, validates the body against the declared schema.
//! Each response becomes one [`Hit`] appended as a JSON line to the report
//! file of this process, which `scripts/contract_coverage.py` merges across
//! the test processes into the declared-versus-observed report CI Gate
//! shows. The layer never fails a test: a mismatch is an outcome in the
//! report, so the report says how far the document and the daemon agree
//! before any floor is enforced.
//!
//! [`assess`] is sans-IO (document, its compiled validators, request facts
//! and body bytes in, an outcome out) and [`observe`] records nothing, so
//! the unit tests drive the same code the layer does against documents and
//! report sinks of their own.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use axum::body::Body;
use axum::extract::{MatchedPath, Request};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use serde::Serialize;

/// The largest JSON body, by its `Content-Length`, the layer buffers to
/// validate; a longer one passes through unread and is recorded as not
/// validated, so the response under test is never changed.
const BODY_LIMIT: u64 = 16 * 1024 * 1024;

/// Where the report files go when `BOOKCLERK_CONTRACT_REPORT_DIR` is unset:
/// `target/contract-report` at the workspace root, one file per process.
const DEFAULT_REPORT_DIR: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/contract-report");

/// What the layer found when it compared one response with the document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// The route, status and media type are declared and the body, when
    /// JSON, validates against the declared schema.
    Declared,
    /// The router matched a route the document does not describe.
    Undocumented,
    /// The route is documented but does not declare this status.
    UndeclaredStatus,
    /// The route declares the status but not this media type (`none` when
    /// the response carried no `Content-Type`).
    UndeclaredMedia,
    /// The body is not JSON although the media type says it is.
    Unparsed {
        /// The parser's message.
        error: String,
    },
    /// The JSON body does not validate against the declared schema.
    Invalid {
        /// Each violation as the validator describes it, with the instance
        /// path it concerns.
        errors: Vec<String>,
    },
}

/// One response the layer saw, as written to the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Hit {
    /// The HTTP method in lower case, as the document keys it.
    pub(crate) method: String,
    /// The route template the router matched, such as `/api/jobs/{id}`.
    pub(crate) path: String,
    /// The response status.
    pub(crate) status: u16,
    /// The response media type without parameters, or `none` when the
    /// response carried no `Content-Type`.
    pub(crate) media: String,
    /// What the comparison found.
    pub(crate) outcome: Outcome,
}

/// The document as JSON, serialized once per process.
fn document() -> &'static serde_json::Value {
    static DOCUMENT: OnceLock<serde_json::Value> = OnceLock::new();
    DOCUMENT.get_or_init(|| {
        serde_json::to_value(crate::openapi::shared_document().as_ref())
            .expect("the document serializes")
    })
}

/// Rewrites every `#/components/schemas/` reference under `value` to
/// `#/$defs/`, so a schema lifted out of the document resolves its
/// references against a `$defs` map in the same root.
pub(crate) fn redirect_refs(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, inner) in map.iter_mut() {
                if key == "$ref" {
                    if let Some(target) = inner.as_str() {
                        if let Some(name) = target.strip_prefix("#/components/schemas/") {
                            *inner = serde_json::Value::String(format!("#/$defs/{name}"));
                        }
                    }
                } else {
                    redirect_refs(inner);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redirect_refs),
        _ => {}
    }
}

/// Compiles `schema` as a JSON Schema 2020-12 root carrying `components`
/// under `$defs`, with every reachable subschema addressable by pointer
/// and formats asserted, not only annotated.
pub(crate) fn validators(
    components: &serde_json::Value,
    schema: &serde_json::Value,
) -> jsonschema::ValidatorMap {
    let mut root = schema.clone();
    // A document without components (a test fixture) has no `$defs`.
    root["$defs"] = if components.is_object() {
        components.clone()
    } else {
        serde_json::json!({})
    };
    redirect_refs(&mut root);
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .should_validate_formats(true)
        .build_map(&root)
        .expect("the document's schemas are valid JSON Schema")
}

/// The validators compiled from one document's components, plus one map
/// per inline body schema met so far, keyed by `method path status media`.
/// Each document gets its own: the daemon's document has [`Compiled::shared`],
/// a test fixture builds one with [`Compiled::new`].
pub(crate) struct Compiled {
    /// `#/$defs/<name>` for every component schema and its subschemas.
    components: jsonschema::ValidatorMap,
    /// Inline body schemas compiled on first use.
    inline: Mutex<HashMap<String, jsonschema::ValidatorMap>>,
}

impl Compiled {
    /// Compiles the component schemas of `doc`.
    pub(crate) fn new(doc: &serde_json::Value) -> Self {
        Self {
            components: validators(&doc["components"]["schemas"], &serde_json::json!({})),
            inline: Mutex::new(HashMap::new()),
        }
    }

    /// The validators for the daemon's own document ([`document`]), compiled
    /// once per process.
    fn shared() -> &'static Self {
        static COMPILED: OnceLock<Compiled> = OnceLock::new();
        COMPILED.get_or_init(|| Self::new(document()))
    }
}

/// The media type of `content_type` without its parameters, in lower
/// case, or `none` for no header.
fn media_type(content_type: Option<&str>) -> String {
    content_type
        .and_then(|value| value.split(';').next())
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "none".to_string())
}

/// Whether `media` carries JSON (`application/json` or a `+json` suffix).
fn is_json(media: &str) -> bool {
    media == "application/json" || media.ends_with("+json")
}

/// Validates `body` against the schema declared for `key` in `content`,
/// a Media Type Object, with `doc`'s components reachable by reference
/// through `compiled`, the validators built from `doc`.
fn validate_body(
    doc: &serde_json::Value,
    compiled: &Compiled,
    key: &str,
    content: &serde_json::Value,
    body: &serde_json::Value,
) -> Vec<String> {
    let Some(schema) = content.get("schema") else {
        return Vec::new();
    };
    let referenced = schema["$ref"]
        .as_str()
        .filter(|_| schema.as_object().is_some_and(|map| map.len() == 1))
        .and_then(|target| target.strip_prefix("#/components/schemas/"));
    let errors = |validator: &jsonschema::Validator| {
        validator
            .iter_errors(body)
            .map(|error| format!("{error} (at instance path {})", error.instance_path()))
            .collect::<Vec<_>>()
    };
    if let Some(name) = referenced {
        let pointer = format!("#/$defs/{name}");
        return match compiled.components.get(&pointer) {
            Some(validator) => errors(validator),
            None => vec![format!("no schema compiled at {pointer}")],
        };
    }
    #[allow(clippy::expect_used)] // a poisoned map only follows a panic in another test
    let mut inline = compiled.inline.lock().expect("inline validator map");
    let map = inline
        .entry(key.to_string())
        .or_insert_with(|| validators(&doc["components"]["schemas"], schema));
    match map.get("#") {
        Some(validator) => errors(validator),
        None => vec!["no schema compiled at #".to_string()],
    }
}

/// Compares one response with `doc` (whose validators are `compiled`): the route template `path` and
/// `method` (lower case) the router matched, the `status` it answered,
/// the `media` type of the body and the body `bytes` when the media is
/// JSON.
pub(crate) fn assess(
    doc: &serde_json::Value,
    compiled: &Compiled,
    method: &str,
    path: &str,
    status: StatusCode,
    media: &str,
    bytes: Option<&[u8]>,
) -> Outcome {
    let operation = &doc["paths"][path][method];
    if !operation.is_object() {
        return Outcome::Undocumented;
    }
    let responses = &operation["responses"];
    let declared = match responses.get(status.as_str()) {
        Some(response) => response,
        None => match responses.get("default") {
            Some(response) => response,
            None => return Outcome::UndeclaredStatus,
        },
    };
    let content = declared["content"].as_object();
    let Some(entry) = content.and_then(|content| {
        content
            .iter()
            .find(|(declared_media, _)| media_type(Some(declared_media)) == media)
    }) else {
        return if media == "none" && content.is_none_or(|content| content.is_empty()) {
            Outcome::Declared
        } else {
            Outcome::UndeclaredMedia
        };
    };
    if !is_json(media) {
        return Outcome::Declared;
    }
    let body: serde_json::Value = match serde_json::from_slice(bytes.unwrap_or_default()) {
        Ok(body) => body,
        Err(error) => {
            return Outcome::Unparsed {
                error: error.to_string(),
            }
        }
    };
    let key = format!("{method} {path} {} {media}", status.as_u16());
    let errors = validate_body(doc, compiled, &key, entry.1, &body);
    if errors.is_empty() {
        Outcome::Declared
    } else {
        Outcome::Invalid { errors }
    }
}

/// The report file for process `pid` under `dir`.
pub(crate) fn report_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.jsonl"))
}

/// Appends `hit` as one JSON line to the file at `path`, creating the
/// directory and file as needed.
///
/// # Errors
///
/// Returns the I/O error when the directory or file cannot be written.
pub(crate) fn append(path: &Path, hit: &Hit) -> std::io::Result<()> {
    // One write per line, under a process-wide lock, so lines from the
    // responses one process answers concurrently never interleave.
    static LOCK: Mutex<()> = Mutex::new(());
    let mut line = serde_json::to_string(hit).map_err(std::io::Error::other)?;
    line.push('\n');
    let _guard = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())
}

/// This process's report file, from `BOOKCLERK_CONTRACT_REPORT_DIR` or
/// the default directory under `target/`.
fn process_report() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::env::var_os("BOOKCLERK_CONTRACT_REPORT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_REPORT_DIR));
        report_path(&dir, std::process::id())
    })
}

/// Records `hit` in this process's report; a report that cannot be written
/// is noted once on stderr and otherwise ignored, since the layer never
/// fails a test.
fn record(hit: &Hit) {
    static WARNED: OnceLock<()> = OnceLock::new();
    if let Err(error) = append(process_report(), hit) {
        WARNED.get_or_init(|| {
            eprintln!(
                "openapi contract: cannot write {}: {error}",
                process_report().display()
            );
        });
    }
}

/// The layer: compares the response to a matched route with the daemon's
/// document ([`observe`]) and records the outcome in this process's
/// report, passing the response on unchanged.
pub(crate) async fn check(req: Request, next: Next) -> Response {
    let (hit, response) = observe(document(), Compiled::shared(), req, next).await;
    if let Some(hit) = hit {
        record(&hit);
    }
    response
}

/// Runs `req` through `next` and compares the response with `doc` (whose
/// validators are `compiled`): the [`Hit`] for a route the router matched
/// (`None` for an unmatched request) and the response to pass on. A JSON
/// body is buffered and re-wrapped; one whose `Content-Length` is over
/// [`BODY_LIMIT`] passes through unread and is recorded as not validated,
/// so the response a test asserts on is never changed.
pub(crate) async fn observe(
    doc: &serde_json::Value,
    compiled: &Compiled,
    req: Request,
    next: Next,
) -> (Option<Hit>, Response) {
    let method = req.method().as_str().to_ascii_lowercase();
    let matched = req
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_string());
    let response = next.run(req).await;
    let Some(path) = matched else {
        return (None, response);
    };
    let status = response.status();
    let media = media_type(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
    );
    let length = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let hit = |outcome: Outcome| {
        Some(Hit {
            method: method.clone(),
            path: path.clone(),
            status: status.as_u16(),
            media: media.clone(),
            outcome,
        })
    };
    if !is_json(&media) {
        let outcome = assess(doc, compiled, &method, &path, status, &media, None);
        return (hit(outcome), response);
    }
    if let Some(length) = length.filter(|length| *length > BODY_LIMIT) {
        let outcome = Outcome::Unparsed {
            error: format!("body of {length} bytes not validated (limit {BODY_LIMIT})"),
        };
        return (hit(outcome), response);
    }
    // The length is within the limit, or unstated, which no JSON response
    // of the daemon's tests is at a size worth refusing; the body is read
    // whole so that it can be handed on intact.
    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(error) => {
            let outcome = Outcome::Unparsed {
                error: format!("body not readable: {error}"),
            };
            return (hit(outcome), Response::from_parts(parts, Body::empty()));
        }
    };
    let outcome = assess(doc, compiled, &method, &path, status, &media, Some(&bytes));
    (hit(outcome), Response::from_parts(parts, Body::from(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A document with one route for the unit tests: a JSON 200, a bare
    /// 204, a JSON `default` and a `text/plain` 415.
    fn doc() -> serde_json::Value {
        serde_json::json!({
            "paths": {
                "/api/things/{id}": {
                    "get": {
                        "responses": {
                            "200": { "content": { "application/json": {
                                "schema": { "$ref": "#/components/schemas/Thing" }
                            } } },
                            "204": { "description": "gone" },
                            "415": { "content": { "text/plain": { "schema": { "type": "string" } } } },
                            "default": { "content": { "application/json": {
                                "schema": { "type": "object", "required": ["error"], "properties": {
                                    "error": { "type": "string" },
                                    "thing": { "$ref": "#/components/schemas/Thing" }
                                } }
                            } } }
                        }
                    }
                }
            },
            "components": { "schemas": {
                "Thing": { "type": "object", "required": ["id"], "properties": {
                    "id": { "type": "string", "format": "uuid" },
                    "count": { "type": "integer", "minimum": 0 }
                } }
            } }
        })
    }

    /// Shorthand for [`assess`] on the test document, with validators of
    /// its own (the daemon's document keeps the process-wide ones).
    fn outcome(method: &str, status: u16, media: &str, body: Option<&str>) -> Outcome {
        let doc = doc();
        assess(
            &doc,
            &Compiled::new(&doc),
            method,
            "/api/things/{id}",
            StatusCode::from_u16(status).expect("status"),
            media,
            body.map(str::as_bytes),
        )
    }

    #[test]
    fn a_declared_json_response_with_a_valid_body_is_declared() {
        let body = r#"{"id":"6d2f0c1e-8a4b-4f3d-9e7c-5b1a2c3d4e5f","count":2}"#;
        assert_eq!(
            outcome("get", 200, "application/json", Some(body)),
            Outcome::Declared
        );
    }

    #[test]
    fn a_body_outside_the_schema_names_each_violation() {
        let body = r#"{"id":"nope","count":-1}"#;
        let Outcome::Invalid { errors } = outcome("get", 200, "application/json", Some(body))
        else {
            panic!("expected an invalid outcome");
        };
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("/id")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("/count")), "{errors:?}");
    }

    #[test]
    fn an_inline_schema_resolves_its_component_references() {
        let body = r#"{"error":"not_found","thing":{"id":"nope"}}"#;
        let Outcome::Invalid { errors } = outcome("get", 404, "application/json", Some(body))
        else {
            panic!("expected an invalid outcome");
        };
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("/thing/id"), "{errors:?}");
        assert_eq!(
            outcome(
                "get",
                404,
                "application/json",
                Some(r#"{"error":"not_found"}"#)
            ),
            Outcome::Declared
        );
    }

    #[test]
    fn statuses_media_types_and_routes_outside_the_document_are_named() {
        assert_eq!(outcome("get", 204, "none", None), Outcome::Declared);
        assert_eq!(
            outcome("get", 200, "none", None),
            Outcome::UndeclaredMedia,
            "a declared JSON body answered with no body"
        );
        assert_eq!(
            outcome("get", 200, "text/html", None),
            Outcome::UndeclaredMedia
        );
        assert_eq!(outcome("get", 415, "text/plain", None), Outcome::Declared);
        assert_eq!(outcome("post", 200, "none", None), Outcome::Undocumented);
        let bare = serde_json::json!({ "paths": { "/x": { "get": { "responses": {
            "200": { "content": { "text/plain": {} } } } } } } });
        assert_eq!(
            assess(
                &bare,
                &Compiled::new(&bare),
                "get",
                "/x",
                StatusCode::NOT_FOUND,
                "text/plain",
                None
            ),
            Outcome::UndeclaredStatus
        );
        assert_eq!(
            outcome("get", 200, "application/json", Some("{")),
            Outcome::Unparsed {
                error: "EOF while parsing an object at line 1 column 1".to_string()
            }
        );
    }

    #[test]
    fn media_types_drop_parameters_and_case() {
        assert_eq!(
            media_type(Some("Application/JSON; charset=utf-8")),
            "application/json"
        );
        assert_eq!(media_type(Some("")), "none");
        assert_eq!(media_type(None), "none");
        assert!(is_json("application/problem+json"));
        assert!(!is_json("text/plain"));
    }

    #[test]
    fn hits_are_appended_as_json_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = report_path(&dir.path().join("nested"), 42);
        assert!(path.ends_with("nested/42.jsonl"));
        let hit = Hit {
            method: "get".to_string(),
            path: "/api/things/{id}".to_string(),
            status: 200,
            media: "application/json".to_string(),
            outcome: Outcome::Declared,
        };
        append(&path, &hit).expect("first line");
        append(
            &path,
            &Hit {
                outcome: Outcome::Invalid {
                    errors: vec!["bad".to_string()],
                },
                ..hit.clone()
            },
        )
        .expect("second line");
        let text = std::fs::read_to_string(&path).expect("report");
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("json line"))
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0]["outcome"],
            serde_json::json!({ "kind": "declared" })
        );
        assert_eq!(lines[1]["outcome"]["kind"], "invalid");
        assert_eq!(lines[1]["outcome"]["errors"][0], "bad");
        assert_eq!(lines[1]["path"], "/api/things/{id}");
    }

    /// The real router matches a route template, the layer sees the final
    /// response and records it under the template, not the request path;
    /// the body passes through, and a body too long to validate passes
    /// through unread. The test's own document and sink keep it apart
    /// from the daemon's report, whichever test runs first in the process.
    #[tokio::test]
    async fn the_layer_records_the_matched_route() {
        use axum::routing::get;
        use std::sync::Arc;
        use tower::ServiceExt as _;
        static DOC: OnceLock<(serde_json::Value, Compiled)> = OnceLock::new();
        let (doc, compiled) = DOC.get_or_init(|| {
            let doc = doc();
            let compiled = Compiled::new(&doc);
            (doc, compiled)
        });
        let hits: Arc<Mutex<Vec<Hit>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = hits.clone();
        let app = axum::Router::new()
            .route(
                "/api/things/{id}",
                get(|| async { axum::Json(serde_json::json!({ "id": "x" })) }),
            )
            .route(
                "/api/things/{id}/large",
                get(|| async {
                    (
                        [
                            (header::CONTENT_TYPE, "application/json".to_string()),
                            (header::CONTENT_LENGTH, (BODY_LIMIT + 1).to_string()),
                        ],
                        r#"{"id":"x"}"#,
                    )
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| {
                let sink = sink.clone();
                async move {
                    let (hit, response) = observe(doc, compiled, req, next).await;
                    if let Some(hit) = hit {
                        sink.lock().expect("hits").push(hit);
                    }
                    response
                }
            }));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/things/7")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("body");
        assert_eq!(body.as_ref(), br#"{"id":"x"}"#, "the body passes through");
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/things/7/large")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("body");
        assert_eq!(
            body.as_ref(),
            br#"{"id":"x"}"#,
            "a long body passes through unread"
        );
        let hits = hits.lock().expect("hits");
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].method, "get");
        assert_eq!(hits[0].path, "/api/things/{id}");
        assert_eq!(hits[0].status, 200);
        assert_eq!(hits[0].media, "application/json");
        assert!(
            matches!(&hits[0].outcome, Outcome::Invalid { errors } if errors.len() == 1 && errors[0].contains("/id")),
            "{:?}",
            hits[0].outcome
        );
        assert_eq!(hits[1].path, "/api/things/{id}/large");
        assert!(
            matches!(&hits[1].outcome, Outcome::Unparsed { error } if error.contains("not validated")),
            "{:?}",
            hits[1].outcome
        );
    }
}
