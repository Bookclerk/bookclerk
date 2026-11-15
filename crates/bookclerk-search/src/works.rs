//! The works search index: one document per held top-level work.
//!
//! `GET /api/library/works?q=` reads it. It lives beside the per-title
//! index ([`crate::SearchEngine`]) in its own directory, so each can be
//! rebuilt on its own, and like that index it is node-local and kept
//! current by the daemon's search consumer.

use std::path::{Path, PathBuf};

use bookclerk_library::{LibraryStore, WorkSearchDoc};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{Query, QueryParser};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED, STRING, TEXT,
};
use tantivy::tokenizer::{LowerCaser, RawTokenizer, TextAnalyzer};
use tantivy::{doc, Index, IndexWriter, ReloadPolicy, TantivyDocument, TantivyError, Term};

use crate::engine::{writer_error, REFRESH_WRITER_HEAP, REPLACE_LOCK};
use crate::{Result, SearchEngine, SearchError};

/// Version of the works document layout; a rebuild marker of another
/// version does not count.
const WORKS_SCHEMA_VERSION: u32 = 2;

/// Tokenizer of the identifiers field: the whole value, lowercased, so a
/// query's `B0DUNE` matches the stored `b0dune`.
const IDENTIFIER_TOKENIZER: &str = "raw_lower";

/// Works read per page by a rebuild.
const REBUILD_PAGE: u64 = 100;

/// Writer heap of a rebuild.
const REBUILD_WRITER_HEAP: usize = 50_000_000;

/// Name of the file a finished rebuild leaves in the index directory.
fn complete_marker() -> String {
    format!("bookclerk-works-complete-v{WORKS_SCHEMA_VERSION}")
}

/// One window of a query's matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkMatches {
    /// Work ids in the window, best match first.
    pub ids: Vec<String>,
    /// How many documents the query matches in all.
    pub total: usize,
}

/// Rewrites the field names people type into the works index's own:
/// `author:` and `narrator:` (the title search's spellings) and the store
/// identifier names, which all search `identifiers`.
fn normalize_work_query(input: &str) -> String {
    let mut out = input.trim().to_string();
    for (from, to) in [
        ("author:", "authors:"),
        ("narrator:", "narrators:"),
        ("productid:", "identifiers:"),
        ("product_id:", "identifiers:"),
        ("asin:", "identifiers:"),
        ("isbn:", "identifiers:"),
    ] {
        out = out.replace(from, to);
    }
    out
}

/// The works schema and its fields, in [`WorkSearchIndex`] field order.
fn works_schema() -> (Schema, [Field; 8]) {
    let mut builder = Schema::builder();
    let work_id = builder.add_text_field("work_id", STRING | STORED);
    let title = builder.add_text_field("title", TEXT);
    let other_titles = builder.add_text_field("other_titles", TEXT);
    let authors = builder.add_text_field("authors", TEXT);
    let narrators = builder.add_text_field("narrators", TEXT);
    let series = builder.add_text_field("series", TEXT);
    let identifiers = builder.add_text_field(
        "identifiers",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(IDENTIFIER_TOKENIZER)
                .set_index_option(IndexRecordOption::Basic),
        ),
    );
    let all = builder.add_text_field("all", TEXT);
    (
        builder.build(),
        [
            work_id,
            title,
            other_titles,
            authors,
            narrators,
            series,
            identifiers,
            all,
        ],
    )
}

/// Maps any displayable error to [`SearchError::Index`].
fn index_error(err: impl std::fmt::Display) -> SearchError {
    SearchError::Index(err.to_string())
}

/// On-disk Tantivy index of works.
pub struct WorkSearchIndex {
    /// Index directory, where the rebuild marker lives.
    dir: PathBuf,
    /// The open index.
    index: Index,
    /// Stored work id; refreshes delete by it.
    work_id: Field,
    /// The work's title.
    title: Field,
    /// Edition, listing and episode titles and subtitles.
    other_titles: Field,
    /// Authors or hosts.
    authors: Field,
    /// Narrators.
    narrators: Field,
    /// Series names.
    series: Field,
    /// Product ids, ISBNs, ASINs and other identifiers, lowercased, one
    /// term each.
    identifiers: Field,
    /// Catch-all text field over everything above.
    all: Field,
}

impl WorkSearchIndex {
    /// The works index directory beside the per-title index in
    /// `search_index_dir` (`search_index_works/` next to `search_index/`).
    #[must_use]
    pub fn dir_beside(search_index_dir: &Path) -> PathBuf {
        let name = search_index_dir
            .file_name()
            .map(|n| format!("{}_works", n.to_string_lossy()))
            .unwrap_or_else(|| "search_index_works".into());
        search_index_dir.with_file_name(name)
    }

    /// Opens or creates the works index in `dir`, replacing an index of
    /// another layout.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::Index`] when the directory or index cannot be
    /// opened.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(index_error)?;
        let (schema, _) = works_schema();
        let index = match SearchEngine::open_index(dir, schema.clone()) {
            Err(TantivyError::SchemaError(_)) => {
                let _replacing = REPLACE_LOCK
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match SearchEngine::open_index(dir, schema.clone()) {
                    Err(TantivyError::SchemaError(_)) => {
                        SearchEngine::discard_index(dir)?;
                        SearchEngine::open_index(dir, schema)
                    }
                    other => other,
                }
            }
            other => other,
        }
        .map_err(index_error)?;
        Ok(Self::with_index(dir, index))
    }

    /// Opens the index in `dir` for searching only: nothing is created or
    /// replaced, so a search never races a writer over the directory.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::NotReady`] when `dir` holds no index of the
    /// current layout (the rebuild has not run yet), or
    /// [`SearchError::Index`] when it cannot be opened.
    pub fn open_for_search(dir: &Path) -> Result<Self> {
        if !dir.join("meta.json").is_file() {
            return Err(SearchError::NotReady);
        }
        let (schema, _) = works_schema();
        match SearchEngine::open_index(dir, schema) {
            Ok(index) => Ok(Self::with_index(dir, index)),
            Err(TantivyError::SchemaError(_)) => Err(SearchError::NotReady),
            Err(err) => Err(index_error(err)),
        }
    }

    /// Wraps an open index of the works schema, registering its tokenizers.
    fn with_index(dir: &Path, index: Index) -> Self {
        index.tokenizers().register(
            IDENTIFIER_TOKENIZER,
            TextAnalyzer::builder(RawTokenizer::default())
                .filter(LowerCaser)
                .build(),
        );
        let (_, [work_id, title, other_titles, authors, narrators, series, identifiers, all]) =
            works_schema();
        Self {
            dir: dir.to_path_buf(),
            index,
            work_id,
            title,
            other_titles,
            authors,
            narrators,
            series,
            identifiers,
            all,
        }
    }

    /// True when a full rebuild of the current layout has finished in `dir`.
    #[must_use]
    pub fn is_complete(dir: &Path) -> bool {
        dir.join(complete_marker()).is_file()
    }

    /// Forgets that the index was fully rebuilt, so the daemon queues a new
    /// rebuild.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::Index`] when the marker cannot be removed.
    pub fn mark_incomplete(&self) -> Result<()> {
        match std::fs::remove_file(self.dir.join(complete_marker())) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(index_error(err)),
        }
    }

    /// Replaces the documents of `work_ids` with what the library holds
    /// now: a held work's document is written again, and an unheld or
    /// missing work's is removed.
    ///
    /// # Returns
    ///
    /// How many documents were written.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::Busy`] when another writer holds the index,
    /// or the library or index error.
    pub async fn refresh_works(
        &self,
        library: &LibraryStore,
        work_ids: &[String],
    ) -> Result<usize> {
        let mut docs = Vec::new();
        let mut tops = Vec::new();
        for id in work_ids {
            let (top, doc) = library.work_search_document(id).await?;
            tops.push(top);
            docs.extend(doc);
        }
        let mut writer = self
            .index
            .writer_with_num_threads(1, REFRESH_WRITER_HEAP)
            .map_err(writer_error)?;
        for top in &tops {
            writer.delete_term(Term::from_field_text(self.work_id, top));
        }
        for doc in &docs {
            self.add(&mut writer, doc)?;
        }
        writer.commit().map_err(index_error)?;
        writer.wait_merging_threads().map_err(index_error)?;
        Ok(docs.len())
    }

    /// Rebuilds the whole index from the library, walking every top-level
    /// work in pages, then leaves the rebuild marker.
    ///
    /// # Returns
    ///
    /// How many documents were written.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::Busy`] when another writer holds the index,
    /// or the library or index error.
    pub async fn rebuild(&self, library: &LibraryStore) -> Result<usize> {
        let mut writer = self
            .index
            .writer(REBUILD_WRITER_HEAP)
            .map_err(writer_error)?;
        writer.delete_all_documents().map_err(index_error)?;
        let mut after: Option<String> = None;
        let mut count = 0usize;
        loop {
            let ids = library
                .top_level_work_ids_page(after.as_deref(), REBUILD_PAGE)
                .await?;
            let Some(last) = ids.last().cloned() else {
                break;
            };
            for id in &ids {
                if let (_, Some(doc)) = library.work_search_document(id).await? {
                    self.add(&mut writer, &doc)?;
                    count += 1;
                }
            }
            after = Some(last);
        }
        writer.commit().map_err(index_error)?;
        std::fs::write(self.dir.join(complete_marker()), b"").map_err(index_error)?;
        Ok(count)
    }

    /// Adds one work's document.
    fn add(&self, writer: &mut IndexWriter, work: &WorkSearchDoc) -> Result<()> {
        let all = [
            work.title.as_str(),
            &work.other_titles.join(" "),
            &work.authors.join(" "),
            &work.narrators.join(" "),
            &work.series.join(" "),
            &work.identifiers.join(" "),
        ]
        .join(" ");
        let mut document = doc!(
            self.work_id => work.work_id.clone(),
            self.title => work.title.clone(),
            self.other_titles => work.other_titles.join("\n"),
            self.authors => work.authors.join("\n"),
            self.narrators => work.narrators.join("\n"),
            self.series => work.series.join("\n"),
            self.all => all,
        );
        for id in &work.identifiers {
            document.add_text(self.identifiers, id);
        }
        writer.add_document(document).map_err(index_error)?;
        Ok(())
    }

    /// Opens the index in `dir` for searching and reads one window of a
    /// query's matches without blocking the caller's task (see
    /// [`Self::open_for_search`] and [`Self::match_ids`]).
    ///
    /// # Errors
    ///
    /// Propagates open and query failures.
    pub async fn open_and_match_ids(
        dir: PathBuf,
        query: String,
        offset: usize,
        limit: usize,
    ) -> Result<WorkMatches> {
        tokio::task::spawn_blocking(move || {
            Self::open_for_search(&dir)?.match_ids(&query, offset, limit)
        })
        .await
        .map_err(|err| SearchError::Index(format!("search task join error: {err}")))?
    }

    /// The work ids of the `limit` matches of `raw_query` after the first
    /// `offset`, best first (ties in index order), and the total match
    /// count. Only the window's documents are read. Blocking.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::Query`] for a query that does not parse, or
    /// [`SearchError::Index`] when the index cannot be read.
    pub fn match_ids(&self, raw_query: &str, offset: usize, limit: usize) -> Result<WorkMatches> {
        let Some(query) = self.parse(raw_query)? else {
            return Ok(WorkMatches {
                ids: Vec::new(),
                total: 0,
            });
        };
        let searcher = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map(|reader: tantivy::IndexReader| reader.searcher())
            .map_err(index_error)?;
        let window = TopDocs::with_limit(limit.max(1))
            .and_offset(offset)
            .order_by_score();
        let (total, docs) = searcher
            .search(&query, &(Count, window))
            .map_err(index_error)?;
        let mut ids = Vec::with_capacity(docs.len());
        for (_, addr) in docs {
            let doc: TantivyDocument = searcher.doc(addr).map_err(index_error)?;
            if let Some(id) = doc.get_first(self.work_id).and_then(|v| v.as_str()) {
                ids.push(id.to_string());
            }
        }
        Ok(WorkMatches { ids, total })
    }

    /// Parses a Lucene-style query over the searchable fields, or `None`
    /// when it normalizes to nothing.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::Query`] when the query does not parse.
    fn parse(&self, raw_query: &str) -> Result<Option<Box<dyn Query>>> {
        let query = normalize_work_query(raw_query);
        if query.is_empty() {
            return Ok(None);
        }
        let mut parser = QueryParser::for_index(
            &self.index,
            vec![
                self.all,
                self.title,
                self.other_titles,
                self.authors,
                self.narrators,
                self.series,
                self.identifiers,
            ],
        );
        parser.set_field_boost(self.title, 3.0);
        parser.set_field_boost(self.authors, 2.0);
        parser
            .parse_query(&query)
            .map(Some)
            .map_err(|err| SearchError::Query(format!("{err}")))
    }
}

#[cfg(test)]
mod tests {
    use bookclerk_library::NewBook;

    use super::*;

    /// A library with an account and `books` scanned onto it; returns the
    /// works in the same order.
    async fn library_with(books: &[(&str, &str, &str)]) -> (LibraryStore, Vec<String>) {
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        library
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .unwrap();
        let mut works = Vec::new();
        for (asin, title, author) in books {
            let mut book = NewBook::minimal(*asin, "acct", "us", *title);
            book.authors = Some((*author).into());
            let saved = library.upsert_book(&book).await.unwrap();
            works.push(
                library
                    .work_id_for_book(&saved.uuid)
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        (library, works)
    }

    /// Every match of `q` in `index`, best first.
    fn ids(index: &WorkSearchIndex, q: &str) -> Vec<String> {
        index.match_ids(q, 0, 1000).unwrap().ids
    }

    #[tokio::test]
    async fn a_rebuild_finds_works_by_title_author_and_id() {
        let dir = tempfile::tempdir().unwrap();
        let (library, works) = library_with(&[
            ("B0HOBBIT", "The Hobbit", "J. R. R. Tolkien"),
            ("B0DUNE", "Dune", "Frank Herbert"),
        ])
        .await;
        let index = WorkSearchIndex::open(dir.path()).unwrap();
        assert!(!WorkSearchIndex::is_complete(dir.path()));
        assert_eq!(index.rebuild(&library).await.unwrap(), 2);
        assert!(WorkSearchIndex::is_complete(dir.path()));
        assert_eq!(ids(&index, "hobbit"), [works[0].clone()]);
        assert_eq!(ids(&index, "herbert"), [works[1].clone()]);
        assert_eq!(ids(&index, "b0dune"), [works[1].clone()]);
        assert!(ids(&index, "   ").is_empty());
        assert!(ids(&index, "nothing-like-it").is_empty());
        assert_eq!(ids(&index, "identifiers:B0DUNE"), [works[1].clone()]);
        assert_eq!(ids(&index, "asin:B0DUNE"), [works[1].clone()]);
        assert_eq!(ids(&index, "author:herbert"), [works[1].clone()]);
        assert_eq!(
            ids(&index, "dune acquired finished"),
            [works[1].clone()],
            "the title search's shortcuts are plain words here"
        );
        // Works carry no tags: the title search's `[tag]` is not rewritten,
        // and a bare bracket is query syntax the parser refuses.
        assert!(
            matches!(index.match_ids("[bio]", 0, 10), Err(SearchError::Query(_))),
            "{:?}",
            index.match_ids("[bio]", 0, 10)
        );
        let found = WorkSearchIndex::open_and_match_ids(
            dir.path().to_path_buf(),
            "tolkien OR herbert".into(),
            1,
            5,
        )
        .await
        .unwrap();
        assert_eq!(found.total, 2);
        assert_eq!(found.ids.len(), 1, "the window skips the first match");
        index.mark_incomplete().unwrap();
        index.mark_incomplete().unwrap();
        assert!(!WorkSearchIndex::is_complete(dir.path()));
    }

    #[tokio::test]
    async fn a_refresh_adds_a_held_work_and_drops_an_unheld_one() {
        let dir = tempfile::tempdir().unwrap();
        let (library, works) = library_with(&[("B0DUNE", "Dune", "Frank Herbert")]).await;
        let index = WorkSearchIndex::open(dir.path()).unwrap();
        assert_eq!(index.refresh_works(&library, &works).await.unwrap(), 1);
        assert_eq!(index.refresh_works(&library, &works).await.unwrap(), 1);
        assert_eq!(ids(&index, "dune"), works, "written once");
        library
            .remove_account_holdings("audible", "acct", bookclerk_library::HoldingSelection::All)
            .await
            .unwrap();
        assert_eq!(index.refresh_works(&library, &works).await.unwrap(), 0);
        assert!(ids(&index, "dune").is_empty());
    }

    #[test]
    fn a_search_before_the_first_build_is_not_ready() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("search_index_works");
        assert!(matches!(
            WorkSearchIndex::open_for_search(&missing),
            Err(SearchError::NotReady)
        ));
        assert!(!missing.exists(), "a search creates nothing");
        assert!(WorkSearchIndex::open_for_search(dir.path()).is_err());
    }

    #[test]
    fn the_works_index_sits_beside_the_title_index() {
        assert_eq!(
            WorkSearchIndex::dir_beside(Path::new("/data/search_index")),
            PathBuf::from("/data/search_index_works")
        );
    }
}
