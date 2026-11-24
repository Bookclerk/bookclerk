//! Shared oracle for the `cloudflare_api` fuzz target and the
//! `bookclerk-storage` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is what the `r2` and `d1` guests' guided setups read from the
//! Cloudflare API. Byte 0 modulo 10 picks the reader: the bare envelope (0),
//! an account list (1), a bucket create reply (2), a permission group list
//! searched for the bucket item group (3), a token create reply (4), a
//! bucket name (5), a D1 database list searched for `books` (6), a D1
//! create reply (7), a D1 database name (8), or a permission group list
//! searched for the D1 write group (9). The rest is the bytes.
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
//! - a D1 database id is the reply's own UUID (hyphens at 8, 13, 18 and
//!   23, hex elsewhere), lowercased, and a list search yields the first
//!   entry named exactly `books`;
//! - a bucket name and a D1 database name pass exactly when the oracle's
//!   own reading of the naming rules accepts them.

use bookclerk_storage::cloudflare_api::{
    check_bucket_name, check_d1_database_name, find_d1_database, find_permission_group,
    parse_accounts, parse_bucket_created, parse_d1_created, parse_envelope, parse_minted_token,
    ApiReplyError, BucketCreated, BUCKET_ALREADY_OWNED, BUCKET_ITEM_WRITE, D1_WRITE,
    MAX_API_ERRORS, MAX_API_ERROR_MESSAGE_CHARS, MAX_API_REPLY_BYTES, MAX_D1_DATABASE_NAME_CHARS,
    MAX_TOKEN_VALUE_BYTES,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Checks one input; panics when an invariant fails.
pub fn check(data: &[u8]) {
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    match selector % 10 {
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
        3 => group(rest, BUCKET_ITEM_WRITE),
        9 => group(rest, D1_WRITE),
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
        6 => {
            let verdict = find_d1_database(rest, "books");
            assert_eq!(verdict, find_d1_database(rest, "books"), "deterministic");
            common(rest, verdict.as_ref().map(|_| ()));
            if let Ok(found) = verdict {
                let value = reply_value(rest).expect("accepted reply is JSON");
                let first = result_of(&value)
                    .as_array()
                    .expect("a list")
                    .iter()
                    .find(|entry| entry.get("name").and_then(Value::as_str) == Some("books"))
                    .map(|entry| entry["uuid"].as_str().expect("a uuid").to_string());
                assert_eq!(found.is_some(), first.is_some());
                if let (Some(found), Some(first)) = (found, first) {
                    assert!(uuid_ok(&first));
                    assert_eq!(found, first.to_ascii_lowercase());
                }
            }
        }
        7 => {
            let verdict = parse_d1_created(rest);
            assert_eq!(verdict, parse_d1_created(rest), "deterministic");
            common(rest, verdict.as_ref().map(|_| ()));
            if let Ok(id) = verdict {
                let value = reply_value(rest).expect("accepted reply is JSON");
                let own = result_of(&value)["uuid"]
                    .as_str()
                    .expect("a uuid")
                    .to_string();
                assert!(uuid_ok(&own));
                assert_eq!(id, own.to_ascii_lowercase());
            }
        }
        8 => {
            let Ok(name) = std::str::from_utf8(rest) else {
                return;
            };
            assert_eq!(
                check_d1_database_name(name).is_ok(),
                d1_name_ok(name),
                "{name:?}"
            );
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

/// A permission group search for `name`: the first entry of that name's
/// id, lowercased.
fn group(rest: &[u8], name: &str) {
    let verdict = find_permission_group(rest, name);
    assert_eq!(verdict, find_permission_group(rest, name), "deterministic");
    common(rest, verdict.as_ref().map(|_| ()));
    if let Ok(found) = verdict {
        let value = reply_value(rest).expect("accepted reply is JSON");
        let first = result_of(&value)
            .as_array()
            .expect("a list")
            .iter()
            .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
            .map(|entry| entry["id"].as_str().expect("an id").to_ascii_lowercase());
        assert_eq!(found, first);
    }
}

/// A UUID, read independently: 36 characters, `-` at 8, 13, 18 and 23,
/// hex digits elsewhere.
fn uuid_ok(id: &str) -> bool {
    let chars: Vec<char> = id.chars().collect();
    chars.len() == 36
        && chars.iter().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                *c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// D1's naming rules as the setup reads them: 1 to 64 characters from
/// `a-z`, `0-9`, `-` and `_`, starting with a letter or digit.
fn d1_name_ok(name: &str) -> bool {
    let chars: Vec<char> = name.chars().collect();
    (1..=MAX_D1_DATABASE_NAME_CHARS).contains(&chars.len())
        && chars
            .iter()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-' | '_'))
        && chars
            .first()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
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
