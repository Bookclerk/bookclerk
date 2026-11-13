//! Validation of one `ContentSource.scan` page before the host writes it.
//!
//! A scan page is bytes a jailed storefront guest produced, so the host
//! checks it here, sans-IO, against the bounds `schema/plugin.capnp`
//! declares and against the pass that asked for it, before any title
//! reaches the library. The `scan_page` fuzz target drives this function
//! (`fuzz/oracles/scan_page.rs`) with the same code the host runs.

use crate::limits::{MAX_SCAN_CURSOR_BYTES, MAX_SCAN_PAGE_BOOKS};
use crate::media::{check_record_kinds, MediaError};
use crate::{MediaSupport, PluginError, ScanPage};

/// Why the host refused a scan page; nothing from a refused page is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanPageError {
    /// The page carried more titles than `maxScanPageBooks`.
    TooManyBooks {
        /// Titles the page carried.
        count: usize,
    },
    /// The page carried more removed product ids than `maxScanPageBooks`.
    TooManyRemoved {
        /// Removed ids the page carried.
        count: usize,
    },
    /// `nextCursor` was longer than `maxScanCursorBytes`.
    CursorTooLong {
        /// Length of the cursor in bytes.
        bytes: usize,
    },
    /// `syncToken` was longer than `maxScanCursorBytes`.
    SyncTokenTooLong {
        /// Length of the token in bytes.
        bytes: usize,
    },
    /// `nextCursor` repeated the cursor the page answered, so the pass would
    /// never end.
    CursorRepeated,
    /// A title named an account other than the one the pass scans.
    ForeignAccount {
        /// Index of the offending title in `books`.
        index: usize,
    },
    /// A title had an empty product id.
    EmptyProductId {
        /// Index of the offending title in `books`.
        index: usize,
    },
    /// A removed product id was empty.
    EmptyRemovedId {
        /// Index of the offending id in `removed`.
        index: usize,
    },
    /// A title left a kind `unspecified` or named a work kind or carrier
    /// outside the plugin's `MediaSupport` declaration.
    Media {
        /// Index of the offending title in `books`.
        index: usize,
        /// The rule the title broke.
        error: MediaError,
    },
}

impl std::fmt::Display for ScanPageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyBooks { count } => write!(
                f,
                "scan page carries {count} titles, more than {MAX_SCAN_PAGE_BOOKS}"
            ),
            Self::TooManyRemoved { count } => write!(
                f,
                "scan page carries {count} removed ids, more than {MAX_SCAN_PAGE_BOOKS}"
            ),
            Self::CursorTooLong { bytes } => write!(
                f,
                "scan cursor is {bytes} bytes, more than {MAX_SCAN_CURSOR_BYTES}"
            ),
            Self::SyncTokenTooLong { bytes } => write!(
                f,
                "scan sync token is {bytes} bytes, more than {MAX_SCAN_CURSOR_BYTES}"
            ),
            Self::CursorRepeated => f.write_str("scan page repeated the cursor it answered"),
            Self::ForeignAccount { index } => write!(
                f,
                "scan page title {index} names an account outside this pass"
            ),
            Self::EmptyProductId { index } => {
                write!(f, "scan page title {index} has an empty product id")
            }
            Self::EmptyRemovedId { index } => {
                write!(f, "scan page removed id {index} is empty")
            }
            Self::Media { index, error } => write!(f, "scan page title {index}: {error}"),
        }
    }
}

impl std::error::Error for ScanPageError {}

impl From<ScanPageError> for PluginError {
    fn from(err: ScanPageError) -> Self {
        Self::invalid_params(err.to_string())
    }
}

/// Checks one scan page against the ABI bounds and the pass that asked for it.
///
/// A title whose `account_id` is empty belongs to the pass's account (the
/// host fills it in); any other value must equal `account_id`. Every title
/// must name a work kind, expression kind and carrier, and its work kind and
/// carrier must be inside the plugin's declared `media`.
///
/// # Arguments
///
/// * `page` - The page the guest answered (untrusted).
/// * `sent_cursor` - The cursor the host sent for this page; empty on the
///   first page of a pass.
/// * `account_id` - The account the pass scans.
/// * `media` - The storefront's `MediaSupport` declaration.
///
/// # Errors
///
/// Returns the first [`ScanPageError`] the page breaks, checking the counts
/// and lengths before any per-title field.
pub fn validate_scan_page(
    page: &ScanPage,
    sent_cursor: &str,
    account_id: &str,
    media: &MediaSupport,
) -> Result<(), ScanPageError> {
    let cap = usize::try_from(MAX_SCAN_PAGE_BOOKS).unwrap_or(usize::MAX);
    let cursor_cap = usize::try_from(MAX_SCAN_CURSOR_BYTES).unwrap_or(usize::MAX);
    if page.books.len() > cap {
        return Err(ScanPageError::TooManyBooks {
            count: page.books.len(),
        });
    }
    if page.removed.len() > cap {
        return Err(ScanPageError::TooManyRemoved {
            count: page.removed.len(),
        });
    }
    if page.next_cursor.len() > cursor_cap {
        return Err(ScanPageError::CursorTooLong {
            bytes: page.next_cursor.len(),
        });
    }
    if page.sync_token.len() > cursor_cap {
        return Err(ScanPageError::SyncTokenTooLong {
            bytes: page.sync_token.len(),
        });
    }
    if !page.next_cursor.is_empty() && page.next_cursor == sent_cursor {
        return Err(ScanPageError::CursorRepeated);
    }
    for (index, book) in page.books.iter().enumerate() {
        if !book.account_id.is_empty() && book.account_id != account_id {
            return Err(ScanPageError::ForeignAccount { index });
        }
        if book.product_id.trim().is_empty() {
            return Err(ScanPageError::EmptyProductId { index });
        }
        check_record_kinds(media, book.work_kind, book.expression_kind, book.media_kind)
            .map_err(|error| ScanPageError::Media { index, error })?;
    }
    for (index, id) in page.removed.iter().enumerate() {
        if id.trim().is_empty() {
            return Err(ScanPageError::EmptyRemovedId { index });
        }
    }
    Ok(())
}

/// Decodes an unpacked single-segment Cap'n Proto message whose root is a
/// `ScanPage`, with a traversal budget derived from the input size.
///
/// The host decodes replies through the RPC layer; this byte entry point
/// serves the `scan_page` fuzz target and its corpus replay.
///
/// # Arguments
///
/// * `bytes` - The message bytes (untrusted).
///
/// # Returns
///
/// The decoded page.
///
/// # Errors
///
/// Returns the Cap'n Proto error the framing or the codec reports.
pub fn decode_scan_page_bytes(bytes: &[u8]) -> capnp::Result<ScanPage> {
    let words = (bytes.len() / 8).saturating_add(16);
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(words));
    opts.nesting_limit(32);
    let reader = capnp::serialize::read_message(&mut std::io::Cursor::new(bytes), opts)?;
    crate::read_scan_page(reader.get_root::<crate::plugin_capnp::scan_page::Reader<'_>>()?)
}

/// Encodes `page` as an unpacked single-segment Cap'n Proto message, the
/// inverse of [`decode_scan_page_bytes`].
///
/// # Arguments
///
/// * `page` - The page to encode.
///
/// # Returns
///
/// The message bytes.
///
/// # Errors
///
/// Returns the Cap'n Proto error the builder reports.
pub fn encode_scan_page_bytes(page: &ScanPage) -> capnp::Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    crate::write_scan_page(
        message.init_root::<crate::plugin_capnp::scan_page::Builder<'_>>(),
        page,
    )?;
    Ok(capnp::serialize::write_message_to_words(&message))
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::{ExpressionKind, MediaKind, ScanBook, WorkKind};

    fn book(account: &str, product: &str) -> ScanBook {
        ScanBook {
            account_id: account.into(),
            product_id: product.into(),
            title: "T".into(),
            work_kind: WorkKind::Book,
            expression_kind: ExpressionKind::Narration,
            media_kind: MediaKind::Audio,
            ..ScanBook::default()
        }
    }

    fn audio_books() -> MediaSupport {
        MediaSupport {
            work_kinds: vec![WorkKind::Book],
            lists: vec![MediaKind::Audio],
            fetches: vec![MediaKind::Audio],
        }
    }

    fn cap() -> usize {
        usize::try_from(MAX_SCAN_PAGE_BOOKS).unwrap_or(usize::MAX)
    }

    fn cursor_cap() -> usize {
        usize::try_from(MAX_SCAN_CURSOR_BYTES).unwrap_or(usize::MAX)
    }

    #[test]
    fn accepts_a_page_at_every_cap() {
        let page = ScanPage {
            books: (0..cap()).map(|i| book("a", &i.to_string())).collect(),
            removed: (0..cap()).map(|i| i.to_string()).collect(),
            next_cursor: "c".repeat(cursor_cap()),
            sync_token: "t".repeat(cursor_cap()),
            ..ScanPage::default()
        };
        assert_eq!(validate_scan_page(&page, "", "a", &audio_books()), Ok(()));
    }

    #[test]
    fn refuses_a_page_one_past_each_cap() {
        let over = |f: &dyn Fn(&mut ScanPage)| {
            let mut page = ScanPage::default();
            f(&mut page);
            validate_scan_page(&page, "", "a", &audio_books())
        };
        assert_eq!(
            over(&|p| p.books = (0..=cap()).map(|i| book("a", &i.to_string())).collect()),
            Err(ScanPageError::TooManyBooks { count: cap() + 1 })
        );
        assert_eq!(
            over(&|p| p.removed = (0..=cap()).map(|i| i.to_string()).collect()),
            Err(ScanPageError::TooManyRemoved { count: cap() + 1 })
        );
        assert_eq!(
            over(&|p| p.next_cursor = "c".repeat(cursor_cap() + 1)),
            Err(ScanPageError::CursorTooLong {
                bytes: cursor_cap() + 1
            })
        );
        assert_eq!(
            over(&|p| p.sync_token = "t".repeat(cursor_cap() + 1)),
            Err(ScanPageError::SyncTokenTooLong {
                bytes: cursor_cap() + 1
            })
        );
    }

    #[test]
    fn refuses_a_cursor_that_does_not_move() {
        let page = ScanPage {
            next_cursor: "p2".into(),
            ..ScanPage::default()
        };
        assert_eq!(
            validate_scan_page(&page, "p2", "a", &audio_books()),
            Err(ScanPageError::CursorRepeated)
        );
        assert_eq!(validate_scan_page(&page, "p1", "a", &audio_books()), Ok(()));
        // An empty cursor ends the pass whatever was sent.
        assert_eq!(validate_scan_page(&ScanPage::default(), "", "a", &audio_books()), Ok(()));
    }

    #[test]
    fn refuses_titles_of_another_account_or_without_a_product() {
        let page = ScanPage {
            books: vec![book("", "1"), book("b", "2")],
            ..ScanPage::default()
        };
        assert_eq!(
            validate_scan_page(&page, "", "a", &audio_books()),
            Err(ScanPageError::ForeignAccount { index: 1 })
        );
        let page = ScanPage {
            books: vec![book("a", " ")],
            ..ScanPage::default()
        };
        assert_eq!(
            validate_scan_page(&page, "", "a", &audio_books()),
            Err(ScanPageError::EmptyProductId { index: 0 })
        );
        let page = ScanPage {
            removed: vec!["x".into(), String::new()],
            ..ScanPage::default()
        };
        assert_eq!(
            validate_scan_page(&page, "", "a", &audio_books()),
            Err(ScanPageError::EmptyRemovedId { index: 1 })
        );
    }

    #[test]
    fn refuses_titles_outside_the_declared_media() {
        let mut ebook = book("a", "1");
        ebook.media_kind = MediaKind::Ebook;
        let page = ScanPage {
            books: vec![book("a", "0"), ebook],
            ..ScanPage::default()
        };
        assert_eq!(
            validate_scan_page(&page, "", "a", &audio_books()),
            Err(ScanPageError::Media {
                index: 1,
                error: MediaError::MediaKindNotDeclared(MediaKind::Ebook)
            })
        );
        let page = ScanPage {
            books: vec![book("a", "1")],
            ..ScanPage::default()
        };
        let mut unknown = page.clone();
        unknown.books[0].work_kind = WorkKind::Unspecified;
        assert_eq!(
            validate_scan_page(&unknown, "", "a", &audio_books()),
            Err(ScanPageError::Media {
                index: 0,
                error: MediaError::UnspecifiedKind("workKind")
            })
        );
        let mut podcast = page;
        podcast.books[0].work_kind = WorkKind::Podcast;
        assert_eq!(
            validate_scan_page(&podcast, "", "a", &audio_books()),
            Err(ScanPageError::Media {
                index: 0,
                error: MediaError::WorkKindNotDeclared(WorkKind::Podcast)
            })
        );
    }

    #[test]
    fn bytes_round_trip_and_garbage_is_an_error() {
        let page = ScanPage {
            books: vec![book("a", "1")],
            removed: vec!["2".into()],
            next_cursor: "n".into(),
            sync_token: "s".into(),
            complete: true,
            store_pages: 3,
        };
        let bytes = encode_scan_page_bytes(&page).expect("encode");
        assert_eq!(decode_scan_page_bytes(&bytes).expect("decode"), page);
        assert!(decode_scan_page_bytes(&[1, 2, 3]).is_err());
    }

    #[test]
    fn errors_render_and_convert_to_invalid_params() {
        let all = [
            ScanPageError::TooManyBooks { count: 1 },
            ScanPageError::TooManyRemoved { count: 1 },
            ScanPageError::CursorTooLong { bytes: 1 },
            ScanPageError::SyncTokenTooLong { bytes: 1 },
            ScanPageError::CursorRepeated,
            ScanPageError::ForeignAccount { index: 0 },
            ScanPageError::EmptyProductId { index: 0 },
            ScanPageError::EmptyRemovedId { index: 0 },
        ];
        for err in all {
            let text = err.to_string();
            assert!(!text.is_empty());
            let plugin: PluginError = err.into();
            assert_eq!(plugin.code, crate::PluginErrorCode::InvalidParams);
            assert_eq!(plugin.message, text);
        }
    }
}
