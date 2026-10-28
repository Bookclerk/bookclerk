// Tests for api-reference.ts: how the Web API reference's operation pages
// are laid out (method badges, two columns, status marks, sidebar list).
// Run with `npm test`.

import assert from "node:assert/strict";
import { test } from "vitest";

import { isApiOperationsPage, routeTree, statusClass } from "./api-reference.ts";
import {
  apiNavNested,
  apiRouteGroups,
  crumbsHtml,
  crumbTrail,
  docJsonLd,
  navHtml,
  navParent,
  renderDoc,
  searchPage,
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
    // Paths are relative to the prefix every route shares; the headings show it.
    '<ul><li class="depth-0"><a class="toc-op" href="#get-apijobs"><span class="api-method api-method-get">GET</span> <code>/<wbr></code></a></li>' +
      '<li class="depth-0"><a class="toc-op" href="#post-apijobsidcancel"><span class="api-method api-method-post">POST</span> <code>/<wbr>{id}/<wbr>cancel</code></a></li></ul>',
  );
});

test("the sidebar lists the page under Web API, and its routes go to the rail", () => {
  const out = renderDoc(PAGE, "api/web/jobs", { blobBase: BLOB });
  const nested = apiNavNested("api/web/jobs", out.title, "/docs/");
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
      '<a href="/docs/api/web">Web API</a><ul class="nav-sub"><li><a href="/docs/api/web/jobs" aria-current="page">Jobs</a>',
    ),
  );
  assert.ok(!nav.includes("#get-apijobs"), "no routes in the sidebar");
  assert.ok(
    tocListHtml(out.headings).includes(
      '<a class="toc-op" href="#get-apijobs"><span class="api-method api-method-get">GET</span> <code>/<wbr></code></a>',
    ),
  );
  assert.equal(apiNavNested("server/operations", "Running", "/docs/"), null);
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
    '<nav class="crumbs" aria-label="Breadcrumb"><ol><li><a href="/docs">Docs</a></li><li><a href="/docs/api">API reference</a></li>' +
      '<li><a href="/docs/api/web">Web API</a></li><li><a href="/docs/api/web/jobs" aria-current="page">Jobs</a></li></ol></nav>',
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
      '{"@type":"ListItem","position":2,"name":"API reference","item":"https://x/docs/api"}',
    ),
  );
  assert.ok(
    graph.includes(
      '{"@type":"ListItem","position":3,"name":"Web API","item":"https://x/docs/api/web"}',
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
  const nested = apiNavNested("api/web/schemas", "Schemas", "/docs/");
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
  const nested = apiNavNested("api/web/README", out.title, "/docs/", out.routes);
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
      '<a href="/docs/api/web" aria-current="page">Web API</a><ul class="nav-sub"><li><a href="/docs/api/web/jobs">Jobs</a></li>' +
        '<li><a href="/docs/api/web/health">Health</a></li><li><a href="/docs/api/web/schemas">Schemas</a></li></ul>',
    ),
  );
  assert.ok(!nav.includes("nav-ops"));
  assert.equal(apiNavNested("api/web/README", out.title, "/docs/", []), null);
  // On a group's page that group is marked; its routes are in the rail.
  const onJobs = apiNavNested("api/web/jobs", "Jobs", "/docs/", out.routes)?.html ?? "";
  assert.ok(
    onJobs.startsWith(
      '<ul class="nav-sub"><li><a href="/docs/api/web/jobs" aria-current="page">Jobs</a></li>',
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

test("a route group's routes nest by path in the rail", () => {
  const ops = [
    ["GET", "/api/auth/totp"],
    ["DELETE", "/api/auth/totp"],
    ["POST", "/api/auth/totp/login"],
    ["POST", "/api/auth/totp/enroll/begin"],
    ["POST", "/api/auth/totp/enroll/finish"],
    ["GET", "/api/auth/oidc/config"],
    ["PUT", "/api/auth/oidc/config"],
    ["GET", "/api/auth/oidc/providers"],
    ["POST", "/api/auth/a/b/c/d/e"],
    ["POST", "/api/auth/a/b/c/d/f"],
    ["GET", "/api/auth/a"],
    ["POST", "/api/auth/login"],
  ] as const;
  const tree = routeTree(ops.map(([method, path]) => ({ method, path, id: `${method}${path}` })));
  const lines = tree.map(
    (e) => `${"  ".repeat(e.depth)}${e.kind === "op" ? `${e.method} ` : ""}${e.label}`,
  );
  // The shared `/api/auth` is left out: the page's headings show it.
  assert.deepEqual(lines, [
    "GET /totp",
    "DELETE /totp",
    "  POST /login",
    "  /enroll",
    "    POST /begin",
    "    POST /finish",
    "/oidc",
    "  GET /config",
    "  PUT /config",
    "  GET /providers",
    // Below the deepest level, routes keep their longer remainder.
    "GET /a",
    "  /b/c/d",
    "    POST /e",
    "    POST /f",
    "POST /login",
  ]);
  // Every operation is listed once, whatever its depth.
  assert.equal(tree.filter((e) => e.kind === "op").length, ops.length);
  assert.deepEqual(routeTree([{ method: "GET", path: "/health", id: "h" }]), [
    { kind: "op", method: "GET", id: "h", label: "/health", depth: 0 },
  ]);
  assert.deepEqual(
    routeTree([{ method: "GET", path: "/", id: "r" }]).map((e) => e.label),
    ["/"],
  );
  // A route at the shared prefix reads `/`; routes sharing one path keep it.
  assert.deepEqual(
    routeTree([
      { method: "GET", path: "/api/jobs", id: "a" },
      { method: "POST", path: "/api/jobs/{id}/cancel", id: "b" },
    ]).map((e) => e.label),
    ["/", "/{id}/cancel"],
  );
  assert.deepEqual(
    routeTree([
      { method: "GET", path: "/api/me", id: "a" },
      { method: "PUT", path: "/api/me", id: "b" },
    ]).map((e) => e.label),
    ["/me", "/me"],
  );
});

test("example responses become one panel per status with a status mark", () => {
  const page = `# Books

## \`GET /api/books/{id}\`

Returns a book.

### Example response

Status \`200\`: The book.

\`\`\`json
{}
\`\`\`

Status \`404\`: No such book. No body.

Status \`404\`: Again.
`;
  const out = renderDoc(page, "api/web/books", { blobBase: BLOB });
  const id = "get-apibooksid";
  assert.match(
    out.html,
    /<div class="api-responses">\n<div class="heading-wrapper level-h3"><h3 id="example-response"/,
  );
  assert.ok(
    out.html.includes(
      `<div class="api-response" id="${id}-response-200" data-status="200">\n<p class="api-response-status"><span class="api-response-code"><span class="visually-hidden">Status </span><span class="api-status api-status-2xx">200</span>:</span> The book.</p>\n<div class="code`,
    ),
  );
  assert.ok(out.html.includes(`id="${id}-response-404" data-status="404"`));
  assert.match(
    out.html,
    new RegExp(`id="${id}-response-404-\\d+" data-status="404"`),
    "a repeated status gets its own id",
  );
  assert.equal(out.html.split('<div class="api-response"').length, 4);
  const examples = out.html.slice(out.html.indexOf('<div class="api-op-examples">'));
  assert.ok(
    examples.includes('<div class="api-responses">'),
    "the responses sit in the examples column",
  );
});

test("example requests become one panel per language with a label", () => {
  const page = `# Books

## \`GET /api/books/{id}\`

### Example request

\`\`\`console
$ curl 'http://127.0.0.1:8787/api/books/1'
\`\`\`

\`\`\`powershell
Invoke-RestMethod -Uri 'http://127.0.0.1:8787/api/books/1'
\`\`\`

\`\`\`Rust  main
reqwest::get("http://127.0.0.1:8787/api/books/1").await?;
\`\`\`

\`\`\`zig
unknown
\`\`\`

After the samples.

### Example response

Status \`200\`: The book.
`;
  const out = renderDoc(page, "api/web/books", { blobBase: BLOB });
  const id = "get-apibooksid";
  assert.match(
    out.html,
    /<div class="api-requests">\n<div class="heading-wrapper level-h3"><h3 id="example-request"/,
  );
  assert.ok(
    out.html.includes(
      `<div class="api-request" id="${id}-request-console" data-lang="console" data-label="Shell">\n<div class="code" data-lang="console"><pre tabindex="0"><code class="language-console">$ `,
    ),
  );
  assert.ok(
    out.html.includes(
      `<div class="api-request" id="${id}-request-powershell" data-lang="powershell" data-label="PowerShell">\n<div class="code" data-lang="powershell">`,
    ),
    "PowerShell is highlighted",
  );
  assert.ok(
    out.html.includes(`id="${id}-request-rust" data-lang="rust" data-label="Rust">`),
    "the language name is lower-cased and the rest of the info string ignored",
  );
  assert.ok(
    out.html.includes(
      `id="${id}-request-zig" data-lang="zig" data-label="zig">\n<div class="code"><pre`,
    ),
    "an unknown language is labelled by its name",
  );
  assert.equal(out.html.split('<div class="api-request"').length, 5);
  const card = out.html.slice(
    out.html.indexOf('<div class="api-requests">'),
    out.html.indexOf('<div class="api-responses">'),
  );
  assert.ok(
    card.endsWith("</div>\n<p>After the samples.</p>\n</div>\n"),
    "text after the samples stays in the card, which closes before the responses card",
  );
  assert.ok(
    out.html.indexOf('<div class="api-requests">') >
      out.html.indexOf('<div class="api-op-examples">'),
    "the requests sit in the examples column",
  );
});

test("the request and response cards do not nest, whichever comes first", () => {
  const page = `# Books

## \`GET /api/books\`

### Example response

Status \`200\`: Fine.

### Example request

\`\`\`console
$ curl 'http://127.0.0.1:8787/api/books'
\`\`\`
`;
  const out = renderDoc(page, "api/web/books", { blobBase: BLOB });
  const responses = out.html.slice(
    out.html.indexOf('<div class="api-responses">'),
    out.html.indexOf('<div class="api-requests">'),
  );
  assert.ok(
    responses.endsWith("</div>\n</div>\n"),
    "the responses card closes before the requests card",
  );
  assert.ok(
    out.html.includes("</div>\n</div>\n</div>\n</section>"),
    "the requests card closes before the column",
  );
});

test("only the fences directly under the heading become request panels", () => {
  const page = `# Books

## \`GET /api/books\`

### Example request

A note first.

\`\`\`console
$ curl 'http://127.0.0.1:8787/api/books'
\`\`\`

## \`POST /api/books\`

### Example request

- \`\`\`console
  $ curl 'in a list'
  \`\`\`

## \`DELETE /api/books\`

### Example request

\`\`\`console
$ curl 'first'
\`\`\`

\`\`\`console
$ curl 'the same language again'
\`\`\`

\`\`\`
no language
\`\`\`
`;
  const out = renderDoc(page, "api/web/books", { blobBase: BLOB });
  assert.equal(out.html.split('<div class="api-request"').length, 2);
  assert.ok(out.html.includes('id="delete-apibooks-request-console"'));
  assert.equal(out.html.split('<div class="api-requests">').length, 4);
  assert.ok(out.html.includes('<p>A note first.</p>\n<div class="code" data-lang="console">'));
  assert.ok(out.html.includes('<li>\n<div class="code" data-lang="console">'));
});

test("a status line in a list or quote opens no panel", () => {
  const page = `# Books

## \`GET /api/books\`

### Example response

- Status \`200\`: In a list.

> Status \`404\`: In a quote.

Status \`500\`: On its own.
`;
  const out = renderDoc(page, "api/web/books", { blobBase: BLOB });
  assert.equal(out.html.split('<div class="api-response"').length, 2);
  assert.ok(out.html.includes('id="get-apibooks-response-500"'));
  assert.ok(out.html.includes("<li>Status <code>200</code>: In a list.</li>"));
  assert.ok(out.html.includes("<p>Status <code>404</code>: In a quote.</p>\n</blockquote>"));
});

test("paragraphs that are not status lines stay outside the panels", () => {
  const page = `# Books

## \`GET /api/books\`

### Example response

Some note.

Status \`abc\`: not a status.
`;
  const out = renderDoc(page, "api/web/books", { blobBase: BLOB });
  assert.ok(out.html.includes('<div class="api-responses">'));
  assert.ok(!out.html.includes('<div class="api-response"'));
  assert.ok(out.html.includes("<p>Status <code>abc</code>: not a status.</p>"));
});

test("an operations page offers the filter its operations by method and path", () => {
  const page = searchPage(PAGE, "api/web/jobs", "Web API", "Jobs", "/docs/v/main/");
  assert.deepEqual(page, {
    section: "Web API",
    title: "Jobs",
    url: "/docs/v/main/api/web/jobs",
    headings: [
      { text: "GET /api/jobs", url: "/docs/v/main/api/web/jobs#get-apijobs" },
      {
        text: "POST /api/jobs/{id}/cancel",
        url: "/docs/v/main/api/web/jobs#post-apijobsidcancel",
      },
    ],
  });
});
