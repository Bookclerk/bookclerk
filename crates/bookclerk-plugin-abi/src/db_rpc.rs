//! Cap'n Proto codec for typed database execute / capabilities.

#![allow(clippy::missing_docs_in_private_items, clippy::missing_errors_doc)]

use crate::host_envelope::{AdapterExecuteRequest, GuestReceiptPersist};
use crate::plugin_capnp::{
    adapter_execute_request as adapter_execute_request_capnp,
    adapter_receipt as adapter_receipt_capnp, adapter_statement as adapter_statement_capnp,
    create_table_schema as create_table_schema_capnp, db_bootstrap as db_bootstrap_capnp,
    db_bootstrap_reply, db_capabilities as db_caps_capnp, db_capabilities_reply,
    db_column as db_column_capnp, db_row as db_row_capnp, db_statement as db_statement_capnp,
    db_timing as db_timing_capnp, db_value as db_value_capnp, execute_reply as execute_reply_capnp,
    execute_request as execute_request_capnp, execute_result_reply, identity_export_reply,
    identity_high_water as identity_high_water_capnp,
    integer_arith_site as integer_arith_site_capnp, named_sql_type as named_sql_type_capnp,
    optional_column_reference as optional_column_reference_capnp,
    resolved_statement as resolved_statement_capnp, schema_action as schema_action_capnp,
    sql_span as sql_span_capnp, statement_result as statement_result_capnp,
    table_constraint as table_constraint_capnp, user_relations_reply,
    DbResultSelection as CapnpDbResultSelection, DbStatementKind as CapnpDbStatementKind,
    DbType as CapnpDbType, IntegerArithKind as CapnpIntegerArithKind,
    IsolationReq as CapnpIsolationReq, ResolvedSqlType as CapnpResolvedSqlType,
};
use crate::rpc::{from_capnp, read_error, text_of, write_error};
use crate::{
    DbBootstrap, DbCapabilities, DbColumn, DbIdentityHighWater, DbPlanStatementKind,
    DbResultSelection, DbRow, DbTiming, DbType, DbValue, ExecuteReply, ExecuteRequest,
    IsolationReq, PluginError, Result, StatementResult, TypedDbStatement, MAX_SCALAR_BYTES,
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
    b.set_consistent_backup_read(caps.consistent_backup_read);
    b.set_atomic_unit_restore(caps.atomic_unit_restore);
    b.set_max_function_args(caps.max_function_args);
    b.set_max_schema_columns(caps.max_schema_columns);
    b.set_max_pattern_bytes(caps.max_pattern_bytes);
    b.set_max_lowered_statement_bytes(caps.max_lowered_statement_bytes);
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
        consistent_backup_read: r.get_consistent_backup_read(),
        atomic_unit_restore: r.get_atomic_unit_restore(),
        max_function_args: r.get_max_function_args(),
        max_schema_columns: r.get_max_schema_columns(),
        max_pattern_bytes: r.get_max_pattern_bytes(),
        max_lowered_statement_bytes: r.get_max_lowered_statement_bytes(),
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
    b.set_engine(&bootstrap.engine);
}

/// Decodes bootstrap-only diagnostic engine identity from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns a decode failure when Cap'n text fields cannot be read.
pub(super) fn read_db_bootstrap(r: db_bootstrap_capnp::Reader<'_>) -> Result<DbBootstrap> {
    Ok(DbBootstrap {
        engine: text_of(r.get_engine().map_err(from_capnp)?),
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

pub(super) fn write_adapter_execute_request(
    mut b: adapter_execute_request_capnp::Builder<'_>,
    req: &AdapterExecuteRequest,
) -> Result<()> {
    b.set_operation_id(&req.request.operation_id);
    b.set_request_hash(&req.request.request_hash);
    b.set_deadline_unix_ms(req.request.deadline_unix_ms);
    b.set_isolation(write_isolation(req.isolation));
    write_adapter_receipt(b.reborrow().init_receipt(), &req.guest_receipt);
    let n = req.request.statements.len() as u32;
    let mut stmts = b.reborrow().init_statements(n);
    for (i, stmt) in req.request.statements.iter().enumerate() {
        match req.proofs.get(i) {
            Some(proof) => {
                write_adapter_statement(stmts.reborrow().get(i as u32), stmt, proof)?;
            }
            None => {
                // Empty hash so [`AdapterExecuteRequest::require_proofs`] fails
                // closed instead of inventing a hash-bound empty proof.
                let mut dummy = crate::sql_proof::ResolvedStatement::bound_empty(&stmt.sql);
                dummy.statement_hash.clear();
                write_adapter_statement(stmts.reborrow().get(i as u32), stmt, &dummy)?;
            }
        }
    }
    Ok(())
}

/// # Errors
///
/// Returns when statements, proofs, or isolation cannot be decoded.
pub(super) fn read_adapter_execute_request(
    r: adapter_execute_request_capnp::Reader<'_>,
) -> Result<AdapterExecuteRequest> {
    let list = r.get_statements().map_err(from_capnp)?;
    if list.is_empty() {
        return Err(PluginError::invalid_params(
            "executeAtomic statements must be non-empty",
        ));
    }
    let mut statements = Vec::with_capacity(list.len() as usize);
    let mut proofs = Vec::with_capacity(list.len() as usize);
    for item in list.iter() {
        let (stmt, proof) = read_adapter_statement(item)?;
        statements.push(stmt);
        proofs.push(proof);
    }
    let isolation = read_isolation(
        r.get_isolation()
            .map_err(|_| PluginError::unsupported("unknown IsolationReq"))?,
    )?;
    let guest_receipt = read_adapter_receipt(r.get_receipt().map_err(from_capnp)?)?;
    Ok(AdapterExecuteRequest {
        request: ExecuteRequest {
            operation_id: text_of(r.get_operation_id().map_err(from_capnp)?),
            request_hash: text_of(r.get_request_hash().map_err(from_capnp)?),
            statements,
            deadline_unix_ms: r.get_deadline_unix_ms(),
        },
        guest_receipt,
        proofs,
        isolation,
    })
}

pub(crate) fn write_isolation(iso: IsolationReq) -> CapnpIsolationReq {
    match iso {
        IsolationReq::AtomicBatch => CapnpIsolationReq::AtomicBatch,
        IsolationReq::NestedSavepoint => CapnpIsolationReq::NestedSavepoint,
        IsolationReq::ConsistentSnapshot => CapnpIsolationReq::ConsistentSnapshot,
    }
}

pub(crate) fn read_isolation(iso: CapnpIsolationReq) -> Result<IsolationReq> {
    match iso {
        CapnpIsolationReq::AtomicBatch => Ok(IsolationReq::AtomicBatch),
        CapnpIsolationReq::NestedSavepoint => Ok(IsolationReq::NestedSavepoint),
        CapnpIsolationReq::ConsistentSnapshot => Ok(IsolationReq::ConsistentSnapshot),
    }
}

fn write_adapter_receipt(mut b: adapter_receipt_capnp::Builder<'_>, receipt: &GuestReceiptPersist) {
    b.set_guest_len(receipt.guest_statement_len);
    b.set_guest_hash(&receipt.guest_request_hash);
}

fn read_adapter_receipt(r: adapter_receipt_capnp::Reader<'_>) -> Result<GuestReceiptPersist> {
    Ok(GuestReceiptPersist {
        guest_statement_len: r.get_guest_len(),
        guest_request_hash: text_of(r.get_guest_hash().map_err(from_capnp)?),
    })
}

fn write_adapter_statement(
    mut b: adapter_statement_capnp::Builder<'_>,
    stmt: &TypedDbStatement,
    proof: &crate::sql_proof::ResolvedStatement,
) -> Result<()> {
    crate::sql_text::require_portable_text(&stmt.sql)?;
    for param in &stmt.parameters {
        require_portable_value(param)?;
    }
    b.set_sql(&stmt.sql);
    b.set_kind(write_kind(stmt.kind));
    b.set_max_rows(stmt.max_rows);
    b.set_result_selection(write_selection(stmt.result_selection));
    let mut params = b.reborrow().init_parameters(stmt.parameters.len() as u32);
    for (i, p) in stmt.parameters.iter().enumerate() {
        write_db_value(params.reborrow().get(i as u32), p);
    }
    write_resolved_statement(b.reborrow().init_proof(), proof);
    Ok(())
}

fn read_adapter_statement(
    r: adapter_statement_capnp::Reader<'_>,
) -> Result<(TypedDbStatement, crate::sql_proof::ResolvedStatement)> {
    let stmt = read_db_statement_fields(
        text_of(r.get_sql().map_err(from_capnp)?),
        r.get_kind()
            .map_err(|_| PluginError::unsupported("unknown DbStatementKind"))?,
        r.get_max_rows(),
        r.get_result_selection(),
        r.get_parameters().map_err(from_capnp)?,
    )?;
    let proof = read_resolved_statement(r.get_proof().map_err(from_capnp)?)?;
    Ok((stmt, proof))
}

fn read_db_statement_fields(
    sql: String,
    kind: CapnpDbStatementKind,
    max_rows: u32,
    result_selection: std::result::Result<CapnpDbResultSelection, capnp::NotInSchema>,
    list: capnp::struct_list::Reader<db_value_capnp::Owned>,
) -> Result<TypedDbStatement> {
    let kind = read_kind(kind)?;
    let result_selection = match result_selection {
        Ok(sel) => read_selection(sel).unwrap_or(DbResultSelection::Rows),
        Err(_) => DbResultSelection::Rows,
    };
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

fn write_resolved_sql_type(ty: crate::SqlType) -> CapnpResolvedSqlType {
    match ty {
        crate::SqlType::Integer => CapnpResolvedSqlType::Integer,
        crate::SqlType::Real => CapnpResolvedSqlType::Real,
        crate::SqlType::Text => CapnpResolvedSqlType::Text,
        crate::SqlType::Blob => CapnpResolvedSqlType::Blob,
        crate::SqlType::Boolean => CapnpResolvedSqlType::Boolean,
        crate::SqlType::Null => CapnpResolvedSqlType::Null,
    }
}

fn read_resolved_sql_type(ty: CapnpResolvedSqlType) -> Result<crate::SqlType> {
    match ty {
        CapnpResolvedSqlType::Integer => Ok(crate::SqlType::Integer),
        CapnpResolvedSqlType::Real => Ok(crate::SqlType::Real),
        CapnpResolvedSqlType::Text => Ok(crate::SqlType::Text),
        CapnpResolvedSqlType::Blob => Ok(crate::SqlType::Blob),
        CapnpResolvedSqlType::Boolean => Ok(crate::SqlType::Boolean),
        CapnpResolvedSqlType::Null => Ok(crate::SqlType::Null),
    }
}

fn write_sql_span(mut b: sql_span_capnp::Builder<'_>, span: crate::sql_proof::SqlSpan) {
    b.set_start(u32::try_from(span.start).unwrap_or(u32::MAX));
    b.set_end(u32::try_from(span.end).unwrap_or(u32::MAX));
}

fn read_sql_span(r: sql_span_capnp::Reader<'_>) -> crate::sql_proof::SqlSpan {
    crate::sql_proof::SqlSpan {
        start: r.get_start() as usize,
        end: r.get_end() as usize,
    }
}

fn write_named_sql_type(mut b: named_sql_type_capnp::Builder<'_>, name: &str, ty: crate::SqlType) {
    b.set_name(name);
    b.set_sql_type(write_resolved_sql_type(ty));
}

fn read_named_sql_type(r: named_sql_type_capnp::Reader<'_>) -> Result<(String, crate::SqlType)> {
    Ok((
        text_of(r.get_name().map_err(from_capnp)?),
        read_resolved_sql_type(
            r.get_sql_type()
                .map_err(|_| PluginError::unsupported("unknown ResolvedSqlType"))?,
        )?,
    ))
}

fn write_resolved_statement(
    mut b: resolved_statement_capnp::Builder<'_>,
    proof: &crate::sql_proof::ResolvedStatement,
) {
    b.set_statement_hash(&proof.statement_hash);
    let mut cols = b
        .reborrow()
        .init_output_columns(proof.output_columns.len() as u32);
    for (i, (name, ty)) in proof.output_columns.iter().enumerate() {
        write_named_sql_type(cols.reborrow().get(i as u32), name, *ty);
    }
    let mut acc = b
        .reborrow()
        .init_physical_accesses(proof.physical_accesses.len() as u32);
    for (i, a) in proof.physical_accesses.iter().enumerate() {
        let mut slot = acc.reborrow().get(i as u32);
        slot.set_table(&a.table);
        slot.set_column(a.column.as_deref().unwrap_or(""));
    }
    let mut assigns = b
        .reborrow()
        .init_assignments(proof.assignments.len() as u32);
    for (i, a) in proof.assignments.iter().enumerate() {
        let mut slot = assigns.reborrow().get(i as u32);
        slot.set_table(&a.table);
        slot.set_column(&a.column);
        slot.set_dest(write_resolved_sql_type(a.dest));
        slot.set_source(write_resolved_sql_type(a.source));
    }
    let mut collate = b
        .reborrow()
        .init_text_collate_sites(proof.text_collate_sites.len() as u32);
    for (i, s) in proof.text_collate_sites.iter().enumerate() {
        write_sql_span(collate.reborrow().get(i as u32).init_span(), s.span);
    }
    let mut arith = b
        .reborrow()
        .init_integer_arith_sites(proof.integer_arith_sites.len() as u32);
    for (i, s) in proof.integer_arith_sites.iter().enumerate() {
        write_integer_arith_site(arith.reborrow().get(i as u32), s);
    }
    let mut fns = b.reborrow().init_functions(proof.functions.len() as u32);
    for (i, name) in proof.functions.iter().enumerate() {
        fns.set(i as u32, name);
    }
    write_schema_action(b.reborrow().init_schema_action(), &proof.schema_action);
}

fn read_resolved_statement(
    r: resolved_statement_capnp::Reader<'_>,
) -> Result<crate::sql_proof::ResolvedStatement> {
    let mut output_columns = Vec::new();
    for item in r.get_output_columns().map_err(from_capnp)?.iter() {
        output_columns.push(read_named_sql_type(item)?);
    }
    let mut physical_accesses = Vec::new();
    for item in r.get_physical_accesses().map_err(from_capnp)?.iter() {
        let column = text_of(item.get_column().map_err(from_capnp)?);
        physical_accesses.push(crate::sql_proof::PhysicalAccess {
            table: text_of(item.get_table().map_err(from_capnp)?),
            column: if column.is_empty() {
                None
            } else {
                Some(column)
            },
        });
    }
    let mut assignments = Vec::new();
    for item in r.get_assignments().map_err(from_capnp)?.iter() {
        assignments.push(crate::sql_proof::ResolvedAssignment {
            table: text_of(item.get_table().map_err(from_capnp)?),
            column: text_of(item.get_column().map_err(from_capnp)?),
            dest: read_resolved_sql_type(
                item.get_dest()
                    .map_err(|_| PluginError::unsupported("unknown ResolvedSqlType"))?,
            )?,
            source: read_resolved_sql_type(
                item.get_source()
                    .map_err(|_| PluginError::unsupported("unknown ResolvedSqlType"))?,
            )?,
        });
    }
    let mut text_collate_sites = Vec::new();
    for item in r.get_text_collate_sites().map_err(from_capnp)?.iter() {
        text_collate_sites.push(crate::sql_proof::TextCollateSite {
            span: read_sql_span(item.get_span().map_err(from_capnp)?),
        });
    }
    let mut integer_arith_sites = Vec::new();
    for item in r.get_integer_arith_sites().map_err(from_capnp)?.iter() {
        integer_arith_sites.push(read_integer_arith_site(item)?);
    }
    let mut functions = Vec::new();
    for item in r.get_functions().map_err(from_capnp)?.iter() {
        functions.push(text_of(item.map_err(from_capnp)?));
    }
    Ok(crate::sql_proof::ResolvedStatement {
        statement_hash: text_of(r.get_statement_hash().map_err(from_capnp)?),
        output_columns,
        physical_accesses,
        assignments,
        text_collate_sites,
        integer_arith_sites,
        functions,
        schema_action: read_schema_action(r.get_schema_action().map_err(from_capnp)?)?,
    })
}

fn write_integer_arith_site(
    mut b: integer_arith_site_capnp::Builder<'_>,
    site: &crate::sql_proof::IntegerArithSite,
) {
    write_sql_span(b.reborrow().init_full(), site.full);
    write_sql_span(b.reborrow().init_lhs(), site.lhs);
    write_sql_span(b.reborrow().init_rhs(), site.rhs);
    b.set_kind(match site.kind {
        crate::sql_proof::IntegerArithKind::Add => CapnpIntegerArithKind::Add,
        crate::sql_proof::IntegerArithKind::Sub => CapnpIntegerArithKind::Sub,
        crate::sql_proof::IntegerArithKind::Mul => CapnpIntegerArithKind::Mul,
        crate::sql_proof::IntegerArithKind::Abs => CapnpIntegerArithKind::Abs,
    });
}

fn read_integer_arith_site(
    r: integer_arith_site_capnp::Reader<'_>,
) -> Result<crate::sql_proof::IntegerArithSite> {
    let kind = match r
        .get_kind()
        .map_err(|_| PluginError::unsupported("unknown IntegerArithKind"))?
    {
        CapnpIntegerArithKind::Add => crate::sql_proof::IntegerArithKind::Add,
        CapnpIntegerArithKind::Sub => crate::sql_proof::IntegerArithKind::Sub,
        CapnpIntegerArithKind::Mul => crate::sql_proof::IntegerArithKind::Mul,
        CapnpIntegerArithKind::Abs => crate::sql_proof::IntegerArithKind::Abs,
    };
    Ok(crate::sql_proof::IntegerArithSite {
        full: read_sql_span(r.get_full().map_err(from_capnp)?),
        lhs: read_sql_span(r.get_lhs().map_err(from_capnp)?),
        rhs: read_sql_span(r.get_rhs().map_err(from_capnp)?),
        kind,
    })
}

fn write_schema_action(
    mut b: schema_action_capnp::Builder<'_>,
    action: &crate::sql_proof::SchemaAction,
) {
    match action {
        crate::sql_proof::SchemaAction::None => {
            b.set_none(());
        }
        crate::sql_proof::SchemaAction::Create {
            schema,
            fingerprint,
            noop,
        } => {
            let mut c = b.init_create();
            c.set_fingerprint(fingerprint);
            c.set_noop(*noop);
            write_create_table_schema(c.init_schema(), schema);
        }
        crate::sql_proof::SchemaAction::Drop { table } => {
            b.set_drop(table);
        }
    }
}

fn read_schema_action(
    r: schema_action_capnp::Reader<'_>,
) -> Result<crate::sql_proof::SchemaAction> {
    match r.which().map_err(from_capnp)? {
        schema_action_capnp::None(()) => Ok(crate::sql_proof::SchemaAction::None),
        schema_action_capnp::Create(c) => {
            let c = c.map_err(from_capnp)?;
            Ok(crate::sql_proof::SchemaAction::Create {
                schema: Box::new(read_create_table_schema(
                    c.get_schema().map_err(from_capnp)?,
                )?),
                fingerprint: text_of(c.get_fingerprint().map_err(from_capnp)?),
                noop: c.get_noop(),
            })
        }
        schema_action_capnp::Drop(t) => Ok(crate::sql_proof::SchemaAction::Drop {
            table: text_of(t.map_err(from_capnp)?),
        }),
    }
}

fn write_create_table_schema(
    mut b: create_table_schema_capnp::Builder<'_>,
    schema: &crate::CreateTableSchema,
) {
    b.set_table(&schema.table);
    b.set_identity_column(schema.identity_column.as_deref().unwrap_or(""));
    let mut cols = b.reborrow().init_columns(schema.columns.len() as u32);
    for (i, (name, ty)) in schema.columns.iter().enumerate() {
        write_named_sql_type(cols.reborrow().get(i as u32), name, *ty);
    }
    write_bool_list(
        b.reborrow()
            .init_column_not_null(schema.column_not_null.len() as u32),
        &schema.column_not_null,
    );
    write_bool_list(
        b.reborrow()
            .init_column_unique(schema.column_unique.len() as u32),
        &schema.column_unique,
    );
    write_bool_list(
        b.reborrow()
            .init_column_primary_key(schema.column_primary_key.len() as u32),
        &schema.column_primary_key,
    );
    write_text_list(
        b.reborrow()
            .init_column_defaults(schema.column_defaults.len() as u32),
        &schema.column_defaults,
    );
    write_text_list(
        b.reborrow()
            .init_column_checks(schema.column_checks.len() as u32),
        &schema.column_checks,
    );
    let mut refs = b
        .reborrow()
        .init_column_references(schema.column_references.len() as u32);
    for (i, r) in schema.column_references.iter().enumerate() {
        write_optional_column_ref(refs.reborrow().get(i as u32), r.as_ref());
    }
    let mut cons = b
        .reborrow()
        .init_table_constraints(schema.table_constraints.len() as u32);
    for (i, c) in schema.table_constraints.iter().enumerate() {
        write_table_constraint(cons.reborrow().get(i as u32), c);
    }
}

fn read_create_table_schema(
    r: create_table_schema_capnp::Reader<'_>,
) -> Result<crate::CreateTableSchema> {
    let mut columns = Vec::new();
    for item in r.get_columns().map_err(from_capnp)?.iter() {
        columns.push(read_named_sql_type(item)?);
    }
    let identity = text_of(r.get_identity_column().map_err(from_capnp)?);
    let mut column_references = Vec::new();
    for item in r.get_column_references().map_err(from_capnp)?.iter() {
        column_references.push(read_optional_column_ref(item)?);
    }
    let mut table_constraints = Vec::new();
    for item in r.get_table_constraints().map_err(from_capnp)?.iter() {
        table_constraints.push(read_table_constraint(item)?);
    }
    Ok(crate::CreateTableSchema {
        table: text_of(r.get_table().map_err(from_capnp)?),
        columns,
        identity_column: if identity.is_empty() {
            None
        } else {
            Some(identity)
        },
        column_not_null: read_bool_list(r.get_column_not_null().map_err(from_capnp)?)?,
        column_unique: read_bool_list(r.get_column_unique().map_err(from_capnp)?)?,
        column_primary_key: read_bool_list(r.get_column_primary_key().map_err(from_capnp)?)?,
        column_defaults: read_text_list(r.get_column_defaults().map_err(from_capnp)?)?,
        column_checks: read_text_list(r.get_column_checks().map_err(from_capnp)?)?,
        column_references,
        table_constraints,
    })
}

fn write_optional_column_ref(
    mut b: optional_column_reference_capnp::Builder<'_>,
    r: Option<&crate::ColumnReference>,
) {
    match r {
        None => b.set_none(()),
        Some(r) => {
            let mut s = b.init_some();
            s.set_ref_table(&r.ref_table);
            write_text_list(
                s.reborrow().init_ref_columns(r.ref_columns.len() as u32),
                &r.ref_columns,
            );
        }
    }
}

fn read_optional_column_ref(
    r: optional_column_reference_capnp::Reader<'_>,
) -> Result<Option<crate::ColumnReference>> {
    match r.which().map_err(from_capnp)? {
        optional_column_reference_capnp::None(()) => Ok(None),
        optional_column_reference_capnp::Some(s) => {
            let s = s.map_err(from_capnp)?;
            Ok(Some(crate::ColumnReference {
                ref_table: text_of(s.get_ref_table().map_err(from_capnp)?),
                ref_columns: read_text_list(s.get_ref_columns().map_err(from_capnp)?)?,
            }))
        }
    }
}

fn write_table_constraint(
    mut b: table_constraint_capnp::Builder<'_>,
    c: &crate::sql_types::TableConstraint,
) {
    match c {
        crate::sql_types::TableConstraint::PrimaryKey(cols) => {
            write_text_list(b.init_primary_key(cols.len() as u32), cols);
        }
        crate::sql_types::TableConstraint::Unique(cols) => {
            write_text_list(b.init_unique(cols.len() as u32), cols);
        }
        crate::sql_types::TableConstraint::Check(sql) => b.set_check(sql),
        crate::sql_types::TableConstraint::ForeignKey {
            columns,
            ref_table,
            ref_columns,
        } => {
            let mut fk = b.init_foreign_key();
            write_text_list(fk.reborrow().init_columns(columns.len() as u32), columns);
            fk.set_ref_table(ref_table);
            write_text_list(
                fk.reborrow().init_ref_columns(ref_columns.len() as u32),
                ref_columns,
            );
        }
    }
}

fn read_table_constraint(
    r: table_constraint_capnp::Reader<'_>,
) -> Result<crate::sql_types::TableConstraint> {
    match r.which().map_err(from_capnp)? {
        table_constraint_capnp::PrimaryKey(cols) => {
            Ok(crate::sql_types::TableConstraint::PrimaryKey(
                read_text_list(cols.map_err(from_capnp)?)?,
            ))
        }
        table_constraint_capnp::Unique(cols) => Ok(crate::sql_types::TableConstraint::Unique(
            read_text_list(cols.map_err(from_capnp)?)?,
        )),
        table_constraint_capnp::Check(sql) => Ok(crate::sql_types::TableConstraint::Check(
            text_of(sql.map_err(from_capnp)?),
        )),
        table_constraint_capnp::ForeignKey(fk) => {
            let fk = fk.map_err(from_capnp)?;
            Ok(crate::sql_types::TableConstraint::ForeignKey {
                columns: read_text_list(fk.get_columns().map_err(from_capnp)?)?,
                ref_table: text_of(fk.get_ref_table().map_err(from_capnp)?),
                ref_columns: read_text_list(fk.get_ref_columns().map_err(from_capnp)?)?,
            })
        }
    }
}

fn write_bool_list(mut b: capnp::primitive_list::Builder<'_, bool>, vals: &[bool]) {
    for (i, v) in vals.iter().enumerate() {
        b.set(i as u32, *v);
    }
}

fn read_bool_list(r: capnp::primitive_list::Reader<'_, bool>) -> Result<Vec<bool>> {
    Ok(r.iter().collect())
}

fn write_text_list(mut b: capnp::text_list::Builder<'_>, vals: &[String]) {
    for (i, v) in vals.iter().enumerate() {
        b.set(i as u32, v);
    }
}

pub(super) fn read_text_list(r: capnp::text_list::Reader<'_>) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(r.len() as usize);
    for item in r.iter() {
        out.push(text_of(item.map_err(from_capnp)?));
    }
    Ok(out)
}

pub(super) fn write_identity_high_water(
    mut b: identity_high_water_capnp::Builder<'_>,
    row: &DbIdentityHighWater,
) {
    b.set_table(&row.table);
    b.set_last(row.last);
}

pub(super) fn read_identity_high_water(
    r: identity_high_water_capnp::Reader<'_>,
) -> Result<DbIdentityHighWater> {
    Ok(DbIdentityHighWater {
        table: text_of(r.get_table().map_err(from_capnp)?),
        last: r.get_last(),
    })
}

pub(super) fn write_identity_list(
    mut b: capnp::struct_list::Builder<'_, identity_high_water_capnp::Owned>,
    rows: &[DbIdentityHighWater],
) {
    for (i, row) in rows.iter().enumerate() {
        write_identity_high_water(b.reborrow().get(i as u32), row);
    }
}

pub(super) fn read_identity_list(
    r: capnp::struct_list::Reader<'_, identity_high_water_capnp::Owned>,
) -> Result<Vec<DbIdentityHighWater>> {
    let mut out = Vec::with_capacity(r.len() as usize);
    for item in r.iter() {
        out.push(read_identity_high_water(item)?);
    }
    Ok(out)
}

pub(super) fn write_identity_export_reply(
    result: identity_export_reply::Builder<'_>,
    outcome: Result<Vec<DbIdentityHighWater>>,
) {
    match outcome {
        Ok(rows) => {
            let mut ok = result.init_ok(rows.len() as u32);
            write_identity_list(ok.reborrow(), &rows);
        }
        Err(err) => write_error(result.init_err(), &err),
    }
}

pub(super) fn read_identity_export_reply(
    result: identity_export_reply::Reader<'_>,
) -> Result<Vec<DbIdentityHighWater>> {
    match result.which().map_err(from_capnp)? {
        identity_export_reply::Ok(ok) => read_identity_list(ok.map_err(from_capnp)?),
        identity_export_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
    }
}

pub(super) fn write_user_relations_reply(
    result: user_relations_reply::Builder<'_>,
    outcome: Result<Vec<String>>,
) {
    match outcome {
        Ok(names) => write_text_list(result.init_ok(names.len() as u32), &names),
        Err(err) => write_error(result.init_err(), &err),
    }
}

pub(super) fn read_user_relations_reply(
    result: user_relations_reply::Reader<'_>,
) -> Result<Vec<String>> {
    match result.which().map_err(from_capnp)? {
        user_relations_reply::Ok(ok) => read_text_list(ok.map_err(from_capnp)?),
        user_relations_reply::Err(err) => Err(read_error(err.map_err(from_capnp)?)),
    }
}
