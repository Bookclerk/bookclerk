//! The in-process edition status writers guard their own update (#1055).

use super::{write_guarded_acquisition, write_guarded_pdf};
use crate::entities::holdings;
use crate::models::AcquireStatus;
use crate::store::now_str;
use crate::test_support::{sqlite, Profile};
use crate::{LibraryStore, NewBook};
use sea_orm::{ColumnTrait as _, EntityTrait as _, QueryFilter as _};

/// Two accounts holding one edition; answers the edition and each
/// account's holding id.
async fn two_holders(store: &LibraryStore) -> (String, String, String) {
    let mut holdings = Vec::new();
    for account in ["a-acct", "b-acct"] {
        store
            .upsert_account("audible", account, "us", None, true, "audible")
            .await
            .unwrap();
        holdings.push(
            store
                .upsert_book(&NewBook::minimal("B0SHARED", account, "us", "Shared"))
                .await
                .unwrap(),
        );
    }
    assert_eq!(holdings[0].edition_id, holdings[1].edition_id);
    (
        holdings[0].edition_id.clone(),
        holdings[0].holding_id.clone(),
        holdings[1].holding_id.clone(),
    )
}

/// Deletes holding `holding_id`, as a revoke does.
async fn revoke(store: &LibraryStore, holding_id: &str) {
    holdings::Entity::delete_many()
        .filter(holdings::Column::Id.eq(holding_id))
        .exec(store.db())
        .await
        .unwrap();
}

/// The issue's sequence: A reads, A's holding is revoked, B acquires, A
/// writes. A's write changes neither B's audio nor B's PDF.
#[tokio::test]
async fn a_revoked_holdings_late_write_keeps_another_holdings_acquired_edition() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = sqlite::memory(profile).await.into_store();
        let (edition, a, b) = two_holders(&store).await;
        // A has read its holding; then it is revoked and B acquires.
        revoke(&store, &a).await;
        store
            .set_holding_acquire_status(&b, AcquireStatus::Acquired, Some("B/b.m4b"), None)
            .await
            .unwrap();
        store
            .set_holding_pdf_status(&b, AcquireStatus::Acquired, Some("B/b.pdf"))
            .await
            .unwrap();
        // A's stale writes land now.
        let now = now_str();
        write_guarded_acquisition(
            store.db(),
            &edition,
            &a,
            AcquireStatus::Error,
            None,
            Some("stale"),
            &now,
        )
        .await
        .unwrap();
        write_guarded_pdf(
            store.db(),
            &edition,
            &a,
            AcquireStatus::NotAcquired,
            None,
            &now,
        )
        .await
        .unwrap();
        let row = store.edition_acquisition(&edition).await.unwrap().unwrap();
        assert_eq!(row.status, AcquireStatus::Acquired, "{name}");
        assert_eq!(row.storage_key.as_deref(), Some("B/b.m4b"), "{name}");
        assert_eq!(row.holding_id, b, "{name}");
        assert_eq!(row.pdf_status, AcquireStatus::Acquired, "{name}");
        assert_eq!(row.pdf_storage_key.as_deref(), Some("B/b.pdf"), "{name}");
        assert_eq!(row.pdf_holding_id.as_deref(), Some(b.as_str()), "{name}");
    }
}

/// A holding that still holds the edition takes it over once the holding
/// that acquired it is gone, and not before.
#[tokio::test]
async fn a_live_holding_takes_over_only_from_a_holding_that_is_gone() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = sqlite::memory(profile).await.into_store();
        let (edition, a, b) = two_holders(&store).await;
        store
            .set_holding_acquire_status(&b, AcquireStatus::Acquired, Some("B/b.m4b"), None)
            .await
            .unwrap();
        store
            .set_holding_pdf_status(&b, AcquireStatus::Acquired, Some("B/b.pdf"))
            .await
            .unwrap();
        let now = now_str();
        write_guarded_acquisition(
            store.db(),
            &edition,
            &a,
            AcquireStatus::Acquired,
            Some("A/a.m4b"),
            None,
            &now,
        )
        .await
        .unwrap();
        write_guarded_pdf(
            store.db(),
            &edition,
            &a,
            AcquireStatus::Acquired,
            Some("A/a.pdf"),
            &now,
        )
        .await
        .unwrap();
        let row = store.edition_acquisition(&edition).await.unwrap().unwrap();
        assert_eq!(row.holding_id, b, "B still owns the audio ({name})");
        assert_eq!(row.pdf_holding_id.as_deref(), Some(b.as_str()), "{name}");

        revoke(&store, &b).await;
        write_guarded_acquisition(
            store.db(),
            &edition,
            &a,
            AcquireStatus::Acquired,
            Some("A/a.m4b"),
            None,
            &now,
        )
        .await
        .unwrap();
        write_guarded_pdf(
            store.db(),
            &edition,
            &a,
            AcquireStatus::Acquired,
            Some("A/a.pdf"),
            &now,
        )
        .await
        .unwrap();
        let row = store.edition_acquisition(&edition).await.unwrap().unwrap();
        assert_eq!(row.holding_id, a, "{name}");
        assert_eq!(row.storage_key.as_deref(), Some("A/a.m4b"), "{name}");
        assert_eq!(row.pdf_holding_id.as_deref(), Some(a.as_str()), "{name}");
        assert_eq!(row.pdf_storage_key.as_deref(), Some("A/a.pdf"), "{name}");
    }
}
