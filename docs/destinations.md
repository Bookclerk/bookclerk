# Destinations (output)

Acquire writes finished audio (and sidecars) through **destination plugins**.
Each destination under `[output.<name>]` has an `enabled` flag. **Multiple may
be enabled at once** — Bookclerk writes to every enabled destination.

Built-in destinations today:

| Id | Config table | Credentials |
| --- | --- | --- |
| Local filesystem | `[output.local]` | none |
| S3 / MinIO | `[output.s3]` | `AWS_*` env override → `encrypted_secrets` → SDK chain |

External `kind = "output"` plugins are loaded when discovered. The first-party
S3 guest (`id = "s3"`) replaces the in-process backend when staged under
`plugins/s3/` and `[output.s3].enabled = true` ([plugins.md](plugins.md)).

## Local filesystem

```toml
[output.local]
enabled = true
root = "/data/Audiobooks"
# prefix = "library/"
```

Relative `root` values resolve under `BOOKCLERK_FILES_DIR`. Override with
`BOOKCLERK_OUTPUT_LOCAL_ROOT`.

## S3 / MinIO

```toml
[output.s3]
enabled = true
bucket = "my-audiobooks"
prefix = "library/"
region = "us-east-1"
# endpoint = "http://minio:9000"
# force_path_style = true
```

Credentials resolve in this order:

1. `BOOKCLERK_AWS_ACCESS_KEY_ID` + `BOOKCLERK_AWS_SECRET_ACCESS_KEY`
   (optional `BOOKCLERK_AWS_SESSION_TOKEN`) — process env override. Empty string
   counts as set (intentional override). These are not written to the DB unless
   you run `bookclerk config s3-credentials set`.
2. `encrypted_secrets` row `kind=s3`, `account_type=operator`, `account_id=default`, `name=default`
   (save with `bookclerk config s3-credentials set`; secrets are never accepted
   on argv). **Fail closed** if the row is present but cannot be unsealed —
   Bookclerk does not fall through to the SDK chain in that case.
3. AWS SDK **default provider chain** — shared config files (`~/.aws/credentials`,
   `~/.aws/config`), AWS SSO, and cloud identity (EC2/ECS/EKS roles, …).
   Installing the AWS CLI is not required.

`bookclerk config s3-credentials show|clear` inspects or removes the DB row.

Bucket/region/endpoint also accept `BOOKCLERK_OUTPUT_S3_*`
(or familiar `BOOKCLERK_S3_*`) env vars. Host-only endpoints are accepted;
`https://` is prepended when the value looks like a bare hostname. Whitespace-
only endpoint values are ignored.

Object user-metadata and local `.bookclerk-meta.json` support
`--match-storage` without downloading bodies. S3 timestamp metadata follows
`output.creation_time` / `output.last_write_time`.

## Multi-destination policy

When some destinations already have a title and others do not:

```toml
[output]
# sync_missing | refetch_missing | refetch_all
multi_destination = "sync_missing"
```

| Policy | Behavior |
| --- | --- |
| `sync_missing` (default) | Copy from a present destination into missing ones (no re-fetch) |
| `refetch_missing` | Re-download/encode; write only to missing destinations |
| `refetch_all` | Re-download/encode into every destination |

## Naming

Templates come from Libation’s NamingTemplate engine (`bookclerk-naming`).

```toml
[output]
naming_profile = "audiobookshelf"   # or "classic"
# folder_template / file_template override the profile when set
# folder_template = "<author>/<title>"
# file_template = "<title> [<asin>]"
```

| Profile | Intent |
| --- | --- |
| `audiobookshelf` (default) | Author / series / year-oriented layout friendly to ABS |
| `classic` | Libation desktop defaults (`<title short> [<id>]`, …) |

Per-destination overrides are allowed on `[output.local]` / `[output.s3]`
(`naming_profile`, `folder_template`, `file_template`,
`chapter_file_template`) so object keys can differ from the local tree.

CLI helpers:

```bash
bookclerk config template profiles
bookclerk config template tags
bookclerk config template preview <asin>
```

Path sanitization: `output.path_sanitization` (`auto` / `windows` / `posix` /
`s3` / `none`) or explicit `output.replacement_characters`.

## Packaging format & sidecars

```toml
[output]
format = "enriched_m4b"   # single_mp3 | split_mp3_by_chapter | …
download_cover = true
download_pdf = true
create_cue = true
fixup_metadata = true
save_chapter_json = true
cover_size = "500"        # 500 | 1215 | native
chapter_layout = "tree"   # tree | flat
```

Artifact (sidecar) failures are logged and do not fail the audio acquire.

Audible-specific knobs (Widevine, xHE-AAC, brand trim) are documented under
[sources.md](sources.md#audible).
