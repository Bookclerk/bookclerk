# Design decisions

This shelf is the decision log: one architecture decision record (ADR)
for each choice that shapes Bookclerk. Each record keeps the usual form, a
**Status** line, then **Context**, **Decision** and **Consequences**. When
a later change supersedes one, its **Status** says so.

## Records

- [Plugin capabilities v3](plugin-capabilities-v3.md): entrypoints,
  triggers, typed Cap'n Proto and `env.EVENTS`.
- [Workers RPC and workerd](plugin-workers-rpc-workerd.md): jail,
  isolation and the workerd pin (partly superseded by v3).
- [First-party identity](first-party-identity.md): Owner, Administrator
  and Member, passkeys and OIDC.
- [Plugin-provided OIDC clients](plugin-oidc-clients.md): players declare
  identity-provider clients.
- [SQL database contract](sql-database-contract.md): host-resolved
  canonical SQL, adapter-owned lowering.
- [Schema versioning](schema-versioning.md): frozen schema numbers,
  checksums and backups.
- [Control plane](control-plane.md): the bootstrap boundary, host
  identity and cluster coordination.
- [Untrusted-input fuzzing](untrusted-input-fuzzing.md): every untrusted
  boundary ships a fuzz target.
- [Security advisory automation](security-advisory-automation.md): draft
  advisories for shipped code.
- [Logging and tracing](observability.md): trace everything, keep TRACE
  for diagnostics, show each output its own level.
- [Library data model](library-data-model.md): FRBR works, expressions
  and editions, holdings and edition files.
- [Web API contract checks](web-api-contract-checks.md): the document is
  checked against examples in tests and against real responses in CI.
- [Project licensing](licensing.md): AGPL for the project, MIT OR
  Apache-2.0 for the plugin SDKs, a CLA and a trademark policy.
- [Event delivery model](event-delivery-model.md): events fan out to
  every subscriber, jobs have one handler, and subscriptions follow
  deployments.

## Adding a record

A new architecture decision gets a new file here, listed above and in
the site navigation. [Writing docs](../contributing/writing-docs.md) has
the format.
