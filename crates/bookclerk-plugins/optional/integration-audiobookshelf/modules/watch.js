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
 *
 * A host that acknowledges polls (`ackFollows`) gets each new user again on
 * every poll until it acknowledges that user (`ackEvents`): the user is held
 * in `abs_watch_pending` (or the isolate's memory) between the two, so a
 * reply lost on the way to the host does not lose the user.
 */
import { WATCH_KEY_TAG, hexKey } from "./hash.js";
import { readKeyed, text, textAt, writeAll } from "./keyed.js";
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
export const WATCH_MIGRATION = {
    id: "abs-user-watch-v1",
    operations: [
        {
            kind: "schema",
            value: "CREATE TABLE IF NOT EXISTS abs_watch_users (person TEXT NOT NULL PRIMARY KEY)",
        },
    ],
};
/**
 * The migration that adds the table of users reported but not yet
 * acknowledged.
 *
 * `abs_watch_pending` holds one row per user handed to a host that said it
 * acknowledges polls, until the host acknowledges that user; like
 * `abs_watch_users`, its `person` column is a hash of the user id.
 */
export const WATCH_PENDING_MIGRATION = {
    id: "abs-user-watch-pending-v1",
    operations: [
        {
            kind: "schema",
            value: "CREATE TABLE IF NOT EXISTS abs_watch_pending (person TEXT NOT NULL PRIMARY KEY)",
        },
    ],
};
/**
 * The watch key of an Audiobookshelf user id.
 *
 * @param userId - User id.
 * @returns 16 hex digits; the database never holds the id.
 */
export function watchKey(userId) {
    return hexKey(WATCH_KEY_TAG, userId);
}
/**
 * Every user key the database holds, and whether it was seeded.
 *
 * @param db - The plugin database.
 * @returns The keys and the seed flag.
 * @throws The binding's error, and an `Error` for a row of the wrong shape.
 */
async function known(db) {
    const keys = await keysIn(db, "abs_watch_users");
    const seeded = keys.delete(SEEDED);
    return { keys, seeded };
}
/**
 * Every `person` key in one of the watch tables.
 *
 * @param db - The plugin database.
 * @param table - `abs_watch_users` or `abs_watch_pending`.
 * @returns The keys.
 * @throws The binding's error, and an `Error` for a row of the wrong shape.
 */
async function keysIn(db, table) {
    const rows = await readKeyed(db, `SELECT person FROM ${table} WHERE person > ? ORDER BY person LIMIT ?`, [], "person");
    const keys = new Set();
    for (const row of rows) {
        const person = textAt(row, "person");
        if (person === undefined)
            throw new Error("watch user row has the wrong shape");
        keys.add(person);
    }
    return keys;
}
/**
 * Records `keys` as known, in batches that each fit one request.
 *
 * @param db - The plugin database.
 * @param keys - User keys (and possibly the marker).
 * @throws The binding's error.
 */
export async function insertKeys(db, keys) {
    await writeAll(db, keys.map((person) => db
        .prepare("INSERT OR IGNORE INTO abs_watch_users (person) VALUES (?)")
        .bind(text(person))
        .asRun()));
}
/**
 * Records `keys` as reported but not yet acknowledged.
 *
 * @param db - The plugin database.
 * @param keys - User keys.
 * @throws The binding's error.
 */
async function insertPending(db, keys) {
    await writeAll(db, keys.map((person) => db
        .prepare("INSERT OR IGNORE INTO abs_watch_pending (person) VALUES (?)")
        .bind(text(person))
        .asRun()));
}
/** Where the reported-user set lives for one call. */
export class UserWatch {
    /** The plugin database, when the operator granted it. */
    #db;
    /** The in-memory set, used only without the database. */
    #memory;
    /** The in-memory set of unacknowledged user keys, used only without the database. */
    #pending;
    /**
     * A watch over `db` when granted, else over `memory` and `pending`.
     *
     * @param db - The plugin's `LISTENING_STATE` database, when granted.
     * @param memory - The isolate's in-memory set of user keys, plus
     *   {@link SEEDED} once seeded.
     * @param pending - The isolate's in-memory set of keys of users reported
     *   to a host that acknowledges polls and not yet acknowledged.
     */
    constructor(db, memory, pending = new Set()) {
        this.#db = db;
        this.#memory = memory;
        this.#pending = pending;
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
    async seed(users) {
        const db = this.#db;
        if (db === undefined) {
            // The marker also records a seed of a server with no users yet.
            if (this.#memory.has(SEEDED))
                return 0;
            const keys = await Promise.all(users.map((user) => watchKey(user.id)));
            for (const key of keys)
                this.#memory.add(key);
            this.#memory.add(SEEDED);
            return keys.length;
        }
        if ((await known(db)).seeded)
            return 0;
        const keys = await Promise.all(users.map((user) => watchKey(user.id)));
        const count = keys.length;
        keys.push(SEEDED);
        await insertKeys(db, keys);
        return count;
    }
    /**
     * The users in `users` not reported before, now recorded as known, plus
     * (when `ackFollows`) those reported before but not yet acknowledged.
     *
     * When `ackFollows`, every new user is recorded as pending before it is
     * recorded as known, and stays pending until {@link ack}; a reply lost on
     * the way to the host is answered again by the next poll. A pending user
     * no longer on the server is not reported again.
     *
     * Without `ackFollows` (an older host) there is no acknowledgement: when
     * recording users fails part way they are still returned, and those whose
     * rows were not written are reported again by the next poll (the host
     * then mints a second claim ticket for the same person rather than none),
     * but a reply lost after the rows were written is not retried.
     *
     * @param users - The server's users.
     * @param ackFollows - Whether the host acknowledges what it receives.
     * @returns The users to report, in the server's order.
     * @throws The database's error when the known set cannot be read, or
     *   (when `ackFollows`) when the new users cannot be recorded as pending.
     */
    async takeNew(users, ackFollows = false) {
        const db = this.#db;
        const fresh = [];
        if (db === undefined) {
            for (const user of users) {
                const key = await watchKey(user.id);
                if (!this.#memory.has(key)) {
                    this.#memory.add(key);
                    if (ackFollows)
                        this.#pending.add(key);
                    fresh.push(user);
                }
                else if (ackFollows && this.#pending.has(key)) {
                    fresh.push(user);
                }
            }
            return fresh;
        }
        const seen = (await known(db)).keys;
        const pending = ackFollows ? await keysIn(db, "abs_watch_pending") : new Set();
        const keys = [];
        for (const user of users) {
            const key = await watchKey(user.id);
            if (!seen.has(key)) {
                seen.add(key);
                keys.push(key);
                fresh.push(user);
            }
            else if (pending.has(key)) {
                fresh.push(user);
            }
        }
        // Pending first: a user written as known but not as pending would never
        // be reported again.
        if (ackFollows)
            await insertPending(db, keys);
        try {
            await insertKeys(db, keys);
        }
        catch (err) {
            console.warn(`Audiobookshelf user watch could not record new users; the next poll reports them again: ${String(err)}`);
        }
        return fresh;
    }
    /**
     * Marks the users in `userIds` acknowledged, so polls stop reporting them.
     *
     * Each user is recorded as known before its pending row is deleted: a
     * poll whose known write failed still returned the user, and deleting the
     * pending row alone would leave it unknown, so the next poll would report
     * it as new and the host would mint a second claim ticket. When the known
     * write fails, the users stay pending.
     *
     * @param userIds - Audiobookshelf user ids the host acknowledged; ids that
     *   were never pending are ignored.
     * @throws The database's error.
     */
    async ack(userIds) {
        const keys = await Promise.all(userIds.map((id) => watchKey(id)));
        const db = this.#db;
        if (db === undefined) {
            for (const key of keys)
                this.#pending.delete(key);
            return;
        }
        await insertKeys(db, keys);
        await writeAll(db, keys.map((person) => db.prepare("DELETE FROM abs_watch_pending WHERE person = ?").bind(text(person)).asRun()));
    }
}
