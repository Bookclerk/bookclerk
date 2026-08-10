/**
 * Minimal `HOST` binding so plugins can call `env.HOST.notify(...)`.
 *
 * Reverse channel: POSTs PluginToHost-style JSON to the launcher via the
 * `NOTIFY` external service binding (loopback HTTP). The launcher buffers
 * events in memory and logs them; full host fan-out lands later.
 */

import { WorkerEntrypoint } from "cloudflare:workers";

export default class HostBinding extends WorkerEntrypoint {
  async fetch() {
    return new Response(null, { status: 404 });
  }

  async notify(event) {
    const notify = this.env?.NOTIFY;
    if (!notify || typeof notify.fetch !== "function") {
      return;
    }
    try {
      const resp = await notify.fetch("http://host-notify/notify", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(event ?? null),
      });
      if (!resp.ok) {
        console.error(
          `HOST.notify callback HTTP ${resp.status}: ${await resp.text()}`,
        );
      }
    } catch (err) {
      console.error("HOST.notify failed", err);
    }
  }
}
