import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "./api";
import {
  downloadRecoveryCopy,
  fetchMasterKeyStatus,
  makeRecoveryCopy,
  RECOVERY_COPY_FILE_NAME,
  recordRecoveryCopy,
  recoveryPasswordProblem,
} from "./masterKey";

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

/**
 * Stubs `fetch` with one answer.
 *
 * @param body - The answer's body.
 * @param init - The answer's status and headers.
 * @returns The `fetch` mock.
 */
function answer(body: BodyInit | null, init: ResponseInit = {}) {
  const mock = vi.fn(async () => new Response(body, init));
  vi.stubGlobal("fetch", mock);
  return mock;
}

describe("recoveryPasswordProblem", () => {
  it("needs twelve characters, not counting surrounding spaces", () => {
    expect(recoveryPasswordProblem("   short   ", "   short   ")).toMatch(/12 characters/);
    expect(recoveryPasswordProblem("a".repeat(11), "a".repeat(11))).toMatch(/12/);
  });

  it("needs the confirmation to match", () => {
    expect(recoveryPasswordProblem("correct horse battery", "correct horse")).toMatch(/match/);
    expect(recoveryPasswordProblem("correct horse battery", "correct horse battery")).toBeNull();
  });

  it("counts characters, not UTF-16 units", () => {
    const twelve = "🔑".repeat(12);
    expect(recoveryPasswordProblem(twelve, twelve)).toBeNull();
    const six = "🔑".repeat(6);
    expect(recoveryPasswordProblem(six, six)).toMatch(/12/);
  });
});

describe("master key routes", () => {
  it("reads the status", async () => {
    const status = { store: "keychain", recovery_copy: "unrecorded", needs_recovery_copy: true };
    const mock = answer(JSON.stringify(status), { status: 200 });
    await expect(fetchMasterKeyStatus()).resolves.toEqual(status);
    expect(mock).toHaveBeenCalledWith(
      "/api/config/master-key",
      expect.objectContaining({ credentials: "include", cache: "no-store" }),
    );
  });

  it("sends the password in the body, never the URL", async () => {
    const mock = answer(new Blob(["BCK2…"]), { status: 200 });
    const copy = await makeRecoveryCopy("correct horse battery");
    expect(await copy.text()).toBe("BCK2…");
    const [url, init] = mock.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("/api/config/master-key/recovery-copy");
    expect(url).not.toContain("horse");
    expect(init.method).toBe("POST");
    expect(JSON.parse(String(init.body))).toEqual({ password: "correct horse battery" });
  });

  it("records the answer", async () => {
    const status = { store: "keychain", recovery_copy: "saved", needs_recovery_copy: false };
    const mock = answer(JSON.stringify(status), { status: 200 });
    await expect(recordRecoveryCopy("saved")).resolves.toEqual(status);
    const [url, init] = mock.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("/api/config/master-key/recovery-copy/state");
    expect(init.method).toBe("PUT");
    expect(JSON.parse(String(init.body))).toEqual({ state: "saved" });
  });

  it("surfaces the daemon's detail and slug", async () => {
    answer(
      JSON.stringify({
        error: "password_too_short",
        detail: "a recovery password needs at least 12 characters",
      }),
      { status: 400 },
    );
    const err = await makeRecoveryCopy("x").catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBe(400);
    expect((err as ApiError).code).toBe("password_too_short");
    expect((err as ApiError).message).toBe("a recovery password needs at least 12 characters");
  });

  it("falls back to the middleware's message, then the raw text", async () => {
    answer(JSON.stringify({ error: "unauthorized", message: "Sign in first." }), { status: 401 });
    await expect(fetchMasterKeyStatus()).rejects.toThrow("Sign in first.");
    answer("upstream down", { status: 502, statusText: "Bad Gateway" });
    await expect(recordRecoveryCopy("skipped")).rejects.toThrow("upstream down");
    answer(null, { status: 500, statusText: "Internal Server Error" });
    await expect(fetchMasterKeyStatus()).rejects.toThrow("500 Internal Server Error");
  });
});

describe("downloadRecoveryCopy", () => {
  it("clicks a download link named for the copy and frees the URL later", () => {
    vi.useFakeTimers();
    const createObjectURL = vi.fn(() => "blob:copy");
    const revokeObjectURL = vi.fn();
    vi.stubGlobal("URL", { createObjectURL, revokeObjectURL });
    const click = vi.fn();
    const link = { href: "", download: "", rel: "", click, remove: vi.fn() };
    const appendChild = vi.fn();
    const doc = {
      createElement: vi.fn(() => link),
      body: { appendChild },
    } as unknown as Document;

    downloadRecoveryCopy(new Blob(["BCK2"]), doc);

    expect(link.href).toBe("blob:copy");
    expect(link.download).toBe(RECOVERY_COPY_FILE_NAME);
    expect(appendChild).toHaveBeenCalledWith(link);
    expect(click).toHaveBeenCalledOnce();
    expect(link.remove).toHaveBeenCalledOnce();
    expect(revokeObjectURL).not.toHaveBeenCalled();
    vi.advanceTimersByTime(60_000);
    expect(revokeObjectURL).toHaveBeenCalledWith("blob:copy");
  });
});
