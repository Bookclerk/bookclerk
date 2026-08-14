import type { AppView, AuthRole } from "@/lib/api";

const BASE_LINKS: { id: AppView; label: string }[] = [
  { id: "discover", label: "Discover" },
  { id: "wishlist", label: "Wishlist" },
  { id: "library", label: "Library" },
  { id: "accounts", label: "Accounts" },
];

/**
 * Whether Settings appears in primary nav for this role.
 *
 * All signed-in roles get Settings (Account with Profile / Security / Sessions).
 * User Management / Server / Plugins tabs are gated inside the page by role.
 *
 * @param _role - Effective session role (unused; Settings is always shown).
 * @returns Always `true` for signed-in chrome.
 */
export function showSettingsNav(_role?: AuthRole): boolean {
  return true;
}

/**
 * Primary nav links for an effective role (Settings always included).
 *
 * @param _role - Effective session role (unused; Settings is always shown).
 * @returns Nav link ids and labels.
 */
export function navLinksForRole(
  _role?: AuthRole,
): { id: AppView; label: string }[] {
  return [...BASE_LINKS, { id: "settings", label: "Settings" }];
}
