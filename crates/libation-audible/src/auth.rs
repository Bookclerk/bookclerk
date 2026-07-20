//! OAuth login orchestration via audible-rs (QR + callback server + external paste).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use audible_rs::api::locale;
use audible_rs::auth::authfile::KdfParams;
use audible_rs::auth::device::{Device, DeviceKind};
use audible_rs::auth::login::{self as login_flow, LoginDefaults, LoginServer};
use audible_rs::auth::Authenticator;
use serde::{Deserialize, Serialize};

use crate::error::{AudibleError, Result};
use crate::paths::{auth_dir, auth_file_for};
use crate::qr::{render_login_qr, QrRenderMode};

/// How to complete the browser sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoginMode {
    /// Local reverse-proxy + QR (headless / SSH friendly). Default.
    #[default]
    Server,
    /// Print authorize URL; read redirect from `--response-url` or stdin.
    External,
}

/// Options for `libation auth login`.
#[derive(Debug, Clone)]
pub struct AuthLoginOptions {
    pub marketplace: String,
    pub label: Option<String>,
    pub callback_bind: SocketAddr,
    pub response_url: Option<String>,
    pub show_qr: bool,
    pub qr_mode: QrRenderMode,
    pub files_dir: PathBuf,
    pub mode: LoginMode,
    /// Seconds to wait for LoginServer capture.
    pub timeout_secs: u64,
    /// Pre-merger Audible username login (DE/US/UK).
    pub audible_username: bool,
    /// Overwrite an existing auth file.
    pub force: bool,
}

impl Default for AuthLoginOptions {
    fn default() -> Self {
        Self {
            marketplace: "us".into(),
            label: None,
            callback_bind: "127.0.0.1:0".parse().expect("valid socket addr"),
            response_url: None,
            show_qr: true,
            qr_mode: QrRenderMode::Unicode,
            files_dir: PathBuf::from("LibationFiles"),
            mode: LoginMode::Server,
            timeout_secs: 300,
            audible_username: false,
            force: false,
        }
    }
}

/// Progress events emitted during login (for CLI / TUI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoginProgress {
    LoginUrl { url: String, qr: Option<String> },
    CallbackListening { addr: SocketAddr },
    WaitingForCallback,
    Completed { account_id: String },
}

/// Result of a successful auth session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSession {
    pub account_id: String,
    pub marketplace: String,
    pub label: Option<String>,
    pub auth_file: PathBuf,
    pub customer_id: Option<String>,
}

/// Begin the login flow using audible-rs.
pub async fn begin_login(
    opts: AuthLoginOptions,
    mut on_progress: impl FnMut(LoginProgress),
) -> Result<AuthSession> {
    let locale = locale::require(&opts.marketplace).map_err(AudibleError::Auth)?;

    if opts.audible_username && !login_flow::username_login_supported(&locale) {
        return Err(AudibleError::Auth(
            "username (pre-merger Audible) login is only available for de, us, and uk".into(),
        ));
    }

    std::fs::create_dir_all(auth_dir(&opts.files_dir))?;

    let auth = match opts.mode {
        LoginMode::Server if opts.response_url.is_none() => {
            login_via_server(&opts, &mut on_progress).await?
        }
        LoginMode::Server | LoginMode::External => {
            login_via_external(&opts, &locale, &mut on_progress).await?
        }
    };

    persist_account(opts, auth, on_progress).await
}

async fn login_via_server(
    opts: &AuthLoginOptions,
    on_progress: &mut impl FnMut(LoginProgress),
) -> Result<Authenticator> {
    let defaults = LoginDefaults {
        country_code: Some(opts.marketplace.clone()),
        device: DeviceKind::DEFAULT,
        username: opts.audible_username,
        name: opts.label.clone(),
        marketplaces: None,
        default_marketplaces: None,
        plain: true,
    };

    let server = LoginServer::bind(opts.callback_bind, defaults).await?;
    let port = server.local_port();
    let path = server.landing_path();
    let ip = opts.callback_bind.ip();
    let host = if ip.is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        ip.to_string()
    };
    let url = format!("http://{host}:{port}{path}");
    let addr = SocketAddr::new(ip, port);

    let qr = if opts.show_qr {
        Some(render_login_qr(&url, opts.qr_mode)?)
    } else {
        None
    };
    on_progress(LoginProgress::LoginUrl {
        url: url.clone(),
        qr,
    });
    on_progress(LoginProgress::CallbackListening { addr });
    on_progress(LoginProgress::WaitingForCallback);

    let login = server.run(Duration::from_secs(opts.timeout_secs)).await?;

    let http = reqwest_client()?;
    let auth = login_flow::register(
        &http,
        &login.locale,
        &login.device,
        &login.pkce,
        &login.code,
        login.with_username,
    )
    .await?;
    Ok(auth)
}

async fn login_via_external(
    opts: &AuthLoginOptions,
    locale: &audible_rs::api::locale::Locale,
    on_progress: &mut impl FnMut(LoginProgress),
) -> Result<Authenticator> {
    let device = Device::generate(DeviceKind::DEFAULT);
    let pkce = login_flow::Pkce::generate();
    let url = login_flow::authorize_url(&device, &pkce, locale, opts.audible_username);

    let qr = if opts.show_qr {
        Some(render_login_qr(&url, opts.qr_mode)?)
    } else {
        None
    };
    on_progress(LoginProgress::LoginUrl { url, qr });
    on_progress(LoginProgress::WaitingForCallback);

    let redirect = if let Some(response_url) = &opts.response_url {
        response_url.clone()
    } else {
        read_redirect_from_stdin()?
    };

    let code = login_flow::extract_authorization_code(&redirect)?;
    let http = reqwest_client()?;
    let auth =
        login_flow::register(&http, locale, &device, &pkce, &code, opts.audible_username).await?;
    Ok(auth)
}

async fn persist_account(
    opts: AuthLoginOptions,
    auth: Authenticator,
    mut on_progress: impl FnMut(LoginProgress),
) -> Result<AuthSession> {
    let marketplace = auth.locale().country_code.to_string();
    let customer_id = auth.customer_id().map(str::to_string);
    let account_name = opts
        .label
        .clone()
        .or_else(|| customer_id.clone())
        .unwrap_or_else(|| marketplace.clone());

    let auth_file = auth_file_for(&opts.files_dir, &account_name);
    if auth_file.exists() && !opts.force {
        return Err(AudibleError::Auth(format!(
            "{} already exists (pass --force to overwrite)",
            auth_file.display()
        )));
    }

    // Headless default: plain auth file (encrypt later via password flag if needed).
    auth.save_to(&auth_file, None, KdfParams::default()).await?;

    let account_id = customer_id.clone().unwrap_or_else(|| account_name.clone());

    on_progress(LoginProgress::Completed {
        account_id: account_id.clone(),
    });

    Ok(AuthSession {
        account_id,
        marketplace,
        label: opts.label.or(Some(account_name)),
        auth_file,
        customer_id,
    })
}

fn reqwest_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|err| AudibleError::Auth(err.to_string()))
}

fn read_redirect_from_stdin() -> Result<String> {
    use std::io::{self, BufRead, Write};
    eprint!("Paste the redirect URL, then press Enter:\n> ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|err| AudibleError::Auth(format!("failed to read redirect URL: {err}")))?;
    let trimmed = line.trim().to_string();
    if trimmed.is_empty() {
        return Err(AudibleError::Auth("empty redirect URL".into()));
    }
    Ok(trimmed)
}

/// Load an authenticator from a Libation auth file (plain or encrypted).
pub async fn load_authenticator(
    path: &Path,
    password: Option<secrecy::SecretString>,
) -> Result<Authenticator> {
    Authenticator::load_file(path, password)
        .await
        .map_err(AudibleError::from)
}
