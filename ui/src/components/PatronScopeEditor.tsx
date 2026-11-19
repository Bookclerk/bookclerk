import { useCallback, useEffect, useState } from "react";
import { Plus, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  getDefaultScope,
  getUserScope,
  isApiError,
  putDefaultScope,
  putUserScope,
  type PatronScope,
  type RuleEffect,
  type ScopeField,
  type ScopeRule,
} from "@/lib/api";
import {
  MAX_SCOPE_RULES,
  RULE_EFFECTS,
  SCOPE_FIELDS,
  newRule,
  scopeProblem,
  scopeSummary,
} from "@/lib/patronScope";

const selectClassName =
  "h-9 rounded-md border border-ink/15 bg-card-strong px-2 text-sm text-ink outline-none focus:border-teal";

/** Whose scope the editor changes. */
export type PatronScopeTarget = { kind: "user"; userId: number } | { kind: "default" };

/** Props of {@link PatronScopeEditor}. */
export interface PatronScopeEditorProps {
  /** One patron, or the library's default. */
  target: PatronScopeTarget;
  /** True while the person may not change it. */
  disabled?: boolean;
}

/**
 * Edits which part of the library a patron sees: rules on genre, series,
 * author and narrator, each showing only matching books or hiding them.
 * For one patron it also switches between their own scope and the
 * library's default.
 */
export function PatronScopeEditor({ target, disabled = false }: PatronScopeEditorProps) {
  const [loaded, setLoaded] = useState(false);
  const [draft, setDraft] = useState<PatronScope>({ rules: [] });
  const [own, setOwn] = useState(false);
  const [applies, setApplies] = useState(true);
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const userId = target.kind === "user" ? target.userId : null;

  const load = useCallback(async () => {
    setLoaded(false);
    setError(null);
    try {
      if (userId === null) {
        const res = await getDefaultScope();
        setDraft(res.scope);
        setOwn(true);
      } else {
        const res = await getUserScope(userId);
        setDraft(res.scope);
        setOwn(res.own);
        setApplies(res.applies);
      }
      setDirty(false);
      setLoaded(true);
    } catch (err) {
      setError(isApiError(err) ? err.message : "The scope could not be read.");
    }
  }, [userId]);

  useEffect(() => {
    void load();
  }, [load]);

  function edit(rules: ScopeRule[]) {
    setDraft({ rules });
    setDirty(true);
    setSaved(false);
  }

  function setRule(index: number, patch: Partial<ScopeRule>) {
    edit(draft.rules.map((rule, i) => (i === index ? { ...rule, ...patch } : rule)));
  }

  async function save(next: PatronScope | null) {
    setSaving(true);
    setError(null);
    try {
      if (userId === null) {
        const res = await putDefaultScope(next ?? { rules: [] });
        setDraft(res.scope);
      } else {
        const res = await putUserScope(userId, next);
        setDraft(res.scope);
        setOwn(res.own);
      }
      setDirty(false);
      setSaved(true);
    } catch (err) {
      setError(isApiError(err) ? err.message : "The scope could not be saved.");
    } finally {
      setSaving(false);
    }
  }

  const problem = scopeProblem(draft);
  const locked = disabled || saving || !loaded;
  const editable = own && !locked;
  const heading = userId === null ? "Default patron scope" : "What this patron sees";

  return (
    <section className="space-y-2" aria-label={heading}>
      <p className="text-xs font-semibold uppercase tracking-wide text-ink/45">{heading}</p>
      {userId !== null && !applies ? (
        <p className="text-xs text-ink/55">
          Owners and Administrators always see the whole library. A scope set here applies if their
          role changes to Member or Host.
        </p>
      ) : null}
      {userId === null ? (
        <p className="text-xs text-ink/55">
          Members and Hosts without their own scope see this. Books they hold through their own
          store accounts are always visible.
        </p>
      ) : (
        <div className="flex flex-col gap-1 text-sm text-ink">
          <label className="flex items-center gap-2">
            <input
              type="radio"
              name={`scope-source-${userId}`}
              checked={!own}
              disabled={locked}
              onChange={() => void save(null)}
            />
            Follow the library&apos;s default
          </label>
          <label className="flex items-center gap-2">
            <input
              type="radio"
              name={`scope-source-${userId}`}
              checked={own}
              disabled={locked}
              onChange={() => {
                setOwn(true);
                setDirty(true);
                setSaved(false);
              }}
            />
            Own scope
          </label>
        </div>
      )}

      {!loaded && !error ? <p className="text-sm text-ink/50">Loading…</p> : null}
      {loaded && !own ? <p className="text-sm text-ink/70">{scopeSummary(draft)}</p> : null}
      {loaded && own ? (
        <div className="space-y-2">
          {draft.rules.length === 0 ? (
            <p className="text-sm text-ink/50">No rules: every book is visible.</p>
          ) : (
            <ul className="space-y-1.5">
              {draft.rules.map((rule, index) => (
                <li key={index} className="flex flex-wrap items-center gap-1.5">
                  <select
                    className={selectClassName}
                    aria-label={`Rule ${index + 1} effect`}
                    value={rule.effect}
                    disabled={!editable}
                    onChange={(e) => setRule(index, { effect: e.target.value as RuleEffect })}
                  >
                    {RULE_EFFECTS.map((effect) => (
                      <option key={effect.value} value={effect.value}>
                        {effect.label}
                      </option>
                    ))}
                  </select>
                  <select
                    className={selectClassName}
                    aria-label={`Rule ${index + 1} field`}
                    value={rule.field}
                    disabled={!editable}
                    onChange={(e) => setRule(index, { field: e.target.value as ScopeField })}
                  >
                    {SCOPE_FIELDS.map((field) => (
                      <option key={field.value} value={field.value}>
                        {field.label}
                      </option>
                    ))}
                  </select>
                  <Input
                    className="h-9 min-w-[10rem] flex-1"
                    aria-label={`Rule ${index + 1} value`}
                    placeholder="Value, such as Kids"
                    value={rule.value}
                    disabled={!editable}
                    onChange={(e) => setRule(index, { value: e.target.value })}
                  />
                  <Button
                    type="button"
                    variant="ghost"
                    aria-label={`Remove rule ${index + 1}`}
                    disabled={!editable}
                    onClick={() => edit(draft.rules.filter((_, i) => i !== index))}
                  >
                    <X className="h-4 w-4" />
                  </Button>
                </li>
              ))}
            </ul>
          )}
          <p className="text-xs text-ink/55">
            With any &ldquo;Show only&rdquo; rule, a book needs to match one of them. A
            &ldquo;Hide&rdquo; rule always wins. Values match without case.
          </p>
          <div className="flex flex-wrap items-center gap-2">
            <Button
              type="button"
              variant="secondary"
              disabled={!editable || draft.rules.length >= MAX_SCOPE_RULES}
              onClick={() => edit([...draft.rules, newRule()])}
            >
              <Plus className="mr-1 inline h-4 w-4" />
              Add rule
            </Button>
            <Button
              type="button"
              disabled={!editable || !dirty || problem !== null}
              onClick={() => void save(draft)}
            >
              Save scope
            </Button>
            {saved && !dirty ? <span className="text-xs text-ink/55">Saved</span> : null}
          </div>
          {dirty && problem ? <p className="text-xs text-brick">{problem}</p> : null}
        </div>
      ) : null}
      {error ? (
        <p role="alert" className="text-xs text-brick">
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
