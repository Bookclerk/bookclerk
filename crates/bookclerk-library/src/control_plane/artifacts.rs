//! Plugin version archives stored in the library (`blob:sha256:<hex>`).
//!
//! A local archive with no shared location is kept in the database so every
//! node can materialize it. One archive is larger than a database plugin
//! lets one cell, bind, statement or result carry (256 KiB on the shipped
//! adapters, 1 KiB at the host minimum), so the bytes are split into
//! `plugin_artifact_chunks` rows sized from the plugin's advertised limits
//! and the `plugin_artifacts` header row is written after the last chunk:
//! a header means the archive is complete.
//!
//! Chunks are keyed by `(sha256, chunk_bytes, seq)`. Writers on the same
//! database pick the same chunk size, so two concurrent writes of one
//! archive insert identical rows and either header wins. A write left
//! unfinished under a different size (the plugin's limits changed) cannot
//! mix into the new set; the batch that stores the header deletes every
//! chunk set the header does not name.

use bookclerk_plugin_abi::{
    DbCapabilities, DbRow, DbValue, ExecuteRequest, TypedDbStatement, MAX_SCALAR_BYTES,
};
use chrono::Utc;

use super::batch;
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

use super::versions::ARTIFACT_BLOB_SCHEME;

/// Largest archive stored in the library, in bytes.
///
/// A larger local archive is referenced in place as a `file://` URL.
pub const MAX_ARTIFACT_BLOB_BYTES: usize = 1_048_576;

/// Bytes of a statement-, result- or request-wide cap one chunk leaves for
/// everything else in it: the SQL text, the digest and integer binds, the
/// row framing of a result, and the operation id of a request.
const CHUNK_HEADROOM: usize = 1_024;

/// Share of the plugin's `maxRequestBytes`, in eighths, that a batch of
/// several chunk statements fills. The batch is summed from each
/// statement's own encoding, so the margin covers the framing that summing
/// leaves out.
const BATCH_BYTES_EIGHTHS: usize = 7;

/// Bytes per chunk under `caps`.
///
/// The smallest of `maxCellBytes` (a chunk is read back as one cell) and
/// `maxPayloadBytes`, `maxResultBytes`, `maxAtomicResultBytes`,
/// `maxRequestBytes`, `maxLoweredStatementBytes` and the ABI's scalar cap
/// less [`CHUNK_HEADROOM`], then cut to the share whose base64 form fits:
/// an adapter may carry or count a blob as base64 text (the shipped sqlite
/// adapter charges a blob cell its JSON length, and D1 crosses JSON). A cap
/// of `0` is unlimited. Never below one byte.
fn chunk_bytes(caps: &DbCapabilities) -> usize {
    let less = |cap: u32, headroom: usize| -> usize {
        if cap == 0 {
            usize::MAX
        } else {
            usize::try_from(cap)
                .unwrap_or(usize::MAX)
                .saturating_sub(headroom)
        }
    };
    [
        less(caps.max_cell_bytes, 0),
        less(caps.max_payload_bytes, CHUNK_HEADROOM),
        less(caps.max_result_bytes, CHUNK_HEADROOM),
        less(caps.max_atomic_result_bytes, CHUNK_HEADROOM),
        less(caps.max_request_bytes, CHUNK_HEADROOM),
        less(caps.max_lowered_statement_bytes, CHUNK_HEADROOM),
        less(MAX_SCALAR_BYTES, CHUNK_HEADROOM),
    ]
    .into_iter()
    .min()
    .map_or(1, |cap| cap / 4 * 3)
    .max(1)
}

/// Chunks one read asks for under `caps` when each is `chunk` bytes.
///
/// Bounded by `maxResultRows` and by how many chunks, counted in base64
/// plus [`CHUNK_HEADROOM`] each, fit `maxResultBytes` and
/// `maxAtomicResultBytes`. Never below one row.
fn read_width(caps: &DbCapabilities, chunk: usize) -> u32 {
    let per_row = chunk
        .div_ceil(3)
        .saturating_mul(4)
        .saturating_add(CHUNK_HEADROOM);
    let fit = |cap: u32| -> usize {
        if cap == 0 {
            usize::MAX
        } else {
            usize::try_from(cap).unwrap_or(usize::MAX) / per_row
        }
    };
    let rows = if caps.max_result_rows == 0 {
        usize::MAX
    } else {
        usize::try_from(caps.max_result_rows).unwrap_or(usize::MAX)
    };
    let width = rows
        .min(fit(caps.max_result_bytes))
        .min(fit(caps.max_atomic_result_bytes))
        .max(1);
    u32::try_from(width).unwrap_or(u32::MAX)
}

/// Lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// Encoded bytes of a request carrying `statements`.
///
/// # Errors
///
/// Returns an error when the statements cannot be encoded.
fn encoded_len(statements: &[TypedDbStatement]) -> Result<usize> {
    let req = batch::request(
        &format!("plugin-artifact-{}", uuid::Uuid::nil()),
        statements.to_vec(),
    );
    bookclerk_plugin_abi::encoded_execute_request_bytes(&req)
        .map(|bytes| bytes.len())
        .map_err(|err| LibraryError::Other(anyhow::anyhow!(err.to_string())))
}

/// Splits archive writes into atomic batches the database plugin accepts.
///
/// A batch closes before it would pass `maxStatements` or
/// [`BATCH_BYTES_EIGHTHS`] of `maxRequestBytes`; a statement that cannot
/// share a batch goes alone while it fits the whole request cap.
struct ChunkBatches {
    /// Statements waiting for the next batch.
    pending: Vec<TypedDbStatement>,
    /// Summed encoded bytes of `pending` plus an empty request's framing.
    pending_bytes: usize,
    /// Encoded bytes of a request with no statements.
    empty_bytes: usize,
    /// Most statements one batch may hold.
    max_statements: usize,
    /// Most encoded bytes one batch of several statements may hold.
    max_bytes: usize,
    /// Most encoded bytes a request holding one statement may hold.
    max_single_bytes: usize,
}

impl ChunkBatches {
    /// Starts an empty batch sized to `caps`.
    ///
    /// # Errors
    ///
    /// Returns an error when an empty request cannot be encoded.
    fn new(caps: &DbCapabilities) -> Result<Self> {
        let empty_bytes = encoded_len(&[])?;
        let request_cap = if caps.max_request_bytes == 0 {
            usize::MAX
        } else {
            usize::try_from(caps.max_request_bytes).unwrap_or(usize::MAX)
        };
        let statement_cap = if caps.max_statements == 0 {
            usize::MAX
        } else {
            usize::try_from(caps.max_statements).unwrap_or(usize::MAX)
        };
        Ok(Self {
            pending: Vec::new(),
            pending_bytes: empty_bytes,
            empty_bytes,
            max_statements: statement_cap.max(1),
            max_bytes: request_cap / 8 * BATCH_BYTES_EIGHTHS,
            max_single_bytes: request_cap.saturating_sub(CHUNK_HEADROOM / 4),
        })
    }

    /// Adds one statement.
    ///
    /// # Returns
    ///
    /// The batch that had to close to make room, when one did.
    ///
    /// # Errors
    ///
    /// Returns an error when the statement alone is larger than one request
    /// may carry, or cannot be encoded.
    fn push(&mut self, stmt: TypedDbStatement) -> Result<Option<Vec<TypedDbStatement>>> {
        let size = encoded_len(std::slice::from_ref(&stmt))?.saturating_sub(self.empty_bytes);
        if self.empty_bytes.saturating_add(size) > self.max_single_bytes {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "a plugin archive write is {size} bytes, over the database plugin's request limit"
            )));
        }
        let full = self.pending.len() >= self.max_statements
            || self.pending_bytes.saturating_add(size) > self.max_bytes;
        let closed = if full && !self.pending.is_empty() {
            self.pending_bytes = self.empty_bytes;
            Some(std::mem::take(&mut self.pending))
        } else {
            None
        };
        self.pending_bytes = self.pending_bytes.saturating_add(size);
        self.pending.push(stmt);
        Ok(closed)
    }

    /// The last, partly filled batch, when anything is left.
    fn finish(self) -> Option<Vec<TypedDbStatement>> {
        (!self.pending.is_empty()).then_some(self.pending)
    }
}

/// Runs one archive batch, letting the in-process fallback return up to
/// `max_rows` rows per statement.
///
/// # Errors
///
/// Returns the database error; nothing in the batch is kept then.
async fn run(
    store: &LibraryStore,
    statements: Vec<TypedDbStatement>,
    max_rows: u32,
) -> Result<bookclerk_plugin_abi::ExecuteReply> {
    let req: ExecuteRequest = batch::request(
        &format!("plugin-artifact-{}", uuid::Uuid::new_v4()),
        statements,
    );
    store.execute_host_batch_limited(req, max_rows).await
}

/// Integer cell at `index`.
///
/// # Errors
///
/// Returns an error when the cell is missing or not an integer.
fn int_cell(row: &DbRow, index: usize) -> Result<i64> {
    match row.values.get(index) {
        Some(DbValue::Int64(value)) => Ok(*value),
        other => Err(LibraryError::Other(anyhow::anyhow!(
            "expected an integer plugin archive cell {index}, got {other:?}"
        ))),
    }
}

/// Blob cell at `index`.
///
/// # Errors
///
/// Returns an error when the cell is missing or not a blob.
fn bytes_cell(row: &DbRow, index: usize) -> Result<&[u8]> {
    match row.values.get(index) {
        Some(DbValue::Bytes(value)) => Ok(value),
        other => Err(LibraryError::Other(anyhow::anyhow!(
            "expected a blob plugin archive cell {index}, got {other:?}"
        ))),
    }
}

/// The stored header of archive `sha256`: its size and chunk size.
///
/// # Errors
///
/// Returns the database error, or an error when the row is malformed.
async fn load_header(store: &LibraryStore, sha256: &str) -> Result<Option<(usize, usize)>> {
    let reply = run(
        store,
        vec![batch::query(
            "SELECT size, chunk_bytes FROM plugin_artifacts WHERE sha256 = ?",
            vec![batch::text(sha256)],
            1,
        )],
        1,
    )
    .await?;
    let Some(row) = batch::rows(&reply, 0).first() else {
        return Ok(None);
    };
    let malformed = || {
        LibraryError::Other(anyhow::anyhow!(
            "plugin archive {sha256} has a malformed header"
        ))
    };
    let size = usize::try_from(int_cell(row, 0)?).map_err(|_| malformed())?;
    let chunk = usize::try_from(int_cell(row, 1)?).map_err(|_| malformed())?;
    if chunk == 0 || size > MAX_ARTIFACT_BLOB_BYTES {
        return Err(malformed());
    }
    Ok(Some((size, chunk)))
}

/// Stores a small archive and returns its `blob:sha256:<hex>` URL.
///
/// The bytes go in chunks sized from the database plugin's advertised
/// limits, over as many atomic batches as those limits need; the header row
/// goes in the last one. Storing an archive that is already stored writes
/// nothing.
///
/// # Errors
///
/// Returns an error when the archive exceeds [`MAX_ARTIFACT_BLOB_BYTES`] or
/// a write fails. Chunks from batches written before the failure stay but
/// are invisible without their header, and a retry reuses them.
pub async fn put_artifact_blob(store: &LibraryStore, bytes: &[u8]) -> Result<String> {
    if bytes.len() > MAX_ARTIFACT_BLOB_BYTES {
        return Err(LibraryError::Invalid(format!(
            "archive of {} bytes exceeds the {MAX_ARTIFACT_BLOB_BYTES}-byte library blob cap; \
             install it from a shared location instead",
            bytes.len()
        )));
    }
    let sha = sha256_hex(bytes);
    let url = format!("{ARTIFACT_BLOB_SCHEME}{sha}");
    if load_header(store, &sha).await?.is_some() {
        return Ok(url);
    }
    let caps = store.db_capabilities();
    let chunk = chunk_bytes(caps);
    let too_big =
        |what: &str| LibraryError::Other(anyhow::anyhow!("plugin archive {what} overflows"));
    let chunk_i64 = i64::try_from(chunk).map_err(|_| too_big("chunk size"))?;
    let size_i64 = i64::try_from(bytes.len()).map_err(|_| too_big("size"))?;
    let mut batches = ChunkBatches::new(caps)?;
    for (seq, part) in bytes.chunks(chunk).enumerate() {
        let seq = i64::try_from(seq).map_err(|_| too_big("chunk count"))?;
        let stmt = batch::exec(
            "INSERT OR IGNORE INTO plugin_artifact_chunks (sha256, chunk_bytes, seq, data) \
             VALUES (?, ?, ?, ?)",
            vec![
                batch::text(&sha),
                batch::int(chunk_i64),
                batch::int(seq),
                batch::blob(part),
            ],
        );
        if let Some(full) = batches.push(stmt)? {
            run(store, full, 0).await?;
        }
    }
    let header = batch::exec(
        "INSERT OR IGNORE INTO plugin_artifacts (sha256, size, chunk_bytes, created_at) \
         VALUES (?, ?, ?, ?)",
        vec![
            batch::text(&sha),
            batch::int(size_i64),
            batch::int(chunk_i64),
            batch::text(&Utc::now().to_rfc3339()),
        ],
    );
    // Once a header exists, only the chunk set it names is kept: this
    // clears a set an unfinished write left under another chunk size.
    let stale = batch::exec(
        "DELETE FROM plugin_artifact_chunks WHERE sha256 = ? AND chunk_bytes NOT IN \
         (SELECT chunk_bytes FROM plugin_artifacts WHERE sha256 = ?)",
        vec![batch::text(&sha), batch::text(&sha)],
    );
    // The header and the cleanup share the last batch: a batch that closes
    // to make room for one of them carries only chunks.
    for stmt in [header, stale] {
        if let Some(full) = batches.push(stmt)? {
            run(store, full, 0).await?;
        }
    }
    if let Some(last) = batches.finish() {
        run(store, last, 0).await?;
    }
    Ok(url)
}

/// Reads an archive stored by [`put_artifact_blob`].
///
/// Chunks are read in order, as many per request as the database plugin's
/// result limits allow.
///
/// # Returns
///
/// The archive bytes, or `None` when no archive with that digest is stored.
///
/// # Errors
///
/// Returns an error when a read fails, a chunk is missing or the wrong
/// size, or the stored bytes do not match their digest.
pub async fn load_artifact_blob(store: &LibraryStore, sha256: &str) -> Result<Option<Vec<u8>>> {
    let Some((size, chunk)) = load_header(store, sha256).await? else {
        return Ok(None);
    };
    let corrupt =
        |why: &str| LibraryError::Other(anyhow::anyhow!("version blob {sha256} is corrupt: {why}"));
    let count = size.div_ceil(chunk);
    let chunk_i64 = i64::try_from(chunk).map_err(|_| corrupt("chunk size overflows"))?;
    let width = read_width(store.db_capabilities(), chunk);
    let mut bytes = Vec::with_capacity(size);
    let mut next = 0usize;
    while next < count {
        let from = i64::try_from(next).map_err(|_| corrupt("chunk count overflows"))?;
        let reply = run(
            store,
            vec![batch::query(
                "SELECT seq, data FROM plugin_artifact_chunks \
                 WHERE sha256 = ? AND chunk_bytes = ? AND seq >= ? \
                 ORDER BY seq LIMIT ?",
                vec![
                    batch::text(sha256),
                    batch::int(chunk_i64),
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
            return Err(corrupt(&format!("chunk {next} of {count} is missing")));
        }
        for row in rows {
            if next >= count {
                break;
            }
            let seq = int_cell(row, 0)?;
            if usize::try_from(seq).ok() != Some(next) {
                return Err(corrupt(&format!("chunk {next} of {count} is missing")));
            }
            let data = bytes_cell(row, 1)?;
            let want = chunk.min(size - bytes.len());
            if data.len() != want {
                return Err(corrupt(&format!(
                    "chunk {next} holds {} bytes, expected {want}",
                    data.len()
                )));
            }
            bytes.extend_from_slice(data);
            next += 1;
        }
    }
    if sha256_hex(&bytes) != sha256 {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "version blob {sha256} does not match its digest"
        )));
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use bookclerk_plugin_abi::DbCapabilities;

    use super::*;
    use crate::test_support::Profile;

    /// An archive that compresses badly and spans several chunks under
    /// every profile: larger than the shipped adapters' 256 KiB cell,
    /// request and result caps.
    fn archive(len: usize) -> Vec<u8> {
        let mut state: u32 = 0x9e37_79b9;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state.to_le_bytes()[0]
            })
            .collect()
    }

    #[test]
    fn chunks_fit_every_profile_cap() {
        for profile in Profile::ALL {
            let caps = profile.capabilities();
            let chunk = chunk_bytes(&caps);
            let cell = usize::try_from(caps.max_cell_bytes).unwrap();
            let base64 = chunk.div_ceil(3) * 4;
            assert!(base64 <= cell, "{}: {base64} > cell {cell}", profile.name());
            for cap in [
                caps.max_payload_bytes,
                caps.max_result_bytes,
                caps.max_atomic_result_bytes,
                caps.max_request_bytes,
            ] {
                let cap = usize::try_from(cap).unwrap();
                assert!(base64 + CHUNK_HEADROOM <= cap, "{}", profile.name());
            }
            assert!(read_width(&caps, chunk) >= 1);
        }
        assert_eq!(
            chunk_bytes(&DbCapabilities::host_minimum()),
            usize::try_from(bookclerk_plugin_abi::HOST_MIN_CELL_BYTES).unwrap() / 4 * 3
        );
    }

    #[test]
    fn unlimited_caps_still_bound_a_chunk_by_the_scalar_cap() {
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_cell_bytes = 0;
        caps.max_payload_bytes = 0;
        caps.max_result_bytes = 0;
        caps.max_atomic_result_bytes = 0;
        caps.max_request_bytes = 0;
        caps.max_lowered_statement_bytes = 0;
        caps.max_result_rows = 0;
        let chunk = chunk_bytes(&caps);
        assert_eq!(
            chunk,
            (usize::try_from(MAX_SCALAR_BYTES).unwrap() - CHUNK_HEADROOM) / 4 * 3
        );
        assert_eq!(read_width(&caps, chunk), u32::MAX);
    }

    /// The audit's case: an archive over 256 KiB used to be one base64
    /// cell of about 340 KiB, which no shipped adapter can return.
    #[tokio::test]
    async fn an_archive_over_256_kib_round_trips_under_every_profile() {
        for profile in Profile::ALL {
            // The host minimum moves 768 bytes a round trip; a full 1 MiB
            // archive there only makes the test slow.
            let len = match profile {
                Profile::FirstParty | Profile::D1 => MAX_ARTIFACT_BLOB_BYTES,
                Profile::Minimum => 300 * 1024,
            };
            let bytes = archive(len);
            let sha = sha256_hex(&bytes);
            let kit = crate::test_support::sqlite::memory(profile).await;
            let store = kit.store();
            let url = put_artifact_blob(store, &bytes).await.unwrap();
            assert_eq!(url, format!("{ARTIFACT_BLOB_SCHEME}{sha}"));
            let chunk = chunk_bytes(&profile.capabilities());
            assert!(chunk < bytes.len(), "{}", profile.name());
            assert_eq!(
                load_artifact_blob(store, &sha).await.unwrap().as_deref(),
                Some(&bytes[..]),
                "{}",
                profile.name()
            );
            assert_eq!(kit.refused_begins(), 0, "{}", profile.name());

            // A second store of the same archive writes nothing.
            let batches = kit.batch_sizes().len();
            assert_eq!(put_artifact_blob(store, &bytes).await.unwrap(), url);
            assert_eq!(
                kit.batch_sizes().len(),
                batches + 1,
                "{}: only the header is read",
                profile.name()
            );
        }
    }

    /// Inserts chunk `seq` of `bytes` under `chunk` bytes per chunk, the
    /// way an unfinished write leaves it.
    async fn insert_chunk(store: &LibraryStore, bytes: &[u8], chunk: usize, seq: usize) {
        let sha = sha256_hex(bytes);
        let part = bytes.chunks(chunk).nth(seq).unwrap();
        run(
            store,
            vec![batch::exec(
                "INSERT OR IGNORE INTO plugin_artifact_chunks (sha256, chunk_bytes, seq, data) \
                 VALUES (?, ?, ?, ?)",
                vec![
                    batch::text(&sha),
                    batch::int(i64::try_from(chunk).unwrap()),
                    batch::int(i64::try_from(seq).unwrap()),
                    batch::blob(part),
                ],
            )],
            0,
        )
        .await
        .unwrap();
    }

    /// Chunk rows stored for `sha` under `chunk` bytes per chunk.
    async fn chunk_rows(store: &LibraryStore, sha: &str, chunk: i64) -> i64 {
        let reply = run(
            store,
            vec![batch::query(
                "SELECT COUNT(*) FROM plugin_artifact_chunks WHERE sha256 = ? AND chunk_bytes = ?",
                vec![batch::text(sha), batch::int(chunk)],
                1,
            )],
            1,
        )
        .await
        .unwrap();
        int_cell(&batch::rows(&reply, 0)[0], 0).unwrap()
    }

    #[tokio::test]
    async fn chunks_without_a_header_are_not_an_archive_and_a_retry_completes_it() {
        let kit = crate::test_support::sqlite::memory(Profile::D1).await;
        let store = kit.store();
        let bytes = archive(600 * 1024);
        let sha = sha256_hex(&bytes);
        let chunk = chunk_bytes(store.db_capabilities());
        insert_chunk(store, &bytes, chunk, 0).await;
        assert!(load_artifact_blob(store, &sha).await.unwrap().is_none());
        put_artifact_blob(store, &bytes).await.unwrap();
        assert_eq!(
            load_artifact_blob(store, &sha).await.unwrap().as_deref(),
            Some(&bytes[..])
        );
    }

    #[tokio::test]
    async fn a_chunk_set_left_under_another_size_is_cleared() {
        let kit = crate::test_support::sqlite::memory(Profile::FirstParty).await;
        let store = kit.store();
        let bytes = archive(300 * 1024);
        let sha = sha256_hex(&bytes);
        insert_chunk(store, &bytes, 4_096, 0).await;
        insert_chunk(store, &bytes, 4_096, 1).await;
        put_artifact_blob(store, &bytes).await.unwrap();
        assert_eq!(chunk_rows(store, &sha, 4_096).await, 0);
        let chunk = chunk_bytes(store.db_capabilities());
        assert_eq!(
            chunk_rows(store, &sha, i64::try_from(chunk).unwrap()).await,
            i64::try_from(bytes.len().div_ceil(chunk)).unwrap()
        );
        assert_eq!(
            load_artifact_blob(store, &sha).await.unwrap().as_deref(),
            Some(&bytes[..])
        );
    }

    #[tokio::test]
    async fn a_missing_or_altered_chunk_is_an_error_not_an_archive() {
        let kit = crate::test_support::sqlite::memory(Profile::D1).await;
        let store = kit.store();
        let bytes = archive(300 * 1024);
        let sha = sha256_hex(&bytes);
        put_artifact_blob(store, &bytes).await.unwrap();
        let chunk = i64::try_from(chunk_bytes(store.db_capabilities())).unwrap();

        // Altered bytes of the right length fail the digest check.
        let flipped = batch::exec(
            "UPDATE plugin_artifact_chunks SET data = ? WHERE sha256 = ? AND seq = 0",
            vec![
                batch::blob(&vec![0u8; usize::try_from(chunk).unwrap()]),
                batch::text(&sha),
            ],
        );
        run(store, vec![flipped], 0).await.unwrap();
        let err = load_artifact_blob(store, &sha).await.unwrap_err();
        assert!(
            err.to_string().contains("does not match its digest"),
            "{err}"
        );

        // A short chunk is reported as the wrong size.
        let short = batch::exec(
            "UPDATE plugin_artifact_chunks SET data = ? WHERE sha256 = ? AND seq = 0",
            vec![batch::blob(b"short"), batch::text(&sha)],
        );
        run(store, vec![short], 0).await.unwrap();
        let err = load_artifact_blob(store, &sha).await.unwrap_err();
        assert!(err.to_string().contains("expected"), "{err}");

        // A gap is reported as a missing chunk.
        let gap = batch::exec(
            "DELETE FROM plugin_artifact_chunks WHERE sha256 = ? AND seq = 1",
            vec![batch::text(&sha)],
        );
        run(store, vec![gap], 0).await.unwrap();
        insert_chunk(store, &bytes, usize::try_from(chunk).unwrap(), 0).await;
        let restore = batch::exec(
            "UPDATE plugin_artifact_chunks SET data = ? WHERE sha256 = ? AND seq = 0",
            vec![
                batch::blob(&bytes[..usize::try_from(chunk).unwrap()]),
                batch::text(&sha),
            ],
        );
        run(store, vec![restore], 0).await.unwrap();
        let err = load_artifact_blob(store, &sha).await.unwrap_err();
        assert!(err.to_string().contains("chunk 1 of"), "{err}");
    }
}
