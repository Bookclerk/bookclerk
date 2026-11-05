//! The host's view of deployed metadata-provider plugins.
//!
//! Bibliographic lookups (Open Library, Audnexus, and any third-party
//! provider) run in plugins that export the `metadata` entrypoint. Host code
//! that needs a record asks a [`MetadataProviders`] for it; the daemon and the
//! CLI pass their integration registry, and code with no plugins passes
//! [`NoMetadataProviders`]. There is no in-process fallback: with no provider
//! deployed, every answer is empty.

use async_trait::async_trait;
use bookclerk_plugin_sdk::{
    Identifier, IdentifierScheme, MetadataChapters, MetadataChaptersParams, MetadataLookupParams,
    MetadataRecord, MetadataSearchParams,
};
use serde_json::{Map, Value};

/// One record and the provider plugin that returned it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProvidedRecord {
    /// PluginKey of the provider that answered.
    pub provider: String,
    /// The record as the provider described it.
    pub record: MetadataRecord,
}

/// A provider call that failed, so its records are missing from the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    /// PluginKey of the provider that failed.
    pub provider: String,
    /// Error text from the plugin call.
    pub message: String,
}

/// What every deployed provider answered for one lookup or search.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetadataAnswer {
    /// Records in provider order, each provider's own relevance order kept.
    pub records: Vec<ProvidedRecord>,
    /// Providers whose call failed. A caller that caches "nothing found"
    /// should not do so while this is non-empty, since the missing provider
    /// may have had a record.
    pub failures: Vec<ProviderFailure>,
}

impl MetadataAnswer {
    /// True when at least one provider failed.
    #[must_use]
    pub fn incomplete(&self) -> bool {
        !self.failures.is_empty()
    }
}

/// A chapter layout and the provider plugin that returned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvidedChapters {
    /// PluginKey of the provider that answered.
    pub provider: String,
    /// The layout; always `found`.
    pub chapters: MetadataChapters,
}

/// The first chapter layout any deployed provider had for one edition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChaptersAnswer {
    /// The first `found` layout in provider order, if any.
    pub found: Option<ProvidedChapters>,
    /// Providers whose call failed before a layout was found.
    pub failures: Vec<ProviderFailure>,
}

/// Deployed metadata providers the host asks for bibliographic records.
///
/// Implementations ask every provider in a stable order. A provider that
/// fails is reported in the answer's `failures` and does not stop the others.
#[async_trait]
pub trait MetadataProviders: Send + Sync {
    /// Records for a set of identifiers.
    ///
    /// # Arguments
    ///
    /// * `params` - Identifiers (never empty), marketplace and limit.
    ///
    /// # Returns
    ///
    /// Every provider's records plus the providers that failed.
    async fn lookup(&self, params: &MetadataLookupParams) -> MetadataAnswer;

    /// Records matching a title and author.
    ///
    /// # Arguments
    ///
    /// * `params` - Title (never empty), author, language, marketplace and limit.
    ///
    /// # Returns
    ///
    /// Every provider's records plus the providers that failed.
    async fn search(&self, params: &MetadataSearchParams) -> MetadataAnswer;

    /// The chapter layout of one edition.
    ///
    /// # Arguments
    ///
    /// * `params` - Identifiers of the edition (never empty) and marketplace.
    ///
    /// # Returns
    ///
    /// The first layout a provider found, plus the providers that failed
    /// before it.
    async fn chapters(&self, params: &MetadataChaptersParams) -> ChaptersAnswer;
}

/// No providers: every answer is empty. For callers with no plugin host.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMetadataProviders;

#[async_trait]
impl MetadataProviders for NoMetadataProviders {
    async fn lookup(&self, _params: &MetadataLookupParams) -> MetadataAnswer {
        MetadataAnswer::default()
    }

    async fn search(&self, _params: &MetadataSearchParams) -> MetadataAnswer {
        MetadataAnswer::default()
    }

    async fn chapters(&self, _params: &MetadataChaptersParams) -> ChaptersAnswer {
        ChaptersAnswer::default()
    }
}

/// An ASIN identifier scoped to `marketplace`.
///
/// # Arguments
///
/// * `asin` - The ASIN as the library stores it.
/// * `marketplace` - Marketplace code (`us`, `uk`, …); blank means none.
#[must_use]
pub fn asin_identifier(asin: &str, marketplace: &str) -> Identifier {
    let marketplace = marketplace.trim();
    Identifier {
        scheme: IdentifierScheme::Asin,
        value: asin.trim().to_ascii_uppercase(),
        marketplace: (!marketplace.is_empty()).then(|| marketplace.to_string()),
    }
}

/// An ISBN identifier.
///
/// # Arguments
///
/// * `isbn` - ISBN-10 or ISBN-13, already normalized by the caller.
#[must_use]
pub fn isbn_identifier(isbn: &str) -> Identifier {
    Identifier {
        scheme: IdentifierScheme::Isbn,
        value: isbn.trim().to_string(),
        marketplace: None,
    }
}

/// The first value of `scheme` among a record's identifiers.
///
/// # Arguments
///
/// * `record` - The provider record.
/// * `scheme` - The scheme wanted.
#[must_use]
pub fn record_identifier(record: &MetadataRecord, scheme: IdentifierScheme) -> Option<&str> {
    record
        .identifiers
        .iter()
        .filter(|id| id.scheme == scheme)
        .map(|id| id.value.trim())
        .find(|v| !v.is_empty())
}

/// Deepest chapter nesting [`chapter_info_from_layout`] builds. A provider's
/// layout is plugin output and the tree is later walked recursively, so a
/// deeper `depth` is clamped to this level. Audible itself nests at most a few
/// levels.
pub const MAX_CHAPTER_DEPTH: usize = 8;

/// Rebuilds a provider's flat pre-order chapter list as the nested Audible
/// `chapter_info` object the acquire pipeline reads.
///
/// Each node carries both the camelCase keys Audnexus uses and the
/// snake_case keys of Audible's own API, as the pipeline accepts either.
/// A chapter whose `depth` skips a level is attached to the deepest open
/// shallower chapter, so a malformed list still nests without losing a
/// chapter, chapters of equal depth stay siblings, and
/// nesting stops at [`MAX_CHAPTER_DEPTH`] levels.
///
/// # Arguments
///
/// * `layout` - The provider's chapter layout.
///
/// # Returns
///
/// An object with `chapters`, the brand intro/outro and runtime lengths when
/// known, and `isAccurate`.
#[must_use]
pub fn chapter_info_from_layout(layout: &MetadataChapters) -> Value {
    // Open chapters with their declared (clamped) depths, strictly increasing
    // from the bottom, so the stack never holds more than
    // MAX_CHAPTER_DEPTH entries.
    let mut stack: Vec<(usize, Map<String, Value>)> = Vec::new();
    let mut roots: Vec<Value> = Vec::new();
    for chapter in &layout.chapters {
        // The layout is plugin output, and the tree is walked recursively
        // later; deeper chapters become the last allowed level's children.
        let depth = usize::try_from(chapter.depth)
            .unwrap_or(usize::MAX)
            .min(MAX_CHAPTER_DEPTH - 1);
        // Close every open chapter at this depth or deeper: what stays open
        // is a shallower chapter, this one's parent. Comparing declared
        // depths keeps siblings together after a skipped level.
        while stack.last().is_some_and(|(open, _)| *open >= depth) {
            close_top(&mut stack, &mut roots);
        }
        let mut node = Map::new();
        node.insert("title".into(), Value::from(chapter.title.clone()));
        mirror(
            &mut node,
            "startOffsetMs",
            "start_offset_ms",
            chapter.start_ms,
        );
        if let Some(len) = chapter.length_ms {
            mirror(&mut node, "lengthMs", "length_ms", len);
        }
        stack.push((depth, node));
    }
    while !stack.is_empty() {
        close_top(&mut stack, &mut roots);
    }

    let mut info = Map::new();
    info.insert("chapters".into(), Value::Array(roots));
    if let Some(ms) = layout.brand_intro_ms {
        mirror(
            &mut info,
            "brandIntroDurationMs",
            "brand_intro_duration_ms",
            ms,
        );
    }
    if let Some(ms) = layout.brand_outro_ms {
        mirror(
            &mut info,
            "brandOutroDurationMs",
            "brand_outro_duration_ms",
            ms,
        );
    }
    if let Some(ms) = layout.runtime_ms {
        mirror(&mut info, "runtimeLengthMs", "runtime_length_ms", ms);
    }
    info.insert("isAccurate".into(), Value::from(layout.accurate));
    Value::Object(info)
}

/// Pops the deepest open chapter and attaches it to its parent, or to the
/// top-level list when it has none.
fn close_top(stack: &mut Vec<(usize, Map<String, Value>)>, roots: &mut Vec<Value>) {
    let Some((_, node)) = stack.pop() else {
        return;
    };
    match stack.last_mut() {
        Some((_, parent)) => {
            let children = parent
                .entry("chapters")
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Value::Array(list) = children {
                list.push(Value::Object(node));
            }
        }
        None => roots.push(Value::Object(node)),
    }
}

/// Inserts `value` under both a camelCase and a snake_case key.
fn mirror(obj: &mut Map<String, Value>, camel: &str, snake: &str, value: u64) {
    obj.insert(camel.into(), Value::from(value));
    obj.insert(snake.into(), Value::from(value));
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_sdk::MetadataChapter;
    use serde_json::json;

    fn chapter(title: &str, start_ms: u64, depth: u32) -> MetadataChapter {
        MetadataChapter {
            title: title.into(),
            start_ms,
            length_ms: Some(1000),
            depth,
        }
    }

    #[test]
    fn nesting_stops_at_the_depth_limit() {
        let layout = MetadataChapters {
            found: true,
            chapters: (0..200u32)
                .map(|depth| chapter(&format!("c{depth}"), u64::from(depth), depth))
                .collect(),
            ..MetadataChapters::default()
        };
        let info = chapter_info_from_layout(&layout);
        let mut levels = 0;
        let mut count = 0;
        let mut node = &info;
        while let Some(children) = node.get("chapters").and_then(Value::as_array) {
            let Some(first) = children.first() else { break };
            levels += 1;
            count += children.len();
            node = first;
        }
        assert_eq!(levels, MAX_CHAPTER_DEPTH);
        assert_eq!(count, 200, "every chapter is kept");
    }

    #[test]
    fn equal_depths_after_a_skip_stay_siblings() {
        let layout = MetadataChapters {
            found: true,
            chapters: vec![
                chapter("Root", 0, 0),
                chapter("A", 10, 3),
                chapter("B", 20, 3),
                chapter("C", 30, 2),
                chapter("Next", 40, 0),
            ],
            ..MetadataChapters::default()
        };
        let info = chapter_info_from_layout(&layout);
        let titles = |v: &Value| -> Vec<String> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|n| n["title"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(titles(&info["chapters"]), ["Root", "Next"]);
        assert_eq!(titles(&info["chapters"][0]["chapters"]), ["A", "B", "C"]);
        assert!(info["chapters"][0]["chapters"][0].get("chapters").is_none());
    }

    #[test]
    fn flat_layout_nests_by_depth() {
        let layout = MetadataChapters {
            found: true,
            chapters: vec![
                chapter("Opening", 0, 0),
                chapter("Part 1", 1000, 0),
                chapter("Chapter 1", 1000, 1),
                chapter("Scene", 1500, 2),
                chapter("Chapter 2", 2000, 1),
                chapter("End", 3000, 0),
            ],
            brand_intro_ms: Some(2000),
            brand_outro_ms: Some(5000),
            runtime_ms: Some(9000),
            accurate: true,
        };
        let info = chapter_info_from_layout(&layout);
        let titles = |v: &Value| -> Vec<String> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|n| n["title"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(titles(&info["chapters"]), ["Opening", "Part 1", "End"]);
        let part = &info["chapters"][1];
        assert_eq!(titles(&part["chapters"]), ["Chapter 1", "Chapter 2"]);
        assert_eq!(titles(&part["chapters"][0]["chapters"]), ["Scene"]);
        assert_eq!(part["start_offset_ms"], json!(1000));
        assert_eq!(part["startOffsetMs"], json!(1000));
        assert_eq!(part["length_ms"], json!(1000));
        assert_eq!(info["brand_intro_duration_ms"], json!(2000));
        assert_eq!(info["brandOutroDurationMs"], json!(5000));
        assert_eq!(info["runtime_length_ms"], json!(9000));
        assert_eq!(info["isAccurate"], json!(true));
    }

    #[test]
    fn skipped_depth_attaches_to_deepest_parent_and_unknowns_are_omitted() {
        let layout = MetadataChapters {
            found: true,
            chapters: vec![
                MetadataChapter {
                    title: "Root".into(),
                    start_ms: 0,
                    length_ms: None,
                    depth: 0,
                },
                chapter("Deep", 10, 3),
                chapter("Orphan", 20, 5),
            ],
            ..MetadataChapters::default()
        };
        let info = chapter_info_from_layout(&layout);
        let root = &info["chapters"][0];
        assert!(root.get("length_ms").is_none());
        assert_eq!(root["chapters"][0]["title"], json!("Deep"));
        assert_eq!(root["chapters"][0]["chapters"][0]["title"], json!("Orphan"));
        assert!(info.get("brand_intro_duration_ms").is_none());
        assert!(info.get("runtime_length_ms").is_none());
        assert_eq!(info["isAccurate"], json!(false));
    }

    #[test]
    fn empty_layout_has_no_chapters() {
        let info = chapter_info_from_layout(&MetadataChapters::default());
        assert_eq!(info["chapters"], json!([]));
    }

    #[test]
    fn identifiers_are_normalized() {
        let asin = asin_identifier(" b00abc1234 ", " uk ");
        assert_eq!(asin.scheme, IdentifierScheme::Asin);
        assert_eq!(asin.value, "B00ABC1234");
        assert_eq!(asin.marketplace.as_deref(), Some("uk"));
        assert_eq!(asin_identifier("B1", "  ").marketplace, None);
        let isbn = isbn_identifier(" 9780000000002 ");
        assert_eq!(isbn.scheme, IdentifierScheme::Isbn);
        assert_eq!(isbn.value, "9780000000002");
        let record = MetadataRecord {
            identifiers: vec![
                Identifier {
                    scheme: IdentifierScheme::Isbn,
                    value: "  ".into(),
                    marketplace: None,
                },
                asin.clone(),
            ],
            ..MetadataRecord::default()
        };
        assert_eq!(
            record_identifier(&record, IdentifierScheme::Asin),
            Some("B00ABC1234")
        );
        assert_eq!(record_identifier(&record, IdentifierScheme::Isbn), None);
        let mut later = record.clone();
        later.identifiers.push(isbn);
        assert_eq!(
            record_identifier(&later, IdentifierScheme::Isbn),
            Some("9780000000002"),
            "a blank value does not hide a later one"
        );
    }

    #[tokio::test]
    async fn no_providers_answer_empty() {
        let none = NoMetadataProviders;
        assert_eq!(
            none.lookup(&MetadataLookupParams::default()).await,
            MetadataAnswer::default()
        );
        assert!(!none
            .search(&MetadataSearchParams::default())
            .await
            .incomplete());
        assert_eq!(
            none.chapters(&MetadataChaptersParams::default()).await,
            ChaptersAnswer::default()
        );
    }
}
