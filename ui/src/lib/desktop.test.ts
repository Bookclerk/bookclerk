import { describe, expect, it } from "vitest";
import { isDesktopWindow } from "@/lib/desktop";

describe("isDesktopWindow", () => {
  it("recognizes the custom scheme and its WebView2 form", () => {
    expect(isDesktopWindow({ protocol: "bookclerk:", host: "localhost" })).toBe(true);
    expect(isDesktopWindow({ protocol: "http:", host: "bookclerk.localhost" })).toBe(true);
  });

  it("is false for the daemon's own pages and anything else", () => {
    expect(isDesktopWindow({ protocol: "http:", host: "127.0.0.1:8787" })).toBe(false);
    expect(isDesktopWindow({ protocol: "http:", host: "localhost:5173" })).toBe(false);
    expect(isDesktopWindow({ protocol: "https:", host: "bookclerk.localhost" })).toBe(false);
    expect(isDesktopWindow({ protocol: "bookclerk:", host: "evil.example" })).toBe(false);
  });
});
