import type { AdminControl, AdminControlOverrides, AdminControls } from "@/lib/api";

/** One Administrator switch as the editor shows it. */
export interface AdminControlInfo {
  /** The switch's wire name. */
  control: AdminControl;
  /** Short label. */
  label: string;
  /** What turning it off stops. */
  detail: string;
}

/** Every Administrator switch, in the order the editor lists them. */
export const ADMIN_CONTROLS: readonly AdminControlInfo[] = [
  {
    control: "manage_members",
    label: "Manage members",
    detail: "Invite, add, change, reset the password of, or remove Members.",
  },
  {
    control: "set_patron_limits",
    label: "Set patron limits",
    detail: "Change what Members may see and do, and the library's defaults for them.",
  },
  {
    control: "library_scan",
    label: "Scan and acquire library-wide",
    detail: "Scan or acquire with every store account. Off, they reach only their own accounts.",
  },
  {
    control: "run_jobs",
    label: "Run the job queue",
    detail: "Cancel jobs, retry or resume event deliveries, and start listening sync.",
  },
  {
    control: "fix_catalog",
    label: "Fix the catalog",
    detail: "Pin, unpin and split catalog matches. Reading the conflicts list stays open.",
  },
  {
    control: "invite_branches",
    label: "Invite branches",
    detail: "Create invites that let a new branch join this library.",
  },
  {
    control: "approve_requests",
    label: "Approve interlibrary requests",
    detail:
      "Answer requests from other libraries. Nothing asks for approval yet; the switch is kept for when it does.",
  },
];

/** The default of a library that has never set one: everything allowed. */
export const ALL_ADMIN_ALLOWED: AdminControls = {
  manage_members: true,
  set_patron_limits: true,
  library_scan: true,
  run_jobs: true,
  fix_catalog: true,
  invite_branches: true,
  approve_requests: true,
};

/** An Administrator's choice for one switch: follow the default, or set it. */
export type AdminOverrideChoice = "default" | "on" | "off";

/**
 * The choice an override stands for.
 *
 * @param overrides - The Administrator's overrides.
 * @param control - The switch.
 * @returns `default` when the switch is not overridden.
 */
export function adminOverrideChoice(
  overrides: AdminControlOverrides,
  control: AdminControl,
): AdminOverrideChoice {
  const value = overrides[control];
  if (value === undefined) return "default";
  return value ? "on" : "off";
}

/**
 * The overrides with one switch set to `choice`.
 *
 * @param overrides - The Administrator's overrides.
 * @param control - The switch to change.
 * @param choice - Follow the default, or set it on or off.
 * @returns New overrides; `overrides` is not changed.
 */
export function withAdminOverride(
  overrides: AdminControlOverrides,
  control: AdminControl,
  choice: AdminOverrideChoice,
): AdminControlOverrides {
  const next: AdminControlOverrides = { ...overrides };
  if (choice === "default") delete next[control];
  else next[control] = choice === "on";
  return next;
}
