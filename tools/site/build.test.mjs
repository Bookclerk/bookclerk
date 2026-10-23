// Tests for build.mjs: release-state validation, the template engine and the
// rendered pages in both release states. Run with `npm test`.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { build, docsVersions, escapeHtml, inlineLogo, jsonLd, readState, render, templateContext } from "./build.mjs";

/** Release state of a repository with no release yet. */
const PRE = { SITE_COMMIT: "b6adae4d0123456789abcdef0123456789abcdef", SITE_BUILT_ON: "2026-10-23" };

/** Release state after a first release. */
const REL = { ...PRE, SITE_RELEASE_TAG: "v0.1.0", SITE_RELEASE_DATE: "2026-12-01" };

/**
 * Builds the site into a temporary directory.
 *
 * @param {Record<string, string>} env Release state.
 * @returns {{files: string[], page: (name: string) => string, shell: () => object, done: () => void}}
 *   The published files, a reader for one of them, the docs shell and a
 *   clean-up.
 */
function built(env) {
  const root = mkdtempSync(join(tmpdir(), "bookclerk-site-"));
  const dir = join(root, "dist");
  const shellFile = join(root, "docs-shell.json");
  const files = build(env, dir, shellFile);
  return {
    files,
    page: (name) => readFileSync(join(dir, name), "utf8"),
    shell: () => JSON.parse(readFileSync(shellFile, "utf8")),
    done: () => rmSync(root, { recursive: true, force: true }),
  };
}

test("escapeHtml escapes markup and quotes", () => {
  assert.equal(escapeHtml(`<a href="x">'&'</a>`), "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;");
});

test("readState treats a missing tag as no release", () => {
  assert.equal(readState(PRE).release, null);
  assert.equal(readState({ ...PRE, SITE_RELEASE_TAG: "  " }).release, null);
});

test("readState accepts a release tag with its date", () => {
  assert.deepEqual(readState(REL).release, { tag: "v0.1.0", date: "2026-12-01" });
  assert.equal(readState({ ...REL, SITE_RELEASE_TAG: "v1.2.3-rc.1" }).release.tag, "v1.2.3-rc.1");
});

test("readState rejects malformed values", () => {
  for (const env of [
    { ...PRE, SITE_COMMIT: "" },
    { ...PRE, SITE_COMMIT: "main" },
    { ...PRE, SITE_COMMIT: "abc123" },
    { ...PRE, SITE_BUILT_ON: "23 Oct 2026" },
    { ...REL, SITE_RELEASE_TAG: "1.0.0" },
    { ...REL, SITE_RELEASE_TAG: "v1.0.0<script>" },
    { ...REL, SITE_RELEASE_TAG: "v1.0.0\nv2.0.0" },
    { ...REL, SITE_RELEASE_DATE: "" },
    { ...PRE, SITE_ORIGIN: "http://bookclerk.app" },
    { ...PRE, SITE_ORIGIN: "https://bookclerk.app/" },
    { ...PRE, SITE_ORIGIN: "https://Bookclerk.app" },
    { ...PRE, SITE_ORIGIN: "https://bookclerk.app\"><script>" },
  ]) {
    assert.throws(() => readState(env), Error, JSON.stringify(env));
  }
});

test("render fills values, sections and partials", () => {
  const ctx = { values: { name: "<b>" }, flags: { on: true, off: false } };
  const out = render("{{> p}}{{#on}}yes{{/on}}{{#off}}no{{/off}}{{^off}}not off{{/off}}", ctx, { p: "[{{name}}]" });
  assert.equal(out, "[&lt;b&gt;]yesnot off");
});

test("render fails on unknown names instead of leaving holes", () => {
  const ctx = { values: {}, flags: {} };
  assert.throws(() => render("{{missing}}", ctx, {}), /unknown value/);
  assert.throws(() => render("{{#missing}}x{{/missing}}", ctx, {}), /unknown flag/);
  assert.throws(() => render("{{> missing}}", ctx, {}), /unknown partial/);
  assert.throws(() => render("{{ spaced }}", ctx, {}), /unrendered/);
});

test("repository links follow the release tag", () => {
  assert.match(templateContext(readState(PRE)).values.blobBase, /\/blob\/[0-9a-f]{7,40}\/$/);
  assert.match(templateContext(readState(REL)).values.blobBase, /\/blob\/v0\.1\.0\/$/);
});

test("inlineLogo keeps the wordmark recolorable and drops editor attributes", () => {
  const svg = inlineLogo(readFileSync(new URL("../../assets/brand/svg/bookclerk-logo.svg", import.meta.url), "utf8"));
  assert.ok(svg.startsWith('<svg class="logo" '));
  assert.ok(svg.includes('class="bookclerk-wordmark"'));
  assert.ok(svg.includes('aria-labelledby="logo-title"'));
  assert.ok(!svg.includes("bx:"));
  assert.ok(!svg.includes('id="title"'));
});

test("a pre-release build says so on every page", () => {
  const site = built(PRE);
  try {
    for (const name of ["index.html", "download.html", "404.html"]) {
      const html = site.page(name);
      assert.ok(html.includes('<span class="badge">Pre-release</span>'), name);
      assert.ok(html.includes("b6adae4"), name);
      assert.ok(!html.includes("{{"), name);
    }
    assert.ok(site.page("download.html").includes("None yet"));
    assert.ok(site.page("index.html").includes("Try the preview"));
  } finally {
    site.done();
  }
});

test("a release build drops the pre-release notices", () => {
  const site = built(REL);
  try {
    for (const name of ["index.html", "download.html", "404.html"]) {
      assert.ok(!site.page(name).includes("Pre-release"), name);
    }
    assert.ok(site.page("download.html").includes("/releases/tag/v0.1.0"));
    assert.ok(site.page("index.html").includes("Download v0.1.0"));
  } finally {
    site.done();
  }
});

test("the build publishes pages, styles, headers and brand assets", () => {
  const site = built(PRE);
  try {
    for (const name of ["index.html", "download.html", "404.html", "site.css", "_headers", "robots.txt", "sitemap.xml", "llms.txt", "llms-full.txt", "social-card.png", "favicon.ico", "bookclerk-mark.svg", "site.webmanifest"]) {
      assert.ok(site.files.includes(name), name);
    }
    assert.match(site.page("_headers"), /Content-Security-Policy: default-src 'none'/);
  } finally {
    site.done();
  }
});

test("readState defaults the origin and accepts another", () => {
  assert.equal(readState(PRE).origin, "https://bookclerk.app");
  assert.equal(readState({ ...PRE, SITE_ORIGIN: "https://staging.bookclerk.app" }).origin, "https://staging.bookclerk.app");
});

test("jsonLd cannot close its script element", () => {
  const out = jsonLd({ name: "</script><script>alert(1)</script>" });
  assert.equal(out.match(/<\/script>/g).length, 1);
  assert.ok(out.includes("\\u003c/script>"));
  assert.equal(jsonLd(null), "");
});

test("pages carry search and preview metadata", () => {
  const site = built({ ...PRE, SITE_ORIGIN: "https://example.org" });
  try {
    const home = site.page("index.html");
    assert.match(home, /<link rel="canonical" href="https:\/\/example.org\/">/);
    assert.match(home, /<meta property="og:image" content="https:\/\/example.org\/social-card.png">/);
    assert.match(home, /<meta name="twitter:card" content="summary_large_image">/);
    const ld = JSON.parse(home.match(/<script type="application\/ld\+json">(.*?)<\/script>/)[1]);
    assert.equal(ld["@graph"][1]["@type"], "SoftwareApplication");
    assert.ok(!("softwareVersion" in ld["@graph"][1]));
    assert.equal((home.match(/<title>/g) ?? []).length, 1);

    const missing = site.page("404.html");
    assert.match(missing, /<meta name="robots" content="noindex">/);
    assert.ok(!missing.includes('rel="canonical"'));

    // Docs pages are filled in by the Worker (worker.test.mjs checks them).
    const shell = site.shell();
    assert.equal(shell.origin, "https://example.org");
    assert.match(shell.html, /<link rel="canonical" href="https:\/\/example.org%%pagePath%%">/);
    assert.match(shell.html, /<link rel="alternate" type="text\/markdown" href="%%markdownUrl%%"/);
    assert.match(shell.html, /<meta property="og:type" content="article">/);
    assert.ok(!site.files.includes("docs"), "docs pages are not static files");
  } finally {
    site.done();
  }
});

test("a release build states its version in structured data", () => {
  const site = built(REL);
  try {
    const ld = JSON.parse(site.page("index.html").match(/<script type="application\/ld\+json">(.*?)<\/script>/)[1]);
    assert.equal(ld["@graph"][1].softwareVersion, "0.1.0");
    assert.ok(!site.page("index.html").includes("noarchive"));
  } finally {
    site.done();
  }
});

test("crawler and agent files list the site", () => {
  const site = built(PRE);
  try {
    const sitemap = site.page("sitemap.xml");
    assert.ok(sitemap.includes("<loc>https://bookclerk.app/</loc>"));
    assert.ok(sitemap.includes("<loc>https://bookclerk.app/docs/plugins</loc>"));
    assert.ok(!sitemap.includes("/404"));
    assert.match(site.page("robots.txt"), /^Sitemap: https:\/\/bookclerk.app\/sitemap.xml$/m);

    const llms = site.page("llms.txt");
    assert.ok(llms.startsWith("# Bookclerk\n\n> "));
    assert.match(llms, /^- \[[^\]]+\]\(https:\/\/bookclerk.app\/docs\/plugins.md\): \S/m);
    assert.match(llms, /^## Optional$/m);
    assert.ok(llms.includes("unstable development version"));
    assert.match(llms, /^- \[Rust plugin SDK\]\(https:\/\/bookclerk.app\/docs\/api\/rust\/bookclerk_plugin_sdk\/index.md\)$/m);
    const full = site.page("llms-full.txt");
    assert.ok(full.includes("Source: https://bookclerk.app/docs/plugins.md"));
    // Links in the combined file point at the site, never at bare .md paths.
    assert.ok(full.includes("](/docs/"));
    assert.ok(!/\]\((?!https?:|\/|#|mailto:)[^)]*\.md/.test(full));
  } finally {
    site.done();
  }
});

test("home and download pages have Markdown versions that keep their sections", () => {
  for (const env of [PRE, REL]) {
    const site = built(env);
    try {
      for (const [html, md] of [
        ["index.html", "index.md"],
        ["download.html", "download.md"],
      ]) {
        const page = site.page(html);
        const markdown = site.page(md);
        assert.match(page, new RegExp(`<link rel="alternate" type="text/markdown" href="/${md}"`));
        assert.doesNotMatch(markdown, /\{\{|<[a-z]/);
        const sections = [...page.matchAll(/<h2[^>]*>([^<]+)<\/h2>/g)].map((m) => m[1]);
        assert.ok(sections.length > 0, html);
        for (const title of sections) assert.ok(markdown.includes(`## ${title}`), `${md} lacks "## ${title}"`);
      }
      assert.equal(site.page("index.md").includes("Pre-release"), env === PRE);
      assert.match(site.page("404.md"), /^# We could not find that page/);
      assert.doesNotMatch(site.page("404.html"), /rel="alternate" type="text\/markdown"/);
    } finally {
      site.done();
    }
  }
});

test("the docs versions follow the releases", () => {
  assert.deepEqual(docsVersions(readState(PRE)), [{ ref: "main", label: "Development (main)", base: "/docs/" }]);
  const state = readState({ ...REL, SITE_RELEASE_TAG: "v0.2.0", SITE_DOCS_VERSIONS: "v0.2.0\nv0.1.0" });
  assert.deepEqual(docsVersions(state), [
    { ref: "v0.2.0", label: "v0.2.0 (latest)", base: "/docs/" },
    { ref: "main", label: "Development (main)", base: "/docs/v/main/" },
    { ref: "v0.1.0", label: "v0.1.0", base: "/docs/v/v0.1.0/" },
  ]);
  assert.throws(() => readState({ ...REL, SITE_DOCS_VERSIONS: "v0.1.0 ../main" }), /SITE_DOCS_VERSIONS/);
  assert.throws(() => readState({ ...PRE, SITE_DOCS_VERSIONS: "v0.1.0" }), /SITE_RELEASE_TAG/);
});

test("the docs shell has every hole and marker the Worker fills, once", () => {
  const site = built(REL);
  try {
    const shell = site.shell();
    for (const marker of ["jsonld", "doc-versions", "doc-nav", "doc-note", "doc-body", "doc-footer"]) {
      assert.equal(shell.html.split(`<!--${marker}-->`).length, 2, marker);
    }
    assert.ok(!shell.html.includes("{{"));
    assert.equal(shell.versions[0].ref, "v0.1.0");
    const api = shell.nav.find((g) => g.title === "API reference");
    assert.deepEqual(
      api.pages.map((p) => p.key),
      ["api/README", "api/typescript/README", "api/python/README", "api/rust/bookclerk_plugin_sdk/index", "api/rust/README"],
    );
  } finally {
    site.done();
  }
});
