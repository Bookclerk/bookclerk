import { describe, expect, it } from "vitest";
import { LIBRARY_DEFAULT, MEMBER_CONTROLS, overrideChoice } from "@/lib/memberControls";

describe("member controls", () => {
  it("lists every control once", () => {
    const names = MEMBER_CONTROLS.map((info) => info.control);
    expect(new Set(names).size).toBe(names.length);
    expect(names.sort()).toEqual(Object.keys(LIBRARY_DEFAULT).sort());
  });

  it("reads the choice each override stands for", () => {
    const overrides = { download: false, link_accounts: true };
    expect(overrideChoice(overrides, "download")).toBe("off");
    expect(overrideChoice(overrides, "link_accounts")).toBe("on");
    expect(overrideChoice(overrides, "see_prices")).toBe("default");
  });
});
