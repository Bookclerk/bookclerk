// The Worker's own pages: what the waiting page shows while a copy starts.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { describe, it } from "node:test";
import { waitPage } from "../src/pages.ts";

describe("waitPage", () => {
  it("shows the waveform throbber without inline styles", () => {
    const html = waitPage();
    assert.match(html, /<svg id="demo-wait-throbber" class="demo-throbber"[^>]*aria-hidden="true"/);
    assert.equal(html.match(/class="waveform-bar"/g)?.length, 3);
    // The page CSP allows only same-origin stylesheets.
    assert.doesNotMatch(html, /\sstyle=/);
  });

  it("is animated by the stylesheet and hidden by the script on failure", () => {
    const css = readFileSync(new URL("../public/_demo/demo.css", import.meta.url), "utf8");
    assert.match(css, /@keyframes wavePulse/);
    assert.match(css, /prefers-reduced-motion: reduce/);
    const js = readFileSync(new URL("../public/_demo/wait.js", import.meta.url), "utf8");
    assert.match(js, /getElementById\("demo-wait-throbber"\)/);
  });
});
