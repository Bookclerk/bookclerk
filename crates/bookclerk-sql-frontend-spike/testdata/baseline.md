# Phase 1 baseline (PR #179 / `cursor/sql-thin-adapters-ec97`)

Recorded against `37c487fa` (`fix(sql): keep json_extract paths parseable after TEXT COLLATE`).

## Targeted tests

| Suite | Result | Wall time |
| --- | --- | --- |
| `cargo test -p bookclerk-plugin-abi --lib` | 95 passed | ~20s |
| `cargo test -p bookclerk-db-exec --lib` | 60 passed | ~27s |
| `cargo test -p bookclerk-library --lib sql_plan` | 114 passed, 21 ignored (Postgres URL) | ~52s |

## Custom SQL inventory (`tokei` / `wc -l`, Cap’n generated excluded)

| Module | LOC | Role |
| --- | --- | --- |
| `crates/bookclerk-plugin-abi/src/guest_sql.rs` | 4509 | `Scan`, `parse_v1_*`, `parse_guest_sql_refs` |
| `crates/bookclerk-plugin-abi/src/sql_types.rs` | 3531 | `TScan`, CTE/alias typing, `parse_create_table_schema`, catalog companions |
| `crates/bookclerk-plugin-abi/src/sql_proof.rs` | 163 | `ResolvedStatement` |
| `crates/bookclerk-db-exec/src/lower.rs` | 2144 | Independent trivia scanning for adapter lowering |
| `crates/bookclerk-db-exec/src/typed.rs` | 1953 | Execution; reparses CREATE for sqlite_master fingerprints |
| `crates/bookclerk-db-exec/src/schema_postgres.rs` | 642 | Identity trigger generation reparses CREATE/DROP |

## Deps (pre-spike)

`bookclerk-plugin-abi`: no SQL parser library. Guests compile `guest_sql` + `sql_types`.
`bookclerk-db-exec`: `bookclerk-plugin-abi` (host), sea-orm (pulls sea-query 1.0.1), tokio, serde.

## Corpus

Positive/negative statements live in `src/corpus.rs` (extracted from guest_sql / sql_types / lower tests plus the SQL-v1 construct matrix). Do not shrink the contract.
