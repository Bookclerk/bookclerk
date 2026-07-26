# Libation headless parity (reference)

Bookclerk is a **multi-storefront** library manager. This page is a
**compatibility reference** for the Audible / classic Libation headless surface
— not a product roadmap. For day-to-day docs start at [README.md](README.md).

**Historical goal (PR1):** Full feature parity with
[Libation Chardonnay](https://github.com/rmcrackan/Libation) for
headless/CLI/daemon use. A Bookclerk-native web GUI (React, served by
`bookclerkd`) is tracked in [gui.md](gui.md) and is not a WinForms/Avalonia
port. Native desktop/tray is deferred.

Chardonnay and Classic share the same backend; this matrix uses upstream
`master` / LibationCli + `Configuration.PersistentSettings.cs` as the reference.

## Verdict

**Headless parity with Classic/Chardonnay is complete for the LibationCli +
acquire surface.** All LibationCli verbs, download/decrypt settings that affect
headless operation, naming templates, podcast handling, library metadata,
migrate-from-classic, and classic EF Postgres `copydb` are implemented.

Remaining items are either **GUI-only**, **intentionally deferred** (upgrade
checks), or **minor headless polish** listed under
[Still open (non-GUI)](#still-open-non-gui).

## Status legend

| Symbol | Meaning |
| --- | --- |
| ✅ | Implemented |
| ⚠️ | Partial / subset |
| ❌ | Not yet implemented |
| 🚫 | Out of PR1 scope (GUI-only) |

---

## CLI verbs (LibationCli)

| Verb | Status | bookclerk |
| --- | --- | --- |
| `scan` | ✅ | `library scan` |
| `liberate` | ✅ | `library acquire` (Bookclerk verb) — `--pdf`, `--license`, `-o` overrides, positional ASINs |
| `set-status` | ✅ | `library set-status` — probe-based match; `--downloaded` / `--not-downloaded` / `--force` / `--fix-layout` + ASINs |
| `get-license` | ✅ | `library get-license --json` (summary) / `--full` (raw API JSON) |
| `search` | ✅ | `library search` (+ `--filter` saved quick filters) |
| `export` | ✅ | `library export --csv|--json|--xlsx` |
| `convert` | ✅ | `library convert` (m4b/m4a → mp3) |
| `import-account` | ✅ | `auth import --mkb79` |
| `login-external` | ✅ | `auth login --external` |
| `list-accounts` | ✅ | `auth list` (`--bare`: account, name, locale, scan, auth) |
| `set-scan` | ✅ | `auth set-scan <account> [--scan true\|false]` (GUI checkbox; CLI addition) |
| `get-setting` | ✅ | `config get/show/paths` (classic key aliases + `--bare`) |
| `copydb` | ✅ | `copydb` — classic Libation EF Postgres schema (default); `--format flat` for native |
| `version` | ✅ | `bookclerk version` |
| `version --check` | 🚫 | Intentionally deferred (no upgrade checks in PR1) |
| `help` | ✅ | clap |
| Progress bar (`acquire` / `convert`) | ✅ | TTY batch progress with ETA |
| Template tag list / preview | ✅ | `config template tags` / `config template preview <asin>` |
| Naming profiles | ✅ | `output.naming_profile` (`audiobookshelf` default, `classic`); `config template profiles` |

---

## Settings.json → config.toml

| Classic key | Status | bookclerk key |
| --- | --- | --- |
| `Books` | ✅ | `output.local.root` |
| `FileDownloadQuality` | ✅ | `sources.audible.bitrate` |
| `DecryptToLossy` | ✅ | `output.format = "single_mp3"` or `"enriched_m4b"` |
| `UseWidevine` | ✅ | `output.widevine` |
| `Request_xHE_AAC` | ✅ | `output.xhe_aac` |
| `FolderTemplate` / `FileTemplate` | ✅ | Libation NamingTemplate port (`bookclerk-naming`) + `config template preview`; defaults from `naming_profile` |
| `MaxFilenameLength` (255) | ✅ | `output.max_filename_length` + S3 full-key budget in acquire naming |
| `DownloadCoverArt` | ✅ | `output.download_cover` |
| `CreateCueSheet` | ✅ | `output.create_cue` |
| `AllowLibationFixup` | ✅ | `output.fixup_metadata` |
| `SaveMetadataToFile` | ✅ | `output.save_metadata_json` |
| `AutoDownloadEpisodes` | ✅ | `library.auto_acquire` |
| `AutoScan` | ✅ | `library.scan_interval_minutes` |
| `OverwriteExisting` | ✅ | `output.overwrite_existing` |
| `InProgress` | ✅ | `output.in_progress` |
| `ImportEpisodes` | ✅ | `library.import_episodes` |
| `ImportPlusTitles` | ✅ | `library.import_plus_titles` |
| `DownloadEpisodes` | ✅ | `library.download_episodes` |
| `BadBook` | ✅ | `output.bad_book_action` |
| `SplitFilesByChapter` | ✅ | `output.format = "split_mp3_by_chapter"` |
| `ChapterFileTemplate` / `ChapterTitleTemplate` | ✅ | `output.chapter_file_template` / `chapter_title_template` |
| `MinimumFileDuration` | ✅ | `output.minimum_file_duration_minutes` |
| `CombineNestedChapterTitles` | ✅ | `output.combine_nested_chapter_titles` |
| `MergeOpeningAndEndCredits` | ✅ | `output.merge_opening_and_end_credits` |
| `StripUnabridged` / `StripAudibleBrandAudio` | ✅ | `output.strip_unabridged` / `strip_audible_brand_audio` (brand audio is trimmed from the media using chapter `brand_intro`/`brand_outro` durations; titles are scrubbed too) |
| `DownloadClipsBookmarks` | ✅ | `output.download_clips_bookmarks` (JSON sidecar) |
| `ClipsBookmarksFileFormat` | ⚠️ | Always JSON; classic also offers CSV/XLSX |
| `RetainAaxFile` | ✅ | `output.retain_aax_file` |
| `DownloadSpeedLimit` | ✅ | `output.download_speed_limit_kbps` |
| `Lame*` | ✅ | `output.lame.*` (target/quality/bitrate/mode/downsample/CBR) |
| `LameMatchSourceBR` | ⚠️ | Not a separate toggle; bitrate path uses configured kbps |
| `MoveMoovToBeginning` | ⚠️ | Always on for native remux (`moov` faststart) |
| `ReplacementCharacters` | ✅ | `output.replacement_characters` (explicit) or `output.path_sanitization` (`auto`/`windows`/`posix`/`s3`/`none`) |
| `MaxSampleRate` | ✅ | `output.max_sample_rate` |
| `CreationTime` / `LastWriteTime` | ✅ | `output.creation_time` / `last_write_time` (local + S3 object metadata) |
| `SavePodcastsToParentFolder` | ✅ | `library.save_podcasts_to_parent_folder` |
| `RequestSpatial` / `SpatialAudioCodec` | 🚫 | Stubbed/`false` in upstream classic today |
| Theme / Grid* / Column layout | 🚫 | GUI-only |
| `UseCoverAsFolderIcon` | 🚫 | GUI/OS folder-icon behavior |
| `UseWebView` / `BetaOptIn` / `ShowImportedStats` / `FirstLaunch` | 🚫 | GUI-only |

---

## Library / user metadata

| Field | Status |
| --- | --- |
| Tags | ✅ DB + search index + migrate import |
| User ratings (overall/performance/story) | ✅ DB + migrate import |
| Finished flag | ✅ DB + migrate import |
| Separate PDF status | ✅ |
| Publisher, length, categories, subtitle, published_at, content_kind, series_asin | ✅ scan + DB |
| Podcast parents skipped on acquire | ✅ classic WithoutParents |
| Podcast episode parent-folder naming | ✅ SavePodcastsToParentFolder |
| Lucene/Tantivy search index | ✅ |
| Saved quick filters | ✅ `library filters` list/save/delete |

---

## Still open (non-GUI)

| Item | Notes |
| --- | --- |
| Upgrade checks (`version --check`) | Intentionally deferred for PR1 |
| Clips/bookmarks export format | JSON only (classic CSV/XLSX options unused in headless path) |
| `MoveMoovToBeginning` / `LameMatchSourceBR` toggles | Behavior covered by defaults / always-on moov_faststart |
| Exotic naming edge cases | Rare TimeSpan masks / locale-specific number formats |
| Per-byte download progress | Batch title progress + ETA on acquire/convert; classic also draws a byte bar mid-download |

---

## Explicitly out of PR1 (GUI)

| Item | Reason |
| --- | --- |
| Chardonnay / Classic GUI (WinForms / Avalonia) | Not planned — Bookclerk ships a web GUI ([gui.md](gui.md)); native/tray deferred |
| Password + CAPTCHA WebView login | GUI-only; OAuth headless paths cover CLI |
| Hangover trash-bin / deleted-title recovery UI | Separate classic tool / GUI |
| Series view, find-better-quality, theme, grid layout | GUI-only |

---

## Beyond classic (Bookclerk extras)

| Capability | Notes |
| --- | --- |
| Multi-storefront sources | Libro.fm, Chirp, GraphicAudio (+ plugin sources) |
| Pluggable integrations | Audiobookshelf, Connect portal, external JSON-RPC plugins |
| Multi-destination output | Local + S3/MinIO simultaneously |
| S3 / MinIO storage | Not in classic Libation |
| `bookclerkd` daemon + HTTP control plane | Scheduled scan / auto-acquire |
| TOML config + env overrides | Classic uses `Settings.json` |
| QR / local callback-server login | Extra headless login modes beyond `login-external` |
| `config template tags` / `profiles` / `preview` | Headless template tooling |

---

## Post-PR1 (planned)

| Item | Notes |
| --- | --- |
| Spatial / Atmos (Widevine L1) | Hardware TEE only; desktop L3 cannot satisfy L1 license grants |
| GUI | MVP React web UI ([gui.md](gui.md)); native/tray deferred; not Avalonia |
