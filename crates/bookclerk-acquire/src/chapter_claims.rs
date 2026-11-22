//! Chapter claims an acquire records for the edition it stored
//! (`docs/adr/edition-chapters.md`; the library resolves
//! them in `bookclerk_library::LibraryStore::record_chapter_claim`).
//!
//! An acquire already computes every list it might write into the audio:
//! the markers the store returned, the part titles it packaged, the layout
//! a metadata provider answered, and the provider's chapters aligned to
//! this audio. Each becomes a claim from its own source, so the library
//! keeps all of them and the resolver decides which one the edition plays.
//! Recording is best effort: a failure is logged and never fails the
//! acquire, because the chapters are already in the files.

use bookclerk_enrich::ProvidedChapters;
use bookclerk_library::{
    ChapterCheck, ChapterClaimWrite, ChapterInput, ChapterList, ChapterSource, LibraryStore,
    MarkerKind, ProviderQuality,
};
use bookclerk_source::ChapterKind;
use serde_json::Value;

/// Every chapter list one acquire computed for its edition.
#[derive(Debug, Default)]
pub(crate) struct ChapterEvidence {
    /// The storefront's PluginKey.
    pub plugin_key: String,
    /// The markers the store returned with the audio, and what it says
    /// they mark.
    pub listing: Option<(Vec<(String, u64)>, ChapterKind)>,
    /// One chapter per delivered part, titled as the store titled it,
    /// when the store returned no markers.
    pub part_names: Option<Vec<(String, u64)>>,
    /// The layout a metadata provider answered, on the provider's own
    /// timeline.
    pub provider: Option<ProvidedChapters>,
    /// The provider's chapter tree rebased onto this audio and aligned to
    /// the spoken chapter titles.
    pub processed: Option<Value>,
    /// Length of the audio the store, part and processed lists were
    /// measured on, in milliseconds.
    pub runtime_ms: Option<u64>,
}

/// A flat list of `(title, start)` as lenient chapter input at depth 0.
fn flat_input(chapters: &[(String, u64)]) -> Vec<ChapterInput> {
    chapters
        .iter()
        .map(|(title, start_ms)| ChapterInput {
            title: title.clone(),
            start_ms: *start_ms,
            depth: 0,
        })
        .collect()
}

/// A chapter tree (`chapters` arrays of nodes with `title` and
/// `startOffsetMs` / `start_offset_ms`) as pre-order input with depths.
/// A node without a start is left out with its children.
pub(crate) fn tree_input(tree: &Value) -> Vec<ChapterInput> {
    /// Appends `nodes` and their children at `depth`.
    fn walk(nodes: &[Value], depth: u32, out: &mut Vec<ChapterInput>) {
        for node in nodes {
            let start = node
                .get("startOffsetMs")
                .and_then(Value::as_u64)
                .or_else(|| node.get("start_offset_ms").and_then(Value::as_u64));
            let Some(start_ms) = start else { continue };
            out.push(ChapterInput {
                title: node
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                start_ms,
                depth,
            });
            if let Some(children) = node.get("chapters").and_then(Value::as_array) {
                walk(children, depth.saturating_add(1), out);
            }
        }
    }
    let mut out = Vec::new();
    if let Some(roots) = tree.get("chapters").and_then(Value::as_array) {
        walk(roots, 0, &mut out);
    }
    out
}

/// The claims `evidence` makes, in the order they are recorded: the
/// provider's comes before the processed one, which names it as its input.
/// A list that fails its checks is left out.
pub(crate) fn chapter_claims(evidence: &ChapterEvidence) -> Vec<ChapterClaimWrite> {
    let lenient = |input| ChapterList::new(input, ChapterCheck::Lenient).ok();
    let mut claims = Vec::new();
    if let Some(list) = evidence
        .listing
        .as_ref()
        .and_then(|(chapters, _)| lenient(flat_input(chapters)))
    {
        let kind = match evidence.listing.as_ref().map(|(_, kind)| *kind) {
            Some(ChapterKind::Chapters) => MarkerKind::Chapters,
            Some(ChapterKind::Tracks) => MarkerKind::Tracks,
            Some(ChapterKind::Unknown) | None => MarkerKind::Unknown,
        };
        claims.push(ChapterClaimWrite {
            source: ChapterSource::Listing {
                plugin: evidence.plugin_key.clone(),
                kind,
            },
            list,
            runtime_ms: evidence.runtime_ms,
        });
    }
    if let Some(list) = evidence
        .part_names
        .as_deref()
        .and_then(|parts| lenient(flat_input(parts)))
    {
        claims.push(ChapterClaimWrite {
            source: ChapterSource::PartNames {
                plugin: evidence.plugin_key.clone(),
            },
            list,
            runtime_ms: evidence.runtime_ms,
        });
    }
    let provider = evidence.provider.as_ref().and_then(|found| {
        let input = found
            .chapters
            .chapters
            .iter()
            .map(|c| ChapterInput {
                title: c.title.clone(),
                start_ms: c.start_ms,
                depth: c.depth,
            })
            .collect();
        lenient(input).map(|list| (found, list))
    });
    let input_digest = provider.as_ref().map(|(_, list)| list.digest());
    if let Some((found, list)) = provider {
        claims.push(ChapterClaimWrite {
            source: ChapterSource::Provider {
                plugin: found.provider.clone(),
                // The pipeline asks providers by the edition's ASIN, a
                // strong identifier.
                quality: if found.chapters.accurate {
                    ProviderQuality::StrongAccurate
                } else {
                    ProviderQuality::StrongEstimated
                },
            },
            list,
            runtime_ms: found.chapters.runtime_ms,
        });
    }
    if let Some(list) = evidence
        .processed
        .as_ref()
        .and_then(|tree| lenient(tree_input(tree)))
    {
        claims.push(ChapterClaimWrite {
            source: ChapterSource::Processed {
                input: input_digest,
            },
            list,
            runtime_ms: evidence.runtime_ms,
        });
    }
    claims.retain(|claim| !claim.list.is_empty());
    claims
}

/// Records every claim `evidence` makes for `edition_id`. Failures are
/// logged, never returned.
pub(crate) async fn record_chapter_claims(
    library: &LibraryStore,
    edition_id: &str,
    evidence: &ChapterEvidence,
) {
    for claim in chapter_claims(evidence) {
        if let Err(err) = library.record_chapter_claim(edition_id, &claim).await {
            tracing::warn!(
                edition = %edition_id,
                error = %err,
                "could not record a chapter claim; the files keep their chapters"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::ChapterSourceKind;
    use bookclerk_plugin_abi::{MetadataChapter, MetadataChapters};

    /// A provider layout of two parts with a chapter each.
    fn layout() -> ProvidedChapters {
        let chapter = |title: &str, start_ms, depth| MetadataChapter {
            title: title.into(),
            start_ms,
            depth,
            ..MetadataChapter::default()
        };
        ProvidedChapters {
            provider: "plugin:audnexus".into(),
            chapters: MetadataChapters {
                found: true,
                chapters: vec![
                    chapter("Part One", 0, 0),
                    chapter("Chapter 1", 2_000, 1),
                    chapter("Part Two", 60_000, 0),
                ],
                runtime_ms: Some(120_000),
                accurate: true,
                ..MetadataChapters::default()
            },
        }
    }

    #[test]
    fn a_tree_reads_in_pre_order_with_depths() {
        let tree = serde_json::json!({ "chapters": [
            { "title": "Part One", "startOffsetMs": 0, "chapters": [
                { "title": "Chapter 1", "start_offset_ms": 1_500 },
                { "title": "No start" }
            ]},
            { "title": "Part Two", "startOffsetMs": 59_000 }
        ]});
        let input = tree_input(&tree);
        let flat: Vec<_> = input
            .iter()
            .map(|c| (c.title.as_str(), c.start_ms, c.depth))
            .collect();
        assert_eq!(
            flat,
            [
                ("Part One", 0, 0),
                ("Chapter 1", 1_500, 1),
                ("Part Two", 59_000, 0)
            ]
        );
    }

    #[test]
    fn an_acquire_claims_each_list_from_its_own_source() {
        let evidence = ChapterEvidence {
            plugin_key: "plugin:libro".into(),
            listing: Some((
                vec![("Track 1".into(), 0), ("Track 2".into(), 60_000)],
                ChapterKind::Tracks,
            )),
            part_names: None,
            provider: Some(layout()),
            processed: Some(serde_json::json!({ "chapters": [
                { "title": "Part One", "startOffsetMs": 0, "chapters": [
                    { "title": "Chapter 1", "startOffsetMs": 1_800 }
                ]},
                { "title": "Part Two", "startOffsetMs": 58_400 }
            ]})),
            runtime_ms: Some(118_000),
        };
        let claims = chapter_claims(&evidence);
        let kinds: Vec<_> = claims
            .iter()
            .map(|c| match &c.source {
                ChapterSource::Listing { kind, .. } => (ChapterSourceKind::Listing, Some(*kind)),
                ChapterSource::Provider { .. } => (ChapterSourceKind::Provider, None),
                ChapterSource::Processed { .. } => (ChapterSourceKind::Processed, None),
                ChapterSource::PartNames { .. } => (ChapterSourceKind::PartNames, None),
                ChapterSource::Person { .. } => (ChapterSourceKind::Person, None),
                ChapterSource::File { .. } => (ChapterSourceKind::File, None),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                (ChapterSourceKind::Listing, Some(MarkerKind::Tracks)),
                (ChapterSourceKind::Provider, None),
                (ChapterSourceKind::Processed, None),
            ]
        );
        let provider_digest = claims[1].list.digest();
        assert_eq!(
            claims[1].runtime_ms,
            Some(120_000),
            "the provider's timeline"
        );
        assert_eq!(
            claims[2].source,
            ChapterSource::Processed {
                input: Some(provider_digest)
            }
        );
        assert_eq!(claims[2].runtime_ms, Some(118_000), "this audio's timeline");
        assert_eq!(claims[2].list.chapters()[1].depth, 1);
    }

    #[test]
    fn empty_and_unusable_lists_make_no_claim() {
        let evidence = ChapterEvidence {
            plugin_key: "plugin:graphicaudio".into(),
            listing: Some((Vec::new(), ChapterKind::Unknown)),
            part_names: Some(vec![("B".into(), 10), ("A".into(), 5)]),
            ..ChapterEvidence::default()
        };
        assert!(chapter_claims(&evidence).is_empty());
    }
}
