import { useCallback, useEffect, useState } from "react";
import {
  getDefaultAdminControls,
  getUserAdminControls,
  isApiError,
  patchDefaultAdminControls,
  patchUserAdminControls,
  type AdminControl,
  type AdminControlOverrides,
  type AdminControls,
} from "@/lib/api";
import {
  ADMIN_CONTROLS,
  ALL_ADMIN_ALLOWED,
  adminOverrideChoice,
  type AdminOverrideChoice,
} from "@/lib/adminControls";

const selectClassName =
  "h-8 rounded-md border border-ink/15 bg-card-strong px-2 text-sm text-ink outline-none focus:border-teal";

/** Whose switches the editor changes. */
export type AdminControlsTarget = { kind: "user"; userId: number } | { kind: "default" };

/** Props of {@link AdminControlsEditor}. */
export interface AdminControlsEditorProps {
  /** One Administrator, or the library's default. */
  target: AdminControlsTarget;
  /** True while the person may not change them. */
  disabled?: boolean;
}

/**
 * Edits what Administrators may do; only an Owner or the operator sees it.
 * For the library's default each switch is a checkbox; for one
 * Administrator each switch follows the default or is set on or off for
 * them alone. Every change saves at once.
 */
export function AdminControlsEditor({ target, disabled = false }: AdminControlsEditorProps) {
  const [loaded, setLoaded] = useState(false);
  const [defaults, setDefaults] = useState<AdminControls>(ALL_ADMIN_ALLOWED);
  const [overrides, setOverrides] = useState<AdminControlOverrides>({});
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const userId = target.kind === "user" ? target.userId : null;

  const load = useCallback(async () => {
    setLoaded(false);
    setError(null);
    try {
      if (userId === null) {
        setDefaults((await getDefaultAdminControls()).controls);
      } else {
        const res = await getUserAdminControls(userId);
        setDefaults(res.defaults);
        setOverrides(res.overrides);
      }
      setLoaded(true);
    } catch (err) {
      setError(isApiError(err) ? err.message : "The switches could not be read.");
    }
  }, [userId]);

  useEffect(() => {
    void load();
  }, [load]);

  async function saveDefault(control: AdminControl, value: boolean) {
    setSaving(true);
    setError(null);
    try {
      setDefaults((await patchDefaultAdminControls({ [control]: value })).controls);
    } catch (err) {
      setError(isApiError(err) ? err.message : "The switches could not be saved.");
    } finally {
      setSaving(false);
    }
  }

  async function saveOverride(id: number, control: AdminControl, choice: AdminOverrideChoice) {
    setSaving(true);
    setError(null);
    try {
      const res = await patchUserAdminControls(id, { [control]: choice });
      setDefaults(res.defaults);
      setOverrides(res.overrides);
    } catch (err) {
      setError(isApiError(err) ? err.message : "The switches could not be saved.");
    } finally {
      setSaving(false);
    }
  }

  const locked = disabled || saving || !loaded;
  const heading = userId === null ? "Default Administrator switches" : "Administrator switches";

  return (
    <section className="space-y-2" aria-label={heading}>
      <p className="text-xs font-semibold uppercase tracking-wide text-ink/70">{heading}</p>
      <p className="text-xs text-ink/70">
        {userId === null
          ? "Administrators follow these unless a switch is set for them alone. Only an Owner can change them."
          : "Only an Owner can change these."}
      </p>
      <ul className="space-y-2">
        {ADMIN_CONTROLS.map((info) => (
          <li key={info.control} className="flex items-start justify-between gap-3 text-sm">
            <span>
              <span className="block text-ink">{info.label}</span>
              <span className="block text-xs text-ink/70">{info.detail}</span>
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
                value={adminOverrideChoice(overrides, info.control)}
                disabled={locked}
                onChange={(e) =>
                  void saveOverride(userId, info.control, e.target.value as AdminOverrideChoice)
                }
              >
                <option value="default">
                  {`Default (${defaults[info.control] ? "allowed" : "off"})`}
                </option>
                <option value="on">Allowed</option>
                <option value="off">Off</option>
              </select>
            )}
          </li>
        ))}
      </ul>
      {error ? (
        <p role="alert" className="text-xs text-brick-text">
          {error}
        </p>
      ) : null}
    </section>
  );
}
