//! Member controls: the library default, per-patron overrides and who
//! they apply to, under every database profile.

use super::*;
use crate::control_plane::verify_patron_administrator;
use crate::models::UserRole;
use crate::test_support::{sqlite, Profile};

#[test]
fn an_empty_document_allows_everything() {
    let controls: MemberControls = serde_json::from_str("{}").unwrap();
    assert_eq!(controls, MemberControls::all(true));
    let overrides: ControlOverrides = serde_json::from_str("{}").unwrap();
    assert!(overrides.is_empty());
    assert_eq!(serde_json::to_string(&overrides).unwrap(), "{}");
}

#[test]
fn unknown_fields_are_refused() {
    assert!(serde_json::from_str::<MemberControls>(r#"{"fly":false}"#).is_err());
    assert!(serde_json::from_str::<ControlOverrides>(r#"{"fly":false}"#).is_err());
}

#[test]
fn wire_names_round_trip() {
    for control in MemberControl::ALL {
        assert_eq!(MemberControl::parse(control.as_str()), Some(control));
        assert_eq!(
            serde_json::to_string(&control).unwrap(),
            format!("\"{}\"", control.as_str())
        );
    }
    assert_eq!(MemberControl::parse("fly"), None);
}

#[test]
fn an_override_replaces_only_its_own_control() {
    let mut defaults = MemberControls::all(true);
    defaults.see_prices = false;
    let mut overrides = ControlOverrides::default();
    overrides.set(MemberControl::LinkAccounts, Some(false));
    overrides.set(MemberControl::SeePrices, Some(true));
    let effective = overrides.apply(defaults);
    for control in MemberControl::ALL {
        let expected = !matches!(control, MemberControl::LinkAccounts);
        assert_eq!(effective.allows(control), expected, "{control:?}");
    }
    overrides.set(MemberControl::LinkAccounts, None);
    overrides.set(MemberControl::SeePrices, None);
    assert!(overrides.is_empty());
    assert_eq!(overrides.apply(defaults), defaults);
}

#[tokio::test]
async fn members_follow_the_default_except_where_overridden() {
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let sam = store
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap();
        let admin = store
            .create_user(UserRole::Administrator, Some("Ada"), None)
            .await
            .unwrap();
        for control in MemberControl::ALL {
            assert!(user_may(store, sam.id, control).await.unwrap(), "{name}");
        }

        // The default turns downloads off for every Member.
        let actor = verify_patron_administrator(store, admin.id).await.unwrap();
        let mut defaults = MemberControls::all(true);
        defaults.download = false;
        let stored = set_default_member_controls(store, &actor, defaults)
            .await
            .unwrap();
        assert_eq!(stored.body, defaults, "{name}");
        assert!(
            !user_may(store, sam.id, MemberControl::Download)
                .await
                .unwrap(),
            "{name}"
        );
        // The default is for patrons: it never limits an Administrator.
        assert!(
            user_may(store, admin.id, MemberControl::Download)
                .await
                .unwrap(),
            "{name}"
        );
        // A control set for an Administrator alone does.
        let mut admin_overrides = ControlOverrides::default();
        admin_overrides.set(MemberControl::LinkAccounts, Some(false));
        let limited = set_member_control_overrides(store, admin.id, admin_overrides)
            .await
            .unwrap();
        assert!(!limited.effective.link_accounts, "{name}");
        assert!(limited.effective.download, "{name}: not the default");
        assert!(
            !user_may(store, admin.id, MemberControl::LinkAccounts)
                .await
                .unwrap(),
            "{name}"
        );
        // An Owner never is.
        let owner = store
            .create_user(UserRole::Owner, Some("Olu"), None)
            .await
            .unwrap();
        set_member_control_overrides(store, owner.id, admin_overrides)
            .await
            .unwrap();
        assert!(
            user_may(store, owner.id, MemberControl::LinkAccounts)
                .await
                .unwrap(),
            "{name}"
        );

        // Sam gets downloads back and loses account linking.
        let overrides = ControlOverrides {
            download: Some(true),
            link_accounts: Some(false),
            ..ControlOverrides::default()
        };
        let assigned = set_member_control_overrides(store, sam.id, overrides)
            .await
            .unwrap();
        assert_eq!(assigned.overrides, overrides, "{name}");
        assert!(assigned.effective.download, "{name}");
        assert!(!assigned.effective.link_accounts, "{name}");
        assert!(assigned.effective.see_prices, "{name}");

        // A later change of the default reaches the controls Sam does
        // not override.
        defaults.see_prices = false;
        set_default_member_controls(store, &actor, defaults)
            .await
            .unwrap();
        assert!(
            !user_may(store, sam.id, MemberControl::SeePrices)
                .await
                .unwrap(),
            "{name}"
        );
        assert!(
            user_may(store, sam.id, MemberControl::Download)
                .await
                .unwrap(),
            "{name}"
        );

        // Clearing the overrides removes the row.
        let assigned = set_member_control_overrides(store, sam.id, ControlOverrides::default())
            .await
            .unwrap();
        assert!(assigned.overrides.is_empty(), "{name}");
        assert!(
            patron_controls::Entity::find_by_id(sam.id)
                .one(store.db())
                .await
                .unwrap()
                .is_none(),
            "{name}"
        );
        assert!(!assigned.effective.download, "{name}");

        // A disabled user and an unknown one may do nothing.
        store
            .set_user_status(sam.id, UserStatus::Disabled)
            .await
            .unwrap();
        assert!(
            !user_may(store, sam.id, MemberControl::RequestBooks)
                .await
                .unwrap(),
            "{name}"
        );
        assert!(
            !user_may(store, 9_999, MemberControl::RequestBooks)
                .await
                .unwrap(),
            "{name}"
        );
        assert!(matches!(
            set_member_control_overrides(store, 9_999, overrides).await,
            Err(LibraryError::NotFound(_))
        ));
        assert_eq!(kit.refused_begins(), 0, "{name}");
    }
}

#[tokio::test]
async fn only_administrators_write_the_default() {
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let sam = store
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap();
        assert!(verify_patron_administrator(store, sam.id).await.is_err());
        let member = ConfigActor::Member {
            id: format!("user:{}", sam.id),
        };
        assert!(
            set_default_member_controls(store, &member, MemberControls::all(false))
                .await
                .is_err(),
            "{}",
            profile.name()
        );
        assert_eq!(
            default_member_controls(store).await.unwrap(),
            MemberControls::all(true)
        );
    }
}

#[tokio::test]
async fn a_guarded_write_stops_once_the_controls_change() {
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let sam = store
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap();
        let admin = store
            .create_user(UserRole::Administrator, Some("Ada"), None)
            .await
            .unwrap();
        let actor = verify_patron_administrator(store, admin.id).await.unwrap();
        // Renames Sam under `guard`; true when the write applied.
        let rename = |guard: ControlGuard, to: &'static str| async move {
            let mut values = vec![text(to), int(sam.id)];
            values.extend(guard.values());
            store
                .execute_host_batch(batch::request(
                    &format!("guard-test-{}", uuid::Uuid::new_v4().simple()),
                    vec![batch::exec(
                        &format!(
                            "UPDATE users SET display_name = ? WHERE id = ? AND {}",
                            ControlGuard::SQL
                        ),
                        values,
                    )],
                ))
                .await
                .unwrap();
            store
                .get_user(sam.id)
                .await
                .unwrap()
                .unwrap()
                .display_name
                .as_deref()
                == Some(to)
        };

        let guard = control_guard(store, sam.id, MemberControl::Upload)
            .await
            .unwrap()
            .unwrap();
        assert!(rename(guard, "Sam A").await, "{name}");

        // An unrelated default change after the read still stops the write.
        let guard = control_guard(store, sam.id, MemberControl::Upload)
            .await
            .unwrap()
            .unwrap();
        let mut defaults = MemberControls::all(true);
        defaults.see_prices = false;
        set_default_member_controls(store, &actor, defaults)
            .await
            .unwrap();
        assert!(!rename(guard, "Sam B").await, "{name}");

        // So does an override written after the read.
        let guard = control_guard(store, sam.id, MemberControl::Upload)
            .await
            .unwrap()
            .unwrap();
        let mut overrides = ControlOverrides::default();
        overrides.set(MemberControl::Upload, Some(false));
        set_member_control_overrides(store, sam.id, overrides)
            .await
            .unwrap();
        assert!(!rename(guard, "Sam C").await, "{name}");
        assert!(
            control_guard(store, sam.id, MemberControl::Upload)
                .await
                .unwrap()
                .is_none(),
            "{name}"
        );

        // And a change of role.
        let guard = control_guard(store, sam.id, MemberControl::Download)
            .await
            .unwrap()
            .unwrap();
        store
            .set_user_role(sam.id, UserRole::Administrator)
            .await
            .unwrap();
        assert!(!rename(guard, "Sam D").await, "{name}");
        assert_eq!(kit.refused_begins(), 0, "{name}");
    }
}

#[test]
fn every_control_sets_and_reads_back() {
    for control in MemberControl::ALL {
        let mut controls = MemberControls::all(true);
        controls.set(control, false);
        for other in MemberControl::ALL {
            assert_eq!(controls.allows(other), other != control, "{control:?}");
        }
        let mut overrides = ControlOverrides::default();
        overrides.set(control, Some(false));
        for other in MemberControl::ALL {
            let expected = (other == control).then_some(false);
            assert_eq!(overrides.get(other), expected, "{control:?}");
        }
    }
}

#[test]
fn a_stored_default_of_another_schema_or_bad_json_is_refused() {
    let stored = |schema_version: i64, document_json: &str| StoredDocument {
        key: DocumentKey::cluster(MEMBER_CONTROLS_NAMESPACE),
        schema_version,
        revision: 1,
        document_json: document_json.into(),
        updated_at: String::new(),
        updated_by: String::new(),
        write_operation_id: String::new(),
    };
    assert!(parse_default(stored(MEMBER_CONTROLS_SCHEMA_VERSION + 1, "{}")).is_err());
    let err = parse_default(stored(MEMBER_CONTROLS_SCHEMA_VERSION, "[")).unwrap_err();
    assert!(err.to_string().contains("invalid member controls"), "{err}");
    assert!(parse_default(stored(MEMBER_CONTROLS_SCHEMA_VERSION, "{}")).is_ok());
}

#[tokio::test]
async fn an_unchanged_default_keeps_its_revision_and_unknown_users_may_not() {
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let actor = ConfigActor::Operator { id: "test".into() };
        let mut controls = MemberControls::all(true);
        controls.upload = false;
        let first = set_default_member_controls(store, &actor, controls)
            .await
            .unwrap();
        let again = set_default_member_controls(store, &actor, controls)
            .await
            .unwrap();
        assert_eq!(first.revision, again.revision, "{}", profile.name());
        let guard = control_guard(store, 999_999, MemberControl::Download)
            .await
            .unwrap();
        assert!(guard.is_none(), "{}", profile.name());
    }
}

#[tokio::test]
async fn a_promotion_after_authorization_stops_the_override_write() {
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let sam = store
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap();
        let overrides = ControlOverrides {
            download: Some(false),
            ..ControlOverrides::default()
        };
        // Read as a Member, then promoted before the write.
        store
            .set_user_role(sam.id, UserRole::Administrator)
            .await
            .unwrap();
        let err = set_member_control_overrides_for(store, &sam, overrides)
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Conflict(_)), "{name}: {err:?}");
        assert!(
            member_control_overrides(store, sam.id)
                .await
                .unwrap()
                .is_empty(),
            "{name}"
        );
        // Read fresh, the write goes through.
        let fresh = store.get_user(sam.id).await.unwrap().unwrap();
        let assigned = set_member_control_overrides_for(store, &fresh, overrides)
            .await
            .unwrap();
        assert_eq!(assigned.overrides, overrides, "{name}");
        let cleared = set_member_control_overrides_for(store, &fresh, ControlOverrides::default())
            .await
            .unwrap();
        assert!(cleared.overrides.is_empty(), "{name}");
        // Stored overrides that already match the request still refuse a
        // write authorized against the old role.
        let pat = store
            .create_user(UserRole::Member, Some("Pat"), None)
            .await
            .unwrap();
        set_member_control_overrides_for(store, &pat, overrides)
            .await
            .unwrap();
        store
            .set_user_role(pat.id, UserRole::Administrator)
            .await
            .unwrap();
        let err = set_member_control_overrides_for(store, &pat, overrides)
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Conflict(_)), "{name}: {err:?}");
        // Clearing under a stale role is refused too, and keeps the row.
        let err = set_member_control_overrides_for(store, &pat, ControlOverrides::default())
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Conflict(_)), "{name}: {err:?}");
        assert_eq!(
            member_control_overrides(store, pat.id).await.unwrap(),
            overrides,
            "{name}"
        );
        // Back to the role the caller read as the batch runs: the guard
        // holds, so the write goes through.
        store.set_user_role(pat.id, UserRole::Member).await.unwrap();
        let cleared = set_member_control_overrides_for(store, &pat, ControlOverrides::default())
            .await
            .unwrap();
        assert!(cleared.overrides.is_empty(), "{name}");
        // A role change after the write leaves it in place.
        set_member_control_overrides_for(store, &pat, overrides)
            .await
            .unwrap();
        store
            .set_user_role(pat.id, UserRole::Administrator)
            .await
            .unwrap();
        assert_eq!(
            member_control_overrides(store, pat.id).await.unwrap(),
            overrides,
            "{name}"
        );
    }
}
