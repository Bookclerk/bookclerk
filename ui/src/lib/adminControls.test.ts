import { describe, expect, it } from "vitest";
import {
  ADMIN_CONTROLS,
  ALL_ADMIN_ALLOWED,
  adminOverrideChoice,
  withAdminOverride,
} from "@/lib/adminControls";

describe("administrator switches", () => {
  it("lists every switch once", () => {
    const names = ADMIN_CONTROLS.map((info) => info.control);
    expect(new Set(names).size).toBe(names.length);
    expect(names.sort()).toEqual(Object.keys(ALL_ADMIN_ALLOWED).sort());
  });

  it("sets and clears one override without touching the others", () => {
    const start = { run_jobs: false };
    const off = withAdminOverride(start, "manage_members", "off");
    expect(off).toEqual({ run_jobs: false, manage_members: false });
    expect(start).toEqual({ run_jobs: false });
    expect(adminOverrideChoice(off, "manage_members")).toBe("off");
    expect(adminOverrideChoice(off, "invite_branches")).toBe("default");
    const on = withAdminOverride(off, "run_jobs", "on");
    expect(adminOverrideChoice(on, "run_jobs")).toBe("on");
    expect(withAdminOverride(on, "run_jobs", "default")).toEqual({ manage_members: false });
  });
});
