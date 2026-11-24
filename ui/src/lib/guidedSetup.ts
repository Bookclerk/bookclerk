/**
 * Helpers for the guided setup panel: the operator's drafts and the values
 * the instance document stores.
 */
import type { InstanceSettingValue, PluginSettingOption, ProvisioningView } from "@/lib/api";

/**
 * Converts a draft input to the value the document stores.
 *
 * @param option - The setting the input fills, when the plugin lists it.
 * @param draft - The text the operator typed or picked.
 * @returns The stored value, or `null` to leave the key out.
 */
export function inputValue(
  option: PluginSettingOption | undefined,
  draft: string,
): InstanceSettingValue | null {
  const text = draft.trim();
  if (!text) return null;
  switch (option?.value_type) {
    case "boolean":
      return text === "true";
    case "number": {
      const value = Number(text);
      return Number.isFinite(value) ? value : text;
    }
    case "string":
    case "secret":
    case "table":
    case undefined:
      return text;
  }
}

/**
 * The drafts the panel starts with: each input's current setting.
 *
 * @param table - The plugin's guided setup.
 * @param settings - The instance's settings as the Settings page lists them.
 * @returns Draft text by input key.
 */
export function initialDrafts(
  table: ProvisioningView,
  settings: PluginSettingOption[],
): Record<string, string> {
  const drafts: Record<string, string> = {};
  for (const key of table.inputs) {
    drafts[key] = settings.find((option) => option.key === key)?.value ?? "";
  }
  return drafts;
}
