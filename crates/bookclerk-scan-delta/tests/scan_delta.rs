//! Scan deltas against the binding a host grants, under every database
//! profile `bookclerk-db-testkit` knows: the shipped adapters, D1, and the
//! smallest advertisement a host accepts (one row per inline read, no
//! streaming).

// Test code: unwrap/expect are fine here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bookclerk_db_testkit::{BindingDatabase, Profile};
use bookclerk_plugin_sdk::{
    DatabaseBinding, DbCapabilities, DbValue, PluginErrorCode, ScanBook, ScanParams,
};
use bookclerk_scan_delta::{migrations, DeltaPage, ScanDelta};

/// A fresh scan-state database under `profile`, and its advertisement.
async fn served(profile: Profile) -> (DatabaseBinding, DbCapabilities) {
    let unit = BindingDatabase::open(profile, migrations()).await.unwrap();
    let caps = unit.capabilities().clone();
    (DatabaseBinding::from_session(Arc::new(unit)), caps)
}

fn book(id: &str, title: &str) -> ScanBook {
    ScanBook {
        product_id: id.into(),
        title: title.into(),
        ..ScanBook::default()
    }
}

fn params(account: &str, cursor: &str, token: &str) -> ScanParams {
    ScanParams {
        account_id: account.into(),
        cursor: cursor.into(),
        sync_token: token.into(),
        ..ScanParams::default()
    }
}

/// Runs one pass of account `a1` over `pages` and returns every page's
/// answer.
async fn pass(db: &DatabaseBinding, token: &str, pages: &[Vec<ScanBook>]) -> Vec<DeltaPage> {
    pass_for(db, "a1", token, pages).await
}

/// Runs one pass of `account` over `pages` and returns every page's answer.
async fn pass_for(
    db: &DatabaseBinding,
    account: &str,
    token: &str,
    pages: &[Vec<ScanBook>],
) -> Vec<DeltaPage> {
    let mut out = Vec::new();
    for (i, books) in pages.iter().enumerate() {
        let cursor = if i == 0 {
            String::new()
        } else {
            format!("sp1.0.{i}")
        };
        let delta = ScanDelta::open(Some(db), &params(account, &cursor, token))
            .await
            .unwrap();
        let store_cursor = if i == 0 { String::new() } else { i.to_string() };
        out.push(
            delta
                .page(&store_cursor, books.clone(), i + 1 == pages.len())
                .await
                .unwrap(),
        );
    }
    out
}

fn ids(pages: &[DeltaPage]) -> Vec<String> {
    pages
        .iter()
        .flat_map(|p| p.books.iter().map(|b| b.product_id.clone()))
        .collect()
}

async fn count(db: &DatabaseBinding, table: &str) -> i64 {
    let row = db
        .prepare(format!("SELECT count(*) AS n FROM {table}"))
        .first(None)
        .await
        .unwrap()
        .unwrap();
    match row.into_iter().find(|(name, _)| name == "n") {
        Some((_, DbValue::Int64(n))) => n,
        other => panic!("count came back as {other:?}"),
    }
}

fn is_invalid_cursor(err: &bookclerk_plugin_sdk::PluginError) -> bool {
    err.code == PluginErrorCode::InvalidCursor
}

#[tokio::test]
async fn a_full_pass_sends_everything_and_returns_a_token() {
    for profile in Profile::ALL {
        let (db, _) = served(profile).await;
        let pages = pass(
            &db,
            "",
            &[vec![book("1", "A"), book("2", "B")], vec![book("3", "C")]],
        )
        .await;
        assert_eq!(ids(&pages), ["1", "2", "3"], "{profile:?}");
        let last = pages.last().unwrap();
        assert!(last.complete);
        assert!(last.removed.is_empty());
        assert!(last.sync_token.starts_with("hd1."));
        assert!(pages[0].sync_token.is_empty());
    }
}

#[tokio::test]
async fn a_delta_pass_sends_only_changes_and_lists_removals() {
    for profile in Profile::ALL {
        let (db, _) = served(profile).await;
        let first = pass(
            &db,
            "",
            &[vec![book("1", "A"), book("2", "B")], vec![book("3", "C")]],
        )
        .await;
        let token = first.last().unwrap().sync_token.clone();
        // "1" is unchanged but moved pages, "2" changed, "3" is gone, "4" is new.
        let second = pass(
            &db,
            &token,
            &[vec![book("4", "D"), book("2", "B2")], vec![book("1", "A")]],
        )
        .await;
        assert_eq!(ids(&second), ["4", "2"], "{profile:?}");
        let last = second.last().unwrap();
        assert!(!last.complete);
        assert_eq!(last.removed, ["3"]);
        assert_ne!(last.sync_token, token);
        // Nothing changed since: an empty delta that still moves the token on.
        let third = pass(
            &db,
            &last.sync_token,
            &[vec![book("4", "D"), book("2", "B2"), book("1", "A")]],
        )
        .await;
        assert!(ids(&third).is_empty());
        assert!(third[0].removed.is_empty());
        assert!(third[0].sync_token.starts_with("hd1."));
    }
}

#[tokio::test]
async fn a_lost_last_page_can_be_replayed_with_the_older_token() {
    for profile in Profile::ALL {
        let (db, _) = served(profile).await;
        let first = pass(&db, "", &[vec![book("1", "A")]]).await;
        let token = first[0].sync_token.clone();
        let lost = pass(&db, &token, &[vec![book("1", "A2")]]).await;
        // The host never stored `lost`'s token and sends the older one again.
        let again = pass(&db, &token, &[vec![book("1", "A2")]]).await;
        assert_eq!(ids(&again), ["1"], "{profile:?}");
        assert_eq!(again[0].sync_token, lost[0].sync_token);
    }
}

#[tokio::test]
async fn a_repeated_slice_gives_the_same_answer() {
    for profile in Profile::ALL {
        let (db, _) = served(profile).await;
        let first = pass(&db, "", &[vec![book("1", "A"), book("2", "B")]]).await;
        let token = first[0].sync_token.clone();
        let delta = ScanDelta::open(Some(&db), &params("a1", "", &token))
            .await
            .unwrap();
        let once = delta.page("", vec![book("1", "A1")], true).await.unwrap();
        let resumed = ScanDelta::open(Some(&db), &params("a1", "sp1.500.", &token))
            .await
            .unwrap();
        let twice = resumed.page("", vec![book("1", "A1")], true).await.unwrap();
        assert_eq!(once, twice, "{profile:?}");
        assert_eq!(once.removed, ["2"]);
    }
}

#[tokio::test]
async fn foreign_or_forgotten_tokens_restart_the_pass() {
    for profile in Profile::ALL {
        let (db, _) = served(profile).await;
        for token in ["page:2", "hd1.xyz", "hd1.00000000000000000000000000000000"] {
            let err = ScanDelta::open(Some(&db), &params("a1", "", token))
                .await
                .expect_err(token);
            assert!(is_invalid_cursor(&err), "{profile:?} {token}");
        }
        let finished = pass(&db, "", &[vec![book("1", "A")]]).await;
        let token = finished[0].sync_token.clone();
        let err = ScanDelta::open(None, &params("a1", "", &token))
            .await
            .expect_err("a token without the binding");
        assert!(is_invalid_cursor(&err));
    }
}

#[tokio::test]
async fn a_pass_cleared_while_it_ran_restarts_instead_of_removing() {
    for profile in Profile::ALL {
        let (db, _) = served(profile).await;
        let first = pass(&db, "", &[vec![book("1", "A")]]).await;
        let token = first[0].sync_token.clone();
        // A delta pass is under way when a full pass of the account starts.
        let delta = ScanDelta::open(Some(&db), &params("a1", "", &token))
            .await
            .unwrap();
        delta.page("", vec![book("1", "A")], false).await.unwrap();
        pass(&db, "", &[vec![book("1", "A")]]).await;
        // The delta pass's own last page finds its row gone, and reports no
        // removals from the titles the full pass deleted ("1" is still held).
        let err = delta
            .page("1", vec![], true)
            .await
            .expect_err("cleared pass");
        assert!(is_invalid_cursor(&err), "{profile:?}");
    }
}

#[tokio::test]
async fn a_full_pass_clears_the_account_but_not_other_accounts() {
    for profile in Profile::ALL {
        let (db, _) = served(profile).await;
        let first = pass(&db, "", &[vec![book("1", "A")], vec![book("2", "B")]]).await;
        let token = first.last().unwrap().sync_token.clone();
        let delta = pass(&db, &token, &[vec![book("1", "A")]]).await;
        let delta_token = delta[0].sync_token.clone();
        pass_for(&db, "a2", "", &[vec![book("9", "Z")]]).await;
        // A later full pass of a1 keeps only its own rows (and a2's).
        let full = pass(&db, "", &[vec![book("1", "A")]]).await;
        assert!(full[0].complete);
        assert_eq!(full[0].sync_token, token);
        assert_eq!(count(&db, "scan_delta_titles").await, 2, "{profile:?}");
        assert_eq!(count(&db, "scan_delta_passes").await, 2, "{profile:?}");
        let err = ScanDelta::open(Some(&db), &params("a1", "", &delta_token))
            .await
            .expect_err("cleared pass");
        assert!(is_invalid_cursor(&err));
    }
}

#[tokio::test]
async fn passes_untouched_past_retention_are_reclaimed() {
    for profile in Profile::ALL {
        let (db, _) = served(profile).await;
        pass_for(&db, "a2", "", &[vec![book("9", "Z")]]).await;
        db.prepare("UPDATE scan_delta_passes SET touched_at = 0")
            .run(None)
            .await
            .unwrap();
        // Any account's next pass reclaims a2's abandoned rows.
        pass(&db, "", &[vec![book("1", "A")]]).await;
        assert_eq!(count(&db, "scan_delta_titles").await, 1, "{profile:?}");
        assert_eq!(count(&db, "scan_delta_passes").await, 1, "{profile:?}");
    }
}

#[tokio::test]
async fn removals_past_the_inline_result_cap_are_all_listed() {
    for profile in Profile::ALL {
        let (db, caps) = served(profile).await;
        // Enough ids that one inline read could not hold them twice over.
        let count = (2 * usize::try_from(caps.max_result_bytes).unwrap() / 24 + 1).min(12_000);
        let books: Vec<ScanBook> = (0..count)
            .map(|i| book(&format!("product-{i:08}"), "title"))
            .collect();
        let pages: Vec<Vec<ScanBook>> = books.chunks(500).map(<[ScanBook]>::to_vec).collect();
        let first = pass(&db, "", &pages).await;
        let token = first.last().unwrap().sync_token.clone();
        // Every title but the first left; the first changed.
        let second = pass(&db, &token, &[vec![book("product-00000000", "new")]]).await;
        assert_eq!(ids(&second), ["product-00000000"], "{profile:?}");
        let removed = &second[0].removed;
        assert_eq!(removed.len(), count - 1, "{profile:?}");
        assert_eq!(removed[0], "product-00000001");
        assert!(removed.windows(2).all(|w| w[0] < w[1]));
    }
}

#[tokio::test]
async fn without_a_binding_every_pass_is_a_plain_full_pass() {
    let delta = ScanDelta::open(None, &params("a1", "", "")).await.unwrap();
    let page = delta.page("", vec![book("1", "A")], true).await.unwrap();
    assert_eq!(ids(std::slice::from_ref(&page)), ["1"]);
    assert!(page.complete && page.sync_token.is_empty());
}
