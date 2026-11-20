import { afterEach, describe, expect, it, vi } from "vitest";
import {
  BEFORE_SIGN_OUT_WAIT_MS,
  beforeSignOut,
  signOut,
  startImpersonate,
  stopImpersonate,
} from "./api";

afterEach(() => {
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
    const remove = beforeSignOut(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
      order.push("closed");
    });
    const failing = beforeSignOut(async () => {
      throw new Error("offline");
    });
    await signOut("member");
    expect(order).toEqual(["closed", "/api/portal/logout"]);

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
    const throwing = beforeSignOut(() => {
      throw new Error("broken before it began");
    });
    const hanging = beforeSignOut(() => new Promise<void>(() => {}));
    const done = signOut("member");
    await vi.advanceTimersByTimeAsync(BEFORE_SIGN_OUT_WAIT_MS - 1);
    expect(order).toEqual([]);
    await vi.advanceTimersByTimeAsync(1);
    await done;
    expect(order).toEqual(["/api/portal/logout"]);
    throwing();
    hanging();
  });

  it("finishes registered work before impersonation starts or stops", async () => {
    const order: string[] = [];
    vi.stubGlobal("fetch", async (url: string, init: RequestInit) => {
      order.push(`${init.method} ${url}`);
      return new Response("{}", { status: 200 });
    });
    const remove = beforeSignOut(async () => {
      order.push("closed");
    });
    await startImpersonate(7);
    await stopImpersonate();
    expect(order).toEqual([
      "closed",
      "POST /api/auth/impersonate",
      "closed",
      "DELETE /api/auth/impersonate",
    ]);
    remove();
  });
});
