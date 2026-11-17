/**
 * Listening progress for the host's `listening_progress` table, and the
 * memory that lets a sync send only what changed.
 *
 * With the `LISTENING_STATE` database, each user's progress rows are
 * fingerprinted (progress, position, duration, finished flag and last
 * update). A row whose fingerprint matches what the last sync sent is
 * skipped, along with the library item lookup it would need. Once a day
 * per user every row is sent again, which picks up rows the host could not
 * store earlier (a book the library did not have yet, a person not linked
 * yet) and any write the host lost. Without the database every row is
 * sent.
 *
 * One sync may make at most the requests its budget allows (the host caps
 * each call; see `plugin.toml`). When the budget runs low the sync returns
 * what it collected: the rows it skipped are not remembered as sent, so the
 * next sync sends them.
 */

import type {
  DatabaseBinding,
  ListeningProgress,
  PluginMigration,
  PreparedStatement,
} from "@bookclerk/plugin-sdk/workerd";
import {
  BudgetSpent,
  type AbsClient,
  type AbsItemMetadata,
  type AbsMediaProgress,
} from "./client.ts";
import { FINGERPRINT_TAG, LISTENING_KEY_TAG, fingerprint64, hexKey } from "./hash.ts";
import { int64, int64At, readKeyed, text, textAt, writeAll, type Row } from "./keyed.ts";
import { WATCH_MIGRATION } from "./watch.ts";

/** `[[databases]]` binding the plugin keeps its memory in. */
export const LISTENING_STATE_BINDING = "LISTENING_STATE";

/**
 * How long a user's memory may skip unchanged rows before the next sync
 * sends that user's whole snapshot again, in milliseconds.
 */
export const FULL_RESEND_MS = 24 * 60 * 60 * 1000;

/** How long a user's memory is kept without a sync, in milliseconds. */
export const RETENTION_MS = 7 * 24 * 60 * 60 * 1000;

/**
 * The listening-memory migration.
 *
 * `abs_listening_items` holds the fingerprint of each progress row as last
 * sent (user, library item, fingerprint), and `abs_listening_users` when
 * each user's whole snapshot was last sent and when the user was last
 * synced. The `person` column holds a hash of the user id, never the id.
 */
export const LISTENING_MIGRATION: PluginMigration = {
  id: "abs-listening-v1",
  operations: [
    {
      kind: "schema",
      value:
        "CREATE TABLE IF NOT EXISTS abs_listening_items (person TEXT NOT NULL, item TEXT NOT NULL, fingerprint INTEGER NOT NULL, PRIMARY KEY (person, item))",
    },
    {
      kind: "schema",
      value:
        "CREATE TABLE IF NOT EXISTS abs_listening_users (person TEXT NOT NULL PRIMARY KEY, full_at_ms INTEGER NOT NULL, touched_at_ms INTEGER NOT NULL)",
    },
    {
      kind: "schema",
      value:
        "CREATE INDEX IF NOT EXISTS abs_listening_users_touched ON abs_listening_users (touched_at_ms)",
    },
  ],
};

/**
 * The `databaseMigrations` answer for `binding`.
 *
 * @param binding - Binding name from `plugin.toml`.
 * @returns The listening memory and the user watch for
 *   {@link LISTENING_STATE_BINDING}, nothing for any other binding.
 */
export function migrationsFor(binding: string): PluginMigration[] {
  return binding === LISTENING_STATE_BINDING ? [LISTENING_MIGRATION, WATCH_MIGRATION] : [];
}

/** What one user's rows looked like when they were last sent. */
export interface UserMemory {
  /** When this user's whole snapshot was last sent, UTC Unix milliseconds. */
  readonly fullAtMs: bigint;
  /** Fingerprint of each library item's row as last sent. */
  readonly items: ReadonlyMap<string, bigint>;
}

/** The outcome of one sync. */
export interface ListeningSync {
  /** Rows to hand the host. */
  readonly rows: ListeningProgress[];
  /** `false` when the request budget ran out and rows were left for the next sync. */
  readonly complete: boolean;
}

/**
 * The listening-memory key of a user id.
 *
 * @param userId - Audiobookshelf user id.
 * @returns 16 hex digits.
 */
export function listeningKey(userId: string): Promise<string> {
  return hexKey(LISTENING_KEY_TAG, userId);
}

/**
 * Fingerprint of one progress row: what the host stores from it, keyed to
 * the user and item.
 *
 * @param userId - Audiobookshelf user id.
 * @param row - The progress row.
 * @returns The fingerprint.
 */
export function progressFingerprint(userId: string, row: AbsMediaProgress): Promise<bigint> {
  return fingerprint64(
    FINGERPRINT_TAG,
    JSON.stringify([
      userId,
      row.libraryItemId,
      row.progress,
      row.currentTime,
      row.duration,
      row.isFinished,
      row.lastUpdate,
    ]),
  );
}

/**
 * One host row from a progress row and its item's matching fields.
 *
 * @param userId - Audiobookshelf user id.
 * @param row - The progress row.
 * @param meta - The item's matching fields, when the lookup worked.
 * @returns The row; absent values are omitted, as the wire expects.
 */
export function hostRow(
  userId: string,
  row: AbsMediaProgress,
  meta: AbsItemMetadata | undefined,
): ListeningProgress {
  const out: ListeningProgress = {
    externalUserId: userId,
    externalItemId: row.libraryItemId,
    isFinished: row.isFinished,
  };
  if (meta?.title != null) out.title = meta.title;
  if (meta?.authorName != null) out.authors = meta.authorName;
  if (meta?.asin != null) out.asin = meta.asin.toUpperCase();
  if (meta?.isbn != null) out.isbn = meta.isbn;
  if (row.progress !== null) out.progress = row.progress;
  if (row.currentTime !== null) out.currentTimeSeconds = row.currentTime;
  if (row.duration !== null) out.durationSeconds = row.duration;
  if (row.lastUpdate !== null && row.lastUpdate >= 0 && Number.isSafeInteger(row.lastUpdate)) {
    out.lastListenedAtUnixMs = row.lastUpdate;
  }
  return out;
}

/** One user's rows and the memory to keep for them. */
interface Collected {
  /** Rows to send. */
  readonly rows: ListeningProgress[];
  /** Memory to write for the next sync. */
  readonly next: UserMemory;
  /** `false` when rows were skipped because the budget ran out. */
  readonly complete: boolean;
}

/**
 * One user's rows that differ from `previous`, and the memory to keep.
 *
 * @param client - The Audiobookshelf client.
 * @param userId - User id.
 * @param previous - What the last sync sent for this user, if it still counts.
 * @param nowMs - The time of this sync.
 * @param items - Item lookups already made by this sync, by item id; `undefined` for one that failed.
 * @returns The rows and memory; `undefined` when Audiobookshelf could not
 *   return the user (their memory then stays as it was).
 * @throws {BudgetSpent} When the budget ran out before the user was read.
 */
async function collectUser(
  client: AbsClient,
  userId: string,
  previous: UserMemory | undefined,
  nowMs: bigint,
  items: Map<string, AbsItemMetadata | undefined>,
): Promise<Collected | undefined> {
  let detail;
  try {
    detail = await client.getUser(userId);
  } catch (err) {
    if (err instanceof BudgetSpent) throw err;
    console.warn(`Audiobookshelf could not return a user's progress: ${String(err)}`);
    return undefined;
  }
  const remembered = new Map<string, bigint>();
  const rows: ListeningProgress[] = [];
  let complete = true;
  for (const row of detail.mediaProgress) {
    const fp = await progressFingerprint(detail.id, row);
    if (previous?.items.get(row.libraryItemId) === fp) {
      remembered.set(row.libraryItemId, fp);
      continue;
    }
    let meta: AbsItemMetadata | undefined;
    if (items.has(row.libraryItemId)) {
      meta = items.get(row.libraryItemId);
    } else if (client.budget.left < 1) {
      // Not remembered, so the next sync sends it.
      complete = false;
      continue;
    } else {
      try {
        meta = await client.getItemMetadata(row.libraryItemId);
      } catch (err) {
        if (err instanceof BudgetSpent) {
          complete = false;
          continue;
        }
        console.debug(`Audiobookshelf item lookup failed: ${String(err)}`);
      }
      items.set(row.libraryItemId, meta);
    }
    // A row whose item lookup failed goes out without its identifiers and
    // is not remembered, so the next sync looks the item up again.
    if (meta !== undefined) remembered.set(row.libraryItemId, fp);
    rows.push(hostRow(detail.id, row, meta));
  }
  return { rows, next: { fullAtMs: previous?.fullAtMs ?? nowMs, items: remembered }, complete };
}

/**
 * Drops the memory of every user not synced for {@link RETENTION_MS}.
 *
 * @param db - The plugin database.
 * @param nowMs - The time of this sync.
 * @throws The binding's error.
 */
export async function prune(db: DatabaseBinding, nowMs: bigint): Promise<void> {
  const expired = int64(nowMs - BigInt(RETENTION_MS));
  await writeAll(db, [
    db
      .prepare(
        "DELETE FROM abs_listening_items WHERE person IN (SELECT person FROM abs_listening_users WHERE touched_at_ms < ?)",
      )
      .bind(expired)
      .asRun(),
    db.prepare("DELETE FROM abs_listening_users WHERE touched_at_ms < ?").bind(expired).asRun(),
  ]);
}

/**
 * A user's memory, or `undefined` when there is none or its last whole
 * send is older than {@link FULL_RESEND_MS}.
 *
 * @param db - The plugin database.
 * @param person - The user's listening key.
 * @param nowMs - The time of this sync.
 * @returns The memory.
 * @throws The binding's error, and an `Error` for a row of the wrong shape.
 */
export async function readMemory(
  db: DatabaseBinding,
  person: string,
  nowMs: bigint,
): Promise<UserMemory | undefined> {
  const row = await db
    .prepare("SELECT full_at_ms FROM abs_listening_users WHERE person = ?")
    .bind(text(person))
    .first();
  if (row === null) return undefined;
  const fullAtMs = int64At(row as Row, "full_at_ms");
  if (fullAtMs === undefined) throw new Error("listening user row has no full_at_ms");
  if (nowMs - fullAtMs >= BigInt(FULL_RESEND_MS)) return undefined;
  const rows = await readKeyed(
    db,
    "SELECT item, fingerprint FROM abs_listening_items WHERE person = ? AND item > ? ORDER BY item LIMIT ?",
    [text(person)],
    "item",
  );
  const items = new Map<string, bigint>();
  for (const r of rows) {
    const item = textAt(r, "item");
    const fp = int64At(r, "fingerprint");
    if (item === undefined || fp === undefined) {
      throw new Error("listening item row has the wrong shape");
    }
    items.set(item, fp);
  }
  return { fullAtMs, items };
}

/**
 * Records what this sync sent for one user.
 *
 * After a whole send (`previous` is `undefined`) the user's rows are
 * replaced and the whole-send time moves to `nowMs`; after a partial one
 * only the rows that changed are written. The user row is written last, so
 * a sync cut short leaves either the old whole-send time or none, and a row
 * the memory holds is never newer than what was sent.
 *
 * @param db - The plugin database.
 * @param person - The user's listening key.
 * @param previous - The memory the sync started from.
 * @param next - The memory to keep.
 * @param nowMs - The time of this sync.
 * @throws The binding's error.
 */
export async function writeMemory(
  db: DatabaseBinding,
  person: string,
  previous: UserMemory | undefined,
  next: UserMemory,
  nowMs: bigint,
): Promise<void> {
  const statements: PreparedStatement[] = [];
  if (previous === undefined) {
    statements.push(
      db.prepare("DELETE FROM abs_listening_items WHERE person = ?").bind(text(person)).asRun(),
    );
  }
  const changed = [...next.items]
    .filter(([item, fp]) => previous === undefined || previous.items.get(item) !== fp)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  for (const [item, fp] of changed) {
    if (previous !== undefined) {
      statements.push(
        db
          .prepare("UPDATE abs_listening_items SET fingerprint = ? WHERE person = ? AND item = ?")
          .bind(int64(fp), text(person), text(item))
          .asRun(),
      );
    }
    statements.push(
      db
        .prepare(
          "INSERT OR IGNORE INTO abs_listening_items (person, item, fingerprint) VALUES (?, ?, ?)",
        )
        .bind(text(person), text(item), int64(fp))
        .asRun(),
    );
  }
  statements.push(
    db
      .prepare("UPDATE abs_listening_users SET full_at_ms = ?, touched_at_ms = ? WHERE person = ?")
      .bind(int64(next.fullAtMs), int64(nowMs), text(person))
      .asRun(),
    db
      .prepare(
        "INSERT OR IGNORE INTO abs_listening_users (person, full_at_ms, touched_at_ms) VALUES (?, ?, ?)",
      )
      .bind(text(person), int64(next.fullAtMs), int64(nowMs))
      .asRun(),
  );
  await writeAll(db, statements);
}

/**
 * Collects the listening-progress rows that changed since the last sync.
 *
 * The memory is written only after every user has been collected, so a
 * sync that fails part way records nothing it did not return. A memory
 * call that fails never fails the sync: a read that fails sends that
 * user's whole snapshot, and a write that fails leaves the memory holding
 * at most what this sync returns. Memory of users not synced for seven
 * days is dropped.
 *
 * @param client - The Audiobookshelf client, with this call's request budget.
 * @param memory - The plugin's `LISTENING_STATE` database, when granted.
 * @param nowMs - The time of this sync, UTC Unix milliseconds.
 * @returns The rows and whether the sync covered every user and row.
 * @throws {AbsFailure} When Audiobookshelf cannot list its users.
 */
export async function collectListeningChanges(
  client: AbsClient,
  memory: DatabaseBinding | undefined,
  nowMs: number,
): Promise<ListeningSync> {
  const now = BigInt(Math.trunc(nowMs));
  if (memory !== undefined) {
    try {
      await prune(memory, now);
    } catch (err) {
      console.warn(`Audiobookshelf listening memory prune failed: ${String(err)}`);
    }
  }
  const users = await client.listUsers();
  const rows: ListeningProgress[] = [];
  const remembered: Array<[string, UserMemory | undefined, UserMemory]> = [];
  const items = new Map<string, AbsItemMetadata | undefined>();
  let complete = true;
  for (const user of users) {
    const person = await listeningKey(user.id);
    let previous: UserMemory | undefined;
    if (memory !== undefined) {
      try {
        previous = await readMemory(memory, person, now);
      } catch (err) {
        console.warn(`Audiobookshelf listening memory read failed: ${String(err)}`);
      }
    }
    let collected: Collected | undefined;
    try {
      collected = await collectUser(client, user.id, previous, now, items);
    } catch (err) {
      if (!(err instanceof BudgetSpent)) throw err;
      complete = false;
      break;
    }
    if (collected === undefined) continue;
    rows.push(...collected.rows);
    remembered.push([person, previous, collected.next]);
    if (!collected.complete) {
      complete = false;
      break;
    }
  }
  if (memory !== undefined) {
    for (const [person, previous, next] of remembered) {
      try {
        await writeMemory(memory, person, previous, next, now);
      } catch (err) {
        console.warn(`Audiobookshelf listening memory write failed: ${String(err)}`);
      }
    }
  }
  return { rows, complete };
}
