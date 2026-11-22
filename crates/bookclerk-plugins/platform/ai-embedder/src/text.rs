//! From a text to the model's token ids.
//!
//! [`TextEncoder`] reproduces what the model's published tokenizer does
//! around SentencePiece: the text prefix for the text's role, the split on
//! literal pieces, the token limit and the framing pieces. The ids it
//! gives for a text are the published model's ids.

use bookclerk_model_pack::TokenizerSpec;
use bookclerk_sentencepiece::{SpecialPiece, Tokenizer, TokenizerError};

/// The piece SentencePiece puts where a word starts.
const WORD_BOUNDARY: &str = "\u{2581}";

/// Why a tokenizer could not be set up or a text not encoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    /// The tokenizer file is not a SentencePiece Lite model, or the
    /// runtime failed on a text.
    #[error("tokenizer: {0}")]
    Tokenizer(#[from] TokenizerError),
    /// The model description names a piece the vocabulary does not have.
    #[error("the vocabulary has no piece `{0}`")]
    MissingPiece(String),
    /// The vocabulary has no unknown-token piece to fall back on.
    #[error("the vocabulary has no unknown-token piece")]
    NoUnknownPiece,
}

/// What a text is to the model; some models put a different prefix before
/// a search query than before a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Stored content.
    Document,
    /// A search query matched against documents.
    Query,
}

/// A model's tokenizer with the framing its description asks for.
#[derive(Debug)]
pub struct TextEncoder {
    /// The SentencePiece Lite runtime over the whole vocabulary.
    tokenizer: Tokenizer,
    /// Literal pieces and their ids, longest first, so the first match at
    /// a position is the longest.
    literals: Vec<(String, u32)>,
    /// Whether some literal starts with this byte; skips the literal scan
    /// at most positions.
    literal_starts: [bool; 256],
    /// Ids put before the text's tokens.
    prefix: Vec<u32>,
    /// Ids put after the text's tokens.
    suffix: Vec<u32>,
    /// Id of the word-boundary piece, or of the unknown token when the
    /// vocabulary has none.
    boundary: u32,
    /// Whether the tokenizer puts a word boundary before a text by itself
    /// (bekko's does, Gemma's does not).
    adds_boundary: bool,
    /// Id of the unknown token.
    unknown: u32,
    /// Most ids of the text itself: the model's token limit less the
    /// framing.
    body_limit: usize,
    /// Text put before a document.
    document_prefix: String,
    /// Text put before a query.
    query_prefix: String,
}

impl TextEncoder {
    /// Sets a tokenizer up for a model.
    ///
    /// # Arguments
    ///
    /// * `model` - The tokenizer file's bytes, already checked against the
    ///   model description.
    /// * `spec` - The tokenizer part of the model description.
    /// * `max_tokens` - The model's token limit, framing included.
    ///
    /// # Errors
    ///
    /// When the bytes are not a tokenizer model, or the description names a
    /// piece the vocabulary does not have.
    pub fn new(model: Vec<u8>, spec: &TokenizerSpec, max_tokens: u32) -> Result<Self, TextError> {
        let tokenizer = Tokenizer::from_bytes(model)?;
        let id_of = |piece: &String| {
            tokenizer
                .piece_to_id(piece)
                .ok_or_else(|| TextError::MissingPiece(piece.clone()))
        };
        let mut literals = spec
            .literal_pieces
            .iter()
            .map(|piece| Ok((piece.clone(), id_of(piece)?)))
            .collect::<Result<Vec<_>, TextError>>()?;
        literals.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        let mut literal_starts = [false; 256];
        for (piece, _) in &literals {
            if let Some(first) = piece.as_bytes().first() {
                literal_starts[usize::from(*first)] = true;
            }
        }
        let prefix = spec
            .prefix_pieces
            .iter()
            .map(id_of)
            .collect::<Result<Vec<_>, _>>()?;
        let suffix = spec
            .suffix_pieces
            .iter()
            .map(id_of)
            .collect::<Result<Vec<_>, _>>()?;
        let unknown = tokenizer
            .special_id(SpecialPiece::Unknown)
            .ok_or(TextError::NoUnknownPiece)?;
        let boundary = tokenizer.piece_to_id(WORD_BOUNDARY).unwrap_or(unknown);
        // The tokenizer model does not expose whether it puts a word
        // boundary before a text, so ask it with a one-letter text.
        let adds_boundary = tokenizer
            .encode("a", 2)?
            .ids
            .first()
            .and_then(|id| tokenizer.id_to_piece(*id))
            .is_some_and(|piece| piece.starts_with(WORD_BOUNDARY));
        let limit = usize::try_from(max_tokens).unwrap_or(usize::MAX);
        let body_limit = limit.saturating_sub(prefix.len() + suffix.len());
        Ok(Self {
            tokenizer,
            literals,
            literal_starts,
            prefix,
            suffix,
            boundary,
            adds_boundary,
            unknown,
            body_limit,
            document_prefix: spec.document_prefix.clone(),
            query_prefix: spec.query_prefix.clone(),
        })
    }

    /// Most ids of the text itself, the framing not counted.
    #[must_use]
    pub fn body_limit(&self) -> usize {
        self.body_limit
    }

    /// Ids the framing and the fallback always need: the framing pieces,
    /// the unknown token and the word boundary.
    pub fn required_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.prefix
            .iter()
            .chain(&self.suffix)
            .copied()
            .chain([self.unknown, self.boundary])
    }

    /// The literal piece that starts at the beginning of `rest`, if any.
    fn literal_at(&self, rest: &str) -> Option<(usize, u32)> {
        let first = *rest.as_bytes().first()?;
        if !self.literal_starts[usize::from(first)] {
            return None;
        }
        self.literals
            .iter()
            .find(|(piece, _)| rest.starts_with(piece.as_str()))
            .map(|(piece, id)| (piece.len(), *id))
    }

    /// Tokenizes one stretch of text between literal pieces onto `ids`,
    /// stopping at the body limit.
    fn push_segment(&self, segment: &str, ids: &mut Vec<u32>) -> Result<(), TextError> {
        let room = self.body_limit.saturating_sub(ids.len());
        if segment.is_empty() || room == 0 {
            return Ok(());
        }
        // A published tokenizer that adds a word boundary before a segment
        // does so only when the segment does not already start with a
        // space; SentencePiece then always adds one. Dropping the space
        // keeps the two the same. A tokenizer that adds no boundary gets
        // the segment as it is.
        let segment = match segment.strip_prefix(' ') {
            Some("") if self.adds_boundary => {
                ids.push(self.boundary);
                return Ok(());
            }
            Some(rest) if self.adds_boundary => rest,
            Some(_) | None => segment,
        };
        ids.extend(self.tokenizer.encode(segment, room)?.ids);
        Ok(())
    }

    /// The ids of a text, without the framing pieces.
    ///
    /// # Arguments
    ///
    /// * `text` - The text as the caller gave it.
    /// * `role` - Which text prefix goes before it.
    ///
    /// # Returns
    ///
    /// At most [`TextEncoder::body_limit`] ids; a longer text is cut off.
    ///
    /// # Errors
    ///
    /// When the tokenizer runtime fails on the text.
    pub fn body_ids(&self, text: &str, role: Role) -> Result<Vec<u32>, TextError> {
        let prefix = match role {
            Role::Document => &self.document_prefix,
            Role::Query => &self.query_prefix,
        };
        let framed;
        let text = if prefix.is_empty() {
            text
        } else {
            framed = format!("{prefix}{text}");
            framed.as_str()
        };
        let mut ids = Vec::new();
        let mut segment_start = 0;
        let mut position = 0;
        while position < text.len() && ids.len() < self.body_limit {
            let rest = &text[position..];
            if let Some((length, id)) = self.literal_at(rest) {
                self.push_segment(&text[segment_start..position], &mut ids)?;
                if ids.len() < self.body_limit {
                    ids.push(id);
                }
                position += length;
                segment_start = position;
            } else {
                position += rest.chars().next().map_or(1, char::len_utf8);
            }
        }
        self.push_segment(&text[segment_start..], &mut ids)?;
        ids.truncate(self.body_limit);
        Ok(ids)
    }

    /// Puts the framing pieces around a text's ids.
    #[must_use]
    pub fn frame(&self, mut body: Vec<u32>) -> Vec<u32> {
        body.truncate(self.body_limit);
        let mut ids = Vec::with_capacity(self.prefix.len() + body.len() + self.suffix.len());
        ids.extend_from_slice(&self.prefix);
        ids.append(&mut body);
        ids.extend_from_slice(&self.suffix);
        ids
    }

    /// Spells a token with single-character pieces, for a token whose
    /// table row is not loaded.
    ///
    /// # Arguments
    ///
    /// * `id` - A token id the tokenizer gave.
    ///
    /// # Returns
    ///
    /// One id per character of the token's piece: the word boundary for a
    /// boundary mark, the character's own piece when the vocabulary has
    /// one, the unknown token otherwise. An id outside the vocabulary
    /// gives the unknown token alone.
    #[must_use]
    pub fn spell(&self, id: u32) -> Vec<u32> {
        let Some(piece) = self.tokenizer.id_to_piece(id) else {
            return vec![self.unknown];
        };
        let mut buffer = [0u8; 4];
        piece
            .chars()
            .map(|character| {
                let single: &str = character.encode_utf8(&mut buffer);
                if single == WORD_BOUNDARY {
                    self.boundary
                } else {
                    self.tokenizer.piece_to_id(single).unwrap_or(self.unknown)
                }
            })
            .collect()
    }

    /// Id of the unknown token.
    #[must_use]
    pub fn unknown_id(&self) -> u32 {
        self.unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_model_pack::{PackFile, PackFileName};

    /// The 640-piece tokenizer fixture of `bookclerk-sentencepiece`.
    pub(crate) const TINY_TOKENIZER: &[u8] =
        include_bytes!("../../../../bookclerk-sentencepiece/tests/fixtures/tiny.spm.fb");

    /// A tokenizer description over the fixture.
    fn spec() -> TokenizerSpec {
        TokenizerSpec {
            model: PackFile::describe(
                PackFileName::try_from("tokenizer.spm.fb".to_owned()).unwrap(),
                TINY_TOKENIZER,
            ),
            literal_pieces: Vec::new(),
            prefix_pieces: vec!["<s>".into()],
            suffix_pieces: vec!["</s>".into()],
            document_prefix: String::new(),
            query_prefix: String::new(),
        }
    }

    /// An encoder over the fixture.
    fn encoder(spec: &TokenizerSpec, max_tokens: u32) -> TextEncoder {
        TextEncoder::new(TINY_TOKENIZER.to_vec(), spec, max_tokens).unwrap()
    }

    /// The fixture's own ids for `text`.
    fn reference(text: &str) -> Vec<u32> {
        Tokenizer::from_bytes(TINY_TOKENIZER.to_vec())
            .unwrap()
            .encode(text, 10_000)
            .unwrap()
            .ids
    }

    #[test]
    fn a_plain_text_gets_the_tokenizers_ids_inside_the_framing() {
        let encoder = encoder(&spec(), 64);
        assert_eq!(encoder.body_limit(), 62);
        let body = encoder
            .body_ids("A detective in Victorian London", Role::Document)
            .unwrap();
        assert_eq!(body, reference("A detective in Victorian London"));
        let framed = encoder.frame(body.clone());
        assert_eq!(framed.first(), Some(&2));
        assert_eq!(framed.last(), Some(&1));
        assert_eq!(&framed[1..framed.len() - 1], body.as_slice());
        assert_eq!(encoder.unknown_id(), 3);
        assert!(encoder.required_ids().any(|id| id == 3));
        assert!(encoder.body_ids("", Role::Query).unwrap().is_empty());
    }

    #[test]
    fn a_long_text_is_cut_at_the_token_limit_and_keeps_its_framing() {
        let encoder = encoder(&spec(), 10);
        let text = "Dragons guard a stolen ring beneath the northern mountains.";
        let body = encoder.body_ids(text, Role::Document).unwrap();
        assert_eq!(body, reference(text)[..8]);
        let framed = encoder.frame(body);
        assert_eq!(framed.len(), 10);
        assert_eq!((framed[0], framed[9]), (2, 1));
        assert_eq!(encoder.frame(vec![7; 50]).len(), 10);
    }

    #[test]
    fn the_role_picks_the_text_prefix() {
        let mut spec = spec();
        spec.query_prefix = "Query: ".into();
        spec.document_prefix = "Document: ".into();
        let encoder = encoder(&spec, 64);
        assert_eq!(
            encoder.body_ids("a ring", Role::Query).unwrap(),
            reference("Query: a ring")
        );
        assert_eq!(
            encoder.body_ids("a ring", Role::Document).unwrap(),
            reference("Document: a ring")
        );
    }

    #[test]
    fn literal_pieces_split_the_text_longest_first() {
        let mut spec = spec();
        // Any two pieces of the vocabulary serve as literals: the test is
        // about the split, not about what the pieces mean.
        spec.literal_pieces = vec!["<pad>".into(), "</s>".into(), "<s>".into()];
        let encoder = encoder(&spec, 64);
        let ids = encoder
            .body_ids("a ring<s> stolen</s> <pad>é", Role::Document)
            .unwrap();
        let mut expected = reference("a ring");
        expected.push(2);
        // The segment " stolen" loses its leading space.
        expected.extend(reference("stolen"));
        expected.push(1);
        // The segment " " is a lone word boundary.
        expected.push(encoder.boundary);
        expected.push(0);
        expected.extend(reference("é"));
        assert_eq!(ids, expected);
        // A literal at the very start and end, and two in a row.
        assert_eq!(
            encoder.body_ids("<s><pad>x</s>", Role::Document).unwrap(),
            [vec![2, 0], reference("x"), vec![1]].concat()
        );
        // The limit also stops literals.
        let tight = TextEncoder::new(TINY_TOKENIZER.to_vec(), &spec, 4).unwrap();
        assert_eq!(
            tight.body_ids("<s><s><s><s>", Role::Document).unwrap(),
            [2, 2]
        );
    }

    #[test]
    fn a_tokenizer_without_a_leading_boundary_keeps_a_leading_space() {
        let plain: &[u8] =
            include_bytes!("../../../../bookclerk-sentencepiece/tests/fixtures/plain.spm.fb");
        let plain_reference = |text: &str| {
            Tokenizer::from_bytes(plain.to_vec())
                .unwrap()
                .encode(text, 10_000)
                .unwrap()
                .ids
        };
        let mut spec = spec();
        spec.literal_pieces = vec!["<pad>".into()];
        let encoder = TextEncoder::new(plain.to_vec(), &spec, 64).unwrap();
        assert!(!encoder.adds_boundary);
        assert_ne!(plain_reference(" a ring"), plain_reference("a ring"));
        assert_eq!(
            encoder.body_ids(" a ring", Role::Document).unwrap(),
            plain_reference(" a ring")
        );
        assert_eq!(
            encoder
                .body_ids("ring<pad> <pad> stolen", Role::Document)
                .unwrap(),
            [
                plain_reference("ring"),
                vec![0],
                plain_reference(" "),
                vec![0],
                plain_reference(" stolen")
            ]
            .concat()
        );
        // The fixture with a boundary drops the space, as before.
        let tiny = self::encoder(&self::spec(), 64);
        assert!(tiny.adds_boundary);
        assert_eq!(
            tiny.body_ids(" a ring", Role::Document).unwrap(),
            reference("a ring")
        );
    }

    #[test]
    fn spelling_uses_single_character_pieces() {
        let encoder = encoder(&spec(), 64);
        let tokenizer = Tokenizer::from_bytes(TINY_TOKENIZER.to_vec()).unwrap();
        let (id, piece) = (0..640u32)
            .filter_map(|id| Some((id, tokenizer.id_to_piece(id)?.to_owned())))
            .find(|(_, piece)| {
                piece.starts_with(WORD_BOUNDARY)
                    && piece.chars().count() >= 4
                    && piece.is_char_boundary(3)
                    && piece[3..].chars().all(|c| c.is_ascii_lowercase())
            })
            .expect("a multi-character word piece");
        let spelled = encoder.spell(id);
        assert_eq!(spelled.len(), piece.chars().count());
        assert_eq!(spelled[0], encoder.boundary);
        for (spelled_id, character) in spelled.iter().zip(piece.chars()).skip(1) {
            assert_eq!(
                tokenizer.id_to_piece(*spelled_id),
                Some(character.to_string().as_str())
            );
        }
        assert_eq!(encoder.spell(1_000_000), [3]);
    }

    #[test]
    fn a_missing_piece_or_a_bad_model_is_refused() {
        let mut bad = spec();
        bad.prefix_pieces = vec!["<nope>".into()];
        assert_eq!(
            TextEncoder::new(TINY_TOKENIZER.to_vec(), &bad, 64).unwrap_err(),
            TextError::MissingPiece("<nope>".into())
        );
        let mut bad = spec();
        bad.literal_pieces = vec!["<never>".into()];
        assert!(matches!(
            TextEncoder::new(TINY_TOKENIZER.to_vec(), &bad, 64),
            Err(TextError::MissingPiece(_))
        ));
        assert!(matches!(
            TextEncoder::new(b"not a model".to_vec(), &spec(), 64),
            Err(TextError::Tokenizer(_))
        ));
    }
}
