/**
 * A fake Audiobookshelf server for the unit tests: a `fetch` that answers
 * from a route table and records every request.
 */

import type { Fetch } from "../src/client.ts";

/** One request the fake saw. */
export interface Seen {
  /** HTTP method. */
  readonly method: string;
  /** Path with query. */
  readonly path: string;
  /** Request headers, lower-cased names. */
  readonly headers: Record<string, string>;
  /** Request body text, or empty. */
  readonly body: string;
  /** The `redirect` mode asked for. */
  readonly redirect: string | undefined;
}

/** Answers one request. */
export type Route = (seen: Seen) => Response | undefined;

/** The fake server. */
export interface FakeServer {
  /** The `fetch` to hand the client. */
  readonly fetch: Fetch;
  /** Every request so far. */
  readonly seen: Seen[];
  /** Base URL the fake answers on. */
  readonly baseUrl: string;
}

/**
 * A JSON response.
 *
 * @param body - Body to encode.
 * @param status - HTTP status.
 * @returns The response.
 */
export function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/**
 * The path and query of `url` when it is on the fake's origin, else `url`.
 *
 * @param url - The requested URL.
 * @param baseUrl - The fake's base URL.
 * @returns The path with query, as sent.
 */
function pathOf(url: string, baseUrl: string): string {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return url;
  }
  return parsed.origin === new URL(baseUrl).origin ? `${parsed.pathname}${parsed.search}` : url;
}

/**
 * A fake server answering from `routes` (first match wins; 404 otherwise).
 *
 * @param routes - `"METHOD /path"` (exact path, query included) or a matcher.
 * @returns The server.
 */
export function fakeServer(routes: Record<string, () => Response> | Route): FakeServer {
  const seen: Seen[] = [];
  const baseUrl = "https://abs.example";
  const fetch: Fetch = async (url, init) => {
    const headers: Record<string, string> = {};
    for (const [name, value] of Object.entries((init.headers ?? {}) as Record<string, string>)) {
      headers[name.toLowerCase()] = value;
    }
    const request: Seen = {
      method: init.method ?? "GET",
      path: pathOf(url, baseUrl),
      headers,
      body: typeof init.body === "string" ? init.body : "",
      redirect: init.redirect,
    };
    seen.push(request);
    if (typeof routes === "function")
      return routes(request) ?? new Response("nope", { status: 404 });
    const route = routes[`${request.method} ${request.path}`];
    return route === undefined ? new Response("nope", { status: 404 }) : route();
  };
  return { fetch, seen, baseUrl };
}

/**
 * A server whose `GET /api/users` lists users with the given ids.
 *
 * @param ids - User ids; each username is `<id>-name`.
 * @returns The server.
 */
export function usersServer(ids: readonly string[]): FakeServer {
  return fakeServer({
    "GET /api/users": () => json({ users: ids.map((id) => ({ id, username: `${id}-name` })) }),
  });
}
