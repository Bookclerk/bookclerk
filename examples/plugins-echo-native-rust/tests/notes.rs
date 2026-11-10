//! The notes helpers against the binding a host grants.
//!
//! The binding is served by `bookclerk_db_testkit::BindingDatabase` under one
//! capability profile. Run through `bookclerk-plugin test .` (`cargo plugin
//! -- test examples/plugins-echo-native-rust` in the workspace), which runs
//! this suite once per profile and names the profile in the environment.
//! Every assertion is sized from the profile's own limits, so the same tests
//! prove the code on the shipped adapters, on D1 and on the smallest
//! advertisement a host accepts.

// Test code: unwrap/expect are fine here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::HashMap;

use bookclerk_db_testkit::Profile;
use bookclerk_plugin_echo_native_rust::notes::{
    add_notes, count_notes, migrations_for, note_from_row, note_too_large, Note, NotesReader,
    NOTES_BINDING,
};
use bookclerk_plugin_sdk::{DatabaseBinding, DbValue, LimitExceeded, PluginError};
use support::{bind, served, too_wide, Scripted, Served};

/// Every note the reader hands out.
async fn all_notes(db: &DatabaseBinding, limit: Option<usize>) -> Result<Vec<Note>, PluginError> {
    let mut reader = NotesReader::new(db, limit);
    let mut notes = Vec::new();
    while let Some(note) = reader.next().await {
        notes.push(note?);
    }
    Ok(notes)
}

/// The most bytes one note may hold under the profile.
///
/// A statement's SQL and binds share `maxPayloadBytes` (3,667 bytes on D1),
/// and a reply cell is held to `maxCellBytes`.
fn note_budget(served: &Served) -> usize {
    let caps = &served.caps;
    usize::try_from(caps.max_cell_bytes.min(caps.max_payload_bytes)).unwrap()
}

/// Note bodies that together pass the profile's inline result cap.
///
/// Neither a single inline read nor a single request holds them all. Each is
/// half the per-note budget, in multibyte text so byte and character counts
/// differ ("né" is 3 bytes).
fn bodies_past_the_inline_cap(served: &Served) -> Vec<String> {
    let body = "né".repeat(note_budget(served) / 2 / 3);
    let max_result = usize::try_from(served.caps.max_result_bytes).unwrap();
    let count = (2 * max_result).div_ceil(body.len()) + 1;
    (0..count).map(|i| format!("{i} {body}")).collect()
}

/// Strings from string slices.
fn owned(bodies: &[&str]) -> Vec<String> {
    bodies.iter().map(|b| (*b).to_owned()).collect()
}

#[tokio::test]
async fn the_migrations_create_the_notes_table() {
    let s = served().await;
    assert_eq!(count_notes(&s.db).await.unwrap(), 0);
}

#[tokio::test]
async fn the_migrations_hook_answers_for_the_db_binding_only() {
    assert_eq!(migrations_for(NOTES_BINDING).len(), 1);
    assert!(migrations_for("OTHER").is_empty());
}

#[tokio::test]
async fn add_notes_splits_a_long_write_into_requests_that_fit() {
    let s = served().await;
    let bodies = bodies_past_the_inline_cap(&s);
    let stored = add_notes(&s.db, &bodies, |_| {}).await.unwrap();
    assert_eq!(stored, bodies.len());
    assert_eq!(
        count_notes(&s.db).await.unwrap(),
        i64::try_from(bodies.len()).unwrap()
    );
}

#[tokio::test]
async fn the_reader_returns_every_note_in_order_past_the_inline_cap() {
    let s = served().await;
    let bodies = bodies_past_the_inline_cap(&s);
    add_notes(&s.db, &bodies, |_| {}).await.unwrap();
    let notes = all_notes(&s.db, None).await.unwrap();
    assert_eq!(
        notes.iter().map(|n| n.body.clone()).collect::<Vec<_>>(),
        bodies
    );
    assert!(notes.windows(2).all(|w| w[0].id < w[1].id));
    // The plain inline read the reader avoids is refused with the typed
    // error whenever the session cannot stream it.
    if !s.caps.streams_results() {
        let refused =
            s.db.prepare("SELECT id, body FROM notes")
                .all(None)
                .await
                .unwrap_err();
        assert!(refused.limit().is_some(), "{refused}");
    }
}

#[tokio::test]
async fn the_reader_yields_nothing_for_an_empty_table_or_a_zero_limit() {
    let s = served().await;
    assert!(all_notes(&s.db, None).await.unwrap().is_empty());
    add_notes(&s.db, &owned(&["a"]), |_| {}).await.unwrap();
    assert!(all_notes(&s.db, Some(0)).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_reader_stops_at_its_limit() {
    let s = served().await;
    add_notes(&s.db, &owned(&["a", "b", "c"]), |_| {})
        .await
        .unwrap();
    let notes = all_notes(&s.db, Some(2)).await.unwrap();
    assert_eq!(
        notes.iter().map(|n| n.body.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(count_notes(&s.db).await.unwrap(), 3);
}

#[tokio::test]
async fn a_note_past_the_per_note_budget_is_a_typed_limit_and_nothing_is_stored() {
    let s = served().await;
    let refused = add_notes(&s.db, &["x".repeat(note_budget(&s) + 1)], |_| {})
        .await
        .unwrap_err();
    let limit = refused
        .limit()
        .unwrap_or_else(|| panic!("expected limit_exceeded, got {refused}"));
    assert!(
        ["maxCellBytes", "maxPayloadBytes", "maxRequestBytes"].contains(&limit.limit.as_str()),
        "{limit:?}"
    );
    assert!(limit.used > limit.cap);
    assert_eq!(count_notes(&s.db).await.unwrap(), 0);
}

#[tokio::test]
async fn add_notes_reports_each_committed_batch() {
    let s = served().await;
    let count = usize::try_from(s.caps.max_statements).unwrap() * 2 + 1;
    let bodies: Vec<String> = (0..count).map(|i| format!("n{i}")).collect();
    let mut seen = Vec::new();
    assert_eq!(
        add_notes(&s.db, &bodies, |n| seen.push(n)).await.unwrap(),
        count
    );
    assert!(seen.len() >= 3, "batches {seen:?}");
    assert_eq!(seen.last(), Some(&count));
    assert!(seen.windows(2).all(|w| w[0] < w[1]));
}

/// Five notes on a scripted binding that pages `max_rows` at a time.
async fn five_notes(scripted: Scripted) -> (DatabaseBinding, std::sync::Arc<Scripted>) {
    let (db, handle) = bind(scripted);
    let bodies: Vec<String> = (1..=5).map(|i| format!("note {i}")).collect();
    add_notes(&db, &bodies, |_| {}).await.unwrap();
    (db, handle)
}

#[tokio::test]
async fn the_reader_halves_a_wide_page_then_grows_back() {
    // Four rows a page with no streamed reads, where note 2 is so wide that
    // it fits a page only alone.
    let scripted = Scripted::open(Profile::D1)
        .await
        .paging(4)
        .refusing(|after, size| (after < 2 && after + size >= 2 && size > 1).then(too_wide));
    let (db, handle) = five_notes(scripted).await;
    let notes = all_notes(&db, None).await.unwrap();
    assert_eq!(
        notes.iter().map(|n| n.id).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
    // Halved to fit note 2, then doubled back to the row cap after it.
    let sizes: Vec<i64> = handle
        .reads
        .lock()
        .unwrap()
        .iter()
        .map(|(_, s)| *s)
        .collect();
    assert_eq!(sizes, [4, 2, 1, 2, 1, 2, 4]);
}

#[tokio::test]
async fn the_reader_reads_no_page_after_the_caller_stops() {
    let (db, handle) = five_notes(Scripted::open(Profile::D1).await.paging(2)).await;
    let mut reader = NotesReader::new(&db, None);
    assert_eq!(reader.next().await.unwrap().unwrap().id, 1);
    drop(reader);
    assert_eq!(*handle.reads.lock().unwrap(), [(0, 2)]);
}

#[tokio::test]
async fn the_reader_asks_for_no_more_rows_than_its_limit() {
    let (db, handle) = five_notes(Scripted::open(Profile::D1).await.paging(4)).await;
    let notes = all_notes(&db, Some(3)).await.unwrap();
    assert_eq!(notes.iter().map(|n| n.id).collect::<Vec<_>>(), [1, 2, 3]);
    let sizes: Vec<i64> = handle
        .reads
        .lock()
        .unwrap()
        .iter()
        .map(|(_, s)| *s)
        .collect();
    assert_eq!(sizes, [3]);
}

#[tokio::test]
async fn one_row_past_the_cap_or_any_other_error_is_not_retried() {
    let scripted = Scripted::open(Profile::D1)
        .await
        .paging(1)
        .refusing(|_, _| Some(too_wide()));
    let (db, _handle) = five_notes(scripted).await;
    assert_eq!(all_notes(&db, None).await.unwrap_err(), too_wide());

    let broken = PluginError::unavailable("unavailable");
    let failing = broken.clone();
    let scripted = Scripted::open(Profile::D1)
        .await
        .paging(4)
        .refusing(move |_, _| Some(failing.clone()));
    let (db, handle) = five_notes(scripted).await;
    assert_eq!(all_notes(&db, None).await.unwrap_err(), broken);
    assert_eq!(handle.reads.lock().unwrap().len(), 1);
}

#[test]
fn note_too_large_reads_as_a_typed_limit() {
    let err = note_too_large(5, 4);
    assert_eq!(err.limit(), Some(LimitExceeded::new("maxCellBytes", 4, 5)));
    assert_eq!(err.message, "limit exceeded: maxCellBytes is 4; used 5");
}

#[test]
fn note_from_row_refuses_a_row_of_the_wrong_shape() {
    let row = HashMap::from([
        ("id".to_owned(), DbValue::Int64(7)),
        ("body".to_owned(), DbValue::Text("hi".into())),
    ]);
    assert_eq!(
        note_from_row(&row).unwrap(),
        Note {
            id: 7,
            body: "hi".into()
        }
    );
    let wrong = HashMap::from([("id".to_owned(), DbValue::Text("7".into()))]);
    assert!(note_from_row(&wrong)
        .unwrap_err()
        .message
        .contains("unexpected shape"));
}
