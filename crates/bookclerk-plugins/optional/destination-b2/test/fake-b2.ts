/**
 * An in-memory Backblaze B2 behind a `fetch` function, for the store's
 * tests: the native API calls the store makes, versions kept per name, and
 * knobs to fail, expire or drop replies on purpose.
 */

import { createHash } from "node:crypto";

/** One stored version. */
export interface FakeVersion {
  /** Version id. */
  fileId: string;
  /** File name. */
  fileName: string;
  /** What the version records. */
  action: "upload" | "copy" | "hide" | "start";
  /** Body (empty for hide and start). */
  data: Uint8Array;
  /** Content type. */
  contentType: string;
  /** File info. */
  fileInfo: Record<string, string>;
  /** Upload time. */
  uploadTimestamp: number;
  /** Uploaded parts of an unfinished large file, by number. */
  parts?: Map<number, Uint8Array>;
}

/** A request the fake saw. */
export interface SeenRequest {
  /** Operation name or `download` / `upload`. */
  op: string;
  /** Method. */
  method: string;
  /** Headers, lower-case names. */
  headers: Record<string, string>;
}

/** Installs `crypto.DigestStream` (workerd's) over Node's hashes, for `Sha256Stream`. */
export function installDigestStream(): void {
  const target = crypto as unknown as { DigestStream?: unknown };
  if (target.DigestStream !== undefined) return;
  class DigestStream extends WritableStream<Uint8Array> {
    readonly digest: Promise<ArrayBuffer>;
    constructor(algorithm: string) {
      const hash = createHash(algorithm.replace("-", "").toLowerCase());
      let done: (value: ArrayBuffer) => void = () => undefined;
      const digest = new Promise<ArrayBuffer>((resolve) => {
        done = resolve;
      });
      super({
        write(chunk) {
          hash.update(chunk);
        },
        close() {
          const out = hash.digest();
          done(out.buffer.slice(out.byteOffset, out.byteOffset + out.byteLength));
        },
      });
      this.digest = digest;
    }
  }
  target.DigestStream = DigestStream;
}

/**
 * Lower-case hex SHA-1.
 *
 * @param data - Bytes.
 * @returns The digest.
 */
export function sha1(data: Uint8Array): string {
  return createHash("sha1").update(data).digest("hex");
}

/**
 * Lower-case hex SHA-256.
 *
 * @param data - Bytes.
 * @returns The digest.
 */
export function sha256(data: Uint8Array): string {
  return createHash("sha256").update(data).digest("hex");
}

/**
 * B2's file name encoding, decoded.
 *
 * @param text - The encoded name.
 * @returns The name.
 */
function decode(text: string): string {
  return decodeURIComponent(text.replace(/\+/g, " "));
}

/**
 * B2's file name encoding.
 *
 * @param text - The name.
 * @returns The encoding.
 */
function encode(text: string): string {
  return encodeURIComponent(text).replace(/%2F/g, "/");
}

/** The fake service. */
export class FakeB2 {
  /** Versions by name, newest first. */
  readonly files = new Map<string, FakeVersion[]>();
  /** Every request, in order. */
  readonly seen: SeenRequest[] = [];
  /** The bucket's notification rules. */
  rules: Record<string, unknown>[] = [];
  /** Bucket name. */
  bucket = "books";
  /** Restrict the key to this bucket, when set. */
  restrictBucket: string | null = null;
  /** Restrict the key to this name prefix, when set. */
  namePrefix: string | null = null;
  /** B2's minimum part size. */
  minimumPart = 1;
  /** Account token currently valid. */
  token = "tok-1";
  /** Failures to answer next, by operation, as HTTP statuses (code from `failCodes`). */
  readonly failures = new Map<string, number[]>();
  /** Operations whose next success is answered and then dropped (the store sees a network error). */
  readonly drops = new Set<string>();
  /** Versions a write races in right after each upload, by name (another writer). */
  readonly raceAfterUpload = new Map<string, Uint8Array>();
  /** Next id. */
  #next = 1;
  /** Clock. */
  #now = 1_700_000_000_000;

  /** The fake's `fetch`. */
  readonly fetch = async (input: string, init: RequestInit): Promise<Response> =>
    this.#handle(input, init);

  /**
   * The current version of `name`, ignoring unfinished large files.
   *
   * @param name - File name.
   * @returns The version, or `undefined`.
   */
  current(name: string): FakeVersion | undefined {
    return this.files.get(name)?.find((v) => v.action !== "start");
  }

  /**
   * Stores a version directly.
   *
   * @param name - File name.
   * @param data - Body.
   * @param fileInfo - File info.
   * @param action - What it records.
   * @returns The version.
   */
  put(
    name: string,
    data: Uint8Array,
    fileInfo: Record<string, string> = {},
    action: FakeVersion["action"] = "upload",
  ): FakeVersion {
    const version: FakeVersion = {
      fileId: `id${this.#next++}`,
      fileName: name,
      action,
      data,
      contentType: "application/octet-stream",
      fileInfo,
      uploadTimestamp: this.#now++,
    };
    this.files.set(name, [version, ...(this.files.get(name) ?? [])]);
    return version;
  }

  /**
   * Counts requests of one operation.
   *
   * @param op - Operation.
   * @returns How many.
   */
  count(op: string): number {
    return this.seen.filter((r) => r.op === op).length;
  }

  /**
   * Queues failures for an operation.
   *
   * @param op - Operation.
   * @param statuses - Statuses to answer, in order.
   */
  fail(op: string, ...statuses: number[]): void {
    this.failures.set(op, [...(this.failures.get(op) ?? []), ...statuses]);
  }

  /** The JSON of a version. */
  #wire(v: FakeVersion): Record<string, unknown> {
    return {
      fileId: v.fileId,
      fileName: v.fileName,
      contentLength: v.data.length,
      contentSha1: v.action === "start" || v.action === "hide" ? "none" : sha1(v.data),
      contentType: v.contentType,
      uploadTimestamp: v.uploadTimestamp,
      action: v.action,
      fileInfo: v.fileInfo,
    };
  }

  /** A JSON reply. */
  #json(value: unknown, status = 200): Response {
    return new Response(JSON.stringify(value), {
      status,
      headers: { "content-type": "application/json" },
    });
  }

  /** An error reply. */
  #error(status: number, code: string, message = code): Response {
    return this.#json({ status, code, message }, status);
  }

  /** Answers one request. */
  async #handle(input: string, init: RequestInit): Promise<Response> {
    const url = new URL(input);
    const method = init.method ?? "GET";
    const headers: Record<string, string> = {};
    for (const [k, v] of Object.entries((init.headers ?? {}) as Record<string, string>))
      headers[k.toLowerCase()] = v;
    const body =
      init.body === undefined || init.body === null
        ? new Uint8Array(0)
        : new Uint8Array(await new Response(init.body).arrayBuffer());
    let op: string;
    if (url.pathname.startsWith("/file/")) op = "download";
    else if (url.pathname.startsWith("/upload/"))
      op = url.pathname.split("/")[2] === "part" ? "upload_part" : "upload_file";
    else op = url.pathname.split("/").pop() ?? "";
    this.seen.push({ op, method, headers });
    const queued = this.failures.get(op);
    if (queued !== undefined && queued.length > 0) {
      const status = queued.shift() as number;
      if (status === 0) throw new TypeError("network error");
      if (status === 401) return this.#error(401, "expired_auth_token");
      return this.#error(status, status === 503 ? "service_unavailable" : "bad_request");
    }
    const reply = this.#route(op, url, method, headers, body);
    if (this.drops.delete(op)) {
      throw new TypeError("connection reset");
    }
    return reply;
  }

  /** Routes one request to its operation. */
  #route(
    op: string,
    url: URL,
    method: string,
    headers: Record<string, string>,
    body: Uint8Array,
  ): Response {
    if (op === "b2_authorize_account") {
      if (headers["authorization"] !== `Basic ${btoa("kid:secret")}`)
        return this.#error(401, "unauthorized");
      return this.#json({
        accountId: "acct",
        authorizationToken: this.token,
        apiInfo: {
          storageApi: {
            apiUrl: "https://api001.backblazeb2.com",
            downloadUrl: "https://f001.backblazeb2.com",
            recommendedPartSize: 100_000_000,
            absoluteMinimumPartSize: this.minimumPart,
            bucketId: this.restrictBucket === null ? null : "bid",
            bucketName: this.restrictBucket,
            namePrefix: this.namePrefix,
            capabilities: ["listFiles"],
          },
        },
      });
    }
    const auth = headers["authorization"];
    if (op === "upload_file" || op === "upload_part") return this.#upload(op, url, headers, body);
    if (auth !== this.token) return this.#error(401, "expired_auth_token");
    if (op === "download") return this.#download(url, method, headers);
    const args =
      method === "GET"
        ? Object.fromEntries(url.searchParams)
        : (JSON.parse(new TextDecoder().decode(body)) as Record<string, unknown>);
    switch (op) {
      case "b2_list_buckets":
        return this.#json({
          buckets: [
            { bucketId: "bid", bucketName: this.bucket },
            { bucketId: "other", bucketName: "other" },
          ],
        });
      case "b2_get_upload_url":
        return this.#json({
          uploadUrl: `https://pod-1.backblaze.com/upload/file/${this.#next++}`,
          authorizationToken: "up-tok",
        });
      case "b2_get_upload_part_url":
        return this.#json({
          uploadUrl: `https://pod-1.backblaze.com/upload/part/${String(args["fileId"])}/${this.#next++}`,
          authorizationToken: "up-tok",
        });
      case "b2_start_large_file": {
        const v = this.put(
          String(args["fileName"]),
          new Uint8Array(0),
          args["fileInfo"] as Record<string, string>,
          "start",
        );
        v.contentType = String(args["contentType"]);
        v.parts = new Map();
        return this.#json(this.#wire(v));
      }
      case "b2_finish_large_file":
        return this.#finish(String(args["fileId"]), args["partSha1Array"] as string[]);
      case "b2_get_file_info": {
        const v = this.#byId(String(args["fileId"]));
        return v === undefined ? this.#error(404, "not_found") : this.#json(this.#wire(v));
      }
      case "b2_cancel_large_file": {
        const v = this.#byId(String(args["fileId"]));
        if (v === undefined || v.action !== "start") return this.#error(400, "bad_request");
        this.#remove(v);
        return this.#json({ fileId: v.fileId });
      }
      case "b2_delete_file_version": {
        const v = this.#byId(String(args["fileId"]));
        if (v === undefined || v.fileName !== args["fileName"])
          return this.#error(400, "file_not_present");
        this.#remove(v);
        return this.#json({ fileId: v.fileId, fileName: v.fileName });
      }
      case "b2_list_file_versions":
        return this.#versions(args);
      case "b2_list_file_names":
        return this.#names(args);
      case "b2_copy_file": {
        const source = this.#byId(String(args["sourceFileId"]));
        if (source === undefined) return this.#error(404, "not_found");
        const v = this.put(String(args["fileName"]), source.data, { ...source.fileInfo }, "copy");
        v.contentType = source.contentType;
        return this.#json(this.#wire(v));
      }
      case "b2_copy_part": {
        const source = this.#byId(String(args["sourceFileId"]));
        const large = this.#byId(String(args["largeFileId"]));
        if (source === undefined || large?.parts === undefined)
          return this.#error(400, "bad_request");
        const [, from, to] = /^bytes=(\d+)-(\d+)$/.exec(String(args["range"])) ?? [];
        const data = source.data.slice(Number(from), Number(to) + 1);
        large.parts.set(Number(args["partNumber"]), data);
        return this.#json({ partNumber: args["partNumber"], contentSha1: sha1(data) });
      }
      case "b2_get_bucket_notification_rules":
        return this.#json({ bucketId: "bid", eventNotificationRules: this.rules });
      case "b2_set_bucket_notification_rules":
        this.rules = (args["eventNotificationRules"] as Record<string, unknown>[]).map((rule) => ({
          ...rule,
          isSuspended: false,
          suspensionReason: "",
        }));
        return this.#json({ bucketId: "bid", eventNotificationRules: this.rules });
      default:
        return this.#error(400, "bad_request", `unknown operation ${op}`);
    }
  }

  /** A version by id. */
  #byId(id: string): FakeVersion | undefined {
    for (const versions of this.files.values()) {
      const found = versions.find((v) => v.fileId === id);
      if (found !== undefined) return found;
    }
    return undefined;
  }

  /** Removes a version. */
  #remove(v: FakeVersion): void {
    const rest = (this.files.get(v.fileName) ?? []).filter((x) => x !== v);
    if (rest.length === 0) this.files.delete(v.fileName);
    else this.files.set(v.fileName, rest);
  }

  /** An upload to an upload URL. */
  #upload(op: string, url: URL, headers: Record<string, string>, body: Uint8Array): Response {
    if (headers["authorization"] !== "up-tok") return this.#error(401, "bad_auth_token");
    if (headers["x-bz-content-sha1"] !== sha1(body))
      return this.#error(400, "bad_request", "sha1 mismatch");
    if (op === "upload_part") {
      const large = this.#byId(url.pathname.split("/")[3] ?? "");
      if (large?.parts === undefined) return this.#error(400, "bad_request");
      const number = Number(headers["x-bz-part-number"]);
      large.parts.set(number, body);
      return this.#json({
        fileId: large.fileId,
        partNumber: number,
        contentLength: body.length,
        contentSha1: sha1(body),
      });
    }
    const name = decode(headers["x-bz-file-name"] ?? "");
    const info: Record<string, string> = {};
    for (const [k, v] of Object.entries(headers))
      if (k.startsWith("x-bz-info-")) info[k.slice(10)] = decode(v);
    const v = this.put(name, body, info);
    v.contentType = headers["content-type"] ?? "b2/x-auto";
    const race = this.raceAfterUpload.get(name);
    if (race !== undefined) {
      this.raceAfterUpload.delete(name);
      this.put(name, race);
    }
    return this.#json(this.#wire(v));
  }

  /** Finishes a large file. */
  #finish(fileId: string, sha1s: string[]): Response {
    const v = this.#byId(fileId);
    if (v === undefined || v.action !== "start" || v.parts === undefined)
      return this.#error(400, "bad_request");
    const parts = [...v.parts.entries()].sort((a, b) => a[0] - b[0]).map(([, d]) => d);
    if (parts.length !== sha1s.length || parts.some((d, i) => sha1(d) !== sha1s[i])) {
      return this.#error(400, "bad_request", "part sha1s differ");
    }
    const total = parts.reduce((n, d) => n + d.length, 0);
    const data = new Uint8Array(total);
    let at = 0;
    for (const d of parts) {
      data.set(d, at);
      at += d.length;
    }
    v.data = data;
    v.action = "upload";
    delete v.parts;
    return this.#json(this.#wire(v));
  }

  /** `b2_list_file_versions`. */
  #versions(args: Record<string, unknown>): Response {
    const prefix = String(args["prefix"] ?? "");
    const start = String(args["startFileName"] ?? "");
    const startId = args["startFileId"] === undefined ? null : String(args["startFileId"]);
    const max = Number(args["maxFileCount"] ?? 100);
    const rows: FakeVersion[] = [];
    for (const name of [...this.files.keys()].sort()) {
      if (!name.startsWith(prefix) || name < start) continue;
      rows.push(...(this.files.get(name) ?? []));
    }
    let from = 0;
    if (startId !== null)
      from = Math.max(
        rows.findIndex((v) => v.fileId === startId),
        0,
      );
    const page = rows.slice(from, from + max);
    const next = rows[from + max];
    return this.#json({
      files: page.map((v) => this.#wire(v)),
      nextFileName: next?.fileName ?? null,
      nextFileId: next?.fileId ?? null,
    });
  }

  /** `b2_list_file_names`. */
  #names(args: Record<string, unknown>): Response {
    const prefix = String(args["prefix"] ?? "");
    const start = String(args["startFileName"] ?? "");
    const max = Number(args["maxFileCount"] ?? 100);
    const names = [...this.files.keys()].sort().filter((n) => n.startsWith(prefix) && n >= start);
    const rows = names.map((n) => this.current(n)).filter((v): v is FakeVersion => v !== undefined);
    const page = rows.slice(0, max);
    return this.#json({
      files: page.map((v) => this.#wire(v)),
      nextFileName: rows[max]?.fileName ?? null,
    });
  }

  /** A download (`GET` / `HEAD` on `/file/<bucket>/<name>`). */
  #download(url: URL, method: string, headers: Record<string, string>): Response {
    const [, , bucket, ...rest] = url.pathname.split("/");
    if (bucket !== this.bucket) return this.#error(404, "not_found");
    const name = decode(rest.join("/"));
    const v = this.current(name);
    if (v === undefined || v.action === "hide")
      return method === "HEAD"
        ? new Response(null, { status: 404 })
        : this.#error(404, "not_found");
    const size = v.data.length;
    let start = 0;
    let end = size - 1;
    let status = 200;
    const range = headers["range"];
    if (range !== undefined) {
      const [, a, b] = /^bytes=(\d+)-(\d*)$/.exec(range) ?? [];
      start = Number(a);
      end = b === "" || b === undefined ? size - 1 : Math.min(Number(b), size - 1);
      if (start >= size) return new Response(null, { status: 416 });
      status = 206;
    }
    const out: Record<string, string> = {
      "content-length": String(end - start + 1),
      "x-bz-file-id": v.fileId,
      "x-bz-file-name": encode(v.fileName),
      "x-bz-content-sha1": sha1(v.data),
      "x-bz-upload-timestamp": String(v.uploadTimestamp),
      "content-type": v.contentType,
    };
    for (const [k, val] of Object.entries(v.fileInfo)) out[`x-bz-info-${k}`] = encode(val);
    if (status === 206) out["content-range"] = `bytes ${start}-${end}/${size}`;
    return new Response(method === "HEAD" ? null : v.data.slice(start, end + 1), {
      status,
      headers: out,
    });
  }
}
