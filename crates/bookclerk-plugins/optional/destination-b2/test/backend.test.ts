/**
 * The B2 store against an in-memory B2: sign-in, small and large writes,
 * reads, listings, copies, deletes, checked writes, retries and the
 * notification rules.
 */

import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";

import { PluginError } from "./sdk.ts";

import { FakeB2, installDigestStream, sha256 } from "./fake-b2.ts";

const { B2Store, contentTypeHeader, normalizePrefix, partSizeFor, PART_REQUEST_BUDGET } =
  await import("../src/backend.ts");
const { B2SigningSecret } = await import("../src/b2-events.ts");
type B2Store = InstanceType<typeof B2Store>;

installDigestStream();

/** UTF-8 bytes. */
const bytes = (text: string): Uint8Array => new TextEncoder().encode(text);
/** A stream over bytes, delivered in chunks of `chunk`. */
function stream(data: Uint8Array, chunk = 3): ReadableStream<Uint8Array> {
  let at = 0;
  return new ReadableStream({
    pull(controller) {
      if (at >= data.length) {
        controller.close();
        return;
      }
      controller.enqueue(data.slice(at, at + chunk));
      at += chunk;
    },
  });
}
/** Reads a stream whole. */
async function drain(body: ReadableStream<Uint8Array>): Promise<string> {
  return new TextDecoder().decode(await new Response(body).arrayBuffer());
}
/** Hex to bytes. */
const unhex = (hex: string): Uint8Array =>
  Uint8Array.from(hex.match(/../g) ?? [], (h) => parseInt(h, 16));

/** Asserts a promise rejects with a plugin error code. */
async function rejectsWith(promise: Promise<unknown>, code: string, match?: RegExp): Promise<void> {
  await assert.rejects(promise, (err: unknown) => {
    assert.ok(err instanceof PluginError, String(err));
    assert.equal(err.code, code, err.message);
    if (match !== undefined) assert.match(err.message, match);
    return true;
  });
}

let b2: FakeB2;
/** A store over the fake, with five-byte parts. */
function store(
  prefix = "library/",
  extra: Partial<{ copyFileMaxBytes: number; copyPartBytes: number }> = {},
): B2Store {
  return new B2Store(
    { keyId: "kid", applicationKey: "secret", bucket: "books", prefix },
    { fetch: b2.fetch, sleep: () => Promise.resolve(), partFloor: 5, ...extra },
  );
}

beforeEach(() => {
  b2 = new FakeB2();
});

test("settings are checked before anything is sent", () => {
  const make = (
    s: Partial<{ keyId: string; bucket: string; prefix: string; authorizeUrl: string }>,
  ) =>
    new B2Store(
      { keyId: "kid", applicationKey: "k", bucket: "books", prefix: "", ...s },
      { fetch: b2.fetch },
    );
  assert.throws(() => make({ keyId: " " }), /key id is empty/);
  assert.throws(() => make({ bucket: "" }), /bucket is empty/);
  assert.throws(() => make({ prefix: "a//b" }), /B2 prefix/);
  assert.throws(() => make({ authorizeUrl: "http://api.example" }), /sign-in URL/);
  assert.equal(make({ prefix: "/x" }).prefix, "x/");
  assert.equal(make({ authorizeUrl: "https://api.backblazeb2.com/" }).prefixBytes, 0);
  assert.equal(b2.seen.length, 0);
});

test("helpers normalize prefixes, content types and part sizes", () => {
  assert.equal(normalizePrefix("  "), "");
  assert.equal(normalizePrefix("//a/b"), "a/b/");
  assert.equal(normalizePrefix("a/"), "a/");
  assert.equal(contentTypeHeader("audio/mp4"), "audio/mp4");
  assert.equal(contentTypeHeader("bad\ntype"), "b2/x-auto");
  assert.equal(contentTypeHeader(undefined), "b2/x-auto");
  assert.equal(contentTypeHeader(""), "b2/x-auto");
  assert.equal(partSizeFor(8, 5), 8);
  assert.equal(partSizeFor(8, 50), 50);
  const mib = 1024 * 1024;
  assert.equal(partSizeFor(8 * mib, 1, 8 * mib * PART_REQUEST_BUDGET), 8 * mib);
  assert.equal(partSizeFor(8 * mib, 1, 20 * mib * PART_REQUEST_BUDGET), 20 * mib);
  assert.throws(() => partSizeFor(6_000_000_000, 1), /part size/);
});

test("a small write goes up in one upload and reads back", async () => {
  const s = store();
  const body = bytes("hello");
  const written = await s.put("a b.m4b", stream(body), {
    contentType: "audio/mp4",
    contentLength: 5,
    sha256: unhex(sha256(body)),
    commitToken: "sc-1",
  });
  assert.equal(written.bytesWritten, 5);
  assert.deepEqual(written.sha256, unhex(sha256(body)));
  const v = b2.current("library/a b.m4b");
  assert.ok(v);
  assert.equal(written.etag, v.fileId);
  assert.deepEqual(v.fileInfo, { sha256: sha256(body), "commit-token": "sc-1" });
  assert.equal(v.contentType, "audio/mp4");
  assert.equal(b2.count("b2_authorize_account"), 1);
  assert.equal(b2.count("b2_list_buckets"), 1);

  const head = await s.head("a b.m4b");
  assert.ok(head);
  assert.equal(head.size, 5);
  assert.equal(head.commitToken, "sc-1");
  assert.equal(head.contentType, "audio/mp4");
  assert.deepEqual(head.sha256, unhex(sha256(body)));
  assert.equal(head.etag, v.fileId);
  assert.equal(head.modifiedAtMs, v.uploadTimestamp);
  assert.equal(await s.head("missing"), null);

  const whole = await s.get("a b.m4b", undefined);
  assert.equal(await drain(whole.body), "hello");
  const part = await s.get("a b.m4b", { offset: 1, length: 3 });
  assert.equal(await drain(part.body), "ell");
  const tail = await s.get("a b.m4b", { offset: 2 });
  assert.equal(await drain(tail.body), "llo");
  const end = await s.get("a b.m4b", { offset: 5 });
  assert.equal(await drain(end.body), "");
  await rejectsWith(s.get("a b.m4b", { offset: 9 }), "invalid_params");
  await rejectsWith(s.get("missing", undefined), "not_found");
  // The sign-in is reused.
  assert.equal(b2.count("b2_authorize_account"), 1);
});

test("an empty body is stored as an empty file", async () => {
  const s = store("");
  const written = await s.put("empty", stream(new Uint8Array(0)), {});
  assert.equal(written.bytesWritten, 0);
  const got = await s.get("empty", undefined);
  assert.equal(await drain(got.body), "");
  assert.equal(b2.current("empty")?.contentType, "b2/x-auto");
});

test("a long body becomes a large file sharing one part URL", async () => {
  const s = store();
  const body = bytes("abcdefghijklmnopq"); // 17 bytes: parts of 5, 5, 5, 2
  const written = await s.put("big", stream(body, 4), { contentLength: 17, commitToken: "sc-2" });
  assert.equal(written.bytesWritten, 17);
  const v = b2.current("library/big");
  assert.ok(v);
  assert.equal(new TextDecoder().decode(v.data), "abcdefghijklmnopq");
  assert.equal(v.fileInfo["commit-token"], "sc-2");
  assert.equal(b2.count("upload_part"), 4);
  assert.equal(b2.count("b2_get_upload_part_url"), 1);
  assert.deepEqual(written.sha256, unhex(sha256(body)));
});

test("a failed part upload fetches a new URL and retries", async () => {
  const s = store();
  b2.fail("upload_part", 503, 0);
  await s.put("big", stream(bytes("0123456789ab")), {});
  assert.equal(new TextDecoder().decode(b2.current("library/big")?.data), "0123456789ab");
  assert.equal(b2.count("b2_get_upload_part_url"), 3);
});

test("a body that differs from what was declared is refused and nothing stays", async () => {
  const s = store();
  await rejectsWith(
    s.put("small", stream(bytes("abc")), { sha256: new Uint8Array(32) }),
    "internal",
    /sha256/,
  );
  await rejectsWith(
    s.put("small", stream(bytes("abc")), { contentLength: 4 }),
    "internal",
    /declared 4/,
  );
  await rejectsWith(
    s.put("small", stream(bytes("abcdef")), { contentLength: 4 }),
    "internal",
    /declared length/,
  );
  assert.equal(b2.current("library/small"), undefined);
  await rejectsWith(
    s.put("big", stream(bytes("0123456789ab")), { sha256: new Uint8Array(32) }),
    "internal",
    /sha256/,
  );
  assert.equal(b2.files.get("library/big"), undefined, "the unfinished large file was cancelled");
  await rejectsWith(s.put("bad\\name", stream(bytes("x")), {}), "invalid_params");
});

test("a lost finish reply is settled by reading the file back", async () => {
  const s = store();
  // B2 finishes the file, then the reply is lost; the retry is refused
  // because the file is no longer unfinished.
  b2.drops.add("b2_finish_large_file");
  await s.put("big", stream(bytes("0123456789ab")), {});
  assert.ok(b2.count("b2_get_file_info") >= 1);
  assert.equal(b2.current("library/big")?.action, "upload");
});

test("an expired token signs in again once, and busy replies are retried", async () => {
  const s = store();
  await s.put("x", stream(bytes("1")), {});
  b2.token = "tok-2";
  await s.put("y", stream(bytes("2")), {});
  assert.equal(b2.count("b2_authorize_account"), 2);
  b2.fail("b2_get_upload_url", 503, 0);
  await s.put("z", stream(bytes("3")), {});
  b2.fail("b2_get_upload_url", 503, 503, 503, 503);
  await rejectsWith(s.put("w", stream(bytes("4")), {}), "internal", /503/);
  b2.fail("b2_get_upload_url", 400);
  await rejectsWith(s.put("w", stream(bytes("4")), {}), "internal", /400/);
  // A HEAD with an expired token reads as a bare 401.
  b2.token = "tok-3";
  assert.ok(await s.head("x"));
  assert.equal(b2.count("b2_authorize_account"), 3);
});

test("a restricted key must reach the bucket and prefix", async () => {
  b2.restrictBucket = "other";
  await rejectsWith(store().head("x"), "invalid_params", /only reaches bucket other/);
  b2.restrictBucket = "books";
  b2.namePrefix = "elsewhere/";
  await rejectsWith(store().head("x"), "invalid_params", /only reaches names under/);
  b2.namePrefix = "library/";
  assert.equal(await store().head("x"), null);
  b2.restrictBucket = null;
  b2.namePrefix = null;
  b2.bucket = "renamed";
  await rejectsWith(store().head("x"), "invalid_params", /does not exist/);
});

test("a failed sign-in is not kept", async () => {
  const s = new B2Store(
    { keyId: "kid", applicationKey: "wrong", bucket: "books", prefix: "" },
    { fetch: b2.fetch, sleep: () => Promise.resolve() },
  );
  await rejectsWith(s.head("x"), "internal", /sign-in/);
  await rejectsWith(s.head("x"), "internal", /sign-in/);
  assert.equal(b2.count("b2_authorize_account"), 2);
});

test("listing pages through stored names under the prefix", async () => {
  const s = store();
  b2.put("library/a", bytes("1"));
  b2.put("library/b", bytes("22"));
  b2.put("library/c", bytes("333"));
  b2.put("library/h", bytes(""), {}, "hide");
  b2.put("outside", bytes("x"));
  const first = await s.list("", undefined, 2);
  assert.deepEqual(
    first.objects.map((o) => [o.key, o.size]),
    [
      ["a", 1],
      ["b", 2],
    ],
  );
  assert.equal(first.nextCursor, "library/c");
  const second = await s.list("", first.nextCursor, 2);
  assert.deepEqual(
    second.objects.map((o) => o.key),
    ["c"],
  );
  assert.equal(second.nextCursor, undefined);
  await rejectsWith(s.list("", "outside", 2), "invalid_cursor");
  await rejectsWith(s.list("", "library/", 2), "invalid_cursor");
  const none = await s.list("zz", undefined, 10);
  assert.deepEqual(none.objects, []);
});

test("a delete removes every version, oldest first, and cancels unfinished files", async () => {
  const s = store();
  const old = b2.put("library/k", bytes("old"));
  b2.put("library/k", bytes(""), {}, "hide");
  b2.put("library/k", bytes("new"));
  b2.put("library/k", bytes(""), {}, "start");
  await s.delete("k");
  assert.equal(b2.files.get("library/k"), undefined);
  const deletes = b2.seen.filter(
    (r) => r.op === "b2_delete_file_version" || r.op === "b2_cancel_large_file",
  );
  assert.equal(deletes.length, 4);
  assert.equal(b2.seen.filter((r) => r.op === "b2_cancel_large_file").length, 1);
  void old;
  await s.delete("never-there");
});

test("copies run on B2 and keep the file info", async () => {
  const s = store("library/", { copyFileMaxBytes: 4, copyPartBytes: 3 });
  b2.put("library/small", bytes("abc"), { "commit-token": "t1", sha256: "00".repeat(32) });
  b2.put("library/large", bytes("0123456789"), { "commit-token": "t2" });
  assert.equal(await s.copy("small", "copy-small"), 3);
  assert.deepEqual(b2.current("library/copy-small")?.fileInfo, {
    "commit-token": "t1",
    sha256: "00".repeat(32),
  });
  assert.equal(await s.copy("large", "copy-large"), 10);
  assert.equal(new TextDecoder().decode(b2.current("library/copy-large")?.data), "0123456789");
  assert.equal(b2.current("library/copy-large")?.fileInfo["commit-token"], "t2");
  assert.equal(b2.count("b2_copy_part"), 4);
  await rejectsWith(s.copy("missing", "x"), "not_found");
  b2.fail("b2_copy_part", 400);
  await rejectsWith(s.copy("large", "copy-fail"), "internal");
  assert.equal(b2.files.get("library/copy-fail"), undefined, "the unfinished copy was cancelled");
});

test("checked writes withdraw a write that lost", async () => {
  const s = store();
  const first = await s.putConditional("k", stream(bytes("one")), {}, { kind: "ifNoneMatch" });
  assert.ok(first.etag);
  await rejectsWith(
    s.putConditional("k", stream(bytes("two")), {}, { kind: "ifNoneMatch" }),
    "conflict",
    /already exists/,
  );
  assert.equal(new TextDecoder().decode(b2.current("library/k")?.data), "one");
  const second = await s.putConditional(
    "k",
    stream(bytes("two")),
    {},
    { kind: "ifMatch", etag: first.etag },
  );
  assert.equal(new TextDecoder().decode(b2.current("library/k")?.data), "two");
  await rejectsWith(
    s.putConditional("k", stream(bytes("three")), {}, { kind: "ifMatch", etag: first.etag }),
    "conflict",
    /changed since/,
  );
  assert.equal(b2.current("library/k")?.fileId, second.etag);
  // A hide marker counts as absent.
  b2.put("library/hidden", bytes(""), {}, "hide");
  await s.putConditional("hidden", stream(bytes("x")), {}, { kind: "ifNoneMatch" });
  // A version listing that fails withdraws the write too.
  b2.fail("b2_list_file_versions", 400);
  await rejectsWith(
    s.putConditional("gone", stream(bytes("x")), {}, { kind: "ifNoneMatch" }),
    "internal",
  );
  assert.equal(b2.current("library/gone"), undefined);
});

test("notification rules are kept beside the bucket's other rules", async () => {
  const s = store();
  const secret = B2SigningSecret.parse("abcdefghijklmnopqrstuvwxyz012345");
  assert.ok(secret instanceof B2SigningSecret);
  b2.rules = [
    {
      name: "theirs",
      eventTypes: ["b2:ObjectCreated:*"],
      isEnabled: true,
      objectNamePrefix: "",
      targetConfiguration: { targetType: "webhook", url: "https://x.example/h" },
    },
  ];
  assert.equal(await s.keepNotificationRules("https://lib.example/hook", secret), true);
  assert.equal(b2.rules.length, 4);
  assert.equal(await s.keepNotificationRules("https://lib.example/hook", secret), false);
  assert.equal(await s.keepNotificationRules("https://lib.example/moved", secret), true);
  assert.equal(b2.rules.length, 4);
  assert.equal(await s.dropNotificationRules(), true);
  assert.deepEqual(
    b2.rules.map((r) => r["name"]),
    ["theirs"],
  );
  assert.equal(await s.dropNotificationRules(), false);
});

test("concurrent calls share one sign-in and one refresh", async () => {
  const s = store();
  await Promise.all([s.head("a"), s.head("b"), s.list("", undefined, 5)]);
  assert.equal(b2.count("b2_authorize_account"), 1);
  b2.token = "tok-2";
  await Promise.all([s.head("a"), s.head("b"), s.list("", undefined, 5)]);
  assert.equal(b2.count("b2_authorize_account"), 2);
});
