# Phase 9 go/no-go

Compared to `testdata/baseline.md` (PR #179 head `37c487fa`).

## 1. Baseline

Custom SQL understanding remains split across `guest_sql.rs` (~4509), `sql_types.rs` (~3531), `sql_proof.rs` (~163), `lower.rs` (~2144), `typed.rs` (~1953), `schema_postgres.rs` (~642). Targeted tests were green before the spike.

## 2. Syntaqlite 0.9.0

Parses most SQL-v1 DML/DDL. Tokens + byte spans + comments work with `ParserConfig::with_collect_tokens(true)`. Analyzer + `with_strict_schema` resolves SELECT FROM tables after CTEs and yields SELECT-list lineage.

**Does not replace TypeCx / `physical_accesses`:** DML destinations (`INSERT INTO` / `UPDATE` / `DELETE FROM`) are missing from `physical_tables_accessed()`. Analyzer is SQLite types, not Bookclerk `SqlType`. C Lemon + `cc`; edition 2024; 0.x. Must stay host-only. Corpus: agree 65 / false-admit 4 / false-reject 3 (n=72) after a fail-closed allowlist.

## 3. sqlparser-rs 0.59

Pure Rust, `SQLiteDialect`, Visitor table factors include DML targets. CTE subtraction is residual, not TypeCx. Spans are line/col, not reliable byte offsets for lowering. No semantic analysis. Corpus: agree 67 / false-admit 5 / false-reject 0. Would keep almost all of `TScan`.

## 4. SeaQuery 1.0.1

Adopted for **host-generated catalog INSERT/DELETE** (`catalog_companions_for_action`) behind `bookclerk-plugin-abi` feature `host`. Emits `SqliteQueryBuilder` SQL (quoted identifiers, escaped values). Static `CREATE TABLE IF NOT EXISTS` catalog DDL stays handwritten. Guest SQL is not compiled into SeaQuery. Guest (`!host`) path keeps `escape_sql_str` interpolation.

## 5. SQLGlot / other

Not on the runtime path. Optional CI oracle skipped (noisy vs adapters as source of truth). DataFusion/DuckDB/`pg_query`/`sqlite3-parser` not pursued.

## 6. Chosen architecture

**Keep the current Bookclerk frontend** (`parse_v1_*` / `Scan` / `TScan` / TypeCx → `ResolvedStatement`). Do not wire Syntaqlite or sqlparser-rs into admission. Canonical SQL remains the public representation. `ResolvedStatement` stays the host-private proof.

Independent wins retained:

- `SchemaAction::Create { schema, fingerprint, noop }` carries `CreateTableSchema`.
- Catalog companions + Postgres identity consume the proof (no canonical CREATE reparse on the execute path).
- Authorization uses `physical_accesses` + `functions` when a type env exists.
- SeaQuery for host catalog DML.

## 7. Custom components deleted

- Execute-path reparse of canonical CREATE in `catalog_companions_for_action`, `apply_schema_sql_to_env` (execute uses `apply_schema_action_to_env`), and `postgres_identity_companions` when a proof is present.
- Manual `escape_sql_str` interpolation for host catalog INSERT/DELETE.

Not deleted: `Scan`, `parse_v1_*`, `TScan`, `parse_guest_sql_refs` (library guests without types + D1 declared-type lookup), `parse_create_table_schema` (proof producer + `sqlite_master` engine DDL + no-proof callers), `lower.rs` trivia scanners.

## 8. Semantic code retained and why

`SqlType`, bind typing, CAST matrix, BOOLEAN contexts, portable helpers, INTEGER overflow→NULL, `/` `%` by zero, TEXT collation, schema policy, result shape, authorization policy. Libraries cannot own these. Name/scope/CTE resolution stays in TypeCx because Syntaqlite DML lineage is incomplete.

## 9. Dep / build impact

- `syntaqlite 0.9.0` + `sqlparser 0.59`: **spike crate only** (`bookclerk-sql-frontend-spike`, `publish = false`, not a default-member). Guests do not link C.
- `sea-query 1.0.1` optional on `bookclerk-plugin-abi` feature `host` (already in the workspace via SeaORM).
- MSRV 1.94 unchanged. rustc 1.98 consumes edition-2024 crates.

## 10. Test / conformance

- `cargo test -p bookclerk-plugin-abi --lib --features host` — 102 passed
- `cargo test -p bookclerk-plugin-abi --lib` — 98 passed
- `cargo test -p bookclerk-db-exec --lib` — 60 passed
- `cargo test -p bookclerk-library --lib sql_plan` — 114 passed, 21 ignored
- `cargo test -p bookclerk-library --test binding_databases` — 21 passed
- `cargo test -p bookclerk-sql-frontend-spike --lib` — 2 passed
- `cargo clippy -p bookclerk-plugin-abi --features host --all-targets -- -D warnings`
- `cargo clippy -p bookclerk-db-exec --all-targets -- -D warnings`
- `cargo clippy -p bookclerk-sql-frontend-spike --all-targets -- -D warnings`

No SQL-v1 behavioral deltas intended. Binding mixed DDL/DML still applies catalog rows.

## 11. Residual risks

- Library guests without `SqlTypeEnv` still use lexical `parse_guest_sql_refs` (SQLite helpers vs portable typecheck).
- `sqlite_master` / engine-native CREATE still parsed for physical fingerprint probes.
- Dual-parser risk is avoided by **not** shipping Syntaqlite/sqlparser on the admission path.
- Syntaqlite 0.x API churn if revisited.
- SeaQuery quotes catalog identifiers; companions are host-internal `execute_raw`, not guest SQL-v1 text.

## 12. Commits on PR #179

Spike crate + measurements; CREATE schema on the proof + SeaQuery catalog DML; proof-directed auth + adversarial tests; contract/ADR go/no-go notes.
