import { useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  checkNotifierDeliverability,
  isApiError,
  type DeliverabilityOutcome,
  type DeliverabilityReport,
  type PluginSettingsGroup,
} from "@/lib/api";
import { cn } from "@/lib/utils";

/** The chip each outcome shows, in words and colour. */
const outcomeChips: Record<DeliverabilityOutcome, { label: string; className: string }> = {
  passed: { label: "Passed", className: "bg-teal/15 text-ink" },
  warning: { label: "Improve", className: "bg-fold/60 text-ink" },
  failed: { label: "Fix", className: "bg-brick/10 text-brick-text" },
  skipped: { label: "Not run", className: "bg-ink/5 text-ink/70" },
};

/**
 * One outcome's chip.
 *
 * @param props - The outcome.
 */
function OutcomeChip({ outcome }: { outcome: DeliverabilityOutcome }) {
  const chip = outcomeChips[outcome];
  return <Badge className={cn("normal-case tracking-normal", chip.className)}>{chip.label}</Badge>;
}

interface DeliverabilityReportViewProps {
  /** What the check found. */
  report: DeliverabilityReport;
}

/**
 * A deliverability report: what to do first, most urgent at the top, then
 * one line per checker with a link to its full report when it has one.
 *
 * @param props - The report.
 */
export function DeliverabilityReportView({ report }: DeliverabilityReportViewProps) {
  return (
    <div className="mt-2 grid gap-3">
      {report.advice.length ? (
        <section aria-label="Recommendations">
          <p className="text-xs font-medium text-ink/70">Recommendations</p>
          <ul className="mt-1 grid gap-2">
            {report.advice.map((item) => (
              <li key={item.title} className="rounded-md border border-ink/10 bg-card-strong p-2">
                <div className="flex flex-wrap items-center gap-2">
                  <OutcomeChip outcome={item.outcome} />
                  <span className="text-sm font-medium text-ink">{item.title}</span>
                </div>
                <p className="mt-1 text-xs text-ink/70">{item.detail}</p>
                <p className="mt-1 text-[11px] text-ink/60">From {item.sources.join(", ")}</p>
              </li>
            ))}
          </ul>
        </section>
      ) : null}
      <section aria-label="Checks">
        <p className="text-xs font-medium text-ink/70">Checks</p>
        <ul className="mt-1 grid gap-1">
          {report.checks.map((check) => (
            <li key={check.service} className="flex flex-wrap items-baseline gap-2 text-xs">
              <OutcomeChip outcome={check.outcome} />
              <span className="font-medium text-ink">{check.service}</span>
              <span className="text-ink/70">{check.summary}</span>
              {check.report_url ? (
                <a
                  className="text-teal-text underline"
                  href={check.report_url}
                  target="_blank"
                  rel="noopener noreferrer"
                >
                  Full report
                </a>
              ) : null}
            </li>
          ))}
        </ul>
      </section>
    </div>
  );
}

interface DeliverabilityPanelProps {
  /** The notifier instance to check. */
  plugin: PluginSettingsGroup;
  /** Called on a 401 so the page can return to sign-in. */
  onSessionExpired: () => void;
}

/**
 * The deliverability check for a notifier whose plugin offers one: a button
 * that sends a sample through each checker the plugin uses and shows the
 * merged recommendations. Uses the saved settings.
 *
 * Renders nothing when the plugin offers no check.
 */
export function DeliverabilityPanel({ plugin, onSessionExpired }: DeliverabilityPanelProps) {
  const instanceId = plugin.plugin_instance_id;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [report, setReport] = useState<DeliverabilityReport | null>(null);

  if (!instanceId || !plugin.deliverability_check) return null;

  async function run() {
    setBusy(true);
    setError(null);
    try {
      setReport(await checkNotifierDeliverability(instanceId as string));
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        onSessionExpired();
        return;
      }
      setError(err instanceof Error ? err.message : "The check could not run");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="mt-3 border-t border-ink/10 pt-3">
      <p className="text-xs font-medium text-ink/70">Deliverability</p>
      <p className="mt-1 text-xs text-ink/70">
        Checks whether mail from this notifier is likely to reach inboxes: the message itself, the
        sending domain&apos;s DNS records and, when set up, a test delivery. Uses the saved settings
        and can take up to two minutes. Each checker&apos;s host needs approval in the plugin&apos;s
        network settings.
      </p>
      <div className="mt-2">
        <Button type="button" variant="secondary" disabled={busy} onClick={() => void run()}>
          {busy ? "Checking…" : "Check deliverability"}
        </Button>
      </div>
      {error ? (
        <p role="alert" className="mt-2 text-xs text-brick-text">
          {error}
        </p>
      ) : null}
      {report ? <DeliverabilityReportView report={report} /> : null}
    </div>
  );
}
