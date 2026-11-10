//! A plugin binding under the kit behaves like a granted `[[databases]]`
//! unit: the plugin's schema is applied, its SQL is scoped to its own
//! tables, every batch is held to the profile, and the `bookclerk-db-testkit`
//! binary serves the same binding over the granted channel.

// Test code: unwrap/expect are fine here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use bookclerk_db_testkit::{BindingDatabase, Profile};
use sea_orm::{ConnectionTrait, DbBackend, Statement};

use bookclerk_plugin_abi::{
    decode_db_capabilities_reply_bytes, decode_execute_result_reply_bytes,
    encoded_execute_request_bytes, DbPlanStatementKind, DbResultSelection, DbValue, ExecuteRequest,
    GuestDatabase, PluginErrorCode, PluginMigration, PluginMigrationOp, TypedDbStatement,
};

/// The plugin's registration: one table of notes.
fn migrations() -> Vec<PluginMigration> {
    vec![PluginMigration {
        id: "notes-v1".into(),
        operations: vec![PluginMigrationOp::Schema(
            "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY NOT NULL, body TEXT NOT NULL)".into(),
        )],
    }]
}

/// A write with `parameters`.
fn exec(sql: &str, parameters: Vec<DbValue>) -> TypedDbStatement {
    TypedDbStatement {
        sql: sql.to_string(),
        parameters,
        kind: DbPlanStatementKind::Execute,
        max_rows: 0,
        result_selection: DbResultSelection::AffectedRows,
    }
}

/// A read capped at `max_rows`.
fn query(sql: &str, parameters: Vec<DbValue>, max_rows: u32) -> TypedDbStatement {
    TypedDbStatement {
        sql: sql.to_string(),
        parameters,
        kind: DbPlanStatementKind::Select,
        max_rows,
        result_selection: DbResultSelection::Rows,
    }
}

/// A guest batch: the kit mints the operation id and stamps the hash.
fn request(statements: Vec<TypedDbStatement>) -> ExecuteRequest {
    ExecuteRequest {
        operation_id: String::new(),
        request_hash: String::new(),
        statements,
        deadline_unix_ms: 0,
    }
}

/// Inserts one note.
fn insert_note(id: i64, body: &str) -> TypedDbStatement {
    exec(
        "INSERT INTO notes (id, body) VALUES (?, ?)",
        vec![DbValue::Int64(id), DbValue::Text(body.into())],
    )
}

#[tokio::test]
async fn the_plugin_schema_is_applied_and_queried_under_every_profile() {
    for profile in Profile::ALL {
        let db = BindingDatabase::open(profile, migrations()).await.unwrap();
        assert_eq!(db.profile(), profile);
        assert_eq!(db.limits().await.unwrap(), profile.capabilities());

        db.execute(request(vec![insert_note(1, "first")]))
            .await
            .unwrap_or_else(|err| panic!("{profile:?}: insert: {err}"));
        let reply = db
            .execute(request(vec![query(
                "SELECT id, body FROM notes WHERE id = ?",
                vec![DbValue::Int64(1)],
                1,
            )]))
            .await
            .unwrap_or_else(|err| panic!("{profile:?}: select: {err}"));
        let row = &reply.statements[0].rows[0];
        assert_eq!(row.values[0], DbValue::Int64(1));
        assert_eq!(row.values[1], DbValue::Text("first".into()));
        assert_eq!(db.batch_sizes(), vec![1, 1]);

        // The unit behind the binding shows what the plugin wrote.
        let written = db
            .connection()
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COUNT(*) AS n FROM sqlite_master WHERE type = 'table' AND name LIKE '%notes'",
            ))
            .await
            .unwrap()
            .expect("one count row");
        assert_eq!(written.try_get::<i64>("", "n").unwrap(), 1, "{profile:?}");
    }
}

#[tokio::test]
async fn the_binding_only_reaches_its_own_tables() {
    let db = BindingDatabase::open(Profile::FirstParty, migrations())
        .await
        .unwrap();
    let err = db
        .execute(request(vec![query(
            "SELECT operation_id FROM bookclerk_receipts",
            vec![],
            1,
        )]))
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::InvalidParams, "{err}");
    let err = db
        .execute(request(vec![query("SELECT id FROM books", vec![], 1)]))
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::InvalidParams, "{err}");
}

#[tokio::test]
async fn a_parameter_past_the_payload_cap_is_a_typed_limit_error() {
    let db = BindingDatabase::open(Profile::Minimum, migrations())
        .await
        .unwrap();
    let caps = db.capabilities().clone();
    let body = "x".repeat(usize::try_from(caps.max_payload_bytes).unwrap() + 1);
    let err = db
        .execute(request(vec![insert_note(1, &body)]))
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::LimitExceeded, "{err}");
    let limit = err.limit().expect("typed limit");
    assert_eq!(limit.limit, "maxPayloadBytes");
    assert_eq!(limit.cap, u64::from(caps.max_payload_bytes));
}

#[tokio::test]
async fn a_result_cell_past_the_profile_cap_is_a_typed_limit_error() {
    let db = BindingDatabase::open(Profile::Minimum, migrations())
        .await
        .unwrap();
    let caps = db.capabilities().clone();
    // The parameter fits the payload cap; the concatenated result cell does
    // not fit the cell cap.
    let body = "x".repeat(usize::try_from(caps.max_cell_bytes).unwrap() / 2 + 1);
    db.execute(request(vec![insert_note(1, &body)]))
        .await
        .unwrap();
    let err = db
        .execute(request(vec![query(
            "SELECT body || body AS twice FROM notes WHERE id = ?",
            vec![DbValue::Int64(1)],
            1,
        )]))
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::LimitExceeded, "{err}");
    let limit = err.limit().expect("typed limit");
    assert_eq!(limit.limit, "maxCellBytes");
    assert_eq!(limit.cap, u64::from(caps.max_cell_bytes));
}

#[tokio::test]
async fn a_batch_is_one_transaction() {
    let db = BindingDatabase::open(Profile::D1, migrations())
        .await
        .unwrap();
    db.execute(request(vec![insert_note(1, "first")]))
        .await
        .unwrap();
    // The second insert repeats the primary key, so the first must roll back.
    let err = db
        .execute(request(vec![
            insert_note(2, "second"),
            insert_note(1, "again"),
        ]))
        .await
        .unwrap_err();
    assert_ne!(err.code, PluginErrorCode::InvalidParams, "{err}");
    let reply = db
        .execute(request(vec![query(
            "SELECT id FROM notes ORDER BY id",
            vec![],
            10,
        )]))
        .await
        .unwrap();
    assert_eq!(reply.statements[0].rows.len(), 1);
}

#[tokio::test]
async fn streamed_reads_follow_the_profile() {
    let minimum = BindingDatabase::open(Profile::Minimum, migrations())
        .await
        .unwrap();
    let err = minimum
        .execute_streamed(request(vec![query("SELECT id FROM notes", vec![], 1)]))
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::Unsupported, "{err}");

    let first_party = BindingDatabase::open(Profile::FirstParty, migrations())
        .await
        .unwrap();
    first_party
        .execute(request(vec![
            insert_note(1, "first"),
            insert_note(2, "second"),
        ]))
        .await
        .unwrap();
    let stream = first_party
        .execute_streamed(request(vec![query(
            "SELECT id FROM notes ORDER BY id",
            vec![],
            10,
        )]))
        .await
        .unwrap();
    let limits =
        bookclerk_plugin_abi::ExecuteStreamLimits::from_capabilities(first_party.capabilities());
    let reply = stream.collect(limits).await.unwrap();
    assert_eq!(reply.statements[0].rows.len(), 2);
}

/// A registration whose seed row is longer, in bytes, than `profile`'s
/// statement payload cap.
fn migration_past_the_payload_cap(profile: Profile) -> Vec<PluginMigration> {
    let cap = usize::try_from(profile.capabilities().max_payload_bytes).unwrap();
    vec![PluginMigration {
        id: "seeded-v1".into(),
        operations: vec![
            PluginMigrationOp::Schema(
                "CREATE TABLE IF NOT EXISTS seeded (id INTEGER PRIMARY KEY NOT NULL, body TEXT NOT NULL)"
                    .into(),
            ),
            PluginMigrationOp::Data(format!(
                "INSERT INTO seeded (id, body) VALUES (1, '{}')",
                "x".repeat(cap)
            )),
        ],
    }]
}

#[tokio::test]
async fn a_migration_past_the_profile_payload_cap_is_refused_as_the_adapter_refuses_it() {
    let migrations = migration_past_the_payload_cap(Profile::Minimum);
    let err = BindingDatabase::open(Profile::Minimum, migrations.clone())
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::LimitExceeded, "{err}");
    assert!(err.message.contains("maxPayloadBytes"), "{err}");
    // The same registration applies where the cap is wider.
    BindingDatabase::open(Profile::FirstParty, migrations)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_registration_that_fails_the_proof_is_refused() {
    let err = BindingDatabase::open(
        Profile::FirstParty,
        vec![PluginMigration {
            id: "bad".into(),
            operations: vec![PluginMigrationOp::Schema(
                "CREATE TABLE IF NOT EXISTS bookclerk_receipts (x INTEGER)".into(),
            )],
        }],
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::InvalidParams, "{err}");
}

/// One HTTP/1.1 exchange with the served binding, returning the status and
/// the body.
fn http(url: &str, token: &str, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let host = url.strip_prefix("http://").unwrap();
    let mut stream = std::net::TcpStream::connect(host).unwrap();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head");
    let head = String::from_utf8_lossy(&response[..split]);
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status");
    (status, response[split + 4..].to_vec())
}

#[test]
fn the_binary_serves_the_binding_over_the_granted_channel_until_stdin_closes() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_bookclerk-db-testkit"))
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let serve = serde_json::json!({
        "profile": "minimum",
        "migrations": [{
            "id": "notes-v1",
            "operations": [{"schema": "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY NOT NULL, body TEXT NOT NULL)"}],
        }],
    });
    writeln!(stdin, "{serve}").unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let announced: serde_json::Value = serde_json::from_str(&line).unwrap();
    let url = announced["url"].as_str().unwrap();
    let token = announced["token"].as_str().unwrap();
    assert_eq!(announced["profile"], "minimum");

    let (status, body) = http(url, token, "GET", "/db/limits", &[]);
    assert_eq!(status, 200);
    let caps = decode_db_capabilities_reply_bytes(&body).unwrap();
    assert_eq!(caps, Profile::Minimum.capabilities());

    let insert = encoded_execute_request_bytes(&request(vec![insert_note(1, "first")])).unwrap();
    let (status, body) = http(url, token, "POST", "/db/execute", &insert);
    assert_eq!(status, 200);
    decode_execute_result_reply_bytes(&body).unwrap();

    // `ORDER BY` is rewritten by the desugaring; the channel and the
    // binding have to agree on the request they hash.
    let select = encoded_execute_request_bytes(&request(vec![query(
        "SELECT body FROM notes WHERE id = ? ORDER BY id",
        vec![DbValue::Int64(1)],
        1,
    )]))
    .unwrap();
    let (status, body) = http(url, token, "POST", "/db/execute", &select);
    assert_eq!(status, 200);
    let reply = decode_execute_result_reply_bytes(&body).unwrap();
    assert_eq!(
        reply.statements[0].rows[0].values[0],
        DbValue::Text("first".into())
    );

    let (status, _) = http(url, "not-the-token", "GET", "/db/limits", &[]);
    assert_eq!(status, 401);

    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "{status}");
}

#[test]
fn the_binary_refuses_an_unknown_profile_and_bad_usage() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_bookclerk-db-testkit"))
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), r#"{{"profile":"huge"}}"#).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown profile"));

    let out = Command::new(env!("CARGO_BIN_EXE_bookclerk-db-testkit"))
        .arg("frobnicate")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Usage"));

    let out = Command::new(env!("CARGO_BIN_EXE_bookclerk-db-testkit"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", out.status);
    assert!(String::from_utf8_lossy(&out.stderr).contains("Usage"));

    // Stdin closed before the request line: no binding is served.
    let out = Command::new(env!("CARGO_BIN_EXE_bookclerk-db-testkit"))
        .arg("serve")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("stdin closed before the request line"));
}
