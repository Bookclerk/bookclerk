//! Inbound webhooks: URL tokens and the requests stored for each plugin
//! instance until its guest pulls and acknowledges them (storage
//! destinations ADR, `docs/adr/storage-destinations.md`, decision 6).
//!
//! Each `[[webhooks]]` row of an instance gets one random URL token, kept in
//! `plugin_webhooks` sealed with the library key beside its SHA-256, which
//! the public route looks the webhook up by. A verified request is stored
//! in `plugin_webhook_deliveries`, its kept headers (JSON) and body in
//! `plugin_webhook_chunks` rows sized from the database plugin's limits;
//! the delivery row goes in the batch with the last chunk, so a
//! half-written request is never leased.
//!
//! Leasing is compare-and-set on each row's lease id and visibility time,
//! so two pulls racing on one instance never hand out the same request. A
//! request that outlives its retention, or arrives while the instance
//! already holds [`MAX_WAITING_WEBHOOK_DELIVERIES`], is dropped and the
//! webhook's `missed` flag set; the next pull reports and clears it.

use base64::Engine as _;
use bookclerk_plugin_abi::{
    DbRow, DbValue, ExecuteReply, TypedDbStatement, WebhookDelivery, WebhookHeader, WebhookPullPage,
};

use super::artifacts::{bytes_cell, chunk_bytes, int_cell, read_width, ChunkBatches};
use super::batch;
use super::instances::PluginInstanceId;
use crate::error::{LibraryError, Result};
use crate::master_key::{require_master_key, seal_with_dek, unseal_with_dek};
use crate::store::LibraryStore;

/// Most requests one plugin instance may have waiting, across its webhooks.
/// Past this the route answers 503 and the next pull reports `missed`.
pub const MAX_WAITING_WEBHOOK_DELIVERIES: u32 = 10_000;

/// Most request body bytes one pull returns, summed over its deliveries;
/// a pull always returns at least one waiting delivery.
pub const MAX_WEBHOOK_PULL_PAGE_BYTES: usize = 4 * 1024 * 1024;

/// Most lease ids one statement names, kept under the smallest advertised
/// bind limit with room for the instance and name.
const LEASES_PER_STATEMENT: usize = 32;

/// Largest kept-headers JSON a stored row may claim: every header at its
/// longest name and value, with JSON quoting, plus framing.
const MAX_STORED_HEADER_BYTES: usize = (bookclerk_plugin_abi::MAX_WEBHOOK_HEADERS as usize + 1)
    * (64 + 2 * bookclerk_plugin_abi::MAX_WEBHOOK_HEADER_VALUE_BYTES as usize + 32)
    + 2;

/// Random bytes in a URL token (256 bits).
const TOKEN_BYTES: usize = 32;

/// Random bytes in a lease id (128 bits).
const LEASE_BYTES: usize = 16;

/// A webhook URL token: 43 characters of unpadded base64url.
///
/// The value is a capability: anyone holding it can post to the webhook,
/// so it is never logged and `Debug` shows only its length.
#[derive(Clone, PartialEq, Eq)]
pub struct WebhookUrlToken(String);

impl WebhookUrlToken {
    /// Mints a new random token.
    #[must_use]
    fn mint() -> Self {
        let bytes: [u8; TOKEN_BYTES] = rand::random();
        Self(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Parses a token from a URL path segment.
    ///
    /// # Returns
    ///
    /// `None` unless `raw` is exactly 43 base64url characters.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let ok = raw.len() == 43
            && raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        ok.then(|| Self(raw.to_string()))
    }

    /// The token text, as it goes in the URL.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Lowercase hex SHA-256 of the token, the lookup key.
    #[must_use]
    fn hash(&self) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(self.0.as_bytes()))
    }
}

impl std::fmt::Debug for WebhookUrlToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WebhookUrlToken(<{} chars>)", self.0.len())
    }
}

/// The webhook a URL token names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookTarget {
    /// Owning plugin instance.
    pub instance_id: PluginInstanceId,
    /// `[[webhooks]]` name.
    pub name: String,
}

/// One verified request to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookRequest {
    /// Kept headers, lowercase names, in the order the row lists them.
    pub headers: Vec<WebhookHeader>,
    /// Request body, already within the row's cap.
    pub body: Vec<u8>,
    /// When the host received it, in Unix milliseconds.
    pub received_at_ms: i64,
    /// How long it may wait unacknowledged, in milliseconds.
    pub retention_ms: i64,
}

/// What [`store_webhook_request`] did with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookStoreOutcome {
    /// Stored; this many requests now wait on the instance.
    Stored {
        /// Requests waiting on the instance, counting this one.
        waiting: u32,
    },
    /// The instance already holds [`MAX_WAITING_WEBHOOK_DELIVERIES`]; the
    /// request was dropped and the webhook marked missed.
    Full,
}

/// Runs one webhook batch, letting the in-process fallback return up to
/// `max_rows` rows per statement.
///
/// # Errors
///
/// Returns the database error; nothing in the batch is kept then.
async fn run(
    store: &LibraryStore,
    statements: Vec<TypedDbStatement>,
    max_rows: u32,
) -> Result<ExecuteReply> {
    let req = batch::request(
        &format!("plugin-webhook-{}", uuid::Uuid::new_v4()),
        statements,
    );
    store.execute_host_batch_limited(req, max_rows).await
}

/// Text cell at `index`.
///
/// # Errors
///
/// Returns an error when the cell is missing or not text.
fn text_cell(row: &DbRow, index: usize) -> Result<&str> {
    match row.values.get(index) {
        Some(DbValue::Text(value)) => Ok(value),
        other => Err(LibraryError::Other(anyhow::anyhow!(
            "expected a text webhook cell {index}, got {other:?}"
        ))),
    }
}

/// Error for a stored row that does not decode.
fn corrupt(what: &str) -> LibraryError {
    LibraryError::Other(anyhow::anyhow!("stored webhook data is corrupt: {what}"))
}

/// Standard base64 of `bytes`.
fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decodes standard base64.
///
/// # Errors
///
/// Returns a corrupt-data error when `text` is not base64.
fn unb64(text: &str, what: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|_| corrupt(what))
}

/// The URL token of one webhook, minting it on first use.
///
/// # Arguments
///
/// * `store` - Library store.
/// * `instance_id` - Owning plugin instance.
/// * `name` - `[[webhooks]]` name, already validated.
///
/// # Errors
///
/// Returns an error when the library key is unavailable, a read or write
/// fails, or the stored token does not unseal.
pub async fn webhook_url_token(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
    name: &str,
) -> Result<WebhookUrlToken> {
    let select = || {
        batch::query(
            "SELECT token_sealed, token_nonce FROM plugin_webhooks \
             WHERE instance_id = ? AND name = ?",
            vec![batch::text(instance_id.as_str()), batch::text(name)],
            1,
        )
    };
    let dek = require_master_key(None)?;
    let reply = run(store, vec![select()], 1).await?;
    let row = if let Some(row) = batch::rows(&reply, 0).first() {
        row.clone()
    } else {
        let token = WebhookUrlToken::mint();
        let (sealed, nonce) = seal_with_dek(token.as_str().as_bytes(), &dek)?;
        // Two nodes minting at once both insert-or-ignore; the select in
        // the same batch returns whichever token won.
        let reply = run(
            store,
            vec![
                batch::exec(
                    "INSERT OR IGNORE INTO plugin_webhooks \
                     (instance_id, name, token_hash, token_sealed, token_nonce, missed, created_at) \
                     VALUES (?, ?, ?, ?, ?, 0, ?)",
                    vec![
                        batch::text(instance_id.as_str()),
                        batch::text(name),
                        batch::text(&token.hash()),
                        batch::text(&b64(&sealed)),
                        batch::text(&b64(&nonce)),
                        batch::text(&chrono::Utc::now().to_rfc3339()),
                    ],
                ),
                select(),
            ],
            1,
        )
        .await?;
        batch::rows(&reply, 1)
            .first()
            .cloned()
            .ok_or_else(|| corrupt("the minted token was not stored"))?
    };
    let sealed = unb64(text_cell(&row, 0)?, "token")?;
    let nonce = unb64(text_cell(&row, 1)?, "token nonce")?;
    let plain = unseal_with_dek(&sealed, &nonce, &dek)?;
    let text = String::from_utf8(plain).map_err(|_| corrupt("token"))?;
    WebhookUrlToken::parse(&text).ok_or_else(|| corrupt("token"))
}

/// The webhook a URL token names, if any.
///
/// # Errors
///
/// Returns an error when the read fails or the stored instance id is
/// malformed.
pub async fn find_webhook(
    store: &LibraryStore,
    token: &WebhookUrlToken,
) -> Result<Option<WebhookTarget>> {
    let reply = run(
        store,
        vec![batch::query(
            "SELECT instance_id, name FROM plugin_webhooks WHERE token_hash = ?",
            vec![batch::text(&token.hash())],
            1,
        )],
        1,
    )
    .await?;
    let Some(row) = batch::rows(&reply, 0).first() else {
        return Ok(None);
    };
    Ok(Some(WebhookTarget {
        instance_id: PluginInstanceId::parse(text_cell(row, 0)?)?,
        name: text_cell(row, 1)?.to_string(),
    }))
}

/// Statements that drop one webhook's expired requests and their chunks
/// (including chunk sets an unfinished write left), marking the webhook
/// missed when a request was dropped. They run as one batch.
fn prune_statements(instance_id: &str, name: &str, now_ms: i64) -> Vec<TypedDbStatement> {
    let owner = || vec![batch::text(instance_id), batch::text(name)];
    let with_now = |mut params: Vec<DbValue>| {
        params.push(batch::int(now_ms));
        params
    };
    let mut missed_params = owner();
    missed_params.extend(with_now(owner()));
    vec![
        batch::exec(
            "UPDATE plugin_webhooks SET missed = 1 WHERE instance_id = ? AND name = ? AND EXISTS \
             (SELECT 1 FROM plugin_webhook_deliveries \
              WHERE instance_id = ? AND name = ? AND expires_at_ms <= ?)",
            missed_params,
        ),
        batch::exec(
            "DELETE FROM plugin_webhook_chunks \
             WHERE instance_id = ? AND name = ? AND expires_at_ms <= ?",
            with_now(owner()),
        ),
        batch::exec(
            "DELETE FROM plugin_webhook_deliveries \
             WHERE instance_id = ? AND name = ? AND expires_at_ms <= ?",
            with_now(owner()),
        ),
    ]
}

/// Stores one verified request.
///
/// Expired requests of the webhook are pruned first. When the instance
/// already holds [`MAX_WAITING_WEBHOOK_DELIVERIES`] the request is dropped
/// and the webhook marked missed.
///
/// # Errors
///
/// Returns an error when a write fails. Chunks from batches written before
/// the failure stay invisible and are pruned when they expire.
pub async fn store_webhook_request(
    store: &LibraryStore,
    target: &WebhookTarget,
    request: &WebhookRequest,
) -> Result<WebhookStoreOutcome> {
    let instance = target.instance_id.as_str();
    let name = target.name.as_str();
    let mut statements = prune_statements(instance, name, request.received_at_ms);
    statements.push(batch::query(
        "SELECT COUNT(*) FROM plugin_webhook_deliveries WHERE instance_id = ?",
        vec![batch::text(instance)],
        1,
    ));
    let reply = run(store, statements, 1).await?;
    let waiting = batch::rows(&reply, 3)
        .first()
        .map(|row| int_cell(row, 0))
        .transpose()?
        .unwrap_or(0);
    let waiting = u32::try_from(waiting).unwrap_or(u32::MAX);
    if waiting >= MAX_WAITING_WEBHOOK_DELIVERIES {
        run(
            store,
            vec![batch::exec(
                "UPDATE plugin_webhooks SET missed = 1 WHERE instance_id = ? AND name = ?",
                vec![batch::text(instance), batch::text(name)],
            )],
            0,
        )
        .await?;
        return Ok(WebhookStoreOutcome::Full);
    }
    let caps = store.db_capabilities();
    let chunk = chunk_bytes(caps);
    let overflow = |what: &str| corrupt(&format!("{what} overflows"));
    let chunk_i64 = i64::try_from(chunk).map_err(|_| overflow("chunk size"))?;
    let headers = serde_json::to_vec(&request.headers)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!(err)))?;
    let headers_i64 = i64::try_from(headers.len()).map_err(|_| overflow("header size"))?;
    let size_i64 = i64::try_from(request.body.len()).map_err(|_| overflow("body size"))?;
    let mut payload = headers;
    payload.extend_from_slice(&request.body);
    let expires = request.received_at_ms.saturating_add(request.retention_ms);
    let delivery_id = uuid::Uuid::new_v4().hyphenated().to_string();
    let mut batches = ChunkBatches::new(caps)?;
    for (seq, part) in payload.chunks(chunk).enumerate() {
        let seq = i64::try_from(seq).map_err(|_| overflow("chunk count"))?;
        let stmt = batch::exec(
            "INSERT INTO plugin_webhook_chunks \
             (delivery_id, seq, instance_id, name, expires_at_ms, data) VALUES (?, ?, ?, ?, ?, ?)",
            vec![
                batch::text(&delivery_id),
                batch::int(seq),
                batch::text(instance),
                batch::text(name),
                batch::int(expires),
                batch::blob(part),
            ],
        );
        if let Some(full) = batches.push(stmt)? {
            run(store, full, 0).await?;
        }
    }
    let header = batch::exec(
        "INSERT INTO plugin_webhook_deliveries \
         (delivery_id, instance_id, name, received_at_ms, expires_at_ms, visible_at_ms, \
          attempt, lease_id, headers_bytes, body_bytes, chunk_bytes) \
         VALUES (?, ?, ?, ?, ?, 0, 0, '', ?, ?, ?)",
        vec![
            batch::text(&delivery_id),
            batch::text(instance),
            batch::text(name),
            batch::int(request.received_at_ms),
            batch::int(expires),
            batch::int(headers_i64),
            batch::int(size_i64),
            batch::int(chunk_i64),
        ],
    );
    if let Some(full) = batches.push(header)? {
        run(store, full, 0).await?;
    }
    if let Some(last) = batches.finish() {
        run(store, last, 0).await?;
    }
    Ok(WebhookStoreOutcome::Stored {
        waiting: waiting.saturating_add(1),
    })
}

/// One waiting delivery as a pull selects it.
struct Candidate {
    /// Delivery id.
    delivery_id: String,
    /// Unix milliseconds the host stored it.
    received_at_ms: i64,
    /// Pulls so far.
    attempt: i64,
    /// Lease id it carries now.
    lease_id: String,
    /// Visibility time it carries now.
    visible_at_ms: i64,
    /// Length of the kept headers' JSON, which leads the stored payload.
    headers_bytes: usize,
    /// Body length in bytes.
    body_bytes: usize,
    /// Chunk size its body was written with.
    chunk_bytes: usize,
}

impl Candidate {
    /// Reads one selected row.
    ///
    /// # Errors
    ///
    /// Returns a corrupt-data error for a malformed row.
    fn from_row(row: &DbRow) -> Result<Self> {
        let size = |index: usize, what: &str| -> Result<usize> {
            usize::try_from(int_cell(row, index)?).map_err(|_| corrupt(what))
        };
        let candidate = Self {
            delivery_id: text_cell(row, 0)?.to_string(),
            received_at_ms: int_cell(row, 1)?,
            attempt: int_cell(row, 2)?,
            lease_id: text_cell(row, 3)?.to_string(),
            visible_at_ms: int_cell(row, 4)?,
            headers_bytes: size(5, "header size")?,
            body_bytes: size(6, "body size")?,
            chunk_bytes: size(7, "chunk size")?,
        };
        if candidate.chunk_bytes == 0
            || candidate.body_bytes > bookclerk_plugin_abi::MAX_WEBHOOK_BODY_BYTES as usize
            || candidate.headers_bytes > MAX_STORED_HEADER_BYTES
        {
            return Err(corrupt("delivery header"));
        }
        Ok(candidate)
    }
}

/// Reads one delivery's stored payload (headers JSON, then body) from its
/// chunks.
///
/// # Errors
///
/// Returns an error when a read fails or a chunk is missing or the wrong
/// size.
async fn load_payload(store: &LibraryStore, candidate: &Candidate) -> Result<Vec<u8>> {
    let size = candidate
        .headers_bytes
        .checked_add(candidate.body_bytes)
        .ok_or_else(|| corrupt("payload size"))?;
    let chunk = candidate.chunk_bytes;
    let count = size.div_ceil(chunk);
    let width = read_width(store.db_capabilities(), chunk);
    let mut body = Vec::with_capacity(size);
    let mut next = 0usize;
    while next < count {
        let from = i64::try_from(next).map_err(|_| corrupt("chunk count"))?;
        let reply = run(
            store,
            vec![batch::query(
                "SELECT seq, data FROM plugin_webhook_chunks \
                 WHERE delivery_id = ? AND seq >= ? ORDER BY seq LIMIT ?",
                vec![
                    batch::text(&candidate.delivery_id),
                    batch::int(from),
                    batch::int(i64::from(width)),
                ],
                width,
            )],
            width,
        )
        .await?;
        let rows = batch::rows(&reply, 0);
        if rows.is_empty() {
            return Err(corrupt("a body chunk is missing"));
        }
        for row in rows {
            if next >= count {
                break;
            }
            if usize::try_from(int_cell(row, 0)?).ok() != Some(next) {
                return Err(corrupt("a body chunk is missing"));
            }
            let data = bytes_cell(row, 1)?;
            if data.len() != chunk.min(size - body.len()) {
                return Err(corrupt("a body chunk is the wrong size"));
            }
            body.extend_from_slice(data);
            next += 1;
        }
    }
    Ok(body)
}

/// Leases up to `batch_size` waiting requests of one webhook, oldest first.
///
/// Expired requests are pruned first. The page stops before its bodies
/// would pass [`MAX_WEBHOOK_PULL_PAGE_BYTES`], but always holds at least one
/// request when any waits. `missed` reports, and clears, whether a request
/// was dropped since the last pull.
///
/// # Arguments
///
/// * `store` - Library store.
/// * `instance_id` - Owning plugin instance.
/// * `name` - `[[webhooks]]` name, already validated.
/// * `batch_size` - Most requests to lease, already within the ABI cap.
/// * `visibility_ms` - Lease length in milliseconds.
/// * `now_ms` - Current time in Unix milliseconds.
///
/// # Errors
///
/// Returns an error when a read or write fails or a stored row is corrupt.
pub async fn pull_webhook_requests(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
    name: &str,
    batch_size: u32,
    visibility_ms: u32,
    now_ms: i64,
) -> Result<WebhookPullPage> {
    let instance = instance_id.as_str();
    let row_cap = store.db_capabilities().max_result_rows;
    let batch_size = if row_cap == 0 {
        batch_size
    } else {
        batch_size.min(row_cap)
    }
    .max(1);
    let mut statements = prune_statements(instance, name, now_ms);
    statements.push(batch::query(
        "SELECT delivery_id, received_at_ms, attempt, lease_id, visible_at_ms, headers_bytes, \
         body_bytes, chunk_bytes FROM plugin_webhook_deliveries \
         WHERE instance_id = ? AND name = ? AND visible_at_ms <= ? AND expires_at_ms > ? \
         ORDER BY received_at_ms, delivery_id LIMIT ?",
        vec![
            batch::text(instance),
            batch::text(name),
            batch::int(now_ms),
            batch::int(now_ms),
            batch::int(i64::from(batch_size)),
        ],
        batch_size,
    ));
    let reply = run(store, statements, batch_size).await?;
    let mut chosen = Vec::new();
    let mut bytes = 0usize;
    for row in batch::rows(&reply, 3) {
        let candidate = Candidate::from_row(row)?;
        if !chosen.is_empty()
            && bytes.saturating_add(candidate.body_bytes) > MAX_WEBHOOK_PULL_PAGE_BYTES
        {
            break;
        }
        bytes = bytes.saturating_add(candidate.body_bytes);
        chosen.push(candidate);
    }
    let visible_at = now_ms.saturating_add(i64::from(visibility_ms));
    let leases: Vec<String> = chosen
        .iter()
        .map(|_| {
            let raw: [u8; LEASE_BYTES] = rand::random();
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
        })
        .collect();
    let mut statements: Vec<TypedDbStatement> = chosen
        .iter()
        .zip(&leases)
        .map(|(candidate, lease)| {
            batch::exec(
                "UPDATE plugin_webhook_deliveries \
                 SET lease_id = ?, visible_at_ms = ?, attempt = attempt + 1 \
                 WHERE delivery_id = ? AND lease_id = ? AND visible_at_ms = ?",
                vec![
                    batch::text(lease),
                    batch::int(visible_at),
                    batch::text(&candidate.delivery_id),
                    batch::text(&candidate.lease_id),
                    batch::int(candidate.visible_at_ms),
                ],
            )
        })
        .collect();
    statements.push(batch::exec(
        "UPDATE plugin_webhooks SET missed = 0 WHERE instance_id = ? AND name = ? AND missed = 1",
        vec![batch::text(instance), batch::text(name)],
    ));
    let count = chosen.len();
    let reply = run(store, statements, 0).await?;
    let missed = batch::rows_affected(&reply, count) > 0;
    let mut deliveries = Vec::with_capacity(count);
    for (index, (candidate, lease)) in chosen.into_iter().zip(leases).enumerate() {
        // Another pull leased this one first.
        if batch::rows_affected(&reply, index) == 0 {
            continue;
        }
        let mut body = load_payload(store, &candidate).await?;
        let rest = body.split_off(candidate.headers_bytes);
        let headers: Vec<WebhookHeader> =
            serde_json::from_slice(&body).map_err(|_| corrupt("headers"))?;
        let body = rest;
        deliveries.push(WebhookDelivery {
            lease_id: lease,
            received_at_unix_ms: u64::try_from(candidate.received_at_ms).unwrap_or(0),
            attempt: u32::try_from(candidate.attempt.saturating_add(1)).unwrap_or(u32::MAX),
            headers,
            body,
        });
    }
    Ok(WebhookPullPage { deliveries, missed })
}

/// `?, ?, ...` with `count` placeholders.
fn placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

/// Finishes leased requests of one webhook: `acks` are deleted with their
/// chunks, `retries` become visible to the next pull at once.
///
/// Unknown or stale lease ids are ignored. The lists may land in separate
/// batches; a retry that does not land is offered again when its lease
/// runs out anyway.
///
/// # Errors
///
/// Returns an error when a write fails.
pub async fn ack_webhook_requests(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
    name: &str,
    acks: &[String],
    retries: &[String],
) -> Result<()> {
    let instance = instance_id.as_str();
    let caps = store.db_capabilities();
    let mut batches = ChunkBatches::new(caps)?;
    let params = |group: &[String]| {
        let mut params = vec![batch::text(instance), batch::text(name)];
        params.extend(group.iter().map(|lease| batch::text(lease)));
        params
    };
    let mut statements = Vec::new();
    for group in acks.chunks(LEASES_PER_STATEMENT) {
        let marks = placeholders(group.len());
        statements.push(batch::exec(
            &format!(
                "DELETE FROM plugin_webhook_chunks WHERE delivery_id IN \
                 (SELECT delivery_id FROM plugin_webhook_deliveries \
                  WHERE instance_id = ? AND name = ? AND lease_id IN ({marks}))"
            ),
            params(group),
        ));
        statements.push(batch::exec(
            &format!(
                "DELETE FROM plugin_webhook_deliveries \
                 WHERE instance_id = ? AND name = ? AND lease_id IN ({marks})"
            ),
            params(group),
        ));
    }
    for group in retries.chunks(LEASES_PER_STATEMENT) {
        let marks = placeholders(group.len());
        statements.push(batch::exec(
            &format!(
                "UPDATE plugin_webhook_deliveries SET visible_at_ms = 0 \
                 WHERE instance_id = ? AND name = ? AND lease_id IN ({marks})"
            ),
            params(group),
        ));
    }
    for stmt in statements {
        if let Some(full) = batches.push(stmt)? {
            run(store, full, 0).await?;
        }
    }
    if let Some(last) = batches.finish() {
        run(store, last, 0).await?;
    }
    Ok(())
}

/// Requests waiting on one webhook now (leased or not).
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn waiting_webhook_requests(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
    name: &str,
) -> Result<u32> {
    let reply = run(
        store,
        vec![batch::query(
            "SELECT COUNT(*) FROM plugin_webhook_deliveries WHERE instance_id = ? AND name = ?",
            vec![batch::text(instance_id.as_str()), batch::text(name)],
            1,
        )],
        1,
    )
    .await?;
    let count = batch::rows(&reply, 0)
        .first()
        .map(|row| int_cell(row, 0))
        .transpose()?
        .unwrap_or(0);
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

/// Deletes every webhook of one instance, with its token and stored
/// requests, as one batch. Called when the instance is deleted, so its
/// URLs stop answering.
///
/// # Errors
///
/// Returns an error when the write fails.
pub async fn delete_instance_webhooks(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
) -> Result<()> {
    let id = || vec![batch::text(instance_id.as_str())];
    run(
        store,
        vec![
            batch::exec(
                "DELETE FROM plugin_webhook_chunks WHERE instance_id = ?",
                id(),
            ),
            batch::exec(
                "DELETE FROM plugin_webhook_deliveries WHERE instance_id = ?",
                id(),
            ),
            batch::exec("DELETE FROM plugin_webhooks WHERE instance_id = ?", id()),
        ],
        0,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "webhooks_tests.rs"]
mod tests;
