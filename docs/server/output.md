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
| Cloudflare R2 | `r2` (optional) | Sealed R2 keys | No, until `bookclerk plugins deploy r2` |
| Backblaze B2 | `b2` (optional) | A sealed B2 application key | No, until `bookclerk plugins deploy b2` |

Any other plugin with a `storage` entrypoint is a destination too, and an
acquire writes to it like the built-in ones. The exception is a storage
instance made a [drop folder](drop-folders.md): Bookclerk reads books from
it and never writes acquired books into it.

- Each deployed plugin instance is its own destination, named by its
  plugin instance id. That id is what upload records and `edition_files`
  keep, so two instances of one plugin (two buckets, say) are two
  destinations, and an acquire writes to both. Add one with
  `bookclerk plugins instances create s3 --name "Backup bucket"`, then
  deploy it ([plugin instances](plugins.md#instances)).
- Destinations are ordered by instance id. The first one is the primary
  destination: the library records its key as the title's storage key.
- Every destination receives the same name, and stores it under the key
  it can hold (see [Keys each destination stores](#keys-each-destination-stores)).

### Keys each destination stores

The naming templates render one **host key** for a file: folder names
joined by `/`, each one non-empty, never `.` or `..`, with no `\` or
control characters, at most 1024 bytes in all. The host key doesn't change
between destinations. Each destination's plugin then turns it into the key
it actually stores, and that key is what Bookclerk writes, checks for and
records for that destination:

- `local` stores a path that the host's file system accepts. On Windows it
  replaces `< > : " | ? *`, trims trailing dots and spaces, and prefixes
  reserved device names such as `CON` with `_`. Every platform keeps each
  folder or file name within 255 bytes (255 UTF-16 units on Windows),
  shortening the name before its extension.
- `s3` stores an object key without the characters S3 advises against,
  such as `\`, `{`, `}`, `^`, `%` and `|`, and fits the whole key, its prefix
  included, into 1024 bytes. It shortens the file name before its
  extension first, then the longest folder names.
- `r2` and `b2` follow the same object-key rules as `s3`.
- A third-party destination translates keys when it declares
  `storage.translateKeys` ([plugin ABI](../plugins/abi.md#translating-keys)).
  One that doesn't stores the host key as written.

So the same book can sit at `Ann Author/Dune: Messiah.m4b` in a bucket and
at `Ann Author/Dune_ Messiah.m4b` on a Windows disk.

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
folder, so changes made by other programs show up within minutes (see
[Noticing changes made outside Bookclerk](#noticing-changes-made-outside-bookclerk)).

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
`bookclerk library convert` also adds a copy to each edition once, through
that account.

## Cloudflare R2

The `r2` plugin writes to a Cloudflare R2 bucket through R2's S3 API. It
stores, moves and checks objects the way the `s3` plugin does (see
[Moves and conditional writes](#moves-and-conditional-writes)), and follows
the bucket's changes through a Cloudflare Queue instead of Amazon SQS.

| Setting | Default | Notes |
| --- | --- | --- |
| `account_id` | none | Required. The Cloudflare account that owns the bucket (32 hex digits) |
| `jurisdiction` | `default` | `default`, `eu` or `fedramp`: where the bucket was created, which picks its endpoint |
| `bucket` | none | Required |
| `prefix` | `library/` | |
| `notification_queue_id` | none | Id of the Cloudflare Queue that receives the bucket's events (see [Following changes in an R2 bucket](#following-changes-in-an-r2-bucket)) |
| `naming_profile`, `folder_template`, `file_template`, `chapter_file_template` | from `core.output` | Naming for this destination only |
| `access_key_id`, `secret_access_key` | none | Required secrets: an R2 API token's S3 keys |
| `queue_api_token` | none | Secret: a Cloudflare API token, needed only with a queue |

### Set up an R2 bucket with the guided setup

The guided setup creates the bucket (or uses yours, if you already own
one of that name) and an API token that can read and write that one
bucket's files and nothing else. It fills in the account, bucket and keys.

1. In the Cloudflare dashboard, create an account API token with
   **Workers R2 Storage: Edit** and **Account API Tokens: Edit**. The
   setup uses it once and does not store it; you can delete it afterwards.
2. In **Settings**, open the R2 instance, choose a bucket name (and a
   jurisdiction, if you need one), paste the token and select **Run guided
   setup**. Leave the account ID empty when the token can see only one
   account.
3. Approve the plugin's network access and deploy it, as in steps 4 and 5
   below.

Running the setup again creates a fresh bucket token and deletes the old
one. Before you delete the instance, **Remove what setup created** deletes
the bucket token; it needs a token like the one in step 1. It never deletes
the bucket or your books.

> [!NOTE]
> The guided setup runs under the plugin's normal network grant: it calls
> `api.cloudflare.com`, which the plugin already lists for the queue.

### Set up an R2 bucket by hand

1. In the Cloudflare dashboard, create an R2 API token with **Object Read
   & Write** on the bucket, and note its access key ID and secret access
   key.
2. Set the account and bucket:

   ```sh
   bookclerk plugins config set r2 account_id 3f4b7e3dcab231cbfdaa90a6a28bd548
   bookclerk plugins config set r2 bucket my-audiobooks
   ```

   For a bucket created in a jurisdiction, also set
   `bookclerk plugins config set r2 jurisdiction eu`.

3. Enter the keys. Each command reads the value from stdin:

   ```sh
   bookclerk plugins config set r2 access_key_id --secret
   bookclerk plugins config set r2 secret_access_key --secret
   ```

4. Approve the plugin's network access when you enable it. It reaches
   `*.r2.cloudflarestorage.com` for the bucket and `api.cloudflare.com` for
   the queue, both on port 443.
5. Deploy the plugin:

   ```sh
   bookclerk plugins deploy r2
   ```

### Following changes in an R2 bucket

Without a queue, Bookclerk lists the bucket on a schedule to find files
that other programs add, change or remove. With one, it reads only what
changed.

1. Create a Cloudflare Queue, and turn on **HTTP pull** as its consumer.
2. Add an event notification rule to the bucket that sends object-create
   and object-delete events to the queue. A rule can be limited to the
   destination's `prefix`.
3. Create a Cloudflare API token with **Queues Read** and **Queues Write**
   on the account, and enter it:

   ```sh
   bookclerk plugins config set r2 queue_api_token --secret
   ```

4. Give the plugin the queue's id (the 32-hex-digit id on the queue's
   page, not its name):

   ```sh
   bookclerk plugins config set r2 notification_queue_id 0123456789abcdef0123456789abcdef
   ```

The queue works like an SQS queue for the `s3` plugin (see
[Following changes in the bucket](#following-changes-in-the-bucket)):

- **One queue per destination.** Nothing else may consume it, and only
  one destination on one node should name it. Events for other accounts,
  buckets, or keys outside the `prefix` are skipped and acknowledged, so
  they are gone for any other reader. Limit the bucket's rule to the
  destination's `prefix` rather than sharing a queue.
- **Messages wait for Bookclerk.** The plugin acknowledges a message only
  after Bookclerk has taken the changes in it. Until then it stays leased
  for 12 hours, after which the queue offers it again. The plugin keeps
  its lease ids under its `HOME`.
- **Listing stays the backstop.** After the queue is first set, after the
  plugin loses its lease ids, or when a message cannot be read, Bookclerk
  lists the bucket once and carries on from there. Messages the queue
  drops when its retention runs out are found by the next scheduled
  listing.

## Backblaze B2

The `b2` plugin writes to a Backblaze B2 bucket through B2's native API.

| Setting | Default | Notes |
| --- | --- | --- |
| `bucket` | none | Required. The bucket's name |
| `prefix` | `library/` | |
| `naming_profile`, `folder_template`, `file_template`, `chapter_file_template` | from `core.output` | Naming for this destination only |
| `application_key_id`, `application_key` | none | Required secrets: a B2 application key |
| `webhook_signing_secret` | none | Secret: turns on the change feed (see [Following changes in a B2 bucket](#following-changes-in-a-b2-bucket)) |

It works like the other object stores, with these differences:

- **Large files.** A file over 8 MiB goes up in parts, with at most
  three parts' worth of memory held by the B2 code (two read ahead and
  the part being sent). When the plugin's requests go through the host's
  network proxy, the proxy step holds one more copy of the part in
  flight, so plan for up to four parts. An upload that fails part way is cancelled. If the
  cancel fails too, the unfinished file stays in the bucket (and is
  billed) until a later delete of that name or a lifecycle rule removes
  it.
- **Deletes remove every version.** B2 keeps old versions of a file.
  When Bookclerk deletes a file it deletes all of its versions, oldest
  first, so none stay billed behind a hide marker and a delete that stops
  part way never brings back an older version. A name with more than
  10,000 versions is refused instead of half deleted.
- **Metadata fits the strictest bucket.** B2 limits a file's name and
  stored metadata together (2,048 bytes on a bucket with encryption or
  Object Lock). The checksum and commit token are always stored; the
  ASIN, file times and title follow only while they fit, so a very long
  title is left out of the metadata rather than failing the upload.
- **Checked writes.** B2 cannot write a file only if it is new or
  unchanged. For such a write the plugin writes first, then reads the
  name's versions, and withdraws its own version when another program's
  write got in first; the write then fails as a conflict. Of two writers
  racing, the one B2 lists first stands. A reader can see the withdrawn
  version for that moment, so Bookclerk treats these writes as not
  atomic. Moves are a copy and then a delete.

### Set up a B2 bucket

1. In the Backblaze web app, create an application key with **Read and
   Write** access to the bucket. Note its keyID and its application key,
   which is shown only once. A key limited to a file-name prefix works
   when the destination's `prefix` is inside it.
2. Set the bucket:

   ```sh
   bookclerk plugins config set b2 bucket my-audiobooks
   ```

3. Enter the key. Each command reads the value from stdin:

   ```sh
   bookclerk plugins config set b2 application_key_id --secret
   bookclerk plugins config set b2 application_key --secret
   ```

4. Approve the plugin's network access when you enable it. It reaches
   `*.backblazeb2.com` for the API and downloads and `*.backblaze.com` for
   uploads, both on port 443.
5. Deploy the plugin:

   ```sh
   bookclerk plugins deploy b2
   ```

The plugin signs in to B2 on its first call, not when it starts. A wrong
key or bucket shows up as an error on the first write or listing.

### Following changes in a B2 bucket

Without a change feed, Bookclerk lists the bucket on a schedule to find
files that other programs add, change or remove. With one, B2 tells
Bookclerk what changed through
[event notifications](https://www.backblaze.com/docs/cloud-storage-event-notifications)
posted to the library's webhook.

The feed needs:

- A library that outside services can reach: `integrations.public_origin`
  set to an `https` address on the Internet, and `plugins.webhooks` left
  at `auto` (see [Inbound webhooks](../plugins/abi.md#inbound-webhooks-webhooks)).
  The node that origin reaches must run the destination.
- An application key that may also manage the bucket's notifications
  (`readBucketNotifications` and `writeBucketNotifications`). Event
  notifications may need turning on for the account by Backblaze first.

To turn it on, pick 32 random letters and digits as the signing secret,
and enter them. The command reads the value from stdin:

```sh
bookclerk plugins config set b2 webhook_signing_secret --secret
```

On its next check the plugin adds three event notification rules to the
bucket, named `bookclerk-<16 hex digits>-created`, `-deleted` and
`-hidden`, for the destination's `prefix`. They send every object event
to the webhook URL, signed with the secret, and Bookclerk refuses any
request whose signature does not match. The bucket's other rules are
kept as they are.

- **Rules follow the settings.** The plugin checks the rules every hour
  and rewrites them when they are missing, suspended by B2, or point at
  an old URL or secret. A bucket holds at most 25 rules, and B2 refuses
  two rules for the same kind of event whose prefixes overlap (such as
  `library/` and `library/new/`, or any prefix and an empty one). When the
  three don't fit, or B2 refuses them, Bookclerk keeps listing.
- **Turning it off removes them.** Unset the secret, or let the library
  lose its public address, and the plugin removes the rules it added on
  its next check.
- **Listing stays the backstop.** After the rules are first added or
  rewritten, or when the host dropped requests it could not keep,
  Bookclerk lists the bucket once and carries on from there. B2 sends
  each event at least once but gives up on a request after a few
  seconds, so an event lost while the library was unreachable is found
  by the next scheduled listing.
- **Test events fail.** The **Test rule** button in the Backblaze web app
  may sign its event with another key, so Bookclerk refuses it with 401.

## Choosing which books a destination takes

Every destination takes every book until you give it a filter. A filter
is a list of rules, each a field, one value and include or exclude:

| Field | Value |
| --- | --- |
| `patron` | A user id: books held by a store account linked to that user |
| `storefront` | A storefront plugin's alias, such as `audible` or `libro` (case does not matter) |
| `account` | A store account's id at its store |
| `media_kind` | `book` or `podcast` (a podcast episode is a podcast) |
| `format` | A copy's format: `m4b`, `mp3`, `mp3_chapters` or `original` (see [Keeping several copies](#keeping-several-copies); case does not matter) |
| `quality` | A copy's quality, such as `original`, `64k-mono` or `v2` (case does not matter) |

With no include rule, the destination takes every book the exclude rules
allow. With any include rule, it takes only books that match at least one.
An exclude rule always wins.

`format` and `quality` rules choose copies, not books. The other rules
decide whether a destination takes a book; then the `format` and
`quality` rules decide which of the library's copies of it the
destination receives, by the same include and exclude logic among
themselves. A destination with no `format` or `quality` rule receives
every copy.

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
> are already stored stay where they are. A destination whose filter now
> takes an edition it has no copy of gets one from the next acquire run
> (see [Several destinations](#several-destinations)).

The same holds when a branch acquires a book for the library: the
library works out the destinations that take each book and the branch
uploads it to those only.

## Keeping several copies

A library can keep more than one copy of every book: the main output
(`output.format`), plus up to eight more in other formats or at other
qualities. A small MP3 for an old player beside the M4B is the usual
case. List the extra copies in `output.copies`, each written
`format:quality`:

```sh
bookclerk config set output.copies "mp3:64k-mono,mp3_chapters:v4"
```

| Format | Output | Qualities |
| --- | --- | --- |
| `m4b` | One M4B with tags and chapters (`enriched_m4b`) | `original` |
| `mp3` | One MP3 (`single_mp3`) | A bitrate or a VBR level |
| `mp3_chapters` | One MP3 per chapter (`split_mp3_by_chapter`) | A bitrate or a VBR level |
| `opus` | One Ogg Opus file with tags and chapters (`opus`) | An Opus bitrate |
| `original` | The store's files as delivered (`none`) | `original` |

A quality is `original` (the store's audio, repackaged without
re-encoding), a constant bitrate the MP3 encoder writes (`64k`, `96k`,
`128k`, `160k`, `192k`, `256k` or `320k`), or a LAME VBR level from `v0`
(best) to `v9`. An Opus copy takes an Opus bitrate instead: `16k`,
`24k`, `32k`, `48k`, `64k`, `96k` or `128k`, which the encoder targets
in variable bitrate. A bitrate or VBR level may end in `-mono` to mix the
copy down to one channel; without it, a copy keeps the source's channels,
up to stereo. Opus is an advanced format: fewer players open it than M4B
and MP3.
The main output is a copy too: an M4B or as-delivered output is
`original`, an Opus output is `64k`, and an MP3 output takes its quality
from the `output.lame` settings, named after the bitrate the encoder
writes (it rounds `lame.bitrate_kbps` up to the next of those rates). A
copy may not repeat another or the main output.

Each acquire downloads the book once and makes every copy from that
download, main output first. When a library keeps more than one copy,
every MP3 or Opus copy is encoded at its own quality, even when the
store delivers that format, so each copy holds the audio its name says. A
library with only the main output keeps storing a store's MP3 or Opus
file as delivered, rather than encoding lossy audio a second time. A
book matched to Audible is encoded again only when Audible's chapter list
is laid onto it; when Audible has no chapters for it, the file is kept as
delivered too. Every destination receives each copy its
filter takes (see
[Choosing which books a destination takes](#choosing-which-books-a-destination-takes)).
For example, to send only the MP3 copy to a bucket a phone syncs from:

```sh
bookclerk plugins instances filter set <instance> --include format=mp3
```

The main output keeps the usual names. Every other copy goes under a
folder of its own at the top of the destination, named after the copy:
`mp3 64k-mono/Title [1a2b3c4d]/`. That folder name is short, so the
limits on path lengths shorten the book's own folders before it. If two
copies would still land in one folder (a destination template that
repeats a copy's name, or a path limit so tight it shortens two names
alike), the acquire stops with an error before it stores anything. Give
each destination one copy with filters
when a player scans the whole destination, so it doesn't see the book
twice.

With companion PDFs on (`output.download_pdf`), each destination gets
one PDF. A destination that takes the main output keeps it beside the
main output, at the usual name. A destination that takes only extra
copies gets it beside the first of them, in that copy's folder:
`mp3 64k-mono/Title [1a2b3c4d]/Title.pdf`. The PDF the library records
for the title stays the main output's.

Each copy's files are recorded on the edition with the copy's format and
quality, beside the probe of each audio file (see
[Media](media.md)). The main output's file is the one the library records
as the title's storage key, and the only one offered to a branch that
asks for the library's copy.

> [!NOTE]
> Copies in `output.copies` are made when a book is acquired. A copy added
> to `output.copies` later is not made for books already stored; add it to
> those with a convert (see [Adding a copy to a stored book](#adding-a-copy-to-a-stored-book)).
> When a book no destination receives any copy of is acquired, the acquire
> fails at once with "no destination accepts this book".

### Adding a copy to a stored book

An administrator can add one more copy to a book the library already
stores, without downloading it from the store again:

1. Open the book in **Library**.
2. Under **Convert**, pick a copy, such as **MP3, 64 kbit/s mono**. Opus
   copies are listed after ticking **Advanced formats**.
3. Select **Convert**.

From the command line, name the copy with `--to`; without it, the copy is
an MP3 at the `output.lame` settings:

```sh
bookclerk library convert --to mp3:64k-mono <book>
```

The web API takes the same request as `POST /api/library/convert`.

A convert runs as a job on a host that may store files. It makes the copy
from the stored copy closest to the store's audio: one kept as delivered
or as an M4B first, then the main output, then the copy on the primary
destination. The new copy goes beside the original, under its own folder
(`mp3 64k-mono/Title [1a2b3c4d]/`), to every destination whose filter
takes it, and is recorded on the edition as that copy. The title's
storage key stays the main output's. No companion PDF is written for it.

A convert of a copy the book already has does nothing, unless
**Make it again if it exists** (`--force`) is ticked. A convert refuses
`original` copies and the main output's own copy, which only an acquire
makes, and a book that is not stored yet.

## Upgrading to better files

A store sometimes offers a better file than the one a book was first
downloaded in: a higher bitrate, or a better codec. With
`library.upgrade_quality` on, the daemon looks for those once a day and
downloads them again.

```sh
bookclerk config set library.upgrade_quality true
bookclerk config set library.upgrade_daily_limit 5
```

The same switch is **Upgrade quality automatically** under
**Settings → Library**. It is off by default.

Once a day for each store account, the daemon:

1. walks the account's downloaded books, up to 512 of them, starting
   where the last day's check stopped;
2. asks the store which file the account would download for each today,
   without downloading anything;
3. compares that with the original copy the library holds (the
   `original` quality copy's codec, AAC profile and bitrate, from its
   [probe](media.md#what-a-stored-files-row-says));
4. queues a forced download of each book that would get a better file,
   at most `library.upgrade_daily_limit` per account (1 to 100,
   default 5).

An offer is better when:

| Held file | Offer that wins |
| --- | --- |
| Lossy | A lossless file (FLAC or ALAC), at any bitrate |
| MP3 | AAC, Vorbis, xHE-AAC or Opus at the same bitrate or higher |
| AAC-LC, HE-AAC or Vorbis | xHE-AAC or Opus at the same bitrate or higher |
| Any lossy codec | The same class of codec at 1.15 times the bitrate or more |
| Lossless | Nothing |

A file or an offer whose bitrate is unknown is never an upgrade, and
neither is a Dolby (E-AC-3 or AC-4) stream on either side. Only books
this account downloaded are compared, not a sibling account's copy or an
upload.

The download follows the account's [download pace](../internals/jobs.md#download-pacing),
like auto-acquire. The new file replaces the held one, and the library's
other copies are made again from it (see
[Keeping several copies](#keeping-several-copies)). When the store asks
the daemon to slow down, that account is left alone until the next day.

Only stores that can say what they would deliver take part. Audible
does (see [Stores](stores.md)); a store that can't is skipped.

## Several destinations

An acquire writes to every destination that takes the book at the same
time:

1. Each destination's write is tried up to three times.
2. The acquire succeeds as soon as one destination holds the complete
   files. It fails only when no destination stored them.
3. Bookclerk records each destination's copy of the edition: **stored**,
   **pending** (the write failed or the destination was unavailable) or
   **failed** (the copy failed 8 times in a row).
4. A pending copy is retried in the background, 5 minutes after it
   failed, then with the delay doubling each time, up to a day. Every few
   minutes the daemon queues an acquire for each edition with a retry
   due on a destination it has deployed; the destinations that already
   hold the edition are left alone. An acquire of one title tries its
   pending copies at once, without waiting for the delay. A retry that
   fails counts as an attempt on the copy, and the title stays acquired
   from the destinations that hold it.
5. A destination that cannot be listed when an acquire run starts sits
   that run out. The titles go to the destinations that answer, and the
   run clears nothing as missing and moves no files, since a title it
   cannot see may be on the destination that did not answer. Each title
   the run acquires gets a pending copy there, so the background retry
   writes it once the destination answers. The run fails only when no
   destination answers.

An acquire run also visits an edition that is already acquired when a
destination that takes it has no copy at all: one deployed after the
acquire, or one whose filter took the edition later. A failed copy is
left for you: other acquires of the edition write nothing there, and a
forced acquire of the title (`bookclerk library acquire --force`) tries it
again.

> [!NOTE]
> Editions acquired before copies were recorded get their copies from
> the audio files the acquire recorded on each destination
> (`edition_files`), the first time an acquire run looks at them.

When some destinations already have a title and others don't,
`output.multi_destination` decides what an acquire does:

| Policy | What happens |
| --- | --- |
| `sync_missing` (default) | Copies from a destination that has the title to those that don't, without downloading again. It copies the main output's copy only, so a destination whose copy rules refuse that copy gets nothing. A plain multi-part download is not matched on a destination by name, so it is downloaded again for the destination that lacks it |
| `refetch_missing` | Downloads and encodes again, and writes only to the destinations missing the title |
| `refetch_all` | Downloads and encodes again, and writes to every destination |

```toml
[output]
multi_destination = "sync_missing"
```

## Noticing changes made outside Bookclerk

Other programs change destinations too: a file manager renames a folder,
a sync client removes a book, a person copies files into the bucket.
Every few minutes each host checks the destinations it has deployed (one
host per destination at a time), and Bookclerk keeps the last state it
saw of every file there.

Each destination is followed in one of two ways:

- **A change feed**, when the destination has one turned on: the
  [folder watch](#watching-the-folder) on a local disk, or the
  [bucket's notification queue](#following-changes-in-the-bucket) on
  Amazon S3, the [Cloudflare Queue](#following-changes-in-an-r2-bucket)
  on R2, or [event notifications](#following-changes-in-a-b2-bucket) on
  B2. Bookclerk reads what changed since its last check, so
  changes show up within about five minutes.
- **A listing**, which every destination has: Bookclerk lists every file
  and compares the list with the last one. It runs every 6 hours, or once
  a day while the destination's feed is healthy. When the feed fails or
  says it may have missed changes, a listing runs at once and the feed
  takes over again after it. A long listing continues where it stopped
  on the next check, even after a restart.

When the edition's audio was moved to another key on the same
destination that Bookclerk already recorded, the copy follows the file.
When a file a stored copy needs is gone, `output.missing_files` decides
what happens:

| Value | What happens |
| --- | --- |
| `mark_missing` (default) | The copy is marked **missing** and nothing writes it again on its own. While another destination still holds the book, it stays downloaded there. Once no destination holds it, the book shows as not downloaded, so a member can ask for it again. An acquire of the whole library (or with `library.auto_acquire` on) leaves it out; an acquire of that title, or a forced one, gets it again |
| `restore` | The copy turns **pending**, and the background retry writes it again (see [Several destinations](#several-destinations)) |

```sh
bookclerk config set output.missing_files restore
```

Change it in **Settings → Output** too. A missing copy on a destination
while the book is still downloaded elsewhere is written again by a forced
acquire of the title (`bookclerk library acquire --force`), like a failed
copy.

> [!NOTE]
> A listing that misses more than a fifth of the files Bookclerk knew of
> before it started (and more than 10), or more than a fifth of the books
> it has stored there (and more than 10), more likely failed partway,
> with a wrong prefix, a missing permission or an unmounted disk, than saw
> the files really go. Bookclerk keeps those files and copies as they
> were, and **Settings → Plugins** shows **Listing error** on the destination, with
> the reason in its tooltip. A listing that fails outright shows the same.
> Fix the cause, and the next listing applies what it finds. Up to 10
> missing files always follow `output.missing_files`, so a destination
> with only a few books never stays stuck behind the guard.

**Settings → Plugins** shows each destination's state beside its name:
**Feed healthy**, **Feed gap**, **Feed failing**, **Listing error**, **Listed** or **Not
listed yet**. Hover over it for the number of files, the last and next
listing, and any error. The same fields are the instance's
`storage_sync` in the web API.

## Naming

Bookclerk names files with Libation's NamingTemplate engine
(`bookclerk-naming`). Pick a profile, and override it with templates if
you need to:

```toml
[output]
naming_profile = "audiobookshelf"
# folder_template = "<first author>/<title short>"
# file_template = "<title> [<call number>]"
```

| Profile | Layout |
| --- | --- |
| `audiobookshelf` (default) | `Author/Series/1 - Title/Title [call number].m4b`, or `Author/Title/Title [call number].m4b` outside a series, which Audiobookshelf scans as author, series and book |
| `classic` | Libation's desktop defaults with the call number added: `<title short> [<id>] [<call number>]` |

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

### The call number

Every template set must include `<call number>`: in the folder template,
or in both the file and the chapter file templates. The tag must be
written for every book, so a `<call number>` inside any condition
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
acquired where they are, whether or not their path has a call number.

Copies of one edition on several accounts (the same book bought twice)
share their files. The edition is downloaded once, and every account's
copy is marked acquired at its path once each destination holds it.

**Path sanitization** chooses which characters the templates replace
before a name reaches any destination. `auto`, the default, replaces only
what no key can hold (`/` inside a name, and control characters) and leaves
the rest to each destination ([Keys each destination stores](#keys-each-destination-stores)).
Set `output.path_sanitization` to `windows`, `posix`, `s3` or `none` to
apply that set everywhere instead, or list your own
`output.replacement_characters`.

## Formats and sidecars

```toml
[output]
format = "enriched_m4b"   # single_mp3 | split_mp3_by_chapter | opus | none
copies = []               # extra copies, see "Keeping several copies"
keep_codec_in_m4b = false # see "Keeping unusual audio in M4B"
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

### Keeping unusual audio in M4B

An M4B normally holds AAC audio, which every audiobook player plays. When
a store delivers Opus, FLAC, ALAC, E-AC-3 or AC-4 audio, Bookclerk
re-encodes it to AAC for the M4B. Two codecs are kept as they are
whatever this setting says, because no decoder for them is built in:
E-AC-3 in a build without the [FFmpeg decoders](media.md#ffmpeg-decoders)
(release builds have them, so there it re-encodes), and AC-4 in every
build. The advanced setting
`output.keep_codec_in_m4b` (off by default; **Settings → Output →
Advanced**) keeps that audio unchanged inside the M4B instead, with its
own sample entry, tags and chapters:

```sh
bookclerk config set output.keep_codec_in_m4b true
```

Keeping it saves an encode and keeps the delivered audio bit for bit, but
some players and devices cannot play these codecs inside an M4B. When a
book's parts differ in codec, sample rate or channels, they are still
encoded to AAC.

With `fixup_metadata` on, an M4B also records which Bookclerk version
tagged it, in the freeform item `----:app.bookclerk:TOOL`.

`fixup_metadata` also embeds the chapters: as a chapter track in an M4B,
and as ID3 `CHAP` frames listed by one `CTOC` frame in a single MP3, which
Audiobookshelf and most players read. Each chapter ends where the next one
starts and the last ends with the audio. A file that already carries
chapters keeps them, unless Bookclerk overlays Audible's chapter list on a
book from another store.

Audible's own options (Widevine, xHE-AAC, brand trim) are on the
[Stores](stores.md#audible) page.

## Next steps

- [Managing plugins](plugins.md): deploy, configure and update plugins.
- [Configuration](configuration.md): every shared setting.
- [Backups, updates, import and export](backups.md): keep a copy of the
  library database.
