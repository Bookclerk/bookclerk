/**
 * A storage change feed over a queue of notification messages: the
 * receive, delete and release cycle a bucket's event notifications need.
 *
 * Messages come from a {@link NotificationQueue} (the plugin's `WEBHOOKS`
 * binding through {@link webhookQueue}, or any queue with leases), a
 * plugin-supplied parser turns each message into changes, and the feed
 * hands them out as `changes` pages. A message is acknowledged only once
 * the host sends back a cursor at or past the page that carried it, so a
 * page the host never finished is offered again. The feed's own state (its
 * id, sequence and the leases of pages not yet confirmed) lives in the
 * plugin's KV binding, because the isolate sleeps between calls.
 *
 * Cursors read `q1.<feed>.<seq>`; the Rust storage helpers and the Python
 * SDK write the same.
 */

import { MAX_WEBHOOK_PULL_BATCH, MAX_WEBHOOK_VISIBILITY_MS } from "./abi.js";
import type { KvNamespaceBinding, WebhooksBinding } from "./env.js";
import { PluginError } from "./errors.js";
import type { ChangeFeed } from "./object-store.js";
import type { ChangesPage, ObjectChange } from "./plugin.js";

/** First part of every cursor the feed writes. */
const CURSOR_TAG = "q1";
/** Messages asked for per receive. */
const RECEIVE_BATCH = 10;
/** Pages whose leases the feed remembers; older ones are let go. */
const MAX_PENDING_PAGES = 64;
/** Most changes per page. */
const MAX_PAGE = 256;
/** How long a received message stays leased: 12 hours, within the binding's cap. */
export const QUEUE_LEASE_MS = Math.min(12 * 60 * 60 * 1000, MAX_WEBHOOK_VISIBILITY_MS);

/** One leased message. */
export interface LeasedMessage {
  /** The message body. */
  body: Uint8Array;
  /** The lease to acknowledge or release it by. */
  receipt: string;
}

/** What one receive returned. */
export interface Received {
  /** Leased messages, oldest first. */
  messages: LeasedMessage[];
  /** True when the queue dropped messages since the last receive. */
  missed: boolean;
}

/** A queue of notification messages with leases. */
export interface NotificationQueue {
  /**
   * Leases up to `max` waiting messages.
   *
   * @param max - Most messages, 1 to {@link RECEIVE_BATCH}.
   * @returns The messages.
   */
  receive(max: number): Promise<Received>;
  /**
   * Acknowledges handled messages.
   *
   * @param receipts - Their leases.
   * @returns Resolves once recorded.
   */
  delete(receipts: string[]): Promise<void>;
  /**
   * Offers messages again on the next receive.
   *
   * @param receipts - Their leases.
   * @returns Resolves once recorded.
   */
  release(receipts: string[]): Promise<void>;
}

/** One change read from a message, with what orders it. */
export interface ParsedChange {
  /** The change. */
  change: ObjectChange;
  /** Comparable event time (text that sorts in time order), or `null`. */
  time: string | null;
  /** Per-key event sequencer (hex, sorts after zero-padding), or `null`. */
  sequencer: string | null;
}

/**
 * Reads one message body into changes. Throw to drop an unreadable message
 * (the page then says it may have missed changes).
 */
export type MessageParser = (body: Uint8Array) => ParsedChange[];

/**
 * The plugin's `WEBHOOKS` binding as a {@link NotificationQueue} for one
 * `[[webhooks]]` row.
 *
 * @param webhooks - The binding.
 * @param name - The row's name.
 * @returns The queue.
 */
export function webhookQueue(webhooks: WebhooksBinding, name: string): NotificationQueue {
  const settle = async (acks: string[], retries: string[]): Promise<void> => {
    if (acks.length === 0 && retries.length === 0) return;
    await webhooks.ack(name, { acks, retries });
  };
  return {
    async receive(max) {
      const batchSize = Math.min(Math.max(max, 1), MAX_WEBHOOK_PULL_BATCH);
      const page = await webhooks.pull(name, { batchSize, visibilityTimeoutMs: QUEUE_LEASE_MS });
      return {
        messages: page.messages.map((message) => ({
          body: message.body,
          receipt: message.leaseId,
        })),
        missed: page.missed,
      };
    },
    delete: (receipts) => settle(receipts, []),
    release: (receipts) => settle([], receipts),
  };
}

/** One page whose leases wait for the host's next cursor. */
interface PendingPage {
  /** The page's sequence number. */
  seq: number;
  /** Leases of the messages it carried. */
  receipts: string[];
}

/** What the feed keeps between calls. */
interface FeedState {
  /** Random id; a cursor of another feed id is a new start. */
  feed: string;
  /** Sequence number the next page gets. */
  nextSeq: number;
  /** Pages not yet confirmed, oldest first. */
  pending: PendingPage[];
  /** True while a page is being built, or after a release failed. */
  receiving: boolean;
  /** Sequence of the first page that may have missed changes, if any. */
  gapAt: number | null;
}

/**
 * A fresh state with a new feed id.
 *
 * @returns The state.
 */
function freshState(): FeedState {
  return {
    feed: crypto.randomUUID().replaceAll("-", ""),
    nextSeq: 1,
    pending: [],
    receiving: false,
    gapAt: null,
  };
}

/**
 * Reads a stored state, or `null` when it is missing or unreadable.
 *
 * @param value - The stored JSON value.
 * @returns The state, or `null`.
 */
function readState(value: unknown): FeedState | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  const pending = v["pending"];
  if (
    typeof v["feed"] !== "string" ||
    v["feed"] === "" ||
    !Number.isSafeInteger(v["nextSeq"]) ||
    !Array.isArray(pending)
  ) {
    return null;
  }
  const pages: PendingPage[] = [];
  for (const page of pending as unknown[]) {
    if (typeof page !== "object" || page === null) return null;
    const p = page as Record<string, unknown>;
    const receipts = p["receipts"];
    if (!Number.isSafeInteger(p["seq"]) || !Array.isArray(receipts)) return null;
    if (!(receipts as unknown[]).every((receipt) => typeof receipt === "string")) return null;
    pages.push({ seq: p["seq"] as number, receipts: receipts as string[] });
  }
  const gapAt = v["gapAt"];
  return {
    feed: v["feed"],
    nextSeq: v["nextSeq"] as number,
    pending: pages,
    receiving: v["receiving"] === true,
    gapAt: Number.isSafeInteger(gapAt) ? (gapAt as number) : null,
  };
}

/**
 * Splits a cursor into its feed id and sequence number.
 *
 * @param cursor - A cursor this kind of feed wrote.
 * @returns The feed id and sequence.
 * @throws {PluginError} `invalid_cursor` for any other text.
 */
export function parseQueueCursor(cursor: string): { feed: string; seq: number } {
  const invalid = (): PluginError =>
    PluginError.fromWire("invalid_cursor", "not a notification-queue cursor");
  const first = cursor.indexOf(".");
  const second = first < 0 ? -1 : cursor.indexOf(".", first + 1);
  if (first < 0 || second < 0 || cursor.slice(0, first) !== CURSOR_TAG) throw invalid();
  const feed = cursor.slice(first + 1, second);
  const digits = cursor.slice(second + 1);
  if (feed === "" || !/^\+?[0-9]+$/u.test(digits)) throw invalid();
  const seq = Number(digits);
  if (!Number.isSafeInteger(seq)) throw invalid();
  return { feed, seq };
}

/**
 * Compares two sequencers as hex numbers of any length.
 *
 * @param a - One sequencer.
 * @param b - The other.
 * @returns Negative, zero or positive.
 */
function sequencerCompare(a: string, b: string): number {
  const width = Math.max(a.length, b.length);
  const x = a.padStart(width, "0").toUpperCase();
  const y = b.padStart(width, "0").toUpperCase();
  return x < y ? -1 : x > y ? 1 : 0;
}

/**
 * Orders changes by time (unknown first, stable), then each key's changes
 * by sequencer among the places that key already holds.
 *
 * @param events - Changes, reordered in place.
 */
export function orderChanges(events: ParsedChange[]): void {
  events.sort((a, b) => {
    if (a.time === b.time) return 0;
    if (a.time === null) return -1;
    if (b.time === null) return 1;
    return a.time < b.time ? -1 : 1;
  });
  const byKey = new Map<string, number[]>();
  events.forEach((event, at) => {
    if (event.sequencer === null) return;
    const slots = byKey.get(event.change.key) ?? [];
    slots.push(at);
    byKey.set(event.change.key, slots);
  });
  for (const slots of byKey.values()) {
    if (slots.length < 2) continue;
    const group = slots.map((at) => events[at] as ParsedChange);
    group.sort((a, b) => sequencerCompare(a.sequencer ?? "", b.sequencer ?? ""));
    slots.forEach((at, i) => {
      events[at] = group[i] as ParsedChange;
    });
  }
}

/** Module-wide locks, so two calls in one isolate never interleave on one feed. */
const LOCKS = new Map<string, Promise<unknown>>();

/**
 * Runs `work` after every earlier call holding `key` has finished.
 *
 * @param key - The lock name.
 * @param work - The work.
 * @returns What `work` returns.
 */
async function locked<T>(key: string, work: () => Promise<T>): Promise<T> {
  const before = LOCKS.get(key) ?? Promise.resolve();
  const run = before.then(work, work);
  const settled = run.then(
    () => undefined,
    () => undefined,
  );
  LOCKS.set(key, settled);
  try {
    return await run;
  } finally {
    if (LOCKS.get(key) === settled) LOCKS.delete(key);
  }
}

/** Options for {@link QueueFeed}. */
export interface QueueFeedOptions {
  /** Where the messages come from. */
  queue: NotificationQueue;
  /** Reads one message into changes. */
  parse: MessageParser;
  /** KV binding the feed's state lives in. */
  kv: KvNamespaceBinding;
  /**
   * KV key of the state; name it after the queue, bucket and prefix so a
   * changed setting starts a new feed.
   */
  stateKey: string;
}

/** A change feed over a {@link NotificationQueue}; see the module docs. */
export class QueueFeed implements ChangeFeed {
  /** The options. */
  readonly #options: QueueFeedOptions;

  /**
   * Creates the feed; nothing is read until the first page.
   *
   * @param options - Queue, parser and state storage.
   */
  constructor(options: QueueFeedOptions) {
    this.#options = options;
  }

  /**
   * Loads the saved state, or a fresh one.
   *
   * @returns The state.
   */
  async #load(): Promise<FeedState> {
    const stored = await this.#options.kv.get(this.#options.stateKey, "json");
    return readState(stored) ?? freshState();
  }

  /**
   * Saves the state.
   *
   * @param state - The state.
   * @returns Resolves once stored.
   */
  async #persist(state: FeedState): Promise<void> {
    await this.#options.kv.put(this.#options.stateKey, JSON.stringify(state));
  }

  /**
   * Releases `receipts`, a batch at a time.
   *
   * @param receipts - Leases to release.
   * @returns Whether every release went through.
   */
  async #releaseAll(receipts: string[]): Promise<boolean> {
    let released = true;
    for (let at = 0; at < receipts.length; at += RECEIVE_BATCH) {
      try {
        await this.#options.queue.release(receipts.slice(at, at + RECEIVE_BATCH));
      } catch {
        released = false;
      }
    }
    return released;
  }

  /**
   * Acknowledges pending pages up to `seq` and releases the rest.
   *
   * @param state - The state, changed in place and saved.
   * @param seq - The last page the host confirmed.
   * @returns Whether every release went through.
   */
  async #settle(state: FeedState, seq: number): Promise<boolean> {
    if (state.pending.length === 0) return true;
    const read = state.pending.filter((page) => page.seq <= seq).flatMap((page) => page.receipts);
    const later = state.pending.filter((page) => page.seq > seq).flatMap((page) => page.receipts);
    for (let at = 0; at < read.length; at += RECEIVE_BATCH) {
      await this.#options.queue.delete(read.slice(at, at + RECEIVE_BATCH));
    }
    const released = await this.#releaseAll(later);
    state.pending = [];
    // The leases are forgotten here, so a failed release must be remembered
    // in the same write, before the page reporting it exists.
    state.receiving = state.receiving || !released;
    await this.#persist(state);
    return released;
  }

  /**
   * Records a page and builds its cursor.
   *
   * @param state - The state, replaced on success.
   * @param events - The page's changes.
   * @param receipts - Leases of the messages it carried.
   * @param more - Whether more changes are ready.
   * @param complete - Whether the page may have missed nothing.
   * @returns The page.
   */
  async #handOut(
    state: FeedState,
    events: ParsedChange[],
    receipts: string[],
    more: boolean,
    complete: boolean,
  ): Promise<ChangesPage> {
    const seq = state.nextSeq;
    const next: FeedState = {
      ...state,
      pending: [...state.pending],
      nextSeq: seq + 1,
      receiving: false,
      gapAt: complete ? state.gapAt : seq,
    };
    if (receipts.length > 0) {
      next.pending.push({ seq, receipts });
      while (next.pending.length > MAX_PENDING_PAGES) next.pending.shift();
    }
    try {
      await this.#persist(next);
    } catch (err) {
      await this.#releaseAll(receipts);
      throw err;
    }
    Object.assign(state, next);
    return {
      changes: events.map((event) => event.change),
      cursor: `${CURSOR_TAG}.${next.feed}.${seq}`,
      more,
      complete,
    };
  }

  /**
   * One page of changes after `cursor`.
   *
   * Without a cursor of this feed the caller starts over: everything handed
   * out before is released and the page is only the current position (the
   * host lists). Otherwise pages up to the cursor are acknowledged and the
   * page holds the next messages' changes.
   *
   * @param cursor - The previous page's cursor, or `undefined`.
   * @param limit - Requested changes (0 for {@link MAX_PAGE}).
   * @returns The page.
   * @throws {PluginError} `invalid_cursor` for a cursor that is not this
   *   feed's or is ahead of it; the queue's or KV's own errors.
   */
  changes(cursor: string | undefined, limit: number): Promise<ChangesPage> {
    return locked(this.#options.stateKey, () => this.#changes(cursor, limit));
  }

  /**
   * {@link QueueFeed.changes} under the lock.
   *
   * @param cursor - The previous page's cursor, or `undefined`.
   * @param limitAsked - Requested changes.
   * @returns The page.
   */
  async #changes(cursor: string | undefined, limitAsked: number): Promise<ChangesPage> {
    const limit = limitAsked === 0 ? MAX_PAGE : Math.min(Math.max(limitAsked, 1), MAX_PAGE);
    const state = await this.#load();
    let readUpTo: number | null = null;
    if (cursor !== undefined) {
      const { feed, seq } = parseQueueCursor(cursor);
      if (feed === state.feed && seq >= state.nextSeq) {
        throw PluginError.fromWire("invalid_cursor", "queue cursor is ahead of the feed");
      }
      if (feed === state.feed) readUpTo = seq;
    }
    if (readUpTo === null) {
      // The page is incomplete anyway, so a failed release changes nothing
      // the caller is told.
      await this.#settle(state, 0);
      return this.#handOut(state, [], [], false, false);
    }
    let complete = (await this.#settle(state, readUpTo)) && !state.receiving;
    if (state.gapAt !== null) {
      if (readUpTo < state.gapAt) complete = false;
      else state.gapAt = null;
    }
    state.receiving = true;
    await this.#persist(state);

    const events: ParsedChange[] = [];
    const receipts: string[] = [];
    let full = false;
    // Messages count against the page too, so a backlog of skipped or
    // unreadable messages cannot make one call drain the whole queue.
    let taken = 0;
    while (events.length < limit) {
      if (taken >= limit) {
        full = true;
        break;
      }
      const want = Math.min(limit - Math.max(events.length, taken), RECEIVE_BATCH);
      let received: Received;
      try {
        received = await this.#options.queue.receive(want);
      } catch (err) {
        await this.#releaseAll(receipts);
        throw err;
      }
      complete = complete && !received.missed;
      taken += received.messages.length;
      const overflow: string[] = [];
      for (const message of received.messages) {
        let parsed: ParsedChange[];
        try {
          parsed = this.#options.parse(message.body);
        } catch {
          receipts.push(message.receipt);
          complete = false;
          continue;
        }
        if (events.length + parsed.length <= limit) {
          events.push(...parsed);
          receipts.push(message.receipt);
        } else if (events.length === 0) {
          // One message with more changes than a page holds: hand out what
          // fits and say the rest was missed.
          events.push(...parsed.slice(0, limit));
          receipts.push(message.receipt);
          complete = false;
        } else {
          overflow.push(message.receipt);
        }
      }
      if (overflow.length > 0) {
        complete = (await this.#releaseAll(overflow)) && complete;
        full = true;
        break;
      }
      if (received.messages.length === 0) break;
      if (events.length >= limit) full = true;
    }
    orderChanges(events);
    return this.#handOut(state, events, receipts, full, complete);
  }
}
