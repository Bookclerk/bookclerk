# SQL-v1 frontend spike (host-private)

Temporary comparison of Syntaqlite 0.9 and sqlparser-rs against Bookclerk SQL v1.
Parser-library types do not leave this crate. Not a guest SDK or Cap’n surface.

Run:

```bash
cargo test -p bookclerk-sql-frontend-spike --lib -- --nocapture
```

- `src/corpus.rs` — machine-readable positive/negative list (PR #179 tests + construct matrix)
- `testdata/baseline.md` — Phase 1 LOC / tests / deps
- `testdata/phase2.md` — Syntaqlite vs sqlparser-rs measurements and stop-rule
- `testdata/gonogo.md` — Phase 9 quantitative comparison

**Go/no-go (parser libraries):** keep the current Bookclerk frontend. See `testdata/phase2.md` and `testdata/gonogo.md`. Independent wins (CREATE proof schema, SeaQuery catalog DML, proof auth) stay on the production path.
