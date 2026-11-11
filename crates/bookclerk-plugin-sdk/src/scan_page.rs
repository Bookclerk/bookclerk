//! Paging helper for a storefront's `scan`.
//!
//! A store page can hold more titles than one reply may carry
//! ([`MAX_SCAN_PAGE_BOOKS`]). [`scan_page`] fetches the store page the cursor
//! names, returns the next slice of it, and writes a cursor that names both
//! the store page and the slice, so the plugin pages once and stays stateless
//! between calls. Cursors match the TypeScript SDK's `scanPage` and the Python
//! SDK's `scan_page`.

use std::future::Future;

use crate::{
    PluginError, ScanBook, ScanPage, ScanParams, MAX_SCAN_CURSOR_BYTES, MAX_SCAN_PAGE_BOOKS,
};

/// Prefix of every cursor [`scan_page`] writes; bumps if the layout changes.
const CURSOR_PREFIX: &str = "sp1.";

/// Longest offset a cursor carries, in decimal digits (as the other SDKs).
const MAX_OFFSET_DIGITS: usize = 16;

/// One page of a storefront's own library listing, as the plugin fetched it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorePage {
    /// Titles on this store page, in the store's order; any number of them.
    pub books: Vec<ScanBook>,
    /// Product ids gone from the account since `ScanParams::sync_token`, on a
    /// delta pass only; any number of them.
    pub removed: Vec<String>,
    /// The storefront's own cursor for the store page after this one; empty
    /// when this is the last store page.
    pub next: String,
    /// On the last store page: the token the host sends as
    /// `ScanParams::sync_token` on the account's next pass. Ignored on
    /// earlier pages.
    pub sync_token: String,
    /// On the last store page: whether the pass listed every title the
    /// account holds (a full pass). Ignored on earlier pages.
    pub complete: bool,
    /// Storefront requests this fetch took; `None` counts one.
    pub store_pages: Option<u32>,
}

/// Where a [`scan_page`] cursor points.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScanPosition {
    /// The storefront's own cursor for the store page (empty for the first).
    store_cursor: String,
    /// Index of the first title (and removed id) the reply carries; a
    /// multiple of [`MAX_SCAN_PAGE_BOOKS`].
    offset: usize,
}

/// [`MAX_SCAN_PAGE_BOOKS`] as an index width.
fn page_width() -> usize {
    usize::try_from(MAX_SCAN_PAGE_BOOKS).unwrap_or(usize::MAX)
}

/// Cursor naming `offset` into the store page `store_cursor` names.
///
/// # Errors
///
/// Returns an `internal` [`PluginError`] when the cursor would pass
/// [`MAX_SCAN_CURSOR_BYTES`].
fn write_cursor(store_cursor: &str, offset: usize) -> Result<String, PluginError> {
    let cursor = format!("{CURSOR_PREFIX}{offset}.{store_cursor}");
    let cap = usize::try_from(MAX_SCAN_CURSOR_BYTES).unwrap_or(usize::MAX);
    if cursor.len() > cap {
        return Err(PluginError::internal(format!(
            "store cursor too long: a scan cursor is at most {MAX_SCAN_CURSOR_BYTES} bytes"
        )));
    }
    Ok(cursor)
}

/// Position a scan cursor names: the first store page for the empty cursor
/// a pass starts with, otherwise what [`write_cursor`] wrote.
///
/// # Errors
///
/// Returns an `invalid_cursor` [`PluginError`] for a cursor this helper did
/// not write, so the host restarts the pass.
fn read_cursor(cursor: &str) -> Result<ScanPosition, PluginError> {
    if cursor.is_empty() {
        return Ok(ScanPosition {
            store_cursor: String::new(),
            offset: 0,
        });
    }
    let stale = || PluginError::invalid_cursor("scan cursor was not written by scan_page");
    let rest = cursor.strip_prefix(CURSOR_PREFIX).ok_or_else(stale)?;
    let (digits, store_cursor) = rest.split_once('.').ok_or_else(stale)?;
    let canonical = !digits.is_empty()
        && digits.len() <= MAX_OFFSET_DIGITS
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'));
    if !canonical {
        return Err(stale());
    }
    let offset: usize = digits.parse().map_err(|_| stale())?;
    if !offset.is_multiple_of(page_width()) {
        return Err(stale());
    }
    Ok(ScanPosition {
        store_cursor: store_cursor.to_owned(),
        offset,
    })
}

/// Items `[offset, offset + MAX_SCAN_PAGE_BOOKS)` of `items`.
fn slice<T: Clone>(items: &[T], offset: usize) -> Vec<T> {
    items
        .iter()
        .skip(offset)
        .take(page_width())
        .cloned()
        .collect()
}

/// Answers one `scan` call from the storefront's own pages.
///
/// Fetches the store page `params.cursor` names and returns at most
/// [`MAX_SCAN_PAGE_BOOKS`] of its titles and removed ids. A larger store page
/// is returned over several calls, each fetching it again. The pass ends on
/// the last slice of the store page whose [`StorePage::next`] is empty, which
/// carries that page's `sync_token` and `complete`.
///
/// # Arguments
///
/// * `params` - The scan call's params.
/// * `fetch` - Fetches the store page a storefront cursor names (empty for
///   the first), given the call's params.
///
/// # Returns
///
/// The reply for this call.
///
/// # Errors
///
/// Returns the fetch's error unchanged; an `invalid_cursor` [`PluginError`]
/// when the cursor was not written by this helper or no longer fits the store
/// page (the store changed mid-pass), so the host restarts the pass; and an
/// `internal` one when a store cursor is too long for a scan cursor.
pub async fn scan_page<F, Fut>(params: &ScanParams, fetch: F) -> Result<ScanPage, PluginError>
where
    F: FnOnce(String, &ScanParams) -> Fut,
    Fut: Future<Output = Result<StorePage, PluginError>>,
{
    let ScanPosition {
        store_cursor,
        offset,
    } = read_cursor(&params.cursor)?;
    let page = fetch(store_cursor.clone(), params).await?;
    if offset > 0 && offset >= page.books.len() && offset >= page.removed.len() {
        return Err(PluginError::invalid_cursor(
            "the store page shrank during the pass",
        ));
    }
    let end = offset.saturating_add(page_width());
    let mut reply = ScanPage {
        books: slice(&page.books, offset),
        removed: slice(&page.removed, offset),
        next_cursor: String::new(),
        sync_token: String::new(),
        complete: false,
        store_pages: page.store_pages.unwrap_or(1),
    };
    if page.books.len() > end || page.removed.len() > end {
        reply.next_cursor = write_cursor(&store_cursor, end)?;
    } else if !page.next.is_empty() {
        reply.next_cursor = write_cursor(&page.next, 0)?;
    } else {
        reply.sync_token = page.sync_token;
        reply.complete = page.complete;
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PluginErrorCode;

    /// A title with only the product id set.
    fn book(i: usize) -> ScanBook {
        ScanBook {
            product_id: format!("p{i}"),
            ..ScanBook::default()
        }
    }

    /// Params for one call at `cursor`.
    fn params(cursor: &str) -> ScanParams {
        ScanParams {
            account_id: "a1".into(),
            cursor: cursor.into(),
            ..ScanParams::default()
        }
    }

    /// Runs a pass over store pages of `sizes` the way the host does; returns
    /// every reply and every store cursor fetched.
    async fn run_pass(sizes: &[usize]) -> (Vec<ScanPage>, Vec<String>) {
        let starts: Vec<usize> = sizes
            .iter()
            .scan(0, |next, size| {
                let start = *next;
                *next += size;
                Some(start)
            })
            .collect();
        let mut replies = Vec::new();
        let mut fetches = Vec::new();
        let mut cursor = String::new();
        for _ in 0..1000 {
            let reply = scan_page(&params(&cursor), |store_cursor, p| {
                assert_eq!(p.account_id, "a1");
                fetches.push(store_cursor.clone());
                let index = if store_cursor.is_empty() {
                    0
                } else {
                    store_cursor.parse::<usize>().unwrap() - 1
                };
                let last = index + 1 == sizes.len();
                let page = StorePage {
                    books: (0..sizes[index]).map(|i| book(starts[index] + i)).collect(),
                    next: if last {
                        String::new()
                    } else {
                        (index + 2).to_string()
                    },
                    sync_token: if last { "t2".into() } else { String::new() },
                    complete: last,
                    ..StorePage::default()
                };
                async move { Ok(page) }
            })
            .await
            .unwrap();
            assert!(reply.books.len() <= page_width());
            let next = reply.next_cursor.clone();
            replies.push(reply);
            if next.is_empty() {
                return (replies, fetches);
            }
            assert_ne!(next, cursor, "a reply never repeats its cursor");
            cursor = next;
        }
        panic!("the pass did not end");
    }

    #[tokio::test]
    async fn large_store_pages_are_sliced_in_order() {
        let width = page_width();
        let sizes = [width * 2 + 7, 4, width];
        let (replies, fetches) = run_pass(&sizes).await;
        let ids: Vec<String> = replies
            .iter()
            .flat_map(|r| r.books.iter().map(|b| b.product_id.clone()))
            .collect();
        let want: Vec<String> = (0..sizes.iter().sum()).map(|i| format!("p{i}")).collect();
        assert_eq!(ids, want);
        assert_eq!(replies.len(), 5);
        for r in &replies[..4] {
            assert!(r.sync_token.is_empty() && !r.complete);
        }
        assert_eq!(replies[4].sync_token, "t2");
        assert!(replies[4].complete);
        assert_eq!(fetches, ["", "", "", "2", "3"]);
    }

    #[tokio::test]
    async fn one_store_page_is_one_reply() {
        let (replies, _) = run_pass(&[3]).await;
        assert_eq!(
            replies,
            [ScanPage {
                books: vec![book(0), book(1), book(2)],
                removed: vec![],
                next_cursor: String::new(),
                sync_token: "t2".into(),
                complete: true,
                store_pages: 1,
            }]
        );
    }

    #[tokio::test]
    async fn removed_ids_are_sliced_with_the_titles() {
        let removed: Vec<String> = (0..=page_width()).map(|i| format!("r{i}")).collect();
        let page = StorePage {
            books: vec![book(0)],
            removed: removed.clone(),
            sync_token: "t3".into(),
            store_pages: Some(2),
            ..StorePage::default()
        };
        let first = scan_page(&params(""), |_, _| {
            let page = page.clone();
            async move { Ok(page) }
        })
        .await
        .unwrap();
        let second = scan_page(&params(&first.next_cursor), |_, _| {
            let page = page.clone();
            async move { Ok(page) }
        })
        .await
        .unwrap();
        let all: Vec<String> = first.removed.into_iter().chain(second.removed).collect();
        assert_eq!(all, removed);
        assert_eq!((first.store_pages, second.store_pages), (2, 2));
        assert_eq!(second.sync_token, "t3");
        assert!(!second.complete && second.next_cursor.is_empty());
    }

    #[tokio::test]
    async fn a_foreign_or_stale_cursor_is_invalid() {
        let shrunk = format!("sp1.{}.", page_width());
        for cursor in [
            "3",
            "sp1.",
            "sp1.x.1",
            "sp1.01.1",
            "sp1.7.1",
            "sp2.0.1",
            "sp1.-500.1",
            "sp1.99999999999999999.1",
            shrunk.as_str(),
        ] {
            let err = scan_page(&params(cursor), |_, _| async {
                Ok(StorePage {
                    books: vec![book(0)],
                    ..StorePage::default()
                })
            })
            .await
            .unwrap_err();
            assert_eq!(err.code, PluginErrorCode::InvalidCursor, "{cursor}");
        }
    }

    #[tokio::test]
    async fn a_store_cursor_too_long_fails() {
        let next = "x".repeat(usize::try_from(MAX_SCAN_CURSOR_BYTES).unwrap());
        let err = scan_page(&params(""), |_, _| async move {
            Ok(StorePage {
                next,
                ..StorePage::default()
            })
        })
        .await
        .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::Internal);
    }

    #[test]
    fn cursors_match_the_other_sdks() {
        assert_eq!(write_cursor("a.b.é", 0).unwrap(), "sp1.0.a.b.é");
        assert_eq!(write_cursor("", 500).unwrap(), "sp1.500.");
        assert_eq!(
            read_cursor("sp1.500.a.b").unwrap(),
            ScanPosition {
                store_cursor: "a.b".into(),
                offset: 500
            }
        );
    }

    #[tokio::test]
    async fn a_fetch_error_is_returned_unchanged() {
        let err = scan_page(&params(""), |_, _| async {
            Err(PluginError::new(PluginErrorCode::Unauthorized, "expired"))
        })
        .await
        .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::Unauthorized);
    }
}
