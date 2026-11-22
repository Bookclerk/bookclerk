import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchWorks: vi.fn(),
    fetchWishlist: vi.fn(),
    fetchRequestQueue: vi.fn(),
    portalSources: vi.fn(),
    portalConnections: vi.fn(),
  };
});
vi.mock("@/lib/coverLoads", () => ({ coversSettled: vi.fn(() => Promise.resolve()) }));

import * as api from "@/lib/api";
import { pageImageUrls, warmImages, warmPageImages, WARM_LIBRARY_ROWS } from "./pageImageWarm";

describe("pageImageUrls", () => {
  it("lists store logos, then wishlist, queue and library covers at 128 px, once each", () => {
    expect(
      pageImageUrls({
        libraryCovers: ["/api/covers/c", null, "/api/covers/a"],
        wishlistCovers: ["/api/covers/a", undefined],
        queueCovers: ["/api/covers/b", "/api/covers/a"],
        storeLogos: ["/api/images/x", " /api/images/y ", "/api/images/x"],
      }),
    ).toEqual([
      "/api/images/x",
      "/api/images/y",
      "/api/covers/a/128",
      "/api/covers/b/128",
      "/api/covers/c/128",
    ]);
  });

  it("skips logos and covers that are not same-origin paths", () => {
    expect(
      pageImageUrls({
        libraryCovers: ["https://img.example/c.jpg"],
        wishlistCovers: ["  "],
        queueCovers: [],
        storeLogos: ["https://www.google.com/s2/favicons", "//evil.example/x.png", "", null],
      }),
    ).toEqual([]);
  });
});

describe("warmImages", () => {
  it("loads every URL, at most `concurrency` at once, settling before each", async () => {
    let running = 0;
    let peak = 0;
    const loaded: string[] = [];
    const settle = vi.fn(() => Promise.resolve());
    await warmImages(
      ["a", "b", "c", "d", "e"],
      async (url) => {
        running++;
        peak = Math.max(peak, running);
        await new Promise((r) => setTimeout(r, 1));
        loaded.push(url);
        running--;
      },
      settle,
      2,
      () => false,
    );
    expect([...loaded].sort()).toEqual(["a", "b", "c", "d", "e"]);
    expect(peak).toBe(2);
    expect(settle).toHaveBeenCalledTimes(5);
  });

  it("stops once cancelled", async () => {
    const loaded: string[] = [];
    let stop = false;
    await warmImages(
      ["a", "b", "c"],
      async (url) => {
        loaded.push(url);
        stop = true;
      },
      () => Promise.resolve(),
      1,
      () => stop,
    );
    expect(loaded).toEqual(["a"]);
  });

  it("stops when cancelled while waiting for covers on screen", async () => {
    const load = vi.fn(() => Promise.resolve());
    let stop = false;
    await warmImages(
      ["a"],
      load,
      async () => {
        stop = true;
      },
      1,
      () => stop,
    );
    expect(load).not.toHaveBeenCalled();
  });
});

describe("warmPageImages", () => {
  const requested: string[] = [];

  beforeEach(() => {
    requested.length = 0;
    vi.stubGlobal(
      "Image",
      class {
        decoding = "";
        fetchPriority = "";
        onload: (() => void) | null = null;
        onerror: (() => void) | null = null;
        set src(url: string) {
          requested.push(url);
          const done = url.includes("broken") ? this.onerror : this.onload;
          queueMicrotask(() => done?.());
        }
      },
    );
    vi.mocked(api.fetchWorks).mockResolvedValue({
      works: [{ cover_path: "/api/covers/lib" }, { cover_path: "/api/covers/broken" }],
    } as unknown as Awaited<ReturnType<typeof api.fetchWorks>>);
    vi.mocked(api.fetchWishlist).mockResolvedValue([
      { cover_path: "/api/covers/wish" },
    ] as unknown as Awaited<ReturnType<typeof api.fetchWishlist>>);
    vi.mocked(api.fetchRequestQueue).mockRejectedValue(new Error("403"));
    vi.mocked(api.portalSources).mockResolvedValue([
      { brand: { logo: "/api/images/store" } },
    ] as unknown as Awaited<ReturnType<typeof api.portalSources>>);
    vi.mocked(api.portalConnections).mockResolvedValue([
      { brand: { logo: "/api/images/store" } },
      {},
    ] as unknown as Awaited<ReturnType<typeof api.portalConnections>>);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.clearAllMocks();
  });

  it("reads the three pages and loads their images, skipping reads that fail", async () => {
    await warmPageImages(() => false);
    expect(api.fetchWorks).toHaveBeenCalledWith({
      sort: "title",
      status: "all",
      limit: WARM_LIBRARY_ROWS,
    });
    expect([...requested].sort()).toEqual([
      "/api/covers/broken/128",
      "/api/covers/lib/128",
      "/api/covers/wish/128",
      "/api/images/store",
    ]);
  });

  it("reads nothing when cancelled before it starts", async () => {
    await warmPageImages(() => true);
    expect(api.fetchWorks).not.toHaveBeenCalled();
    expect(requested).toEqual([]);
  });

  it("loads nothing when cancelled during the reads", async () => {
    let stop = false;
    vi.mocked(api.fetchWishlist).mockImplementation(async () => {
      stop = true;
      return [];
    });
    await warmPageImages(() => stop);
    expect(requested).toEqual([]);
  });
});
