import { useCallback, useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { isApiError, type ConfigNodeView } from "@/lib/api";
import {
  sharedBranchNodes,
  joinErrorMessage,
  listSharedBranches,
  removeSharedBranch,
} from "@/lib/join";

/** Props for {@link SharedBranchesPanel}. */
export interface SharedBranchesPanelProps {
  /** Called on a 401 so the page can sign out. */
  onSessionExpired: () => void;
}

/**
 * Settings → Shared branches on the library (operator or Owner): the servers that
 * share this library's database and keys (owner nodes), including this one.
 * A shared branch that no longer runs can be removed here; a running one is stopped
 * on its own server first. Branches are listed under Branches, not here.
 */
export function SharedBranchesPanel({ onSessionExpired }: SharedBranchesPanelProps) {
  const [nodes, setNodes] = useState<ConfigNodeView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const body = await listSharedBranches();
      setNodes(sharedBranchNodes(body.nodes));
      setError(null);
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        onSessionExpired();
        return;
      }
      setError(joinErrorMessage(err, "Could not load the shared branches."));
    }
  }, [onSessionExpired]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function onRemove(nodeId: string) {
    if (!window.confirm(`Remove shared branch ${nodeId} from this library?`)) return;
    setBusy(nodeId);
    try {
      await removeSharedBranch(nodeId);
      await refresh();
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        onSessionExpired();
        return;
      }
      setError(joinErrorMessage(err, "Could not remove that shared branch."));
    } finally {
      setBusy(null);
    }
  }

  return (
    <section className="space-y-6">
      <div className="space-y-1">
        <h2 className="text-lg font-semibold text-ink">Shared branches</h2>
        <p className="text-sm text-ink/55">
          Shared branches share this library's database and keys, so they serve the same library as
          this server. Only an Owner can let a server join this way, and only when the database is
          PostgreSQL or Cloudflare D1. To add one, choose <strong>Join a library</strong> on an
          empty server and pick <strong>shared branch</strong> when you approve it here.
        </p>
      </div>
      {error ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {error}
        </p>
      ) : null}
      {nodes == null ? (
        error ? null : (
          <p className="text-sm text-ink/50">Loading…</p>
        )
      ) : nodes.length === 0 ? (
        <p className="text-sm text-ink/50">No shared branches.</p>
      ) : (
        <ul className="divide-y divide-ink/10 rounded-md border border-ink/10">
          {nodes.map((node) => (
            <li key={node.node_id} className="flex items-center justify-between gap-3 px-4 py-3">
              <div className="min-w-0 space-y-0.5">
                <p className="truncate font-mono text-xs text-ink">{node.node_id}</p>
                <p className="text-xs text-ink/55">
                  {node.this_node ? "This server" : node.live ? "Running" : "Not running"}
                  {node.software_version ? ` · ${node.software_version}` : ""}
                </p>
              </div>
              {node.this_node || node.live ? null : (
                <Button
                  type="button"
                  variant="secondary"
                  disabled={busy !== null}
                  onClick={() => void onRemove(node.node_id)}
                >
                  Remove
                </Button>
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
