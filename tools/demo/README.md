# Hosted demo

The `bookclerk-demo` Worker lets a visitor try Bookclerk without installing
it. Each visitor gets their own Cloudflare Container running `bookclerkd` and
the storefront plugins for 30 minutes. They can sign in to their own store
accounts, scan their library and search it. Nothing can be acquired or
downloaded, and their store sign-ins are revoked when the session ends.

The library is not empty when the visitor arrives. The image is built with
it already seeded (`packaging/demo/bake-seed.sh`, the `seeded` stage of
`packaging/demo/Dockerfile`): `bookclerk-demo-seed`
(`crates/bookclerk-demo-seed`) adds a second library user, Sam, with
accounts at Audible, Libro.fm, Chirp and GraphicAudio, ten well-known
audiobooks bought from them, four wishlist items, and a branch,
`sams-nas`, that shared four more titles and has been offline for three
days. The seed first approves and deploys the four storefronts and the
covers plugin, with exactly the grants their manifests request, then looks
each title up in its store's public catalog through the storefront plugins
(the search Discover uses, at most 15 seconds) and writes the record the
store returns, so product ids, cover art and descriptions match the store.
A title it cannot find keeps built-in fields under a made-up `demo-…` id.
The accounts hold no tokens. It writes through the library code of the same
commit, never raw SQL, and its test runs against a fresh database in CI, so
a schema change that breaks it fails there.

The bake then runs `bookclerkd` once, with a throwaway operator token kept
in its environment, until it has stored Sam's Discover shelves.
`bookclerk-demo-seed --bake-finish` then strips everything a container must
make for itself, and the build fails if any of it is left:

- `master.key`, and the fingerprint of it the library keeps;
- the covers cache's URL-signing key;
- cluster leases, node heartbeats, plugin sessions and logs.

`--bake-finish` refuses a library that holds any sealed secret, sign-in
session or claim ticket. The node identity (`node-identity.json`) is baked,
so every demo container shares the same node and cluster id. Each one is a
VM that never joins a cluster, the ids carry no secret, and the baked plugin
deployments are keyed by the node id.

A fresh container's entrypoint (`packaging/demo/boot.sh`) copies that
template into its empty files dir and execs `bookclerkd`, which mints and
wraps the container's own `master.key` with the Worker's password. Without a
template, or when the copy fails, it seeds live as before; if that fails
too, `bookclerkd` starts with an empty library.

## How a session works

The Worker answers `GET /robots.txt` itself, before any session lookup, so
a running container never serves it: it disallows every crawler and sets
every content signal to `no` (`use=immediate`).

1. The launch page (`GET /`) asks for a Turnstile check. `POST /_demo/start`
   verifies it with Siteverify (hostname and action must match), then admits
   the session against the limits below and records it in D1. The Worker's
   pages load nothing from outside except Turnstile and the Cloudflare Web
   Analytics beacon the edge adds, as on the website, and their CSP allows
   only those two (`PAGE_CSP` in `src/pages.ts`).
2. The visitor's `DemoSession` Durable Object (`src/session.ts`) starts the
   container with a fresh operator token and `master.key` password, and the
   demo's own origin as bookclerkd's `public_origin` (so bookclerkd's own
   cross-site checks and store sign-in callbacks use it), waits for
   `/health`, and approves and deploys the four storefronts and covers as
   the operator (as an operator would in Settings; visitors cannot). It
   reads the plugin instance list first and leaves any plugin already
   deployed on the node alone, so on a baked image that one read is all
   it sends. It then makes the visitor
   the host's Owner (with the fixed password `VISITOR_PASSWORD` in
   `src/lifecycle.ts`) and redeems the Owner's claim ticket. It then reads
   `GET /api/discover/status` as the visitor every second for up to 25
   seconds, so the Discover shelves the visitor lands on are built before
   the session opens; past that it opens anyway and the page fills in as
   they arrive. The Durable Object then forgets the operator token. The
   visitor waits on
   `/_demo/wait`, which says how long a start usually takes: the median of
   the last week's recorded startup times (`sessions.startup_seconds`).
3. `/_demo/enter` hands the browser the portal session cookie. From then on
   every request goes through the Worker (`src/app.ts`), which:
   - refuses any operation not allowed for the demo (`src/routes.ts`) before
     the container sees it: acquire and download, plugin installs and
     settings, users, auth changes, node and cluster routes, among others.
     Every operation in `crates/bookclerkd/openapi.json` has an explicit verdict and a
     test fails when the two drift;
   - refuses cross-site writes itself, then strips `Authorization`,
     `Cookie` (other than bookclerkd's own) and client IP headers;
   - counts store sign-in starts against per-session and per-day limits;
   - forwards the request as the visitor's own session, so bookclerkd
     authorizes everything as the visitor's Owner account and
     scans only the visitor's store accounts. The visitor may elevate to
     operator with the Owner password and impersonate the seeded users, as
     an Owner would. Elevated, they act as the operator of their own
     container, so a scan covers every account in it (their own and the
     seeded ones), and they still reach only the allowed routes;
   - rewrites `Set-Cookie` so the container cannot set the Worker's cookies,
     and adds a banner with the time left and a **Start over** button. Its
     **Demo** label opens a panel that shows the Owner password and how to
     elevate, and the banner fills that password into the app's empty
     **Elevate to Operator** box;
   - keeps bookclerkd's `Cache-Control` on everything it forwards, and adds
     `no-transform` to the app's HTML, so Cloudflare does not add its Web
     Analytics beacon inside the visitor's copy. The Worker's own pages,
     JSON and redirects are `no-store` and `robots.txt` keeps an hour
     (`src/cache-control.ts`, checked by `test/app.test.ts`); the files
     under `public/_demo/` keep the platform default, checked before each
     use.

   The waiting page and the banner follow the session over a WebSocket
   (`/_demo/live`, `src/live.ts`, `public/_demo/live.js`): the Durable Object
   pushes the session's state and the seconds left by its own clock on every
   change, so the waiting page moves on as soon as the copy is ready and every
   open tab learns at once that the session ended. Messages on the sockets do
   not count as activity for the idle timer. When a socket cannot open, or a
   visitor has more than eight tabs open, the pages poll `/_demo/session`
   instead. The countdown is display only;
   the Durable Object's own timer ends the session.
4. The session ends when its time is up, after 10 idle minutes, or on
   **Start over**. The Durable Object revokes every linked store account,
   destroys the container and refunds unused time to the day's budget. A
   cron sweep every five minutes ends any session that outlived its time.

## The image

`packaging/demo/Dockerfile` starts from the production builder and image
built from the same commit, so every crate and plugin is the version that
commit ships. `packaging/demo/docker-bake.hcl` builds all three in one
BuildKit build, from the repository root:

```bash
docker buildx bake -f packaging/demo/docker-bake.hcl --load demo
```

That build never exports the builder, and it links the demo's binaries with
thin LTO instead of the fat LTO shipped images use, which takes minutes
rather than most of the build. The image adds the Audible, Libro.fm, Chirp,
GraphicAudio and
`covers` plugins, and removes the `local` output plugin, so the host has
nowhere to acquire to even if a request got through. `covers` downloads
covers, store icons and avatars, and `bookclerk-media-worker` re-encodes
them, so the web app loads every image from the demo's own origin.

Plugins and the media worker run with `isolation = "best-effort"` because
the Containers kernel has no Landlock. This is a demo-only exception: each
visitor's VM is the boundary, seccomp and network policy still apply, and
shipped builds keep `"required"`.

## Limits

Set in `wrangler.toml` `[vars]`:

| Variable | Default | Meaning |
| --- | --- | --- |
| `MAX_LIVE_SESSIONS` | 20 | Sessions running at once (also the container `max_instances`) |
| `DAILY_BUDGET_HOURS` | 40 | Visitor-hours reserved per UTC day; unused time is refunded |
| `SESSION_MINUTES` | 30 | Length of one session |
| `SESSIONS_PER_HOUR` | 3 | Sessions one IP may start per hour |
| `SIGNINS_PER_SESSION` | 5 | Store sign-in starts per session |
| `SIGNINS_PER_DAY` | 10 | Store sign-in starts per IP per day |

IP addresses are stored only as a keyed hash that changes every day.

## Deploying

`.github/workflows/demo-deploy.yml` deploys on demo changes to `main`, on
each published release, after each image publish from `main` (nightly), and
on demand. When `images-publish.yml` already pushed the demo image for the
commit (`ghcr.io/<owner>/bookclerk-demo:sha-<12 hex>`), the deploy verifies
its build provenance and deploys it in a few minutes; otherwise it builds
the image itself, which takes about 15 minutes. A run after a publish or a
release stops early when the live demo already runs that commit (and an
earlier run for it succeeded) or a newer one. It needs the repository
secrets `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`; the token's
permissions are listed in
[`docs/contributing/automation.md`](../../docs/contributing/automation.md#cloudflare-worker-deploys). The workflow makes
the Worker secrets itself:

| Secret | Source |
| --- | --- |
| `TURNSTILE_SITE_KEY`, `TURNSTILE_SECRET` | The `bookclerk-demo` Turnstile widget, created on the first deploy (`scripts/worker-secrets.ts`) |
| `SESSION_KEY` | Random, generated once; keyed hashes of session tokens and IPs |

With the `DOMAIN` variable set the demo is served on `demo.<DOMAIN>` and the
banner links to `https://<DOMAIN>`. Without it (a fork) the demo is served on
the account's workers.dev subdomain, recorded in the `DEMO_URL` variable.

The deploy smoke checks `/_demo/health`, the launch page and that the API
refuses a request without a session. It cannot start a session, since that
needs a Turnstile answer from a person.

## Tests

```bash
npm ci
npm run typecheck
npm test
```

The Worker, its deploy script and the tests are strict TypeScript. Wrangler
bundles the `.ts` sources and Node (26 in CI, 22.18 or newer) runs
`scripts/worker-secrets.ts` by stripping types, so nothing is compiled.
`npm test` runs Vitest (`vitest.config.ts`): the Worker's tests run inside
the Workers runtime through `@cloudflare/vitest-plugin`, on the Worker's
compatibility date and against real D1 databases with `migrations/`
applied, and the tests of the page scripts run in Node. `npm run coverage`
adds a V8 coverage report.
`npm run typecheck` checks everything with the Workers and Node types
(`tsconfig.json`) and the Worker's sources again with the Workers types only
(`tsconfig.worker.json`). The scripts under `public/_demo/` are browser
scripts the Worker serves, not Worker code, and stay JavaScript. The fonts
under `public/_demo/fonts/` and `public/_demo/mark.svg` are copies of the
website's fonts and the app's mark, the icons (`favicon.ico`,
`favicon-32x32.png`, `apple-touch-icon.png`) are copies of the app's, and
`demo.css` repeats the website's metric-matched fallback fonts so the launch
page does not shift when the web fonts arrive; a test fails when a copy
drifts.

`test/inputs.fuzz.test.ts` replays the seed corpora under `corpus/` and
fuzzes every input boundary against independent oracles (see
[`docs/contributing/fuzzing.md`](../../docs/contributing/fuzzing.md)). `test/routes.test.ts` checks the
route table against the OpenAPI document. CI runs these as `demo_test`.

`package.json` overrides `undici` (a development dependency through
wrangler's `miniflare`, which pins 7.29.1) to 7.30.1 for GHSA-xjj6-f33q-929c,
GHSA-4qv8-2p9j-2685 and GHSA-f72v-6w45-q472. Drop the override once wrangler
ships a `miniflare` with the fix.
