# PR1 parity: libation-rs vs Libation Chardonnay (headless)

**Goal:** Full feature parity with [Libation Chardonnay](https://github.com/rmcrackan/Libation) for headless/CLI/daemon use. **GUI is deferred** until after PR1.

Chardonnay and Classic share the same backend; this matrix uses upstream `master` / LibationCli + `Configuration.PersistentSettings.cs` as the reference.

## Verdict

**Headless parity with Classic/Chardonnay is complete for the LibationCli + liberate surface.** All LibationCli verbs, download/decrypt settings that affect headless operation, naming templates, podcast handling, library metadata, migrate-from-classic, and classic EF Postgres `copydb` are implemented.

Remaining items are either **GUI-only**, **intentionally deferred** (upgrade checks), or **minor headless polish** listed under [Still open (non-GUI)](#still-open-non-gui).

## Status legend

| Symbol | Meaning |
| --- | --- |
| ✅ | Implemented |
| ⚠️ | Partial / subset |
| ❌ | Not yet implemented |
| 🚫 | Out of PR1 scope (GUI-only) |

---

## CLI verbs (LibationCli)

| Verb | Status | libation-rs |
| --- | --- | --- |
| `scan` | ✅ | `library scan` |
| `liberate` | ✅ | `library liberate` — `--pdf`, `--license`, `-o` overrides, positional ASINs |
| `set-status` | ✅ | `library set-status` — `--downloaded` / `--not-downloaded` / `--force` + ASINs |
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
| `version` | ✅ | `libation version` |
| `version --check` | 🚫 | Intentionally deferred (no upgrade checks in PR1) |
| `help` | ✅ | clap |
| Progress bar (`liberate` / `convert`) | ✅ | TTY batch progress with ETA |
| Template tag list / preview | ✅ | `config template tags` / `config template preview <asin>` |

---

## Settings.json → config.toml

| Classic key | Status | libation-rs key |
| --- | --- | --- |
| `Books` | ✅ | `storage.local.root` |
| `FileDownloadQuality` | ✅ | `download.quality` |
| `DecryptToLossy` | ✅ | `download.format` |
| `UseWidevine` | ✅ | `download.widevine` |
| `Request_xHE_AAC` | ✅ | `download.xhe_aac` |
| `FolderTemplate` / `FileTemplate` | ✅ | Chardonnay naming engine (`libation-naming`) + `config template preview` |
| `DownloadCoverArt` | ✅ | `download.download_cover` |
| `CreateCueSheet` | ✅ | `download.create_cue` |
| `AllowLibationFixup` | ✅ | `download.fixup_metadata` |
| `SaveMetadataToFile` | ✅ | `download.save_metadata_json` |
| `AutoDownloadEpisodes` | ✅ | `library.auto_liberate` |
| `AutoScan` | ✅ | `library.scan_interval_minutes` |
| `OverwriteExisting` | ✅ | `download.overwrite_existing` |
| `InProgress` | ✅ | `download.in_progress` |
| `ImportEpisodes` | ✅ | `library.import_episodes` |
| `ImportPlusTitles` | ✅ | `library.import_plus_titles` |
| `DownloadEpisodes` | ✅ | `library.download_episodes` |
| `BadBook` | ✅ | `download.bad_book_action` |
| `SplitFilesByChapter` | ✅ | `download.split_files_by_chapter` |
| `ChapterFileTemplate` / `ChapterTitleTemplate` | ✅ | `download.chapter_file_template` / `chapter_title_template` |
| `MinimumFileDuration` | ✅ | `download.minimum_file_duration_minutes` |
| `CombineNestedChapterTitles` | ✅ | `download.combine_nested_chapter_titles` |
| `MergeOpeningAndEndCredits` | ✅ | `download.merge_opening_and_end_credits` |
| `StripUnabridged` / `StripAudibleBrandAudio` | ✅ | `download.strip_unabridged` / `strip_audible_brand_audio` |
| `DownloadClipsBookmarks` | ✅ | `download.download_clips_bookmarks` (JSON sidecar) |
| `ClipsBookmarksFileFormat` | ⚠️ | Always JSON; classic also offers CSV/XLSX |
| `RetainAaxFile` | ✅ | `download.retain_aax_file` |
| `DownloadSpeedLimit` | ✅ | `download.download_speed_limit_kbps` |
| `Lame*` | ✅ | `download.lame.*` (target/quality/bitrate/mode/downsample/CBR) |
| `LameMatchSourceBR` | ⚠️ | Not a separate toggle; bitrate path uses configured kbps |
| `MoveMoovToBeginning` | ⚠️ | Always on for aaxclean/ffmpeg paths (`--moov_faststart`) |
| `ReplacementCharacters` | ✅ | `download.replacement_characters` |
| `MaxSampleRate` | ✅ | `download.max_sample_rate` |
| `CreationTime` / `LastWriteTime` | ✅ | `download.creation_time` / `last_write_time` (local + S3 object metadata) |
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
| Podcast parents skipped on liberate | ✅ classic WithoutParents |
| Podcast episode parent-folder naming | ✅ SavePodcastsToParentFolder |
| Lucene/Tantivy search index | ✅ |
| Saved quick filters | ✅ `library filters` list/save/delete |

---

## Still open (non-GUI)

| Item | Notes |
| --- | --- |
| Upgrade checks (`version --check`) | Intentionally deferred for PR1 |
| Encrypted auth-file password prompt | Loader accepts a password; interactive CLI prompt not wired |
| Clips/bookmarks export format | JSON only (classic CSV/XLSX options unused in headless path) |
| `MoveMoovToBeginning` / `LameMatchSourceBR` toggles | Behavior covered by defaults / always-on moov_faststart |
| Exotic naming edge cases | Rare TimeSpan masks / locale-specific number formats |
| Per-byte download progress | Batch title progress + ETA on liberate/convert; classic also draws a byte bar mid-download |

---

## Explicitly out of PR1 (GUI)

| Item | Reason |
| --- | --- |
| Chardonnay / Classic GUI (WinForms / Avalonia) | Deferred post-PR1 |
| Password + CAPTCHA WebView login | GUI-only; OAuth headless paths cover CLI |
| Hangover trash-bin / deleted-title recovery UI | Separate classic tool / GUI |
| Series view, find-better-quality, theme, grid layout | GUI-only |

---

## Beyond classic (libation-rs extras)

| Capability | Notes |
| --- | --- |
| S3 / MinIO storage | Not in classic Libation |
| `libationd` daemon + HTTP control plane | Scheduled scan / auto-liberate |
| TOML config + env overrides | Classic uses `Settings.json` |
| QR / local callback-server login | Extra headless login modes beyond `login-external` |
| `config template tags` / `preview` | Headless template tooling |
