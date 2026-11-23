//! Layout of an embedding model on disk.
//!
//! A text-embedding model has a vocabulary table with one row per token,
//! and that table is most of the download: 98 MB of bekko-a8m's 130 MB.
//! A library uses a handful of languages, so the table is shipped in
//! parts. One model directory holds:
//!
//! | File | What it is |
//! | --- | --- |
//! | `model.json` | The [`PackManifest`]: what the model is and the size and SHA-256 of every other file. |
//! | graph (`core.onnx`) | The published ONNX graph with its token tables turned into graph inputs. |
//! | tokenizer (`tokenizer.spm.fb`) | The whole vocabulary as a SentencePiece Lite model. |
//! | base rows (`base.rows`) | Table rows every text needs: special tokens, single characters, bytes. |
//! | language rows (`<tag>.rows`) | The other rows the texts of one language use. |
//!
//! The model plugin tokenizes with the whole vocabulary, looks each token's
//! row up in the row packs it has, and passes only those rows to the
//! graph. Token ids for a text are therefore the published model's own ids
//! whichever packs are present, and the output for a text whose rows are
//! all present is bit-identical to the published model's.
//!
//! Everything here is sans-IO: functions take bytes and return validated
//! values, so the plugin, the pack builder's tests and the fuzzers run the
//! same code. See `docs/internals/model-packs.md`.

mod manifest;
mod rows;

pub use manifest::{
    file_matches, Element, GraphSpec, LanguageTag, PackFile, PackFileName, PackManifest, Pooling,
    Sha256Hex, TableSpec, TokenizerSpec, MANIFEST_FILE, MAX_LANGUAGES, MAX_LITERAL_PIECES,
    MAX_MANIFEST_BYTES, MAX_TABLES, PACK_FORMAT,
};
pub use rows::{encode_rows, Rows, MAX_ROWS, MAX_ROW_BYTES, ROWS_HEADER_BYTES, ROWS_MAGIC};

/// Why a model directory's file was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PackError {
    /// `model.json` is longer than [`MAX_MANIFEST_BYTES`].
    #[error("model description is larger than {MAX_MANIFEST_BYTES} bytes")]
    ManifestTooLarge,
    /// `model.json` is not the JSON document this crate reads.
    #[error("model description is not valid: {0}")]
    ManifestSyntax(String),
    /// `model.json` was written for another layout version.
    #[error("model layout version {0} is not supported (this build reads {PACK_FORMAT})")]
    UnsupportedFormat(u32),
    /// A field of `model.json` breaks a rule; the text names the field.
    #[error("model description field `{field}` is not valid: {reason}")]
    ManifestField {
        /// Dotted path of the field (`graph.output`).
        field: &'static str,
        /// The rule it breaks.
        reason: &'static str,
    },
    /// A file's length or SHA-256 differs from what `model.json` lists.
    #[error("file `{0}` does not match the model description")]
    FileMismatch(String),
    /// A row pack is shorter than its header.
    #[error("row pack is truncated")]
    RowsTruncated,
    /// A row pack does not start with [`ROWS_MAGIC`].
    #[error("row pack has the wrong signature")]
    RowsMagic,
    /// A row pack's rows are not the length the model's tables give.
    #[error("row pack has {found}-byte rows, the model needs {expected}")]
    RowsWidth {
        /// Row length the model's tables add up to.
        expected: usize,
        /// Row length the pack's header states.
        found: usize,
    },
    /// A row pack's row count or row length is over the limits.
    #[error("row pack is larger than the limits allow")]
    RowsTooLarge,
    /// A row pack's length differs from what its header implies.
    #[error("row pack length does not match its header")]
    RowsLength,
    /// A row pack's token ids are not strictly ascending.
    #[error("row pack token ids are not in ascending order")]
    RowsOrder,
}

/// The revision of a loaded model: a digest of its description, which
/// lists the digest of every file, and of the languages loaded.
///
/// Two outputs are comparable only when their revisions match, so the
/// revision changes when the weights, the tokenizer or the set of loaded
/// language packs change.
///
/// # Arguments
///
/// * `manifest_bytes` - The bytes of `model.json`.
/// * `languages` - Tags of the language packs loaded, in any order.
///
/// # Returns
///
/// 32 lowercase hexadecimal characters.
#[must_use]
pub fn revision(manifest_bytes: &[u8], languages: &[LanguageTag]) -> String {
    use sha2::Digest;
    let mut tags: Vec<&str> = languages.iter().map(LanguageTag::as_str).collect();
    tags.sort_unstable();
    tags.dedup();
    let mut hasher = sha2::Sha256::new();
    hasher.update(sha2::Sha256::digest(manifest_bytes));
    for tag in tags {
        hasher.update([0u8]);
        hasher.update(tag.as_bytes());
    }
    let mut text = hex::encode(hasher.finalize());
    text.truncate(32);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tag for tests.
    fn tag(text: &str) -> LanguageTag {
        LanguageTag::try_from(text.to_owned()).expect("tag")
    }

    #[test]
    fn revision_depends_on_the_description_and_the_language_set_only() {
        let a = revision(b"{}", &[tag("en"), tag("de")]);
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(a, revision(b"{}", &[tag("de"), tag("en"), tag("en")]));
        assert_ne!(a, revision(b"{}", &[tag("en")]));
        assert_ne!(a, revision(b"{} ", &[tag("en"), tag("de")]));
        // A tag boundary cannot be moved: ["ab", "c"] differs from ["a", "bc"].
        assert_ne!(
            revision(b"", &[tag("ab"), tag("c")]),
            revision(b"", &[tag("a"), tag("bc")])
        );
    }
}
