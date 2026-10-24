// The route list against the API description: every operation in
// crates/bookclerkd/openapi.json must be classified, the table must not name
// operations the API lacks, and the routes that would let a visitor acquire,
// download, link nodes, install plugins or change sign-in stay refused.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { describe, it } from "node:test";
import { classify, OPERATIONS } from "../src/routes.js";

/** Every (method, path) in the API description. */
function apiOperations() {
  const spec = JSON.parse(readFileSync(new URL("../../../crates/bookclerkd/openapi.json", import.meta.url), "utf8"));
  const out = [];
  for (const [path, item] of Object.entries(spec.paths)) {
    for (const method of ["get", "put", "post", "delete", "patch", "head", "options", "trace"]) {
      if (item[method]) out.push(`${method.toUpperCase()} ${path}`);
    }
  }
  return out.sort();
}

describe("route list", () => {
  it("classifies exactly the API's operations", () => {
    const listed = OPERATIONS.map(([m, p]) => `${m} ${p}`).sort();
    const api = apiOperations();
    const missing = api.filter((op) => !listed.includes(op));
    const extra = listed.filter((op) => !api.includes(op));
    assert.deepEqual(missing, [], `classify these new operations in src/routes.js: ${missing.join(", ")}`);
    assert.deepEqual(extra, [], `these operations are no longer in the API: ${extra.join(", ")}`);
    assert.equal(new Set(listed).size, listed.length, "an operation is listed twice");
  });

  it("matches each listed operation to itself", () => {
    for (const [method, template, verdict] of OPERATIONS) {
      const path = template.replace(/\{[^}]+\}/g, "x1");
      const got = classify(method, path);
      assert.equal(got.verdict, verdict, `${method} ${template}`);
    }
  });

  it("keeps acquire, downloads, nodes, installs and sign-in changes refused", () => {
    const refused = [
      ["POST", "/api/library/acquire"],
      ["POST", "/acquire"],
      ["POST", "/scan"],
      ["POST", "/api/config/plugins"],
      ["POST", "/api/config/plugins/k/versions/1/approve"],
      ["PATCH", "/api/settings"],
      ["PUT", "/api/config/domains/core.output"],
      ["POST", "/api/config/plugin-instances"],
      ["PUT", "/api/config/plugin-instances/1/config"],
      ["POST", "/api/node/v1/enroll"],
      ["GET", "/api/node/v1/connect"],
      ["POST", "/api/config/join/start"],
      ["POST", "/api/config/join/inspect"],
      ["POST", "/api/auth/tray-handoff/prepare"],
      ["POST", "/api/auth/bootstrap"],
      ["POST", "/api/auth/login"],
      ["POST", "/api/auth/logout"],
      ["POST", "/api/portal/logout"],
      ["POST", "/api/portal/redeem"],
      ["POST", "/api/users"],
      ["POST", "/api/users/1/claim-ticket"],
      ["PUT", "/api/auth/oidc/config"],
      ["POST", "/api/events/deliveries/1/retry"],
      ["POST", "/oidc/token"],
    ];
    for (const [m, p] of refused) assert.equal(classify(m, p).verdict, "deny", `${m} ${p}`);
  });

  it("counts every store sign-in start", () => {
    for (const p of [
      "/api/portal/audible/start",
      "/api/portal/libro/login",
      "/api/portal/sources/chirp/login",
      "/api/portal/sources/audible/oauth/start",
    ]) {
      assert.equal(classify("POST", p).verdict, "signin", p);
    }
  });

  it("refuses encoded and dotted spellings of refused routes", () => {
    for (const p of [
      "/api/library/%61cquire",
      "/api/library/acquire%2F",
      "/api/library/books/../acquire",
      "/api/library/books/%2e%2e/acquire",
      "/api//library/acquire",
      "/api/library/acquire/",
      "/API/library/acquire",
      "/api/library/books/a%2Fb",
      "/api/library/books/%",
      "/api/library/books/%00",
    ]) {
      assert.equal(classify("POST", p).verdict, "deny", p);
      assert.equal(classify("GET", p).verdict, "deny", p);
    }
  });

  it("serves the web app's pages and files, read-only", () => {
    for (const p of ["/", "/library", "/accounts", "/assets/index-abc.js", "/favicon.ico", "/join/approve"]) {
      assert.equal(classify("GET", p).verdict, "allow", p);
      assert.equal(classify("HEAD", p).verdict, "allow", p);
      assert.equal(classify("POST", p).verdict, "deny", p);
    }
  });

  it("refuses API paths the description does not list", () => {
    for (const p of ["/api/unknown", "/api/library", "/oidc/other", "/.well-known/x", "/integrations/a/b"]) {
      assert.equal(classify("GET", p).verdict, "deny", p);
    }
    assert.equal(classify("OPTIONS", "/api/status").verdict, "deny");
    assert.equal(classify("GET", "relative").verdict, "deny");
  });
});
