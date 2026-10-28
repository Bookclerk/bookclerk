/**
 * The ingest Action's pull cursor, kept in the collector's D1 database.
 *
 * `GET /report` returns stored objects uploaded after a cursor: the upload
 * time `since` (Unix ms) and, for objects uploaded in that same millisecond,
 * the last object name `after` already pulled. The Action used to keep the
 * cursor in its own cache, which GitHub evicts when the repository's cache
 * fills, and a lost cursor re-pulled (and re-filed) every stored report. The
 * collector now keeps it: `GET /report?cursor=stored` starts from the stored
 * cursor and `POST /report/cursor` stores the next one once a pull succeeds.
 *
 * Both routes need the report key, so the body comes from the Action; it is
 * still parsed strictly (`parseCursor`, fuzzed in
 * `ingest-cursor.fuzz.test.ts`), and a stored cursor only moves forward.
 */

import { first } from "./dashboard/db.ts";
import { isObject, isSafeInteger } from "./json.ts";

/** `sync_state` key holding the cursor as `{"since":…,"after":…}`. */
export const CURSOR_KEY = "ingest_cursor";

/** Longest cursor body accepted, in characters. */
export const MAX_CURSOR_CHARS = 1100;

/** Longest `after` object name kept, in characters (B2 allows 1024 bytes). */
export const MAX_AFTER_CHARS = 1024;

/** Position in the upload order of stored objects. */
export type Cursor = {
  /** Upload time of the last object read, in milliseconds. */
  since: number;
  /** Name of the last object read at `since`, "" before any. */
  after: string;
};

/** The cursor before every object. */
export const START = Object.freeze({ since: 0, after: "" });

/**
 * Parses a cursor body.
 *
 * @param text - Request body or stored value.
 * @returns The cursor: `since` a safe integer ≥ 0 and
 *   `after` a string of at most `MAX_AFTER_CHARS` characters with no control
 *   characters (absent means ""); null for anything else.
 */
export function parseCursor(text: unknown): Cursor | null {
  if (typeof text !== "string" || text.length > MAX_CURSOR_CHARS) return null;
  let data: unknown;
  try {
    data = JSON.parse(text);
  } catch {
    return null;
  }
  if (!isObject(data)) return null;
  const since = data.since;
  if (!isSafeInteger(since) || since < 0) return null;
  const after = data.after ?? "";
  if (typeof after !== "string" || after.length > MAX_AFTER_CHARS) return null;
  if (/[\u0000-\u001f\u007f]/.test(after)) return null;
  return { since, after };
}

/**
 * Whether cursor `a` comes before cursor `b` in upload order.
 *
 * @param a - First cursor.
 * @param b - Second cursor.
 * @returns True when `a` is strictly earlier.
 */
export function cursorBefore(a: Cursor, b: Cursor): boolean {
  return a.since < b.since || (a.since === b.since && a.after < b.after);
}

/**
 * Reads the stored cursor.
 *
 * @param db - Database.
 * @returns The stored cursor, or `START` when none is
 *   stored or the stored value does not parse.
 */
export async function readCursor(db: D1Database): Promise<Cursor> {
  const row = await first(db, "SELECT value FROM sync_state WHERE key = ?", CURSOR_KEY);
  return (row && parseCursor(String(row.value))) || START;
}

/**
 * Stores `next` when it is later than the stored cursor. The ingest workflow
 * runs one job at a time, so a read then a write cannot interleave with
 * another commit.
 *
 * @param db - Database.
 * @param next - Cursor the last pull ended at.
 * @returns Whether it moved,
 *   and the cursor now stored.
 */
export async function commitCursor(
  db: D1Database,
  next: Cursor,
): Promise<{ advanced: boolean; cursor: Cursor }> {
  const stored = await readCursor(db);
  if (!cursorBefore(stored, next)) return { advanced: false, cursor: stored };
  await db
    .prepare(
      "INSERT INTO sync_state (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )
    .bind(CURSOR_KEY, JSON.stringify({ since: next.since, after: next.after }))
    .run();
  return { advanced: true, cursor: next };
}
