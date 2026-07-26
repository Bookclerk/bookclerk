# Configuration

Primary file: `$BOOKCLERK_FILES_DIR/config.toml` (or `--config` /
`BOOKCLERK_CONFIG`). Annotated template:
[`config/config.example.toml`](../config/config.example.toml).

Inspect resolved values:

```bash
bookclerk config show
bookclerk config get <key>
bookclerk config paths
```

Classic Libation setting names are accepted as aliases where documented in
[libation-parity.md](libation-parity.md). Runtime overrides on acquire:
`-o key=value` (repeatable).

## Top-level tables

| Table | Purpose |
| --- | --- |
| `[library]` | Auto-acquire, scan interval, enrichment, storage layout fix |
| `[auth]` | Token encryption password file / plaintext allow |
| `[output]` | Format, Widevine, naming, sidecars, multi-destination policy |
| `[output.local]` / `[output.s3]` | Destination plugins (`enabled`, roots, per-dest naming) |
| `[sources.<id>]` | Per-storefront enable + store knobs |
| `[daemon]` | Listen address, JSON logs |
| `[integrations]` / `[integrations.<id>]` | Portal + outbound integrations |
| `[diagnostics]` | Opt-in report sharing |

## Important environment variables

| Variable | Role |
| --- | --- |
| `BOOKCLERK_FILES_DIR` | State root (DB, Accounts, plugins, …) |
| `BOOKCLERK_CONFIG` | Config path override |
| `BOOKCLERK_DAEMON_LISTEN` | Control plane bind |
| `BOOKCLERK_LOG` / `RUST_LOG` | Log filter |
| `BOOKCLERK_AUTH_PASSWORD` | Audible auth-file passphrase |
| `BOOKCLERK_AUTH_PASSWORD_FILE` | Passphrase file (auto-created if missing) |
| `BOOKCLERK_AUTH_ALLOW_PLAINTEXT` | Allow unprotected Audible tokens |
| `BOOKCLERK_LIBRO_PASSWORD` | Libro.fm login |
| `BOOKCLERK_CHIRP_PASSWORD` | Chirp login |
| `BOOKCLERK_GA_PASSWORD` | GraphicAudio login |
| `BOOKCLERK_GA_ACCESS` | GraphicAudio access mode (`web`/`zip`/`device`) |
| `BOOKCLERK_ABS_API_KEY` | Audiobookshelf API key |
| `BOOKCLERK_OUTPUT_LOCAL_ROOT` | Local destination root |
| `BOOKCLERK_OUTPUT_S3_*` / `BOOKCLERK_S3_*` | S3 destination settings |
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` | S3 credentials (env-only) |
| `BOOKCLERK_SOURCE_<ID>_ENABLED` | Force-enable/disable any source/plugin id (`<ID>` uppercased; e.g. `BOOKCLERK_SOURCE_ECHO_ENABLED=0`) |
| `BOOKCLERK_PLUGIN_DIRS` | Extra plugin search roots (OS path list) |
| `BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL` | Diagnostics collector (build-time or runtime) |

## Library

```toml
[library]
auto_acquire = false
scan_interval_minutes = 60
enrich_from_audible = true
enrich_min_confidence = 90
# fix_storage_layout = false
```

## Output (shared)

```toml
[output]
format = "enriched_m4b"
widevine = false
xhe_aac = false
naming_profile = "audiobookshelf"
download_cover = true
download_pdf = true
fixup_metadata = true
multi_destination = "sync_missing"
```

Destination and naming detail: [destinations.md](destinations.md).
Store-specific knobs: [sources.md](sources.md).
Integrations: [integrations.md](integrations.md).
