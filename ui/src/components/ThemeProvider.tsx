import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import { Button } from "@/components/ui/button";
import {
  applyResolvedTheme,
  osPrefersDark,
  readStoredTheme,
  resolveTheme,
  writeStoredTheme,
  type ResolvedTheme,
  type ThemePreference,
} from "@/lib/theme";

type ThemeContextValue = {
  preference: ThemePreference;
  resolved: ResolvedTheme;
  setPreference: (pref: ThemePreference) => void;
};

const ThemeContext = createContext<ThemeContextValue | null>(null);

/**
 * Applies the stored theme preference to `<html>` and follows OS changes
 * when the preference is `system`.
 *
 * @param props - App tree to wrap.
 */
export function ThemeProvider({ children }: { children: ReactNode }) {
  const [preference, setPreferenceState] = useState<ThemePreference>(readStoredTheme);
  const [osDark, setOsDark] = useState(osPrefersDark);

  useEffect(() => {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = () => setOsDark(mq.matches);
    mq.addEventListener("change", onChange);
    setOsDark(mq.matches);
    return () => mq.removeEventListener("change", onChange);
  }, []);

  const resolved = useMemo(
    () => resolveTheme(preference, osDark),
    [preference, osDark],
  );

  useEffect(() => {
    applyResolvedTheme(resolved);
  }, [resolved]);

  const setPreference = useCallback((pref: ThemePreference) => {
    writeStoredTheme(pref);
    setPreferenceState(pref);
  }, []);

  const value = useMemo(
    () => ({ preference, resolved, setPreference }),
    [preference, resolved, setPreference],
  );

  return <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>;
}

/**
 * Reads the theme context (must be under `ThemeProvider`).
 *
 * @returns Preference, resolved appearance, and setter.
 * @throws When used outside `ThemeProvider`.
 */
export function useTheme(): ThemeContextValue {
  const ctx = useContext(ThemeContext);
  if (!ctx) {
    throw new Error("useTheme must be used within ThemeProvider");
  }
  return ctx;
}

/**
 * True when the SPA has resolved to the dark appearance.
 *
 * @returns Whether `html.dark` should be active.
 */
export function useResolvedDark(): boolean {
  return useTheme().resolved === "dark";
}

const THEME_OPTIONS: { id: ThemePreference; label: string }[] = [
  { id: "system", label: "System" },
  { id: "light", label: "Light" },
  { id: "dark", label: "Dark" },
];

/**
 * System / Light / Dark control that writes the local (and caller-synced) preference.
 *
 * @param props - Current value, change handler, and optional compact layout.
 */
export function ThemePreferenceControl({
  value,
  onChange,
  disabled,
  compact = false,
  id,
}: {
  value: ThemePreference;
  onChange: (pref: ThemePreference) => void;
  disabled?: boolean;
  compact?: boolean;
  id?: string;
}) {
  return (
    <div
      id={id}
      role="radiogroup"
      aria-label="Appearance"
      className="flex flex-wrap gap-2"
    >
      {THEME_OPTIONS.map((opt) => (
        <Button
          key={opt.id}
          type="button"
          role="radio"
          aria-checked={value === opt.id}
          variant={value === opt.id ? "secondary" : "ghost"}
          disabled={disabled}
          className={compact ? "h-8 px-2.5 text-xs" : undefined}
          onClick={() => onChange(opt.id)}
        >
          {opt.label}
        </Button>
      ))}
    </div>
  );
}
