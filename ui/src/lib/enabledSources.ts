import { portalSources, type PortalSource } from "@/lib/api";

/**
 * Loads enabled storefront sources from the portal API.
 *
 * Used by Discover / Preferences so store lists track `[sources.<id>].enabled`
 * instead of a hardcoded product catalog.
 *
 * @returns Enabled sources (may be empty when none are enabled).
 */
export async function loadEnabledSources(): Promise<PortalSource[]> {
  try {
    return await portalSources();
  } catch {
    return [];
  }
}

/**
 * Placeholder for the Discover search field when store names are unknown.
 */
export const DISCOVER_SEARCH_PLACEHOLDER =
  "Search Book Stores... (Enter for results)";
