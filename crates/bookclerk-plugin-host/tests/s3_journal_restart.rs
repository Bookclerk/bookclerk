//! Confined destination-s3 guest: kill during multipart upload, restart, abort.
//!
//! The in-process journal test drops a backend inside one process. This test
//! uses [`PluginSession`] (workerd front door and `bookclerk-jail`) and
//! `SIGKILL`, so the guest's `Drop` abort does not run.

#![cfg(target_os = "linux")]
#![allow(clippy::missing_docs_in_private_items)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bookclerk_config::{Config, Isolation, Paths};
use bookclerk_plugin_host::{
    consent_request, discover_plugins, PluginGrantStore, PluginSession, PluginStorage,
    SessionServices, HOST_SHARED_ACCOUNT,
};
use bookclerk_plugin_sdk::{BindingValues, ExtensibleConfig, OutputS3ContextDto, S3CredentialsDto};
use bookclerk_storage::{ObjectMeta, StorageBackend};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

struct World {
    uploads: HashMap<String, String>,
    aborts: Vec<String>,
    parts: usize,
    hold_part: AtomicBool,
}

async fn serve(world: Arc<Mutex<World>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let world = Arc::clone(&world);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 64 * 1024];
                let mut collected = Vec::new();
                let header_end = loop {
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    collected.extend_from_slice(&buf[..n]);
                    if let Some(pos) = collected.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let head = String::from_utf8_lossy(&collected[..header_end]).to_string();
                let mut lines = head.split("\r\n");
                let request = lines.next().unwrap_or("");
                let mut parts = request.split_whitespace();
                let method = parts.next().unwrap_or("");
                let target = parts.next().unwrap_or("/");
                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                let mut headers = HashMap::new();
                for line in lines {
                    if let Some((name, value)) = line.split_once(':') {
                        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                    }
                }
                let content_len = headers
                    .get("content-length")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                let mut body = collected[header_end..].to_vec();
                while body.len() < content_len {
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&buf[..n]);
                }
                let q = query_map(query);
                let response = if method == "POST" && q.contains_key("uploads") {
                    let mut world = world.lock().await;
                    let id = format!("up-{}", world.uploads.len() + 1);
                    world.uploads.insert(id.clone(), path.to_string());
                    let xml = format!(
                        "<?xml version=\"1.0\"?><InitiateMultipartUploadResult><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"
                    );
                    xml_ok(&xml)
                } else if method == "PUT"
                    && q.contains_key("uploadId")
                    && q.contains_key("partNumber")
                {
                    {
                        let mut world = world.lock().await;
                        world.parts += 1;
                    }
                    if world.lock().await.hold_part.load(Ordering::SeqCst) {
                        std::future::pending::<()>().await;
                    }
                    "HTTP/1.1 200 OK\r\nETag: \"part-1\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .into()
                } else if method == "DELETE" && q.contains_key("uploadId") {
                    let id = q.get("uploadId").cloned().unwrap_or_default();
                    let mut world = world.lock().await;
                    if world.uploads.remove(&id).is_some() {
                        world.aborts.push(id);
                        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .into()
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .into()
                    }
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
                };
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

fn query_map(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (k.to_string(), v.to_string())
        })
        .collect()
}

fn xml_ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn guest_binary() -> Option<PathBuf> {
    let name = "bookclerk-plugin-destination-s3";
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

fn stage_plugin(files: &Path, binary: &Path) -> bookclerk_plugin_host::DiscoveredPlugin {
    let install = files.join("plugins").join("s3");
    std::fs::create_dir_all(&install).expect("install dir");
    let dest = install.join("bookclerk-plugin-destination-s3");
    std::fs::copy(binary, &dest).expect("copy guest");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms).expect("chmod");
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../bookclerk-plugins/optional/destination-s3/plugin.toml");
    std::fs::copy(manifest, install.join("plugin.toml")).expect("plugin.toml");
    discover_plugins(&Config {
        paths: Some(Paths::from_files_dir(files.to_path_buf())),
        ..Config::default()
    })
    .expect("discover")
    .into_iter()
    .find(|plugin| plugin.alias() == "s3")
    .expect("s3 plugin")
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

async fn open_s3(session: &PluginSession, endpoint: &str) {
    let ctx = OutputS3ContextDto {
        plugin_data_dir: String::new(),
        bucket: "library".into(),
        prefix: String::new(),
        region: "us-east-1".into(),
        endpoint: Some(endpoint.into()),
        force_path_style: true,
        credentials: Some(S3CredentialsDto {
            access_key_id: "test-key".into(),
            secret_access_key: "test-secret".into(),
            session_token: None,
        }),
    };
    session
        .open(BindingValues::config(
            ExtensibleConfig::json_from(&ctx).expect("context json"),
        ))
        .await
        .unwrap_or_else(|err| panic!("open s3 destination: {err}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_jailed_s3_guest_aborts_its_upload_on_restart() {
    let Some(binary) = guest_binary() else {
        panic!(
            "unsupported: bookclerk-plugin-destination-s3 is not built. \
             cargo build -p bookclerk-plugin-destination-s3 -p bookclerk-workerd -p bookclerk-jail"
        );
    };
    let caps = bookclerk_sandbox::capabilities();
    assert!(
        caps.filesystem,
        "unsupported: this host cannot enforce the production jail ({})",
        caps.detail
    );
    let world = Arc::new(Mutex::new(World {
        uploads: HashMap::new(),
        aborts: Vec::new(),
        parts: 0,
        hold_part: AtomicBool::new(true),
    }));
    let endpoint = serve(Arc::clone(&world)).await;
    let files = tempfile::tempdir().expect("files");
    let plugin = stage_plugin(files.path(), &binary);
    let mut config = Config {
        paths: Some(Paths::from_files_dir(files.path().to_path_buf())),
        ..Config::default()
    };
    config.plugins.isolation = Isolation::Required;
    config.output.s3.enabled = true;
    config.output.s3.bucket = "library".into();
    config.output.s3.prefix.clear();
    config.output.s3.endpoint = Some(endpoint.clone());
    config.output.s3.force_path_style = true;
    config.output.s3.region = "us-east-1".into();
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
            &[],
            SessionServices::default(),
        ),
    )
    .await
    .expect("spawn timed out")
    .unwrap_or_else(|err| panic!("production spawn failed: {err}"));
    let guest = session.guest_pid().expect("guest pid");
    let mode = seccomp_mode(guest).unwrap_or(0);
    assert!(
        mode >= 2,
        "guest pid {guest} is not seccomp-confined (Seccomp={mode}); this is not the production jail"
    );
    let journal = session.data_dir().join("multipart-journal");
    open_s3(&session, &endpoint).await;
    let storage = PluginStorage::new(std::sync::Arc::new(session));
    let upload = tokio::spawn({
        let storage = storage.clone();
        async move {
            storage
                .put_stream(
                    "book.m4b",
                    Box::pin(std::io::Cursor::new(b"abcdefghij".to_vec())),
                    ObjectMeta {
                        content_length: Some(10),
                        ..ObjectMeta::default()
                    },
                )
                .await
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if world.lock().await.parts >= 1 {
            break;
        }
        if upload.is_finished() {
            panic!("upload ended before a part was stored: {:?}", upload.await);
        }
        if std::time::Instant::now() > deadline {
            panic!("guest never uploaded a part");
        }
        tokio::task::yield_now().await;
    }
    assert!(
        journal_records(&journal) > 0,
        "multipart journal missing under {}",
        journal.display()
    );
    assert_eq!(world.lock().await.aborts.len(), 0);
    let _ = std::process::Command::new("kill")
        .args(["-9", &guest.to_string()])
        .status();
    let _ = tokio::time::timeout(Duration::from_secs(30), upload).await;
    drop(storage);

    let restarted = tokio::time::timeout(
        Duration::from_secs(180),
        PluginSession::spawn_with(
            &plugin,
            &config,
            serde_json::json!({}),
            HOST_SHARED_ACCOUNT,
            &[],
            SessionServices::default(),
        ),
    )
    .await
    .expect("restart timed out")
    .unwrap_or_else(|err| panic!("restart spawn failed: {err}"));
    open_s3(&restarted, &endpoint).await;
    assert_eq!(
        world.lock().await.aborts.len(),
        1,
        "restart did not abort the upload left by the killed guest"
    );
}

fn journal_records(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("uploads"))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                .count()
        })
        .unwrap_or(0)
}
