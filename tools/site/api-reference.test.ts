// Tests for api-reference.ts: how the Web API reference's operation pages
// are laid out (method badges, two columns, status marks, sidebar list).
// Run with `npm test`.

import assert from "node:assert/strict";
import { test } from "node:test";

import { isApiOperationsPage, statusClass } from "./api-reference.ts";
import { apiNavNested, navHtml, renderDoc, tocListHtml } from "./render.ts";

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
  assert.match(out.html, /^<div class="api-ref">/);
  assert.equal(out.html.split('<section class="api-op"').length, 3);
  assert.equal(out.html.split("</section>").length, 3);
  assert.match(
    out.html,
    /<section class="api-op" aria-labelledby="get-apijobs">\n<h2 id="get-apijobs" class="api-op-title"><span class="api-method api-method-get">GET<\/span> <code>\/api\/jobs<\/code><\/h2>/,
  );
  const first = out.html.slice(out.html.indexOf('aria-labelledby="get-apijobs"'), out.html.indexOf('aria-labelledby="post-apijobsidcancel"'));
  const main = first.indexOf('<div class="api-op-main">');
  const examples = first.indexOf('<div class="api-op-examples">');
  assert.ok(main !== -1 && examples > main, "the examples follow the main column");
  assert.ok(first.indexOf("Responses") < examples && first.indexOf("Example request") > examples);
  assert.ok(!out.html.slice(out.html.indexOf("post-apijobsidcancel")).includes("api-op-examples"), "no empty examples column");
});

test("status codes in a Responses table are marked by class", () => {
  const out = renderDoc(PAGE, "api/web/jobs", { blobBase: BLOB });
  assert.ok(out.html.includes('<td><span class="api-status api-status-2xx">200</span></td>'));
  assert.ok(out.html.includes('<td><span class="api-status api-status-4xx">401</span></td>'));
  assert.ok(!out.html.includes('<span class="api-status api-status-2xx">id'), "other tables are left alone");
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
  const nav = navHtml([{ title: "API reference", pages: [{ key: "api/web/README", title: "Web API" }] }], "api/web/jobs", "/docs/", nested);
  assert.ok(nav.startsWith('<details class="nav-group" open>'), "the group opens");
  assert.ok(nav.includes('<a href="/docs/api/web/">Web API</a><ul class="nav-sub"><li><a href="/docs/api/web/jobs" aria-current="page">Jobs</a>'));
  assert.ok(nav.includes('<a class="nav-op" href="#get-apijobs"><span class="api-method api-method-get">GET</span> <code>/<wbr>api/<wbr>jobs</code></a>'));
  assert.equal(apiNavNested("server/operations", "Running", [], "/docs/"), null);
});

test("odd operation pages still render balanced", () => {
  const parts = ["## `GET /a`\n", "## `BREW /a`\n", "## Not an op\n", "### Example request\n", "### Example response\n", "### Responses\n", "| Status | B |\n| --- | --- |\n| 2XX | x |\n", "text\n", "```json\n{}\n```\n"];
  let state = 7;
  const rand = (): number => {
    state = (state * 1103515245 + 12345) % 2147483648;
    return state / 2147483648;
  };
  for (let i = 0; i < 300; i++) {
    const source = Array.from({ length: 1 + Math.floor(rand() * 12) }, () => parts[Math.floor(rand() * parts.length)] ?? "").join("\n");
    const html = renderDoc(source, "api/web/x", { blobBase: BLOB }).html;
    assert.equal(html.split("<section").length, html.split("</section>").length, source);
    assert.equal(html.split("<div").length, html.split("</div>").length, source);
  }
});
