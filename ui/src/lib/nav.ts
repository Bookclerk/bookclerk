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
 * Whether the library Status page is available: to operators, Owners and
 * Administrators, and to a Member who owns a node (a branch they linked).
 * Each sees the nodes they own.
 *
 * @param role - Effective session role.
 * @param administersNodes - The session's `administers_nodes`; when the
 *   server sent none, the role alone decides.
 * @returns True when Status appears in the nav and may be opened.
 */
export function showStatusNav(role?: AuthRole, administersNodes?: boolean): boolean {
  if (administersNodes !== undefined) return administersNodes;
  return role === "operator" || role === "owner" || role === "administrator";
}

/**
 * Primary nav links for an effective role (Settings always included).
 *
 * @param role - Effective session role; Status is added for node administrators.
 * @param administersNodes - The session's `administers_nodes`, if sent.
 * @returns Nav link ids and labels.
 */
export function navLinksForRole(
  role?: AuthRole,
  administersNodes?: boolean,
): { id: AppView; label: string }[] {
  const links = [...BASE_LINKS];
  if (showStatusNav(role, administersNodes)) links.push({ id: "status", label: "Status" });
  return [...links, { id: "settings", label: "Settings" }];
}
