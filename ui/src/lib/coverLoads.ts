/**
 * Which covers on screen are still loading, so other work can let them go
 * first.
 *
 * Discover's price and details lookups make the daemon ask every store,
 * which on a small machine competes with serving covers. Its lookup batches
 * wait in {@link coversSettled} until the covers the viewer can see have
 * loaded (or a short cap passes), so covers never queue behind prices.
 *
 * A cover counts only while it is unloaded **and** intersecting the
 * viewport: a lazy image off screen, or clipped by a shelf's horizontal
 * scroller, is not waited for.
 *
 * @module
 */

/** Cover images registered and not yet loaded. */
const loading = new Set<Element>();
/** Of those, the ones on screen now. */
const onScreen = new Set<Element>();
/**
 * Of those, the ones the observer has not reported on yet: observations
 * arrive asynchronously, so a cover just registered is waited for until
 * its first report says whether it is on screen.
 */
const unseen = new Set<Element>();
/** Callers of {@link coversSettled} waiting for `onScreen` to empty. */
let waiters: Array<() => void> = [];
/** One observer for every tracked cover. */
let observer: IntersectionObserver | null = null;

/** Wakes every waiter once no on-screen cover is loading. */
function notify() {
  if (onScreen.size > 0 || unseen.size > 0) return;
  const woken = waiters;
  waiters = [];
  for (const wake of woken) wake();
}

/**
 * Starts tracking a cover image that is loading.
 *
 * @param el - The `<img>` (or its frame) whose load is pending.
 * @returns A function to call once it loads, fails or unmounts; calling it
 *   again does nothing.
 */
export function trackCoverLoad(el: Element): () => void {
  if (typeof IntersectionObserver === "undefined") return () => {};
  observer ??= new IntersectionObserver((entries) => {
    for (const entry of entries) {
      if (!loading.has(entry.target)) continue;
      unseen.delete(entry.target);
      if (entry.isIntersecting) onScreen.add(entry.target);
      else onScreen.delete(entry.target);
    }
    notify();
  });
  loading.add(el);
  unseen.add(el);
  observer.observe(el);
  return () => {
    if (!loading.delete(el)) return;
    onScreen.delete(el);
    unseen.delete(el);
    observer?.unobserve(el);
    notify();
  };
}

/**
 * Waits until no cover on screen is still loading.
 *
 * @param maxMs - Longest wait in milliseconds; past it the promise resolves
 *   anyway, so a cover that never loads holds nothing up for long.
 * @returns A promise that resolves when the on-screen covers have settled.
 */
export function coversSettled(maxMs: number): Promise<void> {
  if (onScreen.size === 0 && unseen.size === 0) return Promise.resolve();
  return new Promise((resolve) => {
    const done = () => {
      clearTimeout(timer);
      waiters = waiters.filter((w) => w !== done);
      resolve();
    };
    const timer = setTimeout(done, maxMs);
    waiters.push(done);
  });
}

/**
 * How many on-screen covers are loading (for tests).
 *
 * @returns The count.
 */
export function coversLoadingOnScreen(): number {
  return onScreen.size;
}
