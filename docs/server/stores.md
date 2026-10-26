# Stores reference

For connecting a store in plain language, see [Stores](../guide/stores.md).

Sources implement the shared `ContentSource` / storefront entrypoint: login,
list accounts, scan library, fetch title. There is no plugin `kind = "source"`
— storefront guests export `storefront` under `entrypoints` (see
[plugins.md](../plugins/README.md)). A store is enabled on a host by deploying its plugin
instance there (`bookclerk plugins deploy <id>` or **Settings → Plugins**);
its knobs are the plugin's `[[settings]]`, stored in the instance document.
Nothing about a store lives in `config.toml` or the environment.

| Id | Auth | Media path | Notes |
| --- | --- | --- | --- |
| `audible` | Amazon OAuth (QR / callback / external) | Adrm / Widevine; guest decrypt → Plain | [audible-rs](https://github.com/mkb79/audible-rs) |
| `libro` | Email + password | DRM-free M4B/ZIP | Password entered in Accounts |
| `chirp` | Email + password (GraphQL) | Plain MP3 after URL decrypt | Password entered in Accounts |
| `graphicaudio` | Email + password | Plain web / ZIP / device stream | Password entered in Accounts |

First-party source guests live under `crates/bookclerk-plugins/optional/`;
installs are discovered on the plugin search path (`$BOOKCLERK_FILES_DIR/plugins`
or `BOOKCLERK_PLUGIN_DIRS`). Third-party storefront plugins use the same
manifest and instance model.

Accounts belong to the **plugin key**, not to an instance or a host: once a
store account is connected, every deployed instance of that plugin on every
host can scan and acquire for it. An account id is unique per plugin; when
the same id exists on two stores, CLI commands that act on one row take
`--source`.

## Connecting stores

Connect bookstore accounts from the **User SPA Accounts** UI (administrator or
member session via claim ticket / integration login). Operators cannot link
stores. There is no `bookclerk auth` CLI group.

```bash
bookclerk library accounts
bookclerk library status
bookclerk library set-scan <account> --scan false [--source <id>]
bookclerk library revoke-account <account> [--source <id>]
bookclerk library scan --source <id> [--instance <plugin-instance-id>]
bookclerk library acquire <title-id> [--instance <plugin-instance-id>]
```

After scan, non-Audible rows can be enriched with an Audible ASIN via public
catalog search when `library.enrich_from_audible = true` (default). No Audible
account is required for enrichment. Tune `library.enrich_min_confidence`
(default `90`).

## Audible

Connect Audible from Accounts (Amazon OAuth). Re-connect to refresh / re-register
as Android when Widevine L3 provisioning requires it.

### Decrypt / formats

Audible DRM decrypt is native inside the Audible plugin. Host packaging
(`bookclerk-media`) handles remux / metadata / MP3 encode only:

| Path | When |
| --- | --- |
| **Adrm aaxc** | Default when `output.widevine = false` |
| **Widevine DASH fMP4/CENC** | `output.widevine = true`, or automatic fallback on Audible `000307` when a CDM is available |
| **MP3** | `output.format = "single_mp3"` (Symphonia + LAME) |
| **xHE-AAC** | `output.xhe_aac = true` on the Widevine path |

Widevine **L3** (software) covers stereo / xHE-AAC. Spatial/Atmos needs **L1**
and is not available on desktop (same limitation as classic Libation).

On first Widevine acquire, an L3 `.wvd` is auto-provisioned via classic Libation
AudibleCdm and cached in the `encrypted_secrets` DB table (kind=widevine). Override with
`output.widevine_cdm`, or set `output.widevine_cdm_provider = "off"` to require
bring-your-own only. Login registers as an Android device.

Optional brand-audio trim: `output.strip_audible_brand_audio = true`.

Credentials are stored in `encrypted_secrets` (DB-backed). Classic Libation import
(`import libation`) converts `AccountsSettings.json` account metadata only;
IdentityTokens are not converted. Re-connect Audible from the Accounts UI, or see
[backups.md](backups.md) for classic import paths.

Low-level auth/download notes: [`crates/bookclerk-plugins/optional/source-audible/README.md`](../../crates/bookclerk-plugins/optional/source-audible/README.md).

### Settings

| Setting | Default | Values |
| --- | --- | --- |
| `bitrate` | `high` | `high` / `normal` |

```bash
bookclerk plugins config set audible bitrate normal
```

Widevine, xHE-AAC, and the packaging format are shared `core.output`
settings (seeded once from `[output]`; see
[configuration.md](configuration.md#output-shared)):

```bash
bookclerk config set output.widevine true
bookclerk config set output.xhe_aac false
bookclerk config set output.format enriched_m4b
```

## Libro.fm

Connect from the Accounts UI (email + password). Then:

```bash
bookclerk library scan --source libro
bookclerk library acquire --isbn 978…
```

| Setting | Default | Values |
| --- | --- | --- |
| `container` | `m4b` | `m4b` / `zip` |
| `base_url` | store default | API origin override (testing) |

Credentials are stored in `encrypted_secrets` (DB-backed).

## Chirp

Connect from the Accounts UI (email + password). Then:

```bash
bookclerk library scan --source chirp
```

| Setting | Default | Notes |
| --- | --- | --- |
| `graphql_url` | store default | GraphQL endpoint override (testing) |

Headless login uses GraphQL password `signIn` (not browser cookie sessions).
Some egress paths see Cloudflare challenges — use a normal browser User-Agent
context if probes fail. Research background: [source-candidates.md](../contributing/research/source-candidates.md).

## GraphicAudio

Connect from the Accounts UI (email + password). Three access modes (pick one;
default `web`):

| `access` | Behavior |
| --- | --- |
| `web` | Magento Browser Player + CloudFront cookies (no device slot) |
| `zip` | Magento “My Downloadable Products” ZIP (consumes download attempts) |
| `device` | Access App API Hi/Lo streams (uses a device slot / `client_id`) |

```bash
bookclerk plugins config set graphicaudio access web      # web | zip | device
bookclerk plugins config set graphicaudio bitrate hi      # hi | lo (device)
bookclerk plugins config set graphicaudio container auto  # auto | m4b | mp3 | flac (zip)
bookclerk library scan --source graphicaudio
```

`base_url` and `store_url` override the Access App and Magento origins for
testing. The Magento paths use the password stored when the account was
connected.

ZIP SKUs unlock App + Browser as well; App Access–only does **not** include a
computer ZIP. Prefer purchasing an M4B/MP3/FLAC ZIP SKU when validating all
paths. Full Magento/device notes: [source-candidates.md](../contributing/research/source-candidates.md).

## Disabling a store

```bash
bookclerk plugins undeploy chirp              # this host
bookclerk plugins undeploy chirp --node <id>  # another cluster node
```

The deployment row flips to `absent`, the reconciler retires the running
guest, and the store's accounts and books stay in the library.

## Future stores

Candidate research (Kobo, Storytel, Downpour, …) lives in
[source-candidates.md](../contributing/research/source-candidates.md). New first-party adapters implement
`ContentSource`; third parties can ship external source plugins when the v1
protocol fits (plain fetch today; encrypted fetch remains first-party-only).
