//! Tantivy index build and search (classic Libation `SearchEngine` field parity).
//!
//! # Audience
//!
//! Host code that opens [`SearchEngine`] under `search_index/` and runs queries.

use std::path::{Path, PathBuf};

use bookclerk_library::{AcquireStatus, BookRecord, LibraryStore};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, Value, STORED, STRING, TEXT};
use tantivy::{doc, Index, IndexWriter, ReloadPolicy, TantivyDocument};

use crate::query::normalize_lucene_query;
use crate::{Result, SearchError};

/// One search hit.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// Display / primary id (source `product_id`).
    pub asin: String,
    /// Library public uuid (indexed lowercased; returned as stored).
    pub uuid: String,
    /// Library account id that owns this title.
    pub account_id: String,
    /// Display title stored with the hit.
    pub title: String,
    /// Tantivy relevance score (higher is better).
    pub score: f32,
}

/// On-disk Tantivy search engine (classic Lucene `SearchEngine` parity).
pub struct SearchEngine {
    /// On-disk Tantivy index opened under `search_index/`.
    index: Index,
    /// Stored lowercase `product_id` (classic Lucene `id`).
    id: Field,
    /// Stored lowercase library UUID.
    uuid: Field,
    /// Stored lowercase storefront product id.
    product_id: Field,
    /// Stored lowercase ISBN (empty string when unknown).
    isbn: Field,
    /// Stored lowercase ASIN (empty string when unknown).
    asin: Field,
    /// Stored owning account id (not lowercased).
    account: Field,
    /// Tokenized title (TEXT + stored).
    title: Field,
    /// Tokenized authors string.
    authors: Field,
    /// Tokenized narrators string.
    narrators: Field,
    /// Tokenized series name.
    series: Field,
    /// Tokenized operator tags.
    tags: Field,
    /// Catch-all TEXT field (uuid, ids, title, people, series, tags).
    all: Field,
    /// Stored `true`/`false` for acquire status (`acquired`).
    acquired: Field,
    /// Stored `true`/`false` for listener finished flag.
    finished: Field,
}

/// Full `books` rows per rebuild page.
///
/// 256 rows encode to about 310 KiB as a Cap'n `StatementResult`, over the
/// sqlite guest `maxResultBytes` of 256 KiB. 64 rows are about 80 KiB.
/// See `books_page_statement_result_wire_size` in `bookclerk-plugin-abi`.
const CATALOG_PAGE_ROWS: u64 = 64;

/// Rereads of one page while the daemon holds the database file.
const PAGE_LOCK_ATTEMPTS: u32 = 20;

/// True when `err` is SQLite lock contention from the overlapping daemon.
fn database_lock_contention(err: &bookclerk_library::LibraryError) -> bool {
    let upper = err.to_string().to_ascii_uppercase();
    upper.contains("SQLITE_BUSY") || upper.contains("SQLITE_LOCKED")
}

/// Reads one catalog page, waiting out a lock held by the live daemon.
///
/// # Errors
///
/// Returns the library error when the read fails for a reason other than
/// lock contention, or when the lock outlasts [`PAGE_LOCK_ATTEMPTS`].
async fn read_catalog_page(
    library: &LibraryStore,
    after_id: Option<i64>,
) -> Result<Vec<BookRecord>> {
    let mut pause = std::time::Duration::from_millis(20);
    let mut attempt = 0u32;
    loop {
        match library
            .list_books_page(None, after_id, CATALOG_PAGE_ROWS)
            .await
        {
            Ok(page) => return Ok(page),
            Err(err) if database_lock_contention(&err) && attempt + 1 < PAGE_LOCK_ATTEMPTS => {
                attempt += 1;
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(std::time::Duration::from_millis(250));
            }
            Err(err) => return Err(err.into()),
        }
    }
}

impl SearchEngine {
    /// Open or create a search index at `dir`.
    ///
    /// # Arguments
    ///
    /// * `dir` - Absolute path to the Tantivy index directory (created if missing).
    ///
    /// # Errors
    ///
    /// Returns [`crate::SearchError::Index`] when the directory or index cannot
    /// be opened.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|err| SearchError::Index(err.to_string()))?;
        let mut schema_builder = Schema::builder();
        let id = schema_builder.add_text_field("id", STRING | STORED);
        let uuid = schema_builder.add_text_field("uuid", STRING | STORED);
        let product_id = schema_builder.add_text_field("product_id", STRING | STORED);
        let isbn = schema_builder.add_text_field("isbn", STRING | STORED);
        let asin = schema_builder.add_text_field("asin", STRING | STORED);
        let account = schema_builder.add_text_field("account", STRING | STORED);
        let title = schema_builder.add_text_field("title", TEXT | STORED);
        let authors = schema_builder.add_text_field("authors", TEXT | STORED);
        let narrators = schema_builder.add_text_field("narrators", TEXT | STORED);
        let series = schema_builder.add_text_field("series", TEXT | STORED);
        let tags = schema_builder.add_text_field("tags", TEXT | STORED);
        let all = schema_builder.add_text_field("all", TEXT);
        let acquired = schema_builder.add_text_field("acquired", STRING | STORED);
        let finished = schema_builder.add_text_field("finished", STRING | STORED);
        let schema = schema_builder.build();

        let index = Index::open_or_create(
            tantivy::directory::MmapDirectory::open(dir)
                .map_err(|err| SearchError::Index(err.to_string()))?,
            schema,
        )
        .map_err(|err| SearchError::Index(err.to_string()))?;

        Ok(Self {
            index,
            id,
            uuid,
            product_id,
            isbn,
            asin,
            account,
            title,
            authors,
            narrators,
            series,
            tags,
            all,
            acquired,
            finished,
        })
    }

    /// Rebuild the entire index from the library DB.
    ///
    /// Rows are read in pages of [`CATALOG_PAGE_ROWS`]. A Cap'n `StatementResult`
    /// of 256 full `books` rows is about 310 KiB, over the sqlite guest
    /// `maxResultBytes` (256 KiB), so that page never commits and the index
    /// stays empty. Pages of 64 rows are about 80 KiB.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn rebuild(&self, library: &LibraryStore) -> Result<usize> {
        let mut writer = self
            .index
            .writer(50_000_000)
            .map_err(|err| SearchError::Index(err.to_string()))?;
        writer
            .delete_all_documents()
            .map_err(|err| SearchError::Index(err.to_string()))?;

        let mut after_id = None;
        let mut count = 0usize;
        loop {
            let page = read_catalog_page(library, after_id).await?;
            let Some(last) = page.last() else {
                break;
            };
            let last_id = last.id;
            let full_page = u64::try_from(page.len()).unwrap_or(u64::MAX) == CATALOG_PAGE_ROWS;
            for book in &page {
                self.add_book(&mut writer, book)?;
            }
            count += page.len();
            if !full_page {
                break;
            }
            after_id = Some(last_id);
        }
        writer
            .commit()
            .map_err(|err| SearchError::Index(err.to_string()))?;
        Ok(count)
    }

    /// Indexes one library row; missing optional strings become empty, bools become `true`/`false`.
    fn add_book(&self, writer: &mut IndexWriter, book: &BookRecord) -> Result<()> {
        let acquired = bool_str(book.acquire_status == AcquireStatus::Acquired);
        let finished = bool_str(book.is_finished);
        let asin = book.asin.as_deref().unwrap_or("");
        let isbn = book.isbn.as_deref().unwrap_or("");
        let all_text = format!(
            "{} {} {} {} {} {} {} {} {}",
            book.uuid,
            book.product_id,
            isbn,
            asin,
            book.title,
            book.authors.as_deref().unwrap_or(""),
            book.narrators.as_deref().unwrap_or(""),
            book.series.as_deref().unwrap_or(""),
            book.tags.as_deref().unwrap_or(""),
        );
        writer
            .add_document(doc!(
                self.id => book.product_id.to_ascii_lowercase(),
                self.uuid => book.uuid.to_ascii_lowercase(),
                self.product_id => book.product_id.to_ascii_lowercase(),
                self.isbn => isbn.to_ascii_lowercase(),
                self.asin => asin.to_ascii_lowercase(),
                self.account => book.account_id.clone(),
                self.title => book.title.clone(),
                self.authors => book.authors.clone().unwrap_or_default(),
                self.narrators => book.narrators.clone().unwrap_or_default(),
                self.series => book.series.clone().unwrap_or_default(),
                self.tags => book.tags.clone().unwrap_or_default(),
                self.all => all_text,
                self.acquired => acquired,
                self.finished => finished,
            ))
            .map_err(|err| SearchError::Index(err.to_string()))?;
        Ok(())
    }

    /// Open an index and search it without blocking the caller's task.
    ///
    /// Both [`SearchEngine::open`] and [`SearchEngine::search`] do synchronous
    /// disk work — opening builds the schema and touches the index directory,
    /// and querying reads and scores segments. Calling them directly from an
    /// async handler stalls a runtime worker for the whole query, which on the
    /// daemon's `/api/library/books?q=` path meant one search could hold up
    /// unrelated requests.
    ///
    /// # Errors
    ///
    /// Propagates index-open and query failures.
    pub async fn open_and_search(
        dir: PathBuf,
        query: String,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        tokio::task::spawn_blocking(move || Self::open(&dir)?.search(&query, limit))
            .await
            .map_err(|err| SearchError::Index(format!("search task join error: {err}")))?
    }

    /// Search the index. `limit` of 0 returns all matches (classic `-n 0`).
    ///
    /// Blocking. Prefer [`SearchEngine::open_and_search`] from async code.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn search(&self, raw_query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let query_str = normalize_lucene_query(raw_query);
        if query_str.is_empty() {
            return Ok(Vec::new());
        }

        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|err| SearchError::Index(err.to_string()))?;

        let searcher = reader.searcher();
        let parser = QueryParser::for_index(
            &self.index,
            vec![
                self.all,
                self.title,
                self.authors,
                self.narrators,
                self.series,
                self.tags,
                self.id,
                self.uuid,
                self.product_id,
                self.isbn,
                self.asin,
                self.acquired,
                self.finished,
            ],
        );

        let query = parser
            .parse_query(&query_str)
            .map_err(|err| SearchError::Query(format!("{err}")))?;

        // Classic `-n 0` means every hit. Cap at the index size so TopDocs
        // never gets a zero limit (which panics) while still returning all
        // matches for large libraries.
        let top = if limit == 0 {
            let n = searcher.num_docs() as usize;
            if n == 0 {
                return Ok(Vec::new());
            }
            n
        } else {
            limit
        };
        // Tantivy 0.26: TopDocs is a builder; order_by_score() yields the Collector.
        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(top).order_by_score())
            .map_err(|err| SearchError::Index(err.to_string()))?;

        let mut hits = Vec::new();
        for (score, addr) in top_docs {
            let doc: TantivyDocument = searcher
                .doc(addr)
                .map_err(|err| SearchError::Index(err.to_string()))?;
            let asin = doc
                .get_first(self.id)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_ascii_uppercase();
            let uuid = doc
                .get_first(self.uuid)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let account_id = doc
                .get_first(self.account)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let title = doc
                .get_first(self.title)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            hits.push(SearchHit {
                asin,
                uuid,
                account_id,
                title,
                score,
            });
        }
        Ok(hits)
    }
}

/// Formats a boolean as the lowercase strings Tantivy stores on `acquired` / `finished`.
fn bool_str(value: bool) -> String {
    if value {
        "true".into()
    } else {
        "false".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::NewBook;

    #[tokio::test]
    async fn indexes_and_finds_by_title() {
        let dir = tempfile::tempdir().unwrap();
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        library
            .upsert_account("acct", "us", None, true, "audible")
            .await
            .unwrap();
        let mut book = NewBook::minimal("B00TEST", "acct", "us", "Harry Potter");
        book.authors = Some("Rowling".into());
        library.upsert_book(&book).await.unwrap();

        let engine = SearchEngine::open(dir.path()).unwrap();
        engine.rebuild(&library).await.unwrap();
        let hits = engine.search("potter", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].asin, "B00TEST");
        let stored = library.get_book("B00TEST", "acct").await.unwrap().unwrap();
        assert_eq!(hits[0].uuid, stored.uuid.to_ascii_lowercase());
    }

    #[tokio::test]
    async fn indexes_uuid_product_id_isbn_asin() {
        let dir = tempfile::tempdir().unwrap();
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        library
            .upsert_account("acct", "us", None, true, "audible")
            .await
            .unwrap();
        let mut book = NewBook::minimal("B00TEST01", "acct", "us", "Indexed Book");
        book.isbn = Some("9781234567890".into());
        library.upsert_book(&book).await.unwrap();
        let stored = library
            .get_book("B00TEST01", "acct")
            .await
            .unwrap()
            .unwrap();

        let engine = SearchEngine::open(dir.path()).unwrap();
        engine.rebuild(&library).await.unwrap();

        let by_uuid = engine.search(&stored.uuid, 10).unwrap();
        assert_eq!(by_uuid.len(), 1);
        assert_eq!(by_uuid[0].asin, "B00TEST01");

        assert_eq!(engine.search("B00TEST01", 10).unwrap().len(), 1);
        assert_eq!(engine.search("9781234567890", 10).unwrap().len(), 1);
        assert_eq!(engine.search("asin:b00test01", 10).unwrap().len(), 1);
        assert_eq!(engine.search("isbn:9781234567890", 10).unwrap().len(), 1);
    }

    /// One page is `CATALOG_PAGE_ROWS` rows. The next title has to be indexed too.
    #[tokio::test]
    async fn rebuild_reads_past_one_catalog_page() {
        let dir = tempfile::tempdir().unwrap();
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        library
            .upsert_account("acct", "us", None, true, "audible")
            .await
            .unwrap();
        let past = u32::try_from(CATALOG_PAGE_ROWS).unwrap();
        let n = past + 1;
        for i in 0..n {
            let book = NewBook::minimal(
                format!("P{i:05}"),
                "acct",
                "us",
                format!("PagedTitle{i:05}"),
            );
            library.upsert_book(&book).await.unwrap();
        }
        let engine = SearchEngine::open(dir.path()).unwrap();
        let indexed = engine.rebuild(&library).await.unwrap();
        assert_eq!(indexed, usize::try_from(n).unwrap());
        assert_eq!(engine.search("PagedTitle00000", 10).unwrap().len(), 1);
        assert_eq!(
            engine
                .search(&format!("PagedTitle{past:05}"), 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn rebuild_retries_only_database_lock_errors() {
        let busy = bookclerk_library::LibraryError::Unavailable(
            "SQLITE_BUSY (5): database is locked".into(),
        );
        assert!(database_lock_contention(&busy));
        let locked = bookclerk_library::LibraryError::Unavailable(
            "SQLITE_LOCKED (6): database is locked".into(),
        );
        assert!(database_lock_contention(&locked));
        let cap = bookclerk_library::LibraryError::Unavailable(
            "encoded result is 316712 bytes; guest maxResultBytes is 262144".into(),
        );
        assert!(!database_lock_contention(&cap));
    }
}
