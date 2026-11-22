import { describe, expect, it } from "vitest";
import type { OfferedModelView } from "@/lib/api";
import { modelCost, modelStatus, modelUnfit } from "@/lib/modelPicker";

/** A model offer with the given overrides. */
function model(over: Partial<OfferedModelView> = {}): OfferedModelView {
  return {
    id: "bekko-a8m",
    label: "Bekko (fast)",
    summary: "Small and quick.",
    download_bytes: 140_609_206,
    memory_bytes: 230_686_720,
    fits: true,
    state: "not_installed",
    ...over,
  };
}

describe("model picker labels", () => {
  it("says what a model costs", () => {
    expect(modelCost(model())).toBe("134 MiB download · needs 220 MiB of memory");
  });

  it("describes each state", () => {
    expect(modelStatus(model({ state: "installed" }), true)).toEqual({
      label: "Installed",
      trouble: false,
    });
    expect(modelStatus(model({ state: "checking" }), true)?.label).toBe("Checking the files");
    expect(
      modelStatus(model({ state: "downloading", received_bytes: 70_304_603 }), true)?.label,
    ).toBe("Downloading 50%");
    expect(modelStatus(model({ state: "downloading" }), true)?.label).toBe("Downloading 0%");
    expect(
      modelStatus(model({ state: "downloading", download_bytes: 0, received_bytes: 5 }), true)
        ?.label,
    ).toBe("Downloading 0%");
    expect(
      modelStatus(model({ state: "downloading", received_bytes: 999_999_999 }), true)?.label,
    ).toBe("Downloading 100%");
    expect(
      modelStatus(model({ state: "waiting", detail: "the registry answered 503" }), true),
    ).toEqual({ label: "Download failed, trying again: the registry answered 503", trouble: true });
    expect(modelStatus(model({ state: "waiting" }), true)?.label).toBe(
      "Download failed, trying again",
    );
    expect(modelStatus(model({ state: "downloads_off" }), true)?.trouble).toBe(true);
  });

  it("says a newly selected model downloads on save, and nothing for an unselected one", () => {
    expect(modelStatus(model(), true)?.label).toBe("Downloads after you save");
    expect(modelStatus(model(), false)).toBeNull();
  });

  it("explains a model that does not fit", () => {
    expect(modelUnfit(model(), 512 * 2 ** 20)).toBeNull();
    expect(modelUnfit(model({ fits: false, memory_bytes: 2 ** 30 }), 512 * 2 ** 20)).toBe(
      "Needs 1.0 GiB of memory; the plugin may use 512 MiB on this server.",
    );
  });
});
