import {
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { Ban, Settings2, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { StoreLogo } from "@/components/StoreLogo";
import {
  fetchDiscoverFeed,
  fetchPreferences,
  patchPreferences,
  type AppView,
  type CatalogSearchSort,
  type CatalogSortDir,
  type PortalSource,
  type ShelfKindInfo,
} from "@/lib/api";
import {
  CATALOG_LANGUAGE_ALL,
  CATALOG_LANGUAGE_OPTIONS,
  CATALOG_SORT_OPTIONS,
  defaultSortDirFor,
  preferredCatalogLanguage,
  storeLabel,
} from "@/lib/catalogTitle";
import { loadEnabledSources } from "@/lib/enabledSources";
import {
  PreferencesContext,
  ShelvesChangeRegistrationContext,
  type ShelvesListener,
} from "@/components/preferencesContext";
import { usePreferences } from "@/components/usePreferences";
import {
  ThemePreferenceControl,
  useTheme,
} from "@/components/ThemeProvider";
import type { ThemePreference } from "@/lib/theme";

const LANG_BROWSER = "__browser__";

/**
 * Header control that opens the preferences dialog — safe on every authenticated page.
 */
export function PreferencesButton() {
  const { openPreferences, preferencesOpen } = usePreferences();
  return (
    <Button
      variant="ghost"
      onClick={openPreferences}
      aria-label="Preferences"
      aria-haspopup="dialog"
      aria-expanded={preferencesOpen}
    >
      <Settings2 className="h-4 w-4" />
    </Button>
  );
}

/**
 * Provides preferences state and dialog for authenticated app chrome.
 *
 * @param props - Children, current default view, and optional change callback.
 */
export function PreferencesProvider({
  children,
  defaultView,
  onDefaultViewChange,
}: {
  children: ReactNode;
  defaultView: AppView;
  onDefaultViewChange?: (view: AppView) => void;
}) {
  const [open, setOpen] = useState(false);
  const shelvesListener = useRef<ShelvesListener | null>(null);

  const setOnShelvesChange = useCallback((fn: ShelvesListener | null) => {
    shelvesListener.current = fn;
  }, []);

  return (
    <PreferencesContext.Provider
      value={{
        openPreferences: () => setOpen(true),
        preferencesOpen: open,
      }}
    >
      <ShelvesChangeRegistrationContext.Provider value={setOnShelvesChange}>
        {children}
      </ShelvesChangeRegistrationContext.Provider>
      <PreferencesDialog
        open={open}
        onOpenChange={setOpen}
        defaultView={defaultView}
        onDefaultViewChange={onDefaultViewChange}
        onDisabledShelvesChange={() => shelvesListener.current?.()}
      />
    </PreferencesContext.Provider>
  );
}

function PreferencesDialog({
  open,
  onOpenChange,
  defaultView,
  onDefaultViewChange,
  onDisabledShelvesChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  defaultView: AppView;
  onDefaultViewChange?: (view: AppView) => void;
  onDisabledShelvesChange?: () => void;
}) {
  const titleId = useId();
  const panelRef = useRef<HTMLDivElement | null>(null);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [prefsView, setPrefsView] = useState<AppView>(defaultView);
  const [ignored, setIgnored] = useState<string[]>([]);
  const [shelfKinds, setShelfKinds] = useState<ShelfKindInfo[]>([]);
  const [discoverSort, setDiscoverSort] = useState<CatalogSearchSort>("relevance");
  const [discoverSortDir, setDiscoverSortDir] =
    useState<CatalogSortDir>("desc");
  const [discoverLanguage, setDiscoverLanguage] = useState(LANG_BROWSER);
  const [excludedSources, setExcludedSources] = useState<string[]>([]);
  const [enabledSources, setEnabledSources] = useState<PortalSource[]>([]);
  const { preference: themePref, setPreference: setThemePreference } = useTheme();
  const [theme, setTheme] = useState<ThemePreference>(themePref);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    void (async () => {
      try {
        const [prefs, feed, sources] = await Promise.all([
          fetchPreferences(),
          fetchDiscoverFeed(1),
          loadEnabledSources(),
        ]);
        if (cancelled) return;
        setPrefsView(prefs.default_view);
        setIgnored(prefs.disabled_shelves);
        setShelfKinds(feed.shelf_kinds ?? []);
        setDiscoverSort(prefs.discover_sort);
        setDiscoverSortDir(prefs.discover_sort_dir);
        setDiscoverLanguage(prefs.discover_language ?? LANG_BROWSER);
        // Keep exclusions for stores that are currently disabled so they
        // reapply when those sources are re-enabled later.
        setExcludedSources(prefs.discover_excluded_sources ?? []);
        setEnabledSources(sources);
        setTheme(prefs.theme);
        setThemePreference(prefs.theme);
      } catch (err) {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : "Failed to load preferences");
        }
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [open, setThemePreference]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onOpenChange(false);
    };
    window.addEventListener("keydown", onKey);
    const prevOverflow = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    // Focus the panel for keyboard users.
    panelRef.current?.focus();
    return () => {
      window.removeEventListener("keydown", onKey);
      document.body.style.overflow = prevOverflow;
    };
  }, [open, onOpenChange]);

  async function onThemeSelect(next: ThemePreference) {
    const prev = theme;
    setTheme(next);
    setThemePreference(next);
    setBusy(true);
    setError(null);
    try {
      const saved = await patchPreferences({ theme: next });
      setTheme(saved.theme);
      setThemePreference(saved.theme);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to save appearance");
      setTheme(prev);
      setThemePreference(prev);
    } finally {
      setBusy(false);
    }
  }

  async function onDefaultViewSelect(view: AppView) {
    const prev = prefsView;
    setPrefsView(view);
    setBusy(true);
    setError(null);
    try {
      const saved = await patchPreferences({ default_view: view });
      setPrefsView(saved.default_view);
      onDefaultViewChange?.(saved.default_view);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to save default view");
      setPrefsView(prev);
    } finally {
      setBusy(false);
    }
  }

  async function toggleIgnored(kindId: string) {
    const prev = ignored;
    const next = prev.includes(kindId)
      ? prev.filter((x) => x !== kindId)
      : [...prev, kindId];
    setIgnored(next);
    setBusy(true);
    setError(null);
    try {
      const saved = await patchPreferences({ disabled_shelves: next });
      setIgnored(saved.disabled_shelves);
      onDisabledShelvesChange?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to save shelf prefs");
      setIgnored(prev);
    } finally {
      setBusy(false);
    }
  }

  async function saveDiscoverPrefs(patch: {
    discover_sort?: CatalogSearchSort;
    discover_sort_dir?: CatalogSortDir;
    discover_language?: string | null;
    discover_excluded_sources?: string[];
  }) {
    setBusy(true);
    setError(null);
    try {
      const saved = await patchPreferences(patch);
      setDiscoverSort(saved.discover_sort);
      setDiscoverSortDir(saved.discover_sort_dir);
      setDiscoverLanguage(saved.discover_language ?? LANG_BROWSER);
      setExcludedSources(saved.discover_excluded_sources ?? []);
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to save Discover prefs",
      );
    } finally {
      setBusy(false);
    }
  }

  async function toggleExcludedSource(sourceId: string) {
    const key = sourceId.toLowerCase();
    const prev = excludedSources;
    const next = prev.some((s) => s.toLowerCase() === key)
      ? prev.filter((s) => s.toLowerCase() !== key)
      : [...prev, key];
    setExcludedSources(next);
    await saveDiscoverPrefs({ discover_excluded_sources: next });
  }

  if (!open) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-scrim px-4 py-10 sm:items-center"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onOpenChange(false);
      }}
    >
      <div
        ref={panelRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        className="w-full max-w-2xl rounded-lg border border-ink/10 bg-paper p-5 shadow-xl outline-none"
      >
        <div className="mb-4 flex items-start justify-between gap-3">
          <div>
            <h2 id={titleId} className="font-display text-xl font-semibold text-ink">
              Preferences
            </h2>
            <p className="mt-1 text-sm text-ink/55">
              Saved to your Bookclerk profile for this account.
            </p>
          </div>
          <Button
            variant="ghost"
            className="shrink-0 px-2"
            onClick={() => onOpenChange(false)}
            aria-label="Close preferences"
          >
            <X className="h-4 w-4" />
          </Button>
        </div>

        {error ? (
          <p className="mb-4 text-sm font-medium text-brick" role="alert">
            {error}
          </p>
        ) : null}

        {loading ? (
          <p className="text-sm text-ink/50">Loading preferences…</p>
        ) : (
          <div className="space-y-6">
            <div className="space-y-2">
              <h3 className="text-base font-semibold text-ink">Appearance</h3>
              <p className="text-sm text-ink/55">
                Follows this device when set to System. Light is the designed
                look; Dark adapts the same palette.
              </p>
              <ThemePreferenceControl
                value={theme}
                disabled={busy}
                onChange={(next) => void onThemeSelect(next)}
              />
            </div>

            <div className="space-y-2">
              <h3 className="text-base font-semibold text-ink">Default view</h3>
              <p className="text-sm text-ink/55">
                Where this account opens after sign-in.
              </p>
              <div className="flex flex-wrap gap-2">
                {(
                  [
                    ["discover", "Discover"],
                    ["wishlist", "Wishlist"],
                    ["library", "Library"],
                    ["accounts", "Accounts"],
                  ] as const
                ).map(([id, label]) => (
                  <Button
                    key={id}
                    variant={prefsView === id ? "secondary" : "ghost"}
                    disabled={busy}
                    onClick={() => void onDefaultViewSelect(id)}
                  >
                    {label}
                  </Button>
                ))}
              </div>
            </div>

            <div className="space-y-3">
              <div>
                <h3 className="text-base font-semibold text-ink">Discover defaults</h3>
                <p className="text-sm text-ink/55">
                  Applied when you open a catalog search. New stores stay included
                  unless you uncheck them here.
                </p>
              </div>
              <div className="grid gap-3 sm:grid-cols-2">
                <label className="space-y-1 text-sm text-ink">
                  <span className="font-medium">Preferred sort</span>
                  <select
                    className="w-full rounded-md border border-ink/15 bg-card-strong px-3 py-2 shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30"
                    value={discoverSort}
                    disabled={busy}
                    onChange={(e) => {
                      const next = e.target.value as CatalogSearchSort;
                      const dir = defaultSortDirFor(next);
                      setDiscoverSort(next);
                      setDiscoverSortDir(dir);
                      void saveDiscoverPrefs({
                        discover_sort: next,
                        discover_sort_dir: dir,
                      });
                    }}
                  >
                    {CATALOG_SORT_OPTIONS.map((opt) => (
                      <option key={opt.value} value={opt.value}>
                        {opt.label}
                      </option>
                    ))}
                  </select>
                </label>
                <label className="space-y-1 text-sm text-ink">
                  <span className="font-medium">Sort direction</span>
                  <select
                    className="w-full rounded-md border border-ink/15 bg-card-strong px-3 py-2 shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30"
                    value={discoverSortDir}
                    disabled={busy || discoverSort === "relevance"}
                    onChange={(e) => {
                      const next = e.target.value === "asc" ? "asc" : "desc";
                      setDiscoverSortDir(next);
                      void saveDiscoverPrefs({ discover_sort_dir: next });
                    }}
                  >
                    <option value="desc">Descending</option>
                    <option value="asc">Ascending</option>
                  </select>
                </label>
                <label className="space-y-1 text-sm text-ink sm:col-span-2">
                  <span className="font-medium">Preferred language</span>
                  <select
                    className="w-full rounded-md border border-ink/15 bg-card-strong px-3 py-2 shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30"
                    value={discoverLanguage}
                    disabled={busy}
                    onChange={(e) => {
                      const next = e.target.value;
                      setDiscoverLanguage(next);
                      void saveDiscoverPrefs({
                        discover_language:
                          next === LANG_BROWSER ? null : next,
                      });
                    }}
                  >
                    <option value={LANG_BROWSER}>
                      Browser default ({preferredCatalogLanguage()})
                    </option>
                    {CATALOG_LANGUAGE_OPTIONS.map((opt) => (
                      <option key={opt.value} value={opt.value}>
                        {opt.value === CATALOG_LANGUAGE_ALL
                          ? opt.label
                          : opt.label}
                      </option>
                    ))}
                  </select>
                </label>
              </div>
              <div className="space-y-2">
                <p className="text-sm font-medium text-ink">Stores to include</p>
                <p className="text-xs text-ink/50">
                  Only enabled storefront plugins appear here. Exclusions for
                  disabled stores are kept and apply again if re-enabled.
                </p>
                {enabledSources.length === 0 ? (
                  <p className="flex items-center gap-2 text-sm text-ink/55">
                    <Ban className="h-4 w-4 shrink-0 text-ink/40" aria-hidden />
                    <span>None</span>
                  </p>
                ) : (
                  <ul className="grid gap-2 sm:grid-cols-2">
                    {enabledSources.map((source) => {
                      const id = source.id.toLowerCase();
                      const on = !excludedSources.some(
                        (s) => s.toLowerCase() === id,
                      );
                      return (
                        <li key={source.id}>
                          <label className="flex cursor-pointer items-center gap-2 text-sm text-ink">
                            <input
                              type="checkbox"
                              checked={on}
                              disabled={busy}
                              onChange={() => void toggleExcludedSource(id)}
                            />
                            <StoreLogo source={id} className="h-4 w-4" />
                            <span>{source.name || storeLabel(id)}</span>
                          </label>
                        </li>
                      );
                    })}
                  </ul>
                )}
              </div>
            </div>

            {shelfKinds.length > 0 ? (
              <div className="space-y-3">
                <div>
                  <h3 className="text-base font-semibold text-ink">Shelves to show</h3>
                  <p className="text-sm text-ink/55">
                    All shelves are on by default. Uncheck any you want to hide for this
                    account.
                  </p>
                </div>
                <ul className="grid max-h-72 gap-2 overflow-y-auto sm:grid-cols-2">
                  {shelfKinds.map((kind) => {
                    const on = !ignored.includes(kind.id);
                    return (
                      <li key={kind.id}>
                        <label className="flex cursor-pointer items-start gap-2 text-sm text-ink">
                          <input
                            type="checkbox"
                            className="mt-0.5"
                            checked={on}
                            disabled={busy}
                            onChange={() => void toggleIgnored(kind.id)}
                          />
                          <span>
                            <span className="font-medium">{kind.label}</span>
                            <span className="block text-xs text-ink/45">{kind.id}</span>
                          </span>
                        </label>
                      </li>
                    );
                  })}
                </ul>
              </div>
            ) : null}
          </div>
        )}
      </div>
    </div>
  );
}
