//! `libation auth` — login, import, list, status.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Subcommand;
use libation_audible::{
    begin_login, import_auth_file_with_options, import_libation_accounts_json,
    import_mkb79_auth_json, list_accounts, resolve_auth_file_async, AccountStatus,
    AuthLoginOptions, LoginMode, LoginProgress, QrRenderMode, SaveAuthOptions,
};
use libation_config::Config;
use libation_library::LibraryStore;
use libation_source::{LoginOptions, SourceKind};

use crate::registry::{default_registry, parse_source_kind};

const LIBRO_PASSWORD_ENV: &str = "LIBATION_LIBRO_PASSWORD";
const GA_PASSWORD_ENV: &str = "LIBATION_GA_PASSWORD";
const CHIRP_PASSWORD_ENV: &str = "LIBATION_CHIRP_PASSWORD";

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Log in to a content source (Audible OAuth or email/password stores).
    ///
    /// Audible: Amazon accounts with 2FA/MFA must complete OTP (or another
    /// challenge) in the browser — audible-rs OAuth has no username/password flags.
    ///
    /// Libro.fm / GraphicAudio / Chirp: require `--email`; password from
    /// `LIBATION_LIBRO_PASSWORD` / `LIBATION_GA_PASSWORD` / `LIBATION_CHIRP_PASSWORD`
    /// (or an interactive prompt — never pass the password on argv).
    Login {
        /// Content source (`audible`, `libro`, `graphicaudio`, or `chirp`).
        #[arg(long, default_value = "audible", value_parser = parse_source_kind)]
        source: SourceKind,
        /// Marketplace code (`us`, `uk`, `de`, …).
        #[arg(short = 'm', long, default_value = "us")]
        marketplace: String,
        /// Optional account label (also used as auth filename stem).
        #[arg(long)]
        label: Option<String>,
        /// Account email (required for `libro`, `graphicaudio`, `chirp`).
        #[arg(long)]
        email: Option<String>,
        /// Print authorize URL and paste redirect (instead of local login server).
        /// Audible only.
        #[arg(long)]
        external: bool,
        /// Amazon redirect URL (with `--external`; otherwise read from stdin).
        /// Audible only.
        #[arg(long)]
        response_url: Option<String>,
        /// Callback server bind address (login-server mode). Audible only.
        #[arg(long, default_value = "127.0.0.1:0")]
        callback_bind: SocketAddr,
        /// Seconds to wait for login-server capture. Audible only.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        /// Pre-merger Audible username login (de/us/uk only).
        #[arg(long)]
        audible_username: bool,
        /// Overwrite an existing auth file.
        #[arg(long)]
        force: bool,
        /// Disable terminal QR output. Audible only.
        #[arg(long)]
        no_qr: bool,
        /// Use ASCII QR instead of Unicode blocks. Audible only.
        #[arg(long)]
        ascii_qr: bool,
    },
    /// Import audible auth file or Libation AccountsSettings.json.
    Import {
        /// Path to auth file or AccountsSettings.json.
        path: PathBuf,
        /// Treat input as Libation AccountsSettings.json.
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
    /// List configured accounts (LibationCli: `list-accounts`).
    List {
        /// Content source filter. Omit for all.
        #[arg(long, value_parser = parse_source_kind)]
        source: Option<SourceKind>,
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
        #[arg(long, value_parser = parse_source_kind)]
        source: Option<SourceKind>,
    },
}

pub async fn run(command: AuthCommand, config: &Config) -> anyhow::Result<()> {
    let paths = config.paths();
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
        } => match source {
            SourceKind::Audible => {
                login_audible(
                    config,
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
            SourceKind::LibroFm => login_libro(config, marketplace, label, email, force).await,
            SourceKind::GraphicAudio => {
                login_email_password(
                    config,
                    SourceKind::GraphicAudio,
                    GA_PASSWORD_ENV,
                    "GraphicAudio",
                    marketplace,
                    label,
                    email,
                    force,
                )
                .await
            }
            SourceKind::Chirp => {
                login_email_password(
                    config,
                    SourceKind::Chirp,
                    CHIRP_PASSWORD_ENV,
                    "Chirp",
                    marketplace,
                    label,
                    email,
                    force,
                )
                .await
            }
        },
        AuthCommand::Import {
            path,
            libation_accounts,
            mkb79,
            label,
            force,
        } => {
            if libation_accounts || path.ends_with("AccountsSettings.json") {
                let accounts = import_libation_accounts_json(&path)?;
                let store = LibraryStore::open(&paths.library_db)?;
                for acct in &accounts {
                    store.upsert_account(
                        &acct.account_id,
                        &acct.marketplace,
                        acct.label.as_deref(),
                        true,
                    )?;
                    println!("imported {} ({})", acct.account_id, acct.marketplace);
                }
                if accounts.is_empty() {
                    eprintln!("no accounts found in {}", path.display());
                }
                Ok(())
            } else if mkb79 {
                let acct = import_mkb79_auth_json(&paths.files_dir, &path, label.as_deref(), force)
                    .await?;
                let store = LibraryStore::open(&paths.library_db)?;
                store.upsert_account(
                    &acct.account_id,
                    &acct.marketplace,
                    acct.label.as_deref(),
                    true,
                )?;
                println!(
                    "imported mkb79 account {} ({}) → {}",
                    acct.account_id,
                    acct.marketplace,
                    acct.auth_file.as_deref().unwrap_or("-")
                );
                Ok(())
            } else {
                let acct = import_auth_file_with_options(
                    &paths.files_dir,
                    &path,
                    label.as_deref(),
                    force,
                    SaveAuthOptions {
                        password_file: config.auth.password_file.as_deref(),
                        allow_plaintext: config.auth.allow_plaintext,
                    },
                )
                .await?;
                let store = LibraryStore::open(&paths.library_db)?;
                store.upsert_account(
                    &acct.account_id,
                    &acct.marketplace,
                    acct.label.as_deref(),
                    true,
                )?;
                println!(
                    "imported auth {} ({}) → {}",
                    acct.account_id,
                    acct.marketplace,
                    acct.auth_file.as_deref().unwrap_or("-")
                );
                Ok(())
            }
        }
        AuthCommand::List { source, bare } => list_all_accounts(config, source, bare).await,
        AuthCommand::SetScan { account, scan } => {
            let store = LibraryStore::open(&paths.library_db)?;
            let account_id = if let Some(acct) = store.find_account(&account)? {
                acct.account_id
            } else {
                let registry = default_registry(config);
                let mut found = None;
                for src in registry.all() {
                    if let Ok(accounts) = src.list_accounts(&paths.files_dir).await {
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
                if let Some(a) = found {
                    a.account_id
                } else {
                    // Fall back to Audible auth-file stem resolution.
                    let auth_path = resolve_auth_file_async(&paths.files_dir, &account).await?;
                    auth_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&account)
                        .to_string()
                }
            };
            if store.get_account(&account_id)?.is_some() {
                store.set_scan_enabled(&account_id, scan)?;
            } else {
                let registry = default_registry(config);
                let mut info = None;
                for src in registry.all() {
                    if let Ok(accounts) = src.list_accounts(&paths.files_dir).await {
                        if let Some(a) = accounts.into_iter().find(|a| a.account_id == account_id) {
                            info = Some(a);
                            break;
                        }
                    }
                }
                let info = info.ok_or_else(|| {
                    anyhow::anyhow!(
                        "account {account_id} not in library DB — run `libation library scan` first"
                    )
                })?;
                store.upsert_account_with_source(
                    &account_id,
                    &info.marketplace,
                    info.label.as_deref(),
                    scan,
                    info.source.as_str(),
                )?;
            }
            println!(
                "account {} scan_enabled={}",
                account_id,
                if scan { "yes" } else { "no" }
            );
            Ok(())
        }
        AuthCommand::Status { source } => {
            let registry = default_registry(config);
            let sources: Vec<_> = match source {
                Some(kind) => vec![registry.require(kind)?],
                None => registry.all(),
            };
            let mut any = false;
            for src in sources {
                let accounts = src.list_accounts(&paths.files_dir).await?;
                for acct in accounts {
                    any = true;
                    // Audible has richer AccountStatus; Libro lists as present.
                    let status = if acct.source == SourceKind::Audible {
                        list_accounts(&paths.files_dir)
                            .await?
                            .into_iter()
                            .find(|a| a.account_id == acct.account_id)
                            .map(|a| a.status.as_str().to_string())
                            .unwrap_or_else(|| "present".into())
                    } else {
                        String::from("present")
                    };
                    println!(
                        "{}\t{}\t{}\tstatus={status}",
                        acct.source, acct.account_id, acct.marketplace
                    );
                }
            }
            if !any {
                eprintln!("no accounts configured");
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn login_audible(
    config: &Config,
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
    let paths = config.paths();
    let mode = if external || response_url.is_some() {
        LoginMode::External
    } else {
        LoginMode::Server
    };
    let opts = AuthLoginOptions {
        marketplace,
        label,
        callback_bind,
        response_url,
        show_qr: !no_qr,
        qr_mode: if ascii_qr {
            QrRenderMode::Ascii
        } else {
            QrRenderMode::Unicode
        },
        files_dir: paths.files_dir.clone(),
        mode,
        timeout_secs: timeout,
        audible_username,
        force,
        password_file: config.auth.password_file.clone(),
        allow_plaintext: config.auth.allow_plaintext,
    };

    let session = begin_login(opts, |progress| match progress {
        LoginProgress::LoginUrl { url, qr } => {
            if let Some(qr) = qr {
                println!("{qr}");
            } else {
                println!("{url}");
            }
        }
        LoginProgress::CallbackListening { addr } => {
            eprintln!("callback server listening on {addr}");
            if addr.ip().is_loopback() {
                eprintln!(
                    "On a remote host, forward the port: ssh -L {port}:localhost:{port} user@host",
                    port = addr.port()
                );
            }
        }
        LoginProgress::WaitingForCallback => {
            eprintln!("waiting for browser sign-in…");
        }
        LoginProgress::Completed { account_id } => {
            eprintln!("login completed for {account_id}");
        }
    })
    .await?;

    let store = LibraryStore::open(&paths.library_db)?;
    if let Some(label) = session.label.as_deref() {
        if label != session.account_id {
            let _ = store.remap_account_id(label, &session.account_id);
        }
    }
    store.upsert_account_with_source(
        &session.account_id,
        &session.marketplace,
        session.label.as_deref(),
        true,
        SourceKind::Audible.as_str(),
    )?;
    println!(
        "authenticated {} ({}) → {}",
        session.account_id,
        session.marketplace,
        session.auth_file.display()
    );
    Ok(())
}

async fn login_libro(
    config: &Config,
    marketplace: String,
    label: Option<String>,
    email: Option<String>,
    force: bool,
) -> anyhow::Result<()> {
    login_email_password(
        config,
        SourceKind::LibroFm,
        LIBRO_PASSWORD_ENV,
        "Libro.fm",
        marketplace,
        label,
        email,
        force,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn login_email_password(
    config: &Config,
    kind: SourceKind,
    password_env: &str,
    display_name: &str,
    marketplace: String,
    label: Option<String>,
    email: Option<String>,
    force: bool,
) -> anyhow::Result<()> {
    let paths = config.paths();
    let email = email
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{display_name} login requires `--email`"))?;
    let password = resolve_password(password_env, display_name)?;

    let registry = default_registry(config);
    let source = registry.require(kind)?;
    let acct = source
        .login(
            &paths.files_dir,
            LoginOptions {
                marketplace,
                label,
                email: Some(email),
                password: Some(password),
                force,
            },
        )
        .await?;

    let store = LibraryStore::open(&paths.library_db)?;
    store.upsert_account_with_source(
        &acct.account_id,
        &acct.marketplace,
        acct.label.as_deref(),
        true,
        acct.source.as_str(),
    )?;
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
    source_filter: Option<SourceKind>,
    bare: bool,
) -> anyhow::Result<()> {
    let paths = config.paths();
    let registry = default_registry(config);
    let store = LibraryStore::open(&paths.library_db)?;
    let db_accounts = store.list_accounts()?;

    let sources: Vec<_> = match source_filter {
        Some(kind) => vec![registry.require(kind)?],
        None => registry.all(),
    };

    let mut listed_ids = std::collections::HashSet::new();
    let mut any = false;

    let scan_enabled = |account_id: &str| -> bool {
        store
            .get_account(account_id)
            .ok()
            .flatten()
            .map(|a| a.scan_enabled)
            .unwrap_or(true)
    };

    for src in sources {
        let accounts = src.list_accounts(&paths.files_dir).await?;
        for acct in accounts {
            any = true;
            listed_ids.insert(acct.account_id.clone());
            let name = acct.label.as_deref().unwrap_or(&acct.account_id);
            let scan = yes_no(scan_enabled(&acct.account_id));
            let auth_ok = if acct.source == SourceKind::Audible {
                list_accounts(&paths.files_dir)
                    .await?
                    .into_iter()
                    .find(|a| a.account_id == acct.account_id)
                    .map(|a| matches!(a.status, AccountStatus::Valid | AccountStatus::ExpiringSoon))
                    .unwrap_or(true)
            } else {
                true
            };
            if bare {
                println!(
                    "{}\t{}\t{name}\t{}\t{scan}\t{}",
                    acct.source,
                    acct.account_id,
                    acct.marketplace,
                    yes_no(auth_ok)
                );
            } else {
                let status = if acct.source == SourceKind::Audible {
                    list_accounts(&paths.files_dir)
                        .await?
                        .into_iter()
                        .find(|a| a.account_id == acct.account_id)
                        .map(|a| a.status.as_str().to_string())
                        .unwrap_or_else(|| "ok".into())
                } else {
                    String::from("ok")
                };
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
        if let Some(filter) = source_filter {
            if SourceKind::parse(&db.source) != Some(filter) {
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
        eprintln!("no accounts configured — run `libation auth login`");
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
