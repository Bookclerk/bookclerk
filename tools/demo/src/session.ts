// One demo session: a Cloudflare Container running the demo image, owned by
// the Durable Object Cloudflare pairs with it. The object starts bookclerkd
// with a fresh operator token, which it uses only to set the host up and
// sign the visitor in, forwards the visitor's requests as the visitor,
// and on every way a session ends (Start over, 10 minutes idle, the hard
// stop, the sweep) signs their store accounts out before destroying the VM.
// The VM's disk is ephemeral, so destroying it deletes the library,
// master.key and every token with it.

import { Container } from "@cloudflare/containers";
import type { Env } from "./env.ts";
import { finish } from "./limits.ts";
import { type ContainerCall, enableStorefronts, randomHex, signInVisitor, signOutStores, waitForHealth } from "./lifecycle.ts";

/** bookclerkd's port inside the container (BOOKCLERK_DAEMON_LISTEN in the image). */
const PORT = 8787;

/** Longest wait for bookclerkd to answer /health after the VM starts. */
const BOOT_TIMEOUT_MS = 120_000;

/** Longest wait for each store sign-out request at teardown. */
const SIGN_OUT_TIMEOUT_MS = 10_000;

/** Seconds between attempts to destroy a VM whose destroy failed. */
const DESTROY_RETRY_SECONDS = 60;

/** Attempts to destroy a VM before giving up (about an hour). */
const DESTROY_ATTEMPTS = 60;

/** Where a session is. */
export type SessionState = "starting" | "ready" | "ending" | "ended" | "failed";

/**
 * What the object keeps in its storage. `secrets` is deleted when the
 * session ends.
 */
export type SessionMeta = {
  /** Session id (the D1 row and this object's name). */
  id: string;
  /** Hard stop, Unix seconds. */
  endsAt: number;
  /** Where the session is. */
  state: SessionState;
  /** Image version shown in the banner. */
  version: string;
  /** The demo's origin (`https://host`), which bookclerkd serves as its `public_origin`. */
  origin: string;
};

/** What the object keeps in storage while the session runs. */
type SessionSecrets = {
  /** The visitor's bookclerkd portal session. */
  portalSession: string;
};

/** A visitor's container and the object that owns it. */
export class DemoSession extends Container<Env> {
  /** bookclerkd's port. */
  override defaultPort = PORT;
  /** Idle time before onActivityExpired ends the session. */
  override sleepAfter = "10m";
  /** Store traffic leaves the VM directly; the jail applies each plugin's grants. */
  override enableInternet = true;

  /**
   * Reads the session's metadata.
   *
   * @returns The metadata, if opened.
   */
  async meta(): Promise<SessionMeta | undefined> {
    return this.ctx.storage.get<SessionMeta>("meta");
  }

  /**
   * Updates the session's state.
   *
   * @param state - New state.
   */
  async setState(state: SessionState): Promise<void> {
    const meta = await this.meta();
    if (meta) await this.ctx.storage.put("meta", { ...meta, state });
  }

  /**
   * Opens the session: records it and schedules the boot and the hard stop.
   * Calling it again for an open session does nothing.
   *
   * @param params - Session id, hard stop (Unix seconds), image version and the demo's origin.
   */
  async open(params: { id: string; endsAt: number; version: string; origin: string; }): Promise<void> {
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
   * @returns True while the state is still "starting".
   */
  async stillStarting(): Promise<boolean> {
    return (await this.meta())?.state === "starting";
  }

  /**
   * Starts the VM and signs the visitor in. Scheduled by {@link open}.
   *
   */
  async boot(): Promise<void> {
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
      const call: ContainerCall = (path, init) => this.containerFetch(`http://container${path}`, init, PORT);
      if (await this.stillStarting()) {
        await waitForHealth(call, { timeoutMs: BOOT_TIMEOUT_MS, intervalMs: 1000 });
      }
      if (await this.stillStarting()) await enableStorefronts(call, operatorToken);
      const portalSession = (await this.stillStarting()) ? await signInVisitor(call, operatorToken) : null;
      if (portalSession === null || !(await this.stillStarting())) {
        // Ended meanwhile: close() may have run before the VM was up, so
        // make sure it is gone. The end is already recorded.
        await this.retryDestroy(0);
        return;
      }
      // The operator token is not kept: from here on bookclerkd sees only
      // the visitor's own session.
      await this.ctx.storage.put("secrets", { portalSession });
      await this.setState("ready");
    } catch (err) {
      if (!(await this.stillStarting())) {
        await this.retryDestroy(0);
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
   */
  async status(): Promise<{ state: SessionState | "none"; endsAt: number; version: string; }> {
    const meta = await this.meta();
    if (!meta) return { state: "none", endsAt: 0, version: "" };
    return { state: meta.state, endsAt: meta.endsAt, version: meta.version };
  }

  /**
   * The visitor's portal session, handed to their browser once on entry.
   *
   * @returns The session when ready; otherwise null.
   */
  async portalSession(): Promise<string | null> {
    const meta = await this.meta();
    if (meta?.state !== "ready") return null;
    const secrets = await this.ctx.storage.get<SessionSecrets>("secrets");
    return secrets?.portalSession ?? null;
  }

  /**
   * Forwards a visitor's request to bookclerkd as is.
   *
   * The Worker has already checked the route and stripped every credential
   * but the visitor's portal session, so bookclerkd authorizes the request
   * as the visitor's Owner account, never as the operator.
   *
   * @param request - Request from the Worker.
   * @returns bookclerkd's answer, or 503 when the session
   *   is not running.
   */
  override async fetch(request: Request): Promise<Response> {
    const meta = await this.meta();
    const secrets = meta?.state === "ready" ? await this.ctx.storage.get<SessionSecrets>("secrets") : undefined;
    if (!secrets) {
      // The Worker turns this into the waiting page or the ended page.
      return Response.json(
        { error: "This demo session is not running." },
        { status: 503, headers: { "X-Demo-State": meta?.state ?? "none" } },
      );
    }
    return this.containerFetch(request, PORT);
  }

  /**
   * Ends the session: signs store accounts out, destroys the VM, records it.
   *
   * @param reason - Why: visitor, idle, time or sweep.
   * @returns Accounts signed out
   *   and failed sign-outs (zeros when it had already ended).
   */
  async end(reason: string): Promise<{ revoked: number; failed: number; }> {
    const meta = await this.meta();
    if (!meta || meta.state === "ending" || meta.state === "ended" || meta.state === "failed") {
      return { revoked: 0, failed: 0 };
    }
    const wasReady = meta.state === "ready";
    await this.setState("ending");
    let result = { revoked: 0, failed: 0 };
    const secrets = await this.ctx.storage.get<SessionSecrets>("secrets");
    if (wasReady && secrets) {
      const call: ContainerCall = (path, init) => this.containerFetch(`http://container${path}`, init, PORT);
      result = await signOutStores(call, secrets.portalSession, SIGN_OUT_TIMEOUT_MS, meta.origin);
    }
    await this.close(reason, result);
    return result;
  }

  /**
   * Destroys the VM, forgets the secrets and writes the session's end to D1.
   *
   * @param reason - Why the session ended.
   * @param result - Sign-out counts.
   */
  async close(reason: string, result: { revoked: number; failed: number; }): Promise<void> {
    const meta = await this.meta();
    await this.ctx.storage.delete("secrets");
    if (meta && meta.state !== "failed") await this.setState("ended");
    await this.retryDestroy(0);
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

  /**
   * Destroys the VM; when that fails, tries again every minute for about an
   * hour, so a platform hiccup cannot leave a visitor's VM (and the tokens
   * on its disk) running after the session ended.
   *
   * @param attempt - Attempts made before this one.
   */
  async retryDestroy(attempt: number): Promise<void> {
    try {
      await this.destroy();
    } catch (err) {
      const id = (await this.meta())?.id;
      console.error(`demo session ${id} destroy attempt ${attempt + 1} failed: ${String(err)}`);
      if (attempt + 1 < DESTROY_ATTEMPTS) await this.schedule(DESTROY_RETRY_SECONDS, "retryDestroy", attempt + 1);
    }
  }

  /** The hard stop, scheduled by {@link open}. */
  async expire(): Promise<void> {
    await this.end("time");
  }

  /** Ten minutes without a request: end the session rather than just sleep. */
  override async onActivityExpired(): Promise<void> {
    await this.end("idle");
  }

  /**
   * The VM stopped. When that was not teardown (a crash, or the platform
   * stopping it), the stores cannot be signed out any more; record it.
   *
   */
  override async onStop(): Promise<void> {
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
