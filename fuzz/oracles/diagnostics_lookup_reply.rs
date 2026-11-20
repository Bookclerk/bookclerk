//! Shared oracle for the `diagnostics_lookup_reply` fuzz target and the
//! `bookclerk-config` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The diagnostics collector answers a report lookup (`POST /lookup`) with
//! the stored report, the issues tracking it and its fix, and a deletion
//! (`POST /lookup/delete`) with when the stored copy went; the daemon shows
//! both to the operator, issue links included. Each input goes through both
//! [`parse_lookup_reply`] and [`parse_delete_reply`] for one fixed report
//! ID. The invariant, checked against an independent rebuild of the
//! expected answer from a `serde_json::Value`: an answer of at most
//! [`MAX_LOOKUP_REPLY_BYTES`] bytes is accepted exactly when it is an object
//! for the asked report with every field the collector sends of its type
//! (only `deleted_at_unix_ms`, `report`, `fix` and the fix's fields may be
//! absent or null), every issue and pull request link starts `https://`,
//! levels are the five upper-case names and states `open` or `closed`; the
//! accepted answer carries exactly those values.

use bookclerk_config::{
    parse_delete_reply, parse_lookup_reply, LookupReplyError, ReportId, MAX_LOOKUP_REPLY_BYTES,
};
use serde_json::{json, Value};

/// The report every input is read for.
const ID: &str = "3f2a9c4e-7b1d-4e8a-9c0f-1a2b3c4d5e6f";

/// A value that must be a string, copied.
fn s(v: Option<&Value>) -> Option<Value> {
    matches!(v, Some(Value::String(_)))
        .then(|| v.cloned())
        .flatten()
}

/// A value that must be a non-negative integer, copied.
fn n(v: Option<&Value>) -> Option<Value> {
    v.filter(|v| v.is_u64()).cloned()
}

/// A value that may be absent or null, else must pass `f`.
fn opt(v: Option<&Value>, f: impl Fn(Option<&Value>) -> Option<Value>) -> Option<Value> {
    match v {
        None | Some(Value::Null) => Some(Value::Null),
        some => f(some),
    }
}

/// A string link starting `https://`, copied.
fn link(v: Option<&Value>) -> Option<Value> {
    s(v).filter(|l| l.as_str().is_some_and(|l| l.starts_with("https://")))
}

/// An array, each element rebuilt by `f`.
fn each(v: Option<&Value>, f: impl Fn(&Value) -> Option<Value>) -> Option<Value> {
    let mut out = Vec::new();
    for item in v?.as_array()? {
        out.push(f(item)?);
    }
    Some(Value::Array(out))
}

/// The expected lookup answer, normalized the way the parsed answer
/// serializes, or `None` when it must be refused as malformed.
fn expected(o: &serde_json::Map<String, Value>) -> Option<Value> {
    let report = opt(o.get("report"), |r| {
        let r = r?.as_object()?;
        let events = each(r.get("events"), |e| {
            let e = e.as_object()?;
            let level = s(e.get("level")).filter(|l| {
                ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"].contains(&l.as_str().unwrap_or(""))
            })?;
            let fields = each(e.get("fields"), |f| {
                let pair = f.as_array()?;
                (pair.len() == 2 && pair.iter().all(Value::is_string)).then(|| f.clone())
            })?;
            Some(json!({
                "ts_unix_ms": n(e.get("ts_unix_ms"))?,
                "level": level,
                "target": s(e.get("target"))?,
                "message": s(e.get("message"))?,
                "fields": fields,
            }))
        })?;
        Some(json!({
            "version": s(r.get("version"))?,
            "trigger": s(r.get("trigger"))?,
            "os": s(r.get("os"))?,
            "arch": s(r.get("arch"))?,
            "distro": s(r.get("distro"))?,
            "events": events,
        }))
    })?;
    let issues = each(o.get("issues"), |i| {
        let i = i.as_object()?;
        let state = s(i.get("state"))
            .filter(|st| st.as_str() == Some("open") || st.as_str() == Some("closed"))?;
        Some(json!({
            "number": n(i.get("number"))?,
            "title": s(i.get("title"))?,
            "state": state,
            "url": link(i.get("url"))?,
        }))
    })?;
    let fix = opt(o.get("fix"), |f| {
        let f = f?.as_object()?;
        Some(json!({
            "pr_number": opt(f.get("pr_number"), n)?,
            "pr_url": opt(f.get("pr_url"), link)?,
            "fixed_in": opt(f.get("fixed_in"), s)?,
        }))
    })?;
    Some(json!({
        "report_id": ID,
        "received_at_unix_ms": n(o.get("received_at_unix_ms"))?,
        "deleted_at_unix_ms": opt(o.get("deleted_at_unix_ms"), n)?,
        "report": report,
        "issues": issues,
        "fix": fix,
    }))
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - A collector answer body.
///
/// # Panics
///
/// When a parser disagrees with the oracle.
pub fn check(data: &[u8]) {
    let id = ReportId::parse(ID).unwrap_or_else(|| unreachable!("valid id"));
    let lookup = parse_lookup_reply(data, &id)
        .map(|a| serde_json::to_value(a).unwrap_or_else(|_| unreachable!("serializes")));
    let delete = parse_delete_reply(data, &id);
    if data.len() > MAX_LOOKUP_REPLY_BYTES {
        assert_eq!(lookup, Err(LookupReplyError::TooLong));
        assert_eq!(delete, Err(LookupReplyError::TooLong));
        return;
    }
    let Some(Value::Object(o)) = serde_json::from_slice::<Value>(data).ok() else {
        assert_eq!(lookup, Err(LookupReplyError::NotJson));
        assert_eq!(delete, Err(LookupReplyError::NotJson));
        return;
    };
    let named = match o.get("report_id") {
        Some(Value::String(named)) if named == ID => None,
        Some(Value::String(_)) => Some(LookupReplyError::OtherReport),
        _ => Some(LookupReplyError::BadShape),
    };
    if let Some(err) = named {
        assert_eq!(lookup, Err(err));
        assert_eq!(delete, Err(err));
        return;
    }
    assert_eq!(
        delete,
        o.get("deleted_at_unix_ms")
            .and_then(Value::as_u64)
            .ok_or(LookupReplyError::BadShape)
    );
    assert_eq!(lookup, expected(&o).ok_or(LookupReplyError::BadShape));
}
