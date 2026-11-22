//! `model.json`: what a model directory holds and how to run it.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::rows::MAX_ROW_BYTES;
use crate::PackError;

/// Layout version this crate reads and writes (`format` in `model.json`).
pub const PACK_FORMAT: u32 = 1;

/// Name of the model description inside a model directory.
pub const MANIFEST_FILE: &str = "model.json";

/// Most bytes of `model.json` that are read.
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;

/// Most token tables one model has (bekko-a8m has two, Harrier three).
pub const MAX_TABLES: usize = 8;

/// Most language packs one model lists.
pub const MAX_LANGUAGES: usize = 64;

/// Most literal pieces a tokenizer lists (bekko-a8m has 142).
pub const MAX_LITERAL_PIECES: usize = 1024;

/// Most bytes of a graph input or output name, a piece or a text prefix.
const MAX_NAME_BYTES: usize = 1024;

/// Most pieces put before or after a text's tokens.
const MAX_FRAME_PIECES: usize = 8;

/// Most bytes of a model id (the plugin ABI's own cap is checked by the host).
const MAX_MODEL_ID_BYTES: usize = 64;

/// Most numbers in one vector this layout describes.
const MAX_DIMENSIONS: u32 = 8192;

/// Most tokens of one text this layout describes.
const MAX_TOKENS: u32 = 65_536;

/// Shorthand for a refused field.
fn field(field: &'static str, reason: &'static str) -> PackError {
    PackError::ManifestField { field, reason }
}

/// A file name inside a model directory: 1 to 64 lowercase letters,
/// digits, `.`, `_` and `-`, starting with a letter or digit.
///
/// The grammar leaves no way to name a parent directory, a hidden file or
/// a path, so joining it to the model directory stays inside it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PackFileName(String);

impl PackFileName {
    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PackFileName {
    type Error = &'static str;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        let bytes = name.as_bytes();
        let plain = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        let ok = (1..=64).contains(&bytes.len())
            && bytes.first().is_some_and(plain)
            && bytes
                .iter()
                .all(|b| plain(b) || matches!(b, b'.' | b'_' | b'-'))
            && name != MANIFEST_FILE;
        if ok {
            Ok(Self(name))
        } else {
            Err("not a plain file name")
        }
    }
}

impl From<PackFileName> for String {
    fn from(name: PackFileName) -> Self {
        name.0
    }
}

/// A SHA-256 digest as 64 lowercase hexadecimal characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sha256Hex(String);

impl Sha256Hex {
    /// The digest of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        use sha2::Digest;
        Self(hex::encode(sha2::Sha256::digest(bytes)))
    }

    /// The digest as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Sha256Hex {
    type Error = &'static str;

    fn try_from(digest: String) -> Result<Self, Self::Error> {
        if digest.len() == 64
            && digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(digest))
        } else {
            Err("not a lowercase SHA-256 digest")
        }
    }
}

impl From<Sha256Hex> for String {
    fn from(digest: Sha256Hex) -> Self {
        digest.0
    }
}

/// A language tag naming a language pack: lowercase letters and digits in
/// `-`-separated parts of 1 to 8 characters, at most 35 bytes (`en`,
/// `pt-br`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LanguageTag(String);

impl LanguageTag {
    /// The tag as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for LanguageTag {
    type Error = &'static str;

    fn try_from(tag: String) -> Result<Self, Self::Error> {
        let ok = !tag.is_empty()
            && tag.len() <= 35
            && tag.split('-').all(|part| {
                (1..=8).contains(&part.len())
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            });
        if ok {
            Ok(Self(tag))
        } else {
            Err("not a lowercase language tag")
        }
    }
}

impl From<LanguageTag> for String {
    fn from(tag: LanguageTag) -> Self {
        tag.0
    }
}

/// One file of a model directory, with what its content must be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackFile {
    /// Name inside the model directory.
    pub file: PackFileName,
    /// Length in bytes.
    pub bytes: u64,
    /// SHA-256 of the content.
    pub sha256: Sha256Hex,
}

impl PackFile {
    /// Describes `content` under the name `file`.
    #[must_use]
    pub fn describe(file: PackFileName, content: &[u8]) -> Self {
        Self {
            file,
            bytes: u64::try_from(content.len()).unwrap_or(u64::MAX),
            sha256: Sha256Hex::of(content),
        }
    }

    /// Checks that `content` is the file the description lists.
    ///
    /// # Errors
    ///
    /// [`PackError::FileMismatch`] when the length or the digest differs.
    pub fn verify(&self, content: &[u8]) -> Result<(), PackError> {
        let same_length = u64::try_from(content.len()).is_ok_and(|len| len == self.bytes);
        if same_length && Sha256Hex::of(content) == self.sha256 {
            Ok(())
        } else {
            Err(PackError::FileMismatch(self.file.as_str().to_owned()))
        }
    }
}

/// How the graph's output becomes one vector per text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pooling {
    /// The output has one vector per token (`[1, tokens, dimensions]`);
    /// the text's vector is their mean.
    Mean,
    /// The output already is the text's vector (`[1, dimensions]`).
    None,
}

/// The cut ONNX graph and the names it uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphSpec {
    /// The graph file.
    pub model: PackFile,
    /// Input that takes token positions: 64-bit integers `[1, tokens]`,
    /// numbered `0..tokens` because the tables passed hold one row per
    /// token of the text, in order.
    pub ids_input: String,
    /// Input that takes the attention mask: 64-bit ones, `[1, tokens]`.
    pub mask_input: String,
    /// Output read after a run: 32-bit floats.
    pub output: String,
    /// How that output becomes the text's vector.
    pub pooling: Pooling,
}

/// The tokenizer and how a text is framed for the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenizerSpec {
    /// The SentencePiece Lite model with the whole vocabulary.
    pub model: PackFile,
    /// Pieces matched in the text before the tokenizer runs, longest match
    /// first; each becomes its own token and the text between them is
    /// tokenized segment by segment. A segment that starts with a space
    /// loses that space (a segment of only that space becomes the word
    /// boundary piece), as the published tokenizer does.
    #[serde(default)]
    pub literal_pieces: Vec<String>,
    /// Pieces put before the text's tokens (`<bos>`).
    #[serde(default)]
    pub prefix_pieces: Vec<String>,
    /// Pieces put after the text's tokens (`<eos>`).
    #[serde(default)]
    pub suffix_pieces: Vec<String>,
    /// Text put before a document before tokenizing.
    #[serde(default)]
    pub document_prefix: String,
    /// Text put before a search query before tokenizing.
    #[serde(default)]
    pub query_prefix: String,
}

/// Element type of a token table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Element {
    /// Signed 8-bit integers.
    I8,
    /// Unsigned 8-bit integers.
    U8,
    /// Little-endian IEEE 754 32-bit floats.
    F32,
}

impl Element {
    /// Bytes one element takes.
    #[must_use]
    pub fn size(self) -> usize {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::F32 => 4,
        }
    }
}

/// One token table the graph takes as an input: `[tokens, width]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableSpec {
    /// Graph input that takes the rows.
    pub input: String,
    /// Element type.
    pub element: Element,
    /// Elements per row.
    pub width: u32,
}

impl TableSpec {
    /// Bytes one row of this table takes, or `None` on overflow.
    #[must_use]
    pub fn row_bytes(&self) -> Option<usize> {
        usize::try_from(self.width)
            .ok()?
            .checked_mul(self.element.size())
    }
}

/// `model.json`: one embedding model, cut for row packs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackManifest {
    /// Layout version; [`PACK_FORMAT`].
    pub format: u32,
    /// Model id within the plugin (`bekko-a8m`).
    pub model_id: String,
    /// Numbers per vector.
    pub dimensions: u32,
    /// Tokens read from one text, counting the framing pieces; the rest of
    /// the text is cut off.
    pub max_tokens: u32,
    /// The cut graph.
    pub graph: GraphSpec,
    /// The tokenizer.
    pub tokenizer: TokenizerSpec,
    /// The token tables, in the order their rows are concatenated in a row
    /// pack.
    pub tables: Vec<TableSpec>,
    /// Rows every text needs.
    pub base_rows: PackFile,
    /// The row pack of each language, present on disk or not.
    #[serde(default)]
    pub languages: BTreeMap<LanguageTag, PackFile>,
}

/// The one field read before the rest, to tell an unknown layout version
/// from a malformed description.
#[derive(Deserialize)]
struct FormatProbe {
    /// `format` of the description.
    format: u32,
}

/// Whether `name` can name a graph input or output, a piece or a prefix:
/// 1 to [`MAX_NAME_BYTES`] bytes with no NUL.
fn is_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME_BYTES && !name.contains('\0')
}

impl PackManifest {
    /// Reads and checks a `model.json`.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The file's content.
    ///
    /// # Errors
    ///
    /// [`PackError::ManifestTooLarge`], [`PackError::ManifestSyntax`],
    /// [`PackError::UnsupportedFormat`] or the first
    /// [`PackError::ManifestField`] rule broken.
    pub fn parse(bytes: &[u8]) -> Result<Self, PackError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(PackError::ManifestTooLarge);
        }
        let probe: FormatProbe = serde_json::from_slice(bytes)
            .map_err(|err| PackError::ManifestSyntax(err.to_string()))?;
        if probe.format != PACK_FORMAT {
            return Err(PackError::UnsupportedFormat(probe.format));
        }
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|err| PackError::ManifestSyntax(err.to_string()))?;
        manifest.check()?;
        Ok(manifest)
    }

    /// Writes the description as JSON that [`PackManifest::parse`] reads.
    ///
    /// # Errors
    ///
    /// The first rule the description breaks, as [`PackManifest::parse`]
    /// would report it.
    pub fn to_json(&self) -> Result<Vec<u8>, PackError> {
        self.check()?;
        let mut bytes = serde_json::to_vec_pretty(self)
            .map_err(|err| PackError::ManifestSyntax(err.to_string()))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(PackError::ManifestTooLarge);
        }
        Ok(bytes)
    }

    /// Bytes of one row of a row pack: every table's row, concatenated.
    ///
    /// Within 1 to [`MAX_ROW_BYTES`] for a checked description.
    #[must_use]
    pub fn row_bytes(&self) -> usize {
        self.tables
            .iter()
            .map(|table| table.row_bytes().unwrap_or(usize::MAX))
            .fold(0usize, usize::saturating_add)
    }

    /// Checks every rule a description must keep.
    fn check(&self) -> Result<(), PackError> {
        if self.format != PACK_FORMAT {
            return Err(PackError::UnsupportedFormat(self.format));
        }
        let id = self.model_id.as_bytes();
        let plain = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        if !((1..=MAX_MODEL_ID_BYTES).contains(&id.len())
            && id.first().is_some_and(plain)
            && id
                .iter()
                .all(|b| plain(b) || matches!(b, b'.' | b'_' | b'-')))
        {
            return Err(field("model_id", "not a model id"));
        }
        if !(1..=MAX_DIMENSIONS).contains(&self.dimensions) {
            return Err(field("dimensions", "out of range"));
        }
        if !(1..=MAX_TOKENS).contains(&self.max_tokens) {
            return Err(field("max_tokens", "out of range"));
        }
        self.check_graph_names()?;
        self.check_tokenizer()?;
        if self.languages.len() > MAX_LANGUAGES {
            return Err(field("languages", "too many"));
        }
        let mut files = BTreeSet::new();
        let listed = [&self.graph.model, &self.tokenizer.model, &self.base_rows]
            .into_iter()
            .chain(self.languages.values());
        for file in listed {
            if !files.insert(file.file.as_str()) {
                return Err(field("file", "listed twice"));
            }
        }
        Ok(())
    }

    /// Checks the graph's names and the tables.
    fn check_graph_names(&self) -> Result<(), PackError> {
        if !is_name(&self.graph.output) {
            return Err(field("graph.output", "empty or too long"));
        }
        if self.tables.is_empty() || self.tables.len() > MAX_TABLES {
            return Err(field("tables", "needs 1 to 8 tables"));
        }
        let mut inputs = BTreeSet::new();
        let names = [&self.graph.ids_input, &self.graph.mask_input]
            .into_iter()
            .chain(self.tables.iter().map(|table| &table.input));
        for name in names {
            if !is_name(name) {
                return Err(field("graph input", "empty or too long"));
            }
            if !inputs.insert(name.as_str()) {
                return Err(field("graph input", "named twice"));
            }
        }
        if self.tables.iter().any(|table| table.width == 0) {
            return Err(field("tables.width", "zero"));
        }
        if !(1..=MAX_ROW_BYTES).contains(&self.row_bytes()) {
            return Err(field("tables", "rows too long"));
        }
        Ok(())
    }

    /// Checks the tokenizer's pieces and prefixes.
    fn check_tokenizer(&self) -> Result<(), PackError> {
        let tokenizer = &self.tokenizer;
        if tokenizer.literal_pieces.len() > MAX_LITERAL_PIECES {
            return Err(field("tokenizer.literal_pieces", "too many"));
        }
        let mut literals = BTreeSet::new();
        for piece in &tokenizer.literal_pieces {
            if !is_name(piece) || !literals.insert(piece.as_str()) {
                return Err(field(
                    "tokenizer.literal_pieces",
                    "empty, too long or listed twice",
                ));
            }
        }
        for frame in [&tokenizer.prefix_pieces, &tokenizer.suffix_pieces] {
            if frame.len() > MAX_FRAME_PIECES || frame.iter().any(|piece| !is_name(piece)) {
                return Err(field("tokenizer framing pieces", "too many or not a piece"));
            }
        }
        let framing = tokenizer.prefix_pieces.len() + tokenizer.suffix_pieces.len();
        if u32::try_from(framing).map_or(true, |framing| framing >= self.max_tokens) {
            return Err(field("max_tokens", "no room for text after the framing"));
        }
        for prefix in [&tokenizer.document_prefix, &tokenizer.query_prefix] {
            if prefix.len() > MAX_NAME_BYTES || prefix.contains('\0') {
                return Err(field("tokenizer text prefix", "too long"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file entry for tests.
    fn file(name: &str) -> PackFile {
        PackFile::describe(
            PackFileName::try_from(name.to_owned()).expect("name"),
            name.as_bytes(),
        )
    }

    /// A description shaped like bekko-a8m's.
    pub(crate) fn sample() -> PackManifest {
        PackManifest {
            format: PACK_FORMAT,
            model_id: "bekko-a8m".into(),
            dimensions: 384,
            max_tokens: 512,
            graph: GraphSpec {
                model: file("core.onnx"),
                ids_input: "input_ids".into(),
                mask_input: "attention_mask".into(),
                output: "last_hidden_state".into(),
                pooling: Pooling::Mean,
            },
            tokenizer: TokenizerSpec {
                model: file("tokenizer.spm.fb"),
                literal_pieces: vec!["\n".into(), "\n\n".into()],
                prefix_pieces: vec!["<bos>".into()],
                suffix_pieces: vec!["<eos>".into()],
                document_prefix: String::new(),
                query_prefix: "Query: ".into(),
            },
            tables: vec![
                TableSpec {
                    input: "embeddings.tok_embeddings.weight".into(),
                    element: Element::I8,
                    width: 384,
                },
                TableSpec {
                    input: "static_small_embeddings.rowwise_scale".into(),
                    element: Element::F32,
                    width: 1,
                },
            ],
            base_rows: file("base.rows"),
            languages: BTreeMap::from([(
                LanguageTag::try_from("en".to_owned()).expect("tag"),
                file("en.rows"),
            )]),
        }
    }

    /// The field a broken description is refused for.
    fn refused(manifest: &PackManifest) -> &'static str {
        match manifest.to_json() {
            Err(PackError::ManifestField { field, .. }) => field,
            other => panic!("expected a field error, got {other:?}"),
        }
    }

    #[test]
    fn a_description_survives_writing_and_reading() {
        let manifest = sample();
        assert_eq!(manifest.row_bytes(), 388);
        let json = manifest.to_json().expect("json");
        assert_eq!(PackManifest::parse(&json).expect("parse"), manifest);
        let text = String::from_utf8(json).expect("utf-8");
        assert!(text.contains("\"pooling\": \"mean\""));
        assert!(text.contains("\"element\": \"i8\""));
        assert!(text.contains("\"file\": \"core.onnx\""));
    }

    #[test]
    fn syntax_size_and_version_are_told_apart() {
        assert!(matches!(
            PackManifest::parse(b"not json"),
            Err(PackError::ManifestSyntax(_))
        ));
        assert_eq!(
            PackManifest::parse(br#"{"format": 2, "anything": true}"#),
            Err(PackError::UnsupportedFormat(2))
        );
        assert!(matches!(
            PackManifest::parse(br#"{"format": 1}"#),
            Err(PackError::ManifestSyntax(_))
        ));
        assert_eq!(
            PackManifest::parse(&vec![b' '; MAX_MANIFEST_BYTES + 1]),
            Err(PackError::ManifestTooLarge)
        );
        let mut extra: serde_json::Value =
            serde_json::from_slice(&sample().to_json().expect("json")).expect("value");
        extra["surprise"] = serde_json::json!(1);
        assert!(matches!(
            PackManifest::parse(&serde_json::to_vec(&extra).expect("bytes")),
            Err(PackError::ManifestSyntax(_))
        ));
    }

    #[test]
    fn file_names_cannot_leave_the_directory() {
        for bad in [
            "",
            ".hidden",
            "..",
            "a/b",
            "a\\b",
            "UPPER",
            "model.json",
            "a b",
            "é",
            &"a".repeat(65),
        ] {
            assert!(PackFileName::try_from(bad.to_owned()).is_err(), "{bad:?}");
        }
        for good in ["core.onnx", "pt-br.rows", "0", &"a".repeat(64)] {
            assert_eq!(
                PackFileName::try_from(good.to_owned())
                    .expect(good)
                    .as_str(),
                good
            );
        }
        let json = String::from_utf8(sample().to_json().expect("json"))
            .expect("utf-8")
            .replace("base.rows", "../base.rows");
        assert!(matches!(
            PackManifest::parse(json.as_bytes()),
            Err(PackError::ManifestSyntax(_))
        ));
    }

    #[test]
    fn digests_and_tags_are_validated_on_the_way_in() {
        assert!(Sha256Hex::try_from("ab".repeat(32)).is_ok());
        assert!(Sha256Hex::try_from("AB".repeat(32)).is_err());
        assert!(Sha256Hex::try_from("ab".repeat(31)).is_err());
        assert_eq!(String::from(Sha256Hex::of(b"")).len(), 64);
        for bad in ["", "EN", "en_us", "abcdefghi", "en--us", &"ab-".repeat(12)] {
            assert!(LanguageTag::try_from(bad.to_owned()).is_err(), "{bad:?}");
        }
        let tag = LanguageTag::try_from("pt-br".to_owned()).expect("tag");
        assert_eq!(String::from(tag.clone()), "pt-br");
        assert_eq!(
            String::from(PackFileName::try_from("a".to_owned()).expect("name")),
            "a"
        );
    }

    #[test]
    fn a_file_is_verified_by_length_and_digest() {
        let entry = file("core.onnx");
        assert_eq!(entry.verify(b"core.onnx"), Ok(()));
        assert_eq!(
            entry.verify(b"core.onnX"),
            Err(PackError::FileMismatch("core.onnx".into()))
        );
        assert_eq!(
            entry.verify(b"core.onnx "),
            Err(PackError::FileMismatch("core.onnx".into()))
        );
    }

    #[test]
    fn every_field_rule_is_enforced() {
        let broken = |change: fn(&mut PackManifest)| {
            let mut manifest = sample();
            change(&mut manifest);
            refused(&manifest)
        };
        assert_eq!(broken(|m| m.model_id = "Bekko".into()), "model_id");
        assert_eq!(broken(|m| m.model_id = String::new()), "model_id");
        assert_eq!(broken(|m| m.dimensions = 0), "dimensions");
        assert_eq!(broken(|m| m.max_tokens = 0), "max_tokens");
        assert_eq!(broken(|m| m.max_tokens = 2), "max_tokens");
        assert_eq!(broken(|m| m.graph.output = String::new()), "graph.output");
        assert_eq!(broken(|m| m.tables.clear()), "tables");
        assert_eq!(
            broken(|m| m.graph.mask_input = "input_ids".into()),
            "graph input"
        );
        assert_eq!(broken(|m| m.tables[1].input = "a\0b".into()), "graph input");
        assert_eq!(broken(|m| m.tables[0].width = 0), "tables.width");
        assert_eq!(broken(|m| m.tables[0].width = u32::MAX), "tables");
        assert_eq!(
            broken(|m| m.tokenizer.literal_pieces.push("\n".into())),
            "tokenizer.literal_pieces"
        );
        assert_eq!(
            broken(|m| m.tokenizer.literal_pieces = vec!["x".into(); MAX_LITERAL_PIECES + 1]),
            "tokenizer.literal_pieces"
        );
        assert_eq!(
            broken(|m| m.tokenizer.prefix_pieces = vec![String::new()]),
            "tokenizer framing pieces"
        );
        assert_eq!(
            broken(|m| m.tokenizer.query_prefix = "q".repeat(MAX_NAME_BYTES + 1)),
            "tokenizer text prefix"
        );
        assert_eq!(
            broken(|m| m.base_rows.file = m.graph.model.file.clone()),
            "file"
        );
        assert_eq!(
            broken(|m| {
                for index in 0..=MAX_LANGUAGES {
                    let tag = LanguageTag::try_from(format!("l{index}")).expect("tag");
                    let name = PackFileName::try_from(format!("l{index}.rows")).expect("name");
                    m.languages.insert(tag, PackFile::describe(name, b""));
                }
            }),
            "languages"
        );
        let mut other_format = sample();
        other_format.format = 9;
        assert_eq!(other_format.to_json(), Err(PackError::UnsupportedFormat(9)));
    }

    #[test]
    fn element_sizes_match_their_types() {
        assert_eq!(Element::I8.size(), 1);
        assert_eq!(Element::U8.size(), 1);
        assert_eq!(Element::F32.size(), 4);
        let harrier = [(Element::U8, 320), (Element::F32, 20), (Element::U8, 10)];
        let bytes: usize = harrier
            .iter()
            .map(|(element, width)| {
                TableSpec {
                    input: "t".into(),
                    element: *element,
                    width: *width,
                }
                .row_bytes()
                .expect("fits")
            })
            .sum();
        assert_eq!(bytes, 410);
    }
}
