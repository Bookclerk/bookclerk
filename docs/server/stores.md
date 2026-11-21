# Stores reference

This page is the operator's reference for the store plugins: how they
load, their settings, and the commands that drive them. To connect a store
in plain language, see [Stores](../guide/stores.md).

## How stores work

A store is a **storefront plugin**. It can sign in, list accounts, scan a
library and fetch a title (the shared `ContentSource` interface).

- A storefront plugin exports `storefront` under `entrypoints` in its
  manifest. There is no plugin `kind = "source"`. See
  [Plugins](../plugins/README.md).
- You turn a store on for a host by deploying its plugin there:
  `bookclerk plugins deploy <id>`, or **Settings → Plugins**.
- A store's settings are its plugin settings, stored in the plugin
  instance. Nothing about a store lives in `config.toml` or the
  environment.

| Id | Sign-in | Audio | Notes |
| --- | --- | --- | --- |
| `audible` | Amazon OAuth (QR, callback or external) | Adrm or Widevine, decrypted in the plugin | Built on [audible-rs](https://github.com/mkb79/audible-rs) |
| `libro` | Email and password | DRM-free M4B or ZIP | Password entered in Accounts |
| `chirp` | Email and password (GraphQL) | Plain MP3 after URL decryption | Password entered in Accounts |
| `graphicaudio` | Email and password | Plain web, ZIP or device stream | Password entered in Accounts |

The first-party store plugins live in `crates/bookclerk-plugins/optional/`.
Installed plugins are found on the plugin search path:
`$BOOKCLERK_FILES_DIR/plugins`, or `BOOKCLERK_PLUGIN_DIRS`. Third-party
storefront plugins use the same manifest and instance model.

### Accounts belong to the plugin

A store account belongs to the **plugin key**, not to an instance or a
host. Once an account is connected, every deployed instance of that
plugin, on every host, can scan and acquire for it.

An account id is unique within one plugin. When the same id exists on two
stores, commands that act on one account take `--source`.

## Connecting stores

People connect their own store accounts in **Accounts** in the web app,
signed in as themselves (from an invite link or an integration login).
Operators can't link stores, and there is no `bookclerk auth` command.

Commands for the accounts once they are connected:

| Task | Command |
| --- | --- |
| List accounts | `bookclerk library accounts` |
| Show library status | `bookclerk library status` |
| Leave an account out of scans | `bookclerk library set-scan <account> --scan false [--source <id>]` |
| Disconnect an account (its books stop counting as held; files stay) | `bookclerk library revoke-account <account> [--source <id>]` |
| Scan one store | `bookclerk library scan --source <id> [--instance <plugin-instance-id>]` |
| Acquire one title | `bookclerk library acquire <title-id> [--instance <plugin-instance-id>]` |

### How a scan reads a library

A scan reads each account's library a page at a time and saves its place
after every page, so a large library never has to arrive in one piece.

- A scan that stops part way, from a restart or a store error, carries on
  from the last saved page the next time, if that was within a day.
- Audible scans after the first ask only for what changed since the last
  one. Every seven days a scan reads the whole library again.
- Libro.fm, Chirp and GraphicAudio have no way to ask for changes, so
  every scan reads the whole library from the store. The plugin remembers
  a fingerprint of each book the last scan saw and passes on only the
  books that are new or changed, and the ones that left. Every seven days
  a scan passes on the whole library again.
- That memory is the plugin's own `SCAN_STATE` database, which the
  plugin's consent lists with its other permissions. Until you approve it,
  these stores pass on the whole library on every scan, as before.
- Each page is saved in a few database writes sized to what your database
  accepts, so the same scan fits Cloudflare D1's smaller limits. Books the
  library already has, as the store still describes them, are checked
  together for the whole page instead of one at a time.
- A book that leaves an account (a returned loan, a refund) stops counting
  as held by that account: the next scan reports it. Its downloaded files
  stay.
- A scan that runs on a [host](branches.md) works the same way: the
  library hands it the account's place, so an Audible scan on a host also
  asks only for what changed. Libro.fm, Chirp and GraphicAudio scans on a
  host pass on the whole library, since their memory stays on the
  library's own server.

### Matching to Audible's catalog

After a scan, books from other stores can get an Audible ASIN from
Audible's public catalog search. This needs no Audible account.

The match is made per edition: every held edition that has no ASIN yet
is searched in the marketplace of the store that sells it. A confident
match records the ASIN on the edition, fills in the description, genres,
cover and language the book still lacks, and marks the edition so the
match is not repeated. An ASIN another edition in the library already
has stays with that edition, and the marked edition is not matched again.
An edition that already has an ASIN, such as one an Audible scan
reported with its ASIN, is skipped.

Genres from Audible's catalog are its genre categories (such as *Science
Fiction & Fantasy*, *Fantasy*, *Epic*). The subject keywords the catalog
also sends are never used: they mix a few subjects with Audible's internal
labels, such as `AI_edition_inclusion`.

- It is on while `library.enrich_from_audible = true` (the default).
- `library.enrich_min_confidence` sets how sure a match must be (default
  `90`).
- A title that is itself an ASIN (`B0` and eight letters or digits, or an
  ISBN-10: nine digits and a digit or `X`) is looked up directly. Any other title, including a
  ten-letter one such as *Foundation*, is matched by search and scored.

## Audible

Connect Audible from **Accounts**, which signs in with Amazon OAuth.
Sign-in always registers the device as Android: Widevine L3 provisioning
needs an Android registration, so there is no option to register as an
iPhone. Connect again to refresh the sign-in, or when L3 provisioning
needs the device registered again.

The plugin's network grants cover the Amazon and Audible hosts of every
Audible marketplace (`*.amazon.co.uk`, `*.audible.de`, `*.amazon.co.jp`
and so on), so accounts outside the US can refresh their sign-in and sign
the device out when the account is removed.

> [!NOTE]
> Earlier versions granted only the US hosts. After an upgrade the plugin
> keeps running under its earlier grant until you approve the new hosts:
> `bookclerk plugins approve audible --yes` (see
> [Plugins](plugins.md#consent-rides-on-the-version)).

### Decryption and formats

The Audible plugin decrypts Audible's DRM itself. The host's media
packaging (`bookclerk-media`) only remuxes, fixes metadata and encodes
MP3 or Opus.

| Path | When it is used |
| --- | --- |
| **Adrm aaxc** | The default, while `output.widevine = false` |
| **Widevine DASH fMP4/CENC** | With `output.widevine = true`, or automatically after Audible error `000307` when a CDM is available |
| **MP3** | With `output.format = "single_mp3"` (Symphonia and LAME) |
| **Opus** | With `output.format = "opus"` (Symphonia and libopus) |
| **xHE-AAC** | With `output.xhe_aac = true`, on the Widevine path |

Widevine **L3** (software) covers stereo and xHE-AAC. Spatial audio and
Atmos need **L1**, which isn't available on a desktop, as in classic
Libation, so the plugin never asks Audible for them: every license
request is for the stereo edition.

### Quality offers

Audible tells the [daily upgrade check](output.md#upgrading-to-better-files)
which file an account would download for a book today. The plugin asks
Audible's content metadata for each book with the same DRM path, quality
and xHE-AAC choice a download would use, and reads the codec and the
`<FORMAT>_<kHz>_<kbps>` content format from the answer (`AAX_44_128` is
AAC-LC at 44.1 kHz and 128 kbps). Nothing is licensed or downloaded.

Spatial streams (Dolby E-AC-3 and AC-4) are never offered, since the
plugin never downloads them, and an xHE-AAC stream is offered only while
`output.xhe_aac` is on. A book Audible gives no usable answer for is
skipped.

### The Widevine CDM

On the first Widevine acquire, Bookclerk provisions an L3 `.wvd` through
classic Libation's AudibleCdm. It caches it in the `encrypted_secrets`
table (kind `widevine`). Sign-in registers as an Android device.

- To use your own CDM, set `output.widevine_cdm`.
- To require your own and never provision one, set
  `output.widevine_cdm_provider = "off"`.

### Other Audible options

- **Trim the Audible brand audio**: `output.strip_audible_brand_audio = true`.
  The intro and outro are cut from every Audible download, decrypted or
  already clear, and the chapters move with the audio, so each chapter
  still starts where its narration does. A file that is not MP4 is
  stored untrimmed.
- **Bitrate**, a plugin setting:

  | Setting | Default | Values |
  | --- | --- | --- |
  | `bitrate` | `high` | `high`, `normal` |

  ```sh
  bookclerk plugins config set audible bitrate normal
  ```

Widevine, xHE-AAC and the packaging format are shared `core.output`
settings, seeded once from `[output]`:

```sh
bookclerk config set output.widevine true
bookclerk config set output.xhe_aac false
bookclerk config set output.format enriched_m4b
```

See [Configuration](configuration.md#output-shared).

### Audible credentials

Credentials are stored in `encrypted_secrets` in the database.

> [!NOTE]
> Importing from Libation (`import libation`) brings over account details
> from `AccountsSettings.json` only. IdentityTokens are not converted, so
> connect Audible again in **Accounts**. See
> [Backups, updates, import and export](backups.md).

Low-level sign-in and download notes:
[`crates/bookclerk-plugins/optional/source-audible/README.md`](../../crates/bookclerk-plugins/optional/source-audible/README.md).

## Libro.fm

Connect Libro.fm from **Accounts** with your email and password. Then:

```sh
bookclerk library scan --source libro
bookclerk library acquire --isbn 978…
```

| Setting | Default | Values |
| --- | --- | --- |
| `container` | `m4b` | `m4b`, `zip` |
| `base_url` | the store's | Overrides the API address for sign-in, scans, downloads and the catalog, for testing |

Credentials are stored in `encrypted_secrets` in the database.

Catalog search, title details, related titles and purchase links work
without an account. They use the signed-out catalog of the Libro.fm Android
app's API, with a user agent that names Bookclerk and the app-version header
that API requires. Libro.fm's website pages refuse automated requests, so
the plugin does not read them.

Search lists the title, ISBN, authors, narrators and cover. Title details
add the description, genres, running time, publisher, series, abridged
status, language, list price in US dollars and related titles.

A purchase link points at the matching ISBN on Libro.fm, or else at the
best title-and-author match. A book that the details call reports as not
for sale never gets a link, even through the title match.

## Chirp

Connect Chirp from **Accounts** with your email and password. Then:

```sh
bookclerk library scan --source chirp
```

| Setting | Default | Notes |
| --- | --- | --- |
| `graphql_url` | the store's | Overrides the GraphQL address for sign-in, scans, downloads and the catalog, for testing |

The plugin signs in with Chirp's GraphQL `signIn` mutation, not a browser
cookie session.

> [!TIP]
> Some networks get Cloudflare challenges from Chirp. If probes fail, try
> from a normal browser User-Agent context.

Background research:
[Store candidates](../contributing/research/source-candidates.md).

## GraphicAudio

Connect GraphicAudio from **Accounts** with your email and password. Then
pick one of three ways to get the audio with the `access` setting:

| `access` | How it gets audio | Cost |
| --- | --- | --- |
| `web` (default) | The Magento browser player, with CloudFront cookies | No device slot |
| `zip` | Magento's **My Downloadable Products** ZIP | Uses up download attempts |
| `device` | The Access App API's Hi and Lo streams | Uses a device slot (`client_id`) |

```sh
bookclerk plugins config set graphicaudio access web
bookclerk plugins config set graphicaudio bitrate hi
bookclerk plugins config set graphicaudio container auto
bookclerk library scan --source graphicaudio
```

- `bitrate` is `hi` or `lo`, for `device` access.
- `container` is `auto`, `m4b`, `mp3` or `flac`, for `zip` access.
- `base_url` and `store_url` override the Access App and Magento
  addresses, for testing. Catalog search, details, related titles and
  prices use `store_url` too.

The Magento paths use the password stored when the account was connected.

Catalog search and Discover read GraphicAudio's public store, which needs no
account. Search and series pages only list a title, a link and a cover. The
plugin's title-details call reads the product page instead: catalog search
uses it to fill the GraphicAudio rows on a results page, and title dialogs
use it for a title GraphicAudio listed. A product page gives the author,
the cast (shown as narrators), the synopsis, the ISBN, the release date,
the running time, the genre, the review score and the price.

The store's search answers a query it can't match, such as one with a
colon or an author's name, with an alphabetical list of its whole catalog.
The plugin sends the query as plain words, and when the answer still looks
like that list it tries again without the last word, up to three searches,
judging each answer by the words it sent. It stops early when only common
or short words would be left.
If every search gets the list, the plugin returns no results rather than
the list. A later page of results uses the search that worked on page 1,
and comes back empty when that page is the list. Author and narrator
searches are sent once and kept as they come.

> [!NOTE]
> A ZIP purchase also unlocks the app and the browser player. An
> app-only purchase does **not** include a computer ZIP. To test every
> path, buy an M4B, MP3 or FLAC ZIP edition.

Full Magento and device notes:
[Store candidates](../contributing/research/source-candidates.md).

## Turning a store off

```sh
bookclerk plugins undeploy chirp
bookclerk plugins undeploy chirp --node <id>
```

The first command acts on this host, the second on another node of the
library. The deployment becomes `absent`, the reconciler stops the
running plugin, and the store's accounts and books stay in the library.

## Future stores

Research on candidates such as Kobo, Storytel and Downpour is in
[Store candidates](../contributing/research/source-candidates.md).

- New first-party stores implement `ContentSource`.
- Third parties can ship their own store plugins when the v1 protocol
  fits: plain fetch works today, while encrypted fetch is still
  first-party only.

## Next steps

- [Managing plugins](plugins.md): deploy, configure and update plugins.
- [Output and destinations](output.md): where acquired books go.
- [Stores](../guide/stores.md) on the Use Bookclerk shelf: the steps people follow in the
  web app.
