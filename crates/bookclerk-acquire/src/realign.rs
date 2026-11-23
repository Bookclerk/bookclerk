//! Aligning a metadata provider's chapter layout to a stored copy's audio
//! (`docs/adr/edition-chapters.md`, decision 3).
//!
//! An acquire aligns the provider's layout while it still holds the audio.
//! A copy that arrived another way (an upload), or whose provider layout is
//! asked for again, is aligned here instead: the provider is asked by the
//! edition's ASIN, its layout is placed on the copy's timeline (with or
//! without the store's brand intro and outro, whichever the copy's length
//! matches), each chapter start is snapped to the spoken title in the file
//! it falls in, and the provider's layout and the aligned tree are recorded
//! as the edition's provider and processed claims.

use std::collections::HashMap;
use std::path::PathBuf;

use bookclerk_enrich::{asin_identifier, chapter_info_from_layout, MetadataProviders};
use bookclerk_library::LibraryStore;
use bookclerk_media::{
    align_chapter_starts_async, brand_durations_from_chapter_info,
    runtime_length_ms_from_chapter_info, scale_chapters_to_duration, ChapterAlignOptions,
};
use bookclerk_plugin_abi::MetadataChaptersParams;
use bookclerk_source::DownloadOptions;
use serde_json::Value;

use crate::chapter_claims::{record_chapter_claims, ChapterEvidence};
use crate::cue::{
    apply_start_map_to_chapter_tree, chapters_from_catalog_info_for_plain_audio, flatten_chapters,
    process_chapter_titles, rebase_chapter_tree_for_plain_audio,
};

/// One audio file of a stored copy, read to local disk, in play order.
#[derive(Debug, Clone)]
pub struct CopyAudio {
    /// Where the file's bytes are on this node.
    pub path: PathBuf,
    /// Its running time in milliseconds, as its probe recorded it.
    pub duration_ms: u64,
}

/// The edition a copy belongs to and how its provider is asked.
#[derive(Debug, Clone, Copy)]
pub struct RealignTarget<'a> {
    /// The edition whose claims are recorded.
    pub edition_id: &'a str,
    /// The edition's ASIN, the identifier providers are asked by.
    pub asin: &'a str,
    /// The ASIN's marketplace (`us`, `uk`, …).
    pub marketplace: &'a str,
}

/// What [`realign_copy`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealignOutcome {
    /// No provider had a layout for the ASIN, or its layout had no
    /// chapters on the copy's timeline; nothing was recorded.
    NoLayout,
    /// The copy has no audio with a known running time; nothing was asked.
    NoAudio,
    /// The provider's layout and the aligned list were recorded.
    Recorded {
        /// PluginKey of the provider whose layout was aligned.
        provider: String,
        /// How many chapters the aligned flat list holds.
        chapters: usize,
    },
}

/// Asks the providers for the layout of `target` and records it, aligned
/// to `files`, as the edition's provider and processed claims.
///
/// # Arguments
///
/// * `library` - Where the claims are recorded.
/// * `providers` - The deployed metadata providers.
/// * `target` - The edition and the ASIN to ask by.
/// * `files` - The copy's audio files, in play order, on local disk.
/// * `options` - The title clean-ups the copy's destination applies.
///
/// # Returns
///
/// What was recorded. Recording itself is best effort, as at acquire: a
/// claim the library refuses is logged.
pub async fn realign_copy(
    library: &LibraryStore,
    providers: &dyn MetadataProviders,
    target: RealignTarget<'_>,
    files: &[CopyAudio],
    options: &DownloadOptions,
) -> RealignOutcome {
    let copy_ms = files
        .iter()
        .fold(0u64, |sum, file| sum.saturating_add(file.duration_ms));
    if files.is_empty() || files.iter().any(|file| file.duration_ms == 0) {
        return RealignOutcome::NoAudio;
    }
    let marketplace = if target.marketplace.trim().is_empty() {
        "us"
    } else {
        target.marketplace
    };
    let answer = providers
        .chapters(&MetadataChaptersParams {
            identifiers: vec![asin_identifier(target.asin, marketplace)],
            marketplace: Some(marketplace.to_string()),
        })
        .await;
    for failure in &answer.failures {
        tracing::warn!(
            edition = %target.edition_id,
            provider = %failure.provider,
            error = %failure.message,
            "chapter layout lookup failed for re-align"
        );
    }
    let Some(found) = answer.found else {
        return RealignOutcome::NoLayout;
    };
    let info = chapter_info_from_layout(&found.chapters);
    let (flat, tree) = copy_timeline(&info, copy_ms, options);
    if flat.is_empty() {
        return RealignOutcome::NoLayout;
    }
    let aligned = align_across_files(files, &flat).await;
    let start_map: HashMap<u64, u64> = flat
        .iter()
        .zip(&aligned)
        .map(|((_, old), (_, new))| (*old, *new))
        .collect();
    let provider = found.provider.clone();
    let evidence = ChapterEvidence {
        provider: Some(found),
        processed: Some(apply_start_map_to_chapter_tree(&tree, &start_map)),
        runtime_ms: Some(copy_ms),
        ..ChapterEvidence::default()
    };
    record_chapter_claims(library, target.edition_id, &evidence).await;
    RealignOutcome::Recorded {
        provider,
        chapters: aligned.len(),
    }
}

/// The provider's layout `info` placed on a copy `copy_ms` long: the flat
/// list to align and the tree the aligned starts are written back into.
///
/// A copy whose length is nearer the layout's full runtime than its runtime
/// without the brand intro and outro still carries them (a store's own
/// download), so the layout's starts stand and are only scaled to the copy;
/// any other copy is treated as plain audio, as an acquire from another
/// store is: the intro is cut from every start and the outro's chapters
/// dropped.
pub(crate) fn copy_timeline(
    info: &Value,
    copy_ms: u64,
    options: &DownloadOptions,
) -> (Vec<(String, u64)>, Value) {
    let brand = brand_durations_from_chapter_info(info);
    let full = runtime_length_ms_from_chapter_info(info);
    let branded = full.is_some_and(|full| {
        let plain = full
            .saturating_sub(brand.intro_ms)
            .saturating_sub(brand.outro_ms);
        !brand.is_empty() && full.abs_diff(copy_ms) < plain.abs_diff(copy_ms)
    });
    if branded {
        let pairs: Vec<(String, u64)> = process_chapter_titles(
            flatten_chapters(info),
            options.combine_nested_chapter_titles,
            options.merge_opening_and_end_credits,
            options.strip_unabridged,
            options.strip_audible_brand_audio,
        )
        .into_iter()
        .map(|chapter| (chapter.title, chapter.start_ms))
        .collect();
        let scaled = scale_chapters_to_duration(&pairs, full, Some(copy_ms));
        // Every node in the tree moves by the same scale, including those
        // the title clean-ups dropped from the list to align, so none keeps
        // a start on the provider's timeline.
        let mut starts = Vec::new();
        tree_starts(info.get("chapters"), &mut starts);
        let every: Vec<(String, u64)> = starts
            .into_iter()
            .map(|start| (String::new(), start))
            .collect();
        let mut map: HashMap<u64, u64> = every
            .iter()
            .zip(&scale_chapters_to_duration(&every, full, Some(copy_ms)))
            .map(|((_, old), (_, new))| (*old, *new))
            .collect();
        map.extend(
            pairs
                .iter()
                .zip(&scaled)
                .map(|((_, old), (_, new))| (*old, *new)),
        );
        (scaled, apply_start_map_to_chapter_tree(info, &map))
    } else {
        (
            chapters_from_catalog_info_for_plain_audio(
                info,
                options.combine_nested_chapter_titles,
                options.merge_opening_and_end_credits,
                options.strip_unabridged,
                options.strip_audible_brand_audio,
                Some(copy_ms),
            ),
            rebase_chapter_tree_for_plain_audio(info, Some(copy_ms)),
        )
    }
}

/// Collects the start of every node under `nodes` (a `chapters` array),
/// nested ones included and whatever its title, in the spellings
/// [`apply_start_map_to_chapter_tree`] rewrites.
fn tree_starts(nodes: Option<&Value>, out: &mut Vec<u64>) {
    for node in nodes.and_then(Value::as_array).into_iter().flatten() {
        tree_starts(node.get("chapters"), out);
        if let Some(start) = node
            .get("start_offset_ms")
            .and_then(Value::as_u64)
            .or_else(|| node.get("startOffsetMs").and_then(Value::as_u64))
        {
            out.push(start);
        }
    }
}

/// Snaps each chapter of `chapters` (on the copy's timeline) to the spoken
/// title in the file it starts in, and returns the list in the same order.
///
/// A chapter that starts exactly where a file starts stays there. A file
/// the aligner cannot read keeps its chapters' starts.
async fn align_across_files(files: &[CopyAudio], chapters: &[(String, u64)]) -> Vec<(String, u64)> {
    let mut out = chapters.to_vec();
    let mut offset = 0u64;
    for file in files {
        let end = offset.saturating_add(file.duration_ms);
        let inside: Vec<usize> = (0..out.len())
            .filter(|&i| out[i].1 > offset && out[i].1 < end)
            .collect();
        if !inside.is_empty() {
            // The aligner leaves a chapter at 0 alone and needs two
            // chapters, so the file's own start leads the list.
            let mut local = vec![(String::new(), 0)];
            local.extend(
                inside
                    .iter()
                    .map(|&i| (out[i].0.clone(), out[i].1.saturating_sub(offset))),
            );
            let aligned =
                align_chapter_starts_async(&file.path, &local, ChapterAlignOptions::default())
                    .await;
            if aligned.len() == local.len() {
                for (&i, (_, start)) in inside.iter().zip(aligned.iter().skip(1)) {
                    out[i].1 = offset.saturating_add(*start).min(end.saturating_sub(1));
                }
            }
        }
        offset = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A layout with a 2 s brand intro, a 3 s outro and three chapters on a
    /// 100 s timeline.
    fn layout() -> Value {
        json!({
            "brandIntroDurationMs": 2_000,
            "brandOutroDurationMs": 3_000,
            "runtimeLengthMs": 100_000,
            "chapters": [
                {"title": "Opening Credits", "startOffsetMs": 0},
                {"title": "Chapter 1", "startOffsetMs": 10_000},
                {"title": "Chapter 2", "startOffsetMs": 50_000},
            ]
        })
    }

    #[test]
    fn a_copy_as_long_as_the_full_runtime_keeps_the_layouts_starts() {
        let (flat, tree) = copy_timeline(&layout(), 100_000, &DownloadOptions::default());
        let starts: Vec<u64> = flat.iter().map(|(_, start)| *start).collect();
        assert_eq!(starts, vec![0, 10_000, 50_000]);
        assert_eq!(tree["chapters"][2]["startOffsetMs"], json!(50_000));
    }

    #[test]
    fn a_chapter_left_out_of_the_list_still_moves_with_the_copy() {
        let mut info = layout();
        info["chapters"]
            .as_array_mut()
            .expect("chapters")
            .push(json!({"title": "Audible Studios", "startOffsetMs": 30_000}));
        info["chapters"]
            .as_array_mut()
            .expect("chapters")
            .push(json!({"title": " ", "startOffsetMs": 70_000}));
        let options = DownloadOptions {
            strip_audible_brand_audio: true,
            ..DownloadOptions::default()
        };
        let (flat, tree) = copy_timeline(&info, 99_000, &options);
        let starts: Vec<u64> = flat.iter().map(|(_, start)| *start).collect();
        assert_eq!(starts, vec![0, 9_900, 49_500]);
        assert_eq!(tree["chapters"][3]["startOffsetMs"], json!(29_700));
        assert_eq!(tree["chapters"][4]["startOffsetMs"], json!(69_300));
    }

    #[test]
    fn a_copy_without_the_brand_audio_has_the_intro_cut_from_every_start() {
        let (flat, tree) = copy_timeline(&layout(), 95_000, &DownloadOptions::default());
        let starts: Vec<u64> = flat.iter().map(|(_, start)| *start).collect();
        assert_eq!(starts, vec![0, 8_000, 48_000]);
        assert_eq!(tree["chapters"][1]["startOffsetMs"], json!(8_000));
    }

    /// An empty library on an in-memory database.
    async fn store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("an in-memory database"),
        )
    }

    /// Providers that answer every chapters call with one layout.
    struct OneLayout;

    #[async_trait::async_trait]
    impl MetadataProviders for OneLayout {
        async fn lookup(
            &self,
            _params: &bookclerk_plugin_abi::MetadataLookupParams,
        ) -> bookclerk_enrich::MetadataAnswer {
            bookclerk_enrich::MetadataAnswer::default()
        }

        async fn search(
            &self,
            _params: &bookclerk_plugin_abi::MetadataSearchParams,
        ) -> bookclerk_enrich::MetadataAnswer {
            bookclerk_enrich::MetadataAnswer::default()
        }

        async fn chapters(
            &self,
            params: &MetadataChaptersParams,
        ) -> bookclerk_enrich::ChaptersAnswer {
            use bookclerk_plugin_abi::{MetadataChapter, MetadataChapters};
            assert_eq!(params.identifiers[0].value, "B000000001");
            let chapter = |title: &str, start_ms| MetadataChapter {
                title: title.into(),
                start_ms,
                ..MetadataChapter::default()
            };
            bookclerk_enrich::ChaptersAnswer {
                found: Some(bookclerk_enrich::ProvidedChapters {
                    provider: "plugin:audnexus".into(),
                    chapters: MetadataChapters {
                        found: true,
                        chapters: vec![chapter("One", 0), chapter("Two", 30_000)],
                        runtime_ms: Some(60_000),
                        accurate: true,
                        ..MetadataChapters::default()
                    },
                }),
                failures: Vec::new(),
            }
        }
    }

    #[tokio::test]
    async fn a_found_layout_is_recorded_as_provider_and_processed_claims() {
        let library = store().await;
        library
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .expect("an account");
        let book = bookclerk_library::NewBook::minimal("B000000001", "acct", "us", "Title");
        let record = library.upsert_book(&book).await.expect("a book");
        let edition = library
            .edition_of_holding(&record.holding_id)
            .await
            .expect("a read")
            .expect("an edition");
        let target = RealignTarget {
            edition_id: &edition,
            asin: "B000000001",
            marketplace: "us",
        };
        // The aligner cannot read this file, so the starts stand.
        let files = [CopyAudio {
            path: PathBuf::from("/nonexistent/book.m4b"),
            duration_ms: 60_000,
        }];
        let outcome = realign_copy(
            &library,
            &OneLayout,
            target,
            &files,
            &DownloadOptions::default(),
        )
        .await;
        assert_eq!(
            outcome,
            RealignOutcome::Recorded {
                provider: "plugin:audnexus".into(),
                chapters: 2,
            }
        );
        let resolved = library
            .edition_chapters(&edition)
            .await
            .expect("a read")
            .expect("a resolved list");
        assert_eq!(
            resolved.source_kind,
            bookclerk_library::ChapterSourceKind::Processed
        );
        let starts: Vec<u64> = resolved
            .list
            .chapters()
            .iter()
            .map(|c| c.start_ms)
            .collect();
        assert_eq!(starts, vec![0, 30_000]);
        assert_eq!(resolved.runtime_ms, Some(60_000));
    }

    #[tokio::test]
    async fn no_audio_asks_no_provider() {
        let library = store().await;
        let target = RealignTarget {
            edition_id: "e",
            asin: "B000000001",
            marketplace: "",
        };
        let none = bookclerk_enrich::NoMetadataProviders;
        let options = DownloadOptions::default();
        assert_eq!(
            realign_copy(&library, &none, target, &[], &options).await,
            RealignOutcome::NoAudio
        );
        let files = [CopyAudio {
            path: PathBuf::from("/nonexistent.m4b"),
            duration_ms: 60_000,
        }];
        assert_eq!(
            realign_copy(&library, &none, target, &files, &options).await,
            RealignOutcome::NoLayout
        );
    }
}
