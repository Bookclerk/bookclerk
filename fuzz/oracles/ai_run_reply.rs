//! Shared oracle for the `ai_run_reply` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! A model plugin answers the host on its `ai` entrypoint. Byte 0 modulo 2
//! picks what the rest is:
//!
//! - 0: a reply to `AiModels.run`. Byte 1 modulo 65 is the number of texts
//!   the host sent, bytes 2 and 3 are the vector length the model listed
//!   (little-endian), and the rest is an unpacked `AiRunReply` message. The
//!   model's listed revision is [`REVISION`].
//! - 1: a reply to `AiModels.models`, an unpacked `AiModelsReply` message.
//!
//! The invariants:
//!
//! - decoding and checking never panic;
//! - [`check_run_output`] accepts a reply exactly when the independent
//!   restatement below finds it names no model, carries the listed vector
//!   length and revision, holds exactly one vector of finite numbers per
//!   text, and lists fallbacks that ascend strictly inside the input;
//! - an accepted reply splits into that many vectors of that length, and
//!   packing them gives the reply's bytes back;
//! - [`check_models`] accepts a listing exactly when the restatement finds
//!   at most 64 models, each id once and well formed, with a printable
//!   revision, well-formed languages listed once, a vector length from 1 to
//!   4096 and a token limit above zero;
//! - an accepted reply or listing round-trips through the codec.

use bookclerk_plugin_abi::ai::{
    check_models, check_run_output, decode_ai_models_reply_bytes, decode_ai_run_reply_bytes,
    embedding_vectors, encode_ai_models_reply_bytes, encode_ai_run_reply_bytes,
    pack_embedding_vectors,
};
use bookclerk_plugin_abi::{
    AiEmbedText, AiInput, AiModel, AiModelList, AiRunOk, AiTask, AiTextEmbeddingsInput, AiTextRole,
};

/// Largest input worth decoding: 64 vectors of 4096 numbers and room for
/// the message around them.
pub const MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;

/// The revision the model in a run case listed.
pub const REVISION: &str = "sha256:seed";

/// Vector length cap (`maxAiEmbedDimensions`).
const DIMENSIONS: u32 = 4_096;
/// Model count cap (`maxAiModels`).
const MODELS: usize = 64;
/// Language count cap (`maxAiLanguages`).
const LANGUAGES: usize = 64;
/// Longest model id: the 192-byte name cap less `@`, a 32-byte plugin id
/// and `/`.
const MODEL_ID: usize = 158;

/// Runs the invariants on one input.
///
/// # Panics
///
/// When an invariant breaks.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((selector, rest)) = data.split_first() else {
        return;
    };
    if selector % 2 == 0 {
        check_run(rest);
    } else {
        check_listing(rest);
    }
}

/// Checks a reply to a run.
fn check_run(data: &[u8]) {
    let [count, low, high, message @ ..] = data else {
        return;
    };
    let texts = usize::from(count % 65);
    let listed = u32::from(u16::from_le_bytes([*low, *high]));
    let Ok(Ok(reply)) = decode_ai_run_reply_bytes(message) else {
        return;
    };
    let model = AiModel {
        model_id: "seed".into(),
        task: AiTask::TextEmbeddings,
        revision: REVISION.into(),
        dimensions: Some(listed),
        max_tokens: Some(512),
        languages: Vec::new(),
    };
    let input = AiInput {
        text_embeddings: AiTextEmbeddingsInput {
            texts: (0..texts)
                .map(|_| AiEmbedText {
                    text: "t".into(),
                    role: AiTextRole::Document,
                })
                .collect(),
        },
    };
    let accepted = check_run_output(&model, &input, &reply).is_ok();
    assert_eq!(
        accepted,
        expected_run(texts, listed, &reply),
        "texts {texts} listed {listed} reply {reply:?}"
    );
    if !accepted {
        return;
    }
    let output = &reply.output.text_embeddings;
    let vectors = embedding_vectors(output);
    assert_eq!(vectors.len(), texts, "one vector per text");
    for vector in &vectors {
        assert_eq!(u32::try_from(vector.len()).ok(), Some(listed));
        assert!(vector.iter().all(|value| value.is_finite()));
    }
    assert_eq!(pack_embedding_vectors(&vectors), output.vectors);
    let bytes = encode_ai_run_reply_bytes(&Ok(reply.clone()))
        .unwrap_or_else(|err| panic!("encode an accepted reply: {err}"));
    let again = decode_ai_run_reply_bytes(&bytes)
        .unwrap_or_else(|err| panic!("decode an encoded reply: {err}"))
        .unwrap_or_else(|err| panic!("an encoded reply became an error: {err:?}"));
    assert_eq!(again.output, reply.output);
    assert!(check_run_output(&model, &input, &again).is_ok());
}

/// The independent restatement of every rule on a run reply.
fn expected_run(texts: usize, listed: u32, reply: &AiRunOk) -> bool {
    let output = &reply.output.text_embeddings;
    let unnamed = reply.model.as_deref().unwrap_or("").is_empty();
    let length_ok = (1..=DIMENSIONS).contains(&output.dimensions) && output.dimensions == listed;
    let revision_ok = output.revision == REVISION;
    let want = u64::try_from(texts).unwrap_or(u64::MAX) * u64::from(output.dimensions) * 4;
    let count_ok = texts > 0 && u64::try_from(output.vectors.len()).ok() == Some(want);
    let finite = output.vectors.chunks(4).all(|bytes| {
        // The exponent bits are all set exactly for infinities and NaNs.
        bytes.len() == 4
            && (u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) >> 23) & 0xff != 0xff
    });
    let fallbacks_ok = output.fallbacks.windows(2).all(|pair| pair[0] < pair[1])
        && output
            .fallbacks
            .iter()
            .all(|index| u64::from(*index) < u64::try_from(texts).unwrap_or(u64::MAX));
    unnamed && length_ok && revision_ok && count_ok && finite && fallbacks_ok
}

/// Checks a reply to a listing.
fn check_listing(message: &[u8]) {
    let Ok(Ok(list)) = decode_ai_models_reply_bytes(message) else {
        return;
    };
    let accepted = check_models(&list).is_ok();
    assert_eq!(accepted, expected_listing(&list), "listing {list:?}");
    if accepted {
        let bytes = encode_ai_models_reply_bytes(&Ok(list.clone()))
            .unwrap_or_else(|err| panic!("encode an accepted listing: {err}"));
        let again = decode_ai_models_reply_bytes(&bytes)
            .unwrap_or_else(|err| panic!("decode an encoded listing: {err}"))
            .unwrap_or_else(|err| panic!("an encoded listing became an error: {err:?}"));
        assert_eq!(again, list);
    }
}

/// Whether `id` is a model id, restated over characters.
fn model_id(id: &str) -> bool {
    let mut chars = id.chars();
    let head = chars
        .next()
        .is_some_and(|c| matches!(c, 'a'..='z' | '0'..='9'));
    head && id.len() <= MODEL_ID
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '.' | '_' | '-'))
}

/// Whether `tag` has the shape of a language tag, restated over characters.
fn language(tag: &str) -> bool {
    tag.len() <= 35
        && tag.split('-').all(|part| {
            !part.is_empty()
                && part.len() <= 8
                && part
                    .chars()
                    .all(|c| matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9'))
        })
}

/// The independent restatement of every rule on a listing.
fn expected_listing(list: &AiModelList) -> bool {
    let each = list.models.iter().all(|model| {
        let revision = !model.revision.is_empty()
            && model.revision.len() <= 128
            && model.revision.chars().all(|c| ('!'..='~').contains(&c));
        let tags: Vec<String> = model.languages.iter().map(|t| t.to_lowercase()).collect();
        let languages = tags.len() <= LANGUAGES
            && model.languages.iter().all(|tag| language(tag))
            && tags
                .iter()
                .enumerate()
                .all(|(at, tag)| !tags[..at].contains(tag));
        let facts = match model.task {
            AiTask::TextEmbeddings => {
                model
                    .dimensions
                    .is_some_and(|d| (1..=DIMENSIONS).contains(&d))
                    && model.max_tokens.is_some_and(|t| t != 0)
            }
        };
        model_id(&model.model_id) && revision && languages && facts
    });
    let once = list.models.iter().enumerate().all(|(at, model)| {
        list.models[..at]
            .iter()
            .all(|earlier| earlier.model_id != model.model_id)
    });
    list.models.len() <= MODELS && each && once
}
