import { useState } from "react";
import { Button } from "@/components/ui/button";
import { isApiError, realignChapters } from "@/lib/api";

/** Props of {@link RealignChaptersPanel}. */
export interface RealignChaptersPanelProps {
  /** The stored editions of the open work (see `storedEditions`). */
  editions: string[];
}

/**
 * Asks the metadata providers for the book's chapters again and lines them
 * up with the stored audio, snapping each chapter to where its title is
 * spoken. Shown to the operator, Owners and Administrators.
 */
export function RealignChaptersPanel({ editions }: RealignChaptersPanelProps) {
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function realign() {
    setBusy(true);
    setError(null);
    setMessage(null);
    // Each edition is asked for on its own, so one that cannot be aligned
    // (no ASIN, say) does not stop the others.
    let queued = 0;
    const failures: string[] = [];
    for (const edition of editions) {
      try {
        await realignChapters(edition);
        queued += 1;
      } catch (err) {
        failures.push(isApiError(err) ? err.message : "The chapters could not be re-aligned.");
      }
    }
    if (queued > 0) {
      setMessage(
        failures.length === 0
          ? "Queued. The chapters update once the audio has been checked."
          : `Queued for ${queued} of ${editions.length} editions. The chapters update once the audio has been checked.`,
      );
    }
    if (failures.length > 0) {
      const reasons = [...new Set(failures)].join(" ");
      setError(
        queued > 0
          ? `${failures.length} of ${editions.length} editions could not be re-aligned: ${reasons}`
          : reasons,
      );
    }
    setBusy(false);
  }

  return (
    <section className="space-y-2 border-t border-ink/10 pt-3" aria-label="Chapters">
      <p className="text-xs font-semibold uppercase tracking-wide text-ink/70">Chapters</p>
      <p className="text-xs text-ink/70">
        Gets the chapter list again from your metadata sources and lines it up with the stored
        audio. A chapter list you set by hand stays in place.
      </p>
      <Button
        type="button"
        variant="secondary"
        disabled={busy || editions.length === 0}
        onClick={() => void realign()}
      >
        Re-align chapters
      </Button>
      {message ? <p className="text-xs text-teal">{message}</p> : null}
      {error ? (
        <p role="alert" className="text-xs text-brick">
          {error}
        </p>
      ) : null}
    </section>
  );
}
