import { describe, expect, it } from "vitest";
import { diagnosticsIssueSearchUrl, formatReportTime } from "./diagnostics";

describe("diagnosticsIssueSearchUrl", () => {
  it("searches the issues for the quoted report ID", () => {
    expect(diagnosticsIssueSearchUrl("0e9ced79-df3c-4dae-872b-2e5772b06e69")).toBe(
      "https://github.com/Bookclerk/bookclerk/issues?q=%220e9ced79-df3c-4dae-872b-2e5772b06e69%22",
    );
  });

  it("puts nothing else in the URL", () => {
    for (const bad of [
      "",
      "x&y=1",
      "0E9CED79-DF3C-4DAE-872B-2E5772B06E69",
      "javascript:alert(1)",
    ]) {
      expect(diagnosticsIssueSearchUrl(bad)).toBe("https://github.com/Bookclerk/bookclerk/issues");
    }
  });
});

describe("formatReportTime", () => {
  it("reads a time and refuses what is not one", () => {
    expect(formatReportTime(1_763_520_000_000)).not.toBe("unknown time");
    expect(formatReportTime(0)).toBe("unknown time");
    expect(formatReportTime(Number.NaN)).toBe("unknown time");
    expect(formatReportTime(9e15)).toBe("unknown time");
  });
});
