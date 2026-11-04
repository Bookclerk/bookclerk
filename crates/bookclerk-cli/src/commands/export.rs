//! Library export (classic `export --csv|--json|--xlsx`).

use std::path::Path;

use bookclerk_library::{BookRecord, LibraryStore};
use rust_xlsxwriter::Workbook;

/// Writes library rows as CSV with the classic export column set.
pub fn export_csv(path: &Path, books: &[BookRecord]) -> anyhow::Result<()> {
    let mut writer = csv::Writer::from_path(path)?;
    writer.write_record([
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
    ])?;
    for book in books {
        let finished = if book.is_finished { "true" } else { "false" };
        writer.write_record([
            book.asin_or_isbn(),
            &book.account_id,
            &book.title,
            book.authors.as_deref().unwrap_or(""),
            book.narrators.as_deref().unwrap_or(""),
            book.series.as_deref().unwrap_or(""),
            book.acquire_status.as_str(),
            book.pdf_status.as_str(),
            book.storage_key.as_deref().unwrap_or(""),
            book.tags.as_deref().unwrap_or(""),
            finished,
        ])?;
    }
    writer.flush()?;
    Ok(())
}

/// Writes library rows as pretty-printed JSON (`BookRecord` array).
pub fn export_json(path: &Path, books: &[BookRecord]) -> anyhow::Result<()> {
    let text = serde_json::to_string_pretty(books)?;
    std::fs::write(path, text)?;
    Ok(())
}

/// Writes a `library export-state` document as pretty JSON.
///
/// # Arguments
///
/// * `path` - Destination file.
/// * `doc` - The exported state.
/// * `force` - Replace `path` when it exists; otherwise an existing file is
///   an error, so an earlier export is never overwritten by accident.
///
/// # Errors
///
/// Returns an error when the file exists (without `force`), or when it cannot
/// be created or written.
pub fn write_title_state(
    path: &Path,
    doc: &bookclerk_library::TitleStateExport,
    force: bool,
) -> anyhow::Result<()> {
    use std::io::Write as _;

    let text = serde_json::to_string_pretty(doc)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = options.open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow::anyhow!(
                "{} already exists; pass --force to replace it",
                path.display()
            )
        } else {
            anyhow::Error::new(e).context(format!("create {}", path.display()))
        }
    })?;
    file.write_all(text.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

/// Writes library rows to an `.xlsx` workbook with the same columns as CSV.
pub fn export_xlsx(path: &Path, books: &[BookRecord]) -> anyhow::Result<()> {
    let mut workbook = Workbook::new();
    let worksheet = workbook.add_worksheet();
    let headers = [
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
    for (col, header) in headers.iter().enumerate() {
        worksheet.write(0, col as u16, *header)?;
    }
    for (row, book) in books.iter().enumerate() {
        let r = (row + 1) as u32;
        worksheet.write(r, 0, book.asin_or_isbn())?;
        worksheet.write(r, 1, &book.account_id)?;
        worksheet.write(r, 2, &book.title)?;
        worksheet.write(r, 3, book.authors.as_deref().unwrap_or(""))?;
        worksheet.write(r, 4, book.narrators.as_deref().unwrap_or(""))?;
        worksheet.write(r, 5, book.series.as_deref().unwrap_or(""))?;
        worksheet.write(r, 6, book.acquire_status.as_str())?;
        worksheet.write(r, 7, book.pdf_status.as_str())?;
        worksheet.write(r, 8, book.storage_key.as_deref().unwrap_or(""))?;
        worksheet.write(r, 9, book.tags.as_deref().unwrap_or(""))?;
        worksheet.write(r, 10, book.is_finished)?;
    }
    workbook.save(path)?;
    Ok(())
}

/// Keeps books whose uuid, product id, ISBN, or ASIN matches `asins` (case-insensitive).
pub fn filter_books(books: Vec<BookRecord>, asins: Option<&[String]>) -> Vec<BookRecord> {
    match asins {
        None | Some([]) => books,
        Some(list) => books
            .into_iter()
            .filter(|b| {
                list.iter().any(|a| {
                    a.eq_ignore_ascii_case(&b.uuid)
                        || a.eq_ignore_ascii_case(&b.product_id)
                        || b.isbn
                            .as_ref()
                            .is_some_and(|isbn| a.eq_ignore_ascii_case(isbn))
                        || b.asin
                            .as_ref()
                            .is_some_and(|asin| a.eq_ignore_ascii_case(asin))
                })
            })
            .collect(),
    }
}

/// Lists library books, optionally scoped to one account id.
pub async fn load_books(
    store: &LibraryStore,
    account: Option<&str>,
) -> anyhow::Result<Vec<BookRecord>> {
    Ok(store.list_books(account).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::{TitleStateExport, TITLE_STATE_FORMAT, TITLE_STATE_VERSION};
    use chrono::TimeZone;

    /// An empty export document.
    fn doc() -> TitleStateExport {
        TitleStateExport {
            format: TITLE_STATE_FORMAT.to_string(),
            version: TITLE_STATE_VERSION,
            exported_at: chrono::Utc.with_ymd_and_hms(2026, 11, 4, 3, 0, 0).unwrap(),
            titles: vec![],
            ignored: vec![],
            listening_links: vec![],
            request_links: vec![],
        }
    }

    #[test]
    fn title_state_is_written_once_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        write_title_state(&path, &doc(), false).unwrap();
        let back: TitleStateExport =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, doc());

        let err = write_title_state(&path, &doc(), false).unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");

        std::fs::write(&path, "x".repeat(4096)).unwrap();
        write_title_state(&path, &doc(), true).unwrap();
        let back: TitleStateExport =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, doc());
    }

    #[test]
    fn title_state_reports_an_unwritable_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("state.json");
        let err = write_title_state(&path, &doc(), false).unwrap_err();
        assert!(err.to_string().contains("create"), "{err}");
    }
}
