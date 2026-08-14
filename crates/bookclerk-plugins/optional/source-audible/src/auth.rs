//! OAuth login orchestration via audible-rs (QR + callback server + external paste).

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use audible_rs::api::locale;
use audible_rs::auth::device::{Device, DeviceKind};
use audible_rs::auth::login::{self as login_flow, LoginDefaults, LoginServer};
use audible_rs::auth::Authenticator;
use serde::{Deserialize, Serialize};

use crate::error::{AudibleError, Result};
use crate::qr::{render_login_qr, QrRenderMode};
use crate::secret::resolve_auth_password;

/// How to complete the browser sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoginMode {
    /// Local reverse-proxy + QR (headless / SSH friendly). Default.
    #[default]
    Server,
    /// Print authorize URL; read redirect from `--response-url` or stdin.
    External,
}

/// Inputs for interactive Audible OAuth (SPA Accounts connect / CLI login helpers).
///
/// Prefer Accounts UI for operator flows; these options also drive guest
/// `loginStart` / in-process [`begin_login`].
#[derive(Debug, Clone)]
pub struct AuthLoginOptions {
    /// Audible marketplace / locale code (`us`, `uk`, `de`, …).
    pub marketplace: String,
    /// Optional operator-facing account label stored with the secret.
    pub label: Option<String>,
    /// Bind address for the local OAuth redirect receiver (`127.0.0.1:0` = ephemeral).
    pub callback_bind: SocketAddr,
    /// Pre-captured redirect URL (external mode / paste); skips waiting on the server.
    pub response_url: Option<String>,
    /// When true, emit QR text alongside the authorize URL for headless terminals.
    pub show_qr: bool,
    /// How to render the QR payload ([`QrRenderMode`]).
    pub qr_mode: QrRenderMode,
    /// Whether to run a local callback server or require an external redirect paste.
    pub mode: LoginMode,
    /// Seconds to wait for LoginServer capture before timing out.
    pub timeout_secs: u64,
    /// Pre-merger Audible username login (DE/US/UK only).
    pub audible_username: bool,
    /// Overwrite an existing sealed authenticator for the same account id.
    pub force: bool,
    /// When `Some`, credentials are persisted via the Audible
    /// [`bookclerk_library::SourceScope`] (same scoping rules as third-party
    /// plugins). When `None`, only session metadata is returned (guest path).
    pub scope: Option<bookclerk_library::SourceScope>,
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
            mode: LoginMode::Server,
            timeout_secs: 300,
            audible_username: false,
            force: false,
            scope: None,
        }
    }
}

/// Progress events emitted during login (for CLI / TUI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoginProgress {
    /// Browser URL the operator should open (optional pre-rendered QR text).
    LoginUrl {
        /// Absolute HTTPS login URL.
        url: String,
        /// Optional pre-rendered QR text for terminal UIs.
        qr: Option<String>,
    },
    /// Local callback server is listening.
    CallbackListening {
        /// Socket address of the local redirect receiver.
        addr: SocketAddr,
    },
    /// Waiting for the OAuth redirect / callback.
    WaitingForCallback,
    /// Login finished for this account id.
    Completed {
        /// Account id stored after a successful login.
        account_id: String,
    },
}

/// Metadata returned after a successful Audible OAuth session.
///
/// Callers that pass [`AuthLoginOptions::scope`] already have credentials in
/// `encrypted_secrets`; guest callers export the authenticator separately.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSession {
    /// Stable Bookclerk account id for library rows and secret lookup.
    pub account_id: String,
    /// Marketplace used for the login (`us`, `uk`, …).
    pub marketplace: String,
    /// Optional operator-facing label from [`AuthLoginOptions::label`].
    pub label: Option<String>,
    /// Audible customer id from the authenticator when present.
    pub customer_id: Option<String>,
}

/// Runs Audible OAuth via audible-rs and returns [`AuthSession`] metadata.
///
/// When [`AuthLoginOptions::scope`] is `Some`, credentials are persisted to
/// `encrypted_secrets`. When `None` (guest / external plugin path), login
/// completes and returns session metadata only — the caller exports the
/// authenticator separately.
///
/// # Arguments
///
/// * `opts` - Marketplace, callback bind, login mode, and optional DB scope.
/// * `on_progress` - Callback for URL / QR / waiting / completed events (CLI/TUI).
///
/// # Errors
///
/// Returns [`AudibleError::Auth`] when locale, device registration, or OAuth fails.
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

    // Android registration is required for Widevine L3 drmlicense grants.
    let device_kind = DeviceKind::Android;

    let auth = match opts.mode {
        LoginMode::Server if opts.response_url.is_none() => {
            login_via_server(&opts, device_kind, &mut on_progress).await?
        }
        LoginMode::Server | LoginMode::External => {
            login_via_external(&opts, device_kind, &locale, &mut on_progress).await?
        }
    };

    finish_login(opts, auth, on_progress).await
}

/// LoginDefaults for the Audible LoginServer (Android + plain authfile).
pub(crate) fn login_defaults(opts: &AuthLoginOptions) -> LoginDefaults {
    LoginDefaults {
        country_code: Some(opts.marketplace.clone()),
        device: DeviceKind::Android,
        username: opts.audible_username,
        // Account identity is the Audible customer id after login — do not
        // prefill / solicit a local "account name" that could diverge from it.
        name: None,
        marketplaces: None,
        default_marketplaces: None,
        plain: true,
    }
}

/// Browser URL + bound addr for a live [`LoginServer`].
pub(crate) fn login_server_url(server: &LoginServer, bind: SocketAddr) -> (String, SocketAddr) {
    let port = server.local_port();
    let path = server.landing_path();
    let ip = bind.ip();
    let host = if ip.is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        ip.to_string()
    };
    let url = format!("http://{host}:{port}{path}");
    let addr = SocketAddr::new(ip, port);
    (url, addr)
}

/// Runs the local callback server, waits for the Amazon redirect, and registers the device.
pub(crate) async fn login_via_server(
    opts: &AuthLoginOptions,
    device_kind: DeviceKind,
    on_progress: &mut impl FnMut(LoginProgress),
) -> Result<Authenticator> {
    let mut defaults = login_defaults(opts);
    defaults.device = device_kind;

    let server = LoginServer::bind(opts.callback_bind, defaults).await?;
    let (url, addr) = login_server_url(&server, opts.callback_bind);

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

/// Prints the authorize URL and registers after the operator pastes the redirect.
async fn login_via_external(
    opts: &AuthLoginOptions,
    device_kind: DeviceKind,
    locale: &audible_rs::api::locale::Locale,
    on_progress: &mut impl FnMut(LoginProgress),
) -> Result<Authenticator> {
    let device = Device::generate(device_kind);
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

/// Build [`AuthSession`] from a completed authenticator; persist when scope is set.
pub(crate) async fn finish_login(
    opts: AuthLoginOptions,
    auth: Authenticator,
    mut on_progress: impl FnMut(LoginProgress),
) -> Result<AuthSession> {
    let marketplace = auth.locale().country_code.to_string();
    let customer_id = auth.customer_id().map(str::to_string);
    // Secrets and account rows are keyed by Audible customer id (never by
    // display label). Label is display-only metadata on the account row.
    let account_id = customer_id
        .clone()
        .or_else(|| opts.label.clone())
        .unwrap_or_else(|| marketplace.clone());
    let label = opts.label.clone();

    if let Some(scope) = &opts.scope {
        if !opts.force {
            let existing = crate::db::load_authenticator_from_db(scope, &account_id).await?;
            if existing.is_some() {
                return Err(AudibleError::Auth(format!(
                    "audible account `{account_id}` already exists in encrypted_secrets \
                     (pass --force to overwrite)"
                )));
            }
        }
        crate::db::save_authenticator_to_db(&auth, scope, &account_id).await?;
        scope
            .upsert_account(&account_id, &marketplace, label.as_deref(), true)
            .await
            .map_err(|e| AudibleError::Auth(e.to_string()))?;
    }

    on_progress(LoginProgress::Completed {
        account_id: account_id.clone(),
    });

    Ok(AuthSession {
        account_id,
        marketplace,
        label,
        customer_id,
    })
}

/// Session metadata from an authenticator without DB writes (guest path).
#[must_use]
pub(crate) fn session_from_authenticator(
    auth: &Authenticator,
    label: Option<String>,
) -> AuthSession {
    let marketplace = auth.locale().country_code.to_string();
    let customer_id = auth.customer_id().map(str::to_string);
    let account_id = customer_id
        .clone()
        .or_else(|| label.clone())
        .unwrap_or_else(|| marketplace.clone());
    AuthSession {
        account_id,
        marketplace,
        label,
        customer_id,
    }
}

/// Export authenticator as Protection::Plain authfile bytes.
pub(crate) fn export_authfile_plain_bytes(auth: &Authenticator) -> Result<Vec<u8>> {
    let data = auth.export_value();
    let plain = audible_rs::auth::authfile::write(
        &data,
        audible_rs::auth::authfile::Protection::Plain,
        None,
    )
    .map_err(|e| AudibleError::Auth(e.to_string()))?;
    Ok(plain.into_bytes())
}

/// HTTP client for Amazon register calls (30s connect timeout).
fn reqwest_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|err| AudibleError::Auth(err.to_string()))
}

/// Prompts on stderr and reads one redirect URL line from stdin; empty input fails.
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

/// Load an authenticator from an external audible-rs auth file (plain or encrypted).
///
/// Used only for one-shot **import** of a user-supplied file into the DB.
/// Passphrase resolution: `BOOKCLERK_AUTH_PASSWORD`. When unset, loads a
/// plaintext envelope.
pub(crate) async fn load_authenticator(path: &Path) -> Result<Authenticator> {
    let password = resolve_auth_password()?;
    let auth = Authenticator::load_file(path, password)
        .await
        .map_err(|err| {
            let msg = err.to_string();
            if msg.contains("password") || msg.contains("decrypt") || msg.contains("cipher") {
                AudibleError::Auth(format!(
                    "failed to load {} ({msg}) — set {} for encrypted files",
                    path.display(),
                    crate::secret::AUTH_PASSWORD_ENV,
                ))
            } else {
                AudibleError::from(err)
            }
        })?;
    register_authenticator_secrets(&auth);
    Ok(auth)
}

/// Registers access/refresh tokens with the diagnostics redaction set.
fn register_authenticator_secrets(auth: &Authenticator) {
    use secrecy::ExposeSecret;
    if let Some(t) = auth.access_token() {
        bookclerk_config::register_secret(t.expose_secret());
    }
    if let Some(t) = auth.refresh_token() {
        bookclerk_config::register_secret(t.expose_secret());
    }
}
