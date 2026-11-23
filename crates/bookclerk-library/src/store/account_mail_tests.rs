//! Account mail's settings, outbox, invite tickets and verification links,
//! under every database profile.

use chrono::{Duration, Utc};

use super::notifications::NotifyQueueLimits;
use super::LibraryStore;
use crate::hash_token;
use crate::models::UserRole;
use crate::notifications::{
    parse_account_mail_settings, parse_verify_email_request, AccountMailKind, DeliveryState,
    FailedAttempt,
};
use crate::test_support::{sqlite::memory, Profile};

/// Room for every job these tests enqueue.
const LIMITS: NotifyQueueLimits = NotifyQueueLimits {
    max_pending: 1_000,
    max_attempts: 3,
};

/// A member with `email`; answers their id.
async fn member(store: &LibraryStore, email: &str) -> i64 {
    store
        .create_user_with_profile(UserRole::Member, Some("Sam"), None, Some(email), None)
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn choosing_another_notifier_clears_the_last_test_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        assert!(
            store.account_mail_settings().await.unwrap().is_none(),
            "{name}"
        );
        store
            .set_account_mail_instance(Some("email-1"))
            .await
            .unwrap();
        let user = member(&store, "sam@example.com").await;
        let id = store
            .queue_account_mail(
                AccountMailKind::Test,
                user,
                "sam@example.com",
                "email-1",
                LIMITS,
            )
            .await
            .unwrap();
        assert!(
            store
                .record_account_mail_delivered(&id, None)
                .await
                .unwrap(),
            "{name}"
        );
        let settings = store.account_mail_settings().await.unwrap().unwrap();
        assert_eq!(settings.plugin_instance_id, "email-1", "{name}");
        assert!(settings.tested_at.is_some(), "{name}");
        // A second answer for the same mail changes nothing.
        assert!(
            !store
                .record_account_mail_delivered(&id, None)
                .await
                .unwrap(),
            "{name}"
        );

        // The same instance again keeps the test; another clears it.
        store
            .set_account_mail_instance(Some("email-1"))
            .await
            .unwrap();
        let same = store.account_mail_settings().await.unwrap().unwrap();
        assert!(same.tested_at.is_some(), "{name}");
        store
            .set_account_mail_instance(Some("email-2"))
            .await
            .unwrap();
        let other = store.account_mail_settings().await.unwrap().unwrap();
        assert!(other.tested_at.is_none(), "{name}");
        store.set_account_mail_instance(None).await.unwrap();
        assert!(
            store.account_mail_settings().await.unwrap().is_none(),
            "{name}"
        );
    }
}

#[tokio::test]
async fn mail_is_queued_retried_and_ended_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let user = member(&store, "kim@example.com").await;
        let id = store
            .queue_account_mail(
                AccountMailKind::Invite,
                user,
                "kim@example.com",
                "email-1",
                LIMITS,
            )
            .await
            .unwrap();
        let mail = store.account_mail(&id).await.unwrap().unwrap();
        assert_eq!(mail.kind, AccountMailKind::Invite, "{name}");
        assert_eq!(mail.state, DeliveryState::Pending, "{name}");
        // Its send job is linked, so the sweep has nothing to schedule.
        assert!(
            store.unscheduled_account_mail(10).await.unwrap().is_empty(),
            "{name}"
        );
        assert_eq!(
            store.drop_orphaned_account_mail().await.unwrap(),
            0,
            "{name}"
        );
        assert_eq!(
            store
                .record_account_mail_retry(&id, "busy", None)
                .await
                .unwrap(),
            Some(1),
            "{name}"
        );
        assert!(store
            .record_account_mail_ended(
                &id,
                DeliveryState::Failed,
                "refused",
                FailedAttempt::New,
                None
            )
            .await
            .unwrap());
        let mail = store.account_mail(&id).await.unwrap().unwrap();
        assert_eq!(mail.state, DeliveryState::Failed, "{name}");
        assert_eq!(mail.attempts, 2, "{name}");
        assert!(store
            .record_account_mail_ended(&id, DeliveryState::Delivered, "x", FailedAttempt::New, None)
            .await
            .is_err());
        assert!(store
            .last_account_mail_at(user, AccountMailKind::Invite)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .last_account_mail_at(user, AccountMailKind::VerifyEmail)
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn only_the_newest_invite_works_and_using_it_verifies_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let user = member(&store, "lee@example.com").await;
        let identity = store
            .ensure_local_portal_identity(user, Some("Lee"))
            .await
            .unwrap();
        let first = store
            .mint_invite_ticket(identity.id, "lee@example.com", "account_mail")
            .await
            .unwrap();
        let second = store
            .mint_invite_ticket(identity.id, "lee@example.com", "account_mail")
            .await
            .unwrap();
        assert!(store
            .get_claim_ticket_by_hash(&hash_token(first.expose()))
            .await
            .unwrap()
            .is_none());
        let ticket = store
            .get_claim_ticket_by_hash(&hash_token(second.expose()))
            .await
            .unwrap()
            .expect("newest ticket");
        assert_eq!(
            ticket.emailed_to.as_deref(),
            Some("lee@example.com"),
            "{name}"
        );
        assert!(format!("{second:?}").contains("redacted"), "{name}");

        // Only the address the user still has is verified.
        assert!(!store
            .verify_user_email_if(user, "old@example.com")
            .await
            .unwrap());
        assert!(store
            .verify_user_email_if(user, "lee@example.com")
            .await
            .unwrap());
        assert!(
            store.get_user(user).await.unwrap().unwrap().email_verified,
            "{name}"
        );
        // Changing the address clears the mark; setting the same one keeps it.
        store
            .set_user_email(user, Some("LEE@example.com "))
            .await
            .unwrap();
        store
            .set_user_email(user, Some("lee2@example.com"))
            .await
            .unwrap();
        assert!(
            !store.get_user(user).await.unwrap().unwrap().email_verified,
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_verification_link_is_single_use_and_expires_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let user = member(&store, "ash@example.com").await;
        let minted = store
            .mint_email_verification(user, "ash@example.com")
            .await
            .unwrap();
        let body = format!(r#"{{"token":"{}"}}"#, minted.expose());
        let token = parse_verify_email_request(body.as_bytes()).unwrap();
        // Past its 24 hours it does nothing, and is used up.
        let later = Utc::now() + Duration::hours(25);
        assert_eq!(
            store.use_email_verification(&token, later).await.unwrap(),
            None
        );

        let minted = store
            .mint_email_verification(user, "ash@example.com")
            .await
            .unwrap();
        let body = format!(r#"{{"token":"{}"}}"#, minted.expose());
        let token = parse_verify_email_request(body.as_bytes()).unwrap();
        let now = Utc::now();
        assert_eq!(
            store.use_email_verification(&token, now).await.unwrap(),
            Some(user),
            "{name}"
        );
        assert_eq!(
            store.use_email_verification(&token, now).await.unwrap(),
            None
        );

        // A newer link voids the older one.
        let old = store
            .mint_email_verification(user, "ash@example.com")
            .await
            .unwrap();
        let _new = store
            .mint_email_verification(user, "ash@example.com")
            .await
            .unwrap();
        let body = format!(r#"{{"token":"{}"}}"#, old.expose());
        let token = parse_verify_email_request(body.as_bytes()).unwrap();
        assert_eq!(
            store.use_email_verification(&token, now).await.unwrap(),
            None
        );
        // The pass prunes the remaining link once it expires.
        store.prune_account_mail(later).await.unwrap();
    }
}

#[test]
fn the_request_bodies_follow_their_rules() {
    assert_eq!(
        parse_account_mail_settings(br#"{"plugin_instance_id":"email-1"}"#).unwrap(),
        Some("email-1".into())
    );
    assert_eq!(
        parse_account_mail_settings(br#"{"plugin_instance_id":null}"#).unwrap(),
        None
    );
    for bad in [
        &b"{}"[..],
        br#"{"plugin_instance_id":""}"#,
        br#"{"plugin_instance_id":"a b"}"#,
        br#"["x"]"#,
        br#"{"plugin_instance_id":"x","other":1}"#,
    ] {
        assert!(parse_account_mail_settings(bad).is_err());
    }
    let hex = "0123456789abcdef".repeat(4);
    assert!(parse_verify_email_request(format!(r#"{{"token":"{hex}"}}"#).as_bytes()).is_ok());
    for bad in [
        r#"{"token":"abc"}"#.to_string(),
        format!(r#"{{"token":"{}"}}"#, hex.to_uppercase()),
        r#"{"token":null}"#.to_string(),
    ] {
        assert!(parse_verify_email_request(bad.as_bytes()).is_err());
    }
}
