/**
 * A small `notes` table on the plugin's own `[[databases]]` binding, written
 * the way plugin database code should be: sized from `limits()`, split with
 * `chunk()`, read with `iterate()` when the session streams, and paged by key
 * when it does not. Typed `limit_exceeded` errors are handled by the cap
 * they name, never by matching the message.
 *
 * Imports are type-only, so the module runs unchanged inside the isolate and
 * under Node, where `test/notes.test.ts` drives it through
 * `createTestDatabase()` under every capability profile.
 *
 * See docs/plugins/database-limits.md.
 */
/** The `[[databases]]` binding name in `plugin.toml` that holds the notes. */
export const NOTES_BINDING = "DB";
/**
 * The `databaseMigrations` registration for {@link NOTES_BINDING}: applied
 * once, in order, before the binding is handed to the plugin.
 */
export const MIGRATIONS = [
    {
        id: "notes-v1",
        operations: [
            {
                kind: "schema",
                value: "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT NOT NULL)",
            },
        ],
    },
];
/**
 * The cap a database error exceeded, when it is a typed `limit_exceeded`.
 *
 * Checks the error's shape rather than `instanceof PluginError`, so the
 * module needs no runtime import from the SDK.
 *
 * @param err - Anything a binding call threw.
 * @returns The exceeded cap, or `undefined` for any other error.
 */
export function exceededLimit(err) {
    if (typeof err !== "object" || err === null)
        return undefined;
    const { code, limit } = err;
    if (code !== "limit_exceeded" || typeof limit !== "object" || limit === null)
        return undefined;
    const { limit: name, cap, used } = limit;
    if (typeof name !== "string" || typeof cap !== "number" || typeof used !== "number") {
        return undefined;
    }
    return { limit: name, cap, used };
}
/**
 * A note refused before it is sent: the session could store it but never
 * return it, because a reply cell is held to `maxCellBytes`.
 *
 * Shaped like the SDK's typed `limit_exceeded` error, so callers handle
 * both with {@link exceededLimit}.
 */
export class NoteTooLargeError extends Error {
    /** Always `limit_exceeded`, as on the SDK's `PluginError`. */
    code = "limit_exceeded";
    /** The cap the note passes. */
    limit;
    /**
     * Creates the error.
     *
     * @param limit - The cap the note passes and its size in UTF-8 bytes.
     */
    constructor(limit) {
        super(`limit exceeded: ${limit.limit} is ${limit.cap}; used ${limit.used}`);
        this.name = "NoteTooLargeError";
        this.limit = limit;
    }
}
/** Measures text the way the database caps do: in UTF-8 bytes. */
const UTF8 = new TextEncoder();
/** A TEXT bind value. */
function text(value) {
    return { kind: "text", value };
}
/** An INTEGER bind value. */
function int64(value) {
    return { kind: "int64", value };
}
/**
 * Reads one stored row back into a {@link Note}.
 *
 * @param row - Column name to typed value, as the binding returns rows.
 * @returns The note.
 * @throws Error when a column is missing or has an unexpected type.
 */
export function noteFromRow(row) {
    const id = row["id"];
    const body = row["body"];
    if (id?.kind !== "int64" || body?.kind !== "text") {
        throw new Error("notes row has an unexpected shape");
    }
    return { id: id.value, body: body.value };
}
/**
 * Stores `bodies` as new notes.
 *
 * Every body is first checked against the session's `maxCellBytes`: a note
 * the database would accept but could never return is refused up front.
 * Then `chunk()` splits the inserts into batches that each fit one request under
 * the session's `maxStatements` and `maxRequestBytes`. Each batch commits
 * atomically; the loop does not, so when a later batch fails the notes of
 * the earlier ones stay stored and the count says how far it got.
 *
 * @param db - The plugin's notes binding.
 * @param bodies - Note texts, stored in order.
 * @returns How many notes were stored.
 * @throws NoteTooLargeError, before anything is written, when a body passes
 *   `maxCellBytes`; PluginError from the binding, `limit_exceeded` included
 *   when one statement passes `maxPayloadBytes` or `maxRequestBytes`.
 */
export async function addNotes(db, bodies) {
    const caps = await db.limits();
    for (const body of bodies) {
        const used = UTF8.encode(body).byteLength;
        if (used > caps.maxCellBytes) {
            throw new NoteTooLargeError({ limit: "maxCellBytes", cap: caps.maxCellBytes, used });
        }
    }
    const inserts = bodies.map((body) => db.prepare("INSERT INTO notes (body) VALUES (?)").bind(text(body)));
    let stored = 0;
    for await (const batch of db.chunk(inserts)) {
        await db.batch(batch);
        stored += batch.length;
    }
    return stored;
}
/**
 * Counts the stored notes.
 *
 * @param db - The plugin's notes binding.
 * @returns The number of notes.
 */
export async function countNotes(db) {
    const n = await db.prepare("SELECT count(*) AS n FROM notes").first("n");
    return n !== null && typeof n === "object" && "kind" in n && n.kind === "int64"
        ? Number(n.value)
        : 0;
}
/**
 * Yields every note in id order without holding them all in memory.
 *
 * When the session streams (`maxStreamedResultBytes > 0`), one `iterate()`
 * reads the whole table as a single snapshot, a frame at a time. When it
 * does not, every read is held to the inline caps, so the notes are read in
 * pages keyed on `id` (never `OFFSET`), each sized from `maxResultRows` and
 * halved whenever a page's bytes pass `maxResultBytes`.
 *
 * @param db - The plugin's notes binding.
 * @returns The notes, oldest first.
 * @throws PluginError from the binding: `limit_exceeded` when even the
 *   streamed budget is too small, or one note alone passes the inline caps.
 */
export async function* listNotes(db) {
    const caps = await db.limits();
    if (caps.maxStreamedResultBytes > 0) {
        for await (const row of db.prepare("SELECT id, body FROM notes ORDER BY id").iterate()) {
            yield noteFromRow(row);
        }
        return;
    }
    let pageSize = Math.max(1, caps.maxResultRows);
    let after = 0n;
    for (;;) {
        let rows;
        try {
            const page = await db
                .prepare("SELECT id, body FROM notes WHERE id > ? ORDER BY id LIMIT ?")
                .bind(int64(after), int64(BigInt(pageSize)))
                .all();
            rows = page.results ?? [];
        }
        catch (err) {
            const limit = exceededLimit(err);
            if (limit?.limit === "maxResultBytes" && pageSize > 1) {
                pageSize = Math.max(1, Math.floor(pageSize / 2));
                continue;
            }
            throw err;
        }
        for (const row of rows) {
            const note = noteFromRow(row);
            after = note.id;
            yield note;
        }
        if (rows.length < pageSize)
            return;
    }
}
