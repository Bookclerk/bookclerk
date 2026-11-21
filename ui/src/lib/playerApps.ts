/**
 * Player apps: player passwords in Account settings, and the consent page
 * an app's browser sign-in opens (`/apps/approve`).
 *
 * The routes are `/api/auth/player-passwords` and
 * `/api/auth/player-consent`; the apps themselves talk to `/player`.
 */
import { request } from "@/lib/join";

/** A player password as Account settings lists it (never the secret). */
export interface PlayerPasswordView {
  /** Its id. */
  id: string;
  /** The label its owner gave it (the device or app). */
  label: string;
  /** When it was made (RFC 3339). */
  created_at: string;
  /** When an app last signed in with it (RFC 3339), if ever. */
  last_used_at?: string | null;
}

/** A newly made player password: shown once. */
export interface NewPlayerPassword {
  /** The stored password. */
  password: PlayerPasswordView;
  /** The password itself, in groups of five; the library keeps only its digest. */
  secret: string;
}

/**
 * Lists the signed-in user's player passwords.
 *
 * @returns The passwords, newest first.
 */
export async function listPlayerPasswords(): Promise<PlayerPasswordView[]> {
  const body = await request<{ passwords: PlayerPasswordView[] }>("/api/auth/player-passwords");
  return body.passwords;
}

/**
 * Makes a player password.
 *
 * @param label - What it is for, such as the phone's name.
 * @returns The password, with its secret shown this once.
 */
export function createPlayerPassword(label: string): Promise<NewPlayerPassword> {
  return request("/api/auth/player-passwords", { method: "POST", body: { label } });
}

/**
 * Removes a player password and signs out every app that signed in with it.
 *
 * @param id - The password's id.
 */
export async function deletePlayerPassword(id: string): Promise<void> {
  const res = await fetch(`/api/auth/player-passwords/${encodeURIComponent(id)}`, {
    method: "DELETE",
    credentials: "include",
    cache: "no-store",
  });
  if (!res.ok && res.status !== 404) {
    throw new Error(`Could not remove the player password (${res.status}).`);
  }
}

/** What the player consent page shows. */
export interface PlayerConsentView {
  /** The app asking, such as "Absorb". */
  app_name: string;
  /** The signed-in user's display name. */
  display_name?: string | null;
}

/**
 * The consent request's own parameters, from the page's query.
 *
 * @param search - The page's query string.
 * @returns `redirect_uri`, `code_challenge` and `state` when present.
 */
function consentParams(search: string): Record<string, string> {
  const params = new URLSearchParams(search);
  const out: Record<string, string> = {};
  for (const name of ["redirect_uri", "code_challenge", "state"]) {
    const value = params.get(name);
    if (value !== null) out[name] = value;
  }
  return out;
}

/**
 * Reads what the consent page shows.
 *
 * @param search - The page's query string.
 * @returns The app and the signed-in user.
 */
export function fetchPlayerConsent(search: string): Promise<PlayerConsentView> {
  const query = new URLSearchParams(consentParams(search)).toString();
  return request(`/api/auth/player-consent?${query}`);
}

/**
 * Approves or declines the app's sign-in; answers where to send the browser.
 *
 * @param search - The page's query string.
 * @param approve - True to let the app sign in.
 * @returns The library's own way back to the app.
 */
export function answerPlayerConsent(
  search: string,
  approve: boolean,
): Promise<{ redirect: string }> {
  return request("/api/auth/player-consent", {
    method: "POST",
    body: { ...consentParams(search), approve },
  });
}

/** The only place the consent page may send the browser. */
export const PLAYER_RETURN_PATH = "/player/auth/openid/mobile-redirect";

/**
 * True when `redirect` is the library's own way back to an app on this
 * origin, so the page never sends the browser anywhere else.
 *
 * @param redirect - Where the daemon said to go.
 * @param origin - This page's origin.
 * @returns Whether the page may go there.
 */
export function isPlayerReturn(redirect: string, origin: string = window.location.origin): boolean {
  try {
    const to = new URL(redirect, origin);
    return to.origin === origin && to.pathname === PLAYER_RETURN_PATH;
  } catch {
    return false;
  }
}
