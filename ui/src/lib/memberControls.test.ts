import { describe, expect, it } from "vitest";
import {
  LIBRARY_DEFAULT,
  MEMBER_CONTROLS,
  overrideChoice,
  withOverride,
} from "@/lib/memberControls";

describe("member controls", () => {
  it("lists every control once", () => {
    const names = MEMBER_CONTROLS.map((info) => info.control);
    expect(new Set(names).size).toBe(names.length);
    expect(names.sort()).toEqual(Object.keys(LIBRARY_DEFAULT).sort());
  });

  it("sets and clears one override without touching the others", () => {
    const start = { download: false };
    const off = withOverride(start, "link_accounts", "off");
    expect(off).toEqual({ download: false, link_accounts: false });
    expect(start).toEqual({ download: false });
    expect(overrideChoice(off, "link_accounts")).toBe("off");
    expect(overrideChoice(off, "see_prices")).toBe("default");
    const on = withOverride(off, "download", "on");
    expect(overrideChoice(on, "download")).toBe("on");
    expect(withOverride(on, "download", "default")).toEqual({ link_accounts: false });
  });
});
