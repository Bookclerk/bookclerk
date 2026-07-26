# Destinations (output)

Acquire writes finished audio (and sidecars) through **destination plugins**.
Each destination under `[output.<name>]` has an `enabled` flag. **Multiple may
be enabled at once** — Bookclerk writes to every enabled destination.

Built-in destinations today:

| Id | Config table | Credentials |
| --- | --- | --- |
| Local filesystem | `[output.local]` | none |
| S3 / MinIO | `[output.s3]` | `Accounts/*.s3.auth`, AWS env override, or SDK/CLI shared chain |

External `kind = "output"` plugins are discovered; loading is not implemented
yet ([plugins.md](plugins.md)).

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
# credentials_file = "Accounts/default.s3.auth"   # default when unset
```

Credentials use the same `Accounts/*.*.auth` pattern as storefront sources.
Default path when `credentials_file` is unset: `Accounts/default.s3.auth`.

```json
{
  "access_key_id": "…",
  "secret_access_key": "…",
  "session_token": null,
  "label": "minio"
}
```

Resolution order:

1. `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (optional `AWS_SESSION_TOKEN`)
2. `credentials_file` / default `Accounts/default.s3.auth`
3. AWS SDK **default provider chain** — the same lookup the AWS CLI / official
   SDKs use when no static keys are set. That includes shared config files
   written by `aws configure` (`~/.aws/credentials`, `~/.aws/config`), AWS SSO
   profiles, and cloud identity (EC2 instance profile, ECS task role, EKS
   IRSA, …). Installing the AWS CLI is not required; Bookclerk only reads the
   shared files / ambient role credentials the SDK discovers.

Bucket/region/endpoint/credentials path also accept `BOOKCLERK_OUTPUT_S3_*`
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
