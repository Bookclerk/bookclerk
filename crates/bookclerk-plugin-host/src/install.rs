//! Installing and materializing cluster versions.
//!
//! Installing resolves and verifies a package once (archive digest,
//! `plugin.toml`, executable, payload-root digest) and records a
//! [`PluginVersion`] row; nothing on the installing node's plugin tree
//! changes. A node materializes the active version when its reconciler needs
//! it: fetch the artifact for its target, extract under
//! `plugins/.staging/version-<digest>.<pid>`, verify the payload-root digest
//! against the row, and rename to `plugins/.versions/<digest>`. A partial
//! extract never reaches the final path, a cached digest is immutable and
//! shared by every instance of that version, and digests no deployment
//! references are swept after a grace period.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookclerk_library::control_plane::{
    activate_version, add_plugin_version, approve_version_grant, list_installs, list_versions,
    load_active_version, load_artifact_blob, load_version, put_artifact_blob, ConfigActor,
    PluginArtifact, PluginInstall, PluginInstance, PluginVersion, ARTIFACT_BLOB_SCHEME,
    MAX_ARTIFACT_BLOB_BYTES,
};
use bookclerk_library::LibraryStore;
use bookclerk_plugin_catalog::{
    extract_archive, fetch_artifact, fetch_owner_artifact, host_bookclerk_target,
    is_platform_plugin_key, payload_root_sha256, require_mutable_child, safe_join, select_target,
    sha256_file, ArchiveFormat, ArtifactTarget, BookclerkPackageManifest, Installer,
    PackageCoordinate, PluginKey, PluginMutationLock, RegistrySource,
};

use crate::consent::PluginGrant;
use crate::manifest::PluginManifest;
use crate::{DiscoveredPlugin, PluginError, Result};

/// Directory under `plugins/` holding materialized version trees.
pub const VERSION_CACHE_DIR: &str = ".versions";

/// How long an unreferenced cached digest survives before the sweep.
pub const VERSION_CACHE_GRACE: Duration = Duration::from_secs(3600);

/// Cache root: `<files_dir>/plugins/.versions`.
#[must_use]
pub fn version_cache_root(files_dir: &Path) -> PathBuf {
    files_dir.join("plugins").join(VERSION_CACHE_DIR)
}

/// True for a canonical SHA-256 digest: exactly 64 lowercase hex digits.
///
/// Every digest that names a path (the version cache) or is compared
/// against a computed digest is held to this form, so an owner- or
/// registry-supplied value such as `..`, an absolute path, or one with a
/// separator can never select a directory outside the cache.
#[must_use]
pub fn is_sha256_hex(raw: &str) -> bool {
    raw.len() == 64 && raw.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Cached tree of one payload-root digest.
///
/// # Errors
///
/// Returns an error when `payload_root_sha256` is not a canonical SHA-256
/// ([`is_sha256_hex`]): it is joined onto the cache root, so anything else
/// could name a directory outside it.
pub fn version_cache_path(files_dir: &Path, payload_root_sha256: &str) -> Result<PathBuf> {
    if !is_sha256_hex(payload_root_sha256) {
        return Err(PluginError::message(
            "a version payload digest is not 64 lowercase hex digits",
        ));
    }
    Ok(version_cache_root(files_dir).join(payload_root_sha256))
}

/// When installing moves the active pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activate {
    /// Always point at the new version (`update`, `--activate`).
    Always,
    /// Only when the plugin has no active version yet (first install).
    IfNone,
    /// Never; install only.
    Never,
}

/// One package to install.
#[derive(Debug, Clone)]
pub struct VersionRequest {
    /// Canonical plugin key the version is recorded under.
    pub plugin_key: String,
    /// Install-grade package manifest.
    pub package: BookclerkPackageManifest,
    /// Local archive. `None` fetches every target's artifact from its URL
    /// and verifies the manifest digests.
    pub archive: Option<PathBuf>,
    /// Target the local archive is built for (default: this host's).
    pub target: Option<String>,
    /// Package coordinate recorded for `update`; empty for a local archive.
    pub coordinate: String,
    /// Refuse remote downloads (a local archive needs none).
    pub offline: bool,
    /// Publisher verification result (recorded on the version).
    pub publisher_verified: bool,
    /// Consent grant approved at install time, if any.
    pub grant: Option<PluginGrant>,
    /// When to move the active pointer.
    pub activate: Activate,
}

/// What [`add_package_version`] recorded.
#[derive(Debug, Clone)]
pub struct AddedVersion {
    /// The version row.
    pub version: PluginVersion,
    /// True when this call wrote the row (false for an identical re-install).
    pub created: bool,
    /// Pointer after activation, when this call moved it.
    pub pointer: Option<PluginInstall>,
}

/// Maps a library error onto the host error type.
fn lib(err: bookclerk_library::LibraryError) -> PluginError {
    use bookclerk_library::LibraryError;
    match err {
        LibraryError::Conflict(message) => PluginError::Conflict(message),
        LibraryError::NotFound(_) => PluginError::NotFound(err.to_string()),
        LibraryError::Unavailable(message) => PluginError::Unavailable(message),
        other => PluginError::message(other.to_string()),
    }
}

/// Maps a catalog error onto the host error type.
fn cat(err: bookclerk_plugin_catalog::CatalogError) -> PluginError {
    PluginError::message(err.to_string())
}

/// Maps an I/O error onto the host error type.
fn io(err: std::io::Error) -> PluginError {
    PluginError::message(err.to_string())
}

/// What extracting one archive taught the publisher.
struct Inspected {
    /// `plugin.toml` text.
    manifest_toml: String,
    /// Parsed manifest.
    manifest: PluginManifest,
    /// Payload-root digest of the plugin tree.
    payload_root_sha256: String,
    /// SHA-256 of the archive bytes.
    archive_sha256: String,
}

/// Archive format recorded on a version artifact.
fn format_name(format: ArchiveFormat) -> &'static str {
    format.extension()
}

/// Parses a recorded archive format.
fn parse_format(name: &str) -> ArchiveFormat {
    if name == "zip" {
        ArchiveFormat::Zip
    } else {
        ArchiveFormat::TarGz
    }
}

/// Extracts `archive` into a scratch directory and reads the plugin tree.
fn inspect_archive(
    files_dir: &Path,
    archive: &Path,
    artifact: &ArtifactTarget,
) -> Result<Inspected> {
    let plugins = files_dir.join("plugins");
    let staging_root = plugins.join(".staging");
    std::fs::create_dir_all(&staging_root).map_err(io)?;
    let scratch = require_mutable_child(
        &plugins,
        &staging_root.join(format!("install-{}", uuid::Uuid::new_v4().simple())),
    )
    .map_err(cat)?;
    std::fs::create_dir_all(&scratch).map_err(io)?;
    let result = (|| {
        let archive_sha256 = sha256_file(archive).map_err(cat)?;
        let tree = scratch.join("tree");
        extract_archive(archive, artifact.archive_format(), &tree).map_err(cat)?;
        let root = safe_join(&tree, Path::new(&artifact.archive_root)).map_err(cat)?;
        let manifest_toml = std::fs::read_to_string(root.join("plugin.toml"))
            .map_err(|err| PluginError::message(format!("archive has no plugin.toml: {err}")))?;
        let manifest = bookclerk_plugin_manifest::parse(&manifest_toml)
            .map_err(|err| PluginError::message(format!("plugin.toml: {err}")))?;
        let executable = safe_join(&root, Path::new(&artifact.executable)).map_err(cat)?;
        if !executable.is_file() {
            return Err(PluginError::message(format!(
                "archive is missing the executable `{}`",
                artifact.executable
            )));
        }
        let payload_root_sha256 = payload_root_sha256(&root).map_err(cat)?;
        Ok(Inspected {
            manifest_toml,
            manifest,
            payload_root_sha256,
            archive_sha256,
        })
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// Verifies `package`, records a [`PluginVersion`], and moves the active
/// pointer per [`VersionRequest::activate`].
///
/// A local archive's digest is computed here (it is the operator's own
/// file); a remote artifact must match the digest its package manifest
/// declares. Archives up to the library blob cap are stored in the
/// database when no shared URL exists, so every host can materialize them.
///
/// # Errors
///
/// Returns an error for a platform key (platform guests ship with the
/// host), a malformed or yanked package, a digest mismatch, a tree that
/// disagrees with the package, or a library write failure.
pub async fn add_package_version(
    store: &LibraryStore,
    actor: &ConfigActor,
    files_dir: &Path,
    node_id: &str,
    request: VersionRequest,
) -> Result<AddedVersion> {
    let key = PluginKey::parse(&request.plugin_key).map_err(cat)?;
    if is_platform_plugin_key(&key) {
        return Err(PluginError::message(format!(
            "`{}` is a platform guest; it ships with the host and is not installed",
            request.plugin_key
        )));
    }
    request.package.validate_for_install().map_err(cat)?;
    let files = files_dir.to_path_buf();
    let package = request.package.clone();
    let archive = request.archive.clone();
    let target = request.target.clone();
    let offline = request.offline;
    let inspected: Vec<(ArtifactTarget, Inspected, Option<PathBuf>)> =
        tokio::task::spawn_blocking(move || -> Result<_> {
            let mut out = Vec::new();
            if let Some(archive) = archive {
                let artifact =
                    select_target(&package.artifacts, |a| a.target.as_str(), target.as_deref())
                        .map_err(cat)?
                        .clone();
                let inspected = inspect_archive(&files, &archive, &artifact)?;
                out.push((artifact, inspected, Some(archive)));
            } else {
                let scratch = files.join("plugins").join(".staging");
                std::fs::create_dir_all(&scratch).map_err(io)?;
                for artifact in &package.artifacts {
                    let download = scratch.join(format!("fetch-{}", uuid::Uuid::new_v4().simple()));
                    let fetched = fetch_artifact(&artifact.url, &download, offline).map_err(cat);
                    let inspected = fetched.and_then(|()| {
                        let actual = sha256_file(&download).map_err(cat)?;
                        if actual != artifact.archive_sha256 {
                            return Err(PluginError::message(format!(
                                "artifact for `{}` has digest {actual}, the package declares {}",
                                artifact.target, artifact.archive_sha256
                            )));
                        }
                        inspect_archive(&files, &download, artifact)
                    });
                    let _ = std::fs::remove_file(&download);
                    out.push((artifact.clone(), inspected?, None));
                }
            }
            Ok(out)
        })
        .await
        .map_err(|err| PluginError::message(format!("install task: {err}")))??;

    let (_, first, _) = inspected
        .first()
        .ok_or_else(|| PluginError::message("package has no artifact"))?;
    let version = first
        .manifest
        .package_version()
        .map_err(|err| PluginError::message(err.to_string()))?;
    for (artifact, tree, _) in &inspected {
        if tree.manifest.id != request.package.id {
            return Err(PluginError::message(format!(
                "artifact `{}` carries plugin `{}`, the package is `{}`",
                artifact.target, tree.manifest.id, request.package.id
            )));
        }
        let tree_version = tree
            .manifest
            .package_version()
            .map_err(|err| PluginError::message(err.to_string()))?;
        if tree_version != version {
            return Err(PluginError::message(format!(
                "artifacts disagree on the version ({tree_version} vs {version})"
            )));
        }
    }
    let mut artifacts = Vec::with_capacity(inspected.len());
    for (artifact, tree, local) in &inspected {
        let url = match local {
            Some(path) => {
                let bytes = tokio::fs::read(path).await.map_err(io)?;
                if bytes.len() <= MAX_ARTIFACT_BLOB_BYTES {
                    put_artifact_blob(store, &bytes).await.map_err(lib)?
                } else {
                    let canonical = path.canonicalize().map_err(io)?;
                    format!("file://{}", canonical.display())
                }
            }
            None => artifact.url.clone(),
        };
        artifacts.push(PluginArtifact {
            target: artifact.bookclerk_target(),
            url,
            format: format_name(artifact.archive_format()).to_string(),
            archive_root: artifact.archive_root.clone(),
            archive_sha256: tree.archive_sha256.clone(),
            payload_root_sha256: tree.payload_root_sha256.clone(),
            executable: artifact.executable.clone(),
        });
    }
    let grant_json = match &request.grant {
        Some(grant) => serde_json::to_string(grant)
            .map_err(|err| PluginError::message(format!("encode grant: {err}")))?,
        None => String::new(),
    };
    if request.activate == Activate::IfNone {
        // An automatic first activation never rolls a newer pin back: a
        // stale operator-placed archive must not prune documents written
        // for the newer schema. An explicit activation (`update`,
        // `rollback`) may.
        if let Some(pinned) =
            bookclerk_library::control_plane::load_plugin_schema(store, &request.plugin_key)
                .await
                .map_err(lib)?
        {
            let order = bookclerk_plugin_abi::package_version::package_version_order(
                &version,
                &pinned.package_version,
            )
            .map_err(PluginError::message)?;
            if order == std::cmp::Ordering::Less {
                return Err(PluginError::message(format!(
                    "package version {version} is older than {} pinned by node {}; not activated",
                    pinned.package_version, pinned.recorded_by_node
                )));
            }
        }
    }
    let row = PluginVersion {
        plugin_key: request.plugin_key.clone(),
        version: version.clone(),
        manifest_id: first.manifest.id.clone(),
        manifest_toml: first.manifest_toml.clone(),
        settings: first.manifest.settings_schema(),
        artifacts,
        publisher_verified: request.publisher_verified,
        grant_json,
        coordinate: request.coordinate.clone(),
        installed_at: String::new(),
        installed_by: String::new(),
    };
    let outcome = add_plugin_version(store, actor, row).await.map_err(lib)?;
    let created = matches!(
        outcome,
        bookclerk_library::control_plane::AddVersion::Created(_)
    );
    let row = outcome.version().clone();
    let activate = match request.activate {
        Activate::Always => true,
        Activate::IfNone => load_active_version(store, &request.plugin_key)
            .await
            .map_err(lib)?
            .is_none(),
        Activate::Never => false,
    };
    let pointer = if activate {
        Some(
            activate_version(store, actor, &request.plugin_key, &row.version, node_id)
                .await
                .map_err(lib)?
                .0,
        )
    } else {
        None
    };
    Ok(AddedVersion {
        version: row,
        created,
        pointer,
    })
}

/// The version artifact this node runs.
///
/// # Errors
///
/// Returns an error naming the host target when the version has none for
/// it, so placement can exclude this host.
pub fn host_artifact(version: &PluginVersion) -> Result<&PluginArtifact> {
    let target = host_bookclerk_target();
    version.artifact_for(target).ok_or_else(|| {
        PluginError::message(format!(
            "version {} {} has no artifact for target `{target}`",
            version.plugin_key, version.version
        ))
    })
}

/// Makes the active version's tree available under
/// `plugins/.versions/<payload digest>` and returns that path.
///
/// A cached digest is reused as is; otherwise the archive is fetched (from
/// the library blob store, a `file:` path, or HTTPS), its digest checked,
/// extracted under `.staging`, the payload-root digest checked against the
/// version, and renamed into place under the plugin mutation lock.
///
/// # Errors
///
/// Returns an error when the version has no artifact for this host, a
/// digest is not 64 lowercase hex digits ([`is_sha256_hex`]; checked before
/// any path is formed or a cached tree reused), the artifact cannot be
/// fetched, or a digest does not match.
pub async fn materialize_version(
    store: &LibraryStore,
    files_dir: &Path,
    version: &PluginVersion,
) -> Result<PathBuf> {
    materialize_from(store, files_dir, version, ArtifactOrigin::Local).await
}

/// [`materialize_version`] for a version an owner cluster deploys to this
/// host: a `blob:` archive is read from the library as usual (the caller
/// fetched it over the owner link and checked its digest), and any other
/// URL is fetched only as an owner `https:` URL
/// ([`bookclerk_plugin_catalog::fetch_owner_artifact`]: no `file:`, no
/// local or private hosts, every redirect hop checked the same way).
///
/// A tree already cached under the artifact's payload digest is reused
/// without a fetch or an archive check, so the caller must have checked
/// the whole artifact (URL, archive digest and payload digest) against the
/// host operator's approval first
/// (`RemotePluginConsent::approved_artifact_matches`).
///
/// # Arguments
///
/// * `store` - This host's library.
/// * `files_dir` - This host's files dir.
/// * `version` - The owner's version, with the grant this host runs it with.
///
/// # Errors
///
/// As [`materialize_version`], and when the owner's URL is refused.
pub async fn materialize_owner_version(
    store: &LibraryStore,
    files_dir: &Path,
    version: &PluginVersion,
) -> Result<PathBuf> {
    materialize_from(store, files_dir, version, ArtifactOrigin::Owner).await
}

/// Who chose a version's artifact URL, which decides how it is fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactOrigin {
    /// This node's operator or registry ([`fetch_artifact`]).
    Local,
    /// An owner cluster, on a host node ([`fetch_owner_artifact`]).
    Owner,
}

/// [`materialize_version`] with the fetch policy of `origin`.
async fn materialize_from(
    store: &LibraryStore,
    files_dir: &Path,
    version: &PluginVersion,
    origin: ArtifactOrigin,
) -> Result<PathBuf> {
    let artifact = host_artifact(version)?.clone();
    // Both digests are checked before either names a path or the cached
    // tree is reused: a reused tree is trusted by its digest alone.
    if !is_sha256_hex(&artifact.archive_sha256) {
        return Err(PluginError::message(format!(
            "version {} {} archive digest is not 64 lowercase hex digits",
            version.plugin_key, version.version
        )));
    }
    let cache = version_cache_path(files_dir, &artifact.payload_root_sha256)?;
    if cache.join("plugin.toml").is_file() {
        return Ok(cache);
    }
    let blob = match artifact.url.strip_prefix(ARTIFACT_BLOB_SCHEME) {
        Some(sha) => Some(
            load_artifact_blob(store, sha)
                .await
                .map_err(lib)?
                .ok_or_else(|| {
                    PluginError::message(format!(
                        "version {} {} archive is missing from the library",
                        version.plugin_key, version.version
                    ))
                })?,
        ),
        None => None,
    };
    let files = files_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        materialize_blocking(&files, &artifact, blob.as_deref(), &cache, origin)
    })
    .await
    .map_err(|err| PluginError::message(format!("materialize task: {err}")))?
}

/// Filesystem half of [`materialize_version`].
///
/// The archive is fetched and its digest checked without the plugin mutation
/// lock, so a slow download (bounded by `DOWNLOAD_TIMEOUT`) does not block
/// every other install, materialize, and sweep on this node. Each call
/// stages in its own directory; the sweep leaves staging younger than
/// [`VERSION_CACHE_GRACE`] alone. Extraction and the rename into the cache
/// run under the lock.
fn materialize_blocking(
    files_dir: &Path,
    artifact: &PluginArtifact,
    blob: Option<&[u8]>,
    cache: &Path,
    origin: ArtifactOrigin,
) -> Result<PathBuf> {
    if cache.join("plugin.toml").is_file() {
        return Ok(cache.to_path_buf());
    }
    let plugins = files_dir.join("plugins");
    std::fs::create_dir_all(plugins.join(".staging")).map_err(io)?;
    std::fs::create_dir_all(plugins.join(VERSION_CACHE_DIR)).map_err(io)?;
    let staging = require_mutable_child(
        &plugins,
        &plugins.join(".staging").join(format!(
            "version-{}.{}",
            artifact.payload_root_sha256,
            uuid::Uuid::new_v4().simple()
        )),
    )
    .map_err(cat)?;
    std::fs::create_dir_all(&staging).map_err(io)?;
    let result = (|| {
        let archive = staging.join("archive");
        match blob {
            Some(bytes) => std::fs::write(&archive, bytes).map_err(io)?,
            None => match origin {
                ArtifactOrigin::Local => fetch_artifact(&artifact.url, &archive, false),
                ArtifactOrigin::Owner => fetch_owner_artifact(&artifact.url, &archive),
            }
            .map_err(cat)?,
        }
        let actual = sha256_file(&archive).map_err(cat)?;
        if actual != artifact.archive_sha256 {
            return Err(PluginError::message(format!(
                "version archive digest {actual} does not match the version ({})",
                artifact.archive_sha256
            )));
        }
        let _lock = PluginMutationLock::acquire(files_dir).map_err(cat)?;
        // Another materialization of this version may have finished first.
        if cache.join("plugin.toml").is_file() {
            return Ok(cache.to_path_buf());
        }
        let tree = staging.join("tree");
        extract_archive(&archive, parse_format(&artifact.format), &tree).map_err(cat)?;
        let root = safe_join(&tree, Path::new(&artifact.archive_root)).map_err(cat)?;
        let payload = payload_root_sha256(&root).map_err(cat)?;
        if payload != artifact.payload_root_sha256 {
            return Err(PluginError::message(format!(
                "version payload digest {payload} does not match the version ({})",
                artifact.payload_root_sha256
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let exe = safe_join(&root, Path::new(&artifact.executable)).map_err(cat)?;
            if exe.is_file() {
                std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))
                    .map_err(io)?;
            }
        }
        let dest = require_mutable_child(&plugins, cache).map_err(cat)?;
        std::fs::rename(&root, &dest).map_err(io)?;
        Ok(dest)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// Opens a materialized version tree as a spawnable plugin carrying the
/// version's identity and consent grant.
///
/// # Errors
///
/// Returns an error when the tree's manifest is unreadable or names another
/// plugin, or the version has no approved grant yet.
pub fn open_installed_plugin(root: &Path, version: &PluginVersion) -> Result<DiscoveredPlugin> {
    let artifact = host_artifact(version)?;
    let grant = version_grant(version)?;
    let text = std::fs::read_to_string(root.join("plugin.toml")).map_err(io)?;
    let manifest = bookclerk_plugin_manifest::parse(&text)
        .map_err(|err| PluginError::message(format!("plugin.toml: {err}")))?;
    if manifest.id != version.manifest_id {
        return Err(PluginError::message(format!(
            "cached tree carries plugin `{}`, the version is `{}`",
            manifest.id, version.manifest_id
        )));
    }
    DiscoveredPlugin::from_release(
        manifest,
        root.to_path_buf(),
        &version.plugin_key,
        &version.version,
        &artifact.payload_root_sha256,
        &artifact.archive_sha256,
        grant,
    )
}

/// The consent grant recorded on `version`.
///
/// # Errors
///
/// Returns an error when no operator has approved the version yet or the
/// recorded grant is malformed.
pub fn version_grant(version: &PluginVersion) -> Result<PluginGrant> {
    if version.grant_json.trim().is_empty() {
        return Err(PluginError::message(format!(
            "version {} {} has no approved grant; run `bookclerk plugins approve {}`",
            version.plugin_key, version.version, version.manifest_id
        )));
    }
    serde_json::from_str(&version.grant_json)
        .map_err(|err| PluginError::message(format!("version grant is malformed: {err}")))
}

/// Removes cached digests not in `keep` and older than `grace`, plus stale
/// version staging directories. Returns how many entries were removed.
///
/// # Errors
///
/// Returns an error when the mutation lock cannot be taken.
pub fn sweep_version_cache(
    files_dir: &Path,
    keep: &BTreeSet<String>,
    grace: Duration,
) -> Result<usize> {
    let plugins = files_dir.join("plugins");
    if !plugins.is_dir() {
        return Ok(0);
    }
    let _lock = PluginMutationLock::acquire(files_dir).map_err(cat)?;
    let old_enough = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= grace)
    };
    let mut removed = 0;
    for (dir, wanted) in [
        (plugins.join(VERSION_CACHE_DIR), None::<&str>),
        (plugins.join(".staging"), Some("version-")),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            match wanted {
                Some(prefix) if !name.starts_with(prefix) && !name.starts_with("install-") => {
                    continue
                }
                None if keep.contains(&name) => continue,
                _ => {}
            }
            let path = entry.path();
            if !old_enough(&path) {
                continue;
            }
            let Ok(path) = require_mutable_child(&plugins, &path) else {
                continue;
            };
            if std::fs::remove_dir_all(&path).is_ok() {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Consent the version's manifest requests (what `plugins approve` shows).
///
/// # Errors
///
/// Returns an error when the recorded `plugin.toml` or plugin key no longer
/// parses.
pub fn version_consent_request(version: &PluginVersion) -> Result<PluginGrant> {
    let manifest = bookclerk_plugin_manifest::parse(&version.manifest_toml)
        .map_err(|err| PluginError::message(format!("version plugin.toml: {err}")))?;
    let key = PluginKey::parse(&version.plugin_key).map_err(cat)?;
    Ok(crate::consent::consent_request(&manifest, &key))
}

/// True when `grant` still covers everything `version` requests, so an
/// update may carry an earlier approval forward. A widened request needs a
/// fresh `plugins approve`.
#[must_use]
pub fn grant_covers_version(grant: &PluginGrant, version: &PluginVersion) -> bool {
    version_consent_request(version)
        .is_ok_and(|requested| crate::consent::grant_covers(grant, &requested))
}

/// Creates the default instance of a installed plugin with its schema
/// defaults, as enrollment does for a host-local plugin. Idempotent.
///
/// # Errors
///
/// Returns an error when an instance or document write fails.
pub async fn enroll_version(
    store: &LibraryStore,
    actor: &ConfigActor,
    version: &PluginVersion,
) -> Result<bookclerk_library::control_plane::PluginInstance> {
    let instance =
        bookclerk_library::control_plane::ensure_plugin_instance(store, actor, &version.plugin_key)
            .await
            .map_err(lib)?;
    bookclerk_library::control_plane::import_instance_config_if_absent(
        store,
        actor,
        &instance.id,
        &version.settings,
        &bookclerk_library::control_plane::PluginInstanceConfigV1::empty(),
        &format!("seed-instance-config-{}", instance.id),
    )
    .await
    .map_err(lib)?;
    Ok(instance)
}

/// An operator's `install`: what `bookclerk plugins install` and
/// `POST /api/config/plugin-versions` both install.
#[derive(Debug, Clone)]
pub struct InstallRequest {
    /// Resolved package manifest.
    pub package: BookclerkPackageManifest,
    /// Where the package came from. A local archive's coordinate is
    /// path-derived and is not recorded on the version.
    pub coordinate: PackageCoordinate,
    /// Local archive to install instead of fetching the coordinate.
    pub archive: Option<PathBuf>,
    /// Install only this target instead of every target in the package.
    pub target: Option<String>,
    /// Refuse network fetches.
    pub offline: bool,
    /// Make the version active even when another version already is.
    pub activate: bool,
    /// Record the requested consent as approved when no earlier approval
    /// carries forward.
    pub approve: bool,
}

/// What [`install_version`] did.
#[derive(Debug, Clone)]
pub struct InstalledVersion {
    /// The installed (and maybe approved) version.
    pub version: PluginVersion,
    /// False when this exact version was already installed.
    pub created: bool,
    /// True when the version is the plugin's active version afterwards.
    pub active: bool,
    /// The plugin's default instance.
    pub instance: PluginInstance,
}

/// The plugin key an install installs under.
///
/// A registry coordinate names a stable key. A local archive's path does
/// not: a new version of an already installed plugin arrives as another
/// file, so its key is the key its alias is already installed under, and
/// only a first install takes the path-derived key.
///
/// # Errors
///
/// Returns an error when the coordinate yields no key or the alias is
/// installed under several keys.
pub async fn install_plugin_key(
    store: &LibraryStore,
    files_dir: &Path,
    coordinate: &PackageCoordinate,
    package: &BookclerkPackageManifest,
) -> Result<String> {
    let derived = Installer::plugin_key_for(coordinate, &package.id, &files_dir.join("plugins"))
        .map_err(cat)?
        .canonical()
        .to_string();
    if !matches!(coordinate.source, RegistrySource::LocalArchive) {
        return Ok(derived);
    }
    let mut known: Vec<String> = list_versions(store, None)
        .await
        .map_err(lib)?
        .into_iter()
        .filter(|r| r.manifest_id.eq_ignore_ascii_case(&package.id))
        .map(|r| r.plugin_key)
        .collect();
    known.sort();
    known.dedup();
    match known.as_slice() {
        [] => Ok(derived),
        [one] => Ok(one.clone()),
        many => Err(PluginError::conflict(format!(
            "alias `{}` is installed under several plugin keys ({}); uninstall the extra ones",
            package.id,
            many.join(", ")
        ))),
    }
}

/// Installs an operator's package as a version, carries an earlier
/// approval forward when it still covers the new version (or approves when
/// asked), and enrolls the plugin's default instance.
///
/// One alias names one plugin: a version may not take an alias another
/// plugin's active version already answers to.
///
/// # Errors
///
/// Returns an error when the key cannot be derived, the alias belongs to
/// another plugin, or installing, approval, or enrollment fails.
pub async fn install_version(
    store: &LibraryStore,
    actor: &ConfigActor,
    files_dir: &Path,
    node_id: &str,
    request: InstallRequest,
) -> Result<InstalledVersion> {
    let key = install_plugin_key(store, files_dir, &request.coordinate, &request.package).await?;
    for pointer in list_installs(store).await.map_err(lib)? {
        if pointer.plugin_key == key {
            continue;
        }
        let other = load_version(store, &pointer.plugin_key, &pointer.version)
            .await
            .map_err(lib)?;
        if other.is_some_and(|o| o.manifest_id.eq_ignore_ascii_case(&request.package.id)) {
            return Err(PluginError::conflict(format!(
                "alias `{}` is already the active version of {}; uninstall it first",
                request.package.id, pointer.plugin_key
            )));
        }
    }
    let previous = load_active_version(store, &key).await.map_err(lib)?;
    let coordinate = if matches!(request.coordinate.source, RegistrySource::LocalArchive) {
        String::new()
    } else {
        request.coordinate.to_string()
    };
    let installed = add_package_version(
        store,
        actor,
        files_dir,
        node_id,
        VersionRequest {
            plugin_key: key.clone(),
            package: request.package,
            archive: request.archive,
            target: request.target,
            coordinate,
            offline: request.offline,
            publisher_verified: false,
            grant: None,
            activate: if request.activate {
                Activate::Always
            } else {
                Activate::IfNone
            },
        },
    )
    .await?;
    let mut version = installed.version;
    if version.grant_json.is_empty() {
        let carried = previous
            .as_ref()
            .filter(|p| !p.grant_json.is_empty())
            .and_then(|p| serde_json::from_str::<PluginGrant>(&p.grant_json).ok())
            .filter(|grant| grant_covers_version(grant, &version));
        let grant = match carried {
            Some(grant) => Some(grant),
            None if request.approve => Some(version_consent_request(&version)?),
            None => None,
        };
        if let Some(grant) = grant {
            let grant_json = serde_json::to_string(&grant)
                .map_err(|err| PluginError::message(format!("grant: {err}")))?;
            version = approve_version_grant(
                store,
                actor,
                &version.plugin_key,
                &version.version,
                &grant_json,
            )
            .await
            .map_err(lib)?;
        }
    }
    let instance = enroll_version(store, actor, &version).await?;
    let active = load_active_version(store, &key)
        .await
        .map_err(lib)?
        .is_some_and(|a| a.version == version.version);
    Ok(InstalledVersion {
        version,
        created: installed.created,
        active,
        instance,
    })
}
