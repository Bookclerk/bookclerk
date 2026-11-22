import { useCallback, useEffect, useState } from "react";
import { Plus, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { getCollections, isApiError, putCollections } from "@/lib/api";
import {
  MAX_COLLECTIONS,
  collectionsOf,
  collectionsProblem,
  draftsOf,
  type CollectionDraft,
} from "@/lib/collections";

/** Props of {@link CollectionsEditor}. */
export interface CollectionsEditorProps {
  /** True while the person may not change them. */
  disabled?: boolean;
}

/**
 * Edits the library's collections: named groups of storage folders. A book
 * whose files sit under one of a collection's folders is in it, so the
 * Library filters by collection and a patron scope can show or hide one.
 */
export function CollectionsEditor({ disabled = false }: CollectionsEditorProps) {
  const [loaded, setLoaded] = useState(false);
  const [drafts, setDrafts] = useState<CollectionDraft[]>([]);
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  const load = useCallback(async () => {
    setLoaded(false);
    setError(null);
    try {
      const res = await getCollections();
      setDrafts(draftsOf(res.collections));
      setDirty(false);
      setLoaded(true);
    } catch (err) {
      setError(isApiError(err) ? err.message : "The collections could not be read.");
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  function edit(next: CollectionDraft[]) {
    setDrafts(next);
    setDirty(true);
    setSaved(false);
  }

  function setRow(index: number, patch: Partial<CollectionDraft>) {
    edit(drafts.map((row, i) => (i === index ? { ...row, ...patch } : row)));
  }

  async function save() {
    setSaving(true);
    setError(null);
    try {
      const res = await putCollections({ collections: collectionsOf(drafts) });
      setDrafts(draftsOf(res.collections));
      setDirty(false);
      setSaved(true);
    } catch (err) {
      setError(isApiError(err) ? err.message : "The collections could not be saved.");
    } finally {
      setSaving(false);
    }
  }

  const problem = collectionsProblem(collectionsOf(drafts));
  const editable = loaded && !disabled && !saving;

  return (
    <section className="space-y-2" aria-label="Collections">
      <p className="text-xs font-semibold uppercase tracking-wide text-ink/70">Collections</p>
      <p className="text-xs text-ink/70">
        A collection is a set of storage folders, such as kids/. A book with a file under one of
        them is in the collection. Changing collections re-indexes the Library.
      </p>
      {!loaded && !error ? <p className="text-sm text-ink/70">Loading…</p> : null}
      {loaded ? (
        <div className="space-y-2">
          {drafts.length === 0 ? (
            <p className="text-sm text-ink/70">No collections.</p>
          ) : (
            <ul className="space-y-1.5">
              {drafts.map((row, index) => (
                <li key={index} className="flex flex-wrap items-center gap-1.5">
                  <Input
                    className="h-9 w-40"
                    aria-label={`Collection ${index + 1} name`}
                    placeholder="Name, such as Kids"
                    value={row.name}
                    disabled={!editable}
                    onChange={(e) => setRow(index, { name: e.target.value })}
                  />
                  <Input
                    className="h-9 min-w-[10rem] flex-1"
                    aria-label={`Collection ${index + 1} folders`}
                    placeholder="Folders, comma-separated"
                    value={row.prefixes}
                    disabled={!editable}
                    onChange={(e) => setRow(index, { prefixes: e.target.value })}
                  />
                  <Button
                    type="button"
                    variant="ghost"
                    aria-label={`Remove collection ${index + 1}`}
                    disabled={!editable}
                    onClick={() => edit(drafts.filter((_, i) => i !== index))}
                  >
                    <X className="h-4 w-4" />
                  </Button>
                </li>
              ))}
            </ul>
          )}
          <div className="flex flex-wrap items-center gap-2">
            <Button
              type="button"
              variant="secondary"
              disabled={!editable || drafts.length >= MAX_COLLECTIONS}
              onClick={() => edit([...drafts, { name: "", prefixes: "" }])}
            >
              <Plus className="mr-1 inline h-4 w-4" />
              Add collection
            </Button>
            <Button
              type="button"
              disabled={!editable || !dirty || problem !== null}
              onClick={() => void save()}
            >
              Save collections
            </Button>
            {saved && !dirty ? <span className="text-xs text-ink/70">Saved</span> : null}
          </div>
          {dirty && problem ? <p className="text-xs text-brick-text">{problem}</p> : null}
        </div>
      ) : null}
      {error ? (
        <p role="alert" className="text-xs text-brick-text">
          {error}
        </p>
      ) : null}
      {error && !loaded ? (
        <Button type="button" variant="secondary" onClick={() => void load()}>
          Try again
        </Button>
      ) : null}
    </section>
  );
}
