//! Library export (classic `export --csv|--json|--xlsx`).

use std::path::Path;

use bookclerk_library::{BookRecord, LibraryStore};
use rust_xlsxwriter::Workbook;

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

pub fn export_json(path: &Path, books: &[BookRecord]) -> anyhow::Result<()> {
    let text = serde_json::to_string_pretty(books)?;
    std::fs::write(path, text)?;
    Ok(())
}

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

pub fn load_books(store: &LibraryStore, account: Option<&str>) -> anyhow::Result<Vec<BookRecord>> {
    Ok(store.list_books(account)?)
}
