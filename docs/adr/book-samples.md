# ADR: Book samples never count as listening

- **Status:** Accepted
- **Date:** 2026-11-21
- **Related:** [Playback](playback.md),
  [Untrusted-input fuzzing](untrusted-input-fuzzing.md),
  [Library data model](library-data-model.md)

## Context

A person looking at a book's details wants to hear a few minutes of it
before they acquire it, or before they start it. Storefronts publish a
short public sample for most titles (Audible's catalog record carries a
`sample_url`), and a book the library already holds can give its own.

Three constraints shape how:

- **Hearing a sample is not listening.** Playback progress, finished
  marks and the taste Discover learns from must not move because someone
  previewed a book.
- **The web app loads media from its own origin only.** Its CSP has
  `default-src 'self'`, so the browser cannot play a store's sample URL
  directly; it needs a same-origin path to load.
- **No store special-casing in the host.** Which store has samples, and
  where, is storefront knowledge.

## Decision

### 1. A sample is a preview, outside the playback core

Sample routes open no play session, read or write no progress, keep no
bookmark and publish no event. The web app plays a sample in its own
audio element, never the book player, and pauses a book that is playing
before it starts. Nothing a sample does reaches `playback_progress`,
`playback_sessions`, listening history or Discover.

### 2. One encoding, cut by the media worker

Every sample the daemon serves is cut and encoded by the jailed media
worker's `pcm` mode (`Sample` request): mono MP3 at 64 kbit/s, at most 10
minutes. The worker opens no file and has no network; the host feeds it
byte ranges. The bytes served are never the stored file's or the store's
own, so a malformed store file meets the same decoder as an untrusted
upload and nothing in-process decodes audio.

### 3. Own samples come from the edition this node can play

`GET /api/library/editions/{id}/sample` uses the playback plan's rights
check: anyone who may see the edition may hear its sample, as with
streaming. The sample is cut from the first file on a destination this
node reaches. It starts at the second chapter when the first is shorter
than 3 minutes (opening credits or a store's announcement), and only for
single-file editions, whose chapter times are the file's. The chapters
are the ones the player shows for that copy; a copy whose running time
is off that list's timeline and that carries no chapters of its own is
sampled from its start. It runs 5 minutes.

Samples are cut on demand, the first time someone asks, and kept in
`cache/samples/` under a digest of what they were cut from, so a changed
file gets a new sample. The least recently served are removed once the
folder passes 256 MiB. A person's right to an edition can end at any
time, so its sample is sent `Cache-Control: no-store` and every play
passes the rights check again; a store sample, the same for everyone, may
be kept by the browser for a day. The media worker cuts as many samples
at once as it runs jobs, apart from the job slots, and further requests
wait.

### 4. Store samples are declared by storefronts and fetched by plugins

A storefront declares a sample on its catalog record (`CatalogHit.sampleUrl`
in `plugin.capnp`). The host asks for it the same way title-meta asks for
details: the stores that listed the title by their product ids, when they
declared `detailByProductId`, then the stores that declared `detailByIsbn`.

The browser never loads the store URL itself. The host signs it into a
same-origin path (`/api/samples/{token}`) with this node's own key, as
cover paths are signed, and fetches only paths it issued. The token is
authenticated, not secret: it carries the URL in readable form, so
anyone holding the path can read it, but nobody can make this node fetch
a URL it did not sign. The download goes through the deployed `covers`
plugins with `media = audio` (`CoverFetchParams.media`), in order, until
one returns the body (a plugin that refuses audio passes it on): public
`https` hosts only and a size cap. The plugin refuses an answer whose
`Content-Type` names something else (text, HTML, JSON, an image); it
keeps `audio/*`, `video/mp4` and `application/ogg` (which some CDNs send
for M4A and Ogg audio), a generic octet stream, and an answer with no
type, because what makes a download a sample is the media worker
decoding it, not the header. The host makes no outbound request of its
own. A record whose URL this node cannot sign is passed over for the
next store's.

## Consequences

- A person hears a held book within a few seconds of the first press, and
  at once after that; a title only a store lists depends on that store
  publishing a sample.
- Audible implements `catalogDetail` by ASIN and declares
  `detailByProductId`, so title-meta's store fill may now ask Audible too.
- Other storefronts gain samples by filling `sampleUrl`; the host does not
  change. Libro.fm (`sample_url` of its details), Chirp (`sampleUrl`, by a
  `catalogDetail` it declares with `detailByProductId`) and GraphicAudio
  (the product page's sample player) do.
- The details show the sample control only when a sample can be played
  here: `POST /api/discover/title-sample` also takes the held edition and
  answers its sample path when this node reaches its audio, else a
  store's, else none. Opening a title's details therefore asks the
  listing stores for a sample when the library holds no playable copy;
  answers are cached as above. A library whose editions have no audio on
  this node (the hosted demo's seeded books) offers the stores' samples.
- Samples use cache space (bounded) and media worker time on first play.
- The desktop window plays samples in its web view, not through its
  native audio engine.
