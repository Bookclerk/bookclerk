import { describe, expect, it } from "vitest";
import html from "../index.html?raw";

/** Font files bundled from src/fonts/ by the `@font-face` rules in index.css. */
const bundled = Object.keys(import.meta.glob("./fonts/*.woff2"));

describe("web fonts", () => {
  it("are bundled with the UI", () => {
    for (const family of ["source-sans-3", "literata", "google-sans"]) {
      expect(bundled.some((file) => file.startsWith(`./fonts/${family}-`))).toBe(true);
    }
  });

  it("are never loaded from a third-party origin", () => {
    expect(html).toContain("<title>Bookclerk</title>");
    expect(html).not.toMatch(/fonts\.(googleapis|gstatic)\.com/);
  });
});
