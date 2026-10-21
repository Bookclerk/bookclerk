import { useCallback, useEffect, useState } from "react";
import { AddBranchDialog } from "@/components/AddBranchDialog";
import { Button } from "@/components/ui/button";
import { isApiError } from "@/lib/api";
import {
  branchSharingSummary,
  branchTitlesNote,
  fetchBranchSettings,
  fingerprintGroups,
  joinErrorMessage,
  putBranchSettings,
  removeBranchTitles,
  type BranchRowView,
  type BranchSettingsView,
} from "@/lib/join";

/** Props for {@link BranchesPanel}. */
export interface BranchesPanelProps {
  /** Called on a 401 so the page can sign out. */
  onSessionExpired: () => void;
  /** True for a signed-in user (who can add a branch of their own). */
  canAddOwn: boolean;
}

/**
 * One branch: its owner, what it shares (read-only; the branch's operator
 * sets it in Settings → Library there), and a Remove for the entries of
 * titles it no longer shares or left behind.
 */
function BranchRow({
  row,
  busy,
  onRemove,
}: {
  row: BranchRowView;
  busy: boolean;
  onRemove: (nodeId: string) => void;
}) {
  const note = branchTitlesNote(row.titles, row.joined);
  return (
    <li className="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 py-3 text-sm">
      <span className="font-semibold text-ink">
        {row.owner_name ? `${row.owner_name}'s branch` : "Branch"}
      </span>
      <span className="break-all font-mono text-[11px] text-ink/45">{row.node_id}</span>
      <span className="w-full text-xs text-ink/60">{branchSharingSummary(row)}</span>
      {note ? <span className="w-full text-xs text-ink/50">{note}</span> : null}
      {row.titles.detached > 0 ? (
        <Button
          type="button"
          variant="secondary"
          className="h-8"
          disabled={busy}
          onClick={() => onRemove(row.node_id)}
        >
          Remove {row.titles.detached} {row.titles.detached === 1 ? "entry" : "entries"}
        </Button>
      ) : null}
    </li>
  );
}

/**
 * Settings → Branches on the library (operator or Owner): "Let new branches
 * join", the address branches use, in self-signed mode the fingerprint a
 * joining server's operator confirms, and each branch with what it shares.
 */
export function BranchesPanel({ onSessionExpired, canAddOwn }: BranchesPanelProps) {
  const [view, setView] = useState<BranchSettingsView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [adding, setAdding] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setView(await fetchBranchSettings());
      setError(null);
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        onSessionExpired();
        return;
      }
      setError(joinErrorMessage(err, "Could not load the branch settings."));
    }
  }, [onSessionExpired]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function onRemove(nodeId?: string) {
    if (
      !window.confirm(
        "Remove these entries from the library? Files that already arrived stay in your outputs.",
      )
    ) {
      return;
    }
    setBusy(true);
    try {
      await removeBranchTitles(nodeId);
      await refresh();
    } catch (err) {
      setError(joinErrorMessage(err, "Could not remove the entries."));
    } finally {
      setBusy(false);
    }
  }

  async function onToggle(next: boolean) {
    setBusy(true);
    try {
      await putBranchSettings(next);
      await refresh();
    } catch (err) {
      setError(joinErrorMessage(err, "Could not save."));
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className="space-y-6">
      <div className="space-y-1">
        <h2 className="text-lg font-semibold text-ink">Branches</h2>
        <p className="text-sm text-ink/55">
          A branch is another Bookclerk server that runs work for this library: scans and downloads
          for the accounts of the user who owns it. It keeps its own database, and its operator
          approves each plugin before it runs there. Branches show among the nodes.
        </p>
      </div>
      {error ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {error}
        </p>
      ) : null}
      {view ? (
        <div className="space-y-5">
          <label className="flex items-start gap-3 text-sm text-ink">
            <input
              type="checkbox"
              className="mt-0.5"
              checked={view.enrollment}
              disabled={busy}
              onChange={(e) => void onToggle(e.target.checked)}
            />
            <span>
              <span className="block font-medium">Let new branches join</span>
              <span className="block text-ink/55">
                When off, no sign-in or invite can add a branch. Branches already joined keep
                working.
              </span>
            </span>
          </label>
          <div className="space-y-1 text-sm">
            <p className="font-medium text-ink">Address to join</p>
            <p className="font-mono text-xs text-ink/70">{view.origin}</p>
            <p className="text-xs text-ink/50">
              On the new server, choose <strong>Join a library</strong> and enter this address.
            </p>
          </div>
          {view.spki_fingerprint ? (
            <div className="space-y-1 text-sm">
              <p className="font-medium text-ink">Certificate fingerprint</p>
              <p className="break-all rounded bg-card px-3 py-2 font-mono text-xs text-ink">
                {fingerprintGroups(view.spki_fingerprint).join(" ")}
              </p>
              <p className="text-xs text-ink/50">
                This library uses its own certificate. A joining server shows this fingerprint;
                check that it matches before you continue there.
              </p>
            </div>
          ) : null}
          {view.branches && view.branches.length > 0 ? (
            <div className="space-y-2">
              <p className="text-sm font-medium text-ink">Branches and their books</p>
              <p className="text-xs text-ink/50">
                A branch can share its books with this library; they arrive as its owner&apos;s.
                Each branch&apos;s operator chooses that there. When a branch stops sharing or
                leaves, its books stay here, marked, until you remove the entries.
              </p>
              <ul className="divide-y divide-ink/10 rounded border border-ink/10">
                {view.branches.map((row) => (
                  <BranchRow
                    key={row.node_id}
                    row={row}
                    busy={busy}
                    onRemove={(nodeId) => void onRemove(nodeId)}
                  />
                ))}
              </ul>
              {view.branches.filter((row) => row.titles.detached > 0).length > 1 ? (
                <Button
                  type="button"
                  variant="secondary"
                  disabled={busy}
                  onClick={() => void onRemove()}
                >
                  Remove all entries no longer shared
                </Button>
              ) : null}
            </div>
          ) : null}
          {canAddOwn ? (
            <div className="space-y-2">
              <Button type="button" variant="secondary" onClick={() => setAdding(true)}>
                Add a branch
              </Button>
              <p className="text-xs text-ink/50">
                For someone else's branch, use User management → their actions → Add a branch.
              </p>
            </div>
          ) : (
            <p className="text-xs text-ink/50">
              To make a branch invite, use User management → a user's actions → Add a branch.
            </p>
          )}
        </div>
      ) : error ? null : (
        <p className="text-sm text-ink/50">Loading…</p>
      )}
      <AddBranchDialog open={adding} onClose={() => setAdding(false)} />
    </section>
  );
}
