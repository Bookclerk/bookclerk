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
 * Whether the cluster Status page is available to this role: operators,
 * Owners, Administrators, and Hosts (who see the nodes they own).
 *
 * @param role - Effective session role.
 * @returns True when Status appears in the nav and may be opened.
 */
export function showStatusNav(role?: AuthRole): boolean {
  return (
    role === "operator" || role === "owner" || role === "administrator" || role === "host"
  );
}

/**
 * Primary nav links for an effective role (Settings always included).
 *
 * @param role - Effective session role; Status is added for node administrators.
 * @returns Nav link ids and labels.
 */
export function navLinksForRole(role?: AuthRole): { id: AppView; label: string }[] {
  const links = [...BASE_LINKS];
  if (showStatusNav(role)) links.push({ id: "status", label: "Status" });
  return [...links, { id: "settings", label: "Settings" }];
}
