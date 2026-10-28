// Tests for api-reference.ts: how the Web API reference's operation pages
// are laid out (method badges, two columns, status marks, sidebar list).
// Run with `npm test`.

import assert from "node:assert/strict";
import { test } from "node:test";

import { isApiOperationsPage, statusClass } from "./api-reference.ts";
import {
  apiNavNested,
  apiRouteGroups,
  crumbsHtml,
  crumbTrail,
  docJsonLd,
  navHtml,
  navParent,
  renderDoc,
  tocListHtml,
} from "./render.ts";

/** GitHub prefix for repository links. */
const BLOB = "https://github.com/Bookclerk/bookclerk/blob/main/";

/** A Web API page as scripts/openapi_markdown.py writes it. */
const PAGE = `# Jobs

Queued and running control-plane jobs.

## \`GET /api/jobs\`

Returns jobs, newest first.

### Responses

| Status | Description | Body |
| --- | --- | --- |
| 200 | Jobs. | array of JobInfo |
| 401 | Not signed in. | ApiError |

### Example request

\`\`\`console
$ curl 'http://127.0.0.1:8787/api/jobs'
\`\`\`

### Example response

Status \`200\`:

\`\`\`json
[]
\`\`\`

## \`POST /api/jobs/{id}/cancel\`

Cancels a job.

### Parameters

| Name | In | Type | Required | Description |
| --- | --- | --- | --- | --- |
| \`id\` | path | string | yes | Job id. |
`;

test("only operation pages of the Web API get the layout", () => {
  assert.ok(isApiOperationsPage("api/web/jobs"));
  assert.ok(!isApiOperationsPage("api/web/README"));
  assert.ok(!isApiOperationsPage("api/web/schemas"));
  assert.ok(!isApiOperationsPage("server/operations"));
  const plain = renderDoc(PAGE, "server/operations", { blobBase: BLOB });
  assert.ok(!plain.html.includes("api-op"));
});

test("each operation is a section with a method badge and an examples column", () => {
  const out = renderDoc(PAGE, "api/web/jobs", { blobBase: BLOB });
  assert.match(out.html, /^<h1 id="jobs">Jobs<\/h1>\n<div class="api-ref">/);
  assert.equal(out.html.split('<section class="api-op"').length, 3);
  assert.equal(out.html.split("</section>").length, 3);
  assert.match(
    out.html,
    /<section class="api-op" aria-labelledby="get-apijobs">\n<div class="heading-wrapper level-h2"><h2 id="get-apijobs" class="api-op-title"><span class="api-method api-method-get">GET<\/span> <code>\/api\/jobs<\/code><\/h2><a class="heading-anchor" href="#get-apijobs"/,
  );
  const first = out.html.slice(
    out.html.indexOf('aria-labelledby="get-apijobs"'),
    out.html.indexOf('aria-labelledby="post-apijobsidcancel"'),
  );
  const main = first.indexOf('<div class="api-op-main">');
  const examples = first.indexOf('<div class="api-op-examples">');
  assert.ok(main !== -1 && examples > main, "the examples follow the main column");
  assert.ok(first.indexOf("Responses") < examples && first.indexOf("Example request") > examples);
  assert.ok(
    !out.html.slice(out.html.indexOf("post-apijobsidcancel")).includes("api-op-examples"),
    "no empty examples column",
  );
});

test("status codes in a Responses table are marked by class", () => {
  const out = renderDoc(PAGE, "api/web/jobs", { blobBase: BLOB });
  assert.ok(out.html.includes('<td><span class="api-status api-status-2xx">200</span></td>'));
  assert.ok(out.html.includes('<td><span class="api-status api-status-4xx">401</span></td>'));
  assert.ok(
    !out.html.includes('<span class="api-status api-status-2xx">id'),
    "other tables are left alone",
  );
  assert.equal(statusClass("5XX"), "api-status api-status-5xx");
  assert.equal(statusClass("default"), "api-status");
});

test("the page's headings are its operations, with their methods", () => {
  const out = renderDoc(PAGE, "api/web/jobs", { blobBase: BLOB });
  assert.deepEqual(out.headings, [
    { level: 2, id: "get-apijobs", text: "/api/jobs", method: "GET" },
    { level: 2, id: "post-apijobsidcancel", text: "/api/jobs/{id}/cancel", method: "POST" },
  ]);
  assert.equal(
    tocListHtml(out.headings),
    '<ul><li><a href="#get-apijobs"><span class="api-method api-method-get">GET</span> <code>/<wbr>api/<wbr>jobs</code></a></li>' +
      '<li><a href="#post-apijobsidcancel"><span class="api-method api-method-post">POST</span> <code>/<wbr>api/<wbr>jobs/<wbr>{id}/<wbr>cancel</code></a></li></ul>',
  );
});

test("the sidebar lists the page and its operations under Web API", () => {
  const out = renderDoc(PAGE, "api/web/jobs", { blobBase: BLOB });
  const nested = apiNavNested("api/web/jobs", out.title, out.headings, "/docs/");
  assert.equal(nested?.under, "api/web/README");
  const nav = navHtml(
    [{ title: "API reference", pages: [{ key: "api/web/README", title: "Web API" }] }],
    "api/web/jobs",
    "/docs/",
    nested,
  );
  assert.ok(nav.startsWith('<details class="nav-group" open>'), "the group opens");
  assert.ok(
    nav.includes(
      '<a href="/docs/api/web/">Web API</a><ul class="nav-sub"><li><a href="/docs/api/web/jobs" aria-current="page">Jobs</a>',
    ),
  );
  assert.ok(
    nav.includes(
      '<a class="nav-op" href="#get-apijobs"><span class="api-method api-method-get">GET</span> <code>/<wbr>api/<wbr>jobs</code></a>',
    ),
  );
  assert.equal(apiNavNested("server/operations", "Running", [], "/docs/"), null);
});

test("the breadcrumb of an operations page runs through Web API", () => {
  const nav = [
    {
      title: "API reference",
      pages: [
        { key: "api/README", title: "Generated API reference" },
        { key: "api/web/README", title: "Web API" },
      ],
    },
  ];
  const parent = navParent(nav, "api/web/jobs");
  assert.deepEqual(parent, { key: "api/web/README", title: "Web API" });
  assert.equal(
    crumbsHtml(crumbTrail(nav, "api/web/jobs", "Jobs"), "/docs/"),
    '<nav class="crumbs" aria-label="Breadcrumb"><ol><li><a href="/docs/">Docs</a></li><li><a href="/docs/api/">API reference</a></li>' +
      '<li><a href="/docs/api/web/">Web API</a></li><li><a href="/docs/api/web/jobs" aria-current="page">Jobs</a></li></ol></nav>',
  );
  // The structured data lists the same four crumbs.
  const graph = JSON.stringify(
    docJsonLd(
      "https://x",
      "/docs/",
      { key: "api/web/jobs", title: "Jobs", description: "" },
      crumbTrail(nav, "api/web/jobs", "Jobs"),
    ),
  );
  assert.ok(
    graph.includes(
      '{"@type":"ListItem","position":2,"name":"API reference","item":"https://x/docs/api/"}',
    ),
  );
  assert.ok(
    graph.includes(
      '{"@type":"ListItem","position":3,"name":"Web API","item":"https://x/docs/api/web/"}',
    ),
  );
  assert.ok(
    graph.includes(
      '{"@type":"ListItem","position":4,"name":"Jobs","item":"https://x/docs/api/web/jobs"}',
    ),
  );
  assert.equal(navParent(nav, "api/web/README"), null);
  assert.deepEqual(navParent(nav, "api/web/schemas"), parent);
  assert.equal(navParent(nav, "guide/getting-started"), null);
});

test("the title and its meta line stay ahead of the layout", () => {
  const out = renderDoc(PAGE, "api/web/jobs", { blobBase: BLOB });
  assert.ok(
    out.html.startsWith('<h1 id="jobs">Jobs</h1>\n<div class="api-ref">'),
    out.html.slice(0, 80),
  );
});

test("the schemas page sits under Web API in the sidebar too", () => {
  const nested = apiNavNested("api/web/schemas", "Schemas", [], "/docs/");
  assert.equal(
    nested?.html,
    '<ul class="nav-sub"><li><a href="/docs/api/web/schemas" aria-current="page">Schemas</a></li></ul>',
  );
});

test("only operation subsections leave the page's headings", () => {
  const out = renderDoc(
    "# Jobs\n\n## Paging\n\n### Cursors\n\n## `GET /api/jobs`\n\n### Responses\n",
    "api/web/jobs",
    { blobBase: BLOB },
  );
  assert.deepEqual(
    out.headings.map((h) => h.id),
    ["paging", "cursors", "get-apijobs"],
  );
});

test("odd operation pages still render balanced", () => {
  const parts = [
    "## `GET /a`\n",
    "## `BREW /a`\n",
    "## Not an op\n",
    "### Example request\n",
    "### Example response\n",
    "### Responses\n",
    "| Status | B |\n| --- | --- |\n| 2XX | x |\n",
    "text\n",
    "```json\n{}\n```\n",
  ];
  let state = 7;
  const rand = (): number => {
    state = (state * 1103515245 + 12345) % 2147483648;
    return state / 2147483648;
  };
  for (let i = 0; i < 300; i++) {
    const source = Array.from(
      { length: 1 + Math.floor(rand() * 12) },
      () => parts[Math.floor(rand() * parts.length)] ?? "",
    ).join("\n");
    const html = renderDoc(source, "api/web/x", { blobBase: BLOB }).html;
    assert.equal(html.split("<section").length, html.split("</section>").length, source);
    assert.equal(html.split("<div").length, html.split("</div>").length, source);
  }
});

/** The Web API index as scripts/openapi_markdown.py writes it. */
const INDEX = `# Bookclerk daemon API

The HTTP API of \`bookclerkd\`.

## Quick start

\`\`\`console
$ curl 'http://127.0.0.1:8787/api/status'
\`\`\`

## Authentication

| Name | Kind | Description |
| --- | --- | --- |
| \`operatorToken\` | HTTP bearer | The operator token. |

## Routes

### [Jobs](jobs.md)

Queued and running control-plane jobs.

| Route | Summary |
| --- | --- |
| [\`GET /api/jobs\`](jobs.md#get-apijobs) | Lists jobs. |
| [\`POST /api/jobs/{id}/cancel\`](jobs.md#post-apijobsidcancel) (deprecated) | Cancels a job. |

### [Health](health.md)

| Route | Summary |
| --- | --- |
| [\`GET /health\`](health.md#get-health) | Liveness. |

## Schemas

See [\`GET /api/jobs\`](jobs.md#get-apijobs) and [Schemas](schemas.md).
`;

test("the index puts its Quick start beside the introduction", () => {
  const out = renderDoc(INDEX, "api/web/README", { blobBase: BLOB });
  assert.ok(
    out.html.startsWith(
      '<h1 id="bookclerk-daemon-api">Bookclerk daemon API</h1>\n<div class="api-ref api-index">\n<div class="api-intro">\n<div class="api-intro-main">\n<p>',
    ),
  );
  assert.ok(
    out.html.includes(
      '</div>\n<aside class="api-intro-side" aria-labelledby="quick-start">\n<div class="heading-wrapper level-h2"><h2 id="quick-start">Quick start</h2>',
    ),
  );
  assert.ok(out.html.indexOf("</aside>") < out.html.indexOf('<h2 id="authentication">'));
});

test("each route group on the index is a card whose routes carry method badges", () => {
  const out = renderDoc(INDEX, "api/web/README", { blobBase: BLOB });
  assert.equal(out.html.split('<section class="api-group"').length, 3);
  assert.ok(
    out.html.includes(
      '<section class="api-group" aria-labelledby="jobs">\n<div class="heading-wrapper level-h3"><h3 id="jobs"><a href="/docs/api/web/jobs">Jobs</a></h3>',
    ),
  );
  assert.ok(
    out.html.includes(
      '<a href="/docs/api/web/jobs#post-apijobsidcancel"><span class="api-method api-method-post">POST</span> <code>/<wbr>api/<wbr>jobs/<wbr>{id}/<wbr>cancel</code></a> (deprecated)',
    ),
  );
  // A route link outside the groups stays as it is.
  assert.ok(
    out.html.includes(
      'See <a href="/docs/api/web/jobs#get-apijobs"><code>GET /api/jobs</code></a>',
    ),
  );
  assert.ok(out.html.indexOf("</section>") < out.html.indexOf('<h3 id="health">'));
});

test("the index's sidebar lists every route group with its routes", () => {
  const out = renderDoc(INDEX, "api/web/README", { blobBase: BLOB });
  assert.deepEqual(out.routes, [
    {
      title: "Jobs",
      href: "/docs/api/web/jobs",
      routes: [
        { method: "GET", path: "/api/jobs", href: "/docs/api/web/jobs#get-apijobs" },
        {
          method: "POST",
          path: "/api/jobs/{id}/cancel",
          href: "/docs/api/web/jobs#post-apijobsidcancel",
        },
      ],
    },
    {
      title: "Health",
      href: "/docs/api/web/health",
      routes: [{ method: "GET", path: "/health", href: "/docs/api/web/health#get-health" }],
    },
  ]);
  const nested = apiNavNested("api/web/README", out.title, out.headings, "/docs/", out.routes);
  assert.equal(nested?.under, "api/web/README");
  const nav = navHtml(
    [{ title: "API reference", pages: [{ key: "api/web/README", title: "Web API" }] }],
    "api/web/README",
    "/docs/",
    nested,
  );
  // On the index every group is collapsed, and the schemas page follows them.
  assert.ok(
    nav.includes(
      '<a href="/docs/api/web/" aria-current="page">Web API</a><ul class="nav-sub"><li><a href="/docs/api/web/jobs">Jobs</a></li>' +
        '<li><a href="/docs/api/web/health">Health</a></li><li><a href="/docs/api/web/schemas">Schemas</a></li></ul>',
    ),
  );
  assert.ok(!nav.includes("nav-ops"));
  assert.equal(apiNavNested("api/web/README", out.title, out.headings, "/docs/", []), null);
  // On a group's page only that group opens, on its own operations.
  const jobs = renderDoc(PAGE, "api/web/jobs", { blobBase: BLOB });
  const onJobs =
    apiNavNested("api/web/jobs", jobs.title, jobs.headings, "/docs/", out.routes)?.html ?? "";
  assert.ok(
    onJobs.startsWith(
      '<ul class="nav-sub"><li><a href="/docs/api/web/jobs" aria-current="page">Jobs</a><ul class="nav-ops"><li><a class="nav-op" href="#get-apijobs">',
    ),
  );
  assert.ok(
    onJobs.endsWith(
      '<li><a href="/docs/api/web/health">Health</a></li><li><a href="/docs/api/web/schemas">Schemas</a></li></ul>',
    ),
  );
  assert.deepEqual(apiRouteGroups(INDEX, { blobBase: BLOB }), out.routes);
  assert.deepEqual(renderDoc(PAGE, "api/web/jobs", { blobBase: BLOB }).routes, []);
});

test("an index without a Quick start or groups keeps its text in order", () => {
  const out = renderDoc(
    "# API\n\nIntro.\n\n## Routes\n\n### Plain\n\n- [`GET /x`](x.md)\n",
    "api/web/README",
    { blobBase: BLOB },
  );
  assert.ok(
    out.html.startsWith('<h1 id="api">API</h1>\n<div class="api-ref api-index">\n<p>Intro.</p>'),
  );
  assert.ok(
    !out.html.includes("api-intro") &&
      !out.html.includes("api-group") &&
      !out.html.includes("api-method"),
  );
});

test("section headings link to themselves, after the heading so its name stays its text", () => {
  const out = renderDoc(
    "# Title\n\n## Install it\n\n### `cargo` & co\n\n- ## In a list\n",
    "guide/x",
    { blobBase: BLOB },
  );
  assert.ok(
    out.html.startsWith(
      '<h1 id="title">Title</h1>\n<div class="heading-wrapper level-h2"><h2 id="install-it">Install it</h2>',
    ),
  );
  assert.ok(
    out.html.includes(
      '<a class="heading-anchor" href="#install-it" aria-label="Link to section: Install it"><svg aria-hidden="true"',
    ),
  );
  assert.ok(
    out.html.includes(
      '<h3 id="cargo--co"><code>cargo</code> &amp; co</h3><a class="heading-anchor" href="#cargo--co" aria-label="Link to section: cargo &amp; co">',
    ),
  );
  assert.ok(
    out.html.includes(
      '<li>\n<div class="heading-wrapper level-h2"><h2 id="in-a-list">In a list</h2><a class="heading-anchor" href="#in-a-list"',
    ),
  );
  assert.ok(!out.html.includes('href="#title"'), "the title has no link to itself");
});
