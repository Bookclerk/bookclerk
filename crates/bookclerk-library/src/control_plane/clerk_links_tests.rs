//! Clerk links: a library card is sealed and read back only while the link
//! is active, a sign-in is refused where a cluster link already reads that
//! library, signing out drops the card and the books read through it, and
//! the merged view shows a card's books beside a window link's.

use bookclerk_node_proto::{
    Availability, CatalogMetadata, LookupEdition, LookupFormat, LookupIdentifier, LookupScheme,
    WindowWork,
};

use super::*;
use crate::control_plane::{
    record_cluster_link, record_window_page, set_cluster_link_download, window_read_stamp,
    window_view, ClusterLinkKeys, NewClusterLink,
};
use crate::master_key::master_key_test_lock_async;

/// A card that passes [`bookclerk_node_proto::is_clerk_token`].
fn card(fill: char) -> LibraryCard {
    LibraryCard::new(std::iter::repeat_n(fill, 43).collect()).expect("a card")
}

/// A sign-in to `cluster` as user `user`.
fn sign_in(cluster: &str, user: i64) -> NewClerkLink {
    NewClerkLink {
        library_cluster_id: cluster.into(),
        origin: format!("HTTPS://{cluster}.Example:443/"),
        spki_pin: None,
        library_name: format!("  {cluster} library "),
        remote_user_id: user,
        display_name: Some("Pat".into()),
        clerk_name: "Laptop".into(),
    }
}

/// One work holding one audio edition.
fn work(asin: &str, title: &str) -> WindowWork {
    WindowWork {
        work: format!("w-{asin}"),
        identifiers: vec![LookupIdentifier {
            scheme: LookupScheme::Asin,
            value: asin.into(),
            marketplace: "us".into(),
        }],
        editions: vec![bookclerk_node_proto::WindowEdition {
            edition: LookupEdition {
                store: "audible".into(),
                marketplace: "us".into(),
                product_id: asin.into(),
                availability: Availability::Owned,
                formats: vec![LookupFormat::Audio],
                metadata: CatalogMetadata {
                    title: title.into(),
                    ..CatalogMetadata::default()
                },
            },
            yours: true,
            copy: None,
        }],
    }
}

/// Runs `body` on a fresh store under every database test profile,
/// refusing interactive transactions.
macro_rules! under_every_profile {
    ($body:ident) => {{
        let _lock = master_key_test_lock_async().await;
        for profile in crate::test_support::Profile::ALL {
            let kit = crate::test_support::sqlite::memory(profile).await;
            let dir = tempfile::tempdir().unwrap();
            crate::configure_master_key(dir.path()).expect("master key");
            $body(kit.store()).await;
            assert_eq!(
                kit.refused_begins(),
                0,
                "{profile:?} took an interactive transaction"
            );
        }
    }};
}

#[test]
fn a_card_is_checked_and_never_printed() {
    assert!(LibraryCard::new("short".into()).is_err());
    let held = card('a');
    assert_eq!(held.expose().len(), 43);
    assert_eq!(format!("{held:?}"), "LibraryCard(<redacted>)");
    assert_eq!(
        ClerkLinkState::from_column("weird"),
        ClerkLinkState::SignedOut
    );
}

#[tokio::test]
async fn a_card_round_trips_until_it_is_signed_out() {
    under_every_profile!(card_round_trip);
}

async fn card_round_trip(store: &LibraryStore) {
    let link = record_clerk_link(store, &sign_in("alpha", 7), &card('a'))
        .await
        .unwrap();
    assert_eq!(link.origin, "https://alpha.example");
    assert_eq!(link.library_name, "alpha library");
    assert!(link.is_active());
    assert_eq!(
        load_clerk_card(store, &link).await.unwrap().expose(),
        card('a').expose()
    );
    // Signing in again replaces the card.
    let link = record_clerk_link(store, &sign_in("alpha", 7), &card('b'))
        .await
        .unwrap();
    assert_eq!(
        load_clerk_card(store, &link).await.unwrap().expose(),
        card('b').expose()
    );
    assert_eq!(list_clerk_links(store).await.unwrap().len(), 1);

    // An active link cannot be forgotten; a signed-out one can.
    assert!(matches!(
        forget_clerk_link(store, "alpha").await,
        Err(LibraryError::Conflict(_))
    ));
    let out = sign_out_clerk_link(store, "alpha")
        .await
        .unwrap()
        .expect("the link");
    assert_eq!(out.state, ClerkLinkState::SignedOut);
    assert!(matches!(
        load_clerk_card(store, &out).await,
        Err(LibraryError::Forbidden(_))
    ));
    // The card is gone even if the row were flipped back by hand.
    let mut revived = out.clone();
    revived.state = ClerkLinkState::Active;
    assert!(matches!(
        load_clerk_card(store, &revived).await,
        Err(LibraryError::NotFound(_))
    ));
    // Signing out twice is harmless; an unknown library answers None.
    assert!(sign_out_clerk_link(store, "alpha").await.unwrap().is_some());
    assert!(sign_out_clerk_link(store, "nowhere")
        .await
        .unwrap()
        .is_none());
    assert!(forget_clerk_link(store, "alpha").await.unwrap());
    assert!(!forget_clerk_link(store, "alpha").await.unwrap());
    assert!(list_clerk_links(store).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_sign_in_is_validated() {
    under_every_profile!(validation);
}

async fn validation(store: &LibraryStore) {
    let bad = |edit: fn(&mut NewClerkLink)| {
        let mut link = sign_in("alpha", 7);
        edit(&mut link);
        link
    };
    for link in [
        bad(|l| l.library_cluster_id = "has space".into()),
        bad(|l| l.origin = "ftp://alpha.example".into()),
        bad(|l| l.spki_pin = Some("not-a-pin".into())),
        bad(|l| l.library_name = "tab\there".into()),
        bad(|l| l.remote_user_id = 0),
        bad(|l| l.display_name = Some("x".repeat(129))),
        bad(|l| l.clerk_name = String::new()),
    ] {
        assert!(
            matches!(
                record_clerk_link(store, &link, &card('a')).await,
                Err(LibraryError::Invalid(_))
            ),
            "{link:?} was accepted"
        );
    }
    assert!(list_clerk_links(store).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_card_reads_into_the_merged_view_beside_a_window() {
    under_every_profile!(merged_view);
}

async fn merged_view(store: &LibraryStore) {
    // A window link to beta, and a card for alpha.
    record_cluster_link(
        store,
        &NewClusterLink {
            owner_cluster_id: "beta".into(),
            owner_origin: "https://beta.example".into(),
            remote_node_id: "node-b".into(),
            spki_pin: None,
        },
        &ClusterLinkKeys::generate(),
    )
    .await
    .unwrap();
    assert!(set_cluster_link_download(store, "beta", false)
        .await
        .unwrap());
    // A card for a library this install already joined is refused.
    assert!(matches!(
        record_clerk_link(store, &sign_in("beta", 3), &card('c')).await,
        Err(LibraryError::Conflict(_))
    ));
    record_clerk_link(store, &sign_in("alpha", 7), &card('a'))
        .await
        .unwrap();

    let read = window_read_stamp();
    record_window_page(store, "alpha", &read, &[work("B0SHARED01", "Shared")])
        .await
        .unwrap();
    record_window_page(
        store,
        "beta",
        &read,
        &[
            work("B0SHARED01", "Shared (beta)"),
            work("B0BETA0001", "Beta only"),
        ],
    )
    .await
    .unwrap();
    let view = window_view(store).await.unwrap();
    let titles: Vec<&str> = view.iter().map(|e| e.title()).collect();
    assert_eq!(titles, vec!["Beta only", "Shared"]);
    let origins: Vec<&str> = view[1]
        .editions
        .iter()
        .map(|e| e.owner_origin.as_str())
        .collect();
    assert_eq!(
        origins,
        vec!["https://alpha.example", "https://beta.example"]
    );

    // Signing in as someone else drops the previous person's books.
    record_clerk_link(store, &sign_in("alpha", 8), &card('d'))
        .await
        .unwrap();
    assert_eq!(view_titles(store).await, vec!["Beta only", "Shared (beta)"]);
    record_window_page(store, "alpha", &read, &[work("B0ALPHA001", "Alpha")])
        .await
        .unwrap();
    assert_eq!(
        view_titles(store).await,
        vec!["Alpha", "Beta only", "Shared (beta)"]
    );
    // Signing out drops them too, and leaves the window's rows alone.
    sign_out_clerk_link(store, "alpha").await.unwrap();
    assert_eq!(view_titles(store).await, vec!["Beta only", "Shared (beta)"]);
}

/// The merged view's titles.
async fn view_titles(store: &LibraryStore) -> Vec<String> {
    window_view(store)
        .await
        .unwrap()
        .iter()
        .map(|e| e.title().to_string())
        .collect()
}
