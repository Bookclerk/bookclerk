//! The operator's deliverability check, with no I/O of its own.
//!
//! Three checkers look at the mail from different sides, and this module
//! reads their answers and merges what they found into advice:
//!
//! - **Postmark's spam check** scores the message the plugin composes with
//!   SpamAssassin ([`parse_spamcheck`]). It sees the content only: the mail
//!   provider adds DKIM signatures and `Received` lines later, so rules about
//!   those are left to the other two.
//! - **DNS records** of the From domain, read over DNS-over-HTTPS
//!   ([`parse_doh_txt`]): an SPF record ([`check_spf`]) and a DMARC policy
//!   ([`check_dmarc`]).
//! - **mail-tester.com**, when the operator names their account: the plugin
//!   sends a real message through the server to a mail-tester address and
//!   reads the result ([`parse_mail_tester`]), which covers what only a
//!   delivered message shows (DKIM, the server's reverse DNS, blocklists).
//!
//! Everything a checker returns is untrusted: bodies are capped before they
//! reach these parsers, and every text that reaches the operator is cut by
//! the host again.

use bookclerk_plugin_sdk::{
    NotifyCheckOutcome, NotifyDeliverabilityAdvice, NotifyDeliverabilityCheck,
};
use serde_json::Value;

/// Name of the Postmark check, as checks and advice cite it.
pub const POSTMARK: &str = "Postmark spam check";

/// Name of the DNS check.
pub const DNS: &str = "DNS records";

/// Name of the mail-tester check.
pub const MAIL_TESTER: &str = "mail-tester.com";

/// Postmark's spam check endpoint (free, no account).
pub const POSTMARK_URL: &str = "https://spamcheck.postmarkapp.com/filter";

/// The DNS-over-HTTPS resolver the DNS check asks (JSON API).
pub const DOH_URL: &str = "https://cloudflare-dns.com/dns-query";

/// mail-tester.com's site; results are at `/<address local part>`.
pub const MAIL_TESTER_URL: &str = "https://www.mail-tester.com";

/// The domain mail-tester addresses are at.
pub const MAIL_TESTER_DOMAIN: &str = "srv1.mail-tester.com";

/// Largest checker answer read, in bytes.
pub const MAX_ANSWER_BYTES: usize = 256 * 1024;

/// SpamAssassin's usual spam threshold: at or over it, mail goes to spam.
const SPAM_THRESHOLD: f64 = 5.0;

/// Score under which the content is good, not just passing.
const GOOD_SCORE: f64 = 2.0;

/// Rules worth advice: those adding at least this many points.
const RULE_ADVICE_POINTS: f64 = 0.5;

/// One SpamAssassin rule that fired.
#[derive(Debug, Clone, PartialEq)]
pub struct SpamRule {
    /// Points it added (negative for points taken off).
    pub points: f64,
    /// Rule name (`HTML_IMAGE_ONLY_16`); empty when the answer gave none.
    pub name: String,
    /// What it means.
    pub description: String,
}

/// Postmark's answer: the score and the rules that fired.
#[derive(Debug, Clone, PartialEq)]
pub struct SpamCheck {
    /// Total SpamAssassin score.
    pub score: f64,
    /// Rules that fired, in the answer's order.
    pub rules: Vec<SpamRule>,
}

/// A number from JSON that may be written as a string (`"1.5"`).
///
/// # Arguments
///
/// * `value` - The JSON value.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|n| n.is_finite())
}

/// Reads Postmark's answer: `{"success": true, "score": "1.5", "rules":
/// [{"score": "0.1", "description": "..."}], "report": "..."}`. Rule names
/// come from the text report's table (`pts rule description`) when it lists
/// the rule in the same position.
///
/// # Arguments
///
/// * `body` - The answer's body.
///
/// # Errors
///
/// A short reason when the answer is not a successful check.
pub fn parse_spamcheck(body: &str) -> Result<SpamCheck, String> {
    let json: Value = serde_json::from_str(body).map_err(|_| "the answer is not JSON")?;
    if json.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("the service refused the message".to_owned());
    }
    let score = json
        .get("score")
        .and_then(number)
        .ok_or("the answer has no score")?;
    let names: Vec<String> = json
        .get("report")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let points = words.next()?.parse::<f64>().ok()?;
            let name = words.next()?;
            (points.is_finite() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
                .then(|| name.to_owned())
        })
        .collect();
    let rules = json
        .get("rules")
        .and_then(Value::as_array)
        .map(|rules| {
            rules
                .iter()
                .take(64)
                .enumerate()
                .filter_map(|(at, rule)| {
                    Some(SpamRule {
                        points: rule.get("score").and_then(number)?,
                        name: names.get(at).cloned().unwrap_or_default(),
                        description: rule
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .trim()
                            .to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(SpamCheck { score, rules })
}

/// Whether a rule is about how the mail travels or is signed, which only a
/// delivered message shows: the composed message has no DKIM signature or
/// `Received` lines yet, so Postmark's verdict on them says nothing.
///
/// # Arguments
///
/// * `name` - The rule name.
fn is_transport_rule(name: &str) -> bool {
    [
        "DKIM_",
        "SPF_",
        "DMARC_",
        "RDNS_",
        "NO_RELAYS",
        "NO_RECEIVED",
        "T_SPF",
        "T_DKIM",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

/// A piece of advice.
///
/// # Arguments
///
/// * `outcome` - `failed`, `warning` or `passed`.
/// * `title` - Heading.
/// * `detail` - What to do.
/// * `source` - The check that raised it.
fn advice(
    outcome: NotifyCheckOutcome,
    title: impl Into<String>,
    detail: impl Into<String>,
    source: &str,
) -> NotifyDeliverabilityAdvice {
    NotifyDeliverabilityAdvice {
        outcome,
        title: title.into(),
        detail: detail.into(),
        sources: vec![source.to_owned()],
    }
}

/// Turns Postmark's answer into its check and advice.
///
/// # Arguments
///
/// * `answer` - What [`parse_spamcheck`] read, or why the check did not
///   run.
#[must_use]
pub fn postmark_findings(
    answer: Result<SpamCheck, String>,
) -> (NotifyDeliverabilityCheck, Vec<NotifyDeliverabilityAdvice>) {
    let found = match answer {
        Ok(found) => found,
        Err(why) => {
            return (
                NotifyDeliverabilityCheck {
                    service: POSTMARK.to_owned(),
                    outcome: NotifyCheckOutcome::Skipped,
                    summary: format!("Not run: {why}."),
                    report_url: None,
                },
                Vec::new(),
            );
        }
    };
    let content: Vec<&SpamRule> = found
        .rules
        .iter()
        .filter(|rule| !is_transport_rule(&rule.name))
        .collect();
    let content_score: f64 = content.iter().map(|rule| rule.points).sum();
    let outcome = if content_score >= SPAM_THRESHOLD {
        NotifyCheckOutcome::Failed
    } else if content_score >= GOOD_SCORE {
        NotifyCheckOutcome::Warning
    } else {
        NotifyCheckOutcome::Passed
    };
    let summary = format!(
        "SpamAssassin scores the content {content_score:.1} (under {SPAM_THRESHOLD:.0} passes, under {GOOD_SCORE:.0} is good)."
    );
    let mut found_advice: Vec<NotifyDeliverabilityAdvice> = content
        .iter()
        .filter(|rule| rule.points >= RULE_ADVICE_POINTS)
        .map(|rule| {
            let what = if rule.description.is_empty() {
                rule.name.clone()
            } else {
                rule.description.clone()
            };
            let rule_outcome = if rule.points >= GOOD_SCORE {
                NotifyCheckOutcome::Failed
            } else {
                NotifyCheckOutcome::Warning
            };
            advice(
                rule_outcome,
                what,
                format!(
                    "SpamAssassin adds {:.1} points for this{}. If the message's From name or wording triggers it, change them in the plugin's settings; otherwise report it as a Bookclerk bug.",
                    rule.points,
                    if rule.name.is_empty() {
                        String::new()
                    } else {
                        format!(" (rule {})", rule.name)
                    }
                ),
                POSTMARK,
            )
        })
        .collect();
    if found_advice.is_empty() && outcome == NotifyCheckOutcome::Passed {
        found_advice.push(advice(
            NotifyCheckOutcome::Passed,
            "The message content scores well",
            "SpamAssassin finds nothing in the wording or layout that looks like spam.",
            POSTMARK,
        ));
    }
    (
        NotifyDeliverabilityCheck {
            service: POSTMARK.to_owned(),
            outcome,
            summary,
            report_url: None,
        },
        found_advice,
    )
}

/// Reads a DNS-over-HTTPS JSON answer (`application/dns-json`) for TXT
/// records: each `Answer` of type 16, its quoted strings joined.
///
/// # Arguments
///
/// * `body` - The answer's body.
///
/// # Returns
///
/// The records, or `None` for a missing name (`NXDOMAIN`) or no records.
///
/// # Errors
///
/// A short reason when the answer is unreadable or the lookup failed.
pub fn parse_doh_txt(body: &str) -> Result<Vec<String>, String> {
    let json: Value = serde_json::from_str(body).map_err(|_| "the answer is not JSON")?;
    match json.get("Status").and_then(Value::as_u64) {
        Some(0 | 3) => {}
        _ => return Err("the DNS lookup failed".to_owned()),
    }
    Ok(json
        .get("Answer")
        .and_then(Value::as_array)
        .map(|answers| {
            answers
                .iter()
                .take(64)
                .filter(|answer| answer.get("type").and_then(Value::as_u64) == Some(16))
                .filter_map(|answer| answer.get("data").and_then(Value::as_str))
                .map(join_txt_strings)
                .collect()
        })
        .unwrap_or_default())
}

/// Joins a TXT record's strings as presentation format writes them
/// (`"v=spf1 " "include:x -all"`), dropping the quotes and unescaping `\"`
/// and `\\`; unquoted text is kept as is.
///
/// # Arguments
///
/// * `data` - The record's data.
#[must_use]
pub fn join_txt_strings(data: &str) -> String {
    let trimmed = data.trim();
    if !trimmed.starts_with('"') {
        return trimmed.to_owned();
    }
    let mut out = String::new();
    let mut inside = false;
    let mut chars = trimmed.chars();
    while let Some(c) = chars.next() {
        match (inside, c) {
            (false, '"') | (true, '"') => inside = !inside,
            (true, '\\') => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            (true, other) => out.push(other),
            (false, _) => {}
        }
    }
    out
}

/// Checks the From domain's SPF record (RFC 7208): exactly one TXT record
/// starting `v=spf1`, ending in `-all` or `~all`.
///
/// # Arguments
///
/// * `domain` - The From domain.
/// * `records` - Its TXT records.
#[must_use]
pub fn check_spf(domain: &str, records: &[String]) -> NotifyDeliverabilityAdvice {
    let spf: Vec<&String> = records
        .iter()
        .filter(|record| {
            let lower = record.to_ascii_lowercase();
            lower == "v=spf1" || lower.starts_with("v=spf1 ")
        })
        .collect();
    match spf.as_slice() {
        [] => advice(
            NotifyCheckOutcome::Failed,
            format!("Publish an SPF record for {domain}"),
            format!(
                "Receivers check SPF to see which servers may send for {domain}. Add a TXT record at {domain} from your mail provider's setup guide, such as v=spf1 include:<your provider> -all."
            ),
            DNS,
        ),
        [record] => {
            let all = record
                .split_whitespace()
                .last()
                .unwrap_or_default()
                .to_ascii_lowercase();
            match all.as_str() {
                "-all" | "~all" => advice(
                    NotifyCheckOutcome::Passed,
                    format!("{domain} has an SPF record"),
                    format!("Its policy ends in {all}, so other servers can't pass as {domain}."),
                    DNS,
                ),
                "+all" | "all" => advice(
                    NotifyCheckOutcome::Failed,
                    format!("Tighten the SPF record for {domain}"),
                    "It ends in +all, which lets any server send as this domain. End it in -all or ~all.",
                    DNS,
                ),
                _ => advice(
                    NotifyCheckOutcome::Warning,
                    format!("Finish the SPF record for {domain}"),
                    "It doesn't end in -all or ~all, so receivers can't tell what to do with mail from other servers. End it in ~all (or -all once you're sure it lists every sender).",
                    DNS,
                ),
            }
        }
        _ => advice(
            NotifyCheckOutcome::Failed,
            format!("Merge the SPF records for {domain}"),
            "It has more than one record starting v=spf1, so receivers treat SPF as broken. Combine them into one.",
            DNS,
        ),
    }
}

/// Checks the From domain's DMARC policy (RFC 7489): a TXT record at
/// `_dmarc.<domain>` starting `v=DMARC1`.
///
/// # Arguments
///
/// * `domain` - The From domain.
/// * `records` - The TXT records at `_dmarc.<domain>`.
#[must_use]
pub fn check_dmarc(domain: &str, records: &[String]) -> NotifyDeliverabilityAdvice {
    let dmarc = records.iter().find(|record| {
        record
            .get(..8)
            .is_some_and(|start| start.eq_ignore_ascii_case("v=DMARC1"))
    });
    match dmarc {
        None => advice(
            NotifyCheckOutcome::Warning,
            format!("Publish a DMARC policy for {domain}"),
            format!(
                "Gmail, Yahoo and Outlook trust mail more from domains with DMARC. Add a TXT record at _dmarc.{domain}, starting with v=DMARC1; p=none; and tighten p= once reports look clean."
            ),
            DNS,
        ),
        Some(record) => {
            let policy = record
                .split(';')
                .filter_map(|tag| tag.trim().split_once('='))
                .find(|(key, _)| key.trim().eq_ignore_ascii_case("p"))
                .map(|(_, value)| value.trim().to_ascii_lowercase())
                .unwrap_or_default();
            advice(
                NotifyCheckOutcome::Passed,
                format!("{domain} has a DMARC policy"),
                if policy.is_empty() {
                    "A DMARC record is published.".to_owned()
                } else {
                    format!("Its policy is p={policy}.")
                },
                DNS,
            )
        }
    }
}

/// Turns the DNS lookups into the DNS check and its advice.
///
/// # Arguments
///
/// * `domain` - The From domain.
/// * `spf` - The domain's TXT records, or why the lookup failed.
/// * `dmarc` - The TXT records at `_dmarc.<domain>`, or why it failed.
#[must_use]
pub fn dns_findings(
    domain: &str,
    spf: Result<Vec<String>, String>,
    dmarc: Result<Vec<String>, String>,
) -> (NotifyDeliverabilityCheck, Vec<NotifyDeliverabilityAdvice>) {
    let (spf, dmarc) = match (spf, dmarc) {
        (Ok(spf), Ok(dmarc)) => (spf, dmarc),
        (Err(why), _) | (_, Err(why)) => {
            return (
                NotifyDeliverabilityCheck {
                    service: DNS.to_owned(),
                    outcome: NotifyCheckOutcome::Skipped,
                    summary: format!("Not run: {why}."),
                    report_url: None,
                },
                Vec::new(),
            );
        }
    };
    let found = vec![check_spf(domain, &spf), check_dmarc(domain, &dmarc)];
    let outcome = worst(found.iter().map(|a| a.outcome));
    let summary = format!(
        "SPF: {}. DMARC: {}. DKIM is checked on a delivered message (mail-tester).",
        label(found[0].outcome),
        label(found[1].outcome)
    );
    (
        NotifyDeliverabilityCheck {
            service: DNS.to_owned(),
            outcome,
            summary,
            report_url: None,
        },
        found,
    )
}

/// A word for an outcome in a summary.
///
/// # Arguments
///
/// * `outcome` - The outcome.
fn label(outcome: NotifyCheckOutcome) -> &'static str {
    match outcome {
        NotifyCheckOutcome::Passed => "found",
        NotifyCheckOutcome::Warning => "missing or incomplete",
        NotifyCheckOutcome::Failed => "missing or broken",
        NotifyCheckOutcome::Skipped => "not checked",
    }
}

/// Rank of an outcome, most urgent highest.
///
/// # Arguments
///
/// * `outcome` - The outcome.
fn rank(outcome: NotifyCheckOutcome) -> u8 {
    match outcome {
        NotifyCheckOutcome::Failed => 3,
        NotifyCheckOutcome::Warning => 2,
        NotifyCheckOutcome::Skipped => 1,
        NotifyCheckOutcome::Passed => 0,
    }
}

/// The most urgent of some outcomes; `passed` when there are none.
///
/// # Arguments
///
/// * `outcomes` - The outcomes.
fn worst(outcomes: impl Iterator<Item = NotifyCheckOutcome>) -> NotifyCheckOutcome {
    outcomes
        .max_by_key(|outcome| rank(*outcome))
        .unwrap_or(NotifyCheckOutcome::Passed)
}

/// What one mail-tester poll found.
#[derive(Debug, Clone, PartialEq)]
pub enum MailTesterAnswer {
    /// The message has not arrived yet; ask again.
    Waiting,
    /// The result: the mark out of 10 and the tests that did not pass.
    Ready {
        /// Mark out of 10.
        mark: f64,
        /// `(failed, title)` of each test marked `failure` (true) or
        /// `warning` (false), in the answer's order.
        findings: Vec<(bool, String)>,
    },
}

/// Reads a mail-tester JSON answer (`?format=json`). `status: false` means
/// the message has not arrived (`"Mail not found"`), `access: false` that
/// the account may not run tests; otherwise the top-level `mark`
/// (out of 10) and every test object, up to three levels down, whose
/// `statusClass` is `failure` or `warning`, by its `title`.
///
/// # Arguments
///
/// * `body` - The answer's body.
///
/// # Errors
///
/// A short reason when the answer is unreadable.
pub fn parse_mail_tester(body: &str) -> Result<MailTesterAnswer, String> {
    let json: Value = serde_json::from_str(body).map_err(|_| "the answer is not JSON")?;
    if json.get("access").and_then(Value::as_bool) == Some(false) {
        return Err(
            "mail-tester.com refused the test; check the account name and its credits".to_owned(),
        );
    }
    if json.get("status").and_then(Value::as_bool) == Some(false) {
        return Ok(MailTesterAnswer::Waiting);
    }
    let Some(mark) = json.get("mark").and_then(number) else {
        return Err("the answer has no mark".to_owned());
    };
    // mail-tester writes marks as points off a 10 (`-1.5`) in some answers
    // and as the score in others.
    let mark = if mark <= 0.0 { 10.0 + mark } else { mark }.clamp(0.0, 10.0);
    let mut findings = Vec::new();
    collect_findings(&json, 0, &mut findings);
    Ok(MailTesterAnswer::Ready { mark, findings })
}

/// Collects failing and warning tests from a mail-tester answer.
///
/// # Arguments
///
/// * `value` - The object to look in.
/// * `depth` - How far down it is; the walk stops below 3.
/// * `out` - Where findings go; at most 24.
fn collect_findings(value: &Value, depth: usize, out: &mut Vec<(bool, String)>) {
    let Some(object) = value.as_object() else {
        return;
    };
    if depth > 0 {
        let class = object.get("statusClass").and_then(Value::as_str);
        let title = object
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty());
        if let (Some(class @ ("failure" | "warning")), Some(title)) = (class, title) {
            if out.len() < 24 && !out.iter().any(|(_, t)| t == title) {
                out.push((class == "failure", title.to_owned()));
            }
        }
    }
    if depth < 3 {
        for child in object.values() {
            match child {
                Value::Object(_) => collect_findings(child, depth + 1, out),
                Value::Array(items) => {
                    for item in items.iter().take(64) {
                        collect_findings(item, depth + 1, out);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Turns the mail-tester result into its check and advice.
///
/// # Arguments
///
/// * `answer` - The final answer, or why the check did not run (`Ok(None)`
///   when no account is set).
/// * `report_url` - The result page.
#[must_use]
pub fn mail_tester_findings(
    answer: Result<Option<MailTesterAnswer>, String>,
    report_url: &str,
) -> (NotifyDeliverabilityCheck, Vec<NotifyDeliverabilityAdvice>) {
    let skipped = |summary: String| {
        (
            NotifyDeliverabilityCheck {
                service: MAIL_TESTER.to_owned(),
                outcome: NotifyCheckOutcome::Skipped,
                summary,
                report_url: None,
            },
            Vec::new(),
        )
    };
    match answer {
        Ok(None) => skipped(
            "Not run: set your mail-tester.com account name in the plugin's settings to also test a delivered message (DKIM, reverse DNS, blocklists).".to_owned(),
        ),
        Err(why) => skipped(format!("Not run: {why}.")),
        Ok(Some(MailTesterAnswer::Waiting)) => skipped(
            "The test message had not reached mail-tester.com in time. Open the report later, or check that the server delivers to outside addresses.".to_owned(),
        )
        .map_report(report_url),
        Ok(Some(MailTesterAnswer::Ready { mark, findings })) => {
            let outcome = if mark >= 9.0 {
                NotifyCheckOutcome::Passed
            } else if mark >= 7.0 {
                NotifyCheckOutcome::Warning
            } else {
                NotifyCheckOutcome::Failed
            };
            let found = findings
                .into_iter()
                .map(|(failed, title)| {
                    advice(
                        if failed {
                            NotifyCheckOutcome::Failed
                        } else {
                            NotifyCheckOutcome::Warning
                        },
                        title,
                        "mail-tester.com flagged this on the delivered message; its report explains the fix.",
                        MAIL_TESTER,
                    )
                })
                .collect();
            (
                NotifyDeliverabilityCheck {
                    service: MAIL_TESTER.to_owned(),
                    outcome,
                    summary: format!("The delivered message scores {mark:.1} out of 10."),
                    report_url: Some(report_url.to_owned()),
                },
                found,
            )
        }
    }
}

/// Adds a report link to a skipped check.
trait MapReport {
    /// The check with `url` as its report link.
    fn map_report(self, url: &str) -> Self;
}

impl MapReport for (NotifyDeliverabilityCheck, Vec<NotifyDeliverabilityAdvice>) {
    fn map_report(mut self, url: &str) -> Self {
        self.0.report_url = Some(url.to_owned());
        self
    }
}

/// Merges every check's advice: advice with the same title is one entry
/// citing every source and keeping the most urgent outcome; the list is
/// ordered failed, warning, passed (each in the order raised) and cut to
/// the ABI's cap.
///
/// # Arguments
///
/// * `found` - Advice from every check, in check order.
#[must_use]
pub fn merge_advice(found: Vec<NotifyDeliverabilityAdvice>) -> Vec<NotifyDeliverabilityAdvice> {
    let mut merged: Vec<NotifyDeliverabilityAdvice> = Vec::new();
    for item in found {
        if let Some(existing) = merged
            .iter_mut()
            .find(|m| m.title.eq_ignore_ascii_case(&item.title))
        {
            if rank(item.outcome) > rank(existing.outcome) {
                existing.outcome = item.outcome;
                existing.detail = item.detail;
            }
            for source in item.sources {
                if !existing.sources.contains(&source) {
                    existing.sources.push(source);
                }
            }
        } else {
            merged.push(item);
        }
    }
    merged.sort_by_key(|item| std::cmp::Reverse(rank(item.outcome)));
    let cap = usize::try_from(bookclerk_plugin_sdk::MAX_DELIVERABILITY_ADVICE).unwrap_or(32);
    merged.truncate(cap);
    merged
}

#[cfg(test)]
mod tests;
