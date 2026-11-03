# ADR: The Web API document is checked against the running daemon

- **Status:** Accepted
- **Date:** 2026-11-03
- **Related:** [Code documentation → Mechanical checks](../contributing/code-documentation.md#mechanical-checks-ci),
  [CI → Web API contract](../contributing/ci.md#web-api-contract),
  [CI → Coverage](../contributing/ci.md#coverage)

## Context

The Web API reference is generated from the daemon's types and route
declarations (utoipa), and tests keep the document complete: every body
names a schema, every property and body carries an example, every error
response shows its body, and every example validates against its schema.
Two things stayed unchecked. A route's `#[utoipa::path]` declaration is a
hand-written list of statuses and bodies, so a handler can answer a status
the route does not declare, or a body of another shape, and nothing fails.
And an example is a value someone wrote, so it can describe a response the
daemon never produces. Reviewers caught both kinds by hand while the
examples were added, which is the sign a test should do it.

Three ways to close the gap were weighed: validating examples against
schemas (cheap, deterministic, but blind to what handlers do), checking
real responses against the document inside the tests (catches declaration
drift wherever a test reaches, blind where none does), and generating the
examples from recorded responses (the end state, but it needs the second
check's coverage first, plus stable rewrites of ids, times and paths and a
screen for secrets).

## Decision

1. Examples are validated against their schemas in the daemon's test
   suite (`every_example_matches_its_schema`): every example, on a
   schema, a property or a body, must validate against the schema it
   illustrates, references followed and formats asserted.
2. The daemon's test build layers a check over its router that compares
   every response the tests produce with the document: the matched route,
   the method, the status and the media type must be declared, and a JSON
   body must validate against the declared schema. The layer records each
   response, and CI Gate reports how many declared responses the tests
   observed and every disagreement. The layer is compiled only into the
   test binary; the daemon's own router does not change.
3. The report is informational first. Once the disagreements it finds are
   fixed, a disagreement fails CI Gate and the observed share gets a floor
   that only rises, the way the coverage floors do.
4. Examples stay hand-written. Generating them from recorded responses is
   deferred until the observed share is high enough that recordings would
   replace most of them; a recording then replaces an example route by
   route, with the hand-written one kept until a recording exists.

## Consequences

- A handler that answers a status or body its route does not declare is
  named in CI Gate's summary by the first test that exercises it, and will
  fail CI Gate once the report is enforced.
- A route no test reaches is invisible to the layer. The report's
  observed share makes that visible, and the floor keeps it from shrinking.
- The checks add one dev-dependency (`jsonschema`) to the daemon and one
  file per test process under `target/contract-report`, kept with the
  coverage reports in CI.
- Responses the tests buffer for validation are read once more; the test
  build is slower by that much, the daemon is not.
