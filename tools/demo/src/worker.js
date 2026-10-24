// Entry point of the bookclerk-demo Worker (wrangler.toml `main`): the
// request handler and sweep from app.js, and the container class from
// session.js, which Cloudflare binds as SESSIONS.

import { handle, sweep } from "./app.js";

export { DemoSession } from "./session.js";

export default {
  /**
   * Handles one request.
   *
   * @param {Request} request The request.
   * @param {Record<string, any>} env Worker env.
   * @returns {Promise<Response>} The response.
   */
  fetch(request, env) {
    return handle(request, env);
  },

  /**
   * The five-minute cron: ends overdue sessions and prunes old rows.
   *
   * @param {ScheduledController} _controller Cron details (unused).
   * @param {Record<string, any>} env Worker env.
   * @param {ExecutionContext} ctx Lets the sweep finish after the handler returns.
   */
  scheduled(_controller, env, ctx) {
    ctx.waitUntil(sweep(env));
  },
};
