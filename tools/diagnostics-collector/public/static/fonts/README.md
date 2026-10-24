# Web fonts

Copies of `tools/site/src/fonts/` (same files, same hashes) so the
diagnostics dashboard also serves its fonts itself. Update both together.

The site serves its three typefaces itself rather than from Google Fonts, so
no third-party stylesheet blocks the first paint and no visitor's request goes
to another origin. All three are under the SIL Open Font License 1.1; each
license ships beside the files (`OFL-*.txt`) and is published with them.

| File | Family | Weights | Upstream |
| --- | --- | --- | --- |
| `source-sans-3-v20-latin.woff2`, `-latin-ext` | Source Sans 3 | 200–900 (variable) | Google Fonts `sourcesans3/v20` |
| `literata-v40-latin.woff2`, `-latin-ext` | Literata | 400–900, opsz 7–72 (variable) | Google Fonts `literata/v40` |
| `ibm-plex-mono-v20-{400,500}-latin.woff2`, `-latin-ext` | IBM Plex Mono | 400, 500 | Google Fonts `ibmplexmono/v20` |

The files are the latin and latin-ext subsets Google Fonts serves for
`css2?family=IBM+Plex+Mono:wght@400;500&family=Literata:opsz,wght@7..72,500;7..72,700&family=Source+Sans+3:wght@400;600;700`,
downloaded unchanged; `site.css` and `../dashboard.css` repeat their `unicode-range`s. The
upstream version is part of each file name because `_headers` lets browsers
cache the font files for a year: a new version must get a new name.

SHA-256:

```
6bc0f226a5b7884a8170e3f62c63d7675609d4631bdc5931b5cdab81821f00eb  ibm-plex-mono-v20-400-latin-ext.woff2
08949f728dc52d528e69b1667d15c89a5686a4ee9a296ff90983985f99c380f7  ibm-plex-mono-v20-400-latin.woff2
6bb06407c97584b0867a959e05e8874693bfeb8c317de190811c51598f2d99ea  ibm-plex-mono-v20-500-latin-ext.woff2
01d285447409c8a588692162439a038b8cbd7871309ee20267b0d2d91c6e8e22  ibm-plex-mono-v20-500-latin.woff2
334990a82ce173e5169f5f0946c679f7d8c1ee8608eb29b56f85284802c712c9  literata-v40-latin-ext.woff2
7478e1920209e51dcc235d151d2056a0a6e3c54b4436f7030d256f231fe09b58  literata-v40-latin.woff2
a85a7459bdb3cdc1136751e151a506bae653fc29ada3ca86237477df6f1b59e6  source-sans-3-v20-latin-ext.woff2
19143dca075972bc84e3fd9eab7416dd40cc565dccb2435a044bdfabe060c7d5  source-sans-3-v20-latin.woff2
```

## Fallback metrics

`site.css` also declares `… Fallback` faces: local system fonts scaled with
`size-adjust` and `ascent-override` / `descent-override` so their text runs
as wide and as tall as the web font's. Lines then break in the same places
before and after the web font arrives, and the page does not shift.

The values compare the average advance width of the site's own page text in
the web font (at the weight named) with the same text in the
metric-compatible Liberation fonts (Arial, Times New Roman and Courier New
metrics), and divide the web font's vertical metrics by that ratio:

| Fallback | Web font instance | Local font | size-adjust | ascent | descent |
| --- | --- | --- | --- | --- | --- |
| Source Sans 3, 400 | wght 400 | Arial | 93.12% | 109.97% | 42.96% |
| Source Sans 3, 600–700 | mean of wght 600 and 700 | Arial Bold | 90.38% | 113.29% | 44.26% |
| Literata, 400–500 | wght 500, opsz 24 | Times New Roman | 119.61% | 98.40% | 25.75% |
| Literata, 600–700 | wght 700, opsz 24 | Times New Roman Bold | 117.64% | 100.05% | 26.18% |
| IBM Plex Mono | 400 | Courier New | 99.98% | 102.52% | 27.50% |

Recompute them (with fontTools) if a font file changes.
