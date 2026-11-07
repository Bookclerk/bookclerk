//! Replays the checked-in `sql_parse`, `abi_decode` and `abi_roundtrip` fuzz
//! corpora in PR CI
//! (no libFuzzer required), so an input that once crashed a decoder stays
//! fixed (`docs/adr/untrusted-input-fuzzing.md`).
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

/// Regular files directly under `fuzz/corpus/<target>/`, canonicalized and
/// confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn corpus_files(target: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus")
        .join(target)
        .canonicalize()
        .expect("canonicalize fuzz corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fuzz corpus")
        .map(|entry| entry.expect("entry").path())
        // Inline canonicalize → starts_with at each sink (CodeQL two-state barrier).
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty corpus for {target}");
    files
}

#[test]
fn fuzz_corpus_sql_parse_does_not_panic() {
    for path in corpus_files("sql_parse") {
        let sql = std::fs::read_to_string(&path).expect("read");
        let _ = bookclerk_plugin_abi::sql_v1_pack_statements(&sql);
        let _ = bookclerk_plugin_abi::validate_sql_v1_grammar(&sql, false);
        let _ = bookclerk_plugin_abi::validate_sql_v1_grammar(&sql, true);
        let _ = bookclerk_plugin_abi::require_portable_text(&sql);
        let _ = bookclerk_plugin_abi::desugar_canonical_sql(&sql);
        let _ = bookclerk_plugin_abi::like_pattern_sources(&sql);
        let _ = bookclerk_plugin_abi::require_like_patterns_within(
            &sql,
            &[],
            bookclerk_plugin_abi::D1_PORTABLE_LIKE_PATTERN_BYTES,
        );
        let _ = bookclerk_plugin_abi::require_function_args_within(&sql, 32);
    }
}

/// Mirrors `fuzz/fuzz_targets/abi_decode.rs`: the first byte selects the
/// decoder, the rest is the message.
#[test]
fn fuzz_corpus_abi_decode_does_not_panic() {
    const HAND_WRITTEN: u8 = 4;
    for path in corpus_files("abi_decode") {
        let data = std::fs::read(&path).expect("read");
        let Some((&selector, bytes)) = data.split_first() else {
            continue;
        };
        match selector % (HAND_WRITTEN + bookclerk_plugin_abi::READ_ANY_COUNT) {
            0 => {
                let _ = bookclerk_plugin_abi::decode_execute_request_bytes(bytes);
            }
            1 => {
                let _ = bookclerk_plugin_abi::decode_execute_result_reply_bytes(bytes);
            }
            2 => {
                let _ = bookclerk_plugin_abi::decode_db_value_bytes(bytes);
            }
            3 => {
                let _ = bookclerk_plugin_abi::decode_db_capabilities_reply_bytes(bytes);
            }
            generated => {
                let _ = bookclerk_plugin_abi::read_any_message(generated - HAND_WRITTEN, bytes);
            }
        }
    }
}

/// An empty root struct decodes through every generated codec: the
/// selectors the fuzz target can reach are all live.
#[test]
fn read_any_message_covers_every_selector() {
    let empty_root: Vec<u8> = [0u8, 0, 0, 0, 1, 0, 0, 0]
        .iter()
        .chain([0u8; 8].iter())
        .copied()
        .collect();
    for selector in 0..bookclerk_plugin_abi::READ_ANY_COUNT {
        // Empty unions report "unset", which is an error, not a panic; only a
        // selector with no dispatch arm is a failure here.
        if let Err(err) = bookclerk_plugin_abi::read_any_message(selector, &empty_root) {
            assert!(
                !err.to_string().contains("unknown read_any selector"),
                "selector {selector} has no dispatch arm: {err}"
            );
        }
    }
    assert!(bookclerk_plugin_abi::read_any_message(
        bookclerk_plugin_abi::READ_ANY_COUNT,
        &empty_root
    )
    .is_err());
}

/// Mirrors `fuzz/fuzz_targets/abi_roundtrip.rs`: a value any generated codec
/// decodes must survive `write_*` then `read_*` unchanged.
#[test]
fn fuzz_corpus_abi_roundtrip_is_identity() {
    use bookclerk_plugin_abi::{roundtrip_any_message, RoundTripFailure, READ_ANY_COUNT};

    let mut decoded = 0;
    for path in corpus_files("abi_roundtrip") {
        let data = std::fs::read(&path).expect("read");
        let Some((&selector, bytes)) = data.split_first() else {
            continue;
        };
        match roundtrip_any_message(selector % READ_ANY_COUNT, bytes) {
            Ok(()) => decoded += 1,
            Err(RoundTripFailure::Rejected(_)) => {}
            Err(RoundTripFailure::Codec(why)) => {
                panic!("{}: selector {selector}: {why}", path.display())
            }
        }
    }
    assert!(decoded > 0, "no abi_roundtrip seed decoded");
}

/// `read_any_message` selectors of the structured book records, whose seeds
/// live in `fuzz/corpus/{abi_decode,abi_roundtrip}/structured_*.bin`.
const SCAN_SUMMARY_SELECTOR: u8 = 29;
/// See [`SCAN_SUMMARY_SELECTOR`].
const CATALOG_HITS_SELECTOR: u8 = 46;
/// See [`SCAN_SUMMARY_SELECTOR`].
const METADATA_RECORDS_SELECTOR: u8 = 91;
/// See [`SCAN_SUMMARY_SELECTOR`].
const METADATA_CHAPTERS_SELECTOR: u8 = 94;
/// See [`SCAN_SUMMARY_SELECTOR`].
const COVER_FETCH_PARAMS_SELECTOR: u8 = 96;
/// See [`SCAN_SUMMARY_SELECTOR`].
const COVER_IMAGES_SELECTOR: u8 = 99;

/// A scan summary with every structured book record filled.
fn structured_scan_summary() -> bookclerk_plugin_abi::ScanSummary {
    use bookclerk_plugin_abi::{
        Abridgement, Credit, CreditLevel, CreditRole, ExpressionKind, Identifier, IdentifierScheme,
        MediaKind, ScanBook, ScanSummary, SeriesEntry, WorkKind,
    };
    ScanSummary {
        accounts: 1,
        books_upserted: 1,
        pages: 1,
        skipped_disabled: 0,
        books: vec![ScanBook {
            account_id: "acct".into(),
            product_id: "B0EPISODE".into(),
            title: "Trailer".into(),
            marketplace: Some("us".into()),
            work_kind: WorkKind::PodcastEpisode,
            expression_kind: ExpressionKind::PodcastAudio,
            media_kind: MediaKind::PodcastAudio,
            identifiers: vec![Identifier {
                scheme: IdentifierScheme::Asin,
                value: "B0EPISODE".into(),
                marketplace: Some("us".into()),
            }],
            credits: vec![
                Credit {
                    name: "Host One".into(),
                    role: CreditRole::Host,
                    level: CreditLevel::RoleDefault,
                },
                Credit {
                    name: "Guest Two".into(),
                    role: CreditRole::Cast,
                    level: CreditLevel::Edition,
                },
            ],
            series_entries: vec![SeriesEntry {
                name: "Season 1".into(),
                position: Some("1".into()),
                store_series_id: Some("B0SEASON".into()),
            }],
            parent_product_id: Some("B0SHOW".into()),
            language: Some("en".into()),
            abridgement: Abridgement::Unabridged,
            published_at: Some("2026-01-01T00:00:00Z".into()),
            ..ScanBook::default()
        }],
    }
}

/// A catalog hit list with the structured records of a print edition.
fn structured_catalog_hits() -> bookclerk_plugin_abi::CatalogHits {
    use bookclerk_plugin_abi::{
        CatalogHit, CatalogHits, Credit, CreditRole, ExpressionKind, Identifier, IdentifierScheme,
        MediaKind,
    };
    CatalogHits {
        hits: vec![CatalogHit {
            product_id: "9780000000001".into(),
            title: "A Novel".into(),
            origin: "search".into(),
            expression_kind: ExpressionKind::Text,
            media_kind: MediaKind::Print,
            identifiers: vec![
                Identifier {
                    scheme: IdentifierScheme::Isbn,
                    value: "9780000000001".into(),
                    marketplace: None,
                },
                Identifier {
                    scheme: IdentifierScheme::OpenLibraryWork,
                    value: "OL1W".into(),
                    marketplace: None,
                },
            ],
            credits: vec![Credit {
                name: "Martin Luther King, Jr.".into(),
                role: CreditRole::Author,
                ..Credit::default()
            }],
            ..CatalogHit::default()
        }],
    }
}

/// Metadata provider records for an audiobook edition and its print edition.
fn structured_metadata_records() -> bookclerk_plugin_abi::MetadataRecords {
    use bookclerk_plugin_abi::{
        Abridgement, Credit, CreditLevel, CreditRole, ExpressionKind, Identifier, IdentifierScheme,
        MediaKind, MetadataRecord, MetadataRecords, SeriesEntry,
    };
    MetadataRecords {
        records: vec![
            MetadataRecord {
                title: "Dune".into(),
                cover_url: Some("https://covers.openlibrary.org/b/id/1-L.jpg".into()),
                length_minutes: Some(1263),
                language: Some("en".into()),
                abridgement: Abridgement::Unabridged,
                identifiers: vec![Identifier {
                    scheme: IdentifierScheme::Asin,
                    value: "B002V1OF70".into(),
                    marketplace: Some("us".into()),
                }],
                credits: vec![
                    Credit {
                        name: "Frank Herbert".into(),
                        role: CreditRole::Author,
                        level: CreditLevel::RoleDefault,
                    },
                    Credit {
                        name: "Scott Brick".into(),
                        role: CreditRole::Narrator,
                        level: CreditLevel::Expression,
                    },
                ],
                series_entries: vec![SeriesEntry {
                    name: "Dune".into(),
                    position: Some("1".into()),
                    store_series_id: None,
                }],
                rating_overall: Some(4.5),
                rating_count: Some(1200),
                ..MetadataRecord::default()
            },
            MetadataRecord {
                title: "Dune".into(),
                expression_kind: ExpressionKind::Text,
                media_kind: MediaKind::Print,
                identifiers: vec![
                    Identifier {
                        scheme: IdentifierScheme::Isbn,
                        value: "9780441172719".into(),
                        marketplace: None,
                    },
                    Identifier {
                        scheme: IdentifierScheme::OpenLibraryWork,
                        value: "OL893415W".into(),
                        marketplace: None,
                    },
                ],
                ..MetadataRecord::default()
            },
        ],
    }
}

/// A nested chapter layout with brand intro and outro lengths.
fn structured_metadata_chapters() -> bookclerk_plugin_abi::MetadataChapters {
    use bookclerk_plugin_abi::{MetadataChapter, MetadataChapters};
    MetadataChapters {
        found: true,
        chapters: vec![
            MetadataChapter {
                title: "Book One".into(),
                start_ms: 0,
                length_ms: None,
                depth: 0,
            },
            MetadataChapter {
                title: "Chapter 1".into(),
                start_ms: 2_000,
                length_ms: Some(60_000),
                depth: 1,
            },
        ],
        brand_intro_ms: Some(2_000),
        brand_outro_ms: Some(5_000),
        runtime_ms: Some(75_780_000),
        accurate: true,
    }
}

/// A cover request for two URLs, the first conditional on a cached copy.
fn structured_cover_fetch_params() -> bookclerk_plugin_abi::CoverFetchParams {
    bookclerk_plugin_abi::CoverFetchParams {
        urls: vec![
            "https://covers.openlibrary.org/b/id/8231856-L.jpg".into(),
            "https://m.media-amazon.com/images/I/51x.jpg".into(),
        ],
        max_bytes: 10_485_760,
        conditions: vec![bookclerk_plugin_abi::CoverCondition {
            if_none_match: Some("\"5f2b\"".into()),
            if_modified_since: Some("Wed, 21 Oct 2026 07:28:00 GMT".into()),
        }],
    }
}

/// One stored, one failed and one unchanged cover answer.
fn structured_cover_images() -> bookclerk_plugin_abi::CoverImages {
    use bookclerk_plugin_abi::{CoverImage, CoverImages};
    CoverImages {
        images: vec![
            CoverImage {
                url: "https://covers.openlibrary.org/b/id/8231856-L.jpg".into(),
                stored: true,
                content_type: Some("image/jpeg".into()),
                etag: Some("\"5f2c\"".into()),
                cache_control: Some("public, max-age=86400".into()),
                date: Some("Thu, 05 Nov 2026 22:00:00 GMT".into()),
                ..CoverImage::default()
            },
            CoverImage {
                url: "https://m.media-amazon.com/images/I/51x.jpg".into(),
                error: Some("HTTP 404".into()),
                ..CoverImage::default()
            },
            CoverImage {
                url: "https://www.gravatar.com/avatar/00?d=404&s=128".into(),
                not_modified: true,
                etag: Some("W/\"a1\"".into()),
                last_modified: Some("Wed, 21 Oct 2026 07:28:00 GMT".into()),
                expires: Some("Thu, 05 Nov 2026 22:05:00 GMT".into()),
                age: Some("12".into()),
                ..CoverImage::default()
            },
        ],
    }
}

/// Unpacked single-segment message bytes for one root struct.
// Test helper: a failed encode is the test failing.
#[allow(clippy::expect_used)]
fn message_bytes<F>(build: F) -> Vec<u8>
where
    F: FnOnce(&mut capnp::message::Builder<capnp::message::HeapAllocator>),
{
    let mut message = capnp::message::Builder::new_default();
    build(&mut message);
    let mut bytes = Vec::new();
    capnp::serialize::write_message(&mut bytes, &message).expect("serialize");
    bytes
}

/// The committed structured seeds decode to the records above and survive
/// the round trip; `UPDATE_FUZZ_SEEDS=1` rewrites them.
// Test code: failed I/O or encoding is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn structured_book_record_seeds_are_current() {
    use bookclerk_plugin_abi::{
        plugin_capnp, write_catalog_hits, write_cover_fetch_params, write_cover_images,
        write_metadata_chapters, write_metadata_records, write_scan_summary,
    };

    let scan = message_bytes(|m| {
        write_scan_summary(
            m.init_root::<plugin_capnp::scan_summary::Builder<'_>>(),
            &structured_scan_summary(),
        )
        .expect("write scan summary");
    });
    let hits = message_bytes(|m| {
        write_catalog_hits(
            m.init_root::<plugin_capnp::catalog_hits::Builder<'_>>(),
            &structured_catalog_hits(),
        )
        .expect("write catalog hits");
    });
    let records = message_bytes(|m| {
        write_metadata_records(
            m.init_root::<plugin_capnp::metadata_records::Builder<'_>>(),
            &structured_metadata_records(),
        )
        .expect("write metadata records");
    });
    let chapters = message_bytes(|m| {
        write_metadata_chapters(
            m.init_root::<plugin_capnp::metadata_chapters::Builder<'_>>(),
            &structured_metadata_chapters(),
        )
        .expect("write metadata chapters");
    });
    let cover_params = message_bytes(|m| {
        write_cover_fetch_params(
            m.init_root::<plugin_capnp::cover_fetch_params::Builder<'_>>(),
            &structured_cover_fetch_params(),
        )
        .expect("write cover params");
    });
    let cover_images = message_bytes(|m| {
        write_cover_images(
            m.init_root::<plugin_capnp::cover_images::Builder<'_>>(),
            &structured_cover_images(),
        )
        .expect("write cover images");
    });
    let reader = |bytes: &[u8]| {
        capnp::serialize::read_message(
            &mut std::io::Cursor::new(bytes.to_vec()),
            capnp::message::ReaderOptions::new(),
        )
        .expect("read message")
    };
    let scan_message = reader(&scan);
    assert_eq!(
        bookclerk_plugin_abi::read_scan_summary(
            scan_message
                .get_root::<plugin_capnp::scan_summary::Reader<'_>>()
                .expect("scan root")
        )
        .expect("decode scan summary"),
        structured_scan_summary()
    );
    let hits_message = reader(&hits);
    assert_eq!(
        bookclerk_plugin_abi::read_catalog_hits(
            hits_message
                .get_root::<plugin_capnp::catalog_hits::Reader<'_>>()
                .expect("hits root")
        )
        .expect("decode catalog hits"),
        structured_catalog_hits()
    );
    let records_message = reader(&records);
    assert_eq!(
        bookclerk_plugin_abi::read_metadata_records(
            records_message
                .get_root::<plugin_capnp::metadata_records::Reader<'_>>()
                .expect("records root")
        )
        .expect("decode metadata records"),
        structured_metadata_records()
    );
    let chapters_message = reader(&chapters);
    assert_eq!(
        bookclerk_plugin_abi::read_metadata_chapters(
            chapters_message
                .get_root::<plugin_capnp::metadata_chapters::Reader<'_>>()
                .expect("chapters root")
        )
        .expect("decode metadata chapters"),
        structured_metadata_chapters()
    );
    let params_message = reader(&cover_params);
    assert_eq!(
        bookclerk_plugin_abi::read_cover_fetch_params(
            params_message
                .get_root::<plugin_capnp::cover_fetch_params::Reader<'_>>()
                .expect("cover params root")
        )
        .expect("decode cover params"),
        structured_cover_fetch_params()
    );
    let images_message = reader(&cover_images);
    assert_eq!(
        bookclerk_plugin_abi::read_cover_images(
            images_message
                .get_root::<plugin_capnp::cover_images::Reader<'_>>()
                .expect("cover images root")
        )
        .expect("decode cover images"),
        structured_cover_images()
    );
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus");
    for (name, selector, bytes) in [
        ("structured_scan_summary.bin", SCAN_SUMMARY_SELECTOR, &scan),
        ("structured_catalog_hits.bin", CATALOG_HITS_SELECTOR, &hits),
        (
            "structured_metadata_records.bin",
            METADATA_RECORDS_SELECTOR,
            &records,
        ),
        (
            "structured_metadata_chapters.bin",
            METADATA_CHAPTERS_SELECTOR,
            &chapters,
        ),
        (
            "structured_cover_fetch_params.bin",
            COVER_FETCH_PARAMS_SELECTOR,
            &cover_params,
        ),
        (
            "structured_cover_images.bin",
            COVER_IMAGES_SELECTOR,
            &cover_images,
        ),
    ] {
        bookclerk_plugin_abi::roundtrip_any_message(selector, bytes)
            .unwrap_or_else(|_| panic!("{name} does not round-trip at selector {selector}"));
        // `abi_decode` puts four hand-written decoders ahead of the table.
        for (target, prefix) in [("abi_decode", selector + 4), ("abi_roundtrip", selector)] {
            let path = corpus.join(target).join(name);
            let mut seed = vec![prefix];
            seed.extend_from_slice(bytes);
            if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
                std::fs::write(&path, &seed).expect("write seed");
            }
            assert_eq!(
                std::fs::read(&path).expect("read seed"),
                seed,
                "{} is stale; rerun with UPDATE_FUZZ_SEEDS=1",
                path.display()
            );
        }
    }
}

#[path = "../../../fuzz/oracles/cover_sink_key.rs"]
mod cover_sink_key_oracle;

/// Mirrors `fuzz/fuzz_targets/cover_sink_key.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_cover_sink_key_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("cover_sink_key") {
        cover_sink_key_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no cover_sink_key seed");
}

/// Seeds for the hand-written envelope decoders that carry a typed
/// `limit_exceeded` error: an `ExecuteResultReply` whose `err` has
/// `details`, and a `DbCapabilitiesReply` in both members. Refresh with
/// `UPDATE_FUZZ_SEEDS=1`.
#[test]
fn limit_error_envelope_seeds_are_current() {
    use bookclerk_plugin_abi::{
        decode_db_capabilities_reply_bytes, decode_execute_result_reply_bytes,
        encoded_db_capabilities_reply_bytes, encoded_execute_result_reply_bytes, DbCapabilities,
        LimitExceeded, PluginError, PluginErrorCode,
    };

    let limit = LimitExceeded::new("maxResultBytes", 262_144, 300_000);
    let reply_err =
        encoded_execute_result_reply_bytes(Err(PluginError::limit_exceeded(limit.clone())))
            .expect("encode reply err");
    let decoded = decode_execute_result_reply_bytes(&reply_err).expect_err("err member");
    assert_eq!(decoded.code, PluginErrorCode::LimitExceeded);
    assert_eq!(decoded.limit(), Some(limit.clone()));

    let caps_ok = encoded_db_capabilities_reply_bytes(Ok(DbCapabilities::advertised_sqlite()))
        .expect("encode caps ok");
    assert_eq!(
        decode_db_capabilities_reply_bytes(&caps_ok).expect("ok member"),
        DbCapabilities::advertised_sqlite()
    );
    let caps_err =
        encoded_db_capabilities_reply_bytes(Err(PluginError::limit_exceeded(limit.clone())))
            .expect("encode caps err");
    assert_eq!(
        decode_db_capabilities_reply_bytes(&caps_err)
            .expect_err("err member")
            .limit(),
        Some(limit)
    );

    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/abi_decode");
    for (name, selector, bytes) in [
        ("limit_error_reply.bin", 1u8, &reply_err),
        ("caps_reply_ok.bin", 3u8, &caps_ok),
        ("caps_reply_limit_error.bin", 3u8, &caps_err),
    ] {
        let path = corpus.join(name);
        let mut seed = vec![selector];
        seed.extend_from_slice(bytes);
        if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
            std::fs::write(&path, &seed).expect("write seed");
        }
        assert_eq!(
            std::fs::read(&path).expect("read seed"),
            seed,
            "{} is stale; rerun with UPDATE_FUZZ_SEEDS=1",
            path.display()
        );
    }
}
