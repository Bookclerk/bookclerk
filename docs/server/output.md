# Output and destinations reference

This page sets up where Bookclerk writes finished books: a folder, an S3
bucket, or both. It also covers file naming and formats. For a
plain-language overview, see [Where your books go](../guide/where-books-go.md).

## How destinations work

A **destination** is a plugin that stores finished audio and its sidecar
files.

- Each destination is a plugin instance, deployed per host.
- Several can be on at once. An acquire writes to every destination
  deployed on the host that runs it.
- A destination's settings are its plugin settings. Change them with
  `bookclerk plugins config …` or in **Settings → Plugins**; see
  [Managing plugins](plugins.md#enabling-and-settings-database).

| Destination | Package | Credentials | On by default |
| --- | --- | --- | --- |
| Local filesystem | `local` (platform) | none | Yes, on every host at enrollment |
| S3 / MinIO | `s3` (optional) | Sealed keys, or the AWS SDK chain | No, until `bookclerk plugins deploy s3` |

External plugins with a `storage` entrypoint load the same way.

## Local filesystem

The `local` plugin writes to a folder. It may write only to its `root`,
plus its own `plugin-state/<plugin-key-fs-id>/data` and `tmp` folders.

| Setting | Default | Notes |
| --- | --- | --- |
| `root` | `Audiobooks` | A relative path resolves under `BOOKCLERK_FILES_DIR` |
| `prefix` | `""` | Key prefix inside the root |
| `naming_profile`, `folder_template`, `file_template`, `chapter_file_template` | from `core.output` | Naming for this destination only |

To move the library folder:

```sh
bookclerk plugins config set local root /data/Audiobooks
bookclerk plugins config get local
```

## S3 and MinIO

The `s3` plugin writes to an S3 bucket or any S3-compatible service.

| Setting | Default | Notes |
| --- | --- | --- |
| `bucket` | none | Required |
| `region` | `us-east-1` | |
| `endpoint` | none | MinIO or another S3-compatible host; a bare hostname gets `https://` |
| `force_path_style` | `false` | |
| `prefix` | `library/` | |
| `naming_profile`, `folder_template`, `file_template`, `chapter_file_template` | from `core.output` | Naming for this destination only |
| `access_key_id`, `secret_access_key`, `session_token` | none | Secrets, sealed in the database |

### Set up a bucket

1. Name the bucket:

   ```sh
   bookclerk plugins config set s3 bucket my-audiobooks
   ```

2. For MinIO or another S3-compatible service, set its address and path
   style:

   ```sh
   bookclerk plugins config set s3 endpoint http://minio:9000
   bookclerk plugins config set s3 force_path_style true
   ```

3. Enter the keys. Each command reads the value from stdin:

   ```sh
   bookclerk plugins config set s3 access_key_id --secret
   bookclerk plugins config set s3 secret_access_key --secret
   ```

4. Deploy the plugin:

   ```sh
   bookclerk plugins deploy s3
   ```

### Credentials

When both keys are set, the plugin uses them. Otherwise it falls back to
the AWS SDK's **default provider chain** inside its jail: shared config
files, SSO, and EC2, ECS or EKS roles.

The `endpoint` setting is granted to the plugin as a network endpoint.
Nothing is read from `BOOKCLERK_S3_*` or `BOOKCLERK_AWS_*` variables.

### Matching existing files

Object user metadata, and the local `.bookclerk-meta.json` files, let
`--match-storage` match books without downloading them. S3 timestamps
follow `output.creation_time` and `output.last_write_time`.

A match leaves files where they are. To move matched audio and its
sidecars onto your naming layout instead, set
`library.fix_storage_layout = true`.

## Several destinations

When some destinations already have a title and others don't,
`output.multi_destination` decides what an acquire does:

| Policy | What happens |
| --- | --- |
| `sync_missing` (default) | Copies from a destination that has the title to those that don't, without downloading again |
| `refetch_missing` | Downloads and encodes again, and writes only to the destinations missing the title |
| `refetch_all` | Downloads and encodes again, and writes to every destination |

```toml
[output]
multi_destination = "sync_missing"
```

## Naming

Bookclerk names files with Libation's NamingTemplate engine
(`bookclerk-naming`). Pick a profile, and override it with templates if
you need to:

```toml
[output]
naming_profile = "audiobookshelf"
# folder_template = "<author>/<title>"
# file_template = "<title> [<asin>]"
```

| Profile | Layout |
| --- | --- |
| `audiobookshelf` (default) | Author, series and year folders that suit Audiobookshelf |
| `classic` | Libation's desktop defaults, such as `<title short> [<id>]` |

Each destination can override the naming too, with its own
`naming_profile`, `folder_template`, `file_template` and
`chapter_file_template` settings. That lets S3 keys differ from the
local folder tree.

To see the profiles and tags, and to preview a book's path:

```sh
bookclerk config template profiles
bookclerk config template tags
bookclerk config template preview <asin>
```

Replace `<asin>` with a book's ASIN.

**Path sanitization** cleans up characters a file system can't store. Set
`output.path_sanitization` to `auto`, `windows`, `posix`, `s3` or `none`,
or list your own `output.replacement_characters`.

## Formats and sidecars

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

> [!NOTE]
> These `[output]` settings live in the library database, as the
> `core.output` document. The TOML table only seeds it on the first
> enrollment. After that, change them with
> `bookclerk config set output.<key> <value>`, in **Settings → Output**,
> or with `PATCH /api/settings`.

Every host applies a new revision at its next configuration wakeup. See
[Configuration](configuration.md#output-shared).

If a sidecar file fails, Bookclerk logs it and still keeps the audio.

Audible's own options (Widevine, xHE-AAC, brand trim) are on the
[Stores](stores.md#audible) page.

## Next steps

- [Managing plugins](plugins.md): deploy, configure and update plugins.
- [Configuration](configuration.md): every shared setting.
- [Backups, updates, import and export](backups.md): keep a copy of the
  library database.
