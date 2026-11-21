//! `[[webhooks]]`: inbound HTTP endpoints the host serves for a plugin.
//!
//! The host serves each declared webhook at a secret per-instance URL under
//! the library's public origin, outside operator auth. Before anything is
//! stored it enforces the row's body cap, rate limit and signature rule; a
//! verified request is stored and answered at once, and the plugin reads
//! stored deliveries through its `WEBHOOKS` binding (see
//! `docs/adr/storage-destinations.md`, decision 6).
//!
//! Every limit has a hard ceiling here, and the signature rule is required:
//! a plugin that takes unsigned requests says `scheme = "none"`.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The binding name a plugin with `[[webhooks]]` receives; fixed, so no
/// other binding may take it.
pub const WEBHOOKS_BINDING: &str = "WEBHOOKS";

/// Most `[[webhooks]]` rows one plugin may declare.
pub const MAX_WEBHOOKS: usize = 8;

/// Longest webhook name in bytes.
pub const MAX_WEBHOOK_NAME_LEN: usize = bookclerk_plugin_abi::MAX_WEBHOOK_NAME_BYTES as usize;

/// Default body cap in bytes (64 KiB).
pub const DEFAULT_WEBHOOK_MAX_BODY_BYTES: u32 = 64 * 1024;

/// Largest body cap a row may declare, in bytes (1 MiB).
pub const MAX_WEBHOOK_BODY_BYTES: u32 = bookclerk_plugin_abi::MAX_WEBHOOK_BODY_BYTES;

/// Default request rate per minute for one instance's webhook.
pub const DEFAULT_WEBHOOK_RATE_PER_MINUTE: u32 = 60;

/// Highest request rate per minute a row may declare.
pub const MAX_WEBHOOK_RATE_PER_MINUTE: u32 = 600;

/// Default time a stored delivery waits for the plugin, in seconds (1 day).
pub const DEFAULT_WEBHOOK_RETENTION_SECS: u32 = 86_400;

/// Longest time a stored delivery may wait, in seconds (7 days).
pub const MAX_WEBHOOK_RETENTION_SECS: u32 = 7 * 86_400;

/// Most request headers a row may keep with each delivery.
pub const MAX_WEBHOOK_HEADERS: usize = bookclerk_plugin_abi::MAX_WEBHOOK_HEADERS as usize;

/// Longest header name a row may name, in bytes.
pub const MAX_WEBHOOK_HEADER_NAME_LEN: usize =
    bookclerk_plugin_abi::webhooks::MAX_WEBHOOK_HEADER_NAME_BYTES;

/// Longest signature prefix, in bytes.
pub const MAX_WEBHOOK_SIGNATURE_PREFIX_LEN: usize = 16;

/// Headers a row may never keep: they carry credentials or describe the
/// hop rather than the request.
const FORBIDDEN_HEADERS: &[&str] = &[
    "authorization",
    "connection",
    "cookie",
    "forwarded",
    "host",
    "keep-alive",
    "proxy-authorization",
    "set-cookie",
    "transfer-encoding",
    "upgrade",
    "x-forwarded-for",
    "x-real-ip",
];

/// One `[[webhooks]]` row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WebhookManifest {
    /// URL path segment and the name the binding uses
    /// (`[a-z0-9][a-z0-9-]*`, at most [`MAX_WEBHOOK_NAME_LEN`] bytes),
    /// unique within the plugin.
    pub name: String,
    /// Largest body the host accepts, in bytes (default
    /// [`DEFAULT_WEBHOOK_MAX_BODY_BYTES`], at most
    /// [`MAX_WEBHOOK_BODY_BYTES`]); a larger one is refused with 413.
    #[serde(
        default = "default_max_body_bytes",
        skip_serializing_if = "is_default_max_body_bytes"
    )]
    pub max_body_bytes: u32,
    /// Requests per minute the host accepts for one instance (default
    /// [`DEFAULT_WEBHOOK_RATE_PER_MINUTE`], at most
    /// [`MAX_WEBHOOK_RATE_PER_MINUTE`]); more are refused with 429.
    #[serde(
        default = "default_rate_per_minute",
        skip_serializing_if = "is_default_rate_per_minute"
    )]
    pub rate_per_minute: u32,
    /// Seconds a stored delivery waits for the plugin before it is dropped
    /// and counted as missed (default [`DEFAULT_WEBHOOK_RETENTION_SECS`], at
    /// most [`MAX_WEBHOOK_RETENTION_SECS`]).
    #[serde(
        default = "default_retention_secs",
        skip_serializing_if = "is_default_retention_secs"
    )]
    pub retention_secs: u32,
    /// Lowercase request header names kept with each delivery, besides
    /// `content-type`, which is always kept. Credential and hop headers are
    /// refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<String>,
    /// The check the host runs before storing a request.
    pub signature: WebhookSignature,
}

/// How the host checks that a request came from the sender.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "scheme", rename_all = "kebab-case", deny_unknown_fields)]
pub enum WebhookSignature {
    /// No check: every request within the caps is stored. The plugin must
    /// treat deliveries as untrusted hints.
    None,
    /// HMAC-SHA256 of the raw body under a sealed plugin setting, carried in
    /// one header as `<prefix><digest>`.
    HmacSha256(HmacSignature),
}

/// An HMAC-SHA256 signature rule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HmacSignature {
    /// Lowercase header that carries the signature.
    pub header: String,
    /// Text before the digest in the header value (for example `v1=`);
    /// empty when the value is the digest alone.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    /// How the digest is written.
    #[serde(default)]
    pub encoding: SignatureEncoding,
    /// Key of a `type = "secret"` `[[settings]]` entry holding the shared
    /// key. The host reads it; the plugin never needs it.
    pub secret_setting: String,
}

/// How a signature digest is written in its header.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SignatureEncoding {
    /// Lowercase or uppercase hexadecimal.
    #[default]
    Hex,
    /// Standard base64 with padding.
    Base64,
}

/// Serde default for [`WebhookManifest::max_body_bytes`].
fn default_max_body_bytes() -> u32 {
    DEFAULT_WEBHOOK_MAX_BODY_BYTES
}

/// Serde skip check for [`WebhookManifest::max_body_bytes`].
#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
fn is_default_max_body_bytes(value: &u32) -> bool {
    *value == DEFAULT_WEBHOOK_MAX_BODY_BYTES
}

/// Serde default for [`WebhookManifest::rate_per_minute`].
fn default_rate_per_minute() -> u32 {
    DEFAULT_WEBHOOK_RATE_PER_MINUTE
}

/// Serde skip check for [`WebhookManifest::rate_per_minute`].
#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
fn is_default_rate_per_minute(value: &u32) -> bool {
    *value == DEFAULT_WEBHOOK_RATE_PER_MINUTE
}

/// Serde default for [`WebhookManifest::retention_secs`].
fn default_retention_secs() -> u32 {
    DEFAULT_WEBHOOK_RETENTION_SECS
}

/// Serde skip check for [`WebhookManifest::retention_secs`].
#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
fn is_default_retention_secs(value: &u32) -> bool {
    *value == DEFAULT_WEBHOOK_RETENTION_SECS
}

pub use bookclerk_plugin_abi::webhooks::{
    is_valid_webhook_header_name as is_valid_header_name, is_valid_webhook_name,
};

/// Checks every `[[webhooks]]` row.
///
/// # Arguments
///
/// * `rows` - the declared webhooks.
/// * `is_secret_setting` - whether a `[[settings]]` key names a
///   `type = "secret"` entry.
///
/// # Errors
///
/// Names the first row with a bad name, an out-of-range limit, a header the
/// host cannot keep, or a signature rule that does not resolve.
pub fn validate_webhooks(
    rows: &[WebhookManifest],
    is_secret_setting: impl Fn(&str) -> bool,
) -> Result<()> {
    if rows.len() > MAX_WEBHOOKS {
        return Err(Error::message(format!(
            "plugin.toml: at most {MAX_WEBHOOKS} [[webhooks]] rows"
        )));
    }
    let mut names = std::collections::BTreeSet::new();
    for (i, row) in rows.iter().enumerate() {
        if !is_valid_webhook_name(&row.name) {
            return Err(Error::message(format!(
                "plugin.toml: webhooks[{i}].name must be `[a-z0-9][a-z0-9-]*` and at most \
                 {MAX_WEBHOOK_NAME_LEN} chars"
            )));
        }
        if !names.insert(row.name.as_str()) {
            return Err(Error::message(format!(
                "plugin.toml: webhooks[{i}].name repeats an earlier row"
            )));
        }
        if row.max_body_bytes == 0 || row.max_body_bytes > MAX_WEBHOOK_BODY_BYTES {
            return Err(Error::message(format!(
                "plugin.toml: webhooks[{i}].max_body_bytes must be 1..={MAX_WEBHOOK_BODY_BYTES}"
            )));
        }
        if row.rate_per_minute == 0 || row.rate_per_minute > MAX_WEBHOOK_RATE_PER_MINUTE {
            return Err(Error::message(format!(
                "plugin.toml: webhooks[{i}].rate_per_minute must be \
                 1..={MAX_WEBHOOK_RATE_PER_MINUTE}"
            )));
        }
        if row.retention_secs < 60 || row.retention_secs > MAX_WEBHOOK_RETENTION_SECS {
            return Err(Error::message(format!(
                "plugin.toml: webhooks[{i}].retention_secs must be \
                 60..={MAX_WEBHOOK_RETENTION_SECS}"
            )));
        }
        validate_headers(i, &row.headers)?;
        match &row.signature {
            WebhookSignature::None => {}
            WebhookSignature::HmacSha256(rule) => {
                if !is_valid_header_name(&rule.header)
                    || FORBIDDEN_HEADERS.contains(&rule.header.as_str())
                {
                    return Err(Error::message(format!(
                        "plugin.toml: webhooks[{i}].signature.header must be a lowercase \
                         header name the host may read"
                    )));
                }
                if rule.prefix.len() > MAX_WEBHOOK_SIGNATURE_PREFIX_LEN
                    || !rule.prefix.bytes().all(|b| b.is_ascii_graphic())
                {
                    return Err(Error::message(format!(
                        "plugin.toml: webhooks[{i}].signature.prefix must be at most \
                         {MAX_WEBHOOK_SIGNATURE_PREFIX_LEN} visible ASCII characters"
                    )));
                }
                if !is_secret_setting(&rule.secret_setting) {
                    return Err(Error::message(format!(
                        "plugin.toml: webhooks[{i}].signature.secret_setting must name a \
                         `type = \"secret\"` entry in [[settings]]"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Checks one row's kept headers.
///
/// # Errors
///
/// Names the row when a header is malformed, repeated, forbidden or one too
/// many.
fn validate_headers(row: usize, headers: &[String]) -> Result<()> {
    if headers.len() > MAX_WEBHOOK_HEADERS {
        return Err(Error::message(format!(
            "plugin.toml: webhooks[{row}].headers lists more than {MAX_WEBHOOK_HEADERS} names"
        )));
    }
    let mut seen = std::collections::BTreeSet::new();
    for header in headers {
        if !is_valid_header_name(header) {
            return Err(Error::message(format!(
                "plugin.toml: webhooks[{row}].headers entries must be lowercase header names"
            )));
        }
        if FORBIDDEN_HEADERS.contains(&header.as_str()) {
            return Err(Error::message(format!(
                "plugin.toml: webhooks[{row}].headers may not keep `{header}`"
            )));
        }
        if !seen.insert(header.as_str()) {
            return Err(Error::message(format!(
                "plugin.toml: webhooks[{row}].headers repeats `{header}`"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> std::result::Result<Vec<WebhookManifest>, String> {
        #[derive(Deserialize)]
        struct Doc {
            webhooks: Vec<WebhookManifest>,
        }
        toml::from_str::<Doc>(text)
            .map(|d| d.webhooks)
            .map_err(|e| e.to_string())
    }

    fn secret(key: &str) -> bool {
        key == "webhook_secret"
    }

    const B2: &str = r#"
[[webhooks]]
name = "b2-events"
headers = ["x-bz-event-notification-signature"]
[webhooks.signature]
scheme = "hmac-sha256"
header = "x-bz-event-notification-signature"
prefix = "v1="
secret_setting = "webhook_secret"
"#;

    #[test]
    fn a_signed_row_parses_with_defaults() {
        let rows = parse(B2).expect("parse");
        validate_webhooks(&rows, secret).expect("valid");
        let row = &rows[0];
        assert_eq!(row.max_body_bytes, DEFAULT_WEBHOOK_MAX_BODY_BYTES);
        assert_eq!(row.rate_per_minute, DEFAULT_WEBHOOK_RATE_PER_MINUTE);
        assert_eq!(row.retention_secs, DEFAULT_WEBHOOK_RETENTION_SECS);
        let WebhookSignature::HmacSha256(rule) = &row.signature else {
            panic!("hmac rule");
        };
        assert_eq!(rule.encoding, SignatureEncoding::Hex);
        assert_eq!(rule.prefix, "v1=");
    }

    #[test]
    fn the_signature_rule_is_required() {
        let err = parse("[[webhooks]]\nname = \"a\"\n").expect_err("missing signature");
        assert!(err.contains("signature"), "{err}");
        let rows = parse("[[webhooks]]\nname = \"a\"\nsignature = { scheme = \"none\" }\n")
            .expect("none parses");
        validate_webhooks(&rows, secret).expect("none is valid");
    }

    #[test]
    fn limits_and_names_are_checked() {
        let bad = |edit: &str| {
            let text = B2.replace("name = \"b2-events\"", edit);
            let rows = parse(&text).expect("parse");
            validate_webhooks(&rows, secret).expect_err(edit).to_string()
        };
        assert!(bad("name = \"B2\"").contains("name"));
        assert!(bad("name = \"a\"\nmax_body_bytes = 0").contains("max_body_bytes"));
        assert!(bad("name = \"a\"\nmax_body_bytes = 2000000").contains("max_body_bytes"));
        assert!(bad("name = \"a\"\nrate_per_minute = 601").contains("rate_per_minute"));
        assert!(bad("name = \"a\"\nretention_secs = 10").contains("retention_secs"));
        assert!(bad("name = \"a\"\nretention_secs = 700000").contains("retention_secs"));
    }

    #[test]
    fn headers_are_checked() {
        let with_headers = |list: &str| {
            let text = B2.replace(
                "headers = [\"x-bz-event-notification-signature\"]",
                &format!("headers = {list}"),
            );
            let rows = parse(&text).expect("parse");
            validate_webhooks(&rows, secret)
        };
        assert!(with_headers("[\"Authorization\"]").is_err());
        assert!(with_headers("[\"authorization\"]").is_err());
        assert!(with_headers("[\"x-a\", \"x-a\"]").is_err());
        assert!(with_headers("[\"x a\"]").is_err());
        let many: Vec<String> = (0..17).map(|i| format!("\"x-{i}\"")).collect();
        assert!(with_headers(&format!("[{}]", many.join(","))).is_err());
        assert!(with_headers("[\"x-event\", \"user-agent\"]").is_ok());
    }

    #[test]
    fn the_signature_must_resolve() {
        let rows = parse(B2).expect("parse");
        assert!(validate_webhooks(&rows, |_| false).is_err());
        let text = B2.replace(
            "header = \"x-bz-event-notification-signature\"",
            "header = \"cookie\"",
        );
        let rows = parse(&text).expect("parse");
        assert!(validate_webhooks(&rows, secret).is_err());
        let text = B2.replace("prefix = \"v1=\"", "prefix = \"v1 =\"");
        let rows = parse(&text).expect("parse");
        assert!(validate_webhooks(&rows, secret).is_err());
        let text = B2.replace("scheme = \"hmac-sha256\"", "scheme = \"md5\"");
        assert!(parse(&text).is_err());
    }

    #[test]
    fn names_are_unique_and_rows_are_capped() {
        let two = format!("{B2}{B2}");
        let rows = parse(&two).expect("parse");
        assert!(validate_webhooks(&rows, secret).is_err());
        let many: String = (0..9)
            .map(|i| B2.replace("b2-events", &format!("w{i}")))
            .collect();
        let rows = parse(&many).expect("parse");
        assert!(validate_webhooks(&rows, secret).is_err());
    }
}
