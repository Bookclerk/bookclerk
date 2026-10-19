//! The built-in TLS listener for the node API (`[daemon.node_api] mode =
//! "acme" | "self_signed"`, H-4 design section 4 and decision 1).
//!
//! - `acme`: certificates for `integrations.public_origin`'s host come from
//!   an ACME CA (Let's Encrypt by default) over TLS-ALPN-01, answered on
//!   this same listener, and are cached under `$BOOKCLERK_FILES_DIR/acme/`
//!   (mode 0700). Renewal is automatic.
//! - `self_signed`: a P-256 certificate generated once and kept under
//!   `$BOOKCLERK_FILES_DIR/node-api/self-signed/` (mode 0700). Its SPKI pin
//!   (base64url SHA-256 of the SubjectPublicKeyInfo) is written beside it
//!   and travels in enrollment links.
//!
//! The listener serves only the node API routes (HTTP/1.1, so WebSocket
//! upgrades work) and tags each request [`ListenerKind::AcmeTls`] or
//! [`ListenerKind::SelfSignedTls`] for
//! [`super::transport::classify_transport`].
//!
//! [`NodeApiGate`] keeps the node API unmounted, on every listener, until
//! the TLS listener is serving a certificate. If ACME fails, or the listener
//! cannot bind, every node API route answers 404 `node_api_unmounted`;
//! there is no plain-HTTP fallback. `[daemon.node_api]` is bootstrap
//! configuration: changing it needs a daemon restart.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::serve::ListenerExt as _;
use axum::Extension;
use bookclerk_config::{Config, NodeApiMode};
use futures::StreamExt as _;
use rustls_acme::caches::DirCache;
use rustls_acme::{is_tls_alpn_challenge, AcmeConfig, EventOk};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, Semaphore};
use tokio_rustls::rustls::crypto::{aws_lc_rs, CryptoProvider};
use tokio_rustls::rustls::pki_types::pem::PemObject as _;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::server::{Acceptor, ResolvesServerCert};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::server::TlsStream;
use tokio_rustls::LazyConfigAcceptor;

use super::transport::{ListenerKind, LocalAddr};
use crate::api::AppState;

/// Seconds a client has to finish its TLS handshake.
const HANDSHAKE_TIMEOUT_SECS: u64 = 10;

/// Connections in flight at once: handshaking, or finished and waiting for
/// room in [`ACCEPT_QUEUE`]. Further connections are shed.
const MAX_PENDING_HANDSHAKES: usize = 256;

/// Finished handshakes waiting for the HTTP server.
const ACCEPT_QUEUE: usize = 64;

/// Whether the node API is being served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeApiStatus {
    /// The TLS listener has no certificate yet.
    Starting,
    /// Serving.
    Ready,
    /// The TLS listener could not start or obtain a certificate; the node
    /// API stays unmounted (ACME keeps retrying in the background).
    Failed,
}

/// Request extension every listener carries: whether the node API is
/// mounted, and the `[daemon.node_api]` mode it was started for. Modes
/// without a TLS listener are always mounted.
#[derive(Debug, Clone)]
pub struct NodeApiGate {
    /// Status published by the TLS listener (or fixed at start).
    status: watch::Receiver<NodeApiStatus>,
    /// The mode [`start`] ran for; a reloaded config naming another mode
    /// leaves the node API unmounted until the daemon restarts.
    mode: NodeApiMode,
}

impl NodeApiGate {
    /// A gate for `mode` fixed at `status`.
    #[cfg(test)]
    #[must_use]
    pub fn fixed(mode: NodeApiMode, status: NodeApiStatus) -> Self {
        Self {
            status: watch::channel(status).1,
            mode,
        }
    }

    /// Current status.
    #[must_use]
    pub fn status(&self) -> NodeApiStatus {
        *self.status.borrow()
    }

    /// Waits until the status is no longer [`NodeApiStatus::Starting`].
    #[cfg(test)]
    pub async fn settled(&mut self) -> NodeApiStatus {
        let _ = self
            .status
            .wait_for(|status| *status != NodeApiStatus::Starting)
            .await;
        self.status()
    }
}

/// True when the node API may serve a request in `mode` (the live config).
/// Modes with a TLS listener need a gate reporting
/// [`NodeApiStatus::Ready`]; a request without a gate is refused in those
/// modes (fail closed). A gate started for another mode means
/// `[daemon.node_api]` changed on reload: the listeners still match the old
/// mode, so the node API stays unmounted until a restart.
#[must_use]
pub fn node_api_mounted(mode: NodeApiMode, gate: Option<&NodeApiGate>) -> bool {
    if gate.is_some_and(|gate| gate.mode != mode) {
        return false;
    }
    match mode {
        NodeApiMode::Loopback | NodeApiMode::Proxy => true,
        NodeApiMode::Acme | NodeApiMode::SelfSigned => {
            gate.is_some_and(|gate| gate.status() == NodeApiStatus::Ready)
        }
    }
}

/// The started (or refused) TLS listener.
#[derive(Debug)]
pub struct NodeApiTls {
    /// Mount state for every listener.
    pub gate: NodeApiGate,
    /// Bound address, when the listener is up.
    #[cfg_attr(not(test), expect(dead_code, reason = "tests dial the bound port"))]
    pub local: Option<SocketAddr>,
}

/// The process crypto provider for the node API (aws-lc, like the rest of
/// the tree).
fn provider() -> Arc<CryptoProvider> {
    Arc::new(aws_lc_rs::default_provider())
}

/// Creates `dir` (and parents) readable only by this user.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Writes `contents` to `path`, readable only by this user. The bytes go to
/// a temporary sibling that is synced and then renamed over `path`, so a
/// crash leaves either the old file or the complete new one, never a
/// truncated certificate or key that would fail every later start.
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

/// Starts the TLS listener the configured mode needs. Never fails: a mode
/// that cannot start reports [`NodeApiStatus::Failed`] and the node API
/// stays unmounted.
pub async fn start(state: &Arc<AppState>, cfg: &Config, files_dir: &Path) -> NodeApiTls {
    let node_api = &cfg.daemon.node_api;
    let (status, receiver) = watch::channel(NodeApiStatus::Starting);
    let gate = NodeApiGate {
        status: receiver,
        mode: node_api.mode,
    };
    let failed = |status: &watch::Sender<NodeApiStatus>, gate: &NodeApiGate| {
        status.send_replace(NodeApiStatus::Failed);
        NodeApiTls {
            gate: gate.clone(),
            local: None,
        }
    };
    let kind = match node_api.mode {
        NodeApiMode::Loopback | NodeApiMode::Proxy => {
            status.send_replace(NodeApiStatus::Ready);
            return NodeApiTls { gate, local: None };
        }
        NodeApiMode::Acme => ListenerKind::AcmeTls,
        NodeApiMode::SelfSigned => ListenerKind::SelfSignedTls,
    };
    let public_origin = cfg.integrations.public_origin.as_deref();
    if let Err(err) = node_api.validate(&cfg.daemon.trusted_proxies, public_origin) {
        tracing::error!(error = %err, "node API unmounted: invalid [daemon.node_api]");
        return failed(&status, &gate);
    }
    let Some(listen) = node_api
        .tls_listen
        .as_deref()
        .and_then(|addr| addr.trim().parse::<SocketAddr>().ok())
    else {
        return failed(&status, &gate);
    };
    let tcp = match TcpListener::bind(listen).await {
        Ok(tcp) => tcp,
        Err(err) => {
            tracing::error!(%listen, error = %err, "node API unmounted: cannot bind tls_listen");
            return failed(&status, &gate);
        }
    };
    let Ok(local) = tcp.local_addr() else {
        return failed(&status, &gate);
    };
    let certs = match node_api.mode {
        NodeApiMode::Acme => start_acme(cfg, files_dir, status.clone()),
        _ => self_signed(cfg, files_dir, local),
    };
    let (server_config, challenge) = match certs {
        Ok(certs) => certs,
        Err(err) => {
            tracing::error!(error = %err, "node API unmounted: TLS setup failed");
            return failed(&status, &gate);
        }
    };
    let app = super::node_router::<()>(Arc::clone(state))
        .layer(Extension(kind))
        .layer(Extension(LocalAddr(local)))
        .layer(Extension(gate.clone()));
    let (handshakes, accepted) = mpsc::channel(ACCEPT_QUEUE);
    tokio::spawn(accept_loop(
        tcp,
        server_config,
        challenge,
        handshakes,
        MAX_PENDING_HANDSHAKES,
    ));
    tokio::spawn(async move {
        // `tap_io` gives a custom listener axum's `ConnectInfo<SocketAddr>`.
        let listener = TlsListener { accepted, local }.tap_io(|_| {});
        if let Err(err) = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        {
            tracing::error!(error = %err, "node API TLS listener exited");
        }
    });
    if node_api.mode == NodeApiMode::SelfSigned {
        // The certificate is in hand; ACME turns Ready from its event task.
        status.send_replace(NodeApiStatus::Ready);
    }
    tracing::info!(%local, mode = node_api.mode.as_str(), "node API TLS listener bound");
    NodeApiTls {
        gate,
        local: Some(local),
    }
}

/// A server config serving HTTP/1.1 with `resolver`'s certificates.
fn server_config(resolver: Arc<dyn ResolvesServerCert>) -> anyhow::Result<Arc<ServerConfig>> {
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Starts the ACME state machine. Answers the server config and the
/// TLS-ALPN-01 challenge config; `status` turns Ready once a certificate
/// (cached or new) is deployed.
fn start_acme(
    cfg: &Config,
    files_dir: &Path,
    status: watch::Sender<NodeApiStatus>,
) -> anyhow::Result<(Arc<ServerConfig>, Option<Arc<ServerConfig>>)> {
    let node_api = &cfg.daemon.node_api;
    let origin = url::Url::parse(cfg.integrations.public_origin.as_deref().unwrap_or(""))?;
    let domain = match origin.host() {
        Some(url::Host::Domain(domain)) => domain.to_ascii_lowercase(),
        _ => anyhow::bail!("integrations.public_origin has no DNS name"),
    };
    let cache_dir = files_dir.join(bookclerk_config::NODE_API_ACME_DIR);
    create_private_dir(&cache_dir)?;
    let directory = node_api
        .acme_directory
        .as_deref()
        .map(str::trim)
        .unwrap_or(bookclerk_config::LETS_ENCRYPT_DIRECTORY);
    let mut acme = AcmeConfig::new([domain.as_str()])
        .contact_push(node_api.acme_contact.as_deref().unwrap_or("").trim())
        .directory(directory);
    if let Some(ca_file) = node_api.acme_directory_ca.as_deref() {
        acme = acme.client_tls_config(directory_client_config(Path::new(ca_file.trim()))?);
    }
    let mut acme = acme.cache(DirCache::new(cache_dir)).state();
    let resolver = acme.resolver();
    let challenge = acme.challenge_rustls_config();
    tokio::spawn(async move {
        while let Some(event) = acme.next().await {
            match event {
                Ok(EventOk::DeployedCachedCert | EventOk::DeployedNewCert) => {
                    tracing::info!(%domain, "node API ACME certificate deployed");
                    status.send_replace(NodeApiStatus::Ready);
                }
                Ok(other) => tracing::debug!(event = ?other, "node API ACME"),
                Err(err) => {
                    tracing::warn!(
                        %domain,
                        error = %err,
                        "node API ACME failed; retrying with backoff"
                    );
                    if *status.borrow() != NodeApiStatus::Ready {
                        status.send_replace(NodeApiStatus::Failed);
                    }
                }
            }
        }
    });
    Ok((server_config(resolver)?, Some(challenge)))
}

/// Client config for the ACME directory: the webpki roots plus the PEM
/// roots in `ca_file` (a private ACME CA).
fn directory_client_config(ca_file: &Path) -> anyhow::Result<Arc<ClientConfig>> {
    let mut roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut added = 0usize;
    for cert in CertificateDer::pem_file_iter(ca_file)? {
        roots.add(cert?)?;
        added += 1;
    }
    anyhow::ensure!(added > 0, "{} holds no certificates", ca_file.display());
    Ok(Arc::new(
        ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

/// The self-signed certificate, its key, and its SPKI pin.
struct SelfSigned {
    /// Certificate DER.
    cert: CertificateDer<'static>,
    /// PKCS#8 private key DER.
    key: PrivatePkcs8KeyDer<'static>,
    /// base64url SHA-256 of the SubjectPublicKeyInfo.
    pin: String,
}

/// The SPKI pin of a key pair.
fn spki_pin(key_pair: &rcgen::KeyPair) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(key_pair.public_key_der()))
}

/// Loads the self-signed certificate, generating it on first use.
fn load_or_create_self_signed(
    cfg: &Config,
    files_dir: &Path,
    local: SocketAddr,
) -> anyhow::Result<SelfSigned> {
    let dir = files_dir.join(bookclerk_config::NODE_API_SELF_SIGNED_DIR);
    create_private_dir(&dir)?;
    let cert_path = dir.join("cert.der");
    let key_path = dir.join("key.der");
    let pin_path = dir.join(bookclerk_config::NODE_API_SPKI_PIN_FILE);
    if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        let key_pair = rcgen::KeyPair::try_from(key.as_slice())?;
        let pin = spki_pin(&key_pair);
        // The pin file is what the CLI reads; keep it in step with the key.
        write_private(&pin_path, pin.as_bytes())?;
        return Ok(SelfSigned {
            cert: CertificateDer::from(cert),
            key: PrivatePkcs8KeyDer::from(key),
            pin,
        });
    }
    let mut names = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    if let Some(host) = cfg
        .integrations
        .public_origin
        .as_deref()
        .and_then(|origin| url::Url::parse(origin).ok())
        .and_then(|url| {
            url.host_str()
                .map(|host| host.trim_matches(['[', ']']).to_string())
        })
    {
        names.push(host);
    }
    if !local.ip().is_unspecified() {
        names.push(local.ip().to_string());
    }
    names.sort();
    names.dedup();
    let generated = rcgen::generate_simple_self_signed(names)?;
    let pin = spki_pin(&generated.key_pair);
    let key = generated.key_pair.serialize_der();
    write_private(&key_path, &key)?;
    write_private(&cert_path, generated.cert.der())?;
    write_private(&pin_path, pin.as_bytes())?;
    tracing::info!(%pin, "generated the self-signed node API certificate");
    Ok(SelfSigned {
        cert: generated.cert.der().clone(),
        key: PrivatePkcs8KeyDer::from(key),
        pin,
    })
}

/// Server config for the self-signed certificate.
fn self_signed(
    cfg: &Config,
    files_dir: &Path,
    local: SocketAddr,
) -> anyhow::Result<(Arc<ServerConfig>, Option<Arc<ServerConfig>>)> {
    let served = load_or_create_self_signed(cfg, files_dir, local)?;
    tracing::info!(pin = %served.pin, "node API serves the pinned self-signed certificate");
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![served.cert], PrivateKeyDer::Pkcs8(served.key))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok((Arc::new(config), None))
}

/// Accepts TCP connections and runs each TLS handshake in its own task.
/// TLS-ALPN-01 challenge handshakes are answered and closed; finished
/// handshakes go to the HTTP server. At most `max_pending` connections are
/// handshaking or waiting to be handed over at once; the rest are shed.
async fn accept_loop(
    tcp: TcpListener,
    server_config: Arc<ServerConfig>,
    challenge: Option<Arc<ServerConfig>>,
    handshakes: mpsc::Sender<(TlsStream<TcpStream>, SocketAddr)>,
    max_pending: usize,
) {
    let pending = Arc::new(Semaphore::new(max_pending));
    loop {
        let (stream, peer) = match tcp.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                tracing::debug!(error = %err, "node API TLS accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = Arc::clone(&pending).try_acquire_owned() else {
            // Too many slow handshakes: shed this one.
            continue;
        };
        let server_config = Arc::clone(&server_config);
        let challenge = challenge.clone();
        let handshakes = handshakes.clone();
        tokio::spawn(async move {
            let handshake =
                tokio::time::timeout(Duration::from_secs(HANDSHAKE_TIMEOUT_SECS), async move {
                    let start = LazyConfigAcceptor::new(Acceptor::default(), stream).await?;
                    if let Some(challenge) = challenge {
                        if is_tls_alpn_challenge(&start.client_hello()) {
                            start.into_stream(challenge).await?;
                            return Ok::<_, std::io::Error>(None);
                        }
                    }
                    start.into_stream(server_config).await.map(Some)
                })
                .await;
            match handshake {
                Ok(Ok(Some(tls))) => {
                    // Hold the permit until the server takes the stream, so a
                    // stalled server bounds waiters too, not just handshakes.
                    let _ = handshakes.send((tls, peer)).await;
                }
                Ok(Ok(None)) => tracing::debug!(%peer, "answered a TLS-ALPN-01 challenge"),
                Ok(Err(err)) => {
                    tracing::debug!(%peer, error = %err, "node API TLS handshake failed")
                }
                Err(_) => tracing::debug!(%peer, "node API TLS handshake timed out"),
            }
            drop(permit);
        });
    }
}

/// Finished TLS connections, as an axum listener.
struct TlsListener {
    /// Handshakes finished by [`accept_loop`].
    accepted: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    /// Bound address.
    local: SocketAddr,
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.accepted.recv().await {
            Some(accepted) => accepted,
            // The accept loop never ends; if it did, stop accepting.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

#[cfg(test)]
#[path = "tls_tests.rs"]
mod tests;
