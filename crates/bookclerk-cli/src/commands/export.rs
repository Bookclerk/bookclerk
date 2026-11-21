//! Library export (`export library` / `library export` with `--csv`,
//! `--json` or `--xlsx`).
//!
//! One row per account title (holding), read from the book model. Without
//! a user the export is the whole library and the per-person columns
//! (`tags`, `is_finished`) are empty; with a user they carry that person's
//! own state for the title's work.

use std::collections::HashMap;
use std::path::Path;

use bookclerk_library::{AcquireTitle, LibraryStore, UserRecord};
use rust_xlsxwriter::Workbook;
use serde::Serialize;

/// The spreadsheet columns, in order.
const COLUMNS: [&str; 11] = [
    "asin",
    "account_id",
    "title",
    "authors",
    "narrators",
    "series",
    "acquire_status",
    "pdf_status",
    "storage_key",
    "tags",
    "is_finished",
];

/// Titles read per page while loading the export.
const EXPORT_PAGE: u64 = 64;

/// One exported title: an account's holding of an edition, with the
/// exporting person's state for its work when a person was named.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportRow {
    /// The account holding (`holdings.id`).
    pub holding_id: String,
    /// The held edition (`editions.id`).
    pub edition_id: String,
    /// The edition's work (`works.id`); `None` when the edition was moved
    /// away while the export read it.
    pub work_id: Option<String>,
    /// Display alias of the store plugin (`audible`, `libro`, ...).
    pub source: String,
    /// Store account that holds the title.
    pub account_id: String,
    /// Store marketplace of the listing.
    pub marketplace: String,
    /// The store's product id for the title.
    pub product_id: String,
    /// The title's ASIN, when the store has one.
    pub asin: Option<String>,
    /// The title's ISBN, when known.
    pub isbn: Option<String>,
    /// Title as the store lists it.
    pub title: String,
    /// Subtitle, when listed.
    pub subtitle: Option<String>,
    /// Authors, comma separated.
    pub authors: Option<String>,
    /// Narrators, comma separated.
    pub narrators: Option<String>,
    /// Series name, when in one.
    pub series: Option<String>,
    /// Position in the series, as listed.
    pub series_index: Option<String>,
    /// The edition's audio state (`acquired`, `not_acquired`, ...).
    pub acquire_status: String,
    /// The edition's companion PDF state.
    pub pdf_status: String,
    /// Destination key of the audio file once acquired.
    pub storage_key: Option<String>,
    /// The person's tags for the work, comma separated; `None` without a
    /// person or when they set none.
    pub tags: Option<String>,
    /// Whether the person finished the work; `None` without a person, so
    /// the cell is empty (JSON `null`) rather than claiming "unfinished".
    pub is_finished: Option<bool>,
}

impl ExportRow {
    /// The row of account title `title`, with no work or per-person state
    /// yet.
    fn of(title: AcquireTitle) -> Self {
        Self {
            holding_id: title.holding_id,
            edition_id: title.edition_id,
            work_id: None,
            source: title.source,
            account_id: title.account_id,
            marketplace: title.marketplace,
            product_id: title.product_id,
            asin: title.asin,
            isbn: title.isbn,
            title: title.title,
            subtitle: title.subtitle,
            authors: title.authors,
            narrators: title.narrators,
            series: title.series,
            series_index: title.series_index,
            acquire_status: title.status.as_str().to_string(),
            pdf_status: title.pdf_status.as_str().to_string(),
            storage_key: title.storage_key,
            tags: None,
            is_finished: None,
        }
    }

    /// The id the title is exported under: its ASIN, else its ISBN, else
    /// its store product id.
    fn label(&self) -> &str {
        self.asin
            .as_deref()
            .or(self.isbn.as_deref())
            .unwrap_or(self.product_id.as_str())
    }

    /// The row's spreadsheet cells, in [`COLUMNS`] order.
    fn cells(&self) -> [&str; 11] {
        [
            self.label(),
            &self.account_id,
            &self.title,
            self.authors.as_deref().unwrap_or(""),
            self.narrators.as_deref().unwrap_or(""),
            self.series.as_deref().unwrap_or(""),
            &self.acquire_status,
            &self.pdf_status,
            self.storage_key.as_deref().unwrap_or(""),
            self.tags.as_deref().unwrap_or(""),
            match self.is_finished {
                Some(true) => "true",
                Some(false) => "false",
                None => "",
            },
        ]
    }

    /// True when `id` names this title: its holding id, product id, ISBN
    /// or ASIN (case-insensitive).
    fn matches(&self, id: &str) -> bool {
        id.eq_ignore_ascii_case(&self.holding_id)
            || id.eq_ignore_ascii_case(&self.product_id)
            || self
                .isbn
                .as_deref()
                .is_some_and(|isbn| id.eq_ignore_ascii_case(isbn))
            || self
                .asin
                .as_deref()
                .is_some_and(|asin| id.eq_ignore_ascii_case(asin))
    }
}

/// Writes rows as CSV with the classic export column set.
///
/// # Errors
///
/// Returns an error when the file cannot be written.
pub fn export_csv(path: &Path, rows: &[ExportRow]) -> anyhow::Result<()> {
    let mut writer = csv::Writer::from_path(path)?;
    writer.write_record(COLUMNS)?;
    for row in rows {
        writer.write_record(row.cells())?;
    }
    writer.flush()?;
    Ok(())
}

/// Writes rows as a pretty-printed JSON array of [`ExportRow`].
///
/// # Errors
///
/// Returns an error when the file cannot be written.
pub fn export_json(path: &Path, rows: &[ExportRow]) -> anyhow::Result<()> {
    let text = serde_json::to_string_pretty(rows)?;
    std::fs::write(path, text)?;
    Ok(())
}

/// Writes rows to an `.xlsx` workbook with the same columns as CSV
/// (`is_finished` as a boolean cell, blank without a person).
///
/// # Errors
///
/// Returns an error when the sheet is too large for the format or the file
/// cannot be written.
pub fn export_xlsx(path: &Path, rows: &[ExportRow]) -> anyhow::Result<()> {
    let mut workbook = Workbook::new();
    let worksheet = workbook.add_worksheet();
    for (col, header) in (0u16..).zip(COLUMNS) {
        worksheet.write(0, col, header)?;
    }
    for (index, row) in rows.iter().enumerate() {
        let r = u32::try_from(index + 1)?;
        for (col, cell) in (0u16..).zip(row.cells()).take(COLUMNS.len() - 1) {
            worksheet.write(r, col, cell)?;
        }
        if let Some(finished) = row.is_finished {
            worksheet.write(r, 10, finished)?;
        }
    }
    workbook.save(path)?;
    Ok(())
}

/// Reads the export: every account title, optionally of one account and
/// narrowed to `ids`, with `user`'s state for each title's work.
///
/// # Arguments
///
/// * `store` - The library.
/// * `account` - Only this account's titles (id, stem or nickname), or all.
/// * `ids` - Keep only titles one of these names (holding id, book uuid,
///   product id, ISBN or ASIN); empty keeps all.
/// * `user` - Whose tags and finished flags to fill in; `None` leaves them
///   empty.
///
/// # Errors
///
/// Returns an error when a read fails.
pub async fn load_rows(
    store: &LibraryStore,
    account: Option<&str>,
    ids: &[String],
    user: Option<&UserRecord>,
) -> anyhow::Result<Vec<ExportRow>> {
    let mut rows: Vec<ExportRow> = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = store
            .acquire_titles_page(account, after.as_deref(), EXPORT_PAGE)
            .await?;
        rows.extend(page.titles.into_iter().filter_map(|title| {
            let row = ExportRow::of(title);
            (ids.is_empty() || ids.iter().any(|id| row.matches(id))).then_some(row)
        }));
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    // Works and the person's state are read only for the titles kept.
    let works = store
        .works_of_editions(rows.iter().map(|row| row.edition_id.clone()).collect())
        .await?;
    let states: HashMap<String, (Option<String>, bool)> = match user {
        Some(user) => {
            let work_ids: Vec<String> = works.values().cloned().collect();
            store
                .user_title_states_of_works(user.id, &work_ids)
                .await?
                .into_iter()
                .map(|state| (state.work_id, (state.tags, state.is_finished)))
                .collect()
        }
        None => HashMap::new(),
    };
    for row in &mut rows {
        row.work_id = works.get(&row.edition_id).cloned();
        if user.is_some() {
            let (tags, finished) = row
                .work_id
                .as_ref()
                .and_then(|work| states.get(work).cloned())
                .unwrap_or((None, false));
            row.tags = tags.filter(|tags| !tags.is_empty());
            row.is_finished = Some(finished);
        }
    }
    Ok(rows)
}

/// Writes `rows` to `path` in the format the flags or the extension pick
/// (CSV when neither does).
///
/// # Errors
///
/// Returns an error when the file cannot be written.
pub fn write_export(
    path: &Path,
    rows: &[ExportRow],
    csv: bool,
    json: bool,
    xlsx: bool,
) -> anyhow::Result<()> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if csv || (!json && !xlsx && ext == "csv") {
        export_csv(path, rows)
    } else if json || ext == "json" {
        export_json(path, rows)
    } else if xlsx || ext == "xlsx" {
        export_xlsx(path, rows)
    } else {
        export_csv(path, rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::{AcquireStatus, NewBook, UserBookFields, UserRole};

    /// A library with two accounts' titles, one acquired, and Riley's own
    /// state on the acquired title's work.
    async fn library() -> (LibraryStore, UserRecord) {
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("sqlite memory"),
        );
        for account in ["acct", "other"] {
            store
                .upsert_account("audible", account, "us", None, true, "audible")
                .await
                .unwrap();
        }
        store
            .upsert_book(&NewBook::minimal("B0EXPORT01", "acct", "us", "First"))
            .await
            .unwrap();
        store
            .upsert_book(&NewBook::minimal("B0EXPORT02", "other", "us", "Second"))
            .await
            .unwrap();
        store
            .set_acquire_status(
                "B0EXPORT01",
                "acct",
                AcquireStatus::Acquired,
                Some("a/First.m4b"),
                None,
            )
            .await
            .unwrap();
        let riley = store
            .create_user_with_login(UserRole::Member, Some("Riley"), Some("riley"), None)
            .await
            .unwrap();
        let rows = load_rows(&store, None, &["B0EXPORT01".into()], None)
            .await
            .unwrap();
        let work = rows[0].work_id.clone().unwrap();
        store
            .set_user_title_state(
                riley.id,
                &work,
                &UserBookFields {
                    tags: Some("favourite,re-listen".into()),
                    is_finished: Some(true),
                    ..UserBookFields::default()
                },
            )
            .await
            .unwrap();
        (store, riley)
    }

    /// Without a person every title is exported with the per-person
    /// columns empty; with one, that person's tags and finished flag fill
    /// them.
    #[tokio::test]
    async fn the_whole_library_or_one_persons_view() {
        let (store, riley) = library().await;

        let mut all = load_rows(&store, None, &[], None).await.unwrap();
        all.sort_by(|a, b| a.product_id.cmp(&b.product_id));
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].acquire_status, "acquired");
        assert_eq!(all[0].storage_key.as_deref(), Some("a/First.m4b"));
        assert_eq!(all[1].acquire_status, "not_acquired");
        assert!(all
            .iter()
            .all(|row| row.tags.is_none() && row.is_finished.is_none()));

        let mut view = load_rows(&store, None, &[], Some(&riley)).await.unwrap();
        view.sort_by(|a, b| a.product_id.cmp(&b.product_id));
        assert_eq!(view.len(), 2, "a person's view lists the whole library");
        assert_eq!(view[0].tags.as_deref(), Some("favourite,re-listen"));
        assert_eq!(view[0].is_finished, Some(true));
        assert!(view[1].tags.is_none());
        assert_eq!(
            view[1].is_finished,
            Some(false),
            "a person's view says unfinished"
        );

        let other = load_rows(&store, Some("other"), &[], Some(&riley))
            .await
            .unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].product_id, "B0EXPORT02");
    }

    /// Every format writes the same rows: CSV and the workbook with the
    /// classic columns, JSON as the row objects.
    #[tokio::test]
    async fn each_format_writes_the_rows() {
        let (store, riley) = library().await;
        let rows = load_rows(&store, Some("acct"), &[], Some(&riley))
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();

        let csv_path = dir.path().join("library.csv");
        write_export(&csv_path, &rows, false, false, false).unwrap();
        let text = std::fs::read_to_string(&csv_path).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(COLUMNS.join(",").as_str()));
        assert_eq!(
            lines.next(),
            Some(
                "B0EXPORT01,acct,First,,,,acquired,not_acquired,a/First.m4b,\"favourite,re-listen\",true"
            )
        );

        let json_path = dir.path().join("library.out");
        write_export(&json_path, &rows, false, true, false).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
        assert_eq!(parsed[0]["product_id"], "B0EXPORT01");
        assert_eq!(parsed[0]["is_finished"], true);
        assert_eq!(parsed[0]["holding_id"], rows[0].holding_id.as_str());

        let xlsx_path = dir.path().join("library.xlsx");
        write_export(&xlsx_path, &rows, false, false, false).unwrap();
        assert!(std::fs::metadata(&xlsx_path).unwrap().len() > 0);

        // Without a person the finished cell is empty, not "false".
        let whole = load_rows(&store, Some("acct"), &[], None).await.unwrap();
        let whole_csv = dir.path().join("whole.csv");
        write_export(&whole_csv, &whole, false, false, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&whole_csv).unwrap().lines().nth(1),
            Some("B0EXPORT01,acct,First,,,,acquired,not_acquired,a/First.m4b,,")
        );
        let whole_json = dir.path().join("whole.json");
        write_export(&whole_json, &whole, false, false, false).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&whole_json).unwrap()).unwrap();
        assert!(parsed[0]["is_finished"].is_null());
        assert!(parsed[0]["tags"].is_null());
        let whole_xlsx = dir.path().join("whole.xlsx");
        write_export(&whole_xlsx, &whole, false, false, false).unwrap();
        assert!(std::fs::metadata(&whole_xlsx).unwrap().len() > 0);

        let fallback = dir.path().join("library.txt");
        write_export(&fallback, &rows, false, false, false).unwrap();
        assert!(std::fs::read_to_string(&fallback)
            .unwrap()
            .starts_with("asin,"));
    }

    /// Titles are kept by any id that names them, case-insensitively.
    #[tokio::test]
    async fn ids_narrow_the_export() {
        let (store, _) = library().await;
        let all = load_rows(&store, None, &[], None).await.unwrap();
        let first = all
            .iter()
            .find(|row| row.product_id == "B0EXPORT01")
            .unwrap();
        for id in [first.holding_id.clone(), "b0export01".to_string()] {
            let rows = load_rows(&store, None, &[id], None).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].product_id, "B0EXPORT01");
        }
        assert!(load_rows(&store, None, &["nothing".into()], None)
            .await
            .unwrap()
            .is_empty());
    }
}
