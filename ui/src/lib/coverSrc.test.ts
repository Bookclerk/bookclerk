import { describe, expect, it } from "vitest";
import { coverSrc } from "./api";

describe("coverSrc", () => {
  it("appends the size to a same-origin cover path", () => {
    expect(coverSrc("/api/covers/abc.def", 128)).toBe("/api/covers/abc.def/128");
    expect(coverSrc(" /api/covers/abc.def ", 360)).toBe("/api/covers/abc.def/360");
    expect(coverSrc("/api/covers/abc.def", "full")).toBe("/api/covers/abc.def/full");
  });

  it("refuses anything but a cover path", () => {
    expect(coverSrc(null, 128)).toBeNull();
    expect(coverSrc(undefined, 128)).toBeNull();
    expect(coverSrc("  ", 128)).toBeNull();
    expect(coverSrc("https://img.example/c.jpg", 128)).toBeNull();
    expect(coverSrc("/api/library/books/x/cover", 128)).toBeNull();
    expect(coverSrc("/api/covers/abc?x=1", 128)).toBeNull();
  });
});
