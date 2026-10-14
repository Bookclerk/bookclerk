//! External plugin host for Bookclerk (`bookclerk-plugin-host`).
//!
//! Discovers staged guests under `$BOOKCLERK_FILES_DIR/plugins/` and spawns
//! them over the Workers RPC ABI through the mandatory `bookclerk-workerd`
//! front door (`api_version = 3` Cap'n Proto on stdio).
//!
//! Production hosts never link ordinary plugin implementation crates.
//! First-party storefronts (Audible, Libro.fm, …) are staged guests, the
//! same path third-party plugins use. Database adapters own physical
//! lowering behind the typed `databaseAdapter` capability (`openSession`,
//! `dropUnit`); production hosts do not link `bookclerk-plugin-database-*`
//! crates to perform product behavior.
//!
//! External plugins are **untrusted** relative to the host: the host never
//! passes `library.db` / `master.key` / the files-dir root, clears
//! secret-bearing env on spawn, and mediates credentials + library upserts.
//! Operators must `bookclerk plugins approve` domains/bindings before enable;
//! the same covering grant is required again at every external spawn and at
//! privileged delivery points (`config` / `secrets` / `work_fs` / `oauth`).
//! Durable identity is provenance-qualified `PluginKey`;
//! the manifest `id` is a display / CLI alias.
//!
//! Host binaries should depend on **this** crate for registration — not on
//! individual store crates.
//!
//! # Guest SDK
//!
//! Third-party Rust plugins should depend on [`bookclerk_plugin_sdk`], not this
//! crate. This host crate re-exports the protocol types for in-tree convenience.
//!
//! See `docs/plugins.md`, `docs/adr/plugin-workers-rpc-workerd.md`, and
//! `docs/plugin-registry.md`.

#[cfg(test)]
mod deployment_reconcile_tests;
#[cfg(test)]
mod enrollment_tests;

mod authority;
mod callback_proxy;
mod consent;
mod crates_io;
mod deployment;
mod destinations;
mod discover;
mod enrollment;
mod error;
mod event_publisher;
mod host;
mod host_control_plane;
mod install_preflight;
mod instance_bindings;
mod jail;
mod kv_binding;
mod manifest;
mod queue_producer;
mod registry;
mod release;
mod rpc;
mod rpc_session;
mod spawn_plan;
mod spawn_stdio;

pub use bookclerk_plugin_sdk::protocol;
pub use bookclerk_plugin_sdk::{
    methods, CatalogHit, CliArg, CliArgKind, CliArgSpec, CliCommandSpec, CliInvokeParams,
    CliInvokeResult, CliSchema, EventPollResult, ExpandCandidatesParams, ExternalUser,
    FetchTitleParams, ListeningProgress, LoginCompleteParams, LoginParams, LoginResult,
    LoginStartResult, PlainFetch, PlainPart, PluginDescribe, PurchaseHint, PurchaseHintParams,
    ScanBook, ScanParams, ScanSummary, SearchCatalogParams, SourceAccount, SyncListeningResult,
    PRODUCT_API_VERSION, PROTOCOL_NAME,
};

pub use authority::{
    apply_grant_store, authority_revision, fence_plugin_key, fence_stale_grant_revisions,
    fence_stale_sessions, is_fenced, live_session_count, notify_grants_changed,
    reconcile_grants_from_disk, register_session, register_session_revisions,
    register_session_with_shutdown, spawn_grant_watcher, unregister_session, watch_grants_loop,
    SessionShutdown, GRANT_WATCH_INTERVAL,
};
pub use bookclerk_plugin_catalog::host_bookclerk_target;
pub use bookclerk_plugin_manifest::TcpGrant;
pub use bookclerk_plugin_sdk::{JobCheckpoint, JobInvocationLease, JobOutcome};
pub use callback_proxy::CallbackProxy;
pub use consent::{
    active_processes_for, canonical_event_filter, consent_request, consent_request_alias,
    consent_summary, consumers_cover, cores_to_percent, database_binding_name, effective_cpu_cores,
    effective_cpu_rate_percent, effective_disk_budget_bytes, effective_disk_mib,
    effective_extra_processes, effective_grant, effective_memory_mib, format_cpu_cores,
    grant_covers, grant_has_binding, grant_revision, grant_within_ceiling,
    granted_consumers_from_manifest, granted_database_bindings, granted_jobs_from_manifest,
    granted_queue_consumers, host_cpu_cores_max, host_cpu_rate_max, host_logical_cpus,
    host_overlay_config_digest, inject_workerd_grant_env, jail_process_overhead,
    network_compatible, overlay_host_implied_network, pending_structural, percent_to_cores,
    queue_consumer_grant, queue_producer_grant, reconcile_host_overlay_authority, require_binding,
    require_grant, spawn_config_for_grant, spawn_grant, validate_approved_grant,
    validate_described_capabilities, validate_described_config_options,
    windows_session_active_processes, GrantedEventConsumer, PendingStructural, PluginGrant,
    PluginGrantStore, GRANTS_FILE, GRANT_SCHEMA_VERSION, KNOWN_HOST_BINDINGS,
    PLUGIN_JAIL_ACTIVE_PROCESSES_MAX, PLUGIN_JAIL_CPU_CORES_DEFAULT, PLUGIN_JAIL_CPU_RATE_DEFAULT,
    PLUGIN_JAIL_CPU_RATE_MAX, PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT, PLUGIN_JAIL_EXTRA_PROCESSES_MAX,
    PLUGIN_JAIL_MEMORY_MIB_DEFAULT, PLUGIN_JAIL_MEMORY_MIB_MAX, PLUGIN_STATE_BUDGET_MIB_DEFAULT,
    PLUGIN_STATE_BUDGET_MIB_MAX, QUEUE_CONSUMER_BINDING_PREFIX, QUEUE_PRODUCER_BINDING_PREFIX,
    WINDOWS_SESSION_PROCESS_BASELINE, WORKERD_GRANT_CPU_MS_ENV, WORKERD_GRANT_DOMAINS_ENV,
    WORKERD_GRANT_NETWORK_MODE_ENV, WORKERD_GRANT_POLICY_ENV, WORKERD_GRANT_SUBREQUESTS_ENV,
};
pub use crates_io::search_crates_io;
pub use deployment::{
    load_authorized_local_packages, reconcile_local_deployments, AuthorizedLocalPackages,
    DeploymentRuntime, DeploymentSpawn, LiveDeploymentRuntime, LocalPackage, SpawnHealth,
    AUTHORIZED_PACKAGE_DIR,
};
pub use destinations::{build_acquire_destinations, build_storage_backend};
pub use discover::{
    discover_plugins, first_party_database_kind, identity_matches_occupancy,
    is_first_party_audiobookshelf, is_first_party_local_output, is_first_party_s3_output,
    occupancy_matches_alias, occupancy_names_alias, occupancy_spec, plugin_matches_occupancy,
    plugin_search_dirs, resolve_plugin_ref, resolve_plugin_slot, settings_table,
    settings_table_for, stamp_occupancy_plugin_key, upgrade_unique_alias_occupancy,
    DiscoveredPlugin,
};
pub use enrollment::{
    enroll_local_plugins, enroll_one, has_covering_grant, is_bootstrap_only, EnrolledPlugin,
};
pub use error::{PluginError, Result};
pub use event_publisher::{EventOutbox, OutboxEventPublisher};
pub use host::{
    backup_adapter_id, database_connect_bindings, export_registered_plugin_units,
    load_external_database, migrate_database_plugin, migrate_library_schema, open_library_store,
    open_library_store_for_plugin, restore_plugin_backup_units, DatabaseRegistry,
    DestinationRegistry, ExternalDatabase, ExternalIntegration, ExternalSource,
};
pub use host_control_plane::{
    allow_credential_login, enroll_host, open_host_control_plane,
    open_host_control_plane_with_store, refresh_present, HostControlPlane, HostEnrollment,
    PresentDeployment,
};
pub use install_preflight::{
    configured_alias_occupants, install_from_manifest_with_configured_aliases,
    install_local_archive_with_configured_aliases, reject_configured_alias_collision,
};
pub use instance_bindings::{
    binding_grant_flags, prepare_open_for_instance, prepared_open_from_resolved, PreparedOpen,
};
pub use jail::plugin_data_dir;
pub use kv_binding::{KvNamespaces, LibraryKvNamespace, KV_GRANT_BINDING};
pub use manifest::{
    embedded_logo_api_path, entrypoint_family, logo_content_type, validate_logo,
    BindingCapabilities, CapabilitiesManifest, DatabaseBindingManifest, Entrypoint, EventConsumer,
    EventProducer, EventsManifest, JailNetworkNeed, LogoKind, ModuleSpec, NamedBinding,
    NetworkCapabilities, NetworkMode, PluginFamily, PluginManifest, PluginRuntimeKind,
    TriggersManifest, WorkerdLimits, WorkerdRuntimeManifest, ALL_ENTRYPOINTS,
    MAX_EMBEDDED_LOGO_BYTES,
};
pub use queue_producer::{OutboxQueueProducer, QueueOutbox};
pub use registry::{
    family_keyword, host_target_triple, validate_plugin_id, BookclerkPackageMetadata,
    PluginCatalogEntry, PluginCrateName, CRATE_NAME_PREFIX, PRODUCT_KEYWORD, REGISTRY_KEYWORD,
};
pub use release::{
    enroll_release, grant_covers_release, host_artifact, install_plugin_key, install_release,
    materialize_release, open_released_plugin, publish_package_release, release_cache_path,
    release_cache_root, release_consent_request, release_grant, sweep_release_cache, Activate,
    InstallRequest, InstalledRelease, PublishedRelease, ReleaseRequest, RELEASE_CACHE_DIR,
    RELEASE_CACHE_GRACE,
};
pub use rpc_session::{
    plugin_instance_key, ExecutorIdentity, GuestDatabaseFactory, PluginSession, PluginStorage,
    RpcBackupOps, SessionServices, HOST_SHARED_ACCOUNT, OPERATOR_ACCOUNT,
};
pub use spawn_plan::{
    GuestRuntimeKind, SpawnPlan, SpawnTransport, WorkerdFrontDoor, WORKERD_BIN_ENV,
    WORKERD_LAUNCHER_ENV,
};
pub use spawn_stdio::{
    note_spawn_stage, recent_spawn_diagnostics, retry_abandoned_acl_journals,
    revoke_plugin_state_before_purge, TEST_CHANNEL_IDENT_ENV, TEST_CHANNEL_TAG_FILE,
    TEST_INJECT_EXTRA_ENDPOINT_ENV, TEST_OBSERVE_EXTRA_ENDPOINT_ENV,
};
