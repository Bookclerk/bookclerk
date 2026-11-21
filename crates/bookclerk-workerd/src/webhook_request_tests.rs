//! Checks of the public webhook route's request rules.

use super::*;

fn row(signature: WebhookSignature) -> WebhookManifest {
    WebhookManifest {
        name: "events".into(),
        max_body_bytes: 64,
        rate_per_minute: 2,
        retention_secs: 3600,
        headers: vec!["x-bz-event-notification-timestamp".into()],
        signature,
    }
}

fn signed(encoding: SignatureEncoding, prefix: &str) -> WebhookManifest {
    row(WebhookSignature::HmacSha256(HmacSignature {
        header: "x-bz-event-notification-signature".into(),
        prefix: prefix.into(),
        encoding,
        secret_setting: "signing_secret".into(),
    }))
}

fn sign(secret: &[u8], body: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret).unwrap();
    mac.update(body);
    mac.finalize().into_bytes().to_vec()
}

fn header(name: &str, value: &str) -> (String, Vec<u8>) {
    (name.to_string(), value.as_bytes().to_vec())
}

#[test]
fn the_head_refuses_other_methods_and_declared_oversize() {
    let row = row(WebhookSignature::None);
    assert_eq!(check_webhook_head(&row, "POST", Some(64)), Ok(()));
    assert_eq!(check_webhook_head(&row, "POST", None), Ok(()));
    assert_eq!(
        check_webhook_head(&row, "GET", None),
        Err(WebhookRefusal::MethodNotAllowed)
    );
    assert_eq!(
        check_webhook_head(&row, "POST", Some(65)),
        Err(WebhookRefusal::PayloadTooLarge)
    );
}

#[test]
fn unsigned_rows_keep_the_listed_headers() {
    let row = row(WebhookSignature::None);
    let kept = verify_webhook_request(
        &row,
        None,
        &[
            header("Content-Type", "application/json"),
            header("X-Bz-Event-Notification-Timestamp", "1700000000"),
            header("Authorization", "Bearer nope"),
        ],
        b"{}",
    )
    .unwrap();
    assert_eq!(
        kept,
        vec![
            WebhookHeader {
                name: "content-type".into(),
                value: "application/json".into(),
            },
            WebhookHeader {
                name: "x-bz-event-notification-timestamp".into(),
                value: "1700000000".into(),
            },
        ]
    );
    assert_eq!(
        verify_webhook_request(&row, None, &[], &[0; 65]),
        Err(WebhookRefusal::PayloadTooLarge)
    );
    assert_eq!(
        verify_webhook_request(
            &row,
            None,
            &[header("content-type", "a"), header("content-type", "b")],
            b"{}",
        ),
        Err(WebhookRefusal::BadRequest)
    );
    assert_eq!(
        verify_webhook_request(&row, None, &[header("content-type", "a\u{1}b")], b"{}"),
        Err(WebhookRefusal::BadRequest)
    );
}

#[test]
fn hmac_signatures_are_checked_in_both_encodings() {
    let secret = b"shared-secret";
    let body = br#"{"events":[]}"#;
    let digest = sign(secret, body);
    let hex_row = signed(SignatureEncoding::Hex, "v1=");
    let good = format!("v1={}", hex::encode_upper(&digest));
    assert!(verify_webhook_request(
        &hex_row,
        Some(secret),
        &[header("x-bz-event-notification-signature", &good)],
        body,
    )
    .is_ok());
    let b64_row = signed(SignatureEncoding::Base64, "");
    let good = base64::engine::general_purpose::STANDARD.encode(&digest);
    assert!(verify_webhook_request(
        &b64_row,
        Some(secret),
        &[header("x-bz-event-notification-signature", &good)],
        body,
    )
    .is_ok());

    for (value, refusal) in [
        (hex::encode(&digest), WebhookRefusal::BadSignature),
        (
            format!("v1={}", hex::encode(sign(b"other", body))),
            WebhookRefusal::BadSignature,
        ),
        ("v1=zz".to_string(), WebhookRefusal::BadSignature),
        (
            format!("v1={}", hex::encode(&digest[..16])),
            WebhookRefusal::BadSignature,
        ),
    ] {
        assert_eq!(
            verify_webhook_request(
                &hex_row,
                Some(secret),
                &[header("x-bz-event-notification-signature", &value)],
                body,
            ),
            Err(refusal),
            "{value}"
        );
    }
    assert_eq!(
        verify_webhook_request(&hex_row, Some(secret), &[], body),
        Err(WebhookRefusal::BadSignature)
    );
    assert_eq!(
        verify_webhook_request(&hex_row, None, &[], body),
        Err(WebhookRefusal::Unavailable)
    );
    let good = format!("v1={}", hex::encode(&digest));
    assert_eq!(
        verify_webhook_request(
            &hex_row,
            Some(secret),
            &[
                header("x-bz-event-notification-signature", &good),
                header("X-Bz-Event-Notification-Signature", &good),
            ],
            body,
        ),
        Err(WebhookRefusal::BadRequest)
    );
}

#[test]
fn the_rate_limiter_refills_over_a_minute() {
    let mut limiter = WebhookRateLimiter::default();
    assert!(limiter.take("i", "events", 2, 0).is_ok());
    assert!(limiter.take("i", "events", 2, 0).is_ok());
    assert_eq!(
        limiter.take("i", "events", 2, 1),
        Err(WebhookRefusal::RateLimited)
    );
    assert!(
        limiter.take("other", "events", 2, 1).is_ok(),
        "buckets are per instance"
    );
    assert!(limiter.take("i", "events", 2, 30_000).is_ok());
    assert_eq!(
        limiter.take("i", "events", 2, 30_001),
        Err(WebhookRefusal::RateLimited)
    );
    // A clock that steps back never refills.
    assert_eq!(
        limiter.take("i", "events", 2, 0),
        Err(WebhookRefusal::RateLimited)
    );
}

#[test]
fn every_refusal_has_its_status() {
    for (refusal, status) in [
        (WebhookRefusal::MethodNotAllowed, 405),
        (WebhookRefusal::NotFound, 404),
        (WebhookRefusal::RateLimited, 429),
        (WebhookRefusal::PayloadTooLarge, 413),
        (WebhookRefusal::BadRequest, 400),
        (WebhookRefusal::BadSignature, 401),
        (WebhookRefusal::Unavailable, 503),
    ] {
        assert_eq!(refusal.status(), status);
    }
}
