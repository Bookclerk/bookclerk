# ADR: Playback is a host feature with one core

- **Status:** Accepted (decisions made with the maintainer on 2026-11-19).
  Decisions 1 to 7 are built; 8 and 9 are planned
- **Date:** 2026-11-19
- **Related:** [Library data model](library-data-model.md),
  [Patron scopes and linked sign-in](patron-scopes.md),
  [Event delivery model](event-delivery-model.md),
  [Untrusted-input fuzzing](untrusted-input-fuzzing.md)

## Context

Bookclerk acquires audiobooks and files them on storage destinations, but
until now nothing in it played one. People listened in another server's
app and Bookclerk imported their progress from there
(`listening_progress`, one row per provider and work).

Three players are wanted: the web app, the desktop window, and the phone
and car apps people already use with an Audiobookshelf server. Each needs
the same things from the library: which files an edition is, whether this
person may hear them, the bytes by `Range`, and somewhere to keep the
position. Building those three times would give three answers to "may this
person play this".

Two shapes were considered and rejected:

- **Serve app compatibility from the `audiobookshelf` plugin.** That
  plugin links Bookclerk to someone's Audiobookshelf server. Serving a
  player API from it would mean plugins handling arbitrary inbound HTTP
  routes, which the plugin ABI does not do and should not grow to do.
- **A "streaming copy" setting per destination.** Players read the copy
  the library already keeps. A second, stream-shaped copy adds storage and
  a setting for no gain while transcoding is not built.

## Decision

### 1. Playback is built into the host

The player is a native feature. Its tables, routes and rules live in the
host (`bookclerk-library` and `bookclerkd`), and no plugin entrypoint is
added or changed for it. The plugin ABI is not widened to carry general
HTTP handling or routes a plugin was not designed around.

### 2. One playback core, several faces

Everything a player needs comes from one core:

- the tables `playback_progress`, `playback_sessions` and `bookmarks`;
- the routes under `/api/playback` and `/api/library/editions/{id}`;
- one stream route that serves an edition file by `Range`;
- three host events and one live topic.

The web player, the desktop window's native engine and the app-compatible
API (decision 8) are faces over that core. A face adds its own wire shape
and nothing else: it keeps no progress, checks no rights and reads no
files of its own.

### 3. Progress belongs to a patron and an edition

`playback_progress` is keyed `(user_id, edition_id)`. An abridged and an
unabridged recording, or two narrators, are different editions with
different timelines, so a position in one means nothing in the other.

Finishing is a fact about the work: when a listener reaches the end of an
edition (the last 1 percent, or the last 30 seconds of a recording longer
than a minute), the same batch sets `user_title_state.is_finished` for the
edition's work. Opening a finished edition again starts it over and clears
the edition's flag; the work stays finished.

`listening_progress` is unchanged. It holds what other servers report,
per provider; `playback_progress` holds what Bookclerk's own players
report.

### 4. One check decides who may play what

`control_plane::plan_playback` answers whether a person may play an
edition and which files that means. It is the rule a copy grant uses (the
titles of the edition the person may see, and the edition's files on each
destination), narrowed to audio: the primary file and its parts, not
per-chapter files, covers or documents. The manifest, a new session and
every stream request call it.

A patron scope is applied before it, as on every library page: an edition
outside the caller's scope is `404`, the same answer as an unknown id.

The **Download** member control
([patron scopes ADR](patron-scopes.md#member-controls)) covers streaming:
`plan_playback` is the one place that reads it, so a patron who may not
download is planned nothing to play.

### 5. A session is the leave to stream, checked on every request

`POST /api/playback/sessions` records the person, the edition, this node,
the destination and a digest of the edition's files there. A stream
request is served only when:

- its caller is the session's person, signed in now;
- the session is open and was reported on within 24 hours;
- this node opened it;
- the edition is still in the person's scope and `plan_playback` still
  gives the same files (same digest) on the same destination.

So a right that ends, a scope that narrows or a file that is replaced
stops the next request. The player is told `409 session_ended` and opens
a new session, which gives the real answer.

The session cookie is the credential on the stream route too. An `<audio>`
element on the app's own origin plays with no token in a URL, so nothing
secret reaches logs, history or a `Referer`. The stream route streams
from the destination with the code a copy download uses; nothing is
staged or decoded in the host.

Closed sessions are kept as listening history (what was played, on which
kind of player, for how long). They hold no file keys.

### 6. Reports are cheap; changes are announced

A player reports its position every few seconds. A report updates the
session and the progress row in one batch and publishes nothing.

The host publishes, best effort after the write:

| Event | When |
| --- | --- |
| `playback_started` | A session opens |
| `playback_progress` | A session closes, or progress is set outright |
| `playback_finished` | A listener reaches the end of an edition |

They are host event types: a plugin can consume them and cannot produce
them. The `playback` live topic moves on a session opening or closing, a
finish, an outright progress write and a bookmark change, so another open
tab re-reads Continue listening without hearing every report.

### 7. Player input is an untrusted boundary

A sync body and a stream path are parsed sans IO in `bookclerk-library`
(`parse_playback_sync_request`, `parse_track_path`) and fuzzed as
`playback_request` against an independent oracle. Positions and runtimes
are a bounded type (at most sixty days of milliseconds), a session lists
at most 512 tracks, and an edition takes at most 200 bookmarks per person.

### 8. Apps connect to the host at `/player` (planned)

Apps written for an Audiobookshelf server connect to Bookclerk at
`/player`, served by the host from a first-party crate over the core
above. The path names Bookclerk's own implementation; it is not `/abs`.
Sign-in there mints library cards (a short access token and a rotating
refresh token), and every item, progress and stream call lands on the
same tables and the same check as the web player.

### 9. The desktop window plays natively

The desktop window's custom scheme delivers no request headers on Linux,
so it cannot serve `Range` to a web view. The window decodes in the
confined media worker and plays through the operating system's audio and
media controls, reading files and writing progress through the same
core.

The host reads each block of the track through the session's own stream
route in process, so the window gets no second path to the files. A
`pcm` worker per track decodes them under the same rules as the tag
worker: no path, no network, ranges only inside the file. Speed changes
use WSOLA time-stretching and the rate conversion uses a windowed-sinc
resampler, both in a small engine crate (`bookclerk-player-engine`) with
no I/O. The page's stage B player drives that engine as if it were an
audio element, so the timeline, sync and bookmark logic exist once. The
system's media controls come from `souvlaki`. Until the desktop app has
its own sign-in, the window listens as the library's earliest active
Owner.

## Consequences

- The core itself has no user interface. The routes are in the Web API
  reference and are exercised by tests; the web app's player is their
  first caller.
- An edition is playable only on a node that has one of its destinations
  deployed. A session is tied to the node that opened it; behind a load
  balancer without session affinity a stream request can reach another
  node and be answered `421`.
- Runtime is what a player reports, or the catalog's length to the minute
  before one has. Exact per-file lengths for editions in several files
  arrive with the stored media probe.
- Playback events carry a user id, an edition and a position. A plugin
  that consumes them learns what a person listens to, which an operator
  accepts when granting that plugin the event.
- `playback_sessions` grows by one row per stretch of listening and is
  not pruned yet.
- No transcoding: a file the browser cannot decode does not play in the
  web player until transcoding is built.
