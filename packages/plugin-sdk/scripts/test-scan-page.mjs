import assert from "node:assert/strict";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const dist = join(root, "../dist");

const { scanPage } = await import(join(dist, "scan-page.js"));
const { MAX_SCAN_CURSOR_BYTES, MAX_SCAN_PAGE_BOOKS } = await import(join(dist, "abi.js"));
const { PluginError } = await import(join(dist, "errors.js"));

/**
 * A title with only the field these tests read.
 *
 * @param {number} i - Title number.
 * @returns {{productId: string}} The title.
 */
function book(i) {
  return { productId: `p${i}` };
}

/**
 * Scan params for one call at `cursor`.
 *
 * @param {string} cursor - The call's cursor.
 * @param {string} syncToken - The pass's sync token.
 * @returns {object} The params.
 */
function params(cursor = "", syncToken = "") {
  return { accountId: "a1", cursor, syncToken };
}

/**
 * Runs a whole pass the way the host does: from the empty cursor until a
 * reply's `nextCursor` is empty, sending each `nextCursor` back.
 *
 * @param {Function} fetchStorePage - The store fetch.
 * @param {string} syncToken - The pass's sync token.
 * @returns {Promise<object[]>} Every reply of the pass.
 */
async function runPass(fetchStorePage, syncToken = "") {
  const replies = [];
  let cursor = "";
  for (let n = 0; n < 1000; n += 1) {
    const reply = await scanPage(params(cursor, syncToken), fetchStorePage);
    assert.ok(reply.books.length <= MAX_SCAN_PAGE_BOOKS);
    assert.ok(reply.removed.length <= MAX_SCAN_PAGE_BOOKS);
    replies.push(reply);
    if (reply.nextCursor === "") return replies;
    assert.notEqual(reply.nextCursor, cursor, "a reply never repeats its cursor");
    cursor = reply.nextCursor;
  }
  throw new Error("the pass did not end");
}

/**
 * Store pages of the given sizes, numbered from 1 by the store cursor.
 *
 * @param {number[]} sizes - Titles on each store page.
 * @param {object} last - Fields the last store page adds.
 * @returns {{fetch: Function, fetches: string[], total: number}} The store.
 */
function store(sizes, last = {}) {
  const fetches = [];
  let next = 0;
  const starts = sizes.map((size) => {
    const start = next;
    next += size;
    return start;
  });
  const fetch = async (storeCursor, p) => {
    fetches.push(storeCursor);
    assert.equal(p.accountId, "a1", "the fetch sees the call's params");
    const index = storeCursor === "" ? 0 : Number(storeCursor) - 1;
    const books = Array.from({ length: sizes[index] }, (_, i) => book(starts[index] + i));
    const isLast = index === sizes.length - 1;
    return { books, next: isLast ? "" : String(index + 2), ...(isLast ? last : {}) };
  };
  return { fetch, fetches, total: next };
}

// A store that does not page answers with everything, in one or more replies.
{
  const s = store([3], { syncToken: "t1", complete: true });
  const replies = await runPass(s.fetch);
  assert.equal(replies.length, 1);
  assert.deepEqual(replies[0], {
    books: [book(0), book(1), book(2)],
    removed: [],
    nextCursor: "",
    syncToken: "t1",
    complete: true,
    storePages: 1,
  });
}

// Store pages larger than a reply are sliced; every title arrives once, in
// order, and only the last reply carries the token and `complete`.
{
  const big = MAX_SCAN_PAGE_BOOKS * 2 + 7;
  const s = store([big, 4, MAX_SCAN_PAGE_BOOKS], { syncToken: "t2", complete: true });
  const replies = await runPass(s.fetch);
  const ids = replies.flatMap((r) => r.books.map((b) => b.productId));
  assert.deepEqual(
    ids,
    Array.from({ length: s.total }, (_, i) => `p${i}`),
  );
  assert.equal(replies.length, 5);
  for (const r of replies.slice(0, -1)) {
    assert.equal(r.syncToken, "");
    assert.equal(r.complete, false);
  }
  assert.equal(replies.at(-1).syncToken, "t2");
  assert.equal(replies.at(-1).complete, true);
  // The large store page is fetched once per slice.
  assert.deepEqual(s.fetches, ["", "", "", "2", "3"]);
}

// An empty library is one empty reply that ends the pass.
{
  const s = store([0], { complete: true });
  const replies = await runPass(s.fetch);
  assert.equal(replies.length, 1);
  assert.equal(replies[0].books.length, 0);
  assert.equal(replies[0].complete, true);
}

// Removed ids are sliced with the titles, and a delta pass is not complete.
{
  const removed = Array.from({ length: MAX_SCAN_PAGE_BOOKS + 1 }, (_, i) => `r${i}`);
  const fetch = async () => ({ books: [book(0)], removed, syncToken: "t3", storePages: 2 });
  const replies = await runPass(fetch, "t2");
  assert.equal(replies.length, 2);
  assert.deepEqual(
    replies.flatMap((r) => r.removed),
    removed,
  );
  assert.deepEqual(
    replies.flatMap((r) => r.books),
    [book(0)],
  );
  assert.equal(replies[1].syncToken, "t3");
  assert.equal(replies[1].complete, false);
  assert.deepEqual(
    replies.map((r) => r.storePages),
    [2, 2],
  );
}

// A cursor this helper did not write is `invalid_cursor`, so the host
// restarts the pass.
for (const cursor of ["3", "sp1.", "sp1.x.1", "sp1.01.1", "sp1.7.1", "sp2.0.1", "sp1.-500.1"]) {
  await assert.rejects(
    scanPage(params(cursor), async () => ({ books: [] })),
    (e) => e instanceof PluginError && e.code === "invalid_cursor",
    cursor,
  );
}

// A store page that shrank under a later slice is `invalid_cursor` too.
{
  const cursor = `sp1.${MAX_SCAN_PAGE_BOOKS}.`;
  await assert.rejects(
    scanPage(params(cursor), async () => ({ books: [book(0)] })),
    (e) => e instanceof PluginError && e.code === "invalid_cursor",
  );
}

// A store cursor that cannot fit a scan cursor fails before any reply.
{
  const next = "x".repeat(MAX_SCAN_CURSOR_BYTES);
  await assert.rejects(
    scanPage(params(), async () => ({ books: [], next })),
    (e) => e instanceof PluginError && e.code === "internal",
  );
}

// Store cursors may contain dots and any text.
{
  const fetches = [];
  const fetch = async (storeCursor) => {
    fetches.push(storeCursor);
    return storeCursor === "" ? { books: [book(0)], next: "a.b.é" } : { books: [book(1)] };
  };
  const replies = await runPass(fetch);
  assert.equal(replies.length, 2);
  assert.deepEqual(fetches, ["", "a.b.é"]);
  // The layout the Python and Rust SDKs write and read too.
  assert.equal(replies[0].nextCursor, "sp1.0.a.b.é");
}
{
  const fetch = async () => ({ books: Array.from({ length: MAX_SCAN_PAGE_BOOKS + 1 }, book) });
  const reply = await scanPage(params(), fetch);
  assert.equal(reply.nextCursor, `sp1.${MAX_SCAN_PAGE_BOOKS}.`);
}

console.log("scan-page: ok");
