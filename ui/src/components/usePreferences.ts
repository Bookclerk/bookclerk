import { useContext, useEffect, useRef } from "react";
import {
  PreferencesContext,
  ShelvesChangeRegistrationContext,
  type PreferencesContextValue,
  type ShelvesListener,
} from "@/components/preferencesContext";

/**
 * Reads the preferences context (must be under `PreferencesProvider`).
 *
 * @returns Preferences context value.
 * @throws When used outside `PreferencesProvider`.
 */
export function usePreferences(): PreferencesContextValue {
  const ctx = useContext(PreferencesContext);
  if (!ctx) {
    throw new Error("usePreferences must be used within PreferencesProvider");
  }
  return ctx;
}

/**
 * Registers a listener invoked when Discover shelf visibility prefs change.
 *
 * @param listener - Callback, or `null` to clear.
 */
export function useRegisterShelvesChangeListener(listener: ShelvesListener | null) {
  const setOnShelvesChange = useContext(ShelvesChangeRegistrationContext);
  const listenerRef = useRef(listener);
  listenerRef.current = listener;

  useEffect(() => {
    if (!setOnShelvesChange) return;
    setOnShelvesChange(() => listenerRef.current?.());
    return () => setOnShelvesChange(null);
  }, [setOnShelvesChange]);
}
