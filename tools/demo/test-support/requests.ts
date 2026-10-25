// What the scripted fakes in the tests record about each request, in a shape
// the assertions can read without casts.

/** One request a fake saw. */
export type Sent = {
  /** `METHOD path` (or `METHOD url`). */
  key: string;
  /** The request's options, as the code under test passed them. */
  init: {
    /** Headers by the name the code spelled them with. */
    headers: Record<string, string>;
    /** Body text; empty without a body. */
    body: string;
  };
};

/**
 * Records one request. The code under test always passes headers as a plain
 * object and bodies as text, so anything else is a test failure.
 *
 * @param key - `METHOD path`.
 * @param init - Options passed to the fake.
 * @returns The record.
 * @throws {TypeError} For `Headers`, header pairs or a non-text body.
 */
export function sent(key: string, init: RequestInit): Sent {
  const h = init.headers;
  if (h instanceof Headers || (h !== undefined && Symbol.iterator in h)) {
    throw new TypeError("expected headers as a plain object");
  }
  if (init.body !== undefined && init.body !== null && typeof init.body !== "string") {
    throw new TypeError("expected a text body");
  }
  return { key, init: { headers: h ?? {}, body: init.body ?? "" } };
}
