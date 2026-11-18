import { describe, expect, it } from "vitest";
import { newRule, scopeProblem, scopeSummary } from "@/lib/patronScope";

describe("scopeProblem", () => {
  it("accepts an empty scope and distinct rules", () => {
    expect(scopeProblem({ rules: [] })).toBeNull();
    expect(
      scopeProblem({
        rules: [
          { field: "genre", value: "Kids", effect: "include" },
          { field: "genre", value: "kids", effect: "exclude" },
        ],
      }),
    ).toBeNull();
  });

  it("refuses blank, duplicate, long and too many rules", () => {
    expect(scopeProblem({ rules: [newRule()] })).toBe("Every rule needs a value.");
    expect(
      scopeProblem({
        rules: [
          { field: "series", value: "Discworld", effect: "exclude" },
          { field: "series", value: " DISCWORLD ", effect: "exclude" },
        ],
      }),
    ).toMatch(/twice/);
    expect(
      scopeProblem({ rules: [{ field: "author", value: "x".repeat(257), effect: "include" }] }),
    ).toMatch(/256 bytes/);
    const many = Array.from({ length: 65 }, (_, n) => ({
      field: "author" as const,
      value: `a${n}`,
      effect: "include" as const,
    }));
    expect(scopeProblem({ rules: many })).toMatch(/64 rules/);
  });
});

describe("scopeSummary", () => {
  it("names the whole library or counts the rules", () => {
    expect(scopeSummary({ rules: [] })).toBe("Whole library");
    expect(
      scopeSummary({
        rules: [
          { field: "genre", value: "Kids", effect: "include" },
          { field: "series", value: "A", effect: "exclude" },
          { field: "series", value: "B", effect: "exclude" },
        ],
      }),
    ).toBe("1 show-only rule, 2 hide rules");
  });
});
