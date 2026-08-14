import { createContext } from "react";

export type ShelvesListener = () => void;

export type PreferencesContextValue = {
  openPreferences: () => void;
  preferencesOpen: boolean;
};

export const PreferencesContext = createContext<PreferencesContextValue | null>(null);

export const ShelvesChangeRegistrationContext = createContext<
  ((fn: ShelvesListener | null) => void) | null
>(null);
