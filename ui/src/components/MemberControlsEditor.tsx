import { useCallback, useEffect, useState } from "react";
import {
  getDefaultControls,
  getUserControls,
  isApiError,
  putDefaultControls,
  putUserControls,
  type ControlOverrides,
  type MemberControl,
  type MemberControls,
} from "@/lib/api";
import {
  ALL_ALLOWED,
  MEMBER_CONTROLS,
  overrideChoice,
  withOverride,
  type OverrideChoice,
} from "@/lib/memberControls";

const selectClassName =
  "h-8 rounded-md border border-ink/15 bg-card-strong px-2 text-sm text-ink outline-none focus:border-teal";

/** Whose controls the editor changes. */
export type MemberControlsTarget = { kind: "user"; userId: number } | { kind: "default" };

/** Props of {@link MemberControlsEditor}. */
export interface MemberControlsEditorProps {
  /** One patron, or the library's default. */
  target: MemberControlsTarget;
  /** True while the person may not change them. */
  disabled?: boolean;
}

/**
 * Edits what patrons may do. For the library's default each control is a
 * switch; for one patron each control follows the default or is set on
 * or off for them alone. Every change saves at once.
 */
export function MemberControlsEditor({ target, disabled = false }: MemberControlsEditorProps) {
  const [loaded, setLoaded] = useState(false);
  const [defaults, setDefaults] = useState<MemberControls>(ALL_ALLOWED);
  const [overrides, setOverrides] = useState<ControlOverrides>({});
  const [applies, setApplies] = useState(true);
  const [followsDefault, setFollowsDefault] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const userId = target.kind === "user" ? target.userId : null;

  const load = useCallback(async () => {
    setLoaded(false);
    setError(null);
    try {
      if (userId === null) {
        setDefaults((await getDefaultControls()).controls);
      } else {
        const res = await getUserControls(userId);
        setDefaults(res.defaults);
        setOverrides(res.overrides);
        setApplies(res.applies);
        setFollowsDefault(res.follows_default);
      }
      setLoaded(true);
    } catch (err) {
      setError(isApiError(err) ? err.message : "The controls could not be read.");
    }
  }, [userId]);

  useEffect(() => {
    void load();
  }, [load]);

  async function saveDefault(control: MemberControl, value: boolean) {
    setSaving(true);
    setError(null);
    try {
      setDefaults((await putDefaultControls({ ...defaults, [control]: value })).controls);
    } catch (err) {
      setError(isApiError(err) ? err.message : "The controls could not be saved.");
    } finally {
      setSaving(false);
    }
  }

  async function saveOverride(id: number, control: MemberControl, choice: OverrideChoice) {
    setSaving(true);
    setError(null);
    try {
      const res = await putUserControls(id, withOverride(overrides, control, choice));
      setDefaults(res.defaults);
      setOverrides(res.overrides);
    } catch (err) {
      setError(isApiError(err) ? err.message : "The controls could not be saved.");
    } finally {
      setSaving(false);
    }
  }

  const locked = disabled || saving || !loaded;
  const heading =
    userId === null
      ? "Default member controls"
      : followsDefault
        ? "What this patron may do"
        : "What this Administrator may do";

  return (
    <section className="space-y-2" aria-label={heading}>
      <p className="text-xs font-semibold uppercase tracking-wide text-ink/45">{heading}</p>
      {userId === null ? (
        <p className="text-xs text-ink/55">
          Members follow these unless a control is set for them alone.
        </p>
      ) : !applies ? (
        <p className="text-xs text-ink/55">
          Owners may always do everything. Controls set here apply if their role changes.
        </p>
      ) : !followsDefault ? (
        <p className="text-xs text-ink/55">
          Administrators don&apos;t follow the members&apos; default. Only controls turned off here
          limit them, and only an Owner can change these.
        </p>
      ) : null}
      <ul className="space-y-2">
        {MEMBER_CONTROLS.map((info) => (
          <li key={info.control} className="flex items-start justify-between gap-3 text-sm">
            <span>
              <span className="block text-ink">{info.label}</span>
              <span className="block text-xs text-ink/55">{info.detail}</span>
            </span>
            {userId === null ? (
              <input
                type="checkbox"
                className="mt-1"
                aria-label={`${info.label} allowed by default`}
                checked={defaults[info.control]}
                disabled={locked}
                onChange={(e) => void saveDefault(info.control, e.target.checked)}
              />
            ) : (
              <select
                className={selectClassName}
                aria-label={`${info.label} for user ${userId}`}
                value={overrideChoice(overrides, info.control)}
                disabled={locked}
                onChange={(e) =>
                  void saveOverride(userId, info.control, e.target.value as OverrideChoice)
                }
              >
                <option value="default">
                  {followsDefault
                    ? `Default (${defaults[info.control] ? "allowed" : "off"})`
                    : "Not set (allowed)"}
                </option>
                <option value="on">Allowed</option>
                <option value="off">Off</option>
              </select>
            )}
          </li>
        ))}
      </ul>
      {error ? (
        <p role="alert" className="text-xs text-brick">
          {error}
        </p>
      ) : null}
    </section>
  );
}
