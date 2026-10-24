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
const REL = { ...PRE, SITE_RELEASE_TAG: "v0.1.0", SITE_RELEASE_DATE: "2026-12-01", SITE_DOCS_VERSIONS: "v0.1.0" };

/** A deployment with a hosted demo. */
const DEMO = { SITE_DEMO_URL: "https://demo.bookclerk.app" };

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

test("repository links follow the docs' release tag", () => {
  assert.match(templateContext(readState(PRE)).values.blobBase, /\/blob\/[0-9a-f]{7,40}\/$/);
  assert.match(templateContext(readState(REL)).values.blobBase, /\/blob\/v0\.1\.0\/$/);
  const unpublished = templateContext(readState({ ...REL, SITE_DOCS_VERSIONS: "" }));
  assert.match(unpublished.values.blobBase, /\/blob\/[0-9a-f]{7,40}\/$/);
  assert.equal(unpublished.values.docsVersion, "Development (main)");
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
      // A landmark, so screen-reader users can find and skip the notice.
      assert.ok(html.includes('<aside class="ribbon" aria-label="Pre-release notice">'), name);
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

test("readState accepts a demo origin and rejects anything else", () => {
  assert.equal(readState(PRE).demoUrl, null);
  assert.equal(readState({ ...PRE, SITE_DEMO_URL: " " }).demoUrl, null);
  assert.equal(readState({ ...PRE, ...DEMO }).demoUrl, "https://demo.bookclerk.app");
  for (const url of ["http://demo.bookclerk.app", "https://demo.bookclerk.app/", "javascript:alert(1)", "https://demo.bookclerk.app\"><script>"]) {
    assert.throws(() => readState({ ...PRE, SITE_DEMO_URL: url }), /SITE_DEMO_URL/, url);
  }
});

test("a build without a demo links to none", () => {
  for (const env of [PRE, REL]) {
    const site = built(env);
    try {
      for (const name of ["index.html", "download.html", "404.html", "index.md", "download.md", "llms.txt"]) {
        assert.doesNotMatch(site.page(name), /live demo|demo\.bookclerk/i, name);
      }
      assert.ok(site.page("index.html").includes('href="#how">See how it works'));
    } finally {
      site.done();
    }
  }
});

test("before a release the demo leads the home and download pages", () => {
  const site = built({ ...PRE, ...DEMO });
  try {
    const home = site.page("index.html");
    const hero = home.slice(home.indexOf('class="btn-row"'), home.indexOf("</div>", home.indexOf('class="btn-row"')));
    assert.match(hero, /class="btn btn-primary btn-arrow" href="https:\/\/demo.bookclerk.app">Try the live demo/);
    assert.match(hero, /class="btn btn-secondary" href="\/download">Get the preview/);
    assert.ok(!home.includes("Try the preview"));
    assert.ok(home.includes('<div class="demo-strip">'));
    assert.ok(home.includes("Can I try Bookclerk without installing it?"));
    for (const name of ["index.html", "download.html", "404.html"]) {
      assert.ok(site.page(name).includes('<a href="https://demo.bookclerk.app">Demo</a>'), name);
      assert.ok(site.page(name).includes('<a href="https://demo.bookclerk.app">Live demo</a>'), name);
    }
    const download = site.page("download.html");
    assert.ok(download.indexOf('class="demo-card"') < download.indexOf('class="status-card"'));
    assert.ok(download.includes("try Bookclerk in your browser"));
    assert.ok(site.page("llms.txt").includes("Live demo: https://demo.bookclerk.app "));
    assert.ok(site.page("index.md").includes("[Try the live demo](https://demo.bookclerk.app)"));
  } finally {
    site.done();
  }
});

test("after a release the demo follows the download", () => {
  const site = built({ ...REL, ...DEMO });
  try {
    const home = site.page("index.html");
    assert.ok(home.indexOf("Download v0.1.0") < home.indexOf('class="btn btn-secondary" href="https://demo.bookclerk.app">Try the live demo'));
    assert.ok(!home.includes("Get the preview"));
    const download = site.page("download.html");
    assert.ok(download.indexOf('class="status-card"') < download.indexOf('class="demo-card"'));
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

test("pages load fonts from the site itself, and every font they name is published", () => {
  const site = built(PRE);
  try {
    const css = site.page("site.css");
    const named = [...css.matchAll(/url\("\/fonts\/([^"]+)"\)/g)].map((m) => m[1]);
    assert.ok(named.length > 0);
    for (const name of ["index.html", "download.html", "404.html"]) {
      const html = site.page(name);
      // No third-party origin may block rendering or learn who visits.
      assert.ok(!/fonts\.(googleapis|gstatic)\.com/.test(html), name);
      for (const [, font] of html.matchAll(/<link rel="preload" href="\/fonts\/([^"]+)"/g)) assert.ok(named.includes(font), `${name}: ${font}`);
    }
    for (const font of named) assert.ok(site.page(`fonts/${font}`).length > 0, font);
    for (const license of ["OFL-sourcesans3.txt", "OFL-literata.txt", "OFL-ibmplexmono.txt"]) {
      assert.match(site.page(`fonts/${license}`), /SIL Open Font License/);
    }
    assert.match(site.page("_headers"), /\/fonts\/\*\n  Cache-Control: public, max-age=31536000, immutable/);
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
    const app = ld["@graph"].find((n) => n["@type"] === "SoftwareApplication");
    assert.ok(app);
    assert.ok(!("softwareVersion" in app));
    assert.equal(app.dateModified, "2026-10-23");
    // Structured data repeats the page; it never says what visitors cannot see.
    const text = home.replace(/<script[\s\S]*?<\/script>/g, "").replace(/<[^>]*>/g, "").replace(/&#39;/g, "'").replace(/\s+/g, " ");
    const faq = ld["@graph"].find((n) => n["@type"] === "FAQPage");
    assert.equal(faq.mainEntity.length, 6);
    for (const q of faq.mainEntity) {
      assert.ok(text.includes(q.name), q.name);
      assert.ok(text.includes(q.acceptedAnswer.text), q.acceptedAnswer.text);
    }
    assert.equal(app.featureList.length, 6);
    for (const f of app.featureList) assert.ok(text.includes(f.split(": ")[1]), f);
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
    assert.equal(ld["@graph"].find((n) => n["@type"] === "SoftwareApplication").softwareVersion, "0.1.0");
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
    assert.ok(llms.includes("- **One library for every store.** Search, tag and rate"));
    assert.ok(llms.includes("- **Which stores are supported?** Audible, Libro.fm"));
    // No headings between the summary and the first link section.
    assert.doesNotMatch(llms.slice(0, llms.indexOf("\n## ")), /\n#/);
    assert.match(site.page("sitemap.xml"), /<loc>https:\/\/bookclerk.app\/<\/loc><lastmod>2026-10-23<\/lastmod>/);
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
  for (const env of [PRE, REL, { ...PRE, ...DEMO }, { ...REL, ...DEMO }]) {
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
      assert.equal(site.page("index.md").includes("Pre-release"), env.SITE_RELEASE_TAG === undefined);
      assert.match(site.page("404.md"), /^# We could not find that page/);
      assert.doesNotMatch(site.page("404.html"), /rel="alternate" type="text\/markdown"/);
    } finally {
      site.done();
    }
  }
});

test("the docs versions follow the releases", () => {
  assert.deepEqual(docsVersions(readState(PRE)), [{ ref: "main", label: "Development (main)", base: "/docs/" }]);
  // A release whose docs are not published yet leaves /docs/ on main.
  assert.deepEqual(docsVersions(readState({ ...REL, SITE_DOCS_VERSIONS: "" })), [{ ref: "main", label: "Development (main)", base: "/docs/" }]);
  const state = readState({ ...REL, SITE_RELEASE_TAG: "v0.3.0", SITE_DOCS_VERSIONS: "v0.2.0\nv0.1.0" });
  assert.deepEqual(docsVersions(state), [
    { ref: "v0.2.0", label: "v0.2.0 (latest)", base: "/docs/" },
    { ref: "main", label: "Development (main)", base: "/docs/v/main/" },
    { ref: "v0.1.0", label: "v0.1.0", base: "/docs/v/v0.1.0/" },
  ]);
  assert.throws(() => readState({ ...REL, SITE_DOCS_VERSIONS: "v0.1.0 ../main" }), /SITE_DOCS_VERSIONS/);
  assert.throws(() => readState({ ...PRE, SITE_DOCS_VERSIONS: "v0.1.0" }), /SITE_RELEASE_TAG/);
  const many = Array.from({ length: 150 }, (_, i) => `v0.${149 - i}.0`);
  const capped = readState({ ...REL, SITE_RELEASE_TAG: "v0.149.0", SITE_DOCS_VERSIONS: many.join(" ") });
  assert.equal(capped.docsTags.length, 100);
  assert.equal(capped.docsTags[99], "v0.50.0", "the newest tags are kept");
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
