//! Shared Bookclerk SQL v1 execution vectors (binding DDL + portable helpers).
//!
//! Hosts emit these canonical statements. Adapters lower at execute: every
//! backend rewrites `INSERT OR IGNORE` to unique/PK `ON CONFLICT DO NOTHING`;
//! Postgres also applies binding DDL type rewrites and helper lowering.
//! Every name in `GuestSqlPolicy::binding_owned` portable functions
//! (except denied `hex`) appears here.

use bookclerk_plugin_abi::{DbColumn, DbType, DbValue, StatementResult};

/// Binding `CREATE TABLE` covering identity + `BLOB`/`INTEGER`/`REAL`.
pub const BINDING_DDL_AUTOINCREMENT_BLOB: &str = "CREATE TABLE IF NOT EXISTS typed (\
     id INTEGER PRIMARY KEY AUTOINCREMENT, \
     n INTEGER, body TEXT, payload TEXT, blob BLOB, r REAL)";

/// Binding `CREATE TABLE` covering canonical `BOOLEAN` (not INTEGER-as-bool).
pub const BINDING_DDL_BOOLEAN: &str = "CREATE TABLE IF NOT EXISTS flags (\
     id INTEGER PRIMARY KEY AUTOINCREMENT, \
     flag BOOLEAN, \
     missing BOOLEAN)";

/// INSERT [`DbValue::Boolean`] + typed-null into [`BINDING_DDL_BOOLEAN`].
pub const PORTABLE_BOOLEAN_INSERT: &str = "INSERT INTO flags (flag, missing) VALUES (?, ?)";

/// SELECT declared boolean columns from [`BINDING_DDL_BOOLEAN`].
pub const PORTABLE_BOOLEAN_SELECT: &str = "SELECT flag, missing FROM flags";

/// Lowercase / mixed-case DDL admitted by SQL v1 (idents fold) and lowered
/// case-insensitively on Postgres.
pub const BINDING_DDL_LOWERCASE: &str = "create table if not exists typed_lc (\
     id integer primary key autoincrement, \
     blob blob, flag boolean)";

/// Lowercase `INSERT OR IGNORE … RETURNING` against [`BINDING_DDL_LOWERCASE`].
pub const PORTABLE_INSERT_OR_IGNORE_RETURNING_LC: &str =
    "insert or ignore into typed_lc (blob, flag) values (?, ?) returning id";

/// SELECT stored blob + boolean from [`BINDING_DDL_LOWERCASE`].
pub const PORTABLE_LOWERCASE_SELECT: &str = "SELECT blob, flag FROM typed_lc";

/// Seed row for [`PORTABLE_SELECT`]. Bind [`PORTABLE_INSERT_BLOB`] at `?`.
pub const PORTABLE_INSERT: &str =
    "INSERT INTO typed (n, body, payload, blob, r) VALUES (2, 'Ab', '{\"k\":\"v\"}', ?, 1.5)";

/// Blob payload for [`PORTABLE_INSERT`].
pub const PORTABLE_INSERT_BLOB: &[u8] = &[1, 2, 3];

/// Scalar portable helpers (no aggregates).
///
/// Column order matches [`portable_select_expects`]. `json_object` is
/// sampled through `json_extract` so the cell is a scalar TEXT value rather
/// than a JSON document (spacing is engine-specific). Postgres lowering also
/// casts `json_build_object` to TEXT so `length(json_object(…))` is legal.
/// Aggregates live in [`PORTABLE_AGGREGATE_SELECT`]: a mixed aggregate +
/// non-aggregate SELECT list is SQLite-shaped, not Bookclerk SQL v1
/// (PostgreSQL requires `GROUP BY`). `round`/`sum`/`avg` are `CAST` to
/// `INTEGER` so cells stay in the universal `DbValue` domain (Postgres
/// `NUMERIC` is not a wire type).
pub const PORTABLE_SELECT: &str = "SELECT \
     abs(-3) AS c0, \
     coalesce(NULL, 4) AS c1, \
     ifnull(NULL, 5) AS c2, \
     length(body) AS c3, \
     lower(body) AS c4, \
     upper(body) AS c5, \
     trim(' x ') AS c6, \
     substr(body, 1, 1) AS c7, \
     CAST(round(1.4) AS INTEGER) AS c8, \
     nullif(n, 9) AS c9, \
     min(1, 2) AS c10, \
     max(3, 1) AS c11, \
     json_extract(payload, '$.k') AS c12, \
     json_extract(json_object('k', 'v'), '$.k') AS c13, \
     json_valid(payload) AS c14, \
     json_valid(body) AS c15, \
     replace(body, 'A', 'Z') AS c16, \
     CAST(n AS INTEGER) AS c17 \
     FROM typed";

/// Aggregate-only companion to [`PORTABLE_SELECT`].
pub const PORTABLE_AGGREGATE_SELECT: &str = "SELECT \
     count(*) AS a0, \
     CAST(sum(n) AS INTEGER) AS a1, \
     min(n) AS a2, \
     max(n) AS a3, \
     CAST(avg(n) AS INTEGER) AS a4 \
     FROM typed";

/// Expected cells for [`PORTABLE_SELECT`] (adapter-neutral).
#[derive(Clone, Copy, Debug)]
pub enum PortableExpect {
    /// Exact [`DbValue::Int64`].
    Int(i64),
    /// Exact [`DbValue::Text`].
    Text(&'static str),
    /// Exact [`DbValue::Float64`] (tolerance only vs another float).
    Float(f64),
    /// Exact [`DbValue::Boolean`].
    Bool(bool),
    /// Exact [`DbValue::Null`] with [`DbType::Bool`].
    NullBool,
    /// Exact [`DbValue::Null`] (any declared type).
    Null,
}

/// Expected [`PORTABLE_SELECT`] cells in column order.
#[must_use]
pub fn portable_select_expects() -> &'static [PortableExpect] {
    &[
        PortableExpect::Int(3),
        PortableExpect::Int(4),
        PortableExpect::Int(5),
        PortableExpect::Int(2),
        PortableExpect::Text("ab"),
        PortableExpect::Text("AB"),
        PortableExpect::Text("x"),
        PortableExpect::Text("A"),
        PortableExpect::Int(1),
        PortableExpect::Int(2),
        PortableExpect::Int(1),
        PortableExpect::Int(3),
        PortableExpect::Text("v"),
        PortableExpect::Text("v"),
        PortableExpect::Int(1),
        PortableExpect::Int(0),
        PortableExpect::Text("Zb"),
        PortableExpect::Int(2),
    ]
}

/// Expected [`PORTABLE_BOOLEAN_SELECT`] cells in column order.
#[must_use]
pub fn portable_boolean_expects() -> &'static [PortableExpect] {
    &[PortableExpect::Bool(true), PortableExpect::NullBool]
}

/// Binds for [`PORTABLE_BOOLEAN_INSERT`].
#[must_use]
pub fn portable_boolean_insert_binds() -> Vec<DbValue> {
    vec![DbValue::Boolean(true), DbValue::Null(DbType::Bool)]
}

/// Binds for [`PORTABLE_INSERT_OR_IGNORE_RETURNING_LC`].
#[must_use]
pub fn portable_lowercase_insert_binds() -> Vec<DbValue> {
    vec![
        DbValue::Bytes(PORTABLE_INSERT_BLOB.to_vec()),
        DbValue::Boolean(true),
    ]
}

/// Expected [`PORTABLE_AGGREGATE_SELECT`] cells in column order.
#[must_use]
pub fn portable_aggregate_expects() -> &'static [PortableExpect] {
    &[
        PortableExpect::Int(1),
        PortableExpect::Int(2),
        PortableExpect::Int(2),
        PortableExpect::Int(2),
        PortableExpect::Int(2),
    ]
}

/// True when `got` is the exact [`DbValue`] for `want`.
#[must_use]
pub fn portable_value_matches(got: &DbValue, want: PortableExpect) -> bool {
    match (got, want) {
        (DbValue::Int64(n), PortableExpect::Int(w)) => *n == w,
        (DbValue::Text(s), PortableExpect::Text(w)) => s == w,
        (DbValue::Float64(n), PortableExpect::Float(w)) => (*n - w).abs() < 1e-9,
        (DbValue::Boolean(b), PortableExpect::Bool(w)) => *b == w,
        (DbValue::Null(DbType::Bool), PortableExpect::NullBool) => true,
        (DbValue::Null(_), PortableExpect::Null) => true,
        _ => false,
    }
}

/// SQLite expressions may omit a declared type; Postgres OIDs map to Int64/Text.
fn portable_column_type_ok(col: Option<&DbColumn>, want: PortableExpect) -> bool {
    let Some(col) = col else {
        return true;
    };
    match want {
        PortableExpect::Int(_) => matches!(col.db_type, DbType::Int64 | DbType::Unspecified),
        PortableExpect::Text(_) => matches!(col.db_type, DbType::Text | DbType::Unspecified),
        PortableExpect::Float(_) => matches!(col.db_type, DbType::Float64 | DbType::Unspecified),
        PortableExpect::Bool(_) | PortableExpect::NullBool => matches!(col.db_type, DbType::Bool),
        PortableExpect::Null => true,
    }
}

/// Formats a mismatch for adapter execution tests.
#[must_use]
pub fn portable_select_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_statement_mismatch(stmt, portable_select_expects(), "portable SELECT")
}

/// Formats a mismatch for [`PORTABLE_BOOLEAN_SELECT`].
#[must_use]
pub fn portable_boolean_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_statement_mismatch(stmt, portable_boolean_expects(), "portable BOOLEAN SELECT")
}

/// Formats a mismatch for [`PORTABLE_AGGREGATE_SELECT`].
#[must_use]
pub fn portable_aggregate_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_statement_mismatch(
        stmt,
        portable_aggregate_expects(),
        "portable aggregate SELECT",
    )
}

/// Compares `stmt` to `expect` and describes the first mismatch.
#[must_use]
pub fn portable_statement_mismatch(
    stmt: &StatementResult,
    expect: &[PortableExpect],
    label: &str,
) -> Option<String> {
    let Some(row) = stmt.rows.first() else {
        return Some(format!("{label} returned no rows"));
    };
    portable_row_mismatch(&stmt.columns, &row.values, expect, label)
}

/// Compares `values` to `expect` and describes the first mismatch.
fn portable_row_mismatch(
    columns: &[DbColumn],
    values: &[DbValue],
    expect: &[PortableExpect],
    label: &str,
) -> Option<String> {
    if values.len() != expect.len() {
        return Some(format!(
            "{label} returned {} cells; expected {}",
            values.len(),
            expect.len()
        ));
    }
    for (i, (got, want)) in values.iter().zip(expect.iter()).enumerate() {
        if !portable_value_matches(got, *want) {
            return Some(format!(
                "{label} column {i}: got {got:?}, expected {want:?}"
            ));
        }
        if !portable_column_type_ok(columns.get(i), *want) {
            return Some(format!(
                "{label} column {i} db_type {:?}, expected matching DbType (Bool required for boolean cells; Int64/Text/Float64 or Unspecified otherwise) for {want:?}",
                columns[i].db_type
            ));
        }
    }
    None
}

/// Mixed DDL whose companion INSERT stores the receipt-gate text as a value.
pub const MIXED_GATE_LITERAL_DDL: &str =
    "CREATE TABLE IF NOT EXISTS gated_notes (id INTEGER PRIMARY KEY, body TEXT)";

/// INSERT that embeds [`crate::GUEST_RECEIPT_WRITE_GATE`] in a comment and a string.
#[must_use]
pub fn mixed_gate_literal_insert() -> String {
    let gate = crate::GUEST_RECEIPT_WRITE_GATE;
    format!("INSERT INTO gated_notes (id, body)\n-- {gate}\nVALUES (1, '{gate}')")
}

/// SELECT the stored gate literal from [`mixed_gate_literal_insert`].
pub const MIXED_GATE_LITERAL_SELECT: &str = "SELECT body FROM gated_notes WHERE id = 1";

/// COUNT helper for mixed-gate replay assertions.
pub const MIXED_GATE_LITERAL_COUNT: &str = "SELECT count(*) FROM gated_notes";

/// Unique + NOT NULL table for `INSERT OR IGNORE` conflict-domain vectors.
pub const BINDING_DDL_CONFLICT: &str = "CREATE TABLE IF NOT EXISTS conflicted (\
     id INTEGER PRIMARY KEY, \
     k TEXT NOT NULL UNIQUE, \
     v TEXT NOT NULL)";

/// Unique-conflict ignore (second insert is a no-op).
pub const PORTABLE_INSERT_OR_IGNORE_UNIQUE: &str =
    "INSERT OR IGNORE INTO conflicted (id, k, v) VALUES (1, 'a', 'x')";

/// NOT NULL ignore must still error (not swallowed).
pub const PORTABLE_INSERT_OR_IGNORE_NOT_NULL: &str =
    "INSERT OR IGNORE INTO conflicted (id, k, v) VALUES (2, 'b', NULL)";

/// Scalar min/max NULL-poison (any NULL argument → NULL).
pub const PORTABLE_MIN_MAX_NULL: &str =
    "SELECT max(NULL, 3) AS c0, min(NULL, 3) AS c1, max(1, 3) AS c2";

/// Nullable column for ORDER BY NULL ordering.
pub const BINDING_DDL_ORDER_NULLS: &str = "CREATE TABLE IF NOT EXISTS ordered_n (n INTEGER)";

/// Seed three rows: 1, NULL, 2.
pub const PORTABLE_ORDER_NULLS_INSERT_1: &str = "INSERT INTO ordered_n (n) VALUES (1)";
/// NULL seed.
pub const PORTABLE_ORDER_NULLS_INSERT_NULL: &str = "INSERT INTO ordered_n (n) VALUES (NULL)";
/// 2 seed.
pub const PORTABLE_ORDER_NULLS_INSERT_2: &str = "INSERT INTO ordered_n (n) VALUES (2)";

/// ASC: NULL sorts as smallest.
pub const PORTABLE_ORDER_NULLS_ASC: &str = "SELECT n FROM ordered_n ORDER BY n ASC";

/// DESC: NULL sorts as smallest (last).
pub const PORTABLE_ORDER_NULLS_DESC: &str = "SELECT n FROM ordered_n ORDER BY n DESC";

/// Identity table (explicit id then omit-id).
pub const BINDING_DDL_IDENTITY: &str =
    "CREATE TABLE IF NOT EXISTS ident (id INTEGER PRIMARY KEY AUTOINCREMENT, n INTEGER)";

/// Explicit id 100.
pub const PORTABLE_IDENTITY_INSERT_EXPLICIT: &str = "INSERT INTO ident (id, n) VALUES (100, 1)";

/// Omit id (must yield 101 after explicit 100).
pub const PORTABLE_IDENTITY_INSERT_OMIT: &str = "INSERT INTO ident (n) VALUES (2)";

/// Largest id.
pub const PORTABLE_IDENTITY_SELECT_MAX: &str = "SELECT max(id) FROM ident";

/// Delete the max identity row (SQLite AUTOINCREMENT must not reuse it).
pub const PORTABLE_IDENTITY_DELETE_MAX: &str =
    "DELETE FROM ident WHERE id = (SELECT max(id) FROM ident)";

/// Unquoted mixed-case table that folds to lowercase.
pub const BINDING_DDL_UNQUOTED_FOLD: &str =
    "CREATE TABLE IF NOT EXISTS FoldMe (id INTEGER PRIMARY KEY, n INTEGER)";

/// SELECT via folded lowercase name.
pub const PORTABLE_UNQUOTED_FOLD_INSERT: &str = "INSERT INTO FoldMe (id, n) VALUES (1, 7)";

/// SELECT from folded name.
pub const PORTABLE_UNQUOTED_FOLD_SELECT: &str = "SELECT n FROM foldme";

/// Uncast `round` (per-row; not mixed with aggregates).
pub const PORTABLE_UNCAST_ROUND: &str = "SELECT round(r, 2) AS r0 FROM typed";

/// Uncast integer `sum` / `avg` (aggregate-only companion).
pub const PORTABLE_UNCAST_SUM_AVG: &str = "SELECT sum(n) AS s, avg(n) AS a FROM typed";

/// Expected [`PORTABLE_MIN_MAX_NULL`] cells.
#[must_use]
pub fn portable_min_max_null_expects() -> &'static [PortableExpect] {
    &[
        PortableExpect::Null,
        PortableExpect::Null,
        PortableExpect::Int(3),
    ]
}

/// Expected [`PORTABLE_ORDER_NULLS_ASC`] cells.
#[must_use]
pub fn portable_order_nulls_asc_expects() -> &'static [PortableExpect] {
    &[
        PortableExpect::Null,
        PortableExpect::Int(1),
        PortableExpect::Int(2),
    ]
}

/// Expected [`PORTABLE_ORDER_NULLS_DESC`] cells.
#[must_use]
pub fn portable_order_nulls_desc_expects() -> &'static [PortableExpect] {
    &[
        PortableExpect::Int(2),
        PortableExpect::Int(1),
        PortableExpect::Null,
    ]
}

/// Expected [`PORTABLE_UNCAST_ROUND`] cells (r=1.5).
#[must_use]
pub fn portable_uncast_round_expects() -> &'static [PortableExpect] {
    &[PortableExpect::Float(1.5)]
}

/// Expected [`PORTABLE_UNCAST_SUM_AVG`] cells (n=2).
#[must_use]
pub fn portable_uncast_sum_avg_expects() -> &'static [PortableExpect] {
    &[PortableExpect::Int(2), PortableExpect::Float(2.0)]
}

/// Runtime-edge operators/helpers (`/` `%` zero → NULL, `round` halfway, `substr`, `length`, `replace`, `abs`, `lower`).
pub const PORTABLE_RUNTIME_EDGES: &str = "SELECT \
     1 / 0 AS d0, \
     4 % 0 AS m0, \
     CAST(round(1.5) AS INTEGER) AS r0, \
     CAST(round(2.5) AS INTEGER) AS r1, \
     substr('hello', 2, 2) AS s0, \
     length('hi') AS ln, \
     replace('aa', 'a', 'b') AS rp, \
     abs(-3) AS ab, \
     lower('Hi') AS lo";

/// Expected [`PORTABLE_RUNTIME_EDGES`] cells.
#[must_use]
pub fn portable_runtime_edges_expects() -> &'static [PortableExpect] {
    &[
        PortableExpect::Null,
        PortableExpect::Null,
        PortableExpect::Int(2),
        PortableExpect::Int(3),
        PortableExpect::Text("el"),
        PortableExpect::Int(2),
        PortableExpect::Text("bb"),
        PortableExpect::Int(3),
        PortableExpect::Text("hi"),
    ]
}

/// INTEGER overflow (`+` `-` `*` `abs(i64::MIN)`) yields NULL on every backend.
pub const PORTABLE_INTEGER_OVERFLOW: &str = "SELECT \
     9223372036854775807 + 1 AS o0, \
     (-9223372036854775807 - 1) - 1 AS o1, \
     9223372036854775807 * 2 AS o2, \
     abs(-9223372036854775807 - 1) AS o3";

/// Expected [`PORTABLE_INTEGER_OVERFLOW`] cells.
#[must_use]
pub fn portable_integer_overflow_expects() -> &'static [PortableExpect] {
    &[
        PortableExpect::Null,
        PortableExpect::Null,
        PortableExpect::Null,
        PortableExpect::Null,
    ]
}

/// Formats a mismatch for [`PORTABLE_INTEGER_OVERFLOW`].
#[must_use]
pub fn portable_integer_overflow_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_statement_mismatch(
        stmt,
        portable_integer_overflow_expects(),
        "portable integer overflow",
    )
}

/// Nested INTEGER `+`/`*`/`abs` plus TEXT-inside-overflow (`length('ä') + 1`).
pub const PORTABLE_NESTED_INTEGER_ARITH: &str = "SELECT \
     1 + abs(-2) AS n0, \
     1 + 2 * 3 AS n1, \
     1 + abs(-9223372036854775807 - 1) AS n2, \
     length('ä') + 1 AS n3";

/// Expected [`PORTABLE_NESTED_INTEGER_ARITH`] cells.
#[must_use]
pub fn portable_nested_integer_arith_expects() -> &'static [PortableExpect] {
    &[
        PortableExpect::Int(3),
        PortableExpect::Int(7),
        PortableExpect::Null,
        PortableExpect::Int(2),
    ]
}

/// Formats a mismatch for [`PORTABLE_NESTED_INTEGER_ARITH`].
#[must_use]
pub fn portable_nested_integer_arith_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_statement_mismatch(
        stmt,
        portable_nested_integer_arith_expects(),
        "portable nested integer arith",
    )
}

/// `'$.…'` compared as TEXT (not a `json_extract` path) — Postgres must COLLATE.
pub const PORTABLE_JSON_PATH_LIKE_COMPARE: &str =
    "SELECT CASE WHEN '$.ä' < 'a' THEN 1 ELSE 0 END AS c0";

/// Expected [`PORTABLE_JSON_PATH_LIKE_COMPARE`] cells.
#[must_use]
pub fn portable_json_path_like_compare_expects() -> &'static [PortableExpect] {
    &[PortableExpect::Int(1)]
}

/// Formats a mismatch for [`PORTABLE_JSON_PATH_LIKE_COMPARE`].
#[must_use]
pub fn portable_json_path_like_compare_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_statement_mismatch(
        stmt,
        portable_json_path_like_compare_expects(),
        "portable json-path-like compare",
    )
}

/// `/` RHS covering call, unary, CAST, and parenthesized zero.
pub const PORTABLE_DIV_OPERANDS: &str = "SELECT \
     10 / abs(0) AS d0, \
     10 / -0 AS d1, \
     10 / CAST(0 AS INTEGER) AS d2, \
     10 / (0) AS d3";

/// Expected [`PORTABLE_DIV_OPERANDS`] cells.
#[must_use]
pub fn portable_div_operands_expects() -> &'static [PortableExpect] {
    &[
        PortableExpect::Null,
        PortableExpect::Null,
        PortableExpect::Null,
        PortableExpect::Null,
    ]
}

/// Formats a mismatch for [`PORTABLE_DIV_OPERANDS`].
#[must_use]
pub fn portable_div_operands_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_statement_mismatch(
        stmt,
        portable_div_operands_expects(),
        "portable div operands",
    )
}

/// `/` and `%` of a `NULLIF` that is **not** `NULLIF(expr, 0)` must still
/// gain an outer zero guard (`NULLIF(0, 1)` evaluates to `0`).
pub const PORTABLE_DIV_NULLIF_NOT_ZERO_GUARD: &str = "SELECT \
     1 / NULLIF(0, 1) AS d0, \
     1 % NULLIF(0, 1) AS m0, \
     10 / NULLIF(2, 0) AS already";

/// Expected [`PORTABLE_DIV_NULLIF_NOT_ZERO_GUARD`] cells.
#[must_use]
pub fn portable_div_nullif_not_zero_guard_expects() -> &'static [PortableExpect] {
    &[
        PortableExpect::Null,
        PortableExpect::Null,
        PortableExpect::Int(5),
    ]
}

/// Formats a mismatch for [`PORTABLE_DIV_NULLIF_NOT_ZERO_GUARD`].
#[must_use]
pub fn portable_div_nullif_not_zero_guard_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_statement_mismatch(
        stmt,
        portable_div_nullif_not_zero_guard_expects(),
        "portable div NULLIF not-zero guard",
    )
}

/// `json_object` nulls, duplicate keys, nested objects, and 16-pair (32-arg) arity.
///
/// Length comparisons distinguish JSON `null` retention from merge-patch
/// deletion without depending on exact JSON spacing. Postgres lowering casts
/// `json_build_object` to TEXT so `length(json_object(…))` is legal.
pub const PORTABLE_JSON_OBJECT_SEMANTICS: &str = "SELECT \
     CASE WHEN length(json_object('a', 1, 'b', NULL)) > length(json_object('a', 1)) \
          THEN 1 ELSE 0 END AS null_retained, \
     CASE WHEN length(json_object('a', 1, 'a', NULL)) > 2 THEN 1 ELSE 0 END AS dup_null_retained, \
     json_extract(json_extract(json_object('n', json_object('k', 'v')), '$.n'), '$.k') AS nested, \
     json_extract(json_object(\
        'k00', 'v00', 'k01', 'v01', 'k02', 'v02', 'k03', 'v03', \
        'k04', 'v04', 'k05', 'v05', 'k06', 'v06', 'k07', 'v07', \
        'k08', 'v08', 'k09', 'v09', 'k10', 'v10', 'k11', 'v11', \
        'k12', 'v12', 'k13', 'v13', 'k14', 'v14', 'k15', 'v15'\
     ), '$.k15') AS max_arity";

/// Expected [`PORTABLE_JSON_OBJECT_SEMANTICS`] cells.
#[must_use]
pub fn portable_json_object_semantics_expects() -> &'static [PortableExpect] {
    &[
        PortableExpect::Int(1),
        PortableExpect::Int(1),
        PortableExpect::Text("v"),
        PortableExpect::Text("v15"),
    ]
}

/// Formats a mismatch for [`PORTABLE_JSON_OBJECT_SEMANTICS`].
#[must_use]
pub fn portable_json_object_semantics_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_statement_mismatch(
        stmt,
        portable_json_object_semantics_expects(),
        "portable json_object semantics",
    )
}

/// TEXT literals whose contents look like bind placeholders or `COLLATE`.
pub const PORTABLE_TEXT_PREFIX_LITERALS: &str = "SELECT '$ä' AS a, '{ä' AS b, 'COLLATEä' AS c";

/// Expected [`PORTABLE_TEXT_PREFIX_LITERALS`] cells.
#[must_use]
pub fn portable_text_prefix_literals_expects() -> &'static [PortableExpect] {
    &[
        PortableExpect::Text("$ä"),
        PortableExpect::Text("{ä"),
        PortableExpect::Text("COLLATEä"),
    ]
}

/// Formats a mismatch for [`PORTABLE_TEXT_PREFIX_LITERALS`].
#[must_use]
pub fn portable_text_prefix_literals_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_statement_mismatch(
        stmt,
        portable_text_prefix_literals_expects(),
        "portable text prefix literals",
    )
}

/// Formats a mismatch for [`PORTABLE_RUNTIME_EDGES`].
#[must_use]
pub fn portable_runtime_edges_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_statement_mismatch(
        stmt,
        portable_runtime_edges_expects(),
        "portable runtime edges",
    )
}

/// Formats a mismatch for [`PORTABLE_MIN_MAX_NULL`].
#[must_use]
pub fn portable_min_max_null_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_statement_mismatch(
        stmt,
        portable_min_max_null_expects(),
        "portable min/max NULL",
    )
}

/// Formats a mismatch for ASC NULL ordering.
#[must_use]
pub fn portable_order_nulls_asc_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_rows_mismatch(
        stmt,
        &[
            &[PortableExpect::Null],
            &[PortableExpect::Int(1)],
            &[PortableExpect::Int(2)],
        ],
        "portable ORDER BY ASC NULLS",
    )
}

/// Formats a mismatch for DESC NULL ordering.
#[must_use]
pub fn portable_order_nulls_desc_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_rows_mismatch(
        stmt,
        &[
            &[PortableExpect::Int(2)],
            &[PortableExpect::Int(1)],
            &[PortableExpect::Null],
        ],
        "portable ORDER BY DESC NULLS",
    )
}

/// Formats a mismatch for [`PORTABLE_UNCAST_ROUND`].
#[must_use]
pub fn portable_uncast_round_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_statement_mismatch(
        stmt,
        portable_uncast_round_expects(),
        "portable uncast round",
    )
}

/// Formats a mismatch for [`PORTABLE_UNCAST_SUM_AVG`].
#[must_use]
pub fn portable_uncast_sum_avg_mismatch(stmt: &StatementResult) -> Option<String> {
    portable_statement_mismatch(
        stmt,
        portable_uncast_sum_avg_expects(),
        "portable uncast sum/avg",
    )
}

/// Compares every row in `stmt` to `expect`.
#[must_use]
pub fn portable_rows_mismatch(
    stmt: &StatementResult,
    expect: &[&[PortableExpect]],
    label: &str,
) -> Option<String> {
    if stmt.rows.len() != expect.len() {
        return Some(format!(
            "{label} returned {} rows; expected {}",
            stmt.rows.len(),
            expect.len()
        ));
    }
    for (i, (row, want)) in stmt.rows.iter().zip(expect.iter()).enumerate() {
        if let Some(err) = portable_row_mismatch(&stmt.columns, &row.values, want, label) {
            return Some(format!("row {i}: {err}"));
        }
    }
    None
}

/// `INSERT OR IGNORE … SELECT` (plain bind source).
pub const BINDING_DDL_IGNORE_SELECT: &str =
    "CREATE TABLE IF NOT EXISTS ign_sel (id INTEGER PRIMARY KEY)";

/// Plain `INSERT OR IGNORE … SELECT ? RETURNING id`.
pub const PORTABLE_IGNORE_SELECT: &str = "INSERT OR IGNORE INTO ign_sel (id) SELECT ? RETURNING id";

/// `WITH` source for `INSERT OR IGNORE`.
pub const PORTABLE_IGNORE_SELECT_WITH: &str =
    "INSERT OR IGNORE INTO ign_sel (id) WITH s(id) AS (SELECT ?) SELECT * FROM s RETURNING id";

/// Compound `UNION ALL` source.
pub const PORTABLE_IGNORE_SELECT_UNION: &str =
    "INSERT OR IGNORE INTO ign_sel (id) SELECT ? UNION ALL SELECT ? RETURNING id";

/// `ORDER BY` + `LIMIT` source.
pub const PORTABLE_IGNORE_SELECT_ORDER_LIMIT: &str =
    "INSERT OR IGNORE INTO ign_sel (id) SELECT ? AS id ORDER BY id LIMIT 1 RETURNING id";

/// Case-sensitive `LIKE` table.
pub const BINDING_DDL_LIKE: &str = "CREATE TABLE IF NOT EXISTS liked (body TEXT)";

/// Seed `'A'` for LIKE vectors.
pub const PORTABLE_LIKE_INSERT: &str = "INSERT INTO liked (body) VALUES ('A')";

/// Case-sensitive LIKE probes (A/a, prefix, GLOB metacharacters, NULL).
pub const PORTABLE_LIKE_SELECT: &str = "SELECT \
     CASE WHEN 'A' LIKE 'a' THEN 1 ELSE 0 END AS c0, \
     CASE WHEN 'A' LIKE 'A' THEN 1 ELSE 0 END AS c1, \
     CASE WHEN 'A' LIKE 'A%' THEN 1 ELSE 0 END AS c2, \
     CASE WHEN 'A' LIKE 'A*' THEN 1 ELSE 0 END AS c3, \
     CASE WHEN 'A' LIKE 'A?' THEN 1 ELSE 0 END AS c4, \
     CASE WHEN 'A' LIKE 'A[' THEN 1 ELSE 0 END AS c5, \
     CASE WHEN 'ab' LIKE 'a_b' THEN 1 ELSE 0 END AS c6, \
     CASE WHEN 'A' LIKE NULL THEN 1 ELSE 0 END AS c7, \
     CASE WHEN body LIKE ? THEN 1 ELSE 0 END AS c8 \
     FROM liked";

/// Non-ASCII that must not Unicode-fold under LIKE.
pub const PORTABLE_LIKE_NON_ASCII: &str = "SELECT CASE WHEN 'İ' LIKE 'i' THEN 1 ELSE 0 END AS c0";

/// Typed BLOB default hex.
pub const BINDING_DDL_BLOB_DEFAULT: &str =
    "CREATE TABLE IF NOT EXISTS blobdef (id INTEGER PRIMARY KEY, payload BLOB DEFAULT X'deadbeef')";

/// INSERT using the BLOB default.
pub const PORTABLE_BLOB_DEFAULT_INSERT: &str = "INSERT INTO blobdef (id) VALUES (1)";

/// Read the default blob.
pub const PORTABLE_BLOB_DEFAULT_SELECT: &str = "SELECT payload FROM blobdef WHERE id = 1";

/// TEXT collation / order table.
pub const BINDING_DDL_TEXT_ORDER: &str = "CREATE TABLE IF NOT EXISTS textord (body TEXT)";

/// Seed ASCII + non-ASCII rows.
pub const PORTABLE_TEXT_ORDER_INSERT_B: &str = "INSERT INTO textord (body) VALUES ('B')";
/// Lowercase a.
pub const PORTABLE_TEXT_ORDER_INSERT_A: &str = "INSERT INTO textord (body) VALUES ('a')";
/// Non-ASCII é (U+00E9).
pub const PORTABLE_TEXT_ORDER_INSERT_EACUTE: &str = "INSERT INTO textord (body) VALUES ('é')";
/// ASCII e.
pub const PORTABLE_TEXT_ORDER_INSERT_E: &str = "INSERT INTO textord (body) VALUES ('e')";

/// Binary TEXT order (byte/code-point, not locale).
pub const PORTABLE_TEXT_ORDER_SELECT: &str = "SELECT body FROM textord ORDER BY body";

/// TEXT comparison + scalar min/max + lower/upper.
pub const PORTABLE_TEXT_OPS: &str = "SELECT \
     CASE WHEN 'A' < 'a' THEN 1 ELSE 0 END AS c0, \
     CASE WHEN 'A' > 'a' THEN 1 ELSE 0 END AS c1, \
     min('A', 'a') AS c2, \
     max('A', 'a') AS c3, \
     lower('Ab') AS c4, \
     upper('Ab') AS c5";

/// Full Unicode scalar TEXT (UTF-8), including supplementary-plane Han, RTL,
/// combining sequences, and NFC vs NFD that must not be silently normalized.
pub const PORTABLE_TEXT_MULTILINGUAL: &str = "SELECT \
     '汉语' AS cjk, \
     '日本語' AS ja, \
     '한국어' AS ko, \
     '𠀀' AS han_ext, \
     'مرحبا' AS ar, \
     'שלום' AS he, \
     'नमस्ते' AS hi, \
     'สวัสดี' AS th, \
     'γειά' AS el, \
     'привет' AS ru, \
     '🎵🦀' AS emoji, \
     'café' AS nfc, \
     'cafe\u{0301}' AS nfd";

/// Identity omit after rollback of an explicit id (same atomic request fails).
pub const PORTABLE_IDENTITY_INSERT_BAD_TYPE: &str = "INSERT INTO ident (id, n) VALUES (100, 'x')";

/// DROP identity table.
pub const PORTABLE_IDENTITY_DROP: &str = "DROP TABLE IF EXISTS ident";

/// Expected [`PORTABLE_LIKE_SELECT`] cells (`c8` bind is `'A'`).
#[must_use]
pub fn portable_like_expects() -> &'static [PortableExpect] {
    &[
        PortableExpect::Int(0),
        PortableExpect::Int(1),
        PortableExpect::Int(1),
        PortableExpect::Int(0),
        PortableExpect::Int(0),
        PortableExpect::Int(0),
        PortableExpect::Int(0),
        PortableExpect::Int(0),
        PortableExpect::Int(1),
    ]
}

/// Expected [`PORTABLE_TEXT_ORDER_SELECT`] rows.
#[must_use]
pub fn portable_text_order_expects() -> &'static [&'static [PortableExpect]] {
    &[
        &[PortableExpect::Text("B")],
        &[PortableExpect::Text("a")],
        &[PortableExpect::Text("e")],
        &[PortableExpect::Text("é")],
    ]
}

/// Expected [`PORTABLE_TEXT_OPS`] cells.
#[must_use]
pub fn portable_text_ops_expects() -> &'static [PortableExpect] {
    &[
        PortableExpect::Int(1),
        PortableExpect::Int(0),
        PortableExpect::Text("A"),
        PortableExpect::Text("a"),
        PortableExpect::Text("ab"),
        PortableExpect::Text("AB"),
    ]
}

/// Expected [`PORTABLE_TEXT_MULTILINGUAL`] cells (exact UTF-8, no NFC/NFD fold).
#[must_use]
pub fn portable_text_multilingual_expects() -> &'static [PortableExpect] {
    &[
        PortableExpect::Text("汉语"),
        PortableExpect::Text("日本語"),
        PortableExpect::Text("한국어"),
        PortableExpect::Text("𠀀"),
        PortableExpect::Text("مرحبا"),
        PortableExpect::Text("שלום"),
        PortableExpect::Text("नमस्ते"),
        PortableExpect::Text("สวัสดี"),
        PortableExpect::Text("γειά"),
        PortableExpect::Text("привет"),
        PortableExpect::Text("🎵🦀"),
        PortableExpect::Text("café"),
        PortableExpect::Text("cafe\u{0301}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_int_does_not_accept_float_or_text() {
        assert!(portable_value_matches(
            &DbValue::Int64(2),
            PortableExpect::Int(2)
        ));
        assert!(!portable_value_matches(
            &DbValue::Float64(2.0),
            PortableExpect::Int(2)
        ));
        assert!(!portable_value_matches(
            &DbValue::Text("2".into()),
            PortableExpect::Int(2)
        ));
        assert!(portable_value_matches(
            &DbValue::Text("ab".into()),
            PortableExpect::Text("ab")
        ));
        assert!(portable_value_matches(
            &DbValue::Boolean(true),
            PortableExpect::Bool(true)
        ));
        assert!(!portable_value_matches(
            &DbValue::Int64(1),
            PortableExpect::Bool(true)
        ));
        assert!(portable_value_matches(
            &DbValue::Null(DbType::Bool),
            PortableExpect::NullBool
        ));
        assert!(!portable_value_matches(
            &DbValue::Null(DbType::Int64),
            PortableExpect::NullBool
        ));
    }
}
