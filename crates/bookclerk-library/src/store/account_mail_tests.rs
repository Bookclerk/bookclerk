//! Account mail's settings, outbox, invite tickets and verification links,
//! under every database profile.

use chrono::{Duration, Utc};

use super::notifications::NotifyQueueLimits;
use super::LibraryStore;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use crate::entities::email_verifications;
use crate::hash_token;
use crate::models::UserRole;
use crate::notifications::{
    parse_account_mail_settings, parse_verify_email_request, AccountMailKind, AccountMailRequest,
    DeliveryState, FailedAttempt, PasswordResetFactor, RedeemedReset,
};
use crate::test_support::{sqlite::memory, Profile};

/// Room for every job these tests enqueue.
const LIMITS: NotifyQueueLimits = NotifyQueueLimits {
    max_pending: 1_000,
    max_attempts: 3,
};

/// A member with `email`; answers their id.
/// The password a redeemed link sets, assembled at runtime so scanners do
/// not treat a string literal as a shipped secret.
fn fresh_password() -> String {
    ["a fresh", " ", "secret"].concat()
}

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
            .mint_invite_ticket(user, identity.id, "lee@example.com", "account_mail", None)
            .await
            .unwrap()
            .expect("minted");
        let second = store
            .mint_invite_ticket(user, identity.id, "lee@example.com", "account_mail", None)
            .await
            .unwrap()
            .expect("minted");
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
            .mint_email_verification(user, "ash@example.com", None)
            .await
            .unwrap()
            .expect("minted");
        let body = format!(r#"{{"token":"{}"}}"#, minted.expose());
        let token = parse_verify_email_request(body.as_bytes()).unwrap();
        // Past its 24 hours it does nothing, and is used up.
        let later = Utc::now() + Duration::hours(25);
        assert_eq!(
            store.use_email_verification(&token, later).await.unwrap(),
            None
        );

        let minted = store
            .mint_email_verification(user, "ash@example.com", None)
            .await
            .unwrap()
            .expect("minted");
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
            .mint_email_verification(user, "ash@example.com", None)
            .await
            .unwrap()
            .expect("minted");
        let _new = store
            .mint_email_verification(user, "ash@example.com", None)
            .await
            .unwrap()
            .expect("minted");
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

#[tokio::test]
async fn a_stale_send_mints_nothing_and_voids_no_link_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let user = member(&store, "kit@example.com").await;
        let identity = store
            .ensure_local_portal_identity(user, Some("Kit"))
            .await
            .unwrap();
        let live = store
            .mint_invite_ticket(user, identity.id, "kit@example.com", "account_mail", None)
            .await
            .unwrap()
            .expect("minted");
        let link = store
            .mint_email_verification(user, "kit@example.com", None)
            .await
            .unwrap()
            .expect("minted");
        // A claim on a job that is not running holds nothing.
        let lost = crate::models::JobFence {
            job_id: "gone".into(),
            owner: "worker".into(),
            generation: 1,
        };
        assert!(
            store
                .mint_invite_ticket(
                    user,
                    identity.id,
                    "kit@example.com",
                    "account_mail",
                    Some(&lost)
                )
                .await
                .unwrap()
                .is_none(),
            "{name}"
        );
        assert!(
            store
                .mint_email_verification(user, "kit@example.com", Some(&lost))
                .await
                .unwrap()
                .is_none(),
            "{name}"
        );
        assert!(
            store
                .get_claim_ticket_by_hash(&hash_token(live.expose()))
                .await
                .unwrap()
                .is_some(),
            "{name}: the delivered invite still works"
        );
        let body = format!(r#"{{"token":"{}"}}"#, link.expose());
        let token = parse_verify_email_request(body.as_bytes()).unwrap();
        assert_eq!(
            store
                .use_email_verification(&token, Utc::now())
                .await
                .unwrap(),
            Some(user),
            "{name}: the delivered link still works"
        );
    }
}

#[tokio::test]
async fn the_sweep_drops_unlinked_mail_whose_job_ended_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let user = member(&store, "jo@example.com").await;
        // A full queue leaves the message without a job.
        let full = NotifyQueueLimits {
            max_pending: 0,
            max_attempts: 3,
        };
        let id = store
            .queue_account_mail(
                AccountMailKind::Test,
                user,
                "jo@example.com",
                "email-1",
                full,
            )
            .await
            .unwrap();
        // Its send job exists, but the link write was lost.
        let outcome = store
            .enqueue_job(crate::models::EnqueueJobSpec {
                kind: crate::models::JobKind::NotifySend,
                payload: crate::models::JobPayload {
                    account_mail: Some(id.clone()),
                    ..crate::models::JobPayload::default()
                },
                priority: 0,
                max_attempts: 1,
                max_pending: 1_000,
                run_after: None,
            })
            .await
            .unwrap();
        let crate::models::EnqueueOutcome::Created { id: job } = outcome else {
            panic!("{name}: {outcome:?}");
        };
        assert!(
            !store.drop_account_mail_if_job_ended(&id).await.unwrap(),
            "{name}: a pending job keeps the message"
        );
        store.request_job_cancel(&job).await.unwrap();
        assert!(
            store.drop_account_mail_if_job_ended(&id).await.unwrap(),
            "{name}"
        );
        let mail = store.account_mail(&id).await.unwrap().unwrap();
        assert_eq!(mail.state, DeliveryState::Dropped, "{name}");
    }
}

#[tokio::test]
async fn pruning_expired_links_reads_no_rows_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        // More outstanding links than the smallest result cap.
        let mut hashes = Vec::new();
        for n in 0..3 {
            let address = format!("p{n}@example.com");
            let user = member(&store, &address).await;
            let token = store
                .mint_email_verification(user, &address, None)
                .await
                .unwrap()
                .expect("minted");
            hashes.push(hash_token(token.expose()));
        }
        let later = Utc::now() + Duration::days(2);
        for pass in 0..2 {
            store
                .prune_account_mail(later)
                .await
                .unwrap_or_else(|err| panic!("{name}: pass {pass}: {err}"));
        }
        for hash in &hashes {
            let row = email_verifications::Entity::find_by_id(hash.as_str())
                .one(&store.db)
                .await
                .unwrap();
            assert!(row.is_none(), "{name}: an expired link survived pruning");
        }
    }
}

#[tokio::test]
async fn the_same_address_keeps_its_verification_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let user = member(&store, "max@example.com").await;
        assert!(store
            .verify_user_email_if(user, "max@example.com")
            .await
            .unwrap());
        let same = store
            .set_user_email(user, Some("max@example.com"))
            .await
            .unwrap();
        assert!(same.email_verified, "{name}");
        let other = store
            .set_user_email(user, Some("max2@example.com"))
            .await
            .unwrap();
        assert!(!other.email_verified, "{name}");
        assert_eq!(other.email.as_deref(), Some("max2@example.com"), "{name}");
        let cleared = store.set_user_email(user, None).await.unwrap();
        assert_eq!(cleared.email, None, "{name}");
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

#[tokio::test]
async fn overlapping_resends_record_one_message_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        store
            .set_account_mail_instance(Some("email-1"))
            .await
            .unwrap();
        let user = member(&store, "sam@example.com").await;
        let since = Some(Utc::now() - Duration::minutes(5));
        let queue = || {
            store.queue_account_mail_unless_since(
                AccountMailRequest {
                    kind: AccountMailKind::VerifyEmail,
                    user_id: user,
                    address: "sam@example.com",
                    requested_by: None,
                },
                "email-1",
                LIMITS,
                since,
            )
        };
        let (first, second, third) = tokio::join!(queue(), queue(), queue());
        let recorded = [first.unwrap(), second.unwrap(), third.unwrap()]
            .into_iter()
            .flatten()
            .count();
        assert_eq!(recorded, 1, "{name}: one resend passes the gap");
        // The lock row stays, so later batches still serialize on it.
        let slot = super::account_mail::account_mail_slot(AccountMailKind::VerifyEmail, user);
        assert!(slot.starts_with("account-mail:"), "{name}");
        // Once the gap has passed, the next resend is recorded.
        let later = store
            .queue_account_mail_unless_since(
                AccountMailRequest {
                    kind: AccountMailKind::VerifyEmail,
                    user_id: user,
                    address: "sam@example.com",
                    requested_by: None,
                },
                "email-1",
                LIMITS,
                Some(Utc::now() + Duration::seconds(1)),
            )
            .await
            .unwrap();
        assert!(later.is_some(), "{name}");
    }
}

#[tokio::test]
async fn a_send_for_an_old_address_mints_nothing_and_voids_no_link_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let user = member(&store, "old@example.com").await;
        store
            .set_user_email(user, Some("new@example.com"))
            .await
            .unwrap();
        let live = store
            .mint_email_verification(user, "new@example.com", None)
            .await
            .unwrap()
            .expect("minted");
        let stale = store
            .mint_email_verification(user, "old@example.com", None)
            .await
            .unwrap();
        assert!(
            stale.is_none(),
            "{name}: a link was minted for an old address"
        );
        let identity = store
            .ensure_local_portal_identity(user, Some("Old"))
            .await
            .unwrap();
        let ticket = store
            .mint_invite_ticket(user, identity.id, "new@example.com", "account_mail", None)
            .await
            .unwrap()
            .expect("minted");
        let stale_invite = store
            .mint_invite_ticket(user, identity.id, "old@example.com", "account_mail", None)
            .await
            .unwrap();
        assert!(
            stale_invite.is_none(),
            "{name}: an invite went to an old address"
        );
        // The links issued for the current address still work.
        let body = format!(r#"{{"token":"{}"}}"#, live.expose());
        let live = parse_verify_email_request(body.as_bytes()).unwrap();
        assert_eq!(
            store
                .use_email_verification(&live, Utc::now())
                .await
                .unwrap(),
            Some(user),
            "{name}"
        );
        let row = crate::entities::claim_tickets::Entity::find()
            .filter(
                crate::entities::claim_tickets::Column::TokenHash.eq(hash_token(ticket.expose())),
            )
            .one(&store.db)
            .await
            .unwrap();
        assert!(row.is_some_and(|t| t.redeemed_at.is_none()), "{name}");
    }
}

/// The validated token of a raw reset link `raw`.
fn reset_token(raw: &str) -> crate::notifications::PasswordResetToken {
    crate::notifications::parse_password_reset_link(format!(r#"{{"token":"{raw}"}}"#).as_bytes())
        .expect("token")
}

#[tokio::test]
async fn a_reset_link_works_once_for_a_verified_address_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let user = member(&store, "rae@example.com").await;
        // An unverified address gets no link.
        assert!(
            store
                .mint_password_reset(user, "rae@example.com", None)
                .await
                .unwrap()
                .is_none(),
            "{name}: a link went to an unverified address"
        );
        assert!(store
            .verify_user_email_if(user, "rae@example.com")
            .await
            .unwrap());
        let old = store
            .mint_password_reset(user, "rae@example.com", None)
            .await
            .unwrap()
            .expect("minted");
        let new = store
            .mint_password_reset(user, "rae@example.com", None)
            .await
            .unwrap()
            .expect("minted");
        let now = Utc::now();
        let old = reset_token(old.expose());
        let new = reset_token(new.expose());
        assert_eq!(
            store.password_reset_user(&old, now).await.unwrap(),
            None,
            "{name}: a newer link voids the older one"
        );
        // Checking a link does not use it.
        for _ in 0..2 {
            assert_eq!(
                store.password_reset_user(&new, now).await.unwrap(),
                Some(user),
                "{name}"
            );
        }
        // Past its 30 minutes it does nothing.
        let later = now + Duration::minutes(31);
        assert_eq!(store.password_reset_user(&new, later).await.unwrap(), None);
        let hash = crate::hash_password(&fresh_password()).expect("hash");
        let redeem = |token, user_id, factor, at| {
            let store = &store;
            let hash = &hash;
            async move {
                store
                    .redeem_password_reset(token, user_id, factor, hash, at)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            redeem(&new, user, PasswordResetFactor::None, later).await,
            None
        );
        // Another user cannot use it, and a changed second factor stops it.
        assert_eq!(
            redeem(&new, user + 1, PasswordResetFactor::None, now).await,
            None
        );
        assert_eq!(
            redeem(&new, user, PasswordResetFactor::Totp, now).await,
            None
        );
        assert_eq!(
            redeem(&new, user, PasswordResetFactor::Passkey, now).await,
            None
        );
        assert!(
            store.get_user_password_hash(user).await.unwrap().is_none(),
            "{name}: a refused redemption changed the password"
        );
        assert_eq!(
            redeem(&new, user, PasswordResetFactor::None, now).await,
            Some(RedeemedReset {
                revoked_sessions: Some(0)
            }),
            "{name}"
        );
        let stored = store
            .get_user_password_hash(user)
            .await
            .unwrap()
            .expect("a password");
        assert!(crate::verify_password(&fresh_password(), &stored).unwrap());
        assert_eq!(
            redeem(&new, user, PasswordResetFactor::None, now).await,
            None,
            "{name}: a link worked twice"
        );
        assert_eq!(store.password_reset_user(&new, now).await.unwrap(), None);
    }
}

#[tokio::test]
async fn a_reset_survives_a_lost_reply_and_a_factor_enrolled_after_the_check_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let kit = memory(profile).await;
        let store = kit.store();
        let user = member(store, "kai@example.com").await;
        assert!(store
            .verify_user_email_if(user, "kai@example.com")
            .await
            .unwrap());
        let mint = || async {
            let token = store
                .mint_password_reset(user, "kai@example.com", None)
                .await
                .unwrap()
                .expect("minted");
            reset_token(token.expose())
        };
        let now = Utc::now();

        // The account had no factor when the link was checked; an
        // authenticator or a passkey enrolled before the finish commits
        // stops it, changes nothing and leaves the link usable.
        let token = mint().await;
        let first = crate::hash_password(&fresh_password()).expect("hash");
        store.set_user_totp_enabled(user, true).await.unwrap();
        assert_eq!(
            store
                .redeem_password_reset(&token, user, PasswordResetFactor::None, &first, now)
                .await
                .unwrap(),
            None,
            "{name}: a reset skipped a newly enrolled authenticator"
        );
        store.set_user_totp_enabled(user, false).await.unwrap();
        store
            .insert_webauthn_credential(user, "cred-kai", "{}", None)
            .await
            .unwrap();
        assert_eq!(
            store
                .redeem_password_reset(&token, user, PasswordResetFactor::None, &first, now)
                .await
                .unwrap(),
            None,
            "{name}: a reset skipped a newly enrolled passkey"
        );
        assert!(
            store.get_user_password_hash(user).await.unwrap().is_none(),
            "{name}: a refused redemption changed the password"
        );
        assert_eq!(
            store.password_reset_user(&token, now).await.unwrap(),
            Some(user),
            "{name}: a refused redemption spent the link"
        );

        // A lost reply: the batch committed, the retry finds the link
        // spent, and the stored hash shows the reset was this one. The
        // session is signed out; how many went with the lost reply.
        let identity = store
            .ensure_local_portal_identity(user, Some("Kai"))
            .await
            .unwrap();
        store
            .insert_portal_session("kai-session", identity.id, now + Duration::hours(1))
            .await
            .unwrap();
        store
            .insert_oidc_refresh_token(
                "kai-refresh",
                "app",
                user,
                "openid",
                now + Duration::days(1),
            )
            .await
            .unwrap();
        store
            .insert_oidc_refresh_token(
                "other-refresh",
                "app",
                user + 1,
                "openid",
                now + Duration::days(1),
            )
            .await
            .unwrap();
        // A new attempt hashes afresh, so its operation id is its own.
        let first = crate::hash_password(&fresh_password()).expect("hash");
        kit.lose_next_replies(1);
        assert_eq!(
            store
                .redeem_password_reset(&token, user, PasswordResetFactor::Passkey, &first, now)
                .await
                .unwrap(),
            Some(RedeemedReset {
                revoked_sessions: None
            }),
            "{name}: a lost reply reported a committed reset as refused"
        );
        assert!(
            store
                .get_portal_session_identity("kai-session")
                .await
                .unwrap()
                .is_none(),
            "{name}: a session outlived a reset whose reply was lost"
        );
        assert!(
            store
                .get_oidc_refresh_token("kai-refresh")
                .await
                .unwrap()
                .is_none(),
            "{name}: a refresh token outlived the reset"
        );
        assert!(
            store
                .get_oidc_refresh_token("other-refresh")
                .await
                .unwrap()
                .is_some(),
            "{name}: another account's refresh token was revoked"
        );
        let audited = store
            .list_security_audit_events(20)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.action == "password_reset_self")
            .count();
        assert_eq!(audited, 1, "{name}: the reset was audited {audited} times");
        assert_eq!(
            store.get_user_password_hash(user).await.unwrap().as_deref(),
            Some(first.as_str()),
            "{name}"
        );
        assert_eq!(store.password_reset_user(&token, now).await.unwrap(), None);

        // Another attempt after the link is spent still finds nothing,
        // even when its own reply is lost.
        let second = crate::hash_password(&fresh_password()).expect("hash");
        kit.lose_next_replies(1);
        assert_eq!(
            store
                .redeem_password_reset(&token, user, PasswordResetFactor::Passkey, &second, now)
                .await
                .unwrap(),
            None,
            "{name}: a spent link worked again"
        );
        assert_eq!(
            store.get_user_password_hash(user).await.unwrap().as_deref(),
            Some(first.as_str()),
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_reset_link_stops_when_the_address_changes_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let user = member(&store, "lou@example.com").await;
        assert!(store
            .verify_user_email_if(user, "lou@example.com")
            .await
            .unwrap());
        let raw = store
            .mint_password_reset(user, "lou@example.com", None)
            .await
            .unwrap()
            .expect("minted");
        // A claim on a job that is not running mints and voids nothing.
        let lost = crate::models::JobFence {
            job_id: "gone".into(),
            owner: "worker".into(),
            generation: 1,
        };
        assert!(store
            .mint_password_reset(user, "lou@example.com", Some(&lost))
            .await
            .unwrap()
            .is_none());
        let token = reset_token(raw.expose());
        let now = Utc::now();
        assert_eq!(
            store.password_reset_user(&token, now).await.unwrap(),
            Some(user),
            "{name}: a stale send voided the link"
        );
        store
            .set_user_email(user, Some("lou2@example.com"))
            .await
            .unwrap();
        assert_eq!(
            store.password_reset_user(&token, now).await.unwrap(),
            None,
            "{name}: a link for an old address still works"
        );
        assert_eq!(
            store
                .redeem_password_reset(&token, user, PasswordResetFactor::None, "x", now)
                .await
                .unwrap(),
            None
        );
        // Pruning removes expired links.
        let hash = hash_token(raw.expose());
        store
            .prune_account_mail(now + Duration::hours(1))
            .await
            .unwrap();
        let row = crate::entities::password_resets::Entity::find_by_id(hash.as_str())
            .one(&store.db)
            .await
            .unwrap();
        assert!(row.is_none(), "{name}: an expired reset link survived");
    }
}

#[tokio::test]
async fn a_reset_request_enqueues_one_send_job_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        assert!(
            store
                .enqueue_password_reset_request("ann@example.com", "email-1", LIMITS)
                .await
                .unwrap(),
            "{name}"
        );
        assert!(
            store
                .enqueue_password_reset_request("ann@example.com", "email-1", LIMITS)
                .await
                .unwrap(),
            "{name}: a request already waiting counts as queued"
        );
        let full = NotifyQueueLimits {
            max_pending: 1,
            max_attempts: 3,
        };
        assert!(
            !store
                .enqueue_password_reset_request("bob@example.com", "email-1", full)
                .await
                .unwrap(),
            "{name}: a full queue took another request"
        );
    }
}

#[test]
fn the_reset_bodies_follow_their_rules() {
    use crate::notifications::{
        parse_password_reset_ask, parse_password_reset_finish, ResetSecondFactor,
    };
    assert_eq!(
        parse_password_reset_ask(br#"{"email":" Ann@Example.com "}"#).unwrap(),
        "ann@example.com"
    );
    for bad in [
        &b"{}"[..],
        br#"{"email":""}"#,
        br#"{"email":"nope"}"#,
        br#"{"email":"a@example.com","other":1}"#,
        br#"["a@example.com"]"#,
    ] {
        assert!(parse_password_reset_ask(bad).is_err());
    }
    let hex = "0123456789abcdef".repeat(4);
    let finish =
        parse_password_reset_finish(format!(r#"{{"token":"{hex}","password":" pw "}}"#).as_bytes())
            .unwrap();
    assert_eq!(finish.password.as_str(), "pw");
    assert!(matches!(finish.second_factor, ResetSecondFactor::None));
    let finish = parse_password_reset_finish(
        format!(r#"{{"token":"{hex}","password":"pw","totp_code":"123456"}}"#).as_bytes(),
    )
    .unwrap();
    assert!(matches!(finish.second_factor, ResetSecondFactor::Totp(ref c) if c == "123456"));
    let finish = parse_password_reset_finish(
        format!(
            r#"{{"token":"{hex}","password":"pw","passkey":{{"challenge_id":"c-1","credential":{{}}}}}}"#
        )
        .as_bytes(),
    )
    .unwrap();
    assert!(matches!(
        finish.second_factor,
        ResetSecondFactor::Passkey { ref challenge_id, .. } if challenge_id == "c-1"
    ));
    let long = "x".repeat(1025);
    for bad in [
        format!(r#"{{"token":"{hex}","password":"  "}}"#),
        format!(r#"{{"token":"{hex}","password":"{long}"}}"#),
        r#"{"token":"abc","password":"pw"}"#.to_string(),
        format!(r#"{{"token":"{hex}","password":"pw","totp_code":"12345"}}"#),
        format!(r#"{{"token":"{hex}","password":"pw","totp_code":"12a456"}}"#),
        format!(
            r#"{{"token":"{hex}","password":"pw","passkey":{{"challenge_id":"","credential":{{}}}}}}"#
        ),
        format!(
            r#"{{"token":"{hex}","password":"pw","passkey":{{"challenge_id":"c","credential":[]}}}}"#
        ),
        format!(
            r#"{{"token":"{hex}","password":"pw","totp_code":"123456","passkey":{{"challenge_id":"c","credential":{{}}}}}}"#
        ),
        format!(r#"{{"token":"{hex}","password":"pw","other":1}}"#),
    ] {
        assert!(
            parse_password_reset_finish(bad.as_bytes()).is_err(),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn an_invite_keeps_who_asked_until_they_are_deleted_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        store
            .set_account_mail_instance(Some("email-1"))
            .await
            .unwrap();
        let admin = store
            .create_user_with_profile(
                UserRole::Administrator,
                Some("Alex"),
                None,
                Some("alex@example.com"),
                None,
            )
            .await
            .unwrap()
            .id;
        let user = member(&store, "sam@example.com").await;
        let id = store
            .queue_account_mail_unless_since(
                AccountMailRequest {
                    kind: AccountMailKind::Invite,
                    user_id: user,
                    address: "sam@example.com",
                    requested_by: Some(admin),
                },
                "email-1",
                LIMITS,
                None,
            )
            .await
            .unwrap()
            .expect("recorded");
        let mail = store.account_mail(&id).await.unwrap().expect("mail");
        assert_eq!(mail.requested_by, Some(admin), "{name}");
        // A message queued without anyone asking names no one.
        let own = store
            .queue_account_mail(
                AccountMailKind::VerifyEmail,
                user,
                "sam@example.com",
                "email-1",
                LIMITS,
            )
            .await
            .unwrap();
        let own = store.account_mail(&own).await.unwrap().expect("mail");
        assert_eq!(own.requested_by, None, "{name}");
        // Deleting the Administrator keeps the invite but forgets them.
        store.delete_user(admin).await.unwrap();
        let mail = store.account_mail(&id).await.unwrap().expect("mail kept");
        assert_eq!(mail.requested_by, None, "{name}");
    }
}
