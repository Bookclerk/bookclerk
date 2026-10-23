import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  BudgetLedger,
  DEFAULT_DAILY_NEURONS,
  MAX_DAILY_NEURONS,
  MAX_OUTPUT_TOKENS,
  MODELS,
  handleTriage,
  inputTokenBound,
  neuronsFor,
  utcDay,
} from "./triage.js";

/** KV namespace stand-in over a Map, recording put options. */
function fakeKv() {
  const map = new Map();
  return {
    map,
    async get(key) {
      return map.has(key) ? map.get(key).value : null;
    },
    async put(key, value, opts) {
      map.set(key, { value, opts });
    },
  };
}

/** TRIAGE_BUDGET stand-in with a ledger view for assertions. */
function fakeBudget() {
  const kv = fakeKv();
  kv.ledger = new BudgetLedger(kv);
  return kv;
}

const BATCH = [{ report_id: "11111111-1111-4111-8111-111111111111", events: [] }];
const NOW = Date.UTC(2026, 9, 23, 12, 0, 0);

function triageRequest(body) {
  const text = typeof body === "string" ? body : JSON.stringify(body);
  return new Request("https://collector.test/triage", { method: "POST", body: text });
}

describe("BudgetLedger", () => {
  it("reserves under the cap and refuses past it", async () => {
    const ledger = new BudgetLedger(fakeKv());
    assert.deepEqual(await ledger.reserve("2026-10-23", 600, 1000), { ok: true, spent: 600 });
    assert.deepEqual(await ledger.reserve("2026-10-23", 500, 1000), { ok: false, spent: 600 });
    assert.deepEqual(await ledger.reserve("2026-10-23", 400, 1000), { ok: true, spent: 1000 });
  });

  it("settles a reservation to the actual cost", async () => {
    const ledger = new BudgetLedger(fakeKv());
    await ledger.reserve("2026-10-23", 600, 1000);
    assert.equal(await ledger.settle("2026-10-23", 600, 150), 150);
    assert.equal(await ledger.settle("2026-10-23", 900, 0), 0);
  });

  it("keys each UTC day separately and lets old days expire", async () => {
    const kv = fakeKv();
    const ledger = new BudgetLedger(kv);
    await ledger.reserve("2026-10-23", 900, 1000);
    assert.deepEqual(await ledger.reserve("2026-10-24", 900, 1000), { ok: true, spent: 900 });
    for (const { opts } of kv.map.values()) {
      assert.ok(opts.expirationTtl >= 24 * 60 * 60);
    }
  });

  it("reads a missing or corrupt total as zero", async () => {
    const kv = fakeKv();
    await kv.put("triage-neurons:2026-10-23", "garbage");
    assert.equal(await new BudgetLedger(kv).spent("2026-10-23"), 0);
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
    const rates = MODELS[body.model];
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
    assert.equal(await budget.ledger.spent(utcDay(NOW)), body.neurons);
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
    assert.ok(await budget.ledger.spent(utcDay(NOW)) > 0);
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

  it("clamps the daily cap below the free allocation", async () => {
    const env = {
      TRIAGE_BUDGET: fakeBudget(),
      TRIAGE_DAILY_NEURONS: "50000",
      AI: { run: async () => ({ response: "{}", usage: { prompt_tokens: 1, completion_tokens: 1 } }) },
    };
    const body = await (await handleTriage(triageRequest(BATCH), env, { now: () => NOW })).json();
    assert.equal(body.cap, MAX_DAILY_NEURONS);
  });

  it("does not trust null or negative usage", async () => {
    for (const usage of [{ prompt_tokens: null, completion_tokens: 0 }, { prompt_tokens: -500, completion_tokens: 1 }]) {
      const budget = fakeBudget();
      const env = { TRIAGE_BUDGET: budget, AI: { run: async () => ({ response: "{}", usage }) } };
      const body = await (await handleTriage(triageRequest(BATCH), env, { now: () => NOW })).json();
      assert.ok(body.neurons > 0, JSON.stringify(usage));
      assert.equal(await budget.ledger.spent(utcDay(NOW)), body.neurons);
    }
  });

  it("reads OpenAI-style replies", async () => {
    const env = {
      TRIAGE_BUDGET: fakeBudget(),
      AI: { run: async () => ({ choices: [{ message: { content: '{"issues":[]}' } }] }) },
    };
    const body = await (await handleTriage(triageRequest(BATCH), env, { now: () => NOW })).json();
    assert.equal(body.plan_text, '{"issues":[]}');
  });

  it("sizes the batch as the model sees it", async () => {
    // Under the body limit as sent, over it once re-serialized.
    const n = Math.floor((150 * 1024) / 5);
    const raw = `[{"report_id":"x","f":[${Array(n).fill("1e20").join(",")}]}]`;
    assert.ok(raw.length < 160 * 1024);
    const env = { TRIAGE_BUDGET: fakeBudget(), AI: { run: async () => ({ response: "" }) } };
    assert.equal((await handleTriage(triageRequest(raw), env)).status, 413);
  });

  it("is unavailable without its bindings", async () => {
    assert.equal((await handleTriage(triageRequest(BATCH), {})).status, 503);
  });
});
