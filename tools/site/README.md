# bookclerk.app website

The public website: a landing page for people who want to use Bookclerk, a
download page, and the documentation with its generated API reference under
`/docs/`. It is plain HTML and CSS, plus one small script on docs pages
(`src/docs.js`) that the pages work without, served by a
Cloudflare Worker named **`bookclerk-site`** on the domain in the `DOMAIN`
organization variable. `build.ts` writes the static pages; the Worker
(`entry.ts` → `worker.ts`) renders each docs page at request time from the
docs repository [`Bookclerk/bookclerk-docs`](../../docs/contributing/automation.md#docs-publishing),
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
npm run typecheck && npm test
SITE_COMMIT=$(git rev-parse HEAD) npm run build      # writes dist/ and generated/
npx wrangler dev                                       # docs pages need network access to GitHub
```

## Audit

CI runs Lighthouse and axe-core on every pull request that changes the site
(`audit/`, see `docs/contributing/automation.md#website-checks`). It serves `dist/` through the
Worker with docs read from this checkout's `docs/`, so it needs no network:

```bash
SITE_COMMIT=$(git rev-parse HEAD) npm run build
cd audit && npm ci && node audit.ts      # --chrome <path> or CHROME_PATH picks the browser
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
  `render.ts` renders them (shared by the build and the Worker), and
  `docs.ts` reads the local `docs/` for the build. `NAV` in `render.ts`
  orders the sidebar; a doc in `docs/` missing from it is listed under "More".
  The layout follows developers.cloudflare.com's measurements: a slim sticky
  header, a sticky sidebar whose groups fold (only the current page's group
  opens), a reading column of about 80 characters, and an "On this page"
  rail the Worker builds from the doc's `##` and `###` headings, with a
  breadcrumb and previous and next links taken from `NAV`. Below 1200px the
  rail goes; below 900px the sidebar and the rail become two menus over the
  page (`<details>`, so they work without a script).
- `highlight.ts` – syntax colors for fenced code blocks in the languages the
  docs use (shell, TOML, YAML, JSON, TypeScript, Rust, Python, SQL, nginx,
  HTTP), as `<span class="hl-…">` around escaped text; other fences stay
  plain. [Shiki](https://shiki.style/) tokenizes the code with the TextMate
  grammars editors use; a placeholder theme marks each kind of token, and
  `highlight.ts` turns those marks into classes and writes the HTML itself,
  since Shiki's inline `style` colors would break the Content-Security-Policy.
  In a `console` fence only `$ ` prompt lines (and their `\` continuations)
  are colored, and the output lines stay plain. The Worker runs it while
  rendering, so pages need no client-side highlighter.
  It reads doc text, so `fuzz.test.ts` replays `fuzz/highlight/` (first line
  the language, the rest the code) and fuzzes mutations against an oracle
  that the output is only escaped text and those spans and reads back to the
  input exactly.
- `last-updated.ts` – reads `last-updated.json`, which the docs publishing
  workflow writes at the root of each docs version from this repository's
  git history (`scripts/docs-markdown.py`; the generated `api/` pages are
  dated by `scripts/docs-publish.py` from when their content last changed),
  and formats the "Last updated" date under each page's title (also
  `dateModified` in its JSON-LD). The Worker fetches the file beside the
  page, for HTML only; when it is missing or unreadable, the page has no
  date. It also lists every dated page (`datedPages`) for the sitemap. The
  file comes from outside the Worker, so `fuzz.test.ts` replays
  `fuzz/last_updated/` and fuzzes mutations against an independent oracle.
- `sitemap.ts` – writes sitemaps and sitemap indexes, for the build's
  fallback copy and for the Worker's sectioned one (see "Search engines, link previews and agents").
- `diagram.ts` – draws each ```` ```mermaid ```` fence as inline SVG with
  [beautiful-mermaid](https://github.com/lukilabs/beautiful-mermaid), which
  lays diagrams out without a DOM, so pages need no client-side script and
  the docs' Markdown stays what GitHub renders. Flowcharts, state, sequence,
  class and ER diagrams are drawn; any other type, a fence over 4,000
  characters, fences past 12 diagrams or 16,000 characters on one page, a
  diagram with its own colors (`style`, `classDef`, `linkStyle`), or one
  that fails to lay out stays a code block. Each figure is named for its
  kind ("Flowchart") for screen readers. Layout costs CPU (the first one in an
  isolate about a quarter of a second while elkjs starts), so an isolate
  keeps its recent layouts. The SVG is rebuilt from an allowlist of elements
  and attributes (its `<style>` with the font import, tooltips, classes but
  `mono`, and inline styles are dropped, and nothing may name an outside
  resource), its ids get a per-diagram prefix no heading slug can match, and
  `site.css` (`.doc .diagram`) colors it in the site's fonts and light and
  dark colors. A wide diagram shrinks to 70% at most, then scrolls sideways.
  It reads doc text, so `fuzz.test.ts` replays `fuzz/diagram/` and fuzzes
  both mutated sources and mutated renderer output against an independent
  check of the markup; `diagram.test.ts` runs it without Node's `global`,
  as the Worker does.
- `src/docs.js` – the docs pages' only script, served from the site: a Copy
  button on code blocks, the "On this page" link of the section being read
  (`aria-current="location"`), and a filter box above the sidebar groups
  (`/` focuses it). It makes no requests, and everything it enhances works
  without it; Chromium also marks the current section through CSS
  (`scroll-target-group`).
- `src/index.md`, `src/download.md`, `src/privacy.md`, `src/404.md` – Markdown versions of
  those pages, rendered with the same template syntax. Keep them in step with
  the HTML: a test checks that every `<h2>` of the page is a `##` section of
  its Markdown version.
- `src/privacy.html` – the privacy page. It lists every cookie the demo and
  the diagnostics dashboard set; `build.test.ts` reads the cookie names from
  those Workers' sources, so renaming one without updating the page fails.
  Keep it in step when a Worker starts collecting or storing anything new.
- `src/security.html` – the shell of the security page. Its `source` in
  `PAGES` is the repository's root `SECURITY.md`, which the build renders into
  the shell's `<!--source-->` marker and publishes as `/security.md`, so the
  site and GitHub show the same policy. Edit `SECURITY.md`, not the page;
  links to it from the docs point at `/security`.
- `worker.ts` – the docs route, the sitemap and content negotiation (see below).
  `wrangler.toml` runs it only for page paths (`run_worker_first`). It
  compresses what it answers with (Brotli, else gzip) itself, because
  Cloudflare's edge leaves `text/markdown` uncompressed; files served without
  it are compressed by the edge. Its `Accept-Encoding` parser is an
  untrusted-input boundary: `fuzz.test.ts` replays the seeds in
  `fuzz/accept_encoding/` and fuzzes mutations of them against an
  independent oracle (`docs/contributing/fuzzing.md`).
  `entry.ts` binds it to `generated/docs-shell.json`, which `npm run build`
  writes (gitignored), so build before `wrangler dev` or `deploy`.
  `generated/docs-shell.d.json.ts`, checked in, types that file so the
  Worker type-checks before a build.
- TypeScript – every module here is strict TypeScript (`tsconfig.json`).
  Wrangler bundles the Worker from the `.ts` sources and Node (26 in CI,
  22.18 or newer) runs the build and the tests by stripping the types, so
  nothing is compiled; `npm run typecheck` runs `tsc`, and CI runs it before the tests.
- `src/_headers` – security headers, and Markdown and `llms.txt` content
  types. The CSP allows only the site's own scripts (`'self'`, for
  `docs.js`) and Cloudflare Web Analytics, which the edge injects into each
  page: `script-src 'self' https://static.cloudflareinsights.com` and
  `connect-src https://cloudflareinsights.com` beside `'self'` for its
  reports. No inline script is allowed. That beacon is the one approved
  exception to the site's no-third-party-requests rule. `SECURITY_HEADERS` in `worker.ts`
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
- A docs path that names no doc (an old or mistyped link, or a version the
  site does not serve) is a temporary redirect, `no-store`, to that
  version's docs home page, or `/docs/` (its Markdown copy for a `.md`
  path). Only a missing home page itself is a 404.
- Only refs in the version list and paths of plain segments (letters,
  digits, `.`, `_`, `-`, none starting with `.`) are fetched, from
  `raw.githubusercontent.com` with redirects refused (`redirect: "manual"`
  and any 3xx is an error; the Workers runtime throws on
  `redirect: "error"`). Files over 4 MB are refused (also when the body has
  no length and runs past it), and a failing fetch is a 503, never a partial
  page: the "docs could not be loaded" page in the site's look (Markdown for
  Markdown readers), linking to the same doc in the docs repository on
  GitHub, with `Retry-After: 60`, `no-store` and `noindex`. The version's
  `last-updated.json` is fetched the same way (capped at 2 MB), but a
  failure there only leaves out the
  page's date, and that page is kept out of the edge cache so the date
  returns once the docs repository answers.
- Each page is rendered into the shell `build.ts` bakes from `src/doc.html`
  (`%%name%%` holes take escaped values, `<!--name-->` markers take the
  rendered parts). Links between docs stay in the same version, links to
  other repository files go to GitHub at the version's ref, and raw HTML in
  Markdown is shown as text.
- Caching: GitHub responses and rendered pages are cached at the edge, for
  a day for release tags (which never move), five minutes for `main` and a
  minute for missing files. Rendered pages are keyed by the version's ref
  and a hash of the shell, so a deploy (a new release, sidebar, template or Worker)
  starts fresh; browsers keep any docs page for five minutes, since each
  carries the version picker.
- The sidebar, the fallback `sitemap.xml` and `llms.txt` come from the
  `docs/` the site is built with (the newest release tag's, else `main`'s), so a new doc
  appears there on the next deploy. Every version shares that sidebar; a
  doc that exists in one version only is still reachable by its links.
- Only the default version is indexed: pages of `main` (once a release
  exists) and of older releases are sent with `X-Robots-Tag: noindex`. The
  picker lists the newest 100 releases.

## Search engines, link previews and agents

- Every page gets a `<title>`, description, canonical link, Open Graph and
  `twitter:card` tags (from `PAGES` in `build.ts`, or from each doc: its
  introduction, else its section names). The 404 page is `noindex`.
- JSON-LD: the home page describes the site, its organization, the app
  (`SoftwareApplication` with the feature cards as `featureList`), its source
  code and the FAQ (`FAQPage`); each doc is a `TechArticle` with a
  breadcrumb. The features and questions are read from the rendered home page
  (`homeFacts` in `build.ts`), so structured data never says anything a
  visitor cannot see on the page. Search engines treat text shown only to
  crawlers as cloaking, so keep it that way.
- `/sitemap.xml` is a sitemap index with one sitemap per section, so
  Search Console reports indexing per shelf: `/sitemaps/site.xml` holds the
  pages outside `/docs/` and the docs home, and each top docs folder gets
  its own (`/sitemaps/guide.xml`, `/sitemaps/server.xml`,
  `/sitemaps/api.xml`, ...), dated by its newest page. The Worker writes
  them from the default docs version's `last-updated.json`, so together
  they list every page of that version, the generated API reference
  included, each doc with its date as `<lastmod>`. They are cached like
  that version's pages, and a deploy that changes the Worker starts them
  fresh. When the file cannot be read, `/sitemap.xml` is the flat sitemap
  `build.ts` wrote (the pages and the sidebar's docs, no dates) for a
  minute and section sitemaps answer 503. Keys the docs route would not
  serve at their URL are left out, and each sitemap lists at most 50,000
  URLs. `robots.txt` allows all crawlers and points to `/sitemap.xml`.
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
