/**
 * The plugin's settings, read from the `CONFIG` and `SECRETS` bindings.
 *
 * `plugin.toml` declares the settings; the host resolves them from the
 * plugin instance document. Secret settings never reach `CONFIG`: an
 * `api_key` set with `bookclerk plugins config set audiobookshelf api_key
 * --secret` arrives only in `SECRETS`.
 */

/** Integration id written on external users and listening rows. */
export const PROVIDER = "audiobookshelf";

/** What the operator configured, with the manifest defaults filled in. */
export interface Settings {
  /** Audiobookshelf server origin as entered (may end in `/`); empty when unset. */
  readonly baseUrl: string;
  /** API key, from `CONFIG` when set there, else from `SECRETS`; `null` when neither has one. */
  readonly apiKey: string | null;
  /** Library to rescan; `null` when unset or blank. */
  readonly libraryId: string | null;
  /** Whether to report new Audiobookshelf users for claim tickets. */
  readonly watchUsers: boolean;
  /** Whether to ask Audiobookshelf to rescan after an acquire. */
  readonly notifyScanOnAcquire: boolean;
  /** Whether people may sign in with their Audiobookshelf credentials. */
  readonly allowCredentialLogin: boolean;
}

/**
 * A field of a JSON object.
 *
 * @param source - Anything; only a plain object has fields.
 * @param key - Field name.
 * @returns The field's value, or `undefined`.
 */
function field(source: unknown, key: string): unknown {
  if (source === null || typeof source !== "object" || Array.isArray(source)) return undefined;
  return (source as Record<string, unknown>)[key];
}

/**
 * A string field, trimmed; `null` when missing, not a string, or blank.
 *
 * @param source - The binding's JSON.
 * @param key - Field name.
 * @returns The trimmed text or `null`.
 */
function text(source: unknown, key: string): string | null {
  const value = field(source, key);
  if (typeof value !== "string") return null;
  const trimmed = value.trim();
  return trimmed === "" ? null : trimmed;
}

/**
 * A boolean field, or `fallback` when missing or not a boolean.
 *
 * @param source - The binding's JSON.
 * @param key - Field name.
 * @param fallback - The manifest default.
 * @returns The flag.
 */
function flag(source: unknown, key: string, fallback: boolean): boolean {
  const value = field(source, key);
  return typeof value === "boolean" ? value : fallback;
}

/**
 * Reads the settings from the `open` bindings.
 *
 * @param config - The `CONFIG` binding (instance settings).
 * @param secrets - The `SECRETS` binding (unsealed secret settings).
 * @returns The settings; a missing value takes its manifest default.
 */
export function readSettings(config: unknown, secrets: unknown): Settings {
  const baseUrl = field(config, "base_url");
  return {
    baseUrl: typeof baseUrl === "string" ? baseUrl.trim() : "",
    apiKey: text(config, "api_key") ?? text(secrets, "api_key") ?? text(secrets, "apiKey"),
    libraryId: text(config, "library_id"),
    watchUsers: flag(config, "watch_users", true),
    notifyScanOnAcquire: flag(config, "notify_scan_on_acquire", true),
    allowCredentialLogin: flag(config, "allow_credential_login", true),
  };
}
