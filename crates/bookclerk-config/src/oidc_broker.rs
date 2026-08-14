//! Optional upstream OIDC/OAuth identity-broker settings (`[auth.oidc]`).
//!
//! Bookclerk remains the authorization server for Audiobookshelf. These
//! providers are *relying party* entries used to sign Users in.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};

/// How an upstream provider decides who may become a first-party User.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OidcProvisionMode {
    /// Require a mapped group/role claim (`owner` / `administrator` / `member`).
    #[default]
    MappedRole,
    /// Any authenticated account; JIT as [`OidcProviderConfig::default_role`].
    Any,
    /// Authenticated and allowlisted (email domain and/or `sub`).
    Allowlist,
    /// No JIT; SSO only links a pre-created User.
    InviteOnly,
}

/// One upstream OIDC or OAuth 2.0 provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OidcProviderConfig {
    /// Stable id used in URLs and `portal_identities.provider` (`oidc:{id}`).
    pub id: String,
    /// Login-button label.
    pub name: String,
    /// Optional well-known preset (`google`, `github`, `apple`, `discord`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// OIDC issuer URL (discovery). Ignored when `preset` fills endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    /// OAuth client id.
    pub client_id: String,
    /// Optional client secret (prefer env / `encrypted_secrets`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Scopes requested at authorize.
    pub scopes: Vec<String>,
    /// Provisioning policy for this provider.
    pub provision: OidcProvisionMode,
    /// Role used for [`OidcProvisionMode::Any`] (and allowlist without maps).
    pub default_role: String,
    /// Claim name holding groups/roles (typically `groups` or `roles`).
    pub role_claim: String,
    /// Upstream group/role value → Bookclerk role (`owner`/`administrator`/`member`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub role_map: BTreeMap<String, String>,
    /// When true, a new `sub` may attach to the unique matching *verified* email User.
    ///
    /// Defaults off: an unverified upstream email must not attach to an existing Owner.
    pub link_by_email: bool,
    /// Sign in with Apple team ID (`iss` of the client-secret JWT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apple_team_id: Option<String>,
    /// Sign in with Apple key ID (`kid` header of the client-secret JWT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apple_key_id: Option<String>,
    /// PEM-encoded Apple `.p8` private key (prefer env / `encrypted_secrets`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apple_private_key: Option<String>,
    /// Email domains allowed for this provider (empty = no extra filter).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_email_domains: Vec<String>,
    /// Exact emails allowed when `provision = allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_emails: Vec<String>,
    /// Explicit upstream `sub` values allowed when `provision = allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_subjects: Vec<String>,
}

impl Default for OidcProviderConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            preset: None,
            issuer: None,
            client_id: String::new(),
            client_secret: None,
            scopes: vec!["openid".into(), "profile".into(), "email".into()],
            provision: OidcProvisionMode::MappedRole,
            default_role: "member".into(),
            role_claim: "groups".into(),
            role_map: BTreeMap::new(),
            link_by_email: false,
            apple_team_id: None,
            apple_key_id: None,
            apple_private_key: None,
            allowed_email_domains: Vec::new(),
            allowed_emails: Vec::new(),
            allowed_subjects: Vec::new(),
        }
    }
}

impl OidcProviderConfig {
    /// Portal identity provider key (`oidc:{id}`).
    #[must_use]
    pub fn portal_provider(&self) -> String {
        format!("oidc:{}", self.id.trim())
    }

    /// Display name, falling back to id.
    #[must_use]
    pub fn display_name(&self) -> &str {
        let name = self.name.trim();
        if name.is_empty() {
            self.id.trim()
        } else {
            name
        }
    }

    /// Canonical social preset (`google`, `github`, `apple`, `discord`).
    ///
    /// Matching is ASCII case-insensitive so `GitHub` and `apple` select the
    /// same endpoints, scopes, and profile adapter.
    #[must_use]
    pub fn social_preset(&self) -> Option<&'static str> {
        let preset = self.preset.as_deref().map(str::trim).unwrap_or("");
        ["google", "github", "apple", "discord"]
            .into_iter()
            .find(|name| preset.eq_ignore_ascii_case(name))
    }

    /// Scopes sent at authorize, with social-preset defaults when unset.
    #[must_use]
    pub fn effective_scopes(&self) -> Vec<String> {
        let is_oidc_default = self.scopes.is_empty()
            || self
                .scopes
                .iter()
                .all(|s| matches!(s.trim(), "openid" | "profile" | "email"));
        match self.social_preset() {
            Some("github") if is_oidc_default => vec!["read:user".into(), "user:email".into()],
            Some("discord") if is_oidc_default => vec!["identify".into(), "email".into()],
            _ => {
                if self.scopes.is_empty() {
                    vec!["openid".into(), "profile".into(), "email".into()]
                } else {
                    self.scopes.clone()
                }
            }
        }
    }
}

/// `[auth.oidc]` — optional identity broker.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OidcBrokerConfig {
    /// When false, RP routes report disabled and login buttons are hidden.
    pub enabled: bool,
    /// Global email-domain allowlist applied after the provider policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_email_domains: Vec<String>,
    /// Upstream providers (enterprise and/or social).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<OidcProviderConfig>,
    /// Durable `encrypted_secrets` generation for this broker revision.
    ///
    /// Runtime and `config.toml` must agree before rows for this generation
    /// are used. `0` is the unversioned legacy name.
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub secret_generation: u64,
}

/// Serde skip predicate: omit `secret_generation` when it is the legacy `0`.
fn u64_is_zero(v: &u64) -> bool {
    *v == 0
}

/// `encrypted_secrets.name` for an OIDC client secret or Apple key.
///
/// Generation `0` is the unversioned legacy name; later generations use
/// `name@generation` so a crashed PUT cannot make new credentials visible
/// under the still-published config.
#[must_use]
pub fn oidc_secret_store_name(logical: &str, generation: u64) -> String {
    let logical = logical.trim();
    if generation == 0 {
        logical.to_string()
    } else {
        format!("{logical}@{generation}")
    }
}

/// Environment variable holding a provider's client secret.
///
/// Hyphens in `provider_id` become underscores. Example: `github` →
/// `BOOKCLERK_OIDC_GITHUB_CLIENT_SECRET`.
#[must_use]
pub fn oidc_client_secret_env_key(provider_id: &str) -> String {
    format!(
        "BOOKCLERK_OIDC_{}_CLIENT_SECRET",
        provider_id.trim().to_ascii_uppercase().replace('-', "_")
    )
}

/// Environment variable holding a Sign in with Apple `.p8` private key.
#[must_use]
pub fn oidc_apple_private_key_env_key(provider_id: &str) -> String {
    format!(
        "BOOKCLERK_OIDC_{}_APPLE_PRIVATE_KEY",
        provider_id.trim().to_ascii_uppercase().replace('-', "_")
    )
}

impl OidcBrokerConfig {
    /// Whether this section is the serde default (disabled, no providers).
    #[must_use]
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    /// Enabled providers with a non-empty id.
    #[must_use]
    pub fn enabled_providers(&self) -> Vec<&OidcProviderConfig> {
        if !self.enabled {
            return Vec::new();
        }
        self.providers
            .iter()
            .filter(|p| !p.id.trim().is_empty() && !p.client_id.trim().is_empty())
            .collect()
    }

    /// Look up a provider by id.
    #[must_use]
    pub fn provider(&self, id: &str) -> Option<&OidcProviderConfig> {
        let id = id.trim();
        self.enabled_providers()
            .into_iter()
            .find(|p| p.id.trim() == id)
    }

    /// Reject operator maps, unknown roles, duplicate ids, and empty ids when enabled.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.validate_providers()
    }

    /// Validate provider rows even when the broker is disabled.
    ///
    /// Used by the Settings API so a disabled-but-invalid draft cannot be
    /// written and then fail [`Config::load`] when later enabled.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn validate_providers(&self) -> Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for provider in &self.providers {
            let id = provider.id.trim();
            if id.is_empty() {
                return Err(ConfigError::Invalid(
                    "[auth.oidc] provider id must not be empty".into(),
                ));
            }
            if !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(ConfigError::Invalid(format!(
                    "[auth.oidc] provider id `{id}` must be alphanumeric, hyphen, or underscore"
                )));
            }
            if !seen.insert(id.to_string()) {
                return Err(ConfigError::Invalid(format!(
                    "[auth.oidc] duplicate provider id `{id}`"
                )));
            }
            if provider.client_id.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "[auth.oidc.{id}] client_id is required"
                )));
            }
            if provider.preset.is_none()
                && provider.issuer.as_deref().unwrap_or("").trim().is_empty()
            {
                return Err(ConfigError::Invalid(format!(
                    "[auth.oidc.{id}] issuer or preset is required"
                )));
            }
            if provider.social_preset() == Some("apple") {
                if provider
                    .apple_team_id
                    .as_deref()
                    .map(str::trim)
                    .is_none_or(|s| s.is_empty())
                {
                    return Err(ConfigError::Invalid(format!(
                        "[auth.oidc.{id}] apple_team_id is required for the Apple preset"
                    )));
                }
                if provider
                    .apple_key_id
                    .as_deref()
                    .map(str::trim)
                    .is_none_or(|s| s.is_empty())
                {
                    return Err(ConfigError::Invalid(format!(
                        "[auth.oidc.{id}] apple_key_id is required for the Apple preset"
                    )));
                }
            }
            validate_role(
                &provider.default_role,
                &format!("[auth.oidc.{id}] default_role"),
            )?;
            for (claim, role) in &provider.role_map {
                validate_role(role, &format!("[auth.oidc.{id}] role_map.{claim}"))?;
            }
        }
        Ok(())
    }
}

/// Accepts `owner` / `administrator` / `member`; rejects `operator` and unknown roles.
fn validate_role(role: &str, field: &str) -> Result<()> {
    match role.trim() {
        "owner" | "administrator" | "member" => Ok(()),
        "operator" => Err(ConfigError::Invalid(format!(
            "{field} cannot be `operator` — the Operator account is local-only"
        ))),
        other => Err(ConfigError::Invalid(format!(
            "{field} must be owner, administrator, or member (got `{other}`)"
        ))),
    }
}

/// Pick the highest Bookclerk role from mapped claim values.
///
/// Precedence: owner > administrator > member.
#[must_use]
pub fn resolve_mapped_role(
    role_map: &BTreeMap<String, String>,
    claim_values: &[String],
) -> Option<String> {
    let mut best: Option<&'static str> = None;
    for raw in claim_values {
        let key = raw.trim();
        let Some(mapped) = role_map
            .get(key)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let rank = match mapped {
            "owner" => 3,
            "administrator" => 2,
            "member" => 1,
            _ => continue,
        };
        let current = match best {
            Some("owner") => 3,
            Some("administrator") => 2,
            Some("member") => 1,
            _ => 0,
        };
        if rank > current {
            best = Some(match mapped {
                "owner" => "owner",
                "administrator" => "administrator",
                _ => "member",
            });
        }
    }
    best.map(str::to_string)
}

/// Whether an allowlist provider admits this `(email, sub)`.
///
/// Empty email, domain, and subject lists fail closed. When the provider lists
/// are empty, `global_domains` is used as the domain allowlist.
#[must_use]
pub fn allowlist_permits(
    email: Option<&str>,
    sub: &str,
    allowed_emails: &[String],
    allowed_domains: &[String],
    allowed_subjects: &[String],
    global_domains: &[String],
) -> bool {
    let domains: &[String] =
        if allowed_emails.is_empty() && allowed_domains.is_empty() && allowed_subjects.is_empty() {
            global_domains
        } else {
            allowed_domains
        };
    if allowed_emails.is_empty() && domains.is_empty() && allowed_subjects.is_empty() {
        return false;
    }
    let email_ok = email
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some_and(|e| {
            allowed_emails
                .iter()
                .any(|allowed| allowed.trim().eq_ignore_ascii_case(e))
        });
    let domain_ok = !domains.is_empty() && email_domain_allowed(email, domains);
    let sub_ok = allowed_subjects.iter().any(|s| s.trim() == sub);
    email_ok || domain_ok || sub_ok
}

/// Whether `email` matches an allowlist of domains (empty list = unrestricted).
#[must_use]
pub fn email_domain_allowed(email: Option<&str>, domains: &[String]) -> bool {
    if domains.is_empty() {
        return true;
    }
    let Some(email) = email.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let Some((_, domain)) = email.rsplit_once('@') else {
        return false;
    };
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    domains.iter().any(|allowed| {
        allowed
            .trim()
            .trim_start_matches('@')
            .trim_end_matches('.')
            .eq_ignore_ascii_case(&domain)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oidc_client_secret_env_key_uppercases_hyphens() {
        assert_eq!(
            oidc_client_secret_env_key("my-idp"),
            "BOOKCLERK_OIDC_MY_IDP_CLIENT_SECRET"
        );
    }

    #[test]
    fn mapped_role_prefers_owner() {
        let mut map = BTreeMap::new();
        map.insert("bookclerk-users".into(), "member".into());
        map.insert("bookclerk-admins".into(), "administrator".into());
        map.insert("bookclerk-owners".into(), "owner".into());
        let role =
            resolve_mapped_role(&map, &["bookclerk-users".into(), "bookclerk-owners".into()]);
        assert_eq!(role.as_deref(), Some("owner"));
    }

    #[test]
    fn validate_rejects_operator_map() {
        let cfg = OidcBrokerConfig {
            enabled: true,
            providers: vec![OidcProviderConfig {
                id: "corp".into(),
                client_id: "bookclerk".into(),
                issuer: Some("https://idp.example".into()),
                role_map: BTreeMap::from([("ops".into(), "operator".into())]),
                ..OidcProviderConfig::default()
            }],
            ..OidcBrokerConfig::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("operator"), "{err}");
    }

    #[test]
    fn email_domain_filter() {
        assert!(email_domain_allowed(
            Some("a@Family.Example"),
            &["family.example".into()]
        ));
        assert!(!email_domain_allowed(
            Some("a@other.test"),
            &["family.example".into()]
        ));
        assert!(email_domain_allowed(Some("a@x.test"), &[]));
    }

    #[test]
    fn allowlist_fails_closed_when_empty() {
        assert!(!allowlist_permits(
            Some("a@x.test"),
            "sub-1",
            &[],
            &[],
            &[],
            &[]
        ));
    }

    #[test]
    fn allowlist_matches_email_or_sub() {
        assert!(allowlist_permits(
            Some("a@family.example"),
            "sub-1",
            &["a@family.example".into()],
            &[],
            &[],
            &[]
        ));
        assert!(allowlist_permits(
            Some("other@x.test"),
            "sub-1",
            &[],
            &[],
            &["sub-1".into()],
            &[]
        ));
        assert!(!allowlist_permits(
            Some("other@x.test"),
            "nope",
            &["a@family.example".into()],
            &[],
            &["sub-1".into()],
            &[]
        ));
    }

    #[test]
    fn social_preset_is_ascii_case_insensitive() {
        let mut provider = OidcProviderConfig {
            preset: Some("GitHub".into()),
            ..OidcProviderConfig::default()
        };
        assert_eq!(provider.social_preset(), Some("github"));
        provider.preset = Some("APPLE".into());
        assert_eq!(provider.social_preset(), Some("apple"));
        provider.preset = Some("Discord".into());
        assert_eq!(provider.social_preset(), Some("discord"));
        provider.preset = Some("google".into());
        assert_eq!(provider.social_preset(), Some("google"));
        provider.preset = Some("custom".into());
        assert_eq!(provider.social_preset(), None);
    }

    #[test]
    fn effective_scopes_use_social_defaults_for_mixed_case_preset() {
        let github = OidcProviderConfig {
            preset: Some("GitHub".into()),
            ..OidcProviderConfig::default()
        };
        assert_eq!(
            github.effective_scopes(),
            vec!["read:user".to_string(), "user:email".into()]
        );
        let discord = OidcProviderConfig {
            preset: Some("DISCORD".into()),
            ..OidcProviderConfig::default()
        };
        assert_eq!(
            discord.effective_scopes(),
            vec!["identify".to_string(), "email".into()]
        );
        let apple = OidcProviderConfig {
            preset: Some("Apple".into()),
            ..OidcProviderConfig::default()
        };
        assert_eq!(
            apple.effective_scopes(),
            vec!["openid".to_string(), "profile".into(), "email".into()]
        );
    }

    #[test]
    fn apple_preset_requires_team_and_key_id() {
        let cfg = OidcBrokerConfig {
            enabled: true,
            providers: vec![OidcProviderConfig {
                id: "apple".into(),
                client_id: "com.example.bookclerk".into(),
                preset: Some("Apple".into()),
                ..OidcProviderConfig::default()
            }],
            ..OidcBrokerConfig::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("apple_team_id"), "{err}");
    }

    #[test]
    fn link_by_email_defaults_off() {
        assert!(!OidcProviderConfig::default().link_by_email);
    }

    #[test]
    fn oidc_secret_store_name_versions_after_generation_zero() {
        assert_eq!(oidc_secret_store_name("github", 0), "github");
        assert_eq!(oidc_secret_store_name("github", 1), "github@1");
        assert_eq!(
            oidc_secret_store_name("github__apple_key", 2),
            "github__apple_key@2"
        );
    }
}
