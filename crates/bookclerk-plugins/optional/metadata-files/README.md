# Metadata files

A `metadataFiles` plugin (id `metadata_files`) for the sidecar metadata
formats of tools that have no Bookclerk plugin of their own. A format that
belongs to one tool lives in that tool's plugin instead (Audiobookshelf's
`metadata.json`, for example, goes with the Audiobookshelf plugin). It
runs in `workerd` with no network or file grant: the host hands it one
file's bytes.

| Dialect          | Files             | Writes | Fields                                                                                                                                             |
| ---------------- | ----------------- | ------ | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| `libation_json`  | `*.metadata.json` | no     | title, subtitle, description, publisher, date, language, genres, abridgement, ASIN and ISBN, authors and narrators, series, chapters, running time |
| `cue`            | `*.cue`           | yes    | title, date, genres, authors, chapters                                                                                                             |
| `kodi_album_nfo` | `album.nfo`       | yes    | title, description, publisher, date, genres, authors                                                                                               |
| `opf`            | `*.opf`           | yes    | title, description, publisher, date, language, genres, ISBN and ASIN, authors, narrators, translators, editors and illustrators, series            |

## Libation's catalog record

Libation saves the Audible product it downloaded as
`<book name>.metadata.json`. The reader takes the product at the top level
or under `product` or `item`:

- `title`, `subtitle`, `publisher_name`, `language`, `asin` and `isbn`;
- `authors` and `narrators` (`{ name }` lists);
- `release_date`, else `issue_date`;
- `publisher_summary` (else `merchandising_summary`), turned from HTML
  into text;
- `category_ladders` as genres and `format_type` as the abridgement;
- `series` (`{ title, sequence, asin }`), keeping the series ASIN;
- `content_metadata.chapter_info` for the nested chapter layout and
  running time, else `runtime_length_min`.

It is read-only: the file is the store's record, not the library's.

## OPF package documents

An OPF is an EPUB's package document; Calibre and other tools also save
one next to a book as `metadata.opf`. The reader takes the first
`<metadata>` of the `<package>` root:

- `dc:title`, `dc:publisher`, `dc:language`, and `dc:subject` as genres;
- `dc:description`, turned from HTML into text;
- the first `dc:date` with no `opf:event` (or the publication event);
- `dc:identifier` with an `ISBN` or `ASIN` scheme, or a `urn:isbn:` value;
- `dc:creator` and `dc:contributor` by MARC relator role (`aut`, `nrt`,
  `trl`, `edt`, `ill`), on the element or in a
  `<meta refines="#id" property="role">`; a creator with no role is an
  author;
- one series: Calibre's `calibre:series` and `calibre:series_index`, else
  EPUB 3's `belongs-to-collection` with its `group-position`.

A write edits the file in place. For each field it sets, it removes the
elements that field reads from (and the refinements they leave behind) and
appends new ones at the end of `<metadata>`, in Calibre's style; every
other byte stays as it was. Credits change only for the roles the request
states. With no file yet, it writes a new OPF 2 package.

## Kodi album.nfo

Kodi and Jellyfin read an `album.nfo` for a music album, and audiobook
folders reuse it. The reader takes the `<album>` root's `title`, `review`
(the description, as plain text), `label` (publisher), a non-blank
`releasedate` (else `year`) and each `genre`. Authors come from the
`artist` of each `albumArtistCredits`, as Kodi exports them, else from each
direct `artist`, as older files and other tools write them.

A write edits the file in place, like OPF, and leaves the file untouched
when it has nothing to add or remove. Writing the date also writes `year`.
Writing authors writes each one both ways, and replaces `artistdesc` (the
artists as Kodi shows them) with the names joined by `/`, so Kodi does
not show the old artists.

## CUE sheets

A CUE sheet lists an audio file's tracks, which the reader takes as the
book's chapters. The commands before the first track give the book's
`TITLE`, each `PERFORMER` as an author, `REM DATE` and each `REM GENRE`;
each track's `TITLE` and `INDEX 01` (in 1/75 s frames) make a chapter. A
sheet with several `FILE`s times each track from the start of its own
file, so its chapters are left unstated.

A write edits a sheet line by line: the book's details go just before the
first `FILE`, and new chapters replace every track. When the host passes
the book's audio files (`audioFiles` in the render request), a write can
also create a sheet, and writing chapters rebuilds the `FILE` lines from
those files:

- each chapter goes into the file it starts in, timed from that file's
  start, and track numbers run on across files;
- a file that opens partway through a chapter starts with a track that
  carries that chapter on;
- with no chapters, the files get no tracks;
- a file keeps the type the sheet gave it, else takes the type its
  extension names (`MP3`, `AIFF`, `MP4`, otherwise `WAVE`).

Without the audio files, a write cannot create a sheet or write chapters
into a sheet with several files. Chapters past a file of unknown length
are refused, since the write cannot tell where that file ends, and so is a
chapter that starts after the last file's known end. CUE cannot
escape `"`, so a value's double quotes are written as single quotes, and
an audio file name with one is refused.

## XML

XML is scanned without expanding anything but XML's five entities and
numeric references: a document type with an internal subset, nesting past
64 levels or a file that is not well-formed is refused.

Every value is cleaned before it reaches the host: control characters
are dropped, labels stay on one line, text is cut to the ABI's byte caps
on a character boundary, repeats and blanks are dropped, and dates must be
real `YYYY`, `YYYY-MM` or `YYYY-MM-DD` dates. A file is refused only when
its structure is unreadable: not a JSON object for Libation, not a
well-formed package for OPF, not a well-formed `<album>` for
`album.nfo`, no `FILE` line for a CUE sheet.

## Develop

`src/` is the source and `modules/` is what ships; `npm run build`
regenerates `modules/`, and CI fails when the committed copy differs. The
unit tests read the seeds under `fuzz/`, and `test/fuzz.test.ts` mutates
them against an oracle written from the ABI's record rules (see
[Fuzzing](../../../../docs/contributing/fuzzing.md#metadata-file-dialect-guests)).

```sh
cd packages/plugin-sdk && npm ci && npm run build
cd ../../crates/bookclerk-plugins/optional/metadata-files
npm ci
npm run typecheck
npm run build
npm test
```

See [Dynamic plugins](../../../../docs/plugins/README.md#first-party-plugins).
