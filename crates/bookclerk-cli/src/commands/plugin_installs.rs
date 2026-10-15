//! `bookclerk plugins install|update|rollback|remove|approve|versions` for
//! cluster versions.
//!
//! Installing installs a version: the package is resolved and verified once
//! (publisher trust, archive and payload digests, `plugin.toml`), recorded
//! in the library with the operator's consent, and on a first install or
//! `--activate` made the active version. Nothing under this host's
//! `plugins/` changes; every host placed on the plugin materializes the
//! version by digest. The CLI only writes records; a running daemon applies
//! them when its host queue wakes it.

use std::path::Path;

use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    activate_version, approve_version_grant, clear_install, list_installs,
    list_plugin_instances_for_key, list_plugin_schemas, list_versions, load_active_version,
    load_version, undeploy_instance, ConfigActor, PluginVersion,
};
use bookclerk_library::LibraryStore;
use bookclerk_plugin_catalog::{
    BookclerkPackageManifest, Installer, PackageCoordinate, TrustPolicy,
};
use bookclerk_plugin_host::{
    add_package_version, consent_summary, grant_covers_version, host_artifact, install_version,
    version_consent_request, Activate, InstallRequest, InstalledVersion, VersionRequest,
};
use serde_json::json;

use crate::format_out::{emit, OutputFormat};

/// Operator actor recorded on version writes from the CLI.
fn cli_actor() -> ConfigActor {
    ConfigActor::Operator { id: "cli".into() }
}

/// What `plugins install` was asked to install.
pub struct InstallArgs<'a> {
    /// Coordinate text (ignored with `--archive`).
    pub coordinate: &'a str,
    /// Local archive.
    pub archive: Option<&'a Path>,
    /// Package manifest for `--archive`.
    pub manifest: Option<&'a Path>,
    /// Target of the local archive.
    pub target: Option<String>,
    /// Make this the active version even when another is active.
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

/// The active version a plugin alias or key names, if any.
///
/// # Errors
///
/// Returns an error when a library read fails.
pub async fn active_version_for(
    store: &LibraryStore,
    needle: &str,
) -> anyhow::Result<Option<PluginVersion>> {
    if let Some(version) = load_active_version(store, needle).await? {
        return Ok(Some(version));
    }
    let schemas = list_plugin_schemas(store).await?;
    let keys: Vec<_> = schemas
        .iter()
        .filter(|s| s.manifest_id.eq_ignore_ascii_case(needle))
        .map(|s| s.plugin_key.clone())
        .collect();
    for key in keys {
        if let Some(version) = load_active_version(store, &key).await? {
            return Ok(Some(version));
        }
    }
    Ok(None)
}

/// `plugins install`: install a version.
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
                "dry-run: would install {} ({}) as a version of {key}",
                package.id, coordinate
            );
        });
    }
    let plane = crate::registry::open_host(config).await?;
    let installed = install_version(
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
    let InstalledVersion {
        version,
        created,
        active,
        instance,
    } = installed;
    let approved = !version.grant_json.is_empty();
    let payload = json!({
        "plugin_key": version.plugin_key,
        "id": version.manifest_id,
        "version": version.version,
        "created": created,
        "active": active,
        "approved": approved,
        "plugin_instance_id": instance.id.to_string(),
        "artifacts": version.artifacts.iter().map(|a| json!({
            "target": a.target,
            "url": a.url,
            "archive_sha256": a.archive_sha256,
            "payload_root_sha256": a.payload_root_sha256,
        })).collect::<Vec<_>>(),
    });
    emit(format, &payload, || {
        println!(
            "installed {} {} ({}){}{}",
            version.manifest_id,
            version.version,
            version.plugin_key,
            if created { "" } else { " (already installed)" },
            if active {
                "; active"
            } else {
                "; not active (use --activate or plugins rollback)"
            }
        );
        if !approved {
            println!(
                "consent pending: run `bookclerk plugins approve {}`",
                version.manifest_id
            );
        }
        println!(
            "deploy it with `bookclerk plugins deploy {}`; nodes materialize it by digest",
            version.manifest_id
        );
    })
}

/// `plugins update` for installed plugins. Returns false when `id` names no
/// installed plugin (the caller falls back to host-local receipts).
///
/// # Errors
///
/// Returns an error when resolution, installing, or activation fails.
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
        Some(needle) => match active_version_for(store, needle).await? {
            Some(version) => targets.push(version),
            None => return Ok(false),
        },
        None => {
            for pointer in list_installs(store).await? {
                if let Some(version) =
                    load_version(store, &pointer.plugin_key, &pointer.version).await?
                {
                    targets.push(version);
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
                "skipped": "installed from a local archive; install the new archive with `plugins install --archive … --activate`",
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
        let installed = add_package_version(
            store,
            &cli_actor(),
            &config.paths().files_dir,
            &node_id,
            VersionRequest {
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
        let mut version = installed.version;
        let carried = serde_json::from_str(&current.grant_json)
            .ok()
            .filter(|grant| grant_covers_version(grant, &version));
        let grant = match carried {
            Some(grant) => Some(grant),
            None if approve_capabilities => Some(version_consent_request(&version)?),
            None => None,
        };
        if let Some(grant) = grant {
            version = approve_version_grant(
                store,
                &cli_actor(),
                &version.plugin_key,
                &version.version,
                &serde_json::to_string(&grant)?,
            )
            .await?;
        }
        results.push(json!({
            "id": version.manifest_id,
            "from": current.version,
            "to": version.version,
            "approved": !version.grant_json.is_empty(),
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
/// Returns an error when the plugin or version is not installed.
pub async fn run_rollback(
    config: &Config,
    id: &str,
    version: &str,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let plane = crate::registry::open_host(config).await?;
    let store = &plane.store;
    let current = active_version_for(store, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("plugin `{id}` has no active version"))?;
    let (pointer, _) = activate_version(
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
            "{} active version {} -> {}; hosts with the digest cached switch at once",
            current.manifest_id, current.version, pointer.version
        );
    })
}

/// `plugins uninstall` for a installed plugin. Returns false when `id` names
/// no installed plugin.
///
/// # Errors
///
/// Returns an error when a library write fails.
pub async fn run_uninstall(
    config: &Config,
    id: &str,
    format: OutputFormat,
) -> anyhow::Result<bool> {
    let plane = crate::registry::open_host(config).await?;
    let store = &plane.store;
    let Some(version) = active_version_for(store, id).await? else {
        return Ok(false);
    };
    let mut disabled = 0;
    // Every instance must be stopped before the active version goes; a
    // deployment that keeps losing a race fails the command and leaves the
    // install in place.
    for instance in list_plugin_instances_for_key(store, &version.plugin_key).await? {
        disabled += undeploy_instance(store, &cli_actor(), &instance.id).await?;
    }
    clear_install(store, &cli_actor(), &version.plugin_key).await?;
    let payload = json!({
        "plugin_key": version.plugin_key,
        "id": version.manifest_id,
        "uninstalled": true,
        "deployments_undeployed": disabled,
    });
    emit(format, &payload, || {
        println!(
            "uninstalled {} ({}): undeployed from {disabled} node(s); \
             nodes retire the guest and sweep the cached digest",
            version.manifest_id, version.plugin_key
        );
    })?;
    Ok(true)
}

/// `plugins approve` for an installed plugin. Returns false when `id` names
/// no installed plugin.
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
    let Some(version) = active_version_for(store, id).await? else {
        return Ok(false);
    };
    let grant = version_consent_request(&version)?;
    let summary = consent_summary(&grant);
    for line in &summary {
        println!("{line}");
    }
    if !yes {
        if !io::stdin().is_terminal() {
            anyhow::bail!(
                "stdin is not a TTY; pass --yes to approve plugin `{}` non-interactively",
                version.manifest_id
            );
        }
        eprint!(
            "Approve these permissions for `{}` {} on every host? [y/N] ",
            version.manifest_id, version.version
        );
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            anyhow::bail!("approval cancelled");
        }
    }
    approve_version_grant(
        store,
        &cli_actor(),
        &version.plugin_key,
        &version.version,
        &serde_json::to_string(&grant)?,
    )
    .await?;
    let payload = json!({
        "id": version.manifest_id,
        "version": version.version,
        "approved": true,
        "grant": grant,
        "summary": summary,
    });
    emit(format, &payload, || {
        println!(
            "approved permissions for `{}` {} (recorded on the version for every host)",
            version.manifest_id, version.version
        );
    })?;
    Ok(true)
}

/// `plugins versions [plugin]`: installed versions and the active pointer.
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
            active_version_for(&store, needle)
                .await?
                .map(|r| r.plugin_key)
                .unwrap_or_else(|| needle.to_string()),
        ),
        None => None,
    };
    let versions = list_versions(&store, key.as_deref()).await?;
    let pointers = list_installs(&store).await?;
    let rows: Vec<_> = versions
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
                "installed_at": r.installed_at,
                "installed_by": r.installed_by,
                "node_target": host_artifact(r).ok().map(|a| a.payload_root_sha256.clone()),
                "targets": r.artifacts.iter().map(|a| a.target.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();
    emit(format, &rows, || {
        if rows.is_empty() {
            println!("no versions");
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
