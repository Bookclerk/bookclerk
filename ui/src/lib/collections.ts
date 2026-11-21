import type { LibraryCollection } from "@/lib/api";

/** Most collections a library may define (the server's bound). */
export const MAX_COLLECTIONS = 32;

/** Most prefixes one collection may have (the server's bound). */
export const MAX_COLLECTION_PREFIXES = 8;

/** One collection as the editor holds it while someone types. */
export interface CollectionDraft {
  /** The name as typed. */
  name: string;
  /** The prefixes as typed, comma-separated. */
  prefixes: string;
}

/**
 * The editor's rows for saved collections.
 *
 * @param collections - The collections as the server returned them.
 * @returns One draft row per collection.
 */
export function draftsOf(collections: readonly LibraryCollection[]): CollectionDraft[] {
  return collections.map((c) => ({ name: c.name, prefixes: c.prefixes.join(", ") }));
}

/**
 * The collections to send for the editor's rows: names trimmed, prefixes
 * split on commas and trimmed, rows with neither dropped.
 *
 * @param drafts - The editor's rows.
 * @returns The collections; the server adds closing slashes.
 */
export function collectionsOf(drafts: readonly CollectionDraft[]): LibraryCollection[] {
  return drafts
    .map((d) => ({
      name: d.name.trim(),
      prefixes: d.prefixes
        .split(",")
        .map((p) => p.trim())
        .filter((p) => p.length > 0),
    }))
    .filter((c) => c.name.length > 0 || c.prefixes.length > 0);
}

/**
 * Why `collections` can't be saved, or `null` when they can (the server
 * checks the byte bounds).
 *
 * @param collections - From {@link collectionsOf}.
 * @returns A message for the person.
 */
export function collectionsProblem(collections: readonly LibraryCollection[]): string | null {
  if (collections.length > MAX_COLLECTIONS) {
    return `A library has at most ${MAX_COLLECTIONS} collections.`;
  }
  const names = new Set<string>();
  for (const c of collections) {
    if (!c.name) return "Every collection needs a name.";
    const key = c.name.toLowerCase();
    if (names.has(key)) return `Two collections are named ${c.name}.`;
    names.add(key);
    if (c.prefixes.length === 0) return `${c.name} needs at least one folder.`;
    if (c.prefixes.length > MAX_COLLECTION_PREFIXES) {
      return `${c.name} has more than ${MAX_COLLECTION_PREFIXES} folders.`;
    }
    if (c.prefixes.some((p) => p.startsWith("/"))) {
      return "Folders are relative to the storage root and don't start with /.";
    }
  }
  return null;
}
