# Coming from Libation

If you used Libation before, Bookclerk can bring over your library, your
settings and your naming templates, so you do not start from scratch.

## What comes over

- Your book list, and what Libation knew about each book.
- Your settings: file format, cover and chapter options, and folder and
  file naming templates.
- The list of your accounts.

If this Bookclerk has already been started, its output settings stay as
they are. When they differ from Libation's, the import warns you, so check
**Settings → Output** afterward.

> [!IMPORTANT]
> Store sign-ins do **not** come over. After the import, connect each store
> again on **Accounts** (see [Stores](stores.md)).

## Import

The import needs the Audible plugin, so run it with `--optional` as below.

1. Stop Bookclerk.
2. From the Bookclerk checkout, point the import at your Libation files
   folder, first as a dry run. This only reports what it would do:

   ```sh
   cargo dev-cli --optional -- import libation --from ~/LibationFiles --dry-run
   ```

3. If the report looks right, run the import:

   ```sh
   cargo dev-cli --optional -- import libation --from ~/LibationFiles --force
   ```

> [!WARNING]
> The real import overwrites Bookclerk's `config.toml` with settings built
> from Libation's.

## After the import

1. Start Bookclerk again and sign in.
2. Turn on the stores you use and connect your accounts.
3. Check **Settings → Plugins** and **Settings → Output**, so that books go
   to the same folder and with the same names as before.
4. If your books are already in that folder, have Bookclerk match them
   instead of downloading them again (see
   [Where your books go](where-books-go.md#books-you-already-have)).
5. Leave **Auto acquire** off until a scan looks right.

## Going back

Bookclerk can also write a Libation files folder, so you can return to
Libation or use its tools. Store sign-ins are never written out.

The commands, flags and backup options are in
[Backups, import and export](../server/backups.md).
