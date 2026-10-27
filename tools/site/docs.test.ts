// Tests for docs.ts, render.ts, last-updated.ts and the Copy button in
// src/docs.js: URLs, link rewriting, copied commands and every doc in docs/
// rendered the way the Worker renders it. Run with `npm test`.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { fileURLToPath, URL } from "node:url";
import { runInNewContext } from "node:vm";

import { docUrl, GENERATED_TITLES, markdownFiles, renderDocs, rewriteLink, siteMarkdown, slugify } from "./docs.ts";
import { highlight } from "./highlight.ts";
import { formatDate, pageDate, parseIsoDate } from "./last-updated.ts";
import type { IsoDate } from "./last-updated.ts";
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

test("highlight colors each language's own names and leaves the rest plain", () => {
  assert.equal(highlight("[a] # see [b]", "toml"), '<span class="hl-k">[a]</span> <span class="hl-c"># see [b]</span>');
  assert.equal(highlight("[[settings]]", "toml"), '<span class="hl-k">[[settings]]</span>');
  assert.equal(highlight("k = 'C:\\' # path", "toml"), '<span class="hl-v">k</span> = <span class="hl-s">&#39;C:\\&#39;</span> <span class="hl-c"># path</span>');
  assert.equal(
    highlight("for x in a; do echo ${HOME}; done", "bash"),
    '<span class="hl-k">for</span> x <span class="hl-k">in</span> a; <span class="hl-k">do</span> <span class="hl-f">echo</span> <span class="hl-v">${HOME}</span>; <span class="hl-k">done</span>',
  );
  assert.equal(highlight("$ cargo dev --optional", "console"), '$ <span class="hl-f">cargo</span> dev <span class="hl-v">--optional</span>');
  // In a terminal session only prompt lines and their continuations are highlighted; output stays plain.
  assert.equal(
    highlight("$ cargo build \\\n  --release | tee log\n   Compiling foo v1\nok | done", "console"),
    '$ <span class="hl-f">cargo</span> build \\\n  <span class="hl-v">--release</span> | <span class="hl-f">tee</span> log\n   Compiling foo v1\nok | done',
  );
  // `\\` at the end of a line is a literal backslash, so the next line is output.
  assert.equal(highlight("$ printf '%s\\n' \\\\\ncargo --release", "console"), '$ <span class="hl-f">printf</span> <span class="hl-s">&#39;%s\\n&#39;</span> <span class="hl-n">\\\\</span>\ncargo --release');
  // A `\` inside a comment does not continue the command either.
  assert.equal(highlight("$ make # build \\\n  echo hi", "console"), '$ <span class="hl-f">make</span> <span class="hl-c"># build \\</span>\n  echo hi');
  assert.equal(highlight("Compiling foo\ncargo dev", "bash"), '<span class="hl-f">Compiling</span> foo\n<span class="hl-f">cargo</span> dev');
  assert.equal(highlight("- name: x\n  key: value", "yaml"), '- <span class="hl-v">name</span>: x\n  <span class="hl-v">key</span>: value');
  assert.equal(highlight('{"a": "b", "n": 1}', "json"), '{<span class="hl-v">&quot;a&quot;</span>: <span class="hl-s">&quot;b&quot;</span>, <span class="hl-v">&quot;n&quot;</span>: <span class="hl-n">1</span>}');
  assert.equal(highlight("x", "text"), null);
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

test("page dates are real calendar dates, shown as month, day and year", () => {
  for (const ok of ["2026-10-26", "2024-02-29", "2000-02-29", "0001-01-01", "9999-12-31"]) assert.equal(parseIsoDate(ok), ok, ok);
  for (const bad of ["2026-02-29", "1900-02-29", "2026-13-01", "2026-00-10", "2026-04-31", "2026-10-00", "0000-01-01", "2026-1-01", "26-10-26", " 2026-10-26", "2026-10-26T00:00Z", "２０２６-10-26", ""]) {
    assert.equal(parseIsoDate(bad), null, bad);
  }
  assert.equal(formatDate("2026-10-26" as IsoDate), "Oct 26, 2026");
  assert.equal(formatDate("2026-01-05" as IsoDate), "Jan 5, 2026");
  assert.equal(formatDate("0099-12-31" as IsoDate), "Dec 31, 0099");
});

test("pageDate reads one page's date and ignores anything malformed", () => {
  const file = JSON.stringify({ version: 1, pages: { README: "2026-10-26", "guide/x": "2026-02-30", "guide/y": 20261026 } });
  assert.equal(pageDate(file, "README"), "2026-10-26");
  for (const key of ["guide/x", "guide/y", "missing", "__proto__", "constructor", "toString"]) assert.equal(pageDate(file, key), null, key);
  for (const bad of ["", "{", "null", "[]", '"x"', '{"version":2,"pages":{"README":"2026-10-26"}}', '{"version":"1","pages":{"README":"2026-10-26"}}', '{"version":1,"pages":[]}', '{"version":1}']) {
    assert.equal(pageDate(bad, "README"), null, bad);
  }
  assert.equal(pageDate('{"version":1,"pages":{"__proto__":"2026-10-26"}}', "__proto__"), "2026-10-26");
});

/**
 * Runs src/docs.js against a stub page holding one code block, clicks its
 * Copy button and reports what reached the clipboard.
 *
 * @param text - The code block's text, as the browser reports it.
 * @returns The copied text.
 */
async function copied(text: string): Promise<string> {
  let clipboard = "";
  let click: (() => void) | null = null;
  const button = { setAttribute: () => {}, addEventListener: (_: string, f: () => void) => (click = f) };
  const block = { querySelector: () => ({ innerText: text }), append: () => {} };
  const context = {
    navigator: { clipboard: { writeText: (value: string) => ((clipboard = value), Promise.resolve()) } },
    document: {
      querySelectorAll: (selector: string) => (selector === ".doc .code" ? [block] : []),
      querySelector: () => null,
      createElement: () => button,
      addEventListener: () => {},
    },
    window: { addEventListener: () => {}, clearTimeout: () => {}, setTimeout: () => 0 },
  };
  runInNewContext(readFileSync(fileURLToPath(new URL("src/docs.js", import.meta.url)), "utf8"), context);
  assert.ok(click !== null, "no Copy button");
  (click as () => void)();
  await Promise.resolve();
  return clipboard;
}

test("Copy takes a session's commands and the lines that continue them", async () => {
  assert.equal(await copied("cargo dev\nls\n"), "cargo dev\nls");
  assert.equal(await copied("$ cargo build \\\n  --release\n   Compiling foo\n$ ls"), "cargo build \\\n  --release\nls");
  // `\\` at the end is a literal backslash: the next line is output.
  assert.equal(await copied("$ printf '%s\\n' \\\\\ncargo --release"), "printf '%s\\n' \\\\");
  // A `\` in a comment does not continue, also after `;`; a quoted `#` is not a comment.
  assert.equal(await copied("$ make # build \\\n  echo hi"), "make # build \\");
  assert.equal(await copied("$ echo ok;# note \\\noutput"), "echo ok;# note \\");
  assert.equal(await copied('$ echo "a # b" \\\n  more\nout'), 'echo "a # b" \\\n  more');
  assert.equal(await copied("$ echo 'a \\' x\nout"), "echo 'a \\' x");
});

test("GitHub alert blockquotes render as callouts, other blockquotes stay quotes", () => {
  const html = (source: string): string => renderDoc(source, "cluster-hosts", { blobBase: BLOB }).html;
  assert.equal(
    html("> [!WARNING]\n> Leave **Auto acquire** off.\n"),
    '<div class="callout callout-warning" role="note"><p class="callout-title">Warning</p>\n<p>Leave <strong>Auto acquire</strong> off.</p>\n</div>\n',
  );
  // The kind is read without regard to case, and a marker alone still renders.
  assert.match(html("> [!tip]\n"), /^<div class="callout callout-tip" role="note"><p class="callout-title">Tip<\/p>\n<\/div>\n$/);
  // A second paragraph and a nested quote stay inside the callout.
  const nested = html("> [!NOTE]\n> One.\n>\n> > Quoted.\n\nAfter.\n");
  assert.match(nested, /^<div class="callout callout-note"[^>]*><p class="callout-title">Note<\/p>\n<p>One.<\/p>\n<blockquote>\n<p>Quoted.<\/p>\n<\/blockquote>\n<\/div>\n<p>After.<\/p>\n$/);
  // Not a marker: an unknown kind, text after the marker, or no marker at all.
  for (const source of ["> [!DANGER]\n> x\n", "> [!NOTE] inline\n", "> plain\n"]) {
    assert.match(html(source), /^<blockquote>/, source);
  }
});
