//! One account's scan pass: the host side of paged `ContentSource.scan`.
//!
//! The host asks the guest for one page at a time, validates each page
//! ([`bookclerk_plugin_abi::validate_scan_page`]) before writing any of it,
//! checkpoints the next cursor after the page is written, and stores the
//! plugin's sync token when the last page arrives. The IO (guest calls and
//! library writes) sits behind [`PassIo`] so the loop is tested without a
//! guest process.

use bookclerk_library::PassStart;
use bookclerk_plugin_sdk::{PluginError, PluginErrorCode, ScanBook, ScanPage};

/// Most pages one pass may take before the host gives up on it, so a guest
/// that keeps returning new cursors cannot hold a scan job forever.
pub(crate) const MAX_PASS_PAGES: u32 = 10_000;

/// What a pass needs from the guest and the library.
pub(crate) trait PassIo {
    /// Asks the guest for the page at `cursor` of a pass sent `sync_token`.
    async fn page(&mut self, cursor: &str, sync_token: &str) -> Result<ScanPage, PluginError>;
    /// Writes a validated page's titles; returns how many rows were written.
    async fn write(&mut self, books: Vec<ScanBook>) -> bookclerk_source::Result<usize>;
    /// Records that the pass continues at `cursor` (started with `sync_token`).
    async fn checkpoint(&mut self, cursor: &str, sync_token: &str) -> bookclerk_source::Result<()>;
    /// Records the end of the pass.
    async fn finish(&mut self, sync_token: &str, complete: bool) -> bookclerk_source::Result<()>;
    /// Forgets the stored token and cursor (the guest called one invalid).
    async fn reset(&mut self) -> bookclerk_source::Result<()>;
    /// True when the scan job asked to stop.
    fn cancelled(&self) -> bool;
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PassOutcome {
    /// Pages the host wrote.
    pub pages: u32,
    /// Storefront requests the guest reported.
    pub store_pages: u32,
    /// Rows written.
    pub upserted: usize,
    /// Product ids the guest reported gone from the account.
    pub removed: usize,
    /// True when the pass listed every title the account holds.
    pub complete: bool,
}

/// Runs one pass of `account_id` from `start` to its last page.
///
/// A pass restarts from the beginning once: after the guest answers
/// `invalid_cursor` (the stored token and cursor are forgotten and the pass
/// becomes full), or after the first page of a resumed pass fails (its store
/// cursor may have expired).
///
/// # Errors
///
/// Returns an error when the guest fails, a page breaks the ABI rules, a
/// write fails, the pass passes [`MAX_PASS_PAGES`], or the job is cancelled.
/// Pages written before the error stay written and checkpointed.
pub(crate) async fn run_pass(
    io: &mut impl PassIo,
    account_id: &str,
    start: PassStart,
) -> bookclerk_source::Result<PassOutcome> {
    let mut cursor = start.cursor;
    let mut token = start.sync_token;
    let mut resumed = start.resumed;
    let mut restarted = false;
    let mut outcome = PassOutcome::default();
    loop {
        if io.cancelled() {
            return Err(bookclerk_source::SourceError::api("scan cancelled"));
        }
        if outcome.pages >= MAX_PASS_PAGES {
            return Err(bookclerk_source::SourceError::api(format!(
                "scan of account `{account_id}` passed {MAX_PASS_PAGES} pages"
            )));
        }
        let page = match io.page(&cursor, &token).await {
            Ok(page) => page,
            Err(err) if !restarted && err.code == PluginErrorCode::InvalidCursor => {
                tracing::warn!(account_id, error = %err, "scan cursor refused; restarting as a full pass");
                restarted = true;
                io.reset().await?;
                cursor.clear();
                token.clear();
                resumed = false;
                continue;
            }
            Err(err) if !restarted && resumed && !cursor.is_empty() => {
                tracing::warn!(account_id, error = %err, "resumed scan page failed; restarting the pass");
                restarted = true;
                cursor.clear();
                resumed = false;
                continue;
            }
            Err(err) => return Err(bookclerk_source::SourceError::api(err.to_string())),
        };
        bookclerk_plugin_abi::validate_scan_page(&page, &cursor, account_id)
            .map_err(|err| bookclerk_source::SourceError::api(err.to_string()))?;
        resumed = false;
        outcome.pages += 1;
        outcome.store_pages = outcome.store_pages.saturating_add(page.store_pages);
        outcome.removed += page.removed.len();
        let mut books = page.books;
        for book in &mut books {
            if book.account_id.is_empty() {
                book.account_id = account_id.to_string();
            }
        }
        outcome.upserted += io.write(books).await?;
        if page.next_cursor.is_empty() {
            io.finish(&page.sync_token, page.complete).await?;
            outcome.complete = page.complete;
            return Ok(outcome);
        }
        io.checkpoint(&page.next_cursor, &token).await?;
        cursor = page.next_cursor;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// Scripted guest and recording library.
    #[derive(Default)]
    struct Script {
        /// Answers in order.
        pages: VecDeque<Result<ScanPage, PluginError>>,
        /// `(cursor, token)` of each guest call.
        calls: Vec<(String, String)>,
        /// Product ids written.
        written: Vec<String>,
        /// Accounts of the rows written.
        accounts: Vec<String>,
        /// Checkpoints recorded.
        checkpoints: Vec<(String, String)>,
        /// Finish recorded.
        finished: Option<(String, bool)>,
        /// Resets recorded.
        resets: usize,
        /// Cancel after this many guest calls.
        cancel_after: Option<usize>,
    }

    impl PassIo for Script {
        async fn page(&mut self, cursor: &str, token: &str) -> Result<ScanPage, PluginError> {
            self.calls.push((cursor.into(), token.into()));
            self.pages
                .pop_front()
                .unwrap_or_else(|| Err(PluginError::internal("script ran out")))
        }
        async fn write(&mut self, books: Vec<ScanBook>) -> bookclerk_source::Result<usize> {
            let n = books.len();
            for book in books {
                self.written.push(book.product_id);
                self.accounts.push(book.account_id);
            }
            Ok(n)
        }
        async fn checkpoint(&mut self, cursor: &str, token: &str) -> bookclerk_source::Result<()> {
            self.checkpoints.push((cursor.into(), token.into()));
            Ok(())
        }
        async fn finish(&mut self, token: &str, complete: bool) -> bookclerk_source::Result<()> {
            self.finished = Some((token.into(), complete));
            Ok(())
        }
        async fn reset(&mut self) -> bookclerk_source::Result<()> {
            self.resets += 1;
            Ok(())
        }
        fn cancelled(&self) -> bool {
            self.cancel_after.is_some_and(|n| self.calls.len() >= n)
        }
    }

    fn page(products: &[&str], next: &str) -> ScanPage {
        ScanPage {
            books: products
                .iter()
                .map(|p| ScanBook {
                    product_id: (*p).into(),
                    title: "T".into(),
                    ..ScanBook::default()
                })
                .collect(),
            next_cursor: next.into(),
            store_pages: 1,
            ..ScanPage::default()
        }
    }

    fn last(products: &[&str], token: &str, complete: bool) -> ScanPage {
        ScanPage {
            sync_token: token.into(),
            complete,
            removed: vec!["gone".into()],
            ..page(products, "")
        }
    }

    fn start(cursor: &str, token: &str, resumed: bool) -> PassStart {
        PassStart {
            cursor: cursor.into(),
            sync_token: token.into(),
            resumed,
        }
    }

    #[tokio::test]
    async fn a_pass_writes_every_page_checkpoints_and_stores_the_token() {
        let mut io = Script {
            pages: [Ok(page(&["1", "2"], "c2")), Ok(last(&["3"], "t1", true))].into(),
            ..Script::default()
        };
        let outcome = run_pass(&mut io, "a", start("", "", false)).await.unwrap();
        assert_eq!(
            outcome,
            PassOutcome {
                pages: 2,
                store_pages: 2,
                upserted: 3,
                removed: 1,
                complete: true,
            }
        );
        assert_eq!(io.calls, [("".into(), "".into()), ("c2".into(), "".into())]);
        assert_eq!(io.checkpoints, [("c2".into(), "".into())]);
        assert_eq!(io.finished, Some(("t1".into(), true)));
        assert_eq!(io.written, ["1", "2", "3"]);
        assert!(
            io.accounts.iter().all(|a| a == "a"),
            "empty accounts are filled"
        );
    }

    #[tokio::test]
    async fn a_delta_pass_sends_its_token_on_every_page() {
        let mut io = Script {
            pages: [Ok(page(&["1"], "c2")), Ok(last(&[], "t2", false))].into(),
            ..Script::default()
        };
        let outcome = run_pass(&mut io, "a", start("", "t1", false))
            .await
            .unwrap();
        assert!(!outcome.complete);
        assert!(io.calls.iter().all(|(_, token)| token == "t1"));
        assert_eq!(io.checkpoints, [("c2".into(), "t1".into())]);
        assert_eq!(io.finished, Some(("t2".into(), false)));
    }

    #[tokio::test]
    async fn invalid_cursor_restarts_once_as_a_full_pass() {
        let mut io = Script {
            pages: [
                Err(PluginError::invalid_cursor("stale")),
                Ok(last(&["1"], "t9", true)),
            ]
            .into(),
            ..Script::default()
        };
        run_pass(&mut io, "a", start("c5", "t1", true))
            .await
            .unwrap();
        assert_eq!(io.resets, 1);
        assert_eq!(io.calls[1], ("".into(), "".into()));

        let mut io = Script {
            pages: [
                Err(PluginError::invalid_cursor("stale")),
                Err(PluginError::invalid_cursor("still")),
            ]
            .into(),
            ..Script::default()
        };
        assert!(run_pass(&mut io, "a", start("", "t1", false))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_failed_resume_restarts_from_the_beginning_with_its_token() {
        let mut io = Script {
            pages: [
                Err(PluginError::unavailable("expired")),
                Ok(last(&["1"], "t2", false)),
            ]
            .into(),
            ..Script::default()
        };
        run_pass(&mut io, "a", start("c5", "t1", true))
            .await
            .unwrap();
        assert_eq!(io.calls[1], ("".into(), "t1".into()));
        assert_eq!(io.resets, 0);

        // A fresh pass does not retry.
        let mut io = Script {
            pages: [Err(PluginError::unavailable("down"))].into(),
            ..Script::default()
        };
        assert!(run_pass(&mut io, "a", start("", "", false)).await.is_err());
        // Nor does a resumed pass after its first page landed.
        let mut io = Script {
            pages: [
                Ok(page(&["1"], "c6")),
                Err(PluginError::unavailable("down")),
            ]
            .into(),
            ..Script::default()
        };
        assert!(run_pass(&mut io, "a", start("c5", "t1", true))
            .await
            .is_err());
        assert_eq!(io.calls.len(), 2);
    }

    #[tokio::test]
    async fn a_bad_page_is_refused_before_anything_is_written() {
        let mut foreign = page(&["1"], "");
        foreign.books[0].account_id = "other".into();
        let mut io = Script {
            pages: [Ok(foreign)].into(),
            ..Script::default()
        };
        assert!(run_pass(&mut io, "a", start("", "", false)).await.is_err());
        assert!(io.written.is_empty());
        assert!(io.finished.is_none());

        let mut io = Script {
            pages: [Ok(page(&["1"], "c1")), Ok(page(&["2"], "c1"))].into(),
            ..Script::default()
        };
        assert!(run_pass(&mut io, "a", start("", "", false)).await.is_err());
        assert_eq!(io.written, ["1"], "the repeating page is not written");
    }

    #[tokio::test]
    async fn cancel_stops_between_pages_after_the_checkpoint() {
        let mut io = Script {
            pages: [Ok(page(&["1"], "c2")), Ok(last(&["2"], "t", true))].into(),
            cancel_after: Some(1),
            ..Script::default()
        };
        assert!(run_pass(&mut io, "a", start("", "", false)).await.is_err());
        assert_eq!(io.checkpoints, [("c2".into(), "".into())]);
        assert!(io.finished.is_none());
    }

    #[tokio::test]
    async fn a_pass_that_never_ends_is_cut_off() {
        let pages = (0..=MAX_PASS_PAGES)
            .map(|i| Ok(page(&[], &format!("c{}", i + 1))))
            .collect();
        let mut io = Script {
            pages,
            ..Script::default()
        };
        let err = run_pass(&mut io, "a", start("", "", false))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("pages"));
        assert_eq!(io.calls.len(), MAX_PASS_PAGES as usize);
    }
}
