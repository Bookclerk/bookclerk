/**
 * Coalesces many single lookups into a few batch requests.
 *
 * Discover asks the daemon for each card's prices and details, and the
 * daemon asks every store once per batch, so the fewer and fuller the
 * batches, the fewer store calls a page of cards costs. A batcher here:
 *
 * - collects the lookups asked within a short window into one batch, up to
 *   the route's limit, and splits anything larger into several;
 * - keeps **one batch in flight** at a time, so lookups asked while one is
 *   running wait and go out together in the next one, and a slow store
 *   never holds more than one of the browser's connections;
 * - asks once for a lookup already waiting or running with the same key
 *   (within one viewer generation);
 * - runs an optional `before` step ahead of each batch (Discover waits
 *   there for the covers on screen, see `coverLoads.ts`).
 *
 * @module
 */

/** How a batcher reaches its route. */
export interface LookupBatcherOptions<Q, R> {
  /** Most queries in one request (the route's own limit). */
  maxBatch: number;
  /** How long, in milliseconds, the first lookup waits for others to join it. */
  windowMs: number;
  /** Identity of a query: lookups with the same key share one answer. */
  keyOf: (query: Q) => string;
  /**
   * Sends one batch. Resolves to one result per query, in order; a
   * rejection rejects every lookup in the batch.
   */
  send: (queries: Q[]) => Promise<R[]>;
  /** Awaited before each batch is sent; its failure is ignored. */
  before?: () => Promise<void>;
  /**
   * The current viewer generation (see `lookupCache.ts`). A lookup asked
   * in a later generation never shares a lookup asked before it, so an
   * answer meant for a signed-out viewer never reaches the next one.
   */
  generation?: () => number;
}

/** One lookup waiting for its batch, and everyone waiting on its answer. */
interface Waiter<Q, R> {
  /** The query sent for this key. */
  query: Q;
  /** Settles every caller that asked for this key. */
  resolve: (value: R) => void;
  /** Rejects every caller that asked for this key. */
  reject: (err: unknown) => void;
}

/**
 * Creates a batcher over `options.send`.
 *
 * @param options - The route's limit, window, key and sender.
 * @returns A function that looks one query up through the batcher.
 */
export function createLookupBatcher<Q, R>(
  options: LookupBatcherOptions<Q, R>,
): (query: Q) => Promise<R> {
  const maxBatch = Math.max(1, options.maxBatch);
  let queue: Waiter<Q, R>[] = [];
  const pending = new Map<string, Promise<R>>();
  let timer: ReturnType<typeof setTimeout> | null = null;
  let inFlight = false;

  const schedule = () => {
    if (timer != null || inFlight || queue.length === 0) return;
    timer = setTimeout(() => {
      timer = null;
      void drain();
    }, options.windowMs);
  };

  const drain = async () => {
    inFlight = true;
    try {
      while (queue.length > 0) {
        if (options.before) {
          try {
            await options.before();
          } catch {
            // Waiting is a courtesy; the lookups still go out.
          }
        }
        // Lookups asked during the wait join this batch.
        const chunk = queue.slice(0, maxBatch);
        queue = queue.slice(chunk.length);
        try {
          const results = await options.send(chunk.map((w) => w.query));
          chunk.forEach((w, i) => {
            const result = results[i];
            if (result === undefined) w.reject(new Error("lookup batch answered short"));
            else w.resolve(result);
          });
        } catch (err) {
          chunk.forEach((w) => w.reject(err));
        }
      }
    } finally {
      inFlight = false;
      schedule();
    }
  };

  return (query: Q) => {
    const key = `${options.generation?.() ?? 0}\n${options.keyOf(query)}`;
    const existing = pending.get(key);
    if (existing) return existing;
    const promise = new Promise<R>((resolve, reject) => {
      queue.push({ query, resolve, reject });
    });
    pending.set(key, promise);
    const forget = () => {
      if (pending.get(key) === promise) pending.delete(key);
    };
    promise.then(forget, forget);
    schedule();
    return promise;
  };
}
