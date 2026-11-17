# Metadata files

A `metadataFiles` plugin (id `metadata_files`) for the sidecar metadata
formats of tools that have no Bookclerk plugin of their own. A format that
belongs to one tool lives in that tool's plugin instead (Audiobookshelf's
`metadata.json`, for example, goes with the Audiobookshelf plugin). It
runs in `workerd` with no network or file grant: the host hands it one
file's bytes.

| Dialect | Files | Writes | Fields |
| --- | --- | --- | --- |
| `libation_json` | `*.metadata.json` | no | title, subtitle, description, publisher, date, language, genres, abridgement, ASIN and ISBN, authors and narrators, series, chapters, running time |

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

Every value is cleaned before it reaches the host: control characters
are dropped, labels stay on one line, text is cut to the ABI's byte caps
on a character boundary, repeats and blanks are dropped, and dates must be
real `YYYY`, `YYYY-MM` or `YYYY-MM-DD` dates. A file is refused only when
it is not a JSON object.

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
