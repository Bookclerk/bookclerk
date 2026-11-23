//! Shared oracle for the `cloudflare_api` fuzz target and the
//! `bookclerk-storage` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is what the `r2` guest's guided setup reads from the
//! Cloudflare API. Byte 0 modulo 6 picks the reader: the bare envelope (0),
//! an account list (1), a bucket create reply (2), a permission group list
//! searched for the bucket item group (3), a token create reply (4), or a
//! bucket name (5). The rest is the bytes.
//!
//! The invariants:
//!
//! - reading never panics and answers the same twice;
//! - a reply over the cap is refused as too large;
//! - a refusal keeps at most the error cap, each message within its
//!   length without control characters, and only when the reply's own
//!   `success` is not `true`;
//! - an accepted reply says `success: true`, and what it yields agrees
//!   with an independent walk of the same JSON: account ids, a group id and
//!   a token id are the reply's own 32 hex digits, lowercased; the token
//!   value is the reply's own printable value, and its S3 secret is the
//!   oracle's own SHA-256 of it; a bucket create counts as already owned
//!   exactly when the reply refused with code 10004;
//! - a bucket name passes exactly when the oracle's own reading of R2's
//!   naming rules accepts it.

use bookclerk_storage::cloudflare_api::{
    check_bucket_name, find_permission_group, parse_accounts, parse_bucket_created, parse_envelope,
    parse_minted_token, ApiReplyError, BucketCreated, BUCKET_ALREADY_OWNED, BUCKET_ITEM_WRITE,
    MAX_API_ERRORS, MAX_API_ERROR_MESSAGE_CHARS, MAX_API_REPLY_BYTES, MAX_TOKEN_VALUE_BYTES,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Checks one input; panics when an invariant fails.
pub fn check(data: &[u8]) {
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    match selector % 6 {
        0 => {
            let verdict = parse_envelope(rest);
            assert_eq!(verdict, parse_envelope(rest), "deterministic");
            common(rest, verdict.as_ref().map(|_| ()));
            if let Ok(result) = verdict {
                assert_eq!(Some(result), reply_value(rest).as_ref().map(result_of));
            }
        }
        1 => {
            let verdict = parse_accounts(rest);
            assert_eq!(verdict, parse_accounts(rest), "deterministic");
            common(rest, verdict.as_ref().map(|_| ()));
            if let Ok(accounts) = verdict {
                let value = reply_value(rest).expect("accepted reply is JSON");
                let list = result_of(&value).as_array().cloned().expect("a list");
                assert_eq!(list.len(), accounts.len());
                for (entry, account) in list.iter().zip(&accounts) {
                    let id = entry["id"].as_str().expect("an id");
                    assert!(hex_id(id));
                    assert_eq!(account.id, id.to_ascii_lowercase());
                    assert!(!account.name.chars().any(char::is_control));
                }
            }
        }
        2 => {
            let verdict = parse_bucket_created(rest);
            assert_eq!(verdict, parse_bucket_created(rest), "deterministic");
            match &verdict {
                Ok(BucketCreated::Created) => assert!(succeeded(rest)),
                Ok(BucketCreated::AlreadyOwned) => {
                    let value = reply_value(rest).expect("refusal is JSON");
                    assert_ne!(value.get("success"), Some(&Value::Bool(true)));
                    assert!(value["errors"].as_array().is_some_and(|errors| errors
                        .iter()
                        .take(MAX_API_ERRORS)
                        .any(
                            |e| e.get("code").and_then(Value::as_i64) == Some(BUCKET_ALREADY_OWNED)
                        )));
                }
                Err(err) => {
                    assert!(!err.has_code(BUCKET_ALREADY_OWNED));
                    common(rest, Err(err));
                }
            }
        }
        3 => {
            let verdict = find_permission_group(rest, BUCKET_ITEM_WRITE);
            assert_eq!(verdict, find_permission_group(rest, BUCKET_ITEM_WRITE));
            common(rest, verdict.as_ref().map(|_| ()));
            if let Ok(found) = verdict {
                let value = reply_value(rest).expect("accepted reply is JSON");
                let first = result_of(&value)
                    .as_array()
                    .expect("a list")
                    .iter()
                    .find(|entry| {
                        entry.get("name").and_then(Value::as_str) == Some(BUCKET_ITEM_WRITE)
                    })
                    .map(|entry| entry["id"].as_str().expect("an id").to_ascii_lowercase());
                assert_eq!(found, first);
            }
        }
        4 => {
            let verdict = parse_minted_token(rest);
            assert_eq!(verdict, parse_minted_token(rest), "deterministic");
            common(rest, verdict.as_ref().map(|_| ()));
            if let Ok(minted) = verdict {
                let value = reply_value(rest).expect("accepted reply is JSON");
                let result = result_of(&value);
                let id = result["id"].as_str().expect("an id");
                assert!(hex_id(id));
                assert_eq!(minted.id, id.to_ascii_lowercase());
                let token = result["value"].as_str().expect("a value");
                assert_eq!(minted.value, token);
                assert!(!token.is_empty() && token.len() <= MAX_TOKEN_VALUE_BYTES);
                assert!(token.bytes().all(|b| b.is_ascii_graphic()));
                let digest = Sha256::digest(token.as_bytes());
                let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
                assert_eq!(minted.secret_access_key(), hex);
                assert!(
                    format!("{minted:?}").contains("<redacted>"),
                    "Debug hides the value"
                );
            }
        }
        _ => {
            let Ok(name) = std::str::from_utf8(rest) else {
                return;
            };
            assert_eq!(
                check_bucket_name(name).is_ok(),
                bucket_name_ok(name),
                "{name:?}"
            );
        }
    }
}

/// The reply as a JSON value, when it parses.
fn reply_value(reply: &[u8]) -> Option<Value> {
    if reply.len() > MAX_API_REPLY_BYTES {
        return None;
    }
    serde_json::from_slice(reply).ok()
}

/// The envelope's `result`, `null` when missing.
fn result_of(value: &Value) -> Value {
    value.get("result").cloned().unwrap_or(Value::Null)
}

/// Whether the reply parses as an object whose `success` is `true`.
fn succeeded(reply: &[u8]) -> bool {
    reply_value(reply).is_some_and(|value| value.get("success") == Some(&Value::Bool(true)))
}

/// Whether `id` is 32 hex digits.
fn hex_id(id: &str) -> bool {
    id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())
}

/// The invariants every reader shares.
fn common(reply: &[u8], verdict: Result<(), &ApiReplyError>) {
    if reply.len() > MAX_API_REPLY_BYTES {
        assert_eq!(verdict, Err(&ApiReplyError::TooLarge));
        return;
    }
    match verdict {
        Ok(()) => assert!(succeeded(reply), "accepted replies say success"),
        Err(ApiReplyError::Refused(faults)) => {
            assert!(!succeeded(reply), "refused replies do not say success");
            assert!(faults.len() <= MAX_API_ERRORS);
            for fault in faults {
                assert!(fault.message.chars().count() <= MAX_API_ERROR_MESSAGE_CHARS);
                assert!(!fault.message.chars().any(char::is_control));
            }
        }
        Err(ApiReplyError::TooLarge) => panic!("a reply within the cap is not too large"),
        Err(ApiReplyError::Malformed(_)) => {}
    }
}

/// R2's naming rules, read independently: 3 to 63 characters from
/// `a-z`, `0-9` and `-`, with a letter or digit at each end.
fn bucket_name_ok(name: &str) -> bool {
    let chars: Vec<char> = name.chars().collect();
    let allowed = |c: &char| matches!(c, 'a'..='z' | '0'..='9' | '-');
    (3..=63).contains(&chars.len())
        && chars.iter().all(allowed)
        && chars.first().is_some_and(|c| *c != '-')
        && chars.last().is_some_and(|c| *c != '-')
}
