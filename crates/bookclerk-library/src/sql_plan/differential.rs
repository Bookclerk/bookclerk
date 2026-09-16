//! Grammar-aware differential execution (SQLite vs PostgreSQL).
//!
//! Compares typed [`bookclerk_plugin_abi::DbValue`] cells and
//! [`bookclerk_db_exec::DbErrorClass`], not native engine error strings.
//! D1 HTTP mock coverage stays in `database-d1` `executing_mock_typed_shared_vectors`
//! / `executing_mock_sql_v1_semantic_vectors`; generated samples here stay
//! inside D1 `maxPatternBytes` so the same SQL is D1-admissible.

#[cfg(test)]
mod tests {
    use bookclerk_db_exec::{classify_db_err, DbErrorClass, ExecCaps};
    use bookclerk_plugin_abi::{
        admitted_bookclerk_sql_samples, DbPlanStatementKind, DbResultSelection, DbValue,
        ExecuteRequest, TypedDbStatement,
    };
    use sea_orm::DatabaseConnection;

    use super::super::stamp_typed_vector;

    #[derive(Debug, Clone)]
    enum Outcome {
        Rows(Vec<Vec<DbValue>>),
        Affected(u64),
        Class(DbErrorClass),
    }

    impl PartialEq for Outcome {
        fn eq(&self, other: &Self) -> bool {
            match (self, other) {
                (Self::Affected(a), Self::Affected(b)) => a == b,
                (Self::Class(a), Self::Class(b)) => a == b,
                (Self::Rows(a), Self::Rows(b)) => {
                    a.len() == b.len()
                        && a.iter().zip(b).all(|(left, right)| {
                            left.len() == right.len()
                                && left.iter().zip(right).all(|(x, y)| portable_cell_eq(x, y))
                        })
                }
                _ => false,
            }
        }
    }

    /// Compare Bookclerk cells, not engine affinity tags.
    ///
    /// Untyped `SELECT` projections can surface `Null(Text)` vs `Null(Bool)`
    /// or integer `0`/`1` vs `Boolean`. Declared-column vectors still require
    /// exact variants (`vectors_typed`).
    fn portable_cell_eq(left: &DbValue, right: &DbValue) -> bool {
        match (left, right) {
            (DbValue::Null(_), DbValue::Null(_)) => true,
            (DbValue::Boolean(b), DbValue::Int64(n)) | (DbValue::Int64(n), DbValue::Boolean(b)) => {
                i64::from(*b) == *n
            }
            (a, b) => a == b,
        }
    }

    fn is_select(sql: &str) -> bool {
        sql.trim_start()
            .get(..6)
            .is_some_and(|head| head.eq_ignore_ascii_case("select"))
    }

    async fn run_sql(db: &DatabaseConnection, sql: &str, parameters: Vec<DbValue>) -> Outcome {
        let kind = if is_select(sql) {
            DbPlanStatementKind::Select
        } else {
            DbPlanStatementKind::Execute
        };
        let req = ExecuteRequest {
            operation_id: "diff".into(),
            request_hash: String::new(),
            deadline_unix_ms: 0,
            statements: vec![TypedDbStatement {
                sql: sql.into(),
                parameters,
                kind,
                max_rows: 8,
                result_selection: if kind == DbPlanStatementKind::Select {
                    DbResultSelection::Rows
                } else {
                    DbResultSelection::AffectedRows
                },
            }],
        };
        let mut catalog = crate::migrations::host_sql_type_env();
        let envelope = match stamp_typed_vector(req, &mut catalog) {
            Ok(env) => env,
            Err(_) => return Outcome::Class(DbErrorClass::Other),
        };
        match bookclerk_db_exec::execute_typed_envelope_on_connection(
            db,
            &envelope,
            ExecCaps::from(8),
            bookclerk_db_exec::AtomicSession::from_deadline(None)
                .with_type_env(crate::migrations::host_sql_type_env()),
        )
        .await
        {
            Ok(reply) => {
                let stmt = &reply.statements[0];
                if kind == DbPlanStatementKind::Select {
                    Outcome::Rows(stmt.rows.iter().map(|row| row.values.clone()).collect())
                } else {
                    Outcome::Affected(stmt.rows_affected)
                }
            }
            Err(err) => Outcome::Class(classify_db_err(&err)),
        }
    }

    async fn sqlite_db() -> DatabaseConnection {
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .expect("sqlite");
        crate::apply_host_schema(&db, crate::HostSchemaKind::RowMarker)
            .await
            .expect("sqlite schema");
        db
    }

    fn postgres_url() -> Option<String> {
        let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL")
            .ok()
            .filter(|s| !s.trim().is_empty());
        if url.is_some() {
            return url;
        }
        assert!(
            std::env::var("BOOKCLERK_REQUIRE_POSTGRES_TESTS")
                .ok()
                .as_deref()
                != Some("1"),
            "BOOKCLERK_TEST_POSTGRES_URL is required when BOOKCLERK_REQUIRE_POSTGRES_TESTS=1"
        );
        None
    }

    async fn postgres_db() -> DatabaseConnection {
        let url = postgres_url().expect("postgres url");
        let db_name = format!("diff_{}", uuid::Uuid::new_v4().as_simple());
        let admin = sea_orm::Database::connect(url.as_str())
            .await
            .expect("admin");
        let backend = sea_orm::ConnectionTrait::get_database_backend(&admin);
        sea_orm::ConnectionTrait::execute_raw(
            &admin,
            sea_orm::Statement::from_string(backend, format!("CREATE DATABASE {db_name}")),
        )
        .await
        .expect("create db");
        let (base, query) = match url.split_once('?') {
            Some((base, q)) => (base, Some(q)),
            None => (url.as_str(), None),
        };
        let trimmed = base.trim_end_matches('/');
        let slash = trimmed.rfind('/').expect("url path");
        let db_url = match query {
            Some(q) => format!("{}/{db_name}?{q}", &trimmed[..slash]),
            None => format!("{}/{db_name}", &trimmed[..slash]),
        };
        let db = sea_orm::Database::connect(&db_url).await.expect("connect");
        crate::apply_host_schema(&db, crate::HostSchemaKind::RowMarker)
            .await
            .expect("postgres schema");
        db
    }

    fn sample_params(sql: &str) -> Vec<DbValue> {
        if sql.contains('?') {
            vec![DbValue::Text("z".into())]
        } else {
            Vec::new()
        }
    }

    fn cases() -> Vec<(String, Vec<DbValue>)> {
        let mut cases = vec![
            ("SELECT 1 AS n".into(), Vec::new()),
            ("SELECT 'café' AS t".into(), Vec::new()),
            (
                "SELECT CASE WHEN 'x' LIKE NULL THEN 1 ELSE 0 END AS m".into(),
                Vec::new(),
            ),
            ("SELECT ? AS v".into(), vec![DbValue::Text("z".into())]),
            (
                "INSERT INTO db_serialization_slots (slot_key, bump) VALUES ('dup-diff', 0)".into(),
                Vec::new(),
            ),
            (
                "INSERT INTO db_serialization_slots (slot_key, bump) VALUES ('dup-diff', 1)".into(),
                Vec::new(),
            ),
            (
                "SELECT 1 / NULLIF(0, 1) AS d, 1 % NULLIF(0, 1) AS m".into(),
                Vec::new(),
            ),
            ("SELECT 10 / NULLIF(2, 0) AS d".into(), Vec::new()),
            (
                "SELECT CASE WHEN length(json_object('a', 1, 'b', NULL)) \
                 > length(json_object('a', 1)) THEN 1 ELSE 0 END AS null_retained"
                    .into(),
                Vec::new(),
            ),
            (
                "SELECT CASE WHEN length(json_object('a', 1, 'a', NULL)) > 2 \
                 THEN 1 ELSE 0 END AS dup_null_retained"
                    .into(),
                Vec::new(),
            ),
            (
                "SELECT json_extract(json_extract(json_object('n', json_object('k', 'v')), '$.n'), '$.k') AS v".into(),
                Vec::new(),
            ),
        ];
        for sql in admitted_bookclerk_sql_samples(7, 24) {
            cases.push((sql.clone(), sample_params(&sql)));
        }
        cases
    }

    #[tokio::test]
    async fn grammar_aware_sql_is_admitted_on_sqlite() {
        let db = sqlite_db().await;
        for (sql, params) in cases() {
            let _ = run_sql(&db, &sql, params).await;
        }
    }

    #[tokio::test]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
    async fn postgres_differential_grammar_aware_matches_sqlite() {
        if postgres_url().is_none() {
            return;
        }
        let sqlite = sqlite_db().await;
        let postgres = postgres_db().await;
        for (sql, params) in cases() {
            let left = run_sql(&sqlite, &sql, params.clone()).await;
            let right = run_sql(&postgres, &sql, params).await;
            assert_eq!(left, right, "differential mismatch for `{sql}`");
        }
    }
}
