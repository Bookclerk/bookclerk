/**
 * Unit tests for the Standard Webhooks planning and parsing in `webhook.ts`.
 */

import assert from "node:assert/strict";
import { createHmac } from "node:crypto";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import type { NotificationMessage } from "@bookclerk/plugin-sdk/workerd";
import { VERSION } from "../src/version.ts";
import {
  MAX_RETRY_AFTER_MS,
  MAX_URL_CHARS,
  WebhookFailure,
  channelEndpoint,
  checkResponse,
  parseAddress,
  parseRetryAfter,
  parseSecret,
  parseUrl,
  payload,
  plan,
  sign,
  webhookId,
} from "../src/webhook.ts";

/** A 32-byte key as a Standard Webhooks secret. */
const SECRET = `whsec_${Buffer.alloc(32, 7).toString("base64")}`;

/**
 * A rendered message.
 *
 * @param extra - Fields to override.
 * @returns The message.
 */
function message(extra: Partial<NotificationMessage> = {}): NotificationMessage {
  return {
    notificationId: "n-1",
    notificationType: "host:scan_failed",
    severity: "warning",
    title: "Scan failed",
    text: "The scan stopped.",
    fields: [{ name: "Account", value: "libro-1" }],
    alertAction: "none",
    sensitive: false,
    ...extra,
  };
}

/**
 * Asserts `call` fails with `code`.
 *
 * @param call - The call.
 * @param code - Expected wire code.
 */
function failsWith(call: () => unknown, code: string): void {
  assert.throws(call, (err: unknown) => {
    assert.ok(err instanceof WebhookFailure);
    assert.equal(err.code, code);
    return true;
  });
}

test("the version matches plugin.toml", () => {
  const manifest = readFileSync(new URL("../plugin.toml", import.meta.url), "utf8");
  assert.match(manifest, new RegExp(`^version = "${VERSION.replaceAll(".", "\\.")}"$`, "m"));
});

test("signatures match the Standard Webhooks reference vector", async () => {
  // From the reference libraries' tests.
  const key = parseSecret("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw", "secret");
  assert.equal(
    await sign(key, "msg_p5jXN8AQM9LWM0D4loKWxJek", 1614265330, '{"test": 2432232314}'),
    "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=",
  );
});

test("a planned request is signed over its id, timestamp and body", async () => {
  const endpoint = channelEndpoint(" https://hooks.example.com/in ", SECRET);
  const now = new Date("2026-11-23T05:00:00.250Z");
  const request = await plan(endpoint, message(), "dest-9", now, VERSION);
  assert.equal(request.url, "https://hooks.example.com/in");
  assert.equal(request.headers["webhook-id"], "msg_n-1_dest-9");
  assert.equal(request.headers["webhook-timestamp"], "1795410000");
  assert.equal(request.headers["content-type"], "application/json");
  assert.equal(request.headers["user-agent"], `Bookclerk-Webhook/${VERSION}`);
  const expected = createHmac("sha256", Buffer.alloc(32, 7))
    .update(`msg_n-1_dest-9.1795410000.${request.body}`)
    .digest("base64");
  assert.equal(request.headers["webhook-signature"], `v1,${expected}`);
  assert.deepEqual(JSON.parse(request.body), {
    type: "host:scan_failed",
    timestamp: "2026-11-23T05:00:00.250Z",
    data: {
      id: "n-1",
      type: "host:scan_failed",
      severity: "warning",
      title: "Scan failed",
      text: "The scan stopped.",
      fields: [{ name: "Account", value: "libro-1" }],
      alertAction: "none",
    },
  });
});

test("optional bodies, link, group and alert key reach the payload", () => {
  const body = JSON.parse(
    payload(
      message({
        markdown: "**Scan** failed",
        html: "<p>Scan failed</p>",
        link: "https://library.example.com/status",
        groupKey: "scan",
        alertAction: "trigger",
        alertKey: "scan:libro-1",
      }),
      new Date(0),
    ),
  ) as { data: Record<string, unknown> };
  assert.equal(body.data["markdown"], "**Scan** failed");
  assert.equal(body.data["html"], "<p>Scan failed</p>");
  assert.equal(body.data["link"], "https://library.example.com/status");
  assert.equal(body.data["groupKey"], "scan");
  assert.equal(body.data["alertAction"], "trigger");
  assert.equal(body.data["alertKey"], "scan:libro-1");
});

test("the webhook id differs per destination and repeats per retry", () => {
  assert.equal(webhookId("n", "a"), webhookId("n", "a"));
  assert.notEqual(webhookId("n", "a"), webhookId("n", "b"));
});

test("URLs are http or https with a host and no credentials or fragment", () => {
  assert.equal(parseUrl("http://10.0.0.5:8123/api/webhook/x", "u").host, "10.0.0.5:8123");
  failsWith(() => parseUrl("  ", "u"), "invalid_params");
  failsWith(() => parseUrl("not a url", "u"), "invalid_params");
  failsWith(() => parseUrl("ftp://example.com/", "u"), "invalid_params");
  failsWith(() => parseUrl("https://user:pw@example.com/", "u"), "invalid_params");
  failsWith(() => parseUrl("https://example.com/#x", "u"), "invalid_params");
  failsWith(() => parseUrl("https://example.com/#", "u"), "invalid_params");
  failsWith(
    () => parseUrl(`https://example.com/${"a".repeat(MAX_URL_CHARS)}`, "u"),
    "invalid_params",
  );
});

test("errors never repeat the URL or the secret", () => {
  for (const call of [
    () => parseUrl("https://user:token123@example.com/", "the url setting"),
    () => parseSecret("whsec_token123", "the signing secret"),
  ]) {
    assert.throws(call, (err: unknown) => {
      assert.ok(err instanceof Error);
      assert.doesNotMatch(err.message, /token123/);
      return true;
    });
  }
});

test("secrets are whsec_ and 24 to 64 bytes of base64", () => {
  assert.equal(parseSecret(SECRET, "s").length, 32);
  assert.equal(parseSecret(SECRET.replace(/=+$/, ""), "s").length, 32);
  failsWith(() => parseSecret(Buffer.alloc(32).toString("base64"), "s"), "invalid_params");
  failsWith(() => parseSecret("whsec_!!!!", "s"), "invalid_params");
  failsWith(() => parseSecret("whsec_A", "s"), "invalid_params");
  failsWith(
    () => parseSecret(`whsec_${Buffer.alloc(23).toString("base64")}`, "s"),
    "invalid_params",
  );
  failsWith(
    () => parseSecret(`whsec_${Buffer.alloc(65).toString("base64")}`, "s"),
    "invalid_params",
  );
});

test("a channel needs both settings", () => {
  failsWith(() => channelEndpoint(undefined, SECRET), "invalid_params");
  failsWith(() => channelEndpoint("https://example.com/", undefined), "invalid_params");
  failsWith(() => channelEndpoint("https://example.com/", " "), "invalid_params");
});

test("addresses are normalised and labelled by host", () => {
  const address = parseAddress(
    JSON.stringify({ url: " https://hooks.example.com/me ", secret: ` ${SECRET} ` }),
  );
  assert.equal(address.label, "hooks.example.com");
  assert.deepEqual(JSON.parse(address.addressJson), {
    url: "https://hooks.example.com/me",
    secret: SECRET,
  });
  for (const bad of [
    "nope",
    "[]",
    "null",
    JSON.stringify({ url: "https://example.com/" }),
    JSON.stringify({ secret: SECRET }),
    JSON.stringify({ url: 1, secret: SECRET }),
    JSON.stringify({ url: "https://example.com/", secret: SECRET, extra: "x" }),
    JSON.stringify({ url: "https://example.com/", secret: "x".repeat(5000) }),
  ]) {
    failsWith(() => parseAddress(bad), "invalid_params");
  }
});

test("Retry-After takes delta seconds or an IMF date, capped", () => {
  const now = Date.UTC(2026, 10, 23, 5, 0, 0);
  assert.equal(parseRetryAfter(null, now), undefined);
  assert.equal(parseRetryAfter(" 30 ", now), 30_000);
  assert.equal(parseRetryAfter("999999", now), MAX_RETRY_AFTER_MS);
  assert.equal(parseRetryAfter("Mon, 23 Nov 2026 05:01:00 GMT", now), 60_000);
  assert.equal(parseRetryAfter("Mon, 23 Nov 2026 04:00:00 GMT", now), 0);
  assert.equal(parseRetryAfter("Tue, 31 Nov 2026 05:01:00 GMT", now), undefined);
  assert.equal(parseRetryAfter("Mon, 23 Nov 2026 25:01:00 GMT", now), undefined);
  assert.equal(parseRetryAfter("-5", now), undefined);
  assert.equal(parseRetryAfter("soon", now), undefined);
});

test("responses map to delivered, retried or failed", () => {
  checkResponse(200, null, 0);
  checkResponse(204, null, 0);
  assert.throws(
    () => checkResponse(429, "12", 0),
    (err: unknown) =>
      err instanceof WebhookFailure && err.code === "rate_limited" && err.retryAfterMs === 12_000,
  );
  for (const [status, code] of [
    [408, "unavailable"],
    [425, "unavailable"],
    [500, "unavailable"],
    [503, "unavailable"],
    [301, "invalid_params"],
    [401, "forbidden"],
    [403, "forbidden"],
    [404, "not_found"],
    [410, "not_found"],
    [400, "invalid_params"],
    [422, "invalid_params"],
    [100, "invalid_params"],
  ] as const) {
    failsWith(() => checkResponse(status, null, 0), code);
  }
});
