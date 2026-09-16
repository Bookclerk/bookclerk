//! Host-side grammar and scope checks for guest-authored typed SQL.
//!
//! Overwriting [`crate::DbPlanStatementKind`] is classification, not
//! authorization. Guests may only run canonical DML/SELECT against allowed
//! tables, with bind counts and result-selection fields that match the
//! statement. Host-authored schema batches must not use this path.

#![allow(clippy::missing_docs_in_private_items)]

use crate::{
    sql_proof::{ResolvedStatement, PHYSICAL_STAR_COLUMN},
    sql_types::{
        require_sql_v1_helper_arity, sql_host_bookkeeping_type_env, typecheck_execute_request,
        typecheck_execute_request_resolved, SqlType, SqlTypeEnv, INSERT_SELECT_WRAP_ALIAS,
        SQL_CATALOG_TABLE, SQL_IDENTITY_TABLE, SQL_SCHEMA_TABLE,
    },
    DbPlanStatementKind, DbResultSelection, ExecuteRequest, PluginError, Result, TypedDbStatement,
};

/// First-party catalog / engine tables guests must not name.
const DENIED_TABLES: &[&str] = &[
    "encrypted_secrets",
    "sqlite_master",
    "sqlite_temp_master",
    "sqlite_schema",
    "sqlite_temp_schema",
    "sqlite_sequence",
    "information_schema",
    SQL_CATALOG_TABLE,
    SQL_IDENTITY_TABLE,
    SQL_SCHEMA_TABLE,
    INSERT_SELECT_WRAP_ALIAS,
];

/// Leading verbs that are never valid guest SQL (DDL, session, admin).
const DENIED_VERBS: &[&str] = &[
    "ALTER",
    "ANALYZE",
    "ATTACH",
    "BEGIN",
    "CALL",
    "CHECKPOINT",
    "CLUSTER",
    "COMMENT",
    "COMMIT",
    "COPY",
    "CREATE",
    "DEALLOCATE",
    "DECLARE",
    "DETACH",
    "DISCARD",
    "DO",
    "DROP",
    "END",
    "EXECUTE",
    "EXPLAIN",
    "GRANT",
    "INSTALL",
    "LISTEN",
    "LOAD",
    "LOCK",
    "NOTIFY",
    "PRAGMA",
    "PREPARE",
    "REINDEX",
    "RELEASE",
    "REPLACE",
    "REVOKE",
    "ROLLBACK",
    "SAVEPOINT",
    "SECURITY",
    "SET",
    "START",
    "TRUNCATE",
    "UNLISTEN",
    "VACUUM",
];

/// Host-issued scope for one granted database binding.
///
/// Empty [`Self::deny_all`] denies every table. Catalog identifiers
/// (`encrypted_secrets`, `sqlite_*`, `pg_*`, `information_schema`) are always
/// refused even if listed. [`Self::host_authoritative`] skips table-scope
/// authorization so a Cap'n `DatabaseSession` can be the single authorization
/// authority; typing still runs at execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestSqlPolicy {
    tables: std::collections::BTreeSet<String>,
    columns: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    functions: std::collections::BTreeSet<String>,
    /// When true, table/column/function checks are deferred to the host session.
    host_authoritative: bool,
    /// Plugin-owned isolated database: any non-reserved unqualified table is
    /// allowed and bounded idempotent DDL (`CREATE`/`DROP` `TABLE`/`INDEX`
    /// with `IF [NOT] EXISTS`) passes. `ALTER` is refused.
    binding_owned: bool,
    /// Durable/loaded column types for fail-closed expression checking.
    sql_types: SqlTypeEnv,
}

impl Default for GuestSqlPolicy {
    fn default() -> Self {
        Self::deny_all()
    }
}

impl GuestSqlPolicy {
    /// No tables, columns, or functions.
    #[must_use]
    pub fn deny_all() -> Self {
        Self {
            tables: std::collections::BTreeSet::new(),
            columns: std::collections::BTreeMap::new(),
            functions: std::collections::BTreeSet::new(),
            host_authoritative: false,
            binding_owned: false,
            sql_types: SqlTypeEnv::new(),
        }
    }

    /// Broker passthrough: grammar/size checks still run; table scope is the
    /// host `DatabaseSession`'s responsibility. Typing and TEXT lowering still
    /// run at execute against the host catalog.
    #[must_use]
    pub fn host_authoritative() -> Self {
        Self {
            tables: std::collections::BTreeSet::new(),
            columns: std::collections::BTreeMap::new(),
            functions: std::collections::BTreeSet::new(),
            host_authoritative: true,
            binding_owned: false,
            sql_types: SqlTypeEnv::new(),
        }
    }

    /// Plugin-owned isolated database binding (Workers-D1-like ownership).
    ///
    /// The plugin owns and migrates its own schema on a **physically separate**
    /// database (SQLite file / Postgres database / D1 database). Bounded
    /// idempotent DDL (`CREATE`/`DROP` `TABLE`/`INDEX` with `IF [NOT] EXISTS`)
    /// is allowed and any table may be named — except reserved host
    /// bookkeeping (`db_atomic_receipts`, `schema_migrations`,
    /// `plugin_databases`), catalog identifiers, and schema-qualified names.
    /// `CREATE TABLE AS`, `ALTER`, and unqualified `CREATE`/`DROP` without
    /// `IF [NOT] EXISTS` are refused. Grammar and size checks still run.
    /// Functions are the Bookclerk SQL v1 portable set (not a wider SQLite
    /// dialect): contract helpers plus portable scalars. SQLite-only names
    /// such as `strftime`, `typeof`, `group_concat`, `iif`, `instr`, `quote`,
    /// `total`, `date`, `datetime`, and `time` are denied.
    #[must_use]
    pub fn binding_owned() -> Self {
        Self {
            tables: std::collections::BTreeSet::new(),
            columns: std::collections::BTreeMap::new(),
            functions: portable_functions(),
            host_authoritative: false,
            binding_owned: true,
            sql_types: SqlTypeEnv::new(),
        }
    }

    /// True when this policy is a plugin-owned binding scope.
    #[must_use]
    pub fn is_binding_owned(&self) -> bool {
        self.binding_owned
    }

    /// Allows `tables` with builtin scalar functions and any column on those tables.
    #[must_use]
    pub fn allow_tables(tables: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self {
            tables: tables
                .into_iter()
                .map(|t| normalize_ident(t.as_ref()))
                .filter(|t| !t.is_empty())
                .collect(),
            columns: std::collections::BTreeMap::new(),
            functions: builtin_functions(),
            host_authoritative: false,
            binding_owned: false,
            sql_types: SqlTypeEnv::new(),
        }
    }

    /// Attaches a loaded [`SqlTypeEnv`] (durable catalog snapshot).
    #[must_use]
    pub fn with_sql_types(mut self, env: SqlTypeEnv) -> Self {
        self.sql_types = env;
        self
    }

    /// Column types used by fail-closed expression checking.
    #[must_use]
    pub fn sql_types(&self) -> &SqlTypeEnv {
        &self.sql_types
    }
    /// Restricts `table` to `cols` (empty set denies every column on that table).
    #[must_use]
    pub fn restrict_columns(
        mut self,
        table: &str,
        cols: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Self {
        self.columns.insert(
            normalize_ident(table),
            cols.into_iter()
                .map(|c| normalize_ident(c.as_ref()))
                .filter(|c| !c.is_empty())
                .collect(),
        );
        self
    }

    /// Authorizes parsed refs against this allowlist.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::invalid_params`] when a table, column, or
    /// function is outside this policy.
    fn authorize(&self, index: usize, refs: &GuestSqlRefs) -> Result<()> {
        if self.host_authoritative {
            return Ok(());
        }
        for table in &refs.tables {
            if is_cte_name(refs, table) {
                continue;
            }
            self.authorize_table(index, table)?;
        }
        for (table, column) in &refs.columns {
            if self.binding_owned {
                // No column restrictions inside a plugin-owned binding; table
                // scope was already enforced above and qualified column tables
                // are covered by the qualified-name denial.
                if let Some(table) = table {
                    if !is_cte_name(refs, table) {
                        self.authorize_table(index, table)?;
                    }
                }
                continue;
            }
            if column == "*" {
                match table {
                    Some(table) => {
                        if !is_cte_name(refs, table) && self.columns.contains_key(table) {
                            return Err(PluginError::invalid_params(format!(
                                "statement {index} SELECT * is not allowed on column-restricted table {table}"
                            )));
                        }
                    }
                    None => {
                        for table in &refs.tables {
                            if self.columns.contains_key(table) {
                                return Err(PluginError::invalid_params(format!(
                                    "statement {index} SELECT * is not allowed on column-restricted table {table}"
                                )));
                            }
                        }
                    }
                }
                continue;
            }
            let table = match table {
                Some(table) if is_cte_name(refs, table) => continue,
                Some(table) => table.clone(),
                None => {
                    if refs.tables.len() == 1 {
                        refs.tables[0].clone()
                    } else if refs.tables.is_empty() && !refs.ctes.is_empty() {
                        continue;
                    } else {
                        return Err(PluginError::invalid_params(format!(
                            "statement {index} unqualified column {column} cannot be resolved"
                        )));
                    }
                }
            };
            self.authorize_table(index, &table)?;
            if let Some(allowed) = self.columns.get(&table) {
                if !allowed.contains(column) {
                    return Err(PluginError::invalid_params(format!(
                        "statement {index} names unauthorized column {table}.{column}"
                    )));
                }
            }
        }
        for func in &refs.functions {
            if !self.functions.contains(func) {
                return Err(PluginError::invalid_params(format!(
                    "statement {index} names unauthorized function {func}"
                )));
            }
        }
        Ok(())
    }

    /// Authorizes one table name.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::invalid_params`] when `table` is catalog-denied
    /// or not in this policy.
    fn authorize_table(&self, index: usize, table: &str) -> Result<()> {
        if self.binding_owned {
            if binding_table_denied(table) {
                return Err(PluginError::invalid_params(format!(
                    "statement {index} names reserved or qualified table {table}"
                )));
            }
            return Ok(());
        }
        if table_denied(table) || !self.tables.contains(&normalize_ident(table)) {
            return Err(PluginError::invalid_params(format!(
                "statement {index} names unauthorized table {table}"
            )));
        }
        Ok(())
    }
}

/// Host bookkeeping tables reserved inside a plugin-owned binding database.
const BINDING_RESERVED_TABLES: &[&str] = &[
    "db_atomic_receipts",
    "schema_migrations",
    "plugin_databases",
    SQL_CATALOG_TABLE,
    SQL_IDENTITY_TABLE,
    SQL_SCHEMA_TABLE,
    INSERT_SELECT_WRAP_ALIAS,
];

/// True when `name` may not be touched inside a plugin-owned binding.
///
/// Denies catalog identifiers, reserved host bookkeeping, and any
/// schema-qualified name (defense in depth: bindings are physically
/// separate databases, so a qualified name still must not name another
/// catalog).
fn binding_table_denied(name: &str) -> bool {
    if name.contains('.') || table_denied(name) {
        return true;
    }
    let lower = normalize_ident(name);
    BINDING_RESERVED_TABLES.iter().any(|t| *t == lower)
}

fn builtin_functions() -> std::collections::BTreeSet<String> {
    [
        "abs",
        "avg",
        "cast",
        "coalesce",
        "count",
        "date",
        "datetime",
        "group_concat",
        "hex",
        "ifnull",
        "iif",
        "instr",
        "json_array",
        "json_extract",
        "json_object",
        "json_valid",
        "length",
        "lower",
        "max",
        "min",
        "nullif",
        "quote",
        "replace",
        "round",
        "strftime",
        "substr",
        "sum",
        "time",
        "total",
        "trim",
        "typeof",
        "upper",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Portable Bookclerk SQL v1 functions for plugin-owned bindings.
///
/// Contract helpers (`ifnull`, `json_extract`, `json_object`, `json_valid`,
/// 2+-arg `min`/`max`) plus scalars that execute on every admitted adapter
/// after adapter-owned lowering. `hex` is SQLite-only (Postgres has no
/// `hex()`); it stays in [`builtin_functions`] for library guests. Other
/// SQLite-only names in [`builtin_functions`] stay denied here so a binding
/// cannot depend on an engine dialect the host SQL contract does not
/// guarantee.
fn portable_functions() -> std::collections::BTreeSet<String> {
    [
        "abs",
        "avg",
        "cast",
        "coalesce",
        "count",
        "ifnull",
        "json_extract",
        "json_object",
        "json_valid",
        "length",
        "lower",
        "max",
        "min",
        "nullif",
        "replace",
        "round",
        "substr",
        "sum",
        "trim",
        "upper",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Tables, columns, and functions referenced by guest SQL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuestSqlRefs {
    /// FROM/JOIN/INTO/UPDATE/TABLE names.
    pub tables: Vec<String>,
    /// `(table, column)` pairs; `table` is `None` when unqualified.
    pub columns: Vec<(Option<String>, String)>,
    /// Function names (`ident(`).
    pub functions: Vec<String>,
    /// CTE names in scope (not physical tables).
    pub ctes: Vec<String>,
}

/// Authorizes parsed table/column/function refs against a host-issued policy.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a referenced object is outside
/// `policy`.
pub fn authorize_guest_sql_policy(req: &ExecuteRequest, policy: &GuestSqlPolicy) -> Result<()> {
    if policy.host_authoritative {
        return Ok(());
    }
    if policy.is_binding_owned() {
        let mut env = sql_host_bookkeeping_type_env();
        env.merge(policy.sql_types());
        let proofs = typecheck_execute_request_resolved(req, &env)?;
        for (i, (stmt, proof)) in req.statements.iter().zip(proofs.iter()).enumerate() {
            if binding_ddl_verb(&stmt.sql).is_some() {
                authorize_binding_ddl(i, &stmt.sql, policy)?;
                continue;
            }
            authorize_from_proof(i, proof, policy)?;
        }
        return Ok(());
    }
    if !policy.sql_types().is_empty() {
        let proofs = typecheck_execute_request_resolved(req, policy.sql_types())?;
        for (i, proof) in proofs.iter().enumerate() {
            authorize_from_proof(i, proof, policy)?;
        }
        return Ok(());
    }
    for (i, stmt) in req.statements.iter().enumerate() {
        policy.authorize(i, &parse_guest_sql_refs(&stmt.sql)?)?;
    }
    Ok(())
}

/// Authorizes one resolved statement from the typed proof (no lexical reparse).
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a proven table or column is
/// outside `policy`.
fn authorize_from_proof(
    index: usize,
    proof: &ResolvedStatement,
    policy: &GuestSqlPolicy,
) -> Result<()> {
    for access in &proof.physical_accesses {
        policy.authorize_table(index, &access.table)?;
        if policy.binding_owned {
            continue;
        }
        match access.column.as_deref() {
            Some(PHYSICAL_STAR_COLUMN) => {
                if policy.columns.contains_key(&access.table) {
                    return Err(PluginError::invalid_params(format!(
                        "statement {index} SELECT * is not allowed on column-restricted table {}",
                        access.table
                    )));
                }
            }
            Some(column) => {
                if let Some(allowed) = policy.columns.get(&access.table) {
                    if !allowed.contains(column) {
                        return Err(PluginError::invalid_params(format!(
                            "statement {index} names unauthorized column {}.{}",
                            access.table, column
                        )));
                    }
                }
            }
            None => {}
        }
    }
    for func in &proof.functions {
        if !policy.functions.contains(func) {
            return Err(PluginError::invalid_params(format!(
                "statement {index} names unauthorized function {func}"
            )));
        }
    }
    Ok(())
}

/// The leading verb when `sql` is a binding-scope DDL statement.
fn binding_ddl_verb(sql: &str) -> Option<String> {
    let verb = first_top_level_keyword(sql)?;
    ["CREATE", "ALTER", "DROP"]
        .iter()
        .any(|v| verb.eq_ignore_ascii_case(v))
        .then_some(verb)
}

/// True when `sql` is a DDL statement (`CREATE` / `ALTER` / `DROP`).
///
/// Used by hosts to skip receipt write-predicates (DDL has no `WHERE`
/// clause); classification only, never authorization.
#[must_use]
pub fn statement_is_ddl(sql: &str) -> bool {
    binding_ddl_verb(sql).is_some()
}

/// Authorizes one bounded DDL statement inside a plugin-owned binding.
///
/// Allowed forms (fail closed on anything else):
/// `CREATE [UNIQUE] INDEX IF NOT EXISTS name ON table (...)`,
/// `CREATE TABLE IF NOT EXISTS name (...)`,
/// `DROP TABLE|INDEX IF EXISTS name`.
/// `ALTER` is refused (not receipt-idempotent). `CREATE TABLE AS` and
/// schema-qualified names anywhere in the statement (including
/// `REFERENCES other.table`) are refused. Unqualified `REFERENCES` targets
/// are authorized with the same reserved-name rules as `CREATE`/`DROP`
/// object names (`db_atomic_receipts`, `schema_migrations`,
/// `plugin_databases`, catalogs). `IF [NOT] EXISTS` is required so
/// a retried D1 batch cannot re-execute non-idempotent DDL.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the statement is outside the
/// bounded binding DDL grammar or names a reserved / qualified object.
fn authorize_binding_ddl(index: usize, sql: &str, policy: &GuestSqlPolicy) -> Result<()> {
    let mut scan = Scan { sql, i: 0 };
    let deny = |what: &str| {
        PluginError::invalid_params(format!(
            "statement {index} is not allowed binding DDL ({what})"
        ))
    };
    if scan.take_kw("CREATE") {
        let _unique = scan.take_kw("UNIQUE");
        if scan.take_kw("TABLE") {
            if !(scan.take_kw("IF") && scan.take_kw("NOT") && scan.take_kw("EXISTS")) {
                return Err(deny("CREATE TABLE requires IF NOT EXISTS"));
            }
            check_binding_ddl_name(index, &mut scan, "table name")?;
            if scan.peek_kw("AS") {
                return Err(deny("CREATE TABLE AS is not allowed"));
            }
            if !scan.take_byte(b'(') {
                return Err(deny("CREATE TABLE requires a column list"));
            }
            let inner = scan
                .take_balanced_inner()
                .ok_or_else(|| deny("unbalanced CREATE TABLE"))?;
            deny_qualified_names_in(index, inner)?;
            deny_select_in_ddl(index, inner)?;
            deny_binding_ddl_object_refs(index, inner)?;
            validate_binding_create_table_v1(index, inner, scan.rest())?;
            deny_qualified_names_in(index, scan.rest())?;
            deny_select_in_ddl(index, scan.rest())?;
            deny_binding_ddl_object_refs(index, scan.rest())?;
            return authorize_ddl_function_fragments(index, policy, &[inner, scan.rest()]);
        }
        if scan.take_kw("INDEX") {
            if !(scan.take_kw("IF") && scan.take_kw("NOT") && scan.take_kw("EXISTS")) {
                return Err(deny("CREATE INDEX requires IF NOT EXISTS"));
            }
            check_binding_ddl_name(index, &mut scan, "index name")?;
            if !scan.take_kw("ON") {
                return Err(deny("CREATE INDEX requires ON <table>"));
            }
            check_binding_ddl_name(index, &mut scan, "indexed table")?;
            if !scan.take_byte(b'(') {
                return Err(deny("CREATE INDEX requires a column list"));
            }
            let inner = scan
                .take_balanced_inner()
                .ok_or_else(|| deny("unbalanced CREATE INDEX"))?;
            deny_qualified_names_in(index, inner)?;
            validate_binding_create_index_v1(index, inner, scan.rest())?;
            deny_qualified_names_in(index, scan.rest())?;
            deny_select_in_ddl(index, scan.rest())?;
            return authorize_ddl_function_fragments(index, policy, &[inner, scan.rest()]);
        }
        return Err(deny("only TABLE and INDEX may be created"));
    }
    if scan.take_kw("ALTER") {
        return Err(deny(
            "ALTER TABLE is not allowed; binding DDL must be idempotent IF [NOT] EXISTS forms",
        ));
    }
    if scan.take_kw("DROP") {
        if !scan.take_kw("TABLE") && !scan.take_kw("INDEX") {
            return Err(deny("only DROP TABLE / DROP INDEX are allowed"));
        }
        if !(scan.take_kw("IF") && scan.take_kw("EXISTS")) {
            return Err(deny("DROP requires IF EXISTS"));
        }
        check_binding_ddl_name(index, &mut scan, "object name")?;
        deny_v1_ddl_tail(index, scan.rest(), false)?;
        deny_qualified_names_in(index, scan.rest())?;
        return authorize_ddl_function_fragments(index, policy, &[scan.rest()]);
    }
    Err(deny("unsupported verb"))
}

/// Bookclerk SQL v1 grammar (plugin SQL), before scope/security authorization.
///
/// Bindings use the portable subset: `INSERT` / `INSERT OR IGNORE` (not
/// `REPLACE`), no `GLOB` / `COLLATE` / `MATCH` / `REGEXP`, and CREATE TABLE
/// types `INTEGER` / `REAL` / `TEXT` / `BLOB` / `BOOLEAN` only. Every guest
/// rejects non-v1 bind spellings (`$n`, `?NNN`). Library guests still reject
/// `REPLACE` as a statement form.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when `sql` is outside SQL v1.
pub fn validate_sql_v1_grammar(sql: &str, binding_owned: bool) -> Result<()> {
    validate_sql_v1_grammar_at(0, sql, binding_owned)
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when `sql` is outside SQL v1.
fn validate_sql_v1_grammar_at(index: usize, sql: &str, binding_owned: bool) -> Result<()> {
    deny_non_v1_placeholders(index, sql)?;
    deny_non_v1_insert(index, sql)?;
    if binding_owned {
        deny_binding_v1_operators(index, sql)?;
    }
    let verb = first_top_level_keyword(sql).unwrap_or_default();
    if ["CREATE", "ALTER", "DROP"]
        .iter()
        .any(|v| verb.eq_ignore_ascii_case(v))
    {
        return Ok(());
    }
    if DENIED_VERBS.iter().any(|v| verb.eq_ignore_ascii_case(v)) {
        return Err(v1_grammar_err(
            index,
            &format!("disallowed SQL verb {verb}"),
        ));
    }
    parse_positive_sql_v1(index, sql)
}

fn v1_grammar_err(index: usize, what: &str) -> PluginError {
    PluginError::invalid_params(format!(
        "statement {index} is not Bookclerk SQL v1 ({what})"
    ))
}

/// Rejects `"…"`, `` `…` ``, and `[…]` object names.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn deny_quoted_ident(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    scan.skip();
    match scan.sql.as_bytes().get(scan.i) {
        Some(b'"' | b'`' | b'[') => Err(v1_grammar_err(
            index,
            "quoted identifiers are not SQL v1; use unquoted names",
        )),
        _ => Ok(()),
    }
}

/// Fail-closed SELECT/DML/expression productions for SQL v1.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_positive_sql_v1(index: usize, sql: &str) -> Result<()> {
    let mut scan = Scan { sql, i: 0 };
    parse_v1_statement(index, &mut scan)?;
    scan.skip();
    if scan.i < sql.len() {
        return Err(v1_grammar_err(index, "trailing tokens are not SQL v1"));
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_statement(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    if scan.take_kw("WITH") {
        parse_v1_cte_list(index, scan)?;
    }
    if scan.peek_kw("SELECT") || scan.peek_kw("VALUES") {
        return parse_v1_select(index, scan);
    }
    if scan.peek_kw("INSERT") {
        return parse_v1_insert(index, scan);
    }
    if scan.peek_kw("UPDATE") {
        return parse_v1_update(index, scan);
    }
    if scan.peek_kw("DELETE") {
        return parse_v1_delete(index, scan);
    }
    Err(v1_grammar_err(index, "unsupported statement form"))
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_cte_list(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    let _ = scan.take_kw("RECURSIVE");
    loop {
        parse_v1_ident(index, scan)?;
        if scan.take_byte(b'(') {
            loop {
                parse_v1_ident(index, scan)?;
                if !scan.take_byte(b',') {
                    break;
                }
            }
            if !scan.take_byte(b')') {
                return Err(v1_grammar_err(index, "CTE column list"));
            }
        }
        if !scan.take_kw("AS") {
            return Err(v1_grammar_err(index, "CTE AS"));
        }
        let _ = scan.take_kw("NOT");
        let _ = scan.take_kw("MATERIALIZED");
        if !scan.take_byte(b'(') {
            return Err(v1_grammar_err(index, "CTE body"));
        }
        let inner = scan
            .take_balanced_inner()
            .ok_or_else(|| v1_grammar_err(index, "CTE body"))?;
        let mut body = Scan { sql: inner, i: 0 };
        if body.peek_kw("INSERT") || body.peek_kw("UPDATE") || body.peek_kw("DELETE") {
            return Err(v1_grammar_err(index, "nested DML in WITH is not SQL v1"));
        }
        parse_v1_select(index, &mut body)?;
        body.skip();
        if body.i < inner.len() {
            return Err(v1_grammar_err(index, "CTE body trailing tokens"));
        }
        if !scan.take_byte(b',') {
            return Ok(());
        }
    }
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_select(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    if scan.take_kw("VALUES") {
        parse_v1_values_tuples(index, scan)?;
        parse_v1_order_limit(index, scan)?;
        return Ok(());
    }
    loop {
        parse_v1_select_core(index, scan)?;
        if scan.take_kw("UNION") {
            let _ = scan.take_kw("ALL");
            continue;
        }
        if scan.peek_kw("EXCEPT") || scan.peek_kw("INTERSECT") {
            return Err(v1_grammar_err(index, "EXCEPT/INTERSECT are not SQL v1"));
        }
        break;
    }
    parse_v1_order_limit(index, scan)?;
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_select_core(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    if !scan.take_kw("SELECT") {
        return Err(v1_grammar_err(index, "SELECT required"));
    }
    if scan.take_kw("DISTINCT") {
        if scan.peek_kw("ON") {
            return Err(v1_grammar_err(index, "DISTINCT ON is not SQL v1"));
        }
    } else {
        let _ = scan.take_kw("ALL");
    }
    parse_v1_select_list(index, scan)?;
    if scan.take_kw("FROM") {
        loop {
            parse_v1_from_item(index, scan)?;
            parse_v1_joins(index, scan)?;
            if !scan.take_byte(b',') {
                break;
            }
        }
    }
    if scan.take_kw("WHERE") {
        parse_v1_expr(index, scan)?;
    }
    if scan.take_kw("GROUP") {
        if !scan.take_kw("BY") {
            return Err(v1_grammar_err(index, "GROUP BY"));
        }
        loop {
            parse_v1_expr(index, scan)?;
            if !scan.take_byte(b',') {
                break;
            }
        }
    }
    if scan.take_kw("HAVING") {
        parse_v1_expr(index, scan)?;
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_select_list(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    loop {
        if scan.take_byte(b'*') {
            // whole-row *
        } else {
            parse_v1_expr(index, scan)?;
            if scan.take_kw("AS") {
                parse_v1_ident(index, scan)?;
            } else {
                let _ = parse_v1_optional_alias(scan);
            }
        }
        if !scan.take_byte(b',') {
            return Ok(());
        }
    }
}

fn parse_v1_optional_alias(scan: &mut Scan<'_>) -> bool {
    if scan.peek_kw("FROM")
        || scan.peek_kw("WHERE")
        || scan.peek_kw("GROUP")
        || scan.peek_kw("HAVING")
        || scan.peek_kw("ORDER")
        || scan.peek_kw("LIMIT")
        || scan.peek_kw("OFFSET")
        || scan.peek_kw("UNION")
        || scan.peek_kw("EXCEPT")
        || scan.peek_kw("INTERSECT")
        || scan.peek_kw("RETURNING")
        || scan.peek_kw("JOIN")
        || scan.peek_kw("INNER")
        || scan.peek_kw("LEFT")
        || scan.peek_kw("CROSS")
        || scan.peek_kw("RIGHT")
        || scan.peek_kw("FULL")
        || scan.peek_kw("ON")
        || scan.peek_kw("USING")
        || scan.peek_kw("AS")
    {
        return false;
    }
    scan.skip();
    if matches!(scan.sql.as_bytes().get(scan.i), Some(b'"' | b'`' | b'[')) {
        return false;
    }
    scan.read_ident().is_some()
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_from_item(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    if scan.take_byte(b'(') {
        let inner = scan
            .take_balanced_inner()
            .ok_or_else(|| v1_grammar_err(index, "subquery"))?;
        let mut body = Scan { sql: inner, i: 0 };
        parse_v1_select(index, &mut body)?;
        body.skip();
        if body.i < inner.len() {
            return Err(v1_grammar_err(index, "subquery trailing tokens"));
        }
        let _ = scan.take_kw("AS");
        let _ = parse_v1_optional_alias(scan);
        return Ok(());
    }
    parse_v1_table_name(index, scan)?;
    if scan.take_byte(b'(') {
        return Err(v1_grammar_err(index, "table functions are not SQL v1"));
    }
    if scan.take_kw("AS") {
        parse_v1_ident(index, scan)?;
    } else {
        let _ = parse_v1_optional_alias(scan);
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_joins(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    loop {
        let _ = scan.take_kw("INNER");
        if scan.take_kw("LEFT") {
            let _ = scan.take_kw("OUTER");
            if !scan.take_kw("JOIN") {
                return Err(v1_grammar_err(index, "LEFT JOIN"));
            }
        } else if scan.take_kw("CROSS") {
            if !scan.take_kw("JOIN") {
                return Err(v1_grammar_err(index, "CROSS JOIN"));
            }
        } else if scan.take_kw("RIGHT") || scan.take_kw("FULL") {
            return Err(v1_grammar_err(index, "RIGHT/FULL JOIN is not SQL v1"));
        } else if !scan.take_kw("JOIN") {
            return Ok(());
        }
        parse_v1_from_item(index, scan)?;
        if scan.take_kw("ON") {
            parse_v1_expr(index, scan)?;
        } else if scan.take_kw("USING") {
            if !scan.take_byte(b'(') {
                return Err(v1_grammar_err(index, "USING ("));
            }
            loop {
                parse_v1_ident(index, scan)?;
                if !scan.take_byte(b',') {
                    break;
                }
            }
            if !scan.take_byte(b')') {
                return Err(v1_grammar_err(index, "USING )"));
            }
        }
    }
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_order_limit(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    if scan.take_kw("ORDER") {
        if !scan.take_kw("BY") {
            return Err(v1_grammar_err(index, "ORDER BY"));
        }
        loop {
            parse_v1_expr(index, scan)?;
            let _ = scan.take_kw("ASC");
            let _ = scan.take_kw("DESC");
            if scan.take_kw("NULLS") && !scan.take_kw("FIRST") && !scan.take_kw("LAST") {
                return Err(v1_grammar_err(index, "NULLS FIRST|LAST"));
            }
            if !scan.take_byte(b',') {
                break;
            }
        }
    }
    if scan.take_kw("LIMIT") {
        parse_v1_expr(index, scan)?;
    }
    if scan.take_kw("OFFSET") {
        parse_v1_expr(index, scan)?;
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_insert(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    if !scan.take_kw("INSERT") {
        return Err(v1_grammar_err(index, "INSERT"));
    }
    if scan.take_kw("OR") && !scan.take_kw("IGNORE") {
        return Err(v1_grammar_err(index, "INSERT OR IGNORE"));
    }
    if !scan.take_kw("INTO") {
        return Err(v1_grammar_err(index, "INSERT INTO"));
    }
    parse_v1_table_name(index, scan)?;
    if scan.take_byte(b'(') {
        loop {
            parse_v1_ident(index, scan)?;
            if !scan.take_byte(b',') {
                break;
            }
        }
        if !scan.take_byte(b')') {
            return Err(v1_grammar_err(index, "INSERT column list"));
        }
    }
    if scan.peek_kw("SELECT") || scan.peek_kw("WITH") || scan.peek_kw("VALUES") {
        if scan.peek_kw("VALUES") {
            scan.take_kw("VALUES");
            parse_v1_values_tuples(index, scan)?;
        } else {
            if scan.peek_kw("WITH") {
                scan.take_kw("WITH");
                parse_v1_cte_list(index, scan)?;
            }
            parse_v1_select(index, scan)?;
        }
    } else {
        return Err(v1_grammar_err(index, "INSERT VALUES or SELECT"));
    }
    parse_v1_returning(index, scan)
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_values_tuples(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    loop {
        if !scan.take_byte(b'(') {
            return Err(v1_grammar_err(index, "VALUES tuple"));
        }
        loop {
            parse_v1_expr(index, scan)?;
            if !scan.take_byte(b',') {
                break;
            }
        }
        if !scan.take_byte(b')') {
            return Err(v1_grammar_err(index, "VALUES tuple close"));
        }
        if !scan.take_byte(b',') {
            return Ok(());
        }
    }
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_update(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    if !scan.take_kw("UPDATE") {
        return Err(v1_grammar_err(index, "UPDATE"));
    }
    parse_v1_table_name(index, scan)?;
    if !scan.take_kw("SET") {
        return Err(v1_grammar_err(index, "UPDATE SET"));
    }
    loop {
        parse_v1_ident(index, scan)?;
        if !scan.take_byte(b'=') {
            return Err(v1_grammar_err(index, "SET ="));
        }
        parse_v1_expr(index, scan)?;
        if !scan.take_byte(b',') {
            break;
        }
    }
    if scan.take_kw("WHERE") {
        parse_v1_expr(index, scan)?;
    }
    parse_v1_returning(index, scan)
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_delete(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    if !scan.take_kw("DELETE") || !scan.take_kw("FROM") {
        return Err(v1_grammar_err(index, "DELETE FROM"));
    }
    parse_v1_table_name(index, scan)?;
    if scan.take_kw("WHERE") {
        parse_v1_expr(index, scan)?;
    }
    parse_v1_returning(index, scan)
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_returning(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    if !scan.take_kw("RETURNING") {
        return Ok(());
    }
    if scan.take_byte(b'*') {
        return Ok(());
    }
    loop {
        parse_v1_expr(index, scan)?;
        if scan.take_kw("AS") {
            parse_v1_ident(index, scan)?;
        }
        if !scan.take_byte(b',') {
            return Ok(());
        }
    }
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_ident(index: usize, scan: &mut Scan<'_>) -> Result<String> {
    deny_quoted_ident(index, scan)?;
    let ident = scan
        .read_ident()
        .ok_or_else(|| v1_grammar_err(index, "identifier required"))?;
    reject_oversize_ident(index, &ident)?;
    Ok(ident)
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn reject_oversize_ident(index: usize, ident: &str) -> Result<()> {
    if crate::sql_types::sql_v1_ident_in_bounds(ident) {
        Ok(())
    } else {
        Err(v1_grammar_err(
            index,
            &format!(
                "identifier exceeds {} bytes after case fold",
                crate::sql_types::SQL_V1_MAX_IDENT_BYTES
            ),
        ))
    }
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_table_name(index: usize, scan: &mut Scan<'_>) -> Result<String> {
    let name = parse_v1_ident(index, scan)?;
    if scan.peek_byte(b'.') {
        return Err(v1_grammar_err(
            index,
            "schema-qualified names are not SQL v1",
        ));
    }
    Ok(name)
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_expr(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    deny_double_colon(index, scan)?;
    parse_v1_or(index, scan)
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn deny_double_colon(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    scan.skip();
    if scan.sql[scan.i..].starts_with("::") {
        return Err(v1_grammar_err(
            index,
            ":: casts are not SQL v1; use CAST(x AS TYPE)",
        ));
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_or(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    parse_v1_and(index, scan)?;
    while scan.take_kw("OR") {
        parse_v1_and(index, scan)?;
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_and(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    parse_v1_not(index, scan)?;
    while scan.take_kw("AND") {
        parse_v1_not(index, scan)?;
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_not(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    if scan.take_kw("NOT") {
        return parse_v1_not(index, scan);
    }
    parse_v1_cmp(index, scan)
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_cmp(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    parse_v1_concat(index, scan)?;
    deny_double_colon(index, scan)?;
    if scan.take_kw("IS") {
        let _ = scan.take_kw("NOT");
        if !scan.take_kw("NULL") {
            return Err(v1_grammar_err(index, "IS [NOT] NULL"));
        }
        return Ok(());
    }
    if scan.take_kw("NOT") {
        if scan.take_kw("LIKE") {
            if scan.peek_kw("ILIKE") {
                return Err(v1_grammar_err(index, "ILIKE is not SQL v1"));
            }
            parse_v1_concat(index, scan)?;
            return Ok(());
        }
        if scan.take_kw("IN") {
            return parse_v1_in_list(index, scan);
        }
        if scan.take_kw("BETWEEN") {
            parse_v1_concat(index, scan)?;
            if !scan.take_kw("AND") {
                return Err(v1_grammar_err(index, "BETWEEN AND"));
            }
            parse_v1_concat(index, scan)?;
            return Ok(());
        }
        return Err(v1_grammar_err(index, "NOT LIKE/IN/BETWEEN"));
    }
    if scan.peek_kw("ILIKE") || scan.take_kw("ILIKE") {
        return Err(v1_grammar_err(index, "ILIKE is not SQL v1"));
    }
    if scan.take_kw("LIKE") {
        parse_v1_concat(index, scan)?;
        return Ok(());
    }
    if scan.take_kw("IN") {
        return parse_v1_in_list(index, scan);
    }
    if scan.take_kw("BETWEEN") {
        parse_v1_concat(index, scan)?;
        if !scan.take_kw("AND") {
            return Err(v1_grammar_err(index, "BETWEEN AND"));
        }
        parse_v1_concat(index, scan)?;
        return Ok(());
    }
    if scan.take_kw("GLOB")
        || scan.take_kw("MATCH")
        || scan.take_kw("REGEXP")
        || scan.take_kw("COLLATE")
    {
        return Err(v1_grammar_err(
            index,
            "GLOB/MATCH/REGEXP/COLLATE are not SQL v1 operators",
        ));
    }
    if take_cmp_op(scan) {
        parse_v1_concat(index, scan)?;
    }
    Ok(())
}

fn take_cmp_op(scan: &mut Scan<'_>) -> bool {
    scan.skip();
    let rest = &scan.sql[scan.i..];
    for op in ["<=", ">=", "<>", "!=", "=", "<", ">"] {
        if rest.starts_with(op) {
            scan.i += op.len();
            return true;
        }
    }
    false
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_in_list(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    if !scan.take_byte(b'(') {
        return Err(v1_grammar_err(index, "IN ("));
    }
    let inner = scan
        .take_balanced_inner()
        .ok_or_else(|| v1_grammar_err(index, "IN list"))?;
    let mut body = Scan { sql: inner, i: 0 };
    if body.peek_kw("SELECT") || body.peek_kw("WITH") || body.peek_kw("VALUES") {
        parse_v1_select(index, &mut body)?;
    } else {
        loop {
            parse_v1_expr(index, &mut body)?;
            if !body.take_byte(b',') {
                break;
            }
        }
    }
    body.skip();
    if body.i < inner.len() {
        return Err(v1_grammar_err(index, "IN list trailing tokens"));
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_concat(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    parse_v1_add(index, scan)?;
    loop {
        scan.skip();
        if scan.sql[scan.i..].starts_with("||") {
            scan.i += 2;
            parse_v1_add(index, scan)?;
            continue;
        }
        break;
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_add(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    parse_v1_mul(index, scan)?;
    loop {
        scan.skip();
        if scan.take_byte(b'+') || scan.take_byte(b'-') {
            parse_v1_mul(index, scan)?;
            continue;
        }
        break;
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_mul(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    parse_v1_prefix(index, scan)?;
    loop {
        scan.skip();
        if scan.take_byte(b'*') || scan.take_byte(b'/') || scan.take_byte(b'%') {
            parse_v1_prefix(index, scan)?;
            continue;
        }
        break;
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_prefix(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    scan.skip();
    deny_double_colon(index, scan)?;
    if scan.take_byte(b'+') || scan.take_byte(b'-') {
        return parse_v1_prefix(index, scan);
    }
    parse_v1_atom(index, scan)
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_atom(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    deny_double_colon(index, scan)?;
    if scan.take_kw("NULL") || scan.take_kw("TRUE") || scan.take_kw("FALSE") {
        return Ok(());
    }
    if scan.take_kw("EXISTS") {
        if !scan.take_byte(b'(') {
            return Err(v1_grammar_err(index, "EXISTS ("));
        }
        let inner = scan
            .take_balanced_inner()
            .ok_or_else(|| v1_grammar_err(index, "EXISTS subquery"))?;
        let mut body = Scan { sql: inner, i: 0 };
        parse_v1_select(index, &mut body)?;
        return Ok(());
    }
    if scan.take_kw("CASE") {
        return parse_v1_case(index, scan);
    }
    if scan.take_kw("CAST") {
        if !scan.take_byte(b'(') {
            return Err(v1_grammar_err(index, "CAST ("));
        }
        parse_v1_expr(index, scan)?;
        if !scan.take_kw("AS") {
            return Err(v1_grammar_err(index, "CAST AS"));
        }
        let ty = parse_v1_ident(index, scan)?;
        if !["integer", "real", "text", "blob", "boolean"].contains(&ty.as_str()) {
            return Err(v1_grammar_err(
                index,
                "CAST type must be INTEGER, REAL, TEXT, BLOB, or BOOLEAN",
            ));
        }
        if !scan.take_byte(b')') {
            return Err(v1_grammar_err(index, "CAST )"));
        }
        return Ok(());
    }
    if scan.take_byte(b'?') {
        return Ok(());
    }
    if scan.take_byte(b'(') {
        let inner = scan
            .take_balanced_inner()
            .ok_or_else(|| v1_grammar_err(index, "parenthesized expr"))?;
        let mut body = Scan { sql: inner, i: 0 };
        if body.peek_kw("SELECT") || body.peek_kw("WITH") || body.peek_kw("VALUES") {
            parse_v1_select(index, &mut body)?;
        } else {
            parse_v1_expr(index, &mut body)?;
        }
        body.skip();
        if body.i < inner.len() {
            return Err(v1_grammar_err(index, "parenthesized trailing tokens"));
        }
        return Ok(());
    }
    if take_v1_blob_hex(scan) {
        return Ok(());
    }
    if take_v1_number(scan) || take_v1_string(scan) {
        return Ok(());
    }
    deny_quoted_ident(index, scan)?;
    let name = parse_v1_ident(index, scan)?;
    if scan.take_byte(b'.') {
        if scan.take_byte(b'*') {
            return Ok(());
        }
        parse_v1_ident(index, scan)?;
    }
    if scan.take_byte(b'(') {
        parse_v1_call_args(index, scan, &name)?;
        if scan.peek_kw("OVER") || scan.peek_kw("FILTER") {
            return Err(v1_grammar_err(index, "OVER/FILTER are not SQL v1"));
        }
        if scan.peek_kw("SIMILAR") {
            return Err(v1_grammar_err(index, "SIMILAR is not SQL v1"));
        }
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_call_args(index: usize, scan: &mut Scan<'_>, name: &str) -> Result<()> {
    if scan.peek_byte(b')') {
        scan.take_byte(b')');
        return check_v1_arity(index, name, 0);
    }
    if scan.take_byte(b'*') {
        if !scan.take_byte(b')') {
            return Err(v1_grammar_err(index, "count(*)"));
        }
        return check_v1_arity(index, name, 1);
    }
    let mut n = 0usize;
    loop {
        parse_v1_expr(index, scan)?;
        n += 1;
        if !scan.take_byte(b',') {
            break;
        }
    }
    if !scan.take_byte(b')') {
        return Err(v1_grammar_err(index, "call )"));
    }
    check_v1_arity(index, name, n)
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn check_v1_arity(index: usize, name: &str, n: usize) -> Result<()> {
    require_sql_v1_helper_arity(index, name, n)
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the construct is not SQL v1.
fn parse_v1_case(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    if !scan.peek_kw("WHEN") {
        parse_v1_expr(index, scan)?;
    }
    let mut saw_when = false;
    while scan.take_kw("WHEN") {
        saw_when = true;
        parse_v1_expr(index, scan)?;
        if !scan.take_kw("THEN") {
            return Err(v1_grammar_err(index, "CASE WHEN THEN"));
        }
        parse_v1_expr(index, scan)?;
    }
    if !saw_when {
        return Err(v1_grammar_err(index, "CASE WHEN"));
    }
    if scan.take_kw("ELSE") {
        parse_v1_expr(index, scan)?;
    }
    if !scan.take_kw("END") {
        return Err(v1_grammar_err(index, "CASE END"));
    }
    Ok(())
}

fn take_v1_number(scan: &mut Scan<'_>) -> bool {
    scan.skip();
    let bytes = scan.sql.as_bytes();
    let mut i = scan.i;
    if i >= bytes.len() {
        return false;
    }
    let mut saw = false;
    if bytes[i] == b'.' {
        i += 1;
    }
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        saw = true;
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' && bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit()) {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        saw = true;
    }
    if !saw {
        return false;
    }
    scan.i = i;
    true
}

fn take_v1_string(scan: &mut Scan<'_>) -> bool {
    scan.skip();
    if scan.sql.as_bytes().get(scan.i) != Some(&b'\'') {
        return false;
    }
    skip_quoted_string(scan, b'\'');
    true
}

/// `INSERT OR IGNORE` is canonical; `INSERT OR REPLACE` / `OR ABORT` are not.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when `sql` uses a non-portable
/// `INSERT OR` conflict verb.
fn deny_non_v1_insert(index: usize, sql: &str) -> Result<()> {
    let main = sql_after_leading_ctes(sql);
    let mut scan = Scan { sql: main, i: 0 };
    if !scan.take_kw("INSERT") {
        return Ok(());
    }
    if !scan.take_kw("OR") {
        return Ok(());
    }
    if scan.take_kw("IGNORE") {
        return Ok(());
    }
    let conflict = scan.read_ident().unwrap_or_else(|| "unknown".to_string());
    Err(v1_grammar_err(
        index,
        &format!("INSERT OR {conflict} is not portable; use INSERT OR IGNORE"),
    ))
}

/// SQL v1 binds are bare `?` only (`$n` and `?NNN` are engine-specific).
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a code span contains `$n` or
/// `?NNN`.
fn deny_non_v1_placeholders(index: usize, sql: &str) -> Result<()> {
    let mut err = None;
    for_each_unquoted(sql, |slice, i| {
        if err.is_some() {
            return 1;
        }
        let bytes = slice.as_bytes();
        let c = bytes[i];
        if c == b'?' {
            if bytes.get(i + 1) == Some(&b'?') {
                return 2;
            }
            if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                err = Some(v1_grammar_err(
                    index,
                    "?NNN bind placeholders are not SQL v1; use bare ?",
                ));
            }
            return 1;
        }
        if c == b'$' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            err = Some(v1_grammar_err(
                index,
                "$n bind placeholders are not SQL v1; use bare ?",
            ));
        }
        1
    });
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// SQLite-only operators that the expression scanner would otherwise treat as columns.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when `GLOB`, `COLLATE`, `MATCH`, or
/// `REGEXP` appears in a code span.
fn deny_binding_v1_operators(index: usize, sql: &str) -> Result<()> {
    let mut scan = Scan { sql, i: 0 };
    while scan.i < sql.len() {
        scan.skip();
        if scan.i >= sql.len() {
            break;
        }
        let b = scan.sql.as_bytes()[scan.i];
        if b == b'\'' {
            skip_quoted_string(&mut scan, b'\'');
            continue;
        }
        if b == b'"' || b == b'`' || b == b'[' {
            let _ = scan.read_ident();
            continue;
        }
        for kw in ["GLOB", "COLLATE", "MATCH", "REGEXP"] {
            if scan.peek_kw(kw) {
                return Err(v1_grammar_err(
                    index,
                    &format!("{kw} is not a Bookclerk SQL v1 operator"),
                ));
            }
        }
        if scan.read_ident().is_some() {
            continue;
        }
        let ch = scan.sql[scan.i..].chars().next().unwrap_or('\0');
        scan.i += ch.len_utf8().max(1);
    }
    Ok(())
}

const V1_COLUMN_TYPES: &[&str] = &["integer", "real", "text", "blob", "boolean"];

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the column list or table tail
/// is not portable SQL v1.
fn validate_binding_create_table_v1(index: usize, inner: &str, tail: &str) -> Result<()> {
    deny_v1_ddl_tail(index, tail, false)?;
    let mut scan = Scan { sql: inner, i: 0 };
    let mut saw_column = false;
    loop {
        scan.skip();
        if scan.i >= inner.len() {
            break;
        }
        if scan.peek_kw("CONSTRAINT")
            || scan.peek_kw("PRIMARY")
            || scan.peek_kw("UNIQUE")
            || scan.peek_kw("CHECK")
            || scan.peek_kw("FOREIGN")
        {
            parse_table_constraint(index, &mut scan)?;
        } else {
            parse_column_def(index, &mut scan)?;
            saw_column = true;
        }
        scan.skip();
        if scan.take_byte(b',') {
            continue;
        }
        scan.skip();
        if scan.i < inner.len() {
            return Err(v1_grammar_err(
                index,
                "CREATE TABLE column list has trailing tokens",
            ));
        }
        break;
    }
    if !saw_column {
        return Err(v1_grammar_err(
            index,
            "CREATE TABLE requires at least one column",
        ));
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when `tail` is a non-portable
/// `STRICT` / `WITHOUT ROWID` / `USING` / `INCLUDE` clause.
fn deny_v1_ddl_tail(index: usize, tail: &str, allow_where: bool) -> Result<()> {
    let mut scan = Scan { sql: tail, i: 0 };
    scan.skip();
    if scan.i >= tail.len() {
        return Ok(());
    }
    if allow_where && scan.peek_kw("WHERE") {
        return Ok(());
    }
    Err(v1_grammar_err(
        index,
        "SQLite-only or engine-specific table/index tail is not portable",
    ))
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the index column list or tail
/// is not portable SQL v1.
fn validate_binding_create_index_v1(index: usize, inner: &str, tail: &str) -> Result<()> {
    deny_v1_ddl_tail(index, tail, true)?;
    deny_binding_v1_operators(index, inner)?;
    let mut scan = Scan { sql: inner, i: 0 };
    loop {
        parse_v1_ident(index, &mut scan)?;
        let _ = scan.take_kw("ASC");
        let _ = scan.take_kw("DESC");
        if scan.take_byte(b',') {
            continue;
        }
        scan.skip();
        if scan.i < inner.len() {
            return Err(v1_grammar_err(
                index,
                "CREATE INDEX column list has trailing tokens",
            ));
        }
        break;
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the column name, type, or
/// constraints are not portable SQL v1.
fn parse_column_def(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    parse_v1_ident(index, scan)?;
    let ty = parse_v1_ident(index, scan)?;
    if !V1_COLUMN_TYPES.contains(&ty.as_str()) {
        return Err(v1_grammar_err(
            index,
            &format!("column type {ty} is not portable; use INTEGER, REAL, TEXT, BLOB, or BOOLEAN"),
        ));
    }
    if scan.peek_byte(b'(') {
        return Err(v1_grammar_err(
            index,
            "column type parameters (VARCHAR(n), NUMERIC(p,s)) are not portable",
        ));
    }
    parse_column_constraints(index, scan, &ty)
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a column constraint is not
/// portable SQL v1.
fn parse_column_constraints(index: usize, scan: &mut Scan<'_>, ty: &str) -> Result<()> {
    let mut saw_autoincrement = false;
    loop {
        scan.skip();
        if scan.i >= scan.sql.len() || scan.peek_byte(b',') {
            return Ok(());
        }
        if scan.peek_kw("FOREIGN") {
            return Ok(());
        }
        if scan.take_kw("CONSTRAINT") {
            parse_v1_ident(index, scan)?;
            continue;
        }
        if scan.take_kw("PRIMARY") {
            if !scan.take_kw("KEY") {
                return Err(v1_grammar_err(index, "PRIMARY KEY required"));
            }
            if scan.take_kw("AUTOINCREMENT") {
                if ty != "integer" {
                    return Err(v1_grammar_err(
                        index,
                        "AUTOINCREMENT is only valid as INTEGER PRIMARY KEY AUTOINCREMENT",
                    ));
                }
                if saw_autoincrement {
                    return Err(v1_grammar_err(index, "AUTOINCREMENT may appear only once"));
                }
                saw_autoincrement = true;
            }
            continue;
        }
        if scan.take_kw("AUTOINCREMENT") {
            return Err(v1_grammar_err(
                index,
                "AUTOINCREMENT is only valid immediately after INTEGER PRIMARY KEY",
            ));
        }
        if scan.take_kw("NOT") {
            if !scan.take_kw("NULL") {
                return Err(v1_grammar_err(index, "NOT NULL required"));
            }
            continue;
        }
        if scan.take_kw("NULL") {
            continue;
        }
        if scan.take_kw("UNIQUE") {
            continue;
        }
        if scan.take_kw("CHECK") {
            skip_paren_group(index, scan)?;
            continue;
        }
        if scan.take_kw("DEFAULT") {
            parse_default_value(index, scan, ty)?;
            continue;
        }
        if scan.take_kw("REFERENCES") {
            parse_references(index, scan)?;
            continue;
        }
        let unknown = scan.read_ident().unwrap_or_else(|| "token".to_string());
        return Err(v1_grammar_err(
            index,
            &format!("constraint {unknown} is not portable"),
        ));
    }
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a table constraint is not
/// portable SQL v1.
fn parse_table_constraint(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    if scan.take_kw("CONSTRAINT") {
        parse_v1_ident(index, scan)?;
    }
    if scan.take_kw("PRIMARY") {
        if !scan.take_kw("KEY") {
            return Err(v1_grammar_err(index, "PRIMARY KEY required"));
        }
        return skip_paren_group(index, scan);
    }
    if scan.take_kw("UNIQUE") {
        return skip_paren_group(index, scan);
    }
    if scan.take_kw("CHECK") {
        return skip_paren_group(index, scan);
    }
    if scan.take_kw("FOREIGN") {
        if !scan.take_kw("KEY") {
            return Err(v1_grammar_err(index, "FOREIGN KEY required"));
        }
        skip_paren_group(index, scan)?;
        if !scan.take_kw("REFERENCES") {
            return Err(v1_grammar_err(index, "FOREIGN KEY requires REFERENCES"));
        }
        return parse_references(index, scan);
    }
    Err(v1_grammar_err(index, "unsupported table constraint"))
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when parentheses are missing or
/// unbalanced.
fn skip_paren_group(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    if !scan.take_byte(b'(') {
        return Err(v1_grammar_err(index, "expected '('"));
    }
    if scan.take_balanced_inner().is_none() {
        return Err(v1_grammar_err(index, "unbalanced parentheses"));
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the `DEFAULT` value is not a
/// portable literal matching `ty`.
fn parse_default_value(index: usize, scan: &mut Scan<'_>, ty: &str) -> Result<()> {
    scan.skip();
    if scan.i >= scan.sql.len() {
        return Err(v1_grammar_err(index, "DEFAULT requires a value"));
    }
    let want = SqlType::from_column_ident(ty)
        .ok_or_else(|| v1_grammar_err(index, "DEFAULT column type"))?;
    let got = parse_default_literal(index, scan)?;
    crate::sql_types::unify_types(index, want, got)?;
    if got != crate::sql_types::SqlType::Null && got != want {
        if !crate::sql_types::cast_is_legal(got, want) {
            return Err(v1_grammar_err(
                index,
                &format!("DEFAULT type {} does not match column {ty}", got.as_str()),
            ));
        }
        if got != want {
            return Err(v1_grammar_err(
                index,
                &format!("DEFAULT type {} does not match column {ty}", got.as_str()),
            ));
        }
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the `DEFAULT` token is not a
/// portable SQL v1 literal.
fn parse_default_literal(index: usize, scan: &mut Scan<'_>) -> Result<SqlType> {
    scan.skip();
    if scan.take_kw("NULL") {
        return Ok(SqlType::Null);
    }
    if scan.take_kw("TRUE") || scan.take_kw("FALSE") {
        return Ok(SqlType::Boolean);
    }
    if take_v1_blob_hex(scan) {
        return Ok(SqlType::Blob);
    }
    if take_v1_string(scan) {
        return Ok(SqlType::Text);
    }
    let num_start = {
        scan.skip();
        scan.i
    };
    if take_v1_number(scan) {
        let lit = &scan.sql[num_start..scan.i];
        return Ok(if lit.contains('.') {
            SqlType::Real
        } else {
            SqlType::Integer
        });
    }
    if scan.take_kw("CAST") {
        if !scan.take_byte(b'(') {
            return Err(v1_grammar_err(index, "DEFAULT CAST ("));
        }
        let from = parse_default_literal(index, scan)?;
        if !scan.take_kw("AS") {
            return Err(v1_grammar_err(index, "DEFAULT CAST AS"));
        }
        let ty = parse_v1_ident(index, scan)?;
        if !scan.take_byte(b')') {
            return Err(v1_grammar_err(index, "DEFAULT CAST )"));
        }
        let to = SqlType::from_column_ident(&ty)
            .ok_or_else(|| v1_grammar_err(index, "DEFAULT CAST type"))?;
        if !crate::sql_types::cast_is_legal(from, to) {
            return Err(v1_grammar_err(
                index,
                &format!(
                    "DEFAULT CAST from {} to {} is not SQL v1",
                    from.as_str(),
                    to.as_str()
                ),
            ));
        }
        return Ok(to);
    }
    if scan.take_byte(b'(') {
        return Err(v1_grammar_err(
            index,
            "parenthesized DEFAULT is not SQL v1 unless CAST",
        ));
    }
    Err(v1_grammar_err(index, "DEFAULT value is not portable"))
}

fn take_v1_blob_hex(scan: &mut Scan<'_>) -> bool {
    scan.skip();
    let rest = scan.rest();
    if rest.len() < 3 {
        return false;
    }
    let b = rest.as_bytes();
    if !b[0].eq_ignore_ascii_case(&b'x') || b[1] != b'\'' {
        return false;
    }
    let mut i = scan.i + 2;
    while i < scan.sql.len() {
        let c = scan.sql.as_bytes()[i];
        if c == b'\'' {
            scan.i = i + 1;
            return true;
        }
        if !c.is_ascii_hexdigit() {
            return false;
        }
        i += 1;
    }
    false
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when `REFERENCES` is missing a
/// table or uses a non-portable action.
fn parse_references(index: usize, scan: &mut Scan<'_>) -> Result<()> {
    if scan.read_ident().is_none() {
        return Err(v1_grammar_err(index, "REFERENCES requires a table"));
    }
    if scan.peek_byte(b'(') {
        skip_paren_group(index, scan)?;
    }
    while scan.take_kw("ON") {
        if !scan.take_kw("DELETE") && !scan.take_kw("UPDATE") {
            return Err(v1_grammar_err(index, "ON DELETE/UPDATE required"));
        }
        if scan.take_kw("NO") {
            if !scan.take_kw("ACTION") {
                return Err(v1_grammar_err(index, "NO ACTION required"));
            }
            continue;
        }
        if scan.take_kw("SET") {
            if !scan.take_kw("NULL") && !scan.take_kw("DEFAULT") {
                return Err(v1_grammar_err(index, "SET NULL/DEFAULT required"));
            }
            continue;
        }
        if scan.take_kw("CASCADE") || scan.take_kw("RESTRICT") {
            continue;
        }
        return Err(v1_grammar_err(index, "unsupported REFERENCES action"));
    }
    Ok(())
}

/// Fails closed when `sql` contains a schema-qualified `ident.ident` (the
/// class of escape used by `REFERENCES public.books` and CTAS from another
/// schema). String literals and quoted identifiers are skipped.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when `sql` contains a
/// schema-qualified identifier.
fn deny_qualified_names_in(index: usize, sql: &str) -> Result<()> {
    let mut scan = Scan { sql, i: 0 };
    while scan.i < sql.len() {
        scan.skip();
        if scan.i >= sql.len() {
            break;
        }
        let b = scan.sql.as_bytes()[scan.i];
        if b == b'\'' {
            skip_quoted_string(&mut scan, b'\'');
            continue;
        }
        if scan.read_ident().is_some() {
            if scan.take_byte(b'.') {
                return Err(PluginError::invalid_params(format!(
                    "statement {index} names reserved or qualified object"
                )));
            }
            continue;
        }
        scan.i += 1;
    }
    Ok(())
}

/// Fails closed when DDL contains a top-level `SELECT` (CTAS / subquery copy).
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a top-level `SELECT` appears
/// in `sql`.
fn deny_select_in_ddl(index: usize, sql: &str) -> Result<()> {
    if has_top_level_keyword(sql, "SELECT") {
        return Err(PluginError::invalid_params(format!(
            "statement {index} is not allowed binding DDL (SELECT in DDL is not allowed)"
        )));
    }
    Ok(())
}

/// Authorizes every `REFERENCES` target in a binding DDL fragment.
///
/// Column-level (`col TYPE REFERENCES t(id)`) and table-level
/// (`FOREIGN KEY (col) REFERENCES t(id)`) forms both go through
/// [`check_binding_ddl_name`]. Qualified names are already denied by
/// [`deny_qualified_names_in`]; this catches reserved host bookkeeping
/// that would otherwise be skipped by [`ddl_function_names`].
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a `REFERENCES` target is
/// missing, reserved, or schema-qualified.
fn deny_binding_ddl_object_refs(index: usize, sql: &str) -> Result<()> {
    let mut scan = Scan { sql, i: 0 };
    while scan.i < sql.len() {
        scan.skip();
        if scan.i >= sql.len() {
            break;
        }
        let b = scan.sql.as_bytes()[scan.i];
        if b == b'\'' {
            skip_quoted_string(&mut scan, b'\'');
            continue;
        }
        if scan.take_kw("REFERENCES") {
            check_binding_ddl_name(index, &mut scan, "REFERENCES target")?;
            if scan.take_byte(b'(') {
                let _ = scan.take_balanced_inner();
            }
            continue;
        }
        if scan.read_ident().is_some() {
            continue;
        }
        scan.i += 1;
    }
    Ok(())
}

/// Authorizes function calls embedded in binding DDL fragments (DEFAULT /
/// CHECK / generated columns / index expressions) against the same function
/// allowlist as guest DML for this policy.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a function is not on the
/// policy allowlist.
fn authorize_ddl_function_fragments(
    index: usize,
    policy: &GuestSqlPolicy,
    fragments: &[&str],
) -> Result<()> {
    let mut refs = GuestSqlRefs::default();
    for fragment in fragments {
        refs.functions.extend(ddl_function_names(fragment));
    }
    policy.authorize(index, &refs)
}

/// Function names in a DDL fragment (`ident(`), skipping type precision,
/// `REFERENCES` column lists, constraint heads, and quoted strings.
fn ddl_function_names(sql: &str) -> Vec<String> {
    let mut scan = Scan { sql, i: 0 };
    let mut out = Vec::new();
    while scan.i < sql.len() {
        scan.skip();
        if scan.i >= sql.len() {
            break;
        }
        let b = scan.sql.as_bytes()[scan.i];
        if b == b'\'' {
            skip_quoted_string(&mut scan, b'\'');
            continue;
        }
        if scan.take_kw("REFERENCES") {
            let _ = scan.read_ident();
            if scan.take_byte(b'(') {
                let _ = scan.take_balanced_inner();
            }
            continue;
        }
        if let Some(ident) = scan.read_ident() {
            if scan.take_byte(b'(') {
                let inner = scan.take_balanced_inner().unwrap_or("");
                if ddl_type_name(&ident) {
                    continue;
                }
                if !sql_keyword(&ident) && !ddl_constraint_head(&ident) {
                    out.push(ident);
                }
                out.extend(ddl_function_names(inner));
                continue;
            }
            continue;
        }
        scan.i += 1;
    }
    out
}

/// Constraint / clause heads that take a parenthesized body, not a call.
fn ddl_constraint_head(name: &str) -> bool {
    matches!(
        name,
        "check"
            | "unique"
            | "primary"
            | "foreign"
            | "key"
            | "constraint"
            | "generated"
            | "default"
            | "collate"
            | "using"
            | "where"
            | "on"
    )
}

/// SQL type names that take a parenthesized precision/scale list, not a call.
fn ddl_type_name(name: &str) -> bool {
    matches!(
        name,
        "varchar"
            | "character"
            | "nchar"
            | "nvarchar"
            | "char"
            | "numeric"
            | "decimal"
            | "float"
            | "double"
            | "real"
            | "timestamp"
            | "time"
            | "interval"
            | "bit"
            | "binary"
            | "varbinary"
    )
}

/// Reads one DDL object name off `scan`, denying reserved and qualified names.
///
/// A trailing `.` after the identifier means a schema-qualified name,
/// which could name another catalog — fail closed.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the next token is missing, a
/// reserved host bookkeeping name, or schema-qualified.
fn check_binding_ddl_name(index: usize, scan: &mut Scan<'_>, what: &str) -> Result<()> {
    scan.skip();
    deny_quoted_ident(index, scan)?;
    let name = scan.read_ident().ok_or_else(|| {
        PluginError::invalid_params(format!(
            "statement {index} is not allowed binding DDL ({what})"
        ))
    })?;
    reject_oversize_ident(index, &name)?;
    if scan.take_byte(b'.') || binding_table_denied(&name) {
        return Err(PluginError::invalid_params(format!(
            "statement {index} names reserved or qualified object {name}"
        )));
    }
    Ok(())
}

/// Classifies guest SQL the same way the host stamps `DbStatement.kind`.
///
/// Leading `WITH` / `WITH RECURSIVE` CTE lists are skipped so the **main**
/// statement decides the kind: `WITH … SELECT` is [`DbPlanStatementKind::Select`],
/// `WITH … INSERT/UPDATE/DELETE` is [`DbPlanStatementKind::Execute`], and a
/// top-level `RETURNING` on that main statement is [`DbPlanStatementKind::Returning`].
#[must_use]
pub fn guest_statement_kind(sql: &str) -> DbPlanStatementKind {
    let main = sql_after_leading_ctes(sql);
    if has_top_level_keyword(main, "RETURNING") {
        return DbPlanStatementKind::Returning;
    }
    match first_top_level_keyword(main).as_deref() {
        Some("SELECT" | "VALUES") => DbPlanStatementKind::Select,
        _ => DbPlanStatementKind::Execute,
    }
}

/// Proves from SQL shape alone that a `RETURNING` statement yields at most
/// one row.
///
/// Conservative and fail-closed — a caller-asserted `maxRows` is never
/// trusted. Only a single-statement `INSERT` (after any leading CTE list)
/// whose row source is exactly one top-level `VALUES` tuple is proven.
/// `INSERT … SELECT`, `UPDATE … RETURNING`, `DELETE … RETURNING`, and
/// multi-tuple `VALUES` are never proven, because the SQL text does not bound
/// how many rows they touch.
#[must_use]
pub fn returning_single_row_proven(sql: &str) -> bool {
    if has_top_level_semicolon_tail(sql) {
        return false;
    }
    let main = sql_after_leading_ctes(sql);
    if !matches!(first_top_level_keyword(main).as_deref(), Some("INSERT")) {
        return false;
    }
    // A top-level SELECT row source is unbounded; nested `(SELECT …)` scalar
    // expressions sit at depth > 0 and do not trip this check.
    if has_top_level_keyword(main, "SELECT") {
        return false;
    }
    count_top_level_values_tuples(main) == 1
}

/// Number of top-level `VALUES` tuples (`(1),(2)` → 2). `0` when none parsed.
fn count_top_level_values_tuples(sql: &str) -> usize {
    let mut values_at = None;
    for_each_top_level_keyword(sql, |idx, kw| {
        if values_at.is_none() && kw.eq_ignore_ascii_case("VALUES") {
            values_at = Some(idx);
        }
    });
    let Some(idx) = values_at else {
        return 0;
    };
    let bytes = sql.as_bytes();
    let mut i = idx + "VALUES".len();
    let mut depth = 0usize;
    let mut tuples = 0usize;
    let mut in_squote = false;
    let mut in_dquote = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_squote {
            if c == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_squote = false;
            }
            i += 1;
            continue;
        }
        if in_dquote {
            if c == b'"' {
                if bytes.get(i + 1) == Some(&b'"') {
                    i += 2;
                    continue;
                }
                in_dquote = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_squote = true,
            b'"' => in_dquote = true,
            b'(' => {
                if depth == 0 {
                    tuples = tuples.saturating_add(1);
                }
                depth = depth.saturating_add(1);
            }
            b')' => depth = depth.saturating_sub(1),
            _ => {
                if depth == 0 {
                    let rest = &sql[i..];
                    if rest.len() >= 9 && rest[..9].eq_ignore_ascii_case("RETURNING") {
                        break;
                    }
                    if c == b';' {
                        break;
                    }
                }
            }
        }
        i += 1;
    }
    tuples
}

/// Remainder after a leading `WITH [RECURSIVE] cte [, cte …]` list.
///
/// Returns `sql` unchanged when it does not start with `WITH` or the CTE list
/// cannot be parsed (callers then classify from the original text).
fn sql_after_leading_ctes(sql: &str) -> &str {
    let mut i = skip_ws_comments(sql, 0);
    if !keyword_at(sql, i, "WITH") {
        return sql;
    }
    i += 4;
    i = skip_ws_comments(sql, i);
    if keyword_at(sql, i, "RECURSIVE") {
        i += 9;
        i = skip_ws_comments(sql, i);
    }
    loop {
        let Some(next) = skip_ident_or_quoted(sql, i) else {
            return sql;
        };
        i = skip_ws_comments(sql, next);
        if sql.as_bytes().get(i) == Some(&b'(') {
            let Some(next) = skip_balanced_parens(sql, i) else {
                return sql;
            };
            i = skip_ws_comments(sql, next);
        }
        if !keyword_at(sql, i, "AS") {
            return sql;
        }
        i = skip_ws_comments(sql, i + 2);
        if keyword_at(sql, i, "NOT") {
            let after_not = skip_ws_comments(sql, i + 3);
            if keyword_at(sql, after_not, "MATERIALIZED") {
                i = skip_ws_comments(sql, after_not + 12);
            }
        } else if keyword_at(sql, i, "MATERIALIZED") {
            i = skip_ws_comments(sql, i + 12);
        }
        if sql.as_bytes().get(i) != Some(&b'(') {
            return sql;
        }
        let Some(next) = skip_balanced_parens(sql, i) else {
            return sql;
        };
        i = skip_ws_comments(sql, next);
        if sql.as_bytes().get(i) == Some(&b',') {
            i = skip_ws_comments(sql, i + 1);
            continue;
        }
        return &sql[i..];
    }
}

fn skip_ws_comments(sql: &str, mut i: usize) -> usize {
    let bytes = sql.as_bytes();
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) == Some(&b'-') && bytes.get(i + 1) == Some(&b'-') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = i.saturating_add(2).min(bytes.len());
            continue;
        }
        break;
    }
    i
}

fn keyword_at(sql: &str, i: usize, kw: &str) -> bool {
    if !sql.is_char_boundary(i) {
        return false;
    }
    let n = kw.len();
    let Some(end) = i.checked_add(n) else {
        return false;
    };
    if end > sql.len() || !sql.is_char_boundary(end) {
        return false;
    }
    if !sql[i..end].eq_ignore_ascii_case(kw) {
        return false;
    }
    let bytes = sql.as_bytes();
    let before_ok = i == 0 || !ident_cont(bytes[i - 1]);
    let after = bytes.get(end).copied().unwrap_or(b' ');
    before_ok && !ident_cont(after)
}

fn skip_ident_or_quoted(sql: &str, i: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    let c = *bytes.get(i)?;
    if c == b'"' || c == b'`' || c == b'[' {
        let end = if c == b'[' { b']' } else { c };
        let mut j = i + 1;
        while j < bytes.len() {
            if bytes[j] == end {
                if end != b']' && bytes.get(j + 1) == Some(&end) {
                    j += 2;
                    continue;
                }
                return Some(j + 1);
            }
            j += 1;
        }
        return None;
    }
    if ident_start(c) {
        let mut j = i + 1;
        while j < bytes.len() && ident_cont(bytes[j]) {
            j += 1;
        }
        return Some(j);
    }
    None
}

fn skip_balanced_parens(sql: &str, start: usize) -> Option<usize> {
    if sql.as_bytes().get(start) != Some(&b'(') {
        return None;
    }
    let mut depth = 0usize;
    let mut end = None;
    let slice = &sql[start..];
    for_each_unquoted(slice, |text, i| {
        if end.is_some() {
            return 1;
        }
        match text.as_bytes()[i] {
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    end = Some(start + i + 1);
                }
            }
            _ => {}
        }
        1
    });
    end
}

/// Rejects guest SQL that is outside the Bookclerk binding grammar.
///
/// Checks statement verbs, named tables, positional bind counts,
/// `resultSelection` vs classified kind, and `maxRows` consistency.
/// Does **not** stamp the request hash or measure encoded size.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the request is not allowed.
pub fn validate_guest_execute_request(req: &ExecuteRequest) -> Result<()> {
    validate_guest_execute_request_for_policy(req, &GuestSqlPolicy::deny_all())
}

/// [`validate_guest_execute_request`] with policy-dependent grammar.
///
/// Pipeline: plugin SQL → SQL-v1 grammar validation → binding
/// scope/security authorization → capability/limit validation → canonical
/// [`ExecuteRequest`]. Adapter-edge lowering happens later, at execute,
/// against the isolated physical DB.
///
/// A [`GuestSqlPolicy::binding_owned`] policy admits bounded DDL verbs
/// (`CREATE` / `DROP`; `ALTER` is still classified then refused);
/// shapes and names are then authorized by
/// [`authorize_guest_sql_policy`]. Every other policy uses the fixed
/// DML/SELECT grammar.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the request is not allowed.
pub fn validate_guest_execute_request_for_policy(
    req: &ExecuteRequest,
    policy: &GuestSqlPolicy,
) -> Result<()> {
    if req.statements.is_empty() {
        return Err(PluginError::invalid_params(
            "executeAtomic statements must be non-empty",
        ));
    }
    for (i, stmt) in req.statements.iter().enumerate() {
        validate_guest_statement_for(i, stmt, policy)?;
    }
    if policy.is_binding_owned() {
        let mut env = sql_host_bookkeeping_type_env();
        env.merge(policy.sql_types());
        typecheck_execute_request(req, &env)?;
    } else if !policy.sql_types().is_empty() {
        typecheck_execute_request(req, policy.sql_types())?;
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the statement is outside the
/// guest grammar.
fn validate_guest_statement_for(
    index: usize,
    stmt: &TypedDbStatement,
    policy: &GuestSqlPolicy,
) -> Result<()> {
    if stmt.sql.trim().is_empty() {
        return Err(PluginError::invalid_params(format!(
            "statement {index} SQL is empty"
        )));
    }
    if has_top_level_semicolon_tail(&stmt.sql) {
        return Err(PluginError::invalid_params(format!(
            "statement {index} must be a single SQL statement"
        )));
    }
    let Some(verb) = first_top_level_keyword(&stmt.sql) else {
        return Err(PluginError::invalid_params(format!(
            "statement {index} is not valid Bookclerk SQL"
        )));
    };
    validate_sql_v1_grammar_at(index, &stmt.sql, policy.is_binding_owned())?;
    crate::sql_text::require_portable_text(&stmt.sql)
        .map_err(|err| PluginError::invalid_params(format!("statement {index}: {err}")))?;
    crate::sql_text::require_portable_text_binds(&stmt.parameters)
        .map_err(|err| PluginError::invalid_params(format!("statement {index}: {err}")))?;
    let binding_ddl = policy.is_binding_owned()
        && ["CREATE", "ALTER", "DROP"].contains(&verb.to_ascii_uppercase().as_str());
    if !binding_ddl && DENIED_VERBS.iter().any(|v| verb.eq_ignore_ascii_case(v)) {
        return Err(PluginError::invalid_params(format!(
            "statement {index} uses disallowed SQL verb {verb}"
        )));
    }
    if binding_ddl {
        // The DML/SELECT ref parser does not understand DDL shapes; object
        // names are authorized by `authorize_binding_ddl` instead.
        authorize_binding_ddl(index, &stmt.sql, policy)?;
    } else {
        for table in named_tables(&stmt.sql)? {
            if table_denied(&table) {
                return Err(PluginError::invalid_params(format!(
                    "statement {index} names unauthorized table {table}"
                )));
            }
        }
    }
    let expected = count_placeholders(&stmt.sql);
    if expected != stmt.parameters.len() {
        return Err(PluginError::invalid_params(format!(
            "statement {index} has {} bind(s) but SQL has {expected} placeholder(s)",
            stmt.parameters.len()
        )));
    }
    let kind = guest_statement_kind(&stmt.sql);
    validate_selection(index, kind, stmt.result_selection, stmt.max_rows)?;
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when `resultSelection` or `maxRows`
/// does not match the classified statement.
fn validate_selection(
    index: usize,
    kind: DbPlanStatementKind,
    selection: DbResultSelection,
    max_rows: u32,
) -> Result<()> {
    match selection {
        DbResultSelection::Discard => {}
        DbResultSelection::AffectedRows => {
            if !matches!(
                kind,
                DbPlanStatementKind::Execute | DbPlanStatementKind::Returning
            ) {
                return Err(PluginError::invalid_params(format!(
                    "statement {index} resultSelection affectedRows requires DML"
                )));
            }
            if max_rows != 0 {
                return Err(PluginError::invalid_params(format!(
                    "statement {index} affectedRows cannot set maxRows"
                )));
            }
        }
        DbResultSelection::Rows => {}
    }
    Ok(())
}

fn table_denied(name: &str) -> bool {
    name.split('.').any(|part| {
        let lower = normalize_ident(part);
        DENIED_TABLES.iter().any(|t| *t == lower)
            || lower.starts_with("sqlite_")
            || lower.starts_with("pg_")
    })
}

fn normalize_ident(name: &str) -> String {
    name.trim()
        .trim_matches('"')
        .trim_matches('`')
        .trim_matches('[')
        .trim_matches(']')
        .to_ascii_lowercase()
}

/// Parsed table, column, and function names from one guest statement.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the statement uses a form the
/// authorizer cannot fully resolve (fail closed).
pub fn parse_guest_sql_refs(sql: &str) -> Result<GuestSqlRefs> {
    let mut scan = Scan { sql, i: 0 };
    let mut refs = GuestSqlRefs::default();
    let mut ctes = Vec::new();
    if scan.take_kw("WITH") {
        let recursive = scan.take_kw("RECURSIVE");
        loop {
            let name = scan.read_ident().ok_or_else(|| unresolved("CTE name"))?;
            if scan.take_byte(b'(') {
                scan.take_balanced_inner()
                    .ok_or_else(|| unresolved("CTE column list"))?;
            }
            if !scan.take_kw("AS") {
                return Err(unresolved("CTE AS"));
            }
            let _ = scan.take_kw("NOT");
            let _ = scan.take_kw("MATERIALIZED");
            if !scan.take_byte(b'(') {
                return Err(unresolved("CTE body"));
            }
            let body = scan
                .take_balanced_inner()
                .ok_or_else(|| unresolved("CTE body"))?;
            let mut body_ctes = ctes.clone();
            if recursive {
                body_ctes.push(name.clone());
            }
            parse_statement_into(body, &mut refs, &body_ctes)?;
            ctes.push(name);
            if !scan.take_byte(b',') {
                break;
            }
        }
    }
    parse_statement_into(scan.rest(), &mut refs, &ctes)?;
    refs.ctes = ctes;
    refs.tables.sort();
    refs.tables.dedup();
    refs.ctes.sort();
    refs.ctes.dedup();
    Ok(refs)
}

fn unresolved(what: &str) -> PluginError {
    PluginError::invalid_params(format!(
        "guest SQL is not fully resolvable for authorization ({what})"
    ))
}

/// True when parenthesized SQL starts with a statement verb, not an expression.
fn looks_like_sql_statement(sql: &str) -> bool {
    let mut scan = Scan { sql, i: 0 };
    scan.peek_kw("WITH")
        || scan.peek_kw("SELECT")
        || scan.peek_kw("VALUES")
        || scan.peek_kw("INSERT")
        || scan.peek_kw("REPLACE")
        || scan.peek_kw("UPDATE")
        || scan.peek_kw("DELETE")
}

/// True when `name` is a CTE in `refs` (qualified names match on the last label).
fn is_cte_name(refs: &GuestSqlRefs, name: &str) -> bool {
    let last = name.rsplit('.').next().unwrap_or(name);
    refs.ctes.iter().any(|cte| cte == name || cte == last)
}

#[derive(Clone, Copy)]
struct Scan<'a> {
    /// Remaining SQL being scanned.
    sql: &'a str,
    /// Byte offset into [`Self::sql`].
    i: usize,
}

impl<'a> Scan<'a> {
    fn skip(&mut self) {
        self.i = skip_ws_comments(self.sql, self.i);
    }

    fn rest(&self) -> &'a str {
        &self.sql[self.i..]
    }

    fn take_kw(&mut self, kw: &str) -> bool {
        self.skip();
        if keyword_at(self.sql, self.i, kw) {
            self.i += kw.len();
            true
        } else {
            false
        }
    }

    fn peek_kw(&mut self, kw: &str) -> bool {
        self.skip();
        keyword_at(self.sql, self.i, kw)
    }

    fn take_byte(&mut self, b: u8) -> bool {
        self.skip();
        if self.sql.as_bytes().get(self.i) == Some(&b) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn peek_byte(&mut self, b: u8) -> bool {
        self.skip();
        self.sql.as_bytes().get(self.i) == Some(&b)
    }

    fn read_ident(&mut self) -> Option<String> {
        self.skip();
        let start = self.i;
        let end = skip_ident_or_quoted(self.sql, self.i)?;
        self.i = end;
        let ident = normalize_ident(&self.sql[start..end]);
        if ident.is_empty() {
            None
        } else {
            Some(ident)
        }
    }

    fn take_balanced_inner(&mut self) -> Option<&'a str> {
        let start = self.i.saturating_sub(1);
        let end = skip_balanced_parens(self.sql, start)?;
        let inner = &self.sql[start + 1..end - 1];
        self.i = end;
        Some(inner)
    }
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the statement cannot be fully
/// resolved for authorization.
fn parse_statement_into(sql: &str, refs: &mut GuestSqlRefs, ctes: &[String]) -> Result<()> {
    let mut scan = Scan { sql, i: 0 };
    if scan.take_kw("INSERT") {
        if scan.take_kw("OR") && !scan.take_kw("IGNORE") {
            return Err(unresolved("INSERT OR IGNORE"));
        }
        if !scan.take_kw("INTO") {
            return Err(unresolved("INSERT INTO"));
        }
        collect_physical_table(&mut scan, refs, ctes)?;
        if scan.take_byte(b'(') {
            collect_ident_list(&mut scan, refs)?;
            if !scan.take_byte(b')') {
                return Err(unresolved("INSERT column list"));
            }
        }
        collect_expr_tail(&mut scan, refs, ctes)?;
        return Ok(());
    }
    if scan.take_kw("UPDATE") {
        collect_physical_table(&mut scan, refs, ctes)?;
        collect_expr_tail(&mut scan, refs, ctes)?;
        return Ok(());
    }
    if scan.take_kw("DELETE") {
        if !scan.take_kw("FROM") {
            return Err(unresolved("DELETE FROM"));
        }
        collect_physical_table(&mut scan, refs, ctes)?;
        collect_expr_tail(&mut scan, refs, ctes)?;
        return Ok(());
    }
    if scan.take_kw("SELECT") || scan.take_kw("VALUES") {
        collect_expr_tail(&mut scan, refs, ctes)?;
        return Ok(());
    }
    if scan.take_kw("WITH") {
        return parse_guest_sql_refs(sql).map(|nested| {
            refs.tables.extend(nested.tables);
            refs.columns.extend(nested.columns);
            refs.functions.extend(nested.functions);
            refs.ctes.extend(nested.ctes);
        });
    }
    Err(unresolved("statement verb"))
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the table name cannot be read.
fn collect_physical_table(
    scan: &mut Scan<'_>,
    refs: &mut GuestSqlRefs,
    ctes: &[String],
) -> Result<String> {
    if scan.peek_byte(b'(') {
        return Err(unresolved("derived table"));
    }
    let mut name = scan.read_ident().ok_or_else(|| unresolved("table name"))?;
    if scan.take_byte(b'.') {
        let second = scan
            .read_ident()
            .ok_or_else(|| unresolved("qualified table"))?;
        name = format!("{name}.{second}");
    }
    let last = name.rsplit('.').next().unwrap_or(name.as_str());
    if !ctes.iter().any(|c| c == last || c == &name) {
        refs.tables.push(name.clone());
    }
    Ok(name)
}

fn take_alias(scan: &mut Scan<'_>) -> Option<String> {
    if scan.take_kw("AS") {
        return scan.read_ident();
    }
    if scan.peek_kw("WHERE")
        || scan.peek_kw("SET")
        || scan.peek_kw("JOIN")
        || scan.peek_kw("LEFT")
        || scan.peek_kw("RIGHT")
        || scan.peek_kw("INNER")
        || scan.peek_kw("FULL")
        || scan.peek_kw("CROSS")
        || scan.peek_kw("OUTER")
        || scan.peek_kw("ON")
        || scan.peek_kw("USING")
        || scan.peek_kw("GROUP")
        || scan.peek_kw("HAVING")
        || scan.peek_kw("ORDER")
        || scan.peek_kw("LIMIT")
        || scan.peek_kw("OFFSET")
        || scan.peek_kw("RETURNING")
        || scan.peek_kw("UNION")
        || scan.peek_kw("EXCEPT")
        || scan.peek_kw("INTERSECT")
        || scan.peek_kw("FROM")
        || scan.peek_kw("INTO")
        || scan.peek_kw("VALUES")
        || scan.peek_kw("SELECT")
        || scan.peek_kw("AND")
        || scan.peek_kw("OR")
        || scan.peek_byte(b',')
        || scan.peek_byte(b')')
        || scan.peek_byte(b';')
    {
        return None;
    }
    scan.read_ident()
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a column identifier is missing.
fn collect_ident_list(scan: &mut Scan<'_>, refs: &mut GuestSqlRefs) -> Result<()> {
    loop {
        if scan.peek_byte(b')') {
            return Ok(());
        }
        let ident = scan.read_ident().ok_or_else(|| unresolved("column list"))?;
        refs.columns.push((None, ident));
        if !scan.take_byte(b',') {
            return Ok(());
        }
    }
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when an expression or table ref
/// cannot be fully resolved.
fn collect_expr_tail(scan: &mut Scan<'_>, refs: &mut GuestSqlRefs, ctes: &[String]) -> Result<()> {
    let mut aliases: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let start = *scan;
    collect_from_aliases(scan, refs, ctes, &mut aliases)?;
    *scan = start;
    collect_expr_atoms_with_aliases(scan, refs, ctes, &aliases)
}

/// Walks FROM/JOIN sources first so SELECT-list aliases resolve.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a table ref cannot be resolved.
fn collect_from_aliases(
    scan: &mut Scan<'_>,
    refs: &mut GuestSqlRefs,
    ctes: &[String],
    aliases: &mut std::collections::BTreeMap<String, String>,
) -> Result<()> {
    while !scan_at_end(scan) {
        if scan.take_kw("FROM") {
            parse_table_refs(scan, refs, ctes, aliases)?;
            continue;
        }
        if take_join_kw(scan) {
            parse_one_table_ref(scan, refs, ctes, aliases)?;
            continue;
        }
        skip_one_expr_atom(scan, refs, ctes)?;
    }
    Ok(())
}

/// Second pass: column/function refs after FROM aliases are known.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a column, function, or table
/// ref cannot be fully resolved.
fn collect_expr_atoms_with_aliases(
    scan: &mut Scan<'_>,
    refs: &mut GuestSqlRefs,
    ctes: &[String],
    aliases: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    while !scan_at_end(scan) {
        if scan.take_kw("FROM") {
            skip_table_refs(scan)?;
            continue;
        }
        if take_join_kw(scan) {
            skip_one_table_ref(scan)?;
            if scan.take_kw("ON") {
                continue;
            }
            if scan.take_kw("USING") {
                if !scan.take_byte(b'(') {
                    return Err(unresolved("USING"));
                }
                collect_ident_list(scan, refs)?;
                if !scan.take_byte(b')') {
                    return Err(unresolved("USING"));
                }
            }
            continue;
        }
        collect_one_expr_atom(scan, refs, ctes, aliases)?;
        if scan.take_kw("AS") {
            let _ = scan.read_ident();
        }
    }
    Ok(())
}

fn take_join_kw(scan: &mut Scan<'_>) -> bool {
    scan.take_kw("JOIN")
        || (scan.take_kw("INNER") && scan.take_kw("JOIN"))
        || (scan.take_kw("CROSS") && scan.take_kw("JOIN"))
        || join_with_outer(scan)
}

/// Skips a comma-separated FROM list without collecting identifiers.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a table ref is malformed.
fn skip_table_refs(scan: &mut Scan<'_>) -> Result<()> {
    skip_one_table_ref(scan)?;
    while scan.take_byte(b',') {
        skip_one_table_ref(scan)?;
    }
    Ok(())
}

/// Skips one FROM/JOIN table ref (name, alias, or parenthesized subquery).
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the table ref is malformed or
/// a table-valued function.
fn skip_one_table_ref(scan: &mut Scan<'_>) -> Result<()> {
    if scan.take_byte(b'(') {
        scan.take_balanced_inner()
            .ok_or_else(|| unresolved("subquery"))?;
        let _ = take_alias(scan);
        return Ok(());
    }
    let _ = scan.read_ident().ok_or_else(|| unresolved("table name"))?;
    if scan.take_byte(b'.') {
        let _ = scan
            .read_ident()
            .ok_or_else(|| unresolved("qualified table"))?;
    }
    if scan.peek_byte(b'(') {
        return Err(unresolved("table-valued function"));
    }
    let _ = take_alias(scan);
    Ok(())
}

/// Skips one expression atom during the FROM-alias collection pass.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a nested subquery cannot be
/// resolved.
fn skip_one_expr_atom(scan: &mut Scan<'_>, refs: &mut GuestSqlRefs, ctes: &[String]) -> Result<()> {
    scan.skip();
    if scan.i >= scan.sql.len() {
        return Ok(());
    }
    let c = scan.sql.as_bytes()[scan.i];
    if c == b'\'' {
        skip_quoted_string(scan, b'\'');
        return Ok(());
    }
    if c == b'(' {
        scan.i += 1;
        let inner = scan
            .take_balanced_inner()
            .ok_or_else(|| unresolved("subquery"))?;
        if looks_like_sql_statement(inner) {
            parse_statement_into(inner, refs, ctes)?;
        }
        return Ok(());
    }
    if scan.read_ident().is_some() {
        if scan.take_byte(b'(') {
            let _ = scan.take_balanced_inner();
            return Ok(());
        }
        if scan.take_byte(b'.') {
            if scan.take_byte(b'*') {
                return Ok(());
            }
            let _ = scan.read_ident();
            if scan.take_byte(b'(') {
                let _ = scan.take_balanced_inner();
            }
        }
        return Ok(());
    }
    scan.i += 1;
    Ok(())
}

fn join_with_outer(scan: &mut Scan<'_>) -> bool {
    if scan.take_kw("LEFT") || scan.take_kw("RIGHT") || scan.take_kw("FULL") {
        let _ = scan.take_kw("OUTER");
        return scan.take_kw("JOIN");
    }
    false
}

fn scan_at_end(scan: &mut Scan<'_>) -> bool {
    scan.skip();
    scan.i >= scan.sql.len() || scan.peek_byte(b';')
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a FROM/JOIN source cannot be
/// resolved.
fn parse_table_refs(
    scan: &mut Scan<'_>,
    refs: &mut GuestSqlRefs,
    ctes: &[String],
    aliases: &mut std::collections::BTreeMap<String, String>,
) -> Result<()> {
    parse_one_table_ref(scan, refs, ctes, aliases)?;
    while scan.take_byte(b',') {
        parse_one_table_ref(scan, refs, ctes, aliases)?;
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the table, subquery, or
/// table-valued function cannot be resolved.
fn parse_one_table_ref(
    scan: &mut Scan<'_>,
    refs: &mut GuestSqlRefs,
    ctes: &[String],
    aliases: &mut std::collections::BTreeMap<String, String>,
) -> Result<()> {
    if scan.take_byte(b'(') {
        let inner = scan
            .take_balanced_inner()
            .ok_or_else(|| unresolved("subquery"))?;
        parse_statement_into(inner, refs, ctes)?;
        let alias = take_alias(scan);
        let _ = alias;
        return Ok(());
    }
    let table = collect_physical_table(scan, refs, ctes)?;
    if scan.peek_byte(b'(') {
        return Err(unresolved("table-valued function"));
    }
    if let Some(alias) = take_alias(scan) {
        aliases.insert(alias, table);
    }
    Ok(())
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when a qualified column cannot be
/// read.
fn collect_one_expr_atom(
    scan: &mut Scan<'_>,
    refs: &mut GuestSqlRefs,
    ctes: &[String],
    aliases: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    scan.skip();
    if scan.i >= scan.sql.len() {
        return Ok(());
    }
    let c = scan.sql.as_bytes()[scan.i];
    if c == b'\'' {
        skip_quoted_string(scan, b'\'');
        return Ok(());
    }
    if c == b'(' {
        scan.i += 1;
        let inner = scan
            .take_balanced_inner()
            .ok_or_else(|| unresolved("subquery"))?;
        if looks_like_sql_statement(inner) {
            parse_statement_into(inner, refs, ctes)?;
        } else {
            let mut inner_scan = Scan { sql: inner, i: 0 };
            collect_expr_atoms_with_aliases(&mut inner_scan, refs, ctes, aliases)?;
        }
        return Ok(());
    }
    if scan.peek_byte(b'*') {
        scan.i += 1;
        refs.columns.push((None, "*".into()));
        return Ok(());
    }
    if let Some(ident) = scan.read_ident() {
        if scan.take_byte(b'(') {
            if !sql_keyword(&ident) {
                refs.functions.push(ident);
            }
            return Ok(());
        }
        if scan.take_byte(b'.') {
            if scan.take_byte(b'*') {
                let table = aliases.get(&ident).cloned().unwrap_or(ident);
                refs.columns.push((Some(table), "*".into()));
                return Ok(());
            }
            let col = scan
                .read_ident()
                .ok_or_else(|| unresolved("qualified column"))?;
            if scan.take_byte(b'(') {
                if !sql_keyword(&col) {
                    refs.functions.push(col);
                }
                return Ok(());
            }
            let table = aliases.get(&ident).cloned().unwrap_or(ident);
            refs.columns.push((Some(table), col));
            return Ok(());
        }
        if ident == "*" {
            refs.columns.push((None, "*".into()));
            return Ok(());
        }
        if !sql_keyword(&ident) {
            refs.columns.push((None, ident));
        }
        return Ok(());
    }
    scan.i += 1;
    Ok(())
}

fn skip_quoted_string(scan: &mut Scan<'_>, q: u8) {
    scan.i += 1;
    let bytes = scan.sql.as_bytes();
    while scan.i < bytes.len() {
        if bytes[scan.i] == q {
            if bytes.get(scan.i + 1) == Some(&q) {
                scan.i += 2;
                continue;
            }
            scan.i += 1;
            return;
        }
        scan.i += 1;
    }
}

/// # Errors
///
/// Returns [`PluginError::invalid_params`] when table refs cannot be fully
/// resolved.
fn named_tables(sql: &str) -> Result<Vec<String>> {
    Ok(parse_guest_sql_refs(sql)?.tables)
}

fn sql_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "and"
            | "as"
            | "asc"
            | "between"
            | "by"
            | "case"
            | "collate"
            | "cross"
            | "desc"
            | "distinct"
            | "else"
            | "end"
            | "exists"
            | "from"
            | "full"
            | "group"
            | "having"
            | "in"
            | "inner"
            | "insert"
            | "into"
            | "is"
            | "join"
            | "left"
            | "like"
            | "limit"
            | "not"
            | "null"
            | "offset"
            | "on"
            | "or"
            | "order"
            | "outer"
            | "over"
            | "recursive"
            | "replace"
            | "returning"
            | "right"
            | "select"
            | "set"
            | "table"
            | "then"
            | "union"
            | "update"
            | "using"
            | "values"
            | "when"
            | "where"
            | "with"
    )
}

/// Positional bare-`?` count (`??` is an escaped literal, not two binds).
///
/// SQL v1 admits only bare `?`. [`deny_non_v1_placeholders`] rejects `$n` and
/// `?NNN` before this runs.
fn count_placeholders(sql: &str) -> usize {
    let mut n_q = 0usize;
    for_each_unquoted(sql, |slice, i| {
        let c = slice.as_bytes()[i];
        if c == b'?' {
            if slice.as_bytes().get(i + 1) == Some(&b'?') {
                return 2;
            }
            n_q += 1;
        }
        1
    });
    n_q
}

fn ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn ident_cont(c: u8) -> bool {
    ident_start(c) || c.is_ascii_digit()
}

fn first_top_level_keyword(sql: &str) -> Option<String> {
    let mut first = None;
    for_each_top_level_keyword(sql, |_, kw| {
        if first.is_none() {
            first = Some(kw.to_ascii_uppercase());
        }
    });
    first
}

fn has_top_level_keyword(sql: &str, keyword: &str) -> bool {
    let want = keyword.to_ascii_uppercase();
    let mut found = false;
    for_each_top_level_keyword(sql, |_, kw| {
        if kw.eq_ignore_ascii_case(&want) {
            found = true;
        }
    });
    found
}

fn has_top_level_semicolon_tail(sql: &str) -> bool {
    let mut depth = 0usize;
    let mut in_s = false;
    let mut in_d = false;
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_s {
            if c == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_s = false;
            }
            i += 1;
            continue;
        }
        if in_d {
            if c == b'"' {
                if bytes.get(i + 1) == Some(&b'"') {
                    i += 2;
                    continue;
                }
                in_d = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_s = true,
            b'"' => in_d = true,
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b';' if depth == 0 => {
                let rest = sql[i + 1..].trim();
                return !rest.is_empty();
            }
            _ => {}
        }
        i += 1;
    }
    false
}

fn for_each_top_level_keyword(sql: &str, mut on_keyword: impl FnMut(usize, &str)) {
    let bytes = sql.as_bytes();
    let mut depth = 0usize;
    for_each_unquoted(sql, |slice, i| {
        let c = slice.as_bytes()[i];
        if c == b'(' {
            depth += 1;
            return 1;
        }
        if c == b')' {
            depth = depth.saturating_sub(1);
            return 1;
        }
        if depth == 0 && ident_start(c) {
            let mut j = i + 1;
            while j < slice.len() && ident_cont(bytes[j]) {
                j += 1;
            }
            on_keyword(i, &slice[i..j]);
            return j - i;
        }
        1
    });
}

/// Walks `sql`, skipping quoted strings and comments. `step` is given the
/// current index in the original string and returns how many bytes to consume
/// (at least 1). Parentheses are visible so placeholders inside subqueries
/// are counted.
fn for_each_unquoted(sql: &str, mut step: impl FnMut(&str, usize) -> usize) {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut in_s = false;
    let mut in_d = false;
    let mut in_line = false;
    let mut in_block = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_line {
            if c == b'\n' {
                in_line = false;
            }
            i += 1;
            continue;
        }
        if in_block {
            if c == b'*' && bytes.get(i + 1) == Some(&b'/') {
                in_block = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_s {
            if c == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_s = false;
            }
            i += 1;
            continue;
        }
        if in_d {
            if c == b'"' {
                if bytes.get(i + 1) == Some(&b'"') {
                    i += 2;
                    continue;
                }
                in_d = false;
            }
            i += 1;
            continue;
        }
        if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            in_line = true;
            i += 2;
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            in_block = true;
            i += 2;
            continue;
        }
        if c == b'\'' {
            in_s = true;
            i += 1;
            continue;
        }
        if c == b'"' {
            in_d = true;
            i += 1;
            continue;
        }
        let n = step(sql, i).max(1);
        i += n;
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::sql_types::apply_schema_sql_to_env;
    use crate::{DbValue, ExecuteRequest};

    fn req(
        sql: &str,
        params: Vec<DbValue>,
        selection: DbResultSelection,
        max_rows: u32,
    ) -> ExecuteRequest {
        ExecuteRequest {
            operation_id: "op".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: sql.into(),
                parameters: params,
                kind: guest_statement_kind(sql),
                max_rows,
                result_selection: selection,
            }],
            deadline_unix_ms: 0,
        }
    }

    #[test]
    fn sql_v1_grammar_rejects_noncanonical_placeholders_for_every_guest() {
        for sql in [
            "SELECT id FROM books WHERE id = $1",
            "SELECT id FROM books WHERE id = ?1",
            "INSERT INTO books (id) VALUES ($1)",
            "INSERT INTO books (id) VALUES (?2)",
        ] {
            let err = validate_guest_execute_request(&req(
                sql,
                vec![DbValue::Int64(1)],
                DbResultSelection::Rows,
                1,
            ))
            .unwrap_err();
            assert!(
                err.to_string().contains("SQL v1") || err.to_string().contains("placeholder"),
                "{sql}: {err}"
            );
        }
        validate_guest_execute_request(&req(
            "SELECT id FROM books WHERE id = '?' OR id = '$1' -- ?1\nAND body = ?",
            vec![DbValue::Int64(1)],
            DbResultSelection::Rows,
            1,
        ))
        .unwrap();
    }

    #[test]
    fn sql_v1_positive_grammar_rejects_extension_tokens() {
        for sql in [
            "SELECT body FROM books WHERE body ILIKE 'a%'",
            "SELECT DISTINCT ON (id) id FROM books",
            "SELECT id::text FROM books",
            "SELECT CAST(id AS BYTEA) FROM books",
            "SELECT round(id, 2, 3) FROM books",
            r#"SELECT "hex"(id) FROM books"#,
        ] {
            let err = validate_guest_execute_request(&req(sql, vec![], DbResultSelection::Rows, 1))
                .unwrap_err();
            assert!(err.to_string().contains("SQL v1"), "{sql}: {err}");
        }
        validate_guest_execute_request(&req(
            "SELECT body FROM books WHERE body LIKE 'a%' ORDER BY id ASC NULLS FIRST",
            vec![],
            DbResultSelection::Rows,
            1,
        ))
        .unwrap();
    }

    #[test]
    fn binding_owned_sql_v1_rejects_quoted_idents_and_malformed_autoincrement() {
        for sql in [
            r#"CREATE TABLE IF NOT EXISTS "Foo" (id INTEGER PRIMARY KEY)"#,
            "CREATE TABLE IF NOT EXISTS t (id INTEGER AUTOINCREMENT)",
            "CREATE TABLE IF NOT EXISTS t (id REAL PRIMARY KEY AUTOINCREMENT)",
            "CREATE TABLE IF NOT EXISTS t (n INTEGER AUTOINCREMENT)",
        ] {
            let err = binding_check(sql, DbResultSelection::Discard, 0).unwrap_err();
            assert!(
                err.to_string().contains("SQL v1")
                    || err.to_string().contains("quoted")
                    || err.to_string().contains("AUTOINCREMENT"),
                "{sql}: {err}"
            );
        }
        binding_check(
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY AUTOINCREMENT, n INTEGER)",
            DbResultSelection::Discard,
            0,
        )
        .unwrap();
    }

    #[test]
    fn allows_select_and_insert() {
        validate_guest_execute_request(&req(
            "SELECT id FROM books WHERE id = ?",
            vec![DbValue::Int64(1)],
            DbResultSelection::Rows,
            1,
        ))
        .unwrap();
        validate_guest_execute_request(&req(
            "INSERT INTO books (id) VALUES (?)",
            vec![DbValue::Int64(1)],
            DbResultSelection::AffectedRows,
            0,
        ))
        .unwrap();
    }

    #[test]
    fn rejects_ddl_and_pragma() {
        let err = validate_guest_execute_request(&req(
            "CREATE TABLE t (id INTEGER)",
            vec![],
            DbResultSelection::AffectedRows,
            0,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("disallowed"), "{err}");
        let err = validate_guest_execute_request(&req(
            "PRAGMA user_version",
            vec![],
            DbResultSelection::Rows,
            0,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("disallowed"), "{err}");
    }

    /// Validates + policy-authorizes one statement under `binding_owned`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::invalid_params`] when the statement is outside
    /// `GuestSqlPolicy::binding_owned`.
    fn binding_check(sql: &str, selection: DbResultSelection, max_rows: u32) -> Result<()> {
        let mut env = SqlTypeEnv::new();
        apply_schema_sql_to_env(
            &mut env,
            "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, body TEXT, flag BOOLEAN)",
        );
        apply_schema_sql_to_env(
            &mut env,
            "CREATE TABLE IF NOT EXISTS typed (v TEXT, n REAL, r REAL)",
        );
        apply_schema_sql_to_env(&mut env, "CREATE TABLE IF NOT EXISTS ign_sel (id INTEGER)");
        apply_schema_sql_to_env(
            &mut env,
            "CREATE TABLE IF NOT EXISTS anything_i_own (id INTEGER)",
        );
        let policy = GuestSqlPolicy::binding_owned().with_sql_types(env);
        let request = req(sql, vec![], selection, max_rows);
        validate_guest_execute_request_for_policy(&request, &policy)?;
        authorize_guest_sql_policy(&request, &policy)
    }

    #[test]
    fn binding_owned_allows_bounded_ddl_and_any_table_dml() {
        binding_check(
            "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, body TEXT, flag BOOLEAN)",
            DbResultSelection::Discard,
            0,
        )
        .unwrap();
        binding_check(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_body ON notes(body)",
            DbResultSelection::Discard,
            0,
        )
        .unwrap();
        binding_check("DROP TABLE IF EXISTS memos", DbResultSelection::Discard, 0).unwrap();
        binding_check(
            "CREATE TABLE IF NOT EXISTS typed_defaults (v TEXT DEFAULT 'x', n REAL)",
            DbResultSelection::Discard,
            0,
        )
        .unwrap();
        binding_check(
            "CREATE TABLE IF NOT EXISTS blobdef (payload BLOB DEFAULT X'deadbeef')",
            DbResultSelection::Discard,
            0,
        )
        .unwrap();
        binding_check(
            "CREATE TABLE IF NOT EXISTS casted (n INTEGER DEFAULT CAST(1 AS INTEGER))",
            DbResultSelection::Discard,
            0,
        )
        .unwrap();
        binding_check(
            "INSERT OR IGNORE INTO ign_sel (id) WITH s(id) AS (SELECT 1) SELECT * FROM s RETURNING id",
            DbResultSelection::Rows,
            0,
        )
        .unwrap();
        binding_check(
            "CREATE TABLE IF NOT EXISTS keyed (id INTEGER PRIMARY KEY, other_id INTEGER REFERENCES peer(id))",
            DbResultSelection::Discard,
            0,
        )
        .unwrap();
        binding_check(
            "CREATE TABLE IF NOT EXISTS checked (n INTEGER CHECK (n > 0))",
            DbResultSelection::Discard,
            0,
        )
        .unwrap();
        binding_check(
            "SELECT body FROM notes WHERE id = 1",
            DbResultSelection::Rows,
            1,
        )
        .unwrap();
        binding_check(
            "DELETE FROM anything_i_own",
            DbResultSelection::AffectedRows,
            0,
        )
        .unwrap();
        binding_check(
            "SELECT replace(body, 'A', 'Z') FROM notes",
            DbResultSelection::Rows,
            1,
        )
        .unwrap();
    }

    #[test]
    fn sql_v1_identifier_bound_is_63_bytes_after_case_fold() {
        let ok = format!(
            "CREATE TABLE IF NOT EXISTS {} (id INTEGER PRIMARY KEY)",
            "a".repeat(63)
        );
        binding_check(&ok, DbResultSelection::Discard, 0).unwrap();
        let too_long = format!(
            "CREATE TABLE IF NOT EXISTS {} (id INTEGER PRIMARY KEY)",
            "a".repeat(64)
        );
        let err = binding_check(&too_long, DbResultSelection::Discard, 0).unwrap_err();
        assert!(err.to_string().contains("identifier exceeds"), "{err}");
        let col = format!("CREATE TABLE IF NOT EXISTS t ({} INTEGER)", "b".repeat(64));
        let err = binding_check(&col, DbResultSelection::Discard, 0).unwrap_err();
        assert!(err.to_string().contains("identifier exceeds"), "{err}");
    }

    #[test]
    fn binding_owned_sql_v1_grammar_rejects_nonportable_forms() {
        for sql in [
            "REPLACE INTO notes (id) VALUES (1)",
            "INSERT OR REPLACE INTO notes (id) VALUES (1)",
            "INSERT OR ABORT INTO notes (id) VALUES (1)",
            "SELECT body FROM notes WHERE body GLOB 'A*'",
            "SELECT body FROM notes WHERE body COLLATE NOCASE = 'x'",
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY) STRICT",
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY) WITHOUT ROWID",
            "CREATE TABLE IF NOT EXISTS t (doc JSONB)",
            "CREATE TABLE IF NOT EXISTS t (n NUMERIC(10, 2))",
            "CREATE TABLE IF NOT EXISTS t (v VARCHAR(255))",
            "CREATE TABLE IF NOT EXISTS t (body TEXT COLLATE NOCASE)",
            "CREATE INDEX IF NOT EXISTS i ON notes (body COLLATE NOCASE)",
            "CREATE INDEX IF NOT EXISTS i ON notes (body) INCLUDE (id)",
            "CREATE INDEX IF NOT EXISTS i ON notes USING btree (body)",
            "CREATE TABLE IF NOT EXISTS t (flag BOOL)",
            "DROP TABLE IF EXISTS notes CASCADE",
            "DROP TABLE IF EXISTS notes RESTRICT",
            "DROP INDEX IF EXISTS idx_notes_body CASCADE",
            "SELECT body FROM notes WHERE id = $1",
            "SELECT body FROM notes WHERE id = ?1",
            "SELECT body FROM notes WHERE body ILIKE 'a%'",
            "SELECT DISTINCT ON (id) id FROM notes",
            "SELECT body::text FROM notes",
            "SELECT CAST(body AS BYTEA) FROM notes",
            r#"CREATE TABLE IF NOT EXISTS "Foo" (id INTEGER PRIMARY KEY)"#,
            "CREATE TABLE IF NOT EXISTS t (id INTEGER AUTOINCREMENT)",
            "CREATE TABLE IF NOT EXISTS t (id REAL AUTOINCREMENT)",
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, n INTEGER AUTOINCREMENT)",
            "SELECT round(n, 2, 3) FROM notes",
            "CREATE TABLE IF NOT EXISTS t (n INTEGER DEFAULT 'x')",
            "CREATE TABLE IF NOT EXISTS t (flag BOOLEAN DEFAULT 1)",
            "CREATE TABLE IF NOT EXISTS t (n INTEGER DEFAULT (1))",
            "CREATE TABLE IF NOT EXISTS t (n INTEGER DEFAULT CAST('x' AS INTEGER))",
            "CREATE TABLE IF NOT EXISTS _bc_src (id INTEGER PRIMARY KEY)",
        ] {
            let err = binding_check(sql, DbResultSelection::Discard, 0).unwrap_err();
            assert!(
                err.to_string().contains("SQL v1")
                    || err.to_string().contains("not portable")
                    || err.to_string().contains("disallowed")
                    || err.to_string().contains("not allowed")
                    || err.to_string().contains("column list")
                    || err.to_string().contains("reserved")
                    || err.to_string().contains("unknown column")
                    || err.to_string().contains("round()")
                    || err.to_string().contains("arity"),
                "{sql}: {err}"
            );
        }
    }

    #[test]
    fn binding_owned_rejects_helper_arity_and_check_overflow() {
        for sql in [
            "SELECT abs() FROM notes",
            "SELECT ifnull(id) FROM notes",
            "SELECT json_object('k') FROM notes",
            "SELECT json_object('a', 1, 'b') FROM notes",
        ] {
            let err = binding_check(sql, DbResultSelection::Rows, 1).unwrap_err();
            assert!(err.to_string().contains("arity"), "{sql}: {err}");
        }
        let err = binding_check(
            "CREATE TABLE IF NOT EXISTS t (n INTEGER CHECK (n + 1 > n))",
            DbResultSelection::Discard,
            0,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("CHECK") || err.to_string().contains("overflow"),
            "{err}"
        );
    }

    #[test]
    fn binding_owned_denies_reserved_qualified_and_unbounded_ddl() {
        // Reserved host bookkeeping stays denied for both DML and DDL.
        for sql in [
            "SELECT * FROM db_atomic_receipts",
            "DELETE FROM schema_migrations",
            "SELECT * FROM plugin_databases",
            "DROP TABLE IF EXISTS db_atomic_receipts",
            "CREATE TABLE IF NOT EXISTS schema_migrations (v INTEGER)",
            "CREATE INDEX IF NOT EXISTS i ON db_atomic_receipts(expires_at)",
        ] {
            let err = binding_check(sql, DbResultSelection::Discard, 0).unwrap_err();
            assert!(
                err.to_string().contains("reserved")
                    || err.to_string().contains("unauthorized")
                    || err.to_string().contains("unknown"),
                "{sql}: {err}"
            );
        }
        // Schema-qualified names could reach another catalog (host library).
        for sql in [
            "SELECT * FROM public.books",
            "CREATE TABLE IF NOT EXISTS other_schema.t (id INTEGER)",
        ] {
            let err = binding_check(sql, DbResultSelection::Rows, 0).unwrap_err();
            assert!(
                err.to_string().contains("reserved")
                    || err.to_string().contains("qualified")
                    || err.to_string().contains("unauthorized"),
                "{sql}: {err}"
            );
        }
        // Only TABLE / INDEX DDL forms; session/admin verbs stay denied.
        for sql in [
            "CREATE VIEW v AS SELECT 1",
            "CREATE TRIGGER trg AFTER INSERT ON notes BEGIN SELECT 1; END",
            "DROP VIEW v",
            "ATTACH DATABASE 'x' AS y",
            "PRAGMA user_version",
            "VACUUM",
        ] {
            assert!(
                binding_check(sql, DbResultSelection::Discard, 0).is_err(),
                "{sql} must be denied"
            );
        }
        // Catalog identifiers stay denied.
        let err =
            binding_check("SELECT * FROM sqlite_master", DbResultSelection::Rows, 0).unwrap_err();
        assert!(err.to_string().contains("reserved") || err.to_string().contains("unauthorized"));
        // CTAS / FK-to-host / non-idempotent DDL are fail-closed (P1 binding isolation).
        for sql in [
            "CREATE TABLE IF NOT EXISTS leak AS SELECT * FROM public.books",
            "CREATE TABLE leak AS SELECT * FROM public.books",
            "CREATE TABLE IF NOT EXISTS t (id INTEGER REFERENCES public.books(id))",
            "CREATE TABLE t (id INTEGER)",
            "ALTER TABLE notes RENAME TO memos",
            "DROP TABLE memos",
        ] {
            let err = binding_check(sql, DbResultSelection::Discard, 0).unwrap_err();
            assert!(
                err.to_string().contains("not allowed")
                    || err.to_string().contains("qualified")
                    || err.to_string().contains("requires"),
                "{sql}: {err}"
            );
        }
    }

    #[test]
    fn binding_owned_denies_forbidden_functions_in_ddl_expressions() {
        for sql in [
            "CREATE TABLE IF NOT EXISTS t (v text DEFAULT pg_read_file('/path'))",
            "CREATE TABLE IF NOT EXISTS t (v text CHECK (pg_read_file('/x') = ''))",
            "CREATE INDEX IF NOT EXISTS i ON t (v) WHERE pg_read_file('/x') IS NOT NULL",
        ] {
            let err = binding_check(sql, DbResultSelection::Discard, 0).unwrap_err();
            assert!(
                err.to_string().contains("unauthorized function")
                    || err.to_string().contains("not portable"),
                "{sql}: {err}"
            );
        }
        binding_check(
            "CREATE TABLE IF NOT EXISTS t (v text CHECK (length(v) > 0))",
            DbResultSelection::Discard,
            0,
        )
        .unwrap();
    }

    #[test]
    fn binding_owned_denies_reserved_references_targets() {
        for sql in [
            "CREATE TABLE IF NOT EXISTS t (id INTEGER REFERENCES db_atomic_receipts(operation_id))",
            "CREATE TABLE IF NOT EXISTS t (id INTEGER, FOREIGN KEY (id) REFERENCES schema_migrations(v))",
            "CREATE TABLE IF NOT EXISTS t (id INTEGER REFERENCES plugin_databases(id))",
        ] {
            let err = binding_check(sql, DbResultSelection::Discard, 0).unwrap_err();
            assert!(
                err.to_string().contains("reserved")
                    || err.to_string().contains("qualified")
                    || err.to_string().contains("REFERENCES"),
                "{sql}: {err}"
            );
        }
        binding_check(
            "CREATE TABLE IF NOT EXISTS keyed (id INTEGER PRIMARY KEY, other_id INTEGER REFERENCES peer(id))",
            DbResultSelection::Discard,
            0,
        )
        .unwrap();
        binding_check(
            "CREATE TABLE IF NOT EXISTS keyed2 (id INTEGER, other_id INTEGER, FOREIGN KEY (other_id) REFERENCES peer(id))",
            DbResultSelection::Discard,
            0,
        )
        .unwrap();
    }

    #[test]
    fn binding_owned_denies_sqlite_only_functions() {
        for sql in [
            "SELECT strftime('%Y', body) FROM notes",
            "SELECT typeof(body) FROM notes",
            "SELECT group_concat(body) FROM notes",
            "SELECT iif(id > 0, body, '') FROM notes",
            "SELECT instr(body, 'x') FROM notes",
            "SELECT quote(body) FROM notes",
            "SELECT total(id) FROM notes",
            "SELECT date('now') FROM notes",
            "SELECT datetime('now') FROM notes",
            "SELECT time('now') FROM notes",
            "SELECT json_array(body) FROM notes",
        ] {
            let err = binding_check(sql, DbResultSelection::Rows, 1).unwrap_err();
            assert!(
                err.to_string().contains("unauthorized function")
                    || err.to_string().contains("unknown helper"),
                "{sql}: {err}"
            );
        }
        binding_check(
            "SELECT ifnull(body, ''), json_extract(body, '$.k'), length(body) FROM notes",
            DbResultSelection::Rows,
            1,
        )
        .unwrap();
        binding_check(
            "SELECT count(*), sum(id), min(id), max(id), avg(id) FROM notes",
            DbResultSelection::Rows,
            1,
        )
        .unwrap();
        let err =
            binding_check("SELECT CAST('1' AS INTEGER)", DbResultSelection::Rows, 1).unwrap_err();
        assert!(
            err.to_string().contains("CAST") || err.to_string().contains("invalid"),
            "{err}"
        );
        let err = binding_check("SELECT IFNULL('x', 0)", DbResultSelection::Rows, 1).unwrap_err();
        assert!(
            err.to_string().contains("incompatible") || err.to_string().contains("invalid"),
            "{err}"
        );
        let err =
            binding_check("SELECT hex(body) FROM notes", DbResultSelection::Rows, 1).unwrap_err();
        assert!(
            err.to_string().contains("unauthorized function")
                || err.to_string().contains("unknown helper"),
            "hex is not portable: {err}"
        );
        let library = GuestSqlPolicy::allow_tables(["notes"]);
        let request = req(
            "SELECT strftime('%Y', body) FROM notes",
            vec![],
            DbResultSelection::Rows,
            1,
        );
        authorize_guest_sql_policy(&request, &library).expect("library guests keep sqlite helpers");
    }

    #[test]
    fn non_binding_policies_still_deny_ddl() {
        let policy = GuestSqlPolicy::allow_tables(["books"]);
        let request = req(
            "CREATE TABLE t (id INTEGER)",
            vec![],
            DbResultSelection::Discard,
            0,
        );
        let err = validate_guest_execute_request_for_policy(&request, &policy).unwrap_err();
        assert!(err.to_string().contains("disallowed"), "{err}");
        // The policy-free wrapper keeps the fixed grammar.
        assert!(validate_guest_execute_request(&request).is_err());
    }

    #[test]
    fn rejects_encrypted_secrets_and_sqlite_catalog() {
        let err = validate_guest_execute_request(&req(
            "SELECT * FROM encrypted_secrets",
            vec![],
            DbResultSelection::Rows,
            0,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("unauthorized"), "{err}");
        let err = validate_guest_execute_request(&req(
            "SELECT * FROM sqlite_master",
            vec![],
            DbResultSelection::Rows,
            0,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("unauthorized"), "{err}");
        let err = validate_guest_execute_request(&req(
            r#"SELECT * FROM "pg_catalog"."pg_class""#,
            vec![],
            DbResultSelection::Rows,
            0,
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("unauthorized") || err.to_string().contains("SQL v1"),
            "{err}"
        );
    }

    #[test]
    fn rejects_bind_count_mismatch() {
        let err = validate_guest_execute_request(&req(
            "SELECT ? FROM books WHERE id = ?",
            vec![DbValue::Int64(1)],
            DbResultSelection::Rows,
            1,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("placeholder"), "{err}");
    }

    #[test]
    fn allows_rows_on_plain_dml_for_d1_run_parity() {
        validate_guest_execute_request(&req(
            "INSERT INTO books (id) VALUES (?)",
            vec![DbValue::Int64(1)],
            DbResultSelection::Rows,
            1,
        ))
        .unwrap();
    }

    #[test]
    fn ignores_placeholders_inside_strings() {
        validate_guest_execute_request(&req(
            "SELECT '?' FROM books WHERE title = ?",
            vec![DbValue::Text("x".into())],
            DbResultSelection::Rows,
            1,
        ))
        .unwrap();
    }

    #[test]
    fn rejects_replace_upsert() {
        let err = validate_guest_execute_request(&req(
            "REPLACE INTO books (id) VALUES (?)",
            vec![DbValue::Int64(1)],
            DbResultSelection::AffectedRows,
            0,
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("disallowed") || err.to_string().contains("SQL v1"),
            "{err}"
        );
        let err = validate_guest_execute_request(&req(
            "INSERT OR REPLACE INTO books (id) VALUES (?)",
            vec![DbValue::Int64(1)],
            DbResultSelection::AffectedRows,
            0,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("SQL v1"), "{err}");
    }

    #[test]
    fn rejects_information_schema_and_nested_catalog() {
        let err = validate_guest_execute_request(&req(
            "SELECT * FROM information_schema.tables",
            vec![],
            DbResultSelection::Rows,
            0,
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("unauthorized")
                || err.to_string().contains("qualified")
                || err.to_string().contains("SQL v1"),
            "{err}"
        );
        let err = validate_guest_execute_request(&req(
            "SELECT * FROM (SELECT * FROM encrypted_secrets)",
            vec![],
            DbResultSelection::Rows,
            0,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("unauthorized"), "{err}");
    }

    #[test]
    fn rejects_second_statement_and_affected_rows_max_rows() {
        let err = validate_guest_execute_request(&req(
            "SELECT 1; DROP TABLE books",
            vec![],
            DbResultSelection::Rows,
            1,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("single SQL"), "{err}");
        let err = validate_guest_execute_request(&req(
            "INSERT INTO books (id) VALUES (?)",
            vec![DbValue::Int64(1)],
            DbResultSelection::AffectedRows,
            4,
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("affectedRows cannot set maxRows"),
            "{err}"
        );
    }

    #[test]
    fn cte_kind_follows_the_main_statement() {
        assert_eq!(
            guest_statement_kind("WITH seed AS (SELECT 1) SELECT * FROM seed"),
            DbPlanStatementKind::Select
        );
        assert_eq!(
            guest_statement_kind("WITH seed AS (SELECT 1) INSERT INTO t SELECT * FROM seed"),
            DbPlanStatementKind::Execute
        );
        assert_eq!(
            guest_statement_kind("WITH seed AS (SELECT 1) UPDATE t SET x = 1"),
            DbPlanStatementKind::Execute
        );
        assert_eq!(
            guest_statement_kind("WITH seed AS (SELECT 1) DELETE FROM t"),
            DbPlanStatementKind::Execute
        );
        assert_eq!(
            guest_statement_kind(
                "WITH seed AS (SELECT 1) INSERT INTO t SELECT * FROM seed RETURNING id"
            ),
            DbPlanStatementKind::Returning
        );
        assert_eq!(
            guest_statement_kind("WITH seed AS (SELECT 1) UPDATE t SET x = 1 RETURNING x"),
            DbPlanStatementKind::Returning
        );
        assert_eq!(
            guest_statement_kind("WITH seed AS (SELECT 1) DELETE FROM t RETURNING id"),
            DbPlanStatementKind::Returning
        );
        assert_eq!(
            guest_statement_kind(
                "WITH x AS (INSERT INTO t VALUES (1) RETURNING id) SELECT * FROM x"
            ),
            DbPlanStatementKind::Select
        );
        assert_eq!(
            guest_statement_kind(
                "WITH RECURSIVE t(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM t WHERE x < 3) INSERT INTO u SELECT x FROM t"
            ),
            DbPlanStatementKind::Execute
        );
    }

    #[test]
    fn returning_single_row_proof_is_shape_derived_and_fail_closed() {
        // Proven: exactly one INSERT VALUES tuple.
        assert!(returning_single_row_proven(
            "INSERT INTO t (a, b) VALUES (?, ?) RETURNING id"
        ));
        assert!(returning_single_row_proven(
            "INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING RETURNING id"
        ));
        // Nested scalar subqueries stay below top level.
        assert!(returning_single_row_proven(
            "INSERT INTO t (a) VALUES ((SELECT MAX(x) FROM u)) RETURNING id"
        ));
        // Never proven: mutations whose SQL text does not bound row count.
        assert!(!returning_single_row_proven(
            "UPDATE t SET a = 1 RETURNING id"
        ));
        assert!(!returning_single_row_proven(
            "UPDATE t SET a = 1 WHERE id = ? RETURNING id"
        ));
        assert!(!returning_single_row_proven("DELETE FROM t RETURNING id"));
        assert!(!returning_single_row_proven(
            "INSERT INTO t (a) SELECT x FROM u RETURNING id"
        ));
        assert!(!returning_single_row_proven(
            "INSERT INTO t VALUES (1),(2) RETURNING id"
        ));
        assert!(!returning_single_row_proven(
            "WITH seed AS (SELECT 1) INSERT INTO t SELECT * FROM seed RETURNING id"
        ));
        assert!(!returning_single_row_proven(
            "INSERT INTO t VALUES (1) RETURNING id; DELETE FROM t"
        ));
        // Leading CTE with a single-tuple VALUES main statement is proven.
        assert!(returning_single_row_proven(
            "WITH seed AS (SELECT 1) INSERT INTO t (a) VALUES (?) RETURNING id"
        ));
    }

    #[test]
    fn cte_dml_selection_matches_main_statement() {
        validate_guest_execute_request(&req(
            "WITH seed AS (SELECT 1) INSERT INTO books (id) SELECT * FROM seed",
            vec![],
            DbResultSelection::AffectedRows,
            0,
        ))
        .unwrap();
        validate_guest_execute_request(&req(
            "WITH seed AS (SELECT 1) INSERT INTO books (id) SELECT * FROM seed",
            vec![],
            DbResultSelection::Rows,
            1,
        ))
        .unwrap();
        validate_guest_execute_request(&req(
            "WITH seed AS (SELECT 1) INSERT INTO books (id) SELECT * FROM seed RETURNING id",
            vec![],
            DbResultSelection::Rows,
            1,
        ))
        .unwrap();
        let err = validate_guest_execute_request(&req(
            "WITH seed AS (SELECT 1) SELECT * FROM seed",
            vec![],
            DbResultSelection::AffectedRows,
            0,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("affectedRows"), "{err}");
    }

    #[test]
    fn policy_allowlist_rejects_unrelated_tables_and_functions() {
        let books = GuestSqlPolicy::allow_tables(["books"]);
        authorize_guest_sql_policy(
            &req("SELECT id FROM books", vec![], DbResultSelection::Rows, 1),
            &books,
        )
        .unwrap();
        let err = authorize_guest_sql_policy(
            &req("SELECT id FROM jobs", vec![], DbResultSelection::Rows, 1),
            &books,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unauthorized table"), "{err}");
        let err = authorize_guest_sql_policy(
            &req(
                "SELECT load_extension('x') FROM books",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &books,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unauthorized function"), "{err}");
        let restricted = GuestSqlPolicy::allow_tables(["books"]).restrict_columns("books", ["id"]);
        let err = authorize_guest_sql_policy(
            &req(
                "SELECT books.token FROM books",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &restricted,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unauthorized column"), "{err}");
        let err = authorize_guest_sql_policy(
            &req("SELECT id FROM books", vec![], DbResultSelection::Rows, 1),
            &GuestSqlPolicy::deny_all(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unauthorized table"), "{err}");
    }

    #[test]
    fn policy_fail_closed_on_joins_quotes_and_unqualified_columns() {
        let books = GuestSqlPolicy::allow_tables(["books"]);
        let err = authorize_guest_sql_policy(
            &req("SELECT * FROM [jobs]", vec![], DbResultSelection::Rows, 1),
            &books,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unauthorized table"), "{err}");
        let err = authorize_guest_sql_policy(
            &req(
                "SELECT secret FROM books, jobs",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &books,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("unauthorized table")
                || err.to_string().contains("cannot be resolved"),
            "{err}"
        );
        let err = authorize_guest_sql_policy(
            &req(
                "SELECT secret FROM books JOIN jobs USING (id)",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &books,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unauthorized table"), "{err}");
        authorize_guest_sql_policy(
            &req(
                "WITH recent AS (SELECT id FROM books) SELECT id FROM recent",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &books,
        )
        .unwrap();
        let restricted = GuestSqlPolicy::allow_tables(["books"]).restrict_columns("books", ["id"]);
        let err = authorize_guest_sql_policy(
            &req(
                "SELECT token FROM books",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &restricted,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unauthorized column"), "{err}");
        authorize_guest_sql_policy(
            &req(
                "SELECT \"hex\"(id) FROM books",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &books,
        )
        .unwrap();
        let restricted = GuestSqlPolicy::allow_tables(["books"]).restrict_columns("books", ["id"]);
        let err = authorize_guest_sql_policy(
            &req("SELECT * FROM books", vec![], DbResultSelection::Rows, 1),
            &restricted,
        )
        .unwrap_err();
        assert!(err.to_string().contains("SELECT * is not allowed"), "{err}");
        let err = authorize_guest_sql_policy(
            &req(
                "SELECT books.* FROM books",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &restricted,
        )
        .unwrap_err();
        assert!(err.to_string().contains("SELECT * is not allowed"), "{err}");
        authorize_guest_sql_policy(
            &req(
                "SELECT id AS label FROM books",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &restricted,
        )
        .unwrap();
        let err = parse_guest_sql_refs("SELECT id FROM generate_series(1, 2)").unwrap_err();
        assert!(err.to_string().contains("not fully resolvable"), "{err}");
        authorize_guest_sql_policy(
            &req("SELECT id FROM jobs", vec![], DbResultSelection::Rows, 1),
            &GuestSqlPolicy::host_authoritative(),
        )
        .unwrap();
    }

    #[test]
    fn proof_auth_rejects_star_under_column_restriction() {
        let mut env = SqlTypeEnv::new();
        apply_schema_sql_to_env(
            &mut env,
            "CREATE TABLE IF NOT EXISTS books (id INTEGER, token TEXT)",
        );
        let restricted = GuestSqlPolicy::allow_tables(["books"])
            .restrict_columns("books", ["id"])
            .with_sql_types(env);
        for sql in [
            "SELECT * FROM books",
            "SELECT books.* FROM books",
            "SELECT b.* FROM books b",
            "SELECT b.* FROM books AS b",
        ] {
            let err = authorize_guest_sql_policy(
                &req(sql, vec![], DbResultSelection::Rows, 1),
                &restricted,
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("SELECT * is not allowed"),
                "{sql}: {err}"
            );
        }
        authorize_guest_sql_policy(
            &req("SELECT id FROM books", vec![], DbResultSelection::Rows, 1),
            &restricted,
        )
        .unwrap();
        let err = authorize_guest_sql_policy(
            &req(
                "SELECT token FROM books",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &restricted,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unauthorized column"), "{err}");
    }

    #[test]
    fn policy_allows_from_aliases_and_as_aliases() {
        let books = GuestSqlPolicy::allow_tables(["books"]);
        authorize_guest_sql_policy(
            &req(
                "SELECT b.id FROM books b",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &books,
        )
        .unwrap();
        authorize_guest_sql_policy(
            &req(
                "SELECT b.id FROM books AS b",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &books,
        )
        .unwrap();
    }

    #[test]
    fn policy_allows_cte_qualified_columns() {
        let books = GuestSqlPolicy::allow_tables(["books"]);
        authorize_guest_sql_policy(
            &req(
                "WITH recent AS (SELECT id FROM books) SELECT recent.id FROM recent",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &books,
        )
        .unwrap();
    }

    #[test]
    fn policy_allows_recursive_cte_self_scope() {
        let books = GuestSqlPolicy::allow_tables(["books"]);
        authorize_guest_sql_policy(
            &req(
                "WITH RECURSIVE t(x) AS (\
                     SELECT id FROM books \
                     UNION ALL \
                     SELECT t.x FROM t WHERE t.x < 3\
                 ) SELECT t.x FROM t",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &books,
        )
        .unwrap();
    }

    #[test]
    fn policy_allows_parenthesized_column_expressions() {
        let books = GuestSqlPolicy::allow_tables(["books"]);
        authorize_guest_sql_policy(
            &req(
                "SELECT (id + 1) FROM books",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &books,
        )
        .unwrap();
    }

    #[test]
    fn proof_auth_ignores_comment_and_cte_shadow() {
        let mut env = SqlTypeEnv::new();
        apply_schema_sql_to_env(
            &mut env,
            "CREATE TABLE IF NOT EXISTS allowed (id INTEGER, body TEXT)",
        );
        apply_schema_sql_to_env(&mut env, "CREATE TABLE IF NOT EXISTS secret (id INTEGER)");
        let policy = GuestSqlPolicy::allow_tables(["allowed"]).with_sql_types(env);
        authorize_guest_sql_policy(
            &req(
                "SELECT id FROM allowed -- FROM secret\n",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &policy,
        )
        .unwrap();
        authorize_guest_sql_policy(
            &req(
                "WITH secret AS (SELECT id FROM allowed) SELECT * FROM secret",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &policy,
        )
        .unwrap();
        let err = authorize_guest_sql_policy(
            &req("SELECT id FROM secret", vec![], DbResultSelection::Rows, 1),
            &policy,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unauthorized"), "{err}");
        let err = authorize_guest_sql_policy(
            &req(
                "SELECT id FROM allowed WHERE EXISTS (SELECT 1 FROM secret)",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &policy,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unauthorized"), "{err}");
    }

    #[test]
    fn proof_auth_records_functions() {
        let mut env = SqlTypeEnv::new();
        apply_schema_sql_to_env(
            &mut env,
            "CREATE TABLE IF NOT EXISTS notes (id INTEGER, body TEXT)",
        );
        let policy = GuestSqlPolicy::allow_tables(["notes"]).with_sql_types(env);
        authorize_guest_sql_policy(
            &req(
                "SELECT length(body), ifnull(body, '') FROM notes",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &policy,
        )
        .unwrap();
        let err = authorize_guest_sql_policy(
            &req(
                "SELECT hex(body) FROM notes",
                vec![],
                DbResultSelection::Rows,
                1,
            ),
            &policy,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("unknown helper")
                || err.to_string().contains("unauthorized function"),
            "{err}"
        );
    }
}
