//! Replays the checked-in `model_fetch` fuzz corpus in PR CI (no libFuzzer
//! required), so a token reply or a model list that once broke the host's
//! model download stays fixed (`docs/adr/untrusted-input-fuzzing.md`), and
//! holds the embedder package's model list to the pins the pack builder
//! publishes under.
//!
//! `UPDATE_FUZZ_SEEDS=1` rewrites the generated seeds; without it the test
//! fails when a generated seed no longer matches its file.
// Test code: a failed assertion or a missing corpus file is the test failing.
#![allow(clippy::missing_panics_doc, clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use bookclerk_plugin_catalog::{parse_token_reply, ModelOffers};

#[path = "../../../fuzz/oracles/model_fetch.rs"]
mod oracle;

/// The repository root.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `fuzz/corpus/model_fetch`.
fn corpus_dir() -> PathBuf {
    root().join("fuzz/corpus/model_fetch")
}

/// The embedder plugin's source directory.
fn embedder_dir() -> PathBuf {
    root().join("crates/bookclerk-plugins/platform/ai-embedder")
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

/// A seed read as a token reply: the selector byte, then the body.
fn as_token(body: &[u8]) -> Vec<u8> {
    let mut seed = vec![0u8];
    seed.extend_from_slice(body);
    seed
}

/// A seed read as a model list.
fn as_offers(json: &[u8]) -> Vec<u8> {
    let mut seed = vec![1u8];
    seed.extend_from_slice(json);
    seed
}

/// The generated seeds: a token reply and the embedder's model list whole,
/// and each with one rule broken.
fn seeds() -> Vec<(&'static str, Vec<u8>)> {
    let offers = std::fs::read_to_string(embedder_dir().join("models.json")).expect("models.json");
    let swapped = |from: &str, to: &str| {
        assert!(offers.contains(from), "models.json holds no {from:?}");
        as_offers(offers.replacen(from, to, 1).as_bytes())
    };
    vec![
        (
            "token.json",
            as_token(br#"{"token":"djE6Ym9va2NsZXJr.bW9kZWxz-_~+/==","expires_in":300}"#),
        ),
        (
            "token_newline.json",
            as_token(br#"{"token":"abc\r\nHost: evil.example"}"#),
        ),
        ("token_space.json", as_token(br#"{"token":"abc def"}"#)),
        ("token_empty.json", as_token(br#"{"token":""}"#)),
        ("token_number.json", as_token(br#"{"token":7}"#)),
        (
            "token_twice.json",
            as_token(br#"{"token":"first","token":"second"}"#),
        ),
        (
            "token_other_field.json",
            as_token(br#"{"access_token":"abc"}"#),
        ),
        ("token_array.json", as_token(br#"["abc"]"#)),
        ("token_cut.json", as_token(br#"{"token":"abc"#)),
        ("offers.json", as_offers(offers.as_bytes())),
        (
            "offers_registry_scheme.json",
            swapped(
                "ghcr.io/bookclerk/models",
                "https://ghcr.io/bookclerk/models",
            ),
        ),
        (
            "offers_registry_userinfo.json",
            swapped("ghcr.io/bookclerk/models", "user@ghcr.io/bookclerk/models"),
        ),
        (
            "offers_registry_climbs.json",
            swapped("ghcr.io/bookclerk/models", "ghcr.io/../models"),
        ),
        (
            "offers_registry_query.json",
            swapped("ghcr.io/bookclerk/models", "ghcr.io/models?x=1"),
        ),
        (
            "offers_registry_no_repository.json",
            swapped("ghcr.io/bookclerk/models", "ghcr.io"),
        ),
        (
            "offers_id_climbs.json",
            swapped("\"harrier-270m-q4\": {", "\"../harrier\": {"),
        ),
        (
            "offers_id_hidden.json",
            swapped("\"harrier-270m-q4\": {", "\".incoming-bekko-a8m\": {"),
        ),
        (
            "offers_default_missing.json",
            swapped("\"default\": \"bekko-a8m\"", "\"default\": \"other\""),
        ),
        (
            "offers_unknown_field.json",
            swapped("\"default\"", "\"extra\": 1, \"default\""),
        ),
        (
            "offers_digest_short.json",
            swapped(
                "f826a998307bf33fb41522eb8623e116e1a71c5e8384c68c07785cede5cef2eb",
                "f826a998",
            ),
        ),
        (
            "offers_no_models.json",
            as_offers(br#"{"registry":"ghcr.io/bookclerk/models","default":"a","models":{}}"#),
        ),
        (
            "offers_as_array.json",
            as_offers(br#"["ghcr.io/bookclerk/models","a",{}]"#),
        ),
        (
            "offers_cut.json",
            as_offers(&offers.as_bytes()[..offers.len() / 2]),
        ),
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
fn fuzz_corpus_model_fetch_holds_invariants() {
    let (mut tokens, mut lists) = (0, 0);
    for path in corpus_files() {
        let data = std::fs::read(&path).expect("read");
        oracle::check(&data);
        let Some((kind, rest)) = data.split_first() else {
            continue;
        };
        if *kind == 0 {
            tokens += usize::from(parse_token_reply(rest).is_ok());
        } else {
            lists += usize::from(ModelOffers::parse(rest).is_ok());
        }
    }
    // Exactly the whole seeds are accepted: every broken one is refused.
    assert_eq!((tokens, lists), (1, 1), "accepted token replies and lists");
}

#[test]
fn the_embedder_offers_exactly_the_published_packs() {
    let offers = ModelOffers::read(&embedder_dir())
        .expect("models.json is valid")
        .expect("the embedder package carries models.json");
    let pins: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root().join("scripts/model-packs/models.json")).expect("builder pins"),
    )
    .expect("builder pins are JSON");
    let registry = pins["registry"].as_str().expect("registry");
    let host = registry.split('/').next().expect("host");
    let published = pins["models"].as_object().expect("models");

    let offered: Vec<&str> = offers.models().map(|(id, _)| id).collect();
    let built: Vec<&str> = published.keys().map(String::as_str).collect();
    assert_eq!(offered, built, "the offered and the built models differ");
    for (id, offer) in offers.models() {
        assert_eq!(
            offer.model_json.sha256.as_str(),
            published[id]["pack"]["model_json_sha256"]
                .as_str()
                .expect("pin"),
            "{id}: the offer does not pin the published description"
        );
        assert!(
            offers
                .registry()
                .blob_url(id, &offer.model_json.sha256)
                .starts_with(&format!(
                    "https://{host}/v2/{}/{id}/blobs/sha256:",
                    &registry[host.len() + 1..]
                )),
            "{id}: the offer is downloaded from another registry"
        );
    }

    // The `model` setting offers the same ids, with the same default.
    let manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string(embedder_dir().join("plugin.toml")).expect("toml"))
            .expect("plugin.toml");
    let setting = manifest["settings"]
        .as_array()
        .expect("settings")
        .iter()
        .find(|setting| setting["key"].as_str() == Some("model"))
        .expect("the model setting");
    let mut values: Vec<&str> = setting["values"]
        .as_array()
        .expect("values")
        .iter()
        .map(|value| value["id"].as_str().expect("id"))
        .collect();
    values.sort_unstable();
    assert_eq!(
        values, offered,
        "the setting's values and the offers differ"
    );
    assert_eq!(setting["default"].as_str(), Some(offers.default_model()));
    for value in setting["values"].as_array().expect("values") {
        let id = value["id"].as_str().expect("id");
        assert_eq!(
            value["label"].as_str(),
            offers.get(id).map(|offer| offer.label.as_str()),
            "{id}: the setting and the offer are labeled differently"
        );
    }
}
