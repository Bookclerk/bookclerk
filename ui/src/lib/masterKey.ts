/**
 * The library key's recovery copy: the web side of
 * `bookclerk config master-key export`.
 *
 * A key kept in the OS keychain is not in any file a backup picks up. The
 * desktop window's first run asks its operator to save a recovery copy, or
 * skip it after a warning, and Settings → Server Settings reminds them until
 * one is saved. All three routes are operator-only.
 */

import { ApiError } from "@/lib/api";

/** The shortest recovery password the daemon accepts, in characters. */
export const MIN_RECOVERY_PASSWORD_CHARS = 12;

/** The file name a downloaded recovery copy is saved as. */
export const RECOVERY_COPY_FILE_NAME = "bookclerk-recovery-key.bck";

/** Where the install keeps its key. */
export type MasterKeyStore = "file" | "password_file" | "keychain";

/** What the operator recorded about a recovery copy of the key in use. */
export type RecoveryCopyAnswer = "unrecorded" | "saved" | "skipped";

/** `GET /api/config/master-key`. */
export interface MasterKeyStatus {
  /** Where the key lives. */
  store: MasterKeyStore;
  /** What the operator recorded about a recovery copy of this key. */
  recovery_copy: RecoveryCopyAnswer;
  /** When that was recorded (RFC 3339); absent when `unrecorded`. */
  recorded_at?: string;
  /** Whether to remind the operator: keychain key, no copy saved. */
  needs_recovery_copy: boolean;
}

/**
 * Turns a failed answer into an {@link ApiError} carrying the daemon's
 * `detail` (or `message`) and its `error` slug.
 *
 * @param res - The failed response.
 * @returns The error to throw.
 */
async function masterKeyError(res: Response): Promise<ApiError> {
  const text = await res.text().catch(() => "");
  let message = text || `${res.status} ${res.statusText}`;
  let code: string | undefined;
  try {
    const body = JSON.parse(text) as { error?: unknown; detail?: unknown; message?: unknown };
    code = typeof body.error === "string" ? body.error : undefined;
    const detail = typeof body.detail === "string" ? body.detail.trim() : "";
    const said = typeof body.message === "string" ? body.message.trim() : "";
    message = detail || said || code || message;
  } catch {
    // keep the raw text
  }
  return new ApiError(res.status, message, { code });
}

/**
 * Reads where the key lives and whether a recovery copy is recorded.
 *
 * @returns The key's status.
 */
export async function fetchMasterKeyStatus(): Promise<MasterKeyStatus> {
  const res = await fetch("/api/config/master-key", {
    credentials: "include",
    cache: "no-store",
  });
  if (!res.ok) throw await masterKeyError(res);
  return (await res.json()) as MasterKeyStatus;
}

/**
 * Asks the daemon for a recovery copy wrapped with `password`. Nothing is
 * recorded until {@link recordRecoveryCopy}.
 *
 * @param password - The password to wrap the copy with (at least
 *   {@link MIN_RECOVERY_PASSWORD_CHARS} characters).
 * @returns The copy's bytes.
 */
export async function makeRecoveryCopy(password: string): Promise<Blob> {
  const res = await fetch("/api/config/master-key/recovery-copy", {
    method: "POST",
    credentials: "include",
    cache: "no-store",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ password }),
  });
  if (!res.ok) throw await masterKeyError(res);
  return res.blob();
}

/**
 * Records that the operator saved a recovery copy, or skipped the step.
 *
 * @param state - What they did.
 * @returns The key's status after the write.
 */
export async function recordRecoveryCopy(state: "saved" | "skipped"): Promise<MasterKeyStatus> {
  const res = await fetch("/api/config/master-key/recovery-copy/state", {
    method: "PUT",
    credentials: "include",
    cache: "no-store",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ state }),
  });
  if (!res.ok) throw await masterKeyError(res);
  return (await res.json()) as MasterKeyStatus;
}

/**
 * Checks a recovery password and its confirmation before asking the daemon.
 *
 * @param password - The password typed.
 * @param confirm - The password typed again.
 * @returns What is wrong, or `null` when both are fine.
 */
export function recoveryPasswordProblem(password: string, confirm: string): string | null {
  if ([...password.trim()].length < MIN_RECOVERY_PASSWORD_CHARS) {
    return `Use at least ${MIN_RECOVERY_PASSWORD_CHARS} characters.`;
  }
  if (password !== confirm) return "The passwords don't match.";
  return null;
}

/**
 * Hands `copy` to the browser as a file download.
 *
 * @param copy - The recovery copy's bytes.
 * @param doc - The document to download from (defaults to the page's).
 */
export function downloadRecoveryCopy(copy: Blob, doc: Document = document): void {
  const url = URL.createObjectURL(copy);
  try {
    const link = doc.createElement("a");
    link.href = url;
    link.download = RECOVERY_COPY_FILE_NAME;
    link.rel = "noopener";
    doc.body.appendChild(link);
    link.click();
    link.remove();
  } finally {
    // Give the download a moment to start before the URL goes away.
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
  }
}
