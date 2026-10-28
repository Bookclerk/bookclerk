# UI web fonts

Bundled by Vite from `../index.css`, so the web UI never loads fonts from
Google or any other third-party origin. All are SIL Open Font License 1.1;
the license texts live in `ui/public/fonts/`, so Vite copies them into the
build output (`/fonts/OFL-*.txt`) next to the bundled font files.

| File | Family | Weights | Upstream |
| --- | --- | --- | --- |
| `source-sans-3-v20-latin.woff2`, `-latin-ext` | Source Sans 3 | 200–900 (variable) | Google Fonts `sourcesans3/v20` |
| `literata-v40-latin.woff2`, `-latin-ext` | Literata | 400–900, opsz 7–72 (variable) | Google Fonts `literata/v40` |
| `google-sans-v71-500-latin.woff2`, `-latin-ext` | Google Sans (the "Sign in with Google" button only) | 500 | Google Fonts `googlesans/v71` |

These are the latin and latin-ext subsets Google Fonts serves, unchanged. The
Source Sans 3 and Literata files are the same as the website's
(`tools/site/src/fonts/`, which also documents how the fallback metrics in
`index.css` were derived).

SHA-256:

```text
44fb8927aa12c4be6cf4ac884cc6b8d2f1a284d363274c648060d0a49b51cd78  google-sans-v71-500-latin-ext.woff2
7ee2b232de66f7530be28964818bc8a813e62553138e48bffd917e95b19bd7fa  google-sans-v71-500-latin.woff2
334990a82ce173e5169f5f0946c679f7d8c1ee8608eb29b56f85284802c712c9  literata-v40-latin-ext.woff2
7478e1920209e51dcc235d151d2056a0a6e3c54b4436f7030d256f231fe09b58  literata-v40-latin.woff2
a85a7459bdb3cdc1136751e151a506bae653fc29ada3ca86237477df6f1b59e6  source-sans-3-v20-latin-ext.woff2
19143dca075972bc84e3fd9eab7416dd40cc565dccb2435a044bdfabe060c7d5  source-sans-3-v20-latin.woff2
```
