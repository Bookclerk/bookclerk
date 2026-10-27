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
| Disconnect an account | `bookclerk library revoke-account <account> [--source <id>]` |
| Scan one store | `bookclerk library scan --source <id> [--instance <plugin-instance-id>]` |
| Acquire one title | `bookclerk library acquire <title-id> [--instance <plugin-instance-id>]` |

### Matching to Audible's catalog

After a scan, books from other stores can get an Audible ASIN from
Audible's public catalog search. This needs no Audible account.

- It is on while `library.enrich_from_audible = true` (the default).
- `library.enrich_min_confidence` sets how sure a match must be (default
  `90`).

## Audible

Connect Audible from **Accounts**, which signs in with Amazon OAuth.
Connect again to refresh the sign-in, or when Widevine L3 provisioning
needs the device registered as Android again.

### Decryption and formats

The Audible plugin decrypts Audible's DRM itself. The host's media
packaging (`bookclerk-media`) only remuxes, fixes metadata and encodes
MP3.

| Path | When it is used |
| --- | --- |
| **Adrm aaxc** | The default, while `output.widevine = false` |
| **Widevine DASH fMP4/CENC** | With `output.widevine = true`, or automatically after Audible error `000307` when a CDM is available |
| **MP3** | With `output.format = "single_mp3"` (Symphonia and LAME) |
| **xHE-AAC** | With `output.xhe_aac = true`, on the Widevine path |

Widevine **L3** (software) covers stereo and xHE-AAC. Spatial audio and
Atmos need **L1**, which isn't available on a desktop, as in classic
Libation.

### The Widevine CDM

On the first Widevine acquire, Bookclerk provisions an L3 `.wvd` through
classic Libation's AudibleCdm. It caches it in the `encrypted_secrets`
table (kind `widevine`). Sign-in registers as an Android device.

- To use your own CDM, set `output.widevine_cdm`.
- To require your own and never provision one, set
  `output.widevine_cdm_provider = "off"`.

### Other Audible options

- **Trim the Audible brand audio**: `output.strip_audible_brand_audio = true`.
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
| `base_url` | the store's | Overrides the API address, for testing |

Credentials are stored in `encrypted_secrets` in the database.

## Chirp

Connect Chirp from **Accounts** with your email and password. Then:

```sh
bookclerk library scan --source chirp
```

| Setting | Default | Notes |
| --- | --- | --- |
| `graphql_url` | the store's | Overrides the GraphQL address, for testing |

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
  addresses, for testing.

The Magento paths use the password stored when the account was connected.

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
