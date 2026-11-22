//! The checks the host runs on model listings, model runs and their replies.
//!
//! A plugin serves models on its `ai` entrypoint ([`crate::AiModels`]) and
//! any plugin with the grant calls them through the `AI` binding
//! ([`crate::AiRunner`]). Three things cross a trust boundary, and each is
//! checked by sans-IO code that the fuzz targets and the host share:
//!
//! - **A model plugin's listing** ([`check_models`]): the models it serves,
//!   each with a task, a revision and the facts a caller needs.
//! - **A caller's run** on the `AI` binding ([`check_binding_run_params`]):
//!   a model name ([`AiModelName`]) and an input for exactly one task. The
//!   host checks the same input again before a model plugin sees it
//!   ([`check_entrypoint_run_params`]).
//! - **A model plugin's reply** to a run ([`check_run_output`]): for text
//!   embeddings, one finite vector per input text, of the length and
//!   revision the model declared.
//!
//! Nothing here loads or runs a model.

use std::collections::BTreeSet;

use crate::{
    AiInput, AiModel, AiModelList, AiRunOk, AiRunParams, AiTask, AiTextEmbeddingsOutput,
    PluginError, MAX_AI_EMBED_DIMENSIONS, MAX_AI_EMBED_TEXTS, MAX_AI_EMBED_TEXT_BYTES,
    MAX_AI_LANGUAGES, MAX_AI_MODELS, MAX_AI_MODEL_NAME_BYTES,
};

/// The name space of task aliases: `@library/text-embeddings` names the
/// model the library chose for the task. No plugin serving models may use
/// it as its id.
pub const AI_ALIAS_NAMESPACE: &str = "library";

/// Longest model revision digest, in bytes. Revisions are printable ASCII
/// without spaces.
pub const MAX_AI_REVISION_BYTES: usize = 128;

/// Longest language tag on a model, in bytes (the longest well-formed
/// BCP 47 tag).
pub const MAX_AI_LANGUAGE_TAG_BYTES: usize = 35;

/// Bytes in one vector component: an IEEE 754 32-bit float.
const F32_BYTES: usize = 4;

/// Why a model listing, run or reply was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiError {
    /// A model name is not `@<plugin id>/<model id>` or a task alias, or
    /// is over [`MAX_AI_MODEL_NAME_BYTES`].
    BadModelName,
    /// A model id is not `[a-z0-9][a-z0-9._-]*`, or its full name would be
    /// over [`MAX_AI_MODEL_NAME_BYTES`].
    BadModelId,
    /// A task alias names no known task.
    UnknownAlias,
    /// A listing has more than [`MAX_AI_MODELS`] models.
    TooManyModels,
    /// A listing names the same model id twice.
    DuplicateModel(String),
    /// A revision is empty, too long, or holds a space or a non-printable
    /// byte.
    BadRevision,
    /// A vector length is zero, over [`MAX_AI_EMBED_DIMENSIONS`], or
    /// missing on a text-embedding model.
    BadDimensions,
    /// A text-embedding model declares no token limit, or zero.
    BadMaxTokens,
    /// A model's language list is too long, repeats a tag or holds a
    /// malformed one.
    BadLanguages,
    /// A run carries no input for any task.
    NoInput,
    /// A text-embedding input has more than [`MAX_AI_EMBED_TEXTS`] texts.
    TooManyTexts,
    /// The text at this index is empty or over
    /// [`MAX_AI_EMBED_TEXT_BYTES`].
    BadText(usize),
    /// The reply's vector bytes are not one vector of the declared length
    /// per input text.
    VectorCount,
    /// The vector at this index holds a number that is not finite.
    NotFinite(usize),
    /// The fallback indexes are not ascending, repeat, or point past the
    /// input.
    BadFallbacks,
    /// The reply's vector length differs from the model's listing.
    DimensionsChanged,
    /// The reply's revision differs from the model's listing.
    RevisionChanged,
    /// A model plugin's reply named a model; only the host does.
    UnexpectedModelName,
}

impl std::fmt::Display for AiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadModelName => {
                f.write_str("model name is not `@<plugin id>/<model id>` or a task alias")
            }
            Self::BadModelId => {
                f.write_str("model id is not lowercase letters, digits, `.`, `_` or `-`")
            }
            Self::UnknownAlias => f.write_str("model alias names no known task"),
            Self::TooManyModels => write!(f, "more than {MAX_AI_MODELS} models"),
            Self::DuplicateModel(id) => write!(f, "model `{id}` is listed twice"),
            Self::BadRevision => f.write_str("model revision is not a short printable digest"),
            Self::BadDimensions => write!(
                f,
                "vector length is not between 1 and {MAX_AI_EMBED_DIMENSIONS}"
            ),
            Self::BadMaxTokens => f.write_str("text-embedding model declares no token limit"),
            Self::BadLanguages => f.write_str("model language list is malformed"),
            Self::NoInput => f.write_str("model run carries no input"),
            Self::TooManyTexts => write!(f, "more than {MAX_AI_EMBED_TEXTS} texts in one run"),
            Self::BadText(index) => write!(
                f,
                "text {index} is empty or over {MAX_AI_EMBED_TEXT_BYTES} bytes"
            ),
            Self::VectorCount => f.write_str("reply does not hold one vector per text"),
            Self::NotFinite(index) => write!(f, "vector {index} holds a number that is not finite"),
            Self::BadFallbacks => f.write_str("reply fallback indexes are malformed"),
            Self::DimensionsChanged => {
                f.write_str("reply vector length differs from the model's listing")
            }
            Self::RevisionChanged => f.write_str("reply revision differs from the model's listing"),
            Self::UnexpectedModelName => f.write_str("a model plugin's reply may not name a model"),
        }
    }
}

impl std::error::Error for AiError {}

impl From<AiError> for PluginError {
    fn from(err: AiError) -> Self {
        PluginError::invalid_params(err.to_string())
    }
}

/// Converts a `u32` ABI cap to `usize`, saturating on 16-bit targets.
fn cap(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// The name of a task in a task alias (`text-embeddings`).
///
/// # Arguments
///
/// * `task` - The task.
///
/// # Returns
///
/// The alias segment after `@library/`.
#[must_use]
pub fn task_alias(task: AiTask) -> &'static str {
    match task {
        AiTask::TextEmbeddings => "text-embeddings",
    }
}

/// Every task, for looking an alias up.
const ALL_TASKS: [AiTask; 1] = [AiTask::TextEmbeddings];

/// Whether `id` is a model id as a plugin lists it: a lowercase letter or
/// digit, then lowercase letters, digits, `.`, `_` and `-`, short enough
/// that the longest plugin id still fits before it in
/// [`MAX_AI_MODEL_NAME_BYTES`].
///
/// # Arguments
///
/// * `id` - The model id, without its `@<plugin id>/` prefix.
///
/// # Returns
///
/// True when the id is well formed.
#[must_use]
pub fn is_ai_model_id(id: &str) -> bool {
    /// Room kept for `@`, the longest plugin id (32 bytes) and `/`.
    const PREFIX_ROOM: usize = 34;
    let bytes = id.as_bytes();
    let fits = bytes.len() <= cap(MAX_AI_MODEL_NAME_BYTES).saturating_sub(PREFIX_ROOM);
    match bytes.split_first() {
        Some((first, rest)) => {
            fits && (first.is_ascii_lowercase() || first.is_ascii_digit())
                && rest.iter().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
                })
        }
        None => false,
    }
}

/// Whether `id` follows the plugin id grammar: `[a-z0-9_]{2,32}` with no
/// leading, trailing or doubled `_`.
fn is_plugin_id(id: &str) -> bool {
    (2..=32).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !id.starts_with('_')
        && !id.ends_with('_')
        && !id.contains("__")
}

/// A model as a caller names it on the `AI` binding.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AiModelName {
    /// One model of one plugin: `@<plugin id>/<model id>`.
    Model {
        /// Id of the plugin that serves the model.
        plugin: String,
        /// The model's id within that plugin.
        model: String,
    },
    /// The model the library chose for a task: `@library/<task>`.
    Alias(AiTask),
}

impl AiModelName {
    /// Parses a model name.
    ///
    /// # Arguments
    ///
    /// * `name` - The name as a caller wrote it.
    ///
    /// # Returns
    ///
    /// The plugin and model it names, or the task it is an alias for.
    ///
    /// # Errors
    ///
    /// [`AiError::BadModelName`] when the name is malformed or too long,
    /// and [`AiError::UnknownAlias`] when it is `@library/` followed by
    /// something that is not a task.
    pub fn parse(name: &str) -> Result<Self, AiError> {
        if name.len() > cap(MAX_AI_MODEL_NAME_BYTES) {
            return Err(AiError::BadModelName);
        }
        let (plugin, model) = name
            .strip_prefix('@')
            .and_then(|rest| rest.split_once('/'))
            .ok_or(AiError::BadModelName)?;
        if plugin == AI_ALIAS_NAMESPACE {
            return ALL_TASKS
                .into_iter()
                .find(|task| task_alias(*task) == model)
                .map(Self::Alias)
                .ok_or(AiError::UnknownAlias);
        }
        if !is_plugin_id(plugin) || !is_ai_model_id(model) {
            return Err(AiError::BadModelName);
        }
        Ok(Self::Model {
            plugin: plugin.to_string(),
            model: model.to_string(),
        })
    }
}

impl std::fmt::Display for AiModelName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Model { plugin, model } => write!(f, "@{plugin}/{model}"),
            Self::Alias(task) => write!(f, "@{AI_ALIAS_NAMESPACE}/{}", task_alias(*task)),
        }
    }
}

/// Whether `revision` is a usable revision digest: 1 to
/// [`MAX_AI_REVISION_BYTES`] bytes of printable ASCII with no space.
fn is_revision(revision: &str) -> bool {
    !revision.is_empty()
        && revision.len() <= MAX_AI_REVISION_BYTES
        && revision.bytes().all(|b| b.is_ascii_graphic())
}

/// Whether `tag` has the shape of a language tag: ASCII letters and digits
/// in `-`-separated parts of 1 to 8 characters.
fn is_language_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= MAX_AI_LANGUAGE_TAG_BYTES
        && tag.split('-').all(|part| {
            (1..=8).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

/// Checks one listed model.
///
/// # Arguments
///
/// * `model` - The model as the plugin listed it.
///
/// # Errors
///
/// The first rule the model breaks.
pub fn check_model(model: &AiModel) -> Result<(), AiError> {
    if !is_ai_model_id(&model.model_id) {
        return Err(AiError::BadModelId);
    }
    if !is_revision(&model.revision) {
        return Err(AiError::BadRevision);
    }
    if model.languages.len() > cap(MAX_AI_LANGUAGES) {
        return Err(AiError::BadLanguages);
    }
    let mut seen = BTreeSet::new();
    for tag in &model.languages {
        if !is_language_tag(tag) || !seen.insert(tag.to_ascii_lowercase()) {
            return Err(AiError::BadLanguages);
        }
    }
    match model.task {
        AiTask::TextEmbeddings => {
            match model.dimensions {
                Some(dimensions) if (1..=MAX_AI_EMBED_DIMENSIONS).contains(&dimensions) => {}
                Some(_) | None => return Err(AiError::BadDimensions),
            }
            match model.max_tokens {
                Some(tokens) if tokens > 0 => {}
                Some(_) | None => return Err(AiError::BadMaxTokens),
            }
        }
    }
    Ok(())
}

/// Checks a model plugin's listing: at most [`MAX_AI_MODELS`] well-formed
/// models, each id once.
///
/// # Arguments
///
/// * `list` - The reply to `AiModels.models`.
///
/// # Errors
///
/// The first rule the listing breaks.
pub fn check_models(list: &AiModelList) -> Result<(), AiError> {
    if list.models.len() > cap(MAX_AI_MODELS) {
        return Err(AiError::TooManyModels);
    }
    let mut seen = BTreeSet::new();
    for model in &list.models {
        check_model(model)?;
        if !seen.insert(model.model_id.as_str()) {
            return Err(AiError::DuplicateModel(model.model_id.clone()));
        }
    }
    Ok(())
}

/// Checks the input of one run and says which task it is for.
///
/// # Arguments
///
/// * `input` - The run's input.
///
/// # Returns
///
/// The task whose field is set.
///
/// # Errors
///
/// [`AiError::NoInput`] when no task's field is set, otherwise the first
/// rule that task's input breaks.
pub fn check_input(input: &AiInput) -> Result<AiTask, AiError> {
    let texts = &input.text_embeddings.texts;
    if texts.is_empty() {
        return Err(AiError::NoInput);
    }
    if texts.len() > cap(MAX_AI_EMBED_TEXTS) {
        return Err(AiError::TooManyTexts);
    }
    for (index, item) in texts.iter().enumerate() {
        if item.text.is_empty() || item.text.len() > cap(MAX_AI_EMBED_TEXT_BYTES) {
            return Err(AiError::BadText(index));
        }
    }
    Ok(AiTask::TextEmbeddings)
}

/// Checks a caller's run on the `AI` binding.
///
/// # Arguments
///
/// * `params` - The params of `AiRunner.run`.
///
/// # Returns
///
/// The parsed model name and the task the input is for. An alias for
/// another task than the input's is refused.
///
/// # Errors
///
/// The first rule the name or the input breaks.
pub fn check_binding_run_params(params: &AiRunParams) -> Result<(AiModelName, AiTask), AiError> {
    let name = AiModelName::parse(&params.model)?;
    let task = check_input(&params.input)?;
    match &name {
        AiModelName::Alias(alias) if *alias != task => Err(AiError::NoInput),
        AiModelName::Alias(_) | AiModelName::Model { .. } => Ok((name, task)),
    }
}

/// Checks a run before a model plugin sees it on its `ai` entrypoint.
///
/// # Arguments
///
/// * `params` - The params of `AiModels.run`; `model` is the plugin's own
///   model id.
///
/// # Returns
///
/// The task the input is for.
///
/// # Errors
///
/// The first rule the model id or the input breaks.
pub fn check_entrypoint_run_params(params: &AiRunParams) -> Result<AiTask, AiError> {
    if !is_ai_model_id(&params.model) {
        return Err(AiError::BadModelId);
    }
    check_input(&params.input)
}

/// Splits checked text-embedding output into its vectors.
///
/// # Arguments
///
/// * `output` - Output that passed [`check_run_output`].
///
/// # Returns
///
/// One vector per input text, in input order. Empty when `dimensions` is
/// zero.
#[must_use]
pub fn embedding_vectors(output: &AiTextEmbeddingsOutput) -> Vec<Vec<f32>> {
    let width = cap(output.dimensions).saturating_mul(F32_BYTES);
    if width == 0 {
        return Vec::new();
    }
    output
        .vectors
        .chunks_exact(width)
        .map(|vector| {
            vector
                .as_chunks::<F32_BYTES>()
                .0
                .iter()
                .map(|le| f32::from_le_bytes(*le))
                .collect()
        })
        .collect()
}

/// Packs vectors into the wire form of [`AiTextEmbeddingsOutput::vectors`]:
/// little-endian 32-bit floats, concatenated.
///
/// # Arguments
///
/// * `vectors` - The vectors, all of one length.
///
/// # Returns
///
/// The bytes.
#[must_use]
pub fn pack_embedding_vectors(vectors: &[Vec<f32>]) -> Vec<u8> {
    vectors
        .iter()
        .flat_map(|vector| vector.iter().flat_map(|value| value.to_le_bytes()))
        .collect()
}

/// Checks a model plugin's reply to a run against the input it answers and
/// the model's listing.
///
/// For text embeddings: the vector length is the one the model listed,
/// there is exactly one vector per input text, every number is finite, the
/// revision is the model's, and the fallback indexes are ascending, unique
/// and inside the input.
///
/// # Arguments
///
/// * `model` - The model as its plugin listed it (checked by
///   [`check_model`]).
/// * `input` - The input the reply answers (checked by [`check_input`]).
/// * `reply` - The success payload of `AiModels.run`.
///
/// # Errors
///
/// The first rule the reply breaks.
pub fn check_run_output(model: &AiModel, input: &AiInput, reply: &AiRunOk) -> Result<(), AiError> {
    if reply.model.as_deref().is_some_and(|name| !name.is_empty()) {
        return Err(AiError::UnexpectedModelName);
    }
    match model.task {
        AiTask::TextEmbeddings => check_text_embeddings_output(
            model,
            input.text_embeddings.texts.len(),
            &reply.output.text_embeddings,
        ),
    }
}

/// Checks text-embedding output for `texts` input texts.
///
/// # Errors
///
/// The first rule the output breaks.
fn check_text_embeddings_output(
    model: &AiModel,
    texts: usize,
    output: &AiTextEmbeddingsOutput,
) -> Result<(), AiError> {
    if !(1..=MAX_AI_EMBED_DIMENSIONS).contains(&output.dimensions) {
        return Err(AiError::BadDimensions);
    }
    if model.dimensions != Some(output.dimensions) {
        return Err(AiError::DimensionsChanged);
    }
    if !is_revision(&output.revision) {
        return Err(AiError::BadRevision);
    }
    if output.revision != model.revision {
        return Err(AiError::RevisionChanged);
    }
    let width = cap(output.dimensions).saturating_mul(F32_BYTES);
    if texts == 0 || texts.checked_mul(width) != Some(output.vectors.len()) {
        return Err(AiError::VectorCount);
    }
    for (index, vector) in output.vectors.chunks_exact(width).enumerate() {
        let finite = vector
            .as_chunks::<F32_BYTES>()
            .0
            .iter()
            .all(|le| f32::from_le_bytes(*le).is_finite());
        if !finite {
            return Err(AiError::NotFinite(index));
        }
    }
    let mut previous: Option<u32> = None;
    for &index in &output.fallbacks {
        let inside = usize::try_from(index).is_ok_and(|i| i < texts);
        if !inside || previous.is_some_and(|p| p >= index) {
            return Err(AiError::BadFallbacks);
        }
        previous = Some(index);
    }
    Ok(())
}

/// Reads one unpacked Cap'n Proto message with a traversal limit sized to
/// the input, so a message cannot make the reader walk more than it holds.
///
/// # Errors
///
/// Returns the Cap'n Proto error the reader reports.
fn read_message(
    bytes: &[u8],
) -> capnp::Result<capnp::message::Reader<capnp::serialize::OwnedSegments>> {
    let words = (bytes.len() / 8).saturating_add(16);
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(words));
    opts.nesting_limit(32);
    capnp::serialize::read_message(&mut std::io::Cursor::new(bytes), opts)
}

/// Decodes an `AiRunReply` from an unpacked Cap'n Proto message, the form
/// a model plugin's reply crosses the plugin boundary in.
///
/// # Arguments
///
/// * `bytes` - The message bytes.
///
/// # Returns
///
/// The reply: the output, or the plugin's typed error.
///
/// # Errors
///
/// Returns the Cap'n Proto error the reader reports.
pub fn decode_ai_run_reply_bytes(bytes: &[u8]) -> capnp::Result<Result<AiRunOk, PluginError>> {
    let reader = read_message(bytes)?;
    crate::read_ai_run_reply(reader.get_root::<crate::plugin_capnp::ai_run_reply::Reader<'_>>()?)
}

/// Encodes an `AiRunReply`, the inverse of [`decode_ai_run_reply_bytes`].
///
/// # Arguments
///
/// * `reply` - The reply to encode.
///
/// # Returns
///
/// The message bytes.
///
/// # Errors
///
/// Returns the Cap'n Proto error the builder reports.
pub fn encode_ai_run_reply_bytes(reply: &Result<AiRunOk, PluginError>) -> capnp::Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    crate::write_ai_run_reply(
        message.init_root::<crate::plugin_capnp::ai_run_reply::Builder<'_>>(),
        reply,
    )?;
    Ok(capnp::serialize::write_message_to_words(&message))
}

/// Decodes `AiRunParams` from an unpacked Cap'n Proto message, the form a
/// plugin's run on the `AI` binding crosses the plugin boundary in.
///
/// # Arguments
///
/// * `bytes` - The message bytes.
///
/// # Returns
///
/// The run.
///
/// # Errors
///
/// Returns the Cap'n Proto error the reader reports.
pub fn decode_ai_run_params_bytes(bytes: &[u8]) -> capnp::Result<AiRunParams> {
    let reader = read_message(bytes)?;
    crate::read_ai_run_params(reader.get_root::<crate::plugin_capnp::ai_run_params::Reader<'_>>()?)
}

/// Encodes `AiRunParams`, the inverse of [`decode_ai_run_params_bytes`].
///
/// # Arguments
///
/// * `params` - The run to encode.
///
/// # Returns
///
/// The message bytes.
///
/// # Errors
///
/// Returns the Cap'n Proto error the builder reports.
pub fn encode_ai_run_params_bytes(params: &AiRunParams) -> capnp::Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    crate::write_ai_run_params(
        message.init_root::<crate::plugin_capnp::ai_run_params::Builder<'_>>(),
        params,
    )?;
    Ok(capnp::serialize::write_message_to_words(&message))
}

/// Decodes an `AiModelsReply` from an unpacked Cap'n Proto message, the
/// form a model plugin's listing crosses the plugin boundary in.
///
/// # Arguments
///
/// * `bytes` - The message bytes.
///
/// # Returns
///
/// The listing, or the plugin's typed error.
///
/// # Errors
///
/// Returns the Cap'n Proto error the reader reports.
pub fn decode_ai_models_reply_bytes(
    bytes: &[u8],
) -> capnp::Result<Result<AiModelList, PluginError>> {
    let reader = read_message(bytes)?;
    crate::read_ai_models_reply(
        reader.get_root::<crate::plugin_capnp::ai_models_reply::Reader<'_>>()?,
    )
}

/// Encodes an `AiModelsReply`, the inverse of
/// [`decode_ai_models_reply_bytes`].
///
/// # Arguments
///
/// * `reply` - The listing to encode.
///
/// # Returns
///
/// The message bytes.
///
/// # Errors
///
/// Returns the Cap'n Proto error the builder reports.
pub fn encode_ai_models_reply_bytes(
    reply: &Result<AiModelList, PluginError>,
) -> capnp::Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    crate::write_ai_models_reply(
        message.init_root::<crate::plugin_capnp::ai_models_reply::Builder<'_>>(),
        reply,
    )?;
    Ok(capnp::serialize::write_message_to_words(&message))
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::{AiEmbedText, AiOutput, AiTextEmbeddingsInput, AiTextRole};

    fn model() -> AiModel {
        AiModel {
            model_id: "bekko-a8m".into(),
            task: AiTask::TextEmbeddings,
            revision: "sha256:0a1b".into(),
            dimensions: Some(2),
            max_tokens: Some(512),
            languages: vec!["en".into(), "pt-BR".into()],
        }
    }

    fn input(texts: &[&str]) -> AiInput {
        AiInput {
            text_embeddings: AiTextEmbeddingsInput {
                texts: texts
                    .iter()
                    .map(|text| AiEmbedText {
                        text: (*text).to_string(),
                        role: AiTextRole::Document,
                    })
                    .collect(),
            },
        }
    }

    fn reply(vectors: &[Vec<f32>]) -> AiRunOk {
        AiRunOk {
            output: AiOutput {
                text_embeddings: AiTextEmbeddingsOutput {
                    dimensions: 2,
                    vectors: pack_embedding_vectors(vectors),
                    revision: "sha256:0a1b".into(),
                    fallbacks: Vec::new(),
                },
            },
            model: None,
        }
    }

    #[test]
    fn names_parse_and_print_back() {
        let name = AiModelName::parse("@embedder/bekko-a8m").unwrap();
        assert_eq!(
            name,
            AiModelName::Model {
                plugin: "embedder".into(),
                model: "bekko-a8m".into()
            }
        );
        assert_eq!(name.to_string(), "@embedder/bekko-a8m");
        let alias = AiModelName::parse("@library/text-embeddings").unwrap();
        assert_eq!(alias, AiModelName::Alias(AiTask::TextEmbeddings));
        assert_eq!(alias.to_string(), "@library/text-embeddings");
    }

    #[test]
    fn malformed_names_are_refused() {
        for bad in [
            "",
            "bekko",
            "embedder/bekko",
            "@embedder",
            "@embedder/",
            "@/bekko",
            "@Embedder/bekko",
            "@embedder/Bekko",
            "@embedder/a/b",
            "@embedder/-x",
            "@_bad/x",
            "@a/x",
        ] {
            assert_eq!(AiModelName::parse(bad), Err(AiError::BadModelName), "{bad}");
        }
        assert_eq!(
            AiModelName::parse("@library/translation"),
            Err(AiError::UnknownAlias)
        );
        let long = format!("@embedder/{}", "a".repeat(200));
        assert_eq!(AiModelName::parse(&long), Err(AiError::BadModelName));
    }

    #[test]
    fn the_longest_model_id_fits_behind_the_longest_plugin_id() {
        let id = "a".repeat(cap(MAX_AI_MODEL_NAME_BYTES) - 34);
        assert!(is_ai_model_id(&id));
        let name = format!("@{}/{id}", "p".repeat(32));
        assert!(AiModelName::parse(&name).is_ok());
        assert!(!is_ai_model_id(&format!("{id}a")));
    }

    #[test]
    fn listings_are_checked() {
        let good = AiModelList {
            models: vec![model()],
        };
        assert_eq!(check_models(&good), Ok(()));
        let twice = AiModelList {
            models: vec![model(), model()],
        };
        assert_eq!(
            check_models(&twice),
            Err(AiError::DuplicateModel("bekko-a8m".into()))
        );
        let many = AiModelList {
            models: (0..=MAX_AI_MODELS)
                .map(|n| AiModel {
                    model_id: format!("m{n}"),
                    ..model()
                })
                .collect(),
        };
        assert_eq!(check_models(&many), Err(AiError::TooManyModels));
    }

    #[test]
    fn a_model_must_carry_its_facts() {
        let cases: [(AiModel, AiError); 8] = [
            (
                AiModel {
                    model_id: "Bad".into(),
                    ..model()
                },
                AiError::BadModelId,
            ),
            (
                AiModel {
                    revision: String::new(),
                    ..model()
                },
                AiError::BadRevision,
            ),
            (
                AiModel {
                    revision: "has space".into(),
                    ..model()
                },
                AiError::BadRevision,
            ),
            (
                AiModel {
                    dimensions: None,
                    ..model()
                },
                AiError::BadDimensions,
            ),
            (
                AiModel {
                    dimensions: Some(MAX_AI_EMBED_DIMENSIONS + 1),
                    ..model()
                },
                AiError::BadDimensions,
            ),
            (
                AiModel {
                    max_tokens: None,
                    ..model()
                },
                AiError::BadMaxTokens,
            ),
            (
                AiModel {
                    languages: vec!["en".into(), "EN".into()],
                    ..model()
                },
                AiError::BadLanguages,
            ),
            (
                AiModel {
                    languages: vec!["en_US".into()],
                    ..model()
                },
                AiError::BadLanguages,
            ),
        ];
        for (candidate, want) in cases {
            assert_eq!(check_model(&candidate), Err(want));
        }
    }

    #[test]
    fn inputs_are_checked() {
        assert_eq!(check_input(&input(&["a"])), Ok(AiTask::TextEmbeddings));
        assert_eq!(check_input(&input(&[])), Err(AiError::NoInput));
        assert_eq!(check_input(&input(&["a", ""])), Err(AiError::BadText(1)));
        let long = "a".repeat(cap(MAX_AI_EMBED_TEXT_BYTES) + 1);
        assert_eq!(check_input(&input(&[&long])), Err(AiError::BadText(0)));
        let many: Vec<&str> = (0..=MAX_AI_EMBED_TEXTS).map(|_| "a").collect();
        assert_eq!(check_input(&input(&many)), Err(AiError::TooManyTexts));
    }

    #[test]
    fn run_params_are_checked_on_both_sides() {
        let run = AiRunParams {
            model: "@library/text-embeddings".into(),
            input: input(&["a"]),
        };
        assert_eq!(
            check_binding_run_params(&run),
            Ok((
                AiModelName::Alias(AiTask::TextEmbeddings),
                AiTask::TextEmbeddings
            ))
        );
        assert_eq!(
            check_entrypoint_run_params(&run),
            Err(AiError::BadModelId),
            "a model plugin is called by its own model id"
        );
        let own = AiRunParams {
            model: "bekko-a8m".into(),
            input: input(&["a"]),
        };
        assert_eq!(
            check_entrypoint_run_params(&own),
            Ok(AiTask::TextEmbeddings)
        );
        assert_eq!(check_binding_run_params(&own), Err(AiError::BadModelName));
    }

    #[test]
    fn a_good_reply_passes_and_splits_into_vectors() {
        let vectors = vec![vec![0.5, -1.0], vec![2.0, 3.0]];
        let mut ok = reply(&vectors);
        ok.output.text_embeddings.fallbacks = vec![1];
        assert_eq!(check_run_output(&model(), &input(&["a", "b"]), &ok), Ok(()));
        assert_eq!(embedding_vectors(&ok.output.text_embeddings), vectors);
    }

    #[test]
    fn bad_replies_are_refused() {
        let two = input(&["a", "b"]);
        let good = reply(&[vec![0.5, -1.0], vec![2.0, 3.0]]);

        let short = reply(&[vec![0.5, -1.0]]);
        assert_eq!(
            check_run_output(&model(), &two, &short),
            Err(AiError::VectorCount)
        );

        let mut ragged = good.clone();
        ragged.output.text_embeddings.vectors.pop();
        assert_eq!(
            check_run_output(&model(), &two, &ragged),
            Err(AiError::VectorCount)
        );

        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let reply = reply(&[vec![0.5, -1.0], vec![bad, 3.0]]);
            assert_eq!(
                check_run_output(&model(), &two, &reply),
                Err(AiError::NotFinite(1))
            );
        }

        let mut wide = good.clone();
        wide.output.text_embeddings.dimensions = 4;
        assert_eq!(
            check_run_output(&model(), &two, &wide),
            Err(AiError::DimensionsChanged)
        );

        let mut zero = good.clone();
        zero.output.text_embeddings.dimensions = 0;
        assert_eq!(
            check_run_output(&model(), &two, &zero),
            Err(AiError::BadDimensions)
        );
        assert!(embedding_vectors(&zero.output.text_embeddings).is_empty());

        let mut other = good.clone();
        other.output.text_embeddings.revision = "sha256:ffff".into();
        assert_eq!(
            check_run_output(&model(), &two, &other),
            Err(AiError::RevisionChanged)
        );

        let mut blank = good.clone();
        blank.output.text_embeddings.revision = String::new();
        assert_eq!(
            check_run_output(&model(), &two, &blank),
            Err(AiError::BadRevision)
        );

        for fallbacks in [vec![2], vec![1, 0], vec![0, 0]] {
            let mut reply = good.clone();
            reply.output.text_embeddings.fallbacks = fallbacks;
            assert_eq!(
                check_run_output(&model(), &two, &reply),
                Err(AiError::BadFallbacks)
            );
        }

        let mut named = good;
        named.model = Some("@embedder/other".into());
        assert_eq!(
            check_run_output(&model(), &two, &named),
            Err(AiError::UnexpectedModelName)
        );
    }

    #[test]
    fn messages_round_trip() {
        let run = AiRunParams {
            model: "@embedder/bekko-a8m".into(),
            input: input(&["a", "b"]),
        };
        let bytes = encode_ai_run_params_bytes(&run).unwrap();
        assert_eq!(decode_ai_run_params_bytes(&bytes).unwrap(), run);

        let ok = Ok(reply(&[vec![0.5, -1.0]]));
        let bytes = encode_ai_run_reply_bytes(&ok).unwrap();
        assert_eq!(decode_ai_run_reply_bytes(&bytes).unwrap(), ok);

        let listing = Ok(AiModelList {
            models: vec![model()],
        });
        let bytes = encode_ai_models_reply_bytes(&listing).unwrap();
        assert_eq!(decode_ai_models_reply_bytes(&bytes).unwrap(), listing);

        assert!(decode_ai_run_reply_bytes(&[0xff; 7]).is_err());
    }

    #[test]
    fn errors_reach_callers_as_invalid_params() {
        let err: PluginError = AiError::BadText(3).into();
        assert_eq!(err.code, crate::PluginErrorCode::InvalidParams);
        assert!(err.message.contains("text 3"));
        for err in [
            AiError::BadModelName,
            AiError::BadModelId,
            AiError::UnknownAlias,
            AiError::TooManyModels,
            AiError::DuplicateModel("m".into()),
            AiError::BadRevision,
            AiError::BadDimensions,
            AiError::BadMaxTokens,
            AiError::BadLanguages,
            AiError::NoInput,
            AiError::TooManyTexts,
            AiError::VectorCount,
            AiError::NotFinite(0),
            AiError::BadFallbacks,
            AiError::DimensionsChanged,
            AiError::RevisionChanged,
            AiError::UnexpectedModelName,
        ] {
            assert!(!err.to_string().is_empty());
        }
    }
}
