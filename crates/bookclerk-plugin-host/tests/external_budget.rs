//! Production-path memory acceptance for a jailed local destination.
//!
//! The process that owns the RPC client is moved into the cgroup before the
//! tokio runtime starts. `PluginSession` then launches `bookclerk-workerd` and
//! the sibling jail, so the host and every production descendant share one
//! memory budget. The conformance helper that spawns the guest with `Command`
//! is a partial measurement, not this test.
//!
//! ```text
//! BOOKCLERK_RESOURCE_ARTIFACT_DIR=/tmp/bounds cargo test -p bookclerk-plugin-host --test external_budget -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(unix)]
#![allow(clippy::missing_docs_in_private_items)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bookclerk_config::{Config, Isolation, Paths};
use bookclerk_plugin_host::{
    consent_request, discover_plugins, PluginGrantStore, PluginSession, PluginStorage,
    SessionServices, HOST_SHARED_ACCOUNT,
};
use bookclerk_plugin_sdk::{BindingValues, ExtensibleConfig, OutputLocalContextDto};
use bookclerk_storage::{ObjectMeta, StorageBackend};

const KEYS: u64 = 100_001;

#[test]
#[ignore = "cgroup acceptance: host plus production jailed siblings"]
fn external_budget_production_jail() {
    if std::env::var_os("BOOKCLERK_RESOURCE_IN_CGROUP").is_some() {
        let cgroup = PathBuf::from(
            std::env::var("BOOKCLERK_RESOURCE_CGROUP").expect("BOOKCLERK_RESOURCE_CGROUP"),
        );
        if let Err(err) =
            std::fs::write(cgroup.join("cgroup.procs"), std::process::id().to_string())
        {
            panic!(
                "unsupported: cannot move host pid {} into {}: {err}",
                std::process::id(),
                cgroup.display()
            );
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(run_measured(cgroup));
        return;
    }

    let cgroup = PathBuf::from(format!(
        "/sys/fs/cgroup/bookclerk-budget-{}",
        std::process::id()
    ));
    if let Err(err) = std::fs::create_dir(&cgroup) {
        panic!(
            "unsupported: cannot create child cgroup {}: {err}. Refusing a passing zero.",
            cgroup.display()
        );
    }
    let _remove = RemoveCgroup(cgroup.clone());
    let startup = 2 * 1024 * 1024 * 1024u64;
    std::fs::write(cgroup.join("memory.max"), format!("{startup}\n")).unwrap_or_else(|err| {
        panic!(
            "unsupported: cannot set memory.max on {}: {err}",
            cgroup.display()
        )
    });
    if cgroup.join("memory.swap.max").exists() {
        let _ = std::fs::write(cgroup.join("memory.swap.max"), "0\n");
    }
    let artifact = artifact_dir();
    std::fs::create_dir_all(&artifact).unwrap_or_else(|err| {
        panic!(
            "unsupported: cannot create artifact dir {}: {err}",
            artifact.display()
        )
    });
    let status = std::process::Command::new(std::env::current_exe().expect("current exe"))
        .args(std::env::args().skip(1))
        .env("BOOKCLERK_RESOURCE_IN_CGROUP", "1")
        .env("BOOKCLERK_RESOURCE_CGROUP", &cgroup)
        .env("BOOKCLERK_RESOURCE_ARTIFACT_DIR", &artifact)
        .status()
        .unwrap_or_else(|err| panic!("could not re-exec the acceptance harness: {err}"));
    assert!(
        status.success(),
        "acceptance harness exited {status}. Report dir: {}",
        artifact.display()
    );
}

struct RemoveCgroup(PathBuf);

impl Drop for RemoveCgroup {
    fn drop(&mut self) {
        if let Ok(text) = std::fs::read_to_string(self.0.join("cgroup.procs")) {
            for pid in text.split_whitespace() {
                let Ok(pid) = pid.parse::<i32>() else {
                    continue;
                };
                if pid != std::process::id() as i32 {
                    let _ = std::process::Command::new("kill")
                        .args(["-9", &pid.to_string()])
                        .status();
                }
            }
        }
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn artifact_dir() -> PathBuf {
    std::env::var_os("BOOKCLERK_RESOURCE_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("bookclerk-bounds-{}", std::process::id()))
        })
}

async fn run_measured(cgroup: PathBuf) {
    let _leave = LeaveCgroup(cgroup.clone());
    let Some(binary) = local_binary() else {
        panic!(
            "unsupported: bookclerk-plugin-destination-local is not built. \
             cargo build -p bookclerk-plugin-destination-local -p bookclerk-workerd -p bookclerk-jail"
        );
    };
    let caps = bookclerk_sandbox::capabilities();
    assert!(
        caps.filesystem,
        "unsupported: production jail cannot be enforced ({})",
        caps.detail
    );

    let files = tempfile::tempdir().expect("files");
    let plugin = stage_local(files.path(), &binary);
    let root = bookclerk_plugin_host::plugin_data_dir(
        &Config {
            paths: Some(Paths::from_files_dir(files.path().to_path_buf())),
            ..Config::default()
        },
        &plugin,
    )
    .expect("data dir")
    .join("library");
    std::fs::create_dir_all(&root).expect("output root");
    let mut config = Config {
        paths: Some(Paths::from_files_dir(files.path().to_path_buf())),
        ..Config::default()
    };
    config.plugins.isolation = Isolation::Required;
    config.output.local.enabled = true;
    config.output.local.root = root.clone();
    let mut grants = PluginGrantStore::default();
    grants.upsert(consent_request(&plugin.manifest, plugin.plugin_key()));
    grants.save(files.path()).expect("grants");

    let session = tokio::time::timeout(
        Duration::from_secs(180),
        PluginSession::spawn_with(
            &plugin,
            &config,
            serde_json::json!({}),
            HOST_SHARED_ACCOUNT,
            &[(
                "BOOKCLERK_OUTPUT_LOCAL_ROOT",
                std::ffi::OsString::from(root.as_os_str()),
            )],
            SessionServices::default(),
        ),
    )
    .await
    .expect("spawn timed out")
    .unwrap_or_else(|err| panic!("production spawn failed: {err}"));
    let gateway = session.gateway_pid().expect("gateway pid");
    let guest = session.guest_pid().expect("guest pid");
    // Domain controllers leave the parent `cgroup.procs` empty. Membership is
    // the `/proc/<pid>/cgroup` path staying under this budget directory,
    // including the host leaf the session mover creates.
    let budget_name = cgroup
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_string();
    let mut covered_paths = Vec::new();
    for (label, pid) in [
        ("host", std::process::id()),
        ("gateway", gateway),
        ("guest", guest),
    ] {
        let path = cgroup_path(pid);
        assert!(
            path.contains(&budget_name),
            "{label} pid {pid} cgroup `{path}` is outside {budget_name}"
        );
        covered_paths.push(format!("{label}={pid}:{path}"));
    }
    let mode = seccomp_mode(guest).unwrap_or(0);
    assert!(
        mode >= 2,
        "guest pid {guest} Seccomp={mode}; production jail did not enforce seccomp"
    );

    // File cache from copying the guest binary is reclaimable. Dropping it
    // lets `memory.max` sit near anonymous usage so a whole-object buffer
    // cannot fit, while a streaming write can reclaim cache under the cap.
    let _ = std::fs::write("/proc/sys/vm/drop_caches", b"1\n");
    let sample = memory_sample(&cgroup);
    let budget = sample
        .anon
        .saturating_add(256 * 1024 * 1024)
        .max(sample.current);
    std::fs::write(cgroup.join("memory.max"), format!("{budget}\n")).unwrap_or_else(|err| {
        panic!(
            "unsupported: cannot tighten memory.max to {budget} (current {} anon {}): {err}",
            sample.current, sample.anon
        )
    });
    let small = 8 * 1024 * 1024u64;
    let large = budget.saturating_add(64 * 1024 * 1024);
    assert!(large > budget, "object must exceed the cgroup budget");
    assert!(
        large / small >= 4,
        "object sizes are not materially different: small={small} large={large}"
    );

    let ctx = OutputLocalContextDto {
        plugin_data_dir: String::new(),
        root: String::new(),
        prefix: String::new(),
    };
    session
        .open(BindingValues::config(
            ExtensibleConfig::json_from(&ctx).expect("context"),
        ))
        .await
        .unwrap_or_else(|err| panic!("open local destination: {err}"));
    let storage = PluginStorage::new(Arc::new(session));

    let stop = Arc::new(AtomicBool::new(false));
    let anon_peak = Arc::new(AtomicU64::new(0));
    let file_peak = Arc::new(AtomicU64::new(0));
    let total_peak = Arc::new(AtomicU64::new(0));
    let sampler = sampler(
        cgroup.clone(),
        Arc::clone(&stop),
        Arc::clone(&anon_peak),
        Arc::clone(&file_peak),
        Arc::clone(&total_peak),
    );

    storage
        .put_stream(
            "small.bin",
            Box::pin(ZeroReader { left: small }),
            ObjectMeta {
                content_length: Some(small),
                ..ObjectMeta::default()
            },
        )
        .await
        .unwrap_or_else(|err| panic!("small put: {err}"));
    let small_anon = anon_peak.load(Ordering::SeqCst);

    storage
        .put_stream(
            "large.bin",
            Box::pin(ZeroReader { left: large }),
            ObjectMeta {
                content_length: Some(large),
                ..ObjectMeta::default()
            },
        )
        .await
        .unwrap_or_else(|err| panic!("large put: {err}"));
    let transfer_anon = anon_peak.load(Ordering::SeqCst);
    let transfer_file = file_peak.load(Ordering::SeqCst);
    let transfer_total = total_peak.load(Ordering::SeqCst);

    let large_path = root.join("large.bin");
    let on_disk = hash_file(&large_path);
    let expected = zero_digest(large);
    assert_eq!(
        on_disk.0, large,
        "destination length {} != source length {large}",
        on_disk.0
    );
    assert_eq!(
        on_disk.1, expected,
        "destination digest does not match the independently computed source digest"
    );
    assert!(
        transfer_anon.saturating_add(32 * 1024 * 1024) < large,
        "anonymous RSS {transfer_anon} tracked the {large} byte object (file cache {transfer_file}, cgroup peak {transfer_total})"
    );

    let bulk = root.join("bulk");
    std::fs::create_dir_all(&bulk).expect("bulk dir");
    let mut expected_keys = Vec::with_capacity(KEYS as usize);
    for index in 0..KEYS {
        let name = format!("k{index:06}.txt");
        std::fs::write(bulk.join(&name), b"x").expect("key file");
        expected_keys.push(format!("bulk/{name}"));
    }
    let mut listed = Vec::new();
    let mut cursor = None;
    loop {
        let page = storage
            .list_page("bulk/", cursor.as_deref(), 256)
            .await
            .unwrap_or_else(|err| panic!("list: {err}"));
        for obj in page.objects {
            listed.push(obj.key);
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(
        listed,
        expected_keys,
        "listed {} keys, expected {}; first mismatch at {:?}",
        listed.len(),
        expected_keys.len(),
        listed
            .iter()
            .zip(expected_keys.iter())
            .position(|(got, want)| got != want)
    );
    let list_anon = anon_peak.load(Ordering::SeqCst);
    let list_file = file_peak.load(Ordering::SeqCst);
    let list_total = total_peak.load(Ordering::SeqCst);
    stop.store(true, Ordering::SeqCst);
    let _ = sampler.join();

    let covered = format!(
        "re-exec inside {budget_name} before the runtime; {}; seccomp={mode}",
        covered_paths.join(" ")
    );
    let report = format!(
        "covered={covered}\nbudget={budget}\nsmall={small} small_anon={small_anon}\nlarge={large} transfer_anon={transfer_anon} transfer_file={transfer_file} transfer_cgroup_peak={transfer_total}\nlist_keys={KEYS} list_anon={list_anon} list_file={list_file} list_cgroup_peak={list_total}\ndigest_ok=source_file_sha256\n"
    );
    let artifact = artifact_dir().join("external-bounds.txt");
    let mut file = std::fs::File::create(&artifact)
        .unwrap_or_else(|err| panic!("cannot write {}: {err}", artifact.display()));
    write!(file, "{report}").unwrap();
    eprintln!("{report}");
    drop(storage);
}

struct LeaveCgroup(PathBuf);

impl Drop for LeaveCgroup {
    fn drop(&mut self) {
        if let Some(parent) = self.0.parent() {
            let _ = std::fs::write(parent.join("cgroup.procs"), std::process::id().to_string());
        }
    }
}

struct Sample {
    anon: u64,
    file: u64,
    current: u64,
    peak: u64,
}

fn memory_sample(cgroup: &Path) -> Sample {
    let stat = std::fs::read_to_string(cgroup.join("memory.stat")).unwrap_or_default();
    let mut anon = 0u64;
    let mut file = 0u64;
    for line in stat.lines() {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("anon"), Some(value)) => anon = value.parse().unwrap_or(0),
            (Some("file"), Some(value)) => file = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    let current = std::fs::read_to_string(cgroup.join("memory.current"))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0);
    let peak = std::fs::read_to_string(cgroup.join("memory.peak"))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0);
    Sample {
        anon,
        file,
        current,
        peak,
    }
}

fn sampler(
    cgroup: PathBuf,
    stop: Arc<AtomicBool>,
    anon_peak: Arc<AtomicU64>,
    file_peak: Arc<AtomicU64>,
    total_peak: Arc<AtomicU64>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let sample = memory_sample(&cgroup);
            anon_peak.fetch_max(sample.anon, Ordering::SeqCst);
            file_peak.fetch_max(sample.file, Ordering::SeqCst);
            total_peak.fetch_max(sample.peak.max(sample.current), Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(20));
        }
    })
}

fn cgroup_path(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default()
}

fn seccomp_mode(pid: u32) -> Option<u32> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Seccomp:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

fn local_binary() -> Option<PathBuf> {
    let name = "bookclerk-plugin-destination-local";
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(name));
            candidates.push(dir.join("..").join(name));
        }
    }
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    candidates.push(target.join("debug").join(name));
    candidates.into_iter().find(|path| path.is_file())
}

fn stage_local(files: &Path, binary: &Path) -> bookclerk_plugin_host::DiscoveredPlugin {
    let install = files.join("plugins").join("local");
    std::fs::create_dir_all(&install).expect("install");
    let dest = install.join("bookclerk-plugin-destination-local");
    std::fs::copy(binary, &dest).expect("copy guest");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&dest).expect("meta").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&dest, perms).expect("chmod");
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../bookclerk-plugins/platform/destination-local/plugin.toml");
    std::fs::copy(manifest, install.join("plugin.toml")).expect("manifest");
    discover_plugins(&Config {
        paths: Some(Paths::from_files_dir(files.to_path_buf())),
        ..Config::default()
    })
    .expect("discover")
    .into_iter()
    .find(|plugin| plugin.alias() == "local")
    .expect("local plugin")
}

fn hash_file(path: &Path) -> (u64, [u8; 32]) {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path).expect("open destination");
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = std::io::Read::read(&mut file, &mut buf).expect("read destination");
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    (total, hasher.finalize().into())
}

fn zero_digest(len: u64) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let zeros = [0u8; 8192];
    let mut left = len;
    while left > 0 {
        let n = std::cmp::min(left, zeros.len() as u64) as usize;
        hasher.update(&zeros[..n]);
        left -= n as u64;
    }
    hasher.finalize().into()
}

struct ZeroReader {
    left: u64,
}

impl tokio::io::AsyncRead for ZeroReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.left == 0 || buf.remaining() == 0 {
            return std::task::Poll::Ready(Ok(()));
        }
        let n = std::cmp::min(self.left, buf.remaining() as u64) as usize;
        let zeros = [0u8; 8192];
        let mut filled = 0;
        while filled < n {
            let chunk = std::cmp::min(zeros.len(), n - filled);
            buf.put_slice(&zeros[..chunk]);
            filled += chunk;
        }
        self.left -= n as u64;
        std::task::Poll::Ready(Ok(()))
    }
}
