//! Branches of branches: what a branch shares with the library it joined
//! (`catalog.*` frames).
//!
//! A branch is a library of its own, so the result of joins is a tree. Each
//! link knows only its direct neighbour: a branch sends the library it
//! joined the titles in its own library (and, unless its operator turned
//! that off, the titles its own branches shared with it) in fenced
//! [`crate::Body::CatalogSync`] batches. A title carries its store, product
//! id, bibliographic metadata and file manifest, and nothing else: no store
//! account id, no token, no user identity and no downstream library or node
//! id. [`CatalogTitle`] and [`CatalogMetadata`] refuse unknown fields, so a
//! frame that tries to carry one does not parse.
//!
//! 1. branch → library: [`crate::Body::CatalogBegin`] with the link's two
//!    settings. The library records them on the branch's node row, bumps
//!    the branch's catalog fence (any frame under an older one is lost),
//!    and answers [`crate::Body::CatalogSession`]: the fence, the cursor
//!    to resume after, this library's ancestry, its destination ids, and
//!    titles whose files it still wants.
//! 2. branch → library: [`crate::Body::CatalogSync`] batches of titles
//!    after the cursor, each naming the cursor after it; answered with
//!    [`crate::Body::CatalogAccepted`] and the titles whose files it wants.
//! 3. branch → library: [`crate::Body::CatalogUpload`] for one wanted
//!    title; answered with [`crate::Body::UploadGranted`] (the token sealed
//!    for job [`CATALOG_ITEM`]), then the files go over the upload endpoint
//!    exactly as a host acquire's do.

use serde::{Deserialize, Serialize};

use crate::frames::{is_storage_key, MAX_UPLOAD_FILES, MAX_UPLOAD_FILE_BYTES};

/// Item kind of the catalog fence (`lost.item`), and the job id a catalog
/// upload's token is sealed for.
pub const CATALOG_ITEM: &str = "catalog";

/// Most titles one [`crate::Body::CatalogSync`] batch may carry.
pub const MAX_CATALOG_BATCH_TITLES: usize = 100;

/// Largest serialized title in a batch.
pub const MAX_CATALOG_TITLE_BYTES: usize = 64 * 1024;

/// Most titles one answer may say are wanted.
pub const MAX_CATALOG_WANTED: usize = 200;

/// Most cluster ids an ancestry may list.
pub const MAX_ANCESTRY: usize = 64;

/// Longest cursor.
pub const MAX_CATALOG_CURSOR_BYTES: usize = 512;

/// Longest store id.
const MAX_STORE_BYTES: usize = 64;

/// Longest product id.
const MAX_PRODUCT_ID_BYTES: usize = 256;

/// Longest metadata text field.
const MAX_TEXT_BYTES: usize = 2048;

/// One title, as a branch shares it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogTitle {
    /// The store that sells it, as the store's plain id (`audible`,
    /// `libro`, ...).
    pub store: String,
    /// The store's product id (ASIN, ISBN, ...).
    pub product_id: String,
    /// Bibliographic metadata.
    pub metadata: CatalogMetadata,
    /// Storage key of the primary audio file (one of `files`).
    pub primary_key: String,
    /// Storage key of the companion PDF (one of `files`), when shared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf_key: Option<String>,
    /// Every file of the title.
    pub files: Vec<CatalogFile>,
}

/// Bibliographic metadata of a [`CatalogTitle`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogMetadata {
    /// Display title.
    pub title: String,
    /// Subtitle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Authors as the store lists them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authors: Option<String>,
    /// Narrators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrators: Option<String>,
    /// Series name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    /// Position in the series.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_index: Option<String>,
    /// Amazon ASIN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asin: Option<String>,
    /// ISBN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isbn: Option<String>,
    /// Publisher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// Runtime in minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length_minutes: Option<i64>,
    /// `book`, `episode`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_kind: Option<String>,
    /// Language code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Publication date text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
}

/// One file of a [`CatalogTitle`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogFile {
    /// Relative storage key (the same on every destination of the
    /// receiving library).
    pub key: String,
    /// Size in bytes.
    pub size: u64,
    /// MIME type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

/// A title named by store and product id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRef {
    /// Store id.
    pub store: String,
    /// Product id.
    pub product_id: String,
}

/// True for a store id: a short lowercase token.
#[must_use]
pub fn is_store_id(store: &str) -> bool {
    !store.is_empty()
        && store.len() <= MAX_STORE_BYTES
        && store
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.'))
}

/// True for a product id: 1 to 256 bytes, no whitespace at the ends and no
/// control characters.
#[must_use]
pub fn is_product_id(product_id: &str) -> bool {
    !product_id.is_empty()
        && product_id.len() <= MAX_PRODUCT_ID_BYTES
        && product_id.trim() == product_id
        && !product_id.chars().any(char::is_control)
}

/// True for a plain cluster id in an ancestry.
#[must_use]
pub fn is_cluster_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// True when an optional text field is bounded and has no control
/// characters other than newlines.
fn text_ok(text: Option<&str>) -> bool {
    text.is_none_or(|text| {
        text.len() <= MAX_TEXT_BYTES && !text.chars().any(|c| c.is_control() && c != '\n')
    })
}

impl CatalogTitle {
    /// The title's [`CatalogRef`].
    #[must_use]
    pub fn reference(&self) -> CatalogRef {
        CatalogRef {
            store: self.store.clone(),
            product_id: self.product_id.clone(),
        }
    }

    /// Why the title is malformed, or `None`: the store and product id, a
    /// non-empty bounded title and bounded metadata, between one and
    /// [`MAX_UPLOAD_FILES`] files with distinct storage keys within
    /// [`MAX_UPLOAD_FILE_BYTES`], and the primary key (and PDF key) among
    /// them.
    #[must_use]
    pub fn problem(&self) -> Option<&'static str> {
        if !is_store_id(&self.store) {
            return Some("invalid_store");
        }
        if !is_product_id(&self.product_id) {
            return Some("invalid_product_id");
        }
        let meta = &self.metadata;
        if meta.title.trim().is_empty() || !text_ok(Some(&meta.title)) {
            return Some("invalid_title");
        }
        let texts = [
            meta.subtitle.as_deref(),
            meta.authors.as_deref(),
            meta.narrators.as_deref(),
            meta.series.as_deref(),
            meta.series_index.as_deref(),
            meta.asin.as_deref(),
            meta.isbn.as_deref(),
            meta.publisher.as_deref(),
            meta.content_kind.as_deref(),
            meta.language.as_deref(),
            meta.published_at.as_deref(),
        ];
        if !texts.into_iter().all(text_ok) {
            return Some("invalid_metadata");
        }
        if self.files.is_empty() || self.files.len() > MAX_UPLOAD_FILES {
            return Some("invalid_manifest");
        }
        let mut keys = std::collections::BTreeSet::new();
        for file in &self.files {
            let type_ok = file
                .content_type
                .as_deref()
                .is_none_or(|t| t.len() <= 128 && !t.chars().any(char::is_control));
            if !is_storage_key(&file.key)
                || file.size > MAX_UPLOAD_FILE_BYTES
                || !type_ok
                || !keys.insert(file.key.as_str())
            {
                return Some("invalid_manifest");
            }
        }
        if !keys.contains(self.primary_key.as_str())
            || self
                .pdf_key
                .as_deref()
                .is_some_and(|key| !keys.contains(key))
        {
            return Some("invalid_manifest");
        }
        None
    }

    /// Lowercase hex SHA-256 of the title's manifest (primary key, PDF key
    /// and files, sorted by key): equal digests mean the same files.
    #[must_use]
    pub fn manifest_digest(&self) -> String {
        use sha2::Digest as _;
        let mut files = self.files.clone();
        files.sort();
        let canonical = serde_json::json!({
            "primary": self.primary_key,
            "pdf": self.pdf_key,
            "files": files,
        });
        sha2::Sha256::digest(canonical.to_string().as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// True when every id in `ancestry` is a plain cluster id and there are at
/// most [`MAX_ANCESTRY`].
#[must_use]
pub fn is_ancestry(ancestry: &[String]) -> bool {
    ancestry.len() <= MAX_ANCESTRY && ancestry.iter().all(|id| is_cluster_id(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn title() -> CatalogTitle {
        CatalogTitle {
            store: "audible".into(),
            product_id: "B0TEST".into(),
            metadata: CatalogMetadata {
                title: "A Title".into(),
                authors: Some("An Author".into()),
                ..CatalogMetadata::default()
            },
            primary_key: "An Author/A Title/A Title.m4b".into(),
            pdf_key: None,
            files: vec![CatalogFile {
                key: "An Author/A Title/A Title.m4b".into(),
                size: 10,
                content_type: Some("audio/mp4".into()),
            }],
        }
    }

    #[test]
    fn a_title_carries_no_account_or_user_field() {
        let text = serde_json::to_string(&title()).unwrap();
        for forbidden in ["account", "user", "token", "node", "cluster", "owner"] {
            assert!(!text.contains(forbidden), "{forbidden} in {text}");
        }
        // A frame that tries to add one does not parse.
        let mut value = serde_json::to_value(title()).unwrap();
        value["account_id"] = serde_json::json!("acct");
        assert!(serde_json::from_value::<CatalogTitle>(value).is_err());
        let mut value = serde_json::to_value(title()).unwrap();
        value["metadata"]["user_id"] = serde_json::json!(7);
        assert!(serde_json::from_value::<CatalogTitle>(value).is_err());
    }

    #[test]
    fn titles_are_validated() {
        assert_eq!(title().problem(), None);
        let mut bad = title();
        bad.store = "Audible".into();
        assert_eq!(bad.problem(), Some("invalid_store"));
        let mut bad = title();
        bad.product_id = " x".into();
        assert_eq!(bad.problem(), Some("invalid_product_id"));
        let mut bad = title();
        bad.metadata.title = " ".into();
        assert_eq!(bad.problem(), Some("invalid_title"));
        let mut bad = title();
        bad.primary_key = "other.m4b".into();
        assert_eq!(bad.problem(), Some("invalid_manifest"));
        let mut bad = title();
        bad.files[0].key = "../x.m4b".into();
        bad.primary_key = "../x.m4b".into();
        assert_eq!(bad.problem(), Some("invalid_manifest"));
        let mut bad = title();
        bad.files.push(bad.files[0].clone());
        assert_eq!(bad.problem(), Some("invalid_manifest"));
        let mut bad = title();
        bad.pdf_key = Some("missing.pdf".into());
        assert_eq!(bad.problem(), Some("invalid_manifest"));
    }

    #[test]
    fn the_manifest_digest_follows_the_files_only() {
        let a = title();
        let mut b = title();
        b.metadata.title = "Renamed".into();
        assert_eq!(a.manifest_digest(), b.manifest_digest());
        b.files[0].size = 11;
        assert_ne!(a.manifest_digest(), b.manifest_digest());
        assert_eq!(a.manifest_digest().len(), 64);
    }

    #[test]
    fn ancestries_hold_plain_ids() {
        assert!(is_ancestry(&["a".into(), "b-1".into()]));
        assert!(!is_ancestry(&["a b".into()]));
        assert!(!is_ancestry(&vec!["a".to_string(); MAX_ANCESTRY + 1]));
    }
}
