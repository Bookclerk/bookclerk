//! Shared oracle for the `webhook_request` fuzz target and the
//! `bookclerk-workerd` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one request on the public webhook route
//! (`POST /hooks/<token>/<name>`), from anyone on the Internet. The first
//! byte picks the row's signature rule (bits 0-1: none, hex, `v1=`-prefixed
//! hex, base64) and whether the oracle adds a correct signature header
//! itself (bit 2). The rest splits on `0xff` into the signing secret, the
//! body, and header pairs (`name`, `0x00`, `value`).
//!
//! [`verify_webhook_request`] must:
//!
//! - never panic;
//! - refuse a body over the row's `max_body_bytes` with 413, before
//!   anything else;
//! - refuse a signed row with no secret with 503;
//! - accept a signed request only when its one signature header carries
//!   the HMAC-SHA256 of the body under the secret, computed here from
//!   SHA-256 alone (RFC 2104), not with the `hmac` crate the route uses;
//! - accept the correct signature it was given unless a header repeats or
//!   a kept value is malformed;
//! - keep exactly the headers it was given among `content-type` and the
//!   row's list, lowercase, `content-type` first, values unchanged, and
//!   only values of at most `maxWebhookHeaderValueBytes` of UTF-8 with no
//!   control character but tab.

use base64::Engine as _;
use bookclerk_plugin_abi::MAX_WEBHOOK_HEADER_VALUE_BYTES;
use bookclerk_plugin_manifest::{
    HmacSignature, SignatureEncoding, WebhookManifest, WebhookSignature,
};
use bookclerk_workerd::webhook_request::{verify_webhook_request, WebhookRefusal};
use sha2::{Digest, Sha256};

/// Body cap of the row under test, small so the fuzzer reaches it.
const MAX_BODY: u32 = 256;

/// Header the row signs with.
const SIGNATURE_HEADER: &str = "x-signature";

/// Headers the row keeps besides `content-type`.
const KEPT: [&str; 2] = ["x-event", "x-timestamp"];

/// HMAC-SHA256 from SHA-256 alone (RFC 2104, 64-byte blocks).
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// The row under test for `mode`.
fn row(mode: u8) -> WebhookManifest {
    let rule = |encoding, prefix: &str| {
        WebhookSignature::HmacSha256(HmacSignature {
            header: SIGNATURE_HEADER.into(),
            prefix: prefix.into(),
            encoding,
            secret_setting: "secret".into(),
        })
    };
    WebhookManifest {
        name: "events".into(),
        max_body_bytes: MAX_BODY,
        rate_per_minute: 60,
        retention_secs: 3600,
        headers: KEPT.iter().map(|name| (*name).to_string()).collect(),
        signature: match mode & 3 {
            0 => WebhookSignature::None,
            1 => rule(SignatureEncoding::Hex, ""),
            2 => rule(SignatureEncoding::Hex, "v1="),
            _ => rule(SignatureEncoding::Base64, ""),
        },
    }
}

/// The header value carrying `digest` under `row`'s rule.
fn encode(row: &WebhookManifest, digest: &[u8]) -> Option<String> {
    match &row.signature {
        WebhookSignature::None => None,
        WebhookSignature::HmacSha256(rule) => Some(match rule.encoding {
            SignatureEncoding::Hex => format!("{}{}", rule.prefix, hex_lower(digest)),
            SignatureEncoding::Base64 => format!(
                "{}{}",
                rule.prefix,
                base64::engine::general_purpose::STANDARD.encode(digest)
            ),
        }),
    }
}

/// Lowercase hex, restated.
fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decodes a signature value the way the rule says (prefix, then hex in
/// either case or standard base64), restated.
fn decode(rule: &HmacSignature, value: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(value).ok()?.trim();
    let digest = text.strip_prefix(rule.prefix.as_str())?;
    match rule.encoding {
        SignatureEncoding::Hex => {
            if digest.len() % 2 != 0 {
                return None;
            }
            (0..digest.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(digest.get(i..i + 2)?, 16).ok())
                .collect()
        }
        SignatureEncoding::Base64 => base64::engine::general_purpose::STANDARD
            .decode(digest)
            .ok(),
    }
}

/// A kept value the route may accept, restated.
fn value_ok(value: &[u8]) -> bool {
    let cap = usize::try_from(MAX_WEBHOOK_HEADER_VALUE_BYTES).unwrap_or(usize::MAX);
    value.len() <= cap
        && std::str::from_utf8(value)
            .is_ok_and(|text| !text.chars().any(|c| c.is_control() && c != '\t'))
}

/// Checks the route's request invariants for one input.
///
/// # Panics
///
/// Panics when an invariant fails.
pub fn check(data: &[u8]) {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let mut parts = rest.split(|b| *b == 0xff);
    let secret = parts.next().unwrap_or_default();
    let body = parts.next().unwrap_or_default();
    let mut headers: Vec<(String, Vec<u8>)> = parts
        .filter_map(|pair| {
            let at = pair.iter().position(|b| *b == 0)?;
            let name = std::str::from_utf8(&pair[..at]).ok()?;
            Some((name.to_string(), pair[at + 1..].to_vec()))
        })
        .collect();
    let row = row(mode);
    let correct = hmac_sha256(secret, body);
    let injected = mode & 4 != 0;
    if injected {
        if let Some(value) = encode(&row, &correct) {
            headers.push((SIGNATURE_HEADER.to_string(), value.into_bytes()));
        }
    }
    let secret_opt = (!secret.is_empty()).then_some(secret);
    let result = verify_webhook_request(&row, secret_opt, &headers, body);

    let over = body.len() > usize::try_from(MAX_BODY).unwrap_or(usize::MAX);
    if over {
        assert_eq!(
            result,
            Err(WebhookRefusal::PayloadTooLarge),
            "oversize body"
        );
        return;
    }
    assert_ne!(
        result,
        Err(WebhookRefusal::PayloadTooLarge),
        "a body within the cap refused as too large"
    );
    let matching = |name: &str| {
        headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_slice())
            .collect::<Vec<_>>()
    };
    if let WebhookSignature::HmacSha256(rule) = &row.signature {
        if secret_opt.is_none() {
            assert_eq!(result, Err(WebhookRefusal::Unavailable), "no secret");
            return;
        }
        let values = matching(SIGNATURE_HEADER);
        if result.is_ok() {
            assert_eq!(values.len(), 1, "accepted without exactly one signature");
            assert_eq!(
                decode(rule, values[0]).as_deref(),
                Some(&correct[..]),
                "accepted a wrong signature"
            );
        }
        if injected && values.len() == 1 {
            assert_ne!(
                result,
                Err(WebhookRefusal::BadSignature),
                "refused the correct signature"
            );
        }
    } else {
        assert!(
            !matches!(
                result,
                Err(WebhookRefusal::BadSignature | WebhookRefusal::Unavailable)
            ),
            "an unsigned row refused as {result:?}"
        );
    }
    let names = std::iter::once("content-type").chain(KEPT);
    match &result {
        Ok(kept) => {
            let mut expected = Vec::new();
            for name in names {
                let values = matching(name);
                assert!(values.len() <= 1, "accepted a repeated {name}");
                if let Some(value) = values.first() {
                    assert!(value_ok(value), "kept a malformed {name}");
                    expected.push((name.to_string(), value.to_vec()));
                }
            }
            let got: Vec<(String, Vec<u8>)> = kept
                .iter()
                .map(|header| (header.name.clone(), header.value.clone().into_bytes()))
                .collect();
            assert_eq!(got, expected, "kept headers");
        }
        Err(WebhookRefusal::BadRequest) => {
            let signature_repeats = matches!(row.signature, WebhookSignature::HmacSha256(_))
                && matching(SIGNATURE_HEADER).len() > 1;
            let kept_bad = names.into_iter().any(|name| {
                let values = matching(name);
                values.len() > 1 || values.iter().any(|value| !value_ok(value))
            });
            assert!(
                signature_repeats || kept_bad,
                "refused a well-formed request"
            );
        }
        Err(
            WebhookRefusal::BadSignature
            | WebhookRefusal::Unavailable
            | WebhookRefusal::PayloadTooLarge,
        ) => {}
        Err(
            refusal @ (WebhookRefusal::MethodNotAllowed
            | WebhookRefusal::NotFound
            | WebhookRefusal::RateLimited),
        ) => panic!("verify answered a route-level refusal {refusal:?}"),
    }
}
