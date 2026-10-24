# ADR: Every untrusted-input boundary is fuzzed

- **Status:** Accepted
- **Date:** 2026-10-22
- **Related:** [Plugin capabilities v3](plugin-capabilities-v3.md),
  [Workers RPC + workerd](plugin-workers-rpc-workerd.md),
  [SQL database contract](sql-database-contract.md),
  [Continuous integration](../ci.md#fuzzing)

## Context

Bookclerk's security model confines third-party code (plugins, codecs,
database adapters) in jails and lets it talk to the host only through
narrow, hand-written protocol surfaces: the Cap'n Proto ABI, the
`bookclerk-workerd` granted HTTP channel, the native socket proxy, the
multiplexed byte link, canonical SQL authorization and lowering, and store
media and metadata parsers. Those surfaces are exactly where a bug turns
confinement into a host compromise, a confidentiality leak between plugins,
or a denial of service against the daemon.

The first coverage-guided target (`mp4_parse`, 2026-10-19) found six crashes
in a parser that already had unit tests and a review. Reviewing the granted
HTTP channel by hand afterwards found four more bugs of the same shape
(a panic on a split UTF-8 escape, a silently truncated chunked upload, an
unbounded chunk buffer, chunked bodies never decoded). Unit tests encode the
inputs their author imagined; a fuzzer supplies the rest.

## Decision

1. **A fuzz target is part of the definition of done for an untrusted-input
   boundary.** Any code that parses, decodes, authorizes or otherwise
   interprets bytes it did not produce itself ships with a `cargo-fuzz`
   target under `fuzz/`, a checked-in seed corpus under
   `fuzz/corpus/<target>/`, and a replay test in the owning crate that runs
   the corpus in ordinary PR CI. Untrusted here means: anything a plugin
   guest sends the host, anything a store or CDN returns, any plugin
   package or archive, any request from another node, and any reply from a
   confined helper process (`bookclerk-media-worker`, database guests).
2. **The boundary list is maintained in [`docs/ci.md`](../ci.md#fuzzing).**
   A pull request that adds a boundary, or widens one, adds the target in
   the same pull request; a review that finds such a boundary without its
   target, corpus and replay test blocks on it. Only a pre-existing boundary
   unrelated to the pull request's scope is deferred: the review files an
   issue labeled `review-followup` and that target lands in a follow-up.
3. **Targets assert invariants, not just "no panic".** Where a boundary has
   an authorization or confinement property, the target checks it against
   an independent oracle: for guest SQL, the set of tables and columns a
   real SQLite authorizer sees the statement touch must be inside the
   `GuestSqlPolicy`; for egress, the host that is authorized is the host
   that is dialed; for codecs, `read(write(v)) == v`; for framing, every
   documented memory and stream cap holds after every step.
4. **Structure-aware where the grammar is rich.** SQL-v1 statements and
   Cap'n Proto DTOs are generated from their grammar or schema (`arbitrary`
   / the shared `proptest` generators), so the fuzzer spends its budget
   inside the grammar instead of on lexer errors; a grammar as small as the
   multiplexer's nine-byte frame header is covered by raw bytes plus a
   dictionary of header pieces. `scripts/gen-plugin-abi.py` emits the Cap'n
   Proto harness pieces (`read_any_message`, `roundtrip_any_message`) so a
   new schema type is fuzzed without hand-written code.
5. **Crashes are handled as security reports.** A crash found by scheduled
   fuzzing is not filed as a public issue. It is fixed first, and the
   minimized input lands as `fuzz/corpus/<target>/regression_*` in the
   fixing pull request, so the finding becomes public together with its fix.
6. **Boundaries outside the Rust workspace use their language's own
   fuzzing.** A boundary in non-Rust tooling (a CI script or a Worker
   that decides what untrusted data reaches a public surface) cannot be a
   `cargo-fuzz` target. It ships with the same pieces in its own language
   instead: the parsing as a sans-IO function, a checked-in seed corpus, a
   replay test in PR CI, and fuzzing (coverage-guided where the language
   has a practical engine, seeded mutation otherwise) checked against an
   independent oracle. [`docs/ci.md`](../ci.md#fuzzing) lists these
   boundaries beside the `cargo-fuzz` targets. Shipped Rust code is never
   moved out of `cargo-fuzz` this way.
7. **Sanitizers follow the code under test.** Pure-Rust targets run with
   debug assertions and overflow checks on under AddressSanitizer. The first
   target whose boundary code links C (LAME, a bundled SQLite in a host
   adapter) adds UndefinedBehaviorSanitizer to the workflow matrix; none
   does yet. C code that serves only as an oracle (the SQLite the guest SQL
   target compiles admitted statements against) is not under test and does
   not trigger that step. MemorySanitizer is not used.

## Consequences

- `AGENTS.md` lists fuzzing among the code standards; reviewers treat a
  missing target on a new boundary as a finding inside the pull request's
  scope.
- ClusterFuzzLite runs daily, not weekly, so each target gets a useful
  budget as the target count grows; PR fuzzing stays short and focused on
  changed code.
- Boundary code is written sans-IO where it is not already, so the parsing
  can be driven from a byte slice: the granted HTTP channel's request-head,
  chunked-body and query parsing live in `bookclerk_workerd::granted_http`
  for that reason.
- Adding a boundary costs an afternoon more than it used to. That is the
  trade.

## Non-goals

- Fuzzing first-party code paths whose inputs the host itself produces
  (configuration it wrote, its own database rows, its own CLI).
- Replacing the `proptest` property suites or the sqlite/postgres
  differential; those stay as the long-running property checks.
