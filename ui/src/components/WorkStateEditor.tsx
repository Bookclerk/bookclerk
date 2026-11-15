import { useState } from "react";
import { Check, Star } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { putWorkState, type WorkState, type WorkStateEdit } from "@/lib/api";
import { cn } from "@/lib/utils";

/** The rating fields a patron sets, with their labels. */
const RATINGS = [
  ["rating_overall", "Overall"],
  ["rating_story", "Story"],
  ["rating_performance", "Narration"],
] as const;

/** A state with nothing recorded, for a work the patron has not touched. */
const EMPTY: WorkState = {
  is_finished: false,
  rating_overall: null,
  rating_performance: null,
  rating_story: null,
  tags: null,
};

/** Props for {@link StarPicker}. */
type StarPickerProps = {
  /** What the stars rate, for the accessible names. */
  label: string;
  /** Current whole-star rating, or null when unrated. */
  value: number | null;
  /** Whether a save is in flight. */
  disabled: boolean;
  /** Called with the new rating, or null to clear it. */
  onChange: (value: number | null) => void;
};

/**
 * Five star buttons; picking the current rating again clears it.
 *
 * @param props - Label, value, disabled flag and change handler.
 */
function StarPicker({ label, value, disabled, onChange }: StarPickerProps) {
  const current = value == null ? 0 : Math.round(value);
  return (
    <span className="inline-flex items-center gap-0.5" role="group" aria-label={`${label} rating`}>
      {[1, 2, 3, 4, 5].map((n) => (
        <button
          key={n}
          type="button"
          disabled={disabled}
          aria-pressed={current === n}
          aria-label={current === n ? `Clear ${label} rating` : `${label}: ${n} of 5`}
          onClick={() => onChange(current === n ? null : n)}
          className="rounded p-0.5 text-ink/55 hover:text-teal focus:outline-none focus-visible:ring-2 focus-visible:ring-teal/40 disabled:opacity-50"
        >
          <Star
            className={cn(
              "h-4 w-4 stroke-[2.25]",
              n <= current ? "fill-teal text-teal" : "fill-transparent",
            )}
          />
        </button>
      ))}
    </span>
  );
}

/** Props for {@link WorkStateEditor}. */
export type WorkStateEditorProps = {
  /** The library work. */
  workId: string;
  /** The patron's stored state, or null when they recorded none. */
  state: WorkState | null;
  /** Called with the stored state after each save. */
  onSaved: (state: WorkState) => void;
};

/**
 * The signed-in patron's own finished flag, ratings and tags for a library
 * work, saved as each control changes (tags on Save). Give it a `key` of
 * the work id so it starts over for another work.
 *
 * @param props - Work id, stored state and save callback.
 */
export function WorkStateEditor({ workId, state, onSaved }: WorkStateEditorProps) {
  const stored = state ?? EMPTY;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [tags, setTags] = useState(stored.tags ?? "");

  const save = async (edit: WorkStateEdit) => {
    setBusy(true);
    setError(null);
    try {
      const saved = await putWorkState(workId, edit);
      if (edit.tags !== undefined) setTags(saved.tags ?? "");
      onSaved(saved);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not save");
    } finally {
      setBusy(false);
    }
  };

  const tagsChanged = tags.trim().split(/\s+/).join(" ") !== (stored.tags ?? "");

  return (
    <div className="mt-5 space-y-3 border-t border-ink/10 pt-4">
      <p className="text-xs font-semibold uppercase tracking-wide text-ink/45">Your copy</p>
      <div className="flex flex-wrap items-center gap-x-6 gap-y-2">
        <Button
          type="button"
          variant={stored.is_finished ? "secondary" : "ghost"}
          disabled={busy}
          aria-pressed={stored.is_finished}
          onClick={() => void save({ is_finished: !stored.is_finished })}
        >
          <Check className="h-4 w-4" />
          {stored.is_finished ? "Finished" : "Mark finished"}
        </Button>
        {RATINGS.map(([field, label]) => (
          <span key={field} className="inline-flex items-center gap-2 text-sm text-ink/70">
            {label}
            <StarPicker
              label={label}
              value={stored[field]}
              disabled={busy}
              onChange={(value) => void save({ [field]: value })}
            />
          </span>
        ))}
      </div>
      <form
        className="flex items-center gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          const trimmed = tags.trim();
          void save({ tags: trimmed === "" ? null : trimmed });
        }}
      >
        <Input
          value={tags}
          onChange={(e) => setTags(e.target.value)}
          placeholder="Tags, separated by spaces"
          aria-label="Your tags"
          maxLength={1024}
          disabled={busy}
        />
        <Button type="submit" variant="secondary" disabled={busy || !tagsChanged}>
          Save
        </Button>
      </form>
      {error ? <p className="text-sm text-brick">{error}</p> : null}
    </div>
  );
}
