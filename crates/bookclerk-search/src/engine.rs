//! Tantivy index build and search.

use std::path::Path;

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
    pub account_id: String,
    pub title: String,
    pub score: f32,
}

/// On-disk Tantivy search engine (classic Lucene `SearchEngine` parity).
pub struct SearchEngine {
    index: Index,
    id: Field,
    uuid: Field,
    product_id: Field,
    isbn: Field,
    asin: Field,
    account: Field,
    title: Field,
    authors: Field,
    narrators: Field,
    series: Field,
    tags: Field,
    all: Field,
    acquired: Field,
    finished: Field,
}

impl SearchEngine {
    /// Open or create a search index at `dir`.
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
    pub fn rebuild(&self, library: &LibraryStore) -> Result<usize> {
        let mut writer = self
            .index
            .writer(50_000_000)
            .map_err(|err| SearchError::Index(err.to_string()))?;
        writer
            .delete_all_documents()
            .map_err(|err| SearchError::Index(err.to_string()))?;

        let books = library.list_books(None)?;
        for book in &books {
            self.add_book(&mut writer, book)?;
        }
        writer
            .commit()
            .map_err(|err| SearchError::Index(err.to_string()))?;
        Ok(books.len())
    }

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

    /// Search the index. `limit` of 0 returns all matches (classic `-n 0`).
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
        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(top))
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
                account_id,
                title,
                score,
            });
        }
        Ok(hits)
    }
}

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

    #[test]
    fn indexes_and_finds_by_title() {
        let dir = tempfile::tempdir().unwrap();
        let library = LibraryStore::open_in_memory().unwrap();
        library.upsert_account("acct", "us", None, true).unwrap();
        let mut book = NewBook::minimal("B00TEST", "acct", "us", "Harry Potter");
        book.authors = Some("Rowling".into());
        library.upsert_book(&book).unwrap();

        let engine = SearchEngine::open(dir.path()).unwrap();
        engine.rebuild(&library).unwrap();
        let hits = engine.search("potter", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].asin, "B00TEST");
    }

    #[test]
    fn indexes_uuid_product_id_isbn_asin() {
        let dir = tempfile::tempdir().unwrap();
        let library = LibraryStore::open_in_memory().unwrap();
        library.upsert_account("acct", "us", None, true).unwrap();
        let mut book = NewBook::minimal("B00TEST01", "acct", "us", "Indexed Book");
        book.isbn = Some("9781234567890".into());
        library.upsert_book(&book).unwrap();
        let stored = library.get_book("B00TEST01", "acct").unwrap().unwrap();

        let engine = SearchEngine::open(dir.path()).unwrap();
        engine.rebuild(&library).unwrap();

        let by_uuid = engine.search(&stored.uuid, 10).unwrap();
        assert_eq!(by_uuid.len(), 1);
        assert_eq!(by_uuid[0].asin, "B00TEST01");

        assert_eq!(engine.search("B00TEST01", 10).unwrap().len(), 1);
        assert_eq!(engine.search("9781234567890", 10).unwrap().len(), 1);
        assert_eq!(engine.search("asin:b00test01", 10).unwrap().len(), 1);
        assert_eq!(engine.search("isbn:9781234567890", 10).unwrap().len(), 1);
    }
}
