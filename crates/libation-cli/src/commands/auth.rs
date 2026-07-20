//! `libation auth` — login, import, list, status.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Subcommand;
use libation_audible::{
    begin_login, import_auth_file, import_libation_accounts_json, import_mkb79_auth_json,
    list_accounts, resolve_auth_file_async, AccountStatus, AuthLoginOptions, LoginMode,
    LoginProgress, QrRenderMode,
};
use libation_config::Config;
use libation_library::LibraryStore;

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Log in via browser / QR (LibationCli: `login-external`).
    Login {
        /// Marketplace code (`us`, `uk`, `de`, …).
        #[arg(short = 'm', long, default_value = "us")]
        marketplace: String,
        /// Optional account label (also used as auth filename stem).
        #[arg(long)]
        label: Option<String>,
        /// Print authorize URL and paste redirect (instead of local login server).
        #[arg(long)]
        external: bool,
        /// Amazon redirect URL (with `--external`; otherwise read from stdin).
        #[arg(long)]
        response_url: Option<String>,
        /// Callback server bind address (login-server mode).
        #[arg(long, default_value = "127.0.0.1:0")]
        callback_bind: SocketAddr,
        /// Seconds to wait for login-server capture.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        /// Pre-merger Audible username login (de/us/uk only).
        #[arg(long)]
        audible_username: bool,
        /// Overwrite an existing auth file.
        #[arg(long)]
        force: bool,
        /// Disable terminal QR output.
        #[arg(long)]
        no_qr: bool,
        /// Use ASCII QR instead of Unicode blocks.
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
        /// Tab-separated values for scripts (account, name, locale, scan, auth).
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
    /// Show token validity / refresh health.
    Status,
}

pub async fn run(command: AuthCommand, config: &Config) -> anyhow::Result<()> {
    let paths = config.paths();
    match command {
        AuthCommand::Login {
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
        } => {
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
            store.upsert_account(
                &session.account_id,
                &session.marketplace,
                session.label.as_deref(),
                true,
            )?;
            println!(
                "authenticated {} ({}) → {}",
                session.account_id,
                session.marketplace,
                session.auth_file.display()
            );
            Ok(())
        }
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
                let acct =
                    import_auth_file(&paths.files_dir, &path, label.as_deref(), force).await?;
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
        AuthCommand::List { bare } => {
            let accounts = list_accounts(&paths.files_dir).await?;
            let store = LibraryStore::open(&paths.library_db)?;
            let db_accounts = store.list_accounts()?;
            if accounts.is_empty() && db_accounts.is_empty() {
                eprintln!("no accounts configured — run `libation auth login`");
                return Ok(());
            }

            let scan_enabled = |account_id: &str| -> bool {
                store
                    .get_account(account_id)
                    .ok()
                    .flatten()
                    .map(|a| a.scan_enabled)
                    .unwrap_or(true)
            };

            if bare {
                for acct in &accounts {
                    let name = acct.label.as_deref().unwrap_or(&acct.account_id);
                    let scan = yes_no(scan_enabled(&acct.account_id));
                    let auth = yes_no(matches!(
                        acct.status,
                        AccountStatus::Valid | AccountStatus::ExpiringSoon
                    ));
                    println!(
                        "{}\t{}\t{}\t{scan}\t{auth}",
                        acct.account_id, name, acct.marketplace
                    );
                }
                let auth_ids: std::collections::HashSet<_> =
                    accounts.iter().map(|a| a.account_id.as_str()).collect();
                for db in db_accounts {
                    if auth_ids.contains(db.account_id.as_str()) {
                        continue;
                    }
                    let name = db.label.as_deref().unwrap_or(&db.account_id);
                    println!(
                        "{}\t{name}\t{}\t{}\tno",
                        db.account_id,
                        db.marketplace,
                        yes_no(db.scan_enabled)
                    );
                }
                return Ok(());
            }

            for acct in &accounts {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    acct.account_id,
                    acct.marketplace,
                    acct.label.as_deref().unwrap_or("-"),
                    acct.status.as_str(),
                    acct.auth_file.as_deref().unwrap_or("-")
                );
            }
            let auth_ids: std::collections::HashSet<_> =
                accounts.iter().map(|a| a.account_id.as_str()).collect();
            for db in db_accounts {
                if auth_ids.contains(db.account_id.as_str()) {
                    continue;
                }
                println!(
                    "{}\t{}\t{}\tdb_only\t-",
                    db.account_id,
                    db.marketplace,
                    db.label.as_deref().unwrap_or("-")
                );
            }
            Ok(())
        }
        AuthCommand::SetScan { account, scan } => {
            let store = LibraryStore::open(&paths.library_db)?;
            let account_id = if let Some(acct) = store.find_account(&account)? {
                acct.account_id
            } else {
                let auth_path = resolve_auth_file_async(&paths.files_dir, &account).await?;
                let auth_path_str = auth_path.display().to_string();
                list_accounts(&paths.files_dir)
                    .await?
                    .into_iter()
                    .find(|a| {
                        a.auth_file.as_deref() == Some(auth_path_str.as_str())
                            || a.account_id.eq_ignore_ascii_case(&account)
                            || a.label
                                .as_deref()
                                .is_some_and(|l| l.eq_ignore_ascii_case(&account))
                    })
                    .map(|a| a.account_id)
                    .unwrap_or_else(|| {
                        auth_path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or(&account)
                            .to_string()
                    })
            };
            if store.get_account(&account_id)?.is_some() {
                store.set_scan_enabled(&account_id, scan)?;
            } else {
                let info = list_accounts(&paths.files_dir)
                    .await?
                    .into_iter()
                    .find(|a| a.account_id == account_id)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "account {account_id} not in library DB — run `libation library scan` first"
                        )
                    })?;
                store.upsert_account(
                    &account_id,
                    &info.marketplace,
                    info.label.as_deref(),
                    scan,
                )?;
            }
            println!(
                "account {} scan_enabled={}",
                account_id,
                if scan { "yes" } else { "no" }
            );
            Ok(())
        }
        AuthCommand::Status => {
            let accounts = list_accounts(&paths.files_dir).await?;
            if accounts.is_empty() {
                eprintln!("no accounts configured");
                return Ok(());
            }
            for acct in accounts {
                println!(
                    "{}\t{}\tstatus={}",
                    acct.account_id,
                    acct.marketplace,
                    acct.status.as_str()
                );
            }
            Ok(())
        }
    }
}

fn yes_no(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}
