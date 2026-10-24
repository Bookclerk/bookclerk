// One demo session: a Cloudflare Container running the demo image, owned by
// the Durable Object Cloudflare pairs with it. The object starts bookclerkd
// with a fresh operator token, signs the visitor in, forwards their requests,
// and on every way a session ends (Start over, 10 minutes idle, the hard
// stop, the sweep) signs their store accounts out before destroying the VM.
// The VM's disk is ephemeral, so destroying it deletes the library,
// master.key and every token with it.

import { Container } from "@cloudflare/containers";
import { finish } from "./limits.js";
import { randomHex, signInVisitor, signOutStores, waitForHealth } from "./lifecycle.js";

/** bookclerkd's port inside the container (BOOKCLERK_DAEMON_LISTEN in the image). */
const PORT = 8787;

/** Longest wait for bookclerkd to answer /health after the VM starts. */
const BOOT_TIMEOUT_MS = 120_000;

/** Longest wait for each store sign-out request at teardown. */
const SIGN_OUT_TIMEOUT_MS = 10_000;

/**
 * Where a session is.
 *
 * @typedef {"starting" | "ready" | "ending" | "ended" | "failed"} SessionState
 */

/**
 * What the object keeps in its storage. `secrets` is deleted when the
 * session ends.
 *
 * @typedef {object} SessionMeta
 * @property {string} id Session id (the D1 row and this object's name).
 * @property {number} endsAt Hard stop, Unix seconds.
 * @property {SessionState} state Where the session is.
 * @property {string} version Image version shown in the banner.
 * @property {string} origin The demo's origin (`https://host`), which
 *   bookclerkd serves as its `public_origin`.
 */

/** A visitor's container and the object that owns it. */
export class DemoSession extends Container {
  /** bookclerkd's port. */
  defaultPort = PORT;
  /** Idle time before onActivityExpired ends the session. */
  sleepAfter = "10m";
  /** Store traffic leaves the VM directly; the jail applies each plugin's grants. */
  enableInternet = true;

  /**
   * Reads the session's metadata.
   *
   * @returns {Promise<SessionMeta | undefined>} The metadata, if opened.
   */
  async meta() {
    return this.ctx.storage.get("meta");
  }

  /**
   * Updates the session's state.
   *
   * @param {SessionState} state New state.
   * @returns {Promise<void>}
   */
  async setState(state) {
    const meta = await this.meta();
    if (meta) await this.ctx.storage.put("meta", { ...meta, state });
  }

  /**
   * Opens the session: records it and schedules the boot and the hard stop.
   * Calling it again for an open session does nothing.
   *
   * @param {{id: string, endsAt: number, version: string, origin: string}} params
   *   Session id, hard stop (Unix seconds), image version and the demo's origin.
   * @returns {Promise<void>}
   */
  async open(params) {
    if (await this.meta()) return;
    await this.ctx.storage.put("meta", { ...params, state: "starting" });
    await this.schedule(1, "boot");
    await this.schedule(new Date(params.endsAt * 1000), "expire");
  }

  /**
   * Whether the session is still starting. The session can end while boot
   * waits on the VM (Start over, the hard stop, the sweep); boot then must
   * not bring it back.
   *
   * @returns {Promise<boolean>} True while the state is still "starting".
   */
  async stillStarting() {
    return (await this.meta())?.state === "starting";
  }

  /**
   * Starts the VM and signs the visitor in. Scheduled by {@link open}.
   *
   * @returns {Promise<void>}
   */
  async boot() {
    const meta = await this.meta();
    if (!meta || meta.state !== "starting") return;
    const operatorToken = randomHex(32);
    try {
      // Host env only: never argv, never forwarded to a plugin guest. The
      // public origin is the demo's own: bookclerkd checks browsers' Origin
      // against it and builds sign-in callbacks from it.
      await this.startAndWaitForPorts({
        ports: PORT,
        startOptions: {
          envVars: {
            BOOKCLERK_OPERATOR_TOKEN: operatorToken,
            BOOKCLERK_AUTH_PASSWORD: randomHex(32),
            BOOKCLERK_INTEGRATIONS_PUBLIC_ORIGIN: meta.origin,
          },
        },
        cancellationOptions: { portReadyTimeoutMS: BOOT_TIMEOUT_MS },
      });
      const call = (path, init) => this.containerFetch(`http://container${path}`, init, PORT);
      if (await this.stillStarting()) {
        await waitForHealth(call, { timeoutMs: BOOT_TIMEOUT_MS, intervalMs: 1000 });
      }
      const portalSession = (await this.stillStarting()) ? await signInVisitor(call, operatorToken) : null;
      if (portalSession === null || !(await this.stillStarting())) {
        // Ended meanwhile: close() may have run before the VM was up, so
        // make sure it is gone. The end is already recorded.
        await this.destroy();
        return;
      }
      await this.ctx.storage.put("secrets", { operatorToken, portalSession });
      await this.setState("ready");
    } catch (err) {
      if (!(await this.stillStarting())) {
        await this.destroy().catch(() => {});
        return;
      }
      console.error(`demo session ${meta.id} failed to start: ${String(err)}`);
      await this.setState("failed");
      await this.close("failed", { revoked: 0, failed: 0 });
    }
  }

  /**
   * Where the session is, for the waiting page and the banner.
   *
   * @returns {Promise<{state: SessionState | "none", endsAt: number, version: string}>}
   */
  async status() {
    const meta = await this.meta();
    if (!meta) return { state: "none", endsAt: 0, version: "" };
    return { state: meta.state, endsAt: meta.endsAt, version: meta.version };
  }

  /**
   * The visitor's portal session, handed to their browser once on entry.
   *
   * @returns {Promise<string | null>} The session when ready; otherwise null.
   */
  async portalSession() {
    const meta = await this.meta();
    if (meta?.state !== "ready") return null;
    const secrets = await this.ctx.storage.get("secrets");
    return secrets?.portalSession ?? null;
  }

  /**
   * Forwards a visitor's request to bookclerkd with the operator token.
   *
   * The Worker has already checked the route. The token never leaves this
   * object: the browser holds only its portal session.
   *
   * @param {Request} request Request from the Worker.
   * @returns {Promise<Response>} bookclerkd's answer, or 503 when the session
   *   is not running.
   */
  async fetch(request) {
    const meta = await this.meta();
    const secrets = meta?.state === "ready" ? await this.ctx.storage.get("secrets") : undefined;
    if (!secrets) {
      // The Worker turns this into the waiting page or the ended page.
      return Response.json(
        { error: "This demo session is not running." },
        { status: 503, headers: { "X-Demo-State": meta?.state ?? "none" } },
      );
    }
    const forwarded = new Request(request);
    forwarded.headers.set("Authorization", `Bearer ${secrets.operatorToken}`);
    return this.containerFetch(forwarded, PORT);
  }

  /**
   * Ends the session: signs store accounts out, destroys the VM, records it.
   *
   * @param {string} reason Why: visitor, idle, time or sweep.
   * @returns {Promise<{revoked: number, failed: number}>} Accounts signed out
   *   and failed sign-outs (zeros when it had already ended).
   */
  async end(reason) {
    const meta = await this.meta();
    if (!meta || meta.state === "ending" || meta.state === "ended" || meta.state === "failed") {
      return { revoked: 0, failed: 0 };
    }
    const wasReady = meta.state === "ready";
    await this.setState("ending");
    let result = { revoked: 0, failed: 0 };
    const secrets = await this.ctx.storage.get("secrets");
    if (wasReady && secrets) {
      const call = (path, init) => this.containerFetch(`http://container${path}`, init, PORT);
      result = await signOutStores(call, secrets.portalSession, SIGN_OUT_TIMEOUT_MS, meta.origin);
    }
    await this.close(reason, result);
    return result;
  }

  /**
   * Destroys the VM, forgets the secrets and writes the session's end to D1.
   *
   * @param {string} reason Why the session ended.
   * @param {{revoked: number, failed: number}} result Sign-out counts.
   * @returns {Promise<void>}
   */
  async close(reason, result) {
    const meta = await this.meta();
    await this.ctx.storage.delete("secrets");
    if (meta && meta.state !== "failed") await this.setState("ended");
    try {
      await this.destroy();
    } catch (err) {
      console.error(`demo session ${meta?.id} destroy failed: ${String(err)}`);
    }
    if (meta) {
      await finish(this.env.DB, {
        id: meta.id,
        now: Math.floor(Date.now() / 1000),
        reason,
        revoked: result.revoked,
        failed: result.failed,
      });
    }
  }

  /** The hard stop, scheduled by {@link open}. */
  async expire() {
    await this.end("time");
  }

  /** Ten minutes without a request: end the session rather than just sleep. */
  async onActivityExpired() {
    await this.end("idle");
  }

  /**
   * The VM stopped. When that was not teardown (a crash, or the platform
   * stopping it), the stores cannot be signed out any more; record it.
   *
   * @returns {Promise<void>}
   */
  async onStop() {
    const meta = await this.meta();
    if (meta && (meta.state === "ready" || meta.state === "starting")) {
      await this.ctx.storage.delete("secrets");
      await this.setState("ended");
      await finish(this.env.DB, {
        id: meta.id,
        now: Math.floor(Date.now() / 1000),
        reason: "stopped",
        revoked: 0,
        failed: 0,
      });
    }
  }
}
