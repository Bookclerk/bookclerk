//! The `core.ingress` document: activation, exposure and origin reports,
//! under every database profile.

use super::*;
use crate::test_support::{sqlite, Profile};

/// The operator, as the daemon writes on its behalf.
fn operator() -> ConfigActor {
    ConfigActor::Operator {
        id: "operator".into(),
    }
}

/// A report naming `origin`.
fn started(origin: &str) -> IngressStarted {
    IngressStarted {
        origin: origin.into(),
    }
}

#[test]
fn an_empty_document_is_ingress_off_exposing_branches() {
    let settings: IngressSettings = serde_json::from_str("{}").unwrap();
    assert_eq!(settings, IngressSettings::default());
    assert_eq!(settings.exposure, IngressExposure::Branches);
    assert_eq!(settings.active(), None);
    assert_eq!(settings.serving_origin(), None);
    assert_eq!(serde_json::to_string(&settings).unwrap(), r#"{"exposure":"branches"}"#);
    assert!(serde_json::from_str::<IngressSettings>(r#"{"fly":true}"#).is_err());
}

#[test]
fn a_hand_written_instance_id_that_does_not_parse_is_not_active() {
    let settings = IngressSettings {
        active_instance: Some("not-a-uuid".into()),
        origin: Some("https://relay.example.workers.dev".into()),
        ..IngressSettings::default()
    };
    assert_eq!(settings.active(), None);
    assert_eq!(settings.serving_origin(), None);
}

#[test]
fn exposure_spellings_match_the_wire() {
    for exposure in [IngressExposure::Branches, IngressExposure::Library] {
        assert_eq!(
            serde_json::to_string(&exposure).unwrap(),
            format!("\"{}\"", exposure.as_str())
        );
    }
}

#[test]
fn reports_adopt_first_then_wait_for_the_operator() {
    let instance = PluginInstanceId::mint();
    let other = PluginInstanceId::mint();
    let off = IngressSettings::default();
    let origin = |raw: &str| parse_ingress_origin(&started(raw)).unwrap();
    assert_eq!(report_into(&off, &instance, &origin("https://a.example")), None);

    let on = IngressSettings {
        active_instance: Some(instance.to_string()),
        ..IngressSettings::default()
    };
    assert_eq!(report_into(&on, &other, &origin("https://a.example")), None);
    let adopted = report_into(&on, &instance, &origin("https://a.example")).unwrap();
    assert_eq!(adopted.origin.as_deref(), Some("https://a.example"));
    assert_eq!(adopted.serving_origin(), Some("https://a.example"));

    let pending = report_into(&adopted, &instance, &origin("https://b.example")).unwrap();
    assert_eq!(pending.origin.as_deref(), Some("https://a.example"));
    assert_eq!(pending.reported_origin.as_deref(), Some("https://b.example"));
    assert!(matches!(
        classify(&pending, origin("https://b.example")),
        OriginReport::Pending { ref adopted, .. } if adopted == "https://a.example"
    ));

    // The same name again clears a stale pending report.
    let again = report_into(&pending, &instance, &origin("https://a.example")).unwrap();
    assert_eq!(again.reported_origin, None);
    assert!(matches!(
        classify(&again, origin("https://a.example")),
        OriginReport::Adopted(_)
    ));
}

#[tokio::test]
async fn the_document_follows_activation_reports_and_acceptance() {
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store();
        let name = profile.name();
        let actor = operator();
        assert_eq!(
            ingress_settings(store).await.unwrap(),
            IngressSettings::default(),
            "{name}"
        );

        let relay = PluginInstanceId::mint();
        let report = record_started(store, &actor, &relay, &started("https://a.example"))
            .await
            .unwrap();
        assert_eq!(report, OriginReport::NotActive, "{name}");

        activate_ingress(store, &actor, &relay).await.unwrap();
        let report = record_started(store, &actor, &relay, &started("https://a.example"))
            .await
            .unwrap();
        assert!(matches!(report, OriginReport::Adopted(_)), "{name}");
        let settings = ingress_settings(store).await.unwrap();
        assert_eq!(settings.serving_origin(), Some("https://a.example"), "{name}");

        let report = record_started(store, &actor, &relay, &started("https://b.example"))
            .await
            .unwrap();
        assert!(matches!(report, OriginReport::Pending { .. }), "{name}");
        let settings = ingress_settings(store).await.unwrap();
        assert_eq!(settings.serving_origin(), Some("https://a.example"), "{name}");
        assert_eq!(
            settings.reported_origin.as_deref(),
            Some("https://b.example"),
            "{name}"
        );

        accept_reported_origin(store, &actor).await.unwrap();
        let settings = ingress_settings(store).await.unwrap();
        assert_eq!(settings.serving_origin(), Some("https://b.example"), "{name}");
        assert_eq!(settings.reported_origin, None, "{name}");

        set_ingress_exposure(store, &actor, IngressExposure::Library)
            .await
            .unwrap();
        deactivate_ingress(store, &actor).await.unwrap();
        let settings = ingress_settings(store).await.unwrap();
        assert_eq!(settings.serving_origin(), None, "{name}");
        assert_eq!(settings.origin.as_deref(), Some("https://b.example"), "{name}");
        assert_eq!(settings.exposure, IngressExposure::Library, "{name}");
    }
}

#[tokio::test]
async fn a_bad_origin_is_refused_and_nothing_is_written() {
    let kit = sqlite::memory(Profile::FirstParty).await;
    let store = kit.store();
    let actor = operator();
    let relay = PluginInstanceId::mint();
    activate_ingress(store, &actor, &relay).await.unwrap();
    for bad in ["http://a.example", "https://127.0.0.1", "https://a.example/x"] {
        let err = record_started(store, &actor, &relay, &started(bad))
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Invalid(_)), "{bad}: {err}");
    }
    assert_eq!(ingress_settings(store).await.unwrap().origin, None);
}

#[tokio::test]
async fn members_and_administrators_may_not_write() {
    let kit = sqlite::memory(Profile::FirstParty).await;
    let store = kit.store();
    let relay = PluginInstanceId::mint();
    for actor in [
        ConfigActor::Administrator { id: "ada".into() },
        ConfigActor::Member { id: "sam".into() },
    ] {
        assert!(activate_ingress(store, &actor, &relay).await.is_err());
    }
    assert_eq!(ingress_settings(store).await.unwrap().active(), None);
}
