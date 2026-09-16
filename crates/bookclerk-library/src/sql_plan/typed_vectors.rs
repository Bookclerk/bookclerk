//! Typed adapter conformance (`ExecuteRequest` / `ExecuteReply`).
//!
//! [`run_typed_request_vectors`] is the #178 admission path for database plugins.
//! Legacy JSON [`super::vectors::run_request_vectors`] remains for the atomic bridge.

use std::future::Future;

use bookclerk_db_exec::{AtomicSession, ExecCaps};
use bookclerk_plugin_abi::{DbCapabilities, ExecuteReply, ExecuteRequest};
use sea_orm::DatabaseConnection;

use super::{validate_execute_reply, vectors};

/// Runs the shared contract suite through a typed `ExecuteRequest` callback.
///
/// `advertised_cap` is the adapter's negotiated `maxResultRows`. The harness does
/// not wrap SQL or post-reject oversized results.
///
/// # Panics
///
/// Panics when a vector fails.
pub async fn run_typed_request_vectors<F, Fut, E>(
    connect: DbCapabilities,
    advertised_cap: u32,
    mut run: F,
) where
    F: FnMut(ExecuteRequest) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, E>>,
    E: std::fmt::Display,
{
    run_typed_contract_vectors(connect, advertised_cap, move |typed, _cap| {
        let fut = run(typed);
        async move { fut.await.map_err(|err| err.to_string()) }
    })
    .await;
}

/// Runs the shared contract suite through typed `execute_typed_on_session`.
///
/// # Panics
///
/// Panics when a vector fails.
pub async fn run_typed_conn_vectors(
    db: &DatabaseConnection,
    connect: DbCapabilities,
    timing: &str,
) {
    let db = db.clone();
    let timing = timing.to_string();
    let connect_for_run = connect.clone();
    run_typed_contract_vectors(
        connect,
        vectors::CONTRACT_VECTOR_ROW_CAP,
        move |typed, cap| {
            let db = db.clone();
            let timing = timing.clone();
            let connect = connect_for_run.clone();
            async move {
                let mut caps = ExecCaps::from_capabilities(&connect);
                if cap > 0 {
                    caps.max_result_rows = cap;
                }
                let reply = bookclerk_db_exec::execute_typed_on_session(
                    &db,
                    &typed,
                    bookclerk_db_exec::GuestReceiptPersist::default(),
                    &timing,
                    caps,
                    AtomicSession::from_deadline(None)
                        .with_type_env(crate::migrations::host_sql_type_env()),
                )
                .await
                .map_err(|err| err.to_string())?;
                validate_execute_reply(&typed, &reply, &connect).map_err(|err| err.to_string())?;
                Ok(reply)
            }
        },
    )
    .await;
}

/// Native typed contract suite (`ExecuteRequest` → `ExecuteReply`).
async fn run_typed_contract_vectors<F, Fut>(connect: DbCapabilities, row_cap: u32, mut run: F)
where
    F: FnMut(ExecuteRequest, u32) -> Fut,
    Fut: Future<Output = Result<ExecuteReply, String>>,
{
    let connect_for_validate = connect.clone();
    super::run_typed_contract_vectors(connect, row_cap, move |typed, cap| {
        let connect = connect_for_validate.clone();
        let typed_for_validate = typed.clone();
        let fut = run(typed, cap);
        async move {
            let reply = fut.await?;
            validate_typed_reply(&connect, &typed_for_validate, &reply)?;
            Ok(reply)
        }
    })
    .await;
}

/// Asserts a typed reply echoes the request before host interpretation.
fn validate_typed_reply(
    connect: &DbCapabilities,
    req: &ExecuteRequest,
    reply: &ExecuteReply,
) -> Result<(), String> {
    reply.validate_positional().map_err(|err| err.to_string())?;
    if reply.operation_id != req.operation_id {
        return Err(format!(
            "execute reply operationId {:?} does not echo {}",
            reply.operation_id, req.operation_id
        ));
    }
    if reply.statements.len() != req.statements.len() {
        return Err(format!(
            "execute reply has {} statements; request has {}",
            reply.statements.len(),
            req.statements.len()
        ));
    }
    validate_execute_reply(req, reply, connect).map_err(|err| err.to_string())
}

#[cfg(test)]
mod typed_value_matrix {
    use super::*;
    use bookclerk_plugin_abi::{
        DbPlanStatementKind, DbResultSelection, DbType, DbValue, ExecuteRequest, TypedDbStatement,
    };

    async fn mem_db() -> sea_orm::DatabaseConnection {
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .expect("in-memory sqlite")
    }

    fn sqlite_caps() -> ExecCaps {
        ExecCaps::from_capabilities(&DbCapabilities::advertised_sqlite())
    }

    fn select_param(op: &str, sql: &str, param: DbValue) -> ExecuteRequest {
        ExecuteRequest {
            operation_id: op.into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: sql.into(),
                parameters: vec![param],
                kind: DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        }
    }

    async fn roundtrip(db: &DatabaseConnection, req: &ExecuteRequest) -> DbValue {
        let reply = bookclerk_db_exec::execute_typed_on_session(
            db,
            req,
            bookclerk_db_exec::GuestReceiptPersist::default(),
            "sqlite_txn",
            sqlite_caps(),
            AtomicSession::from_deadline(None)
                .with_type_env(crate::migrations::host_sql_type_env()),
        )
        .await
        .expect("typed execute");
        reply.statements[0].rows[0].values[0].clone()
    }

    #[tokio::test]
    async fn int64_min_max_roundtrip_via_select_param() {
        let db = mem_db().await;
        for (label, value) in [
            ("min", DbValue::Int64(i64::MIN)),
            ("max", DbValue::Int64(i64::MAX)),
        ] {
            let got = roundtrip(&db, &select_param(label, "SELECT ? AS v", value.clone())).await;
            assert_eq!(got, value, "{label}");
        }
    }

    #[tokio::test]
    async fn text_utf8_roundtrip_rejects_embedded_nul() {
        let db = mem_db().await;
        let utf8 = DbValue::Text("café 日本語".into());
        let got = roundtrip(&db, &select_param("utf8", "SELECT ? AS v", utf8.clone())).await;
        assert_eq!(got, utf8);

        let nul = DbValue::Text("before\u{0}after".into());
        let err = bookclerk_db_exec::execute_typed_on_session(
            &db,
            &select_param("nul", "SELECT ? AS v", nul),
            bookclerk_db_exec::GuestReceiptPersist::default(),
            "sqlite_txn",
            sqlite_caps(),
            AtomicSession::from_deadline(None)
                .with_type_env(crate::migrations::host_sql_type_env()),
        )
        .await
        .expect_err("TEXT U+0000 must fail closed");
        assert!(
            err.to_string().contains("U+0000"),
            "expected portable TEXT reject, got {err}"
        );

        let blob = DbValue::Bytes(b"before\0after".to_vec());
        let got = roundtrip(&db, &select_param("blob", "SELECT ? AS v", blob.clone())).await;
        assert_eq!(got, blob, "BYTES may contain 0x00");
    }

    #[test]
    fn text_nul_rejected_by_typed_decoder() {
        use bookclerk_db_exec::{db_value_from_sea, db_value_to_sea};
        use sea_orm::Value as SeaValue;
        let err = db_value_from_sea(&SeaValue::String(Some("a\0b".into()))).unwrap_err();
        assert!(
            err.contains("U+0000"),
            "expected TEXT NUL rejection, got {err}"
        );
        let blob = db_value_from_sea(&db_value_to_sea(&DbValue::Bytes(vec![0, 1]))).unwrap();
        assert_eq!(blob, DbValue::Bytes(vec![0, 1]));
    }

    #[tokio::test]
    async fn bytes_roundtrip_via_select_param() {
        let db = mem_db().await;
        let bytes = DbValue::Bytes(vec![0, 127, 255, 42]);
        let got = roundtrip(&db, &select_param("bytes", "SELECT ? AS v", bytes.clone())).await;
        assert_eq!(got, bytes);
    }

    #[tokio::test]
    async fn typed_null_carries_expected_type() {
        let db = mem_db().await;
        let backend = sea_orm::ConnectionTrait::get_database_backend(&db);
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_string(
                backend,
                "CREATE TABLE IF NOT EXISTS typed_null_probe (v INTEGER)",
            ),
        )
        .await
        .unwrap();
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_string(
                backend,
                "INSERT INTO typed_null_probe (v) VALUES (NULL)",
            ),
        )
        .await
        .unwrap();
        let req = ExecuteRequest {
            operation_id: "null-int".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "SELECT v FROM typed_null_probe".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let reply = bookclerk_db_exec::execute_typed_on_session(
            &db,
            &req,
            bookclerk_db_exec::GuestReceiptPersist::default(),
            "sqlite_txn",
            sqlite_caps(),
            AtomicSession::from_deadline(None)
                .with_type_env(crate::migrations::host_sql_type_env()),
        )
        .await
        .expect("typed execute");
        assert_eq!(reply.statements[0].columns[0].db_type, DbType::Int64);
        assert!(matches!(
            reply.statements[0].rows[0].values[0],
            DbValue::Null(DbType::Int64)
        ));
    }

    #[tokio::test]
    async fn finite_float_roundtrip_via_select_param() {
        let db = mem_db().await;
        let finite = DbValue::Float64(1.25);
        let got = roundtrip(
            &db,
            &select_param("finite", "SELECT ? AS v", finite.clone()),
        )
        .await;
        assert_eq!(got, finite);
    }

    #[test]
    fn non_finite_float_rejected_by_typed_decoder() {
        use bookclerk_db_exec::db_value_from_sea;
        use sea_orm::Value as SeaValue;
        for value in [
            SeaValue::Double(Some(f64::NAN)),
            SeaValue::Double(Some(f64::INFINITY)),
        ] {
            let err = db_value_from_sea(&value).unwrap_err();
            assert!(
                err.contains("finite"),
                "expected non-finite rejection for {value:?}, got {err}"
            );
        }
    }

    #[tokio::test]
    async fn failed_statement_rolls_back_first_insert() {
        let db = mem_db().await;
        let req = ExecuteRequest {
            operation_id: "typed-rb".into(),
            request_hash: String::new(),
            statements: vec![
                TypedDbStatement {
                    sql:
                        "INSERT INTO db_serialization_slots (slot_key, bump) VALUES ('typed-rb', 0)"
                            .into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::AffectedRows,
                },
                TypedDbStatement {
                    sql:
                        "INSERT INTO db_serialization_slots (slot_key, bump) VALUES ('typed-rb', 1)"
                            .into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::AffectedRows,
                },
            ],
            deadline_unix_ms: 0,
        };
        let err = bookclerk_db_exec::execute_typed_on_session(
            &db,
            &req,
            bookclerk_db_exec::GuestReceiptPersist::default(),
            "sqlite_txn",
            sqlite_caps(),
            AtomicSession::from_deadline(None)
                .with_type_env(crate::migrations::host_sql_type_env()),
        )
        .await
        .expect_err("duplicate key must fail");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("unique") || msg.contains("constraint"),
            "{err}"
        );

        let check = ExecuteRequest {
            operation_id: "typed-rb-check".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "SELECT slot_key FROM db_serialization_slots WHERE slot_key = 'typed-rb'"
                    .into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let reply = bookclerk_db_exec::execute_typed_on_session(
            &db,
            &check,
            bookclerk_db_exec::GuestReceiptPersist::default(),
            "sqlite_txn",
            sqlite_caps(),
            AtomicSession::from_deadline(None)
                .with_type_env(crate::migrations::host_sql_type_env()),
        )
        .await
        .expect("rollback check");
        assert!(
            reply.statements[0].rows.is_empty(),
            "failed atomic batch must roll back the first insert"
        );
    }
}
