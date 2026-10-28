import { useCallback, useEffect, useRef, useState, type KeyboardEvent } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  isApiError,
  listConfigNodes,
  listUsers,
  putNodeLabels,
  putNodeOwner,
  putNodeRuntime,
  type ConfigNodeView,
  type ListedUser,
  type NodeCapabilities,
  type NodeStatusView,
} from "@/lib/api";
import { useLiveRefresh } from "@/lib/live";
import { age, cores, ownerName, size } from "@/lib/nodeFormat";
import { cn } from "@/lib/utils";

/**
 * How often the panel re-reads heartbeats while the live socket is down
 * (nodes report every 15 s).
 */
const REFRESH_MS = 15_000;

/** Re-read while the socket is up too, so a node that went quiet ages out. */
const OPEN_REFRESH_MS = 60_000;

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

/**
 * Capacity, budget, accounting, and availability from one heartbeat.
 *
 * @param props - The node's latest heartbeat.
 */
export function NodeMeters({ status }: { status: NodeStatusView }) {
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
  capabilities: NodeCapabilities;
  /** Users an operator may assign as the node's owner. */
  users: ListedUser[];
  onChanged: () => Promise<void>;
  /** Shows a node action's failure in the panel; `null` clears it. */
  onError: (message: string | null) => void;
  onSessionExpired: () => void;
}

/** One node: heartbeat summary, meters, and its editable runtime document. */
function NodeRow({
  node,
  capabilities,
  users,
  onChanged,
  onError,
  onSessionExpired,
}: NodeRowProps) {
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
    onError(null);
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
    const labelsChanged =
      capabilities.can_edit_labels && nextLabels.join(",") !== node.labels.join(",");
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
        {node.kind === "host" ? (
          <Badge className="bg-ink/5 text-ink/60 normal-case tracking-normal">Host node</Badge>
        ) : null}
        {node.target ? (
          <span className="font-mono text-[11px] text-ink/45">{node.target}</span>
        ) : null}
      </div>
      {capabilities.can_assign_owner ? (
        <label className="flex flex-wrap items-center gap-2 text-xs text-ink/60">
          Owner
          <select
            className="rounded-md border border-ink/15 bg-paper px-2 py-1 text-xs text-ink"
            value={node.owner ? String(node.owner.user_id) : ""}
            disabled={busy}
            onChange={(e) => {
              const value = e.target.value;
              void run(
                () => putNodeOwner(node.node_id, value === "" ? null : Number(value)),
                "Failed to assign the node's owner",
              );
            }}
          >
            <option value="">No owner</option>
            {node.owner && !users.some((user) => user.id === node.owner?.user_id) ? (
              <option value={node.owner.user_id}>
                {ownerName(node.owner)} (cannot administer nodes)
              </option>
            ) : null}
            {users.map((user) => (
              <option key={user.id} value={user.id}>
                {user.display_name || user.login_name || `user ${user.id}`} ({user.role})
              </option>
            ))}
          </select>
          <span className="text-ink/45">
            The owner may change this node's budgets, drain switch, and name.
          </span>
        </label>
      ) : node.owner ? (
        <p className="text-xs text-ink/55">Owned by {ownerName(node.owner)}</p>
      ) : null}

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
        {capabilities.can_edit_labels ? (
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
        ) : node.labels.length > 0 ? (
          <p className="grid min-w-[12rem] flex-1 gap-1 text-xs text-ink/60">
            Labels
            <span className="font-mono text-ink/70">{node.labels.join(", ")}</span>
          </p>
        ) : null}
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
  /** Called on a 401 so the page can return to sign-in. */
  onSessionExpired: () => void;
}

/**
 * Cluster nodes as their heartbeats report them, with each node's budget
 * and drain switch. The scheduler places plugins by this headroom, and
 * deliveries go to the live node with the most room first.
 */
export function NodesPanel({ onSessionExpired }: NodesPanelProps) {
  const [nodes, setNodes] = useState<ConfigNodeView[] | null>(null);
  const [capabilities, setCapabilities] = useState<NodeCapabilities | null>(null);
  const [users, setUsers] = useState<ListedUser[]>([]);
  // Errors stay on this panel: a failed node edit is not a settings-page
  // failure and must not follow the user to another tab.
  const [loadError, setLoadError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  // The timer outlives renders; read the latest callback through a ref so
  // a parent re-render does not restart polling.
  const callbacks = useRef({ onSessionExpired });
  callbacks.current = { onSessionExpired };

  const refresh = useCallback(async () => {
    try {
      const body = await listConfigNodes();
      setNodes(body.nodes);
      setCapabilities(body.capabilities);
      setLoadError(null);
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        callbacks.current.onSessionExpired();
        return;
      }
      setLoadError(err instanceof Error ? err.message : "Failed to load nodes");
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);
  useLiveRefresh(["nodes"], refresh, { pollMs: REFRESH_MS, openPollMs: OPEN_REFRESH_MS });

  // Only an operator assigns owners. Any active user may own a node (trust
  // still follows the user's role); the API refuses a disabled user.
  const canAssignOwner = capabilities?.can_assign_owner ?? false;
  useEffect(() => {
    if (!canAssignOwner) return;
    listUsers()
      .then((all) => setUsers(all.filter((user) => user.status === "active")))
      .catch(() => setUsers([]));
  }, [canAssignOwner]);

  const owned = capabilities?.scope === "owned";

  return (
    <section className="space-y-5">
      <div className="space-y-1">
        <h2 className="text-lg font-semibold text-ink">{owned ? "Your nodes" : "Nodes"}</h2>
        <p className="text-sm text-ink/55">
          Every daemon reports its capacity, use, and running guests every 15 seconds through the
          shared database. The budget is the share of a node's CPU and memory the scheduler may
          commit to plugin guests; the bar shows what running guests reserve and the marker what the
          machine measures in use. Draining a node moves its scheduled plugins elsewhere; plugins
          deployed by hand keep running.
        </p>
      </div>
      {loadError || actionError ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {actionError ?? loadError}
        </p>
      ) : null}
      {nodes == null || capabilities == null ? (
        <p className="text-sm text-ink/50">Loading nodes…</p>
      ) : nodes.length === 0 ? (
        <p className="text-sm text-ink/50">
          {owned ? "You do not own any nodes yet." : "No nodes registered."}
        </p>
      ) : (
        <ul className="divide-y divide-ink/10 bg-card">
          {nodes.map((node) => (
            <NodeRow
              key={node.node_id}
              node={node}
              capabilities={capabilities}
              users={users}
              onChanged={refresh}
              onError={setActionError}
              onSessionExpired={onSessionExpired}
            />
          ))}
        </ul>
      )}
    </section>
  );
}
