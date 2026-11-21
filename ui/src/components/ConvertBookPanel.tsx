import { useState } from "react";
import { Button } from "@/components/ui/button";
import { isApiError, triggerConvert } from "@/lib/api";
import { copyKey, visibleProfiles } from "@/lib/copyProfiles";

/** Props of {@link ConvertBookPanel}. */
export interface ConvertBookPanelProps {
  /** The stored editions of the open work (see `storedEditions`). */
  editions: string[];
  /** Called once a convert is queued, so the page can refresh its jobs. */
  onQueued?: () => void;
}

/**
 * Adds another copy of a stored book, such as a small MP3 beside the M4B,
 * made from the copy the library holds and kept beside it. Shown to the
 * operator, Owners and Administrators. Advanced formats appear only with
 * the switch on.
 */
export function ConvertBookPanel({ editions, onQueued }: ConvertBookPanelProps) {
  const [advanced, setAdvanced] = useState(false);
  const profiles = visibleProfiles(advanced);
  const [choice, setChoice] = useState(copyKey(profiles[0]!.copy));
  const [force, setForce] = useState(false);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const selected = profiles.find((profile) => copyKey(profile.copy) === choice) ?? profiles[0]!;

  async function convert() {
    setBusy(true);
    setError(null);
    setMessage(null);
    try {
      await triggerConvert({ editions, copy: selected.copy, force });
      setMessage(`Queued: ${selected.label}. It is stored beside the original.`);
      onQueued?.();
    } catch (err) {
      setError(isApiError(err) ? err.message : "The convert could not be queued.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className="space-y-2 border-t border-ink/10 pt-3" aria-label="Convert">
      <p className="text-xs font-semibold uppercase tracking-wide text-ink/45">Convert</p>
      <p className="text-xs text-ink/55">
        Adds another copy of this book, kept beside the original. Each destination receives it
        when its filter takes that copy.
      </p>
      <div className="flex flex-wrap items-center gap-2">
        <select
          className="h-9 min-w-[12rem] flex-1 rounded-md border border-ink/15 bg-card px-2 text-sm"
          aria-label="Copy to add"
          value={copyKey(selected.copy)}
          disabled={busy}
          onChange={(e) => setChoice(e.target.value)}
        >
          {profiles.map((profile) => (
            <option key={copyKey(profile.copy)} value={copyKey(profile.copy)}>
              {profile.label}
            </option>
          ))}
        </select>
        <Button
          type="button"
          disabled={busy || editions.length === 0}
          onClick={() => void convert()}
        >
          Convert
        </Button>
      </div>
      <div className="flex flex-wrap gap-4 text-xs text-ink/65">
        <label className="flex items-center gap-1.5">
          <input
            type="checkbox"
            checked={advanced}
            onChange={(e) => {
              setAdvanced(e.target.checked);
              if (!e.target.checked && selected.advanced) {
                setChoice(copyKey(visibleProfiles(false)[0]!.copy));
              }
            }}
          />
          Advanced formats
        </label>
        <label className="flex items-center gap-1.5">
          <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} />
          Make it again if it exists
        </label>
      </div>
      {selected.advanced ? (
        <p className="text-xs text-ink/55">Fewer players open this format than M4B and MP3.</p>
      ) : null}
      {message ? <p className="text-xs text-teal">{message}</p> : null}
      {error ? (
        <p role="alert" className="text-xs text-brick">
          {error}
        </p>
      ) : null}
    </section>
  );
}
