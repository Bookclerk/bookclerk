// The cross-origin contract (cross-origin.ts): every answer the Worker gives
// carries the closed headers and no CORS grant, whatever the request's
// Origin, and public/_headers gives the dashboard's static files the same set.

import assert from "node:assert/strict";
import { describe, it } from "vitest";

import { CLOSED_HEADERS, withCrossOriginPolicy } from "./cross-origin.ts";
import { SECURITY_HEADERS } from "./dashboard/html.ts";
import type { Env } from "./env.ts";
import worker from "./worker.ts";
// Read by Vite when it transforms this test: it runs inside the Workers
// runtime, which has no view of the repository.
import HEADERS_FILE from "./public/_headers?raw";

/** Execution context: background work is not awaited by these tests. */
const CTX = { waitUntil(): void {} };

/** Bindings without the dashboard, so only the collector's own routes answer. */
const ENV: Env = {
  REPORT_API_KEY: "k".repeat(32),
  B2_KEY_ID: "id",
  B2_APPLICATION_KEY: "secret-application-key",
  B2_BUCKET_ID: "bucket",
};

/** An origin that is not the collector's. */
const ELSEWHERE = "https://elsewhere.example";

describe("cross-origin headers", () => {
  it("close every route's answer to other origins, preflights included", async () => {
    const requests = [
      new Request("https://collector.test/robots.txt"),
      new Request("https://collector.test/"),
      new Request("https://collector.test/health"),
      new Request("https://collector.test/report?since=0"),
      new Request("https://collector.test/submit", { method: "POST", body: "{}" }),
      new Request("https://collector.test/submit", {
        method: "OPTIONS",
        headers: { "Access-Control-Request-Method": "POST" },
      }),
      new Request("https://collector.test/triage", {
        method: "POST",
        headers: { "X-Bookclerk-Report-Key": "k".repeat(32), "Content-Type": "application/json" },
        body: "{}",
      }),
      new Request("https://collector.test/nowhere"),
    ];
    for (const request of requests) {
      const withOrigin = new Request(request, {
        headers: { ...Object.fromEntries(request.headers), Origin: ELSEWHERE },
      });
      const res = await worker.fetch(withOrigin, ENV, CTX);
      const what = `${request.method} ${request.url} ${res.status}`;
      for (const [name, value] of Object.entries(CLOSED_HEADERS))
        assert.equal(res.headers.get(name), value, `${what}: ${name}`);
      for (const name of ["Access-Control-Allow-Origin", "Access-Control-Allow-Credentials"])
        assert.equal(res.headers.get(name), null, `${what}: ${name}`);
    }
  });

  it("strips a CORS grant a handler set, and leaves a WebSocket upgrade alone", () => {
    const granted = withCrossOriginPolicy(
      new Response("{}", {
        headers: {
          "Access-Control-Allow-Origin": "*",
          "Access-Control-Allow-Credentials": "true",
          "Cross-Origin-Resource-Policy": "cross-origin",
        },
      }),
    );
    assert.equal(granted.headers.get("Access-Control-Allow-Origin"), null);
    assert.equal(granted.headers.get("Access-Control-Allow-Credentials"), null);
    assert.equal(granted.headers.get("Cross-Origin-Resource-Policy"), "same-origin");
    const pair = new WebSocketPair();
    const upgrade = new Response(null, { status: 101, webSocket: pair[0] });
    assert.equal(withCrossOriginPolicy(upgrade), upgrade);
  });

  it("agrees with the dashboard's own headers", () => {
    for (const [name, value] of Object.entries(SECURITY_HEADERS)) {
      const closed = CLOSED_HEADERS[name];
      if (closed !== undefined) assert.equal(value, closed, name);
    }
  });

  it("gives the static files the same closed set", () => {
    const block = HEADERS_FILE.split(/\n\s*\n/).find((b) => b.includes("/static/*\n")) ?? "";
    const rules = new Map(
      block
        .split("\n")
        .filter((line) => line.startsWith("  "))
        .map((line) => [
          line.slice(0, line.indexOf(":")).trim(),
          line.slice(line.indexOf(":") + 1).trim(),
        ]),
    );
    for (const [name, value] of Object.entries(CLOSED_HEADERS))
      assert.equal(rules.get(name), value, name);
    assert.ok(!/Access-Control-/i.test(HEADERS_FILE));
  });
});
