//! Canonical effective authority and live session fencing.
//!
//! Structural capabilities originate in the plugin package. The operator may
//! narrow them but cannot invent entrypoints, consumers, producers, jobs, or
//! host bindings.
//! Network destinations are operator-extensible: explicit host-owned grants
//! may add hostnames beyond the manifest. Guest `describe()` cannot widen
//! either class.
//!
//! ```text
//! effective structural = manifest ∩ operator grant ∩ host policy
//! effective network    = host policy ∩ approved(manifest network ∪ operator additions)
//!                      − operator denials
//! ```
//!
//! `authority_revision` is SHA-256 of the canonical **effective** policy
//! (not presentation, not `configuration_revision` / installed
//! `plugin.toml` hash). Grant changes fence already-running sessions that
//! still hold the old revision.
//!
//! Fencing is not process-local and is not deferred until the next host RPC:
//!
//! 1. In-process grant mutation ([`crate::PluginGrantStore::upsert`]) immediately
//!    marks matching sessions cancelled and runs their shutdown hooks
//!    (`Work::Shutdown` → vat exit → `kill_on_drop` child, which tears down
//!    workerd, the native guest, the socket proxy, EVENTS, and granted DB
//!    channels).
//! 2. [`spawn_grant_watcher`] observes `$BOOKCLERK_FILES_DIR/plugin-grants.json`
//!    so `bookclerk plugins approve` in the CLI process fences sessions owned
//!    by `bookclerkd`.
//! 3. New spawns re-read the grant file and refuse to return a session whose
//!    revision no longer matches disk.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::consent::{PluginGrant, PluginGrantStore};
use crate::PluginError;

/// How often the daemon re-reads `plugin-grants.json` when no in-process notify fires.
pub const GRANT_WATCH_INTERVAL: Duration = Duration::from_millis(100);

/// Shutdown hook invoked the moment a live session is fenced.
pub type SessionShutdown = Arc<dyn Fn() + Send + Sync>;

/// Live plugin sessions keyed by provenance-qualified PluginKey.
struct LiveSession {
    /// Canonical PluginKey for this vat.
    plugin_key: String,
    /// [`crate::grant_revision`] of the persisted operator grant at spawn.
    grant_revision: String,
    /// [`authority_revision`] of the effective runtime grant at spawn.
    revision: String,
    /// Set when a later grant for this key no longer matches `revision`.
    cancelled: Arc<AtomicBool>,
    /// Proactively stop the vat / child / mediated resources.
    shutdown: SessionShutdown,
}

/// Process-wide live session table.
fn live() -> &'static Mutex<Vec<LiveSession>> {
    static LIVE: OnceLock<Mutex<Vec<LiveSession>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(Vec::new()))
}

/// In-process wake for [`watch_grants_loop`] (same-process `save`).
fn grant_file_notify() -> &'static Notify {
    static NOTIFY: OnceLock<Notify> = OnceLock::new();
    NOTIFY.get_or_init(Notify::new)
}

/// Wake the grant watcher in **this** process (same-process save / tests).
///
/// Cross-process CLI writes are observed by polling the grants file.
pub fn notify_grants_changed() {
    grant_file_notify().notify_waiters();
}

/// Registers a live session and returns its cancel flag.
///
/// Drop the session (or call [`unregister_session`]) when the vat exits.
/// Sessions registered this way are still cancelled on grant change, but
/// have no shutdown hook — prefer [`register_session_with_shutdown`] for
/// product vats so fencing does not wait for the next RPC.
#[must_use]
pub fn register_session(plugin_key: &str, revision: &str) -> Arc<AtomicBool> {
    register_session_with_shutdown(plugin_key, revision, Arc::new(|| {}))
}

/// Registers a live session whose shutdown hook runs the instant it is fenced.
///
/// Call this **after** the vat work channel exists so `shutdown` can send
/// `Work::Shutdown` without racing spawn.
#[must_use]
pub fn register_session_with_shutdown(
    plugin_key: &str,
    revision: &str,
    shutdown: SessionShutdown,
) -> Arc<AtomicBool> {
    register_session_revisions(plugin_key, revision, revision, shutdown)
}

/// Registers a live session with distinct persisted vs effective revisions.
///
/// `grant_revision` is the digest of the **stored** operator grant. `revision`
/// is [`authority_revision`] of the **effective** runtime grant (after host
/// overlays). The grant watcher compares grant revisions only.
#[must_use]
pub fn register_session_revisions(
    plugin_key: &str,
    grant_revision: &str,
    revision: &str,
    shutdown: SessionShutdown,
) -> Arc<AtomicBool> {
    let cancelled = Arc::new(AtomicBool::new(false));
    if let Ok(mut guard) = live().lock() {
        guard.push(LiveSession {
            plugin_key: plugin_key.to_string(),
            grant_revision: grant_revision.to_string(),
            revision: revision.to_string(),
            cancelled: Arc::clone(&cancelled),
            shutdown,
        });
    }
    cancelled
}

/// Drops a previously registered cancel flag from the live set.
pub fn unregister_session(flag: &Arc<AtomicBool>) {
    if let Ok(mut guard) = live().lock() {
        guard.retain(|s| !Arc::ptr_eq(&s.cancelled, flag));
    }
}

/// Fences every live session for `plugin_key` (grant/authority change).
pub fn fence_plugin_key(plugin_key: &str) {
    fence_stale_sessions(plugin_key, "");
}

/// Fences live sessions for `plugin_key` whose revision is not `current_revision`.
///
/// Pass an empty `current_revision` to fence every session for the key.
/// Each fenced session is cancelled **and** its shutdown hook is invoked
/// immediately (vat `Work::Shutdown`, which kills the child and mediated
/// sockets). Hooks run after the live-table lock is released.
pub fn fence_stale_sessions(plugin_key: &str, current_revision: &str) {
    if plugin_key.is_empty() {
        return;
    }
    let mut hooks: Vec<SessionShutdown> = Vec::new();
    if let Ok(guard) = live().lock() {
        for session in guard.iter() {
            if session.plugin_key == plugin_key
                && (current_revision.is_empty() || session.revision != current_revision)
            {
                session.cancelled.store(true, Ordering::SeqCst);
                hooks.push(Arc::clone(&session.shutdown));
            }
        }
    }
    for hook in hooks {
        hook();
    }
}

/// Fences live sessions whose **persisted** [`crate::grant_revision`] is not current.
pub fn fence_stale_grant_revisions(plugin_key: &str, current_grant_revision: &str) {
    if plugin_key.is_empty() {
        return;
    }
    let mut hooks: Vec<SessionShutdown> = Vec::new();
    if let Ok(guard) = live().lock() {
        for session in guard.iter() {
            if session.plugin_key == plugin_key
                && (current_grant_revision.is_empty()
                    || session.grant_revision != current_grant_revision)
            {
                session.cancelled.store(true, Ordering::SeqCst);
                hooks.push(Arc::clone(&session.shutdown));
            }
        }
    }
    for hook in hooks {
        hook();
    }
}

/// Reconcile every live session against a loaded **persisted** grant store.
///
/// Missing keys and [`crate::grant_revision`] mismatches are fenced. Effective
/// [`authority_revision`] (host overlays, clamped budgets) is **not** compared
/// to the persisted digest. Host-config overlay changes fence through
/// [`crate::ExecutorIdentity::configuration_revision`].
pub fn apply_grant_store(store: &PluginGrantStore) {
    let snapshot: Vec<(String, String)> = match live().lock() {
        Ok(guard) => guard
            .iter()
            .map(|s| (s.plugin_key.clone(), s.grant_revision.clone()))
            .collect(),
        Err(_) => return,
    };
    for (key, session_grant_revision) in snapshot {
        let current = store
            .get_by_plugin_key(&key)
            .map(grant_revision)
            .unwrap_or_default();
        if current != session_grant_revision {
            fence_stale_grant_revisions(&key, &current);
        }
    }
}

/// Live `(plugin_key, authority_revision)` pairs for overlay reconciliation.
#[must_use]
pub fn live_authority_snapshot() -> Vec<(String, String)> {
    match live().lock() {
        Ok(guard) => guard
            .iter()
            .map(|session| (session.plugin_key.clone(), session.revision.clone()))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Load `plugin-grants.json` and fence sessions that no longer match.
///
/// Unreadable / malformed files are left untouched for this tick so a
/// torn write cannot mass-fence. [`PluginGrantStore::save`] writes atomically.
pub fn reconcile_grants_from_disk(files_dir: &Path) {
    match PluginGrantStore::load(files_dir) {
        Ok(store) => apply_grant_store(&store),
        Err(err) => tracing::warn!(
            error = %err,
            "plugin grant store unreadable; not fencing until the next watch tick"
        ),
    }
}

/// SHA-256 of the grants file, or empty when it cannot be read.
fn grants_fingerprint(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            hex::encode(hasher.finalize())
        }
        Err(_) => String::new(),
    }
}

/// Watch `plugin-grants.json` until the process exits.
///
/// In-process [`notify_grants_changed`] wakes immediately; CLI writes in
/// another process are picked up within [`GRANT_WATCH_INTERVAL`].
pub async fn watch_grants_loop(files_dir: PathBuf, stop: Arc<AtomicBool>) {
    let path = PluginGrantStore::path(&files_dir);
    let mut last = String::new();
    match PluginGrantStore::load(&files_dir) {
        Ok(store) => {
            apply_grant_store(&store);
            last = grants_fingerprint(&path);
        }
        Err(err) => tracing::warn!(
            error = %err,
            "plugin grant store unreadable at watch start; retrying on later ticks"
        ),
    }
    while !stop.load(Ordering::SeqCst) {
        tokio::select! {
            () = tokio::time::sleep(GRANT_WATCH_INTERVAL) => {}
            () = grant_file_notify().notified() => {}
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let next = grants_fingerprint(&path);
        if next == last {
            continue;
        }
        match PluginGrantStore::load(&files_dir) {
            Ok(store) => {
                apply_grant_store(&store);
                last = next;
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "plugin grant store unreadable or malformed; keeping last-known-good \
                     authority and retrying on the next watch tick"
                );
            }
        }
    }
}

/// Spawn the daemon grant watcher (fire-and-forget).
///
/// # Returns
///
/// Stop flag; set it to end the loop (tests). Production callers leak the task
/// until process exit.
pub fn spawn_grant_watcher(files_dir: PathBuf) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_task = Arc::clone(&stop);
    tokio::spawn(async move {
        watch_grants_loop(files_dir, stop_task).await;
    });
    stop
}

/// Serializes tests that mutate the process-wide live session table.
#[cfg(test)]
pub(crate) struct TestLiveLock;

/// Occupied while a live-session test holds [`TestLiveLock`].
#[cfg(test)]
static TEST_LIVE_BUSY: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
impl Drop for TestLiveLock {
    fn drop(&mut self) {
        TEST_LIVE_BUSY.store(false, Ordering::SeqCst);
    }
}

/// Acquires the process-wide live-session test lock.
#[cfg(test)]
pub(crate) fn test_live_lock() -> TestLiveLock {
    while TEST_LIVE_BUSY
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    TestLiveLock
}

/// True when this cancel flag has been fenced.
#[must_use]
pub fn is_fenced(flag: &Arc<AtomicBool>) -> bool {
    flag.load(Ordering::SeqCst)
}

/// Error when a session's authority revision is no longer current.
pub fn fenced_error() -> PluginError {
    PluginError::message("plugin session fenced: effective authority changed")
}

/// Canonical SHA-256 of persisted operator consent (no host runtime overlays).
///
/// Used by the grant-file watcher and [`PluginGrantStore::upsert`] to detect
/// CLI/operator changes. Never compare this digest to [`authority_revision`].
#[must_use]
pub fn grant_revision(grant: &PluginGrant) -> String {
    hash_grant(b"grant", grant)
}

/// Canonical SHA-256 of security-relevant **effective** authority.
///
/// Hash the grant **after** manifest ∩ operator ∩ host policy and host-controlled
/// overlays (TCP/CIDR implied by configured destinations, clamped budgets).
/// Omits `approved_at` and display aliases except the PluginKey. Distinct from
/// [`crate::grant_revision`] and [`crate::ExecutorIdentity::configuration_revision`].
#[must_use]
pub fn authority_revision(grant: &PluginGrant) -> String {
    hash_grant(b"authority", grant)
}

/// SHA-256 of `grant` with a domain prefix (`grant` vs `authority`).
fn hash_grant(kind: &[u8], grant: &PluginGrant) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind);
    hasher.update(b"\n");
    hasher.update(grant.plugin_key.as_bytes());
    hasher.update(b"\nstructural\n");
    for e in &grant.entrypoints {
        hasher.update(e.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nproducers\n");
    for p in &grant.producers {
        hasher.update(p.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nconsumers\n");
    for c in &grant.consumers {
        hasher.update(c.event_type.as_bytes());
        hasher.update(b"|");
        for v in &c.schema_versions {
            hasher.update(v.to_string().as_bytes());
            hasher.update(b",");
        }
        hasher.update(if c.supports_suspend { b"s1|" } else { b"s0|" });
        if let Some(filter) = &c.filter {
            hasher.update(filter.as_bytes());
        }
        hasher.update(b";");
    }
    hasher.update(b"\njobs\n");
    for j in &grant.jobs {
        hasher.update(j.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nbindings\n");
    for b in &grant.bindings {
        hasher.update(b.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nflags\n");
    for f in &grant.compatibility_flags {
        hasher.update(f.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nnetwork\n");
    hasher.update(grant.network_mode.as_bytes());
    hasher.update(b"\ndomains\n");
    for d in &grant.domains {
        hasher.update(d.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nmanifest_domains\n");
    for d in &grant.manifest_domains {
        hasher.update(d.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\noperator_added\n");
    for d in &grant.operator_added_domains {
        hasher.update(d.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\noperator_denied\n");
    for d in &grant.operator_denied_domains {
        hasher.update(d.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\ntcp\n");
    hash_tcp_grants(&mut hasher, &grant.tcp);
    hasher.update(b"\noperator_added_tcp\n");
    hash_tcp_grants(&mut hasher, &grant.operator_added_tcp);
    hasher.update(b"\noperator_denied_tcp\n");
    hash_tcp_grants(&mut hasher, &grant.operator_denied_tcp);
    hasher.update(b"\ncidrs\n");
    for c in &grant.address_cidrs {
        hasher.update(c.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\noperator_added_cidrs\n");
    for c in &grant.operator_added_cidrs {
        hasher.update(c.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\noperator_denied_cidrs\n");
    for c in &grant.operator_denied_cidrs {
        hasher.update(c.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nredirects\n");
    hasher.update(
        u8::from(grant.allow_undeclared_public_redirects)
            .to_string()
            .as_bytes(),
    );
    hasher.update(b"\nbudgets\n");
    hasher.update(
        crate::consent::effective_disk_mib(grant.disk_mib)
            .to_string()
            .as_bytes(),
    );
    hasher.update(b"|mem|");
    hasher.update(
        crate::consent::effective_memory_mib(grant.memory_mib)
            .to_string()
            .as_bytes(),
    );
    hasher.update(b"|cpu_ms|");
    if let Some(cpu_ms) = grant.cpu_ms {
        hasher.update(cpu_ms.to_string().as_bytes());
    } else {
        hasher.update(b"unset");
    }
    hasher.update(b"|subrequests|");
    if let Some(subrequests) = grant.subrequests {
        hasher.update(subrequests.to_string().as_bytes());
    } else {
        hasher.update(b"unset");
    }
    hasher.update(b"|cpu_rate|");
    hasher.update(
        crate::consent::effective_cpu_rate_percent(grant.cpu_rate_percent)
            .to_string()
            .as_bytes(),
    );
    hasher.update(b"|extra_proc|");
    hasher.update(
        crate::consent::effective_extra_processes(grant.extra_processes)
            .to_string()
            .as_bytes(),
    );
    hex::encode(hasher.finalize())
}

/// Canonical digest of TCP grants (`host:port,…;`).
fn hash_tcp_grants(hasher: &mut Sha256, grants: &BTreeSet<bookclerk_plugin_manifest::TcpGrant>) {
    for t in grants {
        hasher.update(t.host.as_bytes());
        hasher.update(b":");
        for p in &t.ports {
            hasher.update(p.to_string().as_bytes());
            hasher.update(b",");
        }
        hasher.update(b";");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::io::Read;
    use std::net::Shutdown;
    use std::process::Command;
    use std::time::Instant;

    fn grant(domains: &[&str]) -> PluginGrant {
        PluginGrant {
            schema_version: crate::GRANT_SCHEMA_VERSION,
            plugin_key: "path:file:///tmp/demo".into(),
            plugin_id: "demo".into(),
            entrypoints: ["storefront".to_string()].into_iter().collect(),
            producers: BTreeSet::new(),
            consumers: Default::default(),
            jobs: Default::default(),
            network_mode: "outbound".into(),
            domains: domains.iter().map(|s| (*s).to_string()).collect(),
            manifest_domains: domains.iter().map(|s| (*s).to_string()).collect(),
            operator_added_domains: BTreeSet::new(),
            operator_denied_domains: BTreeSet::new(),
            bindings: ["config".to_string()].into_iter().collect(),
            compatibility_flags: BTreeSet::new(),
            cpu_ms: None,
            subrequests: None,
            disk_mib: None,
            memory_mib: None,
            cpu_rate_percent: None,
            extra_processes: None,
            approved_at: "2026-01-01T00:00:00Z".into(),
            ..PluginGrant::empty()
        }
    }

    #[test]
    fn metadata_does_not_change_authority_revision() {
        let a = grant(&["a.example"]);
        let mut b = a.clone();
        b.approved_at = "2099-01-01T00:00:00Z".into();
        b.plugin_id = "other-alias".into();
        assert_eq!(authority_revision(&a), authority_revision(&b));
    }

    #[test]
    fn operator_added_domain_changes_revision() {
        let a = grant(&["a.example"]);
        let mut b = a.clone();
        b.domains.insert("b.example".into());
        b.operator_added_domains.insert("b.example".into());
        assert_ne!(authority_revision(&a), authority_revision(&b));
    }

    #[test]
    fn consumer_and_job_changes_alter_authority_revision() {
        let a = grant(&["a.example"]);
        let mut b = a.clone();
        b.consumers.insert(crate::GrantedEventConsumer {
            event_type: "book_acquired".into(),
            schema_versions: vec![1],
            supports_suspend: false,
            filter: None,
        });
        assert_ne!(authority_revision(&a), authority_revision(&b));
        let mut c = a.clone();
        c.jobs.insert("stream_copy".into());
        assert_ne!(authority_revision(&a), authority_revision(&c));
    }

    #[test]
    fn fence_stale_sessions_keeps_current_revision() {
        let _lock = test_live_lock();
        let key = "path:file:///tmp/keep#demo";
        let current = register_session(key, "rev-new");
        let stale = register_session(key, "rev-old");
        fence_stale_sessions(key, "rev-new");
        assert!(!is_fenced(&current));
        assert!(is_fenced(&stale));
        unregister_session(&current);
        unregister_session(&stale);
    }

    #[test]
    fn resource_budgets_change_authority_revision() {
        let a = grant(&["a.example"]);
        for mutate in [
            |g: &mut PluginGrant| g.memory_mib = Some(128),
            |g: &mut PluginGrant| g.cpu_rate_percent = Some(25),
            |g: &mut PluginGrant| g.extra_processes = Some(1),
            |g: &mut PluginGrant| g.disk_mib = Some(64),
            |g: &mut PluginGrant| g.subrequests = Some(7),
            |g: &mut PluginGrant| g.cpu_ms = Some(1_000),
        ] {
            let mut b = a.clone();
            mutate(&mut b);
            assert_ne!(
                authority_revision(&a),
                authority_revision(&b),
                "budget mutation must change authority revision"
            );
        }
    }

    #[test]
    fn fence_invokes_shutdown_without_an_rpc() {
        let _lock = test_live_lock();
        let key = "path:file:///tmp/shutdown-hook";
        let ran = Arc::new(AtomicBool::new(false));
        let events_open = Arc::new(AtomicBool::new(true));
        let flag = register_session_with_shutdown(
            key,
            "rev-old",
            Arc::new({
                let ran = Arc::clone(&ran);
                let events_open = Arc::clone(&events_open);
                move || {
                    ran.store(true, Ordering::SeqCst);
                    events_open.store(false, Ordering::SeqCst);
                }
            }),
        );
        fence_stale_sessions(key, "rev-new");
        assert!(is_fenced(&flag));
        assert!(
            ran.load(Ordering::SeqCst),
            "shutdown hook must run immediately"
        );
        assert!(
            !events_open.load(Ordering::SeqCst),
            "EVENTS/capability channel must close on fence"
        );
        unregister_session(&flag);
    }

    #[test]
    fn apply_grant_store_fences_revoked_network_and_keeps_matching() {
        let _lock = test_live_lock();
        let key = "path:file:///tmp/apply-store";
        let mut approved = grant(&["a.example"]);
        approved.plugin_key = key.into();
        let matching_rev = grant_revision(&approved);
        let matching = register_session(key, &matching_rev);
        let stale = register_session(key, "rev-old");
        let mut store = PluginGrantStore::default();
        store.grants.push(approved);
        apply_grant_store(&store);
        assert!(!is_fenced(&matching));
        assert!(is_fenced(&stale));
        unregister_session(&matching);
        unregister_session(&stale);
    }

    #[test]
    fn tcp_and_cidr_changes_revision() {
        let a = grant(&["a.example"]);
        let mut b = a.clone();
        b.tcp.insert(bookclerk_plugin_manifest::TcpGrant {
            host: "db.example.com".into(),
            ports: vec![5432],
        });
        assert_ne!(authority_revision(&a), authority_revision(&b));
        let mut c = a.clone();
        c.address_cidrs.insert("10.0.60.100/32".into());
        assert_ne!(authority_revision(&a), authority_revision(&c));
        let mut d = a.clone();
        d.allow_undeclared_public_redirects = true;
        assert_ne!(authority_revision(&a), authority_revision(&d));
        let mut e = a.clone();
        e.operator_denied_tcp
            .insert(bookclerk_plugin_manifest::TcpGrant {
                host: "cdn.example.com".into(),
                ports: vec![443],
            });
        assert_ne!(authority_revision(&a), authority_revision(&e));
        let mut f = a.clone();
        f.operator_denied_cidrs.insert("10.0.0.0/8".into());
        assert_ne!(authority_revision(&a), authority_revision(&f));
    }

    fn write_grants_from_child(path: &Path, text: &str) {
        let incoming = path.with_extension("incoming.json");
        std::fs::write(&incoming, text).expect("incoming grants");
        let status = if cfg!(windows) {
            Command::new("cmd")
                .args([
                    "/C",
                    "copy",
                    "/Y",
                    incoming.to_str().expect("incoming path"),
                    path.to_str().expect("grants path"),
                ])
                .status()
                .expect("copy grants")
        } else {
            Command::new("cp")
                .arg(&incoming)
                .arg(path)
                .status()
                .expect("cp grants")
        };
        assert!(status.success(), "child failed to write grants: {status}");
    }

    #[tokio::test]
    async fn other_process_grant_write_fences_without_rpc() {
        let _lock = test_live_lock();
        let dir = tempfile::tempdir().expect("tmpdir");
        let key = "path:file:///tmp/cross-process#demo";
        let mut initial = grant(&["api.example"]);
        initial.plugin_key = key.into();
        let initial_grant_rev = grant_revision(&initial);
        let initial_auth_rev = authority_revision(&initial);
        let mut store = PluginGrantStore::default();
        store.grants.push(initial.clone());
        store.save(dir.path()).expect("save initial grants");
        let loaded = PluginGrantStore::load(dir.path()).expect("reload");
        assert_eq!(
            grant_revision(&loaded.grants[0]),
            initial_grant_rev,
            "grant file must round-trip grant_revision"
        );
        assert_ne!(
            initial_grant_rev, initial_auth_rev,
            "persisted grant_revision must not equal effective authority_revision"
        );

        let shutdown = Arc::new(AtomicBool::new(false));
        let events_open = Arc::new(AtomicBool::new(true));
        let flag = register_session_revisions(
            key,
            &initial_grant_rev,
            &initial_auth_rev,
            Arc::new({
                let shutdown = Arc::clone(&shutdown);
                let events_open = Arc::clone(&events_open);
                move || {
                    shutdown.store(true, Ordering::SeqCst);
                    events_open.store(false, Ordering::SeqCst);
                }
            }),
        );

        let stop = spawn_grant_watcher(dir.path().to_path_buf());
        tokio::time::sleep(GRANT_WATCH_INTERVAL).await;
        assert!(
            !is_fenced(&flag),
            "matching grant must not fence on watch start"
        );

        let mut revoked = initial;
        revoked.network_mode = "deny".into();
        revoked.domains.clear();
        revoked.tcp.clear();
        assert_ne!(grant_revision(&revoked), initial_grant_rev);
        let mut next = PluginGrantStore::default();
        next.grants.push(revoked);
        let text = serde_json::to_string_pretty(&next).expect("grants json");
        write_grants_from_child(&PluginGrantStore::path(dir.path()), &text);

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stop.store(true, Ordering::SeqCst);
        notify_grants_changed();
        tokio::time::sleep(GRANT_WATCH_INTERVAL).await;
        assert!(
            shutdown.load(Ordering::SeqCst),
            "CLI-equivalent child write must fence the daemon session without an RPC"
        );
        assert!(is_fenced(&flag));
        assert!(
            !events_open.load(Ordering::SeqCst),
            "non-network EVENTS channel must close on grant change"
        );
        unregister_session(&flag);
    }

    #[tokio::test]
    async fn idle_tcp_dies_when_grant_file_changes_from_child() {
        let _lock = test_live_lock();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("echo bind");
        let addr = listener.local_addr().expect("echo addr");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("echo accept");
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("read timeout");
            let mut buf = [0_u8; 8];
            Read::read(&mut stream, &mut buf)
        });
        let client = std::net::TcpStream::connect(addr).expect("idle connect");
        client.set_nodelay(true).ok();
        let client = Arc::new(client);

        let dir = tempfile::tempdir().expect("tmpdir");
        let key = "path:file:///tmp/idle-tcp#demo";
        let mut initial = grant(&["127.0.0.1"]);
        initial.plugin_key = key.into();
        initial.tcp.insert(bookclerk_plugin_manifest::TcpGrant {
            host: "127.0.0.1".into(),
            ports: vec![addr.port()],
        });
        initial.address_cidrs.insert("127.0.0.1/32".into());
        let initial_grant_rev = grant_revision(&initial);
        let initial_auth_rev = authority_revision(&initial);
        let mut store = PluginGrantStore::default();
        store.grants.push(initial.clone());
        store.save(dir.path()).expect("save");

        let flag = register_session_revisions(
            key,
            &initial_grant_rev,
            &initial_auth_rev,
            Arc::new({
                let client = Arc::clone(&client);
                move || {
                    let _ = client.shutdown(Shutdown::Both);
                }
            }),
        );
        let stop = spawn_grant_watcher(dir.path().to_path_buf());

        let mut revoked = initial;
        revoked.network_mode = "deny".into();
        revoked.domains.clear();
        revoked.tcp.clear();
        revoked.address_cidrs.clear();
        let mut next = PluginGrantStore::default();
        next.grants.push(revoked);
        write_grants_from_child(
            &PluginGrantStore::path(dir.path()),
            &serde_json::to_string_pretty(&next).expect("json"),
        );

        let n = tokio::task::spawn_blocking(move || server.join().expect("server thread"))
            .await
            .expect("join blocking");
        let n = n.expect("idle peer read");
        assert_eq!(
            n, 0,
            "idle TCP must EOF when the grant file changes; got {n} bytes (no plugin RPC)"
        );

        stop.store(true, Ordering::SeqCst);
        notify_grants_changed();
        tokio::time::sleep(GRANT_WATCH_INTERVAL).await;
        assert!(is_fenced(&flag));
        unregister_session(&flag);
    }

    #[test]
    fn grant_and_authority_revisions_are_distinct_digests() {
        let g = grant(&["api.example"]);
        assert_ne!(grant_revision(&g), authority_revision(&g));
        assert_eq!(grant_revision(&g).len(), 64);
        assert_eq!(authority_revision(&g).len(), 64);
    }

    #[test]
    fn host_tcp_overlay_does_not_change_persisted_grant_revision() {
        let _lock = test_live_lock();
        let key = "path:file:///tmp/overlay-postgres";
        let mut persisted = grant(&["api.example"]);
        persisted.plugin_key = key.into();
        persisted.network_mode = "outbound".into();
        let mut effective = persisted.clone();
        effective.tcp.insert(bookclerk_plugin_manifest::TcpGrant {
            host: "127.0.0.1".into(),
            ports: vec![5432],
        });
        effective.address_cidrs.insert("127.0.0.1/32".into());
        assert_ne!(
            authority_revision(&persisted),
            authority_revision(&effective),
            "host overlay TCP must change effective authority"
        );
        let flag = register_session_revisions(
            key,
            &grant_revision(&persisted),
            &authority_revision(&effective),
            Arc::new(|| {}),
        );
        let mut store = PluginGrantStore::default();
        store.grants.push(persisted);
        apply_grant_store(&store);
        assert!(
            !is_fenced(&flag),
            "unchanged persisted grant must not fence an overlaid postgres session"
        );
        unregister_session(&flag);
    }

    #[test]
    fn host_destination_change_fences_overlaid_session() {
        let _lock = test_live_lock();
        let key = "path:file:///tmp/overlay-dest-change";
        let mut persisted = grant(&["api.example"]);
        persisted.plugin_key = key.into();
        persisted.network_mode = "outbound".into();
        let mut effective = persisted.clone();
        effective.tcp.insert(bookclerk_plugin_manifest::TcpGrant {
            host: "127.0.0.1".into(),
            ports: vec![5432],
        });
        let flag = register_session_revisions(
            key,
            &grant_revision(&persisted),
            &authority_revision(&effective),
            Arc::new(|| {}),
        );
        let mut moved = persisted.clone();
        moved.tcp.insert(bookclerk_plugin_manifest::TcpGrant {
            host: "10.0.0.8".into(),
            ports: vec![5432],
        });
        fence_stale_sessions(key, &authority_revision(&moved));
        assert!(
            is_fenced(&flag),
            "host destination change must fence the overlaid session"
        );
        unregister_session(&flag);
    }

    #[test]
    fn persisted_grant_change_fences_overlaid_session() {
        let _lock = test_live_lock();
        let key = "path:file:///tmp/overlay-revoke";
        let mut persisted = grant(&["api.example"]);
        persisted.plugin_key = key.into();
        let mut effective = persisted.clone();
        effective.tcp.insert(bookclerk_plugin_manifest::TcpGrant {
            host: "127.0.0.1".into(),
            ports: vec![5432],
        });
        let flag = register_session_revisions(
            key,
            &grant_revision(&persisted),
            &authority_revision(&effective),
            Arc::new(|| {}),
        );
        persisted.network_mode = "deny".into();
        persisted.domains.clear();
        let mut store = PluginGrantStore::default();
        store.grants.push(persisted);
        apply_grant_store(&store);
        assert!(is_fenced(&flag));
        unregister_session(&flag);
    }

    #[tokio::test]
    async fn malformed_grant_file_is_retried_without_mass_fence() {
        let _lock = test_live_lock();
        let dir = tempfile::tempdir().expect("tmpdir");
        let key = "path:file:///tmp/malformed-grant";
        let mut initial = grant(&["api.example"]);
        initial.plugin_key = key.into();
        let grant_rev = grant_revision(&initial);
        let auth_rev = authority_revision(&initial);
        let mut store = PluginGrantStore::default();
        store.grants.push(initial.clone());
        store.save(dir.path()).expect("save");

        let flag = register_session_revisions(key, &grant_rev, &auth_rev, Arc::new(|| {}));
        let stop = spawn_grant_watcher(dir.path().to_path_buf());
        tokio::time::sleep(GRANT_WATCH_INTERVAL).await;
        assert!(
            !is_fenced(&flag),
            "valid grant must not fence on watch start"
        );

        write_grants_from_child(&PluginGrantStore::path(dir.path()), "{ not valid json");
        tokio::time::sleep(GRANT_WATCH_INTERVAL * 3).await;
        assert!(
            !is_fenced(&flag),
            "malformed grant file must keep last-known-good authority"
        );

        let mut next = PluginGrantStore::default();
        let mut revoked = initial;
        revoked.network_mode = "deny".into();
        revoked.domains.clear();
        next.grants.push(revoked);
        write_grants_from_child(
            &PluginGrantStore::path(dir.path()),
            &serde_json::to_string_pretty(&next).expect("json"),
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if is_fenced(&flag) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stop.store(true, Ordering::SeqCst);
        notify_grants_changed();
        assert!(
            is_fenced(&flag),
            "valid replacement after a malformed file must be observed"
        );
        unregister_session(&flag);
    }
}
