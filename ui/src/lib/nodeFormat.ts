/**
 * Formatting shared by the node views (Settings → Nodes and Status).
 */

/**
 * Millicores as cores: `1536` → `1.5`.
 *
 * @param millicores - CPU in millicores.
 * @returns Cores with at most one decimal.
 */
export function cores(millicores: number): string {
  return (millicores / 1000).toFixed(millicores % 1000 === 0 ? 0 : 1);
}

/**
 * Bytes as `3.2 GiB` / `512 MiB`; `?` when unknown.
 *
 * @param bytes - Byte count, when known.
 * @returns Human-readable size.
 */
export function size(bytes: number | null | undefined): string {
  if (bytes == null) return "?";
  const gib = bytes / 2 ** 30;
  return gib >= 1 ? `${gib.toFixed(1)} GiB` : `${Math.round(bytes / 2 ** 20)} MiB`;
}

/**
 * Time since an RFC 3339 instant: `12 s ago` / `3 min ago` / `2 h ago`.
 *
 * @param iso - RFC 3339 timestamp.
 * @returns Relative age.
 */
export function age(iso: string): string {
  const seconds = Math.max(0, Math.round((Date.now() - Date.parse(iso)) / 1000));
  if (!Number.isFinite(seconds)) return "unknown";
  if (seconds < 90) return `${seconds} s ago`;
  if (seconds < 90 * 60) return `${Math.round(seconds / 60)} min ago`;
  if (seconds < 48 * 3600) return `${Math.round(seconds / 3600)} h ago`;
  return `${Math.round(seconds / 86400)} d ago`;
}

/**
 * A node owner's name for display.
 *
 * @param owner - Owner as the node APIs return it.
 * @returns Display name, or `user <id>`.
 */
export function ownerName(owner: { user_id: number; display_name: string | null }): string {
  return owner.display_name?.trim() || `user ${owner.user_id}`;
}
