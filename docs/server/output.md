# Output and destinations reference

For choosing where your books go in plain language, see [Where your books go](../guide/where-books-go.md).

Acquire writes finished audio (and sidecars) through **destination plugins**.
Each destination is a plugin instance with a deployment per host; **several
may be enabled at once** and Bookclerk writes to every destination deployed on
the host running the job. Per-destination settings are the plugin's
`[[settings]]`, stored in the instance document
(`bookclerk plugins config …` or **Settings → Plugins**); see
[plugins.md](../plugins/README.md#enabling-and-settings-database).

| Id | Package | Credentials | Default load |
| --- | --- | --- | --- |
| Local filesystem | `local` (platform) | none | Enabled on every host at enrollment (platform grant) |
| S3 / MinIO | `s3` (optional) | `access_key_id` / `secret_access_key` secrets → AWS SDK chain | `absent` until `bookclerk plugins deploy s3` |

External `storage` entrypoint plugins are loaded the same way. The local guest
is granted write access only to its `root` (plus its private
`plugin-state/<plugin-key-fs-id>/data` and `tmp`).

## Local filesystem

| Setting | Default | Notes |
| --- | --- | --- |
| `root` | `Audiobooks` | Relative paths resolve under `BOOKCLERK_FILES_DIR` |
| `prefix` | `""` | Key prefix inside the root |
| `naming_profile`, `folder_template`, `file_template`, `chapter_file_template` | inherit `core.output` | Per-destination naming overrides |

```bash
bookclerk plugins config set local root /data/Audiobooks
bookclerk plugins config get local
```

## S3 / MinIO

| Setting | Default | Notes |
| --- | --- | --- |
| `bucket` | — | required |
| `region` | `us-east-1` | |
| `endpoint` | — | MinIO or other S3-compatible host; a bare hostname gets `https://` |
| `force_path_style` | `false` | |
| `prefix` | `library/` | |
| `naming_profile`, `folder_template`, `file_template`, `chapter_file_template` | inherit `core.output` | |
| `access_key_id`, `secret_access_key`, `session_token` | — | `secret`; sealed in the database |

```bash
bookclerk plugins config set s3 bucket my-audiobooks
bookclerk plugins config set s3 endpoint http://minio:9000
bookclerk plugins config set s3 force_path_style true
bookclerk plugins config set s3 access_key_id --secret       # value on stdin
bookclerk plugins config set s3 secret_access_key --secret
bookclerk plugins deploy s3
```

Credentials come from the instance secrets when both keys are set. When they
are not, the guest falls back to the AWS SDK **default provider chain**
(shared config files, SSO, EC2/ECS/EKS roles) inside its jail. The
`endpoint` the document names is granted to the guest as a deployed network
endpoint; nothing is read from `BOOKCLERK_S3_*` or `BOOKCLERK_AWS_*`.

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

Per-destination overrides are the `naming_profile`, `folder_template`,
`file_template`, and `chapter_file_template` settings of the `local` / `s3`
instance, so object keys can differ from the local tree.

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

The shared `[output]` settings above are the `core.output` cluster document
in the library database. The TOML table only seeds it on first enrollment;
afterwards change them with `bookclerk config set output.<key> <value>`,
**Settings → Output**, or `PATCH /api/settings` (see
[configuration.md](configuration.md#output-shared)). Every host applies a
committed revision on its next configuration wakeup.

Artifact (sidecar) failures are logged and do not fail the audio acquire.

Audible-specific knobs (Widevine, xHE-AAC, brand trim) are documented under
[stores.md](stores.md#audible).
