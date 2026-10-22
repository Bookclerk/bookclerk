//! Shared oracle for the `guest_sql_authz` fuzz target and its corpus replay
//! test (`crates/bookclerk-library/tests/fuzz_corpus.rs`): the guest SQL
//! authorizer in `bookclerk-plugin-abi` is checked against SQLite's own
//! authorizer callback, an independent view of which tables, columns and
//! functions a statement really touches (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! Both harnesses include this file with `#[path]`, so the policy set, the
//! schema and the invariant stay identical and cannot drift.
//!
//! Input layout: the first byte selects the policy and bind type; the rest is
//! one canonical SQL statement. A statement the policy admits is lowered the
//! way the SQLite adapter lowers it, prepared on an in-memory database holding
//! the schema below, and every access SQLite reports while compiling it must
//! be inside the policy.

use std::cell::RefCell;
use std::sync::{Arc, Mutex, OnceLock};

use bookclerk_db_exec::{lower_canonical_sql, lower_canonical_sql_typed};
use bookclerk_plugin_abi::{
    authorize_guest_sql_policy, guest_statement_kind, sql_type_env_from_canonical_statements,
    typecheck_execute_request_proofs, validate_guest_execute_request_for_policy,
    DbPlanStatementKind, DbResultSelection, DbValue, ExecuteRequest, GuestSqlPolicy, SqlTypeEnv,
    TypedDbStatement, BOOKCLERK_RESERVED_PREFIX,
};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::Connection;
use sea_orm::DatabaseBackend;

/// Largest statement worth checking; the wire caps guest SQL far below this.
pub const MAX_INPUT_BYTES: usize = 4096;

/// Tables every policy variant grants.
pub const GRANTED_TABLES: &[&str] = &["books", "authors"];

/// Columns the column-restricted variant leaves visible on `books`.
pub const BOOKS_VISIBLE_COLUMNS: &[&str] = &["id", "title"];

/// Schema the oracle database holds: the granted tables, a host table no
/// policy grants, a denied-by-name table, and one host bookkeeping table so a
/// plugin-owned binding statement that names it compiles far enough for
/// SQLite to report the access.
pub const SCHEMA: &[&str] = &[
    "CREATE TABLE books (id INTEGER PRIMARY KEY NOT NULL, title TEXT NOT NULL, note TEXT, \
     author_id INTEGER, price REAL, cover BLOB)",
    "CREATE TABLE authors (id INTEGER PRIMARY KEY NOT NULL, name TEXT NOT NULL, born INTEGER)",
    "CREATE TABLE host_secrets (id INTEGER PRIMARY KEY NOT NULL, token TEXT NOT NULL)",
    "CREATE TABLE encrypted_secrets (id INTEGER PRIMARY KEY NOT NULL, blob BLOB)",
    "CREATE TABLE bookclerk_receipts (operation_id TEXT PRIMARY KEY NOT NULL, \
     operation_kind TEXT NOT NULL)",
];

/// Physical tables in [`SCHEMA`], lower case, as SQLite reports them.
pub const PHYSICAL_TABLES: &[&str] = &[
    "books",
    "authors",
    "host_secrets",
    "encrypted_secrets",
    "bookclerk_receipts",
];

/// Functions the SQLite lowering introduces that no guest wrote: `LIKE` is
/// rewritten to `GLOB`, and SQLite compiles both operators as function calls.
pub const LOWERING_FUNCTIONS: &[&str] = &["like", "glob"];

/// Which host-issued policy a run exercises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// `allow_tables` with no column types: the lexical reference parser.
    Lexical,
    /// `allow_tables` plus a column restriction on `books`.
    ColumnRestricted,
    /// `allow_tables` with the catalog loaded: the typed-proof path.
    Typed,
    /// A plugin-owned binding: any non-reserved table, no durable DDL.
    BindingOwned,
    /// A plugin-owned binding inside a migration: bounded `CREATE`/`DROP`.
    BindingMigration,
}

impl Variant {
    /// Every variant, in selector order.
    pub const ALL: [Self; 5] = [
        Self::Lexical,
        Self::ColumnRestricted,
        Self::Typed,
        Self::BindingOwned,
        Self::BindingMigration,
    ];

    /// Decodes the selector byte: digits `'0'..='4'` keep seeds readable,
    /// anything else wraps into the same five variants.
    #[must_use]
    pub fn from_selector(byte: u8) -> Self {
        let index = usize::from(byte.wrapping_sub(b'0')) % Self::ALL.len();
        Self::ALL[index]
    }

    /// True for the plugin-owned binding variants.
    #[must_use]
    pub fn binding(self) -> bool {
        matches!(self, Self::BindingOwned | Self::BindingMigration)
    }

    /// True for the variants whose policy carries the catalog: the host
    /// executes their statements through the typed adapter path, whose
    /// lowering is directed by the type proof.
    #[must_use]
    pub fn typed(self) -> bool {
        matches!(
            self,
            Self::Typed | Self::BindingOwned | Self::BindingMigration
        )
    }

    /// The policy this variant hands to the authorizer, built once per
    /// process: parsing the catalog on every input would dominate the fuzzer's
    /// time without exercising anything new.
    #[must_use]
    pub fn policy(self) -> &'static GuestSqlPolicy {
        static POLICIES: OnceLock<Vec<GuestSqlPolicy>> = OnceLock::new();
        let all = POLICIES.get_or_init(|| Self::ALL.iter().map(|v| v.build_policy()).collect());
        let index = Self::ALL
            .iter()
            .position(|v| *v == self)
            .unwrap_or_default();
        &all[index]
    }

    /// Builds this variant's policy from [`SCHEMA`].
    fn build_policy(self) -> GuestSqlPolicy {
        let env = typed_env().clone();
        match self {
            Self::Lexical => GuestSqlPolicy::allow_tables(GRANTED_TABLES),
            Self::ColumnRestricted => GuestSqlPolicy::allow_tables(GRANTED_TABLES)
                .restrict_columns("books", BOOKS_VISIBLE_COLUMNS),
            Self::Typed => GuestSqlPolicy::allow_tables(GRANTED_TABLES).with_sql_types(env),
            Self::BindingOwned => GuestSqlPolicy::binding_owned().with_sql_types(env),
            Self::BindingMigration => GuestSqlPolicy::binding_migration().with_sql_types(env),
        }
    }
}

/// The catalog the typed variants carry, parsed from [`SCHEMA`] once.
fn typed_env() -> &'static SqlTypeEnv {
    static ENV: OnceLock<SqlTypeEnv> = OnceLock::new();
    ENV.get_or_init(|| sql_type_env_from_canonical_statements(SCHEMA))
}

/// Lowers `sql` for SQLite the way the host lowers it under `variant`.
///
/// A typed variant executes through the typed adapter, which lowers a non-DDL
/// statement with its type proof (`lower_canonical_sql_typed`), so the
/// proof's rewrite sites (integer overflow wraps) reach the engine. The
/// lexical variants and DDL go through the mechanical lowering
/// (`lower_canonical_sql`), as `execute_physical_sql` does. A statement whose
/// proof cannot be built here falls back to mechanical lowering: the host
/// already typechecked it when it admitted it.
///
/// # Errors
///
/// The lowering's own rejection of a proof it should accept, as text.
pub fn lowered_for(variant: Variant, sql: &str, text_binds: bool) -> Result<String, String> {
    if variant.typed() && !is_ddl(sql) {
        if let Ok(proofs) =
            typecheck_execute_request_proofs(&request_for(sql, text_binds), typed_env())
        {
            // The adapter lowers the trimmed statement, which the proof is
            // bound to.
            return lower_canonical_sql_typed(DatabaseBackend::Sqlite, sql.trim(), proofs.first())
                .map_err(|err| err.to_string());
        }
    }
    Ok(lower_canonical_sql(DatabaseBackend::Sqlite, sql))
}

/// One access SQLite's authorizer reported while compiling a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// A read, insert, update or delete on `table`; `column` is set for reads
    /// and updates of a named column.
    Table {
        /// Lower-cased table name as SQLite reports it.
        table: String,
        /// Lower-cased column name, `None` for whole-table or column-less access.
        column: Option<String>,
    },
    /// A read of a table SQLite does not have: the lower-cased name from its
    /// `no such table` error. A CTE or subquery alias always resolves, so
    /// this is a real table reference the oracle schema does not model.
    MissingTable(String),
    /// A function call by lower-cased name.
    Function(String),
    /// SQLite's tokenizer or parser rejected a statement the host admitted;
    /// carries SQLite's message. The host's grammar and lowering promise SQL
    /// every engine parses, so this is a finding against one of them.
    Unprepared(String),
    /// Schema DDL on `table` (`CREATE`/`DROP` `TABLE`/`INDEX`); `verb` is the
    /// authorizer action name.
    Schema {
        /// The authorizer action, e.g. `CreateTable`.
        verb: &'static str,
        /// Lower-cased table name the action targets.
        table: String,
    },
    /// Anything a guest statement must never cause: transactions, pragmas,
    /// attach, views, triggers, virtual tables, `ALTER`, `ANALYZE`, `REINDEX`.
    Forbidden(&'static str),
}

/// Builds the one-statement request the host would see for `sql`.
#[must_use]
pub fn request_for(sql: &str, text_binds: bool) -> ExecuteRequest {
    let kind = guest_statement_kind(sql);
    let binds = placeholder_count(sql);
    let parameters = (0..binds)
        .map(|i| {
            if text_binds {
                DbValue::Text(format!("v{i}"))
            } else {
                DbValue::Int64(i64::try_from(i).unwrap_or(0) + 1)
            }
        })
        .collect();
    ExecuteRequest {
        operation_id: "fuzz-op".into(),
        request_hash: String::new(),
        statements: vec![TypedDbStatement {
            sql: sql.to_string(),
            parameters,
            kind,
            max_rows: 0,
            result_selection: match kind {
                DbPlanStatementKind::Select | DbPlanStatementKind::Returning => {
                    DbResultSelection::Rows
                }
                DbPlanStatementKind::Execute => DbResultSelection::AffectedRows,
            },
        }],
        deadline_unix_ms: 0,
    }
}

/// Counts `?` placeholders outside quotes and comments.
#[must_use]
pub fn placeholder_count(sql: &str) -> usize {
    let bytes = sql.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            quote @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            b'?' => count += 1,
            _ => {}
        }
        i += 1;
    }
    count
}

/// Runs the host pipeline for `sql` under `variant`: grammar and policy
/// validation, then authorization. `Ok` means the host would execute it.
///
/// # Errors
///
/// The host's own rejection, verbatim.
pub fn host_admits(
    variant: Variant,
    sql: &str,
    text_binds: bool,
) -> Result<(), bookclerk_plugin_abi::PluginError> {
    let policy = variant.policy();
    let req = request_for(sql, text_binds);
    validate_guest_execute_request_for_policy(&req, policy)?;
    authorize_guest_sql_policy(&req, policy)
}

/// Opens the in-memory oracle database and creates [`SCHEMA`] on it.
fn oracle_connection() -> rusqlite::Result<Connection> {
    let conn = Connection::open_in_memory()?;
    for ddl in SCHEMA {
        conn.execute_batch(ddl)?;
    }
    Ok(conn)
}

/// One oracle database per thread with the authorizer installed once; every
/// access it reports lands in `seen`, which [`sqlite_accesses`] clears per
/// statement. Statements are only prepared, never run, so the schema stays
/// as [`SCHEMA`] created it.
struct Oracle {
    /// The connection carrying [`SCHEMA`] and the authorizer callback.
    conn: Connection,
    /// Accesses reported while compiling the current statement.
    seen: Arc<Mutex<Vec<Access>>>,
}

impl Oracle {
    /// Decides whether `table`, which SQLite reported as `no such table`
    /// while compiling `lowered`, stands in a table position.
    ///
    /// SQLite gives the same message for a dangling qualifier (`SELECT x.*
    /// FROM books`) or alias and for a real `FROM x`, so the oracle asks
    /// SQLite: it creates a scratch table of that name inside a savepoint,
    /// compiles again and rolls back. A statement that still fails with the
    /// same message never referenced the table; any other outcome (it
    /// compiles, or fails later on a column of the scratch table) means the
    /// name was consumed as a table. A scratch table that cannot be created
    /// counts as a reference, so the oracle fails closed.
    fn is_table_reference(&self, lowered: &str, table: &str) -> bool {
        let quoted = format!("\"{}\"", table.replace('"', "\"\""));
        if self.conn.execute_batch("SAVEPOINT probe").is_err() {
            return true;
        }
        let same_error = format!("no such table: {table}");
        let verdict = match self
            .conn
            .execute_batch(&format!("CREATE TABLE {quoted} (probe INTEGER)"))
        {
            Err(_) => true,
            Ok(()) => match self.conn.prepare(lowered) {
                Ok(_) => true,
                // SQLite echoes the name as the statement spelled it.
                Err(err) => strip_echo(&err, lowered).trim().to_ascii_lowercase() != same_error,
            },
        };
        let _ = self.conn.execute_batch("ROLLBACK TO probe; RELEASE probe");
        verdict
    }

    /// Opens the database and installs the recording authorizer.
    fn new() -> rusqlite::Result<Self> {
        let conn = oracle_connection()?;
        let seen: Arc<Mutex<Vec<Access>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            if let Some(access) = classify(&ctx.action) {
                if let Ok(mut seen) = sink.lock() {
                    seen.push(access);
                }
            }
            Authorization::Allow
        }))?;
        Ok(Self { conn, seen })
    }
}

/// Returns `err`'s message without the ` in <sql> at offset N` echo rusqlite
/// appends to a prepare error, so words inside the statement (a fuzzer
/// happily writes "syntax error" into a comment) cannot steer classification.
fn strip_echo(err: &rusqlite::Error, lowered: &str) -> String {
    let message = err.to_string();
    let echo = format!(" in {lowered}");
    message
        .split_once(echo.as_str())
        .map_or(message.clone(), |(head, _)| head.to_string())
}

thread_local! {
    /// The thread's oracle, created on first use.
    static ORACLE: RefCell<Option<Oracle>> = const { RefCell::new(None) };
}

/// [`sqlite_accesses`] with the lowering the host applies under `variant`
/// ([`lowered_for`]); a lowering that refuses an admitted statement is an
/// [`Access::Unprepared`] finding.
#[must_use]
pub fn sqlite_accesses_for(variant: Variant, sql: &str, text_binds: bool) -> Vec<Access> {
    match lowered_for(variant, sql, text_binds) {
        Ok(lowered) => accesses_of(sql, &lowered),
        Err(message) => vec![Access::Unprepared(format!(
            "typed lowering failed: {message}"
        ))],
    }
}

/// Compiles `sql`, mechanically lowered for SQLite, on the oracle database
/// and returns every access SQLite's authorizer reported, in order.
///
/// A statement SQLite cannot prepare yields the accesses reported before the
/// error plus one more: a `no such table` error becomes an
/// [`Access::MissingTable`] on that name and `has no column named` a column access,
/// since SQLite stops before authorizing a name it does not know and the
/// lexical policies carry no catalog to know it either; a token-level
/// rejection (syntax error, unrecognized token, incomplete input) of a DML
/// statement becomes [`Access::Unprepared`]; DDL syntax and semantic errors
/// add nothing, since binding DDL is authorized lexically and the adapter
/// edge owns its shape.
///
/// When the oracle database itself cannot be created, the result is a single
/// [`Access::Forbidden`] so the caller fails closed instead of treating a
/// broken oracle as "touched nothing".
#[must_use]
pub fn sqlite_accesses(sql: &str) -> Vec<Access> {
    accesses_of(sql, &lower_canonical_sql(DatabaseBackend::Sqlite, sql))
}

/// Prepares `lowered`, the SQLite form of the canonical `sql`, on the oracle
/// database and classifies what the authorizer and the prepare error report.
fn accesses_of(sql: &str, lowered: &str) -> Vec<Access> {
    ORACLE.with_borrow_mut(|slot| {
        if slot.is_none() {
            match Oracle::new() {
                Ok(oracle) => *slot = Some(oracle),
                Err(_) => return vec![Access::Forbidden("oracle database unavailable")],
            }
        }
        let Some(oracle) = slot.as_ref() else {
            return vec![Access::Forbidden("oracle database unavailable")];
        };
        if let Ok(mut seen) = oracle.seen.lock() {
            seen.clear();
        }
        let prepare_error = oracle
            .conn
            .prepare(lowered)
            .err()
            .map(|err| strip_echo(&err, lowered));
        let mut out = oracle
            .seen
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default();
        if let Some(message) = prepare_error {
            if is_ddl(sql) {
                // Binding DDL is authorized lexically (names, functions,
                // idempotency) and typed at the adapter edge; its column
                // list has no positive grammar yet, so the engine's own
                // syntax and semantic rejections (a trailing comma, two
                // primary keys) are not findings against the authorizer.
            } else if let Some(table) = message.strip_prefix("no such table: ") {
                let table = table.trim().to_ascii_lowercase();
                // `SELECT x.* FROM books` with no source `x` also fails with
                // "no such table: x"; a name that is only a dangling qualifier
                // or alias was never a table reference and reads nothing.
                if oracle.is_table_reference(lowered, &table) {
                    out.push(Access::MissingTable(table));
                }
            } else if let Some(rest) = message.strip_prefix("table ") {
                // "table T has no column named C": a name the lexical policies
                // cannot know and SQLite stops on before authorizing it.
                if let Some((table, column)) = rest.split_once(" has no column named ") {
                    out.push(Access::Table {
                        table: table.trim().to_ascii_lowercase(),
                        column: Some(column.trim().to_ascii_lowercase()),
                    });
                } else if let Some((table, _)) = rest
                    .split_once(" has ")
                    .filter(|(_, tail)| tail.contains(" values were supplied"))
                {
                    // "table T has N columns but M values were supplied": an
                    // INSERT without a column list whose arity the lexical
                    // policies cannot check; the typed catalog refuses it.
                    out.push(Access::Table {
                        table: table.trim().to_ascii_lowercase(),
                        column: None,
                    });
                } else {
                    out.push(Access::Unprepared(message));
                }
            } else if message.contains("syntax error")
                || message.contains("unrecognized token")
                || message.contains("incomplete input")
            {
                // A token-level rejection is a defect in the grammar or the
                // lowering: both promise SQL every engine parses. Semantic
                // errors (unknown columns, arity, aggregate misuse) are facts
                // about a schema the lexical policies do not model.
                out.push(Access::Unprepared(message));
            }
        }
        out
    })
}

/// True when the statement's first word is a DDL verb.
fn is_ddl(sql: &str) -> bool {
    // The grammar skips leading trivia before the verb; so must this.
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, tail)| tail);
        } else {
            break;
        }
    }
    rest.split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .is_some_and(|verb| {
            ["create", "drop", "alter"]
                .iter()
                .any(|ddl| verb.eq_ignore_ascii_case(ddl))
        })
}

/// Maps one authorizer action onto the oracle's vocabulary; `Select` and
/// `Recursive` carry no object and are dropped.
fn classify(action: &AuthAction<'_>) -> Option<Access> {
    let lower = |s: &str| s.to_ascii_lowercase();
    Some(match action {
        AuthAction::Read {
            table_name,
            column_name,
        } => Access::Table {
            table: lower(table_name),
            column: (!column_name.is_empty()).then(|| lower(column_name)),
        },
        AuthAction::Update {
            table_name,
            column_name,
        } => Access::Table {
            table: lower(table_name),
            column: (!column_name.is_empty()).then(|| lower(column_name)),
        },
        AuthAction::Insert { table_name } | AuthAction::Delete { table_name } => Access::Table {
            table: lower(table_name),
            column: None,
        },
        AuthAction::Function { function_name } => Access::Function(lower(function_name)),
        AuthAction::CreateTable { table_name } => Access::Schema {
            verb: "CreateTable",
            table: lower(table_name),
        },
        AuthAction::DropTable { table_name } => Access::Schema {
            verb: "DropTable",
            table: lower(table_name),
        },
        AuthAction::CreateIndex { table_name, .. } => Access::Schema {
            verb: "CreateIndex",
            table: lower(table_name),
        },
        AuthAction::DropIndex { table_name, .. } => Access::Schema {
            verb: "DropIndex",
            table: lower(table_name),
        },
        AuthAction::Select | AuthAction::Recursive => return None,
        AuthAction::Unknown { .. } => Access::Forbidden("Unknown"),
        AuthAction::CreateTempIndex { .. } => Access::Forbidden("CreateTempIndex"),
        AuthAction::CreateTempTable { .. } => Access::Forbidden("CreateTempTable"),
        AuthAction::CreateTempTrigger { .. } => Access::Forbidden("CreateTempTrigger"),
        AuthAction::CreateTempView { .. } => Access::Forbidden("CreateTempView"),
        AuthAction::CreateTrigger { .. } => Access::Forbidden("CreateTrigger"),
        AuthAction::CreateView { .. } => Access::Forbidden("CreateView"),
        AuthAction::DropTempIndex { .. } => Access::Forbidden("DropTempIndex"),
        AuthAction::DropTempTable { .. } => Access::Forbidden("DropTempTable"),
        AuthAction::DropTempTrigger { .. } => Access::Forbidden("DropTempTrigger"),
        AuthAction::DropTempView { .. } => Access::Forbidden("DropTempView"),
        AuthAction::DropTrigger { .. } => Access::Forbidden("DropTrigger"),
        AuthAction::DropView { .. } => Access::Forbidden("DropView"),
        AuthAction::Pragma { .. } => Access::Forbidden("Pragma"),
        AuthAction::Transaction { .. } => Access::Forbidden("Transaction"),
        AuthAction::Attach { .. } => Access::Forbidden("Attach"),
        AuthAction::Detach { .. } => Access::Forbidden("Detach"),
        AuthAction::AlterTable { .. } => Access::Forbidden("AlterTable"),
        // SQLite reports a reindex while it populates a freshly created
        // index, so this follows an admitted `CreateIndex` in a migration.
        AuthAction::Reindex { .. } => Access::Schema {
            verb: "Reindex",
            table: String::new(),
        },
        AuthAction::Analyze { .. } => Access::Forbidden("Analyze"),
        AuthAction::CreateVtable { .. } => Access::Forbidden("CreateVtable"),
        AuthAction::DropVtable { .. } => Access::Forbidden("DropVtable"),
        AuthAction::Savepoint { .. } => Access::Forbidden("Savepoint"),
        // `AuthAction` is `#[non_exhaustive]`; a variant added by a later
        // rusqlite is something no guest statement should trigger either.
        _ => Access::Forbidden("Unclassified"),
    })
}

/// The invariant: every access SQLite reports for a statement the host
/// admitted under `variant` is inside that variant's policy. Returns the
/// first violation as text, `None` when the statement is within policy.
///
/// True for a table name no plugin-owned binding may touch: the host's
/// `bookclerk_` bookkeeping prefix and the engine catalog. A binding is a
/// physically separate database, so host-library names such as
/// `encrypted_secrets` are not reserved there (the host rule says the same);
/// [`GRANTED_TABLES`] keeps them off the non-binding variants.
fn binding_reserved(table: &str) -> bool {
    table.starts_with(BOOKCLERK_RESERVED_PREFIX) || table.starts_with("sqlite_")
}

/// An engine catalog read (`sqlite_*`) is a violation unless the statement
/// is DDL, which SQLite serves from its own catalog. Other table names the
/// authorizer reports that are not in [`PHYSICAL_TABLES`] are CTE or subquery
/// aliases and carry no authority, so they are skipped. A name
/// SQLite could not resolve at all ([`Access::MissingTable`]) is no alias: a
/// non-binding variant that admitted it touched an ungranted table, and a
/// binding variant is held to the reserved-name rule as for a known table.
#[must_use]
pub fn violation(variant: Variant, accesses: &[Access]) -> Option<String> {
    let policy = variant.policy();
    for access in accesses {
        match access {
            Access::Table { table, column } => {
                if table.starts_with("sqlite_") {
                    // SQLite reads and writes its own catalog while it runs
                    // DDL; a statement without a schema action that reaches
                    // the catalog named it, which no grant allows.
                    if accesses.iter().any(|a| matches!(a, Access::Schema { .. })) {
                        continue;
                    }
                    return Some(format!("touched engine catalog {table}"));
                }
                if !PHYSICAL_TABLES.contains(&table.as_str()) {
                    continue;
                }
                if variant.binding() {
                    if binding_reserved(table) {
                        return Some(format!("binding statement touched reserved table {table}"));
                    }
                    continue;
                }
                if !GRANTED_TABLES.contains(&table.as_str()) {
                    return Some(format!("touched ungranted table {table}"));
                }
                if variant == Variant::ColumnRestricted && table == "books" {
                    if let Some(column) = column {
                        if !BOOKS_VISIBLE_COLUMNS.contains(&column.as_str()) {
                            return Some(format!("touched restricted column books.{column}"));
                        }
                    }
                }
            }
            Access::MissingTable(table) => {
                if variant.binding() {
                    if binding_reserved(table) {
                        return Some(format!("binding statement touched reserved table {table}"));
                    }
                    continue;
                }
                return Some(format!(
                    "touched ungranted table {table} (absent from the oracle schema)"
                ));
            }
            Access::Function(name) => {
                if LOWERING_FUNCTIONS.contains(&name.as_str()) {
                    continue;
                }
                if !policy.functions().any(|allowed| allowed == name) {
                    return Some(format!("called unauthorized function {name}"));
                }
            }
            Access::Schema { verb, table } => {
                if variant != Variant::BindingMigration {
                    return Some(format!("{verb} on {table} outside a migration context"));
                }
                if *verb == "Reindex" {
                    continue;
                }
                if binding_reserved(table) {
                    return Some(format!("{verb} on reserved table {table}"));
                }
            }
            Access::Unprepared(message) => {
                return Some(format!(
                    "sqlite cannot compile the admitted statement: {message}"
                ))
            }
            Access::Forbidden(what) => return Some(format!("forbidden action {what}")),
        }
    }
    None
}

/// Full check for one input: decodes the selector, runs the host pipeline,
/// and, when the host admits the statement, compares SQLite's view with the
/// policy. Panics on a violation so the fuzzer and the replay test fail.
///
/// Returns `true` when the host admitted the statement.
///
/// Splits one fuzz input into the policy variant, the statement and the
/// bind-type flag; `None` when the input is too long, empty or not UTF-8.
///
/// The selector byte picks the variant (see [`Variant::from_selector`]) and
/// its high bit asks for text binds. The replay test uses the same decoding
/// so its diagnostics name the variant the target exercised.
#[must_use]
pub fn decode(data: &[u8]) -> Option<(Variant, &str, bool)> {
    if data.len() > MAX_INPUT_BYTES {
        return None;
    }
    let (&selector, sql) = data.split_first()?;
    let sql = std::str::from_utf8(sql).ok()?;
    Some((Variant::from_selector(selector), sql, selector & 0x80 != 0))
}

/// # Panics
///
/// On a policy violation, with the statement and the first offending access.
pub fn check(data: &[u8]) -> bool {
    let Some((variant, sql, text_binds)) = decode(data) else {
        return false;
    };
    if host_admits(variant, sql, text_binds).is_err() {
        return false;
    }
    let accesses = sqlite_accesses_for(variant, sql, text_binds);
    if let Some(why) = violation(variant, &accesses) {
        panic!("{variant:?} admitted {sql:?}: {why}; sqlite saw {accesses:?}");
    }
    true
}
