# Phase 2 frontend comparison

Host-only crate. Parser-library types do not leave this crate. Guests do not link Syntaqlite C.

Run: `cargo test -p bookclerk-sql-frontend-spike --lib -- --nocapture`

## Corpus (n=72)

Agreement is versus `GrammarExpect` (admit/reject), not versus `validate_sql_v1_grammar` alone. Some rejects are type/schema-layer (`quoted-table`, `STRICT`, `AUTOINCREMENT` without PK) and Bookclerk grammar still returns Ok; the allowlist is the fail-closed gate for libraries.

| Engine | agree | false admit | false reject |
| --- | --- | --- | --- |
| Syntaqlite 0.9.0 + allowlist + `with_strict_schema` | 65 | 4 | 3 |
| sqlparser-rs 0.59 `SQLiteDialect` + allowlist + CTE subtract | 67 | 5 | 0 |

## Syntaqlite findings (cannot replace TypeCx / physical_accesses)

- `physical_tables_accessed()` is **query-body lineage only**. `INSERT INTO` / `UPDATE` / `DELETE FROM` destinations are empty. Authorization cannot consume Analyzer tables alone.
- Column lineage is SELECT-list origins, not every referenced column (WHERE/JOIN).
- Analyzer is SQLite types, not Bookclerk `SqlType`. `count(*)` needs a Bookclerk arity (`AtLeast(0)`); unknown columns are diagnostics under strict schema.
- `DEFAULT CAST(1 AS INTEGER)` fails parse (`syntax error near '('`).
- C Lemon parser + `cc`; crate edition 2024. Default features `sqlite`+`fmt`+`analysis` (`fmt` is required for `crate::dialect`). Docs mention a `validation` feature that does not exist.
- Young 0.x (~2026). MSRV of the crate is edition 2024; workspace rustc 1.98 can consume it. Must stay host-only.

False admits without extra allowlist work: `CREATE VIEW`, qualified `public.books`, `round` arity, `DEFAULT (1)`, `REAL PRIMARY KEY AUTOINCREMENT`.

False rejects: `count(*)`, unknown column `c` in `min(a,b,c)` against schema `t(a,b)` (strict schema; Bookclerk grammar admits), `DEFAULT CAST`, `CREATE TABLE … DEFAULT CAST`.

## sqlparser-rs findings (cannot delete TScan)

- Visitor `pre_visit_table_factor` sees DML targets and FROM tables. CTE subtraction is a few dozen lines — **not** TypeCx (no correlated lookup, no column lineage, no types).
- Spans are `Location` (line/col), not guaranteed byte offsets. Lowering still needs a source walker for `?` / comments / quotes.
- No semantic analysis (upstream). Residual `TScan` ≈ name/scope/CTE/types — the expensive part of `sql_types.rs`.
- Pure Rust; guests could theoretically depend, but the production parser stays out of the guest SDK either way.

False admits: `CREATE VIEW`, `AUTOINCREMENT` without PK, `REAL AUTOINCREMENT`, qualified names (`public` as table), `round` arity, `DEFAULT (1)`.

## Decision (Phase 3)

**Do not wire either library into admission.** Neither deletes a Bookclerk-owned *category* of machinery:

- Grammar + fail-closed allowlist still have to live in Bookclerk (library-accept ≠ SQL-v1-admit).
- Syntaqlite Analyzer does not produce `physical_accesses` for DML and is SQLite-typed.
- sqlparser-rs leaves almost all of `TScan` / TypeCx in place.
- Dual parser is forbidden. C in the guest SDK is forbidden.

Keep the handwritten `parse_v1_*` / `Scan` / `TScan` frontend. Independent wins (Phase 4 CREATE proof, Phase 5 proof-auth, Phase 7 SeaQuery catalog DML) still pay.

## Build cost

Syntaqlite already compiles C via `cc` (same class as rusqlite on native hosts). Incremental `cargo test -p bookclerk-sql-frontend-spike --lib` after a warm `target/` is ~1s for this crate + ~0.02s tests. Full Syntaqlite C compile is a one-time host cost, not a guest WASM cost when the crate is not a default-member of plugin-sdk.
