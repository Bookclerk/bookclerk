import { describe, expect, it } from "vitest";
import type { GlobalQueueEntry, TitleRequest } from "@/lib/api";
import { myWishFor } from "@/lib/myWish";

function wish(uuid: string, work_key: string, work_id: string | null): TitleRequest {
  return { uuid, work_key, work_id } as TitleRequest;
}

function entry(work_key: string, work_id?: string | null): GlobalQueueEntry {
  return { work_key, work_id } as GlobalQueueEntry;
}

describe("myWishFor", () => {
  it("finds a wish grouped under another wisher's key by its work", () => {
    const mine = [wish("w1", "isbn:9781427201447", "work-dune")];
    expect(myWishFor(mine, entry("asin:B002V1OF70", "work-dune"))?.uuid).toBe("w1");
  });

  it("falls back to the key when the entry has no work", () => {
    const mine = [wish("w2", "asin:B0SEQUEL", null)];
    expect(myWishFor(mine, entry("asin:B0SEQUEL", null))?.uuid).toBe("w2");
    expect(myWishFor(mine, entry("asin:B0SEQUEL"))?.uuid).toBe("w2");
  });

  it("finds nothing for another work or an empty key", () => {
    const mine = [wish("w3", "", "work-a")];
    expect(myWishFor(mine, entry("", "work-b"))).toBeUndefined();
    expect(myWishFor([], entry("asin:X", "work-a"))).toBeUndefined();
  });
});
