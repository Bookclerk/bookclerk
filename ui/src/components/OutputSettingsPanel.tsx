import { useCallback, useEffect, useMemo, useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { fetchSettings, isApiError, patchSettings, type SettingsResponse } from "@/lib/api";

/** One editable `output.*` key of the `core.output` document. */
type OutputField =
  | { key: string; label: string; kind: "toggle"; hint?: string }
  | { key: string; label: string; kind: "select"; options: [string, string][]; hint?: string }
  | { key: string; label: string; kind: "text"; hint?: string };

/** Fields the panel edits; the full document is editable with `bookclerk config set output.*`. */
const FIELDS: OutputField[] = [
  {
    key: "output.format",
    label: "Format",
    kind: "select",
    options: [
      ["enriched_m4b", "M4B with tags and chapters"],
      ["single_mp3", "Single MP3"],
      ["split_mp3_by_chapter", "MP3 per chapter"],
      ["split_mp3_by_size", "MP3 split by size"],
      ["none", "Keep source file"],
    ],
  },
  {
    key: "output.naming_profile",
    label: "Naming profile",
    kind: "select",
    options: [
      ["audiobookshelf", "Audiobookshelf"],
      ["classic", "Classic Libation"],
    ],
  },
  {
    key: "output.folder_template",
    label: "Folder template",
    kind: "text",
    hint: "Empty uses the naming profile's layout.",
  },
  {
    key: "output.file_template",
    label: "File template",
    kind: "text",
    hint: "Empty uses the naming profile's file name.",
  },
  {
    key: "output.multi_destination",
    label: "When only some destinations have a title",
    kind: "select",
    options: [
      ["sync_missing", "Copy from a destination that has it"],
      ["refetch_missing", "Re-acquire into the missing ones"],
      ["refetch_all", "Re-acquire into every destination"],
    ],
  },
  {
    key: "output.chapter_json",
    label: "Chapter JSON",
    kind: "select",
    options: [
      ["off", "Off"],
      ["tree", "Nested"],
      ["flat", "Flat"],
      ["both", "Both"],
    ],
  },
  {
    key: "output.cover_size",
    label: "Cover size",
    kind: "select",
    options: [
      ["500", "500 px"],
      ["1215", "1215 px"],
      ["native", "Native"],
    ],
  },
  { key: "output.widevine", label: "Prefer Widevine", kind: "toggle" },
  { key: "output.xhe_aac", label: "Prefer xHE-AAC on the Widevine path", kind: "toggle" },
  { key: "output.download_cover", label: "Save cover image", kind: "toggle" },
  { key: "output.download_pdf", label: "Download companion PDF", kind: "toggle" },
  { key: "output.fixup_metadata", label: "Embed tags, cover, and chapters", kind: "toggle" },
  { key: "output.create_cue", label: "Write a .cue sheet", kind: "toggle" },
  { key: "output.save_metadata_json", label: "Save catalog metadata JSON", kind: "toggle" },
  {
    key: "output.overwrite_existing",
    label: "Re-acquire titles a destination already has",
    kind: "toggle",
  },
];

const selectClassName =
  "w-full rounded-md border border-ink/15 bg-card-strong px-3 py-2 text-sm text-ink shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30";

/** `output.*` values from a Settings payload. */
function outputValues(settings: SettingsResponse): Record<string, string> {
  const values: Record<string, string> = {};
  for (const field of FIELDS) values[field.key] = settings.settings[field.key] ?? "";
  return values;
}

/**
 * Shared output packaging (`core.output`): one cluster document in the
 * library database, saved on its own because it is a separate authority from
 * the file-backed server settings.
 *
 * @param props - Called when the session has expired.
 */
export function OutputSettingsPanel({ onSessionExpired }: { onSessionExpired: () => void }) {
  const [baseline, setBaseline] = useState<Record<string, string> | null>(null);
  const [values, setValues] = useState<Record<string, string>>({});
  const [revision, setRevision] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const apply = useCallback((next: SettingsResponse) => {
    const loaded = outputValues(next);
    setBaseline(loaded);
    setValues(loaded);
    setRevision(next.effective?.["output.revision"] ?? null);
  }, []);

  const load = useCallback(async () => {
    setError(null);
    try {
      apply(await fetchSettings());
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        onSessionExpired();
        return;
      }
      setError(err instanceof Error ? err.message : "Failed to load output settings");
    }
  }, [apply, onSessionExpired]);

  useEffect(() => {
    void load();
  }, [load]);

  const changed = useMemo(
    () => (baseline ? FIELDS.filter((field) => values[field.key] !== baseline[field.key]) : []),
    [baseline, values],
  );

  async function onSave(event: FormEvent) {
    event.preventDefault();
    if (changed.length === 0) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const next = await patchSettings({
        settings: changed.map((field) => ({ key: field.key, value: values[field.key] ?? "" })),
      });
      apply(next);
      setNotice("Saved. Every host applies the new revision on its next wakeup.");
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        onSessionExpired();
        return;
      }
      if (isApiError(err) && err.status === 409) {
        setError("Output settings changed elsewhere. Reloaded the current revision.");
        await load();
        return;
      }
      setError(err instanceof Error ? err.message : "Failed to save output settings");
    } finally {
      setBusy(false);
    }
  }

  if (!baseline) {
    return error ? (
      <p className="text-sm font-medium text-brick" role="alert">
        {error}
      </p>
    ) : (
      <p className="text-sm text-ink/50">Loading output settings…</p>
    );
  }

  const set = (key: string, value: string) => setValues((prev) => ({ ...prev, [key]: value }));
  const selects = FIELDS.filter((field) => field.kind !== "toggle");
  const toggles = FIELDS.filter((field) => field.kind === "toggle");

  return (
    <form className="flex flex-col gap-6" onSubmit={(e) => void onSave(e)}>
      <section className="space-y-3">
        <div className="space-y-1">
          <h2 className="text-lg font-semibold text-ink">Output</h2>
          <p className="text-sm text-ink/55">
            How acquired audio is packaged and named on every host. Stored in the library
            database
            {revision ? ` (revision ${revision})` : ""}; destination roots and prefixes are
            plugin settings.
          </p>
        </div>
        {error ? (
          <p className="text-sm font-medium text-brick" role="alert">
            {error}
          </p>
        ) : null}
        {notice ? <p className="text-sm text-teal">{notice}</p> : null}
        <div className="grid gap-4 bg-card px-3 py-3 sm:grid-cols-2">
          {selects.map((field) => (
            <div key={field.key} className="space-y-1.5">
              <label htmlFor={field.key} className="block text-sm font-medium text-ink">
                {field.label}
              </label>
              {field.kind === "select" ? (
                <select
                  id={field.key}
                  value={values[field.key] ?? ""}
                  onChange={(e) => set(field.key, e.target.value)}
                  className={selectClassName}
                >
                  {field.options.some(([value]) => value === values[field.key]) ? null : (
                    <option value={values[field.key] ?? ""}>{values[field.key] || "—"}</option>
                  )}
                  {field.options.map(([value, label]) => (
                    <option key={value} value={value}>
                      {label}
                    </option>
                  ))}
                </select>
              ) : (
                <Input
                  id={field.key}
                  value={values[field.key] ?? ""}
                  onChange={(e) => set(field.key, e.target.value)}
                  spellCheck={false}
                />
              )}
              {field.hint ? <p className="text-xs text-ink/50">{field.hint}</p> : null}
            </div>
          ))}
        </div>
        <div className="divide-y divide-ink/10 bg-card px-3">
          {toggles.map((field) => (
            <div key={field.key} className="flex items-start justify-between gap-4 py-3">
              <label htmlFor={field.key} className="text-sm font-medium text-ink">
                {field.label}
              </label>
              <input
                id={field.key}
                type="checkbox"
                className="mt-1 h-4 w-4 shrink-0 accent-teal"
                checked={values[field.key] === "true"}
                onChange={(e) => set(field.key, String(e.target.checked))}
              />
            </div>
          ))}
        </div>
      </section>
      <div className="flex justify-end gap-2">
        <Button
          type="button"
          variant="ghost"
          disabled={busy || changed.length === 0}
          onClick={() => setValues(baseline)}
        >
          Discard
        </Button>
        <Button type="submit" disabled={busy || changed.length === 0}>
          {busy ? "Saving…" : "Save output settings"}
        </Button>
      </div>
    </form>
  );
}
