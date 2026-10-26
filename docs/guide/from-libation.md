# Coming from Libation

If you used Libation before, Bookclerk can bring over your library, your
settings and your naming templates, so you do not start from scratch.

## What comes over

- Your book list and what Libation knew about each book.
- Your settings: file format, cover and chapter options, and folder and file
  naming templates. If this Bookclerk has already been started, its
  existing output settings stay as they are; when they differ from
  Libation's, the import warns, so check **Settings → Output** afterward.
- The list of your accounts.

Store sign-ins do **not** come over. After the import, connect each store
again on **Accounts** (see [Stores](stores.md)).

## Import

The import needs the Audible plugin, so run it with `--optional` as below.
Stop Bookclerk first. Then, from the Bookclerk checkout, point the import at
your Libation files folder:

```sh
cargo dev-cli --optional -- import libation --from ~/LibationFiles --dry-run
cargo dev-cli --optional -- import libation --from ~/LibationFiles --force
```

The first command only reports what it would do. The second imports, and
overwrites Bookclerk's `config.toml` with settings built from Libation's.

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
