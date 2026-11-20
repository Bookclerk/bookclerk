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

## Chapters

Chapters belong to the edition and are kept in the database, so every
player and every stored copy shows the same list. Each source that knows
an edition's chapters adds a claim, and the library chooses the list it
plays (decisions in the [Edition chapters ADR](../adr/edition-chapters.md)):

1. A claim an Administrator pinned.
2. An Administrator's own edit. An empty edit clears the chapters on
   purpose.
3. Otherwise the best machine source, best first: Bookclerk's chapters
   aligned to the audio during acquire; a metadata provider's chapters
   with exact starts; the store's chapters; a file's chapters; a provider's
   estimated chapters; store markers of unknown kind; store track markers;
   one chapter per delivered part.

A storefront says whether its markers are chapters or tracks
(`chapterKind` in the [Plugin ABI](../plugins/abi.md)), so a store's
track list never outranks a book's real chapters.

| Route | Who | Purpose |
| --- | --- | --- |
| `GET /api/library/editions/{id}/chapters` | Anyone who may see the edition | The chapters it plays, in pre-order with depths, and where they came from |
| `GET /api/library/editions/{id}/chapters/claims` | Administrator | Every source's list, and which one plays |
| `PUT /api/library/editions/{id}/chapters` | Administrator | Set the chapters by hand; `{"chapters": []}` clears them |
| `DELETE /api/library/editions/{id}/chapters` | Administrator | Remove the edit, so the best remaining source plays again |
| `PUT`, `DELETE /api/library/editions/{id}/chapters/pin` | Administrator | Pin one source's list, or remove the pin |

"Administrator" here means the operator, an Owner or an Administrator, as
for every other metadata edit. An edit is checked strictly and stored
exactly as sent: at most 2,000 chapters, each title 1 to 512 bytes with no
control character and no leading or trailing space, starts that never go
back and are at most 60 days in, and each depth (0 to 7) at most one
deeper than the chapter above. A body that breaks a rule gets `400`, one
over 2 MiB gets `413`.

When the list an edition plays changes, the host emits `edition_updated`
with `fields: ["chapters"]`. Media files keep the chapters they were
written with.

## Events and live updates

Plugins can consume three host events (see
[Plugin ABI](../plugins/abi.md)): `playback_started`,
`playback_progress` and `playback_finished`. Periodic sync reports
publish nothing.

The web app's live socket has a `playback` topic. It moves when a session
opens or closes, an edition is finished, progress is set outright or a
bookmark changes.
