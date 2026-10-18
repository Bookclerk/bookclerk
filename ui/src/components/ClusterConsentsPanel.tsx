import { useCallback, useEffect, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  approveClusterConsent,
  isApiError,
  listClusterConsents,
  revokeClusterConsent,
  type ClusterLinkView,
  type RemoteConsentState,
  type RemoteConsentView,
  type RemoteGrantView,
} from "@/lib/api";
import { useLiveRefresh } from "@/lib/live";
import { cn } from "@/lib/utils";

/** Poll interval while the live socket is down. */
const REFRESH_MS = 15_000;

const STATE_LABEL: Record<RemoteConsentState, string> = {
  consented: "Approved",
  awaiting_host_consent: "Awaiting approval",
  awaiting_host_reapproval: "Asks for more",
  invalid_grant: "Invalid request",
};

function StateBadge({ state }: { state: RemoteConsentState }) {
  return (
    <Badge
      className={cn(
        "normal-case tracking-normal",
        state === "consented"
          ? "bg-teal/10 text-ink/70"
          : state === "invalid_grant"
            ? "bg-brick/10 text-brick"
            : "bg-fold text-ink/80",
      )}
    >
      {STATE_LABEL[state] ?? state}
    </Badge>
  );
}

function LinkRow({ link }: { link: ClusterLinkView }) {
  return (
    <li className="flex flex-wrap items-center gap-2 px-4 py-3 text-sm">
      <span className="font-semibold text-ink">{link.owner_origin}</span>
      <span className="break-all font-mono text-[11px] text-ink/45">{link.cluster_id}</span>
      <Badge
        className={cn(
          "normal-case tracking-normal",
          link.state === "active" ? "bg-teal/10 text-ink/70" : "bg-brick/10 text-brick",
        )}
      >
        {link.state === "active" ? "Active" : link.state === "left" ? "Left" : "Revoked"}
      </Badge>
      {link.pinned ? (
        <Badge className="bg-ink/5 text-ink/60 normal-case tracking-normal">Pinned certificate</Badge>
      ) : null}
      <span className="text-xs text-ink/55">
        node <span className="font-mono">{link.node_id}</span> · joined{" "}
        {new Date(link.enrolled_at).toLocaleString()}
      </span>
    </li>
  );
}

/** Read-only lines of a grant that cannot be narrowed here. */
function GrantFacts({ grant }: { grant: RemoteGrantView }) {
  const facts: string[] = [];
  if (grant.tcp?.length) {
    facts.push(`TCP: ${grant.tcp.map((t) => `${t.host}:${t.ports.join(",")}`).join(", ")}`);
  }
  if (grant.addressCidrs?.length) facts.push(`Addresses: ${grant.addressCidrs.join(", ")}`);
  if (grant.jobs?.length) facts.push(`Jobs: ${grant.jobs.join(", ")}`);
  if (grant.memoryMib) facts.push(`Memory: ${grant.memoryMib} MiB`);
  if (grant.cpuRatePercent) facts.push(`CPU: ${grant.cpuRatePercent}%`);
  if (grant.diskMib) facts.push(`Disk: ${grant.diskMib} MiB`);
  if (facts.length === 0) return null;
  return (
    <ul className="space-y-0.5 text-xs text-ink/55">
      {facts.map((fact) => (
        <li key={fact}>{fact}</li>
      ))}
    </ul>
  );
}

function ConsentRow({
  row,
  onChanged,
  onSessionExpired,
}: {
  row: RemoteConsentView;
  onChanged: () => Promise<void>;
  onSessionExpired: () => void;
}) {
  const requested = row.requested_grant ?? {};
  const domains = requested.domains ?? [];
  const bindings = requested.bindings ?? [];
  const networked = Boolean(requested.networkMode && requested.networkMode !== "deny");
  const [deniedDomains, setDeniedDomains] = useState<Set<string>>(new Set());
  const [deniedBindings, setDeniedBindings] = useState<Set<string>>(new Set());
  const [denyNetwork, setDenyNetwork] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // A new request (another version, or more asked for) starts unnarrowed.
  const requestKey = `${row.requested_version}|${JSON.stringify(requested)}`;
  useEffect(() => {
    setDeniedDomains(new Set());
    setDeniedBindings(new Set());
    setDenyNetwork(false);
    setError(null);
  }, [requestKey]);

  function toggle(set: Set<string>, value: string, on: boolean): Set<string> {
    const next = new Set(set);
    if (on) next.add(value);
    else next.delete(value);
    return next;
  }

  async function run(action: () => Promise<unknown>) {
    setBusy(true);
    setError(null);
    try {
      await action();
      await onChanged();
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        onSessionExpired();
        return;
      }
      // 409 carries the refusal's reason (a version changed, a domain not asked for).
      setError(err instanceof Error ? err.message : "Request failed");
    } finally {
      setBusy(false);
    }
  }

  const approved = row.grant != null;
  return (
    <li className="space-y-3 px-4 py-4">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-semibold text-ink">{requested.pluginId ?? row.plugin_key}</span>
        <span className="font-mono text-[11px] text-ink/60">{row.requested_version}</span>
        <StateBadge state={row.state} />
        <span className="break-all font-mono text-[11px] text-ink/45">{row.plugin_key}</span>
      </div>
      <p className="text-xs text-ink/55">
        From cluster <span className="font-mono">{row.cluster_id}</span>
        {approved && row.approved_version
          ? ` · approved ${row.approved_version}${row.approved_by ? ` by ${row.approved_by}` : ""}`
          : ""}
      </p>

      {row.state !== "consented" ? (
        <div className="grid gap-3 sm:grid-cols-2">
          <fieldset className="space-y-1" disabled={busy}>
            <legend className="text-xs font-medium text-ink/70">
              Network ({requested.networkMode ?? "deny"})
            </legend>
            {networked ? (
              <label className="flex items-center gap-2 text-sm">
                <input
                  type="checkbox"
                  checked={denyNetwork}
                  onChange={(e) => setDenyNetwork(e.target.checked)}
                />
                Deny all network access
              </label>
            ) : null}
            {domains.length === 0 ? (
              <p className="text-xs text-ink/50">No domains requested.</p>
            ) : (
              domains.map((domain) => (
                <label key={domain} className="flex items-center gap-2 text-sm">
                  <input
                    type="checkbox"
                    disabled={denyNetwork}
                    checked={denyNetwork || deniedDomains.has(domain)}
                    onChange={(e) =>
                      setDeniedDomains((set) => toggle(set, domain, e.target.checked))
                    }
                  />
                  <span className="text-ink/45">Deny</span>
                  <span className="font-mono text-[12px]">{domain}</span>
                </label>
              ))
            )}
          </fieldset>
          <fieldset className="space-y-1" disabled={busy}>
            <legend className="text-xs font-medium text-ink/70">Bindings</legend>
            {bindings.length === 0 ? (
              <p className="text-xs text-ink/50">No bindings requested.</p>
            ) : (
              bindings.map((binding) => (
                <label key={binding} className="flex items-center gap-2 text-sm">
                  <input
                    type="checkbox"
                    checked={deniedBindings.has(binding)}
                    onChange={(e) =>
                      setDeniedBindings((set) => toggle(set, binding, e.target.checked))
                    }
                  />
                  <span className="text-ink/45">Deny</span>
                  <span className="font-mono text-[12px]">{binding}</span>
                </label>
              ))
            )}
          </fieldset>
        </div>
      ) : null}
      <GrantFacts grant={requested} />
      {approved && row.grant ? (
        <p className="text-xs text-ink/55">
          Granted now: network {row.grant.networkMode ?? "deny"}
          {row.grant.domains?.length ? ` (${row.grant.domains.join(", ")})` : ""}; bindings{" "}
          {row.grant.bindings?.length ? row.grant.bindings.join(", ") : "none"}
        </p>
      ) : null}

      {error ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {error}
        </p>
      ) : null}
      <div className="flex flex-wrap gap-2">
        {row.state !== "consented" ? (
          <Button
            type="button"
            variant="secondary"
            disabled={busy || row.state === "invalid_grant"}
            onClick={() =>
              void run(() =>
                approveClusterConsent({
                  cluster_id: row.cluster_id,
                  plugin_key: row.plugin_key,
                  version: row.requested_version,
                  deny_domains: denyNetwork ? [] : [...deniedDomains],
                  deny_bindings: [...deniedBindings],
                  deny_network: denyNetwork,
                }),
              )
            }
          >
            {row.state === "awaiting_host_reapproval" ? `Approve ${row.requested_version}` : "Approve"}
          </Button>
        ) : null}
        {approved ? (
          <Button
            type="button"
            variant="danger"
            disabled={busy}
            onClick={() => void run(() => revokeClusterConsent(row.cluster_id, row.plugin_key))}
          >
            Revoke
          </Button>
        ) : null}
      </div>
    </li>
  );
}

/** Props for {@link ClusterConsentsPanel}. */
export interface ClusterConsentsPanelProps {
  /** Called on a 401 so the page can sign out. */
  onSessionExpired: () => void;
}

/**
 * This install as a host of owner clusters: the clusters it joined, and
 * each owner plugin's request to run here. The operator approves a
 * request (optionally without some domains, bindings, or the network) or
 * revokes it. Joining happens on the command line (`bookclerk clusters
 * join`), since the enrollment link is a secret.
 */
export function ClusterConsentsPanel({ onSessionExpired }: ClusterConsentsPanelProps) {
  const [clusters, setClusters] = useState<ClusterLinkView[] | null>(null);
  const [consents, setConsents] = useState<RemoteConsentView[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const expired = useRef(onSessionExpired);
  useEffect(() => {
    expired.current = onSessionExpired;
  }, [onSessionExpired]);

  const refresh = useCallback(async () => {
    try {
      const body = await listClusterConsents();
      setClusters(body.clusters);
      setConsents(body.consents);
      setLoadError(null);
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        expired.current();
        return;
      }
      setLoadError(err instanceof Error ? err.message : "Failed to load clusters");
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);
  useLiveRefresh(["consents"], refresh, { pollMs: REFRESH_MS });

  const pending = consents.filter((row) => row.state !== "consented");
  const approved = consents.filter((row) => row.state === "consented");

  return (
    <section className="space-y-6">
      <div className="space-y-1">
        <h2 className="text-lg font-semibold text-ink">Clusters</h2>
        <p className="text-sm text-ink/55">
          Owner clusters this server contributes to as a host. An owner's plugin runs here only
          after you approve what it asks for; you can leave out domains, bindings, or the network,
          and a later version that asks for more waits for approval again. Revoking stops its jobs
          here and hands them back to the owner. To join a cluster, run{" "}
          <code className="font-mono text-[12px]">bookclerk clusters join</code> on this server and
          paste the enrollment link when asked.
        </p>
      </div>
      {loadError ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {loadError}
        </p>
      ) : null}
      {clusters == null ? (
        <p className="text-sm text-ink/50">Loading clusters…</p>
      ) : (
        <>
          {clusters.length === 0 ? (
            <p className="text-sm text-ink/50">This server has not joined any cluster.</p>
          ) : (
            <ul className="divide-y divide-ink/10 bg-card">
              {clusters.map((link) => (
                <LinkRow key={link.cluster_id} link={link} />
              ))}
            </ul>
          )}
          <div className="space-y-2">
            <h3 className="text-sm font-semibold text-ink">
              Waiting for you{pending.length ? ` (${pending.length})` : ""}
            </h3>
            {pending.length === 0 ? (
              <p className="text-sm text-ink/50">No requests waiting.</p>
            ) : (
              <ul className="divide-y divide-ink/10 bg-card">
                {pending.map((row) => (
                  <ConsentRow
                    key={`${row.cluster_id}|${row.plugin_key}`}
                    row={row}
                    onChanged={refresh}
                    onSessionExpired={onSessionExpired}
                  />
                ))}
              </ul>
            )}
          </div>
          {approved.length ? (
            <div className="space-y-2">
              <h3 className="text-sm font-semibold text-ink">Approved</h3>
              <ul className="divide-y divide-ink/10 bg-card">
                {approved.map((row) => (
                  <ConsentRow
                    key={`${row.cluster_id}|${row.plugin_key}`}
                    row={row}
                    onChanged={refresh}
                    onSessionExpired={onSessionExpired}
                  />
                ))}
              </ul>
            </div>
          ) : null}
        </>
      )}
    </section>
  );
}
