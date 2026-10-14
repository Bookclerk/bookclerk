//! `bookclerk plugins install|update|rollback|remove|approve|releases` for
//! cluster releases.
//!
//! Installing publishes a release: the package is resolved and verified once
//! (publisher trust, archive and payload digests, `plugin.toml`), recorded
//! in the library with the operator's consent, and on a first install or
//! `--activate` made the active release. Nothing under this host's
//! `plugins/` changes; every host placed on the plugin materializes the
//! release by digest. The CLI only writes records; a running daemon applies
//! them when its host queue wakes it.

use std::path::Path;

use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    activate_release, approve_release_grant, clear_active_release, delete_plugin_placement,
    list_deployments_for_instance, list_plugin_instances_for_key, list_plugin_schemas,
    list_release_pointers, list_releases, load_active_release, load_release,
    replace_plugin_deployment, ConfigActor, DeploymentReplace, DesiredState, PluginRelease,
};
use bookclerk_library::LibraryStore;
use bookclerk_plugin_catalog::{
    BookclerkPackageManifest, Installer, PackageCoordinate, TrustPolicy,
};
use bookclerk_plugin_host::{
    consent_summary, grant_covers_release, host_artifact, install_release, publish_package_release,
    release_consent_request, Activate, InstallRequest, InstalledRelease, ReleaseRequest,
};
use serde_json::json;

use crate::format_out::{emit, OutputFormat};

/// Operator actor recorded on release writes from the CLI.
fn cli_actor() -> ConfigActor {
    ConfigActor::Operator { id: "cli".into() }
}

/// What `plugins install` was asked to publish.
pub struct InstallArgs<'a> {
    /// Coordinate text (ignored with `--archive`).
    pub coordinate: &'a str,
    /// Local archive.
    pub archive: Option<&'a Path>,
    /// Package manifest for `--archive`.
    pub manifest: Option<&'a Path>,
    /// Target of the local archive.
    pub target: Option<String>,
    /// Make this the active release even when another is active.
    pub activate: bool,
    /// Record the requested consent as approved.
    pub approve_capabilities: bool,
    /// Accept a publisher without independent authenticity.
    pub allow_unverified_publisher: bool,
    /// Resolve and print only.
    pub dry_run: bool,
    /// Refuse remote downloads.
    pub offline: bool,
}

/// The active release a plugin alias or key names, if any.
///
/// # Errors
///
/// Returns an error when a library read fails.
pub async fn active_release_for(
    store: &LibraryStore,
    needle: &str,
) -> anyhow::Result<Option<PluginRelease>> {
    if let Some(release) = load_active_release(store, needle).await? {
        return Ok(Some(release));
    }
    let schemas = list_plugin_schemas(store).await?;
    let keys: Vec<_> = schemas
        .iter()
        .filter(|s| s.manifest_id.eq_ignore_ascii_case(needle))
        .map(|s| s.plugin_key.clone())
        .collect();
    for key in keys {
        if let Some(release) = load_active_release(store, &key).await? {
            return Ok(Some(release));
        }
    }
    Ok(None)
}

/// `plugins install`: publish a release.
///
/// # Errors
///
/// Returns an error when trust policy refuses the publisher, the package
/// does not resolve or verify, or the alias belongs to another plugin.
pub async fn run_install(
    config: &Config,
    args: InstallArgs<'_>,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let trust = TrustPolicy {
        allow_unverified_publisher: args.allow_unverified_publisher
            || config.plugins.allow_unverified_publisher,
        ..TrustPolicy::default()
    };
    trust.check_unverified_publisher_allowed()?;
    let plugins_root = config.paths().files_dir.join("plugins");
    let (package, coordinate, archive) = if let Some(archive) = args.archive {
        let manifest_path = args.manifest.ok_or_else(|| {
            anyhow::anyhow!("--archive requires --manifest <bookclerk-package.json>")
        })?;
        let text = std::fs::read_to_string(manifest_path)?;
        let package = BookclerkPackageManifest::from_json(&text)?;
        let coordinate = Installer::local_archive_coordinate(archive, &package);
        (package, coordinate, Some(archive.canonicalize()?))
    } else {
        let coordinate = resolve_coordinate(args.coordinate)?;
        let package = bookclerk_plugin_catalog::fetch_manifest_for_coordinate(&coordinate, &[])?;
        (package, coordinate, None)
    };
    let path_key = Installer::plugin_key_for(&coordinate, &package.id, &plugins_root)?
        .canonical()
        .to_string();
    if args.dry_run {
        let key = path_key;
        let payload = json!({
            "dry_run": true,
            "plugin_key": key,
            "id": package.id,
            "coordinate": coordinate.to_string(),
            "targets": package.artifacts.iter().map(|a| a.bookclerk_target()).collect::<Vec<_>>(),
        });
        return emit(format, &payload, || {
            println!(
                "dry-run: would publish {} ({}) as a release of {key}",
                package.id, coordinate
            );
        });
    }
    let plane = crate::registry::open_host(config).await?;
    let installed = install_release(
        &plane.store,
        &cli_actor(),
        &config.paths().files_dir,
        plane.node_id(),
        InstallRequest {
            package,
            coordinate,
            archive,
            target: args.target,
            offline: args.offline,
            activate: args.activate,
            approve: args.approve_capabilities,
        },
    )
    .await?;
    let InstalledRelease {
        release,
        created,
        active,
        instance,
    } = installed;
    let approved = !release.grant_json.is_empty();
    let payload = json!({
        "plugin_key": release.plugin_key,
        "id": release.manifest_id,
        "version": release.version,
        "created": created,
        "active": active,
        "approved": approved,
        "plugin_instance_id": instance.id.to_string(),
        "artifacts": release.artifacts.iter().map(|a| json!({
            "target": a.target,
            "url": a.url,
            "archive_sha256": a.archive_sha256,
            "payload_root_sha256": a.payload_root_sha256,
        })).collect::<Vec<_>>(),
    });
    emit(format, &payload, || {
        println!(
            "published {} {} ({}){}{}",
            release.manifest_id,
            release.version,
            release.plugin_key,
            if created { "" } else { " (already published)" },
            if active {
                "; active"
            } else {
                "; not active (use --activate or plugins rollback)"
            }
        );
        if !approved {
            println!(
                "consent pending: run `bookclerk plugins approve {}`",
                release.manifest_id
            );
        }
        println!(
            "enable it with `bookclerk plugins enable {}`; hosts materialize it by digest",
            release.manifest_id
        );
    })
}

/// `plugins update` for released plugins. Returns false when `id` names no
/// released plugin (the caller falls back to host-local receipts).
///
/// # Errors
///
/// Returns an error when resolution, publishing, or activation fails.
pub async fn run_update(
    config: &Config,
    id: Option<&str>,
    to: Option<&str>,
    allow_unverified_publisher: bool,
    approve_capabilities: bool,
    dry_run: bool,
    format: OutputFormat,
) -> anyhow::Result<bool> {
    let store = crate::registry::open_host(config).await?;
    let node_id = store.node_id().to_string();
    let store = &store.store;
    let mut targets = Vec::new();
    match id {
        Some(needle) => match active_release_for(store, needle).await? {
            Some(release) => targets.push(release),
            None => return Ok(false),
        },
        None => {
            for pointer in list_release_pointers(store).await? {
                if let Some(release) =
                    load_release(store, &pointer.plugin_key, &pointer.version).await?
                {
                    targets.push(release);
                }
            }
            if targets.is_empty() {
                return Ok(false);
            }
        }
    }
    let trust = TrustPolicy {
        allow_unverified_publisher: allow_unverified_publisher
            || config.plugins.allow_unverified_publisher,
        ..TrustPolicy::default()
    };
    let mut results = Vec::new();
    for current in targets {
        if current.coordinate.is_empty() {
            results.push(json!({
                "id": current.manifest_id,
                "version": current.version,
                "skipped": "published from a local archive; publish the new archive with `plugins install --archive … --activate`",
            }));
            continue;
        }
        let mut coordinate = PackageCoordinate::parse(&current.coordinate)?;
        match to {
            Some(version) => coordinate.version = version.to_string(),
            None => match bookclerk_plugin_catalog::resolve_newer_version(&coordinate, &[])? {
                Some(version) => coordinate.version = version,
                None => {
                    results.push(json!({
                        "id": current.manifest_id,
                        "version": current.version,
                        "up_to_date": true,
                    }));
                    continue;
                }
            },
        }
        if dry_run {
            results.push(json!({
                "id": current.manifest_id,
                "from": current.version,
                "to": coordinate.version,
                "dry_run": true,
            }));
            continue;
        }
        trust.check_unverified_publisher_allowed()?;
        let package = bookclerk_plugin_catalog::fetch_manifest_for_coordinate(&coordinate, &[])?;
        let published = publish_package_release(
            store,
            &cli_actor(),
            &config.paths().files_dir,
            &node_id,
            ReleaseRequest {
                plugin_key: current.plugin_key.clone(),
                package,
                archive: None,
                target: None,
                coordinate: coordinate.to_string(),
                offline: false,
                publisher_verified: false,
                grant: None,
                activate: Activate::Always,
            },
        )
        .await?;
        let mut release = published.release;
        let carried = serde_json::from_str(&current.grant_json)
            .ok()
            .filter(|grant| grant_covers_release(grant, &release));
        let grant = match carried {
            Some(grant) => Some(grant),
            None if approve_capabilities => Some(release_consent_request(&release)?),
            None => None,
        };
        if let Some(grant) = grant {
            release = approve_release_grant(
                store,
                &cli_actor(),
                &release.plugin_key,
                &release.version,
                &serde_json::to_string(&grant)?,
            )
            .await?;
        }
        results.push(json!({
            "id": release.manifest_id,
            "from": current.version,
            "to": release.version,
            "approved": !release.grant_json.is_empty(),
        }));
    }
    emit(format, &results, || {
        for row in &results {
            if let Some(to) = row["to"].as_str() {
                println!(
                    "{} {} -> {}{}",
                    row["id"].as_str().unwrap_or("-"),
                    row["from"].as_str().unwrap_or("-"),
                    to,
                    if row["dry_run"].as_bool().unwrap_or(false) {
                        " (dry-run)"
                    } else if row["approved"].as_bool() == Some(false) {
                        "; consent widened, run `bookclerk plugins approve`"
                    } else {
                        ""
                    }
                );
            } else if let Some(reason) = row["skipped"].as_str() {
                println!("{}: {reason}", row["id"].as_str().unwrap_or("-"));
            } else {
                println!(
                    "{} {} is up to date",
                    row["id"].as_str().unwrap_or("-"),
                    row["version"].as_str().unwrap_or("-")
                );
            }
        }
    })?;
    Ok(true)
}

/// `plugins rollback <plugin> <version>`: move the active pointer back.
///
/// # Errors
///
/// Returns an error when the plugin or version is not published.
pub async fn run_rollback(
    config: &Config,
    id: &str,
    version: &str,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let plane = crate::registry::open_host(config).await?;
    let store = &plane.store;
    let current = active_release_for(store, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("plugin `{id}` has no active release"))?;
    let (pointer, _) = activate_release(
        store,
        &cli_actor(),
        &current.plugin_key,
        version,
        plane.node_id(),
    )
    .await?;
    let payload = json!({
        "plugin_key": pointer.plugin_key,
        "from": current.version,
        "version": pointer.version,
        "revision": pointer.revision,
    });
    emit(format, &payload, || {
        println!(
            "{} active release {} -> {}; hosts with the digest cached switch at once",
            current.manifest_id, current.version, pointer.version
        );
    })
}

/// `plugins remove` for a released plugin. Returns false when `id` names
/// no released plugin.
///
/// # Errors
///
/// Returns an error when a library write fails.
pub async fn run_remove(config: &Config, id: &str, format: OutputFormat) -> anyhow::Result<bool> {
    let plane = crate::registry::open_host(config).await?;
    let store = &plane.store;
    let Some(release) = active_release_for(store, id).await? else {
        return Ok(false);
    };
    let mut disabled = 0;
    for instance in list_plugin_instances_for_key(store, &release.plugin_key).await? {
        // The scheduler would redeploy a placed instance; drop its placement.
        delete_plugin_placement(store, &cli_actor(), &instance.id).await?;
        for deployment in list_deployments_for_instance(store, &instance.id).await? {
            if !deployment.desired.runs() {
                continue;
            }
            let replaced = replace_plugin_deployment(
                store,
                &cli_actor(),
                &deployment.deployment_id,
                deployment.revision,
                DesiredState::Absent,
                &format!(
                    "cli-remove-{}-{}",
                    deployment.deployment_id, deployment.revision
                ),
            )
            .await?;
            if matches!(replaced, DeploymentReplace::Applied(_)) {
                disabled += 1;
            }
        }
    }
    clear_active_release(store, &cli_actor(), &release.plugin_key).await?;
    let payload = json!({
        "plugin_key": release.plugin_key,
        "id": release.manifest_id,
        "deployments_disabled": disabled,
    });
    emit(format, &payload, || {
        println!(
            "removed {} ({}): {disabled} deployment(s) set absent, active release cleared; \
             hosts retire the guest and sweep the cached digest",
            release.manifest_id, release.plugin_key
        );
    })?;
    Ok(true)
}

/// `plugins approve` for a released plugin. Returns false when `id` names
/// no released plugin.
///
/// # Errors
///
/// Returns an error when approval is declined or a write fails.
pub async fn run_approve(
    config: &Config,
    id: &str,
    yes: bool,
    format: OutputFormat,
) -> anyhow::Result<bool> {
    use std::io::{self, IsTerminal, Write};

    let plane = crate::registry::open_host(config).await?;
    let store = &plane.store;
    let Some(release) = active_release_for(store, id).await? else {
        return Ok(false);
    };
    let grant = release_consent_request(&release)?;
    let summary = consent_summary(&grant);
    for line in &summary {
        println!("{line}");
    }
    if !yes {
        if !io::stdin().is_terminal() {
            anyhow::bail!(
                "stdin is not a TTY; pass --yes to approve plugin `{}` non-interactively",
                release.manifest_id
            );
        }
        eprint!(
            "Approve these permissions for `{}` {} on every host? [y/N] ",
            release.manifest_id, release.version
        );
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            anyhow::bail!("approval cancelled");
        }
    }
    approve_release_grant(
        store,
        &cli_actor(),
        &release.plugin_key,
        &release.version,
        &serde_json::to_string(&grant)?,
    )
    .await?;
    let payload = json!({
        "id": release.manifest_id,
        "version": release.version,
        "approved": true,
        "grant": grant,
        "summary": summary,
    });
    emit(format, &payload, || {
        println!(
            "approved permissions for `{}` {} (recorded on the release for every host)",
            release.manifest_id, release.version
        );
    })?;
    Ok(true)
}

/// `plugins releases [plugin]`: published versions and the active pointer.
///
/// # Errors
///
/// Returns an error when a library read fails.
pub async fn run_list(
    config: &Config,
    id: Option<&str>,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let store = crate::registry::open_library(config).await?;
    let key = match id {
        Some(needle) => Some(
            active_release_for(&store, needle)
                .await?
                .map(|r| r.plugin_key)
                .unwrap_or_else(|| needle.to_string()),
        ),
        None => None,
    };
    let releases = list_releases(&store, key.as_deref()).await?;
    let pointers = list_release_pointers(&store).await?;
    let rows: Vec<_> = releases
        .iter()
        .map(|r| {
            let active = pointers
                .iter()
                .any(|p| p.plugin_key == r.plugin_key && p.version == r.version);
            json!({
                "plugin_key": r.plugin_key,
                "id": r.manifest_id,
                "version": r.version,
                "active": active,
                "approved": !r.grant_json.is_empty(),
                "publisher_verified": r.publisher_verified,
                "published_at": r.published_at,
                "published_by": r.published_by,
                "node_target": host_artifact(r).ok().map(|a| a.payload_root_sha256.clone()),
                "targets": r.artifacts.iter().map(|a| a.target.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();
    emit(format, &rows, || {
        if rows.is_empty() {
            println!("no releases");
        }
        for row in &rows {
            println!(
                "{}{} {} {} targets={} {}",
                if row["active"].as_bool().unwrap_or(false) {
                    "* "
                } else {
                    "  "
                },
                row["id"].as_str().unwrap_or("-"),
                row["version"].as_str().unwrap_or("-"),
                row["plugin_key"].as_str().unwrap_or("-"),
                row["targets"]
                    .as_array()
                    .map(|t| t
                        .iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(","))
                    .unwrap_or_default(),
                if row["approved"].as_bool().unwrap_or(false) {
                    "approved"
                } else {
                    "consent pending"
                }
            );
        }
    })
}

/// Parses a coordinate, treating a bare `name@version` as a crates.io crate.
fn resolve_coordinate(raw: &str) -> anyhow::Result<PackageCoordinate> {
    super::plugins::resolve_coordinate(raw)
}
