/**
 * Which Audiobookshelf users the host has already been told about.
 *
 * The host asks for new users (`pollEvents`) on its own schedule, and the
 * plugin may sleep between asks. So the set of users already reported is
 * kept in the plugin's `LISTENING_STATE` database, as a hash of each user
 * id, and a plugin started later compares the server's user list against
 * it. Without the database the set lives in the isolate's memory and is
 * seeded again whenever the plugin starts, so a user created while it slept
 * is not reported.
 */

import type { DatabaseBinding, PluginMigration } from "@bookclerk/plugin-sdk/workerd";
import type { AbsUser } from "./client.ts";
import { WATCH_KEY_TAG, hexKey } from "./hash.ts";
import { readKeyed, text, textAt, writeAll } from "./keyed.ts";

/**
 * The `person` row written after a complete seed. It sorts before every key
 * (keys are lowercase hex), and a set without it has not been seeded: a
 * seed cut short is redone in full, so no user the server already had is
 * ever reported.
 */
export const SEEDED = "!seeded";

/**
 * The migration that adds the user-watch table.
 *
 * `abs_watch_users` holds one row per Audiobookshelf user already reported
 * or seeded, plus the {@link SEEDED} marker once the first seed completed;
 * its `person` column is a hash of the user id, never the id.
 */
export const WATCH_MIGRATION: PluginMigration = {
  id: "abs-user-watch-v1",
  operations: [
    {
      kind: "schema",
      value: "CREATE TABLE IF NOT EXISTS abs_watch_users (person TEXT NOT NULL PRIMARY KEY)",
    },
  ],
};

/**
 * The watch key of an Audiobookshelf user id.
 *
 * @param userId - User id.
 * @returns 16 hex digits; the database never holds the id.
 */
export function watchKey(userId: string): Promise<string> {
  return hexKey(WATCH_KEY_TAG, userId);
}

/** What the watch table holds. */
interface Known {
  /** Every user key. */
  readonly keys: Set<string>;
  /** Whether the first seed completed. */
  readonly seeded: boolean;
}

/**
 * Every user key the database holds, and whether it was seeded.
 *
 * @param db - The plugin database.
 * @returns The keys and the seed flag.
 * @throws The binding's error, and an `Error` for a row of the wrong shape.
 */
async function known(db: DatabaseBinding): Promise<Known> {
  const rows = await readKeyed(
    db,
    "SELECT person FROM abs_watch_users WHERE person > ? ORDER BY person LIMIT ?",
    [],
    "person",
  );
  const keys = new Set<string>();
  for (const row of rows) {
    const person = textAt(row, "person");
    if (person === undefined) throw new Error("watch user row has the wrong shape");
    keys.add(person);
  }
  const seeded = keys.delete(SEEDED);
  return { keys, seeded };
}

/**
 * Records `keys` as known, in batches that each fit one request.
 *
 * @param db - The plugin database.
 * @param keys - User keys (and possibly the marker).
 * @throws The binding's error.
 */
export async function insertKeys(db: DatabaseBinding, keys: readonly string[]): Promise<void> {
  await writeAll(
    db,
    keys.map((person) =>
      db
        .prepare("INSERT OR IGNORE INTO abs_watch_users (person) VALUES (?)")
        .bind(text(person))
        .asRun(),
    ),
  );
}

/** Where the reported-user set lives for one call. */
export class UserWatch {
  /** The plugin database, when the operator granted it. */
  readonly #db: DatabaseBinding | undefined;
  /** The in-memory set, used only without the database. */
  readonly #memory: Set<string>;

  /**
   * A watch over `db` when granted, else over `memory`.
   *
   * @param db - The plugin's `LISTENING_STATE` database, when granted.
   * @param memory - The isolate's in-memory set of user keys.
   */
  constructor(db: DatabaseBinding | undefined, memory: Set<string>) {
    this.#db = db;
    this.#memory = memory;
  }

  /**
   * Records every user in `users` as known when the set was never seeded,
   * so the users a server already has are never reported.
   *
   * With the database, the seed is complete only once its marker row is
   * written after every user row; a seed that fails part way leaves no
   * marker, and the next seed records every user again.
   *
   * @param users - The server's users.
   * @returns How many users were recorded (0 when already seeded).
   * @throws The database's error.
   */
  async seed(users: readonly AbsUser[]): Promise<number> {
    const db = this.#db;
    if (db === undefined) {
      if (this.#memory.size > 0) return 0;
      for (const user of users) this.#memory.add(await watchKey(user.id));
      return this.#memory.size;
    }
    if ((await known(db)).seeded) return 0;
    const keys = await Promise.all(users.map((user) => watchKey(user.id)));
    const count = keys.length;
    keys.push(SEEDED);
    await insertKeys(db, keys);
    return count;
  }

  /**
   * The users in `users` not reported before, now recorded as known.
   *
   * A failed write does not lose users: when recording them fails part way
   * the users are still returned, and those whose rows were not written are
   * reported again by the next poll (the host then mints a second claim
   * ticket for the same person rather than none). A reply lost after the
   * rows were written is not retried, because `pollEvents` has no
   * acknowledgement.
   *
   * @param users - The server's users.
   * @returns The new users, in the server's order.
   * @throws The database's error when the known set cannot be read.
   */
  async takeNew(users: readonly AbsUser[]): Promise<AbsUser[]> {
    const db = this.#db;
    const fresh: AbsUser[] = [];
    if (db === undefined) {
      for (const user of users) {
        const key = await watchKey(user.id);
        if (!this.#memory.has(key)) {
          this.#memory.add(key);
          fresh.push(user);
        }
      }
      return fresh;
    }
    const seen = (await known(db)).keys;
    const keys: string[] = [];
    for (const user of users) {
      const key = await watchKey(user.id);
      if (!seen.has(key)) {
        seen.add(key);
        keys.push(key);
        fresh.push(user);
      }
    }
    try {
      await insertKeys(db, keys);
    } catch (err) {
      console.warn(
        `Audiobookshelf user watch could not record new users; the next poll reports them again: ${String(err)}`,
      );
    }
    return fresh;
  }
}
