// Tests for docs.mjs and the rendered /docs/ pages. Run with `npm test`.

import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative, sep } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { build } from "./build.mjs";
import { docFile, docUrl, markdownFiles, rewriteLink, slugify } from "./docs.mjs";

/** GitHub prefix used by the link tests. */
const BLOB = "https://github.com/Bookclerk/bookclerk/blob/main/";

/** Docs that exist in the link tests. */
const KEYS = new Set(["README", "plugins", "media", "adr/control-plane", "api/README"]);

test("doc URLs and files", () => {
  assert.equal(docUrl("README"), "/docs/");
  assert.equal(docUrl("api/README"), "/docs/api/");
  assert.equal(docUrl("adr/control-plane"), "/docs/adr/control-plane");
  assert.equal(docFile("README"), "docs/index.html");
  assert.equal(docFile("api/README"), "docs/api/index.html");
  assert.equal(docFile("plugins"), "docs/plugins.html");
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

test("absolute links and anchors are left alone", () => {
  for (const href of ["https://example.com/a.md", "mailto:contact@bookclerk.app", "#local", "//cdn.example/x"]) {
    assert.equal(rewriteLink(href, "plugins", KEYS, BLOB), href);
  }
});

test("every doc renders, and every link on the site resolves", () => {
  const dir = mkdtempSync(join(tmpdir(), "bookclerk-site-docs-"));
  try {
    build({ SITE_COMMIT: "b6adae4d", SITE_BUILT_ON: "2026-10-23" }, dir);
    const pages = [];
    const walk = (d) => {
      for (const n of readdirSync(d)) {
        const f = join(d, n);
        if (statSync(f).isDirectory()) walk(f);
        else if (n.endsWith(".html")) pages.push(f);
      }
    };
    walk(dir);
    const urlOf = (f) => ("/" + relative(dir, f).split(sep).join("/")).replace(/index\.html$/, "").replace(/\.html$/, "");
    const ids = new Map(pages.map((f) => [urlOf(f), new Set([...readFileSync(f, "utf8").matchAll(/ id="([^"]+)"/g)].map((m) => m[1]))]));

    const sources = markdownFiles(fileURLToPath(new URL("../../docs/", import.meta.url)));
    assert.ok(sources.length > 30);
    assert.equal(pages.filter((f) => urlOf(f).startsWith("/docs/")).length, sources.length);
    for (const source of sources) assert.ok(existsSync(join(dir, "docs", source)), `no Markdown copy of ${source}`);

    const broken = [];
    for (const f of pages) {
      const html = readFileSync(f, "utf8");
      assert.ok(!html.includes("<!--doc-"), `unfilled marker in ${urlOf(f)}`);
      for (const [, href] of html.matchAll(/href="(\/[^"]*)"/g)) {
        const [path, hash] = href.split("#");
        if (/\.(css|ico|png|svg|webmanifest|md|txt|xml)$/.test(path)) {
          if (!existsSync(join(dir, path))) broken.push(`${urlOf(f)} -> ${href} (no file)`);
          continue;
        }
        if (!ids.has(path)) broken.push(`${urlOf(f)} -> ${href}`);
        else if (hash && !ids.get(path).has(hash)) broken.push(`${urlOf(f)} -> ${href} (no anchor)`);
      }
    }
    assert.deepEqual(broken, []);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("raw HTML in a doc is shown as text", () => {
  const dir = mkdtempSync(join(tmpdir(), "bookclerk-site-docs-"));
  try {
    build({ SITE_COMMIT: "b6adae4d", SITE_BUILT_ON: "2026-10-23" }, dir);
    // docs/cluster-hosts.md says "Let **<branch>** join".
    assert.match(readFileSync(join(dir, "docs/cluster-hosts.html"), "utf8"), /&lt;branch&gt;/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
