# Open Library metadata provider

A `metadata` plugin (id `openlibrary`) that answers book lookups and title
searches from [Open Library](https://openlibrary.org). It runs in `workerd`
and reaches only `openlibrary.org`.

- `lookup` takes an Open Library edition id, work id or ISBN, strongest
  first, and makes one `search.json` request. With a matched edition the
  record describes that edition (its ids, ISBNs, language, publisher and
  date); otherwise it describes the work.
- `search` takes a title and, optionally, an author and a language. It keeps
  only hits whose title overlaps the one asked for.
- `chapters` is not served: Open Library has no chapter layouts.
- `health` answers without a request.

Open Library asks API users to identify themselves. Set the
`contact_email` setting and it goes in the `User-Agent`.

## Develop

`src/` is the source and `modules/` is what ships; `npm run build`
regenerates `modules/`, and CI fails when the committed copy differs.

```sh
cd packages/plugin-sdk && npm ci && npm run build
cd ../../crates/bookclerk-plugins/optional/metadata-openlibrary
npm ci
npx bookclerk-plugin types .   # after editing plugin.toml
npm run typecheck
npm run build
npm test
```

See [Dynamic plugins](../../../../docs/plugins/README.md#first-party-plugins).
