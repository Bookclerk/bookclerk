//! The R2 destination's guided setup: with the operator's Cloudflare API
//! token it creates (or reuses) the bucket and mints an API token scoped to
//! that one bucket, whose id and hashed value are the S3 credentials the
//! destination then uses.
//!
//! The operator's token needs "Workers R2 Storage Edit" (to create the
//! bucket) and "Account API Tokens Edit" (to mint and delete the bucket's
//! token). It is used for this run only; the destination keeps only the
//! bucket token's S3 credentials, which can read and write that bucket's
//! objects and nothing else.
//!
//! A second run mints a fresh bucket token and deletes the one the first
//! run minted, so a re-run rotates the credentials rather than piling up
//! tokens. Deprovisioning deletes the bucket token and leaves the bucket
//! and its books alone. The replies are read by
//! [`bookclerk_storage::cloudflare_api`].

use bookclerk_plugin_sdk::http::Client;
use bookclerk_plugin_sdk::{
    DeprovisionRequest, PluginError, PluginErrorCode, ProvisionGrantKind, ProvisionRequest,
    Provisioned, ProvisionedSecret, Provisioning,
};
use bookclerk_storage::cloudflare_api::{
    bucket_resource, check_bucket_name, find_permission_group, parse_accounts,
    parse_bucket_created, parse_done, parse_minted_token, token_request, ApiReplyError,
    BucketCreated, BUCKET_ITEM_WRITE, MAX_API_REPLY_BYTES,
};
use bookclerk_storage::CloudflareAccountId;
use serde::Deserialize;

use crate::plugin::Jurisdiction;

/// Result alias matching the ABI crate.
type Result<T> = std::result::Result<T, PluginError>;

/// The Cloudflare API the setup calls.
pub const API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// The settings the setup reads: what is stored, with the operator's
/// choices laid over it.
#[derive(Debug, Default, Deserialize)]
struct SetupConfig {
    /// Cloudflare account id; found from the token when unset.
    #[serde(default)]
    account_id: Option<String>,
    /// The bucket's jurisdiction.
    #[serde(default)]
    jurisdiction: Jurisdiction,
    /// Bucket name.
    #[serde(default)]
    bucket: Option<String>,
}

/// The stored secrets the setup reads.
#[derive(Default, Deserialize)]
struct SetupSecrets {
    /// The access key id an earlier run stored: the id of the bucket token
    /// it minted.
    #[serde(default)]
    access_key_id: Option<String>,
}

/// The `provisioning` entrypoint of the R2 guest.
pub struct R2Provisioning {
    /// The Cloudflare API base ([`API_BASE`] outside tests).
    api_base: String,
    /// The stored settings with the operator's choices laid over them.
    config: SetupConfig,
    /// The bucket token an earlier run minted, when its id is stored.
    previous_token_id: Option<String>,
}

impl R2Provisioning {
    /// The entrypoint for an instance whose `CONFIG` and `SECRETS` are
    /// `config` and `secrets`.
    ///
    /// # Errors
    ///
    /// invalid_params when either is not an object of the expected shape.
    pub fn new(
        api_base: &str,
        config: serde_json::Value,
        secrets: serde_json::Value,
    ) -> Result<Self> {
        let invalid =
            |err: serde_json::Error| PluginError::invalid_params(format!("r2 setup: {err}"));
        let config: SetupConfig = match config {
            serde_json::Value::Null => SetupConfig::default(),
            value => serde_json::from_value(value).map_err(invalid)?,
        };
        let secrets: SetupSecrets = match secrets {
            serde_json::Value::Null => SetupSecrets::default(),
            value => serde_json::from_value(value).map_err(invalid)?,
        };
        let previous_token_id = secrets
            .access_key_id
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
            return CloudflareAccountId::parse(id)
                .map_err(|err| PluginError::invalid_params(err.to_string()));
        }
        let (status, reply) = api
            .call(api.http.get(format!("{}/accounts", self.api_base)))
            .await?;
        let accounts =
            parse_accounts(&reply).map_err(|err| refusal("list accounts", status, &err))?;
        match accounts.as_slice() {
            [only] => CloudflareAccountId::parse(&only.id)
                .map_err(|err| PluginError::invalid_params(err.to_string())),
            [] => Err(PluginError::invalid_params(
                "the token can see no account; enter the account ID",
            )),
            many => Err(PluginError::invalid_params(format!(
                "the token can see {} accounts; enter the account ID of the one to use",
                many.len()
            ))),
        }
    }
}

/// Text after the jurisdiction's `cf-r2-jurisdiction` header value, or
/// `None` for the default jurisdiction.
fn jurisdiction_header(jurisdiction: Jurisdiction) -> Option<&'static str> {
    match jurisdiction {
        Jurisdiction::Default => None,
        Jurisdiction::Eu => Some("eu"),
        Jurisdiction::Fedramp => Some("fedramp"),
    }
}

/// The jurisdiction as the token resource names it.
fn jurisdiction_name(jurisdiction: Jurisdiction) -> &'static str {
    match jurisdiction {
        Jurisdiction::Default => "default",
        Jurisdiction::Eu => "eu",
        Jurisdiction::Fedramp => "fedramp",
    }
}

/// Maps a refused or unreadable reply to a plugin error the setup screen
/// can act on: 401 and 403 say the token lacks something.
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
            .map_err(|err| PluginError::internal(format!("r2 setup client: {err}")))?;
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
impl Provisioning for R2Provisioning {
    async fn provision(&self, request: ProvisionRequest) -> Result<Provisioned> {
        let Some(token) = grant_token(request.grant.kind, request.grant.token.as_deref()) else {
            return Err(PluginError::invalid_params(
                "the guided setup needs a Cloudflare API token",
            ));
        };
        let bucket = self
            .config
            .bucket
            .as_deref()
            .map(str::trim)
            .filter(|bucket| !bucket.is_empty())
            .ok_or_else(|| PluginError::invalid_params("choose a bucket name"))?
            .to_string();
        check_bucket_name(&bucket).map_err(PluginError::invalid_params)?;
        let api = Api::new(token)?;
        let account = self.account(&api).await?;
        let base = format!("{}/accounts/{}", self.api_base, account.as_str());
        let mut summary = Vec::new();

        let mut create = api
            .http
            .post(format!("{base}/r2/buckets"))
            .json(&serde_json::json!({ "name": bucket }));
        if let Some(header) = jurisdiction_header(self.config.jurisdiction) {
            create = create.header("cf-r2-jurisdiction", header);
        }
        let (status, reply) = api.call(create).await?;
        match parse_bucket_created(&reply)
            .map_err(|err| refusal("create the bucket", status, &err))?
        {
            BucketCreated::Created => summary.push(format!("Created bucket {bucket}")),
            BucketCreated::AlreadyOwned => {
                summary.push(format!("Using your existing bucket {bucket}"))
            }
        }

        let (status, reply) = api
            .call(
                api.http
                    .get(format!("{base}/tokens/permission_groups"))
                    .query(&[("name", BUCKET_ITEM_WRITE)]),
            )
            .await?;
        let group = find_permission_group(&reply, BUCKET_ITEM_WRITE)
            .map_err(|err| refusal("read the token permissions", status, &err))?
            .ok_or_else(|| {
                PluginError::forbidden(format!(
                    "the token cannot grant \"{BUCKET_ITEM_WRITE}\"; give it Account API Tokens Edit"
                ))
            })?;

        let resource = bucket_resource(
            account.as_str(),
            jurisdiction_name(self.config.jurisdiction),
            &bucket,
        );
        let (status, reply) = api
            .call(api.http.post(format!("{base}/tokens")).json(&token_request(
                &format!("Bookclerk {bucket}"),
                &group,
                &resource,
            )))
            .await?;
        let minted = parse_minted_token(&reply)
            .map_err(|err| refusal("create the bucket's token", status, &err))?;
        summary.push(format!(
            "Created an API token that can read and write only {bucket}"
        ));

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
                "bucket": bucket,
                "jurisdiction": jurisdiction_name(self.config.jurisdiction),
            })
            .to_string(),
            secrets: vec![
                ProvisionedSecret {
                    key: "access_key_id".into(),
                    value: minted.id.clone(),
                },
                ProvisionedSecret {
                    key: "secret_access_key".into(),
                    value: minted.secret_access_key(),
                },
            ],
            summary,
        })
    }

    async fn deprovision(&self, request: DeprovisionRequest) -> Result<()> {
        let Some(previous) = self.previous_token_id.as_deref() else {
            // Nothing the setup created is recorded.
            return Ok(());
        };
        let Some(token) = grant_token(request.grant.kind, request.grant.token.as_deref()) else {
            return Err(PluginError::new(
                PluginErrorCode::Unauthorized,
                "deleting the bucket's token needs a Cloudflare API token with Account API Tokens Edit",
            ));
        };
        let account = self
            .config
            .account_id
            .as_deref()
            .map(CloudflareAccountId::parse)
            .transpose()
            .map_err(|err| PluginError::invalid_params(err.to_string()))?
            .ok_or_else(|| PluginError::invalid_params("the account ID is not set"))?;
        let api = Api::new(token)?;
        let (status, reply) = api
            .call(api.http.delete(format!(
                "{}/accounts/{}/tokens/{previous}",
                self.api_base,
                account.as_str()
            )))
            .await?;
        match parse_done(&reply) {
            Ok(()) => Ok(()),
            Err(_) if status == 404 => Ok(()),
            Err(err) => Err(refusal("delete the bucket's token", status, &err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_sdk::ProvisionGrant;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const ACCOUNT: &str = "3f4b7e3dcab231cbfdaa90a6a28bd548";
    const GROUP: &str = "2efd5506f9c8494dacb1fa10a3e7d5b6";
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
                // Read the head, then the body by Content-Length.
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
                                let lower = line.to_ascii_lowercase();
                                lower
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

    fn cloudflare(line: &str) -> (u16, String) {
        let ok = |result: String| {
            (
                200,
                format!(r#"{{"success":true,"errors":[],"result":{result}}}"#),
            )
        };
        if line.starts_with("GET /accounts ") || line.starts_with("GET /accounts?") {
            ok(format!(r#"[{{"id":"{ACCOUNT}","name":"Robin"}}]"#))
        } else if line.contains("/r2/buckets") {
            (
                409,
                r#"{"success":false,"errors":[{"code":10004,"message":"owned"}]}"#.into(),
            )
        } else if line.contains("/tokens/permission_groups") {
            ok(format!(
                r#"[{{"id":"{GROUP}","name":"{BUCKET_ITEM_WRITE}"}}]"#
            ))
        } else if line.starts_with("POST") && line.contains("/tokens") {
            ok(format!(r#"{{"id":"{NEW_TOKEN}","value":"abc"}}"#))
        } else if line.starts_with("DELETE") {
            ok(format!(r#"{{"id":"{OLD_TOKEN}"}}"#))
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

    #[tokio::test(flavor = "current_thread")]
    async fn a_run_reuses_the_bucket_mints_a_token_and_rotates_the_old_one() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (base, seen) = serve(cloudflare).await;
                let setup = R2Provisioning::new(
                    &base,
                    serde_json::json!({"bucket": "books", "jurisdiction": "eu"}),
                    serde_json::json!({"access_key_id": OLD_TOKEN}),
                )
                .unwrap();
                let done = setup
                    .provision(ProvisionRequest {
                        grant: grant(),
                        inputs_json: "{}".into(),
                    })
                    .await
                    .expect("provisioned");
                let settings: serde_json::Value =
                    serde_json::from_str(&done.settings_json).unwrap();
                assert_eq!(
                    settings,
                    serde_json::json!({"account_id": ACCOUNT, "bucket": "books", "jurisdiction": "eu"})
                );
                assert_eq!(done.secrets[0].value, NEW_TOKEN);
                assert_eq!(
                    done.secrets[1].value,
                    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                );
                assert_eq!(
                    done.summary,
                    [
                        "Using your existing bucket books",
                        "Created an API token that can read and write only books",
                        "Deleted the token an earlier setup created",
                    ]
                );
                let seen = seen.lock().unwrap().clone();
                assert!(seen.iter().all(|line| line.contains("auth=true")), "{seen:?}");
                assert!(seen[0].starts_with("GET /accounts "), "{seen:?}");
                let mint = seen.iter().find(|l| l.starts_with(&format!("POST /accounts/{ACCOUNT}/tokens "))).unwrap();
                assert!(mint.contains(&format!("com.cloudflare.edge.r2.bucket.{ACCOUNT}_eu_books")), "{mint}");
                assert!(seen.iter().any(|l| l.starts_with(&format!("DELETE /accounts/{ACCOUNT}/tokens/{OLD_TOKEN} "))));
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bad_choices_and_missing_grants_are_refused() {
        let setup = R2Provisioning::new(
            API_BASE,
            serde_json::json!({"bucket": "Bad_Name"}),
            serde_json::Value::Null,
        )
        .unwrap();
        let err = setup
            .provision(ProvisionRequest {
                grant: grant(),
                inputs_json: "{}".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::InvalidParams);
        let err = setup
            .provision(ProvisionRequest {
                grant: ProvisionGrant::default(),
                inputs_json: "{}".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::InvalidParams);

        // Deprovision with nothing recorded is a no-op; with a token id but
        // no grant it asks for one.
        setup
            .deprovision(DeprovisionRequest::default())
            .await
            .expect("nothing to remove");
        let recorded = R2Provisioning::new(
            API_BASE,
            serde_json::json!({"account_id": ACCOUNT, "bucket": "books"}),
            serde_json::json!({"access_key_id": OLD_TOKEN}),
        )
        .unwrap();
        let err = recorded
            .deprovision(DeprovisionRequest::default())
            .await
            .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::Unauthorized);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deprovision_deletes_the_bucket_token() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (base, seen) = serve(cloudflare).await;
                let setup = R2Provisioning::new(
                    &base,
                    serde_json::json!({"account_id": ACCOUNT, "bucket": "books"}),
                    serde_json::json!({"access_key_id": OLD_TOKEN}),
                )
                .unwrap();
                setup
                    .deprovision(DeprovisionRequest { grant: grant() })
                    .await
                    .expect("deprovisioned");
                assert!(seen.lock().unwrap()[0]
                    .starts_with(&format!("DELETE /accounts/{ACCOUNT}/tokens/{OLD_TOKEN} ")));
            })
            .await;
    }
}
