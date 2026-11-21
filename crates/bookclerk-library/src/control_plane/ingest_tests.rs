//! Ingest batches: creation, quotas, received bytes, the job's state
//! changes, expansion, outcomes, patron holdings and pruning, under every
//! database profile.

use super::*;
use crate::models::UserRole;
use crate::test_support::{sqlite, Profile};
use bookclerk_resolve::{
    Abridgement, Entitlement, ExpressionKind, Holder, HoldingDraft, IdentifierScheme, Listing,
    ListingDraft, MediaKind, WorkKind,
};

/// An upload member at `path` of `size` bytes.
fn spec(path: &str, size: u64) -> MemberSpec {
    MemberSpec {
        path: EntryPath::admit(path).unwrap(),
        size,
        sha256: None,
        intake: None,
    }
}

/// Generous limits.
const QUOTA: UploadQuota = UploadQuota {
    max_file_bytes: 1_000,
    max_staged_bytes: 2_000,
};

#[test]
fn ids_digests_and_offsets_parse_strictly() {
    let id = IngestBatchId::new();
    assert_eq!(IngestBatchId::parse(id.as_str()), Some(id.clone()));
    assert!(IngestBatchId::parse("ABC").is_none());
    assert!(IngestBatchId::parse(&id.as_str().to_ascii_uppercase()).is_none());
    let digest = "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD";
    assert_eq!(
        Sha256Hex::parse(digest).unwrap().as_str(),
        digest.to_ascii_lowercase()
    );
    assert!(Sha256Hex::parse("abc").is_none());
    assert_eq!(parse_upload_offset("0"), Some(0));
    assert_eq!(
        parse_upload_offset("9223372036854775807"),
        Some(i64::MAX.unsigned_abs())
    );
    assert_eq!(parse_upload_offset("9223372036854775808"), None);
    assert_eq!(parse_upload_offset("+1"), None);
    assert_eq!(parse_upload_offset(""), None);
    for state in IngestState::ALL {
        assert_eq!(IngestState::parse(state.as_str()), Some(*state));
    }
    for outcome in MemberOutcome::ALL {
        assert_eq!(MemberOutcome::parse(outcome.as_str()), Some(*outcome));
    }
}

#[test]
fn manifests_refuse_bad_paths_and_digests() {
    let specs = parse_upload_manifest(
        br#"{"files":[{"path":"Book\\01.mp3","size":3,"sha256":null},{"path":"Book/02.mp3","size":4}]}"#,
    )
    .unwrap();
    assert_eq!(specs[0].path.as_str(), "Book/01.mp3");
    assert_eq!(specs[1].size, 4);
    assert!(matches!(
        parse_upload_manifest(br#"{"files":[{"path":"../x","size":1}]}"#),
        Err(ManifestError::Path { member_no: 0, .. })
    ));
    assert!(matches!(
        parse_upload_manifest(br#"{"files":[{"path":"x","size":1,"sha256":"no"}]}"#),
        Err(ManifestError::Digest { member_no: 0 })
    ));
    assert!(matches!(
        parse_upload_manifest(br#"{"files":[],"more":1}"#),
        Err(ManifestError::Malformed(_))
    ));
}

#[test]
fn manifest_checks_catch_clashes_sizes_and_references() {
    let check = |specs: &[MemberSpec]| check_manifest(specs, IngestSource::Upload, 10);
    assert_eq!(check(&[]), Err(CreateRefusal::Empty));
    assert_eq!(
        check(&[spec("a.mp3", 1), spec("A.MP3", 1)]),
        Err(CreateRefusal::PathClash { member_no: 1 })
    );
    assert_eq!(
        check(&[spec("a", 1), spec("a/b", 1)]),
        Err(CreateRefusal::PathClash { member_no: 1 })
    );
    assert_eq!(
        check(&[spec("a.mp3", 11)]),
        Err(CreateRefusal::FileTooLarge { member_no: 0 })
    );
    let mut intake = spec("a.mp3", 1);
    intake.intake = Some(IntakeRef {
        key: "drop/a.mp3".into(),
        signature: None,
    });
    assert_eq!(
        check(std::slice::from_ref(&intake)),
        Err(CreateRefusal::BadReference { member_no: 0 })
    );
    assert_eq!(check_manifest(&[intake], IngestSource::Intake, 10), Ok(1));
    assert_eq!(check(&[spec("a.mp3", 4), spec("b.mp3", 5)]), Ok(9));
}

#[tokio::test]
async fn an_upload_runs_from_manifest_to_outcomes() {
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let member = store
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap();
        let owner = store
            .create_user(UserRole::Owner, Some("Olu"), None)
            .await
            .unwrap();
        assert_eq!(
            first_active_owner(store).await.unwrap(),
            Some(owner.id),
            "{name}"
        );

        // Upload is off for patrons unless an Owner turns it on.
        let refused = create_upload_batch(store, member.id, "node-a", &[spec("a.mp3", 1)], QUOTA)
            .await
            .unwrap();
        assert_eq!(refused, Err(CreateRefusal::NotAllowed), "{name}");

        let specs = [spec("Book/01.mp3", 600), spec("Book/cover.jpg", 10)];
        let batch = create_upload_batch(store, owner.id, "node-a", &specs, QUOTA)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(batch.state, IngestState::Open, "{name}");
        assert_eq!(batch.declared_bytes, 610, "{name}");
        assert_eq!(staged_bytes(store, owner.id).await.unwrap(), 610, "{name}");
        let over = create_upload_batch(
            store,
            owner.id,
            "node-a",
            &[spec("x.mp3", 1_000), spec("y.mp3", 1_000)],
            QUOTA,
        )
        .await
        .unwrap();
        assert!(
            matches!(over, Err(CreateRefusal::OverQuota { staged: 610, .. })),
            "{name}"
        );

        // Bytes arrive; committing waits for all of them.
        assert!(
            record_received(store, &batch.id, 0, 600).await.unwrap(),
            "{name}"
        );
        assert!(
            !record_received(store, &batch.id, 1, 11).await.unwrap(),
            "{name}: past size"
        );
        assert!(
            !mark_committed(store, &batch.id, "job-1").await.unwrap(),
            "{name}"
        );
        assert!(
            record_received(store, &batch.id, 1, 10).await.unwrap(),
            "{name}"
        );
        let members = batch_members(store, &batch.id).await.unwrap();
        assert!(members.iter().all(IngestMember::complete), "{name}");
        assert!(
            mark_committed(store, &batch.id, "job-1").await.unwrap(),
            "{name}"
        );
        assert!(
            !record_received(store, &batch.id, 0, 600).await.unwrap(),
            "{name}: closed"
        );

        // The job starts once; a retry of the same job may start again.
        assert!(
            !start_batch(store, &batch.id, "job-2").await.unwrap(),
            "{name}"
        );
        assert!(
            start_batch(store, &batch.id, "job-1").await.unwrap(),
            "{name}"
        );
        assert!(
            start_batch(store, &batch.id, "job-1").await.unwrap(),
            "{name}"
        );
        assert!(
            !cancel_batch(store, &batch.id).await.unwrap(),
            "{name}: running"
        );

        // An archive member expands into numbered children.
        let first = add_expanded_members(
            store,
            &batch.id,
            1,
            &[
                ExpandedEntry {
                    path: "Book/cover/x.jpg".into(),
                    size: 5,
                    outcome: MemberOutcome::Pending,
                    reason: None,
                },
                ExpandedEntry {
                    path: "evil".into(),
                    size: 0,
                    outcome: MemberOutcome::Refused,
                    reason: Some("link".into()),
                },
            ],
            None,
        )
        .await
        .unwrap();
        assert_eq!(first, 2, "{name}");
        let members = batch_members(store, &batch.id).await.unwrap();
        assert_eq!(members.len(), 4, "{name}");
        assert_eq!(members[1].outcome, MemberOutcome::Expanded, "{name}");
        assert_eq!(members[2].parent_no, Some(1), "{name}");
        assert_eq!(members[3].reason.as_deref(), Some("link"), "{name}");

        set_member_outcomes(
            store,
            &batch.id,
            &[
                OutcomeUpdate {
                    member_no: 0,
                    outcome: MemberOutcome::NeedsReview,
                    reason: None,
                    edition_id: Some("ed-1".into()),
                },
                OutcomeUpdate {
                    member_no: 2,
                    outcome: MemberOutcome::Refused,
                    reason: Some("no_audio".into()),
                    edition_id: None,
                },
            ],
        )
        .await
        .unwrap();
        let review = review_items(store, None).await.unwrap();
        assert_eq!(review.len(), 1, "{name}");
        assert_eq!(review[0].edition_id.as_deref(), Some("ed-1"), "{name}");
        assert_eq!(review[0].user_id, owner.id, "{name}");
        assert!(
            review_items(store, Some((&batch.id, 0)))
                .await
                .unwrap()
                .is_empty(),
            "{name}"
        );

        assert!(
            finish_batch(store, &batch.id, IngestState::Running, None)
                .await
                .is_err(),
            "{name}"
        );
        finish_batch(store, &batch.id, IngestState::Done, None)
            .await
            .unwrap();
        let done = load_batch(store, &batch.id).await.unwrap().unwrap();
        assert_eq!(done.state, IngestState::Done, "{name}");
        assert!(done.finished_at.is_some(), "{name}");
        assert_eq!(staged_bytes(store, owner.id).await.unwrap(), 0, "{name}");
        assert_eq!(
            final_batches_on(store, "node-a").await.unwrap(),
            vec![batch.id.clone()],
            "{name}"
        );
        assert!(
            final_batches_on(store, "node-b").await.unwrap().is_empty(),
            "{name}"
        );
        assert_eq!(
            user_batches(store, owner.id, 10).await.unwrap().len(),
            1,
            "{name}"
        );

        // Rows of finished batches go after their retention.
        assert_eq!(
            delete_finished_before(store, Utc::now() - chrono::Duration::days(1))
                .await
                .unwrap(),
            0,
            "{name}"
        );
        assert_eq!(
            delete_finished_before(store, Utc::now() + chrono::Duration::days(1))
                .await
                .unwrap(),
            1,
            "{name}"
        );
        assert!(!batch_exists(store, &batch.id).await.unwrap(), "{name}");
        assert!(
            batch_members(store, &batch.id).await.unwrap().is_empty(),
            "{name}"
        );
    }
}

#[tokio::test]
async fn idle_uploads_are_found_and_cancelled() {
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let owner = store
            .create_user(UserRole::Owner, Some("Olu"), None)
            .await
            .unwrap();
        let batch = create_upload_batch(store, owner.id, "node-a", &[spec("a.mp3", 3)], QUOTA)
            .await
            .unwrap()
            .unwrap();
        assert!(
            idle_upload_batches(store, "node-a", Utc::now() - chrono::Duration::hours(1))
                .await
                .unwrap()
                .is_empty(),
            "{name}"
        );
        let idle = idle_upload_batches(store, "node-a", Utc::now() + chrono::Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(idle, vec![batch.id.clone()], "{name}");
        assert!(cancel_batch(store, &batch.id).await.unwrap(), "{name}");
        assert!(!cancel_batch(store, &batch.id).await.unwrap(), "{name}");
        let cancelled = load_batch(store, &batch.id).await.unwrap().unwrap();
        assert_eq!(cancelled.state, IngestState::Cancelled, "{name}");
    }
}

#[tokio::test]
async fn intake_batches_start_complete() {
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let owner = store
            .create_user(UserRole::Owner, Some("Olu"), None)
            .await
            .unwrap();
        let mut member = spec("Drop/a.mp3", 9);
        member.intake = Some(IntakeRef {
            key: "inbox/Drop/a.mp3".into(),
            signature: Some("etag-1".into()),
        });
        let batch = create_intake_batch(
            store,
            &IngestBatchId::new(),
            owner.id,
            "node-a",
            "drop-1",
            &[member],
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(batch.source, IngestSource::Intake, "{name}");
        assert_eq!(batch.intake_instance.as_deref(), Some("drop-1"), "{name}");
        let members = batch_members(store, &batch.id).await.unwrap();
        assert!(members[0].complete(), "{name}");
        assert_eq!(
            members[0].storage_key.as_deref(),
            Some("inbox/Drop/a.mp3"),
            "{name}"
        );
        assert!(
            mark_committed(store, &batch.id, "job-1").await.unwrap(),
            "{name}"
        );
        // Intake bytes never count against the upload quota.
        assert_eq!(staged_bytes(store, owner.id).await.unwrap(), 0, "{name}");
        assert_eq!(
            create_intake_batch(
                store,
                &IngestBatchId::new(),
                owner.id,
                "node-a",
                "",
                &[spec("b.mp3", 1)],
            )
            .await
            .unwrap(),
            Err(CreateRefusal::BadReference { member_no: 0 }),
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_patron_holding_acquires_its_edition() {
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let owner = store
            .create_user(UserRole::Owner, Some("Olu"), None)
            .await
            .unwrap();
        let draft = ListingDraft {
            plugin_key: "core.import".into(),
            marketplace: None,
            product_id: "0123456789abcdef0123456789abcdef".into(),
            work_kind: Some(WorkKind::Book),
            expression_kind: Some(ExpressionKind::Narration),
            media_kind: Some(MediaKind::Audio),
            language: None,
            abridgement: Some(Abridgement::Unknown),
            title: "The Hobbit".into(),
            subtitle: None,
            published_at: None,
            duration_minutes: None,
            categories: None,
            identifiers: vec![(IdentifierScheme::Isbn, "9780261102217".into(), None)],
            credits: Vec::new(),
            series: Vec::new(),
            parent_product_id: None,
        };
        let listing = Listing::from_draft(&draft).unwrap();
        let plan = store
            .record_listing(
                &listing,
                &HoldingDraft {
                    holder: Holder::Patron(owner.id),
                    entitlement: Entitlement::Own,
                    purchased_at: None,
                },
            )
            .await
            .unwrap();
        let holding = patron_holding_id(store, &plan.listing.id, owner.id)
            .await
            .unwrap()
            .expect("patron holding");
        assert!(
            patron_holding_id(store, &plan.listing.id, owner.id + 1)
                .await
                .unwrap()
                .is_none(),
            "{name}"
        );
        let edition = plan.listing.edition_id.clone();
        assert!(
            record_ingest_acquired(store, &edition, &holding, "Tolkien/The Hobbit/a.mp3")
                .await
                .unwrap(),
            "{name}"
        );
        let acquired = store.edition_acquisition(&edition).await.unwrap().unwrap();
        assert_eq!(acquired.status, crate::AcquireStatus::Acquired, "{name}");
        assert_eq!(
            acquired.storage_key.as_deref(),
            Some("Tolkien/The Hobbit/a.mp3"),
            "{name}"
        );
        assert_eq!(acquired.holding_id, holding, "{name}");
        assert!(
            !record_ingest_acquired(store, &edition, "no-such-holding", "x")
                .await
                .unwrap(),
            "{name}"
        );
    }
}
