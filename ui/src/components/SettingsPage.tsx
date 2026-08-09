import { useEffect, useMemo, useState, type FormEvent } from "react";
import { LogOut, RefreshCw } from "lucide-react";
import { AppNav, type AppNavProps } from "@/components/AppNav";
import { ErrorStatePage } from "@/components/ErrorStatePage";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  fetchPreferences,
  fetchSettings,
  isApiError,
  patchPreferences,
  patchSettings,
  type PluginSettingOption,
  signOut,
  type AuthRole,
  type SettingsResponse,
  type UserPreferences,
} from "@/lib/api";

export function SettingsPage({
  onLogout,
  onSessionExpired,
  nav,
  role,
}: {
  onLogout: () => void;
  onSessionExpired: () => void;
  nav: AppNavProps;
  role?: AuthRole;
}) {
  const [preferences, setPreferences] = useState<UserPreferences | null>(null);
  const [settings, setSettings] = useState<SettingsResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [preferencesLoadError, setPreferencesLoadError] = useState<string | null>(null);
  const [operatorLoadError, setOperatorLoadError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [daemonListen, setDaemonListen] = useState("");
  const [daemonAuthEnabled, setDaemonAuthEnabled] = useState(true);
  const [autoAcquire, setAutoAcquire] = useState(false);
  const [pluginValues, setPluginValues] = useState<Record<string, string>>({});
  const [pluginErrors, setPluginErrors] = useState<Record<string, string>>({});
  const [preferencesBaseline, setPreferencesBaseline] = useState<UserPreferences | null>(null);
  const [operatorBaseline, setOperatorBaseline] = useState<{
    daemonListen: string;
    daemonAuthEnabled: boolean;
    autoAcquire: boolean;
    pluginValues: Record<string, string>;
  } | null>(null);

  function buildPluginValues(nextSettings: SettingsResponse): Record<string, string> {
    const out: Record<string, string> = {};
    for (const plugin of nextSettings.plugins) {
      for (const option of plugin.settings) {
        out[option.key] = option.value;
      }
    }
    return out;
  }

  async function withRequestTimeout<T>(promise: Promise<T>, label: string, timeoutMs = 10_000): Promise<T> {
    let timeoutHandle: ReturnType<typeof setTimeout> | undefined;
    try {
      const timeoutPromise = new Promise<never>((_, reject) => {
        timeoutHandle = setTimeout(() => {
          reject(new Error(`${label} timed out after ${Math.floor(timeoutMs / 1000)}s`));
        }, timeoutMs);
      });
      return await Promise.race([promise, timeoutPromise]);
    } finally {
      if (timeoutHandle) clearTimeout(timeoutHandle);
    }
  }

  function validatePluginValue(): string | null {
    return null;
  }

  function normalizePreferences(preferences: UserPreferences): UserPreferences {
    return {
      default_view: preferences.default_view,
      disabled_shelves: preferences.disabled_shelves.map((item) => item.trim()).filter(Boolean),
    };
  }

  function parseBooleanLike(value: string): boolean {
    const normalized = value.trim().toLowerCase();
    return normalized === "1" || normalized === "true" || normalized === "yes" || normalized === "on";
  }

  function pluginValidationForOption(value: string, option: PluginSettingOption): string | null {
    if (option.value_type === "boolean") {
      return null;
    }
    if (option.choices?.length) {
      return option.choices.some((choice) => choice.value === value)
        ? null
        : "Choose a valid option";
    }
    if (option.value_type === "number") {
      if (!value.trim()) {
        return null;
      }
      if (Number.isNaN(Number(value))) {
        return "Enter a valid number";
      }
      return null;
    }
    return validatePluginValue();
  }

  const operatorHasValidationErrors = Object.keys(pluginErrors).length > 0;

  const pluginsByKind = useMemo(() => {
    const buckets = new Map<string, SettingsResponse["plugins"]>();
    if (!settings) {
      return buckets;
    }
    for (const plugin of settings.plugins) {
      const kind = plugin.kind || "other";
      const current = buckets.get(kind) ?? [];
      current.push(plugin);
      buckets.set(kind, current);
    }
    return buckets;
  }, [settings]);

  const preferencesDirty = useMemo(() => {
    if (!preferences || !preferencesBaseline) {
      return false;
    }
    const current = normalizePreferences(preferences);
    const baseline = normalizePreferences(preferencesBaseline);
    if (current.default_view !== baseline.default_view) {
      return true;
    }
    if (current.disabled_shelves.length !== baseline.disabled_shelves.length) {
      return true;
    }
    return current.disabled_shelves.some((item, index) => item !== baseline.disabled_shelves[index]);
  }, [preferences, preferencesBaseline]);

  const operatorDirty = useMemo(() => {
    if (!operatorBaseline) {
      return false;
    }
    if (daemonListen !== operatorBaseline.daemonListen) {
      return true;
    }
    if (daemonAuthEnabled !== operatorBaseline.daemonAuthEnabled) {
      return true;
    }
    if (autoAcquire !== operatorBaseline.autoAcquire) {
      return true;
    }
    const currentKeys = Object.keys(pluginValues);
    const baselineKeys = Object.keys(operatorBaseline.pluginValues);
    if (currentKeys.length !== baselineKeys.length) {
      return true;
    }
    return currentKeys.some((key) => pluginValues[key] !== operatorBaseline.pluginValues[key]);
  }, [autoAcquire, daemonAuthEnabled, daemonListen, operatorBaseline, pluginValues]);

  async function refresh() {
    setError(null);
    setPreferencesLoadError(null);
    setOperatorLoadError(null);
    setLoading(true);
    try {
      if (role === "operator") {
        const [prefsResult, settingsResult] = await Promise.allSettled([
          withRequestTimeout(fetchPreferences(), "Preferences request"),
          withRequestTimeout(fetchSettings(), "Operator settings request"),
        ]);

        const sessionExpired = [prefsResult, settingsResult].some((result) =>
          result.status === "rejected" && isApiError(result.reason) && result.reason.status === 401,
        );
        if (sessionExpired) {
          onSessionExpired();
          return;
        }

        if (prefsResult.status === "fulfilled") {
          setPreferences(prefsResult.value);
          setPreferencesBaseline(normalizePreferences(prefsResult.value));
        } else {
          setPreferences(null);
          setPreferencesLoadError(
            prefsResult.reason instanceof Error
              ? prefsResult.reason.message
              : "Failed to load preferences",
          );
        }

        if (settingsResult.status === "fulfilled") {
          const nextSettings = settingsResult.value;
          const nextPluginValues = buildPluginValues(nextSettings);
          setSettings(nextSettings);
          setDaemonListen(nextSettings.settings["daemon.listen"] ?? "");
          setDaemonAuthEnabled(nextSettings.settings["daemon.auth.enabled"] === "true");
          setAutoAcquire(nextSettings.settings["library.auto_acquire"] === "true");
          setPluginValues(nextPluginValues);
          setPluginErrors({});
          setOperatorBaseline({
            daemonListen: nextSettings.settings["daemon.listen"] ?? "",
            daemonAuthEnabled: nextSettings.settings["daemon.auth.enabled"] === "true",
            autoAcquire: nextSettings.settings["library.auto_acquire"] === "true",
            pluginValues: nextPluginValues,
          });
        } else {
          setSettings(null);
          setOperatorLoadError(
            settingsResult.reason instanceof Error
              ? settingsResult.reason.message
              : "Failed to load operator settings",
          );
        }
      } else {
        const prefs = await withRequestTimeout(fetchPreferences(), "Preferences request");
        setPreferences(prefs);
        setPreferencesBaseline(normalizePreferences(prefs));
        setSettings(null);
        setPluginValues({});
        setPluginErrors({});
        setOperatorBaseline(null);
      }
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        onSessionExpired();
        return;
      }
      setError(
        err instanceof Error
          ? err.message
          : "Failed to load settings. If this persists, verify bookclerkd is running.",
      );
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    void refresh();
  }, []);

  async function onPreferencesSave(e: FormEvent) {
    e.preventDefault();
    if (!preferences || !preferencesDirty) return;
    setSaving(true);
    setError(null);
    try {
      const payload = normalizePreferences(preferences);
      const next = await withRequestTimeout(patchPreferences({
        default_view: payload.default_view,
        disabled_shelves: payload.disabled_shelves,
      }), "Save preferences");
      setPreferences(next);
      setPreferencesBaseline(normalizePreferences(next));
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        onSessionExpired();
        return;
      }
      setError(err instanceof Error ? err.message : "Failed to save preferences");
    } finally {
      setSaving(false);
    }
  }

  async function onOperatorSave(e: FormEvent) {
    e.preventDefault();
    if (!operatorDirty || operatorHasValidationErrors) {
      return;
    }
    setSaving(true);
    setError(null);
    try {
      const pluginUpdates = Object.entries(pluginValues).map(([key, value]) => ({ key, value }));
      const next = await withRequestTimeout(patchSettings({
        settings: [
          { key: "daemon.listen", value: daemonListen },
          { key: "daemon.auth.enabled", value: String(daemonAuthEnabled) },
          { key: "library.auto_acquire", value: String(autoAcquire) },
          ...pluginUpdates,
        ],
      }), "Save operator settings");
      const nextPluginValues = buildPluginValues(next);
      setSettings(next);
      setPluginValues(nextPluginValues);
      setPluginErrors({});
      setOperatorBaseline({
        daemonListen: next.settings["daemon.listen"] ?? daemonListen,
        daemonAuthEnabled: next.settings["daemon.auth.enabled"] === "true",
        autoAcquire: next.settings["library.auto_acquire"] === "true",
        pluginValues: nextPluginValues,
      });
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        onSessionExpired();
        return;
      }
      setError(err instanceof Error ? err.message : "Failed to save operator settings");
    } finally {
      setSaving(false);
    }
  }

  async function onSignOut() {
    await signOut(role);
    onLogout();
  }

  return (
    <div className="flex h-full flex-col">
      <header className="sticky top-0 z-10 border-b border-ink/10 bg-paper/85 px-3 py-3 backdrop-blur-md sm:px-5">
        <div className="mx-auto flex max-w-6xl items-center justify-between gap-3">
          <div className="flex items-center gap-3 sm:gap-5">
            <img src="/bookclerk-logo.svg" alt="Bookclerk" className="h-8 w-auto sm:h-9" />
            <AppNav {...nav} />
          </div>
          <div className="flex items-center gap-2">
            <Button variant="secondary" onClick={() => void refresh()} disabled={loading || saving}>
              <RefreshCw className="h-4 w-4" />
              Refresh
            </Button>
            <Button variant="ghost" onClick={() => void onSignOut()} aria-label="Sign out">
              <LogOut className="h-4 w-4" />
            </Button>
          </div>
        </div>
      </header>

      <main className="mx-auto flex w-full max-w-6xl flex-1 flex-col gap-8 overflow-auto px-4 py-6">
        <div className="space-y-1">
          <h1 className="font-display text-2xl font-semibold tracking-tight text-ink">Settings</h1>
          <p className="text-sm text-ink/60">
            {role === "operator"
              ? "Manage your preferences and operator configuration."
              : "Manage your preferences."}
          </p>
        </div>

        {error ? (
          <ErrorStatePage
            title="Settings request failed"
            message={error}
            onRetry={() => void refresh()}
          />
        ) : null}

        <section className="space-y-3 rounded-lg border border-ink/10 bg-white/40 p-4">
          <h2 className="text-lg font-semibold text-ink">Preferences</h2>
          {loading ? (
            <p className="text-sm text-ink/50">Loading preferences...</p>
          ) : preferences ? (
            <form className="grid gap-3 sm:grid-cols-2" onSubmit={(e) => void onPreferencesSave(e)}>
              <label className="space-y-1 text-sm text-ink/70">
                <span>Default view</span>
                <select
                  className="w-full rounded-md border border-ink/10 bg-paper px-3 py-2"
                  value={preferences.default_view}
                  onChange={(e) =>
                    setPreferences((current) => current ? { ...current, default_view: e.target.value as UserPreferences["default_view"] } : current)
                  }
                >
                  <option value="discover">Discover</option>
                  <option value="library">Library</option>
                  <option value="wishlist">Wishlist</option>
                  <option value="accounts">Accounts</option>
                </select>
              </label>
              <label className="space-y-1 text-sm text-ink/70">
                <span>Disabled shelves</span>
                <Input
                  value={preferences.disabled_shelves.join(",")}
                  onChange={(e) =>
                    setPreferences((current) => current ? { ...current, disabled_shelves: e.target.value.split(",").map((item) => item.trim()).filter(Boolean) } : current)
                  }
                  placeholder="shelf-a,shelf-b"
                />
              </label>
              <div className="sm:col-span-2">
                <Button type="submit" disabled={saving || !preferencesDirty}>
                  Save preferences
                </Button>
                {!preferencesDirty ? (
                  <p className="mt-1 text-xs text-ink/50">No unsaved preference changes.</p>
                ) : null}
              </div>
            </form>
          ) : (
            <ErrorStatePage
              title="Preferences unavailable"
              message={preferencesLoadError ?? "Preferences could not be loaded. Verify bookclerkd is running, then try Refresh."}
              onRetry={() => void refresh()}
            />
          )}
        </section>

        {role === "operator" ? (
          <section className="space-y-3 rounded-lg border border-ink/10 bg-white/40 p-4">
            <h2 className="text-lg font-semibold text-ink">Operator configuration</h2>
            {loading ? (
              <p className="text-sm text-ink/50">Loading operator settings...</p>
            ) : settings ? (
              <form className="grid gap-3" onSubmit={(e) => void onOperatorSave(e)}>
                <label className="space-y-1 text-sm text-ink/70">
                  <span>Daemon listen address</span>
                  <Input value={daemonListen} onChange={(e) => setDaemonListen(e.target.value)} />
                </label>
                <label className="flex items-center gap-2 text-sm text-ink/70">
                  <input
                    type="checkbox"
                    checked={daemonAuthEnabled}
                    onChange={(e) => setDaemonAuthEnabled(e.target.checked)}
                  />
                  Enable daemon auth
                </label>
                <label className="flex items-center gap-2 text-sm text-ink/70">
                  <input
                    type="checkbox"
                    checked={autoAcquire}
                    onChange={(e) => setAutoAcquire(e.target.checked)}
                  />
                  Auto acquire
                </label>
                <div className="space-y-2">
                  <p className="text-sm font-medium text-ink">Plugin settings</p>
                  {Array.from(pluginsByKind.entries()).map(([kind, plugins]) => (
                    <div key={kind} className="space-y-2">
                      <p className="text-xs font-semibold uppercase tracking-wide text-ink/60">{kind}</p>
                      {plugins.map((plugin) => (
                        <div key={`${plugin.kind}-${plugin.id}`} className="rounded-md border border-ink/10 bg-paper/50 p-3">
                          <p className="text-sm font-semibold text-ink">{plugin.id}</p>
                          {plugin.settings.length === 0 ? (
                            <p className="mt-2 text-xs text-ink/50">No editable settings exposed for this plugin.</p>
                          ) : null}
                          {plugin.settings.map((option) => (
                            <label key={`${plugin.kind}-${plugin.id}-${option.key}`} className="mt-2 flex flex-col gap-1 text-sm text-ink/70">
                              <span>{option.label}</span>
                              {option.value_type === "boolean" ? (
                                <label className="flex items-center gap-2 text-sm text-ink/70">
                                  <input
                                    type="checkbox"
                                    checked={parseBooleanLike(pluginValues[option.key] ?? option.value)}
                                    onChange={(e) => {
                                      const settingKey = option.key;
                                      const nextValue = String(e.target.checked);
                                      setPluginValues((current) => ({
                                        ...current,
                                        [settingKey]: nextValue,
                                      }));
                                      setPluginErrors((current) => {
                                        const { [settingKey]: _removed, ...rest } = current;
                                        return rest;
                                      });
                                    }}
                                  />
                                  Enabled
                                </label>
                              ) : option.choices?.length ? (
                                <select
                                  value={pluginValues[option.key] ?? option.value}
                                  onChange={(e) => {
                                    const settingKey = option.key;
                                    const nextValue = e.target.value;
                                    setPluginValues((current) => ({
                                      ...current,
                                      [settingKey]: nextValue,
                                    }));
                                    const validation = pluginValidationForOption(nextValue, option);
                                    setPluginErrors((current) => {
                                      if (!validation) {
                                        const { [settingKey]: _removed, ...rest } = current;
                                        return rest;
                                      }
                                      return {
                                        ...current,
                                        [settingKey]: validation,
                                      };
                                    });
                                  }}
                                  className="rounded-md border border-ink/15 bg-white/80 px-3 py-2 text-sm shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30"
                                >
                                  {option.choices.map((choice) => (
                                    <option key={`${option.key}-${choice.value}`} value={choice.value}>
                                      {choice.label}
                                    </option>
                                  ))}
                                </select>
                              ) : (
                                <Input
                                  type={option.value_type === "number" ? "number" : "text"}
                                  value={pluginValues[option.key] ?? option.value}
                                  onChange={(e) => {
                                    const settingKey = option.key;
                                    const nextValue = e.target.value;
                                    setPluginValues((current) => ({
                                      ...current,
                                      [settingKey]: nextValue,
                                    }));
                                    const validation = pluginValidationForOption(nextValue, option);
                                    setPluginErrors((current) => {
                                      if (!validation) {
                                        const { [settingKey]: _removed, ...rest } = current;
                                        return rest;
                                      }
                                      return {
                                        ...current,
                                        [settingKey]: validation,
                                      };
                                    });
                                  }}
                                />
                              )}
                              {pluginErrors[option.key] ? (
                                <span className="text-xs text-brick" role="alert">
                                  {pluginErrors[option.key]}
                                </span>
                              ) : null}
                            </label>
                          ))}
                        </div>
                      ))}
                    </div>
                  ))}
                </div>
                <div className="space-y-1">
                  <Button type="submit" disabled={saving || !operatorDirty || operatorHasValidationErrors}>
                    Save operator settings
                  </Button>
                  {!operatorDirty ? (
                    <p className="text-xs text-ink/50">No unsaved operator changes.</p>
                  ) : null}
                  {operatorHasValidationErrors ? (
                    <p className="text-xs text-brick">Fix plugin field errors before saving.</p>
                  ) : null}
                </div>
              </form>
            ) : (
              <ErrorStatePage
                title="Operator settings unavailable"
                message={operatorLoadError ?? "Operator settings could not be loaded. Check your operator session, verify bookclerkd is running, then try Refresh."}
                onRetry={() => void refresh()}
              />
            )}
          </section>
        ) : null}
      </main>
    </div>
  );
}
