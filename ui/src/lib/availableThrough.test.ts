import { describe, expect, it } from "vitest";
import { availableThroughLabel, branchesToAsk, type BranchAvailability } from "./api";

/**
 * A branch listing for the tests.
 *
 * @param node - The branch node id.
 * @param owner - The branch owner's name, if any.
 * @param product - The listed product id.
 * @returns The listing.
 */
function through(node: string, owner?: string | null, product = "B0001"): BranchAvailability {
  return { work_id: "w1", node_id: node, owner_name: owner, store: "audible", product_id: product };
}

describe("availableThroughLabel", () => {
  it("is null when no branch lists the title", () => {
    expect(availableThroughLabel(undefined)).toBeNull();
    expect(availableThroughLabel([])).toBeNull();
  });

  it("names each branch once, by its owner", () => {
    expect(availableThroughLabel([through("n1", "Riley"), through("n1", "Riley", "B0002")])).toBe(
      "Available through Riley's branch",
    );
    expect(availableThroughLabel([through("n1", "Riley"), through("n2", " ")])).toBe(
      "Available through Riley's branch and a branch",
    );
    expect(
      availableThroughLabel([through("n1", "Riley"), through("n2", "Sam"), through("n3", null)]),
    ).toBe("Available through Riley's branch and 2 other branches");
  });
});

describe("branchesToAsk", () => {
  it("lists each branch once, with the work to request", () => {
    expect(branchesToAsk(undefined)).toEqual([]);
    expect(
      branchesToAsk([through("n1", "Riley"), through("n1", "Riley", "B0002"), through("n2", null)]),
    ).toEqual([
      { node_id: "n1", work_id: "w1", label: "Riley's branch" },
      { node_id: "n2", work_id: "w1", label: "the branch" },
    ]);
  });

  it("skips a listing without a work to ask by", () => {
    expect(branchesToAsk([{ ...through("n1", "Riley"), work_id: "" }])).toEqual([]);
  });
});
