import { Badge } from "@/components/ui/badge";
import type { PluginSettingsGroup } from "@/lib/api";
import { summarizeStorageSync } from "@/lib/storageSync";

/**
 * How a storage destination follows changes made outside Bookclerk: its
 * change feed's state and its last listing, with details in the tooltip.
 */
export function StorageSyncBadge({ plugin }: { plugin: PluginSettingsGroup }) {
  const sync = plugin.storage_sync;
  if (!sync) return null;
  const { label, trouble, details } = summarizeStorageSync(sync);
  return (
    <Badge
      className={
        trouble
          ? "bg-brick/10 text-brick-text normal-case tracking-normal"
          : "bg-ink/5 text-ink/70 normal-case tracking-normal"
      }
    >
      <span title={details.join("\n")}>{label}</span>
    </Badge>
  );
}
