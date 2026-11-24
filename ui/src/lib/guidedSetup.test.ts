import { describe, expect, it } from "vitest";
import type { PluginSettingOption, ProvisioningView } from "@/lib/api";
import { initialDrafts, inputValue } from "@/lib/guidedSetup";

const option = (
  value_type: PluginSettingOption["value_type"],
  value = "",
): PluginSettingOption => ({
  key: "k",
  label: "K",
  value,
  value_type,
});

describe("inputValue", () => {
  it("leaves blank drafts out", () => {
    expect(inputValue(option("string"), "   ")).toBeNull();
  });
  it("trims strings and keeps unknown keys as text", () => {
    expect(inputValue(option("string"), " books ")).toBe("books");
    expect(inputValue(undefined, "eu")).toBe("eu");
  });
  it("converts numbers and booleans", () => {
    expect(inputValue(option("number"), "42")).toBe(42);
    expect(inputValue(option("number"), "lots")).toBe("lots");
    expect(inputValue(option("boolean"), "true")).toBe(true);
    expect(inputValue(option("boolean"), "no")).toBe(false);
  });
});

describe("initialDrafts", () => {
  it("starts each input from its current setting", () => {
    const table: ProvisioningView = {
      provider: "cloudflare",
      grants: ["api_token"],
      token_permissions: [],
      inputs: ["bucket", "jurisdiction"],
      outputs: ["bucket"],
      deprovision: false,
    };
    expect(initialDrafts(table, [{ ...option("string", "books"), key: "bucket" }])).toEqual({
      bucket: "books",
      jurisdiction: "",
    });
  });
});
