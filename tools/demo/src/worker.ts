// Entry point of the bookclerk-demo Worker (wrangler.toml `main`): the
// request handler and sweep from app.ts, and the container class from
// session.ts, which Cloudflare binds as SESSIONS.

import { handle, sweep } from "./app.ts";
import type { Env } from "./env.ts";

export { DemoSession } from "./session.ts";

export default {
  /**
   * Handles one request.
   *
   * @param request - The request.
   * @param env - Worker env.
   * @returns The response.
   */
  fetch(request: Request, env: Env): Promise<Response> {
    return handle(request, env);
  },

  /**
   * The five-minute cron: ends overdue sessions and prunes old rows.
   *
   * @param _controller - Cron details (unused).
   * @param env - Worker env.
   * @param ctx - Lets the sweep finish after the handler returns.
   */
  scheduled(_controller: ScheduledController, env: Env, ctx: ExecutionContext): void {
    ctx.waitUntil(sweep(env));
  },
} satisfies ExportedHandler<Env>;
