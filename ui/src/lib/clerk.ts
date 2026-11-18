/**
 * Library cards: the SPA side of signing this install in to a library as
 * a **clerk** (a client app that shows the person's books there, not a
 * branch).
 *
 * - Settings → Library → Library cards lists the cards this install holds
 *   and signs one out. "Sign in with a library card" reads the library
 *   (`/api/config/clerk/inspect`), asks for the fingerprint of a
 *   self-signed library and a name for this computer, then starts the
 *   sign-in (`/api/config/clerk/start`). The daemon keeps the PKCE
 *   verifier; the browser only goes to the library's sign-in page.
 * - The library sends the browser back to `/clerk/callback`, which relays
 *   the `code` and `state` to `/api/config/clerk/callback`. The daemon
 *   redeems them, seals the card and starts reading the person's books,
 *   which then show in Library beside the books of every library this
 *   install is a window of.
 *
 * The card itself never reaches the browser.
 */

import { ApiError } from "@/lib/api";
import { request, type LibraryInfoView } from "@/lib/join";

/** A library this install holds (or held) a card for, without the card. */
export interface ClerkCardView {
  /** The library's library id: what the card is keyed by. */
  library_id: string;
  /** The library's canonical origin (`https://library.example`). */
  origin: string;
  /** The library's name at sign-in. */
  library_name: string;
  /** True when the card's connection pins the library's self-signed certificate. */
  pinned: boolean;
  /** The person's display name on the library, when it has one. */
  display_name: string | null;
  /** The name this install signed in as. */
  clerk_name: string;
  /** `active`, or `signed_out` once the library ended the card. */
  state: string;
  /** When this install signed in (RFC 3339, UTC). */
  signed_in_at: string;
}

/** `GET /api/config/clerk`. */
export interface ClerkCards {
  /** The libraries this install holds (or held) a card for, oldest sign-in first. */
  cards: ClerkCardView[];
  /** The name this install signs in as unless another is given. */
  default_clerk_name: string;
}

/** `POST /api/config/clerk/start`. */
export interface ClerkStarted {
  /** The library's sign-in page to send the browser to. */
  authorize_url: string;
  /** What the library said about itself. */
  library: LibraryInfoView;
  /** The name this install signs in as. */
  clerk_name: string;
  /** Seconds the pending sign-in stays valid. */
  expires_in_secs: number;
}

/** `POST /api/config/clerk/sign-out`. */
export interface ClerkSignedOut {
  /** The library's library id. */
  library_id: string;
  /**
   * Whether the library ended the card's session too. False when it could
   * not be reached: the session then stays in the person's session list
   * there until it idles out or they end it.
   */
  library_signed_out: boolean;
}

/** What the library sent back to `/clerk/callback`. */
export interface ClerkCallback {
  /** The one-use authorization code; empty when the sign-in was refused. */
  code: string;
  /** The opaque sign-in id this daemon issued, echoed back by the library. */
  state: string;
  /** The library's OAuth error (`access_denied` when its user said no), if any. */
  error: string | null;
}

/**
 * The cards this install holds, and the name it signs in as by default.
 * The operator or an Owner only.
 *
 * @returns The cards, oldest sign-in first.
 */
export function fetchClerkCards(): Promise<ClerkCards> {
  return request("/api/config/clerk");
}

/**
 * Reads a library before signing in to it with a card. This daemon reads
 * it, not the browser.
 *
 * @param address - The library's address as typed.
 * @returns What the library says about itself.
 */
export function inspectCardLibrary(address: string): Promise<{ library: LibraryInfoView }> {
  return request("/api/config/clerk/inspect", { method: "POST", body: { address } });
}

/**
 * Starts a card sign-in; the caller sends the browser to `authorize_url`.
 *
 * @param body - The library's origin, the fingerprint the person confirmed
 *   (self-signed libraries), and the name to sign in as.
 * @returns The library's sign-in page and how long the sign-in stays valid.
 */
export function startClerkSignIn(body: {
  address: string;
  fingerprint: string | null;
  clerk_name: string;
}): Promise<ClerkStarted> {
  return request("/api/config/clerk/start", {
    method: "POST",
    body: {
      address: body.address,
      fingerprint: body.fingerprint ?? undefined,
      clerk_name: body.clerk_name.trim() || undefined,
    },
  });
}

/**
 * Finishes a card sign-in: the daemon redeems the code with the verifier
 * only it holds and seals the card.
 *
 * @param callback - The `code`, `state` and `error` from `/clerk/callback`.
 * @returns The card this install now holds.
 */
export function finishClerkSignIn(callback: ClerkCallback): Promise<{ card: ClerkCardView }> {
  return request("/api/config/clerk/callback", {
    method: "POST",
    body: {
      code: callback.code,
      state: callback.state,
      error: callback.error ?? undefined,
    },
  });
}

/**
 * Signs this install out of a library: forgets the card here and ends its
 * session at the library when it can be reached.
 *
 * @param libraryId - The library's library id.
 * @returns Whether the library ended the session too.
 */
export function signOutClerkCard(libraryId: string): Promise<ClerkSignedOut> {
  return request("/api/config/clerk/sign-out", {
    method: "POST",
    body: { library_id: libraryId },
  });
}

/**
 * Reads the `/clerk/callback` query.
 *
 * @param search - `window.location.search` on `/clerk/callback`.
 * @returns The answer, or `null` when it has no `state`.
 */
export function parseClerkCallback(search: string): ClerkCallback | null {
  const params = new URLSearchParams(search);
  const state = params.get("state")?.trim() ?? "";
  if (!state) return null;
  return {
    code: params.get("code")?.trim() ?? "",
    state,
    error: params.get("error")?.trim() || null,
  };
}

/**
 * Whether a failed callback may be sent again with the same code: only
 * when the request got no answer, so the daemon may still hold the
 * sign-in. Every answer from the daemon ends the pending sign-in.
 *
 * @param error - What the callback request threw.
 * @returns True when Try again can help.
 */
export function clerkCallbackRetryable(error: unknown): boolean {
  return !(error instanceof ApiError);
}

/**
 * The words for a refused card request.
 *
 * @param error - What the request threw.
 * @param fallback - Copy when nothing more specific applies.
 * @returns Copy for an inline error.
 */
export function clerkErrorMessage(error: unknown, fallback: string): string {
  if (error instanceof ApiError) {
    switch (error.code) {
      case "already_joined":
        return "This server is already a branch of that library, so its books show through that link.";
      case "join_expired":
        return "This sign-in took too long or was already used. Start again.";
      case "denied":
        return "The library's patron did not let this computer show their books.";
      case "refused":
        return "The library refused the sign-in: it was already used or took too long. Start again.";
      case "transport_insecure":
        return "That library can only be reached over HTTPS (plain HTTP works only on this machine).";
      case "fingerprint_unconfirmed":
        return "Confirm the library's fingerprint first.";
      case "not_found":
        return "This server holds no card for that library.";
      default:
        return error.message || fallback;
    }
  }
  return error instanceof Error && error.message ? error.message : fallback;
}

/**
 * One line for a card in the list: who it signs in as, and as what.
 *
 * @param card - A card.
 * @returns For example `Riley, as "Study laptop"`.
 */
export function clerkCardSummary(card: ClerkCardView): string {
  const who = card.display_name?.trim();
  const as = `as "${card.clerk_name}"`;
  const line = who ? `${who}, ${as}` : `Signed in ${as}`;
  return card.state === "signed_out" ? `${line}. The library ended this card.` : line;
}
