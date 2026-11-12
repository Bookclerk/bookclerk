//! Catalog lookup between libraries: the library stores questions per
//! branch and sends each once (again after the resend window), the branch
//! answers by identifier from the books its listing settings allow, and the
//! library keeps only answers that land on a stored edition of the asked
//! work, as `branch:<node>` listings that later answers can expire.

use bookclerk_node_proto::{Availability, CatalogMetadata, LookupAnswer, LookupEdition};
use sea_orm::{ActiveModelTrait, ActiveValue::Set};

use super::*;
use crate::entities::{node_credentials, nodes};
use crate::master_key::master_key_test_lock_async;
use crate::models::UserRole;
use crate::store::{LibraryStore, NewBook};

const PLUGIN: &str = "path:file:///tmp/store#audible";
const BRANCH: &str = "branch-b";

async fn store_in(dir: &std::path::Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(&dir.join("library.db"))
        .await
        .expect("open sqlite file");
    crate::apply_host_schema(&db).await.expect("schema");
    crate::configure_master_key(dir).expect("master key");
    LibraryStore::from_connection(db)
}

async fn add_branch(store: &LibraryStore, node_id: &str, owner: i64) {
    let now = Utc::now().to_rfc3339();
    nodes::ActiveModel {
        node_id: Set(node_id.into()),
        cluster_id: Set("cluster".into()),
        created_at: Set(now.clone()),
        incarnation: Set("test".into()),
        heartbeat_at: Set(now.clone()),
        software_version: Set("1.0.0".into()),
        schema_state: Set("current".into()),
        compatible: Set(1),
        target: Set(String::new()),
        labels_json: Set("[]".into()),
        kind: Set("host".into()),
        owner_user_id: Set(Some(owner)),
        ..Default::default()
    }
    .insert(store.db())
    .await
    .unwrap();
    node_credentials::ActiveModel {
        node_id: Set(node_id.into()),
        sign_public_key: Set(vec![1; 32]),
        box_public_key: Set(vec![2; 32]),
        link_id: Set(format!("link-{node_id}")),
        enrolled_at: Set(now),
        revoked_at: Set(None),
        last_connected_at: Set(None),
    }
    .insert(store.db())
    .await
    .unwrap();
}

fn book(account: &str, asin: &str, title: &str) -> NewBook {
    NewBook {
        uuid: None,
        product_id: asin.into(),
        plugin_key: PLUGIN.into(),
        source: "audible".into(),
        account_id: account.into(),
        asin: Some(asin.into()),
        isbn: None,
        marketplace: "us".into(),
        title: title.into(),
        authors: Some("Ann Author".into()),
        narrators: Some("Ned Narrator".into()),
        series: None,
        series_index: None,
        series_asin: None,
        purchased_at: None,
        publisher: None,
        length_minutes: Some(600),
        is_abridged: false,
        content_kind: "book".into(),
        categories: None,
        subtitle: None,
        published_at: None,
    }
}

/// Records `asin` as a book of `user` (on their own account) and returns
/// its work id.
async fn hold(store: &LibraryStore, user: i64, account: &str, asin: &str) -> String {
    store
        .upsert_account(PLUGIN, account, "us", None, true, "audible")
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(user, None)
        .await
        .unwrap();
    store
        .link_account(identity.id, PLUGIN, account, "test")
        .await
        .unwrap();
    let record = store
        .upsert_book(&book(account, asin, &format!("Title {asin}")))
        .await
        .unwrap();
    store.work_id_for_book(&record.uuid).await.unwrap().unwrap()
}

/// A library with a member who owns branch node [`BRANCH`].
async fn library(dir: &std::path::Path) -> (LibraryStore, i64) {
    let store = store_in(dir).await;
    let member = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap()
        .id;
    add_branch(&store, BRANCH, member).await;
    (store, member)
}

fn edition(asin: &str) -> LookupEdition {
    LookupEdition {
        store: "audible".into(),
        marketplace: "us".into(),
        product_id: asin.into(),
        availability: Availability::Owned,
        formats: Vec::new(),
        metadata: CatalogMetadata {
            title: format!("Title {asin}"),
            authors: Some("Ann Author".into()),
            narrators: Some("Ned Narrator".into()),
            asin: Some(asin.into()),
            ..CatalogMetadata::default()
        },
    }
}

async fn recorded(
    store: &LibraryStore,
    answers: &[LookupAnswer],
    missing: &[String],
) -> AnswerReport {
    match record_answers(store, BRANCH, answers, missing)
        .await
        .unwrap()
    {
        AnswersRecorded::Recorded(report) => report,
        other => panic!("expected the answers recorded: {other:?}"),
    }
}

#[tokio::test]
async fn questions_go_out_once_and_answers_become_branch_listings() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, member) = library(dir.path()).await;
    let work = hold(&store, member, "riley-acct", "B0ASIN0001").await;

    let asked = ask_branches(&store, &[work.clone(), "no-such-work".into()])
        .await
        .unwrap();
    assert_eq!(
        asked,
        AskReport {
            branches: 1,
            asked: 1
        }
    );

    let questions = pending_questions(&store, BRANCH).await.unwrap().unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].work, work);
    assert!(questions[0].identifiers.iter().any(|id| {
        id.scheme == LookupScheme::Asin && id.value == "B0ASIN0001" && id.marketplace == "us"
    }));
    assert_eq!(questions[0].problem(), None);
    // Sent: not sent again within the resend window.
    assert!(pending_questions(&store, BRANCH)
        .await
        .unwrap()
        .unwrap()
        .is_empty());

    let report = recorded(
        &store,
        &[LookupAnswer {
            work: work.clone(),
            editions: vec![edition("B0ASIN0001")],
        }],
        &[],
    )
    .await;
    assert_eq!(report.works, 1);
    assert_eq!(report.listings, 1);
    assert_eq!(report.dropped, 0);

    let listings = branch_listings_for_works(&store, std::slice::from_ref(&work))
        .await
        .unwrap();
    assert_eq!(listings.len(), 1);
    assert_eq!(listings[0].node_id, BRANCH);
    assert_eq!(listings[0].work_id, work);
    assert_eq!(listings[0].store, "audible");
    assert_eq!(listings[0].product_id, "B0ASIN0001");

    // Answered recently: asking again changes nothing.
    let again = ask_branches(&store, std::slice::from_ref(&work))
        .await
        .unwrap();
    assert_eq!(again.asked, 0);
    // A second answer for a settled question is dropped.
    let late = recorded(
        &store,
        &[LookupAnswer {
            work: work.clone(),
            editions: vec![edition("B0ASIN0001")],
        }],
        &[],
    )
    .await;
    assert_eq!((late.works, late.listings, late.dropped), (0, 0, 1));
}

#[tokio::test]
async fn answers_off_the_asked_work_are_dropped_and_missing_expires() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, member) = library(dir.path()).await;
    let work = hold(&store, member, "riley-acct", "B0ASIN0001").await;
    let other = hold(&store, member, "riley-acct", "B0ASIN0002").await;
    assert_ne!(work, other);

    ask_branches(&store, std::slice::from_ref(&work))
        .await
        .unwrap();
    pending_questions(&store, BRANCH).await.unwrap().unwrap();
    // An edition of another work, and one this library has never seen
    // (it would need a new edition, so a title match), are both dropped.
    let report = recorded(
        &store,
        &[LookupAnswer {
            work: work.clone(),
            editions: vec![
                edition("B0ASIN0001"),
                edition("B0ASIN0002"),
                edition("B0UNKNOWN9"),
            ],
        }],
        &[],
    )
    .await;
    assert_eq!((report.listings, report.dropped), (1, 2));
    assert!(
        branch_listings_for_works(&store, std::slice::from_ref(&other))
            .await
            .unwrap()
            .is_empty()
    );

    // Asked again later, the branch no longer has it: the listing expires.
    let old = stamp(Utc::now() - LOOKUP_ANSWER_TTL - chrono::Duration::minutes(1));
    run_batches(
        &store,
        "test",
        vec![exec(
            "UPDATE catalog_lookups SET answered_at = ?",
            vec![text(&old)],
        )],
    )
    .await
    .unwrap();
    assert_eq!(
        ask_branches(&store, std::slice::from_ref(&work))
            .await
            .unwrap()
            .asked,
        1
    );
    pending_questions(&store, BRANCH).await.unwrap().unwrap();
    let gone = recorded(&store, &[], std::slice::from_ref(&work)).await;
    assert_eq!((gone.works, gone.expired), (1, 1));
    assert!(
        branch_listings_for_works(&store, std::slice::from_ref(&work))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn an_untrusted_node_gets_no_questions_and_records_nothing() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, _member) = library(dir.path()).await;
    assert_eq!(pending_questions(&store, "stranger").await.unwrap(), None);
    assert_eq!(
        record_answers(&store, "stranger", &[], &[]).await.unwrap(),
        AnswersRecorded::Refused("not_a_branch")
    );
    let bad = LookupAnswer {
        work: "w w".into(),
        editions: vec![edition("B0ASIN0001")],
    };
    assert!(matches!(
        record_answers(&store, BRANCH, &[bad], &[]).await,
        Err(LibraryError::Invalid(_))
    ));
}

#[tokio::test]
async fn an_answer_to_a_question_not_yet_sent_is_dropped() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, member) = library(dir.path()).await;
    let work = hold(&store, member, "riley-acct", "B0ASIN0001").await;
    ask_branches(&store, std::slice::from_ref(&work))
        .await
        .unwrap();
    let answer = LookupAnswer {
        work: work.clone(),
        editions: vec![edition("B0ASIN0001")],
    };
    let early = recorded(&store, std::slice::from_ref(&answer), &[]).await;
    assert_eq!((early.works, early.listings, early.dropped), (0, 0, 1));
    assert!(
        branch_listings_for_works(&store, std::slice::from_ref(&work))
            .await
            .unwrap()
            .is_empty()
    );
    // Once sent, the same answer is recorded.
    assert_eq!(
        pending_questions(&store, BRANCH)
            .await
            .unwrap()
            .unwrap()
            .len(),
        1
    );
    let sent = recorded(&store, &[answer], &[]).await;
    assert_eq!((sent.works, sent.listings), (1, 1));
}

#[tokio::test]
async fn a_branch_lists_members_books_unless_turned_off_or_opted_out() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    let owner = store
        .create_user(UserRole::Owner, Some("Owner"), None)
        .await
        .unwrap();
    let member = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap();
    hold(&store, owner.id, "owner-acct", "B0OWNED001").await;
    hold(&store, member.id, "riley-acct", "B0MEMBER01").await;
    let question = |work: &str, asin: &str| LookupWork {
        work: work.into(),
        identifiers: vec![LookupIdentifier {
            scheme: LookupScheme::Asin,
            value: asin.into(),
            marketplace: "us".into(),
        }],
    };
    let questions = vec![
        question("x-owner", "B0OWNED001"),
        question("x-member", "B0MEMBER01"),
        question("x-none", "B0NOTHERE1"),
    ];

    let (answers, missing) = answer_questions(&store, &questions, true).await.unwrap();
    let answered: Vec<&str> = answers.iter().map(|a| a.work.as_str()).collect();
    assert_eq!(answered, ["x-owner", "x-member"]);
    assert_eq!(missing, ["x-none"]);
    assert_eq!(answers[1].editions[0].product_id, "B0MEMBER01");
    assert_eq!(answers[1].editions[0].availability, Availability::Owned);
    assert_eq!(found_problem(&answers, &missing), None);
    // Nothing names an account or a user.
    let text = serde_json::to_string(&answers).unwrap();
    assert!(!text.contains("acct") && !text.contains("Riley"), "{text}");

    // A book on an account linked to no one (disconnected) is never listed.
    store
        .upsert_account(PLUGIN, "loose-acct", "us", None, true, "audible")
        .await
        .unwrap();
    store
        .upsert_book(&book("loose-acct", "B0LOOSE001", "Loose"))
        .await
        .unwrap();
    let (answers, missing) = answer_questions(&store, &[question("x-loose", "B0LOOSE001")], true)
        .await
        .unwrap();
    assert!(answers.is_empty());
    assert_eq!(missing, ["x-loose"]);

    // The link setting off: only the owner's book is listed.
    let (answers, missing) = answer_questions(&store, &questions, false).await.unwrap();
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].work, "x-owner");
    assert_eq!(missing, ["x-member", "x-none"]);

    // The member opts out: the same with the setting on.
    set_catalog_listing_opt_out(&store, member.id, true)
        .await
        .unwrap();
    assert!(catalog_listing_opted_out(&store, member.id).await.unwrap());
    let (answers, _) = answer_questions(&store, &questions, true).await.unwrap();
    assert_eq!(answers.len(), 1);
    set_catalog_listing_opt_out(&store, member.id, false)
        .await
        .unwrap();
    let (answers, _) = answer_questions(&store, &questions, true).await.unwrap();
    assert_eq!(answers.len(), 2);
}

/// More works than one statement may bind are asked, sent in batches of at
/// most [`MAX_LOOKUP_WORKS`], answered, and answered on the branch side,
/// under every database profile.
#[tokio::test]
async fn many_works_are_asked_and_answered_within_every_profiles_limits() {
    const WORKS: usize = 110;
    for profile in crate::test_support::Profile::ALL {
        let name = profile.name();
        let store = crate::test_support::sqlite::memory(profile)
            .await
            .into_store();
        let owner = store
            .create_user(UserRole::Owner, Some("Owner"), None)
            .await
            .unwrap();
        add_branch(&store, BRANCH, owner.id).await;
        store
            .upsert_account(PLUGIN, "owner-acct", "us", None, true, "audible")
            .await
            .unwrap();
        let identity = store
            .ensure_local_portal_identity(owner.id, None)
            .await
            .unwrap();
        store
            .link_account(identity.id, PLUGIN, "owner-acct", "test")
            .await
            .unwrap();
        let mut works = Vec::new();
        for n in 0..WORKS {
            let asin = format!("B0MANY{n:04}");
            let record = store
                .upsert_book(&book("owner-acct", &asin, &format!("Title {asin}")))
                .await
                .unwrap();
            works.push(store.work_id_for_book(&record.uuid).await.unwrap().unwrap());
        }

        let asked = ask_branches(&store, &works).await.unwrap();
        assert_eq!(asked.asked, u64::try_from(WORKS).unwrap(), "{name}");
        assert_eq!(
            works_of_interest(&store, 10).await.unwrap(),
            Vec::<String>::new(),
            "{name}: no wishes or candidates"
        );

        let first = pending_questions(&store, BRANCH).await.unwrap().unwrap();
        assert_eq!(first.len(), MAX_LOOKUP_WORKS, "{name}");
        // The branch side looks a full batch up in one pass.
        let (answers, missing) = answer_questions(&store, &first, true).await.unwrap();
        assert_eq!(answers.len(), MAX_LOOKUP_WORKS, "{name}");
        assert!(missing.is_empty(), "{name}");
        let gone: Vec<String> = first.iter().map(|q| q.work.clone()).collect();
        let report = recorded(&store, &[], &gone).await;
        assert_eq!(
            report.works,
            u64::try_from(MAX_LOOKUP_WORKS).unwrap(),
            "{name}"
        );

        let rest = pending_questions(&store, BRANCH).await.unwrap().unwrap();
        assert_eq!(rest.len(), WORKS - MAX_LOOKUP_WORKS, "{name}");
        assert!(
            branch_listings_for_works(&store, &works)
                .await
                .unwrap()
                .is_empty(),
            "{name}"
        );
    }
}

#[tokio::test]
async fn offers_name_the_branch_owner_and_listings_find_their_work_under_every_profile() {
    for profile in crate::test_support::Profile::ALL {
        let name = profile.name();
        let store = crate::test_support::sqlite::memory(profile)
            .await
            .into_store();
        let member = store
            .create_user(UserRole::Member, Some("Riley"), None)
            .await
            .unwrap()
            .id;
        add_branch(&store, BRANCH, member).await;
        let work = hold(&store, member, "riley-acct", "B0OFFER001").await;
        let other = hold(&store, member, "riley-acct", "B0OFFER002").await;

        assert!(
            branch_offers_for_works(&store, std::slice::from_ref(&work))
                .await
                .unwrap()
                .is_empty(),
            "{name}: nothing answered yet"
        );
        ask_branches(&store, std::slice::from_ref(&work))
            .await
            .unwrap();
        pending_questions(&store, BRANCH).await.unwrap().unwrap();
        recorded(
            &store,
            &[LookupAnswer {
                work: work.clone(),
                editions: vec![edition("B0OFFER001")],
            }],
            &[],
        )
        .await;

        let offers = branch_offers_for_works(&store, &[work.clone(), other.clone()])
            .await
            .unwrap();
        assert_eq!(offers.len(), 1, "{name}: only the answered work");
        assert_eq!(
            offers.get(&work).cloned().unwrap_or_default(),
            vec![BranchOffer {
                node_id: BRANCH.into(),
                owner_name: Some("Riley".into()),
                store: "audible".into(),
                product_id: "B0OFFER001".into(),
            }],
            "{name}"
        );

        let works = works_of_listings(
            &store,
            &[
                (PLUGIN.into(), " B0OFFER001".into()),
                (PLUGIN.into(), "B0OFFER002".into()),
                (PLUGIN.into(), "B0NOTPOOLED".into()),
                ("other-store".into(), "B0OFFER001".into()),
                (branch_listing_key(BRANCH), "audible:B0OFFER001".into()),
            ],
        )
        .await
        .unwrap();
        assert_eq!(works.len(), 2, "{name}: {works:?}");
        assert_eq!(
            works.get(&(PLUGIN.to_string(), " B0OFFER001".to_string())),
            Some(&work),
            "{name}: matched in stored spelling, keyed as asked"
        );
        assert_eq!(
            works.get(&(PLUGIN.to_string(), "B0OFFER002".to_string())),
            Some(&other),
            "{name}"
        );
    }
}

#[tokio::test]
async fn offers_leave_out_branches_no_longer_trusted() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, member) = library(dir.path()).await;
    let work = hold(&store, member, "riley-acct", "B0OFFER003").await;
    ask_branches(&store, std::slice::from_ref(&work))
        .await
        .unwrap();
    pending_questions(&store, BRANCH).await.unwrap().unwrap();
    recorded(
        &store,
        &[LookupAnswer {
            work: work.clone(),
            editions: vec![edition("B0OFFER003")],
        }],
        &[],
    )
    .await;
    assert_eq!(
        branch_offers_for_works(&store, std::slice::from_ref(&work))
            .await
            .unwrap()
            .len(),
        1
    );
    let credential = node_credentials::Entity::find_by_id(BRANCH.to_string())
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    let mut revoked: node_credentials::ActiveModel = credential.into();
    revoked.revoked_at = Set(Some(Utc::now().to_rfc3339()));
    revoked.update(store.db()).await.unwrap();
    assert!(branch_offers_for_works(&store, std::slice::from_ref(&work))
        .await
        .unwrap()
        .is_empty());
}
