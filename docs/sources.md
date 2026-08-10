# Content sources (storefronts)

Sources implement the shared `ContentSource` trait: login, list accounts, scan
library, fetch title. Enable or disable each store under `[sources.<id>]`, or
via `BOOKCLERK_SOURCE_<ID>_ENABLED` for any source/plugin id (`<ID>` is the
uppercased plugin id — e.g. `BOOKCLERK_SOURCE_ECHO_ENABLED=0`).

| Id | Auth | Media path | Notes |
| --- | --- | --- | --- |
| `audible` | Amazon OAuth (QR / callback / external) | Adrm / Widevine; guest decrypt → Plain | Dual load; [audible-rs](https://github.com/mkb79/audible-rs) |
| `libro` | Email + password | DRM-free M4B/ZIP | Dual load; password: `BOOKCLERK_LIBRO_PASSWORD` |
| `chirp` | Email + password (GraphQL) | Plain MP3 after URL decrypt | Dual load; password: `BOOKCLERK_CHIRP_PASSWORD` |
| `graphicaudio` | Email + password | Plain web / ZIP / device stream | Dual load; password: `BOOKCLERK_GA_PASSWORD` |

First-party source binaries live under `crates/bookclerk-plugins/`. Workspace
builds also `register()` them in-process; distributed installs use the plugin
search path (`$BOOKCLERK_FILES_DIR/plugins` or `BOOKCLERK_PLUGIN_DIRS`). See
[plugins.md](plugins.md). External third-party source plugins use the same
config table shape.

## Common CLI

```bash
bookclerk auth login --source <id> …
bookclerk auth list
bookclerk auth status
bookclerk library scan --source <id>
bookclerk library acquire <title-id>
```

After scan, non-Audible rows can be enriched with an Audible ASIN via public
catalog search when `library.enrich_from_audible = true` (default). No Audible
account is required for enrichment. Tune `library.enrich_min_confidence`
(default `90`).

## Audible

```bash
bookclerk auth login -m us              # default --source audible
bookclerk auth login --external         # paste redirect
bookclerk auth login --force            # refresh / re-register (Android)
```

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
IdentityTokens are not converted. Re-authenticate with `auth login`, or import an
audible-rs auth file via `bookclerk auth import`.

Low-level auth/download notes: [`crates/bookclerk-plugins/optional/source-audible/README.md`](../crates/bookclerk-plugins/optional/source-audible/README.md).

### Example

```toml
[sources.audible]
enabled = true
bitrate = "high"          # high | normal

[output]
widevine = false
xhe_aac = false
format = "enriched_m4b"
```

## Libro.fm

```bash
export BOOKCLERK_LIBRO_PASSWORD='…'
bookclerk auth login --source libro --email you@example.com
bookclerk library scan --source libro
bookclerk library acquire --isbn 978…
```

```toml
[sources.libro]
enabled = true
container = "m4b"         # m4b | zip
```

Credentials are stored in `encrypted_secrets` (DB-backed).

## Chirp

```bash
export BOOKCLERK_CHIRP_PASSWORD='…'
bookclerk auth login --source chirp --email you@example.com
bookclerk library scan --source chirp
```

```toml
[sources.chirp]
enabled = true
```

Headless login uses GraphQL password `signIn` (not browser cookie sessions).
Some egress paths see Cloudflare challenges — use a normal browser User-Agent
context if probes fail. Research background: [source-candidates.md](source-candidates.md).

## GraphicAudio

Three access modes (pick one; default `web`):

| `access` | Behavior |
| --- | --- |
| `web` | Magento Browser Player + CloudFront cookies (no device slot) |
| `zip` | Magento “My Downloadable Products” ZIP (consumes download attempts) |
| `device` | Access App API Hi/Lo streams (uses a device slot / `client_id`) |

```bash
export BOOKCLERK_GA_PASSWORD='…'
bookclerk auth login --source graphicaudio --email you@example.com
bookclerk library scan --source graphicaudio
```

```toml
[sources.graphicaudio]
enabled = true
access = "web"            # web | zip | device
# bitrate = "hi"          # hi | lo (device)
# container = "auto"      # auto | m4b | mp3 | flac (zip)
```

Env override: `BOOKCLERK_GA_ACCESS` (legacy alias `BOOKCLERK_GA_FETCH`).
Disable with `enabled = false` or `BOOKCLERK_SOURCE_GRAPHICAUDIO_ENABLED=0`.

ZIP SKUs unlock App + Browser as well; App Access–only does **not** include a
computer ZIP. Prefer purchasing an M4B/MP3/FLAC ZIP SKU when validating all
paths. Full Magento/device notes: [source-candidates.md](source-candidates.md).

## Disabling a store

```toml
[sources.chirp]
enabled = false
```

Or `BOOKCLERK_SOURCE_CHIRP_ENABLED=0` (pattern: `BOOKCLERK_SOURCE_<ID>_ENABLED`).

## Future stores

Candidate research (Kobo, Storytel, Downpour, …) lives in
[source-candidates.md](source-candidates.md). New first-party adapters implement
`ContentSource`; third parties can ship external source plugins when the v1
protocol fits (plain fetch today; encrypted fetch remains first-party-only).
