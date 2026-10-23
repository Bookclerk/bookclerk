import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { DatabaseSync } from "node:sqlite";
import {
  BudgetLedger,
  DEFAULT_DAILY_NEURONS,
  MAX_OUTPUT_TOKENS,
  MODEL_NEURONS,
  handleTriage,
  inputTokenBound,
  neuronsFor,
  utcDay,
} from "./triage.js";

/** Durable Object `sql` stand-in over an in-memory SQLite database. */
function fakeSql() {
  const db = new DatabaseSync(":memory:");
  return {
    exec(query, ...args) {
      const stmt = db.prepare(query);
      const rows = stmt.columns().length ? stmt.all(...args) : (stmt.run(...args), []);
      return { toArray: () => rows };
    },
  };
}

/** TRIAGE_BUDGET namespace stand-in backed by one ledger. */
function fakeBudget(ledger = new BudgetLedger(fakeSql())) {
  return { ledger, idFromName: () => "global", get: () => ledger };
}

const BATCH = [{ report_id: "11111111-1111-4111-8111-111111111111", events: [] }];
const NOW = Date.UTC(2026, 9, 23, 12, 0, 0);

function triageRequest(body) {
  const text = typeof body === "string" ? body : JSON.stringify(body);
  return new Request("https://collector.test/triage", { method: "POST", body: text });
}

describe("BudgetLedger", () => {
  it("reserves under the cap and refuses past it", () => {
    const ledger = new BudgetLedger(fakeSql());
    assert.deepEqual(ledger.reserve("2026-10-23", 600, 1000), { ok: true, spent: 600 });
    assert.deepEqual(ledger.reserve("2026-10-23", 500, 1000), { ok: false, spent: 600 });
    assert.deepEqual(ledger.reserve("2026-10-23", 400, 1000), { ok: true, spent: 1000 });
  });

  it("settles a reservation to the actual cost", () => {
    const ledger = new BudgetLedger(fakeSql());
    ledger.reserve("2026-10-23", 600, 1000);
    assert.equal(ledger.settle("2026-10-23", 600, 150), 150);
  });

  it("starts each UTC day empty and drops older days", () => {
    const ledger = new BudgetLedger(fakeSql());
    ledger.reserve("2026-10-23", 900, 1000);
    assert.deepEqual(ledger.reserve("2026-10-24", 900, 1000), { ok: true, spent: 900 });
    assert.equal(ledger.spent("2026-10-23"), 0);
  });
});

describe("cost helpers", () => {
  it("rounds neurons up", () => {
    assert.equal(neuronsFor({ input: 1000, output: 1000 }, 1, 0), 1);
    assert.equal(neuronsFor({ input: 4119, output: 34868 }, 1_000_000, 0), 4119);
  });

  it("bounds tokens by UTF-8 bytes", () => {
    assert.equal(inputTokenBound("é"), 2);
  });

  it("keys budget days on UTC", () => {
    assert.equal(utcDay(Date.UTC(2026, 9, 23, 23, 59, 59)), "2026-10-23");
  });
});

describe("handleTriage", () => {
  it("returns the plan text and settles to the reported usage", async () => {
    const budget = fakeBudget();
    const seen = [];
    const env = {
      TRIAGE_BUDGET: budget,
      AI: {
        async run(model, input) {
          seen.push({ model, input });
          return { response: '{"issues":[]}', usage: { prompt_tokens: 1000, completion_tokens: 100 } };
        },
      },
    };
    const res = await handleTriage(triageRequest(BATCH), env, { now: () => NOW });
    assert.equal(res.status, 200);
    const body = await res.json();
    const rates = MODEL_NEURONS[body.model];
    assert.equal(body.plan_text, '{"issues":[]}');
    assert.equal(body.neurons, neuronsFor(rates, 1000, 100));
    assert.equal(body.spent_today, body.neurons);
    assert.equal(body.cap, DEFAULT_DAILY_NEURONS);
    assert.equal(seen[0].input.max_tokens, MAX_OUTPUT_TOKENS);
    assert.equal(seen[0].input.messages[0].role, "system");
    assert.match(seen[0].input.messages[1].content, /^UNTRUSTED_DATA_BEGIN\n/);
  });

  it("refuses before calling the model when the budget would be passed", async () => {
    let called = false;
    const env = {
      TRIAGE_BUDGET: fakeBudget(),
      TRIAGE_DAILY_NEURONS: "1",
      AI: {
        async run() {
          called = true;
          return { response: "" };
        },
      },
    };
    const res = await handleTriage(triageRequest(BATCH), env, { now: () => NOW });
    assert.equal(res.status, 429);
    assert.equal(called, false);
  });

  it("keeps the worst case when the model reports no usage", async () => {
    const budget = fakeBudget();
    const env = { TRIAGE_BUDGET: budget, AI: { run: async () => ({ response: "{}" }) } };
    const res = await handleTriage(triageRequest(BATCH), env, { now: () => NOW });
    const body = await res.json();
    assert.ok(body.neurons > 0);
    assert.equal(budget.ledger.spent(utcDay(NOW)), body.neurons);
  });

  it("keeps the reservation when the model call fails", async () => {
    const budget = fakeBudget();
    const env = {
      TRIAGE_BUDGET: budget,
      AI: {
        run: async () => {
          throw new Error("boom");
        },
      },
    };
    const res = await handleTriage(triageRequest(BATCH), env, { now: () => NOW });
    assert.equal(res.status, 502);
    assert.ok(budget.ledger.spent(utcDay(NOW)) > 0);
  });

  it("refuses an unknown model, a bad batch and an oversized batch", async () => {
    const ai = { run: async () => ({ response: "" }) };
    const unknown = await handleTriage(triageRequest(BATCH), {
      TRIAGE_BUDGET: fakeBudget(),
      AI: ai,
      TRIAGE_MODEL: "@cf/some/unpriced-model",
    });
    assert.equal(unknown.status, 503);
    const env = { TRIAGE_BUDGET: fakeBudget(), AI: ai };
    assert.equal((await handleTriage(triageRequest("not json"), env)).status, 400);
    assert.equal((await handleTriage(triageRequest([]), env)).status, 400);
    assert.equal((await handleTriage(triageRequest({ a: 1 }), env)).status, 400);
    const big = [{ report_id: "x", blob: "a".repeat(200 * 1024) }];
    assert.equal((await handleTriage(triageRequest(big), env)).status, 413);
  });

  it("is unavailable without its bindings", async () => {
    assert.equal((await handleTriage(triageRequest(BATCH), {})).status, 503);
  });
});
