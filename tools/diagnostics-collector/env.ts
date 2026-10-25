/**
 * The collector Worker's bindings, as `wrangler.toml` and the deploy
 * workflow declare them (see the header of worker.ts for what each is for).
 *
 * Every field is optional: a fork deploys without the dashboard or triage
 * bindings, and the routes that need one check it and answer without.
 */
export interface Env {
  /** B2 application key ID (secret). */
  B2_KEY_ID?: string;
  /** B2 application key (secret). */
  B2_APPLICATION_KEY?: string;
  /** B2 bucket reports are stored in (secret). */
  B2_BUCKET_ID?: string;
  /** B2 S3-compatible endpoint, when set (unused by the native API). */
  B2_ENDPOINT?: string;
  /** Key the ingest Action sends on every report and triage route (secret). */
  REPORT_API_KEY?: string;
  /** Salt for the hashed client IP; without it no hash is kept (secret). */
  CLIENT_IP_HASH_SALT?: string;
  /** Token for higher GitHub API rate limits (secret). */
  GITHUB_TOKEN?: string;
  /** The automation App's OAuth client ID (secret). */
  GITHUB_APP_CLIENT_ID?: string;
  /** The automation App's OAuth client secret (secret). */
  GITHUB_APP_CLIENT_SECRET?: string;
  /** The automation App's private key, PEM (secret). */
  GITHUB_APP_PRIVATE_KEY?: string;
  /** `owner/name` of the repository issues and releases live in. */
  GITHUB_REPOSITORY?: string;
  /** Workers AI model `/triage` runs. */
  TRIAGE_MODEL?: string;
  /** `/triage` neuron cap per UTC day, as decimal text. */
  TRIAGE_DAILY_NEURONS?: string;
  /** Reports the dashboard indexes per UTC day, as decimal text. */
  DASHBOARD_INDEX_DAILY_ROWS?: string;
  /** Workers AI, as far as `/triage` uses it. */
  AI?: TextModel;
  /** KV namespace holding the daily triage neuron ledger. */
  TRIAGE_BUDGET?: BudgetStore;
  /** D1 database with the dashboard's report index, ledger and sessions. */
  DASHBOARD_DB?: D1Database;
  /** The LiveHub Durable Object namespace. */
  LIVE?: DurableObjectNamespace;
  /** Static dashboard assets under /static/. */
  ASSETS?: Fetcher;
}

/**
 * The slice of the KV API the triage budget ledger uses. A `KVNamespace`
 * satisfies it; declaring only this much lets tests pass a Map-backed store.
 */
export interface BudgetStore {
  /**
   * Reads a value as text.
   *
   * @param key - Key.
   * @returns The stored text, or null when the key is unset.
   */
  get(key: string): Promise<string | null>;
  /**
   * Writes a value.
   *
   * @param key - Key.
   * @param value - Text to store.
   * @param options - `expirationTtl` in seconds.
   */
  put(key: string, value: string, options: { expirationTtl: number }): Promise<void>;
}

/** One chat message sent to a Workers AI text model. */
export interface ChatMessage {
  /** `system` or `user`. */
  role: "system" | "user";
  /** Message text. */
  content: string;
}

/**
 * The slice of the Workers AI binding `/triage` calls. An `Ai` binding
 * satisfies it; the answer is parsed defensively, so it is typed `unknown`.
 */
export interface TextModel {
  /**
   * Runs one chat completion.
   *
   * @param model - Model name, e.g. `@cf/ibm-granite/granite-4.0-h-micro`.
   * @param inputs - Messages and sampling limits.
   * @returns The model's answer, in whichever shape the model uses.
   */
  run(model: string, inputs: { messages: ChatMessage[]; max_tokens: number; temperature: number }): Promise<unknown>;
}

/** A Worker's way to keep work running after the response (`ctx`). */
export type Background = Pick<ExecutionContext, "waitUntil">;

/** Bindings once the dashboard is known to be configured. */
export type DashboardEnv = Env & {
  /** D1 database with the dashboard's report index, ledger and sessions. */
  DASHBOARD_DB: D1Database;
};
