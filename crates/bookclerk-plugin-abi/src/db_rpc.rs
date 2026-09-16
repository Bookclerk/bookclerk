//! Cap'n Proto codec for typed database execute / capabilities.

#![allow(clippy::missing_docs_in_private_items)]

#[cfg(feature = "host")]
use crate::host_envelope::{GuestReceiptPersist, HostExecuteEnvelope};
use crate::plugin_capnp::{
    db_bootstrap as db_bootstrap_capnp, db_bootstrap_reply, db_capabilities as db_caps_capnp,
    db_capabilities_reply, db_column as db_column_capnp, db_row as db_row_capnp,
    db_statement as db_statement_capnp, db_timing as db_timing_capnp, db_value as db_value_capnp,
    execute_reply as execute_reply_capnp, execute_request as execute_request_capnp,
    execute_result_reply, statement_result as statement_result_capnp,
    DbResultSelection as CapnpDbResultSelection, DbStatementKind as CapnpDbStatementKind,
    DbType as CapnpDbType,
};
#[cfg(feature = "host")]
use crate::plugin_host_capnp::{
    host_execute_envelope as host_execute_envelope_capnp,
    host_guest_receipt_persist as host_guest_receipt_persist_capnp,
};
use crate::rpc::{from_capnp, read_error, text_of, write_error};
use crate::{
    DbBootstrap, DbCapabilities, DbColumn, DbPlanStatementKind, DbResultSelection, DbRow, DbTiming,
    DbType, DbValue, ExecuteReply, ExecuteRequest, PluginError, Result, StatementResult,
    TypedDbStatement, MAX_SCALAR_BYTES,
};

pub(super) fn write_db_type(ty: DbType) -> CapnpDbType {
    match ty {
        DbType::Unspecified => CapnpDbType::Unspecified,
        DbType::Bool => CapnpDbType::Bool,
        DbType::Int64 => CapnpDbType::Int64,
        DbType::Float64 => CapnpDbType::Float64,
        DbType::Text => CapnpDbType::Text,
        DbType::Bytes => CapnpDbType::Bytes,
    }
}

/// # Errors
///
/// Returns [`PluginError::unsupported`] when the wire tag is unknown.
pub(super) fn read_db_type(ty: CapnpDbType) -> Result<DbType> {
    match ty {
        CapnpDbType::Unspecified => Ok(DbType::Unspecified),
        CapnpDbType::Bool => Ok(DbType::Bool),
        CapnpDbType::Int64 => Ok(DbType::Int64),
        CapnpDbType::Float64 => Ok(DbType::Float64),
        CapnpDbType::Text => Ok(DbType::Text),
        CapnpDbType::Bytes => Ok(DbType::Bytes),
    }
}

/// BookclerkSQL `TEXT` forbids U+0000; `BYTES` may contain 0x00.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a TEXT cell contains U+0000.
fn require_portable_value(v: &DbValue) -> Result<()> {
    if let DbValue::Text(text) = v {
        crate::sql_text::require_portable_text(text)?;
    }
    Ok(())
}

/// Rejects U+0000 in every TEXT cell of a statement result.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a TEXT cell contains U+0000.
fn require_portable_statement_result(stmt: &StatementResult) -> Result<()> {
    for row in &stmt.rows {
        for value in &row.values {
            require_portable_value(value)?;
        }
    }
    Ok(())
}

/// Rejects U+0000 in every TEXT cell of an execute reply.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a TEXT cell contains U+0000.
fn require_portable_execute_reply(reply: &ExecuteReply) -> Result<()> {
    for stmt in &reply.statements {
        require_portable_statement_result(stmt)?;
    }
    Ok(())
}

pub(super) fn write_db_value(mut b: db_value_capnp::Builder<'_>, v: &DbValue) {
    match v {
        DbValue::Null(ty) => b.set_null(write_db_type(*ty)),
        DbValue::Boolean(x) => b.set_boolean(*x),
        DbValue::Int64(x) => b.set_int64(*x),
        DbValue::Float64(x) => b.set_float64(*x),
        DbValue::Text(s) => b.set_text(s),
        DbValue::Bytes(d) => b.set_bytes(d),
    }
}

/// # Errors
///
/// Returns [`PluginError::unsupported`] or [`PluginError::invalid_params`] when
/// the union member is unknown or a float64 is not finite.
pub(super) fn read_db_value(r: db_value_capnp::Reader<'_>) -> Result<DbValue> {
    match r.which() {
        Ok(db_value_capnp::Null(ty)) => {
            let ty = ty.map_err(|_| PluginError::unsupported("unknown DbType"))?;
            Ok(DbValue::Null(read_db_type(ty)?))
        }
        Ok(db_value_capnp::Boolean(x)) => Ok(DbValue::Boolean(x)),
        Ok(db_value_capnp::Int64(x)) => Ok(DbValue::Int64(x)),
        Ok(db_value_capnp::Float64(x)) => {
            if !x.is_finite() {
                return Err(PluginError::invalid_params("float64 value is not finite"));
            }
            Ok(DbValue::Float64(x))
        }
        Ok(db_value_capnp::Text(t)) => {
            let text = text_of(t.map_err(from_capnp)?);
            crate::sql_text::require_portable_text(&text)?;
            Ok(DbValue::Text(text))
        }
        Ok(db_value_capnp::Bytes(d)) => Ok(DbValue::Bytes(d.map_err(from_capnp)?.to_vec())),
        Err(_) => Err(PluginError::unsupported("unknown DbValue union member")),
    }
}

fn write_kind(kind: DbPlanStatementKind) -> CapnpDbStatementKind {
    match kind {
        DbPlanStatementKind::Select => CapnpDbStatementKind::Select,
        DbPlanStatementKind::Execute => CapnpDbStatementKind::Execute,
        DbPlanStatementKind::Returning => CapnpDbStatementKind::Returning,
    }
}

/// # Errors
///
/// Returns [`PluginError::unsupported`] when the wire tag is unknown.
fn read_kind(kind: CapnpDbStatementKind) -> Result<DbPlanStatementKind> {
    match kind {
        CapnpDbStatementKind::Execute => Ok(DbPlanStatementKind::Execute),
        CapnpDbStatementKind::Select => Ok(DbPlanStatementKind::Select),
        CapnpDbStatementKind::Returning => Ok(DbPlanStatementKind::Returning),
    }
}

fn write_selection(sel: DbResultSelection) -> CapnpDbResultSelection {
    match sel {
        DbResultSelection::Discard => CapnpDbResultSelection::Discard,
        DbResultSelection::AffectedRows => CapnpDbResultSelection::AffectedRows,
        DbResultSelection::Rows => CapnpDbResultSelection::Rows,
    }
}

/// # Errors
///
/// Returns [`PluginError::unsupported`] when the wire tag is unknown.
fn read_selection(sel: CapnpDbResultSelection) -> Result<DbResultSelection> {
    match sel {
        CapnpDbResultSelection::Discard => Ok(DbResultSelection::Discard),
        CapnpDbResultSelection::AffectedRows => Ok(DbResultSelection::AffectedRows),
        CapnpDbResultSelection::Rows => Ok(DbResultSelection::Rows),
    }
}

fn write_db_statement(mut b: db_statement_capnp::Builder<'_>, stmt: &TypedDbStatement) {
    b.set_sql(&stmt.sql);
    b.set_kind(write_kind(stmt.kind));
    b.set_max_rows(stmt.max_rows);
    b.set_result_selection(write_selection(stmt.result_selection));
    let mut params = b.reborrow().init_parameters(stmt.parameters.len() as u32);
    for (i, p) in stmt.parameters.iter().enumerate() {
        write_db_value(params.reborrow().get(i as u32), p);
    }
}

/// # Errors
///
/// Returns when SQL, parameters, or statement tags cannot be decoded.
fn read_db_statement(r: db_statement_capnp::Reader<'_>) -> Result<TypedDbStatement> {
    let sql = text_of(r.get_sql().map_err(from_capnp)?);
    let kind = read_kind(
        r.get_kind()
            .map_err(|_| PluginError::unsupported("unknown DbStatementKind"))?,
    )?;
    let max_rows = r.get_max_rows();
    let result_selection = match r.get_result_selection() {
        Ok(sel) => read_selection(sel).unwrap_or(DbResultSelection::Rows),
        Err(_) => DbResultSelection::Rows,
    };
    let list = r.get_parameters().map_err(from_capnp)?;
    let mut parameters = Vec::with_capacity(list.len() as usize);
    for item in list.iter() {
        parameters.push(read_db_value(item)?);
    }
    Ok(TypedDbStatement {
        sql,
        parameters,
        kind,
        max_rows,
        result_selection,
    })
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when SQL text or a TEXT bind
/// contains U+0000.
pub(super) fn write_execute_request(
    mut b: execute_request_capnp::Builder<'_>,
    req: &ExecuteRequest,
) -> Result<()> {
    for stmt in &req.statements {
        crate::sql_text::require_portable_text(&stmt.sql)?;
        for param in &stmt.parameters {
            require_portable_value(param)?;
        }
    }
    b.set_operation_id(&req.operation_id);
    b.set_request_hash(&req.request_hash);
    b.set_deadline_unix_ms(req.deadline_unix_ms);
    let mut stmts = b.reborrow().init_statements(req.statements.len() as u32);
    for (i, s) in req.statements.iter().enumerate() {
        write_db_statement(stmts.reborrow().get(i as u32), s);
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the statement list is empty, or
/// [`PluginError::unsupported`] when a nested field cannot be decoded.
pub(super) fn read_execute_request(r: execute_request_capnp::Reader<'_>) -> Result<ExecuteRequest> {
    let list = r.get_statements().map_err(from_capnp)?;
    let mut statements = Vec::with_capacity(list.len() as usize);
    for item in list.iter() {
        statements.push(read_db_statement(item)?);
    }
    if statements.is_empty() {
        return Err(PluginError::invalid_params(
            "executeAtomic statements must be non-empty",
        ));
    }
    Ok(ExecuteRequest {
        operation_id: text_of(r.get_operation_id().map_err(from_capnp)?),
        request_hash: text_of(r.get_request_hash().map_err(from_capnp)?),
        statements,
        deadline_unix_ms: r.get_deadline_unix_ms(),
    })
}

fn write_column(mut b: db_column_capnp::Builder<'_>, col: &DbColumn) {
    b.set_name(&col.name);
    b.set_db_type(write_db_type(col.db_type));
}

/// # Errors
///
/// Returns when the column name or declared type cannot be decoded.
fn read_column(r: db_column_capnp::Reader<'_>) -> Result<DbColumn> {
    Ok(DbColumn {
        name: text_of(r.get_name().map_err(from_capnp)?),
        db_type: read_db_type(
            r.get_db_type()
                .map_err(|_| PluginError::unsupported("unknown DbType"))?,
        )?,
    })
}

fn write_row(mut b: db_row_capnp::Builder<'_>, row: &DbRow) {
    let mut vals = b.reborrow().init_values(row.values.len() as u32);
    for (i, v) in row.values.iter().enumerate() {
        write_db_value(vals.reborrow().get(i as u32), v);
    }
}

/// # Errors
///
/// Returns when a cell cannot be decoded as [`DbValue`].
fn read_row(r: db_row_capnp::Reader<'_>) -> Result<DbRow> {
    let list = r.get_values().map_err(from_capnp)?;
    let mut values = Vec::with_capacity(list.len() as usize);
    for item in list.iter() {
        values.push(read_db_value(item)?);
    }
    Ok(DbRow { values })
}

fn write_statement_result(mut b: statement_result_capnp::Builder<'_>, stmt: &StatementResult) {
    b.set_rows_affected(stmt.rows_affected);
    let mut cols = b.reborrow().init_columns(stmt.columns.len() as u32);
    for (i, c) in stmt.columns.iter().enumerate() {
        write_column(cols.reborrow().get(i as u32), c);
    }
    let mut rows = b.reborrow().init_rows(stmt.rows.len() as u32);
    for (i, row) in stmt.rows.iter().enumerate() {
        write_row(rows.reborrow().get(i as u32), row);
    }
}

/// # Errors
///
/// Returns when columns/rows cannot be decoded, a row width does not match
/// `columns.len()`, or column names are duplicated.
fn read_statement_result(r: statement_result_capnp::Reader<'_>) -> Result<StatementResult> {
    let cols = r.get_columns().map_err(from_capnp)?;
    let mut columns = Vec::with_capacity(cols.len() as usize);
    for item in cols.iter() {
        columns.push(read_column(item)?);
    }
    let rows_list = r.get_rows().map_err(from_capnp)?;
    let mut rows = Vec::with_capacity(rows_list.len() as usize);
    for item in rows_list.iter() {
        rows.push(read_row(item)?);
    }
    let result = StatementResult {
        rows,
        columns,
        rows_affected: r.get_rows_affected(),
    };
    result
        .validate_positional()
        .map_err(PluginError::invalid_params)?;
    Ok(result)
}

fn write_timing(mut b: db_timing_capnp::Builder<'_>, t: &DbTiming) {
    b.set_attempt_elapsed_us(t.attempt_elapsed_us);
    b.set_db_execution_us(t.db_execution_us);
    b.set_db_timing_source(&t.db_timing_source);
}

/// # Errors
///
/// Returns when the timing source string cannot be decoded.
fn read_timing(r: db_timing_capnp::Reader<'_>) -> Result<DbTiming> {
    Ok(DbTiming {
        attempt_elapsed_us: r.get_attempt_elapsed_us(),
        db_execution_us: r.get_db_execution_us(),
        db_timing_source: text_of(r.get_db_timing_source().map_err(from_capnp)?),
    })
}

pub(super) fn fill_execute_reply(mut b: execute_reply_capnp::Builder<'_>, reply: &ExecuteReply) {
    b.set_operation_id(&reply.operation_id);
    write_timing(b.reborrow().init_timing(), &reply.timing);
    let mut stmts = b.reborrow().init_statements(reply.statements.len() as u32);
    for (i, s) in reply.statements.iter().enumerate() {
        write_statement_result(stmts.reborrow().get(i as u32), s);
    }
}

/// # Errors
///
/// Returns when statements or timing cannot be decoded, or a statement result
/// fails positional validation.
pub(super) fn read_execute_reply(r: execute_reply_capnp::Reader<'_>) -> Result<ExecuteReply> {
    let list = r.get_statements().map_err(from_capnp)?;
    let mut statements = Vec::with_capacity(list.len() as usize);
    for item in list.iter() {
        statements.push(read_statement_result(item)?);
    }
    let timing = r
        .get_timing()
        .ok()
        .map(read_timing)
        .transpose()?
        .unwrap_or_default();
    Ok(ExecuteReply {
        operation_id: text_of(r.get_operation_id().map_err(from_capnp)?),
        statements,
        timing,
    })
}

pub(super) fn write_execute_result_reply(
    result: execute_result_reply::Builder<'_>,
    outcome: Result<ExecuteReply>,
) {
    match outcome {
        Ok(reply) => match require_portable_execute_reply(&reply) {
            Ok(()) => fill_execute_reply(result.init_ok(), &reply),
            Err(err) => write_error(result.init_err(), &err),
        },
        Err(err) => write_error(result.init_err(), &err),
    }
}

/// # Errors
///
/// Returns the guest [`PluginError`] on `err`, or a decode failure on `ok`.
pub(super) fn read_execute_result_reply(
    result: execute_result_reply::Reader<'_>,
) -> Result<ExecuteReply> {
    match result.which().map_err(from_capnp)? {
        execute_result_reply::Ok(ok) => read_execute_reply(ok.map_err(from_capnp)?),
        execute_result_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
    }
}

pub(super) fn write_db_capabilities(mut b: db_caps_capnp::Builder<'_>, caps: &DbCapabilities) {
    b.set_sql_contract_version(caps.sql_contract_version);
    b.set_atomic_batch(caps.atomic_batch);
    b.set_returning(caps.returning);
    b.set_affected_rows(caps.affected_rows);
    b.set_schema_migrations(caps.schema_migrations);
    b.set_pragma_user_version(caps.pragma_user_version);
    b.set_atomic_schema_batch(caps.atomic_schema_batch);
    b.set_cancellation(caps.cancellation);
    b.set_timing(caps.timing);
    b.set_max_binds(caps.max_binds);
    b.set_max_statements(caps.max_statements);
    b.set_max_result_rows(caps.max_result_rows);
    b.set_max_payload_bytes(caps.max_payload_bytes);
    b.set_max_result_bytes(caps.max_result_bytes);
    b.set_max_cell_bytes(caps.max_cell_bytes);
    b.set_max_request_bytes(caps.max_request_bytes);
    b.set_max_atomic_result_bytes(caps.max_atomic_result_bytes);
    b.set_plugin_databases(caps.plugin_databases);
    b.set_max_function_args(caps.max_function_args);
    b.set_max_schema_columns(caps.max_schema_columns);
    b.set_max_pattern_bytes(caps.max_pattern_bytes);
    b.set_max_lowered_statement_bytes(caps.max_lowered_statement_bytes);
    b.set_consistent_backup_read(caps.consistent_backup_read);
    b.set_atomic_unit_restore(caps.atomic_unit_restore);
}

/// Decodes negotiated database capabilities from a Cap'n Proto reader.
///
/// # Errors
///
/// Currently infallible for well-formed readers; reserved for future validation.
pub(super) fn read_db_capabilities(r: db_caps_capnp::Reader<'_>) -> Result<DbCapabilities> {
    Ok(DbCapabilities {
        sql_contract_version: r.get_sql_contract_version(),
        atomic_batch: r.get_atomic_batch(),
        returning: r.get_returning(),
        affected_rows: r.get_affected_rows(),
        schema_migrations: r.get_schema_migrations(),
        pragma_user_version: r.get_pragma_user_version(),
        atomic_schema_batch: r.get_atomic_schema_batch(),
        cancellation: r.get_cancellation(),
        timing: r.get_timing(),
        max_binds: r.get_max_binds(),
        max_statements: r.get_max_statements(),
        max_result_rows: r.get_max_result_rows(),
        max_payload_bytes: r.get_max_payload_bytes(),
        max_result_bytes: r.get_max_result_bytes(),
        max_cell_bytes: r.get_max_cell_bytes(),
        max_request_bytes: r.get_max_request_bytes(),
        max_atomic_result_bytes: r.get_max_atomic_result_bytes(),
        plugin_databases: r.get_plugin_databases(),
        max_function_args: r.get_max_function_args(),
        max_schema_columns: r.get_max_schema_columns(),
        max_pattern_bytes: r.get_max_pattern_bytes(),
        max_lowered_statement_bytes: r.get_max_lowered_statement_bytes(),
        consistent_backup_read: r.get_consistent_backup_read(),
        atomic_unit_restore: r.get_atomic_unit_restore(),
    })
}

pub(super) fn write_db_capabilities_reply(
    result: db_capabilities_reply::Builder<'_>,
    outcome: Result<DbCapabilities>,
) {
    match outcome {
        Ok(caps) => write_db_capabilities(result.init_ok(), &caps),
        Err(err) => write_error(result.init_err(), &err),
    }
}

/// # Errors
///
/// Returns the guest [`PluginError`] on `err`, or a decode failure on `ok`.
pub(super) fn read_db_capabilities_reply(
    result: db_capabilities_reply::Reader<'_>,
) -> Result<DbCapabilities> {
    match result.which().map_err(from_capnp)? {
        db_capabilities_reply::Ok(ok) => read_db_capabilities(ok.map_err(from_capnp)?),
        db_capabilities_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
    }
}

/// Writes bootstrap-only SeaORM proxy metadata into a Cap'n builder.
pub(super) fn write_db_bootstrap(mut b: db_bootstrap_capnp::Builder<'_>, bootstrap: &DbBootstrap) {
    b.set_sql_family(&bootstrap.sql_family);
    b.set_dialect(&bootstrap.dialect);
}

/// Decodes bootstrap-only SeaORM proxy metadata from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a decode failure when Cap'n text fields cannot be read.
pub(super) fn read_db_bootstrap(r: db_bootstrap_capnp::Reader<'_>) -> Result<DbBootstrap> {
    Ok(DbBootstrap {
        sql_family: text_of(r.get_sql_family().map_err(from_capnp)?),
        dialect: text_of(r.get_dialect().map_err(from_capnp)?),
    })
}

/// Writes a `DbBootstrapReply` union (`ok` or guest `err`).
pub(super) fn write_db_bootstrap_reply(
    result: db_bootstrap_reply::Builder<'_>,
    outcome: Result<DbBootstrap>,
) {
    match outcome {
        Ok(bootstrap) => write_db_bootstrap(result.init_ok(), &bootstrap),
        Err(err) => write_error(result.init_err(), &err),
    }
}

/// # Errors
///
/// Returns the guest [`PluginError`] on `err`, or a decode failure on `ok`.
pub(super) fn read_db_bootstrap_reply(
    result: db_bootstrap_reply::Reader<'_>,
) -> Result<DbBootstrap> {
    match result.which().map_err(from_capnp)? {
        db_bootstrap_reply::Ok(ok) => read_db_bootstrap(ok.map_err(from_capnp)?),
        db_bootstrap_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
    }
}

/// Serializes a Cap'n message to unpacked stream bytes.
///
/// # Errors
///
/// Returns [`PluginError::internal`] when serialization fails.
fn write_message_bytes(
    message: &capnp::message::Builder<capnp::message::HeapAllocator>,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    capnp::serialize::write_message(&mut out, message).map_err(from_capnp)?;
    Ok(out)
}

/// Encodes a standalone Cap'n `DbValue` message (unpacked stream).
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a [`DbValue::Text`] contains
/// U+0000, or [`PluginError::internal`] when the message cannot be serialized.
pub fn encoded_db_value_bytes(v: &DbValue) -> Result<Vec<u8>> {
    require_portable_value(v)?;
    let mut message = capnp::message::Builder::new_default();
    write_db_value(message.init_root(), v);
    write_message_bytes(&message)
}

/// Encodes a standalone Cap'n `ExecuteRequest` message (unpacked stream).
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when SQL or TEXT binds contain
/// U+0000, or [`PluginError::internal`] when the message cannot be serialized.
pub fn encoded_execute_request_bytes(req: &ExecuteRequest) -> Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    write_execute_request(message.init_root(), req)?;
    write_message_bytes(&message)
}

/// SHA-256 hex of the Cap'n-encoded request with transport metadata cleared.
///
/// `operationId`, `requestHash`, and `deadlineUnixMs` are not part of the
/// digest: a retry of the same mutation may mint a remaining deadline without
/// hash-conflicting. The trusted host stamps `requestHash` after validation
/// so guests cannot collide unrelated mutations on one idempotency key.
///
/// # Errors
///
/// Returns [`PluginError::internal`] when the message cannot be serialized.
pub fn canonical_execute_request_hash(req: &ExecuteRequest) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut canonical = req.clone();
    canonical.operation_id.clear();
    canonical.request_hash.clear();
    canonical.deadline_unix_ms = 0;
    let bytes = crate::sdk_wire::encoded_execute_request_sdk_bytes(&canonical)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

/// Encodes a standalone Cap'n `ExecuteReply` message (unpacked stream).
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a TEXT cell contains U+0000,
/// or [`PluginError::internal`] when the message cannot be serialized.
pub fn encoded_execute_reply_bytes(reply: &ExecuteReply) -> Result<Vec<u8>> {
    require_portable_execute_reply(reply)?;
    let mut message = capnp::message::Builder::new_default();
    fill_execute_reply(message.init_root(), reply);
    write_message_bytes(&message)
}

/// Encodes a standalone Cap'n `ExecuteAtomicReply` (`ok` or `err`).
///
/// # Errors
///
/// Returns [`PluginError::internal`] when the message cannot be serialized.
pub fn encoded_execute_result_reply_bytes(outcome: Result<ExecuteReply>) -> Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    write_execute_result_reply(message.init_root(), outcome);
    write_message_bytes(&message)
}

/// Reads one unpacked single-segment Cap'n message.
///
/// Rejects multi-segment streams (matching the TypeScript SDK) and caps
/// traversal to the input size so a small buffer cannot expand into a large
/// graph. Intra-segment far pointers remain allowed so rustc-encoded messages
/// still round-trip.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the stream is truncated or
/// multi-segment, and [`PluginError::unavailable`] when Cap'n decode fails.
fn read_unpacked_message(
    bytes: &[u8],
) -> Result<capnp::message::Reader<capnp::serialize::OwnedSegments>> {
    if bytes.len() < 8 {
        return Err(PluginError::invalid_params("truncated Cap'n message"));
    }
    let mut nseg_minus_bytes = [0u8; 4];
    nseg_minus_bytes.copy_from_slice(&bytes[..4]);
    let nseg = u32::from_le_bytes(nseg_minus_bytes).saturating_add(1);
    if nseg != 1 {
        return Err(PluginError::invalid_params(
            "multi-segment Cap'n messages are not supported",
        ));
    }
    let words = (bytes.len() / 8)
        .saturating_add(16)
        .min(usize::try_from(MAX_SCALAR_BYTES).unwrap_or(usize::MAX) / 8);
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(words));
    opts.nesting_limit(32);
    let mut cursor = std::io::Cursor::new(bytes);
    capnp::serialize::read_message(&mut cursor, opts).map_err(from_capnp)
}

/// Decodes a standalone Cap'n `ExecuteAtomicReply` message.
///
/// # Errors
///
/// Returns the guest [`PluginError`] on `err`, or a decode failure on `ok`.
pub fn decode_execute_result_reply_bytes(bytes: &[u8]) -> Result<ExecuteReply> {
    let reader = read_unpacked_message(bytes)?;
    read_execute_result_reply(reader.get_root().map_err(from_capnp)?)
}

/// Encodes a standalone Cap'n `StatementResult` message (unpacked stream).
///
/// Used to enforce negotiated `maxResultBytes` against the actual wire size of
/// one statement before COMMIT.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a TEXT cell contains U+0000,
/// or [`PluginError::internal`] when the message cannot be serialized.
pub fn encoded_statement_result_bytes(stmt: &StatementResult) -> Result<Vec<u8>> {
    require_portable_statement_result(stmt)?;
    let mut message = capnp::message::Builder::new_default();
    write_statement_result(message.init_root(), stmt);
    write_message_bytes(&message)
}

/// Decodes a standalone Cap'n `DbValue` message.
///
/// # Errors
///
/// Returns when the buffer is not a valid unpacked `DbValue`.
pub fn decode_db_value_bytes(bytes: &[u8]) -> Result<DbValue> {
    let reader = read_unpacked_message(bytes)?;
    read_db_value(reader.get_root().map_err(from_capnp)?)
}

/// Decodes a standalone Cap'n `ExecuteRequest` message.
///
/// # Errors
///
/// Returns when the buffer is not a valid unpacked `ExecuteRequest`.
pub fn decode_execute_request_bytes(bytes: &[u8]) -> Result<ExecuteRequest> {
    let reader = read_unpacked_message(bytes)?;
    read_execute_request(reader.get_root().map_err(from_capnp)?)
}

#[cfg(feature = "host")]
pub(super) fn write_guest_receipt_persist(
    mut b: host_guest_receipt_persist_capnp::Builder<'_>,
    receipt: &GuestReceiptPersist,
) {
    b.set_guest_len(receipt.guest_statement_len);
    b.set_guest_hash(&receipt.guest_request_hash);
}

/// # Errors
///
/// Returns when nested Cap'n fields cannot be decoded.
#[cfg(feature = "host")]
fn read_guest_receipt_persist(
    r: host_guest_receipt_persist_capnp::Reader<'_>,
) -> Result<GuestReceiptPersist> {
    Ok(GuestReceiptPersist {
        guest_statement_len: r.get_guest_len(),
        guest_request_hash: text_of(r.get_guest_hash().map_err(from_capnp)?),
    })
}

/// Writes a host-private execute envelope, including stamped proofs.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the nested request has U+0000
/// in SQL or TEXT binds.
///
/// # Panics
///
/// Panics when `envelope.proofs` cannot be serialized. Proof types are
/// serde-derived; failure is a programming error rather than a transport
/// fallback to an empty sidecar.
#[cfg(feature = "host")]
pub(super) fn write_host_execute_envelope(
    mut b: host_execute_envelope_capnp::Builder<'_>,
    envelope: &HostExecuteEnvelope,
) -> Result<()> {
    write_execute_request(b.reborrow().init_request(), &envelope.request)?;
    write_guest_receipt_persist(b.reborrow().init_guest_receipt(), &envelope.guest_receipt);
    let proofs_json = serde_json::to_string(&envelope.proofs)
        .expect("ResolvedStatement proofs are serde-serializable");
    b.set_proofs_json(&proofs_json);
    Ok(())
}

/// # Errors
///
/// Returns when nested fields cannot be decoded.
#[cfg(feature = "host")]
pub(super) fn read_host_execute_envelope(
    r: host_execute_envelope_capnp::Reader<'_>,
) -> Result<HostExecuteEnvelope> {
    let request = read_execute_request(r.get_request().map_err(from_capnp)?)?;
    let guest_receipt = read_guest_receipt_persist(r.get_guest_receipt().map_err(from_capnp)?)?;
    let json = text_of(r.get_proofs_json().map_err(from_capnp)?);
    let proofs = if json.is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(&json).map_err(|err| {
            PluginError::internal(format!("host execute envelope proofsJson: {err}"))
        })?
    };
    Ok(HostExecuteEnvelope {
        request,
        guest_receipt,
        proofs,
    })
}
