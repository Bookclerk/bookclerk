import { afterEach, describe, expect, it, vi } from "vitest";
import { authMe } from "@/lib/api";
import { pricesVisible, setPricesVisible } from "@/lib/pricesVisible";

describe("pricesVisible", () => {
  afterEach(() => setPricesVisible(true));

  it("shows prices until a session says otherwise", () => {
    expect(pricesVisible()).toBe(true);
    setPricesVisible(false);
    expect(pricesVisible()).toBe(false);
    setPricesVisible(true);
    expect(pricesVisible()).toBe(true);
  });
});

describe("authMe see_prices", () => {
  afterEach(() => vi.unstubAllGlobals());

  async function sessionFrom(body: Record<string, unknown>) {
    vi.stubGlobal("window", globalThis);
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify(body), { status: 200 })),
    );
    return authMe();
  }

  it("reads the session's see_prices and treats a missing one as allowed", async () => {
    const base = { authenticated: true, default_view: "discover", can_acquire: false };
    expect((await sessionFrom({ ...base, see_prices: false })).see_prices).toBe(false);
    expect((await sessionFrom({ ...base, see_prices: true })).see_prices).toBe(true);
    expect((await sessionFrom(base)).see_prices).toBe(true);
  });
});
