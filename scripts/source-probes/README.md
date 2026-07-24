# Source endpoint probes

Live, API-first smoke checks for candidate audiobook sources (no cookie
scraping). Mirrors the intent of `scripts/librofm-apk-probe/`: prove the
reverse-engineered surface is reachable before wiring a `ContentSource`.

## Run

```bash
# Public / contract probes only (no accounts required)
python3 scripts/source-probes/probe_all.py

# Assert Rust client constants still match probe expectations
python3 scripts/source-probes/check_client_constants.py --sources graphicaudio,chirp

# Optional: Storytel password encryption for auth probes
pip install pycryptodome

# Subset
python3 scripts/source-probes/probe_all.py --sources graphicaudio,chirp

# Auth probes when you have test accounts
export TEST_GA_EMAIL=... TEST_GA_PASSWORD=...
export TEST_CHIRP_EMAIL=... TEST_CHIRP_PASSWORD=...
export TEST_STORYTEL_EMAIL=... TEST_STORYTEL_PASSWORD=...
export TEST_ABC_EMAIL=... TEST_ABC_PASSWORD=...
python3 scripts/source-probes/probe_all.py
```

Writes `artifacts/source-probes/report.{md,json}` (gitignored by default;
commit snapshots only when intentionally documenting a probe pass).

## CI

Scheduled GitHub Actions (staggered weekly) keep reverse-engineered surfaces honest:

| Workflow | Schedule | Secrets |
| --- | --- | --- |
| `.github/workflows/librofm-apk-probe.yml` | Monday | `TEST_LIBRO_*` |
| `.github/workflows/graphicaudio-api-probe.yml` | Tuesday | `TEST_GA_EMAIL` / `TEST_GA_PASSWORD` |
| `.github/workflows/chirp-api-probe.yml` | Wednesday | `TEST_CHIRP_EMAIL` / `TEST_CHIRP_PASSWORD` |

### Behaviour

- **Public endpoints always run.** Missing credential secrets do **not** fail
  the job; auth checks are marked `skipped`.
- **Auth runs only when secrets are set.** Then auth failures fail the job.
- **Artifacts never include media.** Uploads are JSON/Markdown reports only
  (`report.json` / `report.md`, plus Libro `apk_shapes.json` / smoke JSON).
  APKs and CDN media stay on the runner (or under `$RUNNER_TEMP`) and are not
  uploaded. Libro live smoke uses `--no-media-download`.

## Credentials checklist

| Source | Probe env | CLI password env |
| --- | --- | --- |
| GraphicAudio | `TEST_GA_EMAIL` / `TEST_GA_PASSWORD` | `LIBATION_GA_PASSWORD` |
| Chirp | `TEST_CHIRP_EMAIL` / `TEST_CHIRP_PASSWORD` | `LIBATION_CHIRP_PASSWORD` |
| Storytel | `TEST_STORYTEL_EMAIL` / `TEST_STORYTEL_PASSWORD` | *(not wired yet)* |
| Audiobooks.com | `TEST_ABC_EMAIL` / `TEST_ABC_PASSWORD` | *(not wired yet)* |
| Kobo | — | Browser ActivateOnWeb |
| Downpour / Podimo | `TEST_DOWNPOUR_*` / `TEST_PODIMO_*` | Deferred |
| LibriVox | — | — |

```bash
export LIBATION_GA_PASSWORD=…
libation auth login --source graphicaudio --email you@example.com
export LIBATION_CHIRP_PASSWORD=…
libation auth login --source chirp --email you@example.com
```

Do **not** put passwords on argv. Prefer cheap/owned libraries for GA/Chirp;
Storytel/ABC are subscription models — use throwaway accounts and do not
bulk-download.

## Preference: API over cookies

Probes target mobile/store APIs discovered from APKs and community clients:

- GraphicAudio Retrofit (`/access/activation/login`, `/access/api/…`)
- Chirp GraphQL (`signIn`, library queries) — not session cookies
- Storytel Android `login.action` + JWT bookshelf
- Audiobooks.com `/api/v2/` form posts + APK `apiKey`
- Kobo `storeapi.kobo.com` device auth + init resources

Web cookie scrapers (Chirp `ci_session`, ABC HTML stream pages) are fallbacks
only if an API path dies.
