import { describe, expect, it } from "vitest";
import { ADMIN_CONTROLS, ALL_ADMIN_ALLOWED, adminOverrideChoice } from "@/lib/adminControls";

describe("administrator switches", () => {
  it("lists every switch once", () => {
    const names = ADMIN_CONTROLS.map((info) => info.control);
    expect(new Set(names).size).toBe(names.length);
    expect(names.sort()).toEqual(Object.keys(ALL_ADMIN_ALLOWED).sort());
  });

  it("reads the choice an override stands for", () => {
    const overrides = { run_jobs: true, manage_members: false };
    expect(adminOverrideChoice(overrides, "manage_members")).toBe("off");
    expect(adminOverrideChoice(overrides, "run_jobs")).toBe("on");
    expect(adminOverrideChoice(overrides, "invite_branches")).toBe("default");
  });
});
