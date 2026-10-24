/**
 * B2 reads the dashboard needs beyond what worker.js already does: one page
 * of object names, and one stored report by its key. Authorization and the
 * download itself come from worker.js (passed in as `B2Access`), so the
 * dashboard uses the Worker's existing B2 credentials and nothing else.
 */

/**
 * The B2 helpers worker.js hands to the dashboard.
 *
 * @typedef {{
 *   authorize(): Promise<{apiUrl: string, authorizationToken: string, downloadUrl: string}>,
 *   downloadById(auth: object, fileId: string): Promise<string>,
 *   bucketId: string,
 * }} B2Access
 */

/** Largest stored report the dashboard will parse, in bytes of text. */
export const MAX_STORED_REPORT_CHARS = 1024 * 1024;

/**
 * One page of object names under a prefix.
 *
 * @param {B2Access} b2 B2 helpers.
 * @param {object} auth Result of `b2.authorize()`.
 * @param {{prefix: string, startFileName?: string | null, maxFileCount: number}} opts
 *   Listing window.
 * @returns {Promise<{files: {fileName: string, fileId: string, uploadTimestamp: number}[],
 *   nextFileName: string | null}>} Names in order and where the next page starts.
 */
export async function listPage(b2, auth, opts) {
  const body = { bucketId: b2.bucketId, prefix: opts.prefix, maxFileCount: opts.maxFileCount };
  if (opts.startFileName) body.startFileName = opts.startFileName;
  const res = await fetch(`${auth.apiUrl}/b2api/v2/b2_list_file_names`, {
    method: "POST",
    headers: { Authorization: auth.authorizationToken, "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error("b2 list failed");
  const data = await res.json();
  const files = (Array.isArray(data.files) ? data.files : [])
    .filter((f) => f && f.action === "upload" && typeof f.fileName === "string" && typeof f.fileId === "string")
    .map((f) => ({ fileName: f.fileName, fileId: f.fileId, uploadTimestamp: Number(f.uploadTimestamp) }));
  return { files, nextFileName: typeof data.nextFileName === "string" ? data.nextFileName : null };
}

/**
 * Downloads and parses one stored report by its object key.
 *
 * @param {B2Access} b2 B2 helpers.
 * @param {string} objectKey `diagnostics/<version>/<report_id>.json`.
 * @returns {Promise<object | null>} The stored object, or null when it is
 *   missing, too large or not JSON.
 */
export async function readStoredReport(b2, objectKey) {
  const auth = await b2.authorize();
  const page = await listPage(b2, auth, { prefix: objectKey, maxFileCount: 1 });
  const file = page.files.find((f) => f.fileName === objectKey);
  if (!file) return null;
  const text = await b2.downloadById(auth, file.fileId);
  if (text.length > MAX_STORED_REPORT_CHARS) return null;
  try {
    const parsed = JSON.parse(text);
    return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : null;
  } catch {
    return null;
  }
}
