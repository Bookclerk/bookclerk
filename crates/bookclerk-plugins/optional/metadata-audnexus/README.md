# Audnexus metadata provider

A `metadata` plugin (id `audnexus`) that answers audiobook lookups and
chapter layouts from [Audnexus](https://audnex.us), which serves Audible
catalog data without an Audible account. It runs in `workerd` and reaches
only `api.audnex.us`.

- `lookup` takes ASINs and makes one request per ASIN, up to five. Each
  identifier's marketplace picks the Audnexus region (`gb` is `uk`, and a
  marketplace Audnexus does not serve falls back to `us`).
- `chapters` returns the first ASIN's chapter layout in pre-order, with the
  brand intro and outro lengths and whether the offsets are exact.
- `search` is not served: Audnexus has no title search.
- `health` answers without a request.

## Develop

`src/` is the source and `modules/` is what ships; `npm run build`
regenerates `modules/`, and CI fails when the committed copy differs.

```sh
cd packages/plugin-sdk && npm ci && npm run build
cd ../../crates/bookclerk-plugins/optional/metadata-audnexus
npm ci
npx bookclerk-plugin types .   # after editing plugin.toml
npm run typecheck
npm run build
npm test
```

See [Dynamic plugins](../../../../docs/plugins/README.md#first-party-plugins).
