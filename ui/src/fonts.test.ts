import { describe, expect, it } from "vitest";
import html from "../index.html?raw";
import css from "./index.css?raw";

/** Font files bundled from src/fonts/, keyed by their path relative to this file. */
const bundled = Object.keys(import.meta.glob("./fonts/*.woff2"));

/** Font files the `@font-face` rules in index.css load. */
const declared = [...css.matchAll(/@font-face\s*\{[^}]*?url\("(\.\/fonts\/[^"]+)"\)/g)].map(
  (m) => m[1],
);

describe("web fonts", () => {
  it("are declared for every family the UI uses, from bundled files", () => {
    for (const family of ["source-sans-3", "literata", "google-sans"]) {
      expect(
        declared.some((file) => file.startsWith(`./fonts/${family}-`)),
        family,
      ).toBe(true);
    }
    for (const file of declared) expect(bundled).toContain(file);
  });

  it("are never loaded from a third-party origin", () => {
    expect(html).toContain("<title>Bookclerk</title>");
    expect(css).toContain("@font-face");
    for (const source of [html, css]) {
      expect(source).not.toMatch(/fonts\.(googleapis|gstatic)\.com/);
    }
  });
});
