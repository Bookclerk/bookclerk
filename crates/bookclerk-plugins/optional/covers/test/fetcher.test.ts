/**
 * Unit tests for the cover fetcher against a scripted `fetch` and sink.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  CONCURRENCY,
  CoverFailure,
  MAX_BYTES,
  MAX_HEADER_BYTES,
  MAX_REDIRECTS,
  MAX_URLS,
  capped,
  checkParams,
  checkUrl,
  declaredLength,
  fetchCovers,
  fetchOne,
  headerValueOk,
  maybeImage,
  passHeaders,
  type CoverSink,
  type Fetch,
  type FetcherOptions,
  type PutOptions,
} from "../src/fetcher.ts";
import { VERSION } from "../src/version.ts";

/** One body the scripted sink stored. */
interface Stored {
  /** Sink key. */
  key: string;
  /** Body bytes. */
  bytes: Uint8Array;
  /** Put options. */
  options: PutOptions | undefined;
}

/**
 * A sink that reads each body in full, as the host's does.
 *
 * @param fail - Makes every put reject with this message when set.
 * @returns The sink and what it stored.
 */
function recordingSink(fail?: string): { sink: CoverSink; stored: Stored[] } {
  const stored: Stored[] = [];
  const sink: CoverSink = {
    async put(key, body, options) {
      const bytes = new Uint8Array(await new Response(body).arrayBuffer());
      if (fail !== undefined) throw new Error(fail);
      stored.push({ key, bytes, options });
      return { key };
    },
  };
  return { sink, stored };
}

/**
 * Options whose `fetch` answers per URL and records each request.
 *
 * @param reply - Builds the response for a URL, or throws.
 * @returns The options and the requests seen.
 */
function scripted(reply: (url: string) => Response): {
  options: FetcherOptions;
  seen: { url: string; init: RequestInit }[];
} {
  const seen: { url: string; init: RequestInit }[] = [];
  const fetch: Fetch = (url, init) => {
    seen.push({ url, init });
    return Promise.resolve().then(() => reply(url));
  };
  return { options: { fetch, version: VERSION }, seen };
}

/**
 * A JPEG-typed response with `bytes` as its body.
 *
 * @param bytes - Body.
 * @param headers - Extra headers.
 * @returns The response.
 */
function image(bytes: Uint8Array<ArrayBuffer>, headers: Record<string, string> = {}): Response {
  return new Response(bytes, { headers: { "content-type": "image/jpeg", ...headers } });
}

test("the plugin version matches plugin.toml", () => {
  const manifest = readFileSync(new URL("../plugin.toml", import.meta.url), "utf8");
  assert.ok(manifest.split(/\r?\n/).includes(`version = "${VERSION}"`));
});

test("checkParams accepts the ABI bounds only", () => {
  assert.deepEqual(checkParams({ urls: ["https://a.example/"], maxBytes: 1 }), {
    urls: ["https://a.example/"],
    maxBytes: 1,
    conditions: [],
  });
  const full = Array.from({ length: MAX_URLS }, (_, i) => `https://a.example/${i}`);
  assert.equal(checkParams({ urls: full, maxBytes: MAX_BYTES }).urls.length, MAX_URLS);
  for (const bad of [
    undefined,
    null,
    {},
    { urls: [], maxBytes: 1 },
    { urls: [...full, "https://a.example/x"], maxBytes: 1 },
    { urls: [1], maxBytes: 1 },
    { urls: "https://a.example/", maxBytes: 1 },
    { urls: ["https://a.example/"], maxBytes: 0 },
    { urls: ["https://a.example/"], maxBytes: MAX_BYTES + 1 },
    { urls: ["https://a.example/"], maxBytes: 1.5 },
    { urls: ["https://a.example/"], maxBytes: "1" },
    { urls: ["https://a.example/"], maxBytes: 1, conditions: "x" },
    { urls: ["https://a.example/"], maxBytes: 1, conditions: [{}, {}] },
    { urls: ["https://a.example/"], maxBytes: 1, conditions: [{ ifNoneMatch: 1 }] },
    { urls: ["https://a.example/"], maxBytes: 1, conditions: [{ ifNoneMatch: '"a"\r\nx: y' }] },
    {
      urls: ["https://a.example/"],
      maxBytes: 1,
      conditions: [{ ifModifiedSince: "x".repeat(MAX_HEADER_BYTES + 1) }],
    },
  ]) {
    assert.throws(() => checkParams(bad), CoverFailure, JSON.stringify(bad));
  }
});

test("checkUrl accepts https hostnames only", () => {
  assert.equal(
    checkUrl("https://covers.openlibrary.org/b/id/1-L.jpg"),
    "https://covers.openlibrary.org/b/id/1-L.jpg",
  );
  assert.equal(
    checkUrl("https://m.media-amazon.com/i/x.jpg?a=1"),
    "https://m.media-amazon.com/i/x.jpg?a=1",
  );
  for (const bad of [
    "http://a.example/x.jpg",
    "ftp://a.example/x.jpg",
    "https://a.example:8443/x.jpg",
    "https://user:pw@a.example/x.jpg",
    "https://127.0.0.1/x.jpg",
    "https://0x7f.0.0.1/x.jpg",
    "https://2130706433/x.jpg",
    "https://[::1]/x.jpg",
    "https://a.example/x y.jpg",
    "ht\ttps://a.example/x.jpg",
    "/relative.jpg",
    "not a url",
    `https://a.example/${"x".repeat(4100)}`,
  ]) {
    assert.equal(checkUrl(bad), undefined, bad);
  }
});

test("declaredLength trusts only plain, unencoded lengths", () => {
  assert.equal(declaredLength(new Headers({ "content-length": "1234" })), 1234);
  assert.equal(declaredLength(new Headers({ "content-length": " 7 " })), 7);
  assert.equal(declaredLength(new Headers()), undefined);
  assert.equal(declaredLength(new Headers({ "content-length": "-1" })), undefined);
  assert.equal(declaredLength(new Headers({ "content-length": "1e3" })), undefined);
  assert.equal(declaredLength(new Headers({ "content-length": "9".repeat(16) })), undefined);
  assert.equal(
    declaredLength(new Headers({ "content-length": "10", "content-encoding": "gzip" })),
    undefined,
  );
});

test("maybeImage refuses text-like types", () => {
  for (const ok of [
    null,
    "",
    "image/jpeg",
    "IMAGE/WEBP; q=1",
    "application/octet-stream",
    "binary/octet-stream",
  ]) {
    assert.equal(maybeImage(ok), true, String(ok));
  }
  for (const bad of ["text/html", "application/json", "text/plain; charset=utf-8", "image"]) {
    assert.equal(maybeImage(bad), false, bad);
  }
});

test("capped passes small bodies and fails large ones", async () => {
  const fits = { exceeded: false };
  const ok = await new Response(capped(new Response("abcd").body!, 4, fits)).text();
  assert.equal(ok, "abcd");
  assert.equal(fits.exceeded, false);
  const over = { exceeded: false };
  await assert.rejects(new Response(capped(new Response("abcde").body!, 4, over)).text());
  assert.equal(over.exceeded, true);
});

test("fetchCovers stores bodies under their index, in request order", async () => {
  const bodies: Record<string, Uint8Array<ArrayBuffer>> = {
    "https://a.example/0.jpg": new Uint8Array([0xff, 0xd8, 0xff, 0]),
    "https://b.example/2.png": new Uint8Array([0x89, 0x50, 0x4e, 0x47]),
  };
  const { options, seen } = scripted((url) => {
    const body = bodies[url];
    if (body !== undefined) return image(body, { "content-length": String(body.byteLength) });
    return new Response("missing", { status: 404 });
  });
  const { sink, stored } = recordingSink();
  const images = await fetchCovers(
    options,
    {
      urls: ["https://a.example/0.jpg", "https://a.example/gone.jpg", "https://b.example/2.png"],
      maxBytes: 64,
    },
    sink,
  );
  assert.deepEqual(images, [
    { url: "https://a.example/0.jpg", stored: true, notModified: false, contentType: "image/jpeg" },
    { url: "https://a.example/gone.jpg", stored: false, notModified: false, error: "HTTP 404" },
    { url: "https://b.example/2.png", stored: true, notModified: false, contentType: "image/jpeg" },
  ]);
  assert.deepEqual(stored.map((s) => [s.key, [...s.bytes]]).sort(), [
    ["0", [0xff, 0xd8, 0xff, 0]],
    ["2", [0x89, 0x50, 0x4e, 0x47]],
  ]);
  assert.deepEqual(stored.find((s) => s.key === "0")?.options, {
    contentType: "image/jpeg",
    contentLength: 4,
  });
  const headers = new Headers(seen[0]?.init.headers);
  assert.equal(headers.get("accept"), "image/*");
  assert.equal(headers.get("user-agent"), `Bookclerk-Covers/${VERSION} (+https://bookclerk.app)`);
  assert.ok(seen[0]?.init.signal instanceof AbortSignal);
});

test("fetchCovers keeps at most CONCURRENCY requests in flight", async () => {
  let inFlight = 0;
  let peak = 0;
  const options: FetcherOptions = {
    version: VERSION,
    fetch: async () => {
      inFlight += 1;
      peak = Math.max(peak, inFlight);
      await new Promise((r) => setTimeout(r, 2));
      inFlight -= 1;
      return image(new Uint8Array([1]));
    },
  };
  const urls = Array.from({ length: 10 }, (_, i) => `https://a.example/${i}`);
  const images = await fetchCovers(options, { urls, maxBytes: 8 }, recordingSink().sink);
  assert.equal(images.length, 10);
  assert.ok(images.every((i) => i.stored));
  assert.equal(peak, CONCURRENCY);
});

test("fetchCovers refuses malformed params before any request", async () => {
  const { options, seen } = scripted(() => image(new Uint8Array([1])));
  await assert.rejects(
    fetchCovers(options, { urls: [], maxBytes: 1 }, recordingSink().sink),
    CoverFailure,
  );
  assert.equal(seen.length, 0);
});

test("fetchOne turns every failure into an answer", async () => {
  const { sink, stored } = recordingSink();
  const one = (reply: () => Response, url = "https://a.example/x.jpg", max = 4) =>
    fetchOne(scripted(reply).options, url, 0, max, sink);

  assert.equal(
    (await one(() => image(new Uint8Array([1])), "http://a.example/x.jpg")).error,
    "not an https URL on a public hostname",
  );
  assert.equal(
    (
      await one(() => {
        throw new TypeError("network down");
      })
    ).error,
    "request failed: network down",
  );
  assert.equal((await one(() => new Response("", { status: 503 }))).error, "HTTP 503");
  assert.equal(
    (await one(() => new Response("<html>", { headers: { "content-type": "text/html" } }))).error,
    "not an image (text/html)",
  );
  assert.equal(
    (await one(() => image(new Uint8Array(8), { "content-length": "8" }))).error,
    "body exceeds 4 bytes",
  );
  assert.equal((await one(() => image(new Uint8Array(8)))).error, "body exceeds 4 bytes");
  assert.equal(
    (await one(() => new Response(null, { status: 200, headers: { "content-type": "image/png" } })))
      .error,
    "empty body",
  );
  assert.equal(stored.length, 0, "nothing reached the sink");

  const failing = recordingSink("sink full");
  const refused = await fetchOne(
    scripted(() => image(new Uint8Array([1]))).options,
    "https://a.example/x.jpg",
    0,
    4,
    failing.sink,
  );
  assert.equal(refused.error, "store failed: sink full");

  const untyped = await fetchOne(
    scripted(() => new Response(new Uint8Array([1, 2]))).options,
    "https://a.example/x",
    3,
    4,
    sink,
  );
  assert.deepEqual(untyped, { url: "https://a.example/x", stored: true, notModified: false });
  assert.deepEqual(stored[0]?.options, {});
});

test("fetchOne follows redirects itself and vets every hop", async () => {
  const redirect = (location: string | null, status = 302): Response => {
    const headers = new Headers();
    if (location !== null) headers.set("location", location);
    return new Response(null, { status, headers });
  };
  const run = async (reply: (url: string) => Response) => {
    const { options, seen } = scripted(reply);
    const { sink, stored } = recordingSink();
    const answer = await fetchOne(options, "https://a.example/start", 0, 8, sink);
    return { answer, seen, stored };
  };

  const followed = await run((url) =>
    url === "https://a.example/start" ? redirect("/img.png", 301) : image(new Uint8Array([7])),
  );
  assert.deepEqual(followed.answer, {
    url: "https://a.example/start",
    stored: true,
    notModified: false,
    contentType: "image/jpeg",
  });
  assert.deepEqual(
    followed.seen.map((s) => [s.url, s.init.redirect]),
    [
      ["https://a.example/start", "manual"],
      ["https://a.example/img.png", "manual"],
    ],
  );
  assert.deepEqual([...(followed.stored[0]?.bytes ?? [])], [7]);

  for (const location of [
    "http://a.example/img.png",
    "https://a.example:8443/img.png",
    "https://127.0.0.1/img.png",
    "https://[",
  ]) {
    const refused = await run(() => redirect(location));
    assert.equal(
      refused.answer.error,
      "redirect to a URL that is not https on a public hostname",
      location,
    );
    assert.equal(refused.seen.length, 1, location);
    assert.equal(refused.stored.length, 0, location);
  }

  assert.equal((await run(() => redirect(null, 307))).answer.error, "HTTP 307 without a location");

  const loop = await run(() => redirect("/again"));
  assert.equal(loop.answer.error, "too many redirects");
  assert.equal(loop.seen.length, MAX_REDIRECTS + 1);
});

test("checkParams keeps conditions and drops empty values", () => {
  assert.deepEqual(
    checkParams({
      urls: ["https://a.example/1", "https://a.example/2"],
      maxBytes: 1,
      conditions: [{ ifNoneMatch: '"abc"', ifModifiedSince: "" }, null],
    }).conditions,
    [{ ifNoneMatch: '"abc"' }, {}],
  );
});

test("headerValueOk allows visible ASCII up to the cap only", () => {
  assert.equal(headerValueOk('W/"abc", "d e"\t'), true);
  assert.equal(headerValueOk("x".repeat(MAX_HEADER_BYTES)), true);
  assert.equal(headerValueOk("x".repeat(MAX_HEADER_BYTES + 1)), false);
  assert.equal(headerValueOk("a\nb"), false);
  assert.equal(headerValueOk("caf\u00e9"), false);
});

test("passHeaders copies the freshness headers that fit", () => {
  const image = { url: "u", stored: true, notModified: false };
  passHeaders(
    new Headers({
      etag: '"v1"',
      "last-modified": "Tue, 01 Sep 2026 00:00:00 GMT",
      "cache-control": "max-age=60",
      expires: "Wed, 02 Sep 2026 00:00:00 GMT",
      date: "Tue, 01 Sep 2026 00:00:00 GMT",
      age: "5",
      "x-other": "ignored",
    }),
    image,
  );
  assert.deepEqual(image, {
    url: "u",
    stored: true,
    notModified: false,
    etag: '"v1"',
    lastModified: "Tue, 01 Sep 2026 00:00:00 GMT",
    cacheControl: "max-age=60",
    expires: "Wed, 02 Sep 2026 00:00:00 GMT",
    date: "Tue, 01 Sep 2026 00:00:00 GMT",
    age: "5",
  });
  const long = { url: "u", stored: true, notModified: false };
  passHeaders(new Headers({ etag: `"${"x".repeat(MAX_HEADER_BYTES)}"`, age: "" }), long);
  assert.deepEqual(long, { url: "u", stored: true, notModified: false });
});

test("fetchOne sends the conditions and reports a 304 without storing", async () => {
  const { options, seen } = scripted(
    () =>
      new Response(null, { status: 304, headers: { etag: '"v1"', "cache-control": "max-age=9" } }),
  );
  const { sink, stored } = recordingSink();
  const answer = await fetchOne(options, "https://a.example/x.jpg", 0, 8, sink, {
    ifNoneMatch: '"v1"',
    ifModifiedSince: "Tue, 01 Sep 2026 00:00:00 GMT",
  });
  assert.deepEqual(answer, {
    url: "https://a.example/x.jpg",
    stored: false,
    notModified: true,
    etag: '"v1"',
    cacheControl: "max-age=9",
  });
  assert.equal(stored.length, 0);
  const headers = new Headers(seen[0]?.init.headers);
  assert.equal(headers.get("if-none-match"), '"v1"');
  assert.equal(headers.get("if-modified-since"), "Tue, 01 Sep 2026 00:00:00 GMT");
});

test("fetchCovers pairs each condition with its URL and passes headers on", async () => {
  const { options, seen } = scripted(() =>
    image(new Uint8Array([1]), { etag: '"new"', "last-modified": "Mon, 31 Aug 2026 00:00:00 GMT" }),
  );
  const images = await fetchCovers(
    options,
    {
      urls: ["https://a.example/1", "https://a.example/2"],
      maxBytes: 8,
      conditions: [{ ifNoneMatch: '"old"' }],
    },
    recordingSink().sink,
  );
  assert.equal(images[0]?.etag, '"new"');
  assert.equal(images[1]?.lastModified, "Mon, 31 Aug 2026 00:00:00 GMT");
  const sent = (url: string) =>
    new Headers(seen.find((s) => s.url === url)?.init.headers).get("if-none-match");
  assert.equal(sent("https://a.example/1"), '"old"');
  assert.equal(sent("https://a.example/2"), null);
});
