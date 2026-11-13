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
| `workerd_control` | Lines on workerd's stdout, shared with plugin isolates, that report the bridge RPC port on Windows (`--control-fd`) | `bookclerk-workerd::control` |
| `egress_policy` | The guest egress allowlist | `bookclerk-plugin-manifest::EgressPolicy` |
| `plugin_manifest` | `plugin.toml` parse, validation, derived egress and consent | `bookclerk-plugin-manifest` |
| `cron_schedule` | `[triggers] crons` parse and fire-time search | `bookclerk-plugin-manifest::CronSchedule` |
| `https_url_policy` | URLs from untrusted output checked against a host allowlist (no caller since covers moved to the cover service) | `bookclerk-plugin-manifest::https_url_on_hosts` |
| `abi_decode` | Every generated Cap'n Proto `read_*` codec and the database envelope decoders | `bookclerk-plugin-abi` |
| `db_stream` | `executeStreamed` frames an adapter sends, reassembled by the host and the SDKs under the streamed result budget. The TypeScript and Python SDK tests replay the same corpus as shared vectors against their own decoders | `bookclerk-plugin-abi::ExecuteStreamDecoder` |
| `cover_sink_key` | The key a cover fetcher writes each image under, mapped to a URL slot | `bookclerk-plugin-abi::cover_sink_slot` |
| `cover_process` | Cover images from any public website: sniff, decode under limits, resize, re-encode | `bookclerk-cover::process` |
| `cover_token` | Cover tokens in `/api/covers` and `/api/images` paths, and Gravatar hashes in `/api/avatars` paths: which remote URL a route may fetch | `bookclerk-cover::token` |
| `cover_wire` | Cover worker frames: requests the worker reads, replies the host reads | `bookclerk-cover::wire` |
| `cache_headers` | `If-None-Match` on image requests, which decides a `304` answer, and an image host's caching headers, which decide whether and how long a fetched image is kept | `bookclerk-cover::http_cache` |
| `abi_roundtrip` | Every generated Cap'n Proto `read_*` / `write_*` codec pair | `bookclerk-plugin-abi` |
| `scan_page` | One page of a storefront's library scan: its size caps, the cursor it hands back, the account it names and the refreshed credentials it carries | `bookclerk-plugin-abi::validate_scan_page` |
| `event_read` | A plugin's request to read past events (`EVENTS.read`): the `/events/read` JSON body, its cursor, the Cap'n Proto request and page codecs, and the consent, page and time bounds | `bookclerk-plugin-abi::validate_event_read_request` |
| `guest_sql_authz` | Guest SQL authorization, every host-issued policy shape | `bookclerk-plugin-abi::GuestSqlPolicy` |
| `mux_peer` | Peer frames on the multiplexed byte link | `bookclerk-plugin-sdk::mux` |
| `library_resolve` | Store listings resolved into works, expressions and editions | `bookclerk-resolve` |
| `node_frame` | Node-to-node frames on the cluster socket | `bookclerk-node-proto::Frame` |
| `node_account_bundle` | Store accounts and their secrets a library sends a joining branch for its person, and a branch offers its library (`accounts.add`) | `bookclerk-node-proto::parse_account_bundle` |
| `cache_settings` | `cache.*` values from the settings API and CLI: the image caches' budget and unviewed age | `bookclerk-config::parse_cache_setting` |
| `libro_catalog` | Libro.fm catalog search pages and audiobook details, which the `libro` guest turns into Discover records, purchase links and prices | `bookclerk-plugin-source-libro` |
| `graphicaudio_catalog` | GraphicAudio product pages, which the `graphicaudio` guest turns into catalog records (credits, synopsis, ISBN, running time, price) for title dialogs, the listing pool and the demo seed | `bookclerk-plugin-source-graphicaudio::parse_product_detail` |
| `plan_authz` | Scan and acquire requests a Member sends (over HTTP or through their Host), authorized against the store accounts linked to them | `bookclerk-library::control_plane::plan_scan` / `plan_acquire` |
| `client_authorization` | A client app's library card, the `Authorization: Bookclerk-Client <token>` header value that signs a request in as a person | `bookclerk-node-proto::parse_client_authorization` |
| `client_authorize` | A client app's authorization request, the `/oidc/authorize` query that decides whether the library shows its library card consent page and where it sends the code | `bookclerk-node-proto::parse_client_query`, `client_request_problem` |
| `client_token_request` | A library card's token exchange, the body of `POST /api/client/v1/token` and its match against the code the library stored | `bookclerk-node-proto::parse_client_token_request`, `client_exchange_matches` |
| `job_cap_request` | A branch's job cap for one library, the body of `POST /api/config/join/capacity` | `bookclerk-library::control_plane::parse_job_cap_request` |
| `sharing_request` | A branch's sharing and work settings for one library, the body of `POST /api/config/join/sharing` | `bookclerk-library::control_plane::parse_sharing_request` |
| `branch_request` | An interlibrary request, the body of `POST /api/branch-requests` | `bookclerk-library::control_plane::parse_branch_request` |
| `connections_request` | A branch Owner's store connections to add to one library, or one to disconnect: the bodies of `POST /api/config/join/connections` and `POST /api/config/join/connections/disconnect` | `bookclerk-library::control_plane::parse_add_connections_request`, `parse_disconnect_request` |
| `window_policy` | Requests a page sends on the desktop window's custom scheme, which reach the router with the operator token unless refused, and the URLs the window is asked to open | `bookclerk-window-policy` |

Some boundaries need more than a cell:

- **`granted_http`** covers the request head, body framing, chunked bodies
  and the query string.
- **`egress_policy`** fuzzes the JSON wire form. The workerd `egress.js`
  gate is a separate implementation: its User-Agent rule is fuzzed in
  JavaScript ([egress bridge User-Agent rule](#egress-bridge-user-agent-rule)),
  the rest is still to add (#335).
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
- **`workerd_control`**: a port the parser accepts agrees with an
  independent `serde_json::Value` reading, every listen object that reading
  accepts is accepted once re-encoded canonically, and no structured log
  line (the input wrapped as a `console.*` message) yields a port. The
  shared oracle is `fuzz/oracles/workerd_control.rs`, replayed from
  `bookclerk-workerd`.
- **`egress_policy`**: deny allows nothing. Loopback, private and metadata
  addresses need a CIDR in every spelling (trailing dot, fullwidth digits,
  legacy `127.1`, hex and octal IPv4). `*.suffix` never matches `xsuffix`,
  `*` never matches an IP literal and never grants a socket,
  percent-encoded hosts never match, and the JSON round-trips. The
  guest's own `User-Agent` is kept only on hosts an own-agent pattern
  matches, never through a `*` entry however it is spelled, and every
  other request gets the host's agent.
- **`plugin_manifest`**: `format_manifest` round-trips, and a deny manifest
  yields a deny policy.
- **`cron_schedule`**: an accepted schedule round-trips through its
  normalized source, and `next_after` / `latest_at_or_before` agree with a
  minute-by-minute scan of `matches`. The shared oracle is
  `fuzz/oracles/cron_schedule.rs`, replayed from
  `bookclerk-plugin-manifest`.
- **`https_url_policy`**: the check is idempotent, and an accepted URL is
  `https://` with no user info, port or IP literal, on a host the patterns
  cover, read byte by byte rather than by the URL parser. The shared oracle
  is `fuzz/oracles/https_url_policy.rs`, replayed from
  `bookclerk-plugin-manifest`.
- **`abi_decode`**: the traversal budget is derived from the input size.
  Structured seeds cover the book records, the metadata replies and the
  cover request and reply (`structured_*.bin`), plus the typed
  `limit_exceeded` envelopes (`limit_error_reply.bin`, `caps_reply_*.bin`).
- **`db_stream`**: decoding a frame stream in one push and in chunks of
  any size agree (both the same reply, or both refused), an accepted
  reply has no statement over the row cap, and it re-encodes to frames
  that decode to the same reply. A streaming decoder (the one the Rust
  SDK's `iterate()` reads with, which keeps no rows) fed the same chunks
  accepts exactly what one push accepts, hands out every row of it once
  and in order, and finishes with the same reply minus its rows. The
  shared oracle is
  `fuzz/oracles/db_stream.rs`, replayed from `bookclerk-plugin-abi`. The
  TypeScript and Python SDK decoders replay the same seeds and, on every
  run, a fixed set of mutants derived from them with a seeded generator
  (a flipped byte, a truncation, an inserted byte, a duplicated slice),
  holding the first two invariants and typing every refusal.
- **`cover_sink_key`**: a key names a slot exactly when it is the
  canonical decimal spelling of an index below the URL count, read digit
  by digit without `str::parse`, so no two keys name one slot. The shared
  oracle is `fuzz/oracles/cover_sink_key.rs`, replayed from
  `bookclerk-plugin-abi`.
- **`cover_process`**: an input without a JPEG, PNG or WebP signature is
  refused. An accepted cover yields three JPEGs whose markers and frame
  headers, read byte by byte rather than by the `image` crate, show 4:4:4
  sampling, the reported size for the full image, and edges within 2400,
  360 and 128 px that never grow from one size to the next. The shared
  oracle is `fuzz/oracles/cover_process.rs`, replayed from
  `bookclerk-cover`, whose replay test also regenerates the image seeds.
- **`cover_token`**: a token opens only when an independent HMAC-SHA256
  re-signing of its URL reproduces it byte for byte, and the URL is `https`
  with a host; a signed URL round-trips, never opens under another key, and
  a one-byte change is refused. The same input as a Gravatar hash names a
  URL only when it is exactly 64 lowercase hex digits, checked byte by
  byte, and that URL is the hash's picture on `www.gravatar.com` with
  nothing else in it. The shared oracle is `fuzz/oracles/cover_token.rs`,
  replayed from `bookclerk-cover`.
- **`cache_settings`**: the first byte picks the key. A value is accepted
  exactly when, after ASCII whitespace is trimmed, it is one or more ASCII
  digits whose number, read digit by digit with overflow detection, lies in
  the key's documented range, and the parser returns that number. The
  shared oracle is `fuzz/oracles/cache_settings.rs`, replayed from
  `bookclerk-config`.
- **`cover_wire`**: the first byte picks the request or the reply decoder.
  An accepted frame re-encodes to exactly its own bytes, and an accepted
  reply carries only JPEG-signed images and a nonzero size. The shared
  oracle is `fuzz/oracles/cover_wire.rs`, replayed from `bookclerk-cover`.
- **`cache_headers`**: the first byte, modulo 4, picks a mode. A built
  `If-None-Match` list of well-formed tags matches exactly when one of
  them is the current tag, strong or weak. Any raw header that matches,
  other than `*`, contains the quoted tag, and padding it with whitespace
  never changes the answer. A built `Cache-Control` list (with `Age`) gets
  the storage decision and freshness lifetime the oracle works out on its
  own. Any raw set of upstream headers gives a lifetime between the floor
  and ceiling, is refused storage only when `no-store` appears, and keeps
  an `ETag` only as the trimmed header. The shared oracle is `fuzz/oracles/cache_headers.rs`,
  replayed from `bookclerk-cover`.
- **`abi_roundtrip`**: a value the decoder accepts re-encodes, re-decodes
  and compares equal (`read(write(v)) == v`).
- **`scan_page`**: the input splits into the cursor the host sent, the
  account it asked for and a `ScanPage` message. The host accepts the page
  exactly when an independent check of every rule agrees: at most
  `MAX_SCAN_PAGE_BOOKS` books and as many removed ids, a cursor and token
  within `MAX_SCAN_CURSOR_BYTES`, a next cursor that is empty or differs
  from the one sent, books for that account only, no blank product or
  removed id, and refreshed credentials that are absent, empty, or one
  JSON object within `MAX_REFRESHED_CREDENTIAL_BYTES` (the same
  `validate_refreshed_credentials` check the host runs on a
  `PlainFetch` reply's refreshed credentials). An accepted
  page round-trips through the codec and is accepted again. The shared
  oracle is `fuzz/oracles/scan_page.rs`, replayed from
  `bookclerk-plugin-abi`.
- **`event_read`**: byte 0 modulo 4 picks a `/events/read` JSON body (0),
  a bare cursor (1), or a Cap'n Proto `EventReadRequest` (2) or
  `EventReadPage` (3). The body parser accepts exactly the JSON objects whose
  keys are request fields, and an accepted request round-trips through
  JSON. Validation against a fixed consent agrees with an independent
  restatement of every rule: the error code, or the resolved start, types
  and limit. The cursor decoder accepts exactly the canonical cursors a
  hand-written decoder accepts, and an accepted cursor re-encodes to the
  same text. A Cap'n Proto request or page that the RPC codec accepts
  survives `write` then `read` unchanged and stays inside the cursor and
  list bounds. The shared oracle is `fuzz/oracles/event_read.rs`, replayed
  from `bookclerk-plugin-abi`.
- **`guest_sql_authz`**: every table, column, function and schema action
  that SQLite's own authorizer reports for an admitted statement is inside
  the policy. The shared oracle is `fuzz/oracles/guest_sql_authz.rs`,
  replayed from `bookclerk-library`.
- **`mux_peer`**: the link settles within a bound once the peer's bytes
  end, delivers no more than the peer sent, accepts no more streams than
  the peer could open, and leaves no task behind. The shared oracle is
  `fuzz/oracles/mux_peer.rs`, replayed from `bookclerk-plugin-sdk`.
- **`library_resolve`**: decodes up to 64 listings from small pools of
  titles, authors and identifiers, so they collide, and records each one
  into an in-memory catalog, either held or as a catalog listing (a store
  answer nobody holds). A catalog listing is placed exactly where a scan
  holding it would be (same listing, edition, link and work reason as the
  held plan on the same rows), passes the same placement checks as a held
  listing, never gains a holding and never announces a holding or its own
  new link. After every step one strong identifier maps
  to one edition; a weak identifier alone never joins editions of
  another media kind, work kind or an incompatible expression; strong
  identifiers naming different editions are never merged and the conflict
  is recorded; a work found by identifier or text match has the listing's
  kind, and an episode's work shares its show. Resolving any listing
  again changes nothing, except that an episode recorded before its show
  gains the show once, and a work stored without a matcher key gains one
  once no other work holds it. The oracle is
  `bookclerk_resolve::fuzzing::run_case`, replayed from `bookclerk-resolve`.
- **`node_frame`**: an accepted frame is within the size limit and the
  frame version, and serializes and parses back to an equal frame (unless
  its serialized text grows past the limit, as `1e9` written out does). Every
  library id in `acquire.targets` is 1 to 16 ASCII letters and digits,
  checked byte by byte. A `catalog.ask` or `catalog.found` that passes
  its validator names at most 100 works, each once, by plain correlation
  keys; its identifiers are plain ASCII with a marketplace exactly on an
  ASIN, and each answer lists 1 to 8 distinct editions with a title. A
  `requests.claimed` that passes its validator holds at most 16 requests,
  each job once, with a valid work and a plain store and product id, and
  a `requests.finish` that passes its validator names a plain job id
  and a positive generation. A
  `window.page` that passes its validator carries at most 100 works, each
  once, with plain identifiers, 1 to 8 distinct owned editions with a
  title, and a next cursor past the one asked with. The shared oracle is `fuzz/oracles/node_frame.rs`,
  replayed from `bookclerk-node-proto`.
- **`node_account_bundle`**: an accepted bundle is within the size limit,
  has the bundle version and names the cluster and node it was opened
  for. It carries at most the account and secret limits; every text field
  is non-empty, within its limit, free of control characters and
  surrounding whitespace; every secret is 1 byte to its limit; no account,
  and no secret within an account, repeats. These are checked character by
  character in the oracle, not through the parser's own rule. An accepted
  bundle serializes and parses back to an equal bundle. The shared oracle
  is `fuzz/oracles/node_account_bundle.rs`, replayed from
  `bookclerk-node-proto`.
- **`plan_authz`**: decodes a small library (store accounts on two plugins
  whose ids and labels collide, who each is linked to, library rows and
  plugin instances) and one Member's scan or acquire request into an
  in-memory database, then plans it. Every planned job names an account,
  and every account its needle reaches at run time (id or label, ignoring
  ASCII case, within the pinned instance's plugin) is linked to that
  Member, checked against the oracle's own model rather than the planner's
  queries. A refusal is `Forbidden` or `Invalid`. The shared oracle is
  `fuzz/oracles/plan_authz.rs`, replayed from `bookclerk-library`.
- **`client_authorization`**: reads the header value byte by byte. It
  yields a token exactly when it is the scheme `Bookclerk-Client` in any
  ASCII case, one space, and 43 characters from `A-Z`, `a-z`, `0-9`, `-`
  and `_`, with nothing else, and the token is those 43 bytes. The shared
  oracle is `fuzz/oracles/client_authorization.rs`, replayed from
  `bookclerk-node-proto`.
- **`client_authorize`**: reads the query as form-encoded pairs. It
  parses exactly when no parameter repeats and `client_id`,
  `redirect_uri`, `response_type` and `code_challenge` are present, and
  each field is the decoded value sent. It is accepted exactly when the
  client is `bookclerk-client`, the response type is `code`, the
  challenge is S256 with 43 base64url characters, the scope is
  `library:member`, the redirect URI is `https` with a host or `http` on
  a loopback address or `localhost` (no user info, no fragment), the
  client name is 1 to 64 bytes, not blank and without control
  characters, and any state is at most 512 bytes without control
  characters. The shared oracle is `fuzz/oracles/client_authorize.rs`,
  replayed from `bookclerk-node-proto`.
- **`client_token_request`**: the first byte picks the stored code (the
  client app's, the branch client's, or one with the `plain` method) and
  the rest is the body. The body parses exactly when it is one JSON
  object with no repeated keys and exactly the string keys `code`,
  `code_verifier` and `redirect_uri`, and each field is the value sent.
  The exchange matches exactly when the code is the client app's, the
  method is S256, the redirect URI is the stored one, and the trimmed
  verifier is not empty and hashes to the stored challenge (checked
  against RFC 7636's example pair). The shared oracle is
  `fuzz/oracles/client_token_request.rs`, replayed from
  `bookclerk-node-proto`.
- **`job_cap_request`**: reads the body as an untyped JSON value. It is
  accepted exactly when it is one object with no repeated keys and no keys
  besides `cluster_id` (a string) and `max_jobs` (`null` or a whole number
  from 0 to 16), both present; the parser then returns the trimmed id and
  that cap, and otherwise refuses it as `Invalid`. The shared oracle is
  `fuzz/oracles/job_cap_request.rs`, replayed from `bookclerk-library`.
- **`branch_request`**: reads the body as an untyped JSON value. It is
  accepted exactly when it is one object with no repeated keys and no keys
  besides `work_id` (a string, required) and `node_id` (absent, `null` or
  a string), where the trimmed `work_id` is 1 to 64 ASCII letters, digits
  and `-`, and a trimmed `node_id` is 1 to 58 lowercase ASCII letters,
  digits, `.`, `_` and `-`. The parser then returns those trimmed ids,
  and otherwise refuses it as `Invalid`. The shared oracle is
  `fuzz/oracles/branch_request.rs`, replayed from `bookclerk-library`.
- **`sharing_request`**: reads the body as an untyped JSON value. It is
  accepted exactly when it is one object with no repeated keys and no keys
  besides `cluster_id`, `share_books`, `include_branches`,
  `list_members_books`, `work_for_members`, `keep_member_copies` and
  `download`, with a string `cluster_id` and booleans for `share_books`
  and `include_branches`, and each of the other four absent, `null` or a
  boolean. The parser then returns the trimmed id and those values, with
  an absent or `null` setting left unset (kept as stored) rather than read
  as `false`, and otherwise refuses it as `Invalid`. The shared oracle is
  `fuzz/oracles/sharing_request.rs`, replayed from `bookclerk-library`.
- **`connections_request`**: feeds each input to both parsers and reads it
  as an untyped JSON value. An add body is accepted exactly when it is one
  object with no repeated keys anywhere and only `cluster_id` (a string)
  and `accounts`, an array of 1 to 64 objects that each hold exactly a
  string `plugin_key` and a string `account_id`; the parser then returns
  the trimmed id and the accounts as sent. A disconnect body is accepted
  exactly when it is one object with no repeated keys, string
  `plugin_key` and `account_id`, and `everywhere` absent (read as `false`)
  or a boolean, and nothing else. Anything else is refused as `Invalid`.
  The shared oracle is `fuzz/oracles/connections_request.rs`, replayed
  from `bookclerk-library`.
- **`window_policy`**: restates the policy from the bytes. A request is
  refused as foreign exactly when its `Origin` is not byte for byte one of
  the two app origins, or when it has no `Origin`, is a write, and its
  `Referer` is not a URL on an app origin; otherwise it is refused exactly
  when its path is the node API or the live socket, by whole segments. The
  headers the router sees never hold the page's cookies, credentials,
  `Referer` or forwarding headers, always hold one `Host: localhost` and
  one `Origin: http://localhost`, carry `Bearer <token>` only when a token
  was given, and keep every other header unchanged. Navigation allows app
  pages always and, only while a join is open, `https` and loopback `http`
  sign-in pages, with the return to the join callback kept on its path. The
  shared oracle is `fuzz/oracles/window_policy.rs`, replayed from
  `bookclerk-window-policy`.

## Boundaries outside the Rust workspace

These boundaries follow the ADR's rule for non-Rust tooling (decision 6).
Each one has a replay test and seeded mutation fuzzing, checked against
an independent oracle, and most replay a seed corpus. Python scripts run
them on every pull request; the Workers run them in `npm test`, inside the
Workers runtime (Vitest with `@cloudflare/vitest-plugin`), which reads each
corpus through Vite rather than from disk. The collector's summary fuzz test
runs in Node instead, since its oracle is a Python subprocess.

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
    of each date digit by digit against a days-in-month table, and of an
    optional `THH:MM:SSZ` time field by field.

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

### Demo deploy (Python)

- **Registry and health readers** (`manifest_digest` and `live_version` in
  `scripts/demo_image.py`). `demo-deploy.yml` reads the registry's
  manifest for the published demo image and the live demo's
  `/_demo/health` answer. The digest becomes the image the public demo
  runs (after its attestation is verified), and the live commit decides
  whether a deploy happens at all.
  - Test and corpus: `scripts/tests/test_demo_image.py`,
    `scripts/tests/demo_image_corpus/` (manifests under `digest/`, health
    answers under `live/`).
  - Oracle: a digest only when the answer is a JSON object whose `digest`
    is a string of `sha256:` and 64 lowercase hex digits; a live commit
    only when `configured` is exactly `true` and `version` is 40 lowercase
    hex digits, each checked character by character.

### Tool versions bump (Python)

- **Releases listing and digest readers** (`select_release` and
  `parse_digest` in `scripts/tool_versions.py`). `tool-versions-bump.yml`
  reads a tool's GitHub releases listing and its image digest from the
  registry. The chosen version is written to `.tool-versions`, the
  Dockerfiles and the pull request title; the digest to the Dockerfiles'
  `FROM` lines.
  - Test and corpus: `scripts/tests/test_tool_versions.py`,
    `scripts/tests/tool_versions_corpus/` (listings named `<tool>__<case>`;
    digests under `digest/`).
  - Oracle: an independent reading of the listing by exact JSON types,
    each version digit by digit, and each `published_at` against a
    days-in-month table; the pick must be the newest non-draft,
    non-prerelease release past the cooldown and above the pin. A digest
    is accepted only as `sha256:` and 64 lowercase hex digits, checked
    character by character.

### CodeQL language chooser (Python)

- **Changed-path reader** (`parse_paths` and `rust_reason` in
  `scripts/codeql_languages.py`). `codeql.yml` reads the pull request's
  changed file names, which its author chose, and decides whether any is a
  Rust input. A pull request with none reuses `main`'s Rust results (see
  [CodeQL](ci.md#codeql)). The scan for embedded files (`scan_repo`)
  runs only when no changed path is a Rust input, so every `.rs` file it
  reads is `main`'s code, not the author's; it is unit-tested against the
  repository's own includes.
  - Test and corpus: `scripts/tests/test_codeql_languages.py`,
    `scripts/tests/codeql_languages_corpus/` (files named `rust__<case>`
    or `norust__<case>`).
  - Oracle: Rust is analyzed exactly when some NUL-separated name is not
    UTF-8, ends in `.rs`, `.capnp` or `.proto`, is named `Cargo.toml`, `Cargo.lock`
    or `rust-toolchain.toml`, sits under a `.cargo/` directory, or is the
    workflow or one of the two scripts, checked with string operations
    rather than glob patterns.

- **Rust result reuse** (`candidate_runs`, `rust_job_succeeded` and
  `sarif_problem` in `scripts/codeql_reuse.py`). It reads GitHub API
  responses and SARIF that `main`'s own runs kept, not pull request
  content, so it isn't an untrusted boundary. Its output decides what a
  pull request uploads, though, so it is fuzzed the same way.
  - Test and corpus: `scripts/tests/test_codeql_reuse.py`,
    `scripts/tests/codeql_reuse_corpus/` (files named `runs__<case>`,
    `jobs__<case>` or `sarif__<case>`), replayed and mutated both as
    bytes and as JSON values.
  - Oracle: explicit walks with exact type checks. A run qualifies only
    when its id is an integer and it is a completed run on `main` for the
    base commit from a push, the schedule or a manual run. A run is
    reused only when it has exactly one successful `Analyze (rust)` job.
    A SARIF file is accepted only when it is version 2.1.0, every run is
    from CodeQL, and every run carries the id `/language:rust`.

- **Missing-runner recovery** (`queued_pr`, `pr_run_attempt`,
  `run_lineage`, `newer_run`, `stranded_jobs`, `not_acquired`, `pr_head`,
  `enqueues_since_head`, `first_enqueue_on_head`, `gate_run`,
  `stranded_job`, `gate_passed` and `previous_queue_run` in
  `scripts/merge_queue_requeue.py`). It reads GitHub API responses about
  a failed merge-group or pull request run, not pull request content, so
  it isn't an untrusted boundary. Its output decides whether a pull
  request goes back in the queue, a run is re-run, or `CI Gate` passes
  without the macOS job, though, so it is fuzzed the same way.
  - Test and corpus: `scripts/tests/test_merge_queue_requeue.py`,
    `scripts/tests/merge_queue_requeue_corpus/` (files named
    `run__<case>`, `runs__<case>`, `jobs__<case>`, `annotations__<case>`,
    `pr__<case>` or `timeline__<case>`, trimmed from the run that removed #604 from the
    queue), replayed and mutated both as bytes and as JSON values.
  - Oracle: explicit walks with exact type checks. A run counts only when
    it is a failed or cancelled `merge_group` run of `ci.yml` in this
    repository on a `gh-readonly-queue/main/pr-<n>-<40 hex>` branch. A
    pull request run's attempt counts only when the run is a failed
    `pull_request` run of `ci.yml` whose base repository is this one and
    whose `run_attempt` is an integer of at least 1. A run's lineage
    needs a non-empty head repository and branch and a `run_number` of at
    least 1; a run listing supersedes it only when every entry is an
    object with an integer `run_number` and one from the same head
    repository has a larger number. Jobs
    qualify only when every job that didn't succeed or skip is a follower
    job or was cancelled with no runner and no steps. The annotation check
    needs a `failure` annotation naming the not-acquired runner. A pull
    request qualifies only when it is open, `draft` is `false`, auto-merge
    is unset and its head is a full commit id in this repository. Enqueues
    are counted after the last `committed` or `head_ref_force_pushed`
    event, and the first entry on the head is the first
    `added_to_merge_queue` event after it, which needs a
    `YYYY-MM-DDTHH:MM:SSZ` `created_at`. For `CI Gate`, a run counts only when it is a `pull_request` or
    `merge_group` run of `ci.yml` in this repository with a non-empty head
    branch; a job is stranded only when exactly one job has its name (or
    `<caller> / <name>`) and it completed `cancelled` with no runner and
    no steps; the gate passed only when a job named `CI Gate` (or
    `<caller> / CI Gate`) succeeded; and a pull request's previous merge
    group is the queue-branch run for that pull request with the largest
    `run_number` below the current one and a `created_at` no earlier than
    that first entry, across listing pages whose every entry has an
    integer `id` and `run_number`, a string head branch and a timestamp
    `created_at`.

- **Pull request lanes** (`active_runs` and `lane_loads` in
  `scripts/ci_lanes.py`). It reads GitHub API responses about this
  repository's active CI runs and their jobs, so it isn't an untrusted
  boundary. Its output decides which lane each pull request job waits in
  and whether lanes are lent (see [Pull request lanes](ci.md#pull-request-lanes)),
  though, so it is fuzzed the same way.
  - Test and corpus: `scripts/tests/test_ci_lanes.py`,
    `scripts/tests/ci_lanes_corpus/` (files named `runs__<case>` or
    `jobs__<case>`, trimmed from the runs waiting on 11 November 2026),
    replayed and mutated both as bytes and as JSON values.
  - Oracle: explicit walks with exact type checks. A listing is unreadable
    when any run lacks an integer `id` or a string `event` or `status`, or
    any job lacks a string `name` or `status`. A run counts only when it is
    a `ci.yml` run in this repository with a queued, in-progress, pending,
    waiting or requested status; a `merge_group` one marks the queue busy.
    A job counts toward a lane only when its status is one of those and its
    name ends in ` · lane ` and a lane number below 17 with no leading
    zero, split off with `rpartition` and checked digit by digit.

- **Mutation testing start commit** (`choose` in
  `scripts/mutation_base.py`). The nightly mutation workflows read the
  commit of their last successful run from the GitHub API and diff `main`
  against it (see [Mutation testing](ci.md#mutation-testing)). The answer
  is checked before git sees it.
  - Test and corpus: `scripts/tests/test_mutation_base.py`,
    `scripts/tests/mutation_base_corpus/` (files named `sha__<case>`),
    replayed and mutated as bytes.
  - Oracle: a candidate is accepted only when, with surrounding whitespace
    removed, it is exactly 40 characters, each a lowercase hex digit,
    checked character by character rather than with a pattern. git is
    asked about nothing else, and the first such candidate git confirms
    wins.

- **ClusterFuzzLite artifact lookup** (`newest_artifact` in
  `scripts/cflite_artifacts.py`). Every fuzzing run finds each target's
  corpus and the coverage report with one listing of the artifacts by
  name (see [Artifact lookups](#artifact-lookups)). The listing names
  artifacts that `main`'s scheduled runs uploaded, not pull request
  content, so it isn't an untrusted boundary. Its answer decides where the
  job's token is sent, though, so it is fuzzed the same way.
  - Test and corpus: `scripts/tests/test_cflite_artifacts.py`,
    `scripts/tests/cflite_artifacts_corpus/` (files named
    `artifacts__<case>`, trimmed from a real listing), replayed and
    mutated both as bytes and as JSON values.
  - Oracle: an explicit walk with exact type checks. The first entry
    wins whose `id` is a positive integer, whose `name` is the one asked
    for, whose `expired` is `false` and whose `archive_download_url` is
    exactly `<listing>/<id>/zip` on this repository's API.

### CLA check (Python)

- **CLA checker** (`scripts/cla_check.py`). It reads the workflow event
  (including the head a `workflow_run` names and the check a `check_run`
  re-run names), the pull request, its commit
  and comment pages, the co-author query answer, the lists of pull requests
  that contain the head commit or come from the head branch, the
  signature file, the git objects it creates when the first signature
  starts the signature branch, the reactions it adds to signing
  comments, the check runs it creates, a commit's combined status, and
  the rules that apply to that branch,
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

### Maintainer review (Python)

- **Maintainer review check** (`scripts/maintainer_review.py`). It reads
  the workflow event (the head a `workflow_run` names, a merge group's
  head, the check a `check_run` re-run names), collaborator permission
  answers, and the pull request's review and changed-file pages, and
  decides whether an outside pull request may merge. Logins, branch names
  and file paths in them are chosen by contributors. It shares the CLA
  checker's JSON readers and pull request parsers.
  - Test and corpus: `scripts/tests/test_maintainer_review.py`,
    `scripts/tests/maintainer_review_corpus/` (files named
    `<kind>__<case>`).
  - Oracle: an independent field-by-field reading of each document, which
    refuses absolute paths, `..` segments and accounts whose login does not
    fit their type. For fuzzed file pages, the flagged paths are exactly
    those an independent reading of the sensitive prefixes, names and
    missing diffs selects, and each listed path stays inside one code span
    in the summary.

### Signed commits (Python)

- **Signed commits check** (`scripts/signed_commits.py`). It reads the
  workflow event (the head a `workflow_run` names, a merge group's head,
  the check a `check_run` re-run names), the pull request with its base
  and default branch, and its commit pages with GitHub's verification of
  each commit, and writes a comment from them. Branch names, logins, titles
  and commit messages in them are chosen by contributors. It shares the CLA checker's JSON readers and comment
  page parser and the maintainer review check's event and pull request
  readers.
  - Test and corpus: `scripts/tests/test_signed_commits.py`,
    `scripts/tests/signed_commits_corpus/` (files named `<kind>__<case>`).
  - Oracle: an independent field-by-field reading of each document, which
    refuses a `verified` that isn't a boolean, a `reason` that isn't
    lowercase letters and underscores, and a default branch name the
    re-sign command could not quote safely. For fuzzed commit pages, the comment
    lists exactly the commits the independent reading finds unverified,
    each table row keeps its two cells, and the only link is GitHub's
    signing guide.

### Coverage gate (Python)

- **Coverage report and diff readers** (`parse_lcov` and `parse_diff` in
  `scripts/coverage_gate.py`). CI Gate reads the LCOV reports and the
  change's hunk headers that the pull request's own run uploaded, so a
  pull request chooses their bytes.
  - Test and corpus: `scripts/tests/test_coverage_gate.py`,
    `scripts/tests/coverage_gate_corpus/` (files named `<kind>__<case>`).
  - Oracle: an independent reading of each report record and hunk header
    with full-line patterns, and of each `SF:` path by walking its
    segments. Every path the module keeps is relative to the repository
    and never climbs out of it, and every hit count is a non-negative
    integer.
  - The Codecov upload sends only what this parser accepted:
    `--export` writes each parsed report back out, and a test checks that
    the written report reads back the same.
- **JUnit test results reader** (`parse_junit` in `scripts/coverage_gate.py`).
  The Codecov upload reads the JUnit reports the pull request's own run
  uploaded, so a pull request chooses their bytes.
  - Test and corpus: the same test file and corpus (`junit__<case>`).
  - Oracle: the boundary's contract, since the standard library has no
    second XML parser. A report with a document type declaration reads as
    empty. Every kept name is one line of safe characters within its
    limit. Durations are finite and in range. Failure text appears only on
    a failed or errored case. The report the export writes reads back to
    the same test cases.
- **Bundle file selection** (`select_bundle_files` and `export_bundles` in
  `scripts/coverage_gate.py`). The Codecov upload reads the built bundles
  the pull request's own run uploaded, so a pull request chooses their file
  names and sizes.
  - Test and corpus: the same test file and corpus (`bundle__<case>`, one
    `<size>` tab `<path>` line per file).
  - Oracle: a second selection that splits each path into segments instead
    of matching one pattern. Kept paths never start with `/`, climb with
    `..` or hold a backslash, and the kept files stay within the count and
    size limits. Symbolic and hard links are never followed or copied.

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
  request path and takes one hop, docs keys never leave the tree, and
  only a version's own `search-index.json` path names its search index.
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
  generated Web API pages from the docs repository into operation
  sections, with one panel per example response (`layoutResponses`),
  one per example request language (`layoutRequests`) and property rows
  for the parameter, body and response lists (`layoutProperties`).
  Corpus `tools/site/fuzz/api_reference/`. Oracle: the HTML nests
  properly, every section is labelled by its own heading with one badge
  for a known method, every response panel has a unique id and a caption
  marking its status, every request panel has a unique id naming its
  language and holds one code block, every property row has its head and
  every fold its summary and list, and the page keeps the same text as it
  has without the layout.
- **Web API index layout** (`layoutIndex` and `indexRoutes` in
  `tools/site/api-reference.ts`, through `renderDoc`) lays out the
  generated Web API index as route group cards with a quick-start
  aside, and reads its routes for the sidebar. Corpus
  `tools/site/fuzz/api_index/`. Oracle: the HTML nests properly, every card
  and aside is labelled by its own heading, every badge names a known
  method, and the page keeps the same text as it has without the layout.
- **Last-updated reader** (`pageDate` and `datedPages` in
  `tools/site/last-updated.ts`) dates each docs page from the docs
  repository's `last-updated.json`, and lists every page for
  `/sitemap.xml`. Corpus `tools/site/fuzz/last_updated/`. Oracle: the
  document's shape read through property descriptors, the date digit by
  digit against a days-in-month table, an optional `THH:MM:SSZ` time field
  by field, and each sitemap key segment by segment. The date shown is only
  `Mon D, YYYY`, and its `<time>` element holds only the date's own
  characters.
- **Generated manifest reader** (`parseGenerated` and the lookups built
  on it in `tools/site/generated.ts`) reads the docs repository's
  `generated.json`, which says which pages the docs build generated, from
  what, and which reference each belongs to; the Worker uses it for the
  Web API layout, the source line, the sidebar lists and the sitemap
  page. Corpus `tools/site/fuzz/generated/`. Oracle: the document read
  through property descriptors by exact types, every folder, page and
  source path segment by segment, each page under its folder, and the
  lookups (a page's reference, whether it is generated, its source, the
  Web API's pages and a reference's modules) derived plainly from the
  oracle's reading.
- **IndexNow sitemap reader** (`readSitemap` in `tools/site/indexnow.ts`)
  reads the live site's sitemaps in the `indexnow` workflow, which then
  submits the URLs they list. Corpus `tools/site/fuzz/indexnow_sitemap/`.
  Oracle: a hand-written scanner for the entries the Worker writes, its own
  entity decoder, a URL check against the site's origin and the
  last-updated date oracle.
- **IndexNow snapshot reader** (`readSnapshot` in `tools/site/indexnow.ts`)
  reads the previous run's snapshot back from the Actions cache. Corpus
  `tools/site/fuzz/indexnow_snapshot/`. Oracle: the document read through
  property descriptors, each URL checked against the site's origin and each
  date by the last-updated date oracle.

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
  `decodeInstances`, `decodeDiscoverReady`). Corpora `connections/`,
  `claim_ticket/`, `plugin_instances/`, `discover_status/`. Oracle:
  connections deduplicated, bounded and well-formed; a claim ticket only
  when well-formed; only wanted default plugin instances with well-formed
  ids, the first per plugin, on a well-formed node id, marked deployed
  only when that instance has a `present` row for that node; a shelves-ready flag
  only when `ready` is a boolean in an object.
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

### Metadata provider guests

The `openlibrary` and `audnexus` guests parse upstream JSON inside the
jailed isolate. What reaches the host is the Cap'n Proto reply that
`abi_decode` and `abi_roundtrip` already cover, so these parsers guard the
records each guest returns. Each guest's `test/fuzz.test.ts` replays its
corpora under `fuzz/` and runs seeded mutation fuzzing, on the parsed JSON
tree and on its text, in `npm test` (the `workerd_guests` check).
`FUZZ_SEED` and `FUZZ_ITERATIONS` pick another run.

- **Open Library search results** (`recordsFromSearch` in
  `crates/bookclerk-plugins/optional/metadata-openlibrary/src/openlibrary.ts`),
  under a lookup and a title search. Corpus `fuzz/search_response/`.
  Oracle: a body is refused exactly when it is not an object or its `docs`
  is not a list. Records stay within the limit, each work and edition id
  appears in the body, ISBNs have the ISBN shape, the URL names the edition
  or else the work, and covers are Open Library cover URLs. Credits are
  distinct authors, and lists stay within their caps. Every string is
  non-empty, well-formed UTF-16 and at most `MAX_TEXT` long, and the
  mapping is deterministic.
- **Audnexus books** (`recordFromBook` in
  `crates/bookclerk-plugins/optional/metadata-audnexus/src/audnexus.ts`).
  Corpus `fuzz/book/`. Oracle: a record exactly when the body has a
  ten-character ASIN and a non-blank title, and the ASIN is the first
  identifier with the asked region. Credits are distinct per role and
  capped. At most two series entries. Dates are `YYYY-MM-DD`, covers are
  `https://`, lengths and ratings are in range, and the same string rules
  as Open Library apply.
- **Audnexus chapter layouts** (`chaptersFromBody`). Corpus
  `fuzz/chapters/`. Oracle: the chapters' starts and depths equal an
  explicit-stack pre-order walk of the body, down to `MAX_DEPTH` and up to
  `MAX_CHAPTERS`. `found` exactly when that walk finds any. Durations are
  positive safe integers.

### Cover fetcher guest

The `covers` guest decides which URLs it requests: the host's URLs and every
redirect `Location` an upstream server sends. It also passes header values
both ways: the host's validators out as request headers, and an upstream
server's caching headers back in its answer. Its `test/fuzz.test.ts` replays
`fuzz/url/`, `fuzz/location/` and `fuzz/header/` and runs seeded text mutations in
`npm test` (the `workerd_guests` check); `FUZZ_SEED` and `FUZZ_ITERATIONS`
pick another run.

- **Cover URL policy** (`checkUrl` in
  `crates/bookclerk-plugins/optional/covers/src/fetcher.ts`). Oracle,
  written from the policy and the WHATWG URL rules: accept exactly an
  `https` URL with the default port, no credentials, no control characters
  or spaces, at most 4096 code units, and a hostname whose labels are
  non-empty and whose last label is not a number (the URL parser's IPv4
  test) or a bracketed address. Accepted URLs are canonical and
  idempotent.
- **Redirect chains** (`fetchOne`). Mutated `Location` chains: every
  request is a policy URL, each hop is the previous `Location` resolved
  against the URL that sent it, no more than `MAX_REDIRECTS` are followed,
  and a body is stored only after a chain the policy allowed in full.
- **Header values** (`headerValueOk`, `checkParams` conditions and
  `passHeaders`). Oracle, written from the ABI's `maxCoverHeaderBytes` and
  the visible-ASCII part of RFC 9110's `field-value`: a value passes
  exactly when it is at most 1024 bytes of tab, space and `VCHAR`. A
  condition outside that is refused, and a response header outside it is
  dropped from the answer.

### Egress bridge User-Agent rule

The workerd egress bridge (`bridge/egress.js`, one file synced into
`bookclerk-workerd` and both plugin SDKs) decides on every hop whether a
request keeps the guest's own `User-Agent` (`keepsOwnUserAgent`) and sets
the host's agent otherwise (`withUserAgent`). The policy comes from the
host, but a request's host comes from the guest.
`packages/plugin-sdk/scripts/test-egress-user-agent.mjs` replays
`egress_user_agent_corpus/` and runs seeded mutations of hosts, patterns
and agents (`npm run test:egress-user-agent`, in the `plugin_sdk_abi`
check); `FUZZ_SEED` and `FUZZ_ITERATIONS` pick another run.

- Oracle, written from the [User-Agent](../plugins/capabilities.md#user-agent)
  rule: host and pattern are trimmed once, lose trailing dots and are
  lowercased, and one holding `%` or non-ASCII never matches. A `*` pattern
  never matches. `*.suffix` matches the suffix and any name whose labels end
  with its labels; any other pattern matches exactly. The guest's agent is
  sent only when it set one and the host matches; otherwise the policy's
  agent, or the bridge's fallback when the policy names none.

## Still to add

These are tracked in the fuzzing ADR's follow-ups:

- a full RPC session against a scripted guest;
- the rest of the workerd `egress.js` gate (destinations, redirects,
  addresses) checked against `EgressPolicy` (#335; the User-Agent rule is
  [covered](#egress-bridge-user-agent-rule));
- Audible CENC/DASH parsing;
- package extraction;
- naming paths;
- the node frames' per-body checks beyond parsing (upload manifests,
  storage keys) and the frames' HTTP side;
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
| `cflite-pr.yml` | Pull requests touching `fuzz/`, the fuzzed crates, or the ClusterFuzzLite config | 10 minutes, AddressSanitizer, code-change mode through `.github/actions/cflite-run-fuzzers`; fails on a new crash |
| `cflite-batch.yml` | Daily, and `workflow_dispatch` | 45 minutes of batch fuzzing split across targets, then [crash triage](#fuzz-crash-triage), corpus pruning and a coverage report |

The batch run's triage is private, and its corpus and coverage report are
kept as Actions artifacts.

### Artifact lookups

Both workflows run the same pinned run-fuzzers image, entered through
`scripts/cflite_artifacts.py`, which changes how ClusterFuzzLite finds a
target's corpus or the coverage report:

- **Stock lookup.** ClusterFuzzLite lists every artifact in the repository,
  100 per page, until a name matches. It stops paging only after as many
  artifacts as the first page counted. When none matches (a target added
  since the last batch run) and artifacts expire meanwhile, it keeps
  asking for empty pages. One pull request run asked for 4,840 pages and
  used up the repository's hourly `GITHUB_TOKEN` budget, so other
  workflows failed until the hour reset.
- **Bounded lookup.** Each lookup is one request for the artifacts with
  that name, newest first. The newest one that has not expired and
  downloads from this repository is used; a target without one fuzzes
  from its seeds, as before. Listing every artifact is disabled.

If the image no longer has the methods the script replaces, the job fails
instead of fuzzing with the stock lookup. Bump the image digest in both
actions and `cflite-batch.yml` together, then re-test.

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
   - Corpus and coverage artifacts are unchanged (looked up as in
     [Artifact lookups](#artifact-lookups)), and pull-request fuzzing still
     logs and uploads its crashes. The log shows a crash count.
   - Timeout inputs are kept (`REPORT_TIMEOUTS`), so triage sees them.
   - Every scheduled build sets `bad-build-check: false`, because that
     check runs each target briefly and prints its output to the log.
   - `prune` gives each target `fuzz-seconds` (3600) divided by the
     targets left as a hard deadline for merging its corpus. A target
     whose merge runs past it, or fails, keeps its corpus as it was, and
     the log says so; the other targets are still pruned and the job
     passes. Any other error fails it.
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
