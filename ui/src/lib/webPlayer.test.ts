import { describe, expect, it } from "vitest";
import { coverArtwork, playerName } from "./webPlayer";

describe("playerName", () => {
  it("names the browser", () => {
    expect(
      playerName("Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0"),
    ).toBe("Web (Firefox)");
    expect(
      playerName("Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36"),
    ).toBe("Web (Chrome)");
    expect(playerName("Mozilla/5.0 AppleWebKit/537.36 Chrome/140.0 Safari/537.36 Edg/140.0")).toBe(
      "Web (Edge)",
    );
    expect(playerName("Mozilla/5.0 AppleWebKit/537.36 Chrome/140.0 Safari/537.36 OPR/120.0")).toBe(
      "Web (Opera)",
    );
    expect(
      playerName("Mozilla/5.0 (Macintosh) AppleWebKit/605.1.15 Version/19.0 Safari/605.1.15"),
    ).toBe("Web (Safari)");
    expect(playerName("curl/8")).toBe("Web");
  });
});

describe("coverArtwork", () => {
  it("offers the two processed sizes of a cover", () => {
    expect(coverArtwork("/api/covers/abc")).toEqual([
      { src: "/api/covers/abc/128", sizes: "128x128" },
      { src: "/api/covers/abc/360", sizes: "360x360" },
    ]);
  });

  it("offers nothing without a usable cover", () => {
    expect(coverArtwork(null)).toEqual([]);
    expect(coverArtwork("https://elsewhere.example/c.jpg")).toEqual([]);
  });
});
