//! Serves one [`BindingDatabase`] over the granted channel for the SDK test
//! wrappers.
//!
//! The TypeScript and Python SDKs' `createTestDatabase({ profile })` spawn
//! this binary, write one JSON line to its stdin (the profile and the
//! plugin's `databaseMigrations` registration), read one JSON line from its
//! stdout (the loopback URL and the grant token) and then drive the binding
//! over `POST /db/execute`, `POST /db/execute-streamed` and `GET /db/limits`,
//! the routes a workerd isolate reaches on a deployed host. The process
//! exits when its stdin closes, so a wrapper that ends takes its database
//! with it.
//!
//! ```text
//! stdin:  {"profile":"minimum","migrations":[{"id":"m1","operations":[{"schema":"CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY NOT NULL, body TEXT NOT NULL)"}]}]}
//! stdout: {"url":"http://127.0.0.1:41234","token":"…","profile":"minimum"}
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bookclerk_db_testkit::{BindingDatabase, Profile};
use bookclerk_plugin_abi::PluginMigration;
use bookclerk_workerd::granted::{spawn_granted, GrantedSlot, GrantedTable};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// How long the grant stays valid: longer than any test run.
const GRANT_LIFETIME: Duration = Duration::from_secs(60 * 60 * 24 * 365);

/// The one request line a wrapper writes to stdin.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Serve {
    /// Profile name (`first-party`, `d1` or `minimum`).
    profile: String,
    /// The plugin's `databaseMigrations` registration, applied before the
    /// URL is announced.
    #[serde(default)]
    migrations: Vec<PluginMigration>,
}

/// The one reply line written to stdout once the binding is served.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Serving {
    /// Loopback origin the granted routes live under.
    url: String,
    /// Bearer token of the grant.
    token: String,
    /// The profile being enforced, as given.
    profile: &'static str,
}

/// Prints usage to stderr.
fn eprint_usage() {
    eprintln!(
        "\
bookclerk-db-testkit — serve a plugin database binding under a capability profile

Usage:
  bookclerk-db-testkit serve   read {{\"profile\",\"migrations\"}} from stdin, print {{\"url\",\"token\"}}
                               to stdout, serve until stdin closes
"
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") if args.len() == 1 => {}
        Some("-h" | "--help" | "help") => {
            eprint_usage();
            return ExitCode::SUCCESS;
        }
        _ => {
            eprint_usage();
            return ExitCode::from(2);
        }
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("bookclerk-db-testkit: runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    let local = tokio::task::LocalSet::new();
    match local.block_on(&runtime, serve()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("bookclerk-db-testkit: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Reads the request line, opens the binding, serves it and waits for stdin
/// to close.
///
/// # Errors
///
/// Returns the reason when the request line is missing or malformed, the
/// profile is unknown, the binding cannot be opened or the listener cannot
/// bind.
async fn serve() -> Result<(), String> {
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    let read = stdin
        .read_line(&mut line)
        .await
        .map_err(|err| format!("read stdin: {err}"))?;
    if read == 0 {
        return Err("stdin closed before the request line".into());
    }
    let request: Serve =
        serde_json::from_str(&line).map_err(|err| format!("request line: {err}"))?;
    let profile = Profile::parse(&request.profile).ok_or_else(|| {
        format!(
            "unknown profile `{}` (one of first-party, d1, minimum)",
            request.profile
        )
    })?;
    let database = BindingDatabase::open(profile, request.migrations)
        .await
        .map_err(|err| format!("open binding: {err}"))?;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|err| format!("bind loopback: {err}"))?;
    let port = listener
        .local_addr()
        .map_err(|err| format!("local address: {err}"))?
        .port();
    // The channel refuses a grant bearer that equals the isolate-wide bridge
    // token, so the two are minted apart; only the grant is announced.
    let token = uuid::Uuid::new_v4().simple().to_string();
    let bridge_token = uuid::Uuid::new_v4().simple().to_string();
    let table: GrantedTable = Rc::new(RefCell::new(HashMap::new()));
    table.borrow_mut().insert(
        token.clone(),
        GrantedSlot::database_only(Rc::new(database), Instant::now() + GRANT_LIFETIME),
    );
    spawn_granted(listener, bridge_token, table);

    let serving = Serving {
        url: format!("http://127.0.0.1:{port}"),
        token,
        profile: profile.name(),
    };
    let mut announce = serde_json::to_string(&serving).map_err(|err| format!("announce: {err}"))?;
    announce.push('\n');
    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(announce.as_bytes())
        .await
        .map_err(|err| format!("write stdout: {err}"))?;
    stdout
        .flush()
        .await
        .map_err(|err| format!("flush stdout: {err}"))?;

    // The wrapper holds stdin open for as long as it needs the database;
    // anything it writes after the request line is ignored.
    let mut sink = Vec::new();
    stdin
        .read_to_end(&mut sink)
        .await
        .map_err(|err| format!("wait for stdin: {err}"))?;
    Ok(())
}
