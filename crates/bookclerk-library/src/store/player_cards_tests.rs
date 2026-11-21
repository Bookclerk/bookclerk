//! Player cards and player passwords, under every database profile.

use super::*;
use crate::hash_token;
use crate::models::UserRole;
use crate::test_support::{sqlite::memory, Profile};

/// A new member and their local portal identity.
async fn patron(store: &LibraryStore, name: &str) -> (i64, i64) {
    let user = store
        .create_user(UserRole::Member, Some(name), None)
        .await
        .unwrap()
        .id;
    let identity = store
        .ensure_local_portal_identity(user, Some(name))
        .await
        .unwrap()
        .id;
    (user, identity)
}

/// Mints a card for `identity` whose tokens are the digests of
/// `<tag>-access` and `<tag>-refresh`.
async fn mint(
    store: &LibraryStore,
    identity: i64,
    tag: &str,
    password: Option<&str>,
) -> Option<i64> {
    let access = hash_token(&format!("{tag}-access"));
    let refresh = hash_token(&format!("{tag}-refresh"));
    let session = hash_token(&format!("{tag}-session"));
    store
        .mint_player_card(NewPlayerCard {
            identity_id: identity,
            session_hash: &session,
            password_id: password,
            label: "  Lissen on Pixel  ",
            user_agent: Some("okhttp/4.12"),
            device_type: "mobile",
            tokens: PlayerCardTokens {
                access_hash: &access,
                refresh_hash: &refresh,
            },
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn passwords_are_kept_by_digest_listed_and_removed() {
    for profile in Profile::ALL {
        let store = memory(profile).await.into_store();
        let (user, _) = patron(&store, "Riley").await;
        let (other, _) = patron(&store, "Sam").await;
        let first = store
            .insert_player_password(user, "  Kitchen tablet  ", &hash_token("one"))
            .await
            .unwrap()
            .expect("stored");
        assert_eq!(first.label, "Kitchen tablet", "{}", profile.name());
        store
            .insert_player_password(user, &"x".repeat(100), &hash_token("two"))
            .await
            .unwrap()
            .expect("stored");
        let listed = store.list_player_passwords(user).await.unwrap();
        assert_eq!(listed.len(), 2, "{}", profile.name());
        assert!(listed
            .iter()
            .all(|p| p.label.len() <= MAX_PLAYER_PASSWORD_LABEL_BYTES));
        assert!(store.list_player_passwords(other).await.unwrap().is_empty());

        let used = store
            .use_player_password(&hash_token("one"))
            .await
            .unwrap()
            .expect("found by digest");
        assert_eq!(used.user_id, user);
        assert!(used.last_used_at.is_some());
        assert!(store
            .use_player_password(&hash_token("nope"))
            .await
            .unwrap()
            .is_none());

        // Another patron cannot remove it.
        assert!(!store
            .delete_player_password(other, &first.id)
            .await
            .unwrap());
        assert!(store.delete_player_password(user, &first.id).await.unwrap());
        assert!(!store.delete_player_password(user, &first.id).await.unwrap());
        assert!(store
            .use_player_password(&hash_token("one"))
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn a_patron_keeps_at_most_the_cap_of_passwords() {
    let store = memory(Profile::ALL[0]).await.into_store();
    let (user, _) = patron(&store, "Riley").await;
    for n in 0..MAX_PLAYER_PASSWORDS {
        assert!(store
            .insert_player_password(user, "app", &hash_token(&format!("p{n}")))
            .await
            .unwrap()
            .is_some());
    }
    assert!(store
        .insert_player_password(user, "app", &hash_token("one too many"))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_card_signs_in_by_access_token_and_shows_as_a_player_session() {
    for profile in Profile::ALL {
        let store = memory(profile).await.into_store();
        let (user, identity) = patron(&store, "Riley").await;
        let session = mint(&store, identity, "a", None).await.expect("minted");
        let card = store
            .player_card_by_access(&hash_token("a-access"))
            .await
            .unwrap()
            .expect("live");
        assert_eq!(card.session_id, session, "{}", profile.name());
        assert_eq!(card.identity.user_id, Some(user));
        // The refresh token and the session's own hash are not access tokens.
        for other in ["a-refresh", "a-session"] {
            assert!(store
                .player_card_by_access(&hash_token(other))
                .await
                .unwrap()
                .is_none());
        }
        // Nor is the card a browser session or a clerk's card.
        assert!(store
            .get_portal_session_identity(&hash_token("a-session"))
            .await
            .unwrap()
            .is_none());
        let rows = store
            .list_portal_session_records_for_identity(identity)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, PortalSessionKind::Player);
        assert_eq!(rows[0].client_label.as_deref(), Some("Lissen on Pixel"));
    }
}

#[tokio::test]
async fn refreshing_rotates_the_pair_and_keeps_racing_tokens_briefly() {
    for profile in Profile::ALL {
        let store = memory(profile).await.into_store();
        let (_, identity) = patron(&store, "Riley").await;
        let session = mint(&store, identity, "a", None).await.expect("minted");
        let (b_access, b_refresh) = (hash_token("b-access"), hash_token("b-refresh"));
        let card = store
            .refresh_player_card(
                &hash_token("a-refresh"),
                PlayerCardTokens {
                    access_hash: &b_access,
                    refresh_hash: &b_refresh,
                },
            )
            .await
            .unwrap()
            .expect("refreshed");
        assert_eq!(card.session_id, session, "{}", profile.name());
        // Both access tokens work until they expire.
        for access in [&b_access, &hash_token("a-access")] {
            assert!(store.player_card_by_access(access).await.unwrap().is_some());
        }
        // A second refresh with the old token, racing the first, works too.
        let (c_access, c_refresh) = (hash_token("c-access"), hash_token("c-refresh"));
        assert!(store
            .refresh_player_card(
                &hash_token("a-refresh"),
                PlayerCardTokens {
                    access_hash: &c_access,
                    refresh_hash: &c_refresh,
                },
            )
            .await
            .unwrap()
            .is_some());
        // An access token is not a refresh token.
        let (d_access, d_refresh) = (hash_token("d-access"), hash_token("d-refresh"));
        assert!(store
            .refresh_player_card(
                &b_access,
                PlayerCardTokens {
                    access_hash: &d_access,
                    refresh_hash: &d_refresh,
                },
            )
            .await
            .unwrap()
            .is_none());
        // Every refresh token but the newest now has at most the grace period.
        let grace = Utc::now() + Duration::seconds(PLAYER_REFRESH_GRACE_SECS + 5);
        for old in ["a-refresh", "b-refresh"] {
            let row = player_card_tokens::Entity::find_by_id(hash_token(old))
                .one(&store.db)
                .await
                .unwrap()
                .expect("kept for the grace period");
            assert!(parse_dt(&row.expires_at) < grace, "{old}");
        }
        let newest = player_card_tokens::Entity::find_by_id(c_refresh.clone())
            .one(&store.db)
            .await
            .unwrap()
            .expect("stored");
        assert!(parse_dt(&newest.expires_at) > Utc::now() + Duration::days(29));
    }
}

#[tokio::test]
async fn signing_out_or_revoking_ends_the_card() {
    for profile in Profile::ALL {
        let store = memory(profile).await.into_store();
        let (_, identity) = patron(&store, "Riley").await;
        mint(&store, identity, "a", None).await.expect("minted");
        assert!(store
            .end_player_card(&hash_token("a-refresh"))
            .await
            .unwrap());
        assert!(store
            .player_card_by_access(&hash_token("a-access"))
            .await
            .unwrap()
            .is_none());
        assert!(!store
            .end_player_card(&hash_token("a-access"))
            .await
            .unwrap());

        // Revoked from the session list: the tokens stop working.
        let session = mint(&store, identity, "b", None).await.expect("minted");
        assert!(store
            .delete_portal_session_by_id_for_identity(session, identity)
            .await
            .unwrap());
        assert!(store
            .player_card_by_access(&hash_token("b-access"))
            .await
            .unwrap()
            .is_none());
        let (c_access, c_refresh) = (hash_token("c-access"), hash_token("c-refresh"));
        assert!(
            store
                .refresh_player_card(
                    &hash_token("b-refresh"),
                    PlayerCardTokens {
                        access_hash: &c_access,
                        refresh_hash: &c_refresh,
                    },
                )
                .await
                .unwrap()
                .is_none(),
            "{}",
            profile.name()
        );
    }
}

#[tokio::test]
async fn removing_a_password_ends_its_cards_only() {
    for profile in Profile::ALL {
        let store = memory(profile).await.into_store();
        let (user, identity) = patron(&store, "Riley").await;
        let password = store
            .insert_player_password(user, "Phone", &hash_token("pw"))
            .await
            .unwrap()
            .expect("stored");
        mint(&store, identity, "a", Some(&password.id))
            .await
            .expect("minted");
        mint(&store, identity, "b", None).await.expect("minted");
        assert!(store
            .delete_player_password(user, &password.id)
            .await
            .unwrap());
        assert!(
            store
                .player_card_by_access(&hash_token("a-access"))
                .await
                .unwrap()
                .is_none(),
            "{}",
            profile.name()
        );
        assert!(store
            .player_card_by_access(&hash_token("b-access"))
            .await
            .unwrap()
            .is_some());
        // A card is not made with a password that is gone.
        assert!(mint(&store, identity, "c", Some(&password.id))
            .await
            .is_none());
    }
}

#[tokio::test]
async fn deleting_a_patron_deletes_their_player_rows_only() {
    use sea_orm::{EntityTrait, PaginatorTrait};
    for profile in Profile::ALL {
        let store = memory(profile).await.into_store();
        let (user, identity) = patron(&store, "Riley").await;
        let (other, other_identity) = patron(&store, "Sam").await;
        let password = store
            .insert_player_password(user, "Phone", &hash_token("pw"))
            .await
            .unwrap()
            .expect("stored");
        store
            .insert_player_password(other, "Tablet", &hash_token("pw2"))
            .await
            .unwrap()
            .expect("stored");
        mint(&store, identity, "a", Some(&password.id))
            .await
            .expect("minted");
        mint(&store, other_identity, "b", None)
            .await
            .expect("minted");
        store.delete_user(user).await.unwrap();

        let name = profile.name();
        let db = store.connection();
        let cards = crate::entities::player_cards::Entity::find()
            .count(db)
            .await
            .unwrap();
        let tokens = crate::entities::player_card_tokens::Entity::find()
            .count(db)
            .await
            .unwrap();
        assert_eq!((cards, tokens), (1, 2), "{name}");
        assert!(store.list_player_passwords(user).await.unwrap().is_empty());
        assert_eq!(store.list_player_passwords(other).await.unwrap().len(), 1);
        assert!(store
            .player_card_by_access(&hash_token("b-access"))
            .await
            .unwrap()
            .is_some());
    }
}
