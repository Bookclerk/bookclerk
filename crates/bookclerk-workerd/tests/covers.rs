//! `CoverFetcher.fetchCovers` through the workerd launcher: the host sink
//! reaches the isolate as a put-only grant, and the isolate may stream
//! several bodies into it at once.
//!
//! CI must ship `target/debug/workerd`. Local native-only profiles may skip
//! with `BOOKCLERK_SKIP_WORKERD=1`.

// Test code: unwrap/expect are fine here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::rc::Rc;
use std::time::Duration;

use bookclerk_plugin_abi::{
    connect_plugin, ByteRange, CopyResult, CoverFetchParams, CoverFetcher, Destination,
    HostBindings, Invocation, ListOptions, ListPage, ObjectMetadata, PluginError, PutResult,
    ReadResult, WriteOptions,
};
use bookclerk_workerd::pin::binary_name;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

fn skip_workerd_allowed() -> bool {
    std::env::var("BOOKCLERK_SKIP_WORKERD").as_deref() == Ok("1")
}

fn find_workerd() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("BOOKCLERK_WORKERD_BIN") {
        if let Ok(path) = bookclerk_sandbox::require_spawn_executable(Path::new(&p)) {
            return Some(path);
        }
    }
    let launcher = PathBuf::from(env!("CARGO_BIN_EXE_bookclerk-workerd"));
    launcher
        .parent()
        .and_then(|dir| bookclerk_sandbox::require_spawn_executable(&dir.join(binary_name())).ok())
}

/// In-memory sink that records each put; refuses bodies over `max` and a
/// second put under one key, as the host's cover sink does.
struct MemSink {
    max: u64,
    puts: Rc<RefCell<BTreeMap<String, (Vec<u8>, Option<String>)>>>,
}

#[async_trait::async_trait(?Send)]
impl Destination for MemSink {
    async fn head(&self, _key: &str) -> Result<Option<ObjectMetadata>, PluginError> {
        Err(PluginError::unsupported("head"))
    }

    async fn list(&self, _options: ListOptions) -> Result<ListPage, PluginError> {
        Err(PluginError::unsupported("list"))
    }

    async fn get(&self, _key: &str, _range: Option<ByteRange>) -> Result<ReadResult, PluginError> {
        Err(PluginError::unsupported("get"))
    }

    async fn put(
        &self,
        key: &str,
        body: Pin<Box<dyn AsyncRead + Send>>,
        options: WriteOptions,
    ) -> Result<PutResult, PluginError> {
        if self.puts.borrow().contains_key(key) {
            return Err(PluginError::invalid_params(format!(
                "{key} already written"
            )));
        }
        let mut bytes = Vec::new();
        body.take(self.max + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|err| PluginError::internal(err.to_string()))?;
        if bytes.len() as u64 > self.max {
            return Err(PluginError::payload_too_large("cover body too large"));
        }
        let size = bytes.len() as u64;
        self.puts
            .borrow_mut()
            .insert(key.to_string(), (bytes, options.content_type));
        Ok(PutResult {
            key: key.to_string(),
            bytes_written: size,
            ..PutResult::default()
        })
    }

    async fn copy(&self, _from: &str, _to: &str) -> Result<CopyResult, PluginError> {
        Err(PluginError::unsupported("copy"))
    }

    async fn delete(&self, _key: &str) -> Result<(), PluginError> {
        Err(PluginError::unsupported("delete"))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn workerd_cover_fetcher_streams_bodies_into_the_host_sink() {
    let Some(workerd) = find_workerd() else {
        if skip_workerd_allowed() {
            eprintln!("skipping workerd covers contract (BOOKCLERK_SKIP_WORKERD=1)");
            return;
        }
        panic!(
            "pinned workerd binary missing; run `cargo ensure-workerd`. \
             Local native-only skip: BOOKCLERK_SKIP_WORKERD=1"
        );
    };
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/covers");
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
            let (client, rpc) = connect_plugin(stdout, stdin, 64 * 1024);
            tokio::task::spawn_local(rpc);
            tokio::time::timeout(Duration::from_secs(90), client.describe())
                .await
                .expect("describe timed out")
                .expect("describe");
            let covers = client
                .open(
                    &Invocation {
                        id: "covers".into(),
                        ..Default::default()
                    },
                    HostBindings::default(),
                )
                .await
                .expect("open")
                .covers
                .expect("guest exports `covers`");

            let health = covers.health().await.expect("health");
            assert!(health.ok);

            let puts = Rc::new(RefCell::new(BTreeMap::new()));
            let urls: Vec<String> = [
                "https://a.example/0",
                "https://a.example/missing",
                "https://a.example/2",
                "https://a.example/big",
                "https://a.example/twice",
                "https://a.example/5",
            ]
            .iter()
            .map(|u| (*u).to_string())
            .collect();
            let images = tokio::time::timeout(
                Duration::from_secs(30),
                covers.fetch_covers(
                    CoverFetchParams {
                        urls: urls.clone(),
                        max_bytes: 64,
                    },
                    Box::new(MemSink {
                        max: 64,
                        puts: Rc::clone(&puts),
                    }),
                ),
            )
            .await
            .expect("fetchCovers timed out")
            .expect("fetchCovers");
            assert_eq!(
                images.iter().map(|i| i.url.clone()).collect::<Vec<_>>(),
                urls
            );
            let stored: Vec<bool> = images.iter().map(|i| i.stored).collect();
            assert_eq!(stored, [true, false, true, false, false, true]);
            assert_eq!(images[1].error.as_deref(), Some("HTTP 404"));
            assert!(
                images[3].error.as_deref().is_some_and(|e| !e.is_empty()),
                "an oversized body fails that URL: {:?}",
                images[3]
            );
            assert!(
                images[4].error.as_deref().is_some_and(|e| !e.is_empty()),
                "a second put under one key fails: {:?}",
                images[4]
            );

            let puts = puts.borrow();
            for index in ["0", "2", "5"] {
                let (body, content_type) = puts.get(index).expect("stored body");
                assert_eq!(body, format!("body:{index}").as_bytes());
                assert_eq!(content_type.as_deref(), Some("image/png"));
            }
            assert_eq!(
                puts.get("4").map(|(b, _)| b.as_slice()),
                Some(&b"first"[..])
            );
            assert!(!puts.contains_key("3"), "the oversized body was refused");
            let _ = child.kill().await;
        })
        .await;
}
