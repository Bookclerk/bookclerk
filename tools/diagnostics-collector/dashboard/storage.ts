/**
 * B2 reads the dashboard needs beyond what worker.ts already does: one page
 * of object names, and one stored report by its key. Authorization and the
 * download itself come from worker.ts (passed in as `B2Access`), so the
 * dashboard uses the Worker's existing B2 credentials and nothing else.
 */

import { asObject, isObject, type JsonObject } from "../json.ts";

/** A B2 authorization: where to call and with which token. */
export type B2Auth = {
  /** Base URL of the B2 API. */
  apiUrl: string;
  /** Token for the `Authorization` header. */
  authorizationToken: string;
  /** Base URL for downloads. */
  downloadUrl: string;
};

/** The B2 helpers worker.ts hands to the dashboard. */
export type B2Access = {
  /** Authorizes the account (cached by the Worker). */
  authorize(): Promise<B2Auth>;
  /** Downloads one object's text by its file ID. */
  downloadById(auth: B2Auth, fileId: string): Promise<string>;
  /** The bucket reports are stored in. */
  bucketId: string;
};

/** Largest stored report the dashboard will parse, in bytes of text. */
export const MAX_STORED_REPORT_CHARS = 1024 * 1024;

/**
 * One page of object names under a prefix.
 *
 * @param b2 - B2 helpers.
 * @param auth - Result of `b2.authorize()`.
 * @param opts - Listing window.
 * @returns Names in order and where the next page starts.
 */
export async function listPage(b2: B2Access, auth: B2Auth, opts: { prefix: string; startFileName?: string | null; maxFileCount: number; }): Promise<{
  files: { fileName: string; fileId: string; uploadTimestamp: number; }[];
  nextFileName: string | null;
}> {
  const body: { bucketId: string; prefix: string; maxFileCount: number; startFileName?: string } = {
    bucketId: b2.bucketId,
    prefix: opts.prefix,
    maxFileCount: opts.maxFileCount,
  };
  if (opts.startFileName) body.startFileName = opts.startFileName;
  const res = await fetch(`${auth.apiUrl}/b2api/v2/b2_list_file_names`, {
    method: "POST",
    headers: { Authorization: auth.authorizationToken, "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error("b2 list failed");
  return decodeListPage(await res.json().catch(() => null));
}

/**
 * Reads a `b2_list_file_names` answer. Sans-IO, so the fuzzer drives the
 * same code report viewing and reindexing do. Never throws.
 *
 * @param data - Parsed response body.
 * @returns Uploaded files with string name and id, in
 *   order (a timestamp that is not a finite number becomes 0), and the next
 *   page's start when it is a non-empty string.
 */
export function decodeListPage(data: unknown): {
  files: { fileName: string; fileId: string; uploadTimestamp: number; }[];
  nextFileName: string | null;
} {
  const d = asObject(data);
  const files: { fileName: string; fileId: string; uploadTimestamp: number; }[] = [];
  for (const f of Array.isArray(d.files) ? d.files : []) {
    if (!isObject(f) || f.action !== "upload" || typeof f.fileName !== "string" || typeof f.fileId !== "string") continue;
    files.push({
      fileName: f.fileName,
      fileId: f.fileId,
      uploadTimestamp: typeof f.uploadTimestamp === "number" && Number.isFinite(f.uploadTimestamp) ? f.uploadTimestamp : 0,
    });
  }
  return { files, nextFileName: typeof d.nextFileName === "string" && d.nextFileName !== "" ? d.nextFileName : null };
}

/**
 * Downloads and parses one stored report by its object key.
 *
 * @param b2 - B2 helpers.
 * @param objectKey - `diagnostics/<version>/<report_id>.json`.
 * @returns The stored object, or null when it is
 *   missing, too large or not JSON.
 */
export async function readStoredReport(b2: B2Access, objectKey: string): Promise<JsonObject | null> {
  const auth = await b2.authorize();
  const page = await listPage(b2, auth, { prefix: objectKey, maxFileCount: 1 });
  const file = page.files.find((f) => f.fileName === objectKey);
  if (!file) return null;
  const text = await b2.downloadById(auth, file.fileId);
  if (text.length > MAX_STORED_REPORT_CHARS) return null;
  try {
    const parsed: unknown = JSON.parse(text);
    return isObject(parsed) ? parsed : null;
  } catch {
    return null;
  }
}
