import { describe, expect, it } from "vitest";
import { seriesChoices, seriesKey } from "@/lib/catalogTitle";

describe("series choices", () => {
  it("keys every store's spelling of a series the same", () => {
    const key = seriesKey("King and Maxwell");
    expect(key).toBe("king and maxwell");
    for (const variant of ["King & Maxwell", "king  AND  maxwell", "King and Maxwell (abridged)"]) {
      expect(seriesKey(variant)).toBe(key);
    }
    expect(seriesKey("Kingsbridge")).not.toBe(key);
    expect(seriesKey("  ")).toBe("");
  });

  it("offers one choice per series, spelled the way most titles spell it", () => {
    expect(
      seriesChoices([
        "King & Maxwell",
        "King and Maxwell",
        "King & Maxwell",
        "",
        "Camel Club",
        " camel club ",
      ]),
    ).toEqual(["Camel Club", "King & Maxwell"]);
  });
});
