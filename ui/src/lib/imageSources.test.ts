import { describe, expect, it } from "vitest";
import { gravatarUrl, resolveAvatar, ssoPictureSrc } from "@/lib/avatar";
import { storeIconSrc } from "@/lib/catalogTitle";

const HASH = "0123456789abcdef".repeat(4);

describe("same-origin image sources", () => {
  it("loads a Gravatar through Bookclerk", () => {
    expect(gravatarUrl(HASH)).toBe(`/api/avatars/gravatar/${HASH}`);
    expect(resolveAvatar({ gravatar_hash: HASH })).toEqual({
      kind: "gravatar",
      src: `/api/avatars/gravatar/${HASH}`,
    });
  });

  it("shows an identity picture only through its signed path", () => {
    const signed = {
      identity_id: 3,
      provider: "oidc:google",
      picture_url: "https://lh3.example/a.png",
      picture_path: "/api/covers/abc.def/128",
    };
    const unsigned = { ...signed, identity_id: 4, picture_path: null };
    expect(ssoPictureSrc(signed)).toBe("/api/covers/abc.def/128");
    expect(ssoPictureSrc(unsigned)).toBeUndefined();
    expect(resolveAvatar({ sso_pictures: [unsigned, signed] })).toEqual({
      kind: "sso",
      src: "/api/covers/abc.def/128",
      identityId: 3,
    });
    expect(resolveAvatar({ sso_pictures: [unsigned], avatar_source: "sso:4" })).toEqual({
      kind: "monogram",
    });
  });

  it("names store icons by plugin id", () => {
    expect(storeIconSrc("audible")).toBe("/api/stores/audible/icon");
    expect(storeIconSrc(" Libro.fm ")).toBe("/api/stores/libro/icon");
    expect(storeIconSrc("")).toBeUndefined();
    expect(storeIconSrc("../x")).toBeUndefined();
    expect(storeIconSrc("https://evil.example")).toBeUndefined();
  });
});
