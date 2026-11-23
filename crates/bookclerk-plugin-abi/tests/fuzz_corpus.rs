//! Replays the checked-in `sql_parse`, `abi_decode`, `abi_roundtrip`,
//! `cover_sink_key`, `db_stream`, `scan_page`, `quality_offers`, `plain_fetch`, `metadata_file_record`, `notifier_reply`, `notify_address_body`, `notify_raise`, `ai_run_reply`, `ai_run`, `event_read`, `socket_request`, `webhook_message`, `storage_reply`, `origin_request` and `provision_reply` fuzz corpora in PR CI
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

/// Hand-written decoders `abi_decode` puts ahead of the generated table;
/// matches `HAND_WRITTEN` in `fuzz/fuzz_targets/abi_decode.rs`.
const ABI_DECODE_HAND_WRITTEN: u8 = 6;

/// Mirrors `fuzz/fuzz_targets/abi_decode.rs`: the first byte selects the
/// decoder, the rest is the message.
#[test]
fn fuzz_corpus_abi_decode_does_not_panic() {
    const HAND_WRITTEN: u8 = ABI_DECODE_HAND_WRITTEN;
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
            4 => {
                let _ = bookclerk_plugin_abi::decode_empty_reply_bytes(bytes);
            }
            5 => {
                if let Ok(err) = bookclerk_plugin_abi::decode_plugin_error_bytes(bytes) {
                    if let Some(details) = &err.details {
                        let text = serde_json::Value::Object(details.clone()).to_string();
                        assert_eq!(
                            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
                                &text
                            )
                            .ok()
                            .as_ref(),
                            Some(details),
                            "{}: kept details must round-trip",
                            path.display()
                        );
                    }
                }
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
const SCAN_PAGE_SELECTOR: u8 = 31;
/// See [`SCAN_PAGE_SELECTOR`].
const CATALOG_HITS_SELECTOR: u8 = 48;
/// See [`SCAN_PAGE_SELECTOR`].
const METADATA_RECORDS_SELECTOR: u8 = 102;
/// See [`SCAN_PAGE_SELECTOR`].
const METADATA_CHAPTERS_SELECTOR: u8 = 105;
/// See [`SCAN_PAGE_SELECTOR`].
const COVER_FETCH_PARAMS_SELECTOR: u8 = 107;
/// See [`SCAN_PAGE_SELECTOR`].
const COVER_IMAGES_SELECTOR: u8 = 110;

/// A scan page with every structured book record and paging field filled.
fn structured_scan_page() -> bookclerk_plugin_abi::ScanPage {
    use bookclerk_plugin_abi::{
        Abridgement, Credit, CreditLevel, CreditRole, ExpressionKind, Identifier, IdentifierScheme,
        MediaKind, ScanBook, ScanPage, SeriesEntry, WorkKind,
    };
    ScanPage {
        removed: vec!["B0GONE".into()],
        next_cursor: "page=2".into(),
        sync_token: "purchased_after=2026-01-01".into(),
        complete: false,
        store_pages: 1,
        refreshed_credentials: None,
        books: vec![ScanBook {
            account_id: "acct".into(),
            product_id: "B0EPISODE".into(),
            title: "Trailer".into(),
            marketplace: Some("us".into()),
            work_kind: WorkKind::Podcast,
            expression_kind: ExpressionKind::OriginalAudio,
            media_kind: MediaKind::Audio,
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
        MediaKind, WorkKind,
    };
    CatalogHits {
        hits: vec![CatalogHit {
            product_id: "9780000000001".into(),
            title: "A Novel".into(),
            origin: "search".into(),
            work_kind: WorkKind::Book,
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
            rank_hint: Some(21),
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
        media: bookclerk_plugin_abi::RemoteMedia::Image,
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
        write_metadata_chapters, write_metadata_records, write_scan_page,
    };

    let scan = message_bytes(|m| {
        write_scan_page(
            m.init_root::<plugin_capnp::scan_page::Builder<'_>>(),
            &structured_scan_page(),
        )
        .expect("write scan page");
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
        bookclerk_plugin_abi::read_scan_page(
            scan_message
                .get_root::<plugin_capnp::scan_page::Reader<'_>>()
                .expect("scan root")
        )
        .expect("decode scan page"),
        structured_scan_page()
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
        ("structured_scan_page.bin", SCAN_PAGE_SELECTOR, &scan),
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
        // `abi_decode` puts its hand-written decoders ahead of the table.
        for (target, prefix) in [
            ("abi_decode", selector + ABI_DECODE_HAND_WRITTEN),
            ("abi_roundtrip", selector),
        ] {
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

/// Seeds for the hand-written decoders that carry a typed `limit_exceeded`
/// error: an `ExecuteResultReply` whose `err` has `details`, and a
/// `DbCapabilitiesReply`, an `EmptyReply` and a bare `PluginError` in both
/// shapes. Refresh with `UPDATE_FUZZ_SEEDS=1`.
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
        Some(limit.clone())
    );

    // The reply helpers every payload-free guest reply and guest error goes
    // through: an `EmptyReply` in both members, and a bare `PluginError`
    // whose `details` are kept (an object) or dropped (not one).
    let write_error = |mut b: bookclerk_plugin_abi::plugin_capnp::plugin_error::Builder<'_>,
                       err: &PluginError,
                       details: &str| {
        b.set_code(err.wire_str());
        b.set_message(&err.message);
        b.set_details(details);
    };
    let limit_err = PluginError::limit_exceeded(limit.clone());
    let limit_details = serde_json::Value::Object(limit.details()).to_string();
    let empty_ok = message_bytes(|m| {
        m.init_root::<bookclerk_plugin_abi::plugin_capnp::empty_reply::Builder<'_>>()
            .set_ok(());
    });
    let empty_err = message_bytes(|m| {
        let reply = m.init_root::<bookclerk_plugin_abi::plugin_capnp::empty_reply::Builder<'_>>();
        write_error(reply.init_err(), &limit_err, &limit_details);
    });
    let error_details = message_bytes(|m| {
        write_error(
            m.init_root::<bookclerk_plugin_abi::plugin_capnp::plugin_error::Builder<'_>>(),
            &limit_err,
            &limit_details,
        );
    });
    let error_bad_details = message_bytes(|m| {
        write_error(
            m.init_root::<bookclerk_plugin_abi::plugin_capnp::plugin_error::Builder<'_>>(),
            &PluginError::unavailable("guest restarting"),
            "[1, 2]",
        );
    });
    assert_eq!(
        bookclerk_plugin_abi::decode_empty_reply_bytes(&empty_err)
            .expect_err("err member")
            .limit(),
        Some(limit.clone())
    );
    assert!(bookclerk_plugin_abi::decode_empty_reply_bytes(&empty_ok).is_ok());
    assert_eq!(
        bookclerk_plugin_abi::decode_plugin_error_bytes(&error_details)
            .expect("plugin error")
            .limit(),
        Some(limit.clone())
    );
    assert_eq!(
        bookclerk_plugin_abi::decode_plugin_error_bytes(&error_bad_details)
            .expect("plugin error")
            .details,
        None
    );

    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/abi_decode");
    for (name, selector, bytes) in [
        ("limit_error_reply.bin", 1u8, &reply_err),
        ("caps_reply_ok.bin", 3u8, &caps_ok),
        ("caps_reply_limit_error.bin", 3u8, &caps_err),
        ("empty_reply_ok.bin", 4u8, &empty_ok),
        ("empty_reply_limit_error.bin", 4u8, &empty_err),
        ("plugin_error_limit_details.bin", 5u8, &error_details),
        ("plugin_error_array_details.bin", 5u8, &error_bad_details),
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

/// A standalone reply larger than the default first Cap'n segment still
/// decodes: the encoders resize to one segment instead of spilling into a
/// second one that no SDK decoder accepts.
#[test]
fn oversized_standalone_replies_stay_single_segment() {
    use bookclerk_plugin_abi::{
        decode_db_capabilities_reply_bytes, decode_execute_result_reply_bytes,
        encoded_db_capabilities_reply_bytes, encoded_execute_result_reply_bytes, PluginError,
        PluginErrorCode,
    };

    // 1024 words is the default first segment; this message alone is bigger.
    let message = "x".repeat(16 * 1024);
    for bytes in [
        encoded_db_capabilities_reply_bytes(Err(PluginError::unavailable(message.clone())))
            .expect("encode caps err"),
        encoded_execute_result_reply_bytes(Err(PluginError::unavailable(message.clone())))
            .expect("encode reply err"),
    ] {
        assert_eq!(
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            0,
            "segment count minus one"
        );
    }
    let caps = encoded_db_capabilities_reply_bytes(Err(PluginError::unavailable(message.clone())))
        .expect("encode caps err");
    let decoded = decode_db_capabilities_reply_bytes(&caps).expect_err("err member");
    assert_eq!(decoded.code, PluginErrorCode::Unavailable);
    assert_eq!(decoded.message, message);
    let reply = encoded_execute_result_reply_bytes(Err(PluginError::unavailable(message.clone())))
        .expect("encode reply err");
    let decoded = decode_execute_result_reply_bytes(&reply).expect_err("err member");
    assert_eq!(decoded.message, message);
}

#[path = "../../../fuzz/oracles/db_stream.rs"]
mod db_stream_oracle;

#[test]
fn fuzz_corpus_db_stream_holds_invariants() {
    for path in corpus_files("db_stream") {
        let data = std::fs::read(&path).expect("read");
        db_stream_oracle::check(&data);
    }
}

/// The raw byte ceiling lies past the fuzz input cap, so it is checked here
/// for every budget the oracle knows.
#[test]
fn db_stream_raw_ceiling_is_enforced_for_every_budget() {
    for selector in 0..db_stream_oracle::BUDGETS {
        db_stream_oracle::check_raw_ceiling(selector);
    }
}

/// Builds the `db_stream` seeds from the encoder and keeps them current.
///
/// Each seed is a budget byte, a chunk-size byte and a stream: a reply that
/// fits, an adapter error, a reply over the tiny row cap, a stream cut
/// before its final frame, and bytes after the final frame.
#[test]
fn db_stream_seeds_are_current() {
    use bookclerk_plugin_abi::{
        encode_execute_stream, encode_execute_stream_error, DbColumn, DbRow, DbTiming, DbType,
        DbValue, ExecuteReply, PluginError, StatementResult,
    };

    let reply = |rows: usize| ExecuteReply {
        operation_id: "op-stream".into(),
        statements: vec![
            StatementResult::from_rows(
                vec![
                    DbColumn {
                        name: "id".into(),
                        db_type: DbType::Int64,
                    },
                    DbColumn {
                        name: "title".into(),
                        db_type: DbType::Text,
                    },
                ],
                (0..rows)
                    .map(|i| DbRow {
                        values: vec![
                            DbValue::Int64(i64::try_from(i).expect("small index")),
                            DbValue::Text(format!("Title {i:03}")),
                        ],
                    })
                    .collect(),
            )
            .expect("well-formed rows"),
            StatementResult {
                rows: Vec::new(),
                columns: Vec::new(),
                rows_affected: 2,
            },
        ],
        timing: DbTiming {
            attempt_elapsed_us: 11,
            db_execution_us: 7,
            db_timing_source: "sqlite_txn".into(),
        },
    };
    let fits = encode_execute_stream(&reply(3)).expect("encode reply");
    let error = encode_execute_stream_error(&PluginError::unavailable("database is locked"))
        .expect("encode error");
    let too_many_rows = encode_execute_stream(&reply(9)).expect("encode reply");
    let truncated = fits[..fits.len() / 2].to_vec();
    let mut trailing = fits.clone();
    trailing.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);

    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/db_stream");
    for (name, budget, chunk, bytes) in [
        ("reply_fits.bin", 1u8, 255u8, &fits),
        ("reply_fits_tiny_chunks.bin", 1, 2, &fits),
        ("adapter_error.bin", 1, 16, &error),
        ("rows_over_tiny_cap.bin", 0, 64, &too_many_rows),
        ("inline_caps.bin", 2, 32, &fits),
        ("truncated.bin", 1, 8, &truncated),
        ("bytes_after_end.bin", 1, 8, &trailing),
    ] {
        let path = corpus.join(name);
        let mut seed = vec![budget, chunk];
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

#[path = "../../../fuzz/oracles/scan_page.rs"]
mod scan_page_oracle;

/// Mirrors `fuzz/fuzz_targets/scan_page.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_scan_page_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("scan_page") {
        scan_page_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no scan_page seed");
}

/// Seeds for `scan_page`: an accepted first page, a last delta page, a page
/// handing back refreshed credentials, and one page breaking each pass rule. Refresh with `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed I/O or encoding is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn scan_page_seeds_are_current() {
    use bookclerk_plugin_abi::{encode_scan_page_bytes, ScanBook, ScanPage};

    let book = |account: &str, product: &str| ScanBook {
        account_id: account.into(),
        product_id: product.into(),
        title: "Title".into(),
        work_kind: bookclerk_plugin_abi::WorkKind::Book,
        expression_kind: bookclerk_plugin_abi::ExpressionKind::Narration,
        media_kind: bookclerk_plugin_abi::MediaKind::Audio,
        ..ScanBook::default()
    };
    let seeds: Vec<(&str, &str, &str, ScanPage)> = vec![
        (
            "first_page.bin",
            "",
            "acct",
            ScanPage {
                books: vec![book("acct", "B01"), book("", "B02")],
                next_cursor: "page=2".into(),
                store_pages: 1,
                ..ScanPage::default()
            },
        ),
        (
            "last_delta_page.bin",
            "page=2",
            "acct",
            structured_scan_page(),
        ),
        (
            "repeated_cursor.bin",
            "page=2",
            "acct",
            ScanPage {
                next_cursor: "page=2".into(),
                ..ScanPage::default()
            },
        ),
        (
            "foreign_account.bin",
            "",
            "acct",
            ScanPage {
                books: vec![book("other", "B01")],
                ..ScanPage::default()
            },
        ),
        (
            "empty_ids.bin",
            "",
            "acct",
            ScanPage {
                books: vec![book("acct", " ")],
                removed: vec![String::new()],
                ..ScanPage::default()
            },
        ),
        (
            "refreshed_credentials.bin",
            "",
            "acct",
            ScanPage {
                books: vec![book("acct", "B01")],
                refreshed_credentials: Some(br#"{"authfile_b64":"e30="}"#.to_vec()),
                ..ScanPage::default()
            },
        ),
        (
            "undeclared_carrier.bin",
            "",
            "acct",
            ScanPage {
                books: vec![ScanBook {
                    media_kind: bookclerk_plugin_abi::MediaKind::Print,
                    ..book("acct", "B01")
                }],
                ..ScanPage::default()
            },
        ),
        (
            "refreshed_not_object.bin",
            "",
            "acct",
            ScanPage {
                refreshed_credentials: Some(b"[\"token\"]".to_vec()),
                ..ScanPage::default()
            },
        ),
    ];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/scan_page");
    for (name, sent, account, page) in seeds {
        let mut seed = vec![u8::try_from(sent.len()).expect("short cursor")];
        seed.extend_from_slice(sent.as_bytes());
        seed.push(u8::try_from(account.len()).expect("short account"));
        seed.extend_from_slice(account.as_bytes());
        seed.extend(encode_scan_page_bytes(&page).expect("encode"));
        let path = dir.join(name);
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

#[path = "../../../fuzz/oracles/quality_offers.rs"]
mod quality_offers_oracle;

/// Mirrors `fuzz/fuzz_targets/quality_offers.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_quality_offers_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("quality_offers") {
        quality_offers_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no quality_offers seed");
}

/// Seeds for `quality_offers`: accepted replies (an xHE-AAC offer next to a
/// title the store could not answer, a lossless offer with refreshed
/// credentials) and one reply breaking each rule. Refresh with
/// `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed I/O or encoding is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn quality_offers_seeds_are_current() {
    use bookclerk_plugin_abi::{
        encode_quality_offers_bytes, OfferedAacProfile, OfferedCodec, QualityOffer,
        QualityOffersResult, TitleQualityOffer,
    };

    let found = |offer: QualityOffer| TitleQualityOffer { found: true, offer };
    let xhe = QualityOffer {
        codec: OfferedCodec::Aac,
        aac_profile: OfferedAacProfile::Xhe,
        bitrate_kbps: Some(137),
        sample_rate_hz: Some(44_100),
        channels: Some(2),
    };
    let seeds: Vec<(&str, u8, QualityOffersResult)> = vec![
        (
            "xhe_and_unknown.bin",
            2,
            QualityOffersResult {
                offers: vec![found(xhe.clone()), TitleQualityOffer::default()],
                refreshed_credentials: None,
            },
        ),
        (
            "flac_refreshed.bin",
            1,
            QualityOffersResult {
                offers: vec![found(QualityOffer {
                    codec: OfferedCodec::Flac,
                    aac_profile: OfferedAacProfile::Unknown,
                    bitrate_kbps: Some(1_411),
                    sample_rate_hz: Some(96_000),
                    channels: Some(2),
                })],
                refreshed_credentials: Some(br#"{"authfile_b64":"e30="}"#.to_vec()),
            },
        ),
        (
            "too_many_answers.bin",
            1,
            QualityOffersResult {
                offers: vec![found(xhe.clone()), found(xhe.clone())],
                refreshed_credentials: None,
            },
        ),
        (
            "impossible_channels.bin",
            1,
            QualityOffersResult {
                offers: vec![found(QualityOffer {
                    channels: Some(33),
                    ..xhe.clone()
                })],
                refreshed_credentials: None,
            },
        ),
        (
            "profile_on_mp3.bin",
            1,
            QualityOffersResult {
                offers: vec![found(QualityOffer {
                    codec: OfferedCodec::Mp3,
                    ..xhe.clone()
                })],
                refreshed_credentials: None,
            },
        ),
        (
            "refreshed_not_object.bin",
            0,
            QualityOffersResult {
                offers: Vec::new(),
                refreshed_credentials: Some(b"[\"token\"]".to_vec()),
            },
        ),
    ];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/quality_offers");
    for (name, sent, result) in seeds {
        let mut seed = vec![sent];
        seed.extend(encode_quality_offers_bytes(&result).expect("encode"));
        let path = dir.join(name);
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

#[path = "../../../fuzz/oracles/event_read.rs"]
mod event_read_oracle;

/// Mirrors `fuzz/fuzz_targets/event_read.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_event_read_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("event_read") {
        event_read_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no event_read seed");
}

/// A `2`-prefixed Cap'n Proto `EventReadRequest` seed with `types` entries.
// Test helper: an encoding failure is the test failing.
#[allow(clippy::expect_used)]
fn capnp_request(after: &str, types: u32) -> Vec<u8> {
    use bookclerk_plugin_abi::plugin_capnp::event_read_request;
    let mut message = capnp::message::Builder::new_default();
    let mut root = message.init_root::<event_read_request::Builder<'_>>();
    root.set_after(after);
    root.set_limit(10);
    let mut list = root.init_types(types);
    for i in 0..types {
        list.set(i, "book_acquired");
    }
    let mut seed = vec![2u8];
    seed.extend(capnp::serialize::write_message_to_words(&message));
    seed
}

/// A `3`-prefixed Cap'n Proto `EventReadPage` seed with `events` entries.
// Test helper: an encoding failure is the test failing.
#[allow(clippy::expect_used)]
fn capnp_page(cursor: &str, events: u32) -> Vec<u8> {
    use bookclerk_plugin_abi::plugin_capnp::event_read_page;
    let mut message = capnp::message::Builder::new_default();
    let mut root = message.init_root::<event_read_page::Builder<'_>>();
    root.set_cursor(cursor);
    root.set_more(true);
    {
        let mut list = root.reborrow().init_events(events);
        for i in 0..events {
            let mut event = list.reborrow().get(i);
            event.set_event_id(format!("evt-{i}").as_str());
            event.set_event_type("book_acquired");
            event.set_schema_version(1);
            event.set_payload(b"{}");
        }
    }
    let mut ids = root.init_expired_event_ids(1);
    ids.set(0, "evt-0");
    let mut seed = vec![3u8];
    seed.extend(capnp::serialize::write_message_to_words(&message));
    seed
}

/// Seeds for `event_read`: accepted request bodies, cursors and Cap'n Proto
/// messages, and one input breaking each rule. Byte 0 is `0` for a JSON
/// body, `1` for a bare cursor, `2` for a Cap'n Proto request and `3` for a
/// Cap'n Proto page. Refresh with `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed I/O or encoding is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn event_read_seeds_are_current() {
    use bookclerk_plugin_abi::EventCursor;

    let cursor = EventCursor::new("2026-11-11T10:00:00.123+00:00", "evt-1")
        .expect("cursor")
        .encode();
    let body = |json: String| {
        let mut seed = vec![0u8];
        seed.extend(json.into_bytes());
        seed
    };
    let bare = |text: &str| {
        let mut seed = vec![1u8];
        seed.extend_from_slice(text.as_bytes());
        seed
    };
    let seeds: Vec<(&str, Vec<u8>)> = vec![
        ("empty_body.bin", body("{}".into())),
        (
            "after_cursor.bin",
            body(format!(
                r#"{{"after":"{cursor}","types":["book_acquired"],"limit":10}}"#
            )),
        ),
        ("since.bin", body(r#"{"sinceUnixMs":1762855200000}"#.into())),
        (
            "after_and_since.bin",
            body(format!(r#"{{"after":"{cursor}","sinceUnixMs":1}}"#)),
        ),
        (
            "unconsented_type.bin",
            body(r#"{"types":["secret_event"]}"#.into()),
        ),
        ("limit_over_page.bin", body(r#"{"limit":100000}"#.into())),
        ("unknown_field.bin", body(r#"{"cursor":"x"}"#.into())),
        ("array_body.bin", body("[]".into())),
        ("cursor.bin", bare(&cursor)),
        ("cursor_uppercase.bin", bare(&cursor.to_uppercase())),
        ("cursor_not_time.bin", bare("v1.6e6f77.6576742d31")),
        ("capnp_request.bin", capnp_request(&cursor, 1)),
        (
            "capnp_request_many_types.bin",
            capnp_request("", bookclerk_plugin_abi::MAX_LIST_PAGE + 1),
        ),
        ("capnp_page.bin", capnp_page(&cursor, 2)),
        (
            "capnp_page_long_cursor.bin",
            capnp_page(
                &"0".repeat(bookclerk_plugin_abi::MAX_EVENT_CURSOR_BYTES as usize + 1),
                0,
            ),
        ),
    ];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/event_read");
    if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
        std::fs::create_dir_all(&dir).expect("corpus dir");
    }
    for (name, seed) in seeds {
        let path = dir.join(name);
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

#[path = "../../../fuzz/oracles/plain_fetch.rs"]
mod plain_fetch_oracle;

/// Mirrors `fuzz/fuzz_targets/plain_fetch.rs` with the shared oracle.
#[test]
fn fuzz_corpus_plain_fetch_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("plain_fetch") {
        plain_fetch_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no plain_fetch seed");
}

/// Seeds for `plain_fetch`: accepted part and whole replies, and one reply
/// breaking each rule. Refresh with `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed I/O or encoding is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn plain_fetch_seeds_are_current() {
    use bookclerk_plugin_abi::{
        encode_plain_fetch_bytes, ChapterMarker, FetchedFile, FetchedFileRole, MediaKind,
        PlainFetch,
    };

    let file = |path: &str, role: FetchedFileRole| FetchedFile {
        path: path.into(),
        role,
        ..FetchedFile::default()
    };
    let audio = |files: Vec<FetchedFile>| PlainFetch {
        media_kind: MediaKind::Audio,
        files,
        ..PlainFetch::default()
    };
    let seeds: Vec<(&str, PlainFetch)> = vec![
        (
            "parts.bin",
            PlainFetch {
                chapters: vec![ChapterMarker {
                    title: "One".into(),
                    start_ms: 0,
                }],
                pdf_url: Some("https://cdn.example/book.pdf".into()),
                refreshed_credentials: Some(br#"{"token":"t"}"#.to_vec()),
                ..audio(vec![
                    FetchedFile {
                        title: Some("Part 1".into()),
                        duration_ms: Some(60_000),
                        content_type: Some("audio/mpeg".into()),
                        ..file("/c/1.mp3", FetchedFileRole::Part)
                    },
                    file("/c/2.mp3", FetchedFileRole::Part),
                    file("/c/cover.jpg", FetchedFileRole::Cover),
                ])
            },
        ),
        (
            "whole.bin",
            audio(vec![file("/c/book.m4b", FetchedFileRole::Whole)]),
        ),
        (
            "parts_and_whole.bin",
            audio(vec![
                file("/c/1.mp3", FetchedFileRole::Part),
                file("/c/book.m4b", FetchedFileRole::Whole),
            ]),
        ),
        (
            "no_role.bin",
            audio(vec![file("/c/book.m4b", FetchedFileRole::Unspecified)]),
        ),
        (
            "no_content.bin",
            audio(vec![file("/c/cover.jpg", FetchedFileRole::Cover)]),
        ),
        (
            "undeclared_carrier.bin",
            PlainFetch {
                media_kind: MediaKind::Ebook,
                ..audio(vec![file("/c/book.epub", FetchedFileRole::Whole)])
            },
        ),
    ];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/plain_fetch");
    for (name, fetch) in seeds {
        let seed = encode_plain_fetch_bytes(&fetch).expect("encode");
        let path = dir.join(name);
        if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
            std::fs::create_dir_all(&dir).expect("corpus dir");
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

#[path = "../../../fuzz/oracles/metadata_file_record.rs"]
mod metadata_file_record_oracle;

/// Mirrors `fuzz/fuzz_targets/metadata_file_record.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_metadata_file_record_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("metadata_file_record") {
        metadata_file_record_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no metadata_file_record seed");
}

/// Seeds for `metadata_file_record`: a full record inside the dialect, an
/// empty one, one that states a field the dialect does not carry, and one
/// breaking each record rule. Refresh with `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed I/O or encoding is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn metadata_file_record_seeds_are_current() {
    use bookclerk_plugin_abi::{
        encode_metadata_file_record_bytes, Abridgement, Credit, CreditRole, ExpressionKind,
        Identifier, IdentifierScheme, MetadataChapter, MetadataField, MetadataFileRecord,
        SeriesEntry,
    };

    let full = MetadataFileRecord {
        present: vec![
            MetadataField::Title,
            MetadataField::Subtitle,
            MetadataField::Description,
            MetadataField::Publisher,
            MetadataField::PublishedAt,
            MetadataField::Language,
            MetadataField::Genres,
            MetadataField::Tags,
            MetadataField::Abridgement,
            MetadataField::Identifiers,
            MetadataField::Credits,
            MetadataField::Series,
            MetadataField::Chapters,
        ],
        title: Some("The Final Empire".into()),
        subtitle: Some("Mistborn, Book 1".into()),
        description: Some("A world of ash.\n\nA heist.".into()),
        publisher: Some("Macmillan Audio".into()),
        published_at: Some("2009-02-28".into()),
        language: Some("English".into()),
        genres: vec!["Fantasy".into(), "Epic".into()],
        tags: vec!["heist".into()],
        abridgement: Abridgement::Unabridged,
        identifiers: vec![
            Identifier {
                scheme: IdentifierScheme::Asin,
                value: "B002UZMLXM".into(),
                marketplace: Some("us".into()),
            },
            Identifier {
                scheme: IdentifierScheme::Isbn,
                value: "9781427205391".into(),
                marketplace: None,
            },
        ],
        credits: vec![
            Credit {
                name: "Brandon Sanderson".into(),
                role: CreditRole::Author,
                ..Credit::default()
            },
            Credit {
                name: "Michael Kramer".into(),
                role: CreditRole::Narrator,
                ..Credit::default()
            },
        ],
        credit_roles: vec![CreditRole::Author, CreditRole::Narrator],
        series_entries: vec![SeriesEntry {
            name: "Mistborn".into(),
            position: Some("1".into()),
            store_series_id: None,
        }],
        chapters: vec![
            MetadataChapter {
                title: "Prologue".into(),
                start_ms: 0,
                depth: 0,
                ..MetadataChapter::default()
            },
            MetadataChapter {
                title: "Part One".into(),
                start_ms: 61_000,
                depth: 0,
                ..MetadataChapter::default()
            },
            MetadataChapter {
                title: "Chapter 1".into(),
                start_ms: 61_000,
                length_ms: Some(1_200_000),
                depth: 1,
            },
        ],
        ..MetadataFileRecord::default()
    };
    let seeds: Vec<(&str, MetadataFileRecord)> = vec![
        ("full.bin", full.clone()),
        ("empty.bin", MetadataFileRecord::default()),
        (
            "stated_empty.bin",
            MetadataFileRecord {
                present: vec![MetadataField::Genres, MetadataField::Credits],
                credit_roles: vec![CreditRole::Narrator],
                ..MetadataFileRecord::default()
            },
        ),
        (
            "outside_dialect.bin",
            MetadataFileRecord {
                present: vec![MetadataField::ExpressionKind, MetadataField::Duration],
                expression_kind: ExpressionKind::Dramatisation,
                duration_ms: Some(3_600_000),
                ..MetadataFileRecord::default()
            },
        ),
        (
            "value_not_present.bin",
            MetadataFileRecord {
                present: Vec::new(),
                ..full.clone()
            },
        ),
        (
            "role_not_stated.bin",
            MetadataFileRecord {
                credit_roles: vec![CreditRole::Author],
                ..full.clone()
            },
        ),
        (
            "bad_date.bin",
            MetadataFileRecord {
                published_at: Some("2023-02-29".into()),
                ..full.clone()
            },
        ),
        (
            "multiline_title.bin",
            MetadataFileRecord {
                title: Some("The\nFinal Empire".into()),
                ..full.clone()
            },
        ),
        (
            "chapters_out_of_order.bin",
            MetadataFileRecord {
                chapters: vec![
                    MetadataChapter {
                        title: "Late".into(),
                        start_ms: 10,
                        ..MetadataChapter::default()
                    },
                    MetadataChapter {
                        title: "Early".into(),
                        start_ms: 5,
                        ..MetadataChapter::default()
                    },
                ],
                ..full.clone()
            },
        ),
        (
            "duplicate_field.bin",
            MetadataFileRecord {
                present: vec![MetadataField::Title, MetadataField::Title],
                title: Some("Twice".into()),
                ..MetadataFileRecord::default()
            },
        ),
    ];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/metadata_file_record");
    for (name, record) in seeds {
        let seed = encode_metadata_file_record_bytes(&record).expect("encode");
        let path = dir.join(name);
        if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
            std::fs::create_dir_all(&dir).expect("corpus dir");
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

#[path = "../../../fuzz/oracles/socket_request.rs"]
mod socket_request_oracle;

/// Mirrors `fuzz/fuzz_targets/socket_request.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_socket_request_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("socket_request") {
        socket_request_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no socket_request seed");
}

/// A `1`-prefixed Cap'n Proto `SocketConnectRequest` seed.
// Test helper: an encoding failure is the test failing.
#[allow(clippy::expect_used)]
fn capnp_socket_connect(request: &bookclerk_plugin_abi::SocketConnectRequest) -> Vec<u8> {
    let mut message = capnp::message::Builder::new_default();
    bookclerk_plugin_abi::write_socket_connect_request(message.init_root(), request)
        .expect("encode connect request");
    let mut seed = vec![1u8];
    seed.extend(capnp::serialize::write_message_to_words(&message));
    seed
}

/// A `2`-prefixed Cap'n Proto `SocketMessage` seed.
// Test helper: an encoding failure is the test failing.
#[allow(clippy::expect_used)]
fn capnp_socket_message(event: &bookclerk_plugin_abi::SocketMessage) -> Vec<u8> {
    let mut message = capnp::message::Builder::new_default();
    bookclerk_plugin_abi::write_socket_message(message.init_root(), event)
        .expect("encode socket message");
    let mut seed = vec![2u8];
    seed.extend(capnp::serialize::write_message_to_words(&message));
    seed
}

/// Seeds for `socket_request`: accepted and refused URLs, connect requests
/// and deliveries. Byte 0 is `0` for a URL, `1` for a Cap'n Proto
/// `SocketConnectRequest` and `2` for a Cap'n Proto `SocketMessage`.
/// Refresh with `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed I/O or encoding is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn socket_request_seeds_are_current() {
    use bookclerk_plugin_abi::{
        SocketAutoResponse, SocketConnectRequest, SocketFrame, SocketHeader, SocketMessage,
    };

    let url = |text: &str| {
        let mut seed = vec![0u8];
        seed.extend_from_slice(text.as_bytes());
        seed
    };
    let abs = SocketConnectRequest {
        url: "wss://abs.example.com/socket.io/?EIO=4&transport=websocket".into(),
        protocols: vec!["chat".into()],
        headers: vec![SocketHeader {
            name: "Authorization".into(),
            value: "Bearer token".into(),
        }],
        auto_responses: vec![SocketAutoResponse {
            request: SocketFrame::Text("2".into()),
            response: SocketFrame::Text("3".into()),
        }],
        attachment: b"{\"user\":\"u1\"}".to_vec(),
        trust_anchors: Vec::new(),
    };
    let mut smuggled = abs.clone();
    smuggled.headers[0].name = "Sec-WebSocket-Key".into();
    let mut crlf = abs.clone();
    crlf.headers[0].value = "x\r\nX-Evil: 1".into();
    let mut heavy = abs.clone();
    heavy.attachment = vec![0; bookclerk_plugin_abi::MAX_SOCKET_ATTACHMENT_BYTES as usize + 1];
    let mut aliased = abs.clone();
    aliased.url = "ws://0x7f.1:13378/socket.io".into();
    let delivery = SocketMessage {
        socket: "s-1".into(),
        frame: SocketFrame::Text("42[\"item_updated\",{}]".into()),
        attachment: b"{}".to_vec(),
    };
    let mut bad_id = delivery.clone();
    bad_id.socket = "../s".into();
    let tunnel = SocketConnectRequest {
        url: "tls://connect.ngrok-agent.com:443".into(),
        protocols: vec!["h2".into(), "x-tunnel/1".into()],
        attachment: b"session".to_vec(),
        ..SocketConnectRequest::default()
    };
    let mut tunnel_headers = tunnel.clone();
    tunnel_headers.headers = abs.headers.clone();
    let mut tunnel_heartbeat = tunnel.clone();
    tunnel_heartbeat.auto_responses = abs.auto_responses.clone();
    let mut tunnel_alpn = tunnel.clone();
    tunnel_alpn.protocols = vec!["has space".into()];
    let mut anchored = tunnel.clone();
    anchored.trust_anchors = vec![vec![0x30, 0x03, 0x02, 0x01, 0x00]];
    let mut bad_anchor = tunnel.clone();
    bad_anchor.trust_anchors = vec![vec![0x30, 0x81, 0x05, 0, 0, 0, 0, 0]];
    let mut ws_anchor = abs.clone();
    ws_anchor.trust_anchors = anchored.trust_anchors.clone();
    let seeds: Vec<(&str, Vec<u8>)> = vec![
        ("url_wss.bin", url("wss://abs.example.com/socket.io/?EIO=4")),
        ("url_ws_port.bin", url("ws://10.0.0.5:13378/socket.io")),
        ("url_v6.bin", url("ws://[::1]:8080/")),
        ("url_credentials.bin", url("wss://user:pass@example.com")),
        ("url_backslash.bin", url("wss://exa\\mple.com/")),
        ("url_short_ipv4.bin", url("ws://127.1/")),
        ("url_https.bin", url("https://example.com")),
        ("url_tls.bin", url("tls://Connect.Example.com:443")),
        ("url_tls_v6.bin", url("tls://[::1]:7844")),
        ("url_tls_no_port.bin", url("tls://connect.example.com")),
        ("url_tls_path.bin", url("tls://connect.example.com:443/x")),
        ("connect_tls.bin", capnp_socket_connect(&tunnel)),
        (
            "connect_tls_headers.bin",
            capnp_socket_connect(&tunnel_headers),
        ),
        (
            "connect_tls_heartbeat.bin",
            capnp_socket_connect(&tunnel_heartbeat),
        ),
        (
            "connect_tls_bad_alpn.bin",
            capnp_socket_connect(&tunnel_alpn),
        ),
        ("connect_tls_anchor.bin", capnp_socket_connect(&anchored)),
        (
            "connect_tls_bad_anchor.bin",
            capnp_socket_connect(&bad_anchor),
        ),
        ("connect_ws_anchor.bin", capnp_socket_connect(&ws_anchor)),
        ("connect_abs.bin", capnp_socket_connect(&abs)),
        (
            "connect_smuggled_header.bin",
            capnp_socket_connect(&smuggled),
        ),
        ("connect_crlf_value.bin", capnp_socket_connect(&crlf)),
        ("connect_heavy_attachment.bin", capnp_socket_connect(&heavy)),
        ("connect_aliased_ipv4.bin", capnp_socket_connect(&aliased)),
        ("message.bin", capnp_socket_message(&delivery)),
        ("message_bad_id.bin", capnp_socket_message(&bad_id)),
    ];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/socket_request");
    if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
        std::fs::create_dir_all(&dir).expect("corpus dir");
    }
    for (name, seed) in seeds {
        let path = dir.join(name);
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

#[path = "../../../fuzz/oracles/storage_reply.rs"]
mod storage_reply_oracle;

/// Mirrors `fuzz/fuzz_targets/storage_reply.rs` with the shared oracle.
// Test code: a failed read is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_storage_reply_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("storage_reply") {
        storage_reply_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no storage_reply seed");
}

/// A `deleteMany` reply (selector `1`) whose first outcome carries an error
/// with a struct where its `code` text belongs, built by hand because no
/// encoder writes one. The second outcome succeeds.
// Test code: a failed build is the test failing.
#[allow(clippy::expect_used)]
fn delete_many_struct_code_seed() -> Vec<u8> {
    use bookclerk_plugin_abi::plugin_capnp::{delete_many_result, move_many_request, plugin_error};

    // `MoveManyRequest` keeps a struct at pointer 0, where `PluginError`
    // keeps `code`: copied in as a `PluginError`, its code is unreadable.
    let mut fake = capnp::message::Builder::new_default();
    fake.init_root::<move_many_request::Builder<'_>>()
        .init_target()
        .init_pairs(0);
    let fake_error = fake
        .get_root_as_reader::<plugin_error::Reader<'_>>()
        .expect("struct root");
    let mut msg = capnp::message::Builder::new_default();
    let mut results = msg
        .init_root::<delete_many_result::Builder<'_>>()
        .init_results(2);
    let mut first = results.reborrow().get(0);
    first.set_key("a/1.m4b");
    first.set_error(fake_error).expect("copy error");
    results.reborrow().get(1).set_key("a/2.m4b");
    let mut out = vec![1];
    out.extend(capnp::serialize::write_message_to_words(&msg));
    out
}

/// The hand-built seed's failure survives decoding: a round trip cannot
/// catch a failure read as success, because the wrong reading round-trips.
// Test code: a failed decode is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn an_unreadable_error_code_never_decodes_as_success() {
    use bookclerk_plugin_abi::{decode_storage_reply_bytes, StorageReply, StorageReplyKind};

    let seed = delete_many_struct_code_seed();
    let reply =
        decode_storage_reply_bytes(StorageReplyKind::DeleteMany, &seed[1..]).expect("decodes");
    let StorageReply::DeleteMany(results) = reply else {
        panic!("a deleteMany reply");
    };
    assert_eq!(
        results[0]
            .error
            .as_ref()
            .map(bookclerk_plugin_abi::PluginError::wire_str),
        Some("unknown")
    );
    assert!(results[1].error.is_none());
}

/// Seeds for `storage_reply`: replies each validator accepts and one
/// breaking each rule. Byte 0 is `0` for a `moveMany` page, `1` for
/// `deleteMany`, `2` for `headMany`, `3` for `changes`, `4` for a hinted
/// `list` page and `5` for `translateKeys`. Refresh with `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed I/O or encoding is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn storage_reply_seeds_are_current() {
    use bookclerk_plugin_abi::{
        encode_storage_reply_bytes, ChangesPage, KeyOutcome, ListPage, MoveManyPage, ObjectChange,
        ObjectChangeKind, ObjectInfo, ObjectMetadata, PluginError, StorageReply,
    };

    let seed = |selector: u8, reply: StorageReply| {
        let mut out = vec![selector];
        out.extend(encode_storage_reply_bytes(&reply).expect("encode seed"));
        out
    };
    let page = |keys: &[&str], next: Option<&str>| MoveManyPage {
        results: keys.iter().map(|k| KeyOutcome::ok(*k)).collect(),
        next_cursor: next.map(str::to_string),
        atomic: false,
        objects_moved: u64::try_from(keys.len()).expect("small"),
    };
    let changes = |cursor: &str, more: bool, keys: &[&str]| ChangesPage {
        changes: keys
            .iter()
            .map(|k| ObjectChange {
                key: (*k).to_string(),
                kind: ObjectChangeKind::Upserted,
                info: Some(ObjectInfo {
                    key: (*k).to_string(),
                    size: 3,
                    etag: Some("e1".into()),
                    modified_at_ms: Some(1_762_855_200_000),
                    version_id: None,
                    signature: Some("sig".into()),
                }),
            })
            .collect(),
        cursor: cursor.to_string(),
        more,
        complete: true,
        live_watch: false,
    };
    let meta = |key: &str| ObjectMetadata {
        key: key.to_string(),
        size: 3,
        etag: Some("e1".into()),
        version_id: Some("v1".into()),
        ..ObjectMetadata::default()
    };
    let seeds: Vec<(&str, Vec<u8>)> = vec![
        (
            "move_pairs.bin",
            seed(
                0,
                StorageReply::MoveMany(page(&["a/1.m4b", "a/2.m4b"], None)),
            ),
        ),
        (
            "move_prefix_page.bin",
            seed(0, StorageReply::MoveMany(page(&["a/1.m4b"], Some("c2")))),
        ),
        (
            "move_prefix_stuck_cursor.bin",
            seed(0, StorageReply::MoveMany(page(&["a/1.m4b"], Some("c1")))),
        ),
        (
            "move_outside_prefix.bin",
            seed(0, StorageReply::MoveMany(page(&["b/1.m4b"], None))),
        ),
        (
            "delete_many.bin",
            seed(
                1,
                StorageReply::DeleteMany(vec![
                    KeyOutcome::ok("a/1.m4b"),
                    KeyOutcome::failed("a/2.m4b", PluginError::conflict("busy")),
                ]),
            ),
        ),
        (
            "delete_many_reordered.bin",
            seed(
                1,
                StorageReply::DeleteMany(vec![
                    KeyOutcome::ok("a/2.m4b"),
                    KeyOutcome::ok("a/1.m4b"),
                ]),
            ),
        ),
        (
            "delete_many_struct_code.bin",
            delete_many_struct_code_seed(),
        ),
        (
            "head_many.bin",
            seed(2, StorageReply::HeadMany(vec![Some(meta("a/1.m4b")), None])),
        ),
        (
            "head_many_wrong_key.bin",
            seed(2, StorageReply::HeadMany(vec![None, Some(meta("a/9.m4b"))])),
        ),
        (
            "changes.bin",
            seed(3, StorageReply::Changes(changes("c2", true, &["a/1.m4b"]))),
        ),
        (
            "changes_stuck_cursor.bin",
            seed(3, StorageReply::Changes(changes("c1", true, &[]))),
        ),
        (
            "changes_empty_cursor.bin",
            seed(3, StorageReply::Changes(changes("", false, &[]))),
        ),
        (
            "changes_live_watch.bin",
            seed(
                3,
                StorageReply::Changes(ChangesPage {
                    live_watch: true,
                    ..changes("w1.id.3", false, &["a/2.m4b"])
                }),
            ),
        ),
        (
            "list_hinted.bin",
            seed(
                4,
                StorageReply::List(ListPage {
                    objects: vec![ObjectInfo {
                        key: "a/1.m4b".into(),
                        size: 3,
                        etag: Some("e1".into()),
                        modified_at_ms: Some(1),
                        version_id: Some("v1".into()),
                        signature: Some("sig".into()),
                    }],
                    next_cursor: None,
                    signatures_included: true,
                    modified_since_applied: true,
                }),
            ),
        ),
        (
            "translate_keys.bin",
            seed(
                5,
                StorageReply::TranslateKeys(vec!["a/1.m4b".into(), "a_/2_.m4b".into()]),
            ),
        ),
        (
            "translate_keys_escape.bin",
            seed(
                5,
                StorageReply::TranslateKeys(vec!["a/1.m4b".into(), "a\\..\\x".into()]),
            ),
        ),
        (
            "translate_keys_short.bin",
            seed(5, StorageReply::TranslateKeys(vec!["a/1.m4b".into()])),
        ),
    ];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/storage_reply");
    if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
        std::fs::create_dir_all(&dir).expect("corpus dir");
    }
    for (name, seed) in seeds {
        let path = dir.join(name);
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

#[path = "../../../fuzz/oracles/notify_address_body.rs"]
mod notify_address_body_oracle;

/// Mirrors `fuzz/fuzz_targets/notify_address_body.rs` with the shared
/// oracle; the seeds must include both accepted and refused bodies.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_notify_address_body_replays() {
    let (mut accepted, mut refused) = (0, 0);
    for path in corpus_files("notify_address_body") {
        if notify_address_body_oracle::check(&std::fs::read(&path).expect("read")) {
            accepted += 1;
        } else {
            refused += 1;
        }
    }
    assert!(accepted > 0, "no notify_address_body seed was accepted");
    assert!(refused > 0, "every notify_address_body seed was accepted");
}

#[path = "../../../fuzz/oracles/notifier_reply.rs"]
mod notifier_reply_oracle;

/// Mirrors `fuzz/fuzz_targets/notifier_reply.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_notifier_reply_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("notifier_reply") {
        notifier_reply_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no notifier_reply seed");
}

/// Writes `seed` to `fuzz/corpus/<target>/<name>` under `UPDATE_FUZZ_SEEDS=1`
/// and otherwise asserts the checked-in file matches it.
// Test code: failed I/O is the test failing.
#[allow(clippy::expect_used)]
fn assert_notify_seed(target: &str, name: &str, seed: &[u8]) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus")
        .join(target);
    let path = dir.join(name);
    if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
        std::fs::create_dir_all(&dir).expect("corpus dir");
        std::fs::write(&path, seed).expect("write seed");
    }
    assert_eq!(
        std::fs::read(&path).expect("read seed"),
        seed,
        "{} is stale; rerun with UPDATE_FUZZ_SEEDS=1",
        path.display()
    );
}

/// Seeds for `notifier_reply`: delivered replies with and without a usable
/// provider id, a rate limit with and past the hour cap, an unavailable
/// service, a permanent failure with a multi-line message, and address
/// documents that fit, repeat a key, use a bad key or a non-string value.
/// Refresh with `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed encoding is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn notifier_reply_seeds_are_current() {
    use bookclerk_plugin_abi::notifications::encode_notify_send_reply_bytes;
    use bookclerk_plugin_abi::{NotifySent, PluginError, PluginErrorCode};

    let limited = |ms: u64| {
        let mut details = serde_json::Map::new();
        details.insert("retryAfterMs".into(), serde_json::Value::from(ms));
        PluginError {
            details: Some(details),
            ..PluginError::new(PluginErrorCode::RateLimited, "slow down")
        }
    };
    let replies: Vec<(&str, Result<NotifySent, PluginError>)> = vec![
        (
            "delivered.bin",
            Ok(NotifySent {
                provider_id: Some("msg-1234".into()),
            }),
        ),
        ("delivered_no_id.bin", Ok(NotifySent { provider_id: None })),
        (
            "delivered_bad_id.bin",
            Ok(NotifySent {
                provider_id: Some("line\nbreak".into()),
            }),
        ),
        ("rate_limited.bin", Err(limited(5_000))),
        ("rate_limited_past_cap.bin", Err(limited(86_400_000))),
        (
            "unavailable.bin",
            Err(PluginError::new(
                PluginErrorCode::Unavailable,
                "502 from relay",
            )),
        ),
        (
            "failed_multiline.bin",
            Err(PluginError::new(
                PluginErrorCode::InvalidParams,
                format!("bad webhook\r\n{}", "x".repeat(600)),
            )),
        ),
    ];
    for (name, reply) in replies {
        let mut seed = vec![0];
        seed.extend(encode_notify_send_reply_bytes(&reply).expect("encode"));
        assert_notify_seed("notifier_reply", name, &seed);
    }
    for (name, text) in [
        (
            "address_ok.bin",
            r#"{"email":"ada@example.com","name":"Ada"}"#,
        ),
        (
            "address_extra_key.bin",
            r#"{"email":"ada@example.com","phone":"1"}"#,
        ),
        ("address_repeated.bin", r#"{"email":"a@x","email":"b@x"}"#),
        ("address_bad_key.bin", r#"{"Email":"ada@example.com"}"#),
        ("address_number.bin", r#"{"email":7}"#),
        ("address_blank_email.bin", r#"{"email":"  "}"#),
        ("address_array.bin", r#"["email"]"#),
    ] {
        let mut seed = vec![1];
        seed.extend_from_slice(text.as_bytes());
        assert_notify_seed("notifier_reply", name, &seed);
    }
}

#[path = "../../../fuzz/oracles/notify_raise.rs"]
mod notify_raise_oracle;

/// Mirrors `fuzz/fuzz_targets/notify_raise.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_notify_raise_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("notify_raise") {
        notify_raise_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no notify_raise seed");
}

/// Seeds for `notify_raise`: a full raise and one breaking each rule, as
/// JSON bodies and as Cap'n Proto messages. Refresh with
/// `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed encoding is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn notify_raise_seeds_are_current() {
    use bookclerk_plugin_abi::notifications::encode_notify_raise_params_bytes;
    use bookclerk_plugin_abi::{NotificationAlertAction, NotificationField, NotifyRaiseParams};

    let full = NotifyRaiseParams {
        notification_type: "sync_failed".into(),
        fields: vec![NotificationField {
            name: "Account".into(),
            value: "ada@example.com".into(),
        }],
        patron_id: Some("p_42".into()),
        link_path: Some("/books/42".into()),
        group_key: Some("sync".into()),
        alert_action: NotificationAlertAction::Trigger,
        alert_key: Some("sync:acct".into()),
    };
    let raises = [
        ("full", full.clone()),
        (
            "qualified_type",
            NotifyRaiseParams {
                notification_type: "other:sync_failed".into(),
                ..full.clone()
            },
        ),
        (
            "protocol_relative_link",
            NotifyRaiseParams {
                link_path: Some("//evil.example/x".into()),
                ..full.clone()
            },
        ),
        (
            "alert_without_key",
            NotifyRaiseParams {
                alert_key: None,
                ..full.clone()
            },
        ),
        (
            "key_without_alert",
            NotifyRaiseParams {
                alert_action: NotificationAlertAction::None,
                ..full.clone()
            },
        ),
        (
            "blank_field_name",
            NotifyRaiseParams {
                fields: vec![NotificationField {
                    name: " ".into(),
                    value: "v".into(),
                }],
                ..full.clone()
            },
        ),
        (
            "patron_with_space",
            NotifyRaiseParams {
                patron_id: Some("p 42".into()),
                ..full.clone()
            },
        ),
        // The patron-scope parse: a canonical user id, and ones that name
        // a user another way or not at all.
        (
            "patron_user_id",
            NotifyRaiseParams {
                patron_id: Some("42".into()),
                ..full.clone()
            },
        ),
        (
            "patron_signed",
            NotifyRaiseParams {
                patron_id: Some("+42".into()),
                ..full.clone()
            },
        ),
        (
            "patron_leading_zero",
            NotifyRaiseParams {
                patron_id: Some("042".into()),
                ..full.clone()
            },
        ),
        (
            "patron_past_i64",
            NotifyRaiseParams {
                patron_id: Some("9223372036854775808".into()),
                ..full.clone()
            },
        ),
    ];
    for (name, raise) in raises {
        let mut json = vec![0];
        json.extend(serde_json::to_vec(&raise).expect("json"));
        assert_notify_seed("notify_raise", &format!("{name}.json.bin"), &json);
        let mut capnp = vec![1];
        capnp.extend(encode_notify_raise_params_bytes(&raise).expect("encode"));
        assert_notify_seed("notify_raise", &format!("{name}.capnp.bin"), &capnp);
    }
}

#[path = "../../../fuzz/oracles/webhook_message.rs"]
mod webhook_message_oracle;

/// Mirrors `fuzz/fuzz_targets/webhook_message.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_webhook_message_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("webhook_message") {
        webhook_message_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no webhook_message seed");
}

/// Seeds for `webhook_message`: byte 0 is `0` for a Cap'n Proto
/// `WebhookPullRequest`, `1` for a `WebhookAckRequest` and `2` for a
/// `WebhookDelivery`. Refresh with `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed I/O or encoding is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn webhook_message_seeds_are_current() {
    use bookclerk_plugin_abi::{
        WebhookAckRequest, WebhookDelivery, WebhookHeader, WebhookPullRequest,
    };

    fn seed<F>(tag: u8, write: F) -> Vec<u8>
    where
        F: FnOnce(&mut capnp::message::Builder<capnp::message::HeapAllocator>),
    {
        let mut message = capnp::message::Builder::new_default();
        write(&mut message);
        let mut out = vec![tag];
        out.extend(capnp::serialize::write_message_to_words(&message));
        out
    }
    let pull = |request: WebhookPullRequest| {
        seed(0, |m| {
            bookclerk_plugin_abi::write_webhook_pull_request(m.init_root(), &request)
                .expect("encode pull");
        })
    };
    let ack = |request: WebhookAckRequest| {
        seed(1, |m| {
            bookclerk_plugin_abi::write_webhook_ack_request(m.init_root(), &request)
                .expect("encode ack");
        })
    };
    let delivery = |value: WebhookDelivery| {
        seed(2, |m| {
            bookclerk_plugin_abi::write_webhook_delivery(m.init_root(), &value)
                .expect("encode delivery");
        })
    };
    let good_pull = WebhookPullRequest {
        name: "events".into(),
        batch_size: 10,
        visibility_timeout_ms: 30_000,
    };
    let good_ack = WebhookAckRequest {
        name: "events".into(),
        acks: vec!["l1".into()],
        retries: vec!["l2".into()],
    };
    let good_delivery = WebhookDelivery {
        lease_id: "l1".into(),
        received_at_unix_ms: 1_700_000_000_000,
        attempt: 1,
        headers: vec![WebhookHeader {
            name: "content-type".into(),
            value: "application/json".into(),
        }],
        body: b"{}".to_vec(),
    };
    let seeds: Vec<(&str, Vec<u8>)> = vec![
        ("pull.bin", pull(good_pull.clone())),
        (
            "pull_zero_batch.bin",
            pull(WebhookPullRequest {
                batch_size: 0,
                ..good_pull.clone()
            }),
        ),
        (
            "pull_long_visibility.bin",
            pull(WebhookPullRequest {
                visibility_timeout_ms: bookclerk_plugin_abi::MAX_WEBHOOK_VISIBILITY_MS + 1,
                ..good_pull.clone()
            }),
        ),
        (
            "pull_bad_name.bin",
            pull(WebhookPullRequest {
                name: "Events".into(),
                ..good_pull
            }),
        ),
        ("ack.bin", ack(good_ack.clone())),
        (
            "ack_overlap.bin",
            ack(WebhookAckRequest {
                retries: vec!["l1".into()],
                ..good_ack.clone()
            }),
        ),
        (
            "ack_bad_lease.bin",
            ack(WebhookAckRequest {
                acks: vec!["../l".into()],
                ..good_ack
            }),
        ),
        ("delivery.bin", delivery(good_delivery.clone())),
        (
            "delivery_control_value.bin",
            delivery(WebhookDelivery {
                headers: vec![WebhookHeader {
                    name: "x-event".into(),
                    value: "a\u{1}b".into(),
                }],
                ..good_delivery
            }),
        ),
    ];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/webhook_message");
    if std::env::var_os("UPDATE_FUZZ_SEEDS").is_some() {
        std::fs::create_dir_all(&dir).expect("corpus dir");
    }

    for (name, seed) in seeds {
        let path = dir.join(name);
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

#[path = "../../../fuzz/oracles/origin_request.rs"]
mod origin_request_oracle;

/// Mirrors `fuzz/fuzz_targets/origin_request.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_origin_request_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("origin_request") {
        origin_request_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no origin_request seed");
}

/// Seeds for `origin_request`: framed bodies, request and response heads
/// and ingress origins, accepted and refused. Byte 0 is `0` for a framed
/// `POST /origin/fetch` body, `1` for request JSON, `2` for response JSON
/// and `3` for an origin. Refresh with `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed I/O or encoding is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn origin_request_seeds_are_current() {
    use bookclerk_plugin_abi::origin::encode_origin_frame;
    use bookclerk_plugin_abi::{OriginHeader, OriginRequest};

    let tagged = |tag: u8, bytes: &[u8]| {
        let mut seed = vec![tag];
        seed.extend_from_slice(bytes);
        seed
    };
    let header = |name: &str, value: &str| OriginHeader {
        name: name.into(),
        value: value.into(),
    };
    let request = OriginRequest {
        method: "POST".into(),
        target: "/api/library/works?page=2".into(),
        headers: vec![
            header("content-type", "application/json"),
            header("cookie", "session=abc"),
            header("Host", "evil.example"),
            header("x-forwarded-for", "198.51.100.1"),
        ],
        client_address: Some("203.0.113.9".into()),
    };
    let framed = |request: &OriginRequest, body: &[u8]| {
        let mut out = encode_origin_frame(request).expect("frame");
        out.extend_from_slice(body);
        tagged(0, &out)
    };
    let json = |request: &OriginRequest| tagged(1, &serde_json::to_vec(request).expect("json"));
    let mut hop = request.clone();
    hop.headers.push(header("Transfer-Encoding", "chunked"));
    let mut absolute = request.clone();
    absolute.target = "https://evil.example/".into();
    let mut scheme_relative = request.clone();
    scheme_relative.target = "//evil.example/".into();
    let mut crlf = request.clone();
    crlf.headers[1].value = "a\r\nset-cookie: x=1".into();
    let mut v6 = request.clone();
    v6.client_address = Some("2001:db8::1".into());
    let mut bad_client = request.clone();
    bad_client.client_address = Some("203.0.113.9:443".into());
    let whole = framed(&request, b"{\"a\":1}");
    let seeds: Vec<(&str, Vec<u8>)> = vec![
        ("frame_post_with_body.bin", whole.clone()),
        ("frame_partial_head.bin", whole[..12].to_vec()),
        ("frame_oversized_length.bin", tagged(0, &[0xff, 0xff, 0xff, 0xff, b'{'])),
        ("frame_hop_by_hop.bin", framed(&hop, b"")),
        ("request_json.bin", json(&request)),
        ("request_absolute_target.bin", json(&absolute)),
        ("request_scheme_relative.bin", json(&scheme_relative)),
        ("request_crlf_value.bin", json(&crlf)),
        ("request_v6_client.bin", json(&v6)),
        ("request_client_with_port.bin", json(&bad_client)),
        ("request_not_json.bin", tagged(1, b"GET / HTTP/1.1")),
        (
            "response_ok.bin",
            tagged(
                2,
                br#"{"status":200,"headers":[{"name":"set-cookie","value":"a=1"},{"name":"set-cookie","value":"b=2"}]}"#,
            ),
        ),
        ("response_bad_status.bin", tagged(2, br#"{"status":42,"headers":[]}"#)),
        (
            "response_upgrade.bin",
            tagged(2, br#"{"status":101,"headers":[{"name":"upgrade","value":"websocket"}]}"#),
        ),
        ("origin_ok.bin", tagged(3, b"https://lib.example.net")),
        ("origin_punycode.bin", tagged(3, b"https://xn--bcher-kva.example")),
        ("origin_port.bin", tagged(3, b"https://lib.example.net:8443")),
        ("origin_ipv4.bin", tagged(3, b"https://10.0.0.1")),
        ("origin_hex_top.bin", tagged(3, b"https://a.0x7f")),
        ("origin_uppercase.bin", tagged(3, b"https://Lib.Example.net")),
        ("origin_userinfo.bin", tagged(3, b"https://user@lib.example.net")),
        ("origin_http.bin", tagged(3, b"http://lib.example.net")),
    ];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/origin_request");
    std::fs::create_dir_all(&dir).expect("corpus dir");
    for (name, seed) in seeds {
        let path = dir.join(name);
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

#[path = "../../../fuzz/oracles/ai_run_reply.rs"]
mod ai_run_reply_oracle;

/// Mirrors `fuzz/fuzz_targets/ai_run_reply.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_ai_run_reply_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("ai_run_reply") {
        ai_run_reply_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no ai_run_reply seed");
}

/// Seeds for `ai_run_reply`: a reply that fits its run and ones with a
/// missing vector, a number that is not finite, another vector length,
/// another revision, a model name, and fallbacks that repeat or point past
/// the input; then a listing that passes and ones breaking each listing
/// rule. Refresh with `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed encoding is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn ai_run_reply_seeds_are_current() {
    use bookclerk_plugin_abi::ai::{
        encode_ai_models_reply_bytes, encode_ai_run_reply_bytes, pack_embedding_vectors,
    };
    use bookclerk_plugin_abi::{
        AiModel, AiModelList, AiOutput, AiRunOk, AiTask, AiTextEmbeddingsOutput, PluginError,
        PluginErrorCode,
    };

    let reply = |vectors: &[Vec<f32>], dimensions: u32| AiRunOk {
        output: AiOutput {
            text_embeddings: AiTextEmbeddingsOutput {
                dimensions,
                vectors: pack_embedding_vectors(vectors),
                revision: ai_run_reply_oracle::REVISION.into(),
                fallbacks: Vec::new(),
            },
        },
        model: None,
    };
    let two = vec![vec![0.25_f32, -0.5, 1.0], vec![0.0, 0.125, -1.0]];
    let good = reply(&two, 3);
    let with = |change: &dyn Fn(&mut AiRunOk)| {
        let mut changed = good.clone();
        change(&mut changed);
        changed
    };
    // Each case: name, texts sent, vector length listed, reply.
    let runs: Vec<(&str, u8, u16, Result<AiRunOk, PluginError>)> = vec![
        ("run_ok.bin", 2, 3, Ok(good.clone())),
        ("run_missing_vector.bin", 3, 3, Ok(good.clone())),
        ("run_no_texts.bin", 0, 3, Ok(reply(&[], 3))),
        ("run_other_length.bin", 2, 4, Ok(good.clone())),
        (
            "run_nan.bin",
            2,
            3,
            Ok(reply(&[vec![0.0, f32::NAN, 0.0], vec![0.0; 3]], 3)),
        ),
        (
            "run_infinite.bin",
            1,
            2,
            Ok(reply(&[vec![f32::INFINITY, 0.0]], 2)),
        ),
        (
            "run_other_revision.bin",
            2,
            3,
            Ok(with(&|r| {
                r.output.text_embeddings.revision = "sha256:other".into();
            })),
        ),
        (
            "run_names_model.bin",
            2,
            3,
            Ok(with(&|r| r.model = Some("@other/model".into()))),
        ),
        (
            "run_fallbacks.bin",
            2,
            3,
            Ok(with(&|r| r.output.text_embeddings.fallbacks = vec![0, 1])),
        ),
        (
            "run_fallback_repeats.bin",
            2,
            3,
            Ok(with(&|r| r.output.text_embeddings.fallbacks = vec![1, 1])),
        ),
        (
            "run_fallback_past_input.bin",
            2,
            3,
            Ok(with(&|r| r.output.text_embeddings.fallbacks = vec![2])),
        ),
        (
            "run_ragged_bytes.bin",
            2,
            3,
            Ok(with(&|r| r.output.text_embeddings.vectors.push(0))),
        ),
        (
            "run_error.bin",
            2,
            3,
            Err(PluginError::new(
                PluginErrorCode::Unavailable,
                "model not loaded",
            )),
        ),
    ];
    for (name, texts, listed, reply) in runs {
        let mut seed = vec![0, texts];
        seed.extend(listed.to_le_bytes());
        seed.extend(encode_ai_run_reply_bytes(&reply).expect("encode"));
        assert_notify_seed("ai_run_reply", name, &seed);
    }

    let model = AiModel {
        model_id: "bekko-a8m".into(),
        task: AiTask::TextEmbeddings,
        revision: "sha256:0123abcd".into(),
        dimensions: Some(384),
        max_tokens: Some(512),
        languages: vec!["en".into(), "de".into(), "pt-BR".into()],
    };
    let listing = |change: &dyn Fn(&mut AiModel)| {
        let mut changed = model.clone();
        change(&mut changed);
        AiModelList {
            models: vec![changed],
        }
    };
    let listings: Vec<(&str, AiModelList)> = vec![
        ("models_ok.bin", listing(&|_| {})),
        ("models_empty.bin", AiModelList { models: Vec::new() }),
        (
            "models_twice.bin",
            AiModelList {
                models: vec![model.clone(), model.clone()],
            },
        ),
        (
            "models_bad_id.bin",
            listing(&|m| m.model_id = "Bekko/a8m".into()),
        ),
        (
            "models_revision_space.bin",
            listing(&|m| m.revision = "rev 1".into()),
        ),
        ("models_no_length.bin", listing(&|m| m.dimensions = None)),
        (
            "models_long_vectors.bin",
            listing(&|m| m.dimensions = Some(4_097)),
        ),
        ("models_no_tokens.bin", listing(&|m| m.max_tokens = Some(0))),
        (
            "models_language_twice.bin",
            listing(&|m| m.languages = vec!["en".into(), "EN".into()]),
        ),
        (
            "models_bad_language.bin",
            listing(&|m| m.languages = vec!["english language".into()]),
        ),
    ];
    for (name, list) in listings {
        let mut seed = vec![1];
        seed.extend(encode_ai_models_reply_bytes(&Ok(list)).expect("encode"));
        assert_notify_seed("ai_run_reply", name, &seed);
    }
}

#[path = "../../../fuzz/oracles/ai_run.rs"]
mod ai_run_oracle;

/// Mirrors `fuzz/fuzz_targets/ai_run.rs` with the shared oracle.
// Test code: a missing corpus file is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn fuzz_corpus_ai_run_holds_invariants() {
    let mut checked = 0;
    for path in corpus_files("ai_run") {
        ai_run_oracle::check(&std::fs::read(&path).expect("read"));
        checked += 1;
    }
    assert!(checked > 0, "no ai_run seed");
}

/// Seeds for `ai_run`: a run by model name, one by task alias, and ones
/// breaking each rule, as JSON bodies and as Cap'n Proto messages. Refresh
/// with `UPDATE_FUZZ_SEEDS=1`.
// Test code: failed encoding is the test failing.
#[allow(clippy::expect_used)]
#[test]
fn ai_run_seeds_are_current() {
    use bookclerk_plugin_abi::ai::encode_ai_run_params_bytes;
    use bookclerk_plugin_abi::{
        AiEmbedText, AiInput, AiRunParams, AiTextEmbeddingsInput, AiTextRole,
    };

    let run = |model: &str, texts: Vec<AiEmbedText>| AiRunParams {
        model: model.into(),
        input: AiInput {
            text_embeddings: AiTextEmbeddingsInput { texts },
        },
    };
    let text = |text: &str, role| AiEmbedText {
        text: text.into(),
        role,
    };
    let pair = || {
        vec![
            text("A detective in Victorian London", AiTextRole::Document),
            text("cosy mysteries", AiTextRole::Query),
        ]
    };
    let runs = [
        ("named", run("@embedder/bekko-a8m", pair())),
        ("alias", run("@library/text-embeddings", pair())),
        ("unknown_alias", run("@library/transcription", pair())),
        ("bare_name", run("bekko-a8m", pair())),
        ("bad_plugin", run("@Embedder/bekko-a8m", pair())),
        ("nested_model", run("@embedder/a/b", pair())),
        ("no_texts", run("@embedder/bekko-a8m", Vec::new())),
        (
            "empty_text",
            run("@embedder/bekko-a8m", vec![text("", AiTextRole::Document)]),
        ),
        (
            "too_many_texts",
            run(
                "@embedder/bekko-a8m",
                (0..65).map(|_| text("t", AiTextRole::Document)).collect(),
            ),
        ),
    ];
    for (name, params) in runs {
        let mut json = vec![0];
        json.extend(serde_json::to_vec(&params).expect("json"));
        assert_notify_seed("ai_run", &format!("{name}.json.bin"), &json);
        let mut capnp = vec![1];
        capnp.extend(encode_ai_run_params_bytes(&params).expect("encode"));
        assert_notify_seed("ai_run", &format!("{name}.capnp.bin"), &capnp);
    }
}

#[path = "../../../fuzz/oracles/provision_reply.rs"]
mod provision_reply_oracle;

#[test]
fn fuzz_corpus_provision_reply_holds_the_oracle() {
    for path in corpus_files("provision_reply") {
        let data = std::fs::read(&path).expect("read");
        provision_reply_oracle::check(&data);
    }
}

/// The host's verdict on one wire-form `provision_reply` seed: `Some(true)`
/// accepted, `Some(false)` refused by the checker, `None` a plugin error.
fn provision_seed_verdict(name: &str) -> Option<bool> {
    use bookclerk_plugin_abi::provision::{
        check_provisioned, decode_provision_reply_bytes, ProvisionOutputs,
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/provision_reply")
        .join(name);
    let data = std::fs::read(&path).expect("read seed");
    let (&selector, rest) = data.split_first().expect("non-empty seed");
    let outputs = ProvisionOutputs {
        settings: ["account_id", "bucket", "jurisdiction"]
            .into_iter()
            .map(String::from)
            .collect(),
        secrets: ["access_key_id", "secret_access_key"]
            .into_iter()
            .map(String::from)
            .collect(),
    };
    let grant = (selector & 2 != 0).then_some("grant-token-xyz");
    let provisioned = decode_provision_reply_bytes(rest).expect("decodes").ok()?;
    Some(check_provisioned(&provisioned, &outputs, grant).is_ok())
}

#[test]
fn provision_reply_seeds_mean_what_they_say() {
    assert_eq!(provision_seed_verdict("wire_ok"), Some(true));
    assert_eq!(provision_seed_verdict("wire_ok_with_grant"), Some(true));
    assert_eq!(provision_seed_verdict("wire_refused"), None);
    assert_eq!(provision_seed_verdict("wire_grant_in_summary"), Some(false));
    assert_eq!(
        provision_seed_verdict("wire_undeclared_setting"),
        Some(false)
    );
}
