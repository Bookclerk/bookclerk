//! Shared oracle for the `checker_answers` fuzz target and the
//! `bookclerk-plugin-notifier-email` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The email guest's deliverability check reads answers from three outside
//! services the host does not control. Byte 0 modulo 3 picks the service;
//! the rest is its answer body as text (invalid UTF-8 replaced).
//!
//! - **0: Postmark's spam check.** [`parse_spamcheck`] succeeds exactly when
//!   the body is a JSON object with `"success": true` and a finite `score`
//!   (a number, or a string holding one). It keeps at most 64 rules, each
//!   with finite points; a rule's name is empty or a word of ASCII letters,
//!   digits and `_` that appears in the `report` text. The findings never
//!   carry a `skipped` piece of advice.
//! - **1: a DNS-over-HTTPS TXT answer.** [`parse_doh_txt`] succeeds exactly
//!   when the body is a JSON object whose `Status` is 0 or 3, and gives one
//!   record per `Answer` of type 16 with string `data` among the first 64,
//!   none longer than its data. The SPF and DMARC verdicts on those records
//!   name only the DNS check as their source.
//! - **2: a mail-tester result.** [`parse_mail_tester`] refuses an answer
//!   with `"access": false`, waits on `"status": false`, and otherwise reads
//!   a finite mark into 0 to 10 with at most 24 findings. A result it
//!   could read becomes a check that links the report page.
//!
//! Every list of advice, merged, has at most `MAX_DELIVERABILITY_ADVICE`
//! entries.

use bookclerk_plugin_notifier_email::deliverability::{
    check_dmarc, check_spf, dns_findings, mail_tester_findings, merge_advice, parse_doh_txt,
    parse_mail_tester, parse_spamcheck, postmark_findings, MailTesterAnswer, DNS,
};
use bookclerk_plugin_sdk::{NotifyCheckOutcome, MAX_DELIVERABILITY_ADVICE};
use serde_json::Value;

/// A finite number, or a string holding one, as the services write them.
fn finite(value: &Value) -> bool {
    match value {
        Value::Number(n) => n.as_f64().is_some_and(f64::is_finite),
        Value::String(s) => s.trim().parse::<f64>().is_ok_and(f64::is_finite),
        _ => false,
    }
}

/// Runs the invariants on one input.
///
/// # Panics
///
/// When an invariant breaks.
pub fn check(data: &[u8]) {
    let Some((selector, rest)) = data.split_first() else {
        return;
    };
    let body = String::from_utf8_lossy(rest);
    let json: Option<Value> = serde_json::from_str(&body).ok();
    let advice_cap = usize::try_from(MAX_DELIVERABILITY_ADVICE).unwrap_or(usize::MAX);
    match selector % 3 {
        0 => {
            let parsed = parse_spamcheck(&body);
            let expected = json.as_ref().is_some_and(|j| {
                j.get("success").and_then(Value::as_bool) == Some(true)
                    && j.get("score").is_some_and(finite)
            });
            assert_eq!(parsed.is_ok(), expected, "spamcheck accepted wrongly");
            if let Ok(found) = &parsed {
                let report = json
                    .as_ref()
                    .and_then(|j| j.get("report"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                assert!(found.rules.len() <= 64, "too many rules");
                for rule in &found.rules {
                    assert!(rule.points.is_finite(), "rule points");
                    assert!(
                        rule.name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_'),
                        "rule name {:?}",
                        rule.name
                    );
                    assert!(
                        rule.name.is_empty() || report.contains(rule.name.as_str()),
                        "rule name not from the report"
                    );
                }
            }
            let (_, advice) = postmark_findings(parsed);
            assert!(
                advice
                    .iter()
                    .all(|a| a.outcome != NotifyCheckOutcome::Skipped),
                "skipped advice"
            );
            assert!(merge_advice(advice).len() <= advice_cap, "advice cap");
        }
        1 => {
            let parsed = parse_doh_txt(&body);
            let expected = json.as_ref().is_some_and(|j| {
                j.is_object() && matches!(j.get("Status").and_then(Value::as_u64), Some(0 | 3))
            });
            assert_eq!(parsed.is_ok(), expected, "DoH answer accepted wrongly");
            if let Ok(records) = &parsed {
                let datas: Vec<&str> = json
                    .as_ref()
                    .and_then(|j| j.get("Answer"))
                    .and_then(Value::as_array)
                    .map(|answers| {
                        answers
                            .iter()
                            .take(64)
                            .filter(|a| a.get("type").and_then(Value::as_u64) == Some(16))
                            .filter_map(|a| a.get("data").and_then(Value::as_str))
                            .collect()
                    })
                    .unwrap_or_default();
                assert_eq!(records.len(), datas.len(), "one record per TXT answer");
                for (record, data) in records.iter().zip(&datas) {
                    assert!(record.len() <= data.len(), "a record grew");
                }
                assert_eq!(check_spf("example.com", records).sources, [DNS]);
                assert_eq!(check_dmarc("example.com", records).sources, [DNS]);
            }
            let (_, advice) = dns_findings("example.com", parsed.clone(), parsed);
            assert!(merge_advice(advice).len() <= advice_cap, "advice cap");
        }
        _ => {
            let parsed = parse_mail_tester(&body);
            let access_denied = json
                .as_ref()
                .is_some_and(|j| j.get("access").and_then(Value::as_bool) == Some(false));
            let waiting = json
                .as_ref()
                .is_some_and(|j| j.get("status").and_then(Value::as_bool) == Some(false));
            if json.is_none() || access_denied {
                assert!(parsed.is_err(), "unreadable or refused answer accepted");
            } else if waiting {
                assert_eq!(parsed, Ok(MailTesterAnswer::Waiting), "should wait");
            }
            if let Ok(MailTesterAnswer::Ready { mark, findings }) = &parsed {
                assert!((0.0..=10.0).contains(mark), "mark {mark}");
                assert!(findings.len() <= 24, "too many findings");
            }
            let url = "https://www.mail-tester.com/acct-1";
            let read = parsed.is_ok();
            let (check, advice) = mail_tester_findings(parsed.map(Some), url);
            if read {
                assert_eq!(check.report_url.as_deref(), Some(url), "report link");
            }
            assert!(merge_advice(advice).len() <= advice_cap, "advice cap");
        }
    }
}
