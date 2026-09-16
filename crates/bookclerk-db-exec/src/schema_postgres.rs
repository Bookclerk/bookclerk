//! Adapter execution-edge SQL lowering (host schema packs and binding DDL).
//!
//! `bookclerk-library` owns canonical SQLite-shaped SQL and never lowers it.
//! SeaORM adapters invoke these helpers at execute: the SQLite family applies
//! canonical DDL verbatim; PostgreSQL lowers types/`AUTOINCREMENT` here (not
//! inside [`crate::lower_canonical_sql`], which stays DML/query helpers).
//! There is no hand-authored parallel Postgres schema.

use std::borrow::Cow;

use bookclerk_plugin_abi::ExecuteRequest;
use sea_orm::DatabaseBackend;

/// SQL text for one host-schema statement on the live connection backend.
///
/// SQLite-family backends return the canonical statement unchanged; Postgres
/// applies the mechanical DDL lowering. Unknown SeaORM backends fail closed.
///
/// # Panics
///
/// Panics when `backend` is not SQLite or PostgreSQL. `DatabaseBackend` is
/// non-exhaustive; BookclerkSQL adapters do not implement other SeaORM
/// families.
#[must_use]
pub fn schema_sql_for_backend(backend: DatabaseBackend, canonical: &str) -> Cow<'_, str> {
    match backend {
        DatabaseBackend::Postgres => Cow::Owned(crate::lower_canonical_ddl_to_postgres(canonical)),
        DatabaseBackend::Sqlite => Cow::Borrowed(canonical),
        other => crate::exec::reject_unknown_seaorm_backend(other),
    }
}

/// Mechanical type/identity lowering for one **binding** statement.
///
/// Hosts emit canonical SQLite-shaped `CREATE`/`DROP`. Postgres adapters
/// rewrite `AUTOINCREMENT`/`BLOB`/`INTEGER`/`REAL` here; SQLite/D1 leave the
/// statement unchanged. DML stays for [`crate::lower_canonical_sql`].
///
/// # Panics
///
/// Panics when `backend` is not SQLite or PostgreSQL. `DatabaseBackend` is
/// non-exhaustive; BookclerkSQL adapters do not implement other SeaORM
/// families.
#[must_use]
pub fn lower_binding_sql_for_backend(backend: DatabaseBackend, sql: &str) -> Cow<'_, str> {
    match backend {
        DatabaseBackend::Postgres if bookclerk_plugin_abi::statement_is_ddl(sql) => {
            Cow::Owned(crate::lower::rewrite_canonical_ddl_types_for_postgres(sql))
        }
        DatabaseBackend::Postgres | DatabaseBackend::Sqlite => Cow::Borrowed(sql),
        other => crate::exec::reject_unknown_seaorm_backend(other),
    }
}

/// Applies [`lower_binding_sql_for_backend`] to every statement in `req`.
///
/// Identity when no statement changes (SQLite/D1, or Postgres DML-only).
#[must_use]
pub fn lower_binding_ddl_execute_request(
    backend: DatabaseBackend,
    req: &ExecuteRequest,
) -> ExecuteRequest {
    if backend != DatabaseBackend::Postgres {
        return req.clone();
    }
    let mut out = req.clone();
    let mut changed = false;
    for stmt in &mut out.statements {
        let lowered = lower_binding_sql_for_backend(backend, &stmt.sql);
        if lowered.as_ref() != stmt.sql.as_str() {
            stmt.sql = lowered.into_owned();
            changed = true;
        }
    }
    if changed {
        out
    } else {
        req.clone()
    }
}

/// True when `sql` is a host schema version marker (`schema_migrations` or `PRAGMA user_version`).
#[must_use]
pub fn is_host_schema_version_marker(sql: &str) -> bool {
    let t = sql.trim();
    t.starts_with("INSERT INTO schema_migrations")
        || t.starts_with("DELETE FROM schema_migrations")
        || t.starts_with("PRAGMA user_version =")
}

/// Expands `[stmt, …, version_marker]` at the adapter execution edge.
///
/// Each incoming string is already one canonical statement. Adapters lower
/// per statement and may insert identity companions; they do not split SQL.
#[must_use]
pub fn expand_host_schema_batch(backend: DatabaseBackend, batch: &[String]) -> Option<Vec<String>> {
    expand_host_schema_batch_grouped(backend, batch).map(|(stmts, _)| stmts)
}

/// [`expand_host_schema_batch`] plus per-original-statement expansion counts.
#[must_use]
pub fn expand_host_schema_batch_grouped(
    backend: DatabaseBackend,
    batch: &[String],
) -> Option<(Vec<String>, Vec<usize>)> {
    if batch.len() < 2 {
        return None;
    }
    let version = batch.last()?;
    if !is_host_schema_version_marker(version) {
        return None;
    }
    // Identity companions are adapter-private plpgsql (`$bookclerk_ident$`).
    // Re-running this expander would SQL-v1-split those bodies on `;`.
    if batch.iter().any(|sql| sql.contains("$bookclerk_ident$")) {
        return None;
    }
    let mut stmts: Vec<String> = Vec::new();
    let mut groups: Vec<usize> = Vec::new();
    for stmt in &batch[..batch.len() - 1] {
        let lowered = schema_sql_for_backend(backend, stmt).into_owned();
        let companions = if backend == DatabaseBackend::Postgres {
            postgres_identity_companions(stmt)
        } else {
            Vec::new()
        };
        groups.push(1usize.saturating_add(companions.len()));
        stmts.push(lowered);
        stmts.extend(companions);
    }
    stmts.push(version.clone());
    groups.push(1);
    Some((stmts, groups))
}

/// Postgres-only companion DDL: transactional identity counter + BEFORE INSERT trigger.
///
/// SQLite `AUTOINCREMENT` records the largest committed value. A PostgreSQL
/// `setval` sequence does not roll back with the `ExecuteRequest`. The adapter
/// keeps a `BIGINT` high-water row in [`bookclerk_plugin_abi::SQL_IDENTITY_TABLE`]
/// and assigns omit-ids from that row under `SELECT … FOR UPDATE`. Companions
/// are generated from **canonical** SQL (parsed identity column, not assumed
/// `id`) and executed internally (not extra Cap'n statements). Empty when `sql`
/// is not `CREATE TABLE` with `INTEGER PRIMARY KEY AUTOINCREMENT`.
#[must_use]
pub fn postgres_identity_companions(sql: &str) -> Vec<String> {
    postgres_identity_companions_for_action(sql, None)
}

/// [`postgres_identity_companions`] using a resolved schema action when present.
#[must_use]
pub fn postgres_identity_companions_for_action(
    sql: &str,
    action: Option<&bookclerk_plugin_abi::SchemaAction>,
) -> Vec<String> {
    match action {
        Some(bookclerk_plugin_abi::SchemaAction::Create { noop: true, .. })
        | Some(bookclerk_plugin_abi::SchemaAction::None) => return Vec::new(),
        Some(bookclerk_plugin_abi::SchemaAction::Create { schema, .. }) => {
            return postgres_identity_create(schema);
        }
        Some(bookclerk_plugin_abi::SchemaAction::Drop { table }) => {
            return postgres_identity_drop(table);
        }
        None => {}
    }
    if let Some(schema) = bookclerk_plugin_abi::parse_create_table_schema(sql) {
        return postgres_identity_create(&schema);
    }
    if let Some(table) = bookclerk_plugin_abi::parse_drop_table_name(sql) {
        return postgres_identity_drop(&table);
    }
    Vec::new()
}

/// Builds CREATE-table identity trigger/function companions from a resolved schema.
fn postgres_identity_create(schema: &bookclerk_plugin_abi::CreateTableSchema) -> Vec<String> {
    let Some(col) = schema.identity_column.as_deref() else {
        return Vec::new();
    };
    let table = schema.table.as_str();
    if !is_safe_ident(table) || !is_safe_ident(col) {
        return Vec::new();
    }
    let fn_name = bookclerk_plugin_abi::postgres_identity_function_name(table);
    let trig_name = bookclerk_plugin_abi::postgres_identity_trigger_name(table);
    vec![
        format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             table_name TEXT PRIMARY KEY, last BIGINT NOT NULL)",
            bookclerk_plugin_abi::SQL_IDENTITY_TABLE
        ),
        format!(
            "CREATE OR REPLACE FUNCTION {fn_name}() RETURNS trigger \
             LANGUAGE plpgsql AS $bookclerk_ident$ \
             DECLARE nxt bigint; \
             BEGIN \
               INSERT INTO {} (table_name, last) VALUES (TG_TABLE_NAME, 0) \
                 ON CONFLICT (table_name) DO NOTHING; \
               PERFORM 1 FROM {} WHERE table_name = TG_TABLE_NAME FOR UPDATE; \
               IF NEW.{col} IS NULL THEN \
                 UPDATE {} SET last = last + 1 WHERE table_name = TG_TABLE_NAME \
                   RETURNING last INTO nxt; \
                 NEW.{col} := nxt; \
               ELSE \
                 UPDATE {} SET last = GREATEST(last, NEW.{col}) \
                   WHERE table_name = TG_TABLE_NAME; \
               END IF; \
               RETURN NEW; \
             END; \
             $bookclerk_ident$",
            bookclerk_plugin_abi::SQL_IDENTITY_TABLE,
            bookclerk_plugin_abi::SQL_IDENTITY_TABLE,
            bookclerk_plugin_abi::SQL_IDENTITY_TABLE,
            bookclerk_plugin_abi::SQL_IDENTITY_TABLE
        ),
        format!("DROP TRIGGER IF EXISTS {trig_name} ON {table}"),
        format!(
            "CREATE TRIGGER {trig_name} BEFORE INSERT ON {table} \
             FOR EACH ROW EXECUTE FUNCTION {fn_name}()"
        ),
    ]
}

/// Builds DROP-table identity cleanup companions.
fn postgres_identity_drop(table: &str) -> Vec<String> {
    if !is_safe_ident(table) {
        return Vec::new();
    }
    let fn_name = bookclerk_plugin_abi::postgres_identity_function_name(table);
    vec![
        format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             table_name TEXT PRIMARY KEY, last BIGINT NOT NULL)",
            bookclerk_plugin_abi::SQL_IDENTITY_TABLE
        ),
        format!(
            "DELETE FROM {} WHERE table_name = '{table}'",
            bookclerk_plugin_abi::SQL_IDENTITY_TABLE
        ),
        format!("DROP FUNCTION IF EXISTS {fn_name}()"),
    ]
}

/// Catalog + Postgres identity companions for one canonical binding statement.
#[must_use]
pub fn binding_companions(backend: DatabaseBackend, canonical: &str) -> Vec<String> {
    let mut out = bookclerk_plugin_abi::catalog_companions(canonical);
    if backend == DatabaseBackend::Postgres {
        out.extend(postgres_identity_companions(canonical));
    }
    out
}

/// Applies one canonical host DDL statement (library restore / schema).
///
/// Postgres identity companions are installed; binding catalog DML is not.
///
/// # Errors
///
/// Returns when the engine rejects the lowered DDL or a companion.
pub async fn realize_host_ddl<C>(conn: &C, canonical: &str) -> Result<(), sea_orm::DbErr>
where
    C: sea_orm::ConnectionTrait,
{
    let backend = conn.get_database_backend();
    let lowered = schema_sql_for_backend(backend, canonical).into_owned();
    conn.execute_raw(sea_orm::Statement::from_string(backend, lowered))
        .await?;
    if backend == DatabaseBackend::Postgres {
        for companion in postgres_identity_companions(canonical) {
            conn.execute_raw(sea_orm::Statement::from_string(backend, companion))
                .await?;
        }
    }
    Ok(())
}

/// Applies one canonical binding DDL statement (plugin restore).
///
/// Includes adapter-private catalog DML and Postgres identity companions.
///
/// # Errors
///
/// Returns when the engine rejects the lowered DDL or a companion.
pub async fn realize_binding_ddl<C>(conn: &C, canonical: &str) -> Result<(), sea_orm::DbErr>
where
    C: sea_orm::ConnectionTrait,
{
    let backend = conn.get_database_backend();
    let lowered = schema_sql_for_backend(backend, canonical).into_owned();
    conn.execute_raw(sea_orm::Statement::from_string(backend, lowered))
        .await?;
    for companion in binding_companions(backend, canonical) {
        conn.execute_raw(sea_orm::Statement::from_string(backend, companion))
            .await?;
    }
    Ok(())
}

/// Expands a binding request with adapter-private catalog/identity companions.
///
/// Returns the expanded request and per-original-statement group sizes so
/// callers can collapse results back to the wire statement count.
#[must_use]
pub fn expand_binding_execute_request(
    backend: DatabaseBackend,
    req: &ExecuteRequest,
) -> (ExecuteRequest, Vec<usize>) {
    let mut statements = Vec::new();
    let mut groups = Vec::with_capacity(req.statements.len());
    for stmt in &req.statements {
        let companions = binding_companions(backend, &stmt.sql);
        groups.push(1 + companions.len());
        statements.push(stmt.clone());
        for sql in companions {
            let mut extra = stmt.clone();
            extra.sql = sql;
            extra.parameters.clear();
            extra.kind = bookclerk_plugin_abi::DbPlanStatementKind::Execute;
            extra.max_rows = 0;
            extra.result_selection = bookclerk_plugin_abi::DbResultSelection::Discard;
            statements.push(extra);
        }
    }
    (
        ExecuteRequest {
            statements,
            ..req.clone()
        },
        groups,
    )
}

/// Collapses interleaved companion results back to one result per original statement.
#[must_use]
pub fn collapse_companion_groups(
    groups: &[usize],
    results: Vec<bookclerk_plugin_abi::StatementResult>,
) -> Vec<bookclerk_plugin_abi::StatementResult> {
    let expected: usize = groups.iter().copied().sum();
    if expected == 0 || expected != results.len() || groups.iter().all(|g| *g == 1) {
        return results;
    }
    let mut out = Vec::with_capacity(groups.len());
    let mut i = 0;
    for &g in groups {
        if i >= results.len() {
            break;
        }
        out.push(results[i].clone());
        i = i.saturating_add(g);
    }
    out
}

/// Unquoted SQL v1 ident safe to interpolate into adapter-private DDL.
fn is_safe_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && bookclerk_plugin_abi::sql_v1_ident_in_bounds(s)
}

/// Expands a typed host schema batch at the adapter execution edge.
///
/// Returns the expanded request and per-original-statement expansion counts
/// (identity when the batch is not a host schema apply unit).
#[must_use]
pub fn expand_host_schema_execute_request(
    backend: DatabaseBackend,
    req: &ExecuteRequest,
) -> ExecuteRequest {
    expand_host_schema_execute_request_grouped(backend, req).0
}

/// [`expand_host_schema_execute_request`] plus collapse groups.
#[must_use]
pub fn expand_host_schema_execute_request_grouped(
    backend: DatabaseBackend,
    req: &ExecuteRequest,
) -> (ExecuteRequest, Vec<usize>) {
    let batch: Vec<String> = req.statements.iter().map(|s| s.sql.clone()).collect();
    let Some((expanded, groups)) = expand_host_schema_batch_grouped(backend, &batch) else {
        return (req.clone(), vec![1usize; req.statements.len()]);
    };
    if expanded == batch {
        return (req.clone(), groups);
    }
    let Some(template) = req.statements.first().cloned() else {
        return (req.clone(), groups);
    };
    let marker_template = req
        .statements
        .last()
        .cloned()
        .unwrap_or_else(|| template.clone());
    let n = expanded.len();
    let statements = expanded
        .into_iter()
        .enumerate()
        .map(|(i, sql)| {
            let mut stmt = if i + 1 == n && is_host_schema_version_marker(&sql) {
                marker_template.clone()
            } else {
                template.clone()
            };
            stmt.sql = sql;
            stmt
        })
        .collect();
    (
        ExecuteRequest {
            statements,
            ..req.clone()
        },
        groups,
    )
}

/// Collapses adapter-expanded host-schema results using per-statement groups.
#[must_use]
pub fn collapse_host_schema_results(
    groups: &[usize],
    results: Vec<bookclerk_plugin_abi::StatementResult>,
) -> Vec<bookclerk_plugin_abi::StatementResult> {
    collapse_companion_groups(groups, results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_abi::{DbPlanStatementKind, DbResultSelection, TypedDbStatement};

    #[test]
    fn collapse_restores_wire_request_shape() {
        use bookclerk_plugin_abi::StatementResult;
        let results = vec![
            StatementResult::from_affected(1),
            StatementResult::from_affected(2),
            StatementResult::from_affected(3),
            StatementResult::from_affected(1),
        ];
        let out = collapse_host_schema_results(&[3, 1], results);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].rows_affected, 1, "first of CREATE + companions");
        assert_eq!(out[1].rows_affected, 1, "marker maps one-to-one");
        let same = collapse_host_schema_results(
            &[1, 1],
            vec![
                StatementResult::from_affected(4),
                StatementResult::from_affected(1),
            ],
        );
        assert_eq!(same.len(), 2);
        assert_eq!(same[0].rows_affected, 4);
    }

    #[test]
    fn expand_host_schema_batch_lowers_canonical_for_postgres() {
        let canonical = "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY AUTOINCREMENT)";
        let batch = vec![
            canonical.to_string(),
            "INSERT INTO schema_migrations (version) VALUES (1)".to_string(),
        ];
        let expanded =
            expand_host_schema_batch(DatabaseBackend::Postgres, &batch).expect("host schema batch");
        assert!(
            expanded.iter().any(|s| s.contains("BIGINT PRIMARY KEY")),
            "adapter must lower canonical sqlite DDL for postgres: {expanded:?}"
        );
        let fn_name = bookclerk_plugin_abi::postgres_identity_function_name("users");
        assert!(
            expanded.iter().any(|s| s.contains(&fn_name)),
            "identity CREATE TABLE must install transactional identity companions: {expanded:?}"
        );
        assert!(
            !expanded[0].contains("AUTOINCREMENT"),
            "host canonical must not be pre-lowered: {}",
            expanded[0]
        );
        assert_eq!(
            expanded.last().map(String::as_str),
            Some("INSERT INTO schema_migrations (version) VALUES (1)")
        );
        assert!(
            expand_host_schema_batch(DatabaseBackend::Postgres, &expanded).is_none(),
            "already-expanded identity companions must not be packed again"
        );
    }

    #[test]
    fn expand_host_schema_execute_request_preserves_marker_statement() {
        let req = ExecuteRequest {
            operation_id: "host-schema".into(),
            request_hash: String::new(),
            statements: vec![
                TypedDbStatement {
                    sql: "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY AUTOINCREMENT)"
                        .into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::AffectedRows,
                },
                TypedDbStatement {
                    sql: "INSERT INTO schema_migrations (version) VALUES (1)".into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::AffectedRows,
                },
            ],
            deadline_unix_ms: 0,
        };
        let expanded = expand_host_schema_execute_request(DatabaseBackend::Postgres, &req);
        assert!(
            expanded.statements.len() > req.statements.len(),
            "identity CREATE TABLE must expand with serial-sync companions"
        );
        assert!(
            expanded
                .statements
                .iter()
                .any(|s| s.sql.contains("BIGINT PRIMARY KEY")),
            "typed execute must lower canonical DDL at adapter edge"
        );
        let fn_name = bookclerk_plugin_abi::postgres_identity_function_name("users");
        assert!(
            expanded.statements.iter().any(|s| s.sql.contains(&fn_name)),
            "typed execute must attach identity companions"
        );
        assert!(
            expanded
                .statements
                .iter()
                .all(|s| !s.sql.contains("AUTOINCREMENT")),
            "no sqlite-ism may reach the engine"
        );
        assert_eq!(
            expanded.statements.last().map(|s| s.sql.as_str()),
            Some("INSERT INTO schema_migrations (version) VALUES (1)")
        );
    }

    #[test]
    fn postgres_adapter_lowers_binding_ddl_types() {
        let canonical =
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY AUTOINCREMENT, b BLOB, n INTEGER, r REAL)";
        let req = ExecuteRequest {
            operation_id: "binding-ddl".into(),
            request_hash: String::new(),
            statements: vec![
                TypedDbStatement {
                    sql: canonical.into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::AffectedRows,
                },
                TypedDbStatement {
                    sql: "INSERT INTO t (n) VALUES (?)".into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::AffectedRows,
                },
            ],
            deadline_unix_ms: 0,
        };
        let sqlite = lower_binding_ddl_execute_request(DatabaseBackend::Sqlite, &req);
        assert_eq!(sqlite.statements[0].sql, canonical);
        let pg = lower_binding_ddl_execute_request(DatabaseBackend::Postgres, &req);
        assert!(
            pg.statements[0].sql.contains("BIGINT PRIMARY KEY"),
            "{}",
            pg.statements[0].sql
        );
        assert!(
            pg.statements[0].sql.contains("BYTEA"),
            "{}",
            pg.statements[0].sql
        );
        assert!(
            pg.statements[0].sql.contains("BIGINT"),
            "{}",
            pg.statements[0].sql
        );
        assert!(
            pg.statements[0].sql.contains("DOUBLE PRECISION"),
            "{}",
            pg.statements[0].sql
        );
        assert!(
            !pg.statements[0].sql.contains("AUTOINCREMENT"),
            "{}",
            pg.statements[0].sql
        );
        assert!(
            !pg.statements[0].sql.contains("BLOB"),
            "{}",
            pg.statements[0].sql
        );
        assert_eq!(
            pg.statements[1].sql, req.statements[1].sql,
            "DML must not take the DDL type pass"
        );
    }

    #[test]
    fn postgres_adapter_lowers_lowercase_binding_ddl_and_keeps_boolean() {
        let canonical = "create table if not exists t (\
             id integer primary key autoincrement, b blob, flag boolean)";
        let req = ExecuteRequest {
            operation_id: "binding-ddl-lc".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: canonical.into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        };
        let pg = lower_binding_ddl_execute_request(DatabaseBackend::Postgres, &req);
        assert!(
            pg.statements[0].sql.contains("BIGINT PRIMARY KEY"),
            "{}",
            pg.statements[0].sql
        );
        assert!(
            pg.statements[0].sql.contains("BYTEA"),
            "{}",
            pg.statements[0].sql
        );
        assert!(
            pg.statements[0].sql.contains("boolean"),
            "{}",
            pg.statements[0].sql
        );
        assert!(
            !pg.statements[0].sql.contains("autoincrement"),
            "{}",
            pg.statements[0].sql
        );
        assert!(
            !pg.statements[0].sql.contains("blob"),
            "{}",
            pg.statements[0].sql
        );
    }

    #[test]
    fn postgres_identity_companions_follow_autoincrement_create() {
        let sql =
            "CREATE TABLE IF NOT EXISTS ident (id INTEGER PRIMARY KEY AUTOINCREMENT, n INTEGER)";
        let companions = postgres_identity_companions(sql);
        assert_eq!(companions.len(), 4, "{companions:?}");
        let fn_name = bookclerk_plugin_abi::postgres_identity_function_name("ident");
        let trig_name = bookclerk_plugin_abi::postgres_identity_trigger_name("ident");
        assert!(
            companions[0].contains("bookclerk_identity"),
            "{}",
            companions[0]
        );
        assert!(
            companions[1].contains(&format!("CREATE OR REPLACE FUNCTION {fn_name}")),
            "{}",
            companions[1]
        );
        assert!(
            companions[1].contains("NEW.id"),
            "trigger must use the parsed identity column: {}",
            companions[1]
        );
        assert!(
            companions[2].contains(&format!("DROP TRIGGER IF EXISTS {trig_name} ON ident")),
            "{}",
            companions[2]
        );
        assert!(
            companions[3].contains("BEFORE INSERT ON ident"),
            "{}",
            companions[3]
        );
        assert!(postgres_identity_companions("CREATE TABLE t (n INTEGER)").is_empty());
        let named = postgres_identity_companions(
            "CREATE TABLE IF NOT EXISTS t (pk INTEGER PRIMARY KEY AUTOINCREMENT, n INTEGER)",
        );
        assert!(named.iter().any(|s| s.contains("NEW.pk")), "{named:?}");
        let drop = postgres_identity_companions("DROP TABLE IF EXISTS ident");
        assert!(
            drop.iter()
                .any(|s| s.contains("DELETE FROM") && s.contains("bookclerk_identity")),
            "{drop:?}"
        );
        assert!(
            drop.iter()
                .any(|s| s.contains(&format!("DROP FUNCTION IF EXISTS {fn_name}"))),
            "{drop:?}"
        );
        assert!(
            drop.iter().all(|s| !s.contains("DROP TRIGGER")),
            "DROP TABLE companions must not name the dropped relation: {drop:?}"
        );
        let none = postgres_identity_companions_for_action(
            sql,
            Some(&bookclerk_plugin_abi::SchemaAction::None),
        );
        assert!(
            none.is_empty(),
            "stamped None must not emit identity companions unless the caller parses SQL"
        );
    }

    #[test]
    fn hashed_identity_names_split_postgres_truncation_collisions() {
        // `bookclerk_ident_` is 16 bytes; PostgreSQL truncates at 63, so
        // table names that share a 47-byte prefix collided under the old
        // `bookclerk_ident_<table>` spelling.
        let prefix = "a".repeat(47);
        let left = format!("{prefix}x");
        let right = format!("{prefix}y");
        let old_left = format!("bookclerk_ident_{left}");
        let old_right = format!("bookclerk_ident_{right}");
        assert_eq!(&old_left[..63], &old_right[..63]);
        let fn_left = bookclerk_plugin_abi::postgres_identity_function_name(&left);
        let fn_right = bookclerk_plugin_abi::postgres_identity_function_name(&right);
        let trig_left = bookclerk_plugin_abi::postgres_identity_trigger_name(&left);
        let trig_right = bookclerk_plugin_abi::postgres_identity_trigger_name(&right);
        assert_ne!(fn_left, fn_right);
        assert_ne!(trig_left, trig_right);
        assert_ne!(fn_left, trig_left);
        assert!(fn_left.len() < 63 && trig_left.len() < 63);
        assert!(fn_left.starts_with(bookclerk_plugin_abi::POSTGRES_IDENT_FN_PREFIX));
        assert!(trig_left.starts_with(bookclerk_plugin_abi::POSTGRES_IDENT_TRIGGER_PREFIX));
    }
}
