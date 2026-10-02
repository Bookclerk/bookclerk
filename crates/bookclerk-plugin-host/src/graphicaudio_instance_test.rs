//! #105: GraphicAudio SCAN uses the instance CONFIG document, not TOML or
//! `BOOKCLERK_GA_ACCESS`. Login without a password never reaches HTTP, so the
//! device-versus-web proof is the Access App catalog request.

#![allow(clippy::missing_docs_in_private_items)]

use std::path::{Path, PathBuf};

use bookclerk_config::{Config, EventsConfig, Isolation};
use bookclerk_library::control_plane::{
    create_plugin_instance, import_instance_config_if_absent, ConfigActor, InstancePackagePolicy,
    PluginInstanceConfigV1, SettingValue,
};
use bookclerk_library::LibraryStore;
use bookclerk_source::{ContentSource, ScanOptions};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::consent::{consent_request, PluginGrantStore};
use crate::discover::DiscoveredPlugin;
use crate::{ExternalSource, SessionServices};

fn find_guest_binary() -> Option<PathBuf> {
    let name = "bookclerk-plugin-source-graphicaudio";
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let candidate = target
        .join(profile)
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    candidate.is_file().then_some(candidate)
}

fn stage_files() -> tempfile::TempDir {
    let base = std::env::temp_dir().join("gha-sunlen").join("x".repeat(48));
    std::fs::create_dir_all(&base).expect("stage base");
    tempfile::Builder::new()
        .prefix("td")
        .tempdir_in(base)
        .expect("stage files")
}

struct Staged {
    files: tempfile::TempDir,
    _install: tempfile::TempDir,
    plugin: DiscoveredPlugin,
}

fn stage_graphicaudio() -> Staged {
    let binary = find_guest_binary().unwrap_or_else(|| {
        panic!(
            "bookclerk-plugin-source-graphicaudio is missing; build it before this test \
             (BOOKCLERK_REQUIRE_TEST_GUESTS={})",
            std::env::var("BOOKCLERK_REQUIRE_TEST_GUESTS").unwrap_or_default()
        );
    });
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/bookclerk-plugins/optional/source-graphicaudio");
    let install = tempfile::tempdir().unwrap();
    std::fs::copy(src.join("plugin.toml"), install.path().join("plugin.toml")).unwrap();
    let dest = install.path().join(format!(
        "bookclerk-plugin-source-graphicaudio{}",
        std::env::consts::EXE_SUFFIX
    ));
    std::fs::copy(&binary, &dest).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms).unwrap();
    }
    let toml = std::fs::read_to_string(install.path().join("plugin.toml")).unwrap();
    let manifest = bookclerk_plugin_manifest::parse(&toml).unwrap();
    let files = stage_files();
    let plugin = DiscoveredPlugin::try_new(
        manifest,
        install.path().to_path_buf(),
        dest,
        Some(files.path()),
    )
    .expect("discover graphicaudio");
    Staged {
        files,
        _install: install,
        plugin,
    }
}

async fn file_store(path: &Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(path)
        .await
        .expect("sqlite");
    bookclerk_library::apply_host_schema(&db)
        .await
        .expect("schema");
    LibraryStore::from_connection(db)
}

fn device_document(base_url: &str) -> PluginInstanceConfigV1 {
    let mut settings = std::collections::BTreeMap::new();
    settings.insert("access".into(), SettingValue::String("device".into()));
    settings.insert("base_url".into(), SettingValue::String(base_url.into()));
    PluginInstanceConfigV1 {
        settings,
        secret_refs: Vec::new(),
    }
}

async fn product_hits(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path() == "/api/products")
        .count()
}

#[allow(unsafe_code)]
fn publish_transitional_access_env() {
    // The parent must actually hold the env var the guest is forbidden to see.
    // `set_var` is unsafe; this crate denies unsafe everywhere else.
    unsafe { std::env::set_var("BOOKCLERK_GA_ACCESS", "web") };
}

#[tokio::test]
async fn graphicaudio_scan_uses_instance_access_not_toml_or_env() {
    publish_transitional_access_env();
    let staged = stage_graphicaudio();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/products"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;

    let files = staged.files.path();
    std::fs::write(
        files.join("config.toml"),
        "[sources.graphicaudio]\naccess = \"web\"\nbase_url = \"http://127.0.0.1:9\"\n",
    )
    .unwrap();
    let mut config =
        Config::load(Some(files.to_path_buf()), Some(files.join("config.toml"))).expect("config");
    config.plugins.isolation = Isolation::Off;
    let store = file_store(&files.join("library.db")).await;
    bookclerk_library::configure_master_key(files).expect("dek");
    bookclerk_library::control_plane::bootstrap_control_plane(
        &store,
        files,
        None,
        &EventsConfig::default(),
    )
    .await
    .expect("bootstrap");
    let actor = ConfigActor::Bootstrap;
    let instance = create_plugin_instance(&store, &actor, staged.plugin.plugin_key().canonical())
        .await
        .expect("instance");
    import_instance_config_if_absent(
        &store,
        &actor,
        &instance.id,
        InstancePackagePolicy::GraphicAudio,
        &device_document(&server.uri()),
        "import-ga-test",
    )
    .await
    .expect("import device config");
    let mut grants = PluginGrantStore::load(files).unwrap();
    let mut grant = consent_request(&staged.plugin.manifest, staged.plugin.plugin_key());
    // The guest's HTTP client connects through the host socket proxy. Loopback
    // needs an explicit TCP grant and a CIDR; the product manifest only names
    // GraphicAudio's public hosts.
    let port = server.address().port();
    grant.tcp.insert(crate::TcpGrant {
        host: "127.0.0.1".into(),
        ports: vec![port],
    });
    grant.address_cidrs.insert("127.0.0.1/32".into());
    grants.upsert(grant);
    grants.save(files).unwrap();

    let email = "reader@example.com";
    let scope = store.scope("graphicaudio");
    scope
        .upsert_account(email, "us", Some("Reader"), true)
        .await
        .unwrap();
    let prepared = crate::prepare_open_bindings(
        Some(&store),
        files,
        &staged.plugin,
        serde_json::json!({"access": "web", "base_url": "http://127.0.0.1:9"}),
    )
    .await
    .expect("prepare");
    assert!(prepared.from_instance, "{}", prepared.granted_config);
    assert_eq!(
        prepared.granted_config["base_url"].as_str(),
        Some(server.uri().as_str()),
        "{}",
        prepared.granted_config
    );

    bookclerk_library::configure_master_key(files).unwrap();
    scope
        .save_credentials_json(
            email,
            &serde_json::json!({
                "token": "device-token",
                "client_id": "bookclerk-test",
                "email": email,
                "marketplace": "us",
            }),
        )
        .await
        .unwrap();

    let source = ExternalSource::spawn_with(
        &staged.plugin,
        &config,
        SessionServices::with_event_outbox(store.clone()),
    )
    .await
    .expect("spawn graphicaudio");
    let pid = source.guest_pid().expect("native guest pid");
    #[cfg(target_os = "linux")]
    {
        let environ = std::fs::read(format!("/proc/{pid}/environ")).expect("guest environ");
        let environ = String::from_utf8_lossy(&environ);
        assert!(
            !environ.contains("BOOKCLERK_GA_ACCESS"),
            "guest inherited BOOKCLERK_GA_ACCESS: {environ}"
        );
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pid;

    bookclerk_library::configure_master_key(files).unwrap();
    let summary = source
        .scan(
            &scope,
            ScanOptions {
                accounts: vec![email.into()],
                ..ScanOptions::default()
            },
        )
        .await
        .expect("scan with device config");
    assert!(
        !summary_debug(&summary).contains("BOOKCLERK_GA_PASSWORD"),
        "scan took the Magento password path"
    );
    assert_eq!(product_hits(&server).await, 1);
    drop(source);

    config.sources.set_string("graphicaudio", "access", "zip");
    config
        .sources
        .set_string("graphicaudio", "base_url", "http://127.0.0.1:9");
    let source = ExternalSource::spawn_with(
        &staged.plugin,
        &config,
        SessionServices::with_event_outbox(store.clone()),
    )
    .await
    .expect("respawn after toml-only change");
    bookclerk_library::configure_master_key(files).unwrap();
    source
        .scan(
            &scope,
            ScanOptions {
                accounts: vec![email.into()],
                ..ScanOptions::default()
            },
        )
        .await
        .expect("scan still uses the instance document");
    assert_eq!(
        product_hits(&server).await,
        2,
        "toml access/base_url must not replace the instance document"
    );
}

fn summary_debug(summary: &bookclerk_source::ScanSummary) -> String {
    format!("{summary:?}")
}
