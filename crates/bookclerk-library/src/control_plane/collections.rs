//! Collections: named parts of the library, each defined by storage key
//! prefixes (see `docs/adr/patron-scopes.md`).
//!
//! The library's collections live in the `core.collections` library
//! document. A book belongs to a collection when one of its editions has a
//! file whose storage key starts with one of the collection's prefixes, so
//! a book held only through a store account, with no file yet, belongs to
//! none. The page index refresh writes each book's collections as facet
//! `collection`, which the Library filters by and a patron scope matches.
//! Changing the collections drops the index's finished build, so the
//! heartbeat rebuilds every row with the new prefixes.

use serde::{Deserialize, Serialize};

use super::documents::{
    self, import_if_absent, load_document, replace_document, DocumentKey, ReplaceOutcome,
    StoredDocument,
};
use super::{document, ConfigActor, ConfigurationDocument};
use crate::error::{LibraryError, Result};
use crate::store::library_index::facet_key;
use crate::store::LibraryStore;

/// Namespace of the library document that holds the library's collections.
pub const COLLECTIONS_NAMESPACE: &str = "core.collections";

/// Schema version of [`LibraryCollections`] in the document.
pub const COLLECTIONS_SCHEMA_VERSION: i64 = 1;

/// Most collections a library may define.
pub const MAX_COLLECTIONS: usize = 32;

/// Most prefixes one collection may have.
pub const MAX_COLLECTION_PREFIXES: usize = 8;

/// Longest collection name, in UTF-8 bytes after trimming.
pub const MAX_COLLECTION_NAME_BYTES: usize = 64;

/// Longest prefix, in UTF-8 bytes after trimming and the closing `/`.
pub const MAX_COLLECTION_PREFIX_BYTES: usize = 256;

/// Largest set of collections as compact JSON, in bytes: well inside the
/// largest configuration document.
pub const MAX_COLLECTIONS_JSON_BYTES: usize = 8 * 1024;

/// One collection: a name and the storage key prefixes whose files belong
/// to it.
// Read by `from_object` (objects only); the serde attributes shape the
// schema and match what that reader takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LibraryCollection {
    /// The name people see and scope rules match, 1 to 64 bytes, distinct
    /// ignoring case.
    #[schema(example = "Kids")]
    pub name: String,
    /// Storage key prefixes, 1 to 8, each a folder relative to the
    /// destination's root (no leading `/`); a closing `/` is added when
    /// missing, so `kids` matches `kids/…` but not `kidsbooks/…`.
    #[schema(example = json!(["kids/", "children/"]), max_items = 8)]
    pub prefixes: Vec<String>,
}

/// [`LibraryCollection`]'s fields as JSON carries them.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectionFields {
    /// See [`LibraryCollection::name`].
    name: String,
    /// See [`LibraryCollection::prefixes`].
    prefixes: Vec<String>,
}

impl<'de> Deserialize<'de> for LibraryCollection {
    /// Reads a collection from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: CollectionFields =
            super::from_object(deserializer, "a JSON object with name and prefixes")?;
        Ok(Self {
            name: fields.name,
            prefixes: fields.prefixes,
        })
    }
}

/// The library's collections: the `core.collections` document and the
/// body of `PUT /api/library/collections`.
// Read by `from_object` (objects only); the serde attributes shape the
// schema and match what that reader takes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LibraryCollections {
    /// The collections, at most 32, in the order shown.
    #[schema(max_items = 32)]
    pub collections: Vec<LibraryCollection>,
}

/// [`LibraryCollections`]' fields as JSON carries them.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectionsFields {
    /// See [`LibraryCollections::collections`].
    collections: Vec<LibraryCollection>,
}

impl<'de> Deserialize<'de> for LibraryCollections {
    /// Reads the collections from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: CollectionsFields =
            super::from_object(deserializer, "a JSON object with collections")?;
        Ok(Self {
            collections: fields.collections,
        })
    }
}

/// The refusal for a set of collections outside the bounds.
fn invalid(detail: &str) -> LibraryError {
    LibraryError::Invalid(format!("invalid collections: {detail}"))
}

/// Trims a prefix and gives it a closing `/`.
fn normalize_prefix(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(invalid("a prefix is empty"));
    }
    if trimmed.starts_with('/') {
        return Err(invalid(
            "a prefix is relative to the destination's root and doesn't start with /",
        ));
    }
    if trimmed.contains('\0') {
        return Err(invalid("a prefix holds a NUL"));
    }
    let prefix = if trimmed.ends_with('/') {
        trimmed.to_string()
    } else {
        format!("{trimmed}/")
    };
    if prefix.len() > MAX_COLLECTION_PREFIX_BYTES {
        return Err(invalid(&format!(
            "a prefix is at most {MAX_COLLECTION_PREFIX_BYTES} bytes"
        )));
    }
    Ok(prefix)
}

impl LibraryCollections {
    /// Checks and normalizes the collections: names trimmed and distinct
    /// ignoring case, prefixes trimmed with a closing `/` and distinct
    /// within a collection, every bound held.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] naming the first bound broken.
    pub fn validate(self) -> Result<Self> {
        if self.collections.len() > MAX_COLLECTIONS {
            return Err(invalid(&format!(
                "a library has at most {MAX_COLLECTIONS} collections"
            )));
        }
        let mut names: Vec<String> = Vec::new();
        let mut out = Vec::new();
        for collection in self.collections {
            let name = collection.name.trim().to_string();
            if name.is_empty() || name.len() > MAX_COLLECTION_NAME_BYTES || name.contains('\0') {
                return Err(invalid(&format!(
                    "a name is 1 to {MAX_COLLECTION_NAME_BYTES} bytes"
                )));
            }
            let key = facet_key(&name);
            if names.contains(&key) {
                return Err(invalid(&format!("two collections are named {name}")));
            }
            names.push(key);
            if collection.prefixes.is_empty() || collection.prefixes.len() > MAX_COLLECTION_PREFIXES
            {
                return Err(invalid(&format!(
                    "a collection has 1 to {MAX_COLLECTION_PREFIXES} prefixes"
                )));
            }
            let mut prefixes: Vec<String> = Vec::new();
            for raw in &collection.prefixes {
                let prefix = normalize_prefix(raw)?;
                if !prefixes.contains(&prefix) {
                    prefixes.push(prefix);
                }
            }
            out.push(LibraryCollection { name, prefixes });
        }
        let normalized = Self { collections: out };
        let json = serde_json::to_string(&normalized).map_err(|err| invalid(&err.to_string()))?;
        if json.len() > MAX_COLLECTIONS_JSON_BYTES {
            return Err(invalid(&format!(
                "the collections come to more than {MAX_COLLECTIONS_JSON_BYTES} bytes"
            )));
        }
        Ok(normalized)
    }

    /// The names of the collections any of `keys` belongs to, in the
    /// collections' order.
    ///
    /// # Arguments
    ///
    /// * `keys` - Storage keys of a book's files.
    #[must_use]
    pub fn names_for_keys<'k>(&self, keys: impl IntoIterator<Item = &'k str> + Clone) -> Vec<&str> {
        self.collections
            .iter()
            .filter(|collection| {
                keys.clone().into_iter().any(|key| {
                    collection
                        .prefixes
                        .iter()
                        .any(|prefix| key.starts_with(prefix.as_str()))
                })
            })
            .map(|collection| collection.name.as_str())
            .collect()
    }
}

/// Parses and validates a `PUT /api/library/collections` body from its raw
/// bytes, sans IO, the way the route reads it: JSON into
/// [`LibraryCollections`], then [`LibraryCollections::validate`].
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for bytes that are not the body's JSON
/// shape (unknown or repeated fields, wrong types, a missing field, an
/// array instead of an object) or collections outside the bounds.
pub fn parse_collections_request(body: &[u8]) -> Result<LibraryCollections> {
    serde_json::from_slice::<LibraryCollections>(body)
        .map_err(|err| invalid(&err.to_string()))?
        .validate()
}

/// Parses a stored document, failing closed on an unknown schema.
fn parse_stored(stored: StoredDocument) -> Result<ConfigurationDocument<LibraryCollections>> {
    if stored.schema_version != COLLECTIONS_SCHEMA_VERSION {
        return Err(documents::unsupported(
            COLLECTIONS_NAMESPACE,
            stored.schema_version,
        ));
    }
    let body = parse_collections_request(stored.document_json.as_bytes())?;
    Ok(document(stored, body))
}

/// Reads the stored collections document, if one was ever written.
///
/// # Errors
///
/// Returns the read error, or [`LibraryError::Invalid`] for a stored
/// document this binary can't read.
pub async fn load_collections(
    store: &LibraryStore,
) -> Result<Option<ConfigurationDocument<LibraryCollections>>> {
    load_document(store, &DocumentKey::library(COLLECTIONS_NAMESPACE))
        .await?
        .map(parse_stored)
        .transpose()
}

/// The library's collections; none when the document was never written.
///
/// # Errors
///
/// See [`load_collections`].
pub async fn library_collections(store: &LibraryStore) -> Result<LibraryCollections> {
    Ok(load_collections(store)
        .await?
        .map(|document| document.body)
        .unwrap_or_default())
}

/// The collections document's revision, 0 when it was never written. The
/// page index rebuild compares it before and after its walk.
///
/// # Errors
///
/// See [`load_collections`].
pub async fn collections_revision(store: &LibraryStore) -> Result<i64> {
    Ok(load_collections(store)
        .await?
        .map_or(0, |document| document.revision))
}

/// Replaces the library's collections (compare-and-swap, audited) and,
/// when they changed, drops the page index's finished build so every row
/// is rebuilt with the new prefixes.
///
/// The caller checks who may write them; `actor` must be the operator or a
/// verified Owner or Administrator.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for collections outside the bounds,
/// [`LibraryError::Conflict`] when the document kept changing, or the write
/// error.
pub async fn set_library_collections(
    store: &LibraryStore,
    actor: &ConfigActor,
    collections: LibraryCollections,
) -> Result<ConfigurationDocument<LibraryCollections>> {
    let collections = collections.validate()?;
    let json = serde_json::to_string(&collections).map_err(|err| invalid(&err.to_string()))?;
    let key = DocumentKey::library(COLLECTIONS_NAMESPACE);
    for attempt in 0..4 {
        let current = match load_collections(store).await? {
            Some(document) => document,
            None => parse_stored(
                import_if_absent(
                    store,
                    actor,
                    &key,
                    COLLECTIONS_SCHEMA_VERSION,
                    r#"{"collections":[]}"#,
                    "core-collections-default",
                )
                .await?,
            )?,
        };
        if current.body == collections {
            return Ok(current);
        }
        let operation_id = format!(
            "core-collections-{}-{attempt}-{}",
            current.revision,
            uuid::Uuid::new_v4().simple()
        );
        match replace_document(
            store,
            actor,
            &key,
            COLLECTIONS_SCHEMA_VERSION,
            current.revision,
            &json,
            &operation_id,
        )
        .await?
        {
            ReplaceOutcome::Applied(stored) => {
                store.drop_library_index_build().await?;
                return parse_stored(stored);
            }
            ReplaceOutcome::Conflict { .. } | ReplaceOutcome::Replayed { .. } => {}
        }
    }
    Err(LibraryError::Conflict(
        "core.collections kept changing; retry".into(),
    ))
}

#[cfg(test)]
#[path = "collections_tests.rs"]
mod tests;
