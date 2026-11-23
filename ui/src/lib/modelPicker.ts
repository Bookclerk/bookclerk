/**
 * Labels for the model picker in Settings: what a model costs and where
 * its files stand on this node.
 *
 * @module
 */
import type { OfferedModelView } from "@/lib/api";
import { size } from "@/lib/nodeFormat";

/** One model's status line and whether it needs the operator's attention. */
export interface ModelStatusLine {
  /** Short status text, such as `Installed` or `Downloading 37%`. */
  label: string;
  /** True when the operator should look: a failed download, or downloads turned off. */
  trouble: boolean;
}

/**
 * What a model costs on a node.
 *
 * @param model - The offered model.
 * @returns Text such as `134 MiB download · needs 220 MiB of memory`.
 */
export function modelCost(model: OfferedModelView): string {
  return `${size(model.download_bytes)} download · needs ${size(model.memory_bytes)} of memory`;
}

/**
 * Where a model's files stand on this node.
 *
 * @param model - The offered model.
 * @param selected - Whether the form has this model selected.
 * @returns The status line; `null` for a model that is neither selected nor on this node.
 */
export function modelStatus(model: OfferedModelView, selected: boolean): ModelStatusLine | null {
  switch (model.state) {
    case "installed":
      return { label: "Installed", trouble: false };
    case "checking":
      return { label: "Checking the files", trouble: false };
    case "downloading": {
      const share =
        model.download_bytes > 0
          ? Math.min(100, Math.floor(((model.received_bytes ?? 0) / model.download_bytes) * 100))
          : 0;
      return { label: `Downloading ${share}%`, trouble: false };
    }
    case "waiting":
      return {
        label: model.detail
          ? `Download failed, trying again: ${model.detail}`
          : "Download failed, trying again",
        trouble: true,
      };
    case "downloads_off":
      return { label: "Not on this server, and downloads are turned off", trouble: true };
    case "not_installed":
      return selected ? { label: "Downloads after you save", trouble: false } : null;
  }
}

/**
 * Why a model cannot be chosen on this node.
 *
 * @param model - The offered model.
 * @param memoryCeilingBytes - Memory the plugin may use on this node.
 * @returns The reason; `null` when the model fits.
 */
export function modelUnfit(model: OfferedModelView, memoryCeilingBytes: number): string | null {
  if (model.fits) return null;
  return `Needs ${size(model.memory_bytes)} of memory; the plugin may use ${size(memoryCeilingBytes)} on this server.`;
}
