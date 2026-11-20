# Playback reference

Bookclerk can play the audiobooks it keeps. This page describes the
playback core the daemon serves: what it stores, the routes a player
calls and the rules behind them. The decisions are in the
[Playback ADR](../adr/playback.md); every route is in the
[Web API](api/README.md) reference with example requests and responses.

> [!NOTE]
> The web app's player uses this core (see
> [Listen in the web app](../guide/listening.md)). The desktop player and
> the app-compatible `/player` API are being built on it and are not in
> this release.

## What is stored

| Table               | One row per          | Holds                                                                                    |
| ------------------- | -------------------- | ---------------------------------------------------------------------------------------- |
| `playback_progress` | Person and edition   | Position, the runtime the player reported, finished flag and times                       |
| `playback_sessions` | Stretch of listening | The edition, the player's kind and name, where it started and stopped, and time listened |
| `bookmarks`         | Marked place         | Position and a note                                                                      |

All three belong to one library user and are removed with the user.
Finishing an edition also marks its work finished for that person.
Progress imported from other servers stays in `listening_progress`.

## Routes

Every route needs a signed-in library user. The operator token has no
user and gets `403`. An edition outside the caller's
[patron scope](../guide/sharing.md#limit-what-a-patron-sees) is `404`.
The **Download**
[control](../guide/sharing.md#limit-what-a-patron-may-do) does not apply
to playing: it stops taking copies to keep, and a patron who can see a
book may listen to it.

| Route                                               | Purpose                                                                                                      |
| --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `GET /api/library/editions/{id}/playback`           | Whether this server can play the edition for the caller, its tracks, and the caller's progress and bookmarks |
| `POST /api/playback/sessions`                       | Open a play session: tracks with stream paths, and where to start                                            |
| `POST /api/playback/sessions/{id}/sync`             | Report the position while playing                                                                            |
| `POST /api/playback/sessions/{id}/close`            | Send the last report and end the session                                                                     |
| `GET /api/playback/sessions/{id}/tracks/{index}`    | One track's bytes, whole or by `Range`                                                                       |
| `GET /api/playback/progress`                        | The caller's progress, most recent first                                                                     |
| `PUT`, `DELETE /api/playback/progress/{edition_id}` | Set or forget the caller's progress                                                                          |
| `GET`, `POST /api/library/editions/{id}/bookmarks`  | List or add the caller's bookmarks in an edition                                                             |
| `PUT`, `DELETE /api/playback/bookmarks/{id}`        | Change or remove one                                                                                         |

## How a player uses them

1. Read the manifest. `playable` is true when this server has a
   destination holding the edition's audio and the caller may hear it.
2. Open a session with the edition's id. The answer lists the tracks in
   play order, each with a `path`, and `position_ms`: the caller's saved
   position, or 0 when they have none or had finished the edition.
3. Play each track's `path`. The route answers `Range` requests, so a
   browser's `<audio>` element can seek. The session cookie signs the
   request; no token goes in the URL.
4. Send `sync` every few seconds with `position_ms`, and `listened_ms`
   since the last report.
5. Send `close` when playback stops.

Positions are milliseconds on the edition's whole timeline, across all
of its tracks.

## Rules worth knowing

- **Tracks** are the edition's primary audio file and its parts.
  Per-chapter files, covers and documents are not tracks.
- **Finished** means the position reached the last 1 percent of the
  runtime, or its last 30 seconds for a recording over a minute.
- **A session stops streaming** when it is closed, when it has not
  reported for 24 hours, when the caller may no longer see the edition,
  or when the edition's files change. Stream requests to a session that
  stopped get `409` with `session_ended`, as does a report to a closed
  session; the player opens a new session. Turning a member's Download
  control off does not end a session: it does not cover listening.
- **A session streams from the server that opened it.** Another server
  on the same library answers `421`.
- **Limits:** a position or runtime is at most sixty days, a session
  lists at most 512 tracks, and a person keeps at most 200 bookmarks in
  one edition. A bookmark's note is at most 200 bytes with no control
  characters, and a player's name is kept to 64 bytes.
- **Request bodies** are JSON objects with only the documented fields.
  One out of range gets `400`; one of another shape gets `422`. A
  report (`sync` or `close`) is at most 1024 bytes and every other body
  at most 4096 bytes; a longer one gets `413`.
- **Time listened** in a session is the sum of what its reports carry,
  so reports that overlap all count.
- **Cross-site requests** are refused like every other cookie-signed
  write: `POST`, `PUT` and `DELETE` need the app's own `Origin`.

## Events and live updates

Plugins can consume three host events (see
[Plugin ABI](../plugins/abi.md)): `playback_started`,
`playback_progress` and `playback_finished`. Periodic sync reports
publish nothing.

The web app's live socket has a `playback` topic. It moves when a session
opens or closes, an edition is finished, progress is set outright or a
bookmark changes.
