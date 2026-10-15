import { useCallback, useEffect, useRef, useState } from "react";
import type { AppNavProps } from "@/components/AppNav";
import { AppTopBar } from "@/components/AppTopBar";
import { NodeMeters } from "@/components/NodesPanel";
import { Badge } from "@/components/ui/badge";
import {
  fetchClusterStatus,
  isApiError,
  signOut,
  type AuthRole,
  type ClusterStatusResponse,
  type ConfigNodeView,
} from "@/lib/api";
import { age, cores, ownerName, size } from "@/lib/nodeFormat";
import { cn, pageWidthClass } from "@/lib/utils";

/** How often the page re-reads cluster status (nodes report every 15 s). */
const REFRESH_MS = 15_000;

/** One headline figure. */
function Stat({ label, value, detail }: { label: string; value: string; detail?: string }) {
  return (
    <div className="rounded-lg border border-ink/10 bg-card px-4 py-3">
      <p className="text-xs font-medium uppercase tracking-wide text-ink/50">{label}</p>
      <p className="mt-1 font-display text-2xl font-semibold text-ink">{value}</p>
      {detail ? <p className="mt-0.5 text-xs text-ink/55">{detail}</p> : null}
    </div>
  );
}

/** A visible node, read-only: identity, liveness, owner, and meters. */
function NodeCard({ node }: { node: ConfigNodeView }) {
  const status = node.status;
  return (
    <li className="space-y-3 px-4 py-4">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-semibold text-ink">{node.runtime.label || node.node_id}</span>
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
        {!node.runtime.schedulable ? (
          <Badge className="bg-brick/10 text-brick normal-case tracking-normal">Drained</Badge>
        ) : null}
        {node.kind === "host" ? (
          <Badge className="bg-ink/5 text-ink/60 normal-case tracking-normal">Host node</Badge>
        ) : null}
        {node.owner ? (
          <span className="text-xs text-ink/55">Owned by {ownerName(node.owner)}</span>
        ) : null}
      </div>
      {status ? (
        <>
          <p className="text-xs text-ink/55">
            Heartbeat {age(status.heartbeat_at)} · {status.running_guests} guest(s) running ·{" "}
            {(status.availability_ppm / 10_000).toFixed(1)}% available recently
          </p>
          <div className={cn(!node.live && "opacity-50")}>
            <NodeMeters status={status} />
          </div>
        </>
      ) : (
        <p className="text-xs text-ink/55">No heartbeat yet.</p>
      )}
    </li>
  );
}

/**
 * Cluster status: health every node administrator may see (nodes live,
 * committed capacity, active jobs, delivery backlog) and the nodes the
 * caller may see. An operator sees every node; a Host, Administrator, or
 * Owner signed in to the portal sees the nodes they own.
 *
 * @param props - Sign-out callback, navigation, and the session role.
 */
export function StatusPage({
  onLogout,
  nav,
  role,
}: {
  onLogout: () => void;
  nav: AppNavProps;
  role?: AuthRole;
}) {
  const [status, setStatus] = useState<ClusterStatusResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const logout = useRef(onLogout);
  logout.current = onLogout;

  const refresh = useCallback(async () => {
    try {
      setStatus(await fetchClusterStatus());
      setError(null);
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        logout.current();
        return;
      }
      setError(err instanceof Error ? err.message : "Failed to load cluster status");
    }
  }, []);

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), REFRESH_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  async function onSignOut() {
    await signOut(role);
    onLogout();
  }

  const cluster = status?.cluster;
  const owned = status?.capabilities.scope === "owned";
  return (
    <div className="flex h-full flex-col">
      <header className="sticky top-0 z-10 border-b border-ink/10 bg-paper/85 px-3 py-3 backdrop-blur-md sm:px-5">
        <div className={pageWidthClass}>
          <AppTopBar nav={nav} onSignOut={onSignOut} />
        </div>
      </header>
      <div className="flex-1 overflow-y-auto px-3 py-5 sm:px-5">
        <main className={cn("space-y-6", pageWidthClass)}>
          <div className="space-y-1">
            <h1 className="font-display text-2xl font-semibold tracking-tight text-ink">Status</h1>
            <p className="text-sm text-ink/60">
              Cluster health from every node's heartbeat.{" "}
              {owned
                ? "Below are the nodes you own; change their budgets in Settings → Nodes."
                : "Change budgets, labels, and owners in Settings → Nodes."}
            </p>
          </div>
          {error ? <p className="text-sm text-brick">{error}</p> : null}
          {cluster == null || status == null ? (
            <p className="text-sm text-ink/50">Loading status…</p>
          ) : (
            <>
              <section className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
                <Stat
                  label="Nodes"
                  value={`${cluster.live} / ${cluster.nodes} live`}
                  detail={[
                    cluster.drained > 0 ? `${cluster.drained} drained` : null,
                    cluster.host_nodes > 0 ? `${cluster.host_nodes} host node(s)` : null,
                  ]
                    .filter(Boolean)
                    .join(" · ")}
                />
                <Stat
                  label="CPU committed"
                  value={`${cores(cluster.cpu_reserved_millicores)} / ${cores(
                    cluster.cpu_budget_millicores,
                  )} cores`}
                  detail={`${cluster.running_guests} guest(s) running`}
                />
                <Stat
                  label="Memory committed"
                  value={`${size(cluster.memory_reserved_bytes)} / ${size(
                    cluster.memory_budget_bytes,
                  )}`}
                />
                <Stat
                  label="Work"
                  value={`${cluster.active_jobs} job(s)`}
                  detail={`${cluster.deliveries.pending} deliveries pending${
                    cluster.deliveries.dead_letter > 0
                      ? ` · ${cluster.deliveries.dead_letter} dead-lettered`
                      : ""
                  }`}
                />
              </section>
              <section className="space-y-3">
                <h2 className="text-lg font-semibold text-ink">
                  {owned ? "Your nodes" : "Nodes"}
                </h2>
                {status.nodes.length === 0 ? (
                  <p className="text-sm text-ink/50">
                    {owned ? "You do not own any nodes yet." : "No nodes registered."}
                  </p>
                ) : (
                  <ul className="divide-y divide-ink/10 rounded-lg border border-ink/10 bg-card">
                    {status.nodes.map((node) => (
                      <NodeCard key={node.node_id} node={node} />
                    ))}
                  </ul>
                )}
              </section>
            </>
          )}
        </main>
      </div>
    </div>
  );
}
