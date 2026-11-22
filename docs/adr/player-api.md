# ADR: The player API for apps at `/player`

- **Status:** Accepted (decisions made with the maintainer on 2026-11-19
  and 2026-11-20). Supersedes the "planned" note of decision 8 of the
  [Playback ADR](playback.md). Its consequence that chapters are empty
  is superseded by decision 7 of the
  [Edition chapters ADR](edition-chapters.md): apps get the chapters the
  edition plays
- **Date:** 2026-11-20
- **Related:** [Playback](playback.md),
  [Patron scopes and linked sign-in](patron-scopes.md),
  [Untrusted-input fuzzing](untrusted-input-fuzzing.md)

## Context

People already listen in phone and car apps written for an
Audiobookshelf server: the official app, Absorb, AudioBooth, Lissen and
Plappa. Those apps speak one HTTP API and one Socket.IO socket. Bookclerk
has a playback core (progress, sessions, bookmarks, one rights check and
one stream route), so answering that API from the core lets those apps
play a Bookclerk library without a second server.

Three constraints shaped the answer:

- The plugin ABI does not handle arbitrary inbound HTTP routes, and should
  not grow to. An `audiobookshelf` plugin cannot serve the API.
- The path must name Bookclerk's own implementation; it is not `/abs`.
- The apps sign in with a username and password. Bookclerk's account
  password must never be typed into a third-party app.

## Decision

### 1. The host serves the API from a first-party crate

`bookclerk-player-api` holds everything about the wire: request parsers,
response shapes and the Socket.IO framing, all sans-IO. `bookclerkd`
mounts the routes at `/player` (`crates/bookclerkd/src/player/`) and
answers them from the playback core and the library's page index. An app
is given the library's address plus `/player` as its server address.

### 2. Apps hold player cards, never sessions

A sign-in mints a **player card**: an access token that lives an hour and
a refresh token that lives thirty days, a new pair on every refresh with
the old refresh token accepted for ten more minutes. A card is a
`portal_sessions` row of kind `player`, so it shows in the patron's
session list and is signed out there. Only the token's digest is stored.

The carded routes sit behind one layer that drops cookies and
`Authorization` values, reads the access token from
`Authorization: Bearer` or `?token=` (apps put it in audio and cover
URLs), and resolves it to an active patron and their scope. A browser
session never reaches a `/player` route, and a card never reaches any
other route.

### 3. Two ways to sign in, neither with the account password

- **Player passwords.** A patron makes one per device in Account
  settings: 20 symbols from an alphabet without look-alike characters,
  shown once, stored as a digest, at most 20 per patron. An app signs in
  with the patron's sign-in name (or email) and a player password.
  Removing a password signs out every card it made.
- **Browser sign-in.** `/player/auth/openid` sends the app's browser to
  the library's consent page (`/apps/approve`), signed in as usual. On
  approval the library hands the app a single-use code bound to a PKCE
  S256 challenge, through the library's own return path, to one of the
  app addresses the library knows. The app trades code and verifier at
  `/player/auth/openid/callback`.

The second-factor rule applies to cards as to sessions: a patron who must
enroll first is refused.

### 4. What apps see is what the patron may see

A library is the whole library (one entry). An item is an audio edition
the library acquired, listed by the patron's scope through the page
index exactly as the library's own pages are. Authors, series and
narrators are page index values; their ids are the value's key in
URL-safe Base64, so the same id works as a filter. Collections and
playlists answer empty, podcasts and ebooks are not listed, and
administration routes are not served.

### 5. Streaming is always allowed; downloads follow the Download control

Every stream goes through the playback core's check on every request.
Anyone who may see a book may stream it. The Download member control
gates only saving files for offline listening (the file download route),
as an Audiobookshelf server's `download` permission does, and is what
apps show as that permission.

### 6. Live updates over a websocket only

`/player/socket.io/` speaks Engine.IO 4 and Socket.IO 5 over a websocket;
long polling is not served. The app signs the socket in with its access
token. The socket sends the patron's progress changes and items newly
added to what they may see, read from the same revisions as the web
app's live socket, and re-checks the card every minute.

### 7. Every untrusted input is fuzzed

The request parsers and the socket's frame reader are the
`player_request` and `player_socket` fuzz targets, with oracles shared by
a replay test in the crate.

## Consequences

- No TLS requirement is added for `/player`: it follows the web sign-in.
  Operators exposing a library put it behind TLS as the
  [security checklist](../server/security.md) says.
- Tracks play directly. An app that asks for transcoding gets the same
  direct tracks; HLS and transcoding come later.
- Chapters are empty until metadata providers supply them.
- Per-file durations come from probed bitrates, or from each file's share
  of the catalog runtime, until the stored media probe gives exact ones.
- AudioBooth finds servers only by scanning for port 13378; it is pointed
  at the library by hand.
