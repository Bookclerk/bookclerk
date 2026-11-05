import { describe, expect, it } from "vitest";
import { domainLabel, fetchHostsSummary } from "./networkGrant";

describe("network grant wording", () => {
  it("names the any-public-host grant", () => {
    expect(domainLabel("*")).toBe("Any public website");
    expect(domainLabel(" * ")).toBe("Any public website");
    expect(domainLabel("*.example.com")).toBe("*.example.com");
    expect(domainLabel("covers.openlibrary.org")).toBe("covers.openlibrary.org");
  });

  it("summarizes fetch hosts", () => {
    expect(fetchHostsSummary([])).toBe("no fetch hosts");
    expect(fetchHostsSummary(["a.example"])).toBe("1 fetch host");
    expect(fetchHostsSummary(["a.example", "b.example"])).toBe("2 fetch hosts");
    expect(fetchHostsSummary(["a.example", "*"])).toBe("any public website");
  });
});
