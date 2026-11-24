# Media downloads

A `mediaDownload` plugin (id `media_download`) that downloads public media
for the host: cover art, and storefronts' audio samples. It runs in
`workerd` and may reach any public website, because cover art and samples
live on every store's and catalog's own CDN.

- `downloadMedia` takes up to 32 `https` URLs and a per-body byte cap (at
  most 10 MiB for images, 32 MiB for audio). It fetches four at a time,
  with a limit of 15 seconds per image and 60 seconds per audio sample, follows up to five
  redirects only to URLs that pass the same `https` check, and streams each
  body into the sink the host passes, under the URL's index. It answers
  one entry per URL: stored, or why not (HTTP status, not an image, over
  the cap, refused URL).
- When the host already holds an image, it sends that image's `ETag` and
  `Last-Modified`, which the plugin passes on as `If-None-Match` and
  `If-Modified-Since`. A `304 Not Modified` answer stores nothing and
  reports `notModified`.
- Every answer carries the server's `ETag`, `Last-Modified`,
  `Cache-Control`, `Expires`, `Date` and `Age`, each at most 1024 bytes of
  plain ASCII. The host decides from them how long to keep the image.
- The request's `media` is `image` (the default) or `audio`. With `audio`
  the plugin downloads a storefront's public audio sample for a
  [book sample](../../../../docs/adr/book-samples.md): it asks for
  `audio/*` and refuses an answer that is plainly not audio, the same way
  it refuses a cover that is not an image.
- `health` answers without a request.

The grant is `domains = ["*"]`: every public hostname, but never an IP
literal, and never a private, loopback or metadata address. Nothing the
plugin fetches is trusted. The host checks each body's real type, decodes
it with limits and re-encodes it before it stores or serves anything; an
audio sample is decoded and re-encoded by the jailed media worker.

## Develop

`src/` is the source and `modules/` is what ships; `npm run build`
regenerates `modules/`, and CI fails when the committed copy differs.

```sh
cd packages/plugin-sdk && npm ci && npm run build
cd ../../crates/bookclerk-plugins/optional/media-download
npm ci
npm run typecheck
npm run build
npm test
```

See [Dynamic plugins](../../../../docs/plugins/README.md#first-party-plugins).
