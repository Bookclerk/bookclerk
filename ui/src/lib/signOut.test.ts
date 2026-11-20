import { afterEach, describe, expect, it, vi } from "vitest";
import { beforeSignOut, signOut } from "./api";

afterEach(() => vi.unstubAllGlobals());

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
});
