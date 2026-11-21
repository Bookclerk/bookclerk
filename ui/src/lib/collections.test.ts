import { describe, expect, it } from "vitest";
import { collectionsOf, collectionsProblem, draftsOf } from "@/lib/collections";

describe("collections editor", () => {
  it("round-trips saved collections through the rows", () => {
    const saved = [{ name: "Kids", prefixes: ["kids/", "children/"] }];
    expect(collectionsOf(draftsOf(saved))).toEqual(saved);
  });

  it("trims, splits and drops empty rows", () => {
    expect(
      collectionsOf([
        { name: " Kids ", prefixes: "kids, ,children/" },
        { name: "", prefixes: "" },
      ]),
    ).toEqual([{ name: "Kids", prefixes: ["kids", "children/"] }]);
  });

  it("names the first problem", () => {
    expect(collectionsProblem([{ name: "Kids", prefixes: ["kids/"] }])).toBeNull();
    expect(collectionsProblem([{ name: "", prefixes: ["a"] }])).toMatch(/name/);
    expect(
      collectionsProblem([
        { name: "A", prefixes: ["a"] },
        { name: "a", prefixes: ["b"] },
      ]),
    ).toMatch(/Two collections/);
    expect(collectionsProblem([{ name: "A", prefixes: [] }])).toMatch(/folder/);
    expect(collectionsProblem([{ name: "A", prefixes: ["/abs"] }])).toMatch(/relative/);
  });
});
