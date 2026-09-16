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
/// applies the mechanical DDL lowering.
#[must_use]
pub fn schema_sql_for_backend(backend: DatabaseBackend, canonical: &str) -> Cow<'_, str> {
    match backend {
        DatabaseBackend::Postgres => Cow::Owned(crate::lower_canonical_ddl_to_postgres(canonical)),
        _ => Cow::Borrowed(canonical),
    }
}

/// Mechanical type/identity lowering for one **binding** statement.
///
/// Hosts emit canonical SQLite-shaped `CREATE`/`DROP`. Postgres adapters
/// rewrite `AUTOINCREMENT`/`BLOB`/`INTEGER`/`REAL` here; SQLite/D1 leave the
/// statement unchanged. DML stays for [`crate::lower_canonical_sql`].
#[must_use]
pub fn lower_binding_sql_for_backend(backend: DatabaseBackend, sql: &str) -> Cow<'_, str> {
    if backend == DatabaseBackend::Postgres && bookclerk_plugin_abi::statement_is_ddl(sql) {
        Cow::Owned(crate::lower::rewrite_canonical_ddl_types_for_postgres(sql))
    } else {
        Cow::Borrowed(sql)
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
    t.starts_with("INSERT INTO schema_migrations") || t.starts_with("PRAGMA user_version =")
}

/// Splits a canonical schema pack on top-level `;` (quote-aware).
///
/// Uses [`bookclerk_plugin_abi::split_sql_statements`] so literals such as
/// `DEFAULT 'a;b'` stay inside one statement.
#[must_use]
pub fn split_schema_statements(sql: &str) -> Vec<String> {
    bookclerk_plugin_abi::split_sql_statements(sql)
}

/// Expands `[canonical_ddl, version_marker, …]` at the adapter execution edge.
///
/// Host schema orchestration sends unsplit canonical DDL plus a version marker;
/// adapters lower the canonical pack for the live backend before execution.
#[must_use]
pub fn expand_host_schema_batch(backend: DatabaseBackend, batch: &[String]) -> Option<Vec<String>> {
    if batch.len() < 2 {
        return None;
    }
    let version = batch.last()?;
    if !is_host_schema_version_marker(version) {
        return None;
    }
    // Split first, lower per statement: statement-shaped rewrites such as
    // `INSERT OR IGNORE` → `ON CONFLICT DO NOTHING` anchor on the statement
    // head. Every non-marker entry is a canonical pack (quote-aware split).
    let mut stmts: Vec<String> = Vec::new();
    for canonical in &batch[..batch.len() - 1] {
        for stmt in split_schema_statements(canonical) {
            let lowered = schema_sql_for_backend(backend, &stmt).into_owned();
            let companions = if backend == DatabaseBackend::Postgres {
                postgres_identity_companions(&stmt)
            } else {
                Vec::new()
            };
            stmts.push(lowered);
            stmts.extend(companions);
        }
    }
    stmts.push(version.clone());
    Some(stmts)
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
#[must_use]
pub fn expand_host_schema_execute_request(
    backend: DatabaseBackend,
    req: &ExecuteRequest,
) -> ExecuteRequest {
    let batch: Vec<String> = req.statements.iter().map(|s| s.sql.clone()).collect();
    let Some(expanded) = expand_host_schema_batch(backend, &batch) else {
        return req.clone();
    };
    // Rebuild even when the statement count is unchanged: per-statement
    // lowering may have rewritten SQL without splitting the pack further.
    if expanded == batch {
        return req.clone();
    }
    let Some(template) = req.statements.first().cloned() else {
        return req.clone();
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
    ExecuteRequest {
        statements,
        ..req.clone()
    }
}

/// Collapses statement results for an adapter-expanded host-schema request
/// back to the original wire request shape.
///
/// The canonical pack (statement 0) reports the summed `rowsAffected` of its
/// expanded statements; trailing statements (version marker, …) map
/// one-to-one, so the reply stays positional against the request the host
/// actually sent.
#[must_use]
pub fn collapse_host_schema_results(
    original_len: usize,
    results: Vec<bookclerk_plugin_abi::StatementResult>,
) -> Vec<bookclerk_plugin_abi::StatementResult> {
    if original_len == 0 || results.len() <= original_len {
        return results;
    }
    let tail = original_len - 1;
    let pack_len = results.len() - tail;
    let pack_affected: u64 = results[..pack_len]
        .iter()
        .map(|r| r.rows_affected)
        .fold(0, u64::saturating_add);
    let mut out = Vec::with_capacity(original_len);
    out.push(bookclerk_plugin_abi::StatementResult::from_affected(
        pack_affected,
    ));
    out.extend(results.into_iter().skip(pack_len));
    out
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
        let out = collapse_host_schema_results(2, results);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].rows_affected, 6, "pack sums expanded statements");
        assert_eq!(out[1].rows_affected, 1, "marker maps one-to-one");
        // No-op when the adapter did not expand.
        let same = collapse_host_schema_results(
            2,
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
    }

    #[test]
    fn expand_host_schema_batch_keeps_semicolon_inside_quoted_default() {
        let canonical = "CREATE TABLE IF NOT EXISTS notes (body TEXT NOT NULL DEFAULT 'a;b');\n\
             CREATE TABLE IF NOT EXISTS tags (id INTEGER);";
        let naive = canonical
            .split(';')
            .filter(|s| !s.trim().is_empty())
            .count();
        assert!(
            naive > 2,
            "raw split must be the broken baseline this test locks out: {naive}"
        );
        let stmts = split_schema_statements(canonical);
        assert_eq!(stmts.len(), 2, "{stmts:?}");
        assert!(
            stmts[0].contains("DEFAULT 'a;b'"),
            "quoted semicolon must not become a statement boundary: {stmts:?}"
        );
        let batch = vec![
            canonical.to_string(),
            "INSERT INTO schema_migrations (version) VALUES (1)".to_string(),
        ];
        let expanded =
            expand_host_schema_batch(DatabaseBackend::Sqlite, &batch).expect("host schema batch");
        assert_eq!(expanded.len(), 3, "{expanded:?}");
        assert!(
            expanded[0].contains("DEFAULT 'a;b'"),
            "adapter expand must keep the DEFAULT literal intact: {expanded:?}"
        );
        assert!(
            expanded[1].contains("CREATE TABLE IF NOT EXISTS tags"),
            "second CREATE must survive as its own statement: {expanded:?}"
        );
        assert_eq!(
            expanded.last().map(String::as_str),
            Some("INSERT INTO schema_migrations (version) VALUES (1)")
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
