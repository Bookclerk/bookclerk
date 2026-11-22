//! Shared oracle for the `b2_reply` fuzz target and the `bookclerk-storage`
//! corpus replay test (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is what the `b2` destination reads from Backblaze B2: byte 0
//! picks the reader (its value modulo 9) and the rest is the bytes.
//!
//! - 0: `b2_authorize_account`; 1: `b2_list_buckets`; 2: an upload URL;
//!   3: one file version; 4: a `b2_list_file_names` page; 5: a
//!   `b2_list_file_versions` page; 6: an uploaded part; 7: an error reply
//!   (byte 1 and 2 are the status); 8: download headers, one
//!   `name: value` per line.
//!
//! The invariants:
//!
//! - reading never panics and gives the same answer twice;
//! - an error reply always reads, with a non-empty code and code and
//!   message within their caps and free of control characters;
//! - an accepted JSON reply is a JSON object, as is every struct nested in
//!   it (`apiInfo`, `storageApi`, each bucket and each listed file), and an
//!   independent reading
//!   of the same JSON (a `serde_json::Value` walk) finds the same token, URLs, names, counts and
//!   part numbers, every file name passes the oracle's own copy of B2's
//!   naming rules, a listing's continuation name is past every listed
//!   name, and every SHA-1 is 40 lowercase hex digits;
//! - percent-encoding any text and decoding it gives the text back, and the
//!   encoding matches the oracle's own encoder.

use bookclerk_storage::b2_api::{
    decode_file_name, encode_file_name, parse_api_error, parse_authorization, parse_buckets,
    parse_download_headers, parse_file_version, parse_names_page, parse_part, parse_upload_target,
    parse_versions_page, FileAction, FileVersion, MAX_ERROR_TEXT_BYTES, MAX_LIST_FILES, MAX_PARTS,
};
use serde_json::Value;

/// B2's naming rules, written apart from the parser's.
fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 1024
        && !name.bytes().any(|b| b < 0x20 || b == 0x7f || b == b'\\')
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("//")
}

/// B2's bucket naming rule, written apart from the parser's: letters,
/// digits, `-` and `.`, no `.` at either end or doubled, not an IPv4
/// address.
fn bucket_name_ok(name: &str) -> bool {
    let labels: Vec<&str> = name.split('.').collect();
    let ipv4 = labels.len() == 4 && labels.iter().all(|label| label.parse::<u32>().is_ok());
    (1..=63).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && labels.iter().all(|label| !label.is_empty())
        && !ipv4
}

/// Whether `text` is 40 lowercase hex digits.
fn sha1_ok(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether `url` is a plain `https` URL with a host.
fn url_ok(url: &str) -> bool {
    url.strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|host| !host.is_empty() && !host.contains('@'))
        && !url.contains(['?', '#'])
}

/// The oracle's own percent-encoder.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Checks one accepted version against its JSON.
fn check_version(file: &FileVersion, json: Option<&Value>) {
    let name = match file.action {
        FileAction::Folder => file.file_name.strip_suffix('/').unwrap_or(&file.file_name),
        FileAction::Upload | FileAction::Copy | FileAction::Hide | FileAction::Start => {
            &file.file_name
        }
    };
    assert!(name_ok(name), "{name:?}");
    assert_eq!(file.file_id.is_none(), file.action == FileAction::Folder);
    if let Some(sha1) = &file.content_sha1 {
        assert!(sha1_ok(sha1), "{sha1}");
    }
    if let Some(json) = json {
        assert!(json.is_object(), "accepted file entry is an object");
        assert_eq!(json["fileName"].as_str(), Some(file.file_name.as_str()));
        if let Some(id) = &file.file_id {
            assert_eq!(json["fileId"].as_str(), Some(id.as_str()));
        }
    }
}

/// Checks a listing's files against its JSON array.
fn check_files(files: &[FileVersion], json: &Value) {
    let listed = json["files"]
        .as_array()
        .expect("accepted listing has files");
    assert_eq!(listed.len(), files.len());
    assert!(files.len() <= MAX_LIST_FILES);
    for (file, json) in files.iter().zip(listed) {
        check_version(file, Some(json));
    }
}

/// Runs every invariant on one input.
pub fn check(data: &[u8]) {
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    if let Ok(text) = std::str::from_utf8(rest) {
        let encoded = encode_file_name(text);
        assert_eq!(encoded, encode(text));
        assert_eq!(decode_file_name(&encoded).as_deref(), Some(text));
    }
    let json: Option<Value> = serde_json::from_slice(rest).ok();
    let object = json.as_ref().is_some_and(Value::is_object);
    match selector % 9 {
        0 => {
            let first = parse_authorization(rest);
            assert_eq!(first, parse_authorization(rest));
            assert!(first.is_err() || object);
            if let Ok(auth) = first {
                let json = json.expect("accepted reply is JSON");
                assert!(json["apiInfo"]["storageApi"].is_object());
                assert_eq!(
                    json["authorizationToken"].as_str(),
                    Some(auth.token.expose())
                );
                assert!(url_ok(auth.api_url.as_str()) && url_ok(auth.download_url.as_str()));
                assert!(auth.absolute_minimum_part_size > 0);
                assert!(auth.recommended_part_size >= auth.absolute_minimum_part_size);
                if let Some(bucket) = &auth.bucket {
                    assert!(bucket_name_ok(&bucket.name), "{:?}", bucket.name);
                }
            }
        }
        1 => {
            let first = parse_buckets(rest);
            assert_eq!(first, parse_buckets(rest));
            assert!(first.is_err() || object);
            if let Ok(buckets) = first {
                let json = json.expect("accepted reply is JSON");
                let listed = json["buckets"]
                    .as_array()
                    .expect("accepted reply has buckets");
                assert_eq!(listed.len(), buckets.len());
                assert!(listed.iter().all(Value::is_object));
                assert!(buckets.iter().all(|bucket| bucket_name_ok(&bucket.name)));
            }
        }
        2 => {
            let first = parse_upload_target(rest);
            assert_eq!(first, parse_upload_target(rest));
            assert!(first.is_err() || object);
            if let Ok(target) = first {
                assert!(url_ok(target.url.as_str()));
                assert!(target.token.expose().bytes().all(|b| b.is_ascii_graphic()));
            }
        }
        3 => {
            let first = parse_file_version(rest);
            assert_eq!(first, parse_file_version(rest));
            assert!(first.is_err() || object);
            if let Ok(file) = first {
                check_version(&file, json.as_ref());
            }
        }
        4 => {
            let first = parse_names_page(rest);
            assert_eq!(first, parse_names_page(rest));
            assert!(first.is_err() || object);
            if let Ok(page) = first {
                check_files(&page.files, &json.expect("accepted reply is JSON"));
                if let Some(next) = &page.next_file_name {
                    assert!(name_ok(next));
                    assert!(page.files.iter().all(|file| &file.file_name < next));
                }
            }
        }
        5 => {
            let first = parse_versions_page(rest);
            assert_eq!(first, parse_versions_page(rest));
            assert!(first.is_err() || object);
            if let Ok(page) = first {
                check_files(&page.files, &json.expect("accepted reply is JSON"));
                if let Some((next, _)) = &page.next {
                    assert!(name_ok(next));
                    assert!(page.files.iter().all(|file| &file.file_name <= next));
                }
            }
        }
        6 => {
            let first = parse_part(rest);
            assert_eq!(first, parse_part(rest));
            assert!(first.is_err() || object);
            if let Ok(part) = first {
                let json = json.expect("accepted reply is JSON");
                assert!((1..=MAX_PARTS).contains(&part.part_number));
                assert_eq!(
                    json["partNumber"].as_u64(),
                    Some(u64::from(part.part_number))
                );
                assert!(sha1_ok(&part.content_sha1));
            }
        }
        7 => {
            let status = rest
                .get(..2)
                .map_or(500, |b| u16::from_be_bytes([b[0], b[1]]));
            let body = rest.get(2..).unwrap_or_default();
            let err = parse_api_error(status, body);
            assert_eq!(err, parse_api_error(status, body));
            assert_eq!(err.status, status);
            assert!(!err.code.is_empty() && err.code.len() <= MAX_ERROR_TEXT_BYTES);
            assert!(err.message.len() <= MAX_ERROR_TEXT_BYTES);
            assert!(!err
                .code
                .chars()
                .chain(err.message.chars())
                .any(char::is_control));
            let _ = err.to_string();
        }
        _ => {
            let lines: Vec<(&str, &[u8])> = rest
                .split(|&b| b == b'\n')
                .filter_map(|line| {
                    let colon = line.iter().position(|&b| b == b':')?;
                    let name = std::str::from_utf8(&line[..colon]).ok()?;
                    let value = line[colon + 1..].trim_ascii();
                    Some((name.trim(), value))
                })
                .collect();
            let first = parse_download_headers(lines.clone());
            assert_eq!(first, parse_download_headers(lines));
            if let Ok(head) = first {
                assert!(
                    head.body_offset.saturating_add(head.body_length)
                        <= head.version.content_length
                );
                assert!(head.version.file_id.is_some());
                check_version(&head.version, None);
            }
        }
    }
}
