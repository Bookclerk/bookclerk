import { describe, expect, it } from "vitest";
import {
  MAX_WORK_TAGS,
  MAX_WORK_TAG_BYTES,
  adminTagText,
  parseTagInput,
  tagInputProblem,
} from "@/lib/workTags";

describe("library tags", () => {
  it("splits, trims and drops repeats ignoring case", () => {
    expect(parseTagInput(" Kids, kids ,, Bedtime ")).toEqual(["Kids", "Bedtime"]);
    expect(parseTagInput("")).toEqual([]);
  });

  it("names the bound a list breaks", () => {
    expect(tagInputProblem(["a"])).toBeNull();
    const many = Array.from({ length: MAX_WORK_TAGS + 1 }, (_, n) => `t${n}`);
    expect(tagInputProblem(many)).toMatch(/at most/);
    expect(tagInputProblem(["x".repeat(MAX_WORK_TAG_BYTES + 1)])).toMatch(/bytes/);
  });

  it("edits only the tags set here", () => {
    expect(
      adminTagText([
        { value: "Kids", source: "admin" },
        { value: "Picture Book", source: "sidecar" },
        { value: "Bedtime", source: "admin" },
      ]),
    ).toBe("Kids, Bedtime");
  });
});
