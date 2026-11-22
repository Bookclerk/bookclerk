import { describe, expect, it } from "vitest";
import { COPY_PROFILES, copyKey, visibleProfiles } from "@/lib/copyProfiles";

describe("copy profiles", () => {
  it("names each copy once, as output.copies writes it", () => {
    const keys = COPY_PROFILES.map((profile) => copyKey(profile.copy));
    expect(new Set(keys).size).toBe(keys.length);
    expect(keys).toContain("mp3:64k-mono");
    expect(keys.every((key) => !key.startsWith("original:"))).toBe(true);
  });

  it("keeps advanced formats behind the switch", () => {
    expect(visibleProfiles(false).some((profile) => profile.copy.format === "opus")).toBe(false);
    expect(visibleProfiles(true).some((profile) => profile.copy.format === "opus")).toBe(true);
    expect(visibleProfiles(true)).toHaveLength(COPY_PROFILES.length);
  });
});
