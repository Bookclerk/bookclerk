// Error handling of the authenticated routes: a failed upstream call comes
// back as JSON naming the stage, never as Cloudflare's opaque error 1101.

import assert from "node:assert/strict";
import { afterEach, describe, it } from "node:test";
import worker, { UpstreamError } from "./worker.js";

const KEY = "k".repeat(32);
const ENV = {
  REPORT_API_KEY: KEY,
  B2_KEY_ID: "id",
  B2_APPLICATION_KEY: "secret-application-key",
  B2_BUCKET_ID: "bucket",
};
const realFetch = globalThis.fetch;
const realError = console.error;

afterEach(() => {
  globalThis.fetch = realFetch;
  console.error = realError;
});

/**
 * Replaces fetch with a router over URL substrings.
 *
 * @param {Record<string, () => Response>} routes Substring to response.
 */
function stubFetch(routes) {
  globalThis.fetch = async (input) => {
    const url = String(input);
    for (const [part, respond] of Object.entries(routes)) {
      if (url.includes(part)) return respond();
    }
    throw new Error(`unexpected fetch ${url}`);
  };
}

function report(headers = {}) {
  return new Request("https://collector.test/report?since=0", { headers });
}

describe("worker errors", () => {
  it("names the failed B2 stage to a caller with the report key", async () => {
    const logged = [];
    console.error = (line) => logged.push(line);
    stubFetch({
      "releases/latest": () => new Response("{}", { status: 404 }),
      b2_authorize_account: () =>
        Response.json({ apiUrl: "https://b2.test", downloadUrl: "https://d.test", authorizationToken: "t" }),
      b2_list_file_names: () => Response.json({ code: "unauthorized", message: "no listFiles" }, { status: 401 }),
    });
    const res = await worker.fetch(report({ authorization: `Bearer ${KEY}` }), ENV);
    assert.equal(res.status, 502);
    assert.deepEqual(await res.json(), { error: "upstream failed", detail: "b2 list failed: HTTP 401: unauthorized" });
    assert.equal(logged.length, 1);
    assert.ok(!logged[0].includes(ENV.B2_APPLICATION_KEY));
  });

  it("names a rate-limited GitHub lookup", async () => {
    console.error = () => {};
    stubFetch({ "releases/latest": () => new Response("{}", { status: 403 }) });
    const env = { ...ENV, GITHUB_REPOSITORY: "Bookclerk/bookclerk" };
    const res = await worker.fetch(report({ authorization: `Bearer ${KEY}` }), env);
    assert.equal(res.status, 502);
    assert.equal((await res.json()).detail, "github releases/latest failed: HTTP 403");
  });

  it("keeps B2 codes that are not short identifiers out of the detail", () => {
    assert.equal(new UpstreamError("b2 list", 400, "<b>x</b>").message, "b2 list failed: HTTP 400");
  });

  it("gives /submit callers no upstream detail", async () => {
    console.error = () => {};
    stubFetch({ b2_authorize_account: () => new Response("{}", { status: 503 }) });
    const body = JSON.stringify({ version: "0.1.0", trigger: "test", os: "linux", events: [] });
    const res = await worker.fetch(
      new Request("https://collector.test/submit", { method: "POST", body, headers: { "content-type": "application/json" } }),
      ENV,
    );
    assert.equal(res.status, 502);
    assert.deepEqual(await res.json(), { error: "upstream failed" });
  });

  it("still refuses /report without the key", async () => {
    const res = await worker.fetch(report(), ENV);
    assert.equal(res.status, 401);
  });
});
