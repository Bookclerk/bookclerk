//! Shared oracle for the `node_frame` fuzz target and the
//! `bookclerk-node-proto` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is the text of one node-to-node frame, as a peer node sends it
//! over the library socket. The invariants:
//!
//! - parsing never panics, and refuses text over the frame size limit or of
//!   another frame version;
//! - an accepted frame whose serialized text stays within the limit
//!   parses back to an equal frame (a float inside an opaque JSON field may
//!   move by a rounding step, as `serde_json` parses floats without exact
//!   round trips);
//! - every call number an `acquire.targets` frame carries is 1 to 16 ASCII
//!   letters and digits, checked byte by byte here rather than through the
//!   parser's own rule, since a host names storage keys with it;
//! - every target of an `acquire.targets` frame the host would accept
//!   (`target_destinations_problem` finds nothing) names only destinations
//!   its plan lists, each once and in the plan's order, checked here
//!   against the plan directly, since the host writes the title to exactly
//!   those destinations and the owner refuses an upload to any other;
//! - every probe on a file an `upload.grant` or `catalog.upload` frame the
//!   library would accept (`UploadFile::is_well_formed`) states a bitrate
//!   of 1 to 20000 kbit/s when it states one, and names a storefront of 1
//!   to 64 lowercase letters, digits, `_`, `-` or `.` when it names one,
//!   checked byte by byte here, since the library stores them on the
//!   file's row and compares two copies of an edition by them;
//! - every marketplace a `catalog.sync` title the library would accept
//!   carries is at most 16 lowercase ASCII letters and digits, checked byte
//!   by byte here, since the library scopes ASIN identifiers to it; each
//!   of its files' content versions is at most 128 bytes with no control
//!   byte, since the library stores it in the title's manifest.
//! - every catalog reference a `catalog.upload`, `catalog.session` or
//!   `catalog.accepted` frame carries that its receiver would act on
//!   (`CatalogRef::is_well_formed`) names a store and a marketplace without
//!   `:` or `@`, checked byte by byte here, since the library stores the
//!   three parts as one `store@marketplace:product_id` text and reads them
//!   back by those separators.
//! - a `catalog.ask` or `catalog.found` frame the library or branch would
//!   accept (`questions_problem` / `found_problem` find nothing) names at
//!   most 100 works, each once, with plain correlation keys; every
//!   identifier is plain ASCII with a marketplace exactly when it is an
//!   ASIN; and every answer lists 1 to 8 distinct editions with a
//!   non-blank title. Each is checked byte by byte here, not through the
//!   validators, since the library writes them into its listing pool;
//! - a `requests.claimed` frame the branch would accept (`claimed_problem`
//!   finds nothing) carries at most 16 requests, each for a different job
//!   with a plain job id and a positive lease generation, a question that
//!   meets the same identifier rules, a plain store id, a lowercase
//!   marketplace and a product id with no control characters or outer
//!   whitespace. Checked byte by byte here too, since the branch looks the
//!   request up in its own library and records it;
//! - a `requests.finish` the library would accept (`finish_problem` finds
//!   nothing) names a plain job id and a positive lease generation, since
//!   the library looks the job up and may echo the id back.
//! - a `window.page` frame a window would accept (`window_page_problem`
//!   finds nothing for the cursor it asked with) carries at most 100 works,
//!   each once, with plain grouping keys, plain identifiers (a marketplace
//!   exactly on an ASIN), 1 to 8 distinct editions with a non-blank title,
//!   each owned, or reachable (only a branch of the library lists it) and
//!   then neither the person's nor with a copy, and, where the library
//!   states the copy it holds, a copy with a
//!   lowercase `type/subtype` content type of at most 64 bytes, a bitrate
//!   of 1 to 20000 kbit/s when stated, a short lowercase source store when
//!   stated and a plain version of at most 64 bytes, and a plain next cursor (1 to 64 letters, digits or `-`)
//!   sorting byte-wise past the one asked with, so a window's paging
//!   always moves forward. Checked byte by byte here, since the window
//!   stores them.
//! - a `deployments` page a branch would accept (`deployments_page_problem`
//!   finds nothing for the cursor it asked with) lists deployment ids in
//!   strictly rising byte order, each after that cursor, and a next cursor
//!   that sorts after it and not before the last entry, so a branch's
//!   paging always moves forward and never lists a deployment twice.
//!   Checked byte by byte here.
//! - a `scan.removed` or `scan.pass_end` frame the owner would accept
//!   (`scan_removed_problem` / `scan_pass_end_problem` find nothing) names
//!   a non-blank account and carries at most 500 product ids, each
//!   non-blank, at most 512 bytes and free of control characters, or a
//!   sync token of at most 4096 bytes with no control characters. Checked
//!   character by character here, since the owner removes holdings by
//!   those ids and stores the token for the account's next pass.
//! - a `copy.grant` the library would accept (`copy_grant_problem` finds
//!   nothing) names a plain store id, a lowercase marketplace and a
//!   product id with no control characters or outer whitespace, since the
//!   library looks the listing up by them. A `copy.granted` a branch would
//!   accept (`copy_granted_problem` finds nothing) carries a lowercase
//!   hyphenated UUID as its copy id (the branch puts it in a URL path), a
//!   token of 1 to 1024 base64url characters, a life of one second to a
//!   day, and 1 to 1000 files with distinct names, exactly one of them
//!   the primary audio, each at most 1 TiB with a name of at most 512
//!   bytes that is a relative path (no empty, `.` or `..` segment, no
//!   leading `/`, no backslash, no control character, no drive prefix).
//!   Checked character by character here, since the branch asks for byte
//!   ranges by those sizes and reads the names when it places the files.
//! - a `heartbeat`'s public origin that the library would keep
//!   (`BranchOrigin::parse` accepts it) is `http://` or `https://` and a
//!   non-empty authority of printable ASCII with no `/`, `?`, `#` or `@`,
//!   at most 255 bytes in all. Checked byte by byte here, since the library
//!   stores it and shows it as the branch's address.

use std::collections::BTreeSet;

use bookclerk_node_proto::{
    claimed_problem, copy_grant_problem, copy_granted_problem, deployments_page_problem,
    finish_problem, found_problem, questions_problem, scan_pass_end_problem, scan_removed_problem,
    target_destinations_problem, window_page_problem, Availability, Body, BranchOrigin,
    CopyGrantedFrame, CopyRole, Frame, LookupScheme, LookupWork, WindowCursor, FRAME_VERSION,
    MAX_BRANCH_ORIGIN_BYTES, MAX_CALL_NUMBER_BYTES, MAX_FRAME_BYTES,
};
use serde_json::Value;

/// Runs every invariant on one input; panics on a violation.
pub fn check(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(frame) = Frame::parse(text) else {
        return;
    };
    assert!(text.len() <= MAX_FRAME_BYTES, "an oversized frame parsed");
    assert_eq!(frame.v, FRAME_VERSION, "another frame version parsed");
    if let Body::AcquireTargets {
        targets,
        destinations,
        ..
    } = &frame.body
    {
        for target in targets {
            if target_destinations_problem(target, destinations).is_none() {
                if let Some(chosen) = &target.destinations {
                    let mut at = 0usize;
                    for id in chosen {
                        let found = destinations
                            .iter()
                            .skip(at)
                            .position(|planned| planned.id == *id);
                        let Some(found) = found else {
                            panic!("target destination {id:?} outside its plan accepted");
                        };
                        at += found + 1;
                    }
                    assert!(chosen.len() <= destinations.len(), "a longer list accepted");
                }
            }
            for id in [&target.call_number, &target.parent_call_number]
                .into_iter()
                .flatten()
            {
                let id = id.as_str();
                assert!(
                    !id.is_empty()
                        && id.len() <= MAX_CALL_NUMBER_BYTES
                        && id.bytes().all(|b| b.is_ascii_alphanumeric()),
                    "call number {id:?} accepted"
                );
            }
        }
    }
    if let Body::UploadGrant { files, .. } | Body::CatalogUpload { files, .. } = &frame.body {
        for probe in files
            .iter()
            .filter(|file| file.is_well_formed())
            .filter_map(|file| file.probe.as_ref())
        {
            assert!(
                probe
                    .bitrate_kbps
                    .is_none_or(|kbps| (1..=20_000).contains(&kbps)),
                "probe bitrate {:?} accepted",
                probe.bitrate_kbps
            );
            if let Some(store) = &probe.source_store {
                assert!(
                    !store.is_empty()
                        && store.len() <= 64
                        && store.bytes().all(|b| b.is_ascii_lowercase()
                            || b.is_ascii_digit()
                            || matches!(b, b'_' | b'-' | b'.')),
                    "probe store {store:?} accepted"
                );
            }
        }
    }
    if let Body::CatalogSync { titles, .. } = &frame.body {
        for title in titles.iter().filter(|title| title.problem().is_none()) {
            let market = title.marketplace.as_bytes();
            assert!(
                market.len() <= 16
                    && market
                        .iter()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "marketplace {:?} accepted",
                title.marketplace
            );
            for file in &title.files {
                if let Some(version) = file.version.as_deref() {
                    assert!(
                        version.len() <= 128 && !version.bytes().any(|b| b < 0x20 || b == 0x7f),
                        "file version {version:?} accepted"
                    );
                }
            }
        }
    }
    check_catalog_refs(&frame.body);
    check_lookup(&frame.body);
    check_requests(&frame.body);
    check_window(&frame.body);
    check_deployments(&frame.body);
    check_scan_reports(&frame.body);
    check_copies(&frame.body);
    check_heartbeat(&frame.body);
    let text_again = frame.to_text();
    // Serializing can lengthen an opaque JSON field (`1e9` is written out
    // in full), past the limit the parser enforces: such a frame is never
    // sent again, so there is no round trip to check.
    if text_again.len() > MAX_FRAME_BYTES {
        return;
    }
    let again = Frame::parse(&text_again);
    let Ok(again) = again else {
        panic!("a parsed frame did not parse again: {again:?}");
    };
    assert!(
        same(&json(&again), &json(&frame)),
        "a frame changed across a round trip: {frame:?} became {again:?}"
    );
}

/// The rules of a `deployments` page a branch would accept, for the first
/// page's cursor and for one that sorts just before the page's first entry.
fn check_deployments(body: &Body) {
    let Body::Deployments { items, next } = body else {
        return;
    };
    let before_first = items
        .first()
        .map(|item| item.deployment_id.as_str())
        .filter(|id| !id.is_empty())
        .map(|id| &id[..id.len() - id.chars().last().map_or(0, char::len_utf8)]);
    for after in [None, before_first] {
        if deployments_page_problem(after, items, next.as_deref()).is_some() {
            continue;
        }
        let mut previous = after.map(str::as_bytes);
        for item in items {
            let id = item.deployment_id.as_bytes();
            assert!(
                previous.is_none_or(|previous| id > previous),
                "deployment {:?} out of order accepted",
                item.deployment_id
            );
            previous = Some(id);
        }
        if let Some(next) = next {
            assert!(
                after.is_none_or(|after| next.as_bytes() > after.as_bytes()),
                "a cursor that does not move forward accepted"
            );
            assert!(
                previous.is_none_or(|last| next.as_bytes() >= last),
                "a cursor before the page's last entry accepted"
            );
        }
    }
}

/// The rules of a `copy.grant` the library would accept and of a
/// `copy.granted` a branch would accept.
fn check_copies(body: &Body) {
    match body {
        Body::CopyGrant {
            store,
            marketplace,
            product_id,
        } if copy_grant_problem(store, marketplace, product_id).is_none() => {
            assert!(
                !store.is_empty()
                    && store.len() <= 64
                    && store
                        .bytes()
                        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.')),
                "copy store {store:?} accepted"
            );
            assert!(
                marketplace.len() <= 16
                    && marketplace
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "copy marketplace {marketplace:?} accepted"
            );
            assert!(
                !product_id.is_empty()
                    && product_id.len() <= 256
                    && product_id.trim() == product_id
                    && !product_id.chars().any(char::is_control),
                "copy product id {product_id:?} accepted"
            );
        }
        Body::CopyGranted {
            copy_id,
            sealed_token,
            expires_in_secs,
            copy,
            files,
        } if copy_granted_problem(&CopyGrantedFrame {
            copy_id,
            sealed_token,
            expires_in_secs: *expires_in_secs,
            copy,
            files,
        })
        .is_none() =>
        {
            let groups: Vec<&str> = copy_id.split('-').collect();
            assert!(
                groups.iter().map(|group| group.len()).eq([8, 4, 4, 4, 12])
                    && groups.iter().all(|group| group
                        .chars()
                        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))),
                "copy id {copy_id:?} accepted"
            );
            assert!(
                (1..=1024).contains(&sealed_token.len())
                    && sealed_token
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "a sealed token that is not base64url text accepted"
            );
            assert!(
                (1..=86_400).contains(expires_in_secs),
                "a token life of {expires_in_secs} s accepted"
            );
            assert!(
                (1..=1000).contains(&files.len()),
                "a copy of {} files accepted",
                files.len()
            );
            let mut names = BTreeSet::new();
            for file in files {
                let name = file.name.as_str();
                let drive = name.chars().nth(1) == Some(':')
                    && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
                assert!(
                    name.len() <= 512
                        && !drive
                        && !name.chars().any(|c| c == '\\' || c.is_control())
                        && name.split('/').all(|part| !matches!(part, "" | "." | "..")),
                    "copy file name {name:?} accepted"
                );
                assert!(names.insert(name), "a copy file named twice was accepted");
                assert!(
                    file.size <= 1 << 40,
                    "a copy file of {} bytes accepted",
                    file.size
                );
            }
            assert_eq!(
                files
                    .iter()
                    .filter(|file| file.role == CopyRole::Audio)
                    .count(),
                1,
                "a copy without exactly one primary audio file accepted"
            );
        }
        Body::CopyGrant { .. } | Body::CopyGranted { .. } => {}
        _ => {}
    }
}

/// The rules of a `scan.removed` or `scan.pass_end` the owner would accept.
fn check_scan_reports(body: &Body) {
    let named = |account: &str| account.chars().any(|c| !c.is_whitespace());
    let clean = |text: &str| text.chars().all(|c| !c.is_control());
    match body {
        Body::ScanRemoved {
            account_id,
            product_ids,
            ..
        } if scan_removed_problem(account_id, product_ids).is_none() => {
            assert!(
                named(account_id),
                "a blank account of {} bytes was accepted",
                account_id.len()
            );
            assert!(
                product_ids.len() <= 500,
                "{} removed ids accepted",
                product_ids.len()
            );
            for id in product_ids {
                assert!(
                    named(id) && id.len() <= 512 && clean(id),
                    "removed id {id:?} accepted"
                );
            }
        }
        Body::ScanPassEnd {
            account_id,
            sync_token,
            ..
        } if scan_pass_end_problem(account_id, sync_token).is_none() => {
            assert!(
                named(account_id),
                "a blank account of {} bytes was accepted",
                account_id.len()
            );
            assert!(
                sync_token.len() <= 4096 && clean(sync_token),
                "sync token of {} bytes accepted",
                sync_token.len()
            );
        }
        _ => {}
    }
}

/// True for a plain token: 1 to `max` ASCII letters, digits and `-`.
fn plain(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The identifier rules of one accepted question.
fn check_question(work: &LookupWork) {
    assert!(plain(&work.work, 64), "work ref {:?} accepted", work.work);
    assert!(
        (1..=16).contains(&work.identifiers.len()),
        "a question with {} identifiers accepted",
        work.identifiers.len()
    );
    for id in &work.identifiers {
        assert!(plain(&id.value, 64), "identifier {:?} accepted", id.value);
        let asin = id.scheme == LookupScheme::Asin;
        assert_eq!(
            asin,
            !id.marketplace.is_empty(),
            "marketplace {:?} accepted on {:?}",
            id.marketplace,
            id.scheme
        );
        assert!(
            id.marketplace.len() <= 16
                && id
                    .marketplace
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
            "marketplace {:?} accepted",
            id.marketplace
        );
    }
}

/// The job id and lease generation invariants of a request.
fn check_request_job(job: &str, generation: i64) {
    assert!(
        !job.is_empty()
            && job.len() <= 96
            && job
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "job id {job:?} accepted"
    );
    assert!(generation >= 1, "generation {generation} accepted");
}

/// The interlibrary request invariants on an accepted claimed list or
/// finish.
fn check_requests(body: &Body) {
    if let Body::RequestFinish {
        job, generation, ..
    } = body
    {
        if finish_problem(job, *generation).is_none() {
            check_request_job(job, *generation);
        }
        return;
    }
    let Body::RequestsClaimed { requests } = body else {
        return;
    };
    if claimed_problem(requests).is_some() {
        return;
    }
    assert!(requests.len() <= 16, "too many requests accepted");
    let mut jobs = BTreeSet::new();
    for request in requests {
        check_request_job(&request.job, request.generation);
        assert!(
            jobs.insert(&request.job),
            "a job claimed twice was accepted"
        );
        check_question(&request.work);
        assert!(
            !request.store.is_empty()
                && request.store.len() <= 64
                && request
                    .store
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.')),
            "store {:?} accepted",
            request.store
        );
        assert!(
            request.marketplace.len() <= 16
                && request
                    .marketplace
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
            "marketplace {:?} accepted",
            request.marketplace
        );
        let product = request.product_id.as_str();
        assert!(
            !product.is_empty()
                && product.len() <= 256
                && product.trim() == product
                && !product.chars().any(char::is_control),
            "product id {product:?} accepted"
        );
    }
}

/// The catalog lookup invariants on an accepted question or answer list.
fn check_lookup(body: &Body) {
    match body {
        Body::CatalogAsk { works } if questions_problem(works).is_none() => {
            assert!(works.len() <= 100, "too many questions accepted");
            let mut seen = BTreeSet::new();
            for work in works {
                check_question(work);
                assert!(seen.insert(&work.work), "a work asked twice was accepted");
            }
        }
        Body::CatalogFound { answers, missing } if found_problem(answers, missing).is_none() => {
            assert!(
                answers.len() + missing.len() <= 100,
                "too many answers accepted"
            );
            let mut seen = BTreeSet::new();
            for answer in answers {
                assert!(
                    plain(&answer.work, 64),
                    "work ref {:?} accepted",
                    answer.work
                );
                assert!(
                    seen.insert(&answer.work),
                    "a work answered twice was accepted"
                );
                assert!(
                    (1..=8).contains(&answer.editions.len()),
                    "an answer with {} editions accepted",
                    answer.editions.len()
                );
                let mut keys = BTreeSet::new();
                for edition in &answer.editions {
                    assert!(
                        !edition.metadata.title.trim().is_empty(),
                        "a blank title accepted"
                    );
                    assert!(
                        keys.insert((&edition.store, &edition.marketplace, &edition.product_id)),
                        "an edition listed twice was accepted"
                    );
                }
            }
            for work in missing {
                assert!(plain(work, 64), "missing work ref {work:?} accepted");
                assert!(
                    seen.insert(work),
                    "a work both answered and missing was accepted"
                );
            }
        }
        _ => {}
    }
}

/// The window page invariants on an accepted page, for the cursors a
/// window could have asked with (the first page, and the one just before
/// the page's `next`).
fn check_window(body: &Body) {
    let Body::WindowPage { works, next } = body else {
        return;
    };
    // The first page's cursor, and the next cursor less its last byte,
    // which sorts just before it.
    let before_next = next
        .as_ref()
        .and_then(|n| WindowCursor::parse(&n.as_str()[..n.as_str().len().saturating_sub(1)]))
        .unwrap_or_default();
    for after in [WindowCursor::start(), before_next] {
        if window_page_problem(&after, works, next.as_ref()).is_some() {
            continue;
        }
        assert!(works.len() <= 100, "too many window works accepted");
        if let Some(next) = next {
            assert!(plain(next.as_str(), 64), "next cursor {next:?} accepted");
            assert!(
                next.as_str().as_bytes() > after.as_str().as_bytes(),
                "a cursor that does not move forward accepted"
            );
        }
        let mut seen = BTreeSet::new();
        for work in works {
            assert!(plain(&work.work, 64), "work key {:?} accepted", work.work);
            assert!(seen.insert(&work.work), "a work listed twice was accepted");
            assert!(
                work.identifiers.len() <= 16,
                "too many identifiers accepted"
            );
            let mut ids = BTreeSet::new();
            for id in &work.identifiers {
                assert!(plain(&id.value, 64), "identifier {:?} accepted", id.value);
                assert_eq!(
                    id.scheme == LookupScheme::Asin,
                    !id.marketplace.is_empty(),
                    "marketplace {:?} accepted on {:?}",
                    id.marketplace,
                    id.scheme
                );
                assert!(ids.insert(id), "an identifier listed twice was accepted");
            }
            assert!(
                (1..=8).contains(&work.editions.len()),
                "a window work with {} editions accepted",
                work.editions.len()
            );
            let mut keys = BTreeSet::new();
            for copy in work.editions.iter().filter_map(|e| e.copy.as_ref()) {
                let token = |text: &str, extra: &[u8]| {
                    !text.is_empty()
                        && text.chars().all(|c| {
                            c.is_ascii_lowercase()
                                || c.is_ascii_digit()
                                || u8::try_from(c).is_ok_and(|b| extra.contains(&b))
                        })
                };
                let mut parts = copy.content_type.split('/');
                assert!(
                    copy.content_type.len() <= 64
                        && parts.next().is_some_and(|kind| token(kind, b"+-."))
                        && parts.next().is_some_and(|sub| token(sub, b"+-."))
                        && parts.next().is_none(),
                    "copy content type {:?} accepted",
                    copy.content_type
                );
                assert!(
                    copy.bitrate_kbps
                        .is_none_or(|kbps| (1..=20_000).contains(&kbps)),
                    "copy bitrate {:?} accepted",
                    copy.bitrate_kbps
                );
                assert!(
                    copy.source_store
                        .as_deref()
                        .is_none_or(|store| store.len() <= 64 && token(store, b"_-.")),
                    "copy source store {:?} accepted",
                    copy.source_store
                );
                assert!(
                    plain(&copy.version, 64),
                    "copy version {:?} accepted",
                    copy.version
                );
            }
            for listed in &work.editions {
                let edition = &listed.edition;
                assert!(
                    edition.availability == Availability::Owned
                        || (!listed.yours && listed.copy.is_none()),
                    "a reachable window edition that is the person's or has a copy was accepted"
                );
                assert!(
                    !edition.metadata.title.trim().is_empty(),
                    "a blank title accepted"
                );
                assert!(
                    keys.insert((&edition.store, &edition.marketplace, &edition.product_id)),
                    "an edition listed twice was accepted"
                );
            }
        }
    }
}

/// The `heartbeat` invariant: a public origin the library would keep is a
/// plain `http(s)` origin, checked byte by byte.
fn check_heartbeat(body: &Body) {
    if let Body::Heartbeat {
        public_origin: Some(raw),
        ..
    } = body
    {
        if let Some(origin) = BranchOrigin::parse(raw) {
            let origin = origin.as_str();
            let rest = origin
                .strip_prefix("https://")
                .or_else(|| origin.strip_prefix("http://"));
            assert!(
                origin.len() <= MAX_BRANCH_ORIGIN_BYTES
                    && rest.is_some_and(|rest| {
                        !rest.is_empty()
                            && rest.bytes().all(|b| {
                                b.is_ascii_graphic() && !matches!(b, b'/' | b'?' | b'#' | b'@')
                            })
                    }),
                "branch origin {origin:?} accepted from {raw:?}"
            );
        }
    }
}

/// `frame` as a JSON tree.
fn json(frame: &Frame) -> Value {
    serde_json::from_str(&frame.to_text()).unwrap_or(Value::Null)
}

/// True when `a` and `b` are the same tree, floats equal to within a few
/// rounding steps.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) if x.is_f64() || y.is_f64() => {
            match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) => {
                    x == y || (x - y).abs() <= x.abs().max(y.abs()) * 4.0 * f64::EPSILON
                }
                _ => false,
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(key, x)| y.get(key).is_some_and(|y| same(x, y)))
        }
        _ => a == b,
    }
}

/// The catalog references of `body` its receiver would act on keep `:` and
/// `@` out of their store and marketplace.
fn check_catalog_refs(body: &Body) {
    let refs: Vec<bookclerk_node_proto::CatalogRef> = match body {
        Body::CatalogUpload {
            store,
            product_id,
            marketplace,
            ..
        } => vec![bookclerk_node_proto::CatalogRef {
            store: store.clone(),
            product_id: product_id.clone(),
            marketplace: marketplace.clone(),
        }],
        Body::CatalogSession { wanted, .. } | Body::CatalogAccepted { wanted, .. } => {
            wanted.clone()
        }
        _ => Vec::new(),
    };
    for reference in refs.iter().filter(|r| r.is_well_formed()) {
        for part in [&reference.store, &reference.marketplace] {
            assert!(
                !part.bytes().any(|b| matches!(b, b':' | b'@')),
                "catalog reference part {part:?} accepted"
            );
        }
        assert!(
            !reference.store.is_empty(),
            "an empty store accepted in {reference:?}"
        );
    }
}
