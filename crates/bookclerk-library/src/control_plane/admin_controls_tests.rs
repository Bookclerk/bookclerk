//! Administrator switches: the library default, per-Administrator
//! overrides and who they apply to, under every database profile.

use super::*;
use crate::control_plane::{verify_library_owner, verify_patron_administrator};
use crate::models::UserRole;
use crate::test_support::{sqlite, Profile};

#[test]
fn an_empty_document_allows_everything() {
    let controls: AdminControls = serde_json::from_str("{}").unwrap();
    assert_eq!(controls, AdminControls::all(true));
    let overrides: AdminControlOverrides = serde_json::from_str("{}").unwrap();
    assert!(overrides.is_empty());
    assert_eq!(serde_json::to_string(&overrides).unwrap(), "{}");
}

#[test]
fn unknown_fields_are_refused() {
    assert!(serde_json::from_str::<AdminControls>(r#"{"fly":false}"#).is_err());
    assert!(serde_json::from_str::<AdminControlOverrides>(r#"{"fly":false}"#).is_err());
}

#[test]
fn wire_names_round_trip() {
    for control in AdminControl::ALL {
        assert_eq!(AdminControl::parse(control.as_str()), Some(control));
        assert_eq!(
            serde_json::to_string(&control).unwrap(),
            format!("\"{}\"", control.as_str())
        );
    }
    assert_eq!(AdminControl::parse("fly"), None);
}

#[test]
fn an_override_replaces_only_its_own_switch() {
    let mut defaults = AdminControls::all(true);
    defaults.run_jobs = false;
    let mut overrides = AdminControlOverrides::default();
    overrides.set(AdminControl::LibraryScan, Some(false));
    overrides.set(AdminControl::RunJobs, Some(true));
    let effective = overrides.apply(defaults);
    for control in AdminControl::ALL {
        let expected = !matches!(control, AdminControl::LibraryScan);
        assert_eq!(effective.allows(control), expected, "{control:?}");
    }
    overrides.set(AdminControl::LibraryScan, None);
    overrides.set(AdminControl::RunJobs, None);
    assert!(overrides.is_empty());
    assert_eq!(overrides.apply(defaults), defaults);
}

#[test]
fn every_switch_sets_and_clears_on_its_own() {
    for control in AdminControl::ALL {
        let mut controls = AdminControls::all(true);
        controls.set(control, false);
        for other in AdminControl::ALL {
            assert_eq!(controls.allows(other), other != control, "{control:?}");
        }
        let mut overrides = AdminControlOverrides::default();
        overrides.set(control, Some(false));
        assert_eq!(overrides.get(control), Some(false));
        assert!(!overrides.apply(AdminControls::all(true)).allows(control));
        overrides.set(control, None);
        assert!(overrides.is_empty());
    }
}

#[tokio::test]
async fn overrides_for_an_unknown_user_are_refused() {
    let kit = sqlite::memory(Profile::ALL[0]).await;
    let store = kit.store();
    let mut overrides = AdminControlOverrides::default();
    overrides.set(AdminControl::RunJobs, Some(false));
    let err = set_admin_control_overrides(store, 987_654, overrides)
        .await
        .unwrap_err();
    assert!(matches!(err, LibraryError::NotFound(_)), "{err}");
}

#[tokio::test]
async fn administrators_follow_the_default_except_where_overridden() {
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let owner = store
            .create_user(UserRole::Owner, Some("Olu"), None)
            .await
            .unwrap();
        let ada = store
            .create_user(UserRole::Administrator, Some("Ada"), None)
            .await
            .unwrap();
        let sam = store
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap();
        for control in AdminControl::ALL {
            assert!(admin_may(store, ada.id, control).await.unwrap(), "{name}");
            assert!(admin_may(store, owner.id, control).await.unwrap(), "{name}");
            assert!(!admin_may(store, sam.id, control).await.unwrap(), "{name}");
        }
        assert!(!admin_may(store, 9_999, AdminControl::RunJobs)
            .await
            .unwrap());
        assert!(
            !administrator_switch_allows(store, 9_999, AdminControl::InviteBranches)
                .await
                .unwrap()
        );

        // The default turns the job queue off for every Administrator, but
        // never for an Owner.
        let actor = verify_library_owner(store, owner.id).await.unwrap();
        let mut defaults = AdminControls::all(true);
        defaults.run_jobs = false;
        let stored = set_default_admin_controls(store, &actor, defaults)
            .await
            .unwrap();
        assert_eq!(stored.body, defaults, "{name}");
        assert!(!admin_may(store, ada.id, AdminControl::RunJobs)
            .await
            .unwrap());
        assert!(admin_may(store, owner.id, AdminControl::RunJobs)
            .await
            .unwrap());

        // One Administrator gets the queue back and loses library-wide scans.
        let mut overrides = AdminControlOverrides::default();
        overrides.set(AdminControl::RunJobs, Some(true));
        overrides.set(AdminControl::LibraryScan, Some(false));
        let assigned = set_admin_control_overrides(store, ada.id, overrides)
            .await
            .unwrap();
        assert_eq!(assigned.overrides, overrides, "{name}");
        assert!(assigned.effective.run_jobs, "{name}");
        assert!(!assigned.effective.library_scan, "{name}");
        assert_eq!(
            admin_control_overrides(store, ada.id).await.unwrap(),
            overrides
        );

        // Clearing every override removes the row and follows the default.
        let cleared = set_admin_control_overrides(store, ada.id, AdminControlOverrides::default())
            .await
            .unwrap();
        assert!(cleared.overrides.is_empty(), "{name}");
        // Members are judged by their own rules, not an Administrator's
        // switch; the Administrator follows theirs.
        let mut no_hosts = defaults;
        no_hosts.invite_branches = false;
        set_default_admin_controls(store, &actor, no_hosts)
            .await
            .unwrap();
        assert!(
            administrator_switch_allows(store, sam.id, AdminControl::InviteBranches)
                .await
                .unwrap()
        );
        assert!(
            !administrator_switch_allows(store, ada.id, AdminControl::InviteBranches)
                .await
                .unwrap()
        );
        set_default_admin_controls(store, &actor, defaults)
            .await
            .unwrap();
        assert_eq!(cleared.effective, defaults, "{name}");

        // A disabled Administrator may do none of it.
        store
            .set_user_status(ada.id, crate::models::UserStatus::Disabled)
            .await
            .unwrap();
        assert!(!admin_may(store, ada.id, AdminControl::ManageMembers)
            .await
            .unwrap());
    }
}

#[tokio::test]
async fn only_an_owner_or_the_operator_writes_the_default() {
    let kit = sqlite::memory(Profile::FirstParty).await;
    let store = kit.store();
    let ada = store
        .create_user(UserRole::Administrator, Some("Ada"), None)
        .await
        .unwrap();
    assert!(verify_library_owner(store, ada.id).await.is_err());
    // The patron-defaults actor an Administrator holds cannot write it.
    let patron_admin = verify_patron_administrator(store, ada.id).await.unwrap();
    assert!(
        set_default_admin_controls(store, &patron_admin, AdminControls::all(false))
            .await
            .is_err()
    );
    let operator = ConfigActor::Operator {
        id: "operator".into(),
    };
    let stored = set_default_admin_controls(store, &operator, AdminControls::all(false))
        .await
        .unwrap();
    assert_eq!(stored.body, AdminControls::all(false));
    assert_eq!(
        default_admin_controls(store).await.unwrap(),
        AdminControls::all(false)
    );
}
