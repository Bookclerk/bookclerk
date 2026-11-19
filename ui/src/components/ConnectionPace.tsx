import { useState, type FormEvent } from "react";
import { Gauge } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { paceSummary } from "@/lib/pace";
import {
  portalSetConnectionPace,
  type PortalConnection,
  type PortalPace,
  type PortalPaceMode,
} from "@/lib/api";

/**
 * The pace line under a store connection, with a form to change it.
 *
 * Downloads Bookclerk starts itself wait for the pace; downloads a person
 * starts are counted but never held.
 *
 * @param props - The connection, whether the page is busy, and a callback
 *   with the stored pace after a change.
 */
export function ConnectionPace({
  connection,
  busy,
  onChanged,
}: {
  connection: PortalConnection;
  busy: boolean;
  onChanged: (pace: PortalPace) => void;
}) {
  const pace = connection.pace;
  const [open, setOpen] = useState(false);
  const [mode, setMode] = useState<PortalPaceMode>(pace.mode);
  const [perDay, setPerDay] = useState(
    String(pace.custom_downloads_per_day ?? pace.store_downloads_per_day ?? ""),
  );
  const [gap, setGap] = useState(
    String(pace.custom_min_gap_seconds ?? pace.store_min_gap_seconds ?? ""),
  );
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const source = connection.plugin_key || connection.source;

  async function save(e: FormEvent) {
    e.preventDefault();
    setSaving(true);
    setError(null);
    try {
      const change =
        mode === "custom"
          ? {
              mode,
              ...(perDay.trim() ? { downloads_per_day: Number(perDay) } : {}),
              ...(gap.trim() ? { min_gap_seconds: Number(gap) } : {}),
            }
          : { mode };
      onChanged(await portalSetConnectionPace(source, connection.account_id, change));
      setOpen(false);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Saving the pace failed");
    } finally {
      setSaving(false);
    }
  }

  async function clearCooldown() {
    setSaving(true);
    setError(null);
    try {
      onChanged(
        await portalSetConnectionPace(source, connection.account_id, {
          mode: pace.mode,
          ...(pace.mode === "custom" && pace.custom_downloads_per_day !== null
            ? { downloads_per_day: pace.custom_downloads_per_day }
            : {}),
          ...(pace.mode === "custom" && pace.custom_min_gap_seconds !== null
            ? { min_gap_seconds: pace.custom_min_gap_seconds }
            : {}),
          clear_cooldown: true,
        }),
      );
    } catch (err) {
      setError(err instanceof Error ? err.message : "Clearing the cool-down failed");
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="w-full space-y-2 pl-10">
      <div className="flex flex-wrap items-center gap-2">
        <p className="text-xs text-ink/60">{paceSummary(pace)}</p>
        <Button
          variant="ghost"
          disabled={busy || saving}
          onClick={() => setOpen((v) => !v)}
          aria-expanded={open}
        >
          <Gauge className="h-4 w-4" />
          Change pace
        </Button>
        {pace.cooldown_until ? (
          <Button variant="ghost" disabled={busy || saving} onClick={() => void clearCooldown()}>
            Clear pause
          </Button>
        ) : null}
      </div>
      {open ? (
        <form
          className="grid gap-2 sm:grid-cols-[auto_1fr_1fr_auto] sm:items-end"
          onSubmit={(e) => void save(e)}
        >
          <label className="grid gap-1 text-xs text-ink/60">
            Pace
            <select
              className="h-9 rounded border border-ink/20 bg-card px-2 text-sm text-ink"
              value={mode}
              onChange={(e) => setMode(e.target.value as PortalPaceMode)}
            >
              <option value="default">Store default</option>
              <option value="custom">Custom</option>
              <option value="unlimited">No limit</option>
            </select>
          </label>
          <label className="grid gap-1 text-xs text-ink/60">
            Downloads per day
            <Input
              type="number"
              min={1}
              max={10000}
              value={perDay}
              disabled={mode !== "custom"}
              onChange={(e) => setPerDay(e.target.value)}
            />
          </label>
          <label className="grid gap-1 text-xs text-ink/60">
            Seconds between downloads
            <Input
              type="number"
              min={0}
              max={86400}
              value={gap}
              disabled={mode !== "custom"}
              onChange={(e) => setGap(e.target.value)}
            />
          </label>
          <Button type="submit" disabled={saving}>
            Save
          </Button>
        </form>
      ) : null}
      {error ? (
        <p className="text-xs font-medium text-brick" role="alert">
          {error}
        </p>
      ) : null}
    </div>
  );
}
