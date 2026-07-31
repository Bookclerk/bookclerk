//! `bookclerk auth` — login, import, list, status, revoke.

use std::net::SocketAddr;
use std::path::PathBuf;

use bookclerk_config::Config;
use bookclerk_source::{ImportCredentialsOptions, LoginOptions, OAuthProgress, PortalAuthMode};
use clap::Subcommand;

use crate::registry::{default_registry_with_plugins, resolve_source_id};

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Log in to a content source (OAuth or email/password stores).
    ///
    /// OAuth: Amazon accounts with 2FA/MFA must complete OTP (or another
    /// challenge) in the browser — store OAuth has no username/password flags.
    ///
    /// Password sources: require `--email`; password from the source's env var
    /// (e.g. `BOOKCLERK_LIBRO_PASSWORD`) or an interactive prompt — never on argv.
    Login {
        /// Content source id (`audible`, `libro`, `graphicaudio`, `chirp`, …).
        #[arg(long, default_value = "audible")]
        source: String,
        /// Marketplace code (`us`, `uk`, `de`, …).
        #[arg(short = 'm', long, default_value = "us")]
        marketplace: String,
        /// Optional display label (Audible secrets are keyed by customer id).
        #[arg(long)]
        label: Option<String>,
        /// Account email (required for password sources).
        #[arg(long)]
        email: Option<String>,
        /// Print authorize URL and paste redirect (instead of local login server).
        /// OAuth sources only.
        #[arg(long)]
        external: bool,
        /// Amazon redirect URL (with `--external`; otherwise read from stdin).
        /// OAuth sources only.
        #[arg(long)]
        response_url: Option<String>,
        /// Callback server bind address (login-server mode). OAuth sources only.
        #[arg(long, default_value = "127.0.0.1:0")]
        callback_bind: SocketAddr,
        /// Seconds to wait for login-server capture. OAuth sources only.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        /// Pre-merger Audible username login (de/us/uk only).
        #[arg(long)]
        audible_username: bool,
        /// Overwrite an existing auth file.
        #[arg(long)]
        force: bool,
        /// Disable terminal QR output. OAuth sources only.
        #[arg(long)]
        no_qr: bool,
        /// Use ASCII QR instead of Unicode blocks. OAuth sources only.
        #[arg(long)]
        ascii_qr: bool,
    },
    /// Import store auth file or classic Libation AccountsSettings.json.
    Import {
        /// Path to auth file or AccountsSettings.json.
        path: PathBuf,
        /// Content source that understands this import format.
        #[arg(long, default_value = "audible")]
        source: String,
        /// Treat input as classic Libation AccountsSettings.json.
        #[arg(long)]
        libation_accounts: bool,
        /// Import mkb79/audible-cli legacy auth JSON (classic `import-account`).
        #[arg(long)]
        mkb79: bool,
        /// Destination auth filename stem (auth-file import).
        #[arg(long)]
        label: Option<String>,
        /// Overwrite an existing auth file.
        #[arg(long)]
        force: bool,
    },
    /// List configured accounts.
    List {
        /// Content source filter. Omit for all.
        #[arg(long)]
        source: Option<String>,
        /// Tab-separated values for scripts (source, account, name, locale, scan, auth).
        #[arg(short, long)]
        bare: bool,
    },
    /// Enable or disable an account for library scans (GUI: Include in library scan).
    SetScan {
        /// Account id, auth-file stem, or nickname.
        account: String,
        /// Include this account when scanning (default: true).
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        scan: bool,
    },
    /// Show token validity / refresh health across sources.
    Status {
        /// Content source filter. Omit for all.
        #[arg(long)]
        source: Option<String>,
    },
    /// Remove store credentials but keep acquired books and account rows.
    Revoke {
        /// Account id, auth-file stem, or nickname.
        account: String,
    },
}

pub async fn run(command: AuthCommand, config: &Config) -> anyhow::Result<()> {
    match command {
        AuthCommand::Login {
            source,
            marketplace,
            label,
            email,
            external,
            response_url,
            callback_bind,
            timeout,
            audible_username,
            force,
            no_qr,
            ascii_qr,
        } => {
            let registry = default_registry_with_plugins(config).await?;
            let source_id = resolve_source_id(&registry, &source)?;
            let content = registry.require(&source_id)?;
            match content.portal_auth_mode() {
                PortalAuthMode::Oauth => {
                    login_oauth(
                        config,
                        content.as_ref(),
                        marketplace,
                        label,
                        external,
                        response_url,
                        callback_bind,
                        timeout,
                        audible_username,
                        force,
                        no_qr,
                        ascii_qr,
                    )
                    .await
                }
                PortalAuthMode::Password => {
                    login_password(config, content.as_ref(), marketplace, label, email, force).await
                }
            }
        }
        AuthCommand::Import {
            path,
            source,
            libation_accounts,
            mkb79,
            label,
            force,
        } => {
            let registry = default_registry_with_plugins(config).await?;
            let source_id = resolve_source_id(&registry, &source)?;
            let content = registry.require(&source_id)?;
            let store = crate::registry::open_library(config).await?;
            let scope = store.scope(content.id());
            let accounts = content
                .import_credentials(
                    &scope,
                    &path,
                    ImportCredentialsOptions {
                        libation_accounts,
                        mkb79,
                        label,
                        force,
                    },
                )
                .await?;
            if accounts.is_empty() {
                eprintln!("no accounts found in {}", path.display());
            }
            for acct in accounts {
                println!(
                    "imported {} ({}) source={}",
                    acct.account_id, acct.marketplace, acct.source
                );
            }
            Ok(())
        }
        AuthCommand::List { source, bare } => {
            list_all_accounts(config, source.as_deref(), bare).await
        }
        AuthCommand::SetScan { account, scan } => {
            let store = crate::registry::open_library(config).await?;
            let account_id = if let Some(acct) = store.find_account(&account).await? {
                acct.account_id
            } else {
                let registry = default_registry_with_plugins(config).await?;
                let mut found = None;
                for src in registry.all() {
                    if let Ok(accounts) = src.list_accounts(&store.scope(src.id())).await {
                        if let Some(a) = accounts.into_iter().find(|a| {
                            a.account_id.eq_ignore_ascii_case(&account)
                                || a.label
                                    .as_deref()
                                    .is_some_and(|l| l.eq_ignore_ascii_case(&account))
                        }) {
                            found = Some(a);
                            break;
                        }
                    }
                }
                found.map(|a| a.account_id).ok_or_else(|| {
                    anyhow::anyhow!("account `{account}` not found — run `bookclerk auth list`")
                })?
            };
            if store.get_account(&account_id).await?.is_some() {
                store.set_scan_enabled(&account_id, scan).await?;
            } else {
                let registry = default_registry_with_plugins(config).await?;
                let mut info = None;
                for src in registry.all() {
                    if let Ok(accounts) = src.list_accounts(&store.scope(src.id())).await {
                        if let Some(a) = accounts.into_iter().find(|a| a.account_id == account_id) {
                            info = Some(a);
                            break;
                        }
                    }
                }
                let info = info.ok_or_else(|| {
                    anyhow::anyhow!(
                        "account {account_id} not in library DB — run `bookclerk library scan` first"
                    )
                })?;
                store
                    .upsert_account(
                        &account_id,
                        &info.marketplace,
                        info.label.as_deref(),
                        scan,
                        info.source.as_str(),
                    )
                    .await?;
            }
            println!(
                "account {} scan_enabled={}",
                account_id,
                if scan { "yes" } else { "no" }
            );
            Ok(())
        }
        AuthCommand::Status { source } => {
            let store = crate::registry::open_library(config).await?;
            let registry = default_registry_with_plugins(config).await?;
            let sources: Vec<_> = match source.as_deref() {
                Some(needle) => {
                    let id = resolve_source_id(&registry, needle)?;
                    vec![registry.require(&id)?]
                }
                None => registry.all(),
            };
            let mut any = false;
            for src in sources {
                let accounts = src.list_accounts(&store.scope(src.id())).await?;
                for acct in accounts {
                    any = true;
                    println!(
                        "{}\t{}\t{}\tstatus=present",
                        acct.source, acct.account_id, acct.marketplace
                    );
                }
            }
            if !any {
                eprintln!("no accounts configured");
            }
            Ok(())
        }
        AuthCommand::Revoke { account } => {
            let store = crate::registry::open_library(config).await?;
            let acct = store
                .find_account(&account)
                .await?
                .ok_or_else(|| anyhow::anyhow!("account `{account}` not found in library DB"))?;
            let registry = default_registry_with_plugins(config).await?;
            let scope = store.scope(acct.source.as_str());
            if let Ok(content) = registry.require(acct.source.as_str()) {
                if let Err(e) = content.revoke_credentials(&scope, &acct.account_id).await {
                    tracing::warn!(
                        error = %e,
                        source = %acct.source,
                        account = %acct.account_id,
                        "failed to revoke source credentials"
                    );
                }
            } else {
                // Unknown / disabled source — still clear common secret patterns.
                bookclerk_source::revoke_credentials_default(&scope, &acct.account_id).await?;
            }
            store.revoke_credentials(&acct.account_id).await?;
            println!(
                "revoked credentials for {} (books retained, scan_enabled=false)",
                acct.account_id
            );
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn login_oauth(
    config: &Config,
    source: &dyn bookclerk_source::ContentSource,
    marketplace: String,
    label: Option<String>,
    external: bool,
    response_url: Option<String>,
    callback_bind: SocketAddr,
    timeout: u64,
    audible_username: bool,
    force: bool,
    no_qr: bool,
    ascii_qr: bool,
) -> anyhow::Result<()> {
    let store = crate::registry::open_library(config).await?;
    let scope = store.scope(source.id());
    let mut extra = serde_json::Map::new();
    if audible_username {
        extra.insert("audible_username".into(), serde_json::Value::Bool(true));
    }
    if ascii_qr {
        extra.insert("ascii_qr".into(), serde_json::Value::Bool(true));
    }
    let opts = LoginOptions {
        marketplace,
        label,
        force,
        callback_bind: Some(callback_bind.to_string()),
        external: external || response_url.is_some(),
        response_url,
        show_qr: !no_qr,
        timeout_secs: Some(timeout),
        extra: serde_json::Value::Object(extra),
        ..Default::default()
    };

    let acct = source
        .login_with_oauth_progress(&scope, opts, &|progress| match progress {
            OAuthProgress::LoginUrl { url, qr } => {
                if let Some(qr) = qr {
                    println!("{qr}");
                } else {
                    println!("{url}");
                }
            }
            OAuthProgress::CallbackListening { addr } => {
                eprintln!("callback server listening on {addr}");
                if let Ok(sock) = addr.parse::<SocketAddr>() {
                    if sock.ip().is_loopback() {
                        eprintln!(
                            "On a remote host, forward the port: ssh -L {port}:localhost:{port} user@host",
                            port = sock.port()
                        );
                    }
                }
            }
            OAuthProgress::WaitingForCallback => {
                eprintln!("waiting for browser sign-in…");
            }
            OAuthProgress::Completed { account_id } => {
                eprintln!("login completed for {account_id}");
            }
        })
        .await?;

    scope
        .upsert_account(
            &acct.account_id,
            &acct.marketplace,
            acct.label.as_deref(),
            true,
        )
        .await?;
    println!(
        "authenticated {} ({}) → encrypted_secrets",
        acct.account_id, acct.marketplace,
    );
    Ok(())
}

async fn login_password(
    config: &Config,
    source: &dyn bookclerk_source::ContentSource,
    marketplace: String,
    label: Option<String>,
    email: Option<String>,
    force: bool,
) -> anyhow::Result<()> {
    let display_name = source.display_name();
    let email = email
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{display_name} login requires `--email`"))?;
    let password_env = source
        .password_env_var()
        .ok_or_else(|| anyhow::anyhow!("{display_name} has no password env var configured"))?;
    let password = resolve_password(password_env, display_name)?;

    let store = crate::registry::open_library(config).await?;
    let scope = store.scope(source.id());
    let acct = source
        .login(
            &scope,
            LoginOptions {
                marketplace,
                label,
                email: Some(email),
                password: Some(password),
                force,
                ..Default::default()
            },
        )
        .await?;
    scope
        .upsert_account(
            &acct.account_id,
            &acct.marketplace,
            acct.label.as_deref(),
            true,
        )
        .await?;
    println!(
        "authenticated {} ({}) source={}",
        acct.account_id, acct.marketplace, acct.source
    );
    Ok(())
}

fn resolve_password(password_env: &str, display_name: &str) -> anyhow::Result<String> {
    if let Ok(pw) = std::env::var(password_env) {
        let trimmed = pw.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }
    match rpassword::prompt_password(format!("{display_name} password: ")) {
        Ok(password) => {
            let password = password.trim_end_matches(['\r', '\n']).to_string();
            if password.is_empty() {
                anyhow::bail!("empty password — set {password_env} or re-run and enter a password");
            }
            Ok(password)
        }
        Err(err) => {
            anyhow::bail!("cannot prompt for password securely ({err}); set {password_env} instead")
        }
    }
}

async fn list_all_accounts(
    config: &Config,
    source_filter: Option<&str>,
    bare: bool,
) -> anyhow::Result<()> {
    let registry = default_registry_with_plugins(config).await?;
    let store = crate::registry::open_library(config).await?;
    let db_accounts = store.list_accounts().await?;

    let filter_id = match source_filter {
        Some(needle) => Some(resolve_source_id(&registry, needle)?),
        None => None,
    };

    let sources: Vec<_> = match filter_id.as_deref() {
        Some(id) => vec![registry.require(id)?],
        None => registry.all(),
    };

    let mut listed_ids = std::collections::HashSet::new();
    let mut any = false;

    let scan_by_id: std::collections::HashMap<String, bool> = db_accounts
        .iter()
        .map(|a| (a.account_id.clone(), a.scan_enabled))
        .collect();
    let scan_enabled =
        |account_id: &str| -> bool { scan_by_id.get(account_id).copied().unwrap_or(true) };

    for src in sources {
        let accounts = src.list_accounts(&store.scope(src.id())).await?;
        for acct in accounts {
            any = true;
            listed_ids.insert(acct.account_id.clone());
            let name = acct.label.as_deref().unwrap_or(&acct.account_id);
            let scan = yes_no(scan_enabled(&acct.account_id));
            let auth_ok = true;
            let status = String::from("ok");
            if bare {
                println!(
                    "{}\t{}\t{name}\t{}\t{scan}\t{}",
                    acct.source,
                    acct.account_id,
                    acct.marketplace,
                    yes_no(auth_ok)
                );
            } else {
                println!(
                    "{}\t{}\t{}\t{}\t{status}",
                    acct.source, acct.account_id, acct.marketplace, name
                );
            }
        }
    }

    // DB-only rows (e.g. after migrate) not covered by auth files.
    for db in db_accounts {
        if listed_ids.contains(db.account_id.as_str()) {
            continue;
        }
        if let Some(filter) = filter_id.as_deref() {
            if !db.source.eq_ignore_ascii_case(filter) {
                continue;
            }
        }
        any = true;
        let name = db.label.as_deref().unwrap_or(&db.account_id);
        if bare {
            println!(
                "{}\t{}\t{name}\t{}\t{}\tno",
                db.source,
                db.account_id,
                db.marketplace,
                yes_no(db.scan_enabled)
            );
        } else {
            println!(
                "{}\t{}\t{}\t{}\tdb_only",
                db.source, db.account_id, db.marketplace, name
            );
        }
    }

    if !any {
        eprintln!("no accounts configured — run `bookclerk auth login`");
    }
    Ok(())
}

fn yes_no(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}
