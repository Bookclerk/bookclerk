import { afterEach, describe, expect, it, vi } from "vitest";
import { authMe } from "./api";

/**
 * Answers every request with one JSON body, standing in for the daemon.
 *
 * @param body - The `/api/auth/me` response body.
 */
function answer(body: unknown): void {
  vi.stubGlobal("window", globalThis);
  vi.stubGlobal(
    "fetch",
    vi.fn(() =>
      Promise.resolve(
        new Response(JSON.stringify(body), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }),
      ),
    ),
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("authMe", () => {
  it("keeps administers_nodes for a Member who owns a node", async () => {
    answer({ authenticated: true, role: "member", administers_nodes: true });
    const session = await authMe();
    expect(session.role).toBe("member");
    expect(session.administers_nodes).toBe(true);
  });

  it("keeps administers_nodes false for a Member without a node", async () => {
    answer({ authenticated: true, role: "member", administers_nodes: false });
    expect((await authMe()).administers_nodes).toBe(false);
  });
});
