# Fuzzing

This page explains how Bookclerk fuzzes its untrusted-input boundaries: the
list of boundaries and their oracles, how the fuzz targets run in CI, how
to add one, and what happens when one finds a crash.

The rule that every boundary ships with a target is in
[ADR: Untrusted-input fuzzing](../adr/untrusted-input-fuzzing.md). The rest
of CI is in [Continuous integration](ci.md).

## Rust boundaries

Coverage-guided fuzzing runs on
[ClusterFuzzLite](https://google.github.io/clusterfuzzlite/) over the
`cargo-fuzz` targets in `fuzz/`. Every untrusted-input boundary ships a
target, a seed corpus and a replay test.

This table is the boundary list. A pull request that adds or widens a
boundary extends it.

| Target | Boundary | Crate |
| --- | --- | --- |
| `sql_parse` | SQL-v1 lexer, packer, grammar and TEXT admission | `bookclerk-plugin-abi` |
| `sql_lower` | Adapter lowering of canonical SQL | `bookclerk-db-exec` |
| `mp4_parse` | ISO-BMFF parse and remux of store downloads | `bookclerk-mp4` |
| `granted_http` | The granted reverse channel | `bookclerk-workerd::granted_http` |
| `egress_policy` | The guest egress allowlist | `bookclerk-plugin-manifest::EgressPolicy` |
| `plugin_manifest` | `plugin.toml` parse, validation, derived egress and consent | `bookclerk-plugin-manifest` |
| `abi_decode` | Every generated Cap'n Proto `read_*` codec and the database envelope decoders | `bookclerk-plugin-abi` |
| `abi_roundtrip` | Every generated Cap'n Proto `read_*` / `write_*` codec pair | `bookclerk-plugin-abi` |
| `guest_sql_authz` | Guest SQL authorization, every host-issued policy shape | `bookclerk-plugin-abi::GuestSqlPolicy` |
| `mux_peer` | Peer frames on the multiplexed byte link | `bookclerk-plugin-sdk::mux` |

Some boundaries need more than a cell:

- **`granted_http`** covers the request head, body framing, chunked bodies
  and the query string.
- **`egress_policy`** fuzzes the JSON wire form. The workerd `egress.js`
  gate is a separate implementation (see #335 in the
  [boundaries still to add](#still-to-add)).
- **`abi_decode`** goes through `read_any_message`, and
  **`abi_roundtrip`** through `roundtrip_any_message`.
- **`guest_sql_authz`** lowers each policy as the SQLite adapter lowers it.
- **`mux_peer`** covers `Open`, `Data`, `Close` and `Window` frames, their
  lengths and their ids.

### Oracles

Most targets check more than "no panic". `sql_parse` and `mp4_parse` check
only that. The others check:

- **`sql_lower`**: quoted strings and comments survive lowering verbatim,
  except for documented rewrites.
- **`granted_http`**: chunked decoding is split-invariant and agrees with
  an `httparse` reference decoder. The route body reader (framing dispatch,
  byte cap, end of stream) is split-invariant and yields what the declared
  framing promises.
- **`egress_policy`**: deny allows nothing. Loopback, private and metadata
  addresses need a CIDR in every spelling (trailing dot, fullwidth digits,
  legacy `127.1`, hex and octal IPv4). `*.suffix` never matches `xsuffix`,
  percent-encoded hosts never match, and the JSON round-trips.
- **`plugin_manifest`**: `format_manifest` round-trips, and a deny manifest
  yields a deny policy.
- **`abi_decode`**: the traversal budget is derived from the input size.
- **`abi_roundtrip`**: a value the decoder accepts re-encodes, re-decodes
  and compares equal (`read(write(v)) == v`).
- **`guest_sql_authz`**: every table, column, function and schema action
  that SQLite's own authorizer reports for an admitted statement is inside
  the policy. The shared oracle is `fuzz/oracles/guest_sql_authz.rs`,
  replayed from `bookclerk-library`.
- **`mux_peer`**: the link settles within a bound once the peer's bytes
  end, delivers no more than the peer sent, accepts no more streams than
  the peer could open, and leaves no task behind. The shared oracle is
  `fuzz/oracles/mux_peer.rs`, replayed from `bookclerk-plugin-sdk`.

## Boundaries outside the Rust workspace

These boundaries follow the ADR's rule for non-Rust tooling (decision 6).
Each one has a replay test and seeded mutation fuzzing, checked against
an independent oracle, and most replay a seed corpus. Python scripts run
them on every pull request; the Workers run them in `npm test`.

### Diagnostics scripts (Python)

The tests live in `scripts/tests/`, and each corpus beside them.

- **Holding issue editor**
  (`tools/diagnostics-collector/scripts/holding_issue.py`). It appends
  report IDs to the hidden report block in the body of the GitHub issue
  that holds untriaged reports, a body anyone with triage rights can edit.
  - Test and corpus: `test_holding_issue.py`, `holding_issue_corpus/`.
  - Oracle: an independent reading of the body, using the dashboard's
    block rule. Only a body whose last block's ID lines end on its `-->`
    line is edited. The new IDs follow the listed ones inside it, nothing
    else in the body changes, the list and body stay within their caps, and
    a second append changes nothing.
- **Triage plan validator**
  (`tools/diagnostics-collector/scripts/validate_plan.py`). It decides what
  model output reaches public issues.
  - Test and corpus: `test_diagnostics_plan.py`,
    `diagnostics_plan_corpus/`.
  - Oracle: no links, mentions, cross-references, raw HTML, comments, or
    forged Report IDs or report blocks. Only batch report IDs, listed bare
    in the one report block that ends the body, and at most five issues.
    Attachments go only to the issues the collector offered, with only
    batch report UUIDs, each report under one issue and in no new issue,
    and at most twenty issues.
- **Regression issue planner**
  (`tools/diagnostics-collector/scripts/regression_issue.py`). It turns the
  `regression_of` entries of an ingest pull into public issues. Their
  versions and platforms come from reports anyone can submit, and their
  titles from issues anyone with triage rights can edit.
  - Test and corpus: `test_regression_issue.py`,
    `regression_issue_corpus/`.
  - Oracle: an independent re-derivation of the groups. One issue per
    closed issue and `major.minor` line, at most ten. Only the pull's
    report IDs, listed bare in the one report block that ends the body. A
    marker line names the validated issue number. No links, mentions, raw
    HTML or references in the title and prose beyond the closed issue and
    its fix pull request, both validated integers.
  - The dashboard's Report IDs parser (`dashboard/report-ids.ts`, below)
    reads that marker.
- **Summary comment renderer**
  (`tools/diagnostics-collector/scripts/summary_comment.py`). It turns the
  collector's `GET /issues/summaries` answer (counts, plus versions and
  platforms taken from reports anyone can submit) into the one comment the
  ingest run keeps on each issue.
  - Test and corpus: `test_summary_comment.py`, `summary_comment_corpus/`.
  - Oracle: an independent re-derivation. The first valid entry per issue,
    at most twenty, with the marker first. Only the answer's report IDs,
    each linked only to its own dashboard page. No links, mentions, raw HTML
    or references outside code spans, and no Report IDs heading.
  - The dashboard's port of the renderer
    (`tools/diagnostics-collector/dashboard/summary.ts`, behind the owner's
    "Update GitHub now" button) is held to the Python module as its oracle.
    `tools/diagnostics-collector/summary.fuzz.test.ts` renders the same seed
    corpus, and seeded text and value mutations of it, on both sides (the
    Python side through `scripts/tests/summary_comment_oracle.py`). It
    requires identical comments, fingerprints and origin checks.
- **Ingest release-tag reader**
  (`tools/diagnostics-collector/scripts/release_baseline.py`). It turns the
  GitHub `releases/latest` response into the tag the ingest job writes to
  `$GITHUB_OUTPUT` and passes to `/report`.
  - Test and corpus: `test_release_baseline.py`,
    `release_baseline_corpus/`.
  - Oracle: a refusal, an empty tag for a draft or prerelease, or exactly
    the body's own `tag_name` when it is 1–64 characters of
    `[A-Za-z0-9._-]`.

### Docs publishing (Python)

- **Previous dates reader** (`previous_dates` in
  `scripts/docs-publish.py`). It reads the docs repository's current
  `last-updated.json`, which carries the generated pages' dates forward
  from one publish to the next.
  - Test and corpus: `scripts/tests/test_docs_publish.py`,
    `scripts/tests/docs_publish_dates_corpus/`.
  - Oracle: an independent reading of the document's shape by exact types,
    and of each date digit by digit against a days-in-month table.

### Image publishing (Python)

- **Release-tag reader** (`scripts/image_release_tags.py`). It turns a
  published release's `tag_name` into the registry tags
  `images-publish.yml` pushes.
  - Test and corpus: `scripts/tests/test_image_release_tags.py`,
    `scripts/tests/image_release_tags_corpus/`.
  - Oracle: a refusal, or exactly `<major>.<minor>.<patch>`,
    `<major>.<minor>`, `<major>` and `latest` when the tag is `v` and
    three dot-separated ASCII numbers of one to nine digits without
    leading zeros.

### CLA check (Python)

- **CLA checker** (`scripts/cla_check.py`). It reads the workflow event
  (including the head a `workflow_run` names), the pull request, its commit
  and comment pages, the co-author query answer, the lists of pull requests
  that contain the head commit or come from the head branch, and the
  signature file,
  and decides who still has to sign. Logins, comment bodies and commit
  metadata in them are chosen by contributors.
  - Test and corpus: `scripts/tests/test_cla_check.py`,
    `scripts/tests/cla_check_corpus/` (files named `<kind>__<case>`).
  - Oracle: an independent field-by-field reading of each document, which
    refuses any account whose login does not fit its type (`[bot]` logins
    only for bot accounts). For fuzzed commit pages, the unsigned accounts
    are exactly the pull request author and linked commit authors and
    co-authors, minus the allowlisted bots, the assistant accounts and the
    signed ids. The comment mentions only them and links only to the CLA.

### Diagnostics collector Worker

The tests and corpora live in `tools/diagnostics-collector/`.

- **`/triage` admission** (`admitBatch` in `triage.ts`). It parses the
  report-derived batch and prices it against the daily neuron budget.
  - Test and corpus: `triage-admit.fuzz.test.ts`, `corpus/triage_admit/`.
  - Oracle: a refusal has a cause. An admitted prompt is the re-serialized
    batch, after the Worker's own open-issue list when there is one. The
    batch fits the size cap and the prompt fits the context window, and the
    reservation covers the call's worst case.
- **Ingest cursor body** (`parseCursor` in `ingest-cursor.ts`). It is the
  body `POST /report/cursor` stores, and decides where the next ingest run
  starts.
  - Test and corpus: `ingest-cursor.fuzz.test.ts`,
    `corpus/ingest_cursor/`.
  - Oracle: a cursor exactly when the body is a JSON object whose `since`
    is a safe non-negative integer, and whose `after` (absent or null
    meaning empty) is a string of at most 1,024 characters with no control
    characters.
- **B2 error-code reader** (`b2ErrorCode` in `worker.ts`). It picks the
  `code` out of a B2 error body for logs and authenticated responses.
  - Test and corpus: `b2-error.fuzz.test.ts`, `corpus/b2_error/`.
  - Oracle: nothing, or exactly the body's own `code` when it is a 1–40
    character lowercase identifier.

### Member dashboard

The member dashboard's readers of untrusted text share one test,
`tools/diagnostics-collector/dashboard-input.fuzz.test.ts`. Each reader
below names the corpus it replays, under `tools/diagnostics-collector/`,
and the oracle its results are checked against.

In `dashboard/report-ids.ts`, `dashboard/signature.ts`,
`dashboard/pages.ts` and `dashboard/session.ts`:

- **Report IDs parser** (`parseIssueReportIds`) decides which reports an
  issue claims. Corpus `corpus/report_ids/`. Oracle: IDs re-derived by a
  separate walk, all UUIDs present in the body, capped.
- **Stored-report summary** (`summarizeReport`) writes every report's index
  row. Corpus `corpus/signature/`. Oracle: level counts recounted, a
  signature exactly when an error or warning exists, bounded control-free
  fields.
- **Event log** (`eventLog`) renders a report page's stored events. Corpus
  `corpus/signature/` (shared with the signature reader). Oracle: one row
  per event up to 500, level counts that add up to the rows, no event text
  left as markup, and no throw.
- **Session check** (`sessionToken` and `memberFromRow`) decides whether a
  request is signed in, and as whom. Corpus `corpus/session/`. Oracle: a
  session token found exactly when a separate cookie walk finds a
  well-formed one; a member only for a live row with a valid id and login;
  admin only when the row says so.

In `dashboard/auth.ts`:

- **Sign-in callback check** (`parseCallback`) decides, from the callback
  query and cookies, whether a code is traded with GitHub and which page
  the member lands on. Corpus `corpus/oauth_callback/`. Oracle: accepted
  exactly when a separate cookie walk finds a state that matches the query
  and a well-formed code and verifier. It returns those values, and a
  return page that the URL parser resolves on the same origin to a
  dashboard page.
- **Return-page check** (`safeNext`) turns a `next` query parameter or
  cookie part into a redirect target. Corpus `corpus/next_path/`. Oracle: a
  return page kept only when it is such a page, and never refused when it
  is one.

In `dashboard/github.ts`:

- **Membership decision** (`membershipDecision`) decides, from GitHub's
  membership answer, whether sign-in grants member or owner access. Corpus
  `corpus/membership/`. Oracle: granted only for an active membership with
  the same user id, or a 404 for the owner itself; admin only when GitHub
  says so; an error for any other failed status.
- **Pull request decoder** (`decodePullRequests`) decides, from GitHub's
  GraphQL answer, whether the sync drops an issue or replaces its pull
  request links. Corpus `corpus/pull_requests/`. Oracle: decoded to exactly
  the well-formed pull requests in this repository; the label check fails
  only on a complete label list without `diagnostics`; an error for GraphQL
  errors or a missing repository.
- **Issue listing decoder** (`decodeIssuePage`) moves the sync cursor and
  feeds `storeIssue`. Corpus `corpus/issue_page/`. Oracle: decoded to the
  numbered non-PR entries with string fields, full by raw count, with the
  newest parseable update.
- **Sign-in and token decoders** (`decodeInstallationId`,
  `decodeInstallationToken`, `decodeAccessToken`, `oauthErrorCode`,
  `decodeUser`). Corpus `corpus/github_auth/`. Oracle: tokens, ids and users
  accepted only when well-formed; an OAuth error code kept only when it is
  a short lowercase identifier.
- **Issue notice** (`parseIssueNotify`) and **single-issue decoder**
  (`decodeIssue`), from the ingest Action, decide which issue is re-read
  and whether it stays linked. Corpora `corpus/issue_notify/` and
  `corpus/issue/`. Oracle: a notice accepted only as an object whose
  `issue` is an integer from 1 to 10^9; a single issue decoded only when it
  is a numbered non-PR issue, labeled exactly when a label is
  `diagnostics`.
- **Summary receipt** (`parseSummaryDone`), from the ingest Action, decides
  which issue stops being handed out for a summary comment. Corpus
  `corpus/summary_done/`. Oracle: a separate walk over the parsed value,
  with the fingerprint checked character by character.
- **Attach body** (`parseAttachRequest`), from the ingest Action, decides
  which reports the ledger links to which open issues as triage's. Corpus
  `corpus/triage_attach/`. Oracle: accepted only as a `links` list of at
  most twenty objects, each a distinct issue number from 1 to 10^9 with a
  non-empty list of report UUIDs or collector object names, lowercased. A
  report is kept under the first issue naming it, at most five hundred in
  all, re-derived by a separate walk.
- **Issue comments decoder** (`decodeComments`) names the comment the
  owner's "Update GitHub now" button edits in place, or decides that one is
  posted. Corpus `corpus/issue_comments/`. Oracle: a hand walk of the
  first hundred entries that takes the first with a positive safe integer
  `id`, a `user` whose `type` is exactly `Bot`, and a `body` that begins
  with the marker.
- **Locked-issue reader** (`isLockedMessage`) decides whether GitHub's
  refusal of that write is final. Corpus `corpus/locked_message/`.
  Oracle: true only for an object whose `message` is a string of at most
  1000 characters containing "issue is locked" in any ASCII letter case,
  single-spaced, with no word character touching either end.

The same test also checks the release and fix-tracking readers against
these oracles:

- a merge commit kept only for a merged pull request with a 40-character
  lowercase hex oid;
- a `fixed-in:<version>` label read only as a well-formed
  `MAJOR.MINOR.PATCH`, with optional prerelease and build;
- releases decoded to the published non-draft entries with a plain tag
  name, oldest first;
- a compare answer read as containing only for `identical` or `behind`,
  and as not containing only for `ahead` or `diverged`;
- a default branch kept only when it is a plain git branch name, by rules
  written out one by one.

In `dashboard/links.ts` and `dashboard/storage.ts`:

- **Ledger correction form** (`parseLinkAction`) decides which report a
  member attaches to, detaches from or moves between which issues, and
  which issue is merged into which. Corpus `corpus/link_action/`. Oracle:
  accepted exactly when a separate walk over its pairs finds a known
  action, an issue number, a lowercase UUID where the action needs a
  report, and a different issue number where it needs a target.
- **B2 listing decoder** (`decodeListPage`) names the objects report
  viewing downloads, and where reindexing continues. Corpus
  `corpus/b2_list/`. Oracle: decoded to exactly the uploaded entries with a
  string name and id, without throwing.

### Website Worker

`tools/site/fuzz.test.ts` replays each corpus and runs seeded mutation
fuzzing in `npm test`.

- **`Accept-Encoding` parser** (`pickEncoding` in `tools/site/worker.ts`)
  picks the response's content coding. Corpus
  `tools/site/fuzz/accept_encoding/`. Oracle: RFC 9110 qvalues, `*` for
  unlisted codings, Brotli before gzip on a tie, nothing at `q=0`.
- **Page redirects** (`sitePageLocation` and `docsTarget` in
  `tools/site/worker.ts`) turn a request path into the page's one
  slash-free URL. Corpus `tools/site/fuzz/site_redirects/`. Oracle: the
  website moves exactly the slash and `.html` spellings of a listed page;
  a docs redirect stays under `/docs`, is slash-free, never repeats the
  request path and takes one hop, and docs keys never leave the tree.
- **Code-block highlighter** (`highlight` in `tools/site/highlight.ts`)
  colors fenced code from the docs repository. Corpus
  `tools/site/fuzz/highlight/`, fuzzed across its languages. Oracle: an
  independent reader confirms the output is only escaped text and `hl-`
  spans, and gives back the input exactly.
- **Mermaid diagrams** (`diagram` and `cleanSvg` in
  `tools/site/diagram.ts`) draw ```` ```mermaid ```` fences from the docs
  repository as inline SVG. Corpus `tools/site/fuzz/diagram/`, fuzzed on
  both the diagram sources and the SVG that `cleanSvg` rebuilds. Oracle:
  an independent reader confirms the output is one `<svg>` of allowed
  elements, with no script or style attributes, no class but `mono`, no
  reference outside the diagram, and ids carrying the page's prefix.
- **Web API layout** (`layoutOperations` in
  `tools/site/api-reference.ts`, through `renderDoc`) rearranges the
  generated `api/web/` pages from the docs repository into operation
  sections. Corpus `tools/site/fuzz/api_reference/`. Oracle: the HTML nests
  properly, every section is labelled by its own heading with one badge
  for a known method, and the page keeps the same text as it has without
  the layout.
- **Web API index layout** (`layoutIndex` and `indexRoutes` in
  `tools/site/api-reference.ts`, through `renderDoc`) lays out the
  generated `api/web/README.md` as route group cards with a quick-start
  aside, and reads its routes for the sidebar. Corpus
  `tools/site/fuzz/api_index/`. Oracle: the HTML nests properly, every card
  and aside is labelled by its own heading, every badge names a known
  method, and the page keeps the same text as it has without the layout.
- **Last-updated reader** (`pageDate` and `datedPages` in
  `tools/site/last-updated.ts`) dates each docs page from the docs
  repository's `last-updated.json`, and lists every page for
  `/sitemap.xml`. Corpus `tools/site/fuzz/last_updated/`. Oracle: the
  document's shape read through property descriptors, the date digit by
  digit against a days-in-month table, and each sitemap key segment by
  segment. The date shown is only `Mon D, YYYY`.

### Hosted demo Worker

`tools/demo/test/inputs.fuzz.test.ts` replays the corpora under
`tools/demo/corpus/` and runs seeded mutation fuzzing in `npm test`. The
inputs live in `tools/demo/src/routes.ts`, `src/inputs.ts` and
`src/live.ts`.

- **Route check** (`classify`) decides whether a visitor's request may
  reach their container. Corpus `route/`. Oracle: a forwarded path decodes
  to clean segments and matches no refused operation, a write matches a
  listed one, and a sign-in start is always counted.
- **Visitor cookies** (`readSessionToken`, `cookiesForContainer`). Corpus
  `cookie/`. Oracle: the session cookie is found exactly when a separate
  walk finds a well-formed `__Host-demo`, and no `__Host-` or `__Secure-`
  cookie is forwarded.
- **Container `Set-Cookie` lines** (`rewriteSetCookie`,
  `portalSessionFrom`). Corpus `set_cookie/`. Oracle: a rewritten
  `Set-Cookie` keeps its name, has no `Domain`, ends in exactly one
  `Secure` and has no control characters. A portal session only when
  exactly one well-formed value is set.
- **Container JSON answers** (`decodeConnections`, `decodeClaimTicket`,
  `decodeInstances`). Corpora `connections/`, `claim_ticket/`,
  `plugin_instances/`. Oracle: connections deduplicated, bounded and
  well-formed; a claim ticket only when well-formed; only wanted default
  plugin instances with well-formed ids, the first per plugin, on a
  well-formed node id.
- **Launch form** (`decodeLaunchForm`). Corpus `launch_form/`. Oracle: the
  token equals a separate form-urlencoded parse's first
  `cf-turnstile-response` when that is printable ASCII, and there is none
  from an oversized body or token.
- **Turnstile's verdict** (`siteverifyPassed`). Corpus `siteverify/`.
  Oracle: passed only for success on this host and action.
- **Live status socket** (`liveRequest`, `liveCommand`). Corpora
  `live_request/`, `live_message/`. Oracle: a socket only for a GET whose
  `Upgrade` is `websocket` in ASCII letters of either case, from this
  origin, with a session; a command only for exactly the text `sync`.
- **Cloudflare API answers at deploy time** (`decodeApiResponse`,
  `findWidget`, `widgetKeys` in `scripts/worker-secrets.ts`). Corpus
  `cf_api/`. Oracle: `result` only on a 2xx with `success: true`, otherwise
  only its integer error codes. The demo widget only with a printable site
  key and printable, deduplicated, bounded hostnames, and keys only when
  both are printable.

## Still to add

These are tracked in the fuzzing ADR's follow-ups:

- a full RPC session against a scripted guest;
- the workerd `egress.js` gate checked against `EgressPolicy` (#335);
- Audible CENC/DASH parsing;
- package extraction;
- naming paths;
- node frames;
- media chapter math;
- backup import.

## Corpora, oracles and replay

Seed corpora are checked in under `fuzz/corpus/<target>/`. Ordinary tests
replay them in pull-request CI (`tests/fuzz_corpus.rs` in the owning
crate), so a fixed crash stays fixed without a nightly toolchain.

- **Routing.** The planner routes a corpus change to that crate's tests
  only (`relations.toml` `test_input`).
- **Shared oracles.** An oracle shared by a target and a replay test lives
  in `fuzz/oracles/<target>.rs`. Both include it with `#[path]`, so the two
  can't drift. `relations.toml` lists the oracle beside the corpus, so an
  oracle change runs that crate's tests only.
- **Dictionaries and options.** `fuzz/dictionaries/<target>.dict` and
  `<target>.options` ride beside each binary. The options file is libFuzzer
  INI with a `[libfuzzer]` section whose `max_len` matches the target's
  early-return cap.

`scripts/tests/test_ci_contract.py` fails when a target lacks a corpus, a
planner route or a replay test, or when a fuzzed crate is missing from the
pull-request workflow's path filter.

## Workflows

| Workflow | When | What |
| --- | --- | --- |
| `cflite-pr.yml` | Pull requests touching `fuzz/`, the fuzzed crates, or the ClusterFuzzLite config | 10 minutes, AddressSanitizer, code-change mode; fails on a new crash |
| `cflite-batch.yml` | Daily, and `workflow_dispatch` | 45 minutes of batch fuzzing split across targets, then [crash triage](#fuzz-crash-triage), corpus pruning and a coverage report |

The batch run's triage is private, and its corpus and coverage report are
kept as Actions artifacts.

> [!IMPORTANT]
> A crash found by the scheduled run is handled as a security report,
> unless it is a failed oracle, which is filed as an issue. Either way, fix
> it first, then land its reproducer as `fuzz/corpus/<target>/regression_*`
> in the fixing pull request.

The build config lives in `.clusterfuzzlite/` (`Dockerfile`, `build.sh`,
`project.yaml`). `build.sh` packages every target `cargo fuzz list`
reports.

The scheduled **workerd pin bump** job
(`.github/workflows/workerd-pin-bump.yml`) also compiles
`bookclerk-workerd` and `bookclerk-plugin-abi`, so it installs `capnproto`
before `cargo build` when a pin bump is eligible.

## Add a target

1. Add a `[[bin]]` to `fuzz/Cargo.toml`.
2. Add a seed folder under `fuzz/corpus/`.
3. Add a `test_input` route in `scripts/ci_plan/relations.toml`.
4. Add a replay test in the owning crate.
5. Put the crate on `cflite-pr.yml`'s path filter.
6. Add a row to the [boundary table](#rust-boundaries).
7. Add an entry to `fuzz/boundaries.toml` (see
   [triage](#fuzz-crash-triage)).

To reproduce locally, run this from `fuzz/` (it needs `capnp` on `PATH`):

```sh
cargo +nightly fuzz run <target>
```

## Fuzz-crash triage

Anyone can read the job logs and artifacts of this public repository. The
stock ClusterFuzzLite `run_fuzzers` action prints each crash's stack and
libFuzzer output (small crashing inputs included) to the log, and uploads
the reproducer as an artifact. So the scheduled workflow never uses it.
Instead:

1. **Fuzz privately.** `batch`, `prune` and `coverage` run the same pinned
   run-fuzzers image through `.github/actions/cflite-private-run`, which
   runs `scripts/fuzz-crash-triage.py run`. That is the image's own mode,
   with its output in a private log and its crash upload disabled.
   - Corpus and coverage artifacts are unchanged, and so is pull-request
     fuzzing. The log shows a crash count.
   - Timeout inputs are kept (`REPORT_TIMEOUTS`), so triage sees them.
   - Every scheduled build sets `bad-build-check: false`, because that
     check runs each target briefly and prints its output to the log.
2. **Seal the crashes.** When `batch` kept crashes, it seals them with its
   log (`FUZZ_CRASH_BUNDLE_KEY`, `scripts/osv-remediation-bundle.sh`) as
   the `fuzz-crashes-<sanitizer>` artifact, kept 7 days.
3. **Reproduce and classify.** `triage` rebuilds the fuzzers from the same
   commit and runs `fuzz-crash-triage.py reproduce` in the same image. Each
   crash is:
   - re-run, and dropped unless it reproduces;
   - minimized with `-minimize_crash`, except a timeout, out of memory or
     leak, which keeps its original input;
   - classified as a sanitizer error, panic, stack overflow, out of memory,
     timeout, leak, fatal signal, or unknown;
   - marked as a failed oracle when the panic is raised from code under
     `fuzz/`, which decides how it is filed;
   - deduplicated by target, kind and the top three frames of the code
     under test. A crash with no usable frames, such as a wall-clock
     timeout, is keyed by its input instead, so unrelated ones stay apart.

   The output is sealed as `fuzz-triage-<sanitizer>`, kept 90 days.
4. **File.** `fuzz-crash-triage.py file`, with the
   [automation App](automation.md#automation-app) token, files each
   signature not already on record. A signature is on record when a hidden
   `<!-- fuzz-crash-signature: … -->` marker is in any repository
   advisory, or in a `fuzz-crash` issue that is open or was closed as not
   planned or duplicate. See [What gets filed](#what-gets-filed).

### What gets filed

- **A draft security advisory** for every crash that isn't an oracle
  failure, before the first release too. Drafts are private, and filing
  them proves the path. A maintainer reviews and publishes.
- **An issue** for a failed oracle (a panic raised from
  `fuzz/fuzz_targets/` or `fuzz/oracles/`). That is a bug rather than a
  vulnerability. The issue names the target, its boundary and the sealed
  bundle.
- **Nothing new** for a crash already on record as an advisory.

The draft advisory carries the stack, the reproducer (base64, up to
4 KiB) and a reproduce command.

- **Affected packages** are the target's artifacts from
  `fuzz/boundaries.toml`, with a proposed range `<= <newest tag>` that a
  maintainer confirms, because the crash reproduced on `main`, not on the
  release.
- **Before the first release** no range is proposed. Prereleases count as
  releases; drafts don't.
- **Default severity** is high for memory-safety errors on either side, a
  fatal signal, and anything unrecognized. A denial of service (panic,
  stack overflow, out of memory, timeout) is medium in host code and low
  inside a guest. A leak is low.

> [!NOTE]
> A high-severity advisory asks for the fix in a temporary private fork.
> A maintainer starts that fork, because it needs the Administration
> permission the App doesn't have.

An issue closed as completed doesn't suppress a later identical crash,
because that is a regression. One closed as not planned or duplicate does.

While an advisory on record is still a draft, a run that sees the crash
again rewrites only the automated part of its description: a new
reproducer, boundary or default severity mapping. That is the same way the
[OSV advisories](automation.md#osv-advisories) work. Closed and published
advisories are never touched.

### Boundaries file and bundles

`fuzz/boundaries.toml` maps every target to its crate, who controls its
input, whether it runs host-side, and which shipped artifacts contain it.
`scripts/tests/test_fuzz_crash_triage.py` fails when a target in
`fuzz/Cargo.toml` or in the [boundary table](#rust-boundaries) lacks an
entry. So adding a target means adding one there too.

To read a bundle, download the artifact and run:

```sh
BUNDLE_KEY=<passphrase> scripts/osv-remediation-bundle.sh open sealed.bin <dir>
```

Replace `<passphrase>` with the bundle key maintainers keep, and `<dir>`
with a new folder to open the bundle into. `sealed.bin` is the file the
artifact download writes.

- A new sanitizer needs a matrix entry in both jobs.
- A new run-fuzzers image digest must be bumped in the action and in
  `triage` together, and re-tested. The wrapper patches one method of the
  image's Python package, and refuses to fuzz when that method is missing.

## Next steps

- [ADR: Untrusted-input fuzzing](../adr/untrusted-input-fuzzing.md): why
  every boundary is fuzzed.
- [Continuous integration](ci.md): how replay tests are planned.
- [Repository automation](automation.md): the App and the advisory
  workflows triage shares.
