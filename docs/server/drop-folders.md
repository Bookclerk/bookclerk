# Drop folders

A **drop folder** is a folder Bookclerk watches. Copy a book into it, and
once the files stop changing, Bookclerk adds the book to the library and
moves the files out of the way. It is the hands-off twin of uploading a
book in the web app: both go through the same ingest job, so a dropped
book is matched, tagged and stored exactly like an uploaded one.

Any storage plugin instance can be a drop folder: a local folder (`local`),
a mounted share, or an S3 prefix (`s3`). A drop folder is never a
destination: acquired books are not written into it.

## How it works

1. You copy files into the folder. Each top-level entry is one **item**: a
   single file (`Dune.m4b`) or a folder of files (`Dune/01.mp3`,
   `Dune/02.mp3`, `Dune/cover.jpg`).
2. Bookclerk lists the folder every `scan_interval_seconds` (5 minutes by
   default), and sooner when the storage plugin reports a change (the
   local plugin's watcher, or the S3 plugin's queue feed).
3. An item is ready once none of its files has changed for
   `settle_seconds` (one minute by default). A file still being copied
   keeps changing size or modification time, so it keeps waiting. Partial
   downloads (`*.part`, `*.crdownload`, `*.tmp` and similar) are ignored.
4. Bookclerk hands the ready item to the ingest job as one batch, the
   same job an [upload](uploads.md) runs. The job copies the files from the
   folder, checks that they still have the size that settled, and adds the
   book. Files it does not keep, such as a stray `.exe`, are refused on
   their own without failing the book.
5. When the batch ends, the item is moved to `_imported/<date>/` (the
   default, with the date in UTC), deleted, or left where it is. An item
   that could not be added, or with a file that failed to store, goes to
   `_failed/`, with a `<item>.bookclerk-error.txt` note beside it saying
   why (for example `intake_changed` when a file was still growing). An
   item whose files changed while it was being added stays where it is and
   settles again.

> [!NOTE]
> Bookclerk never matches a dropped book by its title alone. A book whose
> files carry no ASIN, ISBN or other strong identifier is still added and
> playable, as `needs_review`, and waits for an Administrator to confirm
> which edition it is (see
> [Review uploaded books](uploads.md#review-uploaded-books)).

Files and folders whose names start with a dot, and the `_imported` and
`_failed` folders at the top of the drop folder, are always skipped.

## Who holds a dropped book

Every dropped book gets a patron holding, as an uploaded book does. The
folder's `attribute_to` setting says whose:

- `owner` (the default): one patron, named by user id with `--patron`,
  holds everything the folder adds.
- `subfolder`: each top-level folder is a patron's username, and holds that
  patron's books (`alice/Dune.m4b`, `bob/Emma/01.mp3`). An item under a
  name that matches nobody waits until an Administrator creates that
  patron or moves the files.

## Set up a drop folder

A storage instance's role is read when its plugin starts, so change it
while the instance is deployed nowhere.

1. Create a storage instance for the folder, for example a second `local`
   instance:

   ```bash
   bookclerk plugins instances create local --name "Drop folder"
   bookclerk plugins config set <instance-id> root /srv/audiobooks/drop
   ```

2. Make it a drop folder. `--patron` takes the user id of the patron who
   holds what it adds:

   ```bash
   bookclerk plugins instances intake set <instance-id> --patron 1
   ```

   Or, for one subfolder per patron:

   ```bash
   bookclerk plugins instances intake set <instance-id> --subfolder
   ```

3. Deploy it on the node that can see the folder:

   ```bash
   bookclerk plugins deploy <instance-id> --node <node-id>
   ```

To check the settings, run `bookclerk plugins instances intake get
<instance-id>`. To turn the drop folder back into a destination, undeploy
it, then run `bookclerk plugins instances intake clear <instance-id>`.

The web API has the same three operations on
`/api/config/plugin-instances/{id}/intake` (`GET`, `PUT` with the settings,
`DELETE`), for Administrators and Owners.

## Settings

The settings of a drop folder that is already one can change at any time,
deployed or not; Bookclerk reads them on every pass. Options left out of
`intake set` keep their current value.

| Setting | CLI option | Default | Meaning |
| --- | --- | --- | --- |
| `settle_seconds` | `--settle-seconds` | `60` | How long an item must stay unchanged before it is added (5 to 86400). Raise it for slow network copies. |
| `scan_interval_seconds` | `--scan-interval-seconds` | `300` | Time between two full listings (30 to 86400). Change events only make a pass come sooner. |
| `after_ingest` | `--after-ingest` | `move` | `move` to `_imported/<date>/`, `delete`, or `leave`. With `leave`, Bookclerk remembers the item and adds it again only if its files change. |
| `attribute_to` | `--patron` / `--subfolder` | `owner` | Who holds the books; see [Who holds a dropped book](#who-holds-a-dropped-book). |
| `patron` | `--patron` | none | The holding patron's user id. Required for `owner`, refused for `subfolder`. |
| `ignore` | `--ignore` (repeatable) | none | Extra file name patterns to skip, with `*` and `?`, matched without regard to case (up to 64). |
| `prune_imported_days` | `--prune-imported-days` | off | With `move`, delete `_imported/` day folders older than this many days (1 to 3650). Checked at most once an hour. |

> [!NOTE]
> `delete` removes the files from the drop folder only after the book is
> stored in every destination. Keep `move` if the drop folder is your only
> copy until you have checked the result.

## Next steps

- [Output and destinations reference](output.md): where added books are
  stored.
- [Managing plugins](plugins.md#instances): create, configure and deploy
  plugin instances.
