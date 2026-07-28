# Discovery, recommendations, and wishlist

Bookclerk’s discovery stack turns **local taste** (owned library + listening)
into **unowned storefront candidates**, then ranks those with embeddings and
heuristics. Operator-facing surfaces ship first; the schema is ready for
Connect-portal identities later.

## Goals

1. **Discover unowned titles** from storefront catalogs (Libro related books,
   Audible public search / series ASIN, Chirp GraphQL related/series/author/
   deals, GraphicAudio Magento series + related), not only from the owned library.
2. **Evaluate** those candidates against local ownership, listening, ratings,
   and embeddings.
3. **Personal wishlist** (store-agnostic) with a **global request queue** ranked
   by how many users wishlisted the same work — including one-click from Discover
   cards and catalog search.
4. **Suggest where to buy** once a candidate is known.
5. Stay fit for a **cheap VPS** (≈1 vCPU, 1–2 GB RAM): no separate vector DB;
   embeddings score remote candidates locally.

## Candidate generation (storefronts first)

```text
 local taste seeds          storefront expansion           local scoring
 (finished / rated /   →   Libro related_audiobooks   →   filter owned
  listening)                Audible author / series         embed similarity
                            ASIN / (opt) narrator           author/series boost
                            Chirp related / series /        purchase hints
                            author / catalog / deals
                            GraphicAudio Magento
                            related + series + search
```

Owned library rows are **seeds**, not the candidate pool. Open wishlist items
merge into the personalized Discover feed (and the shared global queue).

Chirp and GraphicAudio expansion prefer seeds already owned on those sources
(Chirp product id → `relatedAudiobooks`; Magento product id → related block +
series page). Series / author signals from any seed can still query Chirp or
Magento when the catalog has a match. GraphicAudio series-set SKUs are
**included by default**; set `exclude_graphicaudio_series_sets = true` to drop
them. Chirp top/free deals are pulled once per recommend run (unowned only).

Listening taste is **optional** and **provider-agnostic**. Integrations that
support listening sync (Audiobookshelf today; third-party plugins via capability
`sync_listening`) write into the shared `listening_progress` table. Ranking never
calls those adapters — if no listening data is present, or you pass
`--no-listening` / `?no_listening=1`, discovery still runs on owned-library taste
alone. Scope with optional `external_user_id` / `?user=` and/or
`--listening-provider` / `?listening_providers=` for multi-user or multi-integration
libraries.

## Open Library (compliance)

Used only for **low-volume metadata gap-fill** on owned rows (subjects /
description / cover), never as a bulk catalog backend.

Per [Open Library API guidelines](https://openlibrary.org/developers/api):

- Identify with `User-Agent: Bookclerk/<ver> (<email>; …)` via
  `discovery.openlibrary_contact_email`
- Space requests (~400 ms); cap `openlibrary_max_requests_per_run` (default 25)
- Skip rows already enriched (`enrich_source = openlibrary`) — provenance cache
- For bulk data, use [monthly dumps](https://openlibrary.org/data), not the API

WorldCat remains reserved (API key + ToS).

## Embeddings (small / VPS-friendly)

Discovery ships with **quantized MiniLM-L6-v2** via `fastembed` / ONNX Runtime
(~22 MB model under `models/` plus the ORT shared library, ~50 MB RAM, 1
intra-thread by default). ORT is **loaded dynamically** (not statically linked)
so hosts with glibc < 2.38 still build; on first successful load the runtime
and model are cached under the Bookclerk files dir.

If ONNX cannot load (offline host, unsupported glibc for the ORT dylib, corrupt
cache, …), Bookclerk **warns and falls back** to a **local-hash** 384-d
embedder (no download, negligible RAM) so recommend still works.

`discovery.embeddings_enabled = false` (or CLI `--hash` / `?no` paths that pass
`prefer_onnx = false`) skips ONNX and uses local-hash directly.

### OSV / `paste`

`fastembed` → `tokenizers` (and related crates) pull crates.io `paste` 1.0.15
as a **compile-time proc-macro**. That crate is **unmaintained**
([RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436.html):
INFO, no CVE, no patched release). There is no crates.io drop-in under the same
name; renaming to maintained [`pastey`](https://crates.io/crates/pastey) would
still leave OSV matching the lockfile package name `paste`. Bookclerk therefore
keeps the registry dependency and records a narrow ignore in
[`osv-scanner.toml`](../osv-scanner.toml). Revisit when upstream depends on
`pastey` (or drops `paste`) directly.

## Recommendation ranking

Candidates are **unowned** storefront hits (plus open wishlist items), scored by:

1. Storefront origin (related / series / author / catalog search)
2. **Series completion** — own some of a series → boost unowned siblings; prefer
   the next index; stronger when the series is actively listened to, and
   stronger still when multiple books in that series have listening activity.
   Listening weight is driven by **absolute hours heard** (soft-saturated), with
   percent complete only a secondary bonus — so 50% of a 30‑hour book outweighs
   50% of a 3‑hour book. Finished titles still get a completion bump on top of
   hours listened.
3. Overlap with liked authors
4. Embedding similarity to finished / highly rated / recently listened works
5. Wishlist boost (`wish_count × 40` on the shared queue / Discover merge)
6. Purchase hints resolved at view time (see below)

## Wishlist (no approval flow)

There is **no triage / approve / reject** path. An item stays in the **global
queue** while at least one user has it open on their personal wishlist and the
title is **not** in the household library.

- Personal wishlist: `GET`/`POST` `/api/wishlist`, `DELETE /api/wishlist/{uuid}`
  (un-wishlist = cancel own row only)
- CLI: `bookclerk discover wishlist add|list|remove`
- Global queue: `GET /api/request-queue` — shared order using **overall /
  operator** taste (not per-portal personalization) plus a heavy wish-count
  weight
- Identity merge: canonical ISBN-10↔13 when present, else ASIN, else soft
  title+author; ASIN-keyed and ISBN-keyed rows for the same work are merged
  (ISBN is **not** universal across Chirp / GA / Audible public search)

Multi-region storefronts are deferred (US default for now).

## Configuration

```toml
[discovery]
embeddings_enabled = true
embedding_model = "local-hash-v1"
embed_intra_threads = 1
storefront_candidates = true
storefront_seed_limit = 8
storefront_max_remote_calls = 32
# exclude_graphicaudio_series_sets = true  # opt-in; default keeps Magento series sets
openlibrary_enabled = true
# openlibrary_contact_email = "you@example.com"
openlibrary_max_requests_per_run = 25
listen_sync_interval_minutes = 60  # 0 disables daemon listening sync
recommend_limit = 20
```

Listening sync fans out through `IntegrationRegistry` (every integration with
`supports_listening_sync`). Daemon honors `listen_sync_interval_minutes`; CLI
`discover sync-listening` and `POST /api/discover/sync-listening` do the same.

Shelf visibility is **per-user** (SQLite `user_preferences`, not TOML). Use
Discover → settings in the GUI, or `GET` / `PATCH /api/preferences` with
`{ "disabled_shelves": ["chirp_deals", "genre"] }`. Empty list = all shelves on.
CLI `discover recommend` applies the operator prefs row.

## Surfaces

| Surface | Commands / routes |
| --- | --- |
| CLI | `bookclerk discover recommend` (prints shelves), `embed`, `sync-listening`, `wishlist …` |
| Daemon | `GET /api/discover/recommendations`, `GET /api/discover/search?q=` (multi-store catalog autocomplete), `POST /api/discover/purchase-hints` (+ `/batch`), `GET`/`POST` `/api/wishlist`, `DELETE /api/wishlist/{uuid}`, `GET /api/request-queue`, `GET`/`PATCH /api/preferences` |
| GUI | **Discover** — shelves + top catalog search (wishlist from cards or suggestions); **Wishlist** — personal list with global queue sidebar |

Wishlists are **store-agnostic**. Rows share a `work_key` plus runtime identity
merge (ISBN/ASIN aliases and soft title+author). The global queue ranks with
**overall / operator** taste plus `wish_count × 40`. Every viewer sees the same
queue order; Discover personalizes the feed (including wishlist items).

### Shelf taxonomy

| Shelf | Kind id | Signal |
| --- | --- | --- |
| Finish these series | `finish_series` | Incomplete series + next index |
| Pick up where you left off | `keep_listening` | Active / multi-book listening in a series |
| More from {Author} | `author` | Liked-author overlap (top authors) |
| If you like {Author} | `because` | Related/similar titles **not** by that author |
| Narrated by {Narrator} | `narrator` | Liked-narrator overlap |
| Because you like {Genre} | `genre` | Category/subject overlap from finished titles |
| From {Store} | `from_store` (`from_audible`, …) | Candidates from storefronts already in the library |
| Chirp deals right now | `chirp_deals` | Chirp top + free deals |
| Similar to books you finish | `similar_taste` | Embedding similarity |
| On the wishlist | `requests` | Open shared wishlist items |
| Top picks for you | `top_picks` | Fallback when every other shelf is empty |

Shelf **titles** stay dynamic (`More from {Author}`, …). Prefs filter on stable
**category tags** carried on each recommendation (`finish_series`, `author`,
`genre`, …), not on human-readable reason strings.

All kinds are **offered by default**. Each user hides shelves via
`disabled_shelves` in `/api/preferences` (Discover settings in the GUI). The
recommendations endpoint applies the caller's prefs server-side; the API still
returns `shelf_kinds` so the prefs UI knows what can be toggled.

Kind matching: `author` hides every `author:…` row; `from_store` hides all
`from_*` rows; exact ids like `finish_series` or `from_chirp` also work.

## Store links and pricing

Recommendations are consolidated by bibliographic identity (canonical ISBN when
present, ASIN, soft title+author) so the same book from multiple storefronts
appears as **one card** with `store_editions` for each catalog match.

Live prices use **public** storefront endpoints (no auth). The Discover feed
loads with `no_purchase_hints=true`; the GUI:

1. Progressively reveals shelves/cards while scrolling
2. **Viewport-gates** pricing (`IntersectionObserver`)
3. **Batches** visible cards into `POST /api/discover/purchase-hints/batch`

The daemon still searches **all** stores, then highlights `best` using the
caller’s associated accounts (portal account links, or all operator accounts):
lowest priced offer among linked storefronts when priced, otherwise the global
lowest known price. Client-supplied `preferred_sources` are ignored.

## Non-goals (this iteration)

- Chirp personalized endpoints that require a logged-in session
  (`currentUserRelatedAudiobooks`, …)
- Multi-region storefront merchandising (US only for now)
- Bulk Open Library harvest (use dumps)
- Paid WorldCat / Goodreads / Hardcover providers
- A long-running vector DB sidecar
- Audible Spatial/Atmos (L1) merchandising
