//! Streamed execute replies (`executeStreamed`).
//!
//! An adapter that advertises `maxStreamedResultBytes` answers
//! `executeStreamed` with the same `ExecuteReply` it would have answered
//! `execute` with, cut into `ExecuteStreamFrame` messages: a head, then for
//! each statement its columns followed by its rows in bounded slices, then
//! the engine timing. The frames travel inline when they fit one scalar and
//! as a `ByteSource` otherwise, so a result past the inline caps arrives
//! whole without any caller paging by hand.
//!
//! [`ExecuteStreamDecoder`] is the sans-IO receiver every transport feeds:
//! the host vat, the granted HTTP channel and the fuzz target run the same
//! state machine over the same bytes.

use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::db_execute::{DbCapabilities, DbColumn, DbRow, DbTiming, ExecuteReply, StatementResult};
use crate::db_rpc::{decode_execute_stream_frame_bytes, encoded_execute_stream_frame_bytes};
use crate::error::{LimitExceeded, PluginError, Result};
use crate::limits::{MAX_SCALAR_BYTES, MAX_STREAM_WINDOW_BYTES};

/// Encoded size an encoder aims for per `rows` frame. A frame may exceed it
/// only when one row alone does.
pub const STREAM_FRAME_TARGET_BYTES: u32 = 64 * 1024;

/// One frame of a streamed execute reply, in Rust terms.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecuteStreamFrame {
    /// First frame.
    Head {
        /// Echo of the request `operationId`.
        operation_id: String,
        /// Number of `Statement` frames that follow.
        statement_count: u32,
    },
    /// Opens the next statement result; its rows follow.
    Statement {
        /// Request statement index, increasing by one per frame from `0`.
        index: u32,
        /// Result column descriptors.
        columns: Vec<DbColumn>,
        /// Rows changed by a mutation.
        rows_affected: u64,
    },
    /// Rows of the open statement, in order.
    Rows(Vec<DbRow>),
    /// Last frame: the batch committed.
    End(DbTiming),
    /// Last frame: the batch failed.
    Err(PluginError),
}

/// Rough wire size of one row: cell payloads plus pointer and tag words.
fn row_size_estimate(row: &DbRow) -> usize {
    row.values
        .iter()
        .map(|value| match value {
            crate::db_value::DbValue::Text(text) => text.len() + 24,
            crate::db_value::DbValue::Bytes(bytes) => bytes.len() + 24,
            _ => 24,
        })
        .sum::<usize>()
        + 16
}

/// Encodes `rows` as one or more `Rows` frames, splitting a slice whose
/// encoding exceeds the scalar ceiling until each frame fits or holds one row.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a TEXT cell contains U+0000,
/// or [`PluginError::internal`] when a frame cannot be serialized.
fn push_rows_frames(out: &mut Vec<u8>, rows: &[DbRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let encoded = encoded_execute_stream_frame_bytes(&ExecuteStreamFrame::Rows(rows.to_vec()))?;
    let ceiling = usize::try_from(MAX_SCALAR_BYTES).unwrap_or(usize::MAX);
    if encoded.len() > ceiling && rows.len() > 1 {
        let (left, right) = rows.split_at(rows.len() / 2);
        push_rows_frames(out, left)?;
        return push_rows_frames(out, right);
    }
    out.extend_from_slice(&encoded);
    Ok(())
}

/// Frames `reply` as the byte sequence `executeStreamed` carries.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a TEXT cell contains U+0000,
/// or [`PluginError::internal`] when a frame cannot be serialized.
pub fn encode_execute_stream(reply: &ExecuteReply) -> Result<Vec<u8>> {
    let mut out = encoded_execute_stream_frame_bytes(&ExecuteStreamFrame::Head {
        operation_id: reply.operation_id.clone(),
        statement_count: u32::try_from(reply.statements.len()).map_err(|_| {
            PluginError::internal("execute reply has more than u32::MAX statements")
        })?,
    })?;
    let target = usize::try_from(STREAM_FRAME_TARGET_BYTES).unwrap_or(usize::MAX);
    for (index, stmt) in reply.statements.iter().enumerate() {
        out.extend_from_slice(&encoded_execute_stream_frame_bytes(
            &ExecuteStreamFrame::Statement {
                index: u32::try_from(index)
                    .map_err(|_| PluginError::internal("statement index exceeds u32"))?,
                columns: stmt.columns.clone(),
                rows_affected: stmt.rows_affected,
            },
        )?);
        let mut start = 0usize;
        let mut size = 0usize;
        for (i, row) in stmt.rows.iter().enumerate() {
            let estimate = row_size_estimate(row);
            if size > 0 && size + estimate > target {
                push_rows_frames(&mut out, &stmt.rows[start..i])?;
                start = i;
                size = 0;
            }
            size += estimate;
        }
        push_rows_frames(&mut out, &stmt.rows[start..])?;
    }
    out.extend_from_slice(&encoded_execute_stream_frame_bytes(
        &ExecuteStreamFrame::End(reply.timing.clone()),
    )?);
    Ok(out)
}

/// Frames a failure as the one `Err` frame a receiver accepts in place of a
/// reply (the granted HTTP channel answers every outcome as frames).
///
/// # Errors
///
/// Returns [`PluginError::internal`] when the frame cannot be serialized.
pub fn encode_execute_stream_error(err: &PluginError) -> Result<Vec<u8>> {
    encoded_execute_stream_frame_bytes(&ExecuteStreamFrame::Err(err.clone()))
}

/// What a receiver lets a streamed reply grow to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecuteStreamLimits {
    /// Encoded reply budget in bytes (`maxStreamedResultBytes`, or the inline
    /// `maxAtomicResultBytes` when the session does not stream).
    pub max_bytes: u32,
    /// Capability field `max_bytes` came from, for the error it raises.
    pub bytes_limit: &'static str,
    /// Rows one statement may return.
    pub max_rows: u32,
    /// Capability field `max_rows` came from, for the error it raises.
    pub rows_limit: &'static str,
}

impl ExecuteStreamLimits {
    /// The streamed budget of `caps`, or its inline caps when it does not
    /// stream.
    #[must_use]
    pub fn from_capabilities(caps: &DbCapabilities) -> Self {
        if caps.streams_results() {
            Self {
                max_bytes: caps.max_streamed_result_bytes,
                bytes_limit: "maxStreamedResultBytes",
                max_rows: caps.max_streamed_result_rows,
                rows_limit: "maxStreamedResultRows",
            }
        } else {
            Self {
                max_bytes: caps.max_atomic_result_bytes,
                bytes_limit: "maxAtomicResultBytes",
                max_rows: caps.max_result_rows,
                rows_limit: "maxResultRows",
            }
        }
    }

    /// Raw frame bytes a receiver accepts before failing closed: the reply
    /// budget plus one eighth for frame headers and one window of slack. A
    /// `0` budget accepts one window.
    #[must_use]
    pub fn raw_ceiling(&self) -> u64 {
        u64::from(self.max_bytes)
            + u64::from(self.max_bytes) / 8
            + u64::from(MAX_STREAM_WINDOW_BYTES)
    }

    /// Largest single frame the receiver decodes: the raw ceiling, clamped to
    /// `u32`.
    fn frame_ceiling(&self) -> u32 {
        u32::try_from(self.raw_ceiling()).unwrap_or(u32::MAX)
    }
}

/// Where the decoder is in the frame sequence.
#[derive(Debug)]
enum DecoderState {
    /// Nothing decoded yet; the next frame must be `Head`.
    AwaitingHead,
    /// Inside the reply; `next_index` statements have been opened.
    InReply {
        /// Echo of the request `operationId`.
        operation_id: String,
        /// Statement frames the head announced.
        statement_count: u32,
        /// Index the next `Statement` frame must carry.
        next_index: u32,
        /// Finished statements, in order.
        statements: Vec<StatementResult>,
        /// Statement whose rows are arriving.
        open: Option<StatementResult>,
    },
    /// The final frame arrived.
    Finished(std::result::Result<ExecuteReply, PluginError>),
}

/// Sans-IO decoder of a streamed execute reply: feed it bytes in any split,
/// then take the reassembled [`ExecuteReply`].
///
/// It fails closed on more raw bytes than the budget allows, on a statement
/// with more rows than the row cap, on duplicate column names or a row
/// whose width disagrees with its statement's columns (when the statement
/// closes, or as soon as it arrives on a [`Self::streaming`] decoder), and
/// on any frame out of sequence.
///
/// A decoder made with [`Self::streaming`] counts rows against the budget
/// but does not keep them: [`Self::push_frames`] hands each row out once,
/// as it arrives, so a reader can walk a result without holding it.
#[derive(Debug)]
pub struct ExecuteStreamDecoder {
    /// Budget the stream is held to.
    limits: ExecuteStreamLimits,
    /// Whether rows are kept for [`Self::finish`] (`false` for
    /// [`Self::streaming`], which hands them out instead).
    retain_rows: bool,
    /// Rows the open statement has received so far, kept or not; held to
    /// the row cap.
    open_rows: usize,
    /// Bytes of the frame being assembled.
    buf: Vec<u8>,
    /// Raw bytes pushed so far.
    seen: u64,
    /// Sequence position.
    state: DecoderState,
}

impl ExecuteStreamDecoder {
    /// A decoder that holds the stream to `limits`.
    #[must_use]
    pub fn new(limits: ExecuteStreamLimits) -> Self {
        Self {
            limits,
            retain_rows: true,
            open_rows: 0,
            buf: Vec::new(),
            seen: 0,
            state: DecoderState::AwaitingHead,
        }
    }

    /// A decoder that holds the stream to `limits` but keeps no rows: each
    /// row is handed out once by [`Self::push_frames`] and counted against
    /// the row cap, and the reply [`Self::finish`] returns carries every
    /// statement's columns and `rows_affected` with no rows.
    #[must_use]
    pub fn streaming(limits: ExecuteStreamLimits) -> Self {
        Self {
            retain_rows: false,
            ..Self::new(limits)
        }
    }

    /// True once the final frame (`End` or `Err`) has been decoded.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        matches!(self.state, DecoderState::Finished(_))
    }

    /// Feeds the next bytes of the stream.
    ///
    /// # Errors
    ///
    /// Returns a typed `limit_exceeded` error when the stream exceeds the
    /// byte or row budget, [`PluginError::invalid_params`] when a frame is
    /// malformed or out of sequence or bytes follow the final frame, and a
    /// decode failure from a frame's codec.
    pub fn push(&mut self, bytes: &[u8]) -> Result<()> {
        self.push_into(bytes, None)
    }

    /// Feeds the next bytes of the stream and appends the frames they
    /// completed to `out`, in order.
    ///
    /// A `Rows` frame carries the rows that arrived; on a
    /// [`Self::streaming`] decoder those rows are handed out here and not
    /// kept. A frame is appended only once the decoder has accepted it, so
    /// a row past the budget or out of sequence is never handed out, and
    /// the frames accepted before a failure stay in `out`.
    ///
    /// # Errors
    ///
    /// See [`Self::push`].
    pub fn push_frames(&mut self, bytes: &[u8], out: &mut Vec<ExecuteStreamFrame>) -> Result<()> {
        self.push_into(bytes, Some(out))
    }

    /// [`Self::push`], appending each accepted frame to `out` when given.
    ///
    /// # Errors
    ///
    /// See [`Self::push`].
    fn push_into(
        &mut self,
        bytes: &[u8],
        mut out: Option<&mut Vec<ExecuteStreamFrame>>,
    ) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.is_finished() {
            return Err(PluginError::invalid_params(
                "streamed reply carries bytes after its final frame",
            ));
        }
        self.seen = self
            .seen
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        if self.seen > self.limits.raw_ceiling() {
            let err = self.bytes_exceeded(self.seen);
            return self.fail(err);
        }
        self.buf.extend_from_slice(bytes);
        while self.buf.len() >= 8 {
            let mut word = [0u8; 4];
            word.copy_from_slice(&self.buf[..4]);
            if u32::from_le_bytes(word) != 0 {
                return self.fail(PluginError::invalid_params(
                    "streamed reply frame is not a single-segment Cap'n message",
                ));
            }
            word.copy_from_slice(&self.buf[4..8]);
            let frame_len = (u64::from(u32::from_le_bytes(word)) * 8).saturating_add(8);
            if frame_len > u64::from(self.limits.frame_ceiling()) {
                let err = self.bytes_exceeded(frame_len);
                return self.fail(err);
            }
            let frame_len = usize::try_from(frame_len).unwrap_or(usize::MAX);
            if self.buf.len() < frame_len {
                break;
            }
            let frame = decode_execute_stream_frame_bytes(
                &self.buf[..frame_len],
                self.limits.frame_ceiling(),
            )?;
            self.buf.drain(..frame_len);
            // Rows a retaining decoder keeps are copied out; a streaming
            // decoder hands its rows back from `apply` instead.
            let echo = (out.is_some()
                && (self.retain_rows || !matches!(frame, ExecuteStreamFrame::Rows(_))))
            .then(|| frame.clone());
            let handed = self.apply(frame)?;
            if let Some(out) = out.as_deref_mut() {
                if let Some(frame) = echo.or_else(|| handed.map(ExecuteStreamFrame::Rows)) {
                    out.push(frame);
                }
            }
            if self.is_finished() {
                if !self.buf.is_empty() {
                    return Err(PluginError::invalid_params(
                        "streamed reply carries bytes after its final frame",
                    ));
                }
                break;
            }
        }
        Ok(())
    }

    /// Takes the reassembled reply once the stream has ended.
    ///
    /// # Errors
    ///
    /// Returns the adapter's error when the stream ended with `Err`, and
    /// [`PluginError::unavailable`] when it ended before its final frame.
    pub fn finish(self) -> Result<ExecuteReply> {
        match self.state {
            DecoderState::Finished(outcome) => outcome,
            DecoderState::AwaitingHead | DecoderState::InReply { .. } => Err(
                PluginError::unavailable("streamed reply ended before its final frame"),
            ),
        }
    }

    /// The byte-budget error for `used` raw bytes.
    fn bytes_exceeded(&self, used: u64) -> PluginError {
        PluginError::limit_exceeded(LimitExceeded::new(
            self.limits.bytes_limit,
            u64::from(self.limits.max_bytes),
            used,
        ))
    }

    /// Records `err` as the stream's outcome, so every later push and the
    /// final `finish` report the same failure, and returns it.
    ///
    /// # Errors
    ///
    /// Always returns `err`.
    fn fail<T>(&mut self, err: PluginError) -> Result<T> {
        self.state = DecoderState::Finished(Err(err.clone()));
        Err(err)
    }

    /// Advances the sequence by one decoded frame.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::invalid_params`] when the frame is out of order
    /// (a frame before `Head`, a statement index that skips ahead, rows
    /// outside a statement, or any frame after the final one) or when a row's
    /// width or column names disagree with its statement, and
    /// [`PluginError::limit_exceeded`] when the rows received so far pass
    /// the row budget.
    ///
    /// # Returns
    ///
    /// The rows of a `Rows` frame on a [`Self::streaming`] decoder, which
    /// keeps none; `None` otherwise.
    fn apply(&mut self, frame: ExecuteStreamFrame) -> Result<Option<Vec<DbRow>>> {
        let mut handed = None;
        let state = std::mem::replace(&mut self.state, DecoderState::AwaitingHead);
        self.state = match (state, frame) {
            (
                DecoderState::AwaitingHead,
                ExecuteStreamFrame::Head {
                    operation_id,
                    statement_count,
                },
            ) => DecoderState::InReply {
                operation_id,
                statement_count,
                next_index: 0,
                statements: Vec::new(),
                open: None,
            },
            (DecoderState::AwaitingHead, ExecuteStreamFrame::Err(err)) => {
                DecoderState::Finished(Err(err))
            }
            (DecoderState::AwaitingHead, _) => {
                return self.fail(PluginError::invalid_params(
                    "streamed reply does not start with a head frame",
                ));
            }
            (DecoderState::InReply { .. }, ExecuteStreamFrame::Head { .. }) => {
                return self.fail(PluginError::invalid_params(
                    "streamed reply carries a second head frame",
                ));
            }
            (
                DecoderState::InReply {
                    operation_id,
                    statement_count,
                    next_index,
                    mut statements,
                    open,
                },
                ExecuteStreamFrame::Statement {
                    index,
                    columns,
                    rows_affected,
                },
            ) => {
                if let Some(done) = open {
                    if let Err(err) = close_statement(&mut statements, done) {
                        return self.fail(err);
                    }
                }
                if index != next_index {
                    return self.fail(PluginError::invalid_params(format!(
                        "streamed reply statement {index} arrived; expected {next_index}"
                    )));
                }
                if index >= statement_count {
                    return self.fail(PluginError::invalid_params(format!(
                        "streamed reply statement {index} exceeds the announced count {statement_count}"
                    )));
                }
                let opened = StatementResult {
                    rows: Vec::new(),
                    columns,
                    rows_affected,
                };
                // A retaining decoder validates a statement when it closes; a
                // streaming one hands rows out before then, so it checks the
                // columns before any row arrives (a reader never sees two
                // cells under one name).
                if !self.retain_rows {
                    if let Err(err) = opened.validate_positional() {
                        return self.fail(PluginError::invalid_params(err));
                    }
                }
                self.open_rows = 0;
                DecoderState::InReply {
                    operation_id,
                    statement_count,
                    next_index: next_index.saturating_add(1),
                    statements,
                    open: Some(opened),
                }
            }
            (
                DecoderState::InReply {
                    operation_id,
                    statement_count,
                    next_index,
                    statements,
                    open: Some(mut open),
                },
                ExecuteStreamFrame::Rows(rows),
            ) => {
                let total = self.open_rows.saturating_add(rows.len());
                let cap = usize::try_from(self.limits.max_rows).unwrap_or(usize::MAX);
                if self.limits.max_rows != 0 && total > cap {
                    return self.fail(PluginError::limit_exceeded(LimitExceeded::new(
                        self.limits.rows_limit,
                        u64::from(self.limits.max_rows),
                        u64::try_from(total).unwrap_or(u64::MAX),
                    )));
                }
                // Each row's width is checked as it arrives on a streaming
                // decoder, which will not have the rows when the statement
                // closes.
                let width = open.columns.len();
                if let Some(row) = rows
                    .iter()
                    .find(|row| !self.retain_rows && row.values.len() != width)
                {
                    return self.fail(PluginError::invalid_params(format!(
                        "streamed reply carries a row of {} values for {width} columns",
                        row.values.len()
                    )));
                }
                self.open_rows = total;
                if self.retain_rows {
                    open.rows.extend(rows);
                } else {
                    handed = Some(rows);
                }
                DecoderState::InReply {
                    operation_id,
                    statement_count,
                    next_index,
                    statements,
                    open: Some(open),
                }
            }
            (DecoderState::InReply { open: None, .. }, ExecuteStreamFrame::Rows(_)) => {
                return self.fail(PluginError::invalid_params(
                    "streamed reply carries rows outside a statement",
                ));
            }
            (
                DecoderState::InReply {
                    operation_id,
                    statement_count,
                    mut statements,
                    open,
                    ..
                },
                ExecuteStreamFrame::End(timing),
            ) => {
                if let Some(done) = open {
                    if let Err(err) = close_statement(&mut statements, done) {
                        return self.fail(err);
                    }
                }
                let count = u32::try_from(statements.len()).unwrap_or(u32::MAX);
                if count != statement_count {
                    return self.fail(PluginError::invalid_params(format!(
                        "streamed reply ended with {count} statements; its head announced {statement_count}"
                    )));
                }
                DecoderState::Finished(Ok(ExecuteReply {
                    operation_id,
                    statements,
                    timing,
                }))
            }
            (DecoderState::InReply { .. }, ExecuteStreamFrame::Err(err)) => {
                DecoderState::Finished(Err(err))
            }
            (DecoderState::Finished(_), _) => {
                return self.fail(PluginError::invalid_params(
                    "streamed reply carries a frame after its final frame",
                ));
            }
        };
        Ok(handed)
    }
}

/// Appends a completed statement after checking its row widths and names.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a row's width or the column
/// names disagree with the statement's declared columns.
fn close_statement(statements: &mut Vec<StatementResult>, done: StatementResult) -> Result<()> {
    done.validate_positional()
        .map_err(PluginError::invalid_params)?;
    statements.push(done);
    Ok(())
}

/// Decodes a whole streamed reply held in memory.
///
/// # Errors
///
/// See [`ExecuteStreamDecoder::push`] and [`ExecuteStreamDecoder::finish`].
pub fn decode_execute_stream(bytes: &[u8], limits: ExecuteStreamLimits) -> Result<ExecuteReply> {
    let mut decoder = ExecuteStreamDecoder::new(limits);
    decoder.push(bytes)?;
    decoder.finish()
}

/// How the frames of an [`ExecuteStream`] are held.
pub enum ExecuteStreamBody {
    /// Every frame, in memory; at most one scalar when produced by
    /// [`ExecuteStream::from_frames`].
    Inline(Vec<u8>),
    /// Frames read on demand.
    Reader(Pin<Box<dyn AsyncRead + Send>>),
}

impl std::fmt::Debug for ExecuteStreamBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inline(bytes) => f.debug_tuple("Inline").field(&bytes.len()).finish(),
            Self::Reader(_) => f.debug_tuple("Reader").finish(),
        }
    }
}

/// A streamed execute reply in flight: the frames, either inline or behind
/// a reader, before the receiver reassembles them.
#[derive(Debug)]
pub struct ExecuteStream {
    /// The frames.
    body: ExecuteStreamBody,
}

impl ExecuteStream {
    /// Frames already in memory: inline when they fit one scalar, otherwise
    /// behind a reader so the wire moves them in windows.
    #[must_use]
    pub fn from_frames(frames: Vec<u8>) -> Self {
        if frames.len() <= usize::try_from(MAX_SCALAR_BYTES).unwrap_or(usize::MAX) {
            Self::inline(frames)
        } else {
            Self::reader(Box::pin(std::io::Cursor::new(frames)))
        }
    }

    /// Frames `reply`, as an adapter answering `executeStreamed` does.
    ///
    /// # Errors
    ///
    /// See [`encode_execute_stream`].
    pub fn from_reply(reply: &ExecuteReply) -> Result<Self> {
        Ok(Self::from_frames(encode_execute_stream(reply)?))
    }

    /// Frames held inline (the `inline` member of `ExecuteStreamReply`).
    #[must_use]
    pub fn inline(frames: Vec<u8>) -> Self {
        Self {
            body: ExecuteStreamBody::Inline(frames),
        }
    }

    /// Frames behind a reader (the `stream` member of `ExecuteStreamReply`).
    #[must_use]
    pub fn reader(reader: Pin<Box<dyn AsyncRead + Send>>) -> Self {
        Self {
            body: ExecuteStreamBody::Reader(reader),
        }
    }

    /// The frames as the transport holds them.
    #[must_use]
    pub fn into_body(self) -> ExecuteStreamBody {
        self.body
    }

    /// Reads every frame and reassembles the reply, holding the stream to
    /// `limits`.
    ///
    /// # Errors
    ///
    /// Returns the decoder's errors (see [`ExecuteStreamDecoder::push`]) and
    /// [`PluginError::unavailable`] when the reader fails.
    pub async fn collect(self, limits: ExecuteStreamLimits) -> Result<ExecuteReply> {
        let mut decoder = ExecuteStreamDecoder::new(limits);
        match self.body {
            ExecuteStreamBody::Inline(bytes) => decoder.push(&bytes)?,
            ExecuteStreamBody::Reader(mut reader) => {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = reader.read(&mut buf).await.map_err(|err| {
                        PluginError::unavailable(format!("streamed reply read failed: {err}"))
                    })?;
                    if n == 0 {
                        break;
                    }
                    decoder.push(&buf[..n])?;
                }
            }
        }
        decoder.finish()
    }

    /// Reads every frame into memory without decoding, refusing more than
    /// `limits` allows (the granted HTTP channel relays the bytes as they are).
    ///
    /// # Errors
    ///
    /// Returns a typed `limit_exceeded` error past the raw ceiling and
    /// [`PluginError::unavailable`] when the reader fails.
    pub async fn into_bytes(self, limits: ExecuteStreamLimits) -> Result<Vec<u8>> {
        let ceiling = limits.raw_ceiling();
        match self.body {
            ExecuteStreamBody::Inline(bytes) => {
                let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                if len > ceiling {
                    return Err(PluginError::limit_exceeded(LimitExceeded::new(
                        limits.bytes_limit,
                        u64::from(limits.max_bytes),
                        len,
                    )));
                }
                Ok(bytes)
            }
            ExecuteStreamBody::Reader(mut reader) => {
                let mut out = Vec::new();
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = reader.read(&mut buf).await.map_err(|err| {
                        PluginError::unavailable(format!("streamed reply read failed: {err}"))
                    })?;
                    if n == 0 {
                        break;
                    }
                    let total = u64::try_from(out.len().saturating_add(n)).unwrap_or(u64::MAX);
                    if total > ceiling {
                        return Err(PluginError::limit_exceeded(LimitExceeded::new(
                            limits.bytes_limit,
                            u64::from(limits.max_bytes),
                            total,
                        )));
                    }
                    out.extend_from_slice(&buf[..n]);
                }
                Ok(out)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::db_value::{DbType, DbValue};
    use crate::PluginErrorCode;

    fn reply(rows: usize, text_len: usize) -> ExecuteReply {
        let text = "x".repeat(text_len);
        ExecuteReply {
            operation_id: "op-1".into(),
            statements: vec![
                StatementResult {
                    rows: (0..rows)
                        .map(|i| DbRow {
                            values: vec![
                                DbValue::Int64(i64::try_from(i).unwrap_or(0)),
                                DbValue::Text(text.clone()),
                            ],
                        })
                        .collect(),
                    columns: vec![
                        DbColumn {
                            name: "id".into(),
                            db_type: DbType::Int64,
                        },
                        DbColumn {
                            name: "body".into(),
                            db_type: DbType::Text,
                        },
                    ],
                    rows_affected: 0,
                },
                StatementResult {
                    rows: Vec::new(),
                    columns: Vec::new(),
                    rows_affected: 3,
                },
            ],
            timing: DbTiming {
                attempt_elapsed_us: 7,
                db_execution_us: 5,
                db_timing_source: "sqlite_txn".into(),
            },
        }
    }

    fn limits() -> ExecuteStreamLimits {
        ExecuteStreamLimits::from_capabilities(&DbCapabilities::advertised_sqlite())
    }

    #[test]
    fn round_trips_in_one_push_and_byte_by_byte() {
        let reply = reply(500, 300);
        let frames = encode_execute_stream(&reply).unwrap_or_else(|err| panic!("{err}"));
        assert!(frames.len() > usize::try_from(MAX_SCALAR_BYTES).unwrap_or(0) / 2);
        assert_eq!(
            decode_execute_stream(&frames, limits()).unwrap_or_else(|err| panic!("{err}")),
            reply
        );
        let mut decoder = ExecuteStreamDecoder::new(limits());
        for chunk in frames.chunks(7) {
            decoder.push(chunk).unwrap_or_else(|err| panic!("{err}"));
        }
        assert!(decoder.is_finished());
        assert_eq!(
            decoder.finish().unwrap_or_else(|err| panic!("{err}")),
            reply
        );
    }

    #[test]
    fn rows_frames_stay_under_the_scalar_ceiling() {
        let reply = reply(4_000, 200);
        let frames = encode_execute_stream(&reply).unwrap_or_else(|err| panic!("{err}"));
        let mut offset = 0usize;
        let mut count = 0usize;
        while offset < frames.len() {
            let words = u32::from_le_bytes([
                frames[offset + 4],
                frames[offset + 5],
                frames[offset + 6],
                frames[offset + 7],
            ]);
            let len = 8 + usize::try_from(words).unwrap_or(0) * 8;
            assert!(
                len <= usize::try_from(MAX_SCALAR_BYTES).unwrap_or(0),
                "frame {count} is {len} bytes"
            );
            offset += len;
            count += 1;
        }
        assert!(count > 4, "{count} frames");
        assert_eq!(
            decode_execute_stream(&frames, limits()).unwrap_or_else(|err| panic!("{err}")),
            reply
        );
    }

    #[test]
    fn an_error_frame_is_the_adapter_error() {
        let frames = encode_execute_stream_error(&PluginError::unavailable("engine gone"))
            .unwrap_or_else(|err| panic!("{err}"));
        let err = decode_execute_stream(&frames, limits()).expect_err("err frame");
        assert_eq!(err.code, PluginErrorCode::Unavailable);
        assert_eq!(err.message, "engine gone");
    }

    #[test]
    fn too_many_rows_fail_closed_with_the_streamed_row_cap() {
        let reply = reply(12, 4);
        let frames = encode_execute_stream(&reply).unwrap_or_else(|err| panic!("{err}"));
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_streamed_result_rows = 10;
        let err = decode_execute_stream(&frames, ExecuteStreamLimits::from_capabilities(&caps))
            .expect_err("row cap");
        assert_eq!(err.code, PluginErrorCode::LimitExceeded);
        assert_eq!(
            err.limit(),
            Some(LimitExceeded::new("maxStreamedResultRows", 10, 12))
        );
    }

    #[test]
    fn too_many_bytes_fail_closed_before_decoding() {
        let caps = DbCapabilities::advertised_sqlite();
        let limits = ExecuteStreamLimits::from_capabilities(&caps);
        let mut decoder = ExecuteStreamDecoder::new(limits);
        let over = usize::try_from(limits.raw_ceiling()).unwrap_or(usize::MAX) + 1;
        let err = decoder.push(&vec![0u8; over]).expect_err("byte cap");
        assert_eq!(err.code, PluginErrorCode::LimitExceeded);
        assert_eq!(
            err.limit().map(|limit| limit.limit),
            Some("maxStreamedResultBytes".to_string())
        );
    }

    #[test]
    fn frames_out_of_sequence_are_rejected() {
        let frames = encoded_execute_stream_frame_bytes(&ExecuteStreamFrame::Rows(Vec::new()))
            .unwrap_or_else(|err| panic!("{err}"));
        let err = decode_execute_stream(&frames, limits()).expect_err("rows first");
        assert_eq!(err.code, PluginErrorCode::InvalidParams);

        let reply = reply(1, 1);
        let mut frames = encode_execute_stream(&reply).unwrap_or_else(|err| panic!("{err}"));
        frames.extend_from_slice(
            &encoded_execute_stream_frame_bytes(&ExecuteStreamFrame::End(DbTiming::default()))
                .unwrap_or_else(|err| panic!("{err}")),
        );
        let err = decode_execute_stream(&frames, limits()).expect_err("trailing frame");
        assert_eq!(err.code, PluginErrorCode::InvalidParams);
    }

    #[test]
    fn a_truncated_stream_is_unavailable() {
        let reply = reply(3, 10);
        let frames = encode_execute_stream(&reply).unwrap_or_else(|err| panic!("{err}"));
        let err =
            decode_execute_stream(&frames[..frames.len() - 9], limits()).expect_err("truncated");
        assert_eq!(err.code, PluginErrorCode::Unavailable);
    }

    #[tokio::test]
    async fn collect_reads_inline_and_reader_bodies() {
        let reply = reply(2_000, 300);
        let stream = ExecuteStream::from_reply(&reply).unwrap_or_else(|err| panic!("{err}"));
        assert!(matches!(stream.body, ExecuteStreamBody::Reader(_)));
        assert_eq!(
            stream
                .collect(limits())
                .await
                .unwrap_or_else(|err| panic!("{err}")),
            reply
        );

        let small = self::reply(2, 3);
        let stream = ExecuteStream::from_reply(&small).unwrap_or_else(|err| panic!("{err}"));
        assert!(matches!(stream.body, ExecuteStreamBody::Inline(_)));
        assert_eq!(
            stream
                .collect(limits())
                .await
                .unwrap_or_else(|err| panic!("{err}")),
            small
        );
    }

    fn frame(frame: &ExecuteStreamFrame) -> Vec<u8> {
        encoded_execute_stream_frame_bytes(frame).unwrap_or_else(|err| panic!("{err}"))
    }

    fn head(statement_count: u32) -> Vec<u8> {
        frame(&ExecuteStreamFrame::Head {
            operation_id: "op-seq".into(),
            statement_count,
        })
    }

    fn statement(index: u32) -> Vec<u8> {
        frame(&ExecuteStreamFrame::Statement {
            index,
            columns: Vec::new(),
            rows_affected: 1,
        })
    }

    fn invalid(frames: &[&[u8]]) -> PluginError {
        let bytes = frames.concat();
        let err = decode_execute_stream(&bytes, limits()).expect_err("out-of-sequence frames");
        assert_eq!(err.code, PluginErrorCode::InvalidParams, "{err}");
        err
    }

    #[test]
    fn every_out_of_sequence_frame_is_invalid_params() {
        let end = frame(&ExecuteStreamFrame::End(DbTiming::default()));
        let rows = frame(&ExecuteStreamFrame::Rows(vec![DbRow {
            values: Vec::new(),
        }]));
        assert!(invalid(&[&head(1), &head(1)])
            .message
            .contains("second head"));
        assert!(invalid(&[&head(2), &statement(1)])
            .message
            .contains("expected 0"));
        assert!(invalid(&[&head(1), &statement(0), &statement(1)])
            .message
            .contains("exceeds the announced count"));
        assert!(invalid(&[&head(1), &rows])
            .message
            .contains("rows outside a statement"));
        assert!(invalid(&[&head(2), &statement(0), &end])
            .message
            .contains("ended with 1 statements"));
    }

    #[test]
    fn a_rejected_frame_ends_the_stream_for_good() {
        let mut decoder = ExecuteStreamDecoder::new(limits());
        decoder.push(&head(1)).unwrap_or_else(|err| panic!("{err}"));
        let first = decoder.push(&head(1)).expect_err("second head");
        assert!(decoder.is_finished());
        let again = decoder.push(&statement(0)).expect_err("sticky failure");
        assert_eq!(again.code, PluginErrorCode::InvalidParams);
        assert!(again.message.contains("after its final frame"), "{again}");
        assert_eq!(decoder.finish().expect_err("finish after failure"), first);
    }

    /// A statement whose rows do not match its columns is rejected when it
    /// closes, and that rejection ends the stream like any other.
    #[test]
    fn a_statement_that_fails_validation_ends_the_stream_for_good() {
        let mut decoder = ExecuteStreamDecoder::new(limits());
        decoder.push(&head(1)).unwrap_or_else(|err| panic!("{err}"));
        decoder
            .push(&statement(0))
            .unwrap_or_else(|err| panic!("{err}"));
        let wide = frame(&ExecuteStreamFrame::Rows(vec![DbRow {
            values: vec![DbValue::Int64(1)],
        }]));
        decoder.push(&wide).unwrap_or_else(|err| panic!("{err}"));
        let first = decoder
            .push(&frame(&ExecuteStreamFrame::End(DbTiming::default())))
            .expect_err("a row wider than its columns");
        assert_eq!(first.code, PluginErrorCode::InvalidParams);
        assert!(decoder.is_finished(), "the failure is final");
        let again = decoder.push(&head(1)).expect_err("no new reply");
        assert!(again.message.contains("after its final frame"), "{again}");
        assert_eq!(decoder.finish().expect_err("finish after failure"), first);
    }

    /// The next statement frame closes the open one, so a row wider than
    /// the open statement's columns is caught when the next statement opens.
    #[test]
    fn the_next_statement_closes_and_validates_the_open_one() {
        let mut decoder = ExecuteStreamDecoder::new(limits());
        decoder.push(&head(2)).unwrap_or_else(|err| panic!("{err}"));
        decoder
            .push(&statement(0))
            .unwrap_or_else(|err| panic!("{err}"));
        let wide = frame(&ExecuteStreamFrame::Rows(vec![DbRow {
            values: vec![DbValue::Int64(1)],
        }]));
        decoder.push(&wide).unwrap_or_else(|err| panic!("{err}"));
        let err = decoder
            .push(&statement(1))
            .expect_err("a row wider than its columns");
        assert_eq!(err.code, PluginErrorCode::InvalidParams, "{err}");
        assert!(err.message.contains("row 0 has 1 values"), "{err}");
        assert!(decoder.is_finished(), "the failure is final");
    }

    /// `push` refuses bytes after the final frame before decoding them, and
    /// the state machine refuses a frame after its final one as well.
    #[test]
    fn a_frame_after_the_final_frame_is_rejected_by_the_state_machine() {
        let mut decoder = ExecuteStreamDecoder::new(limits());
        let bytes = [
            head(0),
            frame(&ExecuteStreamFrame::End(DbTiming::default())),
        ]
        .concat();
        decoder.push(&bytes).unwrap_or_else(|err| panic!("{err}"));
        assert!(
            decoder.is_finished(),
            "an empty reply ends at its end frame"
        );
        let err = decoder
            .apply(ExecuteStreamFrame::End(DbTiming::default()))
            .expect_err("a frame after the final frame");
        assert_eq!(err.code, PluginErrorCode::InvalidParams, "{err}");
        assert!(err.message.contains("after its final frame"), "{err}");
        assert_eq!(decoder.finish().expect_err("the rejection is final"), err);
    }

    #[test]
    fn an_error_frame_after_the_head_is_the_adapter_error() {
        let mut bytes = head(1);
        bytes.extend_from_slice(&statement(0));
        bytes.extend_from_slice(
            &encode_execute_stream_error(&PluginError::conflict("unique"))
                .unwrap_or_else(|err| panic!("{err}")),
        );
        let err = decode_execute_stream(&bytes, limits()).expect_err("err frame");
        assert_eq!(err.code, PluginErrorCode::Conflict);
        assert_eq!(err.message, "unique");
    }

    #[test]
    fn malformed_frame_headers_fail_before_decoding() {
        let mut decoder = ExecuteStreamDecoder::new(limits());
        decoder.push(&[]).unwrap_or_else(|err| panic!("{err}"));
        let err = decoder
            .push(&[1, 0, 0, 0, 1, 0, 0, 0])
            .expect_err("segment count");
        assert_eq!(err.code, PluginErrorCode::InvalidParams);
        assert!(err.message.contains("single-segment"), "{err}");

        let mut decoder = ExecuteStreamDecoder::new(limits());
        let err = decoder
            .push(&[0, 0, 0, 0, 0xff, 0xff, 0xff, 0x7f])
            .expect_err("frame length");
        assert_eq!(err.code, PluginErrorCode::LimitExceeded);
        assert_eq!(
            err.limit().map(|limit| limit.limit),
            Some("maxStreamedResultBytes".to_string())
        );
    }

    #[test]
    fn bytes_after_the_final_frame_are_rejected() {
        let frames = encode_execute_stream(&reply(1, 1)).unwrap_or_else(|err| panic!("{err}"));
        let mut decoder = ExecuteStreamDecoder::new(limits());
        decoder.push(&frames).unwrap_or_else(|err| panic!("{err}"));
        assert!(decoder.is_finished());
        let err = decoder.push(&[0]).expect_err("trailing bytes");
        assert_eq!(err.code, PluginErrorCode::InvalidParams);
        assert!(err.message.contains("after its final frame"), "{err}");
    }

    #[test]
    fn blob_cells_round_trip_through_the_frames() {
        let mut reply = reply(3, 2);
        reply.statements[0].columns[1].db_type = DbType::Bytes;
        for (i, row) in reply.statements[0].rows.iter_mut().enumerate() {
            row.values[1] = DbValue::Bytes(vec![u8::try_from(i).unwrap_or(0); 300]);
        }
        let frames = encode_execute_stream(&reply).unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(
            decode_execute_stream(&frames, limits()).unwrap_or_else(|err| panic!("{err}")),
            reply
        );
    }

    /// A reader whose first read fails.
    struct FailingReader;

    impl AsyncRead for FailingReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("pipe closed")))
        }
    }

    #[tokio::test]
    async fn a_failing_reader_is_unavailable() {
        let err = ExecuteStream::reader(Box::pin(FailingReader))
            .collect(limits())
            .await
            .expect_err("read failure");
        assert_eq!(err.code, PluginErrorCode::Unavailable);
        assert!(err.message.contains("pipe closed"), "{err}");
        let err = ExecuteStream::reader(Box::pin(FailingReader))
            .into_bytes(limits())
            .await
            .expect_err("read failure");
        assert_eq!(err.code, PluginErrorCode::Unavailable);
    }

    #[tokio::test]
    async fn into_bytes_relays_both_bodies_under_the_raw_ceiling() {
        let large = reply(2_000, 300);
        let frames = encode_execute_stream(&large).unwrap_or_else(|err| panic!("{err}"));
        let stream = ExecuteStream::from_frames(frames.clone());
        assert_eq!(format!("{stream:?}"), "ExecuteStream { body: Reader }");
        assert_eq!(
            stream
                .into_bytes(limits())
                .await
                .unwrap_or_else(|err| panic!("{err}")),
            frames
        );

        let small = encode_execute_stream(&reply(1, 1)).unwrap_or_else(|err| panic!("{err}"));
        let stream = ExecuteStream::inline(small.clone());
        assert_eq!(
            format!("{stream:?}"),
            format!("ExecuteStream {{ body: Inline({}) }}", small.len())
        );
        match stream.into_body() {
            ExecuteStreamBody::Inline(bytes) => assert_eq!(bytes, small),
            ExecuteStreamBody::Reader(_) => panic!("inline body expected"),
        }

        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_streamed_result_bytes = caps.max_atomic_result_bytes;
        let tight = ExecuteStreamLimits::from_capabilities(&caps);
        let over = usize::try_from(tight.raw_ceiling()).unwrap_or(usize::MAX) + 1;
        let err = ExecuteStream::inline(vec![0u8; over])
            .into_bytes(tight)
            .await
            .expect_err("inline over the ceiling");
        assert_eq!(err.code, PluginErrorCode::LimitExceeded);
        let err = ExecuteStream::reader(Box::pin(std::io::Cursor::new(vec![0u8; over])))
            .into_bytes(tight)
            .await
            .expect_err("reader over the ceiling");
        assert_eq!(err.code, PluginErrorCode::LimitExceeded);
        assert_eq!(
            err.limit().map(|limit| limit.limit),
            Some("maxStreamedResultBytes".to_string())
        );
    }

    #[test]
    fn limits_follow_the_advertisement() {
        let streamed = limits();
        assert_eq!(
            streamed.max_bytes,
            crate::FIRST_PARTY_MAX_STREAMED_RESULT_BYTES
        );
        assert_eq!(streamed.rows_limit, "maxStreamedResultRows");
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_streamed_result_bytes = 0;
        caps.max_streamed_result_rows = 0;
        let inline = ExecuteStreamLimits::from_capabilities(&caps);
        assert_eq!(inline.max_bytes, caps.max_atomic_result_bytes);
        assert_eq!(inline.bytes_limit, "maxAtomicResultBytes");
        assert_eq!(inline.max_rows, caps.max_result_rows);
    }
    /// The body's `Debug` names the transport form without dumping frames,
    /// and `into_body` hands the frames back as the wire holds them.
    #[tokio::test]
    async fn body_debug_names_the_transport_form() {
        let inline = ExecuteStream::inline(vec![1, 2, 3]);
        assert_eq!(format!("{inline:?}"), "ExecuteStream { body: Inline(3) }");
        let ExecuteStreamBody::Inline(bytes) = inline.into_body() else {
            panic!("inline frames stay inline");
        };
        assert_eq!(bytes, vec![1, 2, 3]);

        let reader = ExecuteStream::reader(Box::pin(std::io::Cursor::new(vec![0u8; 16])));
        assert_eq!(format!("{reader:?}"), "ExecuteStream { body: Reader }");
        let ExecuteStreamBody::Reader(mut reader) = reader.into_body() else {
            panic!("reader frames stay behind the reader");
        };
        let mut out = Vec::new();
        reader
            .read_to_end(&mut out)
            .await
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(out.len(), 16);
    }

    /// A statement whose rows exceed one scalar is cut into several `Rows`
    /// frames, down to one row per frame when a single row is that wide.
    #[test]
    fn wide_rows_split_until_each_frame_fits_or_holds_one_row() {
        let wide = "x".repeat(200 * 1024);
        let rows = (0..3)
            .map(|_| DbRow {
                values: vec![crate::db_value::DbValue::Text(wide.clone())],
            })
            .collect::<Vec<_>>();
        let mut out = Vec::new();
        push_rows_frames(&mut out, &rows).unwrap_or_else(|err| panic!("{err}"));
        let mut frames = 0usize;
        let mut cursor = 0usize;
        while cursor < out.len() {
            let mut word = [0u8; 4];
            word.copy_from_slice(&out[cursor + 4..cursor + 8]);
            let len = usize::try_from(u32::from_le_bytes(word)).unwrap_or(usize::MAX) * 8 + 8;
            let frame = decode_execute_stream_frame_bytes(&out[cursor..cursor + len], u32::MAX)
                .unwrap_or_else(|err| panic!("{err}"));
            let ExecuteStreamFrame::Rows(rows) = frame else {
                panic!("only rows frames: {frame:?}");
            };
            assert_eq!(rows.len(), 1, "each 200 KiB row travels alone");
            frames += 1;
            cursor += len;
        }
        assert_eq!(frames, 3);
    }

    /// Rows handed out by `push_frames`, per statement, and the other
    /// frames' kinds, in order.
    fn drive(
        decoder: &mut ExecuteStreamDecoder,
        frames: &[u8],
        chunk: usize,
    ) -> (Vec<Vec<DbRow>>, Vec<&'static str>) {
        let mut rows: Vec<Vec<DbRow>> = Vec::new();
        let mut kinds = Vec::new();
        for piece in frames.chunks(chunk) {
            let mut frames = Vec::new();
            decoder
                .push_frames(piece, &mut frames)
                .unwrap_or_else(|err| panic!("{err}"));
            for frame in frames {
                match frame {
                    ExecuteStreamFrame::Head { .. } => kinds.push("head"),
                    ExecuteStreamFrame::Statement { .. } => {
                        kinds.push("statement");
                        rows.push(Vec::new());
                    }
                    ExecuteStreamFrame::Rows(batch) => {
                        if let Some(open) = rows.last_mut() {
                            open.extend(batch);
                        }
                    }
                    ExecuteStreamFrame::End(_) => kinds.push("end"),
                    ExecuteStreamFrame::Err(_) => kinds.push("err"),
                }
            }
        }
        (rows, kinds)
    }

    /// A streaming decoder hands every row out once, in order, keeps none,
    /// and still finishes with the reply's columns, counts and timing.
    #[test]
    fn a_streaming_decoder_hands_rows_out_and_keeps_none() {
        let reply = reply(500, 300);
        let frames = encode_execute_stream(&reply).unwrap_or_else(|err| panic!("{err}"));
        for chunk in [7, 4096, frames.len()] {
            let mut decoder = ExecuteStreamDecoder::streaming(limits());
            let (rows, kinds) = drive(&mut decoder, &frames, chunk);
            assert_eq!(kinds, ["head", "statement", "statement", "end"]);
            let expected: Vec<Vec<DbRow>> =
                reply.statements.iter().map(|s| s.rows.clone()).collect();
            assert_eq!(rows, expected, "chunks of {chunk}");
            let mut stripped = reply.clone();
            for stmt in &mut stripped.statements {
                stmt.rows.clear();
            }
            assert_eq!(
                decoder.finish().unwrap_or_else(|err| panic!("{err}")),
                stripped
            );
        }
    }

    /// A retaining decoder's `push_frames` returns copies of the rows it
    /// keeps, so both modes see the same frames.
    #[test]
    fn a_retaining_decoder_returns_the_frames_it_keeps() {
        let reply = reply(40, 10);
        let frames = encode_execute_stream(&reply).unwrap_or_else(|err| panic!("{err}"));
        let mut decoder = ExecuteStreamDecoder::new(limits());
        let (rows, _) = drive(&mut decoder, &frames, 100);
        assert_eq!(rows[0], reply.statements[0].rows);
        assert_eq!(
            decoder.finish().unwrap_or_else(|err| panic!("{err}")),
            reply
        );
    }

    /// The row cap counts rows a streaming decoder no longer holds, and no
    /// row past it is handed out.
    #[test]
    fn a_streaming_decoder_holds_the_row_cap() {
        let tight = ExecuteStreamLimits {
            max_rows: 3,
            ..limits()
        };
        let mut decoder = ExecuteStreamDecoder::streaming(tight);
        decoder.push(&head(1)).unwrap_or_else(|err| panic!("{err}"));
        decoder
            .push(&statement(0))
            .unwrap_or_else(|err| panic!("{err}"));
        let two = frame(&ExecuteStreamFrame::Rows(vec![
            DbRow { values: Vec::new() },
            DbRow { values: Vec::new() },
        ]));
        let mut handed = Vec::new();
        decoder
            .push_frames(&two, &mut handed)
            .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(handed.len(), 1);
        let err = decoder.push_frames(&two, &mut handed).expect_err("row cap");
        assert_eq!(handed.len(), 1, "no row past the cap is handed out");
        assert_eq!(err.code, PluginErrorCode::LimitExceeded, "{err}");
        let limit = err.limit().unwrap_or_else(|| panic!("typed limit: {err}"));
        assert_eq!((limit.cap, limit.used), (3, 4));
        assert!(decoder.is_finished(), "the refusal is final");
    }

    /// A streaming decoder refuses a row whose width disagrees with its
    /// statement, and duplicate column names, before handing anything out.
    #[test]
    fn a_streaming_decoder_validates_before_handing_rows_out() {
        let mut decoder = ExecuteStreamDecoder::streaming(limits());
        decoder.push(&head(1)).unwrap_or_else(|err| panic!("{err}"));
        decoder
            .push(&statement(0))
            .unwrap_or_else(|err| panic!("{err}"));
        let wide = frame(&ExecuteStreamFrame::Rows(vec![DbRow {
            values: vec![DbValue::Int64(1)],
        }]));
        let mut handed = Vec::new();
        let err = decoder
            .push_frames(&wide, &mut handed)
            .expect_err("wide row");
        assert!(handed.is_empty());
        assert_eq!(err.code, PluginErrorCode::InvalidParams, "{err}");
        assert!(err.message.contains("1 values for 0 columns"), "{err}");
        assert!(decoder.is_finished(), "the refusal is final");

        let column = DbColumn {
            name: "id".into(),
            db_type: DbType::Int64,
        };
        let duplicate = frame(&ExecuteStreamFrame::Statement {
            index: 0,
            columns: vec![column.clone(), column],
            rows_affected: 0,
        });
        let mut decoder = ExecuteStreamDecoder::streaming(limits());
        decoder.push(&head(1)).unwrap_or_else(|err| panic!("{err}"));
        let err = decoder
            .push_frames(&duplicate, &mut handed)
            .expect_err("duplicate column");
        assert!(handed.is_empty());
        assert!(err.message.contains("duplicate result column"), "{err}");
        // A retaining decoder accepts the statement until it closes.
        let mut decoder = ExecuteStreamDecoder::new(limits());
        decoder.push(&head(1)).unwrap_or_else(|err| panic!("{err}"));
        decoder
            .push(&duplicate)
            .unwrap_or_else(|err| panic!("{err}"));
    }

    /// Frames a push accepted before it failed stay with the caller, so a
    /// streaming reader keeps the rows that arrived ahead of the failure.
    #[test]
    fn frames_accepted_before_a_failure_stay_with_the_caller() {
        let good = frame(&ExecuteStreamFrame::Rows(vec![DbRow {
            values: Vec::new(),
        }]));
        let bytes = [head(1), statement(0), good, head(1)].concat();
        let mut decoder = ExecuteStreamDecoder::streaming(limits());
        let mut out = Vec::new();
        let err = decoder
            .push_frames(&bytes, &mut out)
            .expect_err("second head");
        assert_eq!(err.code, PluginErrorCode::InvalidParams, "{err}");
        assert_eq!(out.len(), 3, "head, statement and the row frame");
        assert!(matches!(&out[2], ExecuteStreamFrame::Rows(rows) if rows.len() == 1));
    }
}
