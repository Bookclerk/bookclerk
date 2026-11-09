//! Tantivy index build and search (classic Libation `SearchEngine` field parity).
//!
//! # Audience
//!
//! Host code that opens [`SearchEngine`] under `search_index/` and runs queries.

use std::path::{Path, PathBuf};

use bookclerk_library::{AcquireStatus, BookRecord, LibraryStore, ListingRef};
use tantivy::collector::{Count, TopDocs};
use tantivy::directory::error::LockError;
use tantivy::query::{Query, QueryParser};
use tantivy::schema::{Field, Schema, Value, FAST, STORED, STRING, TEXT};
use tantivy::{doc, Index, IndexWriter, Order, ReloadPolicy, TantivyDocument, TantivyError, Term};

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

/// One page of search hits in title order, and how many documents match.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchPage {
    /// Library uuids of the page's hits (lowercased, as indexed), in
    /// case-insensitive title order, then uuid.
    pub uuids: Vec<String>,
    /// Documents that match the query, before paging.
    pub total: usize,
}

/// Version of the document layout. An index written under another layout
/// is discarded and rebuilt, and [`SearchEngine::is_complete`] only
/// accepts a rebuild of this one.
const SCHEMA_VERSION: u32 = 2;

/// Name of the file a finished rebuild leaves in the index directory.
fn complete_marker() -> String {
    format!("bookclerk-complete-v{SCHEMA_VERSION}")
}

/// Writer heap of an event refresh: Tantivy's one-thread minimum, since a
/// refresh writes a handful of documents.
const REFRESH_WRITER_HEAP: usize = 15_000_000;

/// Maps a Tantivy error to [`SearchError::Busy`] when another writer holds
/// the index lock, else to [`SearchError::Index`].
fn writer_error(err: TantivyError) -> SearchError {
    match err {
        TantivyError::LockFailure(LockError::LockBusy, _) => SearchError::Busy,
        other => SearchError::Index(other.to_string()),
    }
}

/// On-disk Tantivy search engine (classic Lucene `SearchEngine` parity).
pub struct SearchEngine {
    /// Index directory, where the rebuild marker lives.
    dir: PathBuf,
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
    /// The title's listing ([`ListingRef::term`]); an event refresh deletes
    /// by it.
    listing: Field,
    /// Fast sort key: ASCII-lowercased title, NUL, stored uuid.
    sort: Field,
}

/// Starting page width for a catalog rebuild.
///
/// Enriched `books` rows can push 64 rows over the sqlite guest
/// `maxResultBytes` (256 KiB). The catalog reader halves that width and
/// reuses the fit for the rest of the page.
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
        let listing = schema_builder.add_text_field("listing", STRING);
        let sort = schema_builder.add_text_field("sort", STRING | FAST);
        let schema = schema_builder.build();

        let index = match Self::open_index(dir, schema.clone()) {
            Err(TantivyError::SchemaError(_)) => {
                // An index of an older layout: it is a cache of the
                // library, so it is replaced and the missing marker asks
                // for a rebuild.
                Self::discard_index(dir)?;
                Self::open_index(dir, schema)
            }
            other => other,
        }
        .map_err(|err| SearchError::Index(err.to_string()))?;

        Ok(Self {
            dir: dir.to_path_buf(),
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
            listing,
            sort,
        })
    }

    /// Opens or creates the Tantivy index in `dir` under `schema`.
    fn open_index(dir: &Path, schema: Schema) -> tantivy::Result<Index> {
        let directory = tantivy::directory::MmapDirectory::open(dir)
            .map_err(|err| TantivyError::SystemError(err.to_string()))?;
        Index::open_or_create(directory, schema)
    }

    /// Removes an index of another layout from `dir`.
    ///
    /// Only a directory that holds a Tantivy `meta.json` is touched, so a
    /// misconfigured path that names something else is refused rather than
    /// emptied.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::Index`] when `dir` is not a Tantivy index or
    /// cannot be removed and recreated.
    fn discard_index(dir: &Path) -> Result<()> {
        if !dir.join("meta.json").is_file() {
            return Err(SearchError::Index(format!(
                "{} holds no search index to replace",
                dir.display()
            )));
        }
        tracing::info!(dir = %dir.display(), "replacing a search index of an older layout");
        std::fs::remove_dir_all(dir).map_err(|err| SearchError::Index(err.to_string()))?;
        std::fs::create_dir_all(dir).map_err(|err| SearchError::Index(err.to_string()))
    }

    /// True when a full rebuild of the current layout has finished in `dir`.
    ///
    /// An index that was never rebuilt, was deleted, or was written under
    /// an older layout is not complete; the daemon queues a rebuild for it.
    #[must_use]
    pub fn is_complete(dir: &Path) -> bool {
        dir.join(complete_marker()).is_file()
    }

    /// Rebuild the entire index from the library DB.
    ///
    /// Rows are read in pages that start at 64. A page of enriched `books`
    /// rows can exceed the sqlite guest `maxResultBytes` (256 KiB). The reader
    /// halves until a slice fits, then keeps that width, so the index still
    /// receives every catalog row.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn rebuild(&self, library: &LibraryStore) -> Result<usize> {
        let mut writer = self.index.writer(50_000_000).map_err(writer_error)?;
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
        std::fs::write(self.dir.join(complete_marker()), b"")
            .map_err(|err| SearchError::Index(err.to_string()))?;
        Ok(count)
    }

    /// Replaces the documents of `listings` with the library titles that
    /// sell them now.
    ///
    /// Every document carrying a listing's term is deleted and each title
    /// [`LibraryStore::books_of_listing`] still finds is added again, so a
    /// title whose row is gone (a holding removed, an account revoked)
    /// leaves the index, and one that changed is replaced.
    ///
    /// # Arguments
    ///
    /// * `library` - The library the titles are read from.
    /// * `listings` - The listings an event touched.
    ///
    /// # Returns
    ///
    /// How many documents were written.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::Busy`] when another writer holds the index,
    /// [`SearchError::Library`] when a read fails, or
    /// [`SearchError::Index`] when the write fails.
    pub async fn refresh_listings(
        &self,
        library: &LibraryStore,
        listings: &[ListingRef],
    ) -> Result<usize> {
        let mut titles = Vec::new();
        for listing in listings {
            titles.push((listing, library.books_of_listing(listing).await?));
        }
        let mut writer = self.refresh_writer()?;
        let mut count = 0usize;
        for (listing, books) in &titles {
            writer.delete_term(Term::from_field_text(self.listing, &listing.term()));
            for book in books {
                self.add_book(&mut writer, book)?;
            }
            count += books.len();
        }
        writer
            .commit()
            .map_err(|err| SearchError::Index(err.to_string()))?;
        Ok(count)
    }

    /// Replaces the documents of the titles `uuids` with their rows now, and
    /// drops any whose row is gone.
    ///
    /// # Arguments
    ///
    /// * `library` - The library the titles are read from.
    /// * `uuids` - Library `books.uuid` values.
    ///
    /// # Returns
    ///
    /// How many documents were written.
    ///
    /// # Errors
    ///
    /// As [`Self::refresh_listings`].
    pub async fn refresh_titles(&self, library: &LibraryStore, uuids: &[String]) -> Result<usize> {
        let mut titles = Vec::new();
        for uuid in uuids {
            titles.push((uuid, library.get_book_by_uuid(uuid).await?));
        }
        let mut writer = self.refresh_writer()?;
        let mut count = 0usize;
        for (uuid, book) in &titles {
            writer.delete_term(Term::from_field_text(self.uuid, &uuid.to_ascii_lowercase()));
            if let Some(book) = book {
                self.add_book(&mut writer, book)?;
                count += 1;
            }
        }
        writer
            .commit()
            .map_err(|err| SearchError::Index(err.to_string()))?;
        Ok(count)
    }

    /// A one-thread writer for a small refresh.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::Busy`] when another writer holds the lock.
    fn refresh_writer(&self) -> Result<IndexWriter> {
        self.index
            .writer_with_num_threads(1, REFRESH_WRITER_HEAP)
            .map_err(writer_error)
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
        let sort_key = format!("{}\u{0}{}", book.title.to_ascii_lowercase(), book.uuid);
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
                self.listing => ListingRef::of_book(book).term(),
                self.sort => sort_key,
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
        let Some(query) = self.parse(raw_query)? else {
            return Ok(Vec::new());
        };
        let searcher = self.reader()?.searcher();

        // Classic `-n 0` means every hit. Cap at the index size so TopDocs
        // never gets a zero limit (which panics) while still returning all
        // matches for large libraries.
        let top = if limit == 0 {
            let n = usize::try_from(searcher.num_docs()).unwrap_or(usize::MAX);
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

    /// Opens an index and reads one title-ordered page of a query's hits
    /// without blocking the caller's task (see [`Self::open_and_search`]).
    ///
    /// # Errors
    ///
    /// Propagates index-open and query failures.
    pub async fn open_and_search_page(
        dir: PathBuf,
        query: String,
        offset: usize,
        limit: usize,
    ) -> Result<SearchPage> {
        tokio::task::spawn_blocking(move || Self::open(&dir)?.search_page(&query, offset, limit))
            .await
            .map_err(|err| SearchError::Index(format!("search task join error: {err}")))?
    }

    /// Opens an index and lists every hit of a query without blocking the
    /// caller's task (see [`Self::open_and_search`]).
    ///
    /// # Errors
    ///
    /// Propagates index-open and query failures.
    pub async fn open_and_match_uuids(dir: PathBuf, query: String) -> Result<Vec<String>> {
        tokio::task::spawn_blocking(move || Self::open(&dir)?.match_uuids(&query))
            .await
            .map_err(|err| SearchError::Index(format!("search task join error: {err}")))?
    }

    /// One page of a query's hits in case-insensitive title order, with the
    /// number of matching documents.
    ///
    /// Sorting, paging and counting all happen in the index, so no hit cap
    /// applies and only `limit` uuids leave it. Blocking.
    ///
    /// # Arguments
    ///
    /// * `raw_query` - Lucene-style query (normalized before parse).
    /// * `offset` - Hits to skip.
    /// * `limit` - Most hits to return; 0 returns only the count.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::Query`] for a query that does not parse, or
    /// [`SearchError::Index`] when the index cannot be read.
    pub fn search_page(&self, raw_query: &str, offset: usize, limit: usize) -> Result<SearchPage> {
        let Some(query) = self.parse(raw_query)? else {
            return Ok(SearchPage::default());
        };
        let searcher = self.reader()?.searcher();
        if limit == 0 {
            let total = searcher
                .search(&query, &Count)
                .map_err(|err| SearchError::Index(err.to_string()))?;
            return Ok(SearchPage {
                uuids: Vec::new(),
                total,
            });
        }
        let page = TopDocs::with_limit(limit)
            .and_offset(offset)
            .order_by_string_fast_field("sort", Order::Asc);
        let (total, docs) = searcher
            .search(&query, &(Count, page))
            .map_err(|err| SearchError::Index(err.to_string()))?;
        let mut uuids = Vec::with_capacity(docs.len());
        for (_, addr) in docs {
            let doc: TantivyDocument = searcher
                .doc(addr)
                .map_err(|err| SearchError::Index(err.to_string()))?;
            if let Some(uuid) = doc.get_first(self.uuid).and_then(|v| v.as_str()) {
                uuids.push(uuid.to_string());
            }
        }
        Ok(SearchPage { uuids, total })
    }

    /// The uuid of every document a query matches, in title order.
    ///
    /// For callers that must filter the hits by something the index does
    /// not hold authoritatively; there is no cap. Blocking.
    ///
    /// # Errors
    ///
    /// As [`Self::search_page`].
    pub fn match_uuids(&self, raw_query: &str) -> Result<Vec<String>> {
        let total = self.search_page(raw_query, 0, 0)?.total;
        if total == 0 {
            return Ok(Vec::new());
        }
        Ok(self.search_page(raw_query, 0, total)?.uuids)
    }

    /// A reader on the latest commit.
    ///
    /// Each search opens its own reader and drops it. A commit watcher
    /// would spawn a thread per request and panic if that spawn fails,
    /// which the books handler turns into an empty 500.
    fn reader(&self) -> Result<tantivy::IndexReader> {
        self.index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|err| SearchError::Index(err.to_string()))
    }

    /// Parses a Lucene-style query over the searchable fields, or `None`
    /// when it normalizes to nothing.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::Query`] when the query does not parse.
    fn parse(&self, raw_query: &str) -> Result<Option<Box<dyn Query>>> {
        let query_str = normalize_lucene_query(raw_query);
        if query_str.is_empty() {
            return Ok(None);
        }
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
        parser
            .parse_query(&query_str)
            .map(Some)
            .map_err(|err| SearchError::Query(format!("{err}")))
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
            .upsert_account("audible", "acct", "us", None, true, "audible")
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
            .upsert_account("audible", "acct", "us", None, true, "audible")
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
            .upsert_account("audible", "acct", "us", None, true, "audible")
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

    /// A store with one account and `titles` minimal rows on it.
    async fn store_with(titles: &[(&str, &str)]) -> LibraryStore {
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        library
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .unwrap();
        for (product, title) in titles {
            library
                .upsert_book(&NewBook::minimal(*product, "acct", "us", *title))
                .await
                .unwrap();
        }
        library
    }

    #[tokio::test]
    async fn pages_hits_in_title_order_with_the_full_count() {
        let dir = tempfile::tempdir().unwrap();
        let library = store_with(&[
            ("B3", "gamma shelf"),
            ("B1", "Alpha shelf"),
            ("B2", "beta shelf"),
            ("B4", "unrelated"),
        ])
        .await;
        let engine = SearchEngine::open(dir.path()).unwrap();
        engine.rebuild(&library).await.unwrap();

        let uuid_of = |product: &'static str| {
            let library = &library;
            async move {
                library
                    .get_book(product, "acct")
                    .await
                    .unwrap()
                    .unwrap()
                    .uuid
                    .to_ascii_lowercase()
            }
        };
        let first = engine.search_page("shelf", 0, 2).unwrap();
        assert_eq!(first.total, 3);
        assert_eq!(first.uuids, vec![uuid_of("B1").await, uuid_of("B2").await]);
        let second = engine.search_page("shelf", 2, 2).unwrap();
        assert_eq!(second.total, 3);
        assert_eq!(second.uuids, vec![uuid_of("B3").await]);
        assert_eq!(
            engine.search_page("shelf", 0, 0).unwrap(),
            SearchPage {
                uuids: Vec::new(),
                total: 3
            }
        );
        assert_eq!(engine.match_uuids("shelf").unwrap().len(), 3);
        assert_eq!(
            engine.search_page("  ", 0, 10).unwrap(),
            SearchPage::default()
        );
        assert!(engine.match_uuids("nothing-matches").unwrap().is_empty());
    }

    /// The old handler kept the best 500 hits and dropped the rest.
    #[tokio::test]
    async fn search_has_no_hit_cap() {
        let dir = tempfile::tempdir().unwrap();
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        library
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .unwrap();
        let n = 520usize;
        for i in 0..n {
            library
                .upsert_book(&NewBook::minimal(
                    format!("C{i:05}"),
                    "acct",
                    "us",
                    format!("Common {i:05}"),
                ))
                .await
                .unwrap();
        }
        let engine = SearchEngine::open(dir.path()).unwrap();
        engine.rebuild(&library).await.unwrap();
        let tail = engine.search_page("common", 510, 40).unwrap();
        assert_eq!(tail.total, n);
        assert_eq!(tail.uuids.len(), 10);
        assert_eq!(engine.match_uuids("common").unwrap().len(), n);
    }

    #[tokio::test]
    async fn only_a_finished_rebuild_marks_the_index_complete() {
        let dir = tempfile::tempdir().unwrap();
        let library = store_with(&[("B1", "Alpha")]).await;
        let engine = SearchEngine::open(dir.path()).unwrap();
        assert!(!SearchEngine::is_complete(dir.path()));
        engine.rebuild(&library).await.unwrap();
        assert!(SearchEngine::is_complete(dir.path()));
    }

    #[tokio::test]
    async fn an_index_of_an_older_layout_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut old = Schema::builder();
            let title = old.add_text_field("title", TEXT | STORED);
            let index = Index::create_in_dir(dir.path(), old.build()).unwrap();
            let mut writer: IndexWriter = index.writer(15_000_000).unwrap();
            writer.add_document(doc!(title => "Old")).unwrap();
            writer.commit().unwrap();
        }
        std::fs::write(dir.path().join("bookclerk-complete-v1"), b"").unwrap();
        let engine = SearchEngine::open(dir.path()).unwrap();
        assert!(!SearchEngine::is_complete(dir.path()));
        assert!(!dir.path().join("bookclerk-complete-v1").exists());
        assert!(engine.search("old", 10).unwrap().is_empty());
    }

    #[test]
    fn a_directory_that_is_not_an_index_is_never_emptied() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"x").unwrap();
        assert!(SearchEngine::discard_index(dir.path()).is_err());
        assert!(dir.path().join("keep.txt").exists());
    }

    #[tokio::test]
    async fn a_listing_refresh_replaces_its_titles_and_drops_stale_ones() {
        let dir = tempfile::tempdir().unwrap();
        let library = store_with(&[("B1", "Moved title"), ("B2", "Kept title")]).await;
        let engine = SearchEngine::open(dir.path()).unwrap();
        engine.rebuild(&library).await.unwrap();
        let moved = library.get_book("B1", "acct").await.unwrap().unwrap();

        // The row moves to another account; the index still has the old one.
        library
            .upsert_account("audible", "acct-2", "us", None, true, "audible")
            .await
            .unwrap();
        library
            .remap_account_id("audible", "acct", "acct-2")
            .await
            .unwrap();
        let hits = engine.search("moved", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].account_id, "acct");

        let written = engine
            .refresh_listings(&library, &[ListingRef::of_book(&moved)])
            .await
            .unwrap();
        assert_eq!(written, 1);
        let hits = engine.search("moved", 10).unwrap();
        assert_eq!(hits.len(), 1, "the stale document is gone");
        assert_eq!(hits[0].account_id, "acct-2");
        assert_eq!(engine.search("kept", 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_title_refresh_picks_up_a_new_row_and_drops_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let library = store_with(&[]).await;
        let engine = SearchEngine::open(dir.path()).unwrap();
        engine.rebuild(&library).await.unwrap();
        let saved = library
            .upsert_book(&NewBook::minimal("B9", "acct", "us", "Fresh title"))
            .await
            .unwrap();
        assert!(engine.search("fresh", 10).unwrap().is_empty());
        let written = engine
            .refresh_titles(&library, &[saved.uuid.clone(), "no-such-title".into()])
            .await
            .unwrap();
        assert_eq!(written, 1);
        assert_eq!(engine.search("fresh", 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_second_writer_is_reported_busy() {
        let dir = tempfile::tempdir().unwrap();
        let library = store_with(&[("B1", "Alpha")]).await;
        let engine = SearchEngine::open(dir.path()).unwrap();
        let _held = engine.refresh_writer().unwrap();
        assert!(matches!(
            engine.refresh_titles(&library, &[]).await,
            Err(SearchError::Busy)
        ));
        assert!(matches!(
            engine.rebuild(&library).await,
            Err(SearchError::Busy)
        ));
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
