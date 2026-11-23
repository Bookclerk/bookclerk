//! Shared oracle for the `model_fetch` fuzz target and the
//! `bookclerk-plugin-catalog` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The host downloads embedding models for a plugin that has no network.
//! Two inputs on that path are parsed before anything is verified by
//! digest. The first input byte picks which one the rest is read as:
//!
//! - `0`: a registry's token reply. The token goes into an `Authorization`
//!   header, so an accepted token is 1 to [`MAX_TOKEN_BYTES`] bytes of the
//!   characters RFC 6750 allows, by a rule written apart from the crate's,
//!   and is the `token` string of the reply read as plain JSON.
//! - anything else: a plugin package's model list (`models.json`). An
//!   accepted list offers 1 to [`MAX_MODEL_OFFERS`] models, its default
//!   among them; every model id is a single directory name by a rule
//!   written apart from the crate's; and every URL the host would request
//!   for it is `https://` on the listed registry host, with nothing a
//!   model id or the registry text could smuggle in (no credentials, no
//!   second query, no fragment, no white space).

use bookclerk_plugin_catalog::{
    parse_token_reply, ModelOffers, MAX_MODEL_BYTES, MAX_MODEL_OFFERS, MAX_MODEL_OFFERS_BYTES,
    MAX_TOKEN_BYTES, MAX_TOKEN_REPLY_BYTES,
};

/// Largest input worth checking: a little over both parsers' own limits,
/// so the limits themselves are exercised.
pub const MAX_INPUT_BYTES: usize = 80 * 1024;

/// Whether `token` may follow `Bearer ` in a header (RFC 6750 `b64token`
/// characters).
fn header_safe(token: &str) -> bool {
    const ALLOWED: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~+/=";
    !token.is_empty() && token.chars().all(|c| ALLOWED.contains(c))
}

/// Whether `id` is one plain directory name: lowercase letters, digits,
/// `.`, `_` and `-`, not starting with a dot or dash.
fn plain_name(id: &str) -> bool {
    const ALLOWED: &str = "abcdefghijklmnopqrstuvwxyz0123456789._-";
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| ALLOWED.contains(c))
        && !id.starts_with(['.', '-', '_'])
}

/// Checks a token reply.
fn check_token(body: &[u8]) {
    let Ok(token) = parse_token_reply(body) else {
        return;
    };
    assert!(
        body.len() <= MAX_TOKEN_REPLY_BYTES,
        "a reply over the limit was read"
    );
    let token = token.as_str();
    assert!(token.len() <= MAX_TOKEN_BYTES, "token over the limit");
    assert!(header_safe(token), "token {token:?} is not header safe");
    let plain: serde_json::Value = serde_json::from_slice(body).expect("an accepted reply is JSON");
    assert_eq!(
        plain.get("token").and_then(serde_json::Value::as_str),
        Some(token),
        "the token is not the reply's `token`"
    );
}

/// Checks one URL the host would request: `https://<host>/...` with only
/// the characters of a path the registry and model ids may hold.
fn check_url(url: &str, host: &str, query: bool) {
    let rest = url
        .strip_prefix("https://")
        .unwrap_or_else(|| panic!("{url:?} is not https"));
    let (authority, path) = rest
        .split_once('/')
        .unwrap_or_else(|| panic!("{url:?} has no path"));
    assert_eq!(authority, host, "{url:?} goes to another host");
    let (path, query_text) = match path.split_once('?') {
        Some((path, query_text)) => (path, Some(query_text)),
        None => (path, None),
    };
    assert_eq!(query_text.is_some(), query, "{url:?}: unexpected query");
    let plain = |text: &str, extra: &str| {
        text.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || extra.contains(c))
    };
    assert!(plain(path, "._-/:"), "{url:?}: odd path");
    assert!(
        !path.split('/').any(|part| part == ".." || part == "."),
        "{url:?}: the path climbs"
    );
    if let Some(query_text) = query_text {
        assert!(plain(query_text, "._-/:=&"), "{url:?}: odd query");
    }
}

/// Checks a model list.
fn check_offers(bytes: &[u8]) {
    let Ok(offers) = ModelOffers::parse(bytes) else {
        return;
    };
    assert!(
        bytes.len() <= MAX_MODEL_OFFERS_BYTES,
        "a list over the limit was read"
    );
    assert_eq!(
        Some(&offers),
        ModelOffers::parse(bytes).ok().as_ref(),
        "the same bytes read differently"
    );
    let plain: serde_json::Value = serde_json::from_slice(bytes).expect("an accepted list is JSON");
    let registry_text = plain
        .get("registry")
        .and_then(serde_json::Value::as_str)
        .expect("an accepted list names its registry");
    let host = registry_text.split('/').next().unwrap_or_default();
    assert!(
        !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-'),
        "registry host {host:?} is not a host name"
    );

    let count = offers.models().count();
    assert!(
        (1..=MAX_MODEL_OFFERS).contains(&count),
        "{count} models offered"
    );
    assert!(
        offers.get(offers.default_model()).is_some(),
        "the default is not offered"
    );
    let registry = offers.registry();
    for (id, offer) in offers.models() {
        assert!(plain_name(id), "model id {id:?} is not a directory name");
        for text in [&offer.label, &offer.summary] {
            assert!(
                !text.is_empty() && !text.chars().any(char::is_control),
                "label or summary {text:?} cannot be shown"
            );
        }
        assert!(offer.download_bytes <= MAX_MODEL_BYTES, "model over limit");
        check_url(&registry.token_url(id), host, true);
        check_url(
            &registry.blob_url(id, &offer.model_json.sha256),
            host,
            false,
        );
    }
}

/// Runs the oracle on one input.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((kind, rest)) = data.split_first() else {
        return;
    };
    if *kind == 0 {
        check_token(rest);
    } else {
        check_offers(rest);
    }
}
