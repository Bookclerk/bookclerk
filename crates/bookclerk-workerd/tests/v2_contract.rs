//! Dual-adapter contract: workerd `RpcTarget` guest over Bookclerk capnp stdio.
//!
//! CI must ship `target/debug/workerd`. Local native-only profiles may skip
//! with `BOOKCLERK_V2_SKIP_WORKERD=1`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use bookclerk_plugin_abi::v2::{
    connect_plugin, ByteRange, Destination, DestinationContext, JobInvocation, JobOutcome,
    ListOptions, ProgressSink, Source, StreamCopySpec, WorkerContext, WriteOptions,
    PRODUCT_API_VERSION,
};
use bookclerk_workerd::pin::binary_name;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const PAYLOAD: u64 = 2 * 1024 * 1024;

fn skip_workerd_allowed() -> bool {
    std::env::var("BOOKCLERK_V2_SKIP_WORKERD").ok().as_deref() == Some("1")
}

fn find_workerd() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("BOOKCLERK_WORKERD_BIN") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    let launcher = PathBuf::from(env!("CARGO_BIN_EXE_bookclerk-workerd"));
    if let Some(dir) = launcher.parent() {
        let candidate = dir.join(binary_name());
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn rss_kib(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next().and_then(|s| s.parse().ok());
        }
    }
    None
}

fn child_pids(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let task_dir = format!("/proc/{pid}/task");
    let Ok(entries) = std::fs::read_dir(&task_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let children = entry.path().join("children");
        if let Ok(text) = std::fs::read_to_string(children) {
            for tok in text.split_whitespace() {
                if let Ok(p) = tok.parse() {
                    out.push(p);
                }
            }
        }
    }
    out
}

fn tree_rss_kib(pid: u32) -> Option<u64> {
    let mut total = rss_kib(pid)?;
    for child in child_pids(pid) {
        total += tree_rss_kib(child).unwrap_or(0);
    }
    Some(total)
}

fn dir_size(path: &Path) -> u64 {
    let mut n = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            n += dir_size(&p);
        } else if let Ok(meta) = entry.metadata() {
            n += meta.len();
        }
    }
    n
}

struct PatternReader {
    remaining: u64,
    pos: u64,
}

impl tokio::io::AsyncRead for PatternReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let n = (self.remaining as usize)
            .min(buf.remaining())
            .min(64 * 1024);
        if n == 0 {
            return std::task::Poll::Ready(Ok(()));
        }
        for i in 0..n {
            buf.put_slice(&[((self.pos + i as u64) % 251) as u8]);
        }
        self.pos += n as u64;
        self.remaining -= n as u64;
        std::task::Poll::Ready(Ok(()))
    }
}

struct CountingDest {
    written: std::sync::Mutex<u64>,
}

#[async_trait::async_trait(?Send)]
impl Destination for CountingDest {
    async fn head(
        &self,
        _key: &str,
    ) -> bookclerk_plugin_abi::Result<Option<bookclerk_plugin_abi::v2::ObjectMetadata>> {
        Ok(None)
    }
    async fn list(
        &self,
        _options: ListOptions,
    ) -> bookclerk_plugin_abi::Result<bookclerk_plugin_abi::v2::ListPage> {
        Ok(bookclerk_plugin_abi::v2::ListPage::default())
    }
    async fn get(
        &self,
        key: &str,
        _range: Option<ByteRange>,
    ) -> bookclerk_plugin_abi::Result<bookclerk_plugin_abi::v2::ReadResult> {
        let size: u64 = key
            .strip_prefix("pattern:")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Ok(bookclerk_plugin_abi::v2::ReadResult {
            meta: bookclerk_plugin_abi::v2::ObjectMetadata {
                key: key.into(),
                size,
                ..Default::default()
            },
            body: Box::pin(PatternReader {
                remaining: size,
                pos: 0,
            }),
        })
    }
    async fn put(
        &self,
        key: &str,
        mut body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        _options: WriteOptions,
    ) -> bookclerk_plugin_abi::Result<bookclerk_plugin_abi::v2::PutResult> {
        let mut n = 0u64;
        let mut buf = [0u8; 65536];
        loop {
            let read = body.read(&mut buf).await.map_err(|err| {
                bookclerk_plugin_abi::PluginError::internal(format!("read: {err}"))
            })?;
            if read == 0 {
                break;
            }
            n += read as u64;
        }
        *self.written.lock().expect("lock") = n;
        Ok(bookclerk_plugin_abi::v2::PutResult {
            key: key.into(),
            bytes_written: n,
            ..Default::default()
        })
    }
    async fn copy(
        &self,
        _from: &str,
        _to: &str,
    ) -> bookclerk_plugin_abi::Result<bookclerk_plugin_abi::v2::CopyResult> {
        Err(bookclerk_plugin_abi::PluginError::unsupported("copy"))
    }
    async fn delete(&self, _key: &str) -> bookclerk_plugin_abi::Result<()> {
        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl Source for CountingDest {
    async fn open(
        &self,
        key: &str,
    ) -> bookclerk_plugin_abi::Result<bookclerk_plugin_abi::v2::ReadResult> {
        Destination::get(self, key, None).await
    }
}

struct NoopProgress;

#[async_trait::async_trait(?Send)]
impl ProgressSink for NoopProgress {
    async fn report(&self, _percent: f32, _message: &str) -> bookclerk_plugin_abi::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn workerd_v2_stream_and_job_handler_contract() {
    let Some(workerd) = find_workerd() else {
        if skip_workerd_allowed() {
            eprintln!("skipping workerd v2 contract (BOOKCLERK_V2_SKIP_WORKERD=1)");
            return;
        }
        panic!(
            "pinned workerd binary missing; run `cargo ensure-workerd` / `cargo build-app --platform`. \
             Local native-only skip: BOOKCLERK_V2_SKIP_WORKERD=1"
        );
    };

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v2-stream");
    assert!(
        fixture.join("plugin.toml").is_file(),
        "missing fixture {}",
        fixture.display()
    );
    let tmp = tempfile::tempdir().expect("tmpdir");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut child = Command::new(env!("CARGO_BIN_EXE_bookclerk-workerd"))
                .env("BOOKCLERK_PLUGIN_ROOT", &fixture)
                .env("BOOKCLERK_WORKERD_BIN", &workerd)
                .env("TMPDIR", tmp.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn bookclerk-workerd");
            let stdin = child.stdin.take().expect("stdin");
            let stdout = child.stdout.take().expect("stdout");
            let launcher_pid = child.id().expect("pid");
            let (client, rpc) = connect_plugin(stdout, stdin, 64 * 1024);
            tokio::task::spawn_local(rpc);

            let desc = tokio::time::timeout(Duration::from_secs(90), client.describe())
                .await
                .expect("describe timed out — workerd adapter failed to start")
                .expect("describe");
            assert_eq!(desc.api_version, PRODUCT_API_VERSION);
            assert_eq!(desc.id, "v2_stream");
            let display = desc.display_name.clone().unwrap_or_default();
            assert!(
                !display.contains("GRANTED") && !display.contains("BRIDGE_TOKEN"),
                "author env must not include adapter-private bindings: {display}"
            );
            assert!(
                !display.contains("stubs="),
                "invocation-scoped adapter must not retain dest-id maps: {display}"
            );

            let dest = client
                .destination(DestinationContext::default())
                .await
                .expect("destination");
            tokio::time::timeout(
                Duration::from_secs(30),
                dest.put(
                    "hello",
                    Box::pin(std::io::Cursor::new(b"abc".to_vec())),
                    WriteOptions::default(),
                ),
            )
            .await
            .expect("small put timed out")
            .expect("small put");
            let got = dest.get("hello", None).await.expect("small get");
            let mut buf = Vec::new();
            let mut body = got.body;
            body.read_to_end(&mut buf).await.unwrap();
            assert_eq!(buf, b"abc");

            let after_create = tokio::time::timeout(Duration::from_secs(15), client.describe())
                .await
                .expect("describe after dest")
                .expect("describe after dest");
            let after_create_display = after_create.display_name.unwrap_or_default();
            assert!(
                !after_create_display.contains("stubs="),
                "no dest-id map after put: {after_create_display}"
            );
            drop(dest);
            tokio::time::sleep(Duration::from_millis(150)).await;
            let dest = client
                .destination(DestinationContext::default())
                .await
                .expect("destination after drop");
            let got_again = dest.get("hello", None).await.expect("get after dest drop");
            let mut buf2 = Vec::new();
            let mut body2 = got_again.body;
            body2.read_to_end(&mut buf2).await.unwrap();
            assert_eq!(buf2, b"abc", "module state survives dest drop");
            let src = client
                .source(bookclerk_plugin_abi::v2::SourceContext::default())
                .await
                .expect("source");
            drop(src);
            tokio::time::sleep(Duration::from_millis(150)).await;

            let before = tree_rss_kib(launcher_pid);
            let put = tokio::time::timeout(
                Duration::from_secs(120),
                dest.put(
                    "count:out",
                    Box::pin(PatternReader {
                        remaining: PAYLOAD,
                        pos: 0,
                    }),
                    WriteOptions {
                        content_length: Some(PAYLOAD),
                        ..Default::default()
                    },
                ),
            )
            .await
            .expect("large put timed out")
            .expect("large put");
            assert_eq!(put.bytes_written, PAYLOAD);
            if let (Some(before), Some(after)) = (before, tree_rss_kib(launcher_pid)) {
                let grew = after.saturating_sub(before);
                assert!(
                    grew < 24 * 1024,
                    "workerd+launcher RSS grew {grew} KiB during {PAYLOAD} byte stream (budget 24 MiB)"
                );
            }

            let mut n = 0u64;
            let got = dest
                .get(&format!("pattern:{PAYLOAD}"), None)
                .await
                .expect("pattern get");
            let mut body = got.body;
            let mut chunk = [0u8; 65536];
            loop {
                let r = body.read(&mut chunk).await.unwrap();
                if r == 0 {
                    break;
                }
                n += r as u64;
            }
            assert_eq!(n, PAYLOAD);

            let scratch = dir_size(tmp.path());
            assert!(
                scratch < 8 * 1024 * 1024,
                "scratch under TMPDIR grew to {scratch} bytes"
            );

            let handler = client
                .worker(WorkerContext {
                    job_id: "contract".into(),
                    ..Default::default()
                })
                .await
                .expect("worker");
            let granted = Arc::new(CountingDest {
                written: std::sync::Mutex::new(0),
            });
            let invocation = JobInvocation::stream_copy(
                "contract",
                serde_json::to_string(&StreamCopySpec {
                    from: "pattern:4096".into(),
                    to: "count:copy".into(),
                })
                .unwrap(),
            );
            let outcome: JobOutcome = tokio::time::timeout(
                Duration::from_secs(60),
                client.handle_job(
                    handler.clone(),
                    invocation.clone(),
                    granted.clone() as Arc<dyn Source>,
                    granted.clone() as Arc<dyn Destination>,
                    Arc::new(NoopProgress),
                ),
            )
            .await
            .expect("handle timed out")
            .expect("handle");
            match outcome {
                JobOutcome::Completed { bytes_copied, .. } => {
                    assert_eq!(bytes_copied, 4096);
                }
                other => panic!("expected completed, got {other:?}"),
            }
            assert_eq!(*granted.written.lock().expect("lock"), 4096);

            let second = tokio::time::timeout(
                Duration::from_secs(15),
                client.handle_job(
                    handler,
                    invocation,
                    granted.clone() as Arc<dyn Source>,
                    granted.clone() as Arc<dyn Destination>,
                    Arc::new(NoopProgress),
                ),
            )
            .await
            .expect("second handle timed out")
            .expect("second handle is a new invocation");
            match second {
                JobOutcome::Completed { bytes_copied, .. } => {
                    assert_eq!(bytes_copied, 4096);
                }
                other => panic!("expected completed, got {other:?}"),
            }

            let typed = tokio::time::timeout(Duration::from_secs(15), dest.head("internal-msg:x"))
                .await
                .expect("typed error timed out");
            let err = typed.expect_err("internal-msg must fail");
            assert_eq!(err.code, bookclerk_plugin_abi::PluginErrorCode::Internal);
            assert!(err.message.contains("not_found"));

            let unknown = tokio::time::timeout(Duration::from_secs(15), dest.head("unknown-code:x"))
                .await
                .expect("unknown code timed out")
                .expect_err("unknown-code must fail");
            assert_eq!(unknown.code, bookclerk_plugin_abi::PluginErrorCode::Unknown);
            assert_eq!(unknown.wire_str(), "future_retry_policy");

            let overflow = tokio::time::timeout(
                Duration::from_secs(15),
                dest.list(ListOptions {
                    prefix: "overflow:".into(),
                    ..Default::default()
                }),
            )
            .await
            .expect("overflow list timed out")
            .expect_err("oversize page");
            assert_eq!(
                overflow.code,
                bookclerk_plugin_abi::PluginErrorCode::PayloadTooLarge
            );

            struct FailAfter {
                remain: usize,
            }
            impl tokio::io::AsyncRead for FailAfter {
                fn poll_read(
                    mut self: std::pin::Pin<&mut Self>,
                    _cx: &mut std::task::Context<'_>,
                    buf: &mut tokio::io::ReadBuf<'_>,
                ) -> std::task::Poll<std::io::Result<()>> {
                    if self.remain == 0 {
                        return std::task::Poll::Ready(Err(std::io::Error::other("source exploded")));
                    }
                    let n = self.remain.min(buf.remaining()).min(4);
                    buf.put_slice(&vec![b'x'; n]);
                    self.remain -= n;
                    std::task::Poll::Ready(Ok(()))
                }
            }
            let put_err = tokio::time::timeout(
                Duration::from_secs(15),
                dest.put(
                    "hello",
                    Box::pin(FailAfter { remain: 8 }),
                    WriteOptions {
                        content_length: Some(100),
                        ..Default::default()
                    },
                ),
            )
            .await
            .expect("failing put timed out");
            assert!(put_err.is_err(), "mid-stream put failure must not publish");
            let kept = dest.get("hello", None).await.expect("hello kept");
            let mut buf = Vec::new();
            let mut body = kept.body;
            body.read_to_end(&mut buf).await.unwrap();
            assert_eq!(buf, b"abc");

            drop(client);
            let _ = child.kill().await;
            let _ = child.wait().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn workerd_v2_separate_instances_do_not_share_objects() {
    let Some(workerd) = find_workerd() else {
        if skip_workerd_allowed() {
            eprintln!("skipping workerd v2 isolation (BOOKCLERK_V2_SKIP_WORKERD=1)");
            return;
        }
        panic!("pinned workerd binary missing");
    };
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v2-stream");
    let tmp_a = tempfile::tempdir().expect("tmp a");
    let tmp_b = tempfile::tempdir().expect("tmp b");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut a = Command::new(env!("CARGO_BIN_EXE_bookclerk-workerd"))
                .env("BOOKCLERK_PLUGIN_ROOT", &fixture)
                .env("BOOKCLERK_WORKERD_BIN", &workerd)
                .env("TMPDIR", tmp_a.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn a");
            let mut b = Command::new(env!("CARGO_BIN_EXE_bookclerk-workerd"))
                .env("BOOKCLERK_PLUGIN_ROOT", &fixture)
                .env("BOOKCLERK_WORKERD_BIN", &workerd)
                .env("TMPDIR", tmp_b.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn b");
            let (client_a, rpc_a) = connect_plugin(
                a.stdout.take().expect("stdout a"),
                a.stdin.take().expect("stdin a"),
                64 * 1024,
            );
            let (client_b, rpc_b) = connect_plugin(
                b.stdout.take().expect("stdout b"),
                b.stdin.take().expect("stdin b"),
                64 * 1024,
            );
            tokio::task::spawn_local(rpc_a);
            tokio::task::spawn_local(rpc_b);
            let dest_a = tokio::time::timeout(
                Duration::from_secs(90),
                client_a.destination(DestinationContext::default()),
            )
            .await
            .expect("a dest timeout")
            .expect("a dest");
            let dest_b = tokio::time::timeout(
                Duration::from_secs(90),
                client_b.destination(DestinationContext::default()),
            )
            .await
            .expect("b dest timeout")
            .expect("b dest");
            dest_a
                .put(
                    "secret-a",
                    Box::pin(std::io::Cursor::new(b"only-a".to_vec())),
                    WriteOptions::default(),
                )
                .await
                .expect("put a");
            let missing = dest_b.head("secret-a").await.expect("head b");
            assert!(
                missing.is_none(),
                "separate instances must not share dest objects"
            );
            let pid_a = a.id().expect("pid a");
            let pid_b = b.id().expect("pid b");
            assert_ne!(
                pid_a, pid_b,
                "distinct account/instance workerd launchers must be distinct processes"
            );
            drop(client_a);
            drop(client_b);
            let _ = a.kill().await;
            let _ = b.kill().await;
        })
        .await;
}
