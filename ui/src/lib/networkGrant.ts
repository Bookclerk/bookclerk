/**
 * Wording for plugin network grants shown on the consent screens.
 */

/** The `domains` entry that grants every public hostname. */
export const ANY_PUBLIC_HOST = "*";

/**
 * How one fetch-domain entry reads to an operator.
 *
 * @param domain - A `capabilities.network.domains` entry as stored.
 * @returns `Any public website` for `*`, otherwise the entry unchanged.
 */
export function domainLabel(domain: string): string {
  return domain.trim() === ANY_PUBLIC_HOST ? "Any public website" : domain;
}

/**
 * Short summary of a list of fetch domains.
 *
 * @param domains - Selected `domains` entries.
 * @returns `any public website` when `*` is among them, otherwise a host count
 *   such as `2 fetch hosts`, or `no fetch hosts` when empty.
 */
export function fetchHostsSummary(domains: readonly string[]): string {
  if (domains.some((d) => d.trim() === ANY_PUBLIC_HOST)) return "any public website";
  if (domains.length === 0) return "no fetch hosts";
  return `${domains.length} fetch host${domains.length === 1 ? "" : "s"}`;
}
