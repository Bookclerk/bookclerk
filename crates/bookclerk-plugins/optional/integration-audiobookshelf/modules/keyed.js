/**
 * Reads of the plugin database sized from the binding's limits, and the
 * small value helpers the other modules share.
 *
 * The module imports only types from the SDK, so the unit tests load it
 * outside workerd.
 */
/**
 * A TEXT bind value.
 *
 * @param value - The text.
 * @returns The bind value.
 */
export function text(value) {
    return { kind: "text", value };
}
/**
 * An INTEGER bind value.
 *
 * @param value - The integer.
 * @returns The bind value.
 */
export function int64(value) {
    return { kind: "int64", value };
}
/**
 * The text in a row's column.
 *
 * @param row - The row.
 * @param column - Column name.
 * @returns The text, or `undefined` when the column is missing or not TEXT.
 */
export function textAt(row, column) {
    const value = row[column];
    return value?.kind === "text" ? value.value : undefined;
}
/**
 * The integer in a row's column.
 *
 * @param row - The row.
 * @param column - Column name.
 * @returns The integer, or `undefined` when the column is missing or not INTEGER.
 */
export function int64At(row, column) {
    const value = row[column];
    return value?.kind === "int64" ? value.value : undefined;
}
/** Largest signed 64-bit integer, the "no limit" of a keyed read. */
const I64_MAX = 0x7fffffffffffffffn;
/**
 * Whether an error is a typed `limit_exceeded` on `maxResultBytes`.
 *
 * Checks the error's shape rather than `instanceof PluginError`, so the
 * module needs no runtime import from the SDK.
 *
 * @param err - Anything a binding call threw.
 * @returns `true` when a page's bytes passed the inline cap.
 */
function tooWide(err) {
    if (typeof err !== "object" || err === null)
        return false;
    const { code, limit } = err;
    if (code !== "limit_exceeded" || typeof limit !== "object" || limit === null)
        return false;
    return limit.limit === "maxResultBytes";
}
/**
 * Every row a key-ordered query returns, read within the session's limits.
 *
 * `sql` ends `AND <key> > ? ORDER BY <key> LIMIT ?` (or `WHERE <key> > ?
 * …`), where `<key>` is the unique TEXT column named by `key`, which the
 * query also returns; `binds` fills the placeholders before those two.
 * When the session streams, one read with no lower bound and no row limit
 * returns it all. When it does not, the rows come in pages keyed on `key`
 * (never `OFFSET`), each sized from `maxResultRows`, halved whenever a
 * page's bytes pass `maxResultBytes` and doubled again (up to
 * `maxResultRows`) after each full page.
 *
 * @param db - The plugin's database binding.
 * @param sql - The query, shaped as above.
 * @param binds - Values for the placeholders before the key bound and limit.
 * @param key - Name of the returned TEXT column the rows are ordered on.
 * @returns The rows in key order.
 * @throws The binding's error, `limit_exceeded` included when a single row
 *   passes the inline caps, and an `Error` when a paged row has no TEXT
 *   `key` column.
 */
export async function readKeyed(db, sql, binds, key) {
    const caps = await db.limits();
    const out = [];
    if (caps.maxStreamedResultBytes > 0 && caps.maxStreamedResultRows > 0) {
        for await (const row of db
            .prepare(sql)
            .bind(...binds, text(""), int64(I64_MAX))
            .iterate()) {
            out.push(row);
        }
        return out;
    }
    const maxPage = Math.max(1, caps.maxResultRows);
    let pageSize = maxPage;
    let after = "";
    for (;;) {
        let rows;
        try {
            const read = await db
                .prepare(sql)
                .bind(...binds, text(after), int64(BigInt(pageSize)))
                .all();
            rows = (read.results ?? []);
        }
        catch (err) {
            if (tooWide(err) && pageSize > 1) {
                pageSize = Math.floor(pageSize / 2);
                continue;
            }
            throw err;
        }
        for (const row of rows) {
            const id = textAt(row, key);
            if (id === undefined)
                throw new Error(`keyed read row has no text ${key}`);
            after = id;
            out.push(row);
        }
        if (rows.length < pageSize)
            return out;
        pageSize = Math.min(pageSize * 2, maxPage);
    }
}
/**
 * Runs `statements` in batches that each fit one request.
 *
 * Each batch is atomic; the run as a whole is not.
 *
 * @param db - The plugin's database binding.
 * @param statements - Prepared statements with their binds.
 * @throws The binding's error; batches before the failing one stay written.
 */
export async function writeAll(db, statements) {
    if (statements.length === 0)
        return;
    for await (const batch of db.chunk(statements))
        await db.batch(batch);
}
