/**
 * What each Audiobookshelf entrypoint method does, apart from the SDK.
 *
 * `index.ts` adapts these functions to the entrypoint classes; keeping them
 * here, with every binding passed in, lets the unit tests drive the same
 * code the isolate runs. The module imports only types from the SDK.
 */

import type {
  DatabaseBinding,
  ExternalUser,
  HealthOk,
  HostWebSocket,
  ListeningProgress,
  SocketMessageData,
  SocketsBinding,
} from "@bookclerk/plugin-sdk/workerd";
import { AbsClient, AbsFailure, RequestBudget, type Fetch } from "./client.ts";
import { collectListeningChanges } from "./listening.ts";
import { PING, PONG, onFrame, socketUrl } from "./push.ts";
import { PROVIDER, type Settings } from "./settings.ts";
import { UserWatch } from "./watch.ts";

/**
 * Requests one listening sync may make: the host's cap (`subrequests` in
 * `plugin.toml`) less a margin for the calls around it.
 */
export const SYNC_REQUEST_BUDGET = 990;

/** Message for a missing API key, naming the command that sets it. */
export const MISSING_KEY =
  "the audiobookshelf `api_key` secret is required (bookclerk plugins config set audiobookshelf api_key --secret)";

/** What the isolate keeps between calls. */
export interface GuestState {
  /** Keys of the users already reported, used only without the plugin database. */
  readonly knownUsers: Set<string>;
  /** Tail of the call chain; calls that touch the watch or the memory run one at a time. */
  tail: Promise<unknown>;
}

/**
 * A fresh isolate state.
 *
 * @returns Empty state.
 */
export function newState(): GuestState {
  return { knownUsers: new Set(), tail: Promise.resolve() };
}

/**
 * Runs `work` after every earlier serialized call has settled.
 *
 * @param state - The isolate state.
 * @param work - The call.
 * @returns What `work` returns.
 */
export function serialized<T>(state: GuestState, work: () => Promise<T>): Promise<T> {
  const run = state.tail.then(work, work);
  state.tail = run.catch(() => undefined);
  return run;
}

/** The bindings and settings one call works with. */
export interface Context {
  /** The plugin settings. */
  readonly settings: Settings;
  /** How requests are sent. */
  readonly fetch: Fetch;
  /** The `LISTENING_STATE` database, when granted. */
  readonly db?: DatabaseBinding | undefined;
  /** The `SOCKETS` binding, when granted. */
  readonly sockets?: SocketsBinding | undefined;
}

/**
 * The Audiobookshelf client for `ctx`.
 *
 * @param ctx - Settings and `fetch`.
 * @param budget - Requests the call may make.
 * @returns The client.
 * @throws {AbsFailure} `invalid_params` without an API key or with a base
 *   URL the client refuses.
 */
export function clientFor(ctx: Context, budget?: RequestBudget): AbsClient {
  if (ctx.settings.apiKey === null) throw new AbsFailure("invalid_params", MISSING_KEY);
  return new AbsClient(ctx.settings.baseUrl, ctx.settings.apiKey, ctx.fetch, budget);
}

/**
 * Whether the plugin is configured, and why not.
 *
 * @param ctx - Settings and `fetch`.
 * @returns The client, or the configuration failure.
 */
function tryClient(ctx: Context): AbsClient | AbsFailure {
  try {
    return clientFor(ctx);
  } catch (err) {
    if (err instanceof AbsFailure) return err;
    throw err;
  }
}

/**
 * `RemoteLibrary.health`: checks the API key.
 *
 * A misconfiguration is reported as not ok rather than thrown, so status
 * pages can show it.
 *
 * @param ctx - Settings and `fetch`.
 * @returns Health and detail.
 */
export async function health(ctx: Context): Promise<HealthOk> {
  const client = tryClient(ctx);
  if (client instanceof AbsFailure) return { ok: false, detail: client.message };
  try {
    const user = await client.authorize();
    return { ok: true, detail: user === null ? "" : `authorized as ${user.username}` };
  } catch (err) {
    return { ok: false, detail: err instanceof Error ? err.message : String(err) };
  }
}

/**
 * `RemoteLibrary.diagnose`: who the key belongs to and the libraries it sees.
 *
 * @param ctx - Settings and `fetch`.
 * @returns One line per finding.
 * @throws {AbsFailure} When the plugin is misconfigured or a call fails.
 */
export async function diagnose(ctx: Context): Promise<string[]> {
  const client = clientFor(ctx);
  const user = await client.authorize();
  const lines = [
    user === null
      ? "authorized (no user in response)"
      : `authorized as ${user.username} (${user.id})`,
  ];
  for (const library of await client.listLibraries()) {
    lines.push(`library ${library.id} — ${library.name}`);
  }
  return lines;
}

/**
 * Opens the pushed-event socket when none is open, so the host learns of a
 * new user as soon as the server announces it.
 *
 * Does nothing without the `SOCKETS` binding, with the user watch off, or
 * without an API key. A socket that closes is not reopened at once: the
 * next start or poll opens it again, so a server that keeps refusing the
 * key is retried at the poll's pace rather than in a loop. The host answers
 * the server's Engine.IO ping itself, so an open socket costs no wakes.
 *
 * @param ctx - Settings and the `SOCKETS` binding.
 * @returns `true` when this call opened a socket.
 * @throws The host's error when the sockets cannot be listed or opened.
 */
export async function ensurePush(ctx: Context): Promise<boolean> {
  const sockets = ctx.sockets;
  if (sockets === undefined || !ctx.settings.watchUsers || ctx.settings.apiKey === null)
    return false;
  if (tryClient(ctx) instanceof AbsFailure) return false;
  const url = socketUrl(ctx.settings.baseUrl);
  if (url === null) return false;
  if ((await sockets.getWebSockets()).length > 0) return false;
  await sockets.connect(url, { autoResponses: [{ request: PING, response: PONG }] });
  console.info("opened the Audiobookshelf pushed-event socket");
  return true;
}

/**
 * {@link ensurePush}, logging a failure instead of throwing: polling still
 * finds every new user.
 *
 * @param ctx - Settings and the `SOCKETS` binding.
 */
async function ensurePushQuietly(ctx: Context): Promise<void> {
  try {
    await ensurePush(ctx);
  } catch (err) {
    console.warn(
      `could not open the Audiobookshelf pushed-event socket; polling continues: ${String(err)}`,
    );
  }
}

/**
 * `RemoteLibrary.start`: seeds the user watch with the users the server
 * already has, then opens the pushed-event socket.
 *
 * With the plugin database only the first start ever records users;
 * without it every start seeds the in-memory set again. A failed seed is
 * logged, and the next poll seeds instead and reports nobody.
 *
 * @param state - The isolate state.
 * @param ctx - Settings and bindings.
 * @throws {AbsFailure} When the plugin is misconfigured.
 */
export async function start(state: GuestState, ctx: Context): Promise<void> {
  const client = clientFor(ctx);
  if (ctx.settings.watchUsers) {
    if (ctx.db === undefined) {
      console.warn(
        "Audiobookshelf user watch has no LISTENING_STATE database; users created while the plugin sleeps are not reported",
      );
    }
    await serialized(state, async () => {
      let users;
      try {
        users = await client.listUsers();
      } catch (err) {
        console.warn(`failed to seed the Audiobookshelf user watch: ${String(err)}`);
        return;
      }
      try {
        const count = await new UserWatch(ctx.db, state.knownUsers).seed(users);
        if (count > 0) console.info(`seeded the Audiobookshelf user watch with ${count} users`);
      } catch (err) {
        console.warn(`failed to seed the Audiobookshelf user watch: ${String(err)}`);
      }
    });
  }
  // Serialized, so a start and a poll that overlap open one socket.
  await serialized(state, () => ensurePushQuietly(ctx));
}

/**
 * `RemoteLibrary.pollEvents`: the users created since the last poll.
 *
 * Opens the pushed-event socket when it is closed, then lists the server's
 * users and returns those the watch has not reported. When the watch was
 * never seeded (the seed at start failed), this poll seeds it and reports
 * nobody.
 *
 * @param state - The isolate state.
 * @param ctx - Settings and bindings.
 * @returns The new users.
 * @throws {AbsFailure} When the plugin is misconfigured or the users
 *   cannot be listed; the binding's error when the watch cannot be read.
 */
export async function pollEvents(state: GuestState, ctx: Context): Promise<ExternalUser[]> {
  // Serialized, so a start and a poll that overlap open one socket.
  await serialized(state, () => ensurePushQuietly(ctx));
  if (!ctx.settings.watchUsers) return [];
  const client = clientFor(ctx);
  return serialized(state, async () => {
    const users = await client.listUsers();
    const watch = new UserWatch(ctx.db, state.knownUsers);
    if ((await watch.seed(users)) > 0) return [];
    return (await watch.takeNew(users)).map((user) => {
      const external: ExternalUser = { provider: PROVIDER, externalUserId: user.id };
      if (user.username !== "") external.displayName = user.username;
      return external;
    });
  });
}

/**
 * `RemoteLibrary.scanLibrary`: asks the server to rescan the configured
 * library.
 *
 * @param ctx - Settings and `fetch`.
 * @param force - Whether to ask for a forced rescan.
 * @throws {AbsFailure} When the plugin is misconfigured, `library_id` is
 *   unset, or the call fails.
 */
export async function scanLibrary(ctx: Context, force: boolean): Promise<void> {
  const client = clientFor(ctx);
  if (ctx.settings.libraryId === null) {
    throw new AbsFailure("invalid_params", "the audiobookshelf `library_id` setting is unset");
  }
  await client.scanLibrary(ctx.settings.libraryId, force);
}

/**
 * `RemoteLibrary.syncListening`: the progress rows that changed since the
 * last sync (every row without the plugin database).
 *
 * @param state - The isolate state.
 * @param ctx - Settings and bindings.
 * @param nowMs - The time of this sync, UTC Unix milliseconds.
 * @returns The rows for the host to upsert.
 * @throws {AbsFailure} When the plugin is misconfigured or the users
 *   cannot be listed.
 */
export async function syncListening(
  state: GuestState,
  ctx: Context,
  nowMs: number,
): Promise<ListeningProgress[]> {
  const client = clientFor(ctx, new RequestBudget(SYNC_REQUEST_BUDGET));
  return serialized(state, async () => {
    const sync = await collectListeningChanges(client, ctx.db, nowMs);
    if (!sync.complete) {
      console.info(
        "Audiobookshelf listening sync reached its request budget; some progress was not sent",
      );
    }
    return sync.rows;
  });
}

/**
 * `Oidc.authenticateUser`: signs a person in with their Audiobookshelf
 * credentials, when the operator allows it.
 *
 * @param ctx - Settings and `fetch`.
 * @param username - Login name.
 * @param password - Password; never logged.
 * @returns The signed-in user.
 * @throws {AbsFailure} `forbidden` when credential sign-in is off, and the
 *   client's failures.
 */
export async function authenticateUser(
  ctx: Context,
  username: string,
  password: string,
): Promise<ExternalUser> {
  if (!ctx.settings.allowCredentialLogin) {
    throw new AbsFailure("forbidden", "signing in with Audiobookshelf credentials is turned off");
  }
  const user = await clientFor(ctx).authenticateUser(username, password);
  const external: ExternalUser = { provider: user.provider, externalUserId: user.externalUserId };
  if (user.displayName !== "") external.displayName = user.displayName;
  if (user.accessToken !== null) external.accessToken = user.accessToken;
  return external;
}

/** What to do with one delivered event. */
export type EventDecision =
  { readonly kind: "ack" } | { readonly kind: "retry"; readonly reason: string };

/**
 * Handles one delivered event: after `book_acquired`, asks the server to
 * rescan the configured library.
 *
 * Other event types, a turned-off `notify_scan_on_acquire` and an unset
 * `library_id` acknowledge the event without a request.
 *
 * @param ctx - Settings and `fetch`.
 * @param eventType - The event's type.
 * @returns `ack`, or `retry` with the reason when the plugin is
 *   misconfigured or the scan request failed.
 */
export async function onEvent(ctx: Context, eventType: string): Promise<EventDecision> {
  if (eventType !== "book_acquired" || !ctx.settings.notifyScanOnAcquire) return { kind: "ack" };
  if (ctx.settings.libraryId === null) {
    console.warn("Audiobookshelf scan skipped: the `library_id` setting is unset");
    return { kind: "ack" };
  }
  const client = tryClient(ctx);
  if (client instanceof AbsFailure) return { kind: "retry", reason: client.message };
  try {
    await client.scanLibrary(ctx.settings.libraryId, false);
    return { kind: "ack" };
  } catch (err) {
    return { kind: "retry", reason: err instanceof Error ? err.message : String(err) };
  }
}

/**
 * Answers one frame pushed on the Audiobookshelf socket.
 *
 * Sends the handshake replies (joining the namespace, then authenticating
 * with the API key), closes the socket when the server rejects the key, and
 * reports whether the frame announced a new user.
 *
 * @param ctx - Settings (the API key).
 * @param ws - The socket the frame came on.
 * @param message - The frame.
 * @returns `true` when the server announced a user created on it.
 * @throws The host's error when a reply cannot be sent.
 */
export async function pushFrame(
  ctx: Context,
  ws: HostWebSocket,
  message: SocketMessageData,
): Promise<boolean> {
  if (typeof message !== "string") return false;
  const reaction = onFrame(message, ctx.settings.apiKey ?? "");
  if (reaction.reply !== null) await ws.send(reaction.reply);
  if (reaction.close) {
    console.warn("Audiobookshelf closed the pushed-event channel; the next poll opens it again");
    await ws.close(1000, "");
  }
  return reaction.newUser;
}
