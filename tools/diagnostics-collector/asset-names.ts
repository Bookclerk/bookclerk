/**
 * Names of the dashboard's hashed styles and script.
 *
 * The files live in public/static/assets/ as `<stem>.<hash>.<ext>`, where
 * the hash is the first 12 hex digits of the SHA-256 of their contents, so
 * public/_headers lets browsers keep them for a year: a change is a new
 * URL. There is no build step, so the name is part of the commit:
 * `npm run hash-assets` renames an edited file and rewrites
 * dashboard/asset-urls.ts, and worker.test.ts fails while a name and its
 * contents disagree.
 */

/** The hashed files, by their name without the hash. */
export const HASHED_ASSETS = ["dashboard.css", "dashboard.js"] as const;

/** One of {@link HASHED_ASSETS}. */
export type HashedAsset = (typeof HASHED_ASSETS)[number];

/** URL path of the folder the hashed files are served from. */
export const ASSETS_PATH = "/static/assets/";

/** Hex digits of the content hash kept in a name. */
const HASH_DIGITS = 12;

/**
 * The content hash a hashed file's name carries.
 *
 * @param bytes - The file's contents.
 * @returns The first 12 hex digits of their SHA-256.
 */
export async function contentHash(bytes: Uint8Array<ArrayBuffer>): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  return Array.from(digest, (b) => b.toString(16).padStart(2, "0"))
    .join("")
    .slice(0, HASH_DIGITS);
}

/**
 * The published name of a hashed file.
 *
 * @param file - The file's name without the hash, such as `dashboard.css`.
 * @param hash - Its content hash from {@link contentHash}.
 * @returns `<stem>.<hash>.<ext>`, such as `dashboard.0123456789ab.css`.
 */
export function hashedName(file: HashedAsset, hash: string): string {
  const dot = file.lastIndexOf(".");
  return `${file.slice(0, dot)}.${hash}${file.slice(dot)}`;
}

/**
 * Finds which hashed file a name in public/static/assets/ is.
 *
 * @param name - A file name, such as `dashboard.0123456789ab.css`.
 * @returns The file without the hash, or null when the name is not a hashed one.
 */
export function assetOf(name: string): HashedAsset | null {
  const match = /^(.+)\.[0-9a-f]{12}(\.[a-z]+)$/.exec(name);
  if (match === null) return null;
  const file = `${match[1]}${match[2]}`;
  return HASHED_ASSETS.find((asset) => asset === file) ?? null;
}

/**
 * The source of dashboard/asset-urls.ts for the given published names.
 *
 * @param names - Each hashed file's published name.
 * @returns The module's text.
 */
export function assetUrlsModule(names: Readonly<Record<HashedAsset, string>>): string {
  const lines = HASHED_ASSETS.map((file) => `  "${file}": "${ASSETS_PATH}${names[file]}",`);
  return [
    "// Written by `npm run hash-assets` (scripts/hash-assets.ts); do not edit.",
    "",
    "/** URL of each hashed dashboard file (see asset-names.ts). */",
    "export const ASSET_URLS = {",
    ...lines,
    "} as const;",
    "",
  ].join("\n");
}
