/** Stored appearance preference. `system` follows the OS, falling back to light. */
export type ThemePreference = "system" | "light" | "dark";

/** Resolved appearance actually applied to the document. */
export type ResolvedTheme = "light" | "dark";

/** `localStorage` key for the anonymous / last-applied theme preference. */
export const THEME_STORAGE_KEY = "bookclerk-theme";

/** Browser chrome color while the SPA is in the designed light theme. */
export const THEME_COLOR_LIGHT = "#0B3553";

/** Browser chrome color while the SPA is in the adapted dark theme. */
export const THEME_COLOR_DARK = "#121c26";

/**
 * Maps an unknown value onto a stored theme preference.
 *
 * Unknown or empty values become `system` (follow the OS, fall back to light).
 *
 * @param raw - Value from storage, the preferences API, or a form control.
 * @returns Canonical preference.
 */
export function normalizeThemePreference(raw: unknown): ThemePreference {
  const s = typeof raw === "string" ? raw.trim().toLowerCase() : "";
  if (s === "light" || s === "dark") return s;
  return "system";
}

/**
 * True only when the OS/browser explicitly prefers a dark color scheme.
 *
 * Missing `matchMedia`, `no-preference`, and light all count as not-dark so
 * the designed light theme remains the fallback.
 *
 * @returns Whether `prefers-color-scheme: dark` currently matches.
 */
export function osPrefersDark(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-color-scheme: dark)").matches
  );
}

/**
 * Resolves a stored preference into the appearance to apply.
 *
 * @param pref - Stored tri-state preference.
 * @param osDark - Current OS dark hint; omit to read `matchMedia` now.
 * @returns `dark` only for an explicit Dark choice or System + OS dark.
 */
export function resolveTheme(
  pref: ThemePreference,
  osDark: boolean = osPrefersDark(),
): ResolvedTheme {
  if (pref === "light") return "light";
  if (pref === "dark") return "dark";
  return osDark ? "dark" : "light";
}

/**
 * Reads the theme preference from `localStorage`.
 *
 * @returns Stored preference, or `system` when missing or unreadable.
 */
export function readStoredTheme(): ThemePreference {
  try {
    return normalizeThemePreference(localStorage.getItem(THEME_STORAGE_KEY));
  } catch {
    return "system";
  }
}

/**
 * Persists the theme preference to `localStorage`.
 *
 * @param pref - Preference to store.
 */
export function writeStoredTheme(pref: ThemePreference): void {
  try {
    localStorage.setItem(THEME_STORAGE_KEY, pref);
  } catch {
    /* quota / private mode */
  }
}

/**
 * Applies a resolved appearance to `<html>` and the theme-color meta tag.
 *
 * Adds `html.dark` only for dark. Light is the absence of that class.
 *
 * @param resolved - Appearance to apply.
 */
export function applyResolvedTheme(resolved: ResolvedTheme): void {
  const root = document.documentElement;
  root.classList.toggle("dark", resolved === "dark");
  root.style.colorScheme = resolved;
  const meta = document.querySelector('meta[name="theme-color"]');
  if (meta) {
    meta.setAttribute(
      "content",
      resolved === "dark" ? THEME_COLOR_DARK : THEME_COLOR_LIGHT,
    );
  }
}
