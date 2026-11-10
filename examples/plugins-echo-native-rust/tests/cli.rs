//! The `cli` entrypoint over the kit's `DB` binding, granted the way the host
//! grants it.

// Test code: unwrap/expect are fine here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use bookclerk_db_testkit::selected_profile;
use bookclerk_plugin_echo_native_rust::cli::NotesCli;
use bookclerk_plugin_sdk::{
    CliArg, CliInvokeParams, CliInvokeResult, DatabaseBinding, LimitExceeded, PluginCli,
    PluginError, PluginErrorCode,
};
use support::{bind, served, Scripted};

/// Invokes one CLI command with `db` granted as `DB`.
async fn run(
    db: &DatabaseBinding,
    command: &str,
    args: &[(&str, &str)],
) -> Result<CliInvokeResult, PluginError> {
    let params = CliInvokeParams {
        command: command.into(),
        args: args
            .iter()
            .map(|(name, value)| CliArg {
                name: (*name).into(),
                value: (*value).into(),
            })
            .collect(),
    };
    NotesCli::new(Some(db.clone())).invoke(params).await
}

/// The lines of `stdout`.
fn lines(stdout: &str) -> Vec<&str> {
    stdout.lines().collect()
}

#[tokio::test]
async fn the_cli_schema_lists_every_command() {
    let schema = NotesCli::new(None).describe().await.unwrap();
    let names: Vec<_> = schema.commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["ping", "notes-add", "notes-list"]);
}

#[tokio::test]
async fn ping_echoes_its_message() {
    let s = served().await;
    let r = run(&s.db, "ping", &[("message", "hello")]).await.unwrap();
    assert_eq!(r.exit_code, 0);
    assert_eq!(r.stdout, "pong: hello\n");
}

#[tokio::test]
async fn notes_add_stores_the_note_count_times_and_notes_list_prints_them() {
    let s = served().await;
    let added = run(&s.db, "notes-add", &[("body", "remember"), ("count", "3")])
        .await
        .unwrap();
    assert_eq!(added.exit_code, 0, "{}", added.stderr);
    assert_eq!(added.stdout, "stored 3; 3 in total\n");
    let listed = run(&s.db, "notes-list", &[]).await.unwrap();
    assert_eq!(listed.exit_code, 0, "{}", listed.stderr);
    let printed = lines(&listed.stdout);
    assert_eq!(printed.len(), 3);
    for line in printed {
        let (id, body) = line.split_once(": ").unwrap();
        assert!(id.parse::<i64>().is_ok(), "{line}");
        assert_eq!(body, "remember");
    }
}

#[tokio::test]
async fn notes_add_refuses_a_count_out_of_range() {
    let s = served().await;
    for count in [
        "0", "-1", "1.5", "lots", "10001", "0x10", "1e3", " 5 ", "", "５",
    ] {
        let r = run(&s.db, "notes-add", &[("body", "x"), ("count", count)])
            .await
            .unwrap();
        assert_eq!(r.exit_code, 2, "{count}");
        assert!(r.stderr.contains("--count must be 1 to 10000"), "{count}");
    }
}

#[tokio::test]
async fn notes_list_stops_at_limit_and_refuses_a_limit_out_of_range() {
    let s = served().await;
    run(&s.db, "notes-add", &[("body", "many"), ("count", "5")])
        .await
        .unwrap();
    let r = run(&s.db, "notes-list", &[("limit", "2")]).await.unwrap();
    assert_eq!(r.exit_code, 0, "{}", r.stderr);
    assert_eq!(lines(&r.stdout).len(), 2);
    for limit in ["0", "1001", "two"] {
        let bad = run(&s.db, "notes-list", &[("limit", limit)]).await.unwrap();
        assert_eq!(bad.exit_code, 2, "{limit}");
        assert!(bad.stderr.contains("--limit must be 1 to 1000"), "{limit}");
    }
}

#[tokio::test]
async fn notes_add_says_how_many_notes_a_failed_write_had_already_stored() {
    let profile = selected_profile().unwrap();
    let count = {
        let s = served().await;
        (s.caps.max_statements * 2).to_string()
    };
    let limit = PluginError::limit_exceeded(LimitExceeded::new("maxRequestBytes", 1, 2));
    let (db, _) = bind(Scripted::open(profile).await.failing_batch(2, limit));
    let r = run(&db, "notes-add", &[("body", "b"), ("count", &count)])
        .await
        .unwrap();
    assert_eq!(r.exit_code, 1);
    let (first, rest) = r.stderr.split_once('\n').unwrap();
    assert_eq!(
        first,
        "the database refused the request: maxRequestBytes is 1, it needed 2"
    );
    assert!(
        rest.ends_with(" notes were stored before the failure\n"),
        "{rest}"
    );

    // Any other error keeps its code, with how many notes were stored.
    let broken = PluginError::unavailable("unavailable");
    let (db, _) = bind(
        Scripted::open(profile)
            .await
            .failing_batch(2, broken.clone()),
    );
    let err = run(&db, "notes-add", &[("body", "b"), ("count", &count)])
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::Unavailable);
    assert!(
        err.message
            .ends_with(" notes were stored before the failure: unavailable"),
        "{err}"
    );

    // Nothing stored: the error as the binding returned it.
    let (db, _) = bind(
        Scripted::open(profile)
            .await
            .failing_batch(1, broken.clone()),
    );
    let err = run(&db, "notes-add", &[("body", "b")]).await.unwrap_err();
    assert_eq!(err, broken);
}

#[tokio::test]
async fn notes_add_reports_the_cap_a_note_is_too_large_for() {
    let s = served().await;
    let budget = usize::try_from(s.caps.max_cell_bytes.min(s.caps.max_payload_bytes)).unwrap();
    let body = "x".repeat(budget + 1);
    let r = run(&s.db, "notes-add", &[("body", &body)]).await.unwrap();
    assert_eq!(r.exit_code, 1);
    let rest = r
        .stderr
        .strip_prefix("the database refused the request: max")
        .unwrap_or_else(|| panic!("{}", r.stderr));
    assert!(
        rest.contains(" is ") && rest.contains(", it needed "),
        "{rest}"
    );
    assert!(
        rest.ends_with('\n') && !rest.trim_end().contains('\n'),
        "{rest}"
    );
}

#[tokio::test]
async fn an_unknown_command_is_a_usage_error() {
    let s = served().await;
    let r = run(&s.db, "nope", &[]).await.unwrap();
    assert_eq!(r.exit_code, 2);
    assert_eq!(r.stderr, "unknown command nope\n");
}

#[tokio::test]
async fn the_notes_commands_need_the_db_binding() {
    let params = CliInvokeParams {
        command: "notes-list".into(),
        args: Vec::new(),
    };
    let err = NotesCli::new(None).invoke(params).await.unwrap_err();
    assert_eq!(err.code, PluginErrorCode::Unavailable);
}
