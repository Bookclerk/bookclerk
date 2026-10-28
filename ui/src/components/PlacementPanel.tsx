import { useEffect, useState, type KeyboardEvent } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  deletePluginPlacement,
  isApiError,
  putPluginPlacement,
  type PluginSettingsGroup,
} from "@/lib/api";

/** Splits `nas, zone=eu  !slow` into trimmed, non-empty constraints. */
function parseConstraints(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((part) => part.trim())
    .filter(Boolean);
}

interface PlacementPanelProps {
  /** Instance group the panel edits. */
  plugin: PluginSettingsGroup;
  /** Reloads the Settings snapshot after a write. */
  onChanged: () => Promise<void>;
  /** Shows a page-level error. */
  onError: (message: string) => void;
  /** Called on a 401 so the page can return to sign-in. */
  onSessionExpired: () => void;
}

/**
 * Where a plugin instance runs: by hand on this node (the Deployed switch), or
 * on `replicas` nodes the cluster scheduler picks among those carrying every
 * label constraint.
 */
export function PlacementBadge({ plugin }: { plugin: PluginSettingsGroup }) {
  const placement = plugin.placement;
  if (!placement) return null;
  const short =
    placement.replicas === "all"
      ? Boolean(placement.detail)
      : placement.placed < placement.replicas;
  return (
    <Badge
      className={
        short
          ? "bg-brick/10 text-brick normal-case tracking-normal"
          : "bg-teal/10 text-ink/70 normal-case tracking-normal"
      }
    >
      <span title={placement.detail || (plugin.present_on ?? []).join(", ")}>
        Placed {placement.placed}
        {placement.replicas === "all" ? " (all)" : `/${placement.replicas}`}
      </span>
    </Badge>
  );
}

/**
 * Placement controls inside an expanded instance row.
 */
export function PlacementPanel({
  plugin,
  onChanged,
  onError,
  onSessionExpired,
}: PlacementPanelProps) {
  const placement = plugin.placement;
  const [replicas, setReplicas] = useState(String(placement?.replicas ?? 1));
  const [constraints, setConstraints] = useState((placement?.constraints ?? []).join(", "));
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setReplicas(String(placement?.replicas ?? 1));
    setConstraints((placement?.constraints ?? []).join(", "));
  }, [placement?.revision, placement?.replicas, placement?.constraints]);

  const instanceId = plugin.plugin_instance_id;
  if (!instanceId) return null;

  async function run(action: () => Promise<unknown>, failure: string) {
    setBusy(true);
    try {
      await action();
      await onChanged();
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        onSessionExpired();
        return;
      }
      onError(err instanceof Error ? err.message : failure);
    } finally {
      setBusy(false);
    }
  }

  function save() {
    const text = replicas.trim().toLowerCase();
    const count: number | "all" = text === "all" ? "all" : Number.parseInt(text, 10);
    if (count !== "all" && (!Number.isFinite(count) || count < 0 || String(count) !== text)) {
      onError("Replicas must be a whole number of nodes (0 or more) or `all`.");
      return;
    }
    void run(
      () =>
        putPluginPlacement(instanceId as string, {
          replicas: count,
          constraints: parseConstraints(constraints),
          expected_revision: placement?.revision ?? 0,
        }),
      "Failed to save placement",
    );
  }

  /** Enter saves the placement instead of submitting the Settings form. */
  function onKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.key === "Enter") {
      event.preventDefault();
      save();
    }
  }

  const runningOn = plugin.present_on ?? [];
  return (
    <div className="mt-3 border-t border-ink/10 pt-3">
      <p className="text-xs font-medium text-ink/70">Placement</p>
      <p className="mt-1 text-xs text-ink/50">
        {placement
          ? `The cluster scheduler runs this instance on ${placement.placed}${
              placement.replicas === "all" ? " (every node with room)" : ` of ${placement.replicas}`
            } node(s)${
              placement.constraints.length ? ` labelled ${placement.constraints.join(", ")}` : ""
            }, choosing the live nodes with the most budget headroom. The Deployed switch does not apply while it is placed.`
          : "Managed by hand: the Deployed switch runs it on this node. Place it to let the scheduler pick nodes."}
      </p>
      {placement?.detail ? <p className="mt-1 text-xs text-brick">{placement.detail}</p> : null}
      {runningOn.length ? (
        <p className="mt-1 break-all font-mono text-[11px] text-ink/45">
          Running on: {runningOn.join(", ")}
        </p>
      ) : null}
      <div className="mt-2 flex flex-wrap items-end gap-2">
        <label className="grid gap-1 text-xs text-ink/60">
          Replicas
          <Input
            className="w-24"
            placeholder="1 or all"
            value={replicas}
            disabled={busy}
            onChange={(e) => setReplicas(e.target.value)}
            onKeyDown={onKeyDown}
          />
        </label>
        <label className="grid min-w-[12rem] flex-1 gap-1 text-xs text-ink/60">
          Node labels
          <Input
            placeholder="nas, zone=eu, !slow"
            value={constraints}
            disabled={busy}
            onChange={(e) => setConstraints(e.target.value)}
            onKeyDown={onKeyDown}
          />
        </label>
        <Button type="button" variant="secondary" disabled={busy} onClick={save}>
          {placement ? "Update placement" : "Place with scheduler"}
        </Button>
        {placement ? (
          <Button
            type="button"
            variant="ghost"
            disabled={busy}
            onClick={() =>
              void run(() => deletePluginPlacement(instanceId), "Failed to drop placement")
            }
          >
            Manage by hand
          </Button>
        ) : null}
      </div>
    </div>
  );
}
