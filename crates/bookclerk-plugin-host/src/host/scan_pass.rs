//! One account's scan pass: the host side of paged `ContentSource.scan`.
//!
//! The host asks the guest for one page at a time, validates each page
//! ([`bookclerk_plugin_abi::validate_scan_page`]) before writing any of it,
//! checkpoints the next cursor after the page is written, and stores the
//! plugin's sync token when the last page arrives. The IO (guest calls and
//! library writes) sits behind [`PassIo`] so the loop is tested without a
//! guest process.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use bookclerk_library::PassStart;
use bookclerk_plugin_sdk::{PluginError, PluginErrorCode, ScanBook, ScanPage};

/// Most pages one pass may take before the host gives up on it, so a guest
/// that keeps returning new cursors cannot hold a scan job forever.
pub(crate) const MAX_PASS_PAGES: u32 = 10_000;

/// What a pass needs from the guest and the library.
pub(crate) trait PassIo {
    /// Asks the guest for the page at `cursor` of a pass sent `sync_token`.
    async fn page(&mut self, cursor: &str, sync_token: &str) -> Result<ScanPage, PluginError>;
    /// Writes a validated page's titles and marks them listed by this pass;
    /// returns how many rows were written.
    async fn write(&mut self, books: Vec<ScanBook>) -> bookclerk_source::Result<usize>;
    /// Removes the account's holdings of products a page reported gone.
    async fn remove(&mut self, product_ids: &[String]) -> bookclerk_source::Result<()>;
    /// Records that the pass continues at `cursor` (started with `sync_token`).
    async fn checkpoint(&mut self, cursor: &str, sync_token: &str) -> bookclerk_source::Result<()>;
    /// Records the end of the pass. `full` is true when no page was sent a
    /// sync token, so with `complete` the pass listed every title the
    /// account holds and the holdings it did not list are gone.
    async fn finish(
        &mut self,
        sync_token: &str,
        complete: bool,
        full: bool,
    ) -> bookclerk_source::Result<()>;
    /// Forgets the stored token and cursor (the guest called one invalid).
    async fn reset(&mut self) -> bookclerk_source::Result<()>;
    /// Starts the pass over from its first page under a new pass id, so
    /// nothing the abandoned pages listed counts as listed by the restart.
    /// Called before any checkpoint of the restarted pass.
    fn restart(&mut self);
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
    // Cursors this pass has already asked for, hashed: a guest that cycles
    // through several cursors is caught on its first repeat rather than at
    // MAX_PASS_PAGES.
    let mut asked: HashSet<u64> = HashSet::new();
    loop {
        if io.cancelled() {
            return Err(bookclerk_source::SourceError::Other(anyhow::anyhow!(
                "cancelled"
            )));
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
                io.restart();
                cursor.clear();
                token.clear();
                resumed = false;
                continue;
            }
            Err(err) if !restarted && resumed && !cursor.is_empty() => {
                tracing::warn!(account_id, error = %err, "resumed scan page failed; restarting the pass");
                restarted = true;
                io.restart();
                // Forget the stored cursor now, so a restart that fails too
                // does not leave the next job resuming the same page.
                io.checkpoint("", &token).await?;
                cursor.clear();
                resumed = false;
                asked.clear();
                continue;
            }
            Err(err) if !restarted && cursor.is_empty() && !token.is_empty() => {
                // A store may refuse an expired token with a plain error
                // rather than `invalid_cursor`. The stored token stays until
                // the full pass finishes and replaces it.
                tracing::warn!(account_id, error = %err, "delta scan failed; restarting as a full pass");
                restarted = true;
                io.restart();
                token.clear();
                resumed = false;
                asked.clear();
                continue;
            }
            Err(err) => return Err(bookclerk_source::SourceError::api(err.to_string())),
        };
        bookclerk_plugin_abi::validate_scan_page(&page, &cursor, account_id)
            .map_err(|err| bookclerk_source::SourceError::api(err.to_string()))?;
        asked.insert(cursor_hash(&cursor));
        if !page.next_cursor.is_empty() && asked.contains(&cursor_hash(&page.next_cursor)) {
            return Err(bookclerk_source::SourceError::api(format!(
                "scan of account `{account_id}` returned a cursor it already returned in this pass"
            )));
        }
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
        if !page.removed.is_empty() {
            io.remove(&page.removed).await?;
        }
        if page.next_cursor.is_empty() {
            // Every page of this pass was sent `token` (a restart clears it
            // and starts over), so an empty one means a full pass.
            io.finish(&page.sync_token, page.complete, token.is_empty())
                .await?;
            outcome.complete = page.complete;
            return Ok(outcome);
        }
        io.checkpoint(&page.next_cursor, &token).await?;
        cursor = page.next_cursor;
    }
}

/// A pass-local hash of `cursor`, for spotting a repeat.
fn cursor_hash(cursor: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    cursor.hash(&mut hasher);
    hasher.finish()
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
        /// Finish recorded: `(token, complete, full)`.
        finished: Option<(String, bool, bool)>,
        /// Product ids reported removed.
        removed: Vec<String>,
        /// Resets recorded.
        resets: usize,
        /// Restarts recorded.
        restarts: usize,
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
        async fn remove(&mut self, product_ids: &[String]) -> bookclerk_source::Result<()> {
            self.removed.extend_from_slice(product_ids);
            Ok(())
        }
        async fn finish(
            &mut self,
            token: &str,
            complete: bool,
            full: bool,
        ) -> bookclerk_source::Result<()> {
            self.finished = Some((token.into(), complete, full));
            Ok(())
        }
        async fn reset(&mut self) -> bookclerk_source::Result<()> {
            self.resets += 1;
            Ok(())
        }
        fn restart(&mut self) {
            self.restarts += 1;
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
            pass_id: "p".into(),
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
        assert_eq!(io.finished, Some(("t1".into(), true, true)));
        assert_eq!(io.written, ["1", "2", "3"]);
        assert_eq!(io.removed, ["gone"], "removed ids reach the library");
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
        assert_eq!(
            io.finished,
            Some(("t2".into(), false, false)),
            "a delta pass is never full"
        );
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
        assert_eq!(io.restarts, 1, "the restart gets a new pass id");
        assert_eq!(io.calls[1], ("".into(), "".into()));
        assert_eq!(
            io.finished,
            Some(("t9".into(), true, true)),
            "the restart is a full pass"
        );

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
        assert_eq!(io.restarts, 1, "the restart gets a new pass id");
        assert_eq!(
            io.checkpoints[0],
            ("".into(), "t1".into()),
            "the dead cursor is forgotten before the restart"
        );

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

    #[tokio::test]
    async fn a_failed_delta_restarts_once_as_a_full_pass_without_a_reset() {
        let mut io = Script {
            pages: [
                Err(PluginError::internal("HTTP 400: bad state_token")),
                Ok(last(&["1"], "t2", true)),
            ]
            .into(),
            ..Script::default()
        };
        let outcome = run_pass(&mut io, "a", start("", "t1", false))
            .await
            .unwrap();
        assert!(outcome.complete);
        assert_eq!(io.calls[1], ("".into(), "".into()));
        assert_eq!(
            io.resets, 0,
            "the stored token stays until the full pass ends"
        );
        assert_eq!(io.restarts, 1);
        assert_eq!(io.finished, Some(("t2".into(), true, true)));

        let mut io = Script {
            pages: [
                Err(PluginError::internal("bad token")),
                Err(PluginError::internal("still down")),
            ]
            .into(),
            ..Script::default()
        };
        assert!(run_pass(&mut io, "a", start("", "t1", false))
            .await
            .is_err());
        assert_eq!(io.calls.len(), 2);
    }

    #[tokio::test]
    async fn a_cursor_cycle_is_refused_on_its_first_repeat() {
        let mut io = Script {
            pages: [
                Ok(page(&["1"], "a")),
                Ok(page(&["2"], "b")),
                Ok(page(&["3"], "a")),
            ]
            .into(),
            ..Script::default()
        };
        let err = run_pass(&mut io, "acct", start("", "", false))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already returned"), "{err}");
        assert_eq!(io.written, ["1", "2"]);
    }

    #[tokio::test]
    async fn a_cancelled_pass_reports_a_cancellation() {
        let mut io = Script {
            cancel_after: Some(0),
            ..Script::default()
        };
        let err = run_pass(&mut io, "a", start("", "", false))
            .await
            .unwrap_err();
        assert!(
            matches!(err, bookclerk_source::SourceError::Other(_)),
            "{err}"
        );
        assert!(err.to_string().contains("cancelled"), "{err}");
    }
}
