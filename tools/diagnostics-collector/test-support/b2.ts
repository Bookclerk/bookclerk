// A B2 stand-in for the dashboard tests: authorization always succeeds and
// downloads are served from a map of file IDs to stored text.

import type { B2Access, B2Auth } from "../dashboard/storage.ts";

/** Authorization the stand-in hands out; the URLs match the scripted fetch. */
export const TEST_B2_AUTH: B2Auth = {
  apiUrl: "https://b2.test",
  authorizationToken: "b2tok",
  downloadUrl: "https://b2dl.test",
};

/**
 * A B2 stand-in serving stored reports by file ID.
 *
 * @param objects - Stored text by file ID; a missing ID fails the download
 *   the way a deleted object would.
 * @returns The helpers the dashboard takes from worker.ts.
 */
export function makeB2(objects: Record<string, string> = {}): B2Access {
  return {
    bucketId: "bucket",
    authorize: async () => TEST_B2_AUTH,
    downloadById: async (_auth, id) => {
      const text = objects[id];
      if (text === undefined) throw new Error(`no stored object ${id}`);
      return text;
    },
  };
}
