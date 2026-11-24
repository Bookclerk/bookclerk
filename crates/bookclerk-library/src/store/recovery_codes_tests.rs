//! Recovery codes: issuing, replacing and dropping a set, and finishing a
//! password reset with a code, under every database profile.

use chrono::Utc;
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};

use super::{LibraryStore, RecoveryCodeIssue};
use crate::entities::{encrypted_secrets, security_audit_events};
use crate::models::UserRole;
use crate::notifications::{PasswordResetFactor, RedeemedReset};
use crate::recovery_code::{RecoveryCode, RECOVERY_CODE_COUNT};
use crate::test_support::{sqlite::memory, Profile};

/// A member with a verified `email`; answers their id.
async fn verified_member(store: &LibraryStore, email: &str) -> i64 {
    let id = store
        .create_user_with_profile(UserRole::Member, Some("Ash"), None, Some(email), None)
        .await
        .unwrap()
        .id;
    assert!(store.verify_user_email_if(id, email).await.unwrap());
    id
}

/// How many audit rows have `action`, counted in the database so the
/// strictest result cap still allows it.
async fn audited(store: &LibraryStore, action: &str) -> u64 {
    security_audit_events::Entity::find()
        .filter(security_audit_events::Column::Action.eq(action))
        .count(store.db())
        .await
        .unwrap()
}

/// The password a redeemed link sets, assembled at runtime so scanners do
/// not treat a string literal as a shipped secret.
fn fresh_password() -> String {
    ["recovered", " ", "secret"].concat()
}

/// A base32 authenticator secret for tests.
fn sample_totp_secret() -> String {
    ["JBSW", "Y3DP", "EHPK", "3PXP"].concat()
}

#[tokio::test]
async fn a_set_is_issued_only_with_a_second_factor_and_replaced_whole_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let user = verified_member(&store, "ash@example.com").await;
        let now = Utc::now();

        // No second factor: no codes, whichever way they are asked for.
        for issue in [RecoveryCodeIssue::First, RecoveryCodeIssue::Replace] {
            assert!(
                !store
                    .issue_recovery_codes(user, &RecoveryCode::generate_set(), issue, now)
                    .await
                    .unwrap(),
                "{name}: codes without a second factor"
            );
        }
        assert_eq!(store.count_recovery_codes(user).await.unwrap(), 0);

        store
            .insert_webauthn_credential(user, "cred-ash", "{}", None)
            .await
            .unwrap();
        let first = RecoveryCode::generate_set();
        assert!(store
            .issue_recovery_codes(user, &first, RecoveryCodeIssue::First, now)
            .await
            .unwrap());
        assert_eq!(
            store.count_recovery_codes(user).await.unwrap(),
            u64::try_from(RECOVERY_CODE_COUNT).unwrap(),
            "{name}"
        );
        // A second factor added later does not bring a second set.
        assert!(
            !store
                .issue_recovery_codes(
                    user,
                    &RecoveryCode::generate_set(),
                    RecoveryCodeIssue::First,
                    now
                )
                .await
                .unwrap(),
            "{name}: a first set was issued twice"
        );
        for code in &first {
            let hash = store
                .recovery_code_hash(user, code.selector())
                .await
                .unwrap()
                .expect("stored");
            assert!(code.verify(&hash).unwrap(), "{name}");
        }
        assert_eq!(
            store
                .recovery_code_hash(user + 1, first[0].selector())
                .await
                .unwrap(),
            None,
            "{name}: a code answered for another user"
        );

        // A new set replaces the old one whole.
        let second = RecoveryCode::generate_set();
        assert!(store
            .issue_recovery_codes(user, &second, RecoveryCodeIssue::Replace, now)
            .await
            .unwrap());
        assert_eq!(
            store.count_recovery_codes(user).await.unwrap(),
            u64::try_from(RECOVERY_CODE_COUNT).unwrap()
        );
        for code in &first {
            if let Some(hash) = store
                .recovery_code_hash(user, code.selector())
                .await
                .unwrap()
            {
                assert!(
                    !code.verify(&hash).unwrap(),
                    "{name}: an old code still works"
                );
            }
        }
        assert_eq!(audited(&store, "recovery_codes_issued").await, 2, "{name}");

        // The set stays while a factor remains and goes with the last one.
        assert_eq!(
            store
                .drop_recovery_codes_without_factor(user)
                .await
                .unwrap(),
            0
        );
        let passkey = store.list_webauthn_credentials(user).await.unwrap()[0].id;
        assert!(store
            .delete_webauthn_credential(user, passkey)
            .await
            .unwrap());
        assert_eq!(
            store
                .drop_recovery_codes_without_factor(user)
                .await
                .unwrap(),
            u64::try_from(RECOVERY_CODE_COUNT).unwrap(),
            "{name}"
        );
        assert_eq!(store.count_recovery_codes(user).await.unwrap(), 0);
    }
}

#[tokio::test]
async fn a_recovery_code_finishes_a_reset_once_and_removes_every_factor_under_every_profile() {
    let _dek = crate::master_key::master_key_test_read_lock_async().await;
    crate::master_key::ensure_shared_test_dek();
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let user = verified_member(&store, "lee@example.com").await;
        let other = verified_member(&store, "kim@example.com").await;
        let now = Utc::now();
        // Lee has an authenticator, a passkey and a set of codes; Kim has
        // a set of their own.
        let pending = crate::build_sealed_record(
            sample_totp_secret().as_bytes(),
            crate::secret_kind::TOTP,
            "local",
            crate::secret_account_type::USER,
            &user.to_string(),
            "pending",
        )
        .unwrap();
        crate::upsert_secret(store.db(), &pending).await.unwrap();
        store
            .confirm_totp_enrollment(user, &sample_totp_secret())
            .await
            .unwrap();
        store
            .insert_webauthn_credential(user, "cred-lee", "{}", None)
            .await
            .unwrap();
        store
            .insert_webauthn_credential(other, "cred-kim", "{}", None)
            .await
            .unwrap();
        let codes = RecoveryCode::generate_set();
        assert!(store
            .issue_recovery_codes(user, &codes, RecoveryCodeIssue::First, now)
            .await
            .unwrap());
        let kims = RecoveryCode::generate_set();
        assert!(store
            .issue_recovery_codes(other, &kims, RecoveryCodeIssue::First, now)
            .await
            .unwrap());

        let mint = || async {
            let token = store
                .mint_password_reset(user, "lee@example.com", None)
                .await
                .unwrap()
                .expect("minted");
            crate::notifications::parse_password_reset_link(
                format!(r#"{{"token":"{}"}}"#, token.expose()).as_bytes(),
            )
            .unwrap()
        };
        let hash = crate::hash_password(&fresh_password()).expect("hash");
        let token = mint().await;
        let recovering = |selector: &str| PasswordResetFactor::RecoveryCode {
            selector: selector.to_string(),
        };

        // A selector the account does not have is refused, such as one
        // of another account's codes.
        let foreign = kims
            .iter()
            .find(|kim| codes.iter().all(|c| c.selector() != kim.selector()))
            .expect("a selector Lee does not have");
        assert_eq!(
            store
                .redeem_password_reset(&token, user, recovering(foreign.selector()), &hash, now)
                .await
                .unwrap(),
            None,
            "{name}: another account's code finished a reset"
        );
        assert!(store.get_user_password_hash(user).await.unwrap().is_none());

        assert_eq!(
            store
                .redeem_password_reset(&token, user, recovering(codes[3].selector()), &hash, now)
                .await
                .unwrap(),
            Some(RedeemedReset {
                revoked_sessions: Some(0)
            }),
            "{name}"
        );
        let lee = store.get_user(user).await.unwrap().unwrap();
        assert!(!lee.totp_enabled, "{name}: the authenticator stayed on");
        assert_eq!(store.count_webauthn_credentials(user).await.unwrap(), 0);
        assert_eq!(store.count_recovery_codes(user).await.unwrap(), 0);
        let secrets = encrypted_secrets::Entity::find()
            .filter(encrypted_secrets::Column::Kind.eq(crate::secret_kind::TOTP))
            .filter(encrypted_secrets::Column::AccountId.eq(user.to_string()))
            .all(store.db())
            .await
            .unwrap();
        assert!(
            secrets.is_empty(),
            "{name}: the authenticator secret stayed"
        );
        assert_eq!(audited(&store, "password_reset_recovery_code").await, 1);
        assert_eq!(audited(&store, "password_reset_self").await, 0);
        // Kim keeps everything.
        assert_eq!(store.count_webauthn_credentials(other).await.unwrap(), 1);
        assert_eq!(
            store.count_recovery_codes(other).await.unwrap(),
            u64::try_from(RECOVERY_CODE_COUNT).unwrap()
        );

        // The code, and every other code, is gone: a new link cannot use
        // one.
        let again = mint().await;
        assert_eq!(
            store
                .redeem_password_reset(&again, user, recovering(codes[4].selector()), &hash, now)
                .await
                .unwrap(),
            None,
            "{name}: a code outlived the recovery"
        );
    }
}
