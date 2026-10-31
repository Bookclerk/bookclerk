// Shared browser layout behavior: the complete notice determines whether it
// collapses, and section anchors clear the sticky header's actual height.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { URL } from "node:url";
import { runInNewContext } from "node:vm";
import { test } from "vitest";

/**
 * Runs the shipped shared-layout script with controllable measurements.
 *
 * @param noticePresent - Whether this page is a pre-release page.
 * @returns Resize and font controls, plus the resulting presentation state.
 */
function layout(noticePresent = true) {
  /** The full notice's block height, including 10px top and bottom padding. */
  let noticeHeight = 102;
  /** The sticky header's border-box height, in CSS pixels. */
  let headerHeight = 81;
  /** Attributes on the visible notice. */
  const attributes = new Set<string>();
  /** Properties written to the document root for anchor clearance. */
  const properties = new Map<string, string>();
  /** Resize callbacks by the independently measured element. */
  const resize = new Map<object, () => void>();
  /** Callback invoked when the web fonts finish loading. */
  let fontsLoaded = () => {};
  /** The full-size measurement copy; its height never depends on collapse. */
  const measure = {
    inert: false,
    classList: { remove: () => {}, add: () => {} },
    setAttribute: () => {},
    getBoundingClientRect: () => ({ height: noticeHeight }),
  };
  /** The header whose natural wrapped height controls anchor clearance. */
  const header = { getBoundingClientRect: () => ({ height: headerHeight }) };
  /** The notice and the existing contents from which the probe is copied. */
  const ribbon = {
    querySelector: () => ({ cloneNode: () => measure }),
    append: () => {},
    setAttribute: (name: string) => attributes.add(name),
    toggleAttribute: (name: string, enabled: boolean) => {
      if (enabled) attributes.add(name);
      else attributes.delete(name);
    },
  };
  runInNewContext(readFileSync(new URL("src/site.js", import.meta.url), "utf8"), {
    document: {
      querySelector: (selector: string) =>
        selector === ".site-header" ? header : noticePresent ? ribbon : null,
      documentElement: {
        style: { setProperty: (name: string, value: string) => properties.set(name, value) },
      },
      fonts: {
        ready: {
          then: (callback: () => void) => {
            fontsLoaded = callback;
          },
        },
      },
    },
    getComputedStyle: () => ({
      paddingTop: "10px",
      paddingBottom: "10px",
      lineHeight: "22px",
      rowGap: "8px",
    }),
    ResizeObserver: class {
      /** Callback retained until the measured element is resized. */
      callback: () => void;
      /**
       * Registers the controller's callback.
       *
       * @param callback - The controller's measurement function.
       */
      constructor(callback: () => void) {
        this.callback = callback;
      }
      /**
       * Associates a resize callback with the observed element.
       *
       * @param target - Either the full notice probe or the sticky header.
       */
      observe(target: object) {
        resize.set(target, this.callback);
      }
    },
  });
  return {
    compact: () => attributes.has("data-compact"),
    ready: () => attributes.has("data-ribbon-ready"),
    inert: () => measure.inert,
    anchorHeight: () => properties.get("--site-header-height"),
    resizeNotice: (height: number) => {
      noticeHeight = height;
      resize.get(measure)?.();
    },
    resizeHeader: (height: number) => {
      headerHeight = height;
      resize.get(header)?.();
    },
    loadFonts: (height: number) => {
      noticeHeight = height;
      fontsLoaded();
    },
  };
}

test("a notice taller than two lines collapses and expands when it fits again", () => {
  const page = layout();
  assert.equal(page.ready(), true);
  assert.equal(page.inert(), true);
  assert.equal(page.compact(), true);
  // 44px of text, an 8px row gap and 20px of padding: exactly two rows.
  page.resizeNotice(72);
  assert.equal(page.compact(), false);
  page.resizeNotice(102);
  assert.equal(page.compact(), true);
  // Remeasuring a compact notice still reads the full-size copy.
  page.resizeNotice(102);
  assert.equal(page.compact(), true);
});

test("rounding at the two-line boundary does not collapse the notice", () => {
  const page = layout();
  page.resizeNotice(72.5);
  assert.equal(page.compact(), false);
  page.resizeNotice(74);
  assert.equal(page.compact(), true);
});

test("a loaded web font rechecks wrapping without a viewport resize", () => {
  const page = layout();
  page.resizeNotice(42);
  assert.equal(page.compact(), false);
  page.loadFonts(102);
  assert.equal(page.compact(), true);
});

test("anchor clearance follows the sticky header's wrapped height", () => {
  const page = layout();
  assert.equal(page.anchorHeight(), "81px");
  page.resizeHeader(113);
  assert.equal(page.anchorHeight(), "113px");
});

test("released pages still measure the header without a notice", () => {
  const page = layout(false);
  assert.equal(page.ready(), false);
  assert.equal(page.anchorHeight(), "81px");
  page.resizeHeader(128);
  assert.equal(page.anchorHeight(), "128px");
});
