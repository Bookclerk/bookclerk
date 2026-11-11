// The cache-class contract (cache-control.ts): every answer the Worker gives
// and every rule in public/_headers is one of the table's classes, and the
// dashboard's hashed styles and script are named after their contents
// (asset-names.ts), so browsers may keep them for a year.
// cache-control-source.test.ts checks the Worker's source text.

import assert from "node:assert/strict";
import { describe, it } from "vitest";

import {
  ASSETS_PATH,
  assetOf,
  contentHash,
  hashedName,
  HASHED_ASSETS,
  type HashedAsset,
} from "./asset-names.ts";
import { CACHE_CONTROL, cacheClassOf } from "./cache-control.ts";
import { ASSET_URLS } from "./dashboard/asset-urls.ts";
import type { Env } from "./env.ts";
import worker, { withCacheClass } from "./worker.ts";
// Read by Vite when it transforms this test: it runs inside the Workers
// runtime, which has no view of the repository.
import HEADERS_FILE from "./public/_headers?raw";

/** The hashed files in public/static/assets/, by path. */
const ASSET_FILES: Readonly<Record<string, string>> = import.meta.glob<string>(
  ["./public/static/assets/*"],
  { query: "?raw", import: "default", eager: true },
);

/** Execution context: background work is not awaited by these tests. */
const CTX = { waitUntil(): void {} };

/** Bindings without the dashboard, so only the collector's own routes answer. */
const ENV: Env = {
  REPORT_API_KEY: "k".repeat(32),
  B2_KEY_ID: "id",
  B2_APPLICATION_KEY: "secret-application-key",
  B2_BUCKET_ID: "bucket",
};

describe("cache classes", () => {
  it("names each class and nothing else", () => {
    for (const [name, value] of Object.entries(CACHE_CONTROL)) {
      assert.equal(cacheClassOf(value), name);
    }
    assert.equal(cacheClassOf(null), null);
    assert.equal(cacheClassOf("private, max-age=60"), null);
  });

  it("gives every route's answer a class, and never keeps an error", async () => {
    const requests = [
      new Request("https://collector.test/robots.txt"),
      new Request("https://collector.test/robots.txt", { method: "HEAD" }),
      new Request("https://collector.test/"),
      new Request("https://collector.test/health"),
      new Request("https://collector.test/report?since=0"),
      new Request("https://collector.test/report/cursor", { method: "POST", body: "{}" }),
      // No dashboard database: a 204 the route sets no header on.
      new Request("https://collector.test/issues/summaries", {
        headers: { "X-Bookclerk-Report-Key": "k".repeat(32) },
      }),
      new Request("https://collector.test/issues/summary", { method: "POST", body: "{}" }),
      new Request("https://collector.test/triage", { method: "POST", body: "{}" }),
      // No AI binding: the triage route's own JSON, which sets no header.
      new Request("https://collector.test/triage", {
        method: "POST",
        headers: { "X-Bookclerk-Report-Key": "k".repeat(32), "Content-Type": "application/json" },
        body: "{}",
      }),
      new Request("https://collector.test/nowhere"),
      new Request("https://collector.test/static/assets/missing.css"),
    ];
    for (const request of requests) {
      const res = await worker.fetch(request, ENV, CTX);
      const value = res.headers.get("Cache-Control");
      assert.notEqual(cacheClassOf(value), null, `${request.method} ${request.url}: ${value}`);
      if (res.status >= 300 || res.status === 204)
        assert.equal(value, CACHE_CONTROL.noStore, request.url);
    }
  });

  it("keeps an internal error out of every cache", async () => {
    const broken = new Proxy(ENV, {
      get(target, name, receiver) {
        if (name === "DASHBOARD_DB") throw new Error("binding failed");
        return Reflect.get(target, name, receiver);
      },
    });
    const realError = console.error;
    console.error = () => {};
    try {
      const res = await worker.fetch(new Request("https://collector.test/"), broken, CTX);
      assert.equal(res.status, 500);
      assert.equal(res.headers.get("Cache-Control"), CACHE_CONTROL.noStore);
    } finally {
      console.error = realError;
    }
  });

  it("runs the scheduled sync, which does nothing with the dashboard off", async () => {
    const pending: Promise<unknown>[] = [];
    const ctx = {
      waitUntil: (p: Promise<unknown>) => void pending.push(p),
      passThroughOnException: () => {},
      props: {},
    } as unknown as ExecutionContext;
    await worker.scheduled({} as ScheduledController, ENV, ctx);
    assert.equal(pending.length, 1);
    await pending[0];
  });

  it("gives an answer without a Cache-Control no-store and leaves the rest alone", async () => {
    const empty = withCacheClass(new Response(null, { status: 204 }));
    assert.equal(empty.status, 204);
    assert.equal(empty.headers.get("Cache-Control"), CACHE_CONTROL.noStore);

    const own = new Response("x", { headers: { "Cache-Control": CACHE_CONTROL.hour } });
    assert.equal(withCacheClass(own), own);

    const pair = new WebSocketPair();
    const upgrade = new Response(null, { status: 101, webSocket: pair[0] });
    assert.equal(withCacheClass(upgrade), upgrade);
    assert.equal(upgrade.headers.get("Cache-Control"), null);
  });

  it("public/_headers lets browsers keep only versioned and hashed files", () => {
    const rules = [...HEADERS_FILE.matchAll(/^(\S+)\n((?:[ \t]+.+\n?)+)/gm)];
    const cached = rules.flatMap(([, path, block]) => {
      const value = /Cache-Control:\s*(.+)/.exec(block ?? "")?.[1]?.trim();
      return value === undefined ? [] : [[path, cacheClassOf(value)]];
    });
    assert.deepEqual(cached, [
      ["/static/fonts/*", "immutable"],
      [ASSETS_PATH + "*", "immutable"],
    ]);
  });
});

describe("hashed dashboard files", () => {
  it("are named after their contents and linked by those names", async () => {
    const names: Partial<Record<HashedAsset, string>> = {};
    for (const [path, text] of Object.entries(ASSET_FILES)) {
      const name = path.slice(path.lastIndexOf("/") + 1);
      const file = assetOf(name);
      assert.ok(file !== null, `${name} is not a hashed dashboard file`);
      const hash = await contentHash(new Uint8Array(new TextEncoder().encode(text)));
      assert.equal(name, hashedName(file, hash), `${name} changed: run npm run hash-assets`);
      assert.equal(names[file], undefined, `two copies of ${file}`);
      names[file] = name;
    }
    assert.deepEqual(Object.keys(names).sort(), [...HASHED_ASSETS].sort());
    for (const file of HASHED_ASSETS) {
      assert.equal(ASSET_URLS[file], `${ASSETS_PATH}${names[file]}`);
    }
  });

  it("assetOf reads only hashed names of known files", () => {
    assert.equal(assetOf("dashboard.0123456789ab.css"), "dashboard.css");
    assert.equal(assetOf("dashboard.0123456789ab.js"), "dashboard.js");
    assert.equal(assetOf("dashboard.css"), null);
    assert.equal(assetOf("other.0123456789ab.css"), null);
    assert.equal(assetOf("dashboard.0123456789AB.css"), null);
  });

  it("contentHash is the first 12 hex digits of SHA-256", async () => {
    // SHA-256 of the empty string.
    assert.equal(await contentHash(new Uint8Array()), "e3b0c44298fc");
  });
});
