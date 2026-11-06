// The Worker's own pages: the launch page's layout and footer, what the
// waiting page shows while a copy starts, and the static files they load.

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { describe, it } from "vitest";
import { runInNewContext } from "node:vm";
import { URL } from "node:url";
import {
  aboutHowLong,
  endedPage,
  LEGAL,
  launchPage,
  PAGE_CSP,
  SOCIAL_IMAGE_ALT,
  TURNSTILE_SCRIPT,
  waitPage,
} from "../src/pages.ts";
import { VISITOR_PASSWORD } from "../src/lifecycle.ts";

/**
 * Reads a file relative to this test.
 *
 * @param path - Path from tools/demo/test/.
 * @returns The bytes.
 */
function file(path: string): Buffer {
  return readFileSync(new URL(path, import.meta.url));
}

/**
 * SHA-256 of a file, hex.
 *
 * @param path - Path from tools/demo/test/.
 * @returns The digest.
 */
function sha256(path: string): string {
  return createHash("sha256").update(file(path)).digest("hex");
}

const LAUNCH = {
  siteKey: "0x4AAAAAAA-site",
  siteUrl: "https://bookclerk.example.test",
  sessionMinutes: 30,
  estimateSeconds: 20,
};

describe("launchPage", () => {
  it("names the icons and the website's social card", () => {
    const html = launchPage(LAUNCH);
    assert.match(html, /<link rel="icon" href="\/_demo\/favicon\.ico" sizes="any">/);
    assert.match(html, /<link rel="apple-touch-icon" href="\/_demo\/apple-touch-icon\.png">/);
    assert.match(
      html,
      /<meta property="og:image" content="https:\/\/bookclerk\.example\.test\/social-card\.png">/,
    );
    assert.match(html, /<meta property="og:title" content="Try Bookclerk">/);
    assert.match(html, /<meta name="twitter:card" content="summary_large_image">/);
    // The same image and alt text as the website's pages.
    const head = file("../../site/src/partials/head.html").toString("utf8");
    assert.ok(head.includes(`<meta property="og:image:alt" content="${SOCIAL_IMAGE_ALT}">`));
    assert.ok(file("../../site/src/social-card.png").byteLength > 0);
  });

  it("has no link preview image without a website", () => {
    const html = launchPage({ ...LAUNCH, siteUrl: "" });
    assert.doesNotMatch(html, /og:image/);
    assert.match(html, /<link rel="icon" href="\/_demo\/favicon\.ico" sizes="any">/);
  });

  it("has the mark, the stores, the notes and the start form, without inline styles", () => {
    const html = launchPage(LAUNCH);
    assert.match(
      html,
      /<img src="\/_demo\/mark\.svg" alt="" width="36" height="36"><span>Bookclerk<\/span>/,
    );
    assert.match(
      html,
      /<a class="demo-back" href="https:\/\/bookclerk\.example\.test">Back to bookclerk\.example\.test<\/a>/,
    );
    for (const store of ["Audible", "Libro.fm", "Chirp", "GraphicAudio"])
      assert.match(html, new RegExp(`<li>${store}</li>`));
    assert.equal(html.match(/class="demo-icon"/g)?.length, 4);
    assert.match(html, /Usually ready in about 20 seconds/);
    assert.match(html, /Up to 30 minutes\./);
    assert.match(html, /after 30 minutes\./);
    assert.doesNotMatch(html, /\sstyle=/);
    // Every script is a file the CSP allows; none is written inline.
    const scripts = html.match(/<script\b[^>]*>/gi) ?? [];
    assert.ok(scripts.length > 0);
    for (const tag of scripts)
      assert.match(tag, /\ssrc="(\/_demo\/|https:\/\/challenges\.cloudflare\.com\/)/);
  });

  it("renders Turnstile from launch.js, compact on the narrowest screens", () => {
    const srcs = [...launchPage(LAUNCH).matchAll(/<script src="([^"]+)"/g)].map((m) => m[1]);
    assert.deepEqual(srcs, ["/_demo/launch.js", TURNSTILE_SCRIPT.replaceAll("&", "&#38;")]);
    const js = file("../public/_demo/launch.js").toString("utf8");
    for (const narrow of [true, false]) {
      const rendered: Array<Record<string, unknown>> = [];
      const el = { dataset: { sitekey: "key", action: "demo_start" } };
      const window: Record<string, unknown> = {
        matchMedia: (query: string) => ({ matches: narrow && query === "(max-width: 365px)" }),
        turnstile: {
          render: (_el: unknown, opts: Record<string, unknown>) => rendered.push({ ...opts }),
        },
      };
      runInNewContext(js, { window, document: { querySelectorAll: () => [el] } });
      (window.bookclerkDemoTurnstile as () => void)();
      assert.deepEqual(JSON.parse(JSON.stringify(rendered)), [
        { sitekey: "key", action: "demo_start", size: narrow ? "compact" : "normal" },
      ]);
    }
  });

  it("leaves the estimate out until one is known, and the form out when not set up", () => {
    assert.doesNotMatch(launchPage({ ...LAUNCH, estimateSeconds: null }), /Usually ready/);
    const off = launchPage({ ...LAUNCH, siteKey: null });
    assert.doesNotMatch(off, /<form/);
    assert.doesNotMatch(off, /turnstile/);
    assert.match(off, /not available right now/);
  });

  it("shows a refusal above the button", () => {
    assert.match(
      launchPage({ ...LAUNCH, message: "Busy <now>" }),
      /<p class="demo-message" role="alert">Busy &#60;now&#62;<\/p>/,
    );
  });

  it("links home to the demo itself without a website", () => {
    const html = launchPage({ ...LAUNCH, siteUrl: "" });
    assert.match(html, /<a class="demo-brand" href="\/">/);
    assert.doesNotMatch(html, /demo-back/);
  });
});

describe("footer", () => {
  it("carries the website's trademark note word for word on every page", () => {
    const site = file("../../site/src/partials/footer.html").toString("utf8");
    const legal = /<p class="legal faint">([^<]+)<\/p>/.exec(site)?.[1];
    assert.equal(legal, LEGAL);
    for (const html of [
      launchPage(LAUNCH),
      waitPage({ estimateSeconds: null, siteUrl: "" }),
      endedPage({ siteUrl: "" }),
    ]) {
      assert.match(html, /<footer class="demo-footer"><p>Bookclerk is free software/);
      assert.ok(html.includes("their names are trademarks of their owners"));
      assert.ok(
        html.includes("Bookclerk and the Bookclerk logo are trademarks of the Bookclerk project."),
      );
    }
  });
});

describe("search engine tags", () => {
  it("gives every page a description and a title search engines show whole", () => {
    for (const html of [
      launchPage(LAUNCH),
      waitPage({ estimateSeconds: null, siteUrl: "" }),
      endedPage({ siteUrl: "" }),
    ]) {
      const description = /<meta name="description" content="([^"]*)">/.exec(html)?.[1] ?? "";
      assert.ok(
        description.length >= 25 && description.length <= 160,
        `description: ${description}`,
      );
      const title = /<title>([^<]*)<\/title>/.exec(html)?.[1] ?? "";
      assert.ok(title.length > 0 && title.length <= 70, `title: ${title}`);
    }
  });
});

describe("privacy link", () => {
  it("links every page to the website's privacy page when there is a website", () => {
    const site = "https://bookclerk.app";
    for (const html of [
      launchPage({ ...LAUNCH, siteUrl: site }),
      waitPage({ estimateSeconds: null, siteUrl: site }),
      endedPage({ siteUrl: site }),
    ]) {
      assert.match(
        html,
        /<footer class="demo-footer"><p>[^<]+<\/p><p><a href="https:\/\/bookclerk\.app\/privacy">Privacy<\/a><\/p><\/footer>/,
      );
    }
    assert.ok(endedPage({ siteUrl: `${site}/` }).includes(`href="${site}/privacy"`));
    for (const html of [
      waitPage({ estimateSeconds: null, siteUrl: "" }),
      endedPage({ siteUrl: "" }),
    ]) {
      assert.doesNotMatch(html, /\/privacy/);
    }
  });
});

describe("waitPage", () => {
  it("shows the waveform throbber without inline styles", () => {
    const html = waitPage({ estimateSeconds: null, siteUrl: "" });
    assert.match(html, /<svg id="demo-wait-throbber" class="demo-throbber"[^>]*aria-hidden="true"/);
    assert.equal(html.match(/class="waveform-bar"/g)?.length, 3);
    // The page CSP allows only same-origin stylesheets.
    assert.doesNotMatch(html, /\sstyle=/);
    assert.match(html, /This usually takes under a minute\./);
  });

  it("is animated by the stylesheet and hidden by the script on failure", () => {
    const css = file("../public/_demo/demo.css").toString("utf8");
    assert.match(css, /@keyframes wavePulse/);
    assert.match(css, /prefers-reduced-motion: reduce/);
    const js = file("../public/_demo/wait.js").toString("utf8");
    assert.match(js, /getElementById\("demo-wait-throbber"\)/);
    assert.match(js, /bookclerkDemoLive\.follow/);
  });
});

describe("aboutHowLong", () => {
  it("says seconds under a minute and whole minutes after", () => {
    assert.equal(aboutHowLong(20), "about 20 seconds");
    assert.equal(aboutHowLong(60), "about a minute");
    assert.equal(aboutHowLong(85), "about a minute");
    assert.equal(aboutHowLong(125), "about 2 minutes");
  });
});

describe("static files", () => {
  it("serves the website's fonts and the app's mark unchanged", () => {
    for (const font of [
      "literata-v40-latin.woff2",
      "literata-v40-latin-ext.woff2",
      "source-sans-3-v20-latin.woff2",
      "source-sans-3-v20-latin-ext.woff2",
      "OFL-literata.txt",
      "OFL-sourcesans3.txt",
    ]) {
      assert.equal(
        sha256(`../public/_demo/fonts/${font}`),
        sha256(`../../site/src/fonts/${font}`),
        font,
      );
    }
    assert.equal(
      sha256("../public/_demo/mark.svg"),
      sha256("../../../ui/public/bookclerk-mark.svg"),
    );
    for (const icon of ["favicon.ico", "favicon-32x32.png", "apple-touch-icon.png"])
      assert.equal(sha256(`../public/_demo/${icon}`), sha256(`../../../ui/public/${icon}`), icon);
  });

  it("uses the website's metric-matched fallback fonts", () => {
    const faces = (css: string): string[] =>
      [...css.matchAll(/@font-face \{[^}]*"(?:Source Sans 3|Literata) Fallback"[^}]*\}/g)].map(
        (m) => m[0],
      );
    const demo = file("../public/_demo/demo.css").toString("utf8");
    const site = faces(file("../../site/src/site.css").toString("utf8"));
    assert.equal(site.length, 4);
    assert.deepEqual(faces(demo), site);
    assert.match(demo, /--display:\s*"Literata",\s*"Literata Fallback",/);
    assert.match(demo, /--sans:\s*"Source Sans 3",\s*"Source Sans 3 Fallback",/);
  });

  it("loads every font the stylesheet names, under a CSP that allows them", () => {
    const css = file("../public/_demo/demo.css").toString("utf8");
    const urls = [...css.matchAll(/url\("\/_demo\/fonts\/([^"]+)"\)/g)].map((m) => m[1]);
    assert.equal(urls.length, 4);
    for (const name of urls) assert.ok(file(`../public/_demo/fonts/${name}`).byteLength > 0);
    assert.match(PAGE_CSP, /font-src 'self'/);
  });

  it("allows the Web Analytics beacon and nothing else from outside but Turnstile", () => {
    const origins = [...PAGE_CSP.matchAll(/https:\/\/[^\s;]+/g)].map((m) => m[0]);
    assert.deepEqual([...new Set(origins)].sort(), [
      "https://challenges.cloudflare.com",
      "https://cloudflareinsights.com",
      "https://static.cloudflareinsights.com",
    ]);
  });
});

describe("banner", () => {
  it("shows the password the demo Owner is created with", () => {
    const js = file("../public/_demo/banner.js").toString("utf8");
    assert.ok(js.includes(`const PASSWORD = ${JSON.stringify(VISITOR_PASSWORD)};`));
  });
});
