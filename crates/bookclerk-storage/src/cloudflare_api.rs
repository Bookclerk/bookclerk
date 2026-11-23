//! Cloudflare API replies the `r2` destination's guided setup reads, and
//! the request parts it builds.
//!
//! The guided setup calls the Cloudflare v4 API with the operator's token:
//! it finds the account, creates (or reuses) the bucket, looks up the
//! bucket-scoped permission group, mints a token for that one bucket, and
//! deletes the token it minted on an earlier run. Every reply comes from
//! outside Bookclerk, so this module is the boundary that reads them: pure
//! functions over bytes, with no I/O, fuzzed by the `cloudflare_api`
//! target. The caller does the HTTP.
//!
//! Every reply is the v4 envelope:
//!
//! ```json
//! {"success": true, "errors": [], "messages": [], "result": ...}
//! ```
//!
//! A failed call keeps only its error codes and short, single-line
//! messages; nothing else from a failed reply reaches the operator.

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Largest reply read, in bytes. The longest reply the setup reads is a
/// filtered permission group list, a few kilobytes.
pub const MAX_API_REPLY_BYTES: usize = 1024 * 1024;

/// Most entries a list reply may hold.
pub const MAX_API_LIST_ENTRIES: usize = 1000;

/// Most errors kept from a failed reply.
pub const MAX_API_ERRORS: usize = 4;

/// Longest error message kept, in characters.
pub const MAX_API_ERROR_MESSAGE_CHARS: usize = 160;

/// Longest token value accepted from a create reply, in bytes.
pub const MAX_TOKEN_VALUE_BYTES: usize = 1024;

/// The error code the bucket create call answers when the caller already
/// owns a bucket of that name.
pub const BUCKET_ALREADY_OWNED: i64 = 10004;

/// The permission group that lets a token read and write the objects of
/// the buckets it names, and nothing else.
pub const BUCKET_ITEM_WRITE: &str = "Workers R2 Storage Bucket Item Write";

/// One error a failed reply reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiFault {
    /// Cloudflare's error code; `None` when the reply gave none.
    pub code: Option<i64>,
    /// The message, cut to [`MAX_API_ERROR_MESSAGE_CHARS`] with control
    /// characters removed.
    pub message: String,
}

impl std::fmt::Display for ApiFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => write!(f, "{code} {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Why a reply could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiReplyError {
    /// The reply is over [`MAX_API_REPLY_BYTES`].
    #[error("the Cloudflare API reply is over {MAX_API_REPLY_BYTES} bytes")]
    TooLarge,
    /// The reply is not the envelope, or its result has the wrong shape.
    #[error("the Cloudflare API reply is malformed: {0}")]
    Malformed(String),
    /// The API reported failure.
    #[error("the Cloudflare API refused: {}", faults_text(.0))]
    Refused(Vec<ApiFault>),
}

impl ApiReplyError {
    /// Whether the API refused with `code`.
    #[must_use]
    pub fn has_code(&self, code: i64) -> bool {
        match self {
            Self::Refused(faults) => faults.iter().any(|fault| fault.code == Some(code)),
            Self::TooLarge | Self::Malformed(_) => false,
        }
    }
}

/// The faults joined for a message.
fn faults_text(faults: &[ApiFault]) -> String {
    if faults.is_empty() {
        return "no reason given".into();
    }
    faults
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// The envelope's members the setup reads.
#[derive(Deserialize)]
struct Envelope {
    /// Whether the call succeeded.
    success: bool,
    /// Errors the API reported.
    #[serde(default)]
    errors: Vec<serde_json::Value>,
    /// The call's result.
    #[serde(default)]
    result: serde_json::Value,
}

/// An error's text, single-line and short.
fn clean_message(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_API_ERROR_MESSAGE_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

/// One error entry, read leniently: a fault is kept even when its members
/// have unexpected types.
fn fault_of(entry: &serde_json::Value) -> ApiFault {
    ApiFault {
        code: entry.get("code").and_then(serde_json::Value::as_i64),
        message: entry
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(clean_message)
            .unwrap_or_default(),
    }
}

/// Reads the envelope and returns its result.
///
/// The reply is parsed as a JSON tree first, so a repeated member counts
/// once, as its last value.
///
/// # Errors
///
/// [`ApiReplyError::TooLarge`] past the cap, [`ApiReplyError::Malformed`]
/// for anything but an envelope object, and [`ApiReplyError::Refused`] when
/// `success` is false.
pub fn parse_envelope(reply: &[u8]) -> Result<serde_json::Value, ApiReplyError> {
    if reply.len() > MAX_API_REPLY_BYTES {
        return Err(ApiReplyError::TooLarge);
    }
    let value: serde_json::Value =
        serde_json::from_slice(reply).map_err(|err| ApiReplyError::Malformed(err.to_string()))?;
    if !value.is_object() {
        return Err(ApiReplyError::Malformed(
            "the reply is not an object".into(),
        ));
    }
    let envelope =
        Envelope::deserialize(&value).map_err(|err| ApiReplyError::Malformed(err.to_string()))?;
    if !envelope.success {
        return Err(ApiReplyError::Refused(
            envelope
                .errors
                .iter()
                .take(MAX_API_ERRORS)
                .map(fault_of)
                .collect(),
        ));
    }
    Ok(envelope.result)
}

/// The result as a list of objects, capped.
fn list_of(result: serde_json::Value) -> Result<Vec<serde_json::Value>, ApiReplyError> {
    let serde_json::Value::Array(entries) = result else {
        return Err(ApiReplyError::Malformed("the result is not a list".into()));
    };
    if entries.len() > MAX_API_LIST_ENTRIES {
        return Err(ApiReplyError::Malformed(format!(
            "the result lists more than {MAX_API_LIST_ENTRIES} entries"
        )));
    }
    if entries.iter().any(|entry| !entry.is_object()) {
        return Err(ApiReplyError::Malformed(
            "a list entry is not an object".into(),
        ));
    }
    Ok(entries)
}

/// Whether `id` is a Cloudflare object id: 32 hex digits.
fn is_hex_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One account the token can see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    /// The account id, 32 lowercase hex digits.
    pub id: String,
    /// The account's name, cleaned like an error message.
    pub name: String,
}

/// Reads a `GET /accounts` reply.
///
/// # Errors
///
/// As [`parse_envelope`], and [`ApiReplyError::Malformed`] when an entry's
/// id is not 32 hex digits.
pub fn parse_accounts(reply: &[u8]) -> Result<Vec<AccountSummary>, ApiReplyError> {
    list_of(parse_envelope(reply)?)?
        .iter()
        .map(|entry| {
            let id = entry
                .get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| is_hex_id(id))
                .ok_or_else(|| {
                    ApiReplyError::Malformed("an account id is not 32 hex digits".into())
                })?;
            Ok(AccountSummary {
                id: id.to_ascii_lowercase(),
                name: entry
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(clean_message)
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// What a bucket create call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketCreated {
    /// The bucket was created.
    Created,
    /// The caller already owned a bucket of that name; it is reused.
    AlreadyOwned,
}

/// Reads a `POST /accounts/{id}/r2/buckets` reply.
///
/// # Errors
///
/// As [`parse_envelope`], except that [`BUCKET_ALREADY_OWNED`] is
/// [`BucketCreated::AlreadyOwned`].
pub fn parse_bucket_created(reply: &[u8]) -> Result<BucketCreated, ApiReplyError> {
    match parse_envelope(reply) {
        Ok(_) => Ok(BucketCreated::Created),
        Err(err) if err.has_code(BUCKET_ALREADY_OWNED) => Ok(BucketCreated::AlreadyOwned),
        Err(err) => Err(err),
    }
}

/// Reads a `GET /accounts/{id}/tokens/permission_groups` reply and returns
/// the id of the group named `name`.
///
/// # Returns
///
/// The group's id, or `None` when the reply lists no group of that name.
///
/// # Errors
///
/// As [`parse_envelope`], and [`ApiReplyError::Malformed`] when the
/// matching group's id is not 32 hex digits.
pub fn find_permission_group(reply: &[u8], name: &str) -> Result<Option<String>, ApiReplyError> {
    for entry in list_of(parse_envelope(reply)?)? {
        if entry.get("name").and_then(serde_json::Value::as_str) != Some(name) {
            continue;
        }
        let id = entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| is_hex_id(id))
            .ok_or_else(|| {
                ApiReplyError::Malformed("a permission group id is not 32 hex digits".into())
            })?;
        return Ok(Some(id.to_ascii_lowercase()));
    }
    Ok(None)
}

/// A token the create call minted.
#[derive(Clone, PartialEq, Eq)]
pub struct MintedToken {
    /// The token's id, which is the bucket's S3 access key id.
    pub id: String,
    /// The token's value, shown once; its SHA-256 is the S3 secret.
    pub value: String,
}

impl std::fmt::Debug for MintedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintedToken")
            .field("id", &self.id)
            .field("value", &"<redacted>")
            .finish()
    }
}

impl MintedToken {
    /// The S3 secret access key: the SHA-256 of the value, in lowercase hex.
    #[must_use]
    pub fn secret_access_key(&self) -> String {
        hex::encode(Sha256::digest(self.value.as_bytes()))
    }
}

/// Reads a `POST /accounts/{id}/tokens` reply.
///
/// # Errors
///
/// As [`parse_envelope`], and [`ApiReplyError::Malformed`] when the id is
/// not 32 hex digits or the value is empty, over
/// [`MAX_TOKEN_VALUE_BYTES`], or not printable ASCII.
pub fn parse_minted_token(reply: &[u8]) -> Result<MintedToken, ApiReplyError> {
    let result = parse_envelope(reply)?;
    let id = result
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| is_hex_id(id))
        .ok_or_else(|| ApiReplyError::Malformed("the token id is not 32 hex digits".into()))?;
    let value = result
        .get("value")
        .and_then(serde_json::Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_TOKEN_VALUE_BYTES
                && value.bytes().all(|b| b.is_ascii_graphic())
        })
        .ok_or_else(|| ApiReplyError::Malformed("the token value is not usable".into()))?;
    Ok(MintedToken {
        id: id.to_ascii_lowercase(),
        value: value.to_string(),
    })
}

/// Reads a reply whose result the setup does not need (a token delete).
///
/// # Errors
///
/// As [`parse_envelope`].
pub fn parse_done(reply: &[u8]) -> Result<(), ApiReplyError> {
    parse_envelope(reply).map(|_| ())
}

/// Checks an R2 bucket name: 3 to 63 lowercase letters, digits and hyphens,
/// not starting or ending with a hyphen.
///
/// # Errors
///
/// A sentence saying what is wrong.
pub fn check_bucket_name(name: &str) -> Result<(), String> {
    let bytes = name.as_bytes();
    if !(3..=63).contains(&bytes.len()) {
        return Err("a bucket name is 3 to 63 characters".into());
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        return Err("a bucket name holds only lowercase letters, digits and hyphens".into());
    }
    if bytes.first() == Some(&b'-') || bytes.last() == Some(&b'-') {
        return Err("a bucket name does not start or end with a hyphen".into());
    }
    Ok(())
}

/// The token policy resource that names one bucket.
///
/// # Arguments
///
/// * `account` - the account id.
/// * `jurisdiction` - `default`, `eu` or `fedramp`.
/// * `bucket` - a name [`check_bucket_name`] accepts.
#[must_use]
pub fn bucket_resource(account: &str, jurisdiction: &str, bucket: &str) -> String {
    format!("com.cloudflare.edge.r2.bucket.{account}_{jurisdiction}_{bucket}")
}

/// The body of the token create call: one policy allowing `group` on one
/// bucket.
#[must_use]
pub fn token_request(name: &str, group: &str, resource: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "policies": [{
            "effect": "allow",
            "permission_groups": [{ "id": group }],
            "resources": { resource: "*" },
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "3f4b7e3dcab231cbfdaa90a6a28bd548";

    #[test]
    fn envelopes_and_refusals() {
        assert_eq!(
            parse_envelope(br#"{"success":true,"result":{"a":1}}"#).unwrap(),
            serde_json::json!({"a": 1})
        );
        let refused = parse_envelope(
            br#"{"success":false,"errors":[{"code":10000,"message":"Authentication\nerror"}]}"#,
        )
        .unwrap_err();
        assert!(refused.has_code(10000));
        assert_eq!(
            refused.to_string(),
            "the Cloudflare API refused: 10000 Authentication error"
        );
        assert!(matches!(
            parse_envelope(b"[]").unwrap_err(),
            ApiReplyError::Malformed(_)
        ));
        assert!(matches!(
            parse_envelope(&vec![b' '; MAX_API_REPLY_BYTES + 1]).unwrap_err(),
            ApiReplyError::TooLarge
        ));
        let long = format!(
            r#"{{"success":false,"errors":[{{"code":"x","message":"{}"}}]}}"#,
            "m".repeat(500)
        );
        let ApiReplyError::Refused(faults) = parse_envelope(long.as_bytes()).unwrap_err() else {
            panic!("refused");
        };
        assert_eq!(faults[0].code, None);
        assert_eq!(
            faults[0].message.chars().count(),
            MAX_API_ERROR_MESSAGE_CHARS
        );
    }

    #[test]
    fn accounts_buckets_groups_and_tokens() {
        let accounts = parse_accounts(
            format!(
                r#"{{"success":true,"result":[{{"id":"{}","name":"Robin"}}]}}"#,
                ID.to_uppercase()
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(accounts[0].id, ID);
        assert_eq!(accounts[0].name, "Robin");
        assert!(parse_accounts(br#"{"success":true,"result":[{"id":"nope"}]}"#).is_err());

        assert_eq!(
            parse_bucket_created(br#"{"success":true,"result":{"name":"books"}}"#).unwrap(),
            BucketCreated::Created
        );
        assert_eq!(
            parse_bucket_created(
                br#"{"success":false,"errors":[{"code":10004,"message":"owned"}]}"#
            )
            .unwrap(),
            BucketCreated::AlreadyOwned
        );
        assert!(parse_bucket_created(br#"{"success":false,"errors":[{"code":10002}]}"#).is_err());

        let groups = format!(
            r#"{{"success":true,"result":[{{"id":"{ID}","name":"Other"}},{{"id":"{ID}","name":"{BUCKET_ITEM_WRITE}"}}]}}"#
        );
        assert_eq!(
            find_permission_group(groups.as_bytes(), BUCKET_ITEM_WRITE).unwrap(),
            Some(ID.to_string())
        );
        assert_eq!(
            find_permission_group(br#"{"success":true,"result":[]}"#, BUCKET_ITEM_WRITE).unwrap(),
            None
        );

        let minted = parse_minted_token(
            format!(r#"{{"success":true,"result":{{"id":"{ID}","value":"abc"}}}}"#).as_bytes(),
        )
        .unwrap();
        assert_eq!(minted.id, ID);
        assert_eq!(
            minted.secret_access_key(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(!format!("{minted:?}").contains("abc"));
        assert!(parse_minted_token(
            format!(r#"{{"success":true,"result":{{"id":"{ID}","value":"a b"}}}}"#).as_bytes()
        )
        .is_err());
        assert!(parse_done(br#"{"success":true,"result":null}"#).is_ok());
    }

    #[test]
    fn bucket_names_and_requests() {
        assert!(check_bucket_name("books-2").is_ok());
        assert!(check_bucket_name("ab").is_err());
        assert!(check_bucket_name("Books").is_err());
        assert!(check_bucket_name("-books").is_err());
        let resource = bucket_resource(ID, "eu", "books");
        assert_eq!(
            resource,
            format!("com.cloudflare.edge.r2.bucket.{ID}_eu_books")
        );
        let body = token_request("Bookclerk books", ID, &resource);
        assert_eq!(body["policies"][0]["resources"][&resource], "*");
        assert_eq!(body["policies"][0]["permission_groups"][0]["id"], ID);
    }
}
