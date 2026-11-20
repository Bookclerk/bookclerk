import { describe, expect, it } from "vitest";

import { navLinksForRole, showStatusNav } from "./nav";

describe("showStatusNav", () => {
  it("follows the session's administers_nodes when the server sends it", () => {
    expect(showStatusNav("member", true)).toBe(true);
    expect(showStatusNav("administrator", false)).toBe(false);
  });

  it("falls back to the role when the server sends none", () => {
    expect(showStatusNav("operator")).toBe(true);
    expect(showStatusNav("owner")).toBe(true);
    expect(showStatusNav("administrator")).toBe(true);
    expect(showStatusNav("member")).toBe(false);
    expect(showStatusNav(undefined)).toBe(false);
  });
});

describe("navLinksForRole", () => {
  it("adds Status for a member who owns a node", () => {
    const ids = navLinksForRole("member", true).map((link) => link.id);
    expect(ids).toContain("status");
    expect(ids[ids.length - 1]).toBe("settings");
  });

  it("leaves Status out for a member who owns none", () => {
    expect(navLinksForRole("member", false).map((link) => link.id)).not.toContain("status");
  });
});
