# Playback reference

Bookclerk can play the audiobooks it keeps. This page describes the
playback core the daemon serves: what it stores, the routes a player
calls and the rules behind them. The decisions are in the
[Playback ADR](../adr/playback.md); every route is in the
[Web API](api/README.md) reference with example requests and responses.

> [!NOTE]
> The web app's player and the desktop window's player use this core
> (see [Listen in the web app](../guide/listening.md)). Phone and car
> apps reach it through the [player API](player-apps.md).

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
| `GET /api/library/editions/{id}/playback`           | Whether this server can play the edition for the caller, its tracks and chapters, and the caller's progress and bookmarks |
| `POST /api/playback/sessions`                       | Open a play session: tracks with stream paths, and where to start                                            |
| `POST /api/playback/sessions/{id}/sync`             | Report the position while playing                                                                            |
| `POST /api/playback/sessions/{id}/close`            | Send the last report and end the session                                                                     |
| `GET /api/playback/sessions/{id}/tracks/{index}`    | One track's bytes, whole or by `Range`                                                                       |
| `GET /api/playback/progress`                        | The caller's progress, most recent first                                                                     |
| `PUT`, `DELETE /api/playback/progress/{edition_id}` | Set or forget the caller's progress                                                                          |
| `GET`, `POST /api/library/editions/{id}/bookmarks`  | List or add the caller's bookmarks in an edition                                                             |
| `PUT`, `DELETE /api/playback/bookmarks/{id}`        | Change or remove one                                                                                         |

A book's **sample** (`GET /api/library/editions/{id}/sample`) follows
the same rights check but is not playback: it opens no session, reads or
writes no progress and publishes no event. See
[Book samples](../adr/book-samples.md).

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
  runtime, or its last 30 seconds for a recording over a minute. A report
  without a runtime uses the one already stored, including one another
  report stored a moment before.
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

## The desktop window

The desktop window's custom scheme carries no `Range` requests, so its
page can't stream a track to an `<audio>` element. The window has its own
player instead, and the web app's player drives it in place of an audio
element:

1. The page opens the session and reports progress through the same
   routes as in a browser.
2. To play a track, the page hands its `path` to the window. The host
   reads the track through that same stream route, in process, one
   `Range` request per block, so each block gets the same checks: an
   open session, this server, the patron scope and the files.
3. A confined media worker decodes the blocks (see
   [Audio for the desktop player](media.md#audio-for-the-desktop-player)).
   The window changes the speed (0.5× to 3×) without changing the pitch,
   converts the sample rate, and plays through the system's default
   output device.
4. The system's media controls show the book and its position and send
   their buttons back to the page: MPRIS on Linux, the media overlay on
   Windows and Now Playing on macOS.

The window listens as the patron it is signed in as. While that patron
is elevated to the operator, or impersonating someone from the
elevation, the window still keeps their own listening state, because it
is the person at the computer who is listening. Before the library has
an Owner the window is the operator and has nothing to play as.

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
| `POST /api/library/editions/{id}/chapters/realign` | Administrator | Ask the metadata providers for the chapters again and align them to a stored copy (`202` with the job's id) |

"Administrator" here means the operator, an Owner or an Administrator, as
for every other metadata edit; an Owner can stop one Administrator
changing chapters with the **Fix the catalog** switch (see
[Sharing](../guide/sharing.md)), which leaves the claims readable. An edit is checked strictly and stored
exactly as sent: at most 2,000 chapters, each title 1 to 512 bytes with no
control character and no leading or trailing space, starts that never go
back and are at most 60 days in, and each depth (0 to 7) at most one
deeper than the chapter above. A body that breaks a rule gets `400`, one
over 2 MiB gets `413`.

When the list an edition plays changes, the host emits `edition_updated`
with `fields: ["chapters"]`. Media files keep the chapters they were
written with.

Players read the list from the database:

- The playback manifest's `chapters` holds it in pre-order, each chapter
  with its depth and an `end_ms` at the next chapter's start (the last
  ends at the runtime). The web player shows the chapter playing, lists
  the chapters and skips to the previous or next one.
- The [player apps](player-apps.md) at `/player` get the same list
  flattened, as Audiobookshelf answers: an expanded item's
  `media.chapters` and a play session's `chapters`, in seconds.

A list is only true on the timeline it was measured on. Each claim keeps
that running time, and an uploaded copy records the chapters its own files
carry as a claim of their own. When the copy a player is given runs more
than 2 seconds longer or shorter than the list's timeline, and its files
carry chapters, the manifest and `/player` serve the copy's own chapters
instead. A copy whose files were never probed, or that carries no
chapters, keeps the edition's list.

### Aligning chapters to a copy

An acquire aligns a metadata provider's chapters to the audio while it
holds it. A copy that arrived another way is aligned by a `chapter_align`
job, which an upload naming an ASIN queues once its files are stored and
an Administrator queues with **Re-align chapters** in the book's details
(or `POST /api/library/editions/{id}/chapters/realign`). One edition has
at most one such job waiting; asking again returns it. An edition with no
ASIN gets `409`.

The job:

1. Asks the metadata providers for the edition's chapters by its ASIN.
2. Reads a stored copy from the first of the node's destinations that
   holds every file with a probed running time, into a scratch folder
   under the download cache that is removed when it ends.
3. Places the provider's chapters on the copy's timeline. A copy as long
   as the provider's full runtime keeps the store's brand intro and outro,
   so the starts stand; any other copy has the intro cut from every start
   and the outro's chapters dropped, as an acquire from another store
   does. The destination's chapter title options apply.
4. Snaps each chapter start to where its title is spoken in the file it
   falls in, and records the provider's list and the aligned list as
   claims. The aligned list plays unless a pin or an edit says otherwise.

An edition with no stored copy here, or no provider list, finishes with
nothing recorded.

## Events and live updates

Plugins can consume three host events (see
[Plugin ABI](../plugins/abi.md)): `playback_started`,
`playback_progress` and `playback_finished`. Periodic sync reports
publish nothing.

The web app's live socket has a `playback` topic. It moves when a session
opens or closes, an edition is finished, progress is set outright or a
bookmark changes.
