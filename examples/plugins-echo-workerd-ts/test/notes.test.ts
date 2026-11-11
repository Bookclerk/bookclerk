/**
 * The notes helpers against the binding a host grants, served by
 * `bookclerk-db-testkit` under one capability profile.
 *
 * Run through `bookclerk-plugin test .` (`cargo plugin -- test
 * examples/plugins-echo-workerd-ts` in the workspace), which runs this suite
 * once per profile and names the kit binary and the profile in the
 * environment. Every assertion is sized from the profile's own limits, so
 * the same tests prove the code on the shipped adapters, on D1 and on the
 * smallest advertisement a host accepts.
 */

import assert from "node:assert/strict";
import { after, before, beforeEach, test } from "node:test";

import { createTestDatabase, type TestDatabase } from "@bookclerk/plugin-sdk/testkit";
import type { DatabaseBinding, DbValue } from "@bookclerk/plugin-sdk/workerd";

import {
  MIGRATIONS,
  NoteTooLargeError,
  addNotes,
  countNotes,
  exceededLimit,
  listNotes,
  noteFromRow,
  type Note,
} from "../src/notes.ts";

/** The served binding; the profile comes from `BOOKCLERK_DB_TESTKIT_PROFILE`. */
let db: TestDatabase;

before(async () => {
  db = await createTestDatabase({ migrations: MIGRATIONS });
});

after(async () => {
  await db.close();
});

beforeEach(async () => {
  await db.binding.prepare("DELETE FROM notes").run();
});

/**
 * Drains {@link listNotes}.
 *
 * @returns Every note, in the order the helper yields them.
 */
async function allNotes(): Promise<Note[]> {
  const notes: Note[] = [];
  for await (const note of listNotes(db.binding)) notes.push(note);
  return notes;
}

/**
 * The most bytes one note may hold under the profile: a statement's SQL and
 * binds share `maxPayloadBytes` (3,667 bytes on D1), and a reply cell is
 * held to `maxCellBytes`.
 *
 * @returns The byte budget for one note body.
 */
async function noteBudget(): Promise<number> {
  const caps = await db.limits();
  return Math.min(caps.maxCellBytes, caps.maxPayloadBytes);
}

/**
 * Note bodies that together pass the profile's inline result cap, so neither
 * a single inline read nor a single request holds them all.
 *
 * @returns The bodies, each half the per-note budget.
 */
async function bodiesPastTheInlineCap(): Promise<string[]> {
  const caps = await db.limits();
  // Multibyte text, so byte and character counts differ: "né" is 3 bytes.
  const body = "né".repeat(Math.floor((await noteBudget()) / 2 / 3));
  const bytes = new TextEncoder().encode(body).byteLength;
  const count = Math.ceil((2 * caps.maxResultBytes) / bytes) + 1;
  return Array.from({ length: count }, (_, i) => `${i} ${body}`);
}

test("the migrations create the notes table", async () => {
  assert.equal(await countNotes(db.binding), 0);
});

test("addNotes splits a long write into requests that fit", async () => {
  const bodies = await bodiesPastTheInlineCap();
  assert.equal(await addNotes(db.binding, bodies), bodies.length);
  assert.equal(await countNotes(db.binding), bodies.length);
});

test("listNotes returns every note in order, past the inline result cap", async () => {
  const bodies = await bodiesPastTheInlineCap();
  await addNotes(db.binding, bodies);
  const notes = await allNotes();
  assert.deepEqual(
    notes.map((n) => n.body),
    bodies,
  );
  for (let i = 1; i < notes.length; i++) {
    assert.ok(notes[i - 1]!.id < notes[i]!.id, "ids increase");
  }
  // The plain inline read the helper avoids is refused with the typed error
  // whenever the session cannot stream it.
  const caps = await db.limits();
  if (caps.maxStreamedResultBytes === 0) {
    await assert.rejects(
      db.binding.prepare("SELECT id, body FROM notes").all(),
      (err) => exceededLimit(err) !== undefined,
    );
  }
});

test("listNotes yields nothing for an empty table", async () => {
  assert.deepEqual(await allNotes(), []);
});

test("listNotes halves a page whose bytes pass maxResultBytes, then grows back", async () => {
  // A session that does not stream and allows four rows a page, where note
  // 2 is so wide that it fits a page only alone: a scripted binding, since
  // no kit profile combines several rows a page with no streamed reads.
  const stored = [1n, 2n, 3n, 4n, 5n].map((id) => ({
    id: { kind: "int64", value: id },
    body: { kind: "text", value: `note ${id}` },
  }));
  const pageSizes: number[] = [];
  const tooWide = { code: "limit_exceeded", limit: { limit: "maxResultBytes", cap: 10, used: 20 } };
  const scripted = {
    limits: () => Promise.resolve({ maxStreamedResultBytes: 0, maxResultRows: 4 }),
    prepare: () => ({
      bind: (after: DbValue, size: DbValue) => ({
        all: () => {
          const n = Number(size.value);
          pageSizes.push(n);
          const results = stored.filter((r) => r.id.value > (after.value as bigint)).slice(0, n);
          if (results.length > 1 && results.some((r) => r.id.value === 2n)) {
            return Promise.reject(tooWide);
          }
          return Promise.resolve({ results });
        },
      }),
    }),
  } as unknown as DatabaseBinding;
  const notes: Note[] = [];
  for await (const note of listNotes(scripted)) notes.push(note);
  assert.deepEqual(
    notes.map((n) => n.id),
    [1n, 2n, 3n, 4n, 5n],
  );
  // Halved to fit note 2, then doubled back to the row cap after it.
  assert.deepEqual(pageSizes, [4, 2, 1, 2, 1, 2, 4]);

  // One row that alone passes the cap, or any other error, is not retried.
  pageSizes.length = 0;
  stored.length = 0;
  const refusing = {
    limits: scripted.limits,
    prepare: () => ({ bind: () => ({ all: () => Promise.reject(tooWide) }) }),
  } as unknown as DatabaseBinding;
  await assert.rejects(listNotes(refusing).next(), (err) => err === tooWide);
  const broken = new Error("unavailable");
  const failing = {
    limits: scripted.limits,
    prepare: () => ({ bind: () => ({ all: () => Promise.reject(broken) }) }),
  } as unknown as DatabaseBinding;
  await assert.rejects(listNotes(failing).next(), (err) => err === broken);
});

test("a note past the per-note budget is a typed limit, and nothing is stored", async () => {
  await assert.rejects(addNotes(db.binding, ["x".repeat((await noteBudget()) + 1)]), (err) => {
    const limit = exceededLimit(err);
    assert.ok(limit, `expected limit_exceeded, got ${String(err)}`);
    assert.ok(
      ["maxCellBytes", "maxPayloadBytes", "maxRequestBytes"].includes(limit.limit),
      limit.limit,
    );
    assert.ok(limit.used > limit.cap);
    return true;
  });
  assert.equal(await countNotes(db.binding), 0);
});

test("exceededLimit reads only typed limit errors", () => {
  const limit = { limit: "maxResultBytes", cap: 10, used: 11 };
  assert.deepEqual(exceededLimit({ code: "limit_exceeded", limit }), limit);
  assert.equal(
    exceededLimit(new Error("limit exceeded: maxResultBytes is 10; used 11")),
    undefined,
  );
  assert.equal(exceededLimit({ code: "unavailable", limit }), undefined);
  assert.equal(exceededLimit({ code: "limit_exceeded" }), undefined);
  assert.equal(
    exceededLimit({ code: "limit_exceeded", limit: { limit: "x", cap: "1" } }),
    undefined,
  );
  assert.equal(exceededLimit(null), undefined);
  assert.equal(exceededLimit("limit_exceeded"), undefined);
});

test("countNotes refuses a count of the wrong shape", async () => {
  const scripted = (row: unknown) =>
    ({ prepare: () => ({ first: () => Promise.resolve(row) }) }) as unknown as DatabaseBinding;
  assert.equal(await countNotes(scripted({ n: { kind: "int64", value: 3n } })), 3);
  await assert.rejects(countNotes(scripted(null)), /unexpected shape/);
  await assert.rejects(
    countNotes(scripted({ n: { kind: "text", value: "3" } })),
    /unexpected shape/,
  );
});

test("addNotes reports each committed batch", async () => {
  const caps = await db.limits();
  const seen: number[] = [];
  const count = caps.maxStatements * 2 + 1;
  const bodies = Array.from({ length: count }, (_, i) => `n${i}`);
  assert.equal(await addNotes(db.binding, bodies, (n) => seen.push(n)), count);
  assert.ok(seen.length >= 3, `batches ${seen.join(",")}`);
  assert.equal(seen.at(-1), count);
  for (let i = 1; i < seen.length; i++) assert.ok(seen[i]! > seen[i - 1]!);
});

test("NoteTooLargeError reads as a typed limit", () => {
  const limit = { limit: "maxCellBytes", cap: 4, used: 5 };
  const err = new NoteTooLargeError(limit);
  assert.deepEqual(exceededLimit(err), limit);
  assert.equal(err.message, "limit exceeded: maxCellBytes is 4; used 5");
});

test("noteFromRow refuses a row of the wrong shape", () => {
  assert.deepEqual(
    noteFromRow({ id: { kind: "int64", value: 7n }, body: { kind: "text", value: "hi" } }),
    { id: 7n, body: "hi" },
  );
  assert.throws(() => noteFromRow({ id: { kind: "text", value: "7" } }), /unexpected shape/);
});
