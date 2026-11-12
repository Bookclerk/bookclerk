import { useEffect, useState } from "react";
import { fetchCatalogListing, isApiError, setCatalogListing } from "@/lib/api";

/**
 * Account → "List my books to joined libraries": whether a library this
 * server is a branch of hears that the branch can supply the signed-in
 * user's books. Hidden while this server has joined no library, and for an
 * account that has no first-party user.
 */
export function CatalogListingSetting() {
  const [listed, setListed] = useState<boolean | null>(null);
  const [libraries, setLibraries] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetchCatalogListing()
      .then((view) => {
        if (cancelled) return;
        setListed(view.listed);
        setLibraries(view.libraries);
      })
      .catch(() => {
        // No first-party user (operator token) or not signed in: nothing to show.
        if (!cancelled) setListed(null);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  if (listed === null || libraries === 0) return null;

  async function save(next: boolean) {
    setBusy(true);
    setError(null);
    try {
      const view = await setCatalogListing(next);
      setListed(view.listed);
      setLibraries(view.libraries);
    } catch (err) {
      setError(isApiError(err) ? err.message : "Could not save.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className="space-y-3">
      <h2 className="text-lg font-semibold text-ink">Joined libraries</h2>
      <label className="flex items-start gap-3 text-sm text-ink">
        <input
          type="checkbox"
          className="mt-0.5"
          checked={listed}
          disabled={busy}
          onChange={(e) => void save(e.target.checked)}
        />
        <span>
          <span className="block font-medium">List my books to joined libraries</span>
          <span className="block text-xs text-ink/55">
            This server is a branch of {libraries === 1 ? "a library" : `${libraries} libraries`}.
            When one asks about a title, it hears that this branch can supply it if you have it.
            Nothing is sent until it asks, and only which book and store, never your account.
            Turning this off can take up to two days to reach the library.
          </span>
        </span>
      </label>
      {error ? (
        <p className="text-xs font-medium text-brick" role="alert">
          {error}
        </p>
      ) : null}
    </section>
  );
}
