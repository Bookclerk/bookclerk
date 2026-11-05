// `Server-Timing` entries on proxied answers, so a slow page in the demo
// shows in the browser's network panel where the time went: in the
// session's Durable Object and its container, or in bookclerkd itself.

/**
 * A copy of `response` with one more `Server-Timing` entry.
 *
 * A WebSocket upgrade is returned as is, since its headers are already
 * sent.
 *
 * @param response - Answer to annotate.
 * @param name - Metric name (`session`, `container`).
 * @param ms - Elapsed milliseconds; negative values count as zero.
 * @returns The annotated answer.
 */
export function withServerTiming(response: Response, name: string, ms: number): Response {
  if (response.webSocket) return response;
  const annotated = new Response(response.body, response);
  annotated.headers.append("Server-Timing", `${name};dur=${Math.max(0, Math.round(ms))}`);
  return annotated;
}
