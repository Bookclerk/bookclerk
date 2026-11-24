//! The D1 database's guided setup: with the operator's Cloudflare API token
//! it finds (or creates) the library's D1 database and mints an account
//! token that can use D1 and nothing else, for the host to keep.
//!
//! The operator's token needs "D1 Edit" (to find and create the database)
//! and "Account API Tokens Edit" (to mint and delete the library's token).
//! It is used for this run only. The minted token is returned as the
//! `api_token` secret; `bookclerk config database setup d1` seals it with the
//! library's key and keeps it in the same store as the master key.
//!
//! A second run keeps the token the first run minted (its id comes back as
//! the `api_token_id` setting): shared branches of the library hold a copy,
//! and a new token would cut them off. With `rotate_token` it mints a fresh
//! token and deletes the earlier one, so tokens never pile up. The setup
//! never deletes a database: it holds the library. The replies are read by
//! [`bookclerk_storage::cloudflare_api`].

use bookclerk_plugin_sdk::http::Client;
use bookclerk_plugin_sdk::{
    DeprovisionRequest, PluginError, PluginErrorCode, ProvisionGrantKind, ProvisionRequest,
    Provisioned, ProvisionedSecret, Provisioning,
};
use bookclerk_storage::cloudflare_api::{
    account_resource, check_d1_database_name, find_d1_database, find_permission_group,
    parse_accounts, parse_d1_created, parse_done, parse_minted_token, token_request, ApiReplyError,
    D1_EDIT, D1_WRITE, MAX_API_REPLY_BYTES,
};
use serde::Deserialize;

/// Result alias matching the ABI crate.
type Result<T> = std::result::Result<T, PluginError>;

/// The Cloudflare API the setup calls.
pub const API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// The database name used when the operator chooses none.
pub const DEFAULT_DATABASE_NAME: &str = "bookclerk";

/// A Cloudflare account id: 32 hex digits, kept lowercase.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CloudflareAccountId(String);

impl CloudflareAccountId {
    /// Checks an account id.
    ///
    /// # Errors
    ///
    /// invalid_params when it is not 32 hex digits.
    fn parse(id: &str) -> Result<Self> {
        let id = id.trim();
        if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Self(id.to_ascii_lowercase()))
        } else {
            Err(PluginError::invalid_params(
                "the account ID is 32 hex digits (shown in the dashboard's account overview)",
            ))
        }
    }

    /// The id's text.
    fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where a new D1 database's data is kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Jurisdiction {
    /// Wherever Cloudflare places it.
    #[default]
    Default,
    /// Inside the European Union.
    Eu,
    /// FedRAMP-compliant data centers.
    Fedramp,
}

impl Jurisdiction {
    /// The setting's value.
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Eu => "eu",
            Self::Fedramp => "fedramp",
        }
    }

    /// The create call's `jurisdiction`, or `None` for the default.
    fn create_value(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Eu => Some("eu"),
            Self::Fedramp => Some("fedramp"),
        }
    }
}

/// The settings the setup reads: what the host's configuration holds, with
/// the operator's choices laid over it.
#[derive(Debug, Default, Deserialize)]
struct SetupConfig {
    /// Cloudflare account id; found from the token when unset.
    #[serde(default)]
    account_id: Option<String>,
    /// The database's name.
    #[serde(default)]
    database_name: Option<String>,
    /// Where a new database is created.
    #[serde(default)]
    jurisdiction: Jurisdiction,
    /// The id of the token an earlier run minted.
    #[serde(default)]
    api_token_id: Option<String>,
    /// Mint a new token (and delete the earlier one) even when an earlier
    /// run minted one.
    #[serde(default)]
    rotate_token: bool,
}

/// The `provisioning` entrypoint of the D1 guest.
pub struct D1Provisioning {
    /// The Cloudflare API base ([`API_BASE`] outside tests).
    api_base: String,
    /// The settings with the operator's choices laid over them.
    config: SetupConfig,
    /// The token an earlier run minted, when its id is known.
    previous_token_id: Option<String>,
}

impl D1Provisioning {
    /// The entrypoint for a setup whose settings are `config`.
    ///
    /// # Errors
    ///
    /// invalid_params when `config` is not an object of the expected shape.
    pub fn new(api_base: &str, config: serde_json::Value) -> Result<Self> {
        let config: SetupConfig = match config {
            serde_json::Value::Null => SetupConfig::default(),
            value => serde_json::from_value(value)
                .map_err(|err| PluginError::invalid_params(format!("d1 setup: {err}")))?,
        };
        let previous_token_id = config
            .api_token_id
            .as_deref()
            .map(|id| id.trim().to_ascii_lowercase())
            .filter(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()));
        Ok(Self {
            api_base: api_base.trim_end_matches('/').to_string(),
            config,
            previous_token_id,
        })
    }

    /// The account the run works in: the configured one, or the only one
    /// the token can see.
    async fn account(&self, api: &Api<'_>) -> Result<CloudflareAccountId> {
        if let Some(id) = self
            .config
            .account_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
        {
            return CloudflareAccountId::parse(id);
        }
        let (status, reply) = api
            .call(api.http.get(format!("{}/accounts", self.api_base)))
            .await?;
        let accounts =
            parse_accounts(&reply).map_err(|err| refusal("list accounts", status, &err))?;
        match accounts.as_slice() {
            [only] => CloudflareAccountId::parse(&only.id),
            [] => Err(PluginError::invalid_params(
                "the token can see no account; enter the account ID",
            )),
            many => Err(PluginError::invalid_params(format!(
                "the token can see {} accounts; enter the account ID of the one to use",
                many.len()
            ))),
        }
    }

    /// The database's id: the existing one of that name, or a new one.
    async fn database(&self, api: &Api<'_>, base: &str, name: &str) -> Result<(String, bool)> {
        let (status, reply) = api
            .call(
                api.http
                    .get(format!("{base}/d1/database"))
                    .query(&[("name", name)]),
            )
            .await?;
        if let Some(id) =
            find_d1_database(&reply, name).map_err(|err| refusal("list databases", status, &err))?
        {
            return Ok((id, false));
        }
        let mut body = serde_json::json!({ "name": name });
        if let Some(jurisdiction) = self.config.jurisdiction.create_value() {
            body["jurisdiction"] = serde_json::Value::from(jurisdiction);
        }
        let (status, reply) = api
            .call(api.http.post(format!("{base}/d1/database")).json(&body))
            .await?;
        let id =
            parse_d1_created(&reply).map_err(|err| refusal("create the database", status, &err))?;
        Ok((id, true))
    }

    /// The D1 write permission group's id, under either of its names.
    async fn d1_group(&self, api: &Api<'_>, base: &str) -> Result<String> {
        for name in [D1_WRITE, D1_EDIT] {
            let (status, reply) = api
                .call(
                    api.http
                        .get(format!("{base}/tokens/permission_groups"))
                        .query(&[("name", name)]),
                )
                .await?;
            if let Some(group) = find_permission_group(&reply, name)
                .map_err(|err| refusal("read the token permissions", status, &err))?
            {
                return Ok(group);
            }
        }
        Err(PluginError::forbidden(
            "the token cannot grant \"D1 Edit\"; give it Account API Tokens Edit",
        ))
    }
}

/// Maps a refused or unreadable reply to a plugin error the operator can
/// act on: 401 and 403 say the token lacks something.
fn refusal(step: &str, status: u16, err: &ApiReplyError) -> PluginError {
    let code = match status {
        401 => PluginErrorCode::Unauthorized,
        403 => PluginErrorCode::Forbidden,
        429 => PluginErrorCode::RateLimited,
        400..=499 => PluginErrorCode::InvalidParams,
        _ => PluginErrorCode::Unavailable,
    };
    PluginError::new(code, format!("could not {step}: {err}"))
}

/// The HTTP side of a run: one client and the operator's token.
struct Api<'a> {
    /// HTTP client.
    http: Client,
    /// The operator's token.
    token: &'a str,
}

impl<'a> Api<'a> {
    /// A client for `token`.
    fn new(token: &'a str) -> Result<Self> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|err| PluginError::internal(format!("d1 setup client: {err}")))?;
        Ok(Self { http, token })
    }

    /// Sends `request` with the token and reads the reply, at most one byte
    /// past [`MAX_API_REPLY_BYTES`] so the parser refuses an oversized one.
    async fn call(
        &self,
        request: bookclerk_plugin_sdk::http::RequestBuilder,
    ) -> Result<(u16, Vec<u8>)> {
        let mut reply = request
            .bearer_auth(self.token)
            .send()
            .await
            .map_err(|err| PluginError::unavailable(format!("the Cloudflare API: {err}")))?;
        let status = reply.status().as_u16();
        let mut bytes = Vec::new();
        while let Some(chunk) = reply
            .chunk()
            .await
            .map_err(|err| PluginError::unavailable(format!("the Cloudflare API: {err}")))?
        {
            let room = (MAX_API_REPLY_BYTES + 1).saturating_sub(bytes.len());
            bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if bytes.len() > MAX_API_REPLY_BYTES {
                break;
            }
        }
        Ok((status, bytes))
    }
}

/// The operator's API token from a grant.
fn grant_token(kind: ProvisionGrantKind, token: Option<&str>) -> Option<&str> {
    match kind {
        ProvisionGrantKind::ApiToken | ProvisionGrantKind::OauthAccessToken => {
            token.map(str::trim).filter(|token| !token.is_empty())
        }
        ProvisionGrantKind::None => None,
    }
}

#[async_trait::async_trait(?Send)]
impl Provisioning for D1Provisioning {
    async fn provision(&self, request: ProvisionRequest) -> Result<Provisioned> {
        let Some(token) = grant_token(request.grant.kind, request.grant.token.as_deref()) else {
            return Err(PluginError::invalid_params(
                "the guided setup needs a Cloudflare API token",
            ));
        };
        let name = self
            .config
            .database_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(DEFAULT_DATABASE_NAME)
            .to_string();
        check_d1_database_name(&name).map_err(PluginError::invalid_params)?;
        let api = Api::new(token)?;
        let account = self.account(&api).await?;
        let base = format!("{}/accounts/{}", self.api_base, account.as_str());
        let mut summary = Vec::new();

        let (database_id, created) = self.database(&api, &base, &name).await?;
        summary.push(if created {
            format!("Created D1 database {name}")
        } else {
            format!("Using your existing D1 database {name}")
        });

        if let Some(previous) = self
            .previous_token_id
            .as_deref()
            .filter(|_| !self.config.rotate_token)
        {
            summary.push("Kept the API token an earlier setup created".into());
            return Ok(Provisioned {
                settings_json: serde_json::json!({
                    "account_id": account.as_str(),
                    "database_id": database_id,
                    "database_name": name,
                    "jurisdiction": self.config.jurisdiction.as_str(),
                    "api_token_id": previous,
                })
                .to_string(),
                secrets: Vec::new(),
                summary,
            });
        }

        let group = self.d1_group(&api, &base).await?;
        let (status, reply) = api
            .call(api.http.post(format!("{base}/tokens")).json(&token_request(
                &format!("Bookclerk D1 {name}"),
                &group,
                &account_resource(account.as_str()),
            )))
            .await?;
        let minted = parse_minted_token(&reply)
            .map_err(|err| refusal("create the library's token", status, &err))?;
        summary.push("Created an API token that can use D1 and nothing else".into());

        if let Some(previous) = self
            .previous_token_id
            .as_deref()
            .filter(|id| *id != minted.id)
        {
            let (status, reply) = api
                .call(api.http.delete(format!("{base}/tokens/{previous}")))
                .await?;
            match parse_done(&reply) {
                Ok(()) => summary.push("Deleted the token an earlier setup created".into()),
                // Already gone: nothing to delete.
                Err(_) if status == 404 => {}
                Err(_) => summary.push(format!(
                    "Could not delete the earlier token {previous}; delete it in the Cloudflare dashboard"
                )),
            }
        }

        Ok(Provisioned {
            settings_json: serde_json::json!({
                "account_id": account.as_str(),
                "database_id": database_id,
                "database_name": name,
                "jurisdiction": self.config.jurisdiction.as_str(),
                "api_token_id": minted.id,
            })
            .to_string(),
            secrets: vec![ProvisionedSecret {
                key: "api_token".into(),
                value: minted.value.clone(),
            }],
            summary,
        })
    }

    async fn deprovision(&self, _request: DeprovisionRequest) -> Result<()> {
        // The database holds the library; nothing the setup created is
        // removed by it (`[provisioning] deprovision` is off).
        Err(PluginError::unsupported(
            "the D1 setup does not remove the library's database",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_sdk::ProvisionGrant;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const ACCOUNT: &str = "3f4b7e3dcab231cbfdaa90a6a28bd548";
    const GROUP: &str = "09b2857d1c31407795e75e3fed8617a1";
    const DB: &str = "0c3a1d2e-4b5f-4a6b-8c7d-9e0f1a2b3c4d";
    const NEW_TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OLD_TOKEN: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// A one-request-per-connection HTTP server answering from `answer`,
    /// recording each request line and its body.
    async fn serve(answer: fn(&str) -> (u16, String)) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        tokio::task::spawn_local(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(split) = text.find("\r\n\r\n") {
                        let length = text[..split]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buf.len() >= split + 4 + length {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                let line = text.lines().next().unwrap_or_default().to_string();
                let auth = text
                    .lines()
                    .any(|l| l.eq_ignore_ascii_case("authorization: Bearer op-token"));
                let body = text
                    .split("\r\n\r\n")
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                log.lock()
                    .unwrap()
                    .push(format!("{line} auth={auth} {body}"));
                let (status, reply) = answer(&line);
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (base, seen)
    }

    /// The JSON envelope around a successful `result`.
    fn ok(result: &str) -> (u16, String) {
        (
            200,
            format!(r#"{{"success":true,"errors":[],"result":{result}}}"#),
        )
    }

    /// A Cloudflare API with no database yet, which only knows the D1
    /// group under its dashboard name.
    fn fresh_account(line: &str) -> (u16, String) {
        if line.starts_with("GET /accounts ") {
            ok(&format!(r#"[{{"id":"{ACCOUNT}","name":"Robin"}}]"#))
        } else if line.starts_with("GET") && line.contains("/d1/database") {
            ok("[]")
        } else if line.starts_with("POST") && line.contains("/d1/database") {
            ok(&format!(r#"{{"uuid":"{DB}","name":"bookclerk"}}"#))
        } else if line.contains("/tokens/permission_groups") && line.contains("D1+Edit") {
            ok(&format!(r#"[{{"id":"{GROUP}","name":"D1 Edit"}}]"#))
        } else if line.contains("/tokens/permission_groups") {
            ok("[]")
        } else if line.starts_with("POST") && line.contains("/tokens") {
            ok(&format!(r#"{{"id":"{NEW_TOKEN}","value":"minted-value"}}"#))
        } else {
            (404, r#"{"success":false,"errors":[]}"#.into())
        }
    }

    /// A Cloudflare API where the database exists and the old token can't
    /// be deleted.
    fn existing_database(line: &str) -> (u16, String) {
        if line.starts_with("GET") && line.contains("/d1/database") {
            ok(&format!(r#"[{{"uuid":"{DB}","name":"library"}}]"#))
        } else if line.contains("/tokens/permission_groups") {
            ok(&format!(r#"[{{"id":"{GROUP}","name":"D1 Write"}}]"#))
        } else if line.starts_with("POST") && line.contains("/tokens") {
            ok(&format!(r#"{{"id":"{NEW_TOKEN}","value":"minted-value"}}"#))
        } else if line.starts_with("DELETE") {
            (
                403,
                r#"{"success":false,"errors":[{"code":9109,"message":"no"}]}"#.into(),
            )
        } else {
            (404, r#"{"success":false,"errors":[]}"#.into())
        }
    }

    fn grant() -> ProvisionGrant {
        ProvisionGrant {
            kind: ProvisionGrantKind::ApiToken,
            token: Some("op-token".into()),
            expires_at_unix_ms: None,
        }
    }

    fn request() -> ProvisionRequest {
        ProvisionRequest {
            grant: grant(),
            inputs_json: "{}".into(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_first_run_creates_the_database_and_mints_a_d1_token() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (base, seen) = serve(fresh_account).await;
                let setup =
                    D1Provisioning::new(&base, serde_json::json!({"jurisdiction": "eu"})).unwrap();
                let done = setup.provision(request()).await.expect("provisioned");
                let settings: serde_json::Value =
                    serde_json::from_str(&done.settings_json).unwrap();
                assert_eq!(
                    settings,
                    serde_json::json!({
                        "account_id": ACCOUNT,
                        "database_id": DB,
                        "database_name": "bookclerk",
                        "jurisdiction": "eu",
                        "api_token_id": NEW_TOKEN,
                    })
                );
                assert_eq!(done.secrets.len(), 1);
                assert_eq!(done.secrets[0].key, "api_token");
                assert_eq!(done.secrets[0].value, "minted-value");
                assert_eq!(
                    done.summary,
                    [
                        "Created D1 database bookclerk",
                        "Created an API token that can use D1 and nothing else",
                    ]
                );
                let seen = seen.lock().unwrap().clone();
                assert!(
                    seen.iter().all(|line| line.contains("auth=true")),
                    "{seen:?}"
                );
                let create = seen
                    .iter()
                    .find(|l| l.starts_with(&format!("POST /accounts/{ACCOUNT}/d1/database ")))
                    .unwrap();
                assert!(create.contains(r#""jurisdiction":"eu""#), "{create}");
                let mint = seen
                    .iter()
                    .find(|l| l.starts_with(&format!("POST /accounts/{ACCOUNT}/tokens ")))
                    .unwrap();
                assert!(
                    mint.contains(&format!("com.cloudflare.api.account.{ACCOUNT}")),
                    "{mint}"
                );
                assert!(mint.contains(GROUP), "{mint}");
                assert!(!seen.iter().any(|l| l.starts_with("DELETE")));
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_rerun_reuses_the_database_and_reports_an_undeleted_token() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (base, seen) = serve(existing_database).await;
                let setup = D1Provisioning::new(
                    &base,
                    serde_json::json!({
                        "account_id": ACCOUNT,
                        "database_name": "library",
                        "api_token_id": OLD_TOKEN,
                        "rotate_token": true,
                    }),
                )
                .unwrap();
                let done = setup.provision(request()).await.expect("provisioned");
                assert_eq!(done.summary[0], "Using your existing D1 database library");
                assert!(
                    done.summary[2].contains("Could not delete"),
                    "{:?}",
                    done.summary
                );
                let seen = seen.lock().unwrap().clone();
                assert!(!seen
                    .iter()
                    .any(|l| l.starts_with("POST /accounts/") && l.contains("/d1/")));
                assert!(seen
                    .iter()
                    .any(|l| l
                        .starts_with(&format!("DELETE /accounts/{ACCOUNT}/tokens/{OLD_TOKEN} "))));
            })
            .await;
    }

    /// Without `rotate_token`, a re-run keeps the earlier token: no token
    /// is minted or deleted, so shared branches keep working.
    #[tokio::test(flavor = "current_thread")]
    async fn a_rerun_keeps_the_earlier_token_unless_asked_to_rotate() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (base, seen) = serve(existing_database).await;
                let setup = D1Provisioning::new(
                    &base,
                    serde_json::json!({
                        "account_id": ACCOUNT,
                        "database_name": "library",
                        "api_token_id": OLD_TOKEN,
                    }),
                )
                .unwrap();
                let done = setup.provision(request()).await.expect("provisioned");
                assert!(done.secrets.is_empty(), "no new token");
                assert_eq!(
                    done.summary.last().map(String::as_str),
                    Some("Kept the API token an earlier setup created")
                );
                let settings: serde_json::Value =
                    serde_json::from_str(&done.settings_json).unwrap();
                assert_eq!(settings["api_token_id"], OLD_TOKEN);
                let seen = seen.lock().unwrap().clone();
                assert!(!seen.iter().any(|l| l.contains("/tokens")), "{seen:?}");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bad_choices_and_missing_grants_are_refused() {
        let setup = D1Provisioning::new(API_BASE, serde_json::json!({"database_name": "Bad Name"}))
            .unwrap();
        let err = setup.provision(request()).await.unwrap_err();
        assert_eq!(err.code, PluginErrorCode::InvalidParams);
        let err = setup
            .provision(ProvisionRequest {
                grant: ProvisionGrant::default(),
                inputs_json: "{}".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::InvalidParams);
        let err = setup
            .deprovision(DeprovisionRequest::default())
            .await
            .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::Unsupported);
        assert!(D1Provisioning::new(API_BASE, serde_json::json!([1])).is_err());
        assert!(D1Provisioning::new(API_BASE, serde_json::Value::Null).is_ok());
        assert!(CloudflareAccountId::parse("nope").is_err());
        let bad_account = D1Provisioning::new(API_BASE, serde_json::json!({"account_id": "nope"}))
            .unwrap()
            .provision(request())
            .await
            .unwrap_err();
        assert_eq!(bad_account.code, PluginErrorCode::InvalidParams);
        assert_eq!(
            refusal("x", 429, &ApiReplyError::TooLarge).code,
            PluginErrorCode::RateLimited
        );
        assert_eq!(
            refusal("x", 500, &ApiReplyError::TooLarge).code,
            PluginErrorCode::Unavailable
        );
    }
}
