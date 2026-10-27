# Fuzzing

How the untrusted-input fuzz targets run in CI, and what to do when one finds
a crash. The rule that every boundary ships with a target is in
[ADR: Untrusted-input fuzzing](../adr/untrusted-input-fuzzing.md); the
rest of CI is in [Continuous integration](ci.md).

Coverage-guided fuzzing runs on [ClusterFuzzLite](https://google.github.io/clusterfuzzlite/)
over the `cargo-fuzz` targets in `fuzz/`. Every untrusted-input boundary
ships a target, a seed corpus and a replay test
([ADR](../adr/untrusted-input-fuzzing.md)); this table is the boundary list, and
a pull request that adds or widens a boundary extends it.

| Target | Boundary | Oracle beyond "no panic" |
| --- | --- | --- |
| `sql_parse` | SQL-v1 lexer, packer, grammar and TEXT admission (`bookclerk-plugin-abi`) | — |
| `sql_lower` | Adapter lowering of canonical SQL (`bookclerk-db-exec`) | Quoted strings and comments survive lowering verbatim except documented rewrites |
| `mp4_parse` | ISO-BMFF parse and remux of store downloads (`bookclerk-mp4`) | — |
| `granted_http` | Granted reverse channel: request head, body framing, chunked bodies, query string (`bookclerk-workerd::granted_http`) | Split-invariant chunked decoding; agrees with an `httparse` reference decoder; the route body reader (framing dispatch, byte cap, end of stream) is split-invariant and yields what the declared framing promises |
| `egress_policy` | Guest egress allowlist (`bookclerk-plugin-manifest::EgressPolicy`, JSON wire form; the workerd `egress.js` gate is a separate implementation, see #335 below) | Deny allows nothing; loopback/private/metadata need a CIDR in every spelling (trailing dot, fullwidth digits, legacy `127.1` / hex / octal IPv4); `*.suffix` never matches `xsuffix`; percent-encoded hosts never match; JSON round trip |
| `plugin_manifest` | `plugin.toml` parse, validation, derived egress and consent (`bookclerk-plugin-manifest`) | `format_manifest` round trip; a deny manifest yields a deny policy |
| `abi_decode` | Every generated Cap'n Proto `read_*` codec (`read_any_message`) and the database envelope decoders (`bookclerk-plugin-abi`) | Traversal budget derived from input size |
| `abi_roundtrip` | Every generated Cap'n Proto `read_*` / `write_*` codec pair (`roundtrip_any_message`, `bookclerk-plugin-abi`) | A value the decoder accepts re-encodes, re-decodes and compares equal (`read(write(v)) == v`) |
| `guest_sql_authz` | Guest SQL authorization, every host-issued policy shape (`bookclerk-plugin-abi::GuestSqlPolicy`, lowered as the SQLite adapter lowers it) | Every table, column, function and schema action SQLite's own authorizer reports for an admitted statement is inside the policy; shared oracle `fuzz/oracles/guest_sql_authz.rs`, replayed from `bookclerk-library` |
| `mux_peer` | Peer frames on the multiplexed byte link (`bookclerk-plugin-sdk::mux`: `Open`, `Data`, `Close`, `Window`, lengths and ids) | Settles within a bound once the peer's bytes end, delivers no more than the peer sent, accepts no more streams than the peer could open, leaves no task behind; shared oracle `fuzz/oracles/mux_peer.rs`, replayed from `bookclerk-plugin-sdk` |

These boundaries live outside the Rust workspace and follow the ADR's rule for
non-Rust tooling (decision 6):

- The diagnostics holding issue editor
  (`tools/diagnostics-collector/scripts/holding_issue.py`), which appends
  report IDs to the hidden report block in the body of the GitHub issue that
  holds untriaged reports (a body anyone with triage rights can edit).
  `scripts/tests/test_holding_issue.py` replays its seed corpus
  (`scripts/tests/holding_issue_corpus/`) and runs seeded mutation fuzzing on
  every PR, checking each accepted result against an independent reading of
  the body (the dashboard's block rule: only a body whose last block's ID
  lines end on its `-->` line is edited, the new IDs follow the listed ones
  inside it, nothing else in the body changes, the list and body stay within
  their caps, and a second append changes nothing).

- The docs publish step's reader of the docs repository's current
  `last-updated.json` (`previous_dates` in `scripts/docs-publish.py`), which
  carries the generated pages' dates forward from one publish to the next.
  `scripts/tests/test_docs_publish.py` replays its seed corpus
  (`scripts/tests/docs_publish_dates_corpus/`) and runs seeded mutation
  fuzzing on every PR, checking each result against an independent reading
  (the document's shape by exact types, each date digit by digit against a
  days-in-month table).

- The diagnostics triage plan validator
  (`tools/diagnostics-collector/scripts/validate_plan.py`), which decides what
  model output reaches public issues.
  `scripts/tests/test_diagnostics_plan.py` replays its seed corpus
  (`scripts/tests/diagnostics_plan_corpus/`) and runs seeded mutation fuzzing
  on every PR, checking each accepted plan against an independent oracle (no
  links, mentions, cross-references, raw HTML, comments or forged Report IDs
  or report blocks; only batch report IDs, listed bare in the one report
  block that ends the body; at most five issues; attachments only to the
  issues the collector offered, only batch report UUIDs, each report under
  one issue and in no new issue, at most twenty issues).
- The diagnostics regression issue planner
  (`tools/diagnostics-collector/scripts/regression_issue.py`), which turns
  the `regression_of` entries of an ingest pull (versions and platforms from
  reports anyone can submit, titles from issues anyone with triage rights can
  edit) into public issues. `scripts/tests/test_regression_issue.py` replays
  its seed corpus (`scripts/tests/regression_issue_corpus/`) and runs seeded
  mutation fuzzing on every PR, checking each result against an independent
  re-derivation of the groups (one issue per closed issue and `major.minor`
  line, at most ten; only the pull's report IDs, listed bare in the one
  report block that ends the body; a marker line naming the validated issue
  number; no links,
  mentions, raw HTML or references in the title and prose beyond the closed
  issue and its fix pull request, both validated integers). The dashboard's
  Report IDs parser (`dashboard/report-ids.ts`, below) reads that marker.
- The diagnostics summary comment renderer
  (`tools/diagnostics-collector/scripts/summary_comment.py`), which turns
  the collector's `GET /issues/summaries` answer (counts, plus versions and
  platforms taken from reports anyone can submit) into the one comment the
  ingest run keeps on each issue. `scripts/tests/test_summary_comment.py`
  replays its seed corpus (`scripts/tests/summary_comment_corpus/`) and runs
  seeded mutation fuzzing on every PR, checking each result against an
  independent re-derivation (the first valid entry per issue, at most twenty;
  the marker first; only the answer's report IDs, each linked only to its own
  dashboard page; no links, mentions, raw HTML or references outside code
  spans; no Report IDs heading). The dashboard's port of that renderer
  (`tools/diagnostics-collector/dashboard/summary.ts`, behind the owner's
  "Update GitHub now" button) is held to the Python module as its oracle:
  `tools/diagnostics-collector/summary.fuzz.test.ts` renders the same seed
  corpus and seeded text and value mutations of it on both sides (the Python
  side through `scripts/tests/summary_comment_oracle.py`) and requires
  identical comments, fingerprints and origin checks.
- The diagnostics ingest release-tag reader
  (`tools/diagnostics-collector/scripts/release_baseline.py`), which turns the
  GitHub `releases/latest` response into the tag the ingest job writes to
  `$GITHUB_OUTPUT` and passes to `/report`.
  `scripts/tests/test_release_baseline.py` replays its seed corpus
  (`scripts/tests/release_baseline_corpus/`) and runs seeded mutation fuzzing
  on every PR, checking each result against an independent oracle (a refusal,
  an empty tag for a draft or prerelease, or exactly the body's own
  `tag_name` when it is 1–64 characters of `[A-Za-z0-9._-]`).
- The image release-tag reader (`scripts/image_release_tags.py`), which
  turns a published release's `tag_name` into the registry tags
  `images-publish.yml` pushes. `scripts/tests/test_image_release_tags.py`
  replays its seed corpus (`scripts/tests/image_release_tags_corpus/`) and
  runs seeded mutation fuzzing on every PR, checking each result against an
  independent oracle (a refusal, or exactly `<major>.<minor>.<patch>`,
  `<major>.<minor>`, `<major>` and `latest` when the tag is `v` and three
  dot-separated ASCII numbers of one to nine digits without leading zeros).
- The collector's `/triage` admission (`admitBatch` in
  `tools/diagnostics-collector/triage.ts`), which parses the report-derived
  batch and prices it against the daily neuron budget.
  `tools/diagnostics-collector/triage-admit.fuzz.test.ts` replays its seed
  corpus (`tools/diagnostics-collector/corpus/triage_admit/`) and runs seeded
  mutation fuzzing on every PR, checking each result against an independent
  oracle (a refusal has a cause; an admitted prompt is the re-serialized
  batch after the Worker's own open-issue list when there is one, the batch
  fits the size cap and the prompt the context window, and the reservation
  covers the call's worst case).
- The collector's ingest cursor body (`parseCursor` in
  `tools/diagnostics-collector/ingest-cursor.ts`), which `POST /report/cursor`
  stores and which decides where the next ingest run starts.
  `tools/diagnostics-collector/ingest-cursor.fuzz.test.ts` replays its seed
  corpus (`tools/diagnostics-collector/corpus/ingest_cursor/`) and runs seeded
  mutation fuzzing on every PR, checking each result against an independent
  oracle (a cursor exactly when the body is a JSON object whose `since` is a
  safe non-negative integer and whose `after`, absent or null meaning empty,
  is a string of at most 1,024 characters with no control characters).
- The member dashboard's readers of untrusted text: the issue-body
  Report IDs parser (`parseIssueReportIds` in
  `tools/diagnostics-collector/dashboard/report-ids.ts`), which decides which
  reports an issue claims; the stored-report summary (`summarizeReport`
  in `dashboard/signature.ts`), which writes every report's index row; the
  report page's event log (`eventLog` in `dashboard/pages.ts`), which renders
  stored events; the session check (`sessionToken` and `memberFromRow` in
  `dashboard/session.ts`), which decides whether a request is signed in and
  as whom; the sign-in callback check (`parseCallback` in
  `dashboard/auth.ts`), which decides from the callback query and cookies
  whether a code is traded with GitHub and which page the member lands on;
  the return-page check (`safeNext`, same file), which turns a `next` query
  parameter or cookie part into a redirect target; the membership decision
  (`membershipDecision` in `dashboard/github.ts`), which decides from
  GitHub's membership answer whether sign-in grants member or owner access;
  the pull request decoder (`decodePullRequests`, same file), which
  decides from GitHub's GraphQL answer whether the sync drops an issue or
  replaces its pull request links; the issue listing decoder
  (`decodeIssuePage`, same file), which moves the sync cursor and feeds
  `storeIssue`; the sign-in and token decoders (`decodeInstallationId`,
  `decodeInstallationToken`, `decodeAccessToken`, `oauthErrorCode`,
  `decodeUser`, same file); the ingest Action's issue notice
  (`parseIssueNotify`, same file) and the single-issue decoder (`decodeIssue`,
  same file), which decide which issue is re-read and whether it stays linked;
  the ingest Action's summary receipt (`parseSummaryDone`, same file), which
  decides which issue stops being handed out for a summary comment; the
  ingest Action's attach body (`parseAttachRequest`, same file), which
  decides which reports the ledger links to which open issues as triage's;
  the issue comments decoder (`decodeComments`, same file), which names the
  comment the owner's "Update GitHub now" button edits in place or decides
  that one is posted, and the locked-issue reader (`isLockedMessage`, same
  file), which decides whether GitHub's refusal of that write is final; the
  ledger correction form (`parseLinkAction` in
  `dashboard/links.ts`), which decides which report a member attaches to,
  detaches from or moves between which issues, and which issue is merged
  into which; and the B2 listing decoder
  (`decodeListPage` in `dashboard/storage.ts`), which names the objects
  report viewing downloads and where reindexing continues. `tools/diagnostics-collector/dashboard-input.fuzz.test.ts` replays
  their seed corpora (`tools/diagnostics-collector/corpus/report_ids/`,
  `corpus/signature/`, `corpus/oauth_callback/`, `corpus/next_path/`, `corpus/session/`,
  `corpus/membership/`, `corpus/pull_requests/`, `corpus/b2_list/`,
  `corpus/issue_page/`, `corpus/github_auth/`, `corpus/issue_notify/`,
  `corpus/summary_done/`, `corpus/triage_attach/`, `corpus/issue/`, `corpus/issue_comments/`, `corpus/locked_message/`,
  `corpus/link_action/`) and runs seeded mutation
  fuzzing on every PR, checking results against independent oracles (IDs
  re-derived by a separate walk, all UUIDs present in the body, capped; level
  counts recounted, a signature exactly when an error or warning exists,
  bounded control-free fields; the event log renders one escaped row per
  event without throwing; a session token found exactly when a separate
  cookie walk finds a well-formed one, and a member only for a live row with
  a valid id and login, admin only when the row says so; a callback accepted exactly when a separate
  cookie walk finds a state that matches the query and well-formed code and
  verifier, returning those values and a return page that the URL parser
  resolves on the same origin to a dashboard page; a return page kept only
  when it is such a page, never refused when it is one; membership granted only for an active
  membership with the same user id, or a 404 for the owner itself, admin
  only when GitHub says so, and an error for any other failed status; pull
  requests decoded to exactly the well-formed ones in this repository, the
  label check failing only on a complete label list without `diagnostics`,
  and an error for GraphQL errors or a missing repository; B2 listings
  decoded to exactly the uploaded entries with string name and id, without
  throwing; issue pages decoded to the numbered non-PR entries with string
  fields, full by raw count, and the newest parseable update; tokens, ids
  and users accepted only when well-formed; an OAuth error code kept only
  when it is a short lowercase identifier; a notice accepted only as an
  object whose `issue` is an integer from 1 to 10^9; an attach body accepted
  only as a `links` list of at most twenty objects, each a distinct issue
  number in that range with a non-empty list of report UUIDs or collector
  object names, lowercased, a report kept under the first issue naming it
  and at most five hundred in all, re-derived by a separate walk; a single issue decoded
  only when it is a numbered non-PR issue, labeled exactly when a label is
  `diagnostics`; a merge commit kept only for a merged pull request with a
  40-character lowercase hex oid; a `fixed-in:<version>` label read only as
  a well-formed `MAJOR.MINOR.PATCH` with optional prerelease and build;
  releases decoded to the published non-draft entries with a plain tag name,
  oldest first; a compare answer read as containing only for `identical` or
  `behind`, as not containing only for `ahead` or `diverged`; a default
  branch kept only when it is a plain git branch name by rules written out
  one by one; a correction form accepted exactly when a separate walk over
  its pairs finds a known action, an issue number, a lowercase UUID where the
  action needs a report and a different issue number where it needs a
  target).
- The collector's B2 error-code reader (`b2ErrorCode` in
  `tools/diagnostics-collector/worker.ts`), which picks the `code` out of a B2
  error body for logs and authenticated responses.
  `tools/diagnostics-collector/b2-error.fuzz.test.ts` replays its seed corpus
  (`tools/diagnostics-collector/corpus/b2_error/`) and runs seeded mutation
  fuzzing on every PR, checking each result against an independent oracle
  (nothing, or exactly the body's own `code` when it is a 1–40 character
  lowercase identifier).
- The website Worker's `Accept-Encoding` parser (`pickEncoding` in
  `tools/site/worker.ts`), which picks the response's content coding.
  `tools/site/fuzz.test.ts` replays its seed corpus
  (`tools/site/fuzz/accept_encoding/`) and runs seeded mutation fuzzing in
  `npm test`, checking every choice against an independent oracle (RFC 9110
  qvalues, `*` for unlisted codings, Brotli before gzip on a tie, nothing at
  `q=0`).
- The website Worker's code-block highlighter (`highlight` in
  `tools/site/highlight.ts`), which colors fenced code from the docs
  repository. `tools/site/fuzz.test.ts` replays its seed corpus
  (`tools/site/fuzz/highlight/`) and runs seeded mutation fuzzing in
  `npm test` across its languages, checking with an independent reader that
  the output is only escaped text and `hl-` spans and gives back the input
  exactly.
- The website Worker's Mermaid diagrams (`diagram` and `cleanSvg` in
  `tools/site/diagram.ts`), which draw ```` ```mermaid ```` fences from the
  docs repository as inline SVG. `tools/site/fuzz.test.ts` replays its seed
  corpus (`tools/site/fuzz/diagram/`) and runs seeded mutation fuzzing in
  `npm test` on both the diagram sources and the SVG `cleanSvg` rebuilds,
  checking with an independent reader that the output is one `<svg>` of
  allowed elements, with no script or style attributes, no class but
  `mono`, no reference outside the diagram, and ids carrying the page's
  prefix.
- The website Worker's reader of the docs repository's `last-updated.json`
  (`pageDate` in `tools/site/last-updated.ts`), which dates each docs page.
  `tools/site/fuzz.test.ts` replays its seed corpus
  (`tools/site/fuzz/last_updated/`) and runs seeded mutation fuzzing in
  `npm test`, checking every answer against an independent oracle (the
  document's shape read through property descriptors, the date digit by
  digit against a days-in-month table) and that the date shown is only
  `Mon D, YYYY`.
- The hosted demo Worker's inputs (`tools/demo/src/routes.ts` and
  `src/inputs.ts`): the route check (`classify`), which decides whether a
  visitor's request may reach their container; the visitor's cookies
  (`readSessionToken`, `cookiesForContainer`); the container's `Set-Cookie`
  lines (`rewriteSetCookie`, `portalSessionFrom`) and JSON answers
  (`decodeConnections`, `decodeClaimTicket`, `decodeInstances`); the launch
  form (`decodeLaunchForm`); Turnstile's verdict (`siteverifyPassed`); the
  live status socket's handshake and messages (`src/live.ts`: `liveRequest`,
  `liveCommand`); and,
  at deploy time, the Cloudflare API answers `scripts/worker-secrets.ts`
  reads (`decodeApiResponse`, `findWidget`, `widgetKeys`). `tools/demo/test/inputs.fuzz.test.ts` replays their
  seed corpora (`tools/demo/corpus/route/`, `cookie/`, `set_cookie/`,
  `connections/`, `siteverify/`, `claim_ticket/`, `launch_form/`, `plugin_instances/`,
  `cf_api/`, `live_request/`, `live_message/`) and runs seeded mutation
  fuzzing in `npm test`, checking results against independent oracles (a
  forwarded path decodes to clean segments and matches no refused
  operation, a write matches a listed one and a sign-in start is always
  counted; the session cookie found exactly when a separate walk finds a
  well-formed `__Host-demo`, and no `__Host-` or `__Secure-` cookie
  forwarded; a rewritten `Set-Cookie` keeps its name, has no `Domain`, ends
  in exactly one `Secure` and no control characters; a portal session only
  when exactly one well-formed value is set; connections deduplicated,
  bounded and well-formed; Siteverify passed only for success on this host
  and action; a claim ticket only when well-formed; only wanted default
  plugin instances with well-formed ids, the first per plugin, on a
  well-formed node id; the launch form's token equal to a separate
  form-urlencoded parse's first `cf-turnstile-response` when that is
  printable ASCII, and none from an oversized body or token; a Cloudflare
  answer's `result` only on a 2xx with `success: true`, otherwise only its
  integer error codes, the demo widget only with a printable site key and
  printable, deduplicated, bounded hostnames, and keys only when both are
  printable; a live status socket only for a GET whose `Upgrade` is
  `websocket` in ASCII letters of either case, from this origin, with a
  session; and a command only for exactly the text `sync`).

Still to add (tracked in the fuzzing ADR's follow-ups): a full RPC session
against a scripted guest, the workerd `egress.js` gate checked against
`EgressPolicy` (#335), Audible CENC/DASH parsing, package extraction, naming
paths, node frames, media chapter math, backup import.

Seed corpora are checked in under `fuzz/corpus/<target>/` and replayed by
ordinary tests in PR CI (`tests/fuzz_corpus.rs` in the owning crate), so a
fixed crash stays fixed without a nightly toolchain. The planner routes a
corpus change to that crate's tests only (`relations.toml` `test_input`);
`scripts/tests/test_ci_contract.py` fails when a target lacks a corpus, a
planner route or a replay test, or when a fuzzed crate is missing from the
PR workflow's path filter. An oracle shared by a target and a replay test lives
in `fuzz/oracles/<target>.rs` and is included by both with `#[path]`, so the
two cannot drift; `relations.toml` lists the oracle beside the corpus, so an
oracle change runs that crate's tests only. `fuzz/dictionaries/<target>.dict` and
`<target>.options` (libFuzzer INI with a `[libfuzzer]` section whose `max_len`
matches the target's early-return cap)
ride beside each binary.

| Workflow | When | What |
| --- | --- | --- |
| `cflite-pr.yml` | PRs touching `fuzz/`, the fuzzed crates, or the ClusterFuzzLite config | 10 min, AddressSanitizer, code-change mode; fails on a new crash |
| `cflite-batch.yml` | daily + `workflow_dispatch` | 45 min batch fuzzing split across targets, private [crash triage](#fuzz-crash-triage), then corpus pruning and a coverage report (kept as Actions artifacts) |

A crash found by the scheduled run is handled as a security report: fix
first, then land the minimized input as `fuzz/corpus/<target>/regression_*`
in the fixing pull request.

Build config lives in `.clusterfuzzlite/` (`Dockerfile`, `build.sh`,
`project.yaml`). To add a target: a `[[bin]]` in `fuzz/Cargo.toml`, a seed
directory under `fuzz/corpus/`, a `test_input` route in
`scripts/ci_plan/relations.toml`, a replay test in the owning crate, the
crate on `cflite-pr.yml`'s path filter, a row above, and an entry in
`fuzz/boundaries.toml` ([triage](#fuzz-crash-triage)). `build.sh`
packages every target `cargo fuzz list` reports. Reproduce locally with
`cargo +nightly fuzz run <target>` from `fuzz/` (needs `capnp` on `PATH`).

## Fuzz-crash triage

Job logs and artifacts of this public repository are readable by anyone, and
the stock ClusterFuzzLite `run_fuzzers` action prints each crash's stack and
libFuzzer output (small crashing inputs included) to the log and uploads the
reproducer as an artifact. The scheduled workflow therefore never uses it:

1. `batch`, `prune` and `coverage` run the same pinned run-fuzzers image
   through `.github/actions/cflite-private-run`, which runs
   `scripts/fuzz-crash-triage.py run`: the image's own mode, with its output
   in a private log and its crash upload disabled. Corpus and coverage
   artifacts are unchanged, so PR fuzzing is too. The log shows a crash count.
   Timeout inputs are kept (`REPORT_TIMEOUTS`) so triage sees them. Every
   scheduled build sets `bad-build-check: false`, because that check runs
   each target briefly and prints its output to the log.
2. When `batch` kept crashes, it seals them with its log
   (`FUZZ_CRASH_BUNDLE_KEY`, `scripts/osv-remediation-bundle.sh`) as the
   `fuzz-crashes-<sanitizer>` artifact (7 days).
3. `triage` rebuilds the fuzzers from the same commit and runs
   `fuzz-crash-triage.py reproduce` in the same image: each crash is re-run
   (dropped unless it reproduces), minimized with `-minimize_crash`,
   classified (sanitizer error, panic, stack overflow, out of memory,
   timeout, leak, or a failed oracle: a panic raised from code under
   `fuzz/`) and deduplicated by target, kind and the top three frames of the
   code under test (a crash with no usable frames, such as a wall-clock
   timeout, is keyed by its input instead, so unrelated ones stay apart).
   The output is sealed as `fuzz-triage-<sanitizer>` (90 days).
4. `fuzz-crash-triage.py file`, with the [automation App](automation.md#automation-app)
   token, files each signature not already on record (a hidden
   `<!-- fuzz-crash-signature: … -->` marker in an open `fuzz-crash` issue
   or in any repository advisory):
   - **Draft security advisory** for every crash that is not an oracle
     failure, before the first release too (drafts are private, and filing
     them proves the path). The draft carries the stack, the reproducer
     (base64, up to 4 KiB) and a reproduce command; affected packages are the
     target's artifacts from `fuzz/boundaries.toml`, with a proposed range
     `<= <newest tag>` that a maintainer confirms (the crash reproduced on
     `main`, not on the release). Before the first release (prereleases
     count, drafts do not) no range is proposed. Default severity: high for memory-safety errors (and anything
     unrecognized; the advisory asks for the fix in a temporary private
     fork, which a maintainer starts, since that needs the Administration
     permission the App does not have), medium for a
     denial of service in host code, low inside a guest or for a leak. A
     maintainer reviews and publishes.
   - **Issue** for a failed oracle (a panic raised from `fuzz/fuzz_targets/`
     or `fuzz/oracles/`), which is a bug rather than a vulnerability: the
     issue names the target, its boundary and the sealed bundle. An issue
     closed as completed does not suppress a later identical crash (a
     regression); one closed as not planned or duplicate does.
   - A crash already on record as an advisory is not filed again. While that
     advisory is still a draft, a run that sees the crash again rewrites
     only the automated part of its description (a new reproducer, boundary
     or default severity mapping), the same way as the
     [OSV advisories](automation.md#osv-advisories) do; closed and published advisories
     are never touched.

`fuzz/boundaries.toml` maps every target to its crate, who controls its input,
whether it runs host-side, and which shipped artifacts contain it;
`scripts/tests/test_fuzz_crash_triage.py` fails when a target in
`fuzz/Cargo.toml` or in the table above lacks an entry, so adding a target
means adding one there too.
To read a bundle, download the artifact and run
`BUNDLE_KEY=<passphrase> scripts/osv-remediation-bundle.sh open sealed.bin <dir>`.
A new sanitizer needs a matrix entry in both jobs; a new
run-fuzzers image digest must be bumped in the action and in `triage`
together, and re-tested, because the wrapper patches one method of the
image's Python package (it refuses to fuzz when that method is missing).

The scheduled **workerd pin bump** job (`.github/workflows/workerd-pin-bump.yml`)
also compiles `bookclerk-workerd` / `bookclerk-plugin-abi`, so it installs
`capnproto` before `cargo build` when a pin bump is eligible.
