import { useCallback, useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import {
  fetchDiagnosticsReports,
  fetchSettings,
  saveDiagnosticsSharing,
  type DiagnosticsReport,
} from "@/lib/api";
import { diagnosticsIssueSearchUrl, formatReportTime } from "@/lib/diagnostics";

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

/**
 * The reports this install sent, newest first, each with its ID, a copy
 * button and a link that searches the project's issues for it.
 */
export function DiagnosticsReportsList() {
  const [reports, setReports] = useState<DiagnosticsReport[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState<string | null>(null);

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

  async function copy(id: string) {
    try {
      await navigator.clipboard.writeText(id);
      setCopied(id);
    } catch {
      setCopied(null);
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
        To follow up on a report, search the project's issues for its ID, or give the ID to a
        maintainer.
      </p>
      <ul className="divide-y divide-ink/10">
        {reports.map((r) => (
          <li
            key={r.report_id}
            className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 py-2"
          >
            <div className="min-w-0 space-y-0.5">
              <code className="block break-all font-mono text-xs text-ink">{r.report_id}</code>
              <span className="block text-xs text-ink/50">
                {formatReportTime(r.sent_at_unix_ms)} · {r.trigger} · {r.version}
              </span>
            </div>
            <div className="flex shrink-0 items-center gap-2">
              <Button
                type="button"
                variant="secondary"
                className="text-xs"
                onClick={() => void copy(r.report_id)}
              >
                {copied === r.report_id ? "Copied" : "Copy ID"}
              </Button>
              <a
                className="text-xs text-teal underline-offset-2 hover:underline"
                href={diagnosticsIssueSearchUrl(r.report_id)}
                target="_blank"
                rel="noreferrer noopener"
              >
                Find the issue
              </a>
            </div>
          </li>
        ))}
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
 * when the person carries on.
 */
export function DiagnosticsSetupStep({ continueLabel, onDone }: DiagnosticsSetupStepProps) {
  const [shareReports, setShareReports] = useState(false);
  const [shareUsage, setShareUsage] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    fetchSettings()
      .then((s) => {
        if (!live) return;
        setShareReports(s.settings["diagnostics.share_reports"] === "true");
        setShareUsage(s.settings["diagnostics.share_usage"] === "true");
      })
      .catch(() => {
        /* the switches start off, as on a fresh install */
      });
    return () => {
      live = false;
    };
  }, []);

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
          disabled={busy}
          onChange={(reports, usage) => {
            setShareReports(reports);
            setShareUsage(usage);
          }}
        />
      </div>
      <p className="text-xs text-ink/50">
        You can change these later under Settings → Server Settings.
      </p>
      {error ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {error}
        </p>
      ) : null}
      <div className="flex flex-wrap gap-2">
        <Button type="button" disabled={busy} onClick={() => void save()}>
          {continueLabel}
        </Button>
        {error ? (
          <Button type="button" variant="secondary" onClick={onDone}>
            Skip for now
          </Button>
        ) : null}
      </div>
    </div>
  );
}
