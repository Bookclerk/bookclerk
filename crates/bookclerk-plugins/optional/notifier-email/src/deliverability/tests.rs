//! Unit tests for the deliverability parsers and the merged advice.

// Test code: a failed expectation is the test failing.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

/// Postmark's answer for a clean message from an unsigned domain, as the
/// service sent it on 2026-11-23.
const POSTMARK_CLEAN: &str = r#"{"success":true,"score":"0.8","rules":[{"score":"-0.0","description":"Informational: message has no Received headers"},{"score":"0.8","description":"No valid author signature and domain not in DNS"},{"score":"-0.0","description":"Informational: message was not relayed via SMTP"},{"score":"0.0","description":"BODY: HTML included in message"}],"report":" pts rule                   description\n---- ---------------------- ----------------\n-0.0 NO_RECEIVED            Informational: message has no Received headers\n 0.8 DKIM_ADSP_NXDOMAIN     No valid author signature and domain not in DNS\n-0.0 NO_RELAYS              Informational: message was not relayed via SMTP\n 0.0 HTML_MESSAGE           BODY: HTML included in message\n"}"#;

#[test]
fn postmark_answers_are_read_with_rule_names() {
    let found = parse_spamcheck(POSTMARK_CLEAN).unwrap();
    assert!((found.score - 0.8).abs() < 1e-9);
    assert_eq!(found.rules.len(), 4);
    assert_eq!(found.rules[1].name, "DKIM_ADSP_NXDOMAIN");
    assert!((found.rules[1].points - 0.8).abs() < 1e-9);
    assert_eq!(found.rules[3].name, "HTML_MESSAGE");
    assert!(parse_spamcheck("nope").is_err());
    assert!(parse_spamcheck(r#"{"success":false,"message":"x"}"#).is_err());
    assert!(parse_spamcheck(r#"{"success":true}"#).is_err());
    assert!(parse_spamcheck(r#"{"success":true,"score":"NaN"}"#).is_err());
    // Names follow points and description, not row order; an extra row
    // takes no name away from the rules after it.
    let shuffled = r#"{"success":true,"score":"3.3","rules":[{"score":"2.5","description":"Message is all caps"},{"score":"0.8","description":"Unsigned"}],"report":" 0.0 EXTRA_ROW a rule the JSON omits\n 0.8 DKIM_ADSP_ALL Unsigned\n 2.5 ALL_CAPS Message is all caps\n"}"#;
    let found = parse_spamcheck(shuffled).unwrap();
    assert_eq!(found.rules[0].name, "ALL_CAPS");
    assert_eq!(found.rules[1].name, "DKIM_ADSP_ALL");
    let bare = parse_spamcheck(r#"{"success":true,"score":3}"#).unwrap();
    assert!(bare.rules.is_empty());
}

#[test]
fn postmark_leaves_transport_rules_to_the_other_checks() {
    let (check, advice) = postmark_findings(parse_spamcheck(POSTMARK_CLEAN));
    assert_eq!(check.outcome, NotifyCheckOutcome::Passed);
    assert!(check.summary.contains("0.0"), "{}", check.summary);
    assert_eq!(advice.len(), 1);
    assert_eq!(advice[0].outcome, NotifyCheckOutcome::Passed);

    let spammy = r#"{"success":true,"score":"6.1","rules":[{"score":"2.5","description":"Message is all caps"},{"score":"1.0","description":"Mentions money"},{"score":"0.2","description":"Tiny"},{"score":"2.4","description":"Unsigned"}],"report":" 2.5 ALL_CAPS x\n 1.0 MONEY y\n 0.2 TINY z\n 2.4 DKIM_ADSP_ALL w\n"}"#;
    let (check, advice) = postmark_findings(parse_spamcheck(spammy));
    assert_eq!(check.outcome, NotifyCheckOutcome::Warning);
    let titles: Vec<&str> = advice.iter().map(|a| a.title.as_str()).collect();
    assert_eq!(titles, ["Message is all caps", "Mentions money"]);
    assert_eq!(advice[0].outcome, NotifyCheckOutcome::Failed);
    assert_eq!(advice[1].outcome, NotifyCheckOutcome::Warning);
    assert!(advice[0].detail.contains("rule ALL_CAPS"));

    let (check, advice) =
        postmark_findings(Err("spamcheck.postmarkapp.com could not be reached".into()));
    assert_eq!(check.outcome, NotifyCheckOutcome::Skipped);
    assert!(check.summary.starts_with("Not run: spamcheck"));
    assert!(advice.is_empty());
}

#[test]
fn doh_answers_join_txt_strings() {
    let body = r#"{"Status":0,"Answer":[{"name":"example.com","type":16,"data":"\"v=spf1 include:_spf.example.net \" \"-all\""},{"type":5,"data":"cname.example."},{"type":16,"data":"\"say \\\"hi\\\"\""}]}"#;
    assert_eq!(
        parse_doh_txt(body).unwrap(),
        ["v=spf1 include:_spf.example.net -all", "say \"hi\""]
    );
    assert_eq!(
        parse_doh_txt(r#"{"Status":3}"#).unwrap(),
        Vec::<String>::new()
    );
    assert!(parse_doh_txt(r#"{"Status":2}"#).is_err());
    assert!(parse_doh_txt("<html>").is_err());
    assert_eq!(join_txt_strings("plain text"), "plain text");
}

#[test]
fn spf_records_are_judged() {
    let spf = |records: &[&str]| {
        let owned: Vec<String> = records.iter().map(|r| (*r).to_owned()).collect();
        check_spf("example.com", &owned)
    };
    assert_eq!(spf(&[]).outcome, NotifyCheckOutcome::Failed);
    assert_eq!(
        spf(&["google-site-verification=x"]).outcome,
        NotifyCheckOutcome::Failed
    );
    assert_eq!(
        spf(&["v=spf1 include:x -all"]).outcome,
        NotifyCheckOutcome::Passed
    );
    assert_eq!(spf(&["V=SPF1 mx ~ALL"]).outcome, NotifyCheckOutcome::Passed);
    assert_eq!(spf(&["v=spf1 +all"]).outcome, NotifyCheckOutcome::Failed);
    assert_eq!(
        spf(&["v=spf1 mx ?all"]).outcome,
        NotifyCheckOutcome::Warning
    );
    assert_eq!(spf(&["v=spf1"]).outcome, NotifyCheckOutcome::Warning);
    let two = spf(&["v=spf1 mx -all", "v=spf1 a -all"]);
    assert_eq!(two.outcome, NotifyCheckOutcome::Failed);
    assert!(two.title.starts_with("Merge"));
    assert_eq!(two.sources, [DNS]);
}

#[test]
fn dmarc_records_are_judged() {
    assert_eq!(
        check_dmarc("example.com", &[]).outcome,
        NotifyCheckOutcome::Warning
    );
    let found = check_dmarc(
        "example.com",
        &["v=DMARC1; p=Quarantine; rua=mailto:d@example.com".to_owned()],
    );
    assert_eq!(found.outcome, NotifyCheckOutcome::Passed);
    assert_eq!(found.detail, "Its policy is p=quarantine.");
    assert_eq!(
        check_dmarc("example.com", &["v=DMARC1".to_owned()]).detail,
        "A DMARC record is published."
    );
}

#[test]
fn dns_findings_need_both_lookups() {
    let (check, advice) = dns_findings(
        "example.com",
        Ok(vec!["v=spf1 mx -all".into()]),
        Ok(Vec::new()),
    );
    assert_eq!(check.outcome, NotifyCheckOutcome::Warning);
    assert!(check
        .summary
        .starts_with("SPF: found. DMARC: missing or incomplete."));
    assert_eq!(advice.len(), 2);
    let (check, advice) = dns_findings(
        "example.com",
        Ok(Vec::new()),
        Err("the DNS lookup failed".into()),
    );
    assert_eq!(check.outcome, NotifyCheckOutcome::Skipped);
    assert!(advice.is_empty());
}

#[test]
fn mail_tester_answers_are_read() {
    assert_eq!(
        parse_mail_tester(r#"{"title":"Mail not found. Please wait a few seconds and try again.","mark":0,"status":false,"access":true,"mailboxId":"test-x"}"#).unwrap(),
        MailTesterAnswer::Waiting
    );
    assert!(parse_mail_tester(r#"{"status":false,"access":false}"#)
        .unwrap_err()
        .contains("credits"));
    assert!(parse_mail_tester(r#"{"status":true}"#).is_err());
    assert!(parse_mail_tester("[").is_err());
    let body = r#"{"status":true,"mark":8.5,"title":"Good","signature":{"title":"Authenticated","statusClass":"warning","subtests":{"dkim":{"title":"Your message is not signed with DKIM","statusClass":"failure"},"spf":{"title":"SPF passes","statusClass":"success"}}},"blacklists":{"title":"Not listed","statusClass":"success","list":[{"title":"Listed on Example BL","statusClass":"failure"}]}}"#;
    let MailTesterAnswer::Ready { mark, findings } = parse_mail_tester(body).unwrap() else {
        panic!("not ready");
    };
    assert!((mark - 8.5).abs() < 1e-9);
    for expected in [
        (false, "Authenticated"),
        (true, "Your message is not signed with DKIM"),
        (true, "Listed on Example BL"),
    ] {
        assert!(
            findings.contains(&(expected.0, expected.1.to_owned())),
            "{findings:?}"
        );
    }
    assert_eq!(findings.len(), 3);
    // A zero mark is the worst score.
    let MailTesterAnswer::Ready { mark, .. } =
        parse_mail_tester(r#"{"status":true,"mark":0}"#).unwrap()
    else {
        panic!("not ready");
    };
    assert!(mark.abs() < 1e-9);
    // Points off a ten.
    let MailTesterAnswer::Ready { mark, .. } = parse_mail_tester(r#"{"mark":"-1.5"}"#).unwrap()
    else {
        panic!("not ready");
    };
    assert!((mark - 8.5).abs() < 1e-9);
}

#[test]
fn mail_tester_findings_cover_every_state() {
    let url = "https://www.mail-tester.com/acct-1";
    let (check, _) = mail_tester_findings(Ok(None), url);
    assert_eq!(check.outcome, NotifyCheckOutcome::Skipped);
    assert!(check.report_url.is_none());
    let (check, _) = mail_tester_findings(Ok(Some(MailTesterAnswer::Waiting)), url);
    assert_eq!(check.outcome, NotifyCheckOutcome::Skipped);
    assert_eq!(check.report_url.as_deref(), Some(url));
    let (check, _) = mail_tester_findings(Err("www.mail-tester.com answered HTTP 500".into()), url);
    assert!(check.summary.contains("HTTP 500"));
    for (mark, outcome) in [
        (9.5, NotifyCheckOutcome::Passed),
        (7.5, NotifyCheckOutcome::Warning),
        (4.0, NotifyCheckOutcome::Failed),
    ] {
        let (check, advice) = mail_tester_findings(
            Ok(Some(MailTesterAnswer::Ready {
                mark,
                findings: vec![(true, "Not signed".into())],
            })),
            url,
        );
        assert_eq!(check.outcome, outcome);
        assert_eq!(check.report_url.as_deref(), Some(url));
        assert_eq!(advice[0].sources, [MAIL_TESTER]);
    }
}

#[test]
fn advice_merges_by_title_and_sorts_by_urgency() {
    let item = |outcome, title: &str, source: &str| NotifyDeliverabilityAdvice {
        outcome,
        title: title.into(),
        detail: format!("{source} detail"),
        sources: vec![source.into()],
    };
    let merged = merge_advice(vec![
        item(NotifyCheckOutcome::Passed, "Content", POSTMARK),
        item(NotifyCheckOutcome::Warning, "Sign with DKIM", DNS),
        item(NotifyCheckOutcome::Failed, "sign with dkim", MAIL_TESTER),
        item(NotifyCheckOutcome::Warning, "DMARC", DNS),
    ]);
    let titles: Vec<&str> = merged.iter().map(|a| a.title.as_str()).collect();
    assert_eq!(titles, ["Sign with DKIM", "DMARC", "Content"]);
    assert_eq!(merged[0].outcome, NotifyCheckOutcome::Failed);
    assert_eq!(merged[0].sources, [DNS, MAIL_TESTER]);
    assert_eq!(merged[0].detail, "mail-tester.com detail");
    let many = (0..40)
        .map(|n| item(NotifyCheckOutcome::Warning, &format!("t{n}"), DNS))
        .collect();
    assert_eq!(merge_advice(many).len(), 32);
}
