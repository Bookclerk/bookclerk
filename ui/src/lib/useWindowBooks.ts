import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError } from "@/lib/api";
import { fetchWindowBooks, type WindowBooks } from "@/lib/join";
import { useLiveRefresh } from "@/lib/live";

/** The books of other libraries the Library list merges in. */
export interface WindowBooksState {
  /** The last view read; `null` before the first read or when not allowed. */
  view: WindowBooks | null;
  /** True when the last refresh failed and `view` is the one before it. */
  stale: boolean;
  /** Reads the view again. */
  reload: () => Promise<void>;
}

/**
 * Reads the books of every library this server is a branch of or holds a
 * library card for (`GET /api/config/join/window`), again whenever the
 * `libraries` live topic changes, and every minute while the live socket
 * is down.
 *
 * A caller who may not read the view (only the operator or an Owner may)
 * gets `view: null`. A refresh that fails for another reason keeps the
 * view it has and sets `stale`.
 *
 * @param enabled - False to read nothing (the caller may not read it).
 * @returns The view, whether it is stale, and a reload.
 */
export function useWindowBooks(enabled: boolean): WindowBooksState {
  const [view, setView] = useState<WindowBooks | null>(null);
  const [stale, setStale] = useState(false);
  // Only the latest refresh may change the view, so an older answer that
  // arrives late never replaces a newer one.
  const latest = useRef(0);

  const reload = useCallback(async () => {
    if (!enabled) return;
    const request = ++latest.current;
    try {
      const next = await fetchWindowBooks();
      if (request !== latest.current) return;
      setView(next);
      setStale(false);
    } catch (err) {
      if (request !== latest.current) return;
      if (err instanceof ApiError && (err.status === 401 || err.status === 403)) {
        setView(null);
        setStale(false);
      } else {
        setStale(true);
      }
    }
  }, [enabled]);

  useEffect(() => {
    void reload();
  }, [reload]);
  useLiveRefresh(["libraries"], reload, { pollMs: 60_000, openPollMs: 300_000, enabled });

  return { view: enabled ? view : null, stale: enabled && stale, reload };
}
