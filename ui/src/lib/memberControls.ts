import type { ControlOverrides, MemberControl, MemberControls } from "@/lib/api";

/** One member control as the editors show it. */
export interface MemberControlInfo {
  /** The control's wire name. */
  control: MemberControl;
  /** Short label. */
  label: string;
  /** What turning it off stops. */
  detail: string;
}

/** Every member control, in the order the editors list them. */
export const MEMBER_CONTROLS: readonly MemberControlInfo[] = [
  {
    control: "link_accounts",
    label: "Link store accounts",
    detail:
      "Connect a store, sign in to one again, or bring connections from another library. Connections already here keep working.",
  },
  {
    control: "download",
    label: "Download",
    detail: "Take copies of the library's files to a branch or a clerk app.",
  },
  {
    control: "edit_metadata",
    label: "Edit metadata",
    detail:
      "Change a book's details. Patrons can't do this here yet; the switch is kept for when they can.",
  },
  {
    control: "upload",
    label: "Upload",
    detail: "Add books by uploading files, folders or archives. Off unless you turn it on.",
  },
  {
    control: "request_books",
    label: "Request books",
    detail: "Ask a branch for a book it listed.",
  },
  {
    control: "see_prices",
    label: "See prices and store links",
    detail: "Prices and store links in Discover and the wishlist.",
  },
];

/** The default of a library that has never set one: everything allowed except upload. */
export const LIBRARY_DEFAULT: MemberControls = {
  link_accounts: true,
  download: true,
  edit_metadata: true,
  upload: false,
  request_books: true,
  see_prices: true,
};

/** A patron's choice for one control: follow the default, or set it. */
export type OverrideChoice = "default" | "on" | "off";

/**
 * The choice an override stands for.
 *
 * @param overrides - The patron's overrides.
 * @param control - The control.
 * @returns `default` when the control is not overridden.
 */
export function overrideChoice(
  overrides: ControlOverrides,
  control: MemberControl,
): OverrideChoice {
  const value = overrides[control];
  if (value === undefined) return "default";
  return value ? "on" : "off";
}
