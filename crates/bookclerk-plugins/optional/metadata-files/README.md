# Metadata files

A `metadataFiles` plugin (id `metadata_files`) that reads, and for most
formats writes, the metadata files other tools keep next to a book's
audio. It runs in `workerd` with no network or file grant: the host hands
it one file's bytes and takes the new bytes back.

| Dialect | Files | Writes | Fields |
| --- | --- | --- | --- |
| `abs_metadata_json` | `metadata.json` | yes | title, subtitle, description, publisher, date, language, genres, tags, abridgement, ISBN and ASIN, authors and narrators, series, chapters |
| `libation_json` | `*.metadata.json` | no | the same without tags, plus running time |
| `abs_nfo` | `*.nfo` | yes | the same as `metadata.json` without chapters |
| `desc_txt` | `desc.txt` | yes | description |
| `reader_txt` | `reader.txt` | yes | narrators |

## How each format reads

- **Audiobookshelf `metadata.json`.** The keys Audiobookshelf 2.x writes:
  `authors` and `narrators` are name lists, `series` entries are
  `"Name #position"`, `publishedDate` wins over `publishedYear`, `abridged`
  is a boolean (`false` reads as unabridged), and `chapters` are
  `{ start, end, title }` in seconds. Values nested under a legacy
  `metadata` object win over the top level, as in Audiobookshelf. A key
  with `null` or an empty list states the field as empty.
- **Libation's catalog record.** The Audible product Libation saves at
  download, at the top level or under `product` or `item`.
  `publisher_summary` is turned from HTML into text, `category_ladders`
  become genres, `format_type` the abridgement, and
  `content_metadata.chapter_info` the nested chapter layout and running
  time (else `runtime_length_min`). It is read-only.
- **NFO text.** `Key: value` lines, keys compared without case, the last
  line for a key winning, then an optional `Book Description` section to
  the end of the file. `Title` holds the subtitle after its first `:`;
  `Author`, `Narrator` (or `Read By`), `Genre` and `Tags` are comma
  separated; `Series Name` and `Position in Series` make one series entry;
  `Release Date`, `Date` or `Audible.com Release` give the date, and
  copyright lines give the year when none of those exist; `Unabridged` or
  `Abridged` answer `Yes` or `No`. A line with an empty value states
  nothing.
- **`desc.txt`** is the description, trimmed. **`reader.txt`**'s first
  line holds the narrators, split at `,`, `;`, `&` and `and`.

Every value is cleaned before it reaches the host: control characters
are dropped, labels stay on one line, text is cut to the ABI's byte caps
on a character boundary, repeats and blanks are dropped, and dates must be
real `YYYY`, `YYYY-MM` or `YYYY-MM-DD` dates. A file is refused only when
its structure is unreadable (a JSON file that is not a JSON object). A
file that is not valid UTF-8 is read as Windows-1252.

## How each format writes

Rendering changes only the fields the host asks for and keeps everything
else in the file:

- `metadata.json` keeps every other key in place and writes with
  two-space indentation, as Audiobookshelf does. A removed field loses its
  key. A credit role the record does not state keeps its key.
- An NFO rewrites a field's first line (keeping its key and spacing),
  drops the field's other lines, adds a new field after the last
  `Key: value` line, and keeps every other line and the file's line
  endings. A removed date also drops the copyright lines it would
  otherwise be read from.
- `reader.txt` replaces its first line and keeps the rest.

An answer with nothing left in it is empty, and the host decides whether
to remove the file. Formats keep one value where the record can hold
several: the NFO keeps the first series, and both `metadata.json` and the
NFO keep the first ISBN and the first ASIN.

## Develop

`src/` is the source and `modules/` is what ships; `npm run build`
regenerates `modules/`, and CI fails when the committed copy differs. The
unit tests read the seeds under `fuzz/`, and `test/fuzz.test.ts` mutates
them against an oracle written from the ABI's record rules (see
[Fuzzing](../../../../docs/contributing/fuzzing.md#metadata-file-dialect-guest)).

```sh
cd packages/plugin-sdk && npm ci && npm run build
cd ../../crates/bookclerk-plugins/optional/metadata-files
npm ci
npm run typecheck
npm run build
npm test
```

See [Dynamic plugins](../../../../docs/plugins/README.md#first-party-plugins).
