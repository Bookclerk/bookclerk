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
    try {
      for (const edition of editions) {
        await realignChapters(edition);
      }
      setMessage("Queued. The chapters update once the audio has been checked.");
    } catch (err) {
      setError(isApiError(err) ? err.message : "The chapters could not be re-aligned.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className="space-y-2 border-t border-ink/10 pt-3" aria-label="Chapters">
      <p className="text-xs font-semibold uppercase tracking-wide text-ink/45">Chapters</p>
      <p className="text-xs text-ink/55">
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
