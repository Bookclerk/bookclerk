import { afterEach, describe, expect, it, vi } from "vitest";
import {
  BEFORE_SIGN_OUT_WAIT_MS,
  beforeSignOut,
  signOut,
  startImpersonate,
  stopImpersonate,
} from "./api";

/** Removers of the work the running test registered. */
const registered: (() => void)[] = [];

/**
 * Registers work with {@link beforeSignOut} that is removed when the test
 * ends, however it ends: work left behind would run in the next test.
 *
 * @param task - The work.
 * @returns Removes the work now.
 */
function register(task: () => Promise<void>): () => void {
  const remove = beforeSignOut(task);
  registered.push(remove);
  return remove;
}

afterEach(() => {
  for (const remove of registered.splice(0)) remove();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("signOut", () => {
  it("finishes registered work before the session is revoked", async () => {
    const order: string[] = [];
    vi.stubGlobal("fetch", async (url: string) => {
      order.push(url);
      return new Response("{}", { status: 200 });
    });
    const remove = register(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
      order.push("closed");
    });
    const failing = register(async () => {
      throw new Error("offline");
    });
    await signOut("member");
    // Twice: the second pass ends what started while the first was out.
    expect(order).toEqual(["closed", "closed", "/api/portal/logout"]);

    remove();
    failing();
    order.length = 0;
    await signOut("member");
    expect(order).toEqual(["/api/portal/logout"]);
  });

  it("signs out past work that throws or never answers", async () => {
    vi.useFakeTimers();
    const order: string[] = [];
    vi.stubGlobal("fetch", async (url: string) => {
      order.push(url);
      return new Response("{}", { status: 200 });
    });
    register(() => {
      throw new Error("broken before it began");
    });
    register(() => new Promise<void>(() => {}));
    const done = signOut("member");
    await vi.advanceTimersByTimeAsync(BEFORE_SIGN_OUT_WAIT_MS - 1);
    expect(order).toEqual([]);
    await vi.advanceTimersByTimeAsync(1);
    await done;
    expect(order).toEqual(["/api/portal/logout"]);
  });

  it("ends what started during a wait that ran out, before the identity changes", async () => {
    vi.useFakeTimers();
    const order: string[] = [];
    vi.stubGlobal("fetch", async (url: string) => {
      order.push(url);
      return new Response("{}", { status: 200 });
    });
    let runs = 0;
    register(() => {
      runs += 1;
      order.push(`closing ${runs}`);
      // The first close never answers; the second finds what started since.
      return runs === 1 ? new Promise<void>(() => {}) : Promise.resolve();
    });
    const done = signOut("member");
    await vi.advanceTimersByTimeAsync(BEFORE_SIGN_OUT_WAIT_MS);
    await done;
    expect(order).toEqual(["closing 1", "closing 2", "/api/portal/logout"]);
  });

  it("ends what started during a wait that did not run out, and waits for it", async () => {
    const order: string[] = [];
    vi.stubGlobal("fetch", async (url: string) => {
      order.push(url);
      return new Response("{}", { status: 200 });
    });
    let runs = 0;
    register(async () => {
      runs += 1;
      const run = runs;
      await new Promise((resolve) => setTimeout(resolve, 0));
      order.push(`closed ${run}`);
    });
    await signOut("member");
    expect(order).toEqual(["closed 1", "closed 2", "/api/portal/logout"]);
  });

  it("finishes registered work before impersonation starts or stops", async () => {
    const order: string[] = [];
    vi.stubGlobal("fetch", async (url: string, init: RequestInit) => {
      order.push(`${init.method} ${url}`);
      return new Response("{}", { status: 200 });
    });
    const remove = register(async () => {
      order.push("closed");
    });
    await startImpersonate(7);
    await stopImpersonate();
    expect(order).toEqual([
      "closed",
      "closed",
      "POST /api/auth/impersonate",
      "closed",
      "closed",
      "DELETE /api/auth/impersonate",
    ]);
    remove();
  });
});
