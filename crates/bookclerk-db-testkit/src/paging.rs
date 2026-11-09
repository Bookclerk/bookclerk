//! Checks that a paged listing returns what the full listing returns.
//!
//! Under [`crate::Profile::Minimum`] a result holds one row, so every
//! listing method pages. These helpers walk a cursor-paged method to the end
//! and compare the walk with the unpaged result.

use std::fmt::Debug;
use std::future::Future;

/// Walks `next_page` from the first page until it reports no further
/// cursor, returning every item in page order.
///
/// `next_page` takes the cursor to start after (`None` for the first page)
/// and the page size, and returns the page's items and the cursor for the
/// page after it (`None` when this was the last page).
///
/// A page may be empty and still carry a cursor: a listing that skips rows
/// it cannot read (unreadable job payloads, say) answers that way for a
/// page of skipped rows, and the walk goes on to the next page.
///
/// # Panics
///
/// Panics when the walk exceeds `max_pages`, so a cursor that never
/// advances fails the test instead of hanging it. Size `max_pages` from the
/// rows the listing reads, skipped ones included.
pub async fn collect_pages<T, C, F, Fut>(
    page_size: u64,
    max_pages: usize,
    mut next_page: F,
) -> Vec<T>
where
    F: FnMut(Option<C>, u64) -> Fut,
    Fut: Future<Output = (Vec<T>, Option<C>)>,
{
    let mut out = Vec::new();
    let mut cursor = None;
    for _ in 0..max_pages {
        let (items, next) = next_page(cursor, page_size).await;
        out.extend(items);
        match next {
            Some(c) => cursor = Some(c),
            None => return out,
        }
    }
    panic!("the listing did not end within {max_pages} pages of {page_size}");
}

/// Asserts that walking `next_page` at `page_size` yields exactly `full`.
///
/// The walk is bounded at one page per item of `full` plus two, which
/// covers a listing that caps its pages below the requested size (one row
/// per page under a one-row result cap). A listing that skips rows it
/// cannot read may need more pages than it returns items; walk it with
/// [`assert_pages_match_full_within`] and a bound sized from the rows it
/// reads.
///
/// # Panics
///
/// Panics when the paged walk differs from `full`, or on the conditions of
/// [`collect_pages`].
pub async fn assert_pages_match_full<T, C, F, Fut>(full: &[T], page_size: u64, next_page: F)
where
    T: PartialEq + Debug,
    F: FnMut(Option<C>, u64) -> Fut,
    Fut: Future<Output = (Vec<T>, Option<C>)>,
{
    assert_pages_match_full_within(full, page_size, full.len() + 2, next_page).await;
}

/// [`assert_pages_match_full`] with the walk bounded at `max_pages`.
///
/// # Panics
///
/// Panics when the paged walk differs from `full`, or on the conditions of
/// [`collect_pages`].
pub async fn assert_pages_match_full_within<T, C, F, Fut>(
    full: &[T],
    page_size: u64,
    max_pages: usize,
    next_page: F,
) where
    T: PartialEq + Debug,
    F: FnMut(Option<C>, u64) -> Fut,
    Fut: Future<Output = (Vec<T>, Option<C>)>,
{
    let paged = collect_pages(page_size, max_pages, next_page).await;
    assert_eq!(
        paged, full,
        "paged listing at {page_size} differs from the full listing"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pages_concatenate_in_order() {
        let items: Vec<u32> = (0..10).collect();
        let full = items.clone();
        assert_pages_match_full(&full, 3, |after: Option<u32>, size| {
            let items = items.clone();
            async move {
                let start = after.map_or(0, |a| usize::try_from(a).unwrap() + 1);
                let page: Vec<u32> = items
                    .iter()
                    .copied()
                    .skip(start)
                    .take(usize::try_from(size).unwrap())
                    .collect();
                let next = page
                    .last()
                    .copied()
                    .filter(|_| start + page.len() < items.len());
                (page, next)
            }
        })
        .await;
    }

    #[tokio::test]
    async fn a_listing_that_caps_its_pages_below_the_requested_size_still_ends() {
        let items: Vec<u32> = (0..10).collect();
        let full = items.clone();
        // One item per page whatever size is asked for, as a listing under a
        // one-row result cap answers.
        assert_pages_match_full(&full, 10, |after: Option<u32>, _| {
            let items = items.clone();
            async move {
                let start = after.map_or(0, |a| usize::try_from(a).unwrap() + 1);
                let page: Vec<u32> = items.iter().copied().skip(start).take(1).collect();
                let next = page.last().copied().filter(|_| start + 1 < items.len());
                (page, next)
            }
        })
        .await;
    }

    #[tokio::test]
    async fn an_empty_page_with_a_cursor_is_walked_past() {
        // Rows 2 and 3 fill one page and are unreadable: the listing skips them
        // and answers an empty page that still points at the rows after them.
        let items: Vec<u32> = (0..6).collect();
        let full: Vec<u32> = items
            .iter()
            .copied()
            .filter(|i| ![2, 3].contains(i))
            .collect();
        assert_pages_match_full_within(&full, 2, items.len(), |after: Option<u32>, size| {
            let items = items.clone();
            async move {
                let start = after.map_or(0, |a| usize::try_from(a).unwrap() + 1);
                let read: Vec<u32> = items
                    .iter()
                    .copied()
                    .skip(start)
                    .take(usize::try_from(size).unwrap())
                    .collect();
                let next = read
                    .last()
                    .copied()
                    .filter(|_| start + read.len() < items.len());
                let page = read.into_iter().filter(|i| ![2, 3].contains(i)).collect();
                (page, next)
            }
        })
        .await;
    }

    #[tokio::test]
    #[should_panic(expected = "did not end")]
    async fn a_cursor_that_never_advances_fails() {
        collect_pages(2, 3, |_: Option<u32>, _| async {
            (vec![1u32, 2], Some(0u32))
        })
        .await;
    }
}
