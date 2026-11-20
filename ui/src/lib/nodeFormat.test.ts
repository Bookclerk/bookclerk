import { describe, expect, it } from "vitest";
import { relationshipLabel } from "./nodeFormat";

describe("relationshipLabel", () => {
  it("names each relationship the way people see it", () => {
    expect(relationshipLabel("shared_branch")).toBe("Shared branch");
    expect(relationshipLabel("remote_branch")).toBe("Remote branch");
    expect(relationshipLabel("local_branch")).toBe("Local branch");
  });
});
