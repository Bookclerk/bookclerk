import { useCallback, useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import {
  deleteDiagnosticsReport,
  fetchDiagnosticsReport,
  fetchDiagnosticsReports,
  fetchSettings,
  saveDiagnosticsSharing,
  type DiagnosticsReport,
  type DiagnosticsReportLookup,
} from "@/lib/api";
import { diagnosticsIssueSearchUrl, formatReportTime, isHttpsLink } from "@/lib/diagnostics";

/** Props for {@link DiagnosticsSharingFields}. */
export interface DiagnosticsSharingFieldsProps {
  /** Id prefix, so two copies of the fields on one page keep unique ids. */
  idPrefix: string;
  /** `diagnostics.share_reports`: upload redacted reports. */
  shareReports: boolean;
  /** `diagnostics.share_usage`: weekly anonymous check-in and install ID on reports. */
  shareUsage: boolean;
  /** Called with the new pair when either switch changes. */
  onChange: (shareReports: boolean, shareUsage: boolean) => void;
  /** Whether the switches are read-only (while a save is in flight). */
  disabled?: boolean;
}

/**
 * The two diagnostics sharing switches, with what each one sends. Used in
 * Operator settings and in the first-install setup.
 */
export function DiagnosticsSharingFields({
  idPrefix,
  shareReports,
  shareUsage,
  onChange,
  disabled,
}: DiagnosticsSharingFieldsProps) {
  const rows: Array<{
    id: string;
    label: string;
    description: string;
    checked: boolean;
    set: (checked: boolean) => void;
  }> = [
    {
      id: `${idPrefix}-share-reports`,
      label: "Send diagnostics reports",
      description:
        "After a crash or a burst of errors, send the recent log lines to the Bookclerk maintainers. Secrets, titles and paths are removed first. Each report gets an ID you can look up below.",
      checked: shareReports,
      set: (checked) => onChange(checked, shareUsage),
    },
    {
      id: `${idPrefix}-share-usage`,
      label: "Count this install",
      description:
        "Once a week, send a random install ID, the Bookclerk version and the platform, and tag reports with the ID so the maintainers can see how many installs a problem affects. Nothing about your library or accounts is sent.",
      checked: shareUsage,
      set: (checked) => onChange(shareReports, checked),
    },
  ];
  return (
    <>
      {rows.map((row) => (
        <div key={row.id} className="flex items-start justify-between gap-4 py-3">
          <div className="min-w-0 space-y-0.5">
            <label htmlFor={row.id} className="text-sm font-medium text-ink">
              {row.label}
            </label>
            <p className="text-xs text-ink/50">{row.description}</p>
          </div>
          <input
            id={row.id}
            type="checkbox"
            className="mt-1 h-4 w-4 shrink-0 accent-teal"
            checked={row.checked}
            disabled={disabled}
            onChange={(e) => row.set(e.target.checked)}
          />
        </div>
      ))}
    </>
  );
}

/** Props for {@link DiagnosticsReportPanel}. */
export interface DiagnosticsReportPanelProps {
  /** The report as the collector returned it. */
  lookup: DiagnosticsReportLookup;
}

/**
 * A report read back from the collector: the issues tracking it, its fix,
 * and the stored log lines (redacted, as sent).
 */
export function DiagnosticsReportPanel({ lookup }: DiagnosticsReportPanelProps) {
  const { fix, issues, report } = lookup;
  return (
    <div className="mt-2 space-y-3 rounded-md border border-ink/15 bg-paper p-3 text-xs text-ink/70">
      {issues.length === 0 ? (
        <p>No issue tracks this report yet.</p>
      ) : (
        <ul className="space-y-1">
          {issues.map((issue) => (
            <li key={issue.number}>
              {isHttpsLink(issue.url) ? (
                <a
                  className="text-teal underline-offset-2 hover:underline"
                  href={issue.url}
                  target="_blank"
                  rel="noreferrer noopener"
                >
                  #{issue.number} {issue.title}
                </a>
              ) : (
                <span>
                  #{issue.number} {issue.title}
                </span>
              )}{" "}
              <span className="text-ink/50">({issue.state})</span>
            </li>
          ))}
        </ul>
      )}
      {fix ? (
        <p>
          Fixed
          {fix.fixed_in ? ` in ${fix.fixed_in}` : ", not released yet"}
          {fix.pr_number !== null ? (
            <>
              {" by "}
              {isHttpsLink(fix.pr_url) ? (
                <a
                  className="text-teal underline-offset-2 hover:underline"
                  href={fix.pr_url}
                  target="_blank"
                  rel="noreferrer noopener"
                >
                  pull request #{fix.pr_number}
                </a>
              ) : (
                `pull request #${fix.pr_number}`
              )}
            </>
          ) : null}
          .
        </p>
      ) : null}
      {lookup.deleted_at_unix_ms !== null ? (
        <p>
          You deleted the stored copy on {formatReportTime(lookup.deleted_at_unix_ms)}. Its summary
          and issue links stay.
        </p>
      ) : null}
      {report ? (
        <div className="space-y-1">
          <p className="text-ink/50">
            {report.version} · {report.trigger} · {report.os} {report.arch} {report.distro}
          </p>
          <ol className="max-h-80 space-y-1 overflow-y-auto font-mono">
            {report.events.map((e, i) => (
              <li key={i} className="break-words">
                <span className="text-ink/50">{formatReportTime(e.ts_unix_ms)}</span>{" "}
                <span className={e.level === "ERROR" ? "text-brick" : undefined}>{e.level}</span>{" "}
                <span className="text-ink/50">{e.target}</span> {e.message}
                {e.fields.map((f) => (
                  <span key={f.name} className="text-ink/50">
                    {" "}
                    {f.name}={f.value}
                  </span>
                ))}
              </li>
            ))}
          </ol>
        </div>
      ) : null}
    </div>
  );
}

/** What one report row shows below itself. */
type RowState =
  | { kind: "loading" }
  | { kind: "shown"; lookup: DiagnosticsReportLookup }
  | { kind: "confirm" }
  | { kind: "deleting" }
  | { kind: "error"; message: string };

/**
 * The reports this install sent, newest first, each with its ID, its access
 * code, a link that searches the project's issues for it, and (when the
 * collector gave a code) View and Delete.
 */
export function DiagnosticsReportsList() {
  const [reports, setReports] = useState<DiagnosticsReport[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState<string | null>(null);
  const [rows, setRows] = useState<Record<string, RowState>>({});

  const setRow = useCallback((id: string, state: RowState | null) => {
    setRows((prev) => {
      const next = { ...prev };
      if (state === null) delete next[id];
      else next[id] = state;
      return next;
    });
  }, []);

  const load = useCallback(async () => {
    try {
      const listed = await fetchDiagnosticsReports();
      setError(null);
      setReports(listed);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not load the report history.");
    }
  }, []);

  useEffect(() => {
    let live = true;
    fetchDiagnosticsReports()
      .then((listed) => {
        if (live) setReports(listed);
      })
      .catch((err: unknown) => {
        if (live)
          setError(err instanceof Error ? err.message : "Could not load the report history.");
      });
    return () => {
      live = false;
    };
  }, []);

  async function copy(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(text);
    } catch {
      setCopied(null);
    }
  }

  async function view(id: string) {
    if (rows[id]?.kind === "shown") {
      setRow(id, null);
      return;
    }
    setRow(id, { kind: "loading" });
    try {
      setRow(id, { kind: "shown", lookup: await fetchDiagnosticsReport(id) });
    } catch (err) {
      setRow(id, {
        kind: "error",
        message: err instanceof Error ? err.message : "Could not read the report back.",
      });
    }
  }

  async function remove(id: string) {
    setRow(id, { kind: "deleting" });
    try {
      const at = await deleteDiagnosticsReport(id);
      setRow(id, null);
      setReports(
        (prev) =>
          prev?.map((r) => (r.report_id === id ? { ...r, deleted_at_unix_ms: at } : r)) ?? prev,
      );
    } catch (err) {
      setRow(id, {
        kind: "error",
        message: err instanceof Error ? err.message : "Could not delete the report.",
      });
    }
  }

  if (error) {
    return (
      <div className="flex items-center justify-between gap-3 py-3 text-sm text-ink/60">
        <span>{error}</span>
        <Button type="button" variant="secondary" className="text-sm" onClick={() => void load()}>
          Try again
        </Button>
      </div>
    );
  }
  if (reports === null) {
    return <p className="py-3 text-sm text-ink/50">Loading the report history…</p>;
  }
  if (reports.length === 0) {
    return <p className="py-3 text-sm text-ink/50">This install has not sent a report yet.</p>;
  }
  return (
    <div className="py-3">
      <p className="mb-2 text-xs text-ink/50">
        View reads a report back from the collector with the issues tracking it. Delete removes the
        collector's copy and keeps its summary and issue links. If this app cannot start, the ID and
        access code (also in <code>diagnostics/reports.json</code>) open the collector's lookup
        page.
      </p>
      <ul className="divide-y divide-ink/10">
        {reports.map((r) => {
          const row = rows[r.report_id];
          const code = r.access_code;
          const deleted = r.deleted_at_unix_ms !== null;
          return (
            <li key={r.report_id} className="py-2">
              <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1">
                <div className="min-w-0 space-y-0.5">
                  <code className="block break-all font-mono text-xs text-ink">{r.report_id}</code>
                  <span className="block text-xs text-ink/50">
                    {formatReportTime(r.sent_at_unix_ms)} · {r.trigger} · {r.version}
                    {code ? (
                      <>
                        {" · code "}
                        <code className="font-mono text-ink/70">{code}</code>
                      </>
                    ) : null}
                    {deleted ? " · deleted" : null}
                  </span>
                </div>
                <div className="flex shrink-0 flex-wrap items-center gap-2">
                  <Button
                    type="button"
                    variant="secondary"
                    className="text-xs"
                    onClick={() => void copy(r.report_id)}
                  >
                    {copied === r.report_id ? "Copied" : "Copy ID"}
                  </Button>
                  {code ? (
                    <>
                      <Button
                        type="button"
                        variant="secondary"
                        className="text-xs"
                        onClick={() => void copy(code)}
                      >
                        {copied === code ? "Copied" : "Copy code"}
                      </Button>
                      <Button
                        type="button"
                        variant="secondary"
                        className="text-xs"
                        disabled={row?.kind === "loading" || row?.kind === "deleting"}
                        onClick={() => void view(r.report_id)}
                      >
                        {row?.kind === "shown"
                          ? "Hide"
                          : row?.kind === "loading"
                            ? "Loading…"
                            : "View"}
                      </Button>
                      {deleted ? null : (
                        <Button
                          type="button"
                          variant="secondary"
                          className="text-xs"
                          disabled={row?.kind === "deleting"}
                          onClick={() => setRow(r.report_id, { kind: "confirm" })}
                        >
                          Delete
                        </Button>
                      )}
                    </>
                  ) : null}
                  <a
                    className="text-xs text-teal underline-offset-2 hover:underline"
                    href={diagnosticsIssueSearchUrl(r.report_id)}
                    target="_blank"
                    rel="noreferrer noopener"
                  >
                    Find the issue
                  </a>
                </div>
              </div>
              {row?.kind === "shown" ? <DiagnosticsReportPanel lookup={row.lookup} /> : null}
              {row?.kind === "confirm" || row?.kind === "deleting" ? (
                <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-ink/70">
                  <span>
                    Delete the collector's copy of this report? Its summary and issue links stay, so
                    it still counts.
                  </span>
                  <Button
                    type="button"
                    className="text-xs"
                    disabled={row.kind === "deleting"}
                    onClick={() => void remove(r.report_id)}
                  >
                    {row.kind === "deleting" ? "Deleting…" : "Delete report"}
                  </Button>
                  <Button
                    type="button"
                    variant="secondary"
                    className="text-xs"
                    disabled={row.kind === "deleting"}
                    onClick={() => setRow(r.report_id, null)}
                  >
                    Cancel
                  </Button>
                </div>
              ) : null}
              {row?.kind === "error" ? (
                <p className="mt-2 text-xs font-medium text-brick" role="alert">
                  {row.message}
                </p>
              ) : null}
            </li>
          );
        })}
      </ul>
    </div>
  );
}

/** Props for {@link DiagnosticsSetupStep}. */
export interface DiagnosticsSetupStepProps {
  /** Label of the button that saves and carries on. */
  continueLabel: string;
  /** Called after the choices are saved, or when the save failed and the person carries on anyway. */
  onDone: () => void;
}

/**
 * The first-install setup's diagnostics step: the two sharing switches,
 * starting from what the install has now (off on a fresh install), saved
 * when the person carries on. The switches stay read-only until the
 * current choices have loaded, so a choice is never overwritten by the
 * load or saved over values that were never read.
 */
export function DiagnosticsSetupStep({ continueLabel, onDone }: DiagnosticsSetupStepProps) {
  const [shareReports, setShareReports] = useState(false);
  const [shareUsage, setShareUsage] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [load, setLoad] = useState<"loading" | "ready" | "failed">("loading");
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    let live = true;
    fetchSettings()
      .then((s) => {
        if (!live) return;
        setShareReports(s.settings["diagnostics.share_reports"] === "true");
        setShareUsage(s.settings["diagnostics.share_usage"] === "true");
        setLoad("ready");
      })
      .catch(() => {
        if (live) setLoad("failed");
      });
    return () => {
      live = false;
    };
  }, [attempt]);

  function retry() {
    setLoad("loading");
    setAttempt((n) => n + 1);
  }

  async function save() {
    setBusy(true);
    setError(null);
    try {
      await saveDiagnosticsSharing(shareReports, shareUsage);
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not save the diagnostics choices.");
      setBusy(false);
    }
  }

  return (
    <div className="space-y-4">
      <div className="divide-y divide-ink/10 rounded-md border border-ink/15 bg-paper px-3">
        <DiagnosticsSharingFields
          idPrefix="setup"
          shareReports={shareReports}
          shareUsage={shareUsage}
          disabled={busy || load !== "ready"}
          onChange={(reports, usage) => {
            setShareReports(reports);
            setShareUsage(usage);
          }}
        />
      </div>
      <p className="text-xs text-ink/50">
        You can change these later under Settings → Server Settings.
      </p>
      {load === "failed" ? (
        <p className="text-sm font-medium text-brick" role="alert">
          Could not load the current choices.
        </p>
      ) : null}
      {error ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {error}
        </p>
      ) : null}
      <div className="flex flex-wrap gap-2">
        {load === "failed" ? (
          <Button type="button" onClick={retry}>
            Try again
          </Button>
        ) : (
          <Button type="button" disabled={busy || load !== "ready"} onClick={() => void save()}>
            {continueLabel}
          </Button>
        )}
        {error || load === "failed" ? (
          <Button type="button" variant="secondary" onClick={onDone}>
            Skip for now
          </Button>
        ) : null}
      </div>
    </div>
  );
}
