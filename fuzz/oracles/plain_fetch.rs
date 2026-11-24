//! Shared oracle for the `plain_fetch` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one unpacked `PlainFetch` message, a storefront guest's
//! `ContentSource.fetchTitle` reply. The invariants:
//!
//! - decoding and validation never panic;
//! - [`validate_plain_fetch`] accepts the reply exactly when the independent
//!   recount below finds every rule kept, against a fixed declaration that
//!   fetches audio: the carrier is audio, every file has a role and a
//!   non-blank path, there is at most one `whole`, `cover`, `catalogRecord`
//!   and `annotations` file, and the reply has `part`s or a `whole` file but
//!   not both;
//! - [`validate_fetched_paths`] accepts the reply exactly when every path,
//!   with repeated slashes folded, starts with `/c/`, has a non-empty
//!   segment after it, and has no `.` or `..` segment;
//! - an accepted reply re-encodes, decodes back equal, and is accepted again.

use bookclerk_plugin_abi::{
    decode_plain_fetch_bytes, encode_plain_fetch_bytes, validate_fetched_paths,
    validate_plain_fetch, FetchedFileRole, MediaKind, MediaSupport, PlainFetch, WorkKind,
};

/// Largest input worth decoding; real replies are far smaller.
pub const MAX_INPUT_BYTES: usize = 256 * 1024;

/// The storefront declaration every input is checked against.
fn declaration() -> MediaSupport {
    MediaSupport {
        work_kinds: vec![WorkKind::Book],
        lists: vec![MediaKind::Audio, MediaKind::Ebook],
        fetches: vec![MediaKind::Audio],
    }
}

/// The reply rules restated directly: true when the reply keeps them all.
fn keeps_every_rule(fetch: &PlainFetch) -> bool {
    let count = |role: FetchedFileRole| fetch.files.iter().filter(|f| f.role == role).count();
    let parts = count(FetchedFileRole::Part);
    let wholes = count(FetchedFileRole::Whole);
    fetch.media_kind == MediaKind::Audio
        && fetch
            .files
            .iter()
            .all(|f| f.role != FetchedFileRole::Unspecified && !f.path.trim().is_empty())
        && wholes <= 1
        && count(FetchedFileRole::Cover) <= 1
        && count(FetchedFileRole::CatalogRecord) <= 1
        && count(FetchedFileRole::Annotations) <= 1
        && ((parts > 0) != (wholes > 0))
}

/// The cache directory every input's paths are checked against.
const CACHE_DIR: &str = "/c";

/// The path rule restated over the spelling: true when `path` lies inside
/// [`CACHE_DIR`].
fn inside_cache_dir(path: &str) -> bool {
    let mut folded = String::with_capacity(path.len());
    for ch in path.chars() {
        if !(ch == '/' && folded.ends_with('/')) {
            folded.push(ch);
        }
    }
    if folded
        .split(['/', '\\'])
        .any(|seg| seg == "." || seg == "..")
    {
        return false;
    }
    folded
        .strip_prefix("/c/")
        .is_some_and(|rest| rest.split('/').any(|seg| !seg.is_empty()))
}

/// The confinement verdict the oracle expects for `fetch`.
///
/// On Unix [`CACHE_DIR`] is absolute, so each path must lie inside it by
/// [`inside_cache_dir`]. On Windows `/c` has no drive and is not absolute,
/// so it confines nothing: only a reply that names no file passes.
fn expected_confined(fetch: &PlainFetch) -> bool {
    if cfg!(windows) {
        fetch.files.is_empty()
    } else {
        fetch.files.iter().all(|f| inside_cache_dir(&f.path))
    }
}

/// Runs every invariant on one input; panics on a violation.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Ok(fetch) = decode_plain_fetch_bytes(data) else {
        return;
    };
    let media = declaration();
    let verdict = validate_plain_fetch(&fetch, &media);
    assert_eq!(
        verdict.is_ok(),
        keeps_every_rule(&fetch),
        "validation disagrees with the oracle: {verdict:?}"
    );
    let confined = validate_fetched_paths(&fetch, &[std::path::Path::new(CACHE_DIR)]);
    assert_eq!(
        confined.is_ok(),
        expected_confined(&fetch),
        "path confinement disagrees with the oracle: {confined:?}"
    );
    if verdict.is_ok() {
        let bytes = encode_plain_fetch_bytes(&fetch)
            .unwrap_or_else(|err| panic!("an accepted reply must encode: {err}"));
        let again = decode_plain_fetch_bytes(&bytes)
            .unwrap_or_else(|err| panic!("an encoded reply must decode: {err}"));
        assert_eq!(again, fetch, "an accepted reply round-trips");
        assert!(validate_plain_fetch(&again, &media).is_ok());
    }
}
