/**
 * The playback routes of the daemon (`/api/playback` and
 * `/api/library/editions/{id}`), as the web player calls them.
 *
 * Positions are milliseconds on an edition's whole timeline, across all
 * of its tracks.
 *
 * @module
 */
import { ApiError } from "@/lib/api";

/** One audio file of an edition. */
export interface PlaybackTrack {
  /** Its place in play order, from 0. */
  index: number;
  /** Same-origin path to stream it from; only in a session's answer. */
  path?: string;
  /** Media type the stream states, such as `audio/mp4`. */
  content_type: string;
  /** Average bitrate in kilobits per second, once probed. */
  bitrate_kbps: number | null;
  /** True for a lossless encoding. */
  lossless: boolean;
}

/** The caller's progress in one edition. */
export interface PlaybackProgress {
  /** The edition. */
  edition_id: string;
  /** Its work, when known. */
  work_id: string | null;
  /** Position on the edition's timeline. */
  position_ms: number;
  /** Runtime the last player reported; `null` until one has. */
  duration_ms: number | null;
  /** True once the caller reached the end. */
  is_finished: boolean;
  /** When the caller first played it (RFC 3339). */
  started_at: string;
  /** When the caller last reached the end (RFC 3339). */
  finished_at: string | null;
  /** When the progress last changed (RFC 3339). */
  updated_at: string;
}

/** A place the caller marked in an edition. */
export interface Bookmark {
  /** Bookmark id. */
  id: string;
  /** The edition it is in. */
  edition_id: string;
  /** The marked place. */
  position_ms: number;
  /** The caller's note; may be empty. */
  title: string;
  /** When it was made (RFC 3339). */
  created_at: string;
}

/** `GET /api/library/editions/{id}/playback`. */
export interface PlaybackManifest {
  /** The edition. */
  edition_id: string;
  /** Its work. */
  work_id: string | null;
  /** The edition's display title. */
  title: string;
  /** True when this server holds a copy the caller may play. */
  playable: boolean;
  /** Runtime, when known. */
  duration_ms: number | null;
  /** The tracks in play order; empty when not playable. */
  tracks: PlaybackTrack[];
  /** The caller's progress; `null` before they first play it. */
  progress: PlaybackProgress | null;
  /** The caller's bookmarks, in timeline order. */
  bookmarks: Bookmark[];
}

/** `POST /api/playback/sessions` answer. */
export interface PlaybackSession {
  /** Session id for the sync, close and stream routes. */
  session_id: string;
  /** The edition. */
  edition_id: string;
  /** Its work. */
  work_id: string | null;
  /** Where to start. */
  position_ms: number;
  /** Runtime, when known. */
  duration_ms: number | null;
  /** The tracks, each with its stream `path`. */
  tracks: PlaybackTrack[];
  /** Seconds a silent session keeps streaming. */
  idle_secs: number;
}

/** A position report. */
export interface PlaybackReport {
  /** Position on the edition's timeline. */
  position_ms: number;
  /** Milliseconds listened since the last report. */
  listened_ms?: number;
  /** The edition's whole runtime, when the player knows it. */
  duration_ms?: number;
}

/** The answer to a report or an outright progress write. */
export interface PlaybackSynced {
  /** The progress after the write. */
  progress: PlaybackProgress;
  /** True when this write finished the edition. */
  finished_now: boolean;
}

/** A page of `GET /api/playback/progress`. */
export interface ProgressPage {
  /** The rows, most recent first. */
  items: PlaybackProgress[];
  /** Cursor of the next page; absent on the last. */
  next_cursor?: string;
}

/**
 * Reads a JSON answer, or throws what the daemon said.
 *
 * @param res - A response from a playback route.
 * @returns The decoded body; `undefined` for 204.
 * @throws ApiError with the status and the `error` slug as `code`.
 */
async function answer<T>(res: Response): Promise<T> {
  if (res.ok) {
    return (res.status === 204 ? undefined : await res.json()) as T;
  }
  const text = await res.text().catch(() => "");
  let code: string | undefined;
  try {
    const body = JSON.parse(text) as { error?: unknown };
    if (typeof body.error === "string") code = body.error;
  } catch {
    // Not JSON: the status speaks for itself.
  }
  throw new ApiError(res.status, code ?? `${res.status} ${res.statusText}`, { code });
}

/**
 * Sends a JSON body to a playback route.
 *
 * @param method - HTTP method.
 * @param url - Route path.
 * @param body - JSON body; omitted for none.
 * @param keepalive - True to let the request outlive the page.
 * @returns The response.
 */
function send(method: string, url: string, body?: unknown, keepalive = false): Promise<Response> {
  return fetch(url, {
    method,
    credentials: "include",
    keepalive,
    headers: body === undefined ? undefined : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

/** Path part for an id. */
const part = encodeURIComponent;

/**
 * Reads whether and what the caller may play of an edition.
 *
 * @param editionId - The edition.
 * @returns The manifest.
 */
export async function fetchPlaybackManifest(editionId: string): Promise<PlaybackManifest> {
  return answer(await send("GET", `/api/library/editions/${part(editionId)}/playback`));
}

/**
 * Opens a play session.
 *
 * @param editionId - The edition to play.
 * @param playerName - A short name for this player, shown in history.
 * @returns The session, its tracks and where to start.
 */
export async function openPlaybackSession(
  editionId: string,
  playerName: string,
): Promise<PlaybackSession> {
  return answer(
    await send("POST", "/api/playback/sessions", {
      edition_id: editionId,
      player: "web",
      player_name: playerName,
    }),
  );
}

/**
 * Reports the position of a playing session.
 *
 * @param sessionId - The session.
 * @param report - Where the listener is.
 * @returns The progress after the report.
 */
export async function syncPlaybackSession(
  sessionId: string,
  report: PlaybackReport,
): Promise<PlaybackSynced> {
  return answer(await send("POST", `/api/playback/sessions/${part(sessionId)}/sync`, report));
}

/**
 * Sends a session's last report and ends it.
 *
 * @param sessionId - The session.
 * @param report - Where the listener stopped.
 * @param keepalive - True while the page is going away.
 * @returns The progress after the report.
 */
export async function closePlaybackSession(
  sessionId: string,
  report: PlaybackReport,
  keepalive = false,
): Promise<PlaybackSynced> {
  return answer(
    await send("POST", `/api/playback/sessions/${part(sessionId)}/close`, report, keepalive),
  );
}

/**
 * Reads the caller's progress, most recent first.
 *
 * @param options - Page size, cursor and whether finished editions count.
 * @returns One page.
 */
export async function fetchPlaybackProgress(
  options: { limit?: number; cursor?: string; includeFinished?: boolean } = {},
): Promise<ProgressPage> {
  const query = new URLSearchParams();
  if (options.limit) query.set("limit", String(options.limit));
  if (options.cursor) query.set("cursor", options.cursor);
  if (options.includeFinished) query.set("include_finished", "true");
  const suffix = query.size ? `?${query.toString()}` : "";
  return answer(await send("GET", `/api/playback/progress${suffix}`));
}

/**
 * Sets the caller's progress in an edition outright.
 *
 * @param editionId - The edition.
 * @param edit - The fields to change.
 * @returns The progress after the write.
 */
export async function putPlaybackProgress(
  editionId: string,
  edit: { position_ms?: number; duration_ms?: number; is_finished?: boolean },
): Promise<PlaybackSynced> {
  return answer(await send("PUT", `/api/playback/progress/${part(editionId)}`, edit));
}

/**
 * Forgets the caller's progress in an edition.
 *
 * @param editionId - The edition.
 */
export async function deletePlaybackProgress(editionId: string): Promise<void> {
  await answer<void>(await send("DELETE", `/api/playback/progress/${part(editionId)}`));
}

/**
 * Marks a place in an edition.
 *
 * @param editionId - The edition.
 * @param positionMs - The place.
 * @param title - A note; may be empty.
 * @returns The new bookmark.
 */
export async function addBookmark(
  editionId: string,
  positionMs: number,
  title: string,
): Promise<Bookmark> {
  return answer(
    await send("POST", `/api/library/editions/${part(editionId)}/bookmarks`, {
      position_ms: Math.round(positionMs),
      title,
    }),
  );
}

/**
 * Removes one of the caller's bookmarks.
 *
 * @param id - The bookmark.
 */
export async function deleteBookmark(id: string): Promise<void> {
  await answer<void>(await send("DELETE", `/api/playback/bookmarks/${part(id)}`));
}
