# Where your books go

When Bookclerk downloads a book, it saves a finished audiobook file, with
its cover and chapters, to one or more places you choose. These places are
called **destinations**.

## A folder on this computer

Out of the box, books go to the `Audiobooks` folder inside Bookclerk's
files folder. That is `BookclerkFiles` when you started with `cargo dev`.

To use another folder, such as the one your Audiobookshelf or Plex server
reads, the operator changes the **local** plugin's `root` setting in
**Settings → Plugins**.

## Cloud storage

Bookclerk can also save to Amazon S3, or to a storage server that works the
same way, such as MinIO.

1. Start Bookclerk with the optional plugins (`cargo dev --optional`),
   which include **s3**.
2. The operator enables **s3** in **Settings → Plugins**.
3. The operator fills in the bucket name and keys.

You can use a folder and cloud storage at the same time. Each new book goes
to every destination that is turned on.

## How files are named

**Settings → Output** sets how folders and files are named. Pick a
**naming profile**:

- **audiobookshelf**, the default, puts each book in an author and series
  folder that Audiobookshelf understands.
- **classic** matches Libation's defaults.
- Or write your own folder and file templates.

The same page also sets:

- the file format: one M4B file per book by default, or MP3;
- whether to save the cover image, a companion PDF and chapter information
  beside each book.

Each book's file name ends with a short library id, such as
`[1a2b3c4d]`, for example `Author/Series/1 - Title/Title [1a2b3c4d].m4b`.
The id comes from the store's product, so the same book gets the same id
every time it is scanned. It keeps two different editions of a book, such as an
abridged and an unabridged recording, from ever overwriting each other.
If you write your own templates, they must include it too. If you own a
book on two accounts, it is downloaded once and both copies point at the
same files.

## Books you already have

If you already keep audiobooks in a folder or bucket, Bookclerk can match
them to your library instead of downloading them again. Ask your operator
to run a scan with storage matching (see the
[Output and destinations reference](../server/output.md)).

For operators, every destination setting, naming template and command-line
equivalent is in the [Output and destinations reference](../server/output.md).
