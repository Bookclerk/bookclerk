// Tests for build.mjs: release-state validation, the template engine and the
// rendered pages in both release states. Run with `npm test`.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { build, escapeHtml, inlineLogo, readState, render, templateContext } from "./build.mjs";

/** Release state of a repository with no release yet. */
const PRE = { SITE_COMMIT: "b6adae4d0123456789abcdef0123456789abcdef", SITE_BUILT_ON: "2026-10-23" };

/** Release state after a first release. */
const REL = { ...PRE, SITE_RELEASE_TAG: "v0.1.0", SITE_RELEASE_DATE: "2026-12-01" };

/**
 * Builds the site into a temporary directory.
 *
 * @param {Record<string, string>} env Release state.
 * @returns {{files: string[], page: (name: string) => string, done: () => void}}
 */
function built(env) {
  const dir = mkdtempSync(join(tmpdir(), "bookclerk-site-"));
  const files = build(env, dir);
  return { files, page: (name) => readFileSync(join(dir, name), "utf8"), done: () => rmSync(dir, { recursive: true, force: true }) };
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

test("docs links follow the release tag", () => {
  assert.match(templateContext(readState(PRE)).values.docsUrl, /\/blob\/main\/docs\/README\.md$/);
  assert.match(templateContext(readState(REL)).values.docsUrl, /\/blob\/v0\.1\.0\/docs\/README\.md$/);
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
    for (const name of ["index.html", "download.html", "404.html", "site.css", "_headers", "robots.txt", "favicon.ico", "bookclerk-mark.svg", "site.webmanifest"]) {
      assert.ok(site.files.includes(name), name);
    }
    assert.match(site.page("_headers"), /Content-Security-Policy: default-src 'none'/);
  } finally {
    site.done();
  }
});
