// The demo Worker's bindings, as wrangler.toml and the deploy workflow
// declare them.

/** What the Worker asks of a visitor's `DemoSession` object over RPC. */
export interface SessionApi {
  /**
   * Records a new session and boots its container.
   *
   * @param params - Session id, hard stop (Unix seconds), image version
   *   and the demo's origin.
   */
  open(params: { id: string; endsAt: number; version: string; origin: string }): Promise<void>;
  /**
   * Where the session is.
   *
   * @returns State, hard stop (Unix seconds) and image version.
   */
  status(): Promise<{ state: string; endsAt: number; version: string }>;
  /**
   * The visitor's bookclerkd portal session, once signed in.
   *
   * @returns The session cookie value, or null.
   */
  portalSession(): Promise<string | null>;
  /**
   * Ends the session: signs the visitor out of the stores and stops the VM.
   *
   * @param reason - Why (`visitor`, `restart`, `sweep`, ...).
   * @returns How many store accounts were revoked and how many failed.
   */
  end(reason: string): Promise<{ revoked: number; failed: number }>;
  /**
   * Forwards one request to the visitor's container.
   *
   * @param request - Request to forward.
   * @returns bookclerkd's answer.
   */
  fetch(request: Request): Promise<Response>;
}

/**
 * The `SESSIONS` Durable Object namespace, as far as the Worker uses it.
 * Cloudflare's `DurableObjectNamespace<DemoSession>` satisfies it.
 */
export interface SessionNamespace {
  /**
   * The id of the object with a given name.
   *
   * @param name - Session id.
   * @returns Object id.
   */
  idFromName(name: string): DurableObjectId;
  /**
   * The stub for an object.
   *
   * @param id - Object id from `idFromName`.
   * @returns RPC stub.
   */
  get(id: DurableObjectId): SessionApi;
}

/**
 * The Worker's bindings. Secrets and vars are optional: the Worker checks
 * them (`configured`, `limitsFrom`) and refuses or falls back without them.
 */
export interface Env {
  /** Sessions, the daily budget and rate counters. */
  DB: D1Database;
  /** One `DemoSession` object (and container) per visitor. */
  SESSIONS: SessionNamespace;
  /** HMAC key naming sessions and hashing addresses (secret). */
  SESSION_KEY?: string;
  /** Turnstile widget site key (secret). */
  TURNSTILE_SITE_KEY?: string;
  /** Turnstile widget secret (secret). */
  TURNSTILE_SECRET?: string;
  /** Image version shown in the banner. */
  DEMO_VERSION?: string;
  /** `https://` URL of the project website, linked from the demo's pages. */
  SITE_URL?: string;
  /** Sessions that may run at once, as decimal text. */
  MAX_LIVE_SESSIONS?: string;
  /** Visitor-hours per UTC day, as decimal text. */
  DAILY_BUDGET_HOURS?: string;
  /** Hard length of one session in minutes, as decimal text. */
  SESSION_MINUTES?: string;
  /** New sessions per address per UTC hour, as decimal text. */
  SESSIONS_PER_HOUR?: string;
  /** Store sign-in attempts per session, as decimal text. */
  SIGNINS_PER_SESSION?: string;
  /** Store sign-in attempts per address per UTC day, as decimal text. */
  SIGNINS_PER_DAY?: string;
}
