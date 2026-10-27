// Tests for docs.ts and render.ts: URLs, link rewriting and every doc in
// docs/ rendered the way the Worker renders it. Run with `npm test`.

import assert from "node:assert/strict";
import { test } from "node:test";
import { fileURLToPath, URL } from "node:url";

import { docUrl, GENERATED_TITLES, markdownFiles, renderDocs, rewriteLink, siteMarkdown, slugify } from "./docs.ts";
import { crumbsHtml, docMarkdownUrl, fillShell, navHtml, navPlace, normalizePath, pagerHtml, renderDoc, SHELL_MARKERS, tocListHtml } from "./render.ts";

/** The repository's docs/. */
const DOCS = fileURLToPath(new URL("../../docs/", import.meta.url));

/** GitHub prefix used by the link tests. */
const BLOB = "https://github.com/Bookclerk/bookclerk/blob/main/";

/** Docs that exist in the link tests. */
const KEYS = new Set(["README", "plugins", "media", "adr/control-plane", "api/README"]);

test("doc URLs, in the default version and under a version prefix", () => {
  assert.equal(docUrl("README"), "/docs/");
  assert.equal(docUrl("api/README"), "/docs/api/");
  assert.equal(docUrl("adr/control-plane"), "/docs/adr/control-plane");
  assert.equal(docUrl("plugins", "/docs/v/main/"), "/docs/v/main/plugins");
  assert.equal(docUrl("README", "/docs/v/main/"), "/docs/v/main/");
  assert.equal(docMarkdownUrl("README", "/docs/v/v0.1.0/"), "/docs/v/v0.1.0/README.md");
});

test("normalizePath matches path.posix.normalize", () => {
  assert.equal(normalizePath("docs/adr/../plugins.md"), "docs/plugins.md");
  assert.equal(normalizePath("docs/./a/"), "docs/a/");
  assert.equal(normalizePath("docs/../../x"), "../x");
  assert.equal(normalizePath("docs/.."), ".");
});

test("slugify matches GitHub heading anchors", () => {
  assert.equal(slugify("The guest jail"), "the-guest-jail");
  assert.equal(slugify("`plugin.toml` [[settings]]"), "plugintoml-settings");
  assert.equal(slugify("Step 1: build (Linux)"), "step-1-build-linux");
});

test("links between docs stay on the site", () => {
  assert.equal(rewriteLink("media.md", "plugins", KEYS, BLOB), "/docs/media");
  assert.equal(rewriteLink("plugins.md#the-guest-jail", "media", KEYS, BLOB), "/docs/plugins#the-guest-jail");
  assert.equal(rewriteLink("../plugins.md", "adr/control-plane", KEYS, BLOB), "/docs/plugins");
  assert.equal(rewriteLink("adr/control-plane.md", "README", KEYS, BLOB), "/docs/adr/control-plane");
  assert.equal(rewriteLink("api/README.md", "README", KEYS, BLOB), "/docs/api/");
});

test("links to other repository files go to GitHub at the same ref", () => {
  assert.equal(rewriteLink("../AGENTS.md", "README", KEYS, BLOB), `${BLOB}AGENTS.md`);
  assert.equal(rewriteLink("../crates/x/src/lib.rs#L10", "plugins", KEYS, BLOB), `${BLOB}crates/x/src/lib.rs#L10`);
  assert.equal(rewriteLink("../examples/echo/", "plugins", KEYS, BLOB), "https://github.com/Bookclerk/bookclerk/tree/main/examples/echo/");
  assert.equal(rewriteLink("missing.md", "plugins", KEYS, BLOB), `${BLOB}docs/missing.md`);
});

test("without a list of docs, any .md inside the docs tree is a doc", () => {
  assert.equal(rewriteLink("api/rust/x/index.md#fn", "README", null, BLOB), "/docs/api/rust/x/index#fn");
  assert.equal(rewriteLink("../AGENTS.md", "README", null, BLOB), `${BLOB}AGENTS.md`);
  assert.equal(rewriteLink("%E0%A4%A.md", "README", null, BLOB), "%E0%A4%A.md");
  assert.equal(rewriteLink("media.md", "plugins", null, BLOB, (k) => docUrl(k, "/docs/v/main/")), "/docs/v/main/media");
});

test("absolute links and anchors are left alone", () => {
  for (const href of ["https://example.com/a.md", "mailto:contact@bookclerk.app", "#local", "//cdn.example/x"]) {
    assert.equal(rewriteLink(href, "plugins", KEYS, BLOB), href);
  }
});

test("every doc renders, and every link between docs resolves", () => {
  const sources = markdownFiles(DOCS);
  assert.ok(sources.length > 30);
  // /docs/v/<ref>/ selects a version, so no doc may live at docs/v.
  assert.deepEqual(sources.filter((f) => f === "v.md" || f.startsWith("v/")), []);
  const { nav, docs } = renderDocs(DOCS, BLOB, GENERATED_TITLES);
  assert.equal(docs.length, sources.length);
  const ids = new Map(docs.map((d) => [docUrl(d.key), new Set([...d.html.matchAll(/ id="([^"]+)"/g)].map((m) => m[1]))]));
  // Generated pages are not in docs/; links to them are checked by the docs build.
  const generated = new Set(Object.keys(GENERATED_TITLES).map((k) => docUrl(k)));
  const broken: string[] = [];
  for (const doc of docs) {
    for (const [, href = ""] of doc.html.matchAll(/href="(\/docs\/[^"]*)"/g)) {
      const [path = "", hash] = href.split("#");
      if (generated.has(path)) continue;
      const anchors = ids.get(path);
      if (anchors === undefined) broken.push(`${doc.key} -> ${href}`);
      else if (hash && !anchors.has(hash)) broken.push(`${doc.key} -> ${href} (no anchor)`);
    }
  }
  assert.deepEqual(broken, []);
  // Every doc is in the sidebar, the index aside.
  const listed = new Set(nav.flatMap((g) => g.pages.map((p) => p.key)));
  assert.deepEqual(docs.map((d) => d.key).filter((k) => k !== "README" && !listed.has(k)), []);
});

test("raw HTML in a doc is shown as text", () => {
  const out = renderDoc("Let **<branch>** join <script>alert(1)</script>", "cluster-hosts", { blobBase: BLOB });
  assert.match(out.html, /&lt;branch&gt;/);
  assert.ok(!out.html.includes("<script"));
});

test("code blocks get syntax colors and a language label when highlight.ts knows the language", () => {
  const out = renderDoc("```toml\n[library]\nauto_acquire = false # off\n```\n\n```text\n<plain>\n```\n", "cluster-hosts", { blobBase: BLOB });
  assert.match(out.html, /<div class="code" data-lang="toml"><pre tabindex="0"><code class="language-toml"><span class="hl-k">\[library\]<\/span>/);
  assert.match(out.html, /<span class="hl-v">auto_acquire<\/span> = <span class="hl-n">false<\/span> <span class="hl-c"># off<\/span>/);
  assert.match(out.html, /<div class="code"><pre tabindex="0"><code class="language-text">&lt;plain&gt;/);
});

test("a doc's section headings are collected for the On this page list", () => {
  const out = renderDoc("# Title\n\n## First `step`\n\n### Detail\n\n#### Deeper\n\n## First step\n", "cluster-hosts", { blobBase: BLOB });
  assert.deepEqual(out.headings, [
    { level: 2, id: "first-step", text: "First step" },
    { level: 3, id: "detail", text: "Detail" },
    { level: 2, id: "first-step-1", text: "First step" },
  ]);
  assert.equal(tocListHtml(out.headings), '<ul><li><a href="#first-step">First step</a></li><li class="sub"><a href="#detail">Detail</a></li><li><a href="#first-step-1">First step</a></li></ul>');
  assert.equal(tocListHtml([]), "");
});

test("the sidebar opens only the current group and places a doc for its breadcrumb and neighbours", () => {
  const nav = [
    { title: "Use", pages: [{ key: "guide/a", title: "A" }, { key: "guide/b", title: "B <b>" }] },
    { title: "Run", pages: [{ key: "server/c", title: "C" }] },
  ];
  const html = navHtml(nav, "guide/b", "/docs/");
  assert.match(html, /<details class="nav-group" open><summary>Use<\/summary>/);
  assert.match(html, /<details class="nav-group"><summary>Run<\/summary>/);
  assert.match(html, /<a href="\/docs\/guide\/b" aria-current="page">B &lt;b&gt;<\/a>/);
  assert.deepEqual(navPlace(nav, "guide/b"), { group: "Use", previous: { key: "guide/a", title: "A" }, next: { key: "server/c", title: "C" } });
  assert.deepEqual(navPlace(nav, "guide/a").previous, null);
  assert.deepEqual(navPlace(nav, "README"), { group: null, previous: null, next: null });
  assert.equal(pagerHtml(navPlace(nav, "README"), "/docs/"), "");
  assert.match(pagerHtml(navPlace(nav, "server/c"), "/docs/"), /<a class="pager-prev" rel="prev" href="\/docs\/guide\/b"><span>Previous<\/span>B &lt;b&gt;<\/a>/);
  assert.match(crumbsHtml(navPlace(nav, "server/c"), "C & D", "/docs/"), /<li><a href="\/docs\/">Docs<\/a><\/li><li>Run<\/li><li aria-current="page">C &amp; D<\/li>/);
});

test("code blocks and tables can be scrolled from the keyboard", () => {
  const out = renderDoc("```sh\ncargo build\n```\n\n    indented\n\n| a | b |\n| - | - |\n| 1 | 2 |\n", "cluster-hosts", { blobBase: BLOB });
  assert.equal(out.html.match(/<pre tabindex="0">/g)?.length, 2);
  assert.ok(!out.html.includes("<pre>"));
  assert.ok(out.html.includes('<div class="table-scroll" tabindex="0"><table>'));
});

test("fillShell escapes values, fills markers once and refuses gaps", () => {
  const shell = `<title>%%pageTitle%%</title>${SHELL_MARKERS.map((m) => `<!--${m}-->`).join("")}`;
  const markers = { "doc-body": "<p>%%pageTitle%% <!--doc-footer--></p>" };
  const out = fillShell(shell, { pageTitle: "<a> & b" }, markers);
  // Text inside a marker's HTML is never read as a hole or a marker.
  assert.equal(out, "<title>&lt;a&gt; &amp; b</title><p>%%pageTitle%% <!--doc-footer--></p>");
  assert.throws(() => fillShell("%%nope%%<!--jsonld-->", {}, {}), /nope/);
  assert.throws(() => fillShell("%%pageTitle%%", { pageTitle: "x" }, {}), /<!--jsonld-->/);
});

test("Markdown copies rewrite whole link targets outside code", () => {
  const md = [
    "See [media](media.md#codecs \"Media\") and [dir](../examples/echo/) and [file](../examples/echo/plugin.toml).",
    "Inline `[x](media.md)` stays.",
    "```md",
    "[y](media.md)",
    "```",
    "[ref]: plugins.md",
  ].join("\n");
  const out = siteMarkdown(md, "plugins", KEYS, BLOB).split("\n");
  assert.equal(
    out[0],
    'See [media](/docs/media.md#codecs "Media") and [dir](https://github.com/Bookclerk/bookclerk/tree/main/examples/echo/) and [file](https://github.com/Bookclerk/bookclerk/blob/main/examples/echo/plugin.toml).',
  );
  assert.equal(out[1], "Inline `[x](media.md)` stays.");
  assert.equal(out[3], "[y](media.md)");
  assert.equal(out[5], "[ref]: /docs/plugins.md");
});
