import { useCallback, useEffect, useRef, useState, type KeyboardEvent } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  isApiError,
  listConfigNodes,
  putNodeLabels,
  putNodeRuntime,
  type ConfigNodeView,
  type NodeStatusView,
} from "@/lib/api";
import { cn } from "@/lib/utils";

/** How often the panel re-reads heartbeats (nodes report every 15 s). */
const REFRESH_MS = 15_000;

/** `1536` millicores → `1.5`. */
function cores(millicores: number): string {
  return (millicores / 1000).toFixed(millicores % 1000 === 0 ? 0 : 1);
}

/** Bytes → `3.2 GiB` / `512 MiB`. */
function size(bytes: number | null | undefined): string {
  if (bytes == null) return "?";
  const gib = bytes / 2 ** 30;
  return gib >= 1 ? `${gib.toFixed(1)} GiB` : `${Math.round(bytes / 2 ** 20)} MiB`;
}

/** Seconds since an RFC 3339 time, as `12 s ago` / `3 min ago` / `2 h ago`. */
function age(iso: string): string {
  const seconds = Math.max(0, Math.round((Date.now() - Date.parse(iso)) / 1000));
  if (!Number.isFinite(seconds)) return "unknown";
  if (seconds < 90) return `${seconds} s ago`;
  if (seconds < 90 * 60) return `${Math.round(seconds / 60)} min ago`;
  if (seconds < 48 * 3600) return `${Math.round(seconds / 3600)} h ago`;
  return `${Math.round(seconds / 86400)} d ago`;
}

/** Splits `nas, zone=eu` into trimmed, non-empty labels. */
function parseLabels(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((part) => part.trim())
    .filter(Boolean);
}

/**
 * A committed-over-budget bar: the filled part is what running guests
 * reserve, the marker what the machine measures in use.
 */
function Meter({
  label,
  committed,
  measured,
  budget,
  text,
}: {
  label: string;
  committed: number;
  measured: number | null;
  budget: number | null;
  text: string;
}) {
  const scale = budget && budget > 0 ? budget : 1;
  const fill = Math.min(100, (committed / scale) * 100);
  const mark = measured == null ? null : Math.min(100, (measured / scale) * 100);
  const over = budget != null && committed > budget;
  return (
    <div className="grid gap-1">
      <div className="flex items-baseline justify-between gap-2 text-xs">
        <span className="font-medium text-ink/70">{label}</span>
        <span className={cn("font-mono text-[11px]", over ? "text-brick" : "text-ink/55")}>
          {text}
        </span>
      </div>
      <div className="relative h-1.5 overflow-hidden rounded-full bg-ink/10">
        <div
          className={cn("h-full rounded-full", over ? "bg-brick" : "bg-teal")}
          style={{ width: `${fill}%` }}
        />
        {mark != null ? (
          <div className="absolute inset-y-0 w-0.5 bg-ink/60" style={{ left: `${mark}%` }} />
        ) : null}
      </div>
    </div>
  );
}

/** Capacity, budget, accounting, and availability from one heartbeat. */
function NodeMeters({ status }: { status: NodeStatusView }) {
  const memoryUsed =
    status.memory_bytes != null && status.memory_available_bytes != null
      ? status.memory_bytes - status.memory_available_bytes
      : null;
  return (
    <div className="grid gap-3 sm:grid-cols-2">
      <Meter
        label="CPU"
        committed={status.reserved.cpu_millicores}
        measured={status.cpu_used_millicores}
        budget={status.cpu_budget_millicores}
        text={`${cores(status.reserved.cpu_millicores)} reserved / ${cores(
          status.cpu_budget_millicores,
        )} budget of ${cores(status.cpu_millicores)} ${status.cpu_millicores === 1000 ? "core" : "cores"}${
          status.cpu_used_millicores != null ? ` · ${cores(status.cpu_used_millicores)} in use` : ""
        }`}
      />
      <Meter
        label="Memory"
        committed={status.reserved.memory_bytes}
        measured={memoryUsed}
        budget={status.memory_budget_bytes}
        text={`${size(status.reserved.memory_bytes)} reserved / ${size(
          status.memory_budget_bytes,
        )} budget of ${size(status.memory_bytes)}${
          status.memory_available_bytes != null
            ? ` · ${size(status.memory_available_bytes)} free`
            : ""
        }`}
      />
    </div>
  );
}

interface NodeRowProps {
  node: ConfigNodeView;
  onChanged: () => Promise<void>;
  onError: (message: string) => void;
  onSessionExpired: () => void;
}

/** One node: heartbeat summary, meters, and its editable runtime document. */
function NodeRow({ node, onChanged, onError, onSessionExpired }: NodeRowProps) {
  const [label, setLabel] = useState(node.runtime.label);
  const [cpu, setCpu] = useState(String(node.runtime.cpu_budget_percent));
  const [memory, setMemory] = useState(String(node.runtime.memory_budget_percent));
  const [labels, setLabels] = useState(node.labels.join(", "));
  const [busy, setBusy] = useState(false);

  // Reset a field only when its stored value changes. Every poll hands back
  // new objects with the same values, which must not wipe an edit in
  // progress, and a change to one field must not wipe another.
  const storedLabel = node.runtime.label;
  const storedCpu = String(node.runtime.cpu_budget_percent);
  const storedMemory = String(node.runtime.memory_budget_percent);
  const storedLabels = node.labels.join(", ");
  useEffect(() => setLabel(storedLabel), [storedLabel]);
  useEffect(() => setCpu(storedCpu), [storedCpu]);
  useEffect(() => setMemory(storedMemory), [storedMemory]);
  useEffect(() => setLabels(storedLabels), [storedLabels]);

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
    const cpuPercent = Number.parseInt(cpu.trim(), 10);
    const memoryPercent = Number.parseInt(memory.trim(), 10);
    for (const [name, value] of [
      ["CPU budget", cpuPercent],
      ["Memory budget", memoryPercent],
    ] as const) {
      if (!Number.isInteger(value) || value < 1 || value > 100) {
        onError(`${name} must be a whole percentage between 1 and 100.`);
        return;
      }
    }
    const nextLabels = parseLabels(labels);
    const labelsChanged = nextLabels.join(",") !== node.labels.join(",");
    void run(async () => {
      await putNodeRuntime(node.node_id, {
        label: label.trim(),
        cpu_budget_percent: cpuPercent,
        memory_budget_percent: memoryPercent,
      });
      if (labelsChanged) await putNodeLabels(node.node_id, nextLabels);
    }, "Failed to save node");
  }

  /** Enter saves the node. */
  function onKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.key === "Enter") {
      event.preventDefault();
      save();
    }
  }

  const status = node.status;
  const drained = !node.runtime.schedulable;
  return (
    <li className="space-y-3 px-4 py-4">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-semibold text-ink">{node.runtime.label || node.node_id}</span>
        {node.runtime.label ? (
          <span className="break-all font-mono text-[11px] text-ink/45">{node.node_id}</span>
        ) : null}
        {node.this_node ? (
          <Badge className="bg-ink/10 text-ink/70 normal-case tracking-normal">This node</Badge>
        ) : null}
        <Badge
          className={cn(
            "normal-case tracking-normal",
            node.live ? "bg-teal/10 text-ink/70" : "bg-brick/10 text-brick",
          )}
        >
          {node.live ? "Live" : "Offline"}
        </Badge>
        {drained ? (
          <Badge className="bg-brick/10 text-brick normal-case tracking-normal">Drained</Badge>
        ) : null}
        {node.target ? (
          <span className="font-mono text-[11px] text-ink/45">{node.target}</span>
        ) : null}
      </div>

      {status ? (
        <>
          <p className="text-xs text-ink/55">
            Heartbeat {age(status.heartbeat_at)} · {status.running_guests} guest(s) running ·{" "}
            {(status.availability_ppm / 10_000).toFixed(1)}% available recently
            {status.files_dir_free_bytes != null
              ? ` · ${size(status.files_dir_free_bytes)} free disk`
              : ""}
          </p>
          <div className={cn(!node.live && "opacity-50")}>
            <NodeMeters status={status} />
          </div>
        </>
      ) : (
        <p className="text-xs text-ink/55">
          No heartbeat yet: this node's daemon has not run since heartbeats were added.
        </p>
      )}

      <div className="flex flex-wrap items-end gap-2">
        <label className="grid min-w-[10rem] flex-1 gap-1 text-xs text-ink/60">
          Name
          <Input
            placeholder="Optional"
            value={label}
            disabled={busy}
            onChange={(e) => setLabel(e.target.value)}
            onKeyDown={onKeyDown}
          />
        </label>
        <label className="grid gap-1 text-xs text-ink/60">
          CPU budget %
          <Input
            className="w-20"
            inputMode="numeric"
            value={cpu}
            disabled={busy}
            onChange={(e) => setCpu(e.target.value)}
            onKeyDown={onKeyDown}
          />
        </label>
        <label className="grid gap-1 text-xs text-ink/60">
          Memory budget %
          <Input
            className="w-20"
            inputMode="numeric"
            value={memory}
            disabled={busy}
            onChange={(e) => setMemory(e.target.value)}
            onKeyDown={onKeyDown}
          />
        </label>
        <label className="grid min-w-[12rem] flex-1 gap-1 text-xs text-ink/60">
          Labels
          <Input
            placeholder="nas, zone=eu"
            value={labels}
            disabled={busy}
            onChange={(e) => setLabels(e.target.value)}
            onKeyDown={onKeyDown}
          />
        </label>
        <Button type="button" variant="secondary" disabled={busy} onClick={save}>
          Save
        </Button>
        <Button
          type="button"
          variant="ghost"
          disabled={busy}
          onClick={() =>
            void run(
              () => putNodeRuntime(node.node_id, { schedulable: drained }),
              drained ? "Failed to resume node" : "Failed to drain node",
            )
          }
        >
          {drained ? "Resume scheduling" : "Drain"}
        </Button>
      </div>
    </li>
  );
}

interface NodesPanelProps {
  /** Shows a page-level error. */
  onError: (message: string) => void;
  /** Called on a 401 so the page can return to sign-in. */
  onSessionExpired: () => void;
}

/**
 * Cluster nodes as their heartbeats report them, with each node's budget
 * and drain switch. The scheduler places plugins by this headroom, and
 * deliveries go to the live node with the most room first.
 */
export function NodesPanel({ onError, onSessionExpired }: NodesPanelProps) {
  const [nodes, setNodes] = useState<ConfigNodeView[] | null>(null);
  // The timer outlives renders; read the latest callbacks through a ref so
  // a parent re-render does not restart polling.
  const callbacks = useRef({ onError, onSessionExpired });
  callbacks.current = { onError, onSessionExpired };

  const refresh = useCallback(async () => {
    try {
      const body = await listConfigNodes();
      setNodes(body.nodes);
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        callbacks.current.onSessionExpired();
        return;
      }
      callbacks.current.onError(err instanceof Error ? err.message : "Failed to load nodes");
    }
  }, []);

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), REFRESH_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  return (
    <section className="space-y-5">
      <div className="space-y-1">
        <h2 className="text-lg font-semibold text-ink">Nodes</h2>
        <p className="text-sm text-ink/55">
          Every daemon reports its capacity, use, and running guests every 15 seconds through the
          shared database. The budget is the share of a node's CPU and memory the scheduler may
          commit to plugin guests; the bar shows what running guests reserve and the marker what
          the machine measures in use. Draining a node moves its scheduled plugins elsewhere;
          plugins deployed by hand keep running.
        </p>
      </div>
      {nodes == null ? (
        <p className="text-sm text-ink/50">Loading nodes…</p>
      ) : nodes.length === 0 ? (
        <p className="text-sm text-ink/50">No nodes registered.</p>
      ) : (
        <ul className="divide-y divide-ink/10 bg-card">
          {nodes.map((node) => (
            <NodeRow
              key={node.node_id}
              node={node}
              onChanged={refresh}
              onError={onError}
              onSessionExpired={onSessionExpired}
            />
          ))}
        </ul>
      )}
    </section>
  );
}
