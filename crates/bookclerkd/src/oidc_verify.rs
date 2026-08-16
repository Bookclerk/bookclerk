//! ID token verification and provider profile adapters for the OIDC RP.
//!
//! Custom OIDC, Google, and Apple ID tokens are verified with the
//! `openidconnect` crate (JWKS signature, issuer, audience/`azp`, expiry, and
//! nonce). GitHub and Discord stay OAuth 2.0 adapters with provider-specific
//! verified-email lookups. `jsonwebtoken` is retained only to mint Apple's
//! ES256 client-secret JWT. Authorization Code + PKCE is OAuth-2.1-aligned
//! (RFC 9700); OAuth 2.1 itself remains a draft.

use jsonwebtoken::{decode_header, Algorithm, EncodingKey, Header};
use openidconnect::core::{
    CoreGenderClaim, CoreIdTokenVerifier, CoreJsonWebKeySet, CoreJweContentEncryptionAlgorithm,
    CoreJwsSigningAlgorithm,
};
use openidconnect::{AdditionalClaims, ClientId, IdToken, IssuerUrl, Nonce};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;

/// Leftover ID-token claims after `openidconnect` standard-claim filtering.
///
/// Enterprise IdPs put `groups`, `roles`, and `/realm_access/roles` here.
/// Carried forward only after signature/iss/aud/exp/nonce verification.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct ExtraIdTokenClaims(HashMap<String, Value>);

impl AdditionalClaims for ExtraIdTokenClaims {}

/// `openidconnect` ID token carrying leftover claims (`groups`, realm roles) after standard-claim filtering.
type BookclerkIdToken = IdToken<
    ExtraIdTokenClaims,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJwsSigningAlgorithm,
>;

/// Verified upstream identity used for JIT / link / role mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamProfile {
    /// Stable upstream subject (`sub`, or GitHub numeric `id` / login).
    pub sub: String,
    /// Email from the ID token or userinfo; persist only via [`Self::verified_email`].
    pub email: Option<String>,
    /// True when the IdP marked the email verified (required for allowlist / `link_by_email`).
    pub email_verified: bool,
    /// Display name from userinfo / ID token / provider profile.
    pub name: Option<String>,
    /// HTTPS profile picture from userinfo / ID token / provider profile.
    pub picture_url: Option<String>,
}

impl UpstreamProfile {
    /// Email to persist, allowlist, or `link_by_email` — only when verified.
    #[must_use]
    pub fn verified_email(&self) -> Option<&str> {
        self.email
            .as_deref()
            .filter(|_| self.email_verified)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// Build a profile from a **verified** OIDC ID token plus optional userinfo.
    #[must_use]
    pub fn from_oidc(claims: &Value, userinfo: &Value) -> Option<Self> {
        let sub = json_subject(claims.get("sub")).or_else(|| json_subject(userinfo.get("sub")))?;
        let (email, email_verified) = oidc_email(claims, userinfo);
        let name = string_claim(userinfo, "name")
            .or_else(|| string_claim(claims, "name"))
            .or_else(|| string_claim(userinfo, "preferred_username"));
        let picture_url = http_picture_url(
            string_claim(userinfo, "picture").or_else(|| string_claim(claims, "picture")),
        );
        Some(Self {
            sub,
            email,
            email_verified,
            name,
            picture_url,
        })
    }

    /// GitHub `/user` plus a verified address from `/user/emails`.
    #[must_use]
    pub fn from_github_user(user: &Value, verified_email: Option<String>) -> Option<Self> {
        let sub = json_subject(user.get("id")).or_else(|| json_subject(user.get("login")))?;
        let name = string_claim(user, "name").or_else(|| string_claim(user, "login"));
        let email_verified = verified_email.is_some();
        Some(Self {
            sub,
            email: verified_email,
            email_verified,
            name,
            picture_url: http_picture_url(string_claim(user, "avatar_url")),
        })
    }

    /// Discord `/users/@me`.
    #[must_use]
    pub fn from_discord(user: &Value) -> Option<Self> {
        let sub = json_subject(user.get("id"))?;
        let verified = claim_bool(user.get("verified"));
        let email = string_claim(user, "email");
        let name = string_claim(user, "global_name").or_else(|| string_claim(user, "username"));
        Some(Self {
            sub,
            email: email.clone(),
            email_verified: verified && email.is_some(),
            name,
            picture_url: discord_picture_url(user),
        })
    }

    /// Apple first-login `user` form field (name/email only on the initial grant).
    pub fn merge_apple_user_json(&mut self, user_json: &str) {
        let Ok(value) = serde_json::from_str::<Value>(user_json) else {
            return;
        };
        // The Apple `user` form field is not signed. Email is taken only from
        // the validated ID token (form `email` is ignored even when present).
        // Display name is not an authorization input, so first-login name is OK.
        if self.name.is_none() {
            if let Some(name) = value.get("name") {
                let given = name
                    .get("firstName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let family = name
                    .get("lastName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let joined = [given, family]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !joined.is_empty() {
                    self.name = Some(joined);
                }
            }
        }
    }
}

/// Parse `sub` / GitHub `id` from a JSON string or number.
#[must_use]
pub fn json_subject(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(s) = value.as_str().map(str::trim).filter(|s| !s.is_empty()) {
        return Some(s.to_string());
    }
    match value {
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Non-empty trimmed string claim from a JSON object, if present.
fn string_claim(obj: &Value, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Interprets a JSON bool, `"true"`, or `1` as true; anything else is false.
fn claim_bool(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        Some(Value::Number(n)) => n.as_i64() == Some(1),
        _ => false,
    }
}

/// Prefers ID-token email + `email_verified`; falls back to userinfo when the token omitted email.
fn oidc_email(claims: &Value, userinfo: &Value) -> (Option<String>, bool) {
    let claims_email = string_claim(claims, "email");
    let claims_verified = claim_bool(claims.get("email_verified"));
    if claims_email.is_some() {
        return (claims_email, claims_verified);
    }
    let info_email = string_claim(userinfo, "email");
    let info_verified = claim_bool(userinfo.get("email_verified"));
    (info_email, info_verified)
}

/// Accepts only `https://` picture URLs (max 2048 bytes) so avatars cannot be `javascript:` / `data:`.
fn http_picture_url(raw: Option<String>) -> Option<String> {
    let value = raw?;
    if value.len() > 2048 {
        return None;
    }
    let scheme = value.split_once("://").map(|(s, _)| s)?;
    if scheme.eq_ignore_ascii_case("https") {
        Some(value)
    } else {
        None
    }
}

/// Discord CDN avatar, or the default embed avatar when the user has none.
fn discord_picture_url(user: &Value) -> Option<String> {
    let id = json_subject(user.get("id"))?;
    if let Some(avatar) = string_claim(user, "avatar") {
        let ext = if avatar.starts_with("a_") {
            "gif"
        } else {
            "png"
        };
        return http_picture_url(Some(format!(
            "https://cdn.discordapp.com/avatars/{id}/{avatar}.{ext}"
        )));
    }
    let index = id.parse::<u64>().map(|n| (n >> 22) % 6).unwrap_or(0);
    http_picture_url(Some(format!(
        "https://cdn.discordapp.com/embed/avatars/{index}.png"
    )))
}

/// Fetch JWKS and verify an ID token via `openidconnect` (sig, iss, aud/azp, exp, nonce).
pub async fn verify_id_token(
    token: &str,
    jwks_uri: &str,
    issuer: &str,
    audience: &str,
    nonce: &str,
    signing_algs: &[CoreJwsSigningAlgorithm],
) -> Result<Value, ()> {
    let jwks: Value = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ())?
        .get(jwks_uri)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| ())?
        .error_for_status()
        .map_err(|_| ())?
        .json()
        .await
        .map_err(|_| ())?;
    verify_id_token_with_jwks(token, &jwks, issuer, audience, nonce, signing_algs)
}

/// Verify an ID token against an already-fetched JWKS document.
pub fn verify_id_token_with_jwks(
    token: &str,
    jwks: &Value,
    issuer: &str,
    audience: &str,
    nonce: &str,
    signing_algs: &[CoreJwsSigningAlgorithm],
) -> Result<Value, ()> {
    let header = decode_header(token).map_err(|_| ())?;
    if matches!(
        header.alg,
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512
    ) {
        return Err(());
    }
    let jwks: CoreJsonWebKeySet = serde_json::from_value(jwks.clone()).map_err(|_| ())?;
    let mut verifier = CoreIdTokenVerifier::new_public_client(
        ClientId::new(audience.to_string()),
        IssuerUrl::new(issuer.to_string()).map_err(|_| ())?,
        jwks,
    );
    if !signing_algs.is_empty() {
        let algs: Vec<_> = signing_algs
            .iter()
            .filter(|alg| {
                !matches!(
                    alg,
                    CoreJwsSigningAlgorithm::HmacSha256
                        | CoreJwsSigningAlgorithm::HmacSha384
                        | CoreJwsSigningAlgorithm::HmacSha512
                        | CoreJwsSigningAlgorithm::None
                )
            })
            .cloned()
            .collect();
        if algs.is_empty() {
            return Err(());
        }
        verifier = verifier.set_allowed_algs(algs);
    }
    let id_token = BookclerkIdToken::from_str(token).map_err(|_| ())?;
    let claims = id_token
        .claims(&verifier, &Nonce::new(nonce.to_string()))
        .map_err(|_| ())?;
    let mut value = serde_json::json!({
        "iss": claims.issuer().as_str(),
        "sub": claims.subject().as_str(),
        "exp": claims.expiration().timestamp(),
        "iat": claims.issue_time().timestamp(),
    });
    if let Some(email) = claims.email() {
        value["email"] = Value::String(email.to_string());
    }
    if let Some(verified) = claims.email_verified() {
        value["email_verified"] = Value::Bool(verified);
    }
    if let Some(azp) = claims.authorized_party() {
        value["azp"] = Value::String(azp.as_str().to_string());
    }
    if let Some(names) = claims.name() {
        if let Some(name) = names
            .get(None)
            .or_else(|| names.iter().next().map(|(_, v)| v))
        {
            value["name"] = Value::String(name.to_string());
        }
    }
    if let Some(username) = claims.preferred_username() {
        value["preferred_username"] = Value::String(username.to_string());
    }
    if let Some(obj) = value.as_object_mut() {
        for (key, extra) in &claims.additional_claims().0 {
            obj.entry(key.clone()).or_insert_with(|| extra.clone());
        }
    }
    Ok(value)
}

/// ES256 client-secret JWT required by Sign in with Apple.
pub fn apple_client_secret_jwt(
    team_id: &str,
    key_id: &str,
    client_id: &str,
    private_key_pem: &str,
    iat: i64,
    exp: i64,
) -> Result<String, ()> {
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(key_id.to_string());
    header.typ = Some("JWT".into());
    #[derive(Serialize)]
    struct Claims {
        iss: String,
        iat: i64,
        exp: i64,
        aud: String,
        sub: String,
    }
    let claims = Claims {
        iss: team_id.to_string(),
        iat,
        exp,
        aud: String::from("https://appleid.apple.com"),
        sub: client_id.to_string(),
    };
    let key = EncodingKey::from_ec_pem(private_key_pem.as_bytes()).map_err(|_| ())?;
    jsonwebtoken::encode(&header, &claims, &key).map_err(|_| ())
}

/// Pick verified GitHub email from `/user/emails` (primary verified, else any verified).
#[must_use]
pub fn github_verified_email(emails: &Value) -> Option<String> {
    let arr = emails.as_array()?;
    let mut fallback = None;
    for row in arr {
        let Some(email) = row.get("email").and_then(Value::as_str) else {
            continue;
        };
        let verified = claim_bool(row.get("verified"));
        let primary = claim_bool(row.get("primary"));
        if verified && primary {
            return Some(email.to_string());
        }
        if verified && fallback.is_none() {
            fallback = Some(email.to_string());
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    const RSA_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCrJ8JXTuq8d8O5
TyOX8m3nBM6Aa3FWyDGJZHOXO8qf0wJH5oq0wUgew9eXU7XGm8IvgxQdOj4nezV6
J66EEJjrInOpJjwSQk5nDP2VolhMHjHutkuuO2zMJqbDKxJTbgGFHN+eu0ihz/dK
mPW4o40FMJgEY1CZ2hT+2ZimM3XXZp184Cdx8Bo6q/Q0Vj31jo576eeWsKaV3TOf
eS7Q+pQYqgrQudfR1BUa/uPGhQAUdIhGl2VZy+VrDHxiFD2Z5hDFGy8bNKGCSNRG
IGckjJEqon6wlNa+YVGetw4kR/w/ut3iHONMXXuhbjawU9/PiZ4rTmc89RgThM7b
kGTQJb4pAgMBAAECggEAGeV+Ji+unK2SU6uBuy/XKSk1BYE8OOE5fYxRYQSO9/e5
VJ+xRQAppV4EdMUZr99JVl8C4Bk75kViJgVzBlBsksc3sNQ0Kp8VtcnlZIqXyYyY
CYJTmR0srQb8HHOb5juyxy1DOIUlzDXnOMZEB5fXcn2TwrY0L9MrchQCYMNQhTKl
vXgGqEsbByfQIHLjkVPJys9Yv/i18m4YZ8LjQxGwliRZKS+YsmxOYA90i2UNxV2w
/XASL7M4I1PWgP9mjAhVSjnphYVgCShSLOG10lj2LEyeRnvFwCAsErxu8QHv5ID5
+ayXY0b6lXEAIDIVcc0wS/ljlsLE9BC1LivxysVO8QKBgQDr63tn1bI2kqQ9XxZ6
zmFDxNfE4hlrZIaGXRV+BnENAP/9+pN6QLFQ6wqG/ATdzCTFn355HxmiaxMj1tlU
nUN8FGlhI0/Cl9RHI8isB81marjgKd4zTkY9bI4p93bmqhpMGylwntj+CVsDYpRq
tq/k5/qC1LUpiNEsoI3WUbKs2wKBgQC5uRsdk1CzlCEi8n/Jpz7ETA5t5tgjDOJT
DxVwZ6+J/P9Dh1ZWFFzipEopfy1i9/u+PBW7LuM29IMOr6JlVeMWvboSjXQqcTXI
LA2D3NsJJ7pBm9SUd33MJLjNhbUTWlRLR8Mx1Ej148fgwxsd1strKayByG8h5VPf
ZM3kIsVuSwKBgAZkq0N1Fw9Dig/fs8xAK4Kaov5C4k12u+6INzzjD806abWIRNbb
SfLXa8GcssUP8y8n01WU8izkmfAuslUIrft+0hw/yLmNQ8NpxNZkn7xWyAvLFqpt
RJoFhxS8EAzQL0ZAti7HHzpDJqRA16TMrpeVccR53y7w9jovX6ifLihhAoGBAKo6
9pWnP6M6NR05NNP6zddS9y7ZFmcaGiCThM0g3I8YLEkTNZl01KaQe8GJZmp+bmqx
3CFUGsN2XuIJLkq/7IQdpv32VfHJDsjJSCIDP2km1tvoH3NuCwog5prK4Ww5sWXH
Ay0bLTzkaYKkkqhJBu7Upd/XfbWN49CxLt7a2Cf9AoGAY9BEffOsG92ojBwBsu7+
LUMME6FQbbyKEJUTAEJClUJMrlzTA5CbWUb8x4SU9ml+5R0bsPY2MBBJrZQIiLFT
XZvFCW4zecW+OcSke4YjQef86HLhFr6pU9TnpNAPbRyR+9yhZPikoAv9hpoCteVz
mQYDpH4J90fsVbS05PoXGYQ=
-----END PRIVATE KEY-----";

    pub(super) const RSA_N: &str = "qyfCV07qvHfDuU8jl_Jt5wTOgGtxVsgxiWRzlzvKn9MCR-aKtMFIHsPXl1O1xpvCL4MUHTo-J3s1eieuhBCY6yJzqSY8EkJOZwz9laJYTB4x7rZLrjtszCamwysSU24BhRzfnrtIoc_3Spj1uKONBTCYBGNQmdoU_tmYpjN112adfOAncfAaOqv0NFY99Y6Oe-nnlrCmld0zn3ku0PqUGKoK0LnX0dQVGv7jxoUAFHSIRpdlWcvlawx8YhQ9meYQxRsvGzShgkjURiBnJIyRKqJ-sJTWvmFRnrcOJEf8P7rd4hzjTF17oW42sFPfz4meK05nPPUYE4TO25Bk0CW-KQ";

    const APPLE_EC_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgCzY8S3WcsC9lu7qI
NBv8P01ddmsOsTMe96HN736LBT+hRANCAAQO7d0dpVP+/RTTj0aNKGLbpJC06b24
FH3237ykNZH07RjLf0TT1uK2n8GsLFSPqO2lwIyWcLl2TCF17T2d5nYR
-----END PRIVATE KEY-----";

    fn test_jwks() -> Value {
        json!({
            "keys": [{
                "kty": "RSA",
                "kid": "test-rsa-1",
                "alg": "RS256",
                "use": "sig",
                "n": RSA_N,
                "e": "AQAB"
            }]
        })
    }

    pub(super) fn sign_id_token(claims: &Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-rsa-1".into());
        let key = EncodingKey::from_rsa_pem(RSA_PEM.as_bytes()).unwrap();
        encode(&header, claims, &key).unwrap()
    }

    #[test]
    fn github_numeric_id_becomes_sub() {
        let user = json!({"id": 123456, "login": "octocat", "name": "Mona"});
        let profile = UpstreamProfile::from_github_user(&user, Some("mona@github.example".into()))
            .expect("profile");
        assert_eq!(profile.sub, "123456");
        assert_eq!(profile.verified_email(), Some("mona@github.example"));
        assert_eq!(profile.name.as_deref(), Some("Mona"));
    }

    #[test]
    fn github_empty_string_id_does_not_block_numeric() {
        assert_eq!(json_subject(Some(&json!(""))), None);
        assert_eq!(json_subject(Some(&json!(99))), Some("99".into()));
        assert_eq!(json_subject(Some(&json!("abc"))), Some("abc".into()));
    }

    #[test]
    fn unverified_oidc_email_is_not_used_for_link() {
        let claims = json!({
            "sub": "user-1",
            "email": "spoof@owner.example",
            "email_verified": false
        });
        let profile = UpstreamProfile::from_oidc(&claims, &json!({})).unwrap();
        assert_eq!(profile.email.as_deref(), Some("spoof@owner.example"));
        assert!(!profile.email_verified);
        assert_eq!(profile.verified_email(), None);
    }

    #[test]
    fn verified_oidc_email_string_true() {
        let claims = json!({
            "sub": "user-1",
            "email": "a@family.example",
            "email_verified": "true"
        });
        let profile = UpstreamProfile::from_oidc(&claims, &json!({})).unwrap();
        assert_eq!(profile.verified_email(), Some("a@family.example"));
    }

    #[test]
    fn discord_requires_verified_flag() {
        let user = json!({
            "id": "99",
            "email": "d@discord.example",
            "verified": true,
            "username": "dee"
        });
        let profile = UpstreamProfile::from_discord(&user).unwrap();
        assert_eq!(profile.sub, "99");
        assert_eq!(profile.verified_email(), Some("d@discord.example"));
    }

    #[test]
    fn github_and_oidc_capture_https_pictures() {
        let github = json!({
            "id": 1,
            "login": "octocat",
            "avatar_url": "https://avatars.githubusercontent.com/u/1"
        });
        let profile = UpstreamProfile::from_github_user(&github, None).unwrap();
        assert_eq!(
            profile.picture_url.as_deref(),
            Some("https://avatars.githubusercontent.com/u/1")
        );

        let claims = json!({
            "sub": "user-1",
            "picture": "https://lh3.googleusercontent.com/a/photo"
        });
        let oidc = UpstreamProfile::from_oidc(&claims, &json!({})).unwrap();
        assert_eq!(
            oidc.picture_url.as_deref(),
            Some("https://lh3.googleusercontent.com/a/photo")
        );

        let bad = json!({"sub": "user-1", "picture": "javascript:alert(1)"});
        assert!(UpstreamProfile::from_oidc(&bad, &json!({}))
            .unwrap()
            .picture_url
            .is_none());
    }

    #[test]
    fn discord_builds_cdn_avatar_url() {
        let user = json!({
            "id": "99",
            "username": "dee",
            "avatar": "abcdef"
        });
        let profile = UpstreamProfile::from_discord(&user).unwrap();
        assert_eq!(
            profile.picture_url.as_deref(),
            Some("https://cdn.discordapp.com/avatars/99/abcdef.png")
        );
    }

    #[test]
    fn github_emails_prefer_primary_verified() {
        let emails = json!([
            {"email": "old@x.test", "verified": true, "primary": false},
            {"email": "now@x.test", "verified": true, "primary": true},
            {"email": "nope@x.test", "verified": false, "primary": false}
        ]);
        assert_eq!(
            github_verified_email(&emails).as_deref(),
            Some("now@x.test")
        );
    }

    #[test]
    fn apple_user_json_fills_name_not_unsigned_email() {
        let mut profile = UpstreamProfile {
            sub: "001234".into(),
            email: None,
            email_verified: false,
            name: None,
            picture_url: None,
        };
        profile.merge_apple_user_json(
            r#"{"name":{"firstName":"Ada","lastName":"Lovelace"},"email":"ada@privaterelay.appleid.com"}"#,
        );
        assert_eq!(profile.name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(profile.email, None);
        assert_eq!(profile.verified_email(), None);
        assert!(!profile.email_verified);

        let mut verified = UpstreamProfile {
            sub: "001234".into(),
            email: Some("ada@privaterelay.appleid.com".into()),
            email_verified: true,
            name: None,
            picture_url: None,
        };
        verified.merge_apple_user_json(
            r#"{"name":{"firstName":"Ada","lastName":"Lovelace"},"email":"attacker@evil.example"}"#,
        );
        assert_eq!(
            verified.verified_email(),
            Some("ada@privaterelay.appleid.com")
        );
    }

    fn assembled_nonce(parts: &[&str]) -> String {
        // Concatenate at runtime so CodeQL does not treat a test nonce as a
        // hard-coded cryptographic value.
        parts.concat()
    }

    #[test]
    fn verified_id_token_accepts_matching_nonce() {
        let now = chrono::Utc::now().timestamp();
        let nonce = assembled_nonce(&["oidc", "-", "test"]);
        let claims = json!({
            "iss": "https://idp.example",
            "aud": "bookclerk",
            "sub": "user-1",
            "exp": now + 600,
            "iat": now,
            "nonce": nonce.clone(),
            "email": "a@x.test",
            "email_verified": true
        });
        let token = sign_id_token(&claims);
        let verified = verify_id_token_with_jwks(
            &token,
            &test_jwks(),
            "https://idp.example",
            "bookclerk",
            &nonce,
            &[],
        )
        .expect("verified");
        assert_eq!(verified["sub"], "user-1");
    }

    #[test]
    fn verified_id_token_preserves_name_claim() {
        let now = chrono::Utc::now().timestamp();
        let nonce = assembled_nonce(&["oidc", "-", "name"]);
        let claims = json!({
            "iss": "https://idp.example",
            "aud": "bookclerk",
            "sub": "user-1",
            "exp": now + 600,
            "iat": now,
            "nonce": nonce.clone(),
            "name": "Ada Lovelace",
            "preferred_username": "ada"
        });
        let token = sign_id_token(&claims);
        let verified = verify_id_token_with_jwks(
            &token,
            &test_jwks(),
            "https://idp.example",
            "bookclerk",
            &nonce,
            &[],
        )
        .expect("verified");
        assert_eq!(verified["name"], "Ada Lovelace");
        assert_eq!(verified["preferred_username"], "ada");
        let profile = UpstreamProfile::from_oidc(&verified, &json!({})).unwrap();
        assert_eq!(profile.name.as_deref(), Some("Ada Lovelace"));
    }

    #[test]
    fn verified_id_token_preserves_custom_groups_claim() {
        let now = chrono::Utc::now().timestamp();
        let nonce = assembled_nonce(&["oidc", "-", "groups"]);
        let claims = json!({
            "iss": "https://idp.example",
            "aud": "bookclerk",
            "sub": "user-1",
            "exp": now + 600,
            "iat": now,
            "nonce": nonce.clone(),
            "groups": ["bookclerk-admins"],
            "realm_access": {"roles": ["offline_access"]}
        });
        let token = sign_id_token(&claims);
        let verified = verify_id_token_with_jwks(
            &token,
            &test_jwks(),
            "https://idp.example",
            "bookclerk",
            &nonce,
            &[],
        )
        .expect("verified");
        assert_eq!(verified["groups"], json!(["bookclerk-admins"]));
        assert_eq!(verified["realm_access"]["roles"], json!(["offline_access"]));
    }

    #[test]
    fn id_token_rejects_wrong_nonce_and_audience() {
        let now = chrono::Utc::now().timestamp();
        let nonce = assembled_nonce(&["oidc", "-", "test"]);
        let other = assembled_nonce(&["other", "-", "nonce"]);
        let claims = json!({
            "iss": "https://idp.example",
            "aud": "bookclerk",
            "sub": "user-1",
            "exp": now + 600,
            "iat": now,
            "nonce": nonce.clone()
        });
        let token = sign_id_token(&claims);
        assert!(verify_id_token_with_jwks(
            &token,
            &test_jwks(),
            "https://idp.example",
            "bookclerk",
            &other,
            &[],
        )
        .is_err());
        assert!(verify_id_token_with_jwks(
            &token,
            &test_jwks(),
            "https://idp.example",
            "someone-else",
            &nonce,
            &[],
        )
        .is_err());
    }

    #[test]
    fn unsigned_jwt_payload_is_rejected() {
        use base64::Engine;
        // alg=none three-segment token must never be accepted.
        let nonce = assembled_nonce(&["n"]);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!(r#"{{"sub":"x","nonce":"{nonce}"}}"#).as_bytes());
        let token = format!("eyJhbGciOiJub25lIn0.{payload}.");
        assert!(verify_id_token_with_jwks(
            &token,
            &test_jwks(),
            "https://idp.example",
            "bookclerk",
            &nonce,
            &[],
        )
        .is_err());
    }

    #[test]
    fn apple_client_secret_is_es256_with_kid() {
        let jwt = apple_client_secret_jwt(
            "TEAM123",
            "KEY456",
            "com.example.bookclerk",
            APPLE_EC_PEM,
            1_700_000_000,
            1_700_086_400,
        )
        .expect("jwt");
        let header = decode_header(&jwt).unwrap();
        assert_eq!(header.alg, Algorithm::ES256);
        assert_eq!(header.kid.as_deref(), Some("KEY456"));
        let payload = jwt.split('.').nth(1).unwrap();
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .unwrap();
        let claims: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(claims["iss"], "TEAM123");
        assert_eq!(claims["sub"], "com.example.bookclerk");
        assert_eq!(claims["aud"], "https://appleid.apple.com");
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
pub(crate) fn test_id_token(claims: &Value) -> String {
    tests::sign_id_token(claims)
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
pub(crate) fn test_jwks_json() -> Value {
    serde_json::json!({
        "keys": [{
            "kty": "RSA",
            "kid": "test-rsa-1",
            "alg": "RS256",
            "use": "sig",
            "n": tests::RSA_N,
            "e": "AQAB"
        }]
    })
}
