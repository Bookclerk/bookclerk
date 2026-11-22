/**
 * The relay hub and the plugin's request handling, end to end under Node:
 * fake sockets carry the hub's notices to `serveRequest`, whose pulls and
 * pushes go straight to the hub, and a fake `ORIGIN` stands in for the
 * library.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import type { OriginBinding, OriginFetchOptions } from "@bookclerk/plugin-sdk/workerd";
import { RelayHub, framed, publicHead, readFrame, type RelaySocket } from "../src/relay-core.ts";
import {
  RELAY_HEALTH_PATH,
  RELAY_PULL_PATH,
  RELAY_PUSH_PATH,
  encodeFrame,
  parseNotice,
} from "../src/protocol.ts";
import { responseHead, serveRequest, type Fetch, type ServeOutcome } from "../src/serve.ts";

/** The relay's token in these tests. */
const TOKEN = "relay-token-0123456789abcdef";

/** The relay's host in these tests. */
const HOST = "bookclerk-relay.example.workers.dev";

/**
 * A fetch that sends every request to the hub.
 *
 * @param hub - The hub.
 * @returns The fetch.
 */
function hubFetch(hub: RelayHub): Fetch {
  return (input, init) => hub.fetch(new Request(input, init));
}

/** What the fake library saw. */
interface Seen {
  /** Method. */
  method: string;
  /** Path and query. */
  target: string;
  /** Request body as text. */
  body: string;
  /** Client address the plugin reported. */
  client: string | undefined;
  /** Request headers. */
  headers: Headers;
}

/**
 * A fake library that echoes the request.
 *
 * @param seen - Collects each request.
 * @param answer - Builds the answer; echoes the body by default.
 * @returns The binding.
 */
function library(
  seen: Seen[],
  answer?: (request: Request, body: string) => Response | Promise<Response>,
): OriginBinding {
  return {
    async fetch(request: Request, options?: OriginFetchOptions): Promise<Response> {
      const body = await request.text();
      const url = new URL(request.url);
      seen.push({
        method: request.method,
        target: `${url.pathname}${url.search}`,
        body,
        client: options?.clientAddress,
        headers: request.headers,
      });
      if (answer) return answer(request, body);
      return new Response(`echo:${body}`, {
        status: 201,
        headers: { "x-library": "yes", "content-type": "text/plain" },
      });
    },
  };
}

/**
 * A plugin socket: each notice it receives is served like the plugin's
 * message handler does.
 *
 * @param hub - The hub to pull from and push to.
 * @param origin - The library.
 * @param outcomes - Collects each outcome.
 * @returns The socket.
 */
function pluginSocket(hub: RelayHub, origin: OriginBinding, outcomes: ServeOutcome[]): RelaySocket {
  return {
    send(text: string): void {
      const notice = parseNotice(text);
      assert.notEqual(typeof notice, "string", `bad notice ${text}`);
      if (typeof notice === "string") return;
      void serveRequest({ fetch: hubFetch(hub), origin, host: HOST, token: TOKEN }, notice.id).then(
        (outcome) => outcomes.push(outcome),
      );
    },
  };
}

test("a public request reaches the library and its answer comes back", async () => {
  const hub = new RelayHub(TOKEN, { queueMs: 1000, headMs: 1000 });
  const seen: Seen[] = [];
  const outcomes: ServeOutcome[] = [];
  hub.attach(pluginSocket(hub, library(seen), outcomes));
  const response = await hub.fetch(
    new Request("https://lib.example/api/node/v1/hello?x=1", {
      method: "POST",
      body: "payload",
      headers: {
        "cf-connecting-ip": "203.0.113.7",
        "x-forwarded-for": "198.51.100.1",
        "content-type": "text/plain",
        cookie: "a=b",
      },
    }),
  );
  assert.equal(response.status, 201);
  assert.equal(await response.text(), "echo:payload");
  assert.equal(response.headers.get("x-library"), "yes");
  assert.equal(seen.length, 1);
  const first = seen[0];
  assert.ok(first);
  assert.equal(first.method, "POST");
  assert.equal(first.target, "/api/node/v1/hello?x=1");
  assert.equal(first.client, "203.0.113.7");
  assert.equal(first.headers.get("x-forwarded-for"), null);
  assert.equal(first.headers.get("cf-connecting-ip"), null);
  assert.equal(first.headers.get("cookie"), "a=b");
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.deepEqual(outcomes, ["served"]);
});

test("requests queue for a free socket and are served in turn", async () => {
  const hub = new RelayHub(TOKEN, { queueMs: 2000, headMs: 2000 });
  const seen: Seen[] = [];
  const outcomes: ServeOutcome[] = [];
  hub.attach(pluginSocket(hub, library(seen), outcomes));
  // Each client reads its answer as it arrives: a socket stays busy until
  // the answer's body has been passed on.
  const answers = await Promise.all(
    [1, 2, 3].map((n) =>
      hub
        .fetch(new Request(`https://lib.example/n/${n}`, { method: "PUT", body: `b${n}` }))
        .then((a) => a.text()),
    ),
  );
  assert.deepEqual(answers, ["echo:b1", "echo:b2", "echo:b3"]);
});

test("a HEAD or 204 answer carries no body", async () => {
  const hub = new RelayHub(TOKEN);
  const seen: Seen[] = [];
  hub.attach(
    pluginSocket(
      hub,
      library(seen, () => new Response(null, { status: 204 })),
      [],
    ),
  );
  const empty = await hub.fetch(new Request("https://lib.example/x", { method: "DELETE" }));
  assert.equal(empty.status, 204);
  assert.equal(await empty.text(), "");
  const head = await hub.fetch(new Request("https://lib.example/x", { method: "HEAD" }));
  assert.equal(head.status, 204);
});

test("the relay answers 503 with no plugin socket, 501 for an upgrade and 400 for a bad header", async () => {
  const hub = new RelayHub(TOKEN);
  assert.equal((await hub.fetch(new Request("https://lib.example/"))).status, 503);
  hub.attach({ send() {} });
  const upgrade = await hub.fetch(
    new Request("https://lib.example/socket", { headers: { upgrade: "websocket" } }),
  );
  assert.equal(upgrade.status, 501);
  const bad = await hub.fetch(
    new Request("https://lib.example/", { headers: { "x-odd": "café" } }),
  );
  assert.equal(bad.status, 400);
});

test("a request no socket takes in time gets 503, and one never answered gets 504", async () => {
  const busy = new RelayHub(TOKEN, { queueMs: 20, headMs: 60 });
  // The socket takes the first notice and never answers it.
  const notices: string[] = [];
  busy.attach({ send: (text) => notices.push(text) });
  const [first, second] = await Promise.all([
    busy.fetch(new Request("https://lib.example/one")),
    busy.fetch(new Request("https://lib.example/two")),
  ]);
  assert.ok(first && second);
  assert.equal(first.status, 504);
  assert.equal(second.status, 503);
  assert.ok(notices.length >= 1);
});

test("a push whose head is still arriving at the 504 is cut off instead of hanging", async () => {
  const hub = new RelayHub(TOKEN, { queueMs: 1000, headMs: 40 });
  const auth = { authorization: `Bearer ${TOKEN}` };
  let push: Promise<Response> | undefined;
  hub.attach({
    send(text: string) {
      const notice = parseNotice(text);
      if (typeof notice === "string") return;
      void (async () => {
        const pulled = await hub.fetch(
          new Request(`https://${HOST}${RELAY_PULL_PATH}${notice.id}`, { headers: auth }),
        );
        await pulled.body?.cancel();
        // Start the push, but hold the head back past the wait.
        const head = encodeFrame({ status: 200, headers: [] });
        let sent = false;
        const body = new ReadableStream<Uint8Array>({
          async pull(controller) {
            if (sent) {
              controller.enqueue(new TextEncoder().encode("late body"));
              return;
            }
            sent = true;
            await new Promise((resolve) => setTimeout(resolve, 80));
            controller.enqueue(head);
          },
        });
        push = hub.fetch(
          new Request(`https://${HOST}${RELAY_PUSH_PATH}${notice.id}`, {
            method: "POST",
            body,
            headers: auth,
            duplex: "half",
          } as RequestInit),
        );
      })();
    },
  });
  const response = await hub.fetch(new Request("https://lib.example/"));
  assert.equal(response.status, 504);
  await response.body?.cancel();
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.ok(push);
  assert.equal((await push).status, 400);
});

test("requests that time out while every socket is busy leave the queue", async () => {
  const hub = new RelayHub(TOKEN, { queueMs: 20, headMs: 5000 });
  const auth = { authorization: `Bearer ${TOKEN}` };
  // The only socket takes the first notice and never answers it.
  hub.attach({ send: () => {} });
  const first = hub.fetch(new Request("https://lib.example/held"));
  const waited = await Promise.all([
    hub.fetch(new Request("https://lib.example/a")),
    hub.fetch(new Request("https://lib.example/b")),
  ]);
  assert.deepEqual(
    waited.map((r) => r.status),
    [503, 503],
  );
  const health = await hub.fetch(
    new Request(`https://${HOST}${RELAY_HEALTH_PATH}`, { headers: auth }),
  );
  assert.deepEqual(await health.json(), { sockets: 1, waiting: 0 });
  void first;
});

test("a socket that closes mid-request answers 502, and detach frees the request", async () => {
  const hub = new RelayHub(TOKEN, { queueMs: 1000, headMs: 1000 });
  const socket: RelaySocket = { send: () => queueMicrotask(() => hub.detach(socket)) };
  hub.attach(socket);
  const response = await hub.fetch(new Request("https://lib.example/"));
  assert.equal(response.status, 502);
  assert.equal(hub.sockets, 0);
});

test("a socket whose send throws is dropped", async () => {
  const hub = new RelayHub(TOKEN, { queueMs: 30, headMs: 30 });
  hub.attach({
    send() {
      throw new Error("closed");
    },
  });
  const response = await hub.fetch(new Request("https://lib.example/"));
  assert.equal(response.status, 502);
  assert.equal(hub.sockets, 0);
});

test("relay paths need the token and refuse unknown or reused ids", async () => {
  const hub = new RelayHub(TOKEN);
  const auth = { authorization: `Bearer ${TOKEN}` };
  assert.equal((await hub.fetch(new Request(`https://${HOST}${RELAY_HEALTH_PATH}`))).status, 401);
  assert.equal(
    (
      await hub.fetch(
        new Request(`https://${HOST}${RELAY_HEALTH_PATH}`, {
          headers: { authorization: "Bearer wrong-token-0123456789" },
        }),
      )
    ).status,
    401,
  );
  const health = await hub.fetch(
    new Request(`https://${HOST}${RELAY_HEALTH_PATH}`, { headers: auth }),
  );
  assert.deepEqual(await health.json(), { sockets: 0, waiting: 0 });
  assert.equal(
    (await hub.fetch(new Request(`https://${HOST}${RELAY_PULL_PATH}nope`, { headers: auth })))
      .status,
    404,
  );
  assert.equal(
    (await hub.fetch(new Request(`https://${HOST}${RELAY_PULL_PATH}../x`, { headers: auth })))
      .status,
    404,
  );
  assert.equal(
    (
      await hub.fetch(
        new Request(`https://${HOST}${RELAY_PUSH_PATH}nope`, {
          method: "POST",
          body: "x",
          headers: auth,
        }),
      )
    ).status,
    404,
  );
  assert.equal(
    (await hub.fetch(new Request(`https://${HOST}/__bookclerk/relay/other`, { headers: auth })))
      .status,
    404,
  );
});

test("a malformed push answers the client 502 and the plugin 400", async () => {
  const hub = new RelayHub(TOKEN, { queueMs: 1000, headMs: 1000 });
  const auth = { authorization: `Bearer ${TOKEN}` };
  const pushes: Response[] = [];
  hub.attach({
    send(text: string) {
      const notice = parseNotice(text);
      if (typeof notice === "string") return;
      void (async () => {
        const pulled = await hub.fetch(
          new Request(`https://${HOST}${RELAY_PULL_PATH}${notice.id}`, { headers: auth }),
        );
        await pulled.body?.cancel();
        // A second pull of the same request is refused.
        const again = await hub.fetch(
          new Request(`https://${HOST}${RELAY_PULL_PATH}${notice.id}`, { headers: auth }),
        );
        assert.equal(again.status, 404);
        pushes.push(
          await hub.fetch(
            new Request(`https://${HOST}${RELAY_PUSH_PATH}${notice.id}`, {
              method: "POST",
              body: framed(new TextEncoder().encode("\u0000\u0000\u0000\u0002{}"), null),
              headers: auth,
              duplex: "half",
            } as RequestInit),
          ),
        );
      })();
    },
  });
  const response = await hub.fetch(new Request("https://lib.example/"));
  assert.equal(response.status, 502);
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(pushes[0]?.status, 400);
});

test("a library failure becomes a 502 for the client", async () => {
  const hub = new RelayHub(TOKEN);
  const outcomes: ServeOutcome[] = [];
  const failing: OriginBinding = {
    fetch: () => Promise.reject(new Error("down")),
  };
  hub.attach(pluginSocket(hub, failing, outcomes));
  const response = await hub.fetch(new Request("https://lib.example/"));
  assert.equal(response.status, 502);
  assert.match(await response.text(), /library_unavailable/);
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.deepEqual(outcomes, ["library_failed"]);
});

test("serveRequest reports a request that is gone or a relay that is unreachable", async () => {
  const origin = library([]);
  const gone: Fetch = () => Promise.resolve(new Response("no", { status: 404 }));
  assert.equal(
    await serveRequest({ fetch: gone, origin, host: HOST, token: TOKEN }, "abc"),
    "gone",
  );
  const down: Fetch = () => Promise.reject(new Error("offline"));
  assert.equal(
    await serveRequest({ fetch: down, origin, host: HOST, token: TOKEN }, "abc"),
    "gone",
  );
});

test("serveRequest answers a malformed pulled frame or head with an error push", async () => {
  const origin = library([]);
  const pushed: number[] = [];
  /**
   * A relay whose pull returns `body` and whose push records the status.
   *
   * @param body - The pull's bytes.
   * @returns The fetch.
   */
  const relay =
    (body: Uint8Array<ArrayBuffer>): Fetch =>
    async (input, init) => {
      if (input.includes(RELAY_PULL_PATH)) return new Response(body);
      const frame = await readFrame(new Request(input, init).body as ReadableStream<Uint8Array>);
      assert.notEqual(typeof frame, "string");
      if (typeof frame !== "string")
        pushed.push((JSON.parse(frame.json) as { status: number }).status);
      return new Response(null, { status: 204 });
    };
  assert.equal(
    await serveRequest(
      { fetch: relay(new Uint8Array([0, 0, 0, 9, 1])), origin, host: HOST, token: TOKEN },
      "a",
    ),
    "bad_request",
  );
  const badHead = encodeFrame({ method: "GET", target: "no-slash", headers: [] });
  assert.equal(
    await serveRequest({ fetch: relay(badHead), origin, host: HOST, token: TOKEN }, "b"),
    "bad_request",
  );
  assert.deepEqual(pushed, [502, 400]);
});

test("serveRequest reports a push the relay refused", async () => {
  const origin = library([]);
  const head = encodeFrame({ method: "GET", target: "/", headers: [] });
  const relay: Fetch = async (input) =>
    input.includes(RELAY_PULL_PATH) ? new Response(head) : new Response("gone", { status: 404 });
  assert.equal(
    await serveRequest({ fetch: relay, origin, host: HOST, token: TOKEN }, "a"),
    "push_failed",
  );
  const throwing: Fetch = async (input) => {
    if (input.includes(RELAY_PULL_PATH)) return new Response(head);
    throw new Error("reset");
  };
  assert.equal(
    await serveRequest({ fetch: throwing, origin, host: HOST, token: TOKEN }, "a"),
    "push_failed",
  );
});

test("responseHead keeps each Set-Cookie and maps informational statuses to 502", () => {
  const headers = new Headers();
  headers.append("set-cookie", "a=1");
  headers.append("set-cookie", "b=2");
  headers.append("connection", "close");
  headers.append("x-one", "1");
  const head = responseHead(new Response("x", { status: 200, headers }));
  assert.deepEqual(
    head.headers.filter((f) => f.name === "set-cookie").map((f) => f.value),
    ["a=1", "b=2"],
  );
  assert.equal(
    head.headers.some((f) => f.name === "connection"),
    false,
  );
  assert.equal(responseHead({ status: 101, headers: new Headers() } as Response).status, 502);
});

test("publicHead drops provider headers and keeps a valid client address only", () => {
  const head = publicHead(
    new Request("https://lib.example/a?b", {
      headers: {
        "cf-connecting-ip": "not-an-ip",
        "cf-ray": "1",
        host: "lib.example",
        accept: "*/*",
      },
    }),
  );
  assert.notEqual(typeof head, "string");
  if (typeof head === "string") return;
  assert.equal(head.client, undefined);
  assert.deepEqual(head.headers, [{ name: "accept", value: "*/*" }]);
});

test("readFrame reports a stream that fails inside the head", async () => {
  const failing = new ReadableStream<Uint8Array>({
    pull(controller) {
      controller.error(new Error("reset"));
    },
  });
  assert.equal(typeof (await readFrame(failing)), "string");
  const short = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(new Uint8Array([0, 0]));
      controller.close();
    },
  });
  assert.equal(await readFrame(short), "stream ended inside the frame head");
});
