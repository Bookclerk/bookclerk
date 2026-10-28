# Hosted demo

The `bookclerk-demo` Worker lets a visitor try Bookclerk without installing
it. Each visitor gets their own Cloudflare Container running `bookclerkd` and
the storefront plugins for 30 minutes. They can sign in to their own store
accounts, scan their library and search it. Nothing can be acquired or
downloaded, and their store sign-ins are revoked when the session ends.

The library is not empty when the visitor arrives. Before `bookclerkd`
starts, the image runs `bookclerk-demo-seed` (`crates/bookclerk-demo-seed`),
which adds a second library user, Sam, with accounts at Audible, Libro.fm,
Chirp and GraphicAudio, ten well-known audiobooks bought from them, four
wishlist items, and a branch, `sams-nas`, that shared four more titles and
has been offline for three days. The seed first approves and deploys the four
storefronts, with exactly the grants their manifests request (the Worker does
the same through the daemon once it is up), then looks each title up in its
store's public catalog through the storefront plugins (the search Discover
uses, at most 15 seconds) and writes the record the store returns, so
product ids, cover art and descriptions match the store. A title it cannot
find keeps built-in fields under a made-up `demo-…` id. The accounts hold no
tokens. It writes through the
library code of the same commit, never raw SQL, and its test runs against a
fresh database in CI, so a schema change that breaks it fails there.

## How a session works

1. The launch page (`GET /`) asks for a Turnstile check. `POST /_demo/start`
   verifies it with Siteverify (hostname and action must match), then admits
   the session against the limits below and records it in D1.
2. The visitor's `DemoSession` Durable Object (`src/session.ts`) starts the
   container with a fresh operator token and `master.key` password, and the
   demo's own origin as bookclerkd's `public_origin` (so bookclerkd's own
   cross-site checks and store sign-in callbacks use it), waits for
   `/health`, approves and deploys the four storefronts as the operator
   (as an operator would in Settings; visitors cannot), makes the visitor
   the host's Owner and redeems the Owner's claim ticket. The
   Durable Object then forgets the operator token. The visitor waits on
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
     scans only the visitor's store accounts;
   - rewrites `Set-Cookie` so the container cannot set the Worker's cookies,
     and adds a banner with the time left and a **Start over** button.

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
commit ships. It adds the Audible, Libro.fm, Chirp and GraphicAudio plugins,
and removes `bookclerk-media-worker` and the `local` output plugin, so the
host has nothing to acquire with even if a request got through. Plugins run
with `isolation = "best-effort"` because the Containers kernel has no
Landlock; this is a demo-only exception (each visitor's VM is the boundary)
and shipped builds keep `"required"`. Media isolation stays `"required"`.

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
each published release, nightly, and on demand; a nightly or release run
stops early when the live demo already runs that commit and an earlier run
for it succeeded. It needs the repository
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
website's fonts and the app's mark; a test fails when a copy drifts.

`test/inputs.fuzz.test.ts` replays the seed corpora under `corpus/` and
fuzzes every input boundary against independent oracles (see
[`docs/contributing/fuzzing.md`](../../docs/contributing/fuzzing.md)). `test/routes.test.ts` checks the
route table against the OpenAPI document. CI runs these as `demo_test`.

`package.json` overrides `undici` (a development dependency through
wrangler's `miniflare`, which pins 7.29.1) to 7.30.1 for GHSA-xjj6-f33q-929c,
GHSA-4qv8-2p9j-2685 and GHSA-f72v-6w45-q472. Drop the override once wrangler
ships a `miniflare` with the fix.
