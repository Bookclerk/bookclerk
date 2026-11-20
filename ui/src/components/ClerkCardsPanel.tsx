import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { isApiError } from "@/lib/api";
import {
  clerkCardSummary,
  clerkErrorMessage,
  fetchClerkCards,
  inspectCardLibrary,
  signOutClerkCard,
  startClerkSignIn,
  type ClerkCardView,
} from "@/lib/clerk";
import {
  fingerprintGroups,
  JoinApiError,
  joinErrorMessage,
  startJoin,
  type LibraryInfoView,
} from "@/lib/join";
import { useLiveRefresh } from "@/lib/live";

/** Props for {@link ClerkCardsPanel}. */
export interface ClerkCardsPanelProps {
  /** Called when the session expired (a 401). */
  onSessionExpired: () => void;
  /**
   * Offer "Make this server a branch" on each card: joining a library is
   * the operator's.
   */
  canJoin?: boolean;
}

/**
 * Settings → Library → Library cards: the libraries this server is signed
 * in to as a clerk, each with Sign out (and, for the operator, "Make this
 * server a branch"), and "Sign in with a library card".
 *
 * A card shows the person's books in that library beside the books here,
 * without making this server a branch of it: nothing is downloaded and the
 * library runs no work here.
 *
 * @param props - See {@link ClerkCardsPanelProps}.
 * @returns The panel.
 */
export function ClerkCardsPanel({ onSessionExpired, canJoin = false }: ClerkCardsPanelProps) {
  const [cards, setCards] = useState<ClerkCardView[] | null>(null);
  const [defaultName, setDefaultName] = useState("");
  const [loadError, setLoadError] = useState<string | null>(null);
  const [signingIn, setSigningIn] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const expired = useRef(onSessionExpired);
  useEffect(() => {
    expired.current = onSessionExpired;
  }, [onSessionExpired]);

  const refresh = useCallback(async () => {
    try {
      const body = await fetchClerkCards();
      setCards(body.cards);
      setDefaultName(body.default_clerk_name);
      setLoadError(null);
    } catch (err) {
      if (isApiError(err) && err.status === 401) {
        expired.current();
        return;
      }
      setLoadError(clerkErrorMessage(err, "Could not load the library cards."));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);
  // A sign-in, a sign-out, or the library ending a card.
  useLiveRefresh(["libraries"], refresh, { pollMs: 60_000, openPollMs: 300_000 });

  return (
    <section className="space-y-4" aria-labelledby="library-cards-heading">
      <div className="space-y-1">
        <h3 id="library-cards-heading" className="text-base font-semibold text-ink">
          Library cards
        </h3>
        <p className="text-sm text-ink/55">
          Sign in to another library with your account there to see your books in it beside the
          books here. This server downloads nothing from it and runs none of its work.
        </p>
      </div>
      {notice ? (
        <p className="text-sm font-medium text-ink" role="status">
          {notice}
        </p>
      ) : null}
      {signingIn ? (
        <div className="space-y-3 rounded-md border border-ink/10 bg-card p-4">
          <h4 className="text-sm font-semibold text-ink">Sign in with a library card</h4>
          <ClerkSignInForm defaultName={defaultName} onCancel={() => setSigningIn(false)} />
        </div>
      ) : (
        <Button
          type="button"
          variant="secondary"
          onClick={() => {
            setNotice(null);
            setSigningIn(true);
          }}
        >
          Sign in with a library card
        </Button>
      )}
      {loadError ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {loadError}
        </p>
      ) : null}
      {cards == null ? (
        loadError ? null : (
          <p className="text-sm text-ink/50">Loading…</p>
        )
      ) : cards.length === 0 ? (
        <p className="text-sm text-ink/50">This server holds no library cards.</p>
      ) : (
        <ul className="divide-y divide-ink/10 bg-card">
          {cards.map((card) => (
            <CardRow
              key={card.library_id}
              card={card}
              canJoin={canJoin}
              onSignedOut={(message) => {
                setNotice(message);
                void refresh();
              }}
            />
          ))}
        </ul>
      )}
    </section>
  );
}

/**
 * One card: the library, who it signs in as, Sign out, and (for the
 * operator) "Make this server a branch", which joins the library with the
 * card's address and pinned certificate; the branch replaces the card once
 * the library approves.
 *
 * @param props - The card, whether this caller may join, and what to do
 *   once it is signed out.
 * @returns The row.
 */
function CardRow({
  card,
  canJoin,
  onSignedOut,
}: {
  card: ClerkCardView;
  canJoin: boolean;
  onSignedOut: (message: string) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const library = card.library_name || card.origin;

  async function onSignOut() {
    setBusy(true);
    setError(null);
    try {
      const done = await signOutClerkCard(card.library_id);
      onSignedOut(
        done.library_signed_out || card.state === "signed_out"
          ? `Signed out of ${library}.`
          : `Signed out of ${library} here. The library could not be reached, so its session for this server stays in your session list there until it idles out or you end it.`,
      );
    } catch (err) {
      setError(clerkErrorMessage(err, "Could not sign out."));
      setBusy(false);
    }
  }

  async function onMakeBranch() {
    if (
      !window.confirm(
        `Make this server a branch of ${library}? It can then download your books from your store accounts and run the library's work. Once the library approves, your library card is signed out: its books come through the branch.`,
      )
    ) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const started = await startJoin({ from_card: card.library_id });
      // The library asks you to approve the branch and sends the browser
      // back to this server's /join/callback.
      window.location.assign(started.authorize_url);
    } catch (err) {
      setError(joinErrorMessage(err, "Could not start joining the library."));
      setBusy(false);
    }
  }

  return (
    <li className="flex flex-wrap items-center justify-between gap-3 px-4 py-3">
      <div className="min-w-0 space-y-0.5">
        <p className="text-sm font-medium text-ink">
          {library} <span className="font-normal text-ink/50">({card.origin})</span>
        </p>
        <p className="text-xs text-ink/55">{clerkCardSummary(card)}</p>
        {error ? (
          <p className="text-xs font-medium text-brick" role="alert">
            {error}
          </p>
        ) : null}
      </div>
      <div className="flex flex-wrap gap-2">
        {canJoin && card.state === "active" ? (
          <Button
            type="button"
            variant="secondary"
            disabled={busy}
            onClick={() => void onMakeBranch()}
          >
            Make this server a branch
          </Button>
        ) : null}
        <Button type="button" variant="secondary" disabled={busy} onClick={() => void onSignOut()}>
          {card.state === "signed_out" ? "Remove" : "Sign out"}
        </Button>
      </div>
    </li>
  );
}

/** Props for {@link ClerkSignInForm}. */
export interface ClerkSignInFormProps {
  /** The name to suggest for this computer. */
  defaultName: string;
  /** Shows a Back button that calls this. */
  onCancel?: () => void;
}

/**
 * Sign in with a library card: address → fingerprint (self-signed
 * libraries) and this computer's name → the library's sign-in page, which
 * sends the browser back to `/clerk/callback`.
 *
 * @param props - See {@link ClerkSignInFormProps}.
 * @returns The form.
 */
export function ClerkSignInForm({ defaultName, onCancel }: ClerkSignInFormProps) {
  const [address, setAddress] = useState("");
  const [name, setName] = useState(defaultName);
  const [library, setLibrary] = useState<LibraryInfoView | null>(null);
  const [confirmed, setConfirmed] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [suggested, setSuggested] = useState<string | null>(null);

  async function onInspect(event: FormEvent) {
    event.preventDefault();
    setError(null);
    setSuggested(null);
    setLibrary(null);
    setConfirmed(false);
    setBusy(true);
    try {
      const { library: info } = await inspectCardLibrary(address.trim());
      setLibrary(info);
    } catch (err) {
      // The library answers to another address: offer it.
      setSuggested(err instanceof JoinApiError && err.origin ? err.origin : null);
      setError(clerkErrorMessage(err, "Could not reach that library."));
    } finally {
      setBusy(false);
    }
  }

  async function onSignIn() {
    if (!library) return;
    setError(null);
    setBusy(true);
    try {
      const started = await startClerkSignIn({
        address: library.origin,
        fingerprint: library.needs_fingerprint ? library.fingerprint : null,
        clerk_name: name,
      });
      // The library signs the patron in and sends the browser back to
      // this server's /clerk/callback.
      window.location.assign(started.authorize_url);
    } catch (err) {
      setError(clerkErrorMessage(err, "Could not start the sign-in."));
      setBusy(false);
    }
  }

  const canSignIn =
    library != null && (!library.needs_fingerprint || confirmed) && name.trim().length > 0;

  return (
    <div className="space-y-4">
      <form className="space-y-2" onSubmit={(e) => void onInspect(e)}>
        <label className="block text-sm font-medium text-ink" htmlFor="card-address">
          Library address
        </label>
        <div className="flex gap-2">
          <Input
            id="card-address"
            placeholder="https://library.example"
            autoComplete="url"
            value={address}
            onChange={(e) => {
              setAddress(e.target.value);
              setLibrary(null);
            }}
            disabled={busy}
          />
          <Button type="submit" disabled={busy || !address.trim()}>
            Continue
          </Button>
        </div>
        <p className="text-xs text-ink/50">
          The address you use to open that library in a browser. This server reads it, not your
          browser.
        </p>
        {suggested ? (
          <Button type="button" variant="ghost" onClick={() => setAddress(suggested)}>
            Use {suggested}
          </Button>
        ) : null}
      </form>

      {library ? (
        <div className="space-y-4 rounded-md border border-ink/10 bg-paper p-4">
          <p className="text-sm text-ink">
            Found <strong>{library.library_name}</strong>{" "}
            <span className="text-ink/50">({library.origin})</span>
          </p>
          {library.needs_fingerprint && library.fingerprint ? (
            <div className="space-y-2">
              <p className="text-sm text-ink/70">
                This library uses its own certificate. Check that this fingerprint matches the one
                on the library's <strong>Settings → Branches</strong> page:
              </p>
              <p className="break-all rounded bg-card px-3 py-2 font-mono text-xs text-ink">
                {fingerprintGroups(library.fingerprint).join(" ")}
              </p>
              <label className="flex items-center gap-2 text-sm text-ink">
                <input
                  type="checkbox"
                  checked={confirmed}
                  onChange={(e) => setConfirmed(e.target.checked)}
                />
                It matches
              </label>
            </div>
          ) : null}
          <div className="space-y-1">
            <label className="block text-sm font-medium text-ink" htmlFor="card-clerk-name">
              Name this computer
            </label>
            <Input
              id="card-clerk-name"
              maxLength={64}
              value={name}
              onChange={(e) => setName(e.target.value)}
              disabled={busy}
            />
            <p className="text-xs text-ink/50">
              The library asks you to give a card to this name when you sign in.
            </p>
          </div>
          <Button type="button" disabled={busy || !canSignIn} onClick={() => void onSignIn()}>
            Sign in to {library.library_name}
          </Button>
        </div>
      ) : null}

      {error ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {error}
        </p>
      ) : null}

      {onCancel ? (
        <Button type="button" variant="ghost" onClick={onCancel} disabled={busy}>
          Back
        </Button>
      ) : null}
    </div>
  );
}
