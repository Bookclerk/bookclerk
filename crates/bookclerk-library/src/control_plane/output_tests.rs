//! `core.output` import precedence, compare-and-swap, and shared-database tests.

use std::path::Path;

use bookclerk_config::{Config, MultiDestinationMode, NamingProfile, OutputConfig, OutputFormat};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
use tempfile::tempdir;

use super::tests::{
    bootstrap_pair, file_store, memory_store, operator, postgres_stores, postgres_tests_enabled,
};
use super::*;
use crate::entities::configuration_documents;
use crate::master_key::{master_key_path, master_key_test_lock_async};
use crate::store::LibraryStore;

/// A seed config whose `[output]` differs from the defaults in two fields.
fn output_seed(widevine: bool, profile: NamingProfile) -> Config {
    Config {
        output: OutputConfig {
            widevine,
            naming_profile: profile,
            ..OutputConfig::default()
        },
        ..Config::default()
    }
}

#[tokio::test]
async fn output_is_imported_once_and_later_seeds_are_ignored() {
    let _guard = master_key_test_lock_async().await;
    let store = memory_store().await;
    let files = tempdir().unwrap();
    let scratch = files.path().join("cache");
    let seed = output_seed(true, NamingProfile::Classic);
    let session = bootstrap_control_plane(&store, files.path(), &scratch, None, &seed)
        .await
        .unwrap();
    assert_eq!(session.output.namespace, OUTPUT_NAMESPACE);
    assert_eq!(session.output.scope_type, CLUSTER_SCOPE_TYPE);
    assert_eq!(session.output.revision, 1);
    assert_eq!(session.output.schema_version, OUTPUT_SCHEMA_VERSION);
    assert_eq!(session.output.updated_by, "bootstrap");
    assert!(session.output.body.widevine);
    assert_eq!(session.output.body.naming_profile, NamingProfile::Classic);

    // A later TOML/environment value, even an invalid one, neither replaces
    // nor fails the stored document.
    let mut later = output_seed(false, NamingProfile::Audiobookshelf);
    later.output.cover_size = "not-a-size".into();
    let again = bootstrap_control_plane(&store, files.path(), &scratch, None, &later)
        .await
        .expect("stored output is authoritative");
    assert_eq!(again.output, session.output);
    assert_eq!(audit_count(&store, OUTPUT_NAMESPACE).await.unwrap(), 1);
    assert_eq!(change_count(&store, OUTPUT_NAMESPACE).await.unwrap(), 1);

    // Overlay replaces the TOML values in memory and keeps host-local scratch.
    let mut config = later.clone();
    config.output.in_progress = Some("/host/scratch".into());
    overlay_core_documents(&mut config, &again);
    assert!(config.output.widevine);
    assert_eq!(config.output.naming_profile, NamingProfile::Classic);
    assert_eq!(config.output.cover_size, "500");
    assert_eq!(
        config.output.in_progress.as_deref(),
        Some(Path::new("/host/scratch"))
    );
    assert_eq!(config.output_revision, Some(1));
    assert_eq!(
        config.output_authority.as_deref(),
        Some(session.cluster_id.as_str())
    );
}

#[test]
fn output_seed_cover_size_is_case_insensitive() {
    let seed = OutputConfig {
        cover_size: " Native ".into(),
        ..OutputConfig::default()
    };
    let settings = OutputSettingsV1::from_config(&seed).expect("`Native` is a valid cover size");
    assert_eq!(settings.cover_size, "native");
}

#[tokio::test]
async fn output_first_import_defaults_an_out_of_range_seed_field() {
    let _guard = master_key_test_lock_async().await;
    let store = memory_store().await;
    let stored = import_output_if_absent(
        &store,
        &ConfigActor::Bootstrap,
        &OutputConfig {
            split_mp3_max_mb: 0,
            widevine: true,
            ..OutputConfig::default()
        },
        "legacy-output-seed",
    )
    .await
    .expect("an out-of-range seed field does not stop the import");
    let defaults = OutputConfig::default();
    assert_eq!(stored.body.split_mp3_max_mb, defaults.split_mp3_max_mb);
    assert!(stored.body.widevine, "valid seed values are imported");
    assert_eq!(load_output(&store).await.unwrap().body, stored.body);
}

#[tokio::test]
async fn output_writes_are_validated_authorized_and_versioned() {
    let _guard = master_key_test_lock_async().await;
    let store = memory_store().await;
    let files = tempdir().unwrap();
    let session = bootstrap_pair(&store, files.path()).await;
    let revision = session.output.revision;

    let mut invalid = session.output.body.clone();
    invalid.chapter_layout = "spiral".into();
    let err = replace_output(&store, &operator(), revision, &invalid, "bad-output")
        .await
        .expect_err("invalid body");
    assert!(err.to_string().contains("chapter_layout"), "{err}");

    let mut body = session.output.body.clone();
    body.set("output.format", "single_mp3").unwrap();
    for actor in [
        ConfigActor::Administrator { id: "admin".into() },
        ConfigActor::Member { id: "m".into() },
    ] {
        let denied = replace_output(&store, &actor, revision, &body, "denied-output")
            .await
            .expect_err("not operator");
        assert!(
            denied
                .to_string()
                .contains("unauthorized configuration write"),
            "{denied}"
        );
    }
    assert_eq!(audit_count(&store, OUTPUT_NAMESPACE).await.unwrap(), 1);

    let applied = replace_output(&store, &operator(), revision, &body, "output-mp3")
        .await
        .unwrap();
    let OutputReplace::Applied(doc) = applied else {
        panic!("{applied:?}");
    };
    assert_eq!(doc.revision, revision + 1);
    assert_eq!(doc.updated_by, "operator");
    assert_eq!(doc.body.format, OutputFormat::SingleMp3);
    let replay = replace_output(&store, &operator(), revision, &body, "output-mp3")
        .await
        .unwrap();
    assert_eq!(
        replay,
        OutputReplace::Replayed {
            revision: doc.revision
        }
    );
    assert_eq!(audit_count(&store, OUTPUT_NAMESPACE).await.unwrap(), 2);
    assert_eq!(change_count(&store, OUTPUT_NAMESPACE).await.unwrap(), 2);

    let row = configuration_documents::Entity::find()
        .all(store.db())
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.namespace == OUTPUT_NAMESPACE)
        .unwrap();
    let mut active: configuration_documents::ActiveModel = row.into();
    active.schema_version = Set(2);
    active.update(store.db()).await.unwrap();
    let unsupported = load_output(&store).await.expect_err("schema 2");
    assert!(
        unsupported
            .to_string()
            .contains("unsupported configuration schema"),
        "{unsupported}"
    );
    let again = bootstrap_control_plane(
        &store,
        files.path(),
        &files.path().join("cache"),
        None,
        &Config::default(),
    )
    .await
    .expect_err("an unsupported output document fails startup closed");
    assert!(again.to_string().contains("core.output"), "{again}");
}

/// Two connections to one database observe one `core.output` document, and a
/// compare-and-swap from the connection holding the older revision loses
/// without writing an audit row or a change notice.
async fn assert_two_connections_share_output(store_a: &LibraryStore, store_b: &LibraryStore) {
    let files_a = tempdir().unwrap();
    let files_b = tempdir().unwrap();
    let session_a = bootstrap_control_plane(
        store_a,
        files_a.path(),
        &files_a.path().join("cache"),
        None,
        &output_seed(true, NamingProfile::Classic),
    )
    .await
    .unwrap();
    std::fs::copy(
        master_key_path(files_a.path()),
        master_key_path(files_b.path()),
    )
    .unwrap();
    // B's own TOML differs; the shared document wins.
    let session_b = bootstrap_control_plane(
        store_b,
        files_b.path(),
        &files_b.path().join("cache"),
        None,
        &output_seed(false, NamingProfile::Audiobookshelf),
    )
    .await
    .unwrap();
    assert_eq!(session_a.cluster_id, session_b.cluster_id);
    assert_ne!(session_a.node.node_id, session_b.node.node_id);
    assert_eq!(session_a.output, session_b.output);
    assert!(session_b.output.body.widevine);

    let stale = session_b.output.revision;
    let mut from_a = session_a.output.body.clone();
    from_a
        .set("output.multi_destination", "refetch_all")
        .unwrap();
    let applied = replace_output(store_a, &operator(), stale, &from_a, "output-a")
        .await
        .unwrap();
    let OutputReplace::Applied(doc_a) = applied else {
        panic!("{applied:?}");
    };
    let seen_by_b = load_output(store_b).await.unwrap();
    assert_eq!(seen_by_b, *doc_a);

    let audits = audit_count(store_b, OUTPUT_NAMESPACE).await.unwrap();
    let changes = change_count(store_b, OUTPUT_NAMESPACE).await.unwrap();
    let mut from_b = session_b.output.body.clone();
    from_b.set("output.create_cue", "true").unwrap();
    let lost = replace_output(store_b, &operator(), stale, &from_b, "output-b")
        .await
        .unwrap();
    assert_eq!(
        lost,
        OutputReplace::Conflict {
            current_revision: doc_a.revision
        }
    );
    assert_eq!(
        audit_count(store_b, OUTPUT_NAMESPACE).await.unwrap(),
        audits
    );
    assert_eq!(
        change_count(store_b, OUTPUT_NAMESPACE).await.unwrap(),
        changes
    );
    let after = load_output(store_a).await.unwrap();
    assert_eq!(after, *doc_a);
    assert!(!after.body.create_cue);
    assert_eq!(
        after.body.multi_destination,
        MultiDestinationMode::RefetchAll
    );
}

#[tokio::test]
async fn two_sqlite_connections_share_one_output_document() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempdir().unwrap();
    let path = dir.path().join("library.db");
    let store_a = file_store(&path).await;
    let store_b = file_store(&path).await;
    assert_two_connections_share_output(&store_a, &store_b).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_two_pools_share_one_output_document() {
    if !postgres_tests_enabled() {
        return;
    }
    let _guard = master_key_test_lock_async().await;
    let (store_a, store_b) = postgres_stores().await;
    assert_two_connections_share_output(&store_a, &store_b).await;
}

#[test]
fn a_legacy_seed_keeps_valid_values_and_defaults_out_of_range_ones() {
    let seed = OutputConfig {
        max_filename_length: 2048,
        split_mp3_max_mb: 0,
        cover_size: "huge".into(),
        widevine: true,
        create_cue: true,
        ..OutputConfig::default()
    };
    assert!(OutputSettingsV1::from_config(&seed).is_err());
    let (settings, replaced) = OutputSettingsV1::from_seed_lenient(&seed);
    settings.validate().expect("a lenient seed is valid");
    assert_eq!(
        replaced,
        vec!["cover_size", "split_mp3_max_mb", "max_filename_length"]
    );
    let defaults = OutputSettingsV1::from_config(&OutputConfig::default()).unwrap();
    assert_eq!(settings.max_filename_length, defaults.max_filename_length);
    assert_eq!(settings.split_mp3_max_mb, defaults.split_mp3_max_mb);
    assert_eq!(settings.cover_size, defaults.cover_size);
    assert!(
        settings.widevine && settings.create_cue,
        "valid values are kept"
    );

    let (clean, none) = OutputSettingsV1::from_seed_lenient(&OutputConfig::default());
    assert!(none.is_empty());
    assert_eq!(clean, defaults);
}
