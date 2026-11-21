import { useCallback, useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { getWorkTags, isApiError, putWorkTags, type WorkTag } from "@/lib/api";
import { adminTagText, parseTagInput, tagInputProblem } from "@/lib/workTags";

/** Props of {@link LibraryTagsEditor}. */
export interface LibraryTagsEditorProps {
  /** The top-level work whose tags to edit. */
  workId: string;
}

/**
 * Edits a work's library tags, which everyone sees, the Library filters by,
 * and a patron scope can show or hide. Shown to the operator, Owners and
 * Administrators; tags read from a sidecar file are listed but not edited.
 */
export function LibraryTagsEditor({ workId }: LibraryTagsEditorProps) {
  const [tags, setTags] = useState<WorkTag[] | null>(null);
  const [text, setText] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      const res = await getWorkTags(workId);
      setTags(res.tags);
      setText(adminTagText(res.tags));
    } catch (err) {
      setError(isApiError(err) ? err.message : "The library tags could not be read.");
    }
  }, [workId]);

  useEffect(() => {
    void load();
  }, [load]);

  const parsed = parseTagInput(text);
  const problem = tagInputProblem(parsed);
  const dirty =
    tags !== null && parsed.join("\u0000") !== parseTagInput(adminTagText(tags)).join("\u0000");
  const sidecar = (tags ?? []).filter((t) => t.source === "sidecar");

  async function save() {
    setSaving(true);
    setError(null);
    try {
      const res = await putWorkTags(workId, parsed);
      setTags(res.tags);
      setText(adminTagText(res.tags));
    } catch (err) {
      setError(isApiError(err) ? err.message : "The library tags could not be saved.");
    } finally {
      setSaving(false);
    }
  }

  return (
    <section className="space-y-2 border-t border-ink/10 pt-3" aria-label="Library tags">
      <p className="text-xs font-semibold uppercase tracking-wide text-ink/45">Library tags</p>
      <p className="text-xs text-ink/55">
        Everyone sees these, and patron scopes can show or hide books by them. Separate tags with
        commas.
      </p>
      <div className="flex flex-wrap items-center gap-2">
        <Input
          className="h-9 min-w-[12rem] flex-1"
          aria-label="Library tags"
          placeholder="Kids, Bedtime"
          value={text}
          disabled={tags === null || saving}
          onChange={(e) => setText(e.target.value)}
        />
        <Button
          type="button"
          disabled={!dirty || problem !== null || saving}
          onClick={() => void save()}
        >
          Save tags
        </Button>
      </div>
      {sidecar.length > 0 ? (
        <p className="text-xs text-ink/55">
          From a sidecar file: {sidecar.map((t) => t.value).join(", ")}
        </p>
      ) : null}
      {problem || error ? (
        <p role="alert" className="text-xs text-brick">
          {problem ?? error}
        </p>
      ) : null}
    </section>
  );
}
