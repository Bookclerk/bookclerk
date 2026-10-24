# bookclerk.app website

The public website: a landing page for people who want to use Bookclerk, a
download page, and the documentation with its generated API reference under
`/docs/`. It is plain HTML and CSS with no client-side scripts of its own, served by a
Cloudflare Worker named **`bookclerk-site`** on the domain in the `DOMAIN`
organization variable. `build.mjs` writes the static pages; the Worker
(`entry.mjs` → `worker.js`) renders each docs page at request time from the
docs repository [`Bookclerk/bookclerk-docs`](../../docs/ci.md#docs-publishing),
and picks between a page's HTML and its Markdown version.

Deployed by GitHub Actions:
[`.github/workflows/site-deploy.yml`](../../.github/workflows/site-deploy.yml),
on changes to this directory, `docs/` or `assets/brand/` on `main`, on every published
release, and by hand. The deploy token is scoped to the Worker by name, so keep
`name` in `wrangler.toml` as it is.

## Release state

Until the repository has a published release, every page carries a
pre-release notice and the download page says there is no release yet. Once a
release exists, the workflow passes its tag to the build: the notices go, the
download page links to that release, and `/docs/` shows that release's docs
instead of `main`'s.

| Variable | Meaning |
| --- | --- |
| `SITE_RELEASE_TAG` | newest release tag (`v1.2.3`); empty means no release |
| `SITE_RELEASE_DATE` | that release's date, `YYYY-MM-DD` |
| `SITE_DOCS_VERSIONS` | the release tags whose docs bookclerk-docs has tagged, whitespace-separated, newest first, for the docs version picker; needs `SITE_RELEASE_TAG` |
| `SITE_COMMIT` | commit the site is built from |
| `SITE_BUILT_ON` | build date, `YYYY-MM-DD` (default: today) |
| `SITE_ORIGIN` | `https://<host>` the site is served from, for canonical and preview URLs (default `https://bookclerk.app`) |
| `SITE_DEMO_URL` | `https://<host>` of the hosted demo ([`tools/demo`](../demo/README.md)); empty means no demo, and the pages leave out every demo link. The workflow uses `https://demo.<DOMAIN>`, else the `DEMO_URL` variable, and none when `DEMO_URL` is `none` |

## Local preview

```bash
npm ci
npm test
SITE_COMMIT=$(git rev-parse HEAD) npm run build      # writes dist/ and generated/
npx wrangler dev                                       # docs pages need network access to GitHub
```

## Audit

CI runs Lighthouse and axe-core on every pull request that changes the site
(`audit/`, see `docs/ci.md#website-checks`). It serves `dist/` through the
Worker with docs read from this checkout's `docs/`, so it needs no network:

```bash
SITE_COMMIT=$(git rev-parse HEAD) npm run build
cd audit && npm ci && node audit.mjs      # --chrome <path> or CHROME_PATH picks the browser
```

## Layout

- `src/*.html` – pages. `{{name}}` inserts an escaped value, `{{> name}}` a
  partial from `src/partials/`, and `{{#prerelease}}…{{/prerelease}}` /
  `{{#released}}…{{/released}}` keep content for one release state.
  `{{#demo}}` keeps content when the deployment has a demo; sections do not
  nest, so `{{#demolead}}` (demo, no release), `{{#demofollow}}` (demo and a
  release) and `{{#previewlead}}` (no demo, no release) cover the
  combinations, and `{{^flag}}` inverts any of them.
- `src/site.css` – styles; colors and fonts follow `ui/src/index.css` and
  `assets/brand/README.md`, with a dark scheme from `prefers-color-scheme`.
- `src/fonts/` – the web fonts, served from the site itself (no third-party
  requests), with their licenses and the fallback metrics `site.css` uses to
  keep the layout from shifting while they load; see its README.
- Docs pages – see [Docs](#docs) below. `src/doc.html` is their template,
  `render.mjs` renders them (shared by the build and the Worker), and
  `docs.mjs` reads the local `docs/` for the build. `NAV` in `render.mjs`
  orders the sidebar; a doc in `docs/` missing from it is listed under "More".
- `src/index.md`, `src/download.md`, `src/404.md` – Markdown versions of
  those pages, rendered with the same template syntax. Keep them in step with
  the HTML: a test checks that every `<h2>` of the page is a `##` section of
  its Markdown version.
- `worker.js` – the docs route and content negotiation (see below).
  `wrangler.toml` runs it only for page paths (`run_worker_first`). It
  compresses what it answers with (Brotli, else gzip) itself, because
  Cloudflare's edge leaves `text/markdown` uncompressed; files served without
  it are compressed by the edge. Its `Accept-Encoding` parser is an
  untrusted-input boundary: `fuzz.test.mjs` replays the seeds in
  `fuzz/accept_encoding/` and fuzzes mutations of them against an
  independent oracle (`docs/ci.md#fuzzing`).
  `entry.mjs` binds it to `generated/docs-shell.json`, which `npm run build`
  writes (gitignored), so build before `wrangler dev` or `deploy`.
- `src/_headers` – security headers, and Markdown and `llms.txt` content
  types. The CSP allows no script except Cloudflare Web Analytics, which the
  edge injects into each page: `script-src https://static.cloudflareinsights.com`
  for the beacon and `connect-src https://cloudflareinsights.com` beside
  `'self'` for its reports. That beacon is the one approved exception to the
  site's no-third-party-requests rule. `SECURITY_HEADERS` in `worker.js`
  carries the same policy, and a test keeps the two equal.
- Brand assets are copied from `assets/brand/` at build time.

## Docs

The docs are published as Markdown to `Bookclerk/bookclerk-docs` by
`docs-publish.yml`: its `main` follows this repository's `main`, and each
release has a tag of the same name. The tree is `docs/` as written plus the
generated API reference under `api/`. The Worker reads pages from it rather
than deploying them, so the Worker's size does not grow with the number of
versions.

- `/docs/<path>` is the default version: the newest release whose docs are
  published, or `main` before that. A release's docs are tagged by
  `docs-publish` after the release event, and `site-deploy` runs again when
  that finishes. `/docs/v/<ref>/<path>` is any other version
  the site was built with (`main` and every release tag); the default
  version's prefixed URLs redirect to `/docs/<path>`.
- `<path>` is the file's path without `.md`; a directory's `README.md` is
  `<dir>/`. The other spelling and old `.html` URLs redirect, and `<dir>/`
  without a README goes to `<dir>/index` (rustdoc's crate pages).
- Only refs in the version list and paths of plain segments (letters,
  digits, `.`, `_`, `-`, none starting with `.`) are fetched, from
  `raw.githubusercontent.com` with redirects refused (`redirect: "manual"`
  and any 3xx is an error; the Workers runtime throws on
  `redirect: "error"`). Files over 4 MB are refused (also when the body has
  no length and runs past it), and a failing fetch is a 503, never a partial
  page: the "docs could not be loaded" page in the site's look (Markdown for
  Markdown readers), linking to the same doc in the docs repository on
  GitHub, with `Retry-After: 60`, `no-store` and `noindex`.
- Each page is rendered into the shell `build.mjs` bakes from `src/doc.html`
  (`%%name%%` holes take escaped values, `<!--name-->` markers take the
  rendered parts). Links between docs stay in the same version, links to
  other repository files go to GitHub at the version's ref, and raw HTML in
  Markdown is shown as text.
- Caching: GitHub responses and rendered pages are cached at the edge, for
  a day for release tags (which never move), five minutes for `main` and a
  minute for missing files. Rendered pages are keyed by the version's ref
  and a hash of the shell, so a deploy (a new release, sidebar or template)
  starts fresh; browsers keep any docs page for five minutes, since each
  carries the version picker.
- The sidebar, `sitemap.xml` and `llms.txt` come from the `docs/` the site
  is built with (the newest release tag's, else `main`'s), so a new doc
  appears there on the next deploy. Every version shares that sidebar; a
  doc that exists in one version only is still reachable by its links.
- Only the default version is indexed: pages of `main` (once a release
  exists) and of older releases are sent with `X-Robots-Tag: noindex`. The
  picker lists the newest 100 releases.

## Search engines, link previews and agents

- Every page gets a `<title>`, description, canonical link, Open Graph and
  `twitter:card` tags (from `PAGES` in `build.mjs`, or from each doc: its
  introduction, else its section names). The 404 page is `noindex`.
- JSON-LD: the home page describes the site, its organization, the app
  (`SoftwareApplication` with the feature cards as `featureList`), its source
  code and the FAQ (`FAQPage`); each doc is a `TechArticle` with a
  breadcrumb. The features and questions are read from the rendered home page
  (`homeFacts` in `build.mjs`), so structured data never says anything a
  visitor cannot see on the page. Search engines treat text shown only to
  crawlers as cloaking, so keep it that way.
- `sitemap.xml` lists every indexable page with the build date as
  `lastmod`; `robots.txt` allows all crawlers and points to it.
- For agents: each doc is also served as Markdown at `/docs/<path>.md` (and
  `/docs/v/<ref>/<path>.md`)
  (linked from the page with `rel="alternate"` and "View as Markdown"), with
  links between docs pointing at the other Markdown copies. `/llms.txt`
  repeats the home page's features and questions, then indexes them per
  [llmstxt.org](https://llmstxt.org), and `/llms-full.txt`
  holds every doc in one file.
- Content negotiation: a request for `/`, `/download` or any `/docs/` page
  whose `Accept` header ranks `text/markdown` (or `text/x-markdown`) at least
  as high as HTML gets the page's Markdown version, with
  `Link: <page>; rel="canonical"`. Browsers and `*/*` keep getting HTML,
  which carries `Link: <page>.md; rel="alternate"; type="text/markdown"`.
  Page responses say `Vary: Accept`; a missing doc requested as Markdown gets
  `404.md` with status 404. The Markdown version is the site's own copy,
  with links rewritten to the site, not the docs repository's file.

### Social preview image

`src/social-card.png` (1200×630) is rendered from `social-card/card.html`.
After changing the card, open it in a Chromium-based browser with a
1200×630 viewport (for example Playwright's `page.screenshot`) and save the
screenshot over `src/social-card.png`.

## Planned

Planned additions: app screenshots captured from each release (replacing the
illustration in the tour section).
