import { describe, expect, it } from "vitest";
import { inLibraryLabel, type CatalogSearchHit } from "./api";
import { catalogTitleFromHit } from "./catalogTitle";

describe("inLibraryLabel", () => {
  it("names the held edition and another held edition", () => {
    expect(inLibraryLabel("this_edition")).toBe("In your library");
    expect(inLibraryLabel("other_edition")).toBe("Another edition in your library");
  });

  it("is null when the library holds neither", () => {
    expect(inLibraryLabel(undefined)).toBeNull();
    expect(inLibraryLabel(null)).toBeNull();
  });

  it("is carried from a search hit to its row", () => {
    const hit: CatalogSearchHit = {
      work_key: "asin:B002V1OF70",
      title: "Dune",
      authors: "Frank Herbert",
      narrators: null,
      series: null,
      asin: "B002V1OF70",
      isbn: null,
      store_editions: [{ source: "audible", product_id: "B002V1OF70" }],
      sources: ["audible"],
      in_library: "this_edition",
    };
    expect(catalogTitleFromHit(hit).in_library).toBe("this_edition");
    expect(catalogTitleFromHit({ ...hit, in_library: undefined }).in_library).toBeNull();
  });
});
