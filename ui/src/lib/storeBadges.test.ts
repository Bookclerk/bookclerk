import { describe, expect, it } from "vitest";
import { storeBadgeSources } from "./storeBadges";

describe("storeBadgeSources", () => {
  it("badges a store once however many fields name it", () => {
    expect(
      storeBadgeSources({
        primary: "audible",
        sources: ["Audible"],
        editions: ["audible", " AUDIBLE "],
        hints: ["audible"],
        audible: true,
      }),
    ).toEqual(["audible"]);
  });

  it("keeps the primary store first and the rest in order", () => {
    expect(
      storeBadgeSources({
        primary: "libro",
        sources: ["chirp", "libro"],
        editions: ["graphicaudio"],
        hints: ["chirp"],
        audible: true,
      }),
    ).toEqual(["libro", "chirp", "graphicaudio", "audible"]);
  });

  it("skips blank names and a missing primary", () => {
    expect(
      storeBadgeSources({
        primary: null,
        sources: ["", "  "],
        editions: [],
        hints: [],
        audible: false,
      }),
    ).toEqual([]);
  });
});
