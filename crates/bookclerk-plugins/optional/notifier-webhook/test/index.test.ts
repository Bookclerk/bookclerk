/**
 * Unit tests for the entrypoint glue, with the SDK's isolate module replaced
 * by stand-ins (the real one imports `cloudflare:workers`, which only exists
 * inside workerd).
 */

import assert from "node:assert/strict";
import { mock, test } from "node:test";

import type { NotificationMessage, NotifySendParams } from "@bookclerk/plugin-sdk/workerd";

/** Stand-in for the SDK's `PluginError`. */
class PluginError extends Error {
  /** Wire code. */
  readonly code: string;
  /** Detail object. */
  readonly details: Record<string, unknown> | undefined;

  /**
   * Creates an error.
   *
   * @param code - Wire code.
   * @param message - Message.
   * @param details - Detail object.
   */
  constructor(code: string, message: string, details?: Record<string, unknown>) {
    super(message);
    this.code = code;
    this.details = details;
  }
}

/** Stand-in for the SDK's entrypoint base classes. */
class Entrypoint {
  /** Bindings. */
  readonly env: unknown;

  /**
   * Keeps the bindings, as `WorkerEntrypoint` does.
   *
   * @param _ctx - Execution context (unused).
   * @param env - Bindings.
   */
  constructor(_ctx: unknown, env: unknown) {
    this.env = env;
  }
}

mock.module("@bookclerk/plugin-sdk/workerd", {
  namedExports: { BookclerkEntrypoint: Entrypoint, NotifierEntrypoint: Entrypoint, PluginError },
});
const { Notifier, default: Plugin } = await import("../src/index.ts");

/** A 32-byte Standard Webhooks secret. */
const SECRET = `whsec_${Buffer.alloc(32, 3).toString("base64")}`;

/** Requests the stubbed global `fetch` saw. */
const seen: { url: string; init: RequestInit | undefined }[] = [];
/** What the stubbed global `fetch` does next. */
let reply: () => Response | Promise<Response> = () => new Response(null, { status: 204 });
globalThis.fetch = (input: string | URL | Request, init?: RequestInit) => {
  seen.push({ url: String(input), init });
  return Promise.resolve().then(reply);
};

/**
 * A notifier with these settings.
 *
 * @param url - The `url` setting.
 * @param secret - The `signing_secret` secret.
 * @returns The entrypoint.
 */
function notifier(url: string | undefined, secret?: string) {
  return new Notifier({} as never, {
    CONFIG: { url },
    SECRETS: secret === undefined ? {} : { signing_secret: secret },
  });
}

/** A rendered message. */
const MESSAGE: NotificationMessage = {
  notificationId: "n-7",
  notificationType: "test",
  severity: "info",
  title: "Test",
  text: "Hello",
  fields: [],
  alertAction: "none",
  sensitive: false,
};

/**
 * Params for a channel send.
 *
 * @returns The params.
 */
function channelSend(): NotifySendParams {
  return {
    message: MESSAGE,
    destination: { kind: "channel", destinationId: "d1", addressJson: "" },
  };
}

/**
 * Asserts `promise` rejects with a `PluginError` of `code`.
 *
 * @param promise - The call.
 * @param code - Expected wire code.
 * @returns The error.
 */
async function rejectsWith(promise: Promise<unknown>, code: string): Promise<PluginError> {
  let caught: PluginError | undefined;
  await assert.rejects(promise, (err: unknown) => {
    assert.ok(err instanceof PluginError);
    assert.equal(err.code, code);
    caught = err;
    return true;
  });
  assert.ok(caught);
  return caught;
}

test("describe names the plugin", async () => {
  assert.deepEqual(await new Plugin({} as never, {} as never).describe(), {
    displayName: "Webhook",
  });
});

test("health checks the channel settings without a request", async () => {
  seen.length = 0;
  assert.equal((await notifier(undefined).health()).ok, true);
  assert.deepEqual(await notifier("https://hooks.example.com/", SECRET).health(), {
    ok: true,
    detail: "",
  });
  const broken = await notifier("https://hooks.example.com/").health();
  assert.equal(broken.ok, false);
  assert.match(broken.detail, /signing_secret/);
  assert.equal(seen.length, 0);
});

test("a channel send posts one signed request without following redirects", async () => {
  seen.length = 0;
  reply = () => new Response(null, { status: 204 });
  assert.deepEqual(await notifier("https://hooks.example.com/in", SECRET).send(channelSend()), {});
  assert.equal(seen.length, 1);
  const [call] = seen;
  assert.equal(call?.url, "https://hooks.example.com/in");
  assert.equal(call?.init?.method, "POST");
  assert.equal(call?.init?.redirect, "manual");
  const headers = new Headers(call?.init?.headers);
  assert.equal(headers.get("webhook-id"), "msg_n-7_d1");
  assert.match(headers.get("webhook-signature") ?? "", /^v1,[A-Za-z0-9+/]{43}=$/);
});

test("a personal send posts to the patron's URL", async () => {
  seen.length = 0;
  reply = () => new Response("ok", { status: 200 });
  await notifier(undefined).send({
    message: MESSAGE,
    destination: {
      kind: "personal",
      destinationId: "p1",
      addressJson: JSON.stringify({ url: "https://me.example.net/hook", secret: SECRET }),
    },
  });
  assert.equal(seen[0]?.url, "https://me.example.net/hook");
});

test("failures carry their codes and the receiver's wait", async () => {
  await rejectsWith(notifier(undefined).send(channelSend()), "invalid_params");
  reply = () => new Response(null, { status: 429, headers: { "retry-after": "7" } });
  const limited = await rejectsWith(
    notifier("https://hooks.example.com/", SECRET).send(channelSend()),
    "rate_limited",
  );
  assert.deepEqual(limited.details, { retryAfterMs: 7000 });
  reply = () => new Response(null, { status: 502 });
  const down = await rejectsWith(
    notifier("https://hooks.example.com/", SECRET).send(channelSend()),
    "unavailable",
  );
  assert.equal(down.details, undefined);
  reply = () => Promise.reject(new TypeError("connection refused"));
  await rejectsWith(
    notifier("https://hooks.example.com/", SECRET).send(channelSend()),
    "unavailable",
  );
});

test("validateDestination normalises the address and labels it by host", async () => {
  seen.length = 0;
  assert.deepEqual(
    await notifier(undefined).validateDestination({
      addressJson: JSON.stringify({ url: " https://me.example.net/hook", secret: SECRET }),
    }),
    {
      addressJson: JSON.stringify({ url: "https://me.example.net/hook", secret: SECRET }),
      label: "me.example.net",
    },
  );
  await rejectsWith(
    notifier(undefined).validateDestination({ addressJson: "{}" }),
    "invalid_params",
  );
  assert.equal(seen.length, 0);
});

test("other errors pass through unchanged", async () => {
  const broken = new Notifier({} as never, {
    get CONFIG(): never {
      throw new RangeError("unexpected");
    },
    SECRETS: {},
  });
  await assert.rejects(broken.send(channelSend()), RangeError);
  assert.throws(() => broken.health(), RangeError);
  const noSecrets = new Notifier({} as never, {
    CONFIG: { url: "https://hooks.example.com/" },
    get SECRETS(): never {
      throw new RangeError("unexpected");
    },
  });
  assert.throws(() => noSecrets.health(), RangeError);
});
