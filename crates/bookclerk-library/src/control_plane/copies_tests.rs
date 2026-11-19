//! A trusted branch is planned and granted the copy this library holds of
//! an edition; everyone else, and every edition without a copy, is not;
//! and a grant stops opening downloads the moment any of its conditions
//! ends.

use bookclerk_node_proto::{copy_granted_problem, CopyGrantedFrame, CopyRole, UploadProbe};
use sea_orm::EntityTrait;

use super::*;
use crate::control_plane::window_books::tests::{add_branch, hold, raw, BRANCH};
use crate::models::{UserRole, UserStatus};
use crate::test_support::{sqlite::memory, Profile};

/// The ASIN of the edition the tests copy.
const ASIN: &str = "B0COPY0001";

/// The primary audio key of that edition.
const PRIMARY: &str = "Ann Author/Title/Title.m4b";

/// The listing a branch names that edition by.
fn listing(product_id: &str) -> CopyListing<'_> {
    CopyListing {
        store: "audible",
        marketplace: "us",
        product_id,
    }
}

/// A library where Riley's account holds [`ASIN`], acquired with a probed
/// primary audio file, a chapter, a cover, a PDF outside the title's
/// folder, and (on a second destination) a cover alone; answers Riley's
/// user id.
async fn library_with_a_copy(store: &LibraryStore) -> i64 {
    let riley = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap()
        .id;
    hold(store, riley, "riley-acct", &[ASIN.to_string()]).await;
    store
        .set_acquire_status(
            ASIN,
            "riley-acct",
            AcquireStatus::Acquired,
            Some(PRIMARY),
            None,
        )
        .await
        .unwrap();
    let record = store.get_book(ASIN, "riley-acct").await.unwrap().unwrap();
    let owner = store
        .file_owner_of_book(&record.uuid)
        .await
        .unwrap()
        .unwrap();
    for (destination, key, role) in [
        ("local", PRIMARY, FileRole::Audio),
        (
            "local",
            "Ann Author/Title/Chapters/01.mp3",
            FileRole::Chapter,
        ),
        ("local", "Ann Author/Title/cover.jpg", FileRole::Cover),
        ("local", "Ann Author/Elsewhere/notes.pdf", FileRole::Pdf),
        ("s3", "Ann Author/Title/cover.jpg", FileRole::Cover),
    ] {
        assert!(store
            .record_edition_file(destination, key, &owner, role)
            .await
            .unwrap());
    }
    assert!(store
        .set_edition_file_probe(
            "local",
            PRIMARY,
            &owner,
            &crate::FileProbe {
                content_type: "audio/mp4".into(),
                bitrate_kbps: Some(125),
                lossless: false,
                source_store: Some("audible".into()),
            },
        )
        .await
        .unwrap());
    riley
}

/// The plan, or a panic naming what came instead.
async fn planned(store: &LibraryStore, node: &str) -> CopyPlan {
    match plan_copy(store, CopyHolder::Node(node), listing(ASIN))
        .await
        .unwrap()
    {
        CopyPlanned::Planned(plan) => plan,
        other => panic!("not planned: {other:?}"),
    }
}

/// A trusted branch is planned the edition's files on the destination
/// that holds its primary audio: the audio first, the rest in key order,
/// named below the title's folder, with what the probe found. Under every
/// database profile.
#[tokio::test]
async fn a_trusted_branch_is_planned_the_librarys_copy_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let riley = library_with_a_copy(&store).await;
        add_branch(&store, BRANCH, riley).await;

        let plan = planned(&store, BRANCH).await;
        assert_eq!(plan.person, riley, "{name}");
        assert_eq!(plan.copy.content_type, "audio/mp4", "{name}");
        assert_eq!(plan.copy.bitrate_kbps, Some(125), "{name}");
        // The second destination holds no primary audio: not offered.
        assert_eq!(
            plan.destinations.keys().collect::<Vec<_>>(),
            ["local"],
            "{name}"
        );
        let files = &plan.destinations["local"];
        let listed: Vec<(&str, &str, CopyRole)> = files
            .iter()
            .map(|file| (file.key.as_str(), file.file.name.as_str(), file.file.role))
            .collect();
        assert_eq!(
            listed,
            [
                (PRIMARY, "Title.m4b", CopyRole::Audio),
                (
                    "Ann Author/Title/Chapters/01.mp3",
                    "Chapters/01.mp3",
                    CopyRole::Chapter
                ),
                ("Ann Author/Title/cover.jpg", "cover.jpg", CopyRole::Cover),
            ],
            "{name}: the PDF outside the title's folder is left out"
        );
        assert_eq!(
            (files[0].file.content_type.as_deref(), &files[0].file.probe),
            (
                Some("audio/mp4"),
                &Some(UploadProbe {
                    bitrate_kbps: Some(125),
                    lossless: false,
                    source_store: Some("audible".into()),
                })
            ),
            "{name}"
        );
        assert_eq!(
            (&files[1].file.content_type, &files[1].file.probe),
            (&None, &None),
            "{name}: an unprobed chapter states nothing"
        );
        // The list is one a frame may carry.
        let sized: Vec<_> = files.iter().map(|file| file.file.clone()).collect();
        assert_eq!(
            copy_granted_problem(&CopyGrantedFrame {
                copy_id: "6f1c2b7e-0d4a-4e8b-9a51-2c3d4e5f6a7b",
                sealed_token: "t",
                expires_in_secs: 60,
                copy: &plan.copy,
                files: &sized,
            }),
            None,
            "{name}"
        );

        // Granted, downloaded, and found again by its token alone.
        let opened = open_copy(&store, CopyHolder::Node(BRANCH), &plan, "local", "owner-1")
            .await
            .unwrap()
            .expect("the plan lists that destination");
        assert_eq!(
            opened.expires_in_secs,
            u64::try_from(COPY_TOKEN_TTL_SECS).unwrap()
        );
        let download = authorize_copy(&store, &opened.copy_id, &opened.token)
            .await
            .unwrap()
            .expect("a live grant");
        assert_eq!(
            (
                download.serving_node_id.as_str(),
                download.destination.as_str()
            ),
            ("owner-1", "local"),
            "{name}"
        );
        assert_eq!(&download.files, files, "{name}");
    }
}

/// A member sees every book in the library, so a branch whose owner holds
/// no store account is still given another member's edition.
#[tokio::test]
async fn a_member_may_take_an_edition_another_member_provided() {
    let store = memory(Profile::ALL[0]).await.into_store();
    library_with_a_copy(&store).await;
    let sam = store
        .create_user(UserRole::Member, Some("Sam"), None)
        .await
        .unwrap()
        .id;
    add_branch(&store, "sams-branch", sam).await;
    assert_eq!(planned(&store, "sams-branch").await.person, sam);
}

/// Anyone who is not a trusted branch is refused; an edition the library
/// does not list under that store, marketplace and product, or has not
/// acquired, is not found.
#[tokio::test]
async fn only_a_trusted_branch_is_answered_and_only_for_a_held_copy() {
    let store = memory(Profile::ALL[0]).await.into_store();
    let riley = library_with_a_copy(&store).await;
    add_branch(&store, BRANCH, riley).await;
    hold(&store, riley, "riley-acct", &["B0NOTYET01".to_string()]).await;

    let ask = |node: &'static str, listing: CopyListing<'static>| {
        let store = store.clone();
        async move {
            plan_copy(&store, CopyHolder::Node(node), listing)
                .await
                .unwrap()
        }
    };
    assert_eq!(ask("nobody", listing(ASIN)).await, CopyPlanned::Refused);
    for (what, named) in [
        ("an unknown product", listing("B0UNKNOWN0")),
        ("not acquired", listing("B0NOTYET01")),
        (
            "another marketplace",
            CopyListing {
                marketplace: "uk",
                ..listing(ASIN)
            },
        ),
        (
            "another store",
            CopyListing {
                store: "libro",
                ..listing(ASIN)
            },
        ),
    ] {
        assert_eq!(ask(BRANCH, named).await, CopyPlanned::NotFound, "{what}");
    }

    // The primary audio's row gone: there is nothing to list.
    raw(
        &store,
        &format!("DELETE FROM edition_files WHERE storage_key = '{PRIMARY}'"),
    )
    .await;
    assert_eq!(ask(BRANCH, listing(ASIN)).await, CopyPlanned::NotFound);

    // A branch whose credential was revoked, and one whose owner was
    // disabled, are refused like a stranger.
    add_branch(&store, "revoked", riley).await;
    raw(
        &store,
        "UPDATE node_credentials SET revoked_at = '2026-01-01T00:00:00Z' WHERE node_id = 'revoked'",
    )
    .await;
    assert_eq!(ask("revoked", listing(ASIN)).await, CopyPlanned::Refused);
    store
        .set_user_status(riley, UserStatus::Disabled)
        .await
        .unwrap();
    assert_eq!(ask(BRANCH, listing(ASIN)).await, CopyPlanned::Refused);
}

/// A grant opens downloads only with its own token, and stops the moment
/// the token expires, the files change, the edition leaves the library,
/// the branch's trust ends, or its owner is disabled.
#[tokio::test]
async fn a_grant_is_checked_afresh_on_every_download() {
    /// A library with a live grant; answers the grant.
    async fn granted() -> (LibraryStore, OpenedCopy) {
        let store = memory(Profile::ALL[0]).await.into_store();
        let riley = library_with_a_copy(&store).await;
        add_branch(&store, BRANCH, riley).await;
        let plan = planned(&store, BRANCH).await;
        // A destination the plan does not list opens nothing.
        assert!(
            open_copy(&store, CopyHolder::Node(BRANCH), &plan, "s3", "owner-1")
                .await
                .unwrap()
                .is_none()
        );
        let opened = open_copy(&store, CopyHolder::Node(BRANCH), &plan, "local", "owner-1")
            .await
            .unwrap()
            .unwrap();
        (store, opened)
    }
    /// Whether the grant still opens a download.
    async fn live(store: &LibraryStore, opened: &OpenedCopy) -> bool {
        authorize_copy(store, &opened.copy_id, &opened.token)
            .await
            .unwrap()
            .is_some()
    }

    let (store, opened) = granted().await;
    assert!(live(&store, &opened).await);
    // Only the token's hash is stored.
    let row = copy_grants::Entity::find_by_id(opened.copy_id.clone())
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(row.token_hash, *opened.token);
    assert_eq!(row.token_hash, crate::hash_token(&opened.token));
    // The log redaction knows it while the grant is held.
    let line = format!("token {}", opened.token.as_str());
    assert!(!bookclerk_config::redact_str(&line).contains(opened.token.as_str()));
    // Nor does the grant print it.
    let printed = format!("{opened:?}");
    assert!(printed.contains(&opened.copy_id) && !printed.contains(opened.token.as_str()));
    assert_eq!(
        (row.holder_kind.as_str(), row.holder_id.as_str()),
        ("node", BRANCH)
    );

    // Another token, another copy's id, and ids and tokens that are not
    // ones at all.
    let other = {
        let plan = planned(&store, BRANCH).await;
        open_copy(&store, CopyHolder::Node(BRANCH), &plan, "local", "owner-1")
            .await
            .unwrap()
            .unwrap()
    };
    for (id, token) in [
        (opened.copy_id.as_str(), other.token.as_str()),
        (other.copy_id.as_str(), opened.token.as_str()),
        (opened.copy_id.as_str(), ""),
        ("not-a-copy-id", opened.token.as_str()),
        (
            "00000000-0000-0000-0000-000000000000",
            opened.token.as_str(),
        ),
    ] {
        assert_eq!(
            authorize_copy(&store, id, token).await.unwrap(),
            None,
            "{id}"
        );
    }
    let long = "a".repeat(129);
    assert_eq!(
        authorize_copy(&store, &opened.copy_id, &long)
            .await
            .unwrap(),
        None
    );

    // Each ending, on a fresh library.
    for (what, change) in [
        (
            "expired",
            "UPDATE copy_grants SET expires_at = '2020-01-01T00:00:00+00:00'".to_string(),
        ),
        (
            "an unreadable expiry",
            "UPDATE copy_grants SET expires_at = 'soon'".to_string(),
        ),
        (
            "a file written again",
            "UPDATE edition_files SET written_at = '2030-01-01T00:00:00Z' \
             WHERE storage_key LIKE '%cover.jpg' AND destination = 'local'"
                .to_string(),
        ),
        (
            "a file added",
            "INSERT INTO edition_files (destination, storage_key, owner_kind, owner_id, role, \
             lossless, writer, written_at) SELECT 'local', 'Ann Author/Title/extra.cue', \
             owner_kind, owner_id, 'sidecar', 0, 'acquire', written_at FROM edition_files \
             WHERE storage_key LIKE '%Title.m4b'"
                .to_string(),
        ),
        (
            "the primary audio gone",
            format!("DELETE FROM edition_files WHERE storage_key = '{PRIMARY}'"),
        ),
        (
            "no longer acquired",
            "UPDATE edition_acquisitions SET status = 'not_acquired'".to_string(),
        ),
        (
            "no longer held by any account",
            "DELETE FROM holdings".to_string(),
        ),
        (
            "the branch's credential revoked",
            "UPDATE node_credentials SET revoked_at = '2026-01-01T00:00:00Z'".to_string(),
        ),
        (
            "the branch given to another person",
            "UPDATE nodes SET owner_user_id = NULL".to_string(),
        ),
        (
            "the person disabled",
            "UPDATE users SET status = 'disabled'".to_string(),
        ),
        (
            "the grant read as a card's, which no card holds",
            "UPDATE copy_grants SET holder_kind = 'card'".to_string(),
        ),
    ] {
        let (store, opened) = granted().await;
        assert!(live(&store, &opened).await, "{what}: live before");
        raw(&store, &change).await;
        assert!(!live(&store, &opened).await, "{what}");
    }
}

/// A library card is planned and granted the library's copy for its
/// person, another member's edition included, and the grant opens
/// downloads only while that card is signed in: signing the card out, its
/// expiry, a disabled person, an identity that no longer names the person
/// or a session that is not a card's all end it. Under every database
/// profile.
#[tokio::test]
async fn a_library_card_is_granted_a_copy_while_it_is_signed_in_under_every_profile() {
    /// A library where Morgan, who holds no book, has a live card;
    /// answers Morgan's user id and the card's token hash.
    async fn with_a_card(profile: Profile) -> (LibraryStore, i64, String) {
        let store = memory(profile).await.into_store();
        library_with_a_copy(&store).await;
        let morgan = store
            .create_user(UserRole::Member, Some("Morgan"), None)
            .await
            .unwrap()
            .id;
        let identity = store
            .ensure_local_portal_identity(morgan, Some("Morgan"))
            .await
            .unwrap();
        let card = crate::hash_token("morgans-card");
        store
            .insert_clerk_session(
                &card,
                identity.id,
                Utc::now() + Duration::hours(1),
                "Morgan's phone",
            )
            .await
            .unwrap();
        (store, morgan, card)
    }
    /// The grant of [`ASIN`] to the card.
    async fn granted(store: &LibraryStore, card: &str) -> (CopyPlan, OpenedCopy) {
        let CopyPlanned::Planned(plan) = plan_copy(store, CopyHolder::Card(card), listing(ASIN))
            .await
            .unwrap()
        else {
            panic!("the card was not planned a copy");
        };
        let opened = open_copy(store, CopyHolder::Card(card), &plan, "local", "owner-1")
            .await
            .unwrap()
            .unwrap();
        (plan, opened)
    }

    for profile in Profile::ALL {
        let name = profile.name();
        let (store, morgan, card) = with_a_card(profile).await;
        let (plan, opened) = granted(&store, &card).await;
        assert_eq!(plan.person, morgan, "{name}");
        let row = copy_grants::Entity::find_by_id(opened.copy_id.clone())
            .one(store.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                row.holder_kind.as_str(),
                row.holder_id.as_str(),
                row.user_id
            ),
            ("card", card.as_str(), morgan),
            "{name}"
        );
        let download = authorize_copy(&store, &opened.copy_id, &opened.token)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{name}: the grant opens no download"));
        assert_eq!(download.destination, "local", "{name}");

        // A card nobody holds, and a node id where a card is expected.
        for holder in [
            CopyHolder::Card("0000"),
            CopyHolder::Card(BRANCH),
            CopyHolder::Node(card.as_str()),
        ] {
            assert_eq!(
                plan_copy(&store, holder, listing(ASIN)).await.unwrap(),
                CopyPlanned::Refused,
                "{name}: {holder:?}"
            );
        }
        // The same answer for an edition the library has no copy of.
        assert_eq!(
            plan_copy(&store, CopyHolder::Card(&card), listing("B0NOCOPY00"))
                .await
                .unwrap(),
            CopyPlanned::NotFound,
            "{name}"
        );

        // Turning downloads off for Morgan ends the grant and refuses a
        // new plan; turning them back on restores both.
        for allowed in [false, true] {
            crate::control_plane::set_member_control_overrides(
                &store,
                morgan,
                crate::control_plane::ControlOverrides {
                    download: Some(allowed),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(
                authorize_copy(&store, &opened.copy_id, &opened.token)
                    .await
                    .unwrap()
                    .is_some(),
                allowed,
                "{name}: downloads {allowed}"
            );
            assert_eq!(
                matches!(
                    plan_copy(&store, CopyHolder::Card(&card), listing(ASIN))
                        .await
                        .unwrap(),
                    CopyPlanned::Planned(_)
                ),
                allowed,
                "{name}: downloads {allowed}"
            );
        }

        // Signing the card out ends the grant at once.
        assert!(store.delete_clerk_session(&card).await.unwrap());
        assert_eq!(
            authorize_copy(&store, &opened.copy_id, &opened.token)
                .await
                .unwrap(),
            None,
            "{name}: signed out"
        );
        assert_eq!(
            plan_copy(&store, CopyHolder::Card(&card), listing(ASIN))
                .await
                .unwrap(),
            CopyPlanned::Refused,
            "{name}: signed out"
        );

        // Each other ending, on a fresh library.
        for (what, change) in [
            (
                "the card expired",
                "UPDATE portal_sessions SET expires_at = '2020-01-01T00:00:00+00:00'",
            ),
            (
                "a browser session with that hash",
                "UPDATE portal_sessions SET kind = 'browser'",
            ),
            (
                "the person disabled",
                "UPDATE users SET status = 'disabled'",
            ),
            (
                "the identity names nobody",
                "UPDATE portal_identities SET user_id = NULL",
            ),
        ] {
            let (store, _, card) = with_a_card(profile).await;
            let (_, opened) = granted(&store, &card).await;
            raw(&store, change).await;
            assert_eq!(
                authorize_copy(&store, &opened.copy_id, &opened.token)
                    .await
                    .unwrap(),
                None,
                "{name}: {what}"
            );
        }

        // A card that now names another person does not carry the grant
        // over to them.
        let (store, _, card) = with_a_card(profile).await;
        let (_, opened) = granted(&store, &card).await;
        let other = store
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap()
            .id;
        raw(
            &store,
            &format!("UPDATE portal_identities SET user_id = {other} WHERE user_id IS NOT NULL"),
        )
        .await;
        assert_eq!(
            authorize_copy(&store, &opened.copy_id, &opened.token)
                .await
                .unwrap(),
            None,
            "{name}: another person"
        );
    }
}

/// A destination that names files its own way holds the primary audio
/// under another key: its files are listed from the one row it recorded
/// as the primary audio, named below that file's folder. A destination
/// with two such rows and neither the acquisition's key is not offered.
#[tokio::test]
async fn a_destination_with_its_own_naming_is_planned_from_its_own_primary() {
    let store = memory(Profile::ALL[0]).await.into_store();
    let riley = library_with_a_copy(&store).await;
    add_branch(&store, BRANCH, riley).await;
    let record = store.get_book(ASIN, "riley-acct").await.unwrap().unwrap();
    let owner = store
        .file_owner_of_book(&record.uuid)
        .await
        .unwrap()
        .unwrap();
    for (destination, key, role) in [
        ("b2", "Title (Ann Author)/Title.m4b", FileRole::Audio),
        ("b2", "Title (Ann Author)/Part 2.m4b", FileRole::Part),
        ("b2", "Title (Ann Author)/cover.jpg", FileRole::Cover),
        ("b2", "Other/stray.pdf", FileRole::Pdf),
        ("twice", "X/One.m4b", FileRole::Audio),
        ("twice", "X/Two.m4b", FileRole::Audio),
    ] {
        assert!(store
            .record_edition_file(destination, key, &owner, role)
            .await
            .unwrap());
    }
    let plan = planned(&store, BRANCH).await;
    assert_eq!(
        plan.destinations.keys().collect::<Vec<_>>(),
        ["b2", "local"]
    );
    let listed: Vec<(&str, &str, CopyRole)> = plan.destinations["b2"]
        .iter()
        .map(|file| (file.key.as_str(), file.file.name.as_str(), file.file.role))
        .collect();
    assert_eq!(
        listed,
        [
            ("Title (Ann Author)/Title.m4b", "Title.m4b", CopyRole::Audio),
            (
                "Title (Ann Author)/Part 2.m4b",
                "Part 2.m4b",
                CopyRole::Part
            ),
            ("Title (Ann Author)/cover.jpg", "cover.jpg", CopyRole::Cover),
        ]
    );
    // The grant on that destination opens and is honoured.
    let opened = open_copy(&store, CopyHolder::Node(BRANCH), &plan, "b2", "owner-1")
        .await
        .unwrap()
        .unwrap();
    let download = authorize_copy(&store, &opened.copy_id, &opened.token)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(download.destination, "b2");
    assert_eq!(download.files.len(), 3);
}

/// A file the answer frame cannot carry (a name past the limit) is left
/// out, and an edition with more files than a frame lists is not offered
/// from that destination.
#[tokio::test]
async fn files_a_frame_cannot_carry_are_not_listed() {
    let store = memory(Profile::ALL[0]).await.into_store();
    let riley = library_with_a_copy(&store).await;
    add_branch(&store, BRANCH, riley).await;
    let long = "x".repeat(bookclerk_node_proto::MAX_COPY_NAME_BYTES + 1);
    raw(
        &store,
        &format!(
            "INSERT INTO edition_files (destination, storage_key, owner_kind, owner_id, role, \
             lossless, writer, written_at) SELECT 'local', 'Ann Author/Title/{long}', \
             owner_kind, owner_id, 'sidecar', 0, 'acquire', written_at FROM edition_files \
             WHERE storage_key LIKE '%Title.m4b'"
        ),
    )
    .await;
    let plan = planned(&store, BRANCH).await;
    assert_eq!(plan.destinations["local"].len(), 3);

    for n in 0..bookclerk_node_proto::MAX_COPY_FILES {
        raw(
            &store,
            &format!(
                "INSERT INTO edition_files (destination, storage_key, owner_kind, owner_id, \
                 role, lossless, writer, written_at) SELECT 'local', \
                 'Ann Author/Title/Chapters/x{n:04}.mp3', owner_kind, owner_id, 'chapter', 0, \
                 'acquire', written_at FROM edition_files WHERE storage_key LIKE '%Title.m4b'"
            ),
        )
        .await;
    }
    assert_eq!(
        plan_copy(&store, CopyHolder::Node(BRANCH), listing(ASIN))
            .await
            .unwrap(),
        CopyPlanned::NotFound
    );
}
