# Output and destinations reference

This page sets up where Bookclerk writes finished books: a folder, an S3
bucket, or both. It also covers file naming and formats. For a
plain-language overview, see [Where your books go](../guide/where-books-go.md).

## How destinations work

A **destination** is a plugin that stores finished audio and its sidecar
files.

- Each destination is a plugin instance, deployed per host.
- Several can be on at once. An acquire writes to every destination
  deployed on the host that runs it whose filter takes the book (see
  [Choosing which books a destination takes](#choosing-which-books-a-destination-takes)).
- A destination's settings are its plugin settings. Change them with
  `bookclerk plugins config …` or in **Settings → Plugins**; see
  [Managing plugins](plugins.md#enabling-and-settings-database).

| Destination | Package | Credentials | On by default |
| --- | --- | --- | --- |
| Local filesystem | `local` (platform) | none | Yes, on every host at enrollment |
| S3 / MinIO | `s3` (optional) | Sealed keys, or the AWS SDK chain | No, until `bookclerk plugins deploy s3` |

Any other plugin with a `storage` entrypoint is a destination too, and an
acquire writes to it like the built-in ones:

- Each deployed plugin instance is its own destination, named by its
  plugin instance id. That id is what upload records and `edition_files`
  keep, so two instances of one plugin (two buckets, say) are two
  destinations, and an acquire writes to both. Add one with
  `bookclerk plugins instances create s3 --name "Backup bucket"`, then
  deploy it ([plugin instances](plugins.md#instances)).
- Destinations that name files as paths come first and the rest follow by
  instance id. The first one is the primary destination: the library
  records its key as the title's storage key.
- `local` names files as paths on a file system. Every other destination
  gets object-store keys: one key of at most 1024 bytes, with the
  object-store character set (`output.path_sanitization = "auto"`).

## Local filesystem

The `local` plugin writes to a folder. It may write only to its `root`,
plus its own `plugin-state/<plugin-key-fs-id>/data` and `tmp` folders.

| Setting | Default | Notes |
| --- | --- | --- |
| `root` | `Audiobooks` | A relative path resolves under `BOOKCLERK_FILES_DIR` |
| `prefix` | `""` | Key prefix inside the root |
| `watch` | `false` | Notice files added, changed or removed outside Bookclerk right away (see [Watching the folder](#watching-the-folder)) |
| `naming_profile`, `folder_template`, `file_template`, `chapter_file_template` | from `core.output` | Naming for this destination only |

To move the library folder:

```sh
bookclerk plugins config set local root /data/Audiobooks
bookclerk plugins config get local
```

Bookclerk moves, renames and checks files in this folder directly. How it
does so depends on the file system each file is on, since a folder inside
the library can be another mount.

On a local disk:

- A move is a rename, so a reader never sees a half-moved file. The
  `.bookclerk-meta.json` sidecar moves with its file.
- A move that must not replace a file links it to the new name first,
  which fails if a file is already there. On a file system without hard
  links (FAT or exFAT, for example) it checks for the file just before the
  rename instead.
- A write that must not replace a file uses the same link step. A write
  that must replace only the file Bookclerk last read checks it just before
  the rename. Bookclerk's own writes cannot slip into that moment, but a
  program outside Bookclerk writing the same file at that instant can.

On a network share (NFS, SMB, AFP, WebDAV, a Windows network drive) or a
FUSE mount (sshfs, rclone, mergerfs and the like):

- Bookclerk never uses hard links. A retried NFS request can report that a
  link failed after it succeeded, and many of these file systems fake hard
  links or have none. Writes and moves that must not replace a file check
  just before the rename instead, and report that they were not atomic.
- A file's change signature is its size and modification time only.
  Inode numbers and change times can shift after a remount without the file
  changing.

Between two file systems (a move into a folder that is another mount),
Bookclerk copies the file, syncs it to disk, checks its size, and only then
deletes the original.

### Watching the folder

With `watch` on, the plugin asks the operating system to tell it whenever a
file in the folder is added, changed, renamed or removed, and keeps a list
of those changes. Bookclerk reads the list instead of walking the whole
folder, so changes made by other programs show up within seconds.

```sh
bookclerk plugins config set local watch true
```

Things to know:

- **It keeps the plugin running.** Plugins normally stop after a few idle
  minutes. A watching plugin stays up while its watch works, because the
  list lives in its memory. That costs a few megabytes (see
  [the record](../adr/event-delivery-model.md#decision)).
- **Local disks only.** On a network share or a FUSE mount the operating
  system is not told about changes made by other machines, so the plugin
  does not watch there, and Bookclerk keeps finding changes by listing.
- **Listing stays the backstop.** When the watch cannot vouch for what it
  saw (the plugin restarted, the operating system dropped events, more than
  16,384 changes piled up unread, or a whole folder was moved), Bookclerk
  lists the folder once and carries on from there.
- On Linux each watched folder uses one inotify watch. A library with more
  folders than `fs.inotify.max_user_watches` allows stops the watch, and
  Bookclerk falls back to listing.

Change signatures need a modification time that moves with every write.
FAT keeps it to 2 seconds and exFAT to 10 milliseconds, so two writes of
the same size within that window can look alike.

## S3 and MinIO

The `s3` plugin writes to an S3 bucket or any S3-compatible service.

| Setting | Default | Notes |
| --- | --- | --- |
| `bucket` | none | Required |
| `region` | `us-east-1` | |
| `endpoint` | none | MinIO or another S3-compatible host; a bare hostname gets `https://` |
| `force_path_style` | `false` | |
| `prefix` | `library/` | |
| `notification_queue_url` | none | Amazon SQS queue that receives the bucket's object events (see [Following changes in the bucket](#following-changes-in-the-bucket)) |
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

### Moves and conditional writes

S3 has no rename, so a move copies the object inside the bucket and then
deletes the original. Some S3-compatible services accept the `If-None-Match`
and `If-Match` headers but ignore them, sometimes on only some kinds of
request. So the first time the plugin needs them it checks the bucket once,
with scratch objects under `.bookclerk-probe/`: it tries a plain upload, a
multipart upload, a copy, a copy in parts and a delete that should each be
refused, and then removes the scratch objects. Each kind of request is trusted only if
the bucket refused it. The plugin remembers the answer until it restarts.

When the bucket enforces the headers on every request an operation sends:

- A write that must not replace an object, or must replace only the object
  Bookclerk last read, sends the condition with the upload, and S3 refuses
  it if another writer got there first.
- A move copies only while the original is unchanged, and with
  `noOverwrite` only while nothing is at the new name. It deletes the
  original only while that is still the object it copied; if something
  rewrote it meanwhile, the original is kept and the move reports a
  conflict.

When it does not, Bookclerk checks each condition just before the write or
copy instead, and reports the result as not atomic. A writer outside
Bookclerk can slip into that moment.

Listings carry each object's ETag and modification time, so Bookclerk can
tell which objects changed without reading them.

### Following changes in the bucket

Bookclerk finds files that other programs add, change or remove by listing
the bucket on a schedule. On Amazon S3 it can also follow the bucket's
event notifications, so it only reads what changed.

1. Create an Amazon SQS standard queue for this bucket, and let the bucket
   send to it (the queue's access policy allows `s3.amazonaws.com` to call
   `sqs:SendMessage` for the bucket).
2. Send the bucket's `s3:ObjectCreated:*` and `s3:ObjectRemoved:*` events
   to the queue in one of these ways:
   - An event notification on the bucket that targets the queue.
   - An event notification that targets an SNS topic the queue subscribes
     to. The topic's access policy allows `s3.amazonaws.com` to call
     `sns:Publish`, and the queue's policy allows `sns.amazonaws.com` to
     call `sqs:SendMessage` for the topic.
   - EventBridge: turn on **Send notifications to Amazon EventBridge** in
     the bucket's event notification settings, then add a rule for the
     bucket's `Object Created` and `Object Deleted` events that targets the
     queue. The queue's policy allows `events.amazonaws.com` to call
     `sqs:SendMessage` for the rule.
3. Let the plugin's credentials call `sqs:ReceiveMessage`,
   `sqs:DeleteMessage` and `sqs:ChangeMessageVisibility` on the queue.
4. Give the plugin the queue's URL:

   ```sh
   bookclerk plugins config set s3 notification_queue_url \
     https://sqs.us-east-1.amazonaws.com/123456789012/my-audiobooks-events
   ```

Things to know:

- **One reader per queue.** Each message goes to whoever receives it, so
  nothing else may read the queue, and only one destination on one node
  should name it. A queue can carry events for several buckets; the plugin
  skips other buckets and keys outside its `prefix`. Its receipts are kept
  per queue, bucket and prefix, so changing either setting starts over
  with one listing.
- **Messages wait for Bookclerk.** The plugin deletes a message only after
  Bookclerk has taken the changes in it. Until then the message stays
  hidden for 12 hours, after which the queue offers it again. It keeps the
  receipts it needs to delete messages under its `HOME`, so they survive
  the plugin sleeping between checks.
- **Listing stays the backstop.** After the queue is first set, after the
  plugin loses its receipts, when a message cannot be read, or when the
  plugin could not hand a message back to the queue (or stopped in the
  middle of receiving), Bookclerk
  lists the bucket once and carries on from there. Messages that the queue
  drops (its retention ran out, or a dead-letter queue took them) are found
  by the next scheduled listing.
- Only Amazon SQS queues are read for now. MinIO and other S3-compatible
  services deliver their events elsewhere, so Bookclerk lists those buckets.

### Matching existing files

Object user metadata, and the local `.bookclerk-meta.json` files, let
`--match-storage` match books without downloading them. S3 timestamps
follow `output.creation_time` and `output.last_write_time`.

A match leaves files where they are. To move matched audio and its
sidecars onto your naming layout instead, set
`library.fix_storage_layout = true`.

Matching works per edition. When two accounts own the same book, its file
is matched once and both accounts see it as acquired. When the file is
gone, the account whose acquire recorded it marks the book not acquired.
`bookclerk library convert` also converts each edition once, through that
account.

## Choosing which books a destination takes

Every destination takes every book until you give it a filter. A filter
is a list of rules, each a field, one value and include or exclude:

| Field | Value |
| --- | --- |
| `patron` | A user id: books held by a store account linked to that user |
| `storefront` | A storefront plugin's alias, such as `audible` or `libro` (case does not matter) |
| `account` | A store account's id at its store |
| `media_kind` | `book` or `podcast` (a podcast episode is a podcast) |

With no include rule, the destination takes every book the exclude rules
allow. With any include rule, it takes only books that match at least one.
An exclude rule always wins.

1. Find the destination's instance id with
   `bookclerk plugins instances list`.
2. Set its filter, for example to keep podcasts out of a bucket:

   ```sh
   bookclerk plugins instances filter set <instance> --exclude media_kind=podcast
   ```

3. Check it with `bookclerk plugins instances filter get <instance>`, or
   remove it with `bookclerk plugins instances filter clear <instance>`.

The web API reads and writes the same filter at
`/api/config/plugin-instances/{id}/filter`.

Before an acquire of a book starts, Bookclerk works out which destinations
take it. The book is written to those destinations only. When none takes
it, the acquire of that book fails at once with "no destination accepts
this book", before anything is downloaded.

> [!NOTE]
> A changed filter applies to acquires that start afterwards. Files that
> are already stored stay where they are. An edition that is already
> stored on one destination is not acquired again for another destination
> that takes it; that changes when each destination's copy is tracked.

> [!NOTE]
> For now filters apply to acquires that run on the library itself. A
> branch that acquires for the library still writes to every destination
> the library names; that changes in a later release.

## Several destinations

When some destinations already have a title and others don't,
`output.multi_destination` decides what an acquire does:

| Policy | What happens |
| --- | --- |
| `sync_missing` (default) | Copies from a destination that has the title to those that don't, without downloading again. This happens while the title is acquired; a title that is already stored is not copied to a destination whose filter now takes it |
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
# folder_template = "<first author>/<title short>"
# file_template = "<title> [<library id>]"
```

| Profile | Layout |
| --- | --- |
| `audiobookshelf` (default) | `Author/Series/1 - Title/Title [library id].m4b`, or `Author/Title/Title [library id].m4b` outside a series, which Audiobookshelf scans as author, series and book |
| `classic` | Libation's desktop defaults with the library id added: `<title short> [<id>] [<library id>]` |

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

### The library id

Every template set must include `<library id>`: in the folder template,
or in both the file and the chapter file templates. The tag must be
written for every book, so a `<library id>` inside any condition
(`<has series->…<-has>`, or `<!has series->…<-has>`) doesn't count. It is a short id for the
edition, such as `1a2b3c4d`, so two editions of a book (an abridged and an
unabridged recording, two narrators' readings) never share a path, while
every store's copy of the same edition does.

The id is eight lower-case letters and digits (Crockford base32, so never
`i`, `l`, `o` or `u`). It is worked out from the store listing that first
brought the edition into the library: its store, marketplace and product
id. Two libraries, or one library rebuilt from a fresh scan, that see the
same listing first give the edition the same id, so its files keep their
names.

The `audiobookshelf` profile puts the id in the file names rather than the
folder, because Audiobookshelf reads a bracketed value in a folder name as
an ASIN. When a book is saved as its separate source tracks, each track's
name carries the id with its track number, such as `hd4j63pc-p002`, so two
tracks with the same title never share a name.

Bookclerk refuses to save naming settings without it. A destination's own
naming overrides are checked when a book is acquired: an acquire whose
templates lack the tag stops before downloading anything and names the
destination to fix. The store product id (`<id>`, `<asin>`) doesn't count,
because the same edition bought from two stores has two product ids, and
not every book has an ISBN or ASIN.

Files you already have keep their names: storage matching marks them
acquired where they are, whether or not their path has a library id.

Copies of one edition on several accounts (the same book bought twice)
share their files. The edition is downloaded once, and every account's
copy is marked acquired at its path once each destination holds it.

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

With `fixup_metadata` on, an M4B also records which Bookclerk version
tagged it, in the freeform item `----:app.bookclerk:TOOL`.

Audible's own options (Widevine, xHE-AAC, brand trim) are on the
[Stores](stores.md#audible) page.

## Next steps

- [Managing plugins](plugins.md): deploy, configure and update plugins.
- [Configuration](configuration.md): every shared setting.
- [Backups, updates, import and export](backups.md): keep a copy of the
  library database.
