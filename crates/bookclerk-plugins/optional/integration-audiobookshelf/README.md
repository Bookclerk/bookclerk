# Audiobookshelf integration

Outbound Audiobookshelf plugin (scan, listening sync, credential return-visit,
and Audiobookshelf's sidecar files).
A TypeScript guest that runs in `workerd`; setup is in
[docs/server/integrations.md](../../../../docs/server/integrations.md#audiobookshelf).

## Layout

| Module | Does |
| --- | --- |
| `src/index.ts` | The entrypoint classes (`remoteLibrary`, `oidc`, the default one with the `book_acquired` consumer, the socket handler and migrations) |
| `src/guest.ts` | What each entrypoint method does, with every binding passed in |
| `src/client.ts` | The Audiobookshelf REST client (no redirects, size and request caps) |
| `src/watch.ts` | Remembers which users were reported, in `LISTENING_STATE` |
| `src/listening.ts` | Remembers what listening progress was sent, in `LISTENING_STATE` |
| `src/keyed.ts` | Paged reads and chunked writes within the database's limits |
| `src/push.ts` | The Socket.io conversation, frame by frame |
| `src/hash.ts` | The hashed keys the database stores instead of user ids |
| `src/settings.ts` | Reads the instance settings and the API key |
| `src/dialects.ts` | The sidecar formats by id, behind the `metadataFiles` entrypoint |
| `src/abs-json.ts` | `metadata.json` |
| `src/nfo.ts` | The plain-text `.nfo` |
| `src/plain-text.ts` | `desc.txt` and `reader.txt` |
| `src/text.ts` | Decoding and value cleaning shared by the formats |

`npm run build` compiles `src/` into the `modules/` that ships; both are
committed. `npm test` runs the unit tests and the sidecar fuzzing (see
[Fuzzing](../../../../docs/contributing/fuzzing.md#metadata-file-dialect-guests)). The database tests run under
every `bookclerk-db-testkit` profile, so set `BOOKCLERK_DB_TESTKIT` to the
built kit (`cargo build -p bookclerk-db-testkit`) or put it on `PATH`.

## Network

The manifest asks for any public website (`domains = ["*"]`), because the
server's address is a setting. The plugin only ever calls `base_url`. A
server on a private or loopback address needs the operator to add its host
and CIDR to the plugin's consent.

## New users

With `watch_users` on, the host polls the plugin for new Audiobookshelf
users and mints a claim ticket for each. The plugin also opens
Audiobookshelf's Socket.io channel through the host (`[sockets]`): the host
holds it while the plugin sleeps and answers its pings, and a `user_added`
message makes the plugin ask the host to poll at once. `src/push.ts` reads
the channel; `src/watch.ts` remembers which users were reported.

## Sidecar files

The plugin reads and writes Audiobookshelf's own sidecar files through
the `metadataFiles` entrypoint. The host hands each file in and takes it
back, so these calls need no network or file grant.

| Dialect | File | Writes | Carries |
| --- | --- | --- | --- |
| `abs_metadata_json` | `metadata.json` | yes | title, subtitle, description, publisher, date, language, genres, tags, abridgement, ISBN and ASIN, authors and narrators, series, chapters |
| `abs_nfo` | `*.nfo` | yes | the same as `metadata.json` without chapters |
| `desc_txt` | `desc.txt` | yes | description |
| `reader_txt` | `reader.txt` | yes | narrators |

### How each format reads

- **Audiobookshelf `metadata.json`.** The keys Audiobookshelf 2.x writes:
  `authors` and `narrators` are name lists, `series` entries are
  `"Name #position"`, `publishedDate` wins over `publishedYear`, `abridged`
  is a boolean (`false` reads as unabridged), and `chapters` are
  `{ start, end, title }` in seconds. Values nested under a legacy
  `metadata` object win over the top level, as in Audiobookshelf. A key
  with `null` or an empty list states the field as empty.
- **NFO text.** `Key: value` lines, keys compared without case, the last
  line for a key winning, then an optional `Book Description` section to
  the end of the file. `Title` holds the subtitle after its first `:`;
  `Author`, `Narrator` (or `Read By`), `Genre` and `Tags` are comma
  separated; `Series Name` and `Position in Series` make one series entry;
  `Release Date`, `Date` or `Audible.com Release` give the date, and
  copyright lines give the year when none of those exist; `Unabridged` or
  `Abridged` answer `Yes` or `No`. A line with an empty value states
  nothing.
- **`desc.txt`** is the description, trimmed. **`reader.txt`**'s first
  line holds the narrators, split at `,`, `;`, `&` and `and`.

Every value is cleaned before it reaches the host: control characters
are dropped, labels stay on one line, text is cut to the ABI's byte caps
on a character boundary, repeats and blanks are dropped, and dates must be
real `YYYY`, `YYYY-MM` or `YYYY-MM-DD` dates. A file is refused only when
its structure is unreadable (a JSON file that is not a JSON object). A
file that is not valid UTF-8 is read as Windows-1252.

### How each format writes

Rendering changes only the fields the host asks for and keeps everything
else in the file:

- `metadata.json` keeps every other key in place and writes with
  two-space indentation, as Audiobookshelf does. A removed field loses its
  key. A credit role the record does not state keeps its key.
- An NFO rewrites a field's first line (keeping its key and spacing),
  drops the field's other lines, adds a new field after the last
  `Key: value` line, and keeps every other line and the file's line
  endings (CRLF, LF or CR). A removed date also drops the copyright lines
  it would otherwise be read from.
- `reader.txt` replaces its first line and keeps the rest.

A value the format cannot hold so that it reads back the same is refused
with `invalid_params` rather than written changed: a series position with
a space or `#` in `metadata.json`, a list entry with a comma or a title
with a colon in an NFO, a narrator name holding a separator in
`reader.txt`. So is a `metadata.json` holding a number that a re-write
would change: any number whose exact value a double cannot hold, however
it is spelled (`9007199254740993`, `9007199254740993e0`, `1e-400`, `1e400`).

An answer with nothing left in it is empty, and the host decides whether
to remove the file. Formats keep one value where the record can hold
several: the NFO keeps the first series, and both `metadata.json` and the
NFO keep the first ISBN and the first ASIN.

## OpenID Connect (Bookclerk as IdP)

Bookclerk hosts a core OIDC authorization server (`/.well-known/openid-configuration`,
`/oidc/*`) so ABS can use Bookclerk Users as the login source. Configure ABS
OpenID with:

- Issuer: Bookclerk `integrations.public_origin` (or the daemon base URL)
- Client id: `audiobookshelf` (plugin-provided public PKCE client; enable it
  under Settings → Sign-in). Redirect URIs are derived from this plugin’s
  `base_url` plus `/auth/openid/callback` (ABS’s own UI, default port 13378 —
  not Bookclerk’s 8787 listen port) and are not edited by hand.
- Auth method: none / PKCE S256

Consent and token minting require a **User** portal session. The shared operator
token cannot obtain user access tokens. Details: [docs/server/integrations.md](../../../../docs/server/integrations.md).
